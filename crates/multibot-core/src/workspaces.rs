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
pub const FREQUENT_MAX_VISITS: usize = 8;

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
    std::fs::rename(tmp_path, target_path).with_context(|| {
        format!(
            "rename {} -> {}",
            tmp_path.display(),
            target_path.display()
        )
    })
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
    std::fs::rename(tmp_path, target_path).with_context(|| {
        format!(
            "rename {} -> {}",
            tmp_path.display(),
            target_path.display()
        )
    })
}

// =============================================================================
// Canonical Session Keys & F2 Rename Helpers
// =============================================================================

pub fn agent_slug(agent: AgentKind) -> &'static str {
    match agent {
        AgentKind::Claude => "claude",
        AgentKind::Antigravity => "antigravity",
        AgentKind::Codex => "codex",
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

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SessionMeta {
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub starred_pinned: bool,
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
        parsed.ensure_default_workspace();
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
                m.workspace_id.is_some() || m.pinned || m.starred_pinned || m.muted
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
                        meta.pinned |= imported.pinned;
                        meta.starred_pinned |= imported.starred_pinned;
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
        let current_idx = ring
            .iter()
            .position(|id| id == current_id)
            .unwrap_or(0) as isize;
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
            let target_id = move_sessions_to
                .ok_or(WorkspaceError::NonEmptyRequiresTarget { session_count })?;
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
        self.session_meta
            .get(session_key)
            .is_some_and(|m| if in_starred { m.starred_pinned } else { m.pinned })
    }

    pub fn toggle_pin(&mut self, session_key: &str, in_starred: bool) -> bool {
        let meta = self
            .session_meta
            .entry(session_key.to_string())
            .or_default();
        if in_starred {
            meta.starred_pinned = !meta.starred_pinned;
            meta.starred_pinned
        } else {
            meta.pinned = !meta.pinned;
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
            row: SidebarRow,
        }

        let mut grouped_buckets: HashMap<String, Vec<Candidate>> = HashMap::new();
        let mut ungrouped_pinned: Vec<Candidate> = Vec::new();
        let mut ungrouped_recents: Vec<Candidate> = Vec::new();

        for (orig_pos, input) in sessions.iter().enumerate() {
            if input.summary.archived || self.waiting_sessions.contains_key(&input.key) {
                continue;
            }
            let meta = self.session_meta.get(&input.key);
            let eff_ws = self.effective_workspace_id(&input.key);

            if in_starred {
                if !self.favorites.contains(&input.key) {
                    continue;
                }
            } else if !in_all && eff_ws != workspace_id {
                continue;
            }

            match workspace_view {
                WorkspaceView::All => {}
                WorkspaceView::Frequent => {
                    let visits = meta
                        .map(|m| m.frequent_visits_ms.as_slice())
                        .unwrap_or(&[]);
                    let last_input = meta.and_then(|m| m.last_input_ms);
                    if !qualifies_for_frequent(visits, last_input, now_ms) {
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
                .map(|m| if in_starred { m.starred_pinned } else { m.pinned })
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
            let used_ms = meta
                .map(|m| m.last_used_ms)
                .unwrap_or(0)
                .max(input.summary.updated_at_ms);

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
                row,
            };

            if in_all {
                ungrouped_recents.push(cand);
            } else if let Some(gid) = eff_group {
                grouped_buckets.entry(gid).or_default().push(cand);
            } else if eff_pinned {
                ungrouped_pinned.push(cand);
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
                            .pinned
                            .cmp(&a.row.pinned)
                            .then_with(|| b.row.used_ms.cmp(&a.row.used_ms))
                            .then_with(|| a.orig_pos.cmp(&b.orig_pos))
                    });
                } else {
                    members.sort_by_key(|c| (c.manual_order.unwrap_or(usize::MAX), c.orig_pos));
                }
                groups.push(SidebarGroupView {
                    group: group.clone(),
                    items: members.into_iter().map(|c| c.row).collect(),
                });
            }
        }

        // Ungrouped pinned: manual order always preserved
        ungrouped_pinned.sort_by_key(|c| (c.manual_order.unwrap_or(usize::MAX), c.orig_pos));
        let pinned_tiles: Vec<SidebarRow> = ungrouped_pinned.into_iter().map(|c| c.row).collect();

        // Ungrouped recents: auto-sort (!muted -> used_ms desc -> pos asc) or manual order
        if in_all {
            ungrouped_recents.sort_by(|a, b| {
                b.row
                    .used_ms
                    .cmp(&a.row.used_ms)
                    .then_with(|| a.orig_pos.cmp(&b.orig_pos))
            });
        } else if self.auto_sort {
            ungrouped_recents.sort_by(|a, b| {
                a.row
                    .muted
                    .cmp(&b.row.muted)
                    .then_with(|| b.row.used_ms.cmp(&a.row.used_ms))
                    .then_with(|| a.orig_pos.cmp(&b.orig_pos))
            });
        } else {
            ungrouped_recents.sort_by_key(|c| (c.manual_order.unwrap_or(usize::MAX), c.orig_pos));
        }
        let recents: Vec<SidebarRow> = ungrouped_recents.into_iter().map(|c| c.row).collect();

        // flat_display_order: groups first -> ungrouped pinned -> ungrouped recents
        let mut flat_display_order = Vec::new();
        for gv in &groups {
            for item in &gv.items {
                flat_display_order.push(item.session_key.clone());
            }
        }
        for item in &pinned_tiles {
            flat_display_order.push(item.session_key.clone());
        }
        for item in &recents {
            flat_display_order.push(item.session_key.clone());
        }

        // ctrl_digit_order: pinned tiles FIRST -> non-collapsed group members -> recents
        // (skipping collapsed Pinned/Recents sections and collapsed groups).
        let mut ctrl_digit_order = Vec::new();
        if !self.pinned_section_collapsed {
            for item in &pinned_tiles {
                ctrl_digit_order.push(item.session_key.clone());
            }
        }
        for gv in &groups {
            if !gv.group.collapsed {
                for item in &gv.items {
                    ctrl_digit_order.push(item.session_key.clone());
                }
            }
        }
        if in_all || !self.recents_section_collapsed {
            for item in &recents {
                ctrl_digit_order.push(item.session_key.clone());
            }
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
    /// (same group, ungrouped pinned, or ungrouped recents). Never crosses boundaries
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
        let meta = self
            .session_meta
            .entry(session_key.to_string())
            .or_default();
        meta.last_used_ms = meta.last_used_ms.max(now_ms);
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
        let Some(entry) = self.waiting_sessions.get(session_key) else {
            return false;
        };
        let effective_activity_ms = summary
            .last_assistant_ms
            .unwrap_or(summary.updated_at_ms)
            .max(summary.updated_at_ms);
        let newer_activity = effective_activity_ms > entry.last_activity_ms;
        let newer_assistant = summary.last_assistant_id.is_some()
            && summary.last_assistant_id != entry.last_assistant_id;
        let is_unread = self.is_session_unread(session_key, summary);
        let became_unread = !entry.was_unread && is_unread;
        if newer_activity || newer_assistant || became_unread {
            self.waiting_sessions.remove(session_key);
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
                    && (ws.archived || ws.is_snoozed_at(now_ms))
                {
                    return false;
                }
                self.is_session_unread(&input.key, input.summary)
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
        let query_lower = filter_query.trim().to_lowercase();
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
            if input.summary.archived
                || self.inbox_excluded.contains(&input.key)
                || self.waiting_sessions.contains_key(&input.key)
            {
                continue;
            }
            let meta = self.session_meta.get(&input.key);
            if meta.is_some_and(|m| m.muted) {
                continue;
            }

            let eff_ws_id = self.effective_workspace_id(&input.key);
            let (ws_pos, ws_name) = match ws_lookup.get(eff_ws_id.as_str()) {
                Some((rank, ws)) => {
                    if ws.archived || ws.is_snoozed_at(now_ms) {
                        continue;
                    }
                    (*rank, ws.name.clone())
                }
                None => (0, DEFAULT_WORKSPACE_NAME.to_string()),
            };

            let visits = meta
                .map(|m| m.frequent_visits_ms.as_slice())
                .unwrap_or(&[]);
            let last_input = meta.and_then(|m| m.last_input_ms);

            let matches_split = match split {
                InboxSplit::Frequent => qualifies_for_frequent(visits, last_input, now_ms),
                InboxSplit::All => true,
                InboxSplit::Favorites => self.favorites.contains(&input.key),
                InboxSplit::Workspace(wid) => eff_ws_id == *wid,
            };
            if !matches_split {
                continue;
            }

            let unread = self.is_session_unread(&input.key, input.summary);
            if unread_only && !unread {
                continue;
            }

            let title = self
                .effective_title(&input.key, &input.summary.title)
                .to_string();
            let preview = input.summary.last_assistant_preview.clone();

            if !query_lower.is_empty() {
                let matches_query = title.to_lowercase().contains(&query_lower)
                    || input
                        .summary
                        .cwd
                        .as_deref()
                        .is_some_and(|c| c.to_lowercase().contains(&query_lower))
                    || preview
                        .as_deref()
                        .is_some_and(|p| p.to_lowercase().contains(&query_lower))
                    || ws_name.to_lowercase().contains(&query_lower)
                    || input
                        .summary
                        .agent
                        .label()
                        .to_lowercase()
                        .contains(&query_lower);
                if !matches_query {
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
            primary
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
    if t_chars.windows(q_chars.len()).any(|w| w == q_chars.as_slice()) {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteMode {
    Global,
    TabContext { target_session_key: String },
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
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(trimmed)
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
                let mut cat_cmds: Vec<(usize, &PaletteCommandSpec, PaletteUsage, u16)> = global_cmds
                    .iter()
                    .copied()
                    .filter(|(_, c, _, _)| c.category == cat && !placed_ids.contains(c.id.as_str()))
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
                    .max(palette_text_match_rank(input.summary.agent.label(), q_trimmed));
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
mod tests {
    use super::*;

    static TEST_DIR_SEQ: AtomicU64 = AtomicU64::new(1);

    fn unique_temp_dir(label: &str) -> PathBuf {
        let seq = TEST_DIR_SEQ.fetch_add(1, AtomicOrdering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "multibot-ws-{label}-{}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_session(
        id: &str,
        agent: AgentKind,
        title: &str,
        updated_at_ms: i64,
        needs_attention: bool,
        working: bool,
    ) -> SessionSummary {
        SessionSummary {
            id: id.to_string(),
            agent,
            title: title.to_string(),
            cwd: Some(format!("/repo/{id}")),
            source_path: format!("/tmp/{id}.jsonl"),
            updated_at_ms,
            working,
            last_assistant_id: if needs_attention {
                Some(format!("asst-{id}-{updated_at_ms}"))
            } else {
                None
            },
            last_assistant_preview: Some(format!("Preview for {title}")),
            last_assistant_ms: Some(updated_at_ms),
            needs_attention,
            ..Default::default()
        }
    }

    fn make_input<'a>(host: &'a str, summary: &'a SessionSummary) -> SessionInput<'a> {
        SessionInput {
            key: canonical_session_key(host, summary.agent, &summary.id),
            host,
            summary,
        }
    }

    #[test]
    fn all_workspaces_is_flat_recent_order_including_pinned_grouped_and_muted() {
        let mut store = WorkspaceStore::new();
        let work = store.create_workspace("Work", None).unwrap();
        let first = make_session("first", AgentKind::Claude, "First", 10, false, false);
        let second = make_session("second", AgentKind::Codex, "Second", 20, false, false);
        let archived = SessionSummary {
            archived: true,
            ..make_session("archived", AgentKind::Claude, "Archived", 30, false, false)
        };
        let inputs = vec![
            make_input("host", &first),
            make_input("host", &second),
            make_input("host", &archived),
        ];
        store.assign_session_workspace(&inputs[0].key, &work.id);
        store.toggle_pin(&inputs[0].key, false);
        store.toggle_favorite(&inputs[0].key);
        store.toggle_group_for_session(&inputs[1].key, DEFAULT_WORKSPACE_ID, None, "Group");
        store.session_meta.get_mut(&inputs[1].key).unwrap().muted = true;
        store.recents_section_collapsed = true;
        store.auto_sort = false;
        let all = store.compute_sidebar_layout(ALL_WORKSPACES_ID, &inputs, false, 100);
        assert!(all.pinned_tiles.is_empty());
        assert!(all.groups.is_empty());
        assert_eq!(
            all.recents
                .iter()
                .map(|r| r.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["second", "first"]
        );
        assert_eq!(
            all.ctrl_digit_order,
            vec![inputs[1].key.clone(), inputs[0].key.clone()]
        );
        let favorites = store.compute_sidebar_layout_with_options(
            ALL_WORKSPACES_ID,
            WorkspaceView::Favorites,
            &inputs,
            false,
            None,
            100,
        );
        assert_eq!(
            favorites
                .recents
                .iter()
                .map(|r| r.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["first"]
        );
        assert_eq!(
            store.cycle_workspace_id(ALL_WORKSPACES_ID, 1, 100),
            STARRED_WORKSPACE_ID
        );
        assert_eq!(
            store.cycle_workspace_id(STARRED_WORKSPACE_ID, -1, 100),
            ALL_WORKSPACES_ID
        );
    }

    #[test]
    fn atomic_persistence_roundtrip_and_no_stray_tmp() {
        let dir = unique_temp_dir("atomic");
        let ws_path = dir.join("workspaces.json");
        let stats_path = dir.join("palette-stats.json");

        let mut store = WorkspaceStore::new();
        let work = store
            .create_workspace("Work", Some("#60A5FA".into()))
            .unwrap();
        let key = canonical_session_key("vps1", AgentKind::Claude, "s1");
        store.assign_session_workspace(&key, &work.id);
        store.toggle_favorite(&key);
        store.apply_session_rename(&key, "Custom Session");
        store.save_to(&ws_path).unwrap();
        // Overwrite existing file atomically
        store.save_to(&ws_path).unwrap();

        let loaded = WorkspaceStore::load_from(&ws_path).unwrap();
        assert_eq!(loaded, store);

        let mut stats = PaletteStats::default();
        stats.record_command_use("pal-cmd-inbox", 1_000_000);
        stats.record_command_use("pal-cmd-inbox", 2_000_000);
        stats.save_to(&stats_path).unwrap();
        let loaded_stats = PaletteStats::load_from(&stats_path).unwrap();
        assert_eq!(loaded_stats, stats);
        assert_eq!(loaded_stats.usage_for("pal-cmd-inbox").count, 2);

        // Zero stray .tmp files
        let stray_tmps: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(stray_tmps.is_empty(), "found stray tmp files: {stray_tmps:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_or_missing_json_falls_back_gracefully() {
        let dir = unique_temp_dir("corrupt");

        let missing = WorkspaceStore::load_or_default(&dir.join("missing.json"));
        assert_eq!(missing.workspaces.len(), 1);
        assert_eq!(missing.workspaces[0].name, DEFAULT_WORKSPACE_NAME);

        for (name, payload) in [
            ("empty.json", ""),
            ("truncated.json", r#"{"workspaces":["#),
            ("wrong_type.json", "[]"),
        ] {
            let p = dir.join(name);
            std::fs::write(&p, payload).unwrap();
            let ws = WorkspaceStore::load_or_default(&p);
            assert_eq!(ws.workspaces.len(), 1, "case {name}");
            assert_eq!(ws.workspaces[0].name, DEFAULT_WORKSPACE_NAME);
            assert!(ws.auto_sort);

            let ps = PaletteStats::load_or_default(&p);
            assert!(ps.usage.is_empty(), "stats case {name}");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn slice1_minimal_workspaces_json_backward_compat() {
        let legacy = r#"{"workspaces":[{"id":"w1","name":"Work"}],"favorites":["h:claude:s1"]}"#;
        let parsed: WorkspaceStore = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.workspaces.len(), 1);
        assert_eq!(parsed.workspaces[0].id, "w1");
        assert!(parsed.auto_sort);
        assert!(parsed.inbox_settings.frequent);
        assert!(parsed.inbox_settings.all);
        assert!(parsed.inbox_settings.favorites);
        assert!(parsed.inbox_settings.frequent_first);
        assert!(parsed.favorites.contains("h:claude:s1"));
    }

    #[test]
    fn canonical_session_key_normalization() {
        assert_eq!(
            canonical_session_key("  VPS:1 ", AgentKind::Claude, " abc-123 "),
            "vps_1:claude:abc-123"
        );
        assert_eq!(
            canonical_session_key("", AgentKind::Codex, "s9"),
            "local:codex:s9"
        );
        assert_eq!(
            canonical_session_key_str("HostA", "  ", "s1"),
            "hosta:claude:s1"
        );
        assert_eq!(
            parse_session_key("vps_1:claude:abc:extra"),
            Some(("vps_1", "claude", "abc:extra"))
        );
        assert_eq!(parse_session_key("bad"), None);
        assert_eq!(parse_session_key(":claude:s1"), None);
    }

    #[test]
    fn reconcile_live_keys_restores_imported_metadata_and_is_idempotent() {
        let sid = "c0a80101-1234-4567-89ab-0123456789ab";
        for host in ["vps0", "remote-1"] {
            let mut store = WorkspaceStore::new();
            let ws = store.create_workspace("Work", None).unwrap();
            let imported = format!("{host}:claude:{sid}");
            let live = format!("vps hetzner:codex:{sid}");
            store.session_meta.insert(
                imported.clone(),
                SessionMeta {
                    workspace_id: Some(ws.id.clone()),
                    pinned: true,
                    starred_pinned: true,
                    muted: true,
                    ..Default::default()
                },
            );
            store.favorites.insert(imported.clone());
            store.inbox_excluded.insert(imported.clone());
            store
                .apply_session_rename(&imported, "Imported title")
                .unwrap();
            store.record_session_view(&live, 123);

            assert_eq!(store.reconcile_live_session_keys(&[live.clone()]), 1);
            let meta = &store.session_meta[&live];
            assert_eq!(meta.workspace_id.as_deref(), Some(ws.id.as_str()));
            assert!(meta.pinned && meta.starred_pinned && meta.muted);
            assert_eq!(meta.last_used_ms, 123);
            assert!(store.is_favorite(&live));
            assert!(store.inbox_excluded.contains(&live));
            assert_eq!(store.effective_title(&live, "fallback"), "Imported title");
            let before = store.clone();
            assert_eq!(store.reconcile_live_session_keys(&[live.clone(), live]), 0);
            assert_eq!(store, before);
        }
    }

    #[test]
    fn reconcile_live_keys_preserves_explicit_workspace_and_title() {
        let sid = "c0a80101-1234-4567-89ab-0123456789ab";
        let mut store = WorkspaceStore::new();
        let ws = store.create_workspace("Work", None).unwrap();
        let imported = format!("vps0:claude:{sid}");
        let live = format!("vps hetzner:codex:{sid}");
        store.assign_session_workspace(&imported, &ws.id);
        store.session_meta.get_mut(&imported).unwrap().pinned = true;
        store.assign_session_workspace(&live, DEFAULT_WORKSPACE_ID);
        store.apply_session_rename(&imported, "Imported").unwrap();
        store.apply_session_rename(&live, "Keep mine").unwrap();
        store.favorites.insert(imported);
        assert_eq!(store.reconcile_live_session_keys(&[live.clone()]), 1);
        assert_eq!(
            store.session_meta[&live].workspace_id.as_deref(),
            Some(DEFAULT_WORKSPACE_ID)
        );
        assert!(!store.session_meta[&live].pinned);
        assert_eq!(store.effective_title(&live, "fallback"), "Keep mine");
        assert!(store.is_favorite(&live));
        assert_eq!(store.reconcile_live_session_keys(&[live]), 0);
    }

    #[test]
    fn reconcile_live_keys_ignores_short_ids_and_missing_workspaces() {
        let mut store = WorkspaceStore::new();
        store.assign_session_workspace("old:claude:short", DEFAULT_WORKSPACE_ID);
        store.favorites.insert("old:claude:short".into());
        store.session_meta.insert(
            "old:claude:long-enough-id".into(),
            SessionMeta {
                workspace_id: Some("deleted".into()),
                ..Default::default()
            },
        );
        let before = store.clone();
        assert_eq!(
            store.reconcile_live_session_keys(&[
                "live:codex:short".into(),
                "live:codex:long-enough-id".into(),
                "bad".into(),
            ]),
            0
        );
        assert_eq!(store, before);
    }

    #[test]
    fn reconcile_live_keys_prefers_agent_and_rejects_ambiguous_sources() {
        let mut store = WorkspaceStore::new();
        let ws = store.create_workspace("Work", None).unwrap();
        store.assign_session_workspace("old:claude:long-enough-id", DEFAULT_WORKSPACE_ID);
        store.assign_session_workspace("old:codex:long-enough-id", &ws.id);
        let live = "live:codex:long-enough-id".to_string();
        assert_eq!(store.reconcile_live_session_keys(&[live.clone()]), 1);
        assert_eq!(
            store.session_meta[&live].workspace_id.as_deref(),
            Some(ws.id.as_str())
        );
        let before = store.clone();
        assert_eq!(
            store.reconcile_live_session_keys(&["live:pi:long-enough-id".into()]),
            0
        );
        assert_eq!(store, before);
    }

    #[test]
    fn reconcile_live_keys_supports_favorites_without_metadata() {
        let mut store = WorkspaceStore::new();
        store.favorites.insert("old:claude:long-enough-id".into());
        let live = "live:codex:long-enough-id".to_string();
        assert_eq!(
            store.reconcile_live_session_keys(&[live.clone(), live.clone()]),
            1
        );
        assert!(store.is_favorite(&live));
        assert_eq!(store.reconcile_live_session_keys(&[live]), 0);
    }

    #[test]
    fn alt_digit_mapping_starred_and_visible_slots() {
        let mut store = WorkspaceStore::new();
        let w2 = store.create_workspace("Alpha", None).unwrap();
        let w3 = store.create_workspace("Archived", None).unwrap();
        let w4 = store.create_workspace("Snoozed", None).unwrap();
        let w5 = store.create_workspace("Beta", None).unwrap();
        let now = 10_000;
        store.archive_workspace(&w3.id).unwrap();
        store.snooze_workspace(&w4.id, now + 5_000, now).unwrap();

        assert_eq!(
            store.resolve_alt_digit(1, now).as_deref(),
            Some(STARRED_WORKSPACE_ID)
        );
        assert_eq!(
            store.resolve_alt_digit(2, now).as_deref(),
            Some(DEFAULT_WORKSPACE_ID)
        );
        assert_eq!(
            store.resolve_alt_digit(3, now).as_deref(),
            Some(w2.id.as_str())
        );
        // Archived and snoozed skipped
        assert_eq!(
            store.resolve_alt_digit(4, now).as_deref(),
            Some(w5.id.as_str())
        );
        assert_eq!(store.resolve_alt_digit(5, now), None);
        assert_eq!(store.resolve_alt_digit(0, now), None);
        assert_eq!(store.resolve_alt_digit(10, now), None);
    }

    #[test]
    fn workspace_rename_rejects_other_case_insensitively() {
        let mut store = WorkspaceStore::new();
        let ws = store.create_workspace("Alpha", None).unwrap();

        assert_eq!(
            store.create_workspace("Other", None),
            Err(WorkspaceError::ReservedName)
        );
        assert_eq!(
            store.create_workspace("  oThEr  ", None),
            Err(WorkspaceError::ReservedName)
        );

        assert_eq!(
            store.rename_workspace(&ws.id, "Other"),
            Err(WorkspaceError::ReservedName)
        );
        assert_eq!(
            store.rename_workspace(&ws.id, "  OTHER  "),
            Err(WorkspaceError::ReservedName)
        );
        assert_eq!(
            store.rename_workspace(&ws.id, "   "),
            Err(WorkspaceError::EmptyName)
        );

        // Existing "Other" workspace cannot be renamed to anything else
        store.workspaces.push(Workspace {
            id: "legacy-other".into(),
            name: "Other".into(),
            ..Default::default()
        });
        assert_eq!(
            store.rename_workspace("legacy-other", "ValidName"),
            Err(WorkspaceError::ReservedName)
        );

        // >63 Unicode char truncation
        let long_name = "🚀日Kraków".repeat(15);
        store.rename_workspace(&ws.id, &long_name).unwrap();
        let updated = store.workspace_by_id(&ws.id).unwrap();
        assert_eq!(updated.name.chars().count(), MAX_WORKSPACE_NAME_CHARS);
    }

    #[test]
    fn workspace_delete_empty_nonempty_and_last_keeps_general() {
        let mut store = WorkspaceStore::new();
        let work = store.create_workspace("Work", None).unwrap();
        let empty = store.create_workspace("Empty", None).unwrap();

        // Starred cannot be deleted
        assert_eq!(
            store.delete_workspace(STARRED_WORKSPACE_ID, None),
            Err(WorkspaceError::CannotDeleteStarred)
        );

        // Empty deletes immediately
        let out = store.delete_workspace(&empty.id, None).unwrap();
        assert_eq!(out.moved_sessions, 0);
        assert!(!out.created_default_general);
        assert!(store.workspace_by_id(&empty.id).is_none());

        // Non-empty requires target
        let s1 = canonical_session_key("h", AgentKind::Claude, "s1");
        store.assign_session_workspace(&s1, &work.id);
        let grp = store.create_group(&work.id, "Backend", None);
        store.session_meta.get_mut(&s1).unwrap().group_id = Some(grp.id.clone());

        assert_eq!(
            store.delete_workspace(&work.id, None),
            Err(WorkspaceError::NonEmptyRequiresTarget { session_count: 1 })
        );
        // Self / starred / archived targets rejected
        assert!(matches!(
            store.delete_workspace(&work.id, Some(&work.id)),
            Err(WorkspaceError::InvalidMoveTarget(_))
        ));
        assert!(matches!(
            store.delete_workspace(&work.id, Some(STARRED_WORKSPACE_ID)),
            Err(WorkspaceError::InvalidMoveTarget(_))
        ));

        let archived = store.create_workspace("Arch", None).unwrap();
        store.archive_workspace(&archived.id).unwrap();
        assert!(matches!(
            store.delete_workspace(&work.id, Some(&archived.id)),
            Err(WorkspaceError::InvalidMoveTarget(_))
        ));

        // Valid target moves sessions and groups
        let out = store
            .delete_workspace(&work.id, Some(DEFAULT_WORKSPACE_ID))
            .unwrap();
        assert_eq!(out.moved_sessions, 1);
        assert_eq!(out.moved_to.as_deref(), Some(DEFAULT_WORKSPACE_ID));
        assert_eq!(
            store.session_meta[&s1].workspace_id.as_deref(),
            Some(DEFAULT_WORKSPACE_ID)
        );
        assert_eq!(
            store
                .groups
                .iter()
                .find(|g| g.id == grp.id)
                .unwrap()
                .workspace_id,
            DEFAULT_WORKSPACE_ID
        );

        // Deleting last empty workspace recreates default General
        let mut solo = WorkspaceStore::new();
        let last_out = solo.delete_workspace(DEFAULT_WORKSPACE_ID, None).unwrap();
        assert!(last_out.created_default_general);
        assert_eq!(solo.workspaces.len(), 1);
        assert_eq!(solo.workspaces[0].id, DEFAULT_WORKSPACE_ID);
        assert_eq!(solo.workspaces[0].name, DEFAULT_WORKSPACE_NAME);
    }

    #[test]
    fn workspace_snooze_expiry_exact_boundary_and_wake() {
        let mut store = WorkspaceStore::new();
        let w2 = store.create_workspace("Focus", None).unwrap();
        let now = 50_000;
        let until = 60_000;

        assert!(store.can_snooze_workspace(&w2.id, now));
        let next = store.snooze_workspace(&w2.id, until, now).unwrap();
        assert_eq!(next.as_deref(), Some(DEFAULT_WORKSPACE_ID));

        // At until - 1: still snoozed
        assert!(store.workspace_by_id(&w2.id).unwrap().is_snoozed_at(until - 1));
        assert_eq!(store.resolve_alt_digit(3, until - 1), None);

        // At exact boundary now == until: awake!
        assert!(!store.workspace_by_id(&w2.id).unwrap().is_snoozed_at(until));
        assert_eq!(
            store.resolve_alt_digit(3, until).as_deref(),
            Some(w2.id.as_str())
        );

        let woken = store.wake_due_workspaces(until);
        assert_eq!(woken, vec![w2.id.clone()]);
        assert!(!store.workspace_by_id(&w2.id).unwrap().snoozed);
        assert_eq!(store.workspace_by_id(&w2.id).unwrap().snoozed_until_ms, None);

        // Manual early wake
        store.snooze_workspace(&w2.id, until + 10_000, until).unwrap();
        assert!(store.wake_workspace(&w2.id));
        assert!(!store.workspace_by_id(&w2.id).unwrap().is_snoozed_at(until));
    }

    #[test]
    fn favorite_toggle_clears_starred_group_and_prunes_empty() {
        let mut store = WorkspaceStore::new();
        let s1 = canonical_session_key("h", AgentKind::Claude, "s1");
        let s2 = canonical_session_key("h", AgentKind::Claude, "s2");

        // Normal workspace group must stay untouched
        let norm_gid = store
            .toggle_group_for_session(&s1, DEFAULT_WORKSPACE_ID, None, "NormalGrp")
            .unwrap();

        assert!(store.toggle_favorite(&s1));
        assert!(store.toggle_favorite(&s2));
        let star_gid_1 = store
            .toggle_group_for_session(&s1, STARRED_WORKSPACE_ID, None, "StarSolo")
            .unwrap();
        let star_gid_2 = store
            .toggle_group_for_session(&s2, STARRED_WORKSPACE_ID, None, "StarKeep")
            .unwrap();

        // Unfavorite s1 -> clears starred_group_id and prunes StarSolo only
        assert!(!store.toggle_favorite(&s1));
        assert_eq!(store.session_meta[&s1].starred_group_id, None);
        assert!(!store.groups.iter().any(|g| g.id == star_gid_1));
        assert!(store.groups.iter().any(|g| g.id == star_gid_2));
        assert!(store.groups.iter().any(|g| g.id == norm_gid));
    }

    #[test]
    fn sidebar_two_tier_comparator_and_groups_order() {
        let mut store = WorkspaceStore::new();
        let s_grp_rec = make_session("grec", AgentKind::Claude, "G Recent", 500, false, false);
        let s_grp_pin = make_session("gpin", AgentKind::Claude, "G Pinned", 100, false, false);
        let s_pin_a = make_session("pina", AgentKind::Claude, "Pin A", 50, false, false);
        let s_pin_b = make_session("pinb", AgentKind::Claude, "Pin B", 900, false, false);
        let s_rec_muted = make_session("rmut", AgentKind::Claude, "Muted New", 999, false, false);
        let s_rec_old = make_session("rold", AgentKind::Claude, "Active Old", 200, false, false);
        let s_rec_new = make_session("rnew", AgentKind::Claude, "Active New", 400, false, false);

        let all = [
            s_grp_rec,
            s_grp_pin,
            s_pin_a,
            s_pin_b,
            s_rec_muted,
            s_rec_old,
            s_rec_new,
        ];
        let inputs: Vec<_> = all.iter().map(|s| make_input("h", s)).collect();

        let k = |idx: usize| inputs[idx].key.clone();
        let grp = store.create_group(DEFAULT_WORKSPACE_ID, "Core", None);
        store.toggle_group_for_session(&k(0), DEFAULT_WORKSPACE_ID, Some(&grp.id), "");
        store.toggle_group_for_session(&k(1), DEFAULT_WORKSPACE_ID, Some(&grp.id), "");
        store.toggle_pin(&k(1), false); // pinned inside group beats newer recent inside group

        // Ungrouped pinned in manual order (pin_b=0, pin_a=1)
        store.toggle_pin(&k(2), false);
        store.toggle_pin(&k(3), false);
        store.session_meta.get_mut(&k(3)).unwrap().manual_order = Some(0);
        store.session_meta.get_mut(&k(2)).unwrap().manual_order = Some(1);

        // Muted recent sorts AFTER non-muted recents even though it has newer timestamp
        store.toggle_session_muted(&k(4));

        let layout = store.compute_sidebar_layout(DEFAULT_WORKSPACE_ID, &inputs, false, 1000);
        assert_eq!(layout.groups.len(), 1);
        assert_eq!(
            layout.groups[0]
                .items
                .iter()
                .map(|r| r.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["gpin", "grec"]
        );
        assert_eq!(
            layout
                .pinned_tiles
                .iter()
                .map(|r| r.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["pinb", "pina"]
        );
        assert_eq!(
            layout
                .recents
                .iter()
                .map(|r| r.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["rnew", "rold", "rmut"]
        );
        assert_eq!(
            layout.flat_display_order,
            vec![k(1), k(0), k(3), k(2), k(6), k(5), k(4)]
        );

        // Starred workspace uses independent starred_pinned & starred_group_id
        store.toggle_favorite(&k(5));
        store.toggle_favorite(&k(6));
        store.toggle_pin(&k(5), true); // pinned ONLY in Starred
        let star_layout = store.compute_sidebar_layout(STARRED_WORKSPACE_ID, &inputs, false, 1000);
        assert_eq!(star_layout.pinned_tiles.len(), 1);
        assert_eq!(star_layout.pinned_tiles[0].session_id, "rold");
        assert_eq!(star_layout.recents.len(), 1);
        assert_eq!(star_layout.recents[0].session_id, "rnew");
        // Normal workspace pin unchanged
        assert!(!store.is_pinned(&k(5), false));
    }

    #[test]
    fn sidebar_manual_reorder_materializes_and_respects_boundaries() {
        let mut store = WorkspaceStore::new();
        let s1 = make_session("g1", AgentKind::Claude, "G1", 300, false, false);
        let s2 = make_session("g2", AgentKind::Claude, "G2", 200, false, false);
        let s3 = make_session("p1", AgentKind::Claude, "P1", 100, false, false);
        let s4 = make_session("r1", AgentKind::Claude, "R1", 500, false, false);
        let s5 = make_session("r2", AgentKind::Claude, "R2", 400, false, false);
        let all = [s1, s2, s3, s4, s5];
        let inputs: Vec<_> = all.iter().map(|s| make_input("h", s)).collect();
        let k = |i: usize| inputs[i].key.clone();

        let grp = store.create_group(DEFAULT_WORKSPACE_ID, "G", None);
        store.toggle_group_for_session(&k(0), DEFAULT_WORKSPACE_ID, Some(&grp.id), "");
        store.toggle_group_for_session(&k(1), DEFAULT_WORKSPACE_ID, Some(&grp.id), "");
        store.toggle_pin(&k(2), false);

        assert!(store.auto_sort);
        // Move g2 up inside group -> materializes order and disables auto_sort
        assert!(store.reorder_session(
            DEFAULT_WORKSPACE_ID,
            &k(1),
            ReorderDirection::Up,
            &inputs,
            1000
        ));
        assert!(!store.auto_sort);

        // Moving g2 up again clamps at top of group (cannot escape)
        assert!(!store.reorder_session(
            DEFAULT_WORKSPACE_ID,
            &k(1),
            ReorderDirection::Up,
            &inputs,
            1000
        ));
        // Moving g1 down clamps at bottom of group (cannot enter pinned/recents)
        assert!(!store.reorder_session(
            DEFAULT_WORKSPACE_ID,
            &k(0),
            ReorderDirection::Down,
            &inputs,
            1000
        ));
        // Moving solo pinned item neither up nor down crosses into groups or recents
        assert!(!store.reorder_session(
            DEFAULT_WORKSPACE_ID,
            &k(2),
            ReorderDirection::Up,
            &inputs,
            1000
        ));
        assert!(!store.reorder_session(
            DEFAULT_WORKSPACE_ID,
            &k(2),
            ReorderDirection::Down,
            &inputs,
            1000
        ));
        // Moving r2 up swaps with r1 inside recents only
        assert!(store.reorder_session(
            DEFAULT_WORKSPACE_ID,
            &k(4),
            ReorderDirection::Up,
            &inputs,
            1000
        ));
        assert!(!store.reorder_session(
            DEFAULT_WORKSPACE_ID,
            &k(4),
            ReorderDirection::Up,
            &inputs,
            1000
        ));

        let layout = store.compute_sidebar_layout(DEFAULT_WORKSPACE_ID, &inputs, false, 1000);
        assert_eq!(layout.groups[0].items[0].session_id, "g2");
        assert_eq!(layout.groups[0].items[1].session_id, "g1");
        assert_eq!(layout.recents[0].session_id, "r2");
        assert_eq!(layout.recents[1].session_id, "r1");
    }

    #[test]
    fn ctrl_digit_resolution_pinned_first_and_unread_only() {
        let mut store = WorkspaceStore::new();
        let s_grp = make_session("g1", AgentKind::Claude, "Grouped", 900, true, false);
        let s_pin = make_session("p1", AgentKind::Claude, "Pinned", 100, true, false);
        let s_rec_read = make_session("r1", AgentKind::Claude, "Rec Read", 800, false, false);
        let s_rec_unread = make_session("r2", AgentKind::Claude, "Rec Unread", 700, true, false);
        let all = [s_grp, s_pin, s_rec_read, s_rec_unread];
        let inputs: Vec<_> = all.iter().map(|s| make_input("h", s)).collect();
        let k = |i: usize| inputs[i].key.clone();

        let grp = store.create_group(DEFAULT_WORKSPACE_ID, "Grp", None);
        store.toggle_group_for_session(&k(0), DEFAULT_WORKSPACE_ID, Some(&grp.id), "");
        store.toggle_pin(&k(1), false);

        // Open group: pinned first (1), then group (2), then recents (3, 4)
        let layout = store.compute_sidebar_layout(DEFAULT_WORKSPACE_ID, &inputs, false, 1000);
        assert_eq!(layout.resolve_ctrl_digit(1), Some(k(1).as_str()));
        assert_eq!(layout.resolve_ctrl_digit(2), Some(k(0).as_str()));
        assert_eq!(layout.resolve_ctrl_digit(3), Some(k(2).as_str()));
        assert_eq!(layout.resolve_ctrl_digit(4), Some(k(3).as_str()));

        // Collapsed group skipped in Ctrl+1..9
        store.toggle_group_collapsed(&grp.id);
        let collapsed = store.compute_sidebar_layout(DEFAULT_WORKSPACE_ID, &inputs, false, 1000);
        assert_eq!(collapsed.resolve_ctrl_digit(1), Some(k(1).as_str()));
        assert_eq!(collapsed.resolve_ctrl_digit(2), Some(k(2).as_str()));
        assert_eq!(collapsed.resolve_ctrl_digit(3), Some(k(3).as_str()));

        // unread_only skips read sessions
        let unread_layout = store.compute_sidebar_layout(DEFAULT_WORKSPACE_ID, &inputs, true, 1000);
        assert_eq!(unread_layout.resolve_ctrl_digit(1), Some(k(1).as_str()));
        assert_eq!(unread_layout.resolve_ctrl_digit(2), Some(k(3).as_str()));
        assert_eq!(unread_layout.resolve_ctrl_digit(3), None);
    }

    #[test]
    fn frequent_24h_input_and_6h_dwell_exact_boundaries() {
        let now = 100_000_000_i64;
        // Input exact 24h boundary
        assert!(qualifies_for_frequent(
            &[],
            Some(now - FREQUENT_INPUT_WINDOW_MS),
            now
        ));
        assert!(!qualifies_for_frequent(
            &[],
            Some(now - FREQUENT_INPUT_WINDOW_MS - 1),
            now
        ));

        // FrontPeriodTracker dwell & focus-loss reset
        let mut tracker = FrontPeriodTracker::new();
        assert!(!tracker.select(Some("h:claude:s1"), false, true, now, 1000));
        assert!(!tracker.select(Some("h:claude:s1"), true, false, now, 1000));
        assert!(tracker.select(Some("h:claude:s1"), true, true, now, 1000));
        assert_eq!(tracker.poll_visit(10_999), None);
        // Focus loss resets timer
        assert!(!tracker.select(Some("h:claude:s1"), true, false, now + 5000, 10_500));
        assert!(tracker.select(Some("h:claude:s1"), true, true, now + 6000, 11_000));
        assert_eq!(tracker.poll_visit(20_999), None);
        assert_eq!(
            tracker.poll_visit(21_000),
            Some(("h:claude:s1".to_string(), now + 6000))
        );
        // Emits only once per period
        assert_eq!(tracker.poll_visit(30_000), None);

        // 3 visits exact 6h boundary
        let ok_visits = [now - FREQUENT_VISIT_WINDOW_MS, now - 2, now - 1];
        let old_visits = [now - FREQUENT_VISIT_WINDOW_MS - 1, now - 2, now - 1];
        assert!(qualifies_for_frequent(&ok_visits, None, now));
        assert!(!qualifies_for_frequent(&old_visits, None, now));

        // Max 8 visits stored (evicts oldest)
        let mut store = WorkspaceStore::new();
        for i in 1..=10 {
            store.record_frequent_visit("h:claude:s1", i * 100);
        }
        let visits = &store.session_meta["h:claude:s1"].frequent_visits_ms;
        assert_eq!(visits.len(), FREQUENT_MAX_VISITS);
        assert_eq!(visits[0], 300);
        assert_eq!(*visits.last().unwrap(), 1000);

        // frequent_order_key prefers recent input even if a visit is newer
        let recent_input = now - 5_000;
        let newer_visit = now - 1_000;
        assert_eq!(
            frequent_order_key(&[newer_visit], Some(recent_input), now),
            recent_input
        );
        // Once input expires, falls back to newest visit
        assert_eq!(
            frequent_order_key(
                &[newer_visit],
                Some(now - FREQUENT_INPUT_WINDOW_MS - 1),
                now
            ),
            newer_visit
        );
    }

    #[test]
    fn inbox_sort_recency_before_unread_and_tiebreakers() {
        let mut store = WorkspaceStore::new();
        let w2 = store.create_workspace("Zeta", None).unwrap();
        let now = 10_000_000_i64;

        let s_new_read = make_session("s1", AgentKind::Claude, "Newest Read", now - 10, false, false);
        let s_old_unread = make_session("s2", AgentKind::Codex, "Older Unread Working", now - 500, true, true);
        let s_tie_unread = make_session("s3", AgentKind::Antigravity, "Tie Unread", now - 500, true, false);
        let s_tie_read = make_session("s4", AgentKind::Claude, "Tie Read", now - 500, false, false);
        let s_muted = make_session("s5", AgentKind::Claude, "Muted", now, true, true);
        let s_excl = make_session("s6", AgentKind::Claude, "Excluded", now, true, true);

        let all = [
            s_new_read,
            s_old_unread,
            s_tie_unread,
            s_tie_read,
            s_muted,
            s_excl,
        ];
        let inputs: Vec<_> = all.iter().map(|s| make_input("h", s)).collect();
        let k = |i: usize| inputs[i].key.clone();

        store.assign_session_workspace(&k(1), &w2.id);
        store.toggle_session_muted(&k(4));
        store.toggle_inbox_excluded(&k(5));

        let rows = store.query_inbox(&InboxSplit::All, &inputs, false, "", now);
        let ids: Vec<&str> = rows.iter().map(|r| r.session_id.as_str()).collect();
        // Newest read beats older unread+working; at equal timestamp, unread+working > unread > read
        assert_eq!(ids, vec!["s1", "s2", "s3", "s4"]);

        // unread_only filter
        let unread_rows = store.query_inbox(&InboxSplit::All, &inputs, true, "", now);
        assert_eq!(
            unread_rows
                .iter()
                .map(|r| r.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["s2", "s3"]
        );

        // Live substring filter across title, cwd, preview, workspace name, agent label
        assert_eq!(
            store
                .query_inbox(&InboxSplit::All, &inputs, false, "zeta", now)
                .len(),
            1
        );
        assert_eq!(
            store
                .query_inbox(&InboxSplit::All, &inputs, false, "antigravity", now)
                .len(),
            1
        );
        assert_eq!(
            store
                .query_inbox(&InboxSplit::All, &inputs, false, "/repo/s1", now)
                .len(),
            1
        );

        // Frequent split sorts by frequent_order_key desc
        store.record_session_input(&k(3), now - 50);
        store.record_session_input(&k(1), now - 200);
        let freq_rows = store.query_inbox(&InboxSplit::Frequent, &inputs, false, "", now);
        assert_eq!(
            freq_rows
                .iter()
                .map(|r| r.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["s4", "s2"]
        );
    }

    #[test]
    fn inbox_splits_with_sessions_hides_zero_count_splits_and_keeps_all() {
        let mut store = WorkspaceStore::new();
        let now = 10_000_000_i64;

        // No sessions at all: only `All` survives so the Inbox keeps a live tab
        assert_eq!(
            store.inbox_splits_with_sessions(&[], false, "", now),
            vec![InboxSplit::All]
        );

        let back = store.create_workspace("Backend", None).unwrap();
        let shelf = store.create_workspace("Empty Shelf", None).unwrap();

        let s1 = make_session("s1", AgentKind::Claude, "Favorite", now - 10, false, false);
        let s2 = make_session("s2", AgentKind::Codex, "Frequent", now - 20, false, false);
        let s3 = make_session("s3", AgentKind::Claude, "Backend", now - 30, false, false);
        let all = [s1, s2, s3];
        let inputs: Vec<_> = all.iter().map(|s| make_input("h", s)).collect();
        let k = |i: usize| inputs[i].key.clone();

        store.toggle_favorite(&k(0));
        store.record_session_input(&k(1), now);
        store.assign_session_workspace(&k(2), &back.id);

        let splits = store.inbox_splits_with_sessions(&inputs, false, "", now);
        assert_eq!(
            splits,
            vec![
                InboxSplit::Frequent,
                InboxSplit::All,
                InboxSplit::Favorites,
                // s1 and s2 never leave the default workspace
                InboxSplit::Workspace(DEFAULT_WORKSPACE_ID.to_string()),
                InboxSplit::Workspace(back.id.clone()),
            ],
            "only splits with >= 1 session stay in the Inbox split bar"
        );
        assert!(
            !splits.contains(&InboxSplit::Workspace(shelf.id.clone())),
            "an empty custom workspace must be hidden"
        );

        // The workspace lists behind the Sidebar switcher and Settings stay unfiltered
        assert!(
            store
                .visible_workspaces(now)
                .iter()
                .any(|w| w.id == shelf.id),
            "empty workspaces must stay manageable in the Sidebar and Settings"
        );

        // A filter that matches nothing leaves only `All`
        assert_eq!(
            store.inbox_splits_with_sessions(&inputs, false, "no-such-title", now),
            vec![InboxSplit::All]
        );

        // An unread-only filter with nothing unread leaves only `All`
        assert_eq!(
            store.inbox_splits_with_sessions(&inputs, true, "", now),
            vec![InboxSplit::All]
        );

        // Emptying a split hides it again
        store.toggle_favorite(&k(0));
        assert!(
            !store
                .inbox_splits_with_sessions(&inputs, false, "", now)
                .contains(&InboxSplit::Favorites),
            "Favorites 0 must drop out of the split bar"
        );
    }

    #[test]
    fn inbox_waiting_undo_and_auto_unarchive_and_unread_badge() {
        let mut store = WorkspaceStore::new();
        let now = 50_000;
        let mut s1 = make_session("s1", AgentKind::Claude, "One", 1000, true, false);
        let s2 = make_session("s2", AgentKind::Claude, "Two", 1000, true, false);
        let s3 = make_session("s3", AgentKind::Claude, "Muted", 1000, true, false);
        let all = [s1.clone(), s2.clone(), s3.clone()];
        let inputs: Vec<_> = all.iter().map(|s| make_input("h", s)).collect();
        let k1 = inputs[0].key.clone();
        let k2 = inputs[1].key.clone();
        let k3 = inputs[2].key.clone();
        store.toggle_session_muted(&k3);

        // Unread badge excludes muted
        assert_eq!(store.inbox_unread_count(&inputs), 2);
        store.mark_session_read(&k1, &s1);
        assert_eq!(store.inbox_unread_count(&inputs), 1);

        // Archive to Waiting hides from Inbox and Sidebar
        store.archive_to_waiting(&k1, now, s1.updated_at_ms, s1.last_assistant_id.clone());
        store.archive_to_waiting(&k2, now + 1, s2.updated_at_ms, s2.last_assistant_id.clone());
        assert!(store
            .query_inbox(&InboxSplit::All, &inputs, false, "", now + 2)
            .is_empty());
        assert_eq!(
            store
                .compute_sidebar_layout(DEFAULT_WORKSPACE_ID, &inputs, false, now + 2)
                .total_visible_sessions(),
            1 // only s3 (muted) remains in sidebar
        );

        // Undo LIFO restores s2 first
        assert_eq!(store.undo_last_waiting(), Some(k2.clone()));
        assert!(!store.waiting_sessions.contains_key(&k2));
        assert!(store.waiting_sessions.contains_key(&k1));

        // New assistant activity auto-unarchives s1
        s1.updated_at_ms = 2000;
        s1.last_assistant_id = Some("asst-s1-2000".into());
        s1.last_assistant_ms = Some(2000);
        assert!(store.auto_unarchive_waiting_if_active(&k1, &s1));
        assert!(!store.waiting_sessions.contains_key(&k1));
    }

    #[test]
    fn palette_empty_query_recent5_most_used10_and_8_categories() {
        let mut store = WorkspaceStore::new();
        let snoozed_ws = store.create_workspace("SnoozedWS", None).unwrap();
        let now = 100_000_000_i64;
        store.snooze_workspace(&snoozed_ws.id, now + 10_000, now).unwrap();

        let summaries: Vec<SessionSummary> = (0..9)
            .map(|i| {
                make_session(
                    &format!("s{i}"),
                    AgentKind::Claude,
                    &format!("Session {i}"),
                    now - (i as i64) * 100,
                    false,
                    false,
                )
            })
            .collect();
        let inputs: Vec<_> = summaries.iter().map(|s| make_input("h", s)).collect();
        let k = |i: usize| inputs[i].key.clone();

        // s0 = current (excluded), s1 = previous (must lead), s2 = snoozed ws (excluded), s3 = waiting (excluded)
        store.assign_session_workspace(&k(2), &snoozed_ws.id);
        store.archive_to_waiting(&k(3), now, 0, None);
        for i in 0..9 {
            store.record_session_view(&k(i), now - (9 - i as i64) * 10);
        }

        let mut stats = PaletteStats::default();
        let mut cmds = Vec::new();
        for i in 0..12 {
            let id = format!("cmd.{i}");
            cmds.push(PaletteCommandSpec {
                id: id.clone(),
                label: format!("Command {i}"),
                chord: String::new(),
                category: PaletteCategory::ALL[i % 8],
                destructive: i == 0 || i == 11,
                aliases: Vec::new(),
                tab_context: true,
                global: true,
            });
            for _ in 0..(12 - i) {
                stats.record_command_use(&id, now - 1000);
            }
        }
        // Add an unused safe & destructive command in Sessions
        cmds.push(PaletteCommandSpec {
            id: "unused.dest".into(),
            label: "AAA Destructive".into(),
            chord: String::new(),
            category: PaletteCategory::Sessions,
            destructive: true,
            aliases: Vec::new(),
            tab_context: false,
            global: true,
        });
        cmds.push(PaletteCommandSpec {
            id: "unused.safe".into(),
            label: "ZZZ Safe".into(),
            chord: String::new(),
            category: PaletteCategory::Sessions,
            destructive: false,
            aliases: Vec::new(),
            tab_context: false,
            global: true,
        });

        let rows = rank_palette(
            &store,
            &stats,
            &inputs,
            &cmds,
            "",
            &PaletteMode::Global,
            Some(&k(0)),
            Some(&k(1)),
            now,
        );

        // Verify Recent sessions: max 5, leads with s1, excludes s0, s2, s3
        assert_eq!(rows[0], PaletteRow::Header("Recent sessions".into()));
        let recent_sids: Vec<&str> = rows[1..]
            .iter()
            .take_while(|r| matches!(r, PaletteRow::Session { .. }))
            .map(|r| match r {
                PaletteRow::Session { session_key, .. } => session_key.as_str(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(recent_sids.len(), 5);
        assert_eq!(recent_sids[0], k(1));
        assert!(!recent_sids.contains(&k(0).as_str()));
        assert!(!recent_sids.contains(&k(2).as_str()));
        assert!(!recent_sids.contains(&k(3).as_str()));

        // Verify Most used has exactly 10 commands and safe precedes destructive
        let mu_idx = rows
            .iter()
            .position(|r| *r == PaletteRow::Header("Most used".into()))
            .unwrap();
        let mu_cmds: Vec<bool> = rows[mu_idx + 1..]
            .iter()
            .take_while(|r| matches!(r, PaletteRow::Command { .. }))
            .map(|r| match r {
                PaletteRow::Command { destructive, .. } => *destructive,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(mu_cmds.len(), 10);
        // Destructive cmd.0 must appear AFTER all 9 safe commands
        assert_eq!(mu_cmds[9], true);
        assert!(mu_cmds[..9].iter().all(|&d| !d));

        // Verify Sessions category puts unused.safe before unused.dest
        let sess_hdr = rows
            .iter()
            .position(|r| *r == PaletteRow::Header("Sessions".into()))
            .unwrap();
        let sess_ids: Vec<&str> = rows[sess_hdr + 1..]
            .iter()
            .take_while(|r| matches!(r, PaletteRow::Command { .. }))
            .map(|r| match r {
                PaletteRow::Command { id, .. } => id.as_str(),
                _ => unreachable!(),
            })
            .collect();
        let safe_pos = sess_ids.iter().position(|&id| id == "unused.safe").unwrap();
        let dest_pos = sess_ids.iter().position(|&id| id == "unused.dest").unwrap();
        assert!(safe_pos < dest_pos);
    }

    #[test]
    fn palette_typed_match_ranks_frecency_band_dominance_and_del_boost() {
        // 4 > 3 > 2 > 1 > 0 including Unicode
        assert_eq!(palette_text_match_rank("Kraków", "kraków"), 4);
        assert_eq!(palette_text_match_rank("Kraków Server", "kraków"), 3);
        assert_eq!(palette_text_match_rank("Open Kraków", "kraków"), 2);
        assert_eq!(palette_text_match_rank("MyKrakówApp", "kraków"), 1);
        assert_eq!(palette_text_match_rank("Warsaw", "kraków"), 0);

        // Recency band strictly dominates lifetime count (step 16 > max freq 15)
        let now = 200_i64 * 24 * 3_600_000;
        let six_days_once = palette_frecency_at(
            PaletteUsage {
                count: 1,
                last_used_ms: now - 6 * 24 * 3_600_000,
            },
            now,
        );
        let eight_days_max = palette_frecency_at(
            PaletteUsage {
                count: u32::MAX,
                last_used_ms: now - 8 * 24 * 3_600_000,
            },
            now,
        );
        assert_eq!(six_days_once, 64);
        assert_eq!(eight_days_max, 48 + 15);
        assert!(six_days_once > eight_days_max);

        // "del" prefix boosts Delete session over Delete workspace even if workspace used more
        let store = WorkspaceStore::new();
        let mut stats = PaletteStats::default();
        for _ in 0..50 {
            stats.record_command_use("workspace.delete", now - 1000);
        }
        let cmds = vec![
            PaletteCommandSpec {
                id: "workspace.delete".into(),
                label: "Delete workspace".into(),
                chord: String::new(),
                category: PaletteCategory::WorkspacesAndWindows,
                destructive: true,
                aliases: vec![],
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "session.delete".into(),
                label: "Delete session".into(),
                chord: "Ctrl+Shift+W".into(),
                category: PaletteCategory::Sessions,
                destructive: true,
                aliases: vec![],
                tab_context: true,
                global: true,
            },
        ];

        let rows = rank_palette(
            &store,
            &stats,
            &[],
            &cmds,
            "del",
            &PaletteMode::Global,
            None,
            None,
            now,
        );
        assert_eq!(rows.len(), 2);
        match &rows[0] {
            PaletteRow::Command { id, .. } => assert_eq!(id, "session.delete"),
            other => panic!("expected session.delete first, got {other:?}"),
        }

        // TabContext excludes global-only commands & sessions
        let s1 = make_session("s1", AgentKind::Claude, "Delete me", now, false, false);
        let inputs = [make_input("h", &s1)];
        let tab_rows = rank_palette(
            &store,
            &stats,
            &inputs,
            &cmds,
            "del",
            &PaletteMode::TabContext {
                target_session_key: inputs[0].key.clone(),
            },
            None,
            None,
            now,
        );
        assert_eq!(tab_rows.len(), 1);
        match &tab_rows[0] {
            PaletteRow::Command { id, .. } => assert_eq!(id, "session.delete"),
            other => panic!("expected tab_context command only, got {other:?}"),
        }
    }

    #[test]
    fn canonical_f2_rename_utf8_63_char_cap_and_blank_rejection() {
        let mut store = WorkspaceStore::new();
        let key = canonical_session_key("h", AgentKind::Claude, "s1");

        // Blank/whitespace rejected
        assert_eq!(canonicalize_session_title("   \t\r\n  "), None);
        assert_eq!(store.apply_session_rename(&key, "   "), None);
        assert!(!store.session_titles.contains_key(&key));

        // Trims surrounding whitespace and locks manual: true
        let renamed = store.apply_session_rename(&key, "  Deploy Fix  ").unwrap();
        assert_eq!(renamed, "Deploy Fix");
        assert!(store.session_titles[&key].manual);
        assert_eq!(store.effective_title(&key, "Original"), "Deploy Fix");

        // Blank rename preserves existing name
        assert_eq!(store.apply_session_rename(&key, "\t  \n"), None);
        assert_eq!(store.effective_title(&key, "Original"), "Deploy Fix");

        // 70-char multibyte UTF-8 truncates cleanly to exact 63 chars
        let utf8_70 = "🚀日Kraków🚀日Kraków🚀日Kraków🚀日Kraków🚀日Kraków🚀日Kraków🚀日Kraków🚀日Kraków🚀日Kraków";
        assert!(utf8_70.chars().count() > 63);
        let capped = store.apply_session_rename(&key, utf8_70).unwrap();
        assert_eq!(capped.chars().count(), 63);
        assert_eq!(store.effective_title(&key, "Original"), capped);
    }

    #[test]
    fn toggle_session_group_collapsed_handles_normal_starred_and_ungrouped() {
        let mut store = WorkspaceStore::new();
        let key = canonical_session_key("h", AgentKind::Claude, "s1");

        // Ungrouped session is a no-op
        assert_eq!(
            store.toggle_session_group_collapsed(&key, DEFAULT_WORKSPACE_ID),
            None
        );
        assert!(store.groups.is_empty());

        // Normal scope group collapses and expands without ungrouping
        let gid = store
            .toggle_group_for_session(&key, DEFAULT_WORKSPACE_ID, None, "Backend")
            .unwrap();
        assert_eq!(
            store.toggle_session_group_collapsed(&key, DEFAULT_WORKSPACE_ID),
            Some(true)
        );
        assert!(store.groups.iter().find(|g| g.id == gid).unwrap().collapsed);
        assert_eq!(
            store.session_meta[&key].group_id.as_deref(),
            Some(gid.as_str())
        );
        assert_eq!(
            store.toggle_session_group_collapsed(&key, DEFAULT_WORKSPACE_ID),
            Some(false)
        );

        // Starred scope group collapses and expands independently
        store.toggle_favorite(&key);
        assert_eq!(
            store.toggle_session_group_collapsed(&key, STARRED_WORKSPACE_ID),
            None
        );
        let sgid = store
            .toggle_group_for_session(&key, STARRED_WORKSPACE_ID, None, "StarGrp")
            .unwrap();
        assert_eq!(
            store.toggle_session_group_collapsed(&key, STARRED_WORKSPACE_ID),
            Some(true)
        );
        assert!(store.groups.iter().find(|g| g.id == sgid).unwrap().collapsed);
        assert!(!store.groups.iter().find(|g| g.id == gid).unwrap().collapsed);
    }

    #[test]
    fn waiting_unread_session_does_not_immediately_unarchive_until_new_activity() {
        let mut store = WorkspaceStore::new();
        let mut s_unread = make_session("s1", AgentKind::Claude, "Unread", 1000, true, false);
        s_unread.last_assistant_id = Some("asst-1".into());
        s_unread.last_assistant_ms = Some(1000);
        let key = canonical_session_key("h", AgentKind::Claude, "s1");

        // Primary Inbox workflow: archive unread session without marking read first
        store.archive_to_waiting(&key, 2000, 1000, Some("asst-1".into()));
        assert!(store.waiting_sessions.contains_key(&key));

        // Polling identical SessionSummary MUST NOT unarchive
        assert!(!store.auto_unarchive_waiting_if_active(&key, &s_unread));
        assert!(store.waiting_sessions.contains_key(&key));

        // New assistant reply wakes it
        s_unread.updated_at_ms = 2500;
        s_unread.last_assistant_id = Some("asst-2".into());
        s_unread.last_assistant_ms = Some(2500);
        assert!(store.auto_unarchive_waiting_if_active(&key, &s_unread));
        assert!(!store.waiting_sessions.contains_key(&key));

        // Read session archived via archive_to_waiting_session wakes if needs_attention flips true
        let mut s_read = make_session("s2", AgentKind::Claude, "Read", 1000, false, false);
        let key2 = canonical_session_key("h", AgentKind::Claude, "s2");
        store.archive_to_waiting_session(&key2, 2000, &s_read);
        assert!(!store.auto_unarchive_waiting_if_active(&key2, &s_read));
        s_read.needs_attention = true;
        assert!(store.auto_unarchive_waiting_if_active(&key2, &s_read));
    }

    #[test]
    fn is_session_unread_recovers_when_assistant_id_and_ms_are_none() {
        let mut store = WorkspaceStore::new();
        let mut s = SessionSummary {
            id: "s_min".into(),
            agent: AgentKind::Claude,
            title: "Minimal".into(),
            source_path: "/tmp/s_min.jsonl".into(),
            updated_at_ms: 1000,
            needs_attention: true,
            last_assistant_id: None,
            last_assistant_ms: None,
            ..Default::default()
        };
        let key = canonical_session_key("h", AgentKind::Claude, "s_min");

        assert!(store.is_session_unread(&key, &s));
        store.mark_session_read(&key, &s);
        assert!(!store.is_session_unread(&key, &s));

        // New activity arrives with updated_at_ms advanced (still None for assistant id/ms)
        s.updated_at_ms = 2000;
        s.needs_attention = true;
        assert!(store.is_session_unread(&key, &s));
    }

    #[test]
    fn reorder_session_preserves_auto_sort_on_noop_and_materializes_all_workspaces() {
        let mut store = WorkspaceStore::new();
        let ws_b = store.create_workspace("WorkB", None).unwrap();

        let a1 = make_session("a1", AgentKind::Claude, "A1", 100, false, false);
        let a2 = make_session("a2", AgentKind::Claude, "A2", 500, false, false);
        let b_muted = make_session("b_muted", AgentKind::Claude, "BMuted", 900, false, false);
        let b_old = make_session("b_old", AgentKind::Claude, "BOld", 200, false, false);
        let b_new = make_session("b_new", AgentKind::Claude, "BNew", 400, false, false);
        let all = [a1, a2, b_muted, b_old, b_new];
        let inputs: Vec<_> = all.iter().map(|s| make_input("h", s)).collect();

        let ka1 = inputs[0].key.clone();
        let ka2 = inputs[1].key.clone();
        let kb_muted = inputs[2].key.clone();
        let kb_old = inputs[3].key.clone();
        let kb_new = inputs[4].key.clone();

        store.assign_session_workspace(&kb_muted, &ws_b.id);
        store.assign_session_workspace(&kb_old, &ws_b.id);
        store.assign_session_workspace(&kb_new, &ws_b.id);
        store.toggle_session_muted(&kb_muted);

        // No-op move at top boundary MUST NOT flip auto_sort to false
        assert!(store.auto_sort);
        assert!(!store.reorder_session(
            DEFAULT_WORKSPACE_ID,
            &ka2,
            ReorderDirection::Up,
            &inputs,
            1000
        ));
        assert!(store.auto_sort);

        // No-op move for missing session MUST NOT flip auto_sort
        assert!(!store.reorder_session(
            DEFAULT_WORKSPACE_ID,
            "h:claude:missing",
            ReorderDirection::Down,
            &inputs,
            1000
        ));
        assert!(store.auto_sort);

        // Valid reorder in Workspace A disables auto_sort WITHOUT scrambling Workspace B
        assert!(store.reorder_session(
            DEFAULT_WORKSPACE_ID,
            &ka1,
            ReorderDirection::Up,
            &inputs,
            1000
        ));
        assert!(!store.auto_sort);

        let b_layout = store.compute_sidebar_layout(&ws_b.id, &inputs, false, 1000);
        let b_ids: Vec<&str> = b_layout
            .recents
            .iter()
            .map(|r| r.session_id.as_str())
            .collect();
        assert_eq!(b_ids, vec!["b_new", "b_old", "b_muted"]);
    }

    #[test]
    fn overflow_safety_in_cycle_workspace_id_and_create_group() {
        let mut store = WorkspaceStore::new();
        let work = store.create_workspace("Work", None).unwrap();

        let _ = store.cycle_workspace_id(&work.id, isize::MAX, 1000);
        let _ = store.cycle_workspace_id(&work.id, isize::MIN, 1000);

        store.groups.push(SessionGroup {
            id: "grp-max".into(),
            name: "Max".into(),
            color: None,
            collapsed: false,
            workspace_id: DEFAULT_WORKSPACE_ID.into(),
            position: usize::MAX,
        });
        let created = store.create_group(DEFAULT_WORKSPACE_ID, "Next", None);
        assert_eq!(created.position, usize::MAX);
    }

    #[test]
    fn inbox_unread_count_excludes_snoozed_and_archived_workspaces() {
        let mut store = WorkspaceStore::new();
        let ws_snz = store.create_workspace("Snoozed", None).unwrap();
        let ws_arch = store.create_workspace("Archived", None).unwrap();
        let now = 1_000;
        store.snooze_workspace(&ws_snz.id, 5_000, now).unwrap();
        store.archive_workspace(&ws_arch.id).unwrap();

        let s_snz = make_session("s_snz", AgentKind::Claude, "S", 100, true, false);
        let s_arch = make_session("s_arch", AgentKind::Claude, "A", 100, true, false);
        let s_ok = make_session("s_ok", AgentKind::Claude, "OK", 100, true, false);
        let all = [s_snz, s_arch, s_ok];
        let inputs: Vec<_> = all.iter().map(|s| make_input("h", s)).collect();
        store.assign_session_workspace(&inputs[0].key, &ws_snz.id);
        store.assign_session_workspace(&inputs[1].key, &ws_arch.id);

        assert_eq!(store.query_inbox(&InboxSplit::All, &inputs, false, "", now).len(), 1);
        assert_eq!(store.inbox_unread_count(&inputs), 1);
        assert_eq!(store.inbox_unread_count_at(&inputs, now), 1);
        // Once snooze expires at now == 5_000, inbox_unread_count_at includes s_snz again
        assert_eq!(store.inbox_unread_count_at(&inputs, 5_000), 2);
    }

    #[test]
    fn delete_or_archive_last_active_workspace_always_keeps_visible_workspace() {
        let mut store = WorkspaceStore::new();
        let active = store.create_workspace("Active", None).unwrap();
        store.archive_workspace(DEFAULT_WORKSPACE_ID).unwrap();
        let outcome = store.delete_workspace(&active.id, None).unwrap();
        assert!(outcome.created_default_general);
        assert!(!store.visible_workspaces(1000).is_empty());
        assert_eq!(store.default_workspace_id(), DEFAULT_WORKSPACE_ID);
        assert!(!store.workspace_by_id(DEFAULT_WORKSPACE_ID).unwrap().archived);

        // Archiving the last active workspace also keeps a non-archived workspace alive
        store.archive_workspace(DEFAULT_WORKSPACE_ID).unwrap();
        assert!(!store.visible_workspaces(1000).is_empty());
    }

    #[test]
    fn query_inbox_preserves_json_array_order_when_positions_default_zero() {
        let json = r#"{"workspaces":[{"id":"w_zebra","name":"Zebra"},{"id":"w_alpha","name":"Alpha"}]}"#;
        let mut store: WorkspaceStore = serde_json::from_str(json).unwrap();

        let s_z = make_session("z_sess", AgentKind::Claude, "Z", 1000, false, false);
        let s_a = make_session("a_sess", AgentKind::Claude, "A", 1000, false, false);
        let all = [s_a, s_z];
        let inputs: Vec<_> = all.iter().map(|s| make_input("h", s)).collect();
        store.assign_session_workspace(&inputs[0].key, "w_alpha");
        store.assign_session_workspace(&inputs[1].key, "w_zebra");

        let rows = store.query_inbox(&InboxSplit::All, &inputs, false, "", 2000);
        let ids: Vec<&str> = rows.iter().map(|r| r.session_id.as_str()).collect();
        assert_eq!(ids, vec!["z_sess", "a_sess"]);
    }

    #[test]
    fn ctrl_digit_order_skips_collapsed_sections_and_palette_matches_non_alnum_word_boundary() {
        let mut store = WorkspaceStore::new();
        let s1 = make_session("s1", AgentKind::Claude, "Pinned", 100, false, false);
        let s2 = make_session("s2", AgentKind::Claude, "Recent", 200, false, false);
        let all = [s1, s2];
        let inputs: Vec<_> = all.iter().map(|s| make_input("h", s)).collect();
        store.toggle_pin(&inputs[0].key, false);

        store.pinned_section_collapsed = true;
        let layout1 = store.compute_sidebar_layout(DEFAULT_WORKSPACE_ID, &inputs, false, 1000);
        assert_eq!(layout1.ctrl_digit_order, vec![inputs[1].key.clone()]);

        store.pinned_section_collapsed = false;
        store.recents_section_collapsed = true;
        let layout2 = store.compute_sidebar_layout(DEFAULT_WORKSPACE_ID, &inputs, false, 1000);
        assert_eq!(layout2.ctrl_digit_order, vec![inputs[0].key.clone()]);

        // Word-boundary match for non-alphanumeric prefixes after separator
        assert_eq!(palette_text_match_rank("Chat View: /rename", "/rename"), 2);
        assert_eq!(palette_text_match_rank("Inbox ★ Favorites", "★"), 2);
        assert_eq!(palette_text_match_rank("foo/bar", "/bar"), 1);
    }

    #[test]
    fn waiting_unread_session_excluded_from_unread_counts_until_woken() {
        let mut store = WorkspaceStore::new();
        let mut s_wait = make_session("s_wait", AgentKind::Claude, "Waiting", 1_000, true, false);
        s_wait.last_assistant_id = Some("a1".into());
        s_wait.last_assistant_ms = Some(1_000);
        let key = canonical_session_key("h", AgentKind::Claude, "s_wait");

        let inputs = [make_input("h", &s_wait)];
        assert_eq!(store.inbox_unread_count(&inputs), 1);
        assert_eq!(store.inbox_unread_count_at(&inputs, 2_000), 1);

        // Archiving unread session to Waiting clears phantom unread badge while hidden
        store.archive_to_waiting_session(&key, 2_000, &s_wait);
        assert_eq!(
            store
                .query_inbox(&InboxSplit::All, &inputs, true, "", 2_000)
                .len(),
            0
        );
        assert_eq!(store.inbox_unread_count(&inputs), 0);
        assert_eq!(store.inbox_unread_count_at(&inputs, 2_000), 0);

        // Once woken by new activity, unread badge returns to 1
        s_wait.updated_at_ms = 3_000;
        s_wait.last_assistant_id = Some("a2".into());
        s_wait.last_assistant_ms = Some(3_000);
        assert!(store.auto_unarchive_waiting_if_active(&key, &s_wait));
        let inputs_woken = [make_input("h", &s_wait)];
        assert_eq!(store.inbox_unread_count(&inputs_woken), 1);
        assert_eq!(store.inbox_unread_count_at(&inputs_woken, 3_000), 1);
    }

    #[test]
    fn archiving_or_deleting_general_while_other_workspace_snoozed_keeps_active_default() {
        // Case 1: Archive general while another workspace is snoozed
        let mut store = WorkspaceStore::new();
        let snz = store.create_workspace("Snoozed", None).unwrap();
        store.snooze_workspace(&snz.id, 100_000, 1_000).unwrap();

        let unassigned = make_session("u1", AgentKind::Claude, "Unassigned", 1_000, true, false);
        let inputs = [make_input("h", &unassigned)];

        store.archive_workspace(DEFAULT_WORKSPACE_ID).unwrap();
        assert!(!store.visible_workspaces(1_000).is_empty());
        let def_id = store.default_workspace_id();
        assert_eq!(def_id, DEFAULT_WORKSPACE_ID);
        let def_ws = store.workspace_by_id(&def_id).unwrap();
        assert!(!def_ws.archived && !def_ws.snoozed);
        assert_eq!(
            store
                .query_inbox(&InboxSplit::All, &inputs, false, "", 1_000)
                .len(),
            1
        );

        // Case 2: Delete general while another workspace is snoozed
        let mut store2 = WorkspaceStore::new();
        let snz2 = store2.create_workspace("Snoozed", None).unwrap();
        store2.snooze_workspace(&snz2.id, 100_000, 1_000).unwrap();

        let outcome = store2.delete_workspace(DEFAULT_WORKSPACE_ID, None).unwrap();
        assert!(outcome.created_default_general);
        assert!(!store2.visible_workspaces(1_000).is_empty());
        let def_id2 = store2.default_workspace_id();
        assert_eq!(def_id2, DEFAULT_WORKSPACE_ID);
        let def_ws2 = store2.workspace_by_id(&def_id2).unwrap();
        assert!(!def_ws2.archived && !def_ws2.snoozed);
        assert_eq!(
            store2
                .query_inbox(&InboxSplit::All, &inputs, false, "", 1_000)
                .len(),
            1
        );
    }

    #[test]
    fn undo_last_waiting_skips_stale_already_auto_unarchived_entries() {
        let mut store = WorkspaceStore::new();
        let s1 = make_session("s1", AgentKind::Claude, "First", 1_000, true, false);
        let mut s2 = make_session("s2", AgentKind::Claude, "Second", 1_000, true, false);
        let k1 = canonical_session_key("h", AgentKind::Claude, "s1");
        let k2 = canonical_session_key("h", AgentKind::Claude, "s2");

        store.archive_to_waiting_session(&k1, 2_000, &s1);
        store.archive_to_waiting_session(&k2, 2_001, &s2);

        // s2 wakes automatically via new activity, leaving a stale top entry on waiting_undo_stack
        s2.updated_at_ms = 3_000;
        s2.last_assistant_id = Some("asst-s2-new".into());
        s2.last_assistant_ms = Some(3_000);
        assert!(store.auto_unarchive_waiting_if_active(&k2, &s2));
        assert!(!store.waiting_sessions.contains_key(&k2));
        assert!(store.waiting_sessions.contains_key(&k1));

        // undo_last_waiting skips stale k2 and restores still-waiting k1
        assert_eq!(store.undo_last_waiting(), Some(k1.clone()));
        assert!(!store.waiting_sessions.contains_key(&k1));
        assert_eq!(store.undo_last_waiting(), None);
    }
}
