use super::*;
use gpui_kit::{AppContext, Entity, Focusable, TestAppContext, WindowOptions, test::TestWindowExt};
use multibot_proto::{AgentKind, SessionSummary};
use std::sync::Arc;

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
            cx.new(|cx| MultibotView::new(rt, window, cx))
        })
        .unwrap()
    })
}

#[gpui_kit::test]
fn dissolve_workspace_confirms_stops_agents_keeps_split_and_restores_on_reactivate(
    cx: &mut TestAppContext,
) {
    use multibot_core::workspaces::{InboxSplit, SessionLifecycle};
    let (handle, view) = open_view(cx);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    cx.update_window(handle, |_, window, cx| {
        let workspace_id = view.update(cx, |v, _| {
            v.daemon_tx = Some(tx);
            let workspace = v.sidebar_state.workspace_store.create_workspace("Project", None).unwrap();
            v.sidebar_state.active_workspace_id = workspace.id.clone();
            v.sessions = (0..5).map(|i| SessionSummary {
                id: format!("workspace-{i}"), title: format!("Chat {i}"),
                agent: AgentKind::Claude,
                archived: i == 3, archive_kill_process: i == 3,
                ..Default::default()
            }).collect();
            for session in &v.sessions[..4] {
                let key = v.session_key_for(session);
                v.sidebar_state.workspace_store.assign_session_workspace(&key, &workspace.id);
            }
            let done = v.sessions[1].clone();
            let hibernated = v.sessions[2].clone();
            let key = v.session_key_for(&done);
            v.sidebar_state.workspace_store.archive_to_waiting_session(&key, now_unix_ms(), &done);
            let key = v.session_key_for(&hibernated);
            v.sidebar_state.workspace_store.hibernate_session(&key, &hibernated, Some(now_unix_ms() + 100000), None, true);
            workspace.id
        });
        window.render_frame(cx);
        window.press("ctrl-k", cx);
        window.input("Dissolve workspace", cx);
        window.press("enter", cx);
        window.render_frame(cx);
        assert!(window.find("workspace.dissolve.confirm").visible());
        assert!(rx.try_recv().is_err(), "confirmation must not stop agents");
        assert!(!view.read(cx).sidebar_state.workspace_store.workspace_by_id(&workspace_id).unwrap().archived);
        window.press("escape", cx);
        window.render_frame(cx);
        assert!(rx.try_recv().is_err());
        window.press("ctrl-k", cx);
        window.input("Dissolve workspace", cx);
        window.press("enter", cx);
        window.render_frame(cx);
        window.press("enter", cx);
        window.render_frame(cx);
        let mut stopped = Vec::new();
        while let Ok(request) = rx.try_recv() {
            match request {
                ClientRequest::ArchiveSession { session_id, kill_process: true, .. } => stopped.push(session_id),
                other => panic!("unexpected request: {other:?}"),
            }
        }
        stopped.sort();
        assert_eq!(stopped, ["workspace-0", "workspace-1", "workspace-2"]);
        view.read_with(cx, |v, _| {
            assert!(!v.sidebar_state.workspace_store.visible_workspaces(now_unix_ms()).iter().any(|w| w.id == workspace_id));
            assert!(v.sessions[..4].iter().all(|s| s.archived && s.archive_kill_process));
            assert!(!v.sessions[4].archived);
        });
        window.press("alt-i", cx);
        window.render_frame(cx);
        assert!(!view.read(cx).inbox_visible_splits(now_unix_ms()).contains(&InboxSplit::Workspace(workspace_id.clone())));
        window.press("alt-shift-d", cx);
        window.render_frame(cx);
        assert!(view.read(cx).inbox_visible_splits(now_unix_ms()).contains(&InboxSplit::Workspace(workspace_id.clone())));
        assert!(window.find("inbox-split-ws-0").visible());
        window.click("inbox-split-ws-0", cx);
        window.render_frame(cx);
        view.read_with(cx, |v, _| {
            assert_eq!(v.inbox_state.active_split, InboxSplit::Workspace(workspace_id.clone()));
            assert_eq!(v.compute_inbox_rows(now_unix_ms()).len(), 4);
        });
        view.update(cx, |v, cx| {
            let key = v.session_key_for(&v.sessions[0]);
            assert!(v.reactivate_session_key(&key, cx));
            assert!(v.sidebar_state.workspace_store.visible_workspaces(now_unix_ms()).iter().any(|w| w.id == workspace_id));
            assert_eq!(v.sidebar_state.workspace_store.effective_workspace_id(&key), workspace_id);
            assert_eq!(v.sidebar_state.workspace_store.session_lifecycle(&key, &v.sessions[0], now_unix_ms()), SessionLifecycle::Active);
        });
        assert!(matches!(rx.try_recv(), Ok(ClientRequest::UnarchiveSession { session_id }) if session_id == "workspace-0"));
    }).unwrap();
}

#[gpui_kit::test]
fn workspace_dissolve_requires_all_hosts_and_routes_stops_to_each_host(cx: &mut TestAppContext) {
    let (handle, view) = open_view(cx);
    let (tx_a, mut rx_a) = tokio::sync::mpsc::unbounded_channel();
    let (tx_b, mut rx_b) = tokio::sync::mpsc::unbounded_channel();
    cx.update_window(handle, |_, window, cx| {
        let workspace_id = view.update(cx, |v, _| {
            let workspace = v.sidebar_state.workspace_store.create_workspace("Two servers", None).unwrap();
            v.host_sessions = ["a", "b"].into_iter().map(|host| (host.into(), SessionSummary {
                id: "same-id".into(), agent: AgentKind::Claude, ..Default::default()
            })).collect();
            v.sessions = v.host_sessions.iter().map(|(_, s)| s.clone()).collect();
            let keys: Vec<_> = v.session_inputs().into_iter().map(|input| input.key).collect();
            for key in keys {
                v.sidebar_state.workspace_store.assign_session_workspace(&key, &workspace.id);
            }
            v.host_txs.insert("a".into(), tx_a);
            workspace.id
        });
        view.update(cx, |v, cx| v.dissolve_workspace(&workspace_id, cx));
        assert!(rx_a.try_recv().is_err(), "no partial stop when a server is disconnected");
        assert!(!view.read(cx).sidebar_state.workspace_store.workspace_by_id(&workspace_id).unwrap().archived);
        view.update(cx, |v, cx| {
            v.host_txs.insert("b".into(), tx_b);
            v.dissolve_workspace(&workspace_id, cx);
        });
        for rx in [&mut rx_a, &mut rx_b] {
            assert!(matches!(rx.try_recv(), Ok(ClientRequest::ArchiveSession { session_id, kill_process: true, .. }) if session_id == "same-id"));
            assert!(rx.try_recv().is_err());
        }
        assert!(view.read(cx).sidebar_state.workspace_store.workspace_by_id(&workspace_id).unwrap().archived);
        window.render_frame(cx);
    }).unwrap();
}

#[gpui_kit::test]
fn workspace_delete_is_safe_and_empty_dissolve_needs_no_confirmation(cx: &mut TestAppContext) {
    let (handle, view) = open_view(cx);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, _| {
            v.daemon_tx = Some(tx);
            let other = v
                .sidebar_state
                .workspace_store
                .create_workspace("Other project", None)
                .unwrap();
            v.sidebar_state.workspace_store.workspaces[0].position = 10;
            assert_eq!(
                v.sidebar_state.workspace_store.default_workspace_id(),
                other.id
            );
            let workspace = v
                .sidebar_state
                .workspace_store
                .create_workspace("Delete me", None)
                .unwrap();
            v.sidebar_state.active_workspace_id = workspace.id.clone();
            let session = SessionSummary {
                id: "live".into(),
                agent: AgentKind::Claude,
                ..Default::default()
            };
            let key = v.session_key_for(&session);
            v.sidebar_state
                .workspace_store
                .assign_session_workspace(&key, &workspace.id);
            v.sessions = vec![session];
        });
        window.render_frame(cx);
        window.press("ctrl-k", cx);
        window.input("Delete workspace", cx);
        window.press("enter", cx);
        window.render_frame(cx);
        assert!(rx.try_recv().is_err(), "delete must not stop any agent");
        view.read_with(cx, |v, _| {
            assert!(!v.sessions[0].archived);
            let key = v.session_key_for(&v.sessions[0]);
            let store = &v.sidebar_state.workspace_store;
            assert_eq!(
                store
                    .workspace_by_id(&store.effective_workspace_id(&key))
                    .unwrap()
                    .name,
                "General"
            );
            assert!(!store.workspaces.iter().any(|w| w.name == "Delete me"));
        });
        let empty_id = view.update(cx, |v, _| {
            let workspace = v
                .sidebar_state
                .workspace_store
                .create_workspace("Empty", None)
                .unwrap();
            v.sidebar_state.active_workspace_id = workspace.id.clone();
            workspace.id
        });
        window.press("ctrl-k", cx);
        window.input("Dissolve workspace", cx);
        window.press("enter", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).active_modal, ActiveModal::None);
        assert!(
            view.read(cx)
                .sidebar_state
                .workspace_store
                .workspace_by_id(&empty_id)
                .unwrap()
                .archived
        );
        assert!(rx.try_recv().is_err());
    })
    .unwrap();
}

#[gpui_kit::test]
fn hibernate_displays_alt_h_and_accepts_ctrl_h_fallback(cx: &mut TestAppContext) {
    let (handle, view) = open_view(cx);
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, _| {
            let session = SessionSummary {
                id: "hibernate".into(),
                agent: AgentKind::Claude,
                ..Default::default()
            };
            v.sessions = vec![session.clone()];
            v.selected_session = Some(session);
            assert_eq!(
                v.build_palette_command_catalog(now_unix_ms())
                    .iter()
                    .find(|c| c.label == "Hibernate")
                    .unwrap()
                    .chord,
                "Alt+H"
            );
        });
        window.render_frame(cx);
        for chord in ["alt-h", "ctrl-h"] {
            window.press(chord, cx);
            window.render_frame(cx);
            assert!(window.find("pal-reminder-0").visible());
            window.press("escape", cx);
            window.render_frame(cx);
        }
        window.press("f1", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).active_modal, ActiveModal::Shortcuts);
        assert!(window.find("shortcuts-modal").visible());
        let spec = multibot_keys::by_id("session.remind_me").unwrap();
        assert_eq!(spec.display, "Alt+H");
        assert!(
            !multibot_keys::SHORTCUTS
                .iter()
                .any(|s| s.gpui_key == "alt-g")
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn new_shell_session_from_palette_spawns_and_opens_live_shell(cx: &mut TestAppContext) {
    let (handle, view) = open_view(cx);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, _| {
            v.daemon_tx = Some(tx);
            let existing = SessionSummary {
                id: "existing-chat".into(),
                agent: AgentKind::Claude,
                cwd: Some("/repo".into()),
                ..Default::default()
            };
            v.sessions = vec![existing.clone()];
            v.selected_session = Some(existing);
        });
        for query in ["new shell", "spawn shell", "tmux", "terminal"] {
            window.render_frame(cx);
            window.press("ctrl-k", cx);
            view.update(cx, |v, cx| {
                v.palette_input
                    .update(cx, |input, cx| input.set_value(query, window, cx));
                assert!(
                    v.build_palette_rows(query, now_unix_ms())
                        .iter()
                        .any(|row| {
                            matches!(row, PaletteRow::Command { id, label, .. }
                        if id == "pal-cmd-spawn-shell" && label == "Session: New Shell session")
                        })
                );
            });
            window.render_frame(cx);
            assert!(window.find("pal-cmd-spawn-shell").visible());
            if query != "terminal" {
                window.press("escape", cx);
            }
        }
        window.click("pal-cmd-spawn-shell", cx);
        window.render_frame(cx);
        assert!(matches!(rx.try_recv(), Ok(ClientRequest::SpawnSession {
            agent: AgentKind::Shell, cwd, title: None, initial_prompt: None, options: None,
        }) if cwd == "/repo"));
        assert!(rx.try_recv().is_err());
        assert_eq!(view.read(cx).active_modal, ActiveModal::None);
        view.update(cx, |v, cx| {
            let shell = SessionSummary {
                id: "term-new-shell".into(),
                title: "Shell".into(),
                agent: AgentKind::Shell,
                cwd: Some("/repo".into()),
                ..Default::default()
            };
            v.handle_daemon_server_event(
                multibot_proto::ServerEvent::SessionSpawned {
                    session: shell.clone(),
                },
                cx,
            );
            assert_eq!(v.selected_session.as_ref().unwrap().id, shell.id);
            assert_eq!(v.selected_session.as_ref().unwrap().agent, AgentKind::Shell);
        });
        window.render_frame(cx);
        assert!(window.find("server-shell-panel").visible());
    })
    .unwrap();
}

#[gpui_kit::test]
fn new_agent_sessions_pick_directory_before_spawning(cx: &mut TestAppContext) {
    let (handle, view) = open_view(cx);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, _| {
            v.daemon_tx = Some(tx);
            v.sessions = ["/repo/a", "/repo/b", "/repo/a"]
                .into_iter()
                .enumerate()
                .map(|(i, cwd)| SessionSummary {
                    id: format!("s{i}"),
                    cwd: Some(cwd.into()),
                    ..Default::default()
                })
                .collect();
            v.selected_session = Some(v.sessions[0].clone());
        });
        for (slug, agent) in [
            ("claude", AgentKind::Claude),
            ("antigravity", AgentKind::Antigravity),
            ("pi", AgentKind::Pi),
            ("codex", AgentKind::Codex),
            ("shell", AgentKind::Shell),
        ] {
            window.render_frame(cx);
            window.press("ctrl-k", cx);
            view.update(cx, |v, cx| {
                v.palette_input.update(cx, |input, cx| {
                    input.set_value(format!("new {slug}"), window, cx)
                });
            });
            window.render_frame(cx);
            window.click(format!("pal-cmd-spawn-{slug}"), cx);
            window.render_frame(cx);
            assert!(
                rx.try_recv().is_err(),
                "{slug} spawned before choosing a directory"
            );
            assert!(window.find("palette-context-banner").visible());
            assert!(window.find("pal-spawn-directory-0").visible());
            assert!(window.find("pal-spawn-directory-1").visible());
            assert!(window.try_find("pal-spawn-directory-2").is_none());
            view.update(cx, |v, cx| {
                v.palette_input
                    .update(cx, |input, cx| input.set_value("/repo/b", window, cx));
            });
            window.render_frame(cx);
            window.press("enter", cx);
            window.render_frame(cx);
            assert!(
                matches!(rx.try_recv(), Ok(ClientRequest::SpawnSession { agent: actual, cwd, .. })
                if actual == agent && cwd == "/repo/b")
            );
            assert!(rx.try_recv().is_err());
            assert_eq!(view.read_with(cx, |v, _| v.active_modal), ActiveModal::None);
        }
    })
    .unwrap();
}

#[gpui_kit::test]
fn new_agent_sessions_launch_directly_with_zero_or_one_directory(cx: &mut TestAppContext) {
    let (handle, view) = open_view(cx);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, _| v.daemon_tx = Some(tx));
        for directories in [vec![], vec!["/repo/only"], vec!["/repo/only", "/repo/only"]] {
            view.update(cx, |v, _| {
                v.selected_session = None;
                v.sessions = directories.iter().enumerate().map(|(i, cwd)| SessionSummary {
                    id: format!("s{i}"),
                    cwd: Some((*cwd).into()),
                    ..Default::default()
                }).collect();
            });
            for (slug, agent) in [
                ("claude", AgentKind::Claude),
                ("antigravity", AgentKind::Antigravity),
                ("pi", AgentKind::Pi),
                ("codex", AgentKind::Codex),
                ("shell", AgentKind::Shell),
            ] {
                window.render_frame(cx);
                window.press("ctrl-k", cx);
                view.update(cx, |v, cx| {
                    v.palette_input.update(cx, |input, cx| input.set_value(format!("new {slug}"), window, cx));
                });
                window.render_frame(cx);
                window.click(format!("pal-cmd-spawn-{slug}"), cx);
                window.render_frame(cx);
                let expected_cwd = directories.first().copied().unwrap_or(".");
                assert!(matches!(rx.try_recv(), Ok(ClientRequest::SpawnSession { agent: actual, cwd, .. })
                    if actual == agent && cwd == expected_cwd));
                assert_eq!(view.read_with(cx, |v, _| v.active_modal), ActiveModal::None);
                assert!(rx.try_recv().is_err());
            }
        }
    }).unwrap();
}

#[gpui_kit::test]
fn directory_picker_cancels_without_spawning_and_ctrl_t_stays_direct(cx: &mut TestAppContext) {
    let (handle, view) = open_view(cx);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, _| {
            v.daemon_tx = Some(tx);
            v.sessions = ["/repo/a", "/repo/b"].into_iter().enumerate().map(|(i, cwd)| SessionSummary {
                id: format!("s{i}"),
                cwd: Some(cwd.into()),
                ..Default::default()
            }).collect();
            v.selected_session = Some(v.sessions[0].clone());
        });
        window.render_frame(cx);
        window.press("ctrl-k", cx);
        view.update(cx, |v, cx| {
            v.palette_input.update(cx, |input, cx| input.set_value("new codex", window, cx));
        });
        window.render_frame(cx);
        window.press("enter", cx);
        window.render_frame(cx);
        assert!(window.find("pal-spawn-directory-0").visible());
        window.press("escape", cx);
        window.render_frame(cx);
        assert!(rx.try_recv().is_err());
        window.press("ctrl-t", cx);
        window.render_frame(cx);
        assert!(matches!(rx.try_recv(), Ok(ClientRequest::SpawnSession { agent: AgentKind::Claude, cwd, .. }) if cwd == "/repo/a"));
        window.press("ctrl-k", cx);
        window.render_frame(cx);
        assert!(window.try_find("pal-spawn-directory-0").is_none());
    }).unwrap();
}

#[gpui_kit::test]
fn changelog_opens_offline_from_palette_scrolls_and_closes(cx: &mut TestAppContext) {
    let (handle, view) = open_view(cx);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).update_ui.state,
            multibot_updater::UpdateState::Idle
        );
        assert!(view.read(cx).update_ui.last_release.is_none());
        window.press("ctrl-k", cx);
        window.input("Changelog", cx);
        window.press("enter", cx);
        window.render_frame(cx);

        assert_eq!(view.read(cx).active_modal, ActiveModal::Changelog);
        assert!(window.find("changelog-modal").visible());
        assert!(window.find(("changelog-release", 0usize)).visible());
        let last = multibot_core::changelog::releases().len() - 1;
        assert!(
            window
                .try_find(("changelog-release", last))
                .is_none_or(|section| !section.visible())
        );
        window.scroll(
            "changelog-scroll",
            gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.0), px(-10000.0))),
            cx,
        );
        window.render_frame(cx);
        assert!(window.find(("changelog-release", last)).visible());

        window.press("escape", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).active_modal, ActiveModal::None);
        assert!(window.try_find("changelog-modal").is_none());
        window.press("ctrl-k", cx);
        window.input("Changelog", cx);
        window.press("enter", cx);
        window.render_frame(cx);
        assert!(window.find("changelog-modal").visible());
        window.click("close-changelog", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).active_modal, ActiveModal::None);
        window.press("ctrl-k", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).active_modal, ActiveModal::Palette);
    })
    .unwrap();
}

#[gpui_kit::test]
fn workspace_shortcut_opens_only_workspaces_and_switches_with_enter(cx: &mut TestAppContext) {
    let (handle, view) = open_view(cx);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.press("alt-shift-w", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).active_modal, ActiveModal::Palette);
        let rows = view.read(cx).build_palette_rows("", now_unix_ms());
        let names: Vec<_> = rows
            .iter()
            .map(|row| match row {
                PaletteRow::Workspace { name, .. } => name.as_str(),
                other => panic!("workspace switcher contains {other:?}"),
            })
            .collect();
        assert_eq!(&names[..3], &["All", "Frequent", "Favorites"]);
        window.press("down", cx);
        window.press("enter", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).active_modal, ActiveModal::None);
        assert_eq!(
            view.read(cx).sidebar_state.active_workspace_view,
            multibot_core::workspaces::WorkspaceView::Frequent
        );
        assert!(
            !view
                .read(cx)
                .palette_input
                .focus_handle(cx)
                .is_focused(window)
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn workspace_main_palette_entry_and_search_prioritize_same_named_session(cx: &mut TestAppContext) {
    let (handle, view) = open_view(cx);
    cx.update_window(handle, |_, window, cx| {
        let workspace_id = view.update(cx, |v, cx| {
            v.sessions = vec![SessionSummary {
                id: "same-name".into(), title: "Shared name".into(),
                agent: AgentKind::Claude, ..Default::default()
            }];
            let ws = v.sidebar_state.workspace_store.create_workspace("Shared name", None).unwrap();
            cx.notify();
            ws.id
        });
        window.render_frame(cx);
        window.press("ctrl-k", cx);
        window.render_frame(cx);
        assert!(window.try_find("pal-cmd-workspaces").is_some());
        window.input("Shared name", cx);
        window.render_frame(cx);
        let rows = view.read(cx).build_palette_rows("Shared name", now_unix_ms());
        let session = rows.iter().position(|r| matches!(r, PaletteRow::Session { .. })).unwrap();
        let workspace = rows.iter().position(|r| matches!(r, PaletteRow::Workspace { workspace_id: id, .. } if id == &workspace_id)).unwrap();
        assert!(session < workspace);
        window.press("ctrl-k", cx);
        window.input("Workspaces", cx);
        window.press("enter", cx);
        window.render_frame(cx);
        assert!(view.read(cx).build_palette_rows("", now_unix_ms()).iter()
            .all(|r| matches!(r, PaletteRow::Workspace { .. })));
        assert_eq!(view.read(cx).palette_input.read(cx).value().as_ref(), "");
        window.input("Shared name", cx);
        window.render_frame(cx);
        window.click("pal-ws-0", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).sidebar_state.active_workspace_id, workspace_id);
        assert_eq!(view.read(cx).active_modal, ActiveModal::None);
        window.press("alt-shift-w", cx);
        window.render_frame(cx);
        window.press("backspace", cx);
        window.render_frame(cx);
        assert!(window.try_find("pal-cmd-workspaces").is_some());
        window.press("escape", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).active_modal, ActiveModal::None);
    }).unwrap();
}

#[gpui_kit::test]
fn palette_empty_query_keeps_pal_cmd_inbox_visible_and_supports_tab_context(
    cx: &mut TestAppContext,
) {
    let (handle, view) = open_view(cx);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.press("ctrl-k", cx);
        window.render_frame(cx);

        // Backward-compat contract (`app.rs:639`): pal-cmd-inbox MUST be visible in 520px viewport
        assert!(window.find("pal-cmd-inbox").visible());

        // Seed sessions and verify recent-session hop-back + tab-context banner
        view.update(cx, |v, cx| {
            v.sessions = vec![
                SessionSummary {
                    id: "s1".into(),
                    agent: AgentKind::Claude,
                    title: "Current Session".into(),
                    source_path: "/tmp/s1.jsonl".into(),
                    updated_at_ms: 5_000,
                    ..Default::default()
                },
                SessionSummary {
                    id: "s2".into(),
                    agent: AgentKind::Codex,
                    title: "Previous Session".into(),
                    source_path: "/tmp/s2.jsonl".into(),
                    updated_at_ms: 4_000,
                    ..Default::default()
                },
            ];
            let k1 = v.session_key_for(&v.sessions[0]);
            let k2 = v.session_key_for(&v.sessions[1]);
            v.selected_session = Some(v.sessions[0].clone());
            v.sidebar_state.previous_session_key = Some(k2.clone());
            v.sidebar_state
                .workspace_store
                .record_session_view(&k2, 4_000);
            v.sidebar_state
                .workspace_store
                .record_session_view(&k1, 5_000);

            let rows = v.build_palette_rows("", 6_000);
            // First selectable row after "Recent sessions" header must be Previous Session
            assert_eq!(rows[0], PaletteRow::Header("Recent sessions".into()));
            match &rows[1] {
                PaletteRow::Session { session_key, .. } => assert_eq!(session_key, &k2),
                other => panic!("expected previous session first, got {other:?}"),
            }

            // Open tab-context mode for s2
            v.open_palette_tab_context(k2, window, cx);
        });
        window.render_frame(cx);
        assert!(window.try_find("palette-context-banner").is_some());

        // Click favorite action in tab-context mode
        window.click("pal-cmd-favorite", cx);
        window.render_frame(cx);

        let (modal, is_fav) = view.read_with(cx, |v, _| {
            let k2 = v.session_key_for(&v.sessions[1]);
            (
                v.active_modal,
                v.sidebar_state.workspace_store.is_favorite(&k2),
            )
        });
        assert_eq!(modal, ActiveModal::None);
        assert!(is_fav);
    })
    .unwrap();
}

#[gpui_kit::test]
fn palette_typed_query_ranks_delete_session_and_records_frecency(cx: &mut TestAppContext) {
    let (handle, view) = open_view(cx);
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            let now = 100_000_000_i64;
            // Record heavy usage on workspace.delete, then verify "del" still boosts session.delete
            for _ in 0..20 {
                v.palette_state
                    .stats
                    .record_command_use("workspace.delete", now - 1_000);
            }
            let rows = v.build_palette_rows("del", now);
            assert!(!rows.is_empty());
            match &rows[0] {
                PaletteRow::Command { id, .. } => assert_eq!(id, "session.delete"),
                other => panic!("expected session.delete first on 'del', got {other:?}"),
            }

            // Execute pal-cmd-sidebar -> records usage & cycles sidebar
            v.execute_palette_row(
                PaletteRow::Command {
                    id: "pal-cmd-sidebar".into(),
                    label: "Cycle Sidebar".into(),
                    chord: "Ctrl+Shift+B".into(),
                    category: PaletteCategory::SidebarAndTabs,
                    destructive: false,
                    match_rank: 0,
                    frecency: 0,
                },
                now,
                Some(window),
                cx,
            );
            assert_eq!(v.sidebar_state.mode, SidebarMode::Collapsed);
            assert_eq!(v.palette_state.stats.usage_for("pal-cmd-sidebar").count, 1);
        });
        window.render_frame(cx);
    })
    .unwrap();
}

#[gpui_kit::test]
fn palette_populated_scrolls_pal_cmd_inbox_into_view_and_supports_global_exclude_and_group_toggle(
    cx: &mut TestAppContext,
) {
    let (handle, view) = open_view(cx);
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            let now = 200_000_000_i64;
            // Seed >5 recent sessions + >10 Most Used commands (excluding pal-cmd-inbox)
            for i in 0..7 {
                let s = SessionSummary {
                    id: format!("s{i}"),
                    agent: AgentKind::Claude,
                    title: format!("Populated Session {i}"),
                    source_path: format!("/tmp/s{i}.jsonl"),
                    updated_at_ms: now - (i as i64) * 100,
                    ..Default::default()
                };
                let key = v.session_key_for(&s);
                v.sessions.push(s);
                v.sidebar_state
                    .workspace_store
                    .record_session_view(&key, now - (i as i64) * 50);
            }
            v.selected_session = Some(v.sessions[0].clone());

            let most_used_ids = [
                "pal-cmd-rename",
                "pal-cmd-favorite",
                "pal-cmd-pin",
                "pal-cmd-mark-unread",
                "pal-cmd-waiting",
                "pal-cmd-undo-waiting",
                "pal-cmd-ws-starred",
                "pal-cmd-ws-new",
                "pal-cmd-window",
                "pal-cmd-sidebar",
                "pal-cmd-settings",
            ];
            for (i, cid) in most_used_ids.iter().enumerate() {
                v.palette_state
                    .stats
                    .record_command_use(cid, now - (i as i64) * 10);
            }

            // Verify pal-cmd-inbox-exclude is globally reachable
            let exclude_rows = v.build_palette_rows("never show", now);
            assert!(exclude_rows.iter().any(|r| matches!(
                r,
                PaletteRow::Command { id, .. } if id == "pal-cmd-inbox-exclude"
            )));

            // Verify pal-cmd-group-toggle collapses group (no-op when ungrouped)
            let k0 = v.session_key_for(&v.sessions[0]);
            let gid = v
                .sidebar_state
                .workspace_store
                .toggle_group_for_session(
                    &k0,
                    multibot_core::workspaces::DEFAULT_WORKSPACE_ID,
                    None,
                    "Backend",
                )
                .unwrap();
            v.execute_palette_row(
                PaletteRow::Command {
                    id: "pal-cmd-group-toggle".into(),
                    label: "Collapse / expand session group".into(),
                    chord: "Ctrl+G".into(),
                    category: PaletteCategory::SidebarAndTabs,
                    destructive: false,
                    match_rank: 0,
                    frecency: 0,
                },
                now,
                Some(window),
                cx,
            );
            assert!(
                v.sidebar_state
                    .workspace_store
                    .groups
                    .iter()
                    .find(|g| g.id == gid)
                    .unwrap()
                    .collapsed
            );

            // Open populated palette with empty query
            v.open_palette(window, cx);
        });
        window.render_frame(cx);

        // Compute selectable index of pal-cmd-inbox and scroll directly to it via Down navigation
        let target_selectable = view.read_with(cx, |v, _| {
            let rows = v.build_palette_rows("", now_unix_ms());
            rows.iter()
                .filter(|r| !matches!(r, PaletteRow::Header(_)))
                .position(|r| matches!(r, PaletteRow::Command { id, .. } if id == "pal-cmd-inbox"))
                .expect("pal-cmd-inbox must be present in empty-query palette")
        });
        assert!(target_selectable >= 15);

        // Simulate pressing Down until selected_index reaches pal-cmd-inbox
        view.update(cx, |v, cx| {
            let rows = v.build_palette_rows("", now_unix_ms());
            v.palette_state.selected_index = target_selectable;
            v.scroll_palette_to_selected(&rows);
            cx.notify();
        });
        window.render_frame(cx);
        assert!(window.find("pal-cmd-inbox").visible());

        // Verify Up clamps stale out-of-range selected_index immediately on first press
        view.update(cx, |v, cx| {
            v.palette_state.selected_index = 999;
            let ev = KeyDownEvent {
                keystroke: gpui_kit::Keystroke::parse("up").unwrap(),
                is_held: false,
                prefer_character_input: false,
            };
            v.handle_palette_capture_keydown(&ev, window, cx);
            let count = v
                .build_palette_rows("", now_unix_ms())
                .into_iter()
                .filter(|r| !matches!(r, PaletteRow::Header(_)))
                .count();
            assert_eq!(v.palette_state.selected_index, count - 2);
        });

        // Verify CloseOverlay clears tab context and closes palette
        view.update(cx, |v, cx| {
            let k0 = v.session_key_for(&v.sessions[0]);
            v.open_palette_tab_context(k0, window, cx);
        });
        window.render_frame(cx);
        let input_fh = view.read_with(cx, |v, cx| v.palette_input.read(cx).focus_handle(cx));
        input_fh.dispatch_action(&CloseOverlay, window, cx);
        window.render_frame(cx);
        let (modal, tab_ctx) = view.read_with(cx, |v, _| {
            (
                v.active_modal,
                v.palette_state
                    .mode
                    .target_session_key()
                    .map(str::to_string),
            )
        });
        assert_eq!(modal, ActiveModal::None);
        assert!(tab_ctx.is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn palette_real_keypress_arrows_backspace_enter_focus_and_copy_id(cx: &mut TestAppContext) {
    let (handle, view) = open_view(cx);
    let s1 = SessionSummary {
        id: "pal-key-1".into(),
        agent: AgentKind::Claude,
        title: "Alpha Chat".into(),
        updated_at_ms: 20_000,
        ..Default::default()
    };
    let s2 = SessionSummary {
        id: "pal-key-2".into(),
        agent: AgentKind::Codex,
        title: "Beta Chat".into(),
        updated_at_ms: 19_000,
        ..Default::default()
    };

    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.sessions = vec![s1.clone(), s2.clone()];
            v.selected_session = Some(s1.clone());
            v.messages = vec![multibot_proto::ChatMessage {
                id: "tbl-1".into(),
                role: multibot_proto::MessageRole::Assistant,
                text: "| Col A | Col B |\n| --- | --- |\n| r1a | r1b |\n| r2a | r2b |\n| r3a | r3b |\n| r4a | r4b |\n| r5a | r5b |\n| r6a | r6b |\n| r7a | r7b |\n| r8a | r8b |\n| r9a | r9b |\n| r10a | r10b |\n| r11a | r11b |\n| r12a | r12b |\n| r13a | r13b |\n| r14a | r14b |\n| r15a | r15b |\n| r16a | r16b |\n| r17a | r17b |\n| r18a | r18b |\n| r19a | r19b |\n| r20a | r20b |\n| r21a | r21b |\n| r22a | r22b |\n| r23a | r23b |\n| r24a | r24b |".into(),
                ..Default::default()
            }];
            cx.notify();
        });
        window.render_frame(cx);

        // 1. Open Ctrl+K (focuses palette-search Input) and navigate with REAL down/up keypresses
        window.press("ctrl-k", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).active_modal, ActiveModal::Palette);
        assert_eq!(view.read(cx).palette_state.selected_index, 0);

        window.press("down", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).palette_state.selected_index,
            1,
            "Real Down keypress while palette-search Input is focused must advance selection"
        );

        window.press("up", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).palette_state.selected_index,
            0,
            "Real Up keypress while palette-search Input is focused must move selection up"
        );

        // 2. Tab-context Backspace: edits text when non-empty, exits tab-context when empty
        let k1 = view.read(cx).session_key_for(&s1);
        view.update(cx, |v, cx| {
            v.open_palette_tab_context(k1.clone(), window, cx);
            v.palette_input.update(cx, |inp, cx| {
                inp.set_value("a", window, cx);
            });
        });
        window.render_frame(cx);

        window.press("backspace", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).palette_input.read(cx).value().as_ref(), "");
        assert_eq!(
            view.read(cx).palette_state.mode.target_session_key(),
            Some(k1.as_str()),
            "Backspace on non-empty query must delete text without exiting tab-context mode"
        );

        window.press("backspace", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).palette_state.mode,
            PaletteMode::Global,
            "Backspace on empty query must exit tab-context mode"
        );

        // 3. Copy Chat ID via real Enter writes session key to clipboard
        view.update(cx, |v, cx| {
            v.palette_input.update(cx, |inp, cx| {
                inp.set_value("copy chat id", window, cx);
                inp.focus(window, cx);
            });
        });
        window.render_frame(cx);
        window.press("enter", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).active_modal, ActiveModal::None);
        let clip_text = cx.read_from_clipboard().and_then(|c| c.text());
        assert_eq!(
            clip_text.as_deref(),
            Some(k1.as_str()),
            "pal-cmd-copy-id must copy the canonical session ID to the clipboard"
        );

        // 4. Expand Table via real Enter passes Some(window) and focuses TableModal for scrolling
        window.press("ctrl-k", cx);
        window.render_frame(cx);
        view.update(cx, |v, cx| {
            v.palette_input.update(cx, |inp, cx| {
                inp.set_value("expand table", window, cx);
                inp.focus(window, cx);
            });
        });
        window.render_frame(cx);
        window.press("enter", cx);
        window.render_frame(cx);
        assert!(view.read(cx).table_modal_state.is_open());
        window.press("down", cx);
        window.render_frame(cx);
        assert!(
            view.read(cx).table_modal_state.scroll.y > 0,
            "Enter on Expand Table in Ctrl+K must focus table modal so Down scrolls it immediately"
        );

        // Close table modal
        window.press("escape", cx);
        window.render_frame(cx);
        assert!(!view.read(cx).table_modal_state.is_open());

        // 5. Open Cross-workspace Inbox via real Enter focuses Inbox so vim `j` works immediately
        window.press("ctrl-k", cx);
        window.render_frame(cx);
        view.update(cx, |v, cx| {
            v.palette_input.update(cx, |inp, cx| {
                inp.set_value("open cross-workspace inbox", window, cx);
                inp.focus(window, cx);
            });
        });
        window.render_frame(cx);
        window.press("enter", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);
        assert_eq!(view.read(cx).inbox_state.selected_row, 0);
        window.press("j", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).inbox_state.selected_row,
            1,
            "Enter on Open Cross-workspace Inbox in Ctrl+K must focus Inbox immediately"
        );
    })
    .unwrap();
}
