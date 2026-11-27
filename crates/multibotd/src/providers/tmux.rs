use super::{AgentProvider, HeaderMeta, LineSlice, ParsedSlice};
use multibot_proto::{AgentKind, ServerEvent, SessionSummary};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const SESSION_FORMAT: &str = "#{session_name}\t#{session_activity}\t#{pane_current_command}\t#{pane_current_path}\t#{session_attached}";

pub fn parse_tmux_sessions_output(stdout: &str) -> Vec<SessionSummary> {
    stdout
        .lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split('\t').collect();
            if fields.len() != 5 || fields[0].is_empty() {
                return None;
            }
            let activity = fields[1].parse::<i64>().ok()?.checked_mul(1000)?;
            if activity < 0 || fields[4].parse::<u32>().is_err() {
                return None;
            }
            let (name, command, cwd) = (fields[0], fields[2], fields[3]);
            let short_cwd = Path::new(cwd)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(cwd);
            Some(SessionSummary {
                id: name.into(),
                agent: AgentKind::Shell,
                title: name.into(),
                cwd: (!cwd.is_empty()).then(|| cwd.into()),
                source_path: format!("tmux:{name}"),
                updated_at_ms: activity,
                working: !command.is_empty()
                    && !matches!(command, "bash" | "sh" | "zsh" | "fish" | "dash"),
                last_assistant_preview: Some(if short_cwd.is_empty() {
                    command.into()
                } else {
                    format!("{command} · {short_cwd}")
                }),
                ..Default::default()
            })
        })
        .collect()
}

pub fn discover_sessions() -> Vec<SessionSummary> {
    let Ok(output) = Command::new("tmux")
        .args(["list-sessions", "-F", SESSION_FORMAT])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_tmux_sessions_output(&String::from_utf8_lossy(&output.stdout))
}

pub fn empty_history(session_id: String) -> ServerEvent {
    ServerEvent::History {
        session_id,
        messages: Vec::new(),
        oldest_offset: Some(0),
        latest_offset: Some(0),
        has_more: false,
        file_size: Some(0),
        file_mtime_ms: None,
        probe_sha256: None,
        todos: Vec::new(),
    }
}

// Shells have no transcript files; terminal I/O belongs to SSH, not the chat parser.
pub struct TmuxProvider;
impl AgentProvider for TmuxProvider {
    fn kind(&self) -> AgentKind {
        AgentKind::Shell
    }
    fn discover_paths(
        &self,
        _: &Path,
        _: usize,
        _: &dyn Fn(&Path, i64) -> bool,
    ) -> Vec<(PathBuf, i64)> {
        Vec::new()
    }
    fn parse_lines(&self, _: &[LineSlice<'_>]) -> ParsedSlice {
        ParsedSlice::default()
    }
    fn extract_header_meta(&self, _: &Path) -> HeaderMeta {
        HeaderMeta::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_exact_names_activity_commands_and_paths() {
        let s = parse_tmux_sessions_output(
            "term-56d16ef56ce668ab2d7235d20e7ecd75\t1700000000\tagy\t/home/valentin/projects/multibot\t1\nfaceless-channels\t1700000001\tbash\t/home/valentin/projects\t0\n",
        );
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].id, "term-56d16ef56ce668ab2d7235d20e7ecd75");
        assert_eq!(s[0].source_path, format!("tmux:{}", s[0].id));
        assert_eq!(s[0].updated_at_ms, 1_700_000_000_000);
        assert_eq!(s[0].agent, AgentKind::Shell);
        assert!(s[0].working);
        assert!(!s[0].archived);
        assert_eq!(
            s[0].last_assistant_preview.as_deref(),
            Some("agy · multibot")
        );
        assert!(!s[1].working);
    }
    #[test]
    fn handles_empty_paths_idle_shells_and_unicode() {
        for command in ["bash", "sh", "zsh", "fish", "dash", ""] {
            let s = parse_tmux_sessions_output(&format!("日本語\t0\t{command}\t\t0\r\n"));
            assert_eq!(s.len(), 1);
            assert!(!s[0].working);
            assert!(s[0].cwd.is_none());
        }
        assert!(parse_tmux_sessions_output("btop\t1\tbtop\t/\t2")[0].working);
    }
    #[test]
    fn skips_malformed_rows_without_losing_valid_sessions() {
        let s = parse_tmux_sessions_output(
            "\nmissing fields\nx\tno\tbash\t/\t0\nx\t-1\tbash\t/\t0\nx\t9223372036854775807\tbash\t/\t0\n\t1\tbash\t/\t0\nx\t1\tbash\t/\tbad\nok\t2\tsh\t/a b\t1\n",
        );
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].title, "ok");
        assert_eq!(s[0].cwd.as_deref(), Some("/a b"));
    }
    #[test]
    fn actual_tmux_format_is_parseable() {
        // Optional live proof, also useful on the server without making CI depend on tmux.
        let Some(path) = std::env::var_os("MULTIBOT_TMUX_FIXTURE") else {
            return;
        };
        let raw = std::fs::read_to_string(path).unwrap();
        let s = parse_tmux_sessions_output(&raw);
        assert_eq!(s.len(), raw.lines().count());
        assert!(
            s.iter()
                .any(|s| s.id == "term-56d16ef56ce668ab2d7235d20e7ecd75")
        );
    }
}
