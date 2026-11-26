use anyhow::Result;
use multibot_gpu::GpuPreference;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshHostConfig {
    pub name: String,
    pub hostname: String,
    pub user: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
}

fn default_ssh_port() -> u16 {
    22
}

impl SshHostConfig {
    pub fn ssh_target(&self) -> String {
        if self.user.trim().is_empty() {
            self.hostname.clone()
        } else {
            format!("{}@{}", self.user.trim(), self.hostname.trim())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MultibotSettings {
    #[serde(default)]
    pub gpu: GpuPreference,
    #[serde(default)]
    pub ssh_hosts: Vec<SshHostConfig>,
    #[serde(default)]
    pub selected_host: Option<String>,
}

impl MultibotSettings {
    /// Returns `%APPDATA%\Multibot` on Windows or `~/.config/multibot` on Linux/macOS.
    /// Completely isolated from legacy `%APPDATA%\ZigShell`.
    pub fn data_dir() -> PathBuf {
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
            let exists = self.ssh_hosts.iter().any(|existing| {
                existing.hostname.eq_ignore_ascii_case(&host.hostname)
                    && existing.user.eq_ignore_ascii_case(&host.user)
                    && existing.port == host.port
            });
            if !exists {
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
        });
        settings.save_to(&path).unwrap();

        let loaded = MultibotSettings::load_from(&path).unwrap();
        assert_eq!(loaded, settings);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
