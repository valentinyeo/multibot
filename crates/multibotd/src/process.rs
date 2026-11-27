use crate::providers::antigravity::{
    AgyStreamAccumulator, AgyStreamOutput, append_history_record, hosted_history_path,
};
use crate::providers::claude::sanitize_claude_project_dir;
use crate::providers::{
    ArchiveStore, ArchivedEntry, DiscoveryContext, MetadataStore, SessionMetaEntry, now_ms,
    sha256_hex, strip_base64_cow, summarize_session_by_agent, system_time_ms,
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

    pub fn is_session_busy(&self, session_id: &str) -> bool {
        let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard.active.get(session_id).is_some_and(|p| p.busy)
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
        self.dispatch_prompt_internal(session_id, agent, cwd, text, false, options.as_ref())
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
        validate_options(agent, options)?;
        // Fast path: persistent hosted `agy` process already running with open stdin.
        if agent == AgentKind::Antigravity {
            let existing_tx = {
                let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(proc) = guard.active.get_mut(session_id)
                    && let Some(ref tx) = proc.stdin_tx
                {
                    proc.busy = true;
                    proc.interrupted = false;
                    Some((tx.clone(), proc.source_path.clone(), proc.pid, proc.cwd.clone()))
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
                    append_agy_user_row(
                        &history_path,
                        session_id,
                        effective_cwd.as_deref(),
                        text,
                        self.next_seq(),
                    )?;
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
        };
        let bin_path = self.resolve_agent_binary(bin_name);

        match agent {
            AgentKind::Claude => {
                let args = build_claude_args(session_id, text, is_new_session, options)?;
                let mut cmd = Command::new(&bin_path);
                cmd.args(&args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                if let Some(dir) = cwd
                    && !dir.trim().is_empty()
                {
                    cmd.current_dir(dir);
                }
                let child = cmd
                    .spawn()
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
                let args = build_codex_args(session_id, text, is_new_session, options)?;
                let mut cmd = Command::new(&bin_path);
                cmd.args(&args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                if let Some(dir) = cwd
                    && !dir.trim().is_empty()
                {
                    cmd.current_dir(dir);
                }
                let child = cmd
                    .spawn()
                    .with_context(|| format!("spawning {}", bin_path.display()))?;
                let pid = child.id().unwrap_or(0);
                let source_path = self.resolve_session_source_path(session_id, agent, cwd);
                self.register_one_shot_child(
                    session_id.to_string(),
                    cwd.map(String::from),
                    source_path,
                    child,
                );
                Ok(format!("dispatched codex (pid {pid})"))
            }
            AgentKind::Antigravity => {
                let meta = MetadataStore::load(&self.home);
                let native_conv_id = meta
                    .get(session_id)
                    .and_then(|e| e.native_session_id.clone());

                let mut args = vec![
                    "--input-format".to_string(),
                    "stream-json".to_string(),
                    "--output-format".to_string(),
                    "stream-json".to_string(),
                    "--dangerously-skip-permissions".to_string(),
                    "--print=".to_string(),
                ];
                if let Some(conv_id) = native_conv_id
                    && !conv_id.trim().is_empty()
                    && !conv_id.starts_with('-')
                {
                    args.push("--conversation".to_string());
                    args.push(conv_id);
                }

                let wire = serde_json::to_string(&json!({
                    "event": "user",
                    "message": { "role": "user", "content": text }
                }))? + "\n";

                let mut cmd = Command::new(&bin_path);
                cmd.args(&args)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null());
                if let Some(dir) = cwd
                    && !dir.trim().is_empty()
                {
                    cmd.current_dir(dir);
                }

                let mut child = cmd
                    .spawn()
                    .with_context(|| format!("spawning {}", bin_path.display()))?;
                let pid = child.id().unwrap_or(0);
                let history_path = hosted_history_path(&self.home, session_id);

                let io_setup = (|| -> Result<_> {
                    let stdin = child
                        .stdin
                        .take()
                        .ok_or_else(|| anyhow!("failed to capture agy stdin"))?;
                    let stdout = child
                        .stdout
                        .take()
                        .ok_or_else(|| anyhow!("failed to capture agy stdout"))?;
                    append_agy_user_row(&history_path, session_id, cwd, text, self.next_seq())?;
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

                // Spawn stdout stream-json reader writing to daemon-owned ~/.multibot/history/
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
                                    let mut guard = mgr_stdout
                                        .state
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner());
                                    if let Some(p) = guard.active.get_mut(&sid_stdout)
                                        && p.pid == pid
                                    {
                                        p.busy = false;
                                        true
                                    } else {
                                        false
                                    }
                                };
                                if matched_pid {
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
            self.drain_next_queued(session_id);
        }
    }

    fn drain_next_queued(self: &Arc<Self>, session_id: &str) {
        loop {
            let next = {
                let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if guard.active.get(session_id).is_some_and(|p| p.busy) {
                    return;
                }
                let popped = guard
                    .queues
                    .get_mut(session_id)
                    .and_then(|q| q.pop_front());
                if guard
                    .queues
                    .get(session_id)
                    .is_some_and(|q| q.is_empty())
                {
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
                    if let Some(p) = guard.active.get_mut(&session_id) {
                        p.interrupted = true;
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

        let source_path = self.resolve_session_source_path(&session_id, agent, Some(&cwd));
        let ts = now_ms();

        let mut created_history_file = false;
        if agent == AgentKind::Antigravity {
            let existed_before = source_path.exists();
            if let Err(err) = append_history_record(
                &source_path,
                &json!({
                    "type": "multibot_meta",
                    "session_id": session_id,
                    "agent": "antigravity",
                    "cwd": cwd,
                    "title": clean_title,
                    "created_at_ms": ts,
                }),
            ) {
                if !existed_before {
                    let _ = std::fs::remove_file(&source_path);
                }
                let _ = self.out_tx.send(ServerEvent::Error {
                    code: "spawn_failed".to_string(),
                    message: format!("Failed to initialize Antigravity history: {err}"),
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
                updated_at_ms: ts,
            },
        );
        if let Err(err) = meta.save(&self.home) {
            if created_history_file {
                let _ = std::fs::remove_file(&source_path);
            }
            let _ = self.out_tx.send(ServerEvent::Error {
                code: "spawn_failed".to_string(),
                message: format!("Failed to save session metadata: {err}"),
            });
            return;
        }

        let mut working = false;
        if let Some(prompt) = initial_prompt
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
        let _ = self.out_tx.send(ServerEvent::SessionSpawned { session: summary });
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
        let active_ids = self.active_session_ids();
        let server_workspaces =
            crate::server_workspaces::ServerWorkspaceIndex::load_default_cached();
        let ctx = DiscoveryContext {
            active_session_ids: &active_ids,
            metadata: &meta,
            archive: &archive,
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

    pub async fn archive_session(
        &self,
        session_id: String,
        agent: AgentKind,
        source_path: String,
        kill_process: bool,
    ) {
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

    fn resolve_session_source_path(
        &self,
        session_id: &str,
        agent: AgentKind,
        cwd: Option<&str>,
    ) -> PathBuf {
        match agent {
            AgentKind::Antigravity => hosted_history_path(&self.home, session_id),
            AgentKind::Claude => {
                let sanitized = sanitize_claude_project_dir(cwd.unwrap_or("."));
                self.home
                    .join(".claude")
                    .join("projects")
                    .join(sanitized)
                    .join(format!("{session_id}.jsonl"))
            }
            AgentKind::Codex => self
                .home
                .join(".codex")
                .join("sessions")
                .join(format!("rollout-{session_id}.jsonl")),
        }
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
        if let Some(permission) = options.permission {
            let mode = match permission {
                PermissionLevel::Ask => "default",
                PermissionLevel::Auto => "acceptEdits",
                PermissionLevel::FullAccess => "bypassPermissions",
            };
            args.extend(["--permission-mode".into(), mode.into()]);
        }
    }
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
        if let Some(permission) = options.permission {
            // Config works for both exec and exec resume; resume has no --sandbox flag.
            let (sandbox, approval) = match permission {
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
        }
    }
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
    if agent == AgentKind::Antigravity {
        return Err(anyhow!(
            "Antigravity does not support composer overrides; use agent defaults"
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
    let stripped: String = scrubbed
        .chars()
        .filter(|c| !c.is_control())
        .collect();
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
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13],
        b[14], b[15]
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
/// and `~/.multibot/metadata.json`. Returns `true` if a turn ended (`event == "result"`).
pub fn apply_agy_stream_output(
    home: &Path,
    session_id: &str,
    history_path: &Path,
    out: AgyStreamOutput,
    seq: u64,
) -> bool {
    let ts = now_ms();
    match out {
        AgyStreamOutput::Init {
            conversation_id,
            model,
        } => {
            if let Some(ref cid) = conversation_id {
                let mut meta = MetadataStore::load(home);
                let entry = meta.sessions.entry(session_id.to_string()).or_default();
                entry.native_session_id = Some(cid.clone());
                entry.updated_at_ms = ts;
                let _ = meta.save(home);
            }
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
            false
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::read_history_page;
    use std::fs;
    use std::sync::atomic::AtomicU64;

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(1);

    fn temp_home(tag: &str) -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "multibotd-proc-{tag}-{}-{n}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn composer_options_map_to_provider_flags_before_prompt_separator() {
        let options = SessionOptions {
            model: Some("opus".into()),
            effort: Some(EffortLevel::High),
            permission: Some(PermissionLevel::Auto),
        };
        for is_new in [true, false] {
            let args =
                build_claude_args("session", "--unsafe-looking prompt", is_new, Some(&options))
                    .unwrap();
            assert!(args.windows(2).any(|a| a == ["--model", "opus"]));
            assert!(args.windows(2).any(|a| a == ["--effort", "high"]));
            assert!(
                args.windows(2)
                    .any(|a| a == ["--permission-mode", "acceptEdits"])
            );
            assert_eq!(&args[args.len() - 2..], ["--", "--unsafe-looking prompt"]);
            for (permission, sandbox, approval) in [
                (PermissionLevel::Ask, "read-only", "on-request"),
                (PermissionLevel::Auto, "workspace-write", "on-request"),
                (PermissionLevel::FullAccess, "danger-full-access", "never"),
            ] {
                let options = SessionOptions {
                    model: Some("gpt-5.3-codex".into()),
                    effort: Some(EffortLevel::Max),
                    permission: Some(permission),
                };
                let args = build_codex_args("session", "hello", is_new, Some(&options)).unwrap();
                assert!(args.windows(2).any(|a| a == ["--model", "gpt-5.3-codex"]));
                assert!(args.contains(&"model_reasoning_effort=\"xhigh\"".to_string()));
                assert!(args.contains(&format!("sandbox_mode=\"{sandbox}\"")));
                assert!(args.contains(&format!("approval_policy=\"{approval}\"")));
                assert_eq!(&args[args.len() - 2..], ["--", "hello"]);
            }
        }
        for (permission, mode) in [
            (PermissionLevel::Ask, "default"),
            (PermissionLevel::FullAccess, "bypassPermissions"),
        ] {
            let options = SessionOptions {
                permission: Some(permission),
                ..Default::default()
            };
            let args = build_claude_args("session", "hello", false, Some(&options)).unwrap();
            assert!(args.windows(2).any(|a| a == ["--permission-mode", mode]));
        }
    }

    #[tokio::test]
    async fn composer_unsupported_options_fail_without_spawning_or_writing_metadata() {
        let home = temp_home("unsupported-options");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mgr = ProcessManager::new(home.clone(), tx);
        let options = SessionOptions {
            model: Some("opus".into()),
            ..Default::default()
        };
        assert!(
            mgr.send_prompt(
                "s1",
                AgentKind::Antigravity,
                None,
                "hello",
                Some(options.clone())
            )
            .await
            .is_err()
        );
        mgr.spawn_session(
            AgentKind::Antigravity,
            home.to_string_lossy().into(),
            None,
            Some("hello".into()),
            Some(options),
        )
        .await;
        assert!(
            matches!(rx.recv().await, Some(ServerEvent::Error { code, .. }) if code == "invalid_options")
        );
        assert!(MetadataStore::load(&home).sessions.is_empty());
        assert!(mgr.active_session_ids().is_empty());
        for model in ["", "--help", "bad\nmodel"] {
            let options = SessionOptions {
                model: Some(model.into()),
                ..Default::default()
            };
            assert!(
                mgr.send_prompt("s1", AgentKind::Claude, None, "hello", Some(options))
                    .await
                    .is_err()
            );
        }
        let _ = fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn rename_archive_unarchive_preserves_jsonl_and_validates_title() {
        let home = temp_home("lifecycle");
        let transcript = home.join(".claude/projects/demo/s1.jsonl");
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        let original_body = r#"{"type":"user","uuid":"u1","message":{"content":"hello"}}"#
            .to_string()
            + "\n";
        fs::write(&transcript, &original_body).unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mgr = ProcessManager::new(home.clone(), tx);

        // Blank title rejected
        mgr.rename_session(
            "s1".into(),
            AgentKind::Claude,
            transcript.to_string_lossy().into(),
            "   \n\t ".into(),
        );
        match rx.recv().await.unwrap() {
            ServerEvent::Error { code, .. } => assert_eq!(code, "invalid_title"),
            other => panic!("expected invalid_title error, got {other:?}"),
        }

        // >63 chars capped to 63 and persisted atomically
        let long_title = format!("  {}  ", "A".repeat(80));
        mgr.rename_session(
            "s1".into(),
            AgentKind::Claude,
            transcript.to_string_lossy().into(),
            long_title,
        );
        match rx.recv().await.unwrap() {
            ServerEvent::SessionUpdated { session } => {
                assert_eq!(session.title.chars().count(), MAX_SESSION_TITLE_CHARS);
                assert_eq!(session.title, "A".repeat(MAX_SESSION_TITLE_CHARS));
            }
            other => panic!("expected SessionUpdated, got {other:?}"),
        }

        // Archive preserves transcript untouched
        mgr.archive_session(
            "s1".into(),
            AgentKind::Claude,
            transcript.to_string_lossy().into(),
            false,
        )
        .await;
        match rx.recv().await.unwrap() {
            ServerEvent::SessionArchived {
                session_id,
                archived,
            } => {
                assert_eq!(session_id, "s1");
                assert!(archived);
            }
            other => panic!("expected SessionArchived, got {other:?}"),
        }
        assert_eq!(fs::read_to_string(&transcript).unwrap(), original_body);
        assert!(ArchiveStore::load(&home).is_archived("s1"));

        // Unarchive clears flag
        mgr.unarchive_session("s1".into());
        match rx.recv().await.unwrap() {
            ServerEvent::SessionArchived {
                session_id,
                archived,
            } => {
                assert_eq!(session_id, "s1");
                assert!(!archived);
            }
            other => panic!("expected SessionArchived(false), got {other:?}"),
        }
        assert!(!ArchiveStore::load(&home).is_archived("s1"));

        let _ = fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn spawn_session_validates_cwd_and_creates_agy_history() {
        let home = temp_home("spawn");
        let workdir = home.join("project");
        fs::create_dir_all(&workdir).unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mgr = ProcessManager::new(home.clone(), tx);

        // Reject '..' and nonexistent dir
        mgr.spawn_session(
            AgentKind::Antigravity,
            "/tmp/../etc".into(),
            None,
            None,
            None,
        )
        .await;
        match rx.recv().await.unwrap() {
            ServerEvent::Error { code, .. } => assert_eq!(code, "invalid_cwd"),
            other => panic!("expected invalid_cwd, got {other:?}"),
        }

        mgr.spawn_session(
            AgentKind::Antigravity,
            home.join("does-not-exist").to_string_lossy().into(),
            None,
            None,
            None,
        )
        .await;
        match rx.recv().await.unwrap() {
            ServerEvent::Error { code, .. } => assert_eq!(code, "invalid_cwd"),
            other => panic!("expected invalid_cwd, got {other:?}"),
        }

        // Valid spawn without initial_prompt creates daemon-owned history file
        mgr.spawn_session(
            AgentKind::Antigravity,
            workdir.to_string_lossy().into(),
            Some("My Agy Chat".into()),
            None,
            None,
        )
        .await;
        match rx.recv().await.unwrap() {
            ServerEvent::SessionSpawned { session } => {
                assert_eq!(session.agent, AgentKind::Antigravity);
                assert_eq!(session.title, "My Agy Chat");
                assert!(Path::new(&session.source_path).is_file());
                assert!(
                    Path::new(&session.source_path)
                        .starts_with(home.join(".multibot").join("history"))
                );
            }
            other => panic!("expected SessionSpawned, got {other:?}"),
        }

        let _ = fs::remove_dir_all(&home);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn interrupt_unowned_returns_false_and_real_child_receives_sigint() {
        let home = temp_home("interrupt");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mgr = ProcessManager::new(home.clone(), tx);

        // Unowned session returns ok: false cleanly
        mgr.interrupt_session("unowned-1".into(), AgentKind::Claude)
            .await;
        match rx.recv().await.unwrap() {
            ServerEvent::SessionInterrupted {
                session_id,
                ok,
                detail,
            } => {
                assert_eq!(session_id, "unowned-1");
                assert!(!ok);
                assert!(detail.contains("no active"));
            }
            other => panic!("expected SessionInterrupted, got {other:?}"),
        }

        // Track a real `/bin/sleep 30` child and verify real SIGINT interrupts it!
        let child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        mgr.register_one_shot_child(
            "live-sleep".into(),
            None,
            home.join("live.jsonl"),
            child,
        );
        assert!(mgr.is_session_busy("live-sleep"));

        // Queue two prompts while busy, cancel the first one
        mgr.steer_session(
            "live-sleep".into(),
            AgentKind::Claude,
            None,
            "first queued".into(),
            None,
        )
        .await;
        let q1_id = match rx.recv().await.unwrap() {
            ServerEvent::PromptStatus {
                message_id, status, ..
            } => {
                assert_eq!(status, "queued");
                message_id
            }
            other => panic!("expected PromptStatus(queued), got {other:?}"),
        };

        mgr.cancel_queued_prompt("live-sleep".into(), q1_id.clone());
        match rx.recv().await.unwrap() {
            ServerEvent::PromptStatus {
                message_id, status, ..
            } => {
                assert_eq!(message_id, q1_id);
                assert_eq!(status, "cancelled");
            }
            other => panic!("expected PromptStatus(cancelled), got {other:?}"),
        }

        // Cancelling missing prompt yields not_found
        mgr.cancel_queued_prompt("live-sleep".into(), "missing-id".into());
        match rx.recv().await.unwrap() {
            ServerEvent::PromptStatus { status, .. } => assert_eq!(status, "not_found"),
            other => panic!("expected PromptStatus(not_found), got {other:?}"),
        }

        // Interrupt real child with SIGINT
        mgr.interrupt_session("live-sleep".into(), AgentKind::Claude)
            .await;
        match rx.recv().await.unwrap() {
            ServerEvent::SessionInterrupted {
                session_id,
                ok,
                detail,
            } => {
                assert_eq!(session_id, "live-sleep");
                assert!(ok);
                assert!(detail.contains("SIGINT"));
            }
            other => panic!("expected SessionInterrupted(ok), got {other:?}"),
        }

        // Wait briefly for reaper to clear active map
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!mgr.is_session_busy("live-sleep"));

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn agy_stream_writer_persists_and_parses_back_rich_messages() {
        let home = temp_home("agy-writer");
        let sid = "agy-hosted-1";
        let hp = hosted_history_path(&home, sid);
        append_agy_user_row(&hp, sid, Some("/repo"), "Run build", 1).unwrap();

        let mut acc = AgyStreamAccumulator::new();
        let events = vec![
            json!({"event":"init","conversation_id":"conv-99","model":"gemini-4"}),
            json!({"event":"step_update","step_update":{"step_type":"tool","step_index":1,"state":"RUNNING","tool_name":"bash","tool_info":{"parameters":{"command":"cargo build"}}}}),
            json!({"event":"step_update","step_update":{"step_type":"tool","step_index":1,"state":"DONE","tool_name":"bash","tool_info":{"output":"Finished dev"}}}),
            json!({"event":"step_update","step_update":{"step_type":"agent_response","step_index":2,"state":"DONE","text_delta":"Build succeeded"}}),
            json!({"event":"result","result":{"status":"SUCCESS","conversation_id":"conv-99"}}),
        ];
        let mut seq = 2u64;
        let mut saw_end = false;
        for ev in events {
            for out in acc.feed_value(&ev, Some(1000), None) {
                if apply_agy_stream_output(&home, sid, &hp, out, seq) {
                    saw_end = true;
                }
                seq += 1;
            }
        }
        assert!(saw_end);
        assert_eq!(
            MetadataStore::load(&home)
                .get(sid)
                .and_then(|e| e.native_session_id.as_deref()),
            Some("conv-99")
        );

        let page = read_history_page(AgentKind::Antigravity, &hp, None, None, None, None).unwrap();
        assert_eq!(page.messages.len(), 3);
        assert_eq!(page.messages[0].text, "Run build");
        assert_eq!(page.messages[2].text, "Build succeeded");

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn cli_arg_builders_insert_double_dash_and_reject_leading_dash_session_id() {
        let c_new = build_claude_args("sess-1", "--help", true, None).unwrap();
        assert_eq!(c_new[c_new.len() - 2], "--");
        assert_eq!(c_new[c_new.len() - 1], "--help");

        let c_res = build_claude_args("sess-1", "- bullet item", false, None).unwrap();
        assert_eq!(c_res[0], "--resume");
        assert_eq!(c_res[c_res.len() - 2], "--");
        assert_eq!(c_res[c_res.len() - 1], "- bullet item");

        assert!(build_claude_args("-bad", "hello", false, None).is_err());
        assert!(build_claude_args("", "hello", true, None).is_err());

        let x_new = build_codex_args("ignored", "- fix tests", true, None).unwrap();
        assert_eq!(x_new, vec!["exec", "--", "- fix tests"]);

        let x_res = build_codex_args("codex-1", "--version", false, None).unwrap();
        assert_eq!(x_res, vec!["exec", "resume", "codex-1", "--", "--version"]);

        assert!(build_codex_args("--evil", "hello", false, None).is_err());
    }

    #[tokio::test]
    async fn spawn_session_antigravity_failure_cleans_up_orphaned_history_file() {
        let home = temp_home("agy-rollback");
        let workdir = home.join("project");
        fs::create_dir_all(&workdir).unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mgr = ProcessManager::new(home.clone(), tx);
        mgr.set_test_bin_override(Some(home.join("nonexistent-agy-binary")));

        mgr.spawn_session(
            AgentKind::Antigravity,
            workdir.to_string_lossy().into(),
            Some("ShouldRollback".into()),
            Some("hello".into()),
            None,
        )
        .await;

        match rx.recv().await.unwrap() {
            ServerEvent::Error { code, .. } => assert_eq!(code, "spawn_failed"),
            other => panic!("expected spawn_failed error, got {other:?}"),
        }

        // Verify ZERO orphaned .history.jsonl files and ZERO leaked sessions in ListSessions
        let hist_dir = home.join(".multibot/history");
        if hist_dir.is_dir() {
            let remaining: Vec<_> = fs::read_dir(&hist_dir)
                .unwrap()
                .flatten()
                .collect();
            assert!(
                remaining.is_empty(),
                "failed Antigravity spawn leaked history files: {remaining:?}"
            );
        }
        assert!(MetadataStore::load(&home).sessions.is_empty());
        assert!(crate::providers::discover_all_sessions(&home, 10).is_empty());

        let _ = fs::remove_dir_all(&home);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn drain_next_queued_continues_past_failed_prompt() {
        let home = temp_home("drain-continue");
        let valid_cwd = home.join("valid-cwd");
        fs::create_dir_all(&valid_cwd).unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mgr = ProcessManager::new(home.clone(), tx);
        // Use `/bin/true` (always present on Linux) so valid cwd succeeds while bad cwd fails spawn
        mgr.set_test_bin_override(Some(PathBuf::from("/bin/true")));

        // Seed two queued prompts: #1 has nonexistent cwd (dispatch fails), #2 has valid cwd (dispatch succeeds)
        {
            let mut guard = mgr.state.lock().unwrap();
            let q = guard.queues.entry("sess-drain".to_string()).or_default();
            q.push_back(QueuedPrompt {
                message_id: "q-bad".into(),
                agent: AgentKind::Claude,
                cwd: Some(home.join("does-not-exist").to_string_lossy().into()),
                text: "will fail".into(),
                options: None,
            });
            q.push_back(QueuedPrompt {
                message_id: "q-good".into(),
                agent: AgentKind::Claude,
                cwd: Some(valid_cwd.to_string_lossy().into()),
                text: "will succeed".into(),
                options: None,
            });
        }

        mgr.drain_next_queued("sess-drain");

        let ev1 = rx.recv().await.expect("first PromptStatus");
        match ev1 {
            ServerEvent::PromptStatus {
                message_id, status, ..
            } => {
                assert_eq!(message_id, "q-bad");
                assert_eq!(status, "failed");
            }
            other => panic!("expected failed status for q-bad, got {other:?}"),
        }

        let ev2 = rx.recv().await.expect("second PromptStatus");
        match ev2 {
            ServerEvent::PromptStatus {
                message_id, status, ..
            } => {
                assert_eq!(message_id, "q-good");
                assert_eq!(status, "sent");
            }
            other => panic!("expected sent status for q-good, got {other:?}"),
        }

        let _ = fs::remove_dir_all(&home);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn agy_stale_stdin_fallback_does_not_duplicate_user_history_row() {
        let home = temp_home("agy-dedup");
        let sid = "agy-stale-1";
        let hp = hosted_history_path(&home, sid);
        let (tx, _rx) = mpsc::unbounded_channel();
        let mgr = ProcessManager::new(home.clone(), tx);
        mgr.set_test_bin_override(Some(PathBuf::from("/bin/true")));

        // Insert stale ActiveProcess whose stdin_rx is already dropped
        let (stale_tx, stale_rx) = mpsc::unbounded_channel::<String>();
        drop(stale_rx);
        {
            let mut guard = mgr.state.lock().unwrap();
            guard.active.insert(
                sid.to_string(),
                ActiveProcess {
                    pid: 999_999,
                    cwd: None,
                    source_path: hp.clone(),
                    stdin_tx: Some(stale_tx),
                    busy: false,
                    interrupted: false,
                },
            );
        }

        mgr.send_prompt(sid, AgentKind::Antigravity, None, "single message", None)
            .await
            .unwrap();

        let page = read_history_page(AgentKind::Antigravity, &hp, None, None, None, None).unwrap();
        assert_eq!(
            page.messages.len(),
            1,
            "stale stdin_tx fallback must write user row exactly once, got {:?}",
            page.messages
        );
        assert_eq!(page.messages[0].text, "single message");

        let _ = fs::remove_dir_all(&home);
    }
}
