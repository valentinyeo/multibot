use crate::app::MultibotView;
use crate::state::{ActiveModal, MainSurface};
use crate::theme;
use anyhow::{Result, bail};
use gpui_kit::{
    Context, InteractiveElement, IntoElement, ParentElement, Styled, TestSupportExt, Window,
    component::{Sizable, button::Button, h_flex, v_flex},
    div,
    prelude::FluentBuilder,
    px,
};
use multibot_core::{SshHostConfig, daemon_client::validate_ssh_host};
use multibot_proto::AgentKind;
use std::collections::HashMap;

pub const SHELL_HEADER_HEIGHT: f32 = 44.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellLaunchSpec {
    pub program: String,
    pub args: Vec<String>,
}

impl ShellLaunchSpec {
    pub fn from_host_and_session(host: &SshHostConfig, session_name: &str) -> Result<Self> {
        validate_ssh_host(host)?;
        if session_name.is_empty() || session_name.chars().any(char::is_control) {
            bail!("Invalid tmux session name");
        }
        let mut args = vec![
            "-t".into(),
            "-o".into(),
            "ServerAliveInterval=30".into(),
            "-o".into(),
            "ServerAliveCountMax=3".into(),
            "-o".into(),
            "StrictHostKeyChecking=accept-new".into(),
            "-p".into(),
            host.port.to_string(),
        ];
        if let Some(key) = host.validated_identity_file()? {
            args.extend(["-i".into(), key.into()]);
        }
        args.extend([
            "--".into(),
            host.ssh_target(),
            format!(
                "tmux attach-session -t '={}'",
                session_name.replace('\'', "'\\''")
            ),
        ]);
        Ok(Self {
            program: "ssh.exe".into(),
            args,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShellViewportRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}
impl ShellViewportRect {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn from_window(width: f32, height: f32, sidebar: f32, header: f32, scale: f32) -> Self {
        Self::from_bounds(
            sidebar,
            header,
            (width - sidebar).max(0.0),
            (height - header).max(0.0),
            scale,
        )
    }
    pub fn from_bounds(x: f32, y: f32, width: f32, height: f32, scale: f32) -> Self {
        let pixels = |n: f32| (n.max(0.0) * scale.max(0.0)).round() as i32;
        let x1 = pixels(x);
        let y1 = pixels(y);
        Self {
            x: x1,
            y: y1,
            width: (pixels(x + width.max(0.0)) - x1).max(0),
            height: (pixels(y + height.max(0.0)) - y1).max(0),
        }
    }
}

pub(crate) fn should_show_embedded_shell(
    surface: MainSurface,
    modal: ActiveModal,
    agent: Option<AgentKind>,
    find_open: bool,
    rename_open: bool,
) -> bool {
    surface == MainSurface::Chat
        && modal == ActiveModal::None
        && agent == Some(AgentKind::Shell)
        && !find_open
        && !rename_open
}

#[cfg(any(windows, test))]
fn forwarded_shortcut(key: u32, ctrl: bool, alt: bool, shift: bool) -> Option<&'static str> {
    registered_native_shortcut(key, ctrl, alt, shift)
        .filter(|spec| spec.category != multibot_keys::ShortcutCategory::Browser)
        .map(|spec| spec.gpui_key)
}

#[cfg(any(windows, test))]
pub(crate) fn registered_native_shortcut(
    key: u32,
    ctrl: bool,
    alt: bool,
    shift: bool,
) -> Option<&'static multibot_keys::ShortcutSpec> {
    // Keep interrupt, EOF, suspend, and Escape terminal-owned even if the app binds them.
    if (ctrl && alt)
        || (ctrl && !alt && !shift && matches!(key, 0x43 | 0x44 | 0x5A))
        || key == 0x1B
    {
        return None;
    }
    let name = match key {
        0x30..=0x39 | 0x41..=0x5A => char::from_u32(key)?.to_ascii_lowercase().to_string(),
        0xBC => ",".into(),
        0xBD => "-".into(),
        0xBB => "=".into(),
        0xBA => ";".into(),
        0xBF => "/".into(),
        0xDB => "[".into(),
        0xDD => "]".into(),
        0x09 => "tab".into(),
        0x20 => "space".into(),
        0x25 => "left".into(),
        0x27 => "right".into(),
        0x26 => "up".into(),
        0x28 => "down".into(),
        0x70 => "f1".into(),
        0x71 => "f2".into(),
        0x1B => "escape".into(),
        _ => return None,
    };
    let chord = format!(
        "{}{}{}{name}",
        if ctrl { "ctrl-" } else { "" },
        if alt { "alt-" } else { "" },
        if shift { "shift-" } else { "" }
    );
    // Only app-owned bindings escape the console. Unbound Ctrl+C/L stay terminal-owned.
    multibot_keys::by_gpui_key(&chord).filter(|spec| spec.wired)
}

#[cfg(any(windows, test))]
fn windows_quote(arg: &str) -> String {
    let mut out = String::from("\"");
    let mut slashes = 0;
    for c in arg.chars() {
        if c == '\\' {
            slashes += 1;
            continue;
        }
        if c == '"' {
            out.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
        } else {
            out.extend(std::iter::repeat_n('\\', slashes));
        }
        out.push(c);
        slashes = 0;
    }
    out.extend(std::iter::repeat_n('\\', slashes * 2));
    out.push('"');
    out
}

enum ShellState {
    #[cfg(windows)]
    Connecting(u64),
    #[cfg(windows)]
    Connected(native::EmbeddedShellHandle),
    Disconnected(String),
}

#[derive(Default)]
pub(crate) struct ShellHost {
    entries: HashMap<String, ShellState>,
    #[cfg(windows)]
    generation: u64,
    #[cfg(windows)]
    native: Option<native::NativeHost>,
}

impl ShellHost {
    fn status(&self, key: &str) -> String {
        match self.entries.get(key) {
            #[cfg(windows)]
            Some(ShellState::Connecting(_)) => "Connecting to server shell...".into(),
            #[cfg(windows)]
            Some(ShellState::Connected(_)) => "Connected in native shell".into(),
            Some(ShellState::Disconnected(error)) => {
                format!("Disconnected — click Reconnect · {error}")
            }
            None => "Connecting to server shell...".into(),
        }
    }

    pub(crate) fn prepare_selection(&mut self, key: &str) {
        if matches!(self.entries.get(key), Some(ShellState::Disconnected(_))) {
            self.entries.remove(key);
        }
    }

    pub(crate) fn focus_parent(&self) {
        #[cfg(windows)]
        if let Some(host) = &self.native {
            host.focus_parent();
        }
    }

    pub(crate) fn reconcile(&mut self, active: Option<&str>, rect: Option<ShellViewportRect>) {
        #[cfg(windows)]
        if let Some(host) = &self.native {
            for (key, state) in &mut self.entries {
                if let ShellState::Connected(handle) = state {
                    handle.reconcile(host.parent, key.as_str() == active.unwrap_or(""), rect);
                }
            }
        }
        #[cfg(not(windows))]
        let _ = (active, rect);
    }

    pub(crate) fn retain_live(&mut self, keys: &[String]) {
        self.entries.retain(|key, _| keys.contains(key));
    }
}

impl MultibotView {
    pub(crate) fn reconnect_embedded_shell(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = &self.selected_session {
            let key = self.session_key_for(session);
            self.shell_host.entries.remove(&key);
            cx.notify();
        }
    }

    pub(crate) fn sync_embedded_shell(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let keys: Vec<_> = if self.host_sessions.is_empty() {
            self.sessions
                .iter()
                .filter(|s| s.agent == AgentKind::Shell)
                .map(|s| self.session_key_for(s))
                .collect()
        } else {
            self.host_sessions
                .iter()
                .filter(|(_, s)| s.agent == AgentKind::Shell)
                .map(|(h, s)| multibot_core::workspaces::canonical_session_key(h, s.agent, &s.id))
                .collect()
        };
        self.shell_host.retain_live(&keys);
        let visible = should_show_embedded_shell(
            self.main_surface,
            self.active_modal,
            self.selected_session.as_ref().map(|s| s.agent),
            self.chat_state.find_open,
            self.sidebar_state.rename.is_some() || self.sidebar_state.new_workspace_draft.is_some(),
        ) && !self.table_modal_state.is_open()
            && !self.computer.is_open()
            && !self.slash_picker_state.is_open()
            && !(self.sidebar_rendered() && self.sidebar_state.dev_stats_expanded);
        let active = visible
            .then(|| {
                self.selected_session
                    .as_ref()
                    .map(|s| self.session_key_for(s))
            })
            .flatten();
        self.shell_host.reconcile(active.as_deref(), None);
        let Some(key) = active else { return };
        if !keys.contains(&key) {
            self.shell_host
                .entries
                .insert(key, ShellState::Disconnected("Server session ended".into()));
            return;
        }
        if self.shell_host.entries.contains_key(&key) {
            return;
        }
        let session = self.selected_session.as_ref().unwrap();
        let host_label = self
            .selected_session_host
            .as_deref()
            .or(self.connected_host.as_deref());
        let host = self
            .settings
            .ssh_hosts
            .iter()
            .find(|h| Some(h.name.as_str()) == host_label)
            .or_else(|| {
                cx.global::<crate::state::SharedAppStore>()
                    .host_connections
                    .get(host_label.unwrap_or(""))
                    .and_then(|c| {
                        if let multibot_core::DaemonTarget::RemoteSsh { host, .. } = &c.target {
                            Some(host)
                        } else {
                            None
                        }
                    })
            });
        let spec = host
            .ok_or_else(|| anyhow::anyhow!("No SSH host configured for this shell"))
            .and_then(|host| ShellLaunchSpec::from_host_and_session(host, &session.id));
        let spec = match spec {
            Ok(spec) => spec,
            Err(err) => {
                self.shell_host
                    .entries
                    .insert(key, ShellState::Disconnected(err.to_string()));
                return;
            }
        };
        #[cfg(not(windows))]
        {
            let _ = (spec, window, cx);
            self.shell_host.entries.insert(
                key,
                ShellState::Disconnected("Native embedded shells require Windows".into()),
            );
        }
        #[cfg(windows)]
        {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            let parent = match HasWindowHandle::window_handle(window).map(|h| h.as_raw()) {
                Ok(RawWindowHandle::Win32(h)) => h.hwnd.get() as usize,
                _ => {
                    self.shell_host.entries.insert(
                        key,
                        ShellState::Disconnected("No Windows window handle".into()),
                    );
                    return;
                }
            };
            if self.shell_host.native.is_none() {
                let (shortcuts, mut shortcut_rx) = tokio::sync::mpsc::unbounded_channel();
                match native::NativeHost::new(parent, shortcuts) {
                    Ok(host) => self.shell_host.native = Some(host),
                    Err(err) => {
                        self.shell_host
                            .entries
                            .insert(key, ShellState::Disconnected(err.to_string()));
                        return;
                    }
                }
                let weak = cx.entity().downgrade();
                window
                    .spawn(cx, async move |cx| {
                        while let Some(key) = shortcut_rx.recv().await {
                            if cx
                                .update(|window, cx| {
                                    if let Some(view) = weak.upgrade() {
                                        view.read(cx).root_focus_handle.clone().focus(window, cx);
                                        window.dispatch_keystroke(key, cx);
                                    }
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                    })
                    .detach();
            }
            self.shell_host.generation += 1;
            let generation = self.shell_host.generation;
            self.shell_host
                .entries
                .insert(key.clone(), ShellState::Connecting(generation));
            let job = self.shell_host.native.as_ref().unwrap().job.clone();
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
            let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
            let spawn = std::thread::Builder::new()
                .name("native-shell".into())
                .spawn(move || {
                    let result = native::spawn(spec, job, parent);
                    let process = result.as_ref().ok().map(|h| h.process.clone());
                    if ready_tx.send(result).is_err() {
                        return;
                    }
                    if let Some(process) = process {
                        process.wait();
                    }
                    let _ = exit_tx.send(());
                });
            if let Err(error) = spawn {
                self.shell_host
                    .entries
                    .insert(key, ShellState::Disconnected(error.to_string()));
                return;
            }
            cx.spawn(async move |this, cx| {
                let Ok(result) = ready_rx.await else { return };
                let _ = this.update(cx, |view, cx| {
                    if matches!(view.shell_host.entries.get(&key), Some(ShellState::Connecting(g)) if *g == generation) {
                        view.shell_host.entries.insert(key.clone(), match result {
                            Ok(mut handle) => { handle.generation = generation; ShellState::Connected(handle) }, Err(error) => ShellState::Disconnected(error.to_string()),
                        });
                        cx.notify();
                    }
                });
                if exit_rx.await.is_ok() {
                    let _ = this.update(cx, |view, cx| {
                        if matches!(view.shell_host.entries.get(&key), Some(ShellState::Connected(h)) if h.generation == generation) {
                            view.shell_host.entries.insert(key, ShellState::Disconnected("SSH exited".into()));
                            cx.notify();
                        }
                    });
                }
            }).detach();
        }
    }

    pub(crate) fn render_shell_panel(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let session = self.selected_session.as_ref().unwrap();
        let key = self.session_key_for(session);
        let title = self
            .sidebar_state
            .workspace_store
            .effective_title(&key, &session.title)
            .to_string();
        let host = self
            .selected_session_host
            .as_deref()
            .or(self.connected_host.as_deref())
            .unwrap_or("server");
        let workspace = self
            .sidebar_state
            .workspace_store
            .session_meta
            .get(&key)
            .and_then(|m| m.workspace_id.as_ref())
            .and_then(|id| {
                self.sidebar_state
                    .workspace_store
                    .workspaces
                    .iter()
                    .find(|w| &w.id == id)
            })
            .map(|w| w.name.clone())
            .unwrap_or_default();
        let header_rename = self
            .sidebar_state
            .rename
            .as_ref()
            .filter(|r| {
                r.session_key == key && self.sidebar_state.mode != multibot_core::SidebarMode::Open
            })
            .map(|r| r.draft.clone());
        let body_index = 1 + usize::from(self.chat_state.find_open);
        let active_key = key.clone();
        let weak = cx.entity().downgrade();
        v_flex()
            .w_full()
            .when(!self.settings.shell_fullscreen, |el| el.max_w(px(760.0)))
            .mx_auto()
            .min_w_0()
            .h_full()
            .bg(theme::bg_canvas())
            .when_some(self.chat_state.focus_handle.as_ref(), |el, handle| {
                el.track_focus(handle)
            })
            .on_key_down(
                cx.listener(|this, event, window, cx| this.handle_chat_key_down(event, window, cx)),
            )
            .child(
                h_flex()
                    .h(px(SHELL_HEADER_HEIGHT))
                    .flex_shrink_0()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .on_mouse_down(
                        gpui_kit::MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            this.shell_host.focus_parent();
                            this.root_focus_handle.focus(window, cx);
                        }),
                    )
                    .child(if let Some(draft) = header_rename {
                        div()
                            .id("header-rename-input")
                            .text_sm()
                            .child(format!("{draft}|"))
                    } else {
                        div()
                            .id("shell-title")
                            .text_sm()
                            .max_w(px(250.0))
                            .truncate()
                            .child(title)
                    })
                    .child(
                        div()
                            .text_xs()
                            .max_w(px(320.0))
                            .truncate()
                            .text_color(theme::text_muted())
                            .child(format!("{host} · {}", session.id)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .rounded(theme::RADIUS_CHIP)
                            .bg(theme::bg_surface())
                            .px_2()
                            .py_1()
                            .text_color(theme::text_secondary())
                            .child(workspace),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("shell-reconnect")
                            .small()
                            .label("Reconnect")
                            .on_click(
                                cx.listener(|this, _, _, cx| this.reconnect_embedded_shell(cx)),
                            ),
                    ),
            )
            .when_some(self.render_find_bar(cx), |el, bar| el.child(bar))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .p_3()
                    .text_xs()
                    .text_color(theme::text_muted())
                    .child(self.shell_host.status(&key)),
            )
            .when_some(self.render_terminal_pane(cx), |el, pane| el.child(pane))
            .on_children_prepainted(move |bounds, window, cx| {
                let Some(bounds) = bounds.get(body_index) else {
                    return;
                };
                let rect = ShellViewportRect::from_bounds(
                    bounds.origin.x.as_f32(),
                    bounds.origin.y.as_f32(),
                    bounds.size.width.as_f32(),
                    bounds.size.height.as_f32(),
                    window.scale_factor(),
                );
                let _ = weak.update(cx, |view, _| {
                    let visible = should_show_embedded_shell(
                        view.main_surface,
                        view.active_modal,
                        view.selected_session.as_ref().map(|s| s.agent),
                        view.chat_state.find_open,
                        view.sidebar_state.rename.is_some()
                            || view.sidebar_state.new_workspace_draft.is_some(),
                    ) && !view.table_modal_state.is_open()
                        && !view.computer.is_open()
                        && !view.slash_picker_state.is_open()
                        && !(view.sidebar_rendered() && view.sidebar_state.dev_stats_expanded);
                    view.shell_host
                        .reconcile(visible.then_some(active_key.as_str()), Some(rect));
                });
            })
            .id("server-shell-panel")
            .test_support()
    }
}

#[cfg(windows)]
#[path = "shell_host_windows.rs"]
mod native;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn launch_is_exact_quoted_and_uses_host_identity() {
        let host = SshHostConfig {
            name: "vps".into(),
            hostname: "example.com".into(),
            user: "valentin".into(),
            port: 2222,
            identity_file: Some("C:/Users/A B/ZigShell/key".into()),
        };
        let spec =
            ShellLaunchSpec::from_host_and_session(&host, "term-'$(touch /tmp/no)'").unwrap();
        assert_eq!(spec.program, "ssh.exe");
        assert_eq!(
            spec.args.last().unwrap(),
            "tmux attach-session -t '=term-'\\''$(touch /tmp/no)'\\''' ".trim_end()
        );
        assert!(
            spec.args
                .windows(2)
                .any(|w| w == ["-i", "C:/Users/A B/ZigShell/key"])
        );
        assert!(spec.args.windows(2).any(|w| w == ["-p", "2222"]));
        assert_eq!(spec.args[spec.args.len() - 2], "valentin@example.com");
        assert!(ShellLaunchSpec::from_host_and_session(&host, "bad\nname").is_err());
        assert!(
            ShellLaunchSpec::from_host_and_session(
                &SshHostConfig {
                    hostname: "-oProxyCommand=evil".into(),
                    ..host
                },
                "ok"
            )
            .is_err()
        );
    }
    #[test]
    fn viewport_handles_dpi_sidebar_modes_and_small_windows() {
        assert_eq!(
            ShellViewportRect::from_window(1200., 800., 280., 44., 1.5),
            ShellViewportRect {
                x: 420,
                y: 66,
                width: 1380,
                height: 1134
            }
        );
        assert_eq!(
            ShellViewportRect::from_window(1200., 800., 56., 44., 2.).x,
            112
        );
        assert_eq!(
            ShellViewportRect::from_window(1200., 800., 0., 44., 1.).width,
            1200
        );
        assert_eq!(
            ShellViewportRect::from_window(100., 20., 280., 44., 1.).height,
            0
        );
        assert_eq!(
            ShellViewportRect::from_bounds(0.3, 0., 10.3, 20., 1.25).width,
            13
        );
    }
    #[test]
    fn overlays_and_non_shells_always_hide_native_child() {
        for surface in [MainSurface::Chat, MainSurface::Inbox, MainSurface::Settings] {
            for modal in [
                ActiveModal::None,
                ActiveModal::Palette,
                ActiveModal::Shortcuts,
            ] {
                for agent in [None, Some(AgentKind::Shell), Some(AgentKind::Claude)] {
                    for find in [false, true] {
                        for rename in [false, true] {
                            assert_eq!(
                                should_show_embedded_shell(surface, modal, agent, find, rename),
                                surface == MainSurface::Chat
                                    && modal == ActiveModal::None
                                    && agent == Some(AgentKind::Shell)
                                    && !find
                                    && !rename
                            );
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn keyboard_escape_preserves_terminal_control_keys() {
        for key in [0x4B, 0x31, 0x39, 0xBC, 0x4E] {
            assert!(forwarded_shortcut(key, true, false, false).is_some());
        }
        assert!(forwarded_shortcut(0x49, false, true, false).is_some());
        assert!(forwarded_shortcut(0x4B, false, true, false).is_some());
        assert!(forwarded_shortcut(0x70, false, false, false).is_some());
        assert!(forwarded_shortcut(0x43, true, false, false).is_none());
        assert!(forwarded_shortcut(0x44, true, false, false).is_none());
        assert!(forwarded_shortcut(0x5A, true, false, false).is_none());
        assert!(forwarded_shortcut(0x41, false, false, false).is_none());
        assert!(forwarded_shortcut(0x1B, false, false, false).is_none());
        assert!(forwarded_shortcut(0x4B, true, true, false).is_none());
    }
    #[test]
    fn workspace_shortcut_is_forwarded_from_native_terminal_and_browser() {
        assert_eq!(
            forwarded_shortcut(0x57, false, true, true),
            Some("alt-shift-w")
        );
        assert_eq!(
            registered_native_shortcut(0x57, false, true, true)
                .unwrap()
                .id,
            "workspace.switcher"
        );
        assert!(forwarded_shortcut(0x57, false, true, false).is_none());
        assert!(registered_native_shortcut(0x57, true, true, true).is_none());
    }

    #[test]
    fn windows_command_line_quotes_spaces_quotes_and_trailing_slashes() {
        assert_eq!(windows_quote("a b"), "\"a b\"");
        assert_eq!(windows_quote("a\"b"), "\"a\\\"b\"");
        assert_eq!(windows_quote("C:\\a b\\"), "\"C:\\a b\\\\\"");
        assert_eq!(windows_quote(""), "\"\"");
    }
}
