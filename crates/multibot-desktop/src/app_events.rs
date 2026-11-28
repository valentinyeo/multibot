use super::*;

impl MultibotView {
    /// Handles daemon `ServerEvent`s fanned out from the App-scoped connection consumer (or from
    /// another `Ctrl+N` window in tests) uniformly across ALL open windows without spurious redraws.
    pub(crate) fn handle_daemon_server_event_with_hint(
        &mut self,
        host_label: &str,
        event: ServerEvent,
        is_older_page_hint: bool,
        cx: &mut Context<Self>,
    ) {
        self.sync_from_shared_store(cx);
        if !is_older_page_hint {
            self.observe_turn_event(host_label, &event);
        }
        let should_flush = self.handle_outbox_event(host_label, &event);
        if let ServerEvent::Error { code, message } = &event
            && code == "switch_shell_client_failed"
        {
            if self.shell_host.switch_failed(message) {
                cx.notify();
            }
            return;
        }
        let matches_active_host = self
            .selection
            .selected_session_host
            .as_deref()
            .is_none_or(|h| h == host_label);
        if !matches_active_host
            && matches!(
                &event,
                ServerEvent::History { .. }
                    | ServerEvent::HistoryAppend { .. }
                    | ServerEvent::SearchResults { .. }
                    | ServerEvent::PromptStatus { .. }
                    | ServerEvent::SendAccepted { .. }
                    | ServerEvent::Commands { .. }
                    | ServerEvent::Error { .. }
            )
        {
            if should_flush {
                self.flush_outbox_for_host(host_label);
            }
            return;
        }
        let mut changed = false;
        match event {
            ServerEvent::BrowserCommand {
                request_id,
                session_id,
                binding_id,
                command,
            } => {
                self.handle_browser_command(
                    host_label,
                    &session_id,
                    &binding_id,
                    request_id,
                    command,
                );
                return;
            }
            ServerEvent::HelloAck { .. } => {
                self.rebind_browsers(host_label);
                self.flush_outbox_for_host(host_label);
                if matches_active_host && let Some(ref sel) = self.selection.selected_session {
                    self.request_daemon_commands(sel.agent, sel.cwd.clone());
                }
            }
            ServerEvent::Sessions { .. } => {
                if self.selection.selected_session.is_none()
                    && let Some(first) = self
                        .connection
                        .sessions
                        .iter()
                        .find(|s| {
                            self.sidebar_state.workspace_store.session_lifecycle(
                                &self.session_key_for(s),
                                s,
                                now_unix_ms(),
                            ) == multibot_core::workspaces::SessionLifecycle::Active
                        })
                        .cloned()
                {
                    let host = self.host_for_summary(&first);
                    self.open_session_on_host(host, first, false, cx);
                }
                self.flush_outbox_for_host(host_label);
            }
            ServerEvent::SessionUpdated { session } => {
                if matches_active_host
                    && self
                        .selection
                        .selected_session
                        .as_ref()
                        .is_some_and(|s| s.id == session.id && s.agent == session.agent)
                    && self.main_surface == MainSurface::Chat
                {
                    let key = self.session_key_for(&session);
                    let prev_boundary = self
                        .sidebar_state
                        .workspace_store
                        .unread_boundaries
                        .get(&key)
                        .cloned();
                    self.sidebar_state
                        .workspace_store
                        .mark_session_read(&key, &session);
                    if self
                        .sidebar_state
                        .workspace_store
                        .unread_boundaries
                        .get(&key)
                        != prev_boundary.as_ref()
                    {
                        self.persist_workspace_store_if_enabled();
                        self.sync_workspace_to_shared(cx);
                        changed = true;
                    }
                }
            }
            ServerEvent::Harnesses { choices } => {
                self.chat_state
                    .harness_choices
                    .insert(host_label.into(), choices);
                changed =
                    self.active_modal == ActiveModal::Palette && self.palette_state.provider_picker;
            }
            ServerEvent::HarnessSwitched { previous, session } => {
                self.apply_harness_switch(host_label, previous, session, cx);
                changed = true;
            }
            ServerEvent::SessionSpawned { session } => {
                if (self.selection.pending_spawn_open
                    && self
                        .selection
                        .pending_spawn_host
                        .as_deref()
                        .is_none_or(|h| h == host_label))
                    || self.selection.selected_session.is_none()
                {
                    self.selection.pending_spawn_open = false;
                    self.selection.pending_spawn_host = None;
                    if let Some(options) = self.chat_state.pending_spawn_options.take() {
                        let key = canonical_session_key(host_label, session.agent, &session.id);
                        self.chat_state.composer_options.insert(key, options);
                    }
                    self.open_session_on_host(host_label.to_string(), session, false, cx);
                }
            }
            ev @ ServerEvent::History { .. } => {
                changed = self.apply_history_event(ev, is_older_page_hint);
            }
            ev @ ServerEvent::HistoryAppend { .. } => {
                let (vis, _) = self.apply_history_append_event(ev);
                changed = vis;
            }
            ServerEvent::Commands {
                agent,
                cwd: _,
                items,
            } => {
                self.slash_picker_state.set_daemon_commands(agent, items);
                changed = true;
            }
            ServerEvent::SearchResults {
                request_id,
                session_id,
                hits,
                truncated,
            } => {
                if self.history.remote_search_request_id.as_deref() == Some(request_id.as_str())
                    && self
                        .selection
                        .selected_session
                        .as_ref()
                        .is_some_and(|s| s.id == session_id)
                {
                    self.history.remote_search_hits = hits;
                    self.history.remote_search_truncated = truncated;
                    let current_oldest = self
                        .history
                        .active_switch_token
                        .as_ref()
                        .and_then(|t| self.history.chat_cache.get(&t.key))
                        .map(|c| c.oldest_offset)
                        .unwrap_or(0);
                    if self.chat_state.find_matches.is_empty()
                        && self
                            .history
                            .remote_search_hits
                            .iter()
                            .any(|h| h.byte_offset < current_oldest)
                    {
                        self.load_older_history(cx);
                    }
                    changed = true;
                }
            }
            ServerEvent::SendAccepted { .. } => {
                if self.selection.last_dispatched_prompt.is_some() {
                    self.selection.last_dispatched_prompt = None;
                    changed = true;
                }
            }
            ServerEvent::PromptStatus {
                session_id,
                message_id,
                status,
                detail,
            } => {
                self.track_queued_prompt(&session_id, &message_id, &status);
                if matches_active_host
                    && self
                        .selection
                        .selected_session
                        .as_ref()
                        .is_some_and(|s| s.id == session_id)
                {
                    match status.as_str() {
                        "failed"
                            if self.selection.last_dispatched_prompt.is_some()
                                || (!message_id.is_empty()
                                    && self
                                        .history
                                        .messages
                                        .iter()
                                        .any(|m| m.id == message_id)) =>
                        {
                            let (msg_id, text) = if !message_id.is_empty() {
                                let matched_text = self
                                    .selection
                                    .last_dispatched_prompt
                                    .as_ref()
                                    .filter(|(id, _)| id == &message_id)
                                    .map(|(_, t)| t.clone())
                                    .or_else(|| {
                                        self.history
                                            .messages
                                            .iter()
                                            .rev()
                                            .find(|m| m.id == message_id)
                                            .map(|m| m.text.clone())
                                    })
                                    .unwrap_or_default();
                                if self
                                    .selection
                                    .last_dispatched_prompt
                                    .as_ref()
                                    .is_some_and(|(id, _)| id == &message_id)
                                {
                                    self.selection.last_dispatched_prompt = None;
                                }
                                (message_id, matched_text)
                            } else {
                                self.selection.last_dispatched_prompt.take().unwrap_or((
                                    format!("failed-{}", self.history.messages.len()),
                                    String::new(),
                                ))
                            };
                            let reason = if detail.is_empty() {
                                "Message delivery failed".to_string()
                            } else {
                                detail
                            };
                            self.record_send_failure(msg_id, text, reason, cx);
                            changed = true;
                        }
                        "cancelled" => {
                            self.notice = Some("Cancelled prompt.".to_string());
                            changed = true;
                        }
                        "sent" => {
                            self.selection.last_dispatched_prompt = None;
                        }
                        "queued" => {
                            self.chat_state.advance_busy_tick();
                            changed = true;
                        }
                        "not_found" => {
                            if !detail.is_empty() {
                                self.notice = Some(detail);
                                changed = true;
                            }
                        }
                        _ => {}
                    }
                }
            }
            ev @ ServerEvent::Error { .. } => {
                let ServerEvent::Error {
                    ref code,
                    ref message,
                } = ev
                else {
                    unreachable!();
                };
                if (code == "read_history_failed" || code == "watch_failed")
                    && let Some(token) = self.history.active_switch_token.clone()
                {
                    let _ = self
                        .history
                        .chat_cache
                        .reconcile_server_event(&token, "", &ev);
                }
                if code == "send_failed" {
                    if let Some((msg_id, text)) = self.selection.last_dispatched_prompt.take() {
                        self.record_send_failure(msg_id, text, message.clone(), cx);
                    } else if let Some(session) = &self.selection.selected_session {
                        let key = self.session_key_for(session);
                        self.finish_turn_without_reply(&key, "Message delivery failed");
                    }
                }
                if code == "switch_harness_failed" {
                    self.chat_state.pending_harness_switch = None;
                }
                self.notice = Some(format!("[{code}] {message}"));
                changed = true;
            }
            // Session list / archive / interrupt metadata events are already applied via SharedAppStore.
            ServerEvent::Pong { .. }
            | ServerEvent::SessionArchived { .. }
            | ServerEvent::SessionInterrupted { .. } => {}
        }
        // Reconcile the completed turn before starting the next queued prompt.
        if should_flush {
            self.flush_outbox_for_host(host_label);
        }
        self.sync_busy_dots_timer(cx);
        if changed {
            self.cleanup_unmounted_surface_state();
            cx.notify();
        }
    }
}
