use super::*;
use gpui_kit::base::TestTextExt;
use gpui_kit::component::{
    highlighter::HighlightTheme,
    text::{
        InlineElement, InlineRenderContext, MarkdownExtensions, MarkdownNode, MarkdownParseContext,
        MarkdownPlugin, TextViewStyle, markdown_ast,
    },
};
use gpui_kit::{App, ClickEvent, FontWeight, HighlightStyle, StyleRefinement, WeakEntity};

struct ChatTokenPlugin {
    view: WeakEntity<MultibotView>,
}

impl MarkdownPlugin for ChatTokenPlugin {
    fn name(&self) -> &str {
        "multibot-chat-token"
    }

    fn parse(
        &self,
        node: &markdown_ast::Node,
        context: &MarkdownParseContext<'_>,
    ) -> Option<MarkdownNode> {
        let markdown_ast::Node::Link(link) = node else {
            return None;
        };
        if !link.url.starts_with("multibot-skill://")
            && !link.url.starts_with("multibot-session://")
        {
            return None;
        }
        let label = link
            .children
            .iter()
            .map(|child| child.to_string())
            .collect::<String>();
        Some(
            MarkdownNode::new("multibot-chat-token", link.url.clone())
                .text(label)
                .markdown(context.node_source(node).unwrap_or_default().to_string()),
        )
    }

    fn render_inline(
        &self,
        node: &MarkdownNode,
        context: &InlineRenderContext,
        window: &mut Window,
        _cx: &mut App,
    ) -> Option<InlineElement> {
        let url = node.data::<String>()?;
        let label = SharedString::from(node.as_text().to_string());
        let element = if url.starts_with("multibot-skill://") {
            div()
                .id(SharedString::from(format!(
                    "chat-skill-{}",
                    node.source_range().map_or(0, |r| r.start)
                )))
                .test_support()
                .h(context.line_height())
                .line_height(context.line_height())
                .text_size(context.font_size())
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::accent_blue())
                .child(label.clone())
                .test_text(label)
                .into_any_element()
        } else {
            let key = crate::mention_picker::key_from_url(url)?;
            let weak = self.view.clone();
            let url = url.clone();
            crate::mention_picker::bare_mention(label, &key, context.line_height())
                .id(SharedString::from(url.clone()))
                .role(Role::Link)
                .aria_label(node.as_text().to_string())
                .test_support()
                .cursor_pointer()
                .on_click(move |event, _, cx| {
                    if !event.standard_click() || event.click_count() != 1 {
                        return;
                    }
                    if let ClickEvent::Mouse(mouse) = event {
                        if (mouse.up.position - mouse.down.position).magnitude()
                            > f64::from(DOUBLE_CLICK_DRAG_THRESHOLD_PX)
                        {
                            return;
                        }
                    }
                    cx.stop_propagation();
                    let _ = weak.update(cx, |this, cx| {
                        this.jump_mention_url(&url, cx);
                    });
                })
                .text_size(context.font_size())
                .into_any_element()
        };
        // Match TextView's shaped-line baseline, including native signed-descent differences.
        let style = context.text_style();
        let shaped = window.text_system().shape_line(
            "M".into(),
            context.font_size(),
            &[style.to_run(1)],
            None,
        );
        let baseline =
            (context.line_height() - shaped.ascent - shaped.descent) / 2. + shaped.ascent;
        Some(InlineElement::new(element).with_baseline(baseline))
    }
}

impl MultibotView {
    pub(super) fn render_tool_cluster(
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
            .tooltip(|window, cx| {
                gpui_kit::component::tooltip::Tooltip::new("Expand / collapse tool calls")
                    .build(window, cx)
            })
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

    pub(super) fn render_markdown_with_actions(
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
        let text = crate::slash_picker::render_skills(&text);
        let weak_mention = cx.entity().downgrade();
        let style = TextViewStyle {
            highlight_theme: HighlightTheme::default_dark(),
            is_dark: true,
            ..Default::default()
        }
        .inline_code(HighlightStyle {
            color: Some(rgb(0xE5E5E5).into()),
            background_color: Some(theme::bg_surface()),
            ..Default::default()
        })
        .code_block(
            StyleRefinement::default()
                .bg(rgb(0x111111))
                .border_1()
                .border_color(theme::border_subtle())
                .rounded(RADIUS_ROW)
                .font_family("monospace")
                .text_color(rgb(0xE5E5E5)),
        );
        let md_view = TextView::markdown(id.clone(), text)
            .style(style)
            .inline_code_style(
                StyleRefinement::default()
                    .border_0()
                    .font_family("monospace"),
            )
            .markdown_extensions(MarkdownExtensions::default().plugin(ChatTokenPlugin {
                view: cx.entity().downgrade(),
            }))
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
                            .accessibility_label("Copy")
                            .tooltip("Copy")
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
                    .icon(lucide_icon(icons::MAXIMIZE_2, 14.0))
                    .accessibility_label("Expand table")
                    .tooltip("Expand table")
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

    pub(super) fn render_tool_step_block(
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
        let label = format!("{} · {}", name, tool_activity_label(name, summary, detail));
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
                    .tooltip(|window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new("Expand / collapse tool details")
                            .build(window, cx)
                    })
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
                        .child(
                            Label::new(detail.to_string())
                                .id(SharedString::from(format!(
                                    "tool-detail-{msg_idx}-{block_idx}"
                                )))
                                .text_xs()
                                .text_color(theme::text_secondary()),
                        ),
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
                        .child(
                            Label::new(out.to_string())
                                .id(SharedString::from(format!(
                                    "tool-output-{msg_idx}-{block_idx}"
                                )))
                                .text_xs()
                                .text_color(theme::text_muted()),
                        ),
                );
            }

            card = card.child(body);
        }

        card.into_any_element()
    }

    pub(super) fn render_image_block(
        &self,
        url: Option<&str>,
        local_path: Option<&str>,
        alt: &str,
        id: &str,
        cx: &mut Context<Self>,
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

        let viewer = ImageViewerState {
            local_path: resolved_local.clone(),
            url: url.map(str::to_string),
            alt: alt.to_string(),
        };
        let mut card = div()
            .id(SharedString::from(format!("chat-image-{id}")))
            .test_support()
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.chat_state.image_viewer = Some(viewer.clone());
                cx.notify();
            }))
            .max_w(px(320.0))
            .rounded(RADIUS_ROW)
            .bg(theme::bg_canvas())
            .overflow_hidden();

        if let Some(path) = resolved_local {
            let (width, height) = read_image_dimensions(&path)
                .map(|(w, h)| fit_image_size(w, h, 320.0, 480.0))
                .unwrap_or((320.0, 180.0));
            card = card
                .w(px(width))
                .h(px(height))
                .child(img(path).w(px(width)).h(px(height)));
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

    pub(super) fn render_question_prompt_block(
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
                                this.inputs.prompt_input.update(cx, |state, cx| {
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

    pub(super) fn render_prose_options_card(
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
                        this.inputs.prompt_input.update(cx, |state, cx| {
                            state.focus(window, cx);
                        });
                        cx.notify();
                    }),
                )
                .child("Type something else"),
        );
        col.into_any_element()
    }

    pub(super) fn render_todo_header_and_drawer(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let tab_key = self.active_chat_tab_key();
        let cached_todos = self
            .chat_state
            .session_todos
            .get(&tab_key)
            .map(Vec::as_slice);
        let (todos, done, total) = effective_todos(
            &self.history.messages,
            self.selection.selected_session.as_ref(),
            cached_todos,
        );
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
            .when(!self.settings.shell_fullscreen, |el| el.max_w(px(960.0)))
            .mx_auto()
            .px_4()
            .pt_1()
            .gap_1p5()
            .child(
                h_flex().w_full().justify_between().items_center().child(
                    div()
                        .id("chat-todo-pill")
                        .tooltip(|window, cx| {
                            gpui_kit::component::tooltip::Tooltip::new("Expand / collapse tasks")
                                .build(window, cx)
                        })
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
}

impl MultibotView {
    pub(crate) fn render_image_viewer(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let viewer = self.chat_state.image_viewer.as_ref()?;
        let mut content = v_flex()
            .id("image-viewer-content")
            .test_support()
            .items_center()
            .justify_center();
        if let Some(path) = viewer.local_path.as_ref() {
            let bounds = window.viewport_size();
            let max_w = f32::from(bounds.width) * 0.85;
            let max_h = f32::from(bounds.height) * 0.85;
            let (w, h) = read_image_dimensions(path)
                .map(|(w, h)| fit_image_size(w, h, max_w, max_h))
                .unwrap_or((max_w.min(320.0), max_h.min(180.0)));
            content = content.child(img(path.clone()).w(px(w)).h(px(h)));
        } else {
            content = content
                .text_color(theme::text_secondary())
                .child(viewer.alt.clone());
        }
        let pills = h_flex()
            .id("image-viewer-controls")
            .absolute()
            .top_4()
            .right_4()
            .gap_2()
            .when_some(
                viewer
                    .url
                    .as_ref()
                    .filter(|url| crate::external_url::is_http_url(url))
                    .cloned(),
                |el, url| {
                    el.child(
                        Button::new("image-viewer-open-browser")
                            .small()
                            .label("Open in browser")
                            .rounded(RADIUS_CHIP)
                            .on_click(move |_, _, _| {
                                let _ = crate::external_url::open_external_url(&url);
                            }),
                    )
                },
            )
            .child(
                Button::new("image-viewer-close")
                    .small()
                    .icon(lucide_icon(icons::X, 16.0))
                    .rounded(RADIUS_CHIP)
                    .accessibility_label("Close image viewer")
                    .tooltip(crate::icons::shortcut_tooltip("Close", "system.escape"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.chat_state.image_viewer = None;
                        cx.notify();
                    })),
            );
        Some(
            div()
                .id("image-viewer-modal")
                .test_support()
                .absolute()
                .inset_0()
                .size_full()
                .bg(rgba(0x000000d9))
                .flex()
                .items_center()
                .justify_center()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(|this, _, _, cx| {
                    this.chat_state.image_viewer = None;
                    cx.notify();
                }))
                .child(content.on_click(|_, _, cx| cx.stop_propagation()))
                .child(pills.on_click(|_, _, cx| cx.stop_propagation()))
                .into_any_element(),
        )
    }
}
