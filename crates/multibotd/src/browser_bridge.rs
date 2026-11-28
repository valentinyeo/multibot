use anyhow::{Context, Result, bail, ensure};
use multibot_proto::{
    BrowserCommand, BrowserResponse, ClientRequest, MAX_BROWSER_RESULT_BYTES, ServerEvent,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{mpsc, oneshot},
};

const MAX_REQUEST_BYTES: u64 = 64 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum LocalRequest {
    Probe {
        session_id: String,
    },
    Execute {
        session_id: String,
        command: BrowserCommand,
    },
}

struct Pending {
    session_id: String,
    binding_id: String,
    tx: oneshot::Sender<BrowserResponse>,
}

#[derive(Default)]
struct State {
    bindings: HashMap<String, HashSet<String>>,
    pending: HashMap<u64, Pending>,
    next_id: u64,
}

#[derive(Clone, Default)]
pub(crate) struct BrowserBridge(Arc<Mutex<State>>);

pub(crate) struct SocketGuard(PathBuf);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn error(message: impl Into<String>) -> BrowserResponse {
    BrowserResponse::Error {
        message: message.into(),
    }
}

impl BrowserBridge {
    pub(crate) fn handle(&self, request: ClientRequest) {
        let mut state = self.0.lock().unwrap();
        match request {
            ClientRequest::BrowserBind {
                session_id,
                binding_id,
            } => {
                state
                    .bindings
                    .entry(session_id)
                    .or_default()
                    .insert(binding_id);
            }
            ClientRequest::BrowserUnbind {
                session_id,
                binding_id,
            } => {
                if let Some(bindings) = state.bindings.get_mut(&session_id) {
                    bindings.remove(&binding_id);
                    if bindings.is_empty() {
                        state.bindings.remove(&session_id);
                    }
                }
                let ids: Vec<_> = state
                    .pending
                    .iter()
                    .filter(|(_, p)| p.session_id == session_id && p.binding_id == binding_id)
                    .map(|(id, _)| *id)
                    .collect();
                for id in ids {
                    if let Some(pending) = state.pending.remove(&id) {
                        let _ = pending.tx.send(error("Browser pane closed"));
                    }
                }
            }
            ClientRequest::BrowserResult {
                request_id,
                binding_id,
                response,
            } => {
                if state
                    .pending
                    .get(&request_id)
                    .is_some_and(|p| p.binding_id == binding_id)
                    && let Some(pending) = state.pending.remove(&request_id)
                {
                    let response = match serde_json::to_vec(&response) {
                        Ok(data) if data.len() <= MAX_BROWSER_RESULT_BYTES => response,
                        _ => error("Browser response exceeds 512 KiB"),
                    };
                    let _ = pending.tx.send(response);
                }
            }
            _ => unreachable!(),
        }
    }

    pub(crate) fn listen(
        &self,
        home: &Path,
        out: mpsc::UnboundedSender<ServerEvent>,
    ) -> Result<SocketGuard> {
        let root = socket_root(home);
        for directory in [home.join(".multibot"), root.clone()] {
            match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    ensure!(
                        std::fs::symlink_metadata(&directory)?.is_dir(),
                        "Browser socket directory is redirected"
                    );
                }
                Err(e) => return Err(e.into()),
            }
        }
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
        let path = root.join(format!("{}.sock", std::process::id()));
        let listener =
            UnixListener::bind(&path).context("Could not bind browser control socket")?;
        let guard = SocketGuard(path.clone());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let bridge = self.clone();
        tokio::spawn(async move {
            // Bound in-flight sockets, not just commands, so idle clients cannot grow daemon RAM.
            let slots = Arc::new(tokio::sync::Semaphore::new(4));
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let Ok(slot) = slots.clone().try_acquire_owned() else {
                    continue;
                };
                let bridge = bridge.clone();
                let out = out.clone();
                tokio::spawn(async move {
                    let _slot = slot;
                    let _ =
                        tokio::time::timeout(Duration::from_secs(22), bridge.serve(stream, out))
                            .await;
                });
            }
        });
        Ok(guard)
    }

    async fn serve(
        &self,
        stream: UnixStream,
        out: mpsc::UnboundedSender<ServerEvent>,
    ) -> Result<()> {
        use tokio::io::AsyncReadExt;
        let (reader, mut writer) = stream.into_split();
        let mut line = String::new();
        BufReader::new(reader.take(MAX_REQUEST_BYTES + 1))
            .read_line(&mut line)
            .await?;
        ensure!(
            line.len() as u64 <= MAX_REQUEST_BYTES && line.ends_with('\n'),
            "Browser request too large or incomplete"
        );
        let response = match serde_json::from_str::<LocalRequest>(&line)? {
            LocalRequest::Probe { session_id } => {
                let count = self
                    .0
                    .lock()
                    .unwrap()
                    .bindings
                    .get(&session_id)
                    .map_or(0, HashSet::len);
                BrowserResponse::Value {
                    value: json!(count),
                }
            }
            LocalRequest::Execute {
                session_id,
                command,
            } => self.execute(session_id, command, out).await,
        };
        let mut bytes = serde_json::to_vec(&response)?;
        bytes.push(b'\n');
        writer.write_all(&bytes).await?;
        Ok(())
    }

    async fn execute(
        &self,
        session_id: String,
        command: BrowserCommand,
        out: mpsc::UnboundedSender<ServerEvent>,
    ) -> BrowserResponse {
        let (id, binding_id, rx) = {
            let mut state = self.0.lock().unwrap();
            let Some(bindings) = state.bindings.get(&session_id) else {
                return error("No docked browser is bound to this session");
            };
            if bindings.len() != 1 {
                return error(
                    "Session has browsers in multiple desktop windows; close the extra pane",
                );
            }
            let binding_id = bindings.iter().next().unwrap().clone();
            if state.pending.len() >= 4 {
                return error("Browser bridge is busy; retry shortly");
            }
            state.next_id += 1;
            let id = state.next_id;
            let (tx, rx) = oneshot::channel();
            state.pending.insert(
                id,
                Pending {
                    session_id: session_id.clone(),
                    binding_id: binding_id.clone(),
                    tx,
                },
            );
            (id, binding_id, rx)
        };
        let response = if out
            .send(ServerEvent::BrowserCommand {
                request_id: id,
                session_id,
                binding_id,
                command,
            })
            .is_err()
        {
            error("Desktop disconnected")
        } else {
            match tokio::time::timeout(Duration::from_secs(18), rx).await {
                Ok(Ok(response)) => response,
                _ => error("Desktop did not answer the browser command"),
            }
        };
        self.0.lock().unwrap().pending.remove(&id);
        response
    }
}

fn socket_root(home: &Path) -> PathBuf {
    home.join(".multibot/browser-control")
}

async fn exchange(path: &Path, request: &LocalRequest) -> Result<BrowserResponse> {
    use tokio::io::AsyncReadExt;
    let stream = UnixStream::connect(path).await?;
    let (read, mut write) = stream.into_split();
    let mut bytes = serde_json::to_vec(request)?;
    ensure!(
        bytes.len() < MAX_REQUEST_BYTES as usize,
        "Browser command exceeds 64 KiB"
    );
    bytes.push(b'\n');
    write.write_all(&bytes).await?;
    let mut line = String::new();
    BufReader::new(read.take(MAX_BROWSER_RESULT_BYTES as u64 + 1))
        .read_line(&mut line)
        .await?;
    ensure!(
        line.len() <= MAX_BROWSER_RESULT_BYTES && line.ends_with('\n'),
        "Browser response too large or incomplete"
    );
    Ok(serde_json::from_str(&line)?)
}

pub(crate) fn cli(args: &[String]) -> Result<()> {
    if args.is_empty() || args == ["--help"] {
        println!(
            "multibotd browser --session <id> [--daemon <pid>] navigate <url>|tabs|activate <target>|text|click <selector>|type <selector> <text>|eval <expression>|screenshot|cdp <method> <params-json>\nOpen the session's docked browser first. Results are JSON; screenshots contain base64 PNG data."
        );
        return Ok(());
    }
    let (session_id, daemon, command) = parse_args(args)?;
    let home = dirs::home_dir().context("Home directory unavailable")?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let response = runtime.block_on(async {
        let root = socket_root(&home);
        let mut candidates = Vec::new();
        if let Some(pid) = daemon {
            candidates.push(root.join(format!("{pid}.sock")));
        } else {
            for entry in
                std::fs::read_dir(&root).context("No connected Multibot desktop browser bridge")?
            {
                let path = entry?.path();
                if path
                    .extension()
                    .is_some_and(|extension| extension == "sock")
                {
                    candidates.push(path);
                }
            }
        }
        let mut matching = Vec::new();
        for path in candidates {
            let probe = LocalRequest::Probe {
                session_id: session_id.clone(),
            };
            if let Ok(Ok(BrowserResponse::Value { value })) =
                tokio::time::timeout(Duration::from_secs(1), exchange(&path, &probe)).await
                && value.as_u64().is_some_and(|count| count > 0)
            {
                matching.push(path);
            }
        }
        ensure!(
            !matching.is_empty(),
            "No connected desktop has a docked browser for this session"
        );
        ensure!(
            matching.len() == 1,
            "Session is connected to multiple desktops; use --daemon <pid>"
        );
        tokio::time::timeout(
            Duration::from_secs(21),
            exchange(
                &matching[0],
                &LocalRequest::Execute {
                    session_id,
                    command,
                },
            ),
        )
        .await
        .context("Browser bridge timed out")?
    })?;
    println!("{}", serde_json::to_string(&response)?);
    if let BrowserResponse::Error { message } = response {
        bail!("{message}");
    }
    Ok(())
}

fn parse_args(args: &[String]) -> Result<(String, Option<u32>, BrowserCommand)> {
    let mut args = args.iter().map(String::as_str);
    ensure!(args.next() == Some("--session"), "Expected --session <id>");
    let session = args
        .next()
        .filter(|s| !s.is_empty())
        .context("Missing session id")?
        .to_owned();
    let mut next = args.next().context("Missing browser command")?;
    let daemon = if next == "--daemon" {
        let pid = args.next().context("Missing daemon PID")?.parse::<u32>()?;
        next = args.next().context("Missing browser command")?;
        Some(pid)
    } else {
        None
    };
    let mut arg = || {
        args.next()
            .context("Missing command argument")
            .map(str::to_owned)
    };
    let command = match next {
        "navigate" => BrowserCommand::Navigate { url: arg()? },
        "tabs" => BrowserCommand::Tabs,
        "activate" => BrowserCommand::ActivateTab { target_id: arg()? },
        "text" => BrowserCommand::ReadText,
        "click" => BrowserCommand::Click { selector: arg()? },
        "type" => BrowserCommand::Type {
            selector: arg()?,
            text: arg()?,
        },
        "eval" => BrowserCommand::Eval { expression: arg()? },
        "screenshot" => BrowserCommand::Screenshot,
        "cdp" => BrowserCommand::RawCdp {
            method: arg()?,
            params: serde_json::from_str(&arg()?)?,
        },
        _ => bail!("Unknown browser command; run multibotd browser --help"),
    };
    ensure!(
        args.next().is_none(),
        "Unexpected extra arguments; quote text and expressions"
    );
    Ok((session, daemon, command))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bridge_routes_responses_only_to_the_bound_session_and_cleans_pending() {
        let bridge = BrowserBridge::default();
        for (session, binding) in [("a", "window-a"), ("b", "window-b")] {
            bridge.handle(ClientRequest::BrowserBind {
                session_id: session.into(),
                binding_id: binding.into(),
            });
        }
        let (out, mut rx) = mpsc::unbounded_channel();
        let request = bridge.execute("a".into(), BrowserCommand::ReadText, out);
        let desktop = async {
            let ServerEvent::BrowserCommand {
                request_id,
                session_id,
                binding_id,
                command,
            } = rx.recv().await.unwrap()
            else {
                panic!()
            };
            assert_eq!(session_id, "a");
            assert_eq!(binding_id, "window-a");
            assert_eq!(command, BrowserCommand::ReadText);
            bridge.handle(ClientRequest::BrowserResult {
                request_id,
                binding_id: "window-b".into(),
                response: BrowserResponse::Value {
                    value: json!("wrong page"),
                },
            });
            assert!(bridge.0.lock().unwrap().pending.contains_key(&request_id));
            bridge.handle(ClientRequest::BrowserResult {
                request_id,
                binding_id,
                response: BrowserResponse::Value {
                    value: json!("page A"),
                },
            });
        };
        let (response, _) = tokio::join!(request, desktop);
        assert_eq!(
            response,
            BrowserResponse::Value {
                value: json!("page A")
            }
        );
        assert!(bridge.0.lock().unwrap().pending.is_empty());
    }

    #[tokio::test]
    async fn local_socket_roundtrip_unbind_and_ambiguous_windows_fail_closed() {
        let home = std::env::temp_dir().join(format!("mb-cdp-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let bridge = BrowserBridge::default();
        let (out, mut rx) = mpsc::unbounded_channel();
        let guard = bridge.listen(&home, out).unwrap();
        bridge.handle(ClientRequest::BrowserBind {
            session_id: "a".into(),
            binding_id: "first".into(),
        });
        let path = guard.0.clone();
        assert_eq!(
            exchange(
                &path,
                &LocalRequest::Probe {
                    session_id: "a".into()
                }
            )
            .await
            .unwrap(),
            BrowserResponse::Value { value: json!(1) }
        );
        let command = LocalRequest::Execute {
            session_id: "a".into(),
            command: BrowserCommand::Screenshot,
        };
        let request = exchange(&path, &command);
        let desktop = async {
            let ServerEvent::BrowserCommand {
                request_id,
                binding_id,
                ..
            } = rx.recv().await.unwrap()
            else {
                panic!()
            };
            bridge.handle(ClientRequest::BrowserResult {
                request_id,
                binding_id,
                response: BrowserResponse::Value {
                    value: json!({"data":"png"}),
                },
            });
        };
        let (response, _) = tokio::join!(request, desktop);
        assert_eq!(
            response.unwrap(),
            BrowserResponse::Value {
                value: json!({"data":"png"})
            }
        );
        bridge.handle(ClientRequest::BrowserBind {
            session_id: "a".into(),
            binding_id: "second".into(),
        });
        assert!(matches!(
            exchange(
                &path,
                &LocalRequest::Execute {
                    session_id: "a".into(),
                    command: BrowserCommand::Tabs
                }
            )
            .await
            .unwrap(),
            BrowserResponse::Error { .. }
        ));
        bridge.handle(ClientRequest::BrowserUnbind {
            session_id: "a".into(),
            binding_id: "first".into(),
        });
        bridge.handle(ClientRequest::BrowserUnbind {
            session_id: "a".into(),
            binding_id: "second".into(),
        });
        assert_eq!(
            exchange(
                &path,
                &LocalRequest::Probe {
                    session_id: "a".into()
                }
            )
            .await
            .unwrap(),
            BrowserResponse::Value { value: json!(0) }
        );
        drop(guard);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn cli_parses_typed_commands_and_rejects_extra_arguments() {
        let parse = |s: &[&str]| parse_args(&s.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(
            parse(&[
                "--session",
                "a",
                "--daemon",
                "123",
                "type",
                "#input",
                "a\nb"
            ])
            .unwrap(),
            (
                "a".into(),
                Some(123),
                BrowserCommand::Type {
                    selector: "#input".into(),
                    text: "a\nb".into()
                }
            )
        );
        assert!(parse(&["--session", "a", "eval", "1", "extra"]).is_err());
        assert!(parse(&["--session", "a", "cdp", "Page.reload", "not-json"]).is_err());
    }
}
