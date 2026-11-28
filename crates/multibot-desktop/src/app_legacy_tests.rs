use super::*;

impl MultibotView {
    pub(crate) fn apply_daemon_bridge_event_to_app(
        conn_gen: u64,
        event: UiBridgeEvent,
        runtime: &Arc<tokio::runtime::Runtime>,
        cx: &mut App,
    ) -> bool {
        if !cx.has_global::<SharedAppStore>() {
            return false;
        }
        let store = cx.global::<SharedAppStore>();
        let host = store
            .connected_host
            .clone()
            .unwrap_or_else(|| "local".into());
        #[cfg(test)]
        if store
            .host_connections
            .get(&host)
            .is_none_or(|h| h.connection_gen != store.connection_gen)
        {
            return Self::apply_legacy_bridge_event_to_app(conn_gen, event, runtime, cx);
        }
        Self::apply_host_bridge_event_to_app(&host, conn_gen, event, runtime, cx)
    }

    #[cfg(test)]
    /// Applies a daemon [`UiBridgeEvent`] at App scope onto [`SharedAppStore`], gated on
    /// `store.connection_gen == conn_gen` and `store.window_count > 0`, and broadcasts
    /// [`ServerEvent`]s uniformly to all open windows via `SharedAppStore.daemon_event_tx`.
    /// Returns `false` if this connection generation is stale or all windows have closed.
    pub(super) fn apply_legacy_bridge_event_to_app(
        conn_gen: u64,
        event: UiBridgeEvent,
        runtime: &Arc<tokio::runtime::Runtime>,
        cx: &mut App,
    ) -> bool {
        if !cx.has_global::<SharedAppStore>() {
            return false;
        }
        {
            let store = cx.global::<SharedAppStore>();
            if store.window_count == 0 || store.connection_gen != conn_gen {
                return false;
            }
        }

        match event {
            UiBridgeEvent::Connected { host_label, req_tx } => {
                let store = cx.global_mut::<SharedAppStore>();
                store.daemon_tx = Some(req_tx);
                store.connection_status = format!("Connected: {host_label}");
                store.bump_revision();
            }
            UiBridgeEvent::Disconnected(reason) => {
                let store = cx.global_mut::<SharedAppStore>();
                store.daemon_tx = None;
                store.connection_status = reason;
                store.bump_revision();
            }
            UiBridgeEvent::Server(server_ev) => {
                if matches!(server_ev, ServerEvent::Pong { .. }) {
                    return true;
                }
                {
                    let store = cx.global::<SharedAppStore>();
                    if let Ok(mut shared) = store.shared_chat_outbox.lock() {
                        shared.outbox.observe_server_event(
                            store.connected_host.as_deref().unwrap_or("local"),
                            &server_ev,
                        );
                    }
                }
                match &server_ev {
                    ServerEvent::HelloAck {
                        hostname,
                        daemon_version,
                        binary_hash,
                        ..
                    } => {
                        let expected_ver = multibot_proto::build_version();
                        let upgrade_job = {
                            let store = cx.global_mut::<SharedAppStore>();
                            store.daemon_version = Some(daemon_version.clone());
                            store.daemon_binary_hash = Some(binary_hash.clone());
                            store.connection_status =
                                format!("{hostname} · multibotd v{daemon_version}");

                            let bundled_path = match &store.connected_target {
                                Some(DaemonTarget::RemoteSsh {
                                    bundled_linux_binary,
                                    ..
                                }) => bundled_linux_binary.clone(),
                                _ => None,
                            };
                            let expected_hash = bundled_path
                                .as_deref()
                                .and_then(|p| {
                                    multibot_core::daemon_deploy::sha256_file_prefix_12(p).ok()
                                })
                                .unwrap_or_default();

                            let mut job = None;
                            if multibot_core::daemon_deploy::needs_remote_upgrade(
                                daemon_version,
                                binary_hash,
                                expected_ver,
                                &expected_hash,
                            ) {
                                if !store.daemon_upgrade_attempted
                                    && let Some(DaemonTarget::RemoteSsh {
                                        host,
                                        bundled_linux_binary: Some(local_bin),
                                    }) = store.connected_target.clone()
                                {
                                    store.daemon_upgrade_attempted = true;
                                    store.daemon_notice = Some(format!(
                                        "Upgrading remote multibotd to v{expected_ver}..."
                                    ));
                                    store.daemon_notice_seq =
                                        store.daemon_notice_seq.wrapping_add(1);
                                    let host_label = store
                                        .connected_host
                                        .clone()
                                        .unwrap_or_else(|| host.name.clone());
                                    let expected_target = store.connected_target.clone();
                                    job = Some((host, local_bin, host_label, expected_target));
                                } else {
                                    store.daemon_notice = Some(format!(
                                        "Remote multibotd v{daemon_version} differs from desktop v{expected_ver}."
                                    ));
                                    store.daemon_notice_seq =
                                        store.daemon_notice_seq.wrapping_add(1);
                                }
                            }
                            store.bump_revision();
                            job
                        };

                        if let Some((host, local_bin, host_label, expected_target)) = upgrade_job {
                            let rt_deploy = runtime.clone();
                            let rt_reconnect = runtime.clone();
                            let host_c = host.clone();
                            let bin_c = local_bin.clone();
                            cx.spawn(async move |cx| {
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
                                            || store.connection_gen != conn_gen
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
                        }
                    }
                    ServerEvent::Sessions { sessions } => {
                        let store = cx.global_mut::<SharedAppStore>();
                        store.sessions = sessions.clone();
                        let host = store.connected_host.as_deref().unwrap_or("local");
                        for session in &mut store.sessions {
                            let key = canonical_session_key(host, session.agent, &session.id);
                            if store
                                .workspace_store
                                .auto_unarchive_waiting_if_active(&key, session)
                                && session.archived
                            {
                                if let Some(tx) = &store.daemon_tx {
                                    if tx
                                        .send(ClientRequest::UnarchiveSession {
                                            session_id: session.id.clone(),
                                        })
                                        .is_ok()
                                    {
                                        session.archived = false;
                                    }
                                }
                            }
                        }
                        store.workspace_store.reconcile_live_sessions(
                            sessions.iter().map(|summary| (host, summary)),
                        );
                        if store.persist_to_disk {
                            let _ = store.workspace_store.save();
                        }
                        store.bump_revision();
                    }
                    ServerEvent::SessionUpdated { session } => {
                        let store = cx.global_mut::<SharedAppStore>();
                        if let Some(existing) = store
                            .sessions
                            .iter_mut()
                            .find(|s| s.id == session.id && s.agent == session.agent)
                        {
                            *existing = session.clone();
                        } else {
                            store.sessions.insert(0, session.clone());
                        }
                        let host_label = store.connected_host.as_deref().unwrap_or("local");
                        let key = canonical_session_key(host_label, session.agent, &session.id);
                        if store
                            .workspace_store
                            .auto_unarchive_waiting_if_active(&key, session)
                            && session.archived
                        {
                            if let Some(tx) = store
                                .host_connections
                                .get(host_label)
                                .and_then(|h| h.daemon_tx.as_ref())
                                .or(store.daemon_tx.as_ref())
                            {
                                let _ = tx.send(ClientRequest::UnarchiveSession {
                                    session_id: session.id.clone(),
                                });
                            }
                        }
                        store
                            .workspace_store
                            .reconcile_session(host_label, &session);
                        if store.persist_to_disk {
                            let _ = store.workspace_store.save();
                        }
                        store.bump_revision();
                    }
                    ServerEvent::HarnessSwitched { previous, session } => {
                        let host = cx
                            .global::<SharedAppStore>()
                            .connected_host
                            .clone()
                            .unwrap_or_else(|| "local".into());
                        cx.global_mut::<SharedAppStore>()
                            .apply_harness_switch(&host, previous, session);
                    }
                    ServerEvent::SessionSpawned { session } => {
                        let store = cx.global_mut::<SharedAppStore>();
                        if let Some(existing) = store
                            .sessions
                            .iter_mut()
                            .find(|s| s.id == session.id && s.agent == session.agent)
                        {
                            *existing = session.clone();
                        } else {
                            store.sessions.insert(0, session.clone());
                        }
                        let host_label = store.connected_host.as_deref().unwrap_or("local");
                        let moved = store
                            .workspace_store
                            .reconcile_session(host_label, &session);
                        if moved > 0 && store.persist_to_disk {
                            let _ = store.workspace_store.save();
                        }
                        store.daemon_notice = Some(format!(
                            "Started {} session: {}",
                            session.agent.label(),
                            session.title
                        ));
                        store.daemon_notice_seq = store.daemon_notice_seq.wrapping_add(1);
                        store.bump_revision();
                    }
                    ServerEvent::SessionArchived {
                        session_id,
                        archived,
                    } => {
                        let store = cx.global_mut::<SharedAppStore>();
                        for s in &mut store.sessions {
                            if s.id == *session_id {
                                s.archived = *archived;
                                s.archive_kill_process = *archived;
                            }
                        }
                        store.bump_revision();
                    }
                    ServerEvent::SessionInterrupted {
                        session_id,
                        ok,
                        detail,
                    } => {
                        let store = cx.global_mut::<SharedAppStore>();
                        if *ok {
                            for s in &mut store.sessions {
                                if s.id == *session_id {
                                    s.working = false;
                                }
                            }
                        }
                        store.daemon_notice = Some(if detail.is_empty() {
                            if *ok {
                                "Interrupted session.".to_string()
                            } else {
                                format!("Could not interrupt {session_id}.")
                            }
                        } else {
                            detail.clone()
                        });
                        store.daemon_notice_seq = store.daemon_notice_seq.wrapping_add(1);
                        store.bump_revision();
                    }
                    ServerEvent::HistoryAppend {
                        session_id,
                        messages,
                        working,
                        ..
                    } => {
                        let mut sessions_changed = false;
                        let store = cx.global_mut::<SharedAppStore>();
                        if let Some(is_working) = working {
                            for s in &mut store.sessions {
                                if s.id == *session_id && s.working != *is_working {
                                    s.working = *is_working;
                                    sessions_changed = true;
                                }
                            }
                        }
                        if let Some(last_asst) = messages
                            .iter()
                            .rev()
                            .find(|m| m.role == MessageRole::Assistant)
                        {
                            let preview: String = last_asst.text.trim().chars().take(120).collect();
                            for s in &mut store.sessions {
                                if s.id == *session_id {
                                    s.last_assistant_id = Some(last_asst.id.clone());
                                    if !preview.is_empty() {
                                        s.last_assistant_preview = Some(preview.clone());
                                    }
                                    if let Some(ts) = last_asst.timestamp_ms {
                                        s.last_assistant_ms = Some(ts);
                                        s.updated_at_ms = s.updated_at_ms.max(ts);
                                    }
                                    sessions_changed = true;
                                }
                            }
                        }
                        if sessions_changed {
                            store.bump_revision();
                        }
                    }
                    _ => {}
                }

                // Match & consume App-scoped `pending_older_page_before` for `ServerEvent::History`
                // (pruning stale entries where the file truncated past `req_before`) so ALL sibling
                // windows receive an exact `is_older_page_hint` without misclassifying partial
                // trailing `.jsonl` lines.
                let is_older_page_hint = Self::consume_shared_older_page_hint(cx, &server_ev);

                // Broadcast uniformly to ALL open windows (origin_id = 0 != any view_id >= 1).
                let tx = &cx.global::<SharedAppStore>().daemon_event_tx;
                if tx.receiver_count() > 0 {
                    let _ = tx.send(DaemonBroadcastEvent {
                        origin_view_id: 0,
                        host_label: cx
                            .global::<SharedAppStore>()
                            .connected_host
                            .clone()
                            .unwrap_or_else(|| "local".into()),
                        is_older_page_hint,
                        event: server_ev,
                    });
                }
            }
        }
        let store = cx.global_mut::<SharedAppStore>();
        if let Some(host) = &store.connected_host
            && let Some(entry) = store.host_connections.get_mut(host)
        {
            entry.daemon_tx = store.daemon_tx.clone();
            entry.connected = entry.daemon_tx.is_some();
            entry.sessions = store.sessions.clone();
            entry.status = store.connection_status.clone();
            store.rebuild_merged_sessions();
        }
        true
    }
}
