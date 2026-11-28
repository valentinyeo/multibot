use super::*;

/// Unified visible text extractor: returns `msg.text` when non-empty, otherwise
/// joins non-empty `MessageBlock::Text` blocks so structured-block messages support
/// reactions, quotes, reply, and copy identically to plain-text messages.
pub(crate) fn message_visible_text(msg: &ChatMessage) -> String {
    if !msg.text.trim().is_empty() {
        return msg.text.clone();
    }
    let mut parts = Vec::new();
    for block in &msg.blocks {
        if let MessageBlock::Text { text } = block
            && !text.trim().is_empty()
        {
            parts.push(text.as_str());
        }
    }
    if parts.is_empty() {
        msg.blocks
            .iter()
            .filter_map(|block| match block {
                MessageBlock::ToolUse {
                    name,
                    summary,
                    detail,
                    ..
                } => Some(format!("{name}\n{summary}\n{detail}")),
                MessageBlock::Image { alt, .. } => Some(alt.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        parts.join("\n")
    }
}

/// Finds the projected **visible** list index for a clicked quote reference.
pub(crate) fn find_visible_quote_target(
    messages: &[ChatMessage],
    local_reactions: &HashMap<String, Vec<ReactionEmoji>>,
    local_removals: &HashMap<String, Vec<ReactionEmoji>>,
    quoted_id: Option<&str>,
    quoted_source: &str,
    show_tools: bool,
) -> Option<usize> {
    let visible = project_visible_messages_with_overrides(
        messages,
        local_reactions,
        local_removals,
        show_tools,
    );
    quoted_id
        .and_then(|qid| visible.iter().position(|v| v.message.id == qid))
        .or_else(|| {
            visible.iter().position(|v| {
                let text = message_visible_text(v.message);
                quote_source_matches(&text, quoted_source)
            })
        })
}

pub(super) fn message_matches_find(msg: &ChatMessage, needle_lower: &str) -> bool {
    if msg.text.to_lowercase().contains(needle_lower) {
        return true;
    }
    for block in &msg.blocks {
        match block {
            MessageBlock::Text { text } => {
                if text.to_lowercase().contains(needle_lower) {
                    return true;
                }
            }
            MessageBlock::ToolUse {
                name,
                summary,
                detail,
                output,
                ..
            } => {
                if name.to_lowercase().contains(needle_lower)
                    || summary.to_lowercase().contains(needle_lower)
                    || detail.to_lowercase().contains(needle_lower)
                    || output
                        .as_deref()
                        .is_some_and(|o| o.to_lowercase().contains(needle_lower))
                {
                    return true;
                }
            }
            MessageBlock::Image { alt, url, .. } => {
                if alt.to_lowercase().contains(needle_lower)
                    || url
                        .as_deref()
                        .is_some_and(|u| u.to_lowercase().contains(needle_lower))
                {
                    return true;
                }
            }
            MessageBlock::QuestionPrompt { questions, .. } => {
                if questions.iter().any(|q| {
                    q.question.to_lowercase().contains(needle_lower)
                        || q.options
                            .iter()
                            .any(|o| o.label.to_lowercase().contains(needle_lower))
                }) {
                    return true;
                }
            }
        }
    }
    false
}

/// FNV-1a 64-bit hash used for reaction message references (`[mp:<16-hex>]`)
/// and deterministic code-block button IDs.
pub(crate) fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

pub(crate) fn reaction_reference(id: &str) -> u64 {
    if id.is_empty() {
        return 0;
    }
    fnv1a_64(id.as_bytes())
}

// ============================================================================
// 4. Quote/Reply (MUBO-457) & Compact Quote Header Parsing
// ============================================================================

pub(super) fn is_unicode_ws(ch: char) -> bool {
    matches!(
        ch as u32,
        0x09..=0x0D
            | 0x1C..=0x20
            | 0xA0
            | 0x1680
            | 0x2000..=0x200A
            | 0x2028
            | 0x2029
            | 0x202F
            | 0x205F
            | 0x3000
    )
}

pub(super) fn trim_end_unicode_ws(s: &str) -> &str {
    s.trim_end_matches(is_unicode_ws)
}

pub(super) fn trim_unicode_ws(s: &str) -> &str {
    s.trim_matches(is_unicode_ws)
}

pub(super) fn split_any_newlines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\r' {
            lines.push(&text[start..i]);
            if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                i += 2;
            } else {
                i += 1;
            }
            start = i;
        } else if bytes[i] == b'\n' {
            lines.push(&text[start..i]);
            i += 1;
            start = i;
        } else {
            i += 1;
        }
    }
    lines.push(&text[start..]);
    lines
}

/// Formats a quoted block capped at 10 lines (`> …` on overflow), stripping
/// leading/trailing blank lines and preserving inner indentation.
pub(crate) fn format_quote_block(message: &str) -> String {
    let raw_lines = split_any_newlines(message);
    let Some(first) = raw_lines
        .iter()
        .position(|l| !trim_end_unicode_ws(l).is_empty())
    else {
        return String::new();
    };
    let last = raw_lines
        .iter()
        .rposition(|l| !trim_end_unicode_ws(l).is_empty())
        .unwrap_or(first);

    let mut out_lines = Vec::new();
    for (emitted, line) in raw_lines[first..=last].iter().enumerate() {
        if emitted == MAX_QUOTE_LINES {
            out_lines.push("> …".to_string());
            break;
        }
        let trimmed = trim_end_unicode_ws(line);
        if trimmed.is_empty() {
            out_lines.push(">".to_string());
        } else {
            out_lines.push(format!("> {trimmed}"));
        }
    }
    out_lines.join("\n")
}

/// Detects a leading `/skill` command (MUBO-457) whose name consists exclusively of
/// `[A-Za-z0-9:_-]`, automatically rejecting file paths like `/tmp/foo` or `/usr/bin/env`.
pub(crate) fn extract_leading_skill_command(message: &str) -> Option<(&str, &str)> {
    let bytes = message.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'/' {
        return None;
    }
    let mut name_end = 1usize;
    while name_end < bytes.len() && !matches!(bytes[name_end], b' ' | b'\t' | b'\r' | b'\n') {
        name_end += 1;
    }
    if name_end == 1 {
        return None;
    }
    if !bytes[1..name_end]
        .iter()
        .all(|&b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b':' | b'_' | b'-'))
    {
        return None;
    }

    let first_line_end = message.find(['\r', '\n']).unwrap_or(message.len());
    let command = trim_end_unicode_ws(&message[..first_line_end]);
    let mut rest_start = first_line_end;
    while rest_start < bytes.len() && matches!(bytes[rest_start], b'\r' | b'\n') {
        rest_start += 1;
    }
    Some((command, &message[rest_start..]))
}

/// Composes a reply with an optional quoted message, hoisting leading `/skill` commands
/// above the quote block (MUBO-457) while keeping ordinary replies and file paths below.
pub(crate) fn compose_quoted_reply(quoted: Option<&str>, reply: &str) -> String {
    let Some(q) = quoted.filter(|s| !trim_end_unicode_ws(s).is_empty()) else {
        return reply.to_string();
    };
    if trim_end_unicode_ws(reply).is_empty() {
        return String::new();
    }
    let quote_block = format_quote_block(q);
    if quote_block.is_empty() {
        return reply.to_string();
    }

    if let Some((cmd, rest)) = extract_leading_skill_command(reply) {
        if trim_end_unicode_ws(rest).is_empty() {
            format!("{cmd}\n{quote_block}")
        } else {
            format!("{cmd}\n{quote_block}\n\n{rest}")
        }
    } else {
        format!("{quote_block}\n\n{reply}")
    }
}

/// Parses a message starting with leading `>` quote lines + blank line + non-empty body
/// into a compact 1-line preview, matchable source text, and body.
pub(crate) fn parse_quoted_message(text: &str) -> Option<ParsedQuotedMessage> {
    let normalized = text.replace("\r\n", "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    let mut idx = 0usize;
    let mut quote_lines: Vec<&str> = Vec::new();

    while idx < lines.len() {
        let line = lines[idx];
        if let Some(after) = line.strip_prefix('>') {
            let content = after.strip_prefix(' ').unwrap_or(after);
            quote_lines.push(content);
            idx += 1;
        } else {
            break;
        }
    }

    if quote_lines.is_empty() {
        return None;
    }

    // Must have at least one blank separator line before body.
    let mut blank_seen = false;
    while idx < lines.len() && trim_unicode_ws(lines[idx]).is_empty() {
        blank_seen = true;
        idx += 1;
    }
    if !blank_seen || idx >= lines.len() {
        return None;
    }

    let body = lines[idx..].join("\n");
    if trim_unicode_ws(&body).is_empty() {
        return None;
    }

    let preview = quote_lines
        .iter()
        .map(|l| trim_unicode_ws(l))
        .find(|l| !l.is_empty() && *l != "…")?
        .to_string();

    if quote_lines
        .last()
        .is_some_and(|l| trim_unicode_ws(l) == "…")
    {
        quote_lines.pop();
    }
    let source = quote_lines.join("\n");

    Some(ParsedQuotedMessage {
        preview,
        source,
        body,
    })
}

/// Strips a leading quoted `> ...` prefix from a rendered `MessageBlock::Text`
/// when a compact clickable quote header is already rendered above the bubble body.
pub(crate) fn strip_rendered_quote_prefix(text: &str) -> String {
    if let Some(pq) = parse_quoted_message(text) {
        return pq.body;
    }
    let normalized = text.replace("\r\n", "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    let mut idx = 0usize;
    while idx < lines.len() && lines[idx].starts_with('>') {
        idx += 1;
    }
    if idx == 0 {
        return text.to_string();
    }
    while idx < lines.len() && trim_unicode_ws(lines[idx]).is_empty() {
        idx += 1;
    }
    lines[idx..].join("\n")
}

pub(super) fn normalize_ws_collapse(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for ch in s.chars() {
        if is_unicode_ws(ch) {
            if !out.is_empty() {
                in_ws = true;
            }
        } else {
            if in_ws {
                out.push(' ');
                in_ws = false;
            }
            out.push(ch);
        }
    }
    out
}

pub(crate) fn quote_source_matches(candidate: &str, quoted_source: &str) -> bool {
    let q = normalize_ws_collapse(quoted_source.trim_end_matches('…'));
    if q.is_empty() {
        return false;
    }
    let c = normalize_ws_collapse(candidate);
    c.starts_with(&q) || c.contains(&q)
}

pub(crate) fn compose_snippet_quotes(content: &InputContent, snippets: &[QuotedSnippet]) -> String {
    let mut text = content.text().to_string();
    for span in content.tokens().iter().rev() {
        if span.token().id().starts_with(QUOTE_TOKEN_PREFIX) {
            if let Some(snippet) = snippets
                .iter()
                .find(|snippet| snippet.token_id() == span.token().id().as_ref())
            {
                let range = span.range();
                let start = text[..range.start].trim_end().len();
                let end = text.len() - text[range.end..].trim_start().len();
                let block = snippet
                    .full_text
                    .trim()
                    .lines()
                    .map(|line| format!("> {line}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                let replacement = format!(
                    "{}{block}{}",
                    if start > 0 { "\n\n" } else { "" },
                    if end < text.len() { "\n\n" } else { "" }
                );
                text.replace_range(start..end, &replacement);
            }
        } else if let Some((_, _, sid)) =
            multibot_core::workspaces::parse_session_key(span.token().id())
        {
            text.replace_range(span.range(), &format!("{}{{{sid}}}", span.token().text()));
        }
    }
    text.trim().to_string()
}

impl MultibotView {
    pub(crate) fn set_reply_quote(
        &mut self,
        message_id: String,
        full_text: String,
        cx: &mut Context<Self>,
    ) {
        let preview = normalize_ws_collapse(&full_text)
            .chars()
            .take(100)
            .collect::<String>();
        self.chat_state.reply_quote = Some(ReplyQuote {
            message_id,
            preview,
            full_text,
        });
        cx.notify();
    }

    pub(crate) fn clear_reply_quote(&mut self, cx: &mut Context<Self>) {
        self.chat_state.reply_quote = None;
        cx.notify();
    }
}
