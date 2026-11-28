use super::*;

impl MultibotView {
    /// Sends an idle session's prompt, or persists it in the shared FIFO while busy or offline.
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
        if let Ok(mut shared) = self.shared_chat_outbox.lock() {
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
                if host == self.active_session_host()
                    && self.selected_session.as_ref().is_some_and(|selected| {
                        selected.id == session.id && selected.agent == session.agent
                    })
                {
                    self.last_dispatched_prompt = Some((msg_id, text));
                }
                return;
            }
        }

        let enqueue_res = if let Ok(mut shared) = self.shared_chat_outbox.lock() {
            let res = shared.outbox.enqueue_with_options(
                &outbox_key,
                &text,
                now_unix_ms(),
                options.clone(),
            );
            if res.is_ok() {
                if shared.persist_to_disk || self.persist_outbox_to_disk {
                    let _ = shared.outbox.save();
                }
                self.chat_outbox = shared.outbox.clone();
            }
            res
        } else {
            let res = self.chat_outbox.enqueue_with_options(
                &outbox_key,
                &text,
                now_unix_ms(),
                options.clone(),
            );
            if res.is_ok() && self.persist_outbox_to_disk {
                let _ = self.chat_outbox.save();
            }
            res
        };
        match enqueue_res {
            Ok(entry) => {
                self.outbox_msg_map.insert(msg_id, entry.id);
                self.flush_outbox_for_host(host);
            }
            Err(err) => {
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
        if let Some(outbox_id) = self.outbox_msg_map.remove(msg_id) {
            if let Ok(mut shared) = self.shared_chat_outbox.lock() {
                shared.outbox.remove_by_id(&outbox_id);
                if shared.persist_to_disk || self.persist_outbox_to_disk {
                    let _ = shared.outbox.save();
                }
                self.chat_outbox = shared.outbox.clone();
            } else {
                self.chat_outbox.remove_by_id(&outbox_id);
                if self.persist_outbox_to_disk {
                    let _ = self.chat_outbox.save();
                }
            }
        }
    }

    pub(crate) fn flush_outbox_for_current_host(&mut self) -> usize {
        let host = self.current_host_label().to_string();
        self.flush_outbox_for_host(&host)
    }

    pub(crate) fn flush_outbox_for_host(&mut self, host: &str) -> usize {
        let Some(tx) = self.tx_for_host(host).cloned() else {
            return 0;
        };
        let Ok(mut shared) = self.shared_chat_outbox.lock() else {
            return 0;
        };
        let keys: std::collections::HashSet<_> = shared
            .outbox
            .entries
            .iter()
            .map(|entry| entry.session_key.clone())
            .collect();
        let mut flushed = 0;
        for outbox_key in keys {
            let Some((entry_host, agent, sid)) = parse_outbox_session_key(&outbox_key) else {
                continue;
            };
            if entry_host != host {
                continue;
            }
            let session = if self.host_statuses.is_empty() {
                self.sessions
                    .iter()
                    .find(|s| s.id == sid && s.agent == agent)
            } else {
                self.host_sessions
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
            if (shared.persist_to_disk || self.persist_outbox_to_disk)
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
            flushed += 1;
        }
        if flushed > 0 && (shared.persist_to_disk || self.persist_outbox_to_disk) {
            let _ = shared.outbox.save();
        }
        self.outbox_msg_map
            .retain(|_, id| shared.outbox.entries.iter().any(|e| &e.id == id));
        self.chat_outbox = shared.outbox.clone();
        flushed
    }

    pub(super) fn handle_outbox_event(&mut self, host: &str, event: &ServerEvent) {
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
            _ => return,
        };
        for (id, agent, working) in updates {
            let agent = agent
                .or_else(|| {
                    self.host_sessions
                        .iter()
                        .find(|(h, s)| h == host && s.id == id)
                        .map(|(_, s)| s.agent)
                })
                .or_else(|| self.sessions.iter().find(|s| s.id == id).map(|s| s.agent));
            let Some(agent) = agent else {
                continue;
            };
            for (h, session) in &mut self.host_sessions {
                if h == host && session.id == id && session.agent == agent {
                    session.working = working;
                }
            }
            if self.host_statuses.is_empty() {
                for session in &mut self.sessions {
                    if session.id == id && session.agent == agent {
                        session.working = working;
                    }
                }
            }
            if self.active_session_host() == host
                && let Some(session) = self.selected_session.as_mut()
                && session.id == id
                && session.agent == agent
            {
                session.working = working;
            }
        }
        self.flush_outbox_for_host(host);
    }

    pub(crate) fn remove_queued_entry(
        &mut self,
        id: &str,
    ) -> Option<multibot_core::outbox::OutboxEntry> {
        let mut shared = self.shared_chat_outbox.lock().ok()?;
        let entry = shared.outbox.remove_by_id(id)?;
        if shared.persist_to_disk || self.persist_outbox_to_disk {
            if let Err(err) = shared.outbox.save() {
                self.notice = Some(err.to_string());
            }
        }
        self.chat_outbox = shared.outbox.clone();
        let message_ids: Vec<_> = self
            .outbox_msg_map
            .iter()
            .filter(|(_, entry_id)| entry_id.as_str() == id)
            .map(|(id, _)| id.clone())
            .collect();
        self.messages
            .retain(|message| !message_ids.contains(&message.id));
        self.outbox_msg_map.retain(|_, entry_id| entry_id != id);
        Some(entry)
    }

    pub(crate) fn force_send_queued_entry(&mut self, id: &str, cx: &mut Context<Self>) {
        let entry = self
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
        let Ok(mut shared) = self.shared_chat_outbox.lock() else {
            return;
        };
        let Some(entry) = shared.outbox.remove_by_id(id) else {
            return;
        };
        shared
            .outbox
            .mark_dispatched(&entry.session_key, entry.id.clone());
        if (shared.persist_to_disk || self.persist_outbox_to_disk)
            && let Err(err) = shared.outbox.save()
        {
            shared.outbox.dispatching.remove(&entry.session_key);
            shared.outbox.entries.insert(0, entry);
            self.chat_outbox = shared.outbox.clone();
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
                    cwd: session.cwd,
                    text: entry.text.clone(),
                    options: entry.options.clone(),
                })
                .is_ok();
        if !sent {
            shared.outbox.dispatching.remove(&entry.session_key);
            shared.outbox.entries.insert(0, entry);
            self.notice = Some("Could not force send. Message remains queued.".into());
        }
        if shared.persist_to_disk || self.persist_outbox_to_disk {
            if let Err(err) = shared.outbox.save() {
                self.notice = Some(err.to_string());
            }
        }
        self.chat_outbox = shared.outbox.clone();
        cx.notify();
    }
}
