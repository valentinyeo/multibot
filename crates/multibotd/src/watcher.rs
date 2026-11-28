use crate::process::ProcessManager;
use crate::providers::{
    ArchiveStore, DEFAULT_HISTORY_MAX_BYTES, DeltaRead, DiscoveryContext, LiveActivity,
    MetadataStore, compute_semantic_working, now_ms, read_history_delta,
    summarize_session_by_agent, system_time_ms,
};
use anyhow::{Result, anyhow};
use multibot_proto::{AgentKind, ServerEvent};
use notify::event::{AccessKind, AccessMode, ModifyKind};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use walkdir::WalkDir;

pub const COALESCE_WINDOW: Duration = Duration::from_millis(5);
pub const DIR_COALESCE_WINDOW: Duration = Duration::from_millis(8);
const MAX_DELTA_CHUNKS_PER_WAKEUP: usize = 64;

pub type DiskJob = Box<dyn FnOnce() + Send + 'static>;

struct ActiveSessionWatch {
    stop_tx: mpsc::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

enum DirEvent {
    FsPath(PathBuf),
    Rescan,
}

struct ActiveDirWatch {
    rescan_tx: mpsc::UnboundedSender<DirEvent>,
    stop_tx: mpsc::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

pub struct WatchManager {
    home: PathBuf,
    out_tx: mpsc::UnboundedSender<ServerEvent>,
    process_mgr: Arc<ProcessManager>,
    disk_tx: Option<std::sync::mpsc::Sender<DiskJob>>,
    sessions: HashMap<String, ActiveSessionWatch>,
    dir_watch: Option<ActiveDirWatch>,
    shell_task: Option<tokio::task::JoinHandle<()>>,
}

async fn run_on_disk<T, F>(disk_tx: Option<&std::sync::mpsc::Sender<DiskJob>>, f: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    match disk_tx {
        Some(tx) => {
            let (res_tx, res_rx) = tokio::sync::oneshot::channel();
            let job: DiskJob = Box::new(move || {
                let _ = res_tx.send(f());
            });
            if tx.send(job).is_err() {
                return None;
            }
            res_rx.await.ok()
        }
        None => Some(f()),
    }
}

impl WatchManager {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(
        home: PathBuf,
        out_tx: mpsc::UnboundedSender<ServerEvent>,
        process_mgr: Arc<ProcessManager>,
    ) -> Self {
        Self {
            home,
            out_tx,
            process_mgr,
            disk_tx: None,
            sessions: HashMap::new(),
            dir_watch: None,
            shell_task: None,
        }
    }

    pub fn with_disk_worker(
        home: PathBuf,
        out_tx: mpsc::UnboundedSender<ServerEvent>,
        process_mgr: Arc<ProcessManager>,
        disk_tx: std::sync::mpsc::Sender<DiskJob>,
    ) -> Self {
        Self {
            home,
            out_tx,
            process_mgr,
            disk_tx: Some(disk_tx),
            sessions: HashMap::new(),
            dir_watch: None,
            shell_task: None,
        }
    }

    pub async fn watch_session(
        &mut self,
        session_id: String,
        source_path: PathBuf,
        agent: AgentKind,
    ) -> Result<()> {
        if agent == AgentKind::Shell {
            let _ = self
                .out_tx
                .send(crate::providers::tmux::empty_history(session_id));
            return Ok(());
        }

        // Idempotent re-watch for the same session_id without affecting other sessions.
        if let Some(prev) = self.sessions.remove(&session_id) {
            let _ = prev.stop_tx.try_send(());
        }

        let requested_path = source_path;
        let source_path = if agent == AgentKind::Antigravity {
            crate::providers::antigravity::resolve_transcript(
                &self.home,
                &session_id,
                &requested_path,
            )
        } else {
            requested_path.clone()
        };
        let targets = session_watch_targets(&self.home, &session_id, &requested_path, agent);
        let watched_targets = Arc::new(std::sync::Mutex::new(targets.clone()));
        let callback_targets = Arc::clone(&watched_targets);
        let (fs_tx, mut fs_rx) = mpsc::unbounded_channel::<()>();
        let fs_tx_requeue = fs_tx.clone();
        let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            let Ok(event) = res else {
                return;
            };
            if is_ignored_read_event(&event.kind) {
                return;
            }
            let matches_name = event.paths.iter().any(|p| {
                callback_targets
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|target| target.starts_with(p))
            }) || (matches!(event.kind, EventKind::Modify(ModifyKind::Name(_)))
                && event.paths.is_empty());
            if matches_name {
                let _ = fs_tx.send(());
            }
        })?;
        let mut watch_dirs = HashSet::new();
        sync_session_watches(&mut watcher, &targets, &mut watch_dirs)?;

        let history_home = self.home.clone();
        let path_for_read = source_path.clone();
        let read_id = session_id.clone();
        let page = run_on_disk(self.disk_tx.as_ref(), move || {
            crate::continuity::read_page(
                &history_home,
                &read_id,
                agent,
                &path_for_read,
                None,
                Some(DEFAULT_HISTORY_MAX_BYTES),
                Some(200),
                None,
            )
        })
        .await
        .ok_or_else(|| anyhow!("disk worker closed"))??;
        let initial_offset = page
            .latest_offset
            .saturating_sub(crate::continuity::base(&self.home, &session_id));
        let initial_probe = page.probe_sha256.clone();
        let _ = self.out_tx.send(ServerEvent::History {
            session_id: session_id.clone(),
            messages: page.messages,
            oldest_offset: Some(page.oldest_offset),
            latest_offset: Some(page.latest_offset),
            has_more: page.has_more,
            file_size: Some(page.file_size),
            file_mtime_ms: Some(page.file_mtime_ms),
            probe_sha256: Some(page.probe_sha256),
            todos: page.todos,
        });

        let home = self.home.clone();
        let out_tx = self.out_tx.clone();
        let process_mgr = Arc::clone(&self.process_mgr);
        let disk_tx = self.disk_tx.clone();
        let sid_task = session_id.clone();

        let task = tokio::spawn(async move {
            let mut watcher = watcher;
            let mut source_path = source_path;
            let mut consumed_offset = initial_offset;
            let mut last_probe = initial_probe;

            loop {
                tokio::select! {
                    _ = stop_rx.recv() => break,
                    maybe = fs_rx.recv() => {
                        if maybe.is_none() {
                            break;
                        }
                        while fs_rx.try_recv().is_ok() {}
                        let coalescer = tokio::time::sleep(COALESCE_WINDOW);
                        tokio::pin!(coalescer);
                        loop {
                            tokio::select! {
                                _ = stop_rx.recv() => return,
                                _ = &mut coalescer => break,
                                more = fs_rx.recv() => {
                                    if more.is_none() {
                                        return;
                                    }
                                }
                            }
                        }
                        while fs_rx.try_recv().is_ok() {}

                        let targets = session_watch_targets(&home, &sid_task, &requested_path, agent);
                        *watched_targets.lock().unwrap() = targets.clone();
                        if let Err(err) = sync_session_watches(&mut watcher, &targets, &mut watch_dirs) {
                            let _ = out_tx.send(ServerEvent::Error { code: "watch_failed".into(), message: err.to_string() });
                            return;
                        }
                        let next_source = if agent == AgentKind::Antigravity {
                            crate::providers::antigravity::resolve_transcript(&home, &sid_task, &requested_path)
                        } else { requested_path.clone() };
                        if next_source != source_path {
                            source_path = next_source;
                            // Offsets from the mirror have no meaning in the native file.
                            consumed_offset = u64::MAX;
                            last_probe.clear();
                        }

                        let home_c = home.clone();
                        let sid_c = sid_task.clone();
                        let path_c = source_path.clone();
                        let pm_c = Arc::clone(&process_mgr);
                        let out_c = out_tx.clone();
                        let mut off = consumed_offset;
                        let mut probe = last_probe.clone();

                        let Some((new_off, new_probe, needs_more)) =
                            run_on_disk(disk_tx.as_ref(), move || {
                                let needs_more = process_file_update(
                                    &home_c,
                                    &sid_c,
                                    &path_c,
                                    agent,
                                    &mut off,
                                    &mut probe,
                                    &pm_c,
                                    &out_c,
                                );
                                (off, probe, needs_more)
                            })
                            .await
                        else {
                            break;
                        };
                        consumed_offset = new_off;
                        last_probe = new_probe;
                        if needs_more {
                            let _ = fs_tx_requeue.send(());
                        }
                    }
                }
            }
        });

        self.sessions
            .insert(session_id, ActiveSessionWatch { stop_tx, task });
        Ok(())
    }

    pub fn unwatch_session(&mut self, session_id: Option<&str>) {
        match session_id {
            None => {
                for (_, w) in self.sessions.drain() {
                    let _ = w.stop_tx.try_send(());
                }
            }
            Some(id) => {
                if let Some(w) = self.sessions.remove(id) {
                    let _ = w.stop_tx.try_send(());
                }
            }
        }
    }

    pub fn watch_directory(&mut self) {
        if self.shell_task.is_none() && self.disk_tx.is_some() {
            self.shell_task = Some(crate::shell_watch::start(
                self.home.clone(),
                self.out_tx.clone(),
                Arc::clone(&self.process_mgr),
                self.disk_tx.as_ref().unwrap().clone(),
            ));
        }

        if let Some(ref w) = self.dir_watch {
            let _ = w.rescan_tx.send(DirEvent::Rescan);
            return;
        }
        let desired_roots = desired_provider_watch_roots(&self.home);
        if desired_roots.is_empty() {
            return;
        }

        let (dir_tx, mut dir_rx) = mpsc::unbounded_channel::<DirEvent>();
        let rescan_tx = dir_tx.clone();
        let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);

        let callback_roots: Vec<PathBuf> = desired_roots.iter().map(|(r, _)| r.clone()).collect();
        let Ok(mut watcher) = notify::recommended_watcher(move |res: notify::Result<Event>| {
            let Ok(event) = res else {
                return;
            };
            if is_ignored_read_event(&event.kind) {
                return;
            }
            for p in event.paths {
                let is_jsonl = p.extension().and_then(|e| e.to_str()) == Some("jsonl");
                let relevant_dir = callback_roots
                    .iter()
                    .any(|r| r.starts_with(&p) || p.starts_with(r));
                if is_jsonl || relevant_dir {
                    let _ = dir_tx.send(DirEvent::FsPath(p));
                }
            }
        }) else {
            return;
        };

        let mut watched_roots: Vec<(PathBuf, AgentKind)> = Vec::new();
        let mut watched_root_set: HashSet<PathBuf> = HashSet::new();
        let mut watched_ancestors: HashSet<PathBuf> = HashSet::new();

        // Synchronously attach watches for existing roots + nearest existing ancestors
        // of not-yet-created roots BEFORE returning, so immediate post-call creates are caught.
        let _ = sync_dir_watches(
            &mut watcher,
            &desired_roots,
            &mut watched_roots,
            &mut watched_root_set,
            &mut watched_ancestors,
            false,
        );

        let home = self.home.clone();
        let out_tx = self.out_tx.clone();
        let process_mgr = Arc::clone(&self.process_mgr);
        let disk_tx = self.disk_tx.clone();

        let task = tokio::spawn(async move {
            let mut watcher = watcher;
            loop {
                tokio::select! {
                    _ = stop_rx.recv() => break,
                    maybe = dir_rx.recv() => {
                        let Some(first_ev) = maybe else {
                            break;
                        };
                        let mut batch = vec![first_ev];

                        let coalescer = tokio::time::sleep(DIR_COALESCE_WINDOW);
                        tokio::pin!(coalescer);
                        loop {
                            tokio::select! {
                                _ = stop_rx.recv() => return,
                                _ = &mut coalescer => break,
                                more = dir_rx.recv() => {
                                    match more {
                                        Some(ev) => batch.push(ev),
                                        None => return,
                                    }
                                }
                            }
                        }
                        while let Ok(ev) = dir_rx.try_recv() {
                            batch.push(ev);
                        }

                        let mut dirty_jsonl = HashSet::new();
                        let mut dirty_dirs = HashSet::new();
                        let mut should_sync = false;

                        for ev in batch {
                            match ev {
                                DirEvent::Rescan => {
                                    should_sync = true;
                                }
                                DirEvent::FsPath(p) => {
                                    if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                                        dirty_jsonl.insert(p);
                                    } else {
                                        should_sync = true;
                                        if p.is_dir() {
                                            dirty_dirs.insert(p);
                                        }
                                    }
                                }
                            }
                        }

                        if should_sync {
                            let discovered = sync_dir_watches(
                                &mut watcher,
                                &desired_roots,
                                &mut watched_roots,
                                &mut watched_root_set,
                                &mut watched_ancestors,
                                true,
                            );
                            dirty_jsonl.extend(discovered);
                        }

                        for dir_path in dirty_dirs {
                            if classify_agent_for_path(&dir_path, &watched_roots).is_some() {
                                for entry in WalkDir::new(&dir_path).max_depth(4).into_iter().flatten() {
                                    let ep = entry.path();
                                    if ep.is_file()
                                        && ep.extension().and_then(|e| e.to_str()) == Some("jsonl")
                                    {
                                        dirty_jsonl.insert(ep.to_path_buf());
                                    }
                                }
                            }
                        }

                        if dirty_jsonl.is_empty() {
                            continue;
                        }

                        let mut targets: Vec<(PathBuf, AgentKind)> = Vec::new();
                        for path in dirty_jsonl {
                            if !path.is_file() {
                                continue;
                            }
                            if let Some(agent) = classify_agent_for_path(&path, &watched_roots) {
                                targets.push((path, agent));
                            }
                        }
                        if targets.is_empty() {
                            continue;
                        }

                        let home_c = home.clone();
                        let pm_c = Arc::clone(&process_mgr);
                        let out_c = out_tx.clone();
                        if run_on_disk(disk_tx.as_ref(), move || {
                            let metadata = MetadataStore::load(&home_c);
                            let archive = ArchiveStore::load(&home_c);
                            let live = LiveActivity::cached(&home_c);
                            let active_ids = pm_c.active_session_ids();
                            let connected_ids = pm_c.connected_session_ids();
                            let server_workspaces =
                                crate::server_workspaces::ServerWorkspaceIndex::load_default_cached();
                            let ctx = DiscoveryContext {
                                active_session_ids: &active_ids,
                                active_session_pids: &pm_c.active_session_pids(),
                                connected_session_ids: &connected_ids,
                                metadata: &metadata,
                                archive: &archive,
                                live: &live,
                                include_archived: true,
                                server_workspaces: &server_workspaces,
                            };
                            let mut summaries = Vec::new();
                            for (path, agent) in targets {
                                if !path.is_file() {
                                    continue;
                                }
                                let mtime_ms = path
                                    .metadata()
                                    .ok()
                                    .and_then(|m| m.modified().ok())
                                    .map(system_time_ms)
                                    .unwrap_or_else(now_ms);
                                if matches!(agent, AgentKind::Codex | AgentKind::Pi | AgentKind::Cursor)
                                    && !ctx.is_held_open_path(&path, &HashSet::new()) {
                                    continue;
                                }
                                let summary =
                                    summarize_session_by_agent(agent, &path, mtime_ms, &ctx);
                                if (agent == AgentKind::Codex && !ctx.is_shown(&summary))
                                    || (matches!(agent, AgentKind::Pi | AgentKind::Cursor)
                                        && summary.archived && summary.archive_kill_process) {
                                    continue;
                                }
                                summaries.push(summary);
                            }
                            crate::subworkers::populate(&mut summaries, &metadata, ctx.active_session_pids);
                            for session in summaries {
                                let _ = out_c.send(ServerEvent::SessionUpdated { session });
                            }
                        })
                        .await
                        .is_none()
                        {
                            break;
                        }
                    }
                }
            }
        });

        self.dir_watch = Some(ActiveDirWatch {
            rescan_tx,
            stop_tx,
            task,
        });
    }

    pub fn unwatch_directory(&mut self) {
        if let Some(task) = self.shell_task.take() {
            task.abort();
        }
        if let Some(w) = self.dir_watch.take() {
            let _ = w.stop_tx.try_send(());
        }
    }

    /// Stops all active session and directory watchers and awaits their tasks so every
    /// task drops its `disk_tx` and `out_tx` clones cleanly before shutdown.
    pub async fn shutdown(&mut self) {
        if let Some(task) = self.shell_task.take() {
            task.abort();
            let _ = task.await;
        }
        let handles: Vec<tokio::task::JoinHandle<()>> = self
            .sessions
            .drain()
            .map(|(_, w)| {
                let _ = w.stop_tx.try_send(());
                w.task
            })
            .collect();
        let dir_handle = self.dir_watch.take().map(|w| {
            let _ = w.stop_tx.try_send(());
            w.task
        });
        for h in handles {
            let _ = h.await;
        }
        if let Some(h) = dir_handle {
            let _ = h.await;
        }
    }
}

fn session_watch_targets(home: &Path, id: &str, source: &Path, agent: AgentKind) -> Vec<PathBuf> {
    let mut targets = vec![source.to_path_buf()];
    if agent == AgentKind::Antigravity {
        let native = crate::providers::antigravity::transcript_target(home, id, source);
        if native != source {
            targets.push(native);
        }
    }
    targets
}

fn sync_session_watches(
    watcher: &mut RecommendedWatcher,
    targets: &[PathBuf],
    current: &mut HashSet<PathBuf>,
) -> Result<()> {
    let desired: HashSet<_> = targets
        .iter()
        .filter_map(|target| {
            target
                .parent()
                .and_then(|parent| parent.ancestors().find(|p| p.is_dir()))
                .map(Path::to_path_buf)
        })
        .collect();
    for path in desired.difference(current) {
        watcher.watch(path, RecursiveMode::NonRecursive)?;
    }
    for path in current.difference(&desired) {
        let _ = watcher.unwatch(path);
    }
    *current = desired;
    Ok(())
}

fn is_ignored_read_event(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Access(
            AccessKind::Open(_)
                | AccessKind::Read
                | AccessKind::Any
                | AccessKind::Other
                | AccessKind::Close(AccessMode::Read | AccessMode::Other | AccessMode::Any)
        )
    )
}

#[allow(clippy::too_many_arguments)]
fn process_file_update(
    home: &Path,
    session_id: &str,
    path: &Path,
    agent: AgentKind,
    consumed_offset: &mut u64,
    last_probe: &mut String,
    process_mgr: &Arc<ProcessManager>,
    out_tx: &mpsc::UnboundedSender<ServerEvent>,
) -> bool {
    if !path.is_file() {
        return false;
    }

    let mut did_append = false;
    let mut needs_more = false;

    for chunk_idx in 0..MAX_DELTA_CHUNKS_PER_WAKEUP {
        let prev_offset = *consumed_offset;
        let delta = match read_history_delta(agent, path, prev_offset, Some(last_probe.as_str())) {
            Ok(d) => d,
            Err(_) => break,
        };

        match delta {
            DeltaRead::Unchanged { .. } => break,
            DeltaRead::ResetRequired { .. } => {
                if let Ok(page) = crate::continuity::read_page(
                    home,
                    session_id,
                    agent,
                    path,
                    None,
                    Some(DEFAULT_HISTORY_MAX_BYTES),
                    None,
                    None,
                ) {
                    *consumed_offset = page
                        .latest_offset
                        .saturating_sub(crate::continuity::base(home, session_id));
                    *last_probe = page.probe_sha256.clone();
                    let _ = out_tx.send(ServerEvent::History {
                        session_id: session_id.to_string(),
                        messages: page.messages,
                        oldest_offset: Some(page.oldest_offset),
                        latest_offset: Some(page.latest_offset),
                        has_more: page.has_more,
                        file_size: Some(page.file_size),
                        file_mtime_ms: Some(page.file_mtime_ms),
                        probe_sha256: Some(page.probe_sha256),
                        todos: page.todos,
                    });
                    emit_session_updated(home, path, agent, process_mgr, out_tx);
                }
                return false;
            }
            DeltaRead::Appended {
                mut messages,
                latest_offset,
                probe_sha256,
                todos,
                tail_activity,
                ..
            } => {
                *consumed_offset = latest_offset;
                *last_probe = probe_sha256.clone();
                did_append = true;

                let meta = path.metadata().ok();
                let file_size = meta.as_ref().map(|m| m.len()).unwrap_or(latest_offset);
                let mtime_ms = meta
                    .and_then(|m| m.modified().ok())
                    .map(system_time_ms)
                    .unwrap_or_else(now_ms);
                let working = compute_semantic_working(
                    process_mgr.is_session_busy(session_id),
                    tail_activity,
                    mtime_ms,
                    now_ms(),
                );
                let handover = crate::continuity::load(home, session_id).ok().flatten();
                let base = handover.as_ref().map_or(0, |h| h.previous_end);
                if handover.is_some() {
                    crate::continuity::strip_context(&mut messages);
                }
                for message in &mut messages {
                    if let Some((start, end)) = message.source_byte_range {
                        message.source_byte_range = Some((start + base, end + base));
                    }
                }
                if !messages.is_empty() || !todos.is_empty() {
                    let _ = out_tx.send(ServerEvent::HistoryAppend {
                        session_id: session_id.to_string(),
                        messages,
                        latest_offset: Some(latest_offset + base),
                        working: Some(working),
                        todos,
                        probe_sha256: Some(probe_sha256),
                    });
                }

                if latest_offset <= prev_offset || latest_offset >= file_size {
                    break;
                }
                if chunk_idx + 1 == MAX_DELTA_CHUNKS_PER_WAKEUP {
                    needs_more = true;
                }
            }
        }
    }

    if did_append {
        emit_session_updated(home, path, agent, process_mgr, out_tx);
    }
    needs_more
}

fn emit_session_updated(
    home: &Path,
    path: &Path,
    agent: AgentKind,
    process_mgr: &Arc<ProcessManager>,
    out_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    let metadata = MetadataStore::load(home);
    let archive = ArchiveStore::load(home);
    let live = LiveActivity::cached(home);
    let active_ids = process_mgr.active_session_ids();
    let connected_ids = process_mgr.connected_session_ids();
    let server_workspaces = crate::server_workspaces::ServerWorkspaceIndex::load_default_cached();
    let ctx = DiscoveryContext {
        active_session_ids: &active_ids,
        active_session_pids: &process_mgr.active_session_pids(),
        connected_session_ids: &connected_ids,
        metadata: &metadata,
        archive: &archive,
        live: &live,
        include_archived: true,
        server_workspaces: &server_workspaces,
    };
    let mtime_ms = path
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .map(system_time_ms)
        .unwrap_or_else(now_ms);
    let mut summary = summarize_session_by_agent(agent, path, mtime_ms, &ctx);
    crate::subworkers::populate(
        std::slice::from_mut(&mut summary),
        &metadata,
        ctx.active_session_pids,
    );
    if agent == AgentKind::Codex && summary.archived {
        return;
    }
    let _ = out_tx.send(ServerEvent::SessionUpdated { session: summary });
}

fn sync_dir_watches(
    watcher: &mut RecommendedWatcher,
    desired_roots: &[(PathBuf, AgentKind)],
    watched_roots: &mut Vec<(PathBuf, AgentKind)>,
    watched_root_set: &mut HashSet<PathBuf>,
    watched_ancestors: &mut HashSet<PathBuf>,
    scan_new_roots: bool,
) -> Vec<PathBuf> {
    let mut discovered_jsonl = Vec::new();
    for (root, agent) in desired_roots {
        if root.is_dir() {
            let canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
            if !watched_root_set.contains(&canon)
                && watcher.watch(&canon, RecursiveMode::Recursive).is_ok()
            {
                watched_root_set.insert(canon.clone());
                watched_roots.push((canon.clone(), *agent));
                if scan_new_roots {
                    for entry in WalkDir::new(&canon).max_depth(5).into_iter().flatten() {
                        let ep = entry.path();
                        if ep.is_file() && ep.extension().and_then(|e| e.to_str()) == Some("jsonl")
                        {
                            discovered_jsonl.push(ep.to_path_buf());
                        }
                    }
                }
            }
        } else if let Some(ancestor) = nearest_existing_ancestor(root) {
            let canon_anc = std::fs::canonicalize(&ancestor).unwrap_or(ancestor);
            if !watched_ancestors.contains(&canon_anc)
                && !watched_root_set.contains(&canon_anc)
                && watcher
                    .watch(&canon_anc, RecursiveMode::NonRecursive)
                    .is_ok()
            {
                watched_ancestors.insert(canon_anc);
            }
        }
    }
    discovered_jsonl
}

fn nearest_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut cur = path.parent();
    while let Some(p) = cur {
        if p.is_dir() {
            return Some(p.to_path_buf());
        }
        cur = p.parent();
    }
    None
}

fn desired_provider_watch_roots(home: &Path) -> Vec<(PathBuf, AgentKind)> {
    let base_home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    let mut candidates: Vec<(PathBuf, AgentKind)> = Vec::new();

    if let Ok(cfg) = std::env::var("CLAUDE_CONFIG_DIR") {
        let trimmed = cfg.trim();
        if !trimmed.is_empty() {
            let expanded = if let Some(rest) = trimmed.strip_prefix("~/") {
                base_home.join(rest)
            } else {
                PathBuf::from(trimmed)
            };
            candidates.push((expanded.join("projects"), AgentKind::Claude));
        }
    }
    candidates.push((
        base_home.join(".claude").join("projects"),
        AgentKind::Claude,
    ));
    candidates.push((
        base_home.join(".config").join("claude").join("projects"),
        AgentKind::Claude,
    ));

    if let Ok(codex_home) = std::env::var("CODEX_HOME") {
        let trimmed = codex_home.trim();
        if !trimmed.is_empty() {
            let expanded = if let Some(rest) = trimmed.strip_prefix("~/") {
                base_home.join(rest)
            } else {
                PathBuf::from(trimmed)
            };
            candidates.push((expanded.join("sessions"), AgentKind::Codex));
        }
    }
    candidates.push((base_home.join(".codex").join("sessions"), AgentKind::Codex));
    candidates.extend(
        crate::providers::pi::pi_discovery_roots(&base_home)
            .into_iter()
            .map(|p| (p, AgentKind::Pi)),
    );
    candidates.extend(
        crate::providers::cursor::cursor_discovery_roots(&base_home)
            .into_iter()
            .map(|p| (p, AgentKind::Cursor)),
    );

    candidates.push((
        base_home.join(".multibot").join("history"),
        AgentKind::Antigravity,
    ));
    candidates.push((
        base_home.join(".gemini").join("antigravity").join("brain"),
        AgentKind::Antigravity,
    ));
    candidates.push((
        base_home.join(".gemini").join("jetski").join("brain"),
        AgentKind::Antigravity,
    ));
    candidates.push((
        base_home.join(".antigravity").join("sessions"),
        AgentKind::Antigravity,
    ));

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for (root, agent) in candidates {
        if seen.insert(root.clone()) {
            out.push((root, agent));
        }
    }
    out
}

fn classify_agent_for_path(path: &Path, roots: &[(PathBuf, AgentKind)]) -> Option<AgentKind> {
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    for (root, agent) in roots {
        if canon.starts_with(root) {
            if *agent == AgentKind::Cursor {
                if root.ends_with("projects")
                    && path.is_file()
                    && !crate::providers::cursor::is_native_transcript(&canon, root)
                {
                    return None;
                }
            }
            return Some(*agent);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    static WATCH_TEST_COUNTER: AtomicU64 = AtomicU64::new(1);

    fn temp_home(tag: &str) -> PathBuf {
        let n = WATCH_TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("multibotd-watch-{tag}-{}-{n}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn recv_matching<F>(
        rx: &mut mpsc::UnboundedReceiver<ServerEvent>,
        timeout: Duration,
        mut pred: F,
    ) -> Option<ServerEvent>
    where
        F: FnMut(&ServerEvent) -> bool,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        while let Ok(Some(ev)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            if pred(&ev) {
                return Some(ev);
            }
        }
        None
    }

    #[tokio::test]
    async fn antigravity_watch_follows_first_native_transcript_without_polling() {
        use crate::providers::antigravity::{append_history_record, hosted_history_path};
        use serde_json::json;
        let home = temp_home("agy-source-switch");
        let cid = "4d5d5535-249c-4f10-90d9-7ec49c2eeb53";
        let mirror = hosted_history_path(&home, "new-chat");
        append_history_record(
            &mirror,
            &json!({"type":"multibot_meta", "session_id":"new-chat"}),
        )
        .unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let pm = ProcessManager::new(home.clone(), tx.clone());
        let mut wm = WatchManager::new(home.clone(), tx, pm);
        wm.watch_session("new-chat".into(), mirror.clone(), AgentKind::Antigravity)
            .await
            .unwrap();
        assert!(matches!(rx.recv().await, Some(ServerEvent::History { .. })));
        append_history_record(
            &mirror,
            &json!({"type":"multibot_meta", "session_id":"new-chat", "conversation_id":cid}),
        )
        .unwrap();
        // A ping through the watcher is not available, so wait for the metadata update.
        assert!(
            recv_matching(&mut rx, Duration::from_secs(3), |ev| matches!(
                ev,
                ServerEvent::SessionUpdated { .. }
            ))
            .await
            .is_some()
        );
        let native = home
            .join(".gemini/antigravity-cli/brain")
            .join(cid)
            .join(".system_generated/logs/transcript.jsonl");
        fs::create_dir_all(native.parent().unwrap()).unwrap();
        append_history_record(
            &native,
            &json!({"type":"PLANNER_RESPONSE", "content":"First native reply"}),
        )
        .unwrap();
        assert!(recv_matching(&mut rx, Duration::from_secs(3), |ev| matches!(ev, ServerEvent::History { session_id, messages, .. } if session_id == "new-chat" && messages.iter().any(|m| m.text == "First native reply"))).await.is_some());
        append_history_record(
            &native,
            &json!({"type":"PLANNER_RESPONSE", "content":"Next native reply"}),
        )
        .unwrap();
        assert!(recv_matching(&mut rx, Duration::from_secs(3), |ev| matches!(ev, ServerEvent::HistoryAppend { messages, .. } if messages.iter().any(|m| m.text == "Next native reply"))).await.is_some());
        wm.shutdown().await;
        fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn antigravity_mirror_recovers_native_replies_and_watches_native_appends() {
        use crate::providers::antigravity::{append_history_record, hosted_history_path};
        use serde_json::json;
        let home = temp_home("agy-authoritative");
        let cid = "4d5d5535-249c-4f10-90d9-7ec49c2eeb53";
        let mirror = hosted_history_path(&home, "hosted");
        append_history_record(
            &mirror,
            &json!({"type":"multibot_meta", "session_id":"hosted", "conversation_id":cid}),
        )
        .unwrap();
        append_history_record(&mirror, &json!({"type":"multibot_history", "row":{"id":"old", "role":"assistant", "text":"Before restart"}})).unwrap();
        let native = home
            .join(".gemini/antigravity-cli/brain")
            .join(cid)
            .join(".system_generated/logs/transcript.jsonl");
        fs::create_dir_all(native.parent().unwrap()).unwrap();
        append_history_record(
            &native,
            &json!({"type":"PLANNER_RESPONSE", "content":"Before restart"}),
        )
        .unwrap();
        append_history_record(
            &native,
            &json!({"type":"PLANNER_RESPONSE", "content":"Saved while daemon stopped"}),
        )
        .unwrap();
        let page = crate::continuity::read_page(
            &home,
            "hosted",
            AgentKind::Antigravity,
            &mirror,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            page.messages
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            vec!["Before restart", "Saved while daemon stopped"]
        );
        let (tx, mut rx) = mpsc::unbounded_channel();
        let pm = ProcessManager::new(home.clone(), tx.clone());
        let mut wm = WatchManager::new(home.clone(), tx, pm);
        wm.watch_session("hosted".into(), mirror, AgentKind::Antigravity)
            .await
            .unwrap();
        assert!(
            matches!(rx.recv().await, Some(ServerEvent::History { messages, .. }) if messages.len() == 2)
        );
        append_history_record(
            &native,
            &json!({"type":"PLANNER_RESPONSE", "content":"Native live reply"}),
        )
        .unwrap();
        let event = recv_matching(&mut rx, Duration::from_secs(3), |ev| matches!(ev, ServerEvent::HistoryAppend { messages, session_id, .. } if session_id == "hosted" && messages.iter().any(|m| m.text == "Native live reply"))).await;
        assert!(event.is_some(), "must tail native transcript, not mirror");
        wm.shutdown().await;
        fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn switched_chat_watcher_preserves_history_and_translates_live_offsets() {
        use multibot_proto::{HandoverContext, HarnessChoice, SessionSummary};
        let home = temp_home("switch-watch");
        let original = home.join("original.jsonl");
        fs::write(
            &original,
            "{\"type\":\"assistant\",\"uuid\":\"old\",\"message\":{\"content\":\"Old reply\"}}\n",
        )
        .unwrap();
        let base = original.metadata().unwrap().len();
        crate::continuity::save(
            &home,
            "new",
            &crate::continuity::Handover {
                previous: SessionSummary {
                    id: "original".into(),
                    agent: AgentKind::Claude,
                    source_path: original.to_string_lossy().into(),
                    ..Default::default()
                },
                previous_end: base,
                target: HarnessChoice::defaults()[4].clone(),
                context: HandoverContext { turns: vec![] },
                started: true,
            },
        )
        .unwrap();
        let current = home.join("new.jsonl");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let pm = ProcessManager::new(home.clone(), tx.clone());
        let mut wm = WatchManager::new(home.clone(), tx, pm);
        wm.watch_session("new".into(), current.clone(), AgentKind::Cursor)
            .await
            .unwrap();
        let initial = recv_matching(&mut rx, Duration::from_secs(2), |e| {
            matches!(e, ServerEvent::History { .. })
        })
        .await
        .unwrap();
        let ServerEvent::History {
            messages,
            latest_offset,
            ..
        } = initial
        else {
            unreachable!()
        };
        assert_eq!(messages[0].text, "Old reply");
        assert_eq!(latest_offset, Some(base));
        fs::write(&current, "{\"type\":\"assistant\",\"uuid\":\"new-reply\",\"message\":{\"content\":\"New reply\"}}\n").unwrap();
        let event = recv_matching(&mut rx, Duration::from_secs(2), |e| {
            matches!(e, ServerEvent::HistoryAppend { .. })
        })
        .await
        .unwrap();
        let ServerEvent::HistoryAppend {
            messages,
            latest_offset,
            ..
        } = event
        else {
            unreachable!()
        };
        assert_eq!(messages[0].text, "New reply");
        assert_eq!(
            latest_offset,
            Some(base + current.metadata().unwrap().len())
        );
        wm.unwatch_session(Some("new".into()));
        fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn multi_session_concurrent_watch_and_selective_unwatch() {
        let home = temp_home("multi");
        let dir = home.join(".claude/projects/p");
        fs::create_dir_all(&dir).unwrap();
        let path_a = dir.join("sessA.jsonl");
        let path_b = dir.join("sessB.jsonl");
        fs::write(
            &path_a,
            r#"{"type":"user","uuid":"a0","message":{"content":"init A"}}"#.to_string() + "\n",
        )
        .unwrap();
        fs::write(
            &path_b,
            r#"{"type":"user","uuid":"b0","message":{"content":"init B"}}"#.to_string() + "\n",
        )
        .unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let pm = ProcessManager::new(home.clone(), tx.clone());
        let mut wm = WatchManager::new(home.clone(), tx, pm);

        wm.watch_session("sessA".into(), path_a.clone(), AgentKind::Claude)
            .await
            .unwrap();
        wm.watch_session("sessB".into(), path_b.clone(), AgentKind::Claude)
            .await
            .unwrap();

        // Drain initial History events
        let _ = recv_matching(
            &mut rx,
            Duration::from_secs(2),
            |e| matches!(e, ServerEvent::History { session_id, .. } if session_id == "sessA"),
        )
        .await
        .expect("initial History A");
        let _ = recv_matching(
            &mut rx,
            Duration::from_secs(2),
            |e| matches!(e, ServerEvent::History { session_id, .. } if session_id == "sessB"),
        )
        .await
        .expect("initial History B");

        // Append to A -> HistoryAppend for A
        let mut fa = OpenOptions::new().append(true).open(&path_a).unwrap();
        writeln!(
            fa,
            r#"{{"type":"assistant","uuid":"a1","message":{{"content":"reply A"}}}}"#
        )
        .unwrap();
        fa.flush().unwrap();

        let ev_a = recv_matching(
            &mut rx,
            Duration::from_secs(2),
            |e| matches!(e, ServerEvent::HistoryAppend { session_id, .. } if session_id == "sessA"),
        )
        .await
        .expect("HistoryAppend A");
        if let ServerEvent::HistoryAppend { messages, .. } = ev_a {
            assert_eq!(messages[0].text, "reply A");
        }

        // Unwatch A only -> B stays live!
        wm.unwatch_session(Some("sessA"));

        let mut fb = OpenOptions::new().append(true).open(&path_b).unwrap();
        writeln!(
            fb,
            r#"{{"type":"assistant","uuid":"b1","message":{{"content":"reply B"}}}}"#
        )
        .unwrap();
        fb.flush().unwrap();

        let ev_b = recv_matching(
            &mut rx,
            Duration::from_secs(2),
            |e| matches!(e, ServerEvent::HistoryAppend { session_id, .. } if session_id == "sessB"),
        )
        .await
        .expect("HistoryAppend B while A unwatched");
        if let ServerEvent::HistoryAppend { messages, .. } = ev_b {
            assert_eq!(messages[0].text, "reply B");
        }

        // Unwatch all (`None`) stops B
        wm.unwatch_session(None);
        tokio::time::sleep(Duration::from_millis(30)).await;
        while rx.try_recv().is_ok() {}

        writeln!(
            fb,
            r#"{{"type":"assistant","uuid":"b2","message":{{"content":"after unwatch"}}}}"#
        )
        .unwrap();
        fb.flush().unwrap();

        let after = recv_matching(&mut rx, Duration::from_millis(120), |e| {
            matches!(e, ServerEvent::HistoryAppend { .. })
        })
        .await;
        assert!(after.is_none());

        let _ = fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn watcher_handles_partial_line_and_atomic_rename_rotation() {
        let home = temp_home("partial-rot");
        let dir = home.join(".claude/projects/p");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stream.jsonl");
        fs::write(
            &path,
            r#"{"type":"user","uuid":"u1","message":{"content":"start"}}"#.to_string() + "\n",
        )
        .unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let pm = ProcessManager::new(home.clone(), tx.clone());
        let mut wm = WatchManager::new(home.clone(), tx, pm);
        wm.watch_session("stream".into(), path.clone(), AgentKind::Claude)
            .await
            .unwrap();
        let _ = recv_matching(&mut rx, Duration::from_secs(2), |e| {
            matches!(e, ServerEvent::History { .. })
        })
        .await
        .expect("initial History");

        // Write partial line WITHOUT '\n' -> must NOT emit HistoryAppend
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(br#"{"type":"assistant","uuid":"a1","message":{"content":"half"#)
            .unwrap();
        f.flush().unwrap();

        let no_emit = recv_matching(&mut rx, Duration::from_millis(120), |e| {
            matches!(e, ServerEvent::HistoryAppend { .. })
        })
        .await;
        assert!(no_emit.is_none(), "partial line must not emit");

        // Finish line with '\n' -> emits cleanly
        f.write_all(b" complete\"}}\n").unwrap();
        f.flush().unwrap();

        let ev = recv_matching(&mut rx, Duration::from_secs(2), |e| {
            matches!(e, ServerEvent::HistoryAppend { .. })
        })
        .await
        .expect("completed line HistoryAppend");
        if let ServerEvent::HistoryAppend {
            messages,
            probe_sha256,
            ..
        } = ev
        {
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].text, "half complete");
            assert_eq!(probe_sha256.as_deref().map(str::len), Some(64));
        }

        // Atomic rename replacement with different content (larger size) -> probe mismatch -> fresh History!
        let tmp = dir.join("stream.jsonl.tmp");
        fs::write(
            &tmp,
            concat!(
                r#"{"type":"user","uuid":"rot1","message":{"content":"completely replaced transcript line one"}}"#,
                "\n",
                r#"{"type":"assistant","uuid":"rot2","message":{"content":"completely replaced transcript line two"}}"#,
                "\n",
                r#"{"type":"assistant","uuid":"rot3","message":{"content":"completely replaced transcript line three"}}"#,
                "\n"
            ),
        )
        .unwrap();
        fs::rename(&tmp, &path).unwrap();

        let reset_ev = recv_matching(&mut rx, Duration::from_secs(2), |e| {
            matches!(e, ServerEvent::History { .. })
        })
        .await
        .expect("rotation reset History");
        if let ServerEvent::History { messages, .. } = reset_ev {
            assert_eq!(messages.len(), 3);
            assert_eq!(messages[0].id, "rot1");
        }

        wm.unwatch_session(None);
        let _ = fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn new_chat_watches_missing_transcript_and_parent_until_first_message() {
        let home = temp_home("missing-parent");
        let path = home.join(".claude/projects/new/chat.jsonl");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let pm = ProcessManager::new(home.clone(), tx.clone());
        let mut wm = WatchManager::new(home.clone(), tx, pm);
        wm.watch_session("chat".into(), path.clone(), AgentKind::Claude)
            .await
            .unwrap();
        assert!(
            matches!(rx.try_recv(), Ok(ServerEvent::History { messages, has_more: false, .. }) if messages.is_empty())
        );
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "{\"type\":\"user\",\"uuid\":\"first\",\"message\":{\"content\":\"Hello\"}}\n",
        )
        .unwrap();
        let event = recv_matching(&mut rx, Duration::from_secs(2), |e| {
            matches!(e, ServerEvent::HistoryAppend { .. })
        })
        .await;
        assert!(
            matches!(event, Some(ServerEvent::HistoryAppend { messages, .. }) if messages[0].text == "Hello")
        );
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(
            file,
            "{{\"type\":\"assistant\",\"uuid\":\"reply\",\"message\":{{\"content\":\"Reply\"}}}}"
        )
        .unwrap();
        file.flush().unwrap();
        let event = recv_matching(&mut rx, Duration::from_secs(2), |e| {
            matches!(e, ServerEvent::HistoryAppend { .. })
        })
        .await;
        assert!(
            matches!(event, Some(ServerEvent::HistoryAppend { messages, .. }) if messages[0].text == "Reply")
        );
        wm.shutdown().await;
        fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn codex_directory_watch_ignores_unheld_and_dissolved_rollouts() {
        use crate::providers::SessionMetaEntry;
        let home = temp_home("codex-lifecycle");
        let dir = home.join(".codex/sessions/2026/11/28");
        fs::create_dir_all(&dir).unwrap();
        let ids = [
            "11111111-2222-3333-4444-555555555555",
            "aaaaaaaa-2222-3333-4444-555555555555",
            "bbbbbbbb-2222-3333-4444-555555555555",
        ];
        let paths = ids.map(|id| dir.join(format!("rollout-{id}.jsonl")));
        let mut metadata = MetadataStore::default();
        for i in 0..2 {
            metadata.sessions.insert(
                ids[i].into(),
                SessionMetaEntry {
                    agent: Some(AgentKind::Codex),
                    source_path: Some(paths[i].to_string_lossy().into()),
                    ..Default::default()
                },
            );
        }
        metadata.save(&home).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let pm = ProcessManager::new(home.clone(), tx.clone());
        pm.archive_session(
            ids[1].into(),
            AgentKind::Codex,
            paths[1].to_string_lossy().into(),
            true,
        )
        .await;
        while rx.try_recv().is_ok() {}
        let mut wm = WatchManager::new(home.clone(), tx, pm);
        wm.watch_directory();
        for (id, path) in ids.iter().zip(&paths) {
            fs::write(path, format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"/repo\"}}}}\n")).unwrap();
        }
        let deadline = tokio::time::Instant::now() + Duration::from_millis(800);
        let mut seen = Vec::new();
        while let Ok(Some(event)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            if let ServerEvent::SessionUpdated { session } = event {
                seen.push(session.id);
            }
        }
        assert!(!seen.is_empty(), "live Codex session must still appear");
        assert!(
            seen.iter().all(|id| id == ids[0]),
            "stray rollouts: {seen:?}"
        );
        wm.shutdown().await;
        fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn watch_directory_emits_session_updated_and_skips_missing_roots() {
        let home = temp_home("dirwatch");
        let claude_proj = home.join(".claude/projects/demo");
        fs::create_dir_all(&claude_proj).unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let pm = ProcessManager::new(home.clone(), tx.clone());
        let mut wm = WatchManager::new(home.clone(), tx, pm);

        // Codex & Antigravity roots do not exist; watch_directory must still succeed cleanly
        wm.watch_directory();
        tokio::time::sleep(Duration::from_millis(30)).await;

        let new_file = claude_proj.join("dir_sess.jsonl");
        fs::write(
            &new_file,
            r#"{"type":"user","uuid":"u1","cwd":"/demo","message":{"content":"from dir watch"}}"#
                .to_string()
                + "\n",
        )
        .unwrap();

        let ev = recv_matching(
            &mut rx,
            Duration::from_secs(2),
            |e| matches!(e, ServerEvent::SessionUpdated { session } if session.id == "dir_sess"),
        )
        .await
        .expect("SessionUpdated from WatchDirectory");
        if let ServerEvent::SessionUpdated { session } = ev {
            assert_eq!(session.title, "from dir watch");
            assert_eq!(session.agent, AgentKind::Claude);
        }

        wm.unwatch_directory();
        let _ = fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn watch_directory_catches_late_created_claude_root_on_empty_home() {
        let home = temp_home("late-claude");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let pm = ProcessManager::new(home.clone(), tx.clone());
        let mut wm = WatchManager::new(home.clone(), tx, pm);

        // Call WatchDirectory on completely empty $HOME (no provider roots exist yet)
        wm.watch_directory();
        tokio::time::sleep(Duration::from_millis(30)).await;

        let late_dir = home.join(".claude/projects/p");
        fs::create_dir_all(&late_dir).unwrap();
        let late_file = late_dir.join("late.jsonl");
        fs::write(
            &late_file,
            r#"{"type":"user","uuid":"u1","cwd":"/demo","message":{"content":"late claude session"}}"#
                .to_string()
                + "\n",
        )
        .unwrap();

        let ev = recv_matching(
            &mut rx,
            Duration::from_secs(3),
            |e| matches!(e, ServerEvent::SessionUpdated { session } if session.id == "late"),
        )
        .await
        .expect("SessionUpdated for late-created Claude root");
        if let ServerEvent::SessionUpdated { session } = ev {
            assert_eq!(session.agent, AgentKind::Claude);
            assert_eq!(session.title, "late claude session");
        }

        wm.unwatch_directory();
        let _ = fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn watch_directory_catches_late_created_multibot_history_root() {
        let home = temp_home("late-history");
        // Only .claude/projects exists initially; .multibot/history does NOT exist yet.
        fs::create_dir_all(home.join(".claude/projects/p")).unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let pm = ProcessManager::new(home.clone(), tx.clone());
        let mut wm = WatchManager::new(home.clone(), tx, pm);

        wm.watch_directory();
        tokio::time::sleep(Duration::from_millis(30)).await;

        let hist_dir = home.join(".multibot/history");
        fs::create_dir_all(&hist_dir).unwrap();
        let late_file = hist_dir.join("late.history.jsonl");
        fs::write(
            &late_file,
            concat!(
                r#"{"type":"multibot_meta","session_id":"late-agy-1","agent":"antigravity","cwd":"/repo","title":"Late Agy"}"#,
                "\n",
                r#"{"type":"multibot_history","row":{"id":"u1","role":"user","text":"hello late agy"}}"#,
                "\n"
            ),
        )
        .unwrap();

        let ev = recv_matching(
            &mut rx,
            Duration::from_secs(3),
            |e| matches!(e, ServerEvent::SessionUpdated { session } if session.id == "late-agy-1"),
        )
        .await
        .expect("SessionUpdated for late-created .multibot/history root");
        if let ServerEvent::SessionUpdated { session } = ev {
            assert_eq!(session.agent, AgentKind::Antigravity);
            assert_eq!(session.title, "Late Agy");
        }

        wm.unwatch_directory();
        let _ = fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn watch_session_drains_single_burst_exceeding_4mib_without_stranding() {
        let home = temp_home("burst-4mib");
        let dir = home.join(".claude/projects/p");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("burst.jsonl");
        fs::write(
            &path,
            r#"{"type":"user","uuid":"u0","message":{"content":"start"}}"#.to_string() + "\n",
        )
        .unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let pm = ProcessManager::new(home.clone(), tx.clone());
        let mut wm = WatchManager::new(home.clone(), tx, pm);
        wm.watch_session("burst".into(), path.clone(), AgentKind::Claude)
            .await
            .unwrap();
        let _ = recv_matching(&mut rx, Duration::from_secs(2), |e| {
            matches!(e, ServerEvent::History { .. })
        })
        .await
        .expect("initial History");

        // Build a single >4.5 MiB append burst ending with FINAL_SENTINEL
        let filler = "word ".repeat(20_000); // ~100 KiB spaced text (not base64)
        let mut burst = String::with_capacity(4_800_000);
        for i in 0..46 {
            burst.push_str(&format!(
                r#"{{"type":"assistant","uuid":"big-{i}","message":{{"content":[{{"type":"text","text":"chunk {i} {filler}"}}]}}}}"#
            ));
            burst.push('\n');
        }
        burst.push_str(
            r#"{"type":"assistant","uuid":"sentinel","message":{"content":[{"type":"text","text":"FINAL_SENTINEL"}]}}"#,
        );
        burst.push('\n');
        assert!(burst.len() > 4_500_000);

        // Write entire >4.5 MiB burst in one go (no second write!)
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(burst.as_bytes()).unwrap();
            f.flush().unwrap();
        }

        let mut saw_sentinel = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while let Ok(Some(ev)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            if let ServerEvent::HistoryAppend { messages, .. } = ev
                && messages
                    .iter()
                    .any(|m| m.id == "sentinel" && m.text == "FINAL_SENTINEL")
            {
                saw_sentinel = true;
                break;
            }
        }
        assert!(
            saw_sentinel,
            "FINAL_SENTINEL at end of >4.5 MiB single burst must be emitted without a second write"
        );

        wm.unwatch_session(None);
        let _ = fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn watch_session_does_not_wedge_on_single_line_exceeding_4mib() {
        let home = temp_home("giant-single-4mib");
        let dir = home.join(".claude/projects/p");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("giant_single.jsonl");
        fs::write(
            &path,
            r#"{"type":"user","uuid":"u0","message":{"content":"start"}}"#.to_string() + "\n",
        )
        .unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let pm = ProcessManager::new(home.clone(), tx.clone());
        let mut wm = WatchManager::new(home.clone(), tx, pm);
        wm.watch_session("giant_single".into(), path.clone(), AgentKind::Claude)
            .await
            .unwrap();
        let _ = recv_matching(&mut rx, Duration::from_secs(2), |e| {
            matches!(e, ServerEvent::History { .. })
        })
        .await
        .expect("initial History");

        // Append a single ~4.6 MiB complete JSONL line followed immediately by a normal message
        let mut giant_line = String::with_capacity(4_650_000);
        giant_line.push_str(
            r#"{"type":"assistant","uuid":"giant1","message":{"content":[{"type":"text","text":""#,
        );
        for _ in 0..2_300_000 {
            giant_line.push_str("a ");
        }
        giant_line.push_str("\"}]}}\n");
        assert!(giant_line.len() > 4_500_000);

        let follow_up = r#"{"type":"assistant","uuid":"follow1","message":{"content":[{"type":"text","text":"FOLLOW_UP_AFTER_GIANT"}]}}"#
            .to_string()
            + "\n";
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(giant_line.as_bytes()).unwrap();
            f.write_all(follow_up.as_bytes()).unwrap();
            f.flush().unwrap();
        }

        let ev1 = recv_matching(&mut rx, Duration::from_secs(5), |e| {
            matches!(
                e,
                ServerEvent::HistoryAppend { messages, .. }
                    if messages.iter().any(|m| m.id == "follow1" && m.text == "FOLLOW_UP_AFTER_GIANT")
            )
        })
        .await;
        assert!(
            ev1.is_some(),
            "FOLLOW_UP_AFTER_GIANT must arrive via HistoryAppend after a single >4 MiB JSONL line"
        );

        // Append a second normal message to verify the watcher remains completely live
        let still_alive = r#"{"type":"assistant","uuid":"alive1","message":{"content":[{"type":"text","text":"STILL_ALIVE_CHECK"}]}}"#
            .to_string()
            + "\n";
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(still_alive.as_bytes()).unwrap();
            f.flush().unwrap();
        }

        let ev2 = recv_matching(&mut rx, Duration::from_secs(5), |e| {
            matches!(
                e,
                ServerEvent::HistoryAppend { messages, .. }
                    if messages.iter().any(|m| m.id == "alive1" && m.text == "STILL_ALIVE_CHECK")
            )
        })
        .await;
        assert!(
            ev2.is_some(),
            "STILL_ALIVE_CHECK must arrive via HistoryAppend on subsequent writes"
        );

        wm.unwatch_session(None);
        let _ = fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn watch_session_streams_live_400kib_and_1_5mib_base64_image_lines() {
        let home = temp_home("live-b64-stream");
        let dir = home.join(".claude/projects/p");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("live_b64.jsonl");
        fs::write(
            &path,
            r#"{"type":"user","uuid":"u0","message":{"content":"start"}}"#.to_string() + "\n",
        )
        .unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let pm = ProcessManager::new(home.clone(), tx.clone());
        let mut wm = WatchManager::new(home.clone(), tx, pm);
        wm.watch_session("live_b64".into(), path.clone(), AgentKind::Claude)
            .await
            .unwrap();
        let init_offset = match recv_matching(&mut rx, Duration::from_secs(2), |e| {
            matches!(e, ServerEvent::History { .. })
        })
        .await
        .expect("initial History")
        {
            ServerEvent::History { latest_offset, .. } => latest_offset.unwrap_or(0),
            _ => unreachable!(),
        };

        // 1. Append a 400 KiB inline data:image/png;base64,... line
        let b64_400k = "A".repeat(400 * 1024);
        let line_400k = format!(
            "{{\"type\":\"assistant\",\"uuid\":\"a-400k\",\"message\":{{\"content\":\"Shot data:image/png;base64,{b64_400k}==\"}}}}\n"
        );
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(line_400k.as_bytes()).unwrap();
            f.flush().unwrap();
        }
        let off1 = match recv_matching(&mut rx, Duration::from_secs(5), |e| {
            matches!(
                e,
                ServerEvent::HistoryAppend { messages, .. }
                    if messages.iter().any(|m| m.id == "a-400k")
            )
        })
        .await
        .expect("HistoryAppend for 400 KiB base64 line")
        {
            ServerEvent::HistoryAppend {
                messages,
                latest_offset,
                probe_sha256,
                ..
            } => {
                let m = messages.iter().find(|m| m.id == "a-400k").unwrap();
                assert_eq!(m.text, "Shot [image omitted]");
                assert!(!m.text.contains(&"A".repeat(64)));
                assert_eq!(probe_sha256.as_deref().map(str::len), Some(64));
                let off = latest_offset.unwrap_or(0);
                assert!(off > init_offset);
                off
            }
            _ => unreachable!(),
        };

        // 2. Append a 1.5 MiB Anthropic structured base64 image line + normal follow-up line
        let b64_1_5m = "B".repeat(1_536 * 1024);
        let line_1_5m = format!(
            "{{\"type\":\"user\",\"uuid\":\"u-1_5m\",\"message\":{{\"content\":[{{\"type\":\"image\",\"source\":{{\"type\":\"base64\",\"media_type\":\"image/png\",\"data\":\"{b64_1_5m}\"}}}}]}}}}\n"
        );
        let line_follow = "{\"type\":\"assistant\",\"uuid\":\"a-follow\",\"message\":{\"content\":\"AFTER_B64\"}}\n";
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(line_1_5m.as_bytes()).unwrap();
            f.write_all(line_follow.as_bytes()).unwrap();
            f.flush().unwrap();
        }

        let mut saw_1_5m = false;
        let mut saw_follow = false;
        let mut last_off = off1;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while (!saw_1_5m || !saw_follow)
            && let Ok(Some(ev)) = tokio::time::timeout_at(deadline, rx.recv()).await
        {
            if let ServerEvent::HistoryAppend {
                messages,
                latest_offset,
                probe_sha256,
                ..
            } = ev
            {
                assert_eq!(probe_sha256.as_deref().map(str::len), Some(64));
                if let Some(off) = latest_offset {
                    assert!(off >= last_off);
                    last_off = off;
                }
                for m in messages {
                    if m.id == "u-1_5m" {
                        assert_eq!(m.text, "[image omitted]");
                        assert!(!m.text.contains(&"B".repeat(64)));
                        saw_1_5m = true;
                    } else if m.id == "a-follow" {
                        assert_eq!(m.text, "AFTER_B64");
                        saw_follow = true;
                    }
                }
            }
        }
        assert!(
            saw_1_5m,
            "1.5 MiB Anthropic base64 line must stream via HistoryAppend"
        );
        assert!(
            saw_follow,
            "follow-up line after 1.5 MiB base64 must stream via HistoryAppend"
        );

        wm.unwatch_session(None);
        let _ = fs::remove_dir_all(&home);
    }
}
