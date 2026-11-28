use super::*;

impl MultibotView {
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

    pub(crate) fn sidebar_neighbor_after_removal(
        &self,
        key: &str,
        direction: ReorderDirection,
    ) -> Option<String> {
        let order = self.active_ctrl_digit_order();
        let index = order.iter().position(|candidate| candidate == key)?;
        let above = index.checked_sub(1).and_then(|i| order.get(i));
        let below = order.get(index + 1);
        match direction {
            ReorderDirection::Up => above.or(below),
            ReorderDirection::Down => below.or(above),
        }
        .cloned()
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
        if let Some(ref current) = self.selection.selected_session {
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
                previous_boundary: self
                    .sidebar_state
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
        if !self.history.messages.is_empty() {
            self.chat_state
                .scroll
                .history_loaded(&self.history.messages, true);
        }
    }

    pub(crate) fn toggle_active_favorite(&mut self, cx: &mut Context<Self>) {
        if let Some(sess) = self.selection.selected_session.clone() {
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
        self.sidebar_state.rename = None;
        self.cleanup_unmounted_surface_state();
        cx.notify();
    }

    pub(crate) fn set_workspace_view(&mut self, view: WorkspaceView, cx: &mut Context<Self>) {
        self.sidebar_state.active_workspace_view = view;
        self.sidebar_state.rename = None;
        self.cleanup_unmounted_surface_state();
        cx.notify();
    }

    pub(crate) fn reorder_active_session(
        &mut self,
        direction: ReorderDirection,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(sel) = self.selection.selected_session.clone() else {
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
        let Some(sel) = self.selection.selected_session.clone() else {
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
}
