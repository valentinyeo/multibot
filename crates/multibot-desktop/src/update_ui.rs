use crate::app::MultibotView;
use crate::state::{PaletteEntry, SharedAppStore};
use crate::theme::{self, RADIUS_CHIP, RADIUS_ROW};
use gpui_kit::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement, Styled, TestSupportExt,
    component::{
        Sizable,
        button::{Button, ButtonVariants},
        h_flex, v_flex,
    },
    div, px, relative,
};
use multibot_core::{MultibotSettings, SidebarMode};
use multibot_updater::{
    ReleaseInfo, StagedUpdate, UpdateState,
    contains_forbidden_duration_wording as updater_contains_forbidden_duration_wording,
    has_forbidden_control_or_bidi, is_newer_version,
};
use std::path::PathBuf;

pub(crate) const MAX_COLLAPSED_NOTES: usize = 3;

pub(crate) const PALETTE_CMD_UPDATE_CHECK: &str = "pal-cmd-update-check";
pub(crate) const PALETTE_CMD_UPDATE_ACTION: &str = "pal-cmd-update-action";
pub(crate) const PALETTE_CMD_UPDATE_RESTART: &str = "pal-cmd-update-restart";
pub(crate) const PALETTE_CMD_UPDATE_CHANGELOG: &str = "pal-cmd-update-changelog";

/// Deterministic UI intents emitted by the update card and `Ctrl+K` commands for the app runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UpdateUiAction {
    CheckForUpdates,
    DownloadAndRestart(ReleaseInfo),
    ApplyAndRestart(StagedUpdate),
    RestartApp,
    RetryUpdate,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct UpdateUiState {
    pub state: UpdateState,
    pub notes_expanded: bool,
    pub error_dismissed: bool,
    pub last_release: Option<ReleaseInfo>,
    pub pending_action: Option<UpdateUiAction>,
    pub settings_path_override: Option<PathBuf>,
}

impl UpdateUiState {
    pub(crate) fn normalize_version(raw: &str) -> String {
        let trimmed = raw.trim();
        trimmed.strip_prefix('v').unwrap_or(trimmed).to_string()
    }

    /// Updates `self.state` and returns `true` ONLY when state actually changed
    /// (Hard Invariant #4: zero UI redraws on `304 Not Modified` / unchanged checks).
    pub(crate) fn set_update_state(&mut self, next: UpdateState) -> bool {
        if self.state == next {
            return false;
        }
        if let UpdateState::Available(ref info) = next {
            let changed_version = self
                .last_release
                .as_ref()
                .map(|prev| Self::normalize_version(&prev.version) != Self::normalize_version(&info.version))
                .unwrap_or(true);
            if changed_version {
                self.notes_expanded = false;
            }
            self.last_release = Some(info.clone());
        }
        self.error_dismissed = false;
        self.state = next;
        true
    }

    /// Returns `true` when `candidate_version` should stay hidden due to `dismissed_version`.
    /// Re-shows automatically whenever a strictly newer SemVer release arrives.
    pub(crate) fn is_version_dismissed(
        candidate_version: &str,
        dismissed_version: Option<&str>,
    ) -> bool {
        let Some(dismissed) = dismissed_version.map(str::trim).filter(|s| !s.is_empty()) else {
            return false;
        };
        match is_newer_version(dismissed, candidate_version) {
            Ok(true) => false,
            Ok(false) => true,
            Err(_) => {
                Self::normalize_version(candidate_version) == Self::normalize_version(dismissed)
            }
        }
    }

    pub(crate) fn is_card_visible(
        &self,
        sidebar_mode: SidebarMode,
        sidebar_shown: bool,
        dismissed_version: Option<&str>,
    ) -> bool {
        if !sidebar_shown || sidebar_mode != SidebarMode::Open {
            return false;
        }
        match &self.state {
            UpdateState::Available(info) => {
                !Self::is_version_dismissed(&info.version, dismissed_version)
            }
            UpdateState::ReadyToRestart(staged) => {
                !Self::is_version_dismissed(&staged.version, dismissed_version)
            }
            UpdateState::Downloading { .. } | UpdateState::Verifying { .. } => true,
            UpdateState::Error { .. } => !self.error_dismissed,
            UpdateState::Idle
            | UpdateState::Checking
            | UpdateState::UpToDate
            | UpdateState::Applying { .. } => false,
        }
    }

    /// Dismisses the currently displayed update prompt, persisting the normalized version into
    /// `settings.dismissed_update_version` for versioned prompts.
    pub(crate) fn dismiss_current_version(
        &mut self,
        settings: &mut MultibotSettings,
    ) -> Option<String> {
        match &self.state {
            UpdateState::Available(info) => {
                let ver = Self::normalize_version(&info.version);
                settings.dismissed_update_version = Some(ver.clone());
                self.notes_expanded = false;
                Some(ver)
            }
            UpdateState::ReadyToRestart(staged) => {
                let ver = Self::normalize_version(&staged.version);
                settings.dismissed_update_version = Some(ver.clone());
                self.notes_expanded = false;
                Some(ver)
            }
            UpdateState::Error { .. } => {
                self.error_dismissed = true;
                None
            }
            _ => None,
        }
    }

    pub(crate) fn toggle_notes_expanded(&mut self) {
        self.notes_expanded = !self.notes_expanded;
    }

    /// Defense-in-depth filter stripping blank lines, control/bidi characters, and any forbidden
    /// time/duration wording so even a directly constructed `ReleaseInfo`/`StagedUpdate` cannot
    /// render prohibited or spoofed copy.
    pub(crate) fn safe_notes<'a>(all_notes: &'a [String]) -> Vec<&'a str> {
        all_notes
            .iter()
            .map(String::as_str)
            .filter(|note| {
                if has_forbidden_control_or_bidi(note) {
                    return false;
                }
                let trimmed = note.trim();
                !trimmed.is_empty() && !contains_forbidden_duration_wording(trimmed)
            })
            .collect()
    }

    pub(crate) fn visible_notes<'a>(&self, all_notes: &'a [String]) -> Vec<&'a str> {
        let safe = Self::safe_notes(all_notes);
        if self.notes_expanded || safe.len() <= MAX_COLLAPSED_NOTES {
            safe
        } else {
            safe[..MAX_COLLAPSED_NOTES].to_vec()
        }
    }

    pub(crate) fn has_more_notes(all_notes: &[String]) -> bool {
        Self::safe_notes(all_notes).len() > MAX_COLLAPSED_NOTES
    }

    /// Overflow-safe download progress calculation returning `(0.0..=1.0, 0..=100)`.
    pub(crate) fn download_progress(bytes_done: u64, bytes_total: Option<u64>) -> Option<(f32, u8)> {
        let total = bytes_total.filter(|&t| t > 0)?;
        let clamped = bytes_done.min(total);
        let frac = (clamped as f64 / total as f64).clamp(0.0, 1.0) as f32;
        let pct = ((clamped as u128 * 100) / total as u128) as u8;
        Some((frac, pct))
    }

    fn ready_headline(version: &str) -> String {
        format!("v{} is ready", Self::normalize_version(version))
    }

    fn downloading_headline(version: &str) -> String {
        format!("Downloading v{}", Self::normalize_version(version))
    }

    fn verifying_headline(version: &str) -> String {
        format!("Verifying v{}", Self::normalize_version(version))
    }

    fn error_detail(message: &str) -> String {
        let trimmed = message.trim();
        if trimmed.is_empty() {
            "Could not complete update.".to_string()
        } else {
            trimmed.to_string()
        }
    }

    /// Single source of truth for all rendered card copy (verified by automated zero-duration tests).
    #[cfg(test)]
    pub(crate) fn formatted_card_strings(&self) -> Vec<String> {
        match &self.state {
            UpdateState::Available(info) => {
                let mut out = vec![Self::ready_headline(&info.version), "×".to_string()];
                for note in self.visible_notes(&info.notes) {
                    out.push(format!("• {note}"));
                }
                if Self::has_more_notes(&info.notes) {
                    out.push(
                        if self.notes_expanded {
                            "Show less"
                        } else {
                            "Show more"
                        }
                        .to_string(),
                    );
                }
                out.push("Update & Restart".to_string());
                out.push("Later".to_string());
                out
            }
            UpdateState::ReadyToRestart(staged) => {
                let mut out = vec![Self::ready_headline(&staged.version), "×".to_string()];
                for note in self.visible_notes(&staged.notes) {
                    out.push(format!("• {note}"));
                }
                if Self::has_more_notes(&staged.notes) {
                    out.push(
                        if self.notes_expanded {
                            "Show less"
                        } else {
                            "Show more"
                        }
                        .to_string(),
                    );
                }
                out.push("Update & Restart".to_string());
                out.push("Later".to_string());
                out
            }
            UpdateState::Downloading {
                version,
                bytes_done,
                bytes_total,
            } => {
                let mut out = vec![Self::downloading_headline(version)];
                if let Some((_, pct)) = Self::download_progress(*bytes_done, *bytes_total) {
                    out.push(format!("{pct}%"));
                }
                out
            }
            UpdateState::Verifying { version } => vec![
                Self::verifying_headline(version),
                "Checking package integrity".to_string(),
            ],
            UpdateState::Error { message, retryable } => {
                let mut out = vec![
                    "Update failed".to_string(),
                    "×".to_string(),
                    Self::error_detail(message),
                ];
                if *retryable {
                    out.push("Retry".to_string());
                }
                out.push("Dismiss".to_string());
                out
            }
            UpdateState::Idle
            | UpdateState::Checking
            | UpdateState::UpToDate
            | UpdateState::Applying { .. } => Vec::new(),
        }
    }

    pub(crate) fn format_changelog_notice(&self, current_version: &str) -> String {
        let loaded = match &self.state {
            UpdateState::Available(info) => {
                let safe = Self::safe_notes(&info.notes);
                (!safe.is_empty()).then(|| (Self::normalize_version(&info.version), safe))
            }
            UpdateState::ReadyToRestart(staged) => {
                let safe = Self::safe_notes(&staged.notes);
                (!safe.is_empty()).then(|| (Self::normalize_version(&staged.version), safe))
            }
            _ => self.last_release.as_ref().and_then(|info| {
                let safe = Self::safe_notes(&info.notes);
                (!safe.is_empty()).then(|| (Self::normalize_version(&info.version), safe))
            }),
        };

        if let Some((ver, notes)) = loaded {
            format!("Changelog v{ver}: {}", notes.join(" · "))
        } else {
            format!(
                "Multibot v{} — no release notes loaded.",
                Self::normalize_version(current_version)
            )
        }
    }

    pub(crate) fn palette_entries(&self, current_version: &str) -> Vec<PaletteEntry> {
        let action_label = match &self.state {
            UpdateState::ReadyToRestart(staged) => {
                format!(
                    "Update: Relaunch to v{}",
                    Self::normalize_version(&staged.version)
                )
            }
            UpdateState::Available(_) => "Update: Download & Relaunch".to_string(),
            UpdateState::Downloading {
                version,
                bytes_done,
                bytes_total,
            } => {
                let ver = Self::normalize_version(version);
                if let Some((_, pct)) = Self::download_progress(*bytes_done, *bytes_total) {
                    format!("Update: Downloading v{ver} ({pct}%)")
                } else {
                    format!("Update: Downloading v{ver}")
                }
            }
            UpdateState::Verifying { version } => {
                format!("Update: Verifying v{}", Self::normalize_version(version))
            }
            UpdateState::Applying { version } => {
                format!("Update: Applying v{}", Self::normalize_version(version))
            }
            UpdateState::Checking => "Update: Checking...".to_string(),
            UpdateState::Error { .. } => "Update: Retry Update".to_string(),
            UpdateState::Idle | UpdateState::UpToDate => {
                format!(
                    "Update: Up to date (v{})",
                    Self::normalize_version(current_version)
                )
            }
        };

        vec![
            PaletteEntry {
                id: PALETTE_CMD_UPDATE_CHECK.to_string(),
                label: "Update: Check for Updates".to_string(),
                chord: String::new(),
                category: "Updates",
            },
            PaletteEntry {
                id: PALETTE_CMD_UPDATE_ACTION.to_string(),
                label: action_label,
                chord: String::new(),
                category: "Updates",
            },
            PaletteEntry {
                id: PALETTE_CMD_UPDATE_RESTART.to_string(),
                label: "Restart Multi Bot".to_string(),
                chord: String::new(),
                category: "Updates",
            },
            PaletteEntry {
                id: PALETTE_CMD_UPDATE_CHANGELOG.to_string(),
                label: "Changelog".to_string(),
                chord: String::new(),
                category: "Updates",
            },
        ]
    }
}

/// Delegates to [`multibot_updater::contains_forbidden_duration_wording`] (Unicode-whitespace plus
/// ZWSP `\u{200B}` and BOM `\u{FEFF}` aware, keeping ZWJ `\u{200D}` intact) to enforce Hard
/// Invariant #4 (zero time/duration wording on update UI copy).
pub(crate) fn contains_forbidden_duration_wording(text: &str) -> bool {
    updater_contains_forbidden_duration_wording(text)
}

impl MultibotView {
    #[allow(dead_code)]
    pub(crate) fn set_update_state(&mut self, next: UpdateState, cx: &mut Context<Self>) {
        if self.update_ui.set_update_state(next) {
            cx.notify();
        }
    }

    pub(crate) fn persist_update_settings(&self, cx: &mut Context<Self>) {
        if cx.has_global::<SharedAppStore>() {
            cx.global_mut::<SharedAppStore>().settings = self.settings.clone();
        }
        let _ = match self.update_ui.settings_path_override.as_deref() {
            Some(path) => self.settings.save_to(path),
            None if self.is_disk_persistence_enabled(cx) => self.settings.save(),
            None => Ok(()),
        };
    }

    pub(crate) fn dismiss_current_update(&mut self, cx: &mut Context<Self>) {
        let persisted = self
            .update_ui
            .dismiss_current_version(&mut self.settings)
            .is_some();
        if persisted {
            self.persist_update_settings(cx);
        }
        cx.notify();
    }

    pub(crate) fn trigger_update_check(&mut self, cx: &mut Context<Self>) {
        if self.update_in_flight
            || matches!(
                self.update_ui.state,
                UpdateState::Checking
                    | UpdateState::Downloading { .. }
                    | UpdateState::Verifying { .. }
                    | UpdateState::Applying { .. }
            )
        {
            return;
        }
        self.update_ui.error_dismissed = false;
        self.update_ui.pending_action = Some(UpdateUiAction::CheckForUpdates);
        self.dispatch_update_ui_action(UpdateUiAction::CheckForUpdates, cx);
        self.update_ui.state = UpdateState::Checking;
        self.sync_update_state_to_shared(cx);
        self.notice = Some("Checking for updates...".to_string());
        cx.notify();
    }

    pub(crate) fn retry_update(&mut self, cx: &mut Context<Self>) {
        self.update_ui.error_dismissed = false;
        if let Some(info) = self.update_ui.last_release.clone() {
            let ver = UpdateUiState::normalize_version(&info.version);
            let total = (info.asset.size_bytes > 0).then_some(info.asset.size_bytes);
            let action = UpdateUiAction::DownloadAndRestart(info);
            self.update_ui.pending_action = Some(action.clone());
            self.dispatch_update_ui_action(action, cx);
            self.update_ui.state = UpdateState::Downloading {
                version: ver.clone(),
                bytes_done: 0,
                bytes_total: total,
            };
            self.sync_update_state_to_shared(cx);
            self.notice = Some(format!("Downloading Multibot v{ver}..."));
        } else {
            self.update_ui.pending_action = Some(UpdateUiAction::RetryUpdate);
            self.dispatch_update_ui_action(UpdateUiAction::RetryUpdate, cx);
            self.update_ui.state = UpdateState::Checking;
            self.sync_update_state_to_shared(cx);
            self.notice = Some("Checking for updates...".to_string());
        }
        cx.notify();
    }

    pub(crate) fn trigger_primary_update_action(&mut self, cx: &mut Context<Self>) {
        match self.update_ui.state.clone() {
            UpdateState::Available(info) => {
                let ver = UpdateUiState::normalize_version(&info.version);
                self.update_ui.last_release = Some(info.clone());
                if UpdateUiState::is_version_dismissed(
                    &info.version,
                    self.settings.dismissed_update_version.as_deref(),
                ) {
                    self.settings.dismissed_update_version = None;
                    self.persist_update_settings(cx);
                }
                let total = (info.asset.size_bytes > 0).then_some(info.asset.size_bytes);
                let action = UpdateUiAction::DownloadAndRestart(info);
                self.update_ui.pending_action = Some(action.clone());
                self.dispatch_update_ui_action(action, cx);
                self.update_ui.state = UpdateState::Downloading {
                    version: ver.clone(),
                    bytes_done: 0,
                    bytes_total: total,
                };
                self.sync_update_state_to_shared(cx);
                self.notice = Some(format!("Downloading Multibot v{ver}..."));
                cx.notify();
            }
            UpdateState::ReadyToRestart(staged) => {
                let ver = UpdateUiState::normalize_version(&staged.version);
                let action = UpdateUiAction::ApplyAndRestart(staged);
                self.update_ui.pending_action = Some(action.clone());
                self.dispatch_update_ui_action(action, cx);
                self.update_ui.state = UpdateState::Applying {
                    version: ver.clone(),
                };
                self.sync_update_state_to_shared(cx);
                self.notice = Some(format!("Restarting to apply v{ver}..."));
                cx.notify();
            }
            UpdateState::Error {
                retryable: true, ..
            } => {
                self.retry_update(cx);
            }
            UpdateState::UpToDate => {
                let cur = UpdateUiState::normalize_version(multibot_proto::build_version());
                self.notice = Some(format!("Multibot v{cur} is up to date."));
                cx.notify();
            }
            UpdateState::Idle => {
                self.trigger_update_check(cx);
            }
            UpdateState::Checking
            | UpdateState::Downloading { .. }
            | UpdateState::Verifying { .. }
            | UpdateState::Applying { .. }
            | UpdateState::Error {
                retryable: false, ..
            } => {}
        }
    }

    fn render_ready_card_body(
        &self,
        version: &str,
        notes: &[String],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut card = v_flex()
            .id("update-card")
            .test_support()
            .w_full()
            .px_3()
            .py_2p5()
            .gap_2()
            .rounded(RADIUS_ROW)
            .bg(theme::bg_surface())
            .border_1()
            .border_color(theme::border_subtle())
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .gap_1()
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1p5()
                            .child(
                                div()
                                    .w(px(6.0))
                                    .h(px(6.0))
                                    .rounded(RADIUS_CHIP)
                                    .bg(theme::accent_green()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::text_primary())
                                    .child(UpdateUiState::ready_headline(version)),
                            ),
                    )
                    .child(
                        Button::new("update-dismiss-x")
                            .xsmall()
                            .ghost()
                            .rounded(RADIUS_CHIP)
                            .label("×")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.dismiss_current_update(cx);
                                this.focus_active_surface_default(window, cx);
                            })),
                    ),
            );

        let visible_notes = self.update_ui.visible_notes(notes);
        if !visible_notes.is_empty() {
            let mut notes_box = v_flex().id("update-notes-list").test_support().gap_0p5();
            for note in visible_notes {
                notes_box = notes_box.child(
                    div()
                        .text_xs()
                        .text_color(theme::text_secondary())
                        .child(format!("• {note}")),
                );
            }
            if UpdateUiState::has_more_notes(notes) {
                let toggle_label = if self.update_ui.notes_expanded {
                    "Show less"
                } else {
                    "Show more"
                };
                notes_box = notes_box.child(
                    Button::new("update-notes-toggle-btn")
                        .xsmall()
                        .ghost()
                        .label(toggle_label)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.update_ui.toggle_notes_expanded();
                            cx.notify();
                        })),
                );
            }
            card = card.child(notes_box);
        }

        card = card.child(
            h_flex()
                .items_center()
                .gap_1p5()
                .pt_0p5()
                .child(
                    Button::new("update-primary-btn")
                        .xsmall()
                        .primary()
                        .rounded(RADIUS_CHIP)
                        .label("Update & Restart")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.trigger_primary_update_action(cx);
                            this.focus_active_surface_default(window, cx);
                        })),
                )
                .child(
                    Button::new("update-later-btn")
                        .xsmall()
                        .ghost()
                        .rounded(RADIUS_CHIP)
                        .label("Later")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.dismiss_current_update(cx);
                            this.focus_active_surface_default(window, cx);
                        })),
                ),
        );

        card.into_any_element()
    }

    /// Sidebar update card rendered directly above `#gpu-status-card`.
    /// Uses `self.sidebar_state.is_shown()` and `self.settings.sidebar_mode` (M4-parallel safe).
    pub(crate) fn render_update_card(
        &self,
        in_settings: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let visible = self.update_ui.is_card_visible(
            if in_settings {
                SidebarMode::Open
            } else {
                self.settings.sidebar_mode
            },
            in_settings || self.sidebar_state.is_shown(),
            if in_settings {
                None
            } else {
                self.settings.dismissed_update_version.as_deref()
            },
        );
        if !visible {
            return None;
        }

        match &self.update_ui.state {
            UpdateState::Available(info) => {
                Some(self.render_ready_card_body(&info.version, &info.notes, cx))
            }
            UpdateState::ReadyToRestart(staged) => {
                Some(self.render_ready_card_body(&staged.version, &staged.notes, cx))
            }
            UpdateState::Downloading {
                version,
                bytes_done,
                bytes_total,
            } => {
                let progress = UpdateUiState::download_progress(*bytes_done, *bytes_total);
                let fill_frac = progress.map(|(f, _)| f).unwrap_or(0.25);
                let mut header = h_flex().justify_between().items_center().gap_1().child(
                    div()
                        .text_xs()
                        .text_color(theme::text_primary())
                        .child(UpdateUiState::downloading_headline(version)),
                );
                if let Some((_, pct)) = progress {
                    header = header.child(
                        div()
                            .text_xs()
                            .text_color(theme::text_muted())
                            .child(format!("{pct}%")),
                    );
                }
                let card = v_flex()
                    .id("update-card")
                    .test_support()
                    .w_full()
                    .px_3()
                    .py_2p5()
                    .gap_2()
                    .rounded(RADIUS_ROW)
                    .bg(theme::bg_surface())
                    .border_1()
                    .border_color(theme::border_subtle())
                    .child(header)
                    .child(
                        div()
                            .id("update-progress-track")
                            .test_support()
                            .w_full()
                            .h(px(4.0))
                            .rounded(RADIUS_CHIP)
                            .bg(theme::bg_hover())
                            .overflow_hidden()
                            .child(
                                div()
                                    .id("update-progress-fill")
                                    .h_full()
                                    .w(relative(fill_frac))
                                    .rounded(RADIUS_CHIP)
                                    .bg(theme::accent_blue()),
                            ),
                    );
                Some(card.into_any_element())
            }
            UpdateState::Verifying { version } => {
                let card = v_flex()
                    .id("update-card")
                    .test_support()
                    .w_full()
                    .px_3()
                    .py_2p5()
                    .gap_1()
                    .rounded(RADIUS_ROW)
                    .bg(theme::bg_surface())
                    .border_1()
                    .border_color(theme::border_subtle())
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::text_primary())
                            .child(UpdateUiState::verifying_headline(version)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::text_muted())
                            .child("Checking package integrity"),
                    );
                Some(card.into_any_element())
            }
            UpdateState::Error { message, retryable } => {
                let mut actions = h_flex().items_center().gap_1p5().pt_0p5();
                if *retryable {
                    actions = actions.child(
                        Button::new("update-retry-btn")
                            .xsmall()
                            .primary()
                            .rounded(RADIUS_CHIP)
                            .label("Retry")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.retry_update(cx);
                                this.focus_active_surface_default(window, cx);
                            })),
                    );
                }
                actions = actions.child(
                    Button::new("update-error-dismiss-btn")
                        .xsmall()
                        .ghost()
                        .rounded(RADIUS_CHIP)
                        .label("Dismiss")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.dismiss_current_update(cx);
                            this.focus_active_surface_default(window, cx);
                        })),
                );

                let card = v_flex()
                    .id("update-card")
                    .test_support()
                    .w_full()
                    .px_3()
                    .py_2p5()
                    .gap_2()
                    .rounded(RADIUS_ROW)
                    .bg(theme::bg_surface())
                    .border_1()
                    .border_color(theme::border_subtle())
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::accent_amber())
                                    .child("Update failed"),
                            )
                            .child(
                                Button::new("update-dismiss-x")
                                    .xsmall()
                                    .ghost()
                                    .rounded(RADIUS_CHIP)
                                    .label("×")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.dismiss_current_update(cx);
                                        this.focus_active_surface_default(window, cx);
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::text_secondary())
                            .child(UpdateUiState::error_detail(message)),
                    )
                    .child(actions);
                Some(card.into_any_element())
            }
            UpdateState::Idle
            | UpdateState::Checking
            | UpdateState::UpToDate
            | UpdateState::Applying { .. } => None,
        }
    }

    /// `Ctrl+K` commands contributed by the updater module.
    pub(crate) fn update_palette_commands(&self) -> Vec<PaletteEntry> {
        self.update_ui
            .palette_entries(multibot_proto::build_version())
    }

    /// Handles `Ctrl+K` updater command invocations.
    pub(crate) fn execute_update_palette_command(
        &mut self,
        cmd_id: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        match cmd_id {
            PALETTE_CMD_UPDATE_CHECK => {
                self.trigger_update_check(cx);
                true
            }
            PALETTE_CMD_UPDATE_ACTION => {
                self.trigger_primary_update_action(cx);
                true
            }
            PALETTE_CMD_UPDATE_RESTART => {
                self.update_ui.pending_action = Some(UpdateUiAction::RestartApp);
                self.dispatch_update_ui_action(UpdateUiAction::RestartApp, cx);
                self.notice = Some("Restarting Multi Bot...".to_string());
                cx.notify();
                true
            }
            PALETTE_CMD_UPDATE_CHANGELOG => {
                let notice = self
                    .update_ui
                    .format_changelog_notice(multibot_proto::build_version());
                self.notice = Some(notice);
                cx.notify();
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{AppContext, Entity, TestAppContext, WindowOptions, test::TestWindowExt};
    use multibot_updater::WindowsAssetInfo;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SETTINGS_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempSettingsGuard {
        dir: PathBuf,
        settings_path: PathBuf,
    }

    impl TempSettingsGuard {
        fn new(label: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "multibot-update-ui-{label}-{}-{}",
                std::process::id(),
                SETTINGS_TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let settings_path = dir.join("settings.json");
            Self { dir, settings_path }
        }
    }

    impl Drop for TempSettingsGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn sample_release(version: &str, notes: Vec<&str>) -> ReleaseInfo {
        ReleaseInfo {
            version: version.to_string(),
            tag: format!("v{version}"),
            published_at: "2026-11-26T20:00:00Z".to_string(),
            notes: notes.into_iter().map(String::from).collect(),
            asset: WindowsAssetInfo {
                asset_name: "multibot-windows-x64.zip".to_string(),
                url: format!(
                    "https://github.com/valentinyeo/multibot/releases/download/v{version}/multibot-windows-x64.zip"
                ),
                sha256: "a".repeat(64),
                size_bytes: 10_000_000,
                multibot_exe_sha256: "b".repeat(64),
                multibotd_linux_x86_64_sha256: "c".repeat(64),
            },
        }
    }

    fn sample_staged(version: &str, notes: Vec<&str>) -> StagedUpdate {
        StagedUpdate {
            version: version.to_string(),
            tag: format!("v{version}"),
            staging_dir: PathBuf::from("/tmp/staging"),
            multibot_exe_path: PathBuf::from("/tmp/staging/multibot.exe"),
            multibotd_linux_path: PathBuf::from("/tmp/staging/multibotd-linux-x86_64"),
            multibot_exe_sha256: "b".repeat(64),
            multibotd_linux_x86_64_sha256: "c".repeat(64),
            notes: notes.into_iter().map(String::from).collect(),
        }
    }

    fn open_test_view(cx: &mut TestAppContext) -> (gpui_kit::AnyWindowHandle, Entity<MultibotView>) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply_multibot_dark_theme(cx);
            crate::bind_multibot_keys(cx);
        });
        let rt = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, move |window, cx| {
                cx.new(|cx| MultibotView::new(rt, window, cx))
            })
            .unwrap()
        })
    }

    // ---------------------------------------------------------------------
    // Pure Unit Tests (7 tests)
    // ---------------------------------------------------------------------

    #[test]
    fn forbidden_duration_matcher_covers_all_regex_branches_without_false_positives() {
        let should_match = [
            "Restarts in 5 seconds",
            "Only 1 second",
            "Done in 3 secs",
            "About 2 sec",
            "Wait 2 minutes",
            "Under a minute",
            "Takes 1 min",
            "Around 5 mins",
            "Takes a moment",
            "It will take a bit",
            "Ready in a few moments",
            "Ready in a\x0bfew moments",
            "a\x0bfew",
            "ETA: soon",
            "eta unknown",
            "~5s restart",
            "~ 12 s",
            "~30S",
            "Downloads in ~2m",
            "Startup in 5min or 30s",
            "Finishes in 4 m",
        ];
        for s in should_match {
            assert!(
                contains_forbidden_duration_wording(s),
                "expected forbidden duration match on: {s:?}"
            );
        }

        let should_not_match = [
            "v0.1.5 is ready",
            "Update & Restart",
            "Downloading v0.1.5",
            "42%",
            "Verifying v0.1.5",
            "Checking package integrity",
            "Update failed",
            "Could not complete update.",
            "Retry",
            "Dismiss",
            "Later",
            "Show more",
            "Show less",
            "Update: Check for Updates",
            "Update: Download & Relaunch",
            "Update: Relaunch to v0.1.5",
            "Update: Up to date (v0.1.0)",
            "Restart Multi Bot",
            "Changelog",
            "Details and metadata", // contains "eta" inside word, NOT a word boundary
            "Minimal dark UI",      // contains "min" inside word, NOT a word boundary
            "Secondary actions",    // contains "sec" inside word, NOT a word boundary
            "~50% complete",        // not ~Ns
        ];
        for s in should_not_match {
            assert!(
                !contains_forbidden_duration_wording(s),
                "unexpected false positive on: {s:?}"
            );
        }
    }

    #[test]
    fn zero_time_or_duration_wording_across_all_states_palette_and_notices() {
        let all_states = vec![
            UpdateState::Idle,
            UpdateState::Checking,
            UpdateState::UpToDate,
            UpdateState::Available(sample_release(
                "0.1.5",
                vec!["Note 1", "Note 2", "Note 3", "Note 4"],
            )),
            UpdateState::Downloading {
                version: "0.1.5".into(),
                bytes_done: 420,
                bytes_total: Some(1000),
            },
            UpdateState::Downloading {
                version: "0.1.5".into(),
                bytes_done: 420,
                bytes_total: None,
            },
            UpdateState::Verifying {
                version: "0.1.5".into(),
            },
            UpdateState::ReadyToRestart(sample_staged("0.1.5", vec!["Note 1", "Note 2", "Note 3", "Note 4"])),
            UpdateState::Applying {
                version: "0.1.5".into(),
            },
            UpdateState::Error {
                message: String::new(),
                retryable: true,
            },
            UpdateState::Error {
                message: "Could not verify archive checksum.".into(),
                retryable: false,
            },
        ];

        for state in all_states {
            for expanded in [false, true] {
                let ui = UpdateUiState {
                    state: state.clone(),
                    notes_expanded: expanded,
                    ..Default::default()
                };
                for s in ui.formatted_card_strings() {
                    assert!(
                        !contains_forbidden_duration_wording(&s),
                        "forbidden duration wording in card copy: {s:?}"
                    );
                }
                for entry in ui.palette_entries("0.1.0") {
                    assert!(
                        !contains_forbidden_duration_wording(&entry.label),
                        "forbidden duration wording in palette label: {:?}",
                        entry.label
                    );
                    assert!(!contains_forbidden_duration_wording(entry.category));
                }
                let changelog = ui.format_changelog_notice("0.1.0");
                assert!(
                    !contains_forbidden_duration_wording(&changelog),
                    "forbidden duration wording in changelog notice: {changelog:?}"
                );
            }
        }

        // Also check all static notice templates used by MultibotView handlers.
        for notice in [
            "Checking for updates...",
            "Downloading Multibot v0.1.5...",
            "Restarting to apply v0.1.5...",
            "Multibot v0.1.0 is up to date.",
            "Restarting Multi Bot...",
        ] {
            assert!(
                !contains_forbidden_duration_wording(notice),
                "forbidden duration wording in notice: {notice:?}"
            );
        }
    }

    #[test]
    fn card_visibility_matrix_sidebar_modes_and_semver_dismissal() {
        let mut ui = UpdateUiState::default();
        assert!(!ui.is_card_visible(SidebarMode::Open, true, None));

        ui.set_update_state(UpdateState::Checking);
        assert!(!ui.is_card_visible(SidebarMode::Open, true, None));

        ui.set_update_state(UpdateState::UpToDate);
        assert!(!ui.is_card_visible(SidebarMode::Open, true, None));

        ui.set_update_state(UpdateState::Available(sample_release("0.1.5", vec!["Fast"])));
        assert!(ui.is_card_visible(SidebarMode::Open, true, None));
        assert!(!ui.is_card_visible(SidebarMode::Collapsed, true, None));
        assert!(!ui.is_card_visible(SidebarMode::Hidden, true, None));
        assert!(!ui.is_card_visible(SidebarMode::Open, false, None));

        // Dismissed for 0.1.5 hides 0.1.5 and older 0.1.4, but re-shows for strictly newer 0.1.6.
        assert!(!ui.is_card_visible(SidebarMode::Open, true, Some("0.1.5")));
        assert!(!ui.is_card_visible(SidebarMode::Open, true, Some("0.1.6")));
        assert!(ui.is_card_visible(SidebarMode::Open, true, Some("0.1.4")));

        ui.set_update_state(UpdateState::Available(sample_release("0.1.6", vec!["Newer"])));
        assert!(ui.is_card_visible(SidebarMode::Open, true, Some("0.1.5")));

        // Downloading / Verifying stay visible when sidebar is Open.
        ui.set_update_state(UpdateState::Downloading {
            version: "0.1.6".into(),
            bytes_done: 10,
            bytes_total: Some(100),
        });
        assert!(ui.is_card_visible(SidebarMode::Open, true, Some("0.1.6")));

        ui.set_update_state(UpdateState::Verifying {
            version: "0.1.6".into(),
        });
        assert!(ui.is_card_visible(SidebarMode::Open, true, Some("0.1.6")));

        ui.set_update_state(UpdateState::Applying {
            version: "0.1.6".into(),
        });
        assert!(!ui.is_card_visible(SidebarMode::Open, true, None));
    }

    #[test]
    fn notes_collapse_at_three_and_toggle_expand() {
        let notes: Vec<String> = (1..=5).map(|i| format!("Improvement {i}")).collect();
        let mut ui = UpdateUiState::default();
        assert!(UpdateUiState::has_more_notes(&notes));
        assert_eq!(ui.visible_notes(&notes).len(), MAX_COLLAPSED_NOTES);

        ui.toggle_notes_expanded();
        assert_eq!(ui.visible_notes(&notes).len(), 5);

        ui.toggle_notes_expanded();
        assert_eq!(ui.visible_notes(&notes).len(), MAX_COLLAPSED_NOTES);

        let short: Vec<String> = vec!["One".into(), "Two".into(), "Three".into()];
        assert!(!UpdateUiState::has_more_notes(&short));
        assert_eq!(ui.visible_notes(&short).len(), 3);
    }

    #[test]
    fn set_update_state_suppresses_redundant_redraws() {
        let mut ui = UpdateUiState::default();
        assert!(!ui.set_update_state(UpdateState::Idle));
        assert!(ui.set_update_state(UpdateState::UpToDate));
        assert!(!ui.set_update_state(UpdateState::UpToDate));

        let rel = sample_release("0.2.0", vec!["Feature A"]);
        assert!(ui.set_update_state(UpdateState::Available(rel.clone())));
        assert!(!ui.set_update_state(UpdateState::Available(rel)));
    }

    #[test]
    fn download_progress_clamps_and_handles_edge_cases() {
        assert_eq!(UpdateUiState::download_progress(0, None), None);
        assert_eq!(UpdateUiState::download_progress(50, Some(0)), None);
        assert_eq!(UpdateUiState::download_progress(0, Some(100)), Some((0.0, 0)));
        assert_eq!(
            UpdateUiState::download_progress(50, Some(100)),
            Some((0.5, 50))
        );
        assert_eq!(
            UpdateUiState::download_progress(250, Some(100)),
            Some((1.0, 100))
        );
        // Large u64 values do not overflow u128 multiplication.
        assert_eq!(
            UpdateUiState::download_progress(u64::MAX / 2, Some(u64::MAX)),
            Some((0.5, 49))
        );
    }

    #[test]
    fn palette_commands_reflect_every_update_state() {
        let mut ui = UpdateUiState::default();
        assert_eq!(ui.palette_entries("0.1.0").len(), 4);
        assert_eq!(ui.palette_entries("0.1.0")[1].label, "Update: Up to date (v0.1.0)");

        ui.set_update_state(UpdateState::Available(sample_release("0.2.0", vec![])));
        assert_eq!(ui.palette_entries("0.1.0")[1].label, "Update: Download & Relaunch");

        ui.set_update_state(UpdateState::ReadyToRestart(sample_staged("0.2.0", vec![])));
        assert_eq!(ui.palette_entries("0.1.0")[1].label, "Update: Relaunch to v0.2.0");

        ui.set_update_state(UpdateState::Downloading {
            version: "0.2.0".into(),
            bytes_done: 75,
            bytes_total: Some(100),
        });
        assert_eq!(ui.palette_entries("0.1.0")[1].label, "Update: Downloading v0.2.0 (75%)");

        ui.set_update_state(UpdateState::Verifying {
            version: "0.2.0".into(),
        });
        assert_eq!(ui.palette_entries("0.1.0")[1].label, "Update: Verifying v0.2.0");

        ui.set_update_state(UpdateState::Error {
            message: "Network error".into(),
            retryable: true,
        });
        assert_eq!(ui.palette_entries("0.1.0")[1].label, "Update: Retry Update");
    }

    // ---------------------------------------------------------------------
    // Headless GPUI Integration Tests (4 tests — M4-parallel safe!)
    // ---------------------------------------------------------------------

    #[gpui_kit::test]
    fn headless_update_card_renders_toggles_notes_and_respects_sidebar_mode(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view(cx);
        let guard = TempSettingsGuard::new("card-notes");

        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.update_ui.settings_path_override = Some(guard.settings_path.clone());
                v.settings.sidebar_mode = SidebarMode::Open;
                v.settings.dismissed_update_version = None;
                v.set_update_state(
                    UpdateState::Available(sample_release(
                        "0.1.5",
                        vec!["Note 1", "Note 2", "Note 3", "Note 4", "Note 5"],
                    )),
                    cx,
                );
            });
            window.render_frame(cx);

            assert!(window.try_find("update-card").is_some());
            assert!(window.try_find("update-primary-btn").is_some());
            assert!(window.try_find("update-later-btn").is_some());
            assert!(window.try_find("update-dismiss-x").is_some());
            assert!(window.try_find("update-notes-toggle-btn").is_some());

            // Expand notes via live click.
            window.click("update-notes-toggle-btn", cx);
            window.render_frame(cx);
            assert!(view.read(cx).update_ui.notes_expanded);

            // Collapse notes via live click.
            window.click("update-notes-toggle-btn", cx);
            window.render_frame(cx);
            assert!(!view.read(cx).update_ui.notes_expanded);

            // Collapsed sidebar hides card.
            view.update(cx, |v, cx| {
                v.settings.sidebar_mode = SidebarMode::Collapsed;
                cx.notify();
            });
            window.render_frame(cx);
            assert!(window.try_find("update-card").is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn headless_later_dismiss_persists_to_disk_until_strictly_newer_semver(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view(cx);
        let guard = TempSettingsGuard::new("later-dismiss");

        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.update_ui.settings_path_override = Some(guard.settings_path.clone());
                v.settings.sidebar_mode = SidebarMode::Open;
                v.settings.dismissed_update_version = None;
                v.set_update_state(
                    UpdateState::Available(sample_release("0.1.5", vec!["Smooth updates"])),
                    cx,
                );
            });
            window.render_frame(cx);
            assert!(window.try_find("update-card").is_some());

            // Click "Later" -> hides card and persists dismissed_update_version = "0.1.5".
            window.click("update-later-btn", cx);
            window.render_frame(cx);
            assert!(window.try_find("update-card").is_none());

            let reloaded = MultibotSettings::load_from(&guard.settings_path).unwrap();
            assert_eq!(reloaded.dismissed_update_version.as_deref(), Some("0.1.5"));

            // Older version stays hidden.
            view.update(cx, |v, cx| {
                v.set_update_state(
                    UpdateState::Available(sample_release("0.1.4", vec!["Old"])),
                    cx,
                );
            });
            window.render_frame(cx);
            assert!(window.try_find("update-card").is_none());

            // Strictly newer SemVer reappears!
            view.update(cx, |v, cx| {
                v.set_update_state(
                    UpdateState::Available(sample_release("0.1.6", vec!["Brand new"])),
                    cx,
                );
            });
            window.render_frame(cx);
            assert!(window.try_find("update-card").is_some());

            // Clicking the × button also dismisses and persists "0.1.6".
            window.click("update-dismiss-x", cx);
            window.render_frame(cx);
            assert!(window.try_find("update-card").is_none());
            let reloaded2 = MultibotSettings::load_from(&guard.settings_path).unwrap();
            assert_eq!(reloaded2.dismissed_update_version.as_deref(), Some("0.1.6"));
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn headless_primary_cta_progress_verifying_ready_and_error_retry(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view(cx);
        let guard = TempSettingsGuard::new("cta-lifecycle");

        cx.update_window(handle, |_, window, cx| {
            let rel = sample_release("0.2.0", vec!["Auto-updater ready"]);
            view.update(cx, |v, cx| {
                v.update_ui.settings_path_override = Some(guard.settings_path.clone());
                v.settings.sidebar_mode = SidebarMode::Open;
                v.settings.dismissed_update_version = None;
                v.set_update_state(UpdateState::Available(rel.clone()), cx);
            });
            window.render_frame(cx);

            // Click "Update & Restart" -> Downloading + progress track visible.
            window.click("update-primary-btn", cx);
            window.render_frame(cx);
            assert!(matches!(
                view.read(cx).update_ui.state,
                UpdateState::Downloading { .. }
            ));
            assert_eq!(
                view.read(cx).update_ui.pending_action,
                Some(UpdateUiAction::DownloadAndRestart(rel))
            );
            assert!(window.try_find("update-progress-track").is_some());

            // Verifying state renders card cleanly.
            view.update(cx, |v, cx| {
                v.set_update_state(
                    UpdateState::Verifying {
                        version: "0.2.0".into(),
                    },
                    cx,
                );
            });
            window.render_frame(cx);
            assert!(window.try_find("update-card").is_some());

            // ReadyToRestart -> clicking primary CTA transitions to Applying and hides card.
            let staged = sample_staged("0.2.0", vec!["Ready"]);
            view.update(cx, |v, cx| {
                v.set_update_state(UpdateState::ReadyToRestart(staged.clone()), cx);
            });
            window.render_frame(cx);
            window.click("update-primary-btn", cx);
            window.render_frame(cx);
            assert!(matches!(
                view.read(cx).update_ui.state,
                UpdateState::Applying { .. }
            ));
            assert_eq!(
                view.read(cx).update_ui.pending_action,
                Some(UpdateUiAction::ApplyAndRestart(staged))
            );
            assert!(window.try_find("update-card").is_none());

            // Retryable error shows Retry button and clicking it retries download.
            view.update(cx, |v, cx| {
                v.set_update_state(
                    UpdateState::Error {
                        message: "Connection interrupted.".into(),
                        retryable: true,
                    },
                    cx,
                );
            });
            window.render_frame(cx);
            assert!(window.try_find("update-retry-btn").is_some());
            window.click("update-retry-btn", cx);
            window.render_frame(cx);
            assert!(matches!(
                view.read(cx).update_ui.state,
                UpdateState::Downloading { .. }
            ));

            // Non-retryable error omits Retry button, Dismiss button hides card.
            view.update(cx, |v, cx| {
                v.set_update_state(
                    UpdateState::Error {
                        message: "Install directory is read-only.".into(),
                        retryable: false,
                    },
                    cx,
                );
            });
            window.render_frame(cx);
            assert!(window.try_find("update-retry-btn").is_none());
            assert!(window.try_find("update-error-dismiss-btn").is_some());
            window.click("update-error-dismiss-btn", cx);
            window.render_frame(cx);
            assert!(window.try_find("update-card").is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn headless_palette_update_commands_execute_directly(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view(cx);
        let guard = TempSettingsGuard::new("palette-cmds");

        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.update_ui.settings_path_override = Some(guard.settings_path.clone());
                v.set_update_state(
                    UpdateState::Available(sample_release("0.3.0", vec!["Instant dark card"])),
                    cx,
                );

                // Verify all 4 Ctrl+K commands are present and free of duration wording.
                let cmds = v.update_palette_commands();
                assert_eq!(cmds.len(), 4);
                let ids: Vec<&str> = cmds.iter().map(|c| c.id.as_str()).collect();
                assert_eq!(
                    ids,
                    vec![
                        PALETTE_CMD_UPDATE_CHECK,
                        PALETTE_CMD_UPDATE_ACTION,
                        PALETTE_CMD_UPDATE_RESTART,
                        PALETTE_CMD_UPDATE_CHANGELOG,
                    ]
                );
                for c in &cmds {
                    assert!(!contains_forbidden_duration_wording(&c.label));
                }

                // 1. Changelog command sets notice with release notes.
                assert!(v.execute_update_palette_command(PALETTE_CMD_UPDATE_CHANGELOG, cx));
                let changelog_notice = v.notice.clone().unwrap();
                assert!(changelog_notice.contains("Changelog v0.3.0"));
                assert!(changelog_notice.contains("Instant dark card"));
                assert!(!contains_forbidden_duration_wording(&changelog_notice));

                // 2. Check command triggers CheckForUpdates.
                assert!(v.execute_update_palette_command(PALETTE_CMD_UPDATE_CHECK, cx));
                assert_eq!(v.update_ui.state, UpdateState::Checking);
                assert_eq!(
                    v.update_ui.pending_action,
                    Some(UpdateUiAction::CheckForUpdates)
                );

                // 3. Restart command triggers RestartApp.
                assert!(v.execute_update_palette_command(PALETTE_CMD_UPDATE_RESTART, cx));
                assert_eq!(v.update_ui.pending_action, Some(UpdateUiAction::RestartApp));
                assert_eq!(v.notice.as_deref(), Some("Restarting Multi Bot..."));

                // 4. Unknown command returns false.
                assert!(!v.execute_update_palette_command("pal-cmd-unknown", cx));
            });
            window.render_frame(cx);
        })
        .unwrap();
    }

    #[test]
    fn defense_in_depth_strips_forbidden_duration_notes_from_card_and_changelog() {
        let injected = sample_release(
            "0.4.0",
            vec![
                "Update takes 5 seconds",
                "Reloads in ~3s",
                "Downloads in ~2m",
                "Startup in 5min or 30s",
                "Ready in a\x0bfew moments",
                // NBSP duration bypasses
                "Installs in a\u{00A0}few clicks",
                "Done in 5\u{00A0}s",
                // Bidi overrides & isolates
                "Bidi RLO \u{202E} exe.tobitlum",
                "Bidi LRE \u{202A} spoofed",
                "Bidi LRI \u{2066} spoofed \u{2069}",
                "Bidi RLI \u{2067} spoofed \u{2069}",
                "Bidi FSI \u{2068} spoofed \u{2069}",
                // Bidi-split duration bypasses
                "Installs in a\u{202E}few clicks",
                "Done in 5\u{202E}s",
                // C1 controls (embedded and leading, checked before trim)
                "C1 next-line \u{0085} spoofed",
                "\u{0085}Leading C1",
                // ANSI ESC
                "ANSI \x1b[31mred text",
                // Unicode line & paragraph separators (embedded and leading)
                "Line sep \u{2028} injected",
                "Para sep \u{2029} injected",
                "\u{2028}Leading line sep",
                "\u{2029}Leading para sep",
                // Embedded & leading newlines
                "Multi\nline injection",
                "\nLeading newline",
                // ZWSP / BOM duration bypasses
                "Installs in a\u{200B}few clicks",
                "Done in 5\u{200B}s",
                "Ready in a\u{FEFF}few clicks",
                "Done in 5\u{FEFF}s",
                // Clean plain-English notes (including compound ZWJ emoji) that MUST survive
                "Clean dark sidebar",
                "Team sync 👩‍💻 & family workspaces 👨‍👩‍👧",
            ],
        );
        let mut ui = UpdateUiState::default();
        ui.set_update_state(UpdateState::Available(injected));

        let card_lines = ui.formatted_card_strings();
        assert!(card_lines.iter().any(|s| s == "• Clean dark sidebar"));
        assert!(
            card_lines
                .iter()
                .any(|s| s == "• Team sync 👩‍💻 & family workspaces 👨‍👩‍👧")
        );
        for s in &card_lines {
            assert!(
                !contains_forbidden_duration_wording(s),
                "forbidden duration wording leaked into card: {s:?}"
            );
            assert!(
                !has_forbidden_control_or_bidi(s),
                "forbidden control/bidi char leaked into card: {s:?}"
            );
        }
        assert!(!card_lines.iter().any(|s| s == "Show more"));

        let changelog = ui.format_changelog_notice("0.1.0");
        assert_eq!(
            changelog,
            "Changelog v0.4.0: Clean dark sidebar · Team sync 👩‍💻 & family workspaces 👨‍👩‍👧"
        );
        assert!(!contains_forbidden_duration_wording(&changelog));
        assert!(!has_forbidden_control_or_bidi(&changelog));

        // If every injected note is forbidden, changelog falls back cleanly and no bullets render.
        let all_bad = sample_staged(
            "0.4.0",
            vec![
                "Update takes 5 seconds",
                "ETA: 2 minutes",
                "\u{0085}Leading C1",
                "Bidi RLO \u{202E} spoofed",
                "Done in 5\u{200B}s",
                "Ready in a\u{FEFF}few clicks",
            ],
        );
        ui.set_update_state(UpdateState::ReadyToRestart(all_bad));
        assert!(ui.visible_notes(&["Update takes 5 seconds".into()]).is_empty());
        assert!(
            UpdateUiState::safe_notes(&[
                "\u{0085}Leading C1".into(),
                "Installs in a\u{202E}few clicks".into(),
                "Done in 5\u{200B}s".into(),
                "Done in 5\u{FEFF}s".into(),
            ])
            .is_empty()
        );
        for s in ui.formatted_card_strings() {
            assert!(!contains_forbidden_duration_wording(&s));
            assert!(!has_forbidden_control_or_bidi(&s));
        }
        assert_eq!(
            ui.format_changelog_notice("0.1.0"),
            "Multibot v0.1.0 — no release notes loaded."
        );
    }

    #[gpui_kit::test]
    fn headless_primary_action_clears_v_prefixed_and_padded_dismissed_version(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view(cx);
        let guard = TempSettingsGuard::new("dismiss-clear-v");

        for dismissed_raw in ["v0.1.5", " 0.1.5 ", "v0.1.6"] {
            cx.update_window(handle, |_, window, cx| {
                view.update(cx, |v, cx| {
                    v.update_ui.settings_path_override = Some(guard.settings_path.clone());
                    v.settings.sidebar_mode = SidebarMode::Open;
                    v.settings.dismissed_update_version = Some(dismissed_raw.to_string());
                    v.persist_update_settings(cx);
                    v.set_update_state(
                        UpdateState::Available(sample_release("0.1.5", vec!["Faster search"])),
                        cx,
                    );
                });
                window.render_frame(cx);
                // Card starts hidden while version is dismissed.
                assert!(
                    window.try_find("update-card").is_none(),
                    "expected card hidden for dismissed={dismissed_raw:?}"
                );

                // User explicitly triggers update via Ctrl+K primary action -> clears dismissal!
                view.update(cx, |v, cx| {
                    v.trigger_primary_update_action(cx);
                });
                window.render_frame(cx);
                assert_eq!(
                    view.read(cx).settings.dismissed_update_version,
                    None,
                    "expected dismissed_update_version cleared for {dismissed_raw:?}"
                );
                let reloaded = MultibotSettings::load_from(&guard.settings_path).unwrap();
                assert_eq!(reloaded.dismissed_update_version, None);

                // Once download finishes -> ReadyToRestart("0.1.5") card MUST be visible!
                view.update(cx, |v, cx| {
                    v.set_update_state(
                        UpdateState::ReadyToRestart(sample_staged("0.1.5", vec!["Faster search"])),
                        cx,
                    );
                });
                window.render_frame(cx);
                assert!(
                    window.try_find("update-card").is_some(),
                    "expected ReadyToRestart card visible after clearing {dismissed_raw:?}"
                );
            })
            .unwrap();
        }
    }

    #[gpui_kit::test]
    fn headless_trigger_update_check_ignores_in_flight_states(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view(cx);

        cx.update_window(handle, |_, window, cx| {
            // Downloading + update_in_flight = true -> trigger_update_check must be a no-op
            view.update(cx, |v, cx| {
                v.settings.sidebar_mode = SidebarMode::Open;
                v.update_in_flight = true;
                v.update_ui.pending_action = None;
                v.set_update_state(
                    UpdateState::Downloading {
                        version: "0.2.0".into(),
                        bytes_done: 512,
                        bytes_total: Some(1024),
                    },
                    cx,
                );
            });
            window.render_frame(cx);
            assert!(window.try_find("update-card").is_some());

            view.update(cx, |v, cx| {
                v.trigger_update_check(cx);
            });
            window.render_frame(cx);
            assert!(
                matches!(
                    view.read(cx).update_ui.state,
                    UpdateState::Downloading { bytes_done: 512, .. }
                ),
                "trigger_update_check must NOT overwrite Downloading state"
            );
            assert_eq!(
                view.read(cx).update_ui.pending_action,
                None,
                "trigger_update_check must NOT queue a duplicate CheckForUpdates while Downloading"
            );
            assert!(
                window.try_find("update-card").is_some(),
                "#update-card must stay visible while Downloading"
            );

            // Verifying, Applying, and Checking states (even with update_in_flight = false) must also be guarded
            for active_state in [
                UpdateState::Checking,
                UpdateState::Verifying {
                    version: "0.2.0".into(),
                },
                UpdateState::Applying {
                    version: "0.2.0".into(),
                },
            ] {
                view.update(cx, |v, cx| {
                    v.update_in_flight = false;
                    v.update_ui.pending_action = None;
                    v.update_ui.state = active_state.clone();
                    v.trigger_update_check(cx);
                });
                assert_eq!(
                    view.read(cx).update_ui.state,
                    active_state,
                    "trigger_update_check must NOT overwrite {active_state:?}"
                );
                assert_eq!(view.read(cx).update_ui.pending_action, None);
            }
        })
        .unwrap();
    }
}
