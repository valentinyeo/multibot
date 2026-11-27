use crate::app::MultibotView;
use crate::theme::{self, RADIUS_BUBBLE, RADIUS_CHIP, RADIUS_ROW};
use gpui_kit::{
    Context, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, TestSupportExt, Window,
    component::{
        Sizable,
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

        let hosts_added = self.settings.merge_hosts(bundle.hosts.clone());
        let merge_stats = merge_zigshel_bundle(&mut self.sidebar_state.workspace_store, &bundle);

        self.sync_settings_to_shared(cx);
        self.sync_workspace_to_shared(cx);
        if self.sidebar_state.persist_to_disk {
            let _ = self.settings.save();
        }
        self.persist_workspace_store_if_enabled();

        self.notice = Some(if hosts_found == 0 && ws_found == 0 && fav_found == 0 {
            "No SSH hosts, workspaces, or favorites found in %APPDATA%\\ZigShell (read-only check)."
                .into()
        } else {
            format!(
                "Imported {hosts_added} host(s), {} workspace(s), and {} favorite(s) from ZigShell (read-only).",
                merge_stats.workspaces_added, merge_stats.favorites_added
            )
        });
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
                discovered_col = discovered_col.child(if is_selected { btn.primary() } else { btn });
            }
        }

        v_flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .child(self.render_top_floating_bar("GPU & VPS Settings".into(), cx))
            .child(
                div()
                    .id("settings-scroll")
                    .flex_1()
                    .overflow_y_scroll()
                    .child(
                        v_flex()
                            .w_full()
                            .max_w(px(760.0))
                            .mx_auto()
                            .p_5()
                            .gap_4()
                            .child(
                                v_flex()
                                    .p_4()
                                    .gap_3()
                                    .rounded(RADIUS_BUBBLE)
                                    .bg(theme::bg_surface())
                                    .border_1()
                                    .border_color(theme::border_subtle())
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(theme::text_primary())
                                            .child("Graphics Adapter Policy (Windows DXGI)"),
                                    )
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .child({
                                                let b = Button::new("gpu-mode-auto")
                                                    .label("Auto Integrated (0% NVIDIA)")
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.set_gpu_preference(
                                                            GpuPreference::AutoIntegrated,
                                                            cx,
                                                        );
                                                    }));
                                                if self.settings.gpu == GpuPreference::AutoIntegrated
                                                {
                                                    b.primary()
                                                } else {
                                                    b
                                                }
                                            })
                                            .child({
                                                let b = Button::new("gpu-mode-software")
                                                    .label("Software Only (WARP)")
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.set_gpu_preference(
                                                            GpuPreference::SoftwareOnly,
                                                            cx,
                                                        );
                                                    }));
                                                if self.settings.gpu == GpuPreference::SoftwareOnly {
                                                    b.primary()
                                                } else {
                                                    b
                                                }
                                            }),
                                    )
                                    .child(discovered_col),
                            )
                            .child(
                                v_flex()
                                    .p_4()
                                    .gap_3()
                                    .rounded(RADIUS_BUBBLE)
                                    .bg(theme::bg_surface())
                                    .border_1()
                                    .border_color(theme::border_subtle())
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(theme::text_primary())
                                            .child("VPS SSH Hosts (%APPDATA%\\Multibot\\settings.json)"),
                                    )
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .child(Input::new(&self.host_input).id("host-input")),
                                            )
                                            .child(
                                                Button::new("add-host")
                                                    .primary()
                                                    .label("Add Host")
                                                    .on_click(cx.listener(|this, _, window, cx| {
                                                        this.add_ssh_host_from_input(window, cx);
                                                    })),
                                            )
                                            .child(
                                                Button::new("import-zigshel")
                                                    .label("Import ZigShell (read-only)")
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.import_from_zigshel(cx);
                                                    })),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme::text_muted())
                                            .child("Imports SSH hosts (including IdentityFile keys), workspaces & favorites without modifying %APPDATA%\\ZigShell."),
                                    ),
                            )
                            .child(
                                v_flex()
                                    .p_4()
                                    .gap_3()
                                    .rounded(RADIUS_BUBBLE)
                                    .bg(theme::bg_surface())
                                    .border_1()
                                    .border_color(theme::border_subtle())
                                    .child(
                                        h_flex()
                                            .items_center()
                                            .justify_between()
                                            .child(
                                                h_flex()
                                                    .items_center()
                                                    .gap_2()
                                                    .child(
                                                        div()
                                                            .text_sm()
                                                            .text_color(theme::text_primary())
                                                            .child("About & Updates"),
                                                    )
                                                    .child(
                                                        div()
                                                            .id("settings-version-label")
                                                            .test_support()
                                                            .px_2p5()
                                                            .py_0p5()
                                                            .rounded(RADIUS_CHIP)
                                                            .bg(theme::bg_canvas())
                                                            .border_1()
                                                            .border_color(theme::border_subtle())
                                                            .text_xs()
                                                            .text_color(theme::text_primary())
                                                            .child(format!(
                                                                "v{}",
                                                                multibot_proto::build_version()
                                                            )),
                                                    ),
                                            )
                                            .child(
                                                Button::new("settings-check-updates-btn")
                                                    .small()
                                                    .label("Check for Updates")
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.trigger_update_check(cx);
                                                    })),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme::text_muted())
                                            .child(format!(
                                                "{} · {}",
                                                self.footprint_label, self.connection_status
                                            )),
                                    ),
                            )
                            .when_some(self.notice.clone(), |el, note| {
                                el.child(
                                    div()
                                        .p_3()
                                        .rounded(RADIUS_ROW)
                                        .bg(theme::bg_surface())
                                        .border_1()
                                        .border_color(theme::border_subtle())
                                        .text_xs()
                                        .text_color(theme::text_primary())
                                        .child(note),
                                )
                            }),
                    ),
            )
    }
}
