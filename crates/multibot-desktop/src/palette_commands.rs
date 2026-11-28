use super::*;
use crate::chat_view::MessageMenuAction;

impl MultibotView {
    pub(super) fn start_agent_session(
        &mut self,
        agent: AgentKind,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        let directories = self.sidebar_state.workspace_store.workspace_directories(
            &self.sidebar_state.active_workspace_id,
            self.active_session_host(),
            &self.session_inputs(),
        );
        if directories.len() <= 1 {
            let cwd = directories
                .into_iter()
                .next()
                .unwrap_or_else(|| self.default_spawn_cwd());
            self.spawn_new_session(agent, cwd, None, None, cx);
            return;
        }
        self.active_modal = ActiveModal::Palette;
        self.palette_state.provider_picker = false;
        self.palette_state.permission_picker = false;
        self.palette_state.directory_picker = Some(SessionDirectoryPicker { agent, directories });
        self.palette_state.selected_index = 0;
        self.palette_state.scroll_handle.scroll_to_item(0);
        if let Some(window) = window {
            self.inputs.palette_input.update(cx, |state, cx| {
                state.set_value("", window, cx);
                state.focus(window, cx);
            });
        }
        cx.notify();
    }

    pub(crate) fn execute_palette_row(
        &mut self,
        row: PaletteRow,
        now_ms: i64,
        mut window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        self.execute_palette_row_inner(row, now_ms, window.as_deref_mut(), cx);
        if let Some(win) = window {
            let palette_still_focused = self.active_modal != ActiveModal::Palette
                && self
                    .inputs
                    .palette_input
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(win);
            if palette_still_focused {
                self.focus_active_surface_default(win, cx);
            }
        }
    }

    pub(super) fn execute_palette_row_inner(
        &mut self,
        row: PaletteRow,
        now_ms: i64,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        if self.palette_state.permission_picker {
            if let PaletteRow::Command { id, .. } = row
                && let Some((_, label, permission, _)) = PERMISSION_CHOICES
                    .iter()
                    .find(|(choice, _, _, _)| *choice == id)
            {
                self.update_composer_options(|options| options.permission = *permission, cx);
                self.notice = Some(format!(
                    "Permission mode: {label}. Applies on the next turn."
                ));
            }
            self.palette_state.permission_picker = false;
            self.active_modal = ActiveModal::None;
            cx.notify();
            return;
        }
        if matches!(row, PaletteRow::Command { ref id, .. } if id == "pal-cmd-permission-mode") {
            if self
                .selection
                .selected_session
                .as_ref()
                .is_some_and(|s| !matches!(s.agent, AgentKind::Claude | AgentKind::Codex))
            {
                self.notice =
                    Some("Permission overrides are supported by Claude and Codex.".into());
                self.active_modal = ActiveModal::None;
                cx.notify();
                return;
            }
            self.chat_state.composer_menu_open = false;
            if let Some(window) = window {
                self.open_palette(window, cx);
            }
            self.palette_state.permission_picker = true;
            self.palette_state.selected_index = 0;
            self.palette_state.scroll_handle.scroll_to_item(0);
            self.active_modal = ActiveModal::Palette;
            cx.notify();
            return;
        }
        if self.palette_state.provider_picker {
            if let PaletteRow::Command { id, .. } = row {
                if let Some(index) = id
                    .strip_prefix("pal-provider-")
                    .and_then(|i| i.parse::<usize>().ok())
                {
                    let choices = self.provider_choices();
                    if let Some(choice) = choices.get(index) {
                        self.request_harness_switch(choice.clone(), cx);
                    }
                }
            }
            self.palette_state.provider_picker = false;
            self.active_modal = ActiveModal::None;
            return;
        }
        if let Some(key) = self.palette_state.shell_kill_target.take() {
            if matches!(row, PaletteRow::Command { ref id, .. } if id == "pal-shell-kill-confirm") {
                self.dissolve_session_key_confirmed(&key, cx);
            }
            self.palette_state.selected_index = 0;
            if let Some(window) = window {
                self.inputs
                    .palette_input
                    .update(cx, |input, cx| input.set_value("", window, cx));
            }
            cx.notify();
            return;
        }
        if matches!(row, PaletteRow::Command { ref id, .. } if id == "pal-cmd-server-shells") {
            self.palette_state.server_shells = true;
            self.palette_state.selected_index = 0;
            self.active_modal = ActiveModal::Palette;
            let hosts: std::collections::HashSet<_> = self
                .session_inputs()
                .iter()
                .map(|input| input.host.to_string())
                .chain(self.connection.host_txs.keys().cloned())
                .chain(std::iter::once(self.current_host_label().to_string()))
                .collect();
            for host in hosts {
                if let Some(tx) = self.tx_for_host(&host) {
                    let _ = tx.send(ClientRequest::ListSessions {
                        limit: Some(200),
                        include_archived: Some(true),
                    });
                }
            }
            if let Some(window) = window {
                self.inputs.palette_input.update(cx, |input, cx| {
                    input.set_value("", window, cx);
                    input.focus(window, cx);
                });
            }
            cx.notify();
            return;
        }
        if self.palette_state.server_shells
            && let PaletteRow::Session {
                ref session_key, ..
            } = row
        {
            let key = session_key.clone();
            if self.reactivate_session_key(&key, cx) {
                self.open_session_key_with_tracking(&key, false, cx);
            }
            self.palette_state.server_shells = false;
            cx.notify();
            return;
        }
        if let Some(id) = self.palette_state.dissolve_workspace_id.take() {
            self.active_modal = ActiveModal::None;
            if matches!(row, PaletteRow::Command { ref id, .. } if id == "workspace.dissolve.confirm")
            {
                self.dissolve_workspace(&id, cx);
            }
            cx.notify();
            return;
        }
        if let Some(picker) = self.palette_state.directory_picker.take() {
            if let PaletteRow::Command { id, .. } = row
                && let Some(idx) = id
                    .strip_prefix("pal-spawn-directory-")
                    .and_then(|s| s.parse::<usize>().ok())
                && let Some(cwd) = picker.directories.get(idx)
            {
                self.active_modal = ActiveModal::None;
                self.spawn_new_session(picker.agent, cwd.clone(), None, None, cx);
            }
            return;
        }
        if let Some(target) = self.palette_state.reminder_target.take() {
            let PaletteRow::Command { id, .. } = row else {
                return;
            };
            let at = if let Some(timestamp) = id.strip_prefix("pal-reminder-custom:") {
                let Ok(timestamp) = timestamp.parse::<i64>() else {
                    self.palette_state.reminder_target = Some(target);
                    return;
                };
                Some(timestamp)
            } else if id == "pal-reminder-4" {
                None
            } else {
                let resolved = HIBERNATE_PRESETS
                    .iter()
                    .find(|(preset_id, _)| *preset_id == id)
                    .and_then(|(_, query)| {
                        let now = chrono::DateTime::from_timestamp_millis(now_ms)?
                            .with_timezone(&chrono::Local);
                        parse_hibernate_query(query, now)
                    });
                let Some(resolved) = resolved else {
                    self.notice = Some("That time is unavailable; choose another time".into());
                    self.palette_state.reminder_target = Some(target);
                    cx.notify();
                    return;
                };
                Some(resolved.timestamp_ms)
            };
            match target {
                ReminderTarget::Session(key) => {
                    let query = self.inputs.palette_input.read(cx).value().to_string();
                    let note = if id.starts_with("pal-reminder-custom:") {
                        None
                    } else {
                        Some(query.as_str())
                    };
                    if let Some(session) = self.find_session_by_key(&key) {
                        if session.archived {
                            self.reactivate_session_key(&key, cx);
                        }
                        self.sidebar_state.workspace_store.hibernate_session(
                            &key,
                            &session,
                            at,
                            note,
                            self.palette_state.hibernate_wake_on_reply,
                        );
                    }
                }
                ReminderTarget::Workspace(id) => {
                    self.active_modal = ActiveModal::None;
                    self.set_workspace_snooze(&id, at, cx);
                    return;
                }
            }
            self.active_modal = ActiveModal::None;
            self.persist_workspace_store_if_enabled();
            self.sync_workspace_to_shared(cx);
            cx.notify();
            return;
        }

        let explicit_tab_target = self
            .palette_state
            .mode
            .target_session_key()
            .map(str::to_string);
        let target_key = explicit_tab_target
            .clone()
            .or_else(|| self.target_triage_session_key());

        self.active_modal = ActiveModal::None;
        self.palette_state.mode = PaletteMode::Global;
        self.palette_state.reminder_target = None;
        self.palette_state.directory_picker = None;
        self.palette_state.dissolve_workspace_id = None;

        match row {
            PaletteRow::Header(_) => {}
            PaletteRow::Session { session_key, .. } => {
                let reactivate = self
                    .find_session_by_key(&session_key)
                    .is_some_and(|session| {
                        session.agent == AgentKind::Shell
                            || session.archived
                            || self.sidebar_state.workspace_store.session_lifecycle(
                                &session_key,
                                &session,
                                now_ms,
                            ) != multibot_core::workspaces::SessionLifecycle::Active
                    });
                if !reactivate || self.reactivate_session_key(&session_key, cx) {
                    self.open_session_key_with_tracking(&session_key, false, cx);
                }
            }
            PaletteRow::Workspace {
                workspace_id,
                view,
                archived,
                ..
            } => {
                if archived {
                    let _ = self
                        .sidebar_state
                        .workspace_store
                        .restore_workspace(&workspace_id);
                }
                self.sidebar_state
                    .workspace_store
                    .wake_workspace(&workspace_id);
                self.sidebar_state.active_workspace_id = workspace_id;
                self.sidebar_state.active_workspace_view = view;
                self.sidebar_state.rename = None;
                self.cleanup_unmounted_surface_state();
                self.persist_workspace_store_if_enabled();
                self.sync_workspace_to_shared(cx);
                cx.notify();
            }
            PaletteRow::Command { id, .. } => {
                self.palette_state.stats.record_command_use(&id, now_ms);
                self.persist_palette_stats_if_enabled();

                if id == "pal-cmd-switch-provider" {
                    if let Some(window) = window {
                        self.open_provider_picker(window, cx);
                    }
                    return;
                }
                if self.execute_update_palette_command(&id, cx) {
                    cx.notify();
                    return;
                }

                if let Some(ws_id) = id.strip_prefix("pal-cmd-wake:") {
                    self.sidebar_state.workspace_store.wake_workspace(ws_id);
                    self.sidebar_state.active_workspace_id = ws_id.to_string();
                    self.sidebar_state.rename = None;
                    self.cleanup_unmounted_surface_state();
                    self.persist_workspace_store_if_enabled();
                    self.sync_workspace_to_shared(cx);
                    cx.notify();
                    return;
                }

                if let Some(ws_id) = id.strip_prefix("pal-cmd-move-ws:") {
                    if let Some(ref key) = target_key {
                        self.sidebar_state
                            .workspace_store
                            .assign_session_workspace(key, ws_id);
                        self.cleanup_unmounted_surface_state();
                        self.persist_workspace_store_if_enabled();
                        self.sync_workspace_to_shared(cx);
                    }
                    cx.notify();
                    return;
                }

                match id.as_str() {
                    "pal-cmd-workspaces" => {
                        if let Some(window) = window {
                            self.open_workspace_switcher(window, cx);
                        }
                        return;
                    }
                    "pal-cmd-account-sign-in" | "pal-cmd-account-sign-out" => {
                        self.settings_tab = crate::settings::SettingsTab::Account;
                        self.set_main_surface(MainSurface::Settings);
                        let job = if id == "pal-cmd-account-sign-in" {
                            crate::account::AccountJob::SignIn
                        } else {
                            self.voice.cancel();
                            crate::account::AccountJob::SignOut
                        };
                        self.start_account_job(job, cx);
                    }
                    "pal-cmd-inbox" => {
                        self.sidebar_state.rename = None;
                        self.sidebar_state.new_workspace_draft = None;
                        // Opening the Inbox always lands on `All`, never on a hidden empty split
                        self.inbox_state.select_all_split();
                        self.inbox_state.lifecycle =
                            multibot_core::workspaces::SessionLifecycle::Active;
                        self.set_main_surface(MainSurface::Inbox);
                        if let Some(win) = window {
                            self.focus_inbox(win, cx);
                        }
                    }
                    "pal-cmd-chat-info" => {
                        if let Some(win) = window {
                            self.toggle_chat_info(win, cx);
                        }
                        return;
                    }
                    "pal-cmd-dictation" => {
                        if let Some(win) = window {
                            self.toggle_dictation(win, cx);
                        }
                        return;
                    }
                    "pal-cmd-fullwidth" => {
                        self.toggle_shell_fullscreen(cx);
                    }
                    "pal-cmd-toggle-terminal" => {
                        self.toggle_terminal_pane(cx);
                        return;
                    }
                    "pal-cmd-browser-toggle" => {
                        if let Some(win) = window {
                            self.toggle_browser_pane(win, cx);
                        }
                    }
                    "pal-cmd-browser-swap" => {
                        if let Some(win) = window {
                            self.focus_browser_pane(win, cx);
                        }
                    }
                    "pal-cmd-toggle-computer-view" => {
                        // The palette runs over the chat surface; the pane docks beside it.
                        if let Some(win) = window {
                            self.toggle_computer_view(win, cx);
                        }
                        return;
                    }
                    "pal-cmd-hosts" => {
                        self.settings_tab = crate::settings::SettingsTab::Hosts;
                        self.set_main_surface(MainSurface::Settings);
                    }
                    "pal-cmd-settings" => {
                        self.sidebar_state.rename = None;
                        self.sidebar_state.new_workspace_draft = None;
                        self.set_main_surface(MainSurface::Settings);
                    }
                    "pal-cmd-sidebar" => {
                        self.cycle_sidebar_mode_synced(cx);
                        if let Some(win) = window
                            && self.sidebar_state.mode == SidebarMode::Open
                            && (self.is_sidebar_rename_mounted()
                                || self.is_sidebar_workspace_draft_mounted())
                        {
                            self.focus_sidebar(win, cx);
                        }
                        return;
                    }
                    "pal-cmd-sidebar-open" => {
                        self.set_sidebar_mode_synced(SidebarMode::Open, cx);
                        if let Some(win) = window
                            && (self.is_sidebar_rename_mounted()
                                || self.is_sidebar_workspace_draft_mounted())
                        {
                            self.focus_sidebar(win, cx);
                        }
                        return;
                    }
                    "pal-cmd-sidebar-rail" => {
                        self.set_sidebar_mode_synced(SidebarMode::Collapsed, cx);
                        return;
                    }
                    "pal-cmd-sidebar-hide" => {
                        self.set_sidebar_mode_synced(SidebarMode::Hidden, cx);
                        return;
                    }
                    "pal-cmd-unread-only" => {
                        self.toggle_unread_only(cx);
                        return;
                    }
                    "pal-cmd-group-toggle" => {
                        if let Some(ref key) = target_key {
                            let ws_id = if self.sidebar_state.active_workspace_id
                                == multibot_core::workspaces::ALL_WORKSPACES_ID
                            {
                                self.sidebar_state
                                    .workspace_store
                                    .effective_workspace_id(key)
                            } else {
                                self.sidebar_state.active_workspace_id.clone()
                            };
                            if self
                                .sidebar_state
                                .workspace_store
                                .toggle_session_group_collapsed(key, &ws_id)
                                .is_some()
                            {
                                self.sidebar_state.rename = None;
                                self.cleanup_unmounted_surface_state();
                                self.persist_workspace_store_if_enabled();
                            }
                        } else {
                            self.toggle_active_group(cx);
                            return;
                        }
                    }
                    "pal-cmd-group-assign" => {
                        if let Some(ref key) = target_key {
                            let ws_id = if self.sidebar_state.active_workspace_id
                                == multibot_core::workspaces::ALL_WORKSPACES_ID
                            {
                                self.sidebar_state
                                    .workspace_store
                                    .effective_workspace_id(key)
                            } else {
                                self.sidebar_state.active_workspace_id.clone()
                            };
                            self.sidebar_state
                                .workspace_store
                                .toggle_group_for_session(key, &ws_id, None, "Group");
                            self.cleanup_unmounted_surface_state();
                            self.persist_workspace_store_if_enabled();
                        }
                    }
                    "pal-cmd-window" => {
                        self.open_extra_window(cx);
                    }
                    "pal-cmd-import" => {
                        self.import_from_zigshel(cx);
                    }
                    "pal-cmd-shortcuts" => {
                        self.active_modal = ActiveModal::Shortcuts;
                    }
                    "pal-cmd-ws-starred" => {
                        self.switch_workspace_digit(1, cx);
                        return;
                    }
                    "pal-cmd-ws-new" => {
                        let next_num = self.sidebar_state.workspace_store.workspaces.len() + 1;
                        if let Ok(ws) = self
                            .sidebar_state
                            .workspace_store
                            .create_workspace(&format!("Workspace {next_num}"), None)
                        {
                            self.sidebar_state.active_workspace_id = ws.id;
                            self.sidebar_state.rename = None;
                            self.cleanup_unmounted_surface_state();
                            self.persist_workspace_store_if_enabled();
                        }
                    }
                    "pal-cmd-remind" => {
                        if let (Some(key), Some(window)) = (target_key, window) {
                            self.open_reminder_picker(ReminderTarget::Session(key), window, cx);
                        }
                        return;
                    }
                    "pal-cmd-ws-snooze" => {
                        let id = self.sidebar_state.active_workspace_id.clone();
                        if let Some(window) = window {
                            self.open_reminder_picker(ReminderTarget::Workspace(id), window, cx);
                        }
                        return;
                    }
                    "workspace.dissolve" => {
                        if let Some(window) = window {
                            self.request_dissolve_workspace(window, cx);
                        }
                        return;
                    }
                    "workspace.delete" => {
                        let cur = self.sidebar_state.active_workspace_id.clone();
                        let keys: Vec<_> = self
                            .session_inputs()
                            .into_iter()
                            .filter(|input| {
                                self.sidebar_state
                                    .workspace_store
                                    .effective_workspace_id(&input.key)
                                    == cur
                            })
                            .map(|input| input.key)
                            .collect();
                        for key in keys {
                            self.sidebar_state
                                .workspace_store
                                .assign_session_workspace(&key, &cur);
                        }
                        let store = &mut self.sidebar_state.workspace_store;
                        let general = store
                            .workspaces
                            .iter()
                            .find(|w| {
                                w.id != cur && !w.archived && w.name == DEFAULT_WORKSPACE_NAME
                            })
                            .map(|w| w.id.clone())
                            .or_else(|| {
                                store
                                    .create_workspace(DEFAULT_WORKSPACE_NAME, None)
                                    .ok()
                                    .map(|w| w.id)
                            });
                        if self
                            .sidebar_state
                            .workspace_store
                            .delete_workspace(&cur, general.as_deref())
                            .is_ok()
                        {
                            self.sidebar_state.active_workspace_id =
                                self.sidebar_state.workspace_store.default_workspace_id();
                            self.sidebar_state.rename = None;
                            self.cleanup_unmounted_surface_state();
                            self.persist_workspace_store_if_enabled();
                        }
                    }
                    "pal-cmd-rename" => {
                        if let Some(key) = explicit_tab_target {
                            // Tab-context rename for an explicit session `key`: never silently
                            // hijack a different Inbox row or start an unmounted rename.
                            if let Some(sess) = self.find_session_by_key(&key) {
                                if self.sidebar_state.mode == SidebarMode::Open
                                    && self.is_session_rendered_in_open_sidebar(&key)
                                {
                                    if self.main_surface == MainSurface::Chat {
                                        self.dismiss_conflicting_chat_overlays_for_rename();
                                    }
                                    if let Some(win) = window {
                                        self.start_sidebar_rename_with_window(key, win, cx);
                                    } else {
                                        self.start_sidebar_rename(key, cx);
                                    }
                                } else if self.main_surface == MainSurface::Inbox
                                    && self.is_session_rendered_in_inbox(&key)
                                {
                                    let rows = self.compute_inbox_rows(now_ms);
                                    if let Some(idx) =
                                        rows.iter().position(|r| r.session_key == key)
                                    {
                                        self.inbox_state.selected_row = idx;
                                    }
                                    let current = self
                                        .sidebar_state
                                        .workspace_store
                                        .effective_title(&key, &sess.title)
                                        .to_string();
                                    self.sidebar_state.rename = None;
                                    self.inbox_state.rename =
                                        Some(InlineRenameState::new(key, sess.id, current));
                                    if let Some(win) = window {
                                        self.focus_inbox(win, cx);
                                    }
                                    cx.notify();
                                } else {
                                    // Target is not mounted in Open sidebar or Inbox (e.g.
                                    // right-clicked a Collapsed rail avatar while on Inbox or
                                    // another Chat session): switch to Chat for that session so
                                    // `#header-rename-input` is visibly mounted and focused.
                                    self.open_session_key_with_tracking(&key, false, cx);
                                    self.dismiss_conflicting_chat_overlays_for_rename();
                                    if let Some(win) = window {
                                        self.start_sidebar_rename_with_window(key, win, cx);
                                    } else {
                                        self.start_sidebar_rename(key, cx);
                                    }
                                }
                            }
                            return;
                        }

                        // Global `Ctrl+K -> Rename session` (no explicit tab target).
                        if self.main_surface == MainSurface::Inbox {
                            if let Some(inbox_key) = self.selected_inbox_session_key()
                                && self.is_session_rendered_in_inbox(&inbox_key)
                                && let Some(sess) = self.find_session_by_key(&inbox_key)
                            {
                                let current = self
                                    .sidebar_state
                                    .workspace_store
                                    .effective_title(&inbox_key, &sess.title)
                                    .to_string();
                                self.sidebar_state.rename = None;
                                self.inbox_state.rename =
                                    Some(InlineRenameState::new(inbox_key, sess.id, current));
                                if let Some(win) = window {
                                    self.focus_inbox(win, cx);
                                }
                                cx.notify();
                            } else if self.sidebar_state.mode == SidebarMode::Open
                                && let Some(key) = target_key
                                && self.is_session_rendered_in_open_sidebar(&key)
                            {
                                if let Some(win) = window {
                                    self.start_sidebar_rename_with_window(key, win, cx);
                                } else {
                                    self.start_sidebar_rename(key, cx);
                                }
                            }
                            return;
                        }

                        if let Some(key) = target_key {
                            if self.can_mount_sidebar_rename(&key) {
                                if self.main_surface == MainSurface::Chat {
                                    self.dismiss_conflicting_chat_overlays_for_rename();
                                }
                                if let Some(win) = window {
                                    self.start_sidebar_rename_with_window(key, win, cx);
                                } else {
                                    self.start_sidebar_rename(key, cx);
                                }
                            }
                            return;
                        }
                    }
                    "pal-cmd-favorite" => {
                        if let Some(ref key) = target_key {
                            self.sidebar_state.workspace_store.toggle_favorite(key);
                            self.cleanup_unmounted_surface_state();
                            self.persist_workspace_store_if_enabled();
                        } else {
                            self.toggle_active_favorite(cx);
                            return;
                        }
                    }
                    "pal-cmd-pin" => {
                        if let Some(ref key) = target_key {
                            let in_starred =
                                self.sidebar_state.active_workspace_id == STARRED_WORKSPACE_ID;
                            self.sidebar_state
                                .workspace_store
                                .toggle_pin(key, in_starred);
                            self.cleanup_unmounted_surface_state();
                            self.persist_workspace_store_if_enabled();
                        }
                    }
                    "pal-cmd-mark-unread" => {
                        if let Some(ref key) = target_key
                            && let Some(sess) = self.find_session_by_key(key)
                        {
                            self.sidebar_state
                                .workspace_store
                                .toggle_session_unread(key, &sess);
                            self.cleanup_unmounted_surface_state();
                            self.persist_workspace_store_if_enabled();
                        }
                    }
                    "pal-cmd-waiting" => {
                        if let Some(ref key) = target_key {
                            self.mark_done_or_inbox(key, cx);
                        }
                    }
                    "pal-cmd-undo-waiting" => {
                        if let Some(key) = self.sidebar_state.workspace_store.undo_last_waiting() {
                            self.reactivate_session_key(&key, cx);
                            self.persist_workspace_store_if_enabled();
                        }
                    }
                    "pal-cmd-inbox-exclude" => {
                        if let Some(ref key) = target_key {
                            self.sidebar_state
                                .workspace_store
                                .toggle_inbox_excluded(key);
                            self.cleanup_unmounted_surface_state();
                            self.persist_workspace_store_if_enabled();
                        }
                    }
                    "session.delete" => {
                        if let Some(ref key) = target_key {
                            self.dissolve_session_key(key, cx);
                        }
                    }
                    "pal-cmd-archive-session" => {
                        if let Some(ref key) = target_key {
                            self.reactivate_session_key(key, cx);
                        }
                    }
                    "pal-view-done" | "pal-view-hibernated" | "pal-view-dissolved" => {
                        let lifecycle = match id.as_str() {
                            "pal-view-done" => multibot_core::workspaces::SessionLifecycle::Done,
                            "pal-view-hibernated" => {
                                multibot_core::workspaces::SessionLifecycle::Hibernated
                            }
                            _ => multibot_core::workspaces::SessionLifecycle::Dissolved,
                        };
                        if let Some(window) = window {
                            self.open_lifecycle_view(lifecycle, window, cx);
                        }
                    }
                    "pal-cmd-copy-id" => {
                        if let Some(ref key) = target_key {
                            cx.write_to_clipboard(ClipboardItem::new_string(key.clone()));
                            self.notice = Some(format!("Session ID: {key}"));
                        }
                    }
                    "pal-cmd-refresh" => {
                        self.refresh_all_hosts();
                    }
                    "pal-cmd-usage" => {
                        self.notice = Some(format!(
                            "Multibot v{} · {} · {}",
                            multibot_proto::build_version(),
                            self.footprint_label,
                            self.connection.connection_status
                        ));
                    }
                    "pal-cmd-reply-message" | "pal-cmd-copy-message" => {
                        if let Some(window) = window {
                            let action = if id == "pal-cmd-reply-message" {
                                MessageMenuAction::Reply
                            } else {
                                MessageMenuAction::Copy
                            };
                            self.execute_focused_message_action(action, window, cx);
                        }
                        return;
                    }
                    "pal-cmd-copy-code" => {
                        self.copy_latest_code_block(cx);
                        return;
                    }
                    "pal-cmd-expand-table" => {
                        if self.expand_best_table(cx)
                            && let Some(win) = window
                        {
                            self.focus_table_modal(win, cx);
                        }
                        return;
                    }
                    "pal-cmd-toggle-tools" => {
                        self.toggle_all_tool_steps(cx);
                        return;
                    }
                    "pal-cmd-toggle-todos" => {
                        self.toggle_todo_drawer(cx);
                        return;
                    }
                    "pal-cmd-load-older" => {
                        self.load_older_history(cx);
                        return;
                    }
                    "pal-cmd-interrupt" => {
                        self.interrupt_active_session(cx);
                        return;
                    }
                    "pal-cmd-spawn-claude"
                    | "pal-cmd-spawn-codex"
                    | "pal-cmd-spawn-antigravity"
                    | "pal-cmd-spawn-pi"
                    | "pal-cmd-spawn-shell" => {
                        let agent = match id.as_str() {
                            "pal-cmd-spawn-claude" => AgentKind::Claude,
                            "pal-cmd-spawn-codex" => AgentKind::Codex,
                            "pal-cmd-spawn-antigravity" => AgentKind::Antigravity,
                            "pal-cmd-spawn-shell" => AgentKind::Shell,
                            _ => AgentKind::Pi,
                        };
                        self.start_agent_session(agent, window, cx);
                        return;
                    }
                    "pal-cmd-unarchive-session" => {
                        if let Some(ref key) = target_key
                            && self.find_session_by_key(key).is_some_and(|session| {
                                self.sidebar_state.workspace_store.session_lifecycle(
                                    key,
                                    &session,
                                    now_unix_ms(),
                                ) == multibot_core::workspaces::SessionLifecycle::Dissolved
                            })
                        {
                            self.reactivate_session_key(key, cx);
                        } else {
                            self.reactivate_last_dissolved(cx);
                        }
                        return;
                    }
                    _ => {}
                }
                self.sync_workspace_to_shared(cx);
                cx.notify();
            }
        }
    }
}
