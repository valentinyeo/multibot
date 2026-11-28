use super::{AgentProvider, ArchiveStore, HeaderMeta, LineSlice, MetadataStore, ParsedSlice};
use crate::server_workspaces::ServerWorkspaceIndex;
use multibot_proto::{AgentKind, ServerEvent, SessionSummary};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const SESSION_FORMAT: &str = "#{session_name}\t#{session_activity}\t#{pane_current_command}\t#{pane_current_path}\t#{session_attached}\t#{window_name}";

pub fn parse_tmux_sessions_output(stdout: &str) -> Vec<SessionSummary> {
    stdout
        .lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split('\t').collect();
            if fields.len() != 6 || fields[0].is_empty() {
                return None;
            }
            let activity = fields[1].parse::<i64>().ok()?.checked_mul(1000)?;
            if activity < 0 || fields[4].parse::<u32>().is_err() {
                return None;
            }
            let (name, command, cwd, window) = (fields[0], fields[2], fields[3], fields[5]);
            let title = if !window.is_empty()
                && !matches!(
                    window.to_ascii_lowercase().as_str(),
                    "bash" | "sh" | "zsh" | "fish" | "dash" | "ash" | "tmux" | "cmd" | "pwsh"
                )
                && (name.starts_with("term-") || !window.eq_ignore_ascii_case(command))
            {
                window
            } else {
                name
            };
            let short_cwd = Path::new(cwd)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(cwd);
            Some(SessionSummary {
                id: name.into(),
                agent: AgentKind::Shell,
                title: title.into(),
                cwd: (!cwd.is_empty()).then(|| cwd.into()),
                source_path: format!("tmux:{name}"),
                shell_attached: Some(fields[4].parse::<u32>().ok()? > 0),
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

pub fn apply_metadata(
    shell: &mut SessionSummary,
    metadata: &MetadataStore,
    index: &ServerWorkspaceIndex,
    archive: &ArchiveStore,
) {
    shell.title = metadata
        .get(&shell.id)
        .and_then(|m| m.title.clone())
        .or_else(|| index.name_for(&shell.id).map(str::to_string))
        .unwrap_or_else(|| shell.title.clone());
    shell.workspace = index.workspace_for(&shell.id).map(str::to_string);
    if let Some(entry) = archive.archived.get(&shell.id) {
        shell.archived = true;
        shell.archive_kill_process = entry.kill_process;
    }
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
    fn shell_inventory_keeps_attachment_and_command_details() {
        let shells = parse_tmux_sessions_output("first\t1\tbtop\t/srv/projects\t0\tBTOP\nsecond\t2\tagy\t/srv/multibot\t2\tagy");
        assert_eq!(shells.len(), 2);
        assert_eq!(shells[0].shell_attached, Some(false));
        assert_eq!(shells[0].last_assistant_preview.as_deref(), Some("btop · projects"));
        assert_eq!(shells[1].shell_attached, Some(true));
        assert_eq!(shells[1].last_assistant_preview.as_deref(), Some("agy · multibot"));
    }

    #[test]
    fn rediscovered_shell_retains_saved_archive_after_daemon_restart() {
        let home =
            std::env::temp_dir().join(format!("multibot-shell-archive-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let mut archive = ArchiveStore::default();
        archive.archived.insert(
            "dissolved".into(),
            super::super::ArchivedEntry {
                kill_process: true,
                archived_at_ms: 1,
                agent: Some(AgentKind::Shell),
                source_path: Some("tmux:dissolved".into()),
            },
        );
        archive.save(&home).unwrap();
        let archive = ArchiveStore::load(&home);
        let mut shells = parse_tmux_sessions_output(
            "dissolved\t2\tbash\t/\t0\tbash\nactive\t3\tbash\t/\t0\tbash",
        );
        for shell in &mut shells {
            apply_metadata(
                shell,
                &MetadataStore::default(),
                &ServerWorkspaceIndex::default(),
                &archive,
            );
        }
        assert!(shells[0].archived && shells[0].archive_kill_process);
        assert!(!shells[1].archived);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn resolves_window_titles_without_changing_session_targets() {
        for (name, window, command, title) in [
            (
                "term-56d16ef56ce668ab2d7235d20e7ecd75",
                "Multi Bot AGY Shell",
                "agy",
                "Multi Bot AGY Shell",
            ),
            ("inne Runner 2", "bash", "bash", "inne Runner 2"),
            ("term-abc", "bash", "bash", "term-abc"),
            ("term-abc", "BTOP", "btop", "BTOP"),
            ("term-abc", "VPS Health", "agy", "VPS Health"),
            ("term-abc", "HT AGY Shell", "agy", "HT AGY Shell"),
            ("term-abc", "Postbox AGY Shell", "agy", "Postbox AGY Shell"),
            (
                "term-abc",
                "chromescribe ratstack",
                "agy",
                "chromescribe ratstack",
            ),
            ("chromescribe", "agy", "agy", "chromescribe"),
            ("inne-cro-2", "Custom title", "agy", "Custom title"),
            ("term-abc", "", "bash", "term-abc"),
        ] {
            let shells =
                parse_tmux_sessions_output(&format!("{name}\t1\t{command}\t/\t0\t{window}\n"));
            assert_eq!(shells.len(), 1);
            assert_eq!(shells[0].id, name);
            assert_eq!(shells[0].source_path, format!("tmux:{name}"));
            assert_eq!(shells[0].title, title);
        }
        for window in [
            "bash", "sh", "zsh", "fish", "dash", "ash", "tmux", "cmd", "pwsh", "BASH",
        ] {
            let shells =
                parse_tmux_sessions_output(&format!("term-abc\t1\tother\t/\t0\t{window}\n"));
            assert_eq!(shells[0].title, "term-abc");
        }
    }

    #[test]
    fn window_title_survives_metadata_fallback_but_manual_rename_wins() {
        let mut shell =
            parse_tmux_sessions_output("term-abc\t1\tagy\t/\t0\tMulti Bot AGY Shell").remove(0);
        let mut metadata = MetadataStore::default();
        let index = ServerWorkspaceIndex::default();
        apply_metadata(&mut shell, &metadata, &index, &ArchiveStore::default());
        assert_eq!(shell.title, "Multi Bot AGY Shell");
        metadata.sessions.insert(
            shell.id.clone(),
            super::super::SessionMetaEntry {
                title: Some("My Multibot name".into()),
                ..Default::default()
            },
        );
        apply_metadata(&mut shell, &metadata, &index, &ArchiveStore::default());
        assert_eq!(shell.title, "My Multibot name");
        assert_eq!(shell.id, "term-abc");
    }

    #[test]
    fn shell_titles_use_legacy_names_but_multibot_renames_win() {
        let dir = std::env::temp_dir().join(format!("multibot-shell-names-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let raw = r#"{
            "term-56d16ef56ce668ab2d7235d20e7ecd75": {"name":"Multi Bot AGY Shell","workspace":"Multi Bot"},
            "hypertask-vps:term-56d16ef56ce668ab2d7235d20e7ecd75": {"name":"Stale alias"},
            "hypertask-vps:term-4d82a90403033d51dd366d2b1d7c89c6": {"name":"HT AGY Shell","workspace":"HT Product"}
        }"#;
        let path = dir.join("chat-names.json");
        std::fs::write(&path, raw).unwrap();
        let index = ServerWorkspaceIndex::load_from_state_dir(&dir);
        let mut metadata = MetadataStore::default();
        let mut shells = parse_tmux_sessions_output(
            "term-56d16ef56ce668ab2d7235d20e7ecd75\t1\tbash\t/\t0\tTmux title\nterm-4d82a90403033d51dd366d2b1d7c89c6\t2\tagy\t/\t1\tAnother title\nterm-deadbeef\t3\tzsh\t/\t0\tzsh",
        );
        for shell in &mut shells {
            apply_metadata(shell, &metadata, &index, &ArchiveStore::default());
        }
        assert_eq!(shells[0].title, "Multi Bot AGY Shell");
        assert_eq!(shells[0].workspace.as_deref(), Some("Multi Bot"));
        assert_eq!(shells[1].title, "HT AGY Shell");
        assert_eq!(shells[1].workspace.as_deref(), Some("HT Product"));
        assert_eq!(shells[2].title, "term-deadbeef");
        metadata.sessions.insert(
            shells[0].id.clone(),
            super::super::SessionMetaEntry {
                title: Some("My Multibot name".into()),
                ..Default::default()
            },
        );
        apply_metadata(&mut shells[0], &metadata, &index, &ArchiveStore::default());
        assert_eq!(shells[0].title, "My Multibot name");
        assert_eq!(shells[0].source_path, format!("tmux:{}", shells[0].id));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn parses_exact_names_activity_commands_and_paths() {
        let s = parse_tmux_sessions_output(
            "term-56d16ef56ce668ab2d7235d20e7ecd75\t1700000000\tagy\t/home/valentin/projects/multibot\t1\tMulti Bot AGY Shell\nfaceless-channels\t1700000001\tbash\t/home/valentin/projects\t0\tbash\n",
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
            let s = parse_tmux_sessions_output(&format!("日本語\t0\t{command}\t\t0\t\r\n"));
            assert_eq!(s.len(), 1);
            assert!(!s[0].working);
            assert!(s[0].cwd.is_none());
        }
        assert!(parse_tmux_sessions_output("btop\t1\tbtop\t/\t2\tbtop")[0].working);
    }
    #[test]
    fn skips_malformed_rows_without_losing_valid_sessions() {
        let s = parse_tmux_sessions_output(
            "\nmissing fields\nx\tno\tbash\t/\t0\tbash\nx\t-1\tbash\t/\t0\tbash\nx\t9223372036854775807\tbash\t/\t0\tbash\n\t1\tbash\t/\t0\tbash\nx\t1\tbash\t/\tbad\tbash\nok\t2\tsh\t/a b\t1\tsh\n",
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
        let shell = s
            .iter()
            .find(|s| s.id == "term-56d16ef56ce668ab2d7235d20e7ecd75")
            .unwrap();
        assert_eq!(shell.title, "Multi Bot AGY Shell");
    }
}
