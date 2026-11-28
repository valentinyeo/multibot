use super::*;
use crate::providers::read_history_page;
use std::fs;
use std::sync::atomic::AtomicU64;

static TEST_COUNTER: AtomicU64 = AtomicU64::new(1);

#[test]
fn resumed_agy_session_updates_the_existing_native_or_source_owner() {
    for by_native_id in [true, false] {
        let home = temp_home("agy-resume-owner");
        let source = home.join("transcript.jsonl");
        fs::write(&source, "").unwrap();
        let mut meta = MetadataStore::default();
        meta.sessions.insert(
            "hosted".into(),
            SessionMetaEntry {
                title: Some("Postbox Rust 2".into()),
                agent: Some(AgentKind::Antigravity),
                source_path: Some(source.to_string_lossy().into()),
                native_session_id: by_native_id.then(|| "native".into()),
                workspace: Some("Postbox".into()),
                ..Default::default()
            },
        );
        meta.save(&home).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let manager = ProcessManager::new(home.clone(), tx);
        let resumed_source = if by_native_id {
            home.join("new-transcript.jsonl")
        } else {
            home.join("./transcript.jsonl")
        };
        manager.record_resumed_agy_session("native", Some("/repo"), &resumed_source);
        manager.record_resumed_agy_session("native", Some("/other"), &resumed_source);
        let meta = MetadataStore::load(&home);
        assert_eq!(meta.sessions.len(), 1);
        let entry = meta.get("hosted").unwrap();
        assert_eq!(entry.title.as_deref(), Some("Postbox Rust 2"));
        assert_eq!(entry.workspace.as_deref(), Some("Postbox"));
        assert_eq!(entry.cwd.as_deref(), Some("/repo"));
        assert_eq!(entry.source_path.as_deref(), resumed_source.to_str());
        assert_eq!(entry.native_session_id.as_deref(), Some("native"));
        assert!(entry.updated_at_ms > 0);
        fs::remove_dir_all(home).unwrap();
    }
}

#[test]
fn shell_client_ids_and_ttys_reject_traversal_and_controls() {
    for id in [
        "",
        "../",
        "/",
        "a..b",
        "a/b",
        "a\\b",
        "a\n",
        "$(evil)",
        &"a".repeat(65),
    ] {
        assert!(!multibot_proto::valid_shell_client_id(id), "{id:?}");
    }
    assert!(multibot_proto::valid_shell_client_id("MB_client-123"));
    for tty in [
        "",
        "/tmp/pts/1",
        "/dev/../etc/passwd",
        "/dev/pts/1\n/dev/pts/2",
        "/dev/a b",
    ] {
        assert!(validate_client_tty(tty).is_err(), "{tty:?}");
    }
    assert_eq!(validate_client_tty("/dev/pts/12\n").unwrap(), "/dev/pts/12");
}

#[tokio::test]
async fn shell_client_invalid_requests_emit_correlated_errors() {
    let home = temp_home("switch-invalid");
    let (tx, mut rx) = mpsc::unbounded_channel();
    let manager = ProcessManager::new(home.clone(), tx);
    for (client, session) in [
        ("../", "ok"),
        ("", "ok"),
        ("/", "ok"),
        ("client", "bad\nname"),
    ] {
        manager
            .switch_shell_client(client.into(), session.into())
            .await;
        assert!(
            matches!(rx.recv().await, Some(ServerEvent::Error { code, message })
                if code == "switch_shell_client_failed" && message.starts_with(&format!("{client}:")))
        );
    }
    let dir = home.join(".multibot/clients");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("client.tty"), "/tmp/not-a-tty\n").unwrap();
    manager
        .switch_shell_client("client".into(), "ok".into())
        .await;
    assert!(
        matches!(rx.recv().await, Some(ServerEvent::Error { code, .. }) if code == "switch_shell_client_failed")
    );
    assert!(!dir.join("client.tty").exists());
    fs::remove_dir_all(home).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn shell_client_switches_a_real_tmux_client_in_place() {
    if std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .is_err()
    {
        return;
    }
    let home = temp_home("switch-real");
    let socket = home.join("tmux.sock");
    let socket = socket.to_str().unwrap();
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::process::Command::new("tmux")
                .env_remove("TMUX")
                .args([
                    "-S",
                    self.0.join("tmux.sock").to_str().unwrap(),
                    "kill-server",
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(home.clone());
    for name in ["a", "b"] {
        assert!(
            Command::new("tmux")
                .env_remove("TMUX")
                .args([
                    "-S",
                    socket,
                    "-f",
                    "/dev/null",
                    "new-session",
                    "-d",
                    "-s",
                    name
                ])
                .status()
                .await
                .unwrap()
                .success()
        );
    }
    let registration = home.join(".multibot/clients/client.tty");
    fs::create_dir_all(registration.parent().unwrap()).unwrap();
    fs::write(&registration, "").unwrap();
    let mut client = Command::new("script")
        .env_remove("TMUX")
        .env("TERM", "xterm")
        .args([
            "-q",
            "-c",
            &format!(
                "stty rows 37 cols 109; sleep 0.1; tty > '{}' && exec tmux -S '{socket}' attach-session -t '=a'",
                registration.display()
            ),
            "/dev/null",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let clients = || async {
        let output = Command::new("tmux")
            .env_remove("TMUX")
            .args([
                "-S",
                socket,
                "list-clients",
                "-F",
                "#{client_pid} #{session_name}",
            ])
            .output()
            .await
            .unwrap();
        String::from_utf8(output.stdout).unwrap()
    };
    assert!(
        Command::new("tmux")
            .env_remove("TMUX")
            .args([
                "-S",
                socket,
                "set-window-option",
                "-t",
                "=b",
                "window-size",
                "manual"
            ])
            .status()
            .await
            .unwrap()
            .success()
    );
    switch_shell_client_at(&home, "client", "b", Some(socket))
        .await
        .unwrap();
    let size_policy = Command::new("tmux")
        .env_remove("TMUX")
        .args([
            "-S",
            socket,
            "show-window-options",
            "-t",
            "=b",
            "-v",
            "window-size",
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8(size_policy.stdout).unwrap().trim(),
        "latest"
    );
    let size = Command::new("tmux")
        .env_remove("TMUX")
        .args([
            "-S",
            socket,
            "list-windows",
            "-t",
            "=b",
            "-F",
            "#{window_width}x#{window_height}",
        ])
        .output()
        .await
        .unwrap();
    // The terminal's 37th row belongs to tmux's status bar.
    assert_eq!(String::from_utf8(size.stdout).unwrap().trim(), "109x36");

    let initial = clients().await;
    assert_eq!(initial.lines().count(), 1);
    assert!(initial.trim().ends_with(" b"));
    switch_shell_client_at(&home, "client", "a", Some(socket))
        .await
        .unwrap();
    assert_eq!(clients().await, initial.replace(" b\n", " a\n"));
    switch_shell_client_at(&home, "client", "b", Some(socket))
        .await
        .unwrap();
    assert_eq!(clients().await, initial);
    assert!(
        switch_shell_client_at(&home, "client", "does-not-exist", Some(socket))
            .await
            .is_err()
    );
    assert!(!registration.exists());
    client.kill().await.unwrap();
    client.wait().await.unwrap();
}

fn temp_home(tag: &str) -> PathBuf {
    let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("multibotd-proc-{tag}-{}-{n}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(unix)]
#[tokio::test]
async fn switch_harness_delivers_handover_to_target_only_on_first_prompt() {
    use multibot_proto::{HandoverContext, HandoverTurn, HarnessChoice, MessageRole};
    use notify::Watcher;
    use std::os::unix::fs::PermissionsExt;
    for target in HarnessChoice::defaults() {
        let home = temp_home("switch-first-prompt");
        let original = home.join("original.jsonl");
        fs::write(&original, "").unwrap();
        let previous = SessionSummary {
            id: "original".into(),
            agent: AgentKind::Claude,
            title: "Keep this chat".into(),
            cwd: Some(home.to_string_lossy().into()),
            source_path: original.to_string_lossy().into(),
            ..Default::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mgr = ProcessManager::new(home.clone(), tx);
        mgr.switch_harness(
            previous,
            target.clone(),
            HandoverContext {
                turns: vec![HandoverTurn {
                    role: MessageRole::User,
                    text: "Keep the GPU rule".into(),
                }],
            },
        )
        .unwrap();
        let ServerEvent::HarnessSwitched { session, .. } = rx.try_recv().unwrap() else {
            unreachable!()
        };
        for turn in 1..=2 {
            let captured = home.join(format!("arguments-{turn}"));
            let script = home.join(format!("fake-agent-{turn}"));
            fs::write(&script, format!(r#"#!/bin/sh
{{ printf '%s\n' "$@"; if [ "$1" = --input-format ]; then IFS= read -r input; printf '%s\n' "$input"; fi; }} > '{}.tmp'
mv '{}.tmp' '{}'
"#, captured.display(), captured.display(), captured.display())).unwrap();
            if target.agent == AgentKind::Antigravity && turn == 1 {
                let second = home.join("arguments-2");
                fs::write(
                    &script,
                    format!(
                        r#"#!/bin/sh
IFS= read -r input
printf '%s\n' "$input" > '{}.tmp'
mv '{}.tmp' '{}'
IFS= read -r input
printf '%s\n' "$input" > '{}.tmp'
mv '{}.tmp' '{}'
"#,
                        captured.display(),
                        captured.display(),
                        captured.display(),
                        second.display(),
                        second.display(),
                        second.display()
                    ),
                )
                .unwrap();
            }
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
            mgr.set_test_bin_override(Some(script));
            let (event_tx, mut event_rx) = mpsc::unbounded_channel();
            let mut watcher =
                notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                    if let Ok(event) = event {
                        let _ = event_tx.send(event);
                    }
                })
                .unwrap();
            watcher
                .watch(&home, notify::RecursiveMode::NonRecursive)
                .unwrap();
            mgr.dispatch_prompt_internal(
                &session.id,
                session.agent,
                session.cwd.as_deref(),
                "Continue now",
                false,
                None,
            )
            .unwrap();
            tokio::time::timeout(Duration::from_secs(3), async {
                while let Some(event) = event_rx.recv().await {
                    if event.paths.contains(&captured) {
                        return;
                    }
                }
                panic!("agent output watcher closed");
            })
            .await
            .unwrap();
            let args = fs::read_to_string(&captured).unwrap();
            assert!(args.contains("Continue now"), "{target:?}: {args}");
            assert_eq!(
                args.contains("Keep the GPU rule"),
                turn == 1,
                "{target:?}: {args}"
            );
            assert!(
                crate::continuity::load(&home, &session.id)
                    .unwrap()
                    .unwrap()
                    .started
            );
        }
        fs::remove_dir_all(home).unwrap();
    }
}

#[test]
fn switch_harness_preserves_metadata_history_and_rejects_busy_sessions() {
    use multibot_proto::{ChatMessage, HandoverContext, HarnessChoice};
    for target in HarnessChoice::defaults() {
        let home = temp_home("switch-harness");
        let source = home.join("original.jsonl");
        fs::write(&source, concat!(
                "{\"type\":\"user\",\"uuid\":\"u1\",\"message\":{\"content\":\"Keep the GPU rule\"}}\n",
                "{\"type\":\"assistant\",\"uuid\":\"a1\",\"message\":{\"content\":\"Understood\"}}\n"
            )).unwrap();
        let original = fs::read(&source).unwrap();
        let previous = SessionSummary {
            id: "original".into(),
            agent: AgentKind::Claude,
            title: "Release planning".into(),
            cwd: Some(home.to_string_lossy().into()),
            source_path: source.to_string_lossy().into(),
            workspace: Some("Build".into()),
            ..Default::default()
        };
        let context = HandoverContext::from_messages(&[ChatMessage {
            text: "Keep the GPU rule".into(),
            ..Default::default()
        }]);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mgr = ProcessManager::new(home.clone(), tx);
        let mut busy = previous.clone();
        busy.working = true;
        assert!(
            mgr.switch_harness(busy, target.clone(), context.clone())
                .is_err()
        );
        assert!(!ArchiveStore::load(&home).is_archived(&previous.id));
        mgr.switch_harness(previous.clone(), target.clone(), context.clone())
            .unwrap();
        let ServerEvent::HarnessSwitched {
            previous: old,
            session,
        } = rx.try_recv().unwrap()
        else {
            panic!("missing switch acknowledgement")
        };
        assert_eq!(old, previous);
        assert_eq!(session.title, previous.title);
        assert_eq!(session.cwd, previous.cwd);
        assert_eq!(session.workspace, previous.workspace);
        assert_eq!(session.agent, target.agent);
        assert_ne!(session.id, previous.id);
        let saved = crate::continuity::load(&home, &session.id)
            .unwrap()
            .unwrap();
        assert_eq!(saved.context, context);
        assert!(!saved.started);
        assert_eq!(
            MetadataStore::load(&home)
                .get(&session.id)
                .unwrap()
                .workspace,
            previous.workspace
        );
        let page = crate::continuity::read_page(
            &home,
            &session.id,
            session.agent,
            Path::new(&session.source_path),
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            page.messages
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            ["Keep the GPU rule", "Understood"]
        );
        assert_eq!(page.file_size, original.len() as u64);
        assert!(ArchiveStore::load(&home).is_archived(&previous.id));
        assert_eq!(fs::read(&source).unwrap(), original);
        mgr.set_test_bin_override(Some(home.join("missing-agent")));
        assert!(
            mgr.dispatch_prompt_internal(
                &session.id,
                session.agent,
                session.cwd.as_deref(),
                "Continue",
                false,
                None
            )
            .is_err()
        );
        assert!(
            !crate::continuity::load(&home, &session.id)
                .unwrap()
                .unwrap()
                .started
        );
        fs::remove_dir_all(home).unwrap();
    }
}

#[tokio::test]
async fn switch_harness_codex_records_native_id_and_visible_reply() {
    let home = temp_home("switch-codex-output");
    let sink = home.join(".multibot/codex/session.jsonl");
    let mut metadata = MetadataStore::default();
    metadata.sessions.insert(
        "session".into(),
        SessionMetaEntry {
            agent: Some(AgentKind::Codex),
            source_path: Some(sink.to_string_lossy().into()),
            ..Default::default()
        },
    );
    metadata.save(&home).unwrap();
    append_history_record(
        &sink,
        &json!({"type":"session_meta", "payload":{"id":"session", "cwd":home}}),
    )
    .unwrap();
    let mut child = Command::new("sh").arg("-c").arg(concat!(
            "printf '%s\n' '{\"type\":\"thread.started\",\"thread_id\":\"native-thread\"}'; ",
            "printf '%s\n' '{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"id\":\"reply\",\"text\":\"Continuing the same chat\"}}'"
        )).stdout(Stdio::piped()).spawn().unwrap();
    read_harness_output(
        child.stdout.take().unwrap(),
        &home,
        "session",
        &sink,
        true,
        AgentKind::Codex,
    )
    .await;
    assert!(child.wait().await.unwrap().success());
    assert_eq!(
        MetadataStore::load(&home)
            .get("session")
            .unwrap()
            .native_session_id
            .as_deref(),
        Some("native-thread")
    );
    let page = read_history_page(AgentKind::Codex, &sink, None, None, None, None).unwrap();
    assert_eq!(page.messages[0].text, "Continuing the same chat");
    fs::remove_dir_all(home).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn every_chat_client_reports_exit_without_a_transcript_write() {
    for agent in [
        AgentKind::Claude,
        AgentKind::Codex,
        AgentKind::Antigravity,
        AgentKind::Pi,
        AgentKind::Cursor,
    ] {
        let home = temp_home("exit-activity");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mgr = ProcessManager::new(home.clone(), tx);
        mgr.set_test_bin_override(Some(PathBuf::from("/bin/true")));
        mgr.send_prompt(
            "exit-session",
            agent,
            Some(home.to_str().unwrap()),
            "prompt",
            None,
        )
        .await
        .unwrap();
        let events = tokio::time::timeout(Duration::from_secs(5), async {
            let mut events = Vec::new();
            while let Some(event) = rx.recv().await {
                if let ServerEvent::HistoryAppend {
                    session_id,
                    messages,
                    working: Some(working),
                    ..
                } = event
                {
                    assert_eq!(session_id, "exit-session");
                    assert!(messages.is_empty());
                    events.push(working);
                    if !working {
                        break;
                    }
                }
            }
            events
        })
        .await
        .unwrap();
        assert_eq!(events, [true, false], "{agent:?}");
        fs::remove_dir_all(home).unwrap();
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn spawn_retries_executable_busy_until_writer_closes() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let home = temp_home("spawn-busy");
    let bin = home.join("fake-agent");
    let mut writer = fs::File::create(&bin).unwrap();
    writer.write_all(b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    let mut cmd = Command::new(&bin);
    assert_eq!(cmd.spawn().unwrap_err().raw_os_error(), Some(26));

    let closer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(30));
        drop(writer);
    });
    let mut child = spawn_with_retry(&mut cmd).unwrap();
    closer.join().unwrap();
    assert!(child.wait().await.unwrap().success());
    fs::remove_dir_all(home).unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn spawn_returns_executable_busy_after_retry_limit() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let home = temp_home("spawn-still-busy");
    let bin = home.join("fake-agent");
    let mut writer = fs::File::create(&bin).unwrap();
    writer.write_all(b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    let err = spawn_with_retry(&mut Command::new(&bin)).unwrap_err();
    assert_eq!(err.raw_os_error(), Some(26));
    drop(writer);
    fs::remove_dir_all(home).unwrap();
}

#[tokio::test]
async fn spawn_preserves_other_errors() {
    let home = temp_home("spawn-missing");
    let err = spawn_with_retry(&mut Command::new(home.join("missing"))).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    fs::remove_dir_all(home).unwrap();
}

#[test]
fn pi_and_cursor_dispatch_use_explicit_sessions_and_safe_prompt_separators() {
    let pi = build_pi_cursor_args(
        AgentKind::Pi,
        "session",
        Path::new("/repo/pi.jsonl"),
        None,
        "--prompt",
        false,
    )
    .unwrap();
    assert_eq!(
        pi,
        ["--print", "--session", "/repo/pi.jsonl", "--", "--prompt"]
    );
    let cursor = build_pi_cursor_args(
        AgentKind::Cursor,
        "session",
        Path::new("/repo/session.jsonl"),
        None,
        "hi",
        false,
    )
    .unwrap();
    assert!(cursor.windows(2).any(|a| a == ["--resume", "session"]));
    let new_cursor = build_pi_cursor_args(
        AgentKind::Cursor,
        "session",
        Path::new("/home/.multibot/cursor/session.jsonl"),
        None,
        "hi",
        true,
    )
    .unwrap();
    assert!(!new_cursor.iter().any(|a| a == "--resume"));
    let resumed = build_pi_cursor_args(
        AgentKind::Cursor,
        "session",
        Path::new("/home/.multibot/cursor/session.jsonl"),
        Some("native-session"),
        "hi",
        false,
    )
    .unwrap();
    assert!(
        resumed
            .windows(2)
            .any(|a| a == ["--resume", "native-session"])
    );
    assert!(
        build_pi_cursor_args(
            AgentKind::Cursor,
            "session",
            Path::new("/repo/session.jsonl"),
            Some("--bad"),
            "hi",
            false
        )
        .is_err()
    );
    assert!(
        validate_options(
            AgentKind::Pi,
            Some(&SessionOptions {
                model: Some("openai/gpt-5.4".into()),
                ..Default::default()
            })
        )
        .is_ok()
    );
    assert!(
        validate_options(
            AgentKind::Pi,
            Some(&SessionOptions {
                effort: Some(EffortLevel::High),
                ..Default::default()
            })
        )
        .is_err()
    );
    for agent in [AgentKind::Cursor] {
        assert!(
            validate_options(
                agent,
                Some(&SessionOptions {
                    model: Some("wrong".into()),
                    ..Default::default()
                })
            )
            .is_err()
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn cursor_output_is_bounded_scrubbed_and_records_native_resume_id() {
    let home = temp_home("cursor-output");
    let sink = home.join(".multibot/cursor/session.jsonl");
    let mut metadata = MetadataStore::default();
    metadata.sessions.insert(
        "session".into(),
        SessionMetaEntry {
            agent: Some(AgentKind::Cursor),
            source_path: Some(sink.to_string_lossy().into()),
            ..Default::default()
        },
    );
    metadata.save(&home).unwrap();
    let mut child = Command::new("sh").arg("-c").arg(concat!(
            "printf '%s\\n' '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"cursor-native\"}'; ",
            "head -c 300000 /dev/zero | tr '\\000' X; printf '\\n'; ",
            "printf '%s\\n' '{\"type\":\"assistant\",\"message\":{\"content\":\"done\"}}'"
        )).stdout(Stdio::piped()).spawn().unwrap();
    read_harness_output(
        child.stdout.take().unwrap(),
        &home,
        "session",
        &sink,
        true,
        AgentKind::Cursor,
    )
    .await;
    assert!(child.wait().await.unwrap().success());
    assert_eq!(
        MetadataStore::load(&home)
            .get("session")
            .unwrap()
            .native_session_id
            .as_deref(),
        Some("cursor-native")
    );
    let page = read_history_page(AgentKind::Cursor, &sink, None, None, None, None).unwrap();
    assert_eq!(page.messages.len(), 1);
    assert_eq!(page.messages[0].text, "done");
    assert!(fs::metadata(&sink).unwrap().len() < 1024);
    fs::remove_dir_all(home).unwrap();
}

#[test]
fn default_permissions_bypass_for_claude_and_codex_with_or_without_other_options() {
    let defaults = SessionOptions::default();
    let model_only = SessionOptions {
        model: Some("custom-model".into()),
        ..Default::default()
    };
    for options in [None, Some(&defaults), Some(&model_only)] {
        for is_new in [true, false] {
            let claude = build_claude_args("session", "hello", is_new, options).unwrap();
            assert!(
                claude
                    .windows(2)
                    .any(|a| a == ["--permission-mode", "bypassPermissions"])
            );
            let codex = build_codex_args("session", "hello", is_new, options).unwrap();
            assert!(codex.contains(&"sandbox_mode=\"danger-full-access\"".into()));
            assert!(codex.contains(&"approval_policy=\"never\"".into()));
        }
    }
}

#[test]
fn permission_overrides_are_accepted_for_all_agent_harnesses() {
    for agent in [
        AgentKind::Claude,
        AgentKind::Codex,
        AgentKind::Pi,
        AgentKind::Cursor,
        AgentKind::Antigravity,
    ] {
        for permission in [
            PermissionLevel::FullAccess,
            PermissionLevel::Auto,
            PermissionLevel::Ask,
        ] {
            let options = SessionOptions {
                permission: Some(permission),
                ..Default::default()
            };
            assert!(
                validate_options(agent, Some(&options)).is_ok(),
                "{agent:?}: {permission:?}"
            );
        }
    }
    for agent in [AgentKind::Pi, AgentKind::Cursor, AgentKind::Antigravity] {
        let options = SessionOptions {
            effort: Some(EffortLevel::High),
            permission: Some(PermissionLevel::FullAccess),
            ..Default::default()
        };
        assert!(validate_options(agent, Some(&options)).is_err());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn antigravity_and_cursor_only_bypass_permissions_when_requested_or_defaulted() {
    use std::os::unix::fs::PermissionsExt;
    for agent in [AgentKind::Antigravity, AgentKind::Cursor, AgentKind::Pi] {
        for permission in [
            None,
            Some(PermissionLevel::FullAccess),
            Some(PermissionLevel::Auto),
            Some(PermissionLevel::Ask),
        ] {
            let home = temp_home("permission-args");
            let bin = home.join("fake-agent");
            let log = home.join("argv");
            fs::write(&bin, format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nif [ \"$1\" = --input-format ]; then IFS= read -r input; fi\n", log.display())).unwrap();
            fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
            let (tx, mut rx) = mpsc::unbounded_channel();
            let mgr = ProcessManager::new(home.clone(), tx);
            mgr.set_test_bin_override(Some(bin));
            let options = permission.map(|permission| SessionOptions {
                permission: Some(permission),
                ..Default::default()
            });
            mgr.dispatch_prompt_internal(
                "session",
                agent,
                Some(home.to_str().unwrap()),
                "hello",
                true,
                options.as_ref(),
            )
            .unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                while let Some(event) = rx.recv().await {
                    if matches!(
                        event,
                        ServerEvent::HistoryAppend {
                            working: Some(false),
                            ..
                        }
                    ) {
                        return;
                    }
                }
                panic!("missing exit event");
            })
            .await
            .unwrap();
            let args = fs::read_to_string(log).unwrap();
            let bypass = matches!(permission, None | Some(PermissionLevel::FullAccess));
            if agent == AgentKind::Antigravity {
                assert_eq!(
                    args.lines().any(|a| a == "--dangerously-skip-permissions"),
                    bypass
                );
            } else if agent == AgentKind::Cursor {
                assert_eq!(args.lines().any(|a| a == "--force"), bypass);
            } else {
                assert!(!args.contains("--force"));
                assert!(!args.contains("--dangerously-skip-permissions"));
            }
            assert!(args.ends_with("--\nhello\n") || agent == AgentKind::Antigravity);
            fs::remove_dir_all(home).unwrap();
        }
    }
}

#[test]
fn composer_options_map_to_provider_flags_before_prompt_separator() {
    let options = SessionOptions {
        model: Some("opus".into()),
        effort: Some(EffortLevel::High),
        permission: Some(PermissionLevel::Auto),
    };
    for is_new in [true, false] {
        let args = build_claude_args("session", "--unsafe-looking prompt", is_new, Some(&options))
            .unwrap();
        assert!(args.windows(2).any(|a| a == ["--model", "opus"]));
        assert!(args.windows(2).any(|a| a == ["--effort", "high"]));
        assert!(
            args.windows(2)
                .any(|a| a == ["--permission-mode", "acceptEdits"])
        );
        assert_eq!(&args[args.len() - 2..], ["--", "--unsafe-looking prompt"]);
        for (permission, sandbox, approval) in [
            (PermissionLevel::Ask, "read-only", "on-request"),
            (PermissionLevel::Auto, "workspace-write", "on-request"),
            (PermissionLevel::FullAccess, "danger-full-access", "never"),
        ] {
            let options = SessionOptions {
                model: Some("gpt-5.3-codex".into()),
                effort: Some(EffortLevel::Max),
                permission: Some(permission),
            };
            let args = build_codex_args("session", "hello", is_new, Some(&options)).unwrap();
            assert!(args.windows(2).any(|a| a == ["--model", "gpt-5.3-codex"]));
            assert!(args.contains(&"model_reasoning_effort=\"xhigh\"".to_string()));
            assert!(args.contains(&format!("sandbox_mode=\"{sandbox}\"")));
            assert!(args.contains(&format!("approval_policy=\"{approval}\"")));
            assert_eq!(&args[args.len() - 2..], ["--", "hello"]);
        }
    }
    for (permission, mode) in [
        (PermissionLevel::Ask, "default"),
        (PermissionLevel::FullAccess, "bypassPermissions"),
    ] {
        let options = SessionOptions {
            permission: Some(permission),
            ..Default::default()
        };
        let args = build_claude_args("session", "hello", false, Some(&options)).unwrap();
        assert!(args.windows(2).any(|a| a == ["--permission-mode", mode]));
    }
}

#[tokio::test]
async fn composer_unsupported_options_fail_without_spawning_or_writing_metadata() {
    let home = temp_home("unsupported-options");
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mgr = ProcessManager::new(home.clone(), tx);
    let options = SessionOptions {
        model: Some("opus".into()),
        ..Default::default()
    };
    assert!(
        mgr.send_prompt(
            "s1",
            AgentKind::Antigravity,
            None,
            "hello",
            Some(options.clone())
        )
        .await
        .is_err()
    );
    mgr.spawn_session(
        AgentKind::Antigravity,
        home.to_string_lossy().into(),
        None,
        Some("hello".into()),
        Some(options),
    )
    .await;
    assert!(
        matches!(rx.recv().await, Some(ServerEvent::Error { code, .. }) if code == "invalid_options")
    );
    assert!(MetadataStore::load(&home).sessions.is_empty());
    assert!(mgr.active_session_ids().is_empty());
    for model in ["", "--help", "bad\nmodel"] {
        let options = SessionOptions {
            model: Some(model.into()),
            ..Default::default()
        };
        assert!(
            mgr.send_prompt("s1", AgentKind::Claude, None, "hello", Some(options))
                .await
                .is_err()
        );
    }
    let _ = fs::remove_dir_all(&home);
}

#[tokio::test]
async fn done_keeps_process_alive_and_dissolve_stops_it_without_touching_history() {
    for agent in [AgentKind::Claude, AgentKind::Codex] {
        let home = temp_home(&format!("dissolve-process-{agent:?}"));
        fs::create_dir_all(&home).unwrap();
        let transcript = home.join("saved.jsonl");
        let original = b"{\"role\":\"assistant\",\"text\":\"saved history\"}\n";
        fs::write(&transcript, original).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mgr = ProcessManager::new(home.clone(), tx);
        let mut child = Command::new("sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        mgr.state.lock().unwrap().active.insert(
            "saved".into(),
            ActiveProcess {
                pid,
                cwd: None,
                source_path: transcript.clone(),
                stdin_tx: None,
                busy: true,
                interrupted: false,
            },
        );
        mgr.archive_session(
            "saved".into(),
            agent,
            transcript.to_string_lossy().into(),
            false,
        )
        .await;
        assert!(child.try_wait().unwrap().is_none());
        assert!(mgr.active_session_ids().contains("saved"));
        assert!(!ArchiveStore::load(&home).archived["saved"].kill_process);
        mgr.archive_session(
            "saved".into(),
            agent,
            transcript.to_string_lossy().into(),
            true,
        )
        .await;
        assert!(!mgr.active_session_ids().contains("saved"));
        assert!(!child.wait().await.unwrap().success());
        let saved = ArchiveStore::load(&home);
        assert!(saved.archived["saved"].kill_process);
        assert_eq!(
            saved.archived["saved"].source_path.as_deref(),
            transcript.to_str()
        );
        assert_eq!(fs::read(&transcript).unwrap(), original);
        mgr.unarchive_session("saved".into());
        assert!(!ArchiveStore::load(&home).is_archived("saved"));
        assert_eq!(fs::read(&transcript).unwrap(), original);
        let _ = fs::remove_dir_all(&home);
    }
}

#[tokio::test]
async fn rename_archive_unarchive_preserves_jsonl_and_validates_title() {
    let home = temp_home("lifecycle");
    let transcript = home.join(".claude/projects/demo/s1.jsonl");
    fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    let original_body =
        r#"{"type":"user","uuid":"u1","message":{"content":"hello"}}"#.to_string() + "\n";
    fs::write(&transcript, &original_body).unwrap();

    let (tx, mut rx) = mpsc::unbounded_channel();
    let mgr = ProcessManager::new(home.clone(), tx);

    // Blank title rejected
    mgr.rename_session(
        "s1".into(),
        AgentKind::Claude,
        transcript.to_string_lossy().into(),
        "   \n\t ".into(),
    );
    match rx.recv().await.unwrap() {
        ServerEvent::Error { code, .. } => assert_eq!(code, "invalid_title"),
        other => panic!("expected invalid_title error, got {other:?}"),
    }

    // >63 chars capped to 63 and persisted atomically
    let long_title = format!("  {}  ", "A".repeat(80));
    mgr.rename_session(
        "s1".into(),
        AgentKind::Claude,
        transcript.to_string_lossy().into(),
        long_title,
    );
    match rx.recv().await.unwrap() {
        ServerEvent::SessionUpdated { session } => {
            assert_eq!(session.title.chars().count(), MAX_SESSION_TITLE_CHARS);
            assert_eq!(session.title, "A".repeat(MAX_SESSION_TITLE_CHARS));
        }
        other => panic!("expected SessionUpdated, got {other:?}"),
    }

    // Archive preserves transcript untouched
    mgr.archive_session(
        "s1".into(),
        AgentKind::Claude,
        transcript.to_string_lossy().into(),
        false,
    )
    .await;
    match rx.recv().await.unwrap() {
        ServerEvent::SessionArchived {
            session_id,
            archived,
        } => {
            assert_eq!(session_id, "s1");
            assert!(archived);
        }
        other => panic!("expected SessionArchived, got {other:?}"),
    }
    assert_eq!(fs::read_to_string(&transcript).unwrap(), original_body);
    assert!(ArchiveStore::load(&home).is_archived("s1"));

    // Unarchive clears flag
    mgr.unarchive_session("s1".into());
    match rx.recv().await.unwrap() {
        ServerEvent::SessionArchived {
            session_id,
            archived,
        } => {
            assert_eq!(session_id, "s1");
            assert!(!archived);
        }
        other => panic!("expected SessionArchived(false), got {other:?}"),
    }
    assert!(!ArchiveStore::load(&home).is_archived("s1"));

    let _ = fs::remove_dir_all(&home);
}

#[tokio::test]
async fn spawn_session_validates_cwd_and_creates_agy_history() {
    let home = temp_home("spawn");
    let workdir = home.join("project");
    fs::create_dir_all(&workdir).unwrap();

    let (tx, mut rx) = mpsc::unbounded_channel();
    let mgr = ProcessManager::new(home.clone(), tx);

    // Reject '..' and nonexistent dir
    mgr.spawn_session(
        AgentKind::Antigravity,
        "/tmp/../etc".into(),
        None,
        None,
        None,
    )
    .await;
    match rx.recv().await.unwrap() {
        ServerEvent::Error { code, .. } => assert_eq!(code, "invalid_cwd"),
        other => panic!("expected invalid_cwd, got {other:?}"),
    }

    mgr.spawn_session(
        AgentKind::Antigravity,
        home.join("does-not-exist").to_string_lossy().into(),
        None,
        None,
        None,
    )
    .await;
    match rx.recv().await.unwrap() {
        ServerEvent::Error { code, .. } => assert_eq!(code, "invalid_cwd"),
        other => panic!("expected invalid_cwd, got {other:?}"),
    }

    // Valid spawn without initial_prompt creates daemon-owned history file
    mgr.spawn_session(
        AgentKind::Antigravity,
        workdir.to_string_lossy().into(),
        Some("My Agy Chat".into()),
        None,
        None,
    )
    .await;
    match rx.recv().await.unwrap() {
        ServerEvent::SessionSpawned { session } => {
            assert_eq!(session.agent, AgentKind::Antigravity);
            assert_eq!(session.title, "My Agy Chat");
            assert!(!session.working);
            assert!(Path::new(&session.source_path).is_file());
            assert!(
                Path::new(&session.source_path).starts_with(home.join(".multibot").join("history"))
            );
        }
        other => panic!("expected SessionSpawned, got {other:?}"),
    }

    let _ = fs::remove_dir_all(&home);
}

#[cfg(unix)]
#[tokio::test]
async fn interrupt_unowned_returns_false_and_real_child_receives_sigint() {
    let home = temp_home("interrupt");
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mgr = ProcessManager::new(home.clone(), tx);

    // Unowned session returns ok: false cleanly
    mgr.interrupt_session("unowned-1".into(), AgentKind::Claude)
        .await;
    match rx.recv().await.unwrap() {
        ServerEvent::SessionInterrupted {
            session_id,
            ok,
            detail,
        } => {
            assert_eq!(session_id, "unowned-1");
            assert!(!ok);
            assert!(detail.contains("no active"));
        }
        other => panic!("expected SessionInterrupted, got {other:?}"),
    }

    // Track a real `/bin/sleep 30` child and verify real SIGINT interrupts it!
    let child = Command::new("sleep")
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    mgr.register_one_shot_child("live-sleep".into(), None, home.join("live.jsonl"), child);
    assert!(mgr.is_session_busy("live-sleep"));

    // Queue two prompts while busy, cancel the first one
    mgr.steer_session(
        "live-sleep".into(),
        AgentKind::Claude,
        None,
        "first queued".into(),
        None,
    )
    .await;
    let q1_id = match rx.recv().await.unwrap() {
        ServerEvent::PromptStatus {
            message_id, status, ..
        } => {
            assert_eq!(status, "queued");
            message_id
        }
        other => panic!("expected PromptStatus(queued), got {other:?}"),
    };

    mgr.cancel_queued_prompt("live-sleep".into(), q1_id.clone());
    match rx.recv().await.unwrap() {
        ServerEvent::PromptStatus {
            message_id, status, ..
        } => {
            assert_eq!(message_id, q1_id);
            assert_eq!(status, "cancelled");
        }
        other => panic!("expected PromptStatus(cancelled), got {other:?}"),
    }

    // Cancelling missing prompt yields not_found
    mgr.cancel_queued_prompt("live-sleep".into(), "missing-id".into());
    match rx.recv().await.unwrap() {
        ServerEvent::PromptStatus { status, .. } => assert_eq!(status, "not_found"),
        other => panic!("expected PromptStatus(not_found), got {other:?}"),
    }

    // Interrupt real child with SIGINT
    tokio::time::timeout(Duration::from_secs(2), async {
        mgr.interrupt_session("live-sleep".into(), AgentKind::Claude)
            .await;
        loop {
            match rx.recv().await.unwrap() {
                ServerEvent::SessionInterrupted {
                    session_id,
                    ok,
                    detail,
                } => {
                    assert_eq!(session_id, "live-sleep");
                    assert!(ok);
                    assert!(detail.contains("SIGINT"));
                    break;
                }
                // The child reaper may publish completion before the interrupt reply.
                ServerEvent::SessionUpdated { .. }
                | ServerEvent::HistoryAppend { .. }
                | ServerEvent::PromptStatus { .. } => {}
                other => panic!("expected SessionInterrupted(ok), got {other:?}"),
            }
        }
    })
    .await
    .expect("SessionInterrupted(ok) did not arrive within 2s");

    tokio::time::timeout(Duration::from_secs(2), async {
        while mgr.is_session_busy("live-sleep") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("interrupted child was not reaped within 2s");
    assert!(!mgr.is_session_busy("live-sleep"));

    let _ = fs::remove_dir_all(&home);
}

#[cfg(unix)]
#[tokio::test]
async fn interrupted_process_closes_stdin_and_force_prompt_resumes_first() {
    for agent in [AgentKind::Antigravity, AgentKind::Claude] {
        let home = temp_home("force-after-interrupt");
        let (bin, log) = fake_agy(&home);
        let (tx, _rx) = mpsc::unbounded_channel();
        let mgr = ProcessManager::new(home.clone(), tx);
        mgr.set_test_bin_override(Some(bin));
        // Keep reaping under test control, so SendPrompt always races the old child.
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id().unwrap();
        let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel();
        mgr.state.lock().unwrap().active.insert(
            "force-resume".into(),
            ActiveProcess {
                pid,
                cwd: Some(home.to_string_lossy().into()),
                source_path: hosted_history_path(&home, "force-resume"),
                stdin_tx: Some(stdin_tx),
                busy: true,
                interrupted: false,
            },
        );
        assert!(mgr.session_accepts_live_stdin("force-resume"));
        mgr.state
            .lock()
            .unwrap()
            .queues
            .entry("force-resume".into())
            .or_default()
            .push_back(QueuedPrompt {
                message_id: "older".into(),
                agent,
                cwd: None,
                text: "older queued prompt".into(),
                options: None,
            });
        mgr.interrupt_session("force-resume".into(), agent).await;
        assert!(!mgr.session_accepts_live_stdin("force-resume"));
        assert_eq!(stdin_rx.recv().await, None, "interrupted stdin must close");
        let result = mgr
            .send_prompt(
                "force-resume",
                agent,
                home.to_str(),
                "force this first",
                None,
            )
            .await
            .unwrap();
        if agent == AgentKind::Antigravity {
            assert!(result.starts_with("dispatched agy"));
            assert_eq!(mgr.state.lock().unwrap().queues["force-resume"].len(), 1);
            assert!(!mgr.state.lock().unwrap().active["force-resume"].interrupted);
            mgr.on_process_exit("force-resume", pid);
            assert_ne!(mgr.state.lock().unwrap().active["force-resume"].pid, pid);
        } else {
            assert_eq!(result, "queued behind active turn");
            let mut state = mgr.state.lock().unwrap();
            assert_eq!(state.queues["force-resume"][0].text, "force this first");
            assert_eq!(state.queues["force-resume"][1].text, "older queued prompt");
            assert!(state.active["force-resume"].interrupted);
            // Even a late turn-end must not permit queue draining before exit.
            state.active.get_mut("force-resume").unwrap().busy = false;
        }
        if agent != AgentKind::Antigravity {
            mgr.drain_next_queued("force-resume");
            assert_eq!(mgr.state.lock().unwrap().queues["force-resume"].len(), 2);
        }
        tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .unwrap()
            .unwrap();
        mgr.on_process_exit("force-resume", pid);
        let needle = if agent == AgentKind::Antigravity {
            "STDIN "
        } else {
            "ARGV "
        };
        assert!(
            wait_for_log_line(&log, needle)
                .await
                .contains("force this first")
        );
        if let Some(active) = mgr.state.lock().unwrap().active.get("force-resume") {
            assert_ne!(active.pid, pid);
            assert!(!active.interrupted);
        }
        mgr.archive_session("force-resume".into(), agent, "".into(), true)
            .await;
        fs::remove_dir_all(home).unwrap();
    }
}

#[test]
fn agy_stream_writer_persists_and_parses_back_rich_messages() {
    let home = temp_home("agy-writer");
    let sid = "agy-hosted-1";
    let hp = hosted_history_path(&home, sid);
    append_agy_user_row(&hp, sid, Some("/repo"), "Run build", 1).unwrap();

    let mut acc = AgyStreamAccumulator::new();
    let events = vec![
        json!({"event":"init","conversation_id":"conv-99","model":"gemini-4"}),
        json!({"event":"step_update","step_update":{"step_type":"tool","step_index":1,"state":"RUNNING","tool_name":"bash","tool_info":{"parameters":{"command":"cargo build"}}}}),
        json!({"event":"step_update","step_update":{"step_type":"tool","step_index":1,"state":"DONE","tool_name":"bash","tool_info":{"output":"Finished dev"}}}),
        json!({"event":"step_update","step_update":{"step_type":"agent_response","step_index":2,"state":"DONE","text_delta":"Build succeeded"}}),
        json!({"event":"result","result":{"status":"SUCCESS","conversation_id":"conv-99"}}),
    ];
    let mut seq = 2u64;
    let mut saw_end = false;
    for ev in events {
        for out in acc.feed_value(&ev, Some(1000), None) {
            if apply_agy_stream_output(&home, sid, &hp, out, seq) {
                saw_end = true;
            }
            seq += 1;
        }
    }
    assert!(saw_end);
    assert_eq!(
        MetadataStore::load(&home)
            .get(sid)
            .and_then(|e| e.native_session_id.as_deref()),
        Some("conv-99")
    );

    let page = read_history_page(AgentKind::Antigravity, &hp, None, None, None, None).unwrap();
    assert_eq!(page.messages.len(), 3);
    assert_eq!(page.messages[0].text, "Run build");
    assert_eq!(page.messages[2].text, "Build succeeded");

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn cli_arg_builders_insert_double_dash_and_reject_leading_dash_session_id() {
    let c_new = build_claude_args("sess-1", "--help", true, None).unwrap();
    assert_eq!(c_new[c_new.len() - 2], "--");
    assert_eq!(c_new[c_new.len() - 1], "--help");

    let c_res = build_claude_args("sess-1", "- bullet item", false, None).unwrap();
    assert_eq!(c_res[0], "--resume");
    assert_eq!(c_res[c_res.len() - 2], "--");
    assert_eq!(c_res[c_res.len() - 1], "- bullet item");

    assert!(build_claude_args("-bad", "hello", false, None).is_err());
    assert!(build_claude_args("", "hello", true, None).is_err());

    let x_new = build_codex_args("ignored", "- fix tests", true, None).unwrap();
    assert_eq!(x_new[0], "exec");
    assert_eq!(&x_new[x_new.len() - 2..], ["--", "- fix tests"]);

    let x_res = build_codex_args("codex-1", "--version", false, None).unwrap();
    assert_eq!(&x_res[..3], ["exec", "resume", "codex-1"]);
    assert_eq!(&x_res[x_res.len() - 2..], ["--", "--version"]);

    assert!(build_codex_args("--evil", "hello", false, None).is_err());
}

#[tokio::test]
async fn spawn_session_antigravity_failure_cleans_up_orphaned_history_file() {
    let home = temp_home("agy-rollback");
    let workdir = home.join("project");
    fs::create_dir_all(&workdir).unwrap();

    let (tx, mut rx) = mpsc::unbounded_channel();
    let mgr = ProcessManager::new(home.clone(), tx);
    mgr.set_test_bin_override(Some(home.join("nonexistent-agy-binary")));

    mgr.spawn_session(
        AgentKind::Antigravity,
        workdir.to_string_lossy().into(),
        Some("ShouldRollback".into()),
        Some("hello".into()),
        None,
    )
    .await;

    match rx.recv().await.unwrap() {
        ServerEvent::Error { code, .. } => assert_eq!(code, "spawn_failed"),
        other => panic!("expected spawn_failed error, got {other:?}"),
    }

    // Verify ZERO orphaned .history.jsonl files and ZERO leaked sessions in ListSessions
    let hist_dir = home.join(".multibot/history");
    if hist_dir.is_dir() {
        let remaining: Vec<_> = fs::read_dir(&hist_dir).unwrap().flatten().collect();
        assert!(
            remaining.is_empty(),
            "failed Antigravity spawn leaked history files: {remaining:?}"
        );
    }
    assert!(MetadataStore::load(&home).sessions.is_empty());
    assert!(crate::providers::discover_all_sessions(&home, 10).is_empty());

    let _ = fs::remove_dir_all(&home);
}

#[cfg(unix)]
#[tokio::test]
async fn drain_next_queued_continues_past_failed_prompt() {
    let home = temp_home("drain-continue");
    let valid_cwd = home.join("valid-cwd");
    fs::create_dir_all(&valid_cwd).unwrap();

    let (tx, mut rx) = mpsc::unbounded_channel();
    let mgr = ProcessManager::new(home.clone(), tx);
    // Use `/bin/true` (always present on Linux) so valid cwd succeeds while bad cwd fails spawn
    mgr.set_test_bin_override(Some(PathBuf::from("/bin/true")));

    // Seed two queued prompts: #1 has nonexistent cwd (dispatch fails), #2 has valid cwd (dispatch succeeds)
    {
        let mut guard = mgr.state.lock().unwrap();
        let q = guard.queues.entry("sess-drain".to_string()).or_default();
        q.push_back(QueuedPrompt {
            message_id: "q-bad".into(),
            agent: AgentKind::Claude,
            cwd: Some(home.join("does-not-exist").to_string_lossy().into()),
            text: "will fail".into(),
            options: None,
        });
        q.push_back(QueuedPrompt {
            message_id: "q-good".into(),
            agent: AgentKind::Claude,
            cwd: Some(valid_cwd.to_string_lossy().into()),
            text: "will succeed".into(),
            options: None,
        });
    }

    mgr.drain_next_queued("sess-drain");

    let ev1 = rx.recv().await.expect("first PromptStatus");
    match ev1 {
        ServerEvent::PromptStatus {
            message_id, status, ..
        } => {
            assert_eq!(message_id, "q-bad");
            assert_eq!(status, "failed");
        }
        other => panic!("expected failed status for q-bad, got {other:?}"),
    }

    assert!(matches!(rx.recv().await, Some(ServerEvent::HistoryAppend {
            working: Some(true), messages, ..
        }) if messages.is_empty()));
    let ev2 = rx.recv().await.expect("second PromptStatus");
    match ev2 {
        ServerEvent::PromptStatus {
            message_id, status, ..
        } => {
            assert_eq!(message_id, "q-good");
            assert_eq!(status, "sent");
        }
        other => panic!("expected sent status for q-good, got {other:?}"),
    }

    let _ = fs::remove_dir_all(&home);
}

#[cfg(unix)]
#[tokio::test]
async fn agy_stale_stdin_fallback_does_not_duplicate_user_history_row() {
    let home = temp_home("agy-dedup");
    let sid = "agy-stale-1";
    let hp = hosted_history_path(&home, sid);
    let (tx, _rx) = mpsc::unbounded_channel();
    let mgr = ProcessManager::new(home.clone(), tx);
    mgr.set_test_bin_override(Some(PathBuf::from("/bin/true")));

    // Insert stale ActiveProcess whose stdin_rx is already dropped
    let (stale_tx, stale_rx) = mpsc::unbounded_channel::<String>();
    drop(stale_rx);
    {
        let mut guard = mgr.state.lock().unwrap();
        guard.active.insert(
            sid.to_string(),
            ActiveProcess {
                pid: 999_999,
                cwd: None,
                source_path: hp.clone(),
                stdin_tx: Some(stale_tx),
                busy: false,
                interrupted: false,
            },
        );
    }

    mgr.send_prompt(sid, AgentKind::Antigravity, None, "single message", None)
        .await
        .unwrap();

    let page = read_history_page(AgentKind::Antigravity, &hp, None, None, None, None).unwrap();
    assert_eq!(
        page.messages.len(),
        1,
        "stale stdin_tx fallback must write user row exactly once, got {:?}",
        page.messages
    );
    assert_eq!(page.messages[0].text, "single message");

    let _ = fs::remove_dir_all(&home);
}

/// Writes a stand-in `agy` that behaves like the real CLI in stream-json mode: it answers
/// every stdin frame with a turn and records each frame plus its own argv.
///
/// The real CLI keeps reading stdin after a turn ends and while background tools run, which
/// is exactly the state where a chat looks like it "cannot take new messages".
#[cfg(unix)]
fn fake_agy(home: &Path) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let bin = home.join("fake-agy");
    let log = home.join("fake-agy.log");
    let mut script = String::from("#!/bin/sh\n");
    script.push_str(&format!(
        "printf 'ARGV %s\\n' \"$*\" >> {}\n",
        log.display()
    ));
    // Echo the conversation the CLI was resumed on, like the real `agy` init event does.
    script.push_str("conv=fake-conv\n");
    script.push_str("while [ $# -gt 0 ]; do\n");
    script.push_str("  if [ \"$1\" = \"--conversation\" ]; then conv=\"$2\"; fi\n");
    script.push_str("  shift\n");
    script.push_str("done\n");
    script.push_str("printf '{\"event\":\"init\",\"conversation_id\":\"%s\"}\\n' \"$conv\"\n");
    script.push_str("while IFS= read -r line; do\n");
    script.push_str(&format!(
        "  printf 'STDIN %s\\n' \"$line\" >> {}\n",
        log.display()
    ));
    script.push_str(
            "  printf '{\"event\":\"step_update\",\"step_update\":{\"step_type\":\"agent_response\",\"step_index\":1,\"state\":\"DONE\",\"text_delta\":\"reply\"}}\\n'\n",
        );
    script.push_str("  printf '{\"event\":\"result\",\"result\":{\"status\":\"SUCCESS\"}}\\n'\n");
    script.push_str("done\n");
    fs::write(&bin, script).unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    (bin, log)
}

/// Waits for the process that owns the log to write `needle`.
///
/// The child runs on its own process and its stdin writer is a task on this runtime, so the
/// wait has to yield. A busy machine can take a few seconds to schedule either one.
#[cfg(unix)]
async fn wait_for_log_line(log: &Path, needle: &str) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        let body = fs::read_to_string(log).unwrap_or_default();
        if let Some(line) = body.lines().find(|l| l.contains(needle)) {
            return line.to_string();
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "never saw {needle:?} in {}: {:?}",
        log.display(),
        fs::read_to_string(log).unwrap_or_default()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn new_agy_session_first_prompt_uses_hosted_path_and_records_conversation() {
    for initial in [false, true] {
        let home = temp_home("agy-first-prompt");
        let (bin, log) = fake_agy(&home);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mgr = ProcessManager::new(home.clone(), tx);
        mgr.set_test_bin_override(Some(bin));
        mgr.spawn_session(
            AgentKind::Antigravity,
            home.to_string_lossy().into(),
            Some("New AGY".into()),
            initial.then(|| "first message".into()),
            None,
        )
        .await;
        let ServerEvent::SessionSpawned { session } = rx.recv().await.unwrap() else {
            panic!("new AGY session must spawn")
        };
        let path = Path::new(&session.source_path);
        assert_eq!(path, hosted_history_path(&home, &session.id));
        if !initial {
            assert!(
                read_history_page(AgentKind::Antigravity, path, None, None, None, None)
                    .unwrap()
                    .messages
                    .is_empty()
            );
            mgr.send_prompt(
                &session.id,
                AgentKind::Antigravity,
                session.cwd.as_deref(),
                "first message",
                None,
            )
            .await
            .unwrap();
        }
        let argv = wait_for_log_line(&log, "ARGV ").await;
        assert!(
            !argv.contains("--conversation"),
            "first prompt must create a conversation"
        );
        assert!(
            wait_for_log_line(&log, "STDIN ")
                .await
                .contains("first message")
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let page = read_history_page(AgentKind::Antigravity, path, None, None, None, None)
                    .unwrap();
                if page.messages.iter().any(|m| m.text == "reply") {
                    assert_eq!(
                        page.messages
                            .iter()
                            .filter(|m| m.text == "first message")
                            .count(),
                        1
                    );
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            MetadataStore::load(&home)
                .get(&session.id)
                .unwrap()
                .native_session_id
                .as_deref(),
            Some("fake-conv")
        );
        mgr.archive_session(
            session.id,
            AgentKind::Antigravity,
            session.source_path,
            true,
        )
        .await;
        fs::remove_dir_all(home).unwrap();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn hosted_agy_reports_turn_complete_while_the_process_stays_connected() {
    let home = temp_home("agy-turn-activity");
    let (bin, _) = fake_agy(&home);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mgr = ProcessManager::new(home.clone(), tx);
    mgr.set_test_bin_override(Some(bin));
    mgr.send_prompt("turn-session", AgentKind::Antigravity, None, "prompt", None)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = rx.recv().await {
            if matches!(
                event,
                ServerEvent::HistoryAppend {
                    working: Some(false),
                    ..
                }
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(mgr.connected_session_ids().contains("turn-session"));
    assert!(!mgr.is_session_busy("turn-session"));
    mgr.archive_session(
        "turn-session".into(),
        AgentKind::Antigravity,
        hosted_history_path(&home, "turn-session")
            .to_string_lossy()
            .into(),
        true,
    )
    .await;
    fs::remove_dir_all(home).unwrap();
}

/// A follow-up typed while the turn is still running must reach the live `agy` stdin.
/// Queueing it behind the turn is what makes an Antigravity chat look stuck.
#[cfg(unix)]
#[tokio::test]
async fn steer_session_writes_to_live_agy_stdin_while_busy() {
    let home = temp_home("agy-steer-live");
    let (bin, log) = fake_agy(&home);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mgr = ProcessManager::new(home.clone(), tx);
    mgr.set_test_bin_override(Some(bin));

    mgr.send_prompt("agy-1", AgentKind::Antigravity, None, "first message", None)
        .await
        .unwrap();
    assert!(mgr.is_session_busy("agy-1"));
    wait_for_log_line(&log, "STDIN ").await;

    mgr.steer_session(
        "agy-1".into(),
        AgentKind::Antigravity,
        None,
        "follow-up while working".into(),
        None,
    )
    .await;

    loop {
        match rx.recv().await.unwrap() {
            ServerEvent::PromptStatus { status, detail, .. } => {
                assert_eq!(status, "sent", "follow-up must not be queued: {detail}");
                break;
            }
            ServerEvent::HistoryAppend {
                messages,
                working: Some(_),
                ..
            } if messages.is_empty() => {}
            other => panic!("expected PromptStatus(sent), got {other:?}"),
        }
    }

    let frame = wait_for_log_line(&log, "follow-up while working").await;
    let payload = frame
        .split_once("STDIN ")
        .map(|(_, rest)| rest)
        .expect("log line carries the stdin frame");
    let parsed: serde_json::Value = serde_json::from_str(payload).unwrap();
    assert_eq!(parsed["event"], "user");
    assert_eq!(parsed["message"]["role"], "user");
    assert_eq!(parsed["message"]["content"], "follow-up while working");
    assert!(
        !mgr.state.lock().unwrap().queues.contains_key("agy-1"),
        "the follow-up was trapped in the queue"
    );

    let _ = fs::remove_dir_all(&home);
}

/// Resuming a chat the daemon never spawned still hands its uuid to `agy`, and its replies
/// land in the transcript the desktop is watching instead of a parallel hosted file.
#[cfg(unix)]
#[tokio::test]
async fn resumed_brain_conversation_passes_conversation_flag_and_writes_watched_transcript() {
    let home = temp_home("agy-resume");
    let (bin, log) = fake_agy(&home);
    let conversation = "4d5d5535-249c-4f10-90d9-7ec49c2eeb53";
    let transcript = home
        .join(".gemini/antigravity-cli/brain")
        .join(conversation)
        .join(".system_generated/logs/transcript.jsonl");
    fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    fs::write(&transcript, "").unwrap();

    let (tx, _rx) = mpsc::unbounded_channel();
    let mgr = ProcessManager::new(home.clone(), tx);
    mgr.set_test_bin_override(Some(bin));

    mgr.send_prompt(
        conversation,
        AgentKind::Antigravity,
        None,
        "resume me",
        None,
    )
    .await
    .unwrap();

    let argv = wait_for_log_line(&log, "ARGV ").await;
    assert!(
        argv.contains(&format!("--conversation {conversation}")),
        "agy was not resumed on the existing conversation: {argv}"
    );

    let reply = wait_for_log_line(&log, "STDIN ").await;
    assert!(reply.contains("resume me"));

    // The daemon must not create a second, unwatched hosted history for this chat.
    let hosted = hosted_history_path(&home, conversation);
    assert!(
        !hosted.exists(),
        "resumed native conversation also wrote {}",
        hosted.display()
    );
    assert!(transcript.is_file());

    // The resumed chat stays in the sidebar after its process exits.
    let recorded = MetadataStore::load(&home);
    assert_eq!(
        recorded
            .get(conversation)
            .and_then(|e| e.native_session_id.as_deref()),
        Some(conversation)
    );
    let connected = mgr.connected_session_ids();
    let archive = ArchiveStore::load(&home);
    let live = crate::providers::LiveActivity::load(&home);
    let server_workspaces = crate::server_workspaces::ServerWorkspaceIndex::default();
    let ctx = DiscoveryContext {
        active_session_ids: &connected,
        active_session_pids: &std::collections::HashMap::new(),
        connected_session_ids: &connected,
        metadata: &recorded,
        archive: &archive,
        live: &live,
        server_workspaces: &server_workspaces,
        include_archived: false,
    };
    let listed = crate::providers::discover_all_sessions_with_ctx(&home, 50, &ctx);
    assert_eq!(
        listed.iter().filter(|s| s.id == conversation).count(),
        1,
        "resumed conversation must list exactly once: {listed:?}"
    );

    let _ = fs::remove_dir_all(&home);
}

/// A finished reply must clear `working` before `event == "result"`, which can wait on
/// background tasks for minutes.
#[tokio::test]
async fn completed_agent_response_ends_turn_before_result_event() {
    let home = temp_home("agy-early-end");
    let sid = "agy-early-1";
    let hp = hosted_history_path(&home, sid);
    let mut acc = AgyStreamAccumulator::new();

    let mut ended = false;
    for ev in [
        json!({"event":"step_update","step_update":{"step_type":"agent_response","step_index":1,"state":"DONE","text_delta":"Done already"}}),
    ] {
        for out in acc.feed_value(&ev, Some(1000), None) {
            if apply_agy_stream_output(&home, sid, &hp, out, 1) {
                ended = true;
            }
        }
    }
    assert!(
        ended,
        "a completed agent_response must end the turn without waiting for event == \"result\""
    );
    let page = read_history_page(AgentKind::Antigravity, &hp, None, None, None, None).unwrap();
    assert_eq!(page.messages.last().unwrap().text, "Done already");
    assert_eq!(page.tail_activity, crate::providers::TailActivity::Idle);

    let _ = fs::remove_dir_all(&home);
}

#[tokio::test]
async fn send_prompt_queues_behind_busy_chat_and_enforces_queue_limit() {
    let home = temp_home("cross-session-busy");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mgr = ProcessManager::new(home.clone(), tx);
    mgr.state.lock().unwrap().active.insert(
        "busy".into(),
        ActiveProcess {
            pid: 12345,
            cwd: None,
            source_path: home.join("chat.jsonl"),
            stdin_tx: None,
            busy: true,
            interrupted: false,
        },
    );
    for _ in 0..MAX_QUEUED_PROMPTS_PER_SESSION {
        assert_eq!(
            mgr.send_prompt(
                "busy",
                AgentKind::Claude,
                None,
                "Cross-session message",
                None
            )
            .await
            .unwrap(),
            "queued behind active turn"
        );
    }
    assert!(
        mgr.send_prompt("busy", AgentKind::Claude, None, "Overflow", None)
            .await
            .unwrap_err()
            .to_string()
            .contains("queue full")
    );
    assert_eq!(mgr.state.lock().unwrap().active["busy"].pid, 12345);
    fs::remove_dir_all(home).unwrap();
}

#[test]
fn hosted_stream_stops_mirroring_messages_as_soon_as_native_exists() {
    let home = temp_home("agy-stop-mirror");
    let conversation = "4d5d5535-249c-4f10-90d9-7ec49c2eeb53";
    let mirror = hosted_history_path(&home, "hosted");
    append_agy_user_row(&mirror, "hosted", Some("/repo"), "Start", 1).unwrap();
    apply_agy_stream_output(
        &home,
        "hosted",
        &mirror,
        AgyStreamOutput::Init {
            conversation_id: Some(conversation.into()),
            model: Some("gemini".into()),
        },
        2,
    );
    let native = home
        .join(".gemini/antigravity-cli/brain")
        .join(conversation)
        .join(".system_generated/logs/transcript.jsonl");
    fs::create_dir_all(native.parent().unwrap()).unwrap();
    fs::write(
        &native,
        "{\"type\":\"PLANNER_RESPONSE\",\"content\":\"Native reply\"}\n",
    )
    .unwrap();
    let mut acc = AgyStreamAccumulator::new();
    for event in [
        json!({"event":"step_update","step_update":{"step_type":"agent_response","step_index":1,"state":"DONE","text_delta":"Native reply"}}),
        json!({"event":"result","result":{"status":"SUCCESS"}}),
    ] {
        for output in acc.feed_value(&event, None, None) {
            apply_agy_stream_output(&home, "hosted", &mirror, output, 3);
        }
    }
    assert!(
        !fs::read_to_string(&mirror)
            .unwrap()
            .contains("Native reply")
    );
    assert_eq!(
        MetadataStore::load(&home)
            .get("hosted")
            .unwrap()
            .source_path
            .as_deref(),
        native.to_str()
    );
    fs::remove_dir_all(home).unwrap();
}

#[test]
fn native_stream_events_never_duplicate_cli_transcript_rows() {
    let home = temp_home("agy-native-no-duplicates");
    let conversation = "4d5d5535-249c-4f10-90d9-7ec49c2eeb53";
    let path = home
        .join(".gemini/antigravity-cli/brain")
        .join(conversation)
        .join(".system_generated/logs/transcript.jsonl");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let original = "{\"type\":\"PLANNER_RESPONSE\",\"content\":\"native reply\"}\n";
    fs::write(&path, original).unwrap();
    let mut acc = AgyStreamAccumulator::new();
    for event in [
        json!({"event":"init","conversation_id":conversation}),
        json!({"event":"step_update","step_update":{"step_type":"agent_response","step_index":1,"state":"DONE","text_delta":"native reply"}}),
        json!({"event":"result","result":{"status":"SUCCESS"}}),
    ] {
        for out in acc.feed_value(&event, Some(1000), None) {
            apply_agy_stream_output(&home, conversation, &path, out, 1);
        }
    }
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
    assert_eq!(
        MetadataStore::load(&home)
            .get(conversation)
            .unwrap()
            .native_session_id
            .as_deref(),
        Some(conversation)
    );
    let (tx, _rx) = mpsc::unbounded_channel();
    let mgr = ProcessManager::new(home.clone(), tx);
    let hosted = hosted_history_path(&home, "hosted-session");
    fs::create_dir_all(hosted.parent().unwrap()).unwrap();
    fs::write(&hosted, "").unwrap();
    assert_eq!(
        mgr.agy_history_sink("hosted-session", Some(conversation)),
        path
    );
    fs::remove_dir_all(home).unwrap();
}

#[tokio::test]
async fn send_and_steer_bypass_queue_on_busy_live_agy() {
    let home = temp_home("agy-busy-stdin");
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel();
    let mgr = ProcessManager::new(home.clone(), tx);
    mgr.state.lock().unwrap().active.insert(
        "s1".into(),
        ActiveProcess {
            pid: 999_998,
            cwd: None,
            source_path: hosted_history_path(&home, "s1"),
            stdin_tx: Some(stdin_tx),
            busy: true,
            interrupted: false,
        },
    );
    mgr.send_prompt("s1", AgentKind::Antigravity, None, "send while busy", None)
        .await
        .unwrap();
    assert!(mgr.is_session_busy("s1"));
    mgr.steer_session(
        "s1".into(),
        AgentKind::Antigravity,
        None,
        "steer while busy".into(),
        None,
    )
    .await;
    for expected in ["send while busy", "steer while busy"] {
        let frame: serde_json::Value = serde_json::from_str(
            &stdin_rx
                .try_recv()
                .expect("prompt must reach live stdin immediately"),
        )
        .unwrap();
        assert_eq!(frame["message"]["content"], expected);
    }
    assert!(matches!(rx.try_recv(), Ok(ServerEvent::HistoryAppend {
            working: Some(true), messages, ..
        }) if messages.is_empty()));
    assert!(
        matches!(rx.try_recv().unwrap(), ServerEvent::PromptStatus { status, .. } if status == "sent")
    );
    assert!(!mgr.state.lock().unwrap().queues.contains_key("s1"));
    fs::remove_dir_all(home).unwrap();
}

#[tokio::test]
async fn steer_session_on_busy_claude_still_queues() {
    let home = temp_home("claude-queue");
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mgr = ProcessManager::new(home.clone(), tx);
    {
        let mut guard = mgr.state.lock().unwrap();
        guard.active.insert(
            "s1".into(),
            ActiveProcess {
                pid: 999_998,
                cwd: None,
                source_path: home.join("s1.jsonl"),
                stdin_tx: None,
                busy: true,
                interrupted: false,
            },
        );
    }
    mgr.steer_session("s1".into(), AgentKind::Claude, None, "later".into(), None)
        .await;
    match rx.recv().await.unwrap() {
        ServerEvent::PromptStatus { status, .. } => assert_eq!(status, "queued"),
        other => panic!("expected queued, got {other:?}"),
    }
    let _ = fs::remove_dir_all(&home);
}

#[cfg(unix)]
#[tokio::test]
async fn agy_broken_stdin_reports_delivery_failure_and_evicts_stale_process() {
    use std::os::unix::fs::PermissionsExt;
    let home = temp_home("agy-broken-stdin");
    let bin = home.join("agy-close-stdin");
    let ready = home.join("stdin-closed");
    fs::write(
        &bin,
        format!(
            "#!/bin/sh\nIFS= read -r line\nexec 0<&-\ntouch '{}'\nexec sleep 30\n",
            ready.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mgr = ProcessManager::new(home.clone(), tx);
    mgr.set_test_bin_override(Some(bin));
    mgr.send_prompt("broken", AgentKind::Antigravity, None, "first", None)
        .await
        .unwrap();
    let pid = mgr.state.lock().unwrap().active["broken"].pid;
    // Wait until the child has closed the pipe, without relying on process scheduling.
    tokio::time::timeout(Duration::from_secs(5), async {
        while !ready.exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    mgr.send_prompt(
        "broken",
        AgentKind::Antigravity,
        None,
        "must report failure",
        None,
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(ServerEvent::PromptStatus { status, detail, .. }) = rx.recv().await {
                assert_eq!(status, "failed");
                assert_eq!(detail, "Antigravity stdin closed before delivery");
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(!mgr.state.lock().unwrap().active.contains_key("broken"));
    assert!(matches!(
        rx.recv().await,
        Some(ServerEvent::HistoryAppend {
            working: Some(false),
            ..
        })
    ));
    Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .await
        .unwrap();
    fs::remove_dir_all(home).unwrap();
}
