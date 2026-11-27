use anyhow::{Context, Result, anyhow};
use multibot_proto::{AgentKind, ChatMessage, ServerEvent, TodoItem};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

pub const CACHE_SCHEMA_VERSION: u32 = 1;
pub const MAX_SESSION_CACHE_BYTES: usize = 512 * 1024;
pub const MAX_CACHE_DIR_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_LOAD_FILE_BYTES: u64 = 16 * 1024 * 1024;
pub const DEFAULT_MEMORY_LRU_CAPACITY: usize = 64;

static TMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Computes a lowercase 64-hex-character SHA-256 digest of `data`.
pub fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CacheKey {
    pub host: String,
    pub agent: AgentKind,
    pub session_id: String,
}

/// Backward-compatible alias for the M1 cache stub name.
pub type ChatCacheKey = CacheKey;

impl CacheKey {
    pub fn new(
        host: impl Into<String>,
        agent: AgentKind,
        session_id: impl Into<String>,
    ) -> Self {
        Self {
            host: host.into(),
            agent,
            session_id: session_id.into(),
        }
    }

    pub fn agent_str(&self) -> &'static str {
        match self.agent {
            AgentKind::Claude => "claude",
            AgentKind::Antigravity => "antigravity",
            AgentKind::Codex => "codex",
            AgentKind::Shell => "shell",
        }
    }

    /// Lowercase 64-hex-char SHA-256 of length-prefixed `"{host_len}:{host}|{agent}|{sid_len}:{session_id}"`.
    pub fn disk_hash(&self) -> String {
        let raw = format!(
            "{}:{}|{}|{}:{}",
            self.host.len(),
            self.host,
            self.agent_str(),
            self.session_id.len(),
            self.session_id
        );
        sha256_hex(raw.as_bytes())
    }

    /// Returns `"{disk_hash}.jsonl"`.
    pub fn disk_filename(&self) -> String {
        format!("{}.jsonl", self.disk_hash())
    }

    /// Returns `dir.join(self.disk_filename())`.
    pub fn disk_path(&self, dir: &Path) -> PathBuf {
        dir.join(self.disk_filename())
    }
}

/// Line 1 JSON header of `<cache_dir>/<sha256(host|agent|session_id)>.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheHeader {
    pub schema_version: u32,
    pub host: String,
    pub agent: AgentKind,
    pub session_id: String,
    #[serde(default)]
    pub source_path: String,
    #[serde(default)]
    pub file_size: u64,
    #[serde(default)]
    pub file_mtime_ms: i64,
    #[serde(default)]
    pub oldest_offset: u64,
    #[serde(default)]
    pub consumed_offset: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_sha256: Option<String>,
    #[serde(default)]
    pub has_more: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub todos: Vec<TodoItem>,
}

/// In-memory and deserialized session transcript snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CachedSession {
    pub source_path: String,
    pub file_size: u64,
    pub file_mtime_ms: i64,
    pub oldest_offset: u64,
    pub consumed_offset: u64,
    pub probe_sha256: Option<String>,
    pub has_more: bool,
    pub todos: Vec<TodoItem>,
    pub messages: Vec<ChatMessage>,
}

/// Backward-compatible alias for the M1 cache stub name.
pub type CachedSessionSnapshot = CachedSession;

/// Request-scoped token issued when switching sessions to prevent late async
/// remote responses from overwriting the currently active session view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchToken {
    pub generation: u64,
    pub key: CacheKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileOutcome {
    /// Token matches active session; cache updated and caller should refresh active view.
    AppliedToActive,
    /// Token is stale; ONLY the token's own cache entry was updated. Active view MUST NOT change.
    UpdatedBackgroundOnly,
    /// Remote error occurred while cached messages exist; cached transcript preserved untouched.
    PreservedCachedOnError,
    /// Remote error occurred with no cached messages; show retry/error only if `is_active`.
    UncachedError { is_active: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuarantineReason {
    NotAFile,
    EmptyFile,
    OversizedFile { bytes: u64, max: u64 },
    InvalidUtf8,
    TruncatedLastLine,
    CorruptHeader,
    UnsupportedSchemaVersion { found: u32, expected: u32 },
    IdentityMismatch,
    CorruptMessageLine { line_number: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiskLoadResult {
    Hit(CachedSession),
    Miss,
    Quarantined {
        reason: QuarantineReason,
        quarantined_path: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EvictionReport {
    pub files_evicted: usize,
    pub bytes_freed: u64,
    pub remaining_bytes: u64,
}

#[derive(Debug)]
struct MemoryEntry {
    session: CachedSession,
    last_used_tick: u64,
}

#[derive(Debug)]
pub struct ChatCache {
    pub dir: Option<PathBuf>,
    entries: HashMap<CacheKey, MemoryEntry>,
    max_memory_entries: usize,
    max_session_bytes: usize,
    max_dir_bytes: u64,
    lru_clock: u64,
    active_generation: u64,
    active_key: Option<CacheKey>,
    latest_generation_by_key: HashMap<CacheKey, u64>,
    disk_access_clock: u64,
    disk_access_order: HashMap<PathBuf, u64>,
}

impl Default for ChatCache {
    fn default() -> Self {
        Self::with_limits(
            None,
            DEFAULT_MEMORY_LRU_CAPACITY,
            MAX_SESSION_CACHE_BYTES,
            MAX_CACHE_DIR_BYTES as u64,
        )
    }
}

impl ChatCache {
    pub fn new(dir: PathBuf) -> Self {
        Self::with_limits(
            Some(dir),
            DEFAULT_MEMORY_LRU_CAPACITY,
            MAX_SESSION_CACHE_BYTES,
            MAX_CACHE_DIR_BYTES as u64,
        )
    }

    pub fn with_limits(
        dir: Option<PathBuf>,
        max_memory_entries: usize,
        max_session_bytes: usize,
        max_dir_bytes: u64,
    ) -> Self {
        Self {
            dir,
            entries: HashMap::new(),
            max_memory_entries,
            max_session_bytes,
            max_dir_bytes,
            lru_clock: 0,
            active_generation: 0,
            active_key: None,
            latest_generation_by_key: HashMap::new(),
            disk_access_clock: 0,
            disk_access_order: HashMap::new(),
        }
    }

    pub fn memory_len(&self) -> usize {
        self.entries.len()
    }

    pub fn clear_memory(&mut self) {
        self.entries.clear();
    }

    pub fn active_token(&self) -> Option<SwitchToken> {
        self.active_key.clone().map(|key| SwitchToken {
            generation: self.active_generation,
            key,
        })
    }

    pub fn is_token_current(&self, token: &SwitchToken) -> bool {
        self.active_generation == token.generation
            && self.active_key.as_ref() == Some(&token.key)
    }

    /// Read-only O(1) Tier 1 in-memory lookup (zero disk I/O).
    pub fn get(&self, key: &CacheKey) -> Option<&CachedSession> {
        self.entries.get(key).map(|entry| &entry.session)
    }

    /// O(1) Tier 1 in-memory lookup that refreshes LRU recency (zero disk I/O).
    pub fn get_memory(&mut self, key: &CacheKey) -> Option<&CachedSession> {
        self.lru_clock = self.lru_clock.saturating_add(1);
        let tick = self.lru_clock;
        let entry = self.entries.get_mut(key)?;
        entry.last_used_tick = tick;
        Some(&entry.session)
    }

    /// Inserts or updates a Tier 1 in-memory LRU entry, evicting the least recently
    /// used entry when capacity is exceeded.
    pub fn put(&mut self, key: CacheKey, session: CachedSession) {
        if self.max_memory_entries == 0 {
            return;
        }
        self.lru_clock = self.lru_clock.saturating_add(1);
        let tick = self.lru_clock;
        self.entries.insert(
            key,
            MemoryEntry {
                session,
                last_used_tick: tick,
            },
        );
        while self.entries.len() > self.max_memory_entries {
            let oldest_key = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used_tick)
                .map(|(k, _)| k.clone());
            if let Some(evict_key) = oldest_key {
                self.entries.remove(&evict_key);
            } else {
                break;
            }
        }
    }

    pub fn remove_memory(&mut self, key: &CacheKey) -> Option<CachedSession> {
        self.entries.remove(key).map(|entry| entry.session)
    }

    /// Advances the request-scoped generation and marks `key` as the active session.
    pub fn begin_switch(&mut self, key: CacheKey) -> SwitchToken {
        self.active_generation = self.active_generation.saturating_add(1);
        self.active_key = Some(key.clone());
        self.latest_generation_by_key
            .insert(key.clone(), self.active_generation);
        SwitchToken {
            generation: self.active_generation,
            key,
        }
    }

    fn is_stale_for_key(&self, token: &SwitchToken) -> bool {
        self.latest_generation_by_key
            .get(&token.key)
            .is_some_and(|&latest| token.generation < latest)
    }

    /// Synchronous frame-0 switch: advances the switch token and performs an O(1)
    /// Tier 1 in-memory lookup with zero disk I/O (`<1ms`, well under `<16ms`).
    pub fn switch_memory(&mut self, key: CacheKey) -> (SwitchToken, Option<&CachedSession>) {
        let token = self.begin_switch(key.clone());
        let hit = self.get_memory(&key);
        (token, hit)
    }

    /// Reconciles a full history payload against the token's cache entry.
    /// Late responses for an inactive session update ONLY that session's cache entry
    /// and return `ReconcileOutcome::UpdatedBackgroundOnly`.
    /// Stale responses from an older generation for the SAME key are discarded without
    /// mutating Tier 1 memory or Tier 2 disk.
    pub fn reconcile_history(
        &mut self,
        token: &SwitchToken,
        session: CachedSession,
    ) -> ReconcileOutcome {
        if self.is_stale_for_key(token) {
            return ReconcileOutcome::UpdatedBackgroundOnly;
        }
        let _ = self.persist_best_effort(&token.key, &session);
        self.put(token.key.clone(), session);
        if self.is_token_current(token) {
            ReconcileOutcome::AppliedToActive
        } else {
            ReconcileOutcome::UpdatedBackgroundOnly
        }
    }

    /// Reconciles an incremental `HistoryAppend` delta with ID-based deduplication.
    /// Stale responses from an older generation for the SAME key are discarded without
    /// mutating Tier 1 memory or Tier 2 disk. On Tier 1 memory miss, hydrates from
    /// Tier 2 disk before applying the delta so prior cached history is never wiped.
    pub fn reconcile_append(
        &mut self,
        token: &SwitchToken,
        new_messages: Vec<ChatMessage>,
        latest_offset: Option<u64>,
        todos: Vec<TodoItem>,
        probe_sha256: Option<String>,
    ) -> ReconcileOutcome {
        if self.is_stale_for_key(token) {
            return ReconcileOutcome::UpdatedBackgroundOnly;
        }

        let mut session = if let Some(entry) = self.entries.get(&token.key) {
            entry.session.clone()
        } else if self.dir.is_some() {
            match self.load_from_disk(&token.key) {
                DiskLoadResult::Hit(s) => s,
                DiskLoadResult::Miss | DiskLoadResult::Quarantined { .. } => {
                    CachedSession::default()
                }
            }
        } else {
            CachedSession::default()
        };

        for msg in new_messages {
            if !msg.id.is_empty()
                && let Some(existing) = session.messages.iter_mut().find(|m| m.id == msg.id)
            {
                *existing = msg;
                continue;
            }
            session.messages.push(msg);
        }

        if let Some(offset) = latest_offset {
            session.consumed_offset = offset;
        }
        if let Some(probe) = probe_sha256 {
            session.probe_sha256 = Some(probe);
        }
        if !todos.is_empty() {
            session.todos = todos;
        }

        let _ = self.persist_best_effort(&token.key, &session);
        self.put(token.key.clone(), session);

        if self.is_token_current(token) {
            ReconcileOutcome::AppliedToActive
        } else {
            ReconcileOutcome::UpdatedBackgroundOnly
        }
    }

    /// Handles a remote error without EVER blanking an already-cached transcript.
    pub fn reconcile_error(&mut self, token: &SwitchToken) -> ReconcileOutcome {
        let has_memory_cache = self
            .entries
            .get(&token.key)
            .is_some_and(|entry| !entry.session.messages.is_empty());
        if has_memory_cache {
            return ReconcileOutcome::PreservedCachedOnError;
        }

        if self.dir.is_some()
            && let DiskLoadResult::Hit(session) = self.load_from_disk(&token.key)
            && !session.messages.is_empty()
        {
            return ReconcileOutcome::PreservedCachedOnError;
        }

        ReconcileOutcome::UncachedError {
            is_active: self.is_token_current(token),
        }
    }

    /// Convenience adapter for reconciling `multibot_proto::ServerEvent` values.
    pub fn reconcile_server_event(
        &mut self,
        token: &SwitchToken,
        source_path: impl Into<String>,
        event: &ServerEvent,
    ) -> Option<ReconcileOutcome> {
        match event {
            ServerEvent::History {
                session_id,
                messages,
                oldest_offset,
                latest_offset,
                has_more,
                file_size,
                file_mtime_ms,
                probe_sha256,
                todos,
            } => {
                if *session_id != token.key.session_id {
                    return None;
                }
                let session = CachedSession {
                    source_path: source_path.into(),
                    file_size: file_size.unwrap_or(0),
                    file_mtime_ms: file_mtime_ms.unwrap_or(0),
                    oldest_offset: oldest_offset.unwrap_or(0),
                    consumed_offset: latest_offset.unwrap_or(0),
                    probe_sha256: probe_sha256.clone(),
                    has_more: *has_more,
                    todos: todos.clone(),
                    messages: messages.clone(),
                };
                Some(self.reconcile_history(token, session))
            }
            ServerEvent::HistoryAppend {
                session_id,
                messages,
                latest_offset,
                todos,
                probe_sha256,
                ..
            } => {
                if *session_id != token.key.session_id {
                    return None;
                }
                Some(self.reconcile_append(
                    token,
                    messages.clone(),
                    *latest_offset,
                    todos.clone(),
                    probe_sha256.clone(),
                ))
            }
            ServerEvent::Error { .. } => Some(self.reconcile_error(token)),
            _ => None,
        }
    }

    /// Encodes a session into the versioned JSONL disk format capped to `max_session_bytes`,
    /// preserving the newest contiguous tail of messages plus a valid header and trailing `\n`.
    pub fn encode_session_jsonl(
        key: &CacheKey,
        session: &CachedSession,
        max_session_bytes: usize,
    ) -> Result<Vec<u8>> {
        if key.session_id.trim().is_empty() {
            return Err(anyhow!("cache session_id must not be empty"));
        }
        let mut header = CacheHeader {
            schema_version: CACHE_SCHEMA_VERSION,
            host: key.host.clone(),
            agent: key.agent,
            session_id: key.session_id.clone(),
            source_path: session.source_path.clone(),
            file_size: session.file_size,
            file_mtime_ms: session.file_mtime_ms,
            oldest_offset: session.oldest_offset,
            consumed_offset: session.consumed_offset,
            probe_sha256: session.probe_sha256.clone(),
            has_more: session.has_more,
            todos: session.todos.clone(),
        };

        let mut header_line = serialize_jsonl_line(&header)?;
        if header_line.len() > max_session_bytes {
            header.todos.clear();
            header_line = serialize_jsonl_line(&header)?;
            if header_line.len() > max_session_bytes {
                return Err(anyhow!(
                    "cache header ({} bytes) exceeds session byte cap ({} bytes)",
                    header_line.len(),
                    max_session_bytes
                ));
            }
        }

        let msg_budget = max_session_bytes - header_line.len();
        let mut kept_rev: Vec<String> = Vec::new();
        let mut used_bytes: usize = 0;
        let mut trimmed_older = false;

        for msg in session.messages.iter().rev() {
            let line = serialize_jsonl_line(msg)?;
            if used_bytes + line.len() <= msg_budget {
                used_bytes += line.len();
                kept_rev.push(line);
            } else {
                trimmed_older = true;
                break;
            }
        }

        if trimmed_older && !header.has_more {
            header.has_more = true;
            header_line = serialize_jsonl_line(&header)?;
            if header_line.len() > max_session_bytes {
                header.todos.clear();
                header_line = serialize_jsonl_line(&header)?;
                if header_line.len() > max_session_bytes {
                    return Err(anyhow!(
                        "cache header ({} bytes) exceeds session byte cap ({} bytes)",
                        header_line.len(),
                        max_session_bytes
                    ));
                }
            }
            while header_line.len() + used_bytes > max_session_bytes {
                if let Some(oldest_kept) = kept_rev.pop() {
                    used_bytes = used_bytes.saturating_sub(oldest_kept.len());
                } else {
                    break;
                }
            }
        }

        let mut out = Vec::with_capacity(header_line.len() + used_bytes);
        out.extend_from_slice(header_line.as_bytes());
        for line in kept_rev.iter().rev() {
            out.extend_from_slice(line.as_bytes());
        }

        debug_assert!(out.len() <= max_session_bytes);
        debug_assert!(out.ends_with(b"\n"));
        Ok(out)
    }

    /// Persists a session atomically to Tier 2 disk cache and enforces the total directory LRU cap.
    pub fn save_to_disk(&mut self, key: &CacheKey, session: &CachedSession) -> Result<PathBuf> {
        let dir = self
            .dir
            .clone()
            .ok_or_else(|| anyhow!("cache directory not configured"))?;
        let payload = Self::encode_session_jsonl(key, session, self.max_session_bytes)?;
        let path = key.disk_path(&dir);
        write_file_atomic(&path, &payload)?;

        self.disk_access_clock = self.disk_access_clock.saturating_add(1);
        self.disk_access_order
            .insert(path.clone(), self.disk_access_clock);

        let _ = self.evict_disk_lru_protecting(Some(&path));
        Ok(path)
    }

    /// Loads and strictly validates a session from Tier 2 disk cache.
    /// Corrupt, truncated, oversized, version-mismatched, or identity-mismatched
    /// files are safely quarantined without panicking.
    pub fn load_from_disk(&mut self, key: &CacheKey) -> DiskLoadResult {
        let Some(dir) = self.dir.clone() else {
            return DiskLoadResult::Miss;
        };
        let path = key.disk_path(&dir);

        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return DiskLoadResult::Miss,
            Err(_) => {
                let quarantined_path = quarantine_file(&path);
                return DiskLoadResult::Quarantined {
                    reason: QuarantineReason::NotAFile,
                    quarantined_path,
                };
            }
        };

        if !meta.file_type().is_file() {
            let quarantined_path = quarantine_file(&path);
            return DiskLoadResult::Quarantined {
                reason: QuarantineReason::NotAFile,
                quarantined_path,
            };
        }

        let file_len = meta.len();
        if file_len == 0 {
            let quarantined_path = quarantine_file(&path);
            return DiskLoadResult::Quarantined {
                reason: QuarantineReason::EmptyFile,
                quarantined_path,
            };
        }

        if file_len > MAX_LOAD_FILE_BYTES {
            let quarantined_path = quarantine_file(&path);
            return DiskLoadResult::Quarantined {
                reason: QuarantineReason::OversizedFile {
                    bytes: file_len,
                    max: MAX_LOAD_FILE_BYTES,
                },
                quarantined_path,
            };
        }

        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(_) => {
                let quarantined_path = quarantine_file(&path);
                return DiskLoadResult::Quarantined {
                    reason: QuarantineReason::CorruptHeader,
                    quarantined_path,
                };
            }
        };

        if bytes.len() as u64 > MAX_LOAD_FILE_BYTES {
            let quarantined_path = quarantine_file(&path);
            return DiskLoadResult::Quarantined {
                reason: QuarantineReason::OversizedFile {
                    bytes: bytes.len() as u64,
                    max: MAX_LOAD_FILE_BYTES,
                },
                quarantined_path,
            };
        }

        if bytes.last() != Some(&b'\n') {
            let quarantined_path = quarantine_file(&path);
            return DiskLoadResult::Quarantined {
                reason: QuarantineReason::TruncatedLastLine,
                quarantined_path,
            };
        }

        let text = match std::str::from_utf8(&bytes) {
            Ok(s) => s,
            Err(_) => {
                let quarantined_path = quarantine_file(&path);
                return DiskLoadResult::Quarantined {
                    reason: QuarantineReason::InvalidUtf8,
                    quarantined_path,
                };
            }
        };

        let Some((header_line, rest)) = text.split_once('\n') else {
            let quarantined_path = quarantine_file(&path);
            return DiskLoadResult::Quarantined {
                reason: QuarantineReason::TruncatedLastLine,
                quarantined_path,
            };
        };

        let header: CacheHeader = match serde_json::from_str(header_line) {
            Ok(h) => h,
            Err(_) => {
                let quarantined_path = quarantine_file(&path);
                return DiskLoadResult::Quarantined {
                    reason: QuarantineReason::CorruptHeader,
                    quarantined_path,
                };
            }
        };

        if header.schema_version != CACHE_SCHEMA_VERSION {
            let quarantined_path = quarantine_file(&path);
            return DiskLoadResult::Quarantined {
                reason: QuarantineReason::UnsupportedSchemaVersion {
                    found: header.schema_version,
                    expected: CACHE_SCHEMA_VERSION,
                },
                quarantined_path,
            };
        }

        if header.session_id.trim().is_empty()
            || header.host != key.host
            || header.agent != key.agent
            || header.session_id != key.session_id
        {
            let quarantined_path = quarantine_file(&path);
            return DiskLoadResult::Quarantined {
                reason: QuarantineReason::IdentityMismatch,
                quarantined_path,
            };
        }

        let mut messages = Vec::new();
        if !rest.is_empty() {
            let body = rest.strip_suffix('\n').unwrap_or(rest);
            for (idx, line) in body.split('\n').enumerate() {
                match serde_json::from_str::<ChatMessage>(line) {
                    Ok(msg) => messages.push(msg),
                    Err(_) => {
                        let quarantined_path = quarantine_file(&path);
                        return DiskLoadResult::Quarantined {
                            reason: QuarantineReason::CorruptMessageLine {
                                line_number: idx + 2,
                            },
                            quarantined_path,
                        };
                    }
                }
            }
        }

        self.disk_access_clock = self.disk_access_clock.saturating_add(1);
        self.disk_access_order
            .insert(path.clone(), self.disk_access_clock);
        touch_file_mtime(&path);

        let session = CachedSession {
            source_path: header.source_path,
            file_size: header.file_size,
            file_mtime_ms: header.file_mtime_ms,
            oldest_offset: header.oldest_offset,
            consumed_offset: header.consumed_offset,
            probe_sha256: header.probe_sha256,
            has_more: header.has_more,
            todos: header.todos,
            messages,
        };
        self.put(key.clone(), session.clone());
        DiskLoadResult::Hit(session)
    }

    /// Checks Tier 1 in-memory LRU first, falling back to Tier 2 disk load on miss.
    pub fn get_or_load(&mut self, key: &CacheKey) -> Option<&CachedSession> {
        if self.entries.contains_key(key) {
            return self.get_memory(key);
        }
        match self.load_from_disk(key) {
            DiskLoadResult::Hit(_) => self.get_memory(key),
            DiskLoadResult::Miss | DiskLoadResult::Quarantined { .. } => None,
        }
    }

    /// Evicts oldest `.jsonl` cache files in `self.dir` until total bytes `<= max_dir_bytes`.
    /// Never removes subdirectories, symlinks, hidden/temp files, or non-`.jsonl` files.
    pub fn evict_disk_lru(&self) -> Result<EvictionReport> {
        self.evict_disk_lru_protecting(None)
    }

    fn persist_best_effort(&mut self, key: &CacheKey, session: &CachedSession) -> Result<()> {
        if self.dir.is_some() {
            let _ = self.save_to_disk(key, session)?;
        }
        Ok(())
    }

    fn evict_disk_lru_protecting(&self, protect: Option<&Path>) -> Result<EvictionReport> {
        let Some(ref dir) = self.dir else {
            return Ok(EvictionReport::default());
        };
        if !dir.exists() {
            return Ok(EvictionReport::default());
        }

        struct Candidate {
            path: PathBuf,
            name: String,
            size: u64,
            mtime: SystemTime,
            access_order: u64,
        }

        let mut candidates: Vec<Candidate> = Vec::new();
        let mut total_bytes: u64 = 0;

        for entry_res in fs::read_dir(dir)
            .with_context(|| format!("reading cache directory {}", dir.display()))?
        {
            let Ok(entry) = entry_res else {
                continue;
            };
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !meta.file_type().is_file() {
                continue;
            }
            let Some(name_os) = path.file_name() else {
                continue;
            };
            let name = name_os.to_string_lossy().into_owned();
            if name.starts_with('.') || !name.ends_with(".jsonl") {
                continue;
            }

            let size = meta.len();
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let access_order = self.disk_access_order.get(&path).copied().unwrap_or(0);
            total_bytes = total_bytes.saturating_add(size);
            candidates.push(Candidate {
                path,
                name,
                size,
                mtime,
                access_order,
            });
        }

        if total_bytes <= self.max_dir_bytes {
            return Ok(EvictionReport {
                files_evicted: 0,
                bytes_freed: 0,
                remaining_bytes: total_bytes,
            });
        }

        candidates.sort_by(|a, b| {
            a.mtime
                .cmp(&b.mtime)
                .then_with(|| a.access_order.cmp(&b.access_order))
                .then_with(|| a.name.cmp(&b.name))
        });

        let mut files_evicted = 0usize;
        let mut bytes_freed = 0u64;

        for candidate in candidates {
            if total_bytes <= self.max_dir_bytes {
                break;
            }
            if protect.is_some_and(|p| p == candidate.path.as_path()) {
                continue;
            }
            if fs::remove_file(&candidate.path).is_ok() {
                total_bytes = total_bytes.saturating_sub(candidate.size);
                bytes_freed = bytes_freed.saturating_add(candidate.size);
                files_evicted += 1;
            }
        }

        Ok(EvictionReport {
            files_evicted,
            bytes_freed,
            remaining_bytes: total_bytes,
        })
    }
}

fn serialize_jsonl_line<T: Serialize>(value: &T) -> Result<String> {
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    Ok(line)
}

fn write_file_atomic(target: &Path, bytes: &[u8]) -> Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("cache target path has no parent directory"))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("creating cache dir {}", parent.display()))?;

    let file_name = target
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cache.jsonl".to_string());
    let seq = TMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_path = parent.join(format!(".{file_name}.tmp.{}.{seq}", std::process::id()));

    let write_result = (|| -> Result<()> {
        let mut file = File::create(&tmp_path)
            .with_context(|| format!("creating temp cache file {}", tmp_path.display()))?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
        Ok(())
    })();

    if let Err(err) = write_result {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }

    if let Err(err) = fs::rename(&tmp_path, target) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err).with_context(|| {
            format!(
                "renaming temp cache file {} to {}",
                tmp_path.display(),
                target.display()
            )
        });
    }

    Ok(())
}

fn touch_file_mtime(path: &Path) {
    if let Ok(file) = OpenOptions::new().write(true).open(path) {
        let _ = file.set_modified(SystemTime::now());
    }
}

fn quarantine_file(path: &Path) -> Option<PathBuf> {
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
    use multibot_proto::{
        DiffBlock, MessageBlock, MessageRole, PromptOption, PromptQuestion, TodoStatus, ToolStatus,
    };
    use std::time::{Duration, Instant};

    static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(1);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(prefix: &str) -> Self {
            let n = TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "multibot-{prefix}-{}-{n}",
                std::process::id()
            ));
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

    fn make_rich_message(i: usize) -> ChatMessage {
        ChatMessage {
            id: format!("msg-{i:04}"),
            role: if i % 2 == 0 {
                MessageRole::User
            } else {
                MessageRole::Assistant
            },
            text: format!("Message #{i}: implementing slice 4 local cache with rich blocks."),
            timestamp_ms: Some(1_700_000_000_000 + i as i64),
            blocks: vec![
                MessageBlock::Text {
                    text: format!("```rust\nfn step_{i}() -> usize {{ {i} }}\n```"),
                },
                MessageBlock::ToolUse {
                    call_id: format!("call-{i}"),
                    name: "Edit".into(),
                    summary: format!("Edited file_{i}.rs"),
                    detail: "updated implementation".into(),
                    status: ToolStatus::Completed,
                    diff: Some(DiffBlock {
                        path: Some(format!("src/file_{i}.rs")),
                        added: 2,
                        removed: 1,
                        lines: vec![
                            "-old_line()".into(),
                            "+new_line()".into(),
                            "+second_line()".into(),
                        ],
                        truncated_lines: 0,
                    }),
                    output: Some("ok".into()),
                },
                MessageBlock::Image {
                    url: Some(format!("https://example.invalid/img-{i}.png")),
                    local_path: None,
                    alt: format!("screenshot {i}"),
                },
                MessageBlock::QuestionPrompt {
                    prompt_id: format!("prompt-{i}"),
                    provider: "claude".into(),
                    questions: vec![PromptQuestion {
                        question: "Proceed?".into(),
                        header: "Confirm".into(),
                        multi_select: false,
                        options: vec![
                            PromptOption {
                                label: "Yes".into(),
                                description: "Continue".into(),
                            },
                            PromptOption {
                                label: "No".into(),
                                description: "Stop".into(),
                            },
                        ],
                    }],
                },
            ],
            todos: vec![TodoItem {
                content: format!("Task {i}"),
                status: if i % 3 == 0 {
                    TodoStatus::Completed
                } else {
                    TodoStatus::Pending
                },
            }],
            source_byte_range: Some((i as u64 * 100, (i as u64 + 1) * 100)),
            quoted_message_id: (i > 0).then(|| format!("msg-{:04}", i - 1)),
        }
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        assert_eq!(
            sha256_hex(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn in_memory_switch_500_rich_messages_under_16ms() {
        let mut cache = ChatCache::default();
        let key_a = CacheKey::new("vps-1", AgentKind::Claude, "sess-a");
        let key_b = CacheKey::new("vps-1", AgentKind::Codex, "sess-b");

        let messages_a: Vec<ChatMessage> = (0..500).map(make_rich_message).collect();
        let messages_b: Vec<ChatMessage> = (500..1000).map(make_rich_message).collect();

        cache.put(
            key_a.clone(),
            CachedSession {
                source_path: "/home/u/.claude/sess-a.jsonl".into(),
                messages: messages_a,
                ..Default::default()
            },
        );
        cache.put(
            key_b.clone(),
            CachedSession {
                source_path: "/home/u/.codex/sess-b.jsonl".into(),
                messages: messages_b,
                ..Default::default()
            },
        );

        let _ = cache.switch_memory(key_a);

        let t0 = Instant::now();
        let (tok, hit) = cache.switch_memory(key_b.clone());
        let elapsed = t0.elapsed();

        assert!(
            elapsed < Duration::from_millis(16),
            "in-memory switch took {:?}, expected <16ms",
            elapsed
        );
        assert_eq!(tok.key, key_b);
        let session = hit.expect("session B should be in Tier 1 memory");
        assert_eq!(session.messages.len(), 500);
        assert_eq!(session.messages[0].id, "msg-0500");
        assert_eq!(session.messages[499].id, "msg-0999");
        assert_eq!(session.messages[499].blocks.len(), 4);
    }

    #[test]
    fn memory_lru_bounds_and_touch_order() {
        let mut cache = ChatCache::with_limits(None, 2, MAX_SESSION_CACHE_BYTES, 1024);
        let a = CacheKey::new("h", AgentKind::Claude, "a");
        let b = CacheKey::new("h", AgentKind::Claude, "b");
        let c = CacheKey::new("h", AgentKind::Claude, "c");

        cache.put(a.clone(), CachedSession::default());
        cache.put(b.clone(), CachedSession::default());
        assert_eq!(cache.memory_len(), 2);

        // Touch A so B becomes least recently used.
        assert!(cache.get_memory(&a).is_some());
        cache.put(c.clone(), CachedSession::default());

        assert_eq!(cache.memory_len(), 2);
        assert!(cache.get(&a).is_some());
        assert!(cache.get(&c).is_some());
        assert!(cache.get(&b).is_none());

        // Zero capacity should no-op cleanly without panicking.
        let mut zero_cache = ChatCache::with_limits(None, 0, MAX_SESSION_CACHE_BYTES, 1024);
        zero_cache.put(a, CachedSession::default());
        assert_eq!(zero_cache.memory_len(), 0);
    }

    #[test]
    fn disk_atomic_roundtrip_and_header_validation() {
        let tmp = TempDir::new("cache-roundtrip");
        let mut cache = ChatCache::new(tmp.path().to_path_buf());
        let key = CacheKey::new("prod-vps", AgentKind::Antigravity, "agy-42");
        let session = CachedSession {
            source_path: "/home/u/.multibot/history/agy-42.history.jsonl".into(),
            file_size: 4096,
            file_mtime_ms: 1_700_000_123_456,
            oldest_offset: 128,
            consumed_offset: 4096,
            probe_sha256: Some("abc123def456".into()),
            has_more: false,
            todos: vec![TodoItem {
                content: "Verify atomic write".into(),
                status: TodoStatus::InProgress,
            }],
            messages: vec![make_rich_message(0), make_rich_message(1)],
        };

        let saved_path = cache.save_to_disk(&key, &session).unwrap();
        assert_eq!(
            saved_path.file_name().unwrap().to_string_lossy(),
            format!("{}.jsonl", key.disk_hash())
        );

        let raw = fs::read_to_string(&saved_path).unwrap();
        assert!(raw.ends_with('\n'));
        let first_line = raw.lines().next().unwrap();
        let header: CacheHeader = serde_json::from_str(first_line).unwrap();
        assert_eq!(header.schema_version, CACHE_SCHEMA_VERSION);
        assert_eq!(header.host, "prod-vps");
        assert_eq!(header.agent, AgentKind::Antigravity);
        assert_eq!(header.session_id, "agy-42");
        assert_eq!(header.oldest_offset, 128);
        assert_eq!(header.consumed_offset, 4096);
        assert_eq!(header.probe_sha256.as_deref(), Some("abc123def456"));
        assert_eq!(header.todos.len(), 1);

        // Ensure zero `.tmp.*` leftovers in directory.
        for entry in fs::read_dir(tmp.path()).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            assert!(!name.contains(".tmp."), "unexpected temp file: {name}");
        }

        cache.clear_memory();
        assert!(cache.get(&key).is_none());
        let reloaded = cache.get_or_load(&key).expect("disk hit expected").clone();
        assert_eq!(reloaded, session);
        assert!(cache.get(&key).is_some(), "re-populated Tier 1 memory");
    }

    #[test]
    fn per_session_512_kib_tail_cap_preserves_newest_and_valid_format() {
        let tmp = TempDir::new("cache-512k");
        let mut cache = ChatCache::new(tmp.path().to_path_buf());
        let key = CacheKey::new("vps", AgentKind::Claude, "huge-session");

        let padding = "x".repeat(900);
        let messages: Vec<ChatMessage> = (0..1000)
            .map(|i| ChatMessage {
                id: format!("m-{i:04}"),
                role: MessageRole::Assistant,
                text: format!("entry {i:04} {padding}"),
                ..Default::default()
            })
            .collect();

        let session = CachedSession {
            source_path: "/tmp/huge.jsonl".into(),
            has_more: false,
            messages,
            ..Default::default()
        };

        let path = cache.save_to_disk(&key, &session).unwrap();
        let disk_bytes = fs::read(&path).unwrap();
        assert!(
            disk_bytes.len() <= MAX_SESSION_CACHE_BYTES,
            "file {} > 512 KiB",
            disk_bytes.len()
        );
        assert!(disk_bytes.ends_with(b"\n"));

        cache.clear_memory();
        let loaded = cache.get_or_load(&key).expect("trimmed cache loads cleanly");
        assert!(loaded.has_more);
        assert!(!loaded.messages.is_empty());
        assert!(loaded.messages.len() < 1000);
        assert_eq!(loaded.messages.last().unwrap().id, "m-0999");
        // Verify strict chronological continuity of preserved tail.
        let first_kept: usize = loaded.messages[0].id[2..].parse().unwrap();
        assert_eq!(first_kept + loaded.messages.len(), 1000);

        // Single giant message (> 512 KiB) writes valid header-only cache with has_more=true.
        let giant_key = CacheKey::new("vps", AgentKind::Claude, "giant-msg");
        let giant_session = CachedSession {
            messages: vec![ChatMessage {
                id: "giant".into(),
                role: MessageRole::Assistant,
                text: "z".repeat(MAX_SESSION_CACHE_BYTES + 1024),
                ..Default::default()
            }],
            ..Default::default()
        };
        let giant_path = cache.save_to_disk(&giant_key, &giant_session).unwrap();
        let giant_bytes = fs::read(&giant_path).unwrap();
        assert!(giant_bytes.len() <= MAX_SESSION_CACHE_BYTES);
        assert!(giant_bytes.ends_with(b"\n"));
        cache.clear_memory();
        let loaded_giant = cache
            .get_or_load(&giant_key)
            .expect("header-only cache loads cleanly");
        assert!(loaded_giant.messages.is_empty());
        assert!(loaded_giant.has_more);
    }

    #[test]
    fn disk_directory_lru_eviction_cap_and_edge_cases() {
        assert_eq!(MAX_CACHE_DIR_BYTES, 64 * 1024 * 1024);

        let tmp = TempDir::new("cache-lru-dir");
        let cap: u64 = 4096;
        let mut cache = ChatCache::with_limits(Some(tmp.path().to_path_buf()), 8, 2048, cap);

        // Empty directory -> 0 evicted.
        let empty_report = cache.evict_disk_lru().unwrap();
        assert_eq!(empty_report.files_evicted, 0);
        assert_eq!(empty_report.remaining_bytes, 0);

        // Create non-.jsonl files and a subdirectory that MUST NEVER be touched.
        let keep_txt = tmp.path().join("keep.txt");
        let corrupt_file = tmp.path().join("bad.jsonl.corrupt");
        let subdir = tmp.path().join("nested.jsonl");
        fs::write(&keep_txt, "do not delete").unwrap();
        fs::write(&corrupt_file, "corrupt backup").unwrap();
        fs::create_dir_all(&subdir).unwrap();

        let make_session = |label: &str| CachedSession {
            messages: vec![ChatMessage {
                id: format!("id-{label}"),
                role: MessageRole::Assistant,
                text: "a".repeat(1300),
                ..Default::default()
            }],
            ..Default::default()
        };

        let k_a = CacheKey::new("h", AgentKind::Claude, "a");
        let k_b = CacheKey::new("h", AgentKind::Claude, "b");
        let k_c = CacheKey::new("h", AgentKind::Claude, "c");
        let k_d = CacheKey::new("h", AgentKind::Claude, "d");

        let p_a = cache.save_to_disk(&k_a, &make_session("a")).unwrap();
        // Single file under cap -> 0 evicted.
        assert_eq!(cache.evict_disk_lru().unwrap().files_evicted, 0);

        let p_b = cache.save_to_disk(&k_b, &make_session("b")).unwrap();
        // Load/touch A so B is older than A.
        assert!(matches!(cache.load_from_disk(&k_a), DiskLoadResult::Hit(_)));

        let p_c = cache.save_to_disk(&k_c, &make_session("c")).unwrap();
        let p_d = cache.save_to_disk(&k_d, &make_session("d")).unwrap();

        // Untouched B should be evicted before recently touched A and newly written D.
        assert!(!p_b.exists(), "oldest untouched file B should be evicted");
        assert!(p_d.exists(), "newly written file D must be protected");
        assert!(
            p_a.exists() || p_c.exists(),
            "at least one recent file besides D remains within cap"
        );

        // Total remaining .jsonl size must be <= cap.
        let report = cache.evict_disk_lru().unwrap();
        assert!(
            report.remaining_bytes <= cap,
            "remaining {} > cap {}",
            report.remaining_bytes,
            cap
        );

        // Non-.jsonl files and directories untouched.
        assert!(keep_txt.exists());
        assert!(corrupt_file.exists());
        assert!(subdir.is_dir());
    }

    #[test]
    fn corrupt_truncated_version_identity_oversized_quarantined_safely() {
        let tmp = TempDir::new("cache-quarantine");
        let mut cache = ChatCache::new(tmp.path().to_path_buf());

        // 1. Corrupt header JSON
        let k1 = CacheKey::new("h", AgentKind::Claude, "corrupt-header");
        let p1 = k1.disk_path(tmp.path());
        fs::write(&p1, "{not valid json\n").unwrap();
        assert!(matches!(
            cache.load_from_disk(&k1),
            DiskLoadResult::Quarantined {
                reason: QuarantineReason::CorruptHeader,
                quarantined_path: Some(_)
            }
        ));
        assert!(!p1.exists());
        assert!(tmp.path().join(format!("{}.corrupt", k1.disk_filename())).exists());

        // 2. Corrupt message JSON on line 2
        let k2 = CacheKey::new("h", AgentKind::Claude, "corrupt-msg");
        let p2 = k2.disk_path(tmp.path());
        let valid_header = serde_json::to_string(&CacheHeader {
            schema_version: CACHE_SCHEMA_VERSION,
            host: "h".into(),
            agent: AgentKind::Claude,
            session_id: "corrupt-msg".into(),
            source_path: String::new(),
            file_size: 0,
            file_mtime_ms: 0,
            oldest_offset: 0,
            consumed_offset: 0,
            probe_sha256: None,
            has_more: false,
            todos: Vec::new(),
        })
        .unwrap();
        fs::write(&p2, format!("{valid_header}\n{{broken msg\n")).unwrap();
        assert!(matches!(
            cache.load_from_disk(&k2),
            DiskLoadResult::Quarantined {
                reason: QuarantineReason::CorruptMessageLine { line_number: 2 },
                ..
            }
        ));
        assert!(!p2.exists());

        // 3. Truncated last line (no trailing newline)
        let k3 = CacheKey::new("h", AgentKind::Claude, "truncated");
        let p3 = k3.disk_path(tmp.path());
        fs::write(&p3, format!("{valid_header}")).unwrap();
        assert!(matches!(
            cache.load_from_disk(&k3),
            DiskLoadResult::Quarantined {
                reason: QuarantineReason::TruncatedLastLine,
                ..
            }
        ));
        assert!(!p3.exists());

        // 4. Wrong schema_version
        let k4 = CacheKey::new("h", AgentKind::Claude, "wrong-ver");
        let p4 = k4.disk_path(tmp.path());
        let bad_ver_header = serde_json::to_string(&CacheHeader {
            schema_version: 99,
            host: "h".into(),
            agent: AgentKind::Claude,
            session_id: "wrong-ver".into(),
            source_path: String::new(),
            file_size: 0,
            file_mtime_ms: 0,
            oldest_offset: 0,
            consumed_offset: 0,
            probe_sha256: None,
            has_more: false,
            todos: Vec::new(),
        })
        .unwrap();
        fs::write(&p4, format!("{bad_ver_header}\n")).unwrap();
        assert!(matches!(
            cache.load_from_disk(&k4),
            DiskLoadResult::Quarantined {
                reason: QuarantineReason::UnsupportedSchemaVersion {
                    found: 99,
                    expected: CACHE_SCHEMA_VERSION
                },
                ..
            }
        ));

        // 5. Identity mismatch (host, agent, session_id)
        for (suffix, host, agent, sid) in [
            ("bad-host", "other-host", AgentKind::Claude, "bad-host"),
            ("bad-agent", "h", AgentKind::Codex, "bad-agent"),
            ("bad-sid", "h", AgentKind::Claude, "different-sid"),
        ] {
            let key = CacheKey::new("h", AgentKind::Claude, suffix);
            let path = key.disk_path(tmp.path());
            let hdr = serde_json::to_string(&CacheHeader {
                schema_version: CACHE_SCHEMA_VERSION,
                host: host.into(),
                agent,
                session_id: sid.into(),
                source_path: String::new(),
                file_size: 0,
                file_mtime_ms: 0,
                oldest_offset: 0,
                consumed_offset: 0,
                probe_sha256: None,
                has_more: false,
                todos: Vec::new(),
            })
            .unwrap();
            fs::write(&path, format!("{hdr}\n")).unwrap();
            assert!(matches!(
                cache.load_from_disk(&key),
                DiskLoadResult::Quarantined {
                    reason: QuarantineReason::IdentityMismatch,
                    ..
                }
            ));
            assert!(cache.get_or_load(&key).is_none());
        }

        // 6. Oversized file (> 16 MiB) using sparse set_len (instant, zero 16 MiB allocation).
        let k6 = CacheKey::new("h", AgentKind::Claude, "oversized");
        let p6 = k6.disk_path(tmp.path());
        let file = File::create(&p6).unwrap();
        file.set_len(MAX_LOAD_FILE_BYTES + 1).unwrap();
        drop(file);
        assert!(matches!(
            cache.load_from_disk(&k6),
            DiskLoadResult::Quarantined {
                reason: QuarantineReason::OversizedFile {
                    bytes,
                    max: MAX_LOAD_FILE_BYTES
                },
                ..
            } if bytes == MAX_LOAD_FILE_BYTES + 1
        ));
        assert!(!p6.exists());
    }

    #[test]
    fn late_response_guard_and_remote_error_never_blanks_cache() {
        let tmp = TempDir::new("cache-reconcile");
        let mut cache = ChatCache::new(tmp.path().to_path_buf());
        let key_a = CacheKey::new("vps", AgentKind::Claude, "sess-a");
        let key_b = CacheKey::new("vps", AgentKind::Claude, "sess-b");
        let key_c = CacheKey::new("vps", AgentKind::Claude, "sess-c");

        let tok_a = cache.begin_switch(key_a.clone());

        cache.put(
            key_b.clone(),
            CachedSession {
                messages: vec![
                    ChatMessage {
                        id: "b1".into(),
                        role: MessageRole::User,
                        text: "hello b".into(),
                        ..Default::default()
                    },
                    ChatMessage {
                        id: "b2".into(),
                        role: MessageRole::Assistant,
                        text: "reply b".into(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
        );
        let (tok_b, hit_b) = cache.switch_memory(key_b.clone());
        assert_eq!(hit_b.unwrap().messages.len(), 2);

        // Late History response for inactive session A updates ONLY A's cache entry.
        let late_event = ServerEvent::History {
            session_id: "sess-a".into(),
            messages: vec![ChatMessage {
                id: "a1".into(),
                role: MessageRole::Assistant,
                text: "late a".into(),
                ..Default::default()
            }],
            oldest_offset: Some(0),
            latest_offset: Some(64),
            has_more: false,
            file_size: Some(64),
            file_mtime_ms: Some(1234),
            probe_sha256: Some("probe-a".into()),
            todos: Vec::new(),
        };
        assert_eq!(
            cache.reconcile_server_event(&tok_a, "/tmp/a.jsonl", &late_event),
            Some(ReconcileOutcome::UpdatedBackgroundOnly)
        );
        assert_eq!(cache.active_token(), Some(tok_b.clone()));
        assert_eq!(cache.get(&key_a).unwrap().messages[0].id, "a1");
        assert_eq!(cache.get(&key_b).unwrap().messages.len(), 2);

        // Remote error for active cached session B MUST preserve cached messages.
        let err_event = ServerEvent::Error {
            code: "ssh_timeout".into(),
            message: "connection dropped".into(),
        };
        assert_eq!(
            cache.reconcile_server_event(&tok_b, "", &err_event),
            Some(ReconcileOutcome::PreservedCachedOnError)
        );
        assert_eq!(cache.get(&key_b).unwrap().messages.len(), 2);
        assert_eq!(cache.get(&key_b).unwrap().messages[1].text, "reply b");

        // HistoryAppend updates matching id in-place and appends new messages.
        let append_event = ServerEvent::HistoryAppend {
            session_id: "sess-b".into(),
            messages: vec![
                ChatMessage {
                    id: "b2".into(),
                    role: MessageRole::Assistant,
                    text: "reply b updated".into(),
                    ..Default::default()
                },
                ChatMessage {
                    id: "b3".into(),
                    role: MessageRole::Assistant,
                    text: "new b3".into(),
                    ..Default::default()
                },
            ],
            latest_offset: Some(512),
            working: Some(false),
            todos: vec![TodoItem {
                content: "Done".into(),
                status: TodoStatus::Completed,
            }],
            probe_sha256: Some("probe-b2".into()),
        };
        assert_eq!(
            cache.reconcile_server_event(&tok_b, "", &append_event),
            Some(ReconcileOutcome::AppliedToActive)
        );
        let b_after = cache.get(&key_b).unwrap();
        assert_eq!(b_after.messages.len(), 3);
        assert_eq!(b_after.messages[1].text, "reply b updated");
        assert_eq!(b_after.messages[2].id, "b3");
        assert_eq!(b_after.consumed_offset, 512);
        assert_eq!(b_after.todos.len(), 1);

        // Uncached active session C returns UncachedError { is_active: true }.
        let (tok_c, hit_c) = cache.switch_memory(key_c);
        assert!(hit_c.is_none());
        assert_eq!(
            cache.reconcile_error(&tok_c),
            ReconcileOutcome::UncachedError { is_active: true }
        );
    }

    #[test]
    fn evicted_reconcile_append_hydrates_from_disk_without_wiping_history() {
        let tmp = TempDir::new("cache-append-hydrate");
        let mut cache = ChatCache::with_limits(
            Some(tmp.path().to_path_buf()),
            2,
            MAX_SESSION_CACHE_BYTES,
            MAX_CACHE_DIR_BYTES as u64,
        );
        let ka = CacheKey::new("vps", AgentKind::Claude, "sess-a");
        let kb = CacheKey::new("vps", AgentKind::Claude, "sess-b");
        let kc = CacheKey::new("vps", AgentKind::Claude, "sess-c");

        let tok_b = cache.begin_switch(kb.clone());
        let initial_b = CachedSession {
            source_path: "/home/u/.claude/sess-b.jsonl".into(),
            oldest_offset: 64,
            consumed_offset: 200,
            has_more: true,
            todos: vec![TodoItem {
                content: "Keep me".into(),
                status: TodoStatus::InProgress,
            }],
            messages: vec![
                ChatMessage {
                    id: "b1".into(),
                    role: MessageRole::User,
                    text: "msg 1".into(),
                    ..Default::default()
                },
                ChatMessage {
                    id: "b2".into(),
                    role: MessageRole::Assistant,
                    text: "msg 2".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            cache.reconcile_history(&tok_b, initial_b),
            ReconcileOutcome::AppliedToActive
        );

        // Evict B from Tier 1 memory (remains intact on Tier 2 disk).
        cache.put(ka, CachedSession::default());
        cache.put(kc, CachedSession::default());
        assert!(cache.get(&kb).is_none());

        // Background HistoryAppend arrives for B: must hydrate disk state first.
        let msg_b3 = ChatMessage {
            id: "b3".into(),
            role: MessageRole::Assistant,
            text: "msg 3".into(),
            ..Default::default()
        };
        cache.reconcile_append(&tok_b, vec![msg_b3], Some(300), vec![], None);

        cache.clear_memory();
        let DiskLoadResult::Hit(reloaded) = cache.load_from_disk(&kb) else {
            panic!("expected hydrated disk cache hit for B");
        };
        assert_eq!(reloaded.messages.len(), 3);
        assert_eq!(reloaded.messages[0].id, "b1");
        assert_eq!(reloaded.messages[1].id, "b2");
        assert_eq!(reloaded.messages[2].id, "b3");
        assert_eq!(reloaded.source_path, "/home/u/.claude/sess-b.jsonl");
        assert_eq!(reloaded.oldest_offset, 64);
        assert_eq!(reloaded.consumed_offset, 300);
        assert!(reloaded.has_more);
        assert_eq!(reloaded.todos.len(), 1);
        assert_eq!(reloaded.todos[0].content, "Keep me");
    }

    #[test]
    fn same_key_stale_generation_discarded_for_history_and_append_in_memory_and_disk() {
        let tmp = TempDir::new("cache-stale-gen");
        let mut cache = ChatCache::new(tmp.path().to_path_buf());
        let ka = CacheKey::new("vps", AgentKind::Claude, "sess-a");
        let kb = CacheKey::new("vps", AgentKind::Claude, "sess-b");

        // Rapid switch: A(gen1) -> B(gen2) -> A(gen3)
        let tok_a1 = cache.begin_switch(ka.clone());
        let _tok_b2 = cache.begin_switch(kb);
        let tok_a3 = cache.begin_switch(ka.clone());

        let fresh_50 = CachedSession {
            source_path: "/tmp/a.jsonl".into(),
            consumed_offset: 5000,
            messages: (0..50)
                .map(|i| ChatMessage {
                    id: format!("m-{i}"),
                    role: MessageRole::Assistant,
                    text: format!("FRESH_V3 #{i}"),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        assert_eq!(
            cache.reconcile_history(&tok_a3, fresh_50),
            ReconcileOutcome::AppliedToActive
        );

        // Stale gen1 full history arrives AFTER gen3 -> must NOT mutate memory OR disk.
        let stale_2 = CachedSession {
            source_path: "/tmp/stale.jsonl".into(),
            consumed_offset: 20,
            messages: vec![
                ChatMessage {
                    id: "m-0".into(),
                    role: MessageRole::Assistant,
                    text: "STALE_V1".into(),
                    ..Default::default()
                },
                ChatMessage {
                    id: "m-1".into(),
                    role: MessageRole::Assistant,
                    text: "STALE_V1".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            cache.reconcile_history(&tok_a1, stale_2),
            ReconcileOutcome::UpdatedBackgroundOnly
        );
        assert_eq!(cache.get(&ka).unwrap().messages.len(), 50);
        assert_eq!(cache.get(&ka).unwrap().messages[0].text, "FRESH_V3 #0");

        // Stale gen1 append arrives AFTER gen3 -> must NOT overwrite m-0 in memory OR disk.
        assert_eq!(
            cache.reconcile_append(
                &tok_a1,
                vec![ChatMessage {
                    id: "m-0".into(),
                    role: MessageRole::Assistant,
                    text: "STALE_APPEND_V1".into(),
                    ..Default::default()
                }],
                Some(99),
                vec![],
                None,
            ),
            ReconcileOutcome::UpdatedBackgroundOnly
        );
        assert_eq!(cache.get(&ka).unwrap().messages.len(), 50);
        assert_eq!(cache.get(&ka).unwrap().messages[0].text, "FRESH_V3 #0");
        assert_eq!(cache.get(&ka).unwrap().consumed_offset, 5000);

        // Verify Tier 2 disk also preserved all 50 fresh messages untouched.
        cache.clear_memory();
        let DiskLoadResult::Hit(on_disk) = cache.load_from_disk(&ka) else {
            panic!("expected fresh disk hit");
        };
        assert_eq!(on_disk.messages.len(), 50);
        assert_eq!(on_disk.messages[0].text, "FRESH_V3 #0");
        assert_eq!(on_disk.consumed_offset, 5000);
    }

    #[test]
    fn cache_key_disk_hash_prevents_pipe_delimiter_collisions() {
        let tmp = TempDir::new("cache-delimiter-collision");
        let mut cache = ChatCache::new(tmp.path().to_path_buf());

        let k1 = CacheKey::new("a|claude", AgentKind::Claude, "b");
        let k2 = CacheKey::new("a", AgentKind::Claude, "claude|b");
        let k3 = CacheKey::new("a|claude", AgentKind::Codex, "c");
        let k4 = CacheKey::new("a", AgentKind::Claude, "codex|c");

        assert_ne!(k1.disk_hash(), k2.disk_hash());
        assert_ne!(k3.disk_hash(), k4.disk_hash());

        let sess_1 = CachedSession {
            messages: vec![ChatMessage {
                id: "1".into(),
                role: MessageRole::Assistant,
                text: "session one".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let sess_2 = CachedSession {
            messages: vec![ChatMessage {
                id: "2".into(),
                role: MessageRole::Assistant,
                text: "session two".into(),
                ..Default::default()
            }],
            ..Default::default()
        };

        cache.save_to_disk(&k1, &sess_1).unwrap();
        cache.save_to_disk(&k2, &sess_2).unwrap();
        cache.clear_memory();

        let DiskLoadResult::Hit(r1) = cache.load_from_disk(&k1) else {
            panic!("k1 must reload cleanly without collision");
        };
        let DiskLoadResult::Hit(r2) = cache.load_from_disk(&k2) else {
            panic!("k2 must reload cleanly without collision");
        };
        assert_eq!(r1.messages[0].text, "session one");
        assert_eq!(r2.messages[0].text, "session two");
    }

    #[test]
    fn encode_and_save_reject_empty_or_whitespace_session_id() {
        let tmp = TempDir::new("cache-empty-sid");
        let mut cache = ChatCache::new(tmp.path().to_path_buf());
        let sess = CachedSession::default();

        for bad_sid in ["", "   \t\n"] {
            let key = CacheKey::new("vps", AgentKind::Claude, bad_sid);
            assert!(
                ChatCache::encode_session_jsonl(&key, &sess, MAX_SESSION_CACHE_BYTES).is_err()
            );
            assert!(cache.save_to_disk(&key, &sess).is_err());
            assert!(!key.disk_path(tmp.path()).exists());
        }
    }
}
