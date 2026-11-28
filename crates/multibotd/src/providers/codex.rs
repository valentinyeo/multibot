use super::{
    AgentProvider, HeaderMeta, LineSlice, MAX_TODOS, MAX_TOOL_DETAIL_CHARS, ParsedSlice,
    TailActivity, append_diff_counts_to_summary, build_diff_block, cap_chars, combine_tool_detail,
    extract_image_blocks, extract_text_content, format_tool_summary, is_safe_transcript_path,
    parse_iso_ms, parse_prompt_questions, parse_todo_status, populate_last_assistant_fields,
    read_header_lines_bounded, sanitize_todo_content, strip_base64_cow, strip_base64_in_str,
    strip_system_reminders, strip_tracking_suffix, system_time_ms, tool_text, truncate_title,
};
use multibot_proto::{
    AgentKind, ChatMessage, DiffBlock, MessageBlock, MessageRole, PromptQuestion, TodoItem,
    TodoStatus, ToolStatus,
};
use regex::Regex;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use walkdir::WalkDir;

pub struct CodexProvider;

pub fn codex_discovery_roots(home: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(codex_home) = std::env::var("CODEX_HOME") {
        let trimmed = codex_home.trim();
        if !trimmed.is_empty() {
            let expanded = if let Some(rest) = trimmed.strip_prefix("~/") {
                home.join(rest)
            } else {
                PathBuf::from(trimmed)
            };
            candidates.push(expanded.join("sessions"));
        }
    }
    candidates.push(home.join(".codex").join("sessions"));

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for root in candidates {
        if !root.is_dir() {
            continue;
        }
        let key = fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
        if seen.insert(key) {
            out.push(root);
        }
    }
    out
}

fn extract_uuid_from_rollout_stem(stem: &str) -> Option<String> {
    static UUID_TAIL_RE: OnceLock<Regex> = OnceLock::new();
    let re = UUID_TAIL_RE.get_or_init(|| {
        Regex::new(
            r"([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12})$",
        )
        .expect("valid uuid regex")
    });
    re.captures(stem)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

fn extract_codex_apply_patch_diff(args: &Value) -> Option<DiffBlock> {
    static PATCH_HDR_RE: OnceLock<Regex> = OnceLock::new();
    let re = PATCH_HDR_RE.get_or_init(|| {
        Regex::new(r"\*\*\* (Update|Add|Delete) File: ([^\r\n]+)")
            .expect("valid patch header regex")
    });
    let patch_str = args
        .get("input")
        .and_then(Value::as_str)
        .or_else(|| args.as_str())?;
    let path = re
        .captures(patch_str)
        .and_then(|c| c.get(2))
        .map(|m| m.as_str().trim().to_string());

    let mut lines = Vec::new();
    for l in patch_str.lines() {
        if l.starts_with("*** ") || l.starts_with("@@") {
            continue;
        }
        if l.starts_with('+') || l.starts_with('-') || l.starts_with(' ') {
            lines.push(l.to_string());
        }
    }
    build_diff_block(path, lines)
}

fn check_structured_failure(val: &Value, depth: usize) -> Option<bool> {
    if depth > 4 {
        return None;
    }
    match val {
        Value::String(s) => {
            let trimmed = s.trim();
            if (trimmed.starts_with('{') && trimmed.ends_with('}'))
                || (trimmed.starts_with('[') && trimmed.ends_with(']'))
            {
                if let Ok(parsed) = serde_json::from_str::<Value>(trimmed) {
                    return check_structured_failure(&parsed, depth + 1);
                }
            }
            None
        }
        Value::Array(arr) => {
            for item in arr {
                if let Some(res) = check_structured_failure(item, depth + 1) {
                    return Some(res);
                }
            }
            None
        }
        Value::Object(map) => {
            if map.get("is_error").and_then(Value::as_bool) == Some(true) {
                return Some(true);
            }
            if map.get("success").and_then(Value::as_bool) == Some(false) {
                return Some(true);
            }
            if let Some(err) = map.get("error") {
                let truthy = match err {
                    Value::Null => false,
                    Value::Bool(b) => *b,
                    Value::String(s) => !s.trim().is_empty(),
                    Value::Array(a) => !a.is_empty(),
                    Value::Object(o) => !o.is_empty(),
                    Value::Number(n) => n.as_i64().unwrap_or(1) != 0,
                };
                if truthy {
                    return Some(true);
                }
            }
            if let Some(status) = map.get("status").and_then(Value::as_str) {
                match status.to_ascii_lowercase().as_str() {
                    "error" | "failed" | "failure" => return Some(true),
                    "ok" | "success" | "succeeded" | "completed" => return Some(false),
                    _ => {}
                }
            }
            if let Some(ec) = map.get("exit_code") {
                if let Some(n) = ec.as_i64() {
                    return Some(n != 0);
                }
                if let Some(s) = ec.as_str()
                    && let Ok(n) = s.trim().parse::<i64>()
                {
                    return Some(n != 0);
                }
            }
            for v in map.values() {
                if let Some(res) = check_structured_failure(v, depth + 1) {
                    return Some(res);
                }
            }
            None
        }
        _ => None,
    }
}

pub fn codex_tool_failed(payload: &Value, output_text: &str) -> bool {
    if let Some(res) = check_structured_failure(payload, 0) {
        return res;
    }
    static ERR_PREFIX_RE: OnceLock<Regex> = OnceLock::new();
    static EXIT_CODE_RE: OnceLock<Regex> = OnceLock::new();

    let err_re = ERR_PREFIX_RE.get_or_init(|| {
        Regex::new(r"(?i)(?:^|\n)(?:Error:|Failed to |apply_patch verification failed:)")
            .expect("valid codex err regex")
    });
    if err_re.is_match(output_text) {
        return true;
    }
    let exit_re = EXIT_CODE_RE.get_or_init(|| {
        Regex::new(r"(?i)(?:Exit code:\s*|Process exited with code\s+)(-?\d+)")
            .expect("valid codex exit regex")
    });
    for cap in exit_re.captures_iter(output_text) {
        if let Some(code_str) = cap.get(1)
            && let Ok(code) = code_str.as_str().parse::<i64>()
            && code != 0
        {
            return true;
        }
    }
    false
}

fn is_omitted_or_data_url(u: &str) -> bool {
    let trimmed = u.trim();
    if trimmed.eq_ignore_ascii_case("[image omitted]")
        || (trimmed.len() >= 5 && trimmed.as_bytes()[..5].eq_ignore_ascii_case(b"data:"))
    {
        return true;
    }
    let is_http = (trimmed.len() >= 7 && trimmed.as_bytes()[..7].eq_ignore_ascii_case(b"http://"))
        || (trimmed.len() >= 8 && trimmed.as_bytes()[..8].eq_ignore_ascii_case(b"https://"));
    !is_http && strip_base64_cow(trimmed).trim() == "[image omitted]"
}

fn extract_codex_content_images(content: &Value) -> (Vec<MessageBlock>, bool) {
    let Some(parts) = content.as_array() else {
        return (Vec::new(), false);
    };
    let mut out = Vec::new();
    let mut had_omitted_base64 = false;
    for part in parts {
        let ptype = part.get("type").and_then(Value::as_str).unwrap_or("");
        if matches!(ptype, "input_image" | "image_url" | "image") {
            let source = part.get("source").unwrap_or(part);
            if source.get("type").and_then(Value::as_str) == Some("base64") {
                had_omitted_base64 = true;
                continue;
            }
            let raw_url = part
                .get("image_url")
                .and_then(|v| v.as_str().or_else(|| v.get("url").and_then(Value::as_str)))
                .or_else(|| source.get("url").and_then(Value::as_str))
                .or_else(|| part.get("url").and_then(Value::as_str))
                .map(str::trim)
                .filter(|u| !u.is_empty());
            if let Some(u) = raw_url {
                if is_omitted_or_data_url(u) {
                    had_omitted_base64 = true;
                } else if (u.len() >= 7 && u.as_bytes()[..7].eq_ignore_ascii_case(b"http://"))
                    || (u.len() >= 8 && u.as_bytes()[..8].eq_ignore_ascii_case(b"https://"))
                {
                    out.push(MessageBlock::Image {
                        url: Some(u.to_string()),
                        local_path: None,
                        alt: String::new(),
                    });
                } else {
                    out.push(MessageBlock::Image {
                        url: None,
                        local_path: Some(u.to_string()),
                        alt: String::new(),
                    });
                }
            } else if source.get("data").is_some() || part.get("data").is_some() {
                had_omitted_base64 = true;
            }
        }
    }
    (out, had_omitted_base64)
}

fn is_duplicate_adjacent(
    messages: &[ChatMessage],
    role: MessageRole,
    text: &str,
    timestamp_ms: Option<i64>,
) -> bool {
    let Some(last) = messages.last() else {
        return false;
    };
    if last.role != role {
        return false;
    }
    if last.text.trim() != text.trim() {
        return false;
    }
    if last
        .blocks
        .iter()
        .any(|b| matches!(b, MessageBlock::ToolUse { .. }))
    {
        return false;
    }
    match (last.timestamp_ms, timestamp_ms) {
        (Some(a), Some(b)) => (a - b).abs() <= 2_000,
        _ => true,
    }
}

struct OpenCodexTool {
    msg_idx: usize,
    block_idx: usize,
    raw_input_detail: String,
}

struct PendingCodexQuestion {
    call_id: String,
    msg_idx: usize,
    block_idx: usize,
    questions: Vec<PromptQuestion>,
}

impl AgentProvider for CodexProvider {
    fn kind(&self) -> AgentKind {
        AgentKind::Codex
    }

    fn discover_paths(
        &self,
        home: &Path,
        limit: usize,
        keep: &dyn Fn(&Path, i64) -> bool,
    ) -> Vec<(PathBuf, i64)> {
        let mut rollout_files = Vec::new();
        let mut other_files = Vec::new();
        for root in codex_discovery_roots(home) {
            for entry in WalkDir::new(&root).max_depth(5).into_iter().flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                if let Ok(meta) = fs::metadata(path)
                    && let Ok(modified) = meta.modified()
                {
                    let ts = system_time_ms(modified);
                    // Cheapest check first: a symlink escaping the root is rare, a stale
                    // transcript is the common case on a long-lived home directory.
                    if !keep(path, ts) || !is_safe_transcript_path(path, &root) {
                        continue;
                    }
                    let is_rollout = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("rollout-"));
                    if is_rollout {
                        rollout_files.push((path.to_path_buf(), ts));
                    } else {
                        other_files.push((path.to_path_buf(), ts));
                    }
                }
            }
        }
        let mut files = if rollout_files.is_empty() {
            other_files
        } else {
            rollout_files.extend(other_files);
            rollout_files
        };
        files.sort_by(|a, b| b.1.cmp(&a.1));
        files.truncate(limit);
        files
    }

    fn extract_header_meta(&self, path: &Path) -> HeaderMeta {
        let mut meta = HeaderMeta::default();
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            meta.session_id_hint = extract_uuid_from_rollout_stem(stem);
        }
        for line in read_header_lines_bounded(path, 24) {
            let Ok(val) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let top_type = val.get("type").and_then(Value::as_str).unwrap_or("");
            let payload = val.get("payload").unwrap_or(&val);
            match top_type {
                "session_meta" => {
                    if let Some(id) = payload
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|s| !s.trim().is_empty())
                    {
                        meta.session_id_hint = Some(id.to_string());
                    }
                    if meta.cwd.is_none() {
                        meta.cwd = payload
                            .get("cwd")
                            .and_then(Value::as_str)
                            .filter(|s| !s.trim().is_empty())
                            .map(String::from);
                    }
                    if meta.model.is_none() {
                        meta.model = payload
                            .get("model")
                            .and_then(Value::as_str)
                            .filter(|s| !s.trim().is_empty())
                            .map(|s| cap_chars(s.trim(), 128));
                    }
                }
                "turn_context" => {
                    meta.effort = super::reported_effort(
                        payload
                            .get("effort")
                            .or_else(|| payload.get("reasoning_effort"))
                            .and_then(Value::as_str),
                    )
                    .or(meta.effort);
                    if meta.cwd.is_none() {
                        meta.cwd = payload
                            .get("cwd")
                            .and_then(Value::as_str)
                            .filter(|s| !s.trim().is_empty())
                            .map(String::from);
                    }
                    if meta.model.is_none() {
                        meta.model = payload
                            .get("model")
                            .and_then(Value::as_str)
                            .filter(|s| !s.trim().is_empty())
                            .map(|s| cap_chars(s.trim(), 128));
                    }
                }
                "event_msg" => {
                    if meta.title_hint.is_none()
                        && payload.get("type").and_then(Value::as_str) == Some("user_message")
                        && let Some(m) = payload.get("message").and_then(Value::as_str)
                    {
                        let cleaned = strip_system_reminders(m);
                        let (cleaned, _) = strip_tracking_suffix(&cleaned);
                        let cleaned = strip_base64_in_str(&cleaned);
                        if let Some(first) = cleaned.lines().map(str::trim).find(|l| !l.is_empty())
                        {
                            meta.title_hint = Some(truncate_title(first, 64));
                        }
                    }
                }
                "response_item" => {
                    if meta.title_hint.is_none()
                        && payload.get("type").and_then(Value::as_str) == Some("message")
                        && payload.get("role").and_then(Value::as_str) == Some("user")
                        && let Some(c) = payload.get("content")
                    {
                        let raw = extract_text_content(c);
                        let cleaned = strip_system_reminders(&raw);
                        let (cleaned, _) = strip_tracking_suffix(&cleaned);
                        let cleaned = strip_base64_in_str(&cleaned);
                        if let Some(first) = cleaned.lines().map(str::trim).find(|l| !l.is_empty())
                        {
                            meta.title_hint = Some(truncate_title(first, 64));
                        }
                    }
                }
                _ => {}
            }
        }
        meta
    }

    fn parse_lines(&self, lines: &[LineSlice<'_>]) -> ParsedSlice {
        let mut parsed = ParsedSlice::default();
        let mut open_tools: HashMap<String, OpenCodexTool> = HashMap::new();
        let mut completed_tool_ids: HashSet<String> = HashSet::new();
        let mut pending_questions: Vec<PendingCodexQuestion> = Vec::new();

        for line in lines {
            let Ok(line_str) = std::str::from_utf8(line.bytes) else {
                continue;
            };
            let trimmed = line_str.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            let timestamp_ms = val
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_iso_ms);
            let top_type = val.get("type").and_then(Value::as_str).unwrap_or("");
            let payload = val.get("payload").unwrap_or(&val);

            match top_type {
                "session_meta" => {
                    if let Some(id) = payload
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|s| !s.trim().is_empty())
                    {
                        parsed.session_id_hint = Some(id.to_string());
                    }
                    if let Some(cwd) = payload
                        .get("cwd")
                        .and_then(Value::as_str)
                        .filter(|s| !s.trim().is_empty())
                    {
                        parsed.cwd = Some(cwd.to_string());
                    }
                    if let Some(model) = payload
                        .get("model")
                        .and_then(Value::as_str)
                        .filter(|s| !s.trim().is_empty())
                    {
                        parsed.model = Some(cap_chars(model.trim(), 128));
                    }
                }
                "turn_context" => {
                    parsed.effort = super::reported_effort(
                        payload
                            .get("effort")
                            .or_else(|| payload.get("reasoning_effort"))
                            .and_then(Value::as_str),
                    )
                    .or(parsed.effort);
                    if let Some(model) = payload
                        .get("model")
                        .and_then(Value::as_str)
                        .filter(|s| !s.trim().is_empty())
                    {
                        parsed.model = Some(cap_chars(model.trim(), 128));
                    }
                    if let Some(cwd) = payload
                        .get("cwd")
                        .and_then(Value::as_str)
                        .filter(|s| !s.trim().is_empty())
                    {
                        parsed.cwd = Some(cwd.to_string());
                    }
                }
                "event_msg" => {
                    let ptype = payload.get("type").and_then(Value::as_str).unwrap_or("");
                    match ptype {
                        "token_count" => {
                            if let Some(info) = payload.get("info").filter(|v| !v.is_null()) {
                                parsed.context_tokens = info
                                    .get("last_token_usage")
                                    .and_then(|usage| usage.get("total_tokens"))
                                    .and_then(Value::as_u64)
                                    .or(parsed.context_tokens);
                                parsed.context_limit = info
                                    .get("model_context_window")
                                    .and_then(Value::as_u64)
                                    .filter(|n| *n > 0)
                                    .or(parsed.context_limit);
                            }
                        }
                        "user_message" => {
                            let raw = payload.get("message").and_then(Value::as_str).unwrap_or("");
                            let cleaned = strip_system_reminders(raw);
                            let (cleaned, quoted_id) = strip_tracking_suffix(&cleaned);
                            let text = strip_base64_in_str(&cleaned).trim().to_string();
                            if !text.is_empty()
                                && !is_duplicate_adjacent(
                                    &parsed.messages,
                                    MessageRole::User,
                                    &text,
                                    timestamp_ms,
                                )
                            {
                                let mut blocks = vec![MessageBlock::Text { text: text.clone() }];
                                blocks.extend(extract_image_blocks(&text));
                                parsed.messages.push(ChatMessage {
                                    id: format!("codex-{}", line.start_offset),
                                    role: MessageRole::User,
                                    text,
                                    timestamp_ms,
                                    blocks,
                                    source_byte_range: Some((line.start_offset, line.end_offset)),
                                    quoted_message_id: quoted_id,
                                    ..Default::default()
                                });
                                parsed.tail_activity = TailActivity::Working;
                            }
                        }
                        "agent_message" => {
                            let raw = payload.get("message").and_then(Value::as_str).unwrap_or("");
                            let text = strip_base64_in_str(&strip_system_reminders(raw))
                                .trim()
                                .to_string();
                            if !text.is_empty()
                                && !is_duplicate_adjacent(
                                    &parsed.messages,
                                    MessageRole::Assistant,
                                    &text,
                                    timestamp_ms,
                                )
                            {
                                let mut blocks = vec![MessageBlock::Text { text: text.clone() }];
                                blocks.extend(extract_image_blocks(&text));
                                parsed.messages.push(ChatMessage {
                                    id: format!("codex-{}", line.start_offset),
                                    role: MessageRole::Assistant,
                                    text,
                                    timestamp_ms,
                                    blocks,
                                    source_byte_range: Some((line.start_offset, line.end_offset)),
                                    ..Default::default()
                                });
                                parsed.tail_activity = TailActivity::Idle;
                            }
                        }
                        "turn_aborted" | "interrupted" => {
                            parsed.tail_activity = TailActivity::Idle;
                        }
                        _ => {}
                    }
                }
                "response_item" => {
                    let ptype = payload.get("type").and_then(Value::as_str).unwrap_or("");
                    match ptype {
                        "message" => {
                            let role = match payload.get("role").and_then(Value::as_str) {
                                Some("user") => MessageRole::User,
                                Some("assistant") => MessageRole::Assistant,
                                _ => continue,
                            };
                            let content_val = payload.get("content").unwrap_or(&Value::Null);
                            let raw_text = extract_text_content(content_val);
                            let cleaned = strip_system_reminders(&raw_text);
                            let (cleaned, quoted_id) = if role == MessageRole::User {
                                strip_tracking_suffix(&cleaned)
                            } else {
                                (cleaned, None)
                            };
                            let mut text = strip_base64_in_str(&cleaned).trim().to_string();
                            let (mut images, had_omitted_base64) =
                                extract_codex_content_images(content_val);
                            if had_omitted_base64 {
                                if text.is_empty() {
                                    text = "[image omitted]".to_string();
                                } else if !text.contains("[image omitted]") {
                                    text.push_str("\n[image omitted]");
                                }
                            }
                            images.extend(extract_image_blocks(&text));

                            if text.is_empty() && images.is_empty() {
                                continue;
                            }
                            if !text.is_empty()
                                && is_duplicate_adjacent(
                                    &parsed.messages,
                                    role,
                                    &text,
                                    timestamp_ms,
                                )
                            {
                                continue;
                            }
                            let mut blocks = Vec::new();
                            if !text.is_empty() {
                                blocks.push(MessageBlock::Text { text: text.clone() });
                            }
                            blocks.extend(images);
                            parsed.messages.push(ChatMessage {
                                id: format!("codex-{}", line.start_offset),
                                role,
                                text,
                                timestamp_ms,
                                blocks,
                                source_byte_range: Some((line.start_offset, line.end_offset)),
                                quoted_message_id: quoted_id,
                                ..Default::default()
                            });
                            parsed.tail_activity = match role {
                                MessageRole::User => TailActivity::Working,
                                _ => TailActivity::Idle,
                            };
                        }
                        "function_call" | "custom_tool_call" | "local_shell_call" => {
                            let Some(call_id) = payload
                                .get("call_id")
                                .or_else(|| payload.get("id"))
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                            else {
                                continue;
                            };
                            let name = payload.get("name").and_then(Value::as_str).unwrap_or(
                                if ptype == "local_shell_call" {
                                    "local_shell"
                                } else {
                                    "Tool"
                                },
                            );
                            let raw_args = payload
                                .get("arguments")
                                .or_else(|| payload.get("input"))
                                .or_else(|| payload.get("action"))
                                .unwrap_or(&Value::Null);
                            let args: Value = match raw_args {
                                Value::String(s) => serde_json::from_str::<Value>(s)
                                    .unwrap_or_else(|_| Value::String(s.clone())),
                                other => other.clone(),
                            };

                            let mut msg_todos = Vec::new();
                            if name == "update_plan"
                                && let Some(plan) = args.get("plan").and_then(Value::as_array)
                            {
                                let todos: Vec<TodoItem> = plan
                                    .iter()
                                    .take(MAX_TODOS)
                                    .filter_map(|item| {
                                        let step = item
                                            .get("step")
                                            .or_else(|| item.get("content"))
                                            .and_then(Value::as_str)
                                            .map(str::trim)
                                            .filter(|s| !s.is_empty())?;
                                        let status = item
                                            .get("status")
                                            .and_then(Value::as_str)
                                            .map(parse_todo_status)
                                            .unwrap_or(TodoStatus::Pending);
                                        Some(TodoItem {
                                            content: sanitize_todo_content(step),
                                            status,
                                        })
                                    })
                                    .collect();
                                parsed.todos = todos.clone();
                                msg_todos = todos;
                            }

                            let mut summary = format_tool_summary(name, &args);
                            let raw_detail = tool_text(&args);
                            let detail = cap_chars(&raw_detail, MAX_TOOL_DETAIL_CHARS);
                            let diff = if name.eq_ignore_ascii_case("apply_patch") {
                                extract_codex_apply_patch_diff(&args)
                            } else {
                                None
                            };
                            if let Some(ref d) = diff {
                                summary = append_diff_counts_to_summary(&summary, d);
                            }

                            let tool_block = MessageBlock::ToolUse {
                                call_id: call_id.to_string(),
                                name: name.to_string(),
                                summary,
                                detail,
                                status: ToolStatus::Running,
                                diff,
                                output: None,
                            };

                            // Attach to immediately preceding assistant message if present, else create one
                            let (msg_idx, block_idx) = if parsed
                                .messages
                                .last()
                                .is_some_and(|m| m.role == MessageRole::Assistant)
                            {
                                let idx = parsed.messages.len() - 1;
                                let msg = &mut parsed.messages[idx];
                                let b_idx = msg.blocks.len();
                                msg.blocks.push(tool_block);
                                if !msg_todos.is_empty() {
                                    msg.todos = msg_todos;
                                }
                                (idx, b_idx)
                            } else {
                                let idx = parsed.messages.len();
                                parsed.messages.push(ChatMessage {
                                    id: format!("codex-{}", line.start_offset),
                                    role: MessageRole::Assistant,
                                    text: String::new(),
                                    timestamp_ms,
                                    blocks: vec![tool_block],
                                    todos: msg_todos,
                                    source_byte_range: Some((line.start_offset, line.end_offset)),
                                    ..Default::default()
                                });
                                (idx, 0)
                            };

                            if name == "request_user_input" {
                                let qs = args
                                    .get("questions")
                                    .map(parse_prompt_questions)
                                    .unwrap_or_default();
                                if !qs.is_empty() {
                                    pending_questions.push(PendingCodexQuestion {
                                        call_id: call_id.to_string(),
                                        msg_idx,
                                        block_idx,
                                        questions: qs,
                                    });
                                }
                            }

                            open_tools.insert(
                                call_id.to_string(),
                                OpenCodexTool {
                                    msg_idx,
                                    block_idx,
                                    raw_input_detail: raw_detail,
                                },
                            );
                            parsed.tail_activity = TailActivity::Working;
                        }
                        "function_call_output"
                        | "custom_tool_call_output"
                        | "local_shell_call_output" => {
                            let Some(call_id) = payload
                                .get("call_id")
                                .or_else(|| payload.get("id"))
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                            else {
                                continue;
                            };
                            completed_tool_ids.insert(call_id.to_string());
                            let output_val = payload.get("output").unwrap_or(&Value::Null);
                            let output_text =
                                cap_chars(&tool_text(output_val), MAX_TOOL_DETAIL_CHARS);
                            let failed = codex_tool_failed(payload, &output_text);
                            let status = if failed {
                                ToolStatus::Error
                            } else {
                                ToolStatus::Completed
                            };

                            if let Some(tref) = open_tools.remove(call_id) {
                                if let Some(msg) = parsed.messages.get_mut(tref.msg_idx)
                                    && let Some(MessageBlock::ToolUse {
                                        detail,
                                        status: blk_status,
                                        output,
                                        ..
                                    }) = msg.blocks.get_mut(tref.block_idx)
                                {
                                    *blk_status = status;
                                    *output = if output_text.is_empty() {
                                        None
                                    } else {
                                        Some(output_text.clone())
                                    };
                                    *detail =
                                        combine_tool_detail(&tref.raw_input_detail, &output_text);
                                }
                            } else {
                                parsed.messages.push(ChatMessage {
                                    id: format!("tool-result-{call_id}"),
                                    role: MessageRole::Tool,
                                    text: String::new(),
                                    timestamp_ms,
                                    blocks: vec![MessageBlock::ToolUse {
                                        call_id: call_id.to_string(),
                                        name: String::new(),
                                        summary: String::new(),
                                        detail: output_text.clone(),
                                        status,
                                        diff: None,
                                        output: if output_text.is_empty() {
                                            None
                                        } else {
                                            Some(output_text)
                                        },
                                    }],
                                    source_byte_range: Some((line.start_offset, line.end_offset)),
                                    ..Default::default()
                                });
                            }
                            parsed.tail_activity = TailActivity::Working;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        if parsed.tail_activity != TailActivity::Idle
            && let Some(pq) = pending_questions
                .into_iter()
                .rev()
                .find(|pq| !completed_tool_ids.contains(&pq.call_id))
            && pq.msg_idx + 1 == parsed.messages.len()
        {
            if let Some(msg) = parsed.messages.get_mut(pq.msg_idx) {
                let prompt_block = MessageBlock::QuestionPrompt {
                    prompt_id: pq.call_id.clone(),
                    provider: "codex".to_string(),
                    questions: pq.questions,
                };
                if pq.block_idx < msg.blocks.len() {
                    msg.blocks[pq.block_idx] = prompt_block;
                } else {
                    msg.blocks.push(prompt_block);
                }
            }
            open_tools.remove(&pq.call_id);
            parsed.has_pending_question = true;
            parsed.tail_activity = TailActivity::AwaitingUserInput;
        } else if !open_tools.is_empty() && parsed.tail_activity != TailActivity::Idle {
            parsed.tail_activity = TailActivity::Working;
        }

        populate_last_assistant_fields(&mut parsed);
        parsed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::discover_all_sessions;
    use serde_json::json;

    #[test]
    fn parses_codex_rollout_dedup_tools_patch_plan_and_prompt() {
        let patch = "*** Begin Patch\n*** Update File: src/app.rs\n@@ -1,2 +1,2 @@\n fn main() {\n-    println!(\"old\");\n+    println!(\"new\");\n }\n*** End Patch";
        let lines = vec![
            json!({"type":"session_meta","timestamp":"2026-11-26T10:00:00Z","payload":{"id":"codex-sess-1","cwd":"/repo/codex"}}),
            json!({"type":"turn_context","timestamp":"2026-11-26T10:00:00Z","payload":{"model":"gpt-5.3-codex","cwd":"/repo/codex"}}),
            // Duplicate user utterance emitted via event_msg + response_item
            json!({"type":"event_msg","timestamp":"2026-11-26T10:00:01Z","payload":{"type":"user_message","message":"Update app"}}),
            json!({"type":"response_item","timestamp":"2026-11-26T10:00:01Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Update app"}]}}),
            // Developer message should be ignored
            json!({"type":"response_item","timestamp":"2026-11-26T10:00:01Z","payload":{"type":"message","role":"developer","content":[{"type":"text","text":"internal system"}]}}),
            json!({"type":"response_item","timestamp":"2026-11-26T10:00:02Z","payload":{"type":"custom_tool_call","call_id":"p1","name":"apply_patch","input":patch}}),
            json!({"type":"response_item","timestamp":"2026-11-26T10:00:03Z","payload":{"type":"custom_tool_call_output","call_id":"p1","output":"Success"}}),
            json!({"type":"response_item","timestamp":"2026-11-26T10:00:04Z","payload":{"type":"local_shell_call","call_id":"sh1","action":{"command":["cargo","test"]}}}),
            json!({"type":"response_item","timestamp":"2026-11-26T10:00:05Z","payload":{"type":"local_shell_call_output","call_id":"sh1","output":"Exit code: 1\nError: test failed"}}),
            json!({"type":"response_item","timestamp":"2026-11-26T10:00:06Z","payload":{"type":"function_call","call_id":"plan1","name":"update_plan","arguments":json!({"plan":[{"step":"Patch app","status":"completed"},{"step":"Verify","status":"in_progress"}]}).to_string()}}),
            json!({"type":"response_item","timestamp":"2026-11-26T10:00:07Z","payload":{"type":"function_call_output","call_id":"plan1","output":"ok"}}),
            json!({"type":"response_item","timestamp":"2026-11-26T10:00:08Z","payload":{"type":"function_call","call_id":"ask1","name":"request_user_input","arguments":json!({"questions":[{"question":"Ship now?","header":"Ship","options":[{"label":"Yes","description":"Deploy"}]}]}).to_string()}}),
        ];
        let payload: String = lines.into_iter().map(|v| format!("{v}\n")).collect();
        let parsed = CodexProvider.parse_slice(payload.as_bytes(), 0);

        assert_eq!(parsed.session_id_hint.as_deref(), Some("codex-sess-1"));
        assert_eq!(parsed.cwd.as_deref(), Some("/repo/codex"));
        assert_eq!(parsed.model.as_deref(), Some("gpt-5.3-codex"));
        // Duplicate user message suppressed, developer message ignored -> 1 user + 1 assistant
        assert_eq!(parsed.messages.len(), 2);
        assert_eq!(parsed.messages[0].role, MessageRole::User);
        assert_eq!(parsed.messages[0].text, "Update app");

        let assistant = &parsed.messages[1];
        assert_eq!(assistant.role, MessageRole::Assistant);

        // apply_patch diff + Completed
        let patch_blk = assistant
            .blocks
            .iter()
            .find_map(|b| match b {
                MessageBlock::ToolUse {
                    call_id,
                    summary,
                    status,
                    diff,
                    ..
                } if call_id == "p1" => Some((summary, status, diff)),
                _ => None,
            })
            .unwrap();
        assert_eq!(patch_blk.0, "Edited app.rs +1 -1");
        assert_eq!(*patch_blk.1, ToolStatus::Completed);
        assert_eq!(
            patch_blk.2.as_ref().unwrap().path.as_deref(),
            Some("src/app.rs")
        );

        // local_shell_call failed
        let shell_status = assistant
            .blocks
            .iter()
            .find_map(|b| match b {
                MessageBlock::ToolUse {
                    call_id, status, ..
                } if call_id == "sh1" => Some(*status),
                _ => None,
            })
            .unwrap();
        assert_eq!(shell_status, ToolStatus::Error);

        // update_plan todos
        assert_eq!(parsed.todos.len(), 2);
        assert_eq!(parsed.todos[0].content, "Patch app");
        assert_eq!(parsed.todos[0].status, TodoStatus::Completed);

        // request_user_input -> QuestionPrompt
        assert!(parsed.has_pending_question);
        assert_eq!(parsed.tail_activity, TailActivity::AwaitingUserInput);
        assert!(assistant.blocks.iter().any(|b| matches!(
            b,
            MessageBlock::QuestionPrompt { prompt_id, provider, .. }
                if prompt_id == "ask1" && provider == "codex"
        )));
    }

    #[test]
    fn codex_discovers_nested_rollout_and_session_meta() {
        let dir = std::env::temp_dir().join(format!("multibotd-codex-disc-{}", std::process::id()));
        let nested = dir.join(".codex/sessions/2026/11/26");
        fs::create_dir_all(&nested).unwrap();
        let path = nested.join("rollout-2026-11-26-11111111-2222-3333-4444-555555555555.jsonl");
        fs::write(
            &path,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"11111111-2222-3333-4444-555555555555","cwd":"/work/api"}}"#,
                "\n",
                r#"{"type":"event_msg","timestamp":"2026-11-26T10:01:00Z","payload":{"type":"agent_message","message":"codex ready"}}"#,
                "\n"
            ),
        )
        .unwrap();

        let sessions = discover_all_sessions(&dir, 10);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].agent, AgentKind::Codex);
        assert_eq!(sessions[0].id, "11111111-2222-3333-4444-555555555555");
        assert_eq!(sessions[0].cwd.as_deref(), Some("/work/api"));
        assert_eq!(
            sessions[0].last_assistant_preview.as_deref(),
            Some("codex ready")
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_scrubs_inline_60kb_base64_in_user_and_assistant_text() {
        let blob = "B".repeat(60_000);
        let lines = vec![
            json!({"type":"event_msg","timestamp":"2026-11-26T10:00:01Z","payload":{"type":"user_message","message":format!("User img data:image/png;base64,{blob} end")}}),
            json!({"type":"event_msg","timestamp":"2026-11-26T10:00:02Z","payload":{"type":"agent_message","message":format!("Agent img data:image/jpeg;base64,{blob} end")}}),
        ];
        let payload: String = lines.into_iter().map(|v| format!("{v}\n")).collect();
        let parsed = CodexProvider.parse_slice(payload.as_bytes(), 0);
        assert_eq!(parsed.messages.len(), 2);
        assert_eq!(parsed.messages[0].text, "User img [image omitted] end");
        assert_eq!(parsed.messages[1].text, "Agent img [image omitted] end");
    }

    #[test]
    fn codex_interrupted_or_superseded_request_user_input_is_not_pending() {
        let req_line = json!({
            "type": "response_item",
            "timestamp": "2026-11-26T10:00:01Z",
            "payload": {
                "type": "function_call",
                "call_id": "rq_stale",
                "name": "request_user_input",
                "arguments": json!({"questions":[{"question":"Proceed?","header":"Go","options":[{"label":"Yes","description":"Run"}]}]}).to_string()
            }
        });

        // Case 1: Aborted right after request_user_input
        let aborted_payload = format!(
            "{req_line}\n{}\n",
            json!({"type":"event_msg","timestamp":"2026-11-26T10:00:02Z","payload":{"type":"turn_aborted"}})
        );
        let p_abort = CodexProvider.parse_slice(aborted_payload.as_bytes(), 0);
        assert!(!p_abort.has_pending_question);
        assert_eq!(p_abort.tail_activity, TailActivity::Idle);
        assert!(!p_abort.messages.iter().any(|m| {
            m.blocks
                .iter()
                .any(|b| matches!(b, MessageBlock::QuestionPrompt { .. }))
        }));

        // Case 2: Superseded by later user + assistant turn
        let superseded_payload = format!(
            "{aborted_payload}{}\n{}\n",
            json!({"type":"event_msg","timestamp":"2026-11-26T10:00:03Z","payload":{"type":"user_message","message":"Do something else"}}),
            json!({"type":"event_msg","timestamp":"2026-11-26T10:00:04Z","payload":{"type":"agent_message","message":"All finished"}})
        );
        let dir =
            std::env::temp_dir().join(format!("multibotd-codex-stale-q-{}", std::process::id()));
        let nested = dir.join(".codex/sessions/2026/11/26");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("rollout-stale.jsonl"), &superseded_payload).unwrap();

        let p_sup = CodexProvider.parse_slice(superseded_payload.as_bytes(), 0);
        assert!(!p_sup.has_pending_question);
        assert_eq!(p_sup.tail_activity, TailActivity::Idle);
        assert!(!p_sup.messages.iter().any(|m| {
            m.blocks
                .iter()
                .any(|b| matches!(b, MessageBlock::QuestionPrompt { .. }))
        }));

        let sessions = discover_all_sessions(&dir, 10);
        assert_eq!(sessions.len(), 1);
        assert!(!sessions[0].needs_attention);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_extract_header_meta_scrubs_base64_in_event_msg_and_response_item_titles() {
        let dir =
            std::env::temp_dir().join(format!("multibotd-codex-title-b64-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let nested = dir.join(".codex/sessions/2026/11/27");
        fs::create_dir_all(&nested).unwrap();

        let raw_b64 = "Q".repeat(300);
        // Fixture 1: event_msg / user_message with data URI and >=256-char raw base64
        let f1 = nested.join("rollout-2026-11-27-aaaa1111-2222-3333-4444-555555555555.jsonl");
        let f1_content = format!(
            "{}\n{}\n",
            json!({"type":"session_meta","payload":{"id":"codex-title-event","cwd":"/repo"}}),
            json!({"type":"event_msg","timestamp":"2026-11-27T10:00:00Z","payload":{"type":"user_message","message":format!("Look data:image/png;base64,iVBORw0KGgo= and {raw_b64} please")}})
        );
        fs::write(&f1, f1_content).unwrap();

        // Fixture 2: response_item / message (role == "user") with data:image/webp URI
        let f2 = nested.join("rollout-2026-11-27-bbbb1111-2222-3333-4444-555555555555.jsonl");
        let f2_content = format!(
            "{}\n{}\n",
            json!({"type":"session_meta","payload":{"id":"codex-title-resp","cwd":"/repo"}}),
            json!({"type":"response_item","timestamp":"2026-11-27T10:00:01Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"See data:image/webp;base64,UklGRh4AAABXRUJQ"}]}})
        );
        fs::write(&f2, f2_content).unwrap();

        let sessions = discover_all_sessions(&dir, 10);
        assert_eq!(sessions.len(), 2);

        let s1 = sessions
            .iter()
            .find(|s| s.id == "codex-title-event")
            .unwrap();
        assert_eq!(s1.title, "Look [image omitted] and [image omitted] please");
        assert!(!s1.title.contains("data:image"));
        assert!(!s1.title.contains("QQQQ"));

        let s2 = sessions
            .iter()
            .find(|s| s.id == "codex-title-resp")
            .unwrap();
        assert_eq!(s2.title, "See [image omitted]");
        assert!(!s2.title.contains("data:image"));
        assert!(!s2.title.contains("UklGR"));

        // Also verify bare >=256-char raw base64 in input_image url is omitted cleanly
        let bare_img_payload = format!(
            "{}\n",
            json!({"type":"response_item","timestamp":"2026-11-27T10:00:02Z","payload":{"type":"message","role":"user","content":[{"type":"input_image","image_url":raw_b64}]}})
        );
        let parsed_img = CodexProvider.parse_slice(bare_img_payload.as_bytes(), 0);
        assert_eq!(parsed_img.messages.len(), 1);
        assert_eq!(parsed_img.messages[0].text, "[image omitted]");
        assert!(
            !parsed_img.messages[0]
                .blocks
                .iter()
                .any(|b| matches!(b, MessageBlock::Image { .. }))
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
