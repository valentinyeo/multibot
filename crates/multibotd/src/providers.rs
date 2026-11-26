use anyhow::Result;
use multibot_proto::{AgentKind, ChatMessage, MessageRole, SessionSummary};
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

pub trait AgentProvider: Send + Sync {
    fn discover(&self, home: &Path, limit: usize) -> Vec<SessionSummary>;
    fn parse_line(&self, line: &str, line_idx: usize) -> Option<ChatMessage>;
}

pub struct ClaudeProvider;
pub struct CodexProvider;
pub struct AntigravityProvider;

pub fn all_providers() -> Vec<Box<dyn AgentProvider>> {
    vec![
        Box::new(ClaudeProvider),
        Box::new(AntigravityProvider),
        Box::new(CodexProvider),
    ]
}

pub fn provider_for(kind: AgentKind) -> Box<dyn AgentProvider> {
    match kind {
        AgentKind::Claude => Box::new(ClaudeProvider),
        AgentKind::Antigravity => Box::new(AntigravityProvider),
        AgentKind::Codex => Box::new(CodexProvider),
    }
}

pub fn discover_all_sessions(home: &Path, limit: usize) -> Vec<SessionSummary> {
    let mut all = Vec::new();
    for provider in all_providers() {
        all.extend(provider.discover(home, limit));
    }
    all.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms));
    all.truncate(limit);
    all
}

pub fn read_messages_from_file(
    agent: AgentKind,
    path: &Path,
    start_offset: u64,
    tail: Option<usize>,
) -> Result<(Vec<ChatMessage>, u64)> {
    let provider = provider_for(agent);
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let effective_offset = if start_offset <= len { start_offset } else { 0 };
    file.seek(SeekFrom::Start(effective_offset))?;

    let mut reader = BufReader::new(file);
    let mut buf = String::new();
    let mut messages = Vec::new();
    let mut idx = 0usize;

    loop {
        buf.clear();
        let bytes = reader.read_line(&mut buf)?;
        if bytes == 0 {
            break;
        }
        if let Some(msg) = provider.parse_line(&buf, idx) {
            messages.push(msg);
        }
        idx += 1;
    }

    let new_offset = reader.stream_position()?;
    if let Some(max_tail) = tail
        && messages.len() > max_tail
    {
        let drop_count = messages.len() - max_tail;
        messages.drain(0..drop_count);
    }

    Ok((messages, new_offset))
}

impl AgentProvider for ClaudeProvider {
    fn discover(&self, home: &Path, limit: usize) -> Vec<SessionSummary> {
        let root = home.join(".claude").join("projects");
        scan_jsonl_dir(&root, AgentKind::Claude, limit, extract_claude_meta)
    }

    fn parse_line(&self, line: &str, line_idx: usize) -> Option<ChatMessage> {
        let val: Value = serde_json::from_str(line.trim()).ok()?;
        let kind = val.get("type").and_then(Value::as_str)?;
        let role = match kind {
            "user" => MessageRole::User,
            "assistant" => MessageRole::Assistant,
            _ => return None,
        };
        if val.get("isMeta").and_then(Value::as_bool) == Some(true) {
            return None;
        }
        let content = val
            .get("message")
            .and_then(|m| m.get("content"))
            .or_else(|| val.get("content"))?;
        let text = extract_text_content(content);
        if text.trim().is_empty() {
            return None;
        }
        let id = val
            .get("uuid")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| format!("claude-{line_idx}"));
        let timestamp_ms = val
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_iso_ms);
        Some(ChatMessage {
            id,
            role,
            text,
            timestamp_ms,
        })
    }
}

impl AgentProvider for CodexProvider {
    fn discover(&self, home: &Path, limit: usize) -> Vec<SessionSummary> {
        let root = home.join(".codex").join("sessions");
        scan_jsonl_dir(&root, AgentKind::Codex, limit, extract_generic_meta)
    }

    fn parse_line(&self, line: &str, line_idx: usize) -> Option<ChatMessage> {
        let val: Value = serde_json::from_str(line.trim()).ok()?;
        let timestamp_ms = val
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_iso_ms);

        // Codex emits either `{type:"response_item", payload:{type:"message", role, content}}`
        // or `{type:"event_msg", payload:{type:"user_message"|"agent_message", message}}`.
        let payload = val.get("payload").unwrap_or(&val);
        let ptype = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let (role, text) = match ptype {
            "user_message" => (
                MessageRole::User,
                payload
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            ),
            "agent_message" => (
                MessageRole::Assistant,
                payload
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            ),
            "message" => {
                let r = match payload.get("role").and_then(Value::as_str) {
                    Some("user") => MessageRole::User,
                    Some("assistant") => MessageRole::Assistant,
                    _ => return None,
                };
                let t = payload
                    .get("content")
                    .map(extract_text_content)
                    .unwrap_or_default();
                (r, t)
            }
            _ => return None,
        };

        if text.trim().is_empty() {
            return None;
        }
        Some(ChatMessage {
            id: format!("codex-{line_idx}"),
            role,
            text,
            timestamp_ms,
        })
    }
}

impl AgentProvider for AntigravityProvider {
    fn discover(&self, home: &Path, limit: usize) -> Vec<SessionSummary> {
        let roots = [
            home.join(".gemini").join("antigravity").join("brain"),
            home.join(".gemini").join("jetski").join("brain"),
            home.join(".antigravity").join("sessions"),
        ];
        let mut sessions = Vec::new();
        for root in roots {
            sessions.extend(scan_jsonl_dir(
                &root,
                AgentKind::Antigravity,
                limit,
                extract_generic_meta,
            ));
        }
        sessions.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms));
        sessions.truncate(limit);
        sessions
    }

    fn parse_line(&self, line: &str, line_idx: usize) -> Option<ChatMessage> {
        let val: Value = serde_json::from_str(line.trim()).ok()?;
        let role_str = val
            .get("role")
            .or_else(|| val.get("author"))
            .or_else(|| val.get("type"))
            .and_then(Value::as_str)?;
        let role = match role_str.to_ascii_lowercase().as_str() {
            "user" | "human" | "user_message" => MessageRole::User,
            "assistant" | "model" | "agent" | "assistant_message" => MessageRole::Assistant,
            _ => return None,
        };
        let text = val
            .get("text")
            .or_else(|| val.get("content"))
            .or_else(|| val.get("message"))
            .map(extract_text_content)
            .unwrap_or_default();
        if text.trim().is_empty() {
            return None;
        }
        let timestamp_ms = val
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_iso_ms)
            .or_else(|| val.get("timestamp_ms").and_then(Value::as_i64));
        Some(ChatMessage {
            id: format!("agy-{line_idx}"),
            role,
            text,
            timestamp_ms,
        })
    }
}

fn scan_jsonl_dir(
    root: &Path,
    agent: AgentKind,
    limit: usize,
    meta_fn: fn(&Path) -> (Option<String>, Option<String>),
) -> Vec<SessionSummary> {
    if !root.is_dir() {
        return Vec::new();
    }
    let mut files: Vec<(PathBuf, i64)> = Vec::new();
    for entry in WalkDir::new(root)
        .max_depth(5)
        .into_iter()
        .flatten()
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        if let Ok(meta) = entry.metadata()
            && let Ok(modified) = meta.modified()
        {
            files.push((path.to_path_buf(), system_time_ms(modified)));
        }
    }

    files.sort_by(|a, b| b.1.cmp(&a.1));
    files.truncate(limit);

    let now_ms = system_time_ms(SystemTime::now());
    files
        .into_iter()
        .map(|(path, updated_at_ms)| {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("session")
                .to_string();
            let (title_opt, cwd) = meta_fn(&path);
            let title = title_opt.unwrap_or_else(|| stem.clone());
            SessionSummary {
                id: stem,
                agent,
                title,
                cwd,
                source_path: path.to_string_lossy().to_string(),
                updated_at_ms,
                working: now_ms.saturating_sub(updated_at_ms) < 8_000,
            }
        })
        .collect()
}

fn extract_claude_meta(path: &Path) -> (Option<String>, Option<String>) {
    let Ok(file) = File::open(path) else {
        return (None, None);
    };
    let reader = BufReader::new(file);
    let mut title = None;
    let mut cwd = None;
    for line in reader.lines().take(24).flatten() {
        let Ok(val) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if cwd.is_none() {
            cwd = val.get("cwd").and_then(Value::as_str).map(String::from);
        }
        if let Some(custom) = val.get("customTitle").and_then(Value::as_str)
            && !custom.trim().is_empty()
        {
            title = Some(custom.trim().to_string());
            break;
        }
        if title.is_none()
            && val.get("type").and_then(Value::as_str) == Some("user")
            && let Some(content) = val
                .get("message")
                .and_then(|m| m.get("content"))
                .or_else(|| val.get("content"))
        {
            let text = extract_text_content(content);
            let first = text.lines().next().unwrap_or("").trim();
            if !first.is_empty() {
                title = Some(truncate_title(first, 64));
            }
        }
    }
    (title, cwd)
}

fn extract_generic_meta(path: &Path) -> (Option<String>, Option<String>) {
    let Ok(file) = File::open(path) else {
        return (None, None);
    };
    let reader = BufReader::new(file);
    let mut cwd = None;
    let mut title = None;
    for line in reader.lines().take(16).flatten() {
        let Ok(val) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if cwd.is_none() {
            cwd = val
                .get("cwd")
                .or_else(|| val.get("payload").and_then(|p| p.get("cwd")))
                .and_then(Value::as_str)
                .map(String::from);
        }
        if title.is_none()
            && let Some(t) = val
                .get("title")
                .or_else(|| val.get("payload").and_then(|p| p.get("message")))
                .and_then(Value::as_str)
        {
            let first = t.lines().next().unwrap_or("").trim();
            if !first.is_empty() {
                title = Some(truncate_title(first, 64));
            }
        }
    }
    (title, cwd)
}

fn extract_text_content(val: &Value) -> String {
    match val {
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                if let Some(s) = item.as_str() {
                    out.push(s.to_string());
                } else if item.get("type").and_then(Value::as_str) == Some("text")
                    || item.get("type").and_then(Value::as_str) == Some("output_text")
                    || item.get("type").and_then(Value::as_str) == Some("input_text")
                {
                    if let Some(t) = item.get("text").and_then(Value::as_str) {
                        out.push(t.to_string());
                    }
                }
            }
            out.join("\n")
        }
        Value::Object(map) => map
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

fn truncate_title(s: &str, max_chars: usize) -> String {
    let mut out: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        out.push('…');
    }
    out
}

fn parse_iso_ms(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

fn system_time_ms(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_claude_jsonl_messages() {
        let dir = std::env::temp_dir().join(format!("multibotd-claude-test-{}", std::process::id()));
        let proj = dir.join(".claude").join("projects").join("demo");
        std::fs::create_dir_all(&proj).unwrap();
        let path = proj.join("abc123.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"user","uuid":"u1","cwd":"/repo","timestamp":"2026-11-26T10:00:00Z","message":{"content":"hello claude"}}"#,
                "\n",
                r#"{"type":"assistant","uuid":"a1","timestamp":"2026-11-26T10:00:02Z","message":{"content":[{"type":"text","text":"hi from rust daemon"}]}}"#,
                "\n"
            ),
        )
        .unwrap();

        let sessions = discover_all_sessions(&dir, 10);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].agent, AgentKind::Claude);
        assert_eq!(sessions[0].title, "hello claude");
        assert_eq!(sessions[0].cwd.as_deref(), Some("/repo"));

        let (msgs, offset) = read_messages_from_file(AgentKind::Claude, &path, 0, None).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, MessageRole::User);
        assert_eq!(msgs[0].text, "hello claude");
        assert_eq!(msgs[1].role, MessageRole::Assistant);
        assert_eq!(msgs[1].text, "hi from rust daemon");
        assert!(offset > 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_codex_jsonl_messages() {
        let provider = CodexProvider;
        let line = r#"{"type":"event_msg","timestamp":"2026-11-26T10:01:00Z","payload":{"type":"agent_message","message":"codex ready"}}"#;
        let msg = provider.parse_line(line, 0).unwrap();
        assert_eq!(msg.role, MessageRole::Assistant);
        assert_eq!(msg.text, "codex ready");
    }
}
