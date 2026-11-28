use crate::settings::MultibotSettings;
use multibot_proto::{AgentKind, ServerEvent, SessionOptions};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub const OUTBOX_SCHEMA_VERSION: u32 = 1;
pub const MAX_OUTBOX_PER_SESSION: usize = 20;
pub const MAX_OUTBOX_FILE_BYTES: u64 = 16 * 1024 * 1024;

static OUTBOX_ID_COUNTER: AtomicU64 = AtomicU64::new(1);
static OUTBOX_TMP_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboxError {
    QueueFull { session_key: String, max: usize },
    EmptyEntryId,
    DuplicateEntryId { id: String },
    EmptySessionKey,
    EmptyText,
    Io(String),
    Serialization(String),
}

impl fmt::Display for OutboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QueueFull { session_key, max } => write!(
                f,
                "Outbox queue full for session '{session_key}' (max {max} queued messages)"
            ),
            Self::EmptyEntryId => write!(f, "Outbox entry id must not be empty"),
            Self::DuplicateEntryId { id } => write!(f, "Outbox entry id '{id}' already exists"),
            Self::EmptySessionKey => write!(f, "Outbox session key must not be empty"),
            Self::EmptyText => write!(f, "Outbox message text must not be empty"),
            Self::Io(msg) => write!(f, "Outbox I/O error: {msg}"),
            Self::Serialization(msg) => write!(f, "Outbox serialization error: {msg}"),
        }
    }
}

impl std::error::Error for OutboxError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxEntry {
    pub id: String,
    #[serde(alias = "session_id")]
    pub session_key: String,
    pub text: String,
    pub created_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<SessionOptions>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxFile {
    pub schema_version: u32,
    #[serde(default)]
    pub entries: Vec<OutboxEntry>,
}

impl Default for OutboxFile {
    fn default() -> Self {
        Self {
            schema_version: OUTBOX_SCHEMA_VERSION,
            entries: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ChatOutbox {
    #[serde(default)]
    pub entries: Vec<OutboxEntry>,
    #[serde(skip)]
    pub dispatching: HashMap<String, OutboxDispatch>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxDispatch {
    pub entry_id: String,
    pub seen_working: bool,
}

impl ChatOutbox {
    /// Builds a collision-free length-prefixed per-session key:
    /// `"{host_len}:{host}|{agent}|{sid_len}:{session_id}"`.
    pub fn session_key(host: &str, agent: AgentKind, session_id: &str) -> String {
        let agent_str = match agent {
            AgentKind::Claude => "claude",
            AgentKind::Antigravity => "antigravity",
            AgentKind::Codex => "codex",
            AgentKind::Shell => "shell",
            AgentKind::Pi => "pi",
            AgentKind::Cursor => "cursor",
        };
        format!(
            "{}:{host}|{agent_str}|{}:{session_id}",
            host.len(),
            session_id.len()
        )
    }

    pub fn enqueue(
        &mut self,
        session_key: impl Into<String>,
        text: impl Into<String>,
        created_at_ms: i64,
    ) -> Result<OutboxEntry, OutboxError> {
        self.enqueue_with_options(session_key, text, created_at_ms, None)
    }

    pub fn enqueue_with_options(
        &mut self,
        session_key: impl Into<String>,
        text: impl Into<String>,
        created_at_ms: i64,
        options: Option<SessionOptions>,
    ) -> Result<OutboxEntry, OutboxError> {
        let session_key = session_key.into();
        let text = text.into();
        if session_key.trim().is_empty() {
            return Err(OutboxError::EmptySessionKey);
        }
        if text.trim().is_empty() {
            return Err(OutboxError::EmptyText);
        }
        if self.count_for_session(&session_key) >= MAX_OUTBOX_PER_SESSION {
            return Err(OutboxError::QueueFull {
                session_key,
                max: MAX_OUTBOX_PER_SESSION,
            });
        }
        let id = loop {
            let seq = OUTBOX_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
            let candidate = format!("outbox-{created_at_ms}-{seq}");
            if !self.entries.iter().any(|e| e.id == candidate) {
                break candidate;
            }
        };
        let entry = OutboxEntry {
            id,
            session_key,
            text,
            created_at_ms,
            options,
        };
        self.entries.push(entry.clone());
        Ok(entry)
    }

    pub fn enqueue_entry(&mut self, entry: OutboxEntry) -> Result<(), OutboxError> {
        if entry.id.trim().is_empty() {
            return Err(OutboxError::EmptyEntryId);
        }
        if entry.session_key.trim().is_empty() {
            return Err(OutboxError::EmptySessionKey);
        }
        if entry.text.trim().is_empty() {
            return Err(OutboxError::EmptyText);
        }
        if self.entries.iter().any(|e| e.id == entry.id) {
            return Err(OutboxError::DuplicateEntryId { id: entry.id });
        }
        if self.count_for_session(&entry.session_key) >= MAX_OUTBOX_PER_SESSION {
            return Err(OutboxError::QueueFull {
                session_key: entry.session_key,
                max: MAX_OUTBOX_PER_SESSION,
            });
        }
        self.entries.push(entry);
        Ok(())
    }

    pub fn mark_dispatched(&mut self, session_key: &str, entry_id: String) {
        self.dispatching.insert(
            session_key.into(),
            OutboxDispatch {
                entry_id,
                seen_working: false,
            },
        );
    }

    pub fn observe_working(&mut self, session_key: &str, working: bool) {
        if let Some(dispatch) = self.dispatching.get_mut(session_key) {
            if working {
                dispatch.seen_working = true;
            } else if dispatch.seen_working {
                self.dispatching.remove(session_key);
            }
        }
    }

    // Observe once before broadcasting to windows, so a slow sibling's old idle event
    // cannot release the next window's newly dispatched prompt.
    pub fn observe_server_event(&mut self, host: &str, event: &ServerEvent) {
        match event {
            ServerEvent::SessionUpdated { session } => {
                self.observe_working(
                    &Self::session_key(host, session.agent, &session.id),
                    session.working,
                );
            }
            ServerEvent::Sessions { sessions } => {
                for session in sessions {
                    self.observe_working(
                        &Self::session_key(host, session.agent, &session.id),
                        session.working,
                    );
                }
            }
            ServerEvent::HistoryAppend {
                session_id,
                working: Some(working),
                ..
            } => {
                for agent in [
                    AgentKind::Claude,
                    AgentKind::Codex,
                    AgentKind::Antigravity,
                    AgentKind::Pi,
                    AgentKind::Cursor,
                ] {
                    self.observe_working(&Self::session_key(host, agent, session_id), *working);
                }
            }
            _ => {}
        }
    }

    pub fn take_next_for_dispatch(
        &mut self,
        session_key: &str,
        working: bool,
    ) -> Option<OutboxEntry> {
        if working || self.dispatching.contains_key(session_key) {
            return None;
        }
        let entry = self.pop_next(session_key)?;
        self.mark_dispatched(session_key, entry.id.clone());
        Some(entry)
    }

    pub fn peek_next(&self, session_key: &str) -> Option<&OutboxEntry> {
        self.entries.iter().find(|e| e.session_key == session_key)
    }

    pub fn pop_next(&mut self, session_key: &str) -> Option<OutboxEntry> {
        let pos = self
            .entries
            .iter()
            .position(|e| e.session_key == session_key)?;
        Some(self.entries.remove(pos))
    }

    pub fn remove_by_id(&mut self, id: &str) -> Option<OutboxEntry> {
        let pos = self.entries.iter().position(|e| e.id == id)?;
        Some(self.entries.remove(pos))
    }

    pub fn entries_for_session(&self, session_key: &str) -> Vec<&OutboxEntry> {
        self.entries
            .iter()
            .filter(|e| e.session_key == session_key)
            .collect()
    }

    pub fn count_for_session(&self, session_key: &str) -> usize {
        self.entries
            .iter()
            .filter(|e| e.session_key == session_key)
            .count()
    }

    /// Removes all queued entries for `session_key` (e.g. on tab close),
    /// returning the count removed while preserving all other sessions.
    pub fn clear_session(&mut self, session_key: &str) -> usize {
        let before = self.entries.len();
        self.entries.retain(|e| e.session_key != session_key);
        before - self.entries.len()
    }

    pub fn clear_all(&mut self) {
        self.entries.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn to_file(&self) -> OutboxFile {
        OutboxFile {
            schema_version: OUTBOX_SCHEMA_VERSION,
            entries: self.entries.clone(),
        }
    }

    pub fn save(&self) -> Result<(), OutboxError> {
        self.save_to(&MultibotSettings::outbox_path())
    }

    pub fn save_to(&self, path: &Path) -> Result<(), OutboxError> {
        let file = self.to_file();
        let bytes = serde_json::to_vec_pretty(&file)
            .map_err(|err| OutboxError::Serialization(err.to_string()))?;
        write_outbox_atomic(path, &bytes)
    }

    pub fn load() -> Self {
        Self::load_from(&MultibotSettings::outbox_path())
    }

    pub fn load_from(path: &Path) -> Self {
        let meta = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(_) => {
                quarantine_outbox(path);
                return Self::default();
            }
        };

        if !meta.file_type().is_file() || meta.len() > MAX_OUTBOX_FILE_BYTES {
            quarantine_outbox(path);
            return Self::default();
        }

        let bytes = match fs::read(path) {
            Ok(b) if (b.len() as u64) <= MAX_OUTBOX_FILE_BYTES => b,
            _ => {
                quarantine_outbox(path);
                return Self::default();
            }
        };

        let parsed: OutboxFile = match serde_json::from_slice(&bytes) {
            Ok(file) => file,
            Err(_) => {
                quarantine_outbox(path);
                return Self::default();
            }
        };

        if parsed.schema_version != OUTBOX_SCHEMA_VERSION {
            quarantine_outbox(path);
            return Self::default();
        }

        let mut counts: HashMap<String, usize> = HashMap::new();
        let mut seen_ids: HashSet<String> = HashSet::new();
        let mut sanitized = Vec::new();
        for entry in parsed.entries {
            if entry.id.trim().is_empty()
                || entry.session_key.trim().is_empty()
                || entry.text.trim().is_empty()
            {
                continue;
            }
            if seen_ids.contains(&entry.id) {
                continue;
            }
            let count = counts.entry(entry.session_key.clone()).or_insert(0);
            if *count >= MAX_OUTBOX_PER_SESSION {
                continue;
            }
            *count += 1;
            seen_ids.insert(entry.id.clone());
            sanitized.push(entry);
        }

        Self {
            entries: sanitized,
            ..Default::default()
        }
    }
}

fn write_outbox_atomic(target: &Path, bytes: &[u8]) -> Result<(), OutboxError> {
    let parent = target
        .parent()
        .ok_or_else(|| OutboxError::Io("outbox path has no parent directory".into()))?;
    fs::create_dir_all(parent).map_err(|err| OutboxError::Io(err.to_string()))?;

    let file_name = target
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "chat-outbox.json".to_string());
    let seq = OUTBOX_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_path = parent.join(format!(".{file_name}.tmp.{}.{seq}", std::process::id()));

    let write_res = (|| -> Result<(), OutboxError> {
        let mut file = File::create(&tmp_path).map_err(|err| OutboxError::Io(err.to_string()))?;
        file.write_all(bytes)
            .map_err(|err| OutboxError::Io(err.to_string()))?;
        file.flush()
            .map_err(|err| OutboxError::Io(err.to_string()))?;
        file.sync_all()
            .map_err(|err| OutboxError::Io(err.to_string()))?;
        Ok(())
    })();

    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }

    if let Err(err) = fs::rename(&tmp_path, target) {
        let _ = fs::remove_file(&tmp_path);
        return Err(OutboxError::Io(err.to_string()));
    }

    Ok(())
}

fn quarantine_outbox(path: &Path) -> Option<PathBuf> {
    let mut corrupt_name = path.file_name()?.to_os_string();
    corrupt_name.push(".corrupt");
    let corrupt_path = path.with_file_name(corrupt_name);

    if fs::rename(path, &corrupt_path).is_ok() {
        return Some(corrupt_path);
    }
    let _ = fs::remove_file(&corrupt_path);
    if fs::rename(path, &corrupt_path).is_ok() {
        return Some(corrupt_path);
    }
    if fs::symlink_metadata(path)
        .map(|m| m.file_type().is_file() || m.file_type().is_symlink())
        .unwrap_or(false)
    {
        let _ = fs::remove_file(path);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_waits_for_a_new_turn_and_duplicate_idle_events_do_not_drain_fifo() {
        for agent in [
            AgentKind::Claude,
            AgentKind::Codex,
            AgentKind::Antigravity,
            AgentKind::Pi,
            AgentKind::Cursor,
        ] {
            let mut outbox = ChatOutbox::default();
            let key = ChatOutbox::session_key("host", agent, "session");
            let first = outbox.enqueue(&key, "first", 1).unwrap();
            let second = outbox.enqueue(&key, "second", 2).unwrap();
            assert!(outbox.take_next_for_dispatch(&key, true).is_none());
            assert_eq!(
                outbox.take_next_for_dispatch(&key, false).unwrap().id,
                first.id
            );
            for _ in 0..3 {
                outbox.observe_working(&key, false);
                assert!(outbox.take_next_for_dispatch(&key, false).is_none());
            }
            outbox.observe_working(&key, true);
            outbox.observe_working(&key, false);
            assert_eq!(
                outbox.take_next_for_dispatch(&key, false).unwrap().id,
                second.id
            );
            assert!(outbox.take_next_for_dispatch(&key, false).is_none());
        }
    }

    #[test]
    fn pending_fifo_survives_reload_without_replaying_a_dispatched_entry() {
        let dir = TempDir::new("pending-reload");
        let path = dir.0.join("outbox.json");
        let mut outbox = ChatOutbox::default();
        outbox.enqueue("session", "sent", 1).unwrap();
        outbox.enqueue("session", "pending", 2).unwrap();
        outbox.take_next_for_dispatch("session", false).unwrap();
        outbox.save_to(&path).unwrap();
        let loaded = ChatOutbox::load_from(&path);
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.peek_next("session").unwrap().text, "pending");
    }

    static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(1);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(prefix: &str) -> Self {
            let n = TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("multibot-{prefix}-{}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn composer_options_survive_outbox_serialization_and_legacy_entries() {
        let mut outbox = ChatOutbox::default();
        let options = SessionOptions {
            model: Some("sonnet".into()),
            ..Default::default()
        };
        outbox
            .enqueue_with_options("session", "draft", 1, Some(options.clone()))
            .unwrap();
        let wire = serde_json::to_string(&outbox).unwrap();
        let restored: ChatOutbox = serde_json::from_str(&wire).unwrap();
        assert_eq!(restored.entries[0].options, Some(options));
        let legacy: ChatOutbox = serde_json::from_str(r#"{"entries":[{"id":"old","session_key":"session","text":"draft","created_at_ms":1}]}"#).unwrap();
        assert_eq!(legacy.entries[0].options, None);
    }

    #[test]
    fn fifo_per_session_ordering_and_isolation() {
        let mut outbox = ChatOutbox::default();
        let s1 = ChatOutbox::session_key("vps", AgentKind::Claude, "s1");
        let s2 = ChatOutbox::session_key("vps", AgentKind::Codex, "s2");

        outbox.enqueue(&s1, "s1-first", 100).unwrap();
        outbox.enqueue(&s2, "s2-first", 101).unwrap();
        outbox.enqueue(&s1, "s1-second", 102).unwrap();
        outbox.enqueue(&s2, "s2-second", 103).unwrap();

        assert_eq!(outbox.peek_next(&s1).unwrap().text, "s1-first");
        assert_eq!(outbox.pop_next(&s1).unwrap().text, "s1-first");
        assert_eq!(outbox.pop_next(&s1).unwrap().text, "s1-second");
        assert!(outbox.pop_next(&s1).is_none());

        assert_eq!(outbox.count_for_session(&s2), 2);
        assert_eq!(outbox.pop_next(&s2).unwrap().text, "s2-first");
        assert_eq!(outbox.pop_next(&s2).unwrap().text, "s2-second");
        assert!(outbox.is_empty());
    }

    #[test]
    fn hard_cap_20_per_session_returns_queue_full() {
        let mut outbox = ChatOutbox::default();
        let s1 = "vps|claude|s1".to_string();
        let s2 = "vps|claude|s2".to_string();

        for i in 0..MAX_OUTBOX_PER_SESSION {
            outbox.enqueue(&s1, format!("msg {i}"), i as i64).unwrap();
        }
        assert_eq!(outbox.count_for_session(&s1), 20);

        let err = outbox.enqueue(&s1, "msg 20", 20).unwrap_err();
        assert_eq!(
            err,
            OutboxError::QueueFull {
                session_key: s1.clone(),
                max: 20
            }
        );
        assert!(err.to_string().contains("20"));

        // Other session is completely unaffected.
        outbox.enqueue(&s2, "other session ok", 21).unwrap();
        assert_eq!(outbox.count_for_session(&s2), 1);

        // Popping one frees a slot in s1.
        assert_eq!(outbox.pop_next(&s1).unwrap().text, "msg 0");
        outbox.enqueue(&s1, "msg 20 retry", 22).unwrap();
        assert_eq!(outbox.count_for_session(&s1), 20);
    }

    #[test]
    fn clear_session_on_tab_close_preserves_other_sessions() {
        let mut outbox = ChatOutbox::default();
        outbox.enqueue("h|claude|a", "a1", 1).unwrap();
        outbox.enqueue("h|claude|b", "b1", 2).unwrap();
        outbox.enqueue("h|claude|a", "a2", 3).unwrap();

        let removed = outbox.clear_session("h|claude|a");
        assert_eq!(removed, 2);
        assert_eq!(outbox.count_for_session("h|claude|a"), 0);
        assert_eq!(outbox.count_for_session("h|claude|b"), 1);
        assert_eq!(outbox.len(), 1);
    }

    #[test]
    fn atomic_save_and_load_roundtrip_schema_v1() {
        let tmp = TempDir::new("outbox-roundtrip");
        let path = tmp.path().join("chat-outbox.json");

        let mut outbox = ChatOutbox::default();
        outbox.enqueue("h|claude|s1", "first", 1000).unwrap();
        outbox.enqueue("h|codex|s2", "second", 2000).unwrap();
        outbox.save_to(&path).unwrap();

        // Verify schema_version: 1 and zero .tmp leftovers.
        let raw = fs::read_to_string(&path).unwrap();
        let file: OutboxFile = serde_json::from_str(&raw).unwrap();
        assert_eq!(file.schema_version, OUTBOX_SCHEMA_VERSION);
        assert_eq!(file.entries.len(), 2);

        for entry in fs::read_dir(tmp.path()).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            assert!(!name.contains(".tmp."), "unexpected temp file: {name}");
        }

        let loaded = ChatOutbox::load_from(&path);
        assert_eq!(loaded, outbox);
    }

    #[test]
    fn corrupt_wrong_version_oversized_and_missing_outbox_recovery() {
        let tmp = TempDir::new("outbox-recovery");

        // Missing file -> empty default.
        let missing = tmp.path().join("missing.json");
        assert!(ChatOutbox::load_from(&missing).is_empty());

        // Corrupt JSON -> quarantined to .corrupt and returns empty.
        let corrupt = tmp.path().join("corrupt.json");
        fs::write(&corrupt, "{not valid json").unwrap();
        assert!(ChatOutbox::load_from(&corrupt).is_empty());
        assert!(!corrupt.exists());
        assert!(tmp.path().join("corrupt.json.corrupt").exists());

        // Wrong schema_version -> quarantined and returns empty.
        let wrong_ver = tmp.path().join("wrong-ver.json");
        fs::write(&wrong_ver, r#"{"schema_version": 99, "entries": []}"#).unwrap();
        assert!(ChatOutbox::load_from(&wrong_ver).is_empty());
        assert!(!wrong_ver.exists());
        assert!(tmp.path().join("wrong-ver.json.corrupt").exists());

        // Oversized (> 16 MiB) sparse file -> quarantined without reading into memory.
        let oversized = tmp.path().join("oversized.json");
        let f = File::create(&oversized).unwrap();
        f.set_len(MAX_OUTBOX_FILE_BYTES + 1).unwrap();
        drop(f);
        assert!(ChatOutbox::load_from(&oversized).is_empty());
        assert!(!oversized.exists());
        assert!(tmp.path().join("oversized.json.corrupt").exists());
    }

    #[test]
    fn rejects_empty_inputs_and_cancels_by_id() {
        let mut outbox = ChatOutbox::default();
        assert_eq!(
            outbox.enqueue("   ", "hello", 1),
            Err(OutboxError::EmptySessionKey)
        );
        assert_eq!(
            outbox.enqueue("h|claude|s1", "  \t\n ", 1),
            Err(OutboxError::EmptyText)
        );

        let e1 = outbox.enqueue("h|claude|s1", "one", 1).unwrap();
        let e2 = outbox.enqueue("h|claude|s1", "two", 2).unwrap();
        let e3 = outbox.enqueue("h|claude|s1", "three", 3).unwrap();

        let removed = outbox.remove_by_id(&e2.id).unwrap();
        assert_eq!(removed.text, "two");
        let remaining: Vec<&str> = outbox
            .entries_for_session("h|claude|s1")
            .into_iter()
            .map(|e| e.text.as_str())
            .collect();
        assert_eq!(remaining, vec![e1.text.as_str(), e3.text.as_str()]);
    }

    #[test]
    fn session_key_prevents_pipe_delimiter_collisions() {
        let k1 = ChatOutbox::session_key("a|claude", AgentKind::Codex, "b");
        let k2 = ChatOutbox::session_key("a", AgentKind::Claude, "codex|b");
        assert_ne!(k1, k2);

        let k3 = ChatOutbox::session_key("a|claude", AgentKind::Claude, "b");
        let k4 = ChatOutbox::session_key("a", AgentKind::Claude, "claude|b");
        assert_ne!(k3, k4);

        let mut outbox = ChatOutbox::default();
        outbox.enqueue(&k1, "from k1", 1).unwrap();
        outbox.enqueue(&k2, "from k2", 2).unwrap();
        assert_eq!(outbox.clear_session(&k1), 1);
        assert_eq!(outbox.count_for_session(&k2), 1);
        assert_eq!(outbox.peek_next(&k2).unwrap().text, "from k2");
    }

    #[test]
    fn enforces_unique_non_empty_entry_ids_across_enqueue_and_load() {
        let mut outbox = ChatOutbox::default();
        // 1. enqueue_entry rejects empty/whitespace and duplicate IDs.
        let empty_id = OutboxEntry {
            id: "   ".into(),
            session_key: "s1".into(),
            text: "hello".into(),
            created_at_ms: 1,
            options: None,
        };
        assert_eq!(
            outbox.enqueue_entry(empty_id),
            Err(OutboxError::EmptyEntryId)
        );
        assert!(OutboxError::EmptyEntryId.to_string().contains("empty"));

        let first = OutboxEntry {
            id: "dup-1".into(),
            session_key: "s1".into(),
            text: "first".into(),
            created_at_ms: 1,
            options: None,
        };
        let dup = OutboxEntry {
            id: "dup-1".into(),
            session_key: "s1".into(),
            text: "second".into(),
            created_at_ms: 2,
            options: None,
        };
        assert!(outbox.enqueue_entry(first).is_ok());
        assert_eq!(
            outbox.enqueue_entry(dup),
            Err(OutboxError::DuplicateEntryId { id: "dup-1".into() })
        );
        assert!(
            OutboxError::DuplicateEntryId { id: "dup-1".into() }
                .to_string()
                .contains("dup-1")
        );

        // 2. enqueue skips any pre-existing persisted ID that collides with the next counter value.
        let next_seq = OUTBOX_ID_COUNTER.load(Ordering::Relaxed);
        let colliding_id = format!("outbox-7777-{next_seq}");
        outbox
            .enqueue_entry(OutboxEntry {
                id: colliding_id.clone(),
                session_key: "s1".into(),
                text: "pre-existing".into(),
                created_at_ms: 7777,
                options: None,
            })
            .unwrap();
        let generated = outbox.enqueue("s1", "new msg", 7777).unwrap();
        assert_ne!(generated.id, colliding_id);

        // 3. load_from filters empty/whitespace IDs and deduplicates keeping first occurrence in FIFO order.
        let tmp = TempDir::new("outbox-dedup-ids");
        let path = tmp.path().join("chat-outbox.json");
        let raw = serde_json::to_string_pretty(&OutboxFile {
            schema_version: OUTBOX_SCHEMA_VERSION,
            entries: vec![
                OutboxEntry {
                    id: "".into(),
                    session_key: "s1".into(),
                    text: "blank id".into(),
                    created_at_ms: 1,
                    options: None,
                },
                OutboxEntry {
                    id: "  \t ".into(),
                    session_key: "s1".into(),
                    text: "ws id".into(),
                    created_at_ms: 2,
                    options: None,
                },
                OutboxEntry {
                    id: "id-a".into(),
                    session_key: "s1".into(),
                    text: "first-a".into(),
                    created_at_ms: 3,
                    options: None,
                },
                OutboxEntry {
                    id: "id-a".into(),
                    session_key: "s1".into(),
                    text: "duplicate-a".into(),
                    created_at_ms: 4,
                    options: None,
                },
                OutboxEntry {
                    id: "id-b".into(),
                    session_key: "s1".into(),
                    text: "first-b".into(),
                    created_at_ms: 5,
                    options: None,
                },
            ],
        })
        .unwrap();
        fs::write(&path, raw).unwrap();

        let loaded = ChatOutbox::load_from(&path);
        let pairs: Vec<(&str, &str)> = loaded
            .entries
            .iter()
            .map(|e| (e.id.as_str(), e.text.as_str()))
            .collect();
        assert_eq!(pairs, vec![("id-a", "first-a"), ("id-b", "first-b")]);
    }
}
