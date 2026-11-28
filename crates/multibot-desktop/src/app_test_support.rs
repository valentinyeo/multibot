use super::*;

impl MultibotView {
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
                    origin_view_id: self.sync.view_id,
                    host_label: self.current_host_label().to_string(),
                    is_older_page_hint,
                    event: server_ev.clone(),
                });
            }
        }

        let mut should_flush = false;
        let event_host = self.current_host_label().to_string();
        if let UiBridgeEvent::Server(ref server_event) = event {
            let host = self.current_host_label().to_string();
            if let Ok(mut shared) = self.outbox.shared_chat_outbox.lock() {
                shared.outbox.observe_server_event(&host, server_event);
            }
            if !is_older_page_hint {
                self.observe_turn_event(&host, server_event);
            }
            should_flush = self.handle_outbox_event(&host, server_event);
        }
        match event {
            UiBridgeEvent::Connected { host_label, req_tx } => {
                self.connection.daemon_tx = Some(req_tx);
                self.connection.connection_status = format!("Connected: {host_label}");
                self.sync_sessions_to_shared(cx);
            }
            UiBridgeEvent::Disconnected(reason) => {
                self.connection.daemon_tx = None;
                self.connection.connection_status = reason;
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
                    self.connection.daemon_version = Some(daemon_version.clone());
                    self.connection.daemon_binary_hash = Some(binary_hash.clone());
                    self.connection.connection_status =
                        format!("{hostname} · multibotd v{daemon_version}");

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
                    if let Some(ref sel) = self.selection.selected_session {
                        self.request_daemon_commands(sel.agent, sel.cwd.clone());
                    }
                    self.sync_sessions_to_shared(cx);
                }
                ServerEvent::Sessions { sessions } => {
                    self.connection.sessions = sessions;
                    for session in self.connection.sessions.clone() {
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
                        .connection
                        .sessions
                        .iter()
                        .map(|s| (self.host_for_summary(s), s.clone()))
                        .collect();
                    self.sidebar_state
                        .workspace_store
                        .reconcile_live_sessions(live.iter().map(|(host, s)| (host.as_str(), s)));
                    self.persist_workspace_store_if_enabled();
                    self.sync_workspace_to_shared(cx);
                    if let Some(ref sel) = self.selection.selected_session.clone()
                        && let Some(fresh) = self
                            .connection
                            .sessions
                            .iter()
                            .find(|s| s.id == sel.id && s.agent == sel.agent)
                            .cloned()
                    {
                        self.selection.selected_session = Some(fresh);
                    }
                    self.sync_sessions_to_shared(cx);
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
                        self.open_session_with_tracking(first, false, cx);
                    }
                    self.flush_outbox_for_current_host();
                }
                ServerEvent::SessionUpdated { session } => {
                    if let Some(existing) = self
                        .connection
                        .sessions
                        .iter_mut()
                        .find(|s| s.id == session.id && s.agent == session.agent)
                    {
                        *existing = session.clone();
                    } else {
                        self.connection.sessions.insert(0, session.clone());
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
                        .selection
                        .selected_session
                        .as_ref()
                        .is_some_and(|s| s.id == session.id && s.agent == session.agent)
                    {
                        self.selection.selected_session = Some(session.clone());
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
                        .connection
                        .sessions
                        .iter_mut()
                        .find(|s| s.id == session.id && s.agent == session.agent)
                    {
                        *existing = session.clone();
                    } else {
                        self.connection.sessions.insert(0, session.clone());
                    }
                    self.sync_sessions_to_shared(cx);
                    let title = session.title.clone();
                    let agent_label = session.agent.label();
                    if let Some(options) = self.chat_state.pending_spawn_options.take() {
                        let key = self.session_key_for(&session);
                        self.chat_state.composer_options.insert(key, options);
                    }
                    self.selection.pending_spawn_open = false;
                    self.open_session_with_tracking(session, false, cx);
                    self.notice = Some(format!("Started {agent_label} session: {title}"));
                }
                ServerEvent::SessionArchived {
                    session_id,
                    archived,
                } => {
                    for s in &mut self.connection.sessions {
                        if s.id == session_id {
                            s.archived = archived;
                            s.archive_kill_process = archived;
                        }
                    }
                    if let Some(ref mut sel) = self.selection.selected_session
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
                        for s in &mut self.connection.sessions {
                            if s.id == session_id {
                                s.working = false;
                            }
                        }
                        if let Some(ref mut sel) = self.selection.selected_session
                            && sel.id == session_id
                        {
                            sel.working = false;
                        }
                        self.sync_sessions_to_shared(cx);
                    }
                    if !ok
                        && !matches!(
                            detail.as_str(),
                            "no active hosted process for session" | "process already exited"
                        )
                    {
                        self.notice = Some(if detail.is_empty() {
                            format!("Could not interrupt {session_id}.")
                        } else {
                            detail
                        });
                    }
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
                    self.selection.last_dispatched_prompt = None;
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
                                (message_id, matched_text)
                            } else if let Some((id, t)) =
                                self.selection.last_dispatched_prompt.take()
                            {
                                (id, t)
                            } else {
                                (
                                    format!("failed-{}", self.history.messages.len()),
                                    String::new(),
                                )
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
                            self.selection.last_dispatched_prompt = None;
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
                    if self.history.remote_search_request_id.as_deref() != Some(request_id.as_str())
                    {
                        return;
                    }
                    if self
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
                        && let Some(token) = self.history.active_switch_token.clone()
                    {
                        let _ = self
                            .history
                            .chat_cache
                            .reconcile_server_event(&token, "", &ev);
                    }
                    if code == "send_failed"
                        && let Some((msg_id, text)) = self.selection.last_dispatched_prompt.take()
                    {
                        self.record_send_failure(msg_id, text, message.clone(), cx);
                    }
                    if code == "switch_harness_failed" {
                        self.chat_state.pending_harness_switch = None;
                    }
                    self.notice = Some(format!("[{code}] {message}"));
                }
            },
        }
        if should_flush {
            self.flush_outbox_for_host(&event_host);
        }
        self.sync_busy_dots_timer(cx);
        cx.notify();
    }

    pub(crate) fn handle_daemon_server_event(
        &mut self,
        event: ServerEvent,
        cx: &mut Context<Self>,
    ) {
        let host = self.active_session_host().to_string();
        self.handle_daemon_server_event_with_hint(&host, event, false, cx);
    }

    pub(crate) fn handle_update_bridge_event(
        &mut self,
        event: UpdateBridgeEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            UpdateBridgeEvent::CheckCompleted { outcome, manual } => {
                self.updater.update_in_flight = false;
                self.update_ui.pending_action = None;
                match outcome {
                    CheckOutcome::NotModified => {
                        if !manual {
                            // Hard Invariant #4: 304 Not Modified triggers ZERO UI redraws!
                            return;
                        }
                        if let Some(rel) = self.update_ui.last_release.clone() {
                            self.update_ui.set_update_state(UpdateState::Available(rel));
                        } else {
                            self.update_ui.set_update_state(UpdateState::UpToDate);
                            let cur =
                                UpdateUiState::normalize_version(multibot_proto::build_version());
                            self.notice = Some(format!("Multibot v{cur} is up to date."));
                        }
                        self.sync_update_state_to_shared(cx);
                        cx.notify();
                    }
                    CheckOutcome::UpToDate { etag } => {
                        if etag.is_some() && etag != self.settings.last_update_etag {
                            self.settings.last_update_etag = etag;
                            self.persist_update_settings(cx);
                        }
                        let was_visible = self.update_ui.is_card_visible(
                            self.settings.sidebar_mode,
                            self.sidebar_state.is_shown(),
                            self.settings.dismissed_update_version.as_deref(),
                        );
                        let changed = self.update_ui.set_update_state(UpdateState::UpToDate);
                        if manual {
                            let cur =
                                UpdateUiState::normalize_version(multibot_proto::build_version());
                            self.notice = Some(format!("Multibot v{cur} is up to date."));
                            self.sync_update_state_to_shared(cx);
                            cx.notify();
                        } else if changed && was_visible {
                            self.sync_update_state_to_shared(cx);
                            cx.notify();
                        }
                    }
                    CheckOutcome::RateLimited {
                        retry_after: _,
                        etag,
                    } => {
                        if etag.is_some() && etag != self.settings.last_update_etag {
                            self.settings.last_update_etag = etag;
                            self.persist_update_settings(cx);
                        }
                        if !manual {
                            // Background 403/429 backoff stays silent (zero UI redraw).
                            return;
                        }
                        if let Some(rel) = self.update_ui.last_release.clone() {
                            self.update_ui.set_update_state(UpdateState::Available(rel));
                        } else {
                            self.update_ui.set_update_state(UpdateState::Idle);
                        }
                        self.notice = Some(
                            "Update check temporarily rate-limited. Please try again later."
                                .to_string(),
                        );
                        self.sync_update_state_to_shared(cx);
                        cx.notify();
                    }
                    CheckOutcome::UpdateAvailable { release, etag } => {
                        if etag.is_some() && etag != self.settings.last_update_etag {
                            self.settings.last_update_etag = etag;
                            self.persist_update_settings(cx);
                        }
                        if manual
                            && UpdateUiState::is_version_dismissed(
                                &release.version,
                                self.settings.dismissed_update_version.as_deref(),
                            )
                        {
                            self.settings.dismissed_update_version = None;
                            self.persist_update_settings(cx);
                        }
                        let was_visible = self.update_ui.is_card_visible(
                            self.settings.sidebar_mode,
                            self.sidebar_state.is_shown(),
                            self.settings.dismissed_update_version.as_deref(),
                        );
                        let changed = self
                            .update_ui
                            .set_update_state(UpdateState::Available(release.clone()));
                        let now_visible = self.update_ui.is_card_visible(
                            self.settings.sidebar_mode,
                            self.sidebar_state.is_shown(),
                            self.settings.dismissed_update_version.as_deref(),
                        );
                        if manual {
                            let ver = UpdateUiState::normalize_version(&release.version);
                            self.notice = Some(format!("Multibot v{ver} is ready."));
                            self.sync_update_state_to_shared(cx);
                            cx.notify();
                        } else if changed && (was_visible || now_visible) {
                            self.sync_update_state_to_shared(cx);
                            cx.notify();
                        }
                    }
                }
            }
            UpdateBridgeEvent::CheckFailed { message, manual } => {
                self.updater.update_in_flight = false;
                self.update_ui.pending_action = None;
                if !manual {
                    // Offline or transient background failure stays silent (zero redraw).
                    return;
                }
                self.update_ui.set_update_state(UpdateState::Error {
                    message,
                    retryable: true,
                });
                self.sync_update_state_to_shared(cx);
                cx.notify();
            }
            UpdateBridgeEvent::DownloadProgress {
                version,
                bytes_done,
                bytes_total,
            } => {
                self.set_update_state(
                    UpdateState::Downloading {
                        version,
                        bytes_done,
                        bytes_total,
                    },
                    cx,
                );
                self.sync_update_state_to_shared(cx);
            }
            UpdateBridgeEvent::Verifying { version } => {
                self.set_update_state(UpdateState::Verifying { version }, cx);
                self.sync_update_state_to_shared(cx);
            }
            UpdateBridgeEvent::StagedReady(staged) => {
                let ver = UpdateUiState::normalize_version(&staged.version);
                self.update_ui.set_update_state(UpdateState::Applying {
                    version: ver.clone(),
                });
                self.notice = Some(format!("Restarting to apply v{ver}..."));
                self.sync_update_state_to_shared(cx);
                cx.notify();
            }
            UpdateBridgeEvent::ApplySucceeded | UpdateBridgeEvent::RestartSucceeded => {
                self.updater.update_in_flight = false;
                self.update_ui.pending_action = None;
                cx.quit();
            }
            UpdateBridgeEvent::Failed { message, retryable } => {
                self.updater.update_in_flight = false;
                self.update_ui.pending_action = None;
                self.set_update_state(UpdateState::Error { message, retryable }, cx);
                self.sync_update_state_to_shared(cx);
            }
        }
    }

    pub(crate) fn steer_active_session(&mut self, text: String, cx: &mut Context<Self>) -> bool {
        let Some(ref sess) = self.selection.selected_session else {
            return false;
        };
        let Some(tx) = self.active_session_tx().cloned() else {
            return false;
        };
        let ok = tx
            .send(ClientRequest::SteerSession {
                session_id: sess.id.clone(),
                agent: sess.agent,
                cwd: sess.cwd.clone(),
                text,
                options: self.composer_options_for(sess),
            })
            .is_ok();
        cx.notify();
        ok
    }
}
