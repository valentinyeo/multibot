use super::{
    AgentProvider, HeaderMeta, LineSlice, ParsedSlice, TailActivity, is_safe_transcript_path,
    parse_normalized_messages, read_header_lines_bounded, system_time_ms, truncate_title,
};
use multibot_proto::{AgentKind, MessageRole};
use serde_json::{Value, json};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct CursorProvider;

#[cfg(test)]
#[path = "cursor_tests.rs"]
mod tests;

pub fn cursor_discovery_roots(home: &Path) -> Vec<PathBuf> {
    vec![home.join(".cursor/projects"), home.join(".multibot/cursor")]
}

pub fn is_native_transcript(path: &Path, root: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    let parts: Vec<_> = relative.components().collect();
    if parts
        .get(1)
        .is_none_or(|p| p.as_os_str() != "agent-transcripts")
    {
        return false;
    }
    parts.len() == 3
        || (parts.len() == 4 && path.file_stem() == path.parent().and_then(Path::file_name))
}

fn read_record(path: &Path) -> Option<Value> {
    let file = File::open(path).ok()?;
    if file.metadata().ok()?.len() > 256 * 1024 {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(256 * 1024).read_to_end(&mut bytes).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn native_cwd(path: &Path, id: &str) -> Option<String> {
    let home = path
        .ancestors()
        .find(|p| p.file_name().is_some_and(|n| n == ".cursor"))?
        .parent()?;
    let root = home.join(".local/state/multiprompt/agent-host");
    for entry in std::fs::read_dir(&root).ok()?.flatten() {
        let path = entry.path();
        let Some(stem) = path
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_suffix(".config.json"))
        else {
            continue;
        };
        if !is_safe_transcript_path(&path, &root) {
            continue;
        }
        let Some(config) = read_record(&path) else {
            continue;
        };
        if config.get("provider").and_then(Value::as_str) != Some("cursor") {
            continue;
        }
        let state_path = root.join(format!("{stem}.state.json"));
        let state = is_safe_transcript_path(&state_path, &root)
            .then(|| read_record(&state_path))
            .flatten();
        if config.get("sid").and_then(Value::as_str) == Some(id)
            || state
                .as_ref()
                .and_then(|v| v.get("native_session"))
                .and_then(Value::as_str)
                == Some(id)
        {
            return config
                .get("cwd")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
    }
    None
}

fn normalize(mut value: Value) -> Option<Value> {
    let kind = value.get("role").or_else(|| value.get("type"))?.as_str()?;
    if !matches!(kind, "user" | "assistant") {
        return None;
    }
    let kind = json!(kind);
    value["type"] = kind;
    Some(value)
}

impl AgentProvider for CursorProvider {
    fn kind(&self) -> AgentKind {
        AgentKind::Cursor
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
        for root in cursor_discovery_roots(home) {
            let native = root.ends_with("projects");
            for entry in WalkDir::new(&root).max_depth(4).into_iter().flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                    continue;
                }
                if native && !is_native_transcript(path, &root) {
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
        let id = parsed.session_id_hint.or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        });
        HeaderMeta {
            cwd: parsed
                .cwd
                .or_else(|| id.as_deref().and_then(|id| native_cwd(path, id))),
            session_id_hint: id,
            title_hint: parsed.title_hint.or_else(|| {
                parsed
                    .messages
                    .iter()
                    .find(|m| m.role == MessageRole::User)
                    .map(|m| truncate_title(&m.text, 64))
            }),
            model: parsed.model,
        }
    }

    fn parse_lines(&self, lines: &[LineSlice<'_>]) -> ParsedSlice {
        let mut meta = HeaderMeta::default();
        let mut ended = false;
        let mut parsed = parse_normalized_messages(lines, "cursor", |value| {
            match value.get("type").and_then(Value::as_str) {
                Some("multibot_meta") => {
                    meta.session_id_hint = value
                        .get("session_id")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    meta.cwd = value.get("cwd").and_then(Value::as_str).map(str::to_string);
                    meta.title_hint = value
                        .get("title")
                        .and_then(Value::as_str)
                        .map(|s| truncate_title(s, 64));
                }
                Some("result") => ended = true,
                _ => {}
            }
            let record = normalize(value);
            if record.is_some() {
                ended = false;
            }
            record
        });
        parsed.session_id_hint = meta.session_id_hint;
        parsed.cwd = meta.cwd.or(parsed.cwd);
        parsed.title_hint = meta.title_hint.or(parsed.title_hint);
        if ended {
            parsed.tail_activity = TailActivity::Idle;
        }
        parsed
    }
}
