use crate::notes::{filter_plain_english_notes, parse_release_body_notes};
use crate::rejected::is_version_rejected;
use crate::security::{
    MAX_METADATA_BYTES, MAX_REDIRECT_HOPS, MAX_UPDATE_ARCHIVE_BYTES, has_ascii_control_chars,
    is_allowed_https_url_with_local, is_newer_version, is_valid_sha256_hex, normalize_version,
    override_allows_local_http, sanitize_update_url_override, update_url_override_from_env,
};
use crate::{MANIFEST_SCHEMA_VERSION, ReleaseInfo, ReleaseManifest, WindowsAssetInfo};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;

pub const DEFAULT_RELEASES_LATEST_URL: &str =
    "https://api.github.com/repos/valentinyeo/multibot/releases/latest";
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);
pub const MIN_RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(15 * 60);
pub const MAX_RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(24 * 60 * 60);
pub const MANIFEST_ASSET_NAME: &str = "update-manifest.json";
pub const SHA256SUMS_ASSET_NAME: &str = "SHA256SUMS.txt";
pub const WINDOWS_ZIP_ASSET_NAME: &str = "multibot-windows-x64.zip";

/// Constructs a lean pure-Rust `ureq` HTTP agent for metadata/API requests with
/// manual redirect enforcement (`max_redirects(0)`) and preserved error headers (`http_status_as_error(false)`).
pub fn default_http_agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_global(Some(Duration::from_secs(30)))
        .build();
    ureq::Agent::new_with_config(config)
}

/// Constructs a `ureq` HTTP agent tuned for large streaming archive downloads,
/// with manual redirect enforcement and preserved response headers.
pub fn download_http_agent(timeout: Duration) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_body(Some(Duration::from_secs(60)))
        .timeout_global(Some(timeout))
        .build();
    ureq::Agent::new_with_config(config)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateClientConfig {
    pub endpoint_url: String,
    pub current_version: String,
    pub allow_local_http: bool,
    pub download_timeout: Duration,
}

impl UpdateClientConfig {
    /// Builds config from an explicit optional `MULTIBOT_UPDATE_URL` override string.
    /// Plaintext loopback HTTP is enabled ONLY when the sanitized override itself uses `http://`.
    pub fn from_override(
        raw_override: Option<&str>,
        current_version: impl Into<String>,
    ) -> Result<Self> {
        let current_version = normalize_version(&current_version.into())?;
        let (endpoint_url, allow_local_http) = match sanitize_update_url_override(raw_override)? {
            Some(override_url) => {
                let is_http = override_allows_local_http(Some(&override_url));
                (override_url, is_http)
            }
            None => (DEFAULT_RELEASES_LATEST_URL.to_string(), false),
        };
        Ok(Self {
            endpoint_url,
            current_version,
            allow_local_http,
            download_timeout: Duration::from_secs(5 * 60),
        })
    }

    /// Builds config from process env (`MULTIBOT_UPDATE_URL` if set & valid, otherwise GitHub Releases API).
    /// Plaintext loopback HTTP is enabled ONLY when the sanitized override itself uses `http://`.
    pub fn from_env(current_version: impl Into<String>) -> Result<Self> {
        let current_version = normalize_version(&current_version.into())?;
        let (endpoint_url, allow_local_http) = match update_url_override_from_env()? {
            Some(override_url) => {
                let is_http = override_allows_local_http(Some(&override_url));
                (override_url, is_http)
            }
            None => (DEFAULT_RELEASES_LATEST_URL.to_string(), false),
        };
        Ok(Self {
            endpoint_url,
            current_version,
            allow_local_http,
            download_timeout: Duration::from_secs(5 * 60),
        })
    }

    /// Builds config targeting an explicit validated endpoint (ideal for deterministic tests).
    pub fn for_endpoint(
        endpoint_url: impl Into<String>,
        current_version: impl Into<String>,
        allow_local_http: bool,
    ) -> Result<Self> {
        let endpoint_url = endpoint_url.into();
        if !is_allowed_https_url_with_local(&endpoint_url, allow_local_http) {
            bail!("disallowed update endpoint URL: {endpoint_url:?}");
        }
        let current_version = normalize_version(&current_version.into())?;
        Ok(Self {
            endpoint_url,
            current_version,
            allow_local_http,
            download_timeout: Duration::from_secs(5 * 60),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    /// HTTP 304 Not Modified — zero rate-limit burn and zero UI redraw needed.
    NotModified,
    /// Latest release is draft/prerelease, not strictly newer SemVer, or listed in `rejected.txt`.
    UpToDate { etag: Option<String> },
    /// GitHub API returned 403/429 — caller should back off for at least `retry_after`.
    RateLimited {
        retry_after: Duration,
        etag: Option<String>,
    },
    /// Validated newer stable release ready for user prompt / download.
    UpdateAvailable {
        release: ReleaseInfo,
        etag: Option<String>,
    },
}

/// Parses a `ReleaseManifest` from JSON text.
pub fn parse_release_manifest(json: &str) -> Result<ReleaseManifest> {
    serde_json::from_str(json).context("failed to parse update-manifest.json")
}

/// Parses `SHA256SUMS.txt` lines (`<64-hex>  <filename>`, `<64-hex> <filename>`, or `<64-hex> *<filename>`).
pub fn parse_sha256sums(text: &str) -> Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    for (idx, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut split = line.splitn(2, |c: char| c == ' ' || c == '\t');
        let Some(hash) = split.next() else {
            bail!("malformed SHA256SUMS.txt line {}", idx + 1);
        };
        let Some(rest) = split.next() else {
            bail!("malformed SHA256SUMS.txt line {}", idx + 1);
        };
        if !is_valid_sha256_hex(hash) {
            bail!("invalid SHA-256 hex on line {}: {hash:?}", idx + 1);
        }
        let mut filename = rest.trim_start_matches([' ', '\t']);
        if let Some(stripped) = filename.strip_prefix('*') {
            filename = stripped;
        }
        if let Some(stripped) = filename.strip_prefix("./") {
            filename = stripped;
        }
        if filename.is_empty() || has_ascii_control_chars(filename) {
            bail!("invalid filename on SHA256SUMS.txt line {}", idx + 1);
        }
        if map.contains_key(filename) {
            bail!("duplicate entry for {filename:?} in SHA256SUMS.txt");
        }
        map.insert(filename.to_string(), hash.to_ascii_lowercase());
    }
    Ok(map)
}

fn parse_ascii_digits<const N: usize>(bytes: &[u8]) -> Option<u32> {
    if bytes.len() != N || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut val = 0u32;
    for &b in bytes {
        val = val * 10 + u32::from(b - b'0');
    }
    Some(val)
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = (y - era * 400) as u64;
    let m = u64::from(month);
    let d = u64::from(day);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + (doe as i64) - 719_468
}

/// Parses an RFC 9110 §5.6.7 IMF-fixdate (e.g. `"Wed, 21 Oct 2026 09:28:00 GMT"`) into Unix seconds.
fn parse_imf_fixdate_unix_secs(raw: &str) -> Option<u64> {
    let b = raw.trim().as_bytes();
    if b.len() != 29 {
        return None;
    }
    if !matches!(
        &b[0..3],
        b"Mon" | b"Tue" | b"Wed" | b"Thu" | b"Fri" | b"Sat" | b"Sun"
    ) {
        return None;
    }
    if &b[3..5] != b", "
        || b[7] != b' '
        || b[11] != b' '
        || b[16] != b' '
        || b[19] != b':'
        || b[22] != b':'
        || &b[25..29] != b" GMT"
    {
        return None;
    }
    let day = parse_ascii_digits::<2>(&b[5..7])?;
    let month = match &b[8..11] {
        b"Jan" => 1,
        b"Feb" => 2,
        b"Mar" => 3,
        b"Apr" => 4,
        b"May" => 5,
        b"Jun" => 6,
        b"Jul" => 7,
        b"Aug" => 8,
        b"Sep" => 9,
        b"Oct" => 10,
        b"Nov" => 11,
        b"Dec" => 12,
        _ => return None,
    };
    let year = i64::from(parse_ascii_digits::<4>(&b[12..16])?);
    if year < 1970 {
        return None;
    }
    if day == 0 || day > days_in_month(year, month) {
        return None;
    }
    let hour = parse_ascii_digits::<2>(&b[17..19])?;
    let minute = parse_ascii_digits::<2>(&b[20..22])?;
    let second = parse_ascii_digits::<2>(&b[23..25])?;
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    if days < 0 {
        return None;
    }
    (days as u64)
        .checked_mul(86_400)?
        .checked_add(u64::from(hour) * 3_600)?
        .checked_add(u64::from(minute) * 60)?
        .checked_add(u64::from(second))
}

/// Computes rate-limit backoff from `Retry-After` (delta-seconds or RFC 9110 IMF-fixdate)
/// and `X-RateLimit-Reset` headers, clamped to `[MIN_RATE_LIMIT_BACKOFF (15m), MAX_RATE_LIMIT_BACKOFF (24h)]`.
pub fn parse_rate_limit_backoff(
    retry_after: Option<&str>,
    ratelimit_reset: Option<&str>,
    now_unix_secs: u64,
) -> Duration {
    let from_retry = retry_after
        .map(str::trim)
        .and_then(|v| {
            v.parse::<u64>().ok().or_else(|| {
                parse_imf_fixdate_unix_secs(v).map(|target| target.saturating_sub(now_unix_secs))
            })
        })
        .unwrap_or(0);
    let from_reset = ratelimit_reset
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|reset| reset.saturating_sub(now_unix_secs))
        .unwrap_or(0);

    let secs = from_retry
        .max(from_reset)
        .max(MIN_RATE_LIMIT_BACKOFF.as_secs())
        .min(MAX_RATE_LIMIT_BACKOFF.as_secs());
    Duration::from_secs(secs)
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn header_str<'a>(response: &'a ureq::http::Response<ureq::Body>, name: &str) -> Option<&'a str> {
    response.headers().get(name).and_then(|v| v.to_str().ok())
}

/// Performs an HTTP GET manually following up to `MAX_REDIRECT_HOPS` redirects while
/// re-validating EVERY target URL against the HTTPS/loopback allowlist before connecting.
/// Sensitive/API headers (`initial_headers`) are stripped on cross-origin redirects.
pub(crate) fn get_following_allowed_redirects(
    agent: &ureq::Agent,
    initial_url: &str,
    user_agent: &str,
    initial_headers: &[(&str, &str)],
    allow_local_http: bool,
) -> Result<(String, ureq::http::Response<ureq::Body>)> {
    let mut current_url = initial_url.to_string();
    if has_ascii_control_chars(&current_url) {
        bail!("blocked URL containing control characters");
    }
    let initial_origin = Url::parse(&current_url).ok().map(|u| u.origin());

    for hop in 0..=MAX_REDIRECT_HOPS {
        if has_ascii_control_chars(&current_url)
            || !is_allowed_https_url_with_local(&current_url, allow_local_http)
        {
            bail!("blocked disallowed update URL: {current_url}");
        }

        let current_origin = Url::parse(&current_url).ok().map(|u| u.origin());
        let same_origin =
            hop == 0 || (initial_origin.is_some() && current_origin == initial_origin);

        let mut req = agent.get(&current_url).header("User-Agent", user_agent);
        if same_origin {
            for (k, v) in initial_headers {
                req = req.header(*k, *v);
            }
        }

        let response = req
            .call()
            .with_context(|| format!("HTTP GET failed for {current_url}"))?;
        let status = response.status().as_u16();
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            if hop == MAX_REDIRECT_HOPS {
                bail!("too many redirects (exceeded {MAX_REDIRECT_HOPS} hops)");
            }
            let Some(loc) = header_str(&response, "location") else {
                bail!("redirect HTTP {status} missing valid Location header");
            };
            if loc.trim().is_empty() || has_ascii_control_chars(loc) {
                bail!("redirect HTTP {status} contained invalid Location header");
            }
            let base = Url::parse(&current_url)
                .with_context(|| format!("failed to parse base URL {current_url}"))?;
            let next = base
                .join(loc.trim())
                .with_context(|| format!("failed to resolve redirect Location {loc:?}"))?;
            let next_str = next.as_str();
            if !is_allowed_https_url_with_local(next_str, allow_local_http) {
                bail!("blocked redirect to disallowed URL: {next_str}");
            }
            current_url = next_str.to_string();
            continue;
        }
        return Ok((current_url, response));
    }

    bail!("too many redirects (exceeded {MAX_REDIRECT_HOPS} hops)")
}

#[derive(Debug, Deserialize)]
struct GitHubReleaseResponse {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    published_at: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    assets: Vec<GitHubReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubReleaseAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

#[derive(Debug, Clone)]
pub struct UpdateClient {
    pub(crate) config: UpdateClientConfig,
    pub(crate) metadata_agent: ureq::Agent,
    pub(crate) download_agent: ureq::Agent,
}

impl UpdateClient {
    pub fn new(config: UpdateClientConfig) -> Self {
        let metadata_agent = default_http_agent();
        let download_agent = download_http_agent(config.download_timeout);
        Self {
            config,
            metadata_agent,
            download_agent,
        }
    }

    pub fn config(&self) -> &UpdateClientConfig {
        &self.config
    }

    pub fn user_agent(&self) -> String {
        format!("Multibot/{}", self.config.current_version)
    }

    fn fetch_metadata_text(&self, url: &str) -> Result<String> {
        let (_final_url, mut response) = get_following_allowed_redirects(
            &self.metadata_agent,
            url,
            &self.user_agent(),
            &[],
            self.config.allow_local_http,
        )?;
        let status = response.status().as_u16();
        if status != 200 {
            bail!("metadata asset request to {url} returned HTTP {status}");
        }
        response
            .body_mut()
            .with_config()
            .limit(MAX_METADATA_BYTES)
            .read_to_string()
            .with_context(|| format!("failed reading metadata asset from {url}"))
    }

    /// Queries the configured GitHub Releases endpoint with optional `If-None-Match` ETag,
    /// applies draft/prerelease/SemVer/`rejected.txt` filters, and validates release metadata.
    pub fn check_for_update(&self, etag: Option<&str>, updates_dir: &Path) -> Result<CheckOutcome> {
        let mut headers: Vec<(&str, &str)> = vec![("Accept", "application/vnd.github+json")];
        if let Some(raw_etag) = etag {
            let trimmed = raw_etag.trim();
            if !trimmed.is_empty() && !has_ascii_control_chars(trimmed) {
                headers.push(("If-None-Match", trimmed));
            }
        }

        let (_final_url, mut response) = get_following_allowed_redirects(
            &self.metadata_agent,
            &self.config.endpoint_url,
            &self.user_agent(),
            &headers,
            self.config.allow_local_http,
        )?;

        let response_etag = header_str(&response, "etag").map(str::to_string);
        let status = response.status().as_u16();

        match status {
            304 => return Ok(CheckOutcome::NotModified),
            403 | 429 => {
                let retry_after = parse_rate_limit_backoff(
                    header_str(&response, "retry-after"),
                    header_str(&response, "x-ratelimit-reset"),
                    unix_now_secs(),
                );
                return Ok(CheckOutcome::RateLimited {
                    retry_after,
                    etag: response_etag,
                });
            }
            200 => {}
            other => bail!("GitHub Releases API returned HTTP {other}"),
        }

        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_METADATA_BYTES)
            .read_to_string()
            .context("failed reading GitHub Releases API response body")?;

        let gh: GitHubReleaseResponse =
            serde_json::from_str(&body).context("failed parsing GitHub Releases API JSON")?;

        if gh.draft || gh.prerelease {
            return Ok(CheckOutcome::UpToDate {
                etag: response_etag,
            });
        }

        if !is_newer_version(&self.config.current_version, &gh.tag_name)? {
            return Ok(CheckOutcome::UpToDate {
                etag: response_etag,
            });
        }

        if is_version_rejected(updates_dir, &gh.tag_name)? {
            return Ok(CheckOutcome::UpToDate {
                etag: response_etag,
            });
        }

        let expected_ver = normalize_version(&gh.tag_name)?;

        // Primary path: update-manifest.json (fail-closed if present and invalid/tampered).
        if let Some(manifest_asset) = gh.assets.iter().find(|a| a.name == MANIFEST_ASSET_NAME) {
            let manifest_json = self.fetch_metadata_text(&manifest_asset.browser_download_url)?;
            let manifest = parse_release_manifest(&manifest_json)?;
            self.validate_manifest(&manifest, &expected_ver)?;

            let mut notes = filter_plain_english_notes(&manifest.notes);
            if notes.is_empty()
                && let Some(body_text) = gh.body.as_deref()
            {
                notes = parse_release_body_notes(body_text);
            }

            let published_at = if manifest.published_at.trim().is_empty() {
                gh.published_at
            } else {
                manifest.published_at
            };

            let release = ReleaseInfo {
                version: expected_ver,
                tag: manifest.tag.trim().to_string(),
                published_at,
                notes,
                asset: manifest.windows_x64,
            };
            return Ok(CheckOutcome::UpdateAvailable {
                release,
                etag: response_etag,
            });
        }

        // Fallback path: ONLY when update-manifest.json is absent from release assets.
        let Some(sums_asset) = gh.assets.iter().find(|a| a.name == SHA256SUMS_ASSET_NAME) else {
            bail!(
                "release {} missing both {MANIFEST_ASSET_NAME} and {SHA256SUMS_ASSET_NAME}",
                gh.tag_name
            );
        };
        let Some(zip_asset) = gh.assets.iter().find(|a| a.name == WINDOWS_ZIP_ASSET_NAME) else {
            bail!("release {} missing {WINDOWS_ZIP_ASSET_NAME}", gh.tag_name);
        };

        if !is_allowed_https_url_with_local(
            &zip_asset.browser_download_url,
            self.config.allow_local_http,
        ) {
            bail!(
                "disallowed asset download URL in release {}: {}",
                gh.tag_name,
                zip_asset.browser_download_url
            );
        }
        if zip_asset.size > MAX_UPDATE_ARCHIVE_BYTES {
            bail!(
                "asset {} size {} exceeds limit {}",
                zip_asset.name,
                zip_asset.size,
                MAX_UPDATE_ARCHIVE_BYTES
            );
        }

        let sums_text = self.fetch_metadata_text(&sums_asset.browser_download_url)?;
        let sums = parse_sha256sums(&sums_text)?;

        let zip_sha256 = sums
            .get(WINDOWS_ZIP_ASSET_NAME)
            .cloned()
            .with_context(|| format!("{SHA256SUMS_ASSET_NAME} missing {WINDOWS_ZIP_ASSET_NAME}"))?;
        let exe_sha256 = sums
            .get("multibot.exe")
            .cloned()
            .with_context(|| format!("{SHA256SUMS_ASSET_NAME} missing multibot.exe"))?;
        let daemon_sha256 = sums
            .get("multibotd-linux-x86_64")
            .cloned()
            .with_context(|| format!("{SHA256SUMS_ASSET_NAME} missing multibotd-linux-x86_64"))?;

        let notes = gh
            .body
            .as_deref()
            .map(parse_release_body_notes)
            .unwrap_or_default();

        let release = ReleaseInfo {
            version: expected_ver,
            tag: gh.tag_name,
            published_at: gh.published_at,
            notes,
            asset: WindowsAssetInfo {
                asset_name: WINDOWS_ZIP_ASSET_NAME.to_string(),
                url: zip_asset.browser_download_url.clone(),
                sha256: zip_sha256,
                size_bytes: zip_asset.size,
                multibot_exe_sha256: exe_sha256,
                multibotd_linux_x86_64_sha256: daemon_sha256,
            },
        };

        Ok(CheckOutcome::UpdateAvailable {
            release,
            etag: response_etag,
        })
    }

    fn validate_manifest(&self, manifest: &ReleaseManifest, expected_ver: &str) -> Result<()> {
        if manifest.schema_version != MANIFEST_SCHEMA_VERSION {
            bail!(
                "unsupported update manifest schema_version {} (expected {MANIFEST_SCHEMA_VERSION})",
                manifest.schema_version
            );
        }
        let manifest_ver = normalize_version(&manifest.version)?;
        if manifest_ver != expected_ver {
            bail!(
                "update manifest version mismatch: release tag is {expected_ver}, manifest has {manifest_ver}"
            );
        }
        if manifest.tag.trim().is_empty() || has_ascii_control_chars(&manifest.tag) {
            bail!("update-manifest.json missing required non-empty tag");
        }
        let tag_ver = normalize_version(&manifest.tag)?;
        if tag_ver != expected_ver {
            bail!(
                "update manifest tag mismatch: release tag is {expected_ver}, manifest tag is {}",
                manifest.tag
            );
        }
        if !is_allowed_https_url_with_local(&manifest.windows_x64.url, self.config.allow_local_http)
        {
            bail!(
                "disallowed asset URL in update-manifest.json: {}",
                manifest.windows_x64.url
            );
        }
        if manifest.windows_x64.size_bytes > MAX_UPDATE_ARCHIVE_BYTES {
            bail!(
                "manifest asset size {} exceeds maximum {}",
                manifest.windows_x64.size_bytes,
                MAX_UPDATE_ARCHIVE_BYTES
            );
        }
        if !is_valid_sha256_hex(&manifest.windows_x64.sha256) {
            bail!("invalid archive sha256 in update-manifest.json");
        }
        if !is_valid_sha256_hex(&manifest.windows_x64.multibot_exe_sha256) {
            bail!("invalid multibot_exe_sha256 in update-manifest.json");
        }
        if !is_valid_sha256_hex(&manifest.windows_x64.multibotd_linux_x86_64_sha256) {
            bail!("invalid multibotd_linux_x86_64_sha256 in update-manifest.json");
        }
        Ok(())
    }
}
