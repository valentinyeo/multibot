use crate::rejected::record_rejected_version;
use crate::security::{sha256_file, verify_sha256_hex};
use crate::{FileSwapper, ProcessLauncher, StagedUpdate, SwapOutcome, SwapPlan, UpdaterPlatform};
use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Internal CLI flag passed to the newly launched binary so it signals startup readiness.
pub const UPDATE_READY_ARG: &str = "--update-ready";
/// Default maximum wait time for the relaunched binary to signal readiness before rollback.
pub const DEFAULT_READINESS_TIMEOUT_MS: u64 = 15_000;

const READINESS_MARKER_FILENAME: &str = ".multibot-update-ready";
const STALE_ARTIFACT_NAMES: &[&str] = &[
    "multibot.exe.old",
    "multibot.exe.previous",
    "multibot.exe.previous.exe",
    "multibot.exe.failed",
    "multibot.exe.installing",
    "multibotd-linux-x86_64.old",
    "multibotd-linux-x86_64.tmp",
    "multibotd-linux-x86_64.failed",
    READINESS_MARKER_FILENAME,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyResult {
    pub launched_pid: u32,
    pub launch_args: Vec<String>,
    pub outcome: SwapOutcome,
}

/// Filters CLI arguments for preservation across update relaunch:
/// preserves `--gpu=...` / `--gpu <val>` and user flags, strips internal `--update-ready*` and `--version`/`-V`.
pub fn filter_preserved_cli_args<I, S>(args: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = Vec::new();
    for raw in args {
        let arg = raw.as_ref();
        if arg == UPDATE_READY_ARG
            || arg.starts_with("--update-ready=")
            || arg == "--version"
            || arg == "-V"
        {
            continue;
        }
        out.push(arg.to_string());
    }
    out
}

/// Builds the relaunch argument vector by filtering preserved args and appending `--update-ready`.
pub fn build_relaunch_args(preserved_args: &[String]) -> Vec<String> {
    let mut args = filter_preserved_cli_args(preserved_args);
    args.push(UPDATE_READY_ARG.to_string());
    args
}

/// Returns the readiness handshake marker path alongside `exe_path`.
pub fn readiness_marker_path(exe_path: &Path) -> PathBuf {
    exe_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(READINESS_MARKER_FILENAME)
}

/// Atomically writes the readiness marker file so the previous updating process knows startup succeeded.
pub fn signal_update_ready(exe_path: &Path) -> Result<()> {
    let marker = readiness_marker_path(exe_path);
    let tmp = append_suffix(&marker, &format!(".{}.tmp", std::process::id()));
    let write_res = (|| -> Result<()> {
        let mut f =
            File::create(&tmp).with_context(|| format!("failed creating {}", tmp.display()))?;
        writeln!(f, "{}", std::process::id())?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    if let Err(err) = fs::rename(&tmp, &marker) {
        let _ = fs::remove_file(&tmp);
        return Err(err).with_context(|| format!("failed writing readiness marker {}", marker.display()));
    }
    Ok(())
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut os = OsString::from(path.as_os_str());
    os.push(suffix);
    PathBuf::from(os)
}

fn move_or_copy_into(src: &Path, dest: &Path) -> Result<()> {
    if fs::rename(src, dest).is_ok() {
        return Ok(());
    }
    // Cross-volume fallback (e.g. EXDEV between %APPDATA% on C: and install dir on D:):
    let installing = append_suffix(dest, ".installing");
    let _ = fs::remove_file(&installing);
    if let Err(err) = fs::copy(src, &installing) {
        let _ = fs::remove_file(&installing);
        return Err(err).with_context(|| {
            format!(
                "failed copying {} to {}",
                src.display(),
                installing.display()
            )
        });
    }
    if let Ok(f) = File::open(&installing) {
        let _ = f.sync_all();
    }
    if let Err(err) = fs::rename(&installing, dest) {
        let _ = fs::remove_file(&installing);
        return Err(err).with_context(|| {
            format!(
                "failed moving {} into {}",
                installing.display(),
                dest.display()
            )
        });
    }
    let _ = fs::remove_file(src);
    Ok(())
}

fn copy_into_temp(src: &Path, tmp_dest: &Path) -> Result<()> {
    let _ = fs::remove_file(tmp_dest);
    if let Err(err) = fs::copy(src, tmp_dest) {
        let _ = fs::remove_file(tmp_dest);
        return Err(err).with_context(|| {
            format!("failed staging {} to {}", src.display(), tmp_dest.display())
        });
    }
    if let Ok(f) = File::open(tmp_dest) {
        let _ = f.sync_all();
    }
    Ok(())
}

fn best_effort_remove(path: &Path) -> bool {
    match fs::remove_file(path) {
        Ok(()) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => false,
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultFileSwapper;

impl FileSwapper for DefaultFileSwapper {
    fn swap_binaries(&self, plan: &SwapPlan) -> Result<SwapOutcome> {
        if !plan.current_exe.is_file() {
            bail!(
                "current executable does not exist: {}",
                plan.current_exe.display()
            );
        }
        if !plan.staged_exe.is_file() {
            bail!(
                "staged executable does not exist: {}",
                plan.staged_exe.display()
            );
        }
        if !plan.staged_daemon.is_file() {
            bail!(
                "staged daemon binary does not exist: {}",
                plan.staged_daemon.display()
            );
        }
        if plan.current_daemon.exists() && !plan.current_daemon.is_file() {
            bail!(
                "current daemon path is not a regular file: {}",
                plan.current_daemon.display()
            );
        }

        // Verify install_dir is writable before touching live binaries.
        let probe = plan
            .install_dir
            .join(format!(".multibot-write-probe-{}", std::process::id()));
        {
            let mut f = File::create(&probe).with_context(|| {
                format!(
                    "install directory is not writable: {}",
                    plan.install_dir.display()
                )
            })?;
            f.write_all(b"ok")?;
        }
        let _ = fs::remove_file(&probe);

        let backup_exe = append_suffix(&plan.current_exe, ".old");
        let backup_daemon = append_suffix(&plan.current_daemon, ".old");
        let daemon_tmp = append_suffix(&plan.current_daemon, ".tmp");

        // Best-effort clear stale artifacts before swapping.
        for candidate in [
            backup_exe.clone(),
            append_suffix(&plan.current_exe, ".previous"),
            append_suffix(&plan.current_exe, ".failed"),
            append_suffix(&plan.current_exe, ".installing"),
            backup_daemon.clone(),
            daemon_tmp.clone(),
            append_suffix(&plan.current_daemon, ".failed"),
        ] {
            let _ = best_effort_remove(&candidate);
        }

        // Step 1: Rename running executable to .old (legal on Windows even while running).
        fs::rename(&plan.current_exe, &backup_exe).with_context(|| {
            format!(
                "failed renaming running executable {} -> {}",
                plan.current_exe.display(),
                backup_exe.display()
            )
        })?;

        // Step 2: Move/copy staged executable into place; roll back immediately on error.
        if let Err(err) = move_or_copy_into(&plan.staged_exe, &plan.current_exe) {
            let _ = fs::remove_file(&plan.current_exe);
            let _ = fs::rename(&backup_exe, &plan.current_exe);
            return Err(err).context("failed installing staged executable (rolled back)");
        }

        // Step 3: Atomically replace sibling multibotd-linux-x86_64; roll back BOTH on error.
        let mut had_existing_daemon = false;
        let daemon_swap_res: Result<()> = (|| {
            copy_into_temp(&plan.staged_daemon, &daemon_tmp)?;
            if plan.current_daemon.exists() {
                fs::rename(&plan.current_daemon, &backup_daemon).with_context(|| {
                    format!(
                        "failed backing up {} -> {}",
                        plan.current_daemon.display(),
                        backup_daemon.display()
                    )
                })?;
                had_existing_daemon = true;
            }
            fs::rename(&daemon_tmp, &plan.current_daemon).with_context(|| {
                format!(
                    "failed replacing daemon binary {} -> {}",
                    daemon_tmp.display(),
                    plan.current_daemon.display()
                )
            })?;
            Ok(())
        })();

        if let Err(err) = daemon_swap_res {
            let _ = fs::remove_file(&daemon_tmp);
            if had_existing_daemon && backup_daemon.exists() {
                let _ = fs::remove_file(&plan.current_daemon);
                let _ = fs::rename(&backup_daemon, &plan.current_daemon);
            }
            let failed_exe = append_suffix(&plan.current_exe, ".failed");
            let _ = fs::remove_file(&failed_exe);
            if plan.current_exe.exists() && fs::rename(&plan.current_exe, &failed_exe).is_err() {
                let _ = fs::remove_file(&plan.current_exe);
            }
            let _ = fs::rename(&backup_exe, &plan.current_exe);
            return Err(err).context("failed replacing sibling daemon binary (rolled back both)");
        }

        Ok(SwapOutcome {
            backup_exe,
            backup_daemon: had_existing_daemon.then_some(backup_daemon),
        })
    }

    fn rollback(&self, plan: &SwapPlan, outcome: &SwapOutcome) -> Result<()> {
        if !outcome.backup_exe.exists() {
            bail!(
                "cannot roll back executable: backup missing at {}",
                outcome.backup_exe.display()
            );
        }

        let failed_exe = append_suffix(&plan.current_exe, ".failed");
        let _ = fs::remove_file(&failed_exe);
        if plan.current_exe.exists() && fs::rename(&plan.current_exe, &failed_exe).is_err() {
            fs::remove_file(&plan.current_exe).with_context(|| {
                format!(
                    "failed clearing new executable during rollback: {}",
                    plan.current_exe.display()
                )
            })?;
        }
        fs::rename(&outcome.backup_exe, &plan.current_exe).with_context(|| {
            format!(
                "failed restoring executable backup {} -> {}",
                outcome.backup_exe.display(),
                plan.current_exe.display()
            )
        })?;

        if let Some(backup_daemon) = &outcome.backup_daemon {
            if backup_daemon.exists() {
                let failed_daemon = append_suffix(&plan.current_daemon, ".failed");
                let _ = fs::remove_file(&failed_daemon);
                if plan.current_daemon.exists()
                    && fs::rename(&plan.current_daemon, &failed_daemon).is_err()
                {
                    let _ = fs::remove_file(&plan.current_daemon);
                }
                fs::rename(backup_daemon, &plan.current_daemon).with_context(|| {
                    format!(
                        "failed restoring daemon backup {} -> {}",
                        backup_daemon.display(),
                        plan.current_daemon.display()
                    )
                })?;
            }
        } else if plan.current_daemon.exists() {
            let _ = fs::remove_file(&plan.current_daemon);
        }
        Ok(())
    }

    fn cleanup_stale(&self, install_dir: &Path) -> Result<usize> {
        let mut removed = 0usize;
        for name in STALE_ARTIFACT_NAMES {
            if best_effort_remove(&install_dir.join(name)) {
                removed += 1;
            }
        }
        Ok(removed)
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultProcessLauncher;

impl ProcessLauncher for DefaultProcessLauncher {
    fn spawn_relaunch(&self, exe_path: &Path, args: &[String]) -> Result<u32> {
        let child = Command::new(exe_path)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed spawning relaunched binary {}", exe_path.display()))?;
        Ok(child.id())
    }

    fn verify_readiness(&self, exe_path: &Path, timeout_ms: u64) -> Result<bool> {
        let marker = readiness_marker_path(exe_path);
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if let Ok(content) = fs::read_to_string(&marker)
                && !content.trim().is_empty()
            {
                let _ = fs::remove_file(&marker);
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

#[derive(Debug)]
pub struct DefaultUpdaterPlatform {
    updates_dir: PathBuf,
    swapper: DefaultFileSwapper,
    launcher: DefaultProcessLauncher,
}

impl DefaultUpdaterPlatform {
    pub fn new(updates_dir: PathBuf) -> Self {
        Self {
            updates_dir,
            swapper: DefaultFileSwapper,
            launcher: DefaultProcessLauncher,
        }
    }
}

impl UpdaterPlatform for DefaultUpdaterPlatform {
    fn current_exe(&self) -> Result<PathBuf> {
        std::env::current_exe().context("failed resolving current executable path")
    }

    fn updates_dir(&self) -> PathBuf {
        self.updates_dir.clone()
    }

    fn preserved_cli_args(&self) -> Vec<String> {
        filter_preserved_cli_args(std::env::args().skip(1))
    }

    fn file_swapper(&self) -> &dyn FileSwapper {
        &self.swapper
    }

    fn process_launcher(&self) -> &dyn ProcessLauncher {
        &self.launcher
    }
}

/// Orchestrates TOCTOU hash verification, atomic binary swap, relaunch with `--gpu=...` forwarding,
/// readiness verification, and automatic rollback + `rejected.txt` recording on failure.
#[derive(Debug, Clone, Copy)]
pub struct AtomicSwapper {
    readiness_timeout_ms: u64,
}

impl Default for AtomicSwapper {
    fn default() -> Self {
        Self::new(DEFAULT_READINESS_TIMEOUT_MS)
    }
}

impl AtomicSwapper {
    pub fn new(readiness_timeout_ms: u64) -> Self {
        Self {
            readiness_timeout_ms,
        }
    }

    pub fn apply_staged_update(
        &self,
        platform: &dyn UpdaterPlatform,
        staged: &StagedUpdate,
    ) -> Result<ApplyResult> {
        // TOCTOU defense: re-verify staged binary SHA-256 digests immediately before touching live files.
        let exe_hash = sha256_file(&staged.multibot_exe_path)?;
        verify_sha256_hex(&exe_hash, &staged.multibot_exe_sha256, "staged multibot.exe")?;

        let daemon_hash = sha256_file(&staged.multibotd_linux_path)?;
        verify_sha256_hex(
            &daemon_hash,
            &staged.multibotd_linux_x86_64_sha256,
            "staged multibotd-linux-x86_64",
        )?;

        let current_exe = platform.current_exe()?;
        let install_dir = current_exe
            .parent()
            .context("current_exe has no parent directory")?
            .to_path_buf();
        let current_daemon = install_dir.join("multibotd-linux-x86_64");

        let plan = SwapPlan {
            install_dir,
            current_exe: current_exe.clone(),
            staged_exe: staged.multibot_exe_path.clone(),
            current_daemon,
            staged_daemon: staged.multibotd_linux_path.clone(),
        };

        let launch_args = build_relaunch_args(&platform.preserved_cli_args());
        let _ = fs::remove_file(readiness_marker_path(&plan.current_exe));

        let outcome = platform.file_swapper().swap_binaries(&plan)?;

        let updates_dir = platform.updates_dir();

        let launched_pid = match platform
            .process_launcher()
            .spawn_relaunch(&plan.current_exe, &launch_args)
        {
            Ok(pid) => pid,
            Err(spawn_err) => {
                let rollback_res = platform.file_swapper().rollback(&plan, &outcome);
                let _ = record_rejected_version(&updates_dir, &staged.version);
                if let Err(rb_err) = rollback_res {
                    bail!(
                        "failed launching updated binary ({spawn_err:#}); rollback also failed: {rb_err:#}"
                    );
                }
                return Err(spawn_err)
                    .context("failed launching updated binary; rolled back and recorded rejected version");
            }
        };

        let ready = match platform
            .process_launcher()
            .verify_readiness(&plan.current_exe, self.readiness_timeout_ms)
        {
            Ok(is_ready) => is_ready,
            Err(ready_err) => {
                let rollback_res = platform.file_swapper().rollback(&plan, &outcome);
                let _ = record_rejected_version(&updates_dir, &staged.version);
                if let Err(rb_err) = rollback_res {
                    bail!(
                        "readiness check failed ({ready_err:#}); rollback also failed: {rb_err:#}"
                    );
                }
                return Err(ready_err)
                    .context("updated binary readiness check errored; rolled back and recorded rejected version");
            }
        };

        if !ready {
            let rollback_res = platform.file_swapper().rollback(&plan, &outcome);
            let _ = record_rejected_version(&updates_dir, &staged.version);
            if let Err(rb_err) = rollback_res {
                bail!(
                    "updated binary v{} did not signal readiness within {}ms; rollback also failed: {rb_err:#}",
                    staged.version,
                    self.readiness_timeout_ms
                );
            }
            bail!(
                "updated binary v{} did not signal readiness within {}ms; rolled back and recorded rejected version",
                staged.version,
                self.readiness_timeout_ms
            );
        }

        Ok(ApplyResult {
            launched_pid,
            launch_args,
            outcome,
        })
    }
}

/// Cleans up stale `.old`/`.previous`/`.failed`/`.tmp` binaries in `exe_dir` and leftover
/// `staging-*` directories in `updates_dir` while strictly preserving `rejected.txt`.
pub fn cleanup_old_update_artifacts(exe_dir: &Path, updates_dir: &Path) -> Result<usize> {
    let mut removed = DefaultFileSwapper.cleanup_stale(exe_dir)?;
    if let Ok(entries) = fs::read_dir(updates_dir) {
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            if name.starts_with("staging-") {
                let path = entry.path();
                if path.is_dir() && fs::remove_dir_all(&path).is_ok() {
                    removed += 1;
                }
            }
        }
    }
    Ok(removed)
}
