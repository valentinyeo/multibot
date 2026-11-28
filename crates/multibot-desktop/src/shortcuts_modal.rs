use super::*;

impl MultibotView {
    pub(crate) fn render_changelog_modal(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let releases = multibot_core::changelog::releases();
        let sections = releases.iter().enumerate().map(|(index, release)| {
            v_flex()
                .id(("changelog-release", index))
                .test_support()
                .flex_shrink_0()
                .gap_3()
                .child(
                    div()
                        .text_lg()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::text_primary())
                        .child(release.title),
                )
                .children(release.bullets.iter().map(|bullet| {
                    h_flex()
                        .items_start()
                        .gap_3()
                        .text_sm()
                        .text_color(theme::text_secondary())
                        .child(div().text_color(theme::text_muted()).child("•"))
                        .child(div().flex_1().min_w_0().child(*bullet))
                }))
        });

        div()
            .id("changelog-backdrop")
            .absolute()
            .inset_0()
            .bg(hsla(0.0, 0.0, 0.0, 0.58))
            .flex()
            .items_center()
            .justify_center()
            .p_6()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.active_modal = ActiveModal::None;
                    this.focus_active_surface_default(window, cx);
                    cx.notify();
                }),
            )
            .child(
                v_flex()
                    .id("changelog-modal")
                    .test_support()
                    .w(px(680.0))
                    .max_w(gpui_kit::relative(1.0))
                    .h(gpui_kit::relative(0.8))
                    .max_h(px(640.0))
                    .rounded(RADIUS_MODAL)
                    .overflow_hidden()
                    .bg(theme::bg_surface())
                    .border_1()
                    .border_color(theme::border_subtle())
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        h_flex()
                            .flex_shrink_0()
                            .px_6()
                            .py_4()
                            .border_b_1()
                            .border_color(theme::border_subtle())
                            .justify_between()
                            .items_center()
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme::text_primary())
                                    .child("Changelog"),
                            )
                            .child(
                                Button::new("close-changelog")
                                    .xsmall()
                                    .label("Esc")
                                    .text_xs()
                                    .text_color(theme::text_muted())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.active_modal = ActiveModal::None;
                                        this.focus_active_surface_default(window, cx);
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(
                        v_flex()
                            .id("changelog-scroll")
                            .test_support()
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .p_6()
                            .gap_6()
                            .children(sections),
                    ),
            )
    }

    pub(crate) fn render_shortcuts_modal(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self
            .inputs
            .shortcuts_search_input
            .read(cx)
            .value()
            .to_string();
        let tokens: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        let mut rows = v_flex().gap_1().p_3();
        let mut match_count = 0;
        for cat in ALL_CATEGORIES {
            let matches: Vec<_> = by_category(cat)
                .filter(|spec| {
                    let searchable = format!(
                        "{} {} {} {}",
                        spec.description,
                        cat.label(),
                        spec.display,
                        spec.gpui_key
                    )
                    .to_lowercase();
                    tokens.iter().all(|token| searchable.contains(token))
                })
                .collect();
            if matches.is_empty() {
                continue;
            }
            match_count += matches.len();
            rows = rows.child(
                div()
                    .px_3()
                    .pt_2()
                    .pb_0p5()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text_muted())
                    .child(cat.label()),
            );
            for spec in matches {
                rows = rows.child(
                    h_flex()
                        .id(format!("shortcut-{}", spec.id))
                        .test_support()
                        .px_3()
                        .py_1p5()
                        .rounded(RADIUS_ROW)
                        .justify_between()
                        .items_center()
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(theme::text_primary())
                                        .child(spec.description),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(if spec.wired {
                                            theme::accent_green()
                                        } else {
                                            theme::text_muted()
                                        })
                                        .child(if spec.wired { "active" } else { "planned" }),
                                ),
                        )
                        .child(
                            div()
                                .px_2p5()
                                .py_0p5()
                                .rounded(RADIUS_CHIP)
                                .bg(theme::bg_hover())
                                .text_xs()
                                .text_color(theme::text_muted())
                                .child(spec.display),
                        ),
                );
            }
        }

        if match_count == 0 {
            rows = rows.child(
                div()
                    .id("shortcuts-empty")
                    .test_support()
                    .p_3()
                    .text_sm()
                    .text_color(theme::text_muted())
                    .child(format!("No shortcuts match \"{query}\"")),
            );
        }

        div()
            .id("shortcuts-backdrop")
            .absolute()
            .inset_0()
            .bg(hsla(0.0, 0.0, 0.0, 0.58))
            .flex()
            .items_start()
            .justify_center()
            .pt(px(64.0))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.active_modal = ActiveModal::None;
                    this.focus_active_surface_default(window, cx);
                    cx.notify();
                }),
            )
            .child(
                v_flex()
                    .id("shortcuts-modal")
                    .test_support()
                    .w(px(640.0))
                    .max_h(px(580.0))
                    .rounded(RADIUS_MODAL)
                    .bg(theme::bg_surface())
                    .border_1()
                    .border_color(theme::border_subtle())
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .child(
                        h_flex()
                            .px_4()
                            .py_3()
                            .border_b_1()
                            .border_color(theme::border_subtle())
                            .justify_between()
                            .items_center()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme::text_primary())
                                    .child("Keyboard Shortcuts (Preserved Multibot Contract)"),
                            )
                            .child(
                                Button::new("close-shortcuts")
                                    .xsmall()
                                    .label("Esc")
                                    .text_xs()
                                    .text_color(theme::text_muted())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.active_modal = ActiveModal::None;
                                        this.focus_active_surface_default(window, cx);
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(div().px_4().py_2().flex_shrink_0().child(
                        Input::new(&self.inputs.shortcuts_search_input).id("shortcuts-search"),
                    ))
                    .child(
                        div()
                            .id("shortcuts-scroll")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .child(rows),
                    ),
            )
    }
}
