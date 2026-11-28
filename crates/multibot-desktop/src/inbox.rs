use crate::app::{CloseOverlay, InboxNextSplit, InboxPrevSplit, MultibotView};
use crate::icons::{self, lucide_icon};
use crate::sidebar::{InlineRenameState, RenameKeyOutcome};
use crate::state::{ActiveModal, MainSurface};
use crate::theme::{self, RADIUS_BUBBLE, RADIUS_CHIP, RADIUS_ROW};
use gpui_kit::{
    Context, FocusHandle, FontWeight, InteractiveElement, IntoElement, KeyDownEvent, Modifiers,
    MouseButton, ParentElement, Role, SharedString, StatefulInteractiveElement, Styled,
    TestSupportExt, Window,
    component::{h_flex, v_flex},
    div,
    prelude::FluentBuilder,
    px,
};
use multibot_core::workspaces::{
    InboxRow, InboxSplit, ReorderDirection, SessionLifecycle, now_unix_ms,
};

const DOUBLE_G_WINDOW_MS: i64 = 600;

#[derive(Debug, Clone, Default)]
pub(crate) struct InboxState {
    pub active_split: InboxSplit,
    pub lifecycle: SessionLifecycle,
    pub selected_row: usize,
    pub filter_query: String,
    pub filter_focused: bool,
    /// The Inbox search bar starts hidden and opens on `Ctrl+F` / `/`.
    pub inbox_search_open: bool,
    pub last_g_press_ms: Option<i64>,
    pub step_order: Vec<String>,
    pub step_index: Option<usize>,
    pub rename: Option<InlineRenameState>,
    pub focus_handle: Option<FocusHandle>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InboxKeyOutcome {
    Ignored,
    Handled,
    OpenSession(String),
    CloseInbox,
    MarkWaiting(String),
    UndoWaiting,
    Hibernate(String),
    ToggleUnread(String),
    ToggleUnreadOnly,
    StartRename(String),
    CommitRename { session_key: String, draft: String },
    CancelRename,
}

impl InboxState {
    /// `true` when the Inbox search bar is open, focused, or holding a query.
    pub(crate) fn is_search_visible(&self) -> bool {
        self.inbox_search_open || self.filter_focused || !self.filter_query.is_empty()
    }

    /// Opens the Inbox search bar and focuses it (`Ctrl+F` / `/`).
    pub(crate) fn open_search(&mut self) {
        self.inbox_search_open = true;
        self.filter_focused = true;
        self.last_g_press_ms = None;
    }

    /// Hides the Inbox search bar, clears the query and restores the full Inbox list.
    /// Returns `true` when the search bar was open or holding a query.
    pub(crate) fn close_search(&mut self) -> bool {
        let was_visible = self.is_search_visible();
        self.inbox_search_open = false;
        self.filter_focused = false;
        if !self.filter_query.is_empty() {
            self.filter_query.clear();
            self.selected_row = 0;
        }
        was_visible
    }

    /// Clamps `active_split` onto the visible (non-empty) split bar.
    ///
    /// A split that dropped to `0` sessions is hidden from the bar, so the Inbox would sit on
    /// a dead-end tab. Fall back to `All` (or the first visible split) instead.
    /// Returns `true` when `active_split` changed.
    pub(crate) fn normalize_active_split(&mut self, visible_splits: &[InboxSplit]) -> bool {
        if visible_splits.is_empty() || visible_splits.contains(&self.active_split) {
            return false;
        }
        self.active_split = if visible_splits.contains(&InboxSplit::All) {
            InboxSplit::All
        } else {
            visible_splits[0].clone()
        };
        self.selected_row = 0;
        self.last_g_press_ms = None;
        true
    }

    pub(crate) fn select_all_split(&mut self) {
        self.active_split = InboxSplit::All;
        self.selected_row = 0;
        self.last_g_press_ms = None;
        self.rename = None;
    }

    pub(crate) fn handle_key(
        &mut self,
        key: &str,
        key_char: Option<&str>,
        modifiers: Modifiers,
        now_ms: i64,
        visible_splits: &[InboxSplit],
        visible_session_keys: &[String],
    ) -> InboxKeyOutcome {
        // 1. Inline F2 rename takes top priority
        if let Some(ref mut rename) = self.rename {
            match rename.handle_key(key, key_char, modifiers) {
                RenameKeyOutcome::Consumed => return InboxKeyOutcome::Handled,
                RenameKeyOutcome::Commit => {
                    let session_key = rename.session_key.clone();
                    let draft = rename.draft.clone();
                    self.rename = None;
                    return InboxKeyOutcome::CommitRename { session_key, draft };
                }
                RenameKeyOutcome::Cancel => {
                    self.rename = None;
                    return InboxKeyOutcome::CancelRename;
                }
                RenameKeyOutcome::Ignored => return InboxKeyOutcome::Ignored,
            }
        }

        let max_idx = visible_session_keys.len().saturating_sub(1);
        let key_low = key.to_ascii_lowercase();

        // 2. Live filter bar editing mode
        if self.filter_focused {
            self.last_g_press_ms = None;
            if key_low == "escape" {
                // Esc closes the search bar and restores the full Inbox list; Inbox stays open
                self.close_search();
                return InboxKeyOutcome::Handled;
            }
            if key_low == "enter" {
                self.filter_focused = false;
                return InboxKeyOutcome::Handled;
            }
            if key_low == "backspace" {
                self.filter_query.pop();
                self.selected_row = 0;
                return InboxKeyOutcome::Handled;
            }
            if key_low == "down" {
                if !visible_session_keys.is_empty() {
                    self.selected_row = self
                        .selected_row
                        .min(max_idx)
                        .saturating_add(1)
                        .min(max_idx);
                }
                return InboxKeyOutcome::Handled;
            }
            if key_low == "up" {
                self.selected_row = self.selected_row.min(max_idx).saturating_sub(1);
                return InboxKeyOutcome::Handled;
            }
            if !modifiers.control && !modifiers.alt && !modifiers.platform {
                if let Some(cs) = key_char {
                    let mut added = false;
                    for ch in cs.chars() {
                        if !ch.is_control() {
                            self.filter_query.push(ch);
                            added = true;
                        }
                    }
                    if added {
                        self.selected_row = 0;
                        return InboxKeyOutcome::Handled;
                    }
                } else if key.chars().count() == 1 {
                    let ch = key.chars().next().unwrap();
                    if !ch.is_control() {
                        self.filter_query.push(if modifiers.shift {
                            ch.to_ascii_uppercase()
                        } else {
                            ch
                        });
                        self.selected_row = 0;
                        return InboxKeyOutcome::Handled;
                    }
                }
            }
            return InboxKeyOutcome::Ignored;
        }

        let no_mods =
            !modifiers.control && !modifiers.alt && !modifiers.shift && !modifiers.platform;
        let is_plain_g = no_mods && key_low == "g";
        if !is_plain_g {
            self.last_g_press_ms = None;
        }

        // Ctrl+Shift+U -> global unread-only filter
        if modifiers.control && modifiers.shift && !modifiers.alt && key_low == "u" {
            return InboxKeyOutcome::ToggleUnreadOnly;
        }

        // Ctrl+F or `/` -> open and focus the search bar
        if (modifiers.control && !modifiers.shift && !modifiers.alt && key_low == "f")
            || (no_mods && (key == "/" || key_char == Some("/")))
        {
            self.open_search();
            return InboxKeyOutcome::Handled;
        }

        // Alt+I -> jump to All split
        if modifiers.alt && !modifiers.control && key_low == "i" {
            self.select_all_split();
            return InboxKeyOutcome::Handled;
        }

        // Down: j / down / Alt+J (clamped)
        if (no_mods && (key_low == "j" || key_low == "down"))
            || (modifiers.alt && !modifiers.control && key_low == "j")
        {
            if !visible_session_keys.is_empty() {
                self.selected_row = self
                    .selected_row
                    .min(max_idx)
                    .saturating_add(1)
                    .min(max_idx);
            }
            return InboxKeyOutcome::Handled;
        }

        // Up: k / up / Alt+K (clamped before decrementing)
        if (no_mods && (key_low == "k" || key_low == "up"))
            || (modifiers.alt && !modifiers.control && key_low == "k")
        {
            self.selected_row = self.selected_row.min(max_idx).saturating_sub(1);
            return InboxKeyOutcome::Handled;
        }

        // `gg` within 600ms -> top row (reject backward clock regressions)
        if is_plain_g {
            if let Some(prev) = self.last_g_press_ms
                && now_ms >= prev
                && now_ms.saturating_sub(prev) <= DOUBLE_G_WINDOW_MS
            {
                self.selected_row = 0;
                self.last_g_press_ms = None;
            } else {
                self.last_g_press_ms = Some(now_ms);
            }
            return InboxKeyOutcome::Handled;
        }

        // `G` -> bottom row
        if (!modifiers.control && !modifiers.alt && modifiers.shift && key_low == "g")
            || (!modifiers.control && !modifiers.alt && key_char == Some("G"))
        {
            self.selected_row = max_idx;
            return InboxKeyOutcome::Handled;
        }

        // `h` / `left` -> previous split (CLAMPED; recovers to visible_splits[0] if vanished)
        if no_mods && key_low == "left" {
            if let Some(idx) = visible_splits.iter().position(|s| *s == self.active_split) {
                if idx > 0 {
                    self.active_split = visible_splits[idx - 1].clone();
                    self.selected_row = 0;
                }
            } else if let Some(first) = visible_splits.first() {
                self.active_split = first.clone();
                self.selected_row = 0;
            }
            return InboxKeyOutcome::Handled;
        }

        // `l` / `right` -> next split (CLAMPED; recovers to visible_splits[0] if vanished)
        if no_mods && key_low == "right" {
            if let Some(idx) = visible_splits.iter().position(|s| *s == self.active_split) {
                if idx + 1 < visible_splits.len() {
                    self.active_split = visible_splits[idx + 1].clone();
                    self.selected_row = 0;
                }
            } else if let Some(first) = visible_splits.first() {
                self.active_split = first.clone();
                self.selected_row = 0;
            }
            return InboxKeyOutcome::Handled;
        }

        // `Tab` / `Shift+Tab` -> cycle splits (WRAPPED)
        if !modifiers.control && !modifiers.alt && key_low == "tab" {
            if !visible_splits.is_empty() {
                let len = visible_splits.len();
                let cur = visible_splits
                    .iter()
                    .position(|s| *s == self.active_split)
                    .unwrap_or(0);
                let next = if modifiers.shift {
                    (cur + len - 1) % len
                } else {
                    (cur + 1) % len
                };
                self.active_split = visible_splits[next].clone();
                self.selected_row = 0;
            }
            return InboxKeyOutcome::Handled;
        }

        // `1..=9` -> direct row jump
        if no_mods
            && key.len() == 1
            && let Some(d) = key.chars().next().and_then(|c| c.to_digit(10))
            && (1..=9).contains(&d)
        {
            let target = (d - 1) as usize;
            if target < visible_session_keys.len() {
                self.selected_row = target;
            }
            return InboxKeyOutcome::Handled;
        }

        // `Enter` -> open selected row and arm Alt+J/K stepping
        if no_mods && key_low == "enter" {
            if !visible_session_keys.is_empty() {
                let idx = self.selected_row.min(max_idx);
                self.step_order = visible_session_keys.to_vec();
                self.step_index = Some(idx);
                return InboxKeyOutcome::OpenSession(visible_session_keys[idx].clone());
            }
            return InboxKeyOutcome::Handled;
        }

        // `Esc` closes the search bar first when it is open; then it closes Inbox
        if no_mods && key_low == "escape" && self.is_search_visible() {
            self.close_search();
            return InboxKeyOutcome::Handled;
        }

        // `Esc` / `q` -> close Inbox
        if no_mods && (key_low == "escape" || key_low == "q") {
            return InboxKeyOutcome::CloseInbox;
        }

        // `e` / `Alt+E` -> move selected to Waiting
        if (no_mods && key_low == "e") || (modifiers.alt && !modifiers.control && key_low == "e") {
            if !visible_session_keys.is_empty() {
                let idx = self.selected_row.min(max_idx);
                return InboxKeyOutcome::MarkWaiting(visible_session_keys[idx].clone());
            }
            return InboxKeyOutcome::Handled;
        }

        if no_mods && key_low == "h" {
            return visible_session_keys
                .get(self.selected_row.min(max_idx))
                .map(|key| InboxKeyOutcome::Hibernate(key.clone()))
                .unwrap_or(InboxKeyOutcome::Handled);
        }

        // `z` / `Alt+Z` -> undo last Waiting
        if (no_mods && key_low == "z") || (modifiers.alt && !modifiers.control && key_low == "z") {
            return InboxKeyOutcome::UndoWaiting;
        }

        // `u` / `Alt+U` -> toggle read/unread
        if (no_mods && key_low == "u") || (modifiers.alt && !modifiers.control && key_low == "u") {
            if !visible_session_keys.is_empty() {
                let idx = self.selected_row.min(max_idx);
                return InboxKeyOutcome::ToggleUnread(visible_session_keys[idx].clone());
            }
            return InboxKeyOutcome::Handled;
        }

        // `F2` -> inline rename selected row
        if no_mods && key_low == "f2" {
            if !visible_session_keys.is_empty() {
                let idx = self.selected_row.min(max_idx);
                return InboxKeyOutcome::StartRename(visible_session_keys[idx].clone());
            }
            return InboxKeyOutcome::Handled;
        }

        InboxKeyOutcome::Ignored
    }
}

pub(crate) fn format_relative_age(timestamp_ms: i64, now_ms: i64) -> String {
    if timestamp_ms <= 0 || now_ms <= timestamp_ms {
        return "now".to_string();
    }
    let diff_secs = (now_ms - timestamp_ms) / 1000;
    if diff_secs < 60 {
        "now".to_string()
    } else if diff_secs < 3600 {
        format!("{}m", diff_secs / 60)
    } else if diff_secs < 86_400 {
        format!("{}h", diff_secs / 3600)
    } else {
        format!("{}d", diff_secs / 86_400)
    }
}

impl MultibotView {
    pub(crate) fn open_lifecycle_view(
        &mut self,
        lifecycle: SessionLifecycle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.table_modal_state.close();
        self.active_modal = ActiveModal::None;
        self.sidebar_state.rename = None;
        self.sidebar_state.new_workspace_draft = None;
        self.inbox_state.rename = None;
        self.inbox_state.lifecycle = lifecycle;
        self.inbox_state.selected_row = 0;
        self.set_main_surface(MainSurface::Inbox);
        self.focus_inbox(window, cx);
        cx.notify();
    }

    pub(crate) fn mark_done_or_inbox(&mut self, key: &str, cx: &mut Context<Self>) {
        if let Some(session) = self.find_session_by_key(key) {
            let advance = self.main_surface == MainSurface::Chat
                && self
                    .selection
                    .selected_session
                    .as_ref()
                    .is_some_and(|s| self.session_key_for(s) == key);
            let next = advance
                .then(|| self.sidebar_neighbor_after_removal(key, ReorderDirection::Down))
                .flatten();
            let store = &mut self.sidebar_state.workspace_store;
            match store.session_lifecycle(key, &session, now_unix_ms()) {
                SessionLifecycle::Done => {
                    if self.main_surface == MainSurface::Inbox
                        && self.inbox_state.lifecycle == SessionLifecycle::Done
                    {
                        self.reactivate_session_key(key, cx);
                    }
                    return;
                }
                SessionLifecycle::Active | SessionLifecycle::Hibernated => {
                    store.archive_to_waiting_session(key, now_unix_ms(), &session)
                }
                SessionLifecycle::Dissolved => {
                    return;
                }
            }
            if advance {
                if let Some((host, session)) = next.and_then(|key| self.host_for_session_key(&key))
                {
                    self.select_session_on_host(host, session, cx);
                } else {
                    self.clear_active_session_view(cx);
                }
            }
            self.cleanup_unmounted_surface_state();
            self.persist_workspace_store_if_enabled();
            self.sync_workspace_to_shared(cx);
            cx.notify();
        }
    }

    pub(crate) fn workspace_live_session_keys(&self, id: &str) -> Vec<String> {
        let store = &self.sidebar_state.workspace_store;
        self.session_inputs()
            .into_iter()
            .filter(|input| {
                store.effective_workspace_id(&input.key) == id
                    && store.session_lifecycle(&input.key, input.summary, now_unix_ms())
                        != SessionLifecycle::Dissolved
            })
            .map(|input| input.key)
            .collect()
    }

    pub(crate) fn dissolve_workspace(&mut self, id: &str, cx: &mut Context<Self>) {
        if self
            .sidebar_state
            .workspace_store
            .workspace_by_id(id)
            .is_none()
        {
            return;
        }
        let session_keys: Vec<_> = self
            .session_inputs()
            .into_iter()
            .filter(|input| {
                self.sidebar_state
                    .workspace_store
                    .effective_workspace_id(&input.key)
                    == id
            })
            .map(|input| input.key)
            .collect();
        let keys = self.workspace_live_session_keys(id);
        if keys.iter().any(|key| {
            self.host_for_session_key(key)
                .and_then(|(host, _)| self.tx_for_host(&host))
                .is_none_or(|tx| tx.is_closed())
        }) {
            self.notice =
                Some("Reconnect workspace servers before dissolving their live agents.".into());
            cx.notify();
            return;
        }
        for key in &keys {
            // The workspace dialog already confirmed stopping all its agents, including shells.
            self.dissolve_session_key_confirmed(key, cx);
            if self.find_session_by_key(key).is_none_or(|session| {
                self.sidebar_state
                    .workspace_store
                    .session_lifecycle(key, &session, now_unix_ms())
                    != SessionLifecycle::Dissolved
            }) {
                self.notice = Some(
                    "Could not stop every agent. Reconnect and dissolve the workspace again."
                        .into(),
                );
                cx.notify();
                return;
            }
        }
        if self
            .sidebar_state
            .workspace_store
            .dissolve_workspace(id, &session_keys, now_unix_ms())
            .is_ok()
        {
            self.sidebar_state.active_workspace_id =
                self.sidebar_state.workspace_store.default_workspace_id();
            self.sidebar_state.rename = None;
            self.cleanup_unmounted_surface_state();
            self.persist_workspace_store_if_enabled();
            self.sync_workspace_to_shared(cx);
        }
        cx.notify();
    }

    pub(crate) fn dissolve_session_key(&mut self, key: &str, cx: &mut Context<Self>) {
        if let Some(session) = self.find_session_by_key(key)
            && session.agent == multibot_proto::AgentKind::Shell
            && !session.archived
            && !self
                .sidebar_state
                .confirm_shell_dissolve(key, std::time::Instant::now())
        {
            let title = self
                .sidebar_state
                .workspace_store
                .effective_title(key, &session.title);
            self.notice = Some(format!(
                "Press Ctrl+Shift+W again to kill live server shell \"{title}\"."
            ));
            cx.notify();
            return;
        }
        self.sidebar_state.pending_shell_dissolve = None;
        self.dissolve_session_key_confirmed(key, cx);
    }

    pub(crate) fn dissolve_session_key_confirmed(&mut self, key: &str, cx: &mut Context<Self>) {
        self.sidebar_state.pending_shell_dissolve = None;
        let Some((host, session)) = self.host_for_session_key(key) else {
            return;
        };
        if self
            .sidebar_state
            .workspace_store
            .session_lifecycle(key, &session, now_unix_ms())
            == SessionLifecycle::Dissolved
            && session.agent != multibot_proto::AgentKind::Shell
        {
            return;
        }
        let Some(tx) = self.tx_for_host(&host).cloned() else {
            return;
        };
        if session.agent == multibot_proto::AgentKind::Shell {
            self.shell_host.close_session(&host, &session.id);
        }
        if tx
            .send(multibot_proto::ClientRequest::ArchiveSession {
                session_id: session.id.clone(),
                agent: session.agent,
                source_path: session.source_path.clone(),
                kill_process: true,
            })
            .is_err()
        {
            return;
        }
        let next = self.sidebar_neighbor_after_removal(key, ReorderDirection::Up);
        self.notice = None;
        self.selection
            .closed_sessions
            .push((host.clone(), session.clone()));
        self.sidebar_state
            .workspace_store
            .dissolve_session(key, now_unix_ms());
        self.set_session_archived_local(&host, &session, true);
        if self
            .selection
            .selected_session
            .as_ref()
            .is_some_and(|s| self.session_key_for(s) == key)
        {
            if let Some((host, next)) = next.and_then(|key| self.host_for_session_key(&key)) {
                self.select_session_on_host(host, next, cx);
            } else {
                self.clear_active_session_view(cx);
            }
        }
        self.cleanup_unmounted_surface_state();
        self.persist_workspace_store_if_enabled();
        self.sync_sessions_to_shared(cx);
        self.sync_workspace_to_shared(cx);
        cx.notify();
    }

    fn set_session_archived_local(
        &mut self,
        host: &str,
        session: &multibot_proto::SessionSummary,
        archived: bool,
    ) {
        for (h, s) in &mut self.connection.host_sessions {
            if h == host && s.id == session.id && s.agent == session.agent {
                s.archived = archived;
                s.archive_kill_process = archived;
                if archived && s.agent == multibot_proto::AgentKind::Shell {
                    s.shell_attached = None;
                }
                s.working = false;
            }
        }
        if self.connection.host_statuses.is_empty() {
            for s in &mut self.connection.sessions {
                if s.id == session.id && s.agent == session.agent {
                    s.archived = archived;
                    s.archive_kill_process = archived;
                    if archived && s.agent == multibot_proto::AgentKind::Shell {
                        s.shell_attached = None;
                    }
                    s.working = false;
                }
            }
        } else {
            self.connection.sessions = self
                .connection
                .host_sessions
                .iter()
                .map(|(_, s)| s.clone())
                .collect();
        }
        if self
            .selection
            .selected_session
            .as_ref()
            .is_some_and(|s| s.id == session.id && s.agent == session.agent)
            && self.active_session_host() == host
        {
            if let Some(s) = &mut self.selection.selected_session {
                s.archived = archived;
                s.archive_kill_process = archived;
                if archived && s.agent == multibot_proto::AgentKind::Shell {
                    s.shell_attached = None;
                }
                s.working = false;
            }
        }
    }

    pub(crate) fn reactivate_session_key(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        let Some((host, session)) = self.host_for_session_key(key) else {
            return false;
        };
        if session.archived
            || self
                .sidebar_state
                .workspace_store
                .session_lifecycle(key, &session, now_unix_ms())
                == SessionLifecycle::Dissolved
        {
            if !self.unarchive_session_on_host(&host, session.id.clone(), cx) {
                return false;
            }
            self.set_session_archived_local(&host, &session, false);
        }
        let workspace_id = self
            .sidebar_state
            .workspace_store
            .effective_workspace_id(key);
        let _ = self
            .sidebar_state
            .workspace_store
            .restore_workspace(&workspace_id);
        self.sidebar_state
            .workspace_store
            .wake_workspace(&workspace_id);
        self.sidebar_state.workspace_store.move_to_inbox(key);
        self.sidebar_state
            .workspace_store
            .record_session_input(key, now_unix_ms());
        self.selection
            .closed_sessions
            .retain(|(h, s)| h != &host || s.id != session.id || s.agent != session.agent);
        self.persist_workspace_store_if_enabled();
        self.sync_sessions_to_shared(cx);
        self.sync_workspace_to_shared(cx);
        cx.notify();
        true
    }

    pub(crate) fn reactivate_last_dissolved(&mut self, cx: &mut Context<Self>) {
        let key = self
            .selection
            .closed_sessions
            .iter()
            .rev()
            .find_map(|(h, s)| {
                let key = multibot_core::workspaces::canonical_session_key(h, s.agent, &s.id);
                self.find_session_by_key(&key)
                    .filter(|s| {
                        self.sidebar_state
                            .workspace_store
                            .session_lifecycle(&key, s, now_unix_ms())
                            == SessionLifecycle::Dissolved
                    })
                    .map(|_| key)
            })
            .or_else(|| {
                self.session_inputs()
                    .into_iter()
                    .filter(|input| {
                        self.sidebar_state.workspace_store.session_lifecycle(
                            &input.key,
                            input.summary,
                            now_unix_ms(),
                        ) == SessionLifecycle::Dissolved
                    })
                    .max_by_key(|input| {
                        self.sidebar_state
                            .workspace_store
                            .session_meta
                            .get(&input.key)
                            .and_then(|m| m.dissolved_at_ms)
                            .unwrap_or(input.summary.updated_at_ms)
                    })
                    .map(|input| input.key.clone())
            });
        if let Some(key) = key {
            if self.reactivate_session_key(&key, cx) {
                if let Some((host, session)) = self.host_for_session_key(&key) {
                    self.open_session_on_host(host, session, false, cx);
                }
            }
        }
    }

    pub(crate) fn handle_lifecycle_keydown(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let key = ev.keystroke.key.to_ascii_lowercase();
        let m = ev.keystroke.modifiers;
        if m.alt && m.shift && !m.control && !m.platform {
            let Some(lifecycle) =
                SessionLifecycle::go_key(&key).filter(|v| *v != SessionLifecycle::Active)
            else {
                return false;
            };
            self.open_lifecycle_view(lifecycle, window, cx);
        } else if self.active_modal != ActiveModal::None || self.table_modal_state.is_open() {
            return false;
        } else if m.alt && !m.shift && !m.control && !m.platform && key == "i" {
            self.inbox_state.select_all_split();
            self.open_lifecycle_view(SessionLifecycle::Active, window, cx);
        } else if (m.alt && !m.control && !m.shift && !m.platform && key == "e")
            || (m.control && m.shift && !m.alt && key == "e")
        {
            if let Some(key) = self.target_triage_session_key() {
                self.mark_done_or_inbox(&key, cx);
            }
        } else if ((m.alt && !m.control) || (m.control && !m.alt))
            && !m.shift
            && !m.platform
            && key == "h"
        {
            if let Some(key) = self.target_triage_session_key() {
                self.open_reminder_picker(crate::palette::ReminderTarget::Session(key), window, cx);
            }
        } else if m.alt && !m.control && !m.shift && !m.platform && key == "z" {
            if let Some(key) = self.sidebar_state.workspace_store.undo_last_waiting() {
                self.reactivate_session_key(&key, cx);
            }
            self.persist_workspace_store_if_enabled();
            self.sync_workspace_to_shared(cx);
            cx.notify();
        } else if m.control && m.shift && !m.alt && !m.platform && key == "w" {
            if let Some(key) = self.target_triage_session_key() {
                self.dissolve_session_key(&key, cx);
            }
        } else if m.control && m.shift && !m.alt && !m.platform && key == "t" {
            self.reactivate_last_dissolved(cx);
        } else {
            return false;
        }
        cx.stop_propagation();
        window.prevent_default();
        true
    }

    pub(crate) fn compute_inbox_rows(&self, now_ms: i64) -> Vec<InboxRow> {
        self.compute_inbox_rows_for(&self.inbox_state.active_split, now_ms)
    }

    pub(crate) fn compute_inbox_rows_for(&self, split: &InboxSplit, now_ms: i64) -> Vec<InboxRow> {
        let inputs = self.session_inputs();
        self.sidebar_state.workspace_store.query_lifecycle(
            self.inbox_state.lifecycle,
            split,
            &inputs,
            self.sidebar_state.unread_only,
            &self.inbox_state.filter_query,
            now_ms,
        )
    }

    /// The Inbox split bar and `Tab` / `Shift+Tab` cycle order: non-empty splits only, with
    /// `All` always present. The Sidebar workspace switcher and Settings keep their own
    /// unfiltered workspace lists.
    pub(crate) fn inbox_visible_splits(&self, now_ms: i64) -> Vec<InboxSplit> {
        self.sidebar_state
            .workspace_store
            .lifecycle_inbox_splits(self.inbox_state.lifecycle, now_ms)
            .into_iter()
            .filter(|split| {
                *split == InboxSplit::All || !self.compute_inbox_rows_for(split, now_ms).is_empty()
            })
            .collect()
    }

    pub(crate) fn ensure_inbox_focus_handle(&mut self, cx: &mut Context<Self>) -> FocusHandle {
        if let Some(ref fh) = self.inbox_state.focus_handle {
            return fh.clone();
        }
        let fh = cx.focus_handle();
        self.inbox_state.focus_handle = Some(fh.clone());
        fh
    }

    pub(crate) fn focus_inbox(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let fh = self.ensure_inbox_focus_handle(cx);
        window.focus(&fh, cx);
    }

    pub(crate) fn cycle_inbox_split(&mut self, reverse: bool, cx: &mut Context<Self>) {
        if self.active_modal != ActiveModal::None
            || self.table_modal_state.is_open()
            || self.is_sidebar_rename_mounted()
            || self.is_sidebar_workspace_draft_mounted()
            || self.is_inbox_rename_mounted()
            || self.inbox_state.filter_focused
        {
            return;
        }
        self.inbox_state.rename = None;
        let now = now_unix_ms();
        let splits = self.inbox_visible_splits(now);
        if !splits.is_empty() {
            // Cycle strictly across the visible (non-empty) splits so `Tab` never lands on a
            // hidden `0`-count split.
            self.inbox_state.normalize_active_split(&splits);
            let len = splits.len();
            let cur = splits
                .iter()
                .position(|s| *s == self.inbox_state.active_split)
                .unwrap_or(0);
            let next = if reverse {
                (cur + len - 1) % len
            } else {
                (cur + 1) % len
            };
            self.inbox_state.active_split = splits[next].clone();
            self.inbox_state.selected_row = 0;
            cx.notify();
        }
    }

    /// Steps through armed Inbox sessions with `Alt+J` (`+1`) / `Alt+K` (`-1`).
    /// Returns `false` when stepping is disarmed (`step_index == None` or `step_order` is empty).
    pub(crate) fn step_inbox_session(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        let Some(current_idx) = self.inbox_state.step_index else {
            return false;
        };
        if self.inbox_state.step_order.is_empty() {
            return false;
        }
        let len = self.inbox_state.step_order.len() as isize;
        let current = current_idx.min(self.inbox_state.step_order.len() - 1) as isize;
        let step = delta.rem_euclid(len);
        let next = (current + step).rem_euclid(len) as usize;
        let key = self.inbox_state.step_order[next].clone();
        self.inbox_state.step_index = Some(next);
        if let Some((host, sess)) = self.host_for_session_key(&key) {
            self.open_session_on_host(host, sess, true, cx);
            return true;
        }
        false
    }

    pub(crate) fn handle_inbox_keydown(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active_modal != ActiveModal::None
            || self.table_modal_state.is_open()
            || self.is_sidebar_rename_mounted()
            || self.is_sidebar_workspace_draft_mounted()
        {
            return;
        }
        if self.inbox_state.rename.is_some() && !self.is_inbox_rename_mounted() {
            self.inbox_state.rename = None;
        }
        let now = now_unix_ms();
        let splits = self.inbox_visible_splits(now);
        self.inbox_state.normalize_active_split(&splits);
        let rows = self.compute_inbox_rows(now);
        let keys: Vec<String> = rows.iter().map(|r| r.session_key.clone()).collect();

        let outcome = self.inbox_state.handle_key(
            &ev.keystroke.key,
            ev.keystroke.key_char.as_deref(),
            ev.keystroke.modifiers,
            now,
            &splits,
            &keys,
        );

        match outcome {
            InboxKeyOutcome::Ignored => return,
            InboxKeyOutcome::Handled => {}
            InboxKeyOutcome::OpenSession(key) => {
                if let Some((host, sess)) = self.host_for_session_key(&key) {
                    self.open_session_on_host(host, sess, true, cx);
                }
            }
            InboxKeyOutcome::CloseInbox => {
                self.set_main_surface(MainSurface::Chat);
            }
            InboxKeyOutcome::MarkWaiting(key) => {
                self.mark_done_or_inbox(&key, cx);
            }
            InboxKeyOutcome::Hibernate(key) => {
                self.open_reminder_picker(crate::palette::ReminderTarget::Session(key), window, cx);
            }
            InboxKeyOutcome::UndoWaiting => {
                if let Some(key) = self.sidebar_state.workspace_store.undo_last_waiting() {
                    self.reactivate_session_key(&key, cx);
                    self.persist_workspace_store_if_enabled();
                    self.sync_workspace_to_shared(cx);
                }
            }
            InboxKeyOutcome::ToggleUnread(key) => {
                if let Some(sess) = self.find_session_by_key(&key) {
                    self.sidebar_state
                        .workspace_store
                        .toggle_session_unread(&key, &sess);
                    self.cleanup_unmounted_surface_state();
                    self.persist_workspace_store_if_enabled();
                    self.sync_workspace_to_shared(cx);
                }
            }
            InboxKeyOutcome::ToggleUnreadOnly => {
                self.toggle_unread_only(cx);
            }
            InboxKeyOutcome::StartRename(key) => {
                if self.is_session_rendered_in_inbox(&key)
                    && let Some(sess) = self.find_session_by_key(&key)
                {
                    let current = self
                        .sidebar_state
                        .workspace_store
                        .effective_title(&key, &sess.title)
                        .to_string();
                    self.sidebar_state.rename = None;
                    self.inbox_state.rename = Some(InlineRenameState::new(key, sess.id, current));
                }
            }
            InboxKeyOutcome::CommitRename { session_key, draft } => {
                self.commit_session_rename(&session_key, &draft, cx);
            }
            InboxKeyOutcome::CancelRename => {}
        }

        cx.stop_propagation();
        cx.notify();
    }

    pub(crate) fn render_inbox_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let now = now_unix_ms();
        let splits = self.inbox_visible_splits(now);
        // `inbox_state` may still point at a split that just dropped to `0` sessions (render is
        // `&self`, so it cannot normalize state). Show `All` instead of rows from a tab that is
        // no longer in the bar; the next key press normalizes the state for good.
        let active_split = if splits.contains(&self.inbox_state.active_split) {
            self.inbox_state.active_split.clone()
        } else if splits.contains(&InboxSplit::All) {
            InboxSplit::All
        } else {
            splits.first().cloned().unwrap_or(InboxSplit::All)
        };
        let filtered_rows = self.compute_inbox_rows_for(&active_split, now);
        let selected_idx = self
            .inbox_state
            .selected_row
            .min(filtered_rows.len().saturating_sub(1));

        let mut lifecycle_tabs = h_flex().gap_2().items_center();
        for lifecycle in SessionLifecycle::ALL {
            let active = self.inbox_state.lifecycle == lifecycle;
            lifecycle_tabs = lifecycle_tabs.child(
                div()
                    .id(SharedString::from(format!(
                        "inbox-view-{}",
                        lifecycle.label().to_lowercase()
                    )))
                    .role(Role::Button)
                    .test_support()
                    .px_3p5()
                    .py_2()
                    .rounded(RADIUS_CHIP)
                    .bg(if active {
                        theme::bg_active_row()
                    } else {
                        theme::bg_surface()
                    })
                    .text_color(if active {
                        theme::text_primary()
                    } else {
                        theme::text_muted()
                    })
                    .cursor_pointer()
                    .child(lifecycle.label())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.open_lifecycle_view(lifecycle, window, cx);
                        }),
                    ),
            );
        }

        // 1. Canonical Workspace Splits Row (Frequent, All, Favorites, per-workspace)
        let mut split_row = h_flex().flex_wrap().gap_1p5().items_center();
        let mut ws_split_counter = 0usize;
        for split in splits {
            let active = active_split == split;
            let split_rows = self.compute_inbox_rows_for(&split, now);
            let count = split_rows.len();

            let (el_id, label): (SharedString, String) = match &split {
                InboxSplit::Frequent => (
                    SharedString::from("inbox-split-frequent"),
                    format!("Frequent  {count}"),
                ),
                InboxSplit::All => (SharedString::from("inbox-tab-all"), format!("All  {count}")),
                InboxSplit::Favorites => (
                    SharedString::from("inbox-split-favorites"),
                    format!("Favorites  {count}"),
                ),
                InboxSplit::Workspace(wid) => {
                    let idx = ws_split_counter;
                    ws_split_counter += 1;
                    let name = self
                        .sidebar_state
                        .workspace_store
                        .workspace_by_id(wid)
                        .map(|w| w.name.as_str())
                        .unwrap_or(wid.as_str());
                    (
                        SharedString::from(format!("inbox-split-ws-{idx}")),
                        format!("{name}  {count}"),
                    )
                }
            };

            let favorite = split == InboxSplit::Favorites;
            let split_clone = split.clone();
            split_row = split_row.child(
                div()
                    .id(el_id)
                    .role(Role::Button)
                    .test_support()
                    .px_3p5()
                    .py_1p5()
                    .rounded(RADIUS_CHIP)
                    .bg(if active {
                        theme::bg_active_row()
                    } else {
                        theme::bg_surface()
                    })
                    .border_1()
                    .border_color(if active {
                        theme::text_secondary()
                    } else {
                        theme::border_subtle()
                    })
                    .text_xs()
                    .text_color(if active {
                        theme::text_primary()
                    } else {
                        theme::text_muted()
                    })
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.inbox_state.active_split = split_clone.clone();
                            this.inbox_state.selected_row = 0;
                            this.inbox_state.rename = None;
                            this.sidebar_state.rename = None;
                            this.sidebar_state.new_workspace_draft = None;
                            this.focus_inbox(window, cx);
                            cx.notify();
                        }),
                    )
                    .flex()
                    .items_center()
                    .gap_1()
                    .when(favorite, |el| {
                        el.child(lucide_icon(icons::STAR_FILLED, 14.0))
                    })
                    .child(label),
            );
        }

        if let Some(pill) = self.render_update_pill(cx) {
            split_row = split_row.child(pill);
        }

        // 2. Filter Bar (workspace splits stay; the search bar only mounts on demand)
        let unread_only = self.sidebar_state.unread_only;
        let filter_bar = self.inbox_state.is_search_visible().then(|| {
            h_flex()
                .flex_wrap()
                .gap_2()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .id("inbox-filter-input")
                        .role(Role::Button)
                        .test_support()
                        .flex_1()
                        .min_w(px(180.0))
                        .px_3()
                        .py_1p5()
                        .rounded(RADIUS_CHIP)
                        .bg(theme::bg_surface())
                        .border_1()
                        .border_color(if self.inbox_state.filter_focused {
                            theme::accent_blue()
                        } else {
                            theme::border_subtle()
                        })
                        .text_xs()
                        .text_color(if self.inbox_state.filter_query.is_empty() {
                            theme::text_muted()
                        } else {
                            theme::text_primary()
                        })
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                this.inbox_state.open_search();
                                this.inbox_state.rename = None;
                                this.sidebar_state.rename = None;
                                this.sidebar_state.new_workspace_draft = None;
                                this.focus_inbox(window, cx);
                                cx.notify();
                            }),
                        )
                        .child(if self.inbox_state.filter_query.is_empty() {
                            if self.inbox_state.filter_focused {
                                "/ |".to_string()
                            } else {
                                "Filter with / or Ctrl+F...".to_string()
                            }
                        } else if self.inbox_state.filter_focused {
                            format!("/ {}|", self.inbox_state.filter_query)
                        } else {
                            format!("/ {}", self.inbox_state.filter_query)
                        }),
                )
        });

        // 3. Session Rows
        let mut rows_el = v_flex().gap_1p5();
        if filtered_rows.is_empty() {
            rows_el = rows_el.child(
                div()
                    .p_5()
                    .rounded(RADIUS_BUBBLE)
                    .bg(theme::bg_surface())
                    .text_sm()
                    .text_color(theme::text_muted())
                    .child(if !self.inbox_state.filter_query.is_empty() {
                        "No sessions match the current Inbox filter."
                    } else if unread_only {
                        "All caught up — no unread sessions in this split."
                    } else {
                        match self.inbox_state.lifecycle {
                            SessionLifecycle::Active => "No sessions in Inbox.",
                            SessionLifecycle::Done => "No Done sessions. Mark a chat Done with Alt+E.",
                            SessionLifecycle::Hibernated => "No Hibernated sessions. Hibernate a chat with Alt+H.",
                            SessionLifecycle::Dissolved => "No Dissolved sessions. Dissolve stops the agent and keeps its history.",
                        }
                    }),
            );
        } else {
            let all_keys: Vec<String> = filtered_rows
                .iter()
                .map(|r| r.session_key.clone())
                .collect();
            for (idx, row) in filtered_rows.into_iter().enumerate() {
                let is_selected = idx == selected_idx;
                let key_click = row.session_key.clone();
                let key_dismiss = row.session_key.clone();
                let keys_for_step = all_keys.clone();
                let age = if self.inbox_state.lifecycle == SessionLifecycle::Hibernated {
                    let meta = &self.sidebar_state.workspace_store.session_meta[&row.session_key];
                    let wake = meta
                        .remind_at_ms
                        .and_then(chrono::DateTime::from_timestamp_millis)
                        .map(|at| {
                            at.with_timezone(&chrono::Local)
                                .format("%a %H:%M")
                                .to_string()
                        })
                        .unwrap_or_default();
                    format!(
                        "{wake} · {}",
                        if meta.hibernate_wake_on_reply {
                            "Wake on reply"
                        } else {
                            "Stay hidden"
                        }
                    )
                } else {
                    format_relative_age(row.sort_ms, now)
                };
                let is_renaming = self
                    .inbox_state
                    .rename
                    .as_ref()
                    .is_some_and(|r| r.session_key == row.session_key);
                let rename_text = if is_renaming {
                    self.inbox_state
                        .rename
                        .as_ref()
                        .map(InlineRenameState::render_text)
                } else {
                    None
                };
                let preview_text = row
                    .reminder_note
                    .clone()
                    .or_else(|| row.preview.clone())
                    .or_else(|| row.cwd.clone())
                    .unwrap_or_else(|| row.agent.label().to_string());

                rows_el = rows_el.child(
                    div()
                        .id(SharedString::from(format!("inbox-row-{idx}")))
                        .role(Role::Button)
                        .test_support()
                        .px_4()
                        .py_3()
                        .rounded(RADIUS_ROW)
                        .bg(if is_selected {
                            theme::bg_active_row()
                        } else {
                            theme::bg_surface()
                        })
                        .border_1()
                        .border_color(if is_selected {
                            theme::text_secondary()
                        } else {
                            theme::border_subtle()
                        })
                        .hover(|s| s.bg(theme::bg_hover()))
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.inbox_state.selected_row = idx;
                                this.inbox_state.step_order = keys_for_step.clone();
                                this.inbox_state.step_index = Some(idx);
                                if let Some((host, sess)) = this.host_for_session_key(&key_click) {
                                    this.open_session_on_host(host, sess, true, cx);
                                    window.focus(&this.focus.root_focus_handle, cx);
                                }
                            }),
                        )
                        .child(
                            h_flex()
                                .items_center()
                                .gap_3()
                                .child(
                                    div()
                                        .w(px(40.0))
                                        .h(px(40.0))
                                        .rounded(RADIUS_CHIP)
                                        .bg(Self::agent_color(row.agent))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .text_sm()
                                        .text_color(theme::bg_canvas())
                                        .child(row.agent.glyph()),
                                )
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .gap_0p5()
                                        .child(
                                            h_flex()
                                                .items_center()
                                                .justify_between()
                                                .gap_2()
                                                .child(
                                                    h_flex()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .items_center()
                                                        .gap_2()
                                                        .child(if let Some(draft) = rename_text {
                                                            div()
                                                                .id("inbox-rename-input")
                                                                .px_1p5()
                                                                .rounded(px(6.0))
                                                                .bg(theme::bg_canvas())
                                                                .border_1()
                                                                .border_color(theme::accent_blue())
                                                                .text_sm()
                                                                .text_color(theme::text_primary())
                                                                .truncate()
                                                                .child(draft)
                                                                .into_any_element()
                                                        } else {
                                                            div()
                                                                .text_sm()
                                                                .font_weight(if row.unread {
                                                                    FontWeight::SEMIBOLD
                                                                } else {
                                                                    FontWeight::NORMAL
                                                                })
                                                                .text_color(theme::text_primary())
                                                                .truncate()
                                                                .child(row.title)
                                                                .into_any_element()
                                                        })
                                                        .child(
                                                            div()
                                                                .px_2()
                                                                .py_0p5()
                                                                .rounded(RADIUS_CHIP)
                                                                .bg(theme::bg_canvas())
                                                                .text_xs()
                                                                .text_color(theme::text_muted())
                                                                .child(row.workspace_name),
                                                        ),
                                                )
                                                .child(
                                                    h_flex()
                                                        .items_center()
                                                        .gap_1p5()
                                                        .child(
                                                            div()
                                                                .text_xs()
                                                                .text_color(theme::text_muted())
                                                                .child(age),
                                                        )
                                                        .when(row.due_reminder, |el| {
                                                            el.child(
                                                                div()
                                                                    .id(SharedString::from(format!("dismiss-reminder-{idx}")))
                                                                    .role(Role::Button)
                                                                    .test_support()
                                                                    .px_2()
                                                                    .py_0p5()
                                                                    .rounded(RADIUS_CHIP)
                                                                    .bg(theme::bg_canvas())
                                                                    .text_xs()
                                                                    .text_color(theme::accent_amber())
                                                                    .child("Due reminder · Dismiss")
                                                                    .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| {
                                                                        cx.stop_propagation();
                                                                        this.sidebar_state.workspace_store.set_reminder(&key_dismiss, None, None);
                                                                        this.persist_workspace_store_if_enabled();
                                                                        this.sync_workspace_to_shared(cx);
                                                                        cx.notify();
                                                                    })),
                                                            )
                                                        })
                                                        .when(row.favorite, |el| {
                                                            el.child(
                                                                div()
                                                                    .w(px(6.0))
                                                                    .h(px(6.0))
                                                                    .rounded(RADIUS_CHIP)
                                                                    .bg(theme::accent_amber()),
                                                            )
                                                        })
                                                        .when(row.working, |el| {
                                                            el.child(
                                                                div()
                                                                    .w(px(7.0))
                                                                    .h(px(7.0))
                                                                    .rounded(RADIUS_CHIP)
                                                                    .bg(theme::accent_green()),
                                                            )
                                                        })
                                                        .when(row.unread, |el| {
                                                            el.child(
                                                                div()
                                                                    .w(px(7.0))
                                                                    .h(px(7.0))
                                                                    .rounded(RADIUS_CHIP)
                                                                    .bg(theme::accent_blue()),
                                                            )
                                                        }),
                                                ),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(if row.unread {
                                                    theme::accent_blue()
                                                } else {
                                                    theme::text_muted()
                                                })
                                                .truncate()
                                                .child(preview_text),
                                        ),
                                ),
                        ),
                );
            }
        }

        v_flex()
            .id("inbox-root")
            .key_context("Inbox")
            .flex_1()
            .h_full()
            .min_w_0()
            .bg(theme::bg_canvas())
            .when_some(self.inbox_state.focus_handle.as_ref(), |el, fh| {
                el.track_focus(fh)
            })
            .on_action(cx.listener(|this, _: &InboxNextSplit, _, cx| {
                this.cycle_inbox_split(false, cx);
            }))
            .on_action(cx.listener(|this, _: &InboxPrevSplit, _, cx| {
                this.cycle_inbox_split(true, cx);
            }))
            .on_action(cx.listener(|this, _: &CloseOverlay, _, cx| {
                if this.table_modal_state.is_open()
                    || this.active_modal != ActiveModal::None
                    || this.is_sidebar_rename_mounted()
                    || this.is_sidebar_workspace_draft_mounted()
                {
                    cx.propagate();
                    return;
                }
                if this.is_inbox_rename_mounted() {
                    this.inbox_state.rename = None;
                    cx.notify();
                } else {
                    if this.inbox_state.rename.is_some() {
                        this.inbox_state.rename = None;
                    }
                    if !this.inbox_state.close_search() {
                        cx.propagate();
                    } else {
                        cx.notify();
                    }
                }
            }))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                this.handle_inbox_keydown(ev, window, cx);
            }))
            .child(
                div().id("inbox-scroll").flex_1().overflow_y_scroll().child(
                    v_flex()
                        .w_full()
                        .max_w(px(800.0))
                        .mx_auto()
                        .p_5()
                        .gap_3()
                        .child(lifecycle_tabs)
                        .child(split_row)
                        .when_some(filter_bar, |el, bar| el.child(bar))
                        .child(rows_el),
                ),
            )
    }
}

#[cfg(test)]
#[path = "inbox_tests.rs"]
mod tests;
