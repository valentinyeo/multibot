use super::*;

impl MultibotView {
    /// Computes the current conditional-subtree visibility signature for structural focus recovery.
    pub(super) fn focus_layout_signature(&self) -> FocusLayoutSignature {
        let update_card_visible = self.update_ui.is_card_visible(
            self.settings.sidebar_mode,
            self.sidebar_state.is_shown(),
            self.settings.dismissed_update_version.as_deref(),
        );
        let update_card_kind = if !update_card_visible {
            0
        } else {
            match &self.update_ui.state {
                UpdateState::Available(_) | UpdateState::ReadyToRestart(_) => 1,
                UpdateState::Downloading { .. } => 2,
                UpdateState::Verifying { .. } | UpdateState::Applying { .. } => 3,
                UpdateState::Error {
                    retryable: false, ..
                } => 4,
                UpdateState::Error {
                    retryable: true, ..
                } => 5,
                UpdateState::Idle | UpdateState::Checking | UpdateState::UpToDate => 0,
            }
        };
        let answered_question_count = self.chat_state.answered_prompts.len()
            + self
                .chat_state
                .answered_questions
                .values()
                .map(HashSet::len)
                .sum::<usize>();
        FocusLayoutSignature {
            main_surface: self.main_surface,
            active_modal: self.active_modal,
            sidebar_mode: self.sidebar_state.mode,
            table_modal_open: self.table_modal_state.is_open(),
            find_open: self.main_surface == MainSurface::Chat && self.chat_state.find_open,
            update_card_kind,
            selected_session_id: self
                .selection
                .selected_session
                .as_ref()
                .map(|s| s.id.clone()),
            slash_picker_open: self.main_surface == MainSurface::Chat
                && (self.slash_picker_state.is_open() || self.mention_picker_state.is_open()),
            rename_active: self.is_sidebar_rename_mounted() || self.is_inbox_rename_mounted(),
            sidebar_workspace_draft_active: self.is_sidebar_workspace_draft_mounted(),
            reply_quote_active: self.main_surface == MainSurface::Chat
                && self.chat_state.reply_quote.is_some(),
            has_pending_attachments: self.main_surface == MainSurface::Chat
                && !self.chat_state.pending_attachments.is_empty(),
            message_action_menu_open: self.main_surface == MainSurface::Chat
                && self.chat_state.message_action_menu_open,
            focused_message_idx: if self.main_surface == MainSurface::Chat {
                self.chat_state.focused_message_idx
            } else {
                None
            },
            message_count: self.history.messages.len(),
            answered_question_count,
            gpu_adapter_count: self.gpu_report.discovered.len(),
            terminal_open: self.terminal.is_open(),
        }
    }

    /// Returns `true` when focus is currently on one of the known view-owned `FocusHandle`s
    /// whose subtree is mounted in the current render state.
    pub(crate) fn is_focused_on_mounted_known_handle(&self, window: &Window, cx: &App) -> bool {
        self.focus.root_focus_handle.is_focused(window)
            || (self.active_modal == ActiveModal::Palette
                && self
                    .inputs
                    .palette_input
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window))
            || (self.active_modal == ActiveModal::Shortcuts
                && self
                    .inputs
                    .shortcuts_search_input
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window))
            || (self.table_modal_state.is_open()
                && self
                    .table_modal_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window)))
            || (self.main_surface == MainSurface::Inbox
                && self
                    .inbox_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window)))
            || (self.main_surface == MainSurface::Chat
                && (self
                    .chat_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window))
                    || self
                        .inputs
                        .prompt_input
                        .read(cx)
                        .focus_handle(cx)
                        .is_focused(window)))
            || (self.main_surface == MainSurface::Settings
                && self.focus.settings_focus_handle.is_focused(window))
            || (self.main_surface == MainSurface::Settings
                && self.settings_tab == crate::settings::SettingsTab::Hosts
                && self
                    .inputs
                    .host_input
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window))
            || (self.is_sidebar_inline_edit_mounted()
                && !self.has_mounted_chat_overlay()
                && self
                    .sidebar_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window)))
    }

    /// Restores focus to the active surface's default mounted `FocusHandle`:
    /// - `table_modal_state.focus_handle` if the expanded table modal is open
    /// - `palette_input` if the Command Palette modal is open
    /// - `shortcuts_search_input` if the Shortcuts modal is open
    /// - `prompt_input` for chat sessions, or the native input for shell sessions
    /// - `chat_state.focus_handle` (`#chat-panel-root`) if on `MainSurface::Chat` with no modal
    ///   open and Chat Find or Message Navigation / Action Menu is active
    /// - `sidebar_state.focus_handle` if no modal/chat overlay is active and an Open-sidebar
    ///   inline rename or new-workspace draft is visibly mounted
    /// - `inbox_state.focus_handle` if on `MainSurface::Inbox` with no modal open
    /// - `root_focus_handle` otherwise
    pub(crate) fn focus_active_surface_default(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.table_modal_state.is_open() {
            self.focus_table_modal(window, cx);
        } else if self.active_modal == ActiveModal::Palette {
            self.inputs.palette_input.update(cx, |state, cx| {
                state.focus(window, cx);
            });
        } else if self.active_modal == ActiveModal::Shortcuts {
            self.shell_host.focus_parent();
            self.inputs
                .shortcuts_search_input
                .update(cx, |state, cx| state.focus(window, cx));
        } else if self.active_modal == ActiveModal::None
            && self.main_surface == MainSurface::Settings
        {
            window.focus(&self.focus.settings_focus_handle, cx);
        } else if self.active_modal == ActiveModal::None
            && self.main_surface == MainSurface::Chat
            && (self.slash_picker_state.is_open() || self.mention_picker_state.is_open())
        {
            self.inputs.prompt_input.update(cx, |state, cx| {
                state.focus(window, cx);
            });
        } else if self.active_modal == ActiveModal::None
            && self.main_surface == MainSurface::Chat
            && (self.chat_state.find_open
                || self.chat_state.message_action_menu_open
                || self.chat_state.focused_message_idx.is_some())
        {
            self.focus_chat_panel(window, cx);
        } else if self.active_modal == ActiveModal::None
            && !self.has_mounted_chat_overlay()
            && self.is_sidebar_inline_edit_mounted()
        {
            self.focus_sidebar(window, cx);
        } else if self.main_surface == MainSurface::Inbox && self.active_modal == ActiveModal::None
        {
            self.focus_inbox(window, cx);
        } else if self.main_surface == MainSurface::Chat && self.active_modal == ActiveModal::None {
            if let Some(open) = self.computer.open() {
                self.shell_host.focus_parent();
                open.url_input
                    .update(cx, |state, cx| state.focus(window, cx));
            } else if let Some(session) = &self.selection.selected_session {
                if session.agent == AgentKind::Shell {
                    let host = self.active_session_host().to_string();
                    self.focus_chat_panel(window, cx);
                    self.shell_host.focus_session(&host);
                } else {
                    self.shell_host.focus_parent();
                    self.inputs
                        .prompt_input
                        .update(cx, |state, cx| state.focus(window, cx));
                }
            } else {
                window.focus(&self.focus.root_focus_handle, cx);
            }
        } else {
            window.focus(&self.focus.root_focus_handle, cx);
        }
    }

    /// Returns `true` when focus is currently stranded on a child `FocusHandle` whose subtree
    /// is unmounted in the current state (which would otherwise cause GPUI to fall back to
    /// `gpui_base::Root` above `#multibot-root`, breaking all global shortcuts).
    pub(super) fn has_focused_unmounted_child(&self, window: &Window, cx: &Context<Self>) -> bool {
        (self.active_modal != ActiveModal::Shortcuts
            && self
                .inputs
                .shortcuts_search_input
                .read(cx)
                .focus_handle(cx)
                .is_focused(window))
            || (self.active_modal != ActiveModal::Palette
                && self
                    .inputs
                    .palette_input
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window))
            || (!self.table_modal_state.is_open()
                && self
                    .table_modal_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window)))
            || (self.main_surface != MainSurface::Inbox
                && self
                    .inbox_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window)))
            || (self.main_surface != MainSurface::Chat
                && (self
                    .chat_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window))
                    || self
                        .inputs
                        .prompt_input
                        .read(cx)
                        .focus_handle(cx)
                        .is_focused(window)))
            || (self.main_surface != MainSurface::Settings
                && self.focus.settings_focus_handle.is_focused(window))
            || ((self.main_surface != MainSurface::Settings
                || self.settings_tab != crate::settings::SettingsTab::Hosts)
                && self
                    .inputs
                    .host_input
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window))
            || ((self.sidebar_state.mode != SidebarMode::Open || !self.sidebar_rendered())
                && self
                    .sidebar_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window)))
            || (!self.terminal.is_open()
                && self
                    .terminal
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window)))
    }
}
