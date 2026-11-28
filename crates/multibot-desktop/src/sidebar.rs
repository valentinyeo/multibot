#[path = "sidebar_rows.rs"]
mod rows;
#[path = "sidebar_workspaces.rs"]
mod workspaces;

use crate::app::{CloseOverlay, MultibotView};
use crate::icons::{self, lucide_icon};
use crate::state::{ActiveModal, MainSurface};
use crate::theme::{self, RADIUS_CHIP, RADIUS_ROW};
use gpui_kit::{
    Context, FocusHandle, FontWeight, Hsla, InteractiveElement, IntoElement, KeyDownEvent,
    Modifiers, ModifiersChangedEvent, MouseButton, ParentElement, Role, SharedString,
    StatefulInteractiveElement, Styled, TestSupportExt, Window,
    component::{Sizable, button::Button, h_flex, label::Label, v_flex},
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
use multibot_proto::{AgentKind, ClientRequest, SessionSummary};

fn session_subtitle(agent: AgentKind, cwd: Option<&str>, shell_preview: Option<&str>) -> String {
    let cli = match agent {
        AgentKind::Claude => "cld",
        AgentKind::Codex => "cdx",
        AgentKind::Antigravity => "agy",
        AgentKind::Pi => "pi",
        AgentKind::Cursor => "cur",
        AgentKind::Shell => shell_preview
            .and_then(|preview| preview.split(" · ").next())
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .unwrap_or("shl"),
    };
    let directory = cwd
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(|path| {
            let trimmed = path.trim_end_matches(['/', '\\']);
            if trimmed.is_empty() {
                path
            } else {
                trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed)
            }
        });
    match directory {
        Some(directory) => format!("{cli} · {directory}"),
        None => cli.to_string(),
    }
}

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
    pub dev_stats_expanded: bool,
    pub show_jump_badges: bool,
    pub previous_session_key: Option<String>,
    pub pending_shell_dissolve: Option<(String, std::time::Instant)>,
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
            dev_stats_expanded: false,
            show_jump_badges: false,
            previous_session_key: None,
            pending_shell_dissolve: None,
            rename: None,
            new_workspace_draft: None,
            persist_to_disk: false,
            focus_handle: None,
        }
    }
}

impl SidebarState {
    pub(crate) fn confirm_shell_dissolve(&mut self, key: &str, now: std::time::Instant) -> bool {
        if self
            .pending_shell_dissolve
            .take()
            .is_some_and(|(pending, started)| {
                pending == key && now.duration_since(started) < std::time::Duration::from_secs(8)
            })
        {
            return true;
        }
        self.pending_shell_dissolve = Some((key.to_string(), now));
        false
    }

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
    pub(crate) fn current_host_label(&self) -> &str {
        self.connection.connected_host.as_deref().unwrap_or("local")
    }

    pub(crate) fn tx_for_host(
        &self,
        host: &str,
    ) -> Option<&tokio::sync::mpsc::UnboundedSender<ClientRequest>> {
        self.connection.host_txs.get(host).or_else(|| {
            // Never send a disconnected host's requests to another server.
            if self.connection.host_statuses.is_empty() {
                self.connection.daemon_tx.as_ref()
            } else {
                None
            }
        })
    }

    pub(crate) fn active_session_host(&self) -> &str {
        self.selection
            .selected_session_host
            .as_deref()
            .unwrap_or_else(|| self.current_host_label())
    }

    pub(crate) fn active_session_tx(
        &self,
    ) -> Option<&tokio::sync::mpsc::UnboundedSender<ClientRequest>> {
        self.tx_for_host(self.active_session_host())
    }

    pub(crate) fn host_for_summary(&self, session: &SessionSummary) -> String {
        self.connection
            .host_sessions
            .iter()
            .find(|(_, s)| s == session)
            .map(|(host, _)| host.clone())
            .unwrap_or_else(|| self.active_session_host().to_string())
    }

    pub(crate) fn session_key_for(&self, session: &SessionSummary) -> String {
        let host = if self
            .selection
            .selected_session
            .as_ref()
            .is_some_and(|s| s == session)
        {
            self.active_session_host().to_string()
        } else {
            self.host_for_summary(session)
        };
        canonical_session_key(&host, session.agent, &session.id)
    }

    pub(crate) fn session_inputs(&self) -> Vec<SessionInput<'_>> {
        if !self.connection.host_sessions.is_empty() {
            return self
                .connection
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
        self.connection
            .sessions
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
            for (h, s) in &mut self.connection.host_sessions {
                if h == &host && s.id == session.id && s.agent == session.agent {
                    *s = session.clone();
                }
            }
            if self.connection.host_statuses.is_empty() {
                for s in &mut self.connection.sessions {
                    if s.id == session.id && s.agent == session.agent {
                        *s = session.clone();
                    }
                }
            }
            if self.active_session_host() == host
                && self
                    .selection
                    .selected_session
                    .as_ref()
                    .is_some_and(|s| s.id == session.id && s.agent == session.agent)
            {
                self.selection.selected_session = Some(session);
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
            self.sidebar_state.rename = Some(InlineRenameState::new(session_key, sess.id, current));
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
                window.focus(&self.focus.root_focus_handle, cx);
            }
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
            .selection
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

        let in_starred = self.sidebar_state.active_workspace_id == STARRED_WORKSPACE_ID;

        let store = &self.sidebar_state.workspace_store;
        let active_label = if self.sidebar_state.active_workspace_id == ALL_WORKSPACES_ID {
            match self.sidebar_state.active_workspace_view {
                WorkspaceView::All => "All",
                WorkspaceView::Frequent => "Frequent",
                WorkspaceView::Favorites => "Favorites",
            }
        } else if in_starred {
            "Favorites"
        } else {
            store
                .workspace_by_id(&self.sidebar_state.active_workspace_id)
                .map(|ws| ws.name.as_str())
                .unwrap_or("All")
        };
        let ws_strip = Button::new("workspace-switcher")
            .w_full()
            .bg(theme::bg_surface())
            .rounded(RADIUS_CHIP)
            .label(active_label.to_string())
            .tooltip(crate::icons::shortcut_tooltip(
                "Switch workspace",
                "workspace.switcher",
            ))
            .child(div().flex_1())
            .child(lucide_icon(icons::CHEVRON_DOWN, 12.0))
            .on_click(cx.listener(|this, _, window, cx| {
                this.open_workspace_switcher(window, cx);
            }));

        // Sessions scroll contents: Pinned Grid -> Groups -> Recents
        let update_card = self.render_update_card(false, cx);
        let mut sessions_content = v_flex()
            .gap_2()
            .px_3()
            .pt(px(56.0
                + if self.sidebar_state.new_workspace_draft.is_some() {
                    48.0
                } else {
                    0.0
                }
                + if self.sidebar_state.unread_only {
                    36.0
                } else {
                    0.0
                }))
            .pb(px(if update_card.is_some() { 220.0 } else { 52.0 }));

        // 1. Pinned avatar tile grid
        if !layout.pinned_tiles.is_empty() {
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
                        .h(px(100.0))
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
                                cx.stop_propagation();
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
                                            el.child(
                                                lucide_icon(icons::STAR_FILLED, 14.0)
                                                    .text_color(theme::accent_amber()),
                                            )
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
                                )
                                .child(
                                    div()
                                        .id(SharedString::from(format!(
                                            "sidebar-subtitle-{}",
                                            row.session_key
                                        )))
                                        .test_support()
                                        .w_full()
                                        .text_xs()
                                        .text_color(theme::text_muted())
                                        .truncate()
                                        .child(Label::new(session_subtitle(
                                            row.agent,
                                            row.cwd.as_deref(),
                                            self.find_session_by_key(&row.session_key)
                                                .as_ref()
                                                .and_then(|s| s.last_assistant_preview.as_deref()),
                                        ))),
                                ),
                        ),
                );
            }
            sessions_content = sessions_content
                .child(
                    div()
                        .id("sidebar-pinned-header")
                        .test_support()
                        .px_2()
                        .py_1()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::text_muted())
                        .child(Label::new("Pinned")),
                )
                .child(pin_grid);
        }

        // 2. Collapsible Named Groups (Ctrl+G)
        for (g_idx, gv) in layout.groups.iter().enumerate() {
            let collapsed = gv.group.collapsed;
            let dot_color = gv.group.color.as_deref().and_then(parse_hex_color);
            let mut grp_box = v_flex().gap_1().child(
                div()
                    .id(SharedString::from(format!("sidebar-group-{g_idx}")))
                    .test_support()
                    .px_2()
                    .py_1()
                    .child(
                        h_flex()
                            .items_center()
                            .justify_between()
                            .child(
                                h_flex()
                                    .items_center()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_1p5()
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
                                            .text_color(theme::text_muted())
                                            .truncate()
                                            .child(Label::new(gv.group.name.clone())),
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
                        .test_support()
                        .px_2()
                        .py_1()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::text_muted())
                        .child(Label::new("Recents")),
                )
            },
        );

        if self.connection.sessions.is_empty() {
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
                    } else if self.sidebar_state.active_workspace_view == WorkspaceView::Frequent {
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
                    cx,
                ));
            }
        }
        sessions_content = sessions_content.child(recents_section);

        v_flex()
            .id("sidebar-open")
            .test_support()
            .relative()
            .overflow_hidden()
            .w(px(280.0))
            .h_full()
            .bg(theme::bg_sidebar())
            .border_r_1()
            .border_color(theme::border_subtle())
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
                if this.is_sidebar_rename_mounted() {
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
            .child(
                div()
                    .id("sessions-scroll")
                    .test_support()
                    .flex_1()
                    .h_full()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(sessions_content),
            )
            .child(
                v_flex()
                    .id("sidebar-top-overlay")
                    .test_support()
                    .absolute()
                    .top_3()
                    .left_3()
                    .right_3()
                    .gap_2()
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
                                .tooltip(|window, cx| {
                                    gpui_kit::component::tooltip::Tooltip::new(
                                        crate::icons::shortcut_tooltip(
                                            "Show all messages",
                                            "inbox.unread_only",
                                        ),
                                    )
                                    .build(window, cx)
                                })
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
                    }),
            )
            .when_some(update_card, |el, card| {
                el.child(div().absolute().bottom_12().left_3().right_3().child(card))
            })
            .child(
                div()
                    .absolute()
                    .bottom_3()
                    .left_3()
                    .child(self.render_dev_stats_indicator(false, cx)),
            )
            .into_any_element()
    }
}

#[cfg(test)]
#[path = "sidebar_tests.rs"]
mod tests;
