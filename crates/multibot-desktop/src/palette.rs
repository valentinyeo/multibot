use crate::app::{CloseOverlay, MultibotView};
use crate::sidebar::InlineRenameState;
use crate::state::{ActiveModal, MainSurface};
use crate::theme::{self, RADIUS_CHIP, RADIUS_MODAL, RADIUS_ROW};
use gpui_kit::{
    ClipboardItem, Context, Focusable, FontWeight, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, ParentElement, Role, ScrollHandle, SharedString, StatefulInteractiveElement,
    Styled, Subscription, TestSupportExt, Window,
    component::{
        Sizable,
        button::Button,
        h_flex,
        input::{self, Input, InputEvent},
        v_flex,
    },
    div, hsla,
    prelude::FluentBuilder,
    px,
};
use multibot_core::SidebarMode;
use multibot_core::workspaces::{
    DEFAULT_WORKSPACE_NAME, PaletteCategory, PaletteCommandSpec, PaletteMode, PaletteRow,
    PaletteStats, STARRED_WORKSPACE_ID, now_unix_ms, rank_palette,
};
use multibot_keys::{ALL_CATEGORIES, by_category};
use multibot_proto::{AgentKind, ClientRequest};

pub(crate) struct SessionDirectoryPicker {
    agent: AgentKind,
    directories: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) enum ReminderTarget {
    Session(String),
    Workspace(String),
}

#[derive(Default)]
pub(crate) struct PaletteState {
    pub server_shells: bool,
    pub shell_kill_target: Option<String>,
    pub stats: PaletteStats,
    pub reminder_target: Option<ReminderTarget>,
    pub hibernate_wake_on_reply: bool,
    pub directory_picker: Option<SessionDirectoryPicker>,
    pub dissolve_workspace_id: Option<String>,
    pub provider_picker: bool,
    pub selected_index: usize,
    pub mode: PaletteMode,
    pub input_sub: Option<Subscription>,
    pub persist_to_disk: bool,
    pub scroll_handle: ScrollHandle,
}

impl MultibotView {
    pub(crate) fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.table_modal_state.is_open() {
            self.table_modal_state.close();
        }
        #[cfg(windows)]
        self.hide_browser_pane();
        self.active_modal = ActiveModal::Palette;
        self.palette_state.provider_picker = false;
        self.palette_state.mode = PaletteMode::Global;
        self.palette_state.server_shells = false;
        self.palette_state.shell_kill_target = None;
        self.palette_state.reminder_target = None;
        self.palette_state.directory_picker = None;
        self.palette_state.dissolve_workspace_id = None;
        self.palette_state.selected_index = 0;
        self.palette_state.scroll_handle.scroll_to_item(0);
        self.ensure_palette_input_subscription(window, cx);
        self.palette_input.update(cx, |state, cx| {
            state.set_value("", window, cx);
            state.focus(window, cx);
        });
        cx.notify();
    }

    pub(crate) fn open_workspace_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_state.rename = None;
        self.open_palette(window, cx);
        self.palette_state.mode = PaletteMode::Workspaces;
    }

    pub(crate) fn request_dissolve_workspace(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.sidebar_state.active_workspace_id.clone();
        if self
            .sidebar_state
            .workspace_store
            .workspace_by_id(&id)
            .is_none()
        {
            self.notice = Some("Choose a workspace before dissolving it.".into());
            cx.notify();
            return;
        }
        if self.workspace_live_session_keys(&id).is_empty() {
            self.dissolve_workspace(&id, cx);
            return;
        }
        self.open_palette(window, cx);
        self.palette_state.dissolve_workspace_id = Some(id);
        cx.notify();
    }

    pub(crate) fn open_palette_tab_context(
        &mut self,
        session_key: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.table_modal_state.is_open() {
            self.table_modal_state.close();
        }
        #[cfg(windows)]
        self.hide_browser_pane();
        self.active_modal = ActiveModal::Palette;
        self.palette_state.provider_picker = false;
        self.palette_state.reminder_target = None;
        self.palette_state.directory_picker = None;
        self.palette_state.dissolve_workspace_id = None;
        self.palette_state.mode = PaletteMode::TabContext {
            target_session_key: session_key,
        };
        self.palette_state.selected_index = 0;
        self.palette_state.scroll_handle.scroll_to_item(0);
        self.ensure_palette_input_subscription(window, cx);
        self.palette_input.update(cx, |state, cx| {
            state.set_value("", window, cx);
            state.focus(window, cx);
        });
        cx.notify();
    }

    pub(crate) fn open_reminder_picker(
        &mut self,
        target: ReminderTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = match target {
            ReminderTarget::Workspace(id) => {
                let id = if self
                    .sidebar_state
                    .workspace_store
                    .workspace_by_id(&id)
                    .is_some()
                {
                    id
                } else if let Some(key) = self.target_triage_session_key() {
                    self.sidebar_state
                        .workspace_store
                        .effective_workspace_id(&key)
                } else {
                    return;
                };
                ReminderTarget::Workspace(id)
            }
            target => target,
        };
        self.open_palette(window, cx);
        self.palette_state.hibernate_wake_on_reply = true;
        self.palette_state.reminder_target = Some(target);
        self.palette_state.directory_picker = None;
        self.palette_state.dissolve_workspace_id = None;
    }

    fn ensure_palette_input_subscription(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette_state.input_sub.is_some() {
            return;
        }
        let sub = cx.subscribe_in(
            &self.palette_input,
            window,
            |this, _input, event, window, cx| {
                if this.active_modal != ActiveModal::Palette {
                    return;
                }
                match event {
                    InputEvent::Change => {
                        this.palette_state.selected_index = 0;
                        this.palette_state.scroll_handle.scroll_to_item(0);
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } => {
                        this.execute_selected_palette_row(Some(window), cx);
                    }
                    InputEvent::Focus | InputEvent::Blur => {}
                }
            },
        );
        self.palette_state.input_sub = Some(sub);
    }

    fn persist_palette_stats_if_enabled(&self) {
        if self.palette_state.persist_to_disk {
            let _ = self.palette_state.stats.save();
        }
    }

    pub(crate) fn build_palette_command_catalog(&self, now_ms: i64) -> Vec<PaletteCommandSpec> {
        let mut cmds = vec![
            // Sessions — `"pal-cmd-inbox"` MUST remain first safe item so empty-query open
            // keeps it at the top of the 520px palette viewport (`app.rs` headless test contract).
            PaletteCommandSpec {
                id: "pal-cmd-inbox".into(),
                label: "View: Inbox".into(),
                chord: "Alt+I".into(),
                category: PaletteCategory::Sessions,
                destructive: false,
                aliases: vec![
                    "inbox".into(),
                    "messages".into(),
                    "open cross-workspace inbox".into(),
                ],
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-switch-provider".into(),
                label: "Session: Switch provider / harness".into(),
                chord: String::new(),
                category: PaletteCategory::Sessions,
                destructive: false,
                aliases: [
                    "switch provider",
                    "switch harness",
                    "claude",
                    "codex",
                    "antigravity",
                    "pi",
                    "cursor",
                ]
                .into_iter()
                .map(String::from)
                .collect(),
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-server-shells".into(),
                label: "Server shells".into(),
                chord: String::new(),
                category: PaletteCategory::Sessions,
                destructive: false,
                aliases: [
                    "tmux",
                    "shells",
                    "detached",
                    "unattached",
                    "kill shell",
                    "recover shells",
                    "server shells",
                ]
                .into_iter()
                .map(str::to_string)
                .collect(),
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-rename".into(),
                label: "Rename session".into(),
                chord: "F2".into(),
                category: PaletteCategory::Sessions,
                destructive: false,
                aliases: vec!["title".into(), "edit name".into()],
                tab_context: true,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-favorite".into(),
                label: "Favorite / unfavorite session".into(),
                chord: "Alt+S".into(),
                category: PaletteCategory::Sessions,
                destructive: false,
                aliases: vec!["star".into(), "unstar".into()],
                tab_context: true,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-pin".into(),
                label: "Pin / unpin session to top".into(),
                chord: String::new(),
                category: PaletteCategory::Sessions,
                destructive: false,
                aliases: vec!["pin".into(), "unpin".into()],
                tab_context: true,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-mark-unread".into(),
                label: "Toggle read / unread".into(),
                chord: "Alt+U".into(),
                category: PaletteCategory::Sessions,
                destructive: false,
                aliases: vec!["unread".into(), "mark read".into()],
                tab_context: true,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-remind".into(),
                label: "Hibernate".into(),
                chord: "Alt+H".into(),
                category: PaletteCategory::Sessions,
                destructive: false,
                aliases: vec!["reminder".into()],
                tab_context: true,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-waiting".into(),
                label: "Done".into(),
                chord: "Alt+E".into(),
                category: PaletteCategory::Sessions,
                destructive: false,
                aliases: vec!["waiting".into(), "snooze session".into()],
                tab_context: true,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-undo-waiting".into(),
                label: "Undo last Done".into(),
                chord: "Alt+Z".into(),
                category: PaletteCategory::Sessions,
                destructive: false,
                aliases: vec!["restore waiting".into()],
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-interrupt".into(),
                label: "Session: Interrupt active turn".into(),
                chord: "Esc".into(),
                category: PaletteCategory::Sessions,
                destructive: false,
                aliases: vec!["stop".into(), "cancel turn".into()],
                tab_context: true,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-spawn-claude".into(),
                label: "Session: New Claude session".into(),
                chord: "Ctrl+T".into(),
                category: PaletteCategory::Sessions,
                destructive: false,
                aliases: vec!["new claude".into(), "spawn claude".into()],
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-spawn-codex".into(),
                label: "Session: New Codex session".into(),
                chord: String::new(),
                category: PaletteCategory::Sessions,
                destructive: false,
                aliases: vec!["new codex".into(), "spawn codex".into()],
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-spawn-antigravity".into(),
                label: "Session: New Antigravity session".into(),
                chord: String::new(),
                category: PaletteCategory::Sessions,
                destructive: false,
                aliases: vec!["new agy".into(), "spawn agy".into(), "antigravity".into()],
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-spawn-pi".into(),
                label: "Session: New Pi session".into(),
                chord: String::new(),
                category: PaletteCategory::Sessions,
                destructive: false,
                aliases: vec!["new pi".into(), "spawn pi".into()],
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-spawn-shell".into(),
                label: "Session: New Shell session".into(),
                chord: String::new(),
                category: PaletteCategory::Sessions,
                destructive: false,
                aliases: vec![
                    "new shell".into(),
                    "spawn shell".into(),
                    "tmux".into(),
                    "terminal".into(),
                ],
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-unarchive-session".into(),
                label: "Reactivate".into(),
                chord: "Ctrl+Shift+T".into(),
                category: PaletteCategory::Sessions,
                destructive: false,
                aliases: vec!["restore".into(), "reopen".into()],
                tab_context: true,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-inbox-exclude".into(),
                label: "Inbox: Never show / show again".into(),
                chord: String::new(),
                category: PaletteCategory::Sessions,
                destructive: false,
                aliases: vec!["mute inbox".into(), "exclude".into()],
                tab_context: true,
                global: true,
            },
            PaletteCommandSpec {
                id: "session.delete".into(),
                label: "Dissolve".into(),
                chord: "Ctrl+Shift+W".into(),
                category: PaletteCategory::Sessions,
                destructive: true,
                aliases: vec![
                    "close session".into(),
                    "remove session".into(),
                    "delete session".into(),
                ],
                tab_context: true,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-archive-session".into(),
                label: "Move to Inbox".into(),
                chord: String::new(),
                category: PaletteCategory::Sessions,
                destructive: false,
                aliases: vec!["archive".into()],
                tab_context: true,
                global: true,
            },
            // Workspaces and windows
            PaletteCommandSpec {
                id: "pal-cmd-workspaces".into(),
                label: "Workspaces".into(),
                chord: "Alt+Shift+W".into(),
                category: PaletteCategory::WorkspacesAndWindows,
                destructive: false,
                aliases: vec!["switch workspace".into()],
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-ws-starred".into(),
                label: "Switch to Starred workspace".into(),
                chord: "Alt+1".into(),
                category: PaletteCategory::WorkspacesAndWindows,
                destructive: false,
                aliases: vec!["starred".into(), "favorites".into()],
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-ws-new".into(),
                label: "New workspace".into(),
                chord: String::new(),
                category: PaletteCategory::WorkspacesAndWindows,
                destructive: false,
                aliases: vec!["create workspace".into()],
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-window".into(),
                label: "New Multibot Window".into(),
                chord: "Ctrl+N".into(),
                category: PaletteCategory::WorkspacesAndWindows,
                destructive: false,
                aliases: vec!["window".into()],
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-ws-snooze".into(),
                label: "Snooze active workspace…".into(),
                chord: "Ctrl+Shift+Y".into(),
                category: PaletteCategory::WorkspacesAndWindows,
                destructive: false,
                aliases: vec!["snooze".into()],
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "workspace.dissolve".into(),
                label: "Dissolve workspace".into(),
                chord: String::new(),
                category: PaletteCategory::WorkspacesAndWindows,
                destructive: true,
                aliases: vec!["stop workspace agents".into()],
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "workspace.delete".into(),
                label: "Delete workspace".into(),
                chord: String::new(),
                category: PaletteCategory::WorkspacesAndWindows,
                destructive: true,
                aliases: vec!["remove workspace".into()],
                tab_context: false,
                global: true,
            },
            // Sidebar and tabs
            PaletteCommandSpec {
                id: "pal-cmd-sidebar".into(),
                label: "Cycle Sidebar".into(),
                chord: "Ctrl+Shift+B".into(),
                category: PaletteCategory::SidebarAndTabs,
                destructive: false,
                aliases: vec!["toggle sidebar".into()],
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-sidebar-open".into(),
                label: "Expand Sidebar".into(),
                chord: String::new(),
                category: PaletteCategory::SidebarAndTabs,
                destructive: false,
                aliases: vec!["show sidebar".into()],
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-sidebar-rail".into(),
                label: "Collapse Sidebar to Rail".into(),
                chord: String::new(),
                category: PaletteCategory::SidebarAndTabs,
                destructive: false,
                aliases: vec!["rail".into()],
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-sidebar-hide".into(),
                label: "Hide Sidebar".into(),
                chord: String::new(),
                category: PaletteCategory::SidebarAndTabs,
                destructive: false,
                aliases: vec![],
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-unread-only".into(),
                label: "Toggle Unread-Only Filter".into(),
                chord: "Ctrl+Shift+U".into(),
                category: PaletteCategory::SidebarAndTabs,
                destructive: false,
                aliases: vec!["unread filter".into()],
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-group-toggle".into(),
                label: "Collapse / expand session group".into(),
                chord: "Ctrl+G".into(),
                category: PaletteCategory::SidebarAndTabs,
                destructive: false,
                aliases: vec![
                    "group".into(),
                    "collapse group".into(),
                    "expand group".into(),
                ],
                tab_context: true,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-group-assign".into(),
                label: "Add / remove session from group".into(),
                chord: String::new(),
                category: PaletteCategory::SidebarAndTabs,
                destructive: false,
                aliases: vec!["group".into(), "ungroup".into(), "new group".into()],
                tab_context: true,
                global: true,
            },
            // Look and settings
            PaletteCommandSpec {
                id: "pal-cmd-account-sign-in".into(),
                label: "Account: Sign in to Multibot".into(),
                chord: String::new(),
                category: PaletteCategory::LookAndSettings,
                destructive: false,
                aliases: vec!["login".into(), "account".into(), "deepgram".into()],
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-account-sign-out".into(),
                label: "Account: Sign out".into(),
                chord: String::new(),
                category: PaletteCategory::LookAndSettings,
                destructive: true,
                aliases: vec!["logout".into(), "account".into()],
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-settings".into(),
                label: "Open Settings".into(),
                chord: "Ctrl+,".into(),
                category: PaletteCategory::LookAndSettings,
                destructive: false,
                aliases: vec!["preferences".into(), "gpu".into()],
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-import".into(),
                label: "Import SSH Hosts from ZigShell (read-only)".into(),
                chord: String::new(),
                category: PaletteCategory::LookAndSettings,
                destructive: false,
                aliases: vec!["zigshel".into(), "import".into()],
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-shortcuts".into(),
                label: "Show Keyboard Shortcuts".into(),
                chord: "F1".into(),
                category: PaletteCategory::LookAndSettings,
                destructive: false,
                aliases: vec!["keys".into(), "help".into()],
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-hosts".into(),
                label: "Connect Host / Switch Host".into(),
                chord: String::new(),
                category: PaletteCategory::LookAndSettings,
                destructive: false,
                aliases: vec![
                    "connect".into(),
                    "ssh".into(),
                    "hosts".into(),
                    "local".into(),
                ],
                tab_context: false,
                global: true,
            },
            // Debug and repair
            PaletteCommandSpec {
                id: "pal-cmd-chat-info".into(),
                label: "Chat: Chat info".into(),
                chord: String::new(),
                category: PaletteCategory::ChatView,
                destructive: false,
                aliases: [
                    "chat info",
                    "session info",
                    "working directory",
                    "cwd",
                    "model",
                    "context window",
                ]
                .into_iter()
                .map(str::to_string)
                .collect(),
                tab_context: true,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-copy-id".into(),
                label: "Copy Chat ID".into(),
                chord: String::new(),
                category: PaletteCategory::DebugAndRepair,
                destructive: false,
                aliases: vec!["session id".into()],
                tab_context: true,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-refresh".into(),
                label: "Refresh Sessions".into(),
                chord: "Ctrl+Shift+R".into(),
                category: PaletteCategory::DebugAndRepair,
                destructive: false,
                aliases: vec!["reload".into()],
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-dictation".into(),
                label: "Chat View: Toggle Dictation".into(),
                chord: "Ctrl+Shift+Space".into(),
                category: PaletteCategory::LookAndSettings,
                destructive: false,
                aliases: vec!["voice".into(), "microphone".into(), "deepgram".into()],
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-fullwidth".into(),
                label: "Toggle Full-Width".into(),
                chord: "Ctrl+Shift+F".into(),
                category: PaletteCategory::LookAndSettings,
                destructive: false,
                aliases: vec!["fullscreen".into(), "shell".into()],
                tab_context: false,
                global: true,
            },
            // Terminal (Slice 5)
            PaletteCommandSpec {
                id: "pal-cmd-toggle-terminal".into(),
                label: "Toggle Terminal".into(),
                chord: "Alt+C".into(),
                category: PaletteCategory::SidebarAndTabs,
                destructive: false,
                aliases: vec![
                    "terminal".into(),
                    "shell".into(),
                    "ssh".into(),
                    "console".into(),
                ],
                tab_context: false,
                global: true,
            },
            // Computer View (Slice 5, MBRU-7): no dedicated chord by design
            PaletteCommandSpec {
                id: "pal-cmd-toggle-computer-view".into(),
                label: "Toggle Computer View".into(),
                chord: String::new(),
                category: PaletteCategory::LookAndSettings,
                destructive: false,
                aliases: vec![
                    "computer".into(),
                    "vnc".into(),
                    "novnc".into(),
                    "desktop".into(),
                    "remote screen".into(),
                    "preview".into(),
                ],
                // Reachable from Ctrl+K everywhere and from the chat `...` menu, which
                // opens the palette in tab context.
                tab_context: true,
                global: true,
            },
            // Docked browser pane
            PaletteCommandSpec {
                id: "pal-cmd-browser-toggle".into(),
                label: "Toggle Browser Pane".into(),
                chord: "Alt+A".into(),
                category: PaletteCategory::Browser,
                destructive: false,
                aliases: vec!["browser".into()],
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-browser-swap".into(),
                label: "Jump Focus Between Composer and Browser".into(),
                chord: "Alt+Q".into(),
                category: PaletteCategory::Browser,
                destructive: false,
                aliases: vec![],
                tab_context: false,
                global: true,
            },
            // Account and usage
            PaletteCommandSpec {
                id: "pal-cmd-usage".into(),
                label: "Show Footprint & Status".into(),
                chord: "Ctrl+Shift+G".into(),
                category: PaletteCategory::AccountAndUsage,
                destructive: false,
                aliases: vec!["memory".into(), "ram".into()],
                tab_context: false,
                global: true,
            },
            // Chat View
            PaletteCommandSpec {
                id: "pal-cmd-copy-code".into(),
                label: "Copy Latest Code Block".into(),
                chord: "Ctrl+Shift+D".into(),
                category: PaletteCategory::ChatView,
                destructive: false,
                aliases: vec!["code".into()],
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-expand-table".into(),
                label: "Expand Table".into(),
                chord: String::new(),
                category: PaletteCategory::ChatView,
                destructive: false,
                aliases: vec!["table".into()],
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-toggle-tools".into(),
                label: if self.show_tool_calls() {
                    "Chat: Hide tool calls"
                } else {
                    "Chat: Show tool calls"
                }
                .into(),
                chord: String::new(),
                category: PaletteCategory::ChatView,
                destructive: false,
                aliases: vec!["tools".into()],
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-toggle-todos".into(),
                label: "Toggle Todo List".into(),
                chord: String::new(),
                category: PaletteCategory::ChatView,
                destructive: false,
                aliases: vec!["todos".into()],
                tab_context: false,
                global: true,
            },
            PaletteCommandSpec {
                id: "pal-cmd-load-older".into(),
                label: "Chat: Load older history".into(),
                chord: String::new(),
                category: PaletteCategory::ChatView,
                destructive: false,
                aliases: vec!["older".into(), "more history".into(), "paginate".into()],
                tab_context: false,
                global: true,
            },
        ];
        for (id, label, chord) in [
            ("pal-view-done", "View: Done", "Alt+Shift+E"),
            ("pal-view-hibernated", "View: Hibernated", "Alt+Shift+H"),
            ("pal-view-dissolved", "View: Dissolved", "Alt+Shift+D"),
        ] {
            cmds.push(PaletteCommandSpec {
                id: id.into(),
                label: label.into(),
                chord: chord.into(),
                category: PaletteCategory::Sessions,
                destructive: false,
                aliases: vec![],
                tab_context: false,
                global: true,
            });
        }

        // Dynamic "Wake <ws>" commands for snoozed workspaces
        for ws in self
            .sidebar_state
            .workspace_store
            .snoozed_workspaces(now_ms)
        {
            cmds.push(PaletteCommandSpec {
                id: format!("pal-cmd-wake:{}", ws.id),
                label: format!("Wake {}", ws.name),
                chord: String::new(),
                category: PaletteCategory::WorkspacesAndWindows,
                destructive: false,
                aliases: vec!["unsnooze".into(), ws.name.clone()],
                tab_context: false,
                global: true,
            });
        }

        // Dynamic "Move to <ws>" actions in tab-context mode
        for ws in self
            .sidebar_state
            .workspace_store
            .visible_workspaces(now_ms)
        {
            cmds.push(PaletteCommandSpec {
                id: format!("pal-cmd-move-ws:{}", ws.id),
                label: format!("Move to workspace: {}", ws.name),
                chord: String::new(),
                category: PaletteCategory::WorkspacesAndWindows,
                destructive: false,
                aliases: vec![ws.name.clone()],
                tab_context: true,
                global: false,
            });
        }

        // Pluggable updater commands contributed by `update_ui.rs` (M2)
        for entry in self.update_palette_commands() {
            cmds.push(PaletteCommandSpec {
                id: entry.id,
                label: entry.label,
                chord: entry.chord,
                category: PaletteCategory::from_label(entry.category),
                destructive: false,
                aliases: vec![entry.category.to_string()],
                tab_context: false,
                global: true,
            });
        }

        cmds
    }

    fn start_agent_session(
        &mut self,
        agent: AgentKind,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        let directories = self.sidebar_state.workspace_store.workspace_directories(
            &self.sidebar_state.active_workspace_id,
            self.active_session_host(),
            &self.session_inputs(),
        );
        if directories.len() <= 1 {
            let cwd = directories
                .into_iter()
                .next()
                .unwrap_or_else(|| self.default_spawn_cwd());
            self.spawn_new_session(agent, cwd, None, None, cx);
            return;
        }
        self.active_modal = ActiveModal::Palette;
        self.palette_state.provider_picker = false;
        self.palette_state.directory_picker = Some(SessionDirectoryPicker { agent, directories });
        self.palette_state.selected_index = 0;
        self.palette_state.scroll_handle.scroll_to_item(0);
        if let Some(window) = window {
            self.palette_input.update(cx, |state, cx| {
                state.set_value("", window, cx);
                state.focus(window, cx);
            });
        }
        cx.notify();
    }

    pub(crate) fn build_palette_rows(&self, query: &str, now_ms: i64) -> Vec<PaletteRow> {
        if self.palette_state.provider_picker {
            return self.provider_picker_rows(query);
        }
        if self.palette_state.shell_kill_target.is_some() {
            return [
                ("pal-shell-kill-confirm", "End shell permanently", true),
                ("pal-shell-kill-cancel", "Cancel", false),
            ]
            .into_iter()
            .map(|(id, label, destructive)| PaletteRow::Command {
                id: id.into(),
                label: label.into(),
                chord: "Enter · Esc cancels".into(),
                category: PaletteCategory::Sessions,
                destructive,
                match_rank: 0,
                frecency: 0,
            })
            .collect();
        }
        if self.palette_state.server_shells {
            let query = query.trim().to_lowercase();
            return self
                .session_inputs()
                .into_iter()
                .filter(|input| {
                    input.summary.agent == AgentKind::Shell
                        && (!input.summary.archived || input.summary.shell_attached.is_some())
                        && self.tx_for_host(input.host).is_some()
                })
                .filter_map(|input| {
                    let shell = input.summary;
                    let title = self
                        .sidebar_state
                        .workspace_store
                        .effective_title(&input.key, &shell.title);
                    let preview = shell
                        .last_assistant_preview
                        .clone()
                        .or_else(|| shell.cwd.clone())
                        .unwrap_or_default();
                    let attached = match shell.shell_attached {
                        Some(true) => "Attached",
                        Some(false) => "Detached",
                        None => "Live",
                    };
                    let title = format!("{title} · {}", shell.id);
                    let host = format!("{} · {attached}", input.host);
                    if !format!("{title} {host} {preview}")
                        .to_lowercase()
                        .contains(&query)
                    {
                        return None;
                    }
                    Some(PaletteRow::Session {
                        session_key: input.key,
                        title,
                        agent: AgentKind::Shell,
                        cwd: Some(preview),
                        workspace_name: host,
                        match_rank: 0,
                        frecency: 0,
                    })
                })
                .collect();
        }
        if self.palette_state.dissolve_workspace_id.is_some() {
            return vec![PaletteRow::Command {
                id: "workspace.dissolve.confirm".into(),
                label: "Dissolve workspace and stop agents".into(),
                chord: "Enter · Esc cancels".into(),
                category: PaletteCategory::WorkspacesAndWindows,
                destructive: true,
                match_rank: 0,
                frecency: 0,
            }];
        }
        if let Some(picker) = &self.palette_state.directory_picker {
            let query = query.trim().to_lowercase();
            return picker
                .directories
                .iter()
                .enumerate()
                .filter(|(_, cwd)| cwd.to_lowercase().contains(&query))
                .map(|(idx, cwd)| PaletteRow::Command {
                    id: format!("pal-spawn-directory-{idx}"),
                    label: cwd.clone(),
                    chord: String::new(),
                    category: PaletteCategory::Sessions,
                    destructive: false,
                    match_rank: 0,
                    frecency: 0,
                })
                .collect();
        }
        if let Some(target) = &self.palette_state.reminder_target {
            let clear = match target {
                ReminderTarget::Session(_) => "Move to Inbox",
                ReminderTarget::Workspace(_) => "Unsnooze",
            };
            return [
                "In 15 minutes",
                "In 1 hour",
                "In 3 hours",
                "Tomorrow 9am",
                clear,
            ]
            .into_iter()
            .enumerate()
            .map(|(idx, label)| PaletteRow::Command {
                id: format!("pal-reminder-{idx}"),
                label: label.into(),
                chord: String::new(),
                category: PaletteCategory::Sessions,
                destructive: false,
                match_rank: 0,
                frecency: 0,
            })
            .collect();
        }
        let inputs = self.session_inputs();
        let commands = self.build_palette_command_catalog(now_ms);
        let current_key = self
            .selected_session
            .as_ref()
            .map(|s| self.session_key_for(s));

        rank_palette(
            &self.sidebar_state.workspace_store,
            &self.palette_state.stats,
            &inputs,
            &commands,
            query,
            &self.palette_state.mode,
            current_key.as_deref(),
            self.sidebar_state.previous_session_key.as_deref(),
            now_ms,
        )
    }

    fn scroll_palette_to_selected(&self, rows: &[PaletteRow]) {
        let mut selectable_idx = 0usize;
        for (raw_idx, row) in rows.iter().enumerate() {
            if !matches!(row, PaletteRow::Header(_)) {
                if selectable_idx == self.palette_state.selected_index {
                    self.palette_state.scroll_handle.scroll_to_item(raw_idx);
                    return;
                }
                selectable_idx += 1;
            }
        }
    }

    fn execute_selected_palette_row(
        &mut self,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        let query = self.palette_input.read(cx).value().to_string();
        let now = now_unix_ms();
        let rows = self.build_palette_rows(&query, now);
        let selectable: Vec<PaletteRow> = rows
            .into_iter()
            .filter(|r| !matches!(r, PaletteRow::Header(_)))
            .collect();
        if selectable.is_empty() {
            return;
        }
        let idx = self.palette_state.selected_index.min(selectable.len() - 1);
        let chosen = selectable[idx].clone();
        self.execute_palette_row(chosen, now, window, cx);
    }

    pub(crate) fn execute_palette_row(
        &mut self,
        row: PaletteRow,
        now_ms: i64,
        mut window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        self.execute_palette_row_inner(row, now_ms, window.as_deref_mut(), cx);
        if let Some(win) = window {
            let palette_still_focused = self.active_modal != ActiveModal::Palette
                && self.palette_input.read(cx).focus_handle(cx).is_focused(win);
            if palette_still_focused {
                self.focus_active_surface_default(win, cx);
            }
        }
    }

    fn execute_palette_row_inner(
        &mut self,
        row: PaletteRow,
        now_ms: i64,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        if self.palette_state.provider_picker {
            if let PaletteRow::Command { id, .. } = row {
                if let Some(index) = id
                    .strip_prefix("pal-provider-")
                    .and_then(|i| i.parse::<usize>().ok())
                {
                    let choices = self.provider_choices();
                    if let Some(choice) = choices.get(index) {
                        self.request_harness_switch(choice.clone(), cx);
                    }
                }
            }
            self.palette_state.provider_picker = false;
            self.active_modal = ActiveModal::None;
            return;
        }
        if let Some(key) = self.palette_state.shell_kill_target.take() {
            if matches!(row, PaletteRow::Command { ref id, .. } if id == "pal-shell-kill-confirm") {
                self.dissolve_session_key_confirmed(&key, cx);
            }
            self.palette_state.selected_index = 0;
            if let Some(window) = window {
                self.palette_input
                    .update(cx, |input, cx| input.set_value("", window, cx));
            }
            cx.notify();
            return;
        }
        if matches!(row, PaletteRow::Command { ref id, .. } if id == "pal-cmd-server-shells") {
            self.palette_state.server_shells = true;
            self.palette_state.selected_index = 0;
            self.active_modal = ActiveModal::Palette;
            let hosts: std::collections::HashSet<_> = self
                .session_inputs()
                .iter()
                .map(|input| input.host.to_string())
                .chain(self.host_txs.keys().cloned())
                .chain(std::iter::once(self.current_host_label().to_string()))
                .collect();
            for host in hosts {
                if let Some(tx) = self.tx_for_host(&host) {
                    let _ = tx.send(ClientRequest::ListSessions {
                        limit: Some(200),
                        include_archived: Some(true),
                    });
                }
            }
            if let Some(window) = window {
                self.palette_input.update(cx, |input, cx| {
                    input.set_value("", window, cx);
                    input.focus(window, cx);
                });
            }
            cx.notify();
            return;
        }
        if self.palette_state.server_shells
            && let PaletteRow::Session {
                ref session_key, ..
            } = row
        {
            let key = session_key.clone();
            if self.reactivate_session_key(&key, cx) {
                self.open_session_key_with_tracking(&key, false, cx);
            }
            self.palette_state.server_shells = false;
            cx.notify();
            return;
        }
        if let Some(id) = self.palette_state.dissolve_workspace_id.take() {
            self.active_modal = ActiveModal::None;
            if matches!(row, PaletteRow::Command { ref id, .. } if id == "workspace.dissolve.confirm")
            {
                self.dissolve_workspace(&id, cx);
            }
            cx.notify();
            return;
        }
        if let Some(picker) = self.palette_state.directory_picker.take() {
            if let PaletteRow::Command { id, .. } = row
                && let Some(idx) = id
                    .strip_prefix("pal-spawn-directory-")
                    .and_then(|s| s.parse::<usize>().ok())
                && let Some(cwd) = picker.directories.get(idx)
            {
                self.active_modal = ActiveModal::None;
                self.spawn_new_session(picker.agent, cwd.clone(), None, None, cx);
            }
            return;
        }
        if let Some(target) = self.palette_state.reminder_target.take() {
            let PaletteRow::Command { id, .. } = row else {
                return;
            };
            let at = match id.as_str() {
                "pal-reminder-0" => Some(now_ms.saturating_add(15 * 60 * 1000)),
                "pal-reminder-1" => Some(now_ms.saturating_add(60 * 60 * 1000)),
                "pal-reminder-2" => Some(now_ms.saturating_add(3 * 60 * 60 * 1000)),
                "pal-reminder-3" => {
                    use chrono::{Local, TimeZone};
                    let tomorrow = Local::now()
                        .date_naive()
                        .succ_opt()
                        .and_then(|day| day.and_hms_opt(9, 0, 0))
                        .and_then(|time| Local.from_local_datetime(&time).earliest());
                    let Some(tomorrow) = tomorrow else {
                        self.notice =
                            Some("Tomorrow 9am is unavailable; choose a duration instead".into());
                        self.palette_state.reminder_target = Some(target);
                        self.palette_state.directory_picker = None;
                        cx.notify();
                        return;
                    };
                    Some(tomorrow.timestamp_millis())
                }
                "pal-reminder-4" => None,
                _ => return,
            };
            match target {
                ReminderTarget::Session(key) => {
                    let note = self.palette_input.read(cx).value().to_string();
                    if let Some(session) = self.find_session_by_key(&key) {
                        if session.archived {
                            self.reactivate_session_key(&key, cx);
                        }
                        self.sidebar_state.workspace_store.hibernate_session(
                            &key,
                            &session,
                            at,
                            Some(&note),
                            self.palette_state.hibernate_wake_on_reply,
                        );
                    }
                }
                ReminderTarget::Workspace(id) => {
                    self.active_modal = ActiveModal::None;
                    self.set_workspace_snooze(&id, at, cx);
                    return;
                }
            }
            self.active_modal = ActiveModal::None;
            self.persist_workspace_store_if_enabled();
            self.sync_workspace_to_shared(cx);
            cx.notify();
            return;
        }

        let explicit_tab_target = self
            .palette_state
            .mode
            .target_session_key()
            .map(str::to_string);
        let target_key = explicit_tab_target
            .clone()
            .or_else(|| self.target_triage_session_key());

        self.active_modal = ActiveModal::None;
        self.palette_state.mode = PaletteMode::Global;
        self.palette_state.reminder_target = None;
        self.palette_state.directory_picker = None;
        self.palette_state.dissolve_workspace_id = None;

        match row {
            PaletteRow::Header(_) => {}
            PaletteRow::Session { session_key, .. } => {
                if let Some((host, sess)) = self.host_for_session_key(&session_key) {
                    self.open_session_on_host(host, sess, false, cx);
                }
            }
            PaletteRow::Workspace {
                workspace_id,
                view,
                archived,
                ..
            } => {
                if archived {
                    let _ = self
                        .sidebar_state
                        .workspace_store
                        .restore_workspace(&workspace_id);
                }
                self.sidebar_state
                    .workspace_store
                    .wake_workspace(&workspace_id);
                self.sidebar_state.active_workspace_id = workspace_id;
                self.sidebar_state.active_workspace_view = view;
                self.sidebar_state.rename = None;
                self.cleanup_unmounted_surface_state();
                self.persist_workspace_store_if_enabled();
                self.sync_workspace_to_shared(cx);
                cx.notify();
            }
            PaletteRow::Command { id, .. } => {
                self.palette_state.stats.record_command_use(&id, now_ms);
                self.persist_palette_stats_if_enabled();

                if id == "pal-cmd-switch-provider" {
                    if let Some(window) = window {
                        self.open_provider_picker(window, cx);
                    }
                    return;
                }
                if self.execute_update_palette_command(&id, cx) {
                    cx.notify();
                    return;
                }

                if let Some(ws_id) = id.strip_prefix("pal-cmd-wake:") {
                    self.sidebar_state.workspace_store.wake_workspace(ws_id);
                    self.sidebar_state.active_workspace_id = ws_id.to_string();
                    self.sidebar_state.rename = None;
                    self.cleanup_unmounted_surface_state();
                    self.persist_workspace_store_if_enabled();
                    self.sync_workspace_to_shared(cx);
                    cx.notify();
                    return;
                }

                if let Some(ws_id) = id.strip_prefix("pal-cmd-move-ws:") {
                    if let Some(ref key) = target_key {
                        self.sidebar_state
                            .workspace_store
                            .assign_session_workspace(key, ws_id);
                        self.cleanup_unmounted_surface_state();
                        self.persist_workspace_store_if_enabled();
                        self.sync_workspace_to_shared(cx);
                    }
                    cx.notify();
                    return;
                }

                match id.as_str() {
                    "pal-cmd-workspaces" => {
                        if let Some(window) = window {
                            self.open_workspace_switcher(window, cx);
                        }
                        return;
                    }
                    "pal-cmd-account-sign-in" | "pal-cmd-account-sign-out" => {
                        self.settings_tab = crate::settings::SettingsTab::Account;
                        self.set_main_surface(MainSurface::Settings);
                        let job = if id == "pal-cmd-account-sign-in" {
                            crate::account::AccountJob::SignIn
                        } else {
                            self.voice.cancel();
                            crate::account::AccountJob::SignOut
                        };
                        self.start_account_job(job, cx);
                    }
                    "pal-cmd-inbox" => {
                        self.sidebar_state.rename = None;
                        self.sidebar_state.new_workspace_draft = None;
                        // Opening the Inbox always lands on `All`, never on a hidden empty split
                        self.inbox_state.select_all_split();
                        self.inbox_state.lifecycle =
                            multibot_core::workspaces::SessionLifecycle::Active;
                        self.set_main_surface(MainSurface::Inbox);
                        if let Some(win) = window {
                            self.focus_inbox(win, cx);
                        }
                    }
                    "pal-cmd-chat-info" => {
                        if let Some(win) = window {
                            self.toggle_chat_info(win, cx);
                        }
                        return;
                    }
                    "pal-cmd-dictation" => {
                        if let Some(win) = window {
                            self.toggle_dictation(win, cx);
                        }
                        return;
                    }
                    "pal-cmd-fullwidth" => {
                        self.toggle_shell_fullscreen(cx);
                    }
                    "pal-cmd-toggle-terminal" => {
                        self.toggle_terminal_pane(cx);
                        return;
                    }
                    "pal-cmd-browser-toggle" => {
                        if let Some(win) = window {
                            self.toggle_browser_pane(win, cx);
                        }
                    }
                    "pal-cmd-browser-swap" => {
                        if let Some(win) = window {
                            self.focus_browser_pane(win, cx);
                        }
                    }
                    "pal-cmd-toggle-computer-view" => {
                        // The palette runs over the chat surface; the pane docks beside it.
                        if let Some(win) = window {
                            self.toggle_computer_view(win, cx);
                        }
                        return;
                    }
                    "pal-cmd-hosts" => {
                        self.settings_tab = crate::settings::SettingsTab::Hosts;
                        self.set_main_surface(MainSurface::Settings);
                    }
                    "pal-cmd-settings" => {
                        self.sidebar_state.rename = None;
                        self.sidebar_state.new_workspace_draft = None;
                        self.set_main_surface(MainSurface::Settings);
                    }
                    "pal-cmd-sidebar" => {
                        self.cycle_sidebar_mode_synced(cx);
                        if let Some(win) = window
                            && self.sidebar_state.mode == SidebarMode::Open
                            && (self.is_sidebar_rename_mounted()
                                || self.is_sidebar_workspace_draft_mounted())
                        {
                            self.focus_sidebar(win, cx);
                        }
                        return;
                    }
                    "pal-cmd-sidebar-open" => {
                        self.set_sidebar_mode_synced(SidebarMode::Open, cx);
                        if let Some(win) = window
                            && (self.is_sidebar_rename_mounted()
                                || self.is_sidebar_workspace_draft_mounted())
                        {
                            self.focus_sidebar(win, cx);
                        }
                        return;
                    }
                    "pal-cmd-sidebar-rail" => {
                        self.set_sidebar_mode_synced(SidebarMode::Collapsed, cx);
                        return;
                    }
                    "pal-cmd-sidebar-hide" => {
                        self.set_sidebar_mode_synced(SidebarMode::Hidden, cx);
                        return;
                    }
                    "pal-cmd-unread-only" => {
                        self.toggle_unread_only(cx);
                        return;
                    }
                    "pal-cmd-group-toggle" => {
                        if let Some(ref key) = target_key {
                            let ws_id = if self.sidebar_state.active_workspace_id
                                == multibot_core::workspaces::ALL_WORKSPACES_ID
                            {
                                self.sidebar_state
                                    .workspace_store
                                    .effective_workspace_id(key)
                            } else {
                                self.sidebar_state.active_workspace_id.clone()
                            };
                            if self
                                .sidebar_state
                                .workspace_store
                                .toggle_session_group_collapsed(key, &ws_id)
                                .is_some()
                            {
                                self.sidebar_state.rename = None;
                                self.cleanup_unmounted_surface_state();
                                self.persist_workspace_store_if_enabled();
                            }
                        } else {
                            self.toggle_active_group(cx);
                            return;
                        }
                    }
                    "pal-cmd-group-assign" => {
                        if let Some(ref key) = target_key {
                            let ws_id = if self.sidebar_state.active_workspace_id
                                == multibot_core::workspaces::ALL_WORKSPACES_ID
                            {
                                self.sidebar_state
                                    .workspace_store
                                    .effective_workspace_id(key)
                            } else {
                                self.sidebar_state.active_workspace_id.clone()
                            };
                            self.sidebar_state
                                .workspace_store
                                .toggle_group_for_session(key, &ws_id, None, "Group");
                            self.cleanup_unmounted_surface_state();
                            self.persist_workspace_store_if_enabled();
                        }
                    }
                    "pal-cmd-window" => {
                        self.open_extra_window(cx);
                    }
                    "pal-cmd-import" => {
                        self.import_from_zigshel(cx);
                    }
                    "pal-cmd-shortcuts" => {
                        self.active_modal = ActiveModal::Shortcuts;
                    }
                    "pal-cmd-ws-starred" => {
                        self.switch_workspace_digit(1, cx);
                        return;
                    }
                    "pal-cmd-ws-new" => {
                        let next_num = self.sidebar_state.workspace_store.workspaces.len() + 1;
                        if let Ok(ws) = self
                            .sidebar_state
                            .workspace_store
                            .create_workspace(&format!("Workspace {next_num}"), None)
                        {
                            self.sidebar_state.active_workspace_id = ws.id;
                            self.sidebar_state.rename = None;
                            self.cleanup_unmounted_surface_state();
                            self.persist_workspace_store_if_enabled();
                        }
                    }
                    "pal-cmd-remind" => {
                        if let (Some(key), Some(window)) = (target_key, window) {
                            self.open_reminder_picker(ReminderTarget::Session(key), window, cx);
                        }
                        return;
                    }
                    "pal-cmd-ws-snooze" => {
                        let id = self.sidebar_state.active_workspace_id.clone();
                        if let Some(window) = window {
                            self.open_reminder_picker(ReminderTarget::Workspace(id), window, cx);
                        }
                        return;
                    }
                    "workspace.dissolve" => {
                        if let Some(window) = window {
                            self.request_dissolve_workspace(window, cx);
                        }
                        return;
                    }
                    "workspace.delete" => {
                        let cur = self.sidebar_state.active_workspace_id.clone();
                        let keys: Vec<_> = self
                            .session_inputs()
                            .into_iter()
                            .filter(|input| {
                                self.sidebar_state
                                    .workspace_store
                                    .effective_workspace_id(&input.key)
                                    == cur
                            })
                            .map(|input| input.key)
                            .collect();
                        for key in keys {
                            self.sidebar_state
                                .workspace_store
                                .assign_session_workspace(&key, &cur);
                        }
                        let store = &mut self.sidebar_state.workspace_store;
                        let general = store
                            .workspaces
                            .iter()
                            .find(|w| {
                                w.id != cur && !w.archived && w.name == DEFAULT_WORKSPACE_NAME
                            })
                            .map(|w| w.id.clone())
                            .or_else(|| {
                                store
                                    .create_workspace(DEFAULT_WORKSPACE_NAME, None)
                                    .ok()
                                    .map(|w| w.id)
                            });
                        if self
                            .sidebar_state
                            .workspace_store
                            .delete_workspace(&cur, general.as_deref())
                            .is_ok()
                        {
                            self.sidebar_state.active_workspace_id =
                                self.sidebar_state.workspace_store.default_workspace_id();
                            self.sidebar_state.rename = None;
                            self.cleanup_unmounted_surface_state();
                            self.persist_workspace_store_if_enabled();
                        }
                    }
                    "pal-cmd-rename" => {
                        if let Some(key) = explicit_tab_target {
                            // Tab-context rename for an explicit session `key`: never silently
                            // hijack a different Inbox row or start an unmounted rename.
                            if let Some(sess) = self.find_session_by_key(&key) {
                                if self.sidebar_state.mode == SidebarMode::Open
                                    && self.is_session_rendered_in_open_sidebar(&key)
                                {
                                    if self.main_surface == MainSurface::Chat {
                                        self.dismiss_conflicting_chat_overlays_for_rename();
                                    }
                                    if let Some(win) = window {
                                        self.start_sidebar_rename_with_window(key, win, cx);
                                    } else {
                                        self.start_sidebar_rename(key, cx);
                                    }
                                } else if self.main_surface == MainSurface::Inbox
                                    && self.is_session_rendered_in_inbox(&key)
                                {
                                    let rows = self.compute_inbox_rows(now_ms);
                                    if let Some(idx) =
                                        rows.iter().position(|r| r.session_key == key)
                                    {
                                        self.inbox_state.selected_row = idx;
                                    }
                                    let current = self
                                        .sidebar_state
                                        .workspace_store
                                        .effective_title(&key, &sess.title)
                                        .to_string();
                                    self.sidebar_state.rename = None;
                                    self.inbox_state.rename =
                                        Some(InlineRenameState::new(key, sess.id, current));
                                    if let Some(win) = window {
                                        self.focus_inbox(win, cx);
                                    }
                                    cx.notify();
                                } else {
                                    // Target is not mounted in Open sidebar or Inbox (e.g.
                                    // right-clicked a Collapsed rail avatar while on Inbox or
                                    // another Chat session): switch to Chat for that session so
                                    // `#header-rename-input` is visibly mounted and focused.
                                    self.open_session_key_with_tracking(&key, false, cx);
                                    self.dismiss_conflicting_chat_overlays_for_rename();
                                    if let Some(win) = window {
                                        self.start_sidebar_rename_with_window(key, win, cx);
                                    } else {
                                        self.start_sidebar_rename(key, cx);
                                    }
                                }
                            }
                            return;
                        }

                        // Global `Ctrl+K -> Rename session` (no explicit tab target).
                        if self.main_surface == MainSurface::Inbox {
                            if let Some(inbox_key) = self.selected_inbox_session_key()
                                && self.is_session_rendered_in_inbox(&inbox_key)
                                && let Some(sess) = self.find_session_by_key(&inbox_key)
                            {
                                let current = self
                                    .sidebar_state
                                    .workspace_store
                                    .effective_title(&inbox_key, &sess.title)
                                    .to_string();
                                self.sidebar_state.rename = None;
                                self.inbox_state.rename =
                                    Some(InlineRenameState::new(inbox_key, sess.id, current));
                                if let Some(win) = window {
                                    self.focus_inbox(win, cx);
                                }
                                cx.notify();
                            } else if self.sidebar_state.mode == SidebarMode::Open
                                && let Some(key) = target_key
                                && self.is_session_rendered_in_open_sidebar(&key)
                            {
                                if let Some(win) = window {
                                    self.start_sidebar_rename_with_window(key, win, cx);
                                } else {
                                    self.start_sidebar_rename(key, cx);
                                }
                            }
                            return;
                        }

                        if let Some(key) = target_key {
                            if self.can_mount_sidebar_rename(&key) {
                                if self.main_surface == MainSurface::Chat {
                                    self.dismiss_conflicting_chat_overlays_for_rename();
                                }
                                if let Some(win) = window {
                                    self.start_sidebar_rename_with_window(key, win, cx);
                                } else {
                                    self.start_sidebar_rename(key, cx);
                                }
                            }
                            return;
                        }
                    }
                    "pal-cmd-favorite" => {
                        if let Some(ref key) = target_key {
                            self.sidebar_state.workspace_store.toggle_favorite(key);
                            self.cleanup_unmounted_surface_state();
                            self.persist_workspace_store_if_enabled();
                        } else {
                            self.toggle_active_favorite(cx);
                            return;
                        }
                    }
                    "pal-cmd-pin" => {
                        if let Some(ref key) = target_key {
                            let in_starred =
                                self.sidebar_state.active_workspace_id == STARRED_WORKSPACE_ID;
                            self.sidebar_state
                                .workspace_store
                                .toggle_pin(key, in_starred);
                            self.cleanup_unmounted_surface_state();
                            self.persist_workspace_store_if_enabled();
                        }
                    }
                    "pal-cmd-mark-unread" => {
                        if let Some(ref key) = target_key
                            && let Some(sess) = self.find_session_by_key(key)
                        {
                            self.sidebar_state
                                .workspace_store
                                .toggle_session_unread(key, &sess);
                            self.cleanup_unmounted_surface_state();
                            self.persist_workspace_store_if_enabled();
                        }
                    }
                    "pal-cmd-waiting" => {
                        if let Some(ref key) = target_key {
                            self.mark_done_or_inbox(key, cx);
                        }
                    }
                    "pal-cmd-undo-waiting" => {
                        if let Some(key) = self.sidebar_state.workspace_store.undo_last_waiting() {
                            self.reactivate_session_key(&key, cx);
                            self.persist_workspace_store_if_enabled();
                        }
                    }
                    "pal-cmd-inbox-exclude" => {
                        if let Some(ref key) = target_key {
                            self.sidebar_state
                                .workspace_store
                                .toggle_inbox_excluded(key);
                            self.cleanup_unmounted_surface_state();
                            self.persist_workspace_store_if_enabled();
                        }
                    }
                    "session.delete" => {
                        if let Some(ref key) = target_key {
                            self.dissolve_session_key(key, cx);
                        }
                    }
                    "pal-cmd-archive-session" => {
                        if let Some(ref key) = target_key {
                            self.reactivate_session_key(key, cx);
                        }
                    }
                    "pal-view-done" | "pal-view-hibernated" | "pal-view-dissolved" => {
                        let lifecycle = match id.as_str() {
                            "pal-view-done" => multibot_core::workspaces::SessionLifecycle::Done,
                            "pal-view-hibernated" => {
                                multibot_core::workspaces::SessionLifecycle::Hibernated
                            }
                            _ => multibot_core::workspaces::SessionLifecycle::Dissolved,
                        };
                        if let Some(window) = window {
                            self.open_lifecycle_view(lifecycle, window, cx);
                        }
                    }
                    "pal-cmd-copy-id" => {
                        if let Some(ref key) = target_key {
                            cx.write_to_clipboard(ClipboardItem::new_string(key.clone()));
                            self.notice = Some(format!("Session ID: {key}"));
                        }
                    }
                    "pal-cmd-refresh" => {
                        self.refresh_all_hosts();
                    }
                    "pal-cmd-usage" => {
                        self.notice = Some(format!(
                            "Multibot v{} · {} · {}",
                            multibot_proto::build_version(),
                            self.footprint_label,
                            self.connection_status
                        ));
                    }
                    "pal-cmd-copy-code" => {
                        self.copy_latest_code_block(cx);
                        return;
                    }
                    "pal-cmd-expand-table" => {
                        if self.expand_best_table(cx)
                            && let Some(win) = window
                        {
                            self.focus_table_modal(win, cx);
                        }
                        return;
                    }
                    "pal-cmd-toggle-tools" => {
                        self.toggle_all_tool_steps(cx);
                        return;
                    }
                    "pal-cmd-toggle-todos" => {
                        self.toggle_todo_drawer(cx);
                        return;
                    }
                    "pal-cmd-load-older" => {
                        self.load_older_history(cx);
                        return;
                    }
                    "pal-cmd-interrupt" => {
                        self.interrupt_active_session(cx);
                        return;
                    }
                    "pal-cmd-spawn-claude"
                    | "pal-cmd-spawn-codex"
                    | "pal-cmd-spawn-antigravity"
                    | "pal-cmd-spawn-pi"
                    | "pal-cmd-spawn-shell" => {
                        let agent = match id.as_str() {
                            "pal-cmd-spawn-claude" => AgentKind::Claude,
                            "pal-cmd-spawn-codex" => AgentKind::Codex,
                            "pal-cmd-spawn-antigravity" => AgentKind::Antigravity,
                            "pal-cmd-spawn-shell" => AgentKind::Shell,
                            _ => AgentKind::Pi,
                        };
                        self.start_agent_session(agent, window, cx);
                        return;
                    }
                    "pal-cmd-unarchive-session" => {
                        if let Some(ref key) = target_key
                            && self.find_session_by_key(key).is_some_and(|session| {
                                self.sidebar_state.workspace_store.session_lifecycle(
                                    key,
                                    &session,
                                    now_unix_ms(),
                                ) == multibot_core::workspaces::SessionLifecycle::Dissolved
                            })
                        {
                            self.reactivate_session_key(key, cx);
                        } else {
                            self.reactivate_last_dissolved(cx);
                        }
                        return;
                    }
                    _ => {}
                }
                self.sync_workspace_to_shared(cx);
                cx.notify();
            }
        }
    }

    fn palette_move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let query = self.palette_input.read(cx).value().to_string();
        let now = now_unix_ms();
        let rows = self.build_palette_rows(&query, now);
        let selectable_count = rows
            .iter()
            .filter(|r| !matches!(r, PaletteRow::Header(_)))
            .count();
        if delta < 0 {
            self.palette_state.selected_index = self
                .palette_state
                .selected_index
                .min(selectable_count.saturating_sub(1))
                .saturating_sub((-delta) as usize);
            self.scroll_palette_to_selected(&rows);
        } else if delta > 0 && selectable_count > 0 {
            let max_idx = selectable_count - 1;
            self.palette_state.selected_index = self
                .palette_state
                .selected_index
                .min(max_idx)
                .saturating_add(delta as usize)
                .min(max_idx);
            self.scroll_palette_to_selected(&rows);
        }
        cx.notify();
    }

    fn palette_try_exit_submenu(&mut self, cx: &mut Context<Self>) -> bool {
        let query = self.palette_input.read(cx).value().to_string();
        if query.is_empty()
            && (self.palette_state.provider_picker
                || self.palette_state.mode != PaletteMode::Global)
        {
            self.palette_state.provider_picker = false;
            self.palette_state.mode = PaletteMode::Global;
            self.palette_state.reminder_target = None;
            self.palette_state.directory_picker = None;
            self.palette_state.dissolve_workspace_id = None;
            self.palette_state.selected_index = 0;
            self.palette_state.scroll_handle.scroll_to_item(0);
            cx.notify();
            return true;
        }
        false
    }

    fn handle_palette_capture_keydown(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = ev.keystroke.key.to_ascii_lowercase();
        if self.table_modal_state.is_open() {
            if key == "escape" {
                self.table_modal_state.close();
                self.focus_active_surface_default(window, cx);
                cx.stop_propagation();
                cx.notify();
            } else {
                cx.stop_propagation();
                window.prevent_default();
            }
            return;
        }

        if key == "tab"
            && matches!(
                self.palette_state.reminder_target,
                Some(ReminderTarget::Session(_))
            )
        {
            self.palette_state.hibernate_wake_on_reply =
                !self.palette_state.hibernate_wake_on_reply;
            cx.stop_propagation();
            window.prevent_default();
            cx.notify();
            return;
        }
        match key.as_str() {
            "up" => {
                self.palette_move_selection(-1, cx);
                cx.stop_propagation();
            }
            "down" => {
                self.palette_move_selection(1, cx);
                cx.stop_propagation();
            }
            "enter" => {
                self.execute_selected_palette_row(Some(window), cx);
                cx.stop_propagation();
            }
            "escape" => {
                if self.table_modal_state.is_open() {
                    self.table_modal_state.close();
                    self.focus_active_surface_default(window, cx);
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                self.active_modal = ActiveModal::None;
                self.palette_state.mode = PaletteMode::Global;
                self.palette_state.reminder_target = None;
                self.palette_state.directory_picker = None;
                self.palette_state.dissolve_workspace_id = None;
                self.focus_active_surface_default(window, cx);
                cx.stop_propagation();
                cx.notify();
            }
            "backspace" if self.palette_try_exit_submenu(cx) => {
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    pub(crate) fn render_palette_modal(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.palette_input.read(cx).value().to_string();
        let now = now_unix_ms();
        let rows = self.build_palette_rows(&query, now);
        let selectable_total = rows
            .iter()
            .filter(|r| !matches!(r, PaletteRow::Header(_)))
            .count();
        let clamped_selected = if selectable_total == 0 {
            0
        } else {
            self.palette_state.selected_index.min(selectable_total - 1)
        };

        // Direct scrollable container so `scroll_handle.scroll_to_item(raw_idx)` aligns 1:1 with `rows`
        let mut list = v_flex()
            .id("palette-list-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.palette_state.scroll_handle)
            .gap_1()
            .p_2();
        let mut selectable_idx = 0usize;
        let mut session_el_counter = 0usize;
        let mut ws_el_counter = 0usize;

        if rows.is_empty() {
            list = list.child(
                div()
                    .px_3()
                    .py_4()
                    .text_xs()
                    .text_color(theme::text_muted())
                    .child("No matching sessions, commands, or workspaces."),
            );
        } else {
            for row in rows {
                match row {
                    PaletteRow::Header(title) => {
                        list = list.child(
                            div()
                                .px_3()
                                .pt_1p5()
                                .pb_0p5()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme::text_muted())
                                .child(title),
                        );
                    }
                    PaletteRow::Session {
                        ref session_key,
                        ref title,
                        agent,
                        ref workspace_name,
                        ..
                    } => {
                        let is_selected = selectable_idx == clamped_selected;
                        selectable_idx += 1;
                        let s_idx = session_el_counter;
                        session_el_counter += 1;
                        let row_clone = row.clone();
                        let title_owned = title.clone();
                        let ws_owned = workspace_name.clone();
                        let shell_key = session_key.clone();
                        let shell_preview = match &row {
                            PaletteRow::Session { cwd, .. } => cwd.clone().unwrap_or_default(),
                            _ => String::new(),
                        };

                        list = list.child(
                            div()
                                .id(SharedString::from(format!("pal-sess-{s_idx}")))
                                .role(Role::Button)
                                .test_support()
                                .px_3()
                                .py_2()
                                .rounded(RADIUS_ROW)
                                .bg(if is_selected {
                                    theme::bg_active_row()
                                } else {
                                    theme::bg_surface()
                                })
                                .hover(|s| s.bg(theme::bg_hover()))
                                .cursor_pointer()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, window, cx| {
                                        this.execute_palette_row(
                                            row_clone.clone(),
                                            now_unix_ms(),
                                            Some(window),
                                            cx,
                                        );
                                    }),
                                )
                                .child(
                                    h_flex()
                                        .justify_between()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            h_flex()
                                                .flex_1()
                                                .min_w_0()
                                                .gap_2()
                                                .items_center()
                                                .child(
                                                    div()
                                                        .w(px(22.0))
                                                        .h(px(22.0))
                                                        .rounded(RADIUS_CHIP)
                                                        .bg(Self::agent_color(agent))
                                                        .flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .text_xs()
                                                        .text_color(theme::bg_canvas())
                                                        .child(agent.glyph()),
                                                )
                                                .child(
                                                    div()
                                                        .text_sm()
                                                        .text_color(theme::text_primary())
                                                        .truncate()
                                                        .child(title_owned),
                                                ),
                                        )
                                        .child(
                                            div()
                                                .px_2()
                                                .py_0p5()
                                                .rounded(RADIUS_CHIP)
                                                .bg(theme::bg_hover())
                                                .text_xs()
                                                .text_color(theme::text_muted())
                                                .child(ws_owned),
                                        )
                                        .when(self.palette_state.server_shells, |el| {
                                            el.child(
                                                Button::new(format!("pal-shell-end-{s_idx}"))
                                                    .small()
                                                    .label("End")
                                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                                        cx.stop_propagation()
                                                    })
                                                    .on_click(cx.listener(
                                                        move |this, _, window, cx| {
                                                            cx.stop_propagation();
                                                            this.palette_state.shell_kill_target =
                                                                Some(shell_key.clone());
                                                            this.palette_state.selected_index = 0;
                                                            this.palette_input.update(
                                                                cx,
                                                                |input, cx| {
                                                                    input.set_value("", window, cx);
                                                                    input.focus(window, cx);
                                                                },
                                                            );
                                                            cx.notify();
                                                        },
                                                    )),
                                            )
                                        }),
                                )
                                .when(self.palette_state.server_shells, |el| {
                                    el.child(
                                        div()
                                            .text_xs()
                                            .text_color(theme::text_muted())
                                            .child(shell_preview),
                                    )
                                }),
                        );
                    }
                    PaletteRow::Command {
                        ref id,
                        ref label,
                        ref chord,
                        destructive,
                        ..
                    } => {
                        let is_selected = selectable_idx == clamped_selected;
                        selectable_idx += 1;
                        let row_clone = row.clone();
                        let el_id = SharedString::from(id.clone());
                        let label_owned = label.clone();
                        let chord_owned = chord.clone();

                        list = list.child(
                            div()
                                .id(el_id)
                                .role(Role::Button)
                                .test_support()
                                .px_3()
                                .py_2()
                                .rounded(RADIUS_ROW)
                                .bg(if is_selected {
                                    theme::bg_active_row()
                                } else {
                                    theme::bg_surface()
                                })
                                .hover(|s| s.bg(theme::bg_hover()))
                                .cursor_pointer()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, window, cx| {
                                        this.execute_palette_row(
                                            row_clone.clone(),
                                            now_unix_ms(),
                                            Some(window),
                                            cx,
                                        );
                                    }),
                                )
                                .child(
                                    h_flex()
                                        .justify_between()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .text_sm()
                                                .text_color(if destructive {
                                                    theme::accent_amber()
                                                } else {
                                                    theme::text_primary()
                                                })
                                                .truncate()
                                                .child(label_owned),
                                        )
                                        .when(!chord_owned.is_empty(), |el| {
                                            el.child(
                                                div()
                                                    .px_2()
                                                    .py_0p5()
                                                    .rounded(RADIUS_CHIP)
                                                    .bg(theme::bg_hover())
                                                    .text_xs()
                                                    .text_color(theme::text_muted())
                                                    .child(chord_owned),
                                            )
                                        }),
                                ),
                        );
                    }
                    PaletteRow::Workspace {
                        ref name, archived, ..
                    } => {
                        let is_selected = selectable_idx == clamped_selected;
                        selectable_idx += 1;
                        let w_idx = ws_el_counter;
                        ws_el_counter += 1;
                        let row_clone = row.clone();
                        let name_owned = name.clone();

                        list = list.child(
                            div()
                                .id(SharedString::from(format!("pal-ws-{w_idx}")))
                                .role(Role::Button)
                                .test_support()
                                .px_3()
                                .py_2()
                                .rounded(RADIUS_ROW)
                                .bg(if is_selected {
                                    theme::bg_active_row()
                                } else {
                                    theme::bg_surface()
                                })
                                .hover(|s| s.bg(theme::bg_hover()))
                                .cursor_pointer()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, window, cx| {
                                        this.execute_palette_row(
                                            row_clone.clone(),
                                            now_unix_ms(),
                                            Some(window),
                                            cx,
                                        );
                                    }),
                                )
                                .child(
                                    h_flex()
                                        .justify_between()
                                        .items_center()
                                        .child(
                                            div()
                                                .text_sm()
                                                .text_color(theme::text_primary())
                                                .child(name_owned),
                                        )
                                        .child(
                                            div()
                                                .px_2()
                                                .py_0p5()
                                                .rounded(RADIUS_CHIP)
                                                .bg(theme::bg_hover())
                                                .text_xs()
                                                .text_color(theme::text_muted())
                                                .child(if archived {
                                                    "Archived Workspace"
                                                } else {
                                                    "Workspace"
                                                }),
                                        ),
                                ),
                        );
                    }
                }
            }
        }

        let context_banner = self.palette_state.mode.target_session_key().map(|key| {
            let title = self
                .find_session_by_key(key)
                .map(|s| {
                    self.sidebar_state
                        .workspace_store
                        .effective_title(key, &s.title)
                        .to_string()
                })
                .unwrap_or_else(|| key.to_string());
            format!("Session actions: {title}  (Backspace to exit)")
        });
        let context_banner = if self.palette_state.mode == PaletteMode::Workspaces {
            Some("Workspaces · Up/Down then Enter · Backspace to return".into())
        } else {
            context_banner
        };
        let context_banner = match &self.palette_state.reminder_target {
            Some(ReminderTarget::Session(_)) => Some(format!(
                "Hibernate · {} · Tab toggles · optional note · Up/Down Enter",
                if self.palette_state.hibernate_wake_on_reply {
                    "Wake on reply"
                } else {
                    "Stay hidden"
                }
            )),
            Some(ReminderTarget::Workspace(_)) => {
                Some("Snooze workspace · Up/Down then Enter".into())
            }
            None => context_banner,
        };

        let context_banner = if self.palette_state.provider_picker {
            Some("Switch provider / harness · Up/Down then Enter".into())
        } else {
            context_banner
        };
        let context_banner = if let Some(picker) = &self.palette_state.directory_picker {
            Some(format!(
                "New {} session · Choose a directory · Up/Down then Enter",
                picker.agent.label()
            ))
        } else {
            context_banner
        };

        let context_banner = if let Some(id) = &self.palette_state.dissolve_workspace_id {
            let name = self
                .sidebar_state
                .workspace_store
                .workspace_by_id(id)
                .map(|w| w.name.as_str())
                .unwrap_or("workspace");
            Some(format!(
                "Dissolve {name} and stop {} live agents? Chat logs stay in Dissolved.",
                self.workspace_live_session_keys(id).len()
            ))
        } else {
            context_banner
        };

        let context_banner = if self.palette_state.shell_kill_target.is_some() {
            Some("End this server shell permanently? Running work will stop.".into())
        } else if self.palette_state.server_shells {
            Some("Server shells · Enter to attach · End to kill".into())
        } else {
            context_banner
        };

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
                cx.listener(|this, _, window, cx| {
                    this.active_modal = ActiveModal::None;
                    this.palette_state.mode = PaletteMode::Global;
                    this.palette_state.reminder_target = None;
                    this.palette_state.directory_picker = None;
                    this.palette_state.dissolve_workspace_id = None;
                    this.focus_active_surface_default(window, cx);
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
                    .on_action(cx.listener(|this, _: &CloseOverlay, window, cx| {
                        if this.table_modal_state.is_open() {
                            this.table_modal_state.close();
                            this.focus_active_surface_default(window, cx);
                            cx.notify();
                            return;
                        }
                        this.palette_state.mode = PaletteMode::Global;
                        this.palette_state.reminder_target = None;
                        this.palette_state.directory_picker = None;
                        this.palette_state.dissolve_workspace_id = None;
                        this.active_modal = ActiveModal::None;
                        this.focus_active_surface_default(window, cx);
                        cx.notify();
                    }))
                    .capture_action(cx.listener(|this, _: &input::MoveUp, _, cx| {
                        if this.table_modal_state.is_open() {
                            cx.stop_propagation();
                            return;
                        }
                        this.palette_move_selection(-1, cx);
                        cx.stop_propagation();
                    }))
                    .capture_action(cx.listener(|this, _: &input::MoveDown, _, cx| {
                        if this.table_modal_state.is_open() {
                            cx.stop_propagation();
                            return;
                        }
                        this.palette_move_selection(1, cx);
                        cx.stop_propagation();
                    }))
                    .when(
                        matches!(
                            self.palette_state.reminder_target,
                            Some(ReminderTarget::Session(_))
                        ),
                        |el| el.key_context("HibernatePicker"),
                    )
                    .capture_action(cx.listener(
                        |this, _: &crate::app::ToggleHibernateMode, _, cx| {
                            if matches!(
                                this.palette_state.reminder_target,
                                Some(ReminderTarget::Session(_))
                            ) {
                                this.palette_state.hibernate_wake_on_reply =
                                    !this.palette_state.hibernate_wake_on_reply;
                                cx.stop_propagation();
                                cx.notify();
                            }
                        },
                    ))
                    .capture_action(cx.listener(|this, _: &input::Enter, window, cx| {
                        if this.table_modal_state.is_open() {
                            cx.stop_propagation();
                            return;
                        }
                        this.execute_selected_palette_row(Some(window), cx);
                        cx.stop_propagation();
                    }))
                    .capture_action(cx.listener(|this, _: &input::Backspace, _, cx| {
                        if this.table_modal_state.is_open() {
                            cx.stop_propagation();
                            return;
                        }
                        if this.palette_try_exit_submenu(cx) {
                            cx.stop_propagation();
                        }
                    }))
                    .capture_action(cx.listener(|this, _: &input::Escape, window, cx| {
                        if this.table_modal_state.is_open() {
                            this.table_modal_state.close();
                            this.focus_active_surface_default(window, cx);
                            cx.stop_propagation();
                            cx.notify();
                            return;
                        }
                        this.palette_state.mode = PaletteMode::Global;
                        this.palette_state.reminder_target = None;
                        this.palette_state.directory_picker = None;
                        this.palette_state.dissolve_workspace_id = None;
                        this.active_modal = ActiveModal::None;
                        this.focus_active_surface_default(window, cx);
                        cx.stop_propagation();
                        cx.notify();
                    }))
                    .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                        this.handle_palette_capture_keydown(ev, window, cx);
                    }))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .when_some(context_banner, |el, banner| {
                        el.child(
                            div()
                                .id("palette-context-banner")
                                .test_support()
                                .px_3()
                                .py_1p5()
                                .border_b_1()
                                .border_color(theme::border_subtle())
                                .text_xs()
                                .text_color(theme::accent_blue())
                                .child(banner),
                        )
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
                    .child(list),
            )
    }

    pub(crate) fn render_changelog_modal(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let releases = multibot_core::changelog::releases();
        let sections = releases.iter().enumerate().map(|(index, release)| {
            v_flex()
                .id(("changelog-release", index))
                .test_support()
                .flex_shrink_0()
                .gap_3()
                .child(
                    div()
                        .text_lg()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::text_primary())
                        .child(release.title),
                )
                .children(release.bullets.iter().map(|bullet| {
                    h_flex()
                        .items_start()
                        .gap_3()
                        .text_sm()
                        .text_color(theme::text_secondary())
                        .child(div().text_color(theme::text_muted()).child("•"))
                        .child(div().flex_1().min_w_0().child(*bullet))
                }))
        });

        div()
            .id("changelog-backdrop")
            .absolute()
            .inset_0()
            .bg(hsla(0.0, 0.0, 0.0, 0.58))
            .flex()
            .items_center()
            .justify_center()
            .p_6()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.active_modal = ActiveModal::None;
                    this.focus_active_surface_default(window, cx);
                    cx.notify();
                }),
            )
            .child(
                v_flex()
                    .id("changelog-modal")
                    .test_support()
                    .w(px(680.0))
                    .max_w(gpui_kit::relative(1.0))
                    .h(gpui_kit::relative(0.8))
                    .max_h(px(640.0))
                    .rounded(RADIUS_MODAL)
                    .overflow_hidden()
                    .bg(theme::bg_surface())
                    .border_1()
                    .border_color(theme::border_subtle())
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        h_flex()
                            .flex_shrink_0()
                            .px_6()
                            .py_4()
                            .border_b_1()
                            .border_color(theme::border_subtle())
                            .justify_between()
                            .items_center()
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme::text_primary())
                                    .child("Changelog"),
                            )
                            .child(
                                Button::new("close-changelog")
                                    .xsmall()
                                    .label("Esc")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.active_modal = ActiveModal::None;
                                        this.focus_active_surface_default(window, cx);
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(
                        v_flex()
                            .id("changelog-scroll")
                            .test_support()
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .p_6()
                            .gap_6()
                            .children(sections),
                    ),
            )
    }

    pub(crate) fn render_shortcuts_modal(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.shortcuts_search_input.read(cx).value().to_string();
        let tokens: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        let mut rows = v_flex().gap_1().p_3();
        let mut match_count = 0;
        for cat in ALL_CATEGORIES {
            let matches: Vec<_> = by_category(cat)
                .filter(|spec| {
                    let searchable = format!(
                        "{} {} {} {}",
                        spec.description,
                        cat.label(),
                        spec.display,
                        spec.gpui_key
                    )
                    .to_lowercase();
                    tokens.iter().all(|token| searchable.contains(token))
                })
                .collect();
            if matches.is_empty() {
                continue;
            }
            match_count += matches.len();
            rows = rows.child(
                div()
                    .px_3()
                    .pt_2()
                    .pb_0p5()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text_muted())
                    .child(cat.label()),
            );
            for spec in matches {
                rows = rows.child(
                    h_flex()
                        .id(format!("shortcut-{}", spec.id))
                        .test_support()
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
        }

        if match_count == 0 {
            rows = rows.child(
                div()
                    .id("shortcuts-empty")
                    .test_support()
                    .p_3()
                    .text_sm()
                    .text_color(theme::text_muted())
                    .child(format!("No shortcuts match \"{query}\"")),
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
                cx.listener(|this, _, window, cx| {
                    this.active_modal = ActiveModal::None;
                    this.focus_active_surface_default(window, cx);
                    cx.notify();
                }),
            )
            .child(
                v_flex()
                    .id("shortcuts-modal")
                    .test_support()
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
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.active_modal = ActiveModal::None;
                                        this.focus_active_surface_default(window, cx);
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .px_4()
                            .py_2()
                            .flex_shrink_0()
                            .child(Input::new(&self.shortcuts_search_input).id("shortcuts-search")),
                    )
                    .child(
                        div()
                            .id("shortcuts-scroll")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .child(rows),
                    ),
            )
    }
}

#[cfg(test)]
#[path = "palette_tests.rs"]
mod tests;
