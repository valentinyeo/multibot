use super::*;

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
