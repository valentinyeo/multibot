use crate::settings::SshHostConfig;
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

pub const REMOTE_DAEMON_DIR: &str = "~/.multibot";
pub const REMOTE_DAEMON_SYMLINK: &str = "~/.multibot/multibotd";
pub const BUNDLED_DAEMON_FILENAME: &str = "multibotd-linux-x86_64";

const MAX_VERSION_LEN: usize = 64;
const MAX_SSH_HOSTNAME_LEN: usize = 253;
const MAX_SSH_USER_LEN: usize = 64;
const MAX_CAPTURED_PIPE_BYTES: usize = 64 * 1024;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Returns the lowercase 12-hex-character SHA-256 prefix for `bytes`.
pub fn sha256_prefix_12(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format_prefix_12(&hasher.finalize())
}

fn format_prefix_12(digest: &[u8]) -> String {
    let mut out = String::with_capacity(12);
    for b in &digest[..6] {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

/// Streams a regular file in 64 KiB chunks and returns its lowercase 12-hex SHA-256 prefix.
pub fn sha256_file_prefix_12(path: &Path) -> Result<String> {
    let file = File::open(path)
        .with_context(|| format!("failed to open daemon binary {}", path.display()))?;
    let meta = file
        .metadata()
        .with_context(|| format!("failed to stat daemon binary {}", path.display()))?;
    if !meta.is_file() {
        bail!("daemon binary path is not a regular file: {}", path.display());
    }
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader
            .read(&mut buf)
            .with_context(|| format!("failed reading daemon binary {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format_prefix_12(&hasher.finalize()))
}

/// Validates a daemon version string for safe remote filename and POSIX shell interpolation.
pub fn validate_version(version: &str) -> Result<()> {
    if version.is_empty() || version.len() > MAX_VERSION_LEN {
        bail!("invalid daemon version length: {version:?}");
    }
    if version.starts_with('.') || version.starts_with('-') || version.contains("..") {
        bail!("invalid daemon version prefix/traversal: {version:?}");
    }
    if !version
        .bytes()
        .all(|b| matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
    {
        bail!("daemon version contains disallowed characters: {version:?}");
    }
    Ok(())
}

/// Validates a strict 12-character lowercase hexadecimal SHA-256 prefix.
pub fn validate_sha12(sha12: &str) -> Result<()> {
    if sha12.len() != 12
        || !sha12
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        bail!("invalid 12-char lowercase hex sha prefix: {sha12:?}");
    }
    Ok(())
}

/// Validates SSH host configuration against option injection (`-o...`), shell metacharacters
/// (CVE-2023-51385), embedded `@`, whitespace, and ASCII control chars (checked before trimming).
pub fn validate_ssh_host(host: &SshHostConfig) -> Result<()> {
    if host.hostname.bytes().any(|b| b < 0x20 || b == 0x7f) {
        bail!("invalid SSH hostname: {:?}", host.hostname);
    }
    if host.user.bytes().any(|b| b < 0x20 || b == 0x7f) {
        bail!("invalid SSH user: {:?}", host.user);
    }

    let hostname = host.hostname.trim();
    if hostname.is_empty()
        || hostname.len() > MAX_SSH_HOSTNAME_LEN
        || hostname.starts_with('-')
        || !hostname.bytes().all(|b| {
            matches!(
                b,
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b':' | b'[' | b']' | b'-'
            )
        })
    {
        bail!("invalid SSH hostname: {:?}", host.hostname);
    }

    let user = host.user.trim();
    if !user.is_empty()
        && (user.len() > MAX_SSH_USER_LEN
            || user.starts_with('-')
            || !user
                .bytes()
                .all(|b| matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-')))
    {
        bail!("invalid SSH user: {:?}", host.user);
    }

    if host.port == 0 {
        bail!("invalid SSH port: 0");
    }
    host.validated_identity_file()?;
    Ok(())
}

/// Canonical remote versioned daemon binary filename: `multibotd-<version>-<sha12>`.
pub fn remote_versioned_binary_name(version: &str, sha12: &str) -> String {
    format!("multibotd-{version}-{sha12}")
}

/// Canonical remote versioned daemon path: `~/.multibot/multibotd-<version>-<sha12>`.
pub fn remote_versioned_binary_path(version: &str, sha12: &str) -> String {
    format!(
        "{REMOTE_DAEMON_DIR}/{}",
        remote_versioned_binary_name(version, sha12)
    )
}

/// Fully validated specification for deploying a versioned + hashed `multibotd` binary to `~/.multibot/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonDeploySpec {
    pub version: String,
    pub sha12: String,
    pub remote_dir: String,
    pub remote_versioned_path: String,
    pub remote_symlink_path: String,
    pub local_binary_path: PathBuf,
}

impl DaemonDeploySpec {
    pub fn new(
        local_binary_path: PathBuf,
        version: impl Into<String>,
        sha12: impl Into<String>,
    ) -> Result<Self> {
        let version = version.into();
        let sha12 = sha12.into();
        validate_version(&version)?;
        validate_sha12(&sha12)?;
        let remote_versioned_path = remote_versioned_binary_path(&version, &sha12);
        Ok(Self {
            version,
            sha12,
            remote_dir: REMOTE_DAEMON_DIR.to_string(),
            remote_versioned_path,
            remote_symlink_path: REMOTE_DAEMON_SYMLINK.to_string(),
            local_binary_path,
        })
    }

    pub fn from_local_binary(
        local_binary_path: PathBuf,
        version: impl Into<String>,
    ) -> Result<Self> {
        let version = version.into();
        validate_version(&version)?;
        let sha12 = sha256_file_prefix_12(&local_binary_path)?;
        Self::new(local_binary_path, version, sha12)
    }

    /// Re-validates all fields and guarantees paths stay strictly confined to `~/.multibot/`.
    pub fn validate(&self) -> Result<()> {
        validate_version(&self.version)?;
        validate_sha12(&self.sha12)?;
        if self.remote_dir != REMOTE_DAEMON_DIR {
            bail!(
                "remote_dir must stay confined to {REMOTE_DAEMON_DIR}, got {:?}",
                self.remote_dir
            );
        }
        let expected_versioned = remote_versioned_binary_path(&self.version, &self.sha12);
        if self.remote_versioned_path != expected_versioned {
            bail!(
                "remote_versioned_path mismatch: expected {expected_versioned:?}, got {:?}",
                self.remote_versioned_path
            );
        }
        if self.remote_symlink_path != REMOTE_DAEMON_SYMLINK {
            bail!(
                "remote_symlink_path must be {REMOTE_DAEMON_SYMLINK}, got {:?}",
                self.remote_symlink_path
            );
        }
        Ok(())
    }
}

/// Builds a POSIX `sh` command that checks whether `~/.multibot/multibotd-<ver>-<sha12>` already
/// exists and is executable, and if so atomically updates `~/.multibot/multibotd` symlink to it.
pub fn build_check_and_link_command(spec: &DaemonDeploySpec) -> Result<String> {
    spec.validate()?;
    let bin_name = remote_versioned_binary_name(&spec.version, &spec.sha12);
    Ok(format!(
        "set -eu\n\
         dir=\"$HOME/.multibot\"\n\
         target=\"$dir/{bin_name}\"\n\
         test -f \"$target\" && test -x \"$target\" && ln -sfn \"$target\" \"$HOME/.multibot/multibotd\""
    ))
}

/// Builds a POSIX `sh` (`dash`-compatible) command that atomically receives the daemon binary on stdin
/// with `umask 077`, rejects empty streams (`test -s`), cleans up partial `.tmp.$$` files on signal/exit,
/// atomically renames into `~/.multibot/multibotd-<ver>-<sha12>`, and flips `~/.multibot/multibotd`.
pub fn build_atomic_upload_command(spec: &DaemonDeploySpec) -> Result<String> {
    spec.validate()?;
    let bin_name = remote_versioned_binary_name(&spec.version, &spec.sha12);
    Ok(format!(
        "set -eu\n\
         umask 077\n\
         dir=\"$HOME/.multibot\"\n\
         mkdir -p \"$dir\"\n\
         chmod 700 \"$dir\"\n\
         target=\"$dir/{bin_name}\"\n\
         tmp=\"$target.tmp.$$\"\n\
         trap 'rm -f \"$tmp\"' EXIT INT TERM HUP\n\
         rm -f \"$tmp\"\n\
         cat > \"$tmp\"\n\
         test -s \"$tmp\"\n\
         chmod 700 \"$tmp\"\n\
         mv -f \"$tmp\" \"$target\"\n\
         ln -sfn \"$target\" \"$HOME/.multibot/multibotd\"\n\
         trap - EXIT INT TERM HUP"
    ))
}

/// Upgrades older daemons but never downgrades newer ones.
/// Equal or unparseable versions fall back to the non-empty expected binary hash.
pub fn needs_remote_upgrade(
    remote_version: &str,
    remote_hash: &str,
    expected_version: &str,
    expected_hash: &str,
) -> bool {
    if let (Ok(remote_ver), Ok(expected_ver)) = (
        semver::Version::parse(remote_version),
        semver::Version::parse(expected_version),
    ) {
        if remote_ver != expected_ver {
            return remote_ver < expected_ver;
        }
    }
    !expected_hash.is_empty() && remote_hash != expected_hash
}

/// Inspects a wire `ServerEvent::HelloAck` to determine whether the remote `multibotd` needs upgrading.
/// Returns `false` for all non-`HelloAck` events.
pub fn hello_ack_needs_upgrade(
    event: &multibot_proto::ServerEvent,
    expected_version: &str,
    expected_sha12: &str,
) -> bool {
    match event {
        multibot_proto::ServerEvent::HelloAck {
            daemon_version,
            binary_hash,
            ..
        } => needs_remote_upgrade(daemon_version, binary_hash, expected_version, expected_sha12),
        _ => false,
    }
}

/// Locates the bundled `multibotd-linux-x86_64` regular file inside `exe_dir`.
pub fn find_bundled_daemon_binary(exe_dir: &Path) -> Option<PathBuf> {
    let candidate = exe_dir.join(BUNDLED_DAEMON_FILENAME);
    candidate.is_file().then_some(candidate)
}

/// Builds the `ssh` argv for remote `multibotd` check-and-link / atomic upload commands.
/// Re-validates `host` (including optional `identity_file`) before appending `-i <path>`
/// ahead of `--` (never shell-interpolated).
pub fn build_ssh_deploy_args(host: &SshHostConfig, remote_cmd: &str) -> Result<Vec<String>> {
    validate_ssh_host(host)?;
    let mut args = vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ConnectTimeout=10".to_string(),
    ];
    if let Some(identity) = host.validated_identity_file()? {
        args.push("-i".to_string());
        args.push(identity.to_string());
    }
    args.push("-p".to_string());
    args.push(host.port.to_string());
    args.push("--".to_string());
    args.push(host.ssh_target());
    args.push(remote_cmd.to_string());
    Ok(args)
}

pub fn configure_ssh_command(host: &SshHostConfig, remote_cmd: &str) -> Result<Command> {
    let args = build_ssh_deploy_args(host, remote_cmd)?;
    let mut cmd = Command::new("ssh");
    cmd.args(&args);
    cmd.kill_on_drop(true);
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    Ok(cmd)
}

/// Drains `reader` completely to EOF so the child process never blocks on a full pipe buffer,
/// while retaining at most the trailing `max_retained` bytes in memory.
async fn read_bounded_tail_to_eof<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    max_retained: usize,
) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt as _;
    let mut captured = VecDeque::with_capacity(max_retained.min(8192));
    let mut buf = [0u8; 8192];
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        if max_retained == 0 {
            continue;
        }
        if n >= max_retained {
            captured.clear();
            captured.extend(&buf[n - max_retained..n]);
        } else {
            let overflow = (captured.len() + n).saturating_sub(max_retained);
            if overflow > 0 {
                captured.drain(..overflow);
            }
            captured.extend(&buf[..n]);
        }
    }
    Ok(captured.into_iter().collect())
}

/// Streams a local file's bytes into a prepared upload command's stdin without holding
/// the entire file in memory, concurrently draining stdout/stderr to prevent pipe-buffer
/// deadlocks, and returning the trimmed remote path printed on stdout.
pub async fn upload_file_to_command(cmd: &mut Command, local_path: &Path) -> Result<String> {
    use tokio::io::AsyncWriteExt as _;

    let mut file = tokio::fs::File::open(local_path)
        .await
        .with_context(|| format!("failed to open local attachment {}", local_path.display()))?;

    let mut child = cmd
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn upload command")?;

    let stdin_opt = child.stdin.take();
    let stdout_opt = child.stdout.take();
    let stderr_opt = child.stderr.take();

    let stdin_fut = async move {
        if let Some(mut stdin) = stdin_opt {
            let res = tokio::io::copy(&mut file, &mut stdin).await;
            let shutdown_res = stdin.shutdown().await;
            drop(stdin);
            res.and_then(|n| shutdown_res.map(|()| n))
        } else {
            Ok(0)
        }
    };
    let stdout_fut = async move {
        match stdout_opt {
            Some(stdout) => read_bounded_tail_to_eof(stdout, MAX_CAPTURED_PIPE_BYTES).await,
            None => Ok(Vec::new()),
        }
    };
    let stderr_fut = async move {
        match stderr_opt {
            Some(stderr) => read_bounded_tail_to_eof(stderr, MAX_CAPTURED_PIPE_BYTES).await,
            None => Ok(Vec::new()),
        }
    };

    let (copy_res, stdout_res, stderr_res) = tokio::join!(stdin_fut, stdout_fut, stderr_fut);

    let status = child
        .wait()
        .await
        .context("failed waiting for upload command")?;

    let stdout_bytes = stdout_res.unwrap_or_default();
    let stderr_bytes = stderr_res.unwrap_or_default();

    if let Err(err) = copy_res {
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        bail!(
            "failed streaming {} to remote upload command ({err}): {}",
            local_path.display(),
            stderr.trim()
        );
    }

    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        bail!(
            "remote upload command failed with status {:?}: {}",
            status.code(),
            stderr.trim()
        );
    }

    let remote_path = String::from_utf8_lossy(&stdout_bytes).trim().to_string();
    if remote_path.is_empty() {
        bail!("remote upload command exited 0 without returning a remote path");
    }
    Ok(remote_path)
}

/// Uploads a local attachment file over SSH into `~/.multibot/uploads/<session>/<file>`
/// using `configure_ssh_command` and the prebuilt POSIX `remote_upload_cmd`.
pub async fn upload_file_over_ssh(
    host: &SshHostConfig,
    local_path: &Path,
    remote_upload_cmd: &str,
) -> Result<String> {
    let mut cmd = configure_ssh_command(host, remote_upload_cmd)?;
    upload_file_to_command(&mut cmd, local_path)
        .await
        .with_context(|| format!("SSH attachment upload to {}", host.ssh_target()))
}

/// Ensures `~/.multibot/multibotd-<version>-<sha12>` is atomically deployed and symlinked at
/// `~/.multibot/multibotd` on `host`, returning the validated `DaemonDeploySpec`.
pub async fn ensure_remote_daemon_deployed_with_version(
    host: &SshHostConfig,
    local_bin: &Path,
    version: &str,
) -> Result<DaemonDeploySpec> {
    validate_ssh_host(host)?;
    validate_version(version)?;

    let meta = tokio::fs::metadata(local_bin)
        .await
        .with_context(|| format!("failed to stat local daemon binary {}", local_bin.display()))?;
    if !meta.is_file() {
        bail!(
            "local daemon binary is not a regular file: {}",
            local_bin.display()
        );
    }
    if meta.len() == 0 {
        bail!("local daemon binary is empty: {}", local_bin.display());
    }

    let local_path_owned = local_bin.to_path_buf();
    let version_owned = version.to_string();
    let spec = tokio::task::spawn_blocking(move || {
        DaemonDeploySpec::from_local_binary(local_path_owned, version_owned)
    })
    .await
    .context("daemon hash worker panicked")??;

    // Fast path: check if exact version+sha12 binary already exists and re-link symlink.
    let check_cmd = build_check_and_link_command(&spec)?;
    let check_status = configure_ssh_command(host, &check_cmd)?
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .with_context(|| format!("failed executing ssh check on {}", host.ssh_target()))?;
    if check_status.success() {
        return Ok(spec);
    }

    // Slow path: stream local binary over SSH stdin into atomic .tmp -> mv -f -> ln -sfn.
    let upload_cmd = build_atomic_upload_command(&spec)?;
    let mut src = tokio::fs::File::open(local_bin)
        .await
        .with_context(|| format!("failed opening {}", local_bin.display()))?;
    let mut child = configure_ssh_command(host, &upload_cmd)?
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed spawning ssh upload to {}", host.ssh_target()))?;

    let stdin_opt = child.stdin.take();
    let stdout_opt = child.stdout.take();
    let stderr_opt = child.stderr.take();

    let stdin_fut = async move {
        use tokio::io::AsyncWriteExt as _;
        if let Some(mut stdin) = stdin_opt {
            let res = tokio::io::copy(&mut src, &mut stdin).await;
            let shutdown_res = stdin.shutdown().await;
            drop(stdin);
            res.and_then(|n| shutdown_res.map(|()| n))
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "ssh upload child missing piped stdin",
            ))
        }
    };
    let stdout_fut = async move {
        match stdout_opt {
            Some(stdout) => read_bounded_tail_to_eof(stdout, MAX_CAPTURED_PIPE_BYTES).await,
            None => Ok(Vec::new()),
        }
    };
    let stderr_fut = async move {
        match stderr_opt {
            Some(stderr) => read_bounded_tail_to_eof(stderr, MAX_CAPTURED_PIPE_BYTES).await,
            None => Ok(Vec::new()),
        }
    };

    let (copy_res, _stdout_res, stderr_res) = tokio::join!(stdin_fut, stdout_fut, stderr_fut);
    let status = child
        .wait()
        .await
        .with_context(|| format!("failed waiting for ssh upload to {}", host.ssh_target()))?;
    let stderr_bytes = stderr_res.unwrap_or_default();

    if let Err(err) = copy_res {
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        bail!(
            "failed streaming {} over ssh to {} ({err}): {}",
            local_bin.display(),
            host.ssh_target(),
            stderr.trim()
        );
    }
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        bail!(
            "remote daemon deploy to {} failed ({}): {}",
            host.ssh_target(),
            status,
            stderr.trim()
        );
    }

    Ok(spec)
}

/// Ensures `~/.multibot/multibotd-<build_version>-<sha12>` is atomically deployed on `host`,
/// returning the 12-hex SHA-256 prefix of the deployed binary.
pub async fn ensure_remote_daemon_deployed(
    host: &SshHostConfig,
    local_bin: &Path,
) -> Result<String> {
    let spec = ensure_remote_daemon_deployed_with_version(
        host,
        local_bin,
        multibot_proto::build_version(),
    )
    .await?;
    Ok(spec.sha12)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempTestDir {
        path: PathBuf,
    }

    impl TempTestDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "multibot-deploy-{label}-{}-{}",
                std::process::id(),
                TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempTestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn sha256_prefix_and_upgrade_logic() {
        let prefix = sha256_prefix_12(b"hello");
        assert_eq!(prefix.len(), 12);
        assert_eq!(prefix, "2cf24dba5fb0");
        assert_eq!(
            remote_versioned_binary_name("0.1.5", &prefix),
            "multibotd-0.1.5-2cf24dba5fb0"
        );
        assert_eq!(
            remote_versioned_binary_path("0.1.5", &prefix),
            "~/.multibot/multibotd-0.1.5-2cf24dba5fb0"
        );
        assert!(!needs_remote_upgrade("0.1.5", "2cf24dba5fb0", "0.1.5", "2cf24dba5fb0"));
        assert!(needs_remote_upgrade("0.1.4", "2cf24dba5fb0", "0.1.5", "2cf24dba5fb0"));
        assert!(needs_remote_upgrade("0.1.5", "aaaaaaaaaaaa", "0.1.5", "2cf24dba5fb0"));
        assert!(!needs_remote_upgrade("0.1.5", "", "0.1.5", ""));
    }

    #[test]
    fn newer_remote_daemon_is_not_downgraded() {
        assert!(!needs_remote_upgrade("0.1.48", "aaaaaaaaaaaa", "0.1.47", "2cf24dba5fb0"));
    }

    #[test]
    fn older_remote_daemon_is_upgraded_even_with_matching_hash() {
        assert!(needs_remote_upgrade("0.1.46", "2cf24dba5fb0", "0.1.47", "2cf24dba5fb0"));
        assert!(needs_remote_upgrade("0.1.9", "2cf24dba5fb0", "0.1.47", "2cf24dba5fb0"));
    }

    #[test]
    fn same_version_daemon_is_refreshed_when_hash_differs() {
        assert!(needs_remote_upgrade("0.1.47", "aaaaaaaaaaaa", "0.1.47", "2cf24dba5fb0"));
    }

    #[test]
    fn same_version_daemon_is_kept_when_hash_matches() {
        assert!(!needs_remote_upgrade("0.1.47", "2cf24dba5fb0", "0.1.47", "2cf24dba5fb0"));
    }

    #[test]
    fn invalid_versions_fall_back_to_hash_comparison() {
        for (remote, expected) in [("invalid", "0.1.47"), ("0.1.47", "invalid"), ("invalid", "other")] {
            assert!(needs_remote_upgrade(remote, "aaaaaaaaaaaa", expected, "2cf24dba5fb0"));
            assert!(!needs_remote_upgrade(remote, "2cf24dba5fb0", expected, "2cf24dba5fb0"));
            assert!(!needs_remote_upgrade(remote, "aaaaaaaaaaaa", expected, ""));
        }
    }

    #[test]
    fn sha256_file_prefix_12_streams_multi_chunk_file() {
        let dir = TempTestDir::new("stream-hash");
        let bin = dir.path().join("multibotd-linux-x86_64");
        // >128 KiB to cross multiple 64 KiB read chunks.
        let bytes: Vec<u8> = (0..(140 * 1024)).map(|i| ((i * 31) % 256) as u8).collect();
        fs::write(&bin, &bytes).unwrap();

        let streamed = sha256_file_prefix_12(&bin).unwrap();
        assert_eq!(streamed, sha256_prefix_12(&bytes));

        // Missing file and directory must return Err.
        assert!(sha256_file_prefix_12(&dir.path().join("missing")).is_err());
        assert!(sha256_file_prefix_12(dir.path()).is_err());
    }

    #[test]
    fn validation_rejects_shell_injection_and_malformed_inputs() {
        // Valid versions
        for ok in ["0.1.5", "0.1.42-rc.1", "1.0.0_hotfix"] {
            validate_version(ok).unwrap_or_else(|e| panic!("expected valid version {ok:?}: {e}"));
        }
        // Invalid versions (injection, traversal, empty, overlong)
        let long_ver = "a".repeat(65);
        for bad in [
            "",
            "-x",
            ".hidden",
            "0.1..2",
            "0.1.5;rm -rf /",
            "0.1.5$(id)",
            "0.1.5`id`",
            "0.1.5'a",
            "0.1.5\"a",
            "0.1.5/a",
            "0.1.5\nfoo",
            "0.1.5 a",
            long_ver.as_str(),
        ] {
            assert!(
                validate_version(bad).is_err(),
                "expected invalid version to be rejected: {bad:?}"
            );
        }

        // SHA-12 validation
        validate_sha12("0123456789ab").unwrap();
        for bad in [
            "0123456789a",
            "0123456789abc",
            "0123456789AB",
            "0123456789ag",
            "0123456;id #",
        ] {
            assert!(validate_sha12(bad).is_err(), "expected invalid sha12: {bad:?}");
        }

        // SSH host validation (valid DNS, IPv4, bracketed IPv6, valid/empty usernames)
        let good_host = SshHostConfig {
            name: "vps".into(),
            hostname: "vps.example.com".into(),
            user: "ubuntu".into(),
            port: 22,
            identity_file: None,
        };
        for valid in [
            good_host.clone(),
            SshHostConfig {
                hostname: "192.168.1.42".into(),
                user: "deploy_user-01.ops".into(),
                ..good_host.clone()
            },
            SshHostConfig {
                hostname: "[2001:db8::1]".into(),
                user: "".into(),
                ..good_host.clone()
            },
            SshHostConfig {
                hostname: "fe80::1".into(),
                user: "root".into(),
                ..good_host.clone()
            },
            SshHostConfig {
                hostname: "node_01.prod-eu.internal".into(),
                user: "a".repeat(64),
                ..good_host.clone()
            },
            SshHostConfig {
                identity_file: Some(
                    r"C:\Users\Valentin Yeo\AppData\Roaming\ZigShell\ssh-key\id_ed25519".into(),
                ),
                ..good_host.clone()
            },
            SshHostConfig {
                identity_file: Some("~/.ssh/id_ed25519".into()),
                ..good_host.clone()
            },
        ] {
            validate_ssh_host(&valid)
                .unwrap_or_else(|e| panic!("expected valid SSH host {valid:?}: {e}"));
        }

        let overlong_host = "a".repeat(254);
        let overlong_user = "u".repeat(65);
        let bad_hosts = [
            SshHostConfig {
                hostname: "".into(),
                ..good_host.clone()
            },
            SshHostConfig {
                hostname: "   ".into(),
                ..good_host.clone()
            },
            SshHostConfig {
                hostname: "-oProxyCommand=sh".into(),
                ..good_host.clone()
            },
            SshHostConfig {
                hostname: "vps example.com".into(),
                ..good_host.clone()
            },
            SshHostConfig {
                hostname: overlong_host,
                ..good_host.clone()
            },
            // Leading / trailing ASCII control chars must be rejected BEFORE trim.
            SshHostConfig {
                hostname: "vps.example.com\n".into(),
                ..good_host.clone()
            },
            SshHostConfig {
                hostname: "\nvps.example.com".into(),
                ..good_host.clone()
            },
            SshHostConfig {
                hostname: "\rvps.example.com\r".into(),
                ..good_host.clone()
            },
            SshHostConfig {
                hostname: "\tvps.example.com".into(),
                ..good_host.clone()
            },
            SshHostConfig {
                hostname: "vps.example.com\x7f".into(),
                ..good_host.clone()
            },
            // CVE-2023-51385 shell metacharacters & @ injection in hostname.
            SshHostConfig {
                hostname: "vps'$(id)'".into(),
                ..good_host.clone()
            },
            SshHostConfig {
                hostname: "vps\"$(id)\"".into(),
                ..good_host.clone()
            },
            SshHostConfig {
                hostname: "vps`id`".into(),
                ..good_host.clone()
            },
            SshHostConfig {
                hostname: "vps$(id)".into(),
                ..good_host.clone()
            },
            SshHostConfig {
                hostname: "vps;id".into(),
                ..good_host.clone()
            },
            SshHostConfig {
                hostname: "vps|id".into(),
                ..good_host.clone()
            },
            SshHostConfig {
                hostname: "vps&id".into(),
                ..good_host.clone()
            },
            SshHostConfig {
                hostname: "vps/path".into(),
                ..good_host.clone()
            },
            SshHostConfig {
                hostname: r"vps\path".into(),
                ..good_host.clone()
            },
            SshHostConfig {
                hostname: "evil@vps.example.com".into(),
                ..good_host.clone()
            },
            // Invalid users: option injection, control chars, shell metacharacters, @, overlong.
            SshHostConfig {
                user: "-oBad".into(),
                ..good_host.clone()
            },
            SshHostConfig {
                user: "u@h".into(),
                ..good_host.clone()
            },
            SshHostConfig {
                user: "ubuntu\n".into(),
                ..good_host.clone()
            },
            SshHostConfig {
                user: "\rubuntu".into(),
                ..good_host.clone()
            },
            SshHostConfig {
                user: "\tubuntu".into(),
                ..good_host.clone()
            },
            SshHostConfig {
                user: "u'$(id)'".into(),
                ..good_host.clone()
            },
            SshHostConfig {
                user: "u\"$(id)\"".into(),
                ..good_host.clone()
            },
            SshHostConfig {
                user: "u`id`".into(),
                ..good_host.clone()
            },
            SshHostConfig {
                user: "u$(id)".into(),
                ..good_host.clone()
            },
            SshHostConfig {
                user: "u;id".into(),
                ..good_host.clone()
            },
            SshHostConfig {
                user: "u|id".into(),
                ..good_host.clone()
            },
            SshHostConfig {
                user: overlong_user,
                ..good_host.clone()
            },
            SshHostConfig {
                port: 0,
                ..good_host.clone()
            },
            SshHostConfig {
                identity_file: Some("".into()),
                ..good_host.clone()
            },
            SshHostConfig {
                identity_file: Some("   ".into()),
                ..good_host.clone()
            },
            SshHostConfig {
                identity_file: Some("-oProxyCommand=sh".into()),
                ..good_host.clone()
            },
            SshHostConfig {
                identity_file: Some("/key\nfoo".into()),
                ..good_host.clone()
            },
            SshHostConfig {
                identity_file: Some("/key\rfoo".into()),
                ..good_host.clone()
            },
            SshHostConfig {
                identity_file: Some("/key\0foo".into()),
                ..good_host.clone()
            },
            SshHostConfig {
                identity_file: Some("/key\tfoo".into()),
                ..good_host.clone()
            },
        ];
        for host in bad_hosts {
            assert!(
                validate_ssh_host(&host).is_err(),
                "expected invalid SSH host to be rejected: {host:?}"
            );
        }

        // Verify build_ssh_deploy_args passes -i <path> strictly before -- and rejects injection.
        let args_none = build_ssh_deploy_args(&good_host, "true").unwrap();
        assert!(!args_none.contains(&"-i".to_string()));
        let with_key = SshHostConfig {
            identity_file: Some("C:/Users/Val/ZigShell/ssh-key/multibot_ed25519".into()),
            ..good_host.clone()
        };
        let args_key = build_ssh_deploy_args(&with_key, "true").unwrap();
        let i_pos = args_key.iter().position(|a| a == "-i").unwrap();
        let dd_pos = args_key.iter().position(|a| a == "--").unwrap();
        assert!(i_pos + 1 < dd_pos);
        assert_eq!(
            args_key[i_pos + 1],
            "C:/Users/Val/ZigShell/ssh-key/multibot_ed25519"
        );
        for bad in ["-oProxyCommand=id", "/k\nid", "/k\0id", ""] {
            let bad_host = SshHostConfig {
                identity_file: Some(bad.into()),
                ..good_host.clone()
            };
            assert!(build_ssh_deploy_args(&bad_host, "true").is_err());
        }

        // DaemonDeploySpec::validate rejects tampered remote paths.
        let mut spec =
            DaemonDeploySpec::new(PathBuf::from("multibotd"), "0.1.5", "0123456789ab").unwrap();
        spec.validate().unwrap();
        spec.remote_dir = "~/.zigshell".into();
        assert!(spec.validate().is_err());

        let mut spec2 =
            DaemonDeploySpec::new(PathBuf::from("multibotd"), "0.1.5", "0123456789ab").unwrap();
        spec2.remote_versioned_path = "/tmp/pwned".into();
        assert!(spec2.validate().is_err());

        let mut spec3 =
            DaemonDeploySpec::new(PathBuf::from("multibotd"), "0.1.5", "0123456789ab").unwrap();
        spec3.remote_symlink_path = "/usr/local/bin/multibotd".into();
        assert!(spec3.validate().is_err());
    }

    #[test]
    fn hello_ack_needs_upgrade_checks_version_and_hash() {
        let ack = |ver: &str, hash: &str| multibot_proto::ServerEvent::HelloAck {
            protocol_version: 1,
            daemon_version: ver.into(),
            hostname: "vps".into(),
            pid: 123,
            binary_hash: hash.into(),
        };

        assert!(!hello_ack_needs_upgrade(&ack("0.1.5", "0123456789ab"), "0.1.5", "0123456789ab"));
        assert!(hello_ack_needs_upgrade(&ack("0.1.4", "0123456789ab"), "0.1.5", "0123456789ab"));
        assert!(hello_ack_needs_upgrade(&ack("0.1.5", "ffffffffffff"), "0.1.5", "0123456789ab"));
        // Older Slice 1 daemon with empty binary_hash upgrades when expected_sha12 is non-empty.
        assert!(hello_ack_needs_upgrade(&ack("0.1.5", ""), "0.1.5", "0123456789ab"));
        assert!(!hello_ack_needs_upgrade(&ack("0.1.5", ""), "0.1.5", ""));
        // Non-HelloAck events always return false.
        assert!(!hello_ack_needs_upgrade(
            &multibot_proto::ServerEvent::Pong {
                nonce: 0,
                server_time_ms: 0,
            },
            "0.1.5",
            "0123456789ab"
        ));
    }

    #[test]
    fn find_bundled_daemon_binary_finds_only_regular_file() {
        let dir = TempTestDir::new("bundled-find");
        assert_eq!(find_bundled_daemon_binary(dir.path()), None);

        // Directory with the same name must not match.
        let as_dir = dir.path().join(BUNDLED_DAEMON_FILENAME);
        fs::create_dir(&as_dir).unwrap();
        assert_eq!(find_bundled_daemon_binary(dir.path()), None);
        fs::remove_dir(&as_dir).unwrap();

        // Regular file matches.
        fs::write(&as_dir, b"\x7fELF").unwrap();
        assert_eq!(find_bundled_daemon_binary(dir.path()), Some(as_dir));
    }

    #[test]
    fn build_atomic_upload_command_contains_required_safety_guards() {
        let spec =
            DaemonDeploySpec::new(PathBuf::from("multibotd"), "0.1.9", "abcdef012345").unwrap();
        let cmd = build_atomic_upload_command(&spec).unwrap();
        for snippet in [
            "umask 077",
            "mkdir -p \"$dir\"",
            "chmod 700 \"$dir\"",
            "chmod 700 \"$tmp\"",
            "test -s \"$tmp\"",
            "mv -f \"$tmp\" \"$target\"",
            "ln -sfn \"$target\" \"$HOME/.multibot/multibotd\"",
            "trap 'rm -f \"$tmp\"' EXIT INT TERM HUP",
            "multibotd-0.1.9-abcdef012345",
        ] {
            assert!(
                cmd.contains(snippet),
                "expected upload command to contain {snippet:?}, got:\n{cmd}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn atomic_upload_command_real_sh_execution_and_rollback_safety() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command as StdCommand;

        let home_dir = TempTestDir::new("sh-exec");
        let run_sh_with_stdin = |script: &str, payload: &[u8]| -> std::process::ExitStatus {
            let mut child = StdCommand::new("sh")
                .arg("-c")
                .arg(script)
                .env("HOME", home_dir.path())
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            {
                let mut stdin = child.stdin.take().unwrap();
                stdin.write_all(payload).unwrap();
            }
            child.wait().unwrap()
        };

        // 1. Deploy v1 payload.
        let v1_bytes = b"\x7fELF-multibotd-v1";
        let v1_sha = sha256_prefix_12(v1_bytes);
        let spec_v1 = DaemonDeploySpec::new(PathBuf::from("v1"), "0.1.1", &v1_sha).unwrap();
        let upload_v1 = build_atomic_upload_command(&spec_v1).unwrap();
        assert!(run_sh_with_stdin(&upload_v1, v1_bytes).success());

        let remote_dir = home_dir.path().join(".multibot");
        let v1_target = remote_dir.join(remote_versioned_binary_name("0.1.1", &v1_sha));
        let symlink_path = remote_dir.join("multibotd");

        assert_eq!(fs::metadata(&remote_dir).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(&v1_target).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::read(&v1_target).unwrap(), v1_bytes);
        assert_eq!(fs::read_link(&symlink_path).unwrap(), v1_target);

        // 2. Check-and-link succeeds for v1, fails for missing v2.
        let check_v1 = build_check_and_link_command(&spec_v1).unwrap();
        assert!(run_sh_with_stdin(&check_v1, b"").success());

        let v2_bytes = b"\x7fELF-multibotd-v2-upgraded";
        let v2_sha = sha256_prefix_12(v2_bytes);
        let spec_v2 = DaemonDeploySpec::new(PathBuf::from("v2"), "0.1.2", &v2_sha).unwrap();
        let check_v2 = build_check_and_link_command(&spec_v2).unwrap();
        assert!(!run_sh_with_stdin(&check_v2, b"").success());

        // 3. Deploy v2 -> symlink flips atomically while v1 remains versioned.
        let upload_v2 = build_atomic_upload_command(&spec_v2).unwrap();
        assert!(run_sh_with_stdin(&upload_v2, v2_bytes).success());
        let v2_target = remote_dir.join(remote_versioned_binary_name("0.1.2", &v2_sha));
        assert_eq!(fs::read(&v2_target).unwrap(), v2_bytes);
        assert_eq!(fs::read_link(&symlink_path).unwrap(), v2_target);
        assert!(v1_target.exists());

        // 4. Empty upload for v3 fails closed: v2 symlink untouched, v3 absent, zero .tmp.* left!
        let spec_v3 =
            DaemonDeploySpec::new(PathBuf::from("v3"), "0.1.3", "000000000000").unwrap();
        let upload_v3 = build_atomic_upload_command(&spec_v3).unwrap();
        assert!(!run_sh_with_stdin(&upload_v3, b"").success());

        let v3_target = remote_dir.join(remote_versioned_binary_name("0.1.3", "000000000000"));
        assert!(!v3_target.exists());
        assert_eq!(fs::read_link(&symlink_path).unwrap(), v2_target);
        assert_eq!(fs::read(&symlink_path).unwrap(), v2_bytes);

        let leftover_tmps: Vec<_> = fs::read_dir(&remote_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(
            leftover_tmps.is_empty(),
            "expected zero leftover .tmp files, found: {leftover_tmps:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn upload_file_to_command_handles_concurrent_large_stdin_and_stderr_without_deadlock() {
        use std::time::Duration;

        let dir = TempTestDir::new("concurrent-pipes");
        let local_file = dir.path().join("big-upload.bin");
        let remote_dest = dir.path().join("uploaded.bin");

        // >128 KiB payload to exceed both Windows (4 KiB) and Linux (64 KiB) pipe buffers.
        let payload: Vec<u8> = (0..(192 * 1024))
            .map(|i| ((i * 73 + 19) % 251) as u8)
            .collect();
        fs::write(&local_file, &payload).unwrap();

        // Child writes >128 KiB to stderr BEFORE reading stdin (worst-case deadlock trigger
        // if stdin is copied sequentially before draining stderr), then copies stdin to dest
        // and prints the remote path to stdout.
        let script = format!(
            "dd if=/dev/zero bs=1024 count=160 2>/dev/null | tr '\\0' 'E' >&2; \
             cat > '{}'; \
             printf '%s\\n' '{}'",
            remote_dest.display(),
            remote_dest.display()
        );
        let mut cmd = Command::new("sh");
        cmd.args(["-c", &script]);

        let returned = tokio::time::timeout(
            Duration::from_secs(10),
            upload_file_to_command(&mut cmd, &local_file),
        )
        .await
        .expect("upload_file_to_command deadlocked on concurrent >128 KiB stdin + stderr")
        .expect("upload_file_to_command should succeed when draining pipes concurrently");

        assert_eq!(returned, remote_dest.display().to_string());
        assert_eq!(fs::read(&remote_dest).unwrap(), payload);

        // Also verify early exit (BrokenPipe on stdin) still drains bounded stderr and preserves error detail.
        let mut fail_cmd = Command::new("sh");
        fail_cmd.args([
            "-c",
            "dd if=/dev/zero bs=1024 count=80 2>/dev/null | tr '\\0' 'X' >&2; \
             printf '\\nDISK_FULL_TAIL_MARKER\\n' >&2; \
             exit 1",
        ]);
        let err = tokio::time::timeout(
            Duration::from_secs(10),
            upload_file_to_command(&mut fail_cmd, &local_file),
        )
        .await
        .expect("early-failing upload must not deadlock")
        .expect_err("early-failing upload must return Err");
        let err_msg = format!("{err:#}");
        assert!(
            err_msg.contains("DISK_FULL_TAIL_MARKER"),
            "expected remote stderr tail preserved across BrokenPipe, got: {err_msg}"
        );
    }
}
