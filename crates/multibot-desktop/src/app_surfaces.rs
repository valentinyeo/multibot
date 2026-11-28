use super::*;

impl MultibotView {
    /// Returns `true` when any Chat-local transient overlay is mounted and active.
    pub(crate) fn has_mounted_chat_overlay(&self) -> bool {
        self.main_surface == MainSurface::Chat
            && (self.slash_picker_state.is_open()
                || self.mention_picker_state.is_open()
                || self.chat_state.find_open
                || self.chat_state.message_action_menu_open
                || self.chat_state.focused_message_idx.is_some()
                || self.chat_state.reply_quote.is_some())
    }

    /// Returns `true` if `session_key` is currently rendered as a visible row or pinned tile
    /// inside `#sidebar-open` (accounting for active workspace/view, `unread_only`, collapsed
    /// pinned/group/recents sections, and existence as a non-archived session in `self.connection.sessions`).
    pub(crate) fn is_session_rendered_in_open_sidebar(&self, session_key: &str) -> bool {
        if !self
            .find_session_by_key(session_key)
            .is_some_and(|s| !s.archived)
        {
            return false;
        }
        let now = now_unix_ms();
        let inputs = self.session_inputs();
        let active_key = self
            .selection
            .selected_session
            .as_ref()
            .map(|s| self.session_key_for(s));
        let layout = self
            .sidebar_state
            .workspace_store
            .compute_sidebar_layout_with_options(
                &self.sidebar_state.active_workspace_id,
                self.sidebar_state.active_workspace_view,
                &inputs,
                self.sidebar_state.unread_only,
                active_key.as_deref(),
                now,
            );
        if layout
            .pinned_tiles
            .iter()
            .any(|r| r.session_key == session_key)
        {
            return true;
        }
        for gv in &layout.groups {
            if !gv.group.collapsed && gv.items.iter().any(|r| r.session_key == session_key) {
                return true;
            }
        }
        if layout.recents.iter().any(|r| r.session_key == session_key) {
            return true;
        }
        false
    }

    /// Returns `true` when the sidebar is actually drawn for the current surface.
    /// The Inbox is keyboard-first and minimal: it never renders the sidebar, while the
    /// user's `sidebar_mode` preference stays untouched and reappears when leaving Inbox.
    pub(crate) fn sidebar_rendered(&self) -> bool {
        self.sidebar_state.is_shown() && self.main_surface != MainSurface::Inbox
    }

    /// Returns `true` if `session_key` can mount an inline sidebar or top-pill rename input
    /// in the current surface and sidebar visibility state.
    pub(crate) fn can_mount_sidebar_rename(&self, session_key: &str) -> bool {
        if self.main_surface == MainSurface::Inbox {
            // The minimal Inbox never draws the sidebar, so no sidebar rename can mount there.
            return false;
        }
        match self.sidebar_state.mode {
            SidebarMode::Open => self.is_session_rendered_in_open_sidebar(session_key),
            SidebarMode::Collapsed | SidebarMode::Hidden => {
                self.main_surface == MainSurface::Chat
                    && self
                        .selection
                        .selected_session
                        .as_ref()
                        .is_some_and(|s| self.session_key_for(s) == session_key)
                    && self
                        .find_session_by_key(session_key)
                        .is_some_and(|s| !s.archived)
            }
        }
    }

    /// Returns `true` when `sidebar_state.rename` has a visibly mounted input:
    /// either on a rendered row/tile inside `#sidebar-open`, or in `#header-rename-input`
    /// for the currently selected non-archived session on `MainSurface::Chat` when the
    /// sidebar is Collapsed/Hidden.
    pub(crate) fn is_sidebar_rename_mounted(&self) -> bool {
        self.sidebar_state
            .rename
            .as_ref()
            .is_some_and(|r| self.can_mount_sidebar_rename(&r.session_key))
    }

    /// Returns `true` when `sidebar_state.new_workspace_draft` has a mounted input
    /// (only inside a rendered `#sidebar-open`).
    pub(crate) fn is_sidebar_workspace_draft_mounted(&self) -> bool {
        self.sidebar_state.new_workspace_draft.is_some()
            && self.sidebar_state.mode == SidebarMode::Open
            && self.sidebar_rendered()
    }

    /// Returns `true` when an Open-sidebar inline edit (rename or new-workspace draft) is
    /// currently mounted inside `#sidebar-open`.
    pub(crate) fn is_sidebar_inline_edit_mounted(&self) -> bool {
        self.sidebar_state.mode == SidebarMode::Open
            && self.sidebar_rendered()
            && (self.is_sidebar_rename_mounted() || self.is_sidebar_workspace_draft_mounted())
    }

    /// Returns `true` if `session_key` is currently rendered in the visible Inbox rows.
    pub(crate) fn is_session_rendered_in_inbox(&self, session_key: &str) -> bool {
        self.find_session_by_key(session_key)
            .is_some_and(|s| !s.archived)
            && self
                .compute_inbox_rows(now_unix_ms())
                .iter()
                .any(|row| row.session_key == session_key)
    }

    /// Returns `true` when `inbox_state.rename` has a visibly mounted input on a
    /// currently visible row inside `#inbox-root`.
    pub(crate) fn is_inbox_rename_mounted(&self) -> bool {
        self.main_surface == MainSurface::Inbox
            && self
                .inbox_state
                .rename
                .as_ref()
                .is_some_and(|r| self.is_session_rendered_in_inbox(&r.session_key))
    }

    /// Clears transient surface- or sidebar-local inline edit states whose UI subtree or
    /// target row is no longer visibly mounted.
    pub(crate) fn cleanup_unmounted_surface_state(&mut self) {
        if self.main_surface != MainSurface::Inbox {
            self.inbox_state.filter_focused = false;
            self.inbox_state.inbox_search_open = false;
            self.inbox_state.filter_query.clear();
        }
        if !self.is_inbox_rename_mounted() {
            self.inbox_state.rename = None;
        }
        if !self.is_sidebar_workspace_draft_mounted() {
            self.sidebar_state.new_workspace_draft = None;
        }
        if !self.is_sidebar_rename_mounted() {
            self.sidebar_state.rename = None;
        }
    }

    /// Transitions `self.main_surface` to `next` and clears any transient inline edit state
    /// that is no longer mounted or would lose focus across the surface transition.
    pub(crate) fn set_main_surface(&mut self, next: MainSurface) {
        self.chat_state.composer_menu_open = false;
        if self.main_surface != next {
            #[cfg(windows)]
            if next != MainSurface::Chat {
                self.hide_browser_pane();
            }
            self.voice.cancel();
            self.sidebar_state.rename = None;
            self.sidebar_state.new_workspace_draft = None;
            // Hard Invariant #4: the Computer View pane docks beside the chat, which the
            // Inbox does not render, so its backend and its browser processes must go with
            // the unmount.
            if next == MainSurface::Inbox && self.computer.is_open() {
                self.teardown_computer_view();
            }
            // Hard Invariant #4: the Inbox surface does not render the terminal pane, so
            // its PTY, child process and reader thread must go with the unmount.
            if next == MainSurface::Inbox && self.terminal.is_open() {
                if let Some(session) = self.terminal.session.take() {
                    session.request_reader_stop();
                    drop(session);
                }
                self.terminal.screen = None;
                self.terminal.error = None;
                self.terminal.wake_task = None;
                self.terminal.wake_tx = None;
                self.terminal.wake_rx = None;
            }
        }
        if next == MainSurface::Inbox && self.main_surface != MainSurface::Inbox {
            // Every fresh Inbox open starts clean: search bar hidden, no query.
            self.inbox_state.close_search();
        }
        if next == MainSurface::Inbox {
            // Never open the Inbox on a split that is hidden because it holds 0 sessions
            let splits = self.inbox_visible_splits(now_unix_ms());
            self.inbox_state.normalize_active_split(&splits);
        }
        self.main_surface = next;
        self.cleanup_unmounted_surface_state();
    }

    pub(crate) fn set_sidebar_mode_synced(&mut self, mode: SidebarMode, cx: &mut Context<Self>) {
        self.sidebar_state.set_mode(mode);
        self.cleanup_unmounted_surface_state();
        self.settings.sidebar_mode = mode;
        self.sync_settings_to_shared(cx);
        if self.sidebar_state.persist_to_disk {
            let _ = self.settings.save();
        }
        cx.notify();
    }

    pub(crate) fn toggle_shell_fullscreen(&mut self, cx: &mut Context<Self>) {
        if self.main_surface != MainSurface::Chat {
            return;
        }
        let Some(session) = self.selection.selected_session.as_ref() else {
            return;
        };
        let key = self.session_key_for(session);
        let meta = self
            .sidebar_state
            .workspace_store
            .session_meta
            .entry(key)
            .or_default();
        meta.full_width = !meta.full_width;
        let full_width = meta.full_width;
        self.apply_session_full_width(full_width);
        self.persist_workspace_store_if_enabled();
        self.sync_workspace_to_shared(cx);
        cx.notify();
    }

    pub(super) fn apply_session_full_width(&mut self, full_width: bool) {
        self.settings.shell_fullscreen = full_width;
        self.cleanup_unmounted_surface_state();
    }

    pub(crate) fn cycle_sidebar_mode_synced(&mut self, cx: &mut Context<Self>) {
        self.sidebar_state.cycle();
        let next = self.sidebar_state.mode;
        self.set_sidebar_mode_synced(next, cx);
    }
}
