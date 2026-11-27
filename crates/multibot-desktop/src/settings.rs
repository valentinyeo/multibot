use crate::app::MultibotView;
use crate::theme::{self, RADIUS_BUBBLE};
use gpui_kit::base::FocusTrapElement;
use gpui_kit::{
    Context, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, TestSupportExt, Window,
    component::{
        Selectable, Sizable,
        button::{Button, ButtonVariants},
        h_flex,
        input::Input,
        v_flex,
    },
    div,
    prelude::FluentBuilder,
    px,
};
use multibot_core::zigshel_import::{import_zigshel_bundle_readonly, merge_zigshel_bundle};
use multibot_core::{MultibotSettings, SshHostConfig};
use multibot_gpu::GpuPreference;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum SettingsTab {
    #[default]
    General,
    Hosts,
    Workspaces,
    Gpu,
    Shortcuts,
    Updates,
}

impl SettingsTab {
    const ALL: [Self; 6] = [
        Self::General,
        Self::Hosts,
        Self::Workspaces,
        Self::Gpu,
        Self::Shortcuts,
        Self::Updates,
    ];

    fn id(self) -> &'static str {
        match self {
            Self::General => "settings-tab-general",
            Self::Hosts => "settings-tab-hosts",
            Self::Workspaces => "settings-tab-workspaces",
            Self::Gpu => "settings-tab-gpu",
            Self::Shortcuts => "settings-tab-shortcuts",
            Self::Updates => "settings-tab-updates",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Hosts => "Hosts",
            Self::Workspaces => "Workspaces",
            Self::Gpu => "GPU & Performance",
            Self::Shortcuts => "Shortcuts",
            Self::Updates => "Updates",
        }
    }

    fn subtitle(self) -> &'static str {
        match self {
            Self::General => "Composer preferences and queued message delivery.",
            Self::Hosts => "Connect locally or over SSH. Your legacy app stays untouched.",
            Self::Workspaces => "Organize sessions and switch your workspace.",
            Self::Gpu => "Keep rendering efficient. Auto never selects NVIDIA.",
            Self::Shortcuts => "The complete Multibot keyboard reference.",
            Self::Updates => "Windows releases arrive here automatically.",
        }
    }
}

impl MultibotView {
    pub(crate) fn active_gpu_summary(&self) -> (String, bool) {
        if let Some(ref gpu) = self.gpu_report.active {
            let safe = !gpu.is_nvidia();
            (gpu.badge_label(), safe)
        } else {
            #[cfg(target_os = "windows")]
            {
                ("DXGI report pending (relaunch to verify)".into(), false)
            }
            #[cfg(not(target_os = "windows"))]
            {
                (
                    format!(
                        "Non-Windows host (policy: {})",
                        self.settings.gpu.env_mode_value()
                    ),
                    true,
                )
            }
        }
    }

    pub(crate) fn set_gpu_preference(&mut self, pref: GpuPreference, cx: &mut Context<Self>) {
        self.settings.gpu = pref;
        self.sync_settings_to_shared(cx);
        if self.is_disk_persistence_enabled(cx) {
            match self.settings.save() {
                Ok(()) => {
                    self.notice = Some(format!(
                        "Saved GPU policy to {} — restart Multibot to apply.",
                        MultibotSettings::settings_path().display()
                    ));
                }
                Err(err) => {
                    self.notice = Some(format!("Failed to save settings: {err}"));
                }
            }
        } else {
            self.notice = Some("Updated GPU policy — restart Multibot to apply.".into());
        }
        cx.notify();
    }

    pub(crate) fn add_ssh_host_from_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let raw = self.host_input.read(cx).value().to_string();
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return;
        }
        let (user, host_port) = match trimmed.split_once('@') {
            Some((u, hp)) => (u.trim().to_string(), hp.trim()),
            None => (String::new(), trimmed),
        };
        let (hostname, port) = match host_port.rsplit_once(':') {
            Some((h, p)) => (h.trim().to_string(), p.trim().parse::<u16>().unwrap_or(22)),
            None => (host_port.to_string(), 22),
        };
        if hostname.is_empty() {
            return;
        }
        let host = SshHostConfig {
            name: hostname.clone(),
            hostname,
            user,
            port,
            identity_file: None,
        };
        self.settings.merge_hosts(vec![host]);
        self.sync_settings_to_shared(cx);
        if self.is_disk_persistence_enabled(cx) {
            let _ = self.settings.save();
            self.auto_connect_configured_hosts(cx);
        }
        self.host_input.update(cx, |state, cx| {
            state.set_value("", window, cx);
        });
        self.notice = Some("SSH host added.".into());
        cx.notify();
    }

    pub(crate) fn import_from_zigshel(&mut self, cx: &mut Context<Self>) {
        self.import_from_zigshel_dir(None, cx);
    }

    pub(crate) fn import_from_zigshel_dir(
        &mut self,
        dir_override: Option<&std::path::Path>,
        cx: &mut Context<Self>,
    ) {
        let bundle = import_zigshel_bundle_readonly(dir_override);
        let hosts_found = bundle.hosts.len();
        let ws_found = bundle.workspaces.len();
        let fav_found = bundle.favorites.len();
        let sessions_found = bundle.sessions.len();

        let hosts_added = self.settings.merge_hosts(bundle.hosts.clone());
        let merge_stats = merge_zigshel_bundle(&mut self.sidebar_state.workspace_store, &bundle);
        let live_keys: Vec<_> = self.session_inputs().into_iter().map(|s| s.key).collect();
        self.sidebar_state
            .workspace_store
            .reconcile_live_session_keys(&live_keys);

        self.sync_settings_to_shared(cx);
        self.sync_workspace_to_shared(cx);
        if self.is_disk_persistence_enabled(cx) {
            let _ = self.settings.save();
            self.auto_connect_configured_hosts(cx);
        }
        self.persist_workspace_store_if_enabled();

        self.notice = Some(if hosts_found == 0 && ws_found == 0 && fav_found == 0 && sessions_found == 0 {
            "No SSH hosts, workspaces, sessions, or favorites found in %APPDATA%\\ZigShell (read-only check)."
                .into()
        } else {
            format!(
                "Imported {hosts_added} host(s), {} workspace(s), {} session assignment(s), and {} favorite(s) from ZigShell (read-only).",
                merge_stats.workspaces_added, merge_stats.sessions_assigned, merge_stats.favorites_added
            )
        });
        cx.notify();
    }

    pub(crate) fn disconnect_host(&mut self, cx: &mut Context<Self>) {
        if !self.host_statuses.is_empty() {
            let host = self.active_session_host().to_string();
            Self::disconnect_host_on_app(&host, cx);
            self.sync_from_shared_store(cx);
            return;
        }
        if cx.has_global::<crate::state::SharedAppStore>() {
            let store = cx.global_mut::<crate::state::SharedAppStore>();
            store.connection_gen = store.connection_gen.wrapping_add(1);
            store.connection_shutdown_tx = None;
            store.connected_target = None;
            store.connected_host = None;
            store.daemon_tx = None;
            store.daemon_version = None;
            store.daemon_binary_hash = None;
            store.sessions.clear();
            store.connection_status = "Disconnected".into();
            store.bump_revision();
            self.sync_from_shared_store(cx);
        } else {
            self.daemon_tx = None;
            self.connected_host = None;
            self.connection_status = "Disconnected".into();
        }
        cx.notify();
    }

    pub(crate) fn render_settings_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut discovered_col = v_flex().gap_2();

        if self.gpu_report.discovered.is_empty() {
            discovered_col = discovered_col.child(
                div()
                    .text_xs()
                    .text_color(theme::text_muted())
                    .child("Windows DXGI GPU list appears here automatically when running on Windows."),
            );
        } else {
            for (idx, gpu) in self.gpu_report.discovered.iter().enumerate() {
                let luid = gpu.luid.clone();
                let is_selected = matches!(
                    &self.settings.gpu,
                    GpuPreference::SpecificLuid { luid: saved } if saved.eq_ignore_ascii_case(&luid)
                );
                let tag = if gpu.is_nvidia() {
                    "NVIDIA — blocked by default"
                } else if gpu.is_software {
                    "WARP Software Fallback"
                } else {
                    "Integrated / Non-NVIDIA"
                };
                let label = format!(
                    "{} · {} · {} MB VRAM · LUID {} [{}]",
                    gpu.name,
                    gpu.vendor_label(),
                    gpu.dedicated_vram_mb,
                    gpu.luid,
                    tag
                );
                let btn = Button::new(SharedString::from(format!("pick-gpu-{idx}")))
                    .small()
                    .label(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_gpu_preference(
                            GpuPreference::SpecificLuid { luid: luid.clone() },
                            cx,
                        );
                    }));
                discovered_col = discovered_col.child(btn.selected(is_selected));
            }
        }

        let mut content = v_flex().gap_5().w_full();
        match self.settings_tab {
            SettingsTab::General => {
                content = content.child(v_flex().gap_3().p_4().rounded(RADIUS_BUBBLE)
                    .bg(theme::bg_surface()).border_1().border_color(theme::border_subtle())
                    .child(div().text_sm().child("Input box"))
                    .child(div().text_xs().text_color(theme::text_secondary()).child("Choose which key sends a message."))
                    .child(h_flex().gap_2()
                        .child(Button::new("send-key-enter").label("Enter").selected(!self.settings.composer_ctrl_enter)
                            .on_click(cx.listener(|this, _, _, cx| { this.settings.composer_ctrl_enter = false; this.save_general_preferences(cx); })))
                        .child(Button::new("send-key-ctrl-enter").label("Ctrl+Enter").selected(self.settings.composer_ctrl_enter)
                            .on_click(cx.listener(|this, _, _, cx| { this.settings.composer_ctrl_enter = true; this.save_general_preferences(cx); }))))
                    .child(div().text_sm().child("Queued messages"))
                    .child(h_flex().gap_2()
                        .child(Button::new("queue-messages").label("Queue").selected(self.settings.queue_while_working)
                            .on_click(cx.listener(|this, _, _, cx| { this.settings.queue_while_working = true; this.save_general_preferences(cx); })))
                        .child(Button::new("send-messages-immediately").label("Send immediately").selected(!self.settings.queue_while_working)
                            .on_click(cx.listener(|this, _, _, cx| { this.settings.queue_while_working = false; this.save_general_preferences(cx); }))))
                    .child(div().text_xs().text_color(theme::text_secondary())
                        .child("Queue waits until the agent finishes. Send immediately starts another turn. Offline messages always stay in the outbox until reconnecting.")));
            }
            SettingsTab::Hosts => {
                let mut hosts = v_flex().gap_3().p_4().rounded(RADIUS_BUBBLE)
                    .bg(theme::bg_surface()).border_1().border_color(theme::border_subtle())
                    .child(div().text_xs().text_color(theme::text_secondary()).child(self.connection_status.clone()))
                    .child(h_flex().justify_between().items_center().gap_2().child("Local")
                        .child(Button::new("connect-local").small().label("Connect")
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(bin) = Self::find_local_daemon() {
                                    this.connect_to_target("localhost".into(), multibot_core::DaemonTarget::LocalBinary(bin), cx);
                                } else { this.notice = Some("No local multibotd found. Add a remote SSH host below.".into()); cx.notify(); }
                            }))));
                if self.settings.ssh_hosts.len() > 1 || self.host_statuses.len() > 1 {
                    hosts = hosts.child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("reconnect-all-hosts")
                                    .small()
                                    .label("Reconnect all")
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.reconnect_all_hosts(cx)),
                                    ),
                            )
                            .child(
                                Button::new("disconnect-all-hosts")
                                    .small()
                                    .label("Disconnect all")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        Self::disconnect_all_hosts(cx);
                                        this.sync_from_shared_store(cx);
                                    })),
                            ),
                    );
                }
                for (idx, host) in self.settings.ssh_hosts.iter().enumerate() {
                    let host = host.clone();
                    let snapshot = self
                        .host_statuses
                        .iter()
                        .find(|s| s.host_label == host.name);
                    let active = snapshot.is_some_and(|s| s.connected || s.status.starts_with("Connecting to "));
                    let status = snapshot
                        .map(|s| {
                            if s.connected {
                                format!(
                                    "Connected · v{} · {} sessions",
                                    s.daemon_version.as_deref().unwrap_or("?"),
                                    s.session_count
                                )
                            } else {
                                s.status.clone()
                            }
                        })
                        .unwrap_or_else(|| "Offline".into());
                    hosts = hosts.child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .gap_2()
                            .child(
                                v_flex().gap_1().child(host.name.clone()).child(
                                    div()
                                        .text_xs()
                                        .text_color(theme::text_muted())
                                        .child(format!("{} · {status}", host.ssh_target())),
                                ),
                            )
                            .child(
                                Button::new(SharedString::from(format!("host-{idx}")))
                                    .small()
                                    .label(if active { "Disconnect" } else { "Connect" })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if active {
                                            Self::disconnect_host_on_app(&host.name, cx);
                                            this.sync_from_shared_store(cx);
                                        } else {
                                            this.connect_to_target(
                                                host.name.clone(),
                                                multibot_core::DaemonTarget::RemoteSsh {
                                                    host: host.clone(),
                                                    bundled_linux_binary:
                                                        Self::find_bundled_linux_daemon(),
                                                },
                                                cx,
                                            );
                                        }
                                    })),
                            ),
                    );
                }
                if self.connected_host.is_some() {
                    hosts = hosts.child(
                        Button::new("disconnect-host")
                            .small()
                            .label("Disconnect host")
                            .on_click(cx.listener(|this, _, _, cx| this.disconnect_host(cx))),
                    );
                }
                content = content.child(hosts).child(v_flex().gap_3().p_4().rounded(RADIUS_BUBBLE)
                    .bg(theme::bg_surface()).border_1().border_color(theme::border_subtle())
                    .child(div().text_sm().child("Add a host"))
                    .child(h_flex().gap_2().child(div().flex_1().min_w_0().child(Input::new(&self.host_input).id("host-input")))
                        .child(Button::new("add-host").label("Add Host").on_click(cx.listener(|this, _, window, cx| this.add_ssh_host_from_input(window, cx)))))
                    .child(Button::new("import-zigshel").label("Import from ZigShell (read-only)")
                        .on_click(cx.listener(|this, _, _, cx| this.import_from_zigshel(cx))))
                    .child(div().text_xs().text_color(theme::text_secondary())
                        .child("Imports hosts, SSH keys, workspaces, session assignments and favorites. Never changes ZigShell files.")));
            }
            SettingsTab::Workspaces => {
                let mut workspaces = v_flex()
                    .gap_3()
                    .p_4()
                    .rounded(RADIUS_BUBBLE)
                    .bg(theme::bg_surface())
                    .border_1()
                    .border_color(theme::border_subtle());
                for (idx, ws) in self
                    .sidebar_state
                    .workspace_store
                    .visible_workspaces(multibot_core::workspaces::now_unix_ms())
                    .into_iter()
                    .enumerate()
                {
                    let id = ws.id.clone();
                    workspaces =
                        workspaces.child(
                            h_flex()
                                .justify_between()
                                .items_center()
                                .gap_2()
                                .child(ws.name.clone())
                                .child(
                                    Button::new(SharedString::from(format!(
                                        "settings-workspace-{idx}"
                                    )))
                                    .small()
                                    .label("Open")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.sidebar_state.active_workspace_id = id.clone();
                                        this.sidebar_state.active_workspace_view =
                                            multibot_core::workspaces::WorkspaceView::All;
                                        this.set_main_surface(crate::state::MainSurface::Chat);
                                        this.focus_active_surface_default(window, cx);
                                        cx.notify();
                                    })),
                                ),
                        );
                }
                content = content
                    .child(workspaces)
                    .child(
                        Button::new("settings-new-workspace")
                            .label("New workspace")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.set_main_surface(crate::state::MainSurface::Chat);
                                this.set_sidebar_mode_synced(multibot_core::SidebarMode::Open, cx);
                                this.sidebar_state.new_workspace_draft = Some(String::new());
                                this.focus_sidebar(window, cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("settings-manage-workspaces")
                            .label("Manage workspaces…")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_palette(window, cx);
                                this.palette_input.update(cx, |input, cx| {
                                    input.set_value("workspace", window, cx)
                                });
                            })),
                    );
            }
            SettingsTab::Gpu => {
                let (gpu, _) = self.active_gpu_summary();
                content =
                    content.child(
                        v_flex()
                            .gap_3()
                            .p_4()
                            .rounded(RADIUS_BUBBLE)
                            .bg(theme::bg_surface())
                            .border_1()
                            .border_color(theme::border_subtle())
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme::accent_green())
                                    .child(format!("Active GPU: {gpu}")),
                            )
                            .child(div().text_xs().text_color(theme::text_secondary()).child(
                                format!("{} · {}", self.footprint_label, self.connection_status),
                            ))
                            .child(
                                Button::new("gpu-mode-auto")
                                    .label("Auto (iGPU, 0% NVIDIA)")
                                    .selected(self.settings.gpu == GpuPreference::AutoIntegrated)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.set_gpu_preference(GpuPreference::AutoIntegrated, cx)
                                    })),
                            )
                            .child(
                                Button::new("gpu-mode-software")
                                    .label("Software Only (WARP)")
                                    .selected(self.settings.gpu == GpuPreference::SoftwareOnly)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.set_gpu_preference(GpuPreference::SoftwareOnly, cx)
                                    })),
                            )
                            .child(discovered_col)
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::text_secondary())
                                    .child("Restart Multibot after changing the adapter."),
                            ),
                    );
            }
            SettingsTab::Shortcuts => {
                let mut index = 0;
                for category in multibot_keys::ALL_CATEGORIES {
                    let mut card = v_flex()
                        .gap_2()
                        .p_4()
                        .rounded(RADIUS_BUBBLE)
                        .bg(theme::bg_surface())
                        .border_1()
                        .border_color(theme::border_subtle())
                        .child(div().text_sm().child(category.label()));
                    for spec in multibot_keys::by_category(category) {
                        card = card.child(
                            h_flex()
                                .id(SharedString::from(format!("settings-shortcut-{index}")))
                                .test_support()
                                .justify_between()
                                .gap_3()
                                .text_xs()
                                .child(div().flex_1().child(spec.description))
                                .child(
                                    div()
                                        .text_color(theme::text_secondary())
                                        .child(spec.display),
                                ),
                        );
                        index += 1;
                    }
                    content = content.child(card);
                }
            }
            SettingsTab::Updates => {
                content = content
                    .child(
                        v_flex()
                            .gap_3()
                            .p_4()
                            .rounded(RADIUS_BUBBLE)
                            .bg(theme::bg_surface())
                            .border_1()
                            .border_color(theme::border_subtle())
                            .child(
                                div()
                                    .id("settings-version-label")
                                    .test_support()
                                    .text_sm()
                                    .child(format!(
                                        "Multibot v{}",
                                        multibot_proto::build_version()
                                    )),
                            )
                            .child(
                                Button::new("settings-check-updates-btn")
                                    .label("Check for Updates")
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.trigger_update_check(cx)),
                                    ),
                            ),
                    )
                    .when_some(self.render_update_card(true, cx), |el, card| el.child(card));
            }
        }
        let mut rail = v_flex()
            .id("settings-nav")
            .test_support()
            .w(px(216.0))
            .flex_shrink_0()
            .h_full()
            .p_3()
            .gap_1()
            .bg(theme::bg_sidebar())
            .child(div().p_2().text_sm().child("Settings"));
        for tab in SettingsTab::ALL {
            rail = rail.child(
                Button::new(tab.id())
                    .w_full()
                    .ghost()
                    .label(tab.title())
                    .child(div().flex_1())
                    .selected(self.settings_tab == tab)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.settings_tab = tab;
                        window.focus(&this.settings_focus_handle, cx);
                        cx.notify();
                    })),
            );
        }
        div()
            .id("settings-backdrop")
            .absolute()
            .inset_0()
            .bg(gpui_kit::hsla(0.0, 0.0, 0.0, 0.58))
            .flex()
            .items_center()
            .justify_center()
            .p_5()
            .on_mouse_down(
                gpui_kit::MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.set_main_surface(crate::state::MainSurface::Chat);
                    this.focus_active_surface_default(window, cx);
                    cx.notify();
                }),
            )
            .child(
                h_flex()
                    .id("settings-console")
                    .test_support()
                    .w(px(980.0))
                    .max_w_full()
                    .h_full()
                    .max_h(px(760.0))
                    .rounded(crate::theme::RADIUS_MODAL)
                    .overflow_hidden()
                    .bg(theme::bg_canvas())
                    .border_1()
                    .border_color(theme::border_subtle())
                    .occlude()
                    .focus_trap("settings-console", &self.settings_focus_handle)
                    .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation()
                    })
                    .child(rail)
                    .child(
                        v_flex()
                            .id("settings-content")
                            .test_support()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(
                                h_flex()
                                    .p_5()
                                    .justify_between()
                                    .items_start()
                                    .gap_3()
                                    .child(
                                        v_flex()
                                            .gap_2()
                                            .child(div().text_lg().child(self.settings_tab.title()))
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(theme::text_secondary())
                                                    .child(self.settings_tab.subtitle()),
                                            ),
                                    )
                                    .child(
                                        Button::new("settings-close").xsmall().label("×").on_click(
                                            cx.listener(|this, _, window, cx| {
                                                this.set_main_surface(
                                                    crate::state::MainSurface::Chat,
                                                );
                                                this.focus_active_surface_default(window, cx);
                                                cx.notify();
                                            }),
                                        ),
                                    ),
                            )
                            .child(
                                div()
                                    .id("settings-scroll")
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .p_5()
                                    .pt_0()
                                    .child(content)
                                    .when_some(self.notice.clone(), |el, note| {
                                        el.child(div().pt_4().text_xs().child(note))
                                    }),
                            ),
                    ),
            )
    }

    fn save_general_preferences(&mut self, cx: &mut Context<Self>) {
        self.sync_settings_to_shared(cx);
        if self.is_disk_persistence_enabled(cx) {
            if let Err(err) = self.settings.save() {
                self.notice = Some(format!("Could not save preferences: {err}"));
            }
        }
        cx.notify();
    }
}
