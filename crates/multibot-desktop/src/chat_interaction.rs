use super::*;

pub(crate) fn clamp_chat_popover(
    position: Point<Pixels>,
    bounds: gpui_kit::Bounds<Pixels>,
    width: f32,
    height: f32,
) -> Point<Pixels> {
    Point {
        x: (position.x - bounds.origin.x)
            .clamp(px(0.0), (bounds.size.width - px(width)).max(px(0.0))),
        y: (position.y - bounds.origin.y)
            .clamp(px(0.0), (bounds.size.height - px(height)).max(px(0.0))),
    }
}

impl MultibotView {
    pub(super) fn sync_chat_interaction_session(&mut self, cx: &mut Context<Self>) {
        let key = self.tool_chat_key();
        if self.chat_state.interaction_session_key.as_ref() != Some(&key) {
            if let Some(handle) = self.chat_state.interaction_window {
                TextSelection::clear_for_window(handle.window_id(), cx);
                let tokens: Vec<_> = self
                    .chat_state
                    .quoted_snippets
                    .iter()
                    .map(QuotedSnippet::token)
                    .collect();
                if !tokens.is_empty() {
                    let input = self.inputs.prompt_input.clone();
                    cx.defer(move |cx| {
                        let _ = handle.update(cx, |_, window, cx| {
                            input.update(cx, |input, cx| {
                                let mut draft = input.value().to_string();
                                for token in tokens {
                                    draft = draft.replace(&token, "");
                                }
                                let end = draft.len();
                                input.set_value(draft, window, cx);
                                input.set_selected_range(end..end, cx);
                            })
                        });
                    });
                }
            }
            self.chat_state.reset_chat_interactions(key);
        }
    }

    pub(super) fn quote_snippet(
        &mut self,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if text.trim().is_empty() {
            return;
        }
        let snippet = QuotedSnippet {
            id: self.chat_state.next_quote_id,
            preview: normalize_ws_collapse(&text).chars().take(32).collect(),
            full_text: text,
        };
        self.chat_state.next_quote_id += 1;
        let token = format!("{} ", snippet.token());
        self.chat_state.quoted_snippets.push(snippet);
        self.chat_state.selection_quote_popover = None;
        self.chat_state.focused_message_idx = None;
        self.chat_state.close_message_menu();
        TextSelection::clear(window, cx);
        self.inputs.prompt_input.update(cx, |input, cx| {
            input.replace(token, window, cx);
            input.focus(window, cx);
        });
        cx.notify();
    }

    pub(super) fn show_selection_quote(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = TextSelection::selected_text(window, cx);
        self.chat_state.selection_quote_popover =
            (!text.trim().is_empty()).then(|| SelectionQuotePopover {
                text,
                position: clamp_chat_popover(position, self.chat_state.panel_bounds, 156.0, 36.0),
            });
        cx.notify();
    }

    pub(super) fn render_selection_quote(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let popover = self.chat_state.selection_quote_popover.as_ref()?;
        let text = popover.text.clone();
        Some(
            h_flex()
                .id("selection-quote-pill")
                .role(Role::Button)
                .test_support()
                .keep_text_selection("selection-quote-keep")
                .absolute()
                .left(popover.position.x)
                .top(popover.position.y)
                .px_3()
                .py_2()
                .gap_3()
                .items_center()
                .rounded(RADIUS_CHIP)
                .bg(theme::bg_surface())
                .border_1()
                .border_color(theme::border_subtle())
                .text_xs()
                .cursor_pointer()
                .child("Quote")
                .child(div().text_color(theme::text_muted()).child("Ctrl+L"))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.chat_state.selection_quote_popover = None;
                    cx.notify();
                }))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.quote_snippet(text.clone(), window, cx);
                    }),
                )
                .into_any_element(),
        )
    }

    pub(super) fn is_composer_text_edit_key(ks: &Keystroke) -> bool {
        let key_l = ks.key.to_ascii_lowercase();
        if matches!(key_l.as_str(), "backspace" | "delete")
            && !ks.modifiers.alt
            && !ks.modifiers.platform
        {
            return true;
        }
        if ks.modifiers.control
            && !ks.modifiers.alt
            && !ks.modifiers.platform
            && !ks.modifiers.shift
        {
            return matches!(key_l.as_str(), "v" | "x" | "z" | "y");
        }
        if !ks.modifiers.control && !ks.modifiers.alt && !ks.modifiers.platform {
            return key_l == "space" || ks.key_char.is_some() || ks.key.chars().count() == 1;
        }
        false
    }

    pub(super) fn sync_slash_picker_from_composer(&mut self, cx: &mut Context<Self>) {
        let composer_val = self.inputs.prompt_input.read(cx).value().to_string();
        if composer_val.contains('/')
            || composer_val.contains('$')
            || self.slash_picker_state.is_open()
        {
            let was_open = self.slash_picker_state.is_open();
            let prev_trigger = self.slash_picker_state.trigger.clone();
            let agent = self
                .selection
                .selected_session
                .as_ref()
                .map(|s| s.agent)
                .unwrap_or(multibot_proto::AgentKind::Claude);
            self.slash_picker_state
                .sync_from_composer(&composer_val, composer_val.len(), agent);
            if self.slash_picker_state.is_open() != was_open
                || self.slash_picker_state.trigger != prev_trigger
            {
                cx.notify();
            }
        }
        self.sync_mention_picker(cx);
    }

    pub(super) fn has_higher_priority_modal(&self) -> bool {
        self.table_modal_state.is_open() || self.active_modal != ActiveModal::None
    }

    pub(super) fn schedule_slash_picker_sync(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.chat_state.find_open || self.has_higher_priority_modal() {
            return;
        }
        self.chat_state.slash_sync_pending = true;
        let entity = cx.entity().downgrade();
        window.defer(cx, move |_window, cx| {
            let _ = entity.update(cx, |this, cx| {
                if this.chat_state.slash_sync_pending && !this.has_higher_priority_modal() {
                    this.chat_state.slash_sync_pending = false;
                    this.sync_slash_picker_from_composer(cx);
                }
            });
        });
    }

    pub(super) fn handle_slash_picker_control_key(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.has_higher_priority_modal() && self.handle_mention_key(key, window, cx) {
            return true;
        }
        if !self.slash_picker_state.is_open() || self.has_higher_priority_modal() {
            return false;
        }
        match self.slash_picker_state.handle_key(key) {
            SlashKeyOutcome::Accepted(item, trigger) => {
                self.chat_state.newest_chord.reset();
                self.chat_state.slash_sync_pending = false;
                self.accept_slash_command_with_trigger(item, trigger, window, cx);
                true
            }
            SlashKeyOutcome::Moved | SlashKeyOutcome::Closed => {
                self.chat_state.newest_chord.reset();
                self.chat_state.slash_sync_pending = false;
                cx.notify();
                true
            }
            SlashKeyOutcome::Ignored => false,
        }
    }

    pub(super) fn handle_chat_escape(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // Never steal Escape from higher-priority overlays (table modal, palette, shortcuts).
        if self.has_higher_priority_modal() {
            return false;
        }
        if self.mention_picker_state.is_open() {
            return self.handle_mention_key("escape", window, cx);
        }
        if self.slash_picker_state.is_open() {
            return self.handle_slash_picker_control_key("escape", window, cx);
        }
        if self.chat_state.drag_selecting {
            self.chat_state.drag_selecting = false;
            TextSelection::end(window, cx);
            cx.notify();
            return true;
        }
        if self.chat_state.selection_quote_popover.take().is_some() {
            TextSelection::clear(window, cx);
            cx.notify();
            return true;
        }
        if self.chat_state.find_open {
            self.chat_state.newest_chord.reset();
            self.chat_state.slash_sync_pending = false;
            self.chat_state.close_find();
            cx.notify();
            return true;
        }
        if self.chat_state.message_action_menu_open {
            self.chat_state.newest_chord.reset();
            self.chat_state.slash_sync_pending = false;
            self.chat_state.close_message_menu();
            cx.notify();
            return true;
        }
        if self.chat_state.focused_message_idx.is_some() {
            self.chat_state.newest_chord.reset();
            self.chat_state.slash_sync_pending = false;
            self.chat_state.focused_message_idx = None;
            self.inputs
                .prompt_input
                .update(cx, |input, cx| input.focus(window, cx));
            cx.notify();
            return true;
        }
        if self.chat_state.reply_quote.is_some() {
            self.chat_state.newest_chord.reset();
            self.chat_state.slash_sync_pending = false;
            self.chat_state.reply_quote = None;
            cx.notify();
            return true;
        }
        false
    }

    pub(super) fn handle_chat_capture_key_down(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_higher_priority_modal() {
            self.chat_state.newest_chord.reset();
            let ks = &ev.keystroke;
            let is_escape = ks.key.eq_ignore_ascii_case("escape");
            let is_fn_key = ks.key.len() >= 2
                && ks.key.as_bytes()[0].eq_ignore_ascii_case(&b'f')
                && ks.key[1..].bytes().all(|b| b.is_ascii_digit());
            if !is_escape
                && !is_fn_key
                && !ks.modifiers.control
                && !ks.modifiers.alt
                && !ks.modifiers.platform
            {
                cx.stop_propagation();
                window.prevent_default();
            }
            return;
        }
        self.sync_chat_interaction_session(cx);
        let ks = &ev.keystroke;
        let control_only = ks.modifiers.control
            && !ks.modifiers.alt
            && !ks.modifiers.platform
            && !ks.modifiers.shift;
        if control_only && ks.key.eq_ignore_ascii_case("l") {
            let text = TextSelection::selected_text(window, cx);
            if !text.trim().is_empty() {
                self.quote_snippet(text, window, cx);
                cx.stop_propagation();
                window.prevent_default();
                return;
            }
        }
        if control_only
            && ks.key.eq_ignore_ascii_case("c")
            && self.chat_state.focused_message_idx.is_some()
            && !self.chat_state.message_action_menu_open
            && TextSelection::selected_text(window, cx).is_empty()
        {
            self.execute_focused_message_action(MessageMenuAction::Copy, window, cx);
            cx.stop_propagation();
            window.prevent_default();
            return;
        }
        if multibot_keys::is_inline_dictation_chord(
            ks.modifiers.control,
            ks.modifiers.shift,
            ks.modifiers.alt,
            ks.modifiers.platform,
            &ks.key,
        ) {
            if !ev.is_held {
                self.toggle_dictation(window, cx);
            }
            cx.stop_propagation();
            window.prevent_default();
            return;
        }
        let is_shift_only_g = ks.key.eq_ignore_ascii_case("g")
            && ks.modifiers.shift
            && !ks.modifiers.control
            && !ks.modifiers.alt
            && !ks.modifiers.platform;
        if !is_shift_only_g || self.chat_state.find_open {
            self.chat_state.newest_chord.reset();
        } else if self.main_surface == MainSurface::Chat {
            let composer_focused = self
                .inputs
                .prompt_input
                .read(cx)
                .focus_handle(cx)
                .is_focused(window);
            let now_ms = self.chat_state.chord_epoch.elapsed().as_millis() as u64;
            let jump = if composer_focused {
                let text = self.inputs.prompt_input.read(cx).value().to_string();
                self.chat_state
                    .newest_chord
                    .on_char('G', &text, now_ms, true, ev.is_held)
            } else {
                self.chat_state
                    .newest_chord
                    .on_transcript_g(now_ms, ev.is_held)
            };
            if jump {
                if composer_focused {
                    self.inputs
                        .prompt_input
                        .update(cx, |state, cx| state.set_value("", window, cx));
                }
                self.chat_state.slash_sync_pending = false;
                self.jump_to_newest(cx);
            }
            if jump || !composer_focused {
                cx.stop_propagation();
                window.prevent_default();
                return;
            }
        }
        let plain_key = !ks.modifiers.control
            && !ks.modifiers.alt
            && !ks.modifiers.platform
            && !ks.modifiers.shift;

        // 1. Intercept slash/skill picker control keys BEFORE Input, Tab focus cycling,
        //    or global CloseOverlay can consume them.
        if (self.slash_picker_state.is_open() || self.mention_picker_state.is_open()) && plain_key {
            let key_l = ks.key.to_ascii_lowercase();
            if matches!(
                key_l.as_str(),
                "tab" | "enter" | "up" | "down" | "pageup" | "pagedown" | "escape"
            ) && self.handle_slash_picker_control_key(&key_l, window, cx)
            {
                cx.stop_propagation();
                window.prevent_default();
                return;
            }
        }

        // 2. Intercept Escape for in-chat overlays/states BEFORE global CloseOverlay steals it,
        //    unless a higher-priority modal (table modal / palette / shortcuts) is open.
        if plain_key && ks.key.eq_ignore_ascii_case("escape") && self.handle_chat_escape(window, cx)
        {
            cx.stop_propagation();
            window.prevent_default();
            return;
        }

        // 2b. Intercept Find bar typing/navigation in capture phase BEFORE prompt_input can
        //     consume printable characters even if focus remained inside composer.
        if self.chat_state.find_open && self.handle_chat_find_key_down(ev, window, cx) {
            return;
        }

        if ks.modifiers.shift
            && !ks.modifiers.control
            && !ks.modifiers.alt
            && !ks.modifiers.platform
            && self.chat_state.focused_message_idx.is_some()
            && !self.chat_state.find_open
        {
            let text = ks.key_char.clone().unwrap_or_else(|| ks.key.to_uppercase());
            if text.chars().count() == 1
                && !text.chars().any(char::is_control)
                && self.handle_empty_composer_nav_key_action(&text, window, cx)
            {
                cx.stop_propagation();
                window.prevent_default();
                return;
            }
        }
        if plain_key
            && !self.chat_state.find_open
            && !self.slash_picker_state.is_open()
            && (self.chat_state.focused_message_idx.is_some() || ks.key.eq_ignore_ascii_case("up"))
            && self.handle_empty_composer_nav_key_action(&ks.key, window, cx)
        {
            cx.stop_propagation();
            window.prevent_default();
            return;
        }

        // 3. Schedule zero-lag post-commit slash picker sync after Input commits any text edit
        //    (flushes immediately via window.defer on live OS key events/auto-repeat and via
        //    on_key_up synchronously within TestWindowExt::press).
        if Self::is_composer_text_edit_key(ks) {
            self.schedule_slash_picker_sync(window, cx);
        }
    }

    pub(crate) fn focus_chat_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let fh = self
            .chat_state
            .focus_handle
            .get_or_insert_with(|| cx.focus_handle())
            .clone();
        window.focus(&fh, cx);
    }

    pub(super) fn handle_chat_find_key_down(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.chat_state.find_open {
            return false;
        }
        let ks = &ev.keystroke;
        let key_l = ks.key.to_ascii_lowercase();
        let no_cmd_mods = !ks.modifiers.control && !ks.modifiers.alt && !ks.modifiers.platform;
        if !no_cmd_mods {
            return false;
        }
        let is_fn_key = ks.key.len() >= 2
            && ks.key.as_bytes()[0].eq_ignore_ascii_case(&b'f')
            && ks.key[1..].bytes().all(|b| b.is_ascii_digit());
        match key_l.as_str() {
            "escape" => {
                self.chat_state.slash_sync_pending = false;
                self.chat_state.close_find();
                cx.stop_propagation();
                window.prevent_default();
                cx.notify();
                true
            }
            "enter" | "space"
                if !ks.modifiers.shift && !self.is_focused_on_mounted_known_handle(window, cx) =>
            {
                false
            }
            "enter" | "f3" => {
                self.chat_state.slash_sync_pending = false;
                let target = if ks.modifiers.shift {
                    self.chat_state.find_prev()
                } else {
                    self.chat_state.find_next()
                };
                if let Some(idx) = target {
                    self.chat_state.scroll_handle.scroll_to_item(idx);
                }
                cx.stop_propagation();
                window.prevent_default();
                cx.notify();
                true
            }
            "backspace" => {
                self.chat_state.slash_sync_pending = false;
                let mut next = self.chat_state.find_query.clone();
                next.pop();
                self.chat_state.update_find_query(
                    next.clone(),
                    &self.history.messages,
                    self.show_tool_calls(),
                );
                self.scroll_to_active_find_match();
                if !next.trim().is_empty() && self.has_unloaded_older_history() {
                    self.search_active_session_history(next, cx);
                }
                cx.stop_propagation();
                window.prevent_default();
                cx.notify();
                true
            }
            _ => {
                let printable: Option<&str> = if let Some(ch) = ks.key_char.as_deref() {
                    if !ch.is_empty() && !ch.chars().any(char::is_control) {
                        Some(ch)
                    } else {
                        None
                    }
                } else if key_l == "space" {
                    Some(" ")
                } else if ks.key.chars().count() == 1 && !ks.key.chars().any(char::is_control) {
                    Some(ks.key.as_str())
                } else {
                    None
                };
                if let Some(ch) = printable {
                    self.chat_state.slash_sync_pending = false;
                    let mut next_q = self.chat_state.find_query.clone();
                    next_q.push_str(ch);
                    self.chat_state.update_find_query(
                        next_q.clone(),
                        &self.history.messages,
                        self.show_tool_calls(),
                    );
                    self.scroll_to_active_find_match();
                    if !next_q.trim().is_empty() && self.has_unloaded_older_history() {
                        self.search_active_session_history(next_q, cx);
                    }
                    cx.stop_propagation();
                    window.prevent_default();
                    cx.notify();
                    return true;
                }
                if !is_fn_key {
                    cx.stop_propagation();
                    window.prevent_default();
                    return true;
                }
                false
            }
        }
    }

    pub(crate) fn handle_chat_key_down(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_higher_priority_modal() {
            return;
        }
        let ks = &ev.keystroke;
        // Ctrl+Shift+D (strictly !alt for AltGr safety) -> copy latest code block
        if is_copy_latest_code_block_chord(ks) {
            self.chat_state.slash_sync_pending = false;
            self.copy_latest_code_block(cx);
            cx.stop_propagation();
            return;
        }

        // Ctrl+F -> open chat find
        if ks.modifiers.control
            && !ks.modifiers.alt
            && !ks.modifiers.shift
            && ks.key.eq_ignore_ascii_case("f")
        {
            self.sidebar_state.rename = None;
            self.sidebar_state.new_workspace_draft = None;
            self.chat_state.slash_sync_pending = false;
            self.slash_picker_state.close();
            self.mention_picker_state.close();
            let msgs = self.history.messages.clone();
            self.chat_state.open_find(&msgs, self.show_tool_calls());
            self.focus_chat_panel(window, cx);
            cx.stop_propagation();
            cx.notify();
            return;
        }

        // Ctrl+/ -> full slash command browser
        if ks.modifiers.control && !ks.modifiers.alt && ks.key == "/" {
            self.sidebar_state.rename = None;
            self.sidebar_state.new_workspace_draft = None;
            self.chat_state.slash_sync_pending = false;
            self.chat_state.close_find();
            self.open_slash_browser(cx);
            self.inputs.prompt_input.update(cx, |state, cx| {
                state.focus(window, cx);
            });
            cx.stop_propagation();
            return;
        }

        // Ctrl+V -> image/file clipboard paste
        if ks.modifiers.control
            && !ks.modifiers.alt
            && !ks.modifiers.shift
            && ks.key.eq_ignore_ascii_case("v")
            && self.handle_paste_from_clipboard(window, cx)
        {
            self.chat_state.slash_sync_pending = false;
            cx.stop_propagation();
            return;
        }

        // Font zoom: Ctrl++ / Ctrl+= / Ctrl+- / Ctrl+0
        if ks.modifiers.control && !ks.modifiers.alt {
            match ks.key.as_str() {
                "+" | "=" => {
                    self.chat_state.slash_sync_pending = false;
                    self.zoom_chat_font(1, cx);
                    cx.stop_propagation();
                    return;
                }
                "-" => {
                    self.chat_state.slash_sync_pending = false;
                    self.zoom_chat_font(-1, cx);
                    cx.stop_propagation();
                    return;
                }
                "0" => {
                    self.chat_state.slash_sync_pending = false;
                    self.reset_chat_font(cx);
                    cx.stop_propagation();
                    return;
                }
                _ => {}
            }
        }

        // Active slash picker keyboard routing (fallback if not already handled in capture phase)
        if self.slash_picker_state.is_open()
            && !ks.modifiers.shift
            && !ks.modifiers.control
            && !ks.modifiers.alt
            && !ks.modifiers.platform
        {
            match self.slash_picker_state.handle_key(&ks.key) {
                SlashKeyOutcome::Accepted(item, trigger) => {
                    self.chat_state.slash_sync_pending = false;
                    self.accept_slash_command_with_trigger(item, trigger, window, cx);
                    cx.stop_propagation();
                    return;
                }
                SlashKeyOutcome::Moved | SlashKeyOutcome::Closed => {
                    self.chat_state.slash_sync_pending = false;
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                SlashKeyOutcome::Ignored => {}
            }
        }

        // Active find bar navigation & live query typing (delegate to shared handler so capture
        // and bubble never diverge, including Tab-focused Find bar button Enter/Space activation)
        if self.chat_state.find_open {
            let _ = self.handle_chat_find_key_down(ev, window, cx);
            return;
        }

        // End -> jump to newest
        if !ks.modifiers.control && !ks.modifiers.alt && ks.key.eq_ignore_ascii_case("end") {
            let composer_val = self.inputs.prompt_input.read(cx).value().to_string();
            if composer_val.is_empty() {
                self.chat_state.slash_sync_pending = false;
                self.jump_to_newest(cx);
                self.inputs
                    .prompt_input
                    .update(cx, |input, cx| input.focus(window, cx));
                cx.stop_propagation();
                return;
            }
        }

        // Empty-composer Up/Down/Enter/Delete/Esc message navigation
        if !ks.modifiers.control
            && !ks.modifiers.alt
            && !ks.modifiers.shift
            && self.handle_empty_composer_nav_key_action(&ks.key, window, cx)
        {
            cx.stop_propagation();
        }
    }

    pub(super) fn scroll_to_active_find_match(&mut self) {
        if let Some(idx) = self
            .chat_state
            .find_matches
            .get(self.chat_state.find_current)
            .copied()
        {
            self.chat_state.scroll_handle.scroll_to_item(idx);
        }
    }

    pub(super) fn handle_empty_composer_nav_key_action(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let input = self.inputs.prompt_input.read(cx);
        let composer_empty = input.value().is_empty()
            || (input.focus_handle(cx).is_focused(window) && input.selected_range() == (0..0));
        let visible = project_visible_messages_with_overrides(
            &self.history.messages,
            &self.chat_state.local_reactions,
            &self.chat_state.local_reaction_removals,
            self.show_tool_calls(),
        );
        let focused_meta = self
            .chat_state
            .focused_message_idx
            .and_then(|i| visible.get(i))
            .map(|v| {
                let tool_only = v.message.text.trim().is_empty()
                    && !v.message.blocks.is_empty()
                    && v.message
                        .blocks
                        .iter()
                        .all(|b| matches!(b, MessageBlock::ToolUse { .. }));
                let waiting = self.chat_state.failed_sends.contains_key(&v.message.id)
                    || self.outbox.outbox_msg_map.contains_key(&v.message.id);
                (
                    v.original_index,
                    v.message.id.clone(),
                    message_visible_text(v.message),
                    tool_only,
                    waiting,
                )
            });
        let (is_tool_only, is_waiting) = focused_meta
            .as_ref()
            .map(|(_, _, _, t, w)| (*t, *w))
            .unwrap_or((false, false));

        let outcome = self.chat_state.handle_empty_composer_nav_key(
            key,
            composer_empty,
            visible.len(),
            is_tool_only,
            is_waiting,
        );
        match outcome {
            MessageNavOutcome::Ignored => false,
            MessageNavOutcome::FocusMoved(Some(vis_idx)) => {
                self.chat_state.scroll_handle.scroll_to_item(vis_idx);
                self.focus_chat_panel(window, cx);
                cx.notify();
                true
            }
            MessageNavOutcome::ReturnedToComposer => {
                self.inputs.prompt_input.update(cx, |input, cx| {
                    input.focus(window, cx);
                    if key.chars().count() == 1 || key == "space" {
                        input.replace(if key == "space" { " " } else { key }, window, cx);
                    }
                });
                cx.notify();
                true
            }
            MessageNavOutcome::FocusMoved(None)
            | MessageNavOutcome::MenuOpened(_)
            | MessageNavOutcome::MenuSelectionMoved(_)
            | MessageNavOutcome::DismissedQuote => {
                cx.notify();
                true
            }
            MessageNavOutcome::CancelWaiting(_) => {
                if let Some((orig_idx, msg_id, _, _, _)) = focused_meta {
                    self.chat_state.failed_sends.remove(&msg_id);
                    self.cancel_outbox_message(&msg_id);
                    if orig_idx < self.history.messages.len() {
                        self.history.messages.remove(orig_idx);
                    }
                }
                cx.notify();
                true
            }
            MessageNavOutcome::ExecuteAction { action, .. } => {
                if let Some((orig_idx, msg_id, msg_text, _, _)) = focused_meta {
                    self.execute_message_action(orig_idx, msg_id, msg_text, action, window, cx);
                }
                true
            }
        }
    }

    fn execute_message_action(
        &mut self,
        orig_idx: usize,
        msg_id: String,
        msg_text: String,
        action: MessageMenuAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.chat_state.close_message_menu();
        match action {
            MessageMenuAction::Reply => {
                self.set_reply_quote(msg_id, msg_text, cx);
                self.chat_state.focused_message_idx = None;
                self.inputs
                    .prompt_input
                    .update(cx, |input, cx| input.focus(window, cx));
            }
            MessageMenuAction::Copy => {
                cx.write_to_clipboard(ClipboardItem::new_string(msg_text));
                self.notice = Some("Copied message.".into());
                cx.notify();
            }
            MessageMenuAction::Quote => {
                self.quote_snippet(msg_text, window, cx);
            }
            MessageMenuAction::React(emoji) => {
                self.toggle_local_reaction(&msg_id, &msg_text, emoji, cx);
            }
            MessageMenuAction::CancelWaiting => {
                self.chat_state.failed_sends.remove(&msg_id);
                self.cancel_outbox_message(&msg_id);
                if orig_idx < self.history.messages.len() {
                    self.history.messages.remove(orig_idx);
                }
                cx.notify();
            }
        }
    }

    pub(crate) fn execute_focused_message_action(
        &mut self,
        action: MessageMenuAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let visible = project_visible_messages_with_overrides(
            &self.history.messages,
            &self.chat_state.local_reactions,
            &self.chat_state.local_reaction_removals,
            self.show_tool_calls(),
        );
        let Some(message) = self
            .chat_state
            .focused_message_idx
            .or_else(|| visible.len().checked_sub(1))
            .and_then(|index| visible.get(index))
        else {
            return;
        };
        let (index, id, text) = (
            message.original_index,
            message.message.id.clone(),
            message_visible_text(message.message),
        );
        self.execute_message_action(index, id, text, action, window, cx);
    }

    pub(super) fn render_message_action_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.chat_state.message_action_menu_open {
            return None;
        }
        let visible = project_visible_messages_with_overrides(
            &self.history.messages,
            &self.chat_state.local_reactions,
            &self.chat_state.local_reaction_removals,
            self.show_tool_calls(),
        );
        let message = self
            .chat_state
            .focused_message_idx
            .and_then(|index| visible.get(index))?;
        let waiting = self
            .chat_state
            .failed_sends
            .contains_key(&message.message.id)
            || self.outbox.outbox_msg_map.contains_key(&message.message.id);
        let mut reactions = h_flex().gap_2().p_1();
        for (index, emoji) in ReactionEmoji::all().into_iter().enumerate() {
            reactions = reactions.child(
                div()
                    .id(format!("message-react-{index}"))
                    .role(Role::Button)
                    .test_support()
                    .px_3()
                    .py_2()
                    .rounded(RADIUS_CHIP)
                    .cursor_pointer()
                    .bg(
                        if self.chat_state.reaction_picker_open
                            && self.chat_state.message_action_index == index
                        {
                            theme::bg_active_row()
                        } else {
                            theme::bg_surface()
                        },
                    )
                    .hover(|style| style.bg(theme::bg_hover()))
                    .child(emoji.glyph())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.execute_focused_message_action(
                                MessageMenuAction::React(emoji),
                                window,
                                cx,
                            );
                        }),
                    ),
            );
        }
        let mut menu = v_flex()
            .id("message-context-menu")
            .test_support()
            .w(px(224.0).min(self.chat_state.panel_bounds.size.width))
            .max_h(self.chat_state.panel_bounds.size.height)
            .overflow_y_scroll()
            .p_2()
            .gap_1()
            .rounded(theme::RADIUS_MODAL)
            .bg(theme::bg_surface())
            .border_1()
            .border_color(theme::border_subtle())
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.chat_state.close_message_menu();
                cx.notify();
            }))
            .child(reactions);
        if !self.chat_state.reaction_picker_open {
            for (action, label, icon) in [
                (MessageMenuAction::Reply, "Reply", icons::CORNER_DOWN_LEFT),
                (MessageMenuAction::Copy, "Copy", icons::COPY),
                (MessageMenuAction::Quote, "Quote", icons::CORNER_DOWN_LEFT),
                (MessageMenuAction::CancelWaiting, "Cancel waiting", icons::X),
            ] {
                if action == MessageMenuAction::CancelWaiting && !waiting {
                    continue;
                }
                let index = ChatViewState::menu_actions_for(false, waiting)
                    .iter()
                    .position(|candidate| *candidate == action)
                    .unwrap();
                menu = menu.child(
                    h_flex()
                        .id(format!("message-action-{index}"))
                        .role(Role::Button)
                        .test_support()
                        .px_3()
                        .py_2()
                        .gap_2()
                        .items_center()
                        .rounded(RADIUS_ROW)
                        .cursor_pointer()
                        .bg(if self.chat_state.message_action_index == index {
                            theme::bg_active_row()
                        } else {
                            theme::bg_surface()
                        })
                        .hover(|style| style.bg(theme::bg_hover()))
                        .child(lucide_icon(icon, 14.0))
                        .child(label)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.execute_focused_message_action(action, window, cx);
                            }),
                        ),
                );
            }
        }
        let position = self.chat_state.message_context_position.unwrap_or_else(|| {
            let bounds = self.chat_state.panel_bounds;
            clamp_chat_popover(bounds.center(), bounds, 224.0, 72.0)
        });
        Some(
            div()
                .id("message-action-menu")
                .test_support()
                .absolute()
                .left(position.x)
                .top(position.y)
                .child(menu)
                .into_any_element(),
        )
    }

    pub(crate) fn render_find_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.chat_state.find_open {
            return None;
        }
        let counter = if self.chat_state.find_matches.is_empty() {
            "0 matches".to_string()
        } else {
            format!(
                "{} / {}",
                self.chat_state.find_current + 1,
                self.chat_state.find_matches.len()
            )
        };
        Some(
            div()
                .w_full()
                .when(!self.settings.shell_fullscreen, |el| el.max_w(px(760.0)))
                .mx_auto()
                .px_4()
                .pt_1()
                .child(
                    h_flex()
                        .id("chat-find-bar")
                        .w_full()
                        .px_3()
                        .py_1p5()
                        .justify_between()
                        .items_center()
                        .rounded(RADIUS_ROW)
                        .bg(theme::bg_surface())
                        .border_1()
                        .border_color(theme::border_subtle())
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme::text_secondary())
                                .child(format!(
                                    "Find: {} ({counter})",
                                    if self.chat_state.find_query.is_empty() {
                                        "type query…"
                                    } else {
                                        self.chat_state.find_query.as_str()
                                    }
                                )),
                        )
                        .child(
                            h_flex()
                                .gap_1()
                                .items_center()
                                .child(
                                    Button::new("chat-find-prev")
                                        .xsmall()
                                        .icon(lucide_icon(icons::CHEVRON_UP, 14.0))
                                        .accessibility_label("Previous match")
                                        .tooltip("Previous match")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            if let Some(idx) = this.chat_state.find_prev() {
                                                this.chat_state.scroll_handle.scroll_to_item(idx);
                                            }
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new("chat-find-next")
                                        .xsmall()
                                        .icon(lucide_icon(icons::CHEVRON_DOWN, 14.0))
                                        .accessibility_label("Next match")
                                        .tooltip("Next match")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            if let Some(idx) = this.chat_state.find_next() {
                                                this.chat_state.scroll_handle.scroll_to_item(idx);
                                            }
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new("chat-find-close")
                                        .xsmall()
                                        .icon(lucide_icon(icons::X, 12.0))
                                        .accessibility_label("Close find bar")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.chat_state.close_find();
                                            this.focus_active_surface_default(window, cx);
                                            cx.notify();
                                        })),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }
}
