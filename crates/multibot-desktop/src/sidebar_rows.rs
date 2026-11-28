use super::*;

impl MultibotView {
    pub(super) fn render_session_badge(&self, row: &SidebarRow, size: f32) -> gpui_kit::Div {
        let meta = self
            .sidebar_state
            .workspace_store
            .session_meta
            .get(&row.session_key);
        let badge = div()
            .w(px(size))
            .h(px(size))
            .flex_shrink_0()
            .rounded(RADIUS_CHIP)
            .flex()
            .items_center()
            .justify_center();
        let Some(face) = meta.and_then(|meta| meta.face).filter(|face| *face < 20) else {
            return badge
                .bg(Self::agent_color(row.agent))
                .text_xs()
                .text_color(theme::bg_canvas())
                .child(row.agent.glyph());
        };
        let accent = meta
            .and_then(|meta| meta.color.as_deref())
            .and_then(parse_hex_color)
            .unwrap_or_else(|| Self::agent_color(row.agent));
        // Five eye styles crossed with four mouths give twenty asset-free faces.
        let (eye_width, left_height, right_height, eye_radius) = match face % 5 {
            0 => (0.12, 0.12, 0.12, 0.0),
            1 => (0.12, 0.12, 0.12, size),
            2 => (0.16, 0.06, 0.06, 0.0),
            3 => (0.12, 0.12, 0.04, size),
            _ => (0.07, 0.17, 0.17, 0.0),
        };
        let mouth_style = face / 5;
        let mouth_width = if mouth_style == 2 { 0.14 } else { 0.30 } * size;
        let mouth_height = if mouth_style == 0 { 0.06 } else { 0.14 } * size;
        let mouth = div()
            .absolute()
            .bottom(px(size * 0.08))
            .left(px((size * 0.70 - mouth_width) / 2.0 - 1.0))
            .w(px(mouth_width))
            .h(px(mouth_height))
            .border_1()
            .border_color(accent)
            .when(mouth_style == 0, |el| el.bg(accent))
            .when(mouth_style == 2, |el| el.rounded(RADIUS_CHIP))
            .when(mouth_style == 3, |el| {
                el.child(
                    div()
                        .absolute()
                        .left(px(mouth_width / 3.0))
                        .h_full()
                        .w(px(1.0))
                        .bg(accent),
                )
                .child(
                    div()
                        .absolute()
                        .right(px(mouth_width / 3.0))
                        .h_full()
                        .w(px(1.0))
                        .bg(accent),
                )
            });
        badge.bg(theme::bg_surface()).child(
            div()
                .relative()
                .w(px(size * 0.70))
                .h(px(size * 0.62))
                .rounded(px(4.0))
                .border_1()
                .border_color(accent)
                .child(
                    div()
                        .absolute()
                        .left(px(size * 0.12))
                        .top(px(size * 0.10))
                        .w(px(size * eye_width))
                        .h(px(size * left_height))
                        .rounded(px(eye_radius))
                        .bg(accent),
                )
                .child(
                    div()
                        .absolute()
                        .right(px(size * 0.12))
                        .top(px(size * 0.10))
                        .w(px(size * eye_width))
                        .h(px(size * right_height))
                        .rounded(px(eye_radius))
                        .bg(accent),
                )
                .child(mouth),
        )
    }

    pub(super) fn render_dev_stats_indicator(
        &self,
        rail: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (gpu_label, gpu_safe) = self.active_gpu_summary();
        v_flex()
            .relative()
            .items_start()
            .when(self.sidebar_state.dev_stats_expanded, |el| {
                el.child(
                    gpui_kit::deferred(
                        v_flex()
                            .id("dev-stats-details")
                            .test_support()
                            .absolute()
                            .bottom(px(28.0))
                            .left_0()
                            .w(px(280.0))
                            .p_2()
                            .gap_1()
                            .rounded(RADIUS_ROW)
                            .border_1()
                            .border_color(theme::border_subtle())
                            .occlude()
                            .bg(theme::bg_surface())
                            .text_xs()
                            .text_color(theme::text_secondary())
                            .child(
                                Label::new(format!("GPU: {gpu_label}"))
                                    .id("dev-stats-gpu")
                                    .text_color(theme::text_secondary()),
                            )
                            .child(
                                Label::new(self.footprint_label.clone())
                                    .id("dev-stats-footprint")
                                    .text_color(theme::text_secondary()),
                            )
                            .child(
                                Label::new(self.connection.connection_status.clone())
                                    .id("dev-stats-connection")
                                    .text_color(theme::text_secondary()),
                            )
                            .child(
                                Button::new("dev-stats-gpu-settings")
                                    .small()
                                    .label("GPU settings")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.sidebar_state.dev_stats_expanded = false;
                                        this.table_modal_state.close();
                                        this.settings_tab = crate::settings::SettingsTab::Gpu;
                                        this.set_main_surface(MainSurface::Settings);
                                        this.active_modal = ActiveModal::None;
                                        this.focus_active_surface_default(window, cx);
                                        cx.notify();
                                    })),
                            ),
                    )
                    .with_priority(1),
                )
            })
            .child(
                h_flex()
                    .id("gpu-status-card")
                    .role(Role::Button)
                    .aria_label("Developer stats")
                    .aria_expanded(self.sidebar_state.dev_stats_expanded)
                    .test_support()
                    .h(px(26.0))
                    .px_2p5()
                    .bg(theme::bg_surface())
                    .border_1()
                    .border_color(theme::border_subtle())
                    .gap_1()
                    .items_center()
                    .rounded(RADIUS_CHIP)
                    .text_xs()
                    .text_color(theme::text_muted())
                    .hover(|s| s.bg(theme::bg_hover()))
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            cx.stop_propagation();
                            this.shell_host.focus_parent();
                            this.focus.root_focus_handle.focus(window, cx);
                            this.sidebar_state.dev_stats_expanded =
                                !this.sidebar_state.dev_stats_expanded;
                            cx.notify();
                        }),
                    )
                    .child(
                        div()
                            .w(px(4.0))
                            .h(px(4.0))
                            .rounded(RADIUS_CHIP)
                            .bg(if gpu_safe {
                                theme::accent_green()
                            } else {
                                theme::accent_amber()
                            }),
                    )
                    .when(!rail, |el| {
                        el.child(format!("v{}", multibot_proto::build_version()))
                    }),
            )
    }

    pub(super) fn render_sidebar_session_row(
        &self,
        id: SharedString,
        row: &SidebarRow,
        is_selected: bool,
        jump_digit: Option<usize>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let key_left = row.session_key.clone();
        let key_right = row.session_key.clone();
        let is_renaming = self
            .sidebar_state
            .rename
            .as_ref()
            .is_some_and(|r| r.session_key == row.session_key);
        let rename_draft = if is_renaming {
            self.sidebar_state
                .rename
                .as_ref()
                .map(InlineRenameState::render_text)
        } else {
            None
        };
        let session = self.find_session_by_key(&row.session_key);
        let subtitle = session_subtitle(
            row.agent,
            row.cwd.as_deref(),
            session
                .as_ref()
                .and_then(|s| s.last_assistant_preview.as_deref()),
        );

        div()
            .id(id)
            .role(Role::Button)
            .test_support()
            .px_2p5()
            .py_2()
            .rounded(RADIUS_ROW)
            .bg(if is_selected {
                theme::bg_active_row()
            } else {
                theme::bg_sidebar()
            })
            .hover(|s| s.bg(theme::bg_hover()))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    if let Some((host, sess)) = this.host_for_session_key(&key_left) {
                        this.open_session_on_host(host, sess, false, cx);
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, window, cx| {
                    this.open_palette_tab_context(key_right.clone(), window, cx);
                    cx.stop_propagation();
                }),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_2p5()
                    .child(self.render_session_badge(row, 26.0))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(if let Some(draft_text) = rename_draft {
                                div()
                                    .id("sidebar-rename-input")
                                    .px_1p5()
                                    .py_0p5()
                                    .rounded(px(6.0))
                                    .bg(theme::bg_surface())
                                    .border_1()
                                    .border_color(theme::accent_blue())
                                    .text_sm()
                                    .text_color(theme::text_primary())
                                    .truncate()
                                    .child(draft_text)
                                    .into_any_element()
                            } else {
                                div()
                                    .text_sm()
                                    .font_weight(if row.unread {
                                        FontWeight::SEMIBOLD
                                    } else {
                                        FontWeight::NORMAL
                                    })
                                    .text_color(if row.muted {
                                        theme::text_muted()
                                    } else {
                                        theme::text_primary()
                                    })
                                    .truncate()
                                    .child(row.title.clone())
                                    .into_any_element()
                            })
                            .child(
                                div()
                                    .id(SharedString::from(format!(
                                        "sidebar-subtitle-{}",
                                        row.session_key
                                    )))
                                    .test_support()
                                    .text_xs()
                                    .text_color(theme::text_muted())
                                    .truncate()
                                    .child(Label::new(subtitle)),
                            ),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1()
                            .when_some(
                                session.as_ref().filter(|s| !s.subworkers.is_empty()),
                                |el, session| {
                                    let count = session.subworkers.len();
                                    let tooltip = format!(
                                        "{count} active {}\n{}",
                                        if count == 1 {
                                            "subworker"
                                        } else {
                                            "subworkers"
                                        },
                                        session
                                            .subworkers
                                            .iter()
                                            .map(|worker| worker.label.as_str())
                                            .collect::<Vec<_>>()
                                            .join("\n")
                                    );
                                    el.child(
                                        div()
                                            .id(format!("sidebar-subworkers-{}", row.session_key))
                                            .test_support()
                                            .flex_shrink_0()
                                            .px_1p5()
                                            .py_0p5()
                                            .rounded(RADIUS_CHIP)
                                            .bg(theme::bg_surface())
                                            .text_xs()
                                            .text_color(theme::text_muted())
                                            .tooltip(move |window, cx| {
                                                gpui_kit::component::tooltip::Tooltip::new(
                                                    tooltip.clone(),
                                                )
                                                .build(window, cx)
                                            })
                                            .child(
                                                Label::new(format!("{count}w"))
                                                    .text_xs()
                                                    .text_color(theme::text_muted()),
                                            ),
                                    )
                                },
                            )
                            .when(row.favorite, |el| {
                                el.child(
                                    lucide_icon(icons::STAR_FILLED, 14.0)
                                        .text_color(theme::accent_amber()),
                                )
                            })
                            .when(row.working, |el| {
                                el.child(
                                    div()
                                        .w(px(7.0))
                                        .h(px(7.0))
                                        .rounded(RADIUS_CHIP)
                                        .bg(theme::accent_green()),
                                )
                            })
                            .when(row.unread, |el| {
                                el.child(
                                    div()
                                        .w(px(7.0))
                                        .h(px(7.0))
                                        .rounded(RADIUS_CHIP)
                                        .bg(theme::accent_blue()),
                                )
                            })
                            .when_some(jump_digit, |el, digit| {
                                el.child(
                                    div()
                                        .px_1p5()
                                        .py_0p5()
                                        .rounded(RADIUS_CHIP)
                                        .bg(theme::bg_surface())
                                        .text_xs()
                                        .text_color(theme::accent_blue())
                                        .child(digit.to_string()),
                                )
                            }),
                    ),
            )
    }

    pub(super) fn render_collapsed_sidebar(
        &self,
        layout: multibot_core::workspaces::SidebarLayout,
        unread_count: usize,
        jump_badges: bool,
        now: i64,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active_key = self
            .selection
            .selected_session
            .as_ref()
            .map(|s| self.session_key_for(s));

        // Derive 1..9 jump badges from layout.ctrl_digit_order (matching Open sidebar & Ctrl+1..9)
        let ctrl_digit_order = layout.ctrl_digit_order;
        let jump_index_for = |session_key: &str| -> Option<usize> {
            ctrl_digit_order
                .iter()
                .position(|k| k == session_key)
                .filter(|&i| i < 9)
                .map(|i| i + 1)
        };

        // Match the open sidebar, including deliberate Ctrl+G group collapse.
        let mut rail_rows = layout.pinned_tiles;
        for gv in layout.groups {
            if !gv.group.collapsed {
                rail_rows.extend(gv.items);
            }
        }
        rail_rows.extend(layout.recents);

        let mut ws_rail = v_flex().gap_1().items_center();
        let in_starred = self.sidebar_state.active_workspace_id == STARRED_WORKSPACE_ID;
        ws_rail = ws_rail.child(
            div()
                .id("ws-pill-starred")
                .role(Role::Button)
                .test_support()
                .w(px(28.0))
                .h(px(24.0))
                .rounded(RADIUS_CHIP)
                .bg(if in_starred {
                    theme::bg_active_row()
                } else {
                    theme::bg_surface()
                })
                .border_1()
                .border_color(if in_starred {
                    theme::accent_amber()
                } else {
                    theme::border_subtle()
                })
                .flex()
                .items_center()
                .justify_center()
                .text_xs()
                .text_color(theme::accent_amber())
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.sidebar_state.active_workspace_id = STARRED_WORKSPACE_ID.to_string();
                        this.sidebar_state.rename = None;
                        this.cleanup_unmounted_surface_state();
                        cx.notify();
                    }),
                )
                .child(lucide_icon(icons::STAR_FILLED, 14.0)),
        );

        for (idx, ws) in self
            .sidebar_state
            .workspace_store
            .visible_workspaces(now)
            .into_iter()
            .take(5)
            .enumerate()
        {
            let active = self.sidebar_state.active_workspace_id == ws.id;
            let ws_id = ws.id.clone();
            let initial: String = ws
                .name
                .chars()
                .next()
                .map(|c| c.to_ascii_uppercase().to_string())
                .unwrap_or_else(|| "W".into());
            let accent = ws
                .color
                .as_deref()
                .and_then(parse_hex_color)
                .unwrap_or_else(theme::text_secondary);
            ws_rail = ws_rail.child(
                div()
                    .id(SharedString::from(format!("ws-pill-{idx}")))
                    .role(Role::Button)
                    .test_support()
                    .w(px(28.0))
                    .h(px(24.0))
                    .rounded(RADIUS_CHIP)
                    .bg(if active {
                        theme::bg_active_row()
                    } else {
                        theme::bg_surface()
                    })
                    .border_1()
                    .border_color(if active {
                        accent
                    } else {
                        theme::border_subtle()
                    })
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_xs()
                    .text_color(if active {
                        theme::text_primary()
                    } else {
                        theme::text_muted()
                    })
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.sidebar_state.active_workspace_id = ws_id.clone();
                            this.sidebar_state.rename = None;
                            this.cleanup_unmounted_surface_state();
                            cx.notify();
                        }),
                    )
                    .child(initial),
            );
        }

        let mut avatars = v_flex().gap_1p5().items_center();
        for (idx, row) in rail_rows.iter().enumerate() {
            let is_selected = active_key.as_deref() == Some(row.session_key.as_str());
            let key_left = row.session_key.clone();
            let key_right = row.session_key.clone();
            let badge = if jump_badges {
                jump_index_for(&row.session_key).map(|n| n.to_string())
            } else {
                None
            };
            let dot_color = if row.working {
                Some(theme::accent_green())
            } else if row.unread {
                Some(theme::accent_blue())
            } else if row.favorite {
                Some(theme::accent_amber())
            } else {
                None
            };

            avatars = avatars.child(
                div()
                    .id(SharedString::from(format!("rail-session-{idx}")))
                    .role(Role::Button)
                    .test_support()
                    .relative()
                    .w(px(34.0))
                    .h(px(34.0))
                    .rounded(RADIUS_CHIP)
                    .bg(Self::agent_color(row.agent))
                    .border_1()
                    .border_color(if is_selected {
                        theme::text_primary()
                    } else {
                        theme::bg_sidebar()
                    })
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_xs()
                    .text_color(theme::bg_canvas())
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            if let Some((host, sess)) = this.host_for_session_key(&key_left) {
                                this.open_session_on_host(host, sess, false, cx);
                            }
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, _, window, cx| {
                            this.open_palette_tab_context(key_right.clone(), window, cx);
                            cx.stop_propagation();
                        }),
                    )
                    .child(if let Some(digit) = badge {
                        div().child(digit)
                    } else {
                        self.render_session_badge(row, 32.0)
                    })
                    .when_some(dot_color, |el, col| {
                        el.child(
                            div()
                                .absolute()
                                .right_0()
                                .bottom_0()
                                .w(px(8.0))
                                .h(px(8.0))
                                .rounded(RADIUS_CHIP)
                                .bg(col)
                                .border_1()
                                .border_color(theme::bg_sidebar()),
                        )
                    }),
            );
        }

        v_flex()
            .id("sidebar-rail")
            .test_support()
            .w(px(56.0))
            .h_full()
            .bg(theme::bg_sidebar())
            .border_r_1()
            .border_color(theme::border_subtle())
            .py_2()
            .px_1p5()
            .gap_2()
            .items_center()
            .on_modifiers_changed(cx.listener(|this, ev: &ModifiersChangedEvent, _, cx| {
                this.handle_modifiers_changed(ev, cx);
            }))
            .child(
                Button::new("open-palette-btn")
                    .xsmall()
                    .icon(lucide_icon(icons::COMMAND, 16.0))
                    .label("K")
                    .accessibility_label("Command palette")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_palette(window, cx);
                    })),
            )
            .when(self.main_surface != MainSurface::Inbox, |el| {
                el.child(
                    div()
                        .id("sidebar-inbox-btn")
                        .role(Role::Button)
                        .test_support()
                        .w(px(32.0))
                        .h(px(26.0))
                        .rounded(RADIUS_CHIP)
                        .bg(theme::bg_surface())
                        .border_1()
                        .border_color(if unread_count > 0 {
                            theme::accent_blue()
                        } else {
                            theme::border_subtle()
                        })
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_xs()
                        .text_color(if unread_count > 0 {
                            theme::accent_blue()
                        } else {
                            theme::text_muted()
                        })
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                cx.stop_propagation();
                                if this.table_modal_state.is_open() {
                                    this.table_modal_state.close();
                                }
                                this.active_modal = ActiveModal::None;
                                this.set_main_surface(MainSurface::Inbox);
                                this.focus_inbox(window, cx);
                                cx.notify();
                            }),
                        )
                        .child(if unread_count > 0 {
                            unread_count.to_string()
                        } else {
                            "In".to_string()
                        }),
                )
            })
            .child(ws_rail)
            .child(
                div()
                    .id("sessions-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(avatars),
            )
            // Update card intentionally omitted in Collapsed rail mode to prevent overflow.
            .child(self.render_dev_stats_indicator(true, cx))
    }
}
