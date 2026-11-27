use crate::settings::{SshHostConfig, sanitize_identity_file};
use crate::workspaces::{
    MAX_WORKSPACE_NAME_CHARS, STARRED_WORKSPACE_ID, Workspace, WorkspaceStore,
    canonical_session_key_str, parse_session_key, truncate_chars,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

const MAX_IMPORT_FILE_BYTES: u64 = 16 * 1024 * 1024; // 16 MiB per-file safety cap

const ZIGSHEL_PALETTE_HEX: [&str; 8] = [
    "#60A5FA", "#34D399", "#F59E0B", "#F472B6", "#A78BFA", "#FB7185", "#38BDF8", "#FBBF24",
];

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ImportedWorkspace {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub snoozed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ImportedFavorite {
    pub session_key: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub workspace_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ImportedSession {
    pub session_key: String,
    pub face: Option<u8>,
    pub color: Option<String>,
    pub title: Option<String>,
    pub workspace_id: Option<String>,
    pub favorite: bool,
    pub pinned: bool,
    pub starred_pinned: bool,
    pub muted: bool,
    pub inbox_excluded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ZigShellImportBundle {
    pub hosts: Vec<SshHostConfig>,
    pub workspaces: Vec<ImportedWorkspace>,
    pub favorites: Vec<ImportedFavorite>,
    pub favorite_keys: Vec<String>,
    #[serde(default)]
    pub sessions: Vec<ImportedSession>,
    pub source_files_read: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ImportMergeStats {
    pub workspaces_added: usize,
    pub favorites_added: usize,
    pub sessions_assigned: usize,
}

/// Returns the legacy `%APPDATA%\ZigShell` directory (read-only source).
pub fn legacy_zigshel_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ZigShell")
}

/// Strictly read-only import of SSH hosts from legacy `%APPDATA%\ZigShell` files.
/// Never modifies or creates any files inside `ZigShell`.
pub fn import_ssh_hosts_readonly(zigshel_dir: Option<&Path>) -> Vec<SshHostConfig> {
    import_zigshel_bundle_readonly(zigshel_dir).hosts
}

/// Strictly read-only import of legacy ZigShell hosts, workspaces, sessions, and favorites.
/// Never modifies, creates, or deletes any files inside the legacy directory.
pub fn import_zigshel_bundle_readonly(zigshel_dir: Option<&Path>) -> ZigShellImportBundle {
    let dir = zigshel_dir
        .map(PathBuf::from)
        .unwrap_or_else(legacy_zigshel_dir);
    import_zigshel_bundle_from_dir(&dir)
}

fn is_safe_ssh_token(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && !s.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn is_regular_non_symlink_dir(path: &Path) -> bool {
    let normalized: PathBuf = path.components().collect();
    if normalized.as_os_str().is_empty() {
        return false;
    }
    std::fs::symlink_metadata(&normalized)
        .map(|m| m.file_type().is_dir() && !m.file_type().is_symlink())
        .unwrap_or(false)
}

fn read_regular_file_capped(path: &Path, max_bytes: u64) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    let ft = meta.file_type();
    if !ft.is_file() || ft.is_symlink() || meta.len() > max_bytes {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut buf)
        .ok()?;
    if buf.len() as u64 > max_bytes {
        return None;
    }
    String::from_utf8(buf).ok()
}

/// Strictly read-only import from an explicit ZigShell directory path.
/// Rejects symlinks and enforces a 16 MiB per-file read cap.
pub fn import_zigshel_bundle_from_dir(dir: &Path) -> ZigShellImportBundle {
    if !is_regular_non_symlink_dir(dir) {
        return ZigShellImportBundle::default();
    }

    let mut bundle = ZigShellImportBundle::default();
    let mut raw_hosts: Vec<SshHostConfig> = Vec::new();
    let mut host_slot_map: HashMap<usize, String> = HashMap::new();

    // 1 & 2. Read-only `ssh-key/servers.txt` and `ssh-key/config` (never follow symlinked ssh-key)
    let ssh_key_dir = dir.join("ssh-key");
    if is_regular_non_symlink_dir(&ssh_key_dir) {
        let servers_txt = ssh_key_dir.join("servers.txt");
        if let Some(text) = read_regular_file_capped(&servers_txt, MAX_IMPORT_FILE_BYTES) {
            bundle.source_files_read.push(servers_txt);
            parse_servers_txt(&text, &mut raw_hosts, &mut host_slot_map);
        }

        let ssh_config = ssh_key_dir.join("config");
        if let Some(text) = read_regular_file_capped(&ssh_config, MAX_IMPORT_FILE_BYTES) {
            bundle.source_files_read.push(ssh_config);
            parse_ssh_config(
                &text,
                Some(&ssh_key_dir),
                &mut raw_hosts,
                &mut host_slot_map,
            );
        }
    }

    // 3. Collect and deterministically sort top-level regular `*.json` files (case-insensitive)
    let mut json_files: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let is_regular = std::fs::symlink_metadata(&path)
                .map(|m| m.file_type().is_file() && !m.file_type().is_symlink())
                .unwrap_or(false);
            let is_json = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("json"));
            if is_regular && is_json {
                json_files.push(path);
            }
        }
    }
    json_files.sort_by(|a, b| {
        let rank = |p: &Path| -> (u8, String) {
            let name = p
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let prio = match name.as_str() {
                "session.json" => 0,
                "windows.json" => 1,
                _ => 2,
            };
            (prio, name)
        };
        rank(a).cmp(&rank(b))
    });

    let mut workspaces: Vec<ImportedWorkspace> = Vec::new();
    let mut favorites: Vec<ImportedFavorite> = Vec::new();
    let mut primary_ws_slots: Option<Vec<String>> = None;

    for path in json_files {
        let Some(text) = read_regular_file_capped(&path, MAX_IMPORT_FILE_BYTES) else {
            continue;
        };
        let Ok(val) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        bundle.source_files_read.push(path.clone());

        // Top-level-only hosts extraction (never recurses into tabs/extra/unknown)
        extract_hosts_from_json(&val, Some(dir), &mut raw_hosts);

        let Some(root) = val.as_object() else {
            continue;
        };

        // Top-level-only workspaces extraction (per-file slot table for tab.workspace index)
        let mut file_ws_slots: Vec<String> = Vec::new();
        if let Some(ws_arr) = root.get("workspaces").and_then(Value::as_array) {
            for (slot_idx, item) in ws_arr.iter().enumerate() {
                let Some(obj) = item.as_object() else {
                    file_ws_slots.push(format!("ws-{slot_idx}"));
                    continue;
                };
                let Some(name_raw) = obj
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                else {
                    file_ws_slots.push(format!("ws-{slot_idx}"));
                    continue;
                };
                let name = truncate_chars(name_raw, MAX_WORKSPACE_NAME_CHARS);
                let id = obj
                    .get("wid")
                    .or_else(|| obj.get("id"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty() && *s != STARRED_WORKSPACE_ID)
                    .map(str::to_string)
                    .unwrap_or_else(|| deterministic_ws_slug(&name, slot_idx));
                let color = parse_zigshel_color(obj.get("color"));
                let snoozed = obj
                    .get("snoozed")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let archived = obj
                    .get("archived")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);

                file_ws_slots.push(id.clone());
                upsert_imported_workspace(
                    &mut workspaces,
                    ImportedWorkspace {
                        id,
                        name,
                        color,
                        archived,
                        snoozed,
                    },
                );
            }
            if primary_ws_slots.is_none() {
                primary_ws_slots = Some(file_ws_slots.clone());
            }
        }
        if file_ws_slots.is_empty() {
            file_ws_slots = primary_ws_slots.clone().unwrap_or_default();
        }

        // Top-level-only `windows.json` workspace registry
        if let Some(win_arr) = root.get("windows").and_then(Value::as_array) {
            for item in win_arr {
                let Some(obj) = item.as_object() else {
                    continue;
                };
                let Some(id) = obj
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty() && *s != STARRED_WORKSPACE_ID)
                else {
                    continue;
                };
                let name = obj
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(id);
                let archived = obj
                    .get("archived")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                upsert_imported_workspace(
                    &mut workspaces,
                    ImportedWorkspace {
                        id: id.to_string(),
                        name: truncate_chars(name, MAX_WORKSPACE_NAME_CHARS),
                        color: None,
                        archived,
                        snoozed: false,
                    },
                );
            }
        }

        // Top-level-only tabs (NEVER recurse into tab["extra"] or root["unknown"])
        if let Some(tabs_arr) = root.get("tabs").and_then(Value::as_array) {
            for tab in tabs_arr {
                let Some(obj) = tab.as_object() else {
                    continue;
                };
                let fields: &[&str] = if obj.get("kind").and_then(Value::as_str) == Some("tmux")
                    && obj
                        .get("tmux_session")
                        .and_then(Value::as_str)
                        .is_some_and(|s| !s.trim().is_empty())
                {
                    &["tmux_session"]
                } else {
                    &[
                        "csid",
                        "session_id",
                        "tmux_identity",
                        "tmux_session",
                        "name",
                    ]
                };
                let Some(sid) = fields.iter().find_map(|field| {
                    obj.get(*field)
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                }) else {
                    continue;
                };

                let host = derive_tab_host(obj, &host_slot_map);
                let agent = derive_tab_agent(obj);
                let key = canonical_session_key_str(&host, &agent, sid);
                let title = obj
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                let workspace_id = obj
                    .get("workspace")
                    .and_then(Value::as_u64)
                    .and_then(|idx| file_ws_slots.get(idx as usize).cloned())
                    .or_else(|| {
                        obj.get("workspace_id")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                    })
                    .or_else(|| file_ws_slots.first().cloned());
                let favorite = obj.get("favorite") == Some(&Value::Bool(true));
                if favorite {
                    favorites.push(ImportedFavorite {
                        session_key: key.clone(),
                        title: title.clone(),
                        workspace_id: workspace_id.clone(),
                    });
                }
                bundle.sessions.push(ImportedSession {
                    session_key: key,
                    face: obj
                        .get("face")
                        .and_then(Value::as_u64)
                        .filter(|face| *face < 20)
                        .map(|face| face as u8),
                    color: parse_zigshel_color(obj.get("color")),
                    title,
                    workspace_id,
                    favorite,
                    pinned: obj.get("pinned_top") == Some(&Value::Bool(true)),
                    starred_pinned: obj.get("starred_pinned_top") == Some(&Value::Bool(true)),
                    muted: obj.get("muted") == Some(&Value::Bool(true)),
                    inbox_excluded: obj.get("inbox_excluded") == Some(&Value::Bool(true)),
                });
            }
        }

        // Top-level-only `favorites` string array (canonicalized via parse_session_key)
        if let Some(fav_arr) = root.get("favorites").and_then(Value::as_array) {
            for item in fav_arr {
                if let Some(raw_key) = item.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                    let session_key = if let Some((host, agent, sid)) = parse_session_key(raw_key) {
                        canonical_session_key_str(host, agent, sid)
                    } else {
                        raw_key.to_string()
                    };
                    favorites.push(ImportedFavorite {
                        session_key,
                        title: None,
                        workspace_id: None,
                    });
                }
            }
        }
    }

    // Deduplicate favorites preserving discovery order
    let mut seen_favs: HashSet<String> = HashSet::new();
    let mut deduped_favs = Vec::new();
    let mut favorite_keys = Vec::new();
    for fav in favorites {
        if seen_favs.insert(fav.session_key.clone()) {
            favorite_keys.push(fav.session_key.clone());
            deduped_favs.push(fav);
        }
    }

    bundle.hosts = dedup_hosts(raw_hosts);
    bundle.workspaces = workspaces;
    bundle.favorites = deduped_favs;
    bundle.favorite_keys = favorite_keys;
    bundle
}

/// Idempotently merges a `ZigShellImportBundle` into `WorkspaceStore`, preserving existing
/// user workspaces and avoiding duplicate workspaces or favorites.
pub fn merge_zigshel_bundle(
    store: &mut WorkspaceStore,
    bundle: &ZigShellImportBundle,
) -> ImportMergeStats {
    let mut stats = ImportMergeStats::default();
    store.ensure_default_workspace();

    let mut ws_id_remap: HashMap<String, String> = HashMap::new();
    for ws in &bundle.workspaces {
        if ws.id == STARRED_WORKSPACE_ID {
            continue;
        }
        let existing = store
            .workspaces
            .iter()
            .find(|existing| existing.id.eq_ignore_ascii_case(&ws.id))
            .or_else(|| {
                store
                    .workspaces
                    .iter()
                    .find(|existing| existing.name.eq_ignore_ascii_case(&ws.name))
            });
        if let Some(existing) = existing {
            ws_id_remap.insert(ws.id.clone(), existing.id.clone());
        } else {
            ws_id_remap.insert(ws.id.clone(), ws.id.clone());
            let pos = store.workspaces.len();
            store.workspaces.push(Workspace {
                id: ws.id.clone(),
                name: ws.name.clone(),
                color: ws.color.clone(),
                archived: ws.archived,
                snoozed: ws.snoozed,
                snoozed_until_ms: None,
                position: pos,
                last_active_session: None,
            });
            stats.workspaces_added += 1;
        }
    }

    let sessions = bundle.sessions.iter().cloned().chain(
        bundle.favorites.iter().map(|fav| ImportedSession {
            session_key: fav.session_key.clone(),
            title: fav.title.clone(),
            workspace_id: fav.workspace_id.clone(),
            favorite: true,
            ..Default::default()
        }),
    );
    for session in sessions {
        if session.favorite && store.favorites.insert(session.session_key.clone()) {
            stats.favorites_added += 1;
        }
        let meta = store
            .session_meta
            .entry(session.session_key.clone())
            .or_default();
        if meta.workspace_id.is_none()
            && let Some(ws_id) = session.workspace_id.as_ref()
        {
            let mapped = ws_id_remap.get(ws_id).unwrap_or(ws_id);
            if let Some(ws) = store.workspaces.iter().find(|w| w.id == *mapped) {
                meta.workspace_id = Some(ws.id.clone());
                stats.sessions_assigned += 1;
            }
        }
        if meta.face.is_none() {
            meta.face = session.face;
        }
        if meta.color.is_none() {
            meta.color = session.color;
        }
        if !meta.pin_overridden {
            meta.pinned |= session.pinned;
        }
        if !meta.starred_pin_overridden {
            meta.starred_pinned |= session.starred_pinned;
        }
        meta.muted |= session.muted;
        if session.inbox_excluded {
            store.inbox_excluded.insert(session.session_key.clone());
        }
        if !store.session_titles.contains_key(&session.session_key)
            && let Some(title) = session.title.as_deref()
        {
            let _ = store.apply_session_rename(&session.session_key, title);
        }
    }

    stats
}

fn parse_servers_txt(
    text: &str,
    out: &mut Vec<SshHostConfig>,
    slot_map: &mut HashMap<usize, String>,
) {
    let mut lines = text.lines();
    let Some(first) = lines.next() else {
        return;
    };
    if first.trim() != "v1" {
        return;
    }

    for (slot_idx, line) in lines.enumerate() {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 7 {
            continue;
        }
        let status = cols[0].trim();
        if status.eq_ignore_ascii_case("empty") || status.eq_ignore_ascii_case("removed") {
            continue;
        }
        let Ok(port) = cols[2].trim().parse::<u16>() else {
            continue;
        };
        if port == 0 {
            continue;
        }
        let raw_hostname = cols[4];
        if raw_hostname.chars().any(char::is_control) {
            continue;
        }
        let hostname = raw_hostname.trim();
        if !is_safe_ssh_token(hostname) {
            continue;
        }
        let raw_user = cols[5];
        if raw_user.chars().any(char::is_control) {
            continue;
        }
        let user = raw_user.trim();
        if !user.is_empty() && !is_safe_ssh_token(user) {
            continue;
        }
        let name_col = cols[6].trim();
        let name = if name_col.is_empty() {
            hostname
        } else {
            name_col
        };

        let canonical_name = out
            .iter()
            .find(|existing| {
                existing.hostname.eq_ignore_ascii_case(hostname)
                    && existing.user.eq_ignore_ascii_case(user)
                    && existing.port == port
            })
            .map(|existing| existing.name.as_str())
            .unwrap_or(name);
        slot_map.insert(slot_idx, canonical_name.to_string());
        out.push(SshHostConfig {
            name: name.to_string(),
            hostname: hostname.to_string(),
            user: user.to_string(),
            port,
            identity_file: None,
        });
    }
}

/// Splits an OpenSSH config line into `(keyword, raw_value)`:
/// - Ignores empty lines and `#` comment lines.
/// - Strips unquoted trailing `# ...` comments while preserving `#` inside `"..."`.
/// - Supports `Key value`, `Key=value`, `Key = value`, and tab separators.
/// - Preserves internal spaces inside quoted values (critical for paths with spaces).
fn split_ssh_config_directive(raw_line: &str) -> Option<(&str, &str)> {
    let mut in_quotes = false;
    let mut comment_start = raw_line.len();
    for (idx, ch) in raw_line.char_indices() {
        match ch {
            '"' => in_quotes = !in_quotes,
            '#' if !in_quotes => {
                comment_start = idx;
                break;
            }
            _ => {}
        }
    }
    let line = raw_line[..comment_start].trim_matches(|c| c == ' ' || c == '\t');
    if line.is_empty() {
        return None;
    }

    let key_end = line
        .find(|c: char| c == ' ' || c == '\t' || c == '=')
        .unwrap_or(line.len());
    let key = &line[..key_end];
    if key.is_empty() {
        return None;
    }

    let mut rest = line[key_end..].trim_start_matches(|c| c == ' ' || c == '\t');
    if let Some(after_eq) = rest.strip_prefix('=') {
        rest = after_eq.trim_start_matches(|c| c == ' ' || c == '\t');
    }
    let val = rest.trim_end_matches(|c| c == ' ' || c == '\t');
    if val.is_empty() {
        return None;
    }
    Some((key, val))
}

/// Unquotes a double-quoted SSH config value (`"..."`). Rejects unmatched/embedded quotes.
fn unquote_ssh_value(raw: &str) -> Option<&str> {
    if let Some(rest) = raw.strip_prefix('"') {
        let inner = rest.strip_suffix('"')?;
        if inner.contains('"') {
            return None;
        }
        Some(inner)
    } else if raw.contains('"') {
        None
    } else {
        Some(raw)
    }
}

/// Cross-platform check for absolute or `~`-rooted SSH paths so Windows drive/UNC paths
/// (`C:\...`, `C:/...`, `\\server\share\...`), Unix absolute paths (`/...`), and `~/...`
/// stay intact on both Linux and Windows hosts.
fn is_absolute_or_tilde_ssh_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    if path.starts_with('/') || path.starts_with('\\') {
        return true;
    }
    if path == "~" || path.starts_with("~/") || path.starts_with("~\\") {
        return true;
    }
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
}

/// Parses, validates, and resolves an `IdentityFile` value from an SSH config or JSON host entry.
/// SECURITY: validates BEFORE resolving relative paths so `-oProxyCommand=...` cannot be
/// disguised by prepending `base_dir`, then re-validates the resolved path.
fn parse_identity_file_value(raw_val: &str, base_dir: Option<&Path>) -> Option<String> {
    let unquoted = unquote_ssh_value(raw_val)?;
    let safe_raw = sanitize_identity_file(unquoted)?;

    if is_absolute_or_tilde_ssh_path(&safe_raw) {
        return Some(safe_raw);
    }

    if let Some(dir) = base_dir {
        let rel = safe_raw
            .strip_prefix("./")
            .or_else(|| safe_raw.strip_prefix(".\\"))
            .unwrap_or(&safe_raw);
        let joined = dir.join(rel).to_string_lossy().into_owned();
        sanitize_identity_file(&joined)
    } else {
        Some(safe_raw)
    }
}

fn parse_ssh_config(
    text: &str,
    base_dir: Option<&Path>,
    out: &mut Vec<SshHostConfig>,
    host_slot_map: &mut HashMap<usize, String>,
) {
    let mut current_alias: Option<String> = None;
    let mut current_hostname: Option<String> = None;
    let mut current_user: String = String::new();
    let mut current_port: u16 = 22;
    let mut current_identity_file: Option<String> = None;

    let mut flush = |alias: &mut Option<String>,
                     hostname: &mut Option<String>,
                     user: &mut String,
                     port: &mut u16,
                     identity_file: &mut Option<String>,
                     out: &mut Vec<SshHostConfig>| {
        if let Some(a) = alias.take()
            && a != "*"
            && !a.contains('*')
            && !a.contains('?')
            && let Some(h) = hostname.take().filter(|s| is_safe_ssh_token(s))
            && (user.is_empty() || is_safe_ssh_token(user))
        {
            let slot = match a.to_ascii_lowercase().as_str() {
                "remote-1" => Some(0),
                "remote-2" => Some(1),
                "remote-3" => Some(2),
                _ => None,
            };
            if let Some(slot) = slot {
                host_slot_map.entry(slot).or_insert_with(|| a.clone());
            }
            out.push(SshHostConfig {
                name: a,
                hostname: h,
                user: std::mem::take(user),
                port: *port,
                identity_file: identity_file.take(),
            });
        } else {
            *hostname = None;
            user.clear();
            *identity_file = None;
        }
        *port = 22;
    };

    for raw_line in text.lines() {
        let Some((key, val)) = split_ssh_config_directive(raw_line) else {
            continue;
        };

        if key.eq_ignore_ascii_case("host") {
            flush(
                &mut current_alias,
                &mut current_hostname,
                &mut current_user,
                &mut current_port,
                &mut current_identity_file,
                out,
            );
            // Take first alias token
            let first_alias = val.split_whitespace().next().unwrap_or("");
            if !first_alias.is_empty() && first_alias != "*" {
                current_alias = Some(first_alias.to_string());
            }
        } else if key.eq_ignore_ascii_case("match") {
            // OpenSSH `Match` terminates the preceding `Host` stanza; directives inside
            // `Match` blocks must never bleed into preceding or subsequent `Host` entries.
            flush(
                &mut current_alias,
                &mut current_hostname,
                &mut current_user,
                &mut current_port,
                &mut current_identity_file,
                out,
            );
        } else if key.eq_ignore_ascii_case("hostname") {
            current_hostname = Some(val.to_string());
        } else if key.eq_ignore_ascii_case("user") {
            current_user = val.to_string();
        } else if key.eq_ignore_ascii_case("port")
            && let Ok(p) = val.parse::<u16>()
            && p > 0
        {
            current_port = p;
        } else if key.eq_ignore_ascii_case("identityfile") {
            // First valid IdentityFile in the Host stanza wins (OpenSSH/ZigShell semantics).
            if current_identity_file.is_none()
                && let Some(resolved) = parse_identity_file_value(val, base_dir)
            {
                current_identity_file = Some(resolved);
            }
        }
    }

    flush(
        &mut current_alias,
        &mut current_hostname,
        &mut current_user,
        &mut current_port,
        &mut current_identity_file,
        out,
    );
}

fn parse_zigshel_color(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) => {
            let trimmed = s.trim();
            let hex = trimmed.strip_prefix('#').unwrap_or(trimmed);
            if hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                Some(format!("#{hex}"))
            } else {
                None
            }
        }
        Value::Number(n) => {
            let idx = usize::from(u8::try_from(n.as_u64()?).ok()?);
            Some(ZIGSHEL_PALETTE_HEX[idx % ZIGSHEL_PALETTE_HEX.len()].to_string())
        }
        _ => None,
    }
}

fn deterministic_ws_slug(name: &str, slot_idx: usize) -> String {
    let mut slug = String::new();
    let mut prev_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !slug.is_empty() {
            slug.push('-');
            prev_dash = true;
        }
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() || slug == STARRED_WORKSPACE_ID {
        format!("ws-{slot_idx}")
    } else {
        slug
    }
}

fn upsert_imported_workspace(list: &mut Vec<ImportedWorkspace>, incoming: ImportedWorkspace) {
    if let Some(existing) = list
        .iter_mut()
        .find(|w| w.id.eq_ignore_ascii_case(&incoming.id))
    {
        if incoming.archived {
            existing.archived = true;
        }
        if incoming.snoozed {
            existing.snoozed = true;
        }
        if existing.color.is_none() && incoming.color.is_some() {
            existing.color = incoming.color;
        }
    } else {
        list.push(incoming);
    }
}

fn derive_tab_host(
    obj: &serde_json::Map<String, Value>,
    slot_map: &HashMap<usize, String>,
) -> String {
    if let Some(h) = obj
        .get("host")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return h.to_string();
    }
    if let Some(tmux_host) = obj.get("tmux_host") {
        if let Some(s) = tmux_host.as_str().map(str::trim).filter(|s| !s.is_empty()) {
            return s.to_string();
        }
        if let Some(slot) = tmux_host.as_u64() {
            let idx = slot as usize;
            if let Some(mapped) = slot_map.get(&idx) {
                return mapped.clone();
            }
            return format!("vps{idx}");
        }
    }
    let slot = match obj.get("kind").and_then(Value::as_str) {
        Some("tmux" | "vps") => 0,
        Some("vps2") => 1,
        _ => return "local".to_string(),
    };
    slot_map
        .get(&slot)
        .cloned()
        .unwrap_or_else(|| format!("vps{slot}"))
}

fn derive_tab_agent(obj: &serde_json::Map<String, Value>) -> String {
    if obj.get("kind").and_then(Value::as_str) == Some("tmux")
        && obj
            .get("tmux_session")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
    {
        return "shell".to_string();
    }
    if let Some(a) = obj
        .get("agent")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return a.to_string();
    }
    if let Some(agent) = obj
        .get("ai_agent")
        .and_then(Value::as_u64)
        .filter(|a| *a != 0)
    {
        return match agent {
            1 => "claude",
            2 => "codex",
            3 => "pi",
            7 => "antigravity",
            _ => "claude",
        }
        .to_string();
    }
    if let Some(tmux) = obj.get("tmux_session").and_then(Value::as_str) {
        let low = tmux.to_ascii_lowercase();
        if low.starts_with("codex") || low.contains("codex") {
            return "codex".to_string();
        }
        if low.starts_with("agy") || low.contains("antigravity") {
            return "antigravity".to_string();
        }
    }
    "claude".to_string()
}

fn parse_host_object(
    map: &serde_json::Map<String, Value>,
    base_dir: Option<&Path>,
) -> Option<SshHostConfig> {
    let raw_hostname = map
        .get("hostname")
        .or_else(|| map.get("host"))
        .and_then(Value::as_str)?;
    if raw_hostname.chars().any(char::is_control) {
        return None;
    }
    let hostname = raw_hostname.trim();
    if !is_safe_ssh_token(hostname) {
        return None;
    }
    let raw_user = map
        .get("user")
        .or_else(|| map.get("ssh_user"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if raw_user.chars().any(char::is_control) {
        return None;
    }
    let user = raw_user.trim().to_string();
    if !user.is_empty() && !is_safe_ssh_token(&user) {
        return None;
    }
    let port = map
        .get("port")
        .or_else(|| map.get("ssh_port"))
        .and_then(Value::as_u64)
        .and_then(|p| u16::try_from(p).ok())
        .filter(|&p| p > 0)
        .unwrap_or(22);
    let name = map
        .get("name")
        .or_else(|| map.get("label"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(hostname)
        .to_string();
    let identity_file = map
        .get("identity_file")
        .or_else(|| map.get("identityFile"))
        .and_then(Value::as_str)
        .and_then(|raw| parse_identity_file_value(raw, base_dir));
    if hostname.contains('.') || !user.is_empty() {
        Some(SshHostConfig {
            name,
            hostname: hostname.to_string(),
            user,
            port,
            identity_file,
        })
    } else {
        None
    }
}

fn extract_hosts_from_json(value: &Value, base_dir: Option<&Path>, out: &mut Vec<SshHostConfig>) {
    match value {
        Value::Object(root) => {
            for key in ["servers", "hosts", "ssh_hosts", "connections"] {
                let Some(collection) = root.get(key) else {
                    continue;
                };
                match collection {
                    Value::Array(items) => {
                        for item in items {
                            if let Some(obj) = item.as_object()
                                && let Some(host) = parse_host_object(obj, base_dir)
                            {
                                out.push(host);
                            }
                        }
                    }
                    Value::Object(obj) => {
                        if let Some(host) = parse_host_object(obj, base_dir) {
                            out.push(host);
                        } else {
                            for child in obj.values() {
                                if let Some(child_obj) = child.as_object()
                                    && let Some(host) = parse_host_object(child_obj, base_dir)
                                {
                                    out.push(host);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                if let Some(obj) = item.as_object()
                    && let Some(host) = parse_host_object(obj, base_dir)
                {
                    out.push(host);
                }
            }
        }
        _ => {}
    }
}

fn dedup_hosts(hosts: Vec<SshHostConfig>) -> Vec<SshHostConfig> {
    let mut unique: Vec<SshHostConfig> = Vec::new();
    for host in hosts {
        if let Some(existing) = unique.iter_mut().find(|existing| {
            existing.hostname.eq_ignore_ascii_case(&host.hostname)
                && existing.user.eq_ignore_ascii_case(&host.user)
                && existing.port == host.port
        }) {
            if existing.identity_file.is_none() && host.identity_file.is_some() {
                existing.identity_file = host.identity_file;
            }
        } else {
            unique.push(host);
        }
    }
    unique
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static IMPORT_TEST_SEQ: AtomicU64 = AtomicU64::new(1);

    fn unique_dir(label: &str) -> PathBuf {
        let seq = IMPORT_TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "multibot-zigshel-{label}-{}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn tmux_tabs_keep_exact_identity_and_restore_shell_metadata_across_host_aliases() {
        use crate::workspaces::{SessionInput, WorkspaceView, canonical_session_key};
        use multibot_proto::{AgentKind, SessionSummary};
        let dir = unique_dir("shells");
        let raw = r#"{"workspaces":[{"name":"Multi Bot","wid":"multibot"}],"tabs":[
            {"kind":"tmux","csid":"not-the-shell-id","tmux_session":"term-56d16ef56ce668ab2d7235d20e7ecd75","name":"Multi Bot AGY Shell","workspace":0,"favorite":true,"muted":true},
            {"kind":"tmux","tmux_session":"faceless-channels","name":"Faceless Channels","workspace":0},
            {"kind":"tmux","tmux_session":"term-btop-session","name":"BTOP","workspace":0}
        ]}"#;
        std::fs::write(dir.join("session.json"), raw).unwrap();
        let bundle = import_zigshel_bundle_from_dir(&dir);
        assert_eq!(bundle.sessions.len(), 3);
        assert_eq!(
            bundle.sessions[0].session_key,
            "vps0:shell:term-56d16ef56ce668ab2d7235d20e7ecd75"
        );
        let mut store = WorkspaceStore::new();
        merge_zigshel_bundle(&mut store, &bundle);
        let live = SessionSummary {
            id: "term-56d16ef56ce668ab2d7235d20e7ecd75".into(),
            agent: AgentKind::Shell,
            title: "fallback".into(),
            ..Default::default()
        };
        let key = canonical_session_key("hypertask-vps", live.agent, &live.id);
        assert!(store.reconcile_live_session_keys(&[key.clone()]) > 0);
        assert_eq!(
            store.effective_title(&key, &live.title),
            "Multi Bot AGY Shell"
        );
        assert_eq!(
            store.session_meta[&key].workspace_id.as_deref(),
            Some("multibot")
        );
        assert!(store.session_meta[&key].muted);
        assert!(store.favorites.contains(&key));
        let input = SessionInput {
            key: key.clone(),
            host: "hypertask-vps",
            summary: &live,
        };
        let layout = store.compute_sidebar_layout_with_options(
            "multibot",
            WorkspaceView::All,
            &[input],
            false,
            Some(&key),
            0,
        );
        assert!(layout.ctrl_digit_order.contains(&key));
        assert_eq!(
            std::fs::read_to_string(dir.join("session.json")).unwrap(),
            raw
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn imported_vps_sessions_appear_in_exact_workspaces_and_favorites() {
        use crate::workspaces::{
            DEFAULT_WORKSPACE_ID, SessionInput, WorkspaceView, canonical_session_key,
        };
        use multibot_proto::{AgentKind, SessionSummary};

        let dir = unique_dir("session-assignment");
        std::fs::create_dir_all(dir.join("ssh-key")).unwrap();
        let servers = "v1\nconnected\t1\t22\t1000\tvps.example.com\tubuntu\tVPS Hetzner\t\n";
        let session_json = r#"{
            "workspaces": [
                {"name":"General", "wid":"legacy-general"},
                {"name":"Vstack", "wid":"legacy-vstack"},
                {"name":"HT Product", "wid":"ht-product"},
                {"name":"Multiprompt", "wid":"multiprompt"}
            ],
            "tabs": [
                {"kind":"tmux", "ai_agent":1, "csid":"general-session"},
                {"kind":"vps", "ai_agent":2, "csid":"codex-uuid", "workspace":1,
                 "name":"Ship Vstack", "manual":true, "pinned_top":true,
                 "muted":true, "inbox_excluded":true},
                {"kind":"tmux", "ai_agent":1, "csid":"product-session", "workspace":2,
                 "favorite":true, "starred_pinned_top":true},
                {"kind":"tmux", "ai_agent":7, "csid":"agy-session", "workspace":3,
                 "extra":{"kind":"tmux", "csid":"nested-decoy", "favorite":true}}
            ],
            "unknown":{"tabs":[{"csid":"root-decoy", "favorite":true}]}
        }"#;
        std::fs::write(dir.join("ssh-key/servers.txt"), servers).unwrap();
        std::fs::write(dir.join("session.json"), session_json).unwrap();

        let bundle = import_zigshel_bundle_from_dir(&dir);
        let mut store = WorkspaceStore::new();
        let vstack = store.create_workspace("vSTACK", None).unwrap();
        let stats = merge_zigshel_bundle(&mut store, &bundle);
        assert_eq!(stats.sessions_assigned, 4);
        assert_eq!(stats.favorites_added, 1);
        let state_dir = unique_dir("imported-state");
        let state_path = state_dir.join("workspaces.json");
        store.save_to(&state_path).unwrap();
        let store = WorkspaceStore::load_from(&state_path).unwrap();
        let summaries: Vec<_> = [
            ("general-session", AgentKind::Claude),
            ("codex-uuid", AgentKind::Codex),
            ("product-session", AgentKind::Claude),
            ("agy-session", AgentKind::Antigravity),
        ]
        .into_iter()
        .map(|(id, agent)| SessionSummary {
            id: id.into(),
            agent,
            title: "Discovered title".into(),
            ..Default::default()
        })
        .collect();
        let inputs: Vec<_> = summaries
            .iter()
            .map(|summary| SessionInput {
                key: canonical_session_key("VPS Hetzner", summary.agent, &summary.id),
                host: "VPS Hetzner",
                summary,
            })
            .collect();
        for (workspace, expected) in [
            (DEFAULT_WORKSPACE_ID, &inputs[0].key),
            (vstack.id.as_str(), &inputs[1].key),
            ("ht-product", &inputs[2].key),
            ("multiprompt", &inputs[3].key),
            (STARRED_WORKSPACE_ID, &inputs[2].key),
        ] {
            let layout = store.compute_sidebar_layout_with_options(
                workspace, WorkspaceView::All, &inputs, false, None, 1000,
            );
            assert_eq!(layout.flat_display_order, vec![expected.clone()], "{workspace}");
        }
        assert_eq!(store.session_meta.len(), 4);
        let codex = &store.session_meta[&inputs[1].key];
        assert!(codex.pinned && codex.muted);
        assert!(store.inbox_excluded.contains(&inputs[1].key));
        assert_eq!(store.effective_title(&inputs[1].key, "fallback"), "Ship Vstack");
        assert!(store.session_meta[&inputs[2].key].starred_pinned);
        assert_eq!(std::fs::read_to_string(dir.join("session.json")).unwrap(), session_json);
        assert_eq!(std::fs::read_to_string(dir.join("ssh-key/servers.txt")).unwrap(), servers);
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(state_dir).unwrap();
    }

    #[test]
    fn imports_host_slots_using_deduplicated_names_and_kind_defaults() {
        let dir = unique_dir("host-slots");
        std::fs::create_dir_all(dir.join("ssh-key")).unwrap();
        std::fs::write(
            dir.join("ssh-key/servers.txt"),
            concat!(
                "v1\n",
                "connected\t1\t22\t1000\tvps.example.com\tubuntu\tVPS Hetzner\t\n",
                "connected\t1\t22\t1000\tVPS.EXAMPLE.COM\tUbuntu\tDuplicate Alias\t\n",
            ),
        ).unwrap();
        std::fs::write(dir.join("session.json"), r#"{"tabs":[
            {"kind":"tmux", "csid":"zero"},
            {"kind":"vps", "csid":"vps-zero"},
            {"kind":"vps2", "csid":"one"},
            {"tmux_host":1, "csid":"explicit-one"},
            {"host":"Explicit Host", "tmux_host":1, "csid":"explicit-host"},
            {"tmux_host":"Named Host", "csid":"named-host"},
            {"kind":"local", "csid":"local"}
        ]}"#).unwrap();
        let bundle = import_zigshel_bundle_from_dir(&dir);
        assert_eq!(bundle.hosts.len(), 1);
        assert_eq!(bundle.hosts[0].name, "VPS Hetzner");
        let keys: Vec<_> = bundle.sessions.iter().map(|s| s.session_key.as_str()).collect();
        assert_eq!(keys, vec![
            "vps hetzner:claude:zero", "vps hetzner:claude:vps-zero",
            "vps hetzner:claude:one", "vps hetzner:claude:explicit-one",
            "explicit host:claude:explicit-host", "named host:claude:named-host",
            "local:claude:local",
        ]);
        std::fs::remove_file(dir.join("ssh-key/servers.txt")).unwrap();
        let fallback = import_zigshel_bundle_from_dir(&dir);
        assert_eq!(fallback.sessions[0].session_key, "vps0:claude:zero");
        assert_eq!(fallback.sessions[1].session_key, "vps0:claude:vps-zero");
        assert_eq!(fallback.sessions[2].session_key, "vps1:claude:one");
        assert_eq!(fallback.sessions[3].session_key, "vps1:claude:explicit-one");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn imports_agent_discriminants_before_session_name_heuristics() {
        let dir = unique_dir("agent-discriminants");
        std::fs::write(dir.join("session.json"), r#"{"tabs":[
            {"agent":"codex", "ai_agent":1, "csid":"explicit"},
            {"ai_agent":1, "tmux_session":"codex-misleading", "csid":"claude"},
            {"ai_agent":2, "claude_session":true, "csid":"codex"},
            {"ai_agent":3, "csid":"pi"},
            {"ai_agent":7, "csid":"antigravity"},
            {"ai_agent":0, "tmux_session":"codex-old", "csid":"codex-old"},
            {"tmux_session":"antigravity-old", "csid":"agy-old"},
            {"tmux_session":"agy-short", "csid":"agy-short"},
            {"claude_session":true, "csid":"claude-old"},
            {"csid":"csid-only"},
            {"ai_agent":99, "tmux_session":"codex-unknown", "csid":"unknown"}
        ]}"#).unwrap();
        let bundle = import_zigshel_bundle_from_dir(&dir);
        let keys: Vec<_> = bundle.sessions.iter().map(|s| s.session_key.as_str()).collect();
        assert_eq!(keys, vec![
            "local:codex:explicit", "local:claude:claude", "local:codex:codex",
            "local:pi:pi", "local:antigravity:antigravity", "local:codex:codex-old",
            "local:antigravity:agy-old", "local:antigravity:agy-short",
            "local:claude:claude-old", "local:claude:csid-only", "local:claude:unknown",
        ]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn imports_workspace_precedence_and_preserves_existing_choices_on_reimport() {
        let dir = unique_dir("workspace-precedence");
        std::fs::write(dir.join("session.json"), r#"{
            "workspaces":[{"wid":"legacy-general", "name":"GENERAL"},
                          {"wid":"WORK", "name":"Legacy Work"}],
            "tabs":[
                {"csid":"omitted", "name":"  My title  "},
                {"csid":"index", "workspace":1, "workspace_id":"legacy-general",
                 "favorite":true, "pinned_top":true, "starred_pinned_top":true,
                 "muted":true, "inbox_excluded":true},
                {"csid":"id", "workspace_id":"WORK", "name":"Old title"},
                {"csid":"fallback", "workspace":99},
                {"csid":"invalid", "workspace_id":"missing"}
            ]
        }"#).unwrap();
        let bundle = import_zigshel_bundle_from_dir(&dir);
        assert_eq!(bundle.sessions[0].workspace_id.as_deref(), Some("legacy-general"));
        assert_eq!(bundle.sessions[1].workspace_id.as_deref(), Some("WORK"));
        assert_eq!(bundle.sessions[2].workspace_id.as_deref(), Some("WORK"));
        assert_eq!(bundle.sessions[3].workspace_id.as_deref(), Some("legacy-general"));
        assert_eq!(bundle.favorite_keys, vec!["local:claude:index"]);
        let mut store = WorkspaceStore::new();
        store.create_workspace("Work", None).unwrap();
        store.assign_session_workspace("local:claude:id", "general");
        store.apply_session_rename("local:claude:id", "Keep my title").unwrap();
        let first = merge_zigshel_bundle(&mut store, &bundle);
        assert_eq!(first, ImportMergeStats {
            workspaces_added: 0, favorites_added: 1, sessions_assigned: 3,
        });
        assert_eq!(store.session_meta["local:claude:index"].workspace_id.as_deref(), Some("work"));
        assert_eq!(store.session_meta["local:claude:id"].workspace_id.as_deref(), Some("general"));
        assert!(store.session_meta["local:claude:invalid"].workspace_id.is_none());
        assert_eq!(store.effective_title("local:claude:omitted", "fallback"), "My title");
        assert_eq!(store.effective_title("local:claude:id", "fallback"), "Keep my title");
        assert!(store.session_titles["local:claude:omitted"].manual);
        let before = store.clone();
        assert_eq!(merge_zigshel_bundle(&mut store, &bundle), ImportMergeStats::default());
        assert_eq!(store, before);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn merges_older_favorite_bundles_with_workspace_name_remapping() {
        let mut store = WorkspaceStore::new();
        let bundle = ZigShellImportBundle {
            workspaces: vec![ImportedWorkspace {
                id: "legacy-general".into(),
                name: "GENERAL".into(),
                ..Default::default()
            }],
            favorites: vec![ImportedFavorite {
                session_key: "vps hetzner:codex:rollout".into(),
                title: Some("Favorite title".into()),
                workspace_id: Some("legacy-general".into()),
            }],
            ..Default::default()
        };
        let stats = merge_zigshel_bundle(&mut store, &bundle);
        assert_eq!(stats.workspaces_added, 0);
        assert_eq!(stats.favorites_added, 1);
        assert_eq!(stats.sessions_assigned, 1);
        assert_eq!(store.session_meta["vps hetzner:codex:rollout"].workspace_id.as_deref(), Some("general"));
        assert_eq!(store.effective_title("vps hetzner:codex:rollout", "fallback"), "Favorite title");
        assert_eq!(merge_zigshel_bundle(&mut store, &bundle), ImportMergeStats::default());
    }

    #[test]
    fn imports_sessions_without_workspaces_and_deserializes_older_bundles() {
        let dir = unique_dir("sessions-only");
        std::fs::write(dir.join("session.json"), r#"{"tabs":[
            {"csid":"ordinary", "name":"Named session", "favorite":false},
            {"csid":"untitled", "name":"   "},
            null, {"favorite":true}
        ]}"#).unwrap();
        let bundle = import_zigshel_bundle_from_dir(&dir);
        assert_eq!(bundle.sessions.len(), 2);
        assert!(bundle.sessions[0].workspace_id.is_none());
        assert!(bundle.sessions[1].title.is_none());
        assert!(bundle.favorites.is_empty());
        let mut store = WorkspaceStore::new();
        assert_eq!(merge_zigshel_bundle(&mut store, &bundle), ImportMergeStats::default());
        assert_eq!(store.session_meta.len(), 2);
        assert_eq!(store.effective_title("local:claude:ordinary", "fallback"), "Named session");
        let old: ZigShellImportBundle = serde_json::from_str(r#"{
            "hosts":[], "workspaces":[], "favorites":[], "favorite_keys":[], "source_files_read":[]
        }"#).unwrap();
        assert!(old.sessions.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ssh_remote_slots_fill_missing_servers_slots_without_overriding_them() {
        let dir = unique_dir("remote-slots");
        let ssh = dir.join("ssh-key");
        std::fs::create_dir_all(&ssh).unwrap();
        let config = concat!(
            "Host remote-1\n HostName one.example.com\n User ubuntu\n",
            "Host remote-2\n HostName two.example.com\n User ubuntu\n",
            "Host remote-3\n HostName three.example.com\n User ubuntu\n",
        );
        std::fs::write(ssh.join("config"), config).unwrap();
        std::fs::write(
            dir.join("session.json"),
            r#"{"tabs":[
            {"tmux_host":0,"csid":"session-one"},
            {"tmux_host":1,"csid":"session-two"},
            {"tmux_host":2,"csid":"session-three"}
        ]}"#,
        )
        .unwrap();
        for servers in [
            None,
            Some(""),
            Some("v1\nconnected\t1\t22\t1000\tone.example.com\tubuntu\tPreferred\t\n"),
        ] {
            if let Some(servers) = servers {
                std::fs::write(ssh.join("servers.txt"), servers).unwrap();
            }
            let bundle = import_zigshel_bundle_from_dir(&dir);
            let expected_host = if servers.is_some_and(|s| !s.is_empty()) {
                "preferred"
            } else {
                "remote-1"
            };
            assert_eq!(
                bundle.sessions[0].session_key,
                format!("{expected_host}:claude:session-one")
            );
            assert_eq!(
                bundle.sessions[1].session_key,
                "remote-2:claude:session-two"
            );
            assert_eq!(
                bundle.sessions[2].session_key,
                "remote-3:claude:session-three"
            );
            assert_eq!(std::fs::read_to_string(ssh.join("config")).unwrap(), config);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn secondary_sessions_inherit_primary_workspace_slots_but_can_define_their_own() {
        let dir = unique_dir("secondary-workspaces");
        let primary =
            r#"{"workspaces":[{"wid":"home","name":"Home"},{"wid":"work","name":"Work"}]}"#;
        let secondary = r#"{"tabs":[{"csid":"secondary-session","workspace":1}]}"#;
        std::fs::write(dir.join("session.json"), primary).unwrap();
        std::fs::write(dir.join("session-win2.json"), secondary).unwrap();
        std::fs::write(
            dir.join("session-win3.json"),
            r#"{
            "workspaces":[{"wid":"other","name":"Other"}],
            "tabs":[{"csid":"own-session","workspace":0}]
        }"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("session-win4.json"),
            secondary.replace("secondary-session", "last-session"),
        )
        .unwrap();
        let bundle = import_zigshel_bundle_from_dir(&dir);
        assert_eq!(bundle.sessions.len(), 3);
        assert_eq!(bundle.sessions[0].workspace_id.as_deref(), Some("work"));
        assert_eq!(bundle.sessions[1].workspace_id.as_deref(), Some("other"));
        assert_eq!(bundle.sessions[2].workspace_id.as_deref(), Some("work"));
        assert_eq!(
            std::fs::read_to_string(dir.join("session.json")).unwrap(),
            primary
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("session-win2.json")).unwrap(),
            secondary
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn imports_hosts_readonly_without_touching_source() {
        let dir = unique_dir("hosts");
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

    #[test]
    fn imports_zigshel_servers_txt_and_ssh_config_readonly() {
        let dir = unique_dir("servers-config");
        let ssh_dir = dir.join("ssh-key");
        std::fs::create_dir_all(&ssh_dir).unwrap();

        std::fs::write(
            ssh_dir.join("servers.txt"),
            concat!(
                "v1\n",
                "connected\t1\t2222\t1000\tvps1.example.com\tubuntu\thypertask\t\n",
                "removed\t1\t22\t1000\told.example.com\troot\told\tdeleted\n",
                "empty\t0\t22\t0\t\t\t\t\n",
                "failed\t1\t0\t1000\tbadport.example.com\tubuntu\tbad\t\n",
                "needs_setup\t1\t22\t0\tvps2.example.com\tvalentin\tbox2\t\n",
            ),
        )
        .unwrap();

        std::fs::write(
            ssh_dir.join("config"),
            concat!(
                "# comment\n",
                "Host *\n",
                "    ServerAliveInterval 60\n",
                "Host hypertask-dup\n",
                "    HostName vps1.example.com\n",
                "    User ubuntu\n",
                "    Port 2222\n",
                "Host extra-box\n",
                "    HostName vps3.example.com\n",
                "    User deploy\n",
                "    Port 2200\n",
            ),
        )
        .unwrap();

        let bundle = import_zigshel_bundle_from_dir(&dir);
        assert_eq!(bundle.hosts.len(), 3);
        assert_eq!(bundle.hosts[0].name, "hypertask");
        assert_eq!(bundle.hosts[0].port, 2222);
        assert_eq!(bundle.hosts[1].name, "box2");
        assert_eq!(bundle.hosts[2].name, "extra-box");
        assert_eq!(bundle.hosts[2].port, 2200);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn imports_legacy_session_json_workspaces_and_favorites_ignoring_nested_decoys() {
        let dir = unique_dir("decoys");
        let ssh_dir = dir.join("ssh-key");
        std::fs::create_dir_all(&ssh_dir).unwrap();
        std::fs::write(
            ssh_dir.join("servers.txt"),
            "v1\nconnected\t1\t22\t1000\tvps.example.com\tubuntu\tprod\t\n",
        )
        .unwrap();

        std::fs::write(
            dir.join("session.json"),
            r#"{
                "workspaces": [
                    {"name": "Main", "wid": "ws-main", "color": 0, "snoozed": false},
                    {"name": "Focus", "color": 2, "snoozed": true}
                ],
                "tabs": [
                    {
                        "name": "Real Favorite",
                        "favorite": true,
                        "csid": "sess-real",
                        "tmux_host": 0,
                        "claude_session": true,
                        "workspace": 0,
                        "extra": {
                            "favorite": true,
                            "csid": "decoy-nested"
                        }
                    },
                    {
                        "name": "Not Favorite With Decoy",
                        "favorite": false,
                        "csid": "sess-notfav",
                        "extra": {
                            "favorite": true,
                            "csid": "decoy-inside-nonfav"
                        }
                    }
                ],
                "unknown": {
                    "workspaces": [{"name": "DecoyWS", "wid": "decoy-ws"}],
                    "tabs": [{"name": "DecoyTab", "favorite": true, "csid": "decoy-root"}]
                }
            }"#,
        )
        .unwrap();

        std::fs::write(
            dir.join("windows.json"),
            r#"{
                "version": 1,
                "windows": [
                    {"id": "ws-main", "name": "Main", "restore": true, "archived": false},
                    {"id": "ws-arch", "name": "Legacy Archived", "restore": false, "archived": true}
                ]
            }"#,
        )
        .unwrap();

        let bundle = import_zigshel_bundle_from_dir(&dir);
        assert_eq!(bundle.workspaces.len(), 3);
        assert!(bundle.workspaces.iter().any(|w| w.id == "ws-main" && w.color.as_deref() == Some("#60A5FA")));
        assert!(bundle.workspaces.iter().any(|w| w.id == "focus" && w.snoozed && w.color.as_deref() == Some("#F59E0B")));
        assert!(bundle.workspaces.iter().any(|w| w.id == "ws-arch" && w.archived));
        assert!(!bundle.workspaces.iter().any(|w| w.id == "decoy-ws"));

        assert_eq!(bundle.favorites.len(), 1);
        assert_eq!(bundle.favorites[0].session_key, "prod:claude:sess-real");
        assert_eq!(bundle.favorites[0].workspace_id.as_deref(), Some("ws-main"));
        assert!(!bundle.favorite_keys.iter().any(|k| k.contains("decoy")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn imports_custom_faces_and_colors_round_trip() {
        let root = unique_dir("bot-faces");
        let legacy = root.join("ZigShell");
        std::fs::create_dir_all(&legacy).unwrap();
        let tabs: Vec<Value> = (0..20)
            .map(|face| serde_json::json!({
                "csid": format!("face-{face}"),
                "face": face,
                "color": if face % 2 == 0 { serde_json::json!(face) } else { serde_json::json!("#A1b2C3") },
            }))
            .collect();
        std::fs::write(
            legacy.join("session.json"),
            serde_json::json!({"tabs": tabs}).to_string(),
        )
        .unwrap();

        let bundle = import_zigshel_bundle_readonly(Some(&legacy));
        assert_eq!(bundle.sessions.len(), 20);
        let bundle: ZigShellImportBundle =
            serde_json::from_str(&serde_json::to_string(&bundle).unwrap()).unwrap();
        let mut store = WorkspaceStore::new();
        merge_zigshel_bundle(&mut store, &bundle);
        let path = root.join("Multibot").join("workspaces.json");
        store.save_to(&path).unwrap();
        let mut loaded = WorkspaceStore::load_from(&path).unwrap();
        for (face, session) in bundle.sessions.iter().enumerate() {
            let meta = &loaded.session_meta[&session.session_key];
            let expected_color = if face % 2 == 0 {
                ZIGSHEL_PALETTE_HEX[face % ZIGSHEL_PALETTE_HEX.len()]
            } else {
                "#A1b2C3"
            };
            assert_eq!(session.face, Some(face as u8));
            assert_eq!(session.color.as_deref(), Some(expected_color));
            assert_eq!(meta.face, session.face);
            assert_eq!(meta.color, session.color);
        }
        let before = loaded.clone();
        merge_zigshel_bundle(&mut loaded, &bundle);
        assert_eq!(loaded, before);

        let key = &bundle.sessions[0].session_key;
        let meta = loaded.session_meta.get_mut(key).unwrap();
        meta.face = Some(19);
        meta.color = Some("#123456".into());
        merge_zigshel_bundle(&mut loaded, &bundle);
        assert_eq!(loaded.session_meta[key].face, Some(19));
        assert_eq!(loaded.session_meta[key].color.as_deref(), Some("#123456"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn custom_face_import_ignores_invalid_missing_and_unknown_fields() {
        let dir = unique_dir("invalid-bot-faces");
        let invalid = [
            serde_json::json!(null),
            serde_json::json!(-1),
            serde_json::json!(256),
            serde_json::json!(1.5),
            serde_json::json!("invalid"),
            serde_json::json!(true),
            serde_json::json!({"face": 3}),
        ];
        let mut tabs: Vec<Value> = invalid.iter().enumerate().map(|(i, value)| {
            serde_json::json!({"csid": format!("bad-{i}"), "face": value, "color": value})
        }).collect();
        tabs.push(serde_json::json!({"csid": "missing", "unknown": {"face": 3, "color": 2}}));
        tabs.push(serde_json::json!({"csid": "too-large", "face": 20}));
        tabs.push(serde_json::json!({"csid": "bad-hex", "color": "#GG0000"}));
        std::fs::write(
            dir.join("session.json"),
            serde_json::json!({"tabs": tabs}).to_string(),
        )
        .unwrap();
        let bundle = import_zigshel_bundle_readonly(Some(&dir));
        assert_eq!(bundle.sessions.len(), tabs.len());
        let mut store = WorkspaceStore::new();
        merge_zigshel_bundle(&mut store, &bundle);
        for session in &bundle.sessions {
            assert_eq!(session.face, None);
            assert_eq!(session.color, None);
            assert_eq!(store.session_meta[&session.session_key].face, None);
            assert_eq!(store.session_meta[&session.session_key].color, None);
        }
        let old_session: ImportedSession =
            serde_json::from_str(r#"{"session_key":"local:claude:old","unknown":true}"#).unwrap();
        let old_meta: crate::workspaces::SessionMeta =
            serde_json::from_str(r#"{"pinned":true,"unknown":true}"#).unwrap();
        assert_eq!(old_session.face, None);
        assert_eq!(old_session.color, None);
        assert_eq!(old_meta.face, None);
        assert_eq!(old_meta.color, None);
        assert!(old_meta.pinned);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn zigshel_import_strictly_readonly_source_dir_immutable() {
        let dir = unique_dir("immutable");
        let ssh_dir = dir.join("ssh-key");
        std::fs::create_dir_all(&ssh_dir).unwrap();
        let servers = ssh_dir.join("servers.txt");
        let session = dir.join("session.json");
        std::fs::write(&servers, "v1\nconnected\t1\t22\t1\tvps.test\tu\tvps\t\n").unwrap();
        std::fs::write(
            &session,
            r#"{"workspaces":[{"name":"Alpha","wid":"alpha"}],"tabs":[{"name":"T","favorite":true,"csid":"c1","tmux_host":0,"face":0,"color":2}]}"#,
        )
        .unwrap();

        let snapshot = |root: &Path| -> Vec<(PathBuf, Vec<u8>, std::time::SystemTime)> {
            let mut items = Vec::new();
            for p in [root.join("session.json"), root.join("ssh-key").join("servers.txt")] {
                let bytes = std::fs::read(&p).unwrap();
                let mtime = std::fs::metadata(&p).unwrap().modified().unwrap();
                items.push((p, bytes, mtime));
            }
            items
        };

        let before_entries = std::fs::read_dir(&dir).unwrap().count();
        let before_ssh_entries = std::fs::read_dir(&ssh_dir).unwrap().count();
        let before = snapshot(&dir);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&servers, std::fs::Permissions::from_mode(0o444)).unwrap();
            std::fs::set_permissions(&session, std::fs::Permissions::from_mode(0o444)).unwrap();
            std::fs::set_permissions(&ssh_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        }

        let bundle = import_zigshel_bundle_from_dir(&dir);
        assert_eq!(bundle.hosts.len(), 1);
        assert_eq!(bundle.workspaces.len(), 1);
        assert_eq!(bundle.favorites.len(), 1);
        let mut store = WorkspaceStore::new();
        merge_zigshel_bundle(&mut store, &bundle);
        let meta = &store.session_meta[&bundle.sessions[0].session_key];
        assert_eq!(meta.face, Some(0));
        assert_eq!(meta.color.as_deref(), Some("#F59E0B"));

        let after_entries = std::fs::read_dir(&dir).unwrap().count();
        let after_ssh_entries = std::fs::read_dir(&ssh_dir).unwrap().count();
        let after = snapshot(&dir);

        assert_eq!(before_entries, after_entries);
        assert_eq!(before_ssh_entries, after_ssh_entries);
        assert_eq!(before, after);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755));
            let _ = std::fs::set_permissions(&ssh_dir, std::fs::Permissions::from_mode(0o755));
            let _ = std::fs::set_permissions(&servers, std::fs::Permissions::from_mode(0o644));
            let _ = std::fs::set_permissions(&session, std::fs::Permissions::from_mode(0o644));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_or_corrupt_zigshel_dir_returns_empty_bundle() {
        let missing = std::env::temp_dir().join("multibot-nonexistent-zigshel-dir-999999");
        let empty = import_zigshel_bundle_from_dir(&missing);
        assert!(empty.hosts.is_empty());
        assert!(empty.workspaces.is_empty());
        assert!(empty.favorites.is_empty());

        let dir = unique_dir("corrupt");
        std::fs::write(dir.join("broken.json"), "{not valid json").unwrap();
        std::fs::write(
            dir.join("good.json"),
            r#"{"workspaces":[{"name":"Good","wid":"good"}],"favorites":["local:claude:s1"]}"#,
        )
        .unwrap();

        let bundle = import_zigshel_bundle_from_dir(&dir);
        assert_eq!(bundle.workspaces.len(), 1);
        assert_eq!(bundle.workspaces[0].id, "good");
        assert_eq!(bundle.favorite_keys, vec!["local:claude:s1"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn repeated_import_respects_local_pin_choices_after_reload() {
        let key = "h:claude:chief-of-staff-session";
        let bundle = ZigShellImportBundle {
            sessions: vec![ImportedSession {
                session_key: key.into(),
                pinned: true,
                starred_pinned: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut store = WorkspaceStore::new();
        store.record_session_view(key, 100);
        merge_zigshel_bundle(&mut store, &bundle);
        assert!(store.is_pinned(key, false));
        assert!(store.is_pinned(key, true));
        assert!(!store.toggle_pin(key, false));
        let mut store: WorkspaceStore =
            serde_json::from_str(&serde_json::to_string(&store).unwrap()).unwrap();
        merge_zigshel_bundle(&mut store, &bundle);
        assert!(!store.is_pinned(key, false));
        assert!(store.is_pinned(key, true));
        assert!(!store.toggle_pin(key, true));
        merge_zigshel_bundle(&mut store, &bundle);
        assert!(!store.is_pinned(key, false));
        assert!(!store.is_pinned(key, true));
        assert!(store.toggle_pin(key, false));
        assert!(store.toggle_pin(key, true));
        merge_zigshel_bundle(&mut store, &bundle);
        assert!(store.is_pinned(key, false));
        assert!(store.is_pinned(key, true));
    }

    #[test]
    fn merge_zigshel_bundle_into_workspace_store_idempotent() {
        let mut store = WorkspaceStore::new();
        let existing = store.create_workspace("UserOwned", None).unwrap();

        let bundle = ZigShellImportBundle {
            hosts: Vec::new(),
            workspaces: vec![
                ImportedWorkspace {
                    id: "imported-a".into(),
                    name: "Imported A".into(),
                    color: Some("#60A5FA".into()),
                    archived: false,
                    snoozed: false,
                },
                ImportedWorkspace {
                    id: "userowned".into(),
                    name: "UserOwned".into(),
                    color: None,
                    archived: false,
                    snoozed: false,
                },
            ],
            favorites: vec![ImportedFavorite {
                session_key: "prod:claude:s1".into(),
                title: Some("One".into()),
                workspace_id: Some("imported-a".into()),
            }],
            favorite_keys: vec!["prod:claude:s1".into()],
            sessions: Vec::new(),
            source_files_read: Vec::new(),
        };

        let first = merge_zigshel_bundle(&mut store, &bundle);
        assert_eq!(first.workspaces_added, 1);
        assert_eq!(first.favorites_added, 1);
        assert!(store.workspace_by_id(&existing.id).is_some());
        assert!(store.workspace_by_id("imported-a").is_some());
        assert!(store.is_favorite("prod:claude:s1"));
        assert_eq!(
            store.session_meta["prod:claude:s1"].workspace_id.as_deref(),
            Some("imported-a")
        );

        let second = merge_zigshel_bundle(&mut store, &bundle);
        assert_eq!(second.workspaces_added, 0);
        assert_eq!(second.favorites_added, 0);
    }

    #[test]
    fn rejects_symlinks_and_enforces_16mib_cap() {
        let dir = unique_dir("symlink-cap");
        let outside = unique_dir("outside-secret");
        let outside_json = outside.join("secret.json");
        let outside_servers = outside.join("servers.txt");
        let outside_ssh_dir = outside.join("ssh-key-real");
        std::fs::create_dir_all(&outside_ssh_dir).unwrap();
        std::fs::write(
            &outside_json,
            r#"{"workspaces":[{"name":"SecretWS","wid":"secret-ws"}],"servers":[{"name":"secret","hostname":"secret.internal","user":"root","port":22}]}"#,
        )
        .unwrap();
        std::fs::write(
            &outside_servers,
            "v1\nconnected\t1\t22\t1000\tleaked.internal\troot\tleaked\t\n",
        )
        .unwrap();
        std::fs::write(
            outside_ssh_dir.join("servers.txt"),
            "v1\nconnected\t1\t22\t1000\tdir-escape.internal\troot\tescape\t\n",
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(&outside_json, dir.join("escape.json")).unwrap();
            let ssh_dir = dir.join("ssh-key");
            std::fs::create_dir_all(&ssh_dir).unwrap();
            symlink(&outside_servers, ssh_dir.join("servers.txt")).unwrap();

            let bundle = import_zigshel_bundle_from_dir(&dir);
            assert!(bundle.hosts.is_empty());
            assert!(bundle.workspaces.is_empty());

            // Also test symlinked ssh-key directory itself
            let dir2 = unique_dir("symlink-sshdir");
            symlink(&outside_ssh_dir, dir2.join("ssh-key")).unwrap();
            let bundle2 = import_zigshel_bundle_from_dir(&dir2);
            assert!(bundle2.hosts.is_empty());
            let _ = std::fs::remove_dir_all(&dir2);
        }

        // Test file size cap via read_regular_file_capped
        let big = dir.join("big.json");
        std::fs::write(&big, "x".repeat(256)).unwrap();
        assert!(read_regular_file_capped(&big, 128).is_none());
        assert!(read_regular_file_capped(&big, 256).is_some());

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn json_host_extraction_ignores_tabs_nested_decoys_and_zero_port_and_handles_uppercase_json_and_raw_favs() {
        let dir = unique_dir("strict-json");
        std::fs::write(
            dir.join("SESSION.JSON"),
            r#"{
                "servers": [
                    {"name": "RealBox", "hostname": "real.example.com", "user": "ops", "port": 0}
                ],
                "tabs": [
                    {
                        "name": "Chat About Deploying",
                        "favorite": true,
                        "csid": "s1",
                        "host": "prod",
                        "claude_session": true,
                        "extra": {
                            "name": "DecoyHost",
                            "host": "decoy.evil.com",
                            "port": 0
                        }
                    }
                ],
                "favorites": ["PROD:Claude:s1", "DEV:Codex:s2"],
                "unknown": {
                    "host": "nested-decoy.example.net",
                    "user": "attacker",
                    "port": 0
                }
            }"#,
        )
        .unwrap();

        let bundle = import_zigshel_bundle_from_dir(&dir);
        // Only the top-level "servers" entry is imported, with invalid port 0 defaulted to 22
        assert_eq!(bundle.hosts.len(), 1);
        assert_eq!(bundle.hosts[0].name, "RealBox");
        assert_eq!(bundle.hosts[0].hostname, "real.example.com");
        assert_eq!(bundle.hosts[0].port, 22);

        // Case-insensitive SESSION.JSON parsed & raw favorites canonicalized and deduplicated
        assert_eq!(
            bundle.favorite_keys,
            vec!["prod:claude:s1".to_string(), "dev:codex:s2".to_string()]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_unsafe_ssh_hostname_and_user_tokens_across_all_parsers() {
        let dir = unique_dir("unsafe-ssh-tokens");
        let ssh_dir = dir.join("ssh-key");
        std::fs::create_dir_all(&ssh_dir).unwrap();

        // 1. JSON host entries with newlines, tabs, CR, NUL, spaces, or leading '-'
        std::fs::write(
            dir.join("session.json"),
            concat!(
                r#"{"servers":["#,
                r#"{"name":"nl","hostname":"evil.com\nProxyCommand=id","user":"u","port":22},"#,
                r#"{"name":"cr","hostname":"evil.com\rProxyCommand=id","user":"u","port":22},"#,
                r#"{"name":"tab","hostname":"evil.com\tfoo","user":"u","port":22},"#,
                r#"{"name":"nul","hostname":"evil.com\u0000foo","user":"u","port":22},"#,
                r#"{"name":"space","hostname":"evil.com foo","user":"u","port":22},"#,
                r#"{"name":"opt","hostname":"-oProxyCommand=id","user":"u","port":22},"#,
                r#"{"name":"bad-user-opt","hostname":"good.example.com","user":"-oProxyCommand=id","port":22},"#,
                r#"{"name":"bad-user-nl","hostname":"good.example.com","user":"u\nProxyCommand=id","port":22},"#,
                r#"{"name":"bad-user-sp","hostname":"good.example.com","user":"bad user","port":22},"#,
                r#"{"name":"bad-user-nul","hostname":"good.example.com","user":"u\u0000x","port":22},"#,
                r#"{"name":"ok-json","hostname":"safe-json.example.com","user":"ops","port":2222}"#,
                r#"]}"#,
            ),
        )
        .unwrap();

        // 2. servers.txt entries with spaces, leading '-', control chars
        std::fs::write(
            ssh_dir.join("servers.txt"),
            concat!(
                "v1\n",
                "connected\t1\t22\t1000\t-oProxyCommand=id\tubuntu\topt\t\n",
                "connected\t1\t22\t1000\tevil.com foo\tubuntu\tsp\t\n",
                "connected\t1\t22\t1000\tevil.com\0bar\tubuntu\tnul\t\n",
                "connected\t1\t22\t1000\tevil.com\rbar\tubuntu\tcr\t\n",
                "connected\t1\t22\t1000\tgood.servers.com\t-oProxyCommand=id\tbadu\t\n",
                "connected\t1\t22\t1000\tgood.servers.com\tbad user\tbadu2\t\n",
                "connected\t1\t22\t1000\tgood.servers.com\tu\0x\tbadu3\t\n",
                "connected\t1\t2201\t1000\tsafe-txt.example.com\tdeploy\tok-txt\t\n",
            ),
        )
        .unwrap();

        // 3. ssh-key/config entries with spaces, leading '-', control chars
        std::fs::write(
            ssh_dir.join("config"),
            concat!(
                "Host bad-opt-host\n",
                "    HostName -oProxyCommand=id\n",
                "    User ubuntu\n",
                "Host bad-space-host\n",
                "    HostName evil.com ProxyCommand=id\n",
                "    User ubuntu\n",
                "Host bad-nul-host\n",
                "    HostName evil.com\0foo\n",
                "    User ubuntu\n",
                "Host bad-opt-user\n",
                "    HostName good.cfg.example.com\n",
                "    User -oProxyCommand=id\n",
                "Host bad-space-user\n",
                "    HostName good.cfg.example.com\n",
                "    User bad user\n",
                "Host ok-cfg\n",
                "    HostName safe-cfg.example.com\n",
                "    User admin\n",
                "    Port 2202\n",
            ),
        )
        .unwrap();

        let bundle = import_zigshel_bundle_from_dir(&dir);
        let hostnames: Vec<&str> = bundle.hosts.iter().map(|h| h.hostname.as_str()).collect();
        assert_eq!(
            hostnames,
            vec![
                "safe-txt.example.com",
                "safe-cfg.example.com",
                "safe-json.example.com",
            ]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_root_dir_with_trailing_slash() {
        use std::os::unix::fs::symlink;

        let real_target = unique_dir("real-target");
        std::fs::write(
            real_target.join("session.json"),
            r#"{"servers":[{"name":"Secret","hostname":"secret.example.com","user":"root","port":22}]}"#,
        )
        .unwrap();

        let parent = unique_dir("symlink-root-parent");
        let symlink_root = parent.join("symlink_zdir");
        symlink(&real_target, &symlink_root).unwrap();

        // Both without trailing slash and WITH trailing slash (and trailing /.) must be rejected
        assert!(import_zigshel_bundle_from_dir(&symlink_root).hosts.is_empty());

        let with_trailing_slash = PathBuf::from(format!("{}/", symlink_root.display()));
        assert!(
            import_zigshel_bundle_from_dir(&with_trailing_slash)
                .hosts
                .is_empty()
        );

        let with_join_empty = symlink_root.join("");
        assert!(
            import_zigshel_bundle_from_dir(&with_join_empty)
                .hosts
                .is_empty()
        );

        let with_dot = symlink_root.join(".");
        assert!(import_zigshel_bundle_from_dir(&with_dot).hosts.is_empty());

        let _ = std::fs::remove_dir_all(&parent);
        let _ = std::fs::remove_dir_all(&real_target);
    }

    #[test]
    fn parses_zigshel_identity_file_quoted_relative_windows_tilde_equals_and_first_valid_wins() {
        let root = unique_dir("identity-file-config");
        let ssh_dir = root.join("ssh-key");
        std::fs::create_dir_all(&ssh_dir).unwrap();

        let cfg = concat!(
            "Host win-quoted\r\n",
            "  HostName 10.1.0.1\r\n",
            "  User ubuntu\r\n",
            "  IdentityFile \"C:\\Users\\Valentin Yeo\\AppData\\Roaming\\ZigShell\\ssh-key\\id_ed25519\" # comment\r\n",
            "\r\n",
            "Host win-fwd\r\n",
            "  HostName 10.1.0.2\r\n",
            "  User root\r\n",
            "  IdentityFile \"C:/Users/Example User/AppData/Roaming/ZigShell/ssh-key/multibot_ed25519\"\r\n",
            "\r\n",
            "Host unc-host\r\n",
            "  HostName 10.1.0.3\r\n",
            "  User ops\r\n",
            "  IdentityFile \"\\\\fileserver\\keys\\id_ed25519\"\r\n",
            "\r\n",
            "Host tilde-host\r\n",
            "  HostName 10.1.0.4\r\n",
            "  User dev\r\n",
            "\tIdentityFile=~/.ssh/custom_ed25519\r\n",
            "\r\n",
            "Host rel-host\r\n",
            "  HostName 10.1.0.5\r\n",
            "  User dev\r\n",
            "  IdentityFile id_ed25519\r\n",
            "\r\n",
            "Host multi-key\r\n",
            "  HostName 10.1.0.6\r\n",
            "  User dev\r\n",
            "  IdentityFile first_key\r\n",
            "  IdentityFile second_key\r\n",
            "\r\n",
            "Host *\r\n",
            "  IdentityFile wildcard_key\r\n",
            "\r\n",
            "Host no-key\r\n",
            "  HostName 10.1.0.7\r\n",
            "  User dev\r\n",
        );
        std::fs::write(ssh_dir.join("config"), cfg).unwrap();

        let bundle = import_zigshel_bundle_from_dir(&root);
        let by_name = |n: &str| {
            bundle
                .hosts
                .iter()
                .find(|h| h.name == n)
                .unwrap_or_else(|| panic!("missing host {n}"))
        };

        assert_eq!(
            by_name("win-quoted").identity_file.as_deref(),
            Some(r"C:\Users\Valentin Yeo\AppData\Roaming\ZigShell\ssh-key\id_ed25519")
        );
        assert_eq!(
            by_name("win-fwd").identity_file.as_deref(),
            Some("C:/Users/Example User/AppData/Roaming/ZigShell/ssh-key/multibot_ed25519")
        );
        assert_eq!(
            by_name("unc-host").identity_file.as_deref(),
            Some(r"\\fileserver\keys\id_ed25519")
        );
        assert_eq!(
            by_name("tilde-host").identity_file.as_deref(),
            Some("~/.ssh/custom_ed25519")
        );
        assert_eq!(
            by_name("rel-host").identity_file.as_deref(),
            Some(ssh_dir.join("id_ed25519").to_string_lossy().as_ref())
        );
        assert_eq!(
            by_name("multi-key").identity_file.as_deref(),
            Some(ssh_dir.join("first_key").to_string_lossy().as_ref())
        );
        assert_eq!(by_name("no-key").identity_file, None);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dedup_hosts_backfills_identity_file_from_ssh_config_onto_servers_txt() {
        let root = unique_dir("identity-file-dedup");
        let ssh_dir = root.join("ssh-key");
        std::fs::create_dir_all(&ssh_dir).unwrap();

        let servers_txt = "v1\nready\t0\t2222\t0\tvps1.example.com\tubuntu\thypertask\n";
        let ssh_config = concat!(
            "Host remote-1\r\n",
            "  HostName vps1.example.com\r\n",
            "  User ubuntu\r\n",
            "  Port 2222\r\n",
            "  IdentityFile \"C:/Users/Val/ZigShell/ssh-key/multibot_ed25519\"\r\n",
        );
        std::fs::write(ssh_dir.join("servers.txt"), servers_txt).unwrap();
        std::fs::write(ssh_dir.join("config"), ssh_config).unwrap();

        let bundle = import_zigshel_bundle_from_dir(&root);
        assert_eq!(bundle.hosts.len(), 1);
        assert_eq!(bundle.hosts[0].name, "hypertask");
        assert_eq!(bundle.hosts[0].port, 2222);
        assert_eq!(
            bundle.hosts[0].identity_file.as_deref(),
            Some("C:/Users/Val/ZigShell/ssh-key/multibot_ed25519")
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn parses_json_identity_file_and_identity_file_camel_case_and_rejects_injections() {
        let root = unique_dir("identity-file-json-inject");
        let ssh_dir = root.join("ssh-key");
        std::fs::create_dir_all(&ssh_dir).unwrap();

        let ssh_cfg = concat!(
            "Host bad-flag\n",
            "  HostName 10.2.0.1\n",
            "  User ubuntu\n",
            "  IdentityFile -oProxyCommand=evil\n",
            "Host bad-quote\n",
            "  HostName 10.2.0.2\n",
            "  User ubuntu\n",
            "  IdentityFile \"unterminated\n",
            "Host bad-tab\n",
            "  HostName 10.2.0.3\n",
            "  User ubuntu\n",
            "  IdentityFile \"/tmp/key\tbad\"\n",
            "Host bad-nul\n",
            "  HostName 10.2.0.4\n",
            "  User ubuntu\n",
            "  IdentityFile /tmp/key\0bad\n",
        );
        std::fs::write(ssh_dir.join("config"), ssh_cfg).unwrap();

        let json = r#"{
            "servers": [
                {"name": "json-snake", "hostname": "10.3.0.1", "user": "u", "port": 22, "identity_file": "keys/my_ed25519"},
                {"name": "json-camel", "hostname": "10.3.0.2", "user": "u", "port": 22, "identityFile": "/home/u/.ssh/id_ed25519"},
                {"name": "json-bad-flag", "hostname": "10.3.0.3", "user": "u", "port": 22, "identity_file": "-oProxyCommand=id"},
                {"name": "json-bad-nl", "hostname": "10.3.0.4", "user": "u", "port": 22, "identity_file": "/tmp/k\nProxyCommand=id"},
                {"name": "json-bad-nul", "hostname": "10.3.0.5", "user": "u", "port": 22, "identity_file": "/tmp/k\u0000x"},
                {"name": "json-empty", "hostname": "10.3.0.6", "user": "u", "port": 22, "identity_file": ""}
            ]
        }"#;
        std::fs::write(root.join("hosts.json"), json).unwrap();

        let bundle = import_zigshel_bundle_from_dir(&root);
        let by_name = |n: &str| {
            bundle
                .hosts
                .iter()
                .find(|h| h.name == n)
                .unwrap_or_else(|| panic!("missing host {n}"))
        };

        assert_eq!(by_name("bad-flag").identity_file, None);
        assert_eq!(by_name("bad-quote").identity_file, None);
        assert_eq!(by_name("bad-tab").identity_file, None);
        assert_eq!(by_name("bad-nul").identity_file, None);

        assert_eq!(
            by_name("json-snake").identity_file.as_deref(),
            Some(root.join("keys/my_ed25519").to_string_lossy().as_ref())
        );
        assert_eq!(
            by_name("json-camel").identity_file.as_deref(),
            Some("/home/u/.ssh/id_ed25519")
        );
        assert_eq!(by_name("json-bad-flag").identity_file, None);
        assert_eq!(by_name("json-bad-nl").identity_file, None);
        assert_eq!(by_name("json-bad-nul").identity_file, None);
        assert_eq!(by_name("json-empty").identity_file, None);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn zigshel_identity_file_import_is_strictly_readonly() {
        use std::os::unix::fs::PermissionsExt;

        let root = unique_dir("identity-file-readonly");
        let ssh_dir = root.join("ssh-key");
        std::fs::create_dir_all(&ssh_dir).unwrap();
        let cfg_path = ssh_dir.join("config");
        let key_path = ssh_dir.join("id_ed25519");
        std::fs::write(
            &cfg_path,
            "Host prod\n  HostName prod.example.com\n  User ubuntu\n  IdentityFile id_ed25519\n",
        )
        .unwrap();
        std::fs::write(&key_path, "DUMMY-KEY-NEVER-MODIFIED").unwrap();

        let cfg_mtime = std::fs::metadata(&cfg_path).unwrap().modified().unwrap();
        let key_mtime = std::fs::metadata(&key_path).unwrap().modified().unwrap();

        std::fs::set_permissions(&cfg_path, std::fs::Permissions::from_mode(0o444)).unwrap();
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o444)).unwrap();
        std::fs::set_permissions(&ssh_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o555)).unwrap();

        let bundle = import_zigshel_bundle_from_dir(&root);
        assert_eq!(bundle.hosts.len(), 1);
        assert_eq!(
            bundle.hosts[0].identity_file.as_deref(),
            Some(key_path.to_string_lossy().as_ref())
        );

        assert_eq!(
            std::fs::metadata(&cfg_path).unwrap().modified().unwrap(),
            cfg_mtime
        );
        assert_eq!(
            std::fs::metadata(&key_path).unwrap().modified().unwrap(),
            key_mtime
        );
        assert_eq!(std::fs::read_dir(&ssh_dir).unwrap().count(), 2);

        // Restore write permissions so temp cleanup succeeds cleanly.
        let _ = std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::set_permissions(&ssh_dir, std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::set_permissions(&cfg_path, std::fs::Permissions::from_mode(0o644));
        let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o644));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ssh_config_match_stanza_does_not_bleed_into_preceding_or_following_host() {
        let root = unique_dir("ssh-match-stanza-boundary");
        let ssh_dir = root.join("ssh-key");
        std::fs::create_dir_all(&ssh_dir).unwrap();
        std::fs::write(
            ssh_dir.join("config"),
            "\
Host before-match
  HostName 10.10.0.7
  User safeuser
  Port 22
Match host other-box
  HostName 10.99.99.99
  User eviluser
  Port 9999
  IdentityFile /tmp/match_bleed_key
Match all
  IdentityFile /tmp/match_all_key
Host after-match
  HostName 10.10.0.8
  User cleanuser
",
        )
        .unwrap();

        let bundle = import_zigshel_bundle_from_dir(&root);
        assert_eq!(bundle.hosts.len(), 2);

        let before = bundle
            .hosts
            .iter()
            .find(|h| h.name == "before-match")
            .expect("before-match host");
        assert_eq!(before.hostname, "10.10.0.7");
        assert_eq!(before.user, "safeuser");
        assert_eq!(before.port, 22);
        assert_eq!(before.identity_file, None);

        let after = bundle
            .hosts
            .iter()
            .find(|h| h.name == "after-match")
            .expect("after-match host");
        assert_eq!(after.hostname, "10.10.0.8");
        assert_eq!(after.user, "cleanuser");
        assert_eq!(after.port, 22);
        assert_eq!(after.identity_file, None);

        let _ = std::fs::remove_dir_all(&root);
    }
}
