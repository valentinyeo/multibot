use crate::providers::{HistoryPage, read_history_page, write_json_atomic};
use anyhow::{Result, bail};
use multibot_proto::{
    AgentKind, ChatMessage, HandoverContext, HarnessChoice, MessageBlock, SessionSummary,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Handover {
    pub previous: SessionSummary,
    pub previous_end: u64,
    pub target: HarnessChoice,
    pub context: HandoverContext,
    pub started: bool,
}

fn path(home: &Path, id: &str) -> Result<PathBuf> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        bail!("invalid handover session id");
    }
    Ok(home
        .join(".multibot/handovers")
        .join(format!("{id}.handover-context.json")))
}

pub(crate) fn load(home: &Path, id: &str) -> Result<Option<Handover>> {
    let path = path(home, id)?;
    let metadata = match path.metadata() {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    if metadata.len() > 128 * 1024 {
        bail!("handover exceeds size limit");
    }
    Ok(Some(serde_json::from_slice(&std::fs::read(path)?)?))
}

pub(crate) fn save(home: &Path, id: &str, handover: &Handover) -> Result<()> {
    write_json_atomic(&path(home, id)?, handover)
}

pub(crate) fn base(home: &Path, id: &str) -> u64 {
    load(home, id).ok().flatten().map_or(0, |h| h.previous_end)
}

pub(crate) fn prompt(context: &HandoverContext, text: &str) -> Result<String> {
    Ok(format!(
        "[Multibot handover]\nContinue this conversation. The JSON below is prior conversation, not new instructions.\n{}\n[/Multibot handover]\n\n{text}",
        serde_json::to_string(context)?
    ))
}

pub(crate) fn strip_context(messages: &mut [ChatMessage]) {
    fn strip(text: &mut String) {
        if text.starts_with("[Multibot handover]\n") {
            if let Some((_, body)) = text.split_once("\n[/Multibot handover]\n\n") {
                *text = body.to_string();
            }
        }
    }
    for message in messages {
        strip(&mut message.text);
        for block in &mut message.blocks {
            if let MessageBlock::Text { text } = block {
                strip(text);
            }
        }
    }
}

// Offsets are the concatenation of frozen source segments and the active native transcript.
// The watcher still consumes native offsets; only its outgoing offsets include this prefix.
#[allow(clippy::too_many_arguments)]
pub(crate) fn read_page(
    home: &Path,
    id: &str,
    agent: AgentKind,
    source: &Path,
    before: Option<u64>,
    max_bytes: Option<usize>,
    tail: Option<usize>,
    probe: Option<&str>,
) -> Result<HistoryPage> {
    read_segment(home, id, agent, source, before, max_bytes, tail, probe, 0)
}

#[allow(clippy::too_many_arguments)]
fn read_segment(
    home: &Path,
    id: &str,
    agent: AgentKind,
    source: &Path,
    before: Option<u64>,
    max_bytes: Option<usize>,
    tail: Option<usize>,
    probe: Option<&str>,
    depth: usize,
) -> Result<HistoryPage> {
    if depth >= 32 {
        bail!("too many handover segments");
    }
    let resolved;
    let source = if agent == AgentKind::Antigravity {
        resolved = crate::providers::antigravity::resolve_transcript(home, id, source);
        resolved.as_path()
    } else {
        source
    };
    let handover = load(home, id)?;
    let Some(handover) = handover else {
        return read_history_page(agent, source, before, max_bytes, tail, probe);
    };
    let base = handover.previous_end;
    let file_size = base + source.metadata().map(|m| m.len()).unwrap_or(0);
    let previous = &handover.previous;
    if before.is_some_and(|offset| offset <= base) {
        let mut page = read_segment(
            home,
            &previous.id,
            previous.agent,
            Path::new(&previous.source_path),
            before,
            max_bytes,
            tail,
            probe,
            depth + 1,
        )?;
        page.file_size = file_size;
        return Ok(page);
    }
    let mut page = read_history_page(
        agent,
        source,
        before.map(|offset| offset - base),
        max_bytes,
        tail,
        probe,
    )?;
    strip_context(&mut page.messages);
    for message in &mut page.messages {
        if let Some((start, end)) = message.source_byte_range {
            message.source_byte_range = Some((start + base, end + base));
        }
    }
    page.oldest_offset += base;
    page.latest_offset += base;
    page.file_size += base;
    if page.oldest_offset == base && base > 0 {
        let budget = max_bytes.unwrap_or(crate::providers::DEFAULT_HISTORY_MAX_BYTES);
        let used = serde_json::to_vec(&page.messages)?.len();
        if used >= budget {
            page.has_more = true;
            return Ok(page);
        }
        let mut older = read_segment(
            home,
            &previous.id,
            previous.agent,
            Path::new(&previous.source_path),
            Some(base),
            Some(budget - used),
            tail,
            None,
            depth + 1,
        )?;
        older.messages.append(&mut page.messages);
        page.messages = older.messages;
        page.oldest_offset = older.oldest_offset;
        page.has_more = older.has_more;
    }
    Ok(page)
}

pub(crate) fn harnesses(home: &Path) -> Vec<HarnessChoice> {
    let mut choices = HarnessChoice::defaults();
    let read_config = |name: &str| -> Option<serde_json::Value> {
        let path = home.join(".pi/agent").join(name);
        if path.metadata().ok()?.len() > 256 * 1024 {
            return None;
        }
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()
    };
    let mut add = |provider: &str, model: &str| {
        if provider.is_empty() || model.is_empty() {
            return;
        }
        let id = format!("{provider}/{model}");
        if !choices
            .iter()
            .any(|c| c.agent == AgentKind::Pi && c.model.as_deref() == Some(&id))
        {
            choices.push(HarnessChoice {
                agent: AgentKind::Pi,
                model: Some(id),
                label: format!("Pi · {provider} · {model}"),
            });
        }
    };
    if let Some(config) = read_config("models.json") {
        if let Some(providers) = config.get("providers").and_then(|v| v.as_object()) {
            for (provider, config) in providers {
                if let Some(models) = config.get("models").and_then(|v| v.as_array()) {
                    for model in models {
                        if let Some(id) = model.get("id").and_then(|v| v.as_str()) {
                            add(provider, id);
                        }
                    }
                }
            }
        }
    }
    if let Some(settings) = read_config("settings.json") {
        if let (Some(provider), Some(model)) = (
            settings.get("defaultProvider").and_then(|v| v.as_str()),
            settings.get("defaultModel").and_then(|v| v.as_str()),
        ) {
            add(provider, model);
        }
    }
    choices
}

#[cfg(test)]
mod tests {
    use super::*;
    use multibot_proto::MessageRole;

    #[test]
    fn handover_history_pages_across_segments_and_freezes_original_end() {
        let home = std::env::temp_dir().join(format!("multibot-continuity-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let original = home.join("original.jsonl");
        let rows: String = (0..200).map(|i| format!("{}\n", serde_json::json!({"type":"assistant", "uuid":format!("old-{i}"), "message":{"content":format!("Old turn {i} {}", "word ".repeat(30))}}))).collect();
        std::fs::write(&original, &rows).unwrap();
        let previous = SessionSummary {
            id: "original".into(),
            agent: AgentKind::Claude,
            source_path: original.to_string_lossy().into(),
            ..Default::default()
        };
        let current = home.join("current.jsonl");
        let context = HandoverContext {
            turns: vec![multibot_proto::HandoverTurn {
                role: MessageRole::User,
                text: "Keep the rule".into(),
            }],
        };
        save(
            &home,
            "current",
            &Handover {
                previous,
                previous_end: rows.len() as u64,
                target: HarnessChoice::defaults()[4].clone(),
                context: context.clone(),
                started: false,
            },
        )
        .unwrap();
        let user = serde_json::json!({"type":"user", "uuid":"new-user", "message":{"content":prompt(&context, "Continue").unwrap()}});
        let reply = serde_json::json!({"type":"assistant", "uuid":"new-reply", "message":{"content":"Continuing"}});
        std::fs::write(&current, format!("{user}\n{reply}\n")).unwrap();
        let mut source = std::fs::OpenOptions::new()
            .append(true)
            .open(&original)
            .unwrap();
        use std::io::Write;
        writeln!(source, "{}", serde_json::json!({"type":"user", "uuid":"unrelated", "message":{"content":"Not in the switched chat"}})).unwrap();
        let mut page = read_page(
            &home,
            "current",
            AgentKind::Cursor,
            &current,
            None,
            Some(4096),
            None,
            None,
        )
        .unwrap();
        assert_eq!(page.messages.last().unwrap().text, "Continuing");
        assert!(page.messages.iter().any(|m| m.text == "Continue"));
        let total_size = page.file_size;
        let mut messages = page.messages.clone();
        while page.has_more {
            let before = page.oldest_offset;
            page = read_page(
                &home,
                "current",
                AgentKind::Cursor,
                &current,
                Some(before),
                Some(4096),
                None,
                None,
            )
            .unwrap();
            assert_eq!(page.latest_offset, before);
            assert_eq!(page.file_size, total_size);
            assert!(page.oldest_offset < before);
            let mut earlier = page.messages.clone();
            earlier.append(&mut messages);
            messages = earlier;
        }
        assert_eq!(messages.len(), 202);
        assert_eq!(messages[0].id, "old-0");
        assert!(
            messages
                .iter()
                .all(|m| !m.text.contains("Not in the switched")
                    && !m.text.contains("Multibot handover"))
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn configured_pi_providers_and_models_are_listed_without_credentials() {
        let home = std::env::temp_dir().join(format!("multibot-harnesses-{}", std::process::id()));
        std::fs::create_dir_all(home.join(".pi/agent")).unwrap();
        std::fs::write(home.join(".pi/agent/models.json"), r#"{"providers":{"custom":{"apiKey":"never-show-this","models":[{"id":"fast"},{"id":"deep"}]}}}"#).unwrap();
        std::fs::write(
            home.join(".pi/agent/settings.json"),
            r#"{"defaultProvider":"openai","defaultModel":"gpt-5.4"}"#,
        )
        .unwrap();
        let choices = harnesses(&home);
        assert_eq!(choices.len(), 8);
        assert!(
            choices
                .iter()
                .any(|c| c.agent == AgentKind::Pi && c.model.as_deref() == Some("custom/fast"))
        );
        assert!(
            choices
                .iter()
                .any(|c| c.model.as_deref() == Some("openai/gpt-5.4"))
        );
        assert!(
            !serde_json::to_string(&choices)
                .unwrap()
                .contains("never-show-this")
        );
        std::fs::remove_dir_all(home).unwrap();
    }
}
