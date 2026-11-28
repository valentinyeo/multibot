use super::*;

impl MultibotView {
    /// Records an asynchronous daemon send failure (wired in M6).
    pub(crate) fn record_send_failure(
        &mut self,
        msg_id: String,
        text: String,
        reason: String,
        cx: &mut Context<Self>,
    ) {
        self.history.pending_sent_messages.retain(|_, pending| {
            pending.retain(|sent| sent.message.id != msg_id);
            !pending.is_empty()
        });
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
        let Some(session) = self.selection.selected_session.clone() else {
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
                session_id: session.id.clone(),
                agent: session.agent,
                cwd: session.cwd.clone(),
                text: failed.text.clone(),
            })
            .is_ok();
        if sent {
            self.chat_state.failed_sends.remove(msg_id);
            self.history.messages.retain(|m| m.id != msg_id);
            let host = self.active_session_host().to_string();
            self.record_pending_sent_message(&host, &session, msg_id.into(), failed.text);
            self.sidebar_state
                .workspace_store
                .record_session_input(&key, now_unix_ms());
            self.persist_workspace_store_if_enabled();
            self.sync_workspace_to_shared(cx);
        }
        cx.notify();
        sent
    }

    pub(super) fn dispatch_text_prompt(
        &mut self,
        text: String,
        quoted_message_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.selection.selected_session.clone() else {
            self.notice = Some("Select a session first.".into());
            cx.notify();
            return;
        };
        let host = self.active_session_host().to_string();
        self.dispatch_text_prompt_for_session(&host, session, text, quoted_message_id, cx);
    }

    pub(super) fn dispatch_text_prompt_for_session(
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
        let msg_id = self.next_local_message_id();
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
        self.send_or_queue_prompt_on_host(host, &session, msg_id.clone(), text);
        if let Some(sent) = self
            .history
            .pending_sent_messages
            .get_mut(&key)
            .and_then(|pending| pending.iter_mut().find(|p| p.message.id == msg_id))
        {
            sent.message.quoted_message_id = quoted_message_id.clone();
        }
        if let Some(message) = self.history.messages.iter_mut().find(|m| m.id == msg_id) {
            message.quoted_message_id = quoted_message_id;
        }
        cx.notify();
    }

    pub(crate) fn track_queued_prompt(&mut self, session_id: &str, message_id: &str, status: &str) {
        let Some(session) = self
            .selection
            .selected_session
            .as_ref()
            .filter(|s| s.id == session_id)
            .or_else(|| {
                self.connection
                    .host_sessions
                    .iter()
                    .find(|(h, s)| h == self.active_session_host() && s.id == session_id)
                    .map(|(_, s)| s)
            })
            .or_else(|| self.connection.sessions.iter().find(|s| s.id == session_id))
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

    pub(crate) fn send_current_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .chat_state
            .pending_harness_switch
            .as_ref()
            .is_some_and(|id| {
                self.selection
                    .selected_session
                    .as_ref()
                    .is_some_and(|s| &s.id == id)
            })
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
        let Some(session) = self.selection.selected_session.clone() else {
            self.notice = Some("Select a session first.".into());
            cx.notify();
            return;
        };

        let key = self.session_key_for(&session);
        if session.agent == AgentKind::Codex && matches!(trimmed.as_str(), "/exit" | "/quit") {
            self.inputs
                .prompt_input
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
            self.inputs.prompt_input.update(cx, |state, cx| {
                state.set_value("", window, cx);
            });
            self.slash_picker_state.close();
            self.commit_session_rename(&key, new_title, cx);
            return;
        }

        let quote = self.chat_state.reply_quote.take();
        let payload = compose_quoted_reply(quote.as_ref().map(|q| q.full_text.as_str()), &trimmed);
        let quoted_id = quote.as_ref().map(|q| q.message_id.clone());

        self.inputs.prompt_input.update(cx, |state, cx| {
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
            .selection
            .selected_session
            .as_ref()
            .is_some_and(|s| s.id == target_session.id && s.agent == target_session.agent);
        let prompt_empty = self.inputs.prompt_input.read(cx).value().trim().is_empty();
        if same_session && prompt_empty && self.chat_state.pending_attachments.is_empty() {
            self.chat_state.pending_attachments = staged;
            if self.chat_state.reply_quote.is_none() {
                self.chat_state.reply_quote = quote;
            }
            self.inputs.prompt_input.update(cx, |state, cx| {
                state.select_all(window, cx);
                state.replace(raw_prompt, window, cx);
            });
        }
        cx.notify();
    }
}
