use super::*;

// ============================================================================
// 1. Markdown Code Block Extraction & Ctrl+Shift+D Guard
// ============================================================================

pub(super) fn detect_code_fence(line: &str) -> Option<(char, usize, &str)> {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let leading_spaces = line.bytes().take_while(|&b| b == b' ').count();
    if leading_spaces > 3 {
        return None;
    }
    let rest = &line[leading_spaces..];
    let first = rest.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let count = rest.chars().take_while(|&c| c == first).count();
    if count < 3 {
        return None;
    }
    let info = &rest[count..];
    if first == '`' && info.contains('`') {
        return None;
    }
    Some((first, count, info))
}

/// Extracts all fenced code blocks (`` ``` `` and `~~~`) from Markdown, stripping
/// fence lines while preserving inner blank lines and indentation verbatim.
pub(crate) fn extract_code_blocks_from_markdown(md: &str) -> Vec<(Option<String>, String)> {
    let normalized = md.replace("\r\n", "\n");
    let mut blocks = Vec::new();
    let mut active: Option<(char, usize, Option<String>, Vec<&str>)> = None;

    for line in normalized.split('\n') {
        if let Some((fch, flen, ref mut lang, ref mut lines)) = active {
            if let Some((ch, count, info)) = detect_code_fence(line)
                && ch == fch
                && count >= flen
                && info.trim().is_empty()
            {
                let finished_lang = lang.take();
                let code = lines.join("\n");
                blocks.push((finished_lang, code));
                active = None;
                continue;
            }
            lines.push(line);
        } else if let Some((ch, count, info)) = detect_code_fence(line) {
            let lang = info
                .split_whitespace()
                .next()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            active = Some((ch, count, lang, Vec::new()));
        }
    }

    // Unclosed trailing fence (streaming safety)
    if let Some((_, _, lang, lines)) = active {
        blocks.push((lang, lines.join("\n")));
    }

    blocks
}

/// Returns the latest non-empty fenced code block across `messages` without fences,
/// or `None` if no non-empty code block exists.
pub(crate) fn extract_latest_code_block(messages: &[ChatMessage]) -> Option<String> {
    for msg in messages.iter().rev() {
        if !msg.blocks.is_empty() {
            for block in msg.blocks.iter().rev() {
                if let MessageBlock::Text { text } = block {
                    for (_, code) in extract_code_blocks_from_markdown(text).into_iter().rev() {
                        if !code.trim().is_empty() {
                            return Some(code);
                        }
                    }
                }
            }
        }
        for (_, code) in extract_code_blocks_from_markdown(&msg.text)
            .into_iter()
            .rev()
        {
            if !code.trim().is_empty() {
                return Some(code);
            }
        }
    }
    None
}

/// Strictly validates `Ctrl+Shift+D` while rejecting `Ctrl+Alt` (AltGr) and `Platform`.
pub(crate) fn is_copy_latest_code_block_modifiers(
    ctrl: bool,
    shift: bool,
    alt: bool,
    platform: bool,
    key: &str,
) -> bool {
    ctrl && shift && !alt && !platform && key.eq_ignore_ascii_case("d")
}

pub(crate) fn is_copy_latest_code_block_chord(keystroke: &Keystroke) -> bool {
    is_copy_latest_code_block_modifiers(
        keystroke.modifiers.control,
        keystroke.modifiers.shift,
        keystroke.modifiers.alt,
        keystroke.modifiers.platform,
        &keystroke.key,
    )
}

// ============================================================================
// 2. Collapsible Tool Steps, Inline Unified Diff & Todos
// ============================================================================

pub(super) fn tool_cluster_key(msg: &ChatMessage, msg_idx: usize) -> String {
    if msg.id.is_empty() {
        format!("message-{msg_idx}")
    } else {
        msg.id.clone()
    }
}

pub(super) fn tool_step_key(
    msg: &ChatMessage,
    msg_idx: usize,
    block_idx: usize,
    call_id: &str,
) -> String {
    if !call_id.is_empty() {
        call_id.to_string()
    } else {
        format!("{}:{}:{}", msg.id, msg_idx, block_idx)
    }
}

pub(crate) fn classify_diff_line(line: &str) -> DiffLineKind {
    if line.starts_with('+') && !line.starts_with("+++") {
        DiffLineKind::Added
    } else if line.starts_with('-') && !line.starts_with("---") {
        DiffLineKind::Removed
    } else {
        DiffLineKind::Context
    }
}

pub(crate) fn visible_diff_slice(diff: &DiffBlock, expanded: bool) -> (&[String], usize) {
    let limit = if expanded {
        DIFF_EXPANDED_ROWS
    } else {
        DIFF_FOLDED_ROWS
    };
    let shown_len = diff.lines.len().min(limit);
    let remaining = diff.lines.len().saturating_sub(shown_len) + diff.truncated_lines as usize;
    (&diff.lines[..shown_len], remaining)
}

pub(super) fn truncate_tool_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    out
}

pub(crate) fn effective_todos(
    messages: &[ChatMessage],
    session: Option<&SessionSummary>,
    cached: Option<&[TodoItem]>,
) -> (Vec<TodoItem>, u32, u32) {
    if let Some(msg) = messages.iter().rev().find(|m| !m.todos.is_empty()) {
        let done = msg
            .todos
            .iter()
            .filter(|t| t.status == TodoStatus::Completed)
            .count() as u32;
        let total = msg.todos.len() as u32;
        return (msg.todos.clone(), done, total);
    }
    if let Some(items) = cached
        && !items.is_empty()
    {
        let done = items
            .iter()
            .filter(|t| t.status == TodoStatus::Completed)
            .count() as u32;
        return (items.to_vec(), done, items.len() as u32);
    }
    if let Some(s) = session {
        return (Vec::new(), s.todos_done, s.todos_total);
    }
    (Vec::new(), 0, 0)
}

// ============================================================================
// 3. Interactive Option Cards (Structured Primary + Strictly Gated Fallback)
// ============================================================================

pub(crate) fn validate_question_prompt<'a>(
    prompt_id: &str,
    questions: &'a [PromptQuestion],
) -> Option<Vec<&'a PromptQuestion>> {
    if prompt_id.trim().is_empty() || questions.is_empty() || questions.len() > MAX_PROMPT_QUESTIONS
    {
        return None;
    }
    let mut valid = Vec::with_capacity(questions.len());
    for q in questions {
        if q.question.trim().is_empty()
            || q.options.is_empty()
            || q.options.len() > MAX_PROMPT_OPTIONS
            || q.options.iter().any(|o| o.label.trim().is_empty())
        {
            return None;
        }
        valid.push(q);
    }
    Some(valid)
}

pub(crate) fn format_single_option_reply(index: usize, label: &str) -> String {
    format!("{}: {}", index + 1, label.trim())
}

pub(crate) fn format_multi_option_reply(q: &PromptQuestion, mask: u32) -> Option<String> {
    let mut chosen = Vec::new();
    for (i, opt) in q.options.iter().enumerate() {
        if i < 32 && (mask & (1u32 << i)) != 0 {
            chosen.push(format!("{}: {}", i + 1, opt.label.trim()));
        }
    }
    if chosen.is_empty() {
        None
    } else {
        Some(chosen.join(", "))
    }
}

pub(super) fn parse_prose_option_line(line: &str) -> Option<(char, String)> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Strip optional bold wrapper around marker, e.g. `**A)** Title` or `**1.** Title`
    let (marker_src, rest_after_marker) = if let Some(after_stars) = trimmed.strip_prefix("**") {
        let close = after_stars.find("**")?;
        let inside = &after_stars[..close];
        let tail = after_stars[close + 2..].trim_start();
        (inside, tail)
    } else {
        let split_at = trimmed.find(|c: char| c.is_ascii_whitespace())?;
        (&trimmed[..split_at], trimmed[split_at..].trim_start())
    };

    let marker = marker_src.trim();
    if marker.len() != 2 {
        return None;
    }
    let mut chars = marker.chars();
    let key = chars.next()?;
    let punct = chars.next()?;
    if punct != ')' && punct != '.' {
        return None;
    }
    let title = rest_after_marker.trim();
    if title.is_empty() || title.chars().count() > 200 {
        return None;
    }
    if key.is_ascii_uppercase() || key.is_ascii_digit() {
        Some((key, title.to_string()))
    } else {
        None
    }
}

pub(super) fn has_numbered_choice_cue(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.ends_with('?') || trimmed.ends_with(':') {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    lower.contains("choose")
        || lower.contains("select")
        || lower.contains("which option")
        || lower.contains("prefer")
        || lower.contains("would you like")
}

/// Strictly gated trailing prose option detector:
/// - ONLY inspects the **latest** message and ONLY if `role == MessageRole::Assistant`.
/// - Yields (`None`) when a valid structured `MessageBlock::QuestionPrompt` is present.
/// - Strictly ignores fenced code blocks (`` ``` `` / `~~~`).
/// - Requires sequential `A..D` or `1..4` (count `2..=4`), rejecting 5+ item lists.
pub(crate) fn detect_trailing_prose_options(messages: &[ChatMessage]) -> Option<Vec<ProseOption>> {
    let (hidden, _) = compute_hidden_and_reaction_masks(messages);
    let last_idx = (0..messages.len()).rev().find(|&i| !hidden[i])?;
    let last = &messages[last_idx];
    if last.role != MessageRole::Assistant {
        return None;
    }
    for block in &last.blocks {
        if let MessageBlock::QuestionPrompt {
            prompt_id,
            questions,
            ..
        } = block
            && validate_question_prompt(prompt_id, questions).is_some()
        {
            return None;
        }
    }

    let combined_text = if last
        .blocks
        .iter()
        .any(|b| matches!(b, MessageBlock::Text { .. }))
    {
        last.blocks
            .iter()
            .filter_map(|b| match b {
                MessageBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        last.text.clone()
    };

    let normalized = combined_text.replace("\r\n", "\n");
    let mut tagged: Vec<(&str, bool)> = Vec::new();
    let mut fence: Option<(char, usize)> = None;

    for line in normalized.split('\n') {
        if let Some((fch, flen)) = fence {
            tagged.push((line, true));
            if let Some((ch, count, info)) = detect_code_fence(line)
                && ch == fch
                && count >= flen
                && info.trim().is_empty()
            {
                fence = None;
            }
        } else if let Some((ch, count, _)) = detect_code_fence(line) {
            tagged.push((line, true));
            fence = Some((ch, count));
        } else {
            tagged.push((line, false));
        }
    }

    while tagged
        .last()
        .is_some_and(|(l, in_f)| !*in_f && l.trim().is_empty())
    {
        tagged.pop();
    }
    let (_, last_in_fence) = *tagged.last()?;
    if last_in_fence {
        return None;
    }

    let mut matched_rev: Vec<(char, String)> = Vec::new();
    let mut idx = tagged.len();
    while idx > 0 {
        let (line, in_f) = tagged[idx - 1];
        if in_f {
            break;
        }
        if let Some(parsed) = parse_prose_option_line(line) {
            matched_rev.push(parsed);
            idx -= 1;
        } else {
            break;
        }
    }

    if matched_rev.len() < 2 || matched_rev.len() > MAX_PROMPT_OPTIONS {
        return None;
    }
    matched_rev.reverse();

    let first_key = matched_rev[0].0;
    let is_lettered = first_key == 'A';
    let is_numbered = first_key == '1';
    if !is_lettered && !is_numbered {
        return None;
    }

    for (pos, (key, _)) in matched_rev.iter().enumerate() {
        let expected = ((first_key as u8) + (pos as u8)) as char;
        if *key != expected {
            return None;
        }
    }

    // For numbered lists, require a clear question/choice cue immediately above the list.
    if is_numbered {
        let mut cue_idx = idx;
        while cue_idx > 0 && !tagged[cue_idx - 1].1 && tagged[cue_idx - 1].0.trim().is_empty() {
            cue_idx -= 1;
        }
        if cue_idx == 0 || tagged[cue_idx - 1].1 || !has_numbered_choice_cue(tagged[cue_idx - 1].0)
        {
            return None;
        }
    }

    Some(
        matched_rev
            .into_iter()
            .map(|(k, title)| ProseOption {
                label: k.to_string(),
                title,
            })
            .collect(),
    )
}
