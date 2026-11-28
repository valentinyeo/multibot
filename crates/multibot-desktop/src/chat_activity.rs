use super::*;
use multibot_proto::ServerEvent;
use std::time::Duration;

#[derive(Debug)]
pub(crate) struct AwaitingReply {
    pub host: String,
    pub session_id: String,
    pub message_id: String,
    pub prompt_text: String,
    pub baseline_user_ids: HashSet<String>,
    pub baseline_replies: HashSet<(String, u64)>,
}

fn assistant_replies(messages: &[ChatMessage]) -> impl Iterator<Item = (String, u64)> + '_ {
    messages.iter().filter_map(|m| {
        if m.role != MessageRole::Assistant {
            return None;
        }
        let text = if m.blocks.is_empty() {
            m.text.clone()
        } else {
            m.blocks
                .iter()
                .filter_map(|b| match b {
                    MessageBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        (!text.trim().is_empty()).then(|| (m.id.clone(), fnv1a_64(text.as_bytes())))
    })
}

pub(crate) fn activity_label(messages: &[ChatMessage]) -> String {
    let turn = messages
        .iter()
        .rposition(|m| m.role == MessageRole::User)
        .map_or(messages, |i| &messages[i + 1..]);
    let tools: Vec<_> = turn
        .iter()
        .rev()
        .flat_map(|m| m.blocks.iter().rev())
        .filter_map(|b| match b {
            MessageBlock::ToolUse {
                name,
                summary,
                detail,
                status,
                ..
            } => Some((name, summary, detail, status)),
            _ => None,
        })
        .collect();
    let real_tool =
        |name: &str| !matches!(name.to_ascii_lowercase().as_str(), "system" | "checkpoint");
    let selected = tools
        .iter()
        .find(|(name, _, _, status)| real_tool(name) && **status == ToolStatus::Running)
        .or_else(|| tools.iter().find(|(name, _, _, _)| real_tool(name)))
        .or_else(|| tools.first());
    selected.map_or_else(
        || "Thinking".into(),
        |(name, summary, detail, _)| tool_activity_label(name, summary, detail),
    )
}

fn readable_summary(summary: &str) -> Option<String> {
    let summary = summary.trim();
    (!summary.is_empty()
        && summary.chars().count() <= 48
        && !summary.contains(['{', '}', '[', ']', '/', '\\', '\n', '`'])
        && summary.chars().any(char::is_alphabetic))
    .then(|| summary.to_string())
}

pub(crate) fn tool_activity_label(name: &str, summary: &str, detail: &str) -> String {
    let file_label = |verb: &str| {
        let json = serde_json::from_str::<serde_json::Value>(detail)
            .ok()
            .or_else(|| serde_json::from_str::<serde_json::Value>(summary).ok());
        let path = json
            .as_ref()
            .and_then(|v| {
                [
                    "path",
                    "file_path",
                    "file",
                    "target_file",
                    "AbsolutePath",
                    "TargetFile",
                ]
                .into_iter()
                .find_map(|key| v.get(key).and_then(|v| v.as_str()))
            })
            .or_else(|| {
                (!detail.trim().contains(['{', '\n']) && !detail.trim().contains(' '))
                    .then_some(detail.trim())
            })
            .filter(|s| !s.is_empty())
            .or_else(|| {
                summary.split_whitespace().find(|s| {
                    !s.contains(['{', '}', '[', ']']) && (s.contains('/') || s.contains('\\'))
                })
            });
        let basename = path.map(extract_basename).filter(|s| !s.is_empty());
        match basename {
            Some(file) => format!("{verb} {}", file.chars().take(36).collect::<String>()),
            None => format!("{verb} files"),
        }
    };
    let label = match name.to_ascii_lowercase().as_str() {
        "read_file" | "read" | "view_file" => file_label("Reading"),
        "edit_file" | "edit" | "write_file" | "write" | "multiedit" => file_label("Editing"),
        "run_command" | "bash" | "exec_command" => readable_summary(summary)
            .map(|s| {
                let mut action = s.as_str();
                loop {
                    let stripped = ["Run ", "Ran ", "Running "].into_iter().find_map(|prefix| {
                        action
                            .get(..prefix.len())
                            .filter(|part| part.eq_ignore_ascii_case(prefix))
                            .map(|_| action[prefix.len()..].trim_start())
                    });
                    match stripped {
                        Some(rest) => action = rest,
                        None => break,
                    }
                }
                if action.len() != s.len() {
                    format!("Running {action}")
                } else {
                    s
                }
            })
            .unwrap_or_else(|| "Running command".into()),
        "search_web" | "websearch" | "web_fetch" | "webfetch" => "Searching the web".into(),
        "grep" | "find_by_name" | "glob" | "ripgrep" => "Searching codebase".into(),
        "invoke_subagent" | "agent" | "spawn_agent" => "Running helper agent".into(),
        "thinking" => "Thinking".into(),
        "system" | "checkpoint" => "Updating context".into(),
        _ => readable_summary(summary).unwrap_or_else(|| {
            let clean: String = name
                .chars()
                .map(|c| {
                    if c.is_alphabetic() || c == ' ' {
                        c
                    } else {
                        ' '
                    }
                })
                .collect();
            format!(
                "Using {}",
                clean.split_whitespace().collect::<Vec<_>>().join(" ")
            )
        }),
    };
    label.chars().take(48).collect()
}

impl MultibotView {
    pub(crate) fn is_active_chat_working(&self) -> bool {
        self.is_active_chat_replying()
    }

    pub(crate) fn has_active_subworkers(&self) -> bool {
        self.selection
            .selected_session
            .as_ref()
            .is_some_and(|s| !s.subworkers.is_empty())
    }

    pub(crate) fn is_active_chat_replying(&self) -> bool {
        self.selection
            .selected_session
            .as_ref()
            .is_some_and(|session| {
                let key = self.session_key_for(session);
                session.working
                    || self.chat_state.awaiting_reply_sessions.contains_key(&key)
                    || self
                        .history
                        .pending_sent_messages
                        .get(&key)
                        .is_some_and(|p| p.iter().any(|sent| sent.awaiting_reply))
            })
    }

    pub(crate) fn sync_busy_dots_timer(&mut self, cx: &mut Context<Self>) {
        let key = (self.main_surface == MainSurface::Chat && self.is_active_chat_working())
            .then(|| self.session_key_for(self.selection.selected_session.as_ref().unwrap()));
        if self.chat_state.busy_timer_key == key {
            return;
        }
        self.chat_state.busy_timer = None;
        self.chat_state.busy_timer_key = key.clone();
        self.chat_state.busy_tick_phase = 0;
        let Some(key) = key else {
            return;
        };
        self.chat_state.busy_timer = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(320))
                    .await;
                let keep_running = this
                    .update(cx, |view, cx| {
                        if view.main_surface != MainSurface::Chat
                            || !view.is_active_chat_working()
                            || view.chat_state.busy_timer_key.as_ref() != Some(&key)
                        {
                            view.chat_state.busy_timer_key = None;
                            view.chat_state.busy_timer = None;
                            return false;
                        }
                        view.chat_state.advance_busy_tick();
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !keep_running {
                    break;
                }
            }
        }));
    }

    pub(crate) fn begin_awaiting_reply(
        &mut self,
        key: &str,
        host: &str,
        session: &SessionSummary,
        id: &str,
        text: &str,
        messages: &[ChatMessage],
    ) {
        self.chat_state.turn_notices.remove(key);
        self.chat_state.awaiting_reply_sessions.insert(
            key.into(),
            AwaitingReply {
                host: host.into(),
                session_id: session.id.clone(),
                message_id: id.into(),
                prompt_text: text.into(),
                baseline_user_ids: messages
                    .iter()
                    .filter(|m| m.role == MessageRole::User)
                    .map(|m| m.id.clone())
                    .collect(),
                baseline_replies: assistant_replies(messages).collect(),
            },
        );
    }

    pub(crate) fn observe_turn_snapshot(&mut self, key: &str, messages: &[ChatMessage]) {
        let Some(turn) = self.chat_state.awaiting_reply_sessions.get(key) else {
            return;
        };
        if let Some((index, user)) = messages
            .iter()
            .enumerate()
            .rev()
            .find(|(_, m)| m.role == MessageRole::User)
            && !turn.baseline_user_ids.contains(&user.id)
            && user.text.trim() == turn.prompt_text.trim()
        {
            self.observe_turn_history(key, &messages[index + 1..], None);
        }
    }

    pub(crate) fn observe_turn_history(
        &mut self,
        key: &str,
        messages: &[ChatMessage],
        working: Option<bool>,
    ) {
        if working == Some(true) || assistant_replies(messages).next().is_some() {
            self.chat_state.turn_notices.remove(key);
        }
        let Some(turn) = self.chat_state.awaiting_reply_sessions.get(key) else {
            return;
        };
        if assistant_replies(messages).any(|reply| !turn.baseline_replies.contains(&reply)) {
            self.chat_state.awaiting_reply_sessions.remove(key);
            if let Some(pending) = self.history.pending_sent_messages.get_mut(key) {
                for sent in pending {
                    sent.awaiting_reply = false;
                }
            }
        } else if working == Some(false) && messages.is_empty() {
            // Empty idle appends signal process exit, even if no start event reached this view.
            self.finish_turn_without_reply(key, "No reply received");
        }
    }

    pub(crate) fn finish_turn_without_reply(&mut self, key: &str, notice: &str) {
        self.chat_state.awaiting_reply_sessions.remove(key);
        if let Some(pending) = self.history.pending_sent_messages.get_mut(key) {
            for sent in pending {
                sent.awaiting_reply = false;
            }
        }
        self.chat_state
            .turn_notices
            .insert(key.into(), notice.into());
        if let Some(session) = self.selection.selected_session.as_ref()
            && self.session_key_for(session) == key
        {
            self.selection.selected_session.as_mut().unwrap().working = false;
            self.chat_state.busy_timer = None;
            self.chat_state.busy_timer_key = None;
        }
    }

    pub(crate) fn observe_turn_event(&mut self, host: &str, event: &ServerEvent) {
        if let ServerEvent::Sessions { sessions } = event {
            self.chat_state.turn_notices.retain(|key, _| {
                !sessions
                    .iter()
                    .any(|session| session.working && turn_session_matches(key, host, &session.id))
            });
            return;
        }
        if let ServerEvent::PromptStatus {
            session_id,
            message_id,
            status,
            ..
        } = event
            && matches!(status.as_str(), "failed" | "cancelled")
        {
            let keys: Vec<_> = self
                .chat_state
                .awaiting_reply_sessions
                .iter()
                .filter(|(_, turn)| {
                    turn.host == host
                        && turn.session_id == *session_id
                        && (message_id.is_empty() || turn.message_id == *message_id)
                })
                .map(|(key, _)| key.clone())
                .collect();
            for key in keys {
                self.finish_turn_without_reply(
                    &key,
                    if status == "failed" {
                        "Message delivery failed"
                    } else {
                        "Stopped"
                    },
                );
            }
            return;
        }
        let (session_id, messages, working) = match event {
            ServerEvent::History {
                session_id,
                messages,
                ..
            } => {
                if assistant_replies(messages).next().is_some() {
                    self.chat_state
                        .turn_notices
                        .retain(|key, _| !turn_session_matches(key, host, session_id));
                }
                let keys: Vec<_> = self
                    .chat_state
                    .awaiting_reply_sessions
                    .iter()
                    .filter(|(_, turn)| turn.host == host && turn.session_id == *session_id)
                    .map(|(key, _)| key.clone())
                    .collect();
                for key in keys {
                    self.observe_turn_snapshot(&key, messages);
                }
                return;
            }
            ServerEvent::HistoryAppend {
                session_id,
                messages,
                working,
                ..
            } => (session_id, messages.as_slice(), *working),
            ServerEvent::SessionUpdated { session } if session.working => {
                (&session.id, &[][..], Some(true))
            }
            _ => return,
        };
        if working == Some(true) || assistant_replies(messages).next().is_some() {
            self.chat_state.turn_notices.retain(|key, _| {
                !turn_session_matches(key, host, session_id)
            });
        }
        let keys: Vec<_> = self
            .chat_state
            .awaiting_reply_sessions
            .iter()
            .filter(|(_, turn)| turn.host == host && turn.session_id == *session_id)
            .map(|(key, _)| key.clone())
            .collect();
        for key in keys {
            self.observe_turn_history(&key, messages, working);
        }
    }
}

fn turn_session_matches(key: &str, host: &str, session_id: &str) -> bool {
    matches!(multibot_core::workspaces::parse_session_key(key), Some((h, _, id)) if h == host && id == session_id)
}
