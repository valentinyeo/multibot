#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutCategory {
    Navigation,
    Inbox,
    Workspaces,
    Chat,
    View,
    Browser,
    System,
}

impl ShortcutCategory {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Navigation => "Navigation & Palette",
            Self::Inbox => "Inbox",
            Self::Workspaces => "Workspaces",
            Self::Chat => "Chat & Voice",
            Self::View => "View & Terminal",
            Self::Browser => "Browser Pane",
            Self::System => "System & Settings",
        }
    }
}

pub const ALL_CATEGORIES: [ShortcutCategory; 7] = [
    ShortcutCategory::Navigation,
    ShortcutCategory::Workspaces,
    ShortcutCategory::Inbox,
    ShortcutCategory::Chat,
    ShortcutCategory::View,
    ShortcutCategory::Browser,
    ShortcutCategory::System,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShortcutSpec {
    pub id: &'static str,
    /// Human-readable display chord (shown in Ctrl+K and F1).
    pub display: &'static str,
    /// GPUI keystroke syntax (used to bind KeyBinding).
    pub gpui_key: &'static str,
    pub category: ShortcutCategory,
    pub description: &'static str,
    pub wired: bool,
}

/// Single source of truth for preserved Multibot keyboard shortcuts.
/// Canonical contract: `docs/keyboard-shortcuts.md`.
pub const SHORTCUTS: &[ShortcutSpec] = &[
    // =========================================================================
    // Navigation & Palette (25 entries)
    // =========================================================================
    ShortcutSpec {
        id: "palette.open",
        display: "Ctrl+K",
        gpui_key: "ctrl-k",
        category: ShortcutCategory::Navigation,
        description: "Open command palette & quick session switcher",
        wired: true,
    },
    ShortcutSpec {
        id: "palette.open_global",
        display: "Ctrl+Shift+K",
        gpui_key: "ctrl-shift-k",
        category: ShortcutCategory::Navigation,
        description: "Open command palette from any surface (including browser)",
        wired: true,
    },
    ShortcutSpec {
        id: "session.next",
        display: "Ctrl+Tab",
        gpui_key: "ctrl-tab",
        category: ShortcutCategory::Navigation,
        description: "Switch to next session",
        wired: true,
    },
    ShortcutSpec {
        id: "session.prev",
        display: "Ctrl+Shift+Tab",
        gpui_key: "ctrl-shift-tab",
        category: ShortcutCategory::Navigation,
        description: "Switch to previous session",
        wired: true,
    },
    ShortcutSpec {
        id: "session.jump_1",
        display: "Ctrl+1",
        gpui_key: "ctrl-1",
        category: ShortcutCategory::Navigation,
        description: "Jump to session 1 (pinned first)",
        wired: true,
    },
    ShortcutSpec {
        id: "session.jump_2",
        display: "Ctrl+2",
        gpui_key: "ctrl-2",
        category: ShortcutCategory::Navigation,
        description: "Jump to session 2 (pinned first)",
        wired: true,
    },
    ShortcutSpec {
        id: "session.jump_3",
        display: "Ctrl+3",
        gpui_key: "ctrl-3",
        category: ShortcutCategory::Navigation,
        description: "Jump to session 3 (pinned first)",
        wired: true,
    },
    ShortcutSpec {
        id: "session.jump_4",
        display: "Ctrl+4",
        gpui_key: "ctrl-4",
        category: ShortcutCategory::Navigation,
        description: "Jump to session 4 (pinned first)",
        wired: true,
    },
    ShortcutSpec {
        id: "session.jump_5",
        display: "Ctrl+5",
        gpui_key: "ctrl-5",
        category: ShortcutCategory::Navigation,
        description: "Jump to session 5 (pinned first)",
        wired: true,
    },
    ShortcutSpec {
        id: "session.jump_6",
        display: "Ctrl+6",
        gpui_key: "ctrl-6",
        category: ShortcutCategory::Navigation,
        description: "Jump to session 6 (pinned first)",
        wired: true,
    },
    ShortcutSpec {
        id: "session.jump_7",
        display: "Ctrl+7",
        gpui_key: "ctrl-7",
        category: ShortcutCategory::Navigation,
        description: "Jump to session 7 (pinned first)",
        wired: true,
    },
    ShortcutSpec {
        id: "session.jump_8",
        display: "Ctrl+8",
        gpui_key: "ctrl-8",
        category: ShortcutCategory::Navigation,
        description: "Jump to session 8 (pinned first)",
        wired: true,
    },
    ShortcutSpec {
        id: "session.jump_9",
        display: "Ctrl+9",
        gpui_key: "ctrl-9",
        category: ShortcutCategory::Navigation,
        description: "Jump to session 9 (pinned first)",
        wired: true,
    },
    ShortcutSpec {
        id: "session.new",
        display: "Ctrl+T",
        gpui_key: "ctrl-t",
        category: ShortcutCategory::Navigation,
        description: "New Claude session in active workspace",
        wired: true,
    },
    ShortcutSpec {
        id: "session.new_alt",
        display: "Alt+T",
        gpui_key: "alt-t",
        category: ShortcutCategory::Navigation,
        description: "New workspace session (Claude by default)",
        wired: true,
    },
    ShortcutSpec {
        id: "session.delete",
        display: "Ctrl+Shift+W",
        gpui_key: "ctrl-shift-w",
        category: ShortcutCategory::Navigation,
        description: "Delete current session (stop agent, keep history)",
        wired: true,
    },
    ShortcutSpec {
        id: "session.archive",
        display: "Ctrl+Shift+E",
        gpui_key: "ctrl-shift-e",
        category: ShortcutCategory::Navigation,
        description: "Archive active session (press twice to confirm)",
        wired: true,
    },
    ShortcutSpec {
        id: "session.reopen_closed",
        display: "Ctrl+Shift+T",
        gpui_key: "ctrl-shift-t",
        category: ShortcutCategory::Navigation,
        description: "Reopen last closed session",
        wired: true,
    },
    ShortcutSpec {
        id: "session.reconnect",
        display: "Ctrl+Shift+R",
        gpui_key: "ctrl-shift-r",
        category: ShortcutCategory::Navigation,
        description: "Reconnect / reattach active SSH daemon or session",
        wired: true,
    },
    ShortcutSpec {
        id: "session.move_up",
        display: "Ctrl+Shift+Up",
        gpui_key: "ctrl-shift-up",
        category: ShortcutCategory::Navigation,
        description: "Move active session up within group or section",
        wired: true,
    },
    ShortcutSpec {
        id: "session.move_down",
        display: "Ctrl+Shift+Down",
        gpui_key: "ctrl-shift-down",
        category: ShortcutCategory::Navigation,
        description: "Move active session down within group or section",
        wired: true,
    },
    ShortcutSpec {
        id: "group.toggle",
        display: "Ctrl+G",
        gpui_key: "ctrl-g",
        category: ShortcutCategory::Navigation,
        description: "Collapse or expand active session's group",
        wired: true,
    },
    ShortcutSpec {
        id: "session.remind_me",
        display: "Ctrl+H",
        gpui_key: "ctrl-h",
        category: ShortcutCategory::Navigation,
        description: "Remind Me: schedule or clear a chat reminder",
        wired: true,
    },
    ShortcutSpec {
        id: "session.remind_me_alt",
        display: "Alt+H",
        gpui_key: "alt-h",
        category: ShortcutCategory::Navigation,
        description: "Remind Me: schedule or clear a chat reminder",
        wired: true,
    },
    ShortcutSpec {
        id: "room.new",
        display: "Ctrl+Shift+N",
        gpui_key: "ctrl-shift-n",
        category: ShortcutCategory::Navigation,
        description: "Create a Room",
        wired: false,
    },
    // =========================================================================
    // Inbox (8 entries)
    // =========================================================================
    ShortcutSpec {
        id: "inbox.open",
        display: "Alt+I",
        gpui_key: "alt-i",
        category: ShortcutCategory::Inbox,
        description: "Open Cross-workspace Inbox / jump to All split",
        wired: true,
    },
    ShortcutSpec {
        id: "inbox.toggle",
        display: "Ctrl+Shift+I",
        gpui_key: "ctrl-shift-i",
        category: ShortcutCategory::Inbox,
        description: "Toggle Cross-workspace Inbox",
        wired: true,
    },
    ShortcutSpec {
        id: "inbox.next",
        display: "Alt+J",
        gpui_key: "alt-j",
        category: ShortcutCategory::Inbox,
        description: "Next Inbox session / cycle workspace",
        wired: true,
    },
    ShortcutSpec {
        id: "inbox.prev",
        display: "Alt+K",
        gpui_key: "alt-k",
        category: ShortcutCategory::Inbox,
        description: "Previous Inbox session / cycle workspace",
        wired: true,
    },
    ShortcutSpec {
        id: "inbox.unread_only",
        display: "Ctrl+Shift+U",
        gpui_key: "ctrl-shift-u",
        category: ShortcutCategory::Inbox,
        description: "Toggle global unread-only filter (sidebar + inbox)",
        wired: true,
    },
    ShortcutSpec {
        id: "session.mark_unread",
        display: "Alt+U",
        gpui_key: "alt-u",
        category: ShortcutCategory::Inbox,
        description: "Mark active session unread / restore blue dot",
        wired: true,
    },
    ShortcutSpec {
        id: "session.waiting",
        display: "Alt+E",
        gpui_key: "alt-e",
        category: ShortcutCategory::Inbox,
        description: "Move active session to Waiting until unread",
        wired: true,
    },
    ShortcutSpec {
        id: "session.undo_waiting",
        display: "Alt+Z",
        gpui_key: "alt-z",
        category: ShortcutCategory::Inbox,
        description: "Undo most recent move to Waiting",
        wired: true,
    },
    // =========================================================================
    // Workspaces (16 entries)
    // =========================================================================
    ShortcutSpec {
        id: "workspace.favorite",
        display: "Alt+S",
        gpui_key: "alt-s",
        category: ShortcutCategory::Workspaces,
        description: "Favorite or unfavorite active session",
        wired: true,
    },
    ShortcutSpec {
        id: "workspace.snooze",
        display: "Ctrl+Shift+Y",
        gpui_key: "ctrl-shift-y",
        category: ShortcutCategory::Workspaces,
        description: "Snooze active workspace",
        wired: true,
    },
    ShortcutSpec {
        id: "workspace.jump_1",
        display: "Alt+1",
        gpui_key: "alt-1",
        category: ShortcutCategory::Workspaces,
        description: "Switch to Starred workspace (★)",
        wired: true,
    },
    ShortcutSpec {
        id: "workspace.jump_2",
        display: "Alt+2",
        gpui_key: "alt-2",
        category: ShortcutCategory::Workspaces,
        description: "Switch to workspace 2",
        wired: true,
    },
    ShortcutSpec {
        id: "workspace.jump_3",
        display: "Alt+3",
        gpui_key: "alt-3",
        category: ShortcutCategory::Workspaces,
        description: "Switch to workspace 3",
        wired: true,
    },
    ShortcutSpec {
        id: "workspace.jump_4",
        display: "Alt+4",
        gpui_key: "alt-4",
        category: ShortcutCategory::Workspaces,
        description: "Switch to workspace 4",
        wired: true,
    },
    ShortcutSpec {
        id: "workspace.jump_5",
        display: "Alt+5",
        gpui_key: "alt-5",
        category: ShortcutCategory::Workspaces,
        description: "Switch to workspace 5",
        wired: true,
    },
    ShortcutSpec {
        id: "workspace.jump_6",
        display: "Alt+6",
        gpui_key: "alt-6",
        category: ShortcutCategory::Workspaces,
        description: "Switch to workspace 6",
        wired: true,
    },
    ShortcutSpec {
        id: "workspace.jump_7",
        display: "Alt+7",
        gpui_key: "alt-7",
        category: ShortcutCategory::Workspaces,
        description: "Switch to workspace 7",
        wired: true,
    },
    ShortcutSpec {
        id: "workspace.jump_8",
        display: "Alt+8",
        gpui_key: "alt-8",
        category: ShortcutCategory::Workspaces,
        description: "Switch to workspace 8",
        wired: true,
    },
    ShortcutSpec {
        id: "workspace.jump_9",
        display: "Alt+9",
        gpui_key: "alt-9",
        category: ShortcutCategory::Workspaces,
        description: "Switch to workspace 9",
        wired: true,
    },
    ShortcutSpec {
        id: "workspace.prev",
        display: "Alt+Left",
        gpui_key: "alt-left",
        category: ShortcutCategory::Workspaces,
        description: "Switch to previous workspace",
        wired: true,
    },
    ShortcutSpec {
        id: "workspace.next",
        display: "Alt+Right",
        gpui_key: "alt-right",
        category: ShortcutCategory::Workspaces,
        description: "Switch to next workspace",
        wired: true,
    },
    ShortcutSpec {
        id: "workspace.view_all",
        display: "Alt+Shift+1",
        gpui_key: "alt-shift-1",
        category: ShortcutCategory::Workspaces,
        description: "Workspace view: All sessions by recency",
        wired: true,
    },
    ShortcutSpec {
        id: "workspace.view_frequent",
        display: "Alt+Shift+2",
        gpui_key: "alt-shift-2",
        category: ShortcutCategory::Workspaces,
        description: "Workspace view: Frequent sessions",
        wired: true,
    },
    ShortcutSpec {
        id: "workspace.view_favorites",
        display: "Alt+Shift+3",
        gpui_key: "alt-shift-3",
        category: ShortcutCategory::Workspaces,
        description: "Workspace view: Favorites",
        wired: true,
    },
    // =========================================================================
    // View & Terminal (10 entries)
    // =========================================================================
    ShortcutSpec {
        id: "view.sidebar_cycle",
        display: "Ctrl+Shift+B",
        gpui_key: "ctrl-shift-b",
        category: ShortcutCategory::View,
        description: "Cycle sidebar (open / collapsed / hidden)",
        wired: true,
    },
    ShortcutSpec {
        id: "view.chat_terminal_toggle",
        display: "Alt+C",
        gpui_key: "alt-c",
        category: ShortcutCategory::View,
        description: "Toggle the built-in Terminal panel",
        wired: true,
    },
    ShortcutSpec {
        id: "view.fullscreen",
        display: "Ctrl+Shift+F",
        gpui_key: "ctrl-shift-f",
        category: ShortcutCategory::View,
        description: "Toggle full-width content mode",
        wired: false,
    },
    ShortcutSpec {
        id: "view.font_inc",
        display: "Ctrl+=",
        gpui_key: "ctrl-=",
        category: ShortcutCategory::View,
        description: "Increase chat / terminal font size",
        wired: true,
    },
    ShortcutSpec {
        id: "view.font_dec",
        display: "Ctrl+-",
        gpui_key: "ctrl--",
        category: ShortcutCategory::View,
        description: "Decrease chat / terminal font size",
        wired: true,
    },
    ShortcutSpec {
        id: "view.font_reset",
        display: "Ctrl+0",
        gpui_key: "ctrl-0",
        category: ShortcutCategory::View,
        description: "Reset chat / terminal font size",
        wired: true,
    },
    ShortcutSpec {
        id: "view.status_cards",
        display: "Ctrl+Shift+G",
        gpui_key: "ctrl-shift-g",
        category: ShortcutCategory::View,
        description: "Show usage, servers, and app version / cycle status cards",
        wired: false,
    },
    ShortcutSpec {
        id: "view.scan_urls",
        display: "Ctrl+O",
        gpui_key: "ctrl-o",
        category: ShortcutCategory::View,
        description: "Scan visible shell buffer for URLs",
        wired: false,
    },
    ShortcutSpec {
        id: "view.copy_console",
        display: "Ctrl+Shift+C",
        gpui_key: "ctrl-shift-c",
        category: ShortcutCategory::View,
        description: "Copy current console selection / send interrupt",
        wired: false,
    },
    ShortcutSpec {
        id: "view.paste_shell",
        display: "Ctrl+Shift+V",
        gpui_key: "ctrl-shift-v",
        category: ShortcutCategory::View,
        description: "Paste clipboard into focused shell",
        wired: false,
    },
    // =========================================================================
    // Chat & Voice (14 entries)
    // =========================================================================
    ShortcutSpec {
        id: "chat.copy_code",
        display: "Ctrl+Shift+D",
        gpui_key: "ctrl-shift-d",
        category: ShortcutCategory::Chat,
        description: "Copy latest nonempty code block",
        wired: true,
    },
    ShortcutSpec {
        id: "chat.jump_to_newest",
        display: "Shift+G twice",
        gpui_key: "shift-g shift-g",
        category: ShortcutCategory::Chat,
        description: "Jump to newest from the transcript or an empty composer (within 500 ms)",
        wired: true,
    },
    ShortcutSpec {
        id: "chat.find",
        display: "Ctrl+F",
        gpui_key: "ctrl-f",
        category: ShortcutCategory::Chat,
        description: "Find in conversation (or filter active Inbox list)",
        wired: true,
    },
    ShortcutSpec {
        id: "chat.commands",
        display: "Ctrl+/",
        gpui_key: "ctrl-/",
        category: ShortcutCategory::Chat,
        description: "Browse slash commands and skills",
        wired: true,
    },
    ShortcutSpec {
        id: "chat.steer",
        display: "Ctrl+X Ctrl+S",
        gpui_key: "ctrl-x ctrl-s",
        category: ShortcutCategory::Chat,
        description: "Steer: send queued message into agent immediately",
        wired: false,
    },
    ShortcutSpec {
        id: "chat.perm_allow",
        display: "Alt+Y",
        gpui_key: "alt-y",
        category: ShortcutCategory::Chat,
        description: "Approve pending tool permission once",
        wired: false,
    },
    ShortcutSpec {
        id: "chat.perm_always",
        display: "Alt+O",
        gpui_key: "alt-o",
        category: ShortcutCategory::Chat,
        description: "Always allow pending tool permission this session",
        wired: false,
    },
    ShortcutSpec {
        id: "chat.perm_deny",
        display: "Alt+D",
        gpui_key: "alt-d",
        category: ShortcutCategory::Chat,
        description: "Deny pending tool permission",
        wired: false,
    },
    ShortcutSpec {
        id: "chat.dictation",
        display: "Ctrl+Shift+Space",
        gpui_key: "ctrl-shift-space",
        category: ShortcutCategory::Chat,
        description: "Start / stop voice dictation",
        wired: true,
    },
    ShortcutSpec {
        id: "chat.dictation_inline",
        display: "Ctrl+D",
        gpui_key: "ctrl-d",
        category: ShortcutCategory::Chat,
        description: "Start / stop dictation in Chat View",
        wired: true,
    },
    ShortcutSpec {
        id: "voice.read_newest",
        display: "Alt+V",
        gpui_key: "alt-v",
        category: ShortcutCategory::Chat,
        description: "Read newest assistant reply aloud (double-press stops)",
        wired: false,
    },
    ShortcutSpec {
        id: "voice.read_last",
        display: "Alt+R",
        gpui_key: "alt-r",
        category: ShortcutCategory::Chat,
        description: "Read last assistant reply aloud again",
        wired: false,
    },
    ShortcutSpec {
        id: "voice.read_prev",
        display: "Alt+[",
        gpui_key: "alt-[",
        category: ShortcutCategory::Chat,
        description: "Read previous assistant reply aloud",
        wired: false,
    },
    ShortcutSpec {
        id: "voice.read_next",
        display: "Alt+]",
        gpui_key: "alt-]",
        category: ShortcutCategory::Chat,
        description: "Read next assistant reply aloud",
        wired: false,
    },
    // =========================================================================
    // Browser Pane (12 entries)
    // =========================================================================
    ShortcutSpec {
        id: "browser.toggle",
        display: "Alt+A",
        gpui_key: "alt-a",
        category: ShortcutCategory::Browser,
        description: "Toggle docked Chrome/Edge browser pane",
        wired: true,
    },
    ShortcutSpec {
        id: "browser.toggle_alt",
        display: "Ctrl+Shift+O",
        gpui_key: "ctrl-shift-o",
        category: ShortcutCategory::Browser,
        description: "Toggle docked Edge browser pane (alternate)",
        wired: false,
    },
    ShortcutSpec {
        id: "browser.retry",
        display: "Alt+Shift+A",
        gpui_key: "alt-shift-a",
        category: ShortcutCategory::Browser,
        description: "Retry browser pane connection",
        wired: false,
    },
    ShortcutSpec {
        id: "browser.focus_swap",
        display: "Alt+Q",
        gpui_key: "alt-q",
        category: ShortcutCategory::Browser,
        description: "Jump focus between composer and browser pane",
        wired: true,
    },
    ShortcutSpec {
        id: "browser.focus_swap_alt",
        display: "Alt+M",
        gpui_key: "alt-m",
        category: ShortcutCategory::Browser,
        description: "Jump focus between composer and browser pane (legacy alias)",
        wired: false,
    },
    ShortcutSpec {
        id: "browser.screenshot",
        display: "Ctrl+Shift+S",
        gpui_key: "ctrl-shift-s",
        category: ShortcutCategory::Browser,
        description: "Capture browser screenshot and attach it to the chat draft",
        wired: true,
    },
    ShortcutSpec {
        id: "browser.open_url_tab",
        display: "Alt+W",
        gpui_key: "alt-w",
        category: ShortcutCategory::Browser,
        description: "Open URL in a new browser pane tab",
        wired: false,
    },
    ShortcutSpec {
        id: "browser.close_tab",
        display: "Ctrl+W",
        gpui_key: "ctrl-w",
        category: ShortcutCategory::Browser,
        description: "Close active browser pane tab (browser-owned; use Ctrl+Shift+W to delete session)",
        wired: false,
    },
    ShortcutSpec {
        id: "browser.insert_url",
        display: "Ctrl+Shift+X",
        gpui_key: "ctrl-shift-x",
        category: ShortcutCategory::Browser,
        description: "Insert current browser tab URL at composer caret",
        wired: false,
    },
    ShortcutSpec {
        id: "browser.address_bar",
        display: "Ctrl+Shift+L",
        gpui_key: "ctrl-shift-l",
        category: ShortcutCategory::Browser,
        description: "Focus browser address bar",
        wired: false,
    },
    ShortcutSpec {
        id: "browser.address_bar_alt",
        display: "Ctrl+L",
        gpui_key: "ctrl-l",
        category: ShortcutCategory::Browser,
        description: "Focus browser address bar (alternate)",
        wired: false,
    },
    ShortcutSpec {
        id: "browser.keep_awake",
        display: "Alt+P",
        gpui_key: "alt-p",
        category: ShortcutCategory::Browser,
        description: "Keep active tab's browser pane awake (prevent suspend)",
        wired: false,
    },
    // =========================================================================
    // System & Settings (12 entries)
    // =========================================================================
    ShortcutSpec {
        id: "system.new_window",
        display: "Ctrl+N",
        gpui_key: "ctrl-n",
        category: ShortcutCategory::System,
        description: "Open a new Multibot window / workspace",
        wired: true,
    },
    ShortcutSpec {
        id: "system.settings",
        display: "Ctrl+,",
        gpui_key: "ctrl-,",
        category: ShortcutCategory::System,
        description: "Open GPU & VPS Settings",
        wired: true,
    },
    ShortcutSpec {
        id: "system.shortcuts",
        display: "F1",
        gpui_key: "f1",
        category: ShortcutCategory::System,
        description: "Open Keyboard Shortcuts overview",
        wired: true,
    },
    ShortcutSpec {
        id: "system.rename",
        display: "F2",
        gpui_key: "f2",
        category: ShortcutCategory::System,
        description: "Rename active session (sidebar, header, or Inbox)",
        wired: true,
    },
    ShortcutSpec {
        id: "system.escape",
        display: "Esc",
        gpui_key: "escape",
        category: ShortcutCategory::System,
        description: "Close active overlay / modal (or stop working agent)",
        wired: true,
    },
    ShortcutSpec {
        id: "snippets.open",
        display: "Ctrl+;",
        gpui_key: "ctrl-;",
        category: ShortcutCategory::System,
        description: "Open saved snippets",
        wired: false,
    },
    ShortcutSpec {
        id: "snippets.open_alt",
        display: "Alt+;",
        gpui_key: "alt-;",
        category: ShortcutCategory::System,
        description: "Open saved snippets (alternate)",
        wired: false,
    },
    ShortcutSpec {
        id: "spawn.cmd",
        display: "Ctrl+Shift+1",
        gpui_key: "ctrl-shift-1",
        category: ShortcutCategory::System,
        description: "New CMD tab",
        wired: false,
    },
    ShortcutSpec {
        id: "spawn.tmux",
        display: "Ctrl+Shift+2",
        gpui_key: "ctrl-shift-2",
        category: ShortcutCategory::System,
        description: "New TMUX tab",
        wired: false,
    },
    ShortcutSpec {
        id: "spawn.vps_1",
        display: "Ctrl+Shift+3",
        gpui_key: "ctrl-shift-3",
        category: ShortcutCategory::System,
        description: "New VPS Remote host 1 tab",
        wired: false,
    },
    ShortcutSpec {
        id: "spawn.vps_2",
        display: "Ctrl+Shift+4",
        gpui_key: "ctrl-shift-4",
        category: ShortcutCategory::System,
        description: "New VPS Remote host 2 tab",
        wired: false,
    },
    ShortcutSpec {
        id: "spawn.powershell",
        display: "Ctrl+Shift+5",
        gpui_key: "ctrl-shift-5",
        category: ShortcutCategory::System,
        description: "New PowerShell tab",
        wired: false,
    },
];

/// Chat capture handles Ctrl+D before text editing, without claiming AltGr or copy-code.
pub fn is_inline_dictation_chord(
    control: bool,
    shift: bool,
    alt: bool,
    platform: bool,
    key: &str,
) -> bool {
    control && !shift && !alt && !platform && key.eq_ignore_ascii_case("d")
}

pub fn by_id(id: &str) -> Option<&'static ShortcutSpec> {
    SHORTCUTS.iter().find(|s| s.id == id)
}

pub fn by_gpui_key(gpui_key: &str) -> Option<&'static ShortcutSpec> {
    SHORTCUTS
        .iter()
        .find(|s| s.gpui_key.eq_ignore_ascii_case(gpui_key))
}

pub fn by_category(category: ShortcutCategory) -> impl Iterator<Item = &'static ShortcutSpec> {
    SHORTCUTS.iter().filter(move |s| s.category == category)
}

/// Returns true if any keystroke in a GPUI chord combines Ctrl + Alt (AltGr conflict).
pub fn is_altgr_chord(gpui_key: &str) -> bool {
    gpui_key.split_whitespace().any(|stroke| {
        let mut has_ctrl = false;
        let mut has_alt = false;
        for part in stroke.split('-') {
            if part.eq_ignore_ascii_case("ctrl") {
                has_ctrl = true;
            } else if part.eq_ignore_ascii_case("alt") {
                has_alt = true;
            }
        }
        has_ctrl && has_alt
    })
}

fn is_valid_gpui_key_name(key: &str) -> bool {
    if key.is_empty() {
        return false;
    }
    if key.len() == 1 {
        let b = key.as_bytes()[0];
        return (0x21..=0x7E).contains(&b) && !b.is_ascii_uppercase();
    }
    if matches!(
        key,
        "up" | "down"
            | "left"
            | "right"
            | "tab"
            | "space"
            | "escape"
            | "enter"
            | "backspace"
            | "delete"
            | "home"
            | "end"
            | "pageup"
            | "pagedown"
            | "insert"
    ) {
        return true;
    }
    if let Some(num) = key.strip_prefix('f')
        && !num.is_empty()
        && !num.starts_with('0')
        && num.bytes().all(|b| b.is_ascii_digit())
        && let Ok(n) = num.parse::<u8>()
    {
        return (1..=24).contains(&n);
    }
    false
}

/// Validates that `gpui_key` conforms strictly to GPUI's `KeyBinding::new` / `Keystroke::parse`
/// syntax (single-space-separated ASCII-lowercase strokes, no `secondary-`, no duplicate modifiers,
/// valid key name including literal `-` via trailing `--`).
pub fn is_valid_gpui_key(gpui_key: &str) -> bool {
    if gpui_key.is_empty()
        || gpui_key.starts_with(' ')
        || gpui_key.ends_with(' ')
        || gpui_key.bytes().any(|b| b.is_ascii_uppercase())
    {
        return false;
    }

    let mut stroke_count = 0usize;
    for stroke in gpui_key.split(' ') {
        if stroke.is_empty() || stroke.chars().any(char::is_whitespace) {
            return false;
        }
        stroke_count += 1;
        let (mods, key): (Vec<&str>, &str) = if stroke == "-" {
            (Vec::new(), "-")
        } else if let Some(prefix) = stroke.strip_suffix("--") {
            if prefix.is_empty() {
                return false;
            }
            let parts: Vec<&str> = prefix.split('-').collect();
            if parts.iter().any(|p| p.is_empty()) {
                return false;
            }
            (parts, "-")
        } else {
            let parts: Vec<&str> = stroke.split('-').collect();
            if parts.iter().any(|p| p.is_empty()) {
                return false;
            }
            let (&last, init) = match parts.split_last() {
                Some(v) => v,
                None => return false,
            };
            (init.to_vec(), last)
        };

        let mut seen_ctrl = false;
        let mut seen_alt = false;
        let mut seen_shift = false;
        let mut seen_fn = false;
        let mut seen_platform = false;
        for m in mods {
            match m {
                "ctrl" => {
                    if seen_ctrl {
                        return false;
                    }
                    seen_ctrl = true;
                }
                "alt" => {
                    if seen_alt {
                        return false;
                    }
                    seen_alt = true;
                }
                "shift" => {
                    if seen_shift {
                        return false;
                    }
                    seen_shift = true;
                }
                "fn" => {
                    if seen_fn {
                        return false;
                    }
                    seen_fn = true;
                }
                "cmd" | "super" | "win" => {
                    if seen_platform {
                        return false;
                    }
                    seen_platform = true;
                }
                _ => return false,
            }
        }

        if !is_valid_gpui_key_name(key) {
            return false;
        }
    }

    stroke_count > 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn shortcuts_have_unique_ids_and_keys() {
        let mut ids = HashSet::new();
        let mut keys = HashSet::new();
        let mut multi_stroke_prefixes = HashSet::new();

        for s in SHORTCUTS {
            assert!(!s.id.is_empty(), "empty shortcut id");
            assert!(!s.display.is_empty(), "empty display for {}", s.id);
            assert!(!s.gpui_key.is_empty(), "empty gpui_key for {}", s.id);
            assert!(!s.description.is_empty(), "empty description for {}", s.id);

            assert!(ids.insert(s.id), "duplicate shortcut id: {}", s.id);
            let normalized_key = s
                .gpui_key
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_ascii_lowercase();
            assert!(
                keys.insert(normalized_key.clone()),
                "conflicting gpui_key: {} ({})",
                s.gpui_key,
                s.id
            );

            let tokens: Vec<&str> = normalized_key.split_whitespace().collect();
            if tokens.len() > 1 {
                multi_stroke_prefixes.insert(tokens[0].to_string());
            }
        }

        for s in SHORTCUTS {
            let tokens: Vec<&str> = s.gpui_key.split_whitespace().collect();
            if tokens.len() == 1 {
                let single = tokens[0].to_ascii_lowercase();
                assert!(
                    !multi_stroke_prefixes.contains(&single),
                    "prefix collision: single-stroke '{}' ({}) collides with multi-stroke sequence",
                    s.gpui_key,
                    s.id
                );
            }
        }

        assert_eq!(SHORTCUTS.len(), 97);
    }

    #[test]
    fn zero_altgr_ctrl_alt_chords() {
        for s in SHORTCUTS {
            assert!(
                !is_altgr_chord(s.gpui_key),
                "AltGr conflict (Ctrl+Alt) in gpui_key: {} ({})",
                s.gpui_key,
                s.id
            );
            let key_low = s.gpui_key.to_ascii_lowercase();
            assert!(!key_low.contains("ctrl-alt"), "ctrl-alt in {}", s.id);
            assert!(!key_low.contains("alt-ctrl"), "alt-ctrl in {}", s.id);

            let disp_low = s.display.to_ascii_lowercase();
            assert!(!disp_low.contains("ctrl+alt"), "Ctrl+Alt in {}", s.id);
            assert!(!disp_low.contains("alt+ctrl"), "Alt+Ctrl in {}", s.id);
            assert!(!disp_low.contains("altgr"), "AltGr in {}", s.id);
        }

        assert!(is_altgr_chord("ctrl-alt-s"));
        assert!(is_altgr_chord("alt-ctrl-1"));
        assert!(is_altgr_chord("ctrl-shift-alt-k"));
        assert!(!is_altgr_chord("ctrl-shift-k"));
        assert!(!is_altgr_chord("alt-1"));
        assert!(!is_altgr_chord("ctrl-x ctrl-s"));
    }

    #[test]
    fn all_gpui_keys_have_valid_gpui_syntax() {
        for s in SHORTCUTS {
            assert!(
                is_valid_gpui_key(s.gpui_key),
                "invalid gpui_key '{}' for {}",
                s.gpui_key,
                s.id
            );
        }

        assert!(is_valid_gpui_key("ctrl--"));
        assert!(is_valid_gpui_key("ctrl-="));
        assert!(is_valid_gpui_key("ctrl-/"));
        assert!(is_valid_gpui_key("ctrl-x ctrl-s"));
        assert!(is_valid_gpui_key("alt-["));
        assert!(is_valid_gpui_key("alt-]"));
        assert!(is_valid_gpui_key("f1"));
        assert!(is_valid_gpui_key("f24"));

        assert!(!is_valid_gpui_key("secondary-k"));
        assert!(!is_valid_gpui_key(""));
        assert!(!is_valid_gpui_key("ctrl-"));
        assert!(!is_valid_gpui_key("ctrl-ctrl-k"));
        assert!(!is_valid_gpui_key("Ctrl-k"));
        assert!(!is_valid_gpui_key("--"));
        assert!(!is_valid_gpui_key("ctrl-f25"));
    }

    #[test]
    fn critical_legacy_chords_are_preserved() {
        assert_eq!(by_id("palette.open").unwrap().gpui_key, "ctrl-k");
        assert_eq!(by_id("palette.open_global").unwrap().gpui_key, "ctrl-shift-k");
        assert_eq!(by_id("inbox.open").unwrap().gpui_key, "alt-i");
        assert_eq!(by_id("inbox.toggle").unwrap().gpui_key, "ctrl-shift-i");
        assert_eq!(by_id("view.sidebar_cycle").unwrap().gpui_key, "ctrl-shift-b");
        assert_eq!(by_id("system.settings").unwrap().gpui_key, "ctrl-,");
        assert_eq!(by_id("system.shortcuts").unwrap().gpui_key, "f1");
        assert_eq!(by_id("system.new_window").unwrap().gpui_key, "ctrl-n");
        assert_eq!(by_id("system.escape").unwrap().gpui_key, "escape");
        assert_eq!(
            by_id("chat.jump_to_newest").unwrap().gpui_key,
            "shift-g shift-g"
        );
        assert_eq!(by_id("session.next").unwrap().gpui_key, "ctrl-tab");
        assert_eq!(by_id("session.prev").unwrap().gpui_key, "ctrl-shift-tab");
        for digit in 1..=9 {
            let id = format!("session.jump_{digit}");
            let expected = format!("ctrl-{digit}");
            assert_eq!(by_id(&id).unwrap().gpui_key, expected);
        }

        // Retired Inbox stepper alias remains unbound.
        assert!(by_gpui_key("ctrl-shift-j").is_none());

        // Ctrl+W is strictly owned by Browser Pane (never session delete).
        let close_tab = by_id("browser.close_tab").unwrap();
        assert_eq!(close_tab.gpui_key, "ctrl-w");
        assert!(!close_tab.wired);
        assert_eq!(by_id("session.delete").unwrap().gpui_key, "ctrl-shift-w");
    }

    #[test]
    fn slice_3_shortcuts_are_registered() {
        let expected: &[(&str, &str)] = &[
            ("workspace.jump_1", "alt-1"),
            ("workspace.jump_2", "alt-2"),
            ("workspace.jump_3", "alt-3"),
            ("workspace.jump_4", "alt-4"),
            ("workspace.jump_5", "alt-5"),
            ("workspace.jump_6", "alt-6"),
            ("workspace.jump_7", "alt-7"),
            ("workspace.jump_8", "alt-8"),
            ("workspace.jump_9", "alt-9"),
            ("workspace.prev", "alt-left"),
            ("workspace.next", "alt-right"),
            ("workspace.view_all", "alt-shift-1"),
            ("workspace.view_frequent", "alt-shift-2"),
            ("workspace.view_favorites", "alt-shift-3"),
            ("workspace.favorite", "alt-s"),
            ("workspace.snooze", "ctrl-shift-y"),
            ("inbox.next", "alt-j"),
            ("inbox.prev", "alt-k"),
            ("inbox.unread_only", "ctrl-shift-u"),
            ("session.mark_unread", "alt-u"),
            ("session.waiting", "alt-e"),
            ("session.undo_waiting", "alt-z"),
            ("session.move_up", "ctrl-shift-up"),
            ("session.move_down", "ctrl-shift-down"),
            ("group.toggle", "ctrl-g"),
            ("session.new", "ctrl-t"),
            ("session.new_alt", "alt-t"),
            ("session.delete", "ctrl-shift-w"),
            ("session.archive", "ctrl-shift-e"),
            ("session.reopen_closed", "ctrl-shift-t"),
            ("session.reconnect", "ctrl-shift-r"),
            ("system.rename", "f2"),
        ];
        for &(id, key) in expected {
            let spec = by_id(id).unwrap_or_else(|| panic!("missing Slice 3 shortcut: {id}"));
            assert_eq!(spec.gpui_key, key, "wrong gpui_key for {id}");
        }
    }

    #[test]
    fn inline_dictation_rejects_shift_altgr_and_platform_modifiers() {
        for control in [false, true] {
            for shift in [false, true] {
                for alt in [false, true] {
                    for platform in [false, true] {
                        for key in ["d", "D", "space", "x"] {
                            assert_eq!(
                                is_inline_dictation_chord(control, shift, alt, platform, key),
                                control && !shift && !alt && !platform && matches!(key, "d" | "D"),
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn dictation_and_copy_code_keep_distinct_wired_chords() {
        for (key, id) in [
            ("ctrl-d", "chat.dictation_inline"),
            ("ctrl-shift-space", "chat.dictation"),
            ("ctrl-shift-d", "chat.copy_code"),
        ] {
            let shortcuts: Vec<_> = SHORTCUTS.iter().filter(|s| s.gpui_key == key).collect();
            assert_eq!(shortcuts.len(), 1, "conflicting shortcut: {key}");
            assert_eq!(shortcuts[0].id, id);
            assert!(shortcuts[0].wired, "shortcut must be wired: {key}");
        }
    }

    #[test]
    fn slice_4_shortcuts_are_registered() {
        let expected: &[(&str, &str)] = &[
            ("chat.find", "ctrl-f"),
            ("chat.commands", "ctrl-/"),
            ("view.font_inc", "ctrl-="),
            ("view.font_dec", "ctrl--"),
            ("view.font_reset", "ctrl-0"),
            ("chat.copy_code", "ctrl-shift-d"),
            ("view.fullscreen", "ctrl-shift-f"),
            ("chat.steer", "ctrl-x ctrl-s"),
            ("chat.perm_allow", "alt-y"),
            ("chat.perm_always", "alt-o"),
            ("chat.perm_deny", "alt-d"),
        ];
        for &(id, key) in expected {
            let spec = by_id(id).unwrap_or_else(|| panic!("missing Slice 4 shortcut: {id}"));
            assert_eq!(spec.gpui_key, key, "wrong gpui_key for {id}");
        }
    }

    #[test]
    fn phase_2_reserved_chords_are_conflict_free() {
        let reserved: &[&str] = &[
            "browser.toggle",
            "browser.toggle_alt",
            "browser.retry",
            "browser.focus_swap",
            "browser.focus_swap_alt",
            "browser.screenshot",
            "browser.open_url_tab",
            "browser.close_tab",
            "browser.insert_url",
            "browser.address_bar",
            "browser.address_bar_alt",
            "browser.keep_awake",
            "view.copy_console",
            "view.paste_shell",
            "view.status_cards",
            "view.scan_urls",
            "chat.dictation",
            "chat.dictation_inline",
            "voice.read_newest",
            "voice.read_last",
            "voice.read_prev",
            "voice.read_next",
            "session.remind_me",
            "session.remind_me_alt",
            "snippets.open",
            "snippets.open_alt",
            "room.new",
            "spawn.cmd",
            "spawn.tmux",
            "spawn.vps_1",
            "spawn.vps_2",
            "spawn.powershell",
        ];
        for &id in reserved {
            assert!(
                by_id(id).is_some(),
                "missing reserved Phase 2 shortcut: {id}"
            );
        }

        let partitioned_count: usize = ALL_CATEGORIES
            .iter()
            .map(|&cat| by_category(cat).count())
            .sum();
        assert_eq!(partitioned_count, SHORTCUTS.len());
    }

    #[test]
    fn is_valid_gpui_key_rejects_malformed_function_keys_and_whitespace() {
        // Valid function keys & single-space multi-stroke sequences
        assert!(is_valid_gpui_key("f1"));
        assert!(is_valid_gpui_key("f12"));
        assert!(is_valid_gpui_key("f24"));
        assert!(is_valid_gpui_key("ctrl-f1"));
        assert!(is_valid_gpui_key("ctrl-x ctrl-s"));

        // Malformed function keys (leading +, leading 0, out of range)
        assert!(!is_valid_gpui_key("ctrl-f+1"));
        assert!(!is_valid_gpui_key("ctrl-f01"));
        assert!(!is_valid_gpui_key("f0"));
        assert!(!is_valid_gpui_key("f00"));
        assert!(!is_valid_gpui_key("f25"));
        assert!(!is_valid_gpui_key("f+12"));

        // Non-single-space separators & surrounding whitespace
        assert!(!is_valid_gpui_key("ctrl-x\nctrl-s"));
        assert!(!is_valid_gpui_key("ctrl-x\tctrl-s"));
        assert!(!is_valid_gpui_key("ctrl-x  ctrl-s"));
        assert!(!is_valid_gpui_key(" ctrl-k"));
        assert!(!is_valid_gpui_key("ctrl-k "));
        assert!(!is_valid_gpui_key("\tctrl-k"));
    }

    #[test]
    fn slice_3_and_4_wired_shortcuts_match_contract() {
        let wired_slice34_ids: &[&str] = &[
            "workspace.jump_1",
            "workspace.jump_2",
            "workspace.jump_3",
            "workspace.jump_4",
            "workspace.jump_5",
            "workspace.jump_6",
            "workspace.jump_7",
            "workspace.jump_8",
            "workspace.jump_9",
            "workspace.prev",
            "workspace.next",
            "workspace.view_all",
            "workspace.view_frequent",
            "workspace.view_favorites",
            "workspace.favorite",
            "workspace.snooze",
            "inbox.next",
            "inbox.prev",
            "inbox.unread_only",
            "session.mark_unread",
            "session.waiting",
            "session.remind_me",
            "session.remind_me_alt",
            "session.undo_waiting",
            "session.move_up",
            "session.move_down",
            "group.toggle",
            "session.new",
            "session.new_alt",
            "session.delete",
            "session.archive",
            "session.reopen_closed",
            "session.reconnect",
            "system.rename",
            "view.font_inc",
            "view.font_dec",
            "view.font_reset",
            "chat.copy_code",
            "chat.jump_to_newest",
            "chat.find",
            "chat.commands",
            "view.chat_terminal_toggle",
            "chat.dictation",
            "chat.dictation_inline",
        ];
        for &id in wired_slice34_ids {
            let spec = by_id(id).unwrap_or_else(|| panic!("missing shortcut {id}"));
            assert!(spec.wired, "expected {id} ({}) to be wired: true", spec.gpui_key);
        }
        for (id, key) in [
            ("browser.toggle", "alt-a"),
            ("browser.focus_swap", "alt-q"),
            ("browser.screenshot", "ctrl-shift-s"),
        ] {
            let spec = by_id(id).unwrap();
            assert_eq!(spec.gpui_key, key);
            assert!(spec.wired);
        }
        assert_eq!(SHORTCUTS.iter().filter(|s| s.wired).count(), 67);
        assert!(!by_id("browser.close_tab").unwrap().wired);
        assert!(!by_id("chat.steer").unwrap().wired);
        assert!(by_gpui_key("ctrl-shift-j").is_none());
    }
}
