use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    Claude,
    Antigravity,
    Codex,
}

impl AgentKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Antigravity => "Antigravity",
            Self::Codex => "Codex",
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            Self::Claude => "✳",
            Self::Antigravity => "◈",
            Self::Codex => "⬡",
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub agent: AgentKind,
    pub title: String,
    pub cwd: Option<String>,
    pub source_path: String,
    pub updated_at_ms: i64,
    pub working: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: String,
    pub role: MessageRole,
    pub text: String,
    pub timestamp_ms: Option<i64>,
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
    ListSessions {
        limit: Option<usize>,
    },
    ReadHistory {
        session_id: String,
        source_path: String,
        agent: AgentKind,
        tail: Option<usize>,
    },
    WatchSession {
        session_id: String,
        source_path: String,
        agent: AgentKind,
    },
    UnwatchSession,
    SendPrompt {
        session_id: String,
        agent: AgentKind,
        cwd: Option<String>,
        text: String,
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
    },
    HistoryAppend {
        session_id: String,
        messages: Vec<ChatMessage>,
    },
    SendAccepted {
        session_id: String,
        detail: String,
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
    fn roundtrip_client_request_and_server_event() {
        let req = ClientRequest::ReadHistory {
            session_id: "sess-1".into(),
            source_path: "/home/u/.claude/projects/p/s.jsonl".into(),
            agent: AgentKind::Claude,
            tail: Some(50),
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
        };
        let encoded_ev = encode_line(&ev).unwrap();
        let decoded_ev: ServerEvent = decode_line(&encoded_ev).unwrap();
        assert_eq!(decoded_ev, ev);
    }
}
