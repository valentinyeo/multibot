use crate::app::{CloseOverlay, InboxNextSplit, InboxPrevSplit, MultibotView};
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
use multibot_core::workspaces::{InboxRow, InboxSplit, now_unix_ms};
use multibot_proto::AgentKind;

const DOUBLE_G_WINDOW_MS: i64 = 600;

#[derive(Debug, Clone, Default)]
pub(crate) struct InboxState {
    pub active_split: InboxSplit,
    pub agent_filter: Option<AgentKind>,
    pub selected_row: usize,
    pub filter_query: String,
    pub filter_focused: bool,
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
    ToggleUnread(String),
    ToggleUnreadOnly,
    StartRename(String),
    CommitRename { session_key: String, draft: String },
    CancelRename,
}

impl InboxState {
    pub(crate) fn select_all_split(&mut self) {
        self.active_split = InboxSplit::All;
        self.agent_filter = None;
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
                if !self.filter_query.is_empty() {
                    self.filter_query.clear();
                    self.selected_row = 0;
                }
                self.filter_focused = false;
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

        let no_mods = !modifiers.control && !modifiers.alt && !modifiers.shift && !modifiers.platform;
        let is_plain_g = no_mods && key_low == "g";
        if !is_plain_g {
            self.last_g_press_ms = None;
        }

        // Ctrl+Shift+U -> global unread-only filter
        if modifiers.control && modifiers.shift && !modifiers.alt && key_low == "u" {
            return InboxKeyOutcome::ToggleUnreadOnly;
        }

        // Ctrl+F or `/` -> focus live filter
        if (modifiers.control && !modifiers.shift && !modifiers.alt && key_low == "f")
            || (no_mods && (key == "/" || key_char == Some("/")))
        {
            self.filter_focused = true;
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
        if no_mods && (key_low == "h" || key_low == "left") {
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
        if no_mods && (key_low == "l" || key_low == "right") {
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
    pub(crate) fn compute_inbox_rows(&self, now_ms: i64) -> Vec<InboxRow> {
        let inputs = self.session_inputs();
        let mut rows = self.sidebar_state.workspace_store.query_inbox(
            &self.inbox_state.active_split,
            &inputs,
            self.sidebar_state.unread_only,
            &self.inbox_state.filter_query,
            now_ms,
        );
        if let Some(agent_kind) = self.inbox_state.agent_filter {
            rows.retain(|r| r.agent == agent_kind);
        }
        rows
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
        let splits = self.sidebar_state.workspace_store.visible_inbox_splits(now);
        if !splits.is_empty() {
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
        if let Some(sess) = self.find_session_by_key(&key) {
            self.open_session_with_tracking(sess, true, cx);
            return true;
        }
        false
    }

    pub(crate) fn handle_inbox_keydown(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
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
        let splits = self
            .sidebar_state
            .workspace_store
            .visible_inbox_splits(now);
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
                if let Some(sess) = self.find_session_by_key(&key) {
                    self.open_session_with_tracking(sess, true, cx);
                }
            }
            InboxKeyOutcome::CloseInbox => {
                self.set_main_surface(MainSurface::Chat);
            }
            InboxKeyOutcome::MarkWaiting(key) => {
                if let Some(sess) = self.find_session_by_key(&key) {
                    self.sidebar_state
                        .workspace_store
                        .archive_to_waiting_session(&key, now, &sess);
                    self.cleanup_unmounted_surface_state();
                    self.persist_workspace_store_if_enabled();
                    self.sync_workspace_to_shared(cx);
                }
            }
            InboxKeyOutcome::UndoWaiting => {
                if self
                    .sidebar_state
                    .workspace_store
                    .undo_last_waiting()
                    .is_some()
                {
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
                    self.inbox_state.rename =
                        Some(InlineRenameState::new(key, sess.id, current));
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
        let inputs = self.session_inputs();
        let splits = self
            .sidebar_state
            .workspace_store
            .visible_inbox_splits(now);
        let filtered_rows = self.compute_inbox_rows(now);
        let selected_idx = self
            .inbox_state
            .selected_row
            .min(filtered_rows.len().saturating_sub(1));

        // 1. Canonical Workspace Splits Row (Frequent, All, ★ Favorites, per-workspace)
        let mut split_row = h_flex().flex_wrap().gap_1p5().items_center();
        let mut ws_split_counter = 0usize;
        for split in splits {
            let active = self.inbox_state.active_split == split;
            let mut split_rows = self.sidebar_state.workspace_store.query_inbox(
                &split,
                &inputs,
                self.sidebar_state.unread_only,
                &self.inbox_state.filter_query,
                now,
            );
            if let Some(agent_kind) = self.inbox_state.agent_filter {
                split_rows.retain(|r| r.agent == agent_kind);
            }
            let count = split_rows.len();

            let (el_id, label): (SharedString, String) = match &split {
                InboxSplit::Frequent => (
                    SharedString::from("inbox-split-frequent"),
                    format!("Frequent  {count}"),
                ),
                InboxSplit::All => (
                    SharedString::from("inbox-tab-all"),
                    format!("All  {count}"),
                ),
                InboxSplit::Favorites => (
                    SharedString::from("inbox-split-favorites"),
                    format!("★ Favorites  {count}"),
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
                    .child(label),
            );
        }

        // 2. Filter Bar + Preserved Slice 1 Agent Chips (`inbox-tab-claude/agy/codex`) + Unread-only Pill
        let agent_base_rows = self.sidebar_state.workspace_store.query_inbox(
            &self.inbox_state.active_split,
            &inputs,
            self.sidebar_state.unread_only,
            &self.inbox_state.filter_query,
            now,
        );
        let count_claude = agent_base_rows
            .iter()
            .filter(|r| r.agent == AgentKind::Claude)
            .count();
        let count_agy = agent_base_rows
            .iter()
            .filter(|r| r.agent == AgentKind::Antigravity)
            .count();
        let count_codex = agent_base_rows
            .iter()
            .filter(|r| r.agent == AgentKind::Codex)
            .count();

        let agent_chips = [
            ("inbox-tab-claude", format!("Claude  {count_claude}"), AgentKind::Claude),
            (
                "inbox-tab-agy",
                format!("Antigravity  {count_agy}"),
                AgentKind::Antigravity,
            ),
            ("inbox-tab-codex", format!("Codex  {count_codex}"), AgentKind::Codex),
        ];

        let mut filter_bar = h_flex().flex_wrap().gap_2().items_center().justify_between();
        filter_bar = filter_bar.child(
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
                        this.inbox_state.filter_focused = !this.inbox_state.filter_focused;
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
        );

        let mut right_pills = h_flex().gap_1p5().items_center();
        for (id, label, kind) in agent_chips {
            let active = self.inbox_state.agent_filter == Some(kind);
            right_pills = right_pills.child(
                div()
                    .id(id)
                    .role(Role::Button)
                    .test_support()
                    .px_2p5()
                    .py_1()
                    .rounded(RADIUS_CHIP)
                    .bg(if active {
                        theme::bg_active_row()
                    } else {
                        theme::bg_surface()
                    })
                    .border_1()
                    .border_color(if active {
                        Self::agent_color(kind)
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
                            this.inbox_state.agent_filter =
                                if this.inbox_state.agent_filter == Some(kind) {
                                    None
                                } else {
                                    Some(kind)
                                };
                            this.inbox_state.selected_row = 0;
                            this.inbox_state.rename = None;
                            this.sidebar_state.rename = None;
                            this.sidebar_state.new_workspace_draft = None;
                            this.focus_inbox(window, cx);
                            cx.notify();
                        }),
                    )
                    .child(label),
            );
        }

        let unread_only = self.sidebar_state.unread_only;
        right_pills = right_pills.child(
            div()
                .id("inbox-unread-only-pill")
                .role(Role::Button)
                .test_support()
                .px_2p5()
                .py_1()
                .rounded(RADIUS_CHIP)
                .bg(if unread_only {
                    theme::bg_active_row()
                } else {
                    theme::bg_surface()
                })
                .border_1()
                .border_color(if unread_only {
                    theme::accent_blue()
                } else {
                    theme::border_subtle()
                })
                .text_xs()
                .text_color(if unread_only {
                    theme::accent_blue()
                } else {
                    theme::text_muted()
                })
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        this.sidebar_state.unread_only = !this.sidebar_state.unread_only;
                        this.inbox_state.selected_row = 0;
                        this.inbox_state.rename = None;
                        this.sidebar_state.rename = None;
                        this.sidebar_state.new_workspace_draft = None;
                        this.focus_inbox(window, cx);
                        this.sync_workspace_to_shared(cx);
                        cx.notify();
                    }),
                )
                .child(if unread_only {
                    "Unread Only · ON"
                } else {
                    "Unread Only"
                }),
        );
        filter_bar = filter_bar.child(right_pills);

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
                        "No sessions in this Inbox split yet. Connect a VPS host from the sidebar."
                    }),
            );
        } else {
            let all_keys: Vec<String> = filtered_rows.iter().map(|r| r.session_key.clone()).collect();
            for (idx, row) in filtered_rows.into_iter().enumerate() {
                let is_selected = idx == selected_idx;
                let key_click = row.session_key.clone();
                let keys_for_step = all_keys.clone();
                let age = format_relative_age(row.sort_ms, now);
                let is_renaming = self
                    .inbox_state
                    .rename
                    .as_ref()
                    .is_some_and(|r| r.session_key == row.session_key);
                let rename_text = if is_renaming {
                    self.inbox_state
                        .rename
                        .as_ref()
                        .map(|r| format!("{}|", r.draft))
                } else {
                    None
                };
                let preview_text = row
                    .preview
                    .clone()
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
                                if let Some(sess) = this.find_session_by_key(&key_click) {
                                    this.open_session_with_tracking(sess, true, cx);
                                    window.focus(&this.root_focus_handle, cx);
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
                    if this.inbox_state.filter_focused || !this.inbox_state.filter_query.is_empty()
                    {
                        this.inbox_state.filter_query.clear();
                        this.inbox_state.filter_focused = false;
                        this.inbox_state.selected_row = 0;
                        cx.notify();
                    } else {
                        cx.propagate();
                    }
                }
            }))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                this.handle_inbox_keydown(ev, cx);
            }))
            .child(self.render_top_floating_bar("Inbox".into(), cx))
            .child(
                div()
                    .id("inbox-scroll")
                    .flex_1()
                    .overflow_y_scroll()
                    .child(
                        v_flex()
                            .w_full()
                            .max_w(px(800.0))
                            .mx_auto()
                            .p_5()
                            .gap_3()
                            .child(split_row)
                            .child(filter_bar)
                            .child(rows_el),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{AppContext, Entity, TestAppContext, WindowOptions, test::TestWindowExt};
    use multibot_proto::SessionSummary;
    use std::sync::Arc;

    fn sample_splits() -> Vec<InboxSplit> {
        vec![
            InboxSplit::Frequent,
            InboxSplit::All,
            InboxSplit::Favorites,
            InboxSplit::Workspace("general".into()),
        ]
    }

    fn sample_keys() -> Vec<String> {
        vec![
            "local:claude:s1".into(),
            "local:codex:s2".into(),
            "local:antigravity:s3".into(),
        ]
    }

    #[test]
    fn inbox_clamped_rows_clamped_hl_and_wrapped_tab() {
        let mut st = InboxState::default();
        let splits = sample_splits();
        let keys = sample_keys();
        let none = Modifiers::default();

        // j/k clamp at boundaries
        assert_eq!(st.handle_key("k", None, none, 1000, &splits, &keys), InboxKeyOutcome::Handled);
        assert_eq!(st.selected_row, 0);
        st.handle_key("j", None, none, 1000, &splits, &keys);
        st.handle_key("j", None, none, 1000, &splits, &keys);
        st.handle_key("j", None, none, 1000, &splits, &keys);
        assert_eq!(st.selected_row, 2);

        // h/l clamp splits
        st.active_split = InboxSplit::Frequent;
        st.handle_key("h", None, none, 1000, &splits, &keys);
        assert_eq!(st.active_split, InboxSplit::Frequent);
        st.handle_key("l", None, none, 1000, &splits, &keys);
        assert_eq!(st.active_split, InboxSplit::All);
        st.handle_key("right", None, none, 1000, &splits, &keys);
        st.handle_key("right", None, none, 1000, &splits, &keys);
        st.handle_key("right", None, none, 1000, &splits, &keys);
        assert_eq!(st.active_split, InboxSplit::Workspace("general".into()));

        // Tab wraps forward, Shift+Tab wraps backward
        st.handle_key("tab", None, none, 1000, &splits, &keys);
        assert_eq!(st.active_split, InboxSplit::Frequent);
        st.handle_key(
            "tab",
            None,
            Modifiers {
                shift: true,
                ..Default::default()
            },
            1000,
            &splits,
            &keys,
        );
        assert_eq!(st.active_split, InboxSplit::Workspace("general".into()));
    }

    #[test]
    fn inbox_gg_600ms_window_and_shift_g_jump() {
        let mut st = InboxState::default();
        let splits = sample_splits();
        let keys = sample_keys();
        let none = Modifiers::default();

        // G jumps to bottom
        st.handle_key(
            "g",
            Some("G"),
            Modifiers {
                shift: true,
                ..Default::default()
            },
            1000,
            &splits,
            &keys,
        );
        assert_eq!(st.selected_row, 2);

        // Single g starts timer; >600ms does NOT jump to top
        st.handle_key("g", Some("g"), none, 1000, &splits, &keys);
        assert_eq!(st.selected_row, 2);
        st.handle_key("g", Some("g"), none, 1601, &splits, &keys);
        assert_eq!(st.selected_row, 2);

        // Second g within 600ms jumps to top
        st.handle_key("g", Some("g"), none, 2201, &splits, &keys);
        assert_eq!(st.selected_row, 0);

        // Intervening key cancels pending g
        st.selected_row = 2;
        st.handle_key("g", Some("g"), none, 3000, &splits, &keys);
        st.handle_key("j", Some("j"), none, 3100, &splits, &keys);
        st.handle_key("g", Some("g"), none, 3200, &splits, &keys);
        assert_eq!(st.selected_row, 2);
    }

    #[test]
    fn inbox_actions_digits_enter_waiting_undo_unread_f2_filter_and_escape() {
        let mut st = InboxState::default();
        let splits = sample_splits();
        let keys = sample_keys();
        let none = Modifiers::default();

        // Direct digit jump
        st.handle_key("2", Some("2"), none, 1000, &splits, &keys);
        assert_eq!(st.selected_row, 1);

        // Enter arms step order and emits OpenSession
        assert_eq!(
            st.handle_key("enter", None, none, 1000, &splits, &keys),
            InboxKeyOutcome::OpenSession("local:codex:s2".into())
        );
        assert_eq!(st.step_order, keys);
        assert_eq!(st.step_index, Some(1));

        // e / z / u / Ctrl+Shift+U / F2
        assert_eq!(
            st.handle_key("e", Some("e"), none, 1000, &splits, &keys),
            InboxKeyOutcome::MarkWaiting("local:codex:s2".into())
        );
        assert_eq!(
            st.handle_key("z", Some("z"), none, 1000, &splits, &keys),
            InboxKeyOutcome::UndoWaiting
        );
        assert_eq!(
            st.handle_key("u", Some("u"), none, 1000, &splits, &keys),
            InboxKeyOutcome::ToggleUnread("local:codex:s2".into())
        );
        assert_eq!(
            st.handle_key(
                "u",
                None,
                Modifiers {
                    control: true,
                    shift: true,
                    ..Default::default()
                },
                1000,
                &splits,
                &keys
            ),
            InboxKeyOutcome::ToggleUnreadOnly
        );
        assert_eq!(
            st.handle_key("f2", None, none, 1000, &splits, &keys),
            InboxKeyOutcome::StartRename("local:codex:s2".into())
        );

        // `/` focuses filter, typing appends, Esc clears/blurs first, second Esc closes Inbox
        st.handle_key("/", Some("/"), none, 1000, &splits, &keys);
        assert!(st.filter_focused);
        st.handle_key("a", Some("a"), none, 1000, &splits, &keys);
        assert_eq!(st.filter_query, "a");
        assert_eq!(
            st.handle_key("escape", None, none, 1000, &splits, &keys),
            InboxKeyOutcome::Handled
        );
        assert!(!st.filter_focused);
        assert!(st.filter_query.is_empty());
        assert_eq!(
            st.handle_key("escape", None, none, 1000, &splits, &keys),
            InboxKeyOutcome::CloseInbox
        );
    }

    #[gpui_kit::test]
    fn renders_inbox_splits_agent_chips_and_opens_clicked_row(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply_multibot_dark_theme(cx);
            crate::bind_multibot_keys(cx);
        });
        let rt = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let (handle, view): (gpui_kit::AnyWindowHandle, Entity<MultibotView>) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, move |window, cx| {
                cx.new(|cx| MultibotView::new(rt, window, cx))
            })
            .unwrap()
        });

        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.main_surface = MainSurface::Inbox;
                v.sessions = vec![
                    SessionSummary {
                        id: "c1".into(),
                        agent: AgentKind::Claude,
                        title: "Claude Deploy".into(),
                        cwd: Some("/srv/app".into()),
                        source_path: "/tmp/c1.jsonl".into(),
                        updated_at_ms: 9_000,
                        needs_attention: true,
                        last_assistant_preview: Some("Ready to ship".into()),
                        ..Default::default()
                    },
                    SessionSummary {
                        id: "x1".into(),
                        agent: AgentKind::Codex,
                        title: "Codex Refactor".into(),
                        cwd: Some("/srv/api".into()),
                        source_path: "/tmp/x1.jsonl".into(),
                        updated_at_ms: 8_000,
                        ..Default::default()
                    },
                ];
                let k1 = v.session_key_for(&v.sessions[0]);
                v.sidebar_state.workspace_store.toggle_favorite(&k1);
                cx.notify();
            });
            window.render_frame(cx);

            // Click splits and preserved Slice 1 agent chip
            window.click("inbox-split-frequent", cx);
            window.render_frame(cx);
            window.click("inbox-split-favorites", cx);
            window.render_frame(cx);
            window.click("inbox-tab-all", cx);
            window.render_frame(cx);
            window.click("inbox-tab-claude", cx);
            window.render_frame(cx);
            window.click("inbox-unread-only-pill", cx);
            window.render_frame(cx);

            // Click first row -> switches to Chat with c1 selected
            window.click("inbox-row-0", cx);
            window.render_frame(cx);

            let (surface, sel_id) = view.read_with(cx, |v, _| {
                (v.main_surface, v.selected_session.as_ref().map(|s| s.id.clone()))
            });
            assert_eq!(surface, MainSurface::Chat);
            assert_eq!(sel_id.as_deref(), Some("c1"));
        })
        .unwrap();
    }

    #[test]
    fn inbox_gg_rejects_clock_regression_and_up_clamps_shrunk_list_and_hl_recovers_vanished_split() {
        let mut st = InboxState::default();
        let splits = sample_splits();
        let keys = sample_keys();
        let none = Modifiers::default();

        // 1. gg rejects backward clock step (now_ms < prev)
        st.selected_row = 2;
        st.handle_key("g", Some("g"), none, 10_000, &splits, &keys);
        st.handle_key("g", Some("g"), none, 2_000, &splits, &keys);
        assert_eq!(st.selected_row, 2);
        assert_eq!(st.last_g_press_ms, Some(2_000));
        // Second g within 600ms of 2_000 succeeds
        st.handle_key("g", Some("g"), none, 2_500, &splits, &keys);
        assert_eq!(st.selected_row, 0);

        // 2. Stale selected_row after list shrinks moves immediately on first k / Up
        st.selected_row = 2;
        let shrunk = vec![keys[0].clone(), keys[1].clone()]; // max_idx = 1
        st.handle_key("k", Some("k"), none, 3_000, &splits, &shrunk);
        assert_eq!(st.selected_row, 0);

        // Also in filter mode
        st.filter_focused = true;
        st.selected_row = 5;
        st.handle_key("up", None, none, 3_100, &splits, &shrunk);
        assert_eq!(st.selected_row, 0);
        st.filter_focused = false;

        // 3. h / l recover to visible_splits[0] when active_split vanished
        st.active_split = InboxSplit::Workspace("snoozed-ws".into());
        st.selected_row = 2;
        st.handle_key("h", Some("h"), none, 3_200, &splits, &keys);
        assert_eq!(st.active_split, splits[0]);
        assert_eq!(st.selected_row, 0);

        st.active_split = InboxSplit::Workspace("deleted-ws".into());
        st.selected_row = 2;
        st.handle_key("l", Some("l"), none, 3_300, &splits, &keys);
        assert_eq!(st.active_split, splits[0]);
        assert_eq!(st.selected_row, 0);
    }

    #[gpui_kit::test]
    fn inbox_stepping_disarms_outside_inbox_and_close_overlay_respects_rename_filter_hierarchy(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply_multibot_dark_theme(cx);
            crate::bind_multibot_keys(cx);
        });
        let rt = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let (handle, view): (gpui_kit::AnyWindowHandle, Entity<MultibotView>) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, move |window, cx| {
                cx.new(|cx| MultibotView::new(rt, window, cx))
            })
            .unwrap()
        });

        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.sessions = vec![
                    SessionSummary {
                        id: "c1".into(),
                        agent: AgentKind::Claude,
                        title: "One".into(),
                        source_path: "/tmp/c1.jsonl".into(),
                        updated_at_ms: 9_000,
                        ..Default::default()
                    },
                    SessionSummary {
                        id: "c2".into(),
                        agent: AgentKind::Codex,
                        title: "Two".into(),
                        source_path: "/tmp/c2.jsonl".into(),
                        updated_at_ms: 8_000,
                        ..Default::default()
                    },
                ];
                let k1 = v.session_key_for(&v.sessions[0]);
                let k2 = v.session_key_for(&v.sessions[1]);

                // Arm stepping via Inbox
                v.inbox_state.step_order = vec![k1.clone(), k2.clone()];
                v.inbox_state.step_index = Some(0);
                assert!(v.step_inbox_session(1, cx));
                assert_eq!(v.selected_session.as_ref().unwrap().id, "c2");

                // Opening a session from Sidebar (from_inbox = false) disarms stepping completely
                let s1 = v.sessions[0].clone();
                v.open_session_with_tracking(s1, false, cx);
                assert!(v.inbox_state.step_index.is_none());
                assert!(v.inbox_state.step_order.is_empty());
                assert!(!v.step_inbox_session(1, cx));

                // Open Inbox with rename + matching filter active and focus Inbox
                v.main_surface = MainSurface::Inbox;
                v.inbox_state.filter_focused = true;
                v.inbox_state.filter_query = "one".into();
                v.inbox_state.rename = Some(InlineRenameState::new(k1, "c1".into(), "One".into()));
                v.focus_inbox(window, cx);
                cx.notify();
            });
            window.render_frame(cx);

            let fh = view.read_with(cx, |v, _| v.inbox_state.focus_handle.clone().unwrap());

            // 1st CloseOverlay cancels rename only
            fh.dispatch_action(&CloseOverlay, window, cx);
            window.render_frame(cx);
            assert!(view.read_with(cx, |v, _| v.inbox_state.rename.is_none()
                && v.inbox_state.filter_focused
                && v.main_surface == MainSurface::Inbox));

            // 2nd CloseOverlay clears & blurs filter only
            fh.dispatch_action(&CloseOverlay, window, cx);
            window.render_frame(cx);
            assert!(view.read_with(cx, |v, _| !v.inbox_state.filter_focused
                && v.inbox_state.filter_query.is_empty()
                && v.main_surface == MainSurface::Inbox));

            // 3rd CloseOverlay closes Inbox
            fh.dispatch_action(&CloseOverlay, window, cx);
            window.render_frame(cx);
            assert_eq!(view.read_with(cx, |v, _| v.main_surface), MainSurface::Chat);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn inbox_close_overlay_and_keydown_yield_to_active_modal_precedence(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply_multibot_dark_theme(cx);
            crate::bind_multibot_keys(cx);
        });
        let rt = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let (handle, view): (gpui_kit::AnyWindowHandle, Entity<MultibotView>) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, move |window, cx| {
                cx.new(|cx| MultibotView::new(rt, window, cx))
            })
            .unwrap()
        });

        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.sessions = vec![SessionSummary {
                    id: "c1".into(),
                    agent: AgentKind::Claude,
                    title: "Deploy Prod".into(),
                    source_path: "/tmp/c1.jsonl".into(),
                    updated_at_ms: 9_000,
                    needs_attention: true,
                    ..Default::default()
                }];
                let k1 = v.session_key_for(&v.sessions[0]);
                v.main_surface = MainSurface::Inbox;
                v.inbox_state.active_split = InboxSplit::All;
                v.inbox_state.filter_focused = true;
                v.inbox_state.filter_query = "deploy".into();
                v.inbox_state.rename =
                    Some(InlineRenameState::new(k1, "c1".into(), "Deploy Prod".into()));
                v.focus_inbox(window, cx);
                // Open F1 Shortcuts modal while inbox-root retains focus
                v.active_modal = ActiveModal::Shortcuts;
                cx.notify();
            });
            window.render_frame(cx);

            // Raw keydowns ('q' / 'e') while Shortcuts is open must NOT mutate or close Inbox
            view.update(cx, |v, cx| {
                let mut q_ks = gpui_kit::Keystroke::parse("q").unwrap();
                q_ks.key_char = Some("q".into());
                let ev_q = KeyDownEvent {
                    keystroke: q_ks,
                    is_held: false,
                    prefer_character_input: false,
                };
                v.handle_inbox_keydown(&ev_q, cx);

                let mut e_ks = gpui_kit::Keystroke::parse("e").unwrap();
                e_ks.key_char = Some("e".into());
                let ev_e = KeyDownEvent {
                    keystroke: e_ks,
                    is_held: false,
                    prefer_character_input: false,
                };
                v.handle_inbox_keydown(&ev_e, cx);

                assert_eq!(v.active_modal, ActiveModal::Shortcuts);
                assert_eq!(v.main_surface, MainSurface::Inbox);
                assert_eq!(v.inbox_state.filter_query, "deploy");
                assert!(v.inbox_state.rename.is_some());
                assert!(v.sidebar_state.workspace_store.waiting_sessions.is_empty());
            });

            let fh = view.read_with(cx, |v, _| v.inbox_state.focus_handle.clone().unwrap());

            // 1st CloseOverlay closes ActiveModal::Shortcuts FIRST, leaving Inbox + rename + filter intact
            fh.dispatch_action(&CloseOverlay, window, cx);
            window.render_frame(cx);
            view.read_with(cx, |v, _| {
                assert_eq!(v.active_modal, ActiveModal::None);
                assert_eq!(v.main_surface, MainSurface::Inbox);
                assert!(v.inbox_state.rename.is_some());
                assert_eq!(v.inbox_state.filter_query, "deploy");
                assert!(v.inbox_state.filter_focused);
            });

            // 2nd CloseOverlay walks Inbox hierarchy: cancels rename only
            fh.dispatch_action(&CloseOverlay, window, cx);
            window.render_frame(cx);
            view.read_with(cx, |v, _| {
                assert!(v.inbox_state.rename.is_none());
                assert!(v.inbox_state.filter_focused);
                assert_eq!(v.inbox_state.filter_query, "deploy");
                assert_eq!(v.main_surface, MainSurface::Inbox);
            });

            // 3rd CloseOverlay clears & blurs filter only
            fh.dispatch_action(&CloseOverlay, window, cx);
            window.render_frame(cx);
            view.read_with(cx, |v, _| {
                assert!(!v.inbox_state.filter_focused);
                assert!(v.inbox_state.filter_query.is_empty());
                assert_eq!(v.main_surface, MainSurface::Inbox);
            });

            // 4th CloseOverlay propagates cleanly to multibot-root and closes Inbox -> Chat
            fh.dispatch_action(&CloseOverlay, window, cx);
            window.render_frame(cx);
            assert_eq!(view.read_with(cx, |v, _| v.main_surface), MainSurface::Chat);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn real_tab_and_shift_tab_in_inbox_cycle_splits_and_keep_inbox_focus(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply_multibot_dark_theme(cx);
            crate::bind_multibot_keys(cx);
            crate::state::SharedAppStore::init(
                cx,
                multibot_core::MultibotSettings::default(),
            );
        });
        let rt = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let (handle, view): (gpui_kit::AnyWindowHandle, Entity<MultibotView>) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, move |window, cx| {
                cx.new(|cx| MultibotView::new(rt, window, cx))
            })
            .unwrap()
        });

        let now = now_unix_ms();
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                let s1 = SessionSummary {
                    id: "tab-s1".into(),
                    agent: AgentKind::Claude,
                    title: "Tab Session 1".into(),
                    updated_at_ms: now - 2_000,
                    ..Default::default()
                };
                let s2 = SessionSummary {
                    id: "tab-s2".into(),
                    agent: AgentKind::Codex,
                    title: "Tab Session 2".into(),
                    updated_at_ms: now - 1_000,
                    ..Default::default()
                };
                let k1 = v.session_key_for(&s1);
                v.sessions = vec![s1, s2];
                // Ensure Favorites + Workspace splits are visible alongside All
                v.sidebar_state.workspace_store.toggle_favorite(&k1);
                let _ = v
                    .sidebar_state
                    .workspace_store
                    .create_workspace("Backend", Some("#3B82F6".into()));
                cx.notify();
            });
            window.render_frame(cx);

            // Open Inbox via real Alt+I keypress (focuses inbox-root on All split)
            window.press("alt-i", cx);
            window.render_frame(cx);

            let splits = view.read_with(cx, |v, _| {
                assert_eq!(v.main_surface, MainSurface::Inbox);
                assert_eq!(v.inbox_state.active_split, InboxSplit::All);
                v.sidebar_state
                    .workspace_store
                    .visible_inbox_splits(now_unix_ms())
            });
            assert!(
                splits.len() >= 3,
                "expected at least 3 visible splits (All, Favorites, Workspaces), got {splits:?}"
            );
            let start_idx = splits
                .iter()
                .position(|s| *s == InboxSplit::All)
                .unwrap();

            // Press real Tab across every split and wrap around back to All
            for step in 1..=splits.len() {
                window.press("tab", cx);
                window.render_frame(cx);
                let expected = &splits[(start_idx + step) % splits.len()];
                assert_eq!(
                    &view.read(cx).inbox_state.active_split,
                    expected,
                    "Tab step {step} must advance/wrap active_split instead of being stolen by Root"
                );
                let inbox_still_focused = view
                    .read(cx)
                    .inbox_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window));
                assert!(
                    inbox_still_focused,
                    "Tab step {step} must keep focus on inbox-root"
                );
            }

            // Press real Shift+Tab to wrap backward
            window.press("shift-tab", cx);
            window.render_frame(cx);
            let expected_prev = &splits[(start_idx + splits.len() - 1) % splits.len()];
            assert_eq!(
                &view.read(cx).inbox_state.active_split,
                expected_prev,
                "Shift+Tab must wrap active_split backward"
            );

            // Return to All split via Tab and verify `j` row navigation still works because focus stayed on inbox-root
            window.press("tab", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).inbox_state.active_split, InboxSplit::All);
            assert_eq!(view.read(cx).inbox_state.selected_row, 0);
            window.press("j", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).inbox_state.selected_row,
                1,
                "`j` must still navigate Inbox rows after Tab/Shift+Tab"
            );
        })
        .unwrap();
    }
}
