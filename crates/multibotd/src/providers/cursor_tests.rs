use super::*;
use crate::providers::{
    DeltaRead, MetadataStore, SessionMetaEntry, discover_all_sessions_owned, read_history_delta,
    read_history_page,
};
use std::fs::{self, OpenOptions};
use std::io::Write;

fn home(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "multibot-cursor-{tag}-{}-{}",
        std::process::id(),
        super::super::now_ms()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn discovers_native_transcripts_with_active_pruning_and_legacy_names() {
    let home = home("discovery");
    let native =
        home.join(".cursor/projects/repo/agent-transcripts/cursor-native/cursor-native.jsonl");
    fs::create_dir_all(native.parent().unwrap()).unwrap();
    let body = "{\"role\":\"user\",\"message\":{\"content\":\"hello\"}}\n";
    fs::write(&native, body).unwrap();
    fs::write(
        home.join(".cursor/projects/repo/unrelated.jsonl"),
        "invalid\n",
    )
    .unwrap();
    let dead = native
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("cursor-dead/cursor-dead.jsonl");
    fs::create_dir_all(dead.parent().unwrap()).unwrap();
    fs::write(dead, "invalid\n").unwrap();
    assert!(discover_all_sessions_owned(&home, 20, false).is_empty());
    let host = home.join(".local/state/multiprompt/agent-host");
    fs::create_dir_all(&host).unwrap();
    let config =
        r#"{"provider":"cursor","sid":"cursor-provider","cwd":"/repo","tmux":"live-cursor"}"#;
    fs::write(host.join("hash.config.json"), config).unwrap();
    fs::write(host.join("hash.state.json"), format!("{{\"pid\":{},\"sid\":\"cursor-provider\",\"native_session\":\"cursor-native\",\"tmux\":\"live-cursor\"}}", std::process::id())).unwrap();
    fs::write(
        home.join(".local/state/multiprompt/chat-names.json"),
        r#"{"live-cursor":{"name":"Cursor Work","sid":"cursor-provider","workspace":"Coding"}}"#,
    )
    .unwrap();
    assert!(
        CursorProvider
            .discover_paths(&home, 20, &|_, _| false)
            .is_empty()
    );
    assert_eq!(
        CursorProvider.discover_paths(&home, 20, &|_, _| true).len(),
        2
    );
    let sessions = discover_all_sessions_owned(&home, 20, false);
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, "cursor-native");
    assert_eq!(sessions[0].title, "Cursor Work");
    assert_eq!(sessions[0].cwd.as_deref(), Some("/repo"));
    assert_eq!(sessions[0].workspace.as_deref(), Some("Coding"));
    fs::remove_file(host.join("hash.state.json")).unwrap();
    let mut metadata = MetadataStore::default();
    metadata.sessions.insert(
        "cursor-native".into(),
        SessionMetaEntry {
            agent: Some(AgentKind::Cursor),
            source_path: Some(native.to_string_lossy().into()),
            ..Default::default()
        },
    );
    metadata.save(&home).unwrap();
    assert_eq!(discover_all_sessions_owned(&home, 20, false).len(), 1);
    assert_eq!(
        fs::read_to_string(host.join("hash.config.json")).unwrap(),
        config
    );
    assert_eq!(fs::read_to_string(&native).unwrap(), body);
    fs::remove_dir_all(home).unwrap();
}

#[test]
fn parses_native_tool_records_without_base64() {
    let rows = [
        json!({"role":"user","message":{"content":[{"type":"text","text":"hello"}]}}),
        json!({"role":"assistant","message":{"content":[{"type":"text","text":format!("image data:image/png;base64,{}", "A".repeat(500))}]}}),
        json!({"role":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"bash","input":{"command":"pwd"}}]}}),
        json!({"role":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"failed"}]}}),
        json!({"role":"assistant","message":{"content":"done"}}),
    ];
    let payload: String = rows.iter().map(|v| format!("{v}\n")).collect();
    let parsed = CursorProvider.parse_slice(payload.as_bytes(), 10);
    assert_eq!(parsed.messages[0].role, MessageRole::User);
    assert_eq!(parsed.messages[0].text, "hello");
    assert!(!format!("{:?}", parsed.messages).contains(&"A".repeat(500)));
    assert!(
        parsed
            .messages
            .iter()
            .flat_map(|m| &m.blocks)
            .any(|b| matches!(
                b,
                multibot_proto::MessageBlock::ToolUse {
                    status: multibot_proto::ToolStatus::Error,
                    ..
                }
            ))
    );
    assert_eq!(parsed.tail_activity, TailActivity::Idle);
    assert!(parsed.last_assistant_id.unwrap().starts_with("cursor-"));
}

#[test]
fn pages_and_tails_native_cursor_records() {
    let home = home("history");
    let path = home.join("cursor.jsonl");
    let rows: String = (0..12)
        .map(|i| {
            format!(
                "{}\n",
                json!({"role":"assistant","message":{"content":format!("reply {i}")}})
            )
        })
        .collect();
    fs::write(&path, &rows).unwrap();
    let page = read_history_page(AgentKind::Cursor, &path, None, Some(300), Some(2), None).unwrap();
    assert!(page.has_more);
    let older = read_history_page(
        AgentKind::Cursor,
        &path,
        Some(page.oldest_offset),
        Some(300),
        Some(2),
        None,
    )
    .unwrap();
    assert!(!older.messages.is_empty());
    assert!(
        older
            .messages
            .iter()
            .all(|m| !page.messages.iter().any(|n| n.id == m.id))
    );
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"{\"role\":\"assistant\",\"message\":{\"content\":\"live\"}}")
        .unwrap();
    assert!(matches!(
        read_history_delta(
            AgentKind::Cursor,
            &path,
            page.latest_offset,
            Some(&page.probe_sha256)
        )
        .unwrap(),
        DeltaRead::Unchanged { .. }
    ));
    file.write_all(b"\n").unwrap();
    let DeltaRead::Appended { messages, .. } = read_history_delta(
        AgentKind::Cursor,
        &path,
        page.latest_offset,
        Some(&page.probe_sha256),
    )
    .unwrap() else {
        panic!("expected delta")
    };
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].text, "live");
    assert!(messages[0].id.starts_with("cursor-"));
    fs::remove_dir_all(home).unwrap();
}
