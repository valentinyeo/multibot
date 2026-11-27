use crate::app::{CloseOverlay, MultibotView};
use crate::icons::{self, lucide_icon};
use crate::state::{ActiveModal, MainSurface};
use crate::theme::{self, RADIUS_CHIP, RADIUS_ROW};
use gpui_kit::{
    Context, FocusHandle, FontWeight, Hsla, InteractiveElement, IntoElement, KeyDownEvent,
    Modifiers, ModifiersChangedEvent, MouseButton, ParentElement, Role, SharedString,
    StatefulInteractiveElement, Styled, TestSupportExt, Window,
    component::{Selectable, Sizable, button::Button, h_flex, v_flex},
    div,
    prelude::FluentBuilder,
    px, rgb,
};
use multibot_core::SidebarMode;
use multibot_core::chat_scroll::UnreadVisit;
use multibot_core::workspaces::{
    ALL_WORKSPACES_ID, MAX_SESSION_TITLE_CHARS, ReorderDirection, STARRED_WORKSPACE_ID,
    SessionInput, SidebarRow, WorkspaceStore, WorkspaceView, canonical_session_key, now_unix_ms,
};
use multibot_proto::{ClientRequest, SessionSummary};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InlineRenameState {
    pub session_key: String,
    pub session_id: String,
    pub original: String,
    pub draft: String,
    pub all_selected: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RenameKeyOutcome {
    Consumed,
    Commit,
    Cancel,
    Ignored,
}

impl InlineRenameState {
    pub(crate) fn new(session_key: String, session_id: String, original: String) -> Self {
        Self {
            session_key,
            session_id,
            draft: original.clone(),
            original,
            all_selected: true,
        }
    }

    pub(crate) fn render_text(&self) -> gpui_kit::StyledText {
        let text = if self.all_selected {
            self.draft.clone()
        } else {
            format!("{}|", self.draft)
        };
        gpui_kit::StyledText::new(text).with_highlights(self.all_selected.then_some((
            0..self.draft.len(),
            gpui_kit::HighlightStyle {
                background_color: Some(theme::accent_blue()),
                ..Default::default()
            },
        )))
    }

    pub(crate) fn handle_key(
        &mut self,
        key: &str,
        key_char: Option<&str>,
        modifiers: Modifiers,
    ) -> RenameKeyOutcome {
        if key.eq_ignore_ascii_case("enter") {
            return RenameKeyOutcome::Commit;
        }
        if key.eq_ignore_ascii_case("escape") {
            return RenameKeyOutcome::Cancel;
        }
        if key.eq_ignore_ascii_case("backspace") || key.eq_ignore_ascii_case("delete") {
            if self.all_selected {
                self.draft.clear();
                self.all_selected = false;
            } else if key.eq_ignore_ascii_case("backspace") {
                self.draft.pop();
            }
            return RenameKeyOutcome::Consumed;
        }
        if modifiers.control || modifiers.alt || modifiers.platform {
            return RenameKeyOutcome::Ignored;
        }
        if let Some(ch_str) = key_char {
            let mut saw_printable = false;
            for ch in ch_str.chars() {
                if !ch.is_control() {
                    saw_printable = true;
                    if self.all_selected {
                        self.draft.clear();
                        self.all_selected = false;
                    }
                    if self.draft.chars().count() < MAX_SESSION_TITLE_CHARS {
                        self.draft.push(ch);
                    }
                }
            }
            if saw_printable {
                return RenameKeyOutcome::Consumed;
            }
        } else if key.chars().count() == 1 {
            let ch = key.chars().next().unwrap();
            if !ch.is_control() {
                if self.all_selected {
                    self.draft.clear();
                    self.all_selected = false;
                }
                if self.draft.chars().count() < MAX_SESSION_TITLE_CHARS {
                    let to_push = if modifiers.shift {
                        ch.to_ascii_uppercase()
                    } else {
                        ch
                    };
                    self.draft.push(to_push);
                }
                return RenameKeyOutcome::Consumed;
            }
        }
        RenameKeyOutcome::Ignored
    }
}

/// AltGr-safe check: show `1..9` jump badges ONLY when `Ctrl` is held without `Alt`, `Shift`,
/// or `Platform` (never fires on European AltGr chords).
pub(crate) fn should_show_jump_badges(modifiers: Modifiers) -> bool {
    modifiers.control && !modifiers.alt && !modifiers.shift && !modifiers.platform
}

#[derive(Debug, Clone)]
pub(crate) struct SidebarState {
    pub mode: SidebarMode,
    pub workspace_store: WorkspaceStore,
    pub active_workspace_id: String,
    pub active_workspace_view: WorkspaceView,
    pub unread_only: bool,
    pub recents_collapsed: bool,
    pub workspace_menu_open: bool,
    pub show_jump_badges: bool,
    pub previous_session_key: Option<String>,
    pub rename: Option<InlineRenameState>,
    pub new_workspace_draft: Option<String>,
    pub persist_to_disk: bool,
    pub focus_handle: Option<FocusHandle>,
}

impl Default for SidebarState {
    fn default() -> Self {
        Self {
            mode: SidebarMode::Open,
            workspace_store: WorkspaceStore::new(),
            active_workspace_id: ALL_WORKSPACES_ID.to_string(),
            active_workspace_view: WorkspaceView::All,
            unread_only: false,
            recents_collapsed: false,
            workspace_menu_open: false,
            show_jump_badges: false,
            previous_session_key: None,
            rename: None,
            new_workspace_draft: None,
            persist_to_disk: false,
            focus_handle: None,
        }
    }
}

impl SidebarState {
    pub(crate) fn is_shown(&self) -> bool {
        !matches!(self.mode, SidebarMode::Hidden)
    }

    pub(crate) fn cycle(&mut self) {
        self.mode = self.mode.cycle();
    }

    pub(crate) fn set_mode(&mut self, mode: SidebarMode) {
        self.mode = mode;
    }
}

fn parse_hex_color(hex: &str) -> Option<Hsla> {
    let trimmed = hex.trim().trim_start_matches('#');
    if trimmed.len() != 6 {
        return None;
    }
    let val = u32::from_str_radix(trimmed, 16).ok()?;
    Some(rgb(val).into())
}

impl MultibotView {
    fn render_session_badge(&self, row: &SidebarRow, size: f32) -> gpui_kit::Div {
        let meta = self
            .sidebar_state
            .workspace_store
            .session_meta
            .get(&row.session_key);
        let badge = div()
            .w(px(size))
            .h(px(size))
            .flex_shrink_0()
            .rounded(RADIUS_CHIP)
            .flex()
            .items_center()
            .justify_center();
        let Some(face) = meta.and_then(|meta| meta.face).filter(|face| *face < 20) else {
            return badge
                .bg(Self::agent_color(row.agent))
                .text_xs()
                .text_color(theme::bg_canvas())
                .child(row.agent.glyph());
        };
        let accent = meta
            .and_then(|meta| meta.color.as_deref())
            .and_then(parse_hex_color)
            .unwrap_or_else(|| Self::agent_color(row.agent));
        // Five eye styles crossed with four mouths give twenty asset-free faces.
        let (eye_width, left_height, right_height, eye_radius) = match face % 5 {
            0 => (0.12, 0.12, 0.12, 0.0),
            1 => (0.12, 0.12, 0.12, size),
            2 => (0.16, 0.06, 0.06, 0.0),
            3 => (0.12, 0.12, 0.04, size),
            _ => (0.07, 0.17, 0.17, 0.0),
        };
        let mouth_style = face / 5;
        let mouth_width = if mouth_style == 2 { 0.14 } else { 0.30 } * size;
        let mouth_height = if mouth_style == 0 { 0.06 } else { 0.14 } * size;
        let mouth = div()
            .absolute()
            .bottom(px(size * 0.08))
            .left(px((size * 0.70 - mouth_width) / 2.0 - 1.0))
            .w(px(mouth_width))
            .h(px(mouth_height))
            .border_1()
            .border_color(accent)
            .when(mouth_style == 0, |el| el.bg(accent))
            .when(mouth_style == 2, |el| el.rounded(RADIUS_CHIP))
            .when(mouth_style == 3, |el| {
                el.child(
                    div()
                        .absolute()
                        .left(px(mouth_width / 3.0))
                        .h_full()
                        .w(px(1.0))
                        .bg(accent),
                )
                .child(
                    div()
                        .absolute()
                        .right(px(mouth_width / 3.0))
                        .h_full()
                        .w(px(1.0))
                        .bg(accent),
                )
            });
        badge.bg(theme::bg_surface()).child(
            div()
                .relative()
                .w(px(size * 0.70))
                .h(px(size * 0.62))
                .rounded(px(4.0))
                .border_1()
                .border_color(accent)
                .child(
                    div()
                        .absolute()
                        .left(px(size * 0.12))
                        .top(px(size * 0.10))
                        .w(px(size * eye_width))
                        .h(px(size * left_height))
                        .rounded(px(eye_radius))
                        .bg(accent),
                )
                .child(
                    div()
                        .absolute()
                        .right(px(size * 0.12))
                        .top(px(size * 0.10))
                        .w(px(size * eye_width))
                        .h(px(size * right_height))
                        .rounded(px(eye_radius))
                        .bg(accent),
                )
                .child(mouth),
        )
    }

    pub(crate) fn current_host_label(&self) -> &str {
        self.connected_host.as_deref().unwrap_or("local")
    }

    pub(crate) fn tx_for_host(
        &self,
        host: &str,
    ) -> Option<&tokio::sync::mpsc::UnboundedSender<ClientRequest>> {
        self.host_txs.get(host).or_else(|| {
            // Never send a disconnected host's requests to another server.
            if self.host_statuses.is_empty() {
                self.daemon_tx.as_ref()
            } else {
                None
            }
        })
    }

    pub(crate) fn active_session_host(&self) -> &str {
        self.selected_session_host
            .as_deref()
            .unwrap_or_else(|| self.current_host_label())
    }

    pub(crate) fn active_session_tx(
        &self,
    ) -> Option<&tokio::sync::mpsc::UnboundedSender<ClientRequest>> {
        self.tx_for_host(self.active_session_host())
    }

    pub(crate) fn host_for_summary(&self, session: &SessionSummary) -> String {
        self.host_sessions
            .iter()
            .find(|(_, s)| s == session)
            .map(|(host, _)| host.clone())
            .unwrap_or_else(|| self.active_session_host().to_string())
    }

    pub(crate) fn session_key_for(&self, session: &SessionSummary) -> String {
        let host = if self.selected_session.as_ref().is_some_and(|s| s == session) {
            self.active_session_host().to_string()
        } else {
            self.host_for_summary(session)
        };
        canonical_session_key(&host, session.agent, &session.id)
    }

    pub(crate) fn session_inputs(&self) -> Vec<SessionInput<'_>> {
        if !self.host_sessions.is_empty() {
            return self
                .host_sessions
                .iter()
                .map(|(host, summary)| SessionInput {
                    key: canonical_session_key(host, summary.agent, &summary.id),
                    host,
                    summary,
                })
                .collect();
        }
        let host = self.current_host_label();
        self.sessions
            .iter()
            .map(|summary| SessionInput {
                key: canonical_session_key(host, summary.agent, &summary.id),
                host,
                summary,
            })
            .collect()
    }

    pub(crate) fn host_for_session_key(&self, key: &str) -> Option<(String, SessionSummary)> {
        self.session_inputs()
            .into_iter()
            .find(|s| s.key == key || s.summary.id == key)
            .map(|s| (s.host.to_string(), s.summary.clone()))
    }

    pub(crate) fn find_session_by_key(&self, key: &str) -> Option<SessionSummary> {
        self.host_for_session_key(key).map(|(_, s)| s)
    }

    pub(crate) fn open_session_key_with_tracking(
        &mut self,
        key: &str,
        from_inbox: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some((host, session)) = self.host_for_session_key(key) {
            self.open_session_on_host(host, session, from_inbox, cx);
        }
    }

    /// Loads persisted `workspaces.json` and `palette-stats.json` (called by M6 startup wiring).
    pub(crate) fn load_persisted_workspace_state(&mut self, cx: &mut Context<Self>) {
        self.sidebar_state.workspace_store = WorkspaceStore::load();
        self.sidebar_state.recents_collapsed =
            self.sidebar_state.workspace_store.recents_section_collapsed;
        self.sidebar_state.persist_to_disk = true;
        self.sidebar_state.active_workspace_id = ALL_WORKSPACES_ID.to_string();
        self.palette_state.stats = multibot_core::workspaces::PaletteStats::load();
        self.palette_state.persist_to_disk = true;
        cx.notify();
    }

    pub(crate) fn persist_workspace_store_if_enabled(&self) {
        if self.sidebar_state.persist_to_disk {
            let _ = self.sidebar_state.workspace_store.save();
        }
    }

    pub(crate) fn open_session_with_tracking(
        &mut self,
        session: SessionSummary,
        from_inbox: bool,
        cx: &mut Context<Self>,
    ) {
        let host = self.host_for_summary(&session);
        self.open_session_on_host(host, session, from_inbox, cx);
    }

    pub(crate) fn open_session_on_host(
        &mut self,
        host: String,
        session: SessionSummary,
        from_inbox: bool,
        cx: &mut Context<Self>,
    ) {
        let next_key = canonical_session_key(&host, session.agent, &session.id);
        if let Some(ref current) = self.selected_session {
            let cur_key = self.session_key_for(current);
            if cur_key != next_key {
                self.sidebar_state.previous_session_key = Some(cur_key);
            }
        }

        let was_unread = self
            .sidebar_state
            .workspace_store
            .is_session_unread(&next_key, &session);
        let unread_visit = if was_unread {
            UnreadVisit::Unread {
                previous_boundary: self.sidebar_state
                    .workspace_store
                    .unread_boundaries
                    .get(&next_key)
                    .cloned(),
            }
        } else {
            UnreadVisit::Read
        };
        let now = now_unix_ms();
        self.sidebar_state
            .workspace_store
            .record_session_view(&next_key, now);
        self.sidebar_state
            .workspace_store
            .mark_session_read(&next_key, &session);

        if !from_inbox {
            self.inbox_state.step_index = None;
            self.inbox_state.step_order.clear();
        }
        self.persist_workspace_store_if_enabled();
        self.sync_workspace_to_shared(cx);
        self.select_session_on_host(host, session, cx);
        self.chat_state.scroll.open_visit(unread_visit);
        if !self.messages.is_empty() {
            self.chat_state.scroll.history_loaded(&self.messages, true);
        }
    }

    pub(crate) fn commit_session_rename(
        &mut self,
        session_key: &str,
        draft: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(clean) = self
            .sidebar_state
            .workspace_store
            .apply_session_rename(session_key, draft)
        else {
            self.sidebar_state.rename = None;
            self.inbox_state.rename = None;
            cx.notify();
            return false;
        };

        if let Some((host, mut session)) = self.host_for_session_key(session_key) {
            session.title = clean.clone();
            if let Some(tx) = self.tx_for_host(&host) {
                let _ = tx.send(ClientRequest::RenameSession {
                    session_id: session.id.clone(),
                    agent: session.agent,
                    source_path: session.source_path.clone(),
                    title: clean,
                });
            }
            for (h, s) in &mut self.host_sessions {
                if h == &host && s.id == session.id && s.agent == session.agent {
                    *s = session.clone();
                }
            }
            if self.host_statuses.is_empty() {
                for s in &mut self.sessions {
                    if s.id == session.id && s.agent == session.agent {
                        *s = session.clone();
                    }
                }
            }
            if self.active_session_host() == host
                && self
                    .selected_session
                    .as_ref()
                    .is_some_and(|s| s.id == session.id && s.agent == session.agent)
            {
                self.selected_session = Some(session);
            }
        }

        self.sidebar_state.rename = None;
        self.inbox_state.rename = None;
        self.persist_workspace_store_if_enabled();
        self.sync_workspace_to_shared(cx);
        self.sync_sessions_to_shared(cx);
        cx.notify();
        true
    }

    pub(crate) fn ensure_sidebar_focus_handle(&mut self, cx: &mut Context<Self>) -> FocusHandle {
        if let Some(ref fh) = self.sidebar_state.focus_handle {
            return fh.clone();
        }
        let fh = cx.focus_handle();
        self.sidebar_state.focus_handle = Some(fh.clone());
        fh
    }

    pub(crate) fn focus_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let fh = self.ensure_sidebar_focus_handle(cx);
        window.focus(&fh, cx);
    }

    pub(crate) fn start_sidebar_rename(&mut self, session_key: String, cx: &mut Context<Self>) {
        if !self.can_mount_sidebar_rename(&session_key) {
            return;
        }
        if let Some(sess) = self.find_session_by_key(&session_key) {
            let current = self
                .sidebar_state
                .workspace_store
                .effective_title(&session_key, &sess.title)
                .to_string();
            let _ = self.ensure_sidebar_focus_handle(cx);
            self.inbox_state.rename = None;
            self.sidebar_state.rename =
                Some(InlineRenameState::new(session_key, sess.id, current));
            cx.notify();
        }
    }

    pub(crate) fn dismiss_conflicting_chat_overlays_for_rename(&mut self) {
        self.slash_picker_state.close();
        self.chat_state.close_find();
        self.chat_state.reply_quote = None;
        self.chat_state.focused_message_idx = None;
        self.chat_state.message_action_menu_open = false;
    }

    pub(crate) fn start_sidebar_rename_with_window(
        &mut self,
        session_key: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_sidebar_rename(session_key, cx);
        if self.sidebar_state.rename.is_some() {
            if self.sidebar_state.mode == SidebarMode::Open {
                self.focus_sidebar(window, cx);
            } else {
                window.focus(&self.root_focus_handle, cx);
            }
        }
    }

    pub(crate) fn toggle_active_favorite(&mut self, cx: &mut Context<Self>) {
        if let Some(sess) = self.selected_session.clone() {
            let key = self.session_key_for(&sess);
            self.sidebar_state.workspace_store.toggle_favorite(&key);
            self.cleanup_unmounted_surface_state();
            self.persist_workspace_store_if_enabled();
            self.sync_workspace_to_shared(cx);
            cx.notify();
        }
    }

    pub(crate) fn toggle_unread_only(&mut self, cx: &mut Context<Self>) {
        self.sidebar_state.unread_only = !self.sidebar_state.unread_only;
        self.sidebar_state.rename = None;
        self.inbox_state.rename = None;
        self.sync_workspace_to_shared(cx);
        cx.notify();
    }

    pub(crate) fn switch_workspace_digit(&mut self, digit: u8, cx: &mut Context<Self>) {
        let now = now_unix_ms();
        let woke = !self
            .sidebar_state
            .workspace_store
            .wake_due_workspaces(now)
            .is_empty();
        if woke {
            self.persist_workspace_store_if_enabled();
            self.sync_workspace_to_shared(cx);
        }
        if let Some(ws_id) = self
            .sidebar_state
            .workspace_store
            .resolve_alt_digit(digit, now)
        {
            self.sidebar_state.active_workspace_id = ws_id;
            self.sidebar_state.active_workspace_view = WorkspaceView::All;
            self.sidebar_state.workspace_menu_open = false;
            self.sidebar_state.rename = None;
            self.cleanup_unmounted_surface_state();
            cx.notify();
        } else if woke {
            self.cleanup_unmounted_surface_state();
            cx.notify();
        }
    }

    pub(crate) fn cycle_workspace(&mut self, delta: isize, cx: &mut Context<Self>) {
        let now = now_unix_ms();
        if !self
            .sidebar_state
            .workspace_store
            .wake_due_workspaces(now)
            .is_empty()
        {
            self.persist_workspace_store_if_enabled();
            self.sync_workspace_to_shared(cx);
        }
        let next = self.sidebar_state.workspace_store.cycle_workspace_id(
            &self.sidebar_state.active_workspace_id,
            delta,
            now,
        );
        self.sidebar_state.active_workspace_id = next;
        self.sidebar_state.active_workspace_view = WorkspaceView::All;
        self.sidebar_state.workspace_menu_open = false;
        self.sidebar_state.rename = None;
        self.cleanup_unmounted_surface_state();
        cx.notify();
    }

    pub(crate) fn set_workspace_view(&mut self, view: WorkspaceView, cx: &mut Context<Self>) {
        if view == WorkspaceView::Frequent {
            self.sidebar_state.active_workspace_id = ALL_WORKSPACES_ID.to_string();
        }
        self.sidebar_state.active_workspace_view = view;
        self.sidebar_state.workspace_menu_open = false;
        self.sidebar_state.rename = None;
        self.cleanup_unmounted_surface_state();
        cx.notify();
    }

    pub(crate) fn reorder_active_session(
        &mut self,
        direction: ReorderDirection,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(sel) = self.selected_session.clone() else {
            return false;
        };
        let key = self.session_key_for(&sel);
        let ws_id = self.sidebar_state.active_workspace_id.clone();
        let now = now_unix_ms();
        let sessions: Vec<_> = self
            .session_inputs()
            .into_iter()
            .map(|s| (s.host.to_string(), s.summary.clone()))
            .collect();
        let inputs: Vec<_> = sessions
            .iter()
            .map(|(host, summary)| SessionInput {
                key: canonical_session_key(host, summary.agent, &summary.id),
                host,
                summary,
            })
            .collect();
        let moved = self
            .sidebar_state
            .workspace_store
            .reorder_session(&ws_id, &key, direction, &inputs, now);
        if moved {
            self.persist_workspace_store_if_enabled();
            self.sync_workspace_to_shared(cx);
            cx.notify();
        }
        moved
    }

    /// `Ctrl+G`: collapses or expands the active session's group (no-op if ungrouped).
    pub(crate) fn toggle_active_group(&mut self, cx: &mut Context<Self>) {
        let Some(sel) = self.selected_session.clone() else {
            return;
        };
        let key = self.session_key_for(&sel);
        let ws_id = if self.sidebar_state.active_workspace_id == ALL_WORKSPACES_ID {
            self.sidebar_state
                .workspace_store
                .effective_workspace_id(&key)
        } else {
            self.sidebar_state.active_workspace_id.clone()
        };
        if self
            .sidebar_state
            .workspace_store
            .toggle_session_group_collapsed(&key, &ws_id)
            .is_some()
        {
            self.sidebar_state.rename = None;
            self.cleanup_unmounted_surface_state();
            self.persist_workspace_store_if_enabled();
            self.sync_workspace_to_shared(cx);
            cx.notify();
        }
    }

    /// Updates AltGr-safe `1..9` jump badge visibility on modifier changes (also exposed for M6 root forwarding).
    pub(crate) fn handle_modifiers_changed(
        &mut self,
        ev: &ModifiersChangedEvent,
        cx: &mut Context<Self>,
    ) {
        let next = should_show_jump_badges(ev.modifiers);
        if self.sidebar_state.show_jump_badges != next {
            self.sidebar_state.show_jump_badges = next;
            cx.notify();
        }
    }

    pub(crate) fn set_workspace_snooze(
        &mut self,
        ws_id: &str,
        until_ms: Option<i64>,
        cx: &mut Context<Self>,
    ) {
        if let Some(until) = until_ms {
            match self
                .sidebar_state
                .workspace_store
                .snooze_workspace(ws_id, until, now_unix_ms())
            {
                Ok(next) => {
                    if self.sidebar_state.active_workspace_id == ws_id {
                        self.sidebar_state.active_workspace_id =
                            next.unwrap_or_else(|| ALL_WORKSPACES_ID.into());
                    }
                }
                Err(error) => {
                    self.notice = Some(error.to_string());
                    cx.notify();
                    return;
                }
            }
        } else {
            self.sidebar_state.workspace_store.wake_workspace(ws_id);
        }
        self.sidebar_state.rename = None;
        self.cleanup_unmounted_surface_state();
        self.persist_workspace_store_if_enabled();
        self.sync_workspace_to_shared(cx);
        cx.notify();
    }

    fn handle_sidebar_keydown(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        if self.active_modal != ActiveModal::None
            || self.table_modal_state.is_open()
            || self.has_mounted_chat_overlay()
        {
            return;
        }
        let key = ev.keystroke.key.as_str();
        let key_char = ev.keystroke.key_char.as_deref();
        let mods = ev.keystroke.modifiers;

        if self.sidebar_state.rename.is_some() {
            if !self.is_sidebar_rename_mounted() {
                self.sidebar_state.rename = None;
            } else if let Some(ref mut rename) = self.sidebar_state.rename {
                let outcome = rename.handle_key(key, key_char, mods);
                match outcome {
                    RenameKeyOutcome::Consumed => {
                        cx.stop_propagation();
                        cx.notify();
                    }
                    RenameKeyOutcome::Commit => {
                        let key_str = rename.session_key.clone();
                        let draft = rename.draft.clone();
                        self.commit_session_rename(&key_str, &draft, cx);
                        cx.stop_propagation();
                    }
                    RenameKeyOutcome::Cancel => {
                        self.sidebar_state.rename = None;
                        cx.stop_propagation();
                        cx.notify();
                    }
                    RenameKeyOutcome::Ignored => {}
                }
                return;
            }
        }

        if let Some(ref mut draft) = self.sidebar_state.new_workspace_draft {
            if key.eq_ignore_ascii_case("enter") {
                let name = draft.clone();
                self.sidebar_state.new_workspace_draft = None;
                if let Ok(ws) = self
                    .sidebar_state
                    .workspace_store
                    .create_workspace(&name, None)
                {
                    self.sidebar_state.active_workspace_id = ws.id;
                    self.sidebar_state.rename = None;
                    self.cleanup_unmounted_surface_state();
                    self.persist_workspace_store_if_enabled();
                    self.sync_workspace_to_shared(cx);
                }
                cx.stop_propagation();
                cx.notify();
            } else if key.eq_ignore_ascii_case("escape") {
                self.sidebar_state.new_workspace_draft = None;
                cx.stop_propagation();
                cx.notify();
            } else if key.eq_ignore_ascii_case("backspace") {
                draft.pop();
                cx.stop_propagation();
                cx.notify();
            } else if !mods.control && !mods.alt && !mods.platform {
                if let Some(cs) = key_char {
                    for ch in cs.chars() {
                        if !ch.is_control() && draft.chars().count() < MAX_SESSION_TITLE_CHARS {
                            draft.push(ch);
                        }
                    }
                    cx.stop_propagation();
                    cx.notify();
                }
            }
        }
    }

    pub(crate) fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let now = now_unix_ms();
        let inputs = self.session_inputs();
        let active_key = self
            .selected_session
            .as_ref()
            .map(|s| self.session_key_for(s));
        let layout = self
            .sidebar_state
            .workspace_store
            .compute_sidebar_layout_with_options(
                &self.sidebar_state.active_workspace_id,
                self.sidebar_state.active_workspace_view,
                &inputs,
                self.sidebar_state.unread_only,
                active_key.as_deref(),
                now,
            );
        let unread_count = self
            .sidebar_state
            .workspace_store
            .inbox_unread_count_at(&inputs, now);
        let jump_badges = self.sidebar_state.show_jump_badges;
        let jump_index_for = |session_key: &str| -> Option<usize> {
            layout
                .ctrl_digit_order
                .iter()
                .position(|k| k == session_key)
                .filter(|&i| i < 9)
                .map(|i| i + 1)
        };

        if matches!(self.sidebar_state.mode, SidebarMode::Collapsed) {
            return self
                .render_collapsed_sidebar(layout, unread_count, jump_badges, now, cx)
                .into_any_element();
        }

        let (gpu_label, gpu_safe) = self.active_gpu_summary();
        let in_starred = self.sidebar_state.active_workspace_id == STARRED_WORKSPACE_ID;

        let mut ws_menu = v_flex()
            .id("workspace-menu")
            .test_support()
            .w_full()
            .max_h(px(420.0))
            .overflow_y_scroll()
            .gap_1()
            .p_2()
            .rounded(RADIUS_ROW)
            .bg(theme::bg_surface())
            .border_1()
            .border_color(theme::border_subtle());
        let store = &self.sidebar_state.workspace_store;
        let mut choices = vec![
            (
                "All".to_string(),
                ALL_WORKSPACES_ID.to_string(),
                WorkspaceView::All,
            ),
            (
                "Frequent".to_string(),
                ALL_WORKSPACES_ID.to_string(),
                WorkspaceView::Frequent,
            ),
            (
                "Favorites".to_string(),
                ALL_WORKSPACES_ID.to_string(),
                WorkspaceView::Favorites,
            ),
        ];
        choices.extend(
            store
                .visible_workspaces(now)
                .into_iter()
                .map(|ws| (ws.name.clone(), ws.id.clone(), WorkspaceView::All)),
        );
        let mut active_label = if in_starred {
            "Favorites".to_string()
        } else {
            store
                .workspace_by_id(&self.sidebar_state.active_workspace_id)
                .map(|ws| ws.name.clone())
                .unwrap_or_else(|| "All".to_string())
        };
        for (idx, (name, ws_id, view)) in choices.into_iter().enumerate() {
            let selected = self.sidebar_state.active_workspace_id == ws_id
                && self.sidebar_state.active_workspace_view == view;
            if selected {
                active_label = name.clone();
            }
            let count = store
                .compute_sidebar_layout_with_options(&ws_id, view, &inputs, false, None, now)
                .flat_display_order
                .len();
            ws_menu = ws_menu.child(
                Button::new(SharedString::from(format!("workspace-option-{idx}")))
                    .small()
                    .w_full()
                    .accessibility_label(format!("{name}, {count} sessions"))
                    .label(name)
                    .selected(selected)
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::text_muted())
                            .child(count.to_string()),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.sidebar_state.active_workspace_id = ws_id.clone();
                        this.sidebar_state.active_workspace_view = view;
                        this.sidebar_state.workspace_menu_open = false;
                        this.sidebar_state.rename = None;
                        this.cleanup_unmounted_surface_state();
                        this.focus_active_surface_default(window, cx);
                        cx.notify();
                    })),
            );
        }
        ws_menu = ws_menu
            .child(div().h(px(1.0)).my_1().bg(theme::border_subtle()))
            .child(
                Button::new("workspace-new")
                    .small()
                    .w_full()
                    .label("New workspace")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.sidebar_state.workspace_menu_open = false;
                        this.sidebar_state.new_workspace_draft = Some(String::new());
                        this.focus_sidebar(window, cx);
                        cx.notify();
                    })),
            );
        if store
            .workspace_by_id(&self.sidebar_state.active_workspace_id)
            .is_some()
        {
            ws_menu = ws_menu.child(
                Button::new("workspace-dissolve")
                    .small()
                    .w_full()
                    .label("Dissolve workspace")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.sidebar_state.workspace_menu_open = false;
                        this.request_dissolve_workspace(window, cx);
                    })),
            );
        }
        let ws_strip = v_flex()
            .w_full()
            .relative()
            .child(
                Button::new("workspace-dropdown")
                    .w_full()
                    .rounded(RADIUS_CHIP)
                    .label(active_label)
                    .child(div().flex_1())
                    .child(lucide_icon(icons::CHEVRON_DOWN, 12.0))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.sidebar_state.rename = None;
                        this.sidebar_state.workspace_menu_open =
                            !this.sidebar_state.workspace_menu_open;
                        this.focus_sidebar(window, cx);
                        cx.notify();
                    })),
            )
            .when(self.sidebar_state.workspace_menu_open, |el| {
                el.child(
                    gpui_kit::deferred(
                        div()
                            .absolute()
                            .top(px(40.0))
                            .left_0()
                            .w_full()
                            .occlude()
                            .child(ws_menu),
                    )
                    .with_priority(1),
                )
            });

        // Sessions scroll contents: Pinned Grid -> Groups -> Recents
        let mut sessions_content = v_flex().gap_2();

        // 1. Pinned Avatar Tile Grid (suppresses "Pinned" text heading when non-empty)
        if !self.sidebar_state.workspace_store.pinned_section_collapsed
            && !layout.pinned_tiles.is_empty()
        {
            let mut pin_grid = h_flex()
                .id("sidebar-pinned-grid")
                .test_support()
                .flex_wrap()
                .justify_center()
                .gap(px(4.0));
            for (idx, row) in layout.pinned_tiles.iter().enumerate() {
                let is_selected = active_key.as_deref() == Some(row.session_key.as_str());
                let badge = if jump_badges {
                    jump_index_for(&row.session_key)
                } else {
                    None
                };
                let is_renaming = self
                    .sidebar_state
                    .rename
                    .as_ref()
                    .is_some_and(|r| r.session_key == row.session_key);
                let rename_draft = if is_renaming {
                    self.sidebar_state
                        .rename
                        .as_ref()
                        .map(InlineRenameState::render_text)
                } else {
                    None
                };
                let key_left = row.session_key.clone();
                let key_right = row.session_key.clone();

                pin_grid = pin_grid.child(
                    div()
                        .id(SharedString::from(format!("sidebar-pin-{idx}")))
                        .role(Role::Button)
                        .test_support()
                        .w(px(80.0))
                        .h(px(84.0))
                        .p_1p5()
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
                            cx.listener(move |this, _, _, cx| {
                                if let Some((host, sess)) = this.host_for_session_key(&key_left) {
                                    this.open_session_on_host(host, sess, false, cx);
                                }
                            }),
                        )
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |this, _, window, cx| {
                                this.open_palette_tab_context(key_right.clone(), window, cx);
                            }),
                        )
                        .child(
                            v_flex()
                                .size_full()
                                .items_center()
                                .justify_between()
                                .child(
                                    h_flex()
                                        .items_center()
                                        .gap_1()
                                        .child(self.render_session_badge(row, 34.0))
                                        .when_some(badge, |el, n| {
                                            el.child(
                                                div()
                                                    .px_1()
                                                    .rounded(RADIUS_CHIP)
                                                    .bg(theme::bg_canvas())
                                                    .text_xs()
                                                    .text_color(theme::accent_blue())
                                                    .child(n.to_string()),
                                            )
                                        }),
                                )
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .items_center()
                                        .when(row.favorite, |el| {
                                            el.child(lucide_icon(icons::STAR_FILLED, 14.0)
                                                .text_color(theme::accent_amber()))
                                        })
                                        .when(row.working, |el| {
                                            el.child(
                                                div()
                                                    .w(px(5.0))
                                                    .h(px(5.0))
                                                    .rounded(RADIUS_CHIP)
                                                    .bg(theme::accent_green()),
                                            )
                                        })
                                        .when(row.unread, |el| {
                                            el.child(
                                                div()
                                                    .w(px(5.0))
                                                    .h(px(5.0))
                                                    .rounded(RADIUS_CHIP)
                                                    .bg(theme::accent_blue()),
                                            )
                                        }),
                                )
                                .child(
                                    div()
                                        .w_full()
                                        .text_xs()
                                        .font_weight(if row.unread {
                                            FontWeight::SEMIBOLD
                                        } else {
                                            FontWeight::NORMAL
                                        })
                                        .text_color(theme::text_primary())
                                        .truncate()
                                        .child(rename_draft.unwrap_or_else(|| {
                                            gpui_kit::StyledText::new(row.title.clone())
                                        })),
                                ),
                        ),
                );
            }
            sessions_content = sessions_content.child(pin_grid);
        }

        // 2. Collapsible Named Groups (Ctrl+G)
        for (g_idx, gv) in layout.groups.iter().enumerate() {
            let gid = gv.group.id.clone();
            let collapsed = gv.group.collapsed;
            let dot_color = gv.group.color.as_deref().and_then(parse_hex_color);
            let mut grp_box = v_flex().gap_1().child(
                div()
                    .id(SharedString::from(format!("sidebar-group-{g_idx}")))
                    .role(Role::Button)
                    .test_support()
                    .px_2()
                    .py_1()
                    .rounded(RADIUS_ROW)
                    .hover(|s| s.bg(theme::bg_hover()))
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.sidebar_state
                                .workspace_store
                                .toggle_group_collapsed(&gid);
                            this.sidebar_state.rename = None;
                            this.cleanup_unmounted_surface_state();
                            this.persist_workspace_store_if_enabled();
                            this.sync_workspace_to_shared(cx);
                            cx.notify();
                        }),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .justify_between()
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_1p5()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme::text_muted())
                                            .child(lucide_icon(
                                                if collapsed { icons::CHEVRON_RIGHT } else { icons::CHEVRON_DOWN },
                                                12.0,
                                            )),
                                    )
                                    .when_some(dot_color, |el, col| {
                                        el.child(
                                            div()
                                                .w(px(6.0))
                                                .h(px(6.0))
                                                .rounded(RADIUS_CHIP)
                                                .bg(col),
                                        )
                                    })
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(theme::text_secondary())
                                            .child(gv.group.name.clone()),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::text_muted())
                                    .child(gv.items.len().to_string()),
                            ),
                    ),
            );

            if !collapsed {
                for (m_idx, row) in gv.items.iter().enumerate() {
                    let badge = if jump_badges {
                        jump_index_for(&row.session_key)
                    } else {
                        None
                    };
                    grp_box = grp_box.child(self.render_sidebar_session_row(
                        SharedString::from(format!("sidebar-group-{g_idx}-item-{m_idx}")),
                        row,
                        active_key.as_deref() == Some(row.session_key.as_str()),
                        badge,
                        in_starred,
                        cx,
                    ));
                }
            }
            sessions_content = sessions_content.child(grp_box);
        }

        // 3. Recents Section
        let mut recents_section = v_flex().gap_1().when(
            self.sidebar_state.active_workspace_id != ALL_WORKSPACES_ID,
            |el| {
                el.child(
                    div()
                        .id("sidebar-recents-header")
                        .role(Role::Button)
                        .test_support()
                        .px_1()
                        .py_0p5()
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                let currently_collapsed = this.sidebar_state.recents_collapsed
                                    || this.sidebar_state.workspace_store.recents_section_collapsed;
                                let next = !currently_collapsed;
                                this.sidebar_state.recents_collapsed = next;
                                this.sidebar_state.workspace_store.recents_section_collapsed = next;
                                this.sidebar_state.rename = None;
                                this.cleanup_unmounted_surface_state();
                                this.persist_workspace_store_if_enabled();
                                this.sync_workspace_to_shared(cx);
                                cx.notify();
                            }),
                        )
                        .child(div().text_color(theme::text_muted()).child(lucide_icon(
                            if self.sidebar_state.recents_collapsed
                                || self.sidebar_state.workspace_store.recents_section_collapsed
                            {
                                icons::CHEVRON_RIGHT
                            } else {
                                icons::CHEVRON_DOWN
                            },
                            12.0,
                        ))),
                )
            },
        );

        if self.sidebar_state.active_workspace_id == ALL_WORKSPACES_ID
            || (!self.sidebar_state.recents_collapsed
                && !self.sidebar_state.workspace_store.recents_section_collapsed)
        {
            if self.sessions.is_empty() {
                recents_section = recents_section.child(
                    div()
                        .px_3()
                        .py_3()
                        .rounded(RADIUS_ROW)
                        .bg(theme::bg_surface())
                        .text_xs()
                        .text_color(theme::text_muted())
                        .child("Connect to a VPS or local daemon to stream Claude, Antigravity, Codex, Pi & Cursor chats."),
                );
            } else if layout.recents.is_empty()
                && layout.pinned_tiles.is_empty()
                && layout.groups.is_empty()
            {
                recents_section = recents_section.child(
                    div()
                        .px_3()
                        .py_3()
                        .rounded(RADIUS_ROW)
                        .bg(theme::bg_surface())
                        .text_xs()
                        .text_color(theme::text_muted())
                        .child(if self.sidebar_state.unread_only {
                            "No unread sessions in this workspace."
                        } else if self.sidebar_state.active_workspace_view
                            == WorkspaceView::Frequent
                        {
                            "Open or message a session to see it here."
                        } else if in_starred {
                            "Star a session with Alt+S to pin it into Starred."
                        } else {
                            "No sessions in this workspace yet."
                        }),
                );
            } else {
                for (idx, row) in layout.recents.iter().enumerate() {
                    let badge = if jump_badges {
                        jump_index_for(&row.session_key)
                    } else {
                        None
                    };
                    recents_section = recents_section.child(self.render_sidebar_session_row(
                        SharedString::from(format!("session-{idx}")),
                        row,
                        active_key.as_deref() == Some(row.session_key.as_str()),
                        badge,
                        in_starred,
                        cx,
                    ));
                }
            }
        }
        sessions_content = sessions_content.child(recents_section);

        v_flex()
            .id("sidebar-open")
            .test_support()
            .w(px(280.0))
            .h_full()
            .bg(theme::bg_sidebar())
            .border_r_1()
            .border_color(theme::border_subtle())
            .p_3()
            .gap_2p5()
            .when_some(self.sidebar_state.focus_handle.as_ref(), |el, fh| {
                el.track_focus(fh)
            })
            .on_action(cx.listener(|this, _: &CloseOverlay, window, cx| {
                if this.table_modal_state.is_open()
                    || this.active_modal != ActiveModal::None
                    || this.has_mounted_chat_overlay()
                {
                    cx.propagate();
                    return;
                }
                if this.sidebar_state.workspace_menu_open {
                    this.sidebar_state.workspace_menu_open = false;
                    this.focus_active_surface_default(window, cx);
                    cx.notify();
                } else if this.is_sidebar_rename_mounted() {
                    this.sidebar_state.rename = None;
                    this.focus_active_surface_default(window, cx);
                    cx.notify();
                } else if this.is_sidebar_workspace_draft_mounted() {
                    this.sidebar_state.new_workspace_draft = None;
                    this.focus_active_surface_default(window, cx);
                    cx.notify();
                } else {
                    if this.sidebar_state.rename.is_some() {
                        this.sidebar_state.rename = None;
                    }
                    cx.propagate();
                }
            }))
            .on_modifiers_changed(cx.listener(|this, ev: &ModifiersChangedEvent, _, cx| {
                this.handle_modifiers_changed(ev, cx);
            }))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                let had_rename = this.sidebar_state.rename.is_some();
                let had_draft = this.sidebar_state.new_workspace_draft.is_some();
                this.handle_sidebar_keydown(ev, cx);
                let ended = (had_rename && this.sidebar_state.rename.is_none())
                    || (had_draft && this.sidebar_state.new_workspace_draft.is_none());
                if ended {
                    this.focus_active_surface_default(window, cx);
                }
            }))
            .child(ws_strip)
            .when_some(
                self.sidebar_state.new_workspace_draft.as_ref(),
                |el, draft| {
                    el.child(
                        div()
                            .id("workspace-name-draft")
                            .test_support()
                            .p_2()
                            .rounded(RADIUS_ROW)
                            .bg(theme::bg_surface())
                            .text_sm()
                            .child(format!("Workspace name: {draft}|")),
                    )
                },
            )
            .when(self.sidebar_state.unread_only, |el| {
                el.child(
                    div()
                        .id("sidebar-unread-only-pill")
                        .role(Role::Button)
                        .test_support()
                        .px_2p5()
                        .py_1()
                        .rounded(RADIUS_CHIP)
                        .bg(theme::bg_surface())
                        .border_1()
                        .border_color(theme::accent_blue())
                        .text_xs()
                        .text_color(theme::accent_blue())
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.toggle_unread_only(cx);
                            }),
                        )
                        .flex()
                        .items_center()
                        .gap_1()
                        .child("UNREAD ONLY")
                        .child(lucide_icon(icons::X, 12.0)),
                )
            })
            .child(
                div()
                    .id("sessions-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(sessions_content),
            )
            .when_some(self.render_update_card(false, cx), |el, card| {
                el.child(card)
            })
            .child(
                div()
                    .id("gpu-status-card")
                    .role(Role::Button)
                    .test_support()
                    .px_3()
                    .py_2()
                    .rounded(RADIUS_ROW)
                    .border_1()
                    .border_color(if gpu_safe {
                        theme::accent_green()
                    } else {
                        theme::accent_amber()
                    })
                    .bg(theme::bg_surface())
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            cx.stop_propagation();
                            if this.table_modal_state.is_open() {
                                this.table_modal_state.close();
                            }
                            this.settings_tab = crate::settings::SettingsTab::Gpu;
                            this.set_main_surface(MainSurface::Settings);
                            this.active_modal = ActiveModal::None;
                            this.focus_active_surface_default(window, cx);
                            cx.notify();
                        }),
                    )
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(if gpu_safe {
                                        theme::accent_green()
                                    } else {
                                        theme::accent_amber()
                                    })
                                    .child(format!("GPU: {gpu_label}")),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::text_muted())
                                    .child(format!(
                                        "v{} · {} · {}",
                                        multibot_proto::build_version(),
                                        self.footprint_label,
                                        self.connection_status
                                    )),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_sidebar_session_row(
        &self,
        id: SharedString,
        row: &SidebarRow,
        is_selected: bool,
        jump_digit: Option<usize>,
        in_starred: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let key_left = row.session_key.clone();
        let key_right = row.session_key.clone();
        let is_renaming = self
            .sidebar_state
            .rename
            .as_ref()
            .is_some_and(|r| r.session_key == row.session_key);
        let rename_draft = if is_renaming {
            self.sidebar_state
                .rename
                .as_ref()
                .map(InlineRenameState::render_text)
        } else {
            None
        };
        let subtitle = self
            .find_session_by_key(&row.session_key)
            .and_then(|s| s.last_assistant_preview)
            .filter(|p| !p.trim().is_empty())
            .or_else(|| {
                if in_starred {
                    Some(row.workspace_name.clone())
                } else {
                    row.cwd.clone()
                }
            });

        div()
            .id(id)
            .role(Role::Button)
            .test_support()
            .px_2p5()
            .py_2()
            .rounded(RADIUS_ROW)
            .bg(if is_selected {
                theme::bg_active_row()
            } else {
                theme::bg_sidebar()
            })
            .hover(|s| s.bg(theme::bg_hover()))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    if let Some((host, sess)) = this.host_for_session_key(&key_left) {
                        this.open_session_on_host(host, sess, false, cx);
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, window, cx| {
                    this.open_palette_tab_context(key_right.clone(), window, cx);
                }),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_2p5()
                    .child(self.render_session_badge(row, 26.0))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(if let Some(draft_text) = rename_draft {
                                div()
                                    .id("sidebar-rename-input")
                                    .px_1p5()
                                    .py_0p5()
                                    .rounded(px(6.0))
                                    .bg(theme::bg_surface())
                                    .border_1()
                                    .border_color(theme::accent_blue())
                                    .text_sm()
                                    .text_color(theme::text_primary())
                                    .truncate()
                                    .child(draft_text)
                                    .into_any_element()
                            } else {
                                div()
                                    .text_sm()
                                    .font_weight(if row.unread {
                                        FontWeight::SEMIBOLD
                                    } else {
                                        FontWeight::NORMAL
                                    })
                                    .text_color(if row.muted {
                                        theme::text_muted()
                                    } else {
                                        theme::text_primary()
                                    })
                                    .truncate()
                                    .child(row.title.clone())
                                    .into_any_element()
                            })
                            .when_some(subtitle, |el, sub| {
                                el.child(
                                    div()
                                        .text_xs()
                                        .text_color(theme::text_muted())
                                        .truncate()
                                        .child(sub),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1()
                            .when(row.favorite, |el| {
                                el.child(lucide_icon(icons::STAR_FILLED, 14.0)
                                    .text_color(theme::accent_amber()))
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
                            })
                            .when_some(jump_digit, |el, digit| {
                                el.child(
                                    div()
                                        .px_1p5()
                                        .py_0p5()
                                        .rounded(RADIUS_CHIP)
                                        .bg(theme::bg_surface())
                                        .text_xs()
                                        .text_color(theme::accent_blue())
                                        .child(digit.to_string()),
                                )
                            }),
                    ),
            )
    }

    fn render_collapsed_sidebar(
        &self,
        layout: multibot_core::workspaces::SidebarLayout,
        unread_count: usize,
        jump_badges: bool,
        now: i64,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (_, gpu_safe) = self.active_gpu_summary();
        let active_key = self
            .selected_session
            .as_ref()
            .map(|s| self.session_key_for(s));

        // Derive 1..9 jump badges from layout.ctrl_digit_order (matching Open sidebar & Ctrl+1..9)
        let ctrl_digit_order = layout.ctrl_digit_order;
        let jump_index_for = |session_key: &str| -> Option<usize> {
            ctrl_digit_order
                .iter()
                .position(|k| k == session_key)
                .filter(|&i| i < 9)
                .map(|i| i + 1)
        };

        // Flatten visible rail avatars: non-collapsed pinned -> non-collapsed groups -> non-collapsed recents
        let mut rail_rows: Vec<SidebarRow> = Vec::new();
        if !self.sidebar_state.workspace_store.pinned_section_collapsed {
            rail_rows.extend(layout.pinned_tiles);
        }
        for gv in layout.groups {
            if !gv.group.collapsed {
                rail_rows.extend(gv.items);
            }
        }
        if self.sidebar_state.active_workspace_view == WorkspaceView::Frequent
            || (!self.sidebar_state.recents_collapsed
                && !self.sidebar_state.workspace_store.recents_section_collapsed)
        {
            rail_rows.extend(layout.recents);
        }

        let mut ws_rail = v_flex().gap_1().items_center();
        let in_starred = self.sidebar_state.active_workspace_id == STARRED_WORKSPACE_ID;
        ws_rail = ws_rail.child(
            div()
                .id("ws-pill-starred")
                .role(Role::Button)
                .test_support()
                .w(px(28.0))
                .h(px(24.0))
                .rounded(RADIUS_CHIP)
                .bg(if in_starred {
                    theme::bg_active_row()
                } else {
                    theme::bg_surface()
                })
                .border_1()
                .border_color(if in_starred {
                    theme::accent_amber()
                } else {
                    theme::border_subtle()
                })
                .flex()
                .items_center()
                .justify_center()
                .text_xs()
                .text_color(theme::accent_amber())
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.sidebar_state.active_workspace_id = STARRED_WORKSPACE_ID.to_string();
                        this.sidebar_state.rename = None;
                        this.cleanup_unmounted_surface_state();
                        cx.notify();
                    }),
                )
                .child(lucide_icon(icons::STAR_FILLED, 14.0)),
        );

        for (idx, ws) in self
            .sidebar_state
            .workspace_store
            .visible_workspaces(now)
            .into_iter()
            .take(5)
            .enumerate()
        {
            let active = self.sidebar_state.active_workspace_id == ws.id;
            let ws_id = ws.id.clone();
            let initial: String = ws
                .name
                .chars()
                .next()
                .map(|c| c.to_ascii_uppercase().to_string())
                .unwrap_or_else(|| "W".into());
            let accent = ws
                .color
                .as_deref()
                .and_then(parse_hex_color)
                .unwrap_or_else(theme::text_secondary);
            ws_rail = ws_rail.child(
                div()
                    .id(SharedString::from(format!("ws-pill-{idx}")))
                    .role(Role::Button)
                    .test_support()
                    .w(px(28.0))
                    .h(px(24.0))
                    .rounded(RADIUS_CHIP)
                    .bg(if active {
                        theme::bg_active_row()
                    } else {
                        theme::bg_surface()
                    })
                    .border_1()
                    .border_color(if active {
                        accent
                    } else {
                        theme::border_subtle()
                    })
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_xs()
                    .text_color(if active {
                        theme::text_primary()
                    } else {
                        theme::text_muted()
                    })
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.sidebar_state.active_workspace_id = ws_id.clone();
                            this.sidebar_state.rename = None;
                            this.cleanup_unmounted_surface_state();
                            cx.notify();
                        }),
                    )
                    .child(initial),
            );
        }

        let mut avatars = v_flex().gap_1p5().items_center();
        for (idx, row) in rail_rows.iter().enumerate() {
            let is_selected = active_key.as_deref() == Some(row.session_key.as_str());
            let key_left = row.session_key.clone();
            let key_right = row.session_key.clone();
            let badge = if jump_badges {
                jump_index_for(&row.session_key).map(|n| n.to_string())
            } else {
                None
            };
            let dot_color = if row.working {
                Some(theme::accent_green())
            } else if row.unread {
                Some(theme::accent_blue())
            } else if row.favorite {
                Some(theme::accent_amber())
            } else {
                None
            };

            avatars = avatars.child(
                div()
                    .id(SharedString::from(format!("rail-session-{idx}")))
                    .role(Role::Button)
                    .test_support()
                    .relative()
                    .w(px(34.0))
                    .h(px(34.0))
                    .rounded(RADIUS_CHIP)
                    .bg(Self::agent_color(row.agent))
                    .border_1()
                    .border_color(if is_selected {
                        theme::text_primary()
                    } else {
                        theme::bg_sidebar()
                    })
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_xs()
                    .text_color(theme::bg_canvas())
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            if let Some((host, sess)) = this.host_for_session_key(&key_left) {
                                this.open_session_on_host(host, sess, false, cx);
                            }
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, _, window, cx| {
                            this.open_palette_tab_context(key_right.clone(), window, cx);
                        }),
                    )
                    .child(if let Some(digit) = badge {
                        div().child(digit)
                    } else {
                        self.render_session_badge(row, 32.0)
                    })
                    .when_some(dot_color, |el, col| {
                        el.child(
                            div()
                                .absolute()
                                .right_0()
                                .bottom_0()
                                .w(px(8.0))
                                .h(px(8.0))
                                .rounded(RADIUS_CHIP)
                                .bg(col)
                                .border_1()
                                .border_color(theme::bg_sidebar()),
                        )
                    }),
            );
        }

        v_flex()
            .id("sidebar-rail")
            .test_support()
            .w(px(56.0))
            .h_full()
            .bg(theme::bg_sidebar())
            .border_r_1()
            .border_color(theme::border_subtle())
            .py_2()
            .px_1p5()
            .gap_2()
            .items_center()
            .on_modifiers_changed(cx.listener(|this, ev: &ModifiersChangedEvent, _, cx| {
                this.handle_modifiers_changed(ev, cx);
            }))
            .child(
                Button::new("open-palette-btn")
                    .xsmall()
                    .icon(lucide_icon(icons::COMMAND, 16.0))
                    .label("K")
                    .accessibility_label("Command palette")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_palette(window, cx);
                    })),
            )
            .when(self.main_surface != MainSurface::Inbox, |el| {
                el.child(
                    div()
                        .id("sidebar-inbox-btn")
                        .role(Role::Button)
                        .test_support()
                        .w(px(32.0))
                        .h(px(26.0))
                        .rounded(RADIUS_CHIP)
                        .bg(theme::bg_surface())
                        .border_1()
                        .border_color(if unread_count > 0 {
                            theme::accent_blue()
                        } else {
                            theme::border_subtle()
                        })
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_xs()
                        .text_color(if unread_count > 0 {
                            theme::accent_blue()
                        } else {
                            theme::text_muted()
                        })
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                cx.stop_propagation();
                                if this.table_modal_state.is_open() {
                                    this.table_modal_state.close();
                                }
                                this.active_modal = ActiveModal::None;
                                this.set_main_surface(MainSurface::Inbox);
                                this.focus_inbox(window, cx);
                                cx.notify();
                            }),
                        )
                        .child(if unread_count > 0 {
                            unread_count.to_string()
                        } else {
                            "In".to_string()
                        }),
                )
            })
            .child(ws_rail)
            .child(
                div()
                    .id("sessions-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(avatars),
            )
            // Update card intentionally omitted in Collapsed rail mode to prevent overflow.
            .child(
                div()
                    .id("gpu-status-card")
                    .role(Role::Button)
                    .test_support()
                    .w(px(28.0))
                    .h(px(28.0))
                    .rounded(RADIUS_CHIP)
                    .bg(theme::bg_surface())
                    .border_1()
                    .border_color(if gpu_safe {
                        theme::accent_green()
                    } else {
                        theme::accent_amber()
                    })
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_xs()
                    .text_color(if gpu_safe {
                        theme::accent_green()
                    } else {
                        theme::accent_amber()
                    })
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            cx.stop_propagation();
                            if this.table_modal_state.is_open() {
                                this.table_modal_state.close();
                            }
                            this.settings_tab = crate::settings::SettingsTab::Gpu;
                            this.set_main_surface(MainSurface::Settings);
                            this.active_modal = ActiveModal::None;
                            this.focus_active_surface_default(window, cx);
                            cx.notify();
                        }),
                    )
                    .child("G"),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{AppContext, Entity, TestAppContext, WindowOptions, test::TestWindowExt};
    use multibot_core::workspaces::DEFAULT_WORKSPACE_ID;
    use multibot_core::workspaces::canonicalize_session_title;
    use multibot_proto::AgentKind;
    use std::sync::Arc;

    fn sample_sessions() -> Vec<SessionSummary> {
        vec![
            SessionSummary {
                id: "s1".into(),
                agent: AgentKind::Claude,
                title: "Pinned Chat".into(),
                cwd: Some("/repo/app".into()),
                source_path: "/tmp/s1.jsonl".into(),
                updated_at_ms: 5_000,
                working: true,
                needs_attention: true,
                last_assistant_id: Some("a1".into()),
                last_assistant_ms: Some(5_000),
                ..Default::default()
            },
            SessionSummary {
                id: "s2".into(),
                agent: AgentKind::Codex,
                title: "Grouped Chat".into(),
                cwd: Some("/repo/api".into()),
                source_path: "/tmp/s2.jsonl".into(),
                updated_at_ms: 4_000,
                needs_attention: true,
                last_assistant_id: Some("a2".into()),
                last_assistant_ms: Some(4_000),
                ..Default::default()
            },
            SessionSummary {
                id: "s3".into(),
                agent: AgentKind::Antigravity,
                title: "Recent Read Chat".into(),
                cwd: Some("/repo/ui".into()),
                source_path: "/tmp/s3.jsonl".into(),
                updated_at_ms: 3_000,
                needs_attention: false,
                ..Default::default()
            },
        ]
    }

    fn open_view(cx: &mut TestAppContext) -> (gpui_kit::AnyWindowHandle, Entity<MultibotView>) {
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
        cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, move |window, cx| {
                cx.new(|cx| {
                    let mut view = MultibotView::new(rt, window, cx);
                    view.sidebar_state.active_workspace_id = DEFAULT_WORKSPACE_ID.to_string();
                    view
                })
            })
            .unwrap()
        })
    }

    #[test]
    fn sidebar_3_state_cycle() {
        let mut state = SidebarState::default();
        assert_eq!(state.mode, SidebarMode::Open);
        assert!(state.is_shown());

        state.cycle();
        assert_eq!(state.mode, SidebarMode::Collapsed);
        assert!(state.is_shown());

        state.cycle();
        assert_eq!(state.mode, SidebarMode::Hidden);
        assert!(!state.is_shown());

        state.cycle();
        assert_eq!(state.mode, SidebarMode::Open);
        assert!(state.is_shown());
    }

    #[test]
    fn jump_badges_are_altgr_safe() {
        assert!(should_show_jump_badges(Modifiers {
            control: true,
            ..Default::default()
        }));
        // AltGr = Ctrl+Alt -> MUST return false
        assert!(!should_show_jump_badges(Modifiers {
            control: true,
            alt: true,
            ..Default::default()
        }));
        assert!(!should_show_jump_badges(Modifiers {
            control: true,
            shift: true,
            ..Default::default()
        }));
        assert!(!should_show_jump_badges(Modifiers {
            control: true,
            platform: true,
            ..Default::default()
        }));
        assert!(!should_show_jump_badges(Modifiers::default()));
    }

    #[test]
    fn inline_rename_handles_keys_and_canonical_rules() {
        let mut r = InlineRenameState::new("local:claude:s1".into(), "s1".into(), "Old".into());
        assert_eq!(
            r.handle_key("backspace", None, Modifiers::default()),
            RenameKeyOutcome::Consumed
        );
        assert_eq!(r.draft, "");
        assert_eq!(
            r.handle_key("k", Some("k"), Modifiers::default()),
            RenameKeyOutcome::Consumed
        );
        assert_eq!(r.draft, "k");
        assert_eq!(
            r.handle_key("enter", None, Modifiers::default()),
            RenameKeyOutcome::Commit
        );
        assert_eq!(
            r.handle_key("escape", None, Modifiers::default()),
            RenameKeyOutcome::Cancel
        );

        // Blank rejected, >63 Unicode chars capped
        assert_eq!(canonicalize_session_title("   "), None);
        let long = "日".repeat(80);
        assert_eq!(canonicalize_session_title(&long).unwrap().chars().count(), 63);
    }

    #[gpui_kit::test]
    fn renders_pinned_groups_recents_and_cycles_three_modes(cx: &mut TestAppContext) {
        let (handle, view) = open_view(cx);
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.sessions = sample_sessions();
                let k1 = v.session_key_for(&v.sessions[0]);
                let k2 = v.session_key_for(&v.sessions[1]);
                v.sidebar_state.workspace_store.toggle_pin(&k1, false);
                v.sidebar_state.workspace_store.toggle_favorite(&k1);
                v.sidebar_state.workspace_store.toggle_group_for_session(
                    &k2,
                    DEFAULT_WORKSPACE_ID,
                    None,
                    "Backend",
                );
                v.sidebar_state.unread_only = true;
                cx.notify();
            });
            window.render_frame(cx);

            // Pinned grid rendered, group rendered, unread-only pill rendered
            assert!(window.try_find("sidebar-pinned-grid").is_some());
            assert!(window.try_find("sidebar-group-0").is_some());
            assert!(window.try_find("sidebar-unread-only-pill").is_some());

            // Toggle group collapse and turn off unread-only
            window.click("sidebar-group-0", cx);
            window.render_frame(cx);
            window.click("sidebar-unread-only-pill", cx);
            window.render_frame(cx);
            assert!(window.try_find("sidebar-unread-only-pill").is_none());

            // Switch to Starred workspace and create a new workspace
            window.click("workspace-dropdown", cx);
            window.render_frame(cx);
            window.click("workspace-option-2", cx);
            window.render_frame(cx);
            window.click("workspace-dropdown", cx);
            window.render_frame(cx);
            window.click("workspace-new", cx);
            window.render_frame(cx);
            window.press("w", cx);
            window.press("enter", cx);
            window.render_frame(cx);

            // Cycle Open -> Collapsed -> Hidden -> Open
            window.press("ctrl-shift-b", cx);
            window.render_frame(cx);
            assert!(window.try_find("sidebar-rail").is_some());
            assert!(window.try_find("gpu-status-card").is_some());

            window.press("ctrl-shift-b", cx);
            window.render_frame(cx);
            assert!(window.try_find("sidebar-rail").is_none());
            assert!(window.try_find("sidebar-open").is_none());

            window.press("ctrl-shift-b", cx);
            window.render_frame(cx);
            assert!(window.try_find("sidebar-open").is_some());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn sidebar_clicks_and_ctrl_digits_select_in_place(cx: &mut TestAppContext) {
        let (handle, view) = open_view(cx);
        cx.update_window(handle, |_, window, cx| {
            for workspace_id in [ALL_WORKSPACES_ID, DEFAULT_WORKSPACE_ID] {
                for auto_sort in [true, false] {
                    let keys = view.update(cx, |v, cx| {
                        v.sidebar_state.workspace_store = WorkspaceStore::new();
                        v.sidebar_state.workspace_store.auto_sort = auto_sort;
                        v.sidebar_state.active_workspace_id = workspace_id.to_string();
                        v.sessions = (0..9)
                            .map(|i| SessionSummary {
                                id: format!("select-{i}"),
                                title: format!("Session {i}"),
                                agent: AgentKind::Claude,
                                updated_at_ms: 10_000 - i as i64,
                                needs_attention: true,
                                ..Default::default()
                            })
                            .collect();
                        let keys: Vec<_> =
                            v.sessions.iter().map(|s| v.session_key_for(s)).collect();
                        v.select_session(v.sessions[0].clone(), cx);
                        keys
                    });
                    window.render_frame(cx);

                    for click in [true, false] {
                        for index in (0..9).rev() {
                            if click {
                                window.click(format!("session-{index}"), cx);
                            } else {
                                window.press(&format!("ctrl-{}", index + 1), cx);
                            }
                            window.render_frame(cx);
                            view.read_with(cx, |v, _| {
                                let session = v.selected_session.as_ref().unwrap();
                                let active = v.session_key_for(session);
                                assert_eq!(active, keys[index]);
                                let store = &v.sidebar_state.workspace_store;
                                let layout = store.compute_sidebar_layout(
                                    workspace_id,
                                    &v.session_inputs(),
                                    false,
                                    now_unix_ms(),
                                );
                                assert_eq!(layout.flat_display_order, keys);
                                assert_eq!(layout.ctrl_digit_order, keys);
                                assert_eq!(store.recent_views[0].session_key, active);
                                assert!(!store.session_meta[&active].frequent_visits_ms.is_empty());
                                assert!(!store.is_session_unread(&active, session));
                            });
                        }
                    }
                }
            }
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn ctrl_tab_cycles_every_visible_session_without_reordering(cx: &mut TestAppContext) {
        let (handle, view) = open_view(cx);
        cx.update_window(handle, |_, window, cx| {
            for count in [4, 12] {
                for workspace_id in [ALL_WORKSPACES_ID, DEFAULT_WORKSPACE_ID] {
                    for unread_only in [false, true] {
                        let (keys, before) = view.update(cx, |v, cx| {
                            v.sidebar_state.workspace_store = WorkspaceStore::new();
                            v.sidebar_state.active_workspace_id = workspace_id.to_string();
                            v.sidebar_state.unread_only = unread_only;
                            v.sessions = (0..count)
                                .map(|i| SessionSummary {
                                    id: format!("cycle-{i}"),
                                    title: format!("Session {i}"),
                                    agent: AgentKind::Claude,
                                    updated_at_ms: 10_000 - i as i64,
                                    needs_attention: true,
                                    ..Default::default()
                                })
                                .collect();
                            let keys: Vec<_> =
                                v.sessions.iter().map(|s| v.session_key_for(s)).collect();
                            for (i, key) in keys.iter().enumerate() {
                                v.sidebar_state
                                    .workspace_store
                                    .record_session_view(key, 20_000 - i as i64);
                            }
                            v.select_session(v.sessions[0].clone(), cx);
                            (keys, v.sidebar_state.workspace_store.clone())
                        });
                        window.render_frame(cx);

                        for (chord, indices) in [
                            ("ctrl-tab", (1..count).chain([0]).collect::<Vec<_>>()),
                            ("ctrl-shift-tab", (1..count).rev().chain([0]).collect()),
                        ] {
                            for index in indices {
                                window.press(chord, cx);
                                window.render_frame(cx);
                                view.read_with(cx, |v, _| {
                                    let active =
                                        v.session_key_for(v.selected_session.as_ref().unwrap());
                                    assert_eq!(active, keys[index], "{chord}, {workspace_id}");
                                    let layout = v
                                        .sidebar_state
                                        .workspace_store
                                        .compute_sidebar_layout_with_options(
                                            workspace_id,
                                            v.sidebar_state.active_workspace_view,
                                            &v.session_inputs(),
                                            unread_only,
                                            Some(&active),
                                            now_unix_ms(),
                                        );
                                    assert_eq!(layout.ctrl_digit_order, keys);
                                    let mut without_visits =
                                        v.sidebar_state.workspace_store.clone();
                                    assert!(
                                        !without_visits.session_meta[&active]
                                            .frequent_visits_ms
                                            .is_empty()
                                    );
                                    for (key, meta) in &mut without_visits.session_meta {
                                        let old = &before.session_meta[key];
                                        meta.frequent_visits_ms = old.frequent_visits_ms.clone();
                                        meta.last_used_ms = old.last_used_ms;
                                    }
                                    assert_eq!(without_visits, before);
                                });
                            }
                        }
                    }
                }
            }
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn ctrl_tab_does_not_open_sessions_in_collapsed_sections(cx: &mut TestAppContext) {
        let (handle, view) = open_view(cx);
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.sessions = sample_sessions();
                v.sidebar_state.workspace_store.recents_section_collapsed = true;
                v.sidebar_state.recents_collapsed = true;
                v.select_session(v.sessions[0].clone(), cx);
            });
            window.render_frame(cx);
            for chord in ["ctrl-tab", "ctrl-shift-tab"] {
                window.press(chord, cx);
                window.render_frame(cx);
                view.read_with(cx, |v, _| {
                    assert_eq!(v.selected_session.as_ref().unwrap().id, "s1");
                    assert!(v.sidebar_state.workspace_store.recent_views.is_empty());
                });
            }
        })
        .unwrap();
    }

    #[test]
    fn inline_rename_select_all_replacement_and_deletion() {
        for key_char in [Some("日"), None] {
            let mut r = InlineRenameState::new("k".into(), "s1".into(), "a".repeat(63));
            assert!(r.all_selected);
            assert_eq!(
                r.handle_key("shift", None, Modifiers::default()),
                RenameKeyOutcome::Ignored
            );
            r.handle_key("x", key_char, Modifiers::default());
            assert_eq!(r.draft, key_char.unwrap_or("x"));
            assert!(!r.all_selected);
            r.handle_key("y", Some("y"), Modifiers::default());
            assert_eq!(r.draft, format!("{}y", key_char.unwrap_or("x")));
        }
        for key in ["backspace", "delete"] {
            let mut r = InlineRenameState::new("k".into(), "s1".into(), "Old 日".into());
            assert_eq!(
                r.handle_key(key, None, Modifiers::default()),
                RenameKeyOutcome::Consumed
            );
            assert!(r.draft.is_empty());
            assert!(!r.all_selected);
        }
    }

    #[test]
    fn inline_rename_consumes_printable_keys_at_63_char_cap() {
        let mut r = InlineRenameState::new("k".into(), "s1".into(), "Old".into());
        r.handle_key("a", Some(&"a".repeat(63)), Modifiers::default());
        assert_eq!(
            r.handle_key("q", Some("q"), Modifiers::default()),
            RenameKeyOutcome::Consumed
        );
        assert_eq!(r.draft.chars().count(), 63);
        assert_eq!(
            r.handle_key("j", None, Modifiers::default()),
            RenameKeyOutcome::Consumed
        );
        assert_eq!(
            r.handle_key("e", Some("日"), Modifiers::default()),
            RenameKeyOutcome::Consumed
        );
        // Ctrl chord is still Ignored so system shortcuts can work
        assert_eq!(
            r.handle_key(
                "k",
                None,
                Modifiers {
                    control: true,
                    ..Default::default()
                }
            ),
            RenameKeyOutcome::Ignored
        );
    }

    #[gpui_kit::test]
    fn ctrl_g_collapses_group_without_ungrouping_and_collapsed_rail_matches_ctrl_digit_order(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_view(cx);
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.sessions = sample_sessions();
                let k1 = v.session_key_for(&v.sessions[0]);
                let k2 = v.session_key_for(&v.sessions[1]);
                let k3 = v.session_key_for(&v.sessions[2]);
                v.selected_session = Some(v.sessions[2].clone());

                // Ungrouped session: toggle_active_group is a no-op
                v.toggle_active_group(cx);
                assert!(v.sidebar_state.workspace_store.groups.is_empty());

                // Group s1 & s2; select s1; Ctrl+G collapses group without removing membership
                let gid = v
                    .sidebar_state
                    .workspace_store
                    .toggle_group_for_session(&k1, DEFAULT_WORKSPACE_ID, None, "Team")
                    .unwrap();
                v.sidebar_state.workspace_store.toggle_group_for_session(
                    &k2,
                    DEFAULT_WORKSPACE_ID,
                    Some(&gid),
                    "Team",
                );
                v.selected_session = Some(v.sessions[0].clone());
                v.toggle_active_group(cx);
                assert!(
                    v.sidebar_state
                        .workspace_store
                        .groups
                        .iter()
                        .find(|g| g.id == gid)
                        .unwrap()
                        .collapsed
                );
                assert_eq!(
                    v.sidebar_state.workspace_store.session_meta[&k1]
                        .group_id
                        .as_deref(),
                    Some(gid.as_str())
                );

                // In Collapsed rail with jump badges ON, only s3 is visible with badge "1"
                v.sidebar_state.set_mode(SidebarMode::Collapsed);
                v.sidebar_state.show_jump_badges = true;
                let _ = k3;
                cx.notify();
            });
            window.render_frame(cx);

            assert!(window.try_find("rail-session-0").is_some());
            assert!(window.try_find("rail-session-1").is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn sidebar_rename_focus_and_close_overlay_cancels_rename_without_closing_inbox(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_view(cx);
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.sessions = sample_sessions();
                v.main_surface = MainSurface::Inbox;
                let k1 = v.session_key_for(&v.sessions[0]);
                v.start_sidebar_rename_with_window(k1, window, cx);
                v.focus_inbox(window, cx);
            });
            window.render_frame(cx);
            assert!(
                view.read_with(cx, |v, _| v.sidebar_state.rename.is_none()),
                "The minimal Inbox hides the sidebar, so no Open-sidebar rename may mount there"
            );

            // Dispatch CloseOverlay (Escape) while the Inbox owns focus
            let fh = view.read_with(cx, |v, _| v.inbox_state.focus_handle.clone().unwrap());
            fh.dispatch_action(&CloseOverlay, window, cx);
            window.render_frame(cx);
            assert_eq!(
                view.read_with(cx, |v, _| v.main_surface),
                MainSurface::Chat
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn sidebar_recents_sync_pinned_collapse_and_modal_precedence(cx: &mut TestAppContext) {
        let (handle, view) = open_view(cx);
        cx.update_window(handle, |_, window, cx| {
            // (a) Persisted recents_section_collapsed = true while recents_collapsed = false:
            // a SINGLE click on sidebar-recents-header must expand Recents cleanly.
            // (b) pinned_section_collapsed = true hides Open sidebar's pinned tile grid.
            view.update(cx, |v, cx| {
                v.sessions = sample_sessions();
                let k1 = v.session_key_for(&v.sessions[0]);
                v.sidebar_state.workspace_store.toggle_pin(&k1, false);
                v.sidebar_state.workspace_store.recents_section_collapsed = true;
                v.sidebar_state.recents_collapsed = false;
                v.sidebar_state.workspace_store.pinned_section_collapsed = true;
                cx.notify();
            });
            window.render_frame(cx);

            assert!(window.try_find("sidebar-pinned-grid").is_none());
            assert!(window.try_find("session-0").is_none());

            // Single click on sidebar-recents-header expands Recents on the first click
            window.click("sidebar-recents-header", cx);
            window.render_frame(cx);
            view.read_with(cx, |v, _| {
                assert!(!v.sidebar_state.recents_collapsed);
                assert!(!v.sidebar_state.workspace_store.recents_section_collapsed);
            });
            assert!(window.try_find("session-0").is_some());

            // Uncollapsing pinned_section_collapsed restores sidebar-pinned-grid
            view.update(cx, |v, cx| {
                v.sidebar_state.workspace_store.pinned_section_collapsed = false;
                cx.notify();
            });
            window.render_frame(cx);
            assert!(window.try_find("sidebar-pinned-grid").is_some());

            // (c) Focused sidebar with active rename + ActiveModal::Shortcuts on top:
            // raw keydown must not mutate rename, and 1st CloseOverlay closes Shortcuts FIRST.
            view.update(cx, |v, cx| {
                let k2 = v.session_key_for(&v.sessions[1]);
                v.start_sidebar_rename_with_window(k2, window, cx);
                v.active_modal = ActiveModal::Shortcuts;
                cx.notify();
            });
            window.render_frame(cx);

            let orig_draft = view.read_with(cx, |v, _| {
                v.sidebar_state.rename.as_ref().unwrap().draft.clone()
            });

            // Raw keydown while Shortcuts is open must not touch rename draft
            view.update(cx, |v, cx| {
                let mut x_ks = gpui_kit::Keystroke::parse("x").unwrap();
                x_ks.key_char = Some("x".into());
                let ev_x = KeyDownEvent {
                    keystroke: x_ks,
                    is_held: false,
                    prefer_character_input: false,
                };
                v.handle_sidebar_keydown(&ev_x, cx);
                assert_eq!(
                    v.sidebar_state.rename.as_ref().unwrap().draft,
                    orig_draft
                );
            });

            let fh = view.read_with(cx, |v, _| v.sidebar_state.focus_handle.clone().unwrap());

            // 1st CloseOverlay closes Shortcuts modal while preserving sidebar rename
            fh.dispatch_action(&CloseOverlay, window, cx);
            window.render_frame(cx);
            view.read_with(cx, |v, _| {
                assert_eq!(v.active_modal, ActiveModal::None);
                assert!(v.sidebar_state.rename.is_some());
            });

            // 2nd CloseOverlay cancels sidebar rename
            fh.dispatch_action(&CloseOverlay, window, cx);
            window.render_frame(cx);
            view.read_with(cx, |v, _| {
                assert!(v.sidebar_state.rename.is_none());
            });
        })
        .unwrap();
    }
}
