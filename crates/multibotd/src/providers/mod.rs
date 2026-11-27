pub mod activity;
pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod tmux;
pub mod cursor;
pub mod pi;

use anyhow::Result;
use multibot_proto::{
    AgentKind, ChatMessage, DiffBlock, MessageBlock, MessageRole, PromptOption, PromptQuestion,
    SessionSummary, TodoItem, TodoStatus,
};
use crate::server_workspaces::ServerWorkspaceIndex;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub use activity::LiveActivity;
pub use antigravity::AntigravityProvider;
pub use claude::ClaudeProvider;
pub use codex::CodexProvider;
pub use cursor::CursorProvider;
pub use pi::PiProvider;

pub const DEFAULT_HISTORY_MAX_BYTES: usize = 512 * 1024; // 512 KiB (PROJECT.md Feature #18)
pub const HARD_HISTORY_MAX_BYTES: usize = 512 * 1024; // 512 KiB page ceiling
pub const MAX_HISTORY_LINE_BYTES: usize = 256 * 1024; // 256 KiB single-record ceiling (aligned with SEARCH_MAX_LINE_BYTES)
pub const SUMMARY_HEADER_SCAN_BYTES: usize = 64 * 1024; // 64 KiB header metadata scan
pub const SUMMARY_TAIL_SCAN_BYTES: usize = 64 * 1024; // 64 KiB semantic tail scan
const SCRUB_CHUNK_BYTES: usize = 64 * 1024; // Fixed 64 KiB streaming scrub chunk
const RAW_BASE64_MIN_LEN: usize = 256; // Matches BASE64_RE `[A-Za-z0-9+/]{256,}={0,2}`
const DATA_URI_PREFIX_MAX_LEN: usize = 128; // Cap for `data:image/<subtype>;base64,` header
const OMITTED_MARKER: &[u8] = b"[image omitted]"; // JSON-safe ASCII replacement
const MAX_SCRUB_RAW_LINE_BYTES: u64 = 16 * 1024 * 1024; // 16 MiB per-line raw scrub ceiling
const MAX_RAW_SCAN_BYTES_PER_PAGE: u64 = 16 * 1024 * 1024; // 16 MiB latency ceiling per page
pub const WORKING_STALENESS_MS: i64 = 30_000;
/// Hard ceiling on how many live-candidate files discovery walks per provider when the
/// caller only asked for active sessions. Everything else is pruned by path before parsing.
pub const ACTIVE_SCAN_FILE_CAP: usize = 4_000;
pub const PROBE_WINDOW_BYTES: u64 = 64;
pub const MAX_TOOL_DETAIL_CHARS: usize = 4_000;
pub const MAX_TOOL_INPUT_PRE_PAIR_CHARS: usize = 1_800;
pub const MAX_TOOL_SUMMARY_CHARS: usize = 120;
pub const MAX_DIFF_LINES: usize = 40;
pub const MAX_TODOS: usize = 50;
pub const MAX_TODO_CONTENT_CHARS: usize = 200;
pub const MAX_QUESTIONS: usize = 4;
pub const MAX_OPTIONS_PER_QUESTION: usize = 4;
pub const MAX_IMAGES_PER_MESSAGE: usize = 4;
pub const MAX_PREVIEW_CHARS: usize = 140;

static ATOMIC_TMP_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Semantic working indicator inferred strictly from transcript records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TailActivity {
    #[default]
    Unknown,
    Working,
    Idle,
    AwaitingUserInput,
}

#[derive(Debug, Clone, Default)]
pub struct HeaderMeta {
    pub session_id_hint: Option<String>,
    pub cwd: Option<String>,
    pub title_hint: Option<String>,
    pub model: Option<String>,
}

/// Complete output of parsing a byte slice `[slice_start..slice_end)` of a transcript.
#[derive(Debug, Clone, Default)]
pub struct ParsedSlice {
    pub messages: Vec<ChatMessage>,
    pub todos: Vec<TodoItem>,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub title_hint: Option<String>,
    pub session_id_hint: Option<String>,
    pub tail_activity: TailActivity,
    pub has_pending_question: bool,
    pub last_assistant_id: Option<String>,
    pub last_assistant_preview: Option<String>,
    pub last_assistant_ms: Option<i64>,
}

/// Result returned by `read_history_page` for `ClientRequest::ReadHistory` & `WatchSession`.
#[derive(Debug, Clone)]
pub struct HistoryPage {
    pub messages: Vec<ChatMessage>,
    pub oldest_offset: u64,
    pub latest_offset: u64,
    pub has_more: bool,
    pub file_size: u64,
    pub file_mtime_ms: i64,
    pub probe_sha256: String,
    pub todos: Vec<TodoItem>,
    #[allow(dead_code)]
    pub tail_activity: TailActivity,
    #[allow(dead_code)]
    pub has_pending_question: bool,
    #[allow(dead_code)]
    pub model: Option<String>,
}

/// Result returned by `read_history_delta` for `watcher.rs` (`inotify` incremental appends).
#[derive(Debug, Clone)]
pub enum DeltaRead {
    /// No new complete newline-terminated bytes available yet.
    Unchanged {
        #[allow(dead_code)]
        offset: u64,
        #[allow(dead_code)]
        probe_sha256: String,
    },
    /// New complete lines parsed cleanly from `[prev_offset..latest_offset)`.
    Appended {
        messages: Vec<ChatMessage>,
        latest_offset: u64,
        probe_sha256: String,
        todos: Vec<TodoItem>,
        tail_activity: TailActivity,
        #[allow(dead_code)]
        has_pending_question: bool,
    },
    /// File shrunk, rotated, or boundary 64-byte probe mismatched; caller must re-sync.
    ResetRequired {
        #[allow(dead_code)]
        file_size: u64,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionMetaEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_id: Option<String>,
    #[serde(default)]
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetadataStore {
    #[serde(default)]
    pub sessions: HashMap<String, SessionMetaEntry>,
}

impl MetadataStore {
    pub fn path(home: &Path) -> PathBuf {
        home.join(".multibot").join("metadata.json")
    }

    pub fn load(home: &Path) -> Self {
        let p = Self::path(home);
        let Ok(raw) = fs::read_to_string(&p) else {
            return Self::default();
        };
        serde_json::from_str(&raw).unwrap_or_default()
    }

    pub fn save(&self, home: &Path) -> Result<()> {
        write_json_atomic(&Self::path(home), self)
    }

    pub fn get(&self, session_id: &str) -> Option<&SessionMetaEntry> {
        self.sessions.get(session_id)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArchivedEntry {
    pub archived_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArchiveStore {
    #[serde(default)]
    pub archived: HashMap<String, ArchivedEntry>,
}

impl ArchiveStore {
    pub fn path(home: &Path) -> PathBuf {
        home.join(".multibot").join("archive.json")
    }

    pub fn load(home: &Path) -> Self {
        let p = Self::path(home);
        let Ok(raw) = fs::read_to_string(&p) else {
            return Self::default();
        };
        serde_json::from_str(&raw).unwrap_or_default()
    }

    pub fn save(&self, home: &Path) -> Result<()> {
        write_json_atomic(&Self::path(home), self)
    }

    pub fn is_archived(&self, session_id: &str) -> bool {
        self.archived.contains_key(session_id)
    }
}

/// Context passed into discovery/summarization to combine transcript state with daemon state.
///
/// `server_workspaces` carries the workspace the legacy `multiprompt` server recorded for a
/// chat, so a live session can be surfaced with the workspace it belongs to instead of
/// landing in the desktop's catch-all bucket.
pub struct DiscoveryContext<'a> {
    /// Sessions with a prompt in flight (`busy == true`).
    pub active_session_ids: &'a HashSet<String>,
    /// Sessions this daemon holds open, busy or idle-connected. A chat that is open here is
    /// live even when nothing has written to its transcript for weeks.
    pub connected_session_ids: &'a HashSet<String>,
    pub metadata: &'a MetadataStore,
    pub archive: &'a ArchiveStore,
    /// Live chats and the legacy archive, read-only.
    pub live: &'a LiveActivity,
    /// When `false` (the default for `ListSessions`), archived and dead transcripts are
    /// never returned, so the desktop never streams 1,200 dead sessions.
    pub include_archived: bool,
    pub server_workspaces: &'a ServerWorkspaceIndex,
}

pub trait AgentProvider: Send + Sync {
    fn kind(&self) -> AgentKind;
    /// Newest-first transcript files under `home`, at most `limit`.
    ///
    /// `keep` runs before any file is parsed, so a provider can prune thousands of dead
    /// transcripts from directory metadata alone.
    fn discover_paths(
        &self,
        home: &Path,
        limit: usize,
        keep: &dyn Fn(&Path, i64) -> bool,
    ) -> Vec<(PathBuf, i64)>;
    fn parse_lines(&self, lines: &[LineSlice<'_>]) -> ParsedSlice;
    fn parse_slice(&self, bytes: &[u8], base_offset: u64) -> ParsedSlice {
        let lines = iter_complete_lines(bytes, base_offset);
        self.parse_lines(&lines)
    }
    fn extract_header_meta(&self, path: &Path) -> HeaderMeta;
}

// Normalize one record at a time, keeping both page memory and source byte ranges intact.
fn parse_normalized_messages(
    lines: &[LineSlice<'_>],
    prefix: &str,
    normalize: impl FnMut(Value) -> Option<Value>,
) -> ParsedSlice {
    let mut parsed = ClaudeProvider.parse_records(lines, normalize);
    for message in &mut parsed.messages {
        if let Some(offset) = message.id.strip_prefix("claude-") {
            message.id = format!("{prefix}-{offset}");
        }
    }
    populate_last_assistant_fields(&mut parsed);
    parsed
}

pub fn all_providers() -> Vec<Box<dyn AgentProvider>> {
    vec![
        Box::new(ClaudeProvider),
        Box::new(AntigravityProvider),
        Box::new(CodexProvider),
        Box::new(PiProvider),
        Box::new(CursorProvider),
    ]
}

pub fn provider_for(kind: AgentKind) -> Box<dyn AgentProvider> {
    match kind {
        AgentKind::Claude => Box::new(ClaudeProvider),
        AgentKind::Antigravity => Box::new(AntigravityProvider),
        AgentKind::Codex => Box::new(CodexProvider),
        AgentKind::Shell => Box::new(tmux::TmuxProvider),
        AgentKind::Pi => Box::new(PiProvider),
        AgentKind::Cursor => Box::new(CursorProvider),
    }
}

#[cfg(test)]
/// Parsing-level discovery for provider tests: returns every transcript on disk, because
/// these tests check what a provider parses, not which chats the desktop lists.
pub fn discover_all_sessions(home: &Path, limit: usize) -> Vec<SessionSummary> {
    discover_all_sessions_owned(home, limit, true)
}

/// Test-only entry point: loads daemon state from `home` and discovers sessions.
#[cfg(test)]
pub fn discover_all_sessions_owned(
    home: &Path,
    limit: usize,
    include_archived: bool,
) -> Vec<SessionSummary> {
    let metadata = MetadataStore::load(home);
    let archive = ArchiveStore::load(home);
    let live = LiveActivity::load(home);
    let none = HashSet::new();
    // Read the legacy state seeded under the fake `home`, never this host's real state dir:
    // tests assert against ids sampled from a real machine, so a live index would leak its
    // names into the assertions.
    let server_workspaces = ServerWorkspaceIndex::load_from_state_dir(
        &home.join(".local").join("state").join("multiprompt"),
    );
    let ctx = DiscoveryContext {
        active_session_ids: &none,
        connected_session_ids: &none,
        metadata: &metadata,
        archive: &archive,
        live: &live,
        include_archived,
        server_workspaces: &server_workspaces,
    };
    discover_all_sessions_with_ctx(home, limit, &ctx)
}

/// Returns the sessions the desktop should show, newest first.
///
/// By default a session is shown only when it is not archived and something still holds it
/// open: this daemon (`active_session_ids` / `connected_session_ids`), a record this daemon
/// wrote in `metadata.json`, or a live legacy Multiprompt chat (host process or terminal
/// session). A transcript nothing points at is history, not a session anyone is working in.
/// `ctx.include_archived` widens this to everything.
pub fn discover_all_sessions_with_ctx(
    home: &Path,
    limit: usize,
    ctx: &DiscoveryContext<'_>,
) -> Vec<SessionSummary> {
    let mut all = Vec::new();
    let mut seen_ids = HashSet::new();
    // Filtering happens during the directory walk, so the walk itself has to look past the
    // requested count and is capped to keep a huge history cheap.
    let per_provider_limit = if ctx.include_archived {
        limit.max(50)
    } else {
        limit.max(50).saturating_mul(4).min(ACTIVE_SCAN_FILE_CAP)
    };

    // A daemon-owned `~/.multibot/history/` file is named after a hash of its session id, so
    // its own name can never be matched. Deriving the exact paths up front keeps the walk from
    // parsing history files nothing holds open any more.
    let history_candidates: HashSet<PathBuf> = if ctx.include_archived {
        HashSet::new()
    } else {
        ctx.held_open_ids()
            // Archiving a Multibot-hosted chat keeps its `metadata.json` record, so the id
            // stays held open. Archived chats are history: they must not become a candidate.
            .filter(|id| !ctx.is_archived_id(id.as_str()))
            .map(|id| antigravity::hosted_history_path(home, id))
            .collect()
    };

    for provider in all_providers() {
        let keep = |path: &Path, _mtime_ms: i64| {
            ctx.include_archived || ctx.is_held_open_path(path, &history_candidates)
        };
        let paths = provider.discover_paths(home, per_provider_limit, &keep);
        for (path, mtime_ms) in paths {
            let summary = summarize_session(provider.as_ref(), &path, mtime_ms, ctx);
            // A daemon-owned history file is named after a hash, so the id it parses to is not
            // the session this daemon knows it by. Its exact path is the proof it is ours.
            // An archived chat is never shown by default, candidates or not.
            let shown = !summary.archived
                && (ctx.is_shown(&summary) || history_candidates.contains(&path));
            if !ctx.include_archived && !shown {
                continue;
            }
            if seen_ids.insert((summary.agent, summary.id.clone())) {
                all.push(summary);
            }
        }
    }

    // Also include freshly spawned sessions recorded in metadata.json whose files
    // might not exist on disk yet.
    for (sid, meta) in &ctx.metadata.sessions {
        let Some(agent) = meta.agent else {
            continue;
        };
        if agent == AgentKind::Shell {
            continue;
        }
        if seen_ids.contains(&(agent, sid.clone())) {
            continue;
        }
        let Some(source_path) = meta.source_path.as_ref() else {
            continue;
        };
        let path = Path::new(source_path);
        if path.is_file() {
            let mtime_ms = path
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .map(system_time_ms)
                .unwrap_or(meta.updated_at_ms);
            let provider = provider_for(agent);
            let summary = summarize_session(provider.as_ref(), path, mtime_ms, ctx);
            let shown = !summary.archived
                && (ctx.is_shown(&summary) || history_candidates.contains(path));
            if !ctx.include_archived && !shown {
                continue;
            }
            if seen_ids.insert((summary.agent, summary.id.clone())) {
                all.push(summary);
            }
        }
        let title = meta
            .title
            .clone()
            .and_then(non_blank)
            .or_else(|| ctx.server_workspaces.name_for(sid).map(str::to_string))
            .unwrap_or_else(|| sid.clone());
        let summary = SessionSummary {
            id: sid.clone(),
            agent,
            title,
            cwd: meta.cwd.clone(),
            source_path: source_path.clone(),
            updated_at_ms: meta.updated_at_ms,
            working: ctx.active_session_ids.contains(sid),
            archived: ctx.is_archived(sid, path),
            ..Default::default()
        };
        if !ctx.include_archived && !ctx.is_shown(&summary) {
            continue;
        }
        all.push(summary);
        seen_ids.insert((agent, sid.clone()));
    }

    all.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms));
    all.truncate(limit);
    all
}

pub fn discover_live_sessions_with_ctx(
    home: &Path,
    limit: usize,
    ctx: &DiscoveryContext<'_>,
) -> Vec<SessionSummary> {
    let mut sessions = discover_all_sessions_with_ctx(home, limit, ctx);
    let mut shells = tmux::discover_sessions();
    for shell in &mut shells {
        shell.title = ctx
            .metadata
            .get(&shell.id)
            .and_then(|m| m.title.clone())
            .or_else(|| {
                ctx.server_workspaces
                    .name_for(&shell.id)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| shell.id.clone());
        shell.workspace = ctx
            .server_workspaces
            .workspace_for(&shell.id)
            .map(str::to_string);
    }
    // The transcript limit must never hide a live terminal.
    sessions.extend(shells);
    sessions.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms));
    sessions
}

impl DiscoveryContext<'_> {
    /// Multibot's own archive plus the legacy desktop's, which stays read-only (Invariant #2).
    pub fn is_archived(&self, session_id: &str, source_path: &Path) -> bool {
        self.is_archived_id(session_id) || self.live.is_legacy_archived(session_id, source_path)
    }

    /// Archived by id alone, for ids whose transcript path is not known yet.
    fn is_archived_id(&self, session_id: &str) -> bool {
        self.archive.is_archived(session_id)
            || self.live.legacy_archived_ids.contains(session_id)
    }

    /// Whether an already-summarized session belongs in the default list.
    ///
    /// Nothing else counts: a transcript that is not archived but that no daemon, metadata
    /// record or live legacy chat holds open is history and must not cost RAM or latency.
    fn is_shown(&self, summary: &SessionSummary) -> bool {
        !summary.archived && self.holds_open(&summary.id)
    }

    /// Whether this daemon, its metadata, or a live legacy chat holds the session open.
    fn holds_open(&self, session_id: &str) -> bool {
        self.active_session_ids.contains(session_id)
            || self.connected_session_ids.contains(session_id)
            || self.metadata.get(session_id).is_some()
            || self.live.is_live(session_id)
    }

    /// Every id anything still holds open, daemon state, metadata and live legacy chats.
    fn held_open_ids(&self) -> impl Iterator<Item = &String> {
        self.active_session_ids
            .iter()
            .chain(self.connected_session_ids.iter())
            .chain(self.metadata.sessions.keys())
            .chain(self.live.ids.iter())
    }

    /// Matches a transcript file against the sessions anything holds open, before it is
    /// parsed, so walking tens of thousands of dead transcripts stays cheap.
    ///
    /// Antigravity names its transcript `transcript.jsonl` under a conversation-uuid folder,
    /// so every path component is checked, not just the file stem. Codex names a rollout
    /// `rollout-<date>-<uuid>.jsonl`, so a held-open id that is a suffix of the stem matches.
    pub(crate) fn is_held_open_path(
        &self,
        path: &Path,
        history_candidates: &HashSet<PathBuf>,
    ) -> bool {
        let file_name = path.file_name().and_then(|n| n.to_str());
        if let Some(stem) = file_name.map(|n| {
            n.strip_suffix(".jsonl")
                .or_else(|| n.strip_suffix(".history"))
                .unwrap_or(n)
        }) && self.holds_open(stem)
        {
            return true;
        }
        // A daemon-owned Antigravity history file is named after a hash of its session id, so
        // its name cannot be matched against the id directly. `history_candidates` carries the
        // exact paths derived from the ids anything holds open.
        if history_candidates.contains(path) {
            return true;
        }
        if let Some(name) = file_name
            && (name.starts_with("rollout-") || name.contains('_'))
            && self
                .held_open_ids()
                .any(|id| !self.is_archived_id(id) && is_uuid_suffix(id, name))
        {
            return true;
        }
        path.components()
            .filter_map(|c| c.as_os_str().to_str())
            .any(|part| {
                self.live.ids.contains(part)
                    || self.live.tmux_names.contains(part)
                    || self.holds_open(part)
            })
    }
}

/// True when a title has something in it once trimmed, so an empty hint never wins a slot.
fn non_blank(title: String) -> Option<String> {
    (!title.trim().is_empty()).then_some(title)
}

/// True when a live id such as a Codex session uuid ends the file name's last path segment.
fn is_uuid_suffix(id: &str, file_name: &str) -> bool {
    file_name
        .strip_suffix(".jsonl")
        .unwrap_or(file_name)
        .ends_with(id)
}

pub fn summarize_session_by_agent(
    agent: AgentKind,
    path: &Path,
    mtime_ms: i64,
    ctx: &DiscoveryContext<'_>,
) -> SessionSummary {
    let provider = provider_for(agent);
    summarize_session(provider.as_ref(), path, mtime_ms, ctx)
}

pub fn summarize_session(
    provider: &dyn AgentProvider,
    path: &Path,
    file_mtime_ms: i64,
    ctx: &DiscoveryContext<'_>,
) -> SessionSummary {
    let header = provider.extract_header_meta(path);
    let tail = read_tail_slice_for_summary(provider, path);

    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.strip_suffix(".history").unwrap_or(s))
        .unwrap_or("session")
        .to_string();

    let id = header
        .session_id_hint
        .clone()
        .or_else(|| tail.session_id_hint.clone())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(stem);

    let meta_entry = ctx.metadata.get(&id);

    let cwd = meta_entry
        .and_then(|m| m.cwd.clone())
        .or_else(|| tail.cwd.clone())
        .or(header.cwd);

    let title = meta_entry
        .and_then(|m| m.title.clone())
        .and_then(non_blank)
        // A chat the user renamed in the legacy app keeps that name here, so the same chat
        // reads `"Poteto"` in both apps instead of its first prompt.
        .or_else(|| ctx.server_workspaces.name_for(&id).map(str::to_string))
        .or_else(|| tail.title_hint.clone().and_then(non_blank))
        .or_else(|| header.title_hint.clone().and_then(non_blank))
        .unwrap_or_else(|| id.clone());

    let now = now_ms();
    let working = compute_semantic_working(
        ctx.active_session_ids.contains(&id),
        tail.tail_activity,
        file_mtime_ms,
        now,
    );

    let last_assistant_id = tail.last_assistant_id;
    let last_assistant_preview = tail.last_assistant_preview;
    let last_assistant_ms = match (tail.last_assistant_ms, &last_assistant_id) {
        (Some(ts), _) => Some(ts),
        (None, Some(_)) => Some(file_mtime_ms),
        (None, None) => None,
    };

    let todos_done = tail
        .todos
        .iter()
        .filter(|t| t.status == TodoStatus::Completed)
        .count() as u32;
    let todos_total = tail.todos.len() as u32;

    SessionSummary {
        id: id.clone(),
        agent: provider.kind(),
        title,
        cwd,
        source_path: path.to_string_lossy().to_string(),
        updated_at_ms: file_mtime_ms,
        working,
        last_assistant_id,
        last_assistant_preview,
        last_assistant_ms,
        todos_done,
        todos_total,
        needs_attention: tail.has_pending_question,
        archived: ctx.is_archived(&id, path),
        model: tail.model.or(header.model),
        workspace: ctx
            .server_workspaces
            .workspace_for(&id)
            .map(str::to_string),
    }
}

pub fn compute_semantic_working(
    has_active_hosted_pid: bool,
    tail_activity: TailActivity,
    file_mtime_ms: i64,
    now_ms: i64,
) -> bool {
    if has_active_hosted_pid {
        return true;
    }
    match tail_activity {
        TailActivity::Idle | TailActivity::AwaitingUserInput => false,
        TailActivity::Working => now_ms.saturating_sub(file_mtime_ms) <= WORKING_STALENESS_MS,
        TailActivity::Unknown => {
            let age = now_ms.saturating_sub(file_mtime_ms);
            age >= 0 && age < 5_000
        }
    }
}

/// Reads up to `max_lines` lines from the first `SUMMARY_HEADER_SCAN_BYTES` (64 KiB) of `path`.
///
/// Guarantees O(64 KiB) bounded memory even when an early JSONL line is multi-MiB (e.g. a
/// pasted screenshot on turn 1). If the file exceeds 64 KiB, only complete `\n`-terminated
/// lines within the first 64 KiB are returned; any truncated line crossing the 64 KiB boundary
/// is cleanly dropped rather than allocating >64 KiB or feeding truncated JSON to `serde_json`.
pub fn read_header_lines_bounded(path: &Path, max_lines: usize) -> Vec<String> {
    if max_lines == 0 {
        return Vec::new();
    }
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let mut buf = Vec::new();
    if file
        .take((SUMMARY_HEADER_SCAN_BYTES + 1) as u64)
        .read_to_end(&mut buf)
        .is_err()
    {
        return Vec::new();
    }
    if buf.is_empty() {
        return Vec::new();
    }

    let usable: &[u8] = if buf.len() <= SUMMARY_HEADER_SCAN_BYTES {
        &buf
    } else {
        let prefix = &buf[..SUMMARY_HEADER_SCAN_BYTES];
        match prefix.iter().rposition(|&b| b == b'\n') {
            Some(last_nl) => &prefix[..=last_nl],
            None => return Vec::new(),
        }
    };

    let mut lines = Vec::with_capacity(max_lines.min(32));
    let mut start = 0usize;
    while start < usable.len() && lines.len() < max_lines {
        let (end, next_start) = match usable[start..].iter().position(|&b| b == b'\n') {
            Some(rel) => (start + rel, start + rel + 1),
            None => (usable.len(), usable.len()),
        };
        let mut line_bytes = &usable[start..end];
        if let Some(stripped) = line_bytes.strip_suffix(b"\r") {
            line_bytes = stripped;
        }
        lines.push(String::from_utf8_lossy(line_bytes).into_owned());
        start = next_start;
    }
    lines
}

fn read_tail_slice_for_summary(provider: &dyn AgentProvider, path: &Path) -> ParsedSlice {
    let Ok(mut file) = File::open(path) else {
        return ParsedSlice::default();
    };
    let Ok(meta) = file.metadata() else {
        return ParsedSlice::default();
    };
    let file_size = meta.len();
    if file_size == 0 {
        return ParsedSlice::default();
    }
    let read_len = (file_size as usize).min(SUMMARY_TAIL_SCAN_BYTES);
    let raw_start = file_size - read_len as u64;
    if file.seek(SeekFrom::Start(raw_start)).is_err() {
        return ParsedSlice::default();
    }
    let mut buf = vec![0u8; read_len];
    if file.read_exact(&mut buf).is_err() {
        return ParsedSlice::default();
    }
    let first_nl = buf.iter().position(|&b| b == b'\n');
    let (slice_start_idx, aligned_start) = if raw_start == 0 {
        (0, 0)
    } else if let Some(pos) = first_nl {
        (pos + 1, raw_start + pos as u64 + 1)
    } else {
        // Single line larger than 64 KiB with no '\n' in last 64 KiB; skip tail parse safely
        return ParsedSlice::default();
    };
    let tail_slice = &buf[slice_start_idx..];
    let complete_len = tail_slice
        .iter()
        .rposition(|&b| b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let tail_parsed = if complete_len > 0 {
        let parsed = provider.parse_slice(&tail_slice[..complete_len], aligned_start);
        if !parsed.messages.is_empty() {
            return parsed;
        }
        parsed
    } else {
        ParsedSlice::default()
    };

    // When the tail 64 KiB yielded no chat messages (either the last complete line exceeded
    // 64 KiB, or it was followed only by non-message metadata/turn-end records such as Claude
    // `file-history-snapshot`/`progress`/`system`/`isSidechain` or Antigravity `multibot_turn_end`)
    // and the straddling line's tail looks like base64, perform a bounded backward scan +
    // streaming scrub and parse the scrubbed line together with any trailing complete lines in
    // chronological order. This accurately recovers `last_assistant_preview`, semantic `working`
    // state, and trailing metadata while keeping 20 MiB non-base64 files at zero seeks.
    if raw_start > 0
        && let Some(pos) = first_nl
        && tail_looks_like_base64(&buf[..=pos])
    {
        let line_end = raw_start + pos as u64 + 1;
        let tail_complete = if complete_len > 0 {
            buf[slice_start_idx..slice_start_idx + complete_len].to_vec()
        } else {
            Vec::new()
        };
        drop(buf);
        let max_back = MAX_SCRUB_RAW_LINE_BYTES.saturating_sub(line_end - raw_start);
        if let Ok(Some(line_start)) =
            find_line_start_backward_bounded(&mut file, raw_start, max_back)
            && let Ok(Some(scrubbed)) = scrub_line_range_from_file(
                &mut file,
                line_start,
                line_end,
                MAX_HISTORY_LINE_BYTES - 1,
            )
        {
            let mut lines = Vec::new();
            lines.push(LineSlice {
                bytes: &scrubbed,
                start_offset: line_start,
                end_offset: line_end,
            });
            if !tail_complete.is_empty() {
                lines.extend(iter_complete_lines(&tail_complete, aligned_start));
            }
            return provider.parse_lines(&lines);
        }
    }
    tail_parsed
}

/// Backward-compatible Slice 1 helper delegating to the unified rich parser.
#[cfg(test)]
pub fn read_messages_from_file(
    agent: AgentKind,
    path: &Path,
    start_offset: u64,
    tail: Option<usize>,
) -> Result<(Vec<ChatMessage>, u64)> {
    if start_offset == 0 {
        let page = read_history_page(agent, path, None, None, tail, None)?;
        return Ok((page.messages, page.latest_offset));
    }
    match read_history_delta(agent, path, start_offset, None)? {
        DeltaRead::Unchanged { offset, .. } => Ok((Vec::new(), offset)),
        DeltaRead::Appended {
            mut messages,
            latest_offset,
            ..
        } => {
            if let Some(max_tail) = tail
                && messages.len() > max_tail
            {
                let drop = messages.len() - max_tail;
                messages.drain(0..drop);
            }
            Ok((messages, latest_offset))
        }
        DeltaRead::ResetRequired { .. } => {
            let page = read_history_page(agent, path, None, None, tail, None)?;
            Ok((page.messages, page.latest_offset))
        }
    }
}

pub struct LineSlice<'a> {
    pub bytes: &'a [u8],
    pub start_offset: u64,
    pub end_offset: u64,
}

/// Iterates strictly `\n`-terminated JSONL lines, yielding exact file byte offsets and
/// skipping any single record whose raw byte length (including `\n`) exceeds
/// `MAX_HISTORY_LINE_BYTES` (256 KiB).
pub fn iter_complete_lines<'a>(bytes: &'a [u8], base_offset: u64) -> Vec<LineSlice<'a>> {
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        let Some(rel_nl) = bytes[cursor..].iter().position(|&b| b == b'\n') else {
            // Incomplete trailing line without '\n' — never yield!
            break;
        };
        let line_end_idx = cursor + rel_nl;
        let raw_line_len = (line_end_idx - cursor) + 1;
        let start_offset = base_offset + cursor as u64;
        let end_offset = base_offset + (line_end_idx as u64) + 1;
        let trimmed = if line_end_idx > cursor && bytes[line_end_idx - 1] == b'\r' {
            &bytes[cursor..line_end_idx - 1]
        } else {
            &bytes[cursor..line_end_idx]
        };
        cursor = line_end_idx + 1;
        if raw_line_len > MAX_HISTORY_LINE_BYTES {
            continue;
        }
        out.push(LineSlice {
            bytes: trimmed,
            start_offset,
            end_offset,
        });
    }
    out
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

/// Computes lowercase 64-hex SHA-256 of the up-to-64 bytes ending at `offset`:
/// `[offset.saturating_sub(64) .. offset]`.
pub fn compute_probe_sha256(file: &mut File, offset: u64) -> std::io::Result<String> {
    if offset == 0 {
        return Ok(sha256_hex(&[]));
    }
    let start = offset.saturating_sub(PROBE_WINDOW_BYTES);
    let len = (offset - start) as usize;
    file.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0u8; len];
    file.read_exact(&mut buf)?;
    Ok(sha256_hex(&buf))
}

fn extend_capped(out: &mut Vec<u8>, slice: &[u8], max_bytes: usize) -> bool {
    let needed = out.len().saturating_add(slice.len());
    if needed > max_bytes {
        return false;
    }
    if needed > out.capacity() {
        let target = out
            .capacity()
            .saturating_mul(2)
            .max(needed)
            .max(4096)
            .min(max_bytes);
        out.reserve_exact(target - out.len());
    }
    out.extend_from_slice(slice);
    true
}

#[inline]
fn is_raw_b64_body(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'+' || b == b'/'
}

#[inline]
fn is_data_uri_b64_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'_' | b'-')
}

/// Matches `[^;\s"'<>)]` from `BASE64_RE`, excluding `\` (handled as JSON escape).
#[inline]
fn is_data_uri_subtype_byte(b: u8) -> bool {
    (0x21..0x7F).contains(&b) && !matches!(b, b';' | b'"' | b'\'' | b'<' | b'>' | b')' | b'\\')
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JsonStringState {
    /// Outside any JSON string literal; bytes pass through verbatim.
    Outside,
    /// Inside `"..."`; feeds bytes to Layer 2 scrubber.
    InString,
    /// Immediately after `\` inside `"..."`.
    InEscape,
    /// Inside `\uXXXX` inside `"..."`; `rem` in `1..=4`. Prevents the 4 hex digits from
    /// ever being merged into a following base64 run (which would produce invalid `\u[image omitted]`).
    InUnicodeEscape { rem: u8 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DataUriStage {
    /// Matching `image/` case-insensitively (`pos` in `0..6`, accepting `\/` for `/`).
    ImagePrefix(u8),
    /// Matching `<subtype>` (`[^;\s"'<>)]+`). `has_any` requires >= 1 byte before `;`.
    Subtype { has_any: bool },
    /// Matching `base64,` case-insensitively (`pos` in `0..7`).
    Base64Suffix(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScrubState {
    Idle,
    /// Buffering `< 256` consecutive `[A-Za-z0-9+/]` (or `\/`) chars in `pending`.
    RawCandidate { char_count: usize },
    /// Saw `data:` (case-insensitive); matching `image/<subtype>;base64,`.
    DataUriHeader(DataUriStage),
    /// Matched `data:image/<subtype>;base64,`; consuming `[A-Za-z0-9+/=_-]+`.
    DataUriPayload { seen_payload: bool },
    /// Matched `>= 256` raw base64 chars; consuming remaining body chars and up to 2 `=` padding bytes.
    ConsumingRaw { eq_seen: u8 },
}

/// Bounded-memory two-layer byte-level FSM that scrubs `data:image/...;base64,...` URIs and
/// `>= 256`-char raw base64 runs strictly inside JSON string literals while preserving JSON
/// escape validity and multi-byte UTF-8 across arbitrary chunk boundaries.
pub(crate) struct StreamingBase64Scrubber {
    out: Vec<u8>,
    max_out: usize,
    pending: Vec<u8>,
    json: JsonStringState,
    scrub: ScrubState,
    overflowed: bool,
}

impl StreamingBase64Scrubber {
    pub(crate) fn new(max_out: usize) -> Self {
        Self {
            out: Vec::new(),
            max_out,
            pending: Vec::with_capacity(RAW_BASE64_MIN_LEN),
            json: JsonStringState::Outside,
            scrub: ScrubState::Idle,
            overflowed: false,
        }
    }

    pub(crate) fn feed_chunk(&mut self, chunk: &[u8]) -> bool {
        if self.overflowed {
            return false;
        }
        for &b in chunk {
            if !self.feed_byte(b) {
                self.overflowed = true;
                return false;
            }
        }
        true
    }

    #[inline]
    fn emit(&mut self, bytes: &[u8]) -> bool {
        extend_capped(&mut self.out, bytes, self.max_out)
    }

    fn flush_pending(&mut self) -> bool {
        if self.pending.is_empty() {
            return true;
        }
        if !extend_capped(&mut self.out, &self.pending, self.max_out) {
            return false;
        }
        self.pending.clear();
        true
    }

    fn flush_scrub_state(&mut self) -> bool {
        let state = self.scrub;
        self.scrub = ScrubState::Idle;
        match state {
            ScrubState::Idle => true,
            ScrubState::RawCandidate { .. }
            | ScrubState::DataUriHeader(_)
            | ScrubState::DataUriPayload {
                seen_payload: false,
            } => self.flush_pending(),
            ScrubState::DataUriPayload { seen_payload: true } | ScrubState::ConsumingRaw { .. } => {
                self.pending.clear();
                self.emit(OMITTED_MARKER)
            }
        }
    }

    fn feed_byte(&mut self, b: u8) -> bool {
        match self.json {
            JsonStringState::Outside => {
                if b == b'"' {
                    self.json = JsonStringState::InString;
                }
                self.emit(&[b])
            }
            JsonStringState::InString => {
                if b == b'"' {
                    if !self.flush_scrub_state() {
                        return false;
                    }
                    self.json = JsonStringState::Outside;
                    self.emit(b"\"")
                } else if b == b'\\' {
                    self.json = JsonStringState::InEscape;
                    true
                } else {
                    self.feed_in_string(b, &[b])
                }
            }
            JsonStringState::InEscape => {
                if b == b'/' {
                    self.json = JsonStringState::InString;
                    self.feed_in_string(b'/', b"\\/")
                } else if b == b'u' {
                    if !self.flush_scrub_state() {
                        return false;
                    }
                    self.json = JsonStringState::InUnicodeEscape { rem: 4 };
                    self.emit(b"\\u")
                } else {
                    if !self.flush_scrub_state() {
                        return false;
                    }
                    self.json = JsonStringState::InString;
                    self.emit(&[b'\\', b])
                }
            }
            JsonStringState::InUnicodeEscape { rem } => {
                if b == b'"' {
                    self.json = JsonStringState::Outside;
                    self.emit(b"\"")
                } else if b == b'\\' {
                    self.json = JsonStringState::InEscape;
                    true
                } else {
                    self.json = if rem > 1 {
                        JsonStringState::InUnicodeEscape { rem: rem - 1 }
                    } else {
                        JsonStringState::InString
                    };
                    self.emit(&[b])
                }
            }
        }
    }

    fn feed_in_string(&mut self, logical: u8, raw: &[u8]) -> bool {
        loop {
            match self.scrub {
                ScrubState::Idle => {
                    if is_raw_b64_body(logical) {
                        self.pending.clear();
                        self.pending.extend_from_slice(raw);
                        self.scrub = ScrubState::RawCandidate { char_count: 1 };
                        return true;
                    }
                    return self.emit(raw);
                }
                ScrubState::RawCandidate { char_count } => {
                    if is_raw_b64_body(logical) {
                        let next = char_count + 1;
                        if next >= RAW_BASE64_MIN_LEN {
                            self.pending.clear();
                            self.scrub = ScrubState::ConsumingRaw { eq_seen: 0 };
                        } else {
                            self.pending.extend_from_slice(raw);
                            self.scrub = ScrubState::RawCandidate { char_count: next };
                        }
                        return true;
                    }
                    if logical == b':'
                        && self.pending.len() >= 4
                        && self.pending[self.pending.len() - 4..].eq_ignore_ascii_case(b"data")
                    {
                        let split = self.pending.len() - 4;
                        if split > 0
                            && !extend_capped(&mut self.out, &self.pending[..split], self.max_out)
                        {
                            return false;
                        }
                        self.pending.drain(..split);
                        self.pending.extend_from_slice(raw);
                        self.scrub = ScrubState::DataUriHeader(DataUriStage::ImagePrefix(0));
                        return true;
                    }
                    if !self.flush_pending() {
                        return false;
                    }
                    self.scrub = ScrubState::Idle;
                    return self.emit(raw);
                }
                ScrubState::DataUriHeader(stage) => {
                    let next_stage = if self.pending.len() + raw.len() > DATA_URI_PREFIX_MAX_LEN {
                        None
                    } else {
                        match stage {
                            DataUriStage::ImagePrefix(pos) => {
                                if logical.to_ascii_lowercase() == b"image/"[pos as usize] {
                                    if pos + 1 == 6 {
                                        Some(ScrubState::DataUriHeader(DataUriStage::Subtype {
                                            has_any: false,
                                        }))
                                    } else {
                                        Some(ScrubState::DataUriHeader(DataUriStage::ImagePrefix(
                                            pos + 1,
                                        )))
                                    }
                                } else {
                                    None
                                }
                            }
                            DataUriStage::Subtype { has_any } => {
                                if logical == b';' && has_any {
                                    Some(ScrubState::DataUriHeader(DataUriStage::Base64Suffix(0)))
                                } else if is_data_uri_subtype_byte(logical) {
                                    Some(ScrubState::DataUriHeader(DataUriStage::Subtype {
                                        has_any: true,
                                    }))
                                } else {
                                    None
                                }
                            }
                            DataUriStage::Base64Suffix(pos) => {
                                if logical.to_ascii_lowercase() == b"base64,"[pos as usize] {
                                    if pos + 1 == 7 {
                                        Some(ScrubState::DataUriPayload {
                                            seen_payload: false,
                                        })
                                    } else {
                                        Some(ScrubState::DataUriHeader(DataUriStage::Base64Suffix(
                                            pos + 1,
                                        )))
                                    }
                                } else {
                                    None
                                }
                            }
                        }
                    };

                    if let Some(next) = next_stage {
                        self.pending.extend_from_slice(raw);
                        self.scrub = next;
                        return true;
                    }
                    // Header mismatch: flush pending verbatim, return to Idle, and re-dispatch.
                    if !self.flush_pending() {
                        return false;
                    }
                    self.scrub = ScrubState::Idle;
                }
                ScrubState::DataUriPayload { seen_payload } => {
                    if is_data_uri_b64_char(logical) {
                        if !seen_payload {
                            self.pending.clear();
                            self.scrub = ScrubState::DataUriPayload { seen_payload: true };
                        }
                        return true;
                    }
                    if seen_payload {
                        if !self.emit(OMITTED_MARKER) {
                            return false;
                        }
                    } else if !self.flush_pending() {
                        return false;
                    }
                    self.scrub = ScrubState::Idle;
                    return self.emit(raw);
                }
                ScrubState::ConsumingRaw { eq_seen } => {
                    if eq_seen == 0 && is_raw_b64_body(logical) {
                        return true;
                    }
                    if logical == b'=' && eq_seen < 2 {
                        self.scrub = ScrubState::ConsumingRaw {
                            eq_seen: eq_seen + 1,
                        };
                        return true;
                    }
                    if !self.emit(OMITTED_MARKER) {
                        return false;
                    }
                    self.scrub = ScrubState::Idle;
                    // Re-dispatch `(logical, raw)` in `Idle` (e.g., fresh run or `data:` after `=`).
                }
            }
        }
    }

    pub(crate) fn finish(mut self) -> Option<Vec<u8>> {
        if self.overflowed {
            return None;
        }
        if !self.flush_scrub_state() {
            return None;
        }
        if self.json == JsonStringState::InEscape && !self.emit(b"\\") {
            return None;
        }
        if self.out.last() == Some(&b'\r') {
            self.out.pop();
        }
        Some(self.out)
    }
}

/// Streams raw file range `[line_start .. line_end)` (where `line_end` is immediately after `\n`)
/// in fixed 64 KiB chunks through `StreamingBase64Scrubber`.
/// Returns `Ok(None)` immediately without reading if the raw line exceeds `MAX_SCRUB_RAW_LINE_BYTES`
/// (16 MiB) or if scrubbed output exceeds `max_scrubbed_bytes`.
pub(crate) fn scrub_line_range_from_file(
    file: &mut File,
    line_start: u64,
    line_end: u64,
    max_scrubbed_bytes: usize,
) -> std::io::Result<Option<Vec<u8>>> {
    if line_end.saturating_sub(line_start) > MAX_SCRUB_RAW_LINE_BYTES {
        return Ok(None);
    }
    let content_end = line_end.saturating_sub(1); // exclude terminating '\n'
    if content_end <= line_start {
        return Ok(Some(Vec::new()));
    }
    let chunk_cap = (content_end - line_start).min(SCRUB_CHUNK_BYTES as u64) as usize;
    let mut chunk = vec![0u8; chunk_cap];
    let mut scrubber = StreamingBase64Scrubber::new(max_scrubbed_bytes);
    let mut cursor = line_start;
    file.seek(SeekFrom::Start(line_start))?;
    while cursor < content_end {
        let to_read = (content_end - cursor).min(chunk_cap as u64) as usize;
        file.read_exact(&mut chunk[..to_read])?;
        if !scrubber.feed_chunk(&chunk[..to_read]) {
            return Ok(None);
        }
        cursor += to_read as u64;
    }
    Ok(scrubber.finish())
}

pub(crate) fn scrub_line_bytes(trimmed: &[u8], max_scrubbed_bytes: usize) -> Option<Vec<u8>> {
    if trimmed.len() as u64 > MAX_SCRUB_RAW_LINE_BYTES {
        return None;
    }
    let mut scrubber = StreamingBase64Scrubber::new(max_scrubbed_bytes);
    if !scrubber.feed_chunk(trimmed) {
        return None;
    }
    scrubber.finish()
}

fn read_exact_line_trimmed(
    file: &mut File,
    line_start: u64,
    line_end: u64,
) -> std::io::Result<Vec<u8>> {
    let content_end = line_end.saturating_sub(1); // exclude terminating '\n'
    let len = content_end.saturating_sub(line_start) as usize;
    let mut out = vec![0u8; len];
    if len > 0 {
        file.seek(SeekFrom::Start(line_start))?;
        file.read_exact(&mut out)?;
        if out.last() == Some(&b'\r') {
            out.pop();
        }
    }
    Ok(out)
}

fn contains_ascii_ci(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if haystack.len() < needle.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle))
}

fn tail_looks_like_base64(slice: &[u8]) -> bool {
    // 1. Starts with >= 16 consecutive raw base64 chars (counting `\/` as 1 char),
    //    indicating `raw_start` likely landed inside a base64 run starting earlier.
    let mut lead = 0usize;
    let mut i = 0usize;
    while i < slice.len() {
        if is_raw_b64_body(slice[i]) {
            lead += 1;
            i += 1;
        } else if slice[i] == b'\\' && i + 1 < slice.len() && slice[i + 1] == b'/' {
            lead += 1;
            i += 2;
        } else {
            break;
        }
        if lead >= 16 {
            return true;
        }
    }

    // 2. First 128 bytes contain `;base64,` (split `data:image/...;base64,` header).
    let head = &slice[..slice.len().min(DATA_URI_PREFIX_MAX_LEN)];
    if contains_ascii_ci(head, b";base64,") {
        return true;
    }

    // 3. Contains `data:image/` or `data:image\/` (ASCII case-insensitive).
    if contains_ascii_ci(slice, b"data:image/") || contains_ascii_ci(slice, b"data:image\\/") {
        return true;
    }

    // 4. Contains >= 256 consecutive raw base64 chars (counting `\/` as 1 char).
    let mut run = 0usize;
    let mut idx = 0usize;
    while idx < slice.len() {
        if is_raw_b64_body(slice[idx]) {
            run += 1;
            idx += 1;
            if run >= RAW_BASE64_MIN_LEN {
                return true;
            }
        } else if slice[idx] == b'\\' && idx + 1 < slice.len() && slice[idx + 1] == b'/' {
            run += 1;
            idx += 2;
            if run >= RAW_BASE64_MIN_LEN {
                return true;
            }
        } else {
            run = 0;
            idx += 1;
        }
    }

    false
}

fn should_inspect_straddling_line(straddling_tail: &[u8], rem_budget: usize) -> bool {
    if rem_budget == 0 {
        return false;
    }
    if straddling_tail.len() < rem_budget {
        return true;
    }
    tail_looks_like_base64(straddling_tail)
}

fn has_line_exceeding(slice: &[u8], max_line_bytes: usize) -> bool {
    let mut cursor = 0usize;
    while cursor < slice.len() {
        let Some(rel_nl) = slice[cursor..].iter().position(|&b| b == b'\n') else {
            break;
        };
        let raw_line_len = rel_nl + 1;
        if raw_line_len > max_line_bytes {
            return true;
        }
        cursor += raw_line_len;
    }
    false
}

struct CollectedLine {
    arena_start: usize,
    arena_end: usize,
    raw_start: u64,
    raw_end: u64,
}

#[allow(clippy::too_many_arguments)]
fn finalize_history_page(
    file: &mut File,
    mut parsed: ParsedSlice,
    aligned_start: u64,
    aligned_end: u64,
    effective_before: Option<u64>,
    tail: Option<usize>,
    file_size: u64,
    file_mtime_ms: i64,
) -> std::io::Result<HistoryPage> {
    let mut oldest_offset = aligned_start;
    if effective_before.is_none()
        && let Some(max_tail) = tail
        && parsed.messages.len() > max_tail
    {
        let drop_count = parsed.messages.len() - max_tail;
        parsed.messages.drain(0..drop_count);
        if let Some((first_start, _)) = parsed.messages.first().and_then(|m| m.source_byte_range) {
            oldest_offset = first_start;
        }
    }

    let probe_sha256 = compute_probe_sha256(file, aligned_end)?;
    let has_more = oldest_offset > 0;

    Ok(HistoryPage {
        messages: parsed.messages,
        oldest_offset,
        latest_offset: aligned_end,
        has_more,
        file_size,
        file_mtime_ms,
        probe_sha256,
        todos: parsed.todos,
        tail_activity: parsed.tail_activity,
        has_pending_question: parsed.has_pending_question,
        model: parsed.model,
    })
}

/// Unified bounded history reader for `ClientRequest::ReadHistory` and `WatchSession`.
pub fn read_history_page(
    agent: AgentKind,
    path: &Path,
    before_offset: Option<u64>,
    max_bytes: Option<usize>,
    tail: Option<usize>,
    known_probe: Option<&str>,
) -> Result<HistoryPage> {
    let provider = provider_for(agent);
    let mut file = File::open(path)?;
    let meta = file.metadata()?;
    let file_size = meta.len();
    let file_mtime_ms = meta.modified().map(system_time_ms).unwrap_or(0);
    // The adapters share the 256 KiB live-delta budget, including duplicated text/block output.
    let page_ceiling = if matches!(agent, AgentKind::Pi | AgentKind::Cursor) {
        MAX_HISTORY_LINE_BYTES
    } else {
        HARD_HISTORY_MAX_BYTES
    };
    let budget = max_bytes
        .unwrap_or(DEFAULT_HISTORY_MAX_BYTES)
        .clamp(1, page_ceiling) as u64;

    if before_offset == Some(0) {
        return Ok(HistoryPage {
            messages: Vec::new(),
            oldest_offset: 0,
            latest_offset: 0,
            has_more: false,
            file_size,
            file_mtime_ms,
            probe_sha256: sha256_hex(&[]),
            todos: Vec::new(),
            tail_activity: TailActivity::Unknown,
            has_pending_question: false,
            model: None,
        });
    }

    // Validate `before_offset` and `known_probe`. If stale/mismatched, fall back
    // safely to a fresh tail window from EOF.
    let (effective_before, end_bound) = match before_offset {
        Some(b) => {
            let stale = if b > file_size {
                true
            } else if let Some(kp) = known_probe {
                compute_probe_sha256(&mut file, b).ok().as_deref() != Some(kp)
            } else {
                false
            };
            if stale {
                (None, file_size)
            } else {
                (Some(b), b)
            }
        }
        None => (None, file_size),
    };

    if end_bound == 0 {
        return Ok(HistoryPage {
            messages: Vec::new(),
            oldest_offset: 0,
            latest_offset: 0,
            has_more: false,
            file_size,
            file_mtime_ms,
            probe_sha256: sha256_hex(&[]),
            todos: Vec::new(),
            tail_activity: TailActivity::Unknown,
            has_pending_question: false,
            model: None,
        });
    }

    // Backward newline-aligned reader with zero-overhead fast path and bounded streaming
    // base64 scrubbing slow path. Every normal and scrubbed line preserves exact on-disk
    // `(raw_start, raw_end)` offsets and raw-file `probe_sha256`.
    let mut scan_end = end_bound;
    let mut oldest_offset = end_bound;
    let mut latest_offset: Option<u64> = None;
    let mut scrubbed_budget_left = budget as usize;
    let mut raw_bytes_scanned = 0u64;
    let arena_cap = (budget as usize).max(MAX_HISTORY_LINE_BYTES);
    let mut arena: Vec<u8> = Vec::new();
    let mut segments: Vec<CollectedLine> = Vec::new();
    let mut buf = Vec::new();

    let push_segment = |arena: &mut Vec<u8>,
                        segments: &mut Vec<CollectedLine>,
                        bytes: &[u8],
                        raw_start: u64,
                        raw_end: u64|
     -> bool {
        let arena_start = arena.len();
        if !extend_capped(arena, bytes, arena_cap) {
            return false;
        }
        let arena_end = arena.len();
        segments.push(CollectedLine {
            arena_start,
            arena_end,
            raw_start,
            raw_end,
        });
        true
    };

    loop {
        if scan_end == 0 || scrubbed_budget_left == 0 {
            break;
        }
        if latest_offset.is_some() && raw_bytes_scanned >= MAX_RAW_SCAN_BYTES_PER_PAGE {
            break;
        }

        let window = (scrubbed_budget_left as u64).min(scan_end);
        let raw_start = scan_end.saturating_sub(window);
        let read_len = (scan_end - raw_start) as usize;
        buf.resize(read_len, 0);
        file.seek(SeekFrom::Start(raw_start))?;
        file.read_exact(&mut buf)?;
        raw_bytes_scanned += window;

        let first_nl = buf.iter().position(|&b| b == b'\n');
        let (start_in_buf, cand_start) = if raw_start == 0 {
            (0usize, 0u64)
        } else if let Some(pos) = first_nl {
            (pos + 1, raw_start + (pos as u64) + 1)
        } else {
            (buf.len(), scan_end)
        };

        let tail_slice = &buf[start_in_buf..];
        let (end_in_buf, cand_end) = match tail_slice.iter().rposition(|&b| b == b'\n') {
            Some(pos) => (start_in_buf + pos + 1, cand_start + (pos as u64) + 1),
            None => (start_in_buf, cand_start),
        };

        let has_complete_bytes = end_in_buf > start_in_buf;
        if has_complete_bytes {
            latest_offset.get_or_insert(cand_end);
            let complete_slice = &buf[start_in_buf..end_in_buf];
            let has_oversized_inline = has_line_exceeding(complete_slice, MAX_HISTORY_LINE_BYTES);
            let rem_after_raw = scrubbed_budget_left.saturating_sub(complete_slice.len());
            let inspect_straddling_fast = raw_start > 0
                && first_nl.is_some_and(|p| should_inspect_straddling_line(&buf[..=p], rem_after_raw));

            // ZERO-OVERHEAD FAST PATH (normal non-base64 pages: zero extra seeks or copies)
            if segments.is_empty() && !has_oversized_inline && !inspect_straddling_fast {
                let parsed = provider.parse_slice(complete_slice, cand_start);
                drop(buf);
                return Ok(finalize_history_page(
                    &mut file,
                    parsed,
                    cand_start,
                    cand_end,
                    effective_before,
                    tail,
                    file_size,
                    file_mtime_ms,
                )?);
            }

            // SLOW PATH: collect line spans in `complete_slice`, then iterate newest-to-oldest
            // so `segments` stays in reverse chronological order.
            let mut spans: Vec<(usize, usize, usize)> = Vec::new();
            let mut cursor = 0usize;
            while cursor < complete_slice.len() {
                let Some(rel_nl) = complete_slice[cursor..].iter().position(|&b| b == b'\n') else {
                    break;
                };
                let line_end_idx = cursor + rel_nl;
                let trimmed_end =
                    if line_end_idx > cursor && complete_slice[line_end_idx - 1] == b'\r' {
                        line_end_idx - 1
                    } else {
                        line_end_idx
                    };
                spans.push((cursor, trimmed_end, line_end_idx + 1));
                cursor = line_end_idx + 1;
            }

            for &(rel_start, trimmed_end, rel_end) in spans.iter().rev() {
                let raw_line_len = rel_end - rel_start;
                let line_raw_start = cand_start + rel_start as u64;
                let line_raw_end = cand_start + rel_end as u64;
                let trimmed = &complete_slice[rel_start..trimmed_end];
                if raw_line_len <= MAX_HISTORY_LINE_BYTES {
                    if push_segment(&mut arena, &mut segments, trimmed, line_raw_start, line_raw_end)
                    {
                        scrubbed_budget_left = scrubbed_budget_left.saturating_sub(raw_line_len);
                        oldest_offset = line_raw_start;
                    } else {
                        break;
                    }
                } else {
                    if let Some(scrubbed) = scrub_line_bytes(trimmed, MAX_HISTORY_LINE_BYTES - 1)
                        && push_segment(
                            &mut arena,
                            &mut segments,
                            &scrubbed,
                            line_raw_start,
                            line_raw_end,
                        )
                    {
                        scrubbed_budget_left =
                            scrubbed_budget_left.saturating_sub(scrubbed.len() + 1);
                    }
                    // Inline oversized non-base64 lines are skipped cleanly without charging budget.
                    oldest_offset = line_raw_start;
                }
            }

            let inspect_straddling = raw_start > 0
                && scrubbed_budget_left > 0
                && raw_bytes_scanned < MAX_RAW_SCAN_BYTES_PER_PAGE
                && first_nl
                    .is_some_and(|p| should_inspect_straddling_line(&buf[..=p], scrubbed_budget_left));
            if !inspect_straddling {
                break;
            }

            // Release `buf` BEFORE backward scan + streaming scrub to keep peak heap minimal.
            buf.clear();
            buf.shrink_to(0);
            let line_start = find_line_start_backward(&mut file, raw_start)?;
            raw_bytes_scanned += raw_start - line_start;
            let line_len = cand_start - line_start;

            let candidate = if line_len <= MAX_HISTORY_LINE_BYTES as u64
                && line_len <= scrubbed_budget_left as u64
            {
                Some(read_exact_line_trimmed(&mut file, line_start, cand_start)?)
            } else {
                scrub_line_range_from_file(
                    &mut file,
                    line_start,
                    cand_start,
                    MAX_HISTORY_LINE_BYTES - 1,
                )?
            };

            if let Some(bytes) = candidate
                && bytes.len() + 1 <= scrubbed_budget_left
                && push_segment(&mut arena, &mut segments, &bytes, line_start, cand_start)
            {
                scrubbed_budget_left -= bytes.len() + 1;
                oldest_offset = line_start;
                scan_end = line_start;
                continue;
            }
            break;
        }

        if raw_start == 0 {
            if latest_offset.is_none() {
                oldest_offset = 0;
                latest_offset = Some(0);
            }
            break;
        }

        // `raw_start > 0` and no complete line lay inside `[raw_start .. scan_end)`.
        // Release `buf` BEFORE backward scan + streaming scrub.
        buf.clear();
        buf.shrink_to(0);
        let line_start = find_line_start_backward(&mut file, raw_start)?;
        raw_bytes_scanned += raw_start - line_start;

        if let Some(pos) = first_nl {
            // Single complete line `[line_start .. line_end)` longer than `window`.
            let line_end = raw_start + (pos as u64) + 1;
            latest_offset.get_or_insert(line_end);
            let line_len = line_end - line_start;

            if line_len <= MAX_HISTORY_LINE_BYTES as u64
                && (line_len <= scrubbed_budget_left as u64 || segments.is_empty())
            {
                let bytes = read_exact_line_trimmed(&mut file, line_start, line_end)?;
                if push_segment(&mut arena, &mut segments, &bytes, line_start, line_end) {
                    oldest_offset = line_start;
                    scrubbed_budget_left = scrubbed_budget_left.saturating_sub(line_len as usize);
                    scan_end = line_start;
                    if scrubbed_budget_left == 0 {
                        break;
                    }
                    continue;
                }
                break;
            }

            match scrub_line_range_from_file(
                &mut file,
                line_start,
                line_end,
                MAX_HISTORY_LINE_BYTES - 1,
            )? {
                Some(scrubbed)
                    if scrubbed.len() + 1 <= scrubbed_budget_left || segments.is_empty() =>
                {
                    if push_segment(&mut arena, &mut segments, &scrubbed, line_start, line_end) {
                        oldest_offset = line_start;
                        scrubbed_budget_left =
                            scrubbed_budget_left.saturating_sub(scrubbed.len() + 1);
                        scan_end = line_start;
                        if scrubbed_budget_left == 0 {
                            break;
                        }
                        continue;
                    }
                    break;
                }
                Some(_) => {
                    // Valid scrubbed line won't fit remaining budget; next page will read it.
                    break;
                }
                None => {
                    // Irreducible giant non-base64 line: if no lines collected yet, skip cleanly
                    // so `oldest_offset` strictly decreases; otherwise stop before it.
                    if segments.is_empty() {
                        oldest_offset = line_start;
                    }
                    break;
                }
            }
        } else {
            // Zero '\n' bytes in `[raw_start .. scan_end)`: incomplete trailing line at EOF
            // (or mid-line `before_offset`). When `line_start > 0`, byte `line_start - 1` is
            // '\n', so advancing `scan_end = line_start` lands the next iteration on the
            // preceding complete line while keeping `latest_offset == None` (so a >16 MiB
            // incomplete tail still allows reading the preceding page without exceeding O(1)
            // chunk memory).
            if line_start == 0 {
                if latest_offset.is_none() {
                    oldest_offset = 0;
                    latest_offset = Some(0);
                }
                break;
            }
            scan_end = line_start;
        }
    }

    drop(buf);
    segments.reverse();
    let line_slices: Vec<LineSlice<'_>> = segments
        .iter()
        .map(|s| LineSlice {
            bytes: &arena[s.arena_start..s.arena_end],
            start_offset: s.raw_start,
            end_offset: s.raw_end,
        })
        .collect();
    let parsed = provider.parse_lines(&line_slices);
    drop(line_slices);
    drop(segments);
    drop(arena);

    Ok(finalize_history_page(
        &mut file,
        parsed,
        oldest_offset,
        latest_offset.unwrap_or(0),
        effective_before,
        tail,
        file_size,
        file_mtime_ms,
    )?)
}

/// Scans backward from `before_pos` in bounded 64 KiB chunks to find the start offset of
/// the line spanning `before_pos` (i.e. byte immediately after the nearest preceding `\n`,
/// or `0` if this is the first line in the file).
fn find_line_start_backward(file: &mut File, before_pos: u64) -> std::io::Result<u64> {
    const SCAN_CHUNK: u64 = 64 * 1024;
    let mut cursor = before_pos;
    let mut chunk = Vec::new();
    while cursor > 0 {
        let start = cursor.saturating_sub(SCAN_CHUNK);
        let len = (cursor - start) as usize;
        chunk.resize(len, 0);
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut chunk)?;
        if let Some(pos) = chunk.iter().rposition(|&b| b == b'\n') {
            return Ok(start + (pos as u64) + 1);
        }
        cursor = start;
    }
    Ok(0)
}

/// Scans backward from `before_pos` in bounded 64 KiB chunks up to `max_scan_bytes` to find
/// the start offset of the line spanning `before_pos`. Returns `Ok(None)` if no `\n` (and not
/// offset `0`) is reached within `max_scan_bytes`.
fn find_line_start_backward_bounded(
    file: &mut File,
    before_pos: u64,
    max_scan_bytes: u64,
) -> std::io::Result<Option<u64>> {
    const SCAN_CHUNK: u64 = 64 * 1024;
    let mut cursor = before_pos;
    let mut scanned = 0u64;
    let mut chunk = Vec::new();
    while cursor > 0 {
        let remaining_budget = max_scan_bytes.saturating_sub(scanned);
        if remaining_budget == 0 {
            return Ok(None);
        }
        let step = cursor.min(SCAN_CHUNK).min(remaining_budget);
        let start = cursor - step;
        let len = step as usize;
        chunk.resize(len, 0);
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut chunk)?;
        if let Some(pos) = chunk.iter().rposition(|&b| b == b'\n') {
            return Ok(Some(start + (pos as u64) + 1));
        }
        scanned += step;
        cursor = start;
    }
    Ok(Some(0))
}

/// Scans forward from `from_pos` up to `file_size` in bounded 64 KiB chunks to locate the
/// first `\n` byte (the end of a line exceeding `HARD_HISTORY_MAX_BYTES`).
/// Returns `Ok(Some(nl_offset))` for the absolute file offset of that `\n`, or `Ok(None)` if
/// `file_size` is reached without finding `\n`.
fn find_line_end_forward(
    file: &mut File,
    from_pos: u64,
    file_size: u64,
) -> std::io::Result<Option<u64>> {
    const SCAN_CHUNK: u64 = 64 * 1024;
    let mut cursor = from_pos;
    let mut chunk = Vec::new();
    while cursor < file_size {
        let len = (file_size - cursor).min(SCAN_CHUNK) as usize;
        chunk.resize(len, 0);
        file.seek(SeekFrom::Start(cursor))?;
        file.read_exact(&mut chunk)?;
        if let Some(pos) = chunk.iter().position(|&b| b == b'\n') {
            return Ok(Some(cursor + (pos as u64)));
        }
        cursor += len as u64;
    }
    Ok(None)
}

/// Scans `[from_pos .. end_pos)` in bounded 64 KiB chunks to find the absolute file offset
/// of the **last** `\n` within that window, without allocating a multi-MiB buffer.
fn find_last_newline_in_range(
    file: &mut File,
    from_pos: u64,
    end_pos: u64,
) -> std::io::Result<Option<u64>> {
    const SCAN_CHUNK: u64 = 64 * 1024;
    let mut cursor = from_pos;
    let mut last_nl: Option<u64> = None;
    let mut chunk = Vec::new();
    while cursor < end_pos {
        let len = (end_pos - cursor).min(SCAN_CHUNK) as usize;
        chunk.resize(len, 0);
        file.seek(SeekFrom::Start(cursor))?;
        file.read_exact(&mut chunk)?;
        if let Some(pos) = chunk.iter().rposition(|&b| b == b'\n') {
            last_nl = Some(cursor + (pos as u64));
        }
        cursor += len as u64;
    }
    Ok(last_nl)
}

/// Incremental delta reader used by `watcher.rs` for live `inotify` streaming.
///
/// Strictly caps any buffered read at `MAX_HISTORY_LINE_BYTES` (`256 KiB`): normal appends
/// `<= 64 KiB` are read directly, larger bursts are scanned in 64 KiB chunks and batched up to
/// `256 KiB`, and any single complete line exceeding `MAX_HISTORY_LINE_BYTES` is stream-scrubbed
/// in 64 KiB chunks (parsing base64 image records while skipping irreducible non-base64 giants
/// cleanly with zero large allocation).
pub fn read_history_delta(
    agent: AgentKind,
    path: &Path,
    prev_offset: u64,
    prev_probe: Option<&str>,
) -> Result<DeltaRead> {
    const FAST_READ_MAX_BYTES: u64 = 64 * 1024;
    const DELTA_BATCH_MAX_BYTES: u64 = MAX_HISTORY_LINE_BYTES as u64;

    let provider = provider_for(agent);
    let mut file = File::open(path)?;
    let file_size = file.metadata()?.len();

    if file_size < prev_offset {
        return Ok(DeltaRead::ResetRequired { file_size });
    }

    if prev_offset > 0
        && let Some(expected_probe) = prev_probe
    {
        let actual = compute_probe_sha256(&mut file, prev_offset)?;
        if actual != expected_probe {
            return Ok(DeltaRead::ResetRequired { file_size });
        }
    }

    if file_size == prev_offset {
        let probe = match prev_probe {
            Some(p) => p.to_string(),
            None => compute_probe_sha256(&mut file, prev_offset)?,
        };
        return Ok(DeltaRead::Unchanged {
            offset: prev_offset,
            probe_sha256: probe,
        });
    }

    let remaining = file_size - prev_offset;
    if remaining <= FAST_READ_MAX_BYTES {
        let read_len = remaining as usize;
        file.seek(SeekFrom::Start(prev_offset))?;
        let mut buf = vec![0u8; read_len];
        file.read_exact(&mut buf)?;

        let Some(last_nl) = buf.iter().rposition(|&b| b == b'\n') else {
            let probe = match prev_probe {
                Some(p) => p.to_string(),
                None => compute_probe_sha256(&mut file, prev_offset)?,
            };
            return Ok(DeltaRead::Unchanged {
                offset: prev_offset,
                probe_sha256: probe,
            });
        };

        let complete_len = last_nl + 1;
        let parsed = provider.parse_slice(&buf[..complete_len], prev_offset);
        let latest_offset = prev_offset + complete_len as u64;
        let probe_sha256 = compute_probe_sha256(&mut file, latest_offset)?;
        return Ok(DeltaRead::Appended {
            messages: parsed.messages,
            latest_offset,
            probe_sha256,
            todos: parsed.todos,
            tail_activity: parsed.tail_activity,
            has_pending_question: parsed.has_pending_question,
        });
    }

    // Large pending delta (`> 64 KiB`): locate complete line boundaries using only 64 KiB chunks
    // before allocating a parse buffer (capped strictly at `MAX_HISTORY_LINE_BYTES = 256 KiB`).
    let batch_end = prev_offset.saturating_add(DELTA_BATCH_MAX_BYTES).min(file_size);
    let complete_end = if let Some(last_nl) =
        find_last_newline_in_range(&mut file, prev_offset, batch_end)?
    {
        last_nl + 1
    } else if batch_end == file_size {
        // Incomplete trailing line `<= 256 KiB` with no '\n' yet.
        let probe = match prev_probe {
            Some(p) => p.to_string(),
            None => compute_probe_sha256(&mut file, prev_offset)?,
        };
        return Ok(DeltaRead::Unchanged {
            offset: prev_offset,
            probe_sha256: probe,
        });
    } else {
        // Single line starting at `prev_offset` already exceeds `MAX_HISTORY_LINE_BYTES` (256 KiB).
        // Scan forward strictly in 64 KiB chunks to find its terminating '\n'.
        let Some(nl_pos) = find_line_end_forward(&mut file, batch_end, file_size)? else {
            // Partial trailing JSONL line (> 256 KiB) without '\n' — do not advance offset!
            let probe = match prev_probe {
                Some(p) => p.to_string(),
                None => compute_probe_sha256(&mut file, prev_offset)?,
            };
            return Ok(DeltaRead::Unchanged {
                offset: prev_offset,
                probe_sha256: probe,
            });
        };

        // Single complete JSONL record exceeds MAX_HISTORY_LINE_BYTES: attempt bounded streaming
        // base64 scrubbing before falling back to skipping cleanly past `\n`.
        let line_end = nl_pos + 1;
        let probe_sha256 = compute_probe_sha256(&mut file, line_end)?;
        if let Some(scrubbed) = scrub_line_range_from_file(
            &mut file,
            prev_offset,
            line_end,
            MAX_HISTORY_LINE_BYTES - 1,
        )? {
            let line = LineSlice {
                bytes: &scrubbed,
                start_offset: prev_offset,
                end_offset: line_end,
            };
            let parsed = provider.parse_lines(&[line]);
            return Ok(DeltaRead::Appended {
                messages: parsed.messages,
                latest_offset: line_end,
                probe_sha256,
                todos: parsed.todos,
                tail_activity: parsed.tail_activity,
                has_pending_question: parsed.has_pending_question,
            });
        }
        return Ok(DeltaRead::Appended {
            messages: Vec::new(),
            latest_offset: line_end,
            probe_sha256,
            todos: Vec::new(),
            tail_activity: TailActivity::Unknown,
            has_pending_question: false,
        });
    };

    let complete_len = (complete_end - prev_offset) as usize;
    file.seek(SeekFrom::Start(prev_offset))?;
    let mut buf = vec![0u8; complete_len];
    file.read_exact(&mut buf)?;

    let parsed = provider.parse_slice(&buf, prev_offset);
    drop(buf);
    let probe_sha256 = compute_probe_sha256(&mut file, complete_end)?;

    Ok(DeltaRead::Appended {
        messages: parsed.messages,
        latest_offset: complete_end,
        probe_sha256,
        todos: parsed.todos,
        tail_activity: parsed.tail_activity,
        has_pending_question: parsed.has_pending_question,
    })
}

/// Helper to finalize `ParsedSlice` assistant preview metadata from `messages`.
pub fn populate_last_assistant_fields(parsed: &mut ParsedSlice) {
    for msg in parsed.messages.iter().rev() {
        if msg.role == MessageRole::Assistant {
            parsed.last_assistant_id = Some(msg.id.clone());
            parsed.last_assistant_ms = msg.timestamp_ms;
            let preview_src = msg
                .text
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(collapse_whitespace)
                .or_else(|| {
                    msg.blocks.iter().rev().find_map(|b| match b {
                        MessageBlock::ToolUse { summary, .. } if !summary.trim().is_empty() => {
                            Some(collapse_whitespace(summary))
                        }
                        _ => None,
                    })
                });
            parsed.last_assistant_preview =
                preview_src.map(|s| cap_chars(&strip_base64_cow(&s), MAX_PREVIEW_CHARS));
            break;
        }
    }
}

pub fn cap_chars(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

pub fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn strip_base64_cow(s: &str) -> Cow<'_, str> {
    static BASE64_RE: OnceLock<Regex> = OnceLock::new();
    let re = BASE64_RE.get_or_init(|| {
        Regex::new(
            r#"(?i:data:image/[^;\s"'<>)]+;base64,)[A-Za-z0-9+/=_-]+|[A-Za-z0-9+/]{256,}={0,2}"#,
        )
        .expect("valid base64 regex")
    });
    re.replace_all(s, "[image omitted]")
}

pub fn strip_base64_in_str(s: &str) -> String {
    strip_base64_cow(s).into_owned()
}

pub fn clean_tool_value(val: &Value) -> Value {
    match val {
        Value::String(s) => Value::String(strip_base64_in_str(s)),
        Value::Array(arr) => Value::Array(arr.iter().map(clean_tool_value).collect()),
        Value::Object(map) => {
            if let Some(t) = map.get("type").and_then(Value::as_str)
                && matches!(t, "image" | "input_image" | "output_image" | "base64")
            {
                return Value::String("[image omitted]".to_string());
            }
            let mut out = Map::with_capacity(map.len());
            for (k, v) in map {
                if k == "base64" {
                    continue;
                }
                out.insert(k.clone(), clean_tool_value(v));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

pub fn tool_text(val: &Value) -> String {
    let cleaned = clean_tool_value(val);
    tool_text_cleaned(&cleaned)
}

fn tool_text_cleaned(val: &Value) -> String {
    match val {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(arr) => arr
            .iter()
            .map(tool_text_cleaned)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(map) => {
            if let Some(t) = map.get("text").and_then(Value::as_str) {
                t.to_string()
            } else {
                serde_json::to_string(val).unwrap_or_default()
            }
        }
        other => other.to_string(),
    }
}

pub fn combine_tool_detail(input: &str, output: &str) -> String {
    let in_trimmed = input.trim();
    let out_trimmed = output.trim();
    if in_trimmed.is_empty() {
        return cap_chars(out_trimmed, MAX_TOOL_DETAIL_CHARS);
    }
    if out_trimmed.is_empty() {
        return cap_chars(in_trimmed, MAX_TOOL_DETAIL_CHARS);
    }
    let head = cap_chars(in_trimmed, MAX_TOOL_INPUT_PRE_PAIR_CHARS);
    cap_chars(&format!("{head}\n\n{out_trimmed}"), MAX_TOOL_DETAIL_CHARS)
}

pub fn basename_str(path: &str) -> &str {
    path.rsplit(['/', '\\'])
        .find(|seg| !seg.is_empty())
        .unwrap_or(path)
}

pub fn format_tool_summary(name: &str, raw_args: &Value) -> String {
    let parsed_args: Value = match raw_args {
        Value::String(s) => serde_json::from_str::<Value>(s)
            .unwrap_or_else(|_| json!({ "input": s })),
        other => other.clone(),
    };
    let mut kind = name.rsplit('.').next().unwrap_or(name).to_ascii_lowercase();
    let mut path = parsed_args
        .get("file_path")
        .or_else(|| parsed_args.get("path"))
        .and_then(Value::as_str)
        .unwrap_or("file")
        .to_string();

    if kind == "apply_patch" {
        static PATCH_HDR_RE: OnceLock<Regex> = OnceLock::new();
        let re = PATCH_HDR_RE.get_or_init(|| {
            Regex::new(r"\*\*\* (Update|Add|Delete) File: ([^\r\n]+)")
                .expect("valid patch header regex")
        });
        let patch_str = parsed_args
            .get("input")
            .and_then(Value::as_str)
            .or_else(|| raw_args.as_str())
            .unwrap_or("");
        if let Some(cap) = re.captures(patch_str) {
            let action = cap.get(1).map(|m| m.as_str()).unwrap_or("Update");
            path = cap
                .get(2)
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or(path);
            kind = match action {
                "Add" => "write".to_string(),
                "Delete" => "delete".to_string(),
                _ => "edit".to_string(),
            };
        }
    }

    let raw_summary = match kind.as_str() {
        "edit" | "multiedit" | "apply_patch" | "edit_file" => {
            format!("Edited {}", basename_str(&path))
        }
        "delete" => {
            format!("Deleted {}", basename_str(&path))
        }
        "write" | "write_file" | "write_to_file" => {
            format!("Wrote {}", basename_str(&path))
        }
        "read" | "read_file" | "view_file" => {
            format!("Read {}", basename_str(&path))
        }
        "bash" | "shell" | "exec_command" | "local_shell" | "run_command" => {
            let desc = parsed_args
                .get("description")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let cmd_str = match parsed_args
                .get("command")
                .or_else(|| parsed_args.get("cmd"))
            {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(arr)) => arr
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" "),
                _ => "command".to_string(),
            };
            let label = desc
                .map(|d| d.to_string())
                .unwrap_or_else(|| cap_chars(&strip_base64_cow(&cmd_str), 60));
            format!("Ran {label}")
        }
        "grep" | "glob" | "grep_search" => {
            let pat = parsed_args
                .get("pattern")
                .or_else(|| parsed_args.get("query"))
                .and_then(Value::as_str)
                .unwrap_or("files");
            format!("Searched {pat}")
        }
        "webfetch" => {
            let url = parsed_args
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("url");
            let host = url
                .split("://")
                .last()
                .unwrap_or(url)
                .split('/')
                .next()
                .unwrap_or(url);
            format!("Fetched {host}")
        }
        "websearch" => {
            let q = parsed_args
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or("");
            format!("Searched web {q}")
        }
        "task" | "agent" => {
            let d = parsed_args
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("task");
            format!("Agent: {d}")
        }
        _ => name.to_string(),
    };

    let clean = collapse_whitespace(&strip_base64_in_str(&raw_summary));
    cap_chars(&clean, MAX_TOOL_SUMMARY_CHARS)
}

pub fn append_diff_counts_to_summary(summary: &str, diff: &DiffBlock) -> String {
    static TRAILING_COUNTS_RE: OnceLock<Regex> = OnceLock::new();
    let re = TRAILING_COUNTS_RE
        .get_or_init(|| Regex::new(r" \+\d+ -\d+$").expect("valid diff count regex"));
    let base = re.replace(summary, "");
    cap_chars(
        &format!("{base} +{} -{}", diff.added, diff.removed),
        MAX_TOOL_SUMMARY_CHARS,
    )
}

/// Pure-Rust LCS unified diff body lines with `context` lines around edits (`' '`, `'-'`, `'+'`).
pub fn compute_unified_diff_lines(old: &str, new: &str, context: usize) -> Vec<String> {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    if old_lines == new_lines {
        return Vec::new();
    }

    let n = old_lines.len();
    let m = new_lines.len();

    // Guard quadratic DP blowup on very large edits.
    if n.saturating_mul(m) > 250_000 {
        let mut out = Vec::with_capacity(n + m);
        for l in old_lines {
            out.push(format!("-{l}"));
        }
        for l in new_lines {
            out.push(format!("+{l}"));
        }
        return out;
    }

    let cols = m + 1;
    let mut dp = vec![0u32; (n + 1) * cols];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i * cols + j] = if old_lines[i] == new_lines[j] {
                dp[(i + 1) * cols + (j + 1)] + 1
            } else {
                dp[(i + 1) * cols + j].max(dp[i * cols + (j + 1)])
            };
        }
    }

    #[derive(Clone, Copy)]
    enum Op<'a> {
        Keep(&'a str),
        Del(&'a str),
        Ins(&'a str),
    }

    let mut ops = Vec::with_capacity(n + m);
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if old_lines[i] == new_lines[j] {
            ops.push(Op::Keep(old_lines[i]));
            i += 1;
            j += 1;
        } else if dp[(i + 1) * cols + j] >= dp[i * cols + (j + 1)] {
            ops.push(Op::Del(old_lines[i]));
            i += 1;
        } else {
            ops.push(Op::Ins(new_lines[j]));
            j += 1;
        }
    }
    while i < n {
        ops.push(Op::Del(old_lines[i]));
        i += 1;
    }
    while j < m {
        ops.push(Op::Ins(new_lines[j]));
        j += 1;
    }

    let mut include = vec![false; ops.len()];
    for (idx, op) in ops.iter().enumerate() {
        if !matches!(op, Op::Keep(_)) {
            let start = idx.saturating_sub(context);
            let end = (idx + context + 1).min(ops.len());
            for flag in &mut include[start..end] {
                *flag = true;
            }
        }
    }

    let mut out = Vec::new();
    for (idx, op) in ops.into_iter().enumerate() {
        if !include[idx] {
            continue;
        }
        match op {
            Op::Keep(l) => out.push(format!(" {l}")),
            Op::Del(l) => out.push(format!("-{l}")),
            Op::Ins(l) => out.push(format!("+{l}")),
        }
    }
    out
}

pub fn build_diff_block(path: Option<String>, raw_lines: Vec<String>) -> Option<DiffBlock> {
    if raw_lines.is_empty() {
        return None;
    }
    let added = raw_lines.iter().filter(|l| l.starts_with('+')).count() as u32;
    let removed = raw_lines.iter().filter(|l| l.starts_with('-')).count() as u32;
    let truncated_lines = raw_lines.len().saturating_sub(MAX_DIFF_LINES) as u32;
    let lines = raw_lines
        .into_iter()
        .take(MAX_DIFF_LINES)
        .map(|l| strip_base64_in_str(&l))
        .collect();
    Some(DiffBlock {
        path,
        added,
        removed,
        lines,
        truncated_lines,
    })
}

pub fn extract_image_blocks(text: &str) -> Vec<MessageBlock> {
    static MD_IMG_RE: OnceLock<Regex> = OnceLock::new();
    static URL_IMG_RE: OnceLock<Regex> = OnceLock::new();
    static LOCAL_IMG_RE: OnceLock<Regex> = OnceLock::new();

    let md_re = MD_IMG_RE
        .get_or_init(|| Regex::new(r"!\[([^\]]*)\]\(([^)\s]+)\)").expect("valid md img regex"));
    let url_re = URL_IMG_RE.get_or_init(|| {
        Regex::new(
            r#"(?i)(?:https?://[^\s<>")'\]]+\.(?:png|jpe?g|gif|webp)(?:\?[^\s<>")'\]]*)?|https?://screencast2\.com/[^\s<>")'\]]+|\bscreencast2\.com/[^\s<>")'\]]+)"#,
        )
        .expect("valid url img regex")
    });
    let local_re = LOCAL_IMG_RE.get_or_init(|| {
        Regex::new(r#"(?i)(?:~/\.multibot/uploads/[^\s<>")'\]]+|(?:/[^\s<>")'\]]+)+)\.(?:png|jpe?g|gif|webp)"#)
            .expect("valid local img regex")
    });

    let scrubbed = strip_base64_cow(text);
    let mut blocks = Vec::new();
    let mut seen: HashSet<(Option<String>, Option<String>)> = HashSet::new();
    let mut url_spans: Vec<(usize, usize)> = Vec::new();

    let mut push_image = |url: Option<String>, local_path: Option<String>, alt: String| {
        if blocks.len() >= MAX_IMAGES_PER_MESSAGE {
            return;
        }
        let key = (url.clone(), local_path.clone());
        if seen.insert(key) {
            blocks.push(MessageBlock::Image {
                url,
                local_path,
                alt,
            });
        }
    };

    let starts_with_ignore_ascii_case = |s: &str, prefix: &str| {
        s.len() >= prefix.len()
            && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
    };

    for cap in md_re.captures_iter(&scrubbed) {
        let alt = cap.get(1).map(|m| m.as_str().trim().to_string()).unwrap_or_default();
        let Some(target_m) = cap.get(2) else {
            continue;
        };
        let target = target_m.as_str().trim();
        if target.is_empty() || starts_with_ignore_ascii_case(target, "data:") {
            continue;
        }
        if starts_with_ignore_ascii_case(target, "http://")
            || starts_with_ignore_ascii_case(target, "https://")
        {
            url_spans.push((target_m.start(), target_m.end()));
            push_image(Some(target.to_string()), None, alt);
        } else if starts_with_ignore_ascii_case(target, "screencast2.com/") {
            url_spans.push((target_m.start(), target_m.end()));
            push_image(Some(format!("https://{target}")), None, alt);
        } else if (target.starts_with('/') && !target.starts_with("//")) || target.starts_with("~/")
        {
            push_image(None, Some(target.to_string()), alt);
        }
    }

    for m in url_re.find_iter(&scrubbed) {
        url_spans.push((m.start(), m.end()));
        let raw = m.as_str().trim_end_matches(['.', ',', ';', ':', ')']);
        let normalized = if starts_with_ignore_ascii_case(raw, "http://")
            || starts_with_ignore_ascii_case(raw, "https://")
        {
            raw.to_string()
        } else {
            format!("https://{raw}")
        };
        push_image(Some(normalized), None, String::new());
    }

    for m in local_re.find_iter(&scrubbed) {
        let raw = m.as_str().trim_end_matches(['.', ',', ';', ':', ')']);
        // Never treat protocol-relative `//host/...` as a local path.
        if raw.starts_with("//") {
            continue;
        }
        // Skip any local match that overlaps a previously captured URL span.
        if url_spans
            .iter()
            .any(|&(u_start, u_end)| m.start() < u_end && m.end() > u_start)
        {
            continue;
        }
        // Require start-of-string or a clean whitespace / opening delimiter immediately before.
        let valid_prefix = m.start() == 0
            || scrubbed
                .get(..m.start())
                .and_then(|s| s.chars().next_back())
                .is_some_and(|c| c.is_whitespace() || matches!(c, '(' | '[' | '"' | '\'' | '<'));
        if !valid_prefix {
            continue;
        }
        push_image(None, Some(raw.to_string()), String::new());
    }

    blocks
}

pub fn parse_todo_status(raw: &str) -> TodoStatus {
    match raw.trim().to_ascii_lowercase().as_str() {
        "completed" | "done" => TodoStatus::Completed,
        "in_progress" | "in-progress" | "active" => TodoStatus::InProgress,
        _ => TodoStatus::Pending,
    }
}

pub fn sanitize_todo_content(s: &str) -> String {
    cap_chars(strip_base64_cow(s).trim(), MAX_TODO_CONTENT_CHARS)
}

pub fn parse_prompt_questions(val: &Value) -> Vec<PromptQuestion> {
    let Some(arr) = val.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in arr.iter().take(MAX_QUESTIONS) {
        let Some(question_raw) = item
            .get("question")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let question = strip_base64_in_str(question_raw);
        let header = strip_base64_in_str(
            item.get("header")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim(),
        );
        let multi_select = item
            .get("multiSelect")
            .or_else(|| item.get("multi_select"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut options = Vec::new();
        if let Some(opts) = item.get("options").and_then(Value::as_array) {
            for opt in opts.iter().take(MAX_OPTIONS_PER_QUESTION) {
                if let Some(label_raw) = opt
                    .get("label")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    let label = strip_base64_in_str(label_raw);
                    let description = strip_base64_in_str(
                        opt.get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .trim(),
                    );
                    options.push(PromptOption {
                        label,
                        description,
                    });
                }
            }
        }
        if !options.is_empty() {
            out.push(PromptQuestion {
                question,
                header,
                multi_select,
                options,
            });
        }
    }
    out
}

pub fn strip_system_reminders(text: &str) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    if !text.contains(OPEN) {
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len());
    let mut pos = 0usize;
    while let Some(rel_open) = text[pos..].find(OPEN) {
        let open_idx = pos + rel_open;
        let search_close_from = open_idx + OPEN.len();
        let Some(rel_close) = text[search_close_from..].find(CLOSE) else {
            break;
        };
        let close_end = search_close_from + rel_close + CLOSE.len();

        let before_slice = &text[pos..open_idx];
        let line_start_in_before = before_slice.rfind('\n').map(|i| i + 1).unwrap_or(0);
        let leading_ws_only = before_slice[line_start_in_before..]
            .bytes()
            .all(|b| b == b' ' || b == b'\t');
        let out_has_line_content = if line_start_in_before == 0 {
            let out_line_start = out.rfind('\n').map(|i| i + 1).unwrap_or(0);
            !out[out_line_start..]
                .bytes()
                .all(|b| b == b' ' || b == b'\t')
        } else {
            false
        };

        let after_slice = &text[close_end..];
        let trailing_ws_len = after_slice
            .bytes()
            .take_while(|&b| b == b' ' || b == b'\t')
            .count();
        let after_ws = &after_slice[trailing_ws_len..];
        let trailing_nl_len = if after_ws.starts_with("\r\n") {
            Some(2usize)
        } else if after_ws.starts_with('\n') {
            Some(1usize)
        } else if after_ws.is_empty() {
            Some(0usize)
        } else {
            None
        };

        if leading_ws_only
            && !out_has_line_content
            && let Some(nl_len) = trailing_nl_len
        {
            if line_start_in_before > 0 {
                out.push_str(&before_slice[..line_start_in_before]);
            } else {
                let out_line_start = out.rfind('\n').map(|i| i + 1).unwrap_or(0);
                out.truncate(out_line_start);
            }
            pos = close_end + trailing_ws_len + nl_len;
        } else {
            out.push_str(before_slice);
            pos = close_end;
        }
    }
    out.push_str(&text[pos..]);
    out
}

pub fn strip_tracking_suffix(text: &str) -> (String, Option<String>) {
    static TRACK_RE: OnceLock<Regex> = OnceLock::new();
    let re = TRACK_RE
        .get_or_init(|| Regex::new(r"\n\[mp:([0-9a-f]{16})\]\s*$").expect("valid tracking regex"));
    if let Some(cap) = re.captures(text) {
        let id = cap.get(1).map(|m| m.as_str().to_string());
        let cleaned = re.replace(text, "").into_owned();
        (cleaned, id)
    } else {
        (text.to_string(), None)
    }
}

pub fn extract_text_content(val: &Value) -> String {
    match val {
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                if let Some(s) = item.as_str() {
                    out.push(s.to_string());
                } else if matches!(
                    item.get("type").and_then(Value::as_str),
                    Some("text" | "output_text" | "input_text")
                ) && let Some(t) = item.get("text").and_then(Value::as_str)
                {
                    out.push(t.to_string());
                }
            }
            out.join("\n")
        }
        Value::Object(map) => map
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

pub fn is_safe_transcript_path(path: &Path, root: &Path) -> bool {
    let Ok(canon_root) = fs::canonicalize(root) else {
        return false;
    };
    let Ok(canon_path) = fs::canonicalize(path) else {
        return false;
    };
    if !canon_path.is_file() {
        return false;
    }
    if canon_path.starts_with(&canon_root) {
        return true;
    }
    // Allow symlink living inside root pointing to regular file with matching filename.
    if fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
        && let Some(parent) = path.parent()
        && let Ok(canon_parent) = fs::canonicalize(parent)
        && canon_parent.starts_with(&canon_root)
        && path.file_name() == canon_path.file_name()
    {
        return true;
    }
    false
}

pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }
    let counter = ATOMIC_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_name = format!(
        "{}.tmp.{}.{}",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("state"),
        std::process::id(),
        counter
    );
    let tmp_path = path.with_file_name(tmp_name);

    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }

    let write_res = (|| -> Result<()> {
        let mut file = opts.open(&tmp_path)?;
        let data = serde_json::to_vec_pretty(value)?;
        file.write_all(&data)?;
        file.sync_all()?;
        Ok(())
    })();

    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }
    if let Err(err) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err.into());
    }
    Ok(())
}

pub fn parse_iso_ms(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

pub fn system_time_ms(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn now_ms() -> i64 {
    system_time_ms(SystemTime::now())
}

pub fn truncate_title(s: &str, max_chars: usize) -> String {
    let clean = collapse_whitespace(&strip_base64_cow(s));
    cap_chars(&clean, max_chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pi_cursor_metadata_and_connected_sessions_survive_active_pruning() {
        let home = unique_temp_dir("pi-cursor-held");
        let pi = home.join(".pi/agent/sessions/repo/date_pi-live.jsonl");
        let cursor = home.join(".cursor/projects/repo/agent-transcripts/cursor-live.jsonl");
        fs::create_dir_all(pi.parent().unwrap()).unwrap();
        fs::create_dir_all(cursor.parent().unwrap()).unwrap();
        fs::write(
            &pi,
            "{\"type\":\"session\",\"id\":\"pi-live\",\"cwd\":\"/repo\"}\n",
        )
        .unwrap();
        fs::write(
            &cursor,
            "{\"role\":\"user\",\"message\":{\"content\":\"hi\"}}\n",
        )
        .unwrap();
        let none = HashSet::new();
        let connected = HashSet::from(["pi-live".to_string(), "cursor-live".to_string()]);
        let mut metadata = MetadataStore::default();
        let archive = ArchiveStore::default();
        let live = LiveActivity::default();
        let workspaces = ServerWorkspaceIndex::default();
        let ctx = DiscoveryContext {
            active_session_ids: &none,
            connected_session_ids: &connected,
            metadata: &metadata,
            archive: &archive,
            live: &live,
            include_archived: false,
            server_workspaces: &workspaces,
        };
        assert_eq!(discover_all_sessions_with_ctx(&home, 10, &ctx).len(), 2);
        for (id, agent, source) in [
            ("pi-live", AgentKind::Pi, &pi),
            ("cursor-live", AgentKind::Cursor, &cursor),
        ] {
            metadata.sessions.insert(
                id.into(),
                SessionMetaEntry {
                    agent: Some(agent),
                    source_path: Some(source.to_string_lossy().into()),
                    ..Default::default()
                },
            );
        }
        let ctx = DiscoveryContext {
            active_session_ids: &none,
            connected_session_ids: &none,
            metadata: &metadata,
            archive: &archive,
            live: &live,
            include_archived: false,
            server_workspaces: &workspaces,
        };
        assert_eq!(discover_all_sessions_with_ctx(&home, 10, &ctx).len(), 2);
        fs::remove_dir_all(home).unwrap();
    }

    fn unique_temp_dir(tag: &str) -> PathBuf {
        let n = ATOMIC_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "multibotd-mod-test-{tag}-{}-{n}",
            std::process::id()
        ));
        let _ = fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn cap_chars_is_utf8_safe_for_emojis_and_cjk() {
        let s = "🚀日本語テスト🎉";
        assert_eq!(cap_chars(s, 4), "🚀日本…");
        assert_eq!(cap_chars(s, 20), s);
    }

    #[test]
    fn strip_base64_and_clean_tool_value_scrubs_images() {
        let long_b64 = "A".repeat(300);
        let text = format!("prefix data:image/png;base64,{long_b64} suffix");
        assert_eq!(strip_base64_in_str(&text), "prefix [image omitted] suffix");

        let obj = json!({
            "type": "image",
            "data": long_b64,
        });
        assert_eq!(clean_tool_value(&obj), Value::String("[image omitted]".into()));

        let nested = json!({
            "base64": "should_drop",
            "note": format!("raw:{long_b64}"),
        });
        let cleaned = clean_tool_value(&nested);
        assert!(cleaned.get("base64").is_none());
        assert_eq!(cleaned["note"], "raw:[image omitted]");
    }

    #[test]
    fn unified_diff_and_40_line_cap() {
        let old = (1..=60).map(|i| format!("old_{i}")).collect::<Vec<_>>().join("\n");
        let new = (1..=60).map(|i| format!("new_{i}")).collect::<Vec<_>>().join("\n");
        let lines = compute_unified_diff_lines(&old, &new, 1);
        let diff = build_diff_block(Some("src/lib.rs".into()), lines).unwrap();
        assert_eq!(diff.added, 60);
        assert_eq!(diff.removed, 60);
        assert_eq!(diff.lines.len(), MAX_DIFF_LINES);
        assert_eq!(diff.truncated_lines, 80);
    }

    #[test]
    fn bounded_pagination_utf8_boundaries_and_known_probe() {
        let dir = unique_temp_dir("pagination");
        let path = dir.join("chat.jsonl");
        let mut content = String::new();
        for i in 0..30 {
            content.push_str(&format!(
                r#"{{"type":"user","uuid":"u{i}","timestamp":"2026-11-26T10:00:{:02}Z","message":{{"content":"Message {i} 🚀 日本語 payload padding for boundary test"}}}}"#,
                i % 60
            ));
            content.push('\n');
        }
        fs::write(&path, &content).unwrap();

        let mut collected = Vec::new();
        let mut before: Option<u64> = None;
        let mut probe: Option<String> = None;
        let mut pages = 0usize;

        loop {
            let page = read_history_page(
                AgentKind::Claude,
                &path,
                before,
                Some(512),
                None,
                probe.as_deref(),
            )
            .unwrap();
            pages += 1;
            assert!(pages <= 30, "pagination did not terminate");
            assert_eq!(page.probe_sha256.len(), 64);

            // Prepend older page messages
            let mut combined = page.messages;
            combined.extend(collected);
            collected = combined;

            if !page.has_more {
                assert_eq!(page.oldest_offset, 0);
                break;
            }
            assert!(page.oldest_offset > 0);
            before = Some(page.oldest_offset);
            let mut f = File::open(&path).unwrap();
            probe = Some(compute_probe_sha256(&mut f, page.oldest_offset).unwrap());
        }

        assert!(pages >= 3);
        assert_eq!(collected.len(), 30);
        for (i, msg) in collected.iter().enumerate() {
            assert_eq!(msg.id, format!("u{i}"));
            assert!(msg.text.contains("🚀 日本語"));
        }

        // Stale known_probe with before_offset falls back safely to fresh tail
        let fallback = read_history_page(
            AgentKind::Claude,
            &path,
            Some(500),
            Some(512),
            None,
            Some("0000000000000000000000000000000000000000000000000000000000000000"),
        )
        .unwrap();
        assert_eq!(fallback.latest_offset, content.len() as u64);
        assert_eq!(fallback.messages.last().unwrap().id, "u29");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn giant_single_line_doubles_window_without_stalling() {
        let dir = unique_temp_dir("giant-line");
        let path = dir.join("giant.jsonl");
        let big = "x".repeat(12_000);
        let line = format!(
            r#"{{"type":"user","uuid":"big1","timestamp":"2026-11-26T10:00:00Z","message":{{"content":"{big}"}}}}"#
        );
        fs::write(&path, format!("{line}\n")).unwrap();

        let page = read_history_page(AgentKind::Claude, &path, None, Some(1024), None, None).unwrap();
        assert_eq!(page.messages.len(), 1);
        assert_eq!(page.messages[0].id, "big1");
        assert_eq!(page.oldest_offset, 0);
        assert!(!page.has_more);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn partial_trailing_line_and_truncation_reset_in_delta() {
        let dir = unique_temp_dir("delta");
        let path = dir.join("stream.jsonl");
        let line1 = r#"{"type":"user","uuid":"u1","message":{"content":"first"}}"#;
        fs::write(&path, format!("{line1}\n")).unwrap();

        let page = read_history_page(AgentKind::Claude, &path, None, None, None, None).unwrap();
        assert_eq!(page.messages.len(), 1);
        let off1 = page.latest_offset;
        let probe1 = page.probe_sha256.clone();

        // Append partial line without '\n'
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(br#"{"type":"assistant","uuid":"a1","message":{"content":"sec"#).unwrap();
        f.flush().unwrap();

        let d1 = read_history_delta(AgentKind::Claude, &path, off1, Some(&probe1)).unwrap();
        assert!(matches!(d1, DeltaRead::Unchanged { offset, .. } if offset == off1));

        // Complete the line with '\n'
        f.write_all(b"ond\"}}\n").unwrap();
        f.flush().unwrap();

        let d2 = read_history_delta(AgentKind::Claude, &path, off1, Some(&probe1)).unwrap();
        let (off2, probe2) = match d2 {
            DeltaRead::Appended {
                messages,
                latest_offset,
                probe_sha256,
                ..
            } => {
                assert_eq!(messages.len(), 1);
                assert_eq!(messages[0].id, "a1");
                assert_eq!(messages[0].text, "second");
                (latest_offset, probe_sha256)
            }
            other => panic!("expected Appended, got {other:?}"),
        };

        // Rewrite file in place with longer content (different bytes at off2 -> probe mismatch)
        fs::write(
            &path,
            format!(
                "{}\n{}\n{}\n",
                r#"{"type":"user","uuid":"r1","message":{"content":"rotated line 1 with different bytes"}}"#,
                r#"{"type":"assistant","uuid":"r2","message":{"content":"rotated line 2 with different bytes"}}"#,
                r#"{"type":"user","uuid":"r3","message":{"content":"rotated line 3"}}"#
            ),
        )
        .unwrap();

        let d3 = read_history_delta(AgentKind::Claude, &path, off2, Some(&probe2)).unwrap();
        assert!(matches!(d3, DeltaRead::ResetRequired { .. }));

        let _ = fs::remove_dir_all(&dir);
    }

    /// Writes `count` Claude transcripts, oldest first, one per minute going back in time.
    fn seed_claude_transcripts(dir: &Path, prefix: &str, count: usize, first_age_minutes: i64) {
        let root = dir.join(".claude").join("projects").join("-repo");
        fs::create_dir_all(&root).unwrap();
        for i in 0..count {
            let age_minutes = first_age_minutes + i as i64;
            let path = root.join(format!("{prefix}-{i}.jsonl"));
            let body = format!(
                "{{\"type\":\"user\",\"uuid\":\"u-{prefix}-{i}\",\"cwd\":\"/repo\",\"message\":{{\"content\":\"prompt {i}\"}}}}\n"
            );
            fs::write(&path, body).unwrap();
            set_age(&path, age_minutes * 60);
        }
    }

    /// Writes one Claude transcript with a caller-chosen session id.
    fn seed_claude_transcript_named(dir: &Path, sid: &str, tag: &str, seconds_ago: i64) {
        let root = dir.join(".claude").join("projects").join("-repo");
        fs::create_dir_all(&root).unwrap();
        let path = root.join(format!("{sid}.jsonl"));
        fs::write(
            &path,
            format!(
                "{{\"type\":\"user\",\"uuid\":\"u-{tag}\",\"cwd\":\"/repo\",\"message\":{{\"content\":\"{tag}\"}}}}\n"
            ),
        )
        .unwrap();
        set_age(&path, seconds_ago);
    }

    /// Backdates `path` so tests can place a transcript anywhere on the timeline.
    fn set_age(path: &Path, seconds_ago: i64) {
        let when = SystemTime::now() - std::time::Duration::from_secs(seconds_ago.max(0) as u64);
        let file = OpenOptions::new().write(true).open(path).unwrap();
        let _ = file.set_times(std::fs::FileTimes::new().set_modified(when));
    }

    /// A legacy host record the read-only activity index can see. The pid is this test
    /// process, which is alive for as long as the check runs.
    fn write_live_chat(dir: &Path, sid: &str, tmux: &str, seconds_ago: i64) {
        let host = dir
            .join(".local")
            .join("state")
            .join("multiprompt")
            .join("agent-host");
        fs::create_dir_all(&host).unwrap();
        let stem = &sha256_hex(format!("claude:{sid}").as_bytes())[..32];
        let config = host.join(format!("{stem}.config.json"));
        fs::write(
            &config,
            format!(r#"{{"enabled":true,"provider":"claude","sid":"{sid}","tmux":"{tmux}"}}"#),
        )
        .unwrap();
        let state = host.join(format!("{stem}.state.json"));
        let pid = std::process::id();
        fs::write(
            &state,
            format!(r#"{{"pid":{pid},"sid":"{sid}","provider":"claude","tmux":"{tmux}"}}"#),
        )
        .unwrap();
        set_age(&config, seconds_ago);
        set_age(&state, seconds_ago);
    }

    #[test]
    fn default_discovery_hides_dead_history_even_with_1200_stale_transcripts() {
        let dir = unique_temp_dir("active-only");
        // 1,200 dead transcripts: months old and named by nothing.
        seed_claude_transcripts(&dir, "dead", 1200, 90 * 24 * 60);
        // 50 chats a person is working in, all with months-old transcripts, so recency can
        // not be what keeps them.
        let mut expected_live = Vec::new();
        let live_percent = |tag: &str, i: usize| format!("{tag}{i:07}-1111-2222-3333-444444444444");
        for i in 0..30 {
            let sid = live_percent("a", i);
            seed_claude_transcript_named(&dir, &sid, &format!("live-{i}"), 4 * 86_400);
            write_live_chat(&dir, &sid, &format!("live-claude-{i}"), 4 * 86_400);
            expected_live.push(sid);
        }
        for i in 0..20 {
            let sid = live_percent("b", i);
            seed_claude_transcript_named(&dir, &sid, &format!("live-old-{i}"), 2 * 86_400);
            write_live_chat(&dir, &sid, &format!("live-old-{i}"), 2 * 86_400);
            expected_live.push(sid);
        }

        let started = now_ms();
        let sessions = discover_all_sessions_owned(&dir, 100, false);
        let elapsed = now_ms() - started;

        assert_eq!(
            sessions.len(),
            50,
            "expected the 50 active sessions, got {}: {:?}",
            sessions.len(),
            sessions.iter().map(|s| &s.id).collect::<Vec<_>>()
        );
        let ids: HashSet<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        for sid in &expected_live {
            assert!(
                ids.contains(sid.as_str()),
                "active session {sid} missing from default list"
            );
        }
        assert!(
            !sessions.iter().any(|s| s.title.starts_with("prompt")),
            "a stale transcript leaked into the default list"
        );
        assert!(
            elapsed < 2_000,
            "default discovery over 1,250 transcripts took {elapsed} ms"
        );

        // An explicit full listing still returns the history when asked for it.
        let everything = discover_all_sessions_owned(&dir, 2_000, true);
        assert_eq!(
            everything.len(),
            1_250,
            "include_archived should see every transcript, got {}",
            everything.len()
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// Writes one Antigravity transcript the way the CLI lays it out: the conversation uuid
    /// is a folder and the file is always named `transcript.jsonl`.
    fn seed_antigravity_transcript(dir: &Path, conversation: &str, tag: &str, seconds_ago: i64) {
        let path = dir
            .join(".gemini")
            .join("antigravity")
            .join("brain")
            .join(conversation)
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            format!(
                "{{\"type\":\"USER_INPUT\",\"conversation_id\":\"{conversation}\",\"content\":\"{tag}\"}}\n"
            ),
        )
        .unwrap();
        set_age(&path, seconds_ago);
    }

    /// A legacy host record for an Antigravity conversation, which publishes its id under
    /// `conversation` in `chat-names.json`.
    fn write_live_antigravity_chat(dir: &Path, conversation: &str) {
        let state_root = dir.join(".local").join("state").join("multiprompt");
        let host = state_root.join("agent-host");
        fs::create_dir_all(&host).unwrap();
        let stem = &sha256_hex(format!("antigravity:{conversation}").as_bytes())[..32];
        let pid = std::process::id();
        fs::write(
            host.join(format!("{stem}.config.json")),
            format!(r#"{{"enabled":true,"provider":"antigravity","sid":"{conversation}"}}"#),
        )
        .unwrap();
        fs::write(
            host.join(format!("{stem}.state.json")),
            format!(r#"{{"pid":{pid},"provider":"antigravity","conversation":"{conversation}"}}"#),
        )
        .unwrap();
        fs::write(
            state_root.join("chat-names.json"),
            format!(
                r#"{{"live-agy":{{"name":"Multi Bot Rust","conversation":"{conversation}"}}}}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn antigravity_conversation_folder_matches_a_live_chat() {
        let dir = unique_temp_dir("agy-live-match");
        let live = "4d5d5535-249c-4f10-90d9-7ec49c2eeb53";
        let dead = "0badc0de-249c-4f10-90d9-7ec49c2eeb53";
        seed_antigravity_transcript(&dir, live, "live agy", 30 * 86_400);
        seed_antigravity_transcript(&dir, dead, "dead agy", 30 * 86_400);
        write_live_antigravity_chat(&dir, live);

        let sessions = discover_all_sessions_owned(&dir, 50, false);
        let ids: HashSet<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert!(
            ids.contains(live),
            "live Antigravity conversation missing: {ids:?}"
        );
        assert!(
            !ids.contains(dead),
            "an ended Antigravity conversation leaked: {ids:?}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn archived_multibot_hosted_antigravity_chat_leaves_the_default_list() {
        let dir = unique_temp_dir("agy-archived");
        let sid = "7a1c9e2d-1111-4222-8333-944455556666";
        let live = "4d5d5535-249c-4f10-90d9-7ec49c2eeb53";

        // Two Multibot-hosted chats: one stays open, one gets archived.
        for (id, title) in [(sid, "Archived agy"), (live, "Open agy")] {
            let path = antigravity::hosted_history_path(&dir, id);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(
                &path,
                format!(
                    "{{\"type\":\"multibot_meta\",\"session_id\":\"{id}\",\"agent\":\"antigravity\",\"cwd\":\"/repo\",\"title\":\"{title}\"}}\n"
                ),
            )
            .unwrap();
        }

        let mut meta = MetadataStore::default();
        for id in [sid, live] {
            meta.sessions.insert(
                id.to_string(),
                SessionMetaEntry {
                    title: None,
                    cwd: Some("/repo".into()),
                    agent: Some(AgentKind::Antigravity),
                    source_path: Some(
                        antigravity::hosted_history_path(&dir, id)
                            .to_string_lossy()
                            .to_string(),
                    ),
                    native_session_id: None,
                    updated_at_ms: 1,
                },
            );
        }
        meta.save(&dir).unwrap();

        // Before archiving, both chats are listed.
        let before = discover_all_sessions_owned(&dir, 50, false);
        let ids: HashSet<&str> = before.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(sid) && ids.contains(live), "both chats should list: {ids:?}");

        // Archiving keeps `metadata.json`, so the id stays held open. It must still leave
        // the default list, and stay reachable only with `include_archived`.
        let mut archive = ArchiveStore::default();
        archive.archived.insert(
            sid.to_string(),
            ArchivedEntry {
                archived_at_ms: 2,
                agent: Some(AgentKind::Antigravity),
                source_path: Some(
                    antigravity::hosted_history_path(&dir, sid)
                        .to_string_lossy()
                        .to_string(),
                ),
            },
        );
        archive.save(&dir).unwrap();

        let after = discover_all_sessions_owned(&dir, 50, false);
        let ids: HashSet<&str> = after.iter().map(|s| s.id.as_str()).collect();
        assert!(
            !ids.contains(sid),
            "an archived Multibot-hosted Antigravity chat leaked into the default list: {ids:?}"
        );
        assert!(ids.contains(live), "the open chat must stay listed: {ids:?}");

        let all = discover_all_sessions_owned(&dir, 50, true);
        let ids: HashSet<&str> = all.iter().map(|s| s.id.as_str()).collect();
        assert!(
            ids.contains(sid),
            "an archived chat must stay reachable with include_archived: {ids:?}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_custom_chat_names_win_over_the_transcript_title_but_lose_to_a_manual_rename() {
        let dir = unique_temp_dir("legacy-names");
        let state_root = dir.join(".local").join("state").join("multiprompt");
        // One live chat per provider, each named in the legacy app.
        let claude = "6f353a80-d3df-4e44-88aa-edcc6c2b8999";
        let agy = "cfcfd749-fda1-40bb-b70b-0b3db1c69b2f";
        let codex_sid = "9738e6e1-a533-4db7-823d-3e04af268ccb";
        let codex_rollout = "01a213db-1d2a-7783-8245-01b6e818b48a";
        seed_claude_transcript_named(&dir, claude, "first prompt of the claude chat", 60);
        write_live_chat(&dir, claude, "live-8bc7bac", 60);
        seed_antigravity_transcript(&dir, agy, "first prompt of the agy chat", 60);
        write_live_antigravity_chat(&dir, agy);
        let rollout = dir
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("11")
            .join("26")
            .join(format!("rollout-2026-11-26-{codex_rollout}.jsonl"));
        fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        fs::write(
            &rollout,
            format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{codex_rollout}\"}}}}\n"),
        )
        .unwrap();
        set_age(&rollout, 60);
        let host = state_root.join("agent-host");
        fs::create_dir_all(&host).unwrap();
        let stem = &sha256_hex(format!("codex:{codex_sid}").as_bytes())[..32];
        fs::write(
            host.join(format!("{stem}.config.json")),
            format!(
                r#"{{"enabled":true,"provider":"codex","sid":"{codex_sid}","tmux":"live-4b135e3"}}"#
            ),
        )
        .unwrap();
        fs::write(
            host.join(format!("{stem}.state.json")),
            format!(
                r#"{{"pid":{},"sid":"{codex_sid}","provider":"codex","native_session":"{codex_rollout}","tmux":"live-4b135e3"}}"#,
                std::process::id()
            ),
        )
        .unwrap();
        // Antigravity's own helper already wrote a `chat-names.json`, so rewrite it once with
        // every chat this test needs. Both legacy files stay read-only from here on.
        let chat_names = state_root.join("chat-names.json");
        fs::write(
            &chat_names,
            format!(
                concat!(
                    r#"{{"live-8bc7bac":{{"name":"Poteto","sid":"{claude}"}},"#,
                    r#""live-agy":{{"name":"HT AGY Lead","conversation":"{agy}"}},"#,
                    r#""live-4b135e3":{{"name":"HT Features","sid":"{codex_sid}"}}}}"#,
                ),
                claude = claude,
                agy = agy,
                codex_sid = codex_sid,
            ),
        )
        .unwrap();
        let names_before = fs::read(&chat_names).unwrap();

        let sessions = discover_all_sessions_owned(&dir, 50, false);
        let title_of = |id: &str| {
            sessions
                .iter()
                .find(|s| s.id == id)
                .map(|s| s.title.clone())
                .unwrap_or_else(|| panic!("session {id} missing: {sessions:?}"))
        };
        assert_eq!(title_of(claude), "Poteto", "Claude sid");
        assert_eq!(title_of(agy), "HT AGY Lead", "Antigravity conversation");
        assert_eq!(
            title_of(codex_rollout),
            "HT Features",
            "Codex native_session rollout id"
        );

        // A rename saved in Multibot's own metadata outranks the legacy name.
        let mut metadata = MetadataStore::load(&dir);
        metadata.sessions.insert(
            claude.to_string(),
            SessionMetaEntry {
                title: Some("Renamed In Multibot".to_string()),
                agent: Some(AgentKind::Claude),
                updated_at_ms: now_ms(),
                ..Default::default()
            },
        );
        metadata.save(&dir).unwrap();
        let sessions = discover_all_sessions_owned(&dir, 50, false);
        assert_eq!(
            sessions
                .iter()
                .find(|s| s.id == claude)
                .map(|s| s.title.as_str()),
            Some("Renamed In Multibot")
        );

        // Invariant #2: reading the legacy state never writes to it.
        assert_eq!(fs::read(&chat_names).unwrap(), names_before);
        assert!(!dir.join(".config/ZigShell").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_rollout_name_matches_a_live_tupleid() {
        let dir = unique_temp_dir("codex-live-match");
        let live = "11111111-2222-3333-4444-555555555555";
        let dead = "99999999-2222-3333-4444-555555555555";
        for (sid, seconds_ago) in [(live, 40 * 86_400), (dead, 40 * 86_400)] {
            let path = dir
                .join(".codex")
                .join("sessions")
                .join("2026")
                .join("11")
                .join("26")
                .join(format!("rollout-2026-11-26-{sid}.jsonl"));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(
                &path,
                format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{sid}\"}}}}\n"),
            )
            .unwrap();
            set_age(&path, seconds_ago);
        }
        write_live_chat(&dir, live, "live-codex", 40 * 86_400);

        let sessions = discover_all_sessions_owned(&dir, 50, false);
        let ids: HashSet<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(live), "live Codex session missing: {ids:?}");
        assert!(!ids.contains(dead), "an ended Codex session leaked: {ids:?}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn archiving_and_new_sessions_update_the_default_list_immediately() {
        let dir = unique_temp_dir("archive-flip");
        let sid = "ddddddd1-1111-2222-3333-444444444444";
        seed_claude_transcript_named(&dir, sid, "archive me", 120);
        write_live_chat(&dir, sid, "live-archive-me", 120);

        let sessions = discover_all_sessions_owned(&dir, 50, false);
        assert_eq!(sessions.len(), 1, "live session must be listed: {sessions:?}");

        // Archiving through the daemon's own store drops it from the default list.
        let mut archive = ArchiveStore::default();
        archive.archived.insert(
            sid.to_string(),
            ArchivedEntry {
                archived_at_ms: now_ms(),
                agent: Some(AgentKind::Claude),
                source_path: None,
            },
        );
        archive.save(&dir).unwrap();
        assert!(
            discover_all_sessions_owned(&dir, 50, false).is_empty(),
            "archived session must leave the default list"
        );
        assert_eq!(
            discover_all_sessions_owned(&dir, 50, true).len(),
            1,
            "archived session must stay reachable when explicitly requested"
        );

        // Unarchiving restores it.
        ArchiveStore::default().save(&dir).unwrap();
        assert_eq!(discover_all_sessions_owned(&dir, 50, false).len(), 1);

        // A chat spawned through Multibot is live through its metadata record, even before the
        // agent CLI wrote a first transcript byte.
        let fresh = "eeeeeee2-1111-2222-3333-444444444444";
        let mut meta = MetadataStore::load(&dir);
        meta.sessions.insert(
            fresh.to_string(),
            SessionMetaEntry {
                title: Some("New chat".into()),
                cwd: Some("/repo".into()),
                agent: Some(AgentKind::Claude),
                source_path: Some(
                    dir.join(".claude/projects/-repo")
                        .join(format!("{fresh}.jsonl"))
                        .to_string_lossy()
                        .to_string(),
                ),
                native_session_id: None,
                updated_at_ms: now_ms(),
            },
        );
        meta.save(&dir).unwrap();
        let sessions = discover_all_sessions_owned(&dir, 50, false);
        assert!(
            sessions.iter().any(|s| s.id == fresh),
            "a chat this daemon spawned must appear at once: {sessions:?}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn connected_sessions_stay_visible_with_no_live_legacy_record() {
        let dir = unique_temp_dir("connected-idle");
        let sid = "abababa4-1111-2222-3333-444444444444";
        seed_claude_transcript_named(&dir, sid, "idle connected", 45 * 86_400);

        // Nothing points at it: history, so it stays out.
        assert!(discover_all_sessions_owned(&dir, 50, false).is_empty());

        // The daemon holds it open and idle: it must be listed, but not as `working`.
        let metadata = MetadataStore::load(&dir);
        let archive = ArchiveStore::load(&dir);
        let live = LiveActivity::load(&dir);
        let busy = HashSet::new();
        let connected: HashSet<String> = [sid.to_string()].into_iter().collect();
        let server_workspaces = ServerWorkspaceIndex::default();
        let ctx = DiscoveryContext {
            active_session_ids: &busy,
            connected_session_ids: &connected,
            metadata: &metadata,
            archive: &archive,
            live: &live,
            include_archived: false,
            server_workspaces: &server_workspaces,
        };
        let sessions = discover_all_sessions_with_ctx(&dir, 50, &ctx);
        assert_eq!(sessions.len(), 1, "idle connected chat must be listed");
        assert!(!sessions[0].working, "idle chat must not report working");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_zigshell_archive_hides_chats_without_touching_its_file() {
        let dir = unique_temp_dir("legacy-archive");
        let sid = "fffffff3-1111-2222-3333-444444444444";
        seed_claude_transcript_named(&dir, sid, "legacy archived", 60);
        let legacy = dir.join(".config").join("ZigShell").join("archive.json");
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        let body = format!(
            r#"[{{"name":"Old","tmux_session":"live-legacy","csid":"{sid}","host":0,"ai_agent":1}}]"#
        );
        fs::write(&legacy, &body).unwrap();

        let sessions = discover_all_sessions_owned(&dir, 50, false);
        assert!(
            sessions.is_empty(),
            "legacy-archived chat must stay out of the default list: {sessions:?}"
        );
        assert_eq!(
            fs::read_to_string(&legacy).unwrap(),
            body,
            "the legacy archive must be read-only"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn semantic_working_rules() {
        let now = 100_000;
        // Hosted PID always overrides to working
        assert!(compute_semantic_working(true, TailActivity::Idle, 0, now));
        // Unanswered user/tool within 30s -> working; older than 30s -> stale false
        assert!(compute_semantic_working(false, TailActivity::Working, now - 5_000, now));
        assert!(!compute_semantic_working(false, TailActivity::Working, now - 40_000, now));
        // Completed assistant or pending question -> not working
        assert!(!compute_semantic_working(false, TailActivity::Idle, now - 500, now));
        assert!(!compute_semantic_working(false, TailActivity::AwaitingUserInput, now - 500, now));
    }

    #[test]
    fn metadata_and_archive_stores_persist_atomically() {
        let dir = unique_temp_dir("stores");
        let mut meta = MetadataStore::default();
        meta.sessions.insert(
            "s1".into(),
            SessionMetaEntry {
                title: Some("Custom Name".into()),
                cwd: Some("/repo".into()),
                agent: Some(AgentKind::Claude),
                source_path: Some("/tmp/s1.jsonl".into()),
                native_session_id: None,
                updated_at_ms: 1234,
            },
        );
        meta.save(&dir).unwrap();
        let loaded_meta = MetadataStore::load(&dir);
        assert_eq!(
            loaded_meta.get("s1").and_then(|e| e.title.as_deref()),
            Some("Custom Name")
        );

        let mut arch = ArchiveStore::default();
        arch.archived.insert(
            "s1".into(),
            ArchivedEntry {
                archived_at_ms: 5678,
                agent: Some(AgentKind::Claude),
                source_path: Some("/tmp/s1.jsonl".into()),
            },
        );
        arch.save(&dir).unwrap();
        let loaded_arch = ArchiveStore::load(&dir);
        assert!(loaded_arch.is_archived("s1"));
        assert!(!loaded_arch.is_archived("s2"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_image_blocks_does_not_emit_bogus_local_path_for_http_urls() {
        let text = "See https://example.com/screen.png and https://cdn.example.io:8080/shot.png and /tmp/local.png";
        let blocks = extract_image_blocks(text);
        assert_eq!(blocks.len(), 3, "expected 2 URL blocks + 1 local block, got {blocks:?}");

        let urls: Vec<&str> = blocks
            .iter()
            .filter_map(|b| match b {
                MessageBlock::Image { url: Some(u), local_path: None, .. } => Some(u.as_str()),
                _ => None,
            })
            .collect();
        let locals: Vec<&str> = blocks
            .iter()
            .filter_map(|b| match b {
                MessageBlock::Image { url: None, local_path: Some(p), .. } => Some(p.as_str()),
                _ => None,
            })
            .collect();

        assert_eq!(
            urls,
            vec![
                "https://example.com/screen.png",
                "https://cdn.example.io:8080/shot.png"
            ]
        );
        assert_eq!(locals, vec!["/tmp/local.png"]);
        assert!(locals.iter().all(|p| !p.starts_with("//")));
    }

    #[test]
    fn strip_base64_scrubs_short_data_image_uris_and_long_raw_blobs() {
        // 1x1 transparent PNG (~92 base64 chars — far below 256-char bare threshold)
        let tiny_png = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
        let input = format!("Before {tiny_png} after");
        assert_eq!(strip_base64_in_str(&input), "Before [image omitted] after");

        // Short non-image alphanumeric string must NOT be scrubbed
        let normal = "token_abc1234567890_normal_text";
        assert_eq!(strip_base64_in_str(normal), normal);
    }

    #[test]
    fn strip_system_reminders_preserves_text_between_multiple_tags() {
        let same_line =
            "<system-reminder>rem1</system-reminder> KEEP_SAME_LINE <system-reminder>rem2</system-reminder>\n";
        assert_eq!(strip_system_reminders(same_line).trim(), "KEEP_SAME_LINE");

        let multi_line =
            "<system-reminder>rem1</system-reminder> KEEP_LINE_1\nKEEP_LINE_2\n<system-reminder>rem2</system-reminder>\n";
        assert_eq!(
            strip_system_reminders(multi_line).trim(),
            "KEEP_LINE_1\nKEEP_LINE_2"
        );

        let standalone = "top\n  <system-reminder>\nhidden\n</system-reminder>  \r\nbottom";
        assert_eq!(strip_system_reminders(standalone), "top\nbottom");
    }

    #[test]
    fn read_history_page_never_stalls_on_line_exceeding_hard_max_bytes() {
        let dir = unique_temp_dir("giant-5mb");
        let path = dir.join("giant5m.jsonl");

        let line1 = "{\"type\":\"user\",\"uuid\":\"small1\",\"message\":{\"content\":\"before giant\"}}\n";
        assert_eq!(line1.len(), 69);

        // ~5.2 MiB line (> HARD_HISTORY_MAX_BYTES = 4 MiB)
        let mut line2 = String::with_capacity(5_200_128);
        line2.push_str(r#"{"type":"user","uuid":"giant5m","message":{"content":""#);
        for _ in 0..2_600_000 {
            line2.push_str("x ");
        }
        line2.push_str("\"}}\n");
        let total_size = (line1.len() + line2.len()) as u64;
        assert!(line2.len() > HARD_HISTORY_MAX_BYTES);

        {
            let mut f = File::create(&path).unwrap();
            f.write_all(line1.as_bytes()).unwrap();
            f.write_all(line2.as_bytes()).unwrap();
            f.flush().unwrap();
        }

        // Page 1: must advance strictly past the >4 MiB line to offset 69 (NOT stall at total_size!)
        let page1 = read_history_page(
            AgentKind::Claude,
            &path,
            None,
            Some(DEFAULT_HISTORY_MAX_BYTES),
            None,
            None,
        )
        .unwrap();
        assert_eq!(page1.oldest_offset, 69);
        assert!(page1.oldest_offset < total_size);
        assert!(page1.has_more);

        // Page 2: paginating before offset 69 reaches line 1 (`small1`) cleanly
        let page2 = read_history_page(
            AgentKind::Claude,
            &path,
            Some(page1.oldest_offset),
            Some(DEFAULT_HISTORY_MAX_BYTES),
            None,
            None,
        )
        .unwrap();
        assert_eq!(page2.oldest_offset, 0);
        assert!(!page2.has_more);
        assert_eq!(page2.messages.len(), 1);
        assert_eq!(page2.messages[0].id, "small1");
        assert_eq!(page2.messages[0].text, "before giant");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn format_tool_summary_maps_apply_patch_delete_to_deleted() {
        let patch = "*** Begin Patch\n*** Delete File: src/old.rs\n*** End Patch";
        assert_eq!(
            format_tool_summary("apply_patch", &json!({ "input": patch })),
            "Deleted old.rs"
        );
    }

    #[test]
    fn read_history_delta_skips_complete_single_line_over_4mib_and_keeps_incomplete_partial() {
        let dir = unique_temp_dir("delta-giant-4mib");
        let path = dir.join("giant_delta.jsonl");

        let line1 = "{\"type\":\"user\",\"uuid\":\"u1\",\"message\":{\"content\":\"initial\"}}\n";
        fs::write(&path, line1).unwrap();

        let page = read_history_page(AgentKind::Claude, &path, None, None, None, None).unwrap();
        assert_eq!(page.messages.len(), 1);
        let prev_offset = page.latest_offset;
        let prev_probe = page.probe_sha256.clone();
        assert_eq!(prev_offset, line1.len() as u64);

        // (a) Append an incomplete 4.5 MiB line (NO '\n' yet) -> must stay Unchanged at prev_offset
        let mut giant_body = String::with_capacity(4_500_128);
        giant_body.push_str(r#"{"type":"assistant","uuid":"giant","message":{"content":""#);
        for _ in 0..2_250_000 {
            giant_body.push_str("x ");
        }
        assert!(giant_body.len() > HARD_HISTORY_MAX_BYTES);
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(giant_body.as_bytes()).unwrap();
            f.flush().unwrap();
        }

        let d_incomplete =
            read_history_delta(AgentKind::Claude, &path, prev_offset, Some(&prev_probe)).unwrap();
        assert!(
            matches!(d_incomplete, DeltaRead::Unchanged { offset, .. } if offset == prev_offset),
            "incomplete >4 MiB line without newline must return Unchanged at prev_offset, got {d_incomplete:?}"
        );

        // (b) Complete the 4.5 MiB line with '\n' and append a normal follow-up line
        let giant_suffix = "\"}}\n";
        let followup = "{\"type\":\"assistant\",\"uuid\":\"follow1\",\"message\":{\"content\":\"FOLLOW_UP_AFTER_GIANT\"}}\n";
        let expected_after_giant =
            prev_offset + (giant_body.len() + giant_suffix.len()) as u64;
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(giant_suffix.as_bytes()).unwrap();
            f.write_all(followup.as_bytes()).unwrap();
            f.flush().unwrap();
        }

        // First delta skips cleanly past the completed >4 MiB line to nl + 1
        let d_skip =
            read_history_delta(AgentKind::Claude, &path, prev_offset, Some(&prev_probe)).unwrap();
        let (skip_offset, skip_probe) = match d_skip {
            DeltaRead::Appended {
                messages,
                latest_offset,
                probe_sha256,
                ..
            } => {
                assert!(messages.is_empty());
                assert_eq!(latest_offset, expected_after_giant);
                (latest_offset, probe_sha256)
            }
            other => panic!("expected Appended skipping oversized line, got {other:?}"),
        };

        // Second delta parses the normal follow-up line cleanly
        let d_follow =
            read_history_delta(AgentKind::Claude, &path, skip_offset, Some(&skip_probe)).unwrap();
        match d_follow {
            DeltaRead::Appended {
                messages,
                latest_offset,
                ..
            } => {
                assert_eq!(messages.len(), 1);
                assert_eq!(messages[0].id, "follow1");
                assert_eq!(messages[0].text, "FOLLOW_UP_AFTER_GIANT");
                assert_eq!(latest_offset, expected_after_giant + followup.len() as u64);
            }
            other => panic!("expected Appended with follow-up message, got {other:?}"),
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_image_blocks_preserves_uppercase_http_scheme_without_duplication() {
        let text = "Look at HTTPS://UPPER.IO/PIC.PNG and ![alt](HTTP://EXAMPLE.COM/A.JPG)";
        let blocks = extract_image_blocks(text);
        let urls: Vec<&str> = blocks
            .iter()
            .filter_map(|b| match b {
                MessageBlock::Image {
                    url: Some(u),
                    local_path: None,
                    ..
                } => Some(u.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            urls,
            vec!["HTTP://EXAMPLE.COM/A.JPG", "HTTPS://UPPER.IO/PIC.PNG"]
        );
    }

    #[test]
    fn strip_base64_in_str_scrubs_short_uppercase_data_image_uris() {
        let short_upper = "prefix DATA:image/png;base64,AAAA1234== suffix";
        assert_eq!(
            strip_base64_in_str(short_upper),
            "prefix [image omitted] suffix"
        );
        let mixed = "![px](DaTa:IMAGE/JPEG;BASE64,iVBORw0KGgo=)";
        assert_eq!(strip_base64_in_str(mixed), "![px]([image omitted])");
    }

    #[test]
    fn read_header_lines_bounded_handles_eof_exact_64kib_truncation_crlf_and_invalid_utf8() {
        let dir = unique_temp_dir("header-bounded");

        // 1. Small file without trailing '\n'
        let p_small = dir.join("small.jsonl");
        fs::write(&p_small, "line1\nline2_no_nl").unwrap();
        assert_eq!(
            read_header_lines_bounded(&p_small, 10),
            vec!["line1", "line2_no_nl"]
        );
        assert_eq!(read_header_lines_bounded(&p_small, 1), vec!["line1"]);
        assert!(read_header_lines_bounded(&p_small, 0).is_empty());

        // 2. Exact 64 KiB file without trailing '\n' -> complete, not dropped
        let p_exact = dir.join("exact_64k.jsonl");
        let prefix_line = "first_line\n";
        let rem = SUMMARY_HEADER_SCAN_BYTES - prefix_line.len();
        let exact_body = format!("{prefix_line}{}", "a".repeat(rem));
        assert_eq!(exact_body.len(), SUMMARY_HEADER_SCAN_BYTES);
        fs::write(&p_exact, &exact_body).unwrap();
        let exact_lines = read_header_lines_bounded(&p_exact, 10);
        assert_eq!(exact_lines.len(), 2);
        assert_eq!(exact_lines[0], "first_line");
        assert_eq!(exact_lines[1].len(), rem);

        // 3. 64 KiB + 1 byte file -> truncated final line dropped, line 1 preserved
        let p_over = dir.join("over_64k.jsonl");
        let over_body = format!("{prefix_line}{}", "b".repeat(rem + 1));
        assert_eq!(over_body.len(), SUMMARY_HEADER_SCAN_BYTES + 1);
        fs::write(&p_over, &over_body).unwrap();
        assert_eq!(read_header_lines_bounded(&p_over, 10), vec!["first_line"]);

        // 4. Single >64 KiB line with no '\n' in first 64 KiB -> returns empty Vec
        let p_single_over = dir.join("single_over_64k.jsonl");
        fs::write(&p_single_over, "c".repeat(SUMMARY_HEADER_SCAN_BYTES + 100)).unwrap();
        assert!(read_header_lines_bounded(&p_single_over, 10).is_empty());

        // 5. Invalid UTF-8 bytes in line #1 followed by valid line #2 + CRLF lines
        let p_utf8_crlf = dir.join("utf8_crlf.jsonl");
        let mut raw = Vec::new();
        raw.extend_from_slice(b"bad_\xff\xfe_line\r\n");
        raw.extend_from_slice(b"{\"ok\":true}\r\n");
        fs::write(&p_utf8_crlf, &raw).unwrap();
        let lines = read_header_lines_bounded(&p_utf8_crlf, 10);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("bad_") && lines[0].contains("_line"));
        assert!(!lines[0].ends_with('\r'));
        assert_eq!(lines[1], r#"{"ok":true}"#);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_header_meta_bounded_with_5mib_second_line_across_all_providers() {
        let dir = unique_temp_dir("header-5mib-all");
        let filler = "x".repeat(5 * 1024 * 1024);

        // Claude: line #1 has cwd + user title + model, line #2 is 5 MiB
        let claude_path = dir.join("claude_5mib.jsonl");
        let claude_body = format!(
            "{{\"type\":\"user\",\"cwd\":\"/srv/claude-app\",\"message\":{{\"role\":\"user\",\"content\":\"Fix Claude header\"}}}}\n{{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":\"{filler}\"}}}}\n"
        );
        fs::write(&claude_path, claude_body).unwrap();
        let claude_meta = ClaudeProvider.extract_header_meta(&claude_path);
        assert_eq!(claude_meta.cwd.as_deref(), Some("/srv/claude-app"));
        assert_eq!(claude_meta.title_hint.as_deref(), Some("Fix Claude header"));

        // Codex: line #1 has session_meta (id/cwd/model), line #2 is 5 MiB
        let codex_path = dir.join("rollout-2026-11-26-codex-sid.jsonl");
        let codex_body = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"codex-5mib-1\",\"cwd\":\"/srv/codex-app\",\"model\":\"gpt-5-codex\"}}}}\n{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"content\":\"{filler}\"}}}}\n"
        );
        fs::write(&codex_path, codex_body).unwrap();
        let codex_meta = CodexProvider.extract_header_meta(&codex_path);
        assert_eq!(codex_meta.session_id_hint.as_deref(), Some("codex-5mib-1"));
        assert_eq!(codex_meta.cwd.as_deref(), Some("/srv/codex-app"));
        assert_eq!(codex_meta.model.as_deref(), Some("gpt-5-codex"));

        // Antigravity: line #1 has multibot_meta, line #2 is 5 MiB
        let agy_path = dir.join("agy_5mib.history.jsonl");
        let agy_body = format!(
            "{{\"type\":\"multibot_meta\",\"session_id\":\"agy-5mib-1\",\"cwd\":\"/srv/agy-app\",\"title\":\"Agy Bounded\",\"model\":\"gemini-2.5-pro\"}}\n{{\"type\":\"step_update\",\"text\":\"{filler}\"}}\n"
        );
        fs::write(&agy_path, agy_body).unwrap();
        let agy_meta = AntigravityProvider.extract_header_meta(&agy_path);
        assert_eq!(agy_meta.session_id_hint.as_deref(), Some("agy-5mib-1"));
        assert_eq!(agy_meta.cwd.as_deref(), Some("/srv/agy-app"));
        assert_eq!(agy_meta.title_hint.as_deref(), Some("Agy Bounded"));
        assert_eq!(agy_meta.model.as_deref(), Some("gemini-2.5-pro"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_history_delta_handles_exact_256kib_boundary_and_interleaved_giant_lines() {
        let dir = unique_temp_dir("delta-256k-exact-interleaved");
        let path = dir.join("exact_interleaved.jsonl");

        // Build exact 262,144-byte (MAX_HISTORY_LINE_BYTES) valid Claude line
        let prefix = r#"{"type":"assistant","uuid":"exact256k","message":{"content":""#;
        let suffix = "\"}}\n";
        let exact_fill_len = MAX_HISTORY_LINE_BYTES - prefix.len() - suffix.len();
        let mut exact_256k = String::with_capacity(MAX_HISTORY_LINE_BYTES);
        exact_256k.push_str(prefix);
        for _ in 0..(exact_fill_len / 2) {
            exact_256k.push_str("a ");
        }
        if exact_fill_len % 2 == 1 {
            exact_256k.push('a');
        }
        exact_256k.push_str(suffix);
        assert_eq!(exact_256k.len(), MAX_HISTORY_LINE_BYTES);

        // Build exact 262,145-byte (MAX_HISTORY_LINE_BYTES + 1) oversized line
        let over_prefix = r#"{"type":"assistant","uuid":"over256k","message":{"content":""#;
        let over_fill_len = (MAX_HISTORY_LINE_BYTES + 1) - over_prefix.len() - suffix.len();
        let mut over_256k = String::with_capacity(MAX_HISTORY_LINE_BYTES + 1);
        over_256k.push_str(over_prefix);
        for _ in 0..(over_fill_len / 2) {
            over_256k.push_str("b ");
        }
        if over_fill_len % 2 == 1 {
            over_256k.push('b');
        }
        over_256k.push_str(suffix);
        assert_eq!(over_256k.len(), MAX_HISTORY_LINE_BYTES + 1);

        // Build ~4.6 MiB giant line as well
        let giant_prefix = r#"{"type":"assistant","uuid":"giant4m","message":{"content":""#;
        let mut giant_4m = String::with_capacity(4_600_128);
        giant_4m.push_str(giant_prefix);
        for _ in 0..2_300_000 {
            giant_4m.push_str("g ");
        }
        giant_4m.push_str(suffix);

        let small_b = "{\"type\":\"user\",\"uuid\":\"s-before\",\"message\":{\"content\":\"BEFORE\"}}\n";
        let small_m = "{\"type\":\"assistant\",\"uuid\":\"s-mid\",\"message\":{\"content\":\"MID\"}}\n";
        let small_a = "{\"type\":\"assistant\",\"uuid\":\"s-after\",\"message\":{\"content\":\"AFTER\"}}\n";

        {
            let mut f = File::create(&path).unwrap();
            f.write_all(small_b.as_bytes()).unwrap();
            f.write_all(over_256k.as_bytes()).unwrap();
            f.write_all(small_m.as_bytes()).unwrap();
            f.write_all(giant_4m.as_bytes()).unwrap();
            f.write_all(exact_256k.as_bytes()).unwrap();
            f.write_all(small_a.as_bytes()).unwrap();
            f.flush().unwrap();
        }
        let total_size = fs::metadata(&path).unwrap().len();

        let mut offset = 0u64;
        let mut probe: Option<String> = None;
        let mut collected_ids: Vec<String> = Vec::new();
        let mut skipped_chunks = 0usize;

        while offset < total_size {
            let delta =
                read_history_delta(AgentKind::Claude, &path, offset, probe.as_deref()).unwrap();
            match delta {
                DeltaRead::Appended {
                    messages,
                    latest_offset,
                    probe_sha256,
                    ..
                } => {
                    assert!(latest_offset > offset);
                    if messages.is_empty() {
                        skipped_chunks += 1;
                    } else {
                        for m in messages {
                            collected_ids.push(m.id);
                        }
                    }
                    offset = latest_offset;
                    probe = Some(probe_sha256);
                }
                other => panic!("unexpected delta at offset {offset}: {other:?}"),
            }
        }

        assert_eq!(offset, total_size);
        assert_eq!(
            skipped_chunks, 2,
            "both 256 KiB + 1 and 4.6 MiB lines must be skipped"
        );
        assert_eq!(
            collected_ids,
            vec!["s-before", "s-mid", "exact256k", "s-after"]
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_history_page_clamps_4mib_request_to_512kib_and_skips_300kib_inline_record() {
        let dir = unique_temp_dir("page-clamp-512k-and-300k-skip");

        // 1. Clamping `max_bytes = 4 MiB` on a ~900 KiB transcript -> clamped to 512 KiB with `has_more = true`
        let big_path = dir.join("normal_900k.jsonl");
        let pad = "word ".repeat(180); // ~900 bytes
        let mut body = String::with_capacity(950_000);
        for i in 0..950 {
            body.push_str(&format!(
                "{{\"type\":\"assistant\",\"uuid\":\"m-{i}\",\"message\":{{\"content\":\"msg {i} {pad}\"}}}}\n"
            ));
        }
        fs::write(&big_path, &body).unwrap();

        let page = read_history_page(
            AgentKind::Claude,
            &big_path,
            None,
            Some(4 * 1024 * 1024),
            None,
            None,
        )
        .unwrap();
        assert!(page.has_more, "4 MiB request must clamp to 512 KiB page");
        assert!(
            page.latest_offset - page.oldest_offset <= HARD_HISTORY_MAX_BYTES as u64,
            "returned byte span must not exceed 512 KiB"
        );
        assert!(!page.messages.is_empty());

        // 2. A 300 KiB single line (> 256 KiB MAX_HISTORY_LINE_BYTES, < 512 KiB page) surrounded
        //    by normal messages is skipped cleanly by `iter_complete_lines` without losing neighbors.
        let inline_path = dir.join("inline_300k.jsonl");
        let before = "{\"type\":\"user\",\"uuid\":\"u-pre\",\"message\":{\"content\":\"before 300k\"}}\n";
        let mid_prefix = r#"{"type":"assistant","uuid":"a-300k","message":{"content":""#;
        let mid_suffix = "\"}}\n";
        let mut mid_300k = String::with_capacity(300 * 1024 + 64);
        mid_300k.push_str(mid_prefix);
        for _ in 0..(150 * 1024) {
            mid_300k.push_str("z ");
        }
        mid_300k.push_str(mid_suffix);
        assert!(mid_300k.len() > MAX_HISTORY_LINE_BYTES && mid_300k.len() < HARD_HISTORY_MAX_BYTES);
        let after = "{\"type\":\"assistant\",\"uuid\":\"a-post\",\"message\":{\"content\":\"after 300k\"}}\n";
        fs::write(&inline_path, format!("{before}{mid_300k}{after}")).unwrap();

        let p_inline = read_history_page(
            AgentKind::Claude,
            &inline_path,
            None,
            Some(DEFAULT_HISTORY_MAX_BYTES),
            None,
            None,
        )
        .unwrap();
        let ids: Vec<&str> = p_inline.messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["u-pre", "a-post"]);
        assert_eq!(p_inline.oldest_offset, 0);
        assert!(!p_inline.has_more);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_history_page_backward_paginates_across_multiple_giant_lines_and_partial_tail() {
        let dir = unique_temp_dir("page-multi-giant");
        let path = dir.join("multi_giant.jsonl");

        let line1 = "{\"type\":\"user\",\"uuid\":\"u1\",\"message\":{\"content\":\"first\"}}\n";
        let line2 = "{\"type\":\"assistant\",\"uuid\":\"a2\",\"message\":{\"content\":\"middle\"}}\n";

        let giant_prefix = r#"{"type":"assistant","uuid":"g","message":{"content":""#;
        let giant_suffix = "\"}}\n";
        let mut giant = String::with_capacity(4_600_128);
        giant.push_str(giant_prefix);
        for _ in 0..2_300_000 {
            giant.push_str("g ");
        }
        giant.push_str(giant_suffix);
        assert!(giant.len() > HARD_HISTORY_MAX_BYTES);

        let partial_tail = r#"{"type":"user","uuid":"partial","message":{"content":"unterminated"#;

        {
            let mut f = File::create(&path).unwrap();
            f.write_all(line1.as_bytes()).unwrap();
            f.write_all(giant.as_bytes()).unwrap();
            f.write_all(line2.as_bytes()).unwrap();
            f.write_all(giant.as_bytes()).unwrap();
            f.write_all(partial_tail.as_bytes()).unwrap();
            f.flush().unwrap();
        }

        let off_after_l1 = line1.len() as u64;
        let off_after_g1 = off_after_l1 + giant.len() as u64;
        let off_after_l2 = off_after_g1 + line2.len() as u64;
        let off_after_g2 = off_after_l2 + giant.len() as u64;

        // Page 1: skips trailing partial line and giant #2 -> [off_after_l2 .. off_after_g2)
        let p1 = read_history_page(
            AgentKind::Claude,
            &path,
            None,
            Some(DEFAULT_HISTORY_MAX_BYTES),
            None,
            None,
        )
        .unwrap();
        assert!(p1.messages.is_empty());
        assert_eq!(p1.oldest_offset, off_after_l2);
        assert_eq!(p1.latest_offset, off_after_g2);
        assert!(p1.has_more);

        // Page 2: reads `line2` cleanly
        let p2 = read_history_page(
            AgentKind::Claude,
            &path,
            Some(p1.oldest_offset),
            Some(DEFAULT_HISTORY_MAX_BYTES),
            None,
            None,
        )
        .unwrap();
        assert_eq!(p2.messages.len(), 1);
        assert_eq!(p2.messages[0].id, "a2");
        assert_eq!(p2.oldest_offset, off_after_g1);
        assert_eq!(p2.latest_offset, off_after_l2);
        assert!(p2.has_more);

        // Page 3: skips giant #1 cleanly
        let p3 = read_history_page(
            AgentKind::Claude,
            &path,
            Some(p2.oldest_offset),
            Some(DEFAULT_HISTORY_MAX_BYTES),
            None,
            None,
        )
        .unwrap();
        assert!(p3.messages.is_empty());
        assert_eq!(p3.oldest_offset, off_after_l1);
        assert_eq!(p3.latest_offset, off_after_g1);
        assert!(p3.has_more);

        // Page 4: reads `line1` and finishes at offset 0
        let p4 = read_history_page(
            AgentKind::Claude,
            &path,
            Some(p3.oldest_offset),
            Some(DEFAULT_HISTORY_MAX_BYTES),
            None,
            None,
        )
        .unwrap();
        assert_eq!(p4.messages.len(), 1);
        assert_eq!(p4.messages[0].id, "u1");
        assert_eq!(p4.oldest_offset, 0);
        assert_eq!(p4.latest_offset, off_after_l1);
        assert!(!p4.has_more);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn streaming_scrubber_preserves_json_escapes_utf8_and_chunk_boundaries() {
        let huge_a = "A".repeat(600 * 1024);
        let raw_b64 = "B".repeat(300);
        let input = format!(
            concat!(
                r#"{{"tiny":"data:image/png;base64,iVBORw0KGgo=","#,
                r#""huge":"prefix data:image/png;base64,{}== suffix","#,
                r#""mixed":"DATA:IMAGE/SVG+XML;BASE64,iVBORw0KGgo_abc-123","#,
                r#""escaped_slash":"data:image\/png;base64,AAA\/BBB==","#,
                r#""unicode_guard":"🚀日本語 \u0041{} done","#,
                r#""escapes":"quote:\" back:\\ both:\\\" {}","#,
                r#""false1":"data:text/plain,hi","#,
                r#""false2":"data:data:image/png;base64,AAA=","#,
                r#""empty_uri":"data:image/png;base64,","#,
                r#""nested":"{{\"img\":\"data:image/png;base64,{}==\",\"ok\":true}}"}}"#
            ),
            huge_a, raw_b64, raw_b64, raw_b64
        );
        let bytes = input.as_bytes();

        for &chunk_size in &[1usize, 2, 3, 7, 13, SCRUB_CHUNK_BYTES] {
            let mut scrubber = StreamingBase64Scrubber::new(MAX_HISTORY_LINE_BYTES - 1);
            for chunk in bytes.chunks(chunk_size) {
                assert!(
                    scrubber.feed_chunk(chunk),
                    "scrubber overflowed unexpectedly at chunk_size={chunk_size}"
                );
            }
            let out = scrubber
                .finish()
                .expect("scrubber finish should succeed");
            let parsed: Value = serde_json::from_slice(&out).unwrap_or_else(|e| {
                panic!(
                    "invalid JSON at chunk_size={chunk_size}: {e}\noutput={}",
                    String::from_utf8_lossy(&out)
                )
            });

            assert_eq!(parsed["tiny"], "[image omitted]");
            assert_eq!(parsed["huge"], "prefix [image omitted] suffix");
            assert_eq!(parsed["mixed"], "[image omitted]");
            assert_eq!(parsed["escaped_slash"], "[image omitted]");
            assert_eq!(parsed["unicode_guard"], "🚀日本語 A[image omitted] done");
            assert_eq!(parsed["escapes"], "quote:\" back:\\ both:\\\" [image omitted]");
            assert_eq!(parsed["false1"], "data:text/plain,hi");
            assert_eq!(parsed["false2"], "data:[image omitted]");
            assert_eq!(parsed["empty_uri"], "data:image/png;base64,");
            let nested_str = parsed["nested"].as_str().unwrap();
            let nested_val: Value = serde_json::from_str(nested_str)
                .expect("double-encoded Codex arguments JSON must stay valid");
            assert_eq!(nested_val["img"], "[image omitted]");
            assert_eq!(nested_val["ok"], true);
        }

        // Verify non-base64 oversized payload (>256 KiB) aborts early and returns None
        let mut non_b64 = String::from(r#"{"content":""#);
        for _ in 0..(140 * 1024) {
            non_b64.push_str("x ");
        }
        non_b64.push_str("\"}");
        assert!(scrub_line_bytes(non_b64.as_bytes(), MAX_HISTORY_LINE_BYTES - 1).is_none());
    }

    #[test]
    fn read_history_page_scrubs_300kib_520kib_and_2mib_base64_preserving_raw_offsets_and_pairing() {
        let dir = unique_temp_dir("page-b64-scrub");

        // (a) 300 KiB inline data:image/png;base64,... between u-pre and a-post
        {
            let path = dir.join("inline_300k_b64.jsonl");
            let l0 = "{\"type\":\"user\",\"uuid\":\"u-pre\",\"message\":{\"content\":\"before\"}}\n";
            let b64_300k = "A".repeat(300 * 1024);
            let l1 = format!(
                "{{\"type\":\"user\",\"uuid\":\"u-b64\",\"message\":{{\"content\":\"data:image/png;base64,{b64_300k}==\"}}}}\n"
            );
            let l2 = "{\"type\":\"assistant\",\"uuid\":\"a-post\",\"message\":{\"content\":\"after\"}}\n";
            let off1 = l0.len() as u64;
            let off2 = off1 + l1.len() as u64;
            let total = off2 + l2.len() as u64;
            fs::write(&path, format!("{l0}{l1}{l2}")).unwrap();

            let page = read_history_page(
                AgentKind::Claude,
                &path,
                None,
                Some(DEFAULT_HISTORY_MAX_BYTES),
                None,
                None,
            )
            .unwrap();
            let ids: Vec<&str> = page.messages.iter().map(|m| m.id.as_str()).collect();
            assert_eq!(ids, vec!["u-pre", "u-b64", "a-post"]);
            assert_eq!(page.messages[0].source_byte_range, Some((0, off1)));
            assert_eq!(page.messages[1].source_byte_range, Some((off1, off2)));
            assert_eq!(page.messages[2].source_byte_range, Some((off2, total)));
            assert_eq!(page.messages[1].text, "[image omitted]");
            assert_eq!(page.oldest_offset, 0);
            assert_eq!(page.latest_offset, total);
            assert!(!page.has_more);
            let mut f = File::open(&path).unwrap();
            assert_eq!(page.probe_sha256, compute_probe_sha256(&mut f, total).unwrap());
        }

        // (b) Exact T2-DMN-06 layout (520 KiB tool_result with 512000 'A's + 20000 'X's + trailing a-b64-2)
        {
            let path = dir.join("t2_dmn_06_shape.jsonl");
            let l0 = "{\"type\":\"user\",\"uuid\":\"u-b64-1\",\"timestamp\":\"2026-11-26T10:00:00Z\",\"message\":{\"role\":\"user\",\"content\":\"Capture screenshot and dump log\"}}\n";
            let l1 = "{\"type\":\"assistant\",\"uuid\":\"a-b64-1\",\"timestamp\":\"2026-11-26T10:00:01Z\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"tu_img_1\",\"name\":\"Bash\",\"input\":{\"description\":\"Capture PNG\"}},{\"type\":\"tool_use\",\"id\":\"tu_long_1\",\"name\":\"Bash\",\"input\":{\"description\":\"Dump verbose log\"}}]}}\n";
            let b64_500k = "A".repeat(512_000);
            let long_20k = "X".repeat(20_000);
            let l2 = format!(
                "{{\"type\":\"user\",\"uuid\":\"u-b64-res\",\"timestamp\":\"2026-11-26T10:00:02Z\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"tu_img_1\",\"content\":\"data:image/png;base64,{b64_500k}==\"}},{{\"type\":\"tool_result\",\"tool_use_id\":\"tu_long_1\",\"content\":\"LONG_OUTPUT_LINE_{long_20k}\"}}]}}}}\n"
            );
            let l3 = "{\"type\":\"assistant\",\"uuid\":\"a-b64-2\",\"timestamp\":\"2026-11-26T10:00:03Z\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"Captured and dumped.\"}]}}\n";
            let off1 = l0.len() as u64;
            let off2 = off1 + l1.len() as u64;
            let off3 = off2 + l2.len() as u64;
            let total = off3 + l3.len() as u64;
            assert!(total > HARD_HISTORY_MAX_BYTES as u64);
            fs::write(&path, format!("{l0}{l1}{l2}{l3}")).unwrap();

            let page = read_history_page(
                AgentKind::Claude,
                &path,
                None,
                Some(2 * 1024 * 1024),
                None,
                None,
            )
            .unwrap();
            let ids: Vec<&str> = page.messages.iter().map(|m| m.id.as_str()).collect();
            assert_eq!(ids, vec!["u-b64-1", "a-b64-1", "a-b64-2"]);
            assert_eq!(page.messages[0].source_byte_range, Some((0, off1)));
            assert_eq!(page.messages[1].source_byte_range, Some((off1, off2)));
            assert_eq!(page.messages[2].source_byte_range, Some((off3, total)));
            assert_eq!(page.oldest_offset, 0);
            assert_eq!(page.latest_offset, total);
            assert!(!page.has_more);
            let mut f = File::open(&path).unwrap();
            assert_eq!(page.probe_sha256, compute_probe_sha256(&mut f, total).unwrap());

            let mut saw_omitted = false;
            for blk in &page.messages[1].blocks {
                if let MessageBlock::ToolUse {
                    call_id,
                    detail,
                    output,
                    ..
                } = blk
                {
                    assert!(detail.chars().count() <= MAX_TOOL_DETAIL_CHARS);
                    if let Some(out) = output {
                        assert!(out.chars().count() <= MAX_TOOL_DETAIL_CHARS);
                        assert!(!out.contains(&"A".repeat(256)));
                        if call_id == "tu_img_1" && out.contains("[image omitted]") {
                            saw_omitted = true;
                        }
                    }
                }
            }
            assert!(saw_omitted, "paired tu_img_1 must contain [image omitted]");

            // tail=Some(1) must set oldest_offset to exact raw on-disk byte offset of Line 3!
            let tail1 = read_history_page(
                AgentKind::Claude,
                &path,
                None,
                Some(DEFAULT_HISTORY_MAX_BYTES),
                Some(1),
                None,
            )
            .unwrap();
            assert_eq!(tail1.messages.len(), 1);
            assert_eq!(tail1.messages[0].id, "a-b64-2");
            assert_eq!(tail1.oldest_offset, off3);
            assert!(tail1.has_more);
        }

        // (c) 2 MiB Anthropic structured base64 image in direct user/assistant content & EOF
        {
            let path = dir.join("anthropic_2mib.jsonl");
            let l0 = "{\"type\":\"user\",\"uuid\":\"u0\",\"message\":{\"content\":\"start\"}}\n";
            let b64_2m = "B".repeat(2 * 1024 * 1024);
            let l1 = format!(
                "{{\"type\":\"user\",\"uuid\":\"u-img-2m\",\"message\":{{\"content\":[{{\"type\":\"image\",\"source\":{{\"type\":\"base64\",\"media_type\":\"image/png\",\"data\":\"{b64_2m}\"}}}}]}}}}\n"
            );
            let l2 = format!(
                "{{\"type\":\"assistant\",\"uuid\":\"a-img-1_5m\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"Annotated:\"}},{{\"type\":\"image\",\"source\":{{\"type\":\"base64\",\"media_type\":\"image/png\",\"data\":\"{}\"}}}}]}}}}\n",
                "C".repeat(1_500_000)
            );
            let off1 = l0.len() as u64;
            let off2 = off1 + l1.len() as u64;
            let total = off2 + l2.len() as u64;
            fs::write(&path, format!("{l0}{l1}{l2}")).unwrap();

            let page = read_history_page(
                AgentKind::Claude,
                &path,
                None,
                Some(DEFAULT_HISTORY_MAX_BYTES),
                None,
                None,
            )
            .unwrap();
            let ids: Vec<&str> = page.messages.iter().map(|m| m.id.as_str()).collect();
            assert_eq!(ids, vec!["u0", "u-img-2m", "a-img-1_5m"]);
            assert_eq!(page.messages[1].text, "[image omitted]");
            assert_eq!(page.messages[1].source_byte_range, Some((off1, off2)));
            assert!(page.messages[2].text.contains("Annotated:"));
            assert!(page.messages[2].text.contains("[image omitted]"));
            assert!(!page.messages[2].text.contains(&"C".repeat(256)));
            assert_eq!(page.messages[2].source_byte_range, Some((off2, total)));
            assert_eq!(page.oldest_offset, 0);
            assert_eq!(page.latest_offset, total);
            assert!(!page.has_more);
            let mut f = File::open(&path).unwrap();
            assert_eq!(page.probe_sha256, compute_probe_sha256(&mut f, total).unwrap());
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_history_page_paginates_across_1_5mib_base64_surrounded_by_large_prose() {
        let dir = unique_temp_dir("page-prose-b64-prose");
        let path = dir.join("prose_b64_prose.jsonl");

        let pad = "word ".repeat(200); // ~1 KiB per line
        let mut content = String::new();
        let mut expected_ids = Vec::new();
        for i in 0..400 {
            let id = format!("pre-{i}");
            content.push_str(&format!(
                "{{\"type\":\"assistant\",\"uuid\":\"{id}\",\"message\":{{\"content\":\"pre {i} {pad}\"}}}}\n"
            ));
            expected_ids.push(id);
        }

        let b64_start = content.len() as u64;
        let b64_1_5m = "D".repeat(1_536 * 1024);
        let b64_line = format!(
            "{{\"type\":\"user\",\"uuid\":\"mid-b64\",\"message\":{{\"content\":\"Shot data:image/png;base64,{b64_1_5m}==\"}}}}\n"
        );
        content.push_str(&b64_line);
        let b64_end = content.len() as u64;
        expected_ids.push("mid-b64".into());

        for i in 0..300 {
            let id = format!("post-{i}");
            content.push_str(&format!(
                "{{\"type\":\"assistant\",\"uuid\":\"{id}\",\"message\":{{\"content\":\"post {i} {pad}\"}}}}\n"
            ));
            expected_ids.push(id);
        }
        fs::write(&path, &content).unwrap();

        let mut collected = Vec::new();
        let mut before: Option<u64> = None;
        let mut probe: Option<String> = None;
        let mut pages = 0usize;

        loop {
            let page = read_history_page(
                AgentKind::Claude,
                &path,
                before,
                Some(DEFAULT_HISTORY_MAX_BYTES),
                None,
                probe.as_deref(),
            )
            .unwrap();
            pages += 1;
            assert!(pages <= 10, "pagination did not terminate");

            let mut combined = page.messages;
            combined.extend(collected);
            collected = combined;

            if !page.has_more {
                assert_eq!(page.oldest_offset, 0);
                break;
            }
            assert!(page.oldest_offset > 0);
            before = Some(page.oldest_offset);
            let mut f = File::open(&path).unwrap();
            probe = Some(compute_probe_sha256(&mut f, page.oldest_offset).unwrap());
        }

        assert!(pages >= 2, "expected multi-page traversal, got {pages}");
        let got_ids: Vec<String> = collected.iter().map(|m| m.id.clone()).collect();
        assert_eq!(got_ids, expected_ids);
        let mid = collected.iter().find(|m| m.id == "mid-b64").unwrap();
        assert_eq!(mid.text, "Shot [image omitted]");
        assert_eq!(mid.source_byte_range, Some((b64_start, b64_end)));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_history_delta_streams_300kib_and_1_5mib_base64_and_waits_for_newline() {
        let dir = unique_temp_dir("delta-b64");
        let path = dir.join("stream_b64.jsonl");

        let l0 = "{\"type\":\"user\",\"uuid\":\"u0\",\"message\":{\"content\":\"init\"}}\n";
        fs::write(&path, l0).unwrap();
        let page = read_history_page(AgentKind::Claude, &path, None, None, None, None).unwrap();
        let off0 = page.latest_offset;
        let probe0 = page.probe_sha256.clone();

        // 1. Append incomplete 400 KiB data:image/png;base64,... line without '\n' -> Unchanged
        let b64_400k = "E".repeat(400 * 1024);
        let partial = format!(
            "{{\"type\":\"assistant\",\"uuid\":\"a-400k\",\"message\":{{\"content\":\"img data:image/png;base64,{b64_400k}"
        );
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(partial.as_bytes()).unwrap();
            f.flush().unwrap();
        }
        let d_un = read_history_delta(AgentKind::Claude, &path, off0, Some(&probe0)).unwrap();
        assert!(matches!(d_un, DeltaRead::Unchanged { offset, .. } if offset == off0));

        // Complete the 400 KiB line with '\n'
        let suffix = "==\"}}\n";
        let expected_off1 = off0 + (partial.len() + suffix.len()) as u64;
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(suffix.as_bytes()).unwrap();
            f.flush().unwrap();
        }
        let d1 = read_history_delta(AgentKind::Claude, &path, off0, Some(&probe0)).unwrap();
        let (off1, probe1) = match d1 {
            DeltaRead::Appended {
                messages,
                latest_offset,
                probe_sha256,
                ..
            } => {
                assert_eq!(messages.len(), 1);
                assert_eq!(messages[0].id, "a-400k");
                assert_eq!(messages[0].text, "img [image omitted]");
                assert_eq!(messages[0].source_byte_range, Some((off0, expected_off1)));
                assert_eq!(latest_offset, expected_off1);
                let mut f = File::open(&path).unwrap();
                assert_eq!(probe_sha256, compute_probe_sha256(&mut f, expected_off1).unwrap());
                (latest_offset, probe_sha256)
            }
            other => panic!("expected Appended for 400 KiB b64 line, got {other:?}"),
        };

        // 2. Append a complete 1.5 MiB Anthropic structured base64 line + normal follow-up line
        let b64_1_5m = "F".repeat(1_536 * 1024);
        let l2 = format!(
            "{{\"type\":\"user\",\"uuid\":\"u-1_5m\",\"message\":{{\"content\":[{{\"type\":\"image\",\"source\":{{\"type\":\"base64\",\"media_type\":\"image/png\",\"data\":\"{b64_1_5m}\"}}}}]}}}}\n"
        );
        let l3 = "{\"type\":\"assistant\",\"uuid\":\"a-tail\",\"message\":{\"content\":\"done\"}}\n";
        let expected_off2 = off1 + l2.len() as u64;
        let expected_off3 = expected_off2 + l3.len() as u64;
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(l2.as_bytes()).unwrap();
            f.write_all(l3.as_bytes()).unwrap();
            f.flush().unwrap();
        }

        let d2 = read_history_delta(AgentKind::Claude, &path, off1, Some(&probe1)).unwrap();
        let (off2, probe2) = match d2 {
            DeltaRead::Appended {
                messages,
                latest_offset,
                probe_sha256,
                tail_activity,
                ..
            } => {
                assert_eq!(messages.len(), 1);
                assert_eq!(messages[0].id, "u-1_5m");
                assert_eq!(messages[0].text, "[image omitted]");
                assert_eq!(messages[0].source_byte_range, Some((off1, expected_off2)));
                assert_eq!(tail_activity, TailActivity::Working);
                assert_eq!(latest_offset, expected_off2);
                (latest_offset, probe_sha256)
            }
            other => panic!("expected Appended for 1.5 MiB Anthropic b64 line, got {other:?}"),
        };

        let d3 = read_history_delta(AgentKind::Claude, &path, off2, Some(&probe2)).unwrap();
        match d3 {
            DeltaRead::Appended {
                messages,
                latest_offset,
                tail_activity,
                ..
            } => {
                assert_eq!(messages.len(), 1);
                assert_eq!(messages[0].id, "a-tail");
                assert_eq!(messages[0].text, "done");
                assert_eq!(messages[0].source_byte_range, Some((off2, expected_off3)));
                assert_eq!(latest_offset, expected_off3);
                assert_eq!(tail_activity, TailActivity::Idle);
            }
            other => panic!("expected Appended for follow-up line, got {other:?}"),
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn summarize_session_handles_400kib_and_2mib_base64_tail_preview_and_working_state() {
        let dir = unique_temp_dir("summary-b64-tail");
        let meta = MetadataStore::default();
        let archive = ArchiveStore::default();
        let active = HashSet::new();
        let live = LiveActivity::default();
        let server_workspaces = ServerWorkspaceIndex::default();
        let ctx = DiscoveryContext {
            active_session_ids: &active,
            connected_session_ids: &active,
            metadata: &meta,
            archive: &archive,
            live: &live,
            include_archived: true,
            server_workspaces: &server_workspaces,
        };
        let now = now_ms();

        // (a) 400 KiB completed assistant message at mtime=now -> working=false, preview present
        let p_done = dir.join("done_400k.jsonl");
        let b64_400k = "G".repeat(400 * 1024);
        fs::write(
            &p_done,
            format!(
                "{{\"type\":\"user\",\"uuid\":\"u1\",\"message\":{{\"content\":\"take shot\"}}}}\n{{\"type\":\"assistant\",\"uuid\":\"a-400k\",\"message\":{{\"content\":\"Screenshot ready data:image/png;base64,{b64_400k}==\"}}}}\n"
            ),
        )
        .unwrap();
        let s_done = summarize_session(&ClaudeProvider, &p_done, now, &ctx);
        assert!(
            !s_done.working,
            "completed 400 KiB assistant image message must report working=false"
        );
        assert_eq!(s_done.last_assistant_id.as_deref(), Some("a-400k"));
        let preview = s_done.last_assistant_preview.as_deref().unwrap_or("");
        assert!(preview.contains("Screenshot ready"));
        assert!(!preview.contains(&"G".repeat(64)));

        // (b) 2 MiB user Anthropic base64 image at mtime=now-15s -> working=true
        let p_work = dir.join("work_2m.jsonl");
        let b64_2m = "H".repeat(2 * 1024 * 1024);
        fs::write(
            &p_work,
            format!(
                "{{\"type\":\"assistant\",\"uuid\":\"a0\",\"message\":{{\"content\":\"ready\"}}}}\n{{\"type\":\"user\",\"uuid\":\"u-2m\",\"message\":{{\"content\":[{{\"type\":\"image\",\"source\":{{\"type\":\"base64\",\"media_type\":\"image/png\",\"data\":\"{b64_2m}\"}}}}]}}}}\n"
            ),
        )
        .unwrap();
        let s_work = summarize_session(&ClaudeProvider, &p_work, now - 15_000, &ctx);
        assert!(
            s_work.working,
            "2 MiB user base64 image message written 15s ago must report working=true"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_claude_and_antigravity_scrubbed_image_urls_do_not_emit_bogus_image_blocks() {
        let dir = unique_temp_dir("provider-b64-image-guards");
        let b64_600k = "J".repeat(600 * 1024);

        // 1. Codex 600 KiB input_image data URI
        let codex_path = dir.join("rollout-2026-11-26-codex-img.jsonl");
        fs::write(
            &codex_path,
            format!(
                "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_image\",\"image_url\":\"data:image/png;base64,{b64_600k}==\"}}]}}}}\n"
            ),
        )
        .unwrap();
        let codex_page =
            read_history_page(AgentKind::Codex, &codex_path, None, None, None, None).unwrap();
        assert_eq!(codex_page.messages.len(), 1);
        assert_eq!(codex_page.messages[0].text, "[image omitted]");
        assert!(
            !codex_page.messages[0]
                .blocks
                .iter()
                .any(|b| matches!(b, MessageBlock::Image { .. })),
            "Codex must not emit bogus MessageBlock::Image for scrubbed data URI: {:?}",
            codex_page.messages[0].blocks
        );

        // 2. Claude 600 KiB source.url data URI
        let claude_path = dir.join("claude_url_img.jsonl");
        fs::write(
            &claude_path,
            format!(
                "{{\"type\":\"assistant\",\"uuid\":\"a-url\",\"message\":{{\"content\":[{{\"type\":\"image\",\"source\":{{\"type\":\"url\",\"url\":\"DATA:image/png;base64,{b64_600k}==\"}}}}]}}}}\n"
            ),
        )
        .unwrap();
        let claude_page =
            read_history_page(AgentKind::Claude, &claude_path, None, None, None, None).unwrap();
        assert_eq!(claude_page.messages.len(), 1);
        assert_eq!(claude_page.messages[0].text, "[image omitted]");
        assert!(
            !claude_page.messages[0]
                .blocks
                .iter()
                .any(|b| matches!(b, MessageBlock::Image { .. })),
            "Claude must not emit bogus MessageBlock::Image for scrubbed data URI: {:?}",
            claude_page.messages[0].blocks
        );

        // 3. Antigravity multibot_history role=="tool" string detail & output
        let agy_path = dir.join("agy_tool_b64.history.jsonl");
        fs::write(
            &agy_path,
            format!(
                "{{\"type\":\"multibot_history\",\"row\":{{\"id\":\"t1\",\"role\":\"tool\",\"name\":\"screenshot\",\"detail\":\"in data:image/png;base64,{b64_600k}==\",\"output\":\"out data:image/png;base64,{b64_600k}==\",\"status\":\"completed\"}}}}\n"
            ),
        )
        .unwrap();
        let agy_page =
            read_history_page(AgentKind::Antigravity, &agy_path, None, None, None, None).unwrap();
        assert_eq!(agy_page.messages.len(), 1);
        let tool_blk = agy_page.messages[0].blocks.first().unwrap();
        match tool_blk {
            MessageBlock::ToolUse { detail, output, .. } => {
                assert_eq!(detail, "in [image omitted]");
                assert_eq!(output.as_deref(), Some("out [image omitted]"));
            }
            other => panic!("expected ToolUse block, got {other:?}"),
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pathological_20mib_non_base64_spaced_and_unspaced_skip_cleanly_in_page_and_delta() {
        let dir = unique_temp_dir("giant-20mib");
        let l_before = "{\"type\":\"user\",\"uuid\":\"u-before\",\"message\":{\"content\":\"before 20m\"}}\n";
        let l_after = "{\"type\":\"assistant\",\"uuid\":\"a-after\",\"message\":{\"content\":\"after 20m\"}}\n";

        for (label, chunk_pattern) in [("spaced", "x ".repeat(32 * 1024)), ("unspaced", "z".repeat(64 * 1024))] {
            let path = dir.join(format!("giant_20m_{label}.jsonl"));
            let off_before_giant = l_before.len() as u64;
            {
                let mut f = File::create(&path).unwrap();
                f.write_all(l_before.as_bytes()).unwrap();
                f.write_all(br#"{"type":"assistant","uuid":"g20","message":{"content":""#).unwrap();
                // Write 20 MiB (> MAX_SCRUB_RAW_LINE_BYTES = 16 MiB)
                for _ in 0..320 {
                    f.write_all(chunk_pattern.as_bytes()).unwrap();
                }
                f.write_all(b"\"}}\n").unwrap();
                f.write_all(l_after.as_bytes()).unwrap();
                f.flush().unwrap();
            }
            let total_size = fs::metadata(&path).unwrap().len();
            let off_after_giant = total_size - l_after.len() as u64;
            assert!(off_after_giant - off_before_giant > MAX_SCRUB_RAW_LINE_BYTES);

            // Backward pagination via read_history_page
            let p1 = read_history_page(AgentKind::Claude, &path, None, None, None, None).unwrap();
            assert_eq!(p1.messages.len(), 1, "{label} p1 should return a-after");
            assert_eq!(p1.messages[0].id, "a-after");
            assert_eq!(p1.oldest_offset, off_after_giant);
            assert_eq!(p1.latest_offset, total_size);
            assert!(p1.has_more);

            let p2 = read_history_page(
                AgentKind::Claude,
                &path,
                Some(p1.oldest_offset),
                None,
                None,
                None,
            )
            .unwrap();
            assert!(p2.messages.is_empty(), "{label} p2 should skip 20 MiB line");
            assert_eq!(p2.oldest_offset, off_before_giant);
            assert_eq!(p2.latest_offset, off_after_giant);
            assert!(p2.has_more);

            let p3 = read_history_page(
                AgentKind::Claude,
                &path,
                Some(p2.oldest_offset),
                None,
                None,
                None,
            )
            .unwrap();
            assert_eq!(p3.messages.len(), 1, "{label} p3 should return u-before");
            assert_eq!(p3.messages[0].id, "u-before");
            assert_eq!(p3.oldest_offset, 0);
            assert!(!p3.has_more);

            // Forward streaming via read_history_delta
            let mut offset = 0u64;
            let mut probe: Option<String> = None;
            let mut ids = Vec::new();
            let mut skipped = 0usize;
            while offset < total_size {
                match read_history_delta(AgentKind::Claude, &path, offset, probe.as_deref()).unwrap() {
                    DeltaRead::Appended {
                        messages,
                        latest_offset,
                        probe_sha256,
                        ..
                    } => {
                        assert!(latest_offset > offset);
                        if messages.is_empty() {
                            skipped += 1;
                        } else {
                            ids.extend(messages.into_iter().map(|m| m.id));
                        }
                        offset = latest_offset;
                        probe = Some(probe_sha256);
                    }
                    other => panic!("{label}: unexpected delta {other:?}"),
                }
            }
            assert_eq!(skipped, 1, "{label}: 20 MiB line must be skipped once");
            assert_eq!(ids, vec!["u-before", "a-after"]);
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_history_page_skips_incomplete_trailing_line_over_512kib_and_20mib() {
        let dir = unique_temp_dir("incomplete-trailing-over-512k-20m");

        // Build 5 normal messages m0..m4
        let mut normal_prefix = String::new();
        let mut line_offsets = Vec::new();
        for i in 0..5 {
            line_offsets.push(normal_prefix.len() as u64);
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            normal_prefix.push_str(&format!(
                "{{\"type\":\"{role}\",\"uuid\":\"m{i}\",\"message\":{{\"content\":\"message {i}\"}}}}\n"
            ));
        }
        let off_after_m4 = normal_prefix.len() as u64;

        // Scenario 1: 5 normal messages + 600 KiB incomplete trailing base64 line (no '\n')
        let p_600k = dir.join("incomplete_600k.jsonl");
        let b64_600k = "K".repeat(600 * 1024);
        let partial_600k = format!(
            "{{\"type\":\"user\",\"uuid\":\"u-partial\",\"message\":{{\"content\":\"Shot data:image/png;base64,{b64_600k}"
        );
        fs::write(&p_600k, format!("{normal_prefix}{partial_600k}")).unwrap();

        let expected_probe = {
            let mut f = File::open(&p_600k).unwrap();
            compute_probe_sha256(&mut f, off_after_m4).unwrap()
        };

        // 1a: Default max_bytes = None (512 KiB)
        let page = read_history_page(AgentKind::Claude, &p_600k, None, None, None, None).unwrap();
        let ids: Vec<&str> = page.messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["m0", "m1", "m2", "m3", "m4"]);
        assert_eq!(page.oldest_offset, 0);
        assert_eq!(page.latest_offset, off_after_m4);
        assert!(!page.has_more);
        assert_eq!(page.probe_sha256, expected_probe);

        // 1b: Small max_bytes = Some(64) on the same file
        let p_small =
            read_history_page(AgentKind::Claude, &p_600k, None, Some(64), None, None).unwrap();
        let small_ids: Vec<&str> = p_small.messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(small_ids, vec!["m4"]);
        assert_eq!(p_small.oldest_offset, line_offsets[4]);
        assert_eq!(p_small.latest_offset, off_after_m4);
        assert!(p_small.has_more);

        // 1c: tail = Some(2) on the same file
        let p_tail =
            read_history_page(AgentKind::Claude, &p_600k, None, None, Some(2), None).unwrap();
        let tail_ids: Vec<&str> = p_tail.messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(tail_ids, vec!["m3", "m4"]);
        assert_eq!(p_tail.oldest_offset, line_offsets[3]);
        assert_eq!(p_tail.latest_offset, off_after_m4);
        assert!(p_tail.has_more);

        // 1d: Follow-up read_history_delta stays Unchanged until '\n' arrives, then emits scrubbed line
        let d_unchanged = read_history_delta(
            AgentKind::Claude,
            &p_600k,
            off_after_m4,
            Some(&expected_probe),
        )
        .unwrap();
        assert!(matches!(
            d_unchanged,
            DeltaRead::Unchanged { offset, .. } if offset == off_after_m4
        ));
        {
            let mut f = OpenOptions::new().append(true).open(&p_600k).unwrap();
            f.write_all(b"\"}}\n").unwrap();
        }
        let d_appended = read_history_delta(
            AgentKind::Claude,
            &p_600k,
            off_after_m4,
            Some(&expected_probe),
        )
        .unwrap();
        match d_appended {
            DeltaRead::Appended { messages, .. } => {
                assert_eq!(messages.len(), 1);
                assert_eq!(messages[0].id, "u-partial");
                assert_eq!(messages[0].text, "Shot [image omitted]");
            }
            other => panic!("expected Appended after finishing partial line, got {other:?}"),
        }

        // Scenario 2: 5 normal messages + 20 MiB incomplete trailing line (no '\n', > MAX_RAW_SCAN_BYTES_PER_PAGE)
        let p_20m = dir.join("incomplete_20m.jsonl");
        {
            let mut f = File::create(&p_20m).unwrap();
            f.write_all(normal_prefix.as_bytes()).unwrap();
            let chunk = vec![b'x'; 64 * 1024];
            for _ in 0..320 {
                f.write_all(&chunk).unwrap();
            }
        }
        let page_20m =
            read_history_page(AgentKind::Claude, &p_20m, None, None, None, None).unwrap();
        let ids_20m: Vec<&str> = page_20m.messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids_20m, vec!["m0", "m1", "m2", "m3", "m4"]);
        assert_eq!(page_20m.oldest_offset, 0);
        assert_eq!(page_20m.latest_offset, off_after_m4);
        assert!(!page_20m.has_more);
        assert_eq!(page_20m.probe_sha256, expected_probe);

        // Scenario 3: Single-line file consisting ONLY of a 600 KiB incomplete line (no '\n', line_start == 0)
        let p_only = dir.join("only_incomplete_600k.jsonl");
        fs::write(&p_only, "Y".repeat(600 * 1024)).unwrap();
        let page_only =
            read_history_page(AgentKind::Claude, &p_only, None, None, None, None).unwrap();
        assert!(page_only.messages.is_empty());
        assert_eq!(page_only.oldest_offset, 0);
        assert_eq!(page_only.latest_offset, 0);
        assert!(!page_only.has_more);
        assert_eq!(page_only.probe_sha256, sha256_hex(&[]));

        // Scenario 4: Mid-line before_offset falling > 512 KiB into a 700 KiB line after m0..m4
        let p_mid = dir.join("mid_before_offset.jsonl");
        let line_700k = format!(
            "{{\"type\":\"user\",\"uuid\":\"u-700k\",\"message\":{{\"content\":\"{}\"}}}}\n",
            "M".repeat(700 * 1024)
        );
        fs::write(&p_mid, format!("{normal_prefix}{line_700k}")).unwrap();
        let page_mid = read_history_page(
            AgentKind::Claude,
            &p_mid,
            Some(off_after_m4 + 600 * 1024),
            None,
            None,
            None,
        )
        .unwrap();
        let ids_mid: Vec<&str> = page_mid.messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids_mid, vec!["m0", "m1", "m2", "m3", "m4"]);
        assert_eq!(page_mid.oldest_offset, 0);
        assert_eq!(page_mid.latest_offset, off_after_m4);
        assert!(!page_mid.has_more);

        // Scenario 5: 5 normal + 2 MiB complete base64 + 20 MiB incomplete tail
        let p_combo = dir.join("normal_2m_b64_20m_incomplete.jsonl");
        {
            let mut f = File::create(&p_combo).unwrap();
            f.write_all(normal_prefix.as_bytes()).unwrap();
            let b64_2m = "R".repeat(2 * 1024 * 1024);
            let line_2m = format!(
                "{{\"type\":\"assistant\",\"uuid\":\"a-2m\",\"message\":{{\"content\":\"Done data:image/png;base64,{b64_2m}==\"}}}}\n"
            );
            f.write_all(line_2m.as_bytes()).unwrap();
            let chunk = vec![b'z'; 64 * 1024];
            for _ in 0..320 {
                f.write_all(&chunk).unwrap();
            }
        }
        let c1 = read_history_page(AgentKind::Claude, &p_combo, None, None, None, None).unwrap();
        assert_eq!(c1.messages.len(), 1);
        assert_eq!(c1.messages[0].id, "a-2m");
        assert_eq!(c1.messages[0].text, "Done [image omitted]");
        assert_eq!(c1.oldest_offset, off_after_m4);
        assert!(c1.has_more);

        let c2 = read_history_page(
            AgentKind::Claude,
            &p_combo,
            Some(c1.oldest_offset),
            None,
            None,
            None,
        )
        .unwrap();
        let c2_ids: Vec<&str> = c2.messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(c2_ids, vec!["m0", "m1", "m2", "m3", "m4"]);
        assert_eq!(c2.oldest_offset, 0);
        assert!(!c2.has_more);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn summarize_session_handles_2mib_base64_followed_by_trailing_non_message_records() {
        let dir = unique_temp_dir("summary-2m-b64-trailing-non-msg");
        let meta = MetadataStore::default();
        let archive = ArchiveStore::default();
        let active = HashSet::new();
        let live = LiveActivity::default();
        let server_workspaces = ServerWorkspaceIndex::default();
        let ctx = DiscoveryContext {
            active_session_ids: &active,
            connected_session_ids: &active,
            metadata: &meta,
            archive: &archive,
            live: &live,
            include_archived: true,
            server_workspaces: &server_workspaces,
        };
        let now = now_ms();
        let b64_2m = "T".repeat(2 * 1024 * 1024);

        // (a) Claude 2 MiB completed assistant screenshot (mtime = now - 1s) + file-history-snapshot + progress
        let p_a = dir.join("case_a.jsonl");
        fs::write(
            &p_a,
            format!(
                "{{\"type\":\"user\",\"uuid\":\"u1\",\"message\":{{\"content\":\"send\"}}}}\n\
                 {{\"type\":\"assistant\",\"uuid\":\"a-img\",\"message\":{{\"content\":\"Final answer text data:image/png;base64,{b64_2m}==\"}}}}\n\
                 {{\"type\":\"file-history-snapshot\",\"snapshot\":{{\"files\":[]}}}}\n\
                 {{\"type\":\"progress\",\"data\":{{\"pct\":100}}}}\n"
            ),
        )
        .unwrap();
        let s_a = summarize_session(&ClaudeProvider, &p_a, now - 1_000, &ctx);
        assert!(!s_a.working, "completed 2 MiB assistant + snapshot must report working=false");
        assert_eq!(s_a.last_assistant_id.as_deref(), Some("a-img"));
        let prev_a = s_a.last_assistant_preview.as_deref().unwrap_or("");
        assert!(prev_a.contains("Final answer text"), "prev_a={prev_a}");
        assert!(!prev_a.contains("TTTT"));

        // (b) Claude 2 MiB user screenshot (mtime = now - 15s) + progress + isSidechain: true
        let p_b = dir.join("case_b.jsonl");
        fs::write(
            &p_b,
            format!(
                "{{\"type\":\"assistant\",\"uuid\":\"a0\",\"message\":{{\"content\":\"ready\"}}}}\n\
                 {{\"type\":\"user\",\"uuid\":\"u-img\",\"message\":{{\"content\":[{{\"type\":\"image\",\"source\":{{\"type\":\"base64\",\"media_type\":\"image/png\",\"data\":\"{b64_2m}\"}}}}]}}}}\n\
                 {{\"type\":\"progress\",\"data\":{{\"step\":1}}}}\n\
                 {{\"type\":\"assistant\",\"uuid\":\"a-side\",\"isSidechain\":true,\"message\":{{\"content\":\"side\"}}}}\n"
            ),
        )
        .unwrap();
        let s_b = summarize_session(&ClaudeProvider, &p_b, now - 15_000, &ctx);
        assert!(s_b.working, "2 MiB user screenshot + progress/isSidechain must report working=true");

        // (c) Claude 2 MiB user screenshot (mtime = now - 15s) + trailing [Request interrupted by user]
        let p_c = dir.join("case_c.jsonl");
        fs::write(
            &p_c,
            format!(
                "{{\"type\":\"user\",\"uuid\":\"u-img\",\"message\":{{\"content\":[{{\"type\":\"image\",\"source\":{{\"type\":\"base64\",\"media_type\":\"image/png\",\"data\":\"{b64_2m}\"}}}}]}}}}\n\
                 {{\"type\":\"user\",\"uuid\":\"u-int\",\"message\":{{\"content\":\"[Request interrupted by user]\"}}}}\n"
            ),
        )
        .unwrap();
        let s_c = summarize_session(&ClaudeProvider, &p_c, now - 15_000, &ctx);
        assert!(!s_c.working, "trailing interrupt must override straddling user turn to working=false");

        // (d) Claude 2 MiB assistant screenshot + trailing custom-title
        let p_d = dir.join("case_d.jsonl");
        fs::write(
            &p_d,
            format!(
                "{{\"type\":\"assistant\",\"uuid\":\"a-img\",\"message\":{{\"content\":\"Done answer data:image/png;base64,{b64_2m}==\"}}}}\n\
                 {{\"type\":\"custom-title\",\"customTitle\":\"Renamed Session\"}}\n"
            ),
        )
        .unwrap();
        let s_d = summarize_session(&ClaudeProvider, &p_d, now - 1_000, &ctx);
        assert!(!s_d.working);
        assert_eq!(s_d.title, "Renamed Session");
        assert!(s_d
            .last_assistant_preview
            .as_deref()
            .is_some_and(|p| p.contains("Done answer")));

        // (e) Antigravity 2 MiB assistant screenshot + trailing multibot_turn_end
        let p_e = dir.join("case_e.history.jsonl");
        fs::write(
            &p_e,
            format!(
                "{{\"type\":\"multibot_meta\",\"session_id\":\"agy-2m\",\"agent\":\"antigravity\",\"cwd\":\"/repo\",\"title\":\"Agy Shot\"}}\n\
                 {{\"type\":\"multibot_history\",\"row\":{{\"id\":\"a-agy\",\"role\":\"assistant\",\"text\":\"Agy done data:image/png;base64,{b64_2m}==\"}}}}\n\
                 {{\"type\":\"multibot_turn_end\",\"status\":\"completed\"}}\n"
            ),
        )
        .unwrap();
        let s_e = summarize_session(&AntigravityProvider, &p_e, now - 1_000, &ctx);
        assert!(!s_e.working);
        assert_eq!(s_e.last_assistant_id.as_deref(), Some("a-agy"));
        assert!(s_e
            .last_assistant_preview
            .as_deref()
            .is_some_and(|p| p.contains("Agy done") && !p.contains("TTTT")));

        // (f) Claude 2 MiB earlier screenshot + normal <64 KiB assistant message + file-history-snapshot (fast path)
        let p_f = dir.join("case_f.jsonl");
        fs::write(
            &p_f,
            format!(
                "{{\"type\":\"assistant\",\"uuid\":\"a-old\",\"message\":{{\"content\":\"Old data:image/png;base64,{b64_2m}==\"}}}}\n\
                 {{\"type\":\"assistant\",\"uuid\":\"a-new\",\"message\":{{\"content\":\"Fresh short reply\"}}}}\n\
                 {{\"type\":\"file-history-snapshot\",\"snapshot\":{{}}}}\n"
            ),
        )
        .unwrap();
        let s_f = summarize_session(&ClaudeProvider, &p_f, now - 1_000, &ctx);
        assert!(!s_f.working);
        assert_eq!(s_f.last_assistant_id.as_deref(), Some("a-new"));
        assert_eq!(s_f.last_assistant_preview.as_deref(), Some("Fresh short reply"));

        // (g) Claude 20 MiB spaced non-base64 + trailing file-history-snapshot (zero extra seeks)
        let p_g = dir.join("case_g.jsonl");
        {
            let mut f = File::create(&p_g).unwrap();
            f.write_all(b"{\"type\":\"assistant\",\"uuid\":\"a-20m\",\"message\":{\"content\":\"")
                .unwrap();
            let chunk = "hello world ".repeat(5461); // ~64 KiB spaced text
            for _ in 0..320 {
                f.write_all(chunk.as_bytes()).unwrap();
            }
            f.write_all(b"\"}}\n{\"type\":\"file-history-snapshot\",\"snapshot\":{}}\n")
                .unwrap();
        }
        let s_g = summarize_session(&ClaudeProvider, &p_g, now - 15_000, &ctx);
        assert!(!s_g.working);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn format_tool_summary_diffs_todos_and_questions_scrub_base64_before_truncation() {
        let raw_b64 = "A".repeat(300);

        // 1. format_tool_summary scrubs base64 BEFORE 60-char truncation
        let summary = format_tool_summary("Bash", &serde_json::json!({
            "command": format!("echo {raw_b64}")
        }));
        assert_eq!(summary, "Ran echo [image omitted]");
        assert!(!summary.contains("AAAA"));

        // 2. build_diff_block scrubs base64 in kept diff lines
        let diff = build_diff_block(
            Some("src/a.ts".to_string()),
            vec![
                "+img=data:image/png;base64,iVBORw0KGgo=".to_string(),
                format!("-raw={raw_b64}"),
            ],
        )
        .unwrap();
        assert_eq!(
            diff.lines,
            vec!["+img=[image omitted]", "-raw=[image omitted]"]
        );

        // 3. sanitize_todo_content scrubs before MAX_TODO_CONTENT_CHARS (200)
        assert_eq!(
            sanitize_todo_content(&format!("Verify data:image/png;base64,iVBORw0KGgo= {raw_b64}")),
            "Verify [image omitted] [image omitted]"
        );

        // 4. parse_prompt_questions scrubs question, header, label, description
        let qs = parse_prompt_questions(&serde_json::json!([{
            "question": "Pick data:image/png;base64,iVBORw0KGgo= now?",
            "header": "Hdr data:image/png;base64,iVBORw0KGgo=",
            "options": [{
                "label": "Opt data:image/png;base64,iVBORw0KGgo=",
                "description": format!("Desc {raw_b64}")
            }]
        }]));
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].question, "Pick [image omitted] now?");
        assert_eq!(qs[0].header, "Hdr [image omitted]");
        assert_eq!(qs[0].options[0].label, "Opt [image omitted]");
        assert_eq!(qs[0].options[0].description, "Desc [image omitted]");

        // 5. Claude customTitle with data URI is scrubbed via truncate_title
        let dir = unique_temp_dir("claude-custom-title-b64");
        let p_ct = dir.join("custom_title.jsonl");
        fs::write(
            &p_ct,
            "{\"type\":\"custom-title\",\"customTitle\":\"Title data:image/png;base64,iVBORw0KGgo= end\"}\n",
        )
        .unwrap();
        let meta = MetadataStore::default();
        let archive = ArchiveStore::default();
        let active = HashSet::new();
        let live = LiveActivity::default();
        let server_workspaces = ServerWorkspaceIndex::default();
        let ctx = DiscoveryContext {
            active_session_ids: &active,
            connected_session_ids: &active,
            metadata: &meta,
            archive: &archive,
            live: &live,
            include_archived: true,
            server_workspaces: &server_workspaces,
        };
        let s_ct = summarize_session(&ClaudeProvider, &p_ct, now_ms(), &ctx);
        assert_eq!(s_ct.title, "Title [image omitted] end");

        // 6. process::sanitize_title scrubs base64 before 63-char cap
        assert_eq!(
            crate::process::sanitize_title(&format!("Note data:image/png;base64,iVBORw0KGgo= {raw_b64}")),
            Some("Note [image omitted] [image omitted]".to_string())
        );

        // 7. StreamingBase64Scrubber::finish preserves byte order on trailing backslash
        assert_eq!(
            scrub_line_bytes(br#""abc\"#, 1024),
            Some(br#""abc\"#.to_vec())
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
