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
        description: "Jump to session 1",
        wired: true,
    },
    ShortcutSpec {
        id: "session.jump_2",
        display: "Ctrl+2",
        gpui_key: "ctrl-2",
        category: ShortcutCategory::Navigation,
        description: "Jump to session 2",
        wired: true,
    },
    ShortcutSpec {
        id: "session.jump_3",
        display: "Ctrl+3",
        gpui_key: "ctrl-3",
        category: ShortcutCategory::Navigation,
        description: "Jump to session 3",
        wired: true,
    },
    ShortcutSpec {
        id: "session.jump_4",
        display: "Ctrl+4",
        gpui_key: "ctrl-4",
        category: ShortcutCategory::Navigation,
        description: "Jump to session 4",
        wired: true,
    },
    ShortcutSpec {
        id: "session.jump_5",
        display: "Ctrl+5",
        gpui_key: "ctrl-5",
        category: ShortcutCategory::Navigation,
        description: "Jump to session 5",
        wired: true,
    },
    ShortcutSpec {
        id: "session.jump_6",
        display: "Ctrl+6",
        gpui_key: "ctrl-6",
        category: ShortcutCategory::Navigation,
        description: "Jump to session 6",
        wired: true,
    },
    ShortcutSpec {
        id: "session.jump_7",
        display: "Ctrl+7",
        gpui_key: "ctrl-7",
        category: ShortcutCategory::Navigation,
        description: "Jump to session 7",
        wired: true,
    },
    ShortcutSpec {
        id: "session.jump_8",
        display: "Ctrl+8",
        gpui_key: "ctrl-8",
        category: ShortcutCategory::Navigation,
        description: "Jump to session 8",
        wired: true,
    },
    ShortcutSpec {
        id: "session.jump_9",
        display: "Ctrl+9",
        gpui_key: "ctrl-9",
        category: ShortcutCategory::Navigation,
        description: "Jump to session 9",
        wired: true,
    },
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
        wired: false,
    },
    ShortcutSpec {
        id: "inbox.prev",
        display: "Alt+K",
        gpui_key: "alt-k",
        category: ShortcutCategory::Inbox,
        description: "Previous Inbox session / cycle workspace",
        wired: false,
    },
    ShortcutSpec {
        id: "inbox.unread_only",
        display: "Ctrl+Shift+U",
        gpui_key: "ctrl-shift-u",
        category: ShortcutCategory::Inbox,
        description: "Toggle global unread-only filter",
        wired: false,
    },
    ShortcutSpec {
        id: "workspace.favorite",
        display: "Alt+S",
        gpui_key: "alt-s",
        category: ShortcutCategory::Workspaces,
        description: "Favorite or unfavorite active session",
        wired: false,
    },
    ShortcutSpec {
        id: "workspace.snooze",
        display: "Ctrl+Shift+Y",
        gpui_key: "ctrl-shift-y",
        category: ShortcutCategory::Workspaces,
        description: "Snooze active workspace",
        wired: false,
    },
    ShortcutSpec {
        id: "view.sidebar_cycle",
        display: "Ctrl+Shift+B",
        gpui_key: "ctrl-shift-b",
        category: ShortcutCategory::View,
        description: "Cycle sidebar (open / hidden)",
        wired: true,
    },
    ShortcutSpec {
        id: "view.chat_terminal_toggle",
        display: "Alt+C",
        gpui_key: "alt-c",
        category: ShortcutCategory::View,
        description: "Toggle Chat View / built-in Terminal",
        wired: false,
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
        id: "chat.copy_code",
        display: "Ctrl+Shift+D",
        gpui_key: "ctrl-shift-d",
        category: ShortcutCategory::Chat,
        description: "Copy latest code block",
        wired: false,
    },
    ShortcutSpec {
        id: "chat.dictation",
        display: "Ctrl+Shift+Space",
        gpui_key: "ctrl-shift-space",
        category: ShortcutCategory::Chat,
        description: "Start / stop voice dictation",
        wired: false,
    },
    ShortcutSpec {
        id: "browser.toggle",
        display: "Alt+A",
        gpui_key: "alt-a",
        category: ShortcutCategory::Browser,
        description: "Toggle docked Chrome/Edge browser pane",
        wired: false,
    },
    ShortcutSpec {
        id: "browser.focus_swap",
        display: "Alt+Q",
        gpui_key: "alt-q",
        category: ShortcutCategory::Browser,
        description: "Jump focus between composer and browser pane",
        wired: false,
    },
    ShortcutSpec {
        id: "browser.screenshot",
        display: "Ctrl+Shift+S",
        gpui_key: "ctrl-shift-s",
        category: ShortcutCategory::Browser,
        description: "Screenshot browser pane and paste link into chat",
        wired: false,
    },
    ShortcutSpec {
        id: "system.new_window",
        display: "Ctrl+N",
        gpui_key: "ctrl-n",
        category: ShortcutCategory::System,
        description: "Open a new Multibot window (shared process)",
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
        description: "Rename active session",
        wired: false,
    },
    ShortcutSpec {
        id: "system.escape",
        display: "Esc",
        gpui_key: "escape",
        category: ShortcutCategory::System,
        description: "Close active overlay / modal",
        wired: true,
    },
];

pub fn by_id(id: &str) -> Option<&'static ShortcutSpec> {
    SHORTCUTS.iter().find(|s| s.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn shortcuts_have_unique_ids_and_keys() {
        let mut ids = HashSet::new();
        let mut keys = HashSet::new();
        for s in SHORTCUTS {
            assert!(ids.insert(s.id), "duplicate shortcut id: {}", s.id);
            assert!(
                keys.insert(s.gpui_key),
                "conflicting gpui_key: {} ({})",
                s.gpui_key,
                s.id
            );
            assert!(!s.description.is_empty());
        }
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
    }
}
