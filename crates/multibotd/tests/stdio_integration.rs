#![cfg(unix)]

use multibot_proto::{AgentKind, ClientRequest, ServerEvent, decode_line, encode_line};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

static INTEG_COUNTER: AtomicU64 = AtomicU64::new(1);

fn temp_home(tag: &str) -> PathBuf {
    let n = INTEG_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "multibotd-integ-{tag}-{}-{n}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn sha256_prefix_12(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!(
        "{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3], digest[4], digest[5]
    )
}

#[tokio::test]
async fn multibotd_stdio_full_wire_roundtrip() {
    let bin = env!("CARGO_BIN_EXE_multibotd");
    let bin_bytes = fs::read(bin).unwrap();
    let expected_hash = sha256_prefix_12(&bin_bytes);

    let home = temp_home("wire");
    let workdir = home.join("repo");
    fs::create_dir_all(&workdir).unwrap();

    // Seed the legacy `multiprompt` server state this host keeps read-only: a chat named
    // `claude-s1` belongs to "Yeo UX", and a Codex chat whose transcript id only appears in
    // `agent-host` state must resolve through its tmux key too.
    let legacy = home.join(".local/state/multiprompt");
    fs::create_dir_all(legacy.join("agent-host")).unwrap();
    fs::write(
        legacy.join("chat-names.json"),
        concat!(
            r#"{"live-aaaaaaa":{"name":"Claude chat","sid":"claude-s1","workspace":"Yeo UX"},"#,
            r#""live-bbbbbbb":{"name":"Codex chat","sid":"provider-sid-not-on-disk","workspace":"HT Product"}}"#
        ),
    )
    .unwrap();
    fs::write(
        legacy.join("agent-host/deadbeef.config.json"),
        r#"{"provider":"codex","sid":"provider-sid-not-on-disk","tmux":"live-bbbbbbb"}"#,
    )
    .unwrap();
    fs::write(
        legacy.join("agent-host/deadbeef.state.json"),
        r#"{"sid":"provider-sid-not-on-disk","native_session":"codex-s1","tmux":"live-bbbbbbb"}"#,
    )
    .unwrap();

    // Seed Claude, Codex, and Antigravity fixtures inside isolated HOME
    let claude_dir = home.join(".claude/projects/-repo");
    fs::create_dir_all(&claude_dir).unwrap();
    let claude_path = claude_dir.join("claude-s1.jsonl");
    fs::write(
        &claude_path,
        concat!(
            r#"{"type":"user","uuid":"u1","cwd":"/repo","timestamp":"2026-11-26T10:00:00Z","message":{"content":"Inspect Cargo.toml"}}"#,
            "\n",
            r#"{"type":"assistant","uuid":"a1","timestamp":"2026-11-26T10:00:01Z","message":{"model":"claude-opus-4-6","content":[{"type":"text","text":"All dependencies look clean."}]}}"#,
            "\n"
        ),
    )
    .unwrap();

    let codex_dir = home.join(".codex/sessions/2026/11/26");
    fs::create_dir_all(&codex_dir).unwrap();
    fs::write(
        codex_dir.join("rollout-2026-11-26-codex-s1.jsonl"),
        concat!(
            r#"{"type":"session_meta","payload":{"id":"codex-s1","cwd":"/repo"}}"#,
            "\n",
            r#"{"type":"event_msg","timestamp":"2026-11-26T10:00:02Z","payload":{"type":"agent_message","message":"Codex online"}}"#,
            "\n"
        ),
    )
    .unwrap();

    let agy_dir = home.join(".multibot/history");
    fs::create_dir_all(&agy_dir).unwrap();
    fs::write(
        agy_dir.join("0123456789abcdef0123456789abcdef.history.jsonl"),
        concat!(
            r#"{"type":"multibot_meta","session_id":"agy-s1","agent":"antigravity","cwd":"/repo","title":"Agy Session"}"#,
            "\n",
            r#"{"type":"multibot_history","row":{"id":"a1","role":"assistant","text":"Antigravity ready","timestamp_ms":1764180000000}}"#,
            "\n"
        ),
    )
    .unwrap();

    let mut child = Command::new(bin)
        .arg("--stdio")
        .env("HOME", &home)
        .env("TMUX_TMPDIR", &home)
        .env_remove("TMUX")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout).lines();

    async fn send_req(stdin: &mut tokio::process::ChildStdin, req: &ClientRequest) {
        let line = encode_line(req).unwrap();
        stdin.write_all(line.as_bytes()).await.unwrap();
        stdin.flush().await.unwrap();
    }

    async fn next_ev(
        reader: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    ) -> ServerEvent {
        let line = tokio::time::timeout(Duration::from_secs(20), reader.next_line())
            .await
            .expect("timed out waiting for daemon event")
            .unwrap()
            .expect("daemon closed stdout unexpectedly");
        decode_line::<ServerEvent>(&line).unwrap()
    }

    // 1. Hello -> HelloAck with real 12-hex binary_hash
    send_req(
        &mut stdin,
        &ClientRequest::Hello {
            client_version: "0.1.0".into(),
        },
    )
    .await;
    match next_ev(&mut reader).await {
        ServerEvent::HelloAck { binary_hash, .. } => {
            assert_eq!(binary_hash, expected_hash);
        }
        other => panic!("expected HelloAck, got {other:?}"),
    }

    // 2. Ping -> Pong
    send_req(&mut stdin, &ClientRequest::Ping { nonce: 77 }).await;
    match next_ev(&mut reader).await {
        ServerEvent::Pong { nonce, .. } => assert_eq!(nonce, 77),
        other => panic!("expected Pong, got {other:?}"),
    }

    // 3. ListSessions -> all 3 agents discovered with rich metadata, each carrying the
    //    workspace the legacy server recorded for that chat. This fixture's transcripts are
    //    not held open by a daemon or metadata record, so it asks for the full listing, the
    //    way the archive view will.
    send_req(&mut stdin, &ClientRequest::ListSessions { limit: Some(20), include_archived: Some(true) }).await;
    match next_ev(&mut reader).await {
        ServerEvent::Sessions { sessions } => {
            assert_eq!(sessions.len(), 3);
            let claude = sessions.iter().find(|s| s.agent == AgentKind::Claude).unwrap();
            assert_eq!(claude.model.as_deref(), Some("Opus 4.6"));
            assert_eq!(claude.last_assistant_id.as_deref(), Some("a1"));
            assert_eq!(claude.workspace.as_deref(), Some("Yeo UX"));
            let codex = sessions.iter().find(|s| s.agent == AgentKind::Codex).unwrap();
            assert_eq!(codex.workspace.as_deref(), Some("HT Product"));
            // A chat with no recorded workspace stays unresolvable instead of being guessed.
            let agy = sessions
                .iter()
                .find(|s| s.agent == AgentKind::Antigravity)
                .unwrap();
            assert_eq!(agy.workspace, None);
        }
        other => panic!("expected Sessions, got {other:?}"),
    }

    // 4. ReadHistory -> bounded offsets & probe_sha256
    send_req(
        &mut stdin,
        &ClientRequest::ReadHistory {
            session_id: "claude-s1".into(),
            source_path: claude_path.to_string_lossy().into(),
            agent: AgentKind::Claude,
            tail: None,
            before_offset: None,
            max_bytes: Some(4096),
            known_probe: None,
        },
    )
    .await;
    match next_ev(&mut reader).await {
        ServerEvent::History {
            messages,
            oldest_offset,
            latest_offset,
            probe_sha256,
            ..
        } => {
            assert_eq!(messages.len(), 2);
            assert_eq!(oldest_offset, Some(0));
            assert!(latest_offset.unwrap() > 0);
            assert_eq!(probe_sha256.as_deref().map(str::len), Some(64));
        }
        other => panic!("expected History, got {other:?}"),
    }

    // 5. WatchSession + live append -> HistoryAppend
    send_req(
        &mut stdin,
        &ClientRequest::WatchSession {
            session_id: "claude-s1".into(),
            source_path: claude_path.to_string_lossy().into(),
            agent: AgentKind::Claude,
        },
    )
    .await;
    let _initial = next_ev(&mut reader).await;

    let mut f = OpenOptions::new().append(true).open(&claude_path).unwrap();
    writeln!(
        f,
        r#"{{"type":"assistant","uuid":"a2","message":{{"content":"Live inotify message"}}}}"#
    )
    .unwrap();
    f.flush().unwrap();

    let mut saw_append = false;
    for _ in 0..4 {
        match next_ev(&mut reader).await {
            ServerEvent::HistoryAppend { messages, .. } => {
                assert_eq!(messages[0].text, "Live inotify message");
                saw_append = true;
                break;
            }
            ServerEvent::SessionUpdated { .. } => continue,
            other => panic!("unexpected event during live watch: {other:?}"),
        }
    }
    assert!(saw_append);

    send_req(&mut stdin, &ClientRequest::UnwatchSession { session_id: None }).await;

    // 6. SearchHistory
    send_req(
        &mut stdin,
        &ClientRequest::SearchHistory {
            request_id: "sreq-1".into(),
            session_id: "claude-s1".into(),
            source_path: claude_path.to_string_lossy().into(),
            agent: AgentKind::Claude,
            query: "inotify".into(),
        },
    )
    .await;
    loop {
        match next_ev(&mut reader).await {
            ServerEvent::SessionUpdated { .. } => continue,
            ServerEvent::SearchResults {
                request_id, hits, ..
            } => {
                assert_eq!(request_id, "sreq-1");
                assert_eq!(hits.len(), 1);
                assert!(hits[0].snippet.contains("inotify"));
                break;
            }
            other => panic!("expected SearchResults, got {other:?}"),
        }
    }

    // 7. ListCommands
    send_req(
        &mut stdin,
        &ClientRequest::ListCommands {
            agent: AgentKind::Claude,
            cwd: Some(workdir.to_string_lossy().into()),
        },
    )
    .await;
    match next_ev(&mut reader).await {
        ServerEvent::Commands { agent, items, .. } => {
            assert_eq!(agent, AgentKind::Claude);
            assert!(items.iter().any(|i| i.name == "compact"));
        }
        other => panic!("expected Commands, got {other:?}"),
    }

    // 8. RenameSession, ArchiveSession, UnarchiveSession
    send_req(
        &mut stdin,
        &ClientRequest::RenameSession {
            session_id: "claude-s1".into(),
            agent: AgentKind::Claude,
            source_path: claude_path.to_string_lossy().into(),
            title: "Renamed Integration Session".into(),
        },
    )
    .await;
    match next_ev(&mut reader).await {
        ServerEvent::SessionUpdated { session } => {
            assert_eq!(session.title, "Renamed Integration Session");
        }
        other => panic!("expected SessionUpdated, got {other:?}"),
    }

    send_req(
        &mut stdin,
        &ClientRequest::ArchiveSession {
            session_id: "claude-s1".into(),
            agent: AgentKind::Claude,
            source_path: claude_path.to_string_lossy().into(),
            kill_process: false,
        },
    )
    .await;
    match next_ev(&mut reader).await {
        ServerEvent::SessionArchived {
            session_id,
            archived,
        } => {
            assert_eq!(session_id, "claude-s1");
            assert!(archived);
        }
        other => panic!("expected SessionArchived(true), got {other:?}"),
    }
    assert!(claude_path.is_file(), "ArchiveSession must NEVER delete .jsonl");

    send_req(
        &mut stdin,
        &ClientRequest::UnarchiveSession {
            session_id: "claude-s1".into(),
        },
    )
    .await;
    match next_ev(&mut reader).await {
        ServerEvent::SessionArchived { archived, .. } => assert!(!archived),
        other => panic!("expected SessionArchived(false), got {other:?}"),
    }

    // 9. SpawnSession (Antigravity) & InterruptSession (unowned)
    send_req(
        &mut stdin,
        &ClientRequest::SpawnSession {
            agent: AgentKind::Antigravity,
            cwd: workdir.to_string_lossy().into(),
            title: Some("Spawned Agy".into()),
            initial_prompt: None,
            options: None,
        },
    )
    .await;
    match next_ev(&mut reader).await {
        ServerEvent::SessionSpawned { session } => {
            assert_eq!(session.title, "Spawned Agy");
            assert!(PathBuf::from(&session.source_path).is_file());
        }
        other => panic!("expected SessionSpawned, got {other:?}"),
    }

    send_req(
        &mut stdin,
        &ClientRequest::InterruptSession {
            session_id: "claude-s1".into(),
            agent: AgentKind::Claude,
        },
    )
    .await;
    match next_ev(&mut reader).await {
        ServerEvent::SessionInterrupted { ok, .. } => assert!(!ok),
        other => panic!("expected SessionInterrupted, got {other:?}"),
    }

    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(3), child.wait())
        .await
        .expect("multibotd did not exit cleanly on stdin EOF")
        .unwrap();
    assert!(status.success());

    let _ = fs::remove_dir_all(&home);
}

#[cfg(target_os = "linux")]
fn read_proc_vm_hwm_kib(pid: u32) -> Option<u64> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let kib = rest.split_whitespace().next()?.parse::<u64>().ok()?;
            return Some(kib);
        }
    }
    None
}

#[tokio::test]
async fn multibotd_stdio_huge_base64_and_20mib_non_base64_stay_under_10mb_vmhwm() {
    let bin = env!("CARGO_BIN_EXE_multibotd");
    let home = temp_home("b64-and-20mib-hwm");
    let claude_dir = home.join(".claude/projects/-b64-hwm");
    fs::create_dir_all(&claude_dir).unwrap();

    // 1. Seed a 1.5 MiB base64 image transcript (superset of T2-DMN-06)
    let b64_path = claude_dir.join("claude-b64-1_5m.jsonl");
    {
        let mut f = fs::File::create(&b64_path).unwrap();
        writeln!(
            f,
            r#"{{"type":"user","uuid":"u-b64-1","timestamp":"2026-11-26T10:00:00Z","message":{{"content":"Capture screenshot"}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","uuid":"a-b64-1","timestamp":"2026-11-26T10:00:01Z","message":{{"content":[{{"type":"tool_use","id":"tu_img_1","name":"Bash","input":{{"description":"Capture PNG"}}}}]}}}}"#
        )
        .unwrap();
        write!(
            f,
            r#"{{"type":"user","uuid":"u-b64-res","timestamp":"2026-11-26T10:00:02Z","message":{{"content":[{{"type":"tool_result","tool_use_id":"tu_img_1","content":"data:image/png;base64,"#
        )
        .unwrap();
        let a_chunk = "A".repeat(64 * 1024);
        for _ in 0..24 {
            f.write_all(a_chunk.as_bytes()).unwrap();
        }
        writeln!(f, r#"=="}}]}}}}"#).unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","uuid":"a-b64-2","timestamp":"2026-11-26T10:00:03Z","message":{{"content":[{{"type":"text","text":"Captured."}}]}}}}"#
        )
        .unwrap();
        f.flush().unwrap();
    }
    let b64_size = fs::metadata(&b64_path).unwrap().len();

    // 2. Seed a 20 MiB non-base64 transcript sandwiched between searchable neighbor lines
    let giant_path = claude_dir.join("claude-giant-20m.jsonl");
    {
        let mut f = fs::File::create(&giant_path).unwrap();
        writeln!(
            f,
            r#"{{"type":"user","uuid":"u-pre","timestamp":"2026-11-26T10:00:00Z","message":{{"content":"NEEDLE_BEFORE_20M"}}}}"#
        )
        .unwrap();
        write!(
            f,
            r#"{{"type":"assistant","uuid":"a-20m","timestamp":"2026-11-26T10:00:01Z","message":{{"content":""#
        )
        .unwrap();
        let x_chunk = "x ".repeat(32 * 1024);
        for _ in 0..320 {
            f.write_all(x_chunk.as_bytes()).unwrap();
        }
        writeln!(f, r#""}}}}"#).unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","uuid":"a-post","timestamp":"2026-11-26T10:00:02Z","message":{{"content":"NEEDLE_AFTER_20M"}}}}"#
        )
        .unwrap();
        f.flush().unwrap();
    }

    let mut child = Command::new(bin)
        .arg("--stdio")
        .env("HOME", &home)
        .env("TMUX_TMPDIR", &home)
        .env_remove("TMUX")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    #[cfg(target_os = "linux")]
    let child_pid = child.id().expect("child pid");
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout).lines();

    async fn send_req(stdin: &mut tokio::process::ChildStdin, req: &ClientRequest) {
        let line = encode_line(req).unwrap();
        stdin.write_all(line.as_bytes()).await.unwrap();
        stdin.flush().await.unwrap();
    }

    async fn next_raw_and_ev(
        reader: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    ) -> (String, ServerEvent) {
        let line = tokio::time::timeout(Duration::from_secs(20), reader.next_line())
            .await
            .expect("timed out waiting for daemon event")
            .unwrap()
            .expect("daemon closed stdout unexpectedly");
        let ev = decode_line::<ServerEvent>(&line).unwrap();
        (line, ev)
    }

    // 1. ReadHistory on 1.5 MiB base64 transcript
    send_req(
        &mut stdin,
        &ClientRequest::ReadHistory {
            session_id: "claude-b64-1_5m".into(),
            source_path: b64_path.to_string_lossy().into(),
            agent: AgentKind::Claude,
            tail: None,
            before_offset: None,
            max_bytes: Some(2 * 1024 * 1024),
            known_probe: None,
        },
    )
    .await;
    let (raw_hist, ev_hist) = next_raw_and_ev(&mut reader).await;
    assert!(raw_hist.len() < 40 * 1024, "wire event exceeded 40 KiB");
    assert!(raw_hist.contains("[image omitted]"));
    assert!(!raw_hist.contains(&"A".repeat(256)));
    match ev_hist {
        ServerEvent::History {
            messages,
            oldest_offset,
            latest_offset,
            has_more,
            probe_sha256,
            ..
        } => {
            let ids: Vec<&str> = messages.iter().map(|m| m.id.as_str()).collect();
            assert_eq!(ids, vec!["u-b64-1", "a-b64-1", "a-b64-2"]);
            assert_eq!(oldest_offset, Some(0));
            assert_eq!(latest_offset, Some(b64_size));
            assert!(!has_more);
            assert_eq!(probe_sha256.as_deref().map(str::len), Some(64));
        }
        other => panic!("expected History, got {other:?}"),
    }

    // 2. WatchSession + live append of a 400 KiB Anthropic structured base64 image line
    send_req(
        &mut stdin,
        &ClientRequest::WatchSession {
            session_id: "claude-b64-1_5m".into(),
            source_path: b64_path.to_string_lossy().into(),
            agent: AgentKind::Claude,
        },
    )
    .await;
    let (_, init_ev) = next_raw_and_ev(&mut reader).await;
    assert!(matches!(init_ev, ServerEvent::History { .. }));

    {
        let mut f = OpenOptions::new().append(true).open(&b64_path).unwrap();
        write!(
            f,
            r#"{{"type":"user","uuid":"u-live-400k","message":{{"content":[{{"type":"image","source":{{"type":"base64","media_type":"image/png","data":""#
        )
        .unwrap();
        let b_chunk = "B".repeat(64 * 1024);
        for _ in 0..7 {
            f.write_all(b_chunk.as_bytes()).unwrap();
        }
        writeln!(f, r#""}}}}]}}}}"#).unwrap();
        f.flush().unwrap();
    }

    let mut saw_live_b64 = false;
    for _ in 0..6 {
        let (raw_ev, ev) = next_raw_and_ev(&mut reader).await;
        match ev {
            ServerEvent::HistoryAppend { messages, .. } => {
                assert!(!raw_ev.contains(&"B".repeat(256)));
                if messages
                    .iter()
                    .any(|m| m.id == "u-live-400k" && m.text.contains("[image omitted]"))
                {
                    saw_live_b64 = true;
                    break;
                }
            }
            ServerEvent::SessionUpdated { .. } => continue,
            other => panic!("unexpected event during live watch: {other:?}"),
        }
    }
    assert!(saw_live_b64, "live 400 KiB Anthropic base64 append must arrive");
    send_req(&mut stdin, &ClientRequest::UnwatchSession { session_id: None }).await;

    // 3. ListSessions + full ReadHistory pagination + SearchHistory on 20 MiB non-base64 transcript
    send_req(&mut stdin, &ClientRequest::ListSessions { limit: Some(20), include_archived: Some(true) }).await;
    loop {
        let (_, ev) = next_raw_and_ev(&mut reader).await;
        match ev {
            ServerEvent::SessionUpdated { .. } => continue,
            ServerEvent::Sessions { sessions } => {
                assert_eq!(sessions.len(), 2);
                break;
            }
            other => panic!("expected Sessions, got {other:?}"),
        }
    }

    let mut before: Option<u64> = None;
    let mut giant_ids = Vec::new();
    for _ in 0..6 {
        send_req(
            &mut stdin,
            &ClientRequest::ReadHistory {
                session_id: "claude-giant-20m".into(),
                source_path: giant_path.to_string_lossy().into(),
                agent: AgentKind::Claude,
                tail: None,
                before_offset: before,
                max_bytes: None,
                known_probe: None,
            },
        )
        .await;
        let (_, ev) = next_raw_and_ev(&mut reader).await;
        match ev {
            ServerEvent::History {
                messages,
                oldest_offset,
                has_more,
                ..
            } => {
                let mut page_ids: Vec<String> = messages.into_iter().map(|m| m.id).collect();
                page_ids.extend(giant_ids);
                giant_ids = page_ids;
                if !has_more {
                    assert_eq!(oldest_offset, Some(0));
                    break;
                }
                before = oldest_offset;
            }
            other => panic!("expected History for 20 MiB pagination, got {other:?}"),
        }
    }
    assert_eq!(giant_ids, vec!["u-pre", "a-post"]);

    send_req(
        &mut stdin,
        &ClientRequest::SearchHistory {
            request_id: "sr-20m".into(),
            session_id: "claude-giant-20m".into(),
            source_path: giant_path.to_string_lossy().into(),
            agent: AgentKind::Claude,
            query: "NEEDLE_".into(),
        },
    )
    .await;
    match next_raw_and_ev(&mut reader).await.1 {
        ServerEvent::SearchResults {
            request_id, hits, ..
        } => {
            assert_eq!(request_id, "sr-20m");
            assert_eq!(hits.len(), 2);
        }
        other => panic!("expected SearchResults, got {other:?}"),
    }

    // 4. Verify isolated daemon VmHWM stays strictly < 10 MB (10,240 KiB)
    #[cfg(target_os = "linux")]
    {
        let vm_hwm_kib = read_proc_vm_hwm_kib(child_pid).expect("read /proc/<pid>/status VmHWM");
        eprintln!("multibotd VmHWM after history/watch/search: {vm_hwm_kib} KiB");
        assert!(
            vm_hwm_kib < 10 * 1024,
            "multibotd VmHWM exceeded 10 MB budget: {vm_hwm_kib} KiB ({:.2} MiB)",
            vm_hwm_kib as f64 / 1024.0
        );
    }

    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(3), child.wait())
        .await
        .expect("multibotd did not exit cleanly on stdin EOF")
        .unwrap();
    assert!(status.success());

    let _ = fs::remove_dir_all(&home);
}

#[tokio::test]
async fn shell_sessions_spawn_open_refresh_and_disappear_without_transcripts() {
    assert!(
        std::process::Command::new("tmux")
            .arg("-V")
            .stdout(Stdio::null())
            .status()
            .is_ok_and(|s| s.success()),
        "tmux is required for the live shell lifecycle test"
    );
    struct IsolatedTmux(PathBuf);
    impl Drop for IsolatedTmux {
        fn drop(&mut self) {
            let _ = std::process::Command::new("tmux")
                .args(["kill-server"])
                .env("TMUX_TMPDIR", &self.0)
                .env_remove("TMUX")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let home = temp_home("shell");
    let _server = IsolatedTmux(home.clone());
    let mut child = Command::new(env!("CARGO_BIN_EXE_multibotd"))
        .env("HOME", &home)
        .env("TMUX_TMPDIR", &home)
        .env_remove("TMUX")
        .env_remove("MULTIPROMPT_STATE_DIR")
        .env_remove("XDG_STATE_HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
    input
        .write_all(
            encode_line(&ClientRequest::WatchDirectory)
                .unwrap()
                .as_bytes(),
        )
        .await
        .unwrap();
    input
        .write_all(
            encode_line(&ClientRequest::SpawnSession {
                agent: AgentKind::Shell,
                cwd: home.to_string_lossy().into(),
                title: Some("Native shell".into()),
                initial_prompt: None,
                options: None,
            })
            .unwrap()
            .as_bytes(),
        )
        .await
        .unwrap();
    let session = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let line = output.next_line().await.unwrap().unwrap();
            match decode_line::<ServerEvent>(&line).unwrap() {
                ServerEvent::SessionSpawned { session } => break session,
                ServerEvent::Error { code, message } => panic!("{code}: {message}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(session.agent, AgentKind::Shell);
    assert_eq!(session.title, "Native shell");
    assert!(session.id.starts_with("term-"));
    assert_eq!(session.source_path, format!("tmux:{}", session.id));
    input
        .write_all(
            encode_line(&ClientRequest::ReadHistory {
                session_id: session.id.clone(),
                source_path: session.source_path.clone(),
                agent: AgentKind::Shell,
                tail: None,
                before_offset: None,
                max_bytes: None,
                known_probe: None,
            })
            .unwrap()
            .as_bytes(),
        )
        .await
        .unwrap();
    input
        .write_all(
            encode_line(&ClientRequest::WatchSession {
                session_id: session.id.clone(),
                source_path: session.source_path.clone(),
                agent: AgentKind::Shell,
            })
            .unwrap()
            .as_bytes(),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut histories = 0;
        let mut listed = false;
        while histories < 2 || !listed {
            match decode_line::<ServerEvent>(&output.next_line().await.unwrap().unwrap()).unwrap() {
                ServerEvent::History {
                    session_id,
                    messages,
                    ..
                } => {
                    assert_eq!(session_id, session.id);
                    assert!(messages.is_empty());
                    histories += 1;
                }
                ServerEvent::Sessions { sessions } => {
                    listed |= sessions.iter().any(|s| s.id == session.id);
                }
                ServerEvent::Error { code, message } => panic!("{code}: {message}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    // Create a second shell without asking Multibot to refresh.
    assert!(
        Command::new("tmux")
            .args(["new-session", "-d", "-s", "external-shell", "-c"])
            .arg(&home)
            .env("TMUX_TMPDIR", &home)
            .env_remove("TMUX")
            .status()
            .await
            .unwrap()
            .success()
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let ServerEvent::Sessions { sessions } =
                decode_line::<ServerEvent>(&output.next_line().await.unwrap().unwrap()).unwrap()
                && sessions.iter().any(|s| s.id == "external-shell")
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    input.write_all(encode_line(&ClientRequest::ArchiveSession {
        session_id: "external-shell".into(), agent: AgentKind::Shell,
        source_path: "tmux:external-shell".into(), kill_process: false,
    }).unwrap().as_bytes()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let ServerEvent::SessionArchived { session_id, archived: true } =
                decode_line::<ServerEvent>(&output.next_line().await.unwrap().unwrap()).unwrap()
                && session_id == "external-shell" { break; }
        }
    }).await.unwrap();
    input.write_all(encode_line(&ClientRequest::ListSessions {
        limit: Some(20), include_archived: Some(true),
    }).unwrap().as_bytes()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let ServerEvent::Sessions { sessions } =
                decode_line::<ServerEvent>(&output.next_line().await.unwrap().unwrap()).unwrap()
                && sessions.iter().any(|shell| shell.id == "external-shell"
                    && shell.archived && shell.shell_attached == Some(false)) { break; }
        }
    }).await.unwrap();
    assert!(
        Command::new("tmux")
            .args(["kill-session", "-t", "=external-shell"])
            .env("TMUX_TMPDIR", &home)
            .env_remove("TMUX")
            .status()
            .await
            .unwrap()
            .success()
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let ServerEvent::Sessions { sessions } =
                decode_line::<ServerEvent>(&output.next_line().await.unwrap().unwrap()).unwrap()
                && !sessions.iter().any(|s| s.id == "external-shell")
                && sessions.iter().any(|s| s.id == session.id)
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    // Dissolving must kill the real remote shell, not merely hide its metadata.
    input
        .write_all(
            encode_line(&ClientRequest::ArchiveSession {
                session_id: session.id.clone(),
                agent: AgentKind::Shell,
                source_path: session.source_path.clone(),
                kill_process: true,
            })
            .unwrap()
            .as_bytes(),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match decode_line::<ServerEvent>(&output.next_line().await.unwrap().unwrap()).unwrap() {
                ServerEvent::SessionArchived {
                    session_id,
                    archived: true,
                } if session_id == session.id => break,
                ServerEvent::Error { code, message } => panic!("{code}: {message}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert!(
        !Command::new("tmux")
            .args(["has-session", "-t", &format!("={}", session.id)])
            .env("TMUX_TMPDIR", &home)
            .env_remove("TMUX")
            .stderr(Stdio::null())
            .status()
            .await
            .unwrap()
            .success(),
        "ArchiveSession must terminate the live tmux shell"
    );
    input
        .write_all(
            encode_line(&ClientRequest::ListSessions {
                limit: Some(20),
                include_archived: Some(true),
            })
            .unwrap()
            .as_bytes(),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let ServerEvent::Sessions { sessions } =
                decode_line::<ServerEvent>(&output.next_line().await.unwrap().unwrap()).unwrap()
                && sessions.iter().any(|s| s.id == session.id && s.archived && s.archive_kill_process)
            {
                assert!(sessions.iter().filter(|s| s.agent == AgentKind::Shell).all(|s| s.archived));
                break;
            }
        }
    })
    .await
    .unwrap();
    drop(input);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}

#[tokio::test]
async fn pi_cursor_active_discovery_history_live_updates_stay_under_10mb() {
    use serde_json::json;
    let home = temp_home("pi-cursor");
    let tmux_home = home.join("tmux");
    fs::create_dir_all(&tmux_home).unwrap();
    let pi = home.join(".pi/agent/sessions/--repo--/date_pi-session.jsonl");
    let cursor =
        home.join(".cursor/projects/repo/agent-transcripts/cursor-session/cursor-session.jsonl");
    for (path, agent) in [(&pi, AgentKind::Pi), (&cursor, AgentKind::Cursor)] {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = fs::File::create(path).unwrap();
        if agent == AgentKind::Pi {
            writeln!(
                file,
                "{}",
                json!({"type":"session","id":"pi-session","cwd":"/repo"})
            )
            .unwrap();
        }
        for i in 0..12 {
            let message = json!({"role":"assistant","content":format!("reply {i}")});
            let row = if agent == AgentKind::Pi {
                json!({"type":"message","id":format!("m{i}"),"message":message})
            } else {
                json!({"role":"assistant","message":message})
            };
            writeln!(file, "{row}").unwrap();
        }
        for i in 0..2 {
            let message = json!({"role":"assistant","content":"A bounded prose segment. ".repeat(9000)});
            let row = if agent == AgentKind::Pi { json!({"type":"message","id":format!("large-{i}"),"message":message}) }
                else { json!({"role":"assistant","message":message}) };
            writeln!(file, "{row}").unwrap();
        }
        let message = json!({"role":"user","content":[{"type":"text","text":"screenshot"},{"type":"image","source":{"type":"base64","data":"A".repeat(1500 * 1024)}}]});
        let row = if agent == AgentKind::Pi {
            json!({"type":"message","id":"image","message":message})
        } else {
            json!({"role":"user","message":message})
        };
        writeln!(file, "{row}").unwrap();
        for i in 0..600 {
            let dead = if agent == AgentKind::Cursor {
                path.parent()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .join(format!("dead-{i}/dead-{i}.jsonl"))
            } else {
                path.with_file_name(format!("dead-{i}.jsonl"))
            };
            fs::create_dir_all(dead.parent().unwrap()).unwrap();
            fs::write(dead, "not a transcript\n").unwrap();
        }
    }
    let legacy = home.join(".local/state/multiprompt");
    fs::create_dir_all(legacy.join("agent-host")).unwrap();
    let names = r#"{"live-pi":{"name":"Pi Work","sid":"pi-session","workspace":"Coding"},"live-cursor":{"name":"Cursor Work","sid":"cursor-session","workspace":"Coding"}}"#;
    fs::write(legacy.join("chat-names.json"), names).unwrap();
    for sid in ["pi-session", "cursor-session"] {
        fs::write(
            legacy.join(format!("agent-host/{sid}.state.json")),
            format!("{{\"sid\":\"{sid}\",\"pid\":{}}}", std::process::id()),
        )
        .unwrap();
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_multibotd"))
        .arg("--stdio")
        .env("HOME", &home)
        .env("TMUX_TMPDIR", &tmux_home)
        .env_remove("TMUX")
        .env("MULTIPROMPT_STATE_DIR", &legacy)
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .env_remove("PI_CODING_AGENT_SESSION_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let pid = child.id().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap()).lines();
    async fn send(stdin: &mut tokio::process::ChildStdin, request: ClientRequest) {
        stdin
            .write_all(encode_line(&request).unwrap().as_bytes())
            .await
            .unwrap();
        stdin.flush().await.unwrap();
    }
    async fn next(
        reader: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    ) -> ServerEvent {
        loop {
            let line = tokio::time::timeout(Duration::from_secs(10), reader.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(!line.contains(&"A".repeat(256)));
            let event = decode_line(&line).unwrap();
            if !matches!(event, ServerEvent::SessionUpdated { .. }) {
                return event;
            }
        }
    }
    send(
        &mut stdin,
        ClientRequest::Hello {
            client_version: "test".into(),
        },
    )
    .await;
    assert!(matches!(
        next(&mut reader).await,
        ServerEvent::HelloAck { .. }
    ));
    send(
        &mut stdin,
        ClientRequest::ListSessions {
            limit: Some(20),
            include_archived: None,
        },
    )
    .await;
    let ServerEvent::Sessions { sessions } = next(&mut reader).await else {
        panic!("expected sessions")
    };
    assert_eq!(sessions.len(), 2, "{sessions:?}");
    assert!(
        sessions
            .iter()
            .any(|s| s.agent == AgentKind::Pi && s.title == "Pi Work")
    );
    assert!(
        sessions
            .iter()
            .any(|s| s.agent == AgentKind::Cursor && s.title == "Cursor Work")
    );
    assert!(
        sessions
            .iter()
            .all(|s| s.workspace.as_deref() == Some("Coding"))
    );
    for (path, agent, sid) in [
        (&pi, AgentKind::Pi, "pi-session"),
        (&cursor, AgentKind::Cursor, "cursor-session"),
    ] {
        send(
            &mut stdin,
            ClientRequest::ReadHistory {
                session_id: sid.into(),
                source_path: path.to_string_lossy().into(),
                agent,
                tail: Some(3),
                before_offset: None,
                max_bytes: None,
                known_probe: None,
            },
        )
        .await;
        let event = next(&mut reader).await;
        let ServerEvent::History {
            messages,
            oldest_offset,
            has_more,
            ..
        } = event
        else {
            panic!("expected history, got {event:?}")
        };
        assert!(has_more);
        assert!(messages.iter().any(|m| m.text.contains("[image omitted]")));
        let ids: Vec<_> = messages.iter().map(|m| m.id.clone()).collect();
        send(
            &mut stdin,
            ClientRequest::ReadHistory {
                session_id: sid.into(),
                source_path: path.to_string_lossy().into(),
                agent,
                tail: Some(3),
                before_offset: oldest_offset,
                max_bytes: None,
                known_probe: None,
            },
        )
        .await;
        let ServerEvent::History { messages, .. } = next(&mut reader).await else {
            panic!("expected older history")
        };
        assert!(!messages.is_empty());
        assert!(messages.iter().all(|m| !ids.contains(&m.id)));
        send(
            &mut stdin,
            ClientRequest::WatchSession {
                session_id: sid.into(),
                source_path: path.to_string_lossy().into(),
                agent,
            },
        )
        .await;
        assert!(matches!(
            next(&mut reader).await,
            ServerEvent::History { .. }
        ));
        let row = if agent == AgentKind::Pi {
            json!({"type":"message","id":"live","message":{"role":"assistant","content":"live reply"}})
        } else {
            json!({"role":"assistant","message":{"content":"live reply"}})
        };
        writeln!(OpenOptions::new().append(true).open(path).unwrap(), "{row}").unwrap();
        let ServerEvent::HistoryAppend { messages, .. } = next(&mut reader).await else {
            panic!("expected append")
        };
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "live reply");
        send(
            &mut stdin,
            ClientRequest::UnwatchSession { session_id: None },
        )
        .await;
    }
    #[cfg(target_os = "linux")]
    {
        let peak = read_proc_vm_hwm_kib(pid).unwrap();
        eprintln!("Pi + Cursor discovery/history/live peak RSS: {peak} KiB");
        assert!(
            peak < 10 * 1024,
            "Pi + Cursor peak RSS exceeds budget: {peak} KiB"
        );
    }
    assert_eq!(
        fs::read_to_string(legacy.join("chat-names.json")).unwrap(),
        names
    );
    drop(stdin);
    assert!(
        tokio::time::timeout(Duration::from_secs(3), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    fs::remove_dir_all(home).unwrap();
}

#[tokio::test]
async fn new_sessions_read_empty_history_and_antigravity_streams_first_prompt() {
    use std::os::unix::fs::PermissionsExt;
    let home = temp_home("new-session");
    let bin_dir = home.join(".local/bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let agy = bin_dir.join("agy");
    fs::write(&agy, r##"#!/bin/sh
[ "$*" = '--input-format stream-json --output-format stream-json --dangerously-skip-permissions --print=' ] || exit 41
printf '%s\n' "$PWD" > agy-cwd
printf '{"event":"init","conversation_id":"c5328de3-1234-4567-890a-123456789abc"}\n'
while IFS= read -r frame; do
    printf '%s\n' "$frame" >> agy-input
    printf '{"event":"step_update","step_update":{"step_type":"agent_response","step_index":1,"state":"DONE","text_delta":"First reply"}}\n'
    printf '{"event":"result","result":{"status":"SUCCESS"}}\n'
done
"##).unwrap();
    fs::set_permissions(&agy, fs::Permissions::from_mode(0o700)).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_multibotd"))
        .arg("--stdio")
        .env("HOME", &home)
        .env(
            "PATH",
            format!("{}:/usr/local/bin:/usr/bin:/bin", bin_dir.display()),
        )
        .env("TMUX_TMPDIR", &home)
        .env_remove("TMUX")
        .env_remove("MULTIPROMPT_STATE_DIR")
        .env_remove("XDG_STATE_HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
    async fn send(input: &mut tokio::process::ChildStdin, req: ClientRequest) {
        input
            .write_all(encode_line(&req).unwrap().as_bytes())
            .await
            .unwrap();
        input.flush().await.unwrap();
    }
    async fn next(
        output: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    ) -> ServerEvent {
        let line = tokio::time::timeout(Duration::from_secs(10), output.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        decode_line(&line).unwrap()
    }
    for agent in [
        AgentKind::Claude,
        AgentKind::Codex,
        AgentKind::Pi,
        AgentKind::Cursor,
        AgentKind::Antigravity,
    ] {
        send(
            &mut input,
            ClientRequest::SpawnSession {
                agent,
                cwd: home.to_string_lossy().into(),
                title: Some("Empty chat".into()),
                initial_prompt: None,
                options: None,
            },
        )
        .await;
        let ServerEvent::SessionSpawned { session } = next(&mut output).await else {
            panic!("expected new {agent:?} session")
        };
        send(
            &mut input,
            ClientRequest::ReadHistory {
                session_id: session.id.clone(),
                source_path: session.source_path.clone(),
                agent,
                tail: None,
                before_offset: None,
                max_bytes: None,
                known_probe: None,
            },
        )
        .await;
        match next(&mut output).await {
            ServerEvent::History {
                session_id,
                messages,
                has_more,
                ..
            } => {
                assert_eq!(session_id, session.id);
                assert!(messages.is_empty());
                assert!(!has_more);
            }
            other => panic!("new {agent:?} must have empty history, got {other:?}"),
        }
        // Even a hosted transcript can be absent before its first write.
        send(
            &mut input,
            ClientRequest::ReadHistory {
                session_id: session.id.clone(),
                source_path: home.join("not-written.jsonl").to_string_lossy().into(),
                agent,
                tail: None,
                before_offset: None,
                max_bytes: None,
                known_probe: None,
            },
        )
        .await;
        assert!(matches!(next(&mut output).await, ServerEvent::History {
            session_id, messages, oldest_offset: Some(0), latest_offset: Some(0),
            file_size: Some(0), has_more: false, ..
        } if session_id == session.id && messages.is_empty()));
        if agent != AgentKind::Antigravity {
            continue;
        }
        send(
            &mut input,
            ClientRequest::WatchSession {
                session_id: session.id.clone(),
                source_path: session.source_path.clone(),
                agent,
            },
        )
        .await;
        assert!(
            matches!(next(&mut output).await, ServerEvent::History { messages, .. } if messages.is_empty())
        );
        send(
            &mut input,
            ClientRequest::SendPrompt {
                session_id: session.id.clone(),
                agent,
                cwd: session.cwd.clone(),
                text: "Hello new AGY".into(),
                options: None,
            },
        )
        .await;
        let mut accepted = false;
        let mut user = false;
        let mut reply = false;
        while !accepted || !user || !reply {
            match next(&mut output).await {
                ServerEvent::SendAccepted { session_id, .. } => {
                    assert_eq!(session_id, session.id);
                    accepted = true;
                }
                ServerEvent::HistoryAppend { messages, .. }
                | ServerEvent::History { messages, .. } => {
                    user |= messages.iter().any(|m| m.text == "Hello new AGY");
                    reply |= messages.iter().any(|m| m.text == "First reply");
                }
                ServerEvent::Error { code, message } => panic!("{code}: {message}"),
                _ => {}
            }
        }
        assert_eq!(
            fs::read_to_string(home.join("agy-cwd")).unwrap().trim(),
            home.to_str().unwrap()
        );
        let wire: serde_json::Value =
            serde_json::from_str(fs::read_to_string(home.join("agy-input")).unwrap().trim())
                .unwrap();
        assert_eq!(wire["message"]["content"], "Hello new AGY");
        send(
            &mut input,
            ClientRequest::ArchiveSession {
                session_id: session.id,
                agent,
                source_path: session.source_path,
                kill_process: true,
            },
        )
        .await;
        loop {
            match next(&mut output).await {
                ServerEvent::SessionArchived { archived: true, .. } => break,
                ServerEvent::Error { code, message } => panic!("{code}: {message}"),
                _ => {}
            }
        }
    }
    // Errors other than NotFound must remain visible.
    send(
        &mut input,
        ClientRequest::ReadHistory {
            session_id: "bad-history".into(),
            source_path: home.to_string_lossy().into(),
            agent: AgentKind::Claude,
            tail: None,
            before_offset: None,
            max_bytes: None,
            known_probe: None,
        },
    )
    .await;
    assert!(
        matches!(next(&mut output).await, ServerEvent::Error { code, .. } if code == "read_history_failed")
    );
    drop(input);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    let _ = fs::remove_dir_all(home);
}


#[tokio::test]
async fn browser_cli_roundtrip_through_connected_daemon() {
    use multibot_proto::{BrowserCommand, BrowserResponse};
    use serde_json::json;
    let home = temp_home("browser");
    let bin = env!("CARGO_BIN_EXE_multibotd");
    let mut daemon = Command::new(bin).arg("--stdio").env("HOME", &home)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null())
        .kill_on_drop(true).spawn().unwrap();
    let pid = daemon.id().unwrap();
    let mut input = daemon.stdin.take().unwrap();
    let mut output = BufReader::new(daemon.stdout.take().unwrap()).lines();
    for request in [
        ClientRequest::BrowserBind { session_id:"session-a".into(), binding_id:"window-a".into() },
        ClientRequest::BrowserBind { session_id:"session-b".into(), binding_id:"window-b".into() },
        ClientRequest::Ping { nonce:99 },
    ] { input.write_all(encode_line(&request).unwrap().as_bytes()).await.unwrap(); }
    let line = tokio::time::timeout(Duration::from_secs(5), output.next_line()).await.unwrap().unwrap().unwrap();
    assert!(matches!(decode_line::<ServerEvent>(&line).unwrap(), ServerEvent::Pong { nonce:99, .. }));
    for (args, expected, value) in [
        (vec!["navigate", "https://example.com"], BrowserCommand::Navigate { url:"https://example.com".into() }, json!({"frameId":"a"})),
        (vec!["eval", "1 + 1"], BrowserCommand::Eval { expression:"1 + 1".into() }, json!(2)),
        (vec!["text"], BrowserCommand::ReadText, json!("Page A")),
        (vec!["screenshot"], BrowserCommand::Screenshot, json!({"data":"iVBORw0KGgo="})),
    ] {
        let mut cli = Command::new(bin).args(["browser", "--session", "session-a"]).args(args)
            .env("HOME", &home).stdout(Stdio::piped()).stderr(Stdio::piped())
            .kill_on_drop(true).spawn().unwrap();
        let line = tokio::time::timeout(Duration::from_secs(5), output.next_line()).await.unwrap().unwrap().unwrap();
        let ServerEvent::BrowserCommand { request_id, session_id, binding_id, command } = decode_line::<ServerEvent>(&line).unwrap() else { panic!("unexpected event") };
        assert_eq!(session_id, "session-a");
        assert_eq!(binding_id, "window-a");
        assert_eq!(command, expected);
        let response = BrowserResponse::Value { value };
        let request = ClientRequest::BrowserResult { request_id, binding_id, response:response.clone() };
        input.write_all(encode_line(&request).unwrap().as_bytes()).await.unwrap();
        let stdout = cli.stdout.take().unwrap();
        let mut lines = BufReader::new(stdout).lines();
        let result = tokio::time::timeout(Duration::from_secs(5), lines.next_line()).await.unwrap().unwrap().unwrap();
        assert_eq!(decode_line::<BrowserResponse>(&result).unwrap(), response);
        assert!(tokio::time::timeout(Duration::from_secs(5), cli.wait()).await.unwrap().unwrap().success());
    }
    let rejected = Command::new(bin).args(["browser", "--session", "not-bound", "tabs"])
        .env("HOME", &home).output().await.unwrap();
    assert!(!rejected.status.success());
    drop(input);
    assert!(tokio::time::timeout(Duration::from_secs(5), daemon.wait()).await.unwrap().unwrap().success());
    assert!(!home.join(format!(".multibot/browser-control/{pid}.sock")).exists());
    fs::remove_dir_all(home).unwrap();
}

struct FeatureDaemon {
    child: tokio::process::Child,
    input: tokio::process::ChildStdin,
    output: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
}

impl FeatureDaemon {
    async fn start(home: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_multibotd")).arg("--stdio")
            .env("HOME", home).env("PATH", format!("{}:/usr/bin:/bin", home.join(".local/bin").display()))
            .env("TMUX_TMPDIR", home).env_remove("TMUX").env_remove("MULTIPROMPT_STATE_DIR").env_remove("XDG_STATE_HOME")
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).kill_on_drop(true).spawn().unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut daemon = Self { child, input, output };
        daemon.send(ClientRequest::Ping { nonce: 90 }).await;
        assert!(matches!(daemon.next().await, ServerEvent::Pong { nonce:90, .. }));
        daemon
    }
    async fn send(&mut self, request: ClientRequest) {
        self.input.write_all(encode_line(&request).unwrap().as_bytes()).await.unwrap();
    }
    async fn next(&mut self) -> ServerEvent {
        let line = tokio::time::timeout(Duration::from_secs(8), self.output.next_line()).await.unwrap().unwrap().expect("daemon EOF");
        decode_line(&line).unwrap()
    }
    async fn reply(&mut self) -> ServerEvent {
        loop {
            let event = self.next().await;
            if matches!(event, ServerEvent::SessionUpdated { .. } | ServerEvent::PromptStatus { .. })
                || matches!(&event, ServerEvent::HistoryAppend { messages, latest_offset:None, .. } if messages.is_empty()) { continue; }
            return event;
        }
    }
    async fn stop(mut self) {
        drop(self.input);
        assert!(tokio::time::timeout(Duration::from_secs(8), self.child.wait()).await.unwrap().unwrap().success());
    }
}

async fn wait_for_feature_receipt(path: &std::path::Path, expected: &str) {
    use notify::Watcher;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if let Ok(event) = event && !matches!(event.kind, notify::EventKind::Access(_)) { let _ = tx.send(()); }
    }).unwrap();
    watcher.watch(path.parent().unwrap(), notify::RecursiveMode::NonRecursive).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !fs::read_to_string(path).unwrap_or_default().contains(expected) { rx.recv().await.unwrap(); }
    }).await.expect("fake agent did not persist delivery");
}

#[tokio::test]
async fn antigravity_native_history_survives_daemon_restart_and_native_watch() {
    use serde_json::json;
    let home = temp_home("agy-native-restart");
    let cid = "4d5d5535-249c-4f10-90d9-7ec49c2eeb53";
    let mirror = home.join(".multibot/history/hosted.history.jsonl");
    let native = home.join(".gemini/antigravity-cli/brain").join(cid).join(".system_generated/logs/transcript.jsonl");
    fs::create_dir_all(mirror.parent().unwrap()).unwrap();
    fs::create_dir_all(native.parent().unwrap()).unwrap();
    fs::write(&mirror, format!("{}\n{}\n", json!({"type":"multibot_meta","session_id":"hosted","conversation_id":cid,"cwd":"/repo"}), json!({"type":"multibot_history","row":{"id":"mirror","role":"assistant","text":"Old reply"}}))).unwrap();
    fs::write(home.join(".multibot/metadata.json"), json!({"sessions":{"hosted":{"agent":"antigravity","title":"Postbox","source_path":mirror,"native_session_id":cid}}}).to_string()).unwrap();
    fs::write(&native, "{\"type\":\"PLANNER_RESPONSE\",\"content\":\"Old reply\"}\n").unwrap();
    let mut daemon = FeatureDaemon::start(&home).await;
    daemon.send(ClientRequest::ReadHistory { session_id:"hosted".into(), source_path:mirror.to_string_lossy().into(), agent:AgentKind::Antigravity,
        tail:None, before_offset:None, max_bytes:None, known_probe:None }).await;
    assert!(matches!(daemon.reply().await, ServerEvent::History { messages, .. } if messages.len() == 1));
    daemon.child.kill().await.unwrap();
    drop(daemon);
    writeln!(OpenOptions::new().append(true).open(&native).unwrap(), "{}", json!({"type":"PLANNER_RESPONSE","content":"Saved during restart"})).unwrap();
    let mut daemon = FeatureDaemon::start(&home).await;
    daemon.send(ClientRequest::ListSessions { limit:Some(50), include_archived:None }).await;
    let ServerEvent::Sessions { sessions } = daemon.reply().await else { panic!("expected sessions"); };
    let hosted: Vec<_> = sessions.iter().filter(|s| s.agent == AgentKind::Antigravity).collect();
    assert_eq!(hosted.len(), 1, "native conversation and hosted session must not duplicate");
    assert_eq!(hosted[0].id, "hosted");
    assert_eq!(hosted[0].title, "Postbox");
    assert_eq!(hosted[0].source_path, native.to_string_lossy());
    assert_eq!(hosted[0].last_assistant_preview.as_deref(), Some("Saved during restart"));
    daemon.send(ClientRequest::WatchSession { session_id:"hosted".into(), source_path:mirror.to_string_lossy().into(), agent:AgentKind::Antigravity }).await;
    let ServerEvent::History { messages, .. } = daemon.reply().await else { panic!("expected native snapshot"); };
    assert_eq!(messages.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(), vec!["Old reply", "Saved during restart"]);
    writeln!(OpenOptions::new().append(true).open(&native).unwrap(), "{}", json!({"type":"PLANNER_RESPONSE","content":"Live native reply"})).unwrap();
    assert!(matches!(daemon.reply().await, ServerEvent::HistoryAppend { session_id, messages, .. } if session_id == "hosted" && messages.len() == 1 && messages[0].text == "Live native reply"));
    daemon.stop().await;
    fs::remove_dir_all(home).unwrap();
}

#[tokio::test]
async fn sessions_cli_lists_routes_names_ids_shells_and_stops_agent_ping_pong() {
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    let home = temp_home("cross-session");
    let bins = home.join(".local/bin");
    fs::create_dir_all(&bins).unwrap();
    for (name, script) in [
        ("claude", "#!/bin/sh\nprintf '%s\\n' \"$MULTIBOT_SESSION_ID\" \"$@\" >> \"$HOME/delivered\"\n"),
        ("tmux", "#!/bin/sh\nif [ \"$1\" = list-sessions ]; then printf 'shell\\t1\\tbash\\t/repo\\t1\\tTerminal\\n'; else printf '%s\\n' \"$@\" >> \"$HOME/shell-delivered\"; fi\n"),
    ] {
        let path = bins.join(name); fs::write(&path, script).unwrap(); fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fs::create_dir_all(home.join(".multibot")).unwrap();
    let mut metadata = serde_json::Map::new();
    for (id, title) in [("sender", "Sender"), ("target", "Target"), ("dup-a", "Same"), ("dup-b", "Same")] {
        let path = home.join(".claude/projects/repo").join(format!("{id}.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, format!("{}\n", json!({"type":"user","uuid":"u","sessionId":id,"cwd":"/repo","message":{"content":"Hello"}}))).unwrap();
        metadata.insert(id.into(), json!({"agent":"claude","title":title,"source_path":path,"cwd":home}));
    }
    fs::write(home.join(".multibot/metadata.json"), json!({"sessions":metadata}).to_string()).unwrap();
    let mut daemon = FeatureDaemon::start(&home).await;
    let cli = |args: Vec<String>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_multibotd"));
        command.arg("sessions").args(args).env("HOME", &home).kill_on_drop(true);
        command
    };
    let list = cli(vec!["list".into(), "--json".into()]).output().await.unwrap();
    assert!(list.status.success(), "{}", String::from_utf8_lossy(&list.stderr));
    let list: Vec<multibot_proto::SessionSummary> = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(list.len(), 5);
    assert!(list.iter().any(|s| s.id == "target" && s.title == "Target" && s.cwd.as_ref() == Some(&home.to_string_lossy().to_string())));
    let sent = cli(vec!["send".into(), "Target".into(), "Hello by name".into(), "--from".into(), "sender".into()]).output().await.unwrap();
    assert!(sent.status.success(), "{}", String::from_utf8_lossy(&sent.stderr));
    for (from, target, succeeds) in [("target", "sender", true), ("sender", "target", true), ("target", "sender", true), ("sender", "target", false)] {
        daemon.send(ClientRequest::SendCrossSession { from_session_id:from.into(), target_session_id:target.into(), text:"Fresh reply text".into() }).await;
        let event = daemon.reply().await;
        assert_eq!(matches!(event, ServerEvent::SendAccepted { .. }), succeeds, "{event:?}");
        if !succeeds { assert!(matches!(event, ServerEvent::Error { code, message } if code == "cross_session_failed" && message.contains("depth"))); }
    }
    for name in ["Same", "missing"] {
        let result = cli(vec!["send".into(), name.into(), "No delivery".into()]).output().await.unwrap();
        assert!(!result.status.success());
        if name == "Same" { assert!(String::from_utf8_lossy(&result.stderr).contains("Ambiguous")); }
    }
    let result = cli(vec!["send".into(), "shell".into(), "Literal $text; no shell expansion".into()]).output().await.unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let shell = fs::read_to_string(home.join("shell-delivered")).unwrap();
    assert!(shell.contains("-l\n--\n[Message from CLI]: Literal $text; no shell expansion"));
    for attempt in 1..=5 {
        let result = cli(vec!["send".into(), "dup-a".into(), "Repeated external sender".into(), "--from".into(), "terminal".into()]).output().await.unwrap();
        assert_eq!(result.status.success(), attempt <= 4);
        if attempt == 5 { assert!(String::from_utf8_lossy(&result.stderr).contains("depth")); }
    }
    daemon.send(ClientRequest::SendPrompt { session_id:"sender".into(), agent:AgentKind::Claude, cwd:Some(home.to_string_lossy().into()), text:"New user turn".into(), options:None }).await;
    assert!(matches!(daemon.reply().await, ServerEvent::SendAccepted { .. }));
    let sent = cli(vec!["send".into(), "target".into(), "Hello by ID".into(), "--from".into(), "sender".into()]).output().await.unwrap();
    assert!(sent.status.success(), "{}", String::from_utf8_lossy(&sent.stderr));
    // An acknowledgement alone is insufficient: wait for the fake agent's durable receipt.
    daemon.send(ClientRequest::Ping { nonce:91 }).await;
    assert!(matches!(daemon.reply().await, ServerEvent::Pong { nonce:91, .. }));
    wait_for_feature_receipt(&home.join("delivered"), "[Message from sender]: Hello by ID").await;
    let delivered = fs::read_to_string(home.join("delivered")).unwrap();
    assert!(delivered.contains("[Message from sender]: Hello by name"));
    assert!(delivered.contains("Multibot cross-session depth 4/4"));
    assert!(!delivered.contains("No delivery"));
    if let Some(peak) = read_proc_vm_hwm_kib(daemon.child.id().unwrap()) { assert!(peak < 10 * 1024, "daemon peak {peak} KiB exceeds 10 MiB"); }
    daemon.stop().await;
    fs::remove_dir_all(home).unwrap();
}

#[tokio::test]
async fn sessions_cli_routes_to_the_owning_daemon_and_preserves_cross_daemon_depth() {
    use std::os::unix::fs::PermissionsExt;
    let home = temp_home("cross-daemon");
    let bins = home.join(".local/bin");
    fs::create_dir_all(&bins).unwrap();
    let agy = bins.join("agy");
    fs::write(&agy, r#"#!/bin/sh
printf '{"event":"init","conversation_id":"%s"}\n' "$MULTIBOT_SESSION_ID"
while IFS= read -r frame; do
    printf '%s\n' "$frame" >> "$HOME/receipt-$MULTIBOT_SESSION_ID"
    case "$frame" in
        *"Start first"*|*"Start second"*) ;;
        *) printf '{"event":"result","result":{"status":"SUCCESS"}}\n' ;;
    esac
done
"#).unwrap();
    fs::set_permissions(agy, fs::Permissions::from_mode(0o700)).unwrap();
    let mut first = FeatureDaemon::start(&home).await;
    first.send(ClientRequest::SpawnSession { agent:AgentKind::Antigravity, cwd:home.to_string_lossy().into(), title:Some("First".into()), initial_prompt:Some("Start first".into()), options:None }).await;
    let ServerEvent::SessionSpawned { session:a } = first.reply().await else { panic!("first spawn"); };
    wait_for_feature_receipt(&home.join(format!("receipt-{}", a.id)), "Start first").await;
    let mut second = FeatureDaemon::start(&home).await;
    second.send(ClientRequest::SpawnSession { agent:AgentKind::Antigravity, cwd:home.to_string_lossy().into(), title:Some("Second".into()), initial_prompt:Some("Start second".into()), options:None }).await;
    let ServerEvent::SessionSpawned { session:b } = second.reply().await else { panic!("second spawn"); };
    wait_for_feature_receipt(&home.join(format!("receipt-{}", b.id)), "Start second").await;
    for session in [&a, &b] {
        writeln!(OpenOptions::new().append(true).open(&session.source_path).unwrap(), "{{\"type\":\"multibot_turn_end\",\"status\":\"SUCCESS\"}}").unwrap();
    }
    let list = Command::new(env!("CARGO_BIN_EXE_multibotd")).args(["sessions", "list", "--json"]).env("HOME", &home).output().await.unwrap();
    assert!(list.status.success());
    let list: Vec<multibot_proto::SessionSummary> = serde_json::from_slice(&list.stdout).unwrap();
    assert!(list.iter().find(|s| s.id == a.id).unwrap().working);
    assert!(list.iter().find(|s| s.id == b.id).unwrap().working);
    for (from, target, succeeds) in [(&a.id, &b.id, true), (&b.id, &a.id, true), (&a.id, &b.id, true), (&b.id, &a.id, true), (&a.id, &b.id, false)] {
        let result = Command::new(env!("CARGO_BIN_EXE_multibotd")).args(["sessions", "send", target, "New cross-daemon information"])
            .env("HOME", &home).env("MULTIBOT_SESSION_ID", from).kill_on_drop(true).output().await.unwrap();
        assert_eq!(result.status.success(), succeeds, "{}", String::from_utf8_lossy(&result.stderr));
        if !succeeds { assert!(String::from_utf8_lossy(&result.stderr).contains("depth")); }
    }
    wait_for_feature_receipt(&home.join(format!("receipt-{}", a.id)), "depth 4/4").await;
    wait_for_feature_receipt(&home.join(format!("receipt-{}", b.id)), "depth 3/4").await;
    // Each persistent process must receive the message, not a new process in another daemon.
    assert_eq!(fs::read_to_string(home.join(format!("receipt-{}", a.id))).unwrap().lines().count(), 3);
    assert_eq!(fs::read_to_string(home.join(format!("receipt-{}", b.id))).unwrap().lines().count(), 3);
    first.stop().await;
    second.stop().await;
    fs::remove_dir_all(home).unwrap();
}
