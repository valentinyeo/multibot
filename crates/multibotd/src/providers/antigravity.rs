use super::{
    AgentProvider, HeaderMeta, LineSlice, MAX_DIFF_LINES, MAX_TODOS, MAX_TOOL_DETAIL_CHARS,
    MAX_TOOL_SUMMARY_CHARS, ParsedSlice, TailActivity, append_diff_counts_to_summary,
    build_diff_block, cap_chars, collapse_whitespace, combine_tool_detail,
    compute_unified_diff_lines, extract_image_blocks, extract_text_content, format_tool_summary,
    is_safe_transcript_path, parse_iso_ms, parse_todo_status, populate_last_assistant_fields,
    read_header_lines_bounded, sanitize_todo_content, sha256_hex, strip_base64_in_str,
    strip_system_reminders, system_time_ms, tool_text, truncate_title,
};
use anyhow::Result;
use multibot_proto::{
    AgentKind, ChatMessage, DiffBlock, MessageBlock, MessageRole, TodoItem, TodoStatus, ToolStatus,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct AntigravityProvider;

/// Returns `<home>/.multibot/history/<sha256("antigravity:" + sid)[..32]>.history.jsonl`.
/// Strictly under `~/.multibot/history/` (Hard Invariant #2).
pub fn hosted_history_path(home: &Path, session_id: &str) -> PathBuf {
    let digest = sha256_hex(format!("antigravity:{session_id}").as_bytes());
    let sha32 = &digest[..32];
    home.join(".multibot")
        .join("history")
        .join(format!("{sha32}.history.jsonl"))
}

/// Appends one compact NDJSON record terminated with `\n` and flushes immediately.
pub fn append_history_record(path: &Path, record: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    let mut line = serde_json::to_string(record)?;
    line.push('\n');
    file.write_all(line.as_bytes())?;
    file.flush()?;
    Ok(())
}

pub fn antigravity_discovery_roots(home: &Path) -> Vec<PathBuf> {
    let candidates = [
        home.join(".multibot").join("history"),
        home.join(".gemini").join("antigravity").join("brain"),
        home.join(".gemini").join("jetski").join("brain"),
        home.join(".antigravity").join("sessions"),
    ];
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

#[derive(Debug, Clone)]
struct AgyToolDraft {
    raw_input_detail: String,
    diff: Option<DiffBlock>,
}

#[derive(Debug, Clone)]
pub enum AgyStreamOutput {
    Init {
        conversation_id: Option<String>,
        model: Option<String>,
    },
    UserAccepted {
        text: Option<String>,
    },
    AssistantMessage(ChatMessage),
    ToolStarted {
        call_id: String,
        name: String,
        summary: String,
        detail: String,
        diff: Option<DiffBlock>,
        todos: Vec<TodoItem>,
    },
    ToolFinished {
        call_id: String,
        status: ToolStatus,
        output: String,
        detail: String,
        diff: Option<DiffBlock>,
    },
    TurnEnded {
        status: String,
        fallback_message: Option<ChatMessage>,
    },
}

/// Reusable stateful accumulator for `agy --output-format stream-json` NDJSON events.
/// Used by both `AntigravityProvider::parse_slice` and `process.rs` live stdout streaming.
#[derive(Debug, Default)]
pub struct AgyStreamAccumulator {
    pub conversation_id: Option<String>,
    pub model: Option<String>,
    text_by_step: HashMap<i64, String>,
    emitted_text_in_turn: bool,
    open_tools: HashMap<String, AgyToolDraft>,
}

impl AgyStreamAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed_value(
        &mut self,
        val: &Value,
        timestamp_ms: Option<i64>,
        byte_range: Option<(u64, u64)>,
    ) -> Vec<AgyStreamOutput> {
        let mut events = Vec::new();
        let Some(event_name) = val.get("event").and_then(Value::as_str) else {
            return events;
        };

        match event_name {
            "init" => {
                if let Some(cid) = val
                    .get("conversation_id")
                    .or_else(|| val.get("init").and_then(|i| i.get("conversation_id")))
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                {
                    self.conversation_id = Some(cid.to_string());
                }
                if let Some(m) = val
                    .get("model")
                    .or_else(|| val.get("init").and_then(|i| i.get("model")))
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                {
                    self.model = Some(cap_chars(m.trim(), 128));
                }
                events.push(AgyStreamOutput::Init {
                    conversation_id: self.conversation_id.clone(),
                    model: self.model.clone(),
                });
            }
            "step_update" => {
                let u = val.get("step_update").unwrap_or(val);
                if let Some(cid) = u
                    .get("conversation_id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                {
                    self.conversation_id = Some(cid.to_string());
                }
                let step_type = u.get("step_type").and_then(Value::as_str).unwrap_or("");
                let state = u.get("state").and_then(Value::as_str).unwrap_or("");
                let step_index = u.get("step_index").and_then(Value::as_i64).unwrap_or(0);
                let is_terminal = matches!(state, "DONE" | "ERROR" | "CANCELED");

                match step_type {
                    "user_input" => {
                        let text = u
                            .get("text")
                            .or_else(|| u.get("content"))
                            .and_then(Value::as_str)
                            .map(|s| strip_base64_in_str(&strip_system_reminders(s)).trim().to_string())
                            .filter(|s| !s.is_empty());
                        events.push(AgyStreamOutput::UserAccepted { text });
                    }
                    "agent_response" => {
                        if let Some(delta) = u.get("text_delta").and_then(Value::as_str) {
                            let buf = self.text_by_step.entry(step_index).or_default();
                            buf.push_str(delta);
                            if !delta.is_empty() {
                                self.emitted_text_in_turn = true;
                            }
                        }
                        if is_terminal
                            && let Some(raw_text) = self.text_by_step.remove(&step_index)
                        {
                            let cleaned = strip_base64_in_str(&strip_system_reminders(&raw_text))
                                .trim()
                                .to_string();
                            if !cleaned.is_empty() {
                                let mut blocks = vec![MessageBlock::Text {
                                    text: cleaned.clone(),
                                }];
                                blocks.extend(extract_image_blocks(&cleaned));
                                events.push(AgyStreamOutput::AssistantMessage(ChatMessage {
                                    id: format!("agy-{step_index}"),
                                    role: MessageRole::Assistant,
                                    text: cleaned,
                                    timestamp_ms,
                                    blocks,
                                    source_byte_range: byte_range,
                                    ..Default::default()
                                }));
                            }
                        }
                    }
                    "tool" => {
                        let call_id = format!("agy-tool-{step_index}");
                        let info = u.get("tool_info").unwrap_or(&Value::Null);
                        let tool_name = u
                            .get("tool_name")
                            .or_else(|| info.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or("tool");
                        let params = info.get("parameters").unwrap_or(&Value::Null);

                        if !self.open_tools.contains_key(&call_id) {
                            let mut summary = format_tool_summary(tool_name, params);
                            let raw_detail = tool_text(params);
                            let detail = cap_chars(&raw_detail, MAX_TOOL_DETAIL_CHARS);
                            let diff = extract_agy_tool_diff(tool_name, params);
                            if let Some(ref d) = diff {
                                summary = append_diff_counts_to_summary(&summary, d);
                            }
                            let todos = extract_agy_todos(tool_name, params);
                            self.open_tools.insert(
                                call_id.clone(),
                                AgyToolDraft {
                                    raw_input_detail: raw_detail,
                                    diff: diff.clone(),
                                },
                            );
                            events.push(AgyStreamOutput::ToolStarted {
                                call_id: call_id.clone(),
                                name: tool_name.to_string(),
                                summary,
                                detail,
                                diff,
                                todos,
                            });
                        }

                        if is_terminal {
                            let failed = state != "DONE"
                                || info.get("is_error").and_then(Value::as_bool) == Some(true);
                            let out_val = info
                                .get("output")
                                .or_else(|| info.get("error"))
                                .unwrap_or(&Value::Null);
                            let output = cap_chars(&tool_text(out_val), MAX_TOOL_DETAIL_CHARS);
                            let draft = self.open_tools.remove(&call_id);
                            let detail = match &draft {
                                Some(d) => combine_tool_detail(&d.raw_input_detail, &output),
                                None => output.clone(),
                            };
                            let status = if failed {
                                ToolStatus::Error
                            } else {
                                ToolStatus::Completed
                            };
                            events.push(AgyStreamOutput::ToolFinished {
                                call_id,
                                status,
                                output,
                                detail,
                                diff: draft.and_then(|d| d.diff),
                            });
                        }
                    }
                    _ => {}
                }
            }
            "result" => {
                let r = val.get("result").unwrap_or(val);
                if let Some(cid) = r
                    .get("conversation_id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                {
                    self.conversation_id = Some(cid.to_string());
                }
                let fallback_message = if !self.emitted_text_in_turn {
                    r.get("response")
                        .and_then(Value::as_str)
                        .map(|s| strip_base64_in_str(&strip_system_reminders(s)).trim().to_string())
                        .filter(|s| !s.is_empty())
                        .map(|text| {
                            let mut blocks = vec![MessageBlock::Text { text: text.clone() }];
                            blocks.extend(extract_image_blocks(&text));
                            ChatMessage {
                                id: "agy-result".to_string(),
                                role: MessageRole::Assistant,
                                text,
                                timestamp_ms,
                                blocks,
                                source_byte_range: byte_range,
                                ..Default::default()
                            }
                        })
                } else {
                    None
                };

                let finished_reason = r
                    .get("finished_reason")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let status_raw = r.get("status").and_then(Value::as_str).unwrap_or("");
                let status = if finished_reason.eq_ignore_ascii_case("interrupted") {
                    "interrupted".to_string()
                } else if status_raw.eq_ignore_ascii_case("SUCCESS") {
                    "completed".to_string()
                } else {
                    "failed".to_string()
                };

                // Reset per-turn buffers
                self.text_by_step.clear();
                self.emitted_text_in_turn = false;
                self.open_tools.clear();

                events.push(AgyStreamOutput::TurnEnded {
                    status,
                    fallback_message,
                });
            }
            _ => {}
        }
        events
    }

    /// Flushes any non-empty buffered `agent_response` text at EOF of a static slice.
    pub fn flush_pending_text(&mut self) -> Vec<ChatMessage> {
        let mut keys: Vec<i64> = self.text_by_step.keys().copied().collect();
        keys.sort_unstable();
        let mut out = Vec::new();
        for k in keys {
            if let Some(raw) = self.text_by_step.remove(&k) {
                let cleaned = strip_base64_in_str(&strip_system_reminders(&raw))
                    .trim()
                    .to_string();
                if !cleaned.is_empty() {
                    let mut blocks = vec![MessageBlock::Text {
                        text: cleaned.clone(),
                    }];
                    blocks.extend(extract_image_blocks(&cleaned));
                    out.push(ChatMessage {
                        id: format!("agy-{k}"),
                        role: MessageRole::Assistant,
                        text: cleaned,
                        blocks,
                        ..Default::default()
                    });
                }
            }
        }
        out
    }
}

fn extract_agy_tool_diff(tool_name: &str, params: &Value) -> Option<DiffBlock> {
    let kind = tool_name
        .rsplit('.')
        .next()
        .unwrap_or(tool_name)
        .to_ascii_lowercase();
    let path = params
        .get("file_path")
        .or_else(|| params.get("path"))
        .and_then(Value::as_str)
        .map(String::from);
    match kind.as_str() {
        "edit" | "edit_file" => {
            let old = params.get("old_string").and_then(Value::as_str).unwrap_or("");
            let new = params.get("new_string").and_then(Value::as_str).unwrap_or("");
            build_diff_block(path, compute_unified_diff_lines(old, new, 1))
        }
        "write" | "write_file" | "write_to_file" => {
            let content = params.get("content").and_then(Value::as_str)?;
            build_diff_block(path, content.lines().map(|l| format!("+{l}")).collect())
        }
        _ => None,
    }
}

fn extract_agy_todos(tool_name: &str, params: &Value) -> Vec<TodoItem> {
    let kind = tool_name.to_ascii_lowercase();
    if !matches!(kind.as_str(), "todowrite" | "todo_write" | "update_plan") {
        return Vec::new();
    }
    let arr = params
        .get("todos")
        .or_else(|| params.get("plan"))
        .and_then(Value::as_array);
    let Some(items) = arr else {
        return Vec::new();
    };
    items
        .iter()
        .take(MAX_TODOS)
        .filter_map(|item| {
            let content = item
                .get("content")
                .or_else(|| item.get("step"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())?;
            let status = item
                .get("status")
                .and_then(Value::as_str)
                .map(parse_todo_status)
                .unwrap_or(TodoStatus::Pending);
            Some(TodoItem {
                content: sanitize_todo_content(content),
                status,
            })
        })
        .collect()
}

fn parse_tool_status_str(s: &str) -> ToolStatus {
    match s.trim().to_ascii_lowercase().as_str() {
        "running" | "pending" | "in_progress" => ToolStatus::Running,
        "error" | "failed" | "canceled" => ToolStatus::Error,
        _ => ToolStatus::Completed, // handles "completed", "ok", "success"
    }
}

fn parse_row_timestamp(row: &Value) -> Option<i64> {
    row.get("timestamp_ms")
        .and_then(Value::as_i64)
        .or_else(|| {
            row.get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_iso_ms)
        })
}

fn parse_row_todos(row: &Value) -> Vec<TodoItem> {
    row.get("todos")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .take(MAX_TODOS)
                .filter_map(|item| {
                    let content = item
                        .get("content")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())?;
                    let status = item
                        .get("status")
                        .and_then(Value::as_str)
                        .map(parse_todo_status)
                        .unwrap_or(TodoStatus::Pending);
                    Some(TodoItem {
                        content: sanitize_todo_content(content),
                        status,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

impl AgentProvider for AntigravityProvider {
    fn kind(&self) -> AgentKind {
        AgentKind::Antigravity
    }

    fn discover_paths(&self, home: &Path, limit: usize) -> Vec<(PathBuf, i64)> {
        let mut files = Vec::new();
        for (priority, root) in antigravity_discovery_roots(home).into_iter().enumerate() {
            for entry in WalkDir::new(&root).max_depth(5).into_iter().flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                if !is_safe_transcript_path(path, &root) {
                    continue;
                }
                if let Ok(meta) = fs::metadata(path)
                    && let Ok(modified) = meta.modified()
                {
                    files.push((priority, path.to_path_buf(), system_time_ms(modified)));
                }
            }
        }
        // Prefer ~/.multibot/history (priority 0) first when deduplicating by session_id,
        // otherwise newest mtime first.
        files.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.2.cmp(&a.2)));
        let mut out = Vec::new();
        let mut seen_ids = HashSet::new();
        for (_, path, ts) in files {
            let sid = self
                .extract_header_meta(&path)
                .session_id_hint
                .unwrap_or_else(|| {
                    path.file_stem()
                        .and_then(|s| s.to_str())
                        .map(|s| s.strip_suffix(".history").unwrap_or(s))
                        .unwrap_or("session")
                        .to_string()
                });
            if seen_ids.insert(sid) {
                out.push((path, ts));
            }
            if out.len() >= limit {
                break;
            }
        }
        out.sort_by(|a, b| b.1.cmp(&a.1));
        out
    }

    fn extract_header_meta(&self, path: &Path) -> HeaderMeta {
        let mut meta = HeaderMeta::default();
        for line in read_header_lines_bounded(path, 24) {
            let Ok(val) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let top_type = val.get("type").and_then(Value::as_str).unwrap_or("");
            if top_type == "multibot_meta" {
                if let Some(sid) = val
                    .get("session_id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                {
                    meta.session_id_hint = Some(sid.to_string());
                }
                if let Some(cwd) = val
                    .get("cwd")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                {
                    meta.cwd = Some(cwd.to_string());
                }
                if let Some(title) = val
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                {
                    meta.title_hint = Some(truncate_title(title, 64));
                }
                if let Some(model) = val
                    .get("model")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                {
                    meta.model = Some(cap_chars(model.trim(), 128));
                }
                continue;
            }
            if meta.session_id_hint.is_none()
                && let Some(sid) = val
                    .get("session_id")
                    .or_else(|| val.get("conversation_id"))
                    .or_else(|| {
                        val.get("step_update")
                            .and_then(|u| u.get("conversation_id"))
                    })
                    .or_else(|| val.get("result").and_then(|r| r.get("conversation_id")))
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
            {
                meta.session_id_hint = Some(sid.to_string());
            }
            if meta.model.is_none()
                && let Some(model) = val
                    .get("model")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
            {
                meta.model = Some(cap_chars(model.trim(), 128));
            }
            if meta.cwd.is_none() {
                meta.cwd = val
                    .get("cwd")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .map(String::from);
            }
            if meta.title_hint.is_none() {
                let candidate = val
                    .get("title")
                    .and_then(Value::as_str)
                    .or_else(|| {
                        val.get("row")
                            .filter(|r| r.get("role").and_then(Value::as_str) == Some("user"))
                            .and_then(|r| r.get("text"))
                            .and_then(Value::as_str)
                    })
                    .or_else(|| {
                        val.get("step_update")
                            .filter(|u| {
                                u.get("step_type").and_then(Value::as_str) == Some("user_input")
                            })
                            .and_then(|u| u.get("text").or_else(|| u.get("content")))
                            .and_then(Value::as_str)
                    })
                    .or_else(|| {
                        if matches!(
                            val.get("role").and_then(Value::as_str),
                            Some("user" | "human")
                        ) {
                            val.get("text")
                                .or_else(|| val.get("content"))
                                .and_then(Value::as_str)
                        } else {
                            None
                        }
                    });
                if let Some(t) = candidate {
                    let scrubbed = strip_base64_in_str(&strip_system_reminders(t));
                    if let Some(first) = scrubbed.lines().map(str::trim).find(|l| !l.is_empty()) {
                        meta.title_hint = Some(truncate_title(first, 64));
                    }
                }
            }
        }
        meta
    }

    fn parse_lines(&self, lines: &[LineSlice<'_>]) -> ParsedSlice {
        let mut parsed = ParsedSlice::default();
        let mut stream_acc = AgyStreamAccumulator::new();
        let mut open_tools: HashMap<String, (usize, usize, String)> = HashMap::new();

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
            let byte_range = Some((line.start_offset, line.end_offset));
            let top_type = val.get("type").and_then(Value::as_str).unwrap_or("");

            // 1. Daemon-owned metadata header
            if top_type == "multibot_meta" {
                if let Some(sid) = val
                    .get("session_id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                {
                    parsed.session_id_hint = Some(sid.to_string());
                }
                if let Some(cwd) = val
                    .get("cwd")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                {
                    parsed.cwd = Some(cwd.to_string());
                }
                if let Some(title) = val
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                {
                    parsed.title_hint = Some(truncate_title(title, 64));
                }
                if let Some(model) = val
                    .get("model")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                {
                    parsed.model = Some(cap_chars(model.trim(), 128));
                }
                continue;
            }

            // 2. Turn end marker
            if top_type == "multibot_turn_end" {
                parsed.tail_activity = TailActivity::Idle;
                continue;
            }

            // 3. Daemon-owned history rows (plus read-only legacy multiprompt_history)
            if top_type == "multibot_history" || top_type == "multiprompt_history" {
                if let Some(sid) = val
                    .get("session_id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                {
                    parsed.session_id_hint = Some(sid.to_string());
                }
                if let Some(cwd) = val
                    .get("cwd")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                {
                    parsed.cwd = Some(cwd.to_string());
                }
                let Some(row) = val.get("row") else {
                    continue;
                };
                let role_str = row.get("role").and_then(Value::as_str).unwrap_or("");
                let ts = parse_row_timestamp(row);
                let row_id = row
                    .get("id")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .unwrap_or_else(|| format!("agy-{}", line.start_offset));

                match role_str {
                    "user" => {
                        let text = row
                            .get("text")
                            .or_else(|| row.get("content"))
                            .map(extract_text_content)
                            .unwrap_or_default();
                        let text = strip_base64_in_str(&strip_system_reminders(&text))
                            .trim()
                            .to_string();
                        if !text.is_empty() {
                            let mut blocks = vec![MessageBlock::Text { text: text.clone() }];
                            blocks.extend(extract_image_blocks(&text));
                            parsed.messages.push(ChatMessage {
                                id: row_id,
                                role: MessageRole::User,
                                text,
                                timestamp_ms: ts,
                                blocks,
                                source_byte_range: byte_range,
                                ..Default::default()
                            });
                            parsed.tail_activity = TailActivity::Working;
                        }
                    }
                    "assistant" => {
                        let text = row
                            .get("text")
                            .or_else(|| row.get("content"))
                            .map(extract_text_content)
                            .unwrap_or_default();
                        let text = strip_base64_in_str(&strip_system_reminders(&text))
                            .trim()
                            .to_string();
                        let todos = parse_row_todos(row);
                        if !todos.is_empty() {
                            parsed.todos = todos.clone();
                        }
                        if !text.is_empty() || !todos.is_empty() {
                            let mut blocks = Vec::new();
                            if !text.is_empty() {
                                blocks.push(MessageBlock::Text { text: text.clone() });
                                blocks.extend(extract_image_blocks(&text));
                            }
                            parsed.messages.push(ChatMessage {
                                id: row_id,
                                role: MessageRole::Assistant,
                                text,
                                timestamp_ms: ts,
                                blocks,
                                todos,
                                source_byte_range: byte_range,
                                ..Default::default()
                            });
                            parsed.tail_activity = TailActivity::Idle;
                        }
                    }
                    "tool" => {
                        let call_id = row
                            .get("call_id")
                            .or_else(|| row.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or(&row_id)
                            .to_string();
                        let name = row.get("name").and_then(Value::as_str).unwrap_or("tool");
                        let summary = row
                            .get("summary")
                            .and_then(Value::as_str)
                            .map(|s| {
                                cap_chars(
                                    &collapse_whitespace(&strip_base64_in_str(s)),
                                    MAX_TOOL_SUMMARY_CHARS,
                                )
                            })
                            .unwrap_or_else(|| {
                                format_tool_summary(name, row.get("input").unwrap_or(&Value::Null))
                            });
                        let raw_detail = row
                            .get("detail")
                            .and_then(Value::as_str)
                            .map(strip_base64_in_str)
                            .unwrap_or_else(|| {
                                tool_text(row.get("input").unwrap_or(&Value::Null))
                            });
                        let detail = cap_chars(&raw_detail, MAX_TOOL_DETAIL_CHARS);
                        let status = row
                            .get("status")
                            .and_then(Value::as_str)
                            .map(parse_tool_status_str)
                            .unwrap_or(ToolStatus::Running);
                        let diff: Option<DiffBlock> = row
                            .get("diff")
                            .and_then(|d| serde_json::from_value::<DiffBlock>(d.clone()).ok())
                            .map(|mut d| {
                                let extra = d.lines.len().saturating_sub(MAX_DIFF_LINES) as u32;
                                d.truncated_lines = d.truncated_lines.saturating_add(extra);
                                d.lines = d
                                    .lines
                                    .into_iter()
                                    .take(MAX_DIFF_LINES)
                                    .map(|l| strip_base64_in_str(&l))
                                    .collect();
                                d
                            });
                        let output = row
                            .get("output")
                            .and_then(Value::as_str)
                            .map(|s| cap_chars(&strip_base64_in_str(s), MAX_TOOL_DETAIL_CHARS));
                        let todos = parse_row_todos(row);
                        if !todos.is_empty() {
                            parsed.todos = todos.clone();
                        }

                        let block = MessageBlock::ToolUse {
                            call_id: call_id.clone(),
                            name: name.to_string(),
                            summary,
                            detail,
                            status,
                            diff,
                            output,
                        };
                        let (msg_idx, blk_idx) = if parsed
                            .messages
                            .last()
                            .is_some_and(|m| m.role == MessageRole::Assistant)
                        {
                            let idx = parsed.messages.len() - 1;
                            let b_idx = parsed.messages[idx].blocks.len();
                            parsed.messages[idx].blocks.push(block);
                            if !todos.is_empty() {
                                parsed.messages[idx].todos = todos;
                            }
                            (idx, b_idx)
                        } else {
                            let idx = parsed.messages.len();
                            parsed.messages.push(ChatMessage {
                                id: row_id,
                                role: MessageRole::Assistant,
                                text: String::new(),
                                timestamp_ms: ts,
                                blocks: vec![block],
                                todos,
                                source_byte_range: byte_range,
                                ..Default::default()
                            });
                            (idx, 0)
                        };
                        if status == ToolStatus::Running {
                            open_tools.insert(call_id, (msg_idx, blk_idx, raw_detail));
                            parsed.tail_activity = TailActivity::Working;
                        }
                    }
                    "tool_result" => {
                        let call_id = row
                            .get("call_id")
                            .or_else(|| row.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or(&row_id)
                            .to_string();
                        let status = row
                            .get("status")
                            .and_then(Value::as_str)
                            .map(parse_tool_status_str)
                            .unwrap_or(ToolStatus::Completed);
                        let output = row
                            .get("output")
                            .or_else(|| row.get("detail"))
                            .map(tool_text)
                            .map(|s| cap_chars(&s, MAX_TOOL_DETAIL_CHARS))
                            .unwrap_or_default();

                        if let Some((msg_idx, blk_idx, orig_input)) = open_tools.remove(&call_id) {
                            if let Some(msg) = parsed.messages.get_mut(msg_idx)
                                && let Some(MessageBlock::ToolUse {
                                    status: blk_status,
                                    detail,
                                    output: blk_out,
                                    ..
                                }) = msg.blocks.get_mut(blk_idx)
                            {
                                *blk_status = status;
                                *blk_out = if output.is_empty() {
                                    None
                                } else {
                                    Some(output.clone())
                                };
                                *detail = combine_tool_detail(&orig_input, &output);
                            }
                        } else {
                            parsed.messages.push(ChatMessage {
                                id: format!("tool-result-{call_id}"),
                                role: MessageRole::Tool,
                                text: String::new(),
                                timestamp_ms: ts,
                                blocks: vec![MessageBlock::ToolUse {
                                    call_id,
                                    name: String::new(),
                                    summary: String::new(),
                                    detail: output.clone(),
                                    status,
                                    diff: None,
                                    output: if output.is_empty() {
                                        None
                                    } else {
                                        Some(output)
                                    },
                                }],
                                source_byte_range: byte_range,
                                ..Default::default()
                            });
                        }
                    }
                    _ => {}
                }
                continue;
            }

            // 4. Raw `agy` stream-json events
            if val.get("event").and_then(Value::as_str).is_some() {
                let ts = parse_row_timestamp(&val);
                for ev in stream_acc.feed_value(&val, ts, byte_range) {
                    match ev {
                        AgyStreamOutput::Init {
                            conversation_id,
                            model,
                        } => {
                            if parsed.session_id_hint.is_none()
                                && let Some(cid) = conversation_id
                            {
                                parsed.session_id_hint = Some(cid);
                            }
                            if let Some(m) = model {
                                parsed.model = Some(m);
                            }
                        }
                        AgyStreamOutput::UserAccepted { text } => {
                            if let Some(t) = text {
                                if parsed.title_hint.is_none()
                                    && let Some(first) =
                                        t.lines().map(str::trim).find(|l| !l.is_empty())
                                {
                                    parsed.title_hint = Some(truncate_title(first, 64));
                                }
                                let mut blocks = vec![MessageBlock::Text { text: t.clone() }];
                                blocks.extend(extract_image_blocks(&t));
                                parsed.messages.push(ChatMessage {
                                    id: format!("agy-user-{}", line.start_offset),
                                    role: MessageRole::User,
                                    text: t,
                                    timestamp_ms: ts,
                                    blocks,
                                    source_byte_range: byte_range,
                                    ..Default::default()
                                });
                            }
                            parsed.tail_activity = TailActivity::Working;
                        }
                        AgyStreamOutput::AssistantMessage(msg) => {
                            parsed.messages.push(msg);
                            parsed.tail_activity = TailActivity::Idle;
                        }
                        AgyStreamOutput::ToolStarted {
                            call_id,
                            name,
                            summary,
                            detail,
                            diff,
                            todos,
                        } => {
                            if !todos.is_empty() {
                                parsed.todos = todos.clone();
                            }
                            let block = MessageBlock::ToolUse {
                                call_id: call_id.clone(),
                                name,
                                summary,
                                detail: detail.clone(),
                                status: ToolStatus::Running,
                                diff,
                                output: None,
                            };
                            let (msg_idx, blk_idx) = if parsed
                                .messages
                                .last()
                                .is_some_and(|m| m.role == MessageRole::Assistant)
                            {
                                let idx = parsed.messages.len() - 1;
                                let b_idx = parsed.messages[idx].blocks.len();
                                parsed.messages[idx].blocks.push(block);
                                if !todos.is_empty() {
                                    parsed.messages[idx].todos = todos;
                                }
                                (idx, b_idx)
                            } else {
                                let idx = parsed.messages.len();
                                parsed.messages.push(ChatMessage {
                                    id: format!("agy-{}", line.start_offset),
                                    role: MessageRole::Assistant,
                                    text: String::new(),
                                    timestamp_ms: ts,
                                    blocks: vec![block],
                                    todos,
                                    source_byte_range: byte_range,
                                    ..Default::default()
                                });
                                (idx, 0)
                            };
                            open_tools.insert(call_id, (msg_idx, blk_idx, detail));
                            parsed.tail_activity = TailActivity::Working;
                        }
                        AgyStreamOutput::ToolFinished {
                            call_id,
                            status,
                            output,
                            detail,
                            diff,
                        } => {
                            if let Some((msg_idx, blk_idx, _)) = open_tools.remove(&call_id)
                                && let Some(msg) = parsed.messages.get_mut(msg_idx)
                                && let Some(MessageBlock::ToolUse {
                                    status: blk_status,
                                    output: blk_out,
                                    detail: blk_detail,
                                    diff: blk_diff,
                                    ..
                                }) = msg.blocks.get_mut(blk_idx)
                            {
                                *blk_status = status;
                                *blk_out = if output.is_empty() {
                                    None
                                } else {
                                    Some(output)
                                };
                                *blk_detail = detail;
                                if diff.is_some() {
                                    *blk_diff = diff;
                                }
                            }
                        }
                        AgyStreamOutput::TurnEnded {
                            fallback_message, ..
                        } => {
                            if let Some(msg) = fallback_message {
                                parsed.messages.push(msg);
                            }
                            parsed.tail_activity = TailActivity::Idle;
                        }
                    }
                }
                continue;
            }

            // 5. Generic JSONL fallback (compatible with arbitrary brain/sessions files)
            let Some(role_str) = val
                .get("role")
                .or_else(|| val.get("author"))
                .or_else(|| val.get("type"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            let role = match role_str.to_ascii_lowercase().as_str() {
                "user" | "human" | "user_message" => MessageRole::User,
                "assistant" | "model" | "agent" | "assistant_message" => MessageRole::Assistant,
                _ => continue,
            };
            let text = val
                .get("text")
                .or_else(|| val.get("content"))
                .or_else(|| val.get("message"))
                .map(extract_text_content)
                .unwrap_or_default();
            let text = strip_base64_in_str(&strip_system_reminders(&text))
                .trim()
                .to_string();
            if text.is_empty() {
                continue;
            }
            let ts = parse_row_timestamp(&val);
            let mut blocks = vec![MessageBlock::Text { text: text.clone() }];
            blocks.extend(extract_image_blocks(&text));
            parsed.messages.push(ChatMessage {
                id: format!("agy-{}", line.start_offset),
                role,
                text,
                timestamp_ms: ts,
                blocks,
                source_byte_range: byte_range,
                ..Default::default()
            });
            parsed.tail_activity = match role {
                MessageRole::User => TailActivity::Working,
                _ => TailActivity::Idle,
            };
        }

        // Flush any unfinished streamed agent_response text at EOF
        for msg in stream_acc.flush_pending_text() {
            parsed.messages.push(msg);
        }

        if !open_tools.is_empty() && parsed.tail_activity != TailActivity::Idle {
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
    fn antigravity_discovers_all_roots_preferring_multibot_history() {
        let dir = std::env::temp_dir().join(format!(
            "multibotd-agy-disc-{}",
            std::process::id()
        ));
        let hosted = hosted_history_path(&dir, "agy-1");
        append_history_record(
            &hosted,
            &json!({"type":"multibot_meta","session_id":"agy-1","agent":"antigravity","cwd":"/repo","title":"Hosted Agy","model":"gemini-4"}),
        )
        .unwrap();
        append_history_record(
            &hosted,
            &json!({"type":"multibot_history","row":{"id":"u1","role":"user","timestamp_ms":1000,"text":"Fix tests"}}),
        )
        .unwrap();
        append_history_record(
            &hosted,
            &json!({"type":"multibot_history","row":{"id":"a1","role":"assistant","timestamp_ms":2000,"text":"Tests fixed"}}),
        )
        .unwrap();

        let brain_dir = dir.join(".gemini/antigravity/brain");
        fs::create_dir_all(&brain_dir).unwrap();
        fs::write(
            brain_dir.join("legacy-session.jsonl"),
            r#"{"role":"user","text":"Legacy brain prompt","timestamp_ms":500}"#.to_string() + "\n",
        )
        .unwrap();

        let sessions = discover_all_sessions(&dir, 10);
        assert_eq!(sessions.len(), 2);
        let hosted_s = sessions.iter().find(|s| s.id == "agy-1").unwrap();
        assert_eq!(hosted_s.title, "Hosted Agy");
        assert_eq!(hosted_s.model.as_deref(), Some("gemini-4"));
        assert_eq!(hosted_s.last_assistant_preview.as_deref(), Some("Tests fixed"));
        assert!(
            Path::new(&hosted_s.source_path)
                .starts_with(dir.join(".multibot").join("history"))
        );

        let legacy_s = sessions.iter().find(|s| s.id == "legacy-session").unwrap();
        assert_eq!(legacy_s.title, "Legacy brain prompt");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn antigravity_parses_stream_json_and_daemon_history_rows() {
        let stream_lines = vec![
            json!({"event":"init","conversation_id":"conv-42","model":"gemini-4-neon"}),
            json!({"event":"step_update","step_update":{"step_type":"user_input","step_index":0,"state":"DONE","text":"Refactor parser"}}),
            json!({"event":"step_update","step_update":{"step_type":"tool","step_index":1,"state":"RUNNING","tool_name":"edit_file","tool_info":{"parameters":{"file_path":"src/lib.rs","old_string":"a","new_string":"b"}}}}),
            json!({"event":"step_update","step_update":{"step_type":"tool","step_index":1,"state":"DONE","tool_name":"edit_file","tool_info":{"output":"updated"}}}),
            json!({"event":"step_update","step_update":{"step_type":"agent_response","step_index":2,"state":"RUNNING","text_delta":"Hello "}}),
            json!({"event":"step_update","step_update":{"step_type":"agent_response","step_index":2,"state":"DONE","text_delta":"world!"}}),
            json!({"event":"result","result":{"status":"SUCCESS","conversation_id":"conv-42","response":"fallback"}}),
        ];
        let payload: String = stream_lines.into_iter().map(|v| format!("{v}\n")).collect();
        let parsed = AntigravityProvider.parse_slice(payload.as_bytes(), 0);

        assert_eq!(parsed.model.as_deref(), Some("gemini-4-neon"));
        assert_eq!(parsed.tail_activity, TailActivity::Idle);
        // user + assistant(tool) + assistant(agent_response); fallback suppressed because text_delta was emitted
        assert_eq!(parsed.messages.len(), 3);
        assert_eq!(parsed.messages[0].text, "Refactor parser");
        assert_eq!(parsed.messages[2].text, "Hello world!");

        let tool_blk = parsed.messages[1].blocks.iter().find_map(|b| match b {
            MessageBlock::ToolUse { call_id, summary, status, diff, output, .. } => {
                Some((call_id, summary, status, diff, output))
            }
            _ => None,
        }).unwrap();
        assert_eq!(tool_blk.0, "agy-tool-1");
        assert_eq!(tool_blk.1, "Edited lib.rs +1 -1");
        assert_eq!(*tool_blk.2, ToolStatus::Completed);
        assert_eq!(tool_blk.3.as_ref().unwrap().added, 1);
        assert_eq!(tool_blk.4.as_deref(), Some("updated"));

        // Also test fallback result.response when no text_delta was emitted
        let fallback_payload = format!(
            "{}\n",
            json!({"event":"result","result":{"status":"SUCCESS","response":"Final fallback answer"}})
        );
        let parsed_fb = AntigravityProvider.parse_slice(fallback_payload.as_bytes(), 0);
        assert_eq!(parsed_fb.messages.len(), 1);
        assert_eq!(parsed_fb.messages[0].text, "Final fallback answer");
    }

    #[test]
    fn antigravity_parses_multibot_history_and_legacy_multiprompt_history() {
        let lines = vec![
            json!({"type":"multibot_history","row":{"id":"t1","role":"tool","name":"Bash","summary":"Ran cargo check","detail":"cargo check","status":"running"}}),
            json!({"type":"multiprompt_history","row":{"id":"t1","role":"tool_result","status":"ok","output":"Finished"}}),
            json!({"type":"multibot_history","row":{"id":"a1","role":"assistant","text":"All green","todos":[{"content":"Check","status":"completed"}]}}),
        ];
        let payload: String = lines.into_iter().map(|v| format!("{v}\n")).collect();
        let parsed = AntigravityProvider.parse_slice(payload.as_bytes(), 0);
        assert_eq!(parsed.messages.len(), 2);
        assert_eq!(parsed.todos.len(), 1);
        assert_eq!(parsed.todos[0].status, TodoStatus::Completed);
        let status = parsed.messages[0].blocks.iter().find_map(|b| match b {
            MessageBlock::ToolUse { status, .. } => Some(*status),
            _ => None,
        });
        assert_eq!(status, Some(ToolStatus::Completed));
    }

    #[test]
    fn antigravity_scrubs_inline_60kb_base64_and_extracts_raw_stream_meta() {
        let blob = "C".repeat(60_000);
        let stream_lines = vec![
            json!({"event":"init","conversation_id":"conv-stream-99","model":"gemini-4-pro"}),
            json!({"event":"step_update","step_update":{"step_type":"user_input","step_index":0,"state":"DONE","text":format!("Inspect data:image/png;base64,{blob} please")}}),
            json!({"event":"step_update","step_update":{"step_type":"agent_response","step_index":1,"state":"DONE","text_delta":format!("Result data:image/webp;base64,{blob} ok")}}),
        ];
        let payload: String = stream_lines.into_iter().map(|v| format!("{v}\n")).collect();
        let parsed = AntigravityProvider.parse_slice(payload.as_bytes(), 0);
        assert_eq!(parsed.session_id_hint.as_deref(), Some("conv-stream-99"));
        assert_eq!(
            parsed.title_hint.as_deref(),
            Some("Inspect [image omitted] please")
        );
        assert_eq!(parsed.messages.len(), 2);
        assert_eq!(parsed.messages[0].text, "Inspect [image omitted] please");
        assert_eq!(parsed.messages[1].text, "Result [image omitted] ok");

        // Verify ListSessions / discover_all_sessions also picks up conversation_id & user_input title
        let dir = std::env::temp_dir().join(format!(
            "multibotd-agy-raw-stream-{}",
            std::process::id()
        ));
        let brain = dir.join(".gemini/antigravity/brain");
        fs::create_dir_all(&brain).unwrap();
        fs::write(brain.join("stream_file.jsonl"), &payload).unwrap();

        let sessions = discover_all_sessions(&dir, 10);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "conv-stream-99");
        assert_eq!(sessions[0].title, "Inspect [image omitted] please");
        assert_eq!(sessions[0].model.as_deref(), Some("gemini-4-pro"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn antigravity_scrubs_base64_in_tool_summary_diff_todos_and_meta_title() {
        let dir = std::env::temp_dir().join(format!(
            "multibotd-agy-b64-scrub-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let hosted = hosted_history_path(&dir, "agy-b64");
        let raw_b64 = "Z".repeat(300);

        append_history_record(
            &hosted,
            &json!({
                "type": "multibot_meta",
                "session_id": "agy-b64",
                "agent": "antigravity",
                "cwd": "/repo",
                "title": "Cap data:image/png;base64,iVBORw0KGgo= title"
            }),
        )
        .unwrap();
        append_history_record(
            &hosted,
            &json!({
                "type": "multibot_history",
                "row": {
                    "id": "t-b64",
                    "role": "tool",
                    "name": "Bash",
                    "summary": format!("Cap data:image/png;base64,iVBORw0KGgo=  extra\nspaces {raw_b64}"),
                    "detail": "ok",
                    "status": "completed",
                    "diff": {
                        "path": "src/img.ts",
                        "added": 1,
                        "removed": 0,
                        "lines": ["+const img = \"data:image/png;base64,iVBORw0KGgo=\";"],
                        "truncated_lines": 0
                    },
                    "todos": [
                        {"content": format!("Check data:image/png;base64,iVBORw0KGgo= and {raw_b64}"), "status": "in_progress"}
                    ]
                }
            }),
        )
        .unwrap();

        let bytes = fs::read(&hosted).unwrap();
        let parsed = AntigravityProvider.parse_slice(&bytes, 0);
        assert_eq!(parsed.messages.len(), 1);
        let tool_blk = parsed.messages[0]
            .blocks
            .iter()
            .find_map(|b| match b {
                MessageBlock::ToolUse { summary, diff, .. } => Some((summary, diff)),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            tool_blk.0,
            "Cap [image omitted] extra spaces [image omitted]"
        );
        assert!(tool_blk.0.chars().count() <= MAX_TOOL_SUMMARY_CHARS);
        assert!(!tool_blk.0.contains("data:image"));
        assert!(!tool_blk.0.contains("ZZZZ"));

        let diff = tool_blk.1.as_ref().unwrap();
        assert_eq!(diff.lines, vec!["+const img = \"[image omitted]\";"]);

        assert_eq!(parsed.todos.len(), 1);
        assert_eq!(
            parsed.todos[0].content,
            "Check [image omitted] and [image omitted]"
        );

        let sessions = discover_all_sessions(&dir, 10);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title, "Cap [image omitted] title");
        let preview = sessions[0].last_assistant_preview.as_deref().unwrap();
        assert!(preview.contains("[image omitted]"), "preview={preview}");
        assert!(!preview.contains("data:image"));
        assert!(!preview.contains("iVBOR"));
        assert!(!preview.contains("ZZZZ"));

        let _ = fs::remove_dir_all(&dir);
    }
}
