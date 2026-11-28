use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

/// Build version embedded at compile time via `MULTIBOT_BUILD_VERSION` (injected by CI for
/// versioned GitHub Releases), falling back to `CARGO_PKG_VERSION` for local dev builds.
pub fn build_version() -> &'static str {
    option_env!("MULTIBOT_BUILD_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SessionOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<EffortLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<PermissionLevel>,
}

impl SessionOptions {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffortLevel {
    Low,
    Medium,
    High,
    Max,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionLevel {
    Ask,
    Auto,
    FullAccess,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    Claude,
    Antigravity,
    Codex,
    Shell,
    Pi,
    Cursor,
}

impl AgentKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Antigravity => "Antigravity",
            Self::Codex => "Codex",
            Self::Shell => "Shell",
            Self::Pi => "Pi",
            Self::Cursor => "Cursor",
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            Self::Claude => "✳",
            Self::Antigravity => "◈",
            Self::Codex => "⬡",
            Self::Shell => ">_",
            Self::Pi => "π",
            Self::Cursor => "➤",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    System,
    Tool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Running,
    #[default]
    Completed,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DiffBlock {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default)]
    pub added: u32,
    #[serde(default)]
    pub removed: u32,
    #[serde(default)]
    pub lines: Vec<String>,
    #[serde(default)]
    pub truncated_lines: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    #[serde(default)]
    pub status: TodoStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PromptOption {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PromptQuestion {
    pub question: String,
    #[serde(default)]
    pub header: String,
    #[serde(default, alias = "multiSelect")]
    pub multi_select: bool,
    #[serde(default)]
    pub options: Vec<PromptOption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageBlock {
    Text {
        text: String,
    },
    ToolUse {
        #[serde(default)]
        call_id: String,
        name: String,
        #[serde(default)]
        summary: String,
        #[serde(default)]
        detail: String,
        #[serde(default)]
        status: ToolStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff: Option<DiffBlock>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
    },
    Image {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        local_path: Option<String>,
        #[serde(default)]
        alt: String,
    },
    QuestionPrompt {
        prompt_id: String,
        #[serde(default)]
        provider: String,
        #[serde(default)]
        questions: Vec<PromptQuestion>,
    },
}

fn default_slash_trigger() -> String {
    "/".to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlashCommandItem {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_slash_trigger")]
    pub trigger: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub opens_in_terminal: bool,
}

impl Default for SlashCommandItem {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            trigger: default_slash_trigger(),
            source: String::new(),
            opens_in_terminal: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    pub message_id: String,
    pub byte_offset: u64,
    pub snippet: String,
    pub role: MessageRole,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub agent: AgentKind,
    pub title: String,
    pub cwd: Option<String>,
    pub source_path: String,
    pub updated_at_ms: i64,
    pub working: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_assistant_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_assistant_preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_assistant_ms: Option<i64>,
    #[serde(default)]
    pub todos_done: u32,
    #[serde(default)]
    pub todos_total: u32,
    #[serde(default)]
    pub needs_attention: bool,
    #[serde(default)]
    pub archived: bool,
    /// False for legacy server archives that hid a still-running agent (now Done).
    #[serde(default)]
    pub archive_kill_process: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<EffortLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_limit: Option<u64>,
    /// Workspace name recorded by the server-side session metadata (`multiprompt`
    /// `chat-names.json`). The desktop maps this name onto a local workspace so a live
    /// session lands where the server says it belongs instead of the catch-all bucket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

impl Default for SessionSummary {
    fn default() -> Self {
        Self {
            id: String::new(),
            agent: AgentKind::Claude,
            title: String::new(),
            cwd: None,
            source_path: String::new(),
            updated_at_ms: 0,
            working: false,
            last_assistant_id: None,
            last_assistant_preview: None,
            last_assistant_ms: None,
            todos_done: 0,
            todos_total: 0,
            needs_attention: false,
            archived: false,
            archive_kill_process: false,
            model: None,
            effort: None,
            context_tokens: None,
            context_limit: None,
            workspace: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: String,
    pub role: MessageRole,
    pub text: String,
    pub timestamp_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<MessageBlock>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub todos: Vec<TodoItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_byte_range: Option<(u64, u64)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quoted_message_id: Option<String>,
}

impl Default for ChatMessage {
    fn default() -> Self {
        Self {
            id: String::new(),
            role: MessageRole::Assistant,
            text: String::new(),
            timestamp_ms: None,
            blocks: Vec::new(),
            todos: Vec::new(),
            source_byte_range: None,
            quoted_message_id: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ClientRequest {
    Hello {
        client_version: String,
    },
    Ping {
        nonce: u64,
    },
    /// Only active sessions and explicitly saved lifecycle history, not every old transcript.
    ListLifecycleSessions {
        #[serde(default)]
        limit: Option<usize>,
    },
    ListSessions {
        #[serde(default)]
        limit: Option<usize>,
        /// When omitted or `false`, only live and recently active sessions come back.
        /// `true` also returns archived and historical transcripts (the slow, full listing).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        include_archived: Option<bool>,
    },
    ReadHistory {
        session_id: String,
        source_path: String,
        agent: AgentKind,
        #[serde(default)]
        tail: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before_offset: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_bytes: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        known_probe: Option<String>,
    },
    WatchSession {
        session_id: String,
        source_path: String,
        agent: AgentKind,
    },
    UnwatchSession {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    WatchDirectory,
    UnwatchDirectory,
    SendPrompt {
        session_id: String,
        agent: AgentKind,
        #[serde(default)]
        cwd: Option<String>,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        options: Option<SessionOptions>,
    },
    InterruptSession {
        session_id: String,
        agent: AgentKind,
    },
    SteerSession {
        session_id: String,
        agent: AgentKind,
        #[serde(default)]
        cwd: Option<String>,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        options: Option<SessionOptions>,
    },
    CancelQueuedPrompt {
        session_id: String,
        message_id: String,
    },
    SpawnSession {
        agent: AgentKind,
        cwd: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        initial_prompt: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        options: Option<SessionOptions>,
    },
    RenameSession {
        session_id: String,
        agent: AgentKind,
        source_path: String,
        title: String,
    },
    ArchiveSession {
        session_id: String,
        agent: AgentKind,
        source_path: String,
        #[serde(default)]
        kill_process: bool,
    },
    UnarchiveSession {
        session_id: String,
    },
    SearchHistory {
        request_id: String,
        session_id: String,
        source_path: String,
        agent: AgentKind,
        query: String,
    },
    ListCommands {
        agent: AgentKind,
        #[serde(default)]
        cwd: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ServerEvent {
    HelloAck {
        protocol_version: u32,
        daemon_version: String,
        hostname: String,
        pid: u32,
        #[serde(default)]
        binary_hash: String,
    },
    Pong {
        nonce: u64,
        server_time_ms: i64,
    },
    Sessions {
        sessions: Vec<SessionSummary>,
    },
    History {
        session_id: String,
        messages: Vec<ChatMessage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        oldest_offset: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        latest_offset: Option<u64>,
        #[serde(default)]
        has_more: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file_size: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file_mtime_ms: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        probe_sha256: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        todos: Vec<TodoItem>,
    },
    HistoryAppend {
        session_id: String,
        messages: Vec<ChatMessage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        latest_offset: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        working: Option<bool>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        todos: Vec<TodoItem>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        probe_sha256: Option<String>,
    },
    SendAccepted {
        session_id: String,
        detail: String,
    },
    SessionUpdated {
        session: SessionSummary,
    },
    SessionSpawned {
        session: SessionSummary,
    },
    SessionArchived {
        session_id: String,
        archived: bool,
    },
    SessionInterrupted {
        session_id: String,
        ok: bool,
        #[serde(default)]
        detail: String,
    },
    PromptStatus {
        session_id: String,
        message_id: String,
        status: String,
        #[serde(default)]
        detail: String,
    },
    SearchResults {
        request_id: String,
        session_id: String,
        hits: Vec<SearchHit>,
        #[serde(default)]
        truncated: bool,
    },
    Commands {
        agent: AgentKind,
        #[serde(default)]
        cwd: Option<String>,
        items: Vec<SlashCommandItem>,
    },
    Error {
        code: String,
        message: String,
    },
}

pub fn encode_line<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    Ok(line)
}

pub fn decode_line<T: for<'de> Deserialize<'de>>(line: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_stats_are_optional_and_roundtrip() {
        let legacy = r#"{"id":"old","agent":"claude","title":"Old","cwd":null,"source_path":"old.jsonl","updated_at_ms":0,"working":false}"#;
        let mut session: SessionSummary = serde_json::from_str(legacy).unwrap();
        assert_eq!(session.effort, None);
        assert_eq!(session.context_tokens, None);
        assert_eq!(session.context_limit, None);
        session.effort = Some(EffortLevel::High);
        session.context_tokens = Some(42_000);
        session.context_limit = Some(200_000);
        let json = serde_json::to_string(&session).unwrap();
        assert_eq!(
            serde_json::from_str::<SessionSummary>(&json).unwrap(),
            session
        );
    }

    #[test]
    fn shell_agent_wire_label_and_glyph() {
        assert_eq!(
            serde_json::to_string(&AgentKind::Shell).unwrap(),
            "\"shell\""
        );
        assert_eq!(
            serde_json::from_str::<AgentKind>("\"shell\"").unwrap(),
            AgentKind::Shell
        );
        assert_eq!(AgentKind::Shell.label(), "Shell");
        assert_eq!(AgentKind::Shell.glyph(), ">_");
    }

    #[test]
    fn pi_cursor_wire_names_and_labels_are_stable() {
        for (kind, wire, label) in [
            (AgentKind::Pi, "pi", "Pi"),
            (AgentKind::Cursor, "cursor", "Cursor"),
        ] {
            assert_eq!(serde_json::to_string(&kind).unwrap(), format!("\"{wire}\""));
            assert_eq!(
                serde_json::from_str::<AgentKind>(&format!("\"{wire}\"")).unwrap(),
                kind
            );
            assert_eq!(kind.label(), label);
            assert!(!kind.glyph().is_empty());
        }
    }

    #[test]
    fn composer_options_roundtrip_and_legacy_requests() {
        let options = SessionOptions {
            model: Some("opus".into()),
            effort: Some(EffortLevel::High),
            permission: Some(PermissionLevel::Auto),
        };
        for request in [
            ClientRequest::SendPrompt {
                session_id: "s1".into(),
                agent: AgentKind::Claude,
                cwd: None,
                text: "hello".into(),
                options: Some(options.clone()),
            },
            ClientRequest::SteerSession {
                session_id: "s1".into(),
                agent: AgentKind::Claude,
                cwd: None,
                text: "hello".into(),
                options: Some(options.clone()),
            },
            ClientRequest::SpawnSession {
                agent: AgentKind::Claude,
                cwd: ".".into(),
                title: None,
                initial_prompt: Some("hello".into()),
                options: Some(options.clone()),
            },
        ] {
            let wire = encode_line(&request).unwrap();
            assert_eq!(decode_line::<ClientRequest>(&wire).unwrap(), request);
            let mut legacy = serde_json::from_str::<serde_json::Value>(&wire).unwrap();
            legacy.as_object_mut().unwrap().remove("options");
            let request = decode_line::<ClientRequest>(&legacy.to_string()).unwrap();
            let options = match &request {
                ClientRequest::SendPrompt { options, .. }
                | ClientRequest::SteerSession { options, .. }
                | ClientRequest::SpawnSession { options, .. } => options,
                _ => unreachable!(),
            };
            assert_eq!(*options, None);
            assert!(!encode_line(&request).unwrap().contains("options"));
        }
        let partial: SessionOptions = serde_json::from_str(r#"{"model":"sonnet"}"#).unwrap();
        assert_eq!(partial.effort, None);
        assert_eq!(partial.permission, None);
        assert!(serde_json::from_str::<SessionOptions>(r#"{"permission":"turbo"}"#).is_err());
    }

    #[test]
    fn build_version_falls_back_to_pkg_version() {
        let ver = build_version();
        assert!(!ver.is_empty());
        assert_eq!(
            ver,
            option_env!("MULTIBOT_BUILD_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn roundtrip_client_request_and_server_event() {
        let req = ClientRequest::ReadHistory {
            session_id: "sess-1".into(),
            source_path: "/home/u/.claude/projects/p/s.jsonl".into(),
            agent: AgentKind::Claude,
            tail: Some(50),
            before_offset: Some(1024),
            max_bytes: Some(524_288),
            known_probe: Some("deadbeef".into()),
        };
        let encoded = encode_line(&req).unwrap();
        assert!(encoded.ends_with('\n'));
        let decoded: ClientRequest = decode_line(&encoded).unwrap();
        assert_eq!(decoded, req);

        let ev = ServerEvent::HelloAck {
            protocol_version: PROTOCOL_VERSION,
            daemon_version: "0.1.0".into(),
            hostname: "vps-1".into(),
            pid: 42,
            binary_hash: "0123456789ab".into(),
        };
        let encoded_ev = encode_line(&ev).unwrap();
        let decoded_ev: ServerEvent = decode_line(&encoded_ev).unwrap();
        assert_eq!(decoded_ev, ev);
    }

    #[test]
    fn slice1_backward_compat_requests() {
        let hello: ClientRequest =
            decode_line(r#"{"op":"hello","client_version":"0.1.0"}"#).unwrap();
        assert_eq!(
            hello,
            ClientRequest::Hello {
                client_version: "0.1.0".into(),
            }
        );

        let ping: ClientRequest = decode_line(r#"{"op":"ping","nonce":1}"#).unwrap();
        assert_eq!(ping, ClientRequest::Ping { nonce: 1 });

        let list1: ClientRequest = decode_line(r#"{"op":"list_sessions","limit":50}"#).unwrap();
        assert_eq!(
            list1,
            ClientRequest::ListSessions {
                limit: Some(50),
                include_archived: None
            }
        );

        let list2: ClientRequest = decode_line(r#"{"op":"list_sessions"}"#).unwrap();
        assert_eq!(
            list2,
            ClientRequest::ListSessions {
                limit: None,
                include_archived: None
            }
        );

        let list3: ClientRequest =
            decode_line(r#"{"op":"list_sessions","include_archived":true}"#).unwrap();
        assert_eq!(
            list3,
            ClientRequest::ListSessions {
                limit: None,
                include_archived: Some(true)
            }
        );

        let read: ClientRequest = decode_line(
            r#"{"op":"read_history","session_id":"s1","source_path":"/p.jsonl","agent":"claude","tail":50}"#,
        )
        .unwrap();
        assert_eq!(
            read,
            ClientRequest::ReadHistory {
                session_id: "s1".into(),
                source_path: "/p.jsonl".into(),
                agent: AgentKind::Claude,
                tail: Some(50),
                before_offset: None,
                max_bytes: None,
                known_probe: None,
            }
        );

        let watch: ClientRequest = decode_line(
            r#"{"op":"watch_session","session_id":"s1","source_path":"/p.jsonl","agent":"codex"}"#,
        )
        .unwrap();
        assert_eq!(
            watch,
            ClientRequest::WatchSession {
                session_id: "s1".into(),
                source_path: "/p.jsonl".into(),
                agent: AgentKind::Codex,
            }
        );

        let unwatch_unit: ClientRequest = decode_line(r#"{"op":"unwatch_session"}"#).unwrap();
        assert_eq!(
            unwatch_unit,
            ClientRequest::UnwatchSession { session_id: None }
        );
        assert_eq!(
            serde_json::to_string(&unwatch_unit).unwrap(),
            r#"{"op":"unwatch_session"}"#
        );

        let unwatch_null: ClientRequest =
            decode_line(r#"{"op":"unwatch_session","session_id":null}"#).unwrap();
        assert_eq!(
            unwatch_null,
            ClientRequest::UnwatchSession { session_id: None }
        );

        let unwatch_id: ClientRequest =
            decode_line(r#"{"op":"unwatch_session","session_id":"s1"}"#).unwrap();
        assert_eq!(
            unwatch_id,
            ClientRequest::UnwatchSession {
                session_id: Some("s1".into()),
            }
        );

        let send: ClientRequest = decode_line(
            r#"{"op":"send_prompt","session_id":"s1","agent":"antigravity","cwd":null,"text":"hi"}"#,
        )
        .unwrap();
        assert_eq!(
            send,
            ClientRequest::SendPrompt {
                session_id: "s1".into(),
                agent: AgentKind::Antigravity,
                cwd: None,
                text: "hi".into(),
                options: None,
            }
        );
    }

    #[test]
    fn slice1_backward_compat_events() {
        let hello_ack: ServerEvent = decode_line(
            r#"{"event":"hello_ack","protocol_version":1,"daemon_version":"0.1.0","hostname":"vps","pid":99}"#,
        )
        .unwrap();
        assert_eq!(
            hello_ack,
            ServerEvent::HelloAck {
                protocol_version: 1,
                daemon_version: "0.1.0".into(),
                hostname: "vps".into(),
                pid: 99,
                binary_hash: String::new(),
            }
        );

        let sessions_ev: ServerEvent = decode_line(
            r#"{"event":"sessions","sessions":[{"id":"s1","agent":"claude","title":"demo","cwd":"/repo","source_path":"/p.jsonl","updated_at_ms":1000,"working":true}]}"#,
        )
        .unwrap();
        match sessions_ev {
            ServerEvent::Sessions { sessions } => {
                assert_eq!(sessions.len(), 1);
                let s = &sessions[0];
                assert_eq!(s.todos_done, 0);
                assert_eq!(s.todos_total, 0);
                assert!(!s.needs_attention);
                assert!(!s.archived);
                assert_eq!(s.last_assistant_id, None);
                assert_eq!(s.last_assistant_preview, None);
                assert_eq!(s.last_assistant_ms, None);
                assert_eq!(s.model, None);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let history_ev: ServerEvent = decode_line(
            r#"{"event":"history","session_id":"s1","messages":[{"id":"m1","role":"user","text":"hello","timestamp_ms":100}]}"#,
        )
        .unwrap();
        match history_ev {
            ServerEvent::History {
                session_id,
                messages,
                oldest_offset,
                latest_offset,
                has_more,
                file_size,
                file_mtime_ms,
                probe_sha256,
                todos,
            } => {
                assert_eq!(session_id, "s1");
                assert_eq!(messages.len(), 1);
                assert!(messages[0].blocks.is_empty());
                assert!(messages[0].todos.is_empty());
                assert_eq!(messages[0].source_byte_range, None);
                assert_eq!(messages[0].quoted_message_id, None);
                assert_eq!(oldest_offset, None);
                assert_eq!(latest_offset, None);
                assert!(!has_more);
                assert_eq!(file_size, None);
                assert_eq!(file_mtime_ms, None);
                assert_eq!(probe_sha256, None);
                assert!(todos.is_empty());
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let append_ev: ServerEvent = decode_line(
            r#"{"event":"history_append","session_id":"s1","messages":[{"id":"m2","role":"assistant","text":"ok","timestamp_ms":200}]}"#,
        )
        .unwrap();
        match append_ev {
            ServerEvent::HistoryAppend {
                latest_offset,
                working,
                todos,
                probe_sha256,
                ..
            } => {
                assert_eq!(latest_offset, None);
                assert_eq!(working, None);
                assert!(todos.is_empty());
                assert_eq!(probe_sha256, None);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn roundtrip_all_rich_blocks_and_new_variants() {
        // Verify multiSelect camelCase alias on PromptQuestion.
        let camel_q: PromptQuestion = serde_json::from_str(
            r#"{"question":"Pick one","header":"Mode","multiSelect":true,"options":[{"label":"Fast","description":"Quick path"}]}"#,
        )
        .unwrap();
        assert!(camel_q.multi_select);

        let rich_msg = ChatMessage {
            id: "m-rich".into(),
            role: MessageRole::Assistant,
            text: "Summary".into(),
            timestamp_ms: Some(123456),
            blocks: vec![
                MessageBlock::Text {
                    text: "Here is the diff".into(),
                },
                MessageBlock::ToolUse {
                    call_id: "call_1".into(),
                    name: "Edit".into(),
                    summary: "Edit src/lib.rs".into(),
                    detail: "replace line 1".into(),
                    status: ToolStatus::Completed,
                    diff: Some(DiffBlock {
                        path: Some("src/lib.rs".into()),
                        added: 2,
                        removed: 1,
                        lines: vec!["-old".into(), "+new1".into(), "+new2".into()],
                        truncated_lines: 0,
                    }),
                    output: Some("ok".into()),
                },
                MessageBlock::Image {
                    url: Some("https://example.com/img.png".into()),
                    local_path: Some("/tmp/img.png".into()),
                    alt: "screenshot".into(),
                },
                MessageBlock::QuestionPrompt {
                    prompt_id: "q1".into(),
                    provider: "claude".into(),
                    questions: vec![camel_q],
                },
            ],
            todos: vec![
                TodoItem {
                    content: "Build".into(),
                    status: TodoStatus::Completed,
                },
                TodoItem {
                    content: "Ship".into(),
                    status: TodoStatus::InProgress,
                },
            ],
            source_byte_range: Some((10, 420)),
            quoted_message_id: Some("m-prev".into()),
        };

        let requests = vec![
            ClientRequest::WatchDirectory,
            ClientRequest::UnwatchDirectory,
            ClientRequest::InterruptSession {
                session_id: "s1".into(),
                agent: AgentKind::Claude,
            },
            ClientRequest::SteerSession {
                session_id: "s1".into(),
                agent: AgentKind::Codex,
                cwd: Some("/repo".into()),
                text: "focus on tests".into(),
                options: None,
            },
            ClientRequest::CancelQueuedPrompt {
                session_id: "s1".into(),
                message_id: "q-1".into(),
            },
            ClientRequest::SpawnSession {
                agent: AgentKind::Antigravity,
                cwd: "/repo".into(),
                title: Some("New chat".into()),
                initial_prompt: Some("hello".into()),
                options: None,
            },
            ClientRequest::RenameSession {
                session_id: "s1".into(),
                agent: AgentKind::Claude,
                source_path: "/p.jsonl".into(),
                title: "Renamed".into(),
            },
            ClientRequest::ArchiveSession {
                session_id: "s1".into(),
                agent: AgentKind::Claude,
                source_path: "/p.jsonl".into(),
                kill_process: true,
            },
            ClientRequest::UnarchiveSession {
                session_id: "s1".into(),
            },
            ClientRequest::SearchHistory {
                request_id: "req-1".into(),
                session_id: "s1".into(),
                source_path: "/p.jsonl".into(),
                agent: AgentKind::Claude,
                query: "needle".into(),
            },
            ClientRequest::ListCommands {
                agent: AgentKind::Claude,
                cwd: Some("/repo".into()),
            },
        ];
        for req in requests {
            let line = encode_line(&req).unwrap();
            let decoded: ClientRequest = decode_line(&line).unwrap();
            assert_eq!(decoded, req);
        }

        let summary = SessionSummary {
            id: "s1".into(),
            agent: AgentKind::Claude,
            title: "Rich session".into(),
            cwd: Some("/repo".into()),
            source_path: "/p.jsonl".into(),
            updated_at_ms: 999,
            working: false,
            last_assistant_id: Some("m-rich".into()),
            last_assistant_preview: Some("Summary".into()),
            last_assistant_ms: Some(123456),
            todos_done: 1,
            todos_total: 2,
            needs_attention: true,
            archived: false,
            archive_kill_process: false,
            model: Some("claude-opus".into()),
            effort: Some(EffortLevel::High),
            context_tokens: Some(42_000),
            context_limit: Some(200_000),
            workspace: Some("Yeo UX".into()),
        };

        let events = vec![
            ServerEvent::History {
                session_id: "s1".into(),
                messages: vec![rich_msg.clone()],
                oldest_offset: Some(0),
                latest_offset: Some(512),
                has_more: true,
                file_size: Some(2048),
                file_mtime_ms: Some(123456),
                probe_sha256: Some("abc123".into()),
                todos: rich_msg.todos.clone(),
            },
            ServerEvent::SessionUpdated {
                session: summary.clone(),
            },
            ServerEvent::SessionSpawned { session: summary },
            ServerEvent::SessionArchived {
                session_id: "s1".into(),
                archived: true,
            },
            ServerEvent::SessionInterrupted {
                session_id: "s1".into(),
                ok: true,
                detail: "SIGINT".into(),
            },
            ServerEvent::PromptStatus {
                session_id: "s1".into(),
                message_id: "q-1".into(),
                status: "queued".into(),
                detail: String::new(),
            },
            ServerEvent::SearchResults {
                request_id: "req-1".into(),
                session_id: "s1".into(),
                hits: vec![SearchHit {
                    message_id: "m-rich".into(),
                    byte_offset: 10,
                    snippet: "needle found".into(),
                    role: MessageRole::Assistant,
                }],
                truncated: false,
            },
            ServerEvent::Commands {
                agent: AgentKind::Claude,
                cwd: Some("/repo".into()),
                items: vec![SlashCommandItem {
                    name: "compact".into(),
                    description: "Compact context".into(),
                    trigger: "/".into(),
                    source: "builtin".into(),
                    opens_in_terminal: false,
                }],
            },
        ];
        for ev in events {
            let line = encode_line(&ev).unwrap();
            let decoded: ServerEvent = decode_line(&line).unwrap();
            assert_eq!(decoded, ev);
        }
    }

    #[test]
    fn slash_command_item_default_matches_serde_default() {
        assert_eq!(SlashCommandItem::default().trigger, "/");
        let decoded: SlashCommandItem = serde_json::from_str(r#"{"name":"plan"}"#).unwrap();
        assert_eq!(decoded.trigger, "/");
        assert_eq!(
            decoded,
            SlashCommandItem {
                name: "plan".into(),
                ..SlashCommandItem::default()
            }
        );
    }
}
