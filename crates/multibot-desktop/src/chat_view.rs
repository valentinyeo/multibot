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
    base::text::CodeBlock,
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
    px, rgb,
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
pub(crate) const MAX_TOOL_TEXT_CHARS: usize = 4000;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FailedSend {
    pub message_id: String,
    pub text: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReplyQuote {
    pub message_id: String,
    pub preview: String,
    pub full_text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ReactionEmoji {
    Like,
    Dislike,
    Heart,
}

impl ReactionEmoji {
    pub(crate) fn glyph(self) -> &'static str {
        match self {
            Self::Like => "👍",
            Self::Dislike => "👎",
            Self::Heart => "❤️",
        }
    }

    pub(crate) fn bit(self) -> u8 {
        match self {
            Self::Like => 1 << 0,
            Self::Dislike => 1 << 1,
            Self::Heart => 1 << 2,
        }
    }

    pub(crate) fn all() -> [Self; 3] {
        [Self::Like, Self::Dislike, Self::Heart]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedReaction {
    pub emoji: ReactionEmoji,
    pub quote: String,
    pub reference: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StagedAttachment {
    pub path: PathBuf,
    pub is_image: bool,
    pub size_bytes: u64,
    pub inserted_token: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttachmentCandidate {
    pub path: PathBuf,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AttachmentClassification {
    pub images: Vec<AttachmentCandidate>,
    pub files: Vec<AttachmentCandidate>,
    pub rejected_oversize: Vec<(PathBuf, String)>,
    pub rejected_too_many: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImageInsertion {
    pub text: String,
    pub caret_byte: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffLineKind {
    Added,
    Removed,
    Context,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProseOption {
    pub label: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedQuotedMessage {
    pub preview: String,
    pub source: String,
    pub body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageMenuAction {
    Reply,
    Copy,
    Quote,
    React(ReactionEmoji),
    CancelWaiting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageNavOutcome {
    Ignored,
    FocusMoved(Option<usize>),
    MenuOpened(usize),
    MenuSelectionMoved(usize),
    ExecuteAction {
        message_idx: usize,
        action: MessageMenuAction,
    },
    CancelWaiting(usize),
    DismissedQuote,
    ReturnedToComposer,
}

#[derive(Debug, Clone)]
pub(crate) struct ChatViewState {
    pub scroll_handle: ScrollHandle,
    pub scroll: ChatScrollTracker,
    pub prepend_scroll_anchor: Option<(Pixels, Point<Pixels>)>,
    pub busy_tick_phase: u8,
    pub failed_sends: HashMap<String, FailedSend>,
    pub expanded_tools: HashSet<String>,
    pub show_all_tools: HashSet<String>,
    pub tool_cluster_overrides: HashMap<(String, String), bool>,
    pub expanded_diffs: HashSet<String>,
    pub todo_drawer_open: HashMap<String, bool>,
    pub session_todos: HashMap<String, Vec<TodoItem>>,
    pub question_multi_selected: HashMap<(String, usize), u32>,
    pub answered_questions: HashMap<String, HashSet<usize>>,
    pub answered_prompts: HashSet<String>,
    pub reply_quote: Option<ReplyQuote>,
    pub focused_message_idx: Option<usize>,
    pub message_action_menu_open: bool,
    pub message_action_index: usize,
    pub local_reactions: HashMap<String, Vec<ReactionEmoji>>,
    pub local_reaction_removals: HashMap<String, Vec<ReactionEmoji>>,
    pub pointer_down_origin: Option<(String, f32, f32, Instant)>,
    pub last_click_record: Option<(String, f32, f32, Instant)>,
    pub pending_attachments: Vec<StagedAttachment>,
    pub find_open: bool,
    pub find_query: String,
    pub find_matches: Vec<usize>,
    pub find_current: usize,
    pub newest_chord: NewestChord,
    pub chord_epoch: Instant,
    pub font_size_pt: u16,
    pub slash_sync_pending: bool,
    pub composer_menu_open: bool,
    pub harness_choices: HashMap<String, Vec<multibot_proto::HarnessChoice>>,
    pub pending_harness_switch: Option<String>,
    pub composer_options: HashMap<String, SessionOptions>,
    pub draft_options: SessionOptions,
    pub pending_spawn_options: Option<SessionOptions>,
    pub queued_prompts: HashMap<String, Vec<(String, String)>>,
    pub focus_handle: Option<FocusHandle>,
}

impl Default for ChatViewState {
    fn default() -> Self {
        Self {
            scroll_handle: ScrollHandle::default(),
            scroll: ChatScrollTracker::default(),
            prepend_scroll_anchor: None,
            busy_tick_phase: 0,
            failed_sends: HashMap::new(),
            expanded_tools: HashSet::new(),
            show_all_tools: HashSet::new(),
            tool_cluster_overrides: HashMap::new(),
            expanded_diffs: HashSet::new(),
            todo_drawer_open: HashMap::new(),
            session_todos: HashMap::new(),
            question_multi_selected: HashMap::new(),
            answered_questions: HashMap::new(),
            answered_prompts: HashSet::new(),
            reply_quote: None,
            focused_message_idx: None,
            message_action_menu_open: false,
            message_action_index: 0,
            local_reactions: HashMap::new(),
            local_reaction_removals: HashMap::new(),
            pointer_down_origin: None,
            last_click_record: None,
            pending_attachments: Vec::new(),
            find_open: false,
            find_query: String::new(),
            find_matches: Vec::new(),
            find_current: 0,
            newest_chord: NewestChord::default(),
            chord_epoch: Instant::now(),
            font_size_pt: DEFAULT_CHAT_FONT_SIZE_PT,
            slash_sync_pending: false,
            composer_menu_open: false,
            harness_choices: HashMap::new(),
            pending_harness_switch: None,
            composer_options: HashMap::new(),
            draft_options: SessionOptions::default(),
            pending_spawn_options: None,
            queued_prompts: HashMap::new(),
            focus_handle: None,
        }
    }
}

impl ChatViewState {
    pub(crate) fn advance_busy_tick(&mut self) {
        self.busy_tick_phase = (self.busy_tick_phase + 1) % 3;
    }

    pub(crate) fn zoom_font(&mut self, delta: i16) -> u16 {
        let next = (self.font_size_pt as i16)
            .saturating_add(delta)
            .clamp(MIN_CHAT_FONT_SIZE_PT as i16, MAX_CHAT_FONT_SIZE_PT as i16)
            as u16;
        self.font_size_pt = next;
        next
    }

    pub(crate) fn reset_font(&mut self) -> u16 {
        self.font_size_pt = DEFAULT_CHAT_FONT_SIZE_PT;
        self.font_size_pt
    }

    pub(crate) fn open_find(&mut self, messages: &[ChatMessage], show_tools: bool) {
        self.find_open = true;
        self.recompute_find_matches(messages, show_tools);
    }

    pub(crate) fn close_find(&mut self) {
        self.find_open = false;
        self.find_query.clear();
        self.find_matches.clear();
        self.find_current = 0;
    }

    pub(crate) fn update_find_query(
        &mut self,
        query: impl Into<String>,
        messages: &[ChatMessage],
        show_tools: bool,
    ) {
        self.find_query = query.into();
        self.recompute_find_matches(messages, show_tools);
    }

    /// Recomputes find matches over projected visible messages (excluding hidden
    /// reaction messages and hidden lone assistant acks) storing visible list indices.
    fn recompute_find_matches(&mut self, messages: &[ChatMessage], show_tools: bool) {
        self.find_matches.clear();
        self.find_current = 0;
        let q = self.find_query.trim().to_lowercase();
        if q.is_empty() {
            return;
        }
        let visible = project_visible_messages_with_overrides(
            messages,
            &self.local_reactions,
            &self.local_reaction_removals,
            show_tools,
        );
        for (vis_idx, vis) in visible.iter().enumerate() {
            if message_matches_find(vis.message, &q) {
                self.find_matches.push(vis_idx);
            }
        }
    }

    /// Reconciles optimistic local reaction overrides once daemon echoes arrive.
    #[allow(dead_code)]
    pub(crate) fn reconcile_local_reactions(&mut self, messages: &[ChatMessage]) {
        let server_masks = compute_server_reaction_masks(messages);
        self.local_reactions.retain(|msg_id, emojis| {
            let mask = server_masks.get(msg_id).copied().unwrap_or(0);
            emojis.retain(|e| (mask & e.bit()) == 0);
            !emojis.is_empty()
        });
        self.local_reaction_removals.retain(|msg_id, emojis| {
            let mask = server_masks.get(msg_id).copied().unwrap_or(0);
            emojis.retain(|e| (mask & e.bit()) != 0);
            !emojis.is_empty()
        });
    }

    pub(crate) fn find_next(&mut self) -> Option<usize> {
        if self.find_matches.is_empty() {
            return None;
        }
        self.find_current = (self.find_current + 1) % self.find_matches.len();
        self.find_matches.get(self.find_current).copied()
    }

    pub(crate) fn find_prev(&mut self) -> Option<usize> {
        if self.find_matches.is_empty() {
            return None;
        }
        let len = self.find_matches.len();
        self.find_current = (self.find_current + len - 1) % len;
        self.find_matches.get(self.find_current).copied()
    }

    pub(crate) fn active_find_message_index(&self) -> Option<usize> {
        self.find_matches.get(self.find_current).copied()
    }

    pub(crate) fn menu_actions_for(
        _is_tool_only: bool,
        is_waiting: bool,
    ) -> Vec<MessageMenuAction> {
        let mut actions = vec![
            MessageMenuAction::Copy,
            MessageMenuAction::Reply,
            MessageMenuAction::Quote,
        ];
        actions.push(MessageMenuAction::React(ReactionEmoji::Like));
        actions.push(MessageMenuAction::React(ReactionEmoji::Dislike));
        actions.push(MessageMenuAction::React(ReactionEmoji::Heart));
        if is_waiting {
            actions.push(MessageMenuAction::CancelWaiting);
        }
        actions
    }

    /// Pure state machine for empty-composer keyboard message navigation (`Up`/`Down`/`Enter`/`Delete`/`Esc`).
    pub(crate) fn handle_empty_composer_nav_key(
        &mut self,
        key: &str,
        composer_empty: bool,
        visible_count: usize,
        is_tool_only: bool,
        is_waiting: bool,
    ) -> MessageNavOutcome {
        let key_l = key.to_ascii_lowercase();
        match key_l.as_str() {
            "escape" => {
                if self.message_action_menu_open {
                    self.message_action_menu_open = false;
                    self.message_action_index = 0;
                    return MessageNavOutcome::FocusMoved(self.focused_message_idx);
                }
                if self.focused_message_idx.is_some() {
                    self.focused_message_idx = None;
                    return MessageNavOutcome::ReturnedToComposer;
                }
                if self.reply_quote.is_some() {
                    self.reply_quote = None;
                    return MessageNavOutcome::DismissedQuote;
                }
                MessageNavOutcome::Ignored
            }
            "up" => {
                if self.message_action_menu_open {
                    let actions = Self::menu_actions_for(is_tool_only, is_waiting);
                    if actions.is_empty() {
                        return MessageNavOutcome::Ignored;
                    }
                    let len = actions.len();
                    self.message_action_index = (self.message_action_index + len - 1) % len;
                    return MessageNavOutcome::MenuSelectionMoved(self.message_action_index);
                }
                if visible_count == 0 {
                    return MessageNavOutcome::Ignored;
                }
                match self.focused_message_idx {
                    None => {
                        if !composer_empty {
                            return MessageNavOutcome::Ignored;
                        }
                        self.focused_message_idx = Some(visible_count - 1);
                        MessageNavOutcome::FocusMoved(self.focused_message_idx)
                    }
                    Some(idx) => {
                        let next = idx.saturating_sub(1);
                        self.focused_message_idx = Some(next);
                        MessageNavOutcome::FocusMoved(self.focused_message_idx)
                    }
                }
            }
            "down" => {
                if self.message_action_menu_open {
                    let actions = Self::menu_actions_for(is_tool_only, is_waiting);
                    if actions.is_empty() {
                        return MessageNavOutcome::Ignored;
                    }
                    self.message_action_index = (self.message_action_index + 1) % actions.len();
                    return MessageNavOutcome::MenuSelectionMoved(self.message_action_index);
                }
                let Some(idx) = self.focused_message_idx else {
                    return MessageNavOutcome::Ignored;
                };
                if idx + 1 >= visible_count {
                    self.focused_message_idx = None;
                    MessageNavOutcome::ReturnedToComposer
                } else {
                    self.focused_message_idx = Some(idx + 1);
                    MessageNavOutcome::FocusMoved(self.focused_message_idx)
                }
            }
            "enter" => {
                let Some(idx) = self.focused_message_idx else {
                    return MessageNavOutcome::Ignored;
                };
                if !self.message_action_menu_open {
                    self.message_action_menu_open = true;
                    self.message_action_index = 0;
                    return MessageNavOutcome::MenuOpened(idx);
                }
                let actions = Self::menu_actions_for(is_tool_only, is_waiting);
                let action = actions
                    .get(self.message_action_index)
                    .copied()
                    .unwrap_or(MessageMenuAction::Copy);
                self.message_action_menu_open = false;
                self.message_action_index = 0;
                MessageNavOutcome::ExecuteAction {
                    message_idx: idx,
                    action,
                }
            }
            "delete" => {
                let Some(idx) = self.focused_message_idx else {
                    return MessageNavOutcome::Ignored;
                };
                if is_waiting {
                    self.message_action_menu_open = false;
                    MessageNavOutcome::CancelWaiting(idx)
                } else {
                    MessageNavOutcome::Ignored
                }
            }
            _ => {
                if self.focused_message_idx.is_some()
                    && (key.chars().count() == 1 || key == "space")
                {
                    self.focused_message_idx = None;
                    self.message_action_menu_open = false;
                    self.message_action_index = 0;
                    return MessageNavOutcome::ReturnedToComposer;
                }
                MessageNavOutcome::Ignored
            }
        }
    }
}

/// Unified visible text extractor: returns `msg.text` when non-empty, otherwise
/// joins non-empty `MessageBlock::Text` blocks so structured-block messages support
/// reactions, quotes, reply, and copy identically to plain-text messages.
pub(crate) fn message_visible_text(msg: &ChatMessage) -> String {
    if !msg.text.trim().is_empty() {
        return msg.text.clone();
    }
    let mut parts = Vec::new();
    for block in &msg.blocks {
        if let MessageBlock::Text { text } = block
            && !text.trim().is_empty()
        {
            parts.push(text.as_str());
        }
    }
    if parts.is_empty() {
        msg.blocks
            .iter()
            .filter_map(|block| match block {
                MessageBlock::ToolUse {
                    name,
                    summary,
                    detail,
                    ..
                } => Some(format!("{name}\n{summary}\n{detail}")),
                MessageBlock::Image { alt, .. } => Some(alt.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        parts.join("\n")
    }
}

/// Finds the projected **visible** list index for a clicked quote reference.
pub(crate) fn find_visible_quote_target(
    messages: &[ChatMessage],
    local_reactions: &HashMap<String, Vec<ReactionEmoji>>,
    local_removals: &HashMap<String, Vec<ReactionEmoji>>,
    quoted_id: Option<&str>,
    quoted_source: &str,
    show_tools: bool,
) -> Option<usize> {
    let visible = project_visible_messages_with_overrides(
        messages,
        local_reactions,
        local_removals,
        show_tools,
    );
    quoted_id
        .and_then(|qid| visible.iter().position(|v| v.message.id == qid))
        .or_else(|| {
            visible.iter().position(|v| {
                let text = message_visible_text(v.message);
                quote_source_matches(&text, quoted_source)
            })
        })
}

fn message_matches_find(msg: &ChatMessage, needle_lower: &str) -> bool {
    if msg.text.to_lowercase().contains(needle_lower) {
        return true;
    }
    for block in &msg.blocks {
        match block {
            MessageBlock::Text { text } => {
                if text.to_lowercase().contains(needle_lower) {
                    return true;
                }
            }
            MessageBlock::ToolUse {
                name,
                summary,
                detail,
                output,
                ..
            } => {
                if name.to_lowercase().contains(needle_lower)
                    || summary.to_lowercase().contains(needle_lower)
                    || detail.to_lowercase().contains(needle_lower)
                    || output
                        .as_deref()
                        .is_some_and(|o| o.to_lowercase().contains(needle_lower))
                {
                    return true;
                }
            }
            MessageBlock::Image { alt, url, .. } => {
                if alt.to_lowercase().contains(needle_lower)
                    || url
                        .as_deref()
                        .is_some_and(|u| u.to_lowercase().contains(needle_lower))
                {
                    return true;
                }
            }
            MessageBlock::QuestionPrompt { questions, .. } => {
                if questions.iter().any(|q| {
                    q.question.to_lowercase().contains(needle_lower)
                        || q.options
                            .iter()
                            .any(|o| o.label.to_lowercase().contains(needle_lower))
                }) {
                    return true;
                }
            }
        }
    }
    false
}

/// FNV-1a 64-bit hash used for reaction message references (`[mp:<16-hex>]`)
/// and deterministic code-block button IDs.
pub(crate) fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

pub(crate) fn reaction_reference(id: &str) -> u64 {
    if id.is_empty() {
        return 0;
    }
    fnv1a_64(id.as_bytes())
}

// ============================================================================
// 1. Markdown Code Block Extraction & Ctrl+Shift+D Guard
// ============================================================================

fn detect_code_fence(line: &str) -> Option<(char, usize, &str)> {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let leading_spaces = line.bytes().take_while(|&b| b == b' ').count();
    if leading_spaces > 3 {
        return None;
    }
    let rest = &line[leading_spaces..];
    let first = rest.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let count = rest.chars().take_while(|&c| c == first).count();
    if count < 3 {
        return None;
    }
    let info = &rest[count..];
    if first == '`' && info.contains('`') {
        return None;
    }
    Some((first, count, info))
}

/// Extracts all fenced code blocks (`` ``` `` and `~~~`) from Markdown, stripping
/// fence lines while preserving inner blank lines and indentation verbatim.
pub(crate) fn extract_code_blocks_from_markdown(md: &str) -> Vec<(Option<String>, String)> {
    let normalized = md.replace("\r\n", "\n");
    let mut blocks = Vec::new();
    let mut active: Option<(char, usize, Option<String>, Vec<&str>)> = None;

    for line in normalized.split('\n') {
        if let Some((fch, flen, ref mut lang, ref mut lines)) = active {
            if let Some((ch, count, info)) = detect_code_fence(line)
                && ch == fch
                && count >= flen
                && info.trim().is_empty()
            {
                let finished_lang = lang.take();
                let code = lines.join("\n");
                blocks.push((finished_lang, code));
                active = None;
                continue;
            }
            lines.push(line);
        } else if let Some((ch, count, info)) = detect_code_fence(line) {
            let lang = info
                .split_whitespace()
                .next()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            active = Some((ch, count, lang, Vec::new()));
        }
    }

    // Unclosed trailing fence (streaming safety)
    if let Some((_, _, lang, lines)) = active {
        blocks.push((lang, lines.join("\n")));
    }

    blocks
}

/// Returns the latest non-empty fenced code block across `messages` without fences,
/// or `None` if no non-empty code block exists.
pub(crate) fn extract_latest_code_block(messages: &[ChatMessage]) -> Option<String> {
    for msg in messages.iter().rev() {
        if !msg.blocks.is_empty() {
            for block in msg.blocks.iter().rev() {
                if let MessageBlock::Text { text } = block {
                    for (_, code) in extract_code_blocks_from_markdown(text).into_iter().rev() {
                        if !code.trim().is_empty() {
                            return Some(code);
                        }
                    }
                }
            }
        }
        for (_, code) in extract_code_blocks_from_markdown(&msg.text)
            .into_iter()
            .rev()
        {
            if !code.trim().is_empty() {
                return Some(code);
            }
        }
    }
    None
}

/// Strictly validates `Ctrl+Shift+D` while rejecting `Ctrl+Alt` (AltGr) and `Platform`.
pub(crate) fn is_copy_latest_code_block_modifiers(
    ctrl: bool,
    shift: bool,
    alt: bool,
    platform: bool,
    key: &str,
) -> bool {
    ctrl && shift && !alt && !platform && key.eq_ignore_ascii_case("d")
}

pub(crate) fn is_copy_latest_code_block_chord(keystroke: &Keystroke) -> bool {
    is_copy_latest_code_block_modifiers(
        keystroke.modifiers.control,
        keystroke.modifiers.shift,
        keystroke.modifiers.alt,
        keystroke.modifiers.platform,
        &keystroke.key,
    )
}

// ============================================================================
// 2. Collapsible Tool Steps, Inline Unified Diff & Todos
// ============================================================================

fn tool_cluster_key(msg: &ChatMessage, msg_idx: usize) -> String {
    if msg.id.is_empty() {
        format!("message-{msg_idx}")
    } else {
        msg.id.clone()
    }
}

fn tool_step_key(msg: &ChatMessage, msg_idx: usize, block_idx: usize, call_id: &str) -> String {
    if !call_id.is_empty() {
        call_id.to_string()
    } else {
        format!("{}:{}:{}", msg.id, msg_idx, block_idx)
    }
}

pub(crate) fn classify_diff_line(line: &str) -> DiffLineKind {
    if line.starts_with('+') && !line.starts_with("+++") {
        DiffLineKind::Added
    } else if line.starts_with('-') && !line.starts_with("---") {
        DiffLineKind::Removed
    } else {
        DiffLineKind::Context
    }
}

pub(crate) fn visible_diff_slice(diff: &DiffBlock, expanded: bool) -> (&[String], usize) {
    let limit = if expanded {
        DIFF_EXPANDED_ROWS
    } else {
        DIFF_FOLDED_ROWS
    };
    let shown_len = diff.lines.len().min(limit);
    let remaining = diff.lines.len().saturating_sub(shown_len) + diff.truncated_lines as usize;
    (&diff.lines[..shown_len], remaining)
}

fn truncate_tool_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    out
}

pub(crate) fn effective_todos(
    messages: &[ChatMessage],
    session: Option<&SessionSummary>,
    cached: Option<&[TodoItem]>,
) -> (Vec<TodoItem>, u32, u32) {
    if let Some(msg) = messages.iter().rev().find(|m| !m.todos.is_empty()) {
        let done = msg
            .todos
            .iter()
            .filter(|t| t.status == TodoStatus::Completed)
            .count() as u32;
        let total = msg.todos.len() as u32;
        return (msg.todos.clone(), done, total);
    }
    if let Some(items) = cached
        && !items.is_empty()
    {
        let done = items
            .iter()
            .filter(|t| t.status == TodoStatus::Completed)
            .count() as u32;
        return (items.to_vec(), done, items.len() as u32);
    }
    if let Some(s) = session {
        return (Vec::new(), s.todos_done, s.todos_total);
    }
    (Vec::new(), 0, 0)
}

// ============================================================================
// 3. Interactive Option Cards (Structured Primary + Strictly Gated Fallback)
// ============================================================================

pub(crate) fn validate_question_prompt<'a>(
    prompt_id: &str,
    questions: &'a [PromptQuestion],
) -> Option<Vec<&'a PromptQuestion>> {
    if prompt_id.trim().is_empty() || questions.is_empty() || questions.len() > MAX_PROMPT_QUESTIONS
    {
        return None;
    }
    let mut valid = Vec::with_capacity(questions.len());
    for q in questions {
        if q.question.trim().is_empty()
            || q.options.is_empty()
            || q.options.len() > MAX_PROMPT_OPTIONS
            || q.options.iter().any(|o| o.label.trim().is_empty())
        {
            return None;
        }
        valid.push(q);
    }
    Some(valid)
}

pub(crate) fn format_single_option_reply(index: usize, label: &str) -> String {
    format!("{}: {}", index + 1, label.trim())
}

pub(crate) fn format_multi_option_reply(q: &PromptQuestion, mask: u32) -> Option<String> {
    let mut chosen = Vec::new();
    for (i, opt) in q.options.iter().enumerate() {
        if i < 32 && (mask & (1u32 << i)) != 0 {
            chosen.push(format!("{}: {}", i + 1, opt.label.trim()));
        }
    }
    if chosen.is_empty() {
        None
    } else {
        Some(chosen.join(", "))
    }
}

fn parse_prose_option_line(line: &str) -> Option<(char, String)> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Strip optional bold wrapper around marker, e.g. `**A)** Title` or `**1.** Title`
    let (marker_src, rest_after_marker) = if let Some(after_stars) = trimmed.strip_prefix("**") {
        let close = after_stars.find("**")?;
        let inside = &after_stars[..close];
        let tail = after_stars[close + 2..].trim_start();
        (inside, tail)
    } else {
        let split_at = trimmed.find(|c: char| c.is_ascii_whitespace())?;
        (&trimmed[..split_at], trimmed[split_at..].trim_start())
    };

    let marker = marker_src.trim();
    if marker.len() != 2 {
        return None;
    }
    let mut chars = marker.chars();
    let key = chars.next()?;
    let punct = chars.next()?;
    if punct != ')' && punct != '.' {
        return None;
    }
    let title = rest_after_marker.trim();
    if title.is_empty() || title.chars().count() > 200 {
        return None;
    }
    if key.is_ascii_uppercase() || key.is_ascii_digit() {
        Some((key, title.to_string()))
    } else {
        None
    }
}

fn has_numbered_choice_cue(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.ends_with('?') || trimmed.ends_with(':') {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    lower.contains("choose")
        || lower.contains("select")
        || lower.contains("which option")
        || lower.contains("prefer")
        || lower.contains("would you like")
}

/// Strictly gated trailing prose option detector:
/// - ONLY inspects the **latest** message and ONLY if `role == MessageRole::Assistant`.
/// - Yields (`None`) when a valid structured `MessageBlock::QuestionPrompt` is present.
/// - Strictly ignores fenced code blocks (`` ``` `` / `~~~`).
/// - Requires sequential `A..D` or `1..4` (count `2..=4`), rejecting 5+ item lists.
pub(crate) fn detect_trailing_prose_options(messages: &[ChatMessage]) -> Option<Vec<ProseOption>> {
    let (hidden, _) = compute_hidden_and_reaction_masks(messages);
    let last_idx = (0..messages.len()).rev().find(|&i| !hidden[i])?;
    let last = &messages[last_idx];
    if last.role != MessageRole::Assistant {
        return None;
    }
    for block in &last.blocks {
        if let MessageBlock::QuestionPrompt {
            prompt_id,
            questions,
            ..
        } = block
            && validate_question_prompt(prompt_id, questions).is_some()
        {
            return None;
        }
    }

    let combined_text = if last
        .blocks
        .iter()
        .any(|b| matches!(b, MessageBlock::Text { .. }))
    {
        last.blocks
            .iter()
            .filter_map(|b| match b {
                MessageBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        last.text.clone()
    };

    let normalized = combined_text.replace("\r\n", "\n");
    let mut tagged: Vec<(&str, bool)> = Vec::new();
    let mut fence: Option<(char, usize)> = None;

    for line in normalized.split('\n') {
        if let Some((fch, flen)) = fence {
            tagged.push((line, true));
            if let Some((ch, count, info)) = detect_code_fence(line)
                && ch == fch
                && count >= flen
                && info.trim().is_empty()
            {
                fence = None;
            }
        } else if let Some((ch, count, _)) = detect_code_fence(line) {
            tagged.push((line, true));
            fence = Some((ch, count));
        } else {
            tagged.push((line, false));
        }
    }

    while tagged
        .last()
        .is_some_and(|(l, in_f)| !*in_f && l.trim().is_empty())
    {
        tagged.pop();
    }
    let (_, last_in_fence) = *tagged.last()?;
    if last_in_fence {
        return None;
    }

    let mut matched_rev: Vec<(char, String)> = Vec::new();
    let mut idx = tagged.len();
    while idx > 0 {
        let (line, in_f) = tagged[idx - 1];
        if in_f {
            break;
        }
        if let Some(parsed) = parse_prose_option_line(line) {
            matched_rev.push(parsed);
            idx -= 1;
        } else {
            break;
        }
    }

    if matched_rev.len() < 2 || matched_rev.len() > MAX_PROMPT_OPTIONS {
        return None;
    }
    matched_rev.reverse();

    let first_key = matched_rev[0].0;
    let is_lettered = first_key == 'A';
    let is_numbered = first_key == '1';
    if !is_lettered && !is_numbered {
        return None;
    }

    for (pos, (key, _)) in matched_rev.iter().enumerate() {
        let expected = ((first_key as u8) + (pos as u8)) as char;
        if *key != expected {
            return None;
        }
    }

    // For numbered lists, require a clear question/choice cue immediately above the list.
    if is_numbered {
        let mut cue_idx = idx;
        while cue_idx > 0 && !tagged[cue_idx - 1].1 && tagged[cue_idx - 1].0.trim().is_empty() {
            cue_idx -= 1;
        }
        if cue_idx == 0 || tagged[cue_idx - 1].1 || !has_numbered_choice_cue(tagged[cue_idx - 1].0)
        {
            return None;
        }
    }

    Some(
        matched_rev
            .into_iter()
            .map(|(k, title)| ProseOption {
                label: k.to_string(),
                title,
            })
            .collect(),
    )
}

// ============================================================================
// 4. Quote/Reply (MUBO-457) & Compact Quote Header Parsing
// ============================================================================

fn is_unicode_ws(ch: char) -> bool {
    matches!(
        ch as u32,
        0x09..=0x0D
            | 0x1C..=0x20
            | 0xA0
            | 0x1680
            | 0x2000..=0x200A
            | 0x2028
            | 0x2029
            | 0x202F
            | 0x205F
            | 0x3000
    )
}

fn trim_end_unicode_ws(s: &str) -> &str {
    s.trim_end_matches(is_unicode_ws)
}

fn trim_unicode_ws(s: &str) -> &str {
    s.trim_matches(is_unicode_ws)
}

fn split_any_newlines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\r' {
            lines.push(&text[start..i]);
            if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                i += 2;
            } else {
                i += 1;
            }
            start = i;
        } else if bytes[i] == b'\n' {
            lines.push(&text[start..i]);
            i += 1;
            start = i;
        } else {
            i += 1;
        }
    }
    lines.push(&text[start..]);
    lines
}

/// Formats a quoted block capped at 10 lines (`> …` on overflow), stripping
/// leading/trailing blank lines and preserving inner indentation.
pub(crate) fn format_quote_block(message: &str) -> String {
    let raw_lines = split_any_newlines(message);
    let Some(first) = raw_lines
        .iter()
        .position(|l| !trim_end_unicode_ws(l).is_empty())
    else {
        return String::new();
    };
    let last = raw_lines
        .iter()
        .rposition(|l| !trim_end_unicode_ws(l).is_empty())
        .unwrap_or(first);

    let mut out_lines = Vec::new();
    for (emitted, line) in raw_lines[first..=last].iter().enumerate() {
        if emitted == MAX_QUOTE_LINES {
            out_lines.push("> …".to_string());
            break;
        }
        let trimmed = trim_end_unicode_ws(line);
        if trimmed.is_empty() {
            out_lines.push(">".to_string());
        } else {
            out_lines.push(format!("> {trimmed}"));
        }
    }
    out_lines.join("\n")
}

/// Detects a leading `/skill` command (MUBO-457) whose name consists exclusively of
/// `[A-Za-z0-9:_-]`, automatically rejecting file paths like `/tmp/foo` or `/usr/bin/env`.
pub(crate) fn extract_leading_skill_command(message: &str) -> Option<(&str, &str)> {
    let bytes = message.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'/' {
        return None;
    }
    let mut name_end = 1usize;
    while name_end < bytes.len() && !matches!(bytes[name_end], b' ' | b'\t' | b'\r' | b'\n') {
        name_end += 1;
    }
    if name_end == 1 {
        return None;
    }
    if !bytes[1..name_end]
        .iter()
        .all(|&b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b':' | b'_' | b'-'))
    {
        return None;
    }

    let first_line_end = message.find(['\r', '\n']).unwrap_or(message.len());
    let command = trim_end_unicode_ws(&message[..first_line_end]);
    let mut rest_start = first_line_end;
    while rest_start < bytes.len() && matches!(bytes[rest_start], b'\r' | b'\n') {
        rest_start += 1;
    }
    Some((command, &message[rest_start..]))
}

/// Composes a reply with an optional quoted message, hoisting leading `/skill` commands
/// above the quote block (MUBO-457) while keeping ordinary replies and file paths below.
pub(crate) fn compose_quoted_reply(quoted: Option<&str>, reply: &str) -> String {
    let Some(q) = quoted.filter(|s| !trim_end_unicode_ws(s).is_empty()) else {
        return reply.to_string();
    };
    if trim_end_unicode_ws(reply).is_empty() {
        return String::new();
    }
    let quote_block = format_quote_block(q);
    if quote_block.is_empty() {
        return reply.to_string();
    }

    if let Some((cmd, rest)) = extract_leading_skill_command(reply) {
        if trim_end_unicode_ws(rest).is_empty() {
            format!("{cmd}\n{quote_block}")
        } else {
            format!("{cmd}\n{quote_block}\n\n{rest}")
        }
    } else {
        format!("{quote_block}\n\n{reply}")
    }
}

/// Parses a message starting with leading `>` quote lines + blank line + non-empty body
/// into a compact 1-line preview, matchable source text, and body.
pub(crate) fn parse_quoted_message(text: &str) -> Option<ParsedQuotedMessage> {
    let normalized = text.replace("\r\n", "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    let mut idx = 0usize;
    let mut quote_lines: Vec<&str> = Vec::new();

    while idx < lines.len() {
        let line = lines[idx];
        if let Some(after) = line.strip_prefix('>') {
            let content = after.strip_prefix(' ').unwrap_or(after);
            quote_lines.push(content);
            idx += 1;
        } else {
            break;
        }
    }

    if quote_lines.is_empty() {
        return None;
    }

    // Must have at least one blank separator line before body.
    let mut blank_seen = false;
    while idx < lines.len() && trim_unicode_ws(lines[idx]).is_empty() {
        blank_seen = true;
        idx += 1;
    }
    if !blank_seen || idx >= lines.len() {
        return None;
    }

    let body = lines[idx..].join("\n");
    if trim_unicode_ws(&body).is_empty() {
        return None;
    }

    let preview = quote_lines
        .iter()
        .map(|l| trim_unicode_ws(l))
        .find(|l| !l.is_empty() && *l != "…")?
        .to_string();

    if quote_lines
        .last()
        .is_some_and(|l| trim_unicode_ws(l) == "…")
    {
        quote_lines.pop();
    }
    let source = quote_lines.join("\n");

    Some(ParsedQuotedMessage {
        preview,
        source,
        body,
    })
}

/// Strips a leading quoted `> ...` prefix from a rendered `MessageBlock::Text`
/// when a compact clickable quote header is already rendered above the bubble body.
pub(crate) fn strip_rendered_quote_prefix(text: &str) -> String {
    if let Some(pq) = parse_quoted_message(text) {
        return pq.body;
    }
    let normalized = text.replace("\r\n", "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    let mut idx = 0usize;
    while idx < lines.len() && lines[idx].starts_with('>') {
        idx += 1;
    }
    if idx == 0 {
        return text.to_string();
    }
    while idx < lines.len() && trim_unicode_ws(lines[idx]).is_empty() {
        idx += 1;
    }
    lines[idx..].join("\n")
}

fn normalize_ws_collapse(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for ch in s.chars() {
        if is_unicode_ws(ch) {
            if !out.is_empty() {
                in_ws = true;
            }
        } else {
            if in_ws {
                out.push(' ');
                in_ws = false;
            }
            out.push(ch);
        }
    }
    out
}

pub(crate) fn quote_source_matches(candidate: &str, quoted_source: &str) -> bool {
    let q = normalize_ws_collapse(quoted_source.trim_end_matches('…'));
    if q.is_empty() {
        return false;
    }
    let c = normalize_ws_collapse(candidate);
    c.starts_with(&q) || c.contains(&q)
}

// ============================================================================
// 5. Message Reactions & Double-Click Drag-Selection Guard
// ============================================================================

fn truncate_reaction_quote(raw: &str) -> Option<String> {
    let normalized = normalize_ws_collapse(raw);
    if normalized.is_empty() {
        return None;
    }
    let total_utf16: usize = normalized.chars().map(char::len_utf16).sum();
    if total_utf16 <= REACTION_QUOTE_MAX_UTF16 - 1 {
        return Some(normalized);
    }

    let room = REACTION_QUOTE_MAX_UTF16 - 1; // 79 UTF-16 units + '…' = 80
    let mut units = 0usize;
    let mut last_fit_byte = 0usize;
    let mut last_space_byte = 0usize;
    let mut overflowed = false;

    for (byte_idx, ch) in normalized.char_indices() {
        let u = ch.len_utf16();
        if units + u > room {
            overflowed = true;
            break;
        }
        if ch == ' ' {
            last_space_byte = byte_idx;
        }
        units += u;
        last_fit_byte = byte_idx + ch.len_utf8();
    }

    if !overflowed {
        return Some(normalized);
    }

    let cut = if last_space_byte > 0 {
        last_space_byte
    } else {
        last_fit_byte
    };
    let mut out = normalized[..cut].trim_end_matches(' ').to_string();
    out.push('…');
    Some(out)
}

pub(crate) fn format_reaction_message(
    emoji: ReactionEmoji,
    target_id: &str,
    target_text: &str,
) -> Option<String> {
    let quote = truncate_reaction_quote(target_text)?;
    let reference = reaction_reference(target_id);
    if reference == 0 {
        Some(format!("{} to: {}", emoji.glyph(), quote))
    } else {
        Some(format!(
            "{} to: {} [mp:{:016x}]",
            emoji.glyph(),
            quote,
            reference
        ))
    }
}

fn is_clean_reaction_quote(q: &str) -> bool {
    if q.is_empty() || q.starts_with(' ') || q.ends_with(' ') || q.contains("  ") {
        return false;
    }
    let mut utf16 = 0usize;
    for ch in q.chars() {
        if ch != ' ' && is_unicode_ws(ch) {
            return false;
        }
        utf16 += ch.len_utf16();
        if utf16 > REACTION_QUOTE_MAX_UTF16 {
            return false;
        }
    }
    true
}

/// Parses both tagged `<emoji> to: <quote> [mp:<16-lowercase-hex>]` and
/// spoken single-line `<emoji> to: <quote>` reaction messages.
/// Explicit `[mp:...]` tags must parse to a non-zero `u64` (malformed tags or
/// `[mp:0000000000000000]` are strictly rejected and never downgraded to spoken).
pub(crate) fn parse_reaction_message(text: &str) -> Option<ParsedReaction> {
    if text.contains('\n') || text.contains('\r') {
        return None;
    }
    let (emoji, after_emoji) = if let Some(rest) = text.strip_prefix("👍") {
        (ReactionEmoji::Like, rest)
    } else if let Some(rest) = text.strip_prefix("👎") {
        (ReactionEmoji::Dislike, rest)
    } else if let Some(rest) = text.strip_prefix("❤️") {
        (ReactionEmoji::Heart, rest)
    } else {
        return None;
    };

    let body = after_emoji.strip_prefix(" to: ")?;
    if let Some(tag_pos) = body.rfind(" [mp:") {
        let quote = &body[..tag_pos];
        let tag_rest = &body[tag_pos + 5..];
        let hex = tag_rest.strip_suffix(']')?;
        if hex.len() != 16 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return None;
        }
        if !is_clean_reaction_quote(quote) {
            return None;
        }
        let reference = u64::from_str_radix(hex, 16).ok()?;
        if reference == 0 {
            return None;
        }
        Some(ParsedReaction {
            emoji,
            quote: quote.to_string(),
            reference: Some(reference),
        })
    } else {
        if !is_clean_reaction_quote(body) {
            return None;
        }
        Some(ParsedReaction {
            emoji,
            quote: body.to_string(),
            reference: None,
        })
    }
}

fn normalized_reaction_prefix_matches(collapsed: &str, quote: &str, needle: &str) -> bool {
    if !collapsed.starts_with(needle) {
        return false;
    }
    if quote.ends_with('…') || collapsed.len() == needle.len() {
        return true;
    }
    collapsed[needle.len()..]
        .chars()
        .next()
        .is_some_and(|ch| !ch.is_alphanumeric())
}

fn compute_hidden_and_reaction_masks(messages: &[ChatMessage]) -> (Vec<bool>, Vec<u8>) {
    let mut hidden = vec![false; messages.len()];
    let mut masks = vec![0u8; messages.len()];

    for (idx, msg) in messages.iter().enumerate() {
        if msg.role != MessageRole::User {
            continue;
        }
        let user_text = message_visible_text(msg);
        let Some(parsed) = parse_reaction_message(user_text.trim()) else {
            continue;
        };
        let needle = parsed.quote.strip_suffix('…').unwrap_or(&parsed.quote);
        if needle.is_empty() {
            continue;
        }

        let target_idx = if let Some(reference) = parsed.reference {
            (0..idx).rev().find(|&ti| {
                !hidden[ti]
                    && reaction_reference(&messages[ti].id) == reference
                    && normalized_reaction_prefix_matches(
                        &normalize_ws_collapse(&message_visible_text(&messages[ti])),
                        &parsed.quote,
                        needle,
                    )
            })
        } else {
            // Untagged spoken reactions match newest-first against normalized prefix only.
            (0..idx).rev().find(|&ti| {
                !hidden[ti]
                    && messages[ti].role == MessageRole::Assistant
                    && normalized_reaction_prefix_matches(
                        &normalize_ws_collapse(&message_visible_text(&messages[ti])),
                        &parsed.quote,
                        needle,
                    )
            })
        };

        if let Some(ti) = target_idx {
            hidden[idx] = true;
            masks[ti] ^= parsed.emoji.bit();

            // Hide a lone single-message plain-text assistant ack immediately following a hidden reaction.
            if idx + 1 < messages.len() && messages[idx + 1].role == MessageRole::Assistant {
                let next_is_also_assistant =
                    idx + 2 < messages.len() && messages[idx + 2].role == MessageRole::Assistant;
                if !next_is_also_assistant
                    && messages[idx + 1]
                        .blocks
                        .iter()
                        .all(|b| matches!(b, MessageBlock::Text { .. }))
                {
                    hidden[idx + 1] = true;
                }
            }
        }
    }

    (hidden, masks)
}

fn compute_server_reaction_masks(messages: &[ChatMessage]) -> HashMap<String, u8> {
    let (hidden, masks) = compute_hidden_and_reaction_masks(messages);
    let mut by_id = HashMap::new();
    for (idx, msg) in messages.iter().enumerate() {
        if !hidden[idx] && masks[idx] != 0 {
            by_id.insert(msg.id.clone(), masks[idx]);
        }
    }
    by_id
}

#[derive(Debug)]
pub(crate) struct VisibleMessage<'a> {
    pub original_index: usize,
    pub message: &'a ChatMessage,
    pub reactions: Vec<(ReactionEmoji, usize)>,
}

/// Hides reaction user messages (and any lone assistant ack immediately following a
/// hidden reaction-only user message) and folds reactions onto their target messages.
/// Optimistic `local_reactions` are idempotent with daemon-echoed reaction messages
/// (they ensure the reaction count is at least 1 and never cancel out a server echo).
#[allow(dead_code)]
pub(crate) fn project_visible_messages<'a>(
    messages: &'a [ChatMessage],
    local_reactions: &HashMap<String, Vec<ReactionEmoji>>,
) -> Vec<VisibleMessage<'a>> {
    let empty_removals = HashMap::new();
    project_visible_messages_with_overrides(messages, local_reactions, &empty_removals, true)
}

pub(crate) fn project_visible_messages_with_overrides<'a>(
    messages: &'a [ChatMessage],
    local_reactions: &HashMap<String, Vec<ReactionEmoji>>,
    local_removals: &HashMap<String, Vec<ReactionEmoji>>,
    show_tools: bool,
) -> Vec<VisibleMessage<'a>> {
    let (hidden, masks) = compute_hidden_and_reaction_masks(messages);

    let mut visible = Vec::with_capacity(messages.len());
    for (idx, msg) in messages.iter().enumerate() {
        if hidden[idx]
            || (!show_tools
                && msg.role == MessageRole::Assistant
                && msg.text.trim().is_empty()
                && !msg.blocks.is_empty()
                && msg.blocks.iter().all(|block| match block {
                    MessageBlock::ToolUse { .. } => true,
                    MessageBlock::Text { text } => text.trim().is_empty(),
                    _ => false,
                }))
        {
            continue;
        }
        let mut counts: [usize; 3] = [0; 3];
        for (slot, emoji) in ReactionEmoji::all().into_iter().enumerate() {
            if (masks[idx] & emoji.bit()) != 0 {
                counts[slot] += 1;
            }
        }
        if let Some(local) = local_reactions.get(&msg.id) {
            for &emoji in local {
                let slot = match emoji {
                    ReactionEmoji::Like => 0,
                    ReactionEmoji::Dislike => 1,
                    ReactionEmoji::Heart => 2,
                };
                if counts[slot] == 0 {
                    counts[slot] = 1;
                }
            }
        }
        if let Some(removed) = local_removals.get(&msg.id) {
            for &emoji in removed {
                let slot = match emoji {
                    ReactionEmoji::Like => 0,
                    ReactionEmoji::Dislike => 1,
                    ReactionEmoji::Heart => 2,
                };
                counts[slot] = 0;
            }
        }
        let reactions = ReactionEmoji::all()
            .into_iter()
            .enumerate()
            .filter_map(|(slot, emoji)| (counts[slot] > 0).then_some((emoji, counts[slot])))
            .collect();

        visible.push(VisibleMessage {
            original_index: idx,
            message: msg,
            reactions,
        });
    }
    visible
}

/// Returns `true` iff a strict double-click (`click_count == 2`, never triple-click)
/// occurred within the drag-selection threshold (`|dx| <= 4px && |dy| <= 4px` within `500ms`),
/// preventing accidental `👍` on drag-select or paragraph triple-click.
pub(crate) fn should_toggle_double_click_like(
    click_count: usize,
    down_pos: (f32, f32),
    up_pos: (f32, f32),
    elapsed_ms: u64,
) -> bool {
    click_count == 2
        && (up_pos.0 - down_pos.0).abs() <= DOUBLE_CLICK_DRAG_THRESHOLD_PX
        && (up_pos.1 - down_pos.1).abs() <= DOUBLE_CLICK_DRAG_THRESHOLD_PX
        && elapsed_ms <= DOUBLE_CLICK_WINDOW_MS
}

/// Validates full double-click state for a specific message bubble:
/// requires same-bubble mousedown, same-bubble previous click (rejecting `None` or
/// cross-bubble clicks with `u64::MAX`), click-to-click pointer proximity `<= 4px`,
/// mousedown->mouseup drag `<= 4px`, `<= 500ms`, and `click_count == 2`.
pub(crate) fn should_toggle_double_click_on_bubble(
    click_count: usize,
    target_msg_id: &str,
    down: Option<(&str, f32, f32)>,
    prev_click: Option<(&str, f32, f32, u64)>,
    up_pos: (f32, f32),
) -> bool {
    let Some((down_id, dx, dy)) = down else {
        return false;
    };
    if down_id != target_msg_id {
        return false;
    }
    let (prev_pos, elapsed_ms) = match prev_click {
        Some((prev_id, px, py, elapsed)) if prev_id == target_msg_id => ((px, py), elapsed),
        _ => return false,
    };
    let click_to_click_close = (up_pos.0 - prev_pos.0).abs() <= DOUBLE_CLICK_DRAG_THRESHOLD_PX
        && (up_pos.1 - prev_pos.1).abs() <= DOUBLE_CLICK_DRAG_THRESHOLD_PX;
    click_to_click_close
        && should_toggle_double_click_like(click_count, (dx, dy), up_pos, elapsed_ms)
}

// ============================================================================
// 6. Image Paste/Drop, File Attach, SSH Atomic Staging & Thumbnails
// ============================================================================

fn extract_basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// Checks strictly whether `path`'s basename ends with an allowlisted image extension
/// (`.png`, `.jpg`, `.jpeg`, `.gif`, `.webp`, ASCII case-insensitive).
pub(crate) fn is_supported_image_ext(path: &str) -> bool {
    let base = extract_basename(path);
    let Some((_, ext)) = base.rsplit_once('.') else {
        return false;
    };
    if !ext.is_ascii() {
        return false;
    }
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp"
    )
}

#[cfg(test)]
pub(crate) fn classify_and_validate_attachments(
    items: &[AttachmentCandidate],
) -> AttachmentClassification {
    classify_and_validate_attachments_with_existing(items, 0, 0)
}

pub(crate) fn classify_and_validate_attachments_with_existing(
    items: &[AttachmentCandidate],
    existing_images: usize,
    existing_files: usize,
) -> AttachmentClassification {
    let mut out = AttachmentClassification::default();

    for item in items {
        let path_str = item.path.to_string_lossy();
        let is_image = is_supported_image_ext(&path_str);
        if is_image {
            if item.size_bytes > MAX_IMAGE_BYTES {
                out.rejected_oversize.push((
                    item.path.clone(),
                    format!("Image exceeds 10 MiB limit ({} bytes)", item.size_bytes),
                ));
                continue;
            }
            if existing_images.saturating_add(out.images.len()) >= MAX_IMAGE_ATTACHMENTS {
                out.rejected_too_many += 1;
                continue;
            }
            out.images.push(item.clone());
        } else {
            if item.size_bytes > MAX_FILE_BYTES {
                out.rejected_oversize.push((
                    item.path.clone(),
                    format!("File exceeds 200 MiB limit ({} bytes)", item.size_bytes),
                ));
                continue;
            }
            if existing_files.saturating_add(out.files.len()) >= MAX_FILE_ATTACHMENTS {
                out.rejected_too_many += 1;
                continue;
            }
            out.files.push(item.clone());
        }
    }

    out
}

/// Replaces staged local filename placeholders in `payload` with their uploaded remote
/// `~/.multibot/uploads/<session>/<file>` paths in order, appending any unreferenced uploaded path.
pub(crate) fn substitute_uploaded_attachment_paths(
    payload: &str,
    replacements: &[(String, String)],
) -> String {
    let mut out = payload.to_string();
    let mut cursor = 0usize;
    let mut extra_paths = Vec::new();

    for (token, remote_path) in replacements {
        if token.is_empty() {
            extra_paths.push(remote_path.as_str());
            continue;
        }
        let search_slice = if cursor <= out.len() && out.is_char_boundary(cursor) {
            &out[cursor..]
        } else {
            cursor = 0;
            &out[..]
        };
        let found_offset = find_standalone_token(search_slice, token)
            .map(|rel| cursor + rel)
            .or_else(|| find_standalone_token(&out, token));
        if let Some(pos) = found_offset {
            let end = pos + token.len();
            out.replace_range(pos..end, remote_path);
            cursor = pos + remote_path.len();
        } else {
            extra_paths.push(remote_path.as_str());
        }
    }

    if !extra_paths.is_empty() {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&extra_paths.join("\n"));
    }
    out
}

fn find_standalone_token(haystack: &str, needle: &str) -> Option<usize> {
    let mut start = 0usize;
    while let Some(rel) = haystack[start..].find(needle) {
        let pos = start + rel;
        let end = pos + needle.len();
        let left_ok = pos == 0
            || haystack[..pos]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
        let right_ok = end == haystack.len()
            || haystack[end..]
                .chars()
                .next()
                .is_some_and(char::is_whitespace);
        if left_ok && right_ok {
            return Some(pos);
        }
        let step = haystack[pos..].chars().next().map_or(1, char::len_utf8);
        start = pos + step;
    }
    None
}

fn sanitize_ascii_token(raw: &str, max_bytes: usize, fallback: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(max_bytes));
    let mut at_leading_dots = true;
    for ch in raw.chars() {
        if out.len() >= max_bytes {
            break;
        }
        let safe = if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            ch
        } else {
            '_'
        };
        if at_leading_dots && safe == '.' {
            out.push('_');
        } else {
            at_leading_dots = false;
            out.push(safe);
        }
    }
    if out.is_empty() {
        fallback.to_string()
    } else {
        out
    }
}

pub(crate) fn sanitize_remote_session_dir(session: &str) -> String {
    sanitize_ascii_token(session, REMOTE_NAME_MAX_BYTES, "session")
}

pub(crate) fn sanitize_remote_filename(path: &str) -> String {
    let base = extract_basename(path);
    if base.is_empty() {
        return "file".to_string();
    }
    let sanitized_full = sanitize_ascii_token(base, usize::MAX, "file");
    if sanitized_full.len() <= REMOTE_NAME_MAX_BYTES {
        return sanitized_full;
    }
    if let Some(dot) = sanitized_full.rfind('.') {
        let ext = &sanitized_full[dot..];
        if dot > 0 && ext.len() < REMOTE_NAME_MAX_BYTES {
            let stem_cap = REMOTE_NAME_MAX_BYTES - ext.len();
            let mut truncated = sanitized_full[..stem_cap].to_string();
            truncated.push_str(ext);
            return truncated;
        }
    }
    sanitized_full[..REMOTE_NAME_MAX_BYTES].to_string()
}

/// Builds a shell-safe, Windows-argv-safe (zero `"`), atomic SSH staging upload command
/// targeting `$HOME/.multibot/uploads/<sanitized_session>/<sanitized_name>` (NEVER `~/.multiprompt`).
pub(crate) fn build_ssh_upload_command(
    session_dir: &str,
    filename: &str,
    expected_size: u64,
) -> String {
    let dir = sanitize_remote_session_dir(session_dir);
    let file = sanitize_remote_filename(filename);
    format!(
        "sh -c 'set -ef; IFS=; umask 077; d=$HOME/.multibot/uploads/{dir}; mkdir -p $d; \
find $d -maxdepth 1 -type d -name .staging.* -mmin +1440 -exec rm -rf -- {{}} +; \
t=$(mktemp -d $d/.staging.XXXXXXXXXX); p=; \
trap '\\''[ -z $p ] || kill $p 2>/dev/null || :; rm -rf -- $t'\\'' 0; \
trap '\\''exit 1'\\'' HUP INT TERM; exec 3<&0; \
if dd if=/dev/null of=/dev/null status=progress 2>/dev/null; then \
dd bs=65536 status=progress <&3 > $t/data & else cat <&3 > $t/data & fi; \
p=$!; exec 3<&-; wait $p; p=; \
[ $(wc -c < $t/data) -eq {expected_size} ] || exit 1; \
f=$d/{file}; n=0; \
until ln -T -- $t/data $f 2>/dev/null; do \
[ -e $f ] || [ -L $f ] || exit 1; n=$((n+1)); f=$d/$n-{file}; \
done; rm -rf -- $t; trap - 0 HUP INT TERM; printf %s\\\\n $f'"
    )
}

/// Re-stats a staged attachment right before SSH upload, verifies it is still a regular file
/// within the configured image/file byte cap, and builds a fresh SSH upload command targeting
/// `session_dir` with the current file byte size.
pub(crate) async fn prepare_attachment_upload_command(
    session_dir: &str,
    att: &StagedAttachment,
) -> anyhow::Result<String> {
    use anyhow::Context as _;
    let meta = tokio::fs::metadata(&att.path)
        .await
        .with_context(|| format!("failed to stat attachment {}", att.path.display()))?;
    if !meta.is_file() {
        anyhow::bail!("attachment is not a regular file: {}", att.path.display());
    }
    let current_size = meta.len();
    let (limit, kind) = if att.is_image {
        (MAX_IMAGE_BYTES, "Image")
    } else {
        (MAX_FILE_BYTES, "File")
    };
    if current_size > limit {
        anyhow::bail!(
            "{kind} {} exceeds size limit ({current_size} bytes > {limit} bytes)",
            att.inserted_token
        );
    }
    Ok(build_ssh_upload_command(
        session_dir,
        &att.inserted_token,
        current_size,
    ))
}

/// Inserts an uploaded image/file reference at `(selection_start, selection_end)` with
/// exact single-space boundary rules (preserving `\t`/`\r`/`\n` and UTF-8 boundaries).
pub(crate) fn plan_image_insert(
    current: &str,
    selection_start: usize,
    selection_end: usize,
    reference: &str,
) -> Option<ImageInsertion> {
    let start = selection_start.min(selection_end).min(current.len());
    let mut end = selection_start.max(selection_end).min(current.len());
    if !current.is_char_boundary(start) || !current.is_char_boundary(end) {
        return None;
    }
    let trimmed_ref = reference.trim();
    if trimmed_ref.is_empty() {
        return None;
    }
    let needs_lead_space =
        start > 0 && !matches!(current.as_bytes()[start - 1], b' ' | b'\t' | b'\r' | b'\n');
    let bytes = current.as_bytes();
    while end < bytes.len() && bytes[end] == b' ' {
        end += 1;
    }

    let mut out = String::with_capacity(current.len() + trimmed_ref.len() + 2);
    out.push_str(&current[..start]);
    if needs_lead_space {
        out.push(' ');
    }
    out.push_str(trimmed_ref);
    out.push(' ');
    let caret_byte = out.len();
    out.push_str(&current[end..]);
    Some(ImageInsertion {
        text: out,
        caret_byte,
    })
}

/// Extracts the image file extension from a URL after stripping `?query` and `#fragment`.
pub(crate) fn image_ext_from_url(url: &str) -> &str {
    let without_fragment = url.split('#').next().unwrap_or(url);
    let without_query = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment);
    let base = extract_basename(without_query);
    match base.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() => ext,
        _ => "png",
    }
}

pub(crate) fn image_cache_path_in(cache_dir: &Path, key: &str, ext: &str) -> PathBuf {
    let hash = sha256_hex(key.as_bytes());
    let clean_ext = ext.trim_start_matches('.').to_ascii_lowercase();
    let safe_ext = match clean_ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" => clean_ext.as_str(),
        _ => "png",
    };
    cache_dir.join(format!("{hash}.{safe_ext}"))
}

pub(crate) fn image_cache_path(key: &str, ext: &str) -> PathBuf {
    image_cache_path_in(&MultibotSettings::image_cache_dir(), key, ext)
}

// ============================================================================
// 7. MultibotView Chat Actions & Rich Rendering
// ============================================================================

impl MultibotView {
    fn active_chat_tab_key(&self) -> String {
        self.selected_session
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
        self.chat_state.message_action_menu_open = false;
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
        self.selected_session
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
        self.chat_state.message_action_menu_open = false;
        let query = self.chat_state.find_query.clone();
        self.chat_state
            .update_find_query(query, &self.messages, show);
        cx.notify();
        show
    }

    fn render_tool_cluster(
        &self,
        msg: &ChatMessage,
        msg_idx: usize,
        count: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let expanded = self.tool_cluster_open(msg, msg_idx);
        let key = (self.tool_chat_key(), tool_cluster_key(msg, msg_idx));
        let label = format!("{count} tool call{}", if count == 1 { "" } else { "s" });
        h_flex()
            .id(SharedString::from(format!("tool-cluster-{msg_idx}")))
            .role(Role::Button)
            .aria_label(label.clone())
            .aria_expanded(expanded)
            .test_support()
            .gap_1p5()
            .items_center()
            .self_start()
            .px_2()
            .py_1()
            .rounded(RADIUS_CHIP)
            .bg(rgb(0x161616))
            .border_1()
            .border_color(rgb(0x2A2A2A))
            .text_xs()
            .text_color(theme::text_muted())
            .cursor_pointer()
            .hover(|s| s.bg(theme::bg_hover()))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.chat_state
                    .tool_cluster_overrides
                    .insert(key.clone(), !expanded);
                cx.notify();
            }))
            .child(lucide_icon(
                if expanded {
                    icons::CHEVRON_DOWN
                } else {
                    icons::CHEVRON_RIGHT
                },
                12.0,
            ))
            .child(label)
            .into_any_element()
    }

    pub(crate) fn copy_latest_code_block(&mut self, cx: &mut Context<Self>) -> bool {
        if let Some(code) = extract_latest_code_block(&self.messages) {
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

    pub(crate) fn set_reply_quote(
        &mut self,
        message_id: String,
        full_text: String,
        cx: &mut Context<Self>,
    ) {
        let preview = normalize_ws_collapse(&full_text)
            .chars()
            .take(100)
            .collect::<String>();
        self.chat_state.reply_quote = Some(ReplyQuote {
            message_id,
            preview,
            full_text,
        });
        cx.notify();
    }

    pub(crate) fn clear_reply_quote(&mut self, cx: &mut Context<Self>) {
        self.chat_state.reply_quote = None;
        cx.notify();
    }

    /// Records an asynchronous daemon send failure (wired in M6).
    #[allow(dead_code)]
    pub(crate) fn record_send_failure(
        &mut self,
        msg_id: String,
        text: String,
        reason: String,
        cx: &mut Context<Self>,
    ) {
        self.chat_state.failed_sends.insert(
            msg_id.clone(),
            FailedSend {
                message_id: msg_id,
                text,
                reason,
            },
        );
        cx.notify();
    }

    pub(crate) fn retry_failed_send(&mut self, msg_id: &str, cx: &mut Context<Self>) -> bool {
        let Some(failed) = self.chat_state.failed_sends.get(msg_id).cloned() else {
            return false;
        };
        let Some(session) = self.selected_session.clone() else {
            return false;
        };
        let key = self.session_key_for(&session);
        if (session.archived
            || self
                .sidebar_state
                .workspace_store
                .session_lifecycle(&key, &session, now_unix_ms())
                == multibot_core::workspaces::SessionLifecycle::Dissolved)
            && !self.reactivate_session_key(&key, cx)
        {
            return false;
        }
        let Some(tx) = self.active_session_tx() else {
            return false;
        };
        let sent = tx
            .send(ClientRequest::SendPrompt {
                options: self.composer_options_for(&session),
                session_id: session.id,
                agent: session.agent,
                cwd: session.cwd,
                text: failed.text,
            })
            .is_ok();
        if sent {
            self.chat_state.failed_sends.remove(msg_id);
            self.sidebar_state
                .workspace_store
                .record_session_input(&key, now_unix_ms());
            self.persist_workspace_store_if_enabled();
            self.sync_workspace_to_shared(cx);
        }
        cx.notify();
        sent
    }

    fn dispatch_text_prompt(
        &mut self,
        text: String,
        quoted_message_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.selected_session.clone() else {
            self.notice = Some("Select a session first.".into());
            cx.notify();
            return;
        };
        let host = self.active_session_host().to_string();
        self.dispatch_text_prompt_for_session(&host, session, text, quoted_message_id, cx);
    }

    fn dispatch_text_prompt_for_session(
        &mut self,
        host: &str,
        session: SessionSummary,
        text: String,
        quoted_message_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let key = canonical_session_key(host, session.agent, &session.id);
        if (session.archived
            || self
                .sidebar_state
                .workspace_store
                .session_lifecycle(&key, &session, now_unix_ms())
                == multibot_core::workspaces::SessionLifecycle::Dissolved)
            && !self.reactivate_session_key(&key, cx)
        {
            return;
        }
        let is_selected = self.active_session_host() == host
            && self
                .selected_session
                .as_ref()
                .is_some_and(|s| s.id == session.id && s.agent == session.agent);
        let msg_id = format!("local-msg-{}", self.messages.len());
        if is_selected {
            self.follow_chat_from_composer(cx);
            self.messages.push(ChatMessage {
                id: msg_id.clone(),
                role: MessageRole::User,
                text: text.clone(),
                timestamp_ms: None,
                quoted_message_id,
                ..Default::default()
            });
            self.chat_state.advance_busy_tick();
        }
        self.sidebar_state
            .workspace_store
            .record_session_input(&key, now_unix_ms());
        self.persist_workspace_store_if_enabled();
        self.sync_workspace_to_shared(cx);
        let targets = self.mention_targets();
        let mut delivered = HashSet::new();
        for mention in crate::mention_picker::occurrences(&text, &targets) {
            if text[..mention.range.start]
                .rsplit('\n')
                .next()
                .is_some_and(|line| line.trim_start().starts_with('>'))
            {
                continue;
            }
            if let Some(target) = mention.target
                && target.session_id != session.id
                && self
                    .host_for_session_key(&target.session_key)
                    .is_some_and(|(target_host, _)| target_host == host)
                && delivered.insert(target.session_id.clone())
                && let Some(tx) = self.tx_for_host(host)
            {
                let _ = tx.send(ClientRequest::SendCrossSession {
                    from_session_id: session.id.clone(),
                    target_session_id: target.session_id,
                    text: text.clone(),
                });
            }
        }
        self.send_or_queue_prompt_on_host(host, &session, msg_id, text);
        cx.notify();
    }

    pub(crate) fn track_queued_prompt(&mut self, session_id: &str, message_id: &str, status: &str) {
        let Some(session) = self
            .selected_session
            .as_ref()
            .filter(|s| s.id == session_id)
            .or_else(|| {
                self.host_sessions
                    .iter()
                    .find(|(h, s)| h == self.active_session_host() && s.id == session_id)
                    .map(|(_, s)| s)
            })
            .or_else(|| self.sessions.iter().find(|s| s.id == session_id))
        else {
            return;
        };
        let key = self.session_key_for(session);
        if status == "queued" {
            let preview = "Waiting for the agent to finish".to_string();
            let queue = self.chat_state.queued_prompts.entry(key).or_default();
            if queue.len() < 20 && !queue.iter().any(|(id, _)| id == message_id) {
                queue.push((message_id.to_string(), preview));
            }
        } else if matches!(status, "sent" | "cancelled" | "failed") {
            if let Some(queue) = self.chat_state.queued_prompts.get_mut(&key) {
                queue.retain(|(id, _)| id != message_id);
                if queue.is_empty() {
                    self.chat_state.queued_prompts.remove(&key);
                }
            }
        }
    }

    pub(crate) fn composer_options_for(&self, session: &SessionSummary) -> Option<SessionOptions> {
        self.chat_state
            .composer_options
            .get(&self.session_key_for(session))
            .filter(|options| !options.is_default())
            .cloned()
    }

    pub(crate) fn spawn_composer_options(&self, agent: AgentKind) -> Option<SessionOptions> {
        let options = match &self.selected_session {
            Some(session) if session.agent == agent => self.composer_options_for(session),
            Some(_) => None,
            None if agent == AgentKind::Claude => Some(self.chat_state.draft_options.clone()),
            None => None,
        };
        options.filter(|options| {
            matches!(agent, AgentKind::Claude | AgentKind::Codex) && !options.is_default()
        })
    }

    pub(crate) fn active_composer_options(&self) -> SessionOptions {
        self.selected_session
            .as_ref()
            .and_then(|s| self.composer_options_for(s))
            .unwrap_or_else(|| {
                if self.selected_session.is_none() {
                    self.chat_state.draft_options.clone()
                } else {
                    SessionOptions::default()
                }
            })
    }

    fn update_composer_options(
        &mut self,
        update: impl FnOnce(&mut SessionOptions),
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = &self.selected_session {
            let key = self.session_key_for(session);
            update(self.chat_state.composer_options.entry(key).or_default());
        } else {
            update(&mut self.chat_state.draft_options);
        }
        cx.notify();
    }

    fn render_composer_options(&self, cx: &mut Context<Self>) -> AnyElement {
        let agent = self
            .selected_session
            .as_ref()
            .map_or(AgentKind::Claude, |s| s.agent);
        let options = self.active_composer_options();
        let supported = matches!(agent, AgentKind::Claude | AgentKind::Codex);
        let mut menu = v_flex()
            .id("composer-options")
            .test_support()
            .w(px(390.0))
            .p_3()
            .gap_2()
            .rounded(RADIUS_ROW)
            .bg(theme::bg_surface())
            .border_1()
            .border_color(theme::border_subtle())
            .child(
                div()
                    .text_xs()
                    .text_color(theme::text_secondary())
                    .child("Provider / Harness"),
            )
            .child(
                Button::new("composer-provider")
                    .small()
                    .label(agent.label())
                    .on_click(
                        cx.listener(|this, _, window, cx| this.open_provider_picker(window, cx)),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme::text_secondary())
                    .child("AI model"),
            );
        let models: &[(&str, Option<&str>)] = match agent {
            AgentKind::Claude => &[
                ("Default", None),
                ("Opus", Some("opus")),
                ("Sonnet", Some("sonnet")),
            ],
            AgentKind::Codex => &[
                ("Default", None),
                ("GPT-5.4", Some("gpt-5.4")),
                ("Codex", Some("gpt-5.3-codex")),
            ],
            AgentKind::Antigravity | AgentKind::Shell | AgentKind::Pi | AgentKind::Cursor => {
                &[("Default", None)]
            }
        };
        let mut row = h_flex().gap_1();
        for (index, &(label, model)) in models.iter().enumerate() {
            let selected = options.model.as_deref() == model;
            let model = model.map(String::from);
            row = row.child(
                Button::new(format!("composer-model-{index}"))
                    .small()
                    .label(label)
                    .when(selected, |b| b.icon(lucide_icon(icons::CHECK, 14.0)))
                    .selected(selected)
                    .toggled(selected)
                    .when(selected, |b| b.primary())
                    .disabled(!supported)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.update_composer_options(|o| o.model = model.clone(), cx);
                    })),
            );
        }
        menu = menu.child(row).child(
            div()
                .text_xs()
                .text_color(theme::text_secondary())
                .child("Effort level"),
        );
        let mut row = h_flex().gap_1();
        for (index, (label, effort)) in [
            ("Default", None),
            ("Low", Some(EffortLevel::Low)),
            ("Medium", Some(EffortLevel::Medium)),
            ("High", Some(EffortLevel::High)),
            ("Max", Some(EffortLevel::Max)),
        ]
        .into_iter()
        .enumerate()
        {
            if effort == Some(EffortLevel::Max) && agent != AgentKind::Codex {
                continue;
            }
            let selected = options.effort == effort;
            row = row.child(
                Button::new(format!("composer-effort-{index}"))
                    .small()
                    .label(label)
                    .when(selected, |b| b.icon(lucide_icon(icons::CHECK, 14.0)))
                    .selected(selected)
                    .toggled(selected)
                    .when(selected, |b| b.primary())
                    .disabled(!supported)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.update_composer_options(|o| o.effort = effort, cx);
                    })),
            );
        }
        menu = menu.child(row).child(
            div()
                .text_xs()
                .text_color(theme::text_secondary())
                .child("Permission level"),
        );
        let mut row = h_flex().gap_1();
        for (index, (label, permission)) in [
            ("Default", None),
            ("Ask", Some(PermissionLevel::Ask)),
            ("Auto", Some(PermissionLevel::Auto)),
            ("Full access", Some(PermissionLevel::FullAccess)),
        ]
        .into_iter()
        .enumerate()
        {
            let selected = options.permission == permission;
            row = row.child(
                Button::new(format!("composer-permission-{index}"))
                    .small()
                    .label(label)
                    .when(selected, |b| b.icon(lucide_icon(icons::CHECK, 14.0)))
                    .selected(selected)
                    .toggled(selected)
                    .when(selected, |b| b.primary())
                    .disabled(!supported)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.update_composer_options(|o| o.permission = permission, cx);
                    })),
            );
        }
        menu.child(row)
            .child(
                div()
                    .text_xs()
                    .text_color(theme::text_muted())
                    .child(if !supported {
                        "Antigravity uses agent defaults; overrides are not supported."
                    } else if options.permission == Some(PermissionLevel::FullAccess) {
                        "Full access bypasses approval checks. Applies on the next turn."
                    } else {
                        "Applies on the next turn. Auto allows workspace edits."
                    }),
            )
            .child(
                Button::new("composer-attach")
                    .small()
                    .label("Attach from clipboard")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.chat_state.composer_menu_open = false;
                        if !this.handle_paste_from_clipboard(window, cx) {
                            this.notice =
                                Some("Drop files or paste images/files with Ctrl+V.".into());
                        }
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    pub(crate) fn send_current_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .chat_state
            .pending_harness_switch
            .as_ref()
            .is_some_and(|id| self.selected_session.as_ref().is_some_and(|s| &s.id == id))
        {
            self.notice = Some("Wait for the provider / harness switch to finish.".into());
            cx.notify();
            return;
        }
        self.follow_chat_from_composer(cx);
        match self.voice.phase {
            multibot_core::voice::DictationPhase::Listening => {
                self.voice.submit_after_finish = true;
                self.toggle_dictation(window, cx);
                return;
            }
            multibot_core::voice::DictationPhase::Finishing => {
                self.voice.submit_after_finish = true;
                return;
            }
            multibot_core::voice::DictationPhase::Off => {}
        }
        let raw = self.bound_mention_prompt(cx);
        let trimmed = raw.trim().to_string();
        if trimmed.is_empty() {
            return;
        }
        let Some(session) = self.selected_session.clone() else {
            self.notice = Some("Select a session first.".into());
            cx.notify();
            return;
        };

        let key = self.session_key_for(&session);
        if session.agent == AgentKind::Codex && matches!(trimmed.as_str(), "/exit" | "/quit") {
            self.prompt_input
                .update(cx, |state, cx| state.set_value("", window, cx));
            self.slash_picker_state.close();
            self.dissolve_session_key(&key, cx);
            return;
        }
        if self
            .sidebar_state
            .workspace_store
            .session_lifecycle(&key, &session, now_unix_ms())
            == multibot_core::workspaces::SessionLifecycle::Dissolved
            && !self.reactivate_session_key(&key, cx)
        {
            self.notice = Some("Reconnect to this chat's host before reactivating it.".into());
            cx.notify();
            return;
        }

        // Intercept `/rename <new title>` locally to rename and sync with `multibotd`.
        if let Some(new_title) = trimmed
            .strip_prefix("/rename ")
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let key = self.session_key_for(&session);
            self.prompt_input.update(cx, |state, cx| {
                state.set_value("", window, cx);
            });
            self.slash_picker_state.close();
            self.commit_session_rename(&key, new_title, cx);
            return;
        }

        let quote = self.chat_state.reply_quote.take();
        let payload = compose_quoted_reply(quote.as_ref().map(|q| q.full_text.as_str()), &trimmed);
        let quoted_id = quote.as_ref().map(|q| q.message_id.clone());

        self.prompt_input.update(cx, |state, cx| {
            state.set_value("", window, cx);
        });
        self.slash_picker_state.close();
        let staged = std::mem::take(&mut self.chat_state.pending_attachments);

        if payload.is_empty() {
            return;
        }

        let session_host = self.active_session_host().to_string();
        let remote_ssh_host = cx.try_global::<SharedAppStore>().and_then(|store| {
            let target = store
                .host_connections
                .get(&session_host)
                .map(|h| &h.target)
                .or_else(|| {
                    if store.host_connections.is_empty() {
                        store.connected_target.as_ref()
                    } else {
                        None
                    }
                });
            match target {
                Some(DaemonTarget::RemoteSsh { host, .. }) => Some(host.clone()),
                _ => None,
            }
        });

        if !staged.is_empty()
            && let Some(ssh_host) = remote_ssh_host
        {
            let rt = self.runtime.clone();
            let staged_for_upload = staged.clone();
            let target_session_dir = session.id.clone();
            cx.spawn_in(window, async move |this, cx| {
                let upload_result = rt
                    .spawn(async move {
                        let mut replacements = Vec::with_capacity(staged_for_upload.len());
                        for att in &staged_for_upload {
                            let upload_cmd =
                                prepare_attachment_upload_command(&target_session_dir, att).await?;
                            let remote_path = multibot_core::daemon_deploy::upload_file_over_ssh(
                                &ssh_host,
                                &att.path,
                                &upload_cmd,
                            )
                            .await?;
                            replacements.push((att.inserted_token.clone(), remote_path));
                        }
                        anyhow::Ok(replacements)
                    })
                    .await;

                let _ = this.update_in(cx, |this, window, cx| match upload_result {
                    Ok(Ok(replacements)) => {
                        let final_payload =
                            substitute_uploaded_attachment_paths(&payload, &replacements);
                        this.dispatch_text_prompt_for_session(
                            &session_host,
                            session,
                            final_payload,
                            quoted_id,
                            cx,
                        );
                    }
                    Ok(Err(err)) => {
                        this.restore_failed_upload_draft(
                            &session,
                            raw,
                            quote,
                            staged,
                            format!("Attachment upload failed: {err}"),
                            window,
                            cx,
                        );
                    }
                    Err(join_err) => {
                        this.restore_failed_upload_draft(
                            &session,
                            raw,
                            quote,
                            staged,
                            format!("Attachment upload task failed: {join_err}"),
                            window,
                            cx,
                        );
                    }
                });
            })
            .detach();
            cx.notify();
            return;
        }

        self.dispatch_text_prompt_for_session(&session_host, session, payload, quoted_id, cx);
    }

    pub(crate) fn restore_failed_upload_draft(
        &mut self,
        target_session: &SessionSummary,
        raw_prompt: String,
        quote: Option<ReplyQuote>,
        staged: Vec<StagedAttachment>,
        err_msg: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.notice = Some(err_msg);
        let same_session = self
            .selected_session
            .as_ref()
            .is_some_and(|s| s.id == target_session.id && s.agent == target_session.agent);
        let prompt_empty = self.prompt_input.read(cx).value().trim().is_empty();
        if same_session && prompt_empty && self.chat_state.pending_attachments.is_empty() {
            self.chat_state.pending_attachments = staged;
            if self.chat_state.reply_quote.is_none() {
                self.chat_state.reply_quote = quote;
            }
            self.prompt_input.update(cx, |state, cx| {
                state.select_all(window, cx);
                state.replace(raw_prompt, window, cx);
            });
        }
        cx.notify();
    }

    pub(crate) fn toggle_local_reaction(
        &mut self,
        target_id: &str,
        target_text: &str,
        emoji: ReactionEmoji,
        cx: &mut Context<Self>,
    ) {
        if self.selected_session.as_ref().is_some_and(|s| s.archived) {
            return;
        }
        let server_masks = compute_server_reaction_masks(&self.messages);
        let server_has = (server_masks.get(target_id).copied().unwrap_or(0) & emoji.bit()) != 0;
        let local_add_has = self
            .chat_state
            .local_reactions
            .get(target_id)
            .is_some_and(|v| v.contains(&emoji));
        let local_rem_has = self
            .chat_state
            .local_reaction_removals
            .get(target_id)
            .is_some_and(|v| v.contains(&emoji));
        let currently_shown = if server_has {
            !local_rem_has
        } else {
            local_add_has
        };

        if currently_shown {
            if let Some(adds) = self.chat_state.local_reactions.get_mut(target_id) {
                adds.retain(|&e| e != emoji);
                if adds.is_empty() {
                    self.chat_state.local_reactions.remove(target_id);
                }
            }
            if server_has {
                let rems = self
                    .chat_state
                    .local_reaction_removals
                    .entry(target_id.to_string())
                    .or_default();
                if !rems.contains(&emoji) {
                    rems.push(emoji);
                }
            }
        } else {
            if let Some(rems) = self.chat_state.local_reaction_removals.get_mut(target_id) {
                rems.retain(|&e| e != emoji);
                if rems.is_empty() {
                    self.chat_state.local_reaction_removals.remove(target_id);
                }
            }
            if !server_has {
                let adds = self
                    .chat_state
                    .local_reactions
                    .entry(target_id.to_string())
                    .or_default();
                if !adds.contains(&emoji) {
                    adds.push(emoji);
                }
            }
        }

        if let Some(wire_text) = format_reaction_message(emoji, target_id, target_text)
            && let (Some(session), Some(tx)) =
                (self.selected_session.clone(), self.active_session_tx())
        {
            let _ = tx.send(ClientRequest::SendPrompt {
                options: self.composer_options_for(&session),
                session_id: session.id,
                agent: session.agent,
                cwd: session.cwd,
                text: wire_text,
            });
        }
        cx.notify();
    }

    #[cfg(windows)]
    pub(crate) fn stage_browser_screenshot(
        &mut self,
        path: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.stage_dropped_paths(&[path.to_path_buf()], window, cx);
        self.chat_state
            .pending_attachments
            .iter()
            .any(|attachment| attachment.path == path)
    }

    pub(crate) fn stage_dropped_paths(
        &mut self,
        paths: &[PathBuf],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut candidates = Vec::with_capacity(paths.len());
        let mut rejected_invalid = 0usize;
        for p in paths {
            match std::fs::metadata(p) {
                Ok(meta) if meta.is_file() => {
                    candidates.push(AttachmentCandidate {
                        path: p.clone(),
                        size_bytes: meta.len(),
                    });
                }
                _ => {
                    rejected_invalid += 1;
                }
            }
        }
        let existing_images = self
            .chat_state
            .pending_attachments
            .iter()
            .filter(|a| a.is_image)
            .count();
        let existing_files = self
            .chat_state
            .pending_attachments
            .iter()
            .filter(|a| !a.is_image)
            .count();
        let classified = classify_and_validate_attachments_with_existing(
            &candidates,
            existing_images,
            existing_files,
        );
        let mut current = self.prompt_input.read(cx).value().to_string();
        let mut caret = current.len();
        let staged_any = !classified.images.is_empty() || !classified.files.is_empty();

        for img_cand in classified.images {
            let name = img_cand
                .path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "image.png".to_string());
            if let Some(inserted) = plan_image_insert(&current, caret, caret, &name) {
                current = inserted.text;
                caret = inserted.caret_byte;
            }
            self.chat_state.pending_attachments.push(StagedAttachment {
                path: img_cand.path,
                is_image: true,
                size_bytes: img_cand.size_bytes,
                inserted_token: name,
            });
        }

        for file_cand in classified.files {
            let name = file_cand
                .path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".to_string());
            if let Some(inserted) = plan_image_insert(&current, caret, caret, &name) {
                current = inserted.text;
                caret = inserted.caret_byte;
            }
            self.chat_state.pending_attachments.push(StagedAttachment {
                path: file_cand.path,
                is_image: false,
                size_bytes: file_cand.size_bytes,
                inserted_token: name,
            });
        }

        let mut notice_parts = Vec::new();
        if rejected_invalid > 0 {
            notice_parts.push(format!(
                "Skipped {rejected_invalid} non-file or inaccessible path(s) (only regular files can be attached)"
            ));
        }
        if !classified.rejected_oversize.is_empty() || classified.rejected_too_many > 0 {
            notice_parts.push(format!(
                "Skipped {} oversized and {} extra attachment(s) (max 10 images <=10 MiB, 10 files <=200 MiB)",
                classified.rejected_oversize.len(),
                classified.rejected_too_many
            ));
        }
        if !notice_parts.is_empty() {
            self.notice = Some(format!("{}.", notice_parts.join("; ")));
        }

        if staged_any {
            self.prompt_input.update(cx, |state, cx| {
                state.select_all(window, cx);
                state.replace(current, window, cx);
            });
        }
        cx.notify();
    }

    fn handle_paste_from_clipboard(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(item) = cx.read_from_clipboard() else {
            return false;
        };
        let mut paths = Vec::new();
        for entry in item.entries() {
            if let ClipboardEntry::ExternalPaths(ext) = entry {
                paths.extend(ext.paths().iter().cloned());
            }
        }
        if paths.is_empty() {
            return false;
        }
        self.stage_dropped_paths(&paths, window, cx);
        true
    }

    fn is_composer_text_edit_key(ks: &Keystroke) -> bool {
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

    fn sync_slash_picker_from_composer(&mut self, cx: &mut Context<Self>) {
        let composer_val = self.prompt_input.read(cx).value().to_string();
        if composer_val.contains('/')
            || composer_val.contains('$')
            || self.slash_picker_state.is_open()
        {
            let was_open = self.slash_picker_state.is_open();
            let prev_trigger = self.slash_picker_state.trigger.clone();
            let agent = self
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

    fn has_higher_priority_modal(&self) -> bool {
        self.table_modal_state.is_open() || self.active_modal != ActiveModal::None
    }

    fn schedule_slash_picker_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    fn handle_slash_picker_control_key(
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

    fn handle_chat_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
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
            self.chat_state.message_action_menu_open = false;
            self.chat_state.message_action_index = 0;
            cx.notify();
            return true;
        }
        if self.chat_state.focused_message_idx.is_some() {
            self.chat_state.newest_chord.reset();
            self.chat_state.slash_sync_pending = false;
            self.chat_state.focused_message_idx = None;
            self.prompt_input
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

    fn handle_chat_capture_key_down(
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
        let ks = &ev.keystroke;
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
                .prompt_input
                .read(cx)
                .focus_handle(cx)
                .is_focused(window);
            let now_ms = self.chat_state.chord_epoch.elapsed().as_millis() as u64;
            let jump = if composer_focused {
                let text = self.prompt_input.read(cx).value().to_string();
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
                    self.prompt_input
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

    fn handle_chat_find_key_down(
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
                    &self.messages,
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
                        &self.messages,
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
            let msgs = self.messages.clone();
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
            self.prompt_input.update(cx, |state, cx| {
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
            let composer_val = self.prompt_input.read(cx).value().to_string();
            if composer_val.is_empty() {
                self.chat_state.slash_sync_pending = false;
                self.jump_to_newest(cx);
                self.prompt_input
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

    fn scroll_to_active_find_match(&mut self) {
        if let Some(idx) = self
            .chat_state
            .find_matches
            .get(self.chat_state.find_current)
            .copied()
        {
            self.chat_state.scroll_handle.scroll_to_item(idx);
        }
    }

    fn handle_empty_composer_nav_key_action(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let input = self.prompt_input.read(cx);
        let composer_empty = input.value().is_empty()
            || (input.focus_handle(cx).is_focused(window) && input.selected_range() == (0..0));
        let visible = project_visible_messages_with_overrides(
            &self.messages,
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
                let waiting = self.chat_state.failed_sends.contains_key(&v.message.id);
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
                self.prompt_input.update(cx, |input, cx| {
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
                    if orig_idx < self.messages.len() {
                        self.messages.remove(orig_idx);
                    }
                }
                cx.notify();
                true
            }
            MessageNavOutcome::ExecuteAction { action, .. } => {
                if let Some((orig_idx, msg_id, msg_text, _, _)) = focused_meta {
                    match action {
                        MessageMenuAction::Reply => {
                            self.set_reply_quote(msg_id, msg_text, cx);
                            self.chat_state.focused_message_idx = None;
                            self.prompt_input
                                .update(cx, |input, cx| input.focus(window, cx));
                        }
                        MessageMenuAction::Copy => {
                            cx.write_to_clipboard(ClipboardItem::new_string(msg_text));
                            self.notice = Some("Copied message.".into());
                            cx.notify();
                        }
                        MessageMenuAction::Quote => {
                            let quote = msg_text
                                .lines()
                                .map(|line| format!("> {line}"))
                                .collect::<Vec<_>>()
                                .join("\n");
                            self.prompt_input.update(cx, |input, cx| {
                                let draft = input.value().to_string();
                                let next = if draft.is_empty() {
                                    format!("{quote}\n\n")
                                } else {
                                    format!("{draft}\n\n{quote}\n\n")
                                };
                                let end = next.len();
                                input.set_value(next, window, cx);
                                input.set_selected_range(end..end, cx);
                                input.focus(window, cx);
                            });
                            self.chat_state.focused_message_idx = None;
                            cx.notify();
                        }
                        MessageMenuAction::React(emoji) => {
                            self.toggle_local_reaction(&msg_id, &msg_text, emoji, cx);
                        }
                        MessageMenuAction::CancelWaiting => {
                            self.chat_state.failed_sends.remove(&msg_id);
                            self.cancel_outbox_message(&msg_id);
                            if orig_idx < self.messages.len() {
                                self.messages.remove(orig_idx);
                            }
                            cx.notify();
                        }
                    }
                }
                true
            }
        }
    }

    fn render_message_action_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.chat_state.message_action_menu_open {
            return None;
        }
        let mut menu = v_flex()
            .id("message-action-menu")
            .test_support()
            .w(px(320.0))
            .p_2()
            .gap_1()
            .rounded(theme::RADIUS_MODAL)
            .bg(theme::bg_surface())
            .border_1()
            .border_color(theme::border_subtle());
        let visible = project_visible_messages_with_overrides(
            &self.messages,
            &self.chat_state.local_reactions,
            &self.chat_state.local_reaction_removals,
            self.show_tool_calls(),
        );
        let message = self
            .chat_state
            .focused_message_idx
            .and_then(|i| visible.get(i))?;
        let tool_only = message.message.text.trim().is_empty()
            && !message.message.blocks.is_empty()
            && message
                .message
                .blocks
                .iter()
                .all(|b| matches!(b, MessageBlock::ToolUse { .. }));
        let waiting = self
            .chat_state
            .failed_sends
            .contains_key(&message.message.id);
        for (index, action) in ChatViewState::menu_actions_for(tool_only, waiting)
            .into_iter()
            .enumerate()
        {
            let label = match action {
                MessageMenuAction::Copy => "Copy",
                MessageMenuAction::Reply => "Reply",
                MessageMenuAction::Quote => "Quote",
                MessageMenuAction::React(ReactionEmoji::Like) => "Like",
                MessageMenuAction::React(ReactionEmoji::Dislike) => "Dislike",
                MessageMenuAction::React(ReactionEmoji::Heart) => "Heart",
                MessageMenuAction::CancelWaiting => "Cancel waiting message",
            };
            menu = menu.child(
                div()
                    .id(SharedString::from(format!("message-action-{index}")))
                    .test_support()
                    .px_3()
                    .py_2()
                    .rounded(RADIUS_ROW)
                    .cursor_pointer()
                    .bg(if self.chat_state.message_action_index == index {
                        theme::bg_active_row()
                    } else {
                        theme::bg_surface()
                    })
                    .hover(|s| s.bg(theme::bg_hover()))
                    .child(label)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.chat_state.message_action_index = index;
                            this.handle_empty_composer_nav_key_action("enter", window, cx);
                        }),
                    ),
            );
        }
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui_kit::hsla(0.0, 0.0, 0.0, 0.45))
                .child(menu)
                .into_any_element(),
        )
    }

    fn render_markdown_with_actions(
        &self,
        id: SharedString,
        text: String,
        msg_idx: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let font_pt = self.effective_font_size_pt() as f32;
        let weak_code = cx.entity().downgrade();
        let weak_table = cx.entity().downgrade();

        // Check for Unicode box-drawing tables that CommonMark's pipe parser doesn't recognize.
        let extracted_box_tables: Vec<_> = extract_tables_from_markdown(&text)
            .into_iter()
            .filter(|t| t.table.source == TableSource::Box)
            .collect();

        let targets = self.mention_targets();
        let text = crate::mention_picker::render_mentions(&text, &targets);
        let weak_mention = cx.entity().downgrade();
        let md_view = TextView::markdown(id.clone(), text)
            .link(|link, _| {
                (crate::external_url::is_http_url(&link.url)
                    || link.url.starts_with("multibot-session://"))
                .then(|| link.clone())
            })
            .on_link_click(move |url, event, _, cx| {
                cx.stop_propagation();
                if !event.is_right_click() && !event.is_middle_click() {
                    if url.starts_with("multibot-session://") {
                        let _ = weak_mention.update(cx, |this, cx| {
                            this.jump_mention_url(url, cx);
                        });
                        return;
                    }
                    if let Err(error) = crate::external_url::open_external_url(url) {
                        eprintln!("Could not open chat link: {error}");
                    }
                }
            })
            .selectable(true)
            .text_size(px(font_pt))
            .code_block_actions(move |block: &CodeBlock, _window, _cx| {
                let code = block.code().to_string();
                let lang = block.lang().map(|l| l.to_string()).unwrap_or_default();
                let hash = fnv1a_64(code.as_bytes());
                let btn_id = SharedString::from(format!("copy-code-{msg_idx}-{hash:016x}"));
                let weak = weak_code.clone();
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .when(!lang.is_empty(), |el| {
                        el.child(
                            div()
                                .px_2()
                                .py_0p5()
                                .rounded(RADIUS_CHIP)
                                .bg(theme::bg_hover())
                                .text_xs()
                                .text_color(theme::text_muted())
                                .child(lang),
                        )
                    })
                    .child(
                        Button::new(btn_id)
                            .xsmall()
                            .icon(lucide_icon(icons::COPY, 14.0))
                            .label("Copy")
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(code.clone()));
                                let _ = weak.update(cx, |view, cx| {
                                    view.notice = Some("Copied code block.".into());
                                    cx.notify();
                                });
                            }),
                    )
            })
            .table_actions(move |table: &TableData, _window, _cx| {
                let data_clone = table.clone();
                let hash = fnv1a_64(data_clone.markdown.as_bytes());
                let btn_id = SharedString::from(format!("expand-md-table-{msg_idx}-{hash:016x}"));
                let weak = weak_table.clone();
                Button::new(btn_id)
                    .xsmall()
                    .label("Expand")
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |view, cx| {
                            view.open_table_from_gpui_data(&data_clone, cx);
                            if view.table_modal_state.is_open() {
                                view.focus_table_modal(window, cx);
                            }
                        });
                    })
            });

        if extracted_box_tables.is_empty() {
            md_view.into_any_element()
        } else {
            let mut col = v_flex().w_full().gap_2().child(md_view);
            for (t_idx, extracted) in extracted_box_tables.iter().enumerate() {
                col = col.child(self.render_inline_table_card(
                    &format!("box-{id}-{t_idx}"),
                    &extracted.table,
                    cx,
                ));
            }
            col.into_any_element()
        }
    }

    fn render_tool_step_block(
        &self,
        msg: &ChatMessage,
        msg_idx: usize,
        block_idx: usize,
        call_id: &str,
        name: &str,
        summary: &str,
        detail: &str,
        status: ToolStatus,
        diff: Option<&DiffBlock>,
        output: Option<&str>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = format!(
            "{}:{}",
            self.tool_chat_key(),
            tool_step_key(msg, msg_idx, block_idx, call_id)
        );
        let is_expanded = self.chat_state.expanded_tools.contains(&key);
        let diff_expanded = self.chat_state.expanded_diffs.contains(&key);
        let dot_color = match status {
            ToolStatus::Running => theme::accent_amber(),
            ToolStatus::Completed => theme::accent_green(),
            ToolStatus::Error => accent_red(),
        };
        let label = if !summary.trim().is_empty() {
            summary.to_string()
        } else {
            name.to_string()
        };
        let diff_counts = diff.map(|d| (d.added, d.removed));
        let toggle_key = key.clone();
        let diff_toggle_key = key.clone();

        let mut card = v_flex()
            .w_full()
            .rounded(RADIUS_ROW)
            .bg(theme::bg_canvas())
            .border_1()
            .border_color(theme::border_subtle())
            .overflow_hidden()
            .child(
                h_flex()
                    .id(SharedString::from(format!(
                        "tool-row-{msg_idx}-{block_idx}"
                    )))
                    .role(Role::Button)
                    .aria_expanded(is_expanded)
                    .test_support()
                    .w_full()
                    .px_3()
                    .py_2()
                    .justify_between()
                    .items_center()
                    .hover(|s| s.bg(theme::bg_hover()))
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            if !this.chat_state.expanded_tools.remove(&toggle_key) {
                                this.chat_state.expanded_tools.insert(toggle_key.clone());
                            }
                            cx.notify();
                        }),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .w(px(7.0))
                                    .h(px(7.0))
                                    .rounded(RADIUS_CHIP)
                                    .bg(dot_color),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::text_primary())
                                    .child(label),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .when_some(diff_counts, |el, (added, removed)| {
                                el.child(
                                    h_flex()
                                        .gap_1()
                                        .text_xs()
                                        .child(
                                            div()
                                                .text_color(theme::accent_green())
                                                .child(format!("+{added}")),
                                        )
                                        .child(
                                            div()
                                                .text_color(accent_red())
                                                .child(format!("-{removed}")),
                                        ),
                                )
                            })
                            .child(div().text_xs().text_color(theme::text_muted()).child(
                                lucide_icon(
                                    if is_expanded {
                                        icons::CHEVRON_DOWN
                                    } else {
                                        icons::CHEVRON_RIGHT
                                    },
                                    12.0,
                                ),
                            )),
                    ),
            );

        if is_expanded {
            let mut body = v_flex()
                .w_full()
                .px_3()
                .py_2()
                .gap_2()
                .border_t_1()
                .border_color(theme::border_subtle());

            if !detail.trim().is_empty() {
                body = body.child(
                    div()
                        .w_full()
                        .px_2p5()
                        .py_1p5()
                        .rounded(RADIUS_ROW)
                        .bg(code_bg())
                        .text_xs()
                        .text_color(theme::text_secondary())
                        .child(truncate_tool_text(detail, MAX_TOOL_TEXT_CHARS)),
                );
            }

            if let Some(d) = diff {
                let (visible_lines, remaining) = visible_diff_slice(d, diff_expanded);
                let mut diff_box = v_flex()
                    .w_full()
                    .rounded(RADIUS_ROW)
                    .bg(code_bg())
                    .border_1()
                    .border_color(theme::border_subtle())
                    .overflow_hidden();

                if let Some(ref path) = d.path {
                    diff_box = diff_box.child(
                        div()
                            .px_2p5()
                            .py_1()
                            .border_b_1()
                            .border_color(theme::border_subtle())
                            .text_xs()
                            .text_color(theme::text_muted())
                            .child(path.clone()),
                    );
                }

                for line in visible_lines {
                    let (row_bg, row_fg) = match classify_diff_line(line) {
                        DiffLineKind::Added => (Some(diff_added_bg()), diff_added_fg()),
                        DiffLineKind::Removed => (Some(diff_removed_bg()), diff_removed_fg()),
                        DiffLineKind::Context => (None, theme::text_secondary()),
                    };
                    let mut line_el = div()
                        .w_full()
                        .px_2p5()
                        .py_0p5()
                        .text_xs()
                        .text_color(row_fg);
                    if let Some(bg) = row_bg {
                        line_el = line_el.bg(bg);
                    }
                    diff_box = diff_box.child(line_el.child(line.clone()));
                }

                if remaining > 0 || (diff_expanded && d.lines.len() > DIFF_FOLDED_ROWS) {
                    let toggle_label = if !diff_expanded {
                        format!("… {remaining} more lines")
                    } else if remaining > 0 {
                        format!("… {remaining} more lines · Show fewer lines")
                    } else {
                        "Show fewer lines".to_string()
                    };
                    diff_box = diff_box.child(
                        div()
                            .id(SharedString::from(format!(
                                "diff-toggle-{msg_idx}-{block_idx}"
                            )))
                            .role(Role::Button)
                            .test_support()
                            .w_full()
                            .px_2p5()
                            .py_1()
                            .border_t_1()
                            .border_color(theme::border_subtle())
                            .hover(|s| s.bg(theme::bg_hover()))
                            .cursor_pointer()
                            .text_xs()
                            .text_color(theme::accent_blue())
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    if !this.chat_state.expanded_diffs.remove(&diff_toggle_key) {
                                        this.chat_state
                                            .expanded_diffs
                                            .insert(diff_toggle_key.clone());
                                    }
                                    cx.notify();
                                }),
                            )
                            .child(toggle_label),
                    );
                }

                body = body.child(diff_box);
            }

            if let Some(out) = output
                && !out.trim().is_empty()
            {
                body = body.child(
                    div()
                        .w_full()
                        .px_2p5()
                        .py_1p5()
                        .rounded(RADIUS_ROW)
                        .bg(code_bg())
                        .text_xs()
                        .text_color(theme::text_muted())
                        .child(truncate_tool_text(out, MAX_TOOL_TEXT_CHARS)),
                );
            }

            card = card.child(body);
        }

        card.into_any_element()
    }

    fn render_image_block(
        &self,
        url: Option<&str>,
        local_path: Option<&str>,
        alt: &str,
    ) -> AnyElement {
        let resolved_local = local_path
            .map(PathBuf::from)
            .filter(|p| p.is_file())
            .or_else(|| {
                url.map(|u| {
                    let ext = image_ext_from_url(u);
                    image_cache_path(u, ext)
                })
                .filter(|p| p.is_file())
            });

        let mut card = div()
            .max_w(px(320.0))
            .max_h(px(480.0))
            .rounded(RADIUS_ROW)
            .bg(theme::bg_canvas())
            .border_1()
            .border_color(theme::border_subtle())
            .overflow_hidden();

        if let Some(path) = resolved_local {
            card = card.child(img(path).max_w(px(320.0)).max_h(px(480.0)));
        } else {
            let caption = if !alt.trim().is_empty() {
                alt.to_string()
            } else if let Some(u) = url {
                u.to_string()
            } else {
                "Image attachment".to_string()
            };
            card = card.child(
                div()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .text_color(theme::text_secondary())
                    .child(caption),
            );
        }

        card.into_any_element()
    }

    fn render_question_prompt_block(
        &self,
        orig_idx: usize,
        block_idx: usize,
        prompt_id: &str,
        questions: &[PromptQuestion],
        is_latest_message: bool,
        is_primary_active_prompt: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let valid_questions = validate_question_prompt(prompt_id, questions)?;
        let total_questions = valid_questions.len();

        let mut container = v_flex().w_full().gap_2().pt_1();
        for (q_idx, q) in valid_questions.iter().enumerate() {
            let q_answered = self.chat_state.answered_prompts.contains(prompt_id)
                || self
                    .chat_state
                    .answered_questions
                    .get(prompt_id)
                    .is_some_and(|set| set.contains(&q_idx));
            let interactive = is_latest_message && !q_answered;

            let mask = self
                .chat_state
                .question_multi_selected
                .get(&(prompt_id.to_string(), q_idx))
                .copied()
                .unwrap_or(0);

            let mut q_card = v_flex()
                .w_full()
                .p_3()
                .gap_2()
                .rounded(RADIUS_ROW)
                .bg(theme::bg_canvas())
                .border_1()
                .border_color(theme::border_subtle())
                .child(
                    div()
                        .text_sm()
                        .text_color(theme::text_primary())
                        .child(q.question.clone()),
                );

            let mut opts_col = v_flex().w_full().gap_1p5();
            for (opt_idx, opt) in q.options.iter().enumerate() {
                let is_selected = q.multi_select && (mask & (1u32 << opt_idx)) != 0;
                let prompt_id_owned = prompt_id.to_string();
                let label_owned = opt.label.clone();
                let multi = q.multi_select;
                let card_id = if is_primary_active_prompt {
                    SharedString::from(format!("opt-card-{q_idx}-{opt_idx}"))
                } else if is_latest_message {
                    SharedString::from(format!("opt-card-b{block_idx}-{q_idx}-{opt_idx}"))
                } else {
                    SharedString::from(format!(
                        "opt-card-hist-{orig_idx}-{block_idx}-{q_idx}-{opt_idx}"
                    ))
                };

                opts_col = opts_col.child(
                    div()
                        .id(card_id)
                        .role(Role::Button)
                        .test_support()
                        .w_full()
                        .px_3()
                        .py_2()
                        .rounded(RADIUS_ROW)
                        .bg(if is_selected {
                            theme::bg_active_row()
                        } else {
                            theme::bg_surface()
                        })
                        .border_1()
                        .border_color(if is_selected {
                            theme::accent_blue()
                        } else {
                            theme::border_subtle()
                        })
                        .when(interactive, |el| {
                            el.hover(|s| s.bg(theme::bg_hover()))
                                .cursor_pointer()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        if multi {
                                            let entry = this
                                                .chat_state
                                                .question_multi_selected
                                                .entry((prompt_id_owned.clone(), q_idx))
                                                .or_insert(0);
                                            *entry ^= 1u32 << opt_idx;
                                            cx.notify();
                                        } else {
                                            let answered_set = this
                                                .chat_state
                                                .answered_questions
                                                .entry(prompt_id_owned.clone())
                                                .or_default();
                                            answered_set.insert(q_idx);
                                            if answered_set.len() >= total_questions {
                                                this.chat_state
                                                    .answered_prompts
                                                    .insert(prompt_id_owned.clone());
                                            }
                                            let reply =
                                                format_single_option_reply(opt_idx, &label_owned);
                                            this.dispatch_text_prompt(reply, None, cx);
                                        }
                                    }),
                                )
                                .on_mouse_up(MouseButton::Left, |_, _, cx| {
                                    cx.stop_propagation();
                                })
                        })
                        .child(
                            v_flex()
                                .gap_0p5()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme::text_primary())
                                        .child(format!("{}. {}", opt_idx + 1, opt.label)),
                                )
                                .when(!opt.description.trim().is_empty(), |el| {
                                    el.child(
                                        div()
                                            .text_xs()
                                            .text_color(theme::text_muted())
                                            .child(opt.description.clone()),
                                    )
                                }),
                        ),
                );
            }

            if interactive {
                let mut footer = h_flex().w_full().gap_2().items_center().pt_1();
                if q.multi_select {
                    let q_clone = (*q).clone();
                    let prompt_id_owned = prompt_id.to_string();
                    let submit_id = if is_primary_active_prompt {
                        SharedString::from(format!("opt-submit-{q_idx}"))
                    } else if is_latest_message {
                        SharedString::from(format!("opt-submit-b{block_idx}-{q_idx}"))
                    } else {
                        SharedString::from(format!(
                            "opt-submit-hist-{orig_idx}-{block_idx}-{q_idx}"
                        ))
                    };
                    footer = footer.child(
                        Button::new(submit_id)
                            .xsmall()
                            .primary()
                            .label("Submit")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let m = this
                                    .chat_state
                                    .question_multi_selected
                                    .get(&(prompt_id_owned.clone(), q_idx))
                                    .copied()
                                    .unwrap_or(0);
                                if let Some(reply) = format_multi_option_reply(&q_clone, m) {
                                    let answered_set = this
                                        .chat_state
                                        .answered_questions
                                        .entry(prompt_id_owned.clone())
                                        .or_default();
                                    answered_set.insert(q_idx);
                                    if answered_set.len() >= total_questions {
                                        this.chat_state
                                            .answered_prompts
                                            .insert(prompt_id_owned.clone());
                                    }
                                    this.dispatch_text_prompt(reply, None, cx);
                                }
                            })),
                    );
                }

                let other_id = if is_primary_active_prompt {
                    SharedString::from(format!("opt-other-{q_idx}"))
                } else if is_latest_message {
                    SharedString::from(format!("opt-other-b{block_idx}-{q_idx}"))
                } else {
                    SharedString::from(format!("opt-other-hist-{orig_idx}-{block_idx}-{q_idx}"))
                };
                footer = footer.child(
                    div()
                        .id(other_id)
                        .role(Role::Button)
                        .test_support()
                        .px_3()
                        .py_1()
                        .rounded(RADIUS_CHIP)
                        .bg(theme::bg_surface())
                        .border_1()
                        .border_color(theme::border_subtle())
                        .hover(|s| s.bg(theme::bg_hover()))
                        .cursor_pointer()
                        .text_xs()
                        .text_color(theme::text_secondary())
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                cx.stop_propagation();
                                this.sidebar_state.rename = None;
                                this.sidebar_state.new_workspace_draft = None;
                                this.prompt_input.update(cx, |state, cx| {
                                    state.focus(window, cx);
                                });
                                cx.notify();
                            }),
                        )
                        .child("Type something else"),
                );
                opts_col = opts_col.child(footer);
            }

            q_card = q_card.child(opts_col);
            container = container.child(q_card);
        }

        Some(container.into_any_element())
    }

    fn render_prose_options_card(
        &self,
        options: &[ProseOption],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut col = v_flex().w_full().gap_1p5().pt_1();
        for (idx, opt) in options.iter().enumerate() {
            let reply_text = format!("{}: {}", opt.label, opt.title);
            col = col.child(
                div()
                    .id(SharedString::from(format!("prose-opt-{idx}")))
                    .role(Role::Button)
                    .test_support()
                    .w_full()
                    .px_3()
                    .py_2()
                    .rounded(RADIUS_ROW)
                    .bg(theme::bg_canvas())
                    .border_1()
                    .border_color(theme::border_subtle())
                    .hover(|s| s.bg(theme::bg_hover()))
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.dispatch_text_prompt(reply_text.clone(), None, cx);
                        }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::text_primary())
                            .child(format!("{}) {}", opt.label, opt.title)),
                    ),
            );
        }
        col = col.child(
            div()
                .id("prose-opt-other")
                .role(Role::Button)
                .test_support()
                .px_3()
                .py_1()
                .rounded(RADIUS_CHIP)
                .bg(theme::bg_surface())
                .border_1()
                .border_color(theme::border_subtle())
                .hover(|s| s.bg(theme::bg_hover()))
                .cursor_pointer()
                .text_xs()
                .text_color(theme::text_secondary())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        cx.stop_propagation();
                        this.sidebar_state.rename = None;
                        this.sidebar_state.new_workspace_draft = None;
                        this.prompt_input.update(cx, |state, cx| {
                            state.focus(window, cx);
                        });
                        cx.notify();
                    }),
                )
                .child("Type something else"),
        );
        col.into_any_element()
    }

    fn render_todo_header_and_drawer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let tab_key = self.active_chat_tab_key();
        let cached_todos = self
            .chat_state
            .session_todos
            .get(&tab_key)
            .map(Vec::as_slice);
        let (todos, done, total) =
            effective_todos(&self.messages, self.selected_session.as_ref(), cached_todos);
        if total == 0 {
            return None;
        }
        let drawer_open = self
            .chat_state
            .todo_drawer_open
            .get(&tab_key)
            .copied()
            .unwrap_or(false);

        let mut wrapper = v_flex()
            .w_full()
            .when(!self.settings.shell_fullscreen, |el| el.max_w(px(760.0)))
            .mx_auto()
            .px_4()
            .pt_1()
            .gap_1p5()
            .child(
                h_flex().w_full().justify_between().items_center().child(
                    div()
                        .id("chat-todo-pill")
                        .role(Role::Button)
                        .test_support()
                        .px_3()
                        .py_1()
                        .rounded(RADIUS_CHIP)
                        .bg(theme::bg_surface())
                        .border_1()
                        .border_color(theme::border_subtle())
                        .hover(|s| s.bg(theme::bg_hover()))
                        .cursor_pointer()
                        .text_xs()
                        .text_color(theme::text_secondary())
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                cx.stop_propagation();
                                this.toggle_todo_drawer(cx);
                            }),
                        )
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(format!("{done} of {total} done"))
                        .child(lucide_icon(
                            if drawer_open {
                                icons::CHEVRON_DOWN
                            } else {
                                icons::CHEVRON_RIGHT
                            },
                            12.0,
                        )),
                ),
            );

        if drawer_open {
            let mut list = v_flex()
                .id("chat-todo-drawer")
                .test_support()
                .w_full()
                .max_h(px(220.0))
                .overflow_y_scroll()
                .p_3()
                .gap_1p5()
                .rounded(RADIUS_ROW)
                .bg(theme::bg_surface())
                .border_1()
                .border_color(theme::border_subtle());

            for item in todos {
                let (icon, color) = match item.status {
                    TodoStatus::Completed => (icons::CHECK_CIRCLE, theme::accent_green()),
                    TodoStatus::InProgress => (icons::CHEVRON_RIGHT, theme::accent_amber()),
                    TodoStatus::Pending => (icons::CIRCLE, theme::text_muted()),
                };
                list = list.child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(lucide_icon(icon, 14.0).text_color(color))
                        .child(
                            div()
                                .text_xs()
                                .text_color(if item.status == TodoStatus::Completed {
                                    theme::text_muted()
                                } else {
                                    theme::text_primary()
                                })
                                .child(item.content),
                        ),
                );
            }
            wrapper = wrapper.child(list);
        }

        Some(wrapper.into_any_element())
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
                                        .label("Prev")
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
                                        .label("Next")
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

    pub(crate) fn render_chat_panel(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let lifecycle = self
            .selected_session
            .as_ref()
            .map(|session| {
                self.sidebar_state.workspace_store.session_lifecycle(
                    &self.session_key_for(session),
                    session,
                    now_unix_ms(),
                )
            })
            .unwrap_or_default();
        let header_title = match &self.selected_session {
            Some(s) => format!("{}  {}", s.agent.glyph(), s.title),
            None => "Multibot · Ready".to_string(),
        };
        let options = self.active_composer_options();
        let options_summary = [
            options.model.clone(),
            options.effort.map(|effort| {
                match effort {
                    EffortLevel::Low => "Low",
                    EffortLevel::Medium => "Medium",
                    EffortLevel::High => "High",
                    EffortLevel::Max => "Max",
                }
                .to_string()
            }),
            options.permission.map(|permission| {
                match permission {
                    PermissionLevel::Ask => "Ask",
                    PermissionLevel::Auto => "Auto",
                    PermissionLevel::FullAccess => "Full access",
                }
                .to_string()
            }),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
        let is_working = self.selected_session.as_ref().is_some_and(|s| s.working);
        let busy_phase = self.chat_state.busy_tick_phase % 3;
        let visible_messages = project_visible_messages_with_overrides(
            &self.messages,
            &self.chat_state.local_reactions,
            &self.chat_state.local_reaction_removals,
            self.show_tool_calls(),
        );
        let trailing_prose_options = detect_trailing_prose_options(&self.messages);
        let active_find_idx = self.chat_state.active_find_message_index();

        let mut queued_entries = Vec::new();
        let mut queued_previews = Vec::new();
        if let Some(session) = &self.selected_session {
            let key = self.session_key_for(session);
            if let Some(queue) = self.chat_state.queued_prompts.get(&key) {
                queued_previews.extend(queue.iter().map(|(_, text)| text.clone()));
            }
            let outbox_key = multibot_core::outbox::ChatOutbox::session_key(
                self.active_session_host(),
                session.agent,
                &session.id,
            );
            let outbox = self.shared_chat_outbox.lock().ok();
            let outbox = outbox
                .as_ref()
                .map(|shared| &shared.outbox)
                .unwrap_or(&self.chat_outbox);
            queued_entries = outbox
                .entries
                .iter()
                .filter(|entry| entry.session_key == outbox_key)
                .cloned()
                .collect();
        }
        let mut queue_bar = v_flex()
            .id("queued-messages-bar")
            .test_support()
            .max_h(px(180.0))
            .overflow_y_scroll()
            .w_full()
            .when(!self.settings.shell_fullscreen, |el| el.max_w(px(760.0)))
            .mx_auto()
            .p_3()
            .gap_2()
            .rounded(RADIUS_ROW)
            .bg(theme::bg_surface())
            .border_1()
            .border_color(theme::border_subtle())
            .child(div().text_xs().child(format!(
                "Queued messages · {}",
                queued_previews.len() + queued_entries.len()
            )));
        for preview in &queued_previews {
            queue_bar = queue_bar.child(
                div()
                    .text_xs()
                    .text_color(theme::text_secondary())
                    .truncate()
                    .child(preview.clone()),
            );
        }

        for (index, entry) in queued_entries.iter().enumerate() {
            let force_id = entry.id.clone();
            let edit_id = entry.id.clone();
            let remove_id = entry.id.clone();
            queue_bar = queue_bar.child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .text_xs()
                    .child(
                        div()
                            .rounded(RADIUS_CHIP)
                            .px_2()
                            .py_1()
                            .text_color(theme::accent_amber())
                            .child("Queued"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(entry.text.clone()),
                    )
                    .child(
                        Button::new(format!("queue-send-{index}"))
                            .small()
                            .label("Send now")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.force_send_queued_entry(&force_id, cx);
                            })),
                    )
                    .child(
                        Button::new(format!("queue-edit-{index}"))
                            .small()
                            .label("Edit")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let draft = this.prompt_input.read(cx).value().to_string();
                                if let Some(mut entry) = this.remove_queued_entry(&edit_id) {
                                    let text = std::mem::replace(&mut entry.text, draft.clone());
                                    if !draft.is_empty()
                                        && let Ok(mut shared) = this.shared_chat_outbox.lock()
                                    {
                                        let _ = shared.outbox.enqueue_entry(entry);
                                        if shared.persist_to_disk || this.persist_outbox_to_disk {
                                            if let Err(err) = shared.outbox.save() {
                                                this.notice = Some(err.to_string());
                                            }
                                        }
                                        this.chat_outbox = shared.outbox.clone();
                                    }
                                    this.prompt_input.update(cx, |input, cx| {
                                        input.set_value(text, window, cx);
                                        input.focus(window, cx);
                                    });
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new(format!("queue-remove-{index}"))
                            .small()
                            .icon(lucide_icon(icons::X, 12.0))
                            .accessibility_label("Remove queued message")
                            .tooltip("Remove queued message")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.remove_queued_entry(&remove_id);
                                cx.notify();
                            })),
                    ),
            );
        }

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
            .py_4()
            .px_4()
            .overflow_y_scroll()
            .track_scroll(&self.chat_state.scroll_handle);

        if visible_messages.is_empty() {
            msg_col = msg_col.child(
                v_flex()
                    .w_full()
                    .when(!self.settings.shell_fullscreen, |el| el.max_w(px(760.0)))
                    .mx_auto()
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
                            .child("Multibot — Obsidian Dark GPUI Shell"),
                    )
                    .child(
                        TextView::markdown(
                            "welcome-md",
                            "- **Ctrl+K / Ctrl+Shift+K** — Open Command Palette\n\
                             - **Alt+I / Ctrl+Shift+I** — Open Cross-workspace Inbox\n\
                             - **Ctrl+,** — GPU Picker (0% NVIDIA verified) & VPS SSH hosts\n\
                             - **Ctrl+Shift+B** — Toggle sidebar · **F1** — All shortcuts",
                        )
                        .text_size(px(self.effective_font_size_pt() as f32)),
                    ),
            );
        } else {
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

                let mut body_col = v_flex().w_full().gap_2();

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
                                        &this.messages,
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
                    div()
                        .id(SharedString::from(format!("msg-bubble-{orig_idx}")))
                        .role(Role::Button)
                        .test_support()
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
                    div()
                        .id(SharedString::from(format!("msg-bubble-{orig_idx}")))
                        .role(Role::Button)
                        .test_support()
                        .w_full()
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

                let mut msg_wrapper = v_flex()
                    .w_full()
                    .when(!self.settings.shell_fullscreen, |el| el.max_w(px(760.0)))
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

        // Hard Parity Invariant: busy state = ONLY 3 ticking dots.
        if is_working {
            let dot = |idx: u8| {
                div()
                    .text_xs()
                    .text_color(if busy_phase == idx {
                        theme::text_primary()
                    } else {
                        theme::text_muted()
                    })
                    .child(lucide_icon(icons::DOT, 12.0))
            };
            msg_col = msg_col.child(
                h_flex()
                    .id("chat-busy-dots")
                    .flex_shrink_0()
                    .test_support()
                    .w_full()
                    .when(!self.settings.shell_fullscreen, |el| el.max_w(px(760.0)))
                    .mx_auto()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .items_center()
                    .child(dot(0))
                    .child(dot(1))
                    .child(dot(2)),
            );
        }

        v_flex()
            .id("chat-panel-root")
            .when_some(self.chat_state.focus_handle.as_ref(), |el, fh| {
                el.track_focus(fh)
            })
            .flex_1()
            .h_full()
            .min_w_0()
            .capture_action(cx.listener(|this, _: &input::IndentInline, window, cx| {
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
                if this.prompt_input.read(cx).value().is_empty() {
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
                    let msgs = this.messages.clone();
                    this.chat_state.update_find_query(next, &msgs, this.show_tool_calls());
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
            .capture_action(cx.listener(|this, _: &input::DeleteToBeginningOfLine, window, cx| {
                if this.has_higher_priority_modal() || this.chat_state.find_open {
                    cx.stop_propagation();
                    return;
                }
                this.schedule_slash_picker_sync(window, cx);
            }))
            .capture_action(cx.listener(|this, _: &input::DeleteToEndOfLine, window, cx| {
                if this.has_higher_priority_modal() || this.chat_state.find_open {
                    cx.stop_propagation();
                    return;
                }
                this.schedule_slash_picker_sync(window, cx);
            }))
            .capture_action(cx.listener(|this, _: &input::DeleteToPreviousWordStart, window, cx| {
                if this.has_higher_priority_modal() || this.chat_state.find_open {
                    cx.stop_propagation();
                    return;
                }
                this.schedule_slash_picker_sync(window, cx);
            }))
            .capture_action(cx.listener(|this, _: &input::DeleteToNextWordEnd, window, cx| {
                if this.has_higher_priority_modal() || this.chat_state.find_open {
                    cx.stop_propagation();
                    return;
                }
                this.schedule_slash_picker_sync(window, cx);
            }))
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
            .on_drop::<ExternalPaths>(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.stage_dropped_paths(paths.paths(), window, cx);
            }))
            .when(self.top_floating_bar_rendered(), |el| {
                el.child(self.render_top_floating_bar(header_title, cx))
            })
            .when_some(self.render_todo_header_and_drawer(cx), |el, todo_ui| {
                el.child(todo_ui)
            })
            .when_some(self.render_find_bar(cx), |el, bar| el.child(bar))
            .child(
                v_flex()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(msg_col)
                    .when_some(self.render_message_action_menu(cx), |el, menu| el.child(menu))
                    .when(
                        !self.messages.is_empty()
                            && (self.chat_state.scroll.scrolled_up
                                || self.chat_state.scroll.new_messages_while_scrolled_up > 0),
                        |el| {
                            let count = self.chat_state.scroll.new_messages_while_scrolled_up;
                            el.child(
                                div()
                                    .id("chat-jump-to-bottom-btn")
                                    .role(Role::Button)
                                    .test_support()
                                    .absolute()
                                    .bottom_4()
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
            )
            .when_some(self.notice.clone(), |el, note| {
                el.child(
                    div()
                        .w_full()
                        .when(!self.settings.shell_fullscreen, |el| el.max_w(px(760.0)))
                        .mx_auto()
                        .px_4()
                        .pb_1()
                        .text_xs()
                        .text_color(theme::text_muted())
                        .child(note),
                )
            })
            .when_some(self.chat_state.reply_quote.clone(), |el, quote| {
                el.child(
                    div()
                        .w_full()
                        .when(!self.settings.shell_fullscreen, |el| el.max_w(px(760.0)))
                        .mx_auto()
                        .px_5()
                        .pb_1()
                        .child(
                            h_flex()
                                .id("reply-quote-banner")
                                .w_full()
                                .px_3()
                                .py_1p5()
                                .justify_between()
                                .items_center()
                                .rounded(RADIUS_ROW)
                                .bg(theme::bg_surface())
                                .border_1()
                                .border_color(theme::border_subtle())
                                .border_l_2()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme::text_secondary())
                                        .truncate()
                                        .child(format!("Replying to: {}", quote.preview)),
                                )
                                .child(
                                    div()
                                        .id("dismiss-quote-btn")
                                        .role(Role::Button)
                                        .test_support()
                                        .px_2()
                                        .py_0p5()
                                        .rounded(RADIUS_CHIP)
                                        .hover(|s| s.bg(theme::bg_hover()))
                                        .cursor_pointer()
                                        .text_xs()
                                        .text_color(theme::text_muted())
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(|this, _, _, cx| {
                                                cx.stop_propagation();
                                                this.clear_reply_quote(cx);
                                            }),
                                        )
                                        .child(lucide_icon(icons::X, 12.0)),
                                ),
                        ),
                )
            })
            .when(!self.chat_state.pending_attachments.is_empty(), |el| {
                let mut chips = h_flex()
                    .w_full()
                    .when(!self.settings.shell_fullscreen, |el| el.max_w(px(760.0)))
                    .mx_auto()
                    .px_5()
                    .pb_1()
                    .gap_1p5()
                    .flex_wrap();
                for (att_idx, att) in self.chat_state.pending_attachments.iter().enumerate() {
                    let name = att
                        .path
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "attachment".to_string());
                    let badge = if att.is_image { "img" } else { "file" };
                    chips = chips.child(
                        h_flex()
                            .id(SharedString::from(format!("staged-att-{att_idx}")))
                            .gap_1p5()
                            .items_center()
                            .px_2p5()
                            .py_1()
                            .rounded(RADIUS_CHIP)
                            .bg(theme::bg_surface())
                            .border_1()
                            .border_color(theme::border_subtle())
                            .text_xs()
                            .text_color(theme::text_secondary())
                            .child(format!("[{badge}] {name}"))
                            .child(
                                div()
                                    .id(SharedString::from(format!("remove-att-{att_idx}")))
                                    .role(Role::Button)
                                    .test_support()
                                    .cursor_pointer()
                                    .text_color(theme::text_muted())
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, _, cx| {
                                            cx.stop_propagation();
                                            if att_idx < this.chat_state.pending_attachments.len() {
                                                this.chat_state.pending_attachments.remove(att_idx);
                                                cx.notify();
                                            }
                                        }),
                                    )
                                    .child(lucide_icon(icons::X, 12.0)),
                            ),
                    );
                }
                el.child(chips)
            })
            .when(lifecycle != multibot_core::workspaces::SessionLifecycle::Active, |el| el.child(
                h_flex().id("lifecycle-peek-banner").test_support().w_full().when(!self.settings.shell_fullscreen, |el| el.max_w(px(760.0))).mx_auto()
                    .px_4().py_2().gap_3().items_center().justify_between().rounded(RADIUS_ROW).bg(theme::bg_surface())
                    .child(div().text_xs().text_color(theme::text_muted()).child(
                        if lifecycle == multibot_core::workspaces::SessionLifecycle::Dissolved {
                            "Dissolved · Saved history · Agent stopped. Sending a prompt reactivates this chat."
                        } else { "Reading only · This chat stays out of Inbox until you move it back or send a message." }
                    ))
                    .child(div().id("move-chat-to-inbox").role(Role::Button).test_support().px_3().py_1p5()
                        .rounded(RADIUS_CHIP).bg(theme::bg_active_row()).cursor_pointer()
                        .child(if lifecycle == multibot_core::workspaces::SessionLifecycle::Dissolved { "Reactivate" } else { "Move to Inbox" })
                        .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                            if let Some(session) = &this.selected_session {
                                let key = this.session_key_for(session);
                                this.reactivate_session_key(&key, cx);
                            }
                        })))
            ))
            .when(!queued_previews.is_empty() || !queued_entries.is_empty(), |el| el.child(div().w_full().px_5().pt_2().child(queue_bar)))
            .when_some(self.render_slash_picker(cx), |el, picker| el.child(picker))
            .when_some(self.render_mention_picker(cx), |el, picker| el.child(picker))
            .when(self.chat_state.composer_menu_open, |el| el.child(
                div().w_full().when(!self.settings.shell_fullscreen, |el| el.max_w(px(760.0))).mx_auto().px_5().child(
                    self.render_composer_options(cx)
                )
            ))
            .child(
                div().w_full().flex_shrink_0().px_5().pb_4().pt_2().child(
                    h_flex()
                        .w_full()
                        .when(!self.settings.shell_fullscreen, |el| el.max_w(px(760.0)))
                        .mx_auto()
                        .px_3()
                        .py_1p5()
                        .gap_2()
                        .items_end()
                        .rounded(px(20.0))
                        .bg(theme::bg_surface())
                        .border_1()
                        .border_color(theme::border_subtle())
                        .child(
                            div()
                                .id("attach-file-btn")
                                .role(Role::Button)
                                .test_support()
                                .w(px(28.0))
                                .h(px(28.0))
                                .rounded(RADIUS_CHIP)
                                .bg(theme::bg_hover())
                                .hover(|s| s.bg(theme::bg_active_row()))
                                .cursor_pointer()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_sm()
                                .text_color(theme::text_secondary())
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.chat_state.composer_menu_open = !this.chat_state.composer_menu_open;
                                        cx.notify();
                                    }),
                                )
                                .child(lucide_icon(icons::PLUS, 16.0)),
                        )
                        .when(!options_summary.is_empty(), |el| el.child(
                            div().id("composer-options-summary").test_support().text_xs()
                                .text_color(theme::text_muted()).child(options_summary)
                        ))
                        .child(
                            div().id("prompt-input").test_support().flex_1().min_w_0().child(
                                Textarea::new(&self.prompt_input)
                                    .appearance(false),
                            ),
                        )
                        .child(
                            Button::new("composer-voice")
                                .small()
                                .custom(ButtonCustomVariant::new(cx).foreground(rgb(0xA3A3A3).into())
                                    .hover(rgb(0x333333).into()).active(rgb(0x333333).into()))
                                .h(px(32.0))
                                .rounded(RADIUS_CHIP)
                                .bg(rgb(0x262626))
                                .text_color(rgb(0xA3A3A3))
                                .accessibility_label("Voice dictation")
                                .tooltip(self.voice.error.clone().unwrap_or_else(|| "Start / stop dictation · Ctrl+D / Ctrl+Shift+Space · Esc cancels".into()))
                                .when(self.voice.phase == multibot_core::voice::DictationPhase::Off && self.voice.error.is_none(), |el| {
                                    el.w(px(32.0)).icon(lucide_icon(icons::MIC, 16.0))
                                })
                                .when(self.voice.phase != multibot_core::voice::DictationPhase::Off || self.voice.error.is_some(), |el| {
                                    el.px_3().py_1().child(
                                        h_flex().id("dictation-status").test_support().gap(px(6.0)).items_center().text_xs()
                                            .text_color(rgb(0xF5F5F5))
                                            .when(self.voice.phase == multibot_core::voice::DictationPhase::Listening, |el| el
                                                .child(div().id("dictation-stop").test_support().size(px(9.0)).rounded(px(2.0)).bg(rgb(0xF5F5F5)))
                                                .child(div().id("dictation-timer").test_support().font_family("monospace").child(Label::new(self.voice.elapsed_label()).text_color(rgb(0xF5F5F5))))
                                                .child(div().child(Label::new("·····").text_color(rgb(0x737373)))))
                                            .when(self.voice.phase == multibot_core::voice::DictationPhase::Finishing, |el| el.child("Transcribing..."))
                                            .when(self.voice.error.is_some(), |el| el.text_color(theme::accent_amber()).child("Mic unavailable")),
                                    )
                                })
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.toggle_dictation(window, cx);
                                })),
                        )
                        .when(is_working, |el| el.child(
                            Button::new("stop-agent").small().label("Stop")
                                .tooltip("Stop response")
                                .on_click(cx.listener(|this, _, _, cx| { this.interrupt_active_session(cx); })),
                        ))
                        .child(
                            Button::new("send-prompt")
                                .small()
                                .custom(ButtonCustomVariant::new(cx).foreground(rgb(0xFFFFFF).into())
                                    .hover(rgb(0xB56E3B).into()).active(rgb(0xB56E3B).into()))
                                .size(px(32.0))
                                .rounded(RADIUS_CHIP)
                                .bg(rgb(0xA66335))
                                .text_color(rgb(0xFFFFFF))
                                .icon(lucide_icon(icons::ARROW_UP, 16.0))
                                .accessibility_label(if is_working { "Queue" } else { "Send" })
                                .tooltip(if is_working { "Queue message · Ctrl+Enter" } else { "Send message · Ctrl+Enter" })
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.send_current_prompt(window, cx);
                                })),
                        ),
                ),
            )
            // Slice 5 — built-in terminal pane splits the chat column; the pane is mounted
            // only while open, so a closed pane adds no layout node and no PTY.
            .when_some(self.render_terminal_pane(cx), |el, pane| el.child(pane))
    }
}

#[cfg(test)]
#[path = "chat_view_tests.rs"]
mod tests;
