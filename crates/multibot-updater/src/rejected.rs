use crate::security::{has_ascii_control_chars, parse_semver_version};
use anyhow::{Context, Result, bail};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Filename inside `%APPDATA%\Multibot\updates\` tracking poison-pill versions that failed readiness.
pub const REJECTED_FILENAME: &str = "rejected.txt";

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Returns `<updates_dir>/rejected.txt`.
pub fn rejected_file_path(updates_dir: &Path) -> PathBuf {
    updates_dir.join(REJECTED_FILENAME)
}

fn normalize_rejected_version(raw: &str) -> Result<String> {
    let mut ver = parse_semver_version(raw)?;
    ver.build = semver::BuildMetadata::EMPTY;
    Ok(ver.to_string())
}

fn canonical_entry(raw: &str) -> String {
    normalize_rejected_version(raw).unwrap_or_else(|_| {
        let trimmed = raw.trim();
        let no_v = trimmed.strip_prefix('v').unwrap_or(trimmed);
        no_v.split_once('+')
            .map(|(head, _)| head)
            .unwrap_or(no_v)
            .to_string()
    })
}

/// Reads non-empty, non-comment entries from `<updates_dir>/rejected.txt`.
/// Returns an empty list if the file does not exist.
pub fn read_rejected_versions(updates_dir: &Path) -> Result<Vec<String>> {
    let path = rejected_file_path(updates_dir);
    let content = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(err).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    let mut versions = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        versions.push(trimmed.to_string());
    }
    Ok(versions)
}

/// Returns `true` if `version_or_tag` is recorded in `<updates_dir>/rejected.txt`
/// (normalizing optional single leading `v`).
pub fn is_version_rejected(updates_dir: &Path, version_or_tag: &str) -> Result<bool> {
    let target = canonical_entry(version_or_tag);
    if target.is_empty() {
        return Ok(false);
    }
    let existing = read_rejected_versions(updates_dir)?;
    Ok(existing
        .iter()
        .any(|entry| canonical_entry(entry) == target))
}

/// Persists `version_or_tag` (normalized SemVer without leading `v`) into `<updates_dir>/rejected.txt`
/// atomically via a temporary file + rename. Idempotent if already recorded.
pub fn record_rejected_version(updates_dir: &Path, version_or_tag: &str) -> Result<()> {
    if version_or_tag.trim().is_empty() || has_ascii_control_chars(version_or_tag) {
        bail!("invalid version for rejected.txt: {version_or_tag:?}");
    }
    let normalized = normalize_rejected_version(version_or_tag)?;
    let mut existing = read_rejected_versions(updates_dir)?;
    if existing
        .iter()
        .any(|entry| canonical_entry(entry) == normalized)
    {
        return Ok(());
    }
    existing.push(normalized);

    fs::create_dir_all(updates_dir)
        .with_context(|| format!("failed to create updates dir {}", updates_dir.display()))?;

    let path = rejected_file_path(updates_dir);
    let tmp_name = format!(
        "{REJECTED_FILENAME}.{}.{}.tmp",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let tmp_path = updates_dir.join(tmp_name);

    let mut body = String::new();
    for item in &existing {
        body.push_str(&canonical_entry(item));
        body.push('\n');
    }

    let write_res = (|| -> Result<()> {
        let mut f = fs::File::create(&tmp_path)
            .with_context(|| format!("failed to create {}", tmp_path.display()))?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }

    if let Err(err) = fs::rename(&tmp_path, &path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err).with_context(|| {
            format!(
                "failed to atomically update rejected file {}",
                path.display()
            )
        });
    }
    Ok(())
}
