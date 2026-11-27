use crate::providers::provider_for;
use multibot_proto::{AgentKind, MessageBlock, MessageRole, SearchHit, ServerEvent};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::{Duration, Instant};

pub const MIN_SEARCH_QUERY_CHARS: usize = 1;
pub const MAX_SEARCH_QUERY_CHARS: usize = 200;
pub const MAX_SEARCH_HITS: usize = 500;
pub const SEARCH_TIME_BUDGET: Duration = Duration::from_millis(1500);
pub const SNIPPET_LEAD_CHARS: usize = 60;
pub const SNIPPET_MAX_CHARS: usize = 160;
/// Cap any single JSONL line buffered during `search_history` to 256 KiB (~65 pages of prose)
/// so peak transient allocation (raw line + JSON AST + parsed message) stays comfortably within
/// the `< 10 MB` daemon RSS/HWM budget even across repeated searches.
pub const SEARCH_MAX_LINE_BYTES: usize = 256 * 1024;

#[derive(Debug)]
pub enum SearchError {
    InvalidQuery(String),
    Io(std::io::Error),
}

impl std::fmt::Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidQuery(m) => write!(f, "{m}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SearchError {}

pub fn search_history(
    request_id: String,
    session_id: String,
    source_path: &Path,
    agent: AgentKind,
    query: &str,
) -> Result<ServerEvent, SearchError> {
    let q_chars = query.chars().count();
    if !(MIN_SEARCH_QUERY_CHARS..=MAX_SEARCH_QUERY_CHARS).contains(&q_chars) {
        return Err(SearchError::InvalidQuery(format!(
            "Search query must be {MIN_SEARCH_QUERY_CHARS}..={MAX_SEARCH_QUERY_CHARS} characters"
        )));
    }

    let needle = query.to_lowercase();
    let provider = provider_for(agent);
    let file = File::open(source_path).map_err(SearchError::Io)?;
    let mut reader = BufReader::with_capacity(64 * 1024, file);

    let started = Instant::now();
    let mut raw_line = Vec::with_capacity(4096);
    let mut scratch = String::new();
    let mut cursor: u64 = 0;
    let mut line_num: usize = 0;
    let mut hits = Vec::new();
    let mut truncated = false;

    'scan: loop {
        if line_num % 8 == 0 && started.elapsed() >= SEARCH_TIME_BUDGET {
            truncated = true;
            break;
        }
        let line_offset = cursor;
        match read_line_bounded(&mut reader, &mut raw_line, SEARCH_MAX_LINE_BYTES)
            .map_err(SearchError::Io)?
        {
            BoundedLine::Eof | BoundedLine::IncompleteTrailing { .. } => {
                // Stop at EOF or incomplete trailing line
                break;
            }
            BoundedLine::Oversized {
                bytes_read,
                terminated,
            } => {
                cursor += bytes_read;
                line_num += 1;
                if !terminated {
                    break;
                }
                continue;
            }
            BoundedLine::Complete { bytes_read } => {
                cursor += bytes_read;
                line_num += 1;
            }
        }

        let parsed = provider.parse_slice(&raw_line, line_offset);
        if raw_line.capacity() > 64 * 1024 {
            raw_line.clear();
            raw_line.shrink_to(4096);
        }

        for msg in parsed.messages {
            let mut match_and_record = |role: MessageRole, haystack: &str| -> bool {
                if haystack.is_empty() {
                    return false;
                }
                if let Some(match_char_idx) = find_case_insensitive_char_index(haystack, &needle) {
                    let snippet = extract_snippet(
                        haystack,
                        match_char_idx,
                        SNIPPET_LEAD_CHARS,
                        SNIPPET_MAX_CHARS,
                    );
                    hits.push(SearchHit {
                        message_id: msg.id.clone(),
                        byte_offset: line_offset,
                        snippet,
                        role,
                    });
                    if hits.len() >= MAX_SEARCH_HITS {
                        truncated = true;
                        return true;
                    }
                }
                false
            };

            if match_and_record(msg.role, &msg.text) {
                break 'scan;
            }

            for block in &msg.blocks {
                match block {
                    MessageBlock::ToolUse {
                        summary,
                        detail,
                        output,
                        ..
                    } => {
                        scratch.clear();
                        if !summary.is_empty() {
                            scratch.push_str(summary);
                        }
                        if !detail.is_empty() {
                            if !scratch.is_empty() {
                                scratch.push('\n');
                            }
                            scratch.push_str(detail);
                        }
                        if let Some(out) = output
                            && !out.is_empty()
                        {
                            if !scratch.is_empty() {
                                scratch.push('\n');
                            }
                            scratch.push_str(out);
                        }
                        if match_and_record(MessageRole::Tool, &scratch) {
                            break 'scan;
                        }
                        if scratch.capacity() > 16 * 1024 {
                            scratch.clear();
                            scratch.shrink_to(4096);
                        }
                    }
                    MessageBlock::QuestionPrompt { questions, .. } => {
                        scratch.clear();
                        for q in questions {
                            if !scratch.is_empty() {
                                scratch.push('\n');
                            }
                            scratch.push_str(&q.question);
                            for opt in &q.options {
                                scratch.push(' ');
                                scratch.push_str(&opt.label);
                            }
                        }
                        if match_and_record(msg.role, &scratch) {
                            break 'scan;
                        }
                        if scratch.capacity() > 16 * 1024 {
                            scratch.clear();
                            scratch.shrink_to(4096);
                        }
                    }
                    MessageBlock::Text { .. } | MessageBlock::Image { .. } => {}
                }
            }
        }
    }

    Ok(ServerEvent::SearchResults {
        request_id,
        session_id,
        hits,
        truncated,
    })
}

#[derive(Debug, PartialEq, Eq)]
enum BoundedLine {
    Eof,
    Complete { bytes_read: u64 },
    IncompleteTrailing { bytes_read: u64 },
    Oversized { bytes_read: u64, terminated: bool },
}

/// Appends `slice` to `out` while capping capacity growth at `max_bytes` so `Vec` doubling
/// never overshoots `SEARCH_MAX_LINE_BYTES`.
fn extend_capped(out: &mut Vec<u8>, slice: &[u8], max_bytes: usize) {
    let needed = out.len() + slice.len();
    if needed > out.capacity() {
        let target_cap = out.capacity().saturating_mul(2).max(needed).min(max_bytes);
        out.reserve_exact(target_cap - out.len());
    }
    out.extend_from_slice(slice);
}

/// Reads a single `\n`-terminated line into `out` capped strictly at `max_bytes`.
///
/// If a line exceeds `max_bytes`, drains the remainder of that line in bounded buffer chunks
/// via `fill_buf` / `consume` until `\n` or EOF without growing `out`, returning the exact
/// byte count consumed so callers keep accurate file byte offsets.
fn read_line_bounded<R: BufRead>(
    reader: &mut R,
    out: &mut Vec<u8>,
    max_bytes: usize,
) -> std::io::Result<BoundedLine> {
    out.clear();
    let mut total_bytes: u64 = 0;
    let mut oversized = false;

    loop {
        let available = match reader.fill_buf() {
            Ok(buf) => buf,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            if total_bytes == 0 {
                return Ok(BoundedLine::Eof);
            }
            return Ok(if oversized {
                BoundedLine::Oversized {
                    bytes_read: total_bytes,
                    terminated: false,
                }
            } else {
                BoundedLine::IncompleteTrailing {
                    bytes_read: total_bytes,
                }
            });
        }

        if let Some(nl_pos) = available.iter().position(|&b| b == b'\n') {
            let chunk_len = nl_pos + 1;
            total_bytes += chunk_len as u64;
            if !oversized && out.len() + chunk_len <= max_bytes {
                extend_capped(out, &available[..chunk_len], max_bytes);
                reader.consume(chunk_len);
                return Ok(BoundedLine::Complete {
                    bytes_read: total_bytes,
                });
            }
            out.clear();
            if out.capacity() > 64 * 1024 {
                out.shrink_to(4096);
            }
            reader.consume(chunk_len);
            return Ok(BoundedLine::Oversized {
                bytes_read: total_bytes,
                terminated: true,
            });
        }

        let chunk_len = available.len();
        total_bytes += chunk_len as u64;
        if !oversized {
            if out.len() + chunk_len <= max_bytes {
                extend_capped(out, available, max_bytes);
            } else {
                oversized = true;
                out.clear();
                if out.capacity() > 64 * 1024 {
                    out.shrink_to(4096);
                }
            }
        }
        reader.consume(chunk_len);
    }
}

/// Finds the first literal case-insensitive occurrence of `needle_lower` in `haystack`,
/// returning the Unicode scalar (`char`) index in `haystack` safely even when Unicode
/// lowercasing expands one char into multiple chars (e.g. `İ` -> `i\u{307}`).
///
/// Uses zero heap allocation for ASCII inputs and bounded overlapping windows (`O(|needle|)`,
/// ~2–8 KiB) for Unicode inputs so searching large lines never allocates a full lowercased copy.
fn find_case_insensitive_char_index(haystack: &str, needle_lower: &str) -> Option<usize> {
    if needle_lower.is_empty() {
        return Some(0);
    }

    // Zero-allocation ASCII fast path: byte indices equal char indices and lowercasing is 1-to-1.
    if haystack.is_ascii() && needle_lower.is_ascii() {
        let h = haystack.as_bytes();
        let nb = needle_lower.as_bytes();
        if nb.len() > h.len() {
            return None;
        }
        let first = nb[0];
        let first_upper = first.to_ascii_uppercase();
        let limit = h.len() - nb.len();
        for i in 0..=limit {
            let b = h[i];
            if (b == first || b == first_upper) && h[i..i + nb.len()].eq_ignore_ascii_case(nb) {
                return Some(i);
            }
        }
        return None;
    }

    // Bounded overlapping Unicode chunk scan: every Unicode `char` lowercases to >= 1 scalar,
    // so any match of `needle_lower` spans at most `needle_chars` input chars of `haystack`.
    // Provide leading + trailing context characters around each non-boundary slice edge so
    // context-sensitive Unicode lowercasing (Greek `Final_Sigma`, U+03A3 'Σ' -> 'ς'/'σ')
    // sees the true surrounding characters and produces zero false positives or false negatives.
    const LEAD_CTX_CHARS: usize = 8;
    const TRAIL_CTX_CHARS: usize = 8;
    let needle_chars = needle_lower.chars().count();
    let overlap = needle_chars + TRAIL_CTX_CHARS;
    let chunk_chars = (needle_chars * 2 + TRAIL_CTX_CHARS * 2).max(1024);
    let step_chars = chunk_chars - overlap;

    let mut start_byte = 0usize;
    let mut start_char = 0usize;

    loop {
        let tail = &haystack[start_byte..];
        let mut end_byte = haystack.len();
        let mut next_start_byte = haystack.len();
        let mut next_start_char = start_char;
        let mut seen_chars = 0usize;

        for (rel_byte, _) in tail.char_indices() {
            if seen_chars == step_chars {
                next_start_byte = start_byte + rel_byte;
                next_start_char = start_char + seen_chars;
            }
            if seen_chars == chunk_chars {
                end_byte = start_byte + rel_byte;
                break;
            }
            seen_chars += 1;
        }

        // Extend left by up to `LEAD_CTX_CHARS` when `start_byte > 0` so a true word-final
        // Greek 'Σ' right at `start_char` still sees its preceding cased character.
        let ctx_start_byte = if start_byte > 0 {
            haystack[..start_byte]
                .char_indices()
                .rev()
                .take(LEAD_CTX_CHARS)
                .last()
                .map(|(idx, _)| idx)
                .unwrap_or(0)
        } else {
            0
        };
        let lead_folded_bytes: usize = haystack[ctx_start_byte..start_byte]
            .chars()
            .flat_map(char::to_lowercase)
            .map(char::len_utf8)
            .sum();

        let window = &haystack[ctx_start_byte..end_byte];
        let folded = window.to_lowercase();
        if lead_folded_bytes <= folded.len()
            && folded.is_char_boundary(lead_folded_bytes)
            && let Some(rel_folded_pos) = folded[lead_folded_bytes..].find(needle_lower)
        {
            let core = &haystack[start_byte..end_byte];
            let mut folded_bytes = 0usize;
            let mut match_rel_char = core.chars().count().saturating_sub(1);
            for (rel_char_idx, ch) in core.chars().enumerate() {
                let ch_folded_bytes: usize = ch.to_lowercase().map(char::len_utf8).sum();
                if folded_bytes + ch_folded_bytes > rel_folded_pos {
                    match_rel_char = rel_char_idx;
                    break;
                }
                folded_bytes += ch_folded_bytes;
            }
            // For non-final windows, only accept matches starting strictly inside
            // `[start_char .. start_char + step_chars)` so at least `TRAIL_CTX_CHARS`
            // trailing context chars follow the match inside `window`; any match at
            // `>= step_chars` will be evaluated cleanly in the next window with full context.
            if end_byte == haystack.len() || match_rel_char < step_chars {
                return Some(start_char + match_rel_char);
            }
        }

        if end_byte == haystack.len() {
            return None;
        }
        start_byte = next_start_byte;
        start_char = next_start_char;
    }
}

fn extract_snippet(
    haystack: &str,
    match_char_idx: usize,
    lead_chars: usize,
    max_chars: usize,
) -> String {
    if haystack.is_empty() {
        return String::new();
    }
    let start = match_char_idx.saturating_sub(lead_chars);
    let has_prefix = start > 0;
    let avail = max_chars.saturating_sub(usize::from(has_prefix));

    let mut iter = haystack.chars().skip(start);
    let mut window: Vec<char> = Vec::with_capacity(avail.min(256) + 1);
    for _ in 0..=avail {
        match iter.next() {
            Some(c) => window.push(c),
            None => break,
        }
    }

    let has_suffix = window.len() > avail;
    let take_len = if has_suffix {
        avail.saturating_sub(1)
    } else {
        window.len()
    };

    let mut out = String::with_capacity(max_chars.min(take_len + 2) * 2);
    if has_prefix {
        out.push('…');
    }
    for &c in &window[..take_len] {
        if c == '\r' || c == '\n' || c == '\t' {
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    if has_suffix {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEARCH_COUNTER: AtomicU64 = AtomicU64::new(1);

    fn temp_file(tag: &str, body: &str) -> std::path::PathBuf {
        let n = SEARCH_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "multibotd-search-{tag}-{}-{n}.jsonl",
            std::process::id()
        ));
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn rejects_empty_and_overlong_queries() {
        let p = temp_file("val", "");
        assert!(matches!(
            search_history("r1".into(), "s1".into(), &p, AgentKind::Claude, ""),
            Err(SearchError::InvalidQuery(_))
        ));
        let long_q = "a".repeat(201);
        assert!(matches!(
            search_history("r2".into(), "s1".into(), &p, AgentKind::Claude, &long_q),
            Err(SearchError::InvalidQuery(_))
        ));
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn literal_case_insensitive_search_over_text_and_tools() {
        let body = concat!(
            r#"{"type":"user","uuid":"u1","message":{"content":"Please check foo.*(bar)[0] in config 🚀"}}"#,
            "\n",
            r#"{"type":"assistant","uuid":"a1","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"grep FOO.*(BAR)[0] src/","description":"Search pattern"}}]}}"#,
            "\n"
        );
        let p = temp_file("lit", body);

        let res = search_history(
            "req-1".into(),
            "s1".into(),
            &p,
            AgentKind::Claude,
            "foo.*(BAR)[0]",
        )
        .unwrap();

        match res {
            ServerEvent::SearchResults {
                request_id,
                session_id,
                hits,
                truncated,
            } => {
                assert_eq!(request_id, "req-1");
                assert_eq!(session_id, "s1");
                assert!(!truncated);
                assert_eq!(hits.len(), 2);
                assert_eq!(hits[0].message_id, "u1");
                assert_eq!(hits[0].role, MessageRole::User);
                assert_eq!(hits[0].byte_offset, 0);
                assert!(hits[0].snippet.contains("foo.*(bar)[0]"));
                assert_eq!(hits[1].message_id, "a1");
                assert_eq!(hits[1].role, MessageRole::Tool);
                assert!(hits[1].byte_offset > 0);
            }
            other => panic!("expected SearchResults, got {other:?}"),
        }

        let _ = fs::remove_file(&p);
    }

    #[test]
    fn enforces_500_hit_truncation_cap() {
        let mut body = String::new();
        for i in 0..600 {
            body.push_str(&format!(
                r#"{{"type":"user","uuid":"u{i}","message":{{"content":"matching needle item {i}"}}}}"#
            ));
            body.push('\n');
        }
        let p = temp_file("cap", &body);

        let res = search_history(
            "req-cap".into(),
            "s-cap".into(),
            &p,
            AgentKind::Claude,
            "NEEDLE",
        )
        .unwrap();
        match res {
            ServerEvent::SearchResults {
                hits, truncated, ..
            } => {
                assert_eq!(hits.len(), MAX_SEARCH_HITS);
                assert!(truncated);
            }
            other => panic!("expected SearchResults, got {other:?}"),
        }

        let _ = fs::remove_file(&p);
    }

    #[test]
    fn skips_oversized_5mib_line_and_finds_matches_before_and_after_with_exact_offsets() {
        let line_before = concat!(
            r#"{"type":"user","uuid":"u-before","message":{"content":"Find SPECIAL_TOKEN before giant"}}"#,
            "\n"
        );
        let giant_filler = "z".repeat(5 * 1024 * 1024);
        let giant_line = format!(
            "{{\"type\":\"assistant\",\"uuid\":\"a-giant\",\"message\":{{\"content\":\"SPECIAL_TOKEN {giant_filler}\"}}}}\n"
        );
        assert!(giant_line.len() > SEARCH_MAX_LINE_BYTES);
        let line_after = concat!(
            r#"{"type":"assistant","uuid":"a-after","message":{"content":"Find SPECIAL_TOKEN after giant"}}"#,
            "\n"
        );

        let mut full_body =
            String::with_capacity(line_before.len() + giant_line.len() + line_after.len());
        full_body.push_str(line_before);
        full_body.push_str(&giant_line);
        full_body.push_str(line_after);
        let expected_after_offset = (line_before.len() + giant_line.len()) as u64;

        let p = temp_file("oversized-5mib", &full_body);
        let res = search_history(
            "req-giant".into(),
            "s-giant".into(),
            &p,
            AgentKind::Claude,
            "special_token",
        )
        .unwrap();

        match res {
            ServerEvent::SearchResults {
                hits, truncated, ..
            } => {
                assert!(!truncated);
                assert_eq!(hits.len(), 2);
                assert_eq!(hits[0].message_id, "u-before");
                assert_eq!(hits[0].byte_offset, 0);
                assert_eq!(hits[1].message_id, "a-after");
                assert_eq!(hits[1].byte_offset, expected_after_offset);
            }
            other => panic!("expected SearchResults, got {other:?}"),
        }

        let _ = fs::remove_file(&p);
    }

    #[test]
    fn read_line_bounded_exact_limit_and_unterminated_oversized_eof() {
        let exact = format!("{}\n", "a".repeat(15)); // 16 bytes
        let over = format!("{}\n", "b".repeat(16)); // 17 bytes
        let tail_eof = "c".repeat(25); // 25 bytes, no '\n'
        let combined = format!("{exact}{over}{tail_eof}");

        let mut reader = BufReader::with_capacity(4, combined.as_bytes());
        let mut buf = Vec::new();

        assert_eq!(
            read_line_bounded(&mut reader, &mut buf, 16).unwrap(),
            BoundedLine::Complete { bytes_read: 16 }
        );
        assert_eq!(buf, exact.as_bytes());

        assert_eq!(
            read_line_bounded(&mut reader, &mut buf, 16).unwrap(),
            BoundedLine::Oversized {
                bytes_read: 17,
                terminated: true
            }
        );
        assert!(buf.is_empty());

        assert_eq!(
            read_line_bounded(&mut reader, &mut buf, 16).unwrap(),
            BoundedLine::Oversized {
                bytes_read: 25,
                terminated: false
            }
        );
        assert!(buf.is_empty());

        assert_eq!(
            read_line_bounded(&mut reader, &mut buf, 16).unwrap(),
            BoundedLine::Eof
        );
    }

    #[test]
    fn find_case_insensitive_handles_ascii_unicode_expanding_i_and_chunk_boundaries() {
        // Pure ASCII fast path
        assert_eq!(
            find_case_insensitive_char_index("Hello World FooBar", "foobar"),
            Some(12)
        );
        assert_eq!(find_case_insensitive_char_index("short", "longer"), None);

        // Expanding Unicode 'İ' (U+0130 -> "i\u{307}") + multi-byte UTF-8 ('🚀', '日本語')
        let hay = format!("{}🚀日本語TARGET_END", "İ".repeat(2500));
        let idx = find_case_insensitive_char_index(&hay, "target_end").unwrap();
        assert_eq!(idx, 2500 + 1 + 3);
        let snippet = extract_snippet(&hay, idx, 4, 20);
        assert!(snippet.contains("🚀日本語TARGET_END"));

        // Match straddling the 1024-char Unicode chunk boundary
        let boundary_hay = format!("{}🚀CROSS_BOUNDARY_NEEDLE_TAIL", "İ".repeat(1015));
        let b_idx =
            find_case_insensitive_char_index(&boundary_hay, "cross_boundary_needle").unwrap();
        assert_eq!(b_idx, 1016);

        // Searching directly for Turkish 'İ' lowercased ("i\u{307}")
        let dotted_hay = format!("{}İSTANBUL", "x".repeat(1020));
        let needle_dotted = "İSTANBUL".to_lowercase();
        assert_eq!(
            find_case_insensitive_char_index(&dotted_hay, &needle_dotted),
            Some(1020)
        );
    }

    #[test]
    fn find_case_insensitive_handles_greek_final_sigma_across_chunk_boundaries() {
        // 1. Medial Greek 'Σ' at exact char index 1023 (`chunk_chars - 1`) followed by 'Α':
        //    must NEVER falsely fold to final sigma 'ς', and MUST match medial 'σ' at 1023.
        let medial_1023 = format!("{}Σ{}", "Α".repeat(1023), "Α".repeat(100));
        assert_eq!(find_case_insensitive_char_index(&medial_1023, "ς"), None);
        assert_eq!(
            find_case_insensitive_char_index(&medial_1023, "σ"),
            Some(1023)
        );

        // 2. True word-final Greek 'Σ' at exact `step_chars` (1015 for 1-char needle) followed
        //    by space + trailing text so Window 0 defers it and Window 1 relies on leading context:
        //    must match final sigma 'ς' at 1015 (zero false negative!).
        let step_chars_1 = 1024 - (1 + 8);
        let final_at_step = format!("{}Σ {}", "Α".repeat(step_chars_1), "Α".repeat(200));
        assert_eq!(
            find_case_insensitive_char_index(&final_at_step, "ς"),
            Some(step_chars_1)
        );
        assert_eq!(find_case_insensitive_char_index(&final_at_step, "σ"), None);

        // 3. True word-final Greek 'Σ' right at char 1023 followed by space + more text.
        let final_at_1023 = format!("{}Σ {}", "Α".repeat(1023), "Α".repeat(100));
        assert_eq!(
            find_case_insensitive_char_index(&final_at_1023, "ς"),
            Some(1023)
        );
    }

    #[test]
    fn search_history_exact_256kib_matched_and_256kib_plus_1_skipped_with_exact_offsets() {
        let make_spaced = |ch: char, len: usize| -> String {
            let mut s = String::with_capacity(len);
            for i in 0..len {
                s.push(if i % 8 == 7 { ' ' } else { ch });
            }
            s
        };
        let prefix = r#"{"type":"user","uuid":"u-exact","message":{"content":""#;
        let marker = " EXACT_CAP_NEEDLE";
        let suffix = "\"}}\n";
        let fill_exact = SEARCH_MAX_LINE_BYTES - prefix.len() - marker.len() - suffix.len();
        let exact_line = format!("{prefix}{}{marker}{suffix}", make_spaced('a', fill_exact));
        assert_eq!(exact_line.len(), SEARCH_MAX_LINE_BYTES);

        let over_prefix = r#"{"type":"user","uuid":"u-over","message":{"content":""#;
        let fill_over =
            (SEARCH_MAX_LINE_BYTES + 1) - over_prefix.len() - marker.len() - suffix.len();
        let over_line = format!("{over_prefix}{}{marker}{suffix}", make_spaced('b', fill_over));
        assert_eq!(over_line.len(), SEARCH_MAX_LINE_BYTES + 1);

        let after_line =
            "{\"type\":\"assistant\",\"uuid\":\"u-after\",\"message\":{\"content\":\"EXACT_CAP_NEEDLE after\"}}\n";
        let expected_after_offset = (exact_line.len() + over_line.len()) as u64;

        let combined = format!("{exact_line}{over_line}{after_line}");
        let p = temp_file("exact-256k", &combined);

        let res = search_history(
            "req-256k".into(),
            "s-256k".into(),
            &p,
            AgentKind::Claude,
            "exact_cap_needle",
        )
        .unwrap();

        match res {
            ServerEvent::SearchResults {
                hits, truncated, ..
            } => {
                assert!(!truncated);
                assert_eq!(hits.len(), 2);
                assert_eq!(hits[0].message_id, "u-exact");
                assert_eq!(hits[0].byte_offset, 0);
                assert!(hits[0].snippet.contains("EXACT_CAP_NEEDLE"));
                assert_eq!(hits[1].message_id, "u-after");
                assert_eq!(hits[1].byte_offset, expected_after_offset);
            }
            other => panic!("expected SearchResults, got {other:?}"),
        }

        let _ = fs::remove_file(&p);
    }
}
