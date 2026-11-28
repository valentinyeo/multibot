use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub mod client;
pub mod download;
pub mod notes;
pub mod rejected;
pub mod security;
pub mod swapper;

pub use client::{
    CheckOutcome, DEFAULT_POLL_INTERVAL, DEFAULT_RELEASES_LATEST_URL, MANIFEST_ASSET_NAME,
    MAX_RATE_LIMIT_BACKOFF, MIN_RATE_LIMIT_BACKOFF, SHA256SUMS_ASSET_NAME, UpdateClient,
    UpdateClientConfig, WINDOWS_ZIP_ASSET_NAME, default_http_agent, download_http_agent,
    parse_rate_limit_backoff, parse_release_manifest, parse_sha256sums,
};
pub use download::{download_and_stage_update, extract_and_verify_archive};
pub use notes::{
    COLLAPSED_NOTES_LIMIT, MAX_STORED_NOTES, accumulate_release_notes,
    contains_forbidden_duration_wording, filter_plain_english_notes, has_forbidden_control_or_bidi,
    is_user_facing_note, parse_release_body_notes,
};
pub use rejected::{
    REJECTED_FILENAME, is_version_rejected, read_rejected_versions, record_rejected_version,
    rejected_file_path,
};
pub use security::{
    ALLOWED_ZIP_FILES, ENV_UPDATE_URL, MAX_METADATA_BYTES, MAX_REDIRECT_HOPS,
    MAX_UPDATE_ARCHIVE_BYTES, decompress_gzip_limited, inspect_zip_entries, is_allowed_https_url,
    is_allowed_https_url_with_local, is_newer_version, is_safe_zip_entry_path, is_valid_sha256_hex,
    normalize_version, sanitize_update_url_override, sha256_file, sha256_hex,
    update_url_override_from_env, verify_sha256_hex,
};
pub use swapper::{
    ApplyResult, AtomicSwapper, DEFAULT_READINESS_TIMEOUT_MS, DefaultFileSwapper,
    DefaultProcessLauncher, DefaultUpdaterPlatform, UPDATE_READY_ARG, build_relaunch_args,
    cleanup_old_update_artifacts, filter_preserved_cli_args, readiness_marker_path,
    signal_update_ready,
};

pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

fn default_schema_version() -> u32 {
    MANIFEST_SCHEMA_VERSION
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowsAssetInfo {
    pub asset_name: String,
    pub url: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub multibot_exe_sha256: String,
    pub multibotd_linux_x86_64_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseManifest {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub version: String,
    pub tag: String,
    pub published_at: String,
    #[serde(default)]
    pub notes: Vec<String>,
    pub windows_x64: WindowsAssetInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseInfo {
    pub version: String,
    pub tag: String,
    pub published_at: String,
    #[serde(default)]
    pub notes: Vec<String>,
    pub asset: WindowsAssetInfo,
}

impl From<ReleaseManifest> for ReleaseInfo {
    fn from(manifest: ReleaseManifest) -> Self {
        Self {
            version: manifest.version,
            tag: manifest.tag,
            published_at: manifest.published_at,
            notes: manifest.notes,
            asset: manifest.windows_x64,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedUpdate {
    pub version: String,
    pub tag: String,
    pub staging_dir: PathBuf,
    pub multibot_exe_path: PathBuf,
    pub multibotd_linux_path: PathBuf,
    pub multibot_exe_sha256: String,
    pub multibotd_linux_x86_64_sha256: String,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapPlan {
    pub install_dir: PathBuf,
    pub current_exe: PathBuf,
    pub staged_exe: PathBuf,
    pub current_daemon: PathBuf,
    pub staged_daemon: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapOutcome {
    pub backup_exe: PathBuf,
    pub backup_daemon: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum UpdateState {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available(ReleaseInfo),
    Downloading {
        version: String,
        bytes_done: u64,
        bytes_total: Option<u64>,
    },
    Verifying {
        version: String,
    },
    ReadyToRestart(StagedUpdate),
    Applying {
        version: String,
    },
    Error {
        message: String,
        retryable: bool,
    },
}

pub trait FileSwapper: Send + Sync {
    fn swap_binaries(&self, plan: &SwapPlan) -> Result<SwapOutcome>;
    fn rollback(&self, plan: &SwapPlan, outcome: &SwapOutcome) -> Result<()>;
    fn cleanup_stale(&self, install_dir: &Path) -> Result<usize>;
}

pub trait ProcessLauncher: Send + Sync {
    fn spawn_relaunch(&self, exe_path: &Path, args: &[String]) -> Result<u32>;
    fn verify_readiness(&self, exe_path: &Path, timeout_ms: u64) -> Result<bool>;
}

pub trait UpdaterPlatform: Send + Sync {
    fn current_exe(&self) -> Result<PathBuf>;
    fn updates_dir(&self) -> PathBuf;
    fn preserved_cli_args(&self) -> Vec<String>;
    fn file_swapper(&self) -> &dyn FileSwapper;
    fn process_launcher(&self) -> &dyn ProcessLauncher;
}

#[derive(Debug, Default)]
pub struct StubFileSwapper;

impl FileSwapper for StubFileSwapper {
    fn swap_binaries(&self, plan: &SwapPlan) -> Result<SwapOutcome> {
        let backup_exe = plan.current_exe.with_extension("exe.old");
        let backup_daemon = Some(plan.current_daemon.with_extension("old"));
        Ok(SwapOutcome {
            backup_exe,
            backup_daemon,
        })
    }

    fn rollback(&self, _plan: &SwapPlan, _outcome: &SwapOutcome) -> Result<()> {
        Ok(())
    }

    fn cleanup_stale(&self, _install_dir: &Path) -> Result<usize> {
        Ok(0)
    }
}

#[derive(Debug, Default)]
pub struct StubProcessLauncher;

impl ProcessLauncher for StubProcessLauncher {
    fn spawn_relaunch(&self, _exe_path: &Path, _args: &[String]) -> Result<u32> {
        Ok(0)
    }

    fn verify_readiness(&self, _exe_path: &Path, _timeout_ms: u64) -> Result<bool> {
        Ok(true)
    }
}

#[derive(Debug)]
pub struct StubUpdaterPlatform {
    current_exe: PathBuf,
    updates_dir: PathBuf,
    preserved_args: Vec<String>,
    swapper: StubFileSwapper,
    launcher: StubProcessLauncher,
}

impl StubUpdaterPlatform {
    pub fn new(updates_dir: PathBuf, preserved_args: Vec<String>) -> Self {
        Self {
            current_exe: PathBuf::from("multibot.exe"),
            updates_dir,
            preserved_args,
            swapper: StubFileSwapper,
            launcher: StubProcessLauncher,
        }
    }
}

impl UpdaterPlatform for StubUpdaterPlatform {
    fn current_exe(&self) -> Result<PathBuf> {
        Ok(self.current_exe.clone())
    }

    fn updates_dir(&self) -> PathBuf {
        self.updates_dir.clone()
    }

    fn preserved_cli_args(&self) -> Vec<String> {
        self.preserved_args.clone()
    }

    fn file_swapper(&self) -> &dyn FileSwapper {
        &self.swapper
    }

    fn process_launcher(&self) -> &dyn ProcessLauncher {
        &self.launcher
    }
}

#[cfg(test)]
mod tests;
