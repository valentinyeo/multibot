use crate::settings::SshHostConfig;
use anyhow::{Context, Result, anyhow};
use multibot_proto::{ClientRequest, ServerEvent, decode_line, encode_line};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

pub const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone)]
pub enum DaemonTarget {
    LocalBinary(PathBuf),
    RemoteSsh {
        host: SshHostConfig,
        /// Optional local `multibotd-linux-x86_64` binary to auto-upload if missing on VPS.
        bundled_linux_binary: Option<PathBuf>,
    },
}

pub struct DaemonConnection {
    tx: mpsc::UnboundedSender<ClientRequest>,
    rx: mpsc::UnboundedReceiver<ServerEvent>,
    _child: Child,
}

impl DaemonConnection {
    pub async fn connect(target: DaemonTarget) -> Result<Self> {
        let mut child = match target {
            DaemonTarget::LocalBinary(path) => spawn_local(&path)?,
            DaemonTarget::RemoteSsh {
                host,
                bundled_linux_binary,
            } => {
                if let Some(local_bin) = bundled_linux_binary
                    && local_bin.is_file()
                {
                    let _ = ensure_remote_daemon_deployed(&host, &local_bin).await;
                }
                spawn_ssh_daemon(&host)?
            }
        };

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
            _child: child,
        };
        conn.send(ClientRequest::Hello {
            client_version: DAEMON_VERSION.to_string(),
        })?;
        Ok(conn)
    }

    pub fn send(&self, request: ClientRequest) -> Result<()> {
        self.tx
            .send(request)
            .map_err(|_| anyhow!("daemon write channel closed"))
    }

    pub async fn next_event(&mut self) -> Option<ServerEvent> {
        self.rx.recv().await
    }

    pub fn try_next_event(&mut self) -> Option<ServerEvent> {
        self.rx.try_recv().ok()
    }
}

fn spawn_local(path: &Path) -> Result<Child> {
    let mut cmd = Command::new(path);
    cmd.arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(target_os = "windows")]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.spawn()
        .with_context(|| format!("spawning local multibotd at {}", path.display()))
}

fn spawn_ssh_daemon(host: &SshHostConfig) -> Result<Child> {
    let mut cmd = Command::new("ssh");
    cmd.args([
        "-o",
        "BatchMode=yes",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
        "-p",
        &host.port.to_string(),
        &host.ssh_target(),
        "~/.multibot/multibotd --stdio",
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .kill_on_drop(true);
    #[cfg(target_os = "windows")]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.spawn()
        .with_context(|| format!("spawning ssh multibotd on {}", host.ssh_target()))
}

/// Auto-deploys `multibotd-<version>` to `~/.multibot/` on the VPS if not already installed.
async fn ensure_remote_daemon_deployed(host: &SshHostConfig, local_bin: &Path) -> Result<()> {
    let versioned_path = format!("~/.multibot/multibotd-{DAEMON_VERSION}");
    let check_cmd = format!(
        "test -x {versioned_path} && ln -sf multibotd-{DAEMON_VERSION} ~/.multibot/multibotd"
    );
    let status = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-p",
            &host.port.to_string(),
            &host.ssh_target(),
            &check_cmd,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await?;

    if status.success() {
        return Ok(());
    }

    let bytes = tokio::fs::read(local_bin)
        .await
        .with_context(|| format!("reading bundled daemon {}", local_bin.display()))?;

    let install_cmd = format!(
        "mkdir -p ~/.multibot && cat > {versioned_path}.tmp && chmod +x {versioned_path}.tmp && mv -f {versioned_path}.tmp {versioned_path} && ln -sf multibotd-{DAEMON_VERSION} ~/.multibot/multibotd"
    );
    let mut child = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-p",
            &host.port.to_string(),
            &host.ssh_target(),
            &install_cmd,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&bytes).await?;
        stdin.shutdown().await?;
    }
    let upload_status = child.wait().await?;
    if !upload_status.success() {
        return Err(anyhow!(
            "failed to deploy multibotd to {}",
            host.ssh_target()
        ));
    }
    Ok(())
}
