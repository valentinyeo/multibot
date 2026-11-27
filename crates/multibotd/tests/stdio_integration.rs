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
    if !std::process::Command::new("tmux")
        .arg("-V")
        .stdout(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
    {
        return;
    }
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
    // Killing our only remaining session must also remove the daemon's metadata fallback.
    assert!(
        Command::new("tmux")
            .args(["kill-session", "-t", &format!("={}", session.id)])
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
                && sessions.iter().all(|s| s.agent != AgentKind::Shell)
            {
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
