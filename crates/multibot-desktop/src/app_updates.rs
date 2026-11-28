use super::*;

/// Production `UpdaterPlatform` implementation that preserves explicit CLI flags (e.g. `--gpu=...`).
pub(super) struct DesktopUpdaterPlatform {
    pub(super) updates_dir: PathBuf,
    pub(super) preserved_args: Vec<String>,
    pub(super) swapper: DefaultFileSwapper,
    pub(super) launcher: DefaultProcessLauncher,
}

impl UpdaterPlatform for DesktopUpdaterPlatform {
    fn current_exe(&self) -> anyhow::Result<PathBuf> {
        std::env::current_exe().map_err(Into::into)
    }

    fn updates_dir(&self) -> PathBuf {
        self.updates_dir.clone()
    }

    fn preserved_cli_args(&self) -> Vec<String> {
        filter_preserved_cli_args(&self.preserved_args)
    }

    fn file_swapper(&self) -> &dyn FileSwapper {
        &self.swapper
    }

    fn process_launcher(&self) -> &dyn ProcessLauncher {
        &self.launcher
    }
}

pub(super) const GENERIC_UPDATE_ERROR_COPY: &str =
    "Could not complete update. Check your connection and try again.";

/// Detects millisecond/timeout or compact duration tokens (e.g. `15000ms`, `500 ms`, `30s`,
/// `timed out`, `timeout`) that must never appear in user-facing update UI copy.
pub(super) fn has_millisecond_or_timeout_pattern(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if lower.contains("timed out") || lower.contains("timeout") {
        return true;
    }
    let bytes = lower.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            let mut u = j;
            while u < bytes.len() && bytes[u].is_ascii_whitespace() {
                u += 1;
            }
            let rest = &lower[u..];
            let unit_len =
                if rest.starts_with("ms") || rest.starts_with("us") || rest.starts_with("ns") {
                    2
                } else if rest.starts_with("sec") || rest.starts_with("min") {
                    3
                } else if u == j
                    && (rest.starts_with('s') || rest.starts_with('m') || rest.starts_with('h'))
                {
                    1
                } else {
                    0
                };
            if unit_len > 0 {
                let after = u + unit_len;
                if after >= bytes.len() || !bytes[after].is_ascii_alphanumeric() {
                    return true;
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    false
}

/// Defense-in-depth sanitizer guaranteeing no updater error copy ever contains forbidden
/// duration/time wording, millisecond/timeout tokens, or control/bidi characters — even after
/// truncating to 180 characters.
pub(crate) fn sanitize_update_error_copy(err: &anyhow::Error) -> String {
    let raw = err.to_string();
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || has_forbidden_control_or_bidi(trimmed)
        || contains_forbidden_duration_wording(trimmed)
        || has_millisecond_or_timeout_pattern(trimmed)
    {
        return GENERIC_UPDATE_ERROR_COPY.to_string();
    }
    let truncated: String = trimmed.chars().take(180).collect();
    if contains_forbidden_duration_wording(&truncated)
        || has_millisecond_or_timeout_pattern(&truncated)
    {
        GENERIC_UPDATE_ERROR_COPY.to_string()
    } else {
        truncated
    }
}

impl MultibotView {
    // =========================================================================
    // Auto-Updater Background Runtime & Interactive Actions
    // =========================================================================

    pub(crate) fn dispatch_update_ui_action(
        &mut self,
        action: UpdateUiAction,
        cx: &mut Context<Self>,
    ) {
        let tx = self.updater.update_action_tx.clone().or_else(|| {
            cx.try_global::<SharedAppStore>()
                .and_then(|s| s.update_action_tx.clone())
        });
        if let Some(tx) = tx {
            self.updater.update_action_tx = Some(tx.clone());
            self.updater.update_in_flight = true;
            if cx.has_global::<SharedAppStore>() {
                cx.global_mut::<SharedAppStore>().update_in_flight = true;
            }
            let _ = tx.send(action);
        }
    }

    pub(crate) fn start_update_runtime(&mut self, cx: &mut Context<Self>) {
        if cx.has_global::<SharedAppStore>() && cx.global::<SharedAppStore>().update_poller_started
        {
            self.updater.update_action_tx = cx.global::<SharedAppStore>().update_action_tx.clone();
            return;
        }
        let Ok(config) = UpdateClientConfig::from_env(multibot_proto::build_version()) else {
            return;
        };
        let platform: Arc<dyn UpdaterPlatform> = Arc::new(DesktopUpdaterPlatform {
            updates_dir: MultibotSettings::updates_dir(),
            preserved_args: self.updater.preserved_cli_args.clone(),
            swapper: DefaultFileSwapper,
            launcher: DefaultProcessLauncher,
        });
        self.start_update_runtime_with_config(
            config,
            DEFAULT_POLL_INTERVAL,
            Duration::from_secs(10),
            platform,
            cx,
        );
    }

    pub(crate) fn start_update_runtime_with_config(
        &mut self,
        config: UpdateClientConfig,
        poll_interval: Duration,
        initial_delay: Duration,
        platform: Arc<dyn UpdaterPlatform>,
        cx: &mut Context<Self>,
    ) {
        let (action_tx, mut action_rx) = mpsc::unbounded_channel::<UpdateUiAction>();
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<UpdateBridgeEvent>();

        self.updater.update_action_tx = Some(action_tx.clone());
        if cx.has_global::<SharedAppStore>() {
            let store = cx.global_mut::<SharedAppStore>();
            store.update_action_tx = Some(action_tx);
            store.update_poller_started = true;
        }

        let client = Arc::new(UpdateClient::new(config));
        let updates_dir = platform.updates_dir();

        // 1. App-scoped GPUI bridge consumer (survives closing Window 1 while Window 2 is open;
        //    exits cleanly only when SharedAppStore.window_count == 0).
        //    Strictly demand-driven: zero redraws on 304/silent failures/etag-only updates.
        let app: &mut App = cx;
        app.spawn(async move |cx| {
            while let Some(ev) = ev_rx.recv().await {
                let alive = cx.update(|cx| {
                    if !cx.has_global::<SharedAppStore>()
                        || cx.global::<SharedAppStore>().window_count == 0
                    {
                        return false;
                    }
                    Self::apply_update_bridge_event_to_app(ev, cx);
                    true
                });
                if !alive {
                    break;
                }
            }
        })
        .detach();

        // 2. App-scoped demand-friendly 5-minute background poller with rate-limit backoff.
        let poll_client = client.clone();
        let poll_updates_dir = updates_dir.clone();
        let poll_ev_tx = ev_tx.clone();
        let poll_rt = self.runtime.clone();
        app.spawn(async move |cx| {
            if !initial_delay.is_zero() {
                cx.background_executor().timer(initial_delay).await;
            }
            loop {
                let state_opt = cx.update(|cx| {
                    if !cx.has_global::<SharedAppStore>() {
                        return None;
                    }
                    let store = cx.global::<SharedAppStore>();
                    if store.window_count == 0 {
                        return None;
                    }
                    Some((
                        store.settings.auto_check_updates,
                        store.settings.last_update_etag.clone(),
                        store.update_in_flight,
                    ))
                });
                let Some((auto_check, etag, in_flight)) = state_opt else {
                    break;
                };

                let mut next_sleep = poll_interval;
                if auto_check && !in_flight {
                    let c = poll_client.clone();
                    let dir = poll_updates_dir.clone();
                    let res = poll_rt
                        .spawn(async move {
                            tokio::task::spawn_blocking(move || {
                                c.check_for_update(etag.as_deref(), &dir)
                            })
                            .await
                        })
                        .await;
                    match res {
                        Ok(Ok(Ok(CheckOutcome::RateLimited { retry_after, etag }))) => {
                            next_sleep = retry_after.max(poll_interval);
                            let _ = poll_ev_tx.send(UpdateBridgeEvent::CheckCompleted {
                                outcome: CheckOutcome::RateLimited { retry_after, etag },
                                manual: false,
                            });
                        }
                        Ok(Ok(Ok(outcome))) => {
                            let _ = poll_ev_tx.send(UpdateBridgeEvent::CheckCompleted {
                                outcome,
                                manual: false,
                            });
                        }
                        Ok(Ok(Err(err))) => {
                            let _ = poll_ev_tx.send(UpdateBridgeEvent::CheckFailed {
                                message: sanitize_update_error_copy(&err),
                                manual: false,
                            });
                        }
                        _ => {}
                    }
                }
                cx.background_executor().timer(next_sleep).await;
            }
        })
        .detach();

        // 3. Tokio action worker for interactive Check / Download+Swap+Relaunch / Restart.
        let worker_client = client;
        let worker_updates_dir = updates_dir;
        let worker_ev_tx = ev_tx;
        let worker_platform = platform;
        self.runtime.spawn(async move {
            while let Some(action) = action_rx.recv().await {
                match action {
                    UpdateUiAction::CheckForUpdates | UpdateUiAction::RetryUpdate => {
                        let c = worker_client.clone();
                        let dir = worker_updates_dir.clone();
                        let res =
                            tokio::task::spawn_blocking(move || c.check_for_update(None, &dir))
                                .await;
                        match res {
                            Ok(Ok(outcome)) => {
                                let _ = worker_ev_tx.send(UpdateBridgeEvent::CheckCompleted {
                                    outcome,
                                    manual: true,
                                });
                            }
                            Ok(Err(err)) => {
                                let _ = worker_ev_tx.send(UpdateBridgeEvent::CheckFailed {
                                    message: sanitize_update_error_copy(&err),
                                    manual: true,
                                });
                            }
                            Err(_) => {
                                let _ = worker_ev_tx.send(UpdateBridgeEvent::CheckFailed {
                                    message: "Could not check for updates.".to_string(),
                                    manual: true,
                                });
                            }
                        }
                    }
                    UpdateUiAction::DownloadAndRestart(release) => {
                        let c = worker_client.clone();
                        let dir = worker_updates_dir.clone();
                        let plat = worker_platform.clone();
                        let tx_prog = worker_ev_tx.clone();
                        let res = tokio::task::spawn_blocking(move || {
                            let version = UpdateUiState::normalize_version(&release.version);
                            let mut last_pct: Option<u8> = None;
                            let mut last_reported_bytes: u64 = 0;
                            let staged =
                                c.download_and_stage_update(&release, &dir, |done, total| {
                                    let should_emit = match total {
                                        Some(t) if t > 0 => {
                                            let pct =
                                                ((done.saturating_mul(100)) / t).min(100) as u8;
                                            if last_pct != Some(pct) {
                                                last_pct = Some(pct);
                                                true
                                            } else {
                                                false
                                            }
                                        }
                                        _ => {
                                            if done.saturating_sub(last_reported_bytes)
                                                >= 256 * 1024
                                            {
                                                last_reported_bytes = done;
                                                true
                                            } else {
                                                false
                                            }
                                        }
                                    };
                                    if should_emit {
                                        let _ = tx_prog.send(UpdateBridgeEvent::DownloadProgress {
                                            version: version.clone(),
                                            bytes_done: done,
                                            bytes_total: total,
                                        });
                                        if total.is_some_and(|t| t > 0 && done >= t) {
                                            let _ = tx_prog.send(UpdateBridgeEvent::Verifying {
                                                version: version.clone(),
                                            });
                                        }
                                    }
                                })?;

                            let _ = tx_prog.send(UpdateBridgeEvent::StagedReady(staged.clone()));
                            AtomicSwapper::default().apply_staged_update(plat.as_ref(), &staged)?;
                            Ok::<(), anyhow::Error>(())
                        })
                        .await;
                        match res {
                            Ok(Ok(())) => {
                                let _ = worker_ev_tx.send(UpdateBridgeEvent::ApplySucceeded);
                            }
                            Ok(Err(err)) => {
                                let retryable = !err.to_string().contains("not writable");
                                let _ = worker_ev_tx.send(UpdateBridgeEvent::Failed {
                                    message: sanitize_update_error_copy(&err),
                                    retryable,
                                });
                            }
                            Err(_) => {
                                let _ = worker_ev_tx.send(UpdateBridgeEvent::Failed {
                                    message: "Could not complete update.".to_string(),
                                    retryable: true,
                                });
                            }
                        }
                    }
                    UpdateUiAction::ApplyAndRestart(staged) => {
                        let plat = worker_platform.clone();
                        let res = tokio::task::spawn_blocking(move || {
                            AtomicSwapper::default().apply_staged_update(plat.as_ref(), &staged)
                        })
                        .await;
                        match res {
                            Ok(Ok(_)) => {
                                let _ = worker_ev_tx.send(UpdateBridgeEvent::ApplySucceeded);
                            }
                            Ok(Err(err)) => {
                                let retryable = !err.to_string().contains("not writable");
                                let _ = worker_ev_tx.send(UpdateBridgeEvent::Failed {
                                    message: sanitize_update_error_copy(&err),
                                    retryable,
                                });
                            }
                            Err(_) => {
                                let _ = worker_ev_tx.send(UpdateBridgeEvent::Failed {
                                    message: "Could not complete update.".to_string(),
                                    retryable: true,
                                });
                            }
                        }
                    }
                    UpdateUiAction::RestartApp => {
                        let plat = worker_platform.clone();
                        let res = tokio::task::spawn_blocking(move || {
                            let exe = plat.current_exe()?;
                            let args = plat.preserved_cli_args();
                            plat.process_launcher().spawn_relaunch(&exe, &args)
                        })
                        .await;
                        match res {
                            Ok(Ok(_)) => {
                                let _ = worker_ev_tx.send(UpdateBridgeEvent::RestartSucceeded);
                            }
                            Ok(Err(err)) => {
                                let _ = worker_ev_tx.send(UpdateBridgeEvent::Failed {
                                    message: sanitize_update_error_copy(&err),
                                    retryable: true,
                                });
                            }
                            Err(_) => {
                                let _ = worker_ev_tx.send(UpdateBridgeEvent::Failed {
                                    message: "Could not restart Multi Bot.".to_string(),
                                    retryable: true,
                                });
                            }
                        }
                    }
                }
            }
        });
    }

    #[allow(dead_code)]
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

    pub(super) fn persist_shared_update_settings(store: &SharedAppStore) {
        let _ = match store.update_settings_path_override.as_deref() {
            Some(path) => store.settings.save_to(path),
            None if store.persist_to_disk => store.settings.save(),
            None => Ok(()),
        };
    }

    pub(super) fn is_update_state_visible_for_settings(
        state: &UpdateState,
        settings: &MultibotSettings,
    ) -> bool {
        let mut probe = UpdateUiState::default();
        probe.state = state.clone();
        probe.is_card_visible(
            settings.sidebar_mode,
            settings.sidebar_mode != SidebarMode::Hidden,
            settings.dismissed_update_version.as_deref(),
        )
    }

    /// Applies an [`UpdateBridgeEvent`] at App scope onto [`SharedAppStore`] so closing Window 1
    /// never bricks Window 2's updater actions or `cx.quit()`, while guaranteeing ZERO sibling
    /// `cx.notify()` redraws on silent background `304`, `RateLimited`, `CheckFailed`, or
    /// `last_update_etag`-only updates.
    pub(crate) fn apply_update_bridge_event_to_app(event: UpdateBridgeEvent, cx: &mut App) {
        if !cx.has_global::<SharedAppStore>() {
            return;
        }
        match event {
            UpdateBridgeEvent::CheckCompleted { outcome, manual } => match outcome {
                CheckOutcome::NotModified => {
                    if !manual {
                        // Hard Invariant #4: 304 Not Modified triggers ZERO UI redraws!
                        if cx.global::<SharedAppStore>().update_in_flight {
                            cx.global_mut::<SharedAppStore>().update_in_flight = false;
                        }
                        return;
                    }
                    let store = cx.global_mut::<SharedAppStore>();
                    store.update_in_flight = false;
                    if let Some(rel) = store.update_last_release.clone() {
                        store.update_state = UpdateState::Available(rel);
                    } else {
                        store.update_state = UpdateState::UpToDate;
                        let cur = UpdateUiState::normalize_version(multibot_proto::build_version());
                        store.update_notice = Some(format!("Multibot v{cur} is up to date."));
                        store.update_notice_seq = store.update_notice_seq.wrapping_add(1);
                    }
                    store.bump_revision();
                }
                CheckOutcome::UpToDate { etag } => {
                    let store = cx.global_mut::<SharedAppStore>();
                    store.update_in_flight = false;
                    if etag.is_some() && etag != store.settings.last_update_etag {
                        store.settings.last_update_etag = etag;
                        Self::persist_shared_update_settings(store);
                    }
                    let was_visible = Self::is_update_state_visible_for_settings(
                        &store.update_state,
                        &store.settings,
                    );
                    let changed = store.update_state != UpdateState::UpToDate;
                    store.update_state = UpdateState::UpToDate;
                    if manual {
                        let cur = UpdateUiState::normalize_version(multibot_proto::build_version());
                        store.update_notice = Some(format!("Multibot v{cur} is up to date."));
                        store.update_notice_seq = store.update_notice_seq.wrapping_add(1);
                        store.bump_revision();
                    } else if changed && was_visible {
                        store.bump_revision();
                    }
                }
                CheckOutcome::RateLimited {
                    retry_after: _,
                    etag,
                } => {
                    if !manual {
                        let needs_etag = etag.is_some()
                            && etag != cx.global::<SharedAppStore>().settings.last_update_etag;
                        let was_in_flight = cx.global::<SharedAppStore>().update_in_flight;
                        if needs_etag || was_in_flight {
                            let store = cx.global_mut::<SharedAppStore>();
                            store.update_in_flight = false;
                            if needs_etag {
                                store.settings.last_update_etag = etag;
                                Self::persist_shared_update_settings(store);
                            }
                        }
                        return;
                    }
                    let store = cx.global_mut::<SharedAppStore>();
                    store.update_in_flight = false;
                    if etag.is_some() && etag != store.settings.last_update_etag {
                        store.settings.last_update_etag = etag;
                        Self::persist_shared_update_settings(store);
                    }
                    if let Some(rel) = store.update_last_release.clone() {
                        store.update_state = UpdateState::Available(rel);
                    } else {
                        store.update_state = UpdateState::Idle;
                    }
                    store.update_notice = Some(
                        "Update check temporarily rate-limited. Please try again later."
                            .to_string(),
                    );
                    store.update_notice_seq = store.update_notice_seq.wrapping_add(1);
                    store.bump_revision();
                }
                CheckOutcome::UpdateAvailable { release, etag } => {
                    let store = cx.global_mut::<SharedAppStore>();
                    store.update_in_flight = false;
                    let mut settings_dirty = false;
                    if etag.is_some() && etag != store.settings.last_update_etag {
                        store.settings.last_update_etag = etag;
                        settings_dirty = true;
                    }
                    if manual
                        && UpdateUiState::is_version_dismissed(
                            &release.version,
                            store.settings.dismissed_update_version.as_deref(),
                        )
                    {
                        store.settings.dismissed_update_version = None;
                        settings_dirty = true;
                    }
                    if settings_dirty {
                        Self::persist_shared_update_settings(store);
                    }
                    let was_visible = Self::is_update_state_visible_for_settings(
                        &store.update_state,
                        &store.settings,
                    );
                    let next_state = UpdateState::Available(release.clone());
                    let changed = store.update_state != next_state;
                    store.update_last_release = Some(release.clone());
                    store.update_state = next_state;
                    let now_visible = Self::is_update_state_visible_for_settings(
                        &store.update_state,
                        &store.settings,
                    );
                    if manual {
                        let ver = UpdateUiState::normalize_version(&release.version);
                        store.update_notice = Some(format!("Multibot v{ver} is ready."));
                        store.update_notice_seq = store.update_notice_seq.wrapping_add(1);
                        store.bump_revision();
                    } else if changed && (was_visible || now_visible) {
                        store.bump_revision();
                    }
                }
            },
            UpdateBridgeEvent::CheckFailed { message, manual } => {
                if !manual {
                    if cx.global::<SharedAppStore>().update_in_flight {
                        cx.global_mut::<SharedAppStore>().update_in_flight = false;
                    }
                    return;
                }
                let store = cx.global_mut::<SharedAppStore>();
                store.update_in_flight = false;
                store.update_state = UpdateState::Error {
                    message,
                    retryable: true,
                };
                store.bump_revision();
            }
            UpdateBridgeEvent::DownloadProgress {
                version,
                bytes_done,
                bytes_total,
            } => {
                let next = UpdateState::Downloading {
                    version,
                    bytes_done,
                    bytes_total,
                };
                let store = cx.global_mut::<SharedAppStore>();
                if store.update_state != next {
                    store.update_state = next;
                    store.bump_revision();
                }
            }
            UpdateBridgeEvent::Verifying { version } => {
                let next = UpdateState::Verifying { version };
                let store = cx.global_mut::<SharedAppStore>();
                if store.update_state != next {
                    store.update_state = next;
                    store.bump_revision();
                }
            }
            UpdateBridgeEvent::StagedReady(staged) => {
                let ver = UpdateUiState::normalize_version(&staged.version);
                let store = cx.global_mut::<SharedAppStore>();
                store.update_state = UpdateState::Applying {
                    version: ver.clone(),
                };
                store.update_notice = Some(format!("Restarting to apply v{ver}..."));
                store.update_notice_seq = store.update_notice_seq.wrapping_add(1);
                store.bump_revision();
            }
            UpdateBridgeEvent::ApplySucceeded | UpdateBridgeEvent::RestartSucceeded => {
                cx.global_mut::<SharedAppStore>().update_in_flight = false;
                cx.quit();
            }
            UpdateBridgeEvent::Failed { message, retryable } => {
                let store = cx.global_mut::<SharedAppStore>();
                store.update_in_flight = false;
                store.update_state = UpdateState::Error { message, retryable };
                store.bump_revision();
            }
        }
    }
}
