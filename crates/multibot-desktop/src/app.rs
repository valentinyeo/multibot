use crate::theme::{
    self, RADIUS_BUBBLE, RADIUS_CHIP, RADIUS_MODAL, RADIUS_ROW,
};
use gpui_kit::{
    App, AppContext, Bounds, Context, Entity, InteractiveElement, IntoElement, MouseButton,
    ParentElement, Render, Role, SharedString, StatefulInteractiveElement, Styled, TestSupportExt,
    TitlebarOptions, Window, WindowBounds, WindowOptions, actions,
    component::{
        Sizable,
        button::{Button, ButtonVariants},
        h_flex,
        input::{Input, InputState},
        text::TextView,
        v_flex,
    },
    div, hsla,
    prelude::FluentBuilder,
    px, size,
};
use multibot_core::{
    DaemonConnection, DaemonTarget, FootprintSnapshot, MultibotSettings, SshHostConfig,
    import_ssh_hosts_readonly, sample_self_footprint,
};
use multibot_gpu::{GpuPreference, GpuRuntimeReport, read_gpu_report_file};
use multibot_keys::SHORTCUTS;
use multibot_proto::{AgentKind, ChatMessage, ClientRequest, MessageRole, ServerEvent, SessionSummary};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use sysinfo::System;
use tokio::sync::mpsc;

actions!(
    multibot,
    [
        OpenPalette,
        OpenInbox,
        ToggleInbox,
        ToggleSettings,
        ToggleSidebar,
        NewWindow,
        OpenShortcuts,
        CloseOverlay,
        NextSession,
        PrevSession,
        JumpSession1,
        JumpSession2,
        JumpSession3,
        JumpSession4,
        JumpSession5,
        JumpSession6,
        JumpSession7,
        JumpSession8,
        JumpSession9,
    ]
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MainSurface {
    Chat,
    Inbox,
    Settings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveModal {
    None,
    Palette,
    Shortcuts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InboxFilter {
    All,
    Agent(AgentKind),
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

    // Navigation & overlays
    main_surface: MainSurface,
    active_modal: ActiveModal,
    sidebar_visible: bool,
    inbox_filter: InboxFilter,

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
    palette_input: Entity<InputState>,
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
            cx.new(|cx| InputState::new(window, cx).placeholder("Message this session"));
        let host_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("user@vps-host (or user@host:22)"));
        let palette_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search sessions and commands..."));

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
            main_surface: MainSurface::Chat,
            active_modal: ActiveModal::None,
            sidebar_visible: true,
            inbox_filter: InboxFilter::All,
            connected_host: None,
            connection_status: "Offline".to_string(),
            daemon_tx: None,
            sessions: Vec::new(),
            selected_session: None,
            messages: Vec::new(),
            prompt_input,
            host_input,
            palette_input,
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
                    let _ = conn.send(ClientRequest::ListSessions { limit: Some(60) });

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
                    self.connection_status = format!("{hostname} · multibotd v{daemon_version}");
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
        self.main_surface = MainSurface::Chat;
        self.active_modal = ActiveModal::None;
        if let Some(ref tx) = self.daemon_tx {
            let _ = tx.send(ClientRequest::WatchSession {
                session_id: session.id,
                source_path: session.source_path,
                agent: session.agent,
            });
        }
        cx.notify();
    }

    fn jump_session_index(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(sess) = self.sessions.get(index).cloned() {
            self.select_session(sess, cx);
        }
    }

    fn cycle_session(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.sessions.is_empty() {
            return;
        }
        let current = self
            .selected_session
            .as_ref()
            .and_then(|sel| self.sessions.iter().position(|s| s.id == sel.id))
            .unwrap_or(0);
        let len = self.sessions.len() as isize;
        let next = ((current as isize + delta).rem_euclid(len)) as usize;
        if let Some(sess) = self.sessions.get(next).cloned() {
            self.select_session(sess, cx);
        }
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
                    "Saved GPU policy to {} — restart Multibot to apply.",
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
        let bounds = Bounds::centered(None, size(px(1140.0), px(740.0)), cx);
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

    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.active_modal = ActiveModal::Palette;
        self.palette_input.update(cx, |state, cx| {
            state.set_value("", window, cx);
        });
        cx.notify();
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

    fn agent_color(agent: AgentKind) -> gpui_kit::Hsla {
        match agent {
            AgentKind::Claude => gpui_kit::rgb(0xD97757).into(),
            AgentKind::Antigravity => gpui_kit::rgb(0x60A5FA).into(),
            AgentKind::Codex => gpui_kit::rgb(0x34D399).into(),
        }
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (gpu_label, gpu_safe) = self.active_gpu_summary();

        let mut host_list = v_flex().gap_1();
        for (idx, host) in self.settings.ssh_hosts.iter().enumerate() {
            let host_clone = host.clone();
            let active = self.connected_host.as_deref() == Some(host.name.as_str());
            host_list = host_list.child(
                div()
                    .id(SharedString::from(format!("host-{idx}")))
                    .px_3()
                    .py_2()
                    .rounded(RADIUS_ROW)
                    .bg(if active {
                        theme::bg_active_row()
                    } else {
                        theme::bg_surface()
                    })
                    .border_1()
                    .border_color(theme::border_subtle())
                    .hover(|s| s.bg(theme::bg_hover()))
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            let target = DaemonTarget::RemoteSsh {
                                host: host_clone.clone(),
                                bundled_linux_binary: Self::find_bundled_linux_daemon(),
                            };
                            this.connect_to_target(host_clone.name.clone(), target, cx);
                        }),
                    )
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::text_primary())
                                    .child(host.name.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::text_muted())
                                    .child(host.ssh_target()),
                            ),
                    ),
            );
        }

        let mut session_list = v_flex().gap_1();
        if self.sessions.is_empty() {
            session_list = session_list.child(
                div()
                    .px_3()
                    .py_3()
                    .rounded(RADIUS_ROW)
                    .bg(theme::bg_surface())
                    .text_xs()
                    .text_color(theme::text_muted())
                    .child("Connect to a VPS or local daemon to stream Claude, Antigravity & Codex chats."),
            );
        } else {
            for (idx, session) in self.sessions.iter().enumerate() {
                let is_selected = self
                    .selected_session
                    .as_ref()
                    .is_some_and(|s| s.id == session.id);
                let sess_clone = session.clone();
                let shortcut_hint = if idx < 9 {
                    Some(format!("^{}", idx + 1))
                } else {
                    None
                };

                session_list = session_list.child(
                    div()
                        .id(SharedString::from(format!("session-{idx}")))
                        .px_2p5()
                        .py_2()
                        .rounded(RADIUS_ROW)
                        .bg(if is_selected {
                            theme::bg_active_row()
                        } else {
                            theme::bg_sidebar()
                        })
                        .hover(|s| s.bg(theme::bg_hover()))
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                this.select_session(sess_clone.clone(), cx);
                            }),
                        )
                        .child(
                            h_flex()
                                .items_center()
                                .gap_2p5()
                                .child(
                                    div()
                                        .w(px(26.0))
                                        .h(px(26.0))
                                        .rounded(RADIUS_CHIP)
                                        .bg(Self::agent_color(session.agent))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .text_xs()
                                        .text_color(theme::bg_canvas())
                                        .child(session.agent.glyph()),
                                )
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .child(
                                            div()
                                                .text_sm()
                                                .text_color(theme::text_primary())
                                                .truncate()
                                                .child(session.title.clone()),
                                        )
                                        .when_some(session.cwd.clone(), |el, cwd| {
                                            el.child(
                                                div()
                                                    .text_xs()
                                                    .text_color(theme::text_muted())
                                                    .truncate()
                                                    .child(cwd),
                                            )
                                        }),
                                )
                                .when(session.working, |el| {
                                    el.child(
                                        div()
                                            .w(px(7.0))
                                            .h(px(7.0))
                                            .rounded(RADIUS_CHIP)
                                            .bg(theme::accent_green()),
                                    )
                                })
                                .when_some(shortcut_hint, |el, hint| {
                                    el.child(
                                        div()
                                            .px_1p5()
                                            .py_0p5()
                                            .rounded(RADIUS_CHIP)
                                            .bg(theme::bg_surface())
                                            .text_xs()
                                            .text_color(theme::text_muted())
                                            .child(hint),
                                    )
                                }),
                        ),
                );
            }
        }

        v_flex()
            .w(px(280.0))
            .h_full()
            .bg(theme::bg_sidebar())
            .border_r_1()
            .border_color(theme::border_subtle())
            .p_3()
            .gap_3()
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .px_3()
                            .py_1p5()
                            .rounded(RADIUS_CHIP)
                            .bg(theme::bg_surface())
                            .border_1()
                            .border_color(theme::border_subtle())
                            .text_sm()
                            .text_color(theme::text_primary())
                            .child("Multibot"),
                    )
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(
                                Button::new("open-palette-btn")
                                    .xsmall()
                                    .label("⌘K")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_palette(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("new-window")
                                    .xsmall()
                                    .label("+")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.open_extra_window(cx);
                                    })),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .gap_1p5()
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::text_muted())
                                    .child("HOSTS"),
                            )
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(
                                        Button::new("connect-local")
                                            .xsmall()
                                            .label("Local")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                if let Some(bin) = Self::find_local_daemon() {
                                                    this.connect_to_target(
                                                        "localhost".into(),
                                                        DaemonTarget::LocalBinary(bin),
                                                        cx,
                                                    );
                                                } else {
                                                    this.notice = Some(
                                                        "No local multibotd beside multibot.exe — connect a VPS host in Settings (Ctrl+,)."
                                                            .into(),
                                                    );
                                                    cx.notify();
                                                }
                                            })),
                                    )
                                    .child(
                                        Button::new("import-zigshel-sidebar")
                                            .xsmall()
                                            .label("Import")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.import_from_zigshel(cx);
                                            })),
                                    ),
                            ),
                    )
                    .child(host_list),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .gap_1p5()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::text_muted())
                            .child(format!("RECENTS ({})", self.sessions.len())),
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
                div()
                    .id("gpu-status-card")
                    .px_3()
                    .py_2()
                    .rounded(RADIUS_ROW)
                    .border_1()
                    .border_color(if gpu_safe {
                        theme::accent_green()
                    } else {
                        theme::accent_amber()
                    })
                    .bg(theme::bg_surface())
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.main_surface = MainSurface::Settings;
                            this.active_modal = ActiveModal::None;
                            cx.notify();
                        }),
                    )
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(if gpu_safe {
                                        theme::accent_green()
                                    } else {
                                        theme::accent_amber()
                                    })
                                    .child(format!("GPU: {gpu_label}")),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::text_muted())
                                    .child(format!(
                                        "{:.1} MB · {:.1}% CPU · {}",
                                        self.footprint.memory_mb,
                                        self.footprint.cpu_percent,
                                        self.connection_status
                                    )),
                            ),
                    ),
            )
    }

    fn render_top_floating_bar(&self, title: String, cx: &mut Context<Self>) -> impl IntoElement {
        let is_inbox = self.main_surface == MainSurface::Inbox;
        let is_settings = self.main_surface == MainSurface::Settings;

        h_flex()
            .w_full()
            .px_5()
            .pt_3()
            .pb_2()
            .items_center()
            .justify_between()
            .child(
                Button::new("toggle-sidebar-btn")
                    .xsmall()
                    .label(if self.sidebar_visible { "Sidebar" } else { "Show Sidebar" })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sidebar_visible = !this.sidebar_visible;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .px_4()
                    .py_1p5()
                    .rounded(RADIUS_CHIP)
                    .bg(theme::bg_surface())
                    .border_1()
                    .border_color(theme::border_subtle())
                    .text_sm()
                    .text_color(theme::text_primary())
                    .child(title),
            )
            .child(
                h_flex()
                    .gap_1p5()
                    .child({
                        let b = Button::new("toggle-inbox")
                            .xsmall()
                            .label(format!("Inbox ({})", self.sessions.len()))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.main_surface = if this.main_surface == MainSurface::Inbox {
                                    MainSurface::Chat
                                } else {
                                    MainSurface::Inbox
                                };
                                cx.notify();
                            }));
                        if is_inbox { b.primary() } else { b }
                    })
                    .child({
                        let b = Button::new("toggle-settings")
                            .xsmall()
                            .label("GPU / Settings")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.main_surface = if this.main_surface == MainSurface::Settings {
                                    MainSurface::Chat
                                } else {
                                    MainSurface::Settings
                                };
                                cx.notify();
                            }));
                        if is_settings { b.primary() } else { b }
                    })
                    .child(
                        Button::new("toggle-shortcuts")
                            .xsmall()
                            .label("F1")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.active_modal = match this.active_modal {
                                    ActiveModal::Shortcuts => ActiveModal::None,
                                    _ => ActiveModal::Shortcuts,
                                };
                                cx.notify();
                            })),
                    ),
            )
    }

    fn render_chat_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let header_title = match &self.selected_session {
            Some(s) => format!("{}  {}", s.agent.glyph(), s.title),
            None => "Multibot · Ready".to_string(),
        };

        let mut msg_col = v_flex().w_full().max_w(px(760.0)).mx_auto().gap_3().py_4().px_4();
        if self.messages.is_empty() {
            msg_col = msg_col.child(
                v_flex()
                    .p_5()
                    .gap_2()
                    .rounded(RADIUS_BUBBLE)
                    .bg(theme::bg_surface())
                    .border_1()
                    .border_color(theme::border_subtle())
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme::text_primary())
                            .child("Multibot — Obsidian Dark GPUI Shell (Slice 1)"),
                    )
                    .child(
                        TextView::markdown(
                            "welcome-md",
                            "- **Ctrl+K / Ctrl+Shift+K** — Open Command Palette\n\
                             - **Alt+I / Ctrl+Shift+I** — Open Cross-workspace Inbox\n\
                             - **Ctrl+,** — GPU Picker (0% NVIDIA verified) & VPS SSH hosts\n\
                             - **Ctrl+Shift+B** — Toggle sidebar · **F1** — All shortcuts",
                        ),
                    ),
            );
        } else {
            for (idx, msg) in self.messages.iter().enumerate() {
                let is_user = msg.role == MessageRole::User;
                if is_user {
                    msg_col = msg_col.child(
                        h_flex().w_full().justify_end().child(
                            div()
                                .max_w(px(560.0))
                                .px_4()
                                .py_2p5()
                                .rounded(RADIUS_BUBBLE)
                                .bg(theme::bg_user_bubble())
                                .text_sm()
                                .text_color(theme::text_primary())
                                .child(msg.text.clone()),
                        ),
                    );
                } else {
                    msg_col = msg_col.child(
                        v_flex()
                            .w_full()
                            .px_4()
                            .py_3()
                            .gap_1()
                            .rounded(RADIUS_BUBBLE)
                            .bg(theme::bg_surface())
                            .border_1()
                            .border_color(theme::border_subtle())
                            .child(TextView::markdown(
                                SharedString::from(format!("msg-md-{idx}")),
                                msg.text.clone(),
                            )),
                    );
                }
            }
        }

        v_flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .child(self.render_top_floating_bar(header_title, cx))
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
                        .w_full()
                        .max_w(px(760.0))
                        .mx_auto()
                        .px_4()
                        .pb_1()
                        .text_xs()
                        .text_color(theme::text_muted())
                        .child(note),
                )
            })
            .child(
                div().w_full().px_5().pb_4().pt_2().child(
                    h_flex()
                        .w_full()
                        .max_w(px(760.0))
                        .mx_auto()
                        .px_3()
                        .py_1p5()
                        .gap_2()
                        .items_center()
                        .rounded(RADIUS_CHIP)
                        .bg(theme::bg_surface())
                        .border_1()
                        .border_color(theme::border_subtle())
                        .child(
                            div()
                                .w(px(28.0))
                                .h(px(28.0))
                                .rounded(RADIUS_CHIP)
                                .bg(theme::bg_hover())
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_sm()
                                .text_color(theme::text_secondary())
                                .child("+"),
                        )
                        .child(
                            div()
                                .flex_1()
                                .child(Input::new(&self.prompt_input).id("prompt-input").appearance(false)),
                        )
                        .child(
                            Button::new("send-prompt")
                                .small()
                                .primary()
                                .label("Send")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.send_current_prompt(window, cx);
                                })),
                        ),
                ),
            )
    }

    fn render_inbox_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let count_all = self.sessions.len();
        let count_claude = self
            .sessions
            .iter()
            .filter(|s| s.agent == AgentKind::Claude)
            .count();
        let count_agy = self
            .sessions
            .iter()
            .filter(|s| s.agent == AgentKind::Antigravity)
            .count();
        let count_codex = self
            .sessions
            .iter()
            .filter(|s| s.agent == AgentKind::Codex)
            .count();

        let tabs = [
            ("inbox-tab-all", format!("All  {count_all}"), InboxFilter::All),
            (
                "inbox-tab-claude",
                format!("Claude  {count_claude}"),
                InboxFilter::Agent(AgentKind::Claude),
            ),
            (
                "inbox-tab-agy",
                format!("Antigravity  {count_agy}"),
                InboxFilter::Agent(AgentKind::Antigravity),
            ),
            (
                "inbox-tab-codex",
                format!("Codex  {count_codex}"),
                InboxFilter::Agent(AgentKind::Codex),
            ),
        ];

        let mut tab_row = h_flex().gap_2().items_center();
        for (id, label, filter) in tabs {
            let active = self.inbox_filter == filter;
            tab_row = tab_row.child(
                div()
                    .id(id)
                    .role(Role::Button)
                    .test_support()
                    .px_3p5()
                    .py_1p5()
                    .rounded(RADIUS_CHIP)
                    .bg(if active {
                        theme::bg_active_row()
                    } else {
                        theme::bg_canvas()
                    })
                    .border_1()
                    .border_color(if active {
                        theme::border_subtle()
                    } else {
                        theme::bg_canvas()
                    })
                    .text_xs()
                    .text_color(if active {
                        theme::text_primary()
                    } else {
                        theme::text_muted()
                    })
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.inbox_filter = filter;
                            cx.notify();
                        }),
                    )
                    .child(label),
            );
        }

        let filtered: Vec<SessionSummary> = self
            .sessions
            .iter()
            .filter(|s| match self.inbox_filter {
                InboxFilter::All => true,
                InboxFilter::Agent(kind) => s.agent == kind,
            })
            .cloned()
            .collect();

        let mut rows = v_flex().gap_1p5();
        if filtered.is_empty() {
            rows = rows.child(
                div()
                    .p_5()
                    .rounded(RADIUS_BUBBLE)
                    .bg(theme::bg_surface())
                    .text_sm()
                    .text_color(theme::text_muted())
                    .child("No sessions in this Inbox split yet. Connect a VPS host from the sidebar."),
            );
        } else {
            for (idx, sess) in filtered.into_iter().enumerate() {
                let sess_click = sess.clone();
                rows = rows.child(
                    div()
                        .id(SharedString::from(format!("inbox-row-{idx}")))
                        .px_4()
                        .py_3()
                        .rounded(RADIUS_ROW)
                        .bg(theme::bg_surface())
                        .hover(|s| s.bg(theme::bg_hover()))
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                this.select_session(sess_click.clone(), cx);
                            }),
                        )
                        .child(
                            h_flex()
                                .items_center()
                                .gap_3()
                                .child(
                                    div()
                                        .w(px(30.0))
                                        .h(px(30.0))
                                        .rounded(RADIUS_CHIP)
                                        .bg(Self::agent_color(sess.agent))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .text_xs()
                                        .text_color(theme::bg_canvas())
                                        .child(sess.agent.glyph()),
                                )
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .child(
                                            div()
                                                .text_sm()
                                                .text_color(theme::text_primary())
                                                .child(sess.title),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(theme::text_muted())
                                                .child(
                                                    sess.cwd
                                                        .unwrap_or_else(|| sess.agent.label().into()),
                                                ),
                                        ),
                                )
                                .when(sess.working, |el| {
                                    el.child(
                                        div()
                                            .px_2()
                                            .py_0p5()
                                            .rounded(RADIUS_CHIP)
                                            .bg(theme::bg_hover())
                                            .text_xs()
                                            .text_color(theme::accent_green())
                                            .child("Working"),
                                    )
                                }),
                        ),
                );
            }
        }

        v_flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .child(self.render_top_floating_bar("Inbox".into(), cx))
            .child(
                div()
                    .id("inbox-scroll")
                    .flex_1()
                    .overflow_y_scroll()
                    .child(
                        v_flex()
                            .w_full()
                            .max_w(px(800.0))
                            .mx_auto()
                            .p_5()
                            .gap_4()
                            .child(tab_row)
                            .child(rows),
                    ),
            )
    }

    fn render_settings_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
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

    fn render_palette_modal(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.palette_input.read(cx).value().to_string().to_ascii_lowercase();

        let mut list = v_flex().gap_1().p_2();

        // Matching sessions first
        let matching_sessions: Vec<(usize, SessionSummary)> = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                query.is_empty()
                    || s.title.to_ascii_lowercase().contains(&query)
                    || s.agent.label().to_ascii_lowercase().contains(&query)
            })
            .take(8)
            .map(|(i, s)| (i, s.clone()))
            .collect();

        if !matching_sessions.is_empty() {
            list = list.child(
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(theme::text_muted())
                    .child("RECENT SESSIONS"),
            );
            for (idx, sess) in matching_sessions {
                let sess_clone = sess.clone();
                let key_badge = if idx < 9 {
                    Some(format!("^{}", idx + 1))
                } else {
                    None
                };
                list = list.child(
                    div()
                        .id(SharedString::from(format!("pal-sess-{idx}")))
                        .px_3()
                        .py_2()
                        .rounded(RADIUS_ROW)
                        .hover(|s| s.bg(theme::bg_hover()))
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                this.select_session(sess_clone.clone(), cx);
                            }),
                        )
                        .child(
                            h_flex()
                                .justify_between()
                                .items_center()
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .items_center()
                                        .child(
                                            div()
                                                .w(px(22.0))
                                                .h(px(22.0))
                                                .rounded(RADIUS_CHIP)
                                                .bg(Self::agent_color(sess.agent))
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .text_xs()
                                                .text_color(theme::bg_canvas())
                                                .child(sess.agent.glyph()),
                                        )
                                        .child(
                                            div()
                                                .text_sm()
                                                .text_color(theme::text_primary())
                                                .child(sess.title),
                                        ),
                                )
                                .when_some(key_badge, |el, badge| {
                                    el.child(
                                        div()
                                            .px_2()
                                            .py_0p5()
                                            .rounded(RADIUS_CHIP)
                                            .bg(theme::bg_hover())
                                            .text_xs()
                                            .text_color(theme::text_muted())
                                            .child(badge),
                                    )
                                }),
                        ),
                );
            }
        }

        // Core commands
        let commands: [(&str, &str, &str); 6] = [
            ("pal-cmd-inbox", "Open Cross-workspace Inbox", "Alt+I"),
            ("pal-cmd-settings", "Open GPU & VPS Settings", "Ctrl+,"),
            ("pal-cmd-sidebar", "Toggle Sidebar", "Ctrl+Shift+B"),
            ("pal-cmd-window", "New Multibot Window", "Ctrl+N"),
            ("pal-cmd-import", "Import SSH Hosts from ZigShell (read-only)", ""),
            ("pal-cmd-shortcuts", "Show Keyboard Shortcuts", "F1"),
        ];

        list = list.child(
            div()
                .px_3()
                .pt_2()
                .pb_1()
                .text_xs()
                .text_color(theme::text_muted())
                .child("COMMANDS"),
        );

        for (id, label, chord) in commands {
            if !query.is_empty() && !label.to_ascii_lowercase().contains(&query) {
                continue;
            }
            list = list.child(
                div()
                    .id(id)
                    .role(Role::Button)
                    .test_support()
                    .px_3()
                    .py_2()
                    .rounded(RADIUS_ROW)
                    .hover(|s| s.bg(theme::bg_hover()))
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.active_modal = ActiveModal::None;
                            match id {
                                "pal-cmd-inbox" => this.main_surface = MainSurface::Inbox,
                                "pal-cmd-settings" => this.main_surface = MainSurface::Settings,
                                "pal-cmd-sidebar" => this.sidebar_visible = !this.sidebar_visible,
                                "pal-cmd-window" => this.open_extra_window(cx),
                                "pal-cmd-import" => this.import_from_zigshel(cx),
                                "pal-cmd-shortcuts" => this.active_modal = ActiveModal::Shortcuts,
                                _ => {}
                            }
                            cx.notify();
                        }),
                    )
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .child(div().text_sm().text_color(theme::text_primary()).child(label))
                            .when(!chord.is_empty(), |el| {
                                el.child(
                                    div()
                                        .px_2()
                                        .py_0p5()
                                        .rounded(RADIUS_CHIP)
                                        .bg(theme::bg_hover())
                                        .text_xs()
                                        .text_color(theme::text_muted())
                                        .child(chord),
                                )
                            }),
                    ),
            );
        }

        div()
            .id("palette-backdrop")
            .absolute()
            .inset_0()
            .bg(hsla(0.0, 0.0, 0.0, 0.58))
            .flex()
            .items_start()
            .justify_center()
            .pt(px(88.0))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.active_modal = ActiveModal::None;
                    cx.notify();
                }),
            )
            .child(
                v_flex()
                    .id("palette-modal")
                    .w(px(560.0))
                    .max_h(px(520.0))
                    .rounded(RADIUS_MODAL)
                    .bg(theme::bg_surface())
                    .border_1()
                    .border_color(theme::border_subtle())
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .child(
                        div()
                            .px_3()
                            .py_2p5()
                            .border_b_1()
                            .border_color(theme::border_subtle())
                            .child(
                                Input::new(&self.palette_input)
                                    .id("palette-search")
                                    .appearance(false),
                            ),
                    )
                    .child(
                        div()
                            .id("palette-list-scroll")
                            .flex_1()
                            .overflow_y_scroll()
                            .child(list),
                    ),
            )
    }

    fn render_shortcuts_modal(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut rows = v_flex().gap_1().p_3();
        for spec in SHORTCUTS {
            rows = rows.child(
                h_flex()
                    .px_3()
                    .py_1p5()
                    .rounded(RADIUS_ROW)
                    .justify_between()
                    .items_center()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme::text_primary())
                                    .child(spec.description),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(if spec.wired {
                                        theme::accent_green()
                                    } else {
                                        theme::text_muted()
                                    })
                                    .child(if spec.wired { "active" } else { "planned" }),
                            ),
                    )
                    .child(
                        div()
                            .px_2p5()
                            .py_0p5()
                            .rounded(RADIUS_CHIP)
                            .bg(theme::bg_hover())
                            .text_xs()
                            .text_color(theme::accent_blue())
                            .child(spec.display),
                    ),
            );
        }

        div()
            .id("shortcuts-backdrop")
            .absolute()
            .inset_0()
            .bg(hsla(0.0, 0.0, 0.0, 0.58))
            .flex()
            .items_start()
            .justify_center()
            .pt(px(64.0))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.active_modal = ActiveModal::None;
                    cx.notify();
                }),
            )
            .child(
                v_flex()
                    .id("shortcuts-modal")
                    .w(px(640.0))
                    .max_h(px(580.0))
                    .rounded(RADIUS_MODAL)
                    .bg(theme::bg_surface())
                    .border_1()
                    .border_color(theme::border_subtle())
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .child(
                        h_flex()
                            .px_4()
                            .py_3()
                            .border_b_1()
                            .border_color(theme::border_subtle())
                            .justify_between()
                            .items_center()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme::text_primary())
                                    .child("Keyboard Shortcuts (Preserved Multibot Contract)"),
                            )
                            .child(
                                Button::new("close-shortcuts")
                                    .xsmall()
                                    .label("Esc")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.active_modal = ActiveModal::None;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .id("shortcuts-scroll")
                            .flex_1()
                            .overflow_y_scroll()
                            .child(rows),
                    ),
            )
    }
}

impl Render for MultibotView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("multibot-root")
            .relative()
            .size_full()
            .bg(theme::bg_canvas())
            .text_color(theme::text_primary())
            .on_action(cx.listener(|this, _: &OpenPalette, window, cx| {
                this.open_palette(window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenInbox, _, cx| {
                this.active_modal = ActiveModal::None;
                this.main_surface = MainSurface::Inbox;
                this.inbox_filter = InboxFilter::All;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleInbox, _, cx| {
                this.active_modal = ActiveModal::None;
                this.main_surface = if this.main_surface == MainSurface::Inbox {
                    MainSurface::Chat
                } else {
                    MainSurface::Inbox
                };
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleSettings, _, cx| {
                this.active_modal = ActiveModal::None;
                this.main_surface = if this.main_surface == MainSurface::Settings {
                    MainSurface::Chat
                } else {
                    MainSurface::Settings
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
            .on_action(cx.listener(|this, _: &OpenShortcuts, _, cx| {
                this.active_modal = match this.active_modal {
                    ActiveModal::Shortcuts => ActiveModal::None,
                    _ => ActiveModal::Shortcuts,
                };
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CloseOverlay, _, cx| {
                if this.active_modal != ActiveModal::None {
                    this.active_modal = ActiveModal::None;
                } else if this.main_surface != MainSurface::Chat {
                    this.main_surface = MainSurface::Chat;
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &NextSession, _, cx| {
                this.cycle_session(1, cx);
            }))
            .on_action(cx.listener(|this, _: &PrevSession, _, cx| {
                this.cycle_session(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &JumpSession1, _, cx| this.jump_session_index(0, cx)))
            .on_action(cx.listener(|this, _: &JumpSession2, _, cx| this.jump_session_index(1, cx)))
            .on_action(cx.listener(|this, _: &JumpSession3, _, cx| this.jump_session_index(2, cx)))
            .on_action(cx.listener(|this, _: &JumpSession4, _, cx| this.jump_session_index(3, cx)))
            .on_action(cx.listener(|this, _: &JumpSession5, _, cx| this.jump_session_index(4, cx)))
            .on_action(cx.listener(|this, _: &JumpSession6, _, cx| this.jump_session_index(5, cx)))
            .on_action(cx.listener(|this, _: &JumpSession7, _, cx| this.jump_session_index(6, cx)))
            .on_action(cx.listener(|this, _: &JumpSession8, _, cx| this.jump_session_index(7, cx)))
            .on_action(cx.listener(|this, _: &JumpSession9, _, cx| this.jump_session_index(8, cx)))
            .child(
                h_flex()
                    .size_full()
                    .when(self.sidebar_visible, |el| el.child(self.render_sidebar(cx)))
                    .child(match self.main_surface {
                        MainSurface::Chat => self.render_chat_panel(cx).into_any_element(),
                        MainSurface::Inbox => self.render_inbox_panel(cx).into_any_element(),
                        MainSurface::Settings => self.render_settings_panel(cx).into_any_element(),
                    }),
            )
            .when(self.active_modal == ActiveModal::Palette, |el| {
                el.child(self.render_palette_modal(cx))
            })
            .when(self.active_modal == ActiveModal::Shortcuts, |el| {
                el.child(self.render_shortcuts_modal(cx))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{TestAppContext, test::TestWindowExt};

    fn open_test_view(cx: &mut TestAppContext) -> gpui_kit::AnyWindowHandle {
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
        let (handle, _view) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, move |window, cx| {
                cx.new(|cx| MultibotView::new(rt, window, cx))
            })
            .unwrap()
        });
        handle
    }

    #[gpui_kit::test]
    fn renders_dark_shell_and_toggles_gpu_settings(cx: &mut TestAppContext) {
        let handle = open_test_view(cx);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("toggle-settings", cx);
            window.render_frame(cx);
            window.click("gpu-mode-software", cx);
            window.render_frame(cx);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn opens_command_palette_and_inbox_and_shortcuts(cx: &mut TestAppContext) {
        let handle = open_test_view(cx);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("open-palette-btn", cx);
            window.render_frame(cx);
            window.click("pal-cmd-inbox", cx);
            window.render_frame(cx);
            window.click("inbox-tab-claude", cx);
            window.render_frame(cx);
            window.click("toggle-shortcuts", cx);
            window.render_frame(cx);
            window.click("close-shortcuts", cx);
            window.render_frame(cx);
            window.click("toggle-sidebar-btn", cx);
            window.render_frame(cx);
        })
        .unwrap();
    }
}
