use super::*;

impl MultibotView {
    #[allow(dead_code)]
    pub(crate) fn handle_bridge_event(&mut self, event: UiBridgeEvent, cx: &mut Context<Self>) {
        let mut is_older_page_hint = false;
        if let UiBridgeEvent::Server(ref server_ev) = event
            && !matches!(server_ev, ServerEvent::Pong { .. })
            && cx.has_global::<SharedAppStore>()
        {
            is_older_page_hint = Self::consume_shared_older_page_hint(cx, server_ev);
            let tx = &cx.global::<SharedAppStore>().daemon_event_tx;
            if tx.receiver_count() > 1 {
                let _ = tx.send(DaemonBroadcastEvent {
                    origin_view_id: self.view_id,
                    host_label: self.current_host_label().to_string(),
                    is_older_page_hint,
                    event: server_ev.clone(),
                });
            }
        }

        if let UiBridgeEvent::Server(ref server_event) = event {
            let host = self.current_host_label().to_string();
            if let Ok(mut shared) = self.shared_chat_outbox.lock() {
                shared.outbox.observe_server_event(&host, server_event);
            }
            self.handle_outbox_event(&host, server_event);
        }
        match event {
            UiBridgeEvent::Connected { host_label, req_tx } => {
                self.daemon_tx = Some(req_tx);
                self.connection_status = format!("Connected: {host_label}");
                self.sync_sessions_to_shared(cx);
            }
            UiBridgeEvent::Disconnected(reason) => {
                self.daemon_tx = None;
                self.connection_status = reason;
                self.sync_sessions_to_shared(cx);
            }
            UiBridgeEvent::Server(server_ev) => match server_ev {
                ServerEvent::BrowserCommand {
                    request_id,
                    session_id,
                    binding_id,
                    command,
                } => {
                    self.handle_browser_command(
                        self.current_host_label(),
                        &session_id,
                        &binding_id,
                        request_id,
                        command,
                    );
                    return;
                }
                ServerEvent::Pong { .. } => {
                    return;
                }
                ServerEvent::HelloAck {
                    hostname,
                    daemon_version,
                    binary_hash,
                    ..
                } => {
                    self.daemon_version = Some(daemon_version.clone());
                    self.daemon_binary_hash = Some(binary_hash.clone());
                    self.connection_status = format!("{hostname} · multibotd v{daemon_version}");

                    // Check remote daemon version/hash drift against desktop build + bundled linux binary
                    let expected_ver = multibot_proto::build_version();
                    let (connected_target, upgrade_attempted, expected_gen) = cx
                        .try_global::<SharedAppStore>()
                        .map(|s| {
                            (
                                s.connected_target.clone(),
                                s.daemon_upgrade_attempted,
                                s.connection_gen,
                            )
                        })
                        .unwrap_or((None, false, 0));
                    let bundled_path = match &connected_target {
                        Some(DaemonTarget::RemoteSsh {
                            bundled_linux_binary,
                            ..
                        }) => bundled_linux_binary.clone(),
                        _ => None,
                    };
                    let expected_hash = bundled_path
                        .as_deref()
                        .and_then(|p| multibot_core::daemon_deploy::sha256_file_prefix_12(p).ok())
                        .unwrap_or_default();

                    if multibot_core::daemon_deploy::needs_remote_upgrade(
                        &daemon_version,
                        &binary_hash,
                        expected_ver,
                        &expected_hash,
                    ) {
                        if !upgrade_attempted
                            && let Some(DaemonTarget::RemoteSsh {
                                host,
                                bundled_linux_binary: Some(local_bin),
                            }) = connected_target.clone()
                        {
                            if cx.has_global::<SharedAppStore>() {
                                cx.global_mut::<SharedAppStore>().daemon_upgrade_attempted = true;
                            }
                            self.notice =
                                Some(format!("Upgrading remote multibotd to v{expected_ver}..."));
                            let host_label = self.current_host_label().to_string();
                            let expected_target = connected_target;
                            let rt_deploy = self.runtime.clone();
                            let rt_reconnect = self.runtime.clone();
                            let host_c = host.clone();
                            let bin_c = local_bin.clone();
                            let app: &mut App = cx;
                            app.spawn(async move |cx| {
                                let res = rt_deploy
                                    .spawn(async move {
                                        multibot_core::daemon_deploy::ensure_remote_daemon_deployed(
                                            &host_c, &bin_c,
                                        )
                                        .await
                                    })
                                    .await;
                                cx.update(|cx| {
                                    if !cx.has_global::<SharedAppStore>() {
                                        return;
                                    }
                                    {
                                        let store = cx.global::<SharedAppStore>();
                                        if store.window_count == 0
                                            || store.connection_gen != expected_gen
                                            || store.connected_target != expected_target
                                        {
                                            return;
                                        }
                                    }
                                    match res {
                                        Ok(Ok(_)) => {
                                            Self::connect_to_target_on_app(
                                                rt_reconnect,
                                                host_label,
                                                DaemonTarget::RemoteSsh {
                                                    host,
                                                    bundled_linux_binary: Some(local_bin),
                                                },
                                                false,
                                                cx,
                                            );
                                        }
                                        Ok(Err(err)) => {
                                            let store = cx.global_mut::<SharedAppStore>();
                                            store.daemon_notice = Some(format!(
                                                "Remote multibotd upgrade failed: {err}"
                                            ));
                                            store.daemon_notice_seq =
                                                store.daemon_notice_seq.wrapping_add(1);
                                            store.bump_revision();
                                        }
                                        Err(err) => {
                                            let store = cx.global_mut::<SharedAppStore>();
                                            store.daemon_notice = Some(format!(
                                                "Remote multibotd upgrade failed: {err}"
                                            ));
                                            store.daemon_notice_seq =
                                                store.daemon_notice_seq.wrapping_add(1);
                                            store.bump_revision();
                                        }
                                    }
                                });
                            })
                            .detach();
                        } else {
                            self.notice = Some(format!(
                                "Remote multibotd v{daemon_version} differs from desktop v{expected_ver}."
                            ));
                        }
                    }

                    // Automatic FIFO reconnect flush of queued offline messages for this host
                    self.flush_outbox_for_current_host();

                    // Refresh slash commands for active session if one is selected
                    if let Some(ref sel) = self.selected_session {
                        self.request_daemon_commands(sel.agent, sel.cwd.clone());
                    }
                    self.sync_sessions_to_shared(cx);
                }
                ServerEvent::Sessions { sessions } => {
                    self.sessions = sessions;
                    for session in self.sessions.clone() {
                        let key = self.session_key_for(&session);
                        if self
                            .sidebar_state
                            .workspace_store
                            .auto_unarchive_waiting_if_active(&key, &session)
                            && session.archived
                        {
                            let host = self.host_for_summary(&session);
                            self.unarchive_session_on_host(&host, session.id.clone(), cx);
                        }
                    }
                    let live: Vec<(String, multibot_proto::SessionSummary)> = self
                        .sessions
                        .iter()
                        .map(|s| (self.host_for_summary(s), s.clone()))
                        .collect();
                    self.sidebar_state
                        .workspace_store
                        .reconcile_live_sessions(live.iter().map(|(host, s)| (host.as_str(), s)));
                    self.persist_workspace_store_if_enabled();
                    self.sync_workspace_to_shared(cx);
                    if let Some(ref sel) = self.selected_session.clone()
                        && let Some(fresh) = self
                            .sessions
                            .iter()
                            .find(|s| s.id == sel.id && s.agent == sel.agent)
                            .cloned()
                    {
                        self.selected_session = Some(fresh);
                    }
                    self.sync_sessions_to_shared(cx);
                    if self.selected_session.is_none()
                        && let Some(first) = self
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
                        self.open_session_with_tracking(first, false, cx);
                    }
                    self.flush_outbox_for_current_host();
                }
                ServerEvent::SessionUpdated { session } => {
                    if let Some(existing) = self
                        .sessions
                        .iter_mut()
                        .find(|s| s.id == session.id && s.agent == session.agent)
                    {
                        *existing = session.clone();
                    } else {
                        self.sessions.insert(0, session.clone());
                    }
                    let key = self.session_key_for(&session);
                    if self
                        .sidebar_state
                        .workspace_store
                        .auto_unarchive_waiting_if_active(&key, &session)
                        && session.archived
                    {
                        let host = self.active_session_host().to_string();
                        self.unarchive_session_on_host(&host, session.id.clone(), cx);
                    }
                    if self
                        .selected_session
                        .as_ref()
                        .is_some_and(|s| s.id == session.id && s.agent == session.agent)
                    {
                        self.selected_session = Some(session.clone());
                        if self.main_surface == MainSurface::Chat {
                            self.sidebar_state
                                .workspace_store
                                .mark_session_read(&key, &session);
                        }
                    }
                    self.persist_workspace_store_if_enabled();
                    self.sync_sessions_to_shared(cx);
                    self.sync_workspace_to_shared(cx);
                }
                ServerEvent::Harnesses { choices } => {
                    self.chat_state
                        .harness_choices
                        .insert(self.active_session_host().into(), choices);
                }
                ServerEvent::HarnessSwitched { previous, session } => {
                    let host = self.active_session_host().to_string();
                    self.apply_harness_switch(&host, previous, session, cx);
                }
                ServerEvent::SessionSpawned { session } => {
                    if let Some(existing) = self
                        .sessions
                        .iter_mut()
                        .find(|s| s.id == session.id && s.agent == session.agent)
                    {
                        *existing = session.clone();
                    } else {
                        self.sessions.insert(0, session.clone());
                    }
                    self.sync_sessions_to_shared(cx);
                    let title = session.title.clone();
                    let agent_label = session.agent.label();
                    if let Some(options) = self.chat_state.pending_spawn_options.take() {
                        let key = self.session_key_for(&session);
                        self.chat_state.composer_options.insert(key, options);
                    }
                    self.pending_spawn_open = false;
                    self.open_session_with_tracking(session, false, cx);
                    self.notice = Some(format!("Started {agent_label} session: {title}"));
                }
                ServerEvent::SessionArchived {
                    session_id,
                    archived,
                } => {
                    for s in &mut self.sessions {
                        if s.id == session_id {
                            s.archived = archived;
                            s.archive_kill_process = archived;
                        }
                    }
                    if let Some(ref mut sel) = self.selected_session
                        && sel.id == session_id
                    {
                        sel.archived = archived;
                        sel.archive_kill_process = archived;
                    }
                    self.sync_sessions_to_shared(cx);
                }
                ServerEvent::SessionInterrupted {
                    session_id,
                    ok,
                    detail,
                } => {
                    if ok {
                        for s in &mut self.sessions {
                            if s.id == session_id {
                                s.working = false;
                            }
                        }
                        if let Some(ref mut sel) = self.selected_session
                            && sel.id == session_id
                        {
                            sel.working = false;
                        }
                        self.sync_sessions_to_shared(cx);
                    }
                    self.notice = Some(if detail.is_empty() {
                        if ok {
                            "Interrupted session.".to_string()
                        } else {
                            format!("Could not interrupt {session_id}.")
                        }
                    } else {
                        detail
                    });
                }
                ev @ ServerEvent::History { .. } => {
                    self.apply_history_event(ev, is_older_page_hint);
                }
                ev @ ServerEvent::HistoryAppend { .. } => {
                    let (_, sessions_changed) = self.apply_history_append_event(ev);
                    if sessions_changed {
                        self.sync_sessions_to_shared(cx);
                    }
                }
                ServerEvent::SendAccepted { .. } => {
                    self.last_dispatched_prompt = None;
                }
                ServerEvent::PromptStatus {
                    session_id,
                    message_id,
                    status,
                    detail,
                } => {
                    self.track_queued_prompt(&session_id, &message_id, &status);
                    match status.as_str() {
                        "failed" => {
                            let (msg_id, text) = if !message_id.is_empty() {
                                let matched_text = self
                                    .last_dispatched_prompt
                                    .as_ref()
                                    .filter(|(id, _)| id == &message_id)
                                    .map(|(_, t)| t.clone())
                                    .or_else(|| {
                                        self.messages
                                            .iter()
                                            .rev()
                                            .find(|m| m.id == message_id)
                                            .map(|m| m.text.clone())
                                    })
                                    .unwrap_or_default();
                                (message_id, matched_text)
                            } else if let Some((id, t)) = self.last_dispatched_prompt.take() {
                                (id, t)
                            } else {
                                (format!("failed-{}", self.messages.len()), String::new())
                            };
                            let reason = if detail.is_empty() {
                                "Message delivery failed".to_string()
                            } else {
                                detail
                            };
                            self.record_send_failure(msg_id, text, reason, cx);
                        }
                        "cancelled" => {
                            self.notice = Some("Cancelled prompt.".to_string());
                        }
                        "sent" => {
                            self.last_dispatched_prompt = None;
                        }
                        "queued" => {
                            self.chat_state.advance_busy_tick();
                        }
                        "not_found" => {
                            if !detail.is_empty() {
                                self.notice = Some(detail);
                            }
                        }
                        _ => {}
                    }
                }
                ServerEvent::SearchResults {
                    request_id,
                    session_id,
                    hits,
                    truncated,
                } => {
                    if self.remote_search_request_id.as_deref() != Some(request_id.as_str()) {
                        return;
                    }
                    if self
                        .selected_session
                        .as_ref()
                        .is_some_and(|s| s.id == session_id)
                    {
                        self.remote_search_hits = hits;
                        self.remote_search_truncated = truncated;
                        let current_oldest = self
                            .active_switch_token
                            .as_ref()
                            .and_then(|t| self.chat_cache.get(&t.key))
                            .map(|c| c.oldest_offset)
                            .unwrap_or(0);
                        if self.chat_state.find_matches.is_empty()
                            && self
                                .remote_search_hits
                                .iter()
                                .any(|h| h.byte_offset < current_oldest)
                        {
                            self.load_older_history(cx);
                        }
                    }
                }
                ServerEvent::Commands {
                    agent,
                    cwd: _,
                    items,
                } => {
                    self.slash_picker_state.set_daemon_commands(agent, items);
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
                        && let Some(token) = self.active_switch_token.clone()
                    {
                        let _ = self.chat_cache.reconcile_server_event(&token, "", &ev);
                    }
                    if code == "send_failed"
                        && let Some((msg_id, text)) = self.last_dispatched_prompt.take()
                    {
                        self.record_send_failure(msg_id, text, message.clone(), cx);
                    }
                    if code == "switch_shell_client_failed" {
                        self.shell_host.switch_failed(message);
                        cx.notify();
                        return;
                    }
                    if code == "switch_harness_failed" {
                        self.chat_state.pending_harness_switch = None;
                    }
                    self.notice = Some(format!("[{code}] {message}"));
                }
            },
        }
        cx.notify();
    }

    /// Handles daemon `ServerEvent`s fanned out from the App-scoped connection consumer (or from
    /// another `Ctrl+N` window in tests) uniformly across ALL open windows without spurious redraws.
    #[allow(dead_code)]
    pub(crate) fn handle_daemon_server_event(
        &mut self,
        event: ServerEvent,
        cx: &mut Context<Self>,
    ) {
        let host = self.active_session_host().to_string();
        self.handle_daemon_server_event_with_hint(&host, event, false, cx);
    }

    pub(crate) fn handle_daemon_server_event_with_hint(
        &mut self,
        host_label: &str,
        event: ServerEvent,
        is_older_page_hint: bool,
        cx: &mut Context<Self>,
    ) {
        self.sync_from_shared_store(cx);
        self.handle_outbox_event(host_label, &event);
        if let ServerEvent::Error { code, message } = &event
            && code == "switch_shell_client_failed"
        {
            if self.shell_host.switch_failed(message) {
                cx.notify();
            }
            return;
        }
        let matches_active_host = self
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
                if matches_active_host && let Some(ref sel) = self.selected_session {
                    self.request_daemon_commands(sel.agent, sel.cwd.clone());
                }
            }
            ServerEvent::Sessions { .. } => {
                if self.selected_session.is_none()
                    && let Some(first) = self
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
                if (self.pending_spawn_open
                    && self
                        .pending_spawn_host
                        .as_deref()
                        .is_none_or(|h| h == host_label))
                    || self.selected_session.is_none()
                {
                    self.pending_spawn_open = false;
                    self.pending_spawn_host = None;
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
                if self.remote_search_request_id.as_deref() == Some(request_id.as_str())
                    && self
                        .selected_session
                        .as_ref()
                        .is_some_and(|s| s.id == session_id)
                {
                    self.remote_search_hits = hits;
                    self.remote_search_truncated = truncated;
                    let current_oldest = self
                        .active_switch_token
                        .as_ref()
                        .and_then(|t| self.chat_cache.get(&t.key))
                        .map(|c| c.oldest_offset)
                        .unwrap_or(0);
                    if self.chat_state.find_matches.is_empty()
                        && self
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
                if self.last_dispatched_prompt.is_some() {
                    self.last_dispatched_prompt = None;
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
                        .selected_session
                        .as_ref()
                        .is_some_and(|s| s.id == session_id)
                {
                    match status.as_str() {
                        "failed"
                            if self.last_dispatched_prompt.is_some()
                                || (!message_id.is_empty()
                                    && self.messages.iter().any(|m| m.id == message_id)) =>
                        {
                            let (msg_id, text) = if !message_id.is_empty() {
                                let matched_text = self
                                    .last_dispatched_prompt
                                    .as_ref()
                                    .filter(|(id, _)| id == &message_id)
                                    .map(|(_, t)| t.clone())
                                    .or_else(|| {
                                        self.messages
                                            .iter()
                                            .rev()
                                            .find(|m| m.id == message_id)
                                            .map(|m| m.text.clone())
                                    })
                                    .unwrap_or_default();
                                if self
                                    .last_dispatched_prompt
                                    .as_ref()
                                    .is_some_and(|(id, _)| id == &message_id)
                                {
                                    self.last_dispatched_prompt = None;
                                }
                                (message_id, matched_text)
                            } else {
                                self.last_dispatched_prompt.take().unwrap_or((
                                    format!("failed-{}", self.messages.len()),
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
                            self.last_dispatched_prompt = None;
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
                    && let Some(token) = self.active_switch_token.clone()
                {
                    let _ = self.chat_cache.reconcile_server_event(&token, "", &ev);
                }
                if code == "send_failed"
                    && let Some((msg_id, text)) = self.last_dispatched_prompt.take()
                {
                    self.record_send_failure(msg_id, text, message.clone(), cx);
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
        if changed {
            self.cleanup_unmounted_surface_state();
            cx.notify();
        }
    }
}
