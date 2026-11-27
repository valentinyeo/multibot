use crate::workspaces::UnreadBoundary;
use multibot_proto::{ChatMessage, MessageRole};

pub const NEAR_BOTTOM_PX: f32 = 48.0;
pub const DOUBLE_G_WINDOW_MS: u64 = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PendingScrollTarget {
    #[default]
    None,
    Bottom,
    UnreadDivider(usize),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum UnreadVisit {
    #[default]
    Read,
    Unread {
        previous_boundary: Option<UnreadBoundary>,
    },
}

pub fn find_first_unread_message_idx(
    messages: &[ChatMessage],
    previous_boundary: Option<&UnreadBoundary>,
    was_unread: bool,
) -> Option<usize> {
    let manual = previous_boundary.is_some_and(|boundary| boundary.manual_unread == Some(true));
    if !was_unread && !manual {
        return None;
    }
    if let Some(boundary) = previous_boundary {
        if let Some(id) = &boundary.last_read_assistant_id
            && let Some(i) = messages.iter().position(|msg| msg.id == *id)
        {
            return if i + 1 < messages.len() {
                Some(i + 1)
            } else {
                manual.then_some(0)
            };
        }
        if boundary.last_read_ms > 0
            && let Some(i) = messages.iter().enumerate().find_map(|(i, msg)| {
                (i > 0
                    && msg
                        .timestamp_ms
                        .is_some_and(|ts| ts > boundary.last_read_ms))
                .then_some(i)
            })
        {
            return Some(i);
        }
    }
    if manual {
        return (!messages.is_empty()).then_some(0);
    }
    if let Some(i) = messages
        .iter()
        .rposition(|msg| msg.role == MessageRole::User)
    {
        messages
            .iter()
            .enumerate()
            .skip(i + 1)
            .find_map(|(i, msg)| (msg.role == MessageRole::Assistant).then_some(i))
    } else {
        messages
            .iter()
            .rposition(|msg| msg.role == MessageRole::Assistant)
    }
}

#[derive(Debug, Clone)]
pub struct ChatScrollTracker {
    pub follow_bottom: bool,
    pub pending_scroll: PendingScrollTarget,
    pub scrolled_up: bool,
    pub new_messages_while_scrolled_up: usize,
    pub unread_visit: UnreadVisit,
    pub unread_divider_msg_idx: Option<usize>,
}

impl Default for ChatScrollTracker {
    fn default() -> Self {
        Self {
            follow_bottom: true,
            pending_scroll: PendingScrollTarget::Bottom,
            scrolled_up: false,
            new_messages_while_scrolled_up: 0,
            unread_visit: UnreadVisit::Read,
            unread_divider_msg_idx: None,
        }
    }
}

impl ChatScrollTracker {
    pub fn open_visit(&mut self, unread_visit: UnreadVisit) {
        *self = Self {
            unread_visit,
            ..Self::default()
        };
    }

    pub fn history_loaded(&mut self, messages: &[ChatMessage], cached: bool) {
        if let UnreadVisit::Unread { previous_boundary } = &self.unread_visit {
            self.unread_divider_msg_idx =
                find_first_unread_message_idx(messages, previous_boundary.as_ref(), true);
            self.pending_scroll = self
                .unread_divider_msg_idx
                .map(PendingScrollTarget::UnreadDivider)
                .unwrap_or(PendingScrollTarget::Bottom);
            self.follow_bottom = self.unread_divider_msg_idx.is_none();
            if !cached {
                self.unread_visit = UnreadVisit::Read;
            }
        } else if self.follow_bottom {
            self.pending_scroll = PendingScrollTarget::Bottom;
        }
    }

    pub fn sent_message(&mut self) {
        self.unread_divider_msg_idx = None;
        self.jump_to_bottom();
    }

    pub fn jump_to_bottom(&mut self) {
        self.unread_visit = UnreadVisit::Read;
        self.follow_bottom = true;
        self.scrolled_up = false;
        self.new_messages_while_scrolled_up = 0;
        self.pending_scroll = PendingScrollTarget::Bottom;
    }

    pub fn sync_position(&mut self, distance_from_bottom: f32) -> bool {
        let scrolled_up = distance_from_bottom > NEAR_BOTTOM_PX;
        let changed = self.scrolled_up != scrolled_up
            || self.follow_bottom == scrolled_up
            || (!scrolled_up && self.new_messages_while_scrolled_up != 0);
        self.scrolled_up = scrolled_up;
        self.follow_bottom = !scrolled_up;
        if !scrolled_up {
            self.new_messages_while_scrolled_up = 0;
        }
        changed
    }

    pub fn appended(&mut self, added_messages: usize) {
        if self.follow_bottom {
            self.pending_scroll = PendingScrollTarget::Bottom;
            self.new_messages_while_scrolled_up = 0;
        } else {
            self.new_messages_while_scrolled_up += added_messages;
        }
    }

    pub fn prepended(&mut self, added_messages: usize) {
        if let Some(i) = &mut self.unread_divider_msg_idx {
            *i += added_messages;
        }
        if let PendingScrollTarget::UnreadDivider(i) = &mut self.pending_scroll {
            *i += added_messages;
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct NewestChord {
    first_ms: Option<u64>,
}

impl NewestChord {
    pub fn reset(&mut self) {
        self.first_ms = None;
    }

    pub fn on_char(
        &mut self,
        ch: char,
        composer_text_before: &str,
        now_ms: u64,
        shift_only: bool,
        is_held: bool,
    ) -> bool {
        let first = self.first_ms.take();
        if ch != 'G' || !shift_only || is_held {
            return false;
        }
        if composer_text_before.is_empty() {
            self.first_ms = Some(now_ms);
            return false;
        }
        composer_text_before == "G"
            && first.is_some_and(|start| now_ms.saturating_sub(start) <= DOUBLE_G_WINDOW_MS)
    }

    pub fn on_transcript_g(&mut self, now_ms: u64, is_held: bool) -> bool {
        let first = self.first_ms.take();
        if is_held {
            return false;
        }
        if first.is_some_and(|start| now_ms.saturating_sub(start) <= DOUBLE_G_WINDOW_MS) {
            true
        } else {
            self.first_ms = Some(now_ms);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn messages() -> Vec<ChatMessage> {
        (0..4)
            .map(|i| ChatMessage {
                id: format!("m{i}"),
                timestamp_ms: Some(i * 100),
                ..Default::default()
            })
            .collect()
    }

    #[test]
    fn read_chats_open_at_bottom() {
        let messages = messages();
        let boundary = UnreadBoundary {
            last_read_assistant_id: Some("m1".into()),
            ..Default::default()
        };
        assert_eq!(
            find_first_unread_message_idx(&messages, Some(&boundary), false),
            None
        );
        assert_eq!(find_first_unread_message_idx(&messages, None, false), None);
        let mut scroll = ChatScrollTracker::default();
        scroll.open_visit(UnreadVisit::Read);
        scroll.history_loaded(&messages, false);
        assert_eq!(scroll.unread_divider_msg_idx, None);
        assert_eq!(scroll.pending_scroll, PendingScrollTarget::Bottom);
        assert!(scroll.follow_bottom);
    }

    fn assistant_turn_messages() -> Vec<ChatMessage> {
        use multibot_proto::MessageRole::{Assistant, User};
        [
            ("user1", User),
            ("assistant1", Assistant),
            ("user2", User),
            ("assistant2", Assistant),
            ("assistant3", Assistant),
        ]
        .into_iter()
        .map(|(id, role)| ChatMessage {
            id: id.into(),
            role,
            ..Default::default()
        })
        .collect()
    }

    #[test]
    fn unread_without_a_saved_boundary_starts_at_the_latest_assistant_turn() {
        let messages = assistant_turn_messages();
        assert_eq!(
            find_first_unread_message_idx(&messages, None, true),
            Some(3)
        );
        let mut scroll = ChatScrollTracker::default();
        scroll.open_visit(UnreadVisit::Unread {
            previous_boundary: None,
        });
        scroll.history_loaded(&messages, true);
        assert_eq!(scroll.unread_divider_msg_idx, Some(3));
        assert_eq!(scroll.pending_scroll, PendingScrollTarget::UnreadDivider(3));
        assert!(!scroll.follow_bottom);
        assert!(matches!(scroll.unread_visit, UnreadVisit::Unread { .. }));
        scroll.history_loaded(&messages, false);
        assert_eq!(scroll.pending_scroll, PendingScrollTarget::UnreadDivider(3));
        assert_eq!(scroll.unread_visit, UnreadVisit::Read);
        scroll.pending_scroll = PendingScrollTarget::None;
        scroll.history_loaded(&messages, false);
        assert_eq!(scroll.pending_scroll, PendingScrollTarget::None);
        assert_eq!(scroll.unread_divider_msg_idx, Some(3));
    }

    #[test]
    fn returning_unread_chat_starts_after_the_last_read_assistant() {
        let mut scroll = ChatScrollTracker::default();
        scroll.open_visit(UnreadVisit::Unread {
            previous_boundary: Some(UnreadBoundary {
                last_read_assistant_id: Some("assistant1".into()),
                ..Default::default()
            }),
        });
        scroll.history_loaded(&assistant_turn_messages(), false);
        assert_eq!(scroll.unread_divider_msg_idx, Some(2));
        assert_eq!(scroll.pending_scroll, PendingScrollTarget::UnreadDivider(2));
        assert!(!scroll.follow_bottom);
    }

    #[test]
    fn missing_boundary_falls_back_to_latest_turn_or_last_assistant_without_a_user() {
        let messages = assistant_turn_messages();
        let missing = UnreadBoundary {
            last_read_assistant_id: Some("truncated".into()),
            last_read_ms: 100,
            ..Default::default()
        };
        assert_eq!(
            find_first_unread_message_idx(&messages, Some(&missing), true),
            Some(3)
        );
        let assistants: Vec<_> = messages
            .into_iter()
            .filter(|msg| msg.role == MessageRole::Assistant)
            .collect();
        assert_eq!(
            find_first_unread_message_idx(&assistants, None, true),
            Some(2)
        );
        assert_eq!(
            find_first_unread_message_idx(&assistants, None, false),
            None
        );
        assert_eq!(find_first_unread_message_idx(&[], None, true), None);
        let users = vec![ChatMessage {
            role: MessageRole::User,
            ..Default::default()
        }];
        assert_eq!(find_first_unread_message_idx(&users, None, true), None);
    }

    #[test]
    fn unread_id_timestamp_and_manual_boundaries() {
        let messages = messages();
        let boundary = UnreadBoundary {
            last_read_assistant_id: Some("m1".into()),
            ..Default::default()
        };
        let mut scroll = ChatScrollTracker::default();
        scroll.open_visit(UnreadVisit::Unread {
            previous_boundary: Some(boundary),
        });
        scroll.history_loaded(&messages, true);
        assert_eq!(scroll.pending_scroll, PendingScrollTarget::UnreadDivider(2));
        assert!(!scroll.follow_bottom);
        assert!(matches!(scroll.unread_visit, UnreadVisit::Unread { .. }));
        scroll.history_loaded(&messages, false);
        assert_eq!(scroll.unread_visit, UnreadVisit::Read);
        scroll.pending_scroll = PendingScrollTarget::None;
        scroll.history_loaded(&messages, false);
        assert_eq!(scroll.pending_scroll, PendingScrollTarget::None);
        assert_eq!(scroll.unread_divider_msg_idx, Some(2));
        let boundary = UnreadBoundary {
            last_read_assistant_id: Some("missing".into()),
            last_read_ms: 100,
            manual_unread: None,
        };
        assert_eq!(
            find_first_unread_message_idx(&messages, Some(&boundary), true),
            Some(2)
        );
        let boundary = UnreadBoundary {
            manual_unread: Some(true),
            ..Default::default()
        };
        assert_eq!(
            find_first_unread_message_idx(&messages, Some(&boundary), false),
            Some(0)
        );
        assert_eq!(
            find_first_unread_message_idx(&[], Some(&boundary), true),
            None
        );
    }

    #[test]
    fn boundary_at_end_is_read_but_unmatched_timestamp_falls_back_to_latest_assistant() {
        let messages = messages();
        let boundary = UnreadBoundary {
            last_read_assistant_id: Some("m3".into()),
            last_read_ms: 1,
            manual_unread: None,
        };
        assert_eq!(
            find_first_unread_message_idx(&messages, Some(&boundary), true),
            None
        );
        let manual = UnreadBoundary {
            manual_unread: Some(true),
            ..boundary.clone()
        };
        assert_eq!(
            find_first_unread_message_idx(&messages, Some(&manual), true),
            Some(0)
        );
        let boundary = UnreadBoundary {
            last_read_ms: 300,
            ..Default::default()
        };
        assert_eq!(
            find_first_unread_message_idx(&messages, Some(&boundary), true),
            Some(3)
        );
        let messages = vec![ChatMessage {
            timestamp_ms: Some(200),
            ..Default::default()
        }];
        let boundary = UnreadBoundary {
            last_read_ms: 100,
            ..Default::default()
        };
        assert_eq!(
            find_first_unread_message_idx(&messages, Some(&boundary), true),
            Some(0)
        );
    }

    #[test]
    fn append_follows_bottom_but_counts_only_new_messages_when_reading_history() {
        let mut scroll = ChatScrollTracker::default();
        scroll.appended(2);
        assert_eq!(scroll.pending_scroll, PendingScrollTarget::Bottom);
        scroll.pending_scroll = PendingScrollTarget::None;
        assert!(scroll.sync_position(49.0));
        scroll.appended(2);
        scroll.appended(0); // A streaming update is not a second message.
        assert_eq!(scroll.pending_scroll, PendingScrollTarget::None);
        assert_eq!(scroll.new_messages_while_scrolled_up, 2);
        assert!(!scroll.sync_position(100.0));
        assert!(scroll.sync_position(48.0));
        assert_eq!(scroll.new_messages_while_scrolled_up, 0);
        scroll.appended(1);
        assert_eq!(scroll.pending_scroll, PendingScrollTarget::Bottom);
    }

    #[test]
    fn send_jump_and_switch_reset_scroll_state() {
        let mut scroll = ChatScrollTracker::default();
        scroll.open_visit(UnreadVisit::Unread {
            previous_boundary: Some(UnreadBoundary {
                manual_unread: Some(true),
                ..Default::default()
            }),
        });
        scroll.history_loaded(&messages(), true);
        scroll.sync_position(200.0);
        scroll.appended(3);
        scroll.sent_message();
        assert_eq!(scroll.unread_divider_msg_idx, None);
        assert_eq!(scroll.unread_visit, UnreadVisit::Read);
        assert_eq!(scroll.pending_scroll, PendingScrollTarget::Bottom);
        assert!(scroll.follow_bottom);
        assert!(!scroll.scrolled_up);
        assert_eq!(scroll.new_messages_while_scrolled_up, 0);
        scroll.sync_position(200.0);
        scroll.appended(2);
        scroll.jump_to_bottom();
        assert!(scroll.follow_bottom);
        assert_eq!(scroll.new_messages_while_scrolled_up, 0);
        scroll.sync_position(200.0);
        scroll.appended(2);
        scroll.open_visit(UnreadVisit::Read);
        assert!(scroll.follow_bottom);
        assert!(!scroll.scrolled_up);
        assert_eq!(scroll.new_messages_while_scrolled_up, 0);
    }

    #[test]
    fn jump_to_bottom_overrides_unread_scroll_before_fresh_history_arrives() {
        let mut scroll = ChatScrollTracker::default();
        let messages = assistant_turn_messages();
        scroll.open_visit(UnreadVisit::Unread {
            previous_boundary: None,
        });
        scroll.history_loaded(&messages, true);
        scroll.jump_to_bottom();
        scroll.history_loaded(&messages, false);
        assert_eq!(scroll.pending_scroll, PendingScrollTarget::Bottom);
        assert_eq!(scroll.unread_divider_msg_idx, Some(3));
        assert_eq!(scroll.unread_visit, UnreadVisit::Read);
        assert!(scroll.follow_bottom);
    }

    #[test]
    fn prepending_keeps_the_unread_cutoff_on_the_same_message() {
        let mut scroll = ChatScrollTracker::default();
        scroll.open_visit(UnreadVisit::Unread {
            previous_boundary: Some(UnreadBoundary {
                last_read_assistant_id: Some("m1".into()),
                ..Default::default()
            }),
        });
        scroll.history_loaded(&messages(), false);
        scroll.prepended(10);
        assert_eq!(scroll.unread_divider_msg_idx, Some(12));
        assert_eq!(
            scroll.pending_scroll,
            PendingScrollTarget::UnreadDivider(12)
        );
    }

    #[test]
    fn shift_gg_preserves_typing_and_handles_transcript_focus() {
        let mut chord = NewestChord::default();
        assert!(!chord.on_char('G', "", 1000, true, false));
        assert!(chord.on_char('G', "G", 1500, true, false));
        assert!(!chord.on_char('G', "", 2000, true, false));
        assert!(!chord.on_char('G', "G", 2501, true, false));
        assert!(!chord.on_char('G', "", 3000, true, false));
        assert!(!chord.on_char('o', "G", 3100, false, false));
        assert!(!chord.on_char('G', "Good", 3200, true, false));
        assert!(!chord.on_char('G', "", 4000, true, false));
        assert!(!chord.on_char('G', "G", 4100, true, true));
        assert!(!chord.on_transcript_g(5000, false));
        assert!(chord.on_transcript_g(5400, false));
        assert!(!chord.on_transcript_g(5500, false));
        chord.reset();
        assert!(!chord.on_transcript_g(5600, false));
        assert!(!chord.on_transcript_g(6200, false));
    }
}
