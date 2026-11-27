use crate::daemon_deploy::{
    REMOTE_DAEMON_DIR, REMOTE_DAEMON_SYMLINK, needs_remote_upgrade, remote_versioned_binary_name,
    sha256_prefix_12,
};
use crate::settings::SshHostConfig;
use anyhow::{Context, Result, anyhow, bail};
use multibot_proto::{AgentKind, ClientRequest, ServerEvent, build_version, decode_line, encode_line};
use std::collections::VecDeque;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

pub const DAEMON_VERSION: &str = match option_env!("MULTIBOT_BUILD_VERSION") {
    Some(v) if !v.is_empty() => v,
    _ => env!("CARGO_PKG_VERSION"),
};

pub const MAX_REMOTE_UPGRADE_ATTEMPTS: usize = 1;
pub const HELLO_ACK_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonTarget {
    LocalBinary(PathBuf),
    RemoteSsh {
        host: SshHostConfig,
        /// Optional local `multibotd-linux-x86_64` binary to auto-upload if missing or outdated on VPS.
        bundled_linux_binary: Option<PathBuf>,
    },
}

pub struct DaemonConnection {
    tx: mpsc::UnboundedSender<ClientRequest>,
    rx: mpsc::UnboundedReceiver<ServerEvent>,
    buffered_events: VecDeque<ServerEvent>,
    _child: Option<Child>,
}

impl DaemonConnection {
    pub async fn connect(target: DaemonTarget) -> Result<Self> {
        match target {
            DaemonTarget::LocalBinary(path) => {
                let child = spawn_local(&path)?;
                Self::from_child(child)
            }
            DaemonTarget::RemoteSsh {
                host,
                bundled_linux_binary,
            } => {
                validate_ssh_host(&host)?;
                if let Some(local_bin) = bundled_linux_binary
                    && local_bin.is_file()
                {
                    let bytes = tokio::fs::read(&local_bin).await.with_context(|| {
                        format!("reading bundled daemon {}", local_bin.display())
                    })?;
                    if bytes.is_empty() {
                        bail!(
                            "bundled multibotd binary is empty: {}",
                            local_bin.display()
                        );
                    }
                    let expected_version = build_version();
                    let expected_hash = sha256_prefix_12(&bytes);
                    let binary_name =
                        remote_versioned_binary_name(expected_version, &expected_hash);
                    validate_remote_binary_name(&binary_name)?;

                    // Pre-check & deploy if missing on VPS
                    ensure_remote_daemon_deployed(&host, &bytes, &binary_name, false).await?;

                    // Connect and auto-upgrade at most once if HelloAck version/hash drifts
                    let host_ref = &host;
                    let bytes_ref = &bytes;
                    let bin_name_ref = &binary_name;
                    connect_with_upgrade_loop(
                        expected_version,
                        &expected_hash,
                        HELLO_ACK_TIMEOUT,
                        || async move {
                            let child = spawn_ssh_daemon(host_ref)?;
                            Self::from_child(child)
                        },
                        || async move {
                            ensure_remote_daemon_deployed(host_ref, bytes_ref, bin_name_ref, true)
                                .await
                        },
                    )
                    .await
                } else {
                    let child = spawn_ssh_daemon(&host)?;
                    Self::from_child(child)
                }
            }
        }
    }

    fn from_child(mut child: Child) -> Result<Self> {
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("failed to capture daemon stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("failed to capture daemon stdout"))?;

        let (req_tx, mut req_rx) = mpsc::unbounded_channel::<ClientRequest>();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel::<ServerEvent>();

        tokio::spawn(async move {
            let mut writer = stdin;
            while let Some(req) = req_rx.recv().await {
                let Ok(line) = encode_line(&req) else {
                    continue;
                };
                if writer.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
                if writer.flush().await.is_err() {
                    break;
                }
            }
        });

        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                match decode_line::<ServerEvent>(&line) {
                    Ok(ev) => {
                        if ev_tx.send(ev).is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        let _ = ev_tx.send(ServerEvent::Error {
                            code: "decode_error".into(),
                            message: format!("Failed to parse daemon frame: {err}"),
                        });
                    }
                }
            }
        });

        let conn = Self {
            tx: req_tx,
            rx: ev_rx,
            buffered_events: VecDeque::new(),
            _child: Some(child),
        };
        conn.send(ClientRequest::Hello {
            client_version: build_version().to_string(),
        })?;
        Ok(conn)
    }

    #[cfg(test)]
    fn from_channels_for_test(
        tx: mpsc::UnboundedSender<ClientRequest>,
        rx: mpsc::UnboundedReceiver<ServerEvent>,
    ) -> Self {
        Self {
            tx,
            rx,
            buffered_events: VecDeque::new(),
            _child: None,
        }
    }

    pub fn send(&self, request: ClientRequest) -> Result<()> {
        self.tx
            .send(request)
            .map_err(|_| anyhow!("daemon write channel closed"))
    }

    pub async fn next_event(&mut self) -> Option<ServerEvent> {
        if let Some(ev) = self.buffered_events.pop_front() {
            return Some(ev);
        }
        self.rx.recv().await
    }

    pub fn try_next_event(&mut self) -> Option<ServerEvent> {
        if let Some(ev) = self.buffered_events.pop_front() {
            return Some(ev);
        }
        self.rx.try_recv().ok()
    }

    pub async fn next_event_timeout(&mut self, timeout: Duration) -> Result<Option<ServerEvent>> {
        if let Some(ev) = self.buffered_events.pop_front() {
            return Ok(Some(ev));
        }
        tokio::time::timeout(timeout, self.rx.recv())
            .await
            .map_err(|_| anyhow!("timed out waiting for daemon event after {timeout:?}"))
    }

    pub fn request_sender(&self) -> mpsc::UnboundedSender<ClientRequest> {
        self.tx.clone()
    }

    // ---- Slice 1 typed request helpers ----

    pub fn ping(&self, nonce: u64) -> Result<()> {
        self.send(ClientRequest::Ping { nonce })
    }

    pub fn list_sessions(&self, limit: Option<usize>) -> Result<()> {
        self.send(ClientRequest::ListSessions {
            limit,
            include_archived: None,
        })
    }

    pub fn read_history(
        &self,
        session_id: impl Into<String>,
        source_path: impl Into<String>,
        agent: AgentKind,
        tail: Option<usize>,
    ) -> Result<()> {
        self.send(ClientRequest::ReadHistory {
            session_id: session_id.into(),
            source_path: source_path.into(),
            agent,
            tail,
            before_offset: None,
            max_bytes: None,
            known_probe: None,
        })
    }

    pub fn watch_session(
        &self,
        session_id: impl Into<String>,
        source_path: impl Into<String>,
        agent: AgentKind,
    ) -> Result<()> {
        self.send(ClientRequest::WatchSession {
            session_id: session_id.into(),
            source_path: source_path.into(),
            agent,
        })
    }

    pub fn unwatch_session(&self) -> Result<()> {
        self.send(ClientRequest::UnwatchSession { session_id: None })
    }

    pub fn send_prompt(
        &self,
        session_id: impl Into<String>,
        agent: AgentKind,
        cwd: Option<String>,
        text: impl Into<String>,
    ) -> Result<()> {
        self.send(ClientRequest::SendPrompt {
            session_id: session_id.into(),
            agent,
            cwd,
            text: text.into(),
            options: None,
        })
    }

    // ---- Slice 2 typed request helpers (all 13) ----

    pub fn watch_directory(&self) -> Result<()> {
        self.send(ClientRequest::WatchDirectory)
    }

    pub fn unwatch_directory(&self) -> Result<()> {
        self.send(ClientRequest::UnwatchDirectory)
    }

    pub fn unwatch_session_by_id(&self, session_id: impl Into<String>) -> Result<()> {
        self.send(ClientRequest::UnwatchSession {
            session_id: Some(session_id.into()),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn read_history_page(
        &self,
        session_id: impl Into<String>,
        source_path: impl Into<String>,
        agent: AgentKind,
        tail: Option<usize>,
        before_offset: Option<u64>,
        max_bytes: Option<usize>,
        known_probe: Option<String>,
    ) -> Result<()> {
        self.send(ClientRequest::ReadHistory {
            session_id: session_id.into(),
            source_path: source_path.into(),
            agent,
            tail,
            before_offset,
            max_bytes,
            known_probe,
        })
    }

    pub fn interrupt_session(
        &self,
        session_id: impl Into<String>,
        agent: AgentKind,
    ) -> Result<()> {
        self.send(ClientRequest::InterruptSession {
            session_id: session_id.into(),
            agent,
        })
    }

    pub fn steer_session(
        &self,
        session_id: impl Into<String>,
        agent: AgentKind,
        cwd: Option<String>,
        text: impl Into<String>,
    ) -> Result<()> {
        self.send(ClientRequest::SteerSession {
            session_id: session_id.into(),
            agent,
            cwd,
            text: text.into(),
            options: None,
        })
    }

    pub fn cancel_queued_prompt(
        &self,
        session_id: impl Into<String>,
        message_id: impl Into<String>,
    ) -> Result<()> {
        self.send(ClientRequest::CancelQueuedPrompt {
            session_id: session_id.into(),
            message_id: message_id.into(),
        })
    }

    pub fn spawn_session(
        &self,
        agent: AgentKind,
        cwd: impl Into<String>,
        title: Option<String>,
        initial_prompt: Option<String>,
    ) -> Result<()> {
        self.send(ClientRequest::SpawnSession {
            agent,
            cwd: cwd.into(),
            title,
            initial_prompt,
            options: None,
        })
    }

    pub fn rename_session(
        &self,
        session_id: impl Into<String>,
        agent: AgentKind,
        source_path: impl Into<String>,
        title: impl Into<String>,
    ) -> Result<()> {
        self.send(ClientRequest::RenameSession {
            session_id: session_id.into(),
            agent,
            source_path: source_path.into(),
            title: title.into(),
        })
    }

    pub fn archive_session(
        &self,
        session_id: impl Into<String>,
        agent: AgentKind,
        source_path: impl Into<String>,
        kill_process: bool,
    ) -> Result<()> {
        self.send(ClientRequest::ArchiveSession {
            session_id: session_id.into(),
            agent,
            source_path: source_path.into(),
            kill_process,
        })
    }

    pub fn unarchive_session(&self, session_id: impl Into<String>) -> Result<()> {
        self.send(ClientRequest::UnarchiveSession {
            session_id: session_id.into(),
        })
    }

    pub fn search_history(
        &self,
        request_id: impl Into<String>,
        session_id: impl Into<String>,
        source_path: impl Into<String>,
        agent: AgentKind,
        query: impl Into<String>,
    ) -> Result<()> {
        self.send(ClientRequest::SearchHistory {
            request_id: request_id.into(),
            session_id: session_id.into(),
            source_path: source_path.into(),
            agent,
            query: query.into(),
        })
    }

    pub fn list_commands(&self, agent: AgentKind, cwd: Option<String>) -> Result<()> {
        self.send(ClientRequest::ListCommands { agent, cwd })
    }
}

/// Discovers bundled `multibotd-linux-x86_64` via `MULTIBOT_BUNDLED_DAEMON` env override
/// (if non-empty and an existing file) or beside `std::env::current_exe()`.
pub fn discover_bundled_linux_daemon() -> Option<PathBuf> {
    if let Some(env_val) = std::env::var_os("MULTIBOT_BUNDLED_DAEMON")
        && !env_val.is_empty()
    {
        let p = PathBuf::from(env_val);
        if p.is_file() {
            return Some(p);
        }
    }
    let beside = std::env::current_exe()
        .ok()?
        .parent()?
        .join("multibotd-linux-x86_64");
    beside.is_file().then_some(beside)
}

/// Strictly validates a versioned remote daemon filename (`multibotd-<ver>-<sha12>`)
/// to prevent shell injection or path traversal when interpolated into remote `/bin/sh`.
pub fn validate_remote_binary_name(name: &str) -> Result<()> {
    if !name.starts_with("multibotd-") {
        bail!("remote binary name must start with 'multibotd-': {name:?}");
    }
    if name.len() <= "multibotd-".len() || name.len() > 128 {
        bail!("remote binary name has invalid length: {}", name.len());
    }
    if name.contains("..") {
        bail!("remote binary name must not contain '..': {name:?}");
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
    {
        bail!("remote binary name contains disallowed characters: {name:?}");
    }
    Ok(())
}

/// Strictly validates SSH host configuration to prevent option injection (e.g. `-oProxyCommand=...`),
/// user/URI override (`root@host`, `ssh://...`), and shell metacharacter injection.
pub fn validate_ssh_host(host: &SshHostConfig) -> Result<()> {
    if host.port == 0 {
        bail!("SSH port must be > 0");
    }
    if host.hostname.chars().any(char::is_control) {
        bail!("SSH hostname contains control characters: {:?}", host.hostname);
    }
    if host.user.chars().any(char::is_control) {
        bail!("SSH user contains control characters: {:?}", host.user);
    }
    let hostname = host.hostname.trim();
    if hostname.is_empty() {
        bail!("SSH hostname cannot be empty");
    }
    if hostname.starts_with('-') {
        bail!("SSH hostname must not start with '-': {hostname:?}");
    }
    if hostname.contains('@') || hostname.contains("://") {
        bail!("SSH hostname must not contain '@' or URI scheme: {hostname:?}");
    }
    if !hostname.bytes().all(|b| {
        b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':' | b'[' | b']')
    }) {
        bail!("SSH hostname contains disallowed characters: {hostname:?}");
    }
    let user = host.user.trim();
    if !user.is_empty() {
        if user.starts_with('-') {
            bail!("SSH user must not start with '-': {user:?}");
        }
        if !user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        {
            bail!("SSH user contains disallowed characters: {user:?}");
        }
    }
    host.validated_identity_file()?;
    Ok(())
}

fn remote_dir_expr() -> String {
    REMOTE_DAEMON_DIR.replacen('~', "$HOME", 1)
}

fn remote_symlink_expr() -> String {
    REMOTE_DAEMON_SYMLINK.replacen('~', "$HOME", 1)
}

pub fn build_remote_check_command(binary_name: &str) -> Result<String> {
    validate_remote_binary_name(binary_name)?;
    let dir = remote_dir_expr();
    let symlink = remote_symlink_expr();
    Ok(format!(
        r#"test -f "{dir}/{binary_name}" && test -x "{dir}/{binary_name}" && ln -sfn "{dir}/{binary_name}" "{symlink}""#
    ))
}

pub fn build_remote_install_command(binary_name: &str, expected_bytes: usize) -> Result<String> {
    validate_remote_binary_name(binary_name)?;
    if expected_bytes == 0 {
        bail!("expected_bytes must be > 0");
    }
    let dir = remote_dir_expr();
    let symlink = remote_symlink_expr();
    Ok(format!(
        r#"umask 077 && mkdir -p "{dir}" && tmp="{dir}/{binary_name}.tmp.$$" && trap 'rm -f "$tmp"' EXIT INT TERM HUP && cat > "$tmp" && [ "$(wc -c < "$tmp")" -eq {expected_bytes} ] && chmod +x "$tmp" && mv -f "$tmp" "{dir}/{binary_name}" && ln -sfn "{dir}/{binary_name}" "{symlink}" && trap - EXIT INT TERM HUP"#
    ))
}

pub fn build_remote_launch_command() -> &'static str {
    r#""$HOME/.multibot/multibotd" --stdio"#
}

/// Returns `true` when `event` is `ServerEvent::HelloAck` and `daemon_deploy::needs_remote_upgrade`
/// requests an upgrade against `(expected_version, expected_hash)`, preserving newer daemons.
pub fn hello_ack_needs_upgrade(
    event: &ServerEvent,
    expected_version: &str,
    expected_hash: &str,
) -> bool {
    match event {
        ServerEvent::HelloAck {
            daemon_version,
            binary_hash,
            ..
        } => needs_remote_upgrade(daemon_version, binary_hash, expected_version, expected_hash),
        _ => false,
    }
}

/// Performs initial handshake and bounded 1-shot remote upgrade if the remote `HelloAck`
/// reports an outdated `daemon_version` or mismatched `binary_hash`.
/// Always re-buffers the accepted `HelloAck` (and any prior events) into `conn.buffered_events`
/// so callers calling `next_event()` / `try_next_event()` still receive `HelloAck` first.
async fn connect_with_upgrade_loop<FConnect, FDeploy, FutConnect, FutDeploy>(
    expected_version: &str,
    expected_hash: &str,
    ack_timeout: Duration,
    mut connect_once: FConnect,
    mut force_redeploy: FDeploy,
) -> Result<DaemonConnection>
where
    FConnect: FnMut() -> FutConnect,
    FutConnect: Future<Output = Result<DaemonConnection>>,
    FDeploy: FnMut() -> FutDeploy,
    FutDeploy: Future<Output = Result<()>>,
{
    let mut attempt = 0usize;
    loop {
        let mut conn = connect_once().await?;
        let mut pre_events = VecDeque::new();
        let deadline = tokio::time::Instant::now() + ack_timeout;
        let mut ack_event: Option<ServerEvent> = None;

        while let Ok(Some(ev)) = tokio::time::timeout_at(deadline, conn.rx.recv()).await {
            if matches!(ev, ServerEvent::HelloAck { .. }) {
                ack_event = Some(ev);
                break;
            }
            pre_events.push_back(ev);
        }

        let Some(ack) = ack_event else {
            // Did not receive HelloAck before EOF/timeout; preserve any buffered events and return.
            conn.buffered_events = pre_events;
            return Ok(conn);
        };

        if attempt < MAX_REMOTE_UPGRADE_ATTEMPTS
            && hello_ack_needs_upgrade(&ack, expected_version, expected_hash)
        {
            // Drop old connection (kills SSH child) and force atomic upload once.
            drop(conn);
            force_redeploy().await?;
            attempt += 1;
            continue;
        }

        // Re-buffer pre_events + accepted HelloAck so callers still receive HelloAck cleanly.
        pre_events.push_back(ack);
        conn.buffered_events = pre_events;
        return Ok(conn);
    }
}

fn apply_no_window(#[allow(unused_variables)] cmd: &mut Command) {
    #[cfg(target_os = "windows")]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
}

fn spawn_local(path: &Path) -> Result<Child> {
    let mut cmd = Command::new(path);
    cmd.arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    apply_no_window(&mut cmd);
    cmd.spawn()
        .with_context(|| format!("spawning local multibotd at {}", path.display()))
}

/// Builds the `ssh` argv for launching the remote `multibotd --stdio` process.
/// Appends `-i <identity_file>` before `--` when configured and valid (never shell-interpolated).
pub fn build_ssh_daemon_args(host: &SshHostConfig) -> Result<Vec<String>> {
    validate_ssh_host(host)?;
    let mut args = vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ServerAliveInterval=15".to_string(),
        "-o".to_string(),
        "ServerAliveCountMax=3".to_string(),
    ];
    if let Some(identity) = host.validated_identity_file()? {
        args.push("-i".to_string());
        args.push(identity.to_string());
    }
    args.push("-p".to_string());
    args.push(host.port.to_string());
    args.push("--".to_string());
    args.push(host.ssh_target());
    args.push(build_remote_launch_command().to_string());
    Ok(args)
}

/// Builds the `ssh` argv for one-shot remote `multibotd` probe/check or atomic upload commands.
/// Appends `-i <identity_file>` before `--` when configured and valid (never shell-interpolated).
pub fn build_ssh_exec_args(host: &SshHostConfig, remote_cmd: &str) -> Result<Vec<String>> {
    validate_ssh_host(host)?;
    let mut args = vec!["-o".to_string(), "BatchMode=yes".to_string()];
    if let Some(identity) = host.validated_identity_file()? {
        args.push("-i".to_string());
        args.push(identity.to_string());
    }
    args.push("-p".to_string());
    args.push(host.port.to_string());
    args.push("--".to_string());
    args.push(host.ssh_target());
    args.push(remote_cmd.to_string());
    Ok(args)
}

fn spawn_ssh_daemon(host: &SshHostConfig) -> Result<Child> {
    let args = build_ssh_daemon_args(host)?;
    let mut cmd = Command::new("ssh");
    cmd.args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    apply_no_window(&mut cmd);
    cmd.spawn()
        .with_context(|| format!("spawning ssh multibotd on {}", host.ssh_target()))
}

/// Atomically deploys `multibotd-<version>-<sha12>` to `$HOME/.multibot/` on the remote VPS.
async fn ensure_remote_daemon_deployed(
    host: &SshHostConfig,
    bytes: &[u8],
    binary_name: &str,
    force_upload: bool,
) -> Result<()> {
    validate_ssh_host(host)?;
    validate_remote_binary_name(binary_name)?;

    if !force_upload {
        let check_cmd = build_remote_check_command(binary_name)?;
        let check_args = build_ssh_exec_args(host, &check_cmd)?;
        let mut cmd = Command::new("ssh");
        cmd.args(&check_args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        apply_no_window(&mut cmd);
        if cmd.status().await.is_ok_and(|s| s.success()) {
            return Ok(());
        }
    }

    let install_cmd = build_remote_install_command(binary_name, bytes.len())?;
    let install_args = build_ssh_exec_args(host, &install_cmd)?;
    let mut cmd = Command::new("ssh");
    cmd.args(&install_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    apply_no_window(&mut cmd);
    let mut child = cmd.spawn().with_context(|| {
        format!(
            "spawning ssh upload for multibotd on {}",
            host.ssh_target()
        )
    })?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(bytes).await?;
        stdin.shutdown().await?;
    }
    let upload_status = child.wait().await?;
    if !upload_status.success() {
        return Err(anyhow!(
            "failed to deploy {binary_name} to {}",
            host.ssh_target()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    #[cfg(unix)]
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[cfg(unix)]
    static CLIENT_TEST_COUNTER: AtomicU64 = AtomicU64::new(1);

    #[cfg(unix)]
    fn temp_dir(tag: &str) -> PathBuf {
        let n = CLIENT_TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "multibot-core-client-{tag}-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn validate_remote_binary_name_accepts_canonical_and_rejects_injection() {
        assert!(validate_remote_binary_name("multibotd-0.1.0-2cf24dba5fb0").is_ok());
        assert!(validate_remote_binary_name("multibotd-0.1.42-abcdef012345").is_ok());

        for bad in [
            "",
            "multibotd-",
            "../evil",
            "multibotd-0.1.0;rm -rf /",
            "multibotd-0.1.0$(id)",
            "multibotd-0.1.0\nid",
            "multibotd-0.1.0/sub",
            "multibotd-0.1.0..abc",
            "other-0.1.0-abc",
            &format!("multibotd-{}", "a".repeat(130)),
        ] {
            assert!(
                validate_remote_binary_name(bad).is_err(),
                "expected rejection for {bad:?}"
            );
        }
    }

    #[test]
    fn validate_ssh_host_rejects_option_injection_and_bad_port() {
        let valid = SshHostConfig {
            name: "prod".into(),
            user: "ubuntu".into(),
            hostname: "10.0.0.1".into(),
            port: 22,
            identity_file: None,
        };
        assert!(validate_ssh_host(&valid).is_ok());

        let valid_empty_user = SshHostConfig {
            user: String::new(),
            ..valid.clone()
        };
        assert!(validate_ssh_host(&valid_empty_user).is_ok());

        let valid_ipv6 = SshHostConfig {
            hostname: "[2001:db8::1]".into(),
            user: "deploy_user-1.0".into(),
            ..valid.clone()
        };
        assert!(validate_ssh_host(&valid_ipv6).is_ok());

        let valid_with_key = SshHostConfig {
            identity_file: Some("C:/Users/Example User/ZigShell/ssh-key/multibot_ed25519".into()),
            ..valid.clone()
        };
        assert!(validate_ssh_host(&valid_with_key).is_ok());

        let bad_cases = [
            SshHostConfig { port: 0, ..valid.clone() },
            SshHostConfig { hostname: "".into(), ..valid.clone() },
            SshHostConfig { hostname: "-oProxyCommand=evil".into(), ..valid.clone() },
            SshHostConfig { user: "-oFoo".into(), ..valid.clone() },
            SshHostConfig { hostname: "host name".into(), ..valid.clone() },
            SshHostConfig { hostname: "host\nid".into(), ..valid.clone() },
            SshHostConfig { user: "alice@bob".into(), ..valid.clone() },
            SshHostConfig { hostname: "root@victim.com".into(), ..valid.clone() },
            SshHostConfig { hostname: "ssh://attacker@host:9999".into(), ..valid.clone() },
            SshHostConfig { hostname: "host;id".into(), ..valid.clone() },
            SshHostConfig { hostname: "host$(id)".into(), ..valid.clone() },
            SshHostConfig { hostname: "host`id`".into(), ..valid.clone() },
            SshHostConfig { hostname: "host|cat".into(), ..valid.clone() },
            SshHostConfig { hostname: "host&id".into(), ..valid.clone() },
            SshHostConfig { user: "user'bad".into(), ..valid.clone() },
            SshHostConfig { user: "user;id".into(), ..valid.clone() },
            SshHostConfig { identity_file: Some("".into()), ..valid.clone() },
            SshHostConfig { identity_file: Some("   ".into()), ..valid.clone() },
            SshHostConfig { identity_file: Some("-oProxyCommand=evil".into()), ..valid.clone() },
            SshHostConfig { identity_file: Some("/key\nProxyCommand=id".into()), ..valid.clone() },
            SshHostConfig { identity_file: Some("/key\rfoo".into()), ..valid.clone() },
            SshHostConfig { identity_file: Some("/key\0foo".into()), ..valid.clone() },
            SshHostConfig { identity_file: Some("/key\tfoo".into()), ..valid.clone() },
        ];
        for host in bad_cases {
            assert!(validate_ssh_host(&host).is_err(), "expected rejection for {host:?}");
        }
    }

    #[test]
    fn ssh_argv_includes_identity_file_before_double_dash_and_omits_when_none() {
        let base = SshHostConfig {
            name: "prod".into(),
            user: "ubuntu".into(),
            hostname: "10.0.0.1".into(),
            port: 2222,
            identity_file: None,
        };

        let daemon_none = build_ssh_daemon_args(&base).unwrap();
        let exec_none = build_ssh_exec_args(&base, "true").unwrap();
        assert!(!daemon_none.contains(&"-i".to_string()));
        assert!(!exec_none.contains(&"-i".to_string()));

        let with_key = SshHostConfig {
            identity_file: Some("  C:/Users/Example User/ZigShell/ssh-key/multibot_ed25519  ".into()),
            ..base.clone()
        };
        for args in [
            build_ssh_daemon_args(&with_key).unwrap(),
            build_ssh_exec_args(&with_key, "true").unwrap(),
        ] {
            let i_pos = args.iter().position(|a| a == "-i").expect("expected -i flag");
            let dd_pos = args.iter().position(|a| a == "--").expect("expected -- delimiter");
            assert!(i_pos + 1 < dd_pos, "-i <path> must appear before --: {args:?}");
            assert_eq!(
                args[i_pos + 1],
                "C:/Users/Example User/ZigShell/ssh-key/multibot_ed25519"
            );
        }

        for bad in ["-oProxyCommand=id", "/k\nbad", "/k\0bad", ""] {
            let host = SshHostConfig {
                identity_file: Some(bad.into()),
                ..base.clone()
            };
            assert!(build_ssh_daemon_args(&host).is_err());
            assert!(build_ssh_exec_args(&host, "true").is_err());
        }
    }

    #[tokio::test]
    async fn remote_deploy_commands_are_atomic_and_scoped_to_dot_multibot() {
        let name = "multibotd-0.1.9-2cf24dba5fb0";
        let payload = b"#!/bin/sh\necho multibotd\n";
        let check_cmd = build_remote_check_command(name).unwrap();
        let install_cmd = build_remote_install_command(name, payload.len()).unwrap();

        assert!(check_cmd.contains("test -f"));
        assert!(check_cmd.contains("test -x"));
        assert!(check_cmd.contains("$HOME/.multibot/multibotd-0.1.9-2cf24dba5fb0"));
        assert!(check_cmd.contains("ln -sfn"));
        assert!(check_cmd.contains("$HOME/.multibot/multibotd"));

        assert!(install_cmd.contains("umask 077"));
        assert!(install_cmd.contains(r#"mkdir -p "$HOME/.multibot""#));
        assert!(install_cmd.contains(".tmp.$$"));
        assert!(install_cmd.contains(r#"trap 'rm -f "$tmp"' EXIT INT TERM HUP"#));
        assert!(install_cmd.contains("trap - EXIT INT TERM HUP"));
        assert!(install_cmd.contains("wc -c"));
        assert!(install_cmd.contains("chmod +x"));
        assert!(install_cmd.contains("mv -f"));
        assert!(install_cmd.contains("ln -sfn"));
        assert!(!install_cmd.contains("ZigShell"));
        assert!(!install_cmd.contains("multiprompt"));

        // Execute the actual generated POSIX shell commands against an isolated HOME on Unix.
        #[cfg(unix)]
        {
            let fake_home = temp_dir("sh-deploy");

            // 1. Check fails before install
            let st = Command::new("sh")
                .args(["-c", &check_cmd])
                .env("HOME", &fake_home)
                .status()
                .await
                .unwrap();
            assert!(!st.success());

            // 2. Truncated upload fails non-zero, cleans up .tmp.*, and never creates symlink
            let mut trunc = Command::new("sh")
                .args(["-c", &install_cmd])
                .env("HOME", &fake_home)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            trunc.stdin.take().unwrap().write_all(b"short").await.unwrap();
            let trunc_st = trunc.wait().await.unwrap();
            assert!(!trunc_st.success());
            assert!(!fake_home.join(".multibot/multibotd").exists());
            let left_tmps: Vec<_> = std::fs::read_dir(fake_home.join(".multibot"))
                .unwrap()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
                .collect();
            assert!(left_tmps.is_empty(), "truncated upload leaked tmp files");

            // 2b. Mid-upload SIGHUP cleans up .tmp.$$ cleanly under /bin/sh
            {
                let hup_install = build_remote_install_command(name, 100_000).unwrap();
                let mut hup_child = Command::new("/bin/sh")
                    .args(["-c", &hup_install])
                    .env("HOME", &fake_home)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap();
                let hup_pid = hup_child.id().unwrap();
                let mut hup_stdin = hup_child.stdin.take().unwrap();
                hup_stdin.write_all(b"partial-bytes-before-hup").await.unwrap();
                hup_stdin.flush().await.unwrap();
                tokio::time::sleep(Duration::from_millis(60)).await;
                let _ = Command::new("kill")
                    .args(["-s", "HUP", &hup_pid.to_string()])
                    .status()
                    .await;
                drop(hup_stdin);
                let hup_st = hup_child.wait().await.unwrap();
                assert!(!hup_st.success());
                let left_after_hup: Vec<_> = std::fs::read_dir(fake_home.join(".multibot"))
                    .unwrap()
                    .flatten()
                    .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
                    .collect();
                assert!(
                    left_after_hup.is_empty(),
                    "SIGHUP mid-upload leaked tmp files: {left_after_hup:?}"
                );
            }

            // 3. Full upload succeeds, sets executable, updates symlink, and check now passes!
            let mut full = Command::new("sh")
                .args(["-c", &install_cmd])
                .env("HOME", &fake_home)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            full.stdin.take().unwrap().write_all(payload).await.unwrap();
            let full_st = full.wait().await.unwrap();
            assert!(full_st.success());
            assert!(fake_home.join(".multibot/multibotd").exists());
            assert_eq!(
                std::fs::read(fake_home.join(".multibot").join(name)).unwrap(),
                payload
            );

            let st2 = Command::new("sh")
                .args(["-c", &check_cmd])
                .env("HOME", &fake_home)
                .status()
                .await
                .unwrap();
            assert!(st2.success());

            let _ = std::fs::remove_dir_all(&fake_home);
        }
    }

    #[tokio::test]
    async fn hello_ack_upgrade_state_machine_upgrades_once_and_never_loops() {
        let make_conn = |ver: &str, hash: &str| {
            let (req_tx, _req_rx) = mpsc::unbounded_channel();
            let (ev_tx, ev_rx) = mpsc::unbounded_channel();
            let _ = ev_tx.send(ServerEvent::HelloAck {
                protocol_version: 1,
                daemon_version: ver.to_string(),
                hostname: "vps".into(),
                pid: 42,
                binary_hash: hash.to_string(),
            });
            DaemonConnection::from_channels_for_test(req_tx, ev_rx)
        };

        // Case A: version + hash match -> 0 redeploys, 1 connect, HelloAck buffered first
        let connects = Arc::new(AtomicUsize::new(0));
        let deploys = Arc::new(AtomicUsize::new(0));
        let (c_a, d_a) = (Arc::clone(&connects), Arc::clone(&deploys));
        let mut conn = connect_with_upgrade_loop(
            "0.1.5",
            "abcdef012345",
            Duration::from_secs(1),
            move || {
                c_a.fetch_add(1, Ordering::SeqCst);
                let conn = make_conn("0.1.5", "abcdef012345");
                async move { Ok(conn) }
            },
            move || {
                d_a.fetch_add(1, Ordering::SeqCst);
                async move { Ok(()) }
            },
        )
        .await
        .unwrap();
        assert_eq!(connects.load(Ordering::SeqCst), 1);
        assert_eq!(deploys.load(Ordering::SeqCst), 0);
        match conn.next_event().await.unwrap() {
            ServerEvent::HelloAck { daemon_version, binary_hash, .. } => {
                assert_eq!(daemon_version, "0.1.5");
                assert_eq!(binary_hash, "abcdef012345");
            }
            other => panic!("expected HelloAck, got {other:?}"),
        }

        // Case B: version mismatch on connect 1, match on connect 2 -> 1 redeploy, 2 connects, second HelloAck returned
        let connects = Arc::new(AtomicUsize::new(0));
        let deploys = Arc::new(AtomicUsize::new(0));
        let (c_b, d_b) = (Arc::clone(&connects), Arc::clone(&deploys));
        let mut conn = connect_with_upgrade_loop(
            "0.1.5",
            "abcdef012345",
            Duration::from_secs(1),
            move || {
                let n = c_b.fetch_add(1, Ordering::SeqCst);
                let conn = if n == 0 {
                    make_conn("0.1.4", "111111111111")
                } else {
                    make_conn("0.1.5", "abcdef012345")
                };
                async move { Ok(conn) }
            },
            move || {
                d_b.fetch_add(1, Ordering::SeqCst);
                async move { Ok(()) }
            },
        )
        .await
        .unwrap();
        assert_eq!(connects.load(Ordering::SeqCst), 2);
        assert_eq!(deploys.load(Ordering::SeqCst), 1);
        match conn.next_event().await.unwrap() {
            ServerEvent::HelloAck { daemon_version, .. } => assert_eq!(daemon_version, "0.1.5"),
            other => panic!("expected updated HelloAck, got {other:?}"),
        }

        // Case C: hash drift on same version -> 1 redeploy
        let connects = Arc::new(AtomicUsize::new(0));
        let deploys = Arc::new(AtomicUsize::new(0));
        let (c_c, d_c) = (Arc::clone(&connects), Arc::clone(&deploys));
        let _conn = connect_with_upgrade_loop(
            "0.1.5",
            "abcdef012345",
            Duration::from_secs(1),
            move || {
                let n = c_c.fetch_add(1, Ordering::SeqCst);
                let conn = if n == 0 {
                    make_conn("0.1.5", "999999999999")
                } else {
                    make_conn("0.1.5", "abcdef012345")
                };
                async move { Ok(conn) }
            },
            move || {
                d_c.fetch_add(1, Ordering::SeqCst);
                async move { Ok(()) }
            },
        )
        .await
        .unwrap();
        assert_eq!(connects.load(Ordering::SeqCst), 2);
        assert_eq!(deploys.load(Ordering::SeqCst), 1);

        // Case D: persistent mismatch -> stops after exactly MAX_REMOTE_UPGRADE_ATTEMPTS (1)
        let connects = Arc::new(AtomicUsize::new(0));
        let deploys = Arc::new(AtomicUsize::new(0));
        let (c_d, d_d) = (Arc::clone(&connects), Arc::clone(&deploys));
        let mut conn = connect_with_upgrade_loop(
            "0.1.5",
            "abcdef012345",
            Duration::from_secs(1),
            move || {
                c_d.fetch_add(1, Ordering::SeqCst);
                let conn = make_conn("0.1.0", "000000000000");
                async move { Ok(conn) }
            },
            move || {
                d_d.fetch_add(1, Ordering::SeqCst);
                async move { Ok(()) }
            },
        )
        .await
        .unwrap();
        assert_eq!(connects.load(Ordering::SeqCst), 2);
        assert_eq!(deploys.load(Ordering::SeqCst), MAX_REMOTE_UPGRADE_ATTEMPTS);
        assert!(matches!(
            conn.try_next_event(),
            Some(ServerEvent::HelloAck { daemon_version, .. }) if daemon_version == "0.1.0"
        ));
    }

    #[test]
    fn typed_request_helpers_encode_all_slice1_and_slice2_requests() {
        let (req_tx, mut req_rx) = mpsc::unbounded_channel();
        let (_ev_tx, ev_rx) = mpsc::unbounded_channel();
        let conn = DaemonConnection::from_channels_for_test(req_tx, ev_rx);

        // Slice 1 helpers (6)
        conn.ping(42).unwrap();
        conn.list_sessions(Some(50)).unwrap();
        conn.read_history("s1", "/p.jsonl", AgentKind::Claude, Some(20))
            .unwrap();
        conn.watch_session("s1", "/p.jsonl", AgentKind::Codex)
            .unwrap();
        conn.unwatch_session().unwrap();
        conn.send_prompt("s1", AgentKind::Claude, Some("/repo".into()), "hi")
            .unwrap();

        // Slice 2 helpers (13)
        conn.watch_directory().unwrap();
        conn.unwatch_directory().unwrap();
        conn.unwatch_session_by_id("s1").unwrap();
        conn.read_history_page(
            "s1",
            "/p.jsonl",
            AgentKind::Claude,
            None,
            Some(1024),
            Some(512 * 1024),
            Some("abc".into()),
        )
        .unwrap();
        conn.interrupt_session("s1", AgentKind::Claude).unwrap();
        conn.steer_session("s1", AgentKind::Codex, Some("/repo".into()), "steer")
            .unwrap();
        conn.cancel_queued_prompt("s1", "q-1").unwrap();
        conn.spawn_session(
            AgentKind::Antigravity,
            "/repo",
            Some("Title".into()),
            Some("init".into()),
        )
        .unwrap();
        conn.rename_session("s1", AgentKind::Claude, "/p.jsonl", "New Title")
            .unwrap();
        conn.archive_session("s1", AgentKind::Claude, "/p.jsonl", true)
            .unwrap();
        conn.unarchive_session("s1").unwrap();
        conn.search_history("r1", "s1", "/p.jsonl", AgentKind::Claude, "needle")
            .unwrap();
        conn.list_commands(AgentKind::Claude, Some("/repo".into()))
            .unwrap();

        let mut received = Vec::new();
        while let Ok(req) = req_rx.try_recv() {
            received.push(req);
        }
        assert_eq!(received.len(), 19);
        assert_eq!(received[0], ClientRequest::Ping { nonce: 42 });
        assert_eq!(received[4], ClientRequest::UnwatchSession { session_id: None });
        assert_eq!(received[6], ClientRequest::WatchDirectory);
        assert_eq!(received[7], ClientRequest::UnwatchDirectory);
        assert_eq!(
            received[8],
            ClientRequest::UnwatchSession {
                session_id: Some("s1".into())
            }
        );
        assert_eq!(
            received[9],
            ClientRequest::ReadHistory {
                session_id: "s1".into(),
                source_path: "/p.jsonl".into(),
                agent: AgentKind::Claude,
                tail: None,
                before_offset: Some(1024),
                max_bytes: Some(512 * 1024),
                known_probe: Some("abc".into()),
            }
        );
        assert_eq!(
            received[18],
            ClientRequest::ListCommands {
                agent: AgentKind::Claude,
                cwd: Some("/repo".into()),
            }
        );
    }

    #[test]
    fn validate_ssh_host_rejects_trailing_control_chars_before_trim() {
        let bad_host = SshHostConfig {
            name: "vps".into(),
            hostname: "10.0.0.1\n".into(),
            user: "ubuntu".into(),
            port: 22,
            identity_file: None,
        };
        assert!(validate_ssh_host(&bad_host).is_err());

        let bad_user = SshHostConfig {
            name: "vps".into(),
            hostname: "10.0.0.1".into(),
            user: "ubuntu\r".into(),
            port: 22,
            identity_file: None,
        };
        assert!(validate_ssh_host(&bad_user).is_err());
    }
}
