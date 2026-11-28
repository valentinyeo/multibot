use super::*;

impl MultibotView {
    // =========================================================================
    // Live Daemon Connection, Cache, Outbox & M3/M5 RPC Helpers
    // =========================================================================

    pub(crate) fn auto_connect_configured_hosts(&mut self, cx: &mut Context<Self>) {
        let targets: Vec<_> = if self.settings.ssh_hosts.is_empty() {
            Self::find_local_daemon()
                .map(|bin| ("localhost".to_string(), DaemonTarget::LocalBinary(bin)))
                .into_iter()
                .collect()
        } else {
            self.settings
                .ssh_hosts
                .iter()
                .cloned()
                .map(|host| {
                    (
                        host.name.clone(),
                        DaemonTarget::RemoteSsh {
                            host,
                            bundled_linux_binary: Self::find_bundled_linux_daemon(),
                        },
                    )
                })
                .collect()
        };
        for (label, target) in targets {
            let skip = cx
                .global::<SharedAppStore>()
                .host_connections
                .get(&label)
                .is_some_and(|h| {
                    h.manually_disconnected
                        || h.shutdown_tx.is_some()
                        || h.daemon_tx.as_ref().is_some_and(|tx| !tx.is_closed())
                });
            if !skip {
                Self::connect_host_on_app(self.runtime.clone(), label, target, true, cx);
            }
        }
        self.sync_from_shared_store(cx);
    }

    pub(crate) fn disconnect_host_on_app(host_label: &str, cx: &mut App) {
        let store = cx.global_mut::<SharedAppStore>();
        if let Some(entry) = store.host_connections.get_mut(host_label) {
            entry.manually_disconnected = true;
            entry.connection_gen = entry.connection_gen.wrapping_add(1);
            entry.shutdown_tx = None;
            entry.daemon_tx = None;
            entry.connected = false;
            entry.status = "Disconnected".into();
            entry.sessions.clear();
            let history_prefix = format!("{}:{host_label}", host_label.len());
            store
                .pending_older_page_before
                .retain(|key, _| !key.starts_with(&history_prefix));
            store.rebuild_merged_sessions();
            store.refresh_connection_summary();
            store.bump_revision();
        }
    }

    pub(crate) fn disconnect_all_hosts(cx: &mut App) {
        let hosts: Vec<_> = cx
            .global::<SharedAppStore>()
            .host_connections
            .keys()
            .cloned()
            .collect();
        for host in hosts {
            Self::disconnect_host_on_app(&host, cx);
        }
    }

    pub(crate) fn refresh_all_hosts(&self) -> bool {
        if self.connection.host_txs.is_empty() && self.connection.host_statuses.is_empty() {
            return self.connection.daemon_tx.as_ref().is_some_and(|tx| {
                tx.send(ClientRequest::ListLifecycleSessions {
                    limit: Some(SESSION_LIST_LIMIT),
                })
                .is_ok()
            });
        }
        let mut refreshed = false;
        for tx in self.connection.host_txs.values() {
            refreshed |= tx
                .send(ClientRequest::ListLifecycleSessions {
                    limit: Some(SESSION_LIST_LIMIT),
                })
                .is_ok();
        }
        refreshed
    }

    pub(crate) fn reconnect_all_hosts(&mut self, cx: &mut Context<Self>) {
        let refreshed = self.refresh_all_hosts();
        let targets: Vec<_> = cx
            .global::<SharedAppStore>()
            .host_connections
            .iter()
            .filter(|(_, h)| h.daemon_tx.as_ref().is_none_or(|tx| tx.is_closed()))
            .map(|(label, h)| (label.clone(), h.target.clone()))
            .collect();
        for (label, target) in targets {
            Self::connect_host_on_app(self.runtime.clone(), label, target, true, cx);
        }
        if !self.settings.ssh_hosts.is_empty() || self.connection.host_statuses.is_empty() {
            self.auto_connect_configured_hosts(cx);
        } else {
            self.sync_from_shared_store(cx);
        }
        if let Some(s) = &self.selection.selected_session
            && let Some(tx) = self.active_session_tx()
        {
            let _ = tx.send(ClientRequest::WatchSession {
                session_id: s.id.clone(),
                source_path: s.source_path.clone(),
                agent: s.agent,
            });
        }
        if refreshed {
            self.notice = Some("Refreshed daemon sessions.".into());
        }
        cx.notify();
    }

    pub(crate) fn connect_to_target(
        &mut self,
        label: String,
        target: DaemonTarget,
        cx: &mut Context<Self>,
    ) {
        self.connect_to_target_internal(label, target, true, cx);
    }

    pub(crate) fn connect_to_target_internal(
        &mut self,
        label: String,
        target: DaemonTarget,
        reset_upgrade_guard: bool,
        cx: &mut Context<Self>,
    ) {
        let rt = self.runtime.clone();
        Self::connect_to_target_on_app(rt, label, target, reset_upgrade_guard, cx);
        self.sync_from_shared_store(cx);
    }

    pub(crate) fn connect_to_target_on_app(
        runtime: Arc<tokio::runtime::Runtime>,
        label: String,
        target: DaemonTarget,
        reset_upgrade_guard: bool,
        cx: &mut App,
    ) -> u64 {
        Self::connect_host_on_app(runtime, label, target, reset_upgrade_guard, cx)
    }

    pub(crate) fn connect_host_on_app(
        runtime: Arc<tokio::runtime::Runtime>,
        label: String,
        target: DaemonTarget,
        reset_upgrade_guard: bool,
        cx: &mut App,
    ) -> u64 {
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
        let store = cx.global_mut::<SharedAppStore>();
        let entry = store
            .host_connections
            .entry(label.clone())
            .or_insert_with(|| HostConnectionState {
                host_label: label.clone(),
                target: target.clone(),
                status: String::new(),
                connected: false,
                manually_disconnected: false,
                reconnect_attempt: 0,
                daemon_tx: None,
                shutdown_tx: None,
                connection_gen: 0,
                daemon_version: None,
                daemon_binary_hash: None,
                daemon_upgrade_attempted: false,
                sessions: Vec::new(),
            });
        entry.connection_gen = entry.connection_gen.wrapping_add(1);
        let conn_gen = entry.connection_gen;
        entry.shutdown_tx = Some(shutdown_tx);
        entry.target = target.clone();
        entry.status = format!("Connecting to {label}...");
        entry.connected = false;
        entry.manually_disconnected = false;
        entry.daemon_tx = None;
        entry.daemon_version = None;
        entry.daemon_binary_hash = None;
        entry.sessions.clear();
        if reset_upgrade_guard {
            entry.daemon_upgrade_attempted = false;
            entry.reconnect_attempt = 0;
        }
        // Legacy fields remain available to single-host callers.
        store.connection_gen = store.connection_gen.wrapping_add(1);
        store.connection_shutdown_tx = None;
        let history_prefix = format!("{}:{label}", label.len());
        store
            .pending_older_page_before
            .retain(|key, _| !key.starts_with(&history_prefix));
        store.rebuild_merged_sessions();
        store.refresh_connection_summary();
        store.bump_revision();

        let (bridge_tx, mut bridge_rx) = mpsc::unbounded_channel::<UiBridgeEvent>();
        let rt_for_conn = runtime.clone();

        let event_host = label.clone();
        rt_for_conn.spawn(async move {
            let result = tokio::select! {
                _ = &mut shutdown_rx => return,
                result = DaemonConnection::connect(target) => result,
            };
            match result {
                Ok(mut conn) => {
                    let (req_tx, mut req_rx) = mpsc::unbounded_channel::<ClientRequest>();
                    if bridge_tx
                        .send(UiBridgeEvent::Connected {
                            host_label: label.clone(),
                            req_tx,
                        })
                        .is_err()
                    {
                        return;
                    }
                    let _ = conn.send(ClientRequest::WatchDirectory);
                    let _ = conn.send(ClientRequest::ListLifecycleSessions {
                        limit: Some(SESSION_LIST_LIMIT),
                    });

                    loop {
                        tokio::select! {
                            _ = &mut shutdown_rx => {
                                break;
                            }
                            maybe_req = req_rx.recv() => {
                                match maybe_req {
                                    Some(req) => {
                                        if conn.send(req).is_err() {
                                            break;
                                        }
                                    }
                                    None => break,
                                }
                            }
                            maybe_ev = conn.next_event() => {
                                match maybe_ev {
                                    Some(ev) => {
                                        if bridge_tx.send(UiBridgeEvent::Server(ev)).is_err() {
                                            break;
                                        }
                                    }
                                    None => break,
                                }
                            }
                        }
                    }
                    let _ = bridge_tx.send(UiBridgeEvent::Disconnected(format!(
                        "Disconnected from {label}"
                    )));
                }
                Err(err) => {
                    let _ = bridge_tx.send(UiBridgeEvent::Disconnected(format!(
                        "Connection failed: {err}"
                    )));
                }
            }
        });

        let rt_for_events = runtime;
        cx.spawn(async move |cx| {
            while let Some(event) = bridge_rx.recv().await {
                let keep_going = cx.update(|cx| {
                    Self::apply_host_bridge_event_to_app(
                        &event_host,
                        conn_gen,
                        event,
                        &rt_for_events,
                        cx,
                    )
                });
                if !keep_going {
                    break;
                }
            }
        })
        .detach();

        conn_gen
    }

    pub(crate) fn find_bundled_linux_daemon() -> Option<PathBuf> {
        multibot_core::daemon_client::discover_bundled_linux_daemon()
    }

    pub(crate) fn find_local_daemon() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let dir = exe.parent()?;
        #[cfg(target_os = "windows")]
        let name = "multibotd.exe";
        #[cfg(not(target_os = "windows"))]
        let name = "multibotd";
        let candidate = dir.join(name);
        candidate.is_file().then_some(candidate)
    }
}
