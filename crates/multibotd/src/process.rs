use crate::providers::antigravity::{
    AgyStreamAccumulator, AgyStreamOutput, append_history_record, hosted_history_path,
    is_conversation_id, native_brain_transcript,
};
use crate::providers::claude::sanitize_claude_project_dir;
use crate::providers::{
    ArchiveStore, ArchivedEntry, DiscoveryContext, LiveActivity, MetadataStore, SessionMetaEntry,
    now_ms, sha256_hex, strip_base64_cow, summarize_session_by_agent, system_time_ms,
};
use anyhow::{Context, Result, anyhow};
use multibot_proto::{
    AgentKind, EffortLevel, PermissionLevel, ServerEvent, SessionOptions, SessionSummary,
    ToolStatus,
};
use serde_json::json;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

pub const MAX_QUEUED_PROMPTS_PER_SESSION: usize = 20;
pub const MAX_SESSION_TITLE_CHARS: usize = 63;

fn validate_client_tty(tty: &str) -> Result<&str> {
    let tty = tty.trim();
    if !tty.starts_with("/dev/")
        || tty.len() > 1024
        || tty.chars().any(|c| c.is_control() || c.is_whitespace())
        || Path::new(tty)
            .components()
            .skip(1)
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(anyhow!("Invalid shell client TTY"));
    }
    Ok(tty)
}

async fn switch_shell_client_at(
    home: &Path,
    client_id: &str,
    session_id: &str,
    socket: Option<&str>,
) -> Result<()> {
    if !multibot_proto::valid_shell_client_id(client_id) {
        return Err(anyhow!("Invalid shell client ID"));
    }
    if !multibot_proto::valid_shell_session_id(session_id) {
        return Err(anyhow!("Invalid tmux session ID"));
    }
    let registration = home
        .join(".multibot/clients")
        .join(format!("{client_id}.tty"));
    // A console HWND can appear before SSH has registered and attached its remote TTY.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let result = async {
            let content = tokio::fs::read_to_string(&registration).await?;
            if content.trim().is_empty() {
                return Err(std::io::Error::from(std::io::ErrorKind::WouldBlock).into());
            }
            let tty = validate_client_tty(&content)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::FileTypeExt;
                if !tokio::fs::metadata(tty).await?.file_type().is_char_device() {
                    return Err(anyhow!("Shell client TTY is not a device"));
                }
            }
            let mut command = Command::new("tmux");
            if let Some(socket) = socket {
                command.args(["-S", socket]);
            }
            let output = command
                .env_remove("TMUX")
                .args(["switch-client", "-c", tty, "-t", &format!("={session_id}")])
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output()
                .await?;
            if !output.status.success() {
                return Err(anyhow!(
                    "tmux switch-client: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            Ok(tty.to_owned())
        };
        let result = tokio::time::timeout_at(deadline, result)
            .await
            .unwrap_or_else(|_| Err(anyhow!("Timed out switching shell client")));
        match result {
            Ok(tty) => {
                let target = format!("={session_id}");
                for args in [
                    &["set-window-option", "-t", &target, "window-size", "latest"][..],
                    &["refresh-client", "-t", &tty][..],
                ] {
                    let mut command = Command::new("tmux");
                    if let Some(socket) = socket {
                        command.args(["-S", socket]);
                    }
                    let _ = tokio::time::timeout(
                        Duration::from_secs(1),
                        command
                            .env_remove("TMUX")
                            .args(args)
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .kill_on_drop(true)
                            .status(),
                    )
                    .await;
                }
                return Ok(());
            }
            Err(error) => {
                let waiting_for_attach = error.downcast_ref::<std::io::Error>().is_some_and(|e| {
                    matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::WouldBlock
                    )
                }) || error.to_string().contains("can't find client");
                if waiting_for_attach && tokio::time::Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
                let _ = tokio::fs::remove_file(&registration).await;
                return Err(error);
            }
        }
    }
}

struct ActiveProcess {
    pid: u32,
    cwd: Option<String>,
    source_path: PathBuf,
    stdin_tx: Option<mpsc::UnboundedSender<String>>,
    busy: bool,
    interrupted: bool,
}

#[derive(Debug, Clone)]
struct QueuedPrompt {
    message_id: String,
    agent: AgentKind,
    cwd: Option<String>,
    text: String,
    options: Option<SessionOptions>,
}

#[derive(Default)]
struct ProcessState {
    active: HashMap<String, ActiveProcess>,
    queues: HashMap<String, VecDeque<QueuedPrompt>>,
}

pub struct ProcessManager {
    home: PathBuf,
    out_tx: mpsc::UnboundedSender<ServerEvent>,
    state: Arc<Mutex<ProcessState>>,
    seq: AtomicU64,
    #[cfg(test)]
    test_bin_override: Mutex<Option<PathBuf>>,
}

impl ProcessManager {
    pub fn new(home: PathBuf, out_tx: mpsc::UnboundedSender<ServerEvent>) -> Arc<Self> {
        Arc::new(Self {
            home,
            out_tx,
            state: Arc::new(Mutex::new(ProcessState::default())),
            seq: AtomicU64::new(1),
            #[cfg(test)]
            test_bin_override: Mutex::new(None),
        })
    }

    #[cfg(test)]
    fn set_test_bin_override(&self, path: Option<PathBuf>) {
        *self
            .test_bin_override
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = path;
    }

    fn resolve_agent_binary(&self, bin_name: &str) -> PathBuf {
        #[cfg(test)]
        if let Some(ref p) = *self
            .test_bin_override
            .lock()
            .unwrap_or_else(|e| e.into_inner())
        {
            return p.clone();
        }
        resolve_binary(&self.home, bin_name)
    }

    pub fn active_session_ids(&self) -> HashSet<String> {
        let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .active
            .iter()
            .filter(|(_, p)| p.busy)
            .map(|(k, _)| k.clone())
            .collect()
    }

    pub fn active_session_pids(&self) -> HashMap<String, u32> {
        let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .active
            .iter()
            .map(|(id, process)| (id.clone(), process.pid))
            .collect()
    }

    /// Sessions this daemon holds open, busy or idle-connected.
    ///
    /// A chat nobody is prompting is still a chat the person has open. The desktop lists these,
    /// so leaving them out would hide live work while the transcript history stays on disk.
    pub fn connected_session_ids(&self) -> HashSet<String> {
        let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard.active.keys().cloned().collect()
    }

    pub fn is_session_busy(&self, session_id: &str) -> bool {
        let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard.active.get(session_id).is_some_and(|p| p.busy)
    }

    /// Whether `session_id` has a live `agy` process whose stdin is still open.
    ///
    /// `busy` is about the turn, not about whether the process can take another message. A
    /// hosted `agy` keeps reading stdin after its turn ends and while background tools run.
    fn session_accepts_live_stdin(&self, session_id: &str) -> bool {
        let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .active
            .get(session_id)
            .is_some_and(|p| !p.interrupted && p.stdin_tx.is_some())
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    pub async fn send_prompt(
        self: &Arc<Self>,
        session_id: &str,
        agent: AgentKind,
        cwd: Option<&str>,
        text: &str,
        options: Option<SessionOptions>,
    ) -> Result<String> {
        if text.trim().is_empty() {
            return Err(anyhow!("prompt text is empty"));
        }
        if agent != AgentKind::Antigravity || !self.session_accepts_live_stdin(session_id) {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(interrupted) = state
                .active
                .get(session_id)
                .filter(|p| p.interrupted || p.busy)
                .map(|p| p.interrupted)
            {
                let queue = state.queues.entry(session_id.to_string()).or_default();
                if queue.len() >= MAX_QUEUED_PROMPTS_PER_SESSION {
                    return Err(anyhow!(
                        "session prompt queue full (max {MAX_QUEUED_PROMPTS_PER_SESSION})"
                    ));
                }
                let prompt = QueuedPrompt {
                    message_id: format!("q-{}-{}", now_ms(), self.next_seq()),
                    agent,
                    cwd: cwd.map(String::from),
                    text: text.to_string(),
                    options,
                };
                if interrupted {
                    queue.push_front(prompt);
                } else {
                    queue.push_back(prompt);
                }
                return Ok("queued behind active turn".into());
            }
        }
        let result =
            self.dispatch_prompt_internal(session_id, agent, cwd, text, false, options.as_ref());
        if result.is_ok() {
            self.notify_session_working(session_id, true);
        }
        result
    }

    // An agent can finish without touching its transcript, so filesystem watches alone
    // cannot advance the desktop's pending queue.
    fn notify_session_working(&self, session_id: &str, working: bool) {
        let _ = self.out_tx.send(ServerEvent::HistoryAppend {
            session_id: session_id.into(),
            messages: Vec::new(),
            latest_offset: None,
            working: Some(working),
            todos: Vec::new(),
            probe_sha256: None,
        });
    }

    fn dispatch_prompt_internal(
        self: &Arc<Self>,
        session_id: &str,
        agent: AgentKind,
        cwd: Option<&str>,
        text: &str,
        is_new_session: bool,
        options: Option<&SessionOptions>,
    ) -> Result<String> {
        let mut handover = crate::continuity::load(&self.home, session_id)?;
        let first_turn = handover.as_ref().is_some_and(|h| !h.started);
        let enriched = if let Some(h) = handover.as_ref().filter(|h| !h.started) {
            crate::continuity::prompt(&h.context, text)?
        } else {
            text.to_string()
        };
        let fallback = SessionOptions {
            model: handover.as_ref().and_then(|h| h.target.model.clone()),
            ..Default::default()
        };
        let result = self.dispatch_prompt_native(
            session_id,
            agent,
            cwd,
            &enriched,
            is_new_session || first_turn,
            options.or(Some(&fallback)),
        )?;
        if first_turn {
            if let Some(h) = handover.as_mut() {
                h.started = true;
                crate::continuity::save(&self.home, session_id, h)?;
            }
        }
        Ok(result)
    }

    pub(crate) fn switch_harness(
        self: &Arc<Self>,
        previous: SessionSummary,
        target: multibot_proto::HarnessChoice,
        context: multibot_proto::HandoverContext,
    ) -> Result<()> {
        if previous.agent == AgentKind::Shell || target.agent == AgentKind::Shell {
            return Err(anyhow!("Shell sessions cannot switch harness"));
        }
        if previous.working
            || self.is_session_busy(&previous.id)
            || self
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .queues
                .get(&previous.id)
                .is_some_and(|q| !q.is_empty())
        {
            return Err(anyhow!(
                "Wait for the current turn and queued prompts before switching"
            ));
        }
        let cwd = previous
            .cwd
            .as_deref()
            .ok_or_else(|| anyhow!("Chat has no working directory"))?;
        validate_cwd(cwd).map_err(|e| anyhow!(e))?;
        if context.turns.len() > 24
            || context
                .turns
                .iter()
                .map(|t| t.text.chars().count())
                .sum::<usize>()
                > 16_000
        {
            return Err(anyhow!("handover context exceeds limit"));
        }
        validate_options(
            target.agent,
            Some(&SessionOptions {
                model: target.model.clone(),
                ..Default::default()
            }),
        )?;
        let previous_page = crate::continuity::read_page(
            &self.home,
            &previous.id,
            previous.agent,
            Path::new(&previous.source_path),
            None,
            Some(1),
            Some(1),
            None,
        )?;
        let id = mint_uuid_v4(self.next_seq(), cwd);
        let source_path = if target.agent == AgentKind::Codex {
            self.home
                .join(".multibot/codex")
                .join(format!("{id}.jsonl"))
        } else {
            self.resolve_session_source_path(&id, target.agent, Some(cwd))
        };
        if let Some(parent) = source_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let session = SessionSummary {
            id: id.clone(),
            agent: target.agent,
            source_path: source_path.to_string_lossy().into_owned(),
            working: false,
            model: target.model.clone(),
            effort: None,
            context_tokens: None,
            context_limit: None,
            ..previous.clone()
        };
        let handover = crate::continuity::Handover {
            previous: previous.clone(),
            previous_end: previous_page.file_size,
            target,
            context,
            started: false,
        };
        crate::continuity::save(&self.home, &id, &handover)?;
        let mut metadata = MetadataStore::load(&self.home);
        metadata.sessions.insert(
            id.clone(),
            SessionMetaEntry {
                title: Some(session.title.clone()),
                cwd: session.cwd.clone(),
                agent: Some(session.agent),
                source_path: Some(session.source_path.clone()),
                native_session_id: None,
                workspace: session.workspace.clone(),
                updated_at_ms: now_ms(),
            },
        );
        metadata.save(&self.home)?;
        let mut archive = ArchiveStore::load(&self.home);
        archive.archived.insert(
            previous.id.clone(),
            ArchivedEntry {
                kill_process: false,
                archived_at_ms: now_ms(),
                agent: Some(previous.agent),
                source_path: Some(previous.source_path.clone()),
            },
        );
        if let Err(err) = archive.save(&self.home) {
            metadata.sessions.remove(&id);
            metadata.save(&self.home)?;
            return Err(err);
        }
        let _ = self
            .out_tx
            .send(ServerEvent::HarnessSwitched { previous, session });
        Ok(())
    }

    fn dispatch_prompt_native(
        self: &Arc<Self>,
        session_id: &str,
        agent: AgentKind,
        cwd: Option<&str>,
        text: &str,
        is_new_session: bool,
        options: Option<&SessionOptions>,
    ) -> Result<String> {
        validate_options(agent, options)?;
        // Fast path: persistent hosted `agy` process already running with open stdin.
        if agent == AgentKind::Antigravity {
            let existing_tx = {
                let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(proc) = guard.active.get_mut(session_id)
                    && !proc.interrupted
                    && let Some(ref tx) = proc.stdin_tx
                {
                    proc.busy = true;
                    Some((
                        tx.clone(),
                        proc.source_path.clone(),
                        proc.pid,
                        proc.cwd.clone(),
                    ))
                } else {
                    None
                }
            };
            if let Some((tx, history_path, pid, proc_cwd)) = existing_tx {
                let wire = serde_json::to_string(&json!({
                    "event": "user",
                    "message": { "role": "user", "content": text }
                }))? + "\n";
                if tx.send(wire).is_ok() {
                    let effective_cwd = cwd.map(String::from).or(proc_cwd);
                    // `agy --conversation <uuid>` appends the turn to its own brain transcript.
                    // Mirroring the row into `~/.multibot/history/` would show it twice, and
                    // the desktop is watching whichever file the session actually lives in.
                    if history_path.starts_with(self.home.join(".multibot"))
                        && crate::providers::antigravity::resolve_transcript(
                            &self.home,
                            session_id,
                            &history_path,
                        ) == history_path
                    {
                        append_agy_user_row(
                            &history_path,
                            session_id,
                            effective_cwd.as_deref(),
                            text,
                            self.next_seq(),
                        )?;
                    }
                    return Ok(format!("dispatched agy stream-json (pid {pid})"));
                }
                // Channel closed — remove stale active entry (only if same pid) and fall through
                // to spawn fresh agy process without duplicating the user history row.
                let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if guard.active.get(session_id).is_some_and(|p| p.pid == pid) {
                    guard.active.remove(session_id);
                }
            }
        }

        let bin_name = match agent {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::Antigravity => "agy",
            AgentKind::Shell => return Err(anyhow!("Use the embedded terminal for shell input")),
            AgentKind::Pi => "pi",
            AgentKind::Cursor => "cursor-agent",
        };
        let bin_path = self.resolve_agent_binary(bin_name);

        match agent {
            AgentKind::Shell => Err(anyhow!("Use the embedded terminal for shell input")),
            AgentKind::Claude => {
                let args = build_claude_args(session_id, text, is_new_session, options)?;
                let mut cmd = Command::new(&bin_path);
                cmd.env("MULTIBOT_SESSION_ID", session_id);
                cmd.args(&args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                if let Some(dir) = cwd
                    && !dir.trim().is_empty()
                {
                    cmd.current_dir(dir);
                }
                let child = spawn_with_retry(&mut cmd)
                    .with_context(|| format!("spawning {}", bin_path.display()))?;
                let pid = child.id().unwrap_or(0);
                let source_path = self.resolve_session_source_path(session_id, agent, cwd);
                self.register_one_shot_child(
                    session_id.to_string(),
                    cwd.map(String::from),
                    source_path,
                    child,
                );
                Ok(format!("dispatched claude (pid {pid})"))
            }
            AgentKind::Codex => {
                let source_path = self.resolve_session_source_path(session_id, agent, cwd);
                let owned = source_path.starts_with(self.home.join(".multibot"));
                let metadata = MetadataStore::load(&self.home);
                let native = metadata
                    .get(session_id)
                    .and_then(|m| m.native_session_id.as_deref())
                    .unwrap_or(session_id);
                let mut args = build_codex_args(native, text, is_new_session, options)?;
                if owned {
                    args.insert(1, "--json".into());
                }
                let mut cmd = Command::new(&bin_path);
                cmd.env("MULTIBOT_SESSION_ID", session_id);
                cmd.args(&args)
                    .stdin(Stdio::null())
                    .stdout(if owned { Stdio::piped() } else { Stdio::null() })
                    .stderr(Stdio::null());
                if let Some(dir) = cwd
                    && !dir.trim().is_empty()
                {
                    cmd.current_dir(dir);
                }
                let mut child = spawn_with_retry(&mut cmd)
                    .with_context(|| format!("spawning {}", bin_path.display()))?;
                let pid = child.id().unwrap_or(0);
                let source_path = self.resolve_session_source_path(session_id, agent, cwd);
                if owned {
                    if is_new_session {
                        append_history_record(
                            &source_path,
                            &json!({"type":"session_meta", "payload":{"id":session_id, "cwd":cwd}}),
                        )?;
                    }
                    append_history_record(
                        &source_path,
                        &json!({"type":"response_item", "payload":{"type":"message", "role":"user", "content":[{"type":"input_text", "text":text}]}}),
                    )?;
                    let stdout = child.stdout.take().expect("piped Codex stdout");
                    let mgr = Arc::clone(self);
                    let sid = session_id.to_string();
                    let sink = source_path.clone();
                    self.state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .active
                        .insert(
                            sid.clone(),
                            ActiveProcess {
                                pid,
                                cwd: cwd.map(String::from),
                                source_path,
                                stdin_tx: None,
                                busy: true,
                                interrupted: false,
                            },
                        );
                    tokio::spawn(async move {
                        read_harness_output(stdout, &mgr.home, &sid, &sink, true, AgentKind::Codex)
                            .await;
                        let _ = child.wait().await;
                        mgr.on_process_exit(&sid, pid);
                    });
                    return Ok(format!("dispatched codex (pid {pid})"));
                }
                self.register_one_shot_child(
                    session_id.to_string(),
                    cwd.map(String::from),
                    source_path,
                    child,
                );
                Ok(format!("dispatched codex (pid {pid})"))
            }
            AgentKind::Pi | AgentKind::Cursor => {
                if session_id.is_empty()
                    || !session_id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                {
                    return Err(anyhow!("invalid session id"));
                }
                let source_path = self.resolve_session_source_path(session_id, agent, cwd);
                let metadata = MetadataStore::load(&self.home);
                let native_id = metadata
                    .get(session_id)
                    .and_then(|m| m.native_session_id.as_deref());
                let mut args = build_pi_cursor_args(
                    agent,
                    session_id,
                    &source_path,
                    native_id,
                    text,
                    is_new_session,
                )?;
                if agent == AgentKind::Cursor
                    && matches!(
                        options.and_then(|o| o.permission),
                        None | Some(PermissionLevel::FullAccess)
                    )
                {
                    args.insert(0, "--force".into());
                }
                if agent == AgentKind::Pi {
                    if let Some(model) = options.and_then(|o| o.model.as_ref()) {
                        args.splice(0..0, ["--model".into(), model.clone()]);
                    }
                }
                let mut cmd = Command::new(&bin_path);
                cmd.env("MULTIBOT_SESSION_ID", session_id);
                cmd.args(&args).stdin(Stdio::null()).stderr(Stdio::null());
                if agent == AgentKind::Cursor {
                    cmd.stdout(Stdio::piped());
                } else {
                    cmd.stdout(Stdio::null());
                }
                if let Some(dir) = cwd.filter(|dir| !dir.trim().is_empty()) {
                    cmd.current_dir(dir);
                }
                if let Some(parent) = source_path
                    .parent()
                    .filter(|p| p.starts_with(self.home.join(".multibot")))
                {
                    std::fs::create_dir_all(parent)?;
                }
                let owned = source_path.starts_with(self.home.join(".multibot"));
                if agent == AgentKind::Pi && owned && !source_path.exists() {
                    append_history_record(
                        &source_path,
                        &json!({"type":"session", "version":3,
                        "id":session_id, "cwd":cwd, "timestamp":chrono::Utc::now().to_rfc3339()}),
                    )?;
                }
                let mut child = spawn_with_retry(&mut cmd)
                    .with_context(|| format!("spawning {}", bin_path.display()))?;
                let pid = child.id().unwrap_or(0);
                if agent == AgentKind::Cursor {
                    if owned
                        && let Err(err) = append_history_record(
                            &source_path,
                            &json!({"type":"user", "message":{"content":text}, "timestamp":chrono::Utc::now().to_rfc3339()}),
                        )
                    {
                        let _ = child.start_kill();
                        tokio::spawn(async move {
                            let _ = child.wait().await;
                        });
                        return Err(err);
                    }
                    let stdout = child.stdout.take().expect("piped Cursor stdout");
                    let mgr = Arc::clone(self);
                    let sid = session_id.to_string();
                    let sink = source_path.clone();
                    self.state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .active
                        .insert(
                            sid.clone(),
                            ActiveProcess {
                                pid,
                                cwd: cwd.map(String::from),
                                source_path: sink.clone(),
                                stdin_tx: None,
                                busy: true,
                                interrupted: false,
                            },
                        );
                    tokio::spawn(async move {
                        read_harness_output(
                            stdout,
                            &mgr.home,
                            &sid,
                            &sink,
                            owned,
                            AgentKind::Cursor,
                        )
                        .await;
                        let _ = child.wait().await;
                        mgr.on_process_exit(&sid, pid);
                    });
                    return Ok(format!("dispatched Cursor (pid {pid})"));
                }
                self.register_one_shot_child(
                    session_id.to_string(),
                    cwd.map(String::from),
                    source_path,
                    child,
                );
                Ok(format!("dispatched {} (pid {pid})", agent.label()))
            }
            AgentKind::Antigravity => {
                let conversation = self.resolve_agy_conversation(session_id, is_new_session);
                let history_path = self.agy_history_sink(session_id, conversation.as_deref());

                let mut args = vec![
                    "--input-format".to_string(),
                    "stream-json".to_string(),
                    "--output-format".to_string(),
                    "stream-json".to_string(),
                ];
                if matches!(
                    options.and_then(|o| o.permission),
                    None | Some(PermissionLevel::FullAccess)
                ) {
                    args.push("--dangerously-skip-permissions".to_string());
                }
                args.push("--print=".to_string());
                if let Some(conv_id) = conversation.as_deref() {
                    args.push("--conversation".to_string());
                    args.push(conv_id.to_string());
                }

                let wire = serde_json::to_string(&json!({
                    "event": "user",
                    "message": { "role": "user", "content": text }
                }))? + "\n";

                let mut cmd = Command::new(&bin_path);
                cmd.env("MULTIBOT_SESSION_ID", session_id);
                cmd.args(&args)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null());
                if let Some(dir) = cwd
                    && !dir.trim().is_empty()
                {
                    cmd.current_dir(dir);
                }

                let mut child = spawn_with_retry(&mut cmd)
                    .with_context(|| format!("spawning {}", bin_path.display()))?;
                let pid = child.id().unwrap_or(0);

                let io_setup = (|| -> Result<_> {
                    let stdin = child
                        .stdin
                        .take()
                        .ok_or_else(|| anyhow!("failed to capture agy stdin"))?;
                    let stdout = child
                        .stdout
                        .take()
                        .ok_or_else(|| anyhow!("failed to capture agy stdout"))?;
                    // Only a daemon-owned chat gets a daemon-written row; a native brain
                    // transcript is appended by `agy` itself.
                    if history_path.starts_with(self.home.join(".multibot")) {
                        append_agy_user_row(&history_path, session_id, cwd, text, self.next_seq())?;
                    }
                    Ok((stdin, stdout))
                })();

                let (stdin, stdout) = match io_setup {
                    Ok(pair) => pair,
                    Err(err) => {
                        let _ = child.start_kill();
                        tokio::spawn(async move {
                            let _ = child.wait().await;
                        });
                        return Err(err);
                    }
                };

                let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<String>();
                tokio::spawn(async move {
                    let mut writer = stdin;
                    while let Some(line) = stdin_rx.recv().await {
                        if writer.write_all(line.as_bytes()).await.is_err() {
                            break;
                        }
                        if writer.flush().await.is_err() {
                            break;
                        }
                    }
                });

                let _ = stdin_tx.send(wire);

                if let Some(conv_id) = conversation.as_deref()
                    && !history_path.starts_with(self.home.join(".multibot"))
                {
                    self.record_resumed_agy_session(conv_id, cwd, &history_path);
                }

                {
                    let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    guard.active.insert(
                        session_id.to_string(),
                        ActiveProcess {
                            pid,
                            cwd: cwd.map(String::from),
                            source_path: history_path.clone(),
                            stdin_tx: Some(stdin_tx),
                            busy: true,
                            interrupted: false,
                        },
                    );
                }

                // Spawn stdout stream-json reader writing to `history_path`.
                let mgr_stdout = Arc::clone(self);
                let sid_stdout = session_id.to_string();
                let hp_stdout = history_path.clone();
                tokio::spawn(async move {
                    let mut acc = AgyStreamAccumulator::new();
                    let mut lines = BufReader::new(stdout).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) else {
                            continue;
                        };
                        let outputs = acc.feed_value(&val, Some(now_ms()), None);
                        for out in outputs {
                            let turn_ended = apply_agy_stream_output(
                                &mgr_stdout.home,
                                &sid_stdout,
                                &hp_stdout,
                                out,
                                mgr_stdout.next_seq(),
                            );
                            if turn_ended {
                                let matched_pid = {
                                    let mut guard =
                                        mgr_stdout.state.lock().unwrap_or_else(|e| e.into_inner());
                                    if let Some(p) = guard.active.get_mut(&sid_stdout)
                                        && p.pid == pid
                                        && !p.interrupted
                                    {
                                        p.busy = false;
                                        true
                                    } else {
                                        false
                                    }
                                };
                                if matched_pid {
                                    mgr_stdout.notify_session_working(&sid_stdout, false);
                                    mgr_stdout.drain_next_queued(&sid_stdout);
                                }
                            }
                        }
                    }
                });

                // Spawn child reaper to prevent zombies & drain any remaining queue on exit
                let mgr_reaper = Arc::clone(self);
                let sid_reaper = session_id.to_string();
                tokio::spawn(async move {
                    let mut child = child;
                    let _ = child.wait().await;
                    mgr_reaper.on_process_exit(&sid_reaper, pid);
                });

                Ok(format!("dispatched agy (pid {pid})"))
            }
        }
    }

    fn register_one_shot_child(
        self: &Arc<Self>,
        session_id: String,
        cwd: Option<String>,
        source_path: PathBuf,
        mut child: Child,
    ) {
        let pid = child.id().unwrap_or(0);
        {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            guard.active.insert(
                session_id.clone(),
                ActiveProcess {
                    pid,
                    cwd,
                    source_path,
                    stdin_tx: None,
                    busy: true,
                    interrupted: false,
                },
            );
        }
        let mgr = Arc::clone(self);
        tokio::spawn(async move {
            let _ = child.wait().await;
            mgr.on_process_exit(&session_id, pid);
        });
    }

    fn on_process_exit(self: &Arc<Self>, session_id: &str, exited_pid: u32) {
        let removed_matching = {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if guard
                .active
                .get(session_id)
                .is_some_and(|p| p.pid == exited_pid)
            {
                guard.active.remove(session_id);
                true
            } else {
                false
            }
        };
        if removed_matching {
            self.notify_session_working(session_id, false);
            self.drain_next_queued(session_id);
        }
    }

    fn drain_next_queued(self: &Arc<Self>, session_id: &str) {
        loop {
            let next = {
                let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if guard
                    .active
                    .get(session_id)
                    .is_some_and(|p| p.busy || p.interrupted)
                {
                    return;
                }
                let popped = guard.queues.get_mut(session_id).and_then(|q| q.pop_front());
                if guard.queues.get(session_id).is_some_and(|q| q.is_empty()) {
                    guard.queues.remove(session_id);
                }
                popped
            };

            let Some(qp) = next else {
                return;
            };

            match self.dispatch_prompt_internal(
                session_id,
                qp.agent,
                qp.cwd.as_deref(),
                &qp.text,
                false,
                qp.options.as_ref(),
            ) {
                Ok(detail) => {
                    self.notify_session_working(session_id, true);
                    let _ = self.out_tx.send(ServerEvent::PromptStatus {
                        session_id: session_id.to_string(),
                        message_id: qp.message_id,
                        status: "sent".to_string(),
                        detail,
                    });
                    return;
                }
                Err(err) => {
                    let _ = self.out_tx.send(ServerEvent::PromptStatus {
                        session_id: session_id.to_string(),
                        message_id: qp.message_id,
                        status: "failed".to_string(),
                        detail: err.to_string(),
                    });
                    // Continue draining remaining queued prompts instead of stranding the queue.
                }
            }
        }
    }

    pub async fn interrupt_session(&self, session_id: String, _agent: AgentKind) {
        let pid_opt = {
            let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            guard.active.get(&session_id).map(|p| p.pid)
        };
        let Some(pid) = pid_opt else {
            let _ = self.out_tx.send(ServerEvent::SessionInterrupted {
                session_id,
                ok: false,
                detail: "no active hosted process for session".to_string(),
            });
            return;
        };
        if pid == 0 {
            let _ = self.out_tx.send(ServerEvent::SessionInterrupted {
                session_id,
                ok: false,
                detail: "process already exited".to_string(),
            });
            return;
        }

        let status = Command::new("kill")
            .args(["-s", "INT", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;

        match status {
            Ok(s) if s.success() => {
                {
                    let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(p) = guard.active.get_mut(&session_id)
                        && p.pid == pid
                    {
                        p.interrupted = true;
                        p.stdin_tx = None;
                    }
                }
                let _ = self.out_tx.send(ServerEvent::SessionInterrupted {
                    session_id,
                    ok: true,
                    detail: format!("sent SIGINT to pid {pid}"),
                });
            }
            _ => {
                {
                    let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    if guard.active.get(&session_id).is_some_and(|p| p.pid == pid) {
                        guard.active.remove(&session_id);
                    }
                }
                let _ = self.out_tx.send(ServerEvent::SessionInterrupted {
                    session_id,
                    ok: false,
                    detail: "process already exited".to_string(),
                });
            }
        }
    }

    pub async fn steer_session(
        self: &Arc<Self>,
        session_id: String,
        agent: AgentKind,
        cwd: Option<String>,
        text: String,
        options: Option<SessionOptions>,
    ) {
        if text.trim().is_empty() {
            let _ = self.out_tx.send(ServerEvent::PromptStatus {
                session_id,
                message_id: String::new(),
                status: "failed".to_string(),
                detail: "prompt text is empty".to_string(),
            });
            return;
        }

        let message_id = format!("q-{}-{}", now_ms(), self.next_seq());
        // A live `agy --input-format stream-json` process takes a new user frame at any time,
        // even mid-turn or while a background tool runs. Queueing it behind the turn is what
        // made Antigravity chats look stuck: the frame only left the queue when the turn ended.
        if agent == AgentKind::Antigravity && self.session_accepts_live_stdin(&session_id) {
            match self.dispatch_prompt_internal(
                &session_id,
                agent,
                cwd.as_deref(),
                &text,
                false,
                options.as_ref(),
            ) {
                Ok(detail) => {
                    let _ = self.out_tx.send(ServerEvent::PromptStatus {
                        session_id,
                        message_id,
                        status: "sent".to_string(),
                        detail,
                    });
                    return;
                }
                Err(err) => {
                    let _ = self.out_tx.send(ServerEvent::PromptStatus {
                        session_id,
                        message_id,
                        status: "failed".to_string(),
                        detail: err.to_string(),
                    });
                    return;
                }
            }
        }

        let enqueue_result: Option<Result<(), String>> = {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if guard.active.get(&session_id).is_some_and(|p| p.busy) {
                let q = guard.queues.entry(session_id.clone()).or_default();
                if q.len() >= MAX_QUEUED_PROMPTS_PER_SESSION {
                    Some(Err(format!(
                        "session prompt queue full (max {MAX_QUEUED_PROMPTS_PER_SESSION})"
                    )))
                } else {
                    q.push_back(QueuedPrompt {
                        message_id: message_id.clone(),
                        agent,
                        cwd: cwd.clone(),
                        text: text.clone(),
                        options: options.clone(),
                    });
                    Some(Ok(()))
                }
            } else {
                None
            }
        };

        match enqueue_result {
            Some(Ok(())) => {
                let _ = self.out_tx.send(ServerEvent::PromptStatus {
                    session_id,
                    message_id,
                    status: "queued".to_string(),
                    detail: "queued behind active turn".to_string(),
                });
            }
            Some(Err(reason)) => {
                let _ = self.out_tx.send(ServerEvent::PromptStatus {
                    session_id,
                    message_id,
                    status: "failed".to_string(),
                    detail: reason,
                });
            }
            None => {
                match self.dispatch_prompt_internal(
                    &session_id,
                    agent,
                    cwd.as_deref(),
                    &text,
                    false,
                    options.as_ref(),
                ) {
                    Ok(detail) => {
                        let _ = self.out_tx.send(ServerEvent::PromptStatus {
                            session_id,
                            message_id,
                            status: "sent".to_string(),
                            detail,
                        });
                    }
                    Err(err) => {
                        let _ = self.out_tx.send(ServerEvent::PromptStatus {
                            session_id,
                            message_id,
                            status: "failed".to_string(),
                            detail: err.to_string(),
                        });
                    }
                }
            }
        }
    }

    pub fn cancel_queued_prompt(&self, session_id: String, message_id: String) {
        let removed = {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(q) = guard.queues.get_mut(&session_id) {
                if let Some(pos) = q.iter().position(|p| p.message_id == message_id) {
                    q.remove(pos);
                    if q.is_empty() {
                        guard.queues.remove(&session_id);
                    }
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };

        if removed {
            let _ = self.out_tx.send(ServerEvent::PromptStatus {
                session_id,
                message_id,
                status: "cancelled".to_string(),
                detail: "removed from queue".to_string(),
            });
        } else {
            let _ = self.out_tx.send(ServerEvent::PromptStatus {
                session_id,
                message_id,
                status: "not_found".to_string(),
                detail: "prompt not queued or already dispatched".to_string(),
            });
        }
    }

    pub async fn spawn_session(
        self: &Arc<Self>,
        agent: AgentKind,
        cwd: String,
        title: Option<String>,
        initial_prompt: Option<String>,
        options: Option<SessionOptions>,
    ) {
        if let Err(err) = validate_options(agent, options.as_ref()) {
            let _ = self.out_tx.send(ServerEvent::Error {
                code: "invalid_options".into(),
                message: err.to_string(),
            });
            return;
        }
        if let Err(msg) = validate_cwd(&cwd) {
            let _ = self.out_tx.send(ServerEvent::Error {
                code: "invalid_cwd".to_string(),
                message: msg,
            });
            return;
        }

        let session_id = mint_uuid_v4(self.next_seq(), &cwd);
        let session_id = if agent == AgentKind::Shell {
            format!("term-{session_id}")
        } else {
            session_id
        };
        let clean_title = title
            .as_deref()
            .and_then(sanitize_title)
            .unwrap_or_else(|| {
                Path::new(&cwd)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(String::from)
                    .unwrap_or_else(|| format!("New {} session", agent.label()))
            });

        if agent == AgentKind::Shell {
            let result = Command::new("tmux")
                .args([
                    "new-session",
                    "-d",
                    "-s",
                    &session_id,
                    "-c",
                    &cwd,
                    "-e",
                    &format!("MULTIBOT_SESSION_ID={session_id}"),
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
            if !result.is_ok_and(|status| status.success()) {
                let _ = self.out_tx.send(ServerEvent::Error {
                    code: "spawn_failed".into(),
                    message: "Could not create server shell with tmux".into(),
                });
                return;
            }
        }

        let source_path = self.resolve_session_source_path(&session_id, agent, Some(&cwd));
        let ts = now_ms();

        let mut created_history_file = false;
        if matches!(
            agent,
            AgentKind::Antigravity | AgentKind::Pi | AgentKind::Cursor
        ) {
            let existed_before = source_path.exists();
            if let Err(err) = append_history_record(
                &source_path,
                &if agent == AgentKind::Pi {
                    json!({"type":"session", "version":3, "id":session_id, "cwd":cwd,
                        "timestamp":chrono::Utc::now().to_rfc3339()})
                } else {
                    json!({
                        "type": "multibot_meta",
                        "session_id": session_id,
                        "agent": agent,
                        "cwd": cwd,
                        "title": clean_title,
                        "created_at_ms": ts,
                    })
                },
            ) {
                if !existed_before {
                    let _ = std::fs::remove_file(&source_path);
                }
                let _ = self.out_tx.send(ServerEvent::Error {
                    code: "spawn_failed".to_string(),
                    message: format!("Failed to initialize {} history: {err}", agent.label()),
                });
                return;
            }
            created_history_file = !existed_before;
        }

        let mut meta = MetadataStore::load(&self.home);
        meta.sessions.insert(
            session_id.clone(),
            SessionMetaEntry {
                title: Some(clean_title.clone()),
                cwd: Some(cwd.clone()),
                agent: Some(agent),
                source_path: Some(source_path.to_string_lossy().to_string()),
                native_session_id: None,
                workspace: None,
                updated_at_ms: ts,
            },
        );
        if let Err(err) = meta.save(&self.home) {
            if created_history_file {
                let _ = std::fs::remove_file(&source_path);
            }
            if agent == AgentKind::Shell {
                let _ = Command::new("tmux")
                    .args(["kill-session", "-t", &format!("={session_id}")])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await;
            }
            let _ = self.out_tx.send(ServerEvent::Error {
                code: "spawn_failed".to_string(),
                message: format!("Failed to save session metadata: {err}"),
            });
            return;
        }

        let mut working = false;
        if let Some(prompt) = initial_prompt
            && agent != AgentKind::Shell
            && !prompt.trim().is_empty()
        {
            match self.dispatch_prompt_internal(
                &session_id,
                agent,
                Some(&cwd),
                &prompt,
                true,
                options.as_ref(),
            ) {
                Ok(_) => {
                    working = true;
                }
                Err(err) => {
                    // Roll back metadata entry AND newly created history file on spawn failure
                    if created_history_file {
                        let _ = std::fs::remove_file(&source_path);
                    }
                    let mut meta_rb = MetadataStore::load(&self.home);
                    meta_rb.sessions.remove(&session_id);
                    let _ = meta_rb.save(&self.home);
                    let _ = self.out_tx.send(ServerEvent::Error {
                        code: "spawn_failed".to_string(),
                        message: format!("Failed to spawn {}: {err}", agent.label()),
                    });
                    return;
                }
            }
        }

        let summary = SessionSummary {
            id: session_id.clone(),
            agent,
            title: clean_title,
            cwd: Some(cwd),
            source_path: source_path.to_string_lossy().to_string(),
            updated_at_ms: ts,
            working,
            workspace: crate::server_workspaces::ServerWorkspaceIndex::load_default_cached()
                .workspace_for(&session_id)
                .map(str::to_string),
            ..Default::default()
        };
        let _ = self
            .out_tx
            .send(ServerEvent::SessionSpawned { session: summary });
    }

    pub fn rename_session(
        &self,
        session_id: String,
        agent: AgentKind,
        source_path: String,
        title: String,
    ) {
        let Some(clean_title) = sanitize_title(&title) else {
            let _ = self.out_tx.send(ServerEvent::Error {
                code: "invalid_title".to_string(),
                message: "Session title cannot be blank".to_string(),
            });
            return;
        };

        let mut meta = MetadataStore::load(&self.home);
        let entry = meta.sessions.entry(session_id.clone()).or_default();
        entry.title = Some(clean_title.clone());
        entry.agent = Some(agent);
        if !source_path.trim().is_empty() {
            entry.source_path = Some(source_path.clone());
        }
        entry.updated_at_ms = now_ms();
        if let Err(err) = meta.save(&self.home) {
            let _ = self.out_tx.send(ServerEvent::Error {
                code: "rename_failed".to_string(),
                message: format!("Failed to save session metadata: {err}"),
            });
            return;
        }

        let archive = ArchiveStore::load(&self.home);
        let live = LiveActivity::load(&self.home);
        let active_ids = self.active_session_ids();
        let connected_ids = self.connected_session_ids();
        let server_workspaces =
            crate::server_workspaces::ServerWorkspaceIndex::load_default_cached();
        let ctx = DiscoveryContext {
            active_session_ids: &active_ids,
            active_session_pids: &self.active_session_pids(),
            connected_session_ids: &connected_ids,
            metadata: &meta,
            archive: &archive,
            live: &live,
            include_archived: true,
            server_workspaces: &server_workspaces,
        };

        let path = Path::new(&source_path);
        let session = if path.is_file() {
            let mtime_ms = path
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .map(system_time_ms)
                .unwrap_or_else(now_ms);
            summarize_session_by_agent(agent, path, mtime_ms, &ctx)
        } else {
            SessionSummary {
                id: session_id.clone(),
                agent,
                // `clean_title` was just written to `metadata.json`, so the manual rename is
                // authoritative and outranks any legacy name.
                title: clean_title,
                cwd: meta.get(&session_id).and_then(|e| e.cwd.clone()),
                source_path,
                updated_at_ms: now_ms(),
                working: active_ids.contains(&session_id),
                archived: archive.is_archived(&session_id),
                workspace: server_workspaces
                    .workspace_for(&session_id)
                    .map(str::to_string),
                ..Default::default()
            }
        };

        let _ = self.out_tx.send(ServerEvent::SessionUpdated { session });
    }

    pub async fn switch_shell_client(&self, client_id: String, session_id: String) {
        let socket = std::env::var("TMUX").ok().and_then(|value| {
            value
                .split(',')
                .next()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        });
        if let Err(error) =
            switch_shell_client_at(&self.home, &client_id, &session_id, socket.as_deref()).await
        {
            let _ = self.out_tx.send(ServerEvent::Error {
                code: "switch_shell_client_failed".into(),
                // Correlate failures without changing the existing Error wire shape.
                message: format!("{client_id}: {error}"),
            });
        }
    }

    pub async fn archive_session(
        &self,
        session_id: String,
        agent: AgentKind,
        source_path: String,
        kill_process: bool,
    ) {
        if agent == AgentKind::Shell && kill_process {
            let mut command = Command::new("tmux");
            if let Ok(tmux) = std::env::var("TMUX")
                && let Some(socket) = tmux.split(',').next().filter(|s| !s.is_empty())
            {
                command.args(["-S", socket]);
            }
            let result = command
                .env_remove("TMUX")
                .args(["kill-session", "-t", &format!("={session_id}")])
                .stdin(Stdio::null())
                .output()
                .await;
            match result {
                Ok(output) if output.status.success() => {}
                Ok(output) => {
                    let error = String::from_utf8_lossy(&output.stderr);
                    // A shell can exit between discovery and the dissolve request.
                    if !error.contains("can't find session")
                        && !error.contains("no server running")
                        && !error.contains("No such file or directory")
                    {
                        let _ = self.out_tx.send(ServerEvent::Error {
                            code: "archive_failed".into(),
                            message: format!("Could not kill server shell {session_id}: {error}"),
                        });
                        return;
                    }
                }
                Err(err) => {
                    let _ = self.out_tx.send(ServerEvent::Error {
                        code: "archive_failed".into(),
                        message: format!("Could not kill server shell {session_id}: {err}"),
                    });
                    return;
                }
            }
            let mut metadata = MetadataStore::load(&self.home);
            metadata.sessions.remove(&session_id);
            if let Err(err) = metadata.save(&self.home) {
                let _ = self.out_tx.send(ServerEvent::Error {
                    code: "archive_failed".into(),
                    message: format!("Failed to remove shell metadata: {err}"),
                });
                return;
            }
        }
        if kill_process {
            let (pid_opt, cancelled_ids) = {
                let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
                let pid = guard.active.remove(&session_id).map(|p| p.pid);
                let cancelled: Vec<String> = guard
                    .queues
                    .remove(&session_id)
                    .map(|q| q.into_iter().map(|p| p.message_id).collect())
                    .unwrap_or_default();
                (pid, cancelled)
            };

            for mid in cancelled_ids {
                let _ = self.out_tx.send(ServerEvent::PromptStatus {
                    session_id: session_id.clone(),
                    message_id: mid,
                    status: "cancelled".to_string(),
                    detail: "session archived".to_string(),
                });
            }

            if let Some(pid) = pid_opt
                && pid > 0
            {
                let _ = Command::new("kill")
                    .args(["-s", "TERM", &pid.to_string()])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await;
                tokio::time::sleep(Duration::from_millis(200)).await;
                // Check if still alive with `kill -0`, escalate to KILL if needed
                let still_alive = Command::new("kill")
                    .args(["-0", &pid.to_string()])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await
                    .is_ok_and(|s| s.success());
                if still_alive {
                    let _ = Command::new("kill")
                        .args(["-s", "KILL", &pid.to_string()])
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .await;
                }
            }
        }

        // HARD RULE: NEVER delete or modify the underlying `.jsonl` transcript file!
        let mut archive = ArchiveStore::load(&self.home);
        archive.archived.insert(
            session_id.clone(),
            ArchivedEntry {
                kill_process,
                archived_at_ms: now_ms(),
                agent: Some(agent),
                source_path: if source_path.trim().is_empty() {
                    None
                } else {
                    Some(source_path)
                },
            },
        );
        if let Err(err) = archive.save(&self.home) {
            let _ = self.out_tx.send(ServerEvent::Error {
                code: "archive_failed".to_string(),
                message: format!("Failed to persist archive state: {err}"),
            });
            return;
        }
        let _ = self.out_tx.send(ServerEvent::SessionArchived {
            session_id,
            archived: true,
        });
    }

    pub fn unarchive_session(&self, session_id: String) {
        let mut archive = ArchiveStore::load(&self.home);
        archive.archived.remove(&session_id);
        if let Err(err) = archive.save(&self.home) {
            let _ = self.out_tx.send(ServerEvent::Error {
                code: "unarchive_failed".to_string(),
                message: format!("Failed to persist unarchive state: {err}"),
            });
            return;
        }
        let _ = self.out_tx.send(ServerEvent::SessionArchived {
            session_id,
            archived: false,
        });
    }

    /// The conversation `agy` must resume for `session_id`, so clicking an existing chat never
    /// starts a blank one.
    ///
    /// `metadata.json` only knows chats this daemon spawned. A chat the user opens from
    /// `brain/<uuid>/.system_generated/logs/transcript.jsonl` was never recorded there, but its
    /// own id *is* the conversation id.
    fn resolve_agy_conversation(&self, session_id: &str, is_new_session: bool) -> Option<String> {
        if is_new_session {
            return None;
        }
        let meta = MetadataStore::load(&self.home);
        let recorded = meta
            .get(session_id)
            .and_then(|e| e.native_session_id.clone())
            .filter(|id| is_conversation_id(id));
        recorded
            .or_else(|| {
                native_brain_transcript(&self.home, session_id).map(|_| session_id.to_string())
            })
            .or_else(|| {
                meta.get(session_id)
                    .and_then(|e| e.source_path.clone())
                    .map(PathBuf::from)
                    .and_then(|p| crate::providers::antigravity::brain_conversation_id(&p))
            })
            .or_else(|| {
                crate::providers::antigravity::brain_conversation_id(
                    &crate::providers::antigravity::transcript_target(
                        &self.home,
                        session_id,
                        &hosted_history_path(&self.home, session_id),
                    ),
                )
            })
    }

    /// Keep a resumed native chat discoverable after its process exits.
    fn record_resumed_agy_session(
        &self,
        conversation: &str,
        cwd: Option<&str>,
        source_path: &Path,
    ) {
        let mut meta = MetadataStore::load(&self.home);
        let canonical_source =
            std::fs::canonicalize(source_path).unwrap_or_else(|_| source_path.to_path_buf());
        let owner = meta
            .sessions
            .iter()
            .filter(|(id, entry)| {
                entry
                    .agent
                    .is_none_or(|agent| agent == AgentKind::Antigravity)
                    && (id.as_str() == conversation
                        || entry.native_session_id.as_deref() == Some(conversation)
                        || entry.source_path.as_deref().is_some_and(|source| {
                            let path = Path::new(source);
                            std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
                                == canonical_source
                        }))
            })
            .max_by_key(|(id, entry)| (crate::providers::session_metadata_priority(id, entry), *id))
            .map(|(id, _)| id.clone())
            .unwrap_or_else(|| conversation.to_string());
        let entry = meta.sessions.entry(owner).or_default();
        entry.cwd = entry.cwd.take().or_else(|| cwd.map(String::from));
        entry.agent = Some(AgentKind::Antigravity);
        entry.source_path = Some(source_path.to_string_lossy().to_string());
        entry.native_session_id = Some(conversation.to_string());
        entry.updated_at_ms = now_ms();
        let _ = meta.save(&self.home);
    }

    fn agy_history_sink(&self, session_id: &str, conversation: Option<&str>) -> PathBuf {
        let hosted = hosted_history_path(&self.home, session_id);
        conversation
            .and_then(|id| native_brain_transcript(&self.home, id))
            .unwrap_or(hosted)
    }

    fn resolve_session_source_path(
        &self,
        session_id: &str,
        agent: AgentKind,
        cwd: Option<&str>,
    ) -> PathBuf {
        match agent {
            AgentKind::Shell => PathBuf::from(format!("tmux:{session_id}")),
            AgentKind::Antigravity => {
                let meta = MetadataStore::load(&self.home);
                let recorded = meta
                    .get(session_id)
                    .and_then(|e| e.source_path.clone())
                    .map(PathBuf::from)
                    .filter(|p| p.is_file());
                recorded.unwrap_or_else(|| hosted_history_path(&self.home, session_id))
            }
            AgentKind::Claude => {
                let sanitized = sanitize_claude_project_dir(cwd.unwrap_or("."));
                self.home
                    .join(".claude")
                    .join("projects")
                    .join(sanitized)
                    .join(format!("{session_id}.jsonl"))
            }
            AgentKind::Pi | AgentKind::Cursor => {
                if let Some(path) = MetadataStore::load(&self.home)
                    .get(session_id)
                    .and_then(|m| m.source_path.as_ref())
                    .map(PathBuf::from)
                {
                    return path;
                }
                let provider = crate::providers::provider_for(agent);
                let keep = |path: &Path, _: i64| {
                    path.file_stem()
                        .and_then(|s| s.to_str())
                        .is_some_and(|stem| {
                            stem == session_id || stem.ends_with(&format!("_{session_id}"))
                        })
                };
                provider
                    .discover_paths(&self.home, 1, &keep)
                    .into_iter()
                    .next()
                    .map(|p| p.0)
                    .unwrap_or_else(|| {
                        self.home
                            .join(".multibot")
                            .join(if agent == AgentKind::Pi {
                                "pi"
                            } else {
                                "cursor"
                            })
                            .join(format!("{session_id}.jsonl"))
                    })
            }
            AgentKind::Codex => MetadataStore::load(&self.home)
                .get(session_id)
                .and_then(|m| m.source_path.as_ref())
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    self.home
                        .join(".codex")
                        .join("sessions")
                        .join(format!("rollout-{session_id}.jsonl"))
                }),
        }
    }
}

fn build_pi_cursor_args(
    agent: AgentKind,
    session_id: &str,
    source_path: &Path,
    native_id: Option<&str>,
    text: &str,
    is_new: bool,
) -> Result<Vec<String>> {
    if session_id.is_empty() || session_id.starts_with('-') {
        return Err(anyhow!("invalid session id"));
    }
    let mut args = match agent {
        AgentKind::Pi => vec![
            "--print".into(),
            "--session".into(),
            source_path.to_string_lossy().into_owned(),
        ],
        AgentKind::Cursor => {
            let mut args = vec![
                "--print".into(),
                "--output-format".into(),
                "stream-json".into(),
            ];
            let owned = source_path
                .components()
                .any(|c| c.as_os_str() == ".multibot");
            if let Some(id) = native_id.or_else(|| (!is_new && !owned).then_some(session_id)) {
                if id.is_empty() || id.starts_with('-') {
                    return Err(anyhow!("invalid Cursor session id"));
                }
                args.extend(["--resume".into(), id.into()]);
            }
            args
        }
        _ => return Err(anyhow!("expected Pi or Cursor")),
    };
    args.extend(["--".into(), text.into()]);
    Ok(args)
}

async fn read_harness_output(
    stdout: tokio::process::ChildStdout,
    home: &Path,
    session_id: &str,
    sink: &Path,
    owned: bool,
    agent: AgentKind,
) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    let mut oversized = false;
    loop {
        let Ok(bytes) = reader.fill_buf().await else {
            break;
        };
        if bytes.is_empty() {
            break;
        }
        let count = bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(bytes.len(), |i| i + 1);
        let complete = bytes[count - 1] == b'\n';
        if !oversized && line.len() + count <= crate::providers::MAX_HISTORY_LINE_BYTES {
            line.extend_from_slice(&bytes[..count]);
        } else {
            oversized = true;
            line.clear();
        }
        reader.consume(count);
        if !complete {
            continue;
        }
        if !oversized {
            let raw = String::from_utf8_lossy(&line);
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&strip_base64_cow(&raw)) {
                let kind = value.get("type").and_then(serde_json::Value::as_str);
                if agent == AgentKind::Codex {
                    if let Some(native) = value.get("thread_id").and_then(serde_json::Value::as_str)
                    {
                        let mut meta = MetadataStore::load(home);
                        if let Some(entry) = meta.sessions.get_mut(session_id) {
                            entry.native_session_id = Some(native.to_string());
                            let _ = meta.save(home);
                        }
                    }
                    if kind == Some("item.completed") {
                        if let Some(item) = value.get("item").filter(|i| {
                            i.get("type").and_then(|v| v.as_str()) == Some("agent_message")
                        }) {
                            let _ = append_history_record(
                                sink,
                                &json!({"type":"response_item", "payload":{"type":"message", "role":"assistant", "id":item.get("id"), "content":[{"type":"output_text", "text":item.get("text")} ]}}),
                            );
                        }
                    }
                }

                if kind == Some("system")
                    && value.get("subtype").and_then(serde_json::Value::as_str) == Some("init")
                    && let Some(native) =
                        value.get("session_id").and_then(serde_json::Value::as_str)
                {
                    let mut meta = MetadataStore::load(home);
                    if let Some(entry) = meta.sessions.get_mut(session_id) {
                        entry.native_session_id = Some(native.to_string());
                        let _ = meta.save(home);
                    }
                }
                // Cursor also writes its native transcript. Never mirror into that file.
                if owned && matches!(kind, Some("assistant" | "result" | "system")) {
                    let _ = append_history_record(sink, &value);
                }
            }
        }
        line.clear();
        oversized = false;
    }
}

fn build_claude_args(
    session_id: &str,
    text: &str,
    is_new_session: bool,
    options: Option<&SessionOptions>,
) -> Result<Vec<String>> {
    if session_id.trim().is_empty() || session_id.starts_with('-') {
        return Err(anyhow!("invalid session_id for claude CLI: {session_id:?}"));
    }
    let flag = if is_new_session {
        "--session-id"
    } else {
        "--resume"
    };
    let mut args = vec![
        flag.into(),
        session_id.into(),
        "--print".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
    ];
    if let Some(options) = options {
        if let Some(model) = &options.model {
            args.extend(["--model".into(), model.clone()]);
        }
        if let Some(effort) = options.effort {
            args.extend(["--effort".into(), effort_value(effort, false).into()]);
        }
    }
    let mode = match options
        .and_then(|o| o.permission)
        .unwrap_or(PermissionLevel::FullAccess)
    {
        PermissionLevel::Ask => "default",
        PermissionLevel::Auto => "acceptEdits",
        PermissionLevel::FullAccess => "bypassPermissions",
    };
    args.extend(["--permission-mode".into(), mode.into()]);
    args.extend(["--".into(), text.into()]);
    Ok(args)
}

fn build_codex_args(
    session_id: &str,
    text: &str,
    is_new_session: bool,
    options: Option<&SessionOptions>,
) -> Result<Vec<String>> {
    let mut args = vec!["exec".into()];
    if !is_new_session {
        if session_id.trim().is_empty() || session_id.starts_with('-') {
            return Err(anyhow!("invalid session_id for codex CLI: {session_id:?}"));
        }
        args.extend(["resume".into(), session_id.into()]);
    }
    if let Some(options) = options {
        if let Some(model) = &options.model {
            args.extend(["--model".into(), model.clone()]);
        }
        if let Some(effort) = options.effort {
            args.extend([
                "-c".into(),
                format!("model_reasoning_effort=\"{}\"", effort_value(effort, true)),
            ]);
        }
    }
    // Config works for both exec and exec resume; resume has no --sandbox flag.
    let (sandbox, approval) = match options
        .and_then(|o| o.permission)
        .unwrap_or(PermissionLevel::FullAccess)
    {
        PermissionLevel::Ask => ("read-only", "on-request"),
        PermissionLevel::Auto => ("workspace-write", "on-request"),
        PermissionLevel::FullAccess => ("danger-full-access", "never"),
    };
    args.extend([
        "-c".into(),
        format!("sandbox_mode=\"{sandbox}\""),
        "-c".into(),
        format!("approval_policy=\"{approval}\""),
    ]);
    args.extend(["--".into(), text.into()]);
    Ok(args)
}

fn effort_value(effort: EffortLevel, codex: bool) -> &'static str {
    match effort {
        EffortLevel::Low => "low",
        EffortLevel::Medium => "medium",
        EffortLevel::High => "high",
        EffortLevel::Max if codex => "xhigh",
        EffortLevel::Max => "max",
    }
}

fn validate_options(agent: AgentKind, options: Option<&SessionOptions>) -> Result<()> {
    let Some(options) = options.filter(|o| !o.is_default()) else {
        return Ok(());
    };
    if (matches!(agent, AgentKind::Antigravity | AgentKind::Cursor)
        && (options.model.is_some() || options.effort.is_some()))
        || (agent == AgentKind::Pi && options.effort.is_some())
    {
        return Err(anyhow!(
            "{} does not support composer overrides; use agent defaults",
            agent.label()
        ));
    }
    if options.model.as_ref().is_some_and(|m| {
        m.trim().is_empty()
            || m.starts_with('-')
            || m.len() > 128
            || m.chars().any(char::is_control)
    }) {
        return Err(anyhow!("invalid model name"));
    }
    if agent == AgentKind::Claude && options.effort == Some(EffortLevel::Max) {
        return Err(anyhow!("Max effort is only available for Codex"));
    }
    Ok(())
}

pub fn validate_cwd(cwd: &str) -> Result<(), String> {
    if cwd.trim().is_empty() || cwd.contains('\0') {
        return Err(format!("Invalid working directory: {cwd}"));
    }
    let path = Path::new(cwd);
    for comp in path.components() {
        if matches!(comp, Component::ParentDir) {
            return Err(format!("Working directory must not contain '..': {cwd}"));
        }
    }
    if !path.is_dir() {
        return Err(format!("Working directory does not exist: {cwd}"));
    }
    Ok(())
}

pub fn sanitize_title(raw: &str) -> Option<String> {
    let scrubbed = strip_base64_cow(raw);
    let stripped: String = scrubbed.chars().filter(|c| !c.is_control()).collect();
    let trimmed = stripped.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(MAX_SESSION_TITLE_CHARS).collect())
}

fn mint_uuid_v4(seq: u64, salt: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seed = format!("{nanos}-{}-{seq}-{salt}", std::process::id());
    let hex = sha256_hex(seed.as_bytes());
    let mut b = [0u8; 16];
    for i in 0..16 {
        b[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or(0);
    }
    // RFC 4122 version 4 & variant 1 bits
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0],
        b[1],
        b[2],
        b[3],
        b[4],
        b[5],
        b[6],
        b[7],
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15]
    )
}

fn resolve_binary(home: &Path, name: &str) -> PathBuf {
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    let fallbacks = [
        home.join(".local/bin").join(name),
        home.join(".claude/local").join(name),
        home.join(".npm-global/bin").join(name),
        home.join(".bun/bin").join(name),
    ];
    for cand in fallbacks {
        if cand.is_file() {
            return cand;
        }
    }
    PathBuf::from(name)
}

fn append_agy_user_row(
    history_path: &Path,
    session_id: &str,
    cwd: Option<&str>,
    text: &str,
    seq: u64,
) -> Result<()> {
    let ts = now_ms();
    let rec = json!({
        "type": "multibot_history",
        "session_id": session_id,
        "cwd": cwd,
        "row": {
            "id": format!("user-{ts}-{seq}"),
            "role": "user",
            "text": text,
            "timestamp_ms": ts,
        }
    });
    append_history_record(history_path, &rec)
}

/// Applies one decoded `AgyStreamOutput` event to daemon-owned `~/.multibot/history/`
/// and `~/.multibot/metadata.json`.
///
/// Returns `true` when the assistant has finished answering. That is a completed
/// `agent_response` step as well as `event == "result"`: `result` can wait on background tasks,
/// and a chat that stays `busy` until then cannot take a follow-up and shows "Working" for
/// minutes after the reply already landed. The turn-end marker is written once, at `result`.
pub fn apply_agy_stream_output(
    home: &Path,
    session_id: &str,
    history_path: &Path,
    out: AgyStreamOutput,
    seq: u64,
) -> bool {
    let ts = now_ms();
    if let AgyStreamOutput::Init {
        conversation_id: Some(ref cid),
        ..
    } = out
    {
        let mut meta = MetadataStore::load(home);
        let entry = meta.sessions.entry(session_id.to_string()).or_default();
        entry.native_session_id = Some(cid.clone());
        entry.updated_at_ms = ts;
        let _ = meta.save(home);
    }
    let native = crate::providers::antigravity::resolve_transcript(home, session_id, history_path);
    if native != history_path {
        let mut meta = MetadataStore::load(home);
        let entry = meta.sessions.entry(session_id.to_string()).or_default();
        if entry.source_path.as_deref() != native.to_str() {
            entry.source_path = Some(native.to_string_lossy().into_owned());
            let _ = meta.save(home);
            let _ = append_history_record(
                history_path,
                &json!({"type":"multibot_meta", "session_id":session_id,
                "conversation_id": meta.get(session_id).and_then(|m| m.native_session_id.as_deref())}),
            );
        }
    }
    // Native transcripts belong to the CLI. Mirroring stdout there duplicates its replies.
    if (!matches!(out, AgyStreamOutput::Init { .. }) && native != history_path)
        || !history_path.starts_with(home.join(".multibot/history"))
    {
        return matches!(
            out,
            AgyStreamOutput::AssistantMessage(_) | AgyStreamOutput::TurnEnded { .. }
        );
    }
    match out {
        AgyStreamOutput::Init {
            conversation_id,
            model,
        } => {
            let _ = append_history_record(
                history_path,
                &json!({
                    "type": "multibot_meta",
                    "session_id": session_id,
                    "agent": "antigravity",
                    "conversation_id": conversation_id,
                    "model": model,
                }),
            );
            false
        }
        AgyStreamOutput::UserAccepted { .. } => false,
        AgyStreamOutput::AssistantMessage(msg) => {
            let _ = append_history_record(
                history_path,
                &json!({
                    "type": "multibot_history",
                    "session_id": session_id,
                    "row": {
                        "id": format!("{}-{seq}", msg.id),
                        "role": "assistant",
                        "text": msg.text,
                        "timestamp_ms": msg.timestamp_ms.unwrap_or(ts),
                    }
                }),
            );
            true
        }
        AgyStreamOutput::ToolStarted {
            call_id,
            name,
            summary,
            detail,
            diff,
            todos,
        } => {
            let _ = append_history_record(
                history_path,
                &json!({
                    "type": "multibot_history",
                    "session_id": session_id,
                    "row": {
                        "id": call_id,
                        "call_id": call_id,
                        "role": "tool",
                        "name": name,
                        "summary": summary,
                        "detail": detail,
                        "status": "running",
                        "diff": diff,
                        "todos": todos,
                        "timestamp_ms": ts,
                    }
                }),
            );
            false
        }
        AgyStreamOutput::ToolFinished {
            call_id,
            status,
            output,
            detail,
            ..
        } => {
            let status_str = match status {
                ToolStatus::Running => "running",
                ToolStatus::Completed => "completed",
                ToolStatus::Error => "error",
            };
            let _ = append_history_record(
                history_path,
                &json!({
                    "type": "multibot_history",
                    "session_id": session_id,
                    "row": {
                        "id": call_id,
                        "call_id": call_id,
                        "role": "tool_result",
                        "status": status_str,
                        "output": output,
                        "detail": detail,
                        "timestamp_ms": ts,
                    }
                }),
            );
            false
        }
        AgyStreamOutput::TurnEnded {
            status,
            fallback_message,
        } => {
            if let Some(msg) = fallback_message {
                let _ = append_history_record(
                    history_path,
                    &json!({
                        "type": "multibot_history",
                        "session_id": session_id,
                        "row": {
                            "id": format!("{}-{seq}", msg.id),
                            "role": "assistant",
                            "text": msg.text,
                            "timestamp_ms": msg.timestamp_ms.unwrap_or(ts),
                        }
                    }),
                );
            }
            let _ = append_history_record(
                history_path,
                &json!({
                    "type": "multibot_turn_end",
                    "status": status,
                    "timestamp_ms": ts,
                }),
            );
            true
        }
    }
}

fn spawn_with_retry(cmd: &mut Command) -> std::io::Result<Child> {
    // A concurrent fork can briefly retain a writable descriptor to an executable script.
    for _ in 0..10 {
        match cmd.spawn() {
            Err(err) if err.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(Duration::from_millis(10));
            }
            result => return result,
        }
    }
    cmd.spawn()
}

#[cfg(test)]
#[path = "process_tests.rs"]
mod tests;
