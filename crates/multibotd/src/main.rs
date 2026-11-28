#[cfg(unix)]
mod browser_bridge;
mod commands;
mod continuity;
mod process;
mod providers;
mod search;
mod server_workspaces;
mod sessions;
mod shell_watch;
mod watcher;

use anyhow::Result;
use multibot_proto::{ClientRequest, PROTOCOL_VERSION, ServerEvent, build_version, decode_line};
use process::ProcessManager;
use providers::{
    ArchiveStore, DiscoveryContext, LiveActivity, MetadataStore, discover_live_sessions_with_ctx,
    now_ms,
};
use search::{SearchError, search_history};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use watcher::{DiskJob, WatchManager};

fn main() -> Result<()> {
    #[cfg(unix)]
    {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if args.first().is_some_and(|arg| arg == "sessions") {
            return sessions::cli(&args[1..]);
        }
        if args.first().is_some_and(|arg| arg == "browser") {
            return browser_bridge::cli(&args[1..]);
        }
    }
    if std::env::args()
        .skip(1)
        .any(|a| a == "--version" || a == "-V")
    {
        println!("multibotd {}", build_version());
        return Ok(());
    }

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ServerEvent>();
    let stdout_handle = std::thread::Builder::new()
        .name("stdout-writer".into())
        .spawn(move || {
            let stdout = std::io::stdout();
            let mut writer = BufWriter::with_capacity(16 * 1024, stdout.lock());
            while let Some(ev) = out_rx.blocking_recv() {
                if serde_json::to_writer(&mut writer, &ev).is_err() {
                    break;
                }
                if writer.write_all(b"\n").is_err() {
                    break;
                }
                if writer.flush().is_err() {
                    break;
                }
            }
        })?;

    let (disk_tx, disk_rx) = std::sync::mpsc::channel::<DiskJob>();
    let disk_handle = std::thread::Builder::new()
        .name("disk-worker".into())
        .spawn(move || {
            while let Ok(job) = disk_rx.recv() {
                job();
            }
        })?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let res = rt.block_on(async_main(out_tx, disk_tx));
    // Dropping `rt` cancels any detached background tasks so every clone of `disk_tx` and
    // `out_tx` is released; both worker threads then drain all pending responses and exit cleanly.
    drop(rt);
    let _ = disk_handle.join();
    let _ = stdout_handle.join();
    res
}

async fn async_main(
    out_tx: mpsc::UnboundedSender<ServerEvent>,
    disk_tx: std::sync::mpsc::Sender<DiskJob>,
) -> Result<()> {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    #[cfg(unix)]
    let browser_bridge = browser_bridge::BrowserBridge::default();
    #[cfg(unix)]
    let _browser_socket = browser_bridge.listen(&home, out_tx.clone())?;
    let binary_hash = compute_binary_hash();
    let hostname = read_hostname();

    let (in_tx, mut in_rx) = mpsc::unbounded_channel::<String>();
    let stdin_handle = std::thread::Builder::new()
        .name("stdin-reader".into())
        .spawn(move || {
            let stdin = std::io::stdin();
            let mut reader = BufReader::with_capacity(8 * 1024, stdin.lock());
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        if in_tx.send(std::mem::take(&mut line)).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        })?;

    let process_mgr = ProcessManager::new(home.clone(), out_tx.clone());
    let sessions = sessions::Sessions::new(home.clone(), Arc::clone(&process_mgr), disk_tx.clone());
    #[cfg(unix)]
    let _session_socket = sessions.listen()?;
    let mut watch_mgr = WatchManager::with_disk_worker(
        home.clone(),
        out_tx.clone(),
        Arc::clone(&process_mgr),
        disk_tx.clone(),
    );

    while let Some(line) = in_rx.recv().await {
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
            request @ (ClientRequest::BrowserBind { .. }
            | ClientRequest::BrowserUnbind { .. }
            | ClientRequest::BrowserResult { .. }) => {
                #[cfg(unix)]
                browser_bridge.handle(request);
                #[cfg(not(unix))]
                let _ = request;
            }
            ClientRequest::Hello { .. } => {
                let _ = out_tx.send(ServerEvent::HelloAck {
                    protocol_version: PROTOCOL_VERSION,
                    daemon_version: build_version().to_string(),
                    hostname: hostname.clone(),
                    pid: std::process::id(),
                    binary_hash: binary_hash.clone(),
                });
            }
            ClientRequest::Ping { nonce } => {
                let _ = out_tx.send(ServerEvent::Pong {
                    nonce,
                    server_time_ms: now_ms(),
                });
            }
            ClientRequest::ListSessions { limit, .. }
            | ClientRequest::ListLifecycleSessions { limit } => {
                let (include_archived, lifecycle_only) = match req {
                    ClientRequest::ListSessions {
                        include_archived, ..
                    } => (include_archived.unwrap_or(false), false),
                    _ => (true, true),
                };
                let home_c = home.clone();
                let active_ids = process_mgr.active_session_ids();
                let connected_ids = process_mgr.connected_session_ids();
                let tx = out_tx.clone();
                let lim = limit.unwrap_or(50);
                let _ = disk_tx.send(Box::new(move || {
                    let metadata = MetadataStore::load(&home_c);
                    let archive = ArchiveStore::load(&home_c);
                    let live = LiveActivity::load(&home_c);
                    let server_workspaces =
                        crate::server_workspaces::ServerWorkspaceIndex::load_default_cached();
                    let ctx = DiscoveryContext {
                        active_session_ids: &active_ids,
                        connected_session_ids: &connected_ids,
                        metadata: &metadata,
                        archive: &archive,
                        live: &live,
                        include_archived,
                        server_workspaces: &server_workspaces,
                    };
                    let sessions = if lifecycle_only {
                        providers::discover_lifecycle_sessions_with_ctx(&home_c, lim, &ctx)
                    } else {
                        discover_live_sessions_with_ctx(&home_c, lim, &ctx)
                    };
                    let _ = tx.send(ServerEvent::Sessions { sessions });
                }));
            }
            ClientRequest::ReadHistory {
                session_id,
                source_path,
                agent,
                tail,
                before_offset,
                max_bytes,
                known_probe,
            } => {
                if agent == multibot_proto::AgentKind::Shell {
                    let _ = out_tx.send(providers::tmux::empty_history(session_id));
                    continue;
                }
                let tx = out_tx.clone();
                let history_home = home.clone();
                let _ = disk_tx.send(Box::new(move || {
                    let path = PathBuf::from(&source_path);
                    match continuity::read_page(
                        &history_home,
                        &session_id,
                        agent,
                        &path,
                        before_offset,
                        max_bytes,
                        tail,
                        known_probe.as_deref(),
                    ) {
                        Ok(page) => {
                            let _ = tx.send(ServerEvent::History {
                                session_id,
                                messages: page.messages,
                                oldest_offset: Some(page.oldest_offset),
                                latest_offset: Some(page.latest_offset),
                                has_more: page.has_more,
                                file_size: Some(page.file_size),
                                file_mtime_ms: Some(page.file_mtime_ms),
                                probe_sha256: Some(page.probe_sha256),
                                todos: page.todos,
                            });
                        }
                        Err(err) => {
                            let _ = tx.send(ServerEvent::Error {
                                code: "read_history_failed".into(),
                                message: format!("Failed to read {source_path}: {err}"),
                            });
                        }
                    }
                }));
            }
            ClientRequest::WatchSession {
                session_id,
                source_path,
                agent,
            } => {
                let path = PathBuf::from(&source_path);
                if let Err(err) = watch_mgr.watch_session(session_id, path, agent).await {
                    let _ = out_tx.send(ServerEvent::Error {
                        code: "watch_failed".into(),
                        message: format!("Failed to watch {source_path}: {err}"),
                    });
                }
            }
            ClientRequest::UnwatchSession { session_id } => {
                watch_mgr.unwatch_session(session_id.as_deref());
            }
            ClientRequest::WatchDirectory => {
                watch_mgr.watch_directory();
            }
            ClientRequest::UnwatchDirectory => {
                watch_mgr.unwatch_directory();
            }
            ClientRequest::SendPrompt {
                session_id,
                agent,
                cwd,
                text,
                options,
            } => {
                sessions.user_turn(&session_id);
                match process_mgr
                    .send_prompt(&session_id, agent, cwd.as_deref(), &text, options)
                    .await
                {
                    Ok(detail) => {
                        let _ = out_tx.send(ServerEvent::SendAccepted { session_id, detail });
                    }
                    Err(err) => {
                        let _ = out_tx.send(ServerEvent::Error {
                            code: "send_failed".into(),
                            message: format!("Failed to send prompt: {err}"),
                        });
                    }
                }
            }
            ClientRequest::SwitchShellClient {
                client_id,
                session_id,
            } => {
                process_mgr.switch_shell_client(client_id, session_id).await;
            }
            ClientRequest::SendCrossSession {
                from_session_id,
                target_session_id,
                text,
            } => {
                match sessions
                    .send(&from_session_id, &target_session_id, &text)
                    .await
                {
                    Ok(detail) => {
                        let _ = out_tx.send(ServerEvent::SendAccepted {
                            session_id: target_session_id,
                            detail,
                        });
                    }
                    Err(err) => {
                        let _ = out_tx.send(ServerEvent::Error {
                            code: "cross_session_failed".into(),
                            message: err.to_string(),
                        });
                    }
                }
            }
            ClientRequest::InterruptSession { session_id, agent } => {
                process_mgr.interrupt_session(session_id, agent).await;
            }
            ClientRequest::SteerSession {
                session_id,
                agent,
                cwd,
                text,
                options,
            } => {
                sessions.user_turn(&session_id);
                process_mgr
                    .steer_session(session_id, agent, cwd, text, options)
                    .await;
            }
            ClientRequest::CancelQueuedPrompt {
                session_id,
                message_id,
            } => {
                process_mgr.cancel_queued_prompt(session_id, message_id);
            }
            ClientRequest::ListHarnesses => {
                let _ = out_tx.send(ServerEvent::Harnesses {
                    choices: continuity::harnesses(&home),
                });
            }
            ClientRequest::SwitchHarness {
                session,
                target,
                context,
            } => {
                if let Err(err) = process_mgr.switch_harness(session, target, context) {
                    let _ = out_tx.send(ServerEvent::Error {
                        code: "switch_harness_failed".into(),
                        message: err.to_string(),
                    });
                }
            }
            ClientRequest::SpawnSession {
                agent,
                cwd,
                title,
                initial_prompt,
                options,
            } => {
                process_mgr
                    .spawn_session(agent, cwd, title, initial_prompt, options)
                    .await;
            }
            ClientRequest::RenameSession {
                session_id,
                agent,
                source_path,
                title,
            } => {
                process_mgr.rename_session(session_id, agent, source_path, title);
            }
            ClientRequest::ArchiveSession {
                session_id,
                agent,
                source_path,
                kill_process,
            } => {
                process_mgr
                    .archive_session(session_id, agent, source_path, kill_process)
                    .await;
            }
            ClientRequest::UnarchiveSession { session_id } => {
                process_mgr.unarchive_session(session_id);
            }
            ClientRequest::SearchHistory {
                request_id,
                session_id,
                source_path,
                agent,
                query,
            } => {
                let tx = out_tx.clone();
                let _ = disk_tx.send(Box::new(move || {
                    let path = PathBuf::from(&source_path);
                    match search_history(request_id, session_id, &path, agent, &query) {
                        Ok(ev) => {
                            let _ = tx.send(ev);
                        }
                        Err(SearchError::InvalidQuery(msg)) => {
                            let _ = tx.send(ServerEvent::Error {
                                code: "invalid_query".into(),
                                message: msg,
                            });
                        }
                        Err(SearchError::Io(err)) => {
                            let _ = tx.send(ServerEvent::Error {
                                code: "search_failed".into(),
                                message: format!("Failed to search {source_path}: {err}"),
                            });
                        }
                    }
                }));
            }
            ClientRequest::ListCommands { agent, cwd } => {
                let home_c = home.clone();
                let tx = out_tx.clone();
                let _ = disk_tx.send(Box::new(move || {
                    let cwd_path = cwd.as_deref().map(PathBuf::from);
                    let items = commands::list_commands(&home_c, agent, cwd_path.as_deref());
                    let _ = tx.send(ServerEvent::Commands { agent, cwd, items });
                }));
            }
        }
    }

    // Clean shutdown: stop and await all watchers, drop senders, and join stdin reader.
    watch_mgr.shutdown().await;
    drop(watch_mgr);
    drop(process_mgr);
    drop(disk_tx);
    drop(out_tx);
    let _ = stdin_handle.join();
    Ok(())
}

/// Computes the 12-char lowercase hex SHA-256 prefix of the running daemon binary
/// (matching `multibot_core::daemon_deploy::sha256_prefix_12`), streaming in 64 KiB chunks.
fn compute_binary_hash() -> String {
    if let Some(baked) = option_env!("MULTIBOT_BINARY_HASH")
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return baked.chars().take(12).collect();
    }
    let Ok(exe_path) = std::env::current_exe() else {
        return String::new();
    };
    let Ok(mut file) = std::fs::File::open(exe_path) else {
        return String::new();
    };
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(_) => return String::new(),
        }
    }
    let digest = hasher.finalize();
    format!(
        "{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3], digest[4], digest[5]
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_binary_hash_returns_12_lowercase_hex_chars() {
        let h = compute_binary_hash();
        assert_eq!(h.len(), 12);
        assert!(h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
    }
}
