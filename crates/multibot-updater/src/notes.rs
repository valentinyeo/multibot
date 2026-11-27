use crate::security::parse_semver_version;
use semver::Version;
use std::collections::HashSet;

/// Number of release note bullets shown before clicking "Show more".
pub const COLLAPSED_NOTES_LIMIT: usize = 3;
/// Maximum number of plain-English release notes retained per release.
pub const MAX_STORED_NOTES: usize = 16;
const MAX_NOTE_CHARS: usize = 240;

const EXACT_FILLER_NOTES: &[&str] = &[
    "various improvements",
    "bug fixes",
    "general improvements and bug fixes",
    "maintenance",
    "improvements",
    "fixes",
    "minor fixes",
    "misc",
    "updates",
];

const REJECTED_SUBSTRINGS: &[&str] = &[
    "behind the scenes",
    "latest improvements",
    "checks and fixes, no visible change",
    "full changelog:",
];

const REJECTED_PREFIXES: &[&str] = &[
    "[infra]",
    "[ci]",
    "ci:",
    "ci(",
    "chore:",
    "chore(",
    "test:",
    "test(",
    "tests:",
    "build:",
    "build(",
    "docs:",
    "docs(",
    "style:",
    "refactor:",
    "release gate:",
    "merge pull request",
    "merge branch",
    "merge remote-tracking",
    "multiprompt verify:",
];

const REJECTED_WHOLE_WORDS: &[&str] = &[
    "ci",
    "infra",
    "test",
    "tests",
    "proof",
    "proofs",
    "pipeline",
    "pipelines",
    "workflow",
    "workflows",
    "clippy",
    "rustfmt",
    "lockfile",
];

fn is_forbidden_control_or_bidi_char(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{2028}' | '\u{2029}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
        )
}

/// Returns `true` when `s` contains any Unicode C0/C1 control character, line/paragraph separator
/// (`\u{2028}` / `\u{2029}`), or bidirectional override/isolate format character
/// (`\u{202A}`..=`\u{202E}`, `\u{2066}`..=`\u{2069}`).
pub fn has_forbidden_control_or_bidi(s: &str) -> bool {
    s.chars().any(is_forbidden_control_or_bidi_char)
}

pub(crate) fn clean_note_line(raw: &str) -> Option<String> {
    if raw
        .chars()
        .any(|c| c != '\t' && is_forbidden_control_or_bidi_char(c))
    {
        return None;
    }
    let mut s = raw.trim();
    if s.is_empty() || s.starts_with('#') {
        return None;
    }
    loop {
        let mut stripped = false;
        for prefix in ["- ", "* ", "• "] {
            if let Some(rest) = s.strip_prefix(prefix) {
                s = rest.trim();
                stripped = true;
                break;
            }
        }
        if !stripped {
            break;
        }
    }
    if s.is_empty()
        || s.starts_with('#')
        || s.contains("```")
        || has_forbidden_control_or_bidi(s)
    {
        return None;
    }
    if s.chars().count() > MAX_NOTE_CHARS {
        let truncated: String = s.chars().take(MAX_NOTE_CHARS.saturating_sub(1)).collect();
        Some(format!("{}…", truncated.trim_end()))
    } else {
        Some(s.to_string())
    }
}

fn contains_whole_word_ascii(lower: &str, word: &str) -> bool {
    let bytes = lower.as_bytes();
    let wbytes = word.as_bytes();
    if wbytes.is_empty() || bytes.len() < wbytes.len() {
        return false;
    }
    let mut start = 0;
    while let Some(rel) = lower[start..].find(word) {
        let idx = start + rel;
        let end = idx + wbytes.len();
        let before_ok = idx == 0 || !bytes[idx - 1].is_ascii_alphanumeric();
        let after_ok = end == bytes.len() || !bytes[end].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        start = idx + 1;
    }
    false
}

fn is_duration_unit(unit: &str) -> bool {
    unit.eq_ignore_ascii_case("s")
        || unit.eq_ignore_ascii_case("sec")
        || unit.eq_ignore_ascii_case("secs")
        || unit.eq_ignore_ascii_case("m")
        || unit.eq_ignore_ascii_case("min")
        || unit.eq_ignore_ascii_case("mins")
}

/// Pure-Rust exact equivalent of
/// `(?i)\b(?:seconds?|secs?|minutes?|mins?|a\s+few|takes?|eta|\d+\s*(?:s|sec|secs|m|min|mins))\b|~\s*\d+\s*[sm]\b`
/// (normalizing all Unicode whitespace including NBSP `\u{00A0}`, vertical tab `\x0b`, and
/// zero-width space/BOM `\u{200B}` / `\u{FEFF}` while keeping ZWJ `\u{200D}` intact) used to
/// enforce Hard Invariant #4 (zero time/duration wording on update UI copy and release notes).
pub fn contains_forbidden_duration_wording(text: &str) -> bool {
    let normalized: String = text
        .chars()
        .map(|c| {
            if c.is_whitespace() || matches!(c, '\x0b' | '\u{200B}' | '\u{FEFF}') {
                ' '
            } else {
                c
            }
        })
        .collect();
    let bytes = normalized.as_bytes();
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let is_ws = |b: u8| b == b' ';

    let mut i = 0;
    while i < bytes.len() {
        if is_word(bytes[i]) {
            let start = i;
            while i < bytes.len() && is_word(bytes[i]) {
                i += 1;
            }
            let token = &normalized[start..i];
            if token.eq_ignore_ascii_case("second")
                || token.eq_ignore_ascii_case("seconds")
                || token.eq_ignore_ascii_case("sec")
                || token.eq_ignore_ascii_case("secs")
                || token.eq_ignore_ascii_case("minute")
                || token.eq_ignore_ascii_case("minutes")
                || token.eq_ignore_ascii_case("min")
                || token.eq_ignore_ascii_case("mins")
                || token.eq_ignore_ascii_case("take")
                || token.eq_ignore_ascii_case("takes")
                || token.eq_ignore_ascii_case("eta")
            {
                return true;
            }

            let digit_len = token.bytes().take_while(|b| b.is_ascii_digit()).count();
            if digit_len > 0 {
                if digit_len < token.len() {
                    if is_duration_unit(&token[digit_len..]) {
                        return true;
                    }
                } else {
                    let mut j = i;
                    let mut ws_count = 0;
                    while j < bytes.len() && is_ws(bytes[j]) {
                        j += 1;
                        ws_count += 1;
                    }
                    if ws_count > 0 && j < bytes.len() && is_word(bytes[j]) {
                        let wstart = j;
                        while j < bytes.len() && is_word(bytes[j]) {
                            j += 1;
                        }
                        if is_duration_unit(&normalized[wstart..j]) {
                            return true;
                        }
                    }
                }
            }

            if token.eq_ignore_ascii_case("a") {
                let mut j = i;
                let mut ws_count = 0;
                while j < bytes.len() && is_ws(bytes[j]) {
                    j += 1;
                    ws_count += 1;
                }
                if ws_count > 0 && j < bytes.len() && is_word(bytes[j]) {
                    let wstart = j;
                    while j < bytes.len() && is_word(bytes[j]) {
                        j += 1;
                    }
                    if normalized[wstart..j].eq_ignore_ascii_case("few") {
                        return true;
                    }
                }
            }
        } else {
            if bytes[i] == b'~' {
                let mut j = i + 1;
                while j < bytes.len() && is_ws(bytes[j]) {
                    j += 1;
                }
                let digit_start = j;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j > digit_start {
                    while j < bytes.len() && is_ws(bytes[j]) {
                        j += 1;
                    }
                    if j < bytes.len() && matches!(bytes[j], b's' | b'S' | b'm' | b'M') {
                        let after = j + 1;
                        if after == bytes.len() || !is_word(bytes[after]) {
                            return true;
                        }
                    }
                }
            }
            i += 1;
        }
    }
    false
}

/// Returns `true` when `line` is a concise, user-facing plain-English note rather than CI/test/infra
/// chatter, markdown fences, control/bidi characters, or forbidden duration wording.
pub fn is_user_facing_note(line: &str) -> bool {
    if has_forbidden_control_or_bidi(line) {
        return false;
    }
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.contains("```") {
        return false;
    }
    if contains_forbidden_duration_wording(trimmed) {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    if EXACT_FILLER_NOTES.contains(&lower.as_str()) {
        return false;
    }
    if REJECTED_SUBSTRINGS.iter().any(|sub| lower.contains(sub)) {
        return false;
    }
    if REJECTED_PREFIXES.iter().any(|pref| lower.starts_with(pref)) {
        return false;
    }
    if REJECTED_WHOLE_WORDS
        .iter()
        .any(|word| contains_whole_word_ascii(&lower, word))
    {
        return false;
    }
    true
}

/// Cleans, filters, and case-insensitively deduplicates raw notes up to `MAX_STORED_NOTES`.
pub fn filter_plain_english_notes(raw_notes: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for raw in raw_notes {
        let Some(cleaned) = clean_note_line(raw) else {
            continue;
        };
        if !is_user_facing_note(&cleaned) {
            continue;
        }
        let key = cleaned.to_ascii_lowercase();
        if seen.insert(key) {
            out.push(cleaned);
            if out.len() >= MAX_STORED_NOTES {
                break;
            }
        }
    }
    out
}

/// Extracts plain-English release notes from a GitHub Release markdown `body`, skipping headings and code fences.
pub fn parse_release_body_notes(body: &str) -> Vec<String> {
    let mut in_fence = false;
    let mut candidates = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        candidates.push(trimmed.to_string());
    }
    filter_plain_english_notes(&candidates)
}

/// Accumulates plain-English notes across all stable releases strictly greater than `current_version`
/// and less than or equal to `target_version`, ordered newest SemVer first.
pub fn accumulate_release_notes(
    releases: &[(String, Vec<String>)],
    current_version: &str,
    target_version: &str,
) -> Vec<String> {
    let Ok(cur) = parse_semver_version(current_version) else {
        return Vec::new();
    };
    let Ok(target) = parse_semver_version(target_version) else {
        return Vec::new();
    };
    let mut eligible: Vec<(Version, &[String])> = releases
        .iter()
        .filter_map(|(ver_str, notes)| {
            let ver = parse_semver_version(ver_str).ok()?;
            if !ver.pre.is_empty() {
                return None;
            }
            if ver.cmp_precedence(&cur) == std::cmp::Ordering::Greater
                && ver.cmp_precedence(&target) != std::cmp::Ordering::Greater
            {
                Some((ver, notes.as_slice()))
            } else {
                None
            }
        })
        .collect();

    eligible.sort_by(|a, b| b.0.cmp_precedence(&a.0));

    let flattened: Vec<String> = eligible
        .into_iter()
        .flat_map(|(_, notes)| notes.iter().cloned())
        .collect();
    filter_plain_english_notes(&flattened)
}
