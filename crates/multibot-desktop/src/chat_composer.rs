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
            matches!(
                agent,
                AgentKind::Claude | AgentKind::Codex | AgentKind::Antigravity
            ) && !options.is_default()
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

    pub(crate) fn update_composer_options(
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
        let supported = matches!(
            agent,
            AgentKind::Claude | AgentKind::Codex | AgentKind::Antigravity
        );
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
                ("GPT-6.1 Sol", Some("gpt-6.1-sol")),
                ("GPT-5.4", Some("gpt-5.4")),
                ("Codex", Some("gpt-5.3-codex")),
            ],
            AgentKind::Antigravity => &[
                ("Default", None),
                ("Gemini 4 Neon", Some("gemini-4-neon")),
                ("Gemini 4 Carbon", Some("gemini-4-carbon")),
                ("Gemini 4 Argon", Some("gemini-4-argon")),
                ("Gemini 4 Hydrogen", Some("gemini-4-hydrogen")),
                ("Gemini 3.8 Flash", Some("gemini-3.8-flash")),
                ("Opus 5.5", Some("claude-opus-5-5")),
                ("Sonnet 5.5", Some("claude-sonnet-5-5")),
                ("Orchestrator", Some("gemini-pro-orchestrator-neon")),
                ("GPT-OSS 120B", Some("gpt-oss-120b-medium")),
            ],
            AgentKind::Shell | AgentKind::Pi | AgentKind::Cursor => &[("Default", None)],
        };
        let mut row = h_flex().flex_wrap().gap_1();
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
        let mut row = h_flex().flex_wrap().gap_1();
        for (index, (label, effort)) in [
            ("Default", None),
            ("Low", Some(EffortLevel::Low)),
            ("Medium", Some(EffortLevel::Medium)),
            ("High", Some(EffortLevel::High)),
            (
                if agent == AgentKind::Antigravity {
                    "XHigh"
                } else {
                    "Max"
                },
                Some(EffortLevel::Max),
            ),
        ]
        .into_iter()
        .enumerate()
        {
            if effort == Some(EffortLevel::Max)
                && !matches!(agent, AgentKind::Codex | AgentKind::Antigravity)
            {
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
        let mut row = h_flex().flex_wrap().gap_1();
        for (index, (label, permission)) in [
            ("Full access", None),
            ("Ask", Some(PermissionLevel::Ask)),
            ("Auto", Some(PermissionLevel::Auto)),
        ]
        .into_iter()
        .enumerate()
        {
            let selected = options
                .permission
                .filter(|p| *p != PermissionLevel::FullAccess)
                == permission;
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
                        "This agent uses defaults; overrides are not supported."
                    } else if options.permission.is_none()
                        || options.permission == Some(PermissionLevel::FullAccess)
                    {
                        "Full access (default) bypasses approval checks."
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
        let is_working = self.is_active_chat_working();
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
            .when(!self.settings.shell_fullscreen, |el| el.max_w(px(960.0)))
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
                            .ghost()
                            .size(px(24.0))
                            .rounded(RADIUS_CHIP)
                            .icon(lucide_icon(icons::ARROW_UP, 14.0))
                            .accessibility_label("Send now")
                            .tooltip(crate::icons::shortcut_tooltip(
                                "Send now",
                                "chat.force_send",
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.force_send_queued_entry(&force_id, cx);
                            })),
                    )
                    .child(
                        Button::new(format!("queue-edit-{index}"))
                            .ghost()
                            .size(px(24.0))
                            .rounded(RADIUS_CHIP)
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
                            .ghost()
                            .size(px(24.0))
                            .rounded(RADIUS_CHIP)
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

        let geometry_view = cx.entity().downgrade();
        root
            .child(gpui_kit::canvas(move |bounds, _, cx| {
                let _ = geometry_view.update(cx, |this, cx| {
                    let height = f32::from(bounds.size.height);
                    if this.chat_state.composer_height != height {
                        this.chat_state.composer_height = height;
                        cx.notify();
                    }
                });
            }, |_, _, _, _| {}).absolute().inset_0().size_full())
            .when_some(self.notice.clone(), |el, note| {
                el.child(
                    div()
                        .w_full()
                        .when(!self.settings.shell_fullscreen, |el| el.max_w(px(960.0)))
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
                        .when(!self.settings.shell_fullscreen, |el| el.max_w(px(960.0)))
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
                                        .tooltip(|window, cx| {
                                            gpui_kit::component::tooltip::Tooltip::new(
                                                crate::icons::shortcut_tooltip("Cancel reply", "system.escape"),
                                            ).build(window, cx)
                                        })
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
            .when(!self.chat_state.quoted_snippets.is_empty(), |el| {
                let mut chips = h_flex().id("quoted-snippets").test_support()
                    .w_full().when(!self.settings.shell_fullscreen, |el| el.max_w(px(960.0)))
                    .mx_auto().px_5().pb_1().gap_1p5().flex_wrap();
                for snippet in &self.chat_state.quoted_snippets {
                    let id = snippet.id;
                    chips = chips.child(h_flex()
                        .id(format!("quote-chip-{id}")).test_support()
                        .gap_2().items_center().px_2p5().py_1().rounded(RADIUS_CHIP)
                        .bg(rgb(0x1E293B)).text_xs().text_color(theme::text_secondary())
                        .child(format!("« {} »", snippet.preview))
                        .child(div().id(format!("remove-quote-{id}")).tooltip(|window, cx| gpui_kit::component::tooltip::Tooltip::new("Remove").build(window, cx)).role(Role::Button).test_support()
                            .cursor_pointer().child(lucide_icon(icons::X, 12.0))
                            .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                if let Some(index) = this.chat_state.quoted_snippets.iter().position(|snippet| snippet.id == id) {
                                    let snippet = this.chat_state.quoted_snippets.remove(index);
                                    this.inputs.prompt_input.update(cx, |input, cx| {
                                        let draft = input.value().replace(&snippet.token(), "");
                                        input.set_value(draft, window, cx);
                                        input.focus(window, cx);
                                    });
                                    cx.notify();
                                }
                            }))));
                }
                el.child(chips)
            })
            .when(!self.chat_state.pending_attachments.is_empty(), |el| {
                let mut chips = h_flex()
                    .w_full()
                    .when(!self.settings.shell_fullscreen, |el| el.max_w(px(960.0)))
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
                                    .tooltip(|window, cx| {
                                        gpui_kit::component::tooltip::Tooltip::new("Remove").build(window, cx)
                                    })
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
                h_flex().id("lifecycle-peek-banner").test_support().w_full().when(!self.settings.shell_fullscreen, |el| el.max_w(px(960.0))).mx_auto()
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
                div().w_full().when(!self.settings.shell_fullscreen, |el| el.max_w(px(960.0))).mx_auto().px_5().child(
                    self.render_composer_options(cx)
                )
            ))
            .child(
                div().w_full().flex_shrink_0().px_5().pb_4().pt_2().child(
                    h_flex()
                        .id("chat-composer-pill")
                        .test_support()
                        .w_full()
                        .when(!self.settings.shell_fullscreen, |el| el.max_w(px(960.0)))
                        .mx_auto()
                        .px_3()
                        .py_1p5()
                        .gap_2()
                        .items_center()
                        .rounded(px(20.0))
                        .bg(theme::bg_surface())
                        .border_1()
                        .border_color(theme::border_subtle())
                        .child(
                            div()
                                .id("attach-file-btn")
                                .tooltip(|window, cx| {
                                    gpui_kit::component::tooltip::Tooltip::new("Attach, model & permissions")
                                        .build(window, cx)
                                })
                                .role(Role::Button)
                                .test_support()
                                .w(px(32.0))
                                .h(px(32.0))
                                .flex_shrink_0()
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
                                    .appearance(false)
                                    .token(|ctx, _, _| {
                                        let token = ctx.token();
                                        let element = if !token.id().starts_with("skill:") && multibot_core::workspaces::parse_session_key(token.id()).is_some() {
                                            crate::mention_picker::bare_mention(token.label().clone(), token.id(), ctx.line_height())
                                        } else {
                                            div().h(ctx.line_height()).line_height(ctx.line_height())
                                                .when(token.id().starts_with("skill:"), |el| el
                                                    .text_color(theme::accent_blue())
                                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD))
                                                .child(div().child(token.label().clone()).test_text(token.label().clone()))
                                        };
                                        element.when(ctx.is_selected(), |el| el.bg(rgba(0x60A5FA24)))
                                    }),
                            ),
                        )
                        .when(self.voice.phase != multibot_core::voice::DictationPhase::Off || self.voice.error.is_some(), |el| {
                            el.child(div().px_2().py_1().child(
                                h_flex().id("dictation-status").test_support().gap(px(6.0)).items_center().text_xs()
                                    .text_color(rgb(0xF5F5F5))
                                    .when(self.voice.phase == multibot_core::voice::DictationPhase::Listening, |el| el
                                        .child(div().id("dictation-stop").test_support().size(px(9.0)).rounded(px(2.0)).bg(rgb(0xF5F5F5)))
                                        .child(div().id("dictation-timer").test_support().font_family("monospace").child(Label::new(self.voice.elapsed_label()).text_color(rgb(0xF5F5F5))))
                                        .child(div().child(Label::new("·····").text_color(rgb(0x737373)))))
                                    .when(self.voice.phase == multibot_core::voice::DictationPhase::Finishing, |el| el.child("Transcribing..."))
                                    .when(self.voice.error.is_some(), |el| el.text_color(theme::accent_amber()).child("Mic unavailable")),
                            ))
                        })
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
                                .tooltip(format!("{} / {}", crate::icons::shortcut_tooltip("Start / stop dictation", "chat.dictation_inline"), multibot_keys::by_id("chat.dictation").unwrap().display))
                                .w(px(32.0))
                                .flex_shrink_0()
                                .icon(lucide_icon(icons::MIC, 16.0))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.toggle_dictation(window, cx);
                                })),
                        )
                        .when(is_working, |el| el.child(
                            Button::new("stop-agent")
                                .small()
                                .custom(ButtonCustomVariant::new(cx).foreground(rgb(0xD4D4D4).into())
                                    .hover(rgb(0x333333).into()).active(rgb(0x333333).into()))
                                .size(px(32.0))
                                .rounded(RADIUS_CHIP)
                                .bg(rgb(0x262626))
                                .text_color(rgb(0xD4D4D4))
                                .icon(lucide_icon(icons::SQUARE_FILLED, 16.0))
                                .accessibility_label("Stop response")
                                .tooltip("Stop response")
                                .on_click(cx.listener(|this, _, _, cx| { this.interrupt_active_session(cx); })),
                        ))
                        .child(
                            Button::new("send-prompt")
                                .small()
                                .custom(ButtonCustomVariant::new(cx).foreground(rgb(0xD4D4D4).into())
                                    .hover(rgb(0x333333).into()).active(rgb(0x333333).into()))
                                .size(px(32.0))
                                .rounded(RADIUS_CHIP)
                                .bg(rgb(0x262626))
                                .text_color(rgb(0xD4D4D4))
                                .icon(lucide_icon(icons::ARROW_UP, 16.0))
                                .accessibility_label(if is_working { "Queue" } else { "Send" })
                                .tooltip(if is_working { format!("{} ({} to force send)", crate::icons::shortcut_tooltip("Queue message", "chat.send"), multibot_keys::by_id("chat.force_send").unwrap().display) } else { crate::icons::shortcut_tooltip("Send message", "chat.send") })
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.send_current_prompt(window, cx);
                                })),
                        ),
                ),
            )
    }
}
