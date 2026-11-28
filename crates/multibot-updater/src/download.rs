use crate::client::{UpdateClient, get_following_allowed_redirects};
use crate::security::{
    ALLOWED_ZIP_FILES, MAX_UPDATE_ARCHIVE_BYTES, format_sha256_digest,
    is_allowed_https_url_with_local, is_safe_zip_entry_path, is_valid_sha256_hex,
    normalize_version, verify_sha256_hex,
};
use crate::{ReleaseInfo, StagedUpdate};
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static STAGING_COUNTER: AtomicU64 = AtomicU64::new(0);

struct StagingDirGuard {
    path: PathBuf,
    keep: bool,
}

impl StagingDirGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, keep: false }
    }

    fn keep(&mut self) {
        self.keep = true;
    }
}

impl Drop for StagingDirGuard {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

struct ExtractedFilesGuard {
    paths: Vec<PathBuf>,
    keep: bool,
}

impl ExtractedFilesGuard {
    fn new() -> Self {
        Self {
            paths: Vec::new(),
            keep: false,
        }
    }

    fn track(&mut self, path: PathBuf) {
        self.paths.push(path);
    }

    fn keep(&mut self) {
        self.keep = true;
    }
}

impl Drop for ExtractedFilesGuard {
    fn drop(&mut self) {
        if !self.keep {
            for p in &self.paths {
                let _ = fs::remove_file(p);
            }
        }
    }
}

/// Extracts ONLY the allowlisted `multibot.exe` and `multibotd-linux-x86_64` binaries from
/// `zip_path` into `staging_dir`, rejecting symlinks, zip-slip paths, duplicate entries, and
/// zip bombs exceeding `MAX_UPDATE_ARCHIVE_BYTES`, while verifying per-binary SHA-256 digests.
pub fn extract_and_verify_archive(
    zip_path: &Path,
    staging_dir: &Path,
    expected_exe_sha256: &str,
    expected_daemon_sha256: &str,
) -> Result<(PathBuf, PathBuf)> {
    extract_and_verify_archive_with_limit(
        zip_path,
        staging_dir,
        expected_exe_sha256,
        expected_daemon_sha256,
        MAX_UPDATE_ARCHIVE_BYTES,
    )
}

pub(crate) fn extract_and_verify_archive_with_limit(
    zip_path: &Path,
    staging_dir: &Path,
    expected_exe_sha256: &str,
    expected_daemon_sha256: &str,
    max_uncompressed_bytes: u64,
) -> Result<(PathBuf, PathBuf)> {
    if !is_valid_sha256_hex(expected_exe_sha256) {
        bail!("invalid expected multibot.exe SHA-256");
    }
    if !is_valid_sha256_hex(expected_daemon_sha256) {
        bail!("invalid expected multibotd-linux-x86_64 SHA-256");
    }

    let zip_file = File::open(zip_path)
        .with_context(|| format!("failed to open archive {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(zip_file).context("failed to read zip archive")?;

    let mut total_uncompressed: u64 = 0;
    let mut exe_path: Option<PathBuf> = None;
    let mut daemon_path: Option<PathBuf> = None;
    let mut extracted_guard = ExtractedFilesGuard::new();

    for i in 0..archive.len() {
        let entry = archive.by_index(i)?;
        let raw_name = entry.name().to_string();

        // Gate 1: Reject symlinks anywhere in archive.
        if entry.is_symlink() {
            bail!("symlink entry rejected in zip archive: {raw_name}");
        }

        // Gate 2: Reject zip-slip or unsafe paths anywhere in archive.
        if !is_safe_zip_entry_path(Path::new(&raw_name)) {
            bail!("unsafe zip entry path rejected: {raw_name}");
        }
        let Some(enclosed) = entry.enclosed_name() else {
            bail!("unsafe zip entry path rejected: {raw_name}");
        };
        if !is_safe_zip_entry_path(&enclosed) {
            bail!("unsafe zip entry path rejected: {raw_name}");
        }

        // Gate 3: Allowlist — extract ONLY canonical multibot.exe and multibotd-linux-x86_64,
        // and reject any shadowed/case-colliding/./-prefixed variants (Windows NTFS defense).
        let canonical_rel: Vec<String> = enclosed
            .components()
            .filter_map(|c| match c {
                std::path::Component::Normal(s) => Some(s.to_string_lossy().to_ascii_lowercase()),
                _ => None,
            })
            .collect();
        let matches_allowlisted_name = canonical_rel.len() == 1
            && ALLOWED_ZIP_FILES
                .iter()
                .any(|allowed| canonical_rel[0] == allowed.to_ascii_lowercase());

        if matches_allowlisted_name && !ALLOWED_ZIP_FILES.contains(&raw_name.as_str()) {
            bail!("duplicate or shadowed allowlisted entry in zip archive: {raw_name}");
        }
        if !ALLOWED_ZIP_FILES.contains(&raw_name.as_str()) {
            continue;
        }

        // Gate 4: Reject directory entries and duplicate allowlisted filenames.
        if entry.is_dir() {
            bail!("allowlisted zip entry must be a regular file, not a directory: {raw_name}");
        }
        let (slot, expected_sha) = if raw_name == "multibot.exe" {
            (&mut exe_path, expected_exe_sha256)
        } else {
            (&mut daemon_path, expected_daemon_sha256)
        };
        if slot.is_some() {
            bail!("duplicate allowlisted entry in zip archive: {raw_name}");
        }

        // Gate 5: Pre-check declared uncompressed header size.
        if entry.size() > max_uncompressed_bytes
            || total_uncompressed.saturating_add(entry.size()) > max_uncompressed_bytes
        {
            bail!("uncompressed zip entry exceeded maximum allowed size (zip bomb protection)");
        }

        // Gate 6: Stream with hard uncompressed cap + on-the-fly SHA-256 verification.
        let dest_path = staging_dir.join(&raw_name);
        let remaining = max_uncompressed_bytes.saturating_sub(total_uncompressed);
        let mut limited = entry.take(remaining.saturating_add(1));
        extracted_guard.track(dest_path.clone());
        let mut out_file = File::create(&dest_path)
            .with_context(|| format!("failed creating staged binary {}", dest_path.display()))?;
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 64 * 1024];
        let mut entry_bytes: u64 = 0;

        loop {
            let n = limited.read(&mut buf)?;
            if n == 0 {
                break;
            }
            entry_bytes = entry_bytes.saturating_add(n as u64);
            if total_uncompressed
                .checked_add(entry_bytes)
                .is_none_or(|t| t > max_uncompressed_bytes)
            {
                bail!("uncompressed zip entry exceeded maximum allowed size (zip bomb protection)");
            }
            out_file.write_all(&buf[..n])?;
            hasher.update(&buf[..n]);
        }
        out_file.flush()?;
        out_file.sync_all()?;

        if entry_bytes == 0 {
            bail!("extracted binary {raw_name} is empty");
        }
        total_uncompressed = total_uncompressed.saturating_add(entry_bytes);

        let actual_sha = format_sha256_digest(&hasher.finalize());
        verify_sha256_hex(&actual_sha, expected_sha, &raw_name)?;

        *slot = Some(dest_path);
    }

    let exe = exe_path.context("update archive missing required file: multibot.exe")?;
    let daemon =
        daemon_path.context("update archive missing required file: multibotd-linux-x86_64")?;
    extracted_guard.keep();
    Ok((exe, daemon))
}

/// Downloads, SHA-256 verifies, and extracts `release` into a fresh staging directory under `updates_dir`.
/// Automatically cleans up the staging directory on any failure.
pub fn download_and_stage_update<F>(
    client: &UpdateClient,
    release: &ReleaseInfo,
    updates_dir: &Path,
    on_progress: F,
) -> Result<StagedUpdate>
where
    F: FnMut(u64, Option<u64>),
{
    client.download_and_stage_update(release, updates_dir, on_progress)
}

impl UpdateClient {
    /// Downloads, verifies, and stages `release` into `updates_dir`.
    pub fn download_and_stage_update<F>(
        &self,
        release: &ReleaseInfo,
        updates_dir: &Path,
        on_progress: F,
    ) -> Result<StagedUpdate>
    where
        F: FnMut(u64, Option<u64>),
    {
        self.download_and_stage_update_with_limit(
            release,
            updates_dir,
            MAX_UPDATE_ARCHIVE_BYTES,
            on_progress,
        )
    }

    pub(crate) fn download_and_stage_update_with_limit<F>(
        &self,
        release: &ReleaseInfo,
        updates_dir: &Path,
        max_bytes: u64,
        mut on_progress: F,
    ) -> Result<StagedUpdate>
    where
        F: FnMut(u64, Option<u64>),
    {
        // Pre-flight validation.
        let safe_ver = normalize_version(&release.version)?;
        if !is_allowed_https_url_with_local(&release.asset.url, self.config.allow_local_http) {
            bail!(
                "blocked disallowed release asset URL: {}",
                release.asset.url
            );
        }
        if release.asset.size_bytes > max_bytes {
            bail!(
                "declared archive size {} exceeds maximum allowed size {max_bytes}",
                release.asset.size_bytes
            );
        }
        if !is_valid_sha256_hex(&release.asset.sha256)
            || !is_valid_sha256_hex(&release.asset.multibot_exe_sha256)
            || !is_valid_sha256_hex(&release.asset.multibotd_linux_x86_64_sha256)
        {
            bail!("release asset contains invalid SHA-256 metadata");
        }

        fs::create_dir_all(updates_dir)
            .with_context(|| format!("failed to create updates dir {}", updates_dir.display()))?;

        let staging_dir = updates_dir.join(format!(
            "staging-{safe_ver}-{}-{}",
            std::process::id(),
            STAGING_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&staging_dir)
            .with_context(|| format!("failed to create staging dir {}", staging_dir.display()))?;
        let mut guard = StagingDirGuard::new(staging_dir.clone());

        let (_final_url, mut response) = get_following_allowed_redirects(
            &self.download_agent,
            &release.asset.url,
            &self.user_agent(),
            &[],
            self.config.allow_local_http,
        )?;
        let status = response.status().as_u16();
        if status != 200 {
            bail!(
                "update archive download from {} returned HTTP {status}",
                release.asset.url
            );
        }

        let header_len = response.body().content_length();
        if let Some(len) = header_len
            && len > max_bytes
        {
            bail!("update download Content-Length {len} exceeds maximum allowed size {max_bytes}");
        }
        let bytes_total = header_len
            .or_else(|| (release.asset.size_bytes > 0).then_some(release.asset.size_bytes));

        let zip_path = staging_dir.join("archive.zip.part");
        {
            let mut reader = response.body_mut().as_reader();
            let mut out = File::create(&zip_path)
                .with_context(|| format!("failed to create {}", zip_path.display()))?;
            let mut hasher = Sha256::new();
            let mut buf = [0u8; 64 * 1024];
            let mut bytes_done: u64 = 0;

            loop {
                let n = reader
                    .read(&mut buf)
                    .context("error reading update download stream")?;
                if n == 0 {
                    break;
                }
                bytes_done = match bytes_done.checked_add(n as u64) {
                    Some(next) if next <= max_bytes => next,
                    _ => bail!("update download exceeded size cap of {max_bytes} bytes"),
                };
                out.write_all(&buf[..n])?;
                hasher.update(&buf[..n]);
                on_progress(bytes_done, bytes_total);
            }
            out.flush()?;
            out.sync_all()?;

            let actual_zip_sha256 = format_sha256_digest(&hasher.finalize());
            verify_sha256_hex(&actual_zip_sha256, &release.asset.sha256, "archive zip")?;
        }

        let (exe_path, daemon_path) = extract_and_verify_archive_with_limit(
            &zip_path,
            &staging_dir,
            &release.asset.multibot_exe_sha256,
            &release.asset.multibotd_linux_x86_64_sha256,
            max_bytes,
        )?;

        let _ = fs::remove_file(&zip_path);
        guard.keep();

        Ok(StagedUpdate {
            version: safe_ver,
            tag: release.tag.clone(),
            staging_dir,
            multibot_exe_path: exe_path,
            multibotd_linux_path: daemon_path,
            multibot_exe_sha256: release.asset.multibot_exe_sha256.to_ascii_lowercase(),
            multibotd_linux_x86_64_sha256: release
                .asset
                .multibotd_linux_x86_64_sha256
                .to_ascii_lowercase(),
            notes: release.notes.clone(),
        })
    }
}
