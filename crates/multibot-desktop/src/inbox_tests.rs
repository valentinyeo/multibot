use super::*;
use gpui_kit::{AppContext, Entity, TestAppContext, WindowOptions, test::TestWindowExt};
use multibot_proto::{AgentKind, SessionSummary};
use std::sync::Arc;

fn sample_splits() -> Vec<InboxSplit> {
    vec![
        InboxSplit::Frequent,
        InboxSplit::All,
        InboxSplit::Favorites,
        InboxSplit::Workspace("general".into()),
    ]
}

fn sample_keys() -> Vec<String> {
    vec![
        "local:claude:s1".into(),
        "local:codex:s2".into(),
        "local:antigravity:s3".into(),
    ]
}

#[test]
fn inbox_clamped_rows_clamped_hl_and_wrapped_tab() {
    let mut st = InboxState::default();
    let splits = sample_splits();
    let keys = sample_keys();
    let none = Modifiers::default();

    // j/k clamp at boundaries
    assert_eq!(
        st.handle_key("k", None, none, 1000, &splits, &keys),
        InboxKeyOutcome::Handled
    );
    assert_eq!(st.selected_row, 0);
    st.handle_key("j", None, none, 1000, &splits, &keys);
    st.handle_key("j", None, none, 1000, &splits, &keys);
    st.handle_key("j", None, none, 1000, &splits, &keys);
    assert_eq!(st.selected_row, 2);

    // h/l clamp splits
    st.active_split = InboxSplit::Frequent;
    st.handle_key("left", None, none, 1000, &splits, &keys);
    assert_eq!(st.active_split, InboxSplit::Frequent);
    st.handle_key("right", None, none, 1000, &splits, &keys);
    assert_eq!(st.active_split, InboxSplit::All);
    st.handle_key("right", None, none, 1000, &splits, &keys);
    st.handle_key("right", None, none, 1000, &splits, &keys);
    st.handle_key("right", None, none, 1000, &splits, &keys);
    assert_eq!(st.active_split, InboxSplit::Workspace("general".into()));

    // Tab wraps forward, Shift+Tab wraps backward
    st.handle_key("tab", None, none, 1000, &splits, &keys);
    assert_eq!(st.active_split, InboxSplit::Frequent);
    st.handle_key(
        "tab",
        None,
        Modifiers {
            shift: true,
            ..Default::default()
        },
        1000,
        &splits,
        &keys,
    );
    assert_eq!(st.active_split, InboxSplit::Workspace("general".into()));
}

#[test]
fn inbox_gg_600ms_window_and_shift_g_jump() {
    let mut st = InboxState::default();
    let splits = sample_splits();
    let keys = sample_keys();
    let none = Modifiers::default();

    // G jumps to bottom
    st.handle_key(
        "g",
        Some("G"),
        Modifiers {
            shift: true,
            ..Default::default()
        },
        1000,
        &splits,
        &keys,
    );
    assert_eq!(st.selected_row, 2);

    // Single g starts timer; >600ms does NOT jump to top
    st.handle_key("g", Some("g"), none, 1000, &splits, &keys);
    assert_eq!(st.selected_row, 2);
    st.handle_key("g", Some("g"), none, 1601, &splits, &keys);
    assert_eq!(st.selected_row, 2);

    // Second g within 600ms jumps to top
    st.handle_key("g", Some("g"), none, 2201, &splits, &keys);
    assert_eq!(st.selected_row, 0);

    // Intervening key cancels pending g
    st.selected_row = 2;
    st.handle_key("g", Some("g"), none, 3000, &splits, &keys);
    st.handle_key("j", Some("j"), none, 3100, &splits, &keys);
    st.handle_key("g", Some("g"), none, 3200, &splits, &keys);
    assert_eq!(st.selected_row, 2);
}

#[test]
fn inbox_actions_digits_enter_waiting_undo_unread_f2_filter_and_escape() {
    let mut st = InboxState::default();
    let splits = sample_splits();
    let keys = sample_keys();
    let none = Modifiers::default();

    // Direct digit jump
    st.handle_key("2", Some("2"), none, 1000, &splits, &keys);
    assert_eq!(st.selected_row, 1);

    // Enter arms step order and emits OpenSession
    assert_eq!(
        st.handle_key("enter", None, none, 1000, &splits, &keys),
        InboxKeyOutcome::OpenSession("local:codex:s2".into())
    );
    assert_eq!(st.step_order, keys);
    assert_eq!(st.step_index, Some(1));

    // e / z / u / Ctrl+Shift+U / F2
    assert_eq!(
        st.handle_key("e", Some("e"), none, 1000, &splits, &keys),
        InboxKeyOutcome::MarkWaiting("local:codex:s2".into())
    );
    assert_eq!(
        st.handle_key("z", Some("z"), none, 1000, &splits, &keys),
        InboxKeyOutcome::UndoWaiting
    );
    assert_eq!(
        st.handle_key("u", Some("u"), none, 1000, &splits, &keys),
        InboxKeyOutcome::ToggleUnread("local:codex:s2".into())
    );
    assert_eq!(
        st.handle_key(
            "u",
            None,
            Modifiers {
                control: true,
                shift: true,
                ..Default::default()
            },
            1000,
            &splits,
            &keys
        ),
        InboxKeyOutcome::ToggleUnreadOnly
    );
    assert_eq!(
        st.handle_key("f2", None, none, 1000, &splits, &keys),
        InboxKeyOutcome::StartRename("local:codex:s2".into())
    );

    // `/` opens and focuses the search bar, typing appends, Esc closes/clears, second Esc closes Inbox
    assert!(!st.inbox_search_open, "Inbox search starts hidden");
    st.handle_key("/", Some("/"), none, 1000, &splits, &keys);
    assert!(st.filter_focused);
    assert!(st.inbox_search_open);
    st.handle_key("a", Some("a"), none, 1000, &splits, &keys);
    assert_eq!(st.filter_query, "a");
    assert_eq!(
        st.handle_key("escape", None, none, 1000, &splits, &keys),
        InboxKeyOutcome::Handled
    );
    assert!(!st.filter_focused);
    assert!(!st.inbox_search_open);
    assert!(st.filter_query.is_empty());
    assert_eq!(
        st.handle_key("escape", None, none, 1000, &splits, &keys),
        InboxKeyOutcome::CloseInbox
    );
}

#[test]
fn inbox_ctrl_f_opens_search_and_esc_closes_even_with_empty_query() {
    let mut st = InboxState::default();
    let splits = sample_splits();
    let keys = sample_keys();
    let none = Modifiers::default();

    assert!(!st.inbox_search_open);

    // Ctrl+F opens and focuses the search bar
    assert_eq!(
        st.handle_key(
            "f",
            Some("f"),
            Modifiers {
                control: true,
                ..Default::default()
            },
            1000,
            &splits,
            &keys
        ),
        InboxKeyOutcome::Handled
    );
    assert!(st.inbox_search_open);
    assert!(st.filter_focused);

    // Esc with an EMPTY query still closes the bar and keeps Inbox open
    assert_eq!(
        st.handle_key("escape", None, none, 1000, &splits, &keys),
        InboxKeyOutcome::Handled
    );
    assert!(!st.inbox_search_open);
    assert!(!st.filter_focused);
    assert!(st.filter_query.is_empty());

    // Esc again closes Inbox
    assert_eq!(
        st.handle_key("escape", None, none, 1000, &splits, &keys),
        InboxKeyOutcome::CloseInbox
    );

    // A query typed then blurred (state left from a filter) still counts as visible
    st.filter_query = "multi".into();
    assert!(st.is_search_visible());
    assert!(st.close_search());
    assert!(st.filter_query.is_empty());
    assert!(!st.is_search_visible());
}

#[gpui_kit::test]
fn renders_minimal_inbox_without_header_or_filter_chips_and_opens_clicked_row(
    cx: &mut TestAppContext,
) {
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
    let (handle, view): (gpui_kit::AnyWindowHandle, Entity<MultibotView>) = cx.update(|cx| {
        gpui_kit::open_window(WindowOptions::default(), cx, move |window, cx| {
            cx.new(|cx| MultibotView::new(rt, window, cx))
        })
        .unwrap()
    });

    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.main_surface = MainSurface::Inbox;
            v.connection.sessions = vec![
                SessionSummary {
                    id: "c1".into(),
                    agent: AgentKind::Claude,
                    title: "Claude Deploy".into(),
                    cwd: Some("/srv/app".into()),
                    source_path: "/tmp/c1.jsonl".into(),
                    updated_at_ms: 9_000,
                    needs_attention: true,
                    last_assistant_preview: Some("Ready to ship".into()),
                    ..Default::default()
                },
                SessionSummary {
                    id: "x1".into(),
                    agent: AgentKind::Codex,
                    title: "Codex Refactor".into(),
                    cwd: Some("/srv/api".into()),
                    source_path: "/tmp/x1.jsonl".into(),
                    updated_at_ms: 8_000,
                    ..Default::default()
                },
            ];
            let k1 = v.session_key_for(&v.connection.sessions[0]);
            // Favorites and Frequent each hold one session so their pills render
            v.sidebar_state.workspace_store.toggle_favorite(&k1);
            v.sidebar_state
                .workspace_store
                .record_session_input(&k1, now_unix_ms());
            cx.notify();
        });
        window.render_frame(cx);

        // Minimal Inbox: workspace splits stay, nothing else; the search bar is on demand
        window.click("inbox-split-frequent", cx);
        window.render_frame(cx);
        window.click("inbox-split-favorites", cx);
        window.render_frame(cx);
        window.click("inbox-tab-all", cx);
        window.render_frame(cx);
        assert!(
            window.try_find("inbox-filter-input").is_none(),
            "Inbox must start with the search bar hidden"
        );
        window.press("ctrl-f", cx);
        window.render_frame(cx);
        assert!(
            window.try_find("inbox-filter-input").is_some(),
            "Ctrl+F must mount the Inbox search bar"
        );
        for removed in [
            "inbox-tab-claude",
            "inbox-tab-agy",
            "inbox-tab-codex",
            "inbox-unread-only-pill",
            "header-title-pill",
            "toggle-inbox",
            "chat-menu",
        ] {
            assert!(
                window.try_find(removed).is_none(),
                "Minimal Inbox must not render {removed}"
            );
        }

        // Click first row -> switches to Chat with c1 selected
        window.click("inbox-row-0", cx);
        window.render_frame(cx);

        let (surface, sel_id) = view.read_with(cx, |v, _| {
            (
                v.main_surface,
                v.selection.selected_session.as_ref().map(|s| s.id.clone()),
            )
        });
        assert_eq!(surface, MainSurface::Chat);
        assert_eq!(sel_id.as_deref(), Some("c1"));
    })
    .unwrap();
}

#[gpui_kit::test]
fn minimal_inbox_keeps_keyboard_navigation_and_esc_close(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        crate::theme::apply_multibot_dark_theme(cx);
        crate::bind_multibot_keys(cx);
        crate::state::SharedAppStore::init(cx, multibot_core::MultibotSettings::default());
    });
    let rt = Arc::new(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap(),
    );
    let (handle, view): (gpui_kit::AnyWindowHandle, Entity<MultibotView>) = cx.update(|cx| {
        gpui_kit::open_window(WindowOptions::default(), cx, move |window, cx| {
            cx.new(|cx| MultibotView::new(rt, window, cx))
        })
        .unwrap()
    });

    let now = now_unix_ms();
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.connection.sessions = vec![
                SessionSummary {
                    id: "kb-1".into(),
                    agent: AgentKind::Claude,
                    title: "Keyboard One".into(),
                    updated_at_ms: now - 2_000,
                    ..Default::default()
                },
                SessionSummary {
                    id: "kb-2".into(),
                    agent: AgentKind::Codex,
                    title: "Keyboard Two".into(),
                    updated_at_ms: now - 1_000,
                    ..Default::default()
                },
            ];
            cx.notify();
        });
        window.render_frame(cx);

        // Alt+I opens the Inbox on the All split with the first row selected
        window.press("alt-i", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);
        assert_eq!(view.read(cx).inbox_state.active_split, InboxSplit::All);
        assert_eq!(view.read(cx).inbox_state.selected_row, 0);

        // The search bar starts hidden; Ctrl+F opens and focuses it
        assert!(!view.read(cx).inbox_state.inbox_search_open);
        assert!(
            window.try_find("inbox-filter-input").is_none(),
            "Inbox must open with the search bar hidden"
        );
        window.press("ctrl-f", cx);
        window.render_frame(cx);
        assert!(view.read(cx).inbox_state.inbox_search_open);
        assert!(view.read(cx).inbox_state.filter_focused);
        assert!(
            window.try_find("inbox-filter-input").is_some(),
            "Ctrl+F must mount the Inbox search bar"
        );

        // Multi-word query "two k" matches "Keyboard Two" via token matching, not "Keyboard One"
        for ch in ["t", "w", "o"] {
            window.press(ch, cx);
        }
        window.press("space", cx);
        window.press("k", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).inbox_state.filter_query, "two k");
        let rows = view.read(cx).compute_inbox_rows(now_unix_ms());
        assert_eq!(
            rows.iter()
                .map(|r| r.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["kb-2"],
            "multi-word query must match `Keyboard Two` only"
        );

        // 1st Esc clears the query and hides the bar but keeps Inbox open
        window.press("escape", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);
        assert!(!view.read(cx).inbox_state.inbox_search_open);
        assert!(!view.read(cx).inbox_state.filter_focused);
        assert!(view.read(cx).inbox_state.filter_query.is_empty());
        assert!(window.try_find("inbox-filter-input").is_none());

        // J / K move the selection and clamp at both ends
        window.press("j", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).inbox_state.selected_row, 1);
        window.press("j", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).inbox_state.selected_row,
            1,
            "j must clamp at the last row"
        );
        window.press("k", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).inbox_state.selected_row, 0);
        window.press("k", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).inbox_state.selected_row,
            0,
            "k must clamp at the first row"
        );

        // Down / Up work like J / K
        window.press("down", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).inbox_state.selected_row, 1);
        window.press("up", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).inbox_state.selected_row, 0);

        // Tab / Shift+Tab cycle workspace splits
        let split_before = view.read(cx).inbox_state.active_split.clone();
        window.press("tab", cx);
        window.render_frame(cx);
        assert_ne!(view.read(cx).inbox_state.active_split, split_before);
        window.press("shift-tab", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).inbox_state.active_split, split_before);

        // Enter opens the selected row and returns to Chat
        window.press("enter", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).main_surface, MainSurface::Chat);
        assert_eq!(
            view.read(cx)
                .selection
                .selected_session
                .as_ref()
                .map(|s| s.id.as_str()),
            Some("kb-2"),
            "Enter must open the newest Inbox row (kb-2)"
        );

        // Alt+I again, then Esc closes the Inbox
        window.press("alt-i", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);
        window.press("escape", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).main_surface, MainSurface::Chat);
    })
    .unwrap();
}

#[test]
fn inbox_gg_rejects_clock_regression_and_up_clamps_shrunk_list_and_hl_recovers_vanished_split() {
    let mut st = InboxState::default();
    let splits = sample_splits();
    let keys = sample_keys();
    let none = Modifiers::default();

    // 1. gg rejects backward clock step (now_ms < prev)
    st.selected_row = 2;
    st.handle_key("g", Some("g"), none, 10_000, &splits, &keys);
    st.handle_key("g", Some("g"), none, 2_000, &splits, &keys);
    assert_eq!(st.selected_row, 2);
    assert_eq!(st.last_g_press_ms, Some(2_000));
    // Second g within 600ms of 2_000 succeeds
    st.handle_key("g", Some("g"), none, 2_500, &splits, &keys);
    assert_eq!(st.selected_row, 0);

    // 2. Stale selected_row after list shrinks moves immediately on first k / Up
    st.selected_row = 2;
    let shrunk = vec![keys[0].clone(), keys[1].clone()]; // max_idx = 1
    st.handle_key("k", Some("k"), none, 3_000, &splits, &shrunk);
    assert_eq!(st.selected_row, 0);

    // Also in filter mode
    st.filter_focused = true;
    st.selected_row = 5;
    st.handle_key("up", None, none, 3_100, &splits, &shrunk);
    assert_eq!(st.selected_row, 0);
    st.filter_focused = false;

    // 3. h / l recover to visible_splits[0] when active_split vanished
    st.active_split = InboxSplit::Workspace("snoozed-ws".into());
    st.selected_row = 2;
    st.handle_key("left", None, none, 3_200, &splits, &keys);
    assert_eq!(st.active_split, splits[0]);
    assert_eq!(st.selected_row, 0);

    st.active_split = InboxSplit::Workspace("deleted-ws".into());
    st.selected_row = 2;
    st.handle_key("right", None, none, 3_300, &splits, &keys);
    assert_eq!(st.active_split, splits[0]);
    assert_eq!(st.selected_row, 0);
}

#[gpui_kit::test]
fn inbox_stepping_disarms_outside_inbox_and_close_overlay_respects_rename_filter_hierarchy(
    cx: &mut TestAppContext,
) {
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
    let (handle, view): (gpui_kit::AnyWindowHandle, Entity<MultibotView>) = cx.update(|cx| {
        gpui_kit::open_window(WindowOptions::default(), cx, move |window, cx| {
            cx.new(|cx| MultibotView::new(rt, window, cx))
        })
        .unwrap()
    });

    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.connection.sessions = vec![
                SessionSummary {
                    id: "c1".into(),
                    agent: AgentKind::Claude,
                    title: "One".into(),
                    source_path: "/tmp/c1.jsonl".into(),
                    updated_at_ms: 9_000,
                    ..Default::default()
                },
                SessionSummary {
                    id: "c2".into(),
                    agent: AgentKind::Codex,
                    title: "Two".into(),
                    source_path: "/tmp/c2.jsonl".into(),
                    updated_at_ms: 8_000,
                    ..Default::default()
                },
            ];
            let k1 = v.session_key_for(&v.connection.sessions[0]);
            let k2 = v.session_key_for(&v.connection.sessions[1]);

            // Arm stepping via Inbox
            v.inbox_state.step_order = vec![k1.clone(), k2.clone()];
            v.inbox_state.step_index = Some(0);
            assert!(v.step_inbox_session(1, cx));
            assert_eq!(v.selection.selected_session.as_ref().unwrap().id, "c2");

            // Opening a session from Sidebar (from_inbox = false) disarms stepping completely
            let s1 = v.connection.sessions[0].clone();
            v.open_session_with_tracking(s1, false, cx);
            assert!(v.inbox_state.step_index.is_none());
            assert!(v.inbox_state.step_order.is_empty());
            assert!(!v.step_inbox_session(1, cx));

            // Open Inbox with rename + matching filter active and focus Inbox
            v.main_surface = MainSurface::Inbox;
            v.inbox_state.filter_focused = true;
            v.inbox_state.filter_query = "one".into();
            v.inbox_state.rename = Some(InlineRenameState::new(k1, "c1".into(), "One".into()));
            v.focus_inbox(window, cx);
            cx.notify();
        });
        window.render_frame(cx);

        let fh = view.read_with(cx, |v, _| v.inbox_state.focus_handle.clone().unwrap());

        // 1st CloseOverlay cancels rename only
        fh.dispatch_action(&CloseOverlay, window, cx);
        window.render_frame(cx);
        assert!(view.read_with(cx, |v, _| v.inbox_state.rename.is_none()
            && v.inbox_state.filter_focused
            && v.main_surface == MainSurface::Inbox));

        // 2nd CloseOverlay clears & blurs filter only
        fh.dispatch_action(&CloseOverlay, window, cx);
        window.render_frame(cx);
        assert!(view.read_with(cx, |v, _| !v.inbox_state.filter_focused
            && v.inbox_state.filter_query.is_empty()
            && v.main_surface == MainSurface::Inbox));

        // 3rd CloseOverlay closes Inbox
        fh.dispatch_action(&CloseOverlay, window, cx);
        window.render_frame(cx);
        assert_eq!(view.read_with(cx, |v, _| v.main_surface), MainSurface::Chat);
    })
    .unwrap();
}

#[gpui_kit::test]
fn inbox_close_overlay_and_keydown_yield_to_active_modal_precedence(cx: &mut TestAppContext) {
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
    let (handle, view): (gpui_kit::AnyWindowHandle, Entity<MultibotView>) = cx.update(|cx| {
        gpui_kit::open_window(WindowOptions::default(), cx, move |window, cx| {
            cx.new(|cx| MultibotView::new(rt, window, cx))
        })
        .unwrap()
    });

    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.connection.sessions = vec![SessionSummary {
                id: "c1".into(),
                agent: AgentKind::Claude,
                title: "Deploy Prod".into(),
                source_path: "/tmp/c1.jsonl".into(),
                updated_at_ms: 9_000,
                needs_attention: true,
                ..Default::default()
            }];
            let k1 = v.session_key_for(&v.connection.sessions[0]);
            v.main_surface = MainSurface::Inbox;
            v.inbox_state.active_split = InboxSplit::All;
            v.inbox_state.filter_focused = true;
            v.inbox_state.filter_query = "deploy".into();
            v.inbox_state.rename = Some(InlineRenameState::new(
                k1,
                "c1".into(),
                "Deploy Prod".into(),
            ));
            v.focus_inbox(window, cx);
            // Open F1 Shortcuts modal while inbox-root retains focus
            v.active_modal = ActiveModal::Shortcuts;
            cx.notify();
        });
        window.render_frame(cx);

        // Raw keydowns ('q' / 'e') while Shortcuts is open must NOT mutate or close Inbox
        view.update(cx, |v, cx| {
            let mut q_ks = gpui_kit::Keystroke::parse("q").unwrap();
            q_ks.key_char = Some("q".into());
            let ev_q = KeyDownEvent {
                keystroke: q_ks,
                is_held: false,
                prefer_character_input: false,
            };
            v.handle_inbox_keydown(&ev_q, window, cx);

            let mut e_ks = gpui_kit::Keystroke::parse("e").unwrap();
            e_ks.key_char = Some("e".into());
            let ev_e = KeyDownEvent {
                keystroke: e_ks,
                is_held: false,
                prefer_character_input: false,
            };
            v.handle_inbox_keydown(&ev_e, window, cx);

            assert_eq!(v.active_modal, ActiveModal::Shortcuts);
            assert_eq!(v.main_surface, MainSurface::Inbox);
            assert_eq!(v.inbox_state.filter_query, "deploy");
            assert!(v.inbox_state.rename.is_some());
            assert!(v.sidebar_state.workspace_store.waiting_sessions.is_empty());
        });

        let fh = view.read_with(cx, |v, _| v.inbox_state.focus_handle.clone().unwrap());

        // 1st CloseOverlay closes ActiveModal::Shortcuts FIRST, leaving Inbox + rename + filter intact
        fh.dispatch_action(&CloseOverlay, window, cx);
        window.render_frame(cx);
        view.read_with(cx, |v, _| {
            assert_eq!(v.active_modal, ActiveModal::None);
            assert_eq!(v.main_surface, MainSurface::Inbox);
            assert!(v.inbox_state.rename.is_some());
            assert_eq!(v.inbox_state.filter_query, "deploy");
            assert!(v.inbox_state.filter_focused);
        });

        // 2nd CloseOverlay walks Inbox hierarchy: cancels rename only
        fh.dispatch_action(&CloseOverlay, window, cx);
        window.render_frame(cx);
        view.read_with(cx, |v, _| {
            assert!(v.inbox_state.rename.is_none());
            assert!(v.inbox_state.filter_focused);
            assert_eq!(v.inbox_state.filter_query, "deploy");
            assert_eq!(v.main_surface, MainSurface::Inbox);
        });

        // 3rd CloseOverlay clears & blurs filter only
        fh.dispatch_action(&CloseOverlay, window, cx);
        window.render_frame(cx);
        view.read_with(cx, |v, _| {
            assert!(!v.inbox_state.filter_focused);
            assert!(v.inbox_state.filter_query.is_empty());
            assert_eq!(v.main_surface, MainSurface::Inbox);
        });

        // 4th CloseOverlay propagates cleanly to multibot-root and closes Inbox -> Chat
        fh.dispatch_action(&CloseOverlay, window, cx);
        window.render_frame(cx);
        assert_eq!(view.read_with(cx, |v, _| v.main_surface), MainSurface::Chat);
    })
    .unwrap();
}

#[gpui_kit::test]
fn real_tab_and_shift_tab_in_inbox_cycle_splits_and_keep_inbox_focus(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        crate::theme::apply_multibot_dark_theme(cx);
        crate::bind_multibot_keys(cx);
        crate::state::SharedAppStore::init(cx, multibot_core::MultibotSettings::default());
    });
    let rt = Arc::new(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap(),
    );
    let (handle, view): (gpui_kit::AnyWindowHandle, Entity<MultibotView>) = cx.update(|cx| {
        gpui_kit::open_window(WindowOptions::default(), cx, move |window, cx| {
            cx.new(|cx| MultibotView::new(rt, window, cx))
        })
        .unwrap()
    });

    let now = now_unix_ms();
    let mut empty_ws_id = String::new();
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            let s1 = SessionSummary {
                id: "tab-s1".into(),
                agent: AgentKind::Claude,
                title: "Tab Session 1".into(),
                updated_at_ms: now - 2_000,
                ..Default::default()
            };
            let s2 = SessionSummary {
                id: "tab-s2".into(),
                agent: AgentKind::Codex,
                title: "Tab Session 2".into(),
                updated_at_ms: now - 1_000,
                ..Default::default()
            };
            let k1 = v.session_key_for(&s1);
            v.connection.sessions = vec![s1, s2];
            // Favorites gains a session, the Backend workspace stays empty and is hidden
            v.sidebar_state.workspace_store.toggle_favorite(&k1);
            empty_ws_id = v
                .sidebar_state
                .workspace_store
                .create_workspace("Backend", Some("#3B82F6".into()))
                .expect("Backend workspace must be created")
                .id;
            cx.notify();
        });
        window.render_frame(cx);

        // Open Inbox via real Alt+I keypress (focuses inbox-root on All split)
        window.press("alt-i", cx);
        window.render_frame(cx);

        let splits = view.read_with(cx, |v, _| {
            assert_eq!(v.main_surface, MainSurface::Inbox);
            assert_eq!(v.inbox_state.active_split, InboxSplit::All);
            v.inbox_visible_splits(now_unix_ms())
        });
        assert_eq!(
            splits.iter().take(2).cloned().collect::<Vec<_>>(),
            vec![InboxSplit::All, InboxSplit::Favorites],
            "only non-empty splits stay in the bar (All + Favorites); Frequent 0 is hidden"
        );
        assert!(
            !splits.contains(&InboxSplit::Workspace(empty_ws_id.clone())),
            "the 0-session Backend workspace split must be hidden: {splits:?}"
        );
        let rendered_ws_pills = splits
            .iter()
            .filter(|split| matches!(split, InboxSplit::Workspace(_)))
            .count();
        let hidden_pill_id = format!("inbox-split-ws-{rendered_ws_pills}");
        assert!(
            window.try_find(hidden_pill_id).is_none(),
            "no pill may render for a hidden 0-session workspace split"
        );
        let start_idx = splits.iter().position(|s| *s == InboxSplit::All).unwrap();

        // Press real Tab across every visible split and wrap around back to All
        for step in 1..=splits.len() {
            window.press("tab", cx);
            window.render_frame(cx);
            let expected = &splits[(start_idx + step) % splits.len()];
            assert_eq!(
                &view.read(cx).inbox_state.active_split,
                expected,
                "Tab step {step} must advance/wrap active_split instead of being stolen by Root"
            );
            let inbox_still_focused = view
                .read(cx)
                .inbox_state
                .focus_handle
                .as_ref()
                .is_some_and(|fh| fh.is_focused(window));
            assert!(
                inbox_still_focused,
                "Tab step {step} must keep focus on inbox-root"
            );
        }

        // Press real Shift+Tab to wrap backward
        window.press("shift-tab", cx);
        window.render_frame(cx);
        let expected_prev = &splits[(start_idx + splits.len() - 1) % splits.len()];
        assert_eq!(
            &view.read(cx).inbox_state.active_split,
            expected_prev,
            "Shift+Tab must wrap active_split backward"
        );

        // Return to All split via Tab and verify `j` row navigation still works because focus stayed on inbox-root
        window.press("tab", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).inbox_state.active_split, InboxSplit::All);
        assert_eq!(view.read(cx).inbox_state.selected_row, 0);
        window.press("j", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).inbox_state.selected_row,
            1,
            "`j` must still navigate Inbox rows after Tab/Shift+Tab"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn inbox_split_bar_hides_zero_count_splits_and_tab_skips_them(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        crate::theme::apply_multibot_dark_theme(cx);
        crate::bind_multibot_keys(cx);
        crate::state::SharedAppStore::init(cx, multibot_core::MultibotSettings::default());
    });
    let rt = Arc::new(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap(),
    );
    let (handle, view): (gpui_kit::AnyWindowHandle, Entity<MultibotView>) = cx.update(|cx| {
        gpui_kit::open_window(WindowOptions::default(), cx, move |window, cx| {
            cx.new(|cx| MultibotView::new(rt, window, cx))
        })
        .unwrap()
    });

    // 1. Empty Inbox: `All` stays visible with 0 sessions, every other split is hidden
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.press("alt-i", cx);
        window.render_frame(cx);
        let splits = view.read_with(cx, |v, _| v.inbox_visible_splits(now_unix_ms()));
        assert_eq!(
            splits,
            vec![InboxSplit::All],
            "All must stay visible even at 0 total sessions"
        );
        assert!(
            window.try_find("inbox-tab-all").is_some(),
            "the All pill must render in an empty Inbox"
        );
        assert!(
            window.try_find("inbox-split-frequent").is_none(),
            "Frequent 0 must be hidden"
        );
        assert!(
            window.try_find("inbox-split-favorites").is_none(),
            "Favorites 0 must be hidden"
        );
    })
    .unwrap();

    // 2. Give Frequent, Favorites and Backend one session each; Empty Shelf stays at 0
    let now = now_unix_ms();
    let mut backend_ws_id = String::new();
    let mut empty_ws_id = String::new();
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            let s1 = SessionSummary {
                id: "hide-s1".into(),
                agent: AgentKind::Claude,
                title: "Hidden Split Session 1".into(),
                updated_at_ms: now - 3_000,
                ..Default::default()
            };
            let s2 = SessionSummary {
                id: "hide-s2".into(),
                agent: AgentKind::Codex,
                title: "Hidden Split Session 2".into(),
                updated_at_ms: now - 2_000,
                ..Default::default()
            };
            let s3 = SessionSummary {
                id: "hide-s3".into(),
                agent: AgentKind::Claude,
                title: "Hidden Split Session 3".into(),
                updated_at_ms: now - 1_000,
                ..Default::default()
            };
            let k1 = v.session_key_for(&s1);
            let k2 = v.session_key_for(&s2);
            let k3 = v.session_key_for(&s3);
            v.connection.sessions = vec![s1, s2, s3];
            let store = &mut v.sidebar_state.workspace_store;
            store.toggle_favorite(&k1);
            store.record_session_input(&k2, now);
            backend_ws_id = store
                .create_workspace("Backend", None)
                .expect("Backend workspace must be created")
                .id;
            empty_ws_id = store
                .create_workspace("Empty Shelf", None)
                .expect("Empty Shelf workspace must be created")
                .id;
            store.assign_session_workspace(&k3, &backend_ws_id);
            cx.notify();
        });
        window.render_frame(cx);

        let splits = view.read_with(cx, |v, _| v.inbox_visible_splits(now_unix_ms()));
        assert!(
            splits.contains(&InboxSplit::Frequent)
                && splits.contains(&InboxSplit::All)
                && splits.contains(&InboxSplit::Favorites)
                && splits.contains(&InboxSplit::Workspace(backend_ws_id.clone())),
            "every split with >= 1 session stays in the bar: {splits:?}"
        );
        assert!(
            !splits.contains(&InboxSplit::Workspace(empty_ws_id.clone())),
            "the 0-session Empty Shelf workspace split must be hidden: {splits:?}"
        );

        // Counts on the surviving pills still come from the live Inbox query
        for (split, expected) in [
            (InboxSplit::Frequent, 1usize),
            (InboxSplit::All, 3),
            (InboxSplit::Favorites, 1),
            (InboxSplit::Workspace(backend_ws_id.clone()), 1),
        ] {
            assert_eq!(
                view.read_with(cx, |v, _| {
                    v.compute_inbox_rows_for(&split, now_unix_ms()).len()
                }),
                expected,
                "visible split {split:?} keeps its real count"
            );
        }

        // Rendered pills: one per visible workspace split, none for the hidden one
        assert!(window.try_find("inbox-split-frequent").is_some());
        assert!(window.try_find("inbox-tab-all").is_some());
        assert!(window.try_find("inbox-split-favorites").is_some());
        let rendered_ws_pills = splits
            .iter()
            .filter(|split| matches!(split, InboxSplit::Workspace(_)))
            .count();
        assert!(
            rendered_ws_pills >= 1,
            "the non-empty workspace splits must render: {splits:?}"
        );
        assert!(
            window.try_find("inbox-split-ws-0").is_some(),
            "the first non-empty workspace pill must render"
        );
        let hidden_pill_id = format!("inbox-split-ws-{rendered_ws_pills}");
        assert!(
            window.try_find(hidden_pill_id).is_none(),
            "the 0-session Empty Shelf pill must NOT render"
        );

        // Empty workspaces stay manageable in the Sidebar/Settings workspace lists
        let managed = view.read_with(cx, |v, _| {
            v.sidebar_state
                .workspace_store
                .visible_workspaces(now_unix_ms())
                .iter()
                .map(|w| w.id.clone())
                .collect::<Vec<_>>()
        });
        assert!(
            managed.contains(&empty_ws_id),
            "empty workspaces must stay visible in the Sidebar workspace switcher"
        );
        assert!(managed.contains(&backend_ws_id));
    })
    .unwrap();

    // 3. Tab / Shift+Tab cycle strictly across the visible splits
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.inbox_state.select_all_split();
            v.focus_inbox(window, cx);
            cx.notify();
        });
        window.render_frame(cx);

        // Tab walks the visible order and wraps back to `All` after one full lap
        let order = view.read_with(cx, |v, _| v.inbox_visible_splits(now_unix_ms()));
        assert_eq!(order.first(), Some(&InboxSplit::Frequent));
        let start_idx = order
            .iter()
            .position(|split| *split == InboxSplit::All)
            .expect("All must be in the visible order");
        for step in 1..=order.len() {
            window.press("tab", cx);
            window.render_frame(cx);
            let active = view.read_with(cx, |v, _| v.inbox_state.active_split.clone());
            assert!(
                active != InboxSplit::Workspace(empty_ws_id.clone()),
                "Tab must never select a hidden 0-count split"
            );
            assert_eq!(
                active,
                order[(start_idx + step) % order.len()],
                "Tab step {step} must walk the visible splits only"
            );
        }
        assert_eq!(view.read_with(cx, |v, _| v.inbox_state.selected_row), 0);

        // Shift+Tab from All steps backward to the split before it in the visible order
        window.press("shift-tab", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read_with(cx, |v, _| v.inbox_state.active_split.clone()),
            order[(start_idx + order.len() - 1) % order.len()],
            "Shift+Tab must wrap backward through visible splits only"
        );

        // 4. A split that empties out is dropped: Tab recovers onto a visible split
        view.update(cx, |v, cx| {
            v.inbox_state.active_split = InboxSplit::Workspace(empty_ws_id.clone());
            v.inbox_state.selected_row = 1;
            cx.notify();
        });
        window.render_frame(cx);
        window.press("tab", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read_with(cx, |v, _| v.inbox_state.active_split.clone()),
            order[(start_idx + 1) % order.len()],
            "Tab from a hidden split must land on the next visible split, never on a dead end"
        );
        assert_eq!(view.read_with(cx, |v, _| v.inbox_state.selected_row), 0);
    })
    .unwrap();
}
