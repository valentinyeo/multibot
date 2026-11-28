use super::*;

actions!(
    multibot,
    [
        // Slice 1 (existing 19 actions)
        OpenPalette,
        OpenWorkspaces,
        OpenInbox,
        ToggleInbox,
        ToggleSettings,
        ToggleSidebar,
        ToggleFullscreen,
        NewWindow,
        OpenShortcuts,
        CloseOverlay,
        VoiceDictation,
        NextSession,
        PrevSession,
        JumpSession1,
        JumpSession2,
        JumpSession3,
        JumpSession4,
        JumpSession5,
        JumpSession6,
        JumpSession7,
        JumpSession8,
        JumpSession9,
        // Slice 3 — Session lifecycle & sidebar structure (8 actions)
        NewSession,
        DeleteSession,
        ArchiveSession,
        ReopenClosedSession,
        ReconnectSession,
        MoveSessionUp,
        MoveSessionDown,
        ToggleGroup,
        // Slice 3 — Inbox & session triage (6 actions)
        InboxNext,
        InboxPrev,
        ToggleUnreadOnly,
        MarkSessionUnread,
        MoveSessionWaiting,
        RemindSession,
        UndoWaiting,
        OpenDone,
        OpenHibernated,
        OpenDissolved,
        ToggleHibernateMode,
        // Slice 3 — Workspaces & views (16 actions)
        ToggleFavorite,
        SnoozeWorkspace,
        JumpWorkspace1,
        JumpWorkspace2,
        JumpWorkspace3,
        JumpWorkspace4,
        JumpWorkspace5,
        JumpWorkspace6,
        JumpWorkspace7,
        JumpWorkspace8,
        JumpWorkspace9,
        PrevWorkspace,
        NextWorkspace,
        WorkspaceViewAll,
        WorkspaceViewFrequent,
        WorkspaceViewFavorites,
        // Slice 3 — System inline rename (1 action)
        RenameActiveSession,
        // Slice 4 — Rich Chat View & font zoom (6 actions)
        CopyLatestCodeBlock,
        FindInChat,
        BrowseCommands,
        FontZoomIn,
        FontZoomOut,
        FontZoomReset,
        // Inbox split cycling (scoped to "Inbox" context)
        InboxNextSplit,
        InboxPrevSplit,
        // Slice 5 — built-in native terminal pane (`Alt+C`)
        ToggleTerminal,
        ToggleBrowserPane,
        FocusBrowserPane,
        CaptureBrowserPane,
    ]
);

impl MultibotView {
    pub(super) fn register_actions(
        &self,
        root: gpui_kit::Stateful<gpui_kit::Div>,
        cx: &mut Context<Self>,
    ) -> gpui_kit::Stateful<gpui_kit::Div> {
        root.on_modifiers_changed(cx.listener(|this, ev: &ModifiersChangedEvent, _, cx| {
            this.handle_modifiers_changed(ev, cx);
        }))
        .capture_action(cx.listener(|this, _: &CloseOverlay, _, cx| {
            if this.sidebar_state.dev_stats_expanded
                && this.active_modal == ActiveModal::None
                && !this.table_modal_state.is_open()
            {
                this.sidebar_state.dev_stats_expanded = false;
                cx.stop_propagation();
                cx.notify();
            }
        }))
        .capture_action(
            cx.listener(|this, _: &gpui_kit::component::input::Escape, _, cx| {
                if this.sidebar_state.dev_stats_expanded
                    && this.active_modal == ActiveModal::None
                    && !this.table_modal_state.is_open()
                {
                    this.sidebar_state.dev_stats_expanded = false;
                    cx.stop_propagation();
                    cx.notify();
                }
            }),
        )
        .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
            if this.active_modal == ActiveModal::Shortcuts
                && !this.table_modal_state.is_open()
                && ev.keystroke.key == "escape"
            {
                this.active_modal = ActiveModal::None;
                this.focus_active_surface_default(window, cx);
                cx.stop_propagation();
                window.prevent_default();
                cx.notify();
                return;
            }
            if this.handle_lifecycle_keydown(ev, window, cx) {
                return;
            }
            // Top-pill inline rename fallback when sidebar is Collapsed/Hidden (`F2`).
            // `#header-rename-input` is only mounted on `MainSurface::Chat` for the active session.
            if this.active_modal == ActiveModal::None
                && !this.table_modal_state.is_open()
                && !this.has_mounted_chat_overlay()
                && this.sidebar_state.mode != SidebarMode::Open
                && this.is_sidebar_rename_mounted()
            {
                let outcome = this.sidebar_state.rename.as_mut().map(|rename| {
                    rename.handle_key(
                        &ev.keystroke.key,
                        ev.keystroke.key_char.as_deref(),
                        ev.keystroke.modifiers,
                    )
                });
                match outcome {
                    Some(RenameKeyOutcome::Commit) => {
                        if let Some(rename) = this.sidebar_state.rename.take() {
                            this.commit_session_rename(&rename.session_key, &rename.draft, cx);
                        }
                        cx.stop_propagation();
                        window.prevent_default();
                    }
                    Some(RenameKeyOutcome::Cancel) => {
                        this.sidebar_state.rename = None;
                        cx.stop_propagation();
                        window.prevent_default();
                        cx.notify();
                    }
                    Some(RenameKeyOutcome::Consumed) => {
                        cx.stop_propagation();
                        window.prevent_default();
                        cx.notify();
                    }
                    Some(RenameKeyOutcome::Ignored) | None => {}
                }
            }
        }))
        // =================================================================
        // Slice 1 Actions
        // =================================================================
        .on_action(cx.listener(|this, _: &OpenPalette, window, cx| {
            if this.table_modal_state.is_open() {
                this.table_modal_state.close();
            }
            this.open_palette(window, cx);
        }))
        .on_action(cx.listener(|this, _: &OpenWorkspaces, window, cx| {
            this.open_workspace_switcher(window, cx);
        }))
        .on_action(cx.listener(|this, _: &OpenInbox, window, cx| {
            if this.table_modal_state.is_open() {
                this.table_modal_state.close();
            }
            this.active_modal = ActiveModal::None;
            this.sidebar_state.rename = None;
            this.sidebar_state.new_workspace_draft = None;
            this.set_main_surface(MainSurface::Inbox);
            this.inbox_state.select_all_split();
            this.inbox_state.lifecycle = multibot_core::workspaces::SessionLifecycle::Active;
            this.focus_inbox(window, cx);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &OpenDone, window, cx| {
            this.open_lifecycle_view(
                multibot_core::workspaces::SessionLifecycle::Done,
                window,
                cx,
            );
        }))
        .on_action(cx.listener(|this, _: &OpenHibernated, window, cx| {
            this.open_lifecycle_view(
                multibot_core::workspaces::SessionLifecycle::Hibernated,
                window,
                cx,
            );
        }))
        .on_action(cx.listener(|this, _: &OpenDissolved, window, cx| {
            this.open_lifecycle_view(
                multibot_core::workspaces::SessionLifecycle::Dissolved,
                window,
                cx,
            );
        }))
        .on_action(cx.listener(|this, _: &ToggleInbox, window, cx| {
            if this.table_modal_state.is_open() {
                this.table_modal_state.close();
            }
            this.active_modal = ActiveModal::None;
            if this.main_surface == MainSurface::Inbox {
                this.set_main_surface(MainSurface::Chat);
                this.focus_active_surface_default(window, cx);
            } else {
                this.set_main_surface(MainSurface::Inbox);
                this.focus_inbox(window, cx);
            }
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &ToggleSettings, window, cx| {
            if this.table_modal_state.is_open() {
                this.table_modal_state.close();
            }
            this.active_modal = ActiveModal::None;
            let next = if this.main_surface == MainSurface::Settings {
                MainSurface::Chat
            } else {
                MainSurface::Settings
            };
            this.set_main_surface(next);
            this.focus_active_surface_default(window, cx);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &ToggleFullscreen, _, cx| {
            if this.active_modal == ActiveModal::None && !this.table_modal_state.is_open() {
                this.toggle_shell_fullscreen(cx);
            }
        }))
        .on_action(cx.listener(|this, _: &ToggleSidebar, window, cx| {
            this.cycle_sidebar_mode_synced(cx);
            if this.active_modal == ActiveModal::None
                && !this.table_modal_state.is_open()
                && !this.has_mounted_chat_overlay()
                && this.is_sidebar_inline_edit_mounted()
            {
                this.focus_sidebar(window, cx);
            } else if this.sidebar_state.mode != SidebarMode::Open
                && !this.is_focused_on_mounted_known_handle(window, cx)
            {
                this.focus_active_surface_default(window, cx);
            }
        }))
        .on_action(cx.listener(|this, _: &ToggleBrowserPane, window, cx| {
            this.toggle_browser_pane(window, cx);
        }))
        .on_action(cx.listener(|this, _: &FocusBrowserPane, window, cx| {
            this.focus_browser_pane(window, cx);
        }))
        .on_action(cx.listener(|this, _: &CaptureBrowserPane, window, cx| {
            this.capture_browser_pane(window, cx);
        }))
        .on_action(cx.listener(|this, _: &ToggleTerminal, window, cx| {
            // Modals own the keyboard; the pane never opens over one.
            if this.active_modal != ActiveModal::None || this.table_modal_state.is_open() {
                return;
            }
            // Alt+C always wins: it opens the pane, or closes it even while the grid
            // has focus, dropping the PTY and its child process immediately.
            this.toggle_terminal_pane(cx);
            if this.terminal.is_open() {
                if let Some(focus_handle) = this.terminal.focus_handle.clone() {
                    window.focus(&focus_handle, cx);
                }
            } else {
                this.focus_active_surface_default(window, cx);
            }
        }))
        .on_action(cx.listener(|this, _: &NewWindow, _, cx| {
            this.open_extra_window(cx);
        }))
        .on_action(cx.listener(|this, _: &OpenShortcuts, window, cx| {
            if this.table_modal_state.is_open() {
                this.table_modal_state.close();
            }
            this.active_modal = match this.active_modal {
                ActiveModal::Shortcuts => ActiveModal::None,
                _ => ActiveModal::Shortcuts,
            };
            #[cfg(windows)]
            if this.active_modal == ActiveModal::Shortcuts {
                this.hide_browser_pane();
            }
            this.focus_active_surface_default(window, cx);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &VoiceDictation, window, cx| {
            this.toggle_dictation(window, cx);
        }))
        .on_action(cx.listener(|this, _: &CloseOverlay, window, cx| {
            if this.chat_state.image_viewer.take().is_some() {
                this.focus_active_surface_default(window, cx);
            } else if this.sidebar_state.pending_shell_dissolve.take().is_some() {
                this.notice = None;
            } else if this.active_modal == ActiveModal::ChatInfo {
                this.active_modal = ActiveModal::None;
                this.focus_active_surface_default(window, cx);
            } else if this.voice.phase != multibot_core::voice::DictationPhase::Off {
                this.voice.cancel();
            } else if this.table_modal_state.is_open() {
                // 1. Topmost overlay: expanded table modal
                this.table_modal_state.close();
                this.focus_active_surface_default(window, cx);
            } else if this.active_modal != ActiveModal::None {
                // 2. Modal dialogs (Command Palette / F1 Shortcuts)
                this.active_modal = ActiveModal::None;
                this.palette_state.mode = multibot_core::workspaces::PaletteMode::Global;
                this.palette_state.reminder_target = None;
                this.palette_state.dissolve_workspace_id = None;
                this.focus_active_surface_default(window, cx);
            } else if this.chat_state.composer_menu_open {
                this.chat_state.composer_menu_open = false;
                this.focus_active_surface_default(window, cx);
            } else if this.main_surface == MainSurface::Chat && this.mention_picker_state.is_open()
            {
                this.handle_mention_key("escape", window, cx);
            } else if this.main_surface == MainSurface::Chat && this.slash_picker_state.is_open() {
                // 3. Slash / skill autocomplete popover (Chat surface only)
                this.chat_state.slash_sync_pending = false;
                this.slash_picker_state.close();
            } else if this.main_surface == MainSurface::Chat && this.chat_state.find_open {
                // 4. In-chat Find bar (Chat surface only)
                this.chat_state.close_find();
                if !this.is_focused_on_mounted_known_handle(window, cx) {
                    this.focus_active_surface_default(window, cx);
                }
            } else if this.main_surface == MainSurface::Chat
                && this.chat_state.message_action_menu_open
            {
                // 5. Focused message action menu (Chat surface only)
                this.chat_state.message_action_menu_open = false;
                this.chat_state.message_action_index = 0;
            } else if this.main_surface == MainSurface::Chat
                && this.chat_state.focused_message_idx.is_some()
            {
                // 6. Focused message highlight (Chat surface only)
                this.chat_state.focused_message_idx = None;
            } else if this.main_surface == MainSurface::Chat
                && this.chat_state.reply_quote.is_some()
            {
                // 7. Reply quote banner (Chat surface only)
                this.chat_state.reply_quote = None;
            } else if this.is_sidebar_rename_mounted() {
                // 8. Inline sidebar / top-pill rename (only when mounted)
                this.sidebar_state.rename = None;
                this.focus_active_surface_default(window, cx);
            } else if this.is_sidebar_workspace_draft_mounted() {
                // 9. Inline new workspace draft (only when Open sidebar is rendered)
                this.sidebar_state.new_workspace_draft = None;
                this.focus_active_surface_default(window, cx);
            } else if this.is_inbox_rename_mounted() {
                // 10. Inline Inbox rename (Inbox surface only)
                this.inbox_state.rename = None;
            } else if this.main_surface == MainSurface::Inbox
                && this.inbox_state.is_search_visible()
            {
                // 11. Inbox search bar: close + clear + keep Inbox open
                this.inbox_state.close_search();
            } else if this.main_surface != MainSurface::Chat {
                // 12. Return from Inbox / Settings surface to Chat
                this.set_main_surface(MainSurface::Chat);
                this.focus_active_surface_default(window, cx);
            } else if let Some(ref sess) = this.selection.selected_session
                && sess.working
            {
                // 13. Interrupt working agent on Esc in Chat View
                if let Some(tx) = this.active_session_tx() {
                    let _ = tx.send(ClientRequest::InterruptSession {
                        session_id: sess.id.clone(),
                        agent: sess.agent,
                    });
                    this.notice = Some(format!("Stop requested for {}.", sess.title));
                }
            }
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &NextSession, _, cx| {
            if this.table_modal_state.is_open() || this.active_modal != ActiveModal::None {
                return;
            }
            this.cycle_session(1, cx);
        }))
        .on_action(cx.listener(|this, _: &PrevSession, _, cx| {
            if this.table_modal_state.is_open() || this.active_modal != ActiveModal::None {
                return;
            }
            this.cycle_session(-1, cx);
        }))
        .on_action(cx.listener(|this, _: &JumpSession1, _, cx| {
            if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                this.jump_session_index(0, cx);
            }
        }))
        .on_action(cx.listener(|this, _: &JumpSession2, _, cx| {
            if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                this.jump_session_index(1, cx);
            }
        }))
        .on_action(cx.listener(|this, _: &JumpSession3, _, cx| {
            if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                this.jump_session_index(2, cx);
            }
        }))
        .on_action(cx.listener(|this, _: &JumpSession4, _, cx| {
            if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                this.jump_session_index(3, cx);
            }
        }))
        .on_action(cx.listener(|this, _: &JumpSession5, _, cx| {
            if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                this.jump_session_index(4, cx);
            }
        }))
        .on_action(cx.listener(|this, _: &JumpSession6, _, cx| {
            if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                this.jump_session_index(5, cx);
            }
        }))
        .on_action(cx.listener(|this, _: &JumpSession7, _, cx| {
            if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                this.jump_session_index(6, cx);
            }
        }))
        .on_action(cx.listener(|this, _: &JumpSession8, _, cx| {
            if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                this.jump_session_index(7, cx);
            }
        }))
        .on_action(cx.listener(|this, _: &JumpSession9, _, cx| {
            if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                this.jump_session_index(8, cx);
            }
        }))
        // =================================================================
        // Slice 3 — Session Lifecycle & Sidebar Structure Actions
        // =================================================================
        .on_action(cx.listener(|this, _: &NewSession, _, cx| {
            if this.table_modal_state.is_open() || this.active_modal != ActiveModal::None {
                return;
            }
            let cwd = this.default_spawn_cwd();
            this.spawn_new_session(AgentKind::Claude, cwd, None, None, cx);
        }))
        .on_action(cx.listener(|this, _: &DeleteSession, _, cx| {
            if let Some(key) = this.target_triage_session_key() {
                this.dissolve_session_key(&key, cx);
            }
        }))
        .on_action(cx.listener(|this, _: &ArchiveSession, _, cx| {
            if let Some(key) = this.target_triage_session_key() {
                this.mark_done_or_inbox(&key, cx);
            }
        }))
        .on_action(cx.listener(|this, _: &ReopenClosedSession, _, cx| {
            this.reactivate_last_dissolved(cx);
        }))
        .on_action(cx.listener(|this, _: &ReconnectSession, _, cx| {
            if this.table_modal_state.is_open() || this.active_modal != ActiveModal::None {
                return;
            }
            if this
                .selection
                .selected_session
                .as_ref()
                .is_some_and(|s| s.agent == AgentKind::Shell)
            {
                this.reconnect_embedded_shell(cx);
                return;
            }
            if !this.connection.host_statuses.is_empty() || !this.settings.ssh_hosts.is_empty() {
                this.reconnect_all_hosts(cx);
                return;
            }
            if let Some(ref tx) = this.connection.daemon_tx
                && !tx.is_closed()
                && tx
                    .send(ClientRequest::ListLifecycleSessions {
                        limit: Some(SESSION_LIST_LIMIT),
                    })
                    .is_ok()
            {
                if let Some(ref s) = this.selection.selected_session {
                    let _ = tx.send(ClientRequest::WatchSession {
                        session_id: s.id.clone(),
                        source_path: s.source_path.clone(),
                        agent: s.agent,
                    });
                }
                this.notice = Some("Refreshed daemon sessions.".to_string());
                cx.notify();
                return;
            }

            // Channel is absent or closed: reconnect using SharedAppStore.connected_target or
            // configured/selected host instead of falsely reporting a refresh.
            let shared_target = cx
                .try_global::<SharedAppStore>()
                .and_then(|s| s.connected_target.clone());
            let host_label = this
                .connection
                .connected_host
                .clone()
                .or_else(|| this.settings.selected_host.clone());

            if let Some(target) = shared_target {
                let label = host_label.unwrap_or_else(|| match &target {
                    DaemonTarget::RemoteSsh { host, .. } => host.name.clone(),
                    DaemonTarget::LocalBinary(_) => "localhost".to_string(),
                });
                this.connect_to_target(label, target, cx);
            } else if let Some(host_name) = host_label {
                if let Some(host) = this
                    .settings
                    .ssh_hosts
                    .iter()
                    .find(|h| h.name == host_name || h.hostname == host_name)
                    .cloned()
                {
                    this.connect_to_target(
                        host.name.clone(),
                        DaemonTarget::RemoteSsh {
                            host,
                            bundled_linux_binary: Self::find_bundled_linux_daemon(),
                        },
                        cx,
                    );
                } else if host_name == "localhost"
                    && let Some(bin) = Self::find_local_daemon()
                {
                    this.connect_to_target(host_name, DaemonTarget::LocalBinary(bin), cx);
                }
            }
        }))
        .on_action(cx.listener(|this, _: &MoveSessionUp, _, cx| {
            if this.table_modal_state.is_open() || this.active_modal != ActiveModal::None {
                return;
            }
            this.reorder_active_session(ReorderDirection::Up, cx);
        }))
        .on_action(cx.listener(|this, _: &MoveSessionDown, _, cx| {
            if this.table_modal_state.is_open() || this.active_modal != ActiveModal::None {
                return;
            }
            this.reorder_active_session(ReorderDirection::Down, cx);
        }))
        .on_action(cx.listener(|this, _: &ToggleGroup, _, cx| {
            if this.table_modal_state.is_open() || this.active_modal != ActiveModal::None {
                return;
            }
            this.toggle_active_group(cx);
        }))
        // =================================================================
        // Slice 3 — Inbox & Session Triage Actions
        // =================================================================
        .on_action(cx.listener(|this, _: &InboxNext, _, cx| {
            if this.table_modal_state.is_open()
                || this.active_modal != ActiveModal::None
                || this.is_sidebar_rename_mounted()
                || this.is_inbox_rename_mounted()
            {
                return;
            }
            if this.main_surface == MainSurface::Inbox {
                let count = this.compute_inbox_rows(now_unix_ms()).len();
                if count > 0 {
                    this.inbox_state.selected_row =
                        (this.inbox_state.selected_row + 1).min(count - 1);
                }
                cx.notify();
            } else {
                if !this.step_inbox_session(1, cx) {
                    this.cycle_workspace(1, cx);
                }
                this.sync_workspace_to_shared(cx);
            }
        }))
        .on_action(cx.listener(|this, _: &InboxPrev, _, cx| {
            if this.table_modal_state.is_open()
                || this.active_modal != ActiveModal::None
                || this.is_sidebar_rename_mounted()
                || this.is_inbox_rename_mounted()
            {
                return;
            }
            if this.main_surface == MainSurface::Inbox {
                let count = this.compute_inbox_rows(now_unix_ms()).len();
                if count > 0 {
                    this.inbox_state.selected_row = this
                        .inbox_state
                        .selected_row
                        .min(count - 1)
                        .saturating_sub(1);
                } else {
                    this.inbox_state.selected_row = 0;
                }
                cx.notify();
            } else {
                if !this.step_inbox_session(-1, cx) {
                    this.cycle_workspace(-1, cx);
                }
                this.sync_workspace_to_shared(cx);
            }
        }))
        .on_action(cx.listener(|this, _: &ToggleUnreadOnly, _, cx| {
            if this.table_modal_state.is_open() || this.active_modal != ActiveModal::None {
                return;
            }
            this.toggle_unread_only(cx);
        }))
        .on_action(cx.listener(|this, _: &MarkSessionUnread, _, cx| {
            if this.table_modal_state.is_open()
                || this.active_modal != ActiveModal::None
                || this.is_sidebar_rename_mounted()
                || this.is_inbox_rename_mounted()
            {
                return;
            }
            if let Some(key) = this.target_triage_session_key()
                && let Some(sess) = this.find_session_by_key(&key)
            {
                this.sidebar_state
                    .workspace_store
                    .toggle_session_unread(&key, &sess);
                this.persist_workspace_store_if_enabled();
                this.sync_workspace_to_shared(cx);
                cx.notify();
            }
        }))
        .on_action(cx.listener(|this, _: &RemindSession, window, cx| {
            if this.active_modal == ActiveModal::None
                && !this.table_modal_state.is_open()
                && let Some(key) = this.target_triage_session_key()
            {
                this.open_reminder_picker(crate::palette::ReminderTarget::Session(key), window, cx);
            }
        }))
        .on_action(cx.listener(|this, _: &MoveSessionWaiting, _, cx| {
            if this.table_modal_state.is_open()
                || this.active_modal != ActiveModal::None
                || this.is_sidebar_rename_mounted()
                || this.is_inbox_rename_mounted()
            {
                return;
            }
            if let Some(key) = this.target_triage_session_key() {
                this.mark_done_or_inbox(&key, cx);
            }
        }))
        .on_action(cx.listener(|this, _: &UndoWaiting, _, cx| {
            if this.table_modal_state.is_open()
                || this.active_modal != ActiveModal::None
                || this.is_sidebar_rename_mounted()
                || this.is_inbox_rename_mounted()
            {
                return;
            }
            if let Some(key) = this.sidebar_state.workspace_store.undo_last_waiting() {
                this.reactivate_session_key(&key, cx);
                this.persist_workspace_store_if_enabled();
                this.sync_workspace_to_shared(cx);
                cx.notify();
            }
        }))
        // =================================================================
        // Slice 3 — Workspaces & Views Actions
        // =================================================================
        .on_action(cx.listener(|this, _: &ToggleFavorite, _, cx| {
            if this.table_modal_state.is_open() || this.active_modal != ActiveModal::None {
                return;
            }
            if this.main_surface == MainSurface::Inbox {
                if let Some(key) = this.selected_inbox_session_key() {
                    this.sidebar_state.workspace_store.toggle_favorite(&key);
                    this.cleanup_unmounted_surface_state();
                    this.persist_workspace_store_if_enabled();
                    this.sync_workspace_to_shared(cx);
                    cx.notify();
                }
            } else {
                this.toggle_active_favorite(cx);
            }
        }))
        .on_action(cx.listener(|this, _: &SnoozeWorkspace, window, cx| {
            if this.table_modal_state.is_open() || this.active_modal != ActiveModal::None {
                return;
            }
            let id = this.sidebar_state.active_workspace_id.clone();
            this.open_reminder_picker(crate::palette::ReminderTarget::Workspace(id), window, cx);
        }))
        .on_action(cx.listener(|this, _: &JumpWorkspace1, _, cx| {
            if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                this.switch_workspace_digit(1, cx);
            }
        }))
        .on_action(cx.listener(|this, _: &JumpWorkspace2, _, cx| {
            if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                this.switch_workspace_digit(2, cx);
            }
        }))
        .on_action(cx.listener(|this, _: &JumpWorkspace3, _, cx| {
            if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                this.switch_workspace_digit(3, cx);
            }
        }))
        .on_action(cx.listener(|this, _: &JumpWorkspace4, _, cx| {
            if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                this.switch_workspace_digit(4, cx);
            }
        }))
        .on_action(cx.listener(|this, _: &JumpWorkspace5, _, cx| {
            if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                this.switch_workspace_digit(5, cx);
            }
        }))
        .on_action(cx.listener(|this, _: &JumpWorkspace6, _, cx| {
            if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                this.switch_workspace_digit(6, cx);
            }
        }))
        .on_action(cx.listener(|this, _: &JumpWorkspace7, _, cx| {
            if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                this.switch_workspace_digit(7, cx);
            }
        }))
        .on_action(cx.listener(|this, _: &JumpWorkspace8, _, cx| {
            if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                this.switch_workspace_digit(8, cx);
            }
        }))
        .on_action(cx.listener(|this, _: &JumpWorkspace9, _, cx| {
            if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                this.switch_workspace_digit(9, cx);
            }
        }))
        .on_action(cx.listener(|this, _: &PrevWorkspace, _, cx| {
            if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                this.cycle_workspace(-1, cx);
            }
        }))
        .on_action(cx.listener(|this, _: &NextWorkspace, _, cx| {
            if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                this.cycle_workspace(1, cx);
            }
        }))
        .on_action(cx.listener(|this, _: &WorkspaceViewAll, _, cx| {
            if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                this.set_workspace_view(WorkspaceView::All, cx);
            }
        }))
        .on_action(cx.listener(|this, _: &WorkspaceViewFrequent, _, cx| {
            if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                this.set_workspace_view(WorkspaceView::Frequent, cx);
            }
        }))
        .on_action(cx.listener(|this, _: &WorkspaceViewFavorites, _, cx| {
            if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                this.set_workspace_view(WorkspaceView::Favorites, cx);
            }
        }))
        // =================================================================
        // Slice 3 — System Inline Rename (`F2`)
        // =================================================================
        .on_action(cx.listener(|this, _: &RenameActiveSession, window, cx| {
            if this.table_modal_state.is_open()
                || this.active_modal != ActiveModal::None
                || this.is_sidebar_rename_mounted()
                || this.is_inbox_rename_mounted()
            {
                return;
            }
            if this.main_surface == MainSurface::Inbox {
                if let Some(key) = this.selected_inbox_session_key()
                    && this.is_session_rendered_in_inbox(&key)
                    && let Some(sess) = this.find_session_by_key(&key)
                {
                    let current = this
                        .sidebar_state
                        .workspace_store
                        .effective_title(&key, &sess.title)
                        .to_string();
                    this.sidebar_state.rename = None;
                    this.inbox_state.rename = Some(InlineRenameState::new(key, sess.id, current));
                    this.focus_inbox(window, cx);
                    cx.notify();
                }
            } else if let Some(sel) = this.selection.selected_session.clone() {
                let key = this.session_key_for(&sel);
                if this.can_mount_sidebar_rename(&key) {
                    if this.main_surface == MainSurface::Chat {
                        this.dismiss_conflicting_chat_overlays_for_rename();
                    }
                    this.start_sidebar_rename_with_window(key, window, cx);
                }
            }
        }))
        // =================================================================
        // Slice 4 — Rich Chat View & Font Zoom Actions
        // =================================================================
        .on_action(cx.listener(|this, _: &CopyLatestCodeBlock, _, cx| {
            if this.table_modal_state.is_open() || this.active_modal != ActiveModal::None {
                return;
            }
            if this.main_surface == MainSurface::Chat {
                this.chat_state.slash_sync_pending = false;
                this.copy_latest_code_block(cx);
            }
        }))
        .on_action(cx.listener(|this, _: &FindInChat, window, cx| {
            if this.table_modal_state.is_open() || this.active_modal != ActiveModal::None {
                return;
            }
            if this.main_surface == MainSurface::Inbox {
                this.inbox_state.rename = None;
                this.sidebar_state.rename = None;
                this.sidebar_state.new_workspace_draft = None;
                this.inbox_state.open_search();
                this.focus_inbox(window, cx);
                cx.notify();
            } else if this.main_surface == MainSurface::Chat {
                this.sidebar_state.rename = None;
                this.sidebar_state.new_workspace_draft = None;
                this.chat_state.slash_sync_pending = false;
                this.slash_picker_state.close();
                let msgs = this.history.messages.clone();
                this.mention_picker_state.close();
                this.chat_state.open_find(&msgs, this.show_tool_calls());
                this.focus_chat_panel(window, cx);
                cx.notify();
            }
        }))
        .on_action(cx.listener(|this, _: &BrowseCommands, window, cx| {
            if this.table_modal_state.is_open() || this.active_modal != ActiveModal::None {
                return;
            }
            if this.main_surface == MainSurface::Chat {
                this.sidebar_state.rename = None;
                this.sidebar_state.new_workspace_draft = None;
                this.chat_state.slash_sync_pending = false;
                this.chat_state.close_find();
                if let Some(ref sel) = this.selection.selected_session {
                    this.request_daemon_commands(sel.agent, sel.cwd.clone());
                }
                this.open_slash_browser(cx);
                this.inputs.prompt_input.update(cx, |state, cx| {
                    state.focus(window, cx);
                });
            }
        }))
        .on_action(cx.listener(|this, _: &FontZoomIn, _, cx| {
            if this.table_modal_state.is_open() || this.active_modal != ActiveModal::None {
                return;
            }
            this.chat_state.slash_sync_pending = false;
            this.zoom_chat_font(1, cx);
            this.sync_settings_to_shared(cx);
            if this.sidebar_state.persist_to_disk {
                let _ = this.settings.save();
            }
        }))
        .on_action(cx.listener(|this, _: &FontZoomOut, _, cx| {
            if this.table_modal_state.is_open() || this.active_modal != ActiveModal::None {
                return;
            }
            this.chat_state.slash_sync_pending = false;
            this.zoom_chat_font(-1, cx);
            this.sync_settings_to_shared(cx);
            if this.sidebar_state.persist_to_disk {
                let _ = this.settings.save();
            }
        }))
        .on_action(cx.listener(|this, _: &FontZoomReset, _, cx| {
            if this.table_modal_state.is_open() || this.active_modal != ActiveModal::None {
                return;
            }
            this.chat_state.slash_sync_pending = false;
            this.reset_chat_font(cx);
            this.sync_settings_to_shared(cx);
            if this.sidebar_state.persist_to_disk {
                let _ = this.settings.save();
            }
        }))
    }
}
