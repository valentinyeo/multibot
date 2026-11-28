use super::*;

/// Lossless parser for `ChatOutbox::session_key` (`"{host_len}:{host}|{agent}|{sid_len}:{sid}"`).
pub(crate) fn parse_outbox_session_key(key: &str) -> Option<(&str, AgentKind, &str)> {
    let (host_len_str, rest) = key.split_once(':')?;
    let host_len: usize = host_len_str.parse().ok()?;
    let host = rest.get(..host_len)?;
    let after_host = rest.get(host_len..)?.strip_prefix('|')?;
    let (agent_str, after_agent) = after_host.split_once('|')?;
    let agent = match agent_str {
        "claude" => AgentKind::Claude,
        "antigravity" => AgentKind::Antigravity,
        "codex" => AgentKind::Codex,
        "shell" => AgentKind::Shell,
        "pi" => AgentKind::Pi,
        "cursor" => AgentKind::Cursor,
        _ => return None,
    };
    let (sid_len_str, sid) = after_agent.split_once(':')?;
    let sid_len: usize = sid_len_str.parse().ok()?;
    if sid.len() != sid_len {
        return None;
    }
    Some((host, agent, sid))
}

impl MultibotView {
    /// Sends an idle session's prompt, or persists it in the shared FIFO while busy or offline.
    #[cfg(test)]
    pub(crate) fn send_or_queue_prompt(
        &mut self,
        session: &SessionSummary,
        msg_id: String,
        text: String,
    ) {
        let host = self.active_session_host().to_string();
        self.send_or_queue_prompt_on_host(&host, session, msg_id, text);
    }

    pub(crate) fn send_or_queue_prompt_on_host(
        &mut self,
        host: &str,
        session: &SessionSummary,
        msg_id: String,
        text: String,
    ) {
        let key = canonical_session_key(host, session.agent, &session.id);
        let options = self
            .chat_state
            .composer_options
            .get(&key)
            .filter(|options| !options.is_default())
            .cloned();
        let outbox_key = ChatOutbox::session_key(host, session.agent, &session.id);
        let shared_outbox = self.outbox.shared_chat_outbox.clone();
        if let Ok(mut shared) = shared_outbox.lock() {
            let busy = session.working || shared.outbox.dispatching.contains_key(&outbox_key);
            if !busy
                && shared.outbox.count_for_session(&outbox_key) == 0
                && let Some(tx) = self.tx_for_host(host)
                && tx
                    .send(ClientRequest::SendPrompt {
                        session_id: session.id.clone(),
                        agent: session.agent,
                        cwd: session.cwd.clone(),
                        text: text.clone(),
                        options: options.clone(),
                    })
                    .is_ok()
            {
                shared.outbox.mark_dispatched(&outbox_key, msg_id.clone());
                drop(shared);
                self.record_pending_sent_message(host, session, msg_id, text);
                return;
            }
        }

        let enqueue_res = if let Ok(mut shared) = self.outbox.shared_chat_outbox.lock() {
            let res = shared.outbox.enqueue_with_options(
                &outbox_key,
                &text,
                now_unix_ms(),
                options.clone(),
            );
            if res.is_ok() {
                if shared.persist_to_disk || self.outbox.persist_outbox_to_disk {
                    let _ = shared.outbox.save();
                }
                self.outbox.chat_outbox = shared.outbox.clone();
            }
            res
        } else {
            let res = self.outbox.chat_outbox.enqueue_with_options(
                &outbox_key,
                &text,
                now_unix_ms(),
                options.clone(),
            );
            if res.is_ok() && self.outbox.persist_outbox_to_disk {
                let _ = self.outbox.chat_outbox.save();
            }
            res
        };
        match enqueue_res {
            Ok(entry) => {
                self.outbox.outbox_msg_map.insert(msg_id, entry.id);
                self.flush_outbox_for_host(host);
            }
            Err(err) => {
                if self
                    .selection
                    .selected_session
                    .as_ref()
                    .is_some_and(|selected| self.session_key_for(selected) == key)
                {
                    self.history.messages.push(ChatMessage {
                        id: msg_id.clone(),
                        role: MessageRole::User,
                        text: text.clone(),
                        ..Default::default()
                    });
                }
                self.chat_state.failed_sends.insert(
                    msg_id.clone(),
                    FailedSend {
                        message_id: msg_id,
                        text,
                        reason: err.to_string(),
                    },
                );
            }
        }
    }

    pub(crate) fn cancel_outbox_message(&mut self, msg_id: &str) {
        if let Some(outbox_id) = self.outbox.outbox_msg_map.remove(msg_id) {
            if let Ok(mut shared) = self.outbox.shared_chat_outbox.lock() {
                shared.outbox.remove_by_id(&outbox_id);
                if shared.persist_to_disk || self.outbox.persist_outbox_to_disk {
                    let _ = shared.outbox.save();
                }
                self.outbox.chat_outbox = shared.outbox.clone();
            } else {
                self.outbox.chat_outbox.remove_by_id(&outbox_id);
                if self.outbox.persist_outbox_to_disk {
                    let _ = self.outbox.chat_outbox.save();
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn flush_outbox_for_current_host(&mut self) -> usize {
        let host = self.current_host_label().to_string();
        self.flush_outbox_for_host(&host)
    }

    pub(crate) fn flush_outbox_for_host(&mut self, host: &str) -> usize {
        let Some(tx) = self.tx_for_host(host).cloned() else {
            return 0;
        };
        let Ok(mut shared) = self.outbox.shared_chat_outbox.lock() else {
            return 0;
        };
        let keys: std::collections::HashSet<_> = shared
            .outbox
            .entries
            .iter()
            .map(|entry| entry.session_key.clone())
            .collect();
        let mut dispatched = Vec::new();
        let mut flushed = 0;
        for outbox_key in keys {
            let Some((entry_host, agent, sid)) = parse_outbox_session_key(&outbox_key) else {
                continue;
            };
            if entry_host != host {
                continue;
            }
            let session = if self.connection.host_statuses.is_empty() {
                self.connection
                    .sessions
                    .iter()
                    .find(|s| s.id == sid && s.agent == agent)
            } else {
                self.connection
                    .host_sessions
                    .iter()
                    .find(|(h, s)| h == host && s.id == sid && s.agent == agent)
                    .map(|(_, s)| s)
            };
            let Some(session) = session else {
                continue;
            };
            let key = canonical_session_key(host, agent, sid);
            if session.archived
                || self
                    .sidebar_state
                    .workspace_store
                    .session_meta
                    .get(&key)
                    .is_some_and(|meta| meta.dissolved_at_ms.is_some())
            {
                continue;
            }
            let Some(entry) = shared
                .outbox
                .take_next_for_dispatch(&outbox_key, session.working)
            else {
                continue;
            };
            if (shared.persist_to_disk || self.outbox.persist_outbox_to_disk)
                && let Err(err) = shared.outbox.save()
            {
                shared.outbox.dispatching.remove(&outbox_key);
                shared.outbox.entries.insert(0, entry);
                self.notice = Some(err.to_string());
                break;
            }
            if tx
                .send(ClientRequest::SendPrompt {
                    session_id: sid.into(),
                    agent,
                    cwd: session.cwd.clone(),
                    text: entry.text.clone(),
                    options: entry.options.clone(),
                })
                .is_err()
            {
                shared.outbox.dispatching.remove(&outbox_key);
                shared.outbox.entries.insert(0, entry);
                break;
            }
            dispatched.push((session.clone(), entry.text));
            flushed += 1;
        }
        if flushed > 0 && (shared.persist_to_disk || self.outbox.persist_outbox_to_disk) {
            let _ = shared.outbox.save();
        }
        self.outbox
            .outbox_msg_map
            .retain(|_, id| shared.outbox.entries.iter().any(|e| &e.id == id));
        self.outbox.chat_outbox = shared.outbox.clone();
        drop(shared);
        for (session, text) in dispatched {
            let id = self.next_local_message_id();
            self.record_pending_sent_message(host, &session, id, text);
        }
        flushed
    }

    pub(super) fn handle_outbox_event(&mut self, host: &str, event: &ServerEvent) -> bool {
        let updates: Vec<_> = match event {
            ServerEvent::SessionUpdated { session } => {
                vec![(session.id.clone(), Some(session.agent), session.working)]
            }
            ServerEvent::HistoryAppend {
                session_id,
                working: Some(working),
                ..
            } => vec![(session_id.clone(), None, *working)],
            ServerEvent::Sessions { sessions } => sessions
                .iter()
                .map(|s| (s.id.clone(), Some(s.agent), s.working))
                .collect(),
            _ => return false,
        };
        for (id, agent, working) in updates {
            let agent = agent
                .or_else(|| {
                    self.connection
                        .host_sessions
                        .iter()
                        .find(|(h, s)| h == host && s.id == id)
                        .map(|(_, s)| s.agent)
                })
                .or_else(|| {
                    self.connection
                        .sessions
                        .iter()
                        .find(|s| s.id == id)
                        .map(|s| s.agent)
                });
            let Some(agent) = agent else {
                continue;
            };
            for (h, session) in &mut self.connection.host_sessions {
                if h == host && session.id == id && session.agent == agent {
                    session.working = working;
                }
            }
            if self.connection.host_statuses.is_empty() {
                for session in &mut self.connection.sessions {
                    if session.id == id && session.agent == agent {
                        session.working = working;
                    }
                }
            }
            if self.active_session_host() == host
                && let Some(session) = self.selection.selected_session.as_mut()
                && session.id == id
                && session.agent == agent
            {
                session.working = working;
            }
        }
        true
    }

    pub(crate) fn remove_queued_entry(
        &mut self,
        id: &str,
    ) -> Option<multibot_core::outbox::OutboxEntry> {
        let mut shared = self.outbox.shared_chat_outbox.lock().ok()?;
        let entry = shared.outbox.remove_by_id(id)?;
        if shared.persist_to_disk || self.outbox.persist_outbox_to_disk {
            if let Err(err) = shared.outbox.save() {
                self.notice = Some(err.to_string());
            }
        }
        self.outbox.chat_outbox = shared.outbox.clone();
        let message_ids: Vec<_> = self
            .outbox
            .outbox_msg_map
            .iter()
            .filter(|(_, entry_id)| entry_id.as_str() == id)
            .map(|(id, _)| id.clone())
            .collect();
        self.history
            .messages
            .retain(|message| !message_ids.contains(&message.id));
        self.outbox
            .outbox_msg_map
            .retain(|_, entry_id| entry_id != id);
        Some(entry)
    }

    pub(crate) fn force_send_next_queued(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.main_surface != MainSurface::Chat
            || self.active_modal != ActiveModal::None
            || self.table_modal_state.is_open()
            || self.chat_state.image_viewer.is_some()
        {
            return;
        }
        let Some(session) = self.selection.selected_session.as_ref() else {
            return;
        };
        if session.agent == AgentKind::Shell {
            return;
        }
        let key = ChatOutbox::session_key(self.active_session_host(), session.agent, &session.id);
        if let Some(id) = self.first_queued_entry_id(&key) {
            self.force_send_queued_entry(&id, cx);
            return;
        }
        self.send_current_prompt_with_force(true, window, cx);
    }

    pub(crate) fn force_send_first_for_key(&mut self, key: &str, cx: &mut Context<Self>) {
        if let Some(id) = self.first_queued_entry_id(key) {
            self.force_send_queued_entry(&id, cx);
        }
    }

    fn first_queued_entry_id(&self, key: &str) -> Option<String> {
        let shared = self.outbox.shared_chat_outbox.lock().ok();
        let outbox = shared
            .as_ref()
            .map(|s| &s.outbox)
            .unwrap_or(&self.outbox.chat_outbox);
        outbox
            .entries
            .iter()
            .find(|entry| entry.session_key == key)
            .map(|entry| entry.id.clone())
    }

    pub(crate) fn force_send_queued_entry(&mut self, id: &str, cx: &mut Context<Self>) {
        let entry = self
            .outbox
            .shared_chat_outbox
            .lock()
            .ok()
            .and_then(|shared| shared.outbox.entries.iter().find(|e| e.id == id).cloned());
        let Some(entry) = entry else {
            return;
        };
        let Some((host, agent, sid)) = parse_outbox_session_key(&entry.session_key) else {
            return;
        };
        let key = canonical_session_key(host, agent, sid);
        let Some((_, session)) = self.host_for_session_key(&key) else {
            return;
        };
        let Some(tx) = self.tx_for_host(host).cloned() else {
            return;
        };
        let Ok(mut shared) = self.outbox.shared_chat_outbox.lock() else {
            return;
        };
        let Some(entry) = shared.outbox.remove_by_id(id) else {
            return;
        };
        shared
            .outbox
            .mark_dispatched(&entry.session_key, entry.id.clone());
        if (shared.persist_to_disk || self.outbox.persist_outbox_to_disk)
            && let Err(err) = shared.outbox.save()
        {
            shared.outbox.dispatching.remove(&entry.session_key);
            shared.outbox.entries.insert(0, entry);
            self.outbox.chat_outbox = shared.outbox.clone();
            self.notice = Some(err.to_string());
            cx.notify();
            return;
        }
        let sent = tx
            .send(ClientRequest::InterruptSession {
                session_id: sid.into(),
                agent,
            })
            .is_ok()
            && tx
                .send(ClientRequest::SendPrompt {
                    session_id: sid.into(),
                    agent,
                    cwd: session.cwd.clone(),
                    text: entry.text.clone(),
                    options: entry.options.clone(),
                })
                .is_ok();
        if sent {
            self.notice = None;
        } else {
            shared.outbox.dispatching.remove(&entry.session_key);
            shared.outbox.entries.insert(0, entry.clone());
            self.notice = Some("Could not force send. Message remains queued.".into());
        }
        if shared.persist_to_disk || self.outbox.persist_outbox_to_disk {
            if let Err(err) = shared.outbox.save() {
                self.notice = Some(err.to_string());
            }
        }
        self.outbox.chat_outbox = shared.outbox.clone();
        drop(shared);
        if sent {
            self.outbox
                .outbox_msg_map
                .retain(|_, entry_id| entry_id != id);
            let msg_id = self.next_local_message_id();
            self.record_pending_sent_message(host, &session, msg_id, entry.text);
        }
        cx.notify();
    }
}
