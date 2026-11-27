use super::{
    AgentProvider, HeaderMeta, LineSlice, MAX_TODOS, MAX_TOOL_DETAIL_CHARS, ParsedSlice,
    TailActivity, append_diff_counts_to_summary, build_diff_block, cap_chars, combine_tool_detail,
    compute_unified_diff_lines, extract_image_blocks, extract_text_content, format_tool_summary,
    is_safe_transcript_path, parse_iso_ms, parse_prompt_questions, parse_todo_status,
    populate_last_assistant_fields, read_header_lines_bounded, sanitize_todo_content,
    strip_base64_cow, strip_base64_in_str, strip_system_reminders, strip_tracking_suffix,
    system_time_ms, tool_text, truncate_title,
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

pub struct ClaudeProvider;

/// Exact parity with Claude CLI sanitized project directory names.
pub fn sanitize_claude_project_dir(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

pub fn claude_discovery_roots(home: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(cfg) = std::env::var("CLAUDE_CONFIG_DIR") {
        let trimmed = cfg.trim();
        if !trimmed.is_empty() {
            let expanded = if let Some(rest) = trimmed.strip_prefix("~/") {
                home.join(rest)
            } else {
                PathBuf::from(trimmed)
            };
            candidates.push(expanded.join("projects"));
        }
    }
    candidates.push(home.join(".claude").join("projects"));
    candidates.push(home.join(".config").join("claude").join("projects"));

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

pub fn format_claude_model(raw: &str) -> String {
    static MODEL_RE: OnceLock<Regex> = OnceLock::new();
    let stripped = raw.trim().strip_prefix("claude-").unwrap_or(raw.trim());
    let re = MODEL_RE.get_or_init(|| {
        Regex::new(r"^(?i)(opus|sonnet|fable|haiku)-(\d+)-(\d+)").expect("valid claude model regex")
    });
    let formatted = if let Some(cap) = re.captures(stripped) {
        let family = match cap[1].to_ascii_lowercase().as_str() {
            "opus" => "Opus",
            "sonnet" => "Sonnet",
            "fable" => "Fable",
            "haiku" => "Haiku",
            _ => &cap[1],
        };
        format!("{family} {}.{}", &cap[2], &cap[3])
    } else {
        stripped.to_string()
    };
    cap_chars(&formatted, 128)
}

enum UserCleanOutcome {
    Drop { interrupted: bool },
    Keep { text: String, quoted_id: Option<String> },
}

fn clean_claude_user_text(raw: &str) -> UserCleanOutcome {
    static PASTED_LINE_RE: OnceLock<Regex> = OnceLock::new();
    static PASTED_INLINE_RE: OnceLock<Regex> = OnceLock::new();
    static CMD_NAME_RE: OnceLock<Regex> = OnceLock::new();
    static CMD_MSG_RE: OnceLock<Regex> = OnceLock::new();
    static CMD_ARGS_RE: OnceLock<Regex> = OnceLock::new();
    static MENTION_NOTE_RE: OnceLock<Regex> = OnceLock::new();
    static LOCAL_CMD_RE: OnceLock<Regex> = OnceLock::new();

    // 1. Strip <system-reminder>
    let mut text = strip_system_reminders(raw);

    // 2. Interrupt & task-notification drop
    let trimmed = text.trim();
    if trimmed == "[Request interrupted by user]"
        || trimmed == "[Request interrupted by user for tool use]"
    {
        return UserCleanOutcome::Drop { interrupted: true };
    }
    if text.trim_start().starts_with("<task-notification>") {
        return UserCleanOutcome::Drop { interrupted: false };
    }

    // 3. Unwrap <pasted_content>
    let lower = text.to_ascii_lowercase();
    if lower.contains("<pasted_content") || lower.contains("</pasted_content") {
        let line_re = PASTED_LINE_RE.get_or_init(|| {
            Regex::new(r"(?im)^[ \t]*</?pasted_content\b[^>]*>[ \t]*\r?\n?")
                .expect("valid pasted line regex")
        });
        let inline_re = PASTED_INLINE_RE.get_or_init(|| {
            Regex::new(r"(?i)</?pasted_content\b[^>]*>").expect("valid pasted inline regex")
        });
        let s1 = line_re.replace_all(&text, "");
        text = inline_re.replace_all(&s1, " ").trim().to_string();
    }

    // 4. Convert <command-name> / <command-message> + <command-args>
    let cmd_match = if text.contains("<command-name>") {
        let re = CMD_NAME_RE.get_or_init(|| {
            Regex::new(r"(?s)<command-name>\s*(.*?)\s*</command-name>")
                .expect("valid command-name regex")
        });
        re.captures(&text).and_then(|c| c.get(1)).map(|m| m.as_str().trim().to_string())
    } else if text.contains("<command-message>") {
        let re = CMD_MSG_RE.get_or_init(|| {
            Regex::new(r"(?s)<command-message>\s*(.*?)\s*</command-message>")
                .expect("valid command-message regex")
        });
        re.captures(&text).and_then(|c| c.get(1)).map(|m| m.as_str().trim().to_string())
    } else {
        None
    };

    if let Some(mut cmd) = cmd_match
        && !cmd.is_empty()
    {
        if !cmd.starts_with('/') {
            cmd.insert(0, '/');
        }
        let args_re = CMD_ARGS_RE.get_or_init(|| {
            Regex::new(r"(?s)<command-args>(.*?)</command-args>")
                .expect("valid command-args regex")
        });
        let mention_re = MENTION_NOTE_RE.get_or_init(|| {
            Regex::new(
                r"(?s)\n\n\((?:This line is addressed to .*? or relay it unless you are also asked|Valentin wrote this to you\. It also mentions .*? or relay it)\.\)\s*$",
            )
            .expect("valid mention note regex")
        });
        let raw_args = args_re
            .captures(&text)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str())
            .unwrap_or("");
        let clean_args = mention_re.replace(raw_args, "");
        text = command_reply(&cmd, clean_args.trim());
    }

    // 5. Strip <local-command-stdout|stderr|caveat> (Rust-regex safe alternation, NO \1)
    if text.contains("<local-command-") {
        let local_re = LOCAL_CMD_RE.get_or_init(|| {
            Regex::new(
                r"(?s)<local-command-stdout>.*?</local-command-stdout>|<local-command-stderr>.*?</local-command-stderr>|<local-command-caveat>.*?</local-command-caveat>",
            )
            .expect("valid local-command regex")
        });
        text = local_re.replace_all(&text, "").into_owned();
    }

    // 6. Strip tracking suffix \n[mp:<16hex>] and scrub inline base64 images
    let (cleaned, quoted_id) = strip_tracking_suffix(&text);
    let final_text = strip_base64_in_str(&cleaned).trim().to_string();

    if final_text.is_empty() {
        UserCleanOutcome::Drop { interrupted: false }
    } else {
        UserCleanOutcome::Keep {
            text: final_text,
            quoted_id,
        }
    }
}

fn command_reply(cmd: &str, args: &str) -> String {
    if args.is_empty() {
        return cmd.to_string();
    }
    let lines: Vec<&str> = args.lines().collect();
    let mut quote_end = 0usize;
    let mut i = 0usize;
    while i < lines.len() {
        if lines[i].starts_with('>') {
            quote_end = i + 1;
            i += 1;
        } else if lines[i].trim().is_empty() && quote_end > 0 {
            // Check if another quote line follows this blank line
            if lines[i + 1..].iter().any(|l| l.starts_with('>'))
                && lines[i + 1..]
                    .iter()
                    .find(|l| !l.trim().is_empty())
                    .is_some_and(|l| l.starts_with('>'))
            {
                i += 1;
            } else {
                break;
            }
        } else {
            break;
        }
    }

    if quote_end > 0 {
        let quote = lines[..quote_end].join("\n").trim_end().to_string();
        let own = lines[quote_end..].join("\n").trim().to_string();
        if !own.is_empty() {
            let sep = if own.contains('\n') { "\n" } else { " " };
            return format!("{quote}\n\n{cmd}{sep}{own}");
        }
    }
    format!("{cmd} {args}").trim().to_string()
}

fn extract_claude_input_diff(name: &str, input: &Value) -> Option<DiffBlock> {
    let kind = name.rsplit('.').next().unwrap_or(name).to_ascii_lowercase();
    let path = input
        .get("file_path")
        .or_else(|| input.get("path"))
        .and_then(Value::as_str)
        .map(String::from);

    match kind.as_str() {
        "write" => {
            let content = input.get("content").and_then(Value::as_str)?;
            let lines: Vec<String> = content.lines().map(|l| format!("+{l}")).collect();
            build_diff_block(path, lines)
        }
        "edit" => {
            let old = input.get("old_string").and_then(Value::as_str).unwrap_or("");
            let new = input.get("new_string").and_then(Value::as_str).unwrap_or("");
            let lines = compute_unified_diff_lines(old, new, 1);
            build_diff_block(path, lines)
        }
        "multiedit" => {
            let edits = input.get("edits").and_then(Value::as_array)?;
            let mut all_lines = Vec::new();
            for edit in edits {
                let old = edit.get("old_string").and_then(Value::as_str).unwrap_or("");
                let new = edit.get("new_string").and_then(Value::as_str).unwrap_or("");
                all_lines.extend(compute_unified_diff_lines(old, new, 1));
            }
            build_diff_block(path, all_lines)
        }
        _ => None,
    }
}

fn extract_claude_result_diff(event: &Value) -> Option<DiffBlock> {
    let result = event.get("toolUseResult")?.as_object()?;
    let path = result
        .get("filePath")
        .or_else(|| result.get("file_path"))
        .and_then(Value::as_str)
        .map(String::from);

    let mut lines = Vec::new();
    if let Some(patches) = result.get("structuredPatch").and_then(Value::as_array) {
        for hunk in patches {
            if let Some(hunk_lines) = hunk.get("lines").and_then(Value::as_array) {
                for l in hunk_lines.iter().filter_map(Value::as_str) {
                    if l.starts_with('+') || l.starts_with('-') || l.starts_with(' ') {
                        lines.push(l.to_string());
                    }
                }
            }
        }
    }

    if lines.is_empty()
        && result.get("type").and_then(Value::as_str) == Some("create")
        && let Some(content) = result.get("content").and_then(Value::as_str)
    {
        lines = content.lines().map(|l| format!("+{l}")).collect();
    }

    build_diff_block(path, lines)
}

struct OpenToolRef {
    msg_idx: usize,
    block_idx: usize,
    raw_input_detail: String,
    is_task_create: bool,
    task_internal_id: Option<String>,
}

struct PendingQuestionRef {
    call_id: String,
    msg_idx: usize,
    block_idx: usize,
    questions: Vec<PromptQuestion>,
}

struct TaskEntry {
    id: String,
    content: String,
    status: TodoStatus,
}

impl AgentProvider for ClaudeProvider {
    fn kind(&self) -> AgentKind {
        AgentKind::Claude
    }

    fn discover_paths(
        &self,
        home: &Path,
        limit: usize,
        keep: &dyn Fn(&Path, i64) -> bool,
    ) -> Vec<(PathBuf, i64)> {
        let mut files = Vec::new();
        for root in claude_discovery_roots(home) {
            for entry in WalkDir::new(&root).max_depth(3).into_iter().flatten() {
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
                    if keep(path, ts) && is_safe_transcript_path(path, &root) {
                        files.push((path.to_path_buf(), ts));
                    }
                }
            }
        }
        files.sort_by(|a, b| b.1.cmp(&a.1));
        files.truncate(limit);
        files
    }

    fn extract_header_meta(&self, path: &Path) -> HeaderMeta {
        let mut meta = HeaderMeta::default();
        let mut custom_title_found = false;

        for line in read_header_lines_bounded(path, 32) {
            let Ok(val) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if is_filtered_claude_entry(&val) {
                continue;
            }
            if meta.cwd.is_none() {
                meta.cwd = val
                    .get("cwd")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .map(String::from);
            }
            if meta.model.is_none()
                && let Some(m) = val
                    .get("message")
                    .and_then(|m| m.get("model"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
            {
                meta.model = Some(format_claude_model(m));
            }
            if let Some(custom) = val
                .get("customTitle")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                meta.title_hint = Some(truncate_title(custom, 64));
                custom_title_found = true;
            }
            if !custom_title_found
                && meta.title_hint.is_none()
                && val.get("type").and_then(Value::as_str) == Some("user")
                && let Some(content) = val
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .or_else(|| val.get("content"))
            {
                let raw_text = extract_text_content(content);
                if let UserCleanOutcome::Keep { text, .. } = clean_claude_user_text(&raw_text)
                    && let Some(first) = text.lines().map(str::trim).find(|l| !l.is_empty())
                {
                    meta.title_hint = Some(truncate_title(first, 64));
                }
            }
        }
        meta
    }

    fn parse_lines(&self, lines: &[LineSlice<'_>]) -> ParsedSlice {
        self.parse_records(lines, Some)
    }
}

impl ClaudeProvider {
    pub(super) fn parse_records(
        &self,
        lines: &[LineSlice<'_>],
        mut normalize: impl FnMut(Value) -> Option<Value>,
    ) -> ParsedSlice {
        let mut parsed = ParsedSlice::default();
        let mut open_tools: HashMap<String, OpenToolRef> = HashMap::new();
        let mut completed_tool_ids: HashSet<String> = HashSet::new();
        let mut pending_questions: Vec<PendingQuestionRef> = Vec::new();
        let mut task_entries: Vec<TaskEntry> = Vec::new();
        let mut next_task_num: usize = 1;

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
            let Some(val) = normalize(val) else {
                continue;
            };
            if is_filtered_claude_entry(&val) {
                continue;
            }

            if let Some(cwd) = val
                .get("cwd")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
            {
                parsed.cwd = Some(cwd.to_string());
            }
            if let Some(custom) = val
                .get("customTitle")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                parsed.title_hint = Some(truncate_title(custom, 64));
            }

            let Some(kind) = val.get("type").and_then(Value::as_str) else {
                continue;
            };
            let timestamp_ms = val
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_iso_ms);
            let msg_id = val
                .get("uuid")
                .and_then(Value::as_str)
                .or_else(|| {
                    val.get("message")
                        .and_then(|m| m.get("id"))
                        .and_then(Value::as_str)
                })
                .map(String::from)
                .unwrap_or_else(|| format!("claude-{}", line.start_offset));

            match kind {
                "custom-title" => {
                    // Already captured customTitle above.
                }
                "attachment" => {
                    if val
                        .get("attachment")
                        .and_then(|a| a.get("type"))
                        .and_then(Value::as_str)
                        == Some("queued_command")
                        && let Some(prompt) = val
                            .get("attachment")
                            .and_then(|a| a.get("prompt"))
                            .and_then(Value::as_str)
                    {
                        match clean_claude_user_text(prompt) {
                            UserCleanOutcome::Drop { interrupted } => {
                                if interrupted {
                                    parsed.tail_activity = TailActivity::Idle;
                                }
                            }
                            UserCleanOutcome::Keep { text, quoted_id } => {
                                let mut blocks = vec![MessageBlock::Text { text: text.clone() }];
                                blocks.extend(extract_image_blocks(&text));
                                parsed.messages.push(ChatMessage {
                                    id: msg_id,
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
                    }
                }
                "user" => {
                    let msg_obj = val.get("message").unwrap_or(&val);
                    let content = msg_obj.get("content").unwrap_or(&Value::Null);
                    let mut user_text_parts = Vec::new();
                    let mut user_image_blocks = Vec::new();
                    let mut had_tool_result = false;

                    match content {
                        Value::String(s) => user_text_parts.push(s.clone()),
                        Value::Array(parts) => {
                            for part in parts {
                                let ptype = part.get("type").and_then(Value::as_str).unwrap_or("");
                                match ptype {
                                    "tool_result" => {
                                        had_tool_result = true;
                                        let Some(tool_use_id) = part
                                            .get("tool_use_id")
                                            .and_then(Value::as_str)
                                            .filter(|s| !s.is_empty())
                                        else {
                                            continue;
                                        };
                                        completed_tool_ids.insert(tool_use_id.to_string());
                                        let output_raw =
                                            part.get("content").unwrap_or(&Value::Null);
                                        let output_text = cap_chars(
                                            &tool_text(output_raw),
                                            MAX_TOOL_DETAIL_CHARS,
                                        );
                                        let is_error = part
                                            .get("is_error")
                                            .and_then(Value::as_bool)
                                            == Some(true);
                                        let status = if is_error {
                                            ToolStatus::Error
                                        } else {
                                            ToolStatus::Completed
                                        };
                                        let result_diff = extract_claude_result_diff(&val);

                                        if let Some(tref) = open_tools.remove(tool_use_id) {
                                            if tref.is_task_create
                                                && let Some(ref internal_id) =
                                                    tref.task_internal_id
                                                && let Some(returned_id) =
                                                    extract_task_id_from_output(&output_text)
                                                && let Some(entry) = task_entries
                                                    .iter_mut()
                                                    .find(|t| t.id == *internal_id)
                                            {
                                                entry.id = returned_id;
                                            }
                                            if let Some(msg) = parsed.messages.get_mut(tref.msg_idx)
                                                && let Some(MessageBlock::ToolUse {
                                                    name: blk_name,
                                                    summary,
                                                    detail,
                                                    status: blk_status,
                                                    diff,
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
                                                *detail = combine_tool_detail(
                                                    &tref.raw_input_detail,
                                                    &output_text,
                                                );
                                                let kind = blk_name
                                                    .rsplit('.')
                                                    .next()
                                                    .unwrap_or(blk_name)
                                                    .to_ascii_lowercase();
                                                if matches!(
                                                    kind.as_str(),
                                                    "edit"
                                                        | "multiedit"
                                                        | "write"
                                                        | "notebookedit"
                                                        | "edit_file"
                                                        | "write_file"
                                                ) && let Some(rd) = result_diff
                                                {
                                                    *summary =
                                                        append_diff_counts_to_summary(summary, &rd);
                                                    *diff = Some(rd);
                                                }
                                            }
                                        } else {
                                            // Orphan tool_result from an earlier page/delta
                                            parsed.messages.push(ChatMessage {
                                                id: format!("tool-result-{tool_use_id}"),
                                                role: MessageRole::Tool,
                                                text: String::new(),
                                                timestamp_ms,
                                                blocks: vec![MessageBlock::ToolUse {
                                                    call_id: tool_use_id.to_string(),
                                                    name: String::new(),
                                                    summary: String::new(),
                                                    detail: output_text.clone(),
                                                    status,
                                                    diff: result_diff,
                                                    output: if output_text.is_empty() {
                                                        None
                                                    } else {
                                                        Some(output_text)
                                                    },
                                                }],
                                                source_byte_range: Some((
                                                    line.start_offset,
                                                    line.end_offset,
                                                )),
                                                ..Default::default()
                                            });
                                        }
                                    }
                                    "text" | "input_text" | "output_text" => {
                                        if let Some(t) = part.get("text").and_then(Value::as_str) {
                                            user_text_parts.push(t.to_string());
                                        }
                                    }
                                    "image" => match extract_image_part(part) {
                                        ClaudeImagePart::Block(img) => user_image_blocks.push(img),
                                        ClaudeImagePart::OmittedBase64 => {
                                            user_text_parts.push("[image omitted]".to_string());
                                        }
                                        ClaudeImagePart::None => {}
                                    },
                                    _ => {
                                        if let Some(s) = part.as_str() {
                                            user_text_parts.push(s.to_string());
                                        }
                                    }
                                }
                            }
                        }
                        Value::Object(map) => {
                            if let Some(t) = map.get("text").and_then(Value::as_str) {
                                user_text_parts.push(t.to_string());
                            }
                        }
                        _ => {}
                    }

                    let joined_user = user_text_parts.join("\n");
                    if !joined_user.is_empty() || !user_image_blocks.is_empty() {
                        match clean_claude_user_text(&joined_user) {
                            UserCleanOutcome::Drop { interrupted } => {
                                if interrupted {
                                    parsed.tail_activity = TailActivity::Idle;
                                } else if !user_image_blocks.is_empty() {
                                    parsed.messages.push(ChatMessage {
                                        id: msg_id,
                                        role: MessageRole::User,
                                        text: String::new(),
                                        timestamp_ms,
                                        blocks: user_image_blocks,
                                        source_byte_range: Some((
                                            line.start_offset,
                                            line.end_offset,
                                        )),
                                        ..Default::default()
                                    });
                                    parsed.tail_activity = TailActivity::Working;
                                }
                            }
                            UserCleanOutcome::Keep { text, quoted_id } => {
                                let mut blocks = vec![MessageBlock::Text { text: text.clone() }];
                                blocks.extend(user_image_blocks);
                                blocks.extend(extract_image_blocks(&text));
                                parsed.messages.push(ChatMessage {
                                    id: msg_id,
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
                    } else if had_tool_result {
                        // Tool results fed back to assistant -> model is working on next step
                        parsed.tail_activity = TailActivity::Working;
                    }
                }
                "assistant" => {
                    let msg_obj = val.get("message").unwrap_or(&val);
                    if let Some(m) = msg_obj
                        .get("model")
                        .and_then(Value::as_str)
                        .filter(|s| !s.trim().is_empty())
                    {
                        parsed.model = Some(format_claude_model(m));
                    }

                    let content = msg_obj.get("content").unwrap_or(&Value::Null);
                    let mut blocks: Vec<MessageBlock> = Vec::new();
                    let mut text_segments: Vec<String> = Vec::new();
                    let mut current_text = String::new();
                    let mut msg_todos: Vec<TodoItem> = Vec::new();
                    let mut had_tool_use = false;
                    let msg_idx = parsed.messages.len();

                    let flush_text = |current: &mut String,
                                      blocks: &mut Vec<MessageBlock>,
                                      segments: &mut Vec<String>| {
                        if current.is_empty() {
                            return;
                        }
                        let cleaned = strip_base64_in_str(&strip_system_reminders(current))
                            .trim()
                            .to_string();
                        current.clear();
                        if !cleaned.is_empty() {
                            let imgs = extract_image_blocks(&cleaned);
                            blocks.push(MessageBlock::Text {
                                text: cleaned.clone(),
                            });
                            blocks.extend(imgs);
                            segments.push(cleaned);
                        }
                    };

                    match content {
                        Value::String(s) => {
                            current_text.push_str(s);
                            flush_text(&mut current_text, &mut blocks, &mut text_segments);
                        }
                        Value::Array(parts) => {
                            for part in parts {
                                let ptype = part.get("type").and_then(Value::as_str).unwrap_or("");
                                match ptype {
                                    "text" | "input_text" | "output_text" => {
                                        if let Some(t) = part.get("text").and_then(Value::as_str) {
                                            if !current_text.is_empty() {
                                                current_text.push('\n');
                                            }
                                            current_text.push_str(t);
                                        }
                                    }
                                    "thinking" | "redacted_thinking" => {
                                        // Intentionally omitted from visible chat transcript
                                    }
                                    "image" => match extract_image_part(part) {
                                        ClaudeImagePart::Block(img) => {
                                            flush_text(
                                                &mut current_text,
                                                &mut blocks,
                                                &mut text_segments,
                                            );
                                            blocks.push(img);
                                        }
                                        ClaudeImagePart::OmittedBase64 => {
                                            if !current_text.is_empty() {
                                                current_text.push('\n');
                                            }
                                            current_text.push_str("[image omitted]");
                                        }
                                        ClaudeImagePart::None => {}
                                    },
                                    "tool_use" => {
                                        flush_text(
                                            &mut current_text,
                                            &mut blocks,
                                            &mut text_segments,
                                        );
                                        let Some(call_id) = part
                                            .get("id")
                                            .and_then(Value::as_str)
                                            .filter(|s| !s.is_empty())
                                        else {
                                            continue;
                                        };
                                        had_tool_use = true;
                                        let name = part
                                            .get("name")
                                            .and_then(Value::as_str)
                                            .unwrap_or("Tool");
                                        let input = part.get("input").unwrap_or(&Value::Null);

                                        let mut is_task_create = false;
                                        let mut task_internal_id = None;

                                        match name {
                                            "TodoWrite" => {
                                                if let Some(todos_arr) =
                                                    input.get("todos").and_then(Value::as_array)
                                                {
                                                    let new_todos: Vec<TodoItem> = todos_arr
                                                        .iter()
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
                                                                content: sanitize_todo_content(
                                                                    content,
                                                                ),
                                                                status,
                                                            })
                                                        })
                                                        .collect();
                                                    parsed.todos = new_todos.clone();
                                                    msg_todos = new_todos;
                                                }
                                            }
                                            "TaskCreate" => {
                                                if let Some(subj) = input
                                                    .get("subject")
                                                    .or_else(|| input.get("description"))
                                                    .and_then(Value::as_str)
                                                    .map(str::trim)
                                                    .filter(|s| !s.is_empty())
                                                {
                                                    let tid = next_task_num.to_string();
                                                    next_task_num += 1;
                                                    if task_entries.len() < MAX_TODOS {
                                                        task_entries.push(TaskEntry {
                                                            id: tid.clone(),
                                                            content: sanitize_todo_content(subj),
                                                            status: TodoStatus::Pending,
                                                        });
                                                    }
                                                    is_task_create = true;
                                                    task_internal_id = Some(tid);
                                                    parsed.todos = task_entries_to_todos(&task_entries);
                                                    msg_todos = parsed.todos.clone();
                                                }
                                            }
                                            "TaskUpdate" => {
                                                if let Some(tid) = input
                                                    .get("taskId")
                                                    .and_then(|v| {
                                                        v.as_str()
                                                            .map(String::from)
                                                            .or_else(|| v.as_u64().map(|n| n.to_string()))
                                                    })
                                                {
                                                    let status_str = input
                                                        .get("status")
                                                        .and_then(Value::as_str)
                                                        .unwrap_or("");
                                                    if status_str.eq_ignore_ascii_case("deleted") {
                                                        task_entries.retain(|t| t.id != tid);
                                                    } else if let Some(entry) =
                                                        task_entries.iter_mut().find(|t| t.id == tid)
                                                    {
                                                        if !status_str.is_empty() {
                                                            entry.status =
                                                                parse_todo_status(status_str);
                                                        }
                                                        if let Some(subj) = input
                                                            .get("subject")
                                                            .and_then(Value::as_str)
                                                            .map(str::trim)
                                                            .filter(|s| !s.is_empty())
                                                        {
                                                            entry.content =
                                                                sanitize_todo_content(subj);
                                                        }
                                                    }
                                                    parsed.todos = task_entries_to_todos(&task_entries);
                                                    msg_todos = parsed.todos.clone();
                                                }
                                            }
                                            _ => {}
                                        }

                                        let mut summary = format_tool_summary(name, input);
                                        let raw_detail = tool_text(input);
                                        let detail = cap_chars(&raw_detail, MAX_TOOL_DETAIL_CHARS);
                                        let diff = extract_claude_input_diff(name, input);
                                        if let Some(ref d) = diff {
                                            summary = append_diff_counts_to_summary(&summary, d);
                                        }

                                        let block_idx = blocks.len();
                                        blocks.push(MessageBlock::ToolUse {
                                            call_id: call_id.to_string(),
                                            name: name.to_string(),
                                            summary,
                                            detail,
                                            status: ToolStatus::Running,
                                            diff,
                                            output: None,
                                        });

                                        if name == "AskUserQuestion" {
                                            let qs = input
                                                .get("questions")
                                                .map(parse_prompt_questions)
                                                .unwrap_or_default();
                                            if !qs.is_empty() {
                                                pending_questions.push(PendingQuestionRef {
                                                    call_id: call_id.to_string(),
                                                    msg_idx,
                                                    block_idx,
                                                    questions: qs,
                                                });
                                            }
                                        }

                                        open_tools.insert(
                                            call_id.to_string(),
                                            OpenToolRef {
                                                msg_idx,
                                                block_idx,
                                                raw_input_detail: raw_detail,
                                                is_task_create,
                                                task_internal_id,
                                            },
                                        );
                                    }
                                    _ => {
                                        if let Some(s) = part.as_str() {
                                            if !current_text.is_empty() {
                                                current_text.push('\n');
                                            }
                                            current_text.push_str(s);
                                        }
                                    }
                                }
                            }
                            flush_text(&mut current_text, &mut blocks, &mut text_segments);
                        }
                        _ => {}
                    }

                    if !blocks.is_empty() {
                        let text = text_segments.join("\n\n");
                        parsed.messages.push(ChatMessage {
                            id: msg_id,
                            role: MessageRole::Assistant,
                            text,
                            timestamp_ms,
                            blocks,
                            todos: msg_todos,
                            source_byte_range: Some((line.start_offset, line.end_offset)),
                            quoted_message_id: None,
                        });
                        parsed.tail_activity = if had_tool_use {
                            TailActivity::Working
                        } else {
                            TailActivity::Idle
                        };
                    }
                }
                _ => {
                    // Ignore system, progress, summary, file-history-snapshot, queue-operation
                }
            }
        }

        // Emit QuestionPrompt ONLY for the latest uncompleted AskUserQuestion when it belongs
        // to the latest message and the turn was not interrupted.
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
                    provider: "claude".to_string(),
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

fn is_filtered_claude_entry(val: &Value) -> bool {
    val.get("isSidechain").and_then(Value::as_bool) == Some(true)
        || val.get("isMeta").and_then(Value::as_bool) == Some(true)
        || val.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
        || val.get("isVisibleInTranscriptOnly").and_then(Value::as_bool) == Some(true)
}

enum ClaudeImagePart {
    Block(MessageBlock),
    OmittedBase64,
    None,
}

fn is_omitted_or_data_image_url(u: &str) -> bool {
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

fn extract_image_part(part: &Value) -> ClaudeImagePart {
    let source = part.get("source").unwrap_or(part);
    if source.get("type").and_then(Value::as_str) == Some("base64") {
        return ClaudeImagePart::OmittedBase64;
    }
    let raw_url = source
        .get("url")
        .or_else(|| part.get("url"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(u) = raw_url
        && is_omitted_or_data_image_url(u)
    {
        return ClaudeImagePart::OmittedBase64;
    }
    let url = raw_url.map(String::from);
    let raw_local = source
        .get("path")
        .or_else(|| source.get("local_path"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(p) = raw_local
        && is_omitted_or_data_image_url(p)
    {
        return ClaudeImagePart::OmittedBase64;
    }
    let local_path = raw_local.map(String::from);
    if url.is_none() && local_path.is_none() {
        if source.get("data").is_some() || part.get("data").is_some() {
            return ClaudeImagePart::OmittedBase64;
        }
        return ClaudeImagePart::None;
    }
    let alt = part
        .get("alt")
        .and_then(Value::as_str)
        .map(strip_base64_in_str)
        .unwrap_or_default();
    ClaudeImagePart::Block(MessageBlock::Image {
        url,
        local_path,
        alt,
    })
}

fn task_entries_to_todos(entries: &[TaskEntry]) -> Vec<TodoItem> {
    entries
        .iter()
        .take(MAX_TODOS)
        .map(|t| TodoItem {
            content: t.content.clone(),
            status: t.status,
        })
        .collect()
}

fn extract_task_id_from_output(output: &str) -> Option<String> {
    static TASK_ID_RE: OnceLock<Regex> = OnceLock::new();
    let re = TASK_ID_RE.get_or_init(|| {
        Regex::new(r"(?i)task\s*#?(\d+)").expect("valid task id regex")
    });
    re.captures(output)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{discover_all_sessions, read_messages_from_file};
    use serde_json::json;

    #[test]
    fn claude_filters_sidechain_meta_compact_and_cleans_user_tags() {
        let lines = vec![
            json!({"type":"user","isSidechain":true,"uuid":"skip1","message":{"content":"side"}}),
            json!({"type":"user","isMeta":true,"uuid":"skip2","message":{"content":"meta"}}),
            json!({"type":"user","isCompactSummary":true,"uuid":"skip3","message":{"content":"compact"}}),
            json!({"type":"user","isVisibleInTranscriptOnly":true,"uuid":"skip4","message":{"content":"vis"}}),
            json!({"type":"user","uuid":"u1","timestamp":"2026-11-26T10:00:00Z","message":{"content":"<system-reminder>hidden</system-reminder>\n<pasted_content>hello world</pasted_content><local-command-stdout>noise</local-command-stdout><local-command-stderr>err</local-command-stderr><local-command-caveat>cav</local-command-caveat>\n[mp:0123456789abcdef]"}}),
            json!({"type":"user","uuid":"u2","timestamp":"2026-11-26T10:00:01Z","message":{"content":"<command-name>review</command-name><command-args>> quoted line\n\ncheck this</command-args>"}}),
            json!({"type":"user","uuid":"u3","message":{"content":"<task-notification>bg done</task-notification>"}}),
            json!({"type":"attachment","uuid":"u4","attachment":{"type":"queued_command","prompt":"follow up"}}),
            json!({"type":"user","uuid":"u5","message":{"content":"[Request interrupted by user]"}}),
        ];
        let payload: String = lines.into_iter().map(|v| format!("{v}\n")).collect();
        let parsed = ClaudeProvider.parse_slice(payload.as_bytes(), 0);

        assert_eq!(parsed.messages.len(), 3);
        assert_eq!(parsed.messages[0].id, "u1");
        assert_eq!(parsed.messages[0].text, "hello world");
        assert_eq!(
            parsed.messages[0].quoted_message_id.as_deref(),
            Some("0123456789abcdef")
        );
        assert_eq!(parsed.messages[1].id, "u2");
        assert_eq!(parsed.messages[1].text, "> quoted line\n\n/review check this");
        assert_eq!(parsed.messages[2].id, "u4");
        assert_eq!(parsed.messages[2].text, "follow up");
        // Final line was interrupt marker -> tail_activity must be Idle!
        assert_eq!(parsed.tail_activity, TailActivity::Idle);
    }

    #[test]
    fn claude_interleaves_tools_pairs_results_diffs_todos_questions_and_model() {
        let long_output = "log line 0123456789\n".repeat(300);
        let long_b64 = "B".repeat(300);
        let lines = vec![
            json!({
                "type": "assistant",
                "uuid": "a1",
                "timestamp": "2026-11-26T10:00:02Z",
                "message": {
                    "model": "claude-opus-4-6",
                    "content": [
                        {"type": "thinking", "thinking": "secret chain of thought"},
                        {"type": "text", "text": "Inspecting file https://example.com/screen.png"},
                        {
                            "type": "tool_use",
                            "id": "call_edit",
                            "name": "Edit",
                            "input": {
                                "file_path": "/repo/src/main.rs",
                                "old_string": "fn old() {}",
                                "new_string": "fn new() {}\nfn extra() {}"
                            }
                        },
                        {
                            "type": "tool_use",
                            "id": "call_todo",
                            "name": "TodoWrite",
                            "input": {
                                "todos": [
                                    {"content": "Build parser", "status": "completed"},
                                    {"content": "Write tests", "status": "in_progress"}
                                ]
                            }
                        },
                        {
                            "type": "tool_use",
                            "id": "call_bash",
                            "name": "Bash",
                            "input": {"command": "cargo test", "description": "Run tests"}
                        },
                        {
                            "type": "tool_use",
                            "id": "call_q_done",
                            "name": "AskUserQuestion",
                            "input": {
                                "questions": [{
                                    "question": "Old question?",
                                    "header": "Old",
                                    "options": [{"label": "Yes", "description": "Proceed"}]
                                }]
                            }
                        },
                        {
                            "type": "tool_use",
                            "id": "call_q_open",
                            "name": "AskUserQuestion",
                            "input": {
                                "questions": [{
                                    "question": "Which mode?",
                                    "header": "Mode",
                                    "multiSelect": true,
                                    "options": [
                                        {"label": "Fast", "description": "Quick"},
                                        {"label": "Thorough", "description": "Deep"}
                                    ]
                                }]
                            }
                        }
                    ]
                }
            }),
            json!({
                "type": "user",
                "uuid": "u_tr",
                "toolUseResult": {
                    "filePath": "/repo/src/main.rs",
                    "structuredPatch": [{"lines": ["-fn old() {}", "+fn new() {}", "+fn extra() {}"]}]
                },
                "message": {
                    "content": [
                        {"type": "tool_result", "tool_use_id": "call_edit", "content": format!("ok data:image/png;base64,{long_b64}")},
                        {"type": "tool_result", "tool_use_id": "call_todo", "content": "saved"},
                        {"type": "tool_result", "tool_use_id": "call_bash", "is_error": true, "content": long_output},
                        {"type": "tool_result", "tool_use_id": "call_q_done", "content": "Yes"}
                    ]
                }
            }),
        ];
        let payload: String = lines.into_iter().map(|v| format!("{v}\n")).collect();
        let parsed = ClaudeProvider.parse_slice(payload.as_bytes(), 0);

        // Tool-result-only user entry must NOT produce an empty User message
        assert_eq!(parsed.messages.len(), 1);
        assert_eq!(parsed.model.as_deref(), Some("Opus 4.6"));
        assert_eq!(parsed.todos.len(), 2);
        assert_eq!(parsed.todos[0].status, TodoStatus::Completed);
        assert_eq!(parsed.todos[1].status, TodoStatus::InProgress);
        assert!(parsed.has_pending_question);
        assert_eq!(parsed.tail_activity, TailActivity::AwaitingUserInput);

        let msg = &parsed.messages[0];
        assert!(!msg.text.contains("secret chain"));
        assert!(msg.blocks.iter().any(|b| matches!(b, MessageBlock::Image { url: Some(u), .. } if u == "https://example.com/screen.png")));

        // Verify Edit tool_use paired with Completed, diff, summary +2 -1, base64 scrubbed
        let edit_blk = msg.blocks.iter().find_map(|b| match b {
            MessageBlock::ToolUse { call_id, summary, status, diff, output, .. } if call_id == "call_edit" => {
                Some((summary, status, diff, output))
            }
            _ => None,
        }).unwrap();
        assert_eq!(edit_blk.0, "Edited main.rs +2 -1");
        assert_eq!(*edit_blk.1, ToolStatus::Completed);
        assert_eq!(edit_blk.2.as_ref().unwrap().added, 2);
        assert_eq!(edit_blk.2.as_ref().unwrap().removed, 1);
        assert!(edit_blk.3.as_deref().unwrap().contains("[image omitted]"));

        // Verify Bash error & 4000-char cap
        let bash_blk = msg.blocks.iter().find_map(|b| match b {
            MessageBlock::ToolUse { call_id, summary, status, detail, .. } if call_id == "call_bash" => {
                Some((summary, status, detail))
            }
            _ => None,
        }).unwrap();
        assert_eq!(bash_blk.0, "Ran Run tests");
        assert_eq!(*bash_blk.1, ToolStatus::Error);
        assert_eq!(bash_blk.2.chars().count(), MAX_TOOL_DETAIL_CHARS);
        assert!(!bash_blk.2.contains("[image omitted]"));

        // Verify ONLY call_q_open emitted as QuestionPrompt (call_q_done was answered)
        let q_prompts: Vec<_> = msg.blocks.iter().filter_map(|b| match b {
            MessageBlock::QuestionPrompt { prompt_id, provider, questions } => Some((prompt_id, provider, questions)),
            _ => None,
        }).collect();
        assert_eq!(q_prompts.len(), 1);
        assert_eq!(q_prompts[0].0, "call_q_open");
        assert_eq!(q_prompts[0].1, "claude");
        assert!(q_prompts[0].2[0].multi_select);
    }

    #[test]
    fn claude_task_create_and_update_and_model_formatting() {
        assert_eq!(format_claude_model("claude-sonnet-4-5-20250929"), "Sonnet 4.5");
        assert_eq!(format_claude_model("claude-haiku-3-5"), "Haiku 3.5");
        assert_eq!(sanitize_claude_project_dir("/home/user/my-app"), "-home-user-my-app");

        let lines = vec![
            json!({
                "type": "assistant", "uuid": "a1",
                "message": {"content": [{"type":"tool_use","id":"tc1","name":"TaskCreate","input":{"subject":"Fix bug"}}]}
            }),
            json!({
                "type": "user", "uuid": "u1",
                "message": {"content": [{"type":"tool_result","tool_use_id":"tc1","content":"Created task #7"}]}
            }),
            json!({
                "type": "assistant", "uuid": "a2",
                "message": {"content": [{"type":"tool_use","id":"tu1","name":"TaskUpdate","input":{"taskId":"7","status":"completed"}}]}
            }),
        ];
        let payload: String = lines.into_iter().map(|v| format!("{v}\n")).collect();
        let parsed = ClaudeProvider.parse_slice(payload.as_bytes(), 0);
        assert_eq!(parsed.todos.len(), 1);
        assert_eq!(parsed.todos[0].content, "Fix bug");
        assert_eq!(parsed.todos[0].status, TodoStatus::Completed);
    }

    #[test]
    fn parses_claude_jsonl_messages_slice1_compat() {
        let dir = std::env::temp_dir().join(format!(
            "multibotd-claude-compat-{}",
            std::process::id()
        ));
        let proj = dir.join(".claude").join("projects").join("demo");
        fs::create_dir_all(&proj).unwrap();
        let path = proj.join("abc123.jsonl");
        fs::write(
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
        assert!(!sessions[0].working);
        assert_eq!(sessions[0].last_assistant_id.as_deref(), Some("a1"));
        assert_eq!(
            sessions[0].last_assistant_preview.as_deref(),
            Some("hi from rust daemon")
        );

        let (msgs, offset) = read_messages_from_file(AgentKind::Claude, &path, 0, None).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, MessageRole::User);
        assert_eq!(msgs[0].text, "hello claude");
        assert_eq!(msgs[1].role, MessageRole::Assistant);
        assert_eq!(msgs[1].text, "hi from rust daemon");
        assert!(offset > 0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn claude_scrubs_inline_60kb_base64_in_user_and_assistant_text() {
        let blob = "A".repeat(60_000);
        let lines = vec![
            json!({
                "type": "user",
                "uuid": "u_b64",
                "message": {"content": format!("Check screenshot: data:image/png;base64,{blob} done")}
            }),
            json!({
                "type": "assistant",
                "uuid": "a_b64",
                "message": {"content": [{"type": "text", "text": format!("Here is image: data:image/png;base64,{blob} done")}]}
            }),
        ];
        let payload: String = lines.into_iter().map(|v| format!("{v}\n")).collect();
        let parsed = ClaudeProvider.parse_slice(payload.as_bytes(), 0);
        assert_eq!(parsed.messages.len(), 2);

        assert_eq!(parsed.messages[0].text, "Check screenshot: [image omitted] done");
        assert!(parsed.messages[0].blocks.iter().any(|b| matches!(
            b,
            MessageBlock::Text { text } if text == "Check screenshot: [image omitted] done"
        )));
        assert_eq!(parsed.messages[1].text, "Here is image: [image omitted] done");
        assert!(parsed.messages[1].blocks.iter().any(|b| matches!(
            b,
            MessageBlock::Text { text } if text == "Here is image: [image omitted] done"
        )));
    }

    #[test]
    fn claude_interrupted_or_superseded_ask_user_question_is_not_pending() {
        let ask_line = json!({
            "type": "assistant",
            "uuid": "a1",
            "message": {
                "content": [{
                    "type": "tool_use",
                    "id": "q_stale",
                    "name": "AskUserQuestion",
                    "input": {
                        "questions": [{
                            "question": "Pick option?",
                            "header": "Opt",
                            "options": [{"label": "A", "description": "Alpha"}]
                        }]
                    }
                }]
            }
        });

        // Case 1: Immediately interrupted by user (no subsequent messages yet)
        let interrupted_payload = format!(
            "{ask_line}\n{}\n",
            json!({"type": "user", "uuid": "u_int", "message": {"content": "[Request interrupted by user]"}})
        );
        let p_int = ClaudeProvider.parse_slice(interrupted_payload.as_bytes(), 0);
        assert!(!p_int.has_pending_question);
        assert_eq!(p_int.tail_activity, TailActivity::Idle);
        assert!(!p_int.messages.iter().any(|m| {
            m.blocks
                .iter()
                .any(|b| matches!(b, MessageBlock::QuestionPrompt { .. }))
        }));

        // Case 2: Interrupted and superseded by subsequent user + assistant turn
        let superseded_payload = format!(
            "{interrupted_payload}{}\n{}\n",
            json!({"type": "user", "uuid": "u2", "message": {"content": "Never mind, do X instead"}}),
            json!({"type": "assistant", "uuid": "a2", "message": {"content": [{"type": "text", "text": "Done with X!"}]}})
        );
        let dir = std::env::temp_dir().join(format!(
            "multibotd-claude-stale-q-{}",
            std::process::id()
        ));
        let proj = dir.join(".claude/projects/demo");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("stale_q.jsonl"), &superseded_payload).unwrap();

        let p_sup = ClaudeProvider.parse_slice(superseded_payload.as_bytes(), 0);
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
}
