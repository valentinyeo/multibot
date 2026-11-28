use crate::app::MultibotView;
use crate::state::{ActiveModal, MainSurface};
use crate::theme;
use anyhow::{Result, bail};
use gpui_kit::{
    Context, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement, Styled,
    TestSupportExt, Window,
    component::{Sizable, button::Button, h_flex, v_flex},
    div,
    prelude::FluentBuilder,
    px,
};
use multibot_core::{SshHostConfig, daemon_client::validate_ssh_host};
use multibot_proto::{AgentKind, valid_shell_client_id, valid_shell_session_id};
use std::collections::HashMap;

pub const SHELL_HEADER_HEIGHT: f32 = 44.0;

#[cfg(any(windows, test))]
fn shell_hooks_needed(windows: impl IntoIterator<Item = usize>) -> bool {
    windows.into_iter().any(|hwnd| hwnd != 0)
}

#[cfg(any(windows, test))]
fn shell_mouse_hook_interested(code: i32, message: u32, visible: bool) -> bool {
    // WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEWHEEL. Never inspect mouse movement.
    code >= 0 && matches!(message, 0x0201 | 0x0202 | 0x020A) && visible
}

#[cfg(any(windows, test))]
fn compute_console_grid(
    client_w: i32,
    client_h: i32,
    cell_w: i32,
    cell_h: i32,
) -> Option<(i16, i16)> {
    if client_w <= 0 || client_h <= 0 || cell_w <= 0 || cell_h <= 0 {
        return None;
    }
    Some((
        (client_w / cell_w).clamp(20, 32766) as i16,
        (client_h / cell_h).clamp(4, 32766) as i16,
    ))
}

#[cfg(any(windows, test))]
fn console_url_at_cell(cells: &[u16], cols: usize, row: usize, col: usize) -> Option<String> {
    if cols == 0 || col >= cols || row.checked_mul(cols)?.checked_add(col)? >= cells.len() {
        return None;
    }
    let mut first = row;
    while first > 0 && cells[first * cols - 1] > b' ' as u16 {
        first -= 1;
    }
    let mut last = row;
    while (last + 1) * cols < cells.len() && cells[(last + 1) * cols - 1] > b' ' as u16 {
        last += 1;
    }
    let start = first * cols;
    let clicked = row * cols + col;
    let end = ((last + 1) * cols).min(cells.len());
    let text = String::from_utf16_lossy(&cells[start..end]);
    let byte = String::from_utf16_lossy(&cells[start..clicked]).len();
    crate::external_url::url_at_byte(&text, byte).map(str::to_string)
}

#[cfg(any(windows, test))]
fn console_paste_chunks(text: &[u16]) -> impl Iterator<Item = Vec<u16>> + '_ {
    let multiline = text.iter().any(|&c| c == 13 || c == 10);
    let mut input = text.iter().copied().peekable();
    let normalized = std::iter::from_fn(move || {
        let c = input.next()?;
        if c == 13 && input.peek() == Some(&10) {
            input.next();
        }
        Some(if c == 10 { 13 } else { c })
    });
    let prefix: &[u16] = if multiline {
        &[27, 91, 50, 48, 48, 126]
    } else {
        &[]
    };
    let suffix: &[u16] = if multiline {
        &[27, 91, 50, 48, 49, 126]
    } else {
        &[]
    };
    let mut encoded = prefix
        .iter()
        .copied()
        .chain(normalized)
        .chain(suffix.iter().copied());
    std::iter::from_fn(move || {
        let chunk: Vec<_> = encoded.by_ref().take(128).collect();
        (!chunk.is_empty()).then_some(chunk)
    })
}

#[cfg(any(windows, test))]
fn console_wheel_params(delta: i16, ctrl: bool, shift: bool, x: i32, y: i32) -> (usize, isize) {
    let modifiers = if ctrl { 0x0008 } else { 0 } | if shift { 0x0004 } else { 0 };
    let wparam = ((delta as u16 as u32) << 16) | modifiers;
    let lparam = (x as u16 as u32) | ((y as u16 as u32) << 16);
    (wparam as usize, lparam as isize)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellLaunchSpec {
    pub program: String,
    pub args: Vec<String>,
}

impl ShellLaunchSpec {
    pub fn from_host_and_session(
        host: &SshHostConfig,
        client_id: &str,
        session_name: &str,
    ) -> Result<Self> {
        validate_ssh_host(host)?;
        if !valid_shell_client_id(client_id) {
            bail!("Invalid shell client ID");
        }
        if !valid_shell_session_id(session_name) {
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
                // The outer shell owns cleanup; the subshell becomes tmux, not another SSH client.
                "trap 'rm -f -- \"$HOME/.multibot/clients/{client_id}.tty\"' EXIT; trap 'exit 129' HUP INT TERM; \
                 (mkdir -p \"$HOME/.multibot/clients\" && \
                 tty > \"$HOME/.multibot/clients/{client_id}.tty\" && \
                 exec tmux set-option -w -t '={target}:' window-size latest \\; attach-session -t '={target}')",
                target = session_name.replace('\'', "'\\''")
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
fn should_restore_shell_focus(
    window_active: bool,
    surface: MainSurface,
    modal: ActiveModal,
    agent: Option<AgentKind>,
    find_open: bool,
    rename_open: bool,
    browser_url_focused: bool,
) -> bool {
    window_active
        && !browser_url_focused
        && should_show_embedded_shell(surface, modal, agent, find_open, rename_open)
}

#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellShortcut {
    Paste,
    Forward(&'static str),
}

#[cfg(any(windows, test))]
fn shell_shortcut(key: u32, ctrl: bool, alt: bool, shift: bool) -> Option<ShellShortcut> {
    if key == 0x56 && ctrl && !alt {
        return Some(ShellShortcut::Paste);
    }
    registered_native_shortcut(key, ctrl, alt, shift)
        .map(|spec| ShellShortcut::Forward(spec.gpui_key))
}

#[cfg(any(windows, test))]
pub(crate) fn registered_native_shortcut(
    key: u32,
    ctrl: bool,
    alt: bool,
    shift: bool,
) -> Option<&'static multibot_keys::ShortcutSpec> {
    // Keep interrupt, EOF, suspend, and Escape terminal-owned even if the app binds them.
    if (ctrl && alt) || (ctrl && !alt && !shift && matches!(key, 0x43 | 0x44 | 0x5A)) || key == 0x1B
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

enum ShellConnection {
    #[cfg(any(windows, test))]
    Connecting(u64),
    #[cfg(windows)]
    Connected {
        handle: native::EmbeddedShellHandle,
    },
    #[cfg(test)]
    TestConnected {
        handle: TestShellHandle,
    },
    Disconnected(String),
}

#[cfg(test)]
#[derive(Default)]
struct TestShellHandle {
    rect: Option<ShellViewportRect>,
    visible: bool,
    resizes: usize,
}

#[cfg(any(windows, test))]
impl ShellConnection {
    fn reconcile(
        &mut self,
        parent: usize,
        active: bool,
        rect: Option<ShellViewportRect>,
        focus: bool,
    ) {
        match self {
            #[cfg(windows)]
            Self::Connected { handle } => handle.reconcile(parent, active, rect, focus),
            #[cfg(test)]
            Self::TestConnected { handle } => {
                let _ = (parent, focus);
                handle.visible = active;
                if active
                    && let Some(rect) = rect
                    && handle.rect != Some(rect)
                {
                    handle.rect = Some(rect);
                    handle.resizes += 1;
                }
            }
            _ => {}
        }
    }
}

struct ShellState {
    client_id: String,
    last_used_seq: u64,
    connection: ShellConnection,
    #[cfg(windows)]
    spawn_control: native::SpawnControl,
}

impl ShellState {
    fn new(connection: ShellConnection) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_CLIENT: AtomicU64 = AtomicU64::new(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self {
            client_id: format!(
                "mb-{:x}-{now:x}-{:x}",
                std::process::id(),
                NEXT_CLIENT.fetch_add(1, Ordering::Relaxed)
            ),
            last_used_seq: 0,
            connection,
            #[cfg(windows)]
            spawn_control: native::SpawnControl::default(),
        }
    }
}

const MAX_WARM_SHELLS: usize = 12;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ShellEntryKey {
    host: String,
    session_id: String,
}

impl ShellEntryKey {
    fn new(host: &str, session_id: &str) -> Self {
        Self {
            host: host.into(),
            session_id: session_id.into(),
        }
    }
}

#[derive(Default)]
pub(crate) struct ShellHost {
    entries: HashMap<ShellEntryKey, ShellState>,
    access_seq: u64,
    #[cfg(windows)]
    generation: u64,
    #[cfg(windows)]
    native: Option<native::NativeHost>,
}

impl ShellHost {
    fn status(&self, key: &ShellEntryKey) -> String {
        match self.entries.get(key).map(|s| &s.connection) {
            #[cfg(any(windows, test))]
            Some(ShellConnection::Connecting(_)) => "Connecting to server shell...".into(),
            #[cfg(windows)]
            Some(ShellConnection::Connected { .. }) => "Connected in native shell".into(),
            #[cfg(test)]
            Some(ShellConnection::TestConnected { .. }) => "Connected in native shell".into(),
            Some(ShellConnection::Disconnected(error)) => {
                format!("Disconnected — click Reconnect · {error}")
            }
            None => "Connecting to server shell...".into(),
        }
    }

    pub(crate) fn prepare_selection(&mut self, host: &str, session_id: &str) {
        let key = ShellEntryKey::new(host, session_id);
        if matches!(
            self.entries.get(&key).map(|s| &s.connection),
            Some(ShellConnection::Disconnected(_))
        ) {
            self.entries.remove(&key);
        }
    }

    fn touch(&mut self, key: &ShellEntryKey) {
        if let Some(state) = self.entries.get_mut(key) {
            self.access_seq += 1;
            state.last_used_seq = self.access_seq;
        }
    }

    fn insert(&mut self, key: ShellEntryKey, state: ShellState) {
        if !self.entries.contains_key(&key) && self.entries.len() >= MAX_WARM_SHELLS {
            let oldest = self
                .entries
                .iter()
                .filter(|(candidate, _)| *candidate != &key)
                .min_by_key(|(_, state)| state.last_used_seq)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(key.clone(), state);
        self.touch(&key);
    }

    pub(crate) fn focus_parent(&self) {
        #[cfg(windows)]
        if let Some(host) = &self.native {
            host.focus_parent();
        }
    }

    pub(crate) fn focus_session(&self, host: &str, session_id: &str) {
        #[cfg(windows)]
        if let Some(ShellState {
            connection: ShellConnection::Connected { handle },
            ..
        }) = self.entries.get(&ShellEntryKey::new(host, session_id))
        {
            handle.focus();
        }
        #[cfg(not(windows))]
        let _ = (host, session_id);
    }

    pub(crate) fn reconcile(
        &mut self,
        active: Option<&ShellEntryKey>,
        rect: Option<ShellViewportRect>,
        focus_on_show: bool,
    ) {
        #[cfg(windows)]
        let parent = self.native.as_ref().map(|host| host.parent);
        #[cfg(all(test, not(windows)))]
        let parent = Some(0);
        #[cfg(any(windows, test))]
        if let Some(parent) = parent {
            for (key, state) in &mut self.entries {
                if Some(key) != active {
                    state.connection.reconcile(parent, false, None, false);
                }
            }
            if let Some(key) = active
                && let Some(state) = self.entries.get_mut(key)
            {
                state
                    .connection
                    .reconcile(parent, true, rect, focus_on_show);
            }
        }
        #[cfg(not(any(windows, test)))]
        let _ = (active, rect, focus_on_show);
    }

    pub(crate) fn close_session(&mut self, host: &str, session_id: &str) {
        self.entries.remove(&ShellEntryKey::new(host, session_id));
    }

    pub(crate) fn retain_live(&mut self, live: &[(String, String)]) {
        self.entries.retain(|key, _| {
            live.iter()
                .any(|(host, session)| host == &key.host && session == &key.session_id)
        });
    }
}

impl MultibotView {
    pub(crate) fn embedded_shell_visible(&self) -> bool {
        should_show_embedded_shell(
            self.main_surface,
            self.active_modal,
            self.selection.selected_session.as_ref().map(|s| s.agent),
            self.chat_state.find_open,
            self.sidebar_state.rename.is_some() || self.sidebar_state.new_workspace_draft.is_some(),
        ) && !self.table_modal_state.is_open()
            && !self.slash_picker_state.is_open()
            && !(self.sidebar_rendered() && self.sidebar_state.dev_stats_expanded)
    }

    pub(crate) fn restore_embedded_shell_focus(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(windows)]
        {
            if should_restore_shell_focus(
                window.is_window_active(),
                self.main_surface,
                self.active_modal,
                self.selection.selected_session.as_ref().map(|s| s.agent),
                self.chat_state.find_open,
                self.sidebar_state.rename.is_some()
                    || self.sidebar_state.new_workspace_draft.is_some(),
                false,
            ) && !self.table_modal_state.is_open()
                && !self.computer.is_open()
                && !self.slash_picker_state.is_open()
                && !(self.sidebar_rendered() && self.sidebar_state.dev_stats_expanded)
                && !self
                    .terminal
                    .focus_handle
                    .as_ref()
                    .is_some_and(|h| h.is_focused(window))
            {
                self.sync_embedded_shell(window, cx);
                if let Some(host) = &self.shell_host.native {
                    host.focus_visible();
                }
            }
        }
        #[cfg(not(windows))]
        let _ = (window, cx);
    }

    pub(crate) fn reconnect_embedded_shell(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = &self.selection.selected_session {
            let key = ShellEntryKey::new(self.active_session_host(), &session.id);
            self.shell_host.entries.remove(&key);
        }
        cx.notify();
    }

    fn active_shell_key(&self) -> Option<ShellEntryKey> {
        self.selection
            .selected_session
            .as_ref()
            .filter(|session| session.agent == AgentKind::Shell && !session.archived)
            .map(|session| ShellEntryKey::new(self.active_session_host(), &session.id))
    }

    pub(crate) fn sync_embedded_shell(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let live: Vec<_> = if self.connection.host_sessions.is_empty() {
            self.connection
                .sessions
                .iter()
                .filter(|s| s.agent == AgentKind::Shell && !s.archived)
                .map(|s| (self.active_session_host().to_string(), s.id.clone()))
                .collect()
        } else {
            self.connection
                .host_sessions
                .iter()
                .filter(|(_, s)| s.agent == AgentKind::Shell && !s.archived)
                .map(|(h, s)| (h.clone(), s.id.clone()))
                .collect()
        };
        self.shell_host.retain_live(&live);
        #[cfg(windows)]
        if let Some(host) = &self.shell_host.native {
            let text_input_focused = window.focused(cx).is_some_and(|focus| {
                focus != self.focus.root_focus_handle
                    && self.chat_state.focus_handle.as_ref() != Some(&focus)
            });
            host.set_text_input_focus(text_input_focused);
            host.set_kill_confirmation(
                self.sidebar_state
                    .pending_shell_dissolve
                    .as_ref()
                    .is_some_and(|(key, _)| {
                        self.selection
                            .selected_session
                            .as_ref()
                            .is_some_and(|session| self.session_key_for(session) == *key)
                    }),
            );
        }
        let active = self
            .embedded_shell_visible()
            .then(|| self.active_shell_key())
            .flatten();
        self.shell_host
            .reconcile(active.as_ref(), None, !self.computer.is_open());
        let Some(key) = active else { return };
        let session_id = key.session_id.clone();
        if !live.contains(&(key.host.clone(), session_id.clone())) {
            return;
        }
        self.shell_host.touch(&key);
        if self.shell_host.entries.contains_key(&key) {
            return;
        }
        let state = ShellState::new(ShellConnection::Disconnected(String::new()));
        let host = self
            .settings
            .ssh_hosts
            .iter()
            .find(|h| h.name == key.host)
            .or_else(|| {
                cx.global::<crate::state::SharedAppStore>()
                    .host_connections
                    .get(&key.host)
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
            .and_then(|host| {
                ShellLaunchSpec::from_host_and_session(host, &state.client_id, &session_id)
            });
        self.shell_host.insert(key.clone(), state);
        let spec = match spec {
            Ok(spec) => spec,
            Err(err) => {
                self.shell_host.entries.get_mut(&key).unwrap().connection =
                    ShellConnection::Disconnected(err.to_string());
                return;
            }
        };
        #[cfg(not(windows))]
        {
            let _ = (spec, window, cx);
            self.shell_host.entries.get_mut(&key).unwrap().connection =
                ShellConnection::Disconnected("Native embedded shells require Windows".into());
        }
        #[cfg(windows)]
        {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            let parent = match HasWindowHandle::window_handle(window).map(|h| h.as_raw()) {
                Ok(RawWindowHandle::Win32(h)) => h.hwnd.get() as usize,
                _ => {
                    self.shell_host.entries.get_mut(&key).unwrap().connection =
                        ShellConnection::Disconnected("No Windows window handle".into());
                    return;
                }
            };
            if self.shell_host.native.is_none() {
                let (shortcuts, mut shortcut_rx) = tokio::sync::mpsc::unbounded_channel();
                match native::NativeHost::new(parent, shortcuts) {
                    Ok(host) => self.shell_host.native = Some(host),
                    Err(err) => {
                        self.shell_host.entries.get_mut(&key).unwrap().connection =
                            ShellConnection::Disconnected(err.to_string());
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
                                        if let Some(host) = &view.read(cx).shell_host.native {
                                            host.focus_parent();
                                        }
                                        view.read(cx)
                                            .focus
                                            .root_focus_handle
                                            .clone()
                                            .focus(window, cx);
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
            self.shell_host.entries.get_mut(&key).unwrap().connection =
                ShellConnection::Connecting(generation);
            let job = self.shell_host.native.as_ref().unwrap().job.clone();
            let control = self.shell_host.entries[&key].spawn_control.0.clone();
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
            let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
            let spawn = std::thread::Builder::new()
                .name("native-shell".into())
                .spawn(move || {
                    let result = native::spawn(spec, job, parent, control);
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
                self.shell_host.entries.get_mut(&key).unwrap().connection =
                    ShellConnection::Disconnected(error.to_string());
                return;
            }
            cx.spawn(async move |this, cx| {
                let Ok(result) = ready_rx.await else { return };
                let _ = this.update(cx, |view, cx| {
                    if let Some(state) = view.shell_host.entries.get_mut(&key)
                        && matches!(state.connection, ShellConnection::Connecting(g) if g == generation)
                    {
                        state.connection = match result {
                            Ok(mut handle) => {
                                handle.generation = generation;
                                ShellConnection::Connected { handle }
                            }
                            Err(error) => ShellConnection::Disconnected(error.to_string()),
                        };
                        cx.notify();
                    }
                });
                if exit_rx.await.is_ok() {
                    let _ = this.update(cx, |view, cx| {
                        if let Some(state) = view.shell_host.entries.get_mut(&key)
                            && matches!(&state.connection, ShellConnection::Connected { handle } if handle.generation == generation)
                        {
                            state.connection = ShellConnection::Disconnected("SSH exited".into());
                            cx.notify();
                        }
                    });
                }
            }).detach();
        }
    }

    pub(crate) fn render_shell_panel(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let session = self.selection.selected_session.as_ref().unwrap();
        let key = self.session_key_for(session);
        let confirming_kill = self
            .sidebar_state
            .pending_shell_dissolve
            .as_ref()
            .is_some_and(|(pending, _)| pending == &key);
        let title = self
            .sidebar_state
            .workspace_store
            .effective_title(&key, &session.title)
            .to_string();
        let host = self
            .selection
            .selected_session_host
            .as_deref()
            .or(self.connection.connected_host.as_deref())
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
        let active_key = ShellEntryKey::new(self.active_session_host(), &session.id);
        let weak = cx.entity().downgrade();
        v_flex()
            .flex_1()
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
                            this.focus.root_focus_handle.focus(window, cx);
                        }),
                    )
                    .child(if let Some(draft) = header_rename {
                        div()
                            .id("header-rename-input")
                            .test_support()
                            .text_sm()
                            .child(format!("{draft}|"))
                    } else {
                        div()
                            .id("shell-title")
                            .test_support()
                            .cursor_pointer()
                            .rounded(theme::RADIUS_CHIP)
                            .px_3()
                            .py_1()
                            .bg(theme::bg_surface())
                            .hover(|s| s.bg(theme::bg_hover()))
                            .tooltip(|window, cx| {
                                gpui_kit::component::tooltip::Tooltip::new("Show chat info")
                                    .build(window, cx)
                            })
                            .on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.toggle_chat_info(window, cx)
                                }),
                            )
                            .text_sm()
                            .max_w(px(if confirming_kill { 140.0 } else { 250.0 }))
                            .truncate()
                            .child(title)
                    })
                    .when(!session.subworkers.is_empty(), |el| {
                        let count = session.subworkers.len();
                        el.child(
                            div()
                                .id("shell-subworkers")
                                .test_support()
                                .flex_shrink_0()
                                .px_1p5()
                                .py_0p5()
                                .rounded(theme::RADIUS_CHIP)
                                .bg(theme::bg_surface())
                                .text_xs()
                                .text_color(theme::text_muted())
                                .child(format!(
                                    "{count} {}",
                                    if count == 1 { "worker" } else { "workers" }
                                )),
                        )
                    })
                    .when(!confirming_kill, |el| {
                        el.child(
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
                    })
                    .child(div().flex_1())
                    .when(confirming_kill, |el| {
                        let kill_key = key.clone();
                        el.child(
                            h_flex()
                                .id("shell-kill-confirmation")
                                .flex_shrink_0()
                                .test_support()
                                .gap_2()
                                .px_3()
                                .py_1()
                                .rounded(theme::RADIUS_CHIP)
                                .bg(theme::bg_surface())
                                .text_xs()
                                .text_color(theme::accent_amber())
                                .child("Press Ctrl+Shift+W again to kill shell")
                                .child(Button::new("shell-kill").small().label("Kill").on_click(
                                    cx.listener(move |this, _, _, cx| {
                                        this.dissolve_session_key_confirmed(&kill_key, cx);
                                    }),
                                ))
                                .child(
                                    Button::new("shell-kill-cancel")
                                        .small()
                                        .label("Cancel")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.sidebar_state.pending_shell_dissolve = None;
                                            this.notice = None;
                                            cx.notify();
                                        })),
                                ),
                        )
                    })
                    .when(!confirming_kill, |el| {
                        el.child(
                            Button::new("shell-reconnect")
                                .small()
                                .label("Reconnect")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.reconnect_embedded_shell(cx)),
                                ),
                        )
                    }),
            )
            .when_some(self.render_find_bar(760.0, cx), |el, bar| el.child(bar))
            .child(
                div()
                    .id("shell-viewport")
                    .test_support()
                    .on_mouse_down(
                        gpui_kit::MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            cx.stop_propagation();
                            this.focus.root_focus_handle.focus(window, cx);
                            this.restore_embedded_shell_focus(window, cx);
                        }),
                    )
                    .flex_1()
                    .min_h_0()
                    .p_3()
                    .text_xs()
                    .text_color(theme::text_muted())
                    .child(self.shell_host.status(&active_key)),
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
                    if view.embedded_shell_visible()
                        && view.active_shell_key().as_ref() == Some(&active_key)
                    {
                        view.shell_host.reconcile(
                            Some(&active_key),
                            Some(rect),
                            !view.computer.is_open(),
                        );
                    }
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
    use gpui_kit::AppContext;

    #[test]
    fn input_hooks_follow_visible_windows_across_multiple_hosts() {
        assert!(!shell_hooks_needed([]));
        assert!(!shell_hooks_needed([0, 0]));
        assert!(shell_hooks_needed([42, 0]));
        assert!(shell_hooks_needed([0, 84]));
        assert!(shell_hooks_needed([42, 84]));
        assert!(!shell_hooks_needed([0, 0]));
    }

    #[test]
    fn mouse_hook_bypasses_moves_negative_codes_and_hidden_shells() {
        for message in [0x0200, 0x0203, 0x0204, 0x020E, 0] {
            assert!(!shell_mouse_hook_interested(0, message, true));
        }
        for message in [0x0201, 0x0202, 0x020A] {
            assert!(shell_mouse_hook_interested(0, message, true));
            assert!(!shell_mouse_hook_interested(-1, message, true));
            assert!(!shell_mouse_hook_interested(0, message, false));
        }
    }

    #[test]
    fn console_grid_tracks_client_pixels_and_font_size() {
        assert_eq!(compute_console_grid(1896, 960, 8, 16), Some((237, 60)));
        assert_eq!(compute_console_grid(743, 800, 8, 16), Some((92, 50)));
        assert_eq!(compute_console_grid(759, 799, 8, 16), Some((94, 49)));
        assert_eq!(compute_console_grid(1, 1, 8, 16), Some((20, 4)));
        assert_eq!(
            compute_console_grid(i32::MAX, i32::MAX, 1, 1),
            Some((32766, 32766))
        );
        for dimensions in [
            (0, 800, 8, 16),
            (760, 0, 8, 16),
            (-1, 800, 8, 16),
            (760, -1, 8, 16),
            (760, 800, 0, 16),
            (760, 800, 8, 0),
            (760, 800, -8, 16),
            (760, 800, 8, -16),
        ] {
            let (w, h, cw, ch) = dimensions;
            assert_eq!(compute_console_grid(w, h, cw, ch), None);
        }
    }

    fn connected_shell() -> ShellState {
        ShellState::new(ShellConnection::TestConnected {
            handle: TestShellHandle::default(),
        })
    }

    #[test]
    fn same_host_sessions_have_independent_connections_and_launch_targets() {
        let mut host = ShellHost::default();
        let btop = ShellEntryKey::new("host-a", "btop");
        let multibot = ShellEntryKey::new("host-a", "multibot");
        host.insert(btop.clone(), connected_shell());
        host.insert(
            multibot.clone(),
            ShellState::new(ShellConnection::Connecting(7)),
        );
        assert_eq!(host.entries.len(), 2);
        assert_ne!(
            host.entries[&btop].client_id,
            host.entries[&multibot].client_id
        );
        assert_eq!(host.status(&btop), "Connected in native shell");
        assert!(matches!(
            host.entries[&multibot].connection,
            ShellConnection::Connecting(7)
        ));
        for key in [&btop, &multibot] {
            let spec = ShellLaunchSpec::from_host_and_session(
                &SshHostConfig {
                    name: "host-a".into(),
                    hostname: "example.com".into(),
                    user: "test".into(),
                    port: 22,
                    identity_file: None,
                },
                &host.entries[key].client_id,
                &key.session_id,
            )
            .unwrap();
            assert!(
                spec.args
                    .last()
                    .unwrap()
                    .contains(&format!("attach-session -t '={}'", key.session_id))
            );
            assert!(!spec.args.last().unwrap().contains("switch-client"));
        }
        host.close_session("host-a", "btop");
        assert!(!host.entries.contains_key(&btop));
        assert!(host.entries.contains_key(&multibot));
    }

    #[cfg(not(windows))]
    #[test]
    fn switching_preserves_each_shells_rect_and_only_shows_the_active_session() {
        let mut host = ShellHost::default();
        let btop = ShellEntryKey::new("host-a", "btop");
        let multibot = ShellEntryKey::new("host-a", "multibot");
        host.insert(btop.clone(), connected_shell());
        host.insert(multibot.clone(), connected_shell());
        let wide = ShellViewportRect {
            x: 0,
            y: 44,
            width: 1900,
            height: 900,
        };
        let narrow = ShellViewportRect {
            x: 500,
            y: 44,
            width: 760,
            height: 900,
        };
        host.reconcile(Some(&btop), Some(wide), true);
        host.reconcile(Some(&multibot), Some(narrow), true);
        for _ in 0..5 {
            host.reconcile(Some(&btop), None, true);
            host.reconcile(Some(&btop), Some(wide), true);
            host.reconcile(Some(&multibot), None, true);
            host.reconcile(Some(&multibot), Some(narrow), true);
        }
        for (key, rect, visible) in [(&btop, wide, false), (&multibot, narrow, true)] {
            let ShellConnection::TestConnected { handle } = &host.entries[key].connection else {
                panic!("expected connected shell")
            };
            assert_eq!(handle.rect, Some(rect));
            assert_eq!(handle.visible, visible);
            assert_eq!(handle.resizes, 1);
        }
        host.reconcile(None, None, false);
        for state in host.entries.values() {
            let ShellConnection::TestConnected { handle } = &state.connection else {
                panic!("expected connected shell")
            };
            assert!(!handle.visible);
        }
    }

    #[test]
    fn closing_one_pending_attach_keeps_other_sessions_alive() {
        let mut host = ShellHost::default();
        let a = ShellEntryKey::new("host-a", "a");
        let b = ShellEntryKey::new("host-a", "b");
        host.insert(a.clone(), ShellState::new(ShellConnection::Connecting(1)));
        host.insert(b.clone(), connected_shell());
        host.close_session("host-a", "a");
        assert!(!host.entries.contains_key(&a));
        assert!(host.entries.contains_key(&b));
    }

    #[test]
    fn preparing_selection_only_removes_that_disconnected_session() {
        let mut host = ShellHost::default();
        let a = ShellEntryKey::new("host-a", "a");
        let b = ShellEntryKey::new("host-a", "b");
        let c = ShellEntryKey::new("host-b", "a");
        host.insert(
            a.clone(),
            ShellState::new(ShellConnection::Disconnected("SSH exited".into())),
        );
        host.insert(b.clone(), connected_shell());
        host.insert(c.clone(), connected_shell());
        host.prepare_selection("host-a", "b");
        assert_eq!(host.entries.len(), 3);
        host.prepare_selection("host-a", "a");
        assert!(!host.entries.contains_key(&a));
        assert!(host.entries.contains_key(&b));
        assert!(host.entries.contains_key(&c));
    }

    #[test]
    fn lru_caps_warm_shells_and_keeps_the_active_and_recently_used_sessions() {
        let mut host = ShellHost::default();
        for i in 0..MAX_WARM_SHELLS {
            host.insert(
                ShellEntryKey::new("host-a", &i.to_string()),
                connected_shell(),
            );
        }
        let recent = ShellEntryKey::new("host-a", "0");
        host.touch(&recent);
        let active = ShellEntryKey::new("host-a", "active");
        host.insert(active.clone(), connected_shell());
        assert_eq!(host.entries.len(), MAX_WARM_SHELLS);
        assert!(host.entries.contains_key(&active));
        assert!(host.entries.contains_key(&recent));
        assert!(
            !host
                .entries
                .contains_key(&ShellEntryKey::new("host-a", "1"))
        );
        host.insert(active.clone(), connected_shell());
        assert_eq!(host.entries.len(), MAX_WARM_SHELLS);
        assert!(host.entries.contains_key(&recent));
        for i in 0..30 {
            let active = ShellEntryKey::new("host-b", &i.to_string());
            host.insert(active.clone(), connected_shell());
            assert_eq!(host.entries.len(), MAX_WARM_SHELLS);
            assert!(host.entries.contains_key(&active));
        }
    }

    #[test]
    fn retain_live_matches_both_host_and_session() {
        let mut host = ShellHost::default();
        for (h, s) in [("host-a", "a"), ("host-a", "b"), ("host-b", "a")] {
            host.insert(ShellEntryKey::new(h, s), connected_shell());
        }
        host.retain_live(&[("host-a".into(), "b".into()), ("host-b".into(), "a".into())]);
        assert_eq!(host.entries.len(), 2);
        assert!(
            host.entries
                .contains_key(&ShellEntryKey::new("host-a", "b"))
        );
        assert!(
            host.entries
                .contains_key(&ShellEntryKey::new("host-b", "a"))
        );
        host.retain_live(&[]);
        assert!(host.entries.is_empty());
    }

    #[gpui_kit::test]
    fn syncing_discards_archived_missing_and_non_shell_entries(cx: &mut gpui_kit::TestAppContext) {
        let (window, view) = crate::app::tests::open_test_view_with_entity(cx);
        cx.update_window(window, |_, window, cx| {
            view.update(cx, |view, cx| {
                let live = multibot_proto::SessionSummary {
                    id: "live".into(),
                    agent: AgentKind::Shell,
                    ..Default::default()
                };
                let archived = multibot_proto::SessionSummary {
                    id: "archived".into(),
                    archived: true,
                    ..live.clone()
                };
                let chat = multibot_proto::SessionSummary {
                    id: "chat".into(),
                    agent: AgentKind::Claude,
                    ..live.clone()
                };
                view.selection.selected_session = None;
                view.connection.host_sessions = vec![
                    ("host-a".into(), live.clone()),
                    ("host-b".into(), live),
                    ("host-a".into(), archived),
                    ("host-a".into(), chat),
                ];
                for (h, id) in [
                    ("host-a", "live"),
                    ("host-b", "live"),
                    ("host-a", "archived"),
                    ("host-a", "chat"),
                    ("host-a", "missing"),
                ] {
                    view.shell_host
                        .insert(ShellEntryKey::new(h, id), connected_shell());
                }
                view.sync_embedded_shell(window, cx);
                assert_eq!(view.shell_host.entries.len(), 2);
                for host in ["host-a", "host-b"] {
                    assert!(
                        view.shell_host
                            .entries
                            .contains_key(&ShellEntryKey::new(host, "live"))
                    );
                }
            });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn selecting_reconnecting_and_closing_shells_never_sends_switch_requests(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use multibot_proto::ClientRequest;
        let (window, view) = crate::app::tests::open_test_view_with_entity(cx);
        let (tx_a, mut rx_a) = tokio::sync::mpsc::unbounded_channel();
        let (tx_b, mut rx_b) = tokio::sync::mpsc::unbounded_channel();
        cx.update_window(window, |_, _, cx| {
            view.update(cx, |view, cx| {
                let a = multibot_proto::SessionSummary {
                    id: "a".into(), agent: AgentKind::Shell, ..Default::default()
                };
                let b = multibot_proto::SessionSummary { id: "b".into(), ..a.clone() };
                view.connection.host_sessions = vec![("host-a".into(), a.clone()), ("host-a".into(), b.clone())];
                view.connection.sessions = vec![a.clone(), b.clone()];
                view.connection.host_txs.insert("host-a".into(), tx_a);
                view.connection.host_txs.insert("host-b".into(), tx_b);
                let key_a = ShellEntryKey::new("host-a", "a");
                let key_b = ShellEntryKey::new("host-a", "b");
                view.shell_host.insert(key_a.clone(), connected_shell());
                view.shell_host.insert(key_b.clone(), connected_shell());
                let client_a = view.shell_host.entries[&key_a].client_id.clone();
                for session in [a.clone(), b.clone(), a.clone(), b.clone()] {
                    view.select_session_on_host("host-a".into(), session, cx);
                }
                assert_eq!(view.shell_host.entries.len(), 2);
                view.reconnect_embedded_shell(cx);
                assert!(!view.shell_host.entries.contains_key(&key_b));
                assert_eq!(view.shell_host.entries[&key_a].client_id, client_a);
                view.shell_host.insert(key_b.clone(), connected_shell());
                let key = multibot_core::workspaces::canonical_session_key("host-a", AgentKind::Shell, "b");
                view.dissolve_session_key(&key, cx);
                view.dissolve_session_key(&key, cx);
                let requests: Vec<_> = std::iter::from_fn(|| rx_a.try_recv().ok()).collect();
                assert!(!requests.iter().any(|r| matches!(r, ClientRequest::SwitchShellClient { .. })));
                assert!(requests.iter().any(|r| matches!(r, ClientRequest::ArchiveSession { session_id, .. } if session_id == "b")));
                assert!(rx_b.try_recv().is_err());
                assert_eq!(view.shell_host.entries.len(), 1);
                assert_eq!(view.shell_host.entries[&key_a].client_id, client_a);
            });
        }).unwrap();
    }

    #[test]
    fn dissolve_chord_is_forwarded_on_each_distinct_press() {
        for _ in 0..2 {
            assert_eq!(
                shell_shortcut(0x57, true, false, true),
                Some(ShellShortcut::Forward("ctrl-shift-w"))
            );
        }
        assert!(shell_shortcut(0x57, true, false, false).is_none());
    }

    #[test]
    fn console_paste_encodes_urls_multiline_empty_and_bounded_utf16_chunks() {
        for (text, expected) in [
            (
                "https://screencast2.com/abc.png",
                "https://screencast2.com/abc.png",
            ),
            (
                "one\r\ntwo\nthree\rfour",
                "\x1b[200~one\rtwo\rthree\rfour\x1b[201~",
            ),
            ("", ""),
        ] {
            let input: Vec<_> = text.encode_utf16().collect();
            let chunks: Vec<_> = console_paste_chunks(&input).collect();
            assert!(chunks.iter().all(|chunk| chunk.len() <= 128));
            assert_eq!(chunks.concat(), expected.encode_utf16().collect::<Vec<_>>());
            if text.is_empty() {
                assert!(chunks.is_empty());
            }
        }
        let input: Vec<_> = "😀x".repeat(200).encode_utf16().collect();
        let chunks: Vec<_> = console_paste_chunks(&input).collect();
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|chunk| chunk.len() <= 128));
        assert_eq!(chunks.concat(), input);
    }

    #[test]
    fn console_wheel_packs_signed_delta_coordinates_and_modifiers() {
        for delta in [-120i16, 120] {
            for (ctrl, shift, modifiers) in [
                (false, false, 0),
                (true, false, 8),
                (false, true, 4),
                (true, true, 12),
            ] {
                let (w, l) = console_wheel_params(delta, ctrl, shift, -250, -40);
                assert_eq!((w >> 16) as i16, delta);
                assert_eq!(w & 0xffff, modifiers);
                assert_eq!(l as i16, -250);
                assert_eq!((l >> 16) as i16, -40);
            }
        }
    }

    #[test]
    fn page_scrolling_stays_terminal_owned() {
        for key in [0x21, 0x22] {
            for shift in [false, true] {
                assert_eq!(shell_shortcut(key, false, false, shift), None);
            }
        }
    }

    #[test]
    fn console_url_hit_test_uses_utf16_cells_and_wrapped_rows() {
        let cols = 20;
        let mut cells: Vec<u16> = "é https://example.com/abc_(def)).,;"
            .encode_utf16()
            .collect();
        cells.resize(cols * 2, b' ' as u16);
        for (row, col) in [(0, 6), (1, 2)] {
            let url = console_url_at_cell(&cells, cols, row, col).unwrap();
            assert_eq!(url, "https://example.com/abc_(def)");
            crate::external_url::take_opened_urls();
            crate::external_url::open_external_url(&url).unwrap();
            assert_eq!(crate::external_url::take_opened_urls(), [url]);
        }
        for (row, col) in [(0, 0), (1, 19), (2, 0), (0, 20)] {
            assert_eq!(console_url_at_cell(&cells, cols, row, col), None);
        }
        assert_eq!(console_url_at_cell(&[], 0, 0, 0), None);
        let unsafe_url: Vec<_> = "file:///etc/passwd".encode_utf16().collect();
        assert_eq!(
            console_url_at_cell(&unsafe_url, unsafe_url.len(), 0, 4),
            None
        );
    }

    #[test]
    fn activation_restores_only_visible_shell_without_editing_overlays() {
        let eligible = |active, surface, modal, agent, find, rename, url| {
            should_restore_shell_focus(active, surface, modal, agent, find, rename, url)
        };
        assert!(eligible(
            true,
            MainSurface::Chat,
            ActiveModal::None,
            Some(AgentKind::Shell),
            false,
            false,
            false
        ));
        assert!(!eligible(
            false,
            MainSurface::Chat,
            ActiveModal::None,
            Some(AgentKind::Shell),
            false,
            false,
            false
        ));
        for modal in [
            ActiveModal::Palette,
            ActiveModal::Shortcuts,
            ActiveModal::Changelog,
            ActiveModal::ChatInfo,
        ] {
            assert!(!eligible(
                true,
                MainSurface::Chat,
                modal,
                Some(AgentKind::Shell),
                false,
                false,
                false
            ));
        }
        for (find, rename, url) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            assert!(!eligible(
                true,
                MainSurface::Chat,
                ActiveModal::None,
                Some(AgentKind::Shell),
                find,
                rename,
                url
            ));
        }
        assert!(!eligible(
            true,
            MainSurface::Inbox,
            ActiveModal::None,
            Some(AgentKind::Shell),
            false,
            false,
            false
        ));
        assert!(!eligible(
            true,
            MainSurface::Chat,
            ActiveModal::None,
            Some(AgentKind::Claude),
            false,
            false,
            false
        ));
        assert!(!eligible(
            true,
            MainSurface::Chat,
            ActiveModal::None,
            None,
            false,
            false,
            false
        ));
    }

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
            ShellLaunchSpec::from_host_and_session(&host, "mb-client_1", "term-'$(echo nope)'")
                .unwrap();
        assert_eq!(spec.program, "ssh.exe");
        assert_eq!(
            spec.args.last().unwrap(),
            "trap 'rm -f -- \"$HOME/.multibot/clients/mb-client_1.tty\"' EXIT; trap 'exit 129' HUP INT TERM; \
             (mkdir -p \"$HOME/.multibot/clients\" && \
             tty > \"$HOME/.multibot/clients/mb-client_1.tty\" && \
             exec tmux set-option -w -t '=term-'\\''$(echo nope)'\\'':' window-size latest \\; attach-session -t '=term-'\\''$(echo nope)'\\''')"
        );
        assert!(
            spec.args
                .windows(2)
                .any(|w| w == ["-i", "C:/Users/A B/ZigShell/key"])
        );
        assert!(spec.args.windows(2).any(|w| w == ["-p", "2222"]));
        assert_eq!(spec.args[spec.args.len() - 2], "valentin@example.com");
        for session in ["bad\nname", "../bad", "bad\\name", ""] {
            assert!(ShellLaunchSpec::from_host_and_session(&host, "mb-client_1", session).is_err());
        }
        for client in [
            "",
            "../",
            "/",
            "a..b",
            "$(evil)",
            "a'b",
            "a\n",
            &"a".repeat(65),
        ] {
            assert!(ShellLaunchSpec::from_host_and_session(&host, client, "ok").is_err());
        }
        assert!(
            ShellLaunchSpec::from_host_and_session(
                &SshHostConfig {
                    hostname: "-oProxyCommand=evil".into(),
                    ..host
                },
                "mb-client_1",
                "ok"
            )
            .is_err()
        );
    }
    #[cfg(unix)]
    #[test]
    fn launch_registers_tty_preserves_literal_target_and_cleans_up_on_exit() {
        use std::os::unix::fs::PermissionsExt;
        let state = ShellState::new(ShellConnection::TestConnected {
            handle: TestShellHandle::default(),
        });
        let home = std::env::temp_dir().join(&state.client_id);
        std::fs::create_dir_all(&home).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(home.clone());
        let tmux = home.join("tmux");
        std::fs::write(&tmux, "#!/bin/sh\n[ -s \"$HOME/.multibot/clients/test-client.tty\" ] || exit 42\nprintf '%s\\n' \"$@\" > \"$HOME/args\"\nexit 7\n").unwrap();
        std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o700)).unwrap();
        let host = SshHostConfig {
            name: "test".into(),
            hostname: "example.com".into(),
            user: "test".into(),
            port: 22,
            identity_file: None,
        };
        let session = "term-'$(echo nope)'";
        let spec = ShellLaunchSpec::from_host_and_session(&host, "test-client", session).unwrap();
        let output = std::process::Command::new("script")
            .args(["-q", "-e", "-c", spec.args.last().unwrap(), "/dev/null"])
            .env("HOME", &home)
            .env(
                "PATH",
                format!("{}:{}", home.display(), std::env::var("PATH").unwrap()),
            )
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(
            std::fs::read_to_string(home.join("args")).unwrap(),
            format!(
                "set-option\n-w\n-t\n={session}:\nwindow-size\nlatest\n;\nattach-session\n-t\n={session}\n"
            )
        );
        let registration = home.join(".multibot/clients/test-client.tty");
        assert!(!registration.exists());
        std::fs::write(&registration, "/dev/pts/1\n").unwrap();
        let traps = spec.args.last().unwrap().split_once("; (").unwrap().0;
        let mut shell = std::process::Command::new("sh")
            .args(["-c", &format!("{traps}; echo ready; read value")])
            .env("HOME", &home)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(shell.stdout.take().unwrap()),
            &mut ready,
        )
        .unwrap();
        assert_eq!(ready, "ready\n");
        assert!(
            std::process::Command::new("kill")
                .args(["-HUP", &shell.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(shell.wait().unwrap().code(), Some(129));
        assert!(!registration.exists());
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
    fn daily_driver_browser_shortcuts_are_forwarded_without_changing_terminal_keys() {
        assert_eq!(
            shell_shortcut(0x41, false, true, false),
            Some(ShellShortcut::Forward("alt-a"))
        );
        assert_eq!(
            shell_shortcut(0x51, false, true, false),
            Some(ShellShortcut::Forward("alt-q"))
        );
        assert_eq!(
            shell_shortcut(0x42, true, false, true),
            Some(ShellShortcut::Forward("ctrl-shift-b"))
        );
        assert!(shell_shortcut(0x57, true, false, false).is_none());
        assert!(shell_shortcut(0x4C, true, false, false).is_none());
    }

    #[test]
    fn keyboard_escape_preserves_terminal_control_keys() {
        for key in [0x4B, 0x31, 0x39, 0xBC, 0x4E] {
            assert!(shell_shortcut(key, true, false, false).is_some());
        }
        assert!(shell_shortcut(0x49, false, true, false).is_some());
        assert!(shell_shortcut(0x4B, false, true, false).is_some());
        assert!(shell_shortcut(0x70, false, false, false).is_some());
        assert!(shell_shortcut(0x43, true, false, false).is_none());
        assert!(shell_shortcut(0x44, true, false, false).is_none());
        assert!(shell_shortcut(0x5A, true, false, false).is_none());
        assert!(shell_shortcut(0x41, false, false, false).is_none());
        assert!(shell_shortcut(0x1B, false, false, false).is_none());
        assert!(shell_shortcut(0x4B, true, true, false).is_none());
    }
    #[test]
    fn both_paste_chords_stay_in_the_shell_instead_of_forwarding_ctrl_v() {
        for shift in [false, true] {
            assert_eq!(
                shell_shortcut(0x56, true, false, shift),
                Some(ShellShortcut::Paste)
            );
            assert!(shell_shortcut(0x56, false, false, shift).is_none());
            assert!(shell_shortcut(0x56, true, true, shift).is_none());
            for key in [0x43, 0x44, 0x5A, 0x1B] {
                assert_ne!(
                    shell_shortcut(key, true, false, shift),
                    Some(ShellShortcut::Paste)
                );
            }
        }
    }

    #[test]
    fn workspace_shortcut_is_forwarded_from_native_terminal_and_browser() {
        assert_eq!(
            shell_shortcut(0x57, false, true, true),
            Some(ShellShortcut::Forward("alt-shift-w"))
        );
        assert_eq!(
            registered_native_shortcut(0x57, false, true, true)
                .unwrap()
                .id,
            "workspace.switcher"
        );
        assert!(shell_shortcut(0x57, false, true, false).is_none());
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
