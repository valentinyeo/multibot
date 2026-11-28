use super::*;

impl MultibotView {
    /// Matches and consumes an App-scoped `pending_older_page_before` entry for a `ServerEvent::History`.
    ///
    /// First prunes any stale queued `req_before` entries where the transcript was truncated/rotated
    /// past `req_before` (`file_size < req_before`). Then pops a matching entry (preferring an exact
    /// `lo == req_before` match, falling back to `lo <= req_before`) and returns `true`.
    #[cfg(test)]
    pub(super) fn consume_shared_older_page_hint(cx: &mut App, server_ev: &ServerEvent) -> bool {
        let ServerEvent::History {
            session_id,
            latest_offset,
            file_size,
            ..
        } = server_ev
        else {
            return false;
        };
        if !cx.has_global::<SharedAppStore>() {
            return false;
        }
        Self::consume_older_page_hint(cx, session_id, *latest_offset, *file_size)
    }

    pub(super) fn consume_host_older_page_hint(
        cx: &mut App,
        host: &str,
        event: &ServerEvent,
    ) -> bool {
        let ServerEvent::History {
            session_id,
            latest_offset,
            file_size,
            ..
        } = event
        else {
            return false;
        };
        let key = format!("{}:{host}{session_id}", host.len());
        Self::consume_older_page_hint(cx, &key, *latest_offset, *file_size)
    }

    pub(super) fn consume_older_page_hint(
        cx: &mut App,
        session_id: &str,
        latest_offset: Option<u64>,
        file_size: Option<u64>,
    ) -> bool {
        let store = cx.global_mut::<SharedAppStore>();
        let Some(queue) = store.pending_older_page_before.get_mut(session_id) else {
            return false;
        };
        if let Some(fs) = file_size {
            queue.retain(|&req_before| req_before <= fs);
        }
        let matched = if let Some(lo) = latest_offset {
            if let Some(pos) = queue
                .iter()
                .position(|&req_before| lo == req_before)
                .or_else(|| queue.iter().position(|&req_before| lo <= req_before))
            {
                queue.remove(pos);
                true
            } else {
                false
            }
        } else {
            false
        };
        if queue.is_empty() {
            store.pending_older_page_before.remove(session_id);
        }
        matched
    }

    pub(crate) fn next_local_message_id(&mut self) -> String {
        self.history.next_local_message_id += 1;
        format!("local-msg-{}", self.history.next_local_message_id)
    }

    pub(crate) fn record_pending_sent_message(
        &mut self,
        host: &str,
        session: &SessionSummary,
        id: String,
        text: String,
    ) {
        let key = canonical_session_key(host, session.agent, &session.id);
        let selected = self.selection.selected_session.as_ref().is_some_and(|s| {
            self.active_session_host() == host && s.id == session.id && s.agent == session.agent
        });
        let server_msgs = self
            .history
            .chat_cache
            .get(&CacheKey::new(host, session.agent, &session.id))
            .map(|cached| cached.messages.clone())
            .unwrap_or_else(|| {
                if selected {
                    self.history
                        .messages
                        .iter()
                        .filter(|m| {
                            !self.chat_state.failed_sends.contains_key(&m.id)
                                && !self.history.pending_sent_messages.get(&key).is_some_and(
                                    |pending| pending.iter().any(|p| p.message.id == m.id),
                                )
                        })
                        .cloned()
                        .collect()
                } else {
                    Vec::new()
                }
            });
        self.begin_awaiting_reply(&key, host, session, &id, &text, &server_msgs);
        let server_user_count = server_msgs
            .iter()
            .filter(|m| m.role == MessageRole::User)
            .count();
        let pending = self
            .history
            .pending_sent_messages
            .entry(key.clone())
            .or_default();
        pending.push(PendingSentMessage {
            awaiting_reply: true,
            message: ChatMessage {
                id: id.clone(),
                role: MessageRole::User,
                text: text.clone(),
                ..Default::default()
            },
            baseline_server_user_count: server_user_count,
        });
        if selected {
            self.selection.last_dispatched_prompt = Some((id, text));
            self.history.messages = self.preserve_local_messages(&key, server_msgs);
            self.selection.selected_session.as_mut().unwrap().working = true;
            self.chat_state.scroll.sent_message();
            self.chat_state.prepend_scroll_anchor = None;
            self.chat_state.scroll_handle.scroll_to_bottom();
            self.chat_state.advance_busy_tick();
        }
    }

    pub(super) fn preserve_local_messages(
        &mut self,
        session_key: &str,
        server_msgs: Vec<ChatMessage>,
    ) -> Vec<ChatMessage> {
        self.observe_turn_snapshot(session_key, &server_msgs);
        let mut out = server_msgs;
        if let Some(pending) = self.history.pending_sent_messages.get_mut(session_key) {
            let users: Vec<_> = out.iter().filter(|m| m.role == MessageRole::User).collect();
            let mut next_user = 0;
            pending.retain_mut(|sent| {
                // An echo consumed by an earlier send must stay consumed on later refreshes.
                sent.baseline_server_user_count = next_user.max(sent.baseline_server_user_count);
                let start = sent.baseline_server_user_count;
                if let Some(index) = users
                    .iter()
                    .enumerate()
                    .skip(start)
                    .find_map(|(i, m)| (m.text.trim() == sent.message.text.trim()).then_some(i))
                {
                    next_user = index + 1;
                    false
                } else {
                    true
                }
            });
            out.extend(pending.iter().map(|p| p.message.clone()));
            if pending.is_empty() {
                self.history.pending_sent_messages.remove(session_key);
            }
        }
        if self
            .selection
            .selected_session
            .as_ref()
            .is_some_and(|session| self.session_key_for(session) == session_key)
        {
            let existing_ids: HashSet<_> = out.iter().map(|m| m.id.clone()).collect();
            for msg in &self.history.messages {
                if self.chat_state.failed_sends.contains_key(&msg.id)
                    && !existing_ids.contains(&msg.id)
                {
                    out.push(msg.clone());
                }
            }
        }
        out
    }

    pub(super) fn apply_history_event(
        &mut self,
        ev: ServerEvent,
        is_older_page_hint: bool,
    ) -> bool {
        let ServerEvent::History {
            ref session_id,
            ref messages,
            oldest_offset,
            latest_offset,
            has_more,
            file_size,
            ref todos,
            ..
        } = ev
        else {
            return false;
        };

        let cache_key = self
            .history
            .active_switch_token
            .as_ref()
            .filter(|t| t.key.session_id == *session_id)
            .map(|t| t.key.clone())
            .or_else(|| {
                self.history
                    .pending_switch_tokens
                    .get(session_id)
                    .map(|t| t.key.clone())
            })
            .or_else(|| {
                self.session_inputs()
                    .into_iter()
                    .find(|s| s.host == self.active_session_host() && s.summary.id == *session_id)
                    .map(|s| CacheKey::new(s.host, s.summary.agent, &s.summary.id))
                    .or_else(|| {
                        self.selection
                            .selected_session
                            .as_ref()
                            .filter(|s| s.id == *session_id)
                            .map(|s| CacheKey::new(self.active_session_host(), s.agent, &s.id))
                    })
            });
        let cached_entry = cache_key
            .as_ref()
            .and_then(|k| self.history.chat_cache.get(k));
        let cached_oldest_offset = cached_entry.map(|c| c.oldest_offset);
        let cached_file_size = cached_entry.map(|c| c.file_size);
        let is_truncated_eof_tail =
            file_size.is_some_and(|fs| cached_file_size.is_some_and(|cfs| cfs > 0 && fs < cfs));

        // Older-page pagination detection (works across ALL multi-window broadcast receivers
        // via App-scoped `is_older_page_hint` without misclassifying active `.jsonl` transcripts
        // that end with an unterminated partial trailing line where `latest_offset < file_size`).
        // Truncated/rotated transcripts ALWAYS replace stale cache instead of prepending.
        let is_older_page = !is_truncated_eof_tail
            && (is_older_page_hint
                || self
                    .history
                    .pending_older_page_before
                    .get(session_id)
                    .copied()
                    .is_some_and(|req_before| {
                        file_size.is_none_or(|fs| req_before <= fs)
                            && latest_offset.is_some_and(|lo| lo <= req_before)
                    })
                || cached_oldest_offset
                    .is_some_and(|co| co > 0 && latest_offset.is_some_and(|lo| lo <= co)));

        if is_older_page {
            self.history.pending_older_page_before.remove(session_id);
            let Some(key) = cache_key else {
                return false;
            };
            let Some(mut cached) = self.history.chat_cache.get(&key).cloned() else {
                // This window does not hold the cached tail for this session: ignore cleanly
                // without overwriting Tier-2 disk cache or touching this window's active view.
                return false;
            };
            let Some(lo) = latest_offset else {
                return false;
            };
            // Enforce exact boundary contiguity: ignore already-prepended pages (`oldest_offset < lo`)
            // AND non-contiguous pages from sibling windows at a deeper pagination offset
            // (`oldest_offset > lo`) so no permanent unreachable middle gap can ever be created.
            if cached.oldest_offset != lo {
                return false;
            }

            let existing_ids: HashSet<String> =
                cached.messages.iter().map(|m| m.id.clone()).collect();
            let mut combined: Vec<ChatMessage> = messages
                .iter()
                .filter(|m| !existing_ids.contains(&m.id))
                .cloned()
                .collect();
            let added_count = combined.len();
            combined.extend(cached.messages.drain(..));
            cached.messages = combined;
            if let Some(oo) = oldest_offset {
                cached.oldest_offset = oo;
            }
            cached.has_more = has_more;
            // Preserve `consumed_offset`, `file_size`, and `probe_sha256` (EOF probe).
            // `ChatCache::put` updates Tier-1 memory LRU ONLY (never overwrites Tier-2 disk).
            let session_key = canonical_session_key(&key.host, key.agent, &key.session_id);
            let older_users = messages
                .iter()
                .filter(|m| m.role == MessageRole::User && !existing_ids.contains(&m.id))
                .count();
            if let Some(pending) = self.history.pending_sent_messages.get_mut(&session_key) {
                for sent in pending {
                    sent.baseline_server_user_count += older_users;
                }
            }
            self.history.chat_cache.put(key, cached.clone());

            if self
                .selection
                .selected_session
                .as_ref()
                .is_some_and(|s| s.id == *session_id)
            {
                if added_count > 0 {
                    if self.chat_state.scroll.pending_scroll == PendingScrollTarget::None {
                        let handle = &self.chat_state.scroll_handle;
                        self.chat_state.prepend_scroll_anchor =
                            Some((handle.max_offset().y, handle.offset()));
                    }
                    self.chat_state.scroll.prepended(added_count);
                }
                self.history.messages = self.preserve_local_messages(&session_key, cached.messages);
                self.chat_state
                    .reconcile_local_reactions(&self.history.messages);
                if self.chat_state.find_open {
                    let q = self.chat_state.find_query.clone();
                    self.chat_state.update_find_query(
                        q,
                        &self.history.messages,
                        self.show_tool_calls(),
                    );
                }
                return added_count > 0 || oldest_offset.is_some();
            }
            return false;
        }

        // Non-older tail History: clear any stale pending older-page marker so a subsequent
        // truncated/rotated tail can never be misclassified as an older page.
        self.history.pending_older_page_before.remove(session_id);

        let source_path = self
            .session_inputs()
            .into_iter()
            .find(|s| s.host == self.active_session_host() && s.summary.id == *session_id)
            .map(|s| s.summary.source_path.clone())
            .or_else(|| {
                self.selection
                    .selected_session
                    .as_ref()
                    .map(|s| s.source_path.clone())
            })
            .unwrap_or_default();

        let token = self
            .history
            .pending_switch_tokens
            .get(session_id)
            .cloned()
            .or_else(|| {
                self.history
                    .active_switch_token
                    .as_ref()
                    .filter(|t| t.key.session_id == *session_id)
                    .cloned()
            });

        if let Some(token) = token {
            match self
                .history
                .chat_cache
                .reconcile_server_event(&token, source_path, &ev)
            {
                Some(ReconcileOutcome::AppliedToActive) => {
                    if let Some(cached) = self.history.chat_cache.get(&token.key) {
                        self.history.messages = self.preserve_local_messages(
                            &canonical_session_key(
                                &token.key.host,
                                token.key.agent,
                                &token.key.session_id,
                            ),
                            cached.messages.clone(),
                        );
                    }
                    self.chat_state
                        .scroll
                        .history_loaded(&self.history.messages, false);
                    self.chat_state
                        .reconcile_local_reactions(&self.history.messages);
                    if !todos.is_empty() || self.chat_state.session_todos.contains_key(session_id) {
                        self.chat_state
                            .session_todos
                            .insert(session_id.clone(), todos.clone());
                    }
                    if self.chat_state.find_open {
                        let q = self.chat_state.find_query.clone();
                        self.chat_state.update_find_query(
                            q,
                            &self.history.messages,
                            self.show_tool_calls(),
                        );
                    }
                    return true;
                }
                Some(ReconcileOutcome::UpdatedBackgroundOnly)
                | Some(ReconcileOutcome::PreservedCachedOnError)
                | Some(ReconcileOutcome::UncachedError { .. })
                | None => return false,
            }
        }

        if self
            .selection
            .selected_session
            .as_ref()
            .is_some_and(|s| s.id == *session_id)
        {
            let key = self.session_key_for(self.selection.selected_session.as_ref().unwrap());
            self.history.messages = self.preserve_local_messages(&key, messages.clone());
            self.chat_state
                .scroll
                .history_loaded(&self.history.messages, false);
            self.chat_state
                .reconcile_local_reactions(&self.history.messages);
            if !todos.is_empty() {
                self.chat_state
                    .session_todos
                    .insert(session_id.clone(), todos.clone());
            }
            if self.chat_state.find_open {
                let q = self.chat_state.find_query.clone();
                self.chat_state.update_find_query(
                    q,
                    &self.history.messages,
                    self.show_tool_calls(),
                );
            }
            return true;
        }
        false
    }

    /// Reconciles a `ServerEvent::HistoryAppend`, returning `(visible_change, sessions_changed)`.
    pub(super) fn apply_history_append_event(&mut self, ev: ServerEvent) -> (bool, bool) {
        let ServerEvent::HistoryAppend {
            ref session_id,
            ref messages,
            working,
            ref todos,
            ..
        } = ev
        else {
            return (false, false);
        };

        let is_active = self
            .selection
            .selected_session
            .as_ref()
            .is_some_and(|s| s.id == *session_id);
        let added_count = if is_active {
            if self.chat_state.scroll.pending_scroll == PendingScrollTarget::None {
                let handle = &self.chat_state.scroll_handle;
                let distance = f32::from(handle.max_offset().y + handle.offset().y);
                self.chat_state.scroll.sync_position(distance);
            }
            let existing: HashSet<&str> = self
                .history
                .messages
                .iter()
                .map(|m| m.id.as_str())
                .collect();
            messages
                .iter()
                .filter(|m| !existing.contains(m.id.as_str()))
                .map(|m| m.id.as_str())
                .collect::<HashSet<_>>()
                .len()
        } else {
            0
        };
        let mut visible_changed = false;
        let source_path = self
            .session_inputs()
            .into_iter()
            .find(|s| s.host == self.active_session_host() && s.summary.id == *session_id)
            .map(|s| s.summary.source_path.clone())
            .or_else(|| {
                self.selection
                    .selected_session
                    .as_ref()
                    .map(|s| s.source_path.clone())
            })
            .unwrap_or_default();

        let token = self
            .history
            .pending_switch_tokens
            .get(session_id)
            .cloned()
            .or_else(|| {
                self.history
                    .active_switch_token
                    .as_ref()
                    .filter(|t| t.key.session_id == *session_id)
                    .cloned()
            });

        if let Some(token) = token {
            if matches!(
                self.history
                    .chat_cache
                    .reconcile_server_event(&token, source_path, &ev),
                Some(ReconcileOutcome::AppliedToActive)
            ) {
                if let Some(cached) = self.history.chat_cache.get(&token.key) {
                    self.history.messages = self.preserve_local_messages(
                        &canonical_session_key(
                            &token.key.host,
                            token.key.agent,
                            &token.key.session_id,
                        ),
                        cached.messages.clone(),
                    );
                }
                self.chat_state
                    .reconcile_local_reactions(&self.history.messages);
                if !todos.is_empty() {
                    self.chat_state
                        .session_todos
                        .insert(session_id.clone(), todos.clone());
                }
                if self.chat_state.find_open {
                    let q = self.chat_state.find_query.clone();
                    self.chat_state.update_find_query(
                        q,
                        &self.history.messages,
                        self.show_tool_calls(),
                    );
                }
                visible_changed = true;
            }
        } else if self
            .selection
            .selected_session
            .as_ref()
            .is_some_and(|s| s.id == *session_id)
        {
            let key = self.session_key_for(self.selection.selected_session.as_ref().unwrap());
            let pending_ids: HashSet<_> = self
                .history
                .pending_sent_messages
                .get(&key)
                .into_iter()
                .flatten()
                .map(|p| p.message.id.clone())
                .collect();
            let mut server_msgs: Vec<_> = self
                .history
                .messages
                .iter()
                .filter(|m| {
                    !pending_ids.contains(&m.id)
                        && !self.chat_state.failed_sends.contains_key(&m.id)
                })
                .cloned()
                .collect();
            for msg in messages {
                if !server_msgs.iter().any(|m| m.id == msg.id) {
                    server_msgs.push(msg.clone());
                }
            }
            let reconciled = self.preserve_local_messages(&key, server_msgs);
            visible_changed = reconciled != self.history.messages;
            self.history.messages = reconciled;
            self.chat_state
                .reconcile_local_reactions(&self.history.messages);
            if !todos.is_empty() {
                self.chat_state
                    .session_todos
                    .insert(session_id.clone(), todos.clone());
                visible_changed = true;
            }
        }

        if let Some(session) = self
            .selection
            .selected_session
            .as_ref()
            .filter(|s| s.id == *session_id)
        {
            let key = self.session_key_for(session);
            self.observe_turn_history(&key, messages, working);
        }
        let mut sessions_changed = false;
        if let Some(is_working) = working {
            for s in self
                .connection
                .sessions
                .iter_mut()
                .filter(|_| self.connection.host_statuses.is_empty())
            {
                if s.id == *session_id {
                    s.working = is_working;
                    sessions_changed = true;
                }
            }
            if let Some(ref mut sel) = self.selection.selected_session
                && sel.id == *session_id
            {
                sel.working = is_working;
                visible_changed = true;
            }
            if is_working {
                self.chat_state.advance_busy_tick();
            }
        }

        // Keep sidebar/Inbox previews live when an assistant reply arrives
        if let Some(last_asst) = messages
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::Assistant)
        {
            let preview: String = last_asst.text.trim().chars().take(120).collect();
            for s in self
                .connection
                .sessions
                .iter_mut()
                .filter(|_| self.connection.host_statuses.is_empty())
            {
                if s.id == *session_id {
                    s.last_assistant_id = Some(last_asst.id.clone());
                    if !preview.is_empty() {
                        s.last_assistant_preview = Some(preview.clone());
                    }
                    if let Some(ts) = last_asst.timestamp_ms {
                        s.last_assistant_ms = Some(ts);
                        s.updated_at_ms = s.updated_at_ms.max(ts);
                    }
                    sessions_changed = true;
                }
            }
            if let Some(ref mut sel) = self.selection.selected_session
                && sel.id == *session_id
            {
                sel.last_assistant_id = Some(last_asst.id.clone());
                if !preview.is_empty() {
                    sel.last_assistant_preview = Some(preview);
                }
                if let Some(ts) = last_asst.timestamp_ms {
                    sel.last_assistant_ms = Some(ts);
                    sel.updated_at_ms = sel.updated_at_ms.max(ts);
                }
                visible_changed = true;
            }
        }

        if is_active && visible_changed {
            self.chat_state.scroll.appended(added_count);
        }
        (visible_changed, sessions_changed)
    }

    pub(crate) fn search_active_session_history(
        &mut self,
        query: String,
        cx: &mut Context<Self>,
    ) -> bool {
        let trimmed = query.trim();
        if trimmed.is_empty() || trimmed.chars().count() > 200 {
            return false;
        }
        let Some(ref sess) = self.selection.selected_session else {
            return false;
        };
        let Some(tx) = self.active_session_tx().cloned() else {
            return false;
        };
        self.history.next_search_request_seq = self.history.next_search_request_seq.wrapping_add(1);
        let request_id = format!("search-{}", self.history.next_search_request_seq);
        self.history.remote_search_request_id = Some(request_id.clone());
        let ok = tx
            .send(ClientRequest::SearchHistory {
                request_id,
                session_id: sess.id.clone(),
                source_path: sess.source_path.clone(),
                agent: sess.agent,
                query: trimmed.to_string(),
            })
            .is_ok();
        cx.notify();
        ok
    }

    pub(crate) fn has_unloaded_older_history(&self) -> bool {
        self.history
            .active_switch_token
            .as_ref()
            .and_then(|t| self.history.chat_cache.get(&t.key))
            .is_some_and(|c| c.has_more && c.oldest_offset > 0)
    }

    pub(crate) fn load_older_history(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(ref sess) = self.selection.selected_session else {
            return false;
        };
        let Some(tx) = self.active_session_tx().cloned() else {
            return false;
        };
        let Some(token) = self.history.active_switch_token.as_ref() else {
            return false;
        };
        let Some(cached) = self.history.chat_cache.get(&token.key) else {
            return false;
        };
        if !cached.has_more || cached.oldest_offset == 0 {
            return false;
        }
        let before = cached.oldest_offset;
        let sess_id = sess.id.clone();
        let ok = tx
            .send(ClientRequest::ReadHistory {
                session_id: sess_id.clone(),
                source_path: sess.source_path.clone(),
                agent: sess.agent,
                tail: Some(200),
                before_offset: Some(before),
                max_bytes: Some(MAX_SESSION_CACHE_BYTES),
                // `cached.probe_sha256` is the EOF probe at `latest_offset`, NOT at `before_offset`.
                // Passing `None` ensures `multibotd` honors `before_offset` cleanly.
                known_probe: None,
            })
            .is_ok();
        if ok {
            self.history
                .pending_older_page_before
                .insert(sess_id.clone(), before);
            if cx.has_global::<SharedAppStore>() {
                let store = cx.global_mut::<SharedAppStore>();
                let queue = store
                    .pending_older_page_before
                    .entry(if self.connection.host_statuses.is_empty() {
                        sess_id
                    } else {
                        format!(
                            "{}:{}{}",
                            self.active_session_host().len(),
                            self.active_session_host(),
                            sess_id
                        )
                    })
                    .or_default();
                if queue.len() >= 64 {
                    queue.pop_front();
                }
                queue.push_back(before);
            }
        }
        cx.notify();
        ok
    }
}
