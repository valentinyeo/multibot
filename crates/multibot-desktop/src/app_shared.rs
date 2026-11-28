use super::*;

impl MultibotView {
    /// Enables disk persistence for Tier-2 chat cache and offline outbox (production `main.rs`).
    /// Loads the shared App-scoped `ChatOutbox` from disk ONCE so `Ctrl+N` sibling windows share
    /// the same queue without reloading or double-flushing entries on `HelloAck`.
    pub(crate) fn enable_disk_persistence(&mut self) {
        self.history.chat_cache.dir = Some(MultibotSettings::chat_cache_dir());
        self.outbox.persist_outbox_to_disk = true;
        if let Ok(mut shared) = self.outbox.shared_chat_outbox.lock() {
            if !shared.loaded_from_disk {
                let disk_outbox = ChatOutbox::load();
                for entry in disk_outbox.entries {
                    let _ = shared.outbox.enqueue_entry(entry);
                }
                shared.loaded_from_disk = true;
            }
            shared.persist_to_disk = true;
            self.outbox.chat_outbox = shared.outbox.clone();
        }
    }

    /// True only when disk persistence is explicitly enabled (production builds), ensuring unit
    /// tests never touch real `~/.config/Multibot/settings.json`.
    pub(crate) fn is_disk_persistence_enabled(&self, cx: &App) -> bool {
        self.sidebar_state.persist_to_disk
            || cx
                .try_global::<SharedAppStore>()
                .is_some_and(|s| s.persist_to_disk)
    }

    // =========================================================================
    // Cross-Window SharedAppStore Synchronization
    // =========================================================================

    pub(crate) fn sync_settings_to_shared(&mut self, cx: &mut Context<Self>) {
        if cx.has_global::<SharedAppStore>() {
            let rev = {
                let store = cx.global_mut::<SharedAppStore>();
                store.settings = self.settings.clone();
                store.bump_revision()
            };
            self.sync.last_synced_revision = rev;
        }
    }

    pub(crate) fn sync_sessions_to_shared(&mut self, cx: &mut Context<Self>) {
        if cx.has_global::<SharedAppStore>() {
            let rev = {
                let store = cx.global_mut::<SharedAppStore>();
                if !store.host_connections.is_empty() && !self.connection.host_statuses.is_empty() {
                    for (host, entry) in &mut store.host_connections {
                        entry.sessions = self
                            .connection
                            .host_sessions
                            .iter()
                            .filter(|(h, _)| h == host)
                            .map(|(_, s)| s.clone())
                            .collect();
                    }
                    store.rebuild_merged_sessions();
                    store.refresh_connection_summary();
                    self.connection.sessions = store.sessions.clone();
                    self.connection.host_statuses = store.host_status_snapshots();
                    self.connection.connection_status = store.connection_status.clone();
                    store.bump_revision()
                } else {
                    store.sessions = self.connection.sessions.clone();
                    store.connected_host = self.connection.connected_host.clone();
                    store.connection_status = self.connection.connection_status.clone();
                    store.daemon_tx = self.connection.daemon_tx.clone();
                    store.daemon_version = self.connection.daemon_version.clone();
                    store.daemon_binary_hash = self.connection.daemon_binary_hash.clone();
                    store.bump_revision()
                }
            };
            self.sync.last_synced_revision = rev;
        }
    }

    pub(crate) fn sync_workspace_to_shared(&mut self, cx: &mut Context<Self>) {
        if cx.has_global::<SharedAppStore>() {
            let rev = {
                let store = cx.global_mut::<SharedAppStore>();
                store.workspace_store = self.sidebar_state.workspace_store.clone();
                store.palette_stats = self.palette_state.stats.clone();
                store.unread_only = self.sidebar_state.unread_only;
                store.bump_revision()
            };
            self.sync.last_synced_revision = rev;
            SharedAppStore::schedule_reminder_timer(cx);
        }
    }

    pub(crate) fn sync_update_state_to_shared(&mut self, cx: &mut Context<Self>) {
        if cx.has_global::<SharedAppStore>() {
            let rev = {
                let store = cx.global_mut::<SharedAppStore>();
                store.update_state = self.update_ui.state.clone();
                store.update_last_release = self.update_ui.last_release.clone();
                store.update_in_flight = self.updater.update_in_flight;
                store.bump_revision()
            };
            self.sync.last_synced_revision = rev;
        }
    }

    pub(crate) fn sync_from_shared_store(&mut self, cx: &mut Context<Self>) {
        if !cx.has_global::<SharedAppStore>() {
            return;
        }
        let mut changed = false;

        let (
            rev,
            settings_diff,
            next_settings,
            update_in_flight,
            update_action_tx,
            update_last_release,
            update_state,
        ) = {
            let store = cx.global::<SharedAppStore>();
            (
                store.revision,
                store.settings != self.settings,
                store.settings.clone(),
                store.update_in_flight,
                store.update_action_tx.clone(),
                store.update_last_release.clone(),
                store.update_state.clone(),
            )
        };

        self.updater.update_in_flight = update_in_flight;
        if !update_in_flight {
            self.update_ui.pending_action = None;
        }
        if update_action_tx.is_some() {
            self.updater.update_action_tx = update_action_tx;
        }

        if settings_diff {
            let mut next_settings = next_settings;
            if self.selection.selected_session.is_some() {
                next_settings.shell_fullscreen = self.settings.shell_fullscreen;
            }
            let visible_changed = !settings_visible_eq(&self.settings, &next_settings);
            self.settings = next_settings;
            if visible_changed {
                self.sidebar_state
                    .set_mode(if self.settings.shell_fullscreen {
                        SidebarMode::Hidden
                    } else {
                        self.settings.sidebar_mode
                    });
                self.cleanup_unmounted_surface_state();
                self.chat_state.font_size_pt = self
                    .settings
                    .chat_font_size_pt
                    .clamp(MIN_CHAT_FONT_SIZE_PT, MAX_CHAT_FONT_SIZE_PT);
                changed = true;
            }
        }

        // Always sync update_ui state/release from App-scoped SharedAppStore even when
        // store.revision did not bump (e.g. background UpdateAvailable while sidebar is
        // Collapsed/Hidden), while gating cx.notify() on actual visible UI impact so
        // Collapsed/Hidden sidebars incur ZERO idle redraws (Hard Invariant #4).
        if update_last_release.is_some() && self.update_ui.last_release != update_last_release {
            self.update_ui.last_release = update_last_release;
        }
        let was_card_visible = self.update_ui.is_card_visible(
            self.settings.sidebar_mode,
            self.sidebar_state.is_shown(),
            self.settings.dismissed_update_version.as_deref(),
        );
        let update_state_changed = self.update_ui.set_update_state(update_state);
        let now_card_visible = self.update_ui.is_card_visible(
            self.settings.sidebar_mode,
            self.sidebar_state.is_shown(),
            self.settings.dismissed_update_version.as_deref(),
        );
        if update_state_changed
            && (was_card_visible || now_card_visible || self.active_modal == ActiveModal::Palette)
        {
            changed = true;
        }

        if rev != self.sync.last_synced_revision {
            let store = cx.global::<SharedAppStore>();
            if self.connection.host_sessions != store.host_sessions
                || self.connection.host_statuses != store.host_status_snapshots()
            {
                changed = true;
            }
            self.connection.host_sessions = store.host_sessions.clone();
            self.connection.host_statuses = store.host_status_snapshots();
            self.connection.host_txs = store
                .host_connections
                .iter()
                .filter_map(|(host, entry)| entry.daemon_tx.clone().map(|tx| (host.clone(), tx)))
                .collect();
            let (
                sessions,
                workspace_store,
                palette_stats,
                unread_only,
                connected_host,
                connection_status,
                daemon_tx,
                connection_gen,
                daemon_version,
                daemon_binary_hash,
                daemon_notice,
                daemon_notice_seq,
                update_notice,
                update_notice_seq,
            ) = {
                let store = cx.global::<SharedAppStore>();
                (
                    store.sessions.clone(),
                    store.workspace_store.clone(),
                    store.palette_stats.clone(),
                    store.unread_only,
                    store.connected_host.clone(),
                    store.connection_status.clone(),
                    store.daemon_tx.clone(),
                    store.connection_gen,
                    store.daemon_version.clone(),
                    store.daemon_binary_hash.clone(),
                    store.daemon_notice.clone(),
                    store.daemon_notice_seq,
                    store.update_notice.clone(),
                    store.update_notice_seq,
                )
            };

            if connection_gen != self.sync.last_synced_connection_gen
                && cx.global::<SharedAppStore>().host_connections.is_empty()
            {
                self.sync.last_synced_connection_gen = connection_gen;
                self.clear_active_session_view();
                self.history.pending_switch_tokens.clear();
                self.history.pending_older_page_before.clear();
                self.selection.pending_spawn_open = false;
                self.notice = None;
                changed = true;
            }
            if self.connection.sessions != sessions {
                self.connection.sessions = sessions;
                if let Some(ref sel) = self.selection.selected_session.clone()
                    && let Some((_, fresh)) = self.host_for_session_key(&canonical_session_key(
                        self.active_session_host(),
                        sel.agent,
                        &sel.id,
                    ))
                {
                    self.selection.selected_session = Some(fresh);
                }
                changed = true;
            }
            if self.sidebar_state.workspace_store != workspace_store {
                self.sidebar_state.recents_collapsed = workspace_store.recents_section_collapsed;
                self.sidebar_state.workspace_store = workspace_store;
                if let Some(session) = &self.selection.selected_session {
                    let full_width = self
                        .sidebar_state
                        .workspace_store
                        .session_meta
                        .get(&self.session_key_for(session))
                        .is_some_and(|meta| meta.full_width);
                    if full_width != self.settings.shell_fullscreen {
                        self.apply_session_full_width(full_width);
                    }
                }
                changed = true;
            }
            if self.palette_state.stats != palette_stats {
                self.palette_state.stats = palette_stats;
                changed = true;
            }
            if self.sidebar_state.unread_only != unread_only {
                self.sidebar_state.unread_only = unread_only;
                changed = true;
            }
            if self.connection.connected_host != connected_host {
                self.connection.connected_host = connected_host;
                changed = true;
            }
            if self.connection.connection_status != connection_status {
                self.connection.connection_status = connection_status;
                changed = true;
            }
            if self.connection.daemon_tx.is_some() != daemon_tx.is_some() {
                self.connection.daemon_tx = daemon_tx;
                changed = true;
            } else if daemon_tx.is_some() {
                self.connection.daemon_tx = daemon_tx;
            }
            if self.connection.daemon_version != daemon_version {
                self.connection.daemon_version = daemon_version;
                changed = true;
            }
            if self.connection.daemon_binary_hash != daemon_binary_hash {
                self.connection.daemon_binary_hash = daemon_binary_hash;
                changed = true;
            }
            if daemon_notice_seq != self.sync.last_daemon_notice_seq {
                self.sync.last_daemon_notice_seq = daemon_notice_seq;
                if let Some(msg) = daemon_notice {
                    self.notice = Some(msg);
                    changed = true;
                }
            }
            if update_notice_seq != self.updater.last_update_notice_seq {
                self.updater.last_update_notice_seq = update_notice_seq;
                if let Some(msg) = update_notice {
                    self.notice = Some(msg);
                    changed = true;
                }
            }

            self.sync.last_synced_revision = rev;
        }

        if self.chat_state.pending_harness_switch.is_some()
            && self.active_session_tx().is_none_or(|tx| tx.is_closed())
        {
            self.chat_state.pending_harness_switch = None;
        }
        if changed {
            self.cleanup_unmounted_surface_state();
            cx.notify();
        }
    }
}
