//! Read-only reader for the legacy `multiprompt` server-side workspace assignment.
//!
//! The legacy stack keeps two state files under `~/.local/state/multiprompt/`:
//!
//! * `chat-names.json` maps a tmux session key (`live-xxxxxxx`) to a display name, the
//!   provider session id (`sid`), an Antigravity conversation id (`conversation`) and the
//!   workspace the chat belongs to (`workspace`).
//! * `agent-host/<hash>.config.json` and `agent-host/<hash>.state.json` map that same tmux
//!   key to the ids the agent actually wrote its transcript under. A Claude chat records its
//!   transcript under `sid`; a Codex chat records it under `state.json`'s `native_session`
//!   (the rollout id), because `sid` stays the Claude provider id after a provider switch.
//!
//! Building an index over both is what lets a live transcript id resolve to the workspace a
//! chat really belongs to. Everything here is strictly read-only: no file in the legacy state
//! directory is ever created, modified or deleted, and every read is size-capped.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// Cap for a single `chat-names.json` read (the live file is ~14 KiB).
const MAX_CHAT_NAMES_BYTES: u64 = 4 * 1024 * 1024;
/// Cap for one `agent-host/*.config.json` read (~450 bytes on disk).
const MAX_CONFIG_BYTES: u64 = 256 * 1024;
/// Cap for one `agent-host/*.state.json` read (largest observed ~180 KiB).
const MAX_STATE_BYTES: u64 = 2 * 1024 * 1024;
/// Cap on the per-tmux `written` map so a stale state file cannot blow the memory budget.
const MAX_WRITTEN_KEYS: usize = 32;
/// Minimum length before a bare id is treated as a session id worth indexing.
const MIN_INDEXED_ID_CHARS: usize = 8;

/// Workspace name length cap, matching `MAX_WORKSPACE_NAME_CHARS`.
const MAX_WORKSPACE_NAME_CHARS: usize = 63;

/// Resolves transcript session ids to the workspace names recorded by the legacy server.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerWorkspaceIndex {
    assignments: HashMap<String, String>,
}

/// Cached index together with the state stamp it was built from.
type CachedIndex = (StateStamp, Arc<ServerWorkspaceIndex>);

impl ServerWorkspaceIndex {
    /// Workspace name recorded for the chat that produced `session_id`, if any.
    pub fn workspace_for(&self, session_id: &str) -> Option<&str> {
        self.assignments.get(session_id).map(String::as_str)
    }

    /// Builds the index from an explicit legacy state directory (used by tests).
    pub fn load_from_state_dir(state_dir: &Path) -> Self {
        let mut index = Self::default();
        let chat_names = read_chat_names(&state_dir.join("chat-names.json"));
        for (session_id, workspace) in &chat_names.by_session_id {
            index.insert(session_id, workspace);
        }
        index.add_agent_host(&state_dir.join("agent-host"), &chat_names.by_tmux);
        index
    }

    /// Builds the index from `~/.local/state/multiprompt` on this host.
    ///
    /// Resolution order follows the legacy app: `MULTIPROMPT_STATE_DIR` (tests and alternate
    /// installs), then `XDG_STATE_HOME/multiprompt`, then `~/.local/state/multiprompt`.
    pub fn load_default() -> Self {
        match default_state_dir() {
            Some(dir) => Self::load_from_state_dir(&dir),
            None => Self::default(),
        }
    }

    /// [`Self::load_default`] with a process-wide cache keyed on the mtimes of the two things
    /// that change when a chat moves or switches provider: `chat-names.json` and `agent-host/`.
    ///
    /// Without the cache the daemon would re-read ~190 state files on every transcript append.
    /// The stamp is cheap (two `stat` calls) and a miss costs one full rebuild.
    pub fn load_default_cached() -> Arc<Self> {
        static CACHE: OnceLock<Mutex<Option<CachedIndex>>> = OnceLock::new();
        let cache = CACHE.get_or_init(|| Mutex::new(None));
        let stamp = StateStamp::capture(default_state_dir().as_deref());
        let mut guard = cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((cached, index)) = guard.as_ref()
            && *cached == stamp
        {
            return Arc::clone(index);
        }
        let index = Arc::new(Self::load_default());
        *guard = Some((stamp, Arc::clone(&index)));
        index
    }

    fn add_agent_host(&mut self, dir: &Path, by_tmux: &HashMap<String, String>) {
        if !is_regular_non_symlink_dir(dir) {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        // Config first (`tmux` -> provider id), then state (`tmux` -> native transcript id).
        let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        paths.sort_by(|a, b| {
            let rank = |p: &Path| match p.file_name().and_then(|n| n.to_str()) {
                Some(name) if name.ends_with(".config.json") => 0u8,
                Some(name) if name.ends_with(".state.json") => 1,
                _ => 2,
            };
            (rank(a), a.to_path_buf()).cmp(&(rank(b), b.to_path_buf()))
        });

        // tmux key -> ids the agent wrote its transcript under.
        let mut tmux_ids: HashMap<String, Vec<String>> = HashMap::new();
        for path in paths {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            let (cap, is_state) = if name.ends_with(".config.json") {
                (MAX_CONFIG_BYTES, false)
            } else if name.ends_with(".state.json") {
                (MAX_STATE_BYTES, true)
            } else {
                continue;
            };
            let Some(raw) = read_regular_file_capped(&path, cap) else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
                continue;
            };
            let Some(obj) = value.as_object() else {
                continue;
            };
            let Some(tmux) = obj
                .get("tmux")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let ids = tmux_ids.entry(tmux.to_string()).or_default();
            if is_state {
                // The rollout id a Codex chat actually writes its transcript under.
                push_id(ids, obj.get("native_session"));
            } else {
                push_id(ids, obj.get("sid"));
                push_id(ids, obj.get("native_session"));
                if let Some(switched) = obj.get("switched_from").and_then(|v| v.as_object()) {
                    push_id(ids, switched.get("sid"));
                    push_id(ids, switched.get("native_session"));
                }
                if let Some(switched) = obj.get("switched_to").and_then(|v| v.as_object()) {
                    push_id(ids, switched.get("sid"));
                    push_id(ids, switched.get("native_session"));
                }
            }
        }

        for (tmux, workspace) in by_tmux {
            let Some(ids) = tmux_ids.get(tmux) else {
                continue;
            };
            for id in ids {
                self.insert(id, workspace);
            }
        }
    }

    fn insert(&mut self, id: &str, workspace: &str) {
        let id = id.trim();
        if id.chars().count() < MIN_INDEXED_ID_CHARS {
            return;
        }
        self.assignments
            .entry(id.to_string())
            .or_insert_with(|| workspace.to_string());
    }
}

/// Identity of the legacy state the index was built from.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StateStamp {
    chat_names: Option<(u64, i64)>,
    agent_host: Option<(usize, i64)>,
}

impl StateStamp {
    fn capture(state_dir: Option<&Path>) -> Self {
        let chat_names = state_dir
            .map(|dir| dir.join("chat-names.json"))
            .and_then(|path| file_stamp(&path));
        let agent_host = state_dir
            .map(|dir| dir.join("agent-host"))
            .and_then(|path| dir_stamp(&path));
        Self {
            chat_names,
            agent_host,
        }
    }
}

fn file_stamp(path: &Path) -> Option<(u64, i64)> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.file_type().is_file() || meta.file_type().is_symlink() {
        return None;
    }
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Some((meta.len(), mtime))
}

fn dir_stamp(path: &Path) -> Option<(usize, i64)> {
    if !is_regular_non_symlink_dir(path) {
        return None;
    }
    let mut count = 0usize;
    let mut newest = 0i64;
    for entry in std::fs::read_dir(path).ok()?.flatten() {
        let meta = match entry.metadata() {
            Ok(meta) if meta.is_file() => meta,
            _ => continue,
        };
        count += 1;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        newest = newest.max(mtime);
    }
    Some((count, newest))
}

/// Workspace names recorded in `chat-names.json`, keyed the two ways the file records them.
#[derive(Debug, Default)]
struct ChatNames {
    /// `sid` / `conversation` -> workspace name.
    by_session_id: HashMap<String, String>,
    /// tmux session key (`live-xxxxxxx`) -> workspace name.
    by_tmux: HashMap<String, String>,
}

fn read_chat_names(path: &Path) -> ChatNames {
    let mut out = ChatNames::default();
    let Some(raw) = read_regular_file_capped(path, MAX_CHAT_NAMES_BYTES) else {
        return out;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return out;
    };
    let Some(root) = value.as_object() else {
        return out;
    };
    for (key, entry) in root {
        let Some(obj) = entry.as_object() else {
            continue;
        };
        let Some(workspace) = obj
            .get("workspace")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let workspace = truncate_chars(workspace, MAX_WORKSPACE_NAME_CHARS);
        if key.starts_with("live-") {
            out.by_tmux.insert(key.clone(), workspace.clone());
        }
        for field in ["sid", "conversation"] {
            let Some(id) = obj
                .get(field)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| s.chars().count() >= MIN_INDEXED_ID_CHARS)
            else {
                continue;
            };
            out.by_session_id
                .entry(id.to_string())
                .or_insert_with(|| workspace.clone());
        }
    }
    out
}

fn push_id(ids: &mut Vec<String>, value: Option<&serde_json::Value>) {
    if ids.len() >= MAX_WRITTEN_KEYS {
        return;
    }
    let Some(id) = value.and_then(serde_json::Value::as_str).map(str::trim) else {
        return;
    };
    if id.chars().count() < MIN_INDEXED_ID_CHARS {
        return;
    }
    if !ids.iter().any(|existing| existing == id) {
        ids.push(id.to_string());
    }
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

fn default_state_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("MULTIPROMPT_STATE_DIR") {
        let path = PathBuf::from(dir);
        if path.is_dir() {
            return Some(path);
        }
    }
    if let Some(base) = std::env::var_os("XDG_STATE_HOME") {
        let path = PathBuf::from(base).join("multiprompt");
        if path.is_dir() {
            return Some(path);
        }
    }
    let home = std::env::var_os("HOME")?;
    let path = PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("multiprompt");
    path.is_dir().then_some(path)
}

fn is_regular_non_symlink_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_dir() && !m.file_type().is_symlink())
        .unwrap_or(false)
}

fn read_regular_file_capped(path: &Path, max_bytes: u64) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    let ft = meta.file_type();
    if !ft.is_file() || ft.is_symlink() || meta.len() > max_bytes {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(1);

    fn temp_state_dir(label: &str) -> PathBuf {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "multibot-server-ws-{label}-{}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(dir.join("agent-host")).unwrap();
        dir
    }

    fn write(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn chat_names_sid_and_conversation_ids_resolve_to_their_workspace() {
        let dir = temp_state_dir("chat-names");
        write(
            &dir.join("chat-names.json"),
            r#"{
                "live-1d21840": {
                    "name": "Matt Live Stream",
                    "sid": "d5339a17-2bfb-4710-a297-4c7548e0e636",
                    "workspace": "Yeo UX",
                    "conversation": "c6d22661-f4bc-4d07-a861-4e1410f2f227"
                },
                "live-c2a89c7": {
                    "name": "HT Product Lead",
                    "sid": "05825187-f4b7-4fec-84c9-16726b3b5258",
                    "workspace": "HT Product"
                },
                "live-8bc7bac": { "name": "Poteto", "sid": "6f353a80-d3df-4e44-88aa-edcc6c2b8999" },
                "@agents": { "live-8bc7bac": "2f26726a-ab85-4e39-954c-1f1634227bb6" }
            }"#,
        );

        let index = ServerWorkspaceIndex::load_from_state_dir(&dir);
        assert_eq!(
            index.workspace_for("d5339a17-2bfb-4710-a297-4c7548e0e636"),
            Some("Yeo UX")
        );
        assert_eq!(
            index.workspace_for("c6d22661-f4bc-4d07-a861-4e1410f2f227"),
            Some("Yeo UX")
        );
        assert_eq!(
            index.workspace_for("05825187-f4b7-4fec-84c9-16726b3b5258"),
            Some("HT Product")
        );
        // A chat with no recorded workspace stays unresolved instead of landing in a bucket.
        assert_eq!(index.workspace_for("6f353a80-d3df-4e44-88aa-edcc6c2b8999"), None);
        // The `@agents` tmux -> agent-id map is not a chat, so it must not create assignments.
        assert_eq!(index.workspace_for("2f26726a-ab85-4e39-954c-1f1634227bb6"), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn codex_rollout_id_resolves_through_agent_host_native_session() {
        let dir = temp_state_dir("codex");
        write(
            &dir.join("chat-names.json"),
            r#"{"live-2cf45c23": {"sid": "1d02d00b-a50b-4529-9039-eedafc77bb7f", "workspace": "Vstack"}}"#,
        );
        write(
            &dir.join("agent-host/e2f2f8d3b2de2c172927d3beef065f6d.config.json"),
            r#"{"provider": "codex", "sid": "1d02d00b-a50b-4529-9039-eedafc77bb7f", "tmux": "live-2cf45c23"}"#,
        );
        write(
            &dir.join("agent-host/e2f2f8d3b2de2c172927d3beef065f6d.state.json"),
            r#"{"sid": "1d02d00b-a50b-4529-9039-eedafc77bb7f", "native_session": "01a213db-1d2a-7783-8245-01b6e818b48a", "tmux": "live-2cf45c23"}"#,
        );

        let index = ServerWorkspaceIndex::load_from_state_dir(&dir);
        // A Codex transcript is written under the rollout id, not the provider sid.
        assert_eq!(
            index.workspace_for("01a213db-1d2a-7783-8245-01b6e818b48a"),
            Some("Vstack")
        );
        assert_eq!(
            index.workspace_for("1d02d00b-a50b-4529-9039-eedafc77bb7f"),
            Some("Vstack")
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn provider_switch_ids_still_resolve_to_the_chat_workspace() {
        let dir = temp_state_dir("switch");
        write(
            &dir.join("chat-names.json"),
            r#"{"live-4b135e3": {"sid": "9738e6e1-a533-4db7-823d-3e04af268ccb", "workspace": "Multiprompt"}}"#,
        );
        write(
            &dir.join("agent-host/02820df23e7de670a1ffa5efa2db9046.config.json"),
            r#"{"provider": "codex", "sid": "9738e6e1-a533-4db7-823d-3e04af268ccb", "tmux": "live-4b135e3", "switched_from": {"provider": "claude", "sid": "2a7b79b2-130e-4e53-a68d-2bc0e3cdac0c"}}"#,
        );

        let index = ServerWorkspaceIndex::load_from_state_dir(&dir);
        assert_eq!(
            index.workspace_for("9738e6e1-a533-4db7-823d-3e04af268ccb"),
            Some("Multiprompt")
        );
        // The pre-switch Claude transcript belongs to the same chat, so the same workspace.
        assert_eq!(
            index.workspace_for("2a7b79b2-130e-4e53-a68d-2bc0e3cdac0c"),
            Some("Multiprompt")
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_corrupt_or_symlinked_state_yields_an_empty_index() {
        let missing = PathBuf::from("/nonexistent/multibot-server-ws-state");
        assert_eq!(ServerWorkspaceIndex::load_from_state_dir(&missing), ServerWorkspaceIndex::default());

        let dir = temp_state_dir("corrupt");
        write(&dir.join("chat-names.json"), "{not json");
        write(
            &dir.join("agent-host/broken.state.json"),
            r#"{"tmux": "live-1", "native_session": "01a213db-1d2a-7783-8245-01b6e818b48a"}"#,
        );
        assert_eq!(
            ServerWorkspaceIndex::load_from_state_dir(&dir),
            ServerWorkspaceIndex::default()
        );

        // A symlinked `agent-host` must not be followed.
        let real = dir.join("real-agent-host");
        fs::create_dir_all(&real).unwrap();
        fs::remove_dir_all(dir.join("agent-host")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, dir.join("agent-host")).unwrap();
        assert_eq!(
            ServerWorkspaceIndex::load_from_state_dir(&dir),
            ServerWorkspaceIndex::default()
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn short_ids_are_ignored_and_workspace_names_are_capped() {
        let dir = temp_state_dir("short");
        let long_name = "🚀Kraków".repeat(20);
        write(
            &dir.join("chat-names.json"),
            &format!(
                r#"{{"live-1": {{"sid": "short", "workspace": "{long_name}"}}, "live-2": {{"sid": "long-enough-id", "workspace": "  {long_name}  "}}}}"#
            ),
        );
        let index = ServerWorkspaceIndex::load_from_state_dir(&dir);
        assert_eq!(index.workspace_for("short"), None);
        let resolved = index.workspace_for("long-enough-id").unwrap();
        assert_eq!(resolved.chars().count(), MAX_WORKSPACE_NAME_CHARS);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn default_state_dir_honours_the_override_env_var() {
        let dir = temp_state_dir("override");
        write(
            &dir.join("chat-names.json"),
            r#"{"live-1": {"sid": "long-enough-id", "workspace": "Yeo UX"}}"#,
        );
        // Safety: this test owns the variable and restores it before returning.
        unsafe { std::env::set_var("MULTIPROMPT_STATE_DIR", &dir) };
        let index = ServerWorkspaceIndex::load_default();
        unsafe { std::env::remove_var("MULTIPROMPT_STATE_DIR") };
        assert_eq!(index.workspace_for("long-enough-id"), Some("Yeo UX"));
        fs::remove_dir_all(&dir).unwrap();
    }
}
