use anyhow::Result;
use multibot_gpu::GpuPreference;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DEFAULT_CHAT_FONT_SIZE_PT: u16 = 14;
pub const MIN_CHAT_FONT_SIZE_PT: u16 = 10;
pub const MAX_CHAT_FONT_SIZE_PT: u16 = 28;
pub const DEFAULT_UI_SCALE_PCT: u16 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SidebarMode {
    #[default]
    Open,
    Collapsed,
    Hidden,
}

impl SidebarMode {
    /// Canonical 3-state cycle (`Ctrl+Shift+B`): Open -> Collapsed -> Hidden -> Open.
    pub fn cycle(self) -> Self {
        match self {
            Self::Open => Self::Collapsed,
            Self::Collapsed => Self::Hidden,
            Self::Hidden => Self::Open,
        }
    }
}

pub const MAX_IDENTITY_FILE_LEN: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshHostConfig {
    pub name: String,
    pub hostname: String,
    pub user: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<String>,
}

fn default_ssh_port() -> u16 {
    22
}

fn default_chat_font_size_pt() -> u16 {
    DEFAULT_CHAT_FONT_SIZE_PT
}

fn default_ui_scale_pct() -> u16 {
    DEFAULT_UI_SCALE_PCT
}

fn default_true() -> bool {
    true
}

/// Validates an SSH `IdentityFile` path string and returns the trimmed slice.
/// Rejects ASCII/Unicode control chars (checked BEFORE trimming), empty/whitespace-only strings,
/// paths exceeding `MAX_IDENTITY_FILE_LEN`, and leading `-` (prevents OpenSSH option injection).
/// Spaces inside paths (e.g. `C:/Users/Example User/.../id_ed25519`) are permitted because
/// callers pass the value as a single `Command` argv element, never via shell interpolation.
pub fn validate_identity_file_str(raw: &str) -> Result<&str> {
    if raw.chars().any(char::is_control) {
        anyhow::bail!("SSH identity_file contains control characters: {raw:?}");
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        anyhow::bail!("SSH identity_file cannot be empty");
    }
    if trimmed.len() > MAX_IDENTITY_FILE_LEN {
        anyhow::bail!("SSH identity_file exceeds {MAX_IDENTITY_FILE_LEN} bytes");
    }
    if trimmed.starts_with('-') {
        anyhow::bail!("SSH identity_file must not start with '-': {trimmed:?}");
    }
    Ok(trimmed)
}

/// Convenience wrapper returning `Some(trimmed)` if `raw` is a safe SSH `IdentityFile` path.
pub fn sanitize_identity_file(raw: &str) -> Option<String> {
    validate_identity_file_str(raw).ok().map(str::to_string)
}

impl SshHostConfig {
    pub fn ssh_target(&self) -> String {
        if self.user.trim().is_empty() {
            self.hostname.trim().to_string()
        } else {
            format!("{}@{}", self.user.trim(), self.hostname.trim())
        }
    }

    /// Returns `Ok(Some(trimmed))` when `identity_file` is present and valid,
    /// `Ok(None)` when `identity_file` is `None`, and `Err` if `Some` fails validation.
    pub fn validated_identity_file(&self) -> Result<Option<&str>> {
        match self.identity_file.as_deref() {
            None => Ok(None),
            Some(raw) => validate_identity_file_str(raw).map(Some),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MultibotSettings {
    #[serde(default)]
    pub voice: crate::voice::VoiceSettings,
    #[serde(default)]
    pub gpu: GpuPreference,
    #[serde(default)]
    pub ssh_hosts: Vec<SshHostConfig>,
    #[serde(default)]
    pub selected_host: Option<String>,
    #[serde(default)]
    pub sidebar_mode: SidebarMode,
    #[serde(default)]
    pub shell_fullscreen: bool,
    #[serde(default = "default_chat_font_size_pt")]
    pub chat_font_size_pt: u16,
    #[serde(default)]
    pub composer_ctrl_enter: bool,
    #[serde(default = "default_true")]
    pub queue_while_working: bool,
    #[serde(default = "default_ui_scale_pct")]
    pub ui_scale_pct: u16,
    #[serde(default)]
    pub dismissed_update_version: Option<String>,
    #[serde(default)]
    pub last_update_etag: Option<String>,
    #[serde(default = "default_true")]
    pub auto_check_updates: bool,
}

impl Default for MultibotSettings {
    fn default() -> Self {
        Self {
            voice: crate::voice::VoiceSettings::default(),
            gpu: GpuPreference::default(),
            ssh_hosts: Vec::new(),
            selected_host: None,
            sidebar_mode: SidebarMode::Open,
            shell_fullscreen: false,
            chat_font_size_pt: DEFAULT_CHAT_FONT_SIZE_PT,
            composer_ctrl_enter: false,
            queue_while_working: true,
            ui_scale_pct: DEFAULT_UI_SCALE_PCT,
            dismissed_update_version: None,
            last_update_etag: None,
            auto_check_updates: true,
        }
    }
}

impl MultibotSettings {
    /// Returns `%APPDATA%\Multibot` on Windows or `~/.config/Multibot` on Linux/macOS.
    /// Completely isolated from legacy `%APPDATA%\ZigShell`.
    pub fn data_dir() -> PathBuf {
        // Windows known-folder APIs can ignore process-local profile overrides.
        #[cfg(target_os = "windows")]
        if let Some(path) = std::env::var_os("MULTIBOT_DATA_DIR").map(PathBuf::from)
            && path.is_absolute()
        {
            return path;
        }
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Multibot")
    }

    pub fn settings_path() -> PathBuf {
        Self::data_dir().join("settings.json")
    }

    pub fn gpu_report_path() -> PathBuf {
        Self::data_dir().join("last-gpu.txt")
    }

    pub fn workspaces_path() -> PathBuf {
        Self::data_dir().join("workspaces.json")
    }

    pub fn palette_stats_path() -> PathBuf {
        Self::data_dir().join("palette-stats.json")
    }

    pub fn outbox_path() -> PathBuf {
        Self::data_dir().join("chat-outbox.json")
    }

    pub fn chat_cache_dir() -> PathBuf {
        Self::data_dir().join("chatcache")
    }

    pub fn image_cache_dir() -> PathBuf {
        Self::data_dir().join("imgcache")
    }

    pub fn updates_dir() -> PathBuf {
        Self::data_dir().join("updates")
    }

    pub fn load() -> Self {
        Self::load_from(&Self::settings_path()).unwrap_or_default()
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&text)?)
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::settings_path())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    pub fn merge_hosts(&mut self, incoming: Vec<SshHostConfig>) -> usize {
        let mut added = 0;
        for host in incoming {
            if let Some(existing) = self.ssh_hosts.iter_mut().find(|existing| {
                existing.hostname.eq_ignore_ascii_case(&host.hostname)
                    && existing.user.eq_ignore_ascii_case(&host.user)
                    && existing.port == host.port
            }) {
                if existing.identity_file.is_none() && host.identity_file.is_some() {
                    existing.identity_file = host.identity_file;
                }
            } else {
                self.ssh_hosts.push(host);
                added += 1;
            }
        }
        added
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_data_dir_honors_override() {
        let path = std::env::temp_dir().join("multibot-isolated-profile-test");
        if std::env::var_os("MULTIBOT_DATA_DIR_TEST_CHILD").is_some() {
            assert_eq!(MultibotSettings::data_dir(), path);
            return;
        }
        // A subprocess avoids changing the environment of parallel tests.
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "settings::tests::windows_data_dir_honors_override",
            ])
            .env("MULTIBOT_DATA_DIR", path)
            .env("MULTIBOT_DATA_DIR_TEST_CHILD", "1")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn settings_roundtrip_and_default_is_auto_integrated() {
        let dir = std::env::temp_dir().join(format!("multibot-settings-test-{}", std::process::id()));
        let path = dir.join("settings.json");

        let mut settings = MultibotSettings::default();
        assert_eq!(settings.gpu, GpuPreference::AutoIntegrated);
        settings.ssh_hosts.push(SshHostConfig {
            name: "contabo".into(),
            hostname: "example.com".into(),
            user: "ubuntu".into(),
            port: 22,
            identity_file: None,
        });
        settings.sidebar_mode = SidebarMode::Collapsed;
        settings.chat_font_size_pt = 16;
        settings.dismissed_update_version = Some("0.1.9".into());
        settings.save_to(&path).unwrap();

        let loaded = MultibotSettings::load_from(&path).unwrap();
        assert_eq!(loaded, settings);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shell_fullscreen_defaults_off_and_roundtrips_both_choices() {
        let dir =
            std::env::temp_dir().join(format!("multibot-shell-layout-{}", std::process::id()));
        let path = dir.join("settings.json");
        let mut settings: MultibotSettings = serde_json::from_str("{}").unwrap();
        assert!(!settings.shell_fullscreen);
        for fullscreen in [true, false] {
            settings.shell_fullscreen = fullscreen;
            settings.save_to(&path).unwrap();
            assert_eq!(
                MultibotSettings::load_from(&path).unwrap().shell_fullscreen,
                fullscreen
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn settings_defaults_match_empty_json() {
        let from_empty: MultibotSettings = serde_json::from_str("{}").unwrap();
        let def = MultibotSettings::default();
        assert_eq!(from_empty, def);
        assert_eq!(def.sidebar_mode, SidebarMode::Open);
        assert_eq!(def.chat_font_size_pt, DEFAULT_CHAT_FONT_SIZE_PT);
        assert_eq!(def.ui_scale_pct, DEFAULT_UI_SCALE_PCT);
        assert_eq!(def.dismissed_update_version, None);
        assert_eq!(def.last_update_etag, None);
        assert!(def.auto_check_updates);
    }

    #[test]
    fn slice1_settings_json_backward_compat() {
        let legacy = r#"{"gpu":{"mode":"auto_integrated"},"ssh_hosts":[{"name":"vps","hostname":"10.0.0.1","user":"ubuntu","port":22}],"selected_host":"vps"}"#;
        let parsed: MultibotSettings = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.gpu, GpuPreference::AutoIntegrated);
        assert_eq!(parsed.ssh_hosts.len(), 1);
        assert_eq!(parsed.ssh_hosts[0].identity_file, None);
        assert_eq!(parsed.selected_host.as_deref(), Some("vps"));
        assert_eq!(parsed.sidebar_mode, SidebarMode::Open);
        assert_eq!(parsed.chat_font_size_pt, 14);
        assert_eq!(parsed.ui_scale_pct, 100);
        assert_eq!(parsed.dismissed_update_version, None);
        assert_eq!(parsed.last_update_etag, None);
        assert!(parsed.auto_check_updates);
    }

    #[test]
    fn sidebar_mode_cycle() {
        assert_eq!(SidebarMode::Open.cycle(), SidebarMode::Collapsed);
        assert_eq!(SidebarMode::Collapsed.cycle(), SidebarMode::Hidden);
        assert_eq!(SidebarMode::Hidden.cycle(), SidebarMode::Open);
    }

    #[test]
    fn ssh_target_trims_hostname_and_user() {
        let with_user = SshHostConfig {
            name: "vps".into(),
            hostname: "  example.com  ".into(),
            user: "  ubuntu  ".into(),
            port: 22,
            identity_file: None,
        };
        assert_eq!(with_user.ssh_target(), "ubuntu@example.com");

        let blank_user = SshHostConfig {
            name: "vps".into(),
            hostname: "  example.com  ".into(),
            user: "   ".into(),
            port: 22,
            identity_file: None,
        };
        assert_eq!(blank_user.ssh_target(), "example.com");
    }

    #[test]
    fn ssh_host_identity_file_serde_compat_and_skip_none() {
        let legacy_json = r#"{"name":"vps","hostname":"10.0.0.1","user":"ubuntu","port":22}"#;
        let parsed: SshHostConfig = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(parsed.identity_file, None);

        let serialized_none = serde_json::to_string(&parsed).unwrap();
        assert!(
            !serialized_none.contains("identity_file"),
            "identity_file=None should be omitted from JSON: {serialized_none}"
        );

        let with_key = SshHostConfig {
            identity_file: Some(
                r"C:\Users\Valentin Yeo\AppData\Roaming\ZigShell\ssh-key\id_ed25519".into(),
            ),
            ..parsed
        };
        let serialized_some = serde_json::to_string(&with_key).unwrap();
        let roundtripped: SshHostConfig = serde_json::from_str(&serialized_some).unwrap();
        assert_eq!(roundtripped, with_key);
    }

    #[test]
    fn validate_identity_file_accepts_valid_and_rejects_injection() {
        for ok in [
            r"C:\Users\Valentin Yeo\AppData\Roaming\ZigShell\ssh-key\id_ed25519",
            "C:/Users/Example User/ZigShell/ssh-key/multibot_ed25519",
            "~/.ssh/id_ed25519",
            "/home/u/.ssh/id_ed25519",
            r"\\server\share\id_ed25519",
            "  /home/u/.ssh/id_ed25519  ",
        ] {
            assert!(
                validate_identity_file_str(ok).is_ok(),
                "expected valid identity_file {ok:?}"
            );
            assert!(sanitize_identity_file(ok).is_some());
        }

        let overlong = "a".repeat(MAX_IDENTITY_FILE_LEN + 1);
        for bad in [
            "",
            "   ",
            "-oProxyCommand=evil",
            "  -F/tmp/evil",
            "/path\nProxyCommand=id",
            "/path\rbad",
            "/path\0bad",
            "/path\tbad",
            "/path\x7fbad",
            overlong.as_str(),
        ] {
            assert!(
                validate_identity_file_str(bad).is_err(),
                "expected invalid identity_file to be rejected: {bad:?}"
            );
            assert!(sanitize_identity_file(bad).is_none());
        }
    }

    #[test]
    fn merge_hosts_backfills_missing_identity_file_without_duplicating() {
        let mut settings = MultibotSettings::default();
        let added = settings.merge_hosts(vec![SshHostConfig {
            name: "vps".into(),
            hostname: "example.com".into(),
            user: "ubuntu".into(),
            port: 22,
            identity_file: None,
        }]);
        assert_eq!(added, 1);

        // Backfills missing identity_file without duplicating the host.
        let added2 = settings.merge_hosts(vec![SshHostConfig {
            name: "vps-config".into(),
            hostname: "EXAMPLE.COM".into(),
            user: "UBUNTU".into(),
            port: 22,
            identity_file: Some("~/.ssh/zig_ed25519".into()),
        }]);
        assert_eq!(added2, 0);
        assert_eq!(settings.ssh_hosts.len(), 1);
        assert_eq!(
            settings.ssh_hosts[0].identity_file.as_deref(),
            Some("~/.ssh/zig_ed25519")
        );

        // Does NOT overwrite an already-configured identity_file.
        let added3 = settings.merge_hosts(vec![SshHostConfig {
            name: "vps-other".into(),
            hostname: "example.com".into(),
            user: "ubuntu".into(),
            port: 22,
            identity_file: Some("~/.ssh/other".into()),
        }]);
        assert_eq!(added3, 0);
        assert_eq!(
            settings.ssh_hosts[0].identity_file.as_deref(),
            Some("~/.ssh/zig_ed25519")
        );
    }
}
