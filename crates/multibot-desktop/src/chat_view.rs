#[path = "chat_activity.rs"]
mod activity;
pub(crate) use activity::*;
#[path = "chat_actions.rs"]
mod actions;
#[path = "chat_attachments.rs"]
mod attachments;
#[path = "chat_blocks.rs"]
mod blocks;
#[path = "chat_composer.rs"]
mod composer;
#[path = "chat_interaction.rs"]
mod interaction;
#[path = "chat_markdown.rs"]
mod markdown;
#[path = "chat_reactions.rs"]
mod reactions;
#[path = "chat_send.rs"]
mod send;
#[path = "chat_text.rs"]
mod text;
#[path = "chat_view_state.rs"]
mod view_state;
pub(crate) use attachments::*;
pub(crate) use markdown::*;
pub(crate) use reactions::*;
pub(crate) use text::*;
pub(crate) use view_state::*;

use crate::app::{CloseOverlay, MultibotView};
use crate::icons::{self, lucide_icon};
use crate::slash_picker::SlashKeyOutcome;
use crate::state::{ActiveModal, MainSurface, SharedAppStore};
use crate::table_modal::{TableSource, extract_tables_from_markdown};
use crate::theme::{self, RADIUS_BUBBLE, RADIUS_CHIP, RADIUS_ROW};
use gpui_kit::{
    AnyElement, ClipboardEntry, ClipboardItem, Context, ExternalPaths, FocusHandle, Focusable,
    Hsla, InteractiveElement, IntoElement, KeyDownEvent, KeyUpEvent, Keystroke, MouseButton,
    MouseDownEvent, MouseUpEvent, ParentElement, Pixels, Point, Role, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, TestSupportExt, Window,
    base::{ElementExt, TextSelection, text::CodeBlock},
    component::{
        Disableable, Selectable, Sizable,
        button::{Button, ButtonCustomVariant, ButtonVariants},
        h_flex,
        input::{self, Textarea},
        label::Label,
        text::{TableData, TextView},
        v_flex,
    },
    div, img,
    prelude::FluentBuilder,
    px, rgb, rgba,
};
use multibot_core::cache::sha256_hex;
use multibot_core::chat_scroll::{ChatScrollTracker, NewestChord, PendingScrollTarget};
use multibot_core::daemon_client::DaemonTarget;
use multibot_core::settings::{
    DEFAULT_CHAT_FONT_SIZE_PT, MAX_CHAT_FONT_SIZE_PT, MIN_CHAT_FONT_SIZE_PT, MultibotSettings,
};
use multibot_core::workspaces::{canonical_session_key, now_unix_ms};
use multibot_proto::{
    AgentKind, ChatMessage, ClientRequest, DiffBlock, EffortLevel, MessageBlock, MessageRole,
    PermissionLevel, PromptQuestion, SessionOptions, SessionSummary, TodoItem, TodoStatus,
    ToolStatus,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

pub(crate) const DIFF_FOLDED_ROWS: usize = 6;
pub(crate) const DIFF_EXPANDED_ROWS: usize = 40;
pub(crate) const MAX_PROMPT_QUESTIONS: usize = 4;
pub(crate) const MAX_PROMPT_OPTIONS: usize = 4;
pub(crate) const MAX_QUOTE_LINES: usize = 10;
pub(crate) const DOUBLE_CLICK_DRAG_THRESHOLD_PX: f32 = 4.0;
pub(crate) const DOUBLE_CLICK_WINDOW_MS: u64 = 500;
pub(crate) const MAX_IMAGE_ATTACHMENTS: usize = 10;
pub(crate) const MAX_FILE_ATTACHMENTS: usize = 10;
pub(crate) const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
pub(crate) const MAX_FILE_BYTES: u64 = 200 * 1024 * 1024;
const REACTION_QUOTE_MAX_UTF16: usize = 80;
const REMOTE_NAME_MAX_BYTES: usize = 80;

fn accent_red() -> Hsla {
    rgb(0xEF4444).into()
}

fn diff_added_bg() -> Hsla {
    rgb(0x1C4028).into()
}

fn diff_added_fg() -> Hsla {
    rgb(0x86EFAC).into()
}

fn diff_removed_bg() -> Hsla {
    rgb(0x4A2024).into()
}

fn diff_removed_fg() -> Hsla {
    rgb(0xFCA5A5).into()
}

fn error_strip_bg() -> Hsla {
    rgb(0x3B181A).into()
}

fn error_strip_border() -> Hsla {
    rgb(0x7F1D1D).into()
}

fn code_bg() -> Hsla {
    rgb(0x141414).into()
}

// ============================================================================
// 7. MultibotView Chat Actions & Rich Rendering
// ============================================================================

impl MultibotView {
    fn active_chat_tab_key(&self) -> String {
        self.selection
            .selected_session
            .as_ref()
            .map(|s| s.id.clone())
            .unwrap_or_else(|| "__default__".to_string())
    }

    fn effective_font_size_pt(&self) -> u16 {
        self.chat_state
            .font_size_pt
            .clamp(MIN_CHAT_FONT_SIZE_PT, MAX_CHAT_FONT_SIZE_PT)
    }

    pub(crate) fn zoom_chat_font(&mut self, delta: i16, cx: &mut Context<Self>) {
        let next = self.chat_state.zoom_font(delta);
        self.settings.chat_font_size_pt = next;
        cx.notify();
    }

    pub(crate) fn reset_chat_font(&mut self, cx: &mut Context<Self>) {
        let next = self.chat_state.reset_font();
        self.settings.chat_font_size_pt = next;
        cx.notify();
    }

    pub(crate) fn follow_chat_from_composer(&mut self, cx: &mut Context<Self>) {
        self.chat_state.scroll.sent_message();
        self.chat_state.prepend_scroll_anchor = None;
        self.chat_state.scroll_handle.scroll_to_bottom();
        cx.notify();
    }

    pub(crate) fn jump_to_newest(&mut self, cx: &mut Context<Self>) {
        self.chat_state.scroll.jump_to_bottom();
        self.chat_state.focused_message_idx = None;
        self.chat_state.close_message_menu();
        cx.notify();
    }

    pub(crate) fn toggle_todo_drawer(&mut self, cx: &mut Context<Self>) -> bool {
        let key = self.active_chat_tab_key();
        let entry = self.chat_state.todo_drawer_open.entry(key).or_insert(false);
        *entry = !*entry;
        let open = *entry;
        cx.notify();
        open
    }

    fn tool_chat_key(&self) -> String {
        self.selection
            .selected_session
            .as_ref()
            .map(|session| self.session_key_for(session))
            .unwrap_or_else(|| "__default__".into())
    }

    pub(crate) fn show_tool_calls(&self) -> bool {
        self.chat_state
            .show_all_tools
            .contains(&self.tool_chat_key())
    }

    fn tool_cluster_open(&self, msg: &ChatMessage, msg_idx: usize) -> bool {
        let chat = self.tool_chat_key();
        let message = tool_cluster_key(msg, msg_idx);
        self.chat_state
            .tool_cluster_overrides
            .get(&(chat.clone(), message))
            .copied()
            .unwrap_or(false)
    }

    pub(crate) fn toggle_all_tool_steps(&mut self, cx: &mut Context<Self>) -> bool {
        let chat = self.tool_chat_key();
        let show = !self.chat_state.show_all_tools.contains(&chat);
        self.chat_state
            .tool_cluster_overrides
            .retain(|(key, _), _| key != &chat);
        let prefix = format!("{chat}:");
        self.chat_state
            .expanded_tools
            .retain(|key| !key.starts_with(&prefix));
        self.chat_state
            .expanded_diffs
            .retain(|key| !key.starts_with(&prefix));
        if show {
            self.chat_state.show_all_tools.insert(chat);
        } else {
            self.chat_state.show_all_tools.remove(&chat);
        }
        self.chat_state.focused_message_idx = None;
        self.chat_state.close_message_menu();
        let query = self.chat_state.find_query.clone();
        self.chat_state
            .update_find_query(query, &self.history.messages, show);
        cx.notify();
        show
    }

    pub(crate) fn copy_latest_code_block(&mut self, cx: &mut Context<Self>) -> bool {
        if let Some(code) = extract_latest_code_block(&self.history.messages) {
            cx.write_to_clipboard(ClipboardItem::new_string(code));
            self.notice = Some("Copied latest code block.".into());
            cx.notify();
            true
        } else {
            self.notice = Some("No code block in this chat.".into());
            cx.notify();
            false
        }
    }

    pub(crate) fn render_chat_panel(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_chat_interaction_session(cx);
        let header_title = match &self.selection.selected_session {
            Some(s) => format!("{}  {}", s.agent.glyph(), s.title),
            None => "Multibot · Ready".to_string(),
        };
        let is_working = self.is_active_chat_working();
        self.sync_remote_image_fetches(cx);
        let busy_phase = self.chat_state.busy_tick_phase % 3;
        let visible_messages = project_visible_messages_with_overrides(
            &self.history.messages,
            &self.chat_state.local_reactions,
            &self.chat_state.local_reaction_removals,
            self.show_tool_calls(),
        );
        let trailing_prose_options = detect_trailing_prose_options(&self.history.messages);
        let active_find_idx = self.chat_state.active_find_message_index();

        let unread_visible_idx = self
            .chat_state
            .scroll
            .unread_divider_msg_idx
            .and_then(|idx| {
                visible_messages
                    .iter()
                    .position(|message| message.original_index >= idx)
            });
        match std::mem::take(&mut self.chat_state.scroll.pending_scroll) {
            // GPUI latches this request and applies it against the NEW layout's scroll_max.
            PendingScrollTarget::Bottom => self.chat_state.scroll_handle.scroll_to_bottom(),
            PendingScrollTarget::UnreadDivider(_) => {
                if let Some(idx) = unread_visible_idx {
                    self.chat_state.scroll_handle.scroll_to_top_of_item(idx);
                } else {
                    self.chat_state.scroll.jump_to_bottom();
                    self.chat_state.scroll.pending_scroll = PendingScrollTarget::None;
                    self.chat_state.scroll_handle.scroll_to_bottom();
                }
            }
            PendingScrollTarget::None => {}
        }
        let view = cx.entity().downgrade();
        // Each direct scroll child is one visible message, including its NEW divider.
        let mut msg_col = v_flex()
            .on_children_prepainted(move |_, _, cx| {
                let _ = view.update(cx, |this, cx| {
                    let handle = &this.chat_state.scroll_handle;
                    let size = handle.bounds().size;
                    if size != this.chat_state.viewport_size && this.chat_state.scroll.follow_bottom
                    {
                        let offset = Point {
                            x: handle.offset().x,
                            y: -handle.max_offset().y,
                        };
                        if handle.offset() != offset {
                            handle.set_offset(offset);
                            cx.notify();
                        }
                    }
                    this.chat_state.viewport_size = size;
                    if let Some((old_max, old_offset)) =
                        this.chat_state.prepend_scroll_anchor.take()
                    {
                        let handle = &this.chat_state.scroll_handle;
                        let mut offset = old_offset;
                        offset.y -= handle.max_offset().y - old_max;
                        handle.set_offset(offset);
                        cx.notify();
                    } else {
                        let handle = &this.chat_state.scroll_handle;
                        let distance = f32::from(handle.max_offset().y + handle.offset().y);
                        if this.chat_state.scroll.sync_position(distance) {
                            cx.notify();
                        }
                    }
                });
            })
            .id("chat-scroll")
            .flex_1()
            .min_h_0()
            .w_full()
            .gap_3()
            // Clip selectable text at the overlays so GPUI's single native drag-scroll
            // owner sees the same viewport edges the user sees.
            .mt(px(self.chat_state.header_height))
            .mb(px(self.chat_state.composer_height))
            .pt_3()
            .pb_3()
            .px_4()
            .overflow_y_scroll()
            .track_scroll(&self.chat_state.scroll_handle)
            .capture_any_mouse_down(cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                if ev.button == MouseButton::Left {
                    this.chat_state.drag_selecting =
                        TextSelection::is_selectable_at(ev.position, window, cx);
                    this.chat_state.selection_quote_popover = None;
                }
            }));

        if !visible_messages.is_empty() {
            let last_visible_orig_idx = visible_messages.last().map(|v| v.original_index);
            let latest_visible_assistant_orig_idx = visible_messages
                .iter()
                .rev()
                .find(|v| v.message.role == MessageRole::Assistant)
                .map(|v| v.original_index);

            for (vis_idx, vis) in visible_messages.iter().enumerate() {
                let msg = vis.message;
                let orig_idx = vis.original_index;
                let is_user = msg.role == MessageRole::User;
                let is_latest = last_visible_orig_idx == Some(orig_idx);
                let is_focused = self.chat_state.focused_message_idx == Some(vis_idx);
                let is_find_hit = active_find_idx == Some(vis_idx);
                let failed_send = self.chat_state.failed_sends.get(&msg.id);

                let msg_id_down = msg.id.clone();
                let msg_id_up = msg.id.clone();
                let msg_visible_text = message_visible_text(msg);
                let msg_text_up = msg_visible_text.clone();

                let mut body_col = v_flex().w_auto().min_w_0().gap_2();

                // Compact 1-line clickable quote header when message quotes an earlier turn.
                let parsed_quote = parse_quoted_message(&msg_visible_text);
                if let Some(ref pq) = parsed_quote {
                    let quoted_id_target = msg.quoted_message_id.clone();
                    let quoted_source_target = pq.source.clone();
                    body_col = body_col.child(
                        div()
                            .id(SharedString::from(format!("quote-ref-{orig_idx}")))
                            .role(Role::Button)
                            .test_support()
                            .px_2p5()
                            .py_1()
                            .rounded(RADIUS_ROW)
                            .bg(theme::bg_canvas())
                            .border_l_2()
                            .border_color(theme::accent_blue())
                            .hover(|s| s.bg(theme::bg_hover()))
                            .cursor_pointer()
                            .text_xs()
                            .text_color(theme::text_muted())
                            .truncate()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    if let Some(vis_target) = find_visible_quote_target(
                                        &this.history.messages,
                                        &this.chat_state.local_reactions,
                                        &this.chat_state.local_reaction_removals,
                                        quoted_id_target.as_deref(),
                                        &quoted_source_target,
                                        this.show_tool_calls(),
                                    ) {
                                        this.chat_state.scroll_handle.scroll_to_item(vis_target);
                                        cx.notify();
                                    }
                                }),
                            )
                            .flex()
                            .items_center()
                            .gap_1()
                            .tooltip(|window, cx| {
                                gpui_kit::component::tooltip::Tooltip::new("Go to replied message")
                                    .build(window, cx)
                            })
                            .child(lucide_icon(icons::CORNER_DOWN_LEFT, 14.0))
                            .child(div().min_w_0().truncate().child(pq.preview.clone())),
                    );
                }

                if !msg.blocks.is_empty() {
                    let mut saw_primary_active_prompt = false;
                    let mut stripped_quote_prefix = false;
                    let tool_count = msg
                        .blocks
                        .iter()
                        .filter(|b| matches!(b, MessageBlock::ToolUse { .. }))
                        .count();
                    let mut tool_cluster_rendered = false;
                    for (block_idx, block) in msg.blocks.iter().enumerate() {
                        match block {
                            MessageBlock::Text { text } => {
                                let display_text = if !stripped_quote_prefix
                                    && parsed_quote.is_some()
                                    && !text.is_empty()
                                {
                                    stripped_quote_prefix = true;
                                    strip_rendered_quote_prefix(text)
                                } else {
                                    text.clone()
                                };
                                if !display_text.is_empty() {
                                    if is_user {
                                        body_col = body_col.child(
                                            div()
                                                .id(SharedString::from(format!(
                                                    "msg-user-text-{orig_idx}-{block_idx}"
                                                )))
                                                .test_support()
                                                .text_size(px(self.effective_font_size_pt() as f32))
                                                .text_color(theme::text_primary())
                                                .child(
                                                    if crate::mention_picker::occurrences(
                                                        &display_text,
                                                        &self.mention_targets(),
                                                    )
                                                    .is_empty()
                                                        && crate::external_url::url_ranges(
                                                            &display_text,
                                                        )
                                                        .is_empty()
                                                    {
                                                        display_text.into_any_element()
                                                    } else {
                                                        self.render_markdown_with_actions(
                                                            SharedString::from(format!(
                                                                "msg-user-md-{orig_idx}-{block_idx}"
                                                            )),
                                                            display_text,
                                                            orig_idx,
                                                            cx,
                                                        )
                                                    },
                                                ),
                                        );
                                    } else {
                                        body_col =
                                            body_col.child(self.render_markdown_with_actions(
                                                SharedString::from(format!(
                                                    "msg-md-{orig_idx}-{block_idx}"
                                                )),
                                                display_text,
                                                orig_idx,
                                                cx,
                                            ));
                                    }
                                }
                            }
                            MessageBlock::ToolUse {
                                call_id,
                                name,
                                summary,
                                detail,
                                status,
                                diff,
                                output,
                            } => {
                                if !self.show_tool_calls() {
                                    continue;
                                }
                                if !tool_cluster_rendered {
                                    body_col = body_col.child(
                                        self.render_tool_cluster(msg, orig_idx, tool_count, cx),
                                    );
                                    tool_cluster_rendered = true;
                                }
                                if !self.tool_cluster_open(msg, orig_idx) {
                                    continue;
                                }
                                body_col = body_col.child(self.render_tool_step_block(
                                    msg,
                                    orig_idx,
                                    block_idx,
                                    call_id,
                                    name,
                                    summary,
                                    detail,
                                    *status,
                                    diff.as_ref(),
                                    output.as_deref(),
                                    cx,
                                ));
                            }
                            MessageBlock::Image {
                                url,
                                local_path,
                                alt,
                            } => {
                                body_col = body_col.child(self.render_image_block(
                                    url.as_deref(),
                                    local_path.as_deref(),
                                    alt,
                                    &format!("{orig_idx}-block-{block_idx}"),
                                    cx,
                                ));
                            }
                            MessageBlock::QuestionPrompt {
                                prompt_id,
                                questions,
                                ..
                            } => {
                                let partially_answered_latest_assistant =
                                    latest_visible_assistant_orig_idx == Some(orig_idx)
                                        && !self.chat_state.answered_prompts.contains(prompt_id)
                                        && self
                                            .chat_state
                                            .answered_questions
                                            .get(prompt_id)
                                            .is_some_and(|set| !set.is_empty());
                                let is_active_prompt =
                                    is_latest || partially_answered_latest_assistant;
                                let is_primary_active_prompt =
                                    is_active_prompt && !saw_primary_active_prompt;
                                if let Some(card) = self.render_question_prompt_block(
                                    orig_idx,
                                    block_idx,
                                    prompt_id,
                                    questions,
                                    is_active_prompt,
                                    is_primary_active_prompt,
                                    cx,
                                ) {
                                    if is_primary_active_prompt {
                                        saw_primary_active_prompt = true;
                                    }
                                    body_col = body_col.child(card);
                                }
                            }
                        }
                    }
                } else {
                    let display_text = parsed_quote
                        .as_ref()
                        .map(|pq| pq.body.clone())
                        .unwrap_or_else(|| msg.text.clone());
                    if is_user {
                        body_col = body_col.child(
                            div()
                                .text_size(px(self.effective_font_size_pt() as f32))
                                .text_color(theme::text_primary())
                                .child(
                                    if crate::mention_picker::occurrences(
                                        &display_text,
                                        &self.mention_targets(),
                                    )
                                    .is_empty()
                                        && crate::external_url::url_ranges(&display_text).is_empty()
                                    {
                                        display_text.into_any_element()
                                    } else {
                                        self.render_markdown_with_actions(
                                            SharedString::from(format!("msg-user-md-{orig_idx}")),
                                            display_text,
                                            orig_idx,
                                            cx,
                                        )
                                    },
                                ),
                        );
                    } else {
                        body_col = body_col.child(self.render_markdown_with_actions(
                            SharedString::from(format!("msg-md-{orig_idx}")),
                            display_text,
                            orig_idx,
                            cx,
                        ));
                    }
                }

                for (image_idx, url) in message_image_urls(msg).iter().enumerate() {
                    body_col = body_col.child(self.render_image_block(
                        Some(url),
                        None,
                        "",
                        &format!("{orig_idx}-url-{image_idx}"),
                        cx,
                    ));
                }

                // Strictly gated trailing prose option fallback on latest assistant message ONLY.
                if is_latest
                    && !is_user
                    && let Some(ref prose_opts) = trailing_prose_options
                {
                    body_col = body_col.child(self.render_prose_options_card(prose_opts, cx));
                }

                // Message container (status-free chrome: NO "Sending...", clock, read-check, ETA, model badges).
                let bubble_border = if is_find_hit || is_focused {
                    theme::accent_blue()
                } else {
                    theme::border_subtle()
                };

                let bubble = if is_user {
                    v_flex()
                        .id(SharedString::from(format!("msg-bubble-{orig_idx}")))
                        .role(Role::Button)
                        .test_support()
                        .w_auto()
                        .min_w_0()
                        .max_w(px(560.0))
                        .px_4()
                        .py_2p5()
                        .rounded(RADIUS_BUBBLE)
                        .bg(theme::bg_user_bubble())
                        .when(is_find_hit || is_focused, |el| {
                            el.border_1().border_color(bubble_border)
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, ev: &MouseDownEvent, _, _| {
                                let x: f32 = ev.position.x.into();
                                let y: f32 = ev.position.y.into();
                                this.chat_state.pointer_down_origin =
                                    Some((msg_id_down.clone(), x, y, Instant::now()));
                            }),
                        )
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(move |this, ev: &MouseUpEvent, _, cx| {
                                let x: f32 = ev.position.x.into();
                                let y: f32 = ev.position.y.into();
                                let now = Instant::now();
                                let down = this.chat_state.pointer_down_origin.take();
                                let prev_click = this.chat_state.last_click_record.clone();
                                this.chat_state.last_click_record =
                                    Some((msg_id_up.clone(), x, y, now));
                                let down_info = down
                                    .as_ref()
                                    .map(|(did, dx, dy, _)| (did.as_str(), *dx, *dy));
                                let prev_info = prev_click.as_ref().map(|(pid, px, py, t)| {
                                    (
                                        pid.as_str(),
                                        *px,
                                        *py,
                                        now.duration_since(*t).as_millis() as u64,
                                    )
                                });
                                if should_toggle_double_click_on_bubble(
                                    ev.click_count,
                                    &msg_id_up,
                                    down_info,
                                    prev_info,
                                    (x, y),
                                ) {
                                    this.toggle_local_reaction(
                                        &msg_id_up,
                                        &msg_text_up,
                                        ReactionEmoji::Like,
                                        cx,
                                    );
                                }
                            }),
                        )
                        .child(body_col)
                } else {
                    v_flex()
                        .id(SharedString::from(format!("msg-bubble-{orig_idx}")))
                        .role(Role::Button)
                        .test_support()
                        .w_auto()
                        .min_w_0()
                        .max_w(px(680.0))
                        .self_start()
                        .px_4()
                        .py_3()
                        .rounded(RADIUS_BUBBLE)
                        .bg(theme::bg_surface())
                        .border_1()
                        .border_color(bubble_border)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, ev: &MouseDownEvent, _, _| {
                                let x: f32 = ev.position.x.into();
                                let y: f32 = ev.position.y.into();
                                this.chat_state.pointer_down_origin =
                                    Some((msg_id_down.clone(), x, y, Instant::now()));
                            }),
                        )
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(move |this, ev: &MouseUpEvent, _, cx| {
                                let x: f32 = ev.position.x.into();
                                let y: f32 = ev.position.y.into();
                                let now = Instant::now();
                                let down = this.chat_state.pointer_down_origin.take();
                                let prev_click = this.chat_state.last_click_record.clone();
                                this.chat_state.last_click_record =
                                    Some((msg_id_up.clone(), x, y, now));
                                let down_info = down
                                    .as_ref()
                                    .map(|(did, dx, dy, _)| (did.as_str(), *dx, *dy));
                                let prev_info = prev_click.as_ref().map(|(pid, px, py, t)| {
                                    (
                                        pid.as_str(),
                                        *px,
                                        *py,
                                        now.duration_since(*t).as_millis() as u64,
                                    )
                                });
                                if should_toggle_double_click_on_bubble(
                                    ev.click_count,
                                    &msg_id_up,
                                    down_info,
                                    prev_info,
                                    (x, y),
                                ) {
                                    this.toggle_local_reaction(
                                        &msg_id_up,
                                        &msg_text_up,
                                        ReactionEmoji::Like,
                                        cx,
                                    );
                                }
                            }),
                        )
                        .child(body_col)
                };

                let bubble = bubble.capture_any_mouse_down(cx.listener(
                    move |this, ev: &MouseDownEvent, window, cx| {
                        if ev.button != MouseButton::Right {
                            return;
                        }
                        cx.stop_propagation();
                        this.chat_state.selection_quote_popover = None;
                        this.chat_state.focused_message_idx = Some(vis_idx);
                        this.chat_state.message_action_menu_open = true;
                        this.chat_state.reaction_picker_open = false;
                        this.chat_state.message_action_index = 1;
                        this.chat_state.message_context_position =
                            Some(interaction::clamp_chat_popover(
                                ev.position,
                                this.chat_state.panel_bounds,
                                224.0,
                                232.0,
                            ));
                        this.focus_chat_panel(window, cx);
                        cx.notify();
                    },
                ));
                let mut msg_wrapper = v_flex()
                    .w_full()
                    .when(!self.settings.shell_fullscreen, |el| el.max_w(px(960.0)))
                    .mx_auto()
                    .flex_shrink_0()
                    .gap_1();
                if unread_visible_idx == Some(vis_idx) {
                    msg_wrapper = msg_wrapper.child(
                        h_flex()
                            .id("chat-unread-divider")
                            .test_support()
                            .w_full()
                            .items_center()
                            .gap_3()
                            .py_2()
                            .child(div().flex_1().h(px(1.0)).bg(theme::accent_blue()))
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .text_color(theme::accent_blue())
                                    .rounded(RADIUS_CHIP)
                                    .bg(theme::bg_surface())
                                    .border_1()
                                    .border_color(theme::accent_blue())
                                    .px_2()
                                    .py_0p5()
                                    .child("NEW"),
                            )
                            .child(div().flex_1().h(px(1.0)).bg(theme::accent_blue())),
                    );
                }
                if is_user {
                    msg_wrapper = msg_wrapper.child(h_flex().w_full().justify_end().child(bubble));
                } else {
                    msg_wrapper = msg_wrapper.child(bubble);
                }

                // Compact emoji reaction badges folded onto this target message.
                if !vis.reactions.is_empty() {
                    let mut badges = h_flex()
                        .id(SharedString::from(format!("msg-reactions-{orig_idx}")))
                        .gap_1()
                        .when(is_user, |el| el.justify_end());
                    for &(emoji, count) in &vis.reactions {
                        let label = if count > 1 {
                            format!("{} {count}", emoji.glyph())
                        } else {
                            emoji.glyph().to_string()
                        };
                        let tid = msg.id.clone();
                        let ttext = msg_visible_text.clone();
                        badges = badges.child(
                            div()
                                .id(SharedString::from(format!(
                                    "react-badge-{orig_idx}-{}",
                                    emoji.bit()
                                )))
                                .role(Role::Button)
                                .test_support()
                                .px_2()
                                .py_0p5()
                                .rounded(RADIUS_CHIP)
                                .bg(theme::bg_surface())
                                .border_1()
                                .border_color(theme::border_subtle())
                                .hover(|s| s.bg(theme::bg_hover()))
                                .cursor_pointer()
                                .text_xs()
                                .text_color(theme::text_primary())
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.toggle_local_reaction(&tid, &ttext, emoji, cx);
                                    }),
                                )
                                .child(label),
                        );
                    }
                    msg_wrapper = msg_wrapper.child(badges);
                }

                // ONLY genuine send failure renders an inline red retry strip.
                if let Some(failed) = failed_send {
                    let retry_id = failed.message_id.clone();
                    msg_wrapper = msg_wrapper.child(
                        h_flex().w_full().justify_end().child(
                            div()
                                .id(SharedString::from(format!("retry-send-{orig_idx}")))
                                .role(Role::Button)
                                .test_support()
                                .px_3()
                                .py_1()
                                .rounded(RADIUS_CHIP)
                                .bg(error_strip_bg())
                                .border_1()
                                .border_color(error_strip_border())
                                .cursor_pointer()
                                .text_xs()
                                .text_color(accent_red())
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.retry_failed_send(&retry_id, cx);
                                    }),
                                )
                                .child("Not sent · click to retry"),
                        ),
                    );
                }

                msg_col = msg_col.child(msg_wrapper);
            }
        }

        let mut live_activity = None;
        if is_working {
            let subworkers = self
                .selection
                .selected_session
                .as_ref()
                .map(|session| session.subworkers.as_slice())
                .unwrap_or_default();
            let label = activity_label(&self.history.messages);
            let label = if subworkers.is_empty() {
                label
            } else {
                let workers = format!(
                    "{} {}",
                    subworkers.len(),
                    if subworkers.len() == 1 {
                        "worker"
                    } else {
                        "workers"
                    }
                );
                if label == "Thinking" {
                    format!("{workers} running")
                } else {
                    format!("{workers} · {label}")
                }
            };
            let turn_start = self
                .history
                .messages
                .iter()
                .rposition(|message| message.role == MessageRole::User)
                .map_or(0, |index| index + 1);
            let has_activity = self.history.messages[turn_start..].iter().any(|message| {
                message
                    .blocks
                    .iter()
                    .any(|block| matches!(block, MessageBlock::ToolUse { .. }))
            });
            let expanded = self.chat_state.live_activity_expanded;
            let dot = |idx: u8| {
                div()
                    .id(format!("chat-busy-dot-{idx}"))
                    .test_support()
                    .size(px(5.0))
                    .rounded(RADIUS_CHIP)
                    .when(busy_phase == idx, |el| el.mb(px(3.0)))
                    .when(busy_phase != idx, |el| el.mt(px(3.0)))
                    .bg(if busy_phase == idx {
                        theme::text_primary()
                    } else {
                        theme::text_secondary()
                    })
            };
            let mut activity = v_flex()
                .id("chat-live-activity-container")
                .w_full()
                .when(!self.settings.shell_fullscreen, |el| el.max_w(px(960.0)))
                .mx_auto()
                .px_2()
                .gap_1p5();
            let activity_row = h_flex()
                .id("chat-live-activity")
                .tooltip(|window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new("Expand / collapse live steps")
                        .build(window, cx)
                })
                .role(Role::Button)
                .aria_label("Show current activity")
                .aria_expanded(expanded)
                .test_support()
                .cursor_pointer()
                .self_start()
                .max_w_full()
                .py_1p5()
                .gap_2()
                .items_center()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.chat_state.live_activity_expanded =
                        !this.chat_state.live_activity_expanded;
                    cx.notify();
                }))
                .child(
                    h_flex()
                        .id("chat-busy-dots")
                        .test_support()
                        .flex_shrink_0()
                        .h(px(14.0))
                        .items_center()
                        .gap_1p5()
                        .child(dot(0))
                        .child(dot(1))
                        .child(dot(2)),
                )
                .child(
                    Label::new(label)
                        .id("chat-activity-label")
                        .text_xs()
                        .text_color(theme::text_secondary())
                        .truncate(),
                )
                .when(has_activity || !subworkers.is_empty(), |el| {
                    el.child(
                        div()
                            .id("chat-live-activity-chevron")
                            .test_support()
                            .text_color(theme::text_muted())
                            .child(lucide_icon(
                                if expanded {
                                    icons::CHEVRON_DOWN
                                } else {
                                    icons::CHEVRON_RIGHT
                                },
                                12.0,
                            )),
                    )
                });
            if expanded {
                let mut steps = v_flex()
                    .id("chat-live-activity-steps")
                    .test_support()
                    .w_full()
                    .gap_1p5()
                    .max_h(px(220.0))
                    .overflow_y_scroll()
                    .rounded(RADIUS_ROW)
                    .bg(theme::bg_surface())
                    .p_2();
                if !subworkers.is_empty() {
                    let mut workers = v_flex()
                        .id("chat-live-subworkers")
                        .test_support()
                        .gap_1p5()
                        .child(
                            Label::new("Active workers")
                                .text_xs()
                                .text_color(theme::text_secondary()),
                        );
                    let now = now_unix_ms();
                    for worker in subworkers {
                        let folder = worker
                            .cwd
                            .as_deref()
                            .map(extract_basename)
                            .filter(|folder| !folder.is_empty() && *folder != worker.label);
                        let title = folder.map_or_else(
                            || worker.label.clone(),
                            |folder| format!("{} · {folder}", worker.label),
                        );
                        let details = [worker.runtime.as_deref(), worker.model.as_deref()]
                            .into_iter()
                            .flatten()
                            .collect::<Vec<_>>()
                            .join(" · ");
                        let elapsed = worker.started_at_ms.map(|started| {
                            let seconds = now.saturating_sub(started).max(0) / 1000;
                            if seconds >= 3600 {
                                format!("{}h", seconds / 3600)
                            } else if seconds >= 60 {
                                format!("{}m", seconds / 60)
                            } else {
                                format!("{seconds}s")
                            }
                        });
                        workers = workers.child(
                            h_flex()
                                .id(format!("chat-subworker-{}", worker.pid))
                                .test_support()
                                .items_center()
                                .gap_2()
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .child(Label::new(title).text_xs().truncate())
                                        .when(!details.is_empty(), |el| {
                                            el.child(
                                                Label::new(details)
                                                    .text_xs()
                                                    .text_color(theme::text_muted())
                                                    .truncate(),
                                            )
                                        }),
                                )
                                .when_some(elapsed, |el, elapsed| {
                                    el.child(
                                        div()
                                            .flex_shrink_0()
                                            .text_xs()
                                            .text_color(theme::text_muted())
                                            .child(
                                                Label::new(elapsed)
                                                    .text_xs()
                                                    .text_color(theme::text_muted()),
                                            ),
                                    )
                                }),
                        );
                    }
                    steps = steps.child(workers);
                }
                for (msg_idx, message) in self.history.messages.iter().enumerate().skip(turn_start)
                {
                    for (block_idx, block) in message.blocks.iter().enumerate() {
                        if let MessageBlock::ToolUse {
                            call_id,
                            name,
                            summary,
                            detail,
                            status,
                            diff,
                            output,
                        } = block
                        {
                            steps = steps.child(self.render_tool_step_block(
                                message,
                                msg_idx,
                                block_idx,
                                call_id,
                                name,
                                summary,
                                detail,
                                *status,
                                diff.as_ref(),
                                output.as_deref(),
                                cx,
                            ));
                        }
                    }
                }
                activity = activity.child(steps);
            }
            live_activity = Some(activity.child(activity_row).pb_2());
        }

        if !is_working
            && let Some(session) = &self.selection.selected_session
            && let Some(notice) = self
                .chat_state
                .turn_notices
                .get(&self.session_key_for(session))
        {
            msg_col = msg_col.child(
                div()
                    .id("chat-turn-notice")
                    .test_support()
                    .w_full()
                    .when(!self.settings.shell_fullscreen, |el| el.max_w(px(960.0)))
                    .mx_auto()
                    .px_2()
                    .py_1p5()
                    .text_xs()
                    .text_color(theme::text_muted())
                    .child(notice.clone()),
            );
        }

        let geometry_view = cx.entity().downgrade();
        let pointer_view = geometry_view.clone();
        let geometry = gpui_kit::canvas(
            move |bounds, window, cx| {
                let _ = geometry_view.update(cx, |this, _| {
                    this.chat_state.panel_bounds = bounds;
                    this.chat_state.interaction_window = Some(window.window_handle());
                });
            },
            move |_, _, window, _cx| {
                let view = pointer_view.clone();
                window.on_mouse_event(
                    move |event: &gpui_kit::MouseMoveEvent, phase, window, cx| {
                        if !phase.capture() {
                            return;
                        }
                        let _ = view.update(cx, |this, cx| {
                            if this.chat_state.drag_selecting
                                && (event.pressed_button != Some(MouseButton::Left)
                                    || !this.chat_state.panel_bounds.contains(&event.position))
                            {
                                this.chat_state.drag_selecting = false;
                                TextSelection::end(window, cx);
                            }
                        });
                    },
                );
                let view = pointer_view.clone();
                window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
                    if !phase.capture() || event.button != MouseButton::Left {
                        return;
                    }
                    let _ = view.update(cx, |this, cx| {
                        if !this.chat_state.drag_selecting {
                            return;
                        }
                        this.chat_state.drag_selecting = false;
                        TextSelection::end(window, cx);
                        this.show_selection_quote(event.position, window, cx);
                    });
                });
                let view = pointer_view.clone();
                window.on_mouse_event(move |_: &gpui_kit::MouseExitEvent, phase, window, cx| {
                    if !phase.capture() {
                        return;
                    }
                    let _ = view.update(cx, |this, _| this.chat_state.drag_selecting = false);
                    TextSelection::end(window, cx);
                });
            },
        )
        .absolute()
        .inset_0()
        .size_full();
        let header_view = cx.entity().downgrade();
        let panel = v_flex()
            .id("chat-panel-root")
            .when_some(self.chat_state.focus_handle.as_ref(), |el, fh| {
                el.track_focus(fh)
            })
            .flex_1()
            .h_full()
            .min_w_0();
        let panel = self.register_chat_actions(panel, cx).test_support();
        let panel = panel.child(
            v_flex()
                .relative()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .child(geometry)
                .child(msg_col)
                .child(
                    v_flex()
                        .id("chat-top-overlay")
                        .test_support()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .child(
                            gpui_kit::canvas(
                                move |bounds, _, cx| {
                                    let _ = header_view.update(cx, |this, cx| {
                                        let height = f32::from(bounds.size.height);
                                        if this.chat_state.header_height != height {
                                            this.chat_state.header_height = height;
                                            cx.notify();
                                        }
                                    });
                                },
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .inset_0()
                            .size_full(),
                        )
                        .when(self.top_floating_bar_rendered(), |el| {
                            el.child(self.render_top_floating_bar(header_title, cx))
                        })
                        .when_some(self.render_todo_header_and_drawer(cx), |el, todo_ui| {
                            el.child(todo_ui)
                        })
                        .when_some(self.render_find_bar(cx), |el, bar| el.child(bar)),
                )
                .child(
                    self.render_composer(
                        v_flex()
                            .id("chat-composer-overlay")
                            .absolute()
                            .bottom_0()
                            .left_0()
                            .right_0()
                            .when_some(live_activity, |el, activity| el.child(activity)),
                        cx,
                    )
                    .test_support(),
                )
                .when_some(self.render_selection_quote(cx), |el, pill| el.child(pill))
                .when_some(self.render_message_action_menu(cx), |el, menu| {
                    el.child(menu)
                })
                .when(
                    !self.history.messages.is_empty()
                        && (self.chat_state.scroll.scrolled_up
                            || self.chat_state.scroll.new_messages_while_scrolled_up > 0),
                    |el| {
                        let count = self.chat_state.scroll.new_messages_while_scrolled_up;
                        el.child(
                            div()
                                .id("chat-jump-to-bottom-btn")
                                .tooltip(|window, cx| {
                                    gpui_kit::component::tooltip::Tooltip::new("Scroll to bottom")
                                        .build(window, cx)
                                })
                                .role(Role::Button)
                                .test_support()
                                .absolute()
                                .bottom(px(96.0))
                                .right_6()
                                .rounded(RADIUS_CHIP)
                                .bg(theme::bg_surface())
                                .border_1()
                                .border_color(theme::border_subtle())
                                .px_3()
                                .py_2()
                                .text_sm()
                                .text_color(if count > 0 {
                                    theme::accent_blue()
                                } else {
                                    theme::text_primary()
                                })
                                .hover(|s| s.bg(theme::bg_hover()))
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| this.jump_to_newest(cx)))
                                .flex()
                                .items_center()
                                .gap_1()
                                .child(lucide_icon(icons::ARROW_DOWN, 14.0))
                                .when(count > 0, |el| el.child(format!("{count} new"))),
                        )
                    },
                ),
        );
        panel
            // Slice 5 — built-in terminal pane splits the chat column; the pane is mounted
            // only while open, so a closed pane adds no layout node and no PTY.
            .when_some(self.render_terminal_pane(cx), |el, pane| el.child(pane))
    }
}

#[cfg(test)]
#[path = "chat_view_tests.rs"]
mod tests;
