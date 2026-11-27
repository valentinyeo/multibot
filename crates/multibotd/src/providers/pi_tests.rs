use super::*;
use crate::providers::{
    DeltaRead, TailActivity, discover_all_sessions_owned, read_history_delta, read_history_page,
};
use serde_json::json;
use std::fs::{self, OpenOptions};
use std::io::Write;

fn home(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "multibot-pi-{tag}-{}-{}",
        std::process::id(),
        super::super::now_ms()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn discovers_and_prunes_before_parsing() {
    let home = home("discovery");
    let root = home.join(".pi/agent/sessions/--repo--");
    fs::create_dir_all(&root).unwrap();
    let path = root.join("2026-11-27_live-pi-session.jsonl");
    let body = concat!(
        "{\"type\":\"session\",\"id\":\"live-pi-session\",\"cwd\":\"/repo\"}\n",
        "{\"type\":\"message\",\"id\":\"u1\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n"
    );
    fs::write(&path, body).unwrap();
    fs::write(root.join("2026_dead-pi-session.jsonl"), "invalid json\n").unwrap();
    assert!(
        PiProvider
            .discover_paths(&home, 10, &|_, _| false)
            .is_empty()
    );
    assert!(discover_all_sessions_owned(&home, 10, false).is_empty());
    let host = home.join(".local/state/multiprompt/agent-host");
    fs::create_dir_all(&host).unwrap();
    let record = format!(
        "{{\"pid\":{},\"sid\":\"provider-pi-session\",\"context_session\":\"live-pi-session\",\"tmux\":\"live-pi-chat\"}}",
        std::process::id()
    );
    fs::write(host.join("pi.state.json"), &record).unwrap();
    fs::write(
        home.join(".local/state/multiprompt/chat-names.json"),
        r#"{"live-pi-chat":{"name":"Pi Work","sid":"provider-pi-session","workspace":"Coding"}}"#,
    )
    .unwrap();
    let sessions = discover_all_sessions_owned(&home, 10, false);
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].agent, AgentKind::Pi);
    assert_eq!(sessions[0].id, "live-pi-session");
    assert_eq!(sessions[0].title, "Pi Work");
    assert_eq!(sessions[0].cwd.as_deref(), Some("/repo"));
    assert_eq!(sessions[0].workspace.as_deref(), Some("Coding"));
    assert_eq!(fs::read_to_string(&path).unwrap(), body);
    assert_eq!(
        fs::read_to_string(host.join("pi.state.json")).unwrap(),
        record
    );
    fs::remove_dir_all(home).unwrap();
}

#[test]
fn parses_messages_tools_model_title_and_scrubs_images() {
    let rows = [
        json!({"type":"session","id":"p1","cwd":"/repo"}),
        json!({"type":"model_change","modelId":"test-model"}),
        json!({"type":"session_info","name":"Named Pi"}),
        json!({"type":"message","id":"u1","timestamp":"2026-11-27T10:00:00Z","message":{"role":"user","content":[{"type":"text","text":"hi"},{"type":"image","data":"A".repeat(500)}]}}),
        json!({"type":"message","id":"a1","message":{"role":"assistant","content":[{"type":"text","text":"working"},{"type":"toolCall","id":"t1","name":"bash","arguments":{"command":"pwd"}}]}}),
        json!({"type":"message","id":"r1","message":{"role":"toolResult","toolCallId":"t1","content":[{"type":"text","text":"/repo"}],"isError":false}}),
        json!({"type":"message","id":"a2","message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}),
    ];
    let payload: String = rows.iter().map(|v| format!("{v}\n")).collect();
    let parsed = PiProvider.parse_slice(payload.as_bytes(), 31);
    assert_eq!(parsed.session_id_hint.as_deref(), Some("p1"));
    assert_eq!(parsed.model.as_deref(), Some("test-model"));
    assert_eq!(parsed.title_hint.as_deref(), Some("Named Pi"));
    assert!(parsed.messages[0].text.contains("[image omitted]"));
    assert!(!format!("{:?}", parsed.messages).contains(&"A".repeat(500)));
    assert!(
        parsed
            .messages
            .iter()
            .flat_map(|m| &m.blocks)
            .any(|b| matches!(
                b,
                multibot_proto::MessageBlock::ToolUse {
                    status: multibot_proto::ToolStatus::Completed,
                    ..
                }
            ))
    );
    assert_eq!(parsed.tail_activity, TailActivity::Idle);
    assert_eq!(parsed.last_assistant_id.as_deref(), Some("a2"));
    assert!(parsed.messages[0].source_byte_range.unwrap().0 >= 31);
}

#[test]
fn pages_history_and_tails_complete_lines_only() {
    let home = home("history");
    let path = home.join("session.jsonl");
    let rows: String = (0..12).map(|i| format!("{}\n", json!({"type":"message","id":format!("m{i}"),"message":{"role":"assistant","content":format!("reply {i}")}}))).collect();
    fs::write(&path, &rows).unwrap();
    let page = read_history_page(AgentKind::Pi, &path, None, Some(512), Some(3), None).unwrap();
    assert!(page.has_more);
    assert!(page.messages.len() <= 3);
    let older = read_history_page(
        AgentKind::Pi,
        &path,
        Some(page.oldest_offset),
        Some(512),
        Some(3),
        None,
    )
    .unwrap();
    assert!(
        older
            .messages
            .iter()
            .all(|m| !page.messages.iter().any(|n| n.id == m.id))
    );
    let next =
        json!({"type":"message","id":"live","message":{"role":"assistant","content":"live reply"}})
            .to_string();
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(next.as_bytes()).unwrap();
    assert!(matches!(
        read_history_delta(
            AgentKind::Pi,
            &path,
            page.latest_offset,
            Some(&page.probe_sha256)
        )
        .unwrap(),
        DeltaRead::Unchanged { .. }
    ));
    file.write_all(b"\n").unwrap();
    let DeltaRead::Appended { messages, .. } = read_history_delta(
        AgentKind::Pi,
        &path,
        page.latest_offset,
        Some(&page.probe_sha256),
    )
    .unwrap() else {
        panic!("expected delta")
    };
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].text, "live reply");
    fs::remove_dir_all(home).unwrap();
}
