//! Tmux lifecycle events plus a low-frequency process check for quiet background workers.
use crate::process::ProcessManager;
use crate::providers::{
    ArchiveStore, DiscoveryContext, LiveActivity, MetadataStore, discover_live_sessions_with_ctx,
};
use crate::watcher::DiskJob;
use multibot_proto::ServerEvent;
use notify::{RecursiveMode, Watcher};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

fn socket_dir() -> PathBuf {
    if let Ok(tmux) = std::env::var("TMUX")
        && let Some(socket) = tmux.split(',').next()
        && let Some(parent) = std::path::Path::new(socket).parent()
    {
        return parent.to_path_buf();
    }
    #[cfg(unix)]
    let uid = {
        unsafe extern "C" {
            fn getuid() -> u32;
        }
        unsafe { getuid() }
    };
    #[cfg(not(unix))]
    let uid = 0;
    let base = std::env::var_os("TMUX_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join(format!("tmux-{uid}"))
}

pub fn start(
    home: PathBuf,
    out: mpsc::UnboundedSender<ServerEvent>,
    process: Arc<ProcessManager>,
    disk: std::sync::mpsc::Sender<DiskJob>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let dir = socket_dir();
        let (wake, mut wakes) = mpsc::unbounded_channel();
        let watch_dir = dir.clone();
        let lease_dir = home.join(".local/state/fleet/leases");
        let watch_leases = lease_dir.clone();
        let Ok(mut watcher) =
            notify::recommended_watcher(move |ev: notify::Result<notify::Event>| {
                if let Ok(ev) = ev
                    && !matches!(ev.kind, notify::EventKind::Access(_))
                    && ev.paths.iter().any(|p| {
                        p == &watch_dir
                            || p.parent() == Some(watch_dir.as_path())
                            || p.starts_with(&watch_leases)
                            || watch_leases.starts_with(p)
                    })
                {
                    let _ = wake.send(());
                }
            })
        else {
            return;
        };
        if let Some(parent) = dir.parent() {
            let _ = watcher.watch(parent, RecursiveMode::NonRecursive);
        }
        if let Some(parent) = lease_dir.ancestors().skip(1).find(|p| p.is_dir()) {
            let _ = watcher.watch(parent, RecursiveMode::NonRecursive);
        }
        let mut watched_leases = false;
        let mut watched_dir = false;
        let mut previous = Vec::new();
        let mut attach_retries = 0;
        let mut worker_tick = tokio::time::interval(std::time::Duration::from_secs(3));
        worker_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            // No output subscription, terminal size influence, or mutations to legacy hooks.
            let mut command = tokio::process::Command::new("tmux");
            if let Ok(tmux) = std::env::var("TMUX")
                && let Some(socket) = tmux.split(',').next().filter(|s| !s.is_empty())
            {
                command.args(["-S", socket]);
            }
            let child = command
                .env_remove("TMUX")
                .args(["-C", "attach-session", "-f", "no-output,ignore-size"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn();
            let mut child = match child {
                Ok(child) => Some(child),
                Err(_) => None,
            };
            let mut lines = child
                .as_mut()
                .and_then(|c| c.stdout.take())
                .map(|out| BufReader::new(out).lines());
            let mut refresh = true;
            loop {
                if !watched_dir && dir.is_dir() {
                    watched_dir = watcher.watch(&dir, RecursiveMode::NonRecursive).is_ok();
                }
                if !watched_leases && lease_dir.is_dir() {
                    watched_leases = watcher
                        .watch(&lease_dir, RecursiveMode::NonRecursive)
                        .is_ok();
                }
                if refresh {
                    let home = home.clone();
                    let out = out.clone();
                    let process = Arc::clone(&process);
                    let old = previous.clone();
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    if disk
                        .send(Box::new(move || {
                            let metadata = MetadataStore::load(&home);
                            let archive = ArchiveStore::load(&home);
                            let live = LiveActivity::cached(&home);
                            let active = process.active_session_ids();
                            let connected = process.connected_session_ids();
                            let index =
                                crate::server_workspaces::ServerWorkspaceIndex::load_default_cached(
                                );
                            let ctx = DiscoveryContext {
                                active_session_ids: &active,
                                active_session_pids: &process.active_session_pids(),
                                connected_session_ids: &connected,
                                metadata: &metadata,
                                archive: &archive,
                                live: &live,
                                include_archived: false,
                                server_workspaces: &index,
                            };
                            let sessions = discover_live_sessions_with_ctx(&home, 500, &ctx);
                            if sessions != old {
                                let _ = out.send(ServerEvent::Sessions {
                                    sessions: sessions.clone(),
                                });
                            }
                            let _ = tx.send(sessions);
                        }))
                        .is_err()
                    {
                        return;
                    }
                    let Ok(sessions) = rx.await else { return };
                    previous = sessions;
                    // Live shells need a control client even after the startup grace.
                    // Empty servers return to event-only waiting after bounded retries.
                    if lines.is_none()
                        && (previous
                            .iter()
                            .any(|s| s.agent == multibot_proto::AgentKind::Shell)
                            || (dir.is_dir() && attach_retries < 5))
                    {
                        tokio::time::sleep(std::time::Duration::from_millis(25 << attach_retries))
                            .await;
                        attach_retries = (attach_retries + 1).min(5);
                        break;
                    }
                }
                tokio::select! {
                    _ = worker_tick.tick(), if !previous.is_empty() => {
                        let home = home.clone();
                        let process = Arc::clone(&process);
                        let old = previous.clone();
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        if disk.send(Box::new(move || {
                            let mut sessions = old.clone();
                            crate::subworkers::populate(
                                &mut sessions,
                                &MetadataStore::load(&home),
                                &process.active_session_pids(),
                            );
                            let changed = sessions.iter().zip(&old)
                                .any(|(new, old)| new.subworkers != old.subworkers);
                            let _ = tx.send(changed);
                        })).is_err() {
                            return;
                        }
                        // Rediscover only on a worker change so working state is recomputed
                        // from the transcript when the last background worker exits.
                        let Ok(changed) = rx.await else { return };
                        refresh = changed;
                    }
                    wake = wakes.recv() => {
                        if wake.is_none() { return; }
                        while wakes.try_recv().is_ok() {}
                        attach_retries = 0;
                        if lines.is_none() { break; }
                        refresh = true;
                    }
                    line = async { lines.as_mut().unwrap().next_line().await }, if lines.is_some() => {
                        match line {
                            Ok(Some(line)) => {
                                if line.starts_with("%session-changed ") { attach_retries = 0; }
                                refresh = line.starts_with("%session-changed ") || line.starts_with("%sessions-changed") || line.starts_with("%session-renamed")
                                    || line.starts_with("%window-renamed ") || line.starts_with("%unlinked-window-renamed ")
                                    || line.starts_with("%window-pane-changed") || line.starts_with("%session-window-changed");
                            }
                            _ => { lines = None; drop(child.take()); refresh = true; }
                        }
                    }
                }
            }
        }
    })
}
