use crate::settings::MultibotSettings;
use anyhow::{Context, Result};
use multibot_proto::{AgentKind, SessionSummary};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const STARRED_WORKSPACE_ID: &str = "__starred__";
pub const ALL_WORKSPACES_ID: &str = "__all__";
pub const DEFAULT_WORKSPACE_ID: &str = "general";
pub const DEFAULT_WORKSPACE_NAME: &str = "General";
pub const RESERVED_OTHER_NAME: &str = "Other";
pub const MAX_SESSION_TITLE_CHARS: usize = 63;
pub const MAX_WORKSPACE_NAME_CHARS: usize = 63;
pub const MAX_WAITING_UNDO: usize = 32;
pub const MAX_PALETTE_RECENT_SESSIONS: usize = 5;
pub const PALETTE_MOST_USED_LIMIT: usize = 10;
pub const MAX_RECENT_VIEWS: usize = 64;

pub const FREQUENT_INPUT_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;
pub const FREQUENT_VISIT_WINDOW_MS: i64 = 6 * 60 * 60 * 1000;
pub const FREQUENT_MIN_VISITS: usize = 3;
pub const FREQUENT_DWELL_MS: i64 = 10 * 1000;
pub const FREQUENT_MAX_VISITS: usize = 128;
pub const SIDEBAR_FREQUENT_WINDOW_MS: i64 = 14 * 24 * 60 * 60 * 1000;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(1);

fn default_true() -> bool {
    true
}

/// Current Unix wall-clock timestamp in milliseconds.
pub fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Atomically writes `value` as pretty JSON to `path` via a unique sibling `.tmp` file + rename.
pub fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf);
    if let Some(ref dir) = parent {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create parent dir {}", dir.display()))?;
    }

    let bytes = serde_json::to_vec_pretty(value).context("serialize json payload")?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("state.json");
    let seq = TMP_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
    let pid = std::process::id();
    let tmp_name = format!("{file_name}.{pid}.{seq}.tmp");
    let tmp_path: PathBuf = match &parent {
        Some(dir) => dir.join(&tmp_name),
        None => PathBuf::from(&tmp_name),
    };

    let write_res = (|| -> Result<()> {
        let mut file = std::fs::File::create(&tmp_path)
            .with_context(|| format!("create temp file {}", tmp_path.display()))?;
        file.write_all(&bytes)
            .with_context(|| format!("write temp file {}", tmp_path.display()))?;
        file.flush()
            .with_context(|| format!("flush temp file {}", tmp_path.display()))?;
        let _ = file.sync_all();
        Ok(())
    })();

    if let Err(err) = write_res {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }

    if let Err(err) = replace_file_atomic(&tmp_path, path, file_name, pid, seq) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }

    Ok(())
}

#[cfg(not(windows))]
fn replace_file_atomic(
    tmp_path: &Path,
    target_path: &Path,
    _file_name: &str,
    _pid: u32,
    _seq: u64,
) -> Result<()> {
    std::fs::rename(tmp_path, target_path)
        .with_context(|| format!("rename {} -> {}", tmp_path.display(), target_path.display()))
}

#[cfg(windows)]
fn replace_file_atomic(
    tmp_path: &Path,
    target_path: &Path,
    file_name: &str,
    pid: u32,
    seq: u64,
) -> Result<()> {
    if std::fs::rename(tmp_path, target_path).is_ok() {
        return Ok(());
    }
    if target_path.exists() {
        let bak_name = format!("{file_name}.{pid}.{seq}.bak");
        let bak_path = match target_path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir.join(&bak_name),
            _ => PathBuf::from(&bak_name),
        };
        std::fs::rename(target_path, &bak_path).with_context(|| {
            format!(
                "move existing {} -> {}",
                target_path.display(),
                bak_path.display()
            )
        })?;
        match std::fs::rename(tmp_path, target_path) {
            Ok(()) => {
                let _ = std::fs::remove_file(&bak_path);
                return Ok(());
            }
            Err(err) => {
                let _ = std::fs::rename(&bak_path, target_path);
                return Err(err).with_context(|| {
                    format!(
                        "replace {} with {}",
                        target_path.display(),
                        tmp_path.display()
                    )
                });
            }
        }
    }
    std::fs::rename(tmp_path, target_path)
        .with_context(|| format!("rename {} -> {}", tmp_path.display(), target_path.display()))
}

// =============================================================================
// Canonical Session Keys & F2 Rename Helpers
// =============================================================================

pub fn agent_slug(agent: AgentKind) -> &'static str {
    match agent {
        AgentKind::Claude => "claude",
        AgentKind::Antigravity => "antigravity",
        AgentKind::Codex => "codex",
        AgentKind::Shell => "shell",
        AgentKind::Pi => "pi",
        AgentKind::Cursor => "cursor",
    }
}

pub fn canonical_session_key(host: &str, agent: AgentKind, session_id: &str) -> String {
    canonical_session_key_str(host, agent_slug(agent), session_id)
}

pub fn canonical_session_key_str(host: &str, agent: &str, session_id: &str) -> String {
    let trimmed_host = host.trim();
    let norm_host = if trimmed_host.is_empty() {
        "local".to_string()
    } else {
        trimmed_host.to_ascii_lowercase().replace(':', "_")
    };

    let trimmed_agent = agent.trim();
    let norm_agent = if trimmed_agent.is_empty() {
        "claude".to_string()
    } else {
        trimmed_agent.to_ascii_lowercase().replace(':', "_")
    };

    let norm_sid = session_id.trim();
    format!("{norm_host}:{norm_agent}:{norm_sid}")
}

pub fn parse_session_key(key: &str) -> Option<(&str, &str, &str)> {
    let mut parts = key.splitn(3, ':');
    let host = parts.next()?;
    let agent = parts.next()?;
    let sid = parts.next()?;
    if host.is_empty() || agent.is_empty() || sid.is_empty() {
        return None;
    }
    Some((host, agent, sid))
}

/// Unicode-scalar-safe truncation (never byte-slices inside a multi-byte UTF-8 character).
pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

/// Normalizes a workspace name that arrives from outside Multibot (the server-side
/// `multiprompt` state). Blank names are rejected so an import cannot create a nameless
/// workspace.
///
/// `Other` is deliberately allowed: the legacy app ships a workspace with that name, and the
/// ZigShell import already recreates it verbatim. `RESERVED_OTHER_NAME` only guards the
/// user-facing create and rename commands.
pub fn canonical_workspace_name(raw: &str) -> std::result::Result<String, WorkspaceError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(WorkspaceError::EmptyName);
    }
    Ok(truncate_chars(trimmed, MAX_WORKSPACE_NAME_CHARS))
}

/// Canonical `F2` / `/rename` title normalization:
/// trims whitespace, ignores empty/blank input (`None`), caps at 63 Unicode chars.
pub fn canonicalize_session_title(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(truncate_chars(trimmed, MAX_SESSION_TITLE_CHARS))
}

// =============================================================================
// Core Data Types
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceView {
    #[default]
    All,
    Frequent,
    Favorites,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub snoozed: bool,
    #[serde(default)]
    pub snoozed_until_ms: Option<i64>,
    #[serde(default)]
    pub position: usize,
    #[serde(default)]
    pub last_active_session: Option<String>,
}

impl Workspace {
    /// Returns true if the workspace is snoozed at `now_ms`.
    /// Expiry boundary is inclusive: at `now_ms == snoozed_until_ms` the workspace is awake.
    pub fn is_snoozed_at(&self, now_ms: i64) -> bool {
        match self.snoozed_until_ms {
            Some(until) => now_ms < until,
            None => self.snoozed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionGroup {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub collapsed: bool,
    /// Workspace ID or `STARRED_WORKSPACE_ID` for Starred-scoped groups.
    pub workspace_id: String,
    #[serde(default)]
    pub position: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    #[serde(default)]
    pub full_width: bool,
    #[serde(default)]
    pub hibernate_wake_on_reply: bool,
    #[serde(default)]
    pub hibernate_reply_boundary: Option<WaitingEntry>,
    #[serde(default)]
    pub dissolved_at_ms: Option<i64>,
    #[serde(default)]
    pub face: Option<u8>,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub remind_at_ms: Option<i64>,
    #[serde(default)]
    pub reminder_note: Option<String>,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub starred_pinned: bool,
    // Local pin choices must survive repeated legacy imports and live-key reconciliation.
    #[serde(default)]
    pub pin_overridden: bool,
    #[serde(default)]
    pub starred_pin_overridden: bool,
    #[serde(default)]
    pub group_id: Option<String>,
    #[serde(default)]
    pub starred_group_id: Option<String>,
    #[serde(default)]
    pub manual_order: Option<usize>,
    #[serde(default)]
    pub starred_manual_order: Option<usize>,
    #[serde(default)]
    pub muted: bool,
    #[serde(default)]
    pub last_used_ms: i64,
    #[serde(default)]
    pub last_input_ms: Option<i64>,
    #[serde(default)]
    pub frequent_visits_ms: Vec<i64>,
}

impl Default for SessionMeta {
    fn default() -> Self {
        Self {
            full_width: false,
            hibernate_wake_on_reply: false,
            hibernate_reply_boundary: None,
            dissolved_at_ms: None,
            face: None,
            color: None,
            remind_at_ms: None,
            reminder_note: None,
            workspace_id: None,
            pinned: false,
            starred_pinned: false,
            pin_overridden: false,
            starred_pin_overridden: false,
            group_id: None,
            starred_group_id: None,
            manual_order: None,
            starred_manual_order: None,
            muted: false,
            last_used_ms: 0,
            last_input_ms: None,
            frequent_visits_ms: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SessionLifecycle {
    #[default]
    Active,
    Done,
    Hibernated,
    Dissolved,
}

impl SessionLifecycle {
    pub const ALL: [Self; 4] = [Self::Active, Self::Done, Self::Hibernated, Self::Dissolved];

    pub fn label(self) -> &'static str {
        match self {
            Self::Active => "Inbox",
            Self::Done => "Done",
            Self::Hibernated => "Hibernated",
            Self::Dissolved => "Dissolved",
        }
    }

    pub fn go_key(key: &str) -> Option<Self> {
        match key {
            "i" => Some(Self::Active),
            "e" => Some(Self::Done),
            "h" => Some(Self::Hibernated),
            "d" => Some(Self::Dissolved),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTitleOverride {
    pub title: String,
    #[serde(default = "default_true")]
    pub manual: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UnreadBoundary {
    #[serde(default)]
    pub last_read_assistant_id: Option<String>,
    #[serde(default)]
    pub last_read_ms: i64,
    #[serde(default)]
    pub manual_unread: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WaitingEntry {
    pub archived_at_ms: i64,
    #[serde(default)]
    pub last_activity_ms: i64,
    #[serde(default)]
    pub last_assistant_id: Option<String>,
    #[serde(default)]
    pub was_unread: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitingUndoItem {
    pub session_key: String,
    pub entry: WaitingEntry,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentViewEntry {
    pub session_key: String,
    pub viewed_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxSplitSettings {
    #[serde(default = "default_true")]
    pub frequent: bool,
    #[serde(default = "default_true")]
    pub favorites: bool,
    #[serde(default = "default_true")]
    pub all: bool,
    #[serde(default = "default_true")]
    pub frequent_first: bool,
}

impl Default for InboxSplitSettings {
    fn default() -> Self {
        Self {
            frequent: true,
            favorites: true,
            all: true,
            frequent_first: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredReaction {
    pub message_id: String,
    pub emoji: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceStore {
    #[serde(default)]
    pub workspaces: Vec<Workspace>,
    #[serde(default)]
    pub favorites: HashSet<String>,
    #[serde(default)]
    pub groups: Vec<SessionGroup>,
    #[serde(default)]
    pub session_meta: HashMap<String, SessionMeta>,
    #[serde(default = "default_true")]
    pub auto_sort: bool,
    #[serde(default)]
    pub pinned_section_collapsed: bool,
    #[serde(default)]
    pub recents_section_collapsed: bool,
    #[serde(default)]
    pub session_titles: HashMap<String, SessionTitleOverride>,
    #[serde(default)]
    pub unread_boundaries: HashMap<String, UnreadBoundary>,
    #[serde(default)]
    pub waiting_sessions: HashMap<String, WaitingEntry>,
    #[serde(default)]
    pub waiting_undo_stack: Vec<WaitingUndoItem>,
    #[serde(default)]
    pub inbox_excluded: HashSet<String>,
    #[serde(default)]
    pub recent_views: Vec<RecentViewEntry>,
    #[serde(default)]
    pub inbox_settings: InboxSplitSettings,
    #[serde(default)]
    pub reactions: HashMap<String, Vec<StoredReaction>>,
}

impl Default for WorkspaceStore {
    fn default() -> Self {
        Self {
            workspaces: Vec::new(),
            favorites: HashSet::new(),
            groups: Vec::new(),
            session_meta: HashMap::new(),
            auto_sort: true,
            pinned_section_collapsed: false,
            recents_section_collapsed: false,
            session_titles: HashMap::new(),
            unread_boundaries: HashMap::new(),
            waiting_sessions: HashMap::new(),
            waiting_undo_stack: Vec::new(),
            inbox_excluded: HashSet::new(),
            recent_views: Vec::new(),
            inbox_settings: InboxSplitSettings::default(),
            reactions: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceError {
    EmptyName,
    ReservedName,
    NotFound(String),
    CannotDeleteStarred,
    NonEmptyRequiresTarget { session_count: usize },
    InvalidMoveTarget(String),
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyName => write!(f, "workspace name cannot be empty"),
            Self::ReservedName => write!(f, "\"Other\" is a reserved workspace name"),
            Self::NotFound(id) => write!(f, "workspace not found: {id}"),
            Self::CannotDeleteStarred => write!(f, "cannot delete the Starred workspace"),
            Self::NonEmptyRequiresTarget { session_count } => write!(
                f,
                "workspace still contains {session_count} session(s); choose a target workspace"
            ),
            Self::InvalidMoveTarget(id) => write!(f, "invalid target workspace: {id}"),
        }
    }
}

impl std::error::Error for WorkspaceError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteWorkspaceOutcome {
    pub deleted_id: String,
    pub moved_sessions: usize,
    pub moved_to: Option<String>,
    pub created_default_general: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReorderDirection {
    Up,
    Down,
}

/// Lightweight view passed into Sidebar, Inbox, and Palette engines.
#[derive(Debug, Clone)]
pub struct SessionInput<'a> {
    pub key: String,
    pub host: &'a str,
    pub summary: &'a SessionSummary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarRow {
    pub session_key: String,
    pub session_id: String,
    pub host: String,
    pub agent: AgentKind,
    pub title: String,
    pub cwd: Option<String>,
    pub workspace_id: String,
    pub workspace_name: String,
    pub pinned: bool,
    pub favorite: bool,
    pub unread: bool,
    pub working: bool,
    pub muted: bool,
    pub used_ms: i64,
    pub group_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarGroupView {
    pub group: SessionGroup,
    pub items: Vec<SidebarRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SidebarLayout {
    pub pinned_tiles: Vec<SidebarRow>,
    pub groups: Vec<SidebarGroupView>,
    pub recents: Vec<SidebarRow>,
    /// Exact block layout order: group members (by group position), ungrouped pinned, ungrouped recents.
    pub flat_display_order: Vec<String>,
    /// `Ctrl+1..9` jump order: pinned tiles FIRST, then non-collapsed group members, then recents.
    pub ctrl_digit_order: Vec<String>,
}

impl SidebarLayout {
    pub fn resolve_ctrl_digit(&self, digit: u8) -> Option<&str> {
        if !(1..=9).contains(&digit) {
            return None;
        }
        self.ctrl_digit_order
            .get((digit - 1) as usize)
            .map(String::as_str)
    }

    pub fn total_visible_sessions(&self) -> usize {
        self.pinned_tiles.len()
            + self.groups.iter().map(|g| g.items.len()).sum::<usize>()
            + self.recents.len()
    }
}

// =============================================================================
// Inbox & Frequent Types
// =============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxSplit {
    Frequent,
    #[default]
    All,
    Favorites,
    Workspace(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxRow {
    pub due_reminder: bool,
    pub reminder_note: Option<String>,
    pub session_key: String,
    pub session_id: String,
    pub host: String,
    pub agent: AgentKind,
    pub title: String,
    pub cwd: Option<String>,
    pub preview: Option<String>,
    pub workspace_id: String,
    pub workspace_name: String,
    pub workspace_position: usize,
    pub unread: bool,
    pub working: bool,
    pub favorite: bool,
    pub last_activity_ms: i64,
    pub last_visit_ms: i64,
    pub sort_ms: i64,
    pub frequent_key_ms: i64,
}

/// Tracks focused user-selected dwell periods so visits are recorded once after `>= 10s`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrontPeriodTracker {
    active_session_key: Option<String>,
    start_ms: i64,
    start_tick_ms: u64,
    recorded: bool,
}

impl FrontPeriodTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Updates the current focused selection. Losing focus or non-user-initiated selection
    /// cancels the pending dwell timer so focus loss resets the 10s clock.
    pub fn select(
        &mut self,
        session_key: Option<&str>,
        user_initiated: bool,
        focused: bool,
        now_ms: i64,
        tick_ms: u64,
    ) -> bool {
        if !user_initiated || !focused {
            self.reset();
            return false;
        }
        let Some(key) = session_key.filter(|k| !k.is_empty()) else {
            self.reset();
            return false;
        };
        if self.active_session_key.as_deref() == Some(key) {
            return true;
        }
        self.active_session_key = Some(key.to_string());
        self.start_ms = now_ms;
        self.start_tick_ms = tick_ms;
        self.recorded = false;
        true
    }

    pub fn reset(&mut self) {
        self.active_session_key = None;
        self.start_ms = 0;
        self.start_tick_ms = 0;
        self.recorded = false;
    }

    /// Emits `Some((session_key, start_ms))` at most once when dwell reaches `FREQUENT_DWELL_MS`.
    pub fn poll_visit(&mut self, tick_ms: u64) -> Option<(String, i64)> {
        if self.recorded {
            return None;
        }
        let key = self.active_session_key.as_ref()?;
        if tick_ms >= self.start_tick_ms
            && tick_ms.saturating_sub(self.start_tick_ms) >= FREQUENT_DWELL_MS as u64
        {
            self.recorded = true;
            return Some((key.clone(), self.start_ms));
        }
        None
    }
}

/// Inclusive 24h input recency check (`stamp >= now_ms - 24h`).
pub fn is_frequent_input_recent(last_input_ms: Option<i64>, now_ms: i64) -> bool {
    let cutoff = now_ms.saturating_sub(FREQUENT_INPUT_WINDOW_MS);
    last_input_ms.is_some_and(|t| t >= cutoff)
}

/// Qualifies for Frequent split if user typed within 24h OR has `>= 3` focused 10s visits in 6h.
pub fn qualifies_for_frequent(visits: &[i64], last_input_ms: Option<i64>, now_ms: i64) -> bool {
    if is_frequent_input_recent(last_input_ms, now_ms) {
        return true;
    }
    let cutoff = now_ms.saturating_sub(FREQUENT_VISIT_WINDOW_MS);
    visits.iter().filter(|&&v| v >= cutoff).count() >= FREQUENT_MIN_VISITS
}

/// Order key for the Frequent split: recent input timestamp if input is within 24h,
/// otherwise newest visit timestamp (even if a visit is newer than a recent input).
pub fn frequent_order_key(visits: &[i64], last_input_ms: Option<i64>, now_ms: i64) -> i64 {
    if is_frequent_input_recent(last_input_ms, now_ms) {
        return last_input_ms.unwrap_or(0);
    }
    visits.iter().copied().max().unwrap_or(0)
}

// =============================================================================
// WorkspaceStore Implementation
// =============================================================================

impl WorkspaceStore {
    pub fn replace_session_key(&mut self, old: &str, new: &str) {
        if old == new {
            return;
        }
        if let Some(value) = self.session_meta.remove(old) {
            self.session_meta.insert(new.into(), value);
        }
        if let Some(value) = self.session_titles.remove(old) {
            self.session_titles.insert(new.into(), value);
        }
        if let Some(value) = self.unread_boundaries.remove(old) {
            self.unread_boundaries.insert(new.into(), value);
        }
        if let Some(value) = self.waiting_sessions.remove(old) {
            self.waiting_sessions.insert(new.into(), value);
        }
        if let Some(value) = self.reactions.remove(old) {
            self.reactions.insert(new.into(), value);
        }
        if self.favorites.remove(old) {
            self.favorites.insert(new.into());
        }
        if self.inbox_excluded.remove(old) {
            self.inbox_excluded.insert(new.into());
        }
        for entry in &mut self.recent_views {
            if entry.session_key == old {
                entry.session_key = new.into();
            }
        }
        for entry in &mut self.waiting_undo_stack {
            if entry.session_key == old {
                entry.session_key = new.into();
            }
        }
    }

    pub fn new() -> Self {
        let mut store = Self::default();
        store.ensure_default_workspace();
        store
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read workspaces file {}", path.display()))?;
        let mut parsed: Self = serde_json::from_str(&text)
            .with_context(|| format!("parse workspaces json {}", path.display()))?;
        if parsed.workspaces.len() > 1 && parsed.workspaces.iter().all(|w| w.position == 0) {
            parsed.normalize_workspace_positions();
        }
        if !parsed
            .workspaces
            .iter()
            .any(|w| !w.archived && w.snoozed_until_ms.is_some())
        {
            parsed.ensure_default_workspace();
        }
        Ok(parsed)
    }

    pub fn load_or_default(path: &Path) -> Self {
        Self::load_from(path).unwrap_or_else(|_| Self::new())
    }

    pub fn load() -> Self {
        Self::load_or_default(&MultibotSettings::workspaces_path())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        atomic_write_json(path, self)
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&MultibotSettings::workspaces_path())
    }

    pub fn set_reminder(&mut self, key: &str, remind_at_ms: Option<i64>, note: Option<&str>) {
        let meta = self.session_meta.entry(key.to_string()).or_default();
        meta.remind_at_ms = remind_at_ms;
        if remind_at_ms.is_none() {
            meta.hibernate_reply_boundary = None;
        }
        meta.reminder_note = remind_at_ms.and_then(|_| {
            note.map(str::trim)
                .filter(|n| !n.is_empty())
                .map(|n| n.chars().take(200).collect())
        });
    }

    pub fn session_lifecycle(
        &self,
        key: &str,
        summary: &SessionSummary,
        now_ms: i64,
    ) -> SessionLifecycle {
        let meta = self.session_meta.get(key);
        if (summary.archived && summary.archive_kill_process)
            || meta.is_some_and(|m| m.dissolved_at_ms.is_some())
        {
            SessionLifecycle::Dissolved
        } else if meta.is_some_and(|m| m.remind_at_ms.is_some_and(|at| at > now_ms)) {
            SessionLifecycle::Hibernated
        } else if !self.reminder_due(key, now_ms)
            && (summary.archived || self.waiting_sessions.contains_key(key))
        {
            SessionLifecycle::Done
        } else {
            SessionLifecycle::Active
        }
    }

    pub fn move_to_inbox(&mut self, key: &str) {
        self.waiting_sessions.remove(key);
        self.set_reminder(key, None, None);
        if let Some(meta) = self.session_meta.get_mut(key) {
            meta.dissolved_at_ms = None;
        }
    }

    pub fn hibernate_session(
        &mut self,
        key: &str,
        summary: &SessionSummary,
        until: Option<i64>,
        note: Option<&str>,
        wake_on_reply: bool,
    ) {
        self.move_to_inbox(key);
        self.set_reminder(key, until, note);
        let meta = self.session_meta.get_mut(key).unwrap();
        meta.hibernate_wake_on_reply = wake_on_reply;
        meta.hibernate_reply_boundary = until.map(|_| WaitingEntry {
            archived_at_ms: now_unix_ms(),
            last_activity_ms: summary.last_assistant_ms.unwrap_or(summary.updated_at_ms),
            last_assistant_id: summary.last_assistant_id.clone(),
            was_unread: summary.needs_attention,
        });
    }

    pub fn dissolve_session(&mut self, key: &str, now_ms: i64) {
        self.move_to_inbox(key);
        self.session_meta
            .entry(key.to_string())
            .or_default()
            .dissolved_at_ms = Some(now_ms);
    }

    pub fn wake_due_sessions(&mut self, now_ms: i64) -> bool {
        let keys: Vec<String> = self
            .session_meta
            .iter()
            .filter(|(_, m)| {
                m.dissolved_at_ms.is_none() && m.remind_at_ms.is_some_and(|at| at <= now_ms)
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in &keys {
            self.move_to_inbox(key);
        }
        !keys.is_empty()
    }

    pub fn reminder_due(&self, key: &str, now_ms: i64) -> bool {
        self.session_meta
            .get(key)
            .and_then(|m| m.remind_at_ms)
            .is_some_and(|at| at <= now_ms)
    }

    /// Due reminders remain visible until dismissed; only future reminders need a timer.
    /// Overdue workspace snoozes still need one wake to clear their persisted flags.
    pub fn next_deadline_ms(&self, now_ms: i64) -> Option<i64> {
        self.session_meta
            .values()
            .filter_map(|m| m.remind_at_ms)
            .filter(|&at| at > now_ms)
            .chain(
                self.workspaces
                    .iter()
                    .filter(|w| !w.archived)
                    .filter_map(|w| w.snoozed_until_ms),
            )
            .min()
    }

    /// Applies the workspace names the legacy `multiprompt` server recorded for live sessions.
    ///
    /// `hints` pairs each live session key with the server-side workspace name for that chat.
    /// The workspace is created (or matched by name) on first sight, so a session lands in the
    /// workspace it belongs to instead of the catch-all bucket. A session the user already put
    /// in some workspace keeps its explicit choice: only sessions without any workspace yet are
    /// assigned, exactly like the legacy ZigShell import.
    ///
    /// Returns the number of sessions whose workspace changed.
    pub fn apply_server_workspace_hints<S: AsRef<str>>(&mut self, hints: &[(String, S)]) -> usize {
        let mut assigned = 0;
        for (session_key, name) in hints {
            let Ok(name) = canonical_workspace_name(name.as_ref()) else {
                continue;
            };
            let explicit = self
                .session_meta
                .get(session_key)
                .and_then(|m| m.workspace_id.as_deref())
                .is_some_and(|id| self.workspaces.iter().any(|w| w.id == id));
            if explicit {
                continue;
            }
            let workspace_id = self.ensure_named_workspace(&name);
            let meta = self.session_meta.entry(session_key.clone()).or_default();
            if meta.workspace_id.as_deref() != Some(workspace_id.as_str()) {
                meta.workspace_id = Some(workspace_id);
                meta.group_id = None;
                assigned += 1;
            }
        }
        if assigned > 0 {
            self.prune_empty_groups();
        }
        assigned
    }

    /// Returns the id of the workspace named `name`, creating it when no name matches.
    fn ensure_named_workspace(&mut self, name: &str) -> String {
        if let Some(existing) = self
            .workspaces
            .iter()
            .find(|w| w.name.eq_ignore_ascii_case(name))
        {
            return existing.id.clone();
        }
        let position = self.workspaces.len();
        let id = self.generate_unique_workspace_id(name);
        self.workspaces.push(Workspace {
            id: id.clone(),
            name: truncate_chars(name, MAX_WORKSPACE_NAME_CHARS),
            color: None,
            archived: false,
            snoozed: false,
            snoozed_until_ms: None,
            position,
            last_active_session: None,
        });
        id
    }

    /// Reconciles a live session list against everything Multibot knows about where those
    /// sessions belong: legacy ZigShell assignments first (across host and agent key variants),
    /// then the workspace names the server recorded for each chat.
    ///
    /// The legacy assignment wins when both exist, because the user curated it by hand.
    /// Returns the number of sessions whose workspace or metadata changed.
    pub fn reconcile_live_sessions<'a, I>(&mut self, sessions: I) -> usize
    where
        I: IntoIterator<Item = (&'a str, &'a SessionSummary)>,
    {
        let mut live_keys: Vec<String> = Vec::new();
        let mut hints: Vec<(String, String)> = Vec::new();
        for (host, summary) in sessions {
            let key = canonical_session_key(host, summary.agent, &summary.id);
            if let Some(workspace) = summary.workspace.as_deref() {
                hints.push((key.clone(), workspace.to_string()));
            }
            live_keys.push(key);
        }
        let mut changed = self.reconcile_live_session_keys(&live_keys);
        changed += self.apply_server_workspace_hints(&hints);
        changed
    }

    /// Applies the server's workspace name to one session that just spawned or changed.
    ///
    /// [`Self::reconcile_live_sessions`] also rescans every known session key for legacy
    /// aliases, which is right for a full refresh and too costly for a per-transcript-append
    /// event. This does only the O(1) part: place the one session that has a recorded
    /// workspace, and leave everything else alone.
    pub fn reconcile_session(&mut self, host: &str, summary: &SessionSummary) -> usize {
        let Some(workspace) = summary.workspace.as_deref() else {
            return 0;
        };
        let key = canonical_session_key(host, summary.agent, &summary.id);
        self.apply_server_workspace_hints(&[(key, workspace)])
    }

    /// Restores imported state when host aliases or legacy agent guesses differ from live keys.
    pub fn reconcile_live_session_keys(&mut self, live_keys: &[String]) -> usize {
        let mut by_sid: HashMap<String, Vec<String>> = HashMap::new();
        for key in self
            .session_meta
            .keys()
            .chain(self.session_titles.keys())
            .chain(self.inbox_excluded.iter())
        {
            let Some((_, _, sid)) = parse_session_key(key) else {
                continue;
            };
            // View history alone must not compete with an imported workspace assignment.
            let has_metadata = self.session_meta.get(key).is_some_and(|m| {
                m.workspace_id.is_some()
                    || m.pinned
                    || m.starred_pinned
                    || m.pin_overridden
                    || m.starred_pin_overridden
                    || m.muted
            }) || self.session_titles.contains_key(key)
                || self.inbox_excluded.contains(key);
            if sid.len() >= 8 && has_metadata {
                let keys = by_sid.entry(sid.to_string()).or_default();
                if !keys.contains(key) {
                    keys.push(key.clone());
                }
            }
        }
        let favorite_sids: HashSet<String> = self
            .favorites
            .iter()
            .filter_map(|key| parse_session_key(key))
            .filter(|(_, _, sid)| sid.len() >= 8)
            .map(|(_, _, sid)| sid.to_string())
            .collect();

        let mut updated = 0;
        for live_key in live_keys {
            let Some((_, live_agent, sid)) = parse_session_key(live_key) else {
                continue;
            };
            if sid.len() < 8 {
                continue;
            }
            let mut changed = false;
            if !self
                .session_meta
                .get(live_key)
                .is_some_and(|m| m.workspace_id.is_some())
            {
                let candidates: Vec<_> = by_sid
                    .get(sid)
                    .into_iter()
                    .flatten()
                    .filter(|key| *key != live_key)
                    .collect();
                let same_agent: Vec<_> = candidates
                    .iter()
                    .copied()
                    .filter(|key| {
                        parse_session_key(key).is_some_and(|(_, agent, _)| agent == live_agent)
                    })
                    .collect();
                let preferred = if same_agent.is_empty() {
                    &candidates
                } else {
                    &same_agent
                };
                if let [source] = preferred.as_slice() {
                    let mut meta = self.session_meta.get(live_key).cloned().unwrap_or_default();
                    let before = meta.clone();
                    if let Some(imported) = self.session_meta.get(*source) {
                        meta.workspace_id = imported
                            .workspace_id
                            .clone()
                            .filter(|id| self.workspace_by_id(id).is_some());
                        if !meta.pin_overridden {
                            meta.pinned = if imported.pin_overridden {
                                imported.pinned
                            } else {
                                meta.pinned || imported.pinned
                            };
                            meta.pin_overridden = imported.pin_overridden;
                        }
                        if !meta.starred_pin_overridden {
                            meta.starred_pinned = if imported.starred_pin_overridden {
                                imported.starred_pinned
                            } else {
                                meta.starred_pinned || imported.starred_pinned
                            };
                            meta.starred_pin_overridden = imported.starred_pin_overridden;
                        }
                        meta.muted |= imported.muted;
                    }
                    if meta != before {
                        self.session_meta.insert(live_key.clone(), meta);
                        changed = true;
                    }
                    if self.inbox_excluded.contains(*source) {
                        changed |= self.inbox_excluded.insert(live_key.clone());
                    }
                    if !self.session_titles.contains_key(live_key)
                        && let Some(title) = self.session_titles.get(*source).cloned()
                    {
                        self.session_titles.insert(live_key.clone(), title);
                        changed = true;
                    }
                }
            }
            if favorite_sids.contains(sid) {
                changed |= self.favorites.insert(live_key.clone());
            }
            updated += usize::from(changed);
        }
        updated
    }

    pub fn ensure_default_workspace(&mut self) -> bool {
        if self.workspaces.iter().any(|w| !w.archived && !w.snoozed) {
            return false;
        }
        if let Some(ws) = self
            .workspaces
            .iter_mut()
            .find(|w| w.id == DEFAULT_WORKSPACE_ID)
        {
            ws.archived = false;
            ws.snoozed = false;
            ws.snoozed_until_ms = None;
            return true;
        }
        let next_pos = self.workspaces.len();
        self.workspaces.push(Workspace {
            id: DEFAULT_WORKSPACE_ID.to_string(),
            name: DEFAULT_WORKSPACE_NAME.to_string(),
            color: None,
            archived: false,
            snoozed: false,
            snoozed_until_ms: None,
            position: next_pos,
            last_active_session: None,
        });
        true
    }

    /// Returns non-archived, non-snoozed workspaces ordered by `(position, original_index)`.
    pub fn visible_workspaces(&self, now_ms: i64) -> Vec<&Workspace> {
        let mut indexed: Vec<(usize, &Workspace)> = self
            .workspaces
            .iter()
            .enumerate()
            .filter(|(_, w)| !w.archived && !w.is_snoozed_at(now_ms))
            .collect();
        indexed.sort_by_key(|(idx, w)| (w.position, *idx));
        indexed.into_iter().map(|(_, w)| w).collect()
    }

    /// Returns non-archived workspaces that are currently snoozed at `now_ms`.
    pub fn snoozed_workspaces(&self, now_ms: i64) -> Vec<&Workspace> {
        let mut indexed: Vec<(usize, &Workspace)> = self
            .workspaces
            .iter()
            .enumerate()
            .filter(|(_, w)| !w.archived && w.is_snoozed_at(now_ms))
            .collect();
        indexed.sort_by_key(|(idx, w)| (w.position, *idx));
        indexed.into_iter().map(|(_, w)| w).collect()
    }

    /// Maps `Alt+1` -> `STARRED_WORKSPACE_ID`, `Alt+2..9` -> first 8 visible workspaces (`0..7`).
    pub fn resolve_alt_digit(&self, digit: u8, now_ms: i64) -> Option<String> {
        match digit {
            1 => Some(STARRED_WORKSPACE_ID.to_string()),
            2..=9 => self
                .visible_workspaces(now_ms)
                .get((digit - 2) as usize)
                .map(|w| w.id.clone()),
            _ => None,
        }
    }

    /// Cycles across `[ALL_WORKSPACES_ID, STARRED_WORKSPACE_ID, visible_workspaces...]` by `delta` (wrapping).
    pub fn cycle_workspace_id(&self, current_id: &str, delta: isize, now_ms: i64) -> String {
        let mut ring = Vec::with_capacity(self.workspaces.len() + 2);
        ring.push(ALL_WORKSPACES_ID.to_string());
        ring.push(STARRED_WORKSPACE_ID.to_string());
        for ws in self.visible_workspaces(now_ms) {
            ring.push(ws.id.clone());
        }
        let len = ring.len() as isize;
        let current_idx = ring.iter().position(|id| id == current_id).unwrap_or(0) as isize;
        let step = delta.rem_euclid(len);
        let next_idx = (current_idx + step).rem_euclid(len) as usize;
        ring[next_idx].clone()
    }

    pub fn workspace_by_id(&self, id: &str) -> Option<&Workspace> {
        self.workspaces.iter().find(|w| w.id == id)
    }

    pub fn default_workspace_id(&self) -> String {
        self.workspaces
            .iter()
            .enumerate()
            .filter(|(_, w)| !w.archived && !w.snoozed)
            .min_by_key(|(idx, w)| (w.position, *idx))
            .or_else(|| {
                self.workspaces
                    .iter()
                    .enumerate()
                    .filter(|(_, w)| !w.archived)
                    .min_by_key(|(idx, w)| (w.position, *idx))
            })
            .map(|(_, w)| w.id.clone())
            .or_else(|| self.workspaces.first().map(|w| w.id.clone()))
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string())
    }

    pub fn create_workspace(
        &mut self,
        name: &str,
        color: Option<String>,
    ) -> std::result::Result<Workspace, WorkspaceError> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(WorkspaceError::EmptyName);
        }
        if trimmed.eq_ignore_ascii_case(RESERVED_OTHER_NAME) {
            return Err(WorkspaceError::ReservedName);
        }
        let clean_name = truncate_chars(trimmed, MAX_WORKSPACE_NAME_CHARS);
        let id = self.generate_unique_workspace_id(&clean_name);
        let ws = Workspace {
            id,
            name: clean_name,
            color,
            archived: false,
            snoozed: false,
            snoozed_until_ms: None,
            position: self.workspaces.len(),
            last_active_session: None,
        };
        self.workspaces.push(ws.clone());
        Ok(ws)
    }

    fn generate_unique_workspace_id(&self, name: &str) -> String {
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
        let base = if slug.is_empty() || slug == STARRED_WORKSPACE_ID {
            "ws".to_string()
        } else {
            slug
        };

        if base != STARRED_WORKSPACE_ID
            && !self
                .workspaces
                .iter()
                .any(|w| w.id.eq_ignore_ascii_case(&base))
        {
            return base;
        }
        let mut suffix = 2usize;
        loop {
            let candidate = format!("{base}-{suffix}");
            if candidate != STARRED_WORKSPACE_ID
                && !self
                    .workspaces
                    .iter()
                    .any(|w| w.id.eq_ignore_ascii_case(&candidate))
            {
                return candidate;
            }
            suffix += 1;
        }
    }

    pub fn rename_workspace(
        &mut self,
        id: &str,
        new_name: &str,
    ) -> std::result::Result<(), WorkspaceError> {
        let ws = self
            .workspaces
            .iter_mut()
            .find(|w| w.id == id)
            .ok_or_else(|| WorkspaceError::NotFound(id.to_string()))?;

        if ws.name.trim().eq_ignore_ascii_case(RESERVED_OTHER_NAME) {
            return Err(WorkspaceError::ReservedName);
        }
        let trimmed = new_name.trim();
        if trimmed.is_empty() {
            return Err(WorkspaceError::EmptyName);
        }
        if trimmed.eq_ignore_ascii_case(RESERVED_OTHER_NAME) {
            return Err(WorkspaceError::ReservedName);
        }
        ws.name = truncate_chars(trimmed, MAX_WORKSPACE_NAME_CHARS);
        Ok(())
    }

    pub fn set_workspace_color(
        &mut self,
        id: &str,
        color: Option<String>,
    ) -> std::result::Result<(), WorkspaceError> {
        let ws = self
            .workspaces
            .iter_mut()
            .find(|w| w.id == id)
            .ok_or_else(|| WorkspaceError::NotFound(id.to_string()))?;
        ws.color = color;
        Ok(())
    }

    pub fn reorder_workspace(
        &mut self,
        id: &str,
        new_index: usize,
    ) -> std::result::Result<(), WorkspaceError> {
        let idx = self
            .workspaces
            .iter()
            .position(|w| w.id == id)
            .ok_or_else(|| WorkspaceError::NotFound(id.to_string()))?;
        let ws = self.workspaces.remove(idx);
        let clamped = new_index.min(self.workspaces.len());
        self.workspaces.insert(clamped, ws);
        self.normalize_workspace_positions();
        Ok(())
    }

    pub fn sort_workspaces_by_name(&mut self) {
        self.workspaces.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.id.cmp(&b.id))
        });
        self.normalize_workspace_positions();
    }

    fn normalize_workspace_positions(&mut self) {
        for (i, ws) in self.workspaces.iter_mut().enumerate() {
            ws.position = i;
        }
    }

    pub fn archive_workspace(&mut self, id: &str) -> std::result::Result<(), WorkspaceError> {
        let ws = self
            .workspaces
            .iter_mut()
            .find(|w| w.id == id)
            .ok_or_else(|| WorkspaceError::NotFound(id.to_string()))?;
        ws.archived = true;
        ws.snoozed = false;
        ws.snoozed_until_ms = None;
        self.ensure_default_workspace();
        Ok(())
    }

    pub fn dissolve_workspace(
        &mut self,
        id: &str,
        session_keys: &[String],
        now_ms: i64,
    ) -> std::result::Result<(), WorkspaceError> {
        if self.workspace_by_id(id).is_none() {
            return Err(WorkspaceError::NotFound(id.to_string()));
        }
        let keys: HashSet<String> = self
            .session_meta
            .keys()
            .chain(session_keys.iter())
            .filter(|key| self.effective_workspace_id(key) == id)
            .cloned()
            .collect();
        for key in keys {
            self.assign_session_workspace(&key, id);
            self.dissolve_session(&key, now_ms);
        }
        // Keep a landing workspace without restoring the one being dissolved.
        if !self
            .workspaces
            .iter()
            .any(|w| w.id != id && !w.archived && !w.snoozed)
        {
            self.create_workspace(DEFAULT_WORKSPACE_NAME, None)?;
        }
        self.archive_workspace(id)
    }

    pub fn restore_workspace(&mut self, id: &str) -> std::result::Result<(), WorkspaceError> {
        let ws = self
            .workspaces
            .iter_mut()
            .find(|w| w.id == id)
            .ok_or_else(|| WorkspaceError::NotFound(id.to_string()))?;
        ws.archived = false;
        Ok(())
    }

    pub fn can_snooze_workspace(&self, id: &str, now_ms: i64) -> bool {
        let visible = self.visible_workspaces(now_ms);
        visible.len() > 1 && visible.iter().any(|w| w.id == id)
    }

    /// Snoozes a non-archived workspace until `until_ms` and returns the next visible workspace ID.
    pub fn snooze_workspace(
        &mut self,
        id: &str,
        until_ms: i64,
        now_ms: i64,
    ) -> std::result::Result<Option<String>, WorkspaceError> {
        let ws = self
            .workspaces
            .iter_mut()
            .find(|w| w.id == id && !w.archived)
            .ok_or_else(|| WorkspaceError::NotFound(id.to_string()))?;
        ws.snoozed = true;
        ws.snoozed_until_ms = Some(until_ms);

        let next_visible = self
            .visible_workspaces(now_ms)
            .into_iter()
            .find(|w| w.id != id)
            .map(|w| w.id.clone());
        Ok(next_visible)
    }

    pub fn wake_workspace(&mut self, id: &str) -> bool {
        if let Some(ws) = self.workspaces.iter_mut().find(|w| w.id == id) {
            let was = ws.snoozed || ws.snoozed_until_ms.is_some();
            ws.snoozed = false;
            ws.snoozed_until_ms = None;
            return was;
        }
        false
    }

    /// Wakes all snoozed workspaces whose `snoozed_until_ms <= now_ms` (inclusive boundary).
    pub fn wake_due_workspaces(&mut self, now_ms: i64) -> Vec<String> {
        let mut woken = Vec::new();
        for ws in &mut self.workspaces {
            if let Some(until) = ws.snoozed_until_ms
                && until <= now_ms
            {
                ws.snoozed = false;
                ws.snoozed_until_ms = None;
                woken.push(ws.id.clone());
            }
        }
        woken
    }

    pub fn delete_workspace(
        &mut self,
        id: &str,
        move_sessions_to: Option<&str>,
    ) -> std::result::Result<DeleteWorkspaceOutcome, WorkspaceError> {
        if id == STARRED_WORKSPACE_ID {
            return Err(WorkspaceError::CannotDeleteStarred);
        }
        let ws_idx = self
            .workspaces
            .iter()
            .position(|w| w.id == id)
            .ok_or_else(|| WorkspaceError::NotFound(id.to_string()))?;

        let session_count = self
            .session_meta
            .values()
            .filter(|m| m.workspace_id.as_deref() == Some(id))
            .count();

        let (moved_sessions, moved_to) = if session_count == 0 {
            let removed_group_ids: HashSet<String> = self
                .groups
                .iter()
                .filter(|g| g.workspace_id == id)
                .map(|g| g.id.clone())
                .collect();
            self.groups.retain(|g| g.workspace_id != id);
            if !removed_group_ids.is_empty() {
                for meta in self.session_meta.values_mut() {
                    if meta
                        .group_id
                        .as_ref()
                        .is_some_and(|gid| removed_group_ids.contains(gid))
                    {
                        meta.group_id = None;
                    }
                }
            }
            (0, None)
        } else {
            let target_id =
                move_sessions_to.ok_or(WorkspaceError::NonEmptyRequiresTarget { session_count })?;
            if target_id == id || target_id == STARRED_WORKSPACE_ID {
                return Err(WorkspaceError::InvalidMoveTarget(target_id.to_string()));
            }
            let target_valid = self
                .workspaces
                .iter()
                .any(|w| w.id == target_id && !w.archived);
            if !target_valid {
                return Err(WorkspaceError::InvalidMoveTarget(target_id.to_string()));
            }

            for meta in self.session_meta.values_mut() {
                if meta.workspace_id.as_deref() == Some(id) {
                    meta.workspace_id = Some(target_id.to_string());
                }
            }
            for group in &mut self.groups {
                if group.workspace_id == id {
                    group.workspace_id = target_id.to_string();
                }
            }
            (session_count, Some(target_id.to_string()))
        };

        self.workspaces.remove(ws_idx);
        let created_default_general = self.ensure_default_workspace();
        self.normalize_workspace_positions();

        Ok(DeleteWorkspaceOutcome {
            deleted_id: id.to_string(),
            moved_sessions,
            moved_to,
            created_default_general,
        })
    }

    // =========================================================================
    // Session Titles (`F2` rename), Assignments, Favorites, Pins, Groups
    // =========================================================================

    pub fn effective_title<'a>(&'a self, session_key: &str, fallback_title: &'a str) -> &'a str {
        self.session_titles
            .get(session_key)
            .map(|o| o.title.as_str())
            .filter(|t| !t.is_empty())
            .unwrap_or(fallback_title)
    }

    pub fn apply_session_rename(&mut self, session_key: &str, raw: &str) -> Option<String> {
        let clean = canonicalize_session_title(raw)?;
        self.session_titles.insert(
            session_key.to_string(),
            SessionTitleOverride {
                title: clean.clone(),
                manual: true,
            },
        );
        Some(clean)
    }

    pub fn assign_session_workspace(&mut self, session_key: &str, workspace_id: &str) {
        let meta = self
            .session_meta
            .entry(session_key.to_string())
            .or_default();
        if meta.workspace_id.as_deref() != Some(workspace_id) {
            meta.workspace_id = Some(workspace_id.to_string());
            meta.group_id = None;
            self.prune_empty_groups();
        }
    }

    pub fn is_favorite(&self, session_key: &str) -> bool {
        self.favorites.contains(session_key)
    }

    pub fn toggle_favorite(&mut self, session_key: &str) -> bool {
        if self.favorites.remove(session_key) {
            if let Some(meta) = self.session_meta.get_mut(session_key) {
                meta.starred_group_id = None;
            }
            self.prune_empty_starred_groups();
            false
        } else {
            self.favorites.insert(session_key.to_string());
            true
        }
    }

    pub fn is_pinned(&self, session_key: &str, in_starred: bool) -> bool {
        self.session_meta.get(session_key).is_some_and(|m| {
            if in_starred {
                m.starred_pinned
            } else {
                m.pinned
            }
        })
    }

    pub fn toggle_pin(&mut self, session_key: &str, in_starred: bool) -> bool {
        let meta = self
            .session_meta
            .entry(session_key.to_string())
            .or_default();
        if in_starred {
            meta.starred_pinned = !meta.starred_pinned;
            meta.starred_pin_overridden = true;
            meta.starred_pinned
        } else {
            meta.pinned = !meta.pinned;
            meta.pin_overridden = true;
            meta.pinned
        }
    }

    pub fn toggle_session_muted(&mut self, session_key: &str) -> bool {
        let meta = self
            .session_meta
            .entry(session_key.to_string())
            .or_default();
        meta.muted = !meta.muted;
        meta.muted
    }

    pub fn toggle_inbox_excluded(&mut self, session_key: &str) -> bool {
        if self.inbox_excluded.remove(session_key) {
            false
        } else {
            self.inbox_excluded.insert(session_key.to_string());
            true
        }
    }

    pub fn create_group(
        &mut self,
        workspace_id: &str,
        name: &str,
        color: Option<String>,
    ) -> SessionGroup {
        let trimmed = name.trim();
        let clean_name = if trimmed.is_empty() {
            "Group".to_string()
        } else {
            truncate_chars(trimmed, MAX_WORKSPACE_NAME_CHARS)
        };
        let next_pos = self
            .groups
            .iter()
            .filter(|g| g.workspace_id == workspace_id)
            .map(|g| g.position.saturating_add(1))
            .max()
            .unwrap_or(0);
        let id = self.generate_unique_group_id(workspace_id);
        let group = SessionGroup {
            id,
            name: clean_name,
            color,
            collapsed: false,
            workspace_id: workspace_id.to_string(),
            position: next_pos,
        };
        self.groups.push(group.clone());
        group
    }

    fn generate_unique_group_id(&self, workspace_id: &str) -> String {
        let prefix = if workspace_id == STARRED_WORKSPACE_ID {
            "starred-grp"
        } else {
            "grp"
        };
        let mut idx = 1usize;
        loop {
            let candidate = format!("{prefix}-{idx}");
            if !self.groups.iter().any(|g| g.id == candidate) {
                return candidate;
            }
            idx += 1;
        }
    }

    pub fn toggle_group_collapsed(&mut self, group_id: &str) -> Option<bool> {
        let g = self.groups.iter_mut().find(|g| g.id == group_id)?;
        g.collapsed = !g.collapsed;
        Some(g.collapsed)
    }

    /// `Ctrl+G`: collapses or expands the group containing `session_key` inside `workspace_id`.
    /// Returns `None` (no-op) when the session is ungrouped in that scope.
    pub fn toggle_session_group_collapsed(
        &mut self,
        session_key: &str,
        workspace_id: &str,
    ) -> Option<bool> {
        let in_starred = workspace_id == STARRED_WORKSPACE_ID;
        let group_id = self.session_meta.get(session_key).and_then(|m| {
            if in_starred {
                m.starred_group_id.clone()
            } else {
                m.group_id.clone()
            }
        })?;
        if !self
            .groups
            .iter()
            .any(|g| g.id == group_id && g.workspace_id == workspace_id)
        {
            return None;
        }
        self.toggle_group_collapsed(&group_id)
    }

    /// Removes starred groups that have zero favorited members referencing them.
    pub fn prune_empty_starred_groups(&mut self) {
        let active_starred_groups: HashSet<String> = self
            .favorites
            .iter()
            .filter_map(|key| {
                self.session_meta
                    .get(key)
                    .and_then(|m| m.starred_group_id.clone())
            })
            .collect();
        self.groups.retain(|g| {
            g.workspace_id != STARRED_WORKSPACE_ID || active_starred_groups.contains(&g.id)
        });
    }

    /// Removes any group (normal or starred) that has zero referencing sessions.
    pub fn prune_empty_groups(&mut self) {
        let active_normal_groups: HashSet<String> = self
            .session_meta
            .values()
            .filter_map(|m| m.group_id.clone())
            .collect();
        let active_starred_groups: HashSet<String> = self
            .favorites
            .iter()
            .filter_map(|key| {
                self.session_meta
                    .get(key)
                    .and_then(|m| m.starred_group_id.clone())
            })
            .collect();

        self.groups.retain(|g| {
            if g.workspace_id == STARRED_WORKSPACE_ID {
                active_starred_groups.contains(&g.id)
            } else {
                active_normal_groups.contains(&g.id)
            }
        });
    }

    /// Toggles group membership for `session_key` inside `workspace_id`.
    /// If already grouped in this scope, ungroups and prunes empty groups;
    /// otherwise assigns to `preferred_group_id` (if valid in this scope) or creates a new group.
    pub fn toggle_group_for_session(
        &mut self,
        session_key: &str,
        workspace_id: &str,
        preferred_group_id: Option<&str>,
        new_group_name: &str,
    ) -> Option<String> {
        let in_starred = workspace_id == STARRED_WORKSPACE_ID;
        let already_in_group = self.session_meta.get(session_key).and_then(|m| {
            if in_starred {
                m.starred_group_id.clone()
            } else {
                m.group_id.clone()
            }
        });

        if already_in_group.is_some() {
            if let Some(meta) = self.session_meta.get_mut(session_key) {
                if in_starred {
                    meta.starred_group_id = None;
                } else {
                    meta.group_id = None;
                }
            }
            self.prune_empty_groups();
            return None;
        }

        let target_gid = match preferred_group_id {
            Some(gid)
                if self
                    .groups
                    .iter()
                    .any(|g| g.id == gid && g.workspace_id == workspace_id) =>
            {
                gid.to_string()
            }
            _ => self.create_group(workspace_id, new_group_name, None).id,
        };

        let meta = self
            .session_meta
            .entry(session_key.to_string())
            .or_default();
        if in_starred {
            meta.starred_group_id = Some(target_gid.clone());
        } else {
            meta.group_id = Some(target_gid.clone());
        }
        Some(target_gid)
    }

    // =========================================================================
    // Sidebar Two-Tier Auto-Sort, Layout & Manual Reorder
    // =========================================================================

    pub fn effective_workspace_id(&self, session_key: &str) -> String {
        if let Some(ws_id) = self
            .session_meta
            .get(session_key)
            .and_then(|m| m.workspace_id.as_deref())
            && self.workspaces.iter().any(|w| w.id == ws_id)
        {
            return ws_id.to_string();
        }
        self.default_workspace_id()
    }

    pub fn workspace_directories(
        &self,
        workspace_id: &str,
        host: &str,
        sessions: &[SessionInput<'_>],
    ) -> Vec<String> {
        let mut directories = Vec::new();
        for input in sessions {
            if input.host != host
                || input.summary.archived
                || input.summary.cwd_exists == Some(false)
            {
                continue;
            }
            let in_workspace = match workspace_id {
                ALL_WORKSPACES_ID => true,
                STARRED_WORKSPACE_ID => self.favorites.contains(&input.key),
                _ => self.effective_workspace_id(&input.key) == workspace_id,
            };
            if !in_workspace {
                continue;
            }
            if let Some(cwd) = input
                .summary
                .cwd
                .as_deref()
                .filter(|cwd| !cwd.trim().is_empty())
            {
                directories.push(cwd.to_string());
            }
        }
        directories.sort();
        directories.dedup();
        directories
    }

    pub fn compute_sidebar_layout(
        &self,
        workspace_id: &str,
        sessions: &[SessionInput<'_>],
        unread_only: bool,
        now_ms: i64,
    ) -> SidebarLayout {
        self.compute_sidebar_layout_with_options(
            workspace_id,
            WorkspaceView::All,
            sessions,
            unread_only,
            None,
            now_ms,
        )
    }

    pub fn compute_sidebar_layout_with_options(
        &self,
        workspace_id: &str,
        workspace_view: WorkspaceView,
        sessions: &[SessionInput<'_>],
        unread_only: bool,
        active_session_key: Option<&str>,
        now_ms: i64,
    ) -> SidebarLayout {
        let in_starred = workspace_id == STARRED_WORKSPACE_ID;
        let in_all = workspace_id == ALL_WORKSPACES_ID;

        let ws_name_map: HashMap<&str, &str> = self
            .workspaces
            .iter()
            .map(|w| (w.id.as_str(), w.name.as_str()))
            .collect();

        let valid_group_ids: HashSet<&str> = self
            .groups
            .iter()
            .filter(|g| g.workspace_id == workspace_id)
            .map(|g| g.id.as_str())
            .collect();

        struct Candidate {
            orig_pos: usize,
            manual_order: Option<usize>,
            frequent_count: usize,
            row: SidebarRow,
        }

        let mut grouped_buckets: HashMap<String, Vec<Candidate>> = HashMap::new();
        let mut pinned_candidates: Vec<Candidate> = Vec::new();
        let mut ungrouped_recents: Vec<Candidate> = Vec::new();

        for (orig_pos, input) in sessions.iter().enumerate() {
            let lifecycle = self.session_lifecycle(&input.key, input.summary, now_ms);
            // Keep the open session visible while an unarchive request or workspace wake settles.
            let keep_active = in_all
                && active_session_key == Some(input.key.as_str())
                && lifecycle != SessionLifecycle::Dissolved;
            if lifecycle != SessionLifecycle::Active && !keep_active {
                continue;
            }
            let meta = self.session_meta.get(&input.key);
            let eff_ws = self.effective_workspace_id(&input.key);
            if !keep_active
                && self
                    .workspace_by_id(&eff_ws)
                    .is_some_and(|ws| ws.is_snoozed_at(now_ms))
            {
                continue;
            }

            if in_starred {
                if !self.favorites.contains(&input.key) {
                    continue;
                }
            } else if !in_all && eff_ws != workspace_id {
                continue;
            }

            let mut frequent_count = 0;
            let mut last_frequent_visit_ms = 0;
            match workspace_view {
                WorkspaceView::All => {}
                WorkspaceView::Frequent => {
                    let cutoff = now_ms.saturating_sub(SIDEBAR_FREQUENT_WINDOW_MS);
                    let visits = meta.map(|m| m.frequent_visits_ms.as_slice()).unwrap_or(&[]);
                    for &visit in visits {
                        if (cutoff..=now_ms).contains(&visit) {
                            frequent_count += 1;
                            last_frequent_visit_ms = last_frequent_visit_ms.max(visit);
                        }
                    }
                    // Older stores tracked input without recording a visit.
                    if meta
                        .and_then(|m| m.last_input_ms)
                        .is_some_and(|t| (cutoff..=now_ms).contains(&t))
                    {
                        frequent_count = frequent_count.max(1);
                    }
                    if frequent_count == 0 {
                        continue;
                    }
                }
                WorkspaceView::Favorites => {
                    if !self.favorites.contains(&input.key) {
                        continue;
                    }
                }
            }

            let unread = self.is_session_unread(&input.key, input.summary);
            if unread_only && !unread && active_session_key != Some(input.key.as_str()) {
                continue;
            }

            let eff_pinned = meta
                .map(|m| {
                    if in_starred {
                        m.starred_pinned
                    } else {
                        m.pinned
                    }
                })
                .unwrap_or(false);
            let raw_group = meta.and_then(|m| {
                if in_starred {
                    m.starred_group_id.as_deref()
                } else {
                    m.group_id.as_deref()
                }
            });
            let eff_group = raw_group
                .filter(|gid| valid_group_ids.contains(gid))
                .map(str::to_string);
            let eff_manual_order = meta.and_then(|m| {
                if in_starred {
                    m.starred_manual_order
                } else {
                    m.manual_order
                }
            });
            let muted = meta.map(|m| m.muted).unwrap_or(false);
            // Passive visits feed Recents/Frequent, not sidebar activity order.
            let used_ms = meta
                .and_then(|m| m.last_input_ms)
                .unwrap_or(0)
                .max(input.summary.updated_at_ms)
                .max(input.summary.last_assistant_ms.unwrap_or(0))
                .max(last_frequent_visit_ms);

            let ws_name = ws_name_map
                .get(eff_ws.as_str())
                .copied()
                .unwrap_or(DEFAULT_WORKSPACE_NAME)
                .to_string();

            let row = SidebarRow {
                session_key: input.key.clone(),
                session_id: input.summary.id.clone(),
                host: input.host.to_string(),
                agent: input.summary.agent,
                title: self
                    .effective_title(&input.key, &input.summary.title)
                    .to_string(),
                cwd: input.summary.cwd.clone(),
                workspace_id: eff_ws,
                workspace_name: ws_name,
                pinned: eff_pinned,
                favorite: self.favorites.contains(&input.key),
                unread,
                working: input.summary.working,
                muted,
                used_ms,
                group_id: eff_group.clone(),
            };

            let cand = Candidate {
                orig_pos,
                manual_order: eff_manual_order,
                frequent_count,
                row,
            };

            if in_all || workspace_view == WorkspaceView::Frequent {
                ungrouped_recents.push(cand);
            } else if eff_pinned {
                pinned_candidates.push(cand);
            } else if let Some(gid) = eff_group {
                grouped_buckets.entry(gid).or_default().push(cand);
            } else {
                ungrouped_recents.push(cand);
            }
        }

        // Order groups by (position, original index in self.groups)
        let mut scope_groups: Vec<(usize, &SessionGroup)> = self
            .groups
            .iter()
            .enumerate()
            .filter(|(_, g)| g.workspace_id == workspace_id)
            .collect();
        scope_groups.sort_by_key(|(idx, g)| (g.position, *idx));

        let mut groups = Vec::new();
        for (_, group) in scope_groups {
            if let Some(mut members) = grouped_buckets.remove(&group.id) {
                if self.auto_sort {
                    members.sort_by(|a, b| {
                        b.row
                            .used_ms
                            .cmp(&a.row.used_ms)
                            .then_with(|| a.orig_pos.cmp(&b.orig_pos))
                    });
                } else {
                    members.sort_by_key(|c| (c.manual_order.unwrap_or(usize::MAX), c.orig_pos));
                }
                let mut group = group.clone();
                group.name = match group.name.trim() {
                    "" => "Group".to_string(),
                    name => name.to_string(),
                };
                groups.push(SidebarGroupView {
                    group,
                    items: members.into_iter().map(|c| c.row).collect(),
                });
            }
        }

        // Pinned tiles preserve their manual order.
        pinned_candidates.sort_by_key(|c| (c.manual_order.unwrap_or(usize::MAX), c.orig_pos));
        let pinned_tiles: Vec<SidebarRow> = pinned_candidates.into_iter().map(|c| c.row).collect();

        // Frequent is ranked independently of workspace pins, groups and manual order.
        if workspace_view == WorkspaceView::Frequent {
            ungrouped_recents.sort_by(|a, b| {
                b.frequent_count
                    .cmp(&a.frequent_count)
                    .then_with(|| b.row.used_ms.cmp(&a.row.used_ms))
                    .then_with(|| a.orig_pos.cmp(&b.orig_pos))
            });
        } else if in_all {
            ungrouped_recents.sort_by(|a, b| {
                b.row
                    .used_ms
                    .cmp(&a.row.used_ms)
                    .then_with(|| a.orig_pos.cmp(&b.orig_pos))
            });
        } else if self.auto_sort {
            ungrouped_recents.sort_by(|a, b| {
                b.row
                    .used_ms
                    .cmp(&a.row.used_ms)
                    .then_with(|| a.orig_pos.cmp(&b.orig_pos))
            });
        } else {
            ungrouped_recents.sort_by_key(|c| (c.manual_order.unwrap_or(usize::MAX), c.orig_pos));
        }
        let recents: Vec<SidebarRow> = ungrouped_recents.into_iter().map(|c| c.row).collect();

        // Match the rendered order: Pinned -> named groups -> Recents.
        let mut flat_display_order = Vec::new();
        for item in &pinned_tiles {
            flat_display_order.push(item.session_key.clone());
        }
        for gv in &groups {
            for item in &gv.items {
                flat_display_order.push(item.session_key.clone());
            }
        }
        for item in &recents {
            flat_display_order.push(item.session_key.clone());
        }

        // Ctrl+digits follow visible rows; Ctrl+G can still hide a named group.
        let mut ctrl_digit_order = Vec::new();
        for item in &pinned_tiles {
            ctrl_digit_order.push(item.session_key.clone());
        }
        for gv in &groups {
            if !gv.group.collapsed {
                for item in &gv.items {
                    ctrl_digit_order.push(item.session_key.clone());
                }
            }
        }
        for item in &recents {
            ctrl_digit_order.push(item.session_key.clone());
        }

        SidebarLayout {
            pinned_tiles,
            groups,
            recents,
            flat_display_order,
            ctrl_digit_order,
        }
    }

    /// Materializes the current display order into manual order indices across ALL workspaces
    /// (including `STARRED_WORKSPACE_ID`) and disables `auto_sort`.
    pub fn materialize_display_order(
        &mut self,
        workspace_id: &str,
        sessions: &[SessionInput<'_>],
        now_ms: i64,
    ) {
        let mut workspace_ids: Vec<String> = self.workspaces.iter().map(|w| w.id.clone()).collect();
        if workspace_id != STARRED_WORKSPACE_ID
            && !workspace_ids.iter().any(|id| id == workspace_id)
        {
            workspace_ids.push(workspace_id.to_string());
        }

        let normal_orders: Vec<Vec<String>> = workspace_ids
            .iter()
            .map(|wid| {
                self.compute_sidebar_layout(wid, sessions, false, now_ms)
                    .flat_display_order
            })
            .collect();
        let starred_order = self
            .compute_sidebar_layout(STARRED_WORKSPACE_ID, sessions, false, now_ms)
            .flat_display_order;

        for order in normal_orders {
            for (idx, key) in order.into_iter().enumerate() {
                let meta = self.session_meta.entry(key).or_default();
                meta.manual_order = Some(idx);
            }
        }
        for (idx, key) in starred_order.into_iter().enumerate() {
            let meta = self.session_meta.entry(key).or_default();
            meta.starred_manual_order = Some(idx);
        }
        self.auto_sort = false;
    }

    /// `Ctrl+Shift+Up/Down`: reorders `session_key` within its strict partition
    /// (same group, Pinned, or Recents). Never crosses boundaries
    /// and never mutates `auto_sort` or `manual_order` on invalid/no-op moves.
    pub fn reorder_session(
        &mut self,
        workspace_id: &str,
        session_key: &str,
        direction: ReorderDirection,
        sessions: &[SessionInput<'_>],
        now_ms: i64,
    ) -> bool {
        let in_starred = workspace_id == STARRED_WORKSPACE_ID;
        let layout = self.compute_sidebar_layout(workspace_id, sessions, false, now_ms);

        let mut partition: Option<Vec<String>> = None;
        for gv in &layout.groups {
            if gv.items.iter().any(|r| r.session_key == session_key) {
                partition = Some(gv.items.iter().map(|r| r.session_key.clone()).collect());
                break;
            }
        }
        if partition.is_none()
            && layout
                .pinned_tiles
                .iter()
                .any(|r| r.session_key == session_key)
        {
            partition = Some(
                layout
                    .pinned_tiles
                    .iter()
                    .map(|r| r.session_key.clone())
                    .collect(),
            );
        }
        if partition.is_none() && layout.recents.iter().any(|r| r.session_key == session_key) {
            partition = Some(
                layout
                    .recents
                    .iter()
                    .map(|r| r.session_key.clone())
                    .collect(),
            );
        }

        let Some(mut keys) = partition else {
            return false;
        };
        let Some(idx) = keys.iter().position(|k| k == session_key) else {
            return false;
        };

        let swap_idx = match direction {
            ReorderDirection::Up => {
                if idx == 0 {
                    return false;
                }
                idx - 1
            }
            ReorderDirection::Down => {
                if idx + 1 >= keys.len() {
                    return false;
                }
                idx + 1
            }
        };

        if self.auto_sort {
            self.materialize_display_order(workspace_id, sessions, now_ms);
        }

        keys.swap(idx, swap_idx);
        for (order_idx, key) in keys.into_iter().enumerate() {
            let meta = self.session_meta.entry(key).or_default();
            if in_starred {
                meta.starred_manual_order = Some(order_idx);
            } else {
                meta.manual_order = Some(order_idx);
            }
        }
        true
    }

    // =========================================================================
    // Frequent Visits, Recent Views, Waiting/Undo, Read/Unread & Inbox Engine
    // =========================================================================

    pub fn visible_inbox_splits(&self, now_ms: i64) -> Vec<InboxSplit> {
        let mut splits = Vec::new();
        if self.inbox_settings.frequent_first {
            if self.inbox_settings.frequent {
                splits.push(InboxSplit::Frequent);
            }
            if self.inbox_settings.all {
                splits.push(InboxSplit::All);
            }
            if self.inbox_settings.favorites {
                splits.push(InboxSplit::Favorites);
            }
        } else {
            if self.inbox_settings.all {
                splits.push(InboxSplit::All);
            }
            if self.inbox_settings.frequent {
                splits.push(InboxSplit::Frequent);
            }
            if self.inbox_settings.favorites {
                splits.push(InboxSplit::Favorites);
            }
        }
        if splits.is_empty() {
            splits.push(InboxSplit::All);
        }
        for ws in self.visible_workspaces(now_ms) {
            splits.push(InboxSplit::Workspace(ws.id.clone()));
        }
        splits
    }

    pub fn lifecycle_inbox_splits(
        &self,
        lifecycle: SessionLifecycle,
        now_ms: i64,
    ) -> Vec<InboxSplit> {
        let mut splits = self.visible_inbox_splits(now_ms);
        if lifecycle == SessionLifecycle::Dissolved {
            let mut hidden: Vec<_> = self
                .workspaces
                .iter()
                .filter(|w| w.archived || w.is_snoozed_at(now_ms))
                .collect();
            hidden.sort_by_key(|w| w.position);
            splits.extend(
                hidden
                    .into_iter()
                    .map(|w| InboxSplit::Workspace(w.id.clone())),
            );
        }
        splits
    }

    /// Inbox split bar set: every split with `>= 1` session in the current Inbox query, plus
    /// `All`, which always stays visible so the Inbox keeps a live tab at `0` total sessions.
    ///
    /// This hides `0`-count splits (`Frequent 0`, `★ Favorites 0`, empty custom workspaces)
    /// from the Inbox split bar and from `Tab` / `Shift+Tab` cycling. `visible_inbox_splits`
    /// and `visible_workspaces` stay unfiltered so the Sidebar workspace switcher and
    /// Settings -> Workspaces keep listing empty workspaces for management.
    pub fn inbox_splits_with_sessions(
        &self,
        sessions: &[SessionInput<'_>],
        unread_only: bool,
        filter_query: &str,
        now_ms: i64,
    ) -> Vec<InboxSplit> {
        let splits: Vec<InboxSplit> = self
            .visible_inbox_splits(now_ms)
            .into_iter()
            .filter(|split| {
                *split == InboxSplit::All
                    || !self
                        .query_inbox(split, sessions, unread_only, filter_query, now_ms)
                        .is_empty()
            })
            .collect();
        if splits.is_empty() {
            vec![InboxSplit::All]
        } else {
            splits
        }
    }

    pub fn record_frequent_visit(&mut self, session_key: &str, visit_ms: i64) {
        let meta = self
            .session_meta
            .entry(session_key.to_string())
            .or_default();
        let cutoff = visit_ms.saturating_sub(SIDEBAR_FREQUENT_WINDOW_MS);
        meta.frequent_visits_ms.retain(|&t| t >= cutoff);
        if meta.frequent_visits_ms.contains(&visit_ms) {
            return;
        }
        if meta.frequent_visits_ms.len() >= FREQUENT_MAX_VISITS {
            meta.frequent_visits_ms.sort_unstable();
            if visit_ms <= meta.frequent_visits_ms[0] {
                return;
            }
            meta.frequent_visits_ms.remove(0);
        }
        meta.frequent_visits_ms.push(visit_ms);
        meta.frequent_visits_ms.sort_unstable();
        meta.last_used_ms = meta.last_used_ms.max(visit_ms);
    }

    pub fn record_session_input(&mut self, session_key: &str, now_ms: i64) {
        self.move_to_inbox(session_key);
        self.record_frequent_visit(session_key, now_ms);
        let meta = self
            .session_meta
            .entry(session_key.to_string())
            .or_default();
        meta.last_input_ms = Some(meta.last_input_ms.unwrap_or(i64::MIN).max(now_ms));
        meta.last_used_ms = meta.last_used_ms.max(now_ms);
    }

    pub fn record_session_view(&mut self, session_key: &str, now_ms: i64) {
        self.recent_views.retain(|e| e.session_key != session_key);
        self.recent_views.insert(
            0,
            RecentViewEntry {
                session_key: session_key.to_string(),
                viewed_at_ms: now_ms,
            },
        );
        if self.recent_views.len() > MAX_RECENT_VIEWS {
            self.recent_views.truncate(MAX_RECENT_VIEWS);
        }
        self.record_frequent_visit(session_key, now_ms);
    }

    pub fn is_session_unread(&self, session_key: &str, summary: &SessionSummary) -> bool {
        if let Some(boundary) = self.unread_boundaries.get(session_key) {
            let has_new_id = summary.last_assistant_id.is_some()
                && summary.last_assistant_id != boundary.last_read_assistant_id;
            let effective_ms = summary
                .last_assistant_ms
                .unwrap_or(summary.updated_at_ms)
                .max(summary.updated_at_ms);
            let has_new_ms = effective_ms > boundary.last_read_ms;
            let has_new = summary.needs_attention && (has_new_id || has_new_ms);
            if has_new {
                return true;
            }
            if let Some(manual) = boundary.manual_unread {
                return manual;
            }
            false
        } else {
            summary.needs_attention
        }
    }

    pub fn mark_session_read(&mut self, session_key: &str, summary: &SessionSummary) {
        let read_ms = summary
            .last_assistant_ms
            .unwrap_or(summary.updated_at_ms)
            .max(summary.updated_at_ms);
        self.unread_boundaries.insert(
            session_key.to_string(),
            UnreadBoundary {
                last_read_assistant_id: summary.last_assistant_id.clone(),
                last_read_ms: read_ms,
                manual_unread: Some(false),
            },
        );
    }

    pub fn mark_session_unread(&mut self, session_key: &str) {
        let entry = self
            .unread_boundaries
            .entry(session_key.to_string())
            .or_default();
        entry.manual_unread = Some(true);
        self.waiting_sessions.remove(session_key);
    }

    pub fn toggle_session_unread(&mut self, session_key: &str, summary: &SessionSummary) -> bool {
        if self.is_session_unread(session_key, summary) {
            self.mark_session_read(session_key, summary);
            false
        } else {
            self.mark_session_unread(session_key);
            true
        }
    }

    /// Legacy convenience helper for archiving `session_key` to Waiting without a full
    /// `SessionSummary`. UI callers should prefer [`Self::archive_to_waiting_session`] so the
    /// exact unread snapshot is recorded.
    pub fn archive_to_waiting(
        &mut self,
        session_key: &str,
        now_ms: i64,
        last_activity_ms: i64,
        last_assistant_id: Option<String>,
    ) {
        let was_known_read = self.unread_boundaries.get(session_key).is_some_and(|b| {
            b.manual_unread == Some(false)
                && b.last_read_ms >= last_activity_ms
                && (last_assistant_id.is_none() || b.last_read_assistant_id == last_assistant_id)
        });
        self.archive_to_waiting_inner(
            session_key,
            now_ms,
            last_activity_ms,
            last_assistant_id,
            !was_known_read,
        );
    }

    /// Archives `session_key` to Waiting using the exact unread state and activity timestamp
    /// from `summary`.
    pub fn archive_to_waiting_session(
        &mut self,
        session_key: &str,
        now_ms: i64,
        summary: &SessionSummary,
    ) {
        let last_activity_ms = summary
            .last_assistant_ms
            .unwrap_or(summary.updated_at_ms)
            .max(summary.updated_at_ms);
        let was_unread = self.is_session_unread(session_key, summary);
        self.archive_to_waiting_inner(
            session_key,
            now_ms,
            last_activity_ms,
            summary.last_assistant_id.clone(),
            was_unread,
        );
    }

    fn archive_to_waiting_inner(
        &mut self,
        session_key: &str,
        now_ms: i64,
        last_activity_ms: i64,
        last_assistant_id: Option<String>,
        was_unread: bool,
    ) {
        self.move_to_inbox(session_key);
        let entry = WaitingEntry {
            archived_at_ms: now_ms,
            last_activity_ms,
            last_assistant_id,
            was_unread,
        };
        self.waiting_sessions
            .insert(session_key.to_string(), entry.clone());
        self.waiting_undo_stack.push(WaitingUndoItem {
            session_key: session_key.to_string(),
            entry,
        });
        if self.waiting_undo_stack.len() > MAX_WAITING_UNDO {
            let excess = self.waiting_undo_stack.len() - MAX_WAITING_UNDO;
            self.waiting_undo_stack.drain(0..excess);
        }
    }

    pub fn undo_last_waiting(&mut self) -> Option<String> {
        while let Some(item) = self.waiting_undo_stack.pop() {
            if self.waiting_sessions.remove(&item.session_key).is_some() {
                return Some(item.session_key);
            }
        }
        None
    }

    pub fn auto_unarchive_waiting_if_active(
        &mut self,
        session_key: &str,
        summary: &SessionSummary,
    ) -> bool {
        if (summary.archived && summary.archive_kill_process)
            || self
                .session_meta
                .get(session_key)
                .is_some_and(|m| m.dissolved_at_ms.is_some())
        {
            return false;
        }
        let hibernating = self
            .session_meta
            .get(session_key)
            .is_some_and(|m| m.remind_at_ms.is_some());
        let entry = if hibernating {
            let meta = &self.session_meta[session_key];
            if !meta.hibernate_wake_on_reply {
                return false;
            }
            meta.hibernate_reply_boundary.as_ref()
        } else {
            self.waiting_sessions.get(session_key)
        };
        let Some(entry) = entry else {
            if hibernating {
                if let Some(meta) = self.session_meta.get_mut(session_key) {
                    meta.hibernate_reply_boundary = Some(WaitingEntry {
                        archived_at_ms: now_unix_ms(),
                        last_activity_ms: summary
                            .last_assistant_ms
                            .unwrap_or(summary.updated_at_ms),
                        last_assistant_id: summary.last_assistant_id.clone(),
                        was_unread: summary.needs_attention,
                    });
                }
            } else if summary.archived {
                self.archive_to_waiting_session(session_key, now_unix_ms(), summary);
            }
            return false;
        };
        let newer_assistant = summary.last_assistant_id.is_some()
            && summary.last_assistant_id != entry.last_assistant_id;
        let newer_reply = summary
            .last_assistant_ms
            .is_some_and(|at| at > entry.last_activity_ms);
        let became_unread = !entry.was_unread && self.is_session_unread(session_key, summary);
        if newer_reply || newer_assistant || became_unread {
            self.move_to_inbox(session_key);
            return true;
        }
        false
    }

    pub fn inbox_unread_count(&self, sessions: &[SessionInput<'_>]) -> usize {
        sessions
            .iter()
            .filter(|input| {
                if input.summary.archived
                    || self.inbox_excluded.contains(&input.key)
                    || self.waiting_sessions.contains_key(&input.key)
                {
                    return false;
                }
                let muted = self
                    .session_meta
                    .get(&input.key)
                    .map(|m| m.muted)
                    .unwrap_or(false);
                if muted {
                    return false;
                }
                let eff_ws_id = self.effective_workspace_id(&input.key);
                if let Some(ws) = self.workspace_by_id(&eff_ws_id)
                    && (ws.archived || ws.snoozed || ws.snoozed_until_ms.is_some())
                {
                    return false;
                }
                self.is_session_unread(&input.key, input.summary)
            })
            .count()
    }

    /// Time-aware unread badge count matching `query_inbox`'s snoozed/archived workspace filtering.
    pub fn inbox_unread_count_at(&self, sessions: &[SessionInput<'_>], now_ms: i64) -> usize {
        sessions
            .iter()
            .filter(|input| {
                let meta = self.session_meta.get(&input.key);
                let due = self.reminder_due(&input.key, now_ms);
                if self.session_lifecycle(&input.key, input.summary, now_ms)
                    != SessionLifecycle::Active
                {
                    return false;
                }
                if !due
                    && (input.summary.archived
                        || self.inbox_excluded.contains(&input.key)
                        || self.waiting_sessions.contains_key(&input.key)
                        || meta.is_some_and(|m| m.muted || m.remind_at_ms.is_some()))
                {
                    return false;
                }
                let workspace_id = self.effective_workspace_id(&input.key);
                if self
                    .workspace_by_id(&workspace_id)
                    .is_some_and(|ws| ws.archived || ws.is_snoozed_at(now_ms))
                {
                    return false;
                }
                due || self.is_session_unread(&input.key, input.summary)
            })
            .count()
    }

    pub fn query_inbox(
        &self,
        split: &InboxSplit,
        sessions: &[SessionInput<'_>],
        unread_only: bool,
        filter_query: &str,
        now_ms: i64,
    ) -> Vec<InboxRow> {
        self.query_lifecycle(
            SessionLifecycle::Active,
            split,
            sessions,
            unread_only,
            filter_query,
            now_ms,
        )
    }

    pub fn query_lifecycle(
        &self,
        lifecycle: SessionLifecycle,
        split: &InboxSplit,
        sessions: &[SessionInput<'_>],
        unread_only: bool,
        filter_query: &str,
        now_ms: i64,
    ) -> Vec<InboxRow> {
        let query_lower = filter_query.trim().to_lowercase();
        // Multi-word matching: every whitespace-separated token must appear somewhere in the
        // session's searchable text, so `multi rust` matches `Multi Bot Rust`.
        let query_tokens: Vec<&str> = query_lower.split_whitespace().collect();
        let mut ordered_ws: Vec<(usize, &Workspace)> = self.workspaces.iter().enumerate().collect();
        ordered_ws.sort_by_key(|(idx, w)| (w.position, *idx));
        let ws_lookup: HashMap<&str, (usize, &Workspace)> = ordered_ws
            .into_iter()
            .enumerate()
            .map(|(rank, (_, w))| (w.id.as_str(), (rank, w)))
            .collect();
        let recent_view_map: HashMap<&str, i64> = self
            .recent_views
            .iter()
            .map(|r| (r.session_key.as_str(), r.viewed_at_ms))
            .collect();

        let mut rows = Vec::new();

        for input in sessions {
            let meta = self.session_meta.get(&input.key);
            let due_reminder = self.reminder_due(&input.key, now_ms);
            if self.session_lifecycle(&input.key, input.summary, now_ms) != lifecycle {
                continue;
            }
            if lifecycle == SessionLifecycle::Active
                && !due_reminder
                && (self.inbox_excluded.contains(&input.key) || meta.is_some_and(|m| m.muted))
            {
                continue;
            }

            let eff_ws_id = self.effective_workspace_id(&input.key);
            let (ws_pos, ws_name) = match ws_lookup.get(eff_ws_id.as_str()) {
                Some((rank, ws)) => {
                    if lifecycle != SessionLifecycle::Dissolved
                        && (ws.archived || ws.is_snoozed_at(now_ms))
                    {
                        continue;
                    }
                    (*rank, ws.name.clone())
                }
                None => (0, DEFAULT_WORKSPACE_NAME.to_string()),
            };

            let visits = meta.map(|m| m.frequent_visits_ms.as_slice()).unwrap_or(&[]);
            let last_input = meta.and_then(|m| m.last_input_ms);

            let matches_split = match split {
                InboxSplit::Frequent => {
                    due_reminder || qualifies_for_frequent(visits, last_input, now_ms)
                }
                InboxSplit::All => true,
                InboxSplit::Favorites => due_reminder || self.favorites.contains(&input.key),
                InboxSplit::Workspace(wid) => eff_ws_id == *wid,
            };
            if !matches_split {
                continue;
            }

            let unread = self.is_session_unread(&input.key, input.summary);
            if unread_only && !unread && !due_reminder {
                continue;
            }

            let title = self
                .effective_title(&input.key, &input.summary.title)
                .to_string();
            let preview = input.summary.last_assistant_preview.clone();

            if !query_tokens.is_empty() {
                let haystack = format!(
                    "{}\n{}\n{}\n{}\n{}\n{}\n{}",
                    title.to_lowercase(),
                    input
                        .summary
                        .cwd
                        .as_deref()
                        .unwrap_or_default()
                        .to_lowercase(),
                    preview.as_deref().unwrap_or_default().to_lowercase(),
                    ws_name.to_lowercase(),
                    input.host.to_lowercase(),
                    input.summary.agent.label().to_lowercase(),
                    input.summary.id.to_lowercase(),
                );
                if !query_tokens.iter().all(|token| haystack.contains(token)) {
                    continue;
                }
            }

            let last_activity_ms = input
                .summary
                .last_assistant_ms
                .unwrap_or(0)
                .max(input.summary.updated_at_ms);
            let last_visit_ms = recent_view_map
                .get(input.key.as_str())
                .copied()
                .unwrap_or(0)
                .max(meta.map(|m| m.last_used_ms).unwrap_or(0))
                .max(visits.iter().copied().max().unwrap_or(0));
            let sort_ms = last_activity_ms.max(last_visit_ms);
            let frequent_key_ms = frequent_order_key(visits, last_input, now_ms);

            rows.push(InboxRow {
                due_reminder,
                reminder_note: meta.and_then(|m| m.reminder_note.clone()),
                session_key: input.key.clone(),
                session_id: input.summary.id.clone(),
                host: input.host.to_string(),
                agent: input.summary.agent,
                title,
                cwd: input.summary.cwd.clone(),
                preview,
                workspace_id: eff_ws_id,
                workspace_name: ws_name,
                workspace_position: ws_pos,
                unread,
                working: input.summary.working,
                favorite: self.favorites.contains(&input.key),
                last_activity_ms,
                last_visit_ms,
                sort_ms,
                frequent_key_ms,
            });
        }

        let is_frequent = *split == InboxSplit::Frequent;
        rows.sort_by(|a, b| {
            let primary = if is_frequent {
                b.frequent_key_ms.cmp(&a.frequent_key_ms)
            } else {
                b.sort_ms.cmp(&a.sort_ms)
            };
            b.due_reminder
                .cmp(&a.due_reminder)
                .then(primary)
                .then_with(|| b.unread.cmp(&a.unread))
                .then_with(|| b.working.cmp(&a.working))
                .then_with(|| b.last_activity_ms.cmp(&a.last_activity_ms))
                .then_with(|| a.workspace_position.cmp(&b.workspace_position))
                .then_with(|| a.session_key.cmp(&b.session_key))
        });

        rows
    }
}

// =============================================================================
// Ranked Ctrl+K Command Palette Engine & PaletteStats
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaletteCategory {
    Sessions,
    WorkspacesAndWindows,
    SidebarAndTabs,
    LookAndSettings,
    DebugAndRepair,
    Browser,
    AccountAndUsage,
    ChatView,
}

impl PaletteCategory {
    pub const ALL: [PaletteCategory; 8] = [
        PaletteCategory::Sessions,
        PaletteCategory::WorkspacesAndWindows,
        PaletteCategory::SidebarAndTabs,
        PaletteCategory::LookAndSettings,
        PaletteCategory::DebugAndRepair,
        PaletteCategory::Browser,
        PaletteCategory::AccountAndUsage,
        PaletteCategory::ChatView,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Sessions => "Sessions",
            Self::WorkspacesAndWindows => "Workspaces and windows",
            Self::SidebarAndTabs => "Sidebar and tabs",
            Self::LookAndSettings => "Look and settings",
            Self::DebugAndRepair => "Debug and repair",
            Self::Browser => "Browser",
            Self::AccountAndUsage => "Account and usage",
            Self::ChatView => "Chat View",
        }
    }

    pub fn from_label(label: &str) -> Self {
        match label.trim().to_ascii_lowercase().as_str() {
            "sessions" => Self::Sessions,
            "workspaces and windows" | "workspaces" => Self::WorkspacesAndWindows,
            "sidebar and tabs" | "sidebar" => Self::SidebarAndTabs,
            "look and settings" | "settings" | "updates" | "update" => Self::LookAndSettings,
            "debug and repair" | "debug" => Self::DebugAndRepair,
            "browser" => Self::Browser,
            "account and usage" | "account" => Self::AccountAndUsage,
            "chat view" | "chat" => Self::ChatView,
            _ => Self::LookAndSettings,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PaletteUsage {
    pub count: u32,
    pub last_used_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PaletteStats {
    #[serde(default)]
    pub usage: HashMap<String, PaletteUsage>,
}

impl PaletteStats {
    pub fn load_from(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read palette stats {}", path.display()))?;
        let parsed: Self = serde_json::from_str(&text)
            .with_context(|| format!("parse palette stats {}", path.display()))?;
        Ok(parsed)
    }

    pub fn load_or_default(path: &Path) -> Self {
        Self::load_from(path).unwrap_or_default()
    }

    pub fn load() -> Self {
        Self::load_or_default(&MultibotSettings::palette_stats_path())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        atomic_write_json(path, self)
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&MultibotSettings::palette_stats_path())
    }

    pub fn record_command_use(&mut self, id: &str, now_ms: i64) {
        let entry = self.usage.entry(id.to_string()).or_default();
        entry.count = entry.count.saturating_add(1);
        entry.last_used_ms = entry.last_used_ms.max(now_ms);
    }

    pub fn usage_for(&self, id: &str) -> PaletteUsage {
        self.usage.get(id).copied().unwrap_or_default()
    }
}

/// Unicode-safe 4-level text match rank:
/// `4` exact, `3` prefix, `2` word-boundary prefix, `1` substring, `0` no match.
pub fn palette_text_match_rank(text: &str, query: &str) -> u8 {
    let q_trimmed = query.trim();
    if q_trimmed.is_empty() {
        return 0;
    }
    let t_chars: Vec<char> = text.to_lowercase().chars().collect();
    let q_chars: Vec<char> = q_trimmed.to_lowercase().chars().collect();
    if q_chars.is_empty() || t_chars.len() < q_chars.len() {
        return 0;
    }
    if t_chars == q_chars {
        return 4;
    }
    if t_chars.starts_with(&q_chars) {
        return 3;
    }
    for i in 0..=(t_chars.len() - q_chars.len()) {
        if !t_chars[i].is_whitespace()
            && (i == 0 || !t_chars[i - 1].is_alphanumeric())
            && t_chars[i..].starts_with(&q_chars)
        {
            return 2;
        }
    }
    if t_chars
        .windows(q_chars.len())
        .any(|w| w == q_chars.as_slice())
    {
        return 1;
    }
    0
}

/// Recency-banded frecency score:
/// `<1h` = 96, `<24h` = 80, `<7d` = 64, `<30d` = 48, `<90d` = 32, older = 0,
/// plus `min(15, floor(log2(count)))`. Step 16 > 15 guarantees lifetime count never beats recency band.
pub fn palette_frecency_at(usage: PaletteUsage, now_ms: i64) -> u16 {
    if usage.count == 0 || usage.last_used_ms <= 0 {
        return 0;
    }
    const HOUR_MS: u64 = 3_600_000;
    const DAY_MS: u64 = 24 * HOUR_MS;
    let age_ms = now_ms.saturating_sub(usage.last_used_ms).max(0) as u64;
    let recency: u16 = if age_ms < HOUR_MS {
        96
    } else if age_ms < DAY_MS {
        80
    } else if age_ms < 7 * DAY_MS {
        64
    } else if age_ms < 30 * DAY_MS {
        48
    } else if age_ms < 90 * DAY_MS {
        32
    } else {
        0
    };
    let freq = (usage.count.ilog2() as u16).min(15);
    recency + freq
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteCommandSpec {
    pub id: String,
    pub label: String,
    pub chord: String,
    pub category: PaletteCategory,
    pub destructive: bool,
    pub aliases: Vec<String>,
    pub tab_context: bool,
    pub global: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PaletteMode {
    #[default]
    Global,
    Workspaces,
    TabContext {
        target_session_key: String,
    },
}

impl PaletteMode {
    pub fn target_session_key(&self) -> Option<&str> {
        match self {
            Self::TabContext { target_session_key } => Some(target_session_key),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteRow {
    Header(String),
    Session {
        session_key: String,
        title: String,
        agent: AgentKind,
        cwd: Option<String>,
        workspace_name: String,
        match_rank: u8,
        frecency: u16,
    },
    Command {
        id: String,
        label: String,
        chord: String,
        category: PaletteCategory,
        destructive: bool,
        match_rank: u8,
        frecency: u16,
    },
    Workspace {
        workspace_id: String,
        view: WorkspaceView,
        name: String,
        archived: bool,
        match_rank: u8,
    },
}

fn is_del_session_query(q_lower: &str) -> bool {
    q_lower.chars().count() >= 3 && "delete session".starts_with(q_lower)
}

fn is_delete_session_command(cmd: &PaletteCommandSpec) -> bool {
    cmd.id == "session.delete"
        || cmd.id == "pal-cmd-delete-session"
        || cmd.label.eq_ignore_ascii_case("Delete session")
}

fn cwd_leaf(cwd: Option<&str>) -> &str {
    let Some(path) = cwd else {
        return "";
    };
    let trimmed = path.trim_end_matches(['/', '\\']);
    trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed)
}

#[allow(clippy::too_many_arguments)]
pub fn rank_palette(
    store: &WorkspaceStore,
    stats: &PaletteStats,
    sessions: &[SessionInput<'_>],
    commands: &[PaletteCommandSpec],
    query: &str,
    mode: &PaletteMode,
    current_session_key: Option<&str>,
    previous_session_key: Option<&str>,
    now_ms: i64,
) -> Vec<PaletteRow> {
    let q_trimmed = query.trim();
    let q_lower = q_trimmed.to_lowercase();
    let del_query = is_del_session_query(&q_lower);

    match mode {
        PaletteMode::Workspaces => {
            let choices = [
                (ALL_WORKSPACES_ID, "All", WorkspaceView::All),
                (ALL_WORKSPACES_ID, "Frequent", WorkspaceView::Frequent),
                (ALL_WORKSPACES_ID, "Favorites", WorkspaceView::Favorites),
            ];
            let mut rows: Vec<_> = choices
                .into_iter()
                .chain(
                    store
                        .visible_workspaces(now_ms)
                        .into_iter()
                        .map(|ws| (ws.id.as_str(), ws.name.as_str(), WorkspaceView::All)),
                )
                .filter_map(|(id, name, view)| {
                    let match_rank = palette_text_match_rank(name, q_trimmed);
                    (q_trimmed.is_empty() || match_rank > 0).then(|| PaletteRow::Workspace {
                        workspace_id: id.to_string(),
                        view,
                        name: name.to_string(),
                        archived: false,
                        match_rank,
                    })
                })
                .collect();
            if !q_trimmed.is_empty() {
                rows.sort_by_key(|row| match row {
                    PaletteRow::Workspace { match_rank, .. } => std::cmp::Reverse(*match_rank),
                    _ => unreachable!(),
                });
            }
            rows
        }
        PaletteMode::TabContext { .. } => {
            let mut entries: Vec<(&PaletteCommandSpec, u8, u16, PaletteUsage, bool)> = commands
                .iter()
                .filter(|c| c.tab_context)
                .filter_map(|c| {
                    let usage = stats.usage_for(&c.id);
                    let frecency = palette_frecency_at(usage, now_ms);
                    let boost = del_query && is_delete_session_command(c);
                    if q_trimmed.is_empty() {
                        Some((c, 0, frecency, usage, boost))
                    } else {
                        let mut rank = palette_text_match_rank(&c.label, q_trimmed);
                        for alias in &c.aliases {
                            rank = rank.max(palette_text_match_rank(alias, q_trimmed));
                        }
                        if rank > 0 {
                            Some((c, rank, frecency, usage, boost))
                        } else {
                            None
                        }
                    }
                })
                .collect();

            if q_trimmed.is_empty() {
                entries.sort_by(|a, b| {
                    a.0.destructive
                        .cmp(&b.0.destructive)
                        .then_with(|| b.3.count.cmp(&a.3.count))
                        .then_with(|| b.3.last_used_ms.cmp(&a.3.last_used_ms))
                        .then_with(|| a.0.label.to_lowercase().cmp(&b.0.label.to_lowercase()))
                });
            } else {
                entries.sort_by(|a, b| {
                    b.1.cmp(&a.1)
                        .then_with(|| b.4.cmp(&a.4))
                        .then_with(|| b.2.cmp(&a.2))
                        .then_with(|| b.3.last_used_ms.cmp(&a.3.last_used_ms))
                        .then_with(|| a.0.destructive.cmp(&b.0.destructive))
                        .then_with(|| a.0.label.to_lowercase().cmp(&b.0.label.to_lowercase()))
                });
            }

            entries
                .into_iter()
                .map(|(c, match_rank, frecency, _, _)| PaletteRow::Command {
                    id: c.id.clone(),
                    label: c.label.clone(),
                    chord: c.chord.clone(),
                    category: c.category,
                    destructive: c.destructive,
                    match_rank,
                    frecency,
                })
                .collect()
        }
        PaletteMode::Global if q_trimmed.is_empty() => {
            let mut out = Vec::new();
            let ws_map: HashMap<&str, &Workspace> = store
                .workspaces
                .iter()
                .map(|w| (w.id.as_str(), w))
                .collect();
            let session_map: HashMap<&str, &SessionInput<'_>> =
                sessions.iter().map(|s| (s.key.as_str(), s)).collect();

            let is_session_eligible = |key: &str| -> Option<(&SessionInput<'_>, String)> {
                if current_session_key == Some(key) {
                    return None;
                }
                if store.waiting_sessions.contains_key(key) {
                    return None;
                }
                let input = *session_map.get(key)?;
                if input.summary.archived {
                    return None;
                }
                let ws_id = store.effective_workspace_id(key);
                let ws_name = match ws_map.get(ws_id.as_str()) {
                    Some(ws) => {
                        if ws.archived || ws.is_snoozed_at(now_ms) {
                            return None;
                        }
                        ws.name.clone()
                    }
                    None => DEFAULT_WORKSPACE_NAME.to_string(),
                };
                Some((input, ws_name))
            };

            // 1. Recent sessions (max 5, leading with previous_session_key if eligible)
            let mut recent_keys: Vec<String> = Vec::new();
            if let Some(prev) = previous_session_key
                && is_session_eligible(prev).is_some()
            {
                recent_keys.push(prev.to_string());
            }

            let mut ordered_views = store.recent_views.clone();
            ordered_views.sort_by(|a, b| b.viewed_at_ms.cmp(&a.viewed_at_ms));
            for rv in ordered_views {
                if recent_keys.len() >= MAX_PALETTE_RECENT_SESSIONS {
                    break;
                }
                if !recent_keys.contains(&rv.session_key)
                    && is_session_eligible(&rv.session_key).is_some()
                {
                    recent_keys.push(rv.session_key);
                }
            }

            if !recent_keys.is_empty() {
                out.push(PaletteRow::Header("Recent sessions".to_string()));
                for key in recent_keys {
                    if let Some((input, ws_name)) = is_session_eligible(&key) {
                        out.push(PaletteRow::Session {
                            session_key: key.clone(),
                            title: store
                                .effective_title(&key, &input.summary.title)
                                .to_string(),
                            agent: input.summary.agent,
                            cwd: input.summary.cwd.clone(),
                            workspace_name: ws_name,
                            match_rank: 0,
                            frecency: 0,
                        });
                    }
                }
            }

            // 2. Top-10 Most used global commands
            let global_cmds: Vec<(usize, &PaletteCommandSpec, PaletteUsage, u16)> = commands
                .iter()
                .enumerate()
                .filter(|(_, c)| c.global)
                .map(|(idx, c)| {
                    let u = stats.usage_for(&c.id);
                    let f = palette_frecency_at(u, now_ms);
                    (idx, c, u, f)
                })
                .collect();

            let mut used_cmds: Vec<(usize, &PaletteCommandSpec, PaletteUsage, u16)> = global_cmds
                .iter()
                .copied()
                .filter(|(_, _, u, _)| u.count > 0)
                .collect();
            used_cmds.sort_by(|a, b| {
                b.2.count
                    .cmp(&a.2.count)
                    .then_with(|| b.2.last_used_ms.cmp(&a.2.last_used_ms))
                    .then_with(|| a.1.label.to_lowercase().cmp(&b.1.label.to_lowercase()))
            });
            used_cmds.truncate(PALETTE_MOST_USED_LIMIT);

            let mut placed_ids: HashSet<&str> = HashSet::new();
            if !used_cmds.is_empty() {
                // Safe commands before destructive inside Most used
                used_cmds.sort_by(|a, b| {
                    a.1.destructive
                        .cmp(&b.1.destructive)
                        .then_with(|| b.2.count.cmp(&a.2.count))
                        .then_with(|| b.2.last_used_ms.cmp(&a.2.last_used_ms))
                        .then_with(|| a.1.label.to_lowercase().cmp(&b.1.label.to_lowercase()))
                });
                out.push(PaletteRow::Header("Most used".to_string()));
                for (_, c, _, frecency) in used_cmds {
                    placed_ids.insert(c.id.as_str());
                    out.push(PaletteRow::Command {
                        id: c.id.clone(),
                        label: c.label.clone(),
                        chord: c.chord.clone(),
                        category: c.category,
                        destructive: c.destructive,
                        match_rank: 0,
                        frecency,
                    });
                }
            }

            // 3. 8 Categories (deduplicated from Most used, safe before destructive)
            for cat in PaletteCategory::ALL {
                let mut cat_cmds: Vec<(usize, &PaletteCommandSpec, PaletteUsage, u16)> =
                    global_cmds
                        .iter()
                        .copied()
                        .filter(|(_, c, _, _)| {
                            c.category == cat && !placed_ids.contains(c.id.as_str())
                        })
                        .collect();
                if cat_cmds.is_empty() {
                    continue;
                }
                cat_cmds.sort_by(|a, b| {
                    a.1.destructive
                        .cmp(&b.1.destructive)
                        .then_with(|| b.2.count.cmp(&a.2.count))
                        .then_with(|| b.2.last_used_ms.cmp(&a.2.last_used_ms))
                        .then_with(|| a.0.cmp(&b.0))
                });
                out.push(PaletteRow::Header(cat.label().to_string()));
                for (_, c, _, frecency) in cat_cmds {
                    out.push(PaletteRow::Command {
                        id: c.id.clone(),
                        label: c.label.clone(),
                        chord: c.chord.clone(),
                        category: c.category,
                        destructive: c.destructive,
                        match_rank: 0,
                        frecency,
                    });
                }
            }

            out
        }
        PaletteMode::Global => {
            // Typed query: flat ranked list (no section headers)
            struct ScoredRow {
                kind_tier: u8,
                match_rank: u8,
                del_boost: bool,
                is_session: bool,
                frecency: u16,
                last_used_ms: i64,
                destructive: bool,
                label_lower: String,
                row: PaletteRow,
            }

            let ws_map: HashMap<&str, &Workspace> = store
                .workspaces
                .iter()
                .map(|w| (w.id.as_str(), w))
                .collect();
            let view_map: HashMap<&str, i64> = store
                .recent_views
                .iter()
                .map(|rv| (rv.session_key.as_str(), rv.viewed_at_ms))
                .collect();

            let mut scored: Vec<ScoredRow> = Vec::new();

            // Sessions (kind_tier = 0)
            for input in sessions {
                if input.summary.archived || store.waiting_sessions.contains_key(&input.key) {
                    continue;
                }
                let ws_id = store.effective_workspace_id(&input.key);
                let ws_name = match ws_map.get(ws_id.as_str()) {
                    Some(ws) => {
                        if ws.archived || ws.is_snoozed_at(now_ms) {
                            continue;
                        }
                        ws.name.clone()
                    }
                    None => DEFAULT_WORKSPACE_NAME.to_string(),
                };
                let title = store
                    .effective_title(&input.key, &input.summary.title)
                    .to_string();
                let leaf = cwd_leaf(input.summary.cwd.as_deref());
                let rank = palette_text_match_rank(&title, q_trimmed)
                    .max(palette_text_match_rank(leaf, q_trimmed))
                    .max(palette_text_match_rank(
                        input.summary.agent.label(),
                        q_trimmed,
                    ));
                if rank == 0 {
                    continue;
                }
                let last_used_ms = view_map
                    .get(input.key.as_str())
                    .copied()
                    .unwrap_or(0)
                    .max(
                        store
                            .session_meta
                            .get(&input.key)
                            .map(|m| m.last_used_ms)
                            .unwrap_or(0),
                    )
                    .max(input.summary.updated_at_ms);
                let frecency = palette_frecency_at(
                    PaletteUsage {
                        count: 1,
                        last_used_ms,
                    },
                    now_ms,
                );
                let label_lower = title.to_lowercase();
                scored.push(ScoredRow {
                    kind_tier: 0,
                    match_rank: rank,
                    del_boost: false,
                    is_session: true,
                    frecency,
                    last_used_ms,
                    destructive: false,
                    label_lower,
                    row: PaletteRow::Session {
                        session_key: input.key.clone(),
                        title,
                        agent: input.summary.agent,
                        cwd: input.summary.cwd.clone(),
                        workspace_name: ws_name,
                        match_rank: rank,
                        frecency,
                    },
                });
            }

            // Global commands (kind_tier = 0)
            for cmd in commands.iter().filter(|c| c.global) {
                let mut rank = palette_text_match_rank(&cmd.label, q_trimmed);
                for alias in &cmd.aliases {
                    rank = rank.max(palette_text_match_rank(alias, q_trimmed));
                }
                if rank == 0 {
                    continue;
                }
                let usage = stats.usage_for(&cmd.id);
                let frecency = palette_frecency_at(usage, now_ms);
                let del_boost = del_query && is_delete_session_command(cmd);
                scored.push(ScoredRow {
                    kind_tier: 0,
                    match_rank: rank,
                    del_boost,
                    is_session: false,
                    frecency,
                    last_used_ms: usage.last_used_ms,
                    destructive: cmd.destructive,
                    label_lower: cmd.label.to_lowercase(),
                    row: PaletteRow::Command {
                        id: cmd.id.clone(),
                        label: cmd.label.clone(),
                        chord: cmd.chord.clone(),
                        category: cmd.category,
                        destructive: cmd.destructive,
                        match_rank: rank,
                        frecency,
                    },
                });
            }

            // Workspaces (live = kind_tier 1, archived = kind_tier 2)
            for ws in &store.workspaces {
                let rank = palette_text_match_rank(&ws.name, q_trimmed);
                if rank == 0 {
                    continue;
                }
                let kind_tier = if ws.archived { 2 } else { 1 };
                scored.push(ScoredRow {
                    kind_tier,
                    match_rank: rank,
                    del_boost: false,
                    is_session: false,
                    frecency: 0,
                    last_used_ms: 0,
                    destructive: false,
                    label_lower: ws.name.to_lowercase(),
                    row: PaletteRow::Workspace {
                        workspace_id: ws.id.clone(),
                        view: WorkspaceView::All,
                        name: ws.name.clone(),
                        archived: ws.archived,
                        match_rank: rank,
                    },
                });
            }

            scored.sort_by(|a, b| {
                a.kind_tier
                    .cmp(&b.kind_tier)
                    .then_with(|| b.match_rank.cmp(&a.match_rank))
                    .then_with(|| b.del_boost.cmp(&a.del_boost))
                    .then_with(|| b.is_session.cmp(&a.is_session))
                    .then_with(|| b.frecency.cmp(&a.frecency))
                    .then_with(|| b.last_used_ms.cmp(&a.last_used_ms))
                    .then_with(|| a.destructive.cmp(&b.destructive))
                    .then_with(|| a.label_lower.cmp(&b.label_lower))
            });

            scored.into_iter().map(|s| s.row).collect()
        }
    }
}

#[cfg(test)]
#[path = "workspaces_tests.rs"]
mod tests;
