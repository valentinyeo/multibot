use super::*;

// ============================================================================
// 5. Message Reactions & Double-Click Drag-Selection Guard
// ============================================================================

pub(super) fn truncate_reaction_quote(raw: &str) -> Option<String> {
    let normalized = normalize_ws_collapse(raw);
    if normalized.is_empty() {
        return None;
    }
    let total_utf16: usize = normalized.chars().map(char::len_utf16).sum();
    if total_utf16 <= REACTION_QUOTE_MAX_UTF16 - 1 {
        return Some(normalized);
    }

    let room = REACTION_QUOTE_MAX_UTF16 - 1; // 79 UTF-16 units + '…' = 80
    let mut units = 0usize;
    let mut last_fit_byte = 0usize;
    let mut last_space_byte = 0usize;
    let mut overflowed = false;

    for (byte_idx, ch) in normalized.char_indices() {
        let u = ch.len_utf16();
        if units + u > room {
            overflowed = true;
            break;
        }
        if ch == ' ' {
            last_space_byte = byte_idx;
        }
        units += u;
        last_fit_byte = byte_idx + ch.len_utf8();
    }

    if !overflowed {
        return Some(normalized);
    }

    let cut = if last_space_byte > 0 {
        last_space_byte
    } else {
        last_fit_byte
    };
    let mut out = normalized[..cut].trim_end_matches(' ').to_string();
    out.push('…');
    Some(out)
}

pub(crate) fn format_reaction_message(
    emoji: ReactionEmoji,
    target_id: &str,
    target_text: &str,
) -> Option<String> {
    let quote = truncate_reaction_quote(target_text)?;
    let reference = reaction_reference(target_id);
    if reference == 0 {
        Some(format!("{} to: {}", emoji.glyph(), quote))
    } else {
        Some(format!(
            "{} to: {} [mp:{:016x}]",
            emoji.glyph(),
            quote,
            reference
        ))
    }
}

pub(super) fn is_clean_reaction_quote(q: &str) -> bool {
    if q.is_empty() || q.starts_with(' ') || q.ends_with(' ') || q.contains("  ") {
        return false;
    }
    let mut utf16 = 0usize;
    for ch in q.chars() {
        if ch != ' ' && is_unicode_ws(ch) {
            return false;
        }
        utf16 += ch.len_utf16();
        if utf16 > REACTION_QUOTE_MAX_UTF16 {
            return false;
        }
    }
    true
}

/// Parses both tagged `<emoji> to: <quote> [mp:<16-lowercase-hex>]` and
/// spoken single-line `<emoji> to: <quote>` reaction messages.
/// Explicit `[mp:...]` tags must parse to a non-zero `u64` (malformed tags or
/// `[mp:0000000000000000]` are strictly rejected and never downgraded to spoken).
pub(crate) fn parse_reaction_message(text: &str) -> Option<ParsedReaction> {
    if text.contains('\n') || text.contains('\r') {
        return None;
    }
    let (emoji, after_emoji) = if let Some(rest) = text.strip_prefix("👍") {
        (ReactionEmoji::Like, rest)
    } else if let Some(rest) = text.strip_prefix("👎") {
        (ReactionEmoji::Dislike, rest)
    } else if let Some(rest) = text.strip_prefix("❤️") {
        (ReactionEmoji::Heart, rest)
    } else {
        return None;
    };

    let body = after_emoji.strip_prefix(" to: ")?;
    if let Some(tag_pos) = body.rfind(" [mp:") {
        let quote = &body[..tag_pos];
        let tag_rest = &body[tag_pos + 5..];
        let hex = tag_rest.strip_suffix(']')?;
        if hex.len() != 16 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return None;
        }
        if !is_clean_reaction_quote(quote) {
            return None;
        }
        let reference = u64::from_str_radix(hex, 16).ok()?;
        if reference == 0 {
            return None;
        }
        Some(ParsedReaction {
            emoji,
            quote: quote.to_string(),
            reference: Some(reference),
        })
    } else {
        if !is_clean_reaction_quote(body) {
            return None;
        }
        Some(ParsedReaction {
            emoji,
            quote: body.to_string(),
            reference: None,
        })
    }
}

pub(super) fn normalized_reaction_prefix_matches(
    collapsed: &str,
    quote: &str,
    needle: &str,
) -> bool {
    if !collapsed.starts_with(needle) {
        return false;
    }
    if quote.ends_with('…') || collapsed.len() == needle.len() {
        return true;
    }
    collapsed[needle.len()..]
        .chars()
        .next()
        .is_some_and(|ch| !ch.is_alphanumeric())
}

pub(super) fn compute_hidden_and_reaction_masks(messages: &[ChatMessage]) -> (Vec<bool>, Vec<u8>) {
    let mut hidden = vec![false; messages.len()];
    let mut masks = vec![0u8; messages.len()];

    for (idx, msg) in messages.iter().enumerate() {
        if msg.role != MessageRole::User {
            continue;
        }
        let user_text = message_visible_text(msg);
        let Some(parsed) = parse_reaction_message(user_text.trim()) else {
            continue;
        };
        let needle = parsed.quote.strip_suffix('…').unwrap_or(&parsed.quote);
        if needle.is_empty() {
            continue;
        }

        let target_idx = if let Some(reference) = parsed.reference {
            (0..idx).rev().find(|&ti| {
                !hidden[ti]
                    && reaction_reference(&messages[ti].id) == reference
                    && normalized_reaction_prefix_matches(
                        &normalize_ws_collapse(&message_visible_text(&messages[ti])),
                        &parsed.quote,
                        needle,
                    )
            })
        } else {
            // Untagged spoken reactions match newest-first against normalized prefix only.
            (0..idx).rev().find(|&ti| {
                !hidden[ti]
                    && messages[ti].role == MessageRole::Assistant
                    && normalized_reaction_prefix_matches(
                        &normalize_ws_collapse(&message_visible_text(&messages[ti])),
                        &parsed.quote,
                        needle,
                    )
            })
        };

        if let Some(ti) = target_idx {
            hidden[idx] = true;
            masks[ti] ^= parsed.emoji.bit();

            // Hide a lone single-message plain-text assistant ack immediately following a hidden reaction.
            if idx + 1 < messages.len() && messages[idx + 1].role == MessageRole::Assistant {
                let next_is_also_assistant =
                    idx + 2 < messages.len() && messages[idx + 2].role == MessageRole::Assistant;
                if !next_is_also_assistant
                    && messages[idx + 1]
                        .blocks
                        .iter()
                        .all(|b| matches!(b, MessageBlock::Text { .. }))
                {
                    hidden[idx + 1] = true;
                }
            }
        }
    }

    (hidden, masks)
}

pub(super) fn compute_server_reaction_masks(messages: &[ChatMessage]) -> HashMap<String, u8> {
    let (hidden, masks) = compute_hidden_and_reaction_masks(messages);
    let mut by_id = HashMap::new();
    for (idx, msg) in messages.iter().enumerate() {
        if !hidden[idx] && masks[idx] != 0 {
            by_id.insert(msg.id.clone(), masks[idx]);
        }
    }
    by_id
}

#[derive(Debug)]
pub(crate) struct VisibleMessage<'a> {
    pub original_index: usize,
    pub message: &'a ChatMessage,
    pub reactions: Vec<(ReactionEmoji, usize)>,
}

/// Hides reaction user messages (and any lone assistant ack immediately following a
/// hidden reaction-only user message) and folds reactions onto their target messages.
/// Optimistic `local_reactions` are idempotent with daemon-echoed reaction messages
/// (they ensure the reaction count is at least 1 and never cancel out a server echo).
#[cfg(test)]
pub(crate) fn project_visible_messages<'a>(
    messages: &'a [ChatMessage],
    local_reactions: &HashMap<String, Vec<ReactionEmoji>>,
) -> Vec<VisibleMessage<'a>> {
    let empty_removals = HashMap::new();
    project_visible_messages_with_overrides(messages, local_reactions, &empty_removals, true)
}

pub(crate) fn project_visible_messages_with_overrides<'a>(
    messages: &'a [ChatMessage],
    local_reactions: &HashMap<String, Vec<ReactionEmoji>>,
    local_removals: &HashMap<String, Vec<ReactionEmoji>>,
    show_tools: bool,
) -> Vec<VisibleMessage<'a>> {
    let (hidden, masks) = compute_hidden_and_reaction_masks(messages);

    let mut visible = Vec::with_capacity(messages.len());
    for (idx, msg) in messages.iter().enumerate() {
        if hidden[idx]
            || (!show_tools
                && msg.role == MessageRole::Assistant
                && msg.text.trim().is_empty()
                && !msg.blocks.is_empty()
                && msg.blocks.iter().all(|block| match block {
                    MessageBlock::ToolUse { .. } => true,
                    MessageBlock::Text { text } => text.trim().is_empty(),
                    _ => false,
                }))
        {
            continue;
        }
        let mut counts: [usize; 3] = [0; 3];
        for (slot, emoji) in ReactionEmoji::all().into_iter().enumerate() {
            if (masks[idx] & emoji.bit()) != 0 {
                counts[slot] += 1;
            }
        }
        if let Some(local) = local_reactions.get(&msg.id) {
            for &emoji in local {
                let slot = match emoji {
                    ReactionEmoji::Like => 0,
                    ReactionEmoji::Dislike => 1,
                    ReactionEmoji::Heart => 2,
                };
                if counts[slot] == 0 {
                    counts[slot] = 1;
                }
            }
        }
        if let Some(removed) = local_removals.get(&msg.id) {
            for &emoji in removed {
                let slot = match emoji {
                    ReactionEmoji::Like => 0,
                    ReactionEmoji::Dislike => 1,
                    ReactionEmoji::Heart => 2,
                };
                counts[slot] = 0;
            }
        }
        let reactions = ReactionEmoji::all()
            .into_iter()
            .enumerate()
            .filter_map(|(slot, emoji)| (counts[slot] > 0).then_some((emoji, counts[slot])))
            .collect();

        visible.push(VisibleMessage {
            original_index: idx,
            message: msg,
            reactions,
        });
    }
    visible
}

/// Returns `true` iff a strict double-click (`click_count == 2`, never triple-click)
/// occurred within the drag-selection threshold (`|dx| <= 4px && |dy| <= 4px` within `500ms`),
/// preventing accidental `👍` on drag-select or paragraph triple-click.
pub(crate) fn should_toggle_double_click_like(
    click_count: usize,
    down_pos: (f32, f32),
    up_pos: (f32, f32),
    elapsed_ms: u64,
) -> bool {
    click_count == 2
        && (up_pos.0 - down_pos.0).abs() <= DOUBLE_CLICK_DRAG_THRESHOLD_PX
        && (up_pos.1 - down_pos.1).abs() <= DOUBLE_CLICK_DRAG_THRESHOLD_PX
        && elapsed_ms <= DOUBLE_CLICK_WINDOW_MS
}

/// Validates full double-click state for a specific message bubble:
/// requires same-bubble mousedown, same-bubble previous click (rejecting `None` or
/// cross-bubble clicks with `u64::MAX`), click-to-click pointer proximity `<= 4px`,
/// mousedown->mouseup drag `<= 4px`, `<= 500ms`, and `click_count == 2`.
pub(crate) fn should_toggle_double_click_on_bubble(
    click_count: usize,
    target_msg_id: &str,
    down: Option<(&str, f32, f32)>,
    prev_click: Option<(&str, f32, f32, u64)>,
    up_pos: (f32, f32),
) -> bool {
    let Some((down_id, dx, dy)) = down else {
        return false;
    };
    if down_id != target_msg_id {
        return false;
    }
    let (prev_pos, elapsed_ms) = match prev_click {
        Some((prev_id, px, py, elapsed)) if prev_id == target_msg_id => ((px, py), elapsed),
        _ => return false,
    };
    let click_to_click_close = (up_pos.0 - prev_pos.0).abs() <= DOUBLE_CLICK_DRAG_THRESHOLD_PX
        && (up_pos.1 - prev_pos.1).abs() <= DOUBLE_CLICK_DRAG_THRESHOLD_PX;
    click_to_click_close
        && should_toggle_double_click_like(click_count, (dx, dy), up_pos, elapsed_ms)
}

impl MultibotView {
    pub(crate) fn toggle_local_reaction(
        &mut self,
        target_id: &str,
        target_text: &str,
        emoji: ReactionEmoji,
        cx: &mut Context<Self>,
    ) {
        if self
            .selection
            .selected_session
            .as_ref()
            .is_some_and(|s| s.archived)
        {
            return;
        }
        let server_masks = compute_server_reaction_masks(&self.history.messages);
        let server_has = (server_masks.get(target_id).copied().unwrap_or(0) & emoji.bit()) != 0;
        let local_add_has = self
            .chat_state
            .local_reactions
            .get(target_id)
            .is_some_and(|v| v.contains(&emoji));
        let local_rem_has = self
            .chat_state
            .local_reaction_removals
            .get(target_id)
            .is_some_and(|v| v.contains(&emoji));
        let currently_shown = if server_has {
            !local_rem_has
        } else {
            local_add_has
        };

        if currently_shown {
            if let Some(adds) = self.chat_state.local_reactions.get_mut(target_id) {
                adds.retain(|&e| e != emoji);
                if adds.is_empty() {
                    self.chat_state.local_reactions.remove(target_id);
                }
            }
            if server_has {
                let rems = self
                    .chat_state
                    .local_reaction_removals
                    .entry(target_id.to_string())
                    .or_default();
                if !rems.contains(&emoji) {
                    rems.push(emoji);
                }
            }
        } else {
            if let Some(rems) = self.chat_state.local_reaction_removals.get_mut(target_id) {
                rems.retain(|&e| e != emoji);
                if rems.is_empty() {
                    self.chat_state.local_reaction_removals.remove(target_id);
                }
            }
            if !server_has {
                let adds = self
                    .chat_state
                    .local_reactions
                    .entry(target_id.to_string())
                    .or_default();
                if !adds.contains(&emoji) {
                    adds.push(emoji);
                }
            }
        }

        if let Some(wire_text) = format_reaction_message(emoji, target_id, target_text)
            && let (Some(session), Some(tx)) = (
                self.selection.selected_session.clone(),
                self.active_session_tx(),
            )
        {
            let _ = tx.send(ClientRequest::SendPrompt {
                options: self.composer_options_for(&session),
                session_id: session.id,
                agent: session.agent,
                cwd: session.cwd,
                text: wire_text,
            });
        }
        cx.notify();
    }
}
