use super::*;

#[test]
fn dissolved_shell_survives_save_reload_and_fresh_discovery() {
    let dir = unique_temp_dir("dissolved-shell-reload");
    let path = dir.join("workspaces.json");
    let dissolved = make_session(
        "shell-dead",
        AgentKind::Shell,
        "Dissolved",
        10,
        false,
        false,
    );
    let active = make_session("shell-live", AgentKind::Shell, "Active", 20, false, true);
    let key = canonical_session_key("VPS", dissolved.agent, &dissolved.id);
    let active_key = canonical_session_key("VPS", active.agent, &active.id);
    let mut store = WorkspaceStore::new();
    store.dissolve_session(&key, 30);
    store.save_to(&path).unwrap();
    let mut restored = WorkspaceStore::load_from(&path).unwrap();
    restored.reconcile_live_sessions([("VPS", &dissolved), ("VPS", &active)]);
    assert!(!restored.auto_unarchive_waiting_if_active(&key, &dissolved));
    assert_eq!(
        restored.session_lifecycle(&key, &dissolved, 40),
        SessionLifecycle::Dissolved
    );
    assert_eq!(
        restored.session_lifecycle(&active_key, &active, 40),
        SessionLifecycle::Active
    );
    let inputs = [make_input("VPS", &dissolved), make_input("VPS", &active)];
    let layout = restored.compute_sidebar_layout(ALL_WORKSPACES_ID, &inputs, false, 40);
    assert!(!layout.flat_display_order.contains(&key));
    assert!(layout.flat_display_order.contains(&active_key));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn pi_cursor_session_keys_remain_distinct() {
    assert_eq!(agent_slug(AgentKind::Pi), "pi");
    assert_eq!(agent_slug(AgentKind::Cursor), "cursor");
    assert_ne!(
        canonical_session_key("host", AgentKind::Pi, "same"),
        canonical_session_key("host", AgentKind::Cursor, "same")
    );
}

static TEST_DIR_SEQ: AtomicU64 = AtomicU64::new(1);

fn unique_temp_dir(label: &str) -> PathBuf {
    let seq = TEST_DIR_SEQ.fetch_add(1, AtomicOrdering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("multibot-ws-{label}-{}-{seq}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn make_session(
    id: &str,
    agent: AgentKind,
    title: &str,
    updated_at_ms: i64,
    needs_attention: bool,
    working: bool,
) -> SessionSummary {
    SessionSummary {
        id: id.to_string(),
        agent,
        title: title.to_string(),
        cwd: Some(format!("/repo/{id}")),
        source_path: format!("/tmp/{id}.jsonl"),
        updated_at_ms,
        working,
        last_assistant_id: if needs_attention {
            Some(format!("asst-{id}-{updated_at_ms}"))
        } else {
            None
        },
        last_assistant_preview: Some(format!("Preview for {title}")),
        last_assistant_ms: Some(updated_at_ms),
        needs_attention,
        ..Default::default()
    }
}

fn make_input<'a>(host: &'a str, summary: &'a SessionSummary) -> SessionInput<'a> {
    SessionInput {
        key: canonical_session_key(host, summary.agent, &summary.id),
        host,
        summary,
    }
}

#[test]
fn dissolved_workspace_preserves_all_chats_and_its_named_split_after_reload() {
    let mut store = WorkspaceStore::new();
    let workspace = store.create_workspace("Project", None).unwrap();
    let sessions: Vec<_> = (0..3)
        .map(|i| {
            make_session(
                &format!("workspace-{i}"),
                AgentKind::Claude,
                "Chat",
                100,
                false,
                false,
            )
        })
        .collect();
    let inputs: Vec<_> = sessions.iter().map(|s| make_input("h", s)).collect();
    let keys: Vec<_> = inputs.iter().map(|i| i.key.clone()).collect();
    for key in &keys {
        store.assign_session_workspace(key, &workspace.id);
    }
    store.assign_session_workspace("offline-chat", &workspace.id);
    store.archive_to_waiting_session(&keys[1], 100, &sessions[1]);
    store.hibernate_session(&keys[2], &sessions[2], Some(5000), None, true);
    store
        .dissolve_workspace(&workspace.id, &keys, 1000)
        .unwrap();
    let store: WorkspaceStore =
        serde_json::from_str(&serde_json::to_string(&store).unwrap()).unwrap();
    assert!(
        !store
            .visible_workspaces(1000)
            .iter()
            .any(|w| w.id == workspace.id)
    );
    assert!(store.session_meta["offline-chat"].dissolved_at_ms.is_some());
    for lifecycle in [
        SessionLifecycle::Active,
        SessionLifecycle::Done,
        SessionLifecycle::Hibernated,
    ] {
        assert!(
            store
                .query_lifecycle(lifecycle, &InboxSplit::All, &inputs, false, "", 1000)
                .is_empty()
        );
        assert!(
            !store
                .lifecycle_inbox_splits(lifecycle, 1000)
                .contains(&InboxSplit::Workspace(workspace.id.clone()))
        );
    }
    let split = InboxSplit::Workspace(workspace.id.clone());
    assert!(
        store
            .lifecycle_inbox_splits(SessionLifecycle::Dissolved, 1000)
            .contains(&split)
    );
    let rows = store.query_lifecycle(
        SessionLifecycle::Dissolved,
        &split,
        &inputs,
        false,
        "",
        1000,
    );
    assert_eq!(rows.len(), 3);
    for row in rows {
        assert_eq!(store.effective_workspace_id(&row.session_key), workspace.id);
    }
}

#[test]
fn dissolving_general_keeps_unassigned_chats_there_and_creates_a_new_landing_workspace() {
    let mut store = WorkspaceStore::new();
    let session = make_session("general-chat", AgentKind::Claude, "Chat", 100, false, false);
    let input = make_input("h", &session);
    store
        .dissolve_workspace(DEFAULT_WORKSPACE_ID, &[input.key.clone()], 1000)
        .unwrap();
    assert!(
        store
            .workspace_by_id(DEFAULT_WORKSPACE_ID)
            .unwrap()
            .archived
    );
    assert_ne!(store.default_workspace_id(), DEFAULT_WORKSPACE_ID);
    assert_eq!(
        store.effective_workspace_id(&input.key),
        DEFAULT_WORKSPACE_ID
    );
    assert_eq!(
        store
            .query_lifecycle(
                SessionLifecycle::Dissolved,
                &InboxSplit::Workspace(DEFAULT_WORKSPACE_ID.into()),
                &[input],
                false,
                "",
                1000
            )
            .len(),
        1
    );
}

#[test]
fn lifecycle_views_share_workspace_splits_and_sidebar_only_shows_active() {
    let mut store = WorkspaceStore::new();
    let now = now_unix_ms();
    let workspace = store.create_workspace("Project", None).unwrap();
    let mut sessions: Vec<_> = (0..4)
        .map(|i| {
            make_session(
                &format!("life-{i}"),
                AgentKind::Claude,
                "Chat",
                100,
                false,
                false,
            )
        })
        .collect();
    let keys: Vec<_> = sessions
        .iter()
        .map(|s| canonical_session_key("h", s.agent, &s.id))
        .collect();
    for key in &keys {
        store.assign_session_workspace(key, &workspace.id);
    }
    store.archive_to_waiting_session(&keys[1], now, &sessions[1]);
    store.hibernate_session(&keys[2], &sessions[2], Some(now + 1000), None, true);
    store.dissolve_session(&keys[3], now);
    sessions[3].archived = true;
    sessions[3].archive_kill_process = true;
    let inputs: Vec<_> = sessions.iter().map(|s| make_input("h", s)).collect();
    for (idx, lifecycle) in SessionLifecycle::ALL.into_iter().enumerate() {
        for split in [InboxSplit::All, InboxSplit::Workspace(workspace.id.clone())] {
            let rows = store.query_lifecycle(lifecycle, &split, &inputs, false, "", now);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].session_key, keys[idx]);
        }
    }
    let layout = store.compute_sidebar_layout(&workspace.id, &inputs, false, now);
    assert_eq!(layout.recents.len(), 1);
    assert_eq!(layout.recents[0].session_key, keys[0]);
}

#[test]
fn hibernate_modes_reply_timer_and_dissolved_precedence_survive_serde() {
    let mut store = WorkspaceStore::new();
    let mut session = make_session("life", AgentKind::Claude, "Chat", 100, false, false);
    session.last_assistant_id = Some("a1".into());
    let key = canonical_session_key("h", session.agent, &session.id);
    let now = now_unix_ms();
    assert!(!SessionMeta::default().hibernate_wake_on_reply);
    assert!(
        !serde_json::from_str::<SessionMeta>("{}")
            .unwrap()
            .hibernate_wake_on_reply
    );
    for wake_on_reply in [true, false] {
        store.hibernate_session(
            &key,
            &session,
            Some(now + 1000),
            Some("note"),
            wake_on_reply,
        );
        let saved = serde_json::to_string(&store).unwrap();
        store = serde_json::from_str(&saved).unwrap();
        let mut metadata_only = session.clone();
        metadata_only.updated_at_ms = 500;
        metadata_only.working = true;
        assert!(!store.auto_unarchive_waiting_if_active(&key, &metadata_only));
        let mut reply = session.clone();
        reply.last_assistant_id = Some("a2".into());
        reply.last_assistant_ms = Some(200);
        assert_eq!(
            store.auto_unarchive_waiting_if_active(&key, &reply),
            wake_on_reply
        );
        assert_eq!(
            store.session_lifecycle(&key, &reply, now),
            if wake_on_reply {
                SessionLifecycle::Active
            } else {
                SessionLifecycle::Hibernated
            }
        );
        store.wake_due_sessions(now + 1000);
        assert_eq!(
            store.session_lifecycle(&key, &reply, now + 1000),
            SessionLifecycle::Active
        );
    }
    store.hibernate_session(&key, &session, Some(now + 1000), None, false);
    store.record_session_input(&key, now);
    assert_eq!(
        store.session_lifecycle(&key, &session, now),
        SessionLifecycle::Active
    );
    store.dissolve_session(&key, now);
    store.set_reminder(&key, Some(now - 1), None);
    session.last_assistant_id = Some("a3".into());
    assert!(!store.auto_unarchive_waiting_if_active(&key, &session));
    assert!(!store.wake_due_sessions(now));
    assert_eq!(
        store.session_lifecycle(&key, &session, now),
        SessionLifecycle::Dissolved
    );
    store.move_to_inbox(&key);
    assert_eq!(
        store.session_lifecycle(&key, &session, now),
        SessionLifecycle::Active
    );
}

#[test]
fn legacy_server_archive_is_done_but_killed_archive_is_dissolved() {
    let mut store = WorkspaceStore::new();
    let mut session = make_session("legacy", AgentKind::Claude, "Legacy", 100, false, false);
    let key = canonical_session_key("h", session.agent, &session.id);
    session.archived = true;
    assert_eq!(
        store.session_lifecycle(&key, &session, 200),
        SessionLifecycle::Done
    );
    assert!(!store.auto_unarchive_waiting_if_active(&key, &session));
    session.last_assistant_id = Some("new-reply".into());
    assert!(store.auto_unarchive_waiting_if_active(&key, &session));
    session.archive_kill_process = true;
    assert_eq!(
        store.session_lifecycle(&key, &session, 200),
        SessionLifecycle::Dissolved
    );
    assert!(!store.auto_unarchive_waiting_if_active(&key, &session));
}

#[test]
fn workspace_directories_are_distinct_and_scoped_to_workspace_and_host() {
    let mut store = WorkspaceStore::new();
    let work = store.create_workspace("Work", None).unwrap();
    let other = store.create_workspace("Separate", None).unwrap();
    let sessions: Vec<_> = [
        (Some("/repo/b"), false),
        (Some("/repo/a"), false),
        (Some("/repo/a"), false),
        (Some("   "), false),
        (None, false),
        (Some("/archived"), true),
        (Some("/other-workspace"), false),
        (Some("/other-host"), false),
    ]
    .into_iter()
    .enumerate()
    .map(|(i, (cwd, archived))| SessionSummary {
        id: i.to_string(),
        cwd: cwd.map(str::to_string),
        archived,
        ..Default::default()
    })
    .collect();
    let inputs: Vec<_> = sessions
        .iter()
        .enumerate()
        .map(|(i, session)| make_input(if i == 7 { "remote" } else { "local" }, session))
        .collect();
    for (i, input) in inputs.iter().enumerate() {
        store.assign_session_workspace(&input.key, if i == 6 { &other.id } else { &work.id });
    }
    assert_eq!(
        store.workspace_directories(&work.id, "local", &inputs),
        ["/repo/a", "/repo/b"]
    );
    assert_eq!(
        store.workspace_directories(&work.id, "remote", &inputs),
        ["/other-host"]
    );
    assert!(
        store
            .workspace_directories(&work.id, "offline", &inputs)
            .is_empty()
    );
    store.favorites.insert(inputs[0].key.clone());
    assert_eq!(
        store.workspace_directories(STARRED_WORKSPACE_ID, "local", &inputs),
        ["/repo/b"]
    );
    assert_eq!(
        store.workspace_directories(ALL_WORKSPACES_ID, "local", &inputs),
        ["/other-workspace", "/repo/a", "/repo/b"]
    );
}

#[test]
fn workspace_directories_exclude_only_confirmed_missing_directories() {
    let mut store = WorkspaceStore::new();
    let sessions: Vec<_> = [
        ("/deleted", Some(false)),
        ("/exists", Some(true)),
        ("/unknown", None),
    ]
    .into_iter()
    .map(|(cwd, cwd_exists)| SessionSummary {
        id: cwd.into(),
        cwd: Some(cwd.into()),
        cwd_exists,
        ..Default::default()
    })
    .collect();
    let inputs: Vec<_> = sessions.iter().map(|s| make_input("remote", s)).collect();
    for input in &inputs {
        store.favorites.insert(input.key.clone());
    }
    for workspace in [
        ALL_WORKSPACES_ID,
        STARRED_WORKSPACE_ID,
        &store.default_workspace_id(),
    ] {
        assert_eq!(
            store.workspace_directories(workspace, "remote", &inputs),
            ["/exists", "/unknown"]
        );
    }
}

#[test]
fn workspace_directories_with_zero_or_one_distinct_directory() {
    let store = WorkspaceStore::new();
    let workspace = store.default_workspace_id();
    assert!(
        store
            .workspace_directories(&workspace, "local", &[])
            .is_empty()
    );
    let session = SessionSummary {
        cwd: Some("/repo/only".into()),
        ..Default::default()
    };
    let inputs = [make_input("local", &session), make_input("local", &session)];
    assert_eq!(
        store.workspace_directories(&workspace, "local", &inputs),
        ["/repo/only"]
    );
}

#[test]
fn reminders_and_snooze_persist_atomically() {
    let dir = unique_temp_dir("reminders");
    let path = dir.join("workspaces.json");
    let mut store = WorkspaceStore::new();
    let ws = store.create_workspace("Quiet", None).unwrap();
    store.set_reminder("h:claude:s1", Some(5_000), Some("  Follow up  "));
    store.snooze_workspace(&ws.id, 9_000, 1_000).unwrap();
    store.save_to(&path).unwrap();
    store.save_to(&path).unwrap();
    let loaded = WorkspaceStore::load_from(&path).unwrap();
    assert_eq!(loaded, store);
    assert_eq!(
        loaded.session_meta["h:claude:s1"].reminder_note.as_deref(),
        Some("Follow up")
    );
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn last_workspace_snooze_survives_restart() {
    let dir = unique_temp_dir("last-snooze");
    let path = dir.join("workspaces.json");
    let mut store = WorkspaceStore::new();
    store
        .snooze_workspace(DEFAULT_WORKSPACE_ID, 9_000, 1_000)
        .unwrap();
    store.save_to(&path).unwrap();
    let mut loaded = WorkspaceStore::load_from(&path).unwrap();
    assert!(loaded.visible_workspaces(1_000).is_empty());
    assert_eq!(loaded.snoozed_workspaces(1_000).len(), 1);
    loaded.wake_due_workspaces(9_000);
    assert_eq!(loaded.visible_workspaces(9_000).len(), 1);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn reminder_resurfaces_above_newer_rows_until_dismissed() {
    let mut store = WorkspaceStore::new();
    let sessions = [
        make_session("old", AgentKind::Claude, "Follow up", 100, false, false),
        make_session("new", AgentKind::Claude, "Newer", 9_000, true, false),
    ];
    let inputs: Vec<_> = sessions.iter().map(|s| make_input("h", s)).collect();
    let key = &inputs[0].key;
    store.archive_to_waiting_session(key, 500, &sessions[0]);
    store.set_reminder(key, Some(1_000), Some("Ask again"));
    assert_eq!(
        store
            .query_inbox(&InboxSplit::All, &inputs, false, "", 999)
            .len(),
        1
    );
    let rows = store.query_inbox(&InboxSplit::All, &inputs, true, "", 1_000);
    assert_eq!(rows[0].session_key, *key);
    assert!(rows[0].due_reminder);
    assert_eq!(rows[0].reminder_note.as_deref(), Some("Ask again"));
    assert_eq!(store.inbox_unread_count_at(&inputs, 1_000), 2);
    assert_eq!(store.next_deadline_ms(1_000), None);
    store.set_reminder(key, None, None);
    assert!(!store.reminder_due(key, 1_000));
    assert_eq!(store.session_meta[key].reminder_note, None);
    assert_eq!(
        store
            .query_inbox(&InboxSplit::All, &inputs, false, "", 1_000)
            .len(),
        1
    );
}

#[test]
fn snooze_quiets_even_due_reminders_and_wakes_at_deadline() {
    let mut store = WorkspaceStore::new();
    let ws = store.create_workspace("Quiet", None).unwrap();
    let session = make_session("s", AgentKind::Claude, "Reminder", 100, true, false);
    let inputs = [make_input("h", &session)];
    let key = &inputs[0].key;
    store.assign_session_workspace(key, &ws.id);
    store.set_reminder(key, Some(2_000), None);
    store.snooze_workspace(&ws.id, 3_000, 1_000).unwrap();
    assert!(
        store
            .query_inbox(&InboxSplit::All, &inputs, false, "", 2_999)
            .is_empty()
    );
    assert_eq!(store.inbox_unread_count_at(&inputs, 2_999), 0);
    assert!(
        !store
            .visible_workspaces(2_999)
            .iter()
            .any(|w| w.id == ws.id)
    );
    assert_eq!(
        store
            .compute_sidebar_layout(ALL_WORKSPACES_ID, &inputs, false, 2_999)
            .total_visible_sessions(),
        0
    );
    assert_eq!(
        store
            .compute_sidebar_layout(ALL_WORKSPACES_ID, &inputs, false, 3_000)
            .total_visible_sessions(),
        1
    );
    assert_eq!(
        store
            .query_inbox(&InboxSplit::All, &inputs, false, "", 3_000)
            .len(),
        1
    );
    store.wake_due_workspaces(3_000);
    assert!(!store.workspace_by_id(&ws.id).unwrap().snoozed);
    store.snooze_workspace(&ws.id, 4_000, 3_000).unwrap();
    store.wake_workspace(&ws.id);
    assert_eq!(
        store
            .query_inbox(&InboxSplit::All, &inputs, false, "", 3_000)
            .len(),
        1
    );
}

#[test]
fn next_timer_deadline_ignores_due_reminders_and_tracks_edits() {
    let mut store = WorkspaceStore::new();
    assert_eq!(store.next_deadline_ms(1_000), None);
    let ws = store.create_workspace("Quiet", None).unwrap();
    store.set_reminder("a", Some(2_000), None);
    store.set_reminder("b", Some(3_000), None);
    store.snooze_workspace(&ws.id, 4_000, 1_000).unwrap();
    assert_eq!(store.next_deadline_ms(1_000), Some(2_000));
    assert_eq!(store.next_deadline_ms(2_000), Some(3_000));
    store.set_reminder("b", Some(1_500), None);
    assert_eq!(store.next_deadline_ms(1_000), Some(1_500));
    store.set_reminder("b", None, None);
    assert_eq!(store.next_deadline_ms(3_000), Some(4_000));
    // An overdue snooze still needs its persisted flags cleared at startup.
    assert_eq!(store.next_deadline_ms(5_000), Some(4_000));
    store.wake_due_workspaces(5_000);
    assert_eq!(store.next_deadline_ms(5_000), None);
}

#[test]
fn all_workspaces_is_flat_recent_order_including_pinned_grouped_and_muted() {
    let mut store = WorkspaceStore::new();
    let work = store.create_workspace("Work", None).unwrap();
    let first = make_session("first", AgentKind::Claude, "First", 10, false, false);
    let second = make_session("second", AgentKind::Codex, "Second", 20, false, false);
    let archived = SessionSummary {
        archived: true,
        ..make_session("archived", AgentKind::Claude, "Archived", 30, false, false)
    };
    let inputs = vec![
        make_input("host", &first),
        make_input("host", &second),
        make_input("host", &archived),
    ];
    store.assign_session_workspace(&inputs[0].key, &work.id);
    store.toggle_pin(&inputs[0].key, false);
    store.toggle_favorite(&inputs[0].key);
    store.toggle_group_for_session(&inputs[1].key, DEFAULT_WORKSPACE_ID, None, "Group");
    store.session_meta.get_mut(&inputs[1].key).unwrap().muted = true;
    store.recents_section_collapsed = true;
    store.auto_sort = false;
    let all = store.compute_sidebar_layout(ALL_WORKSPACES_ID, &inputs, false, 100);
    assert!(all.pinned_tiles.is_empty());
    assert!(all.groups.is_empty());
    assert_eq!(
        all.recents
            .iter()
            .map(|r| r.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["second", "first"]
    );
    assert_eq!(
        all.ctrl_digit_order,
        vec![inputs[1].key.clone(), inputs[0].key.clone()]
    );
    let favorites = store.compute_sidebar_layout_with_options(
        ALL_WORKSPACES_ID,
        WorkspaceView::Favorites,
        &inputs,
        false,
        None,
        100,
    );
    assert_eq!(
        favorites
            .recents
            .iter()
            .map(|r| r.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["first"]
    );
    assert_eq!(
        store.cycle_workspace_id(ALL_WORKSPACES_ID, 1, 100),
        STARRED_WORKSPACE_ID
    );
    assert_eq!(
        store.cycle_workspace_id(STARRED_WORKSPACE_ID, -1, 100),
        ALL_WORKSPACES_ID
    );
}

#[test]
fn full_width_is_session_scoped_and_defaults_false_for_older_stores() {
    let dir = unique_temp_dir("full-width");
    let path = dir.join("workspace_store.json");
    let mut store = WorkspaceStore::new();
    store
        .session_meta
        .entry("host:shell:btop".into())
        .or_default()
        .full_width = true;
    store
        .session_meta
        .entry("host:claude:normal".into())
        .or_default();
    store.save_to(&path).unwrap();
    let loaded = WorkspaceStore::load_from(&path).unwrap();
    assert!(loaded.session_meta["host:shell:btop"].full_width);
    assert!(!loaded.session_meta["host:claude:normal"].full_width);
    assert!(
        !serde_json::from_str::<SessionMeta>("{}")
            .unwrap()
            .full_width
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn atomic_persistence_roundtrip_and_no_stray_tmp() {
    let dir = unique_temp_dir("atomic");
    let ws_path = dir.join("workspaces.json");
    let stats_path = dir.join("palette-stats.json");

    let mut store = WorkspaceStore::new();
    let work = store
        .create_workspace("Work", Some("#60A5FA".into()))
        .unwrap();
    let key = canonical_session_key("vps1", AgentKind::Claude, "s1");
    store.assign_session_workspace(&key, &work.id);
    store.toggle_favorite(&key);
    store.apply_session_rename(&key, "Custom Session");
    store.save_to(&ws_path).unwrap();
    // Overwrite existing file atomically
    store.save_to(&ws_path).unwrap();

    let loaded = WorkspaceStore::load_from(&ws_path).unwrap();
    assert_eq!(loaded, store);

    let mut stats = PaletteStats::default();
    stats.record_command_use("pal-cmd-inbox", 1_000_000);
    stats.record_command_use("pal-cmd-inbox", 2_000_000);
    stats.save_to(&stats_path).unwrap();
    let loaded_stats = PaletteStats::load_from(&stats_path).unwrap();
    assert_eq!(loaded_stats, stats);
    assert_eq!(loaded_stats.usage_for("pal-cmd-inbox").count, 2);

    // Zero stray .tmp files
    let stray_tmps: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(
        stray_tmps.is_empty(),
        "found stray tmp files: {stray_tmps:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn corrupt_or_missing_json_falls_back_gracefully() {
    let dir = unique_temp_dir("corrupt");

    let missing = WorkspaceStore::load_or_default(&dir.join("missing.json"));
    assert_eq!(missing.workspaces.len(), 1);
    assert_eq!(missing.workspaces[0].name, DEFAULT_WORKSPACE_NAME);

    for (name, payload) in [
        ("empty.json", ""),
        ("truncated.json", r#"{"workspaces":["#),
        ("wrong_type.json", "[]"),
    ] {
        let p = dir.join(name);
        std::fs::write(&p, payload).unwrap();
        let ws = WorkspaceStore::load_or_default(&p);
        assert_eq!(ws.workspaces.len(), 1, "case {name}");
        assert_eq!(ws.workspaces[0].name, DEFAULT_WORKSPACE_NAME);
        assert!(ws.auto_sort);

        let ps = PaletteStats::load_or_default(&p);
        assert!(ps.usage.is_empty(), "stats case {name}");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn slice1_minimal_workspaces_json_backward_compat() {
    let legacy = r#"{"workspaces":[{"id":"w1","name":"Work"}],"favorites":["h:claude:s1"]}"#;
    let parsed: WorkspaceStore = serde_json::from_str(legacy).unwrap();
    assert_eq!(parsed.workspaces.len(), 1);
    assert_eq!(parsed.workspaces[0].id, "w1");
    assert!(parsed.auto_sort);
    assert!(parsed.inbox_settings.frequent);
    assert!(parsed.inbox_settings.all);
    assert!(parsed.inbox_settings.favorites);
    assert!(parsed.inbox_settings.frequent_first);
    assert!(parsed.favorites.contains("h:claude:s1"));
}

#[test]
fn canonical_session_key_normalization() {
    assert_eq!(
        canonical_session_key("  VPS:1 ", AgentKind::Claude, " abc-123 "),
        "vps_1:claude:abc-123"
    );
    assert_eq!(
        canonical_session_key("", AgentKind::Codex, "s9"),
        "local:codex:s9"
    );
    assert_eq!(
        canonical_session_key_str("HostA", "  ", "s1"),
        "hosta:claude:s1"
    );
    assert_eq!(
        parse_session_key("vps_1:claude:abc:extra"),
        Some(("vps_1", "claude", "abc:extra"))
    );
    assert_eq!(parse_session_key("bad"), None);
    assert_eq!(parse_session_key(":claude:s1"), None);
}

#[test]
fn reconcile_live_keys_restores_imported_metadata_and_is_idempotent() {
    let sid = "c0a80101-1234-4567-89ab-0123456789ab";
    for host in ["vps0", "remote-1"] {
        let mut store = WorkspaceStore::new();
        let ws = store.create_workspace("Work", None).unwrap();
        let imported = format!("{host}:claude:{sid}");
        let live = format!("vps hetzner:codex:{sid}");
        store.session_meta.insert(
            imported.clone(),
            SessionMeta {
                workspace_id: Some(ws.id.clone()),
                pinned: true,
                starred_pinned: true,
                muted: true,
                ..Default::default()
            },
        );
        store.favorites.insert(imported.clone());
        store.inbox_excluded.insert(imported.clone());
        store
            .apply_session_rename(&imported, "Imported title")
            .unwrap();
        store.record_session_view(&live, 123);

        assert_eq!(store.reconcile_live_session_keys(&[live.clone()]), 1);
        let meta = &store.session_meta[&live];
        assert_eq!(meta.workspace_id.as_deref(), Some(ws.id.as_str()));
        assert!(meta.pinned && meta.starred_pinned && meta.muted);
        assert_eq!(meta.last_used_ms, 123);
        assert!(store.is_favorite(&live));
        assert!(store.inbox_excluded.contains(&live));
        assert_eq!(store.effective_title(&live, "fallback"), "Imported title");
        let before = store.clone();
        assert_eq!(store.reconcile_live_session_keys(&[live.clone(), live]), 0);
        assert_eq!(store, before);
    }
}

#[test]
fn reconcile_live_keys_preserves_explicit_workspace_and_title() {
    let sid = "c0a80101-1234-4567-89ab-0123456789ab";
    let mut store = WorkspaceStore::new();
    let ws = store.create_workspace("Work", None).unwrap();
    let imported = format!("vps0:claude:{sid}");
    let live = format!("vps hetzner:codex:{sid}");
    store.assign_session_workspace(&imported, &ws.id);
    store.session_meta.get_mut(&imported).unwrap().pinned = true;
    store.assign_session_workspace(&live, DEFAULT_WORKSPACE_ID);
    store.apply_session_rename(&imported, "Imported").unwrap();
    store.apply_session_rename(&live, "Keep mine").unwrap();
    store.favorites.insert(imported);
    assert_eq!(store.reconcile_live_session_keys(&[live.clone()]), 1);
    assert_eq!(
        store.session_meta[&live].workspace_id.as_deref(),
        Some(DEFAULT_WORKSPACE_ID)
    );
    assert!(!store.session_meta[&live].pinned);
    assert_eq!(store.effective_title(&live, "fallback"), "Keep mine");
    assert!(store.is_favorite(&live));
    assert_eq!(store.reconcile_live_session_keys(&[live]), 0);
}

#[test]
fn reconcile_live_keys_ignores_short_ids_and_missing_workspaces() {
    let mut store = WorkspaceStore::new();
    store.assign_session_workspace("old:claude:short", DEFAULT_WORKSPACE_ID);
    store.favorites.insert("old:claude:short".into());
    store.session_meta.insert(
        "old:claude:long-enough-id".into(),
        SessionMeta {
            workspace_id: Some("deleted".into()),
            ..Default::default()
        },
    );
    let before = store.clone();
    assert_eq!(
        store.reconcile_live_session_keys(&[
            "live:codex:short".into(),
            "live:codex:long-enough-id".into(),
            "bad".into(),
        ]),
        0
    );
    assert_eq!(store, before);
}

#[test]
fn reconcile_live_keys_prefers_agent_and_rejects_ambiguous_sources() {
    let mut store = WorkspaceStore::new();
    let ws = store.create_workspace("Work", None).unwrap();
    store.assign_session_workspace("old:claude:long-enough-id", DEFAULT_WORKSPACE_ID);
    store.assign_session_workspace("old:codex:long-enough-id", &ws.id);
    let live = "live:codex:long-enough-id".to_string();
    assert_eq!(store.reconcile_live_session_keys(&[live.clone()]), 1);
    assert_eq!(
        store.session_meta[&live].workspace_id.as_deref(),
        Some(ws.id.as_str())
    );
    let before = store.clone();
    assert_eq!(
        store.reconcile_live_session_keys(&["live:pi:long-enough-id".into()]),
        0
    );
    assert_eq!(store, before);
}

#[test]
fn reconcile_live_keys_supports_favorites_without_metadata() {
    let mut store = WorkspaceStore::new();
    store.favorites.insert("old:claude:long-enough-id".into());
    let live = "live:codex:long-enough-id".to_string();
    assert_eq!(
        store.reconcile_live_session_keys(&[live.clone(), live.clone()]),
        1
    );
    assert!(store.is_favorite(&live));
    assert_eq!(store.reconcile_live_session_keys(&[live]), 0);
}

fn server_summary(id: &str, workspace: Option<&str>) -> SessionSummary {
    SessionSummary {
        id: id.to_string(),
        agent: AgentKind::Claude,
        title: format!("Live {id}"),
        source_path: format!("/tmp/{id}.jsonl"),
        updated_at_ms: 5,
        workspace: workspace.map(str::to_string),
        ..Default::default()
    }
}

#[test]
fn server_workspace_hint_creates_and_reuses_the_named_workspace() {
    let mut store = WorkspaceStore::new();
    let yeo = store.apply_server_workspace_hints(&[("vps:claude:sid-1".into(), "Yeo UX")]);
    assert_eq!(yeo, 1);
    assert_eq!(store.workspaces.len(), 2); // General + Yeo UX
    let created = store
        .workspaces
        .iter()
        .find(|w| w.name == "Yeo UX")
        .unwrap()
        .id
        .clone();
    assert_eq!(
        store.session_meta["vps:claude:sid-1"]
            .workspace_id
            .as_deref(),
        Some(created.as_str())
    );

    // A second session in the same named workspace reuses it and keeps positions stable.
    assert_eq!(
        store.apply_server_workspace_hints(&[
            ("vps:claude:sid-2".into(), "  yeo ux  "),
            ("vps:codex:sid-3".into(), "HT Product"),
        ]),
        2
    );
    assert_eq!(
        store.session_meta["vps:claude:sid-2"]
            .workspace_id
            .as_deref(),
        Some(created.as_str())
    );
    let names: Vec<&str> = store.workspaces.iter().map(|w| w.name.as_str()).collect();
    assert_eq!(names, vec!["General", "Yeo UX", "HT Product"]);
    // Idempotent on refresh.
    assert_eq!(
        store.apply_server_workspace_hints(&[("vps:claude:sid-1".into(), "Yeo UX")]),
        0
    );
}

#[test]
fn manual_workspace_move_beats_the_server_hint() {
    let mut store = WorkspaceStore::new();
    let mine = store.create_workspace("Mine", None).unwrap();
    store.assign_session_workspace("vps:claude:sid-1", &mine.id);
    assert_eq!(
        store.apply_server_workspace_hints(&[("vps:claude:sid-1".into(), "Yeo UX")]),
        0
    );
    assert_eq!(
        store.session_meta["vps:claude:sid-1"]
            .workspace_id
            .as_deref(),
        Some(mine.id.as_str())
    );
    assert!(store.workspaces.iter().all(|w| w.name != "Yeo UX"));
}

#[test]
fn server_hint_rejects_blank_names_and_recovers_from_dangling_ones() {
    let mut store = WorkspaceStore::new();
    assert_eq!(
        store.apply_server_workspace_hints(&[("a:claude:sid-2".into(), "   ")]),
        0
    );

    // `Other` is a real workspace in the legacy app, so the server name recreates it
    // rather than silently dropping those chats into the catch-all bucket.
    assert_eq!(
        store.apply_server_workspace_hints(&[("a:claude:sid-1".into(), "Other")]),
        1
    );
    let other = store
        .workspaces
        .iter()
        .find(|w| w.name == "Other")
        .unwrap()
        .id
        .clone();
    assert_eq!(
        store.session_meta["a:claude:sid-1"].workspace_id.as_deref(),
        Some(other.as_str())
    );

    // A workspace the user deleted must not keep swallowing the session: a stale
    // `workspace_id` is not an explicit choice, so the hint applies again.
    store.session_meta.insert(
        "a:claude:sid-3".into(),
        SessionMeta {
            workspace_id: Some("deleted-workspace".into()),
            ..Default::default()
        },
    );
    assert_eq!(
        store.apply_server_workspace_hints(&[("a:claude:sid-3".into(), "Yeo UX")]),
        1
    );
    let yeo = store
        .workspaces
        .iter()
        .find(|w| w.name == "Yeo UX")
        .unwrap()
        .id
        .clone();
    assert_eq!(
        store.session_meta["a:claude:sid-3"].workspace_id.as_deref(),
        Some(yeo.as_str())
    );
}

#[test]
fn reconcile_live_sessions_combines_legacy_import_with_server_hints() {
    let sid_imported = "c0a80101-1234-4567-89ab-0123456789ab";
    let sid_server_only = "d1b90202-2345-5678-9abc-1234567890cd";
    let mut store = WorkspaceStore::new();
    let legacy = store.create_workspace("Legacy", None).unwrap();
    store.assign_session_workspace(&format!("vps0:claude:{sid_imported}"), &legacy.id);

    let imported_live = server_summary(sid_imported, Some("Server Says"));
    let server_only = server_summary(sid_server_only, Some("Yeo UX"));
    assert_eq!(
        store.reconcile_live_sessions([
            ("VPS Hetzner", &imported_live),
            ("VPS Hetzner", &server_only),
        ]),
        2
    );

    let imported_key = canonical_session_key("VPS Hetzner", AgentKind::Claude, sid_imported);
    // The hand-curated legacy assignment wins over the server's name.
    assert_eq!(
        store.session_meta[&imported_key].workspace_id.as_deref(),
        Some(legacy.id.as_str())
    );
    let server_key = canonical_session_key("VPS Hetzner", AgentKind::Claude, sid_server_only);
    let yeo = store
        .workspaces
        .iter()
        .find(|w| w.name == "Yeo UX")
        .unwrap()
        .id
        .clone();
    assert_eq!(
        store.session_meta[&server_key].workspace_id.as_deref(),
        Some(yeo.as_str())
    );
    assert!(store.workspaces.iter().all(|w| w.name != "Server Says"));

    // Stable across refreshes and across a host alias change.
    assert_eq!(
        store.reconcile_live_sessions([
            ("VPS Hetzner", &imported_live),
            ("VPS Hetzner", &server_only),
        ]),
        0
    );
}

#[test]
fn one_session_refresh_places_a_spawned_chat_without_a_full_list() {
    let mut store = WorkspaceStore::new();
    let fresh = server_summary("51fe0c0d-2abc-4def-8abc-0123456789ab", Some("Vstack"));

    // A spawn/update event carries the resolved name, so the chat lands immediately.
    assert_eq!(store.reconcile_session("VPS Hetzner", &fresh), 1);
    let key = canonical_session_key("VPS Hetzner", AgentKind::Claude, &fresh.id);
    let ws = store.workspace_by_id(store.session_meta[&key].workspace_id.as_deref().unwrap());
    assert_eq!(ws.unwrap().name, "Vstack");

    // A plain chat with no recorded workspace stays put and creates nothing.
    let unplaced = server_summary("62fe0d0d-3abc-4def-8abc-0123456789ab", None);
    assert_eq!(store.reconcile_session("VPS Hetzner", &unplaced), 0);
    assert_eq!(store.workspaces.len(), 2);
}

#[test]
fn inbox_groups_sessions_under_the_workspace_the_server_recorded() {
    let now = 1_000_000;
    let mut store = WorkspaceStore::new();
    let yeo = server_summary("sid-yeo", Some("Yeo UX"));
    let product = server_summary("sid-product", Some("HT Product"));
    store.reconcile_live_sessions([("vps", &yeo), ("vps", &product)]);

    let inputs = vec![
        SessionInput {
            key: canonical_session_key("vps", AgentKind::Claude, "sid-yeo"),
            host: "vps",
            summary: &yeo,
        },
        SessionInput {
            key: canonical_session_key("vps", AgentKind::Claude, "sid-product"),
            host: "vps",
            summary: &product,
        },
    ];

    // Split order follows workspace position: General, Yeo UX, HT Product.
    let splits = store.inbox_splits_with_sessions(&inputs, false, "", now);
    let workspace_names: Vec<String> = splits
        .iter()
        .filter_map(|split| match split {
            InboxSplit::Workspace(id) => store.workspace_by_id(id).map(|w| w.name.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(workspace_names, vec!["Yeo UX", "HT Product"]);

    let yeo_id = store
        .workspaces
        .iter()
        .find(|w| w.name == "Yeo UX")
        .unwrap()
        .id
        .clone();
    let yeo_rows = store.query_inbox(&InboxSplit::Workspace(yeo_id), &inputs, false, "", now);
    assert_eq!(yeo_rows.len(), 1);
    assert_eq!(yeo_rows[0].session_id, "sid-yeo");
    assert_eq!(yeo_rows[0].workspace_name, "Yeo UX");

    // The catch-all bucket holds neither session.
    let general_rows = store.query_inbox(
        &InboxSplit::Workspace(DEFAULT_WORKSPACE_ID.to_string()),
        &inputs,
        false,
        "",
        now,
    );
    assert!(general_rows.is_empty());
}

#[test]
fn alt_digit_mapping_starred_and_visible_slots() {
    let mut store = WorkspaceStore::new();
    let w2 = store.create_workspace("Alpha", None).unwrap();
    let w3 = store.create_workspace("Archived", None).unwrap();
    let w4 = store.create_workspace("Snoozed", None).unwrap();
    let w5 = store.create_workspace("Beta", None).unwrap();
    let now = 10_000;
    store.archive_workspace(&w3.id).unwrap();
    store.snooze_workspace(&w4.id, now + 5_000, now).unwrap();

    assert_eq!(
        store.resolve_alt_digit(1, now).as_deref(),
        Some(STARRED_WORKSPACE_ID)
    );
    assert_eq!(
        store.resolve_alt_digit(2, now).as_deref(),
        Some(DEFAULT_WORKSPACE_ID)
    );
    assert_eq!(
        store.resolve_alt_digit(3, now).as_deref(),
        Some(w2.id.as_str())
    );
    // Archived and snoozed skipped
    assert_eq!(
        store.resolve_alt_digit(4, now).as_deref(),
        Some(w5.id.as_str())
    );
    assert_eq!(store.resolve_alt_digit(5, now), None);
    assert_eq!(store.resolve_alt_digit(0, now), None);
    assert_eq!(store.resolve_alt_digit(10, now), None);
}

#[test]
fn workspace_rename_rejects_other_case_insensitively() {
    let mut store = WorkspaceStore::new();
    let ws = store.create_workspace("Alpha", None).unwrap();

    assert_eq!(
        store.create_workspace("Other", None),
        Err(WorkspaceError::ReservedName)
    );
    assert_eq!(
        store.create_workspace("  oThEr  ", None),
        Err(WorkspaceError::ReservedName)
    );

    assert_eq!(
        store.rename_workspace(&ws.id, "Other"),
        Err(WorkspaceError::ReservedName)
    );
    assert_eq!(
        store.rename_workspace(&ws.id, "  OTHER  "),
        Err(WorkspaceError::ReservedName)
    );
    assert_eq!(
        store.rename_workspace(&ws.id, "   "),
        Err(WorkspaceError::EmptyName)
    );

    // Existing "Other" workspace cannot be renamed to anything else
    store.workspaces.push(Workspace {
        id: "legacy-other".into(),
        name: "Other".into(),
        ..Default::default()
    });
    assert_eq!(
        store.rename_workspace("legacy-other", "ValidName"),
        Err(WorkspaceError::ReservedName)
    );

    // >63 Unicode char truncation
    let long_name = "🚀日Kraków".repeat(15);
    store.rename_workspace(&ws.id, &long_name).unwrap();
    let updated = store.workspace_by_id(&ws.id).unwrap();
    assert_eq!(updated.name.chars().count(), MAX_WORKSPACE_NAME_CHARS);
}

#[test]
fn workspace_delete_empty_nonempty_and_last_keeps_general() {
    let mut store = WorkspaceStore::new();
    let work = store.create_workspace("Work", None).unwrap();
    let empty = store.create_workspace("Empty", None).unwrap();

    // Starred cannot be deleted
    assert_eq!(
        store.delete_workspace(STARRED_WORKSPACE_ID, None),
        Err(WorkspaceError::CannotDeleteStarred)
    );

    // Empty deletes immediately
    let out = store.delete_workspace(&empty.id, None).unwrap();
    assert_eq!(out.moved_sessions, 0);
    assert!(!out.created_default_general);
    assert!(store.workspace_by_id(&empty.id).is_none());

    // Non-empty requires target
    let s1 = canonical_session_key("h", AgentKind::Claude, "s1");
    store.assign_session_workspace(&s1, &work.id);
    let grp = store.create_group(&work.id, "Backend", None);
    store.session_meta.get_mut(&s1).unwrap().group_id = Some(grp.id.clone());

    assert_eq!(
        store.delete_workspace(&work.id, None),
        Err(WorkspaceError::NonEmptyRequiresTarget { session_count: 1 })
    );
    // Self / starred / archived targets rejected
    assert!(matches!(
        store.delete_workspace(&work.id, Some(&work.id)),
        Err(WorkspaceError::InvalidMoveTarget(_))
    ));
    assert!(matches!(
        store.delete_workspace(&work.id, Some(STARRED_WORKSPACE_ID)),
        Err(WorkspaceError::InvalidMoveTarget(_))
    ));

    let archived = store.create_workspace("Arch", None).unwrap();
    store.archive_workspace(&archived.id).unwrap();
    assert!(matches!(
        store.delete_workspace(&work.id, Some(&archived.id)),
        Err(WorkspaceError::InvalidMoveTarget(_))
    ));

    // Valid target moves sessions and groups
    let out = store
        .delete_workspace(&work.id, Some(DEFAULT_WORKSPACE_ID))
        .unwrap();
    assert_eq!(out.moved_sessions, 1);
    assert_eq!(out.moved_to.as_deref(), Some(DEFAULT_WORKSPACE_ID));
    assert_eq!(
        store.session_meta[&s1].workspace_id.as_deref(),
        Some(DEFAULT_WORKSPACE_ID)
    );
    assert_eq!(
        store
            .groups
            .iter()
            .find(|g| g.id == grp.id)
            .unwrap()
            .workspace_id,
        DEFAULT_WORKSPACE_ID
    );

    // Deleting last empty workspace recreates default General
    let mut solo = WorkspaceStore::new();
    let last_out = solo.delete_workspace(DEFAULT_WORKSPACE_ID, None).unwrap();
    assert!(last_out.created_default_general);
    assert_eq!(solo.workspaces.len(), 1);
    assert_eq!(solo.workspaces[0].id, DEFAULT_WORKSPACE_ID);
    assert_eq!(solo.workspaces[0].name, DEFAULT_WORKSPACE_NAME);
}

#[test]
fn workspace_snooze_expiry_exact_boundary_and_wake() {
    let mut store = WorkspaceStore::new();
    let w2 = store.create_workspace("Focus", None).unwrap();
    let now = 50_000;
    let until = 60_000;

    assert!(store.can_snooze_workspace(&w2.id, now));
    let next = store.snooze_workspace(&w2.id, until, now).unwrap();
    assert_eq!(next.as_deref(), Some(DEFAULT_WORKSPACE_ID));

    // At until - 1: still snoozed
    assert!(
        store
            .workspace_by_id(&w2.id)
            .unwrap()
            .is_snoozed_at(until - 1)
    );
    assert_eq!(store.resolve_alt_digit(3, until - 1), None);

    // At exact boundary now == until: awake!
    assert!(!store.workspace_by_id(&w2.id).unwrap().is_snoozed_at(until));
    assert_eq!(
        store.resolve_alt_digit(3, until).as_deref(),
        Some(w2.id.as_str())
    );

    let woken = store.wake_due_workspaces(until);
    assert_eq!(woken, vec![w2.id.clone()]);
    assert!(!store.workspace_by_id(&w2.id).unwrap().snoozed);
    assert_eq!(
        store.workspace_by_id(&w2.id).unwrap().snoozed_until_ms,
        None
    );

    // Manual early wake
    store
        .snooze_workspace(&w2.id, until + 10_000, until)
        .unwrap();
    assert!(store.wake_workspace(&w2.id));
    assert!(!store.workspace_by_id(&w2.id).unwrap().is_snoozed_at(until));
}

#[test]
fn favorite_toggle_clears_starred_group_and_prunes_empty() {
    let mut store = WorkspaceStore::new();
    let s1 = canonical_session_key("h", AgentKind::Claude, "s1");
    let s2 = canonical_session_key("h", AgentKind::Claude, "s2");

    // Normal workspace group must stay untouched
    let norm_gid = store
        .toggle_group_for_session(&s1, DEFAULT_WORKSPACE_ID, None, "NormalGrp")
        .unwrap();

    assert!(store.toggle_favorite(&s1));
    assert!(store.toggle_favorite(&s2));
    let star_gid_1 = store
        .toggle_group_for_session(&s1, STARRED_WORKSPACE_ID, None, "StarSolo")
        .unwrap();
    let star_gid_2 = store
        .toggle_group_for_session(&s2, STARRED_WORKSPACE_ID, None, "StarKeep")
        .unwrap();

    // Unfavorite s1 -> clears starred_group_id and prunes StarSolo only
    assert!(!store.toggle_favorite(&s1));
    assert_eq!(store.session_meta[&s1].starred_group_id, None);
    assert!(!store.groups.iter().any(|g| g.id == star_gid_1));
    assert!(store.groups.iter().any(|g| g.id == star_gid_2));
    assert!(store.groups.iter().any(|g| g.id == norm_gid));
}

#[test]
fn sidebar_sections_use_message_activity_and_restore_unpinned_group_members() {
    for workspace_id in [DEFAULT_WORKSPACE_ID, STARRED_WORKSPACE_ID] {
        let mut store = WorkspaceStore::new();
        let sessions = [
            make_session(
                "chief",
                AgentKind::Claude,
                "Chief of Staff",
                100,
                false,
                false,
            ),
            make_session(
                "supervisor",
                AgentKind::Claude,
                "Supervisor",
                300,
                false,
                false,
            ),
            SessionSummary {
                last_assistant_ms: Some(500),
                ..make_session("new", AgentKind::Claude, "Newest reply", 200, false, false)
            },
        ];
        let inputs: Vec<_> = sessions.iter().map(|s| make_input("h", s)).collect();
        let keys: Vec<_> = inputs.iter().map(|s| s.key.clone()).collect();
        let group = store.create_group(workspace_id, "Marketing / Help / Docs", None);
        for key in &keys {
            store.favorites.insert(key.clone());
            store.toggle_group_for_session(key, workspace_id, Some(&group.id), "");
        }
        store.toggle_session_muted(&keys[2]);
        let in_starred = workspace_id == STARRED_WORKSPACE_ID;
        assert!(store.toggle_pin(&keys[0], in_starred));
        let pinned = store.compute_sidebar_layout(workspace_id, &inputs, false, 1000);
        assert_eq!(
            pinned
                .pinned_tiles
                .iter()
                .map(|r| &r.session_key)
                .collect::<Vec<_>>(),
            vec![&keys[0]]
        );
        assert_eq!(pinned.groups[0].group.name, "Marketing / Help / Docs");
        assert_eq!(
            pinned.groups[0]
                .items
                .iter()
                .map(|r| &r.session_key)
                .collect::<Vec<_>>(),
            vec![&keys[2], &keys[1]]
        );
        assert_eq!(
            pinned.flat_display_order,
            vec![keys[0].clone(), keys[2].clone(), keys[1].clone()]
        );
        assert!(!store.toggle_pin(&keys[0], in_starred));
        store.record_session_view(&keys[0], 9000);
        let store: WorkspaceStore =
            serde_json::from_str(&serde_json::to_string(&store).unwrap()).unwrap();
        assert!(!store.is_pinned(&keys[0], in_starred));
        let unpinned = store.compute_sidebar_layout(workspace_id, &inputs, false, 10000);
        assert!(unpinned.pinned_tiles.is_empty());
        assert_eq!(
            unpinned.groups[0]
                .items
                .iter()
                .map(|r| &r.session_key)
                .collect::<Vec<_>>(),
            vec![&keys[2], &keys[1], &keys[0]]
        );
    }
}

#[test]
fn ungrouped_pin_choices_and_activity_order_survive_save_and_reload() {
    let dir = unique_temp_dir("sidebar-pins");
    let path = dir.join("workspaces.json");
    let sessions = [
        make_session(
            "chief",
            AgentKind::Claude,
            "Chief of Staff",
            100,
            false,
            false,
        ),
        make_session("new", AgentKind::Claude, "Newest", 500, false, false),
        make_session(
            "supervisor",
            AgentKind::Claude,
            "Supervisor",
            300,
            false,
            false,
        ),
    ];
    let inputs: Vec<_> = sessions.iter().map(|s| make_input("h", s)).collect();
    for workspace_id in [DEFAULT_WORKSPACE_ID, STARRED_WORKSPACE_ID] {
        let mut store = WorkspaceStore::new();
        for input in &inputs {
            store.favorites.insert(input.key.clone());
        }
        let in_starred = workspace_id == STARRED_WORKSPACE_ID;
        for index in [0, 2] {
            assert!(store.toggle_pin(&inputs[index].key, in_starred));
            store.save_to(&path).unwrap();
            store = WorkspaceStore::load_from(&path).unwrap();
            let pinned = store.compute_sidebar_layout(workspace_id, &inputs, false, 1000);
            assert_eq!(pinned.pinned_tiles[0].session_key, inputs[index].key);
            assert!(!store.toggle_pin(&inputs[index].key, in_starred));
            store.save_to(&path).unwrap();
            store = WorkspaceStore::load_from(&path).unwrap();
            let unpinned = store.compute_sidebar_layout(workspace_id, &inputs, false, 1000);
            assert!(unpinned.pinned_tiles.is_empty());
            assert_eq!(
                unpinned
                    .recents
                    .iter()
                    .map(|r| r.session_id.as_str())
                    .collect::<Vec<_>>(),
                vec!["new", "supervisor", "chief"]
            );
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn sidebar_blank_group_names_have_a_visible_fallback() {
    let mut store = WorkspaceStore::new();
    let session = make_session("one", AgentKind::Claude, "One", 100, false, false);
    let inputs = [make_input("h", &session)];
    let group = store.create_group(DEFAULT_WORKSPACE_ID, "Group", None);
    store.toggle_group_for_session(&inputs[0].key, DEFAULT_WORKSPACE_ID, Some(&group.id), "");
    store.groups[0].name = "  \t ".into();
    let layout = store.compute_sidebar_layout(DEFAULT_WORKSPACE_ID, &inputs, false, 1000);
    assert_eq!(layout.groups[0].group.name, "Group");
}

#[test]
fn reconciliation_cannot_restore_explicitly_unpinned_sessions() {
    let mut store = WorkspaceStore::new();
    let sid = "chief-of-staff-session";
    let imported = format!("legacy:claude:{sid}");
    let live = format!("local:claude:{sid}");
    store.session_meta.insert(
        imported,
        SessionMeta {
            pinned: true,
            starred_pinned: true,
            ..Default::default()
        },
    );
    store.reconcile_live_session_keys(&[live.clone()]);
    assert!(store.is_pinned(&live, false));
    assert!(store.is_pinned(&live, true));
    assert!(!store.toggle_pin(&live, false));
    assert!(!store.toggle_pin(&live, true));
    let mut store: WorkspaceStore =
        serde_json::from_str(&serde_json::to_string(&store).unwrap()).unwrap();
    store.reconcile_live_session_keys(&[live.clone()]);
    assert!(!store.is_pinned(&live, false));
    assert!(!store.is_pinned(&live, true));
}

#[test]
fn sidebar_pins_precede_groups_and_recents_follow_message_activity() {
    let mut store = WorkspaceStore::new();
    let s_grp_rec = make_session("grec", AgentKind::Claude, "G Recent", 500, false, false);
    let s_grp_pin = make_session("gpin", AgentKind::Claude, "G Pinned", 100, false, false);
    let s_pin_a = make_session("pina", AgentKind::Claude, "Pin A", 50, false, false);
    let s_pin_b = make_session("pinb", AgentKind::Claude, "Pin B", 900, false, false);
    let s_rec_muted = make_session("rmut", AgentKind::Claude, "Muted New", 999, false, false);
    let s_rec_old = make_session("rold", AgentKind::Claude, "Active Old", 200, false, false);
    let s_rec_new = make_session("rnew", AgentKind::Claude, "Active New", 400, false, false);

    let all = [
        s_grp_rec,
        s_grp_pin,
        s_pin_a,
        s_pin_b,
        s_rec_muted,
        s_rec_old,
        s_rec_new,
    ];
    let inputs: Vec<_> = all.iter().map(|s| make_input("h", s)).collect();

    let k = |idx: usize| inputs[idx].key.clone();
    let grp = store.create_group(DEFAULT_WORKSPACE_ID, "Core", None);
    store.toggle_group_for_session(&k(0), DEFAULT_WORKSPACE_ID, Some(&grp.id), "");
    store.toggle_group_for_session(&k(1), DEFAULT_WORKSPACE_ID, Some(&grp.id), "");
    store.toggle_pin(&k(1), false); // A grouped pin moves to the shared Pinned area.

    // Ungrouped pinned in manual order (pin_b=0, pin_a=1)
    store.toggle_pin(&k(2), false);
    store.toggle_pin(&k(3), false);
    store.session_meta.get_mut(&k(3)).unwrap().manual_order = Some(0);
    store.session_meta.get_mut(&k(2)).unwrap().manual_order = Some(1);

    // Muting does not override newest-message ordering.
    store.toggle_session_muted(&k(4));

    let layout = store.compute_sidebar_layout(DEFAULT_WORKSPACE_ID, &inputs, false, 1000);
    assert_eq!(layout.groups.len(), 1);
    assert_eq!(
        layout.groups[0]
            .items
            .iter()
            .map(|r| r.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["grec"]
    );
    assert_eq!(
        layout
            .pinned_tiles
            .iter()
            .map(|r| r.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["pinb", "pina", "gpin"]
    );
    assert_eq!(
        layout
            .recents
            .iter()
            .map(|r| r.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["rmut", "rnew", "rold"]
    );
    assert_eq!(
        layout.flat_display_order,
        vec![k(3), k(2), k(1), k(0), k(4), k(6), k(5)]
    );

    // Starred workspace uses independent starred_pinned & starred_group_id
    store.toggle_favorite(&k(5));
    store.toggle_favorite(&k(6));
    store.toggle_pin(&k(5), true); // pinned ONLY in Starred
    let star_layout = store.compute_sidebar_layout(STARRED_WORKSPACE_ID, &inputs, false, 1000);
    assert_eq!(star_layout.pinned_tiles.len(), 1);
    assert_eq!(star_layout.pinned_tiles[0].session_id, "rold");
    assert_eq!(star_layout.recents.len(), 1);
    assert_eq!(star_layout.recents[0].session_id, "rnew");
    // Normal workspace pin unchanged
    assert!(!store.is_pinned(&k(5), false));
}

#[test]
fn sidebar_manual_reorder_materializes_and_respects_boundaries() {
    let mut store = WorkspaceStore::new();
    let s1 = make_session("g1", AgentKind::Claude, "G1", 300, false, false);
    let s2 = make_session("g2", AgentKind::Claude, "G2", 200, false, false);
    let s3 = make_session("p1", AgentKind::Claude, "P1", 100, false, false);
    let s4 = make_session("r1", AgentKind::Claude, "R1", 500, false, false);
    let s5 = make_session("r2", AgentKind::Claude, "R2", 400, false, false);
    let all = [s1, s2, s3, s4, s5];
    let inputs: Vec<_> = all.iter().map(|s| make_input("h", s)).collect();
    let k = |i: usize| inputs[i].key.clone();

    let grp = store.create_group(DEFAULT_WORKSPACE_ID, "G", None);
    store.toggle_group_for_session(&k(0), DEFAULT_WORKSPACE_ID, Some(&grp.id), "");
    store.toggle_group_for_session(&k(1), DEFAULT_WORKSPACE_ID, Some(&grp.id), "");
    store.toggle_pin(&k(2), false);

    assert!(store.auto_sort);
    // Move g2 up inside group -> materializes order and disables auto_sort
    assert!(store.reorder_session(
        DEFAULT_WORKSPACE_ID,
        &k(1),
        ReorderDirection::Up,
        &inputs,
        1000
    ));
    assert!(!store.auto_sort);

    // Moving g2 up again clamps at top of group (cannot escape)
    assert!(!store.reorder_session(
        DEFAULT_WORKSPACE_ID,
        &k(1),
        ReorderDirection::Up,
        &inputs,
        1000
    ));
    // Moving g1 down clamps at bottom of group (cannot enter pinned/recents)
    assert!(!store.reorder_session(
        DEFAULT_WORKSPACE_ID,
        &k(0),
        ReorderDirection::Down,
        &inputs,
        1000
    ));
    // Moving solo pinned item neither up nor down crosses into groups or recents
    assert!(!store.reorder_session(
        DEFAULT_WORKSPACE_ID,
        &k(2),
        ReorderDirection::Up,
        &inputs,
        1000
    ));
    assert!(!store.reorder_session(
        DEFAULT_WORKSPACE_ID,
        &k(2),
        ReorderDirection::Down,
        &inputs,
        1000
    ));
    // Moving r2 up swaps with r1 inside recents only
    assert!(store.reorder_session(
        DEFAULT_WORKSPACE_ID,
        &k(4),
        ReorderDirection::Up,
        &inputs,
        1000
    ));
    assert!(!store.reorder_session(
        DEFAULT_WORKSPACE_ID,
        &k(4),
        ReorderDirection::Up,
        &inputs,
        1000
    ));

    let layout = store.compute_sidebar_layout(DEFAULT_WORKSPACE_ID, &inputs, false, 1000);
    assert_eq!(layout.groups[0].items[0].session_id, "g2");
    assert_eq!(layout.groups[0].items[1].session_id, "g1");
    assert_eq!(layout.recents[0].session_id, "r2");
    assert_eq!(layout.recents[1].session_id, "r1");
}

#[test]
fn ctrl_digit_resolution_pinned_first_and_unread_only() {
    let mut store = WorkspaceStore::new();
    let s_grp = make_session("g1", AgentKind::Claude, "Grouped", 900, true, false);
    let s_pin = make_session("p1", AgentKind::Claude, "Pinned", 100, true, false);
    let s_rec_read = make_session("r1", AgentKind::Claude, "Rec Read", 800, false, false);
    let s_rec_unread = make_session("r2", AgentKind::Claude, "Rec Unread", 700, true, false);
    let all = [s_grp, s_pin, s_rec_read, s_rec_unread];
    let inputs: Vec<_> = all.iter().map(|s| make_input("h", s)).collect();
    let k = |i: usize| inputs[i].key.clone();

    let grp = store.create_group(DEFAULT_WORKSPACE_ID, "Grp", None);
    store.toggle_group_for_session(&k(0), DEFAULT_WORKSPACE_ID, Some(&grp.id), "");
    store.toggle_pin(&k(1), false);

    // Open group: pinned first (1), then group (2), then recents (3, 4)
    let layout = store.compute_sidebar_layout(DEFAULT_WORKSPACE_ID, &inputs, false, 1000);
    assert_eq!(layout.resolve_ctrl_digit(1), Some(k(1).as_str()));
    assert_eq!(layout.resolve_ctrl_digit(2), Some(k(0).as_str()));
    assert_eq!(layout.resolve_ctrl_digit(3), Some(k(2).as_str()));
    assert_eq!(layout.resolve_ctrl_digit(4), Some(k(3).as_str()));

    // Collapsed group skipped in Ctrl+1..9
    store.toggle_group_collapsed(&grp.id);
    let collapsed = store.compute_sidebar_layout(DEFAULT_WORKSPACE_ID, &inputs, false, 1000);
    assert_eq!(collapsed.resolve_ctrl_digit(1), Some(k(1).as_str()));
    assert_eq!(collapsed.resolve_ctrl_digit(2), Some(k(2).as_str()));
    assert_eq!(collapsed.resolve_ctrl_digit(3), Some(k(3).as_str()));

    // unread_only skips read sessions
    let unread_layout = store.compute_sidebar_layout(DEFAULT_WORKSPACE_ID, &inputs, true, 1000);
    assert_eq!(unread_layout.resolve_ctrl_digit(1), Some(k(1).as_str()));
    assert_eq!(unread_layout.resolve_ctrl_digit(2), Some(k(3).as_str()));
    assert_eq!(unread_layout.resolve_ctrl_digit(3), None);
}

#[test]
fn sidebar_order_ignores_views_and_dwell_but_tracks_message_input() {
    let sessions: Vec<_> = (0..12)
        .map(|i| {
            make_session(
                &format!("s{i}"),
                AgentKind::Claude,
                "Chat",
                100 - i,
                true,
                false,
            )
        })
        .collect();
    let inputs: Vec<_> = sessions.iter().map(|s| make_input("h", s)).collect();
    let keys: Vec<_> = inputs.iter().map(|s| s.key.clone()).collect();
    for workspace_id in [ALL_WORKSPACES_ID, DEFAULT_WORKSPACE_ID] {
        for auto_sort in [true, false] {
            let mut store = WorkspaceStore::new();
            store.auto_sort = auto_sort;
            for (i, key) in keys.iter().enumerate() {
                store
                    .session_meta
                    .entry(key.clone())
                    .or_default()
                    .manual_order = Some(i);
            }
            for (i, key) in keys.iter().enumerate().rev() {
                store.record_session_view(key, 1000 + i as i64);
                store.record_frequent_visit(key, 2000 + i as i64);
                store.mark_session_read(key, &sessions[i]);
                let layout = store.compute_sidebar_layout(workspace_id, &inputs, false, 3000);
                assert_eq!(layout.flat_display_order, keys);
                assert_eq!(layout.ctrl_digit_order, keys);
                assert_eq!(store.recent_views[0].session_key, *key);
                assert!(!store.is_session_unread(key, &sessions[i]));
            }
            let frequent = store.compute_sidebar_layout_with_options(
                workspace_id,
                WorkspaceView::Frequent,
                &inputs,
                false,
                None,
                3000,
            );
            assert_eq!(
                frequent.flat_display_order,
                keys.iter().rev().cloned().collect::<Vec<_>>()
            );
            assert_eq!(
                store
                    .compute_sidebar_layout(workspace_id, &inputs, false, 3000)
                    .flat_display_order,
                keys
            );
            store.record_session_input(&keys[11], 4000);
            let layout = store.compute_sidebar_layout(workspace_id, &inputs, false, 4000);
            if workspace_id == ALL_WORKSPACES_ID || auto_sort {
                assert_eq!(layout.ctrl_digit_order[0], keys[11]);
            } else {
                assert_eq!(layout.ctrl_digit_order, keys);
            }
        }
    }
}

#[test]
fn sidebar_frequent_ranks_by_rolling_count_then_latest_activity() {
    let now = 20 * 24 * 60 * 60 * 1000;
    let sessions = [
        make_session("new", AgentKind::Claude, "New", now, false, false),
        make_session("most", AgentKind::Claude, "Most", now - 100, false, false),
        make_session(
            "tie-old",
            AgentKind::Claude,
            "Tie old",
            now - 200,
            false,
            false,
        ),
        make_session(
            "tie-new",
            AgentKind::Claude,
            "Tie new",
            now - 150,
            false,
            false,
        ),
        make_session(
            "unvisited",
            AgentKind::Claude,
            "Unvisited",
            now,
            false,
            false,
        ),
    ];
    let inputs: Vec<_> = sessions.iter().map(|s| make_input("h", s)).collect();
    let k = |i: usize| inputs[i].key.clone();
    for auto_sort in [true, false] {
        let mut store = WorkspaceStore::new();
        store.auto_sort = auto_sort;
        store.record_session_view(&k(0), now);
        for offset in [500, 400, 300] {
            store.record_session_view(&k(1), now - offset);
        }
        for i in [2, 3] {
            store.record_session_view(&k(i), now - 500);
            store.record_session_input(&k(i), now - 400);
        }
        store.toggle_pin(&k(0), false);
        store.toggle_group_for_session(&k(2), DEFAULT_WORKSPACE_ID, None, "Group");
        store.session_meta.get_mut(&k(0)).unwrap().manual_order = Some(0);
        store.recents_section_collapsed = true;
        for workspace_id in [ALL_WORKSPACES_ID, DEFAULT_WORKSPACE_ID] {
            let layout = store.compute_sidebar_layout_with_options(
                workspace_id,
                WorkspaceView::Frequent,
                &inputs,
                false,
                None,
                now,
            );
            assert_eq!(layout.flat_display_order, vec![k(1), k(3), k(2), k(0)]);
            assert_eq!(layout.ctrl_digit_order, layout.flat_display_order);
            assert!(layout.groups.is_empty());
            assert!(layout.pinned_tiles.is_empty());
        }
    }
}

#[test]
fn sidebar_frequent_uses_inclusive_fourteen_day_window() {
    let now = 20 * 24 * 60 * 60 * 1000;
    let cutoff = now - 14 * 24 * 60 * 60 * 1000;
    let sessions: Vec<_> = (0..6)
        .map(|i| {
            make_session(
                &format!("s{i}"),
                AgentKind::Claude,
                "Chat",
                now,
                false,
                false,
            )
        })
        .collect();
    let inputs: Vec<_> = sessions.iter().map(|s| make_input("h", s)).collect();
    let k = |i: usize| inputs[i].key.clone();
    let mut store = WorkspaceStore::new();
    store.record_frequent_visit(&k(0), cutoff);
    store.record_frequent_visit(&k(1), cutoff - 1);
    store.record_frequent_visit(&k(2), now + 1);
    // Existing input-only metadata remains useful after upgrading.
    store.session_meta.entry(k(3)).or_default().last_input_ms = Some(cutoff);
    store.session_meta.entry(k(4)).or_default().last_input_ms = Some(cutoff - 1);
    for offset in [3, 2, 1] {
        store.record_frequent_visit(&k(5), cutoff - offset);
    }
    store.record_frequent_visit(&k(5), now - 1);
    let layout = store.compute_sidebar_layout_with_options(
        ALL_WORKSPACES_ID,
        WorkspaceView::Frequent,
        &inputs,
        false,
        None,
        now,
    );
    assert_eq!(layout.flat_display_order, vec![k(0), k(3), k(5)]);
    let expired = store.compute_sidebar_layout_with_options(
        ALL_WORKSPACES_ID,
        WorkspaceView::Frequent,
        &inputs,
        false,
        None,
        now + 2,
    );
    assert_eq!(expired.flat_display_order, vec![k(2), k(5)]);
}

#[test]
fn sidebar_frequent_history_survives_reload_and_prunes_expired_visits() {
    let now = 20 * 24 * 60 * 60 * 1000;
    let session = make_session("s1", AgentKind::Claude, "Chat", 100, false, false);
    let input = make_input("h", &session);
    let key = &input.key;
    let mut store = WorkspaceStore::new();
    store.record_session_view(key, now - 14 * 24 * 60 * 60 * 1000 - 1);
    store.record_session_view(key, now - 1);
    store.record_session_view(key, now - 1);
    store.record_session_input(key, now);
    assert_eq!(
        store.session_meta[key].frequent_visits_ms,
        vec![now - 1, now]
    );
    let loaded: WorkspaceStore =
        serde_json::from_slice(&serde_json::to_vec(&store).unwrap()).unwrap();
    let layout = loaded.compute_sidebar_layout_with_options(
        ALL_WORKSPACES_ID,
        WorkspaceView::Frequent,
        std::slice::from_ref(&input),
        false,
        None,
        now,
    );
    assert_eq!(layout.flat_display_order, vec![key.clone()]);
    assert_eq!(loaded, store);
}

#[test]
fn sidebar_frequent_excludes_inactive_and_respects_scope_and_unread() {
    let now = 2_000;
    let mut sessions: Vec<_> = (0..6)
        .map(|i| {
            make_session(
                &format!("s{i}"),
                AgentKind::Claude,
                "Chat",
                100,
                i == 0,
                false,
            )
        })
        .collect();
    sessions[1].archived = true;
    let inputs: Vec<_> = sessions.iter().map(|s| make_input("h", s)).collect();
    let k = |i: usize| inputs[i].key.clone();
    let mut store = WorkspaceStore::new();
    let other = store.create_workspace("Other work", None).unwrap().id;
    let snoozed = store.create_workspace("Snoozed", None).unwrap().id;
    store.session_meta.entry(k(3)).or_default().workspace_id = Some(other);
    store.session_meta.entry(k(4)).or_default().workspace_id = Some(snoozed.clone());
    store.snooze_workspace(&snoozed, now + 1_000, now).unwrap();
    store.archive_to_waiting_session(&k(2), now, &sessions[2]);
    for input in &inputs {
        store.record_session_view(&input.key, now);
    }
    let layout = store.compute_sidebar_layout_with_options(
        ALL_WORKSPACES_ID,
        WorkspaceView::Frequent,
        &inputs,
        false,
        None,
        now,
    );
    assert_eq!(layout.flat_display_order, vec![k(0), k(3), k(5)]);
    let scoped = store.compute_sidebar_layout_with_options(
        DEFAULT_WORKSPACE_ID,
        WorkspaceView::Frequent,
        &inputs,
        true,
        Some(&k(5)),
        now,
    );
    assert_eq!(scoped.flat_display_order, vec![k(0), k(5)]);
}

#[test]
fn sidebar_navigation_order_includes_sessions_beyond_digit_shortcuts() {
    let store = WorkspaceStore::new();
    let sessions: Vec<_> = (0..12)
        .map(|i| {
            make_session(
                &format!("s{i}"),
                AgentKind::Claude,
                "Chat",
                100 - i,
                false,
                false,
            )
        })
        .collect();
    let inputs: Vec<_> = sessions.iter().map(|s| make_input("h", s)).collect();
    let expected: Vec<_> = inputs.iter().map(|s| s.key.clone()).collect();
    for workspace_id in [ALL_WORKSPACES_ID, DEFAULT_WORKSPACE_ID] {
        let layout = store.compute_sidebar_layout(workspace_id, &inputs, false, 1000);
        assert_eq!(layout.ctrl_digit_order, expected);
        assert_eq!(layout.resolve_ctrl_digit(9), Some(expected[8].as_str()));
        assert_eq!(layout.resolve_ctrl_digit(10), None);
    }
}

#[test]
fn frequent_24h_input_and_6h_dwell_exact_boundaries() {
    let now = 100_000_000_i64;
    // Input exact 24h boundary
    assert!(qualifies_for_frequent(
        &[],
        Some(now - FREQUENT_INPUT_WINDOW_MS),
        now
    ));
    assert!(!qualifies_for_frequent(
        &[],
        Some(now - FREQUENT_INPUT_WINDOW_MS - 1),
        now
    ));

    // FrontPeriodTracker dwell & focus-loss reset
    let mut tracker = FrontPeriodTracker::new();
    assert!(!tracker.select(Some("h:claude:s1"), false, true, now, 1000));
    assert!(!tracker.select(Some("h:claude:s1"), true, false, now, 1000));
    assert!(tracker.select(Some("h:claude:s1"), true, true, now, 1000));
    assert_eq!(tracker.poll_visit(10_999), None);
    // Focus loss resets timer
    assert!(!tracker.select(Some("h:claude:s1"), true, false, now + 5000, 10_500));
    assert!(tracker.select(Some("h:claude:s1"), true, true, now + 6000, 11_000));
    assert_eq!(tracker.poll_visit(20_999), None);
    assert_eq!(
        tracker.poll_visit(21_000),
        Some(("h:claude:s1".to_string(), now + 6000))
    );
    // Emits only once per period
    assert_eq!(tracker.poll_visit(30_000), None);

    // 3 visits exact 6h boundary
    let ok_visits = [now - FREQUENT_VISIT_WINDOW_MS, now - 2, now - 1];
    let old_visits = [now - FREQUENT_VISIT_WINDOW_MS - 1, now - 2, now - 1];
    assert!(qualifies_for_frequent(&ok_visits, None, now));
    assert!(!qualifies_for_frequent(&old_visits, None, now));

    // History stays bounded, evicting oldest visits first.
    let mut store = WorkspaceStore::new();
    let total = FREQUENT_MAX_VISITS as i64 + 2;
    for i in 1..=total {
        store.record_frequent_visit("h:claude:s1", i * 100);
    }
    let visits = &store.session_meta["h:claude:s1"].frequent_visits_ms;
    assert_eq!(visits.len(), FREQUENT_MAX_VISITS);
    assert_eq!(visits[0], 300);
    assert_eq!(*visits.last().unwrap(), total * 100);

    // frequent_order_key prefers recent input even if a visit is newer
    let recent_input = now - 5_000;
    let newer_visit = now - 1_000;
    assert_eq!(
        frequent_order_key(&[newer_visit], Some(recent_input), now),
        recent_input
    );
    // Once input expires, falls back to newest visit
    assert_eq!(
        frequent_order_key(
            &[newer_visit],
            Some(now - FREQUENT_INPUT_WINDOW_MS - 1),
            now
        ),
        newer_visit
    );
}

#[test]
fn inbox_sort_recency_before_unread_and_tiebreakers() {
    let mut store = WorkspaceStore::new();
    let w2 = store.create_workspace("Zeta", None).unwrap();
    let now = 10_000_000_i64;

    let s_new_read = make_session(
        "s1",
        AgentKind::Claude,
        "Newest Read",
        now - 10,
        false,
        false,
    );
    let s_old_unread = make_session(
        "s2",
        AgentKind::Codex,
        "Older Unread Working",
        now - 500,
        true,
        true,
    );
    let s_tie_unread = make_session(
        "s3",
        AgentKind::Antigravity,
        "Tie Unread",
        now - 500,
        true,
        false,
    );
    let s_tie_read = make_session("s4", AgentKind::Claude, "Tie Read", now - 500, false, false);
    let s_muted = make_session("s5", AgentKind::Claude, "Muted", now, true, true);
    let s_excl = make_session("s6", AgentKind::Claude, "Excluded", now, true, true);

    let all = [
        s_new_read,
        s_old_unread,
        s_tie_unread,
        s_tie_read,
        s_muted,
        s_excl,
    ];
    let inputs: Vec<_> = all.iter().map(|s| make_input("h", s)).collect();
    let k = |i: usize| inputs[i].key.clone();

    store.assign_session_workspace(&k(1), &w2.id);
    store.toggle_session_muted(&k(4));
    store.toggle_inbox_excluded(&k(5));

    let rows = store.query_inbox(&InboxSplit::All, &inputs, false, "", now);
    let ids: Vec<&str> = rows.iter().map(|r| r.session_id.as_str()).collect();
    // Newest read beats older unread+working; at equal timestamp, unread+working > unread > read
    assert_eq!(ids, vec!["s1", "s2", "s3", "s4"]);

    // unread_only filter
    let unread_rows = store.query_inbox(&InboxSplit::All, &inputs, true, "", now);
    assert_eq!(
        unread_rows
            .iter()
            .map(|r| r.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["s2", "s3"]
    );

    // Live substring filter across title, cwd, preview, workspace name, agent label
    assert_eq!(
        store
            .query_inbox(&InboxSplit::All, &inputs, false, "zeta", now)
            .len(),
        1
    );
    assert_eq!(
        store
            .query_inbox(&InboxSplit::All, &inputs, false, "antigravity", now)
            .len(),
        1
    );
    assert_eq!(
        store
            .query_inbox(&InboxSplit::All, &inputs, false, "/repo/s1", now)
            .len(),
        1
    );

    // Frequent split sorts by frequent_order_key desc
    store.record_session_input(&k(3), now - 50);
    store.record_session_input(&k(1), now - 200);
    let freq_rows = store.query_inbox(&InboxSplit::Frequent, &inputs, false, "", now);
    assert_eq!(
        freq_rows
            .iter()
            .map(|r| r.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["s4", "s2"]
    );
}

#[test]
fn inbox_filter_matches_every_whitespace_token_in_any_order() {
    let store = WorkspaceStore::new();
    let now = 10_000_000_i64;
    let one = make_session(
        "m1",
        AgentKind::Claude,
        "Multi Bot Rust",
        now - 10,
        false,
        false,
    );
    let two = make_session(
        "m2",
        AgentKind::Codex,
        "Multi Bot Rust 2",
        now - 20,
        false,
        false,
    );
    let other = make_session(
        "p1",
        AgentKind::Codex,
        "Multi Python Tool",
        now - 30,
        false,
        false,
    );
    let all = [one, two, other];
    let inputs: Vec<_> = all.iter().map(|s| make_input("h", s)).collect();

    let ids = |query: &str| {
        store
            .query_inbox(&InboxSplit::All, &inputs, false, query, now)
            .into_iter()
            .map(|r| r.session_id)
            .collect::<Vec<_>>()
    };

    assert_eq!(ids("multi rust"), vec!["m1", "m2"]);
    assert_eq!(ids("rust multi"), vec!["m1", "m2"]);
    assert_eq!(ids("MULTI RUST"), vec!["m1", "m2"]);
    assert_eq!(ids("  multi   rust  "), vec!["m1", "m2"]);
    let broad = ids("multi r");
    assert!(broad.contains(&"m1".to_string()) && broad.contains(&"m2".to_string()));
    assert_eq!(ids("multi python"), vec!["p1"]);
    assert!(ids("multi rust python").is_empty());
    assert!(ids("multi nomatch").is_empty());
}

#[test]
fn inbox_splits_with_sessions_hides_zero_count_splits_and_keeps_all() {
    let mut store = WorkspaceStore::new();
    let now = 10_000_000_i64;

    // No sessions at all: only `All` survives so the Inbox keeps a live tab
    assert_eq!(
        store.inbox_splits_with_sessions(&[], false, "", now),
        vec![InboxSplit::All]
    );

    let back = store.create_workspace("Backend", None).unwrap();
    let shelf = store.create_workspace("Empty Shelf", None).unwrap();

    let s1 = make_session("s1", AgentKind::Claude, "Favorite", now - 10, false, false);
    let s2 = make_session("s2", AgentKind::Codex, "Frequent", now - 20, false, false);
    let s3 = make_session("s3", AgentKind::Claude, "Backend", now - 30, false, false);
    let all = [s1, s2, s3];
    let inputs: Vec<_> = all.iter().map(|s| make_input("h", s)).collect();
    let k = |i: usize| inputs[i].key.clone();

    store.toggle_favorite(&k(0));
    store.record_session_input(&k(1), now);
    store.assign_session_workspace(&k(2), &back.id);

    let splits = store.inbox_splits_with_sessions(&inputs, false, "", now);
    assert_eq!(
        splits,
        vec![
            InboxSplit::Frequent,
            InboxSplit::All,
            InboxSplit::Favorites,
            // s1 and s2 never leave the default workspace
            InboxSplit::Workspace(DEFAULT_WORKSPACE_ID.to_string()),
            InboxSplit::Workspace(back.id.clone()),
        ],
        "only splits with >= 1 session stay in the Inbox split bar"
    );
    assert!(
        !splits.contains(&InboxSplit::Workspace(shelf.id.clone())),
        "an empty custom workspace must be hidden"
    );

    // The workspace lists behind the Sidebar switcher and Settings stay unfiltered
    assert!(
        store
            .visible_workspaces(now)
            .iter()
            .any(|w| w.id == shelf.id),
        "empty workspaces must stay manageable in the Sidebar and Settings"
    );

    // A filter that matches nothing leaves only `All`
    assert_eq!(
        store.inbox_splits_with_sessions(&inputs, false, "no-such-title", now),
        vec![InboxSplit::All]
    );

    // An unread-only filter with nothing unread leaves only `All`
    assert_eq!(
        store.inbox_splits_with_sessions(&inputs, true, "", now),
        vec![InboxSplit::All]
    );

    // Emptying a split hides it again
    store.toggle_favorite(&k(0));
    assert!(
        !store
            .inbox_splits_with_sessions(&inputs, false, "", now)
            .contains(&InboxSplit::Favorites),
        "Favorites 0 must drop out of the split bar"
    );
}

#[test]
fn inbox_waiting_undo_and_auto_unarchive_and_unread_badge() {
    let mut store = WorkspaceStore::new();
    let now = 50_000;
    let mut s1 = make_session("s1", AgentKind::Claude, "One", 1000, true, false);
    let s2 = make_session("s2", AgentKind::Claude, "Two", 1000, true, false);
    let s3 = make_session("s3", AgentKind::Claude, "Muted", 1000, true, false);
    let all = [s1.clone(), s2.clone(), s3.clone()];
    let inputs: Vec<_> = all.iter().map(|s| make_input("h", s)).collect();
    let k1 = inputs[0].key.clone();
    let k2 = inputs[1].key.clone();
    let k3 = inputs[2].key.clone();
    store.toggle_session_muted(&k3);

    // Unread badge excludes muted
    assert_eq!(store.inbox_unread_count(&inputs), 2);
    store.mark_session_read(&k1, &s1);
    assert_eq!(store.inbox_unread_count(&inputs), 1);

    // Archive to Waiting hides from Inbox and Sidebar
    store.archive_to_waiting(&k1, now, s1.updated_at_ms, s1.last_assistant_id.clone());
    store.archive_to_waiting(&k2, now + 1, s2.updated_at_ms, s2.last_assistant_id.clone());
    assert!(
        store
            .query_inbox(&InboxSplit::All, &inputs, false, "", now + 2)
            .is_empty()
    );
    assert_eq!(
        store
            .compute_sidebar_layout(DEFAULT_WORKSPACE_ID, &inputs, false, now + 2)
            .total_visible_sessions(),
        1 // only s3 (muted) remains in sidebar
    );

    // Undo LIFO restores s2 first
    assert_eq!(store.undo_last_waiting(), Some(k2.clone()));
    assert!(!store.waiting_sessions.contains_key(&k2));
    assert!(store.waiting_sessions.contains_key(&k1));

    // New assistant activity auto-unarchives s1
    s1.updated_at_ms = 2000;
    s1.last_assistant_id = Some("asst-s1-2000".into());
    s1.last_assistant_ms = Some(2000);
    assert!(store.auto_unarchive_waiting_if_active(&k1, &s1));
    assert!(!store.waiting_sessions.contains_key(&k1));
}

#[test]
fn palette_empty_query_recent5_most_used10_and_8_categories() {
    let mut store = WorkspaceStore::new();
    let snoozed_ws = store.create_workspace("SnoozedWS", None).unwrap();
    let now = 100_000_000_i64;
    store
        .snooze_workspace(&snoozed_ws.id, now + 10_000, now)
        .unwrap();

    let summaries: Vec<SessionSummary> = (0..9)
        .map(|i| {
            make_session(
                &format!("s{i}"),
                AgentKind::Claude,
                &format!("Session {i}"),
                now - (i as i64) * 100,
                false,
                false,
            )
        })
        .collect();
    let inputs: Vec<_> = summaries.iter().map(|s| make_input("h", s)).collect();
    let k = |i: usize| inputs[i].key.clone();

    // s0 = current (excluded), s1 = previous (must lead), s2 = snoozed ws (excluded), s3 = waiting (excluded)
    store.assign_session_workspace(&k(2), &snoozed_ws.id);
    store.archive_to_waiting(&k(3), now, 0, None);
    for i in 0..9 {
        store.record_session_view(&k(i), now - (9 - i as i64) * 10);
    }

    let mut stats = PaletteStats::default();
    let mut cmds = Vec::new();
    for i in 0..12 {
        let id = format!("cmd.{i}");
        cmds.push(PaletteCommandSpec {
            id: id.clone(),
            label: format!("Command {i}"),
            chord: String::new(),
            category: PaletteCategory::ALL[i % 8],
            destructive: i == 0 || i == 11,
            aliases: Vec::new(),
            tab_context: true,
            global: true,
        });
        for _ in 0..(12 - i) {
            stats.record_command_use(&id, now - 1000);
        }
    }
    // Add an unused safe & destructive command in Sessions
    cmds.push(PaletteCommandSpec {
        id: "unused.dest".into(),
        label: "AAA Destructive".into(),
        chord: String::new(),
        category: PaletteCategory::Sessions,
        destructive: true,
        aliases: Vec::new(),
        tab_context: false,
        global: true,
    });
    cmds.push(PaletteCommandSpec {
        id: "unused.safe".into(),
        label: "ZZZ Safe".into(),
        chord: String::new(),
        category: PaletteCategory::Sessions,
        destructive: false,
        aliases: Vec::new(),
        tab_context: false,
        global: true,
    });

    let rows = rank_palette(
        &store,
        &stats,
        &inputs,
        &cmds,
        "",
        &PaletteMode::Global,
        Some(&k(0)),
        Some(&k(1)),
        now,
    );

    // Verify Recent sessions: max 5, leads with s1, excludes s0, s2, s3
    assert_eq!(rows[0], PaletteRow::Header("Recent sessions".into()));
    let recent_sids: Vec<&str> = rows[1..]
        .iter()
        .take_while(|r| matches!(r, PaletteRow::Session { .. }))
        .map(|r| match r {
            PaletteRow::Session { session_key, .. } => session_key.as_str(),
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(recent_sids.len(), 5);
    assert_eq!(recent_sids[0], k(1));
    assert!(!recent_sids.contains(&k(0).as_str()));
    assert!(!recent_sids.contains(&k(2).as_str()));
    assert!(!recent_sids.contains(&k(3).as_str()));

    // Verify Most used has exactly 10 commands and safe precedes destructive
    let mu_idx = rows
        .iter()
        .position(|r| *r == PaletteRow::Header("Most used".into()))
        .unwrap();
    let mu_cmds: Vec<bool> = rows[mu_idx + 1..]
        .iter()
        .take_while(|r| matches!(r, PaletteRow::Command { .. }))
        .map(|r| match r {
            PaletteRow::Command { destructive, .. } => *destructive,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(mu_cmds.len(), 10);
    // Destructive cmd.0 must appear AFTER all 9 safe commands
    assert_eq!(mu_cmds[9], true);
    assert!(mu_cmds[..9].iter().all(|&d| !d));

    // Verify Sessions category puts unused.safe before unused.dest
    let sess_hdr = rows
        .iter()
        .position(|r| *r == PaletteRow::Header("Sessions".into()))
        .unwrap();
    let sess_ids: Vec<&str> = rows[sess_hdr + 1..]
        .iter()
        .take_while(|r| matches!(r, PaletteRow::Command { .. }))
        .map(|r| match r {
            PaletteRow::Command { id, .. } => id.as_str(),
            _ => unreachable!(),
        })
        .collect();
    let safe_pos = sess_ids.iter().position(|&id| id == "unused.safe").unwrap();
    let dest_pos = sess_ids.iter().position(|&id| id == "unused.dest").unwrap();
    assert!(safe_pos < dest_pos);
}

#[test]
fn palette_workspaces_mode_lists_only_visible_workspaces_and_virtual_views() {
    let mut store = WorkspaceStore::new();
    let active = store.create_workspace("Product", None).unwrap();
    let archived = store.create_workspace("Archived", None).unwrap();
    let snoozed = store.create_workspace("Snoozed", None).unwrap();
    store
        .workspaces
        .iter_mut()
        .find(|w| w.id == archived.id)
        .unwrap()
        .archived = true;
    store
        .workspaces
        .iter_mut()
        .find(|w| w.id == snoozed.id)
        .unwrap()
        .snoozed_until_ms = Some(2_000);
    let session = make_session("s1", AgentKind::Claude, "Product", 1_000, false, false);
    let inputs = [make_input("h", &session)];
    let commands = [PaletteCommandSpec {
        id: "cmd".into(),
        label: "Product".into(),
        chord: String::new(),
        category: PaletteCategory::Sessions,
        destructive: false,
        aliases: vec![],
        tab_context: true,
        global: true,
    }];
    let stats = PaletteStats::default();
    let rank = |query| {
        rank_palette(
            &store,
            &stats,
            &inputs,
            &commands,
            query,
            &PaletteMode::Workspaces,
            None,
            None,
            1_000,
        )
    };
    let rows = rank("");
    let choices: Vec<_> = rows
        .iter()
        .map(|r| match r {
            PaletteRow::Workspace {
                workspace_id,
                view,
                name,
                archived,
                ..
            } => {
                assert!(!archived);
                (workspace_id.as_str(), *view, name.as_str())
            }
            other => panic!("unexpected workspace choice: {other:?}"),
        })
        .collect();
    assert_eq!(
        &choices[..3],
        &[
            (ALL_WORKSPACES_ID, WorkspaceView::All, "All"),
            (ALL_WORKSPACES_ID, WorkspaceView::Frequent, "Frequent"),
            (ALL_WORKSPACES_ID, WorkspaceView::Favorites, "Favorites"),
        ]
    );
    let ids: Vec<_> = choices[3..].iter().map(|c| c.0).collect();
    assert_eq!(
        ids,
        store
            .visible_workspaces(1_000)
            .iter()
            .map(|w| w.id.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        matches!(&rank("pRoDuCt")[..], [PaletteRow::Workspace { workspace_id, view: WorkspaceView::All, .. }] if workspace_id == &active.id)
    );
    assert!(rank("Archived").is_empty());
    assert!(rank("Snoozed").is_empty());
    assert!(rank("no match").is_empty());
    assert!(matches!(
        &rank("frequent")[..],
        [PaletteRow::Workspace {
            view: WorkspaceView::Frequent,
            ..
        }]
    ));
}

#[test]
fn palette_search_includes_done_and_hibernated_but_not_dissolved_sessions() {
    let now = 1_000;
    for state in [
        "active",
        "waiting",
        "archived",
        "hibernated",
        "snoozed",
        "killed",
        "dissolved",
        "archived-workspace",
    ] {
        let mut store = WorkspaceStore::new();
        let workspace = store.create_workspace("Project", None).unwrap();
        let mut session = make_session("s1", AgentKind::Claude, "Find me", now, false, false);
        session.cwd = Some("/home/project-folder".into());
        session.archived = matches!(state, "archived" | "killed");
        session.archive_kill_process = state == "killed";
        let inputs = [make_input("h", &session)];
        let key = &inputs[0].key;
        store.assign_session_workspace(key, &workspace.id);
        match state {
            "waiting" => store.archive_to_waiting_session(key, now, &session),
            "hibernated" => store.hibernate_session(key, &session, Some(now + 60_000), None, false),
            "snoozed" => {
                store.snooze_workspace(&workspace.id, now + 60_000, now).unwrap();
            }
            "dissolved" => store.dissolve_session(key, now),
            "archived-workspace" => {
                store
                    .workspaces
                    .iter_mut()
                    .find(|ws| ws.id == workspace.id)
                    .unwrap()
                    .archived = true;
            }
            _ => {}
        }
        store.record_session_view(key, now);
        let stats = PaletteStats::default();
        for query in ["Find me", "find", "folder", "claude", ""] {
            let rows = rank_palette(
                &store,
                &stats,
                &inputs,
                &[],
                query,
                &PaletteMode::Global,
                None,
                None,
                now,
            );
            let sessions: Vec<_> = rows
                .iter()
                .filter_map(|row| match row {
                    PaletteRow::Session {
                        session_key,
                        workspace_name,
                        ..
                    } => Some((session_key, workspace_name)),
                    _ => None,
                })
                .collect();
            let included = if query.is_empty() {
                state == "active"
            } else {
                !matches!(state, "killed" | "dissolved" | "archived-workspace")
            };
            assert_eq!(sessions.len(), usize::from(included), "{state}: {query:?}");
            if included {
                let label = match state {
                    "waiting" | "archived" => "Project · Done",
                    "hibernated" | "snoozed" => "Project · Hibernated",
                    _ => "Project",
                };
                assert_eq!(sessions[0], (key, &label.to_string()), "{state}: {query:?}");
            }
        }
    }
}

#[test]
fn palette_main_search_ranks_sessions_above_equal_matching_workspaces() {
    let mut store = WorkspaceStore::new();
    let workspace = store.create_workspace("Product", None).unwrap();
    let session = make_session("s1", AgentKind::Claude, "Product", 0, false, false);
    let inputs = [make_input("h", &session)];
    for query in ["Product", "PROD", "duct"] {
        let rows = rank_palette(
            &store,
            &PaletteStats::default(),
            &inputs,
            &[],
            query,
            &PaletteMode::Global,
            None,
            None,
            1_000,
        );
        assert!(
            matches!(&rows[0], PaletteRow::Session { session_key, .. } if session_key == &inputs[0].key)
        );
        assert!(
            matches!(&rows[1], PaletteRow::Workspace { workspace_id, .. } if workspace_id == &workspace.id)
        );
    }
}

#[test]
fn palette_typed_match_ranks_frecency_band_dominance_and_del_boost() {
    // 4 > 3 > 2 > 1 > 0 including Unicode
    assert_eq!(palette_text_match_rank("Kraków", "kraków"), 4);
    assert_eq!(palette_text_match_rank("Kraków Server", "kraków"), 3);
    assert_eq!(palette_text_match_rank("Open Kraków", "kraków"), 2);
    assert_eq!(palette_text_match_rank("MyKrakówApp", "kraków"), 1);
    assert_eq!(palette_text_match_rank("Warsaw", "kraków"), 0);

    // Recency band strictly dominates lifetime count (step 16 > max freq 15)
    let now = 200_i64 * 24 * 3_600_000;
    let six_days_once = palette_frecency_at(
        PaletteUsage {
            count: 1,
            last_used_ms: now - 6 * 24 * 3_600_000,
        },
        now,
    );
    let eight_days_max = palette_frecency_at(
        PaletteUsage {
            count: u32::MAX,
            last_used_ms: now - 8 * 24 * 3_600_000,
        },
        now,
    );
    assert_eq!(six_days_once, 64);
    assert_eq!(eight_days_max, 48 + 15);
    assert!(six_days_once > eight_days_max);

    // "del" prefix boosts Delete session over Delete workspace even if workspace used more
    let store = WorkspaceStore::new();
    let mut stats = PaletteStats::default();
    for _ in 0..50 {
        stats.record_command_use("workspace.delete", now - 1000);
    }
    let cmds = vec![
        PaletteCommandSpec {
            id: "workspace.delete".into(),
            label: "Delete workspace".into(),
            chord: String::new(),
            category: PaletteCategory::WorkspacesAndWindows,
            destructive: true,
            aliases: vec![],
            tab_context: false,
            global: true,
        },
        PaletteCommandSpec {
            id: "session.delete".into(),
            label: "Delete session".into(),
            chord: "Ctrl+Shift+W".into(),
            category: PaletteCategory::Sessions,
            destructive: true,
            aliases: vec![],
            tab_context: true,
            global: true,
        },
    ];

    let rows = rank_palette(
        &store,
        &stats,
        &[],
        &cmds,
        "del",
        &PaletteMode::Global,
        None,
        None,
        now,
    );
    assert_eq!(rows.len(), 2);
    match &rows[0] {
        PaletteRow::Command { id, .. } => assert_eq!(id, "session.delete"),
        other => panic!("expected session.delete first, got {other:?}"),
    }

    // TabContext excludes global-only commands & sessions
    let s1 = make_session("s1", AgentKind::Claude, "Delete me", now, false, false);
    let inputs = [make_input("h", &s1)];
    let tab_rows = rank_palette(
        &store,
        &stats,
        &inputs,
        &cmds,
        "del",
        &PaletteMode::TabContext {
            target_session_key: inputs[0].key.clone(),
        },
        None,
        None,
        now,
    );
    assert_eq!(tab_rows.len(), 1);
    match &tab_rows[0] {
        PaletteRow::Command { id, .. } => assert_eq!(id, "session.delete"),
        other => panic!("expected tab_context command only, got {other:?}"),
    }
}

#[test]
fn canonical_f2_rename_utf8_63_char_cap_and_blank_rejection() {
    let mut store = WorkspaceStore::new();
    let key = canonical_session_key("h", AgentKind::Claude, "s1");

    // Blank/whitespace rejected
    assert_eq!(canonicalize_session_title("   \t\r\n  "), None);
    assert_eq!(store.apply_session_rename(&key, "   "), None);
    assert!(!store.session_titles.contains_key(&key));

    // Trims surrounding whitespace and locks manual: true
    let renamed = store.apply_session_rename(&key, "  Deploy Fix  ").unwrap();
    assert_eq!(renamed, "Deploy Fix");
    assert!(store.session_titles[&key].manual);
    assert_eq!(store.effective_title(&key, "Original"), "Deploy Fix");

    // Blank rename preserves existing name
    assert_eq!(store.apply_session_rename(&key, "\t  \n"), None);
    assert_eq!(store.effective_title(&key, "Original"), "Deploy Fix");

    // 70-char multibyte UTF-8 truncates cleanly to exact 63 chars
    let utf8_70 = "🚀日Kraków🚀日Kraków🚀日Kraków🚀日Kraków🚀日Kraków🚀日Kraków🚀日Kraków🚀日Kraków🚀日Kraków";
    assert!(utf8_70.chars().count() > 63);
    let capped = store.apply_session_rename(&key, utf8_70).unwrap();
    assert_eq!(capped.chars().count(), 63);
    assert_eq!(store.effective_title(&key, "Original"), capped);
}

#[test]
fn toggle_session_group_collapsed_handles_normal_starred_and_ungrouped() {
    let mut store = WorkspaceStore::new();
    let key = canonical_session_key("h", AgentKind::Claude, "s1");

    // Ungrouped session is a no-op
    assert_eq!(
        store.toggle_session_group_collapsed(&key, DEFAULT_WORKSPACE_ID),
        None
    );
    assert!(store.groups.is_empty());

    // Normal scope group collapses and expands without ungrouping
    let gid = store
        .toggle_group_for_session(&key, DEFAULT_WORKSPACE_ID, None, "Backend")
        .unwrap();
    assert_eq!(
        store.toggle_session_group_collapsed(&key, DEFAULT_WORKSPACE_ID),
        Some(true)
    );
    assert!(store.groups.iter().find(|g| g.id == gid).unwrap().collapsed);
    assert_eq!(
        store.session_meta[&key].group_id.as_deref(),
        Some(gid.as_str())
    );
    assert_eq!(
        store.toggle_session_group_collapsed(&key, DEFAULT_WORKSPACE_ID),
        Some(false)
    );

    // Starred scope group collapses and expands independently
    store.toggle_favorite(&key);
    assert_eq!(
        store.toggle_session_group_collapsed(&key, STARRED_WORKSPACE_ID),
        None
    );
    let sgid = store
        .toggle_group_for_session(&key, STARRED_WORKSPACE_ID, None, "StarGrp")
        .unwrap();
    assert_eq!(
        store.toggle_session_group_collapsed(&key, STARRED_WORKSPACE_ID),
        Some(true)
    );
    assert!(
        store
            .groups
            .iter()
            .find(|g| g.id == sgid)
            .unwrap()
            .collapsed
    );
    assert!(!store.groups.iter().find(|g| g.id == gid).unwrap().collapsed);
}

#[test]
fn waiting_unread_session_does_not_immediately_unarchive_until_new_activity() {
    let mut store = WorkspaceStore::new();
    let mut s_unread = make_session("s1", AgentKind::Claude, "Unread", 1000, true, false);
    s_unread.last_assistant_id = Some("asst-1".into());
    s_unread.last_assistant_ms = Some(1000);
    let key = canonical_session_key("h", AgentKind::Claude, "s1");

    // Primary Inbox workflow: archive unread session without marking read first
    store.archive_to_waiting(&key, 2000, 1000, Some("asst-1".into()));
    assert!(store.waiting_sessions.contains_key(&key));

    // Polling identical SessionSummary MUST NOT unarchive
    assert!(!store.auto_unarchive_waiting_if_active(&key, &s_unread));
    assert!(store.waiting_sessions.contains_key(&key));

    // New assistant reply wakes it
    s_unread.updated_at_ms = 2500;
    s_unread.last_assistant_id = Some("asst-2".into());
    s_unread.last_assistant_ms = Some(2500);
    assert!(store.auto_unarchive_waiting_if_active(&key, &s_unread));
    assert!(!store.waiting_sessions.contains_key(&key));

    // Read session archived via archive_to_waiting_session wakes if needs_attention flips true
    let mut s_read = make_session("s2", AgentKind::Claude, "Read", 1000, false, false);
    let key2 = canonical_session_key("h", AgentKind::Claude, "s2");
    store.archive_to_waiting_session(&key2, 2000, &s_read);
    assert!(!store.auto_unarchive_waiting_if_active(&key2, &s_read));
    s_read.needs_attention = true;
    assert!(store.auto_unarchive_waiting_if_active(&key2, &s_read));
}

#[test]
fn is_session_unread_recovers_when_assistant_id_and_ms_are_none() {
    let mut store = WorkspaceStore::new();
    let mut s = SessionSummary {
        id: "s_min".into(),
        agent: AgentKind::Claude,
        title: "Minimal".into(),
        source_path: "/tmp/s_min.jsonl".into(),
        updated_at_ms: 1000,
        needs_attention: true,
        last_assistant_id: None,
        last_assistant_ms: None,
        ..Default::default()
    };
    let key = canonical_session_key("h", AgentKind::Claude, "s_min");

    assert!(store.is_session_unread(&key, &s));
    store.mark_session_read(&key, &s);
    assert!(!store.is_session_unread(&key, &s));

    // New activity arrives with updated_at_ms advanced (still None for assistant id/ms)
    s.updated_at_ms = 2000;
    s.needs_attention = true;
    assert!(store.is_session_unread(&key, &s));
}

#[test]
fn reorder_session_preserves_auto_sort_on_noop_and_materializes_all_workspaces() {
    let mut store = WorkspaceStore::new();
    let ws_b = store.create_workspace("WorkB", None).unwrap();

    let a1 = make_session("a1", AgentKind::Claude, "A1", 100, false, false);
    let a2 = make_session("a2", AgentKind::Claude, "A2", 500, false, false);
    let b_muted = make_session("b_muted", AgentKind::Claude, "BMuted", 900, false, false);
    let b_old = make_session("b_old", AgentKind::Claude, "BOld", 200, false, false);
    let b_new = make_session("b_new", AgentKind::Claude, "BNew", 400, false, false);
    let all = [a1, a2, b_muted, b_old, b_new];
    let inputs: Vec<_> = all.iter().map(|s| make_input("h", s)).collect();

    let ka1 = inputs[0].key.clone();
    let ka2 = inputs[1].key.clone();
    let kb_muted = inputs[2].key.clone();
    let kb_old = inputs[3].key.clone();
    let kb_new = inputs[4].key.clone();

    store.assign_session_workspace(&kb_muted, &ws_b.id);
    store.assign_session_workspace(&kb_old, &ws_b.id);
    store.assign_session_workspace(&kb_new, &ws_b.id);
    store.toggle_session_muted(&kb_muted);

    // No-op move at top boundary MUST NOT flip auto_sort to false
    assert!(store.auto_sort);
    assert!(!store.reorder_session(
        DEFAULT_WORKSPACE_ID,
        &ka2,
        ReorderDirection::Up,
        &inputs,
        1000
    ));
    assert!(store.auto_sort);

    // No-op move for missing session MUST NOT flip auto_sort
    assert!(!store.reorder_session(
        DEFAULT_WORKSPACE_ID,
        "h:claude:missing",
        ReorderDirection::Down,
        &inputs,
        1000
    ));
    assert!(store.auto_sort);

    // Valid reorder in Workspace A disables auto_sort WITHOUT scrambling Workspace B
    assert!(store.reorder_session(
        DEFAULT_WORKSPACE_ID,
        &ka1,
        ReorderDirection::Up,
        &inputs,
        1000
    ));
    assert!(!store.auto_sort);

    let b_layout = store.compute_sidebar_layout(&ws_b.id, &inputs, false, 1000);
    let b_ids: Vec<&str> = b_layout
        .recents
        .iter()
        .map(|r| r.session_id.as_str())
        .collect();
    assert_eq!(b_ids, vec!["b_muted", "b_new", "b_old"]);
}

#[test]
fn overflow_safety_in_cycle_workspace_id_and_create_group() {
    let mut store = WorkspaceStore::new();
    let work = store.create_workspace("Work", None).unwrap();

    let _ = store.cycle_workspace_id(&work.id, isize::MAX, 1000);
    let _ = store.cycle_workspace_id(&work.id, isize::MIN, 1000);

    store.groups.push(SessionGroup {
        id: "grp-max".into(),
        name: "Max".into(),
        color: None,
        collapsed: false,
        workspace_id: DEFAULT_WORKSPACE_ID.into(),
        position: usize::MAX,
    });
    let created = store.create_group(DEFAULT_WORKSPACE_ID, "Next", None);
    assert_eq!(created.position, usize::MAX);
}

#[test]
fn inbox_unread_count_excludes_snoozed_and_archived_workspaces() {
    let mut store = WorkspaceStore::new();
    let ws_snz = store.create_workspace("Snoozed", None).unwrap();
    let ws_arch = store.create_workspace("Archived", None).unwrap();
    let now = 1_000;
    store.snooze_workspace(&ws_snz.id, 5_000, now).unwrap();
    store.archive_workspace(&ws_arch.id).unwrap();

    let s_snz = make_session("s_snz", AgentKind::Claude, "S", 100, true, false);
    let s_arch = make_session("s_arch", AgentKind::Claude, "A", 100, true, false);
    let s_ok = make_session("s_ok", AgentKind::Claude, "OK", 100, true, false);
    let all = [s_snz, s_arch, s_ok];
    let inputs: Vec<_> = all.iter().map(|s| make_input("h", s)).collect();
    store.assign_session_workspace(&inputs[0].key, &ws_snz.id);
    store.assign_session_workspace(&inputs[1].key, &ws_arch.id);

    assert_eq!(
        store
            .query_inbox(&InboxSplit::All, &inputs, false, "", now)
            .len(),
        1
    );
    assert_eq!(store.inbox_unread_count(&inputs), 1);
    assert_eq!(store.inbox_unread_count_at(&inputs, now), 1);
    // Once snooze expires at now == 5_000, inbox_unread_count_at includes s_snz again
    assert_eq!(store.inbox_unread_count_at(&inputs, 5_000), 2);
}

#[test]
fn delete_or_archive_last_active_workspace_always_keeps_visible_workspace() {
    let mut store = WorkspaceStore::new();
    let active = store.create_workspace("Active", None).unwrap();
    store.archive_workspace(DEFAULT_WORKSPACE_ID).unwrap();
    let outcome = store.delete_workspace(&active.id, None).unwrap();
    assert!(outcome.created_default_general);
    assert!(!store.visible_workspaces(1000).is_empty());
    assert_eq!(store.default_workspace_id(), DEFAULT_WORKSPACE_ID);
    assert!(
        !store
            .workspace_by_id(DEFAULT_WORKSPACE_ID)
            .unwrap()
            .archived
    );

    // Archiving the last active workspace also keeps a non-archived workspace alive
    store.archive_workspace(DEFAULT_WORKSPACE_ID).unwrap();
    assert!(!store.visible_workspaces(1000).is_empty());
}

#[test]
fn query_inbox_preserves_json_array_order_when_positions_default_zero() {
    let json =
        r#"{"workspaces":[{"id":"w_zebra","name":"Zebra"},{"id":"w_alpha","name":"Alpha"}]}"#;
    let mut store: WorkspaceStore = serde_json::from_str(json).unwrap();

    let s_z = make_session("z_sess", AgentKind::Claude, "Z", 1000, false, false);
    let s_a = make_session("a_sess", AgentKind::Claude, "A", 1000, false, false);
    let all = [s_a, s_z];
    let inputs: Vec<_> = all.iter().map(|s| make_input("h", s)).collect();
    store.assign_session_workspace(&inputs[0].key, "w_alpha");
    store.assign_session_workspace(&inputs[1].key, "w_zebra");

    let rows = store.query_inbox(&InboxSplit::All, &inputs, false, "", 2000);
    let ids: Vec<&str> = rows.iter().map(|r| r.session_id.as_str()).collect();
    assert_eq!(ids, vec!["z_sess", "a_sess"]);
}

#[test]
fn ctrl_digit_order_keeps_fixed_sections_open_and_palette_matches_non_alnum_word_boundary() {
    let mut store = WorkspaceStore::new();
    let s1 = make_session("s1", AgentKind::Claude, "Pinned", 100, false, false);
    let s2 = make_session("s2", AgentKind::Claude, "Recent", 200, false, false);
    let all = [s1, s2];
    let inputs: Vec<_> = all.iter().map(|s| make_input("h", s)).collect();
    store.toggle_pin(&inputs[0].key, false);

    store.pinned_section_collapsed = true;
    let layout1 = store.compute_sidebar_layout(DEFAULT_WORKSPACE_ID, &inputs, false, 1000);
    assert_eq!(
        layout1.ctrl_digit_order,
        vec![inputs[0].key.clone(), inputs[1].key.clone()]
    );

    store.pinned_section_collapsed = false;
    store.recents_section_collapsed = true;
    let layout2 = store.compute_sidebar_layout(DEFAULT_WORKSPACE_ID, &inputs, false, 1000);
    assert_eq!(
        layout2.ctrl_digit_order,
        vec![inputs[0].key.clone(), inputs[1].key.clone()]
    );

    // Word-boundary match for non-alphanumeric prefixes after separator
    assert_eq!(palette_text_match_rank("Chat View: /rename", "/rename"), 2);
    assert_eq!(palette_text_match_rank("Inbox ★ Favorites", "★"), 2);
    assert_eq!(palette_text_match_rank("foo/bar", "/bar"), 1);
}

#[test]
fn waiting_unread_session_excluded_from_unread_counts_until_woken() {
    let mut store = WorkspaceStore::new();
    let mut s_wait = make_session("s_wait", AgentKind::Claude, "Waiting", 1_000, true, false);
    s_wait.last_assistant_id = Some("a1".into());
    s_wait.last_assistant_ms = Some(1_000);
    let key = canonical_session_key("h", AgentKind::Claude, "s_wait");

    let inputs = [make_input("h", &s_wait)];
    assert_eq!(store.inbox_unread_count(&inputs), 1);
    assert_eq!(store.inbox_unread_count_at(&inputs, 2_000), 1);

    // Archiving unread session to Waiting clears phantom unread badge while hidden
    store.archive_to_waiting_session(&key, 2_000, &s_wait);
    assert_eq!(
        store
            .query_inbox(&InboxSplit::All, &inputs, true, "", 2_000)
            .len(),
        0
    );
    assert_eq!(store.inbox_unread_count(&inputs), 0);
    assert_eq!(store.inbox_unread_count_at(&inputs, 2_000), 0);

    // Once woken by new activity, unread badge returns to 1
    s_wait.updated_at_ms = 3_000;
    s_wait.last_assistant_id = Some("a2".into());
    s_wait.last_assistant_ms = Some(3_000);
    assert!(store.auto_unarchive_waiting_if_active(&key, &s_wait));
    let inputs_woken = [make_input("h", &s_wait)];
    assert_eq!(store.inbox_unread_count(&inputs_woken), 1);
    assert_eq!(store.inbox_unread_count_at(&inputs_woken, 3_000), 1);
}

#[test]
fn archiving_or_deleting_general_while_other_workspace_snoozed_keeps_active_default() {
    // Case 1: Archive general while another workspace is snoozed
    let mut store = WorkspaceStore::new();
    let snz = store.create_workspace("Snoozed", None).unwrap();
    store.snooze_workspace(&snz.id, 100_000, 1_000).unwrap();

    let unassigned = make_session("u1", AgentKind::Claude, "Unassigned", 1_000, true, false);
    let inputs = [make_input("h", &unassigned)];

    store.archive_workspace(DEFAULT_WORKSPACE_ID).unwrap();
    assert!(!store.visible_workspaces(1_000).is_empty());
    let def_id = store.default_workspace_id();
    assert_eq!(def_id, DEFAULT_WORKSPACE_ID);
    let def_ws = store.workspace_by_id(&def_id).unwrap();
    assert!(!def_ws.archived && !def_ws.snoozed);
    assert_eq!(
        store
            .query_inbox(&InboxSplit::All, &inputs, false, "", 1_000)
            .len(),
        1
    );

    // Case 2: Delete general while another workspace is snoozed
    let mut store2 = WorkspaceStore::new();
    let snz2 = store2.create_workspace("Snoozed", None).unwrap();
    store2.snooze_workspace(&snz2.id, 100_000, 1_000).unwrap();

    let outcome = store2.delete_workspace(DEFAULT_WORKSPACE_ID, None).unwrap();
    assert!(outcome.created_default_general);
    assert!(!store2.visible_workspaces(1_000).is_empty());
    let def_id2 = store2.default_workspace_id();
    assert_eq!(def_id2, DEFAULT_WORKSPACE_ID);
    let def_ws2 = store2.workspace_by_id(&def_id2).unwrap();
    assert!(!def_ws2.archived && !def_ws2.snoozed);
    assert_eq!(
        store2
            .query_inbox(&InboxSplit::All, &inputs, false, "", 1_000)
            .len(),
        1
    );
}

#[test]
fn undo_last_waiting_skips_stale_already_auto_unarchived_entries() {
    let mut store = WorkspaceStore::new();
    let s1 = make_session("s1", AgentKind::Claude, "First", 1_000, true, false);
    let mut s2 = make_session("s2", AgentKind::Claude, "Second", 1_000, true, false);
    let k1 = canonical_session_key("h", AgentKind::Claude, "s1");
    let k2 = canonical_session_key("h", AgentKind::Claude, "s2");

    store.archive_to_waiting_session(&k1, 2_000, &s1);
    store.archive_to_waiting_session(&k2, 2_001, &s2);

    // s2 wakes automatically via new activity, leaving a stale top entry on waiting_undo_stack
    s2.updated_at_ms = 3_000;
    s2.last_assistant_id = Some("asst-s2-new".into());
    s2.last_assistant_ms = Some(3_000);
    assert!(store.auto_unarchive_waiting_if_active(&k2, &s2));
    assert!(!store.waiting_sessions.contains_key(&k2));
    assert!(store.waiting_sessions.contains_key(&k1));

    // undo_last_waiting skips stale k2 and restores still-waiting k1
    assert_eq!(store.undo_last_waiting(), Some(k1.clone()));
    assert!(!store.waiting_sessions.contains_key(&k1));
    assert_eq!(store.undo_last_waiting(), None);
}

#[test]
fn all_keeps_open_non_dissolved_shell_visible_during_stale_snapshots() {
    let now = 10_000;
    for state in [
        "done",
        "hibernated",
        "archived",
        "snoozed",
        "dissolved",
        "killed",
    ] {
        let mut store = WorkspaceStore::new();
        let mut shell = make_session(
            "live-shell",
            AgentKind::Shell,
            "Live shell",
            100,
            false,
            false,
        );
        shell.shell_attached = Some(false);
        let key = canonical_session_key("h", shell.agent, &shell.id);
        match state {
            "done" => store.archive_to_waiting_session(&key, 100, &shell),
            "hibernated" => store.hibernate_session(&key, &shell, Some(now + 1_000), None, false),
            "archived" => shell.archived = true,
            "snoozed" => {
                store
                    .snooze_workspace(DEFAULT_WORKSPACE_ID, now + 1_000, now)
                    .unwrap();
            }
            "dissolved" => store.dissolve_session(&key, 100),
            "killed" => {
                shell.archived = true;
                shell.archive_kill_process = true;
            }
            _ => unreachable!(),
        }
        let inputs = [make_input("h", &shell)];
        assert!(
            store
                .compute_sidebar_layout(ALL_WORKSPACES_ID, &inputs, false, now)
                .flat_display_order
                .is_empty(),
            "{state}"
        );
        let layout = store.compute_sidebar_layout_with_options(
            ALL_WORKSPACES_ID,
            WorkspaceView::All,
            &inputs,
            true,
            Some(&key),
            now,
        );
        if matches!(state, "dissolved" | "killed") {
            assert!(layout.flat_display_order.is_empty(), "{state}");
        } else {
            assert_eq!(layout.flat_display_order, vec![key.clone()], "{state}");
            let scoped = store.compute_sidebar_layout_with_options(
                DEFAULT_WORKSPACE_ID,
                WorkspaceView::All,
                &inputs,
                false,
                Some(&key),
                now,
            );
            assert!(scoped.flat_display_order.is_empty(), "{state}");
        }
    }
}

#[test]
fn reactivated_shell_input_promotes_it_to_top_of_all_even_with_stale_archive_flag() {
    let now = 10_000;
    let mut store = WorkspaceStore::new();
    let recent = make_session("recent", AgentKind::Claude, "Recent", 9_000, false, false);
    let mut shell = make_session("quiet", AgentKind::Shell, "Quiet shell", 100, false, false);
    shell.archived = true;
    shell.shell_attached = Some(false);
    let inputs = [make_input("h", &recent), make_input("h", &shell)];
    let key = inputs[1].key.clone();
    store.hibernate_session(&key, &shell, Some(now + 1_000), None, false);
    store.move_to_inbox(&key);
    store.record_session_input(&key, now);
    let layout = store.compute_sidebar_layout_with_options(
        ALL_WORKSPACES_ID,
        WorkspaceView::All,
        &inputs,
        false,
        Some(&key),
        now,
    );
    assert_eq!(layout.flat_display_order, vec![key, inputs[0].key.clone()]);
    assert_eq!(layout.recents[0].used_ms, now);
}
