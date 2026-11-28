use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use semver::Version;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{BufReader, Cursor, Read};
use std::path::{Component, Path};
use url::Url;

/// Maximum permitted update archive size AND total uncompressed extraction size (256 MiB).
pub const MAX_UPDATE_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
/// Maximum permitted size for metadata payloads (GitHub API JSON, `update-manifest.json`, `SHA256SUMS.txt`).
pub const MAX_METADATA_BYTES: u64 = 2 * 1024 * 1024;
/// Maximum number of HTTP redirects followed per request.
pub const MAX_REDIRECT_HOPS: usize = 5;
/// Strict allowlist of files permitted to be extracted from `multibot-windows-x64.zip`.
pub const ALLOWED_ZIP_FILES: [&str; 2] = ["multibot.exe", "multibotd-linux-x86_64"];
/// Environment variable used by tests / local staging to override the GitHub Releases API endpoint.
pub const ENV_UPDATE_URL: &str = "MULTIBOT_UPDATE_URL";

pub(crate) fn has_ascii_control_chars(s: &str) -> bool {
    s.bytes().any(|b| b < 0x20 || b == 0x7f)
}

/// Computes lowercase hex SHA-256 digest of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format_sha256_digest(&hasher.finalize())
}

pub(crate) fn format_sha256_digest(digest: &[u8]) -> String {
    let mut out = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

/// Streams a regular file in 64 KiB chunks and returns its lowercase 64-hex SHA-256 digest.
pub fn sha256_file(path: &Path) -> Result<String> {
    let file = File::open(path)
        .with_context(|| format!("failed to open file for SHA-256: {}", path.display()))?;
    let meta = file
        .metadata()
        .with_context(|| format!("failed to stat file for SHA-256: {}", path.display()))?;
    if !meta.is_file() {
        bail!("path is not a regular file: {}", path.display());
    }
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader
            .read(&mut buf)
            .with_context(|| format!("failed reading file for SHA-256: {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format_sha256_digest(&hasher.finalize()))
}

/// Returns `true` when `s` is exactly 64 ASCII hexadecimal characters.
pub fn is_valid_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Verifies `actual` and `expected` are valid 64-hex SHA-256 strings and match (case-insensitively).
pub fn verify_sha256_hex(actual: &str, expected: &str, label: &str) -> Result<()> {
    if !is_valid_sha256_hex(expected) {
        bail!("invalid expected SHA-256 for {label}: {expected:?}");
    }
    if !is_valid_sha256_hex(actual) {
        bail!("invalid computed SHA-256 for {label}: {actual:?}");
    }
    if !actual.eq_ignore_ascii_case(expected) {
        bail!(
            "SHA-256 mismatch for {label}: expected {}, got {}",
            expected.to_ascii_lowercase(),
            actual.to_ascii_lowercase()
        );
    }
    Ok(())
}

pub(crate) fn parse_semver_version(raw: &str) -> Result<Version> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || has_ascii_control_chars(trimmed) {
        bail!("invalid version: {raw:?}");
    }
    let normalized = trimmed.strip_prefix('v').unwrap_or(trimmed);
    Version::parse(normalized).with_context(|| format!("invalid version: {raw}"))
}

/// Strips at most one leading `v`, validates SemVer, and returns canonical string form.
pub fn normalize_version(raw: &str) -> Result<String> {
    Ok(parse_semver_version(raw)?.to_string())
}

/// Returns `true` when `candidate` is a stable (non-prerelease) SemVer strictly greater in
/// precedence than `current` (accepting at most one leading `v` and ignoring `+build` metadata).
pub fn is_newer_version(current: &str, candidate: &str) -> Result<bool> {
    let cur = parse_semver_version(current)
        .with_context(|| format!("invalid current version: {current}"))?;
    let cand = parse_semver_version(candidate)
        .with_context(|| format!("invalid candidate version: {candidate}"))?;
    if !cand.pre.is_empty() {
        return Ok(false);
    }
    Ok(cand.cmp_precedence(&cur) == std::cmp::Ordering::Greater)
}

/// Validates that an update download URL uses HTTPS on an explicitly allowlisted GitHub
/// Releases/CDN host with standard port and no embedded credentials.
/// Plaintext loopback HTTP (`127.0.0.1` / `localhost`) is permitted ONLY when `allow_local_http`
/// is `true` (tests or explicit sanitized `MULTIBOT_UPDATE_URL` override).
pub fn is_allowed_https_url_with_local(url: &str, allow_local_http: bool) -> bool {
    if url.is_empty() || url.trim() != url || has_ascii_control_chars(url) {
        return false;
    }
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return false;
    }
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host_lower = host.to_ascii_lowercase();
    if parsed.scheme() == "http" {
        return allow_local_http && matches!(host_lower.as_str(), "127.0.0.1" | "localhost");
    }
    if parsed.scheme() != "https" {
        return false;
    }
    if !(parsed.port().is_none() || parsed.port() == Some(443)) {
        return false;
    }
    matches!(
        host_lower.as_str(),
        "github.com"
            | "api.github.com"
            | "objects.githubusercontent.com"
            | "release-assets.githubusercontent.com"
            | "github-releases.githubusercontent.com"
    )
}

/// Sanitizes an optional `MULTIBOT_UPDATE_URL` override value.
///
/// - `None` or empty/whitespace-only string returns `Ok(None)` (treated strictly as unset).
/// - Any ASCII control character (`\r`, `\n`, `\t`, `\0`, `0x7f`) is rejected.
/// - Must pass `is_allowed_https_url_with_local(trimmed, true)`.
pub fn sanitize_update_url_override(raw: Option<&str>) -> Result<Option<String>> {
    let Some(s) = raw else {
        return Ok(None);
    };
    if s.is_empty() {
        return Ok(None);
    }
    if has_ascii_control_chars(s) {
        bail!("{ENV_UPDATE_URL} contains ASCII control characters");
    }
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if !is_allowed_https_url_with_local(trimmed, true) {
        bail!("{ENV_UPDATE_URL} is not an allowed HTTPS or loopback URL: {trimmed:?}");
    }
    Ok(Some(trimmed.to_string()))
}

/// Reads and sanitizes `MULTIBOT_UPDATE_URL` from the process environment.
pub fn update_url_override_from_env() -> Result<Option<String>> {
    match std::env::var(ENV_UPDATE_URL) {
        Ok(val) => sanitize_update_url_override(Some(&val)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            bail!("{ENV_UPDATE_URL} is not valid UTF-8")
        }
    }
}

/// Returns `true` ONLY when a sanitized `MULTIBOT_UPDATE_URL` override itself uses the `http` scheme.
pub(crate) fn override_allows_local_http(override_url: Option<&str>) -> bool {
    override_url
        .and_then(|u| Url::parse(u).ok())
        .is_some_and(|u| u.scheme() == "http")
}

/// Validates that an update download URL uses HTTPS on an allowed GitHub Releases/CDN host
/// (allowing loopback HTTP only when `MULTIBOT_UPDATE_URL` is an `http://` loopback override,
/// or under `#[cfg(test)]` when `MULTIBOT_UPDATE_URL` is unset).
pub fn is_allowed_https_url(url: &str) -> bool {
    let local_allowed = match update_url_override_from_env().ok().flatten() {
        Some(override_url) => override_allows_local_http(Some(&override_url)),
        None => cfg!(test),
    };
    is_allowed_https_url_with_local(url, local_allowed)
}

/// Returns `true` if a relative archive path is strictly safe across Linux and Windows:
/// valid non-empty UTF-8, no NUL/backslash/colon/leading-slash/drive-prefix/parent traversal,
/// and at least one genuine normal non-`.` segment.
pub fn is_safe_zip_entry_path(path: &Path) -> bool {
    let Some(raw) = path.to_str() else {
        return false;
    };
    if raw.is_empty()
        || raw.starts_with('/')
        || raw.contains('\0')
        || raw.contains('\\')
        || raw.contains(':')
        || has_ascii_control_chars(raw)
    {
        return false;
    }

    let mut has_normal = false;
    for segment in raw.split('/') {
        if segment == ".." {
            return false;
        }
        if !segment.is_empty() && segment != "." {
            has_normal = true;
        }
    }
    if !has_normal {
        return false;
    }

    let mut has_normal_comp = false;
    for comp in path.components() {
        match comp {
            Component::Normal(seg) => {
                if !seg.is_empty() {
                    has_normal_comp = true;
                }
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    has_normal_comp
}

/// Inspects zip entry names safely, rejecting symlinks, zip-slip traversal, and unsafe paths.
pub fn inspect_zip_entries(archive_bytes: &[u8]) -> Result<Vec<String>> {
    let reader = Cursor::new(archive_bytes);
    let mut archive = zip::ZipArchive::new(reader).context("failed to open zip archive")?;
    let mut names = Vec::with_capacity(archive.len());
    for i in 0..archive.len() {
        let entry = archive.by_index(i)?;
        let raw_name = entry.name();
        if entry.is_symlink() {
            bail!("symlink entry rejected in zip archive: {raw_name}");
        }
        if !is_safe_zip_entry_path(Path::new(raw_name)) {
            bail!("unsafe zip entry path: {raw_name}");
        }
        let Some(enclosed) = entry.enclosed_name() else {
            bail!("unsafe zip entry path: {raw_name}");
        };
        if !is_safe_zip_entry_path(&enclosed) {
            bail!("unsafe zip entry path: {raw_name}");
        }
        names.push(raw_name.to_string());
    }
    Ok(names)
}

/// Decompresses gzipped bytes up to `max_bytes` (pure-Rust `flate2` backend).
pub fn decompress_gzip_limited(input: &[u8], max_bytes: usize) -> Result<Vec<u8>> {
    let mut decoder = GzDecoder::new(input);
    let mut out = Vec::new();
    let mut limited = (&mut decoder).take((max_bytes as u64).saturating_add(1));
    limited.read_to_end(&mut out)?;
    if out.len() > max_bytes {
        bail!("gzip payload exceeded limit of {max_bytes} bytes");
    }
    Ok(out)
}
