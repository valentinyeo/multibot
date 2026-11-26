use crate::settings::SshHostConfig;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Returns the legacy `%APPDATA%\ZigShell` directory (read-only source).
pub fn legacy_zigshel_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ZigShell")
}

/// Strictly read-only import of SSH hosts from legacy `%APPDATA%\ZigShell` files.
/// Never modifies or creates any files inside `ZigShell`.
pub fn import_ssh_hosts_readonly(zigshel_dir: Option<&Path>) -> Vec<SshHostConfig> {
    let dir = zigshel_dir
        .map(PathBuf::from)
        .unwrap_or_else(legacy_zigshel_dir);
    if !dir.is_dir() {
        return Vec::new();
    }

    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return found;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(val) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        extract_hosts_from_json(&val, &mut found);
    }

    dedup_hosts(found)
}

fn extract_hosts_from_json(value: &Value, out: &mut Vec<SshHostConfig>) {
    match value {
        Value::Object(map) => {
            if let Some(hostname) = map
                .get("hostname")
                .or_else(|| map.get("host"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty() && !s.contains(' '))
            {
                let user = map
                    .get("user")
                    .or_else(|| map.get("ssh_user"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let port = map
                    .get("port")
                    .or_else(|| map.get("ssh_port"))
                    .and_then(Value::as_u64)
                    .and_then(|p| u16::try_from(p).ok())
                    .unwrap_or(22);
                let name = map
                    .get("name")
                    .or_else(|| map.get("label"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(hostname)
                    .to_string();
                if hostname.contains('.') || !user.is_empty() {
                    out.push(SshHostConfig {
                        name,
                        hostname: hostname.to_string(),
                        user,
                        port,
                    });
                }
            }
            for child in map.values() {
                extract_hosts_from_json(child, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                extract_hosts_from_json(item, out);
            }
        }
        _ => {}
    }
}

fn dedup_hosts(hosts: Vec<SshHostConfig>) -> Vec<SshHostConfig> {
    let mut unique: Vec<SshHostConfig> = Vec::new();
    for host in hosts {
        let dup = unique.iter().any(|existing| {
            existing.hostname.eq_ignore_ascii_case(&host.hostname)
                && existing.user.eq_ignore_ascii_case(&host.user)
                && existing.port == host.port
        });
        if !dup {
            unique.push(host);
        }
    }
    unique
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_hosts_readonly_without_touching_source() {
        let dir = std::env::temp_dir().join(format!("multibot-zigshel-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("session.json");
        std::fs::write(
            &file,
            r#"{"servers":[{"name":"hypertask","hostname":"vps.example.net","user":"valentin","port":2222},{"name":"hypertask-dup","hostname":"vps.example.net","user":"valentin","port":2222}]}"#,
        )
        .unwrap();

        let hosts = import_ssh_hosts_readonly(Some(&dir));
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].name, "hypertask");
        assert_eq!(hosts[0].hostname, "vps.example.net");
        assert_eq!(hosts[0].user, "valentin");
        assert_eq!(hosts[0].port, 2222);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
