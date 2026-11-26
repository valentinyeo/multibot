mod providers;

use anyhow::Result;
use multibot_proto::{
    AgentKind, ClientRequest, PROTOCOL_VERSION, ServerEvent, decode_line, encode_line,
};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use providers::{discover_all_sessions, read_messages_from_file};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

struct ActiveWatch {
    _watcher: RecommendedWatcher,
    stop_tx: mpsc::Sender<()>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ServerEvent>();

    let writer_task = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(ev) = out_rx.recv().await {
            let Ok(line) = encode_line(&ev) else {
                continue;
            };
            if stdout.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if stdout.flush().await.is_err() {
                break;
            }
        }
    });

    let mut active_watch: Option<ActiveWatch> = None;
    let mut stdin_lines = BufReader::new(tokio::io::stdin()).lines();

    while let Ok(Some(line)) = stdin_lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let req = match decode_line::<ClientRequest>(&line) {
            Ok(req) => req,
            Err(err) => {
                let _ = out_tx.send(ServerEvent::Error {
                    code: "bad_request".into(),
                    message: format!("Invalid request JSON: {err}"),
                });
                continue;
            }
        };

        match req {
            ClientRequest::Hello { .. } => {
                let _ = out_tx.send(ServerEvent::HelloAck {
                    protocol_version: PROTOCOL_VERSION,
                    daemon_version: env!("CARGO_PKG_VERSION").to_string(),
                    hostname: read_hostname(),
                    pid: std::process::id(),
                });
            }
            ClientRequest::Ping { nonce } => {
                let _ = out_tx.send(ServerEvent::Pong {
                    nonce,
                    server_time_ms: now_ms(),
                });
            }
            ClientRequest::ListSessions { limit } => {
                let sessions = discover_all_sessions(&home, limit.unwrap_or(50));
                let _ = out_tx.send(ServerEvent::Sessions { sessions });
            }
            ClientRequest::ReadHistory {
                session_id,
                source_path,
                agent,
                tail,
            } => match read_messages_from_file(agent, PathBuf::from(&source_path).as_path(), 0, tail)
            {
                Ok((messages, _)) => {
                    let _ = out_tx.send(ServerEvent::History {
                        session_id,
                        messages,
                    });
                }
                Err(err) => {
                    let _ = out_tx.send(ServerEvent::Error {
                        code: "read_history_failed".into(),
                        message: format!("Failed to read {source_path}: {err}"),
                    });
                }
            },
            ClientRequest::WatchSession {
                session_id,
                source_path,
                agent,
            } => {
                if let Some(prev) = active_watch.take() {
                    let _ = prev.stop_tx.try_send(());
                }
                let path = PathBuf::from(&source_path);
                let initial_offset = match read_messages_from_file(agent, &path, 0, Some(200)) {
                    Ok((messages, offset)) => {
                        let _ = out_tx.send(ServerEvent::History {
                            session_id: session_id.clone(),
                            messages,
                        });
                        offset
                    }
                    Err(err) => {
                        let _ = out_tx.send(ServerEvent::Error {
                            code: "watch_failed".into(),
                            message: format!("Failed to open {source_path}: {err}"),
                        });
                        continue;
                    }
                };

                match start_inotify_watch(
                    session_id,
                    path,
                    agent,
                    initial_offset,
                    out_tx.clone(),
                ) {
                    Ok(watch) => active_watch = Some(watch),
                    Err(err) => {
                        let _ = out_tx.send(ServerEvent::Error {
                            code: "watch_init_failed".into(),
                            message: format!("Failed to watch {source_path}: {err}"),
                        });
                    }
                }
            }
            ClientRequest::UnwatchSession => {
                if let Some(prev) = active_watch.take() {
                    let _ = prev.stop_tx.try_send(());
                }
            }
            ClientRequest::SendPrompt {
                session_id,
                agent,
                cwd,
                text,
            } => match dispatch_prompt(&session_id, agent, cwd.as_deref(), &text).await {
                Ok(detail) => {
                    let _ = out_tx.send(ServerEvent::SendAccepted { session_id, detail });
                }
                Err(err) => {
                    let _ = out_tx.send(ServerEvent::Error {
                        code: "send_failed".into(),
                        message: format!("Failed to send prompt: {err}"),
                    });
                }
            },
        }
    }

    drop(out_tx);
    let _ = writer_task.await;
    Ok(())
}

fn start_inotify_watch(
    session_id: String,
    path: PathBuf,
    agent: AgentKind,
    mut offset: u64,
    out_tx: mpsc::UnboundedSender<ServerEvent>,
) -> Result<ActiveWatch> {
    let (fs_tx, mut fs_rx) = mpsc::unbounded_channel::<()>();
    let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
    let target_name = path.file_name().map(|s| s.to_os_string());

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        if let Ok(event) = res {
            let matches = target_name.as_ref().is_none_or(|name| {
                event
                    .paths
                    .iter()
                    .any(|p| p.file_name().is_some_and(|n| n == name))
            });
            if matches {
                let _ = fs_tx.send(());
            }
        }
    })?;

    let watch_target = path.parent().unwrap_or(&path);
    watcher.watch(watch_target, RecursiveMode::NonRecursive)?;

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = stop_rx.recv() => break,
                maybe = fs_rx.recv() => {
                    if maybe.is_none() {
                        break;
                    }
                    // Drain bursts so rapid writes collapse into one delta read.
                    while fs_rx.try_recv().is_ok() {}
                    if let Ok((messages, new_offset)) = read_messages_from_file(agent, &path, offset, None) {
                        offset = new_offset;
                        if !messages.is_empty() {
                            let _ = out_tx.send(ServerEvent::HistoryAppend {
                                session_id: session_id.clone(),
                                messages,
                            });
                        }
                    }
                }
            }
        }
    });

    Ok(ActiveWatch {
        _watcher: watcher,
        stop_tx,
    })
}

async fn dispatch_prompt(
    session_id: &str,
    agent: AgentKind,
    cwd: Option<&str>,
    text: &str,
) -> Result<String> {
    let (bin, args): (&str, Vec<String>) = match agent {
        AgentKind::Claude => (
            "claude",
            vec![
                "--resume".into(),
                session_id.into(),
                "-p".into(),
                text.into(),
            ],
        ),
        AgentKind::Codex => (
            "codex",
            vec![
                "exec".into(),
                "resume".into(),
                session_id.into(),
                text.into(),
            ],
        ),
        AgentKind::Antigravity => (
            "agy",
            vec!["--resume".into(), session_id.into(), text.into()],
        ),
    };

    let mut cmd = Command::new(bin);
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(dir) = cwd
        && !dir.trim().is_empty()
    {
        cmd.current_dir(dir);
    }
    let child = cmd.spawn()?;
    let pid = child.id().unwrap_or(0);
    Ok(format!("dispatched {bin} (pid {pid})"))
}

fn read_hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .unwrap_or_else(|| "unknown-host".to_string())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
