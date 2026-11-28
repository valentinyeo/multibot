use super::*;

impl MultibotView {
    pub(crate) fn clear_active_session_view(&mut self) {
        self.chat_state.pending_harness_switch = None;
        self.voice.cancel();
        self.selection.selected_session = None;
        self.selection.selected_session_host = None;
        self.sidebar_state.rename = None;
        self.history.active_switch_token = None;
        self.history.messages.clear();
        self.chat_state.close_find();
        self.slash_picker_state.close();
        self.mention_picker_state.close();
        self.chat_state.reply_quote = None;
        self.chat_state.pending_attachments.clear();
        self.chat_state.focused_message_idx = None;
        self.chat_state.message_action_menu_open = false;
        self.chat_state.message_action_index = 0;
        self.history.remote_search_hits.clear();
        self.history.remote_search_truncated = false;
        self.cleanup_unmounted_surface_state();
    }

    #[cfg(test)]
    pub(crate) fn select_session(&mut self, session: SessionSummary, cx: &mut Context<Self>) {
        let host = self.host_for_summary(&session);
        self.select_session_on_host(host, session, cx);
    }

    pub(crate) fn select_session_on_host(
        &mut self,
        host: String,
        session: SessionSummary,
        cx: &mut Context<Self>,
    ) {
        self.sidebar_state.pending_shell_dissolve = None;
        if self.selection.selected_session_host.as_deref() != Some(host.as_str())
            || self
                .selection
                .selected_session
                .as_ref()
                .is_none_or(|selected| selected.id != session.id || selected.agent != session.agent)
        {
            self.notice = None;
        }
        self.voice.cancel();
        if self
            .selection
            .selected_session_host
            .as_deref()
            .is_some_and(|h| h != host)
        {
            self.history.pending_switch_tokens.clear();
            self.history.pending_older_page_before.clear();
            self.chat_state.session_todos.clear();
        }
        self.selection.selected_session_host = Some(host.clone());
        self.chat_state.scroll.open_visit(UnreadVisit::Read);
        self.chat_state.scroll_handle = gpui_kit::ScrollHandle::default();
        self.chat_state.prepend_scroll_anchor = None;
        self.chat_state.newest_chord.reset();
        if session.agent == AgentKind::Shell && !session.archived {
            self.shell_host.prepare_selection(&host);
            self.select_embedded_shell(&host, &session.id);
        }
        self.chat_state.composer_menu_open = false;
        // Reset per-session transient Chat View overlays, active inline rename & stale pagination
        self.sidebar_state.rename = None;
        self.history.pending_older_page_before.remove(&session.id);
        self.chat_state.close_find();
        self.slash_picker_state.close();
        self.mention_picker_state.close();
        self.chat_state.reply_quote = None;
        self.chat_state.pending_attachments.clear();
        self.chat_state.focused_message_idx = None;
        self.chat_state.message_action_menu_open = false;
        self.chat_state.message_action_index = 0;

        let key = CacheKey::new(&host, session.agent, &session.id);

        // Frame-0 synchronous switch: Tier 1 memory (<1ms) -> Tier 2 bounded disk (<5ms)
        let (token, mem_hit) = self.history.chat_cache.switch_memory(key.clone());
        let cached = match mem_hit {
            Some(s) => Some(s.clone()),
            None => self.history.chat_cache.get_or_load(&key).cloned(),
        };
        let known_probe = cached.as_ref().and_then(|s| s.probe_sha256.clone());

        if let Some(snapshot) = cached {
            self.history.messages = snapshot.messages;
            if !snapshot.todos.is_empty() {
                self.chat_state
                    .session_todos
                    .insert(session.id.clone(), snapshot.todos);
            }
        } else {
            self.history.messages.clear();
        }
        self.chat_state
            .reconcile_local_reactions(&self.history.messages);

        self.history.active_switch_token = Some(token.clone());
        self.history
            .pending_switch_tokens
            .insert(session.id.clone(), token);
        self.selection.selected_session = Some(session.clone());
        let full_width = self
            .sidebar_state
            .workspace_store
            .session_meta
            .get(&canonical_session_key(&host, session.agent, &session.id))
            .is_some_and(|meta| meta.full_width);
        self.apply_session_full_width(full_width);
        self.focus.pending_focus_input = true;
        self.set_main_surface(MainSurface::Chat);
        self.active_modal = ActiveModal::None;
        self.history.remote_search_hits.clear();
        self.history.remote_search_truncated = false;

        if let Some(tx) = self.active_session_tx() {
            let _ = tx.send(ClientRequest::ReadHistory {
                session_id: session.id.clone(),
                source_path: session.source_path.clone(),
                agent: session.agent,
                tail: Some(200),
                before_offset: None,
                max_bytes: Some(MAX_SESSION_CACHE_BYTES),
                known_probe,
            });
            if self.sidebar_state.workspace_store.session_lifecycle(
                &self.session_key_for(&session),
                &session,
                now_unix_ms(),
            ) != multibot_core::workspaces::SessionLifecycle::Dissolved
            {
                let _ = tx.send(ClientRequest::WatchSession {
                    session_id: session.id.clone(),
                    source_path: session.source_path.clone(),
                    agent: session.agent,
                });
                let _ = tx.send(ClientRequest::ListCommands {
                    agent: session.agent,
                    cwd: session.cwd.clone(),
                });
            }
        }
        cx.notify();
    }

    pub(crate) fn default_spawn_cwd(&self) -> String {
        self.selection
            .selected_session
            .as_ref()
            .and_then(|s| s.cwd.clone())
            .filter(|c| !c.trim().is_empty())
            .unwrap_or_else(|| ".".to_string())
    }

    pub(crate) fn interrupt_active_session(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(ref sess) = self.selection.selected_session else {
            return false;
        };
        let Some(tx) = self.active_session_tx().cloned() else {
            self.notice = Some("Connect to a host first.".to_string());
            cx.notify();
            return false;
        };
        let ok = tx
            .send(ClientRequest::InterruptSession {
                session_id: sess.id.clone(),
                agent: sess.agent,
            })
            .is_ok();
        if ok {
            self.notice = Some(format!("Stop requested for {}.", sess.title));
        }
        cx.notify();
        ok
    }

    pub(crate) fn spawn_new_session(
        &mut self,
        agent: AgentKind,
        cwd: String,
        title: Option<String>,
        initial_prompt: Option<String>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(tx) = self.active_session_tx().cloned() else {
            self.notice = Some("Connect to a host before starting a new session.".to_string());
            cx.notify();
            return false;
        };
        let options = self.spawn_composer_options(agent);
        let ok = tx
            .send(ClientRequest::SpawnSession {
                agent,
                cwd,
                title,
                initial_prompt,
                options: options.clone(),
            })
            .is_ok();
        if ok {
            self.selection.pending_spawn_open = true;
            self.chat_state.pending_spawn_options = options;
            self.selection.pending_spawn_host = Some(self.active_session_host().to_string());
            self.notice = Some(format!("Starting new {} session...", agent.label()));
        }
        cx.notify();
        ok
    }

    pub(crate) fn unarchive_session_on_host(
        &mut self,
        host: &str,
        session_id: String,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(tx) = self.tx_for_host(host) else {
            return false;
        };
        let ok = tx
            .send(ClientRequest::UnarchiveSession { session_id })
            .is_ok();
        cx.notify();
        ok
    }

    pub(crate) fn request_daemon_commands(&self, agent: AgentKind, cwd: Option<String>) {
        if let Some(tx) = self.active_session_tx() {
            let _ = tx.send(ClientRequest::ListCommands { agent, cwd });
        }
    }

    // =========================================================================
    // Workspace-Ordered Session Jumping & Cycling
    // =========================================================================

    pub(crate) fn active_ctrl_digit_order(&self) -> Vec<String> {
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
                now_unix_ms(),
            );
        layout.ctrl_digit_order
    }

    pub(crate) fn jump_session_index(&mut self, index: usize, cx: &mut Context<Self>) {
        let order = self.active_ctrl_digit_order();
        if let Some(key) = order.get(index) {
            self.open_session_key_with_tracking(key, false, cx);
            self.sync_workspace_to_shared(cx);
        } else if order.is_empty()
            && let Some(session) = self.connection.sessions.get(index).cloned()
        {
            self.open_session_with_tracking(session, false, cx);
            self.sync_workspace_to_shared(cx);
        }
    }

    pub(crate) fn cycle_session(&mut self, delta: isize, cx: &mut Context<Self>) {
        let order = self.active_ctrl_digit_order();
        if order.is_empty() {
            return;
        }
        let active_key = self
            .selection
            .selected_session
            .as_ref()
            .map(|s| self.session_key_for(s));
        let current = active_key
            .as_deref()
            .and_then(|k| order.iter().position(|x| x == k));
        let next = current
            .map(|current| (current as isize + delta).rem_euclid(order.len() as isize) as usize)
            .unwrap_or(if delta < 0 { order.len() - 1 } else { 0 });
        if let Some((host, session)) = self.host_for_session_key(&order[next]) {
            if active_key.as_deref() != Some(order[next].as_str()) {
                self.sidebar_state.previous_session_key = active_key;
                // Frequent navigation must not change the ranking it is traversing.
                if self.sidebar_state.active_workspace_view != WorkspaceView::Frequent {
                    self.sidebar_state
                        .workspace_store
                        .record_frequent_visit(&order[next], now_unix_ms());
                    self.persist_workspace_store_if_enabled();
                    self.sync_workspace_to_shared(cx);
                }
            }
            self.inbox_state.step_index = None;
            self.inbox_state.step_order.clear();
            // Tracking a view changes MRU order; marking read also removes rows in unread-only mode.
            self.select_session_on_host(host, session, cx);
        }
    }

    pub(crate) fn open_extra_window(&self, cx: &mut App) {
        let bounds = Bounds::centered(None, size(px(1140.0), px(740.0)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some(format!("{} — Workspace", crate::window_title()).into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let rt = self.runtime.clone();
        let persist_to_disk = cx
            .try_global::<SharedAppStore>()
            .is_some_and(|s| s.persist_to_disk);
        let _ = gpui_kit::open_window(options, cx, move |window, cx| {
            cx.new(|cx| {
                let mut view = MultibotView::new(rt, window, cx);
                if persist_to_disk {
                    view.enable_disk_persistence();
                }
                view
            })
        });
    }

    pub(crate) fn agent_color(agent: AgentKind) -> gpui_kit::Hsla {
        match agent {
            AgentKind::Claude => gpui_kit::rgb(0xD97757).into(),
            AgentKind::Antigravity => gpui_kit::rgb(0x60A5FA).into(),
            AgentKind::Codex => gpui_kit::rgb(0x34D399).into(),
            AgentKind::Shell => gpui_kit::rgb(0xE2E8F0).into(),
            AgentKind::Pi => gpui_kit::rgb(0xC084FC).into(),
            AgentKind::Cursor => gpui_kit::rgb(0xFACC15).into(),
        }
    }

    pub(crate) fn selected_inbox_session_key(&self) -> Option<String> {
        let rows = self.compute_inbox_rows(now_unix_ms());
        rows.get(self.inbox_state.selected_row)
            .or_else(|| rows.first())
            .map(|r| r.session_key.clone())
    }

    pub(crate) fn target_triage_session_key(&self) -> Option<String> {
        if self.main_surface == MainSurface::Inbox {
            self.selected_inbox_session_key()
        } else {
            self.selection
                .selected_session
                .as_ref()
                .map(|s| self.session_key_for(s))
        }
    }
}
