use super::*;

impl MultibotView {
    pub(super) fn register_chat_actions(
        &self,
        root: gpui_kit::Stateful<gpui_kit::Div>,
        cx: &mut Context<Self>,
    ) -> gpui_kit::Stateful<gpui_kit::Div> {
        root.capture_action(cx.listener(|this, _: &input::IndentInline, window, cx| {
            if this.has_higher_priority_modal() {
                cx.stop_propagation();
                return;
            }
            if this.handle_slash_picker_control_key("tab", window, cx) {
                cx.stop_propagation();
            }
        }))
        .capture_action(cx.listener(|this, action: &input::Enter, window, cx| {
            if this.has_higher_priority_modal() {
                cx.stop_propagation();
                return;
            }
            if !action.secondary
                && !action.shift
                && this.handle_slash_picker_control_key("enter", window, cx)
            {
                cx.stop_propagation();
                return;
            }
            if this.chat_state.find_open && !action.secondary {
                this.chat_state.slash_sync_pending = false;
                let target = if action.shift {
                    this.chat_state.find_prev()
                } else {
                    this.chat_state.find_next()
                };
                if let Some(idx) = target {
                    this.chat_state.scroll_handle.scroll_to_item(idx);
                }
                cx.notify();
                cx.stop_propagation();
                return;
            }
            if !action.secondary
                && !action.shift
                && this.chat_state.focused_message_idx.is_some()
                && this.handle_empty_composer_nav_key_action("enter", window, cx)
            {
                this.chat_state.slash_sync_pending = false;
                cx.stop_propagation();
                return;
            }
            if action.secondary && !action.shift {
                this.chat_state.slash_sync_pending = false;
                this.send_current_prompt(window, cx);
                cx.stop_propagation();
            }
        }))
        .capture_action(cx.listener(|this, _: &input::MoveUp, window, cx| {
            if this.has_higher_priority_modal() {
                cx.stop_propagation();
                return;
            }
            if this.handle_slash_picker_control_key("up", window, cx) {
                cx.stop_propagation();
                return;
            }
            if this.chat_state.find_open {
                cx.stop_propagation();
                return;
            }
            if this.handle_empty_composer_nav_key_action("up", window, cx) {
                this.chat_state.slash_sync_pending = false;
                cx.stop_propagation();
            }
        }))
        .capture_action(cx.listener(|this, _: &input::MoveDown, window, cx| {
            if this.has_higher_priority_modal() {
                cx.stop_propagation();
                return;
            }
            if this.handle_slash_picker_control_key("down", window, cx) {
                cx.stop_propagation();
                return;
            }
            if this.chat_state.find_open {
                cx.stop_propagation();
                return;
            }
            if this.handle_empty_composer_nav_key_action("down", window, cx) {
                this.chat_state.slash_sync_pending = false;
                cx.stop_propagation();
            }
        }))
        .capture_action(cx.listener(|this, _: &input::MoveEnd, _, cx| {
            if this.has_higher_priority_modal() || this.chat_state.find_open {
                cx.stop_propagation();
                return;
            }
            if this.inputs.prompt_input.read(cx).value().is_empty() {
                this.chat_state.slash_sync_pending = false;
                this.jump_to_newest(cx);
                cx.stop_propagation();
            }
        }))
        .capture_action(cx.listener(|this, _: &input::MovePageUp, window, cx| {
            if this.handle_slash_picker_control_key("pageup", window, cx) {
                cx.stop_propagation();
            }
        }))
        .capture_action(cx.listener(|this, _: &input::MovePageDown, window, cx| {
            if this.handle_slash_picker_control_key("pagedown", window, cx) {
                cx.stop_propagation();
            }
        }))
        .capture_action(cx.listener(|this, _: &input::Escape, window, cx| {
            if this.handle_chat_escape(window, cx) {
                cx.stop_propagation();
            }
        }))
        .capture_action(cx.listener(|this, _: &CloseOverlay, window, cx| {
            if this.handle_chat_escape(window, cx) {
                cx.stop_propagation();
            }
        }))
        .capture_action(cx.listener(|this, _: &input::Backspace, window, cx| {
            if this.has_higher_priority_modal() {
                cx.stop_propagation();
                return;
            }
            if this.chat_state.find_open {
                this.chat_state.slash_sync_pending = false;
                let mut next = this.chat_state.find_query.clone();
                next.pop();
                let msgs = this.history.messages.clone();
                this.chat_state
                    .update_find_query(next, &msgs, this.show_tool_calls());
                cx.notify();
                cx.stop_propagation();
                return;
            }
            this.schedule_slash_picker_sync(window, cx);
        }))
        .capture_action(cx.listener(|this, _: &input::Delete, window, cx| {
            if this.has_higher_priority_modal() || this.chat_state.find_open {
                cx.stop_propagation();
                return;
            }
            if this.handle_empty_composer_nav_key_action("delete", window, cx) {
                this.chat_state.slash_sync_pending = false;
                cx.stop_propagation();
                return;
            }
            this.schedule_slash_picker_sync(window, cx);
        }))
        .capture_action(
            cx.listener(|this, _: &input::DeleteToBeginningOfLine, window, cx| {
                if this.has_higher_priority_modal() || this.chat_state.find_open {
                    cx.stop_propagation();
                    return;
                }
                this.schedule_slash_picker_sync(window, cx);
            }),
        )
        .capture_action(
            cx.listener(|this, _: &input::DeleteToEndOfLine, window, cx| {
                if this.has_higher_priority_modal() || this.chat_state.find_open {
                    cx.stop_propagation();
                    return;
                }
                this.schedule_slash_picker_sync(window, cx);
            }),
        )
        .capture_action(
            cx.listener(|this, _: &input::DeleteToPreviousWordStart, window, cx| {
                if this.has_higher_priority_modal() || this.chat_state.find_open {
                    cx.stop_propagation();
                    return;
                }
                this.schedule_slash_picker_sync(window, cx);
            }),
        )
        .capture_action(
            cx.listener(|this, _: &input::DeleteToNextWordEnd, window, cx| {
                if this.has_higher_priority_modal() || this.chat_state.find_open {
                    cx.stop_propagation();
                    return;
                }
                this.schedule_slash_picker_sync(window, cx);
            }),
        )
        .capture_action(cx.listener(|this, _: &input::Cut, window, cx| {
            if this.has_higher_priority_modal() || this.chat_state.find_open {
                cx.stop_propagation();
                return;
            }
            this.schedule_slash_picker_sync(window, cx);
        }))
        .capture_action(cx.listener(|this, _: &input::Paste, window, cx| {
            if this.has_higher_priority_modal() || this.chat_state.find_open {
                cx.stop_propagation();
                return;
            }
            if this.handle_paste_from_clipboard(window, cx) {
                this.chat_state.slash_sync_pending = false;
                cx.stop_propagation();
                return;
            }
            this.schedule_slash_picker_sync(window, cx);
        }))
        .capture_action(cx.listener(|this, _: &input::Undo, window, cx| {
            if this.has_higher_priority_modal() || this.chat_state.find_open {
                cx.stop_propagation();
                return;
            }
            this.schedule_slash_picker_sync(window, cx);
        }))
        .capture_action(cx.listener(|this, _: &input::Redo, window, cx| {
            if this.has_higher_priority_modal() || this.chat_state.find_open {
                cx.stop_propagation();
                return;
            }
            this.schedule_slash_picker_sync(window, cx);
        }))
        .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
            // The Computer View URL field owns Escape while it has focus, so the pane
            // closes instead of the chat surface losing the key to a modal handler.
            if this.handle_computer_view_key_down(ev, window, cx) {
                return;
            }
            // The terminal grid owns printable keys, arrows and Ctrl+C/D/L/Z while the
            // pane has focus. `Alt+C` never matches here: it closes the pane instead.
            if this.handle_terminal_key_down(ev, window, cx) {
                return;
            }
            this.handle_chat_capture_key_down(ev, window, cx);
        }))
        .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
            this.handle_chat_key_down(ev, window, cx);
        }))
        .on_key_up(cx.listener(|this, _ev: &KeyUpEvent, _window, cx| {
            if this.has_higher_priority_modal() {
                return;
            }
            if this.chat_state.slash_sync_pending {
                this.chat_state.slash_sync_pending = false;
                this.sync_slash_picker_from_composer(cx);
            }
        }))
        .on_drop::<ExternalPaths>(cx.listener(
            |this, paths: &ExternalPaths, window, cx| {
                this.stage_dropped_paths(paths.paths(), window, cx);
            },
        ))
    }
}
