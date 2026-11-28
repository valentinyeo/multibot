use super::*;
use gpui_kit::{AppContext, Entity, Focusable, TestAppContext, WindowOptions, test::TestWindowExt};
use multibot_core::workspaces::DEFAULT_WORKSPACE_ID;
use multibot_core::workspaces::canonicalize_session_title;
use multibot_proto::AgentKind;
use std::sync::Arc;

fn sample_sessions() -> Vec<SessionSummary> {
    vec![
        SessionSummary {
            id: "s1".into(),
            agent: AgentKind::Claude,
            title: "Pinned Chat".into(),
            cwd: Some("/repo/app".into()),
            source_path: "/tmp/s1.jsonl".into(),
            updated_at_ms: 5_000,
            working: true,
            needs_attention: true,
            last_assistant_id: Some("a1".into()),
            last_assistant_ms: Some(5_000),
            ..Default::default()
        },
        SessionSummary {
            id: "s2".into(),
            agent: AgentKind::Codex,
            title: "Grouped Chat".into(),
            cwd: Some("/repo/api".into()),
            source_path: "/tmp/s2.jsonl".into(),
            updated_at_ms: 4_000,
            needs_attention: true,
            last_assistant_id: Some("a2".into()),
            last_assistant_ms: Some(4_000),
            ..Default::default()
        },
        SessionSummary {
            id: "s3".into(),
            agent: AgentKind::Antigravity,
            title: "Recent Read Chat".into(),
            cwd: Some("/repo/ui".into()),
            source_path: "/tmp/s3.jsonl".into(),
            updated_at_ms: 3_000,
            needs_attention: false,
            ..Default::default()
        },
    ]
}

fn open_view(cx: &mut TestAppContext) -> (gpui_kit::AnyWindowHandle, Entity<MultibotView>) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        crate::theme::apply_multibot_dark_theme(cx);
        crate::bind_multibot_keys(cx);
    });
    let rt = Arc::new(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap(),
    );
    cx.update(|cx| {
        gpui_kit::open_window(WindowOptions::default(), cx, move |window, cx| {
            cx.new(|cx| {
                let mut view = MultibotView::new(rt, window, cx);
                view.sidebar_state.active_workspace_id = DEFAULT_WORKSPACE_ID.to_string();
                view
            })
        })
        .unwrap()
    })
}

#[gpui_kit::test]
fn session_verticons_are_portrait_in_rows_pins_and_rail(cx: &mut TestAppContext) {
    let (handle, view) = open_view(cx);
    cx.update_window(handle, |_, window, cx| {
        let keys = view.update(cx, |v, cx| {
            v.sidebar_state.workspace_store = WorkspaceStore::new();
            v.connection.sessions = sample_sessions();
            let keys = v
                .connection
                .sessions
                .iter()
                .map(|session| v.session_key_for(session))
                .collect::<Vec<_>>();
            v.sidebar_state.workspace_store.toggle_pin(&keys[0], false);
            v.set_sidebar_mode_synced(SidebarMode::Open, cx);
            cx.notify();
            keys
        });
        window.render_frame(cx);
        let row = window.find("session-0").bounds();
        let icon = window.find("session-0-verticon").bounds();
        assert_eq!(icon.size, gpui_kit::size(px(24.0), px(34.0)));
        assert!(icon.size.height > icon.size.width);
        assert_eq!(icon.left() - row.left(), px(10.0));
        assert_eq!(icon.center().y, row.center().y);
        let pin = window.find("sidebar-pin-0").bounds();
        let pinned_icon = window.find("sidebar-pin-0-verticon").bounds();
        assert_eq!(pinned_icon.size, gpui_kit::size(px(28.0), px(40.0)));
        let subtitle_id = gpui_kit::ElementId::Name(format!("sidebar-subtitle-{}", keys[0]).into());
        let subtitle = window.find(subtitle_id).bounds();
        assert!(pinned_icon.top() >= pin.top());
        assert!(pinned_icon.bottom() < subtitle.top());
        assert!(subtitle.bottom() <= pin.bottom());
        for face in 0..24 {
            view.update(cx, |v, cx| {
                let meta = v
                    .sidebar_state
                    .workspace_store
                    .session_meta
                    .entry(keys[1].clone())
                    .or_default();
                meta.face = Some(face);
                cx.notify();
            });
            window.render_frame(cx);
            assert_eq!(
                window.find("session-0-verticon").bounds().size,
                gpui_kit::size(px(24.0), px(34.0)),
                "face {face}"
            );
        }
        view.update(cx, |v, cx| {
            v.sidebar_state.show_jump_badges = false;
            v.set_sidebar_mode_synced(SidebarMode::Collapsed, cx);
            cx.notify();
        });
        window.render_frame(cx);
        let rail = window.find("sidebar-rail").bounds();
        let rail_icon = window.find("rail-session-0-verticon").bounds();
        assert_eq!(rail.size.width, px(56.0));
        assert_eq!(rail_icon.size, gpui_kit::size(px(24.0), px(34.0)));
        assert!(f32::from(rail_icon.center().x - rail.center().x).abs() <= 1.0);
    })
    .unwrap();
}

#[gpui_kit::test]
fn sidebar_subworker_badges_only_show_for_live_workers(cx: &mut TestAppContext) {
    let (handle, view) = open_view(cx);
    cx.update_window(handle, |_, window, cx| {
        let keys = view.update(cx, |v, cx| {
            v.sidebar_state.workspace_store = WorkspaceStore::new();
            v.connection.sessions = sample_sessions();
            v.connection.sessions[0].subworkers = vec![multibot_proto::SubworkerInfo {
                pid: 101,
                label: "MBRU-117".into(),
                ..Default::default()
            }];
            let keys = v
                .connection
                .sessions
                .iter()
                .map(|s| v.session_key_for(s))
                .collect::<Vec<_>>();
            cx.notify();
            keys
        });
        window.render_frame(cx);
        for (index, key) in keys.iter().enumerate() {
            let id =
                gpui_kit::ElementId::from(SharedString::from(format!("sidebar-subworkers-{key}")));
            if index == 0 {
                assert!(window.find(id.clone()).visible());
                assert!(
                    gpui_kit::base::test_support::texts(window)
                        .iter()
                        .any(|text| text.text() == "1w" && text.path().contains(&id))
                );
            } else {
                assert!(window.try_find(id).is_none());
            }
        }
        view.update(cx, |v, cx| {
            v.connection.sessions[0].subworkers.clear();
            cx.notify();
        });
        window.render_frame(cx);
        assert!(
            window
                .try_find(gpui_kit::ElementId::from(SharedString::from(format!(
                    "sidebar-subworkers-{}",
                    keys[0]
                ))))
                .is_none()
        );
    })
    .unwrap();
}

#[test]
fn shell_confirmation_is_single_use_session_scoped_and_expires() {
    let mut state = SidebarState::default();
    let now = std::time::Instant::now();
    assert!(!state.confirm_shell_dissolve("host-a:shell:same-id", now));
    assert!(!state.confirm_shell_dissolve("host-b:shell:same-id", now));
    assert!(state.confirm_shell_dissolve("host-b:shell:same-id", now));
    assert!(!state.confirm_shell_dissolve("host-b:shell:same-id", now));
    let expired = now + std::time::Duration::from_secs(8);
    assert!(!state.confirm_shell_dissolve("host-b:shell:same-id", expired));
    assert!(state.confirm_shell_dissolve("host-b:shell:same-id", expired));
}

#[test]
fn sidebar_3_state_cycle() {
    let mut state = SidebarState::default();
    assert_eq!(state.mode, SidebarMode::Open);
    assert!(state.is_shown());

    state.cycle();
    assert_eq!(state.mode, SidebarMode::Collapsed);
    assert!(state.is_shown());

    state.cycle();
    assert_eq!(state.mode, SidebarMode::Hidden);
    assert!(!state.is_shown());

    state.cycle();
    assert_eq!(state.mode, SidebarMode::Open);
    assert!(state.is_shown());
}

#[test]
fn jump_badges_are_altgr_safe() {
    assert!(should_show_jump_badges(Modifiers {
        control: true,
        ..Default::default()
    }));
    // AltGr = Ctrl+Alt -> MUST return false
    assert!(!should_show_jump_badges(Modifiers {
        control: true,
        alt: true,
        ..Default::default()
    }));
    assert!(!should_show_jump_badges(Modifiers {
        control: true,
        shift: true,
        ..Default::default()
    }));
    assert!(!should_show_jump_badges(Modifiers {
        control: true,
        platform: true,
        ..Default::default()
    }));
    assert!(!should_show_jump_badges(Modifiers::default()));
}

#[test]
fn inline_rename_handles_keys_and_canonical_rules() {
    let mut r = InlineRenameState::new("local:claude:s1".into(), "s1".into(), "Old".into());
    assert_eq!(
        r.handle_key("backspace", None, Modifiers::default()),
        RenameKeyOutcome::Consumed
    );
    assert_eq!(r.draft, "");
    assert_eq!(
        r.handle_key("k", Some("k"), Modifiers::default()),
        RenameKeyOutcome::Consumed
    );
    assert_eq!(r.draft, "k");
    assert_eq!(
        r.handle_key("enter", None, Modifiers::default()),
        RenameKeyOutcome::Commit
    );
    assert_eq!(
        r.handle_key("escape", None, Modifiers::default()),
        RenameKeyOutcome::Cancel
    );

    // Blank rejected, >63 Unicode chars capped
    assert_eq!(canonicalize_session_title("   "), None);
    let long = "日".repeat(80);
    assert_eq!(
        canonicalize_session_title(&long).unwrap().chars().count(),
        63
    );
}

#[gpui_kit::test]
fn renders_pinned_groups_recents_and_cycles_three_modes(cx: &mut TestAppContext) {
    let (handle, view) = open_view(cx);
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.connection.sessions = sample_sessions();
            let k1 = v.session_key_for(&v.connection.sessions[0]);
            let k2 = v.session_key_for(&v.connection.sessions[1]);
            v.sidebar_state.workspace_store.toggle_pin(&k1, false);
            v.sidebar_state.workspace_store.toggle_favorite(&k1);
            v.sidebar_state.workspace_store.toggle_group_for_session(
                &k2,
                DEFAULT_WORKSPACE_ID,
                None,
                "Backend",
            );
            v.sidebar_state.unread_only = true;
            cx.notify();
        });
        window.render_frame(cx);

        // Pinned grid rendered, group rendered, unread-only pill rendered
        assert!(window.try_find("sidebar-pinned-grid").is_some());
        assert!(window.try_find("sidebar-group-0").is_some());
        assert!(window.try_find("sidebar-unread-only-pill").is_some());

        // Clicking the group header leaves it open; turn off unread-only.
        window.click("sidebar-group-0", cx);
        window.render_frame(cx);
        window.click("sidebar-unread-only-pill", cx);
        window.render_frame(cx);
        assert!(window.try_find("sidebar-unread-only-pill").is_none());

        // Switch to Favorites and create a new workspace
        window.click("workspace-switcher", cx);
        window.render_frame(cx);
        window.click("pal-ws-2", cx);
        window.render_frame(cx);
        window.press("ctrl-k", cx);
        window.input("New workspace", cx);
        window.press("enter", cx);
        window.render_frame(cx);

        // Cycle Open -> Collapsed -> Hidden -> Open
        window.press("ctrl-shift-b", cx);
        window.render_frame(cx);
        assert!(window.try_find("sidebar-rail").is_some());
        assert!(window.try_find("gpu-status-card").is_some());

        window.press("ctrl-shift-b", cx);
        window.render_frame(cx);
        assert!(window.try_find("sidebar-rail").is_none());
        assert!(window.try_find("sidebar-open").is_none());

        window.press("ctrl-shift-b", cx);
        window.render_frame(cx);
        assert!(window.try_find("sidebar-open").is_some());
    })
    .unwrap();
}

#[gpui_kit::test]
fn section_headers_show_names_and_clicks_keep_sessions_visible(cx: &mut TestAppContext) {
    let (handle, view) = open_view(cx);
    cx.update_window(handle, |_, window, cx| {
        for name in ["app.hypertask.ai", "Marketing / Help / Docs", "  "] {
            view.update(cx, |v, cx| {
                v.sidebar_state.workspace_store = WorkspaceStore::new();
                v.connection.sessions = sample_sessions();
                let pin = v.session_key_for(&v.connection.sessions[0]);
                let grouped = v.session_key_for(&v.connection.sessions[1]);
                v.sidebar_state.workspace_store.toggle_pin(&pin, false);
                let group =
                    v.sidebar_state
                        .workspace_store
                        .create_group(DEFAULT_WORKSPACE_ID, name, None);
                v.sidebar_state.workspace_store.toggle_group_for_session(
                    &grouped,
                    DEFAULT_WORKSPACE_ID,
                    Some(&group.id),
                    "",
                );
                // Simulate an older persisted group without a usable name.
                v.sidebar_state.workspace_store.groups[0].name = name.into();
                cx.notify();
            });
            window.render_frame(cx);
            for (id, expected) in [
                ("sidebar-pinned-header", "Pinned"),
                (
                    "sidebar-group-0",
                    if name.trim().is_empty() {
                        "Group"
                    } else {
                        name
                    },
                ),
                ("sidebar-recents-header", "Recents"),
            ] {
                let path = window.find(id).path().to_vec();
                assert!(
                    gpui_kit::base::test_support::texts(window)
                        .iter()
                        .any(|text| {
                            text.visible()
                                && text.path().starts_with(&path)
                                && text.text() == expected
                        }),
                    "missing visible section name {expected}"
                );
                window.click(id, cx);
                window.render_frame(cx);
                assert!(window.find("sidebar-pin-0").visible());
                assert!(window.find("sidebar-group-0-item-0").visible());
                assert!(window.find("session-0").visible());
                view.read_with(cx, |v, _| {
                    assert!(!v.sidebar_state.workspace_store.groups[0].collapsed)
                });
            }
        }
    })
    .unwrap();
}

#[gpui_kit::test]
fn right_click_scopes_shared_commands_to_pinned_grouped_recent_and_rail_sessions(
    cx: &mut TestAppContext,
) {
    use multibot_core::workspaces::PaletteRow;
    let (handle, view) = open_view(cx);
    cx.update_window(handle, |_, window, cx| {
        let keys = view.update(cx, |v, cx| {
            v.connection.sessions = sample_sessions();
            let keys: Vec<_> = v.connection.sessions.iter().map(|s| v.session_key_for(s)).collect();
            v.sidebar_state.workspace_store.toggle_pin(&keys[0], false);
            v.sidebar_state.workspace_store.toggle_group_for_session(&keys[1], DEFAULT_WORKSPACE_ID, None, "Backend");
            v.select_session(v.connection.sessions[0].clone(), cx);
            cx.notify();
            keys
        });
        for (mode, ids) in [
            (SidebarMode::Open, ["sidebar-pin-0", "sidebar-group-0-item-0", "session-0"]),
            (SidebarMode::Collapsed, ["rail-session-0", "rail-session-1", "rail-session-2"]),
        ] {
            view.update(cx, |v, cx| { v.sidebar_state.mode = mode; cx.notify(); });
            for (id, key) in ids.into_iter().zip(&keys) {
                let before = view.read(cx).sidebar_state.workspace_store.favorites.clone();
                window.render_frame(cx);
                window.right_click(id, cx);
                window.render_frame(cx);
                assert!(window.find("palette-context-banner").visible());
                view.read_with(cx, |v, _| {
                    assert_eq!(v.active_modal, ActiveModal::Palette);
                    assert_eq!(v.palette_state.mode.target_session_key(), Some(key.as_str()));
                    assert_eq!(v.selection.selected_session.as_ref().unwrap().id, "s1");
                    let commands = v.build_palette_command_catalog(10000);
                    let rows = v.build_palette_rows("", 10000);
                    for command in commands.iter().filter(|c| c.tab_context) {
                        assert!(rows.iter().any(|row| matches!(row, PaletteRow::Command { id, .. } if *id == command.id)), "missing shared command {}", command.id);
                    }
                });
                window.click("pal-cmd-favorite", cx);
                window.render_frame(cx);
                view.read_with(cx, |v, _| {
                    assert_eq!(v.sidebar_state.workspace_store.is_favorite(key), !before.contains(key));
                    for other in keys.iter().filter(|other| *other != key) {
                        assert_eq!(v.sidebar_state.workspace_store.is_favorite(other), before.contains(other));
                    }
                });
            }
        }
    }).unwrap();
}

#[gpui_kit::test]
fn sidebar_group_activity_order_survives_selection_and_palette_pin_round_trip(
    cx: &mut TestAppContext,
) {
    let (handle, view) = open_view(cx);
    cx.update_window(handle, |_, window, cx| {
        for workspace_id in [DEFAULT_WORKSPACE_ID, STARRED_WORKSPACE_ID] {
            let keys = view.update(cx, |v, cx| {
                v.sidebar_state.workspace_store = WorkspaceStore::new();
                v.sidebar_state.active_workspace_id = workspace_id.into();
                v.connection.sessions = sample_sessions();
                v.connection.sessions[0].title = "Chief of Staff".into();
                v.connection.sessions[0].updated_at_ms = 100;
                v.connection.sessions[0].last_assistant_ms = Some(100);
                v.connection.sessions[1].title = "Supervisor".into();
                v.connection.sessions[2].last_assistant_ms = Some(6000);
                let keys: Vec<_> = v
                    .connection
                    .sessions
                    .iter()
                    .map(|s| v.session_key_for(s))
                    .collect();
                let group = v.sidebar_state.workspace_store.create_group(
                    workspace_id,
                    "app.hypertask.ai",
                    None,
                );
                for key in &keys {
                    v.sidebar_state
                        .workspace_store
                        .favorites
                        .insert(key.clone());
                    v.sidebar_state.workspace_store.toggle_group_for_session(
                        key,
                        workspace_id,
                        Some(&group.id),
                        "",
                    );
                }
                cx.notify();
                keys
            });
            let expected = vec![keys[2].clone(), keys[1].clone(), keys[0].clone()];
            window.render_frame(cx);
            for row in ["sidebar-group-0-item-2", "sidebar-group-0-item-1"] {
                window.click(row, cx);
                window.render_frame(cx);
                view.read_with(cx, |v, _| {
                    let layout = v.sidebar_state.workspace_store.compute_sidebar_layout(
                        workspace_id,
                        &v.session_inputs(),
                        false,
                        now_unix_ms(),
                    );
                    assert_eq!(layout.ctrl_digit_order, expected);
                });
            }
            window.press("ctrl-3", cx);
            window.render_frame(cx);
            view.read_with(cx, |v, _| {
                assert_eq!(v.selection.selected_session.as_ref().unwrap().id, "s1")
            });
            window.right_click("sidebar-group-0-item-2", cx);
            window.render_frame(cx);
            window.input("pin", cx);
            window.render_frame(cx);
            window.click("pal-cmd-pin", cx);
            window.render_frame(cx);
            assert!(window.find("sidebar-pin-0").visible());
            view.read_with(cx, |v, _| {
                let layout = v.sidebar_state.workspace_store.compute_sidebar_layout(
                    workspace_id,
                    &v.session_inputs(),
                    false,
                    now_unix_ms(),
                );
                assert_eq!(layout.pinned_tiles[0].session_key, keys[0]);
                assert_eq!(layout.groups[0].items.len(), 2);
            });
            window.right_click("sidebar-pin-0", cx);
            window.render_frame(cx);
            window.input("pin", cx);
            window.render_frame(cx);
            window.click("pal-cmd-pin", cx);
            window.render_frame(cx);
            assert!(window.try_find("sidebar-pinned-grid").is_none());
            assert!(window.find("sidebar-group-0-item-2").visible());
            view.read_with(cx, |v, _| {
                let store = &v.sidebar_state.workspace_store;
                assert!(!store.is_pinned(&keys[0], workspace_id == STARRED_WORKSPACE_ID));
                let layout = store.compute_sidebar_layout(
                    workspace_id,
                    &v.session_inputs(),
                    false,
                    now_unix_ms(),
                );
                assert_eq!(layout.ctrl_digit_order, expected);
            });
        }
    })
    .unwrap();
}

#[gpui_kit::test]
fn sidebar_clicks_and_ctrl_digits_select_in_place(cx: &mut TestAppContext) {
    let (handle, view) = open_view(cx);
    cx.update_window(handle, |_, window, cx| {
        for workspace_id in [ALL_WORKSPACES_ID, DEFAULT_WORKSPACE_ID] {
            for auto_sort in [true, false] {
                let keys = view.update(cx, |v, cx| {
                    v.sidebar_state.workspace_store = WorkspaceStore::new();
                    v.sidebar_state.workspace_store.auto_sort = auto_sort;
                    v.sidebar_state.active_workspace_id = workspace_id.to_string();
                    v.connection.sessions = (0..9)
                        .map(|i| SessionSummary {
                            id: format!("select-{i}"),
                            title: format!("Session {i}"),
                            agent: AgentKind::Claude,
                            updated_at_ms: 10_000 - i as i64,
                            needs_attention: true,
                            ..Default::default()
                        })
                        .collect();
                    let keys: Vec<_> = v
                        .connection
                        .sessions
                        .iter()
                        .map(|s| v.session_key_for(s))
                        .collect();
                    v.select_session(v.connection.sessions[0].clone(), cx);
                    keys
                });
                window.render_frame(cx);

                for click in [true, false] {
                    for index in (0..9).rev() {
                        if click {
                            window.click(format!("session-{index}"), cx);
                        } else {
                            window.press(&format!("ctrl-{}", index + 1), cx);
                        }
                        window.render_frame(cx);
                        view.read_with(cx, |v, _| {
                            let session = v.selection.selected_session.as_ref().unwrap();
                            let active = v.session_key_for(session);
                            assert_eq!(active, keys[index]);
                            let store = &v.sidebar_state.workspace_store;
                            let layout = store.compute_sidebar_layout(
                                workspace_id,
                                &v.session_inputs(),
                                false,
                                now_unix_ms(),
                            );
                            assert_eq!(layout.flat_display_order, keys);
                            assert_eq!(layout.ctrl_digit_order, keys);
                            assert_eq!(store.recent_views[0].session_key, active);
                            assert!(!store.session_meta[&active].frequent_visits_ms.is_empty());
                            assert!(!store.is_session_unread(&active, session));
                        });
                    }
                }
            }
        }
    })
    .unwrap();
}

#[gpui_kit::test]
fn done_steps_down_visible_sidebar_without_reordering(cx: &mut TestAppContext) {
    let (handle, view) = open_view(cx);
    cx.update_window(handle, |_, window, cx| {
        let keys = view.update(cx, |v, cx| {
            v.connection.sessions = (0..5)
                .map(|i| SessionSummary {
                    id: format!("done-{i}"),
                    agent: AgentKind::Claude,
                    title: format!("Chat {i}"),
                    updated_at_ms: 10_000 - i,
                    ..Default::default()
                })
                .collect();
            let keys: Vec<_> = v
                .connection
                .sessions
                .iter()
                .map(|s| v.session_key_for(s))
                .collect();
            v.select_session(v.connection.sessions[2].clone(), cx);
            v.focus_sidebar(window, cx);
            keys
        });
        window.render_frame(cx);
        for (removed, selected, remaining) in [
            (2, 3, vec![0, 1, 3, 4]),
            (3, 4, vec![0, 1, 4]),
            (4, 1, vec![0, 1]),
            (1, 0, vec![0]),
        ] {
            window.press("alt-e", cx);
            window.render_frame(cx);
            assert!(
                view.read(cx)
                    .inputs
                    .prompt_input
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window)
            );
            view.read_with(cx, |v, _| {
                assert_eq!(
                    v.session_key_for(v.selection.selected_session.as_ref().unwrap()),
                    keys[selected]
                );
                assert!(
                    v.sidebar_state
                        .workspace_store
                        .waiting_sessions
                        .contains_key(&keys[removed])
                );
                assert_eq!(
                    v.active_ctrl_digit_order(),
                    remaining
                        .iter()
                        .map(|&i| keys[i].clone())
                        .collect::<Vec<_>>()
                );
            });
        }
        window.press("alt-e", cx);
        window.render_frame(cx);
        assert!(view.read(cx).selection.selected_session.is_none());
        assert!(view.read(cx).active_ctrl_digit_order().is_empty());
    })
    .unwrap();
}

#[gpui_kit::test]
fn dissolve_steps_up_visible_sidebar_and_top_falls_down(cx: &mut TestAppContext) {
    let (handle, view) = open_view(cx);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    cx.update_window(handle, |_, window, cx| {
        let keys = view.update(cx, |v, cx| {
            v.connection.daemon_tx = Some(tx);
            // Backing storage differs from the displayed order.
            v.connection.sessions = (0..5)
                .rev()
                .map(|i| SessionSummary {
                    id: format!("dissolve-{i}"),
                    agent: AgentKind::Antigravity,
                    title: format!("Chat {i}"),
                    updated_at_ms: 10_000 - i,
                    ..Default::default()
                })
                .collect();
            v.select_session(v.connection.sessions[1].clone(), cx);
            v.focus_sidebar(window, cx);
            v.active_ctrl_digit_order()
        });
        window.render_frame(cx);
        for (removed, selected, remaining) in [
            (3, 2, vec![0, 1, 2, 4]),
            (2, 1, vec![0, 1, 4]),
            (1, 0, vec![0, 4]),
            (0, 4, vec![4]),
        ] {
            window.press("ctrl-shift-w", cx);
            window.render_frame(cx);
            assert!(
                view.read(cx)
                    .inputs
                    .prompt_input
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window)
            );
            view.read_with(cx, |v, _| {
                assert_eq!(
                    v.session_key_for(v.selection.selected_session.as_ref().unwrap()),
                    keys[selected]
                );
                assert_eq!(
                    v.sidebar_state.workspace_store.session_lifecycle(
                        &keys[removed],
                        &v.find_session_by_key(&keys[removed]).unwrap(),
                        now_unix_ms()
                    ),
                    multibot_core::workspaces::SessionLifecycle::Dissolved
                );
                assert_eq!(
                    v.active_ctrl_digit_order(),
                    remaining
                        .iter()
                        .map(|&i| keys[i].clone())
                        .collect::<Vec<_>>()
                );
            });
        }
        window.press("ctrl-shift-w", cx);
        window.render_frame(cx);
        assert!(view.read(cx).selection.selected_session.is_none());
        let mut archived = Vec::new();
        while let Ok(request) = rx.try_recv() {
            if let ClientRequest::ArchiveSession {
                session_id,
                kill_process,
                ..
            } = request
            {
                assert!(kill_process);
                archived.push(session_id);
            }
        }
        assert_eq!(
            archived,
            [
                "dissolve-3",
                "dissolve-2",
                "dissolve-1",
                "dissolve-0",
                "dissolve-4"
            ]
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn shell_dissolve_requires_one_confirmation_for_exact_session_within_eight_seconds(
    cx: &mut TestAppContext,
) {
    let (handle, view) = open_view(cx);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.connection.daemon_tx = Some(tx);
            v.connection.sessions = (0..2)
                .map(|i| SessionSummary {
                    id: format!("shell-{i}"),
                    agent: AgentKind::Shell,
                    title: format!("Server {i}"),
                    source_path: format!("tmux:shell-{i}"),
                    updated_at_ms: 10_000 - i,
                    ..Default::default()
                })
                .collect();
            v.select_session(v.connection.sessions[1].clone(), cx);
            v.focus_sidebar(window, cx);
        });
        window.render_frame(cx);
        while rx.try_recv().is_ok() {}
        window.press("ctrl-shift-w", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).notice.as_deref(),
            Some("Press Ctrl+Shift+W again to kill live server shell \"Server 1\".")
        );
        assert_eq!(
            view.read(cx)
                .selection
                .selected_session
                .as_ref()
                .unwrap()
                .id,
            "shell-1"
        );
        assert!(window.find("shell-kill-confirmation").visible());
        assert!(window.find("shell-kill").visible());
        assert!(window.find("shell-kill-cancel").visible());
        assert!(rx.try_recv().is_err(), "first press must not archive");
        view.update(cx, |v, _| {
            v.sidebar_state.pending_shell_dissolve.as_mut().unwrap().1 =
                std::time::Instant::now() - std::time::Duration::from_secs(9);
        });
        window.press("ctrl-shift-w", cx);
        window.render_frame(cx);
        assert!(rx.try_recv().is_err(), "expired confirmation must rearm");
        window.press("ctrl-shift-w", cx);
        window.render_frame(cx);
        assert!(matches!(rx.try_recv(), Ok(ClientRequest::ArchiveSession {
            session_id, agent: AgentKind::Shell, source_path, kill_process: true,
        }) if session_id == "shell-1" && source_path == "tmux:shell-1"));
        assert_eq!(
            view.read(cx)
                .selection
                .selected_session
                .as_ref()
                .unwrap()
                .id,
            "shell-0"
        );
        assert!(view.read(cx).notice.is_none());
        while rx.try_recv().is_ok() {}
        window.press("ctrl-shift-w", cx);
        window.render_frame(cx);
        assert!(
            rx.try_recv().is_err(),
            "confirmation must not transfer to next shell"
        );
        assert!(window.find("shell-kill-confirmation").visible());
        window.press("escape", cx);
        window.render_frame(cx);
        assert!(window.try_find("shell-kill-confirmation").is_none());
        assert!(rx.try_recv().is_err());
        window.press("ctrl-shift-w", cx);
        window.render_frame(cx);
        window.click("shell-kill-cancel", cx);
        window.render_frame(cx);
        assert!(window.try_find("shell-kill-confirmation").is_none());
        window.press("ctrl-shift-w", cx);
        window.render_frame(cx);
        window.click("shell-kill", cx);
        assert!(matches!(rx.try_recv(), Ok(ClientRequest::ArchiveSession {
            session_id, agent: AgentKind::Shell, kill_process: true, ..
        }) if session_id == "shell-0"));
        assert!(view.read(cx).selection.selected_session.is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn archive_acknowledgements_do_not_leak_into_live_chats(cx: &mut TestAppContext) {
    let (handle, view) = open_view(cx);
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.connection.sessions = sample_sessions();
            v.select_session(v.connection.sessions[0].clone(), cx);
            v.notice = Some("A session-specific notice".into());
            v.select_session(v.connection.sessions[1].clone(), cx);
            assert!(v.notice.is_none());
            for archived in [true, false] {
                v.handle_bridge_event(
                    crate::state::UiBridgeEvent::Server(
                        multibot_proto::ServerEvent::SessionArchived {
                            session_id: "s1".into(),
                            archived,
                        },
                    ),
                    cx,
                );
                assert!(v.notice.is_none());
                assert_eq!(v.selection.selected_session.as_ref().unwrap().id, "s2");
                assert!(!v.selection.selected_session.as_ref().unwrap().archived);
            }
        });
        window.render_frame(cx);
        assert!(
            !gpui_kit::base::test_support::texts(window)
                .iter()
                .any(|text| text.text().contains("Archived session")
                    || text.text().contains("Restored session"))
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn codex_exit_commands_dissolve_without_sending_a_prompt(cx: &mut TestAppContext) {
    let (handle, view) = open_view(cx);
    cx.update_window(handle, |_, window, cx| {
        for command in ["ctrl-shift-w", "/exit", "/quit"] {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            view.update(cx, |v, cx| {
                v.sidebar_state.workspace_store = WorkspaceStore::new();
                v.connection.daemon_tx = Some(tx);
                v.connection.sessions = vec![SessionSummary { id: "codex-close".into(), agent: AgentKind::Codex,
                    source_path: "/tmp/rollout-codex-close.jsonl".into(), ..Default::default() }];
                v.select_session(v.connection.sessions[0].clone(), cx);
                while rx.try_recv().is_ok() {}
                if command != "ctrl-shift-w" {
                    v.apply_pending_draft(window, cx);
                    v.inputs.prompt_input.update(cx, |input, cx| input.set_value(command, window, cx));
                    v.send_current_prompt(window, cx);
                }
            });
            if command == "ctrl-shift-w" {
                window.render_frame(cx);
                window.press(command, cx);
            }
            assert!(view.read(cx).selection.selected_session.is_none());
            let requests: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
            assert!(requests.iter().any(|request| matches!(request, ClientRequest::ArchiveSession {
                session_id, agent: AgentKind::Codex, kill_process: true, .. } if session_id == "codex-close")));
            assert!(!requests.iter().any(|request| matches!(request, ClientRequest::SendPrompt { .. } | ClientRequest::SpawnSession { .. })));
        }
    }).unwrap();
}

#[gpui_kit::test]
fn frequent_ctrl_tab_keeps_order_and_workspace_scope(cx: &mut TestAppContext) {
    let (handle, view) = open_view(cx);
    cx.update_window(handle, |_, window, cx| {
        for workspace_id in [ALL_WORKSPACES_ID, DEFAULT_WORKSPACE_ID] {
            let keys = view.update(cx, |v, cx| {
                v.sidebar_state.workspace_store = WorkspaceStore::new();
                v.sidebar_state.active_workspace_id = workspace_id.into();
                let other = v
                    .sidebar_state
                    .workspace_store
                    .create_workspace("Other project", None)
                    .unwrap();
                v.connection.sessions = (0..6)
                    .map(|i| SessionSummary {
                        id: format!("frequent-{i}"),
                        agent: AgentKind::Claude,
                        title: format!("Frequent {i}"),
                        updated_at_ms: 10_000 - i,
                        ..Default::default()
                    })
                    .collect();
                let keys: Vec<_> = v
                    .connection
                    .sessions
                    .iter()
                    .map(|s| v.session_key_for(s))
                    .collect();
                let now = now_unix_ms();
                for (i, key) in keys.iter().enumerate() {
                    if i >= 4 {
                        v.sidebar_state
                            .workspace_store
                            .assign_session_workspace(key, &other.id);
                    }
                    for visit in 0..(8 - i) {
                        v.sidebar_state
                            .workspace_store
                            .record_frequent_visit(key, now - 100 - visit as i64);
                    }
                }
                v.select_session(v.connection.sessions[0].clone(), cx);
                v.set_workspace_view(WorkspaceView::Frequent, cx);
                assert_eq!(v.sidebar_state.active_workspace_id, workspace_id);
                keys
            });
            window.render_frame(cx);
            let count = if workspace_id == ALL_WORKSPACES_ID {
                6
            } else {
                4
            };
            assert_eq!(view.read(cx).active_ctrl_digit_order(), keys[..count]);
            for _ in 0..2 {
                for (chord, indices) in [
                    ("ctrl-tab", (1..count).chain([0]).collect::<Vec<_>>()),
                    ("ctrl-shift-tab", (1..count).rev().chain([0]).collect()),
                ] {
                    for index in indices {
                        window.press(chord, cx);
                        window.render_frame(cx);
                        view.read_with(cx, |v, _| {
                            assert_eq!(
                                v.session_key_for(v.selection.selected_session.as_ref().unwrap()),
                                keys[index]
                            );
                            assert_eq!(v.active_ctrl_digit_order(), keys[..count]);
                        });
                    }
                }
            }
        }
    })
    .unwrap();
}

#[test]
fn session_subtitles_use_cli_and_directory_not_message_previews() {
    for (agent, cli) in [
        (AgentKind::Claude, "cld"),
        (AgentKind::Codex, "cdx"),
        (AgentKind::Antigravity, "agy"),
        (AgentKind::Pi, "pi"),
        (AgentKind::Cursor, "cur"),
    ] {
        for (cwd, suffix) in [
            (Some("/home/valentin/projects/multibot/"), " · multibot"),
            (Some("C:\\projects\\postbox-rs\\"), " · postbox-rs"),
            (Some("/"), " · /"),
            (Some("~"), " · ~"),
            (Some(""), ""),
            (None, ""),
        ] {
            assert_eq!(
                session_subtitle(agent, cwd, Some("old reply")),
                format!("{cli}{suffix}")
            );
        }
    }
    assert_eq!(
        session_subtitle(AgentKind::Shell, Some("/projects/"), Some("btop · old")),
        "btop · projects"
    );
    assert_eq!(session_subtitle(AgentKind::Shell, None, None), "shl");
}

#[gpui_kit::test]
fn every_sidebar_agent_paints_cli_and_directory_on_line_two(cx: &mut TestAppContext) {
    let (handle, view) = open_view(cx);
    cx.update_window(handle, |_, window, cx| {
        for pinned in [false, true] {
            for (agent, expected) in [
                (AgentKind::Claude, "cld · repo"),
                (AgentKind::Codex, "cdx · repo"),
                (AgentKind::Antigravity, "agy · repo"),
                (AgentKind::Pi, "pi · repo"),
                (AgentKind::Cursor, "cur · repo"),
                (AgentKind::Shell, "btop · repo"),
            ] {
                let key = view.update(cx, |v, cx| {
                    v.sidebar_state.workspace_store = WorkspaceStore::new();
                    v.connection.sessions = vec![SessionSummary {
                        id: "subtitle".into(),
                        agent,
                        title: "Session title".into(),
                        cwd: Some("/home/valentin/repo/".into()),
                        last_assistant_preview: Some(
                            if agent == AgentKind::Shell {
                                "btop · old"
                            } else {
                                "Last reply must not appear"
                            }
                            .into(),
                        ),
                        ..Default::default()
                    }];
                    let key = v.session_key_for(&v.connection.sessions[0]);
                    if pinned {
                        v.sidebar_state.workspace_store.toggle_pin(&key, false);
                    }
                    cx.notify();
                    key
                });
                window.render_frame(cx);
                let id = gpui_kit::ElementId::from(SharedString::from(format!(
                    "sidebar-subtitle-{key}"
                )));
                assert!(window.find(id.clone()).visible());
                let texts = gpui_kit::base::test_support::texts(window);
                assert!(
                    texts
                        .iter()
                        .any(|text| text.text() == expected && text.path().contains(&id)),
                    "{agent:?}, pinned={pinned}: {texts:?}"
                );
            }
        }
    })
    .unwrap();
}

#[gpui_kit::test]
fn ctrl_tab_cycles_every_visible_session_without_reordering(cx: &mut TestAppContext) {
    let (handle, view) = open_view(cx);
    cx.update_window(handle, |_, window, cx| {
        for count in [4, 12] {
            for workspace_id in [ALL_WORKSPACES_ID, DEFAULT_WORKSPACE_ID] {
                for unread_only in [false, true] {
                    let (keys, before) = view.update(cx, |v, cx| {
                        v.sidebar_state.workspace_store = WorkspaceStore::new();
                        v.sidebar_state.active_workspace_id = workspace_id.to_string();
                        v.sidebar_state.unread_only = unread_only;
                        v.connection.sessions = (0..count)
                            .map(|i| SessionSummary {
                                id: format!("cycle-{i}"),
                                title: format!("Session {i}"),
                                agent: AgentKind::Claude,
                                updated_at_ms: 10_000 - i as i64,
                                needs_attention: true,
                                ..Default::default()
                            })
                            .collect();
                        let keys: Vec<_> = v
                            .connection
                            .sessions
                            .iter()
                            .map(|s| v.session_key_for(s))
                            .collect();
                        for (i, key) in keys.iter().enumerate() {
                            v.sidebar_state
                                .workspace_store
                                .record_session_view(key, 20_000 - i as i64);
                        }
                        v.select_session(v.connection.sessions[0].clone(), cx);
                        (keys, v.sidebar_state.workspace_store.clone())
                    });
                    window.render_frame(cx);

                    for (chord, indices) in [
                        ("ctrl-tab", (1..count).chain([0]).collect::<Vec<_>>()),
                        ("ctrl-shift-tab", (1..count).rev().chain([0]).collect()),
                    ] {
                        for index in indices {
                            window.press(chord, cx);
                            window.render_frame(cx);
                            view.read_with(cx, |v, _| {
                                let active = v.session_key_for(
                                    v.selection.selected_session.as_ref().unwrap(),
                                );
                                assert_eq!(active, keys[index], "{chord}, {workspace_id}");
                                let layout = v
                                    .sidebar_state
                                    .workspace_store
                                    .compute_sidebar_layout_with_options(
                                        workspace_id,
                                        v.sidebar_state.active_workspace_view,
                                        &v.session_inputs(),
                                        unread_only,
                                        Some(&active),
                                        now_unix_ms(),
                                    );
                                assert_eq!(layout.ctrl_digit_order, keys);
                                let mut without_visits = v.sidebar_state.workspace_store.clone();
                                assert!(
                                    !without_visits.session_meta[&active]
                                        .frequent_visits_ms
                                        .is_empty()
                                );
                                for (key, meta) in &mut without_visits.session_meta {
                                    let old = &before.session_meta[key];
                                    meta.frequent_visits_ms = old.frequent_visits_ms.clone();
                                    meta.last_used_ms = old.last_used_ms;
                                }
                                assert_eq!(without_visits, before);
                            });
                        }
                    }
                }
            }
        }
    })
    .unwrap();
}

#[gpui_kit::test]
fn ctrl_tab_does_not_open_sessions_in_collapsed_sections(cx: &mut TestAppContext) {
    let (handle, view) = open_view(cx);
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.connection.sessions = sample_sessions();
            let group = v.sidebar_state.workspace_store.create_group(
                DEFAULT_WORKSPACE_ID,
                "Hidden with Ctrl+G",
                None,
            );
            for session in &v.connection.sessions {
                let key = v.session_key_for(session);
                v.sidebar_state.workspace_store.toggle_group_for_session(
                    &key,
                    DEFAULT_WORKSPACE_ID,
                    Some(&group.id),
                    "",
                );
            }
            v.sidebar_state
                .workspace_store
                .toggle_group_collapsed(&group.id);
            v.select_session(v.connection.sessions[0].clone(), cx);
        });
        window.render_frame(cx);
        for chord in ["ctrl-tab", "ctrl-shift-tab"] {
            window.press(chord, cx);
            window.render_frame(cx);
            view.read_with(cx, |v, _| {
                assert_eq!(v.selection.selected_session.as_ref().unwrap().id, "s1");
                assert!(v.sidebar_state.workspace_store.recent_views.is_empty());
            });
        }
    })
    .unwrap();
}

#[test]
fn inline_rename_select_all_replacement_and_deletion() {
    for key_char in [Some("日"), None] {
        let mut r = InlineRenameState::new("k".into(), "s1".into(), "a".repeat(63));
        assert!(r.all_selected);
        assert_eq!(
            r.handle_key("shift", None, Modifiers::default()),
            RenameKeyOutcome::Ignored
        );
        r.handle_key("x", key_char, Modifiers::default());
        assert_eq!(r.draft, key_char.unwrap_or("x"));
        assert!(!r.all_selected);
        r.handle_key("y", Some("y"), Modifiers::default());
        assert_eq!(r.draft, format!("{}y", key_char.unwrap_or("x")));
    }
    for key in ["backspace", "delete"] {
        let mut r = InlineRenameState::new("k".into(), "s1".into(), "Old 日".into());
        assert_eq!(
            r.handle_key(key, None, Modifiers::default()),
            RenameKeyOutcome::Consumed
        );
        assert!(r.draft.is_empty());
        assert!(!r.all_selected);
    }
}

#[test]
fn inline_rename_consumes_printable_keys_at_63_char_cap() {
    let mut r = InlineRenameState::new("k".into(), "s1".into(), "Old".into());
    r.handle_key("a", Some(&"a".repeat(63)), Modifiers::default());
    assert_eq!(
        r.handle_key("q", Some("q"), Modifiers::default()),
        RenameKeyOutcome::Consumed
    );
    assert_eq!(r.draft.chars().count(), 63);
    assert_eq!(
        r.handle_key("j", None, Modifiers::default()),
        RenameKeyOutcome::Consumed
    );
    assert_eq!(
        r.handle_key("e", Some("日"), Modifiers::default()),
        RenameKeyOutcome::Consumed
    );
    // Ctrl chord is still Ignored so system shortcuts can work
    assert_eq!(
        r.handle_key(
            "k",
            None,
            Modifiers {
                control: true,
                ..Default::default()
            }
        ),
        RenameKeyOutcome::Ignored
    );
}

#[gpui_kit::test]
fn ctrl_g_collapses_group_without_ungrouping_and_collapsed_rail_matches_ctrl_digit_order(
    cx: &mut TestAppContext,
) {
    let (handle, view) = open_view(cx);
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.connection.sessions = sample_sessions();
            let k1 = v.session_key_for(&v.connection.sessions[0]);
            let k2 = v.session_key_for(&v.connection.sessions[1]);
            let k3 = v.session_key_for(&v.connection.sessions[2]);
            v.selection.selected_session = Some(v.connection.sessions[2].clone());

            // Ungrouped session: toggle_active_group is a no-op
            v.toggle_active_group(cx);
            assert!(v.sidebar_state.workspace_store.groups.is_empty());

            // Group s1 & s2; select s1; Ctrl+G collapses group without removing membership
            let gid = v
                .sidebar_state
                .workspace_store
                .toggle_group_for_session(&k1, DEFAULT_WORKSPACE_ID, None, "Team")
                .unwrap();
            v.sidebar_state.workspace_store.toggle_group_for_session(
                &k2,
                DEFAULT_WORKSPACE_ID,
                Some(&gid),
                "Team",
            );
            v.selection.selected_session = Some(v.connection.sessions[0].clone());
            v.toggle_active_group(cx);
            assert!(
                v.sidebar_state
                    .workspace_store
                    .groups
                    .iter()
                    .find(|g| g.id == gid)
                    .unwrap()
                    .collapsed
            );
            assert_eq!(
                v.sidebar_state.workspace_store.session_meta[&k1]
                    .group_id
                    .as_deref(),
                Some(gid.as_str())
            );

            // In Collapsed rail with jump badges ON, only s3 is visible with badge "1"
            v.sidebar_state.set_mode(SidebarMode::Collapsed);
            v.sidebar_state.show_jump_badges = true;
            let _ = k3;
            cx.notify();
        });
        window.render_frame(cx);

        assert!(window.try_find("rail-session-0").is_some());
        assert!(window.try_find("rail-session-1").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn sidebar_rename_focus_and_close_overlay_cancels_rename_without_closing_inbox(
    cx: &mut TestAppContext,
) {
    let (handle, view) = open_view(cx);
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.connection.sessions = sample_sessions();
            v.main_surface = MainSurface::Inbox;
            let k1 = v.session_key_for(&v.connection.sessions[0]);
            v.start_sidebar_rename_with_window(k1, window, cx);
            v.focus_inbox(window, cx);
        });
        window.render_frame(cx);
        assert!(
            view.read_with(cx, |v, _| v.sidebar_state.rename.is_none()),
            "The minimal Inbox hides the sidebar, so no Open-sidebar rename may mount there"
        );

        // Dispatch CloseOverlay (Escape) while the Inbox owns focus
        let fh = view.read_with(cx, |v, _| v.inbox_state.focus_handle.clone().unwrap());
        fh.dispatch_action(&CloseOverlay, window, cx);
        window.render_frame(cx);
        assert_eq!(view.read_with(cx, |v, _| v.main_surface), MainSurface::Chat);
    })
    .unwrap();
}

#[gpui_kit::test]
fn sidebar_recents_sync_pinned_collapse_and_modal_precedence(cx: &mut TestAppContext) {
    let (handle, view) = open_view(cx);
    cx.update_window(handle, |_, window, cx| {
        // Old fixed-section collapse flags must not strand sessions after header toggles disappear.
        view.update(cx, |v, cx| {
            v.connection.sessions = sample_sessions();
            let k1 = v.session_key_for(&v.connection.sessions[0]);
            v.sidebar_state.workspace_store.toggle_pin(&k1, false);
            v.sidebar_state.workspace_store.recents_section_collapsed = true;
            v.sidebar_state.recents_collapsed = false;
            v.sidebar_state.workspace_store.pinned_section_collapsed = true;
            cx.notify();
        });
        window.render_frame(cx);

        assert!(window.find("sidebar-pinned-grid").visible());
        assert!(window.find("session-0").visible());

        // Clicking Recents is inert even with older collapse flags.
        window.click("sidebar-recents-header", cx);
        window.render_frame(cx);
        view.read_with(cx, |v, _| {
            assert!(!v.sidebar_state.recents_collapsed);
            assert!(v.sidebar_state.workspace_store.recents_section_collapsed);
        });
        assert!(window.try_find("session-0").is_some());

        // Uncollapsing pinned_section_collapsed restores sidebar-pinned-grid
        view.update(cx, |v, cx| {
            v.sidebar_state.workspace_store.pinned_section_collapsed = false;
            cx.notify();
        });
        window.render_frame(cx);
        assert!(window.try_find("sidebar-pinned-grid").is_some());

        // (c) Focused sidebar with active rename + ActiveModal::Shortcuts on top:
        // raw keydown must not mutate rename, and 1st CloseOverlay closes Shortcuts FIRST.
        view.update(cx, |v, cx| {
            let k2 = v.session_key_for(&v.connection.sessions[1]);
            v.start_sidebar_rename_with_window(k2, window, cx);
            v.active_modal = ActiveModal::Shortcuts;
            cx.notify();
        });
        window.render_frame(cx);

        let orig_draft = view.read_with(cx, |v, _| {
            v.sidebar_state.rename.as_ref().unwrap().draft.clone()
        });

        // Raw keydown while Shortcuts is open must not touch rename draft
        view.update(cx, |v, cx| {
            let mut x_ks = gpui_kit::Keystroke::parse("x").unwrap();
            x_ks.key_char = Some("x".into());
            let ev_x = KeyDownEvent {
                keystroke: x_ks,
                is_held: false,
                prefer_character_input: false,
            };
            v.handle_sidebar_keydown(&ev_x, cx);
            assert_eq!(v.sidebar_state.rename.as_ref().unwrap().draft, orig_draft);
        });

        let fh = view.read_with(cx, |v, _| v.sidebar_state.focus_handle.clone().unwrap());

        // 1st CloseOverlay closes Shortcuts modal while preserving sidebar rename
        fh.dispatch_action(&CloseOverlay, window, cx);
        window.render_frame(cx);
        view.read_with(cx, |v, _| {
            assert_eq!(v.active_modal, ActiveModal::None);
            assert!(v.sidebar_state.rename.is_some());
        });

        // 2nd CloseOverlay cancels sidebar rename
        fh.dispatch_action(&CloseOverlay, window, cx);
        window.render_frame(cx);
        view.read_with(cx, |v, _| {
            assert!(v.sidebar_state.rename.is_none());
        });
    })
    .unwrap();
}
