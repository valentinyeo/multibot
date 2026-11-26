use gpui_kit::{
    App, AppContext, Bounds, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    Render, SharedString, StatefulInteractiveElement, Styled, TitlebarOptions, Window,
    WindowBounds, WindowOptions, actions,
    component::{
        ActiveTheme, Sizable,
        button::{Button, ButtonVariants},
        h_flex,
        input::{Input, InputState},
        v_flex,
    },
    div,
    prelude::FluentBuilder,
    px, rgb, size,
};
use multibot_core::{
    DaemonConnection, DaemonTarget, FootprintSnapshot, MultibotSettings, SshHostConfig,
    import_ssh_hosts_readonly, sample_self_footprint,
};
use multibot_gpu::{GpuPreference, GpuRuntimeReport, read_gpu_report_file};
use multibot_proto::{ChatMessage, ClientRequest, MessageRole, ServerEvent, SessionSummary};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use sysinfo::System;
use tokio::sync::mpsc;

actions!(multibot, [ToggleSettings, ToggleSidebar, NewWindow]);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivePanel {
    Chat,
    Settings,
}

enum UiBridgeEvent {
    Connected {
        host_label: String,
        req_tx: mpsc::UnboundedSender<ClientRequest>,
    },
    Server(ServerEvent),
    Disconnected(String),
}

pub struct MultibotView {
    runtime: Arc<tokio::runtime::Runtime>,
    settings: MultibotSettings,
    gpu_report: GpuRuntimeReport,
    footprint: FootprintSnapshot,
    active_panel: ActivePanel,
    sidebar_visible: bool,

    // Daemon & chat state
    connected_host: Option<String>,
    connection_status: String,
    daemon_tx: Option<mpsc::UnboundedSender<ClientRequest>>,
    sessions: Vec<SessionSummary>,
    selected_session: Option<SessionSummary>,
    messages: Vec<ChatMessage>,

    // Inputs
    prompt_input: Entity<InputState>,
    host_input: Entity<InputState>,
    notice: Option<String>,
}

impl MultibotView {
    pub fn new(
        runtime: Arc<tokio::runtime::Runtime>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings = MultibotSettings::load();
        let gpu_report =
            read_gpu_report_file(&MultibotSettings::gpu_report_path()).unwrap_or_default();

        let mut sys = System::new();
        let footprint = sample_self_footprint(&mut sys);

        let prompt_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Send a prompt to this agent..."));
        let host_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("user@vps-host (or user@host:22)"));

        // Lightweight 5-second single-PID footprint refresh (zero GPU/CPU spin).
        cx.spawn(async move |this, cx| {
            let mut sys = System::new();
            loop {
                cx.background_executor().timer(Duration::from_secs(5)).await;
                let snap = sample_self_footprint(&mut sys);
                let ok = this.update(cx, |view, cx| {
                    view.footprint = snap;
                    cx.notify();
                });
                if ok.is_err() {
                    break;
                }
            }
        })
        .detach();

        Self {
            runtime,
            settings,
            gpu_report,
            footprint,
            active_panel: ActivePanel::Chat,
            sidebar_visible: true,
            connected_host: None,
            connection_status: "Not connected".to_string(),
            daemon_tx: None,
            sessions: Vec::new(),
            selected_session: None,
            messages: Vec::new(),
            prompt_input,
            host_input,
            notice: None,
        }
    }

    fn active_gpu_summary(&self) -> (String, bool) {
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

    fn connect_to_target(
        &mut self,
        label: String,
        target: DaemonTarget,
        cx: &mut Context<Self>,
    ) {
        self.connection_status = format!("Connecting to {label}...");
        self.connected_host = Some(label.clone());
        self.sessions.clear();
        self.selected_session = None;
        self.messages.clear();
        self.daemon_tx = None;
        self.notice = None;
        cx.notify();

        let (bridge_tx, mut bridge_rx) = mpsc::unbounded_channel::<UiBridgeEvent>();
        let rt = self.runtime.clone();

        rt.spawn(async move {
            match DaemonConnection::connect(target).await {
                Ok(mut conn) => {
                    let (req_tx, mut req_rx) = mpsc::unbounded_channel::<ClientRequest>();
                    let _ = bridge_tx.send(UiBridgeEvent::Connected {
                        host_label: label.clone(),
                        req_tx,
                    });
                    let _ = conn.send(ClientRequest::ListSessions { limit: Some(40) });

                    loop {
                        tokio::select! {
                            Some(req) = req_rx.recv() => {
                                if conn.send(req).is_err() {
                                    break;
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

        cx.spawn(async move |this, cx| {
            while let Some(event) = bridge_rx.recv().await {
                let res = this.update(cx, |view, cx| {
                    view.handle_bridge_event(event, cx);
                });
                if res.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn handle_bridge_event(&mut self, event: UiBridgeEvent, cx: &mut Context<Self>) {
        match event {
            UiBridgeEvent::Connected { host_label, req_tx } => {
                self.daemon_tx = Some(req_tx);
                self.connection_status = format!("Connected: {host_label}");
            }
            UiBridgeEvent::Disconnected(reason) => {
                self.daemon_tx = None;
                self.connection_status = reason;
            }
            UiBridgeEvent::Server(server_ev) => match server_ev {
                ServerEvent::HelloAck {
                    hostname,
                    daemon_version,
                    ..
                } => {
                    self.connection_status =
                        format!("Connected: {hostname} (multibotd v{daemon_version})");
                }
                ServerEvent::Sessions { sessions } => {
                    self.sessions = sessions;
                    if self.selected_session.is_none()
                        && let Some(first) = self.sessions.first().cloned()
                    {
                        self.select_session(first, cx);
                    }
                }
                ServerEvent::History {
                    session_id,
                    messages,
                } => {
                    if self
                        .selected_session
                        .as_ref()
                        .is_some_and(|s| s.id == session_id)
                    {
                        self.messages = messages;
                    }
                }
                ServerEvent::HistoryAppend {
                    session_id,
                    messages,
                } => {
                    if self
                        .selected_session
                        .as_ref()
                        .is_some_and(|s| s.id == session_id)
                    {
                        self.messages.extend(messages);
                    }
                }
                ServerEvent::SendAccepted { detail, .. } => {
                    self.notice = Some(format!("Sent: {detail}"));
                }
                ServerEvent::Error { code, message } => {
                    self.notice = Some(format!("[{code}] {message}"));
                }
                ServerEvent::Pong { .. } => {}
            },
        }
        cx.notify();
    }

    fn select_session(&mut self, session: SessionSummary, cx: &mut Context<Self>) {
        self.selected_session = Some(session.clone());
        self.messages.clear();
        self.active_panel = ActivePanel::Chat;
        if let Some(ref tx) = self.daemon_tx {
            let _ = tx.send(ClientRequest::WatchSession {
                session_id: session.id,
                source_path: session.source_path,
                agent: session.agent,
            });
        }
        cx.notify();
    }

    fn send_current_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.prompt_input.read(cx).value().to_string();
        let trimmed = text.trim().to_string();
        if trimmed.is_empty() {
            return;
        }
        let Some(session) = self.selected_session.clone() else {
            self.notice = Some("Select a session first.".into());
            cx.notify();
            return;
        };
        let Some(ref tx) = self.daemon_tx else {
            self.notice = Some("Connect to a VPS or local daemon first.".into());
            cx.notify();
            return;
        };

        let _ = tx.send(ClientRequest::SendPrompt {
            session_id: session.id,
            agent: session.agent,
            cwd: session.cwd,
            text: trimmed.clone(),
        });
        self.messages.push(ChatMessage {
            id: format!("local-pending-{}", self.messages.len()),
            role: MessageRole::User,
            text: trimmed,
            timestamp_ms: None,
        });
        self.prompt_input.update(cx, |state, cx| {
            state.set_value("", window, cx);
        });
        cx.notify();
    }

    fn set_gpu_preference(&mut self, pref: GpuPreference, cx: &mut Context<Self>) {
        self.settings.gpu = pref;
        match self.settings.save() {
            Ok(()) => {
                self.notice = Some(format!(
                    "GPU preference saved to {} — restart Multibot to apply.",
                    MultibotSettings::settings_path().display()
                ));
            }
            Err(err) => {
                self.notice = Some(format!("Failed to save settings: {err}"));
            }
        }
        cx.notify();
    }

    fn add_ssh_host_from_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        };
        self.settings.merge_hosts(vec![host]);
        let _ = self.settings.save();
        self.host_input.update(cx, |state, cx| {
            state.set_value("", window, cx);
        });
        self.notice = Some("SSH host added.".into());
        cx.notify();
    }

    fn import_from_zigshel(&mut self, cx: &mut Context<Self>) {
        let found = import_ssh_hosts_readonly(None);
        let total = found.len();
        let added = self.settings.merge_hosts(found);
        let _ = self.settings.save();
        self.notice = Some(if total == 0 {
            "No SSH hosts found in %APPDATA%\\ZigShell (read-only check).".into()
        } else {
            format!("Imported {added} new SSH host(s) from ZigShell (read-only).")
        });
        cx.notify();
    }

    fn open_extra_window(&self, cx: &mut App) {
        let bounds = Bounds::centered(None, size(px(1100.0), px(720.0)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("Multibot — Workspace".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let rt = self.runtime.clone();
        let _ = gpui_kit::open_window(options, cx, move |window, cx| {
            cx.new(|cx| MultibotView::new(rt, window, cx))
        });
    }

    fn find_bundled_linux_daemon() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let dir = exe.parent()?;
        let candidate = dir.join("multibotd-linux-x86_64");
        candidate.is_file().then_some(candidate)
    }

    fn find_local_daemon() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let dir = exe.parent()?;
        #[cfg(target_os = "windows")]
        let name = "multibotd.exe";
        #[cfg(not(target_os = "windows"))]
        let name = "multibotd";
        let candidate = dir.join(name);
        candidate.is_file().then_some(candidate)
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (gpu_label, gpu_safe) = self.active_gpu_summary();
        let is_settings = self.active_panel == ActivePanel::Settings;

        let mut host_list = v_flex().gap_1();
        for (idx, host) in self.settings.ssh_hosts.iter().enumerate() {
            let host_clone = host.clone();
            let label = format!("{} ({})", host.name, host.ssh_target());
            host_list = host_list.child(
                Button::new(SharedString::from(format!("host-{idx}")))
                    .small()
                    .label(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let target = DaemonTarget::RemoteSsh {
                            host: host_clone.clone(),
                            bundled_linux_binary: Self::find_bundled_linux_daemon(),
                        };
                        this.connect_to_target(host_clone.name.clone(), target, cx);
                    })),
            );
        }

        let mut session_list = v_flex().gap_1();
        if self.sessions.is_empty() {
            session_list = session_list.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .p_2()
                    .child("Connect to a VPS or local daemon to list Claude, Antigravity & Codex chats."),
            );
        } else {
            for (idx, session) in self.sessions.iter().enumerate() {
                let is_selected = self
                    .selected_session
                    .as_ref()
                    .is_some_and(|s| s.id == session.id);
                let sess_clone = session.clone();
                let dot = if session.working { " ●" } else { "" };
                let row_label = format!("{} {}{}", session.agent.glyph(), session.title, dot);
                let btn = Button::new(SharedString::from(format!("session-{idx}")))
                    .small()
                    .label(row_label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_session(sess_clone.clone(), cx);
                    }));
                session_list = session_list.child(if is_selected { btn.primary() } else { btn });
            }
        }

        v_flex()
            .w(px(290.0))
            .h_full()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.border)
            .p_3()
            .gap_3()
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.foreground)
                            .child("Multibot v0.1"),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("new-window")
                                    .xsmall()
                                    .label("+ Win")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.open_extra_window(cx);
                                    })),
                            )
                            .child(
                                Button::new("toggle-settings")
                                    .xsmall()
                                    .label(if is_settings { "Chat" } else { "GPU / Settings" })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.active_panel = match this.active_panel {
                                            ActivePanel::Chat => ActivePanel::Settings,
                                            ActivePanel::Settings => ActivePanel::Chat,
                                        };
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("HOSTS"),
                    )
                    .child(host_list)
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("connect-local")
                                    .xsmall()
                                    .label("Local Daemon")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(bin) = Self::find_local_daemon() {
                                            this.connect_to_target(
                                                "localhost".into(),
                                                DaemonTarget::LocalBinary(bin),
                                                cx,
                                            );
                                        } else {
                                            this.notice = Some(
                                                "No local multibotd binary next to multibot.exe — add an SSH VPS host in Settings."
                                                    .into(),
                                            );
                                            cx.notify();
                                        }
                                    })),
                            )
                            .child(
                                Button::new("import-zigshel-sidebar")
                                    .xsmall()
                                    .label("Import ZigShell")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.import_from_zigshel(cx);
                                    })),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!("SESSIONS ({})", self.sessions.len())),
                    )
                    .child(
                        div()
                            .id("sessions-scroll")
                            .flex_1()
                            .overflow_y_scroll()
                            .child(session_list),
                    ),
            )
            .child(
                v_flex()
                    .id("gpu-status-card")
                    .p_2()
                    .gap_1()
                    .rounded_md()
                    .border_1()
                    .border_color(if gpu_safe {
                        rgb(0x22c55e)
                    } else {
                        rgb(0xf59e0b)
                    })
                    .bg(theme.background)
                    .child(
                        div()
                            .text_xs()
                            .text_color(if gpu_safe {
                                rgb(0x22c55e)
                            } else {
                                rgb(0xf59e0b)
                            })
                            .child(format!("GPU: {gpu_label}")),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!(
                                "RAM: {:.1} MB · CPU: {:.1}% · {}",
                                self.footprint.memory_mb,
                                self.footprint.cpu_percent,
                                self.connection_status
                            )),
                    ),
            )
    }

    fn render_chat_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let header_title = match &self.selected_session {
            Some(s) => format!("{} {} ({})", s.agent.glyph(), s.title, s.agent.label()),
            None => "Multibot — Ready (Milestone 0 PoC)".to_string(),
        };

        let mut msg_col = v_flex().gap_3().p_4();
        if self.messages.is_empty() {
            msg_col = msg_col.child(
                div()
                    .p_4()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(
                        "Milestone 0 checklist:\n\
                         1. Check the green GPU badge in the bottom-left and Windows Task Manager (confirm 0% NVIDIA).\n\
                         2. Open GPU / Settings to inspect all detected GPUs and add your Contabo VPS SSH host (or click Import ZigShell).\n\
                         3. Click your VPS host to auto-deploy multibotd and stream live Claude / Antigravity / Codex chats.",
                    ),
            );
        } else {
            for msg in &self.messages {
                let is_user = msg.role == MessageRole::User;
                msg_col = msg_col.child(
                    v_flex()
                        .p_3()
                        .gap_1()
                        .rounded_md()
                        .border_1()
                        .border_color(theme.border)
                        .bg(if is_user {
                            theme.secondary
                        } else {
                            theme.background
                        })
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(if is_user { "You" } else { "Agent" }),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.foreground)
                                .child(msg.text.clone()),
                        ),
                );
            }
        }

        v_flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .child(
                h_flex()
                    .px_4()
                    .py_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .justify_between()
                    .child(div().text_sm().text_color(theme.foreground).child(header_title))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(self.connected_host.clone().unwrap_or_else(|| "Offline".into())),
                    ),
            )
            .child(
                div()
                    .id("chat-scroll")
                    .flex_1()
                    .overflow_y_scroll()
                    .child(msg_col),
            )
            .when_some(self.notice.clone(), |el, note| {
                el.child(
                    div()
                        .px_4()
                        .py_2()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .border_t_1()
                        .border_color(theme.border)
                        .child(note),
                )
            })
            .child(
                h_flex()
                    .p_3()
                    .gap_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(div().flex_1().child(Input::new(&self.prompt_input).id("prompt-input")))
                    .child(
                        Button::new("send-prompt")
                            .primary()
                            .label("Send")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.send_current_prompt(window, cx);
                            })),
                    ),
            )
    }

    fn render_settings_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let mut discovered_col = v_flex().gap_2();

        if self.gpu_report.discovered.is_empty() {
            discovered_col = discovered_col.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
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
            .id("settings-scroll")
            .flex_1()
            .h_full()
            .p_5()
            .gap_4()
            .overflow_y_scroll()
            .child(
                div()
                    .text_base()
                    .text_color(theme.foreground)
                    .child("GPU Selection & VPS Settings"),
            )
            .child(
                v_flex()
                    .p_4()
                    .gap_3()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.foreground)
                            .child("Graphics Adapter Policy (Windows DXGI)"),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child({
                                let b = Button::new("gpu-mode-auto")
                                    .label("Auto Integrated (0% NVIDIA)")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.set_gpu_preference(GpuPreference::AutoIntegrated, cx);
                                    }));
                                if self.settings.gpu == GpuPreference::AutoIntegrated {
                                    b.primary()
                                } else {
                                    b
                                }
                            })
                            .child({
                                let b = Button::new("gpu-mode-software")
                                    .label("Software Only (WARP)")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.set_gpu_preference(GpuPreference::SoftwareOnly, cx);
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
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.foreground)
                            .child("VPS SSH Hosts (stored in %APPDATA%\\Multibot\\settings.json)"),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().child(Input::new(&self.host_input).id("host-input")))
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
                                    .label("Import from ZigShell (read-only)")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.import_from_zigshel(cx);
                                    })),
                            ),
                    ),
            )
            .when_some(self.notice.clone(), |el, note| {
                el.child(
                    div()
                        .p_3()
                        .rounded_md()
                        .border_1()
                        .border_color(theme.border)
                        .text_xs()
                        .text_color(theme.foreground)
                        .child(note),
                )
            })
    }
}

impl Render for MultibotView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        h_flex()
            .id("multibot-root")
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .on_action(cx.listener(|this, _: &ToggleSettings, _, cx| {
                this.active_panel = match this.active_panel {
                    ActivePanel::Chat => ActivePanel::Settings,
                    ActivePanel::Settings => ActivePanel::Chat,
                };
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                this.sidebar_visible = !this.sidebar_visible;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &NewWindow, _, cx| {
                this.open_extra_window(cx);
            }))
            .when(self.sidebar_visible, |el| el.child(self.render_sidebar(cx)))
            .child(match self.active_panel {
                ActivePanel::Chat => self.render_chat_panel(cx).into_any_element(),
                ActivePanel::Settings => self.render_settings_panel(cx).into_any_element(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{TestAppContext, test::TestWindowExt};

    #[gpui_kit::test]
    fn renders_and_toggles_gpu_settings_panel(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let rt = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let (handle, _view) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, move |window, cx| {
                cx.new(|cx| MultibotView::new(rt, window, cx))
            })
            .unwrap()
        });

        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("toggle-settings", cx);
            window.render_frame(cx);
            window.click("gpu-mode-software", cx);
            window.render_frame(cx);
        })
        .unwrap();
    }
}
