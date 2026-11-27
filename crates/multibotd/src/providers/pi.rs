use super::{
    AgentProvider, HeaderMeta, LineSlice, ParsedSlice, cap_chars, is_safe_transcript_path,
    parse_normalized_messages, read_header_lines_bounded, strip_base64_in_str, system_time_ms,
    truncate_title,
};
use multibot_proto::AgentKind;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct PiProvider;

#[cfg(test)]
#[path = "pi_tests.rs"]
mod tests;

pub fn pi_discovery_roots(home: &Path) -> Vec<PathBuf> {
    let mut roots = vec![home.join(".pi/agent/sessions"), home.join(".multibot/pi")];
    if let Ok(dir) = std::env::var("PI_CODING_AGENT_SESSION_DIR") {
        let dir = dir.trim();
        if !dir.is_empty() {
            let path = dir
                .strip_prefix("~/")
                .map(|p| home.join(p))
                .unwrap_or_else(|| PathBuf::from(dir));
            // Relative overrides depend on the agent's cwd, not the daemon's cwd.
            if path.is_absolute() && !roots.contains(&path) {
                roots.push(path);
            }
        }
    }
    roots
}

fn normalize(mut value: Value) -> Option<Value> {
    if value.get("type")?.as_str()? != "message" {
        return None;
    }
    let mut message = value.get_mut("message")?.take();
    let role = message.get("role")?.as_str()?;
    let kind = match role {
        "user" => "user",
        "assistant" => "assistant",
        "toolResult" => "user",
        _ => return None,
    };
    let tool_result = role == "toolResult";
    let mut content = message
        .get_mut("content")
        .map(Value::take)
        .unwrap_or(Value::Null);
    if tool_result {
        content = json!([{
            "type":"tool_result", "tool_use_id":message.get("toolCallId"),
            "content":content, "is_error":message.get("isError").and_then(Value::as_bool).unwrap_or(false)
        }]);
    }
    if let Some(parts) = content.as_array_mut() {
        for part in parts {
            match part.get("type").and_then(Value::as_str) {
                Some("toolCall") => {
                    *part = json!({"type":"tool_use", "id":part.get("id"),
                        "name":part.get("name"), "input":part.get("arguments")});
                }
                Some("thinking") => {
                    *part = json!({"type":"thinking", "thinking":part.get("thinking")});
                }
                Some("image") => {
                    *part = json!({"type":"text", "text":"[image omitted]"});
                }
                _ => {}
            }
        }
    }
    Some(
        json!({"type":kind, "uuid":value.get("id"), "timestamp":value.get("timestamp"),
        "message":{"content":content, "model":message.get("model")}}),
    )
}

impl AgentProvider for PiProvider {
    fn kind(&self) -> AgentKind {
        AgentKind::Pi
    }

    fn discover_paths(
        &self,
        home: &Path,
        limit: usize,
        keep: &dyn Fn(&Path, i64) -> bool,
    ) -> Vec<(PathBuf, i64)> {
        let mut files = Vec::new();
        if limit == 0 {
            return files;
        }
        for root in pi_discovery_roots(home) {
            for entry in WalkDir::new(&root).max_depth(4).into_iter().flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                    continue;
                }
                let Ok(meta) = path.metadata() else {
                    continue;
                };
                let Ok(modified) = meta.modified() else {
                    continue;
                };
                let ts = system_time_ms(modified);
                if meta.is_file() && keep(path, ts) && is_safe_transcript_path(path, &root) {
                    files.push((path.to_path_buf(), ts));
                    files.sort_by(|a, b| b.1.cmp(&a.1));
                    files.truncate(limit);
                }
            }
        }
        files
    }

    fn extract_header_meta(&self, path: &Path) -> HeaderMeta {
        let bytes = read_header_lines_bounded(path, 32).join("\n") + "\n";
        let parsed = self.parse_slice(bytes.as_bytes(), 0);
        HeaderMeta {
            session_id_hint: parsed.session_id_hint,
            cwd: parsed.cwd,
            title_hint: parsed.title_hint.or_else(|| {
                parsed
                    .messages
                    .iter()
                    .find(|m| m.role == multibot_proto::MessageRole::User)
                    .map(|m| truncate_title(&m.text, 64))
            }),
            model: parsed.model,
        }
    }

    fn parse_lines(&self, lines: &[LineSlice<'_>]) -> ParsedSlice {
        let mut meta = HeaderMeta::default();
        let mut parsed = parse_normalized_messages(lines, "pi", |value| {
            match value.get("type").and_then(Value::as_str) {
                Some("session") => {
                    meta.session_id_hint =
                        value.get("id").and_then(Value::as_str).map(str::to_string);
                    meta.cwd = value.get("cwd").and_then(Value::as_str).map(str::to_string);
                }
                Some("model_change") => {
                    meta.model = value
                        .get("modelId")
                        .and_then(Value::as_str)
                        .map(|s| cap_chars(s, 128));
                }
                Some("session_info") => {
                    meta.title_hint = value
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|s| !s.trim().is_empty())
                        .map(|s| truncate_title(&strip_base64_in_str(s), 64));
                }
                _ => {}
            }
            normalize(value)
        });
        parsed.session_id_hint = meta.session_id_hint;
        parsed.cwd = meta.cwd;
        parsed.title_hint = meta.title_hint;
        parsed.model = meta.model.or(parsed.model);
        parsed
    }
}
