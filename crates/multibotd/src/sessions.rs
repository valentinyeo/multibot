use crate::{process::ProcessManager, providers, watcher::DiskJob};
#[cfg(not(unix))]
use anyhow::bail;
use anyhow::{Result, ensure};
use multibot_proto::{AgentKind, SessionSummary};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const MAX_DEPTH: u8 = 4;
const MAX_TEXT_BYTES: usize = 16 * 1024;

#[derive(Clone)]
pub(crate) struct Sessions {
    home: PathBuf,
    manager: Arc<ProcessManager>,
    disk: std::sync::mpsc::Sender<DiskJob>,
    depths: Arc<Mutex<HashMap<String, (u8, Instant)>>>,
}

pub(crate) fn resolve<'a>(
    sessions: &'a [SessionSummary],
    target: &str,
) -> Result<&'a SessionSummary> {
    if let Some(session) = sessions.iter().find(|s| s.id == target) {
        return Ok(session);
    }
    let mut matches = sessions.iter().filter(|s| s.title == target);
    let session = matches
        .next()
        .ok_or_else(|| anyhow::anyhow!("No live session named {target}"))?;
    ensure!(
        matches.next().is_none(),
        "Ambiguous session name {target}; use its session ID"
    );
    Ok(session)
}

impl Sessions {
    pub(crate) fn new(
        home: PathBuf,
        manager: Arc<ProcessManager>,
        disk: std::sync::mpsc::Sender<DiskJob>,
    ) -> Self {
        Self {
            home,
            manager,
            disk,
            depths: Default::default(),
        }
    }

    pub(crate) async fn list(&self) -> Result<Vec<SessionSummary>> {
        let home = self.home.clone();
        let active = self.manager.active_session_ids();
        let connected = self.manager.connected_session_ids();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.disk
            .send(Box::new(move || {
                let metadata = providers::MetadataStore::load(&home);
                let archive = providers::ArchiveStore::load(&home);
                let live = providers::LiveActivity::load(&home);
                let workspaces =
                    crate::server_workspaces::ServerWorkspaceIndex::load_default_cached();
                let ctx = providers::DiscoveryContext {
                    active_session_ids: &active,
                    connected_session_ids: &connected,
                    metadata: &metadata,
                    archive: &archive,
                    live: &live,
                    include_archived: false,
                    server_workspaces: &workspaces,
                };
                let _ = tx.send(providers::discover_live_sessions_with_ctx(&home, 500, &ctx));
            }))
            .map_err(|_| anyhow::anyhow!("disk worker closed"))?;
        Ok(rx.await?)
    }

    fn next_depth(&self, from: &str) -> Result<u8> {
        let mut depths = self.depths.lock().unwrap();
        depths.retain(|_, (_, time)| time.elapsed() < Duration::from_secs(600));
        let depth = depths
            .get(from)
            .map_or(1, |(depth, _)| depth.saturating_add(1));
        ensure!(
            depth <= MAX_DEPTH,
            "Cross-session message depth limit ({MAX_DEPTH}) reached; ask the user to start a new turn"
        );
        Ok(depth)
    }

    pub(crate) fn user_turn(&self, id: &str) {
        self.depths.lock().unwrap().remove(id);
    }

    pub(crate) async fn send(&self, from: &str, target: &str, text: &str) -> Result<String> {
        let sessions = self.list().await?;
        let from = if sessions.iter().any(|s| s.id == from || s.title == from) {
            resolve(&sessions, from)?.id.as_str()
        } else {
            from
        };
        let depth = self.next_depth(from)?;
        let target = resolve(&sessions, target)?;
        ensure!(
            from != target.id,
            "Cannot message the sending session itself"
        );
        ensure!(
            !text.trim().is_empty() && text.len() <= MAX_TEXT_BYTES,
            "Message must contain 1..={MAX_TEXT_BYTES} bytes"
        );
        #[cfg(unix)]
        if let Some(reply) = self.forward_to_owner(from, &target.id, text, depth).await? {
            return Ok(reply);
        }
        self.deliver(from, target, text, depth).await
    }

    async fn deliver(
        &self,
        from: &str,
        target: &SessionSummary,
        text: &str,
        depth: u8,
    ) -> Result<String> {
        ensure!(
            (1..=MAX_DEPTH).contains(&depth),
            "Cross-session message depth limit reached"
        );
        ensure!(
            !text.trim().is_empty() && text.len() <= MAX_TEXT_BYTES,
            "Invalid message length"
        );
        ensure!(
            from != target.id && !from.chars().any(char::is_control) && from.len() <= 128,
            "Invalid sender"
        );
        // Never evict a live chain to make room: that would reset its loop guard.
        let depth = {
            let mut depths = self.depths.lock().unwrap();
            depths.retain(|_, (_, time)| time.elapsed() < Duration::from_secs(600));
            ensure!(
                depths.len() < 512 || depths.contains_key(&target.id),
                "Too many cross-session conversations"
            );
            let depth = depth.max(
                depths
                    .get(&target.id)
                    .map_or(1, |(previous, _)| previous.saturating_add(1)),
            );
            ensure!(
                depth <= MAX_DEPTH,
                "Cross-session message depth limit reached; send a new user prompt to reset it"
            );
            depths.insert(target.id.clone(), (depth, Instant::now()));
            depth
        };
        let sender = if from.is_empty() { "CLI" } else { from };
        let envelope = format!(
            "[Message from {sender}]: {text}\n[Multibot cross-session depth {depth}/{MAX_DEPTH}; do not auto-reply without useful new information.]"
        );
        if target.agent == AgentKind::Shell {
            #[cfg(unix)]
            {
                let status = tokio::process::Command::new("tmux")
                    .args([
                        "send-keys",
                        "-t",
                        &format!("={}:", target.id),
                        "-l",
                        "--",
                        &envelope,
                    ])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .kill_on_drop(true)
                    .status();
                let status = tokio::time::timeout(Duration::from_secs(5), status).await??;
                ensure!(status.success(), "tmux could not deliver message");
                let status = tokio::process::Command::new("tmux")
                    .args(["send-keys", "-t", &format!("={}:", target.id), "Enter"])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .kill_on_drop(true)
                    .status();
                let status = tokio::time::timeout(Duration::from_secs(5), status).await??;
                ensure!(status.success(), "tmux could not submit message");
                return Ok(format!("Delivered to {}", target.id));
            }
            #[cfg(not(unix))]
            bail!("Shell messaging requires a Unix host");
        }
        self.manager
            .send_prompt(
                &target.id,
                target.agent,
                target.cwd.as_deref(),
                &envelope,
                None,
            )
            .await
    }
}

#[cfg(unix)]
mod socket;
#[cfg(unix)]
pub(crate) use socket::cli;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names_must_be_unique_but_ids_win() {
        let sessions = vec![
            SessionSummary {
                id: "a".into(),
                title: "Same".into(),
                ..Default::default()
            },
            SessionSummary {
                id: "b".into(),
                title: "Same".into(),
                ..Default::default()
            },
        ];
        assert_eq!(resolve(&sessions, "b").unwrap().id, "b");
        assert!(
            resolve(&sessions, "Same")
                .unwrap_err()
                .to_string()
                .contains("Ambiguous")
        );
        assert!(resolve(&sessions, "missing").is_err());
        assert_eq!(resolve(&sessions[..1], "Same").unwrap().id, "a");
    }
    #[test]
    fn depth_guard_stops_fresh_text_ping_pong_and_user_turn_resets_it() {
        let (out, _) = tokio::sync::mpsc::unbounded_channel();
        let (disk, _) = std::sync::mpsc::channel();
        let sessions = Sessions::new(
            PathBuf::new(),
            ProcessManager::new(PathBuf::new(), out),
            disk,
        );
        let mut from = "a";
        let mut target = "b";
        for expected in 1..=MAX_DEPTH {
            let depth = sessions.next_depth(from).unwrap();
            assert_eq!(depth, expected);
            sessions
                .depths
                .lock()
                .unwrap()
                .insert(target.into(), (depth, Instant::now()));
            std::mem::swap(&mut from, &mut target);
        }
        assert!(sessions.next_depth(from).is_err());
        sessions.user_turn(from);
        assert_eq!(sessions.next_depth(from).unwrap(), 1);
    }
}
