use super::*;

impl MultibotView {
    pub(crate) fn composer_options_for(&self, session: &SessionSummary) -> Option<SessionOptions> {
        self.chat_state
            .composer_options
            .get(&self.session_key_for(session))
            .filter(|options| !options.is_default())
            .cloned()
    }

    pub(crate) fn spawn_composer_options(&self, agent: AgentKind) -> Option<SessionOptions> {
        let options = match &self.selection.selected_session {
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
        self.selection
            .selected_session
            .as_ref()
            .and_then(|s| self.composer_options_for(s))
            .unwrap_or_else(|| {
                if self.selection.selected_session.is_none() {
                    self.chat_state.draft_options.clone()
                } else {
                    SessionOptions::default()
                }
            })
    }

    pub(super) fn update_composer_options(
        &mut self,
        update: impl FnOnce(&mut SessionOptions),
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = &self.selection.selected_session {
            let key = self.session_key_for(session);
            update(self.chat_state.composer_options.entry(key).or_default());
        } else {
            update(&mut self.chat_state.draft_options);
        }
        cx.notify();
    }

    pub(super) fn render_composer_options(&self, cx: &mut Context<Self>) -> AnyElement {
        let agent = self
            .selection
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

    pub(super) fn render_composer(
        &self,
        root: gpui_kit::Stateful<gpui_kit::Div>,
        cx: &mut Context<Self>,
    ) -> gpui_kit::Stateful<gpui_kit::Div> {
        let lifecycle = self
            .selection
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
        let is_working = self
            .selection
            .selected_session
            .as_ref()
            .is_some_and(|s| s.working);
        let mut queued_entries = Vec::new();
        let mut queued_previews = Vec::new();
        if let Some(session) = &self.selection.selected_session {
            let key = self.session_key_for(session);
            if let Some(queue) = self.chat_state.queued_prompts.get(&key) {
                queued_previews.extend(queue.iter().map(|(_, text)| text.clone()));
            }
            let outbox_key = multibot_core::outbox::ChatOutbox::session_key(
                self.active_session_host(),
                session.agent,
                &session.id,
            );
            let outbox = self.outbox.shared_chat_outbox.lock().ok();
            let outbox = outbox
                .as_ref()
                .map(|shared| &shared.outbox)
                .unwrap_or(&self.outbox.chat_outbox);
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
                            .icon(lucide_icon(icons::SEND, 14.0))
                            .accessibility_label("Send now")
                            .tooltip("Send now")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.force_send_queued_entry(&force_id, cx);
                            })),
                    )
                    .child(
                        Button::new(format!("queue-edit-{index}"))
                            .small()
                            .icon(lucide_icon(icons::PENCIL, 14.0))
                            .accessibility_label("Edit queued message")
                            .tooltip("Edit queued message")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let draft = this.inputs.prompt_input.read(cx).value().to_string();
                                if let Some(mut entry) = this.remove_queued_entry(&edit_id) {
                                    let text = std::mem::replace(&mut entry.text, draft.clone());
                                    if !draft.is_empty()
                                        && let Ok(mut shared) =
                                            this.outbox.shared_chat_outbox.lock()
                                    {
                                        let _ = shared.outbox.enqueue_entry(entry);
                                        if shared.persist_to_disk
                                            || this.outbox.persist_outbox_to_disk
                                        {
                                            if let Err(err) = shared.outbox.save() {
                                                this.notice = Some(err.to_string());
                                            }
                                        }
                                        this.outbox.chat_outbox = shared.outbox.clone();
                                    }
                                    this.inputs.prompt_input.update(cx, |input, cx| {
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
                            .icon(lucide_icon(icons::TRASH_2, 14.0))
                            .accessibility_label("Remove queued message")
                            .tooltip("Remove queued message")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.remove_queued_entry(&remove_id);
                                cx.notify();
                            })),
                    ),
            );
        }

        root
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
                            if let Some(session) = &this.selection.selected_session {
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
                                Textarea::new(&self.inputs.prompt_input)
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
                            Button::new("stop-agent")
                                .small()
                                .custom(ButtonCustomVariant::new(cx).foreground(rgb(0xFFFFFF).into())
                                    .hover(rgb(0xDC4C48).into()).active(rgb(0xDC4C48).into()))
                                .size(px(32.0))
                                .rounded(RADIUS_CHIP)
                                .bg(rgb(0xC83E3B))
                                .text_color(rgb(0xFFFFFF))
                                .icon(lucide_icon(icons::SQUARE_FILLED, 14.0))
                                .accessibility_label("Stop response")
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
    }
}
