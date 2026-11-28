use super::*;
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    io::Write,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

const MAX_WIRE_BYTES: u64 = 512 * 1024;
const MAX_REQUEST_BYTES: u64 = 32 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Request {
    List,
    Send {
        from: String,
        target: String,
        text: String,
        #[serde(default)]
        depth: Option<u8>,
    },
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
enum Reply {
    List {
        sessions: Vec<SessionSummary>,
        owned: HashSet<String>,
    },
    Sent {
        detail: String,
    },
    Error {
        message: String,
    },
}

pub(crate) struct SocketGuard(PathBuf);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn root(home: &Path) -> PathBuf {
    home.join(".multibot/sessions")
}

fn paths(home: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<_> = std::fs::read_dir(root(home))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "sock"))
        .take(128)
        .collect();
    paths.sort();
    paths
}

async fn rpc(path: &Path, request: &Request) -> Result<Reply> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut stream = UnixStream::connect(path).await?;
        let mut wire = serde_json::to_vec(request)?;
        wire.push(b'\n');
        ensure!(wire.len() as u64 <= MAX_REQUEST_BYTES, "Request too large");
        stream.write_all(&wire).await?;
        let mut line = Vec::new();
        BufReader::new(stream)
            .take(MAX_WIRE_BYTES + 1)
            .read_until(b'\n', &mut line)
            .await?;
        ensure!(line.len() as u64 <= MAX_WIRE_BYTES, "Response too large");
        Ok(serde_json::from_slice(&line)?)
    })
    .await
    .context("Session socket timed out")?
}

impl Sessions {
    pub(crate) fn listen(&self) -> Result<SocketGuard> {
        for directory in [self.home.join(".multibot"), root(&self.home)] {
            match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    ensure!(
                        std::fs::symlink_metadata(&directory)?.is_dir(),
                        "Session socket directory is redirected"
                    );
                }
                Err(e) => return Err(e.into()),
            }
        }
        std::fs::set_permissions(root(&self.home), std::fs::Permissions::from_mode(0o700))?;
        let path = root(&self.home).join(format!("{}.sock", std::process::id()));
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let router = self.clone();
        tokio::spawn(async move {
            let slots = Arc::new(tokio::sync::Semaphore::new(4));
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let Ok(slot) = slots.clone().try_acquire_owned() else {
                    drop(stream);
                    continue;
                };
                let router = router.clone();
                tokio::spawn(async move {
                    let _slot = slot;
                    let _ = tokio::time::timeout(Duration::from_secs(15), async {
                        let (reader, mut writer) = stream.into_split();
                        let mut line = Vec::new();
                        BufReader::new(reader)
                            .take(MAX_REQUEST_BYTES + 1)
                            .read_until(b'\n', &mut line)
                            .await?;
                        ensure!(line.len() as u64 <= MAX_REQUEST_BYTES, "Request too large");
                        let result: Result<Reply> = async {
                            match serde_json::from_slice::<Request>(&line)? {
                                Request::List => Ok(Reply::List {
                                    sessions: router.list().await?,
                                    owned: router.manager.connected_session_ids(),
                                }),
                                Request::Send {
                                    from,
                                    target,
                                    text,
                                    depth,
                                } => {
                                    let detail = if let Some(depth) = depth {
                                        let sessions = router.list().await?;
                                        router
                                            .deliver(
                                                &from,
                                                resolve(&sessions, &target)?,
                                                &text,
                                                depth,
                                            )
                                            .await?
                                    } else {
                                        router.send(&from, &target, &text).await?
                                    };
                                    Ok(Reply::Sent { detail })
                                }
                            }
                        }
                        .await;
                        let reply = result.unwrap_or_else(|e| Reply::Error {
                            message: e.to_string(),
                        });
                        let mut wire = serde_json::to_vec(&reply)?;
                        ensure!(wire.len() as u64 <= MAX_WIRE_BYTES, "Response too large");
                        wire.push(b'\n');
                        writer.write_all(&wire).await?;
                        Ok::<_, anyhow::Error>(())
                    })
                    .await;
                });
            }
        });
        Ok(SocketGuard(path))
    }

    pub(super) async fn forward_to_owner(
        &self,
        from: &str,
        target: &str,
        text: &str,
        depth: u8,
    ) -> Result<Option<String>> {
        if self.manager.connected_session_ids().contains(target) {
            return Ok(None);
        }
        let own = root(&self.home).join(format!("{}.sock", std::process::id()));
        for path in paths(&self.home).into_iter().filter(|p| p != &own) {
            if let Ok(Reply::List { owned, .. }) = rpc(&path, &Request::List).await
                && owned.contains(target)
            {
                return match rpc(
                    &path,
                    &Request::Send {
                        from: from.into(),
                        target: target.into(),
                        text: text.into(),
                        depth: Some(depth),
                    },
                )
                .await?
                {
                    Reply::Sent { detail } => Ok(Some(detail)),
                    Reply::Error { message } => bail!(message),
                    _ => bail!("Unexpected session response"),
                };
            }
        }
        Ok(None)
    }
}

pub(crate) fn cli(args: &[String]) -> Result<()> {
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "multibotd sessions list [--json]\nmultibotd sessions send <target-name-or-id> <message> [--from <sender>]"
        );
        return Ok(());
    }
    let home = dirs::home_dir().context("No home directory")?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let mut servers = Vec::new();
            let mut sessions = HashMap::new();
            for path in paths(&home) {
                if let Ok(Reply::List {
                    sessions: list,
                    owned,
                }) = rpc(&path, &Request::List).await
                {
                    for session in list {
                        if owned.contains(&session.id) {
                            sessions.insert(session.id.clone(), session);
                        } else {
                            sessions.entry(session.id.clone()).or_insert(session);
                        }
                    }
                    servers.push((path, owned));
                }
            }
            ensure!(
                !servers.is_empty(),
                "No running multibotd session socket; connect Multibot first"
            );
            let mut sessions: Vec<_> = sessions.into_values().collect();
            sessions.sort_by(|a, b| a.title.cmp(&b.title).then(a.id.cmp(&b.id)));
            match args[0].as_str() {
                "list" => {
                    ensure!(
                        args.len() == 1 || (args.len() == 2 && args[1] == "--json"),
                        "Usage: sessions list [--json]"
                    );
                    let stdout = std::io::stdout();
                    let mut out = stdout.lock();
                    if args.len() == 2 {
                        serde_json::to_writer(&mut out, &sessions)?;
                        writeln!(out)?;
                    } else {
                        for s in sessions {
                            writeln!(
                                out,
                                "{}\t{}\t{}\t{}\t{}",
                                s.title,
                                s.id,
                                s.agent.label(),
                                s.cwd.as_deref().unwrap_or(""),
                                if s.working { "working" } else { "idle" }
                            )?;
                        }
                    }
                }
                "send" => {
                    ensure!(
                        args.len() == 3 || (args.len() == 5 && args[3] == "--from"),
                        "Usage: sessions send <target> <message> [--from <sender>]"
                    );
                    let from = if args.len() == 5 {
                        args[4].clone()
                    } else {
                        std::env::var("MULTIBOT_SESSION_ID").unwrap_or_else(|_| "CLI".into())
                    };
                    let from = if sessions.iter().any(|s| s.id == from || s.title == from) {
                        resolve(&sessions, &from)?.id.clone()
                    } else {
                        from
                    };
                    let target = resolve(&sessions, &args[1])?;
                    // The sender's daemon owns the inherited depth, even when the target is elsewhere.
                    let server = servers
                        .iter()
                        .find(|(_, ids)| ids.contains(&from))
                        .or_else(|| servers.iter().find(|(_, ids)| ids.contains(&target.id)))
                        .unwrap_or(&servers[0]);
                    match rpc(
                        &server.0,
                        &Request::Send {
                            from,
                            target: target.id.clone(),
                            text: args[2].clone(),
                            depth: None,
                        },
                    )
                    .await?
                    {
                        Reply::Sent { detail } => println!("{detail}"),
                        Reply::Error { message } => bail!(message),
                        _ => bail!("Unexpected session response"),
                    }
                }
                _ => bail!("Unknown sessions command; use --help"),
            }
            Ok(())
        })
}
