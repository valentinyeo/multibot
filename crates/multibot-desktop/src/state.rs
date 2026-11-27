use gpui_kit::{App, Global, Task};
use multibot_core::outbox::ChatOutbox;
use multibot_core::workspaces::{PaletteStats, WorkspaceStore, now_unix_ms};
use multibot_core::{DaemonTarget, FootprintSnapshot, MultibotSettings};
use multibot_proto::{ClientRequest, ServerEvent, SessionSummary};
use multibot_updater::{CheckOutcome, ReleaseInfo, StagedUpdate, UpdateState};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc, oneshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MainSurface {
    Chat,
    Inbox,
    Settings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActiveModal {
    None,
    Palette,
    Shortcuts,
}

pub(crate) enum UiBridgeEvent {
    Connected {
        host_label: String,
        req_tx: mpsc::UnboundedSender<ClientRequest>,
    },
    Server(ServerEvent),
    Disconnected(String),
}

#[derive(Debug)]
pub(crate) struct HostConnectionState {
    pub host_label: String,
    pub target: DaemonTarget,
    pub status: String,
    pub connected: bool,
    pub manually_disconnected: bool,
    pub reconnect_attempt: u32,
    pub daemon_tx: Option<mpsc::UnboundedSender<ClientRequest>>,
    pub shutdown_tx: Option<oneshot::Sender<()>>,
    pub connection_gen: u64,
    pub daemon_version: Option<String>,
    pub daemon_binary_hash: Option<String>,
    pub daemon_upgrade_attempted: bool,
    pub sessions: Vec<SessionSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostStatusSnapshot {
    pub host_label: String,
    pub status: String,
    pub connected: bool,
    pub session_count: usize,
    pub daemon_version: Option<String>,
}

/// Event fanned out over `SharedAppStore.daemon_event_tx` to all open `Ctrl+N` windows.
#[derive(Debug, Clone)]
pub(crate) struct DaemonBroadcastEvent {
    /// `0` for App-scoped daemon consumer; non-zero (`view_id`) for direct single-view test calls.
    pub origin_view_id: u64,
    pub host_label: String,
    /// True when an App-scoped `pending_older_page_before` entry matched and was consumed
    /// for this `ServerEvent::History`.
    pub is_older_page_hint: bool,
    pub event: ServerEvent,
}

/// App-scoped shared offline outbox state so all `Ctrl+N` windows enqueue, cancel,
/// persist, and flush offline prompts against a single authoritative queue (never
/// re-loading or double-flushing the same entries across sibling windows on `HelloAck`).
#[derive(Debug, Default)]
pub(crate) struct SharedChatOutbox {
    pub outbox: ChatOutbox,
    pub loaded_from_disk: bool,
    pub persist_to_disk: bool,
}

/// Internal runtime events pushed from background updater tasks to the GPUI app/view.
pub(crate) enum UpdateBridgeEvent {
    CheckCompleted {
        outcome: CheckOutcome,
        manual: bool,
    },
    CheckFailed {
        message: String,
        manual: bool,
    },
    DownloadProgress {
        version: String,
        bytes_done: u64,
        bytes_total: Option<u64>,
    },
    Verifying {
        version: String,
    },
    StagedReady(StagedUpdate),
    ApplySucceeded,
    RestartSucceeded,
    Failed {
        message: String,
        retryable: bool,
    },
}

/// Command row contributed to `Ctrl+K` by pluggable modules (e.g., `update_ui.rs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaletteEntry {
    pub id: String,
    pub label: String,
    pub chord: String,
    pub category: &'static str,
}

/// Compares all user-visible fields of [`MultibotSettings`], ignoring background-only HTTP ETag
/// caching metadata (`last_update_etag`) so silent background ETag updates never trigger spurious
/// sibling window redraws (`cx.notify()`).
pub(crate) fn settings_visible_eq(a: &MultibotSettings, b: &MultibotSettings) -> bool {
    let MultibotSettings {
        voice: a_voice,
        gpu: a_gpu,
        ssh_hosts: a_hosts,
        selected_host: a_sel,
        sidebar_mode: a_sb,
        chat_font_size_pt: a_font,
        composer_ctrl_enter: a_send,
        queue_while_working: a_queue,
        ui_scale_pct: a_scale,
        dismissed_update_version: a_dis,
        last_update_etag: _,
        auto_check_updates: a_auto,
    } = a;
    let MultibotSettings {
        voice: b_voice,
        gpu: b_gpu,
        ssh_hosts: b_hosts,
        selected_host: b_sel,
        sidebar_mode: b_sb,
        chat_font_size_pt: b_font,
        composer_ctrl_enter: b_send,
        queue_while_working: b_queue,
        ui_scale_pct: b_scale,
        dismissed_update_version: b_dis,
        last_update_etag: _,
        auto_check_updates: b_auto,
    } = b;
    a_voice == b_voice
        && a_gpu == b_gpu
        && a_hosts == b_hosts
        && a_sel == b_sel
        && a_sb == b_sb
        && a_send == b_send
        && a_queue == b_queue
        && a_font == b_font
        && a_scale == b_scale
        && a_dis == b_dis
        && a_auto == b_auto
}

/// Shared multi-window GPUI `Global` store synchronized across all OS windows (`Ctrl+N`).
#[derive(Debug)]
pub(crate) struct SharedAppStore {
    pub(crate) host_connections: HashMap<String, HostConnectionState>,
    pub(crate) host_sessions: Vec<(String, SessionSummary)>,
    pub settings: MultibotSettings,
    pub sessions: Vec<SessionSummary>,
    pub workspace_store: WorkspaceStore,
    pub reminder_wake_seq: u64,
    pub reminder_deadline_ms: Option<i64>,
    pub reminder_timer: Option<Task<()>>,
    pub palette_stats: PaletteStats,
    pub unread_only: bool,
    pub connected_host: Option<String>,
    pub connection_status: String,
    pub daemon_tx: Option<mpsc::UnboundedSender<ClientRequest>>,
    /// Broadcast channel fanning out [`DaemonBroadcastEvent`]s to open `Ctrl+N` windows
    /// (`origin_view_id == 0` for App-scoped daemon consumer; non-zero for direct single-view test calls).
    pub daemon_event_tx: broadcast::Sender<DaemonBroadcastEvent>,
    /// App-scoped queue of requested `before_offset` older-page boundaries per `session_id`,
    /// populated on `load_older_history` and consumed/pruned on incoming `ServerEvent::History`
    /// so all sibling windows receive an exact `is_older_page_hint` without misclassifying
    /// partial trailing `.jsonl` lines.
    pub pending_older_page_before: HashMap<String, VecDeque<u64>>,
    /// App-scoped shared offline outbox so multi-window `HelloAck` flushes each entry exactly once.
    pub shared_chat_outbox: Arc<Mutex<SharedChatOutbox>>,
    /// Monotonic global daemon connection generation (guards against stale `UiBridgeEvent`s across windows).
    pub connection_gen: u64,
    /// App-scoped shutdown handle for the active Tokio daemon task (dropped on reconnect or `window_count == 0`).
    pub connection_shutdown_tx: Option<oneshot::Sender<()>>,
    /// Active daemon target stored at App scope for reconnect and remote upgrade guards.
    pub connected_target: Option<DaemonTarget>,
    /// One-shot remote `multibotd` upgrade guard per user-initiated connection.
    pub daemon_upgrade_attempted: bool,
    pub daemon_version: Option<String>,
    pub daemon_binary_hash: Option<String>,
    /// Latest user-facing notice emitted by App-scoped daemon events (monotonic seq avoids re-showing).
    pub daemon_notice: Option<String>,
    pub daemon_notice_seq: u64,
    /// Original CLI args preserved across update relaunch & manual restart (including `--gpu=...`).
    pub preserved_cli_args: Vec<String>,
    /// Shared update action sender so any `Ctrl+N` window can trigger Check / Update / Restart.
    pub update_action_tx: Option<mpsc::UnboundedSender<crate::update_ui::UpdateUiAction>>,
    /// Ensures only one 5-minute background update poller runs per process.
    pub update_poller_started: bool,
    /// True while a manual update check/download/restart action is running at App scope.
    pub update_in_flight: bool,
    /// Latest discovered release cached at App scope (survives Window 1 closing).
    pub update_last_release: Option<ReleaseInfo>,
    /// Latest update state shared across sibling windows.
    pub update_state: UpdateState,
    /// Latest user-facing notice emitted by App-scoped updater events (monotonic seq avoids re-showing).
    pub update_notice: Option<String>,
    pub update_notice_seq: u64,
    /// Optional settings path override used by tests for isolated App-scoped updater persistence.
    pub update_settings_path_override: Option<PathBuf>,
    pub account: crate::account::AccountUi,
    pub persist_to_disk: bool,
    pub window_count: usize,
    pub revision: u64,
}

impl Global for SharedAppStore {}

impl SharedAppStore {
    pub(crate) fn schedule_reminder_timer(cx: &mut App) {
        let now = now_unix_ms();
        let store = cx.global_mut::<Self>();
        let deadline = store.workspace_store.next_deadline_ms(now);
        if store.reminder_deadline_ms == deadline {
            return;
        }
        store.reminder_timer = None;
        store.reminder_deadline_ms = deadline;
        let Some(at) = deadline else { return };
        let task = cx.spawn(async move |cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(
                    at.saturating_sub(now_unix_ms()).max(0) as u64,
                ))
                .await;
            cx.update(|cx| {
                let store = cx.global_mut::<Self>();
                store.reminder_timer = None;
                store.reminder_deadline_ms = None;
                let woken = store.workspace_store.wake_due_workspaces(now_unix_ms());
                if !woken.is_empty() && store.persist_to_disk {
                    let _ = store.workspace_store.save();
                }
                store.reminder_wake_seq = store.reminder_wake_seq.wrapping_add(1);
                store.bump_revision();
                Self::schedule_reminder_timer(cx);
            });
        });
        cx.global_mut::<Self>().reminder_timer = Some(task);
    }

    pub(crate) fn new(settings: MultibotSettings) -> Self {
        let (daemon_event_tx, _) = broadcast::channel(256);
        Self {
            host_connections: HashMap::new(),
            host_sessions: Vec::new(),
            settings,
            sessions: Vec::new(),
            workspace_store: WorkspaceStore::new(),
            reminder_wake_seq: 0,
            reminder_deadline_ms: None,
            reminder_timer: None,
            palette_stats: PaletteStats::default(),
            unread_only: false,
            connected_host: None,
            connection_status: "Offline".to_string(),
            daemon_tx: None,
            daemon_event_tx,
            pending_older_page_before: HashMap::new(),
            shared_chat_outbox: Arc::new(Mutex::new(SharedChatOutbox::default())),
            connection_gen: 0,
            connection_shutdown_tx: None,
            connected_target: None,
            daemon_upgrade_attempted: false,
            daemon_version: None,
            daemon_binary_hash: None,
            daemon_notice: None,
            daemon_notice_seq: 0,
            preserved_cli_args: Vec::new(),
            update_action_tx: None,
            update_poller_started: false,
            update_in_flight: false,
            update_last_release: None,
            update_state: UpdateState::Idle,
            update_notice: None,
            update_notice_seq: 0,
            update_settings_path_override: None,
            account: crate::account::AccountUi::default(),
            persist_to_disk: false,
            window_count: 0,
            revision: 0,
        }
    }

    pub(crate) fn new_persisted_with_args(
        settings: MultibotSettings,
        preserved_cli_args: Vec<String>,
    ) -> Self {
        let (daemon_event_tx, _) = broadcast::channel(256);
        Self {
            host_connections: HashMap::new(),
            host_sessions: Vec::new(),
            settings,
            sessions: Vec::new(),
            workspace_store: WorkspaceStore::load(),
            reminder_wake_seq: 0,
            reminder_deadline_ms: None,
            reminder_timer: None,
            palette_stats: PaletteStats::load(),
            unread_only: false,
            connected_host: None,
            connection_status: "Offline".to_string(),
            daemon_tx: None,
            daemon_event_tx,
            pending_older_page_before: HashMap::new(),
            shared_chat_outbox: Arc::new(Mutex::new(SharedChatOutbox::default())),
            connection_gen: 0,
            connection_shutdown_tx: None,
            connected_target: None,
            daemon_upgrade_attempted: false,
            daemon_version: None,
            daemon_binary_hash: None,
            daemon_notice: None,
            daemon_notice_seq: 0,
            preserved_cli_args,
            update_action_tx: None,
            update_poller_started: false,
            update_in_flight: false,
            update_last_release: None,
            update_state: UpdateState::Idle,
            update_notice: None,
            update_notice_seq: 0,
            update_settings_path_override: None,
            account: crate::account::AccountUi::default(),
            persist_to_disk: true,
            window_count: 0,
            revision: 0,
        }
    }

    pub(crate) fn init(cx: &mut App, settings: MultibotSettings) {
        if !cx.has_global::<Self>() {
            cx.set_global(Self::new(settings));
        }
    }

    pub(crate) fn init_persisted_with_args(
        cx: &mut App,
        settings: MultibotSettings,
        preserved_cli_args: Vec<String>,
    ) {
        if !cx.has_global::<Self>() {
            cx.set_global(Self::new_persisted_with_args(settings, preserved_cli_args));
        }
    }

    pub(crate) fn rebuild_merged_sessions(&mut self) {
        self.host_sessions = self
            .host_connections
            .values()
            .filter(|host| host.connected)
            .flat_map(|host| {
                host.sessions
                    .iter()
                    .cloned()
                    .map(move |s| (host.host_label.clone(), s))
            })
            .collect();
        self.host_sessions.sort_by(|(ha, a), (hb, b)| {
            b.updated_at_ms
                .cmp(&a.updated_at_ms)
                .then_with(|| ha.cmp(hb))
                .then_with(|| a.id.cmp(&b.id))
        });
        self.sessions = self.host_sessions.iter().map(|(_, s)| s.clone()).collect();
    }

    pub(crate) fn refresh_connection_summary(&mut self) {
        if self.host_connections.is_empty() {
            return;
        }
        let mut hosts: Vec<_> = self.host_connections.values().collect();
        hosts.sort_by(|a, b| a.host_label.cmp(&b.host_label));
        let connected = hosts.iter().filter(|h| h.connected).count();
        self.connection_status = if hosts.len() == 1 {
            let h = hosts[0];
            if h.connected {
                let version = h.daemon_version.as_deref().unwrap_or("?");
                format!(
                    "{} · v{version} · {} sessions",
                    h.host_label,
                    h.sessions.len()
                )
            } else {
                h.status.clone()
            }
        } else {
            format!(
                "{connected}/{} hosts · {} sessions",
                hosts.len(),
                self.host_sessions.len()
            )
        };
        let first = hosts.iter().find(|h| h.connected).or_else(|| hosts.first());
        self.connected_host = first.map(|h| h.host_label.clone());
        self.daemon_tx = first.and_then(|h| h.daemon_tx.clone());
        self.connected_target = first.map(|h| h.target.clone());
        self.daemon_version = first.and_then(|h| h.daemon_version.clone());
        self.daemon_binary_hash = first.and_then(|h| h.daemon_binary_hash.clone());
    }

    pub(crate) fn host_status_snapshots(&self) -> Vec<HostStatusSnapshot> {
        let mut snapshots: Vec<_> = self
            .host_connections
            .values()
            .map(|h| HostStatusSnapshot {
                host_label: h.host_label.clone(),
                status: h.status.clone(),
                connected: h.connected,
                session_count: h.sessions.len(),
                daemon_version: h.daemon_version.clone(),
            })
            .collect();
        snapshots.sort_by(|a, b| a.host_label.cmp(&b.host_label));
        snapshots
    }

    pub(crate) fn bump_revision(&mut self) -> u64 {
        self.revision = self.revision.wrapping_add(1);
        self.revision
    }
}

/// Formats the RAM/CPU telemetry snippet rendered in the sidebar status card.
/// Clamps non-finite (`NaN`/`±inf`), negative, and `-0.0` values to `0.0` to prevent formatting jitter.
pub(crate) fn format_footprint(snapshot: &FootprintSnapshot) -> String {
    let mem = if snapshot.memory_mb.is_finite() && snapshot.memory_mb > 0.0 {
        snapshot.memory_mb
    } else {
        0.0
    };
    let cpu = if snapshot.cpu_percent.is_finite() && snapshot.cpu_percent > 0.0 {
        snapshot.cpu_percent
    } else {
        0.0
    };
    format!("{mem:.1} MB · {cpu:.1}% CPU")
}

/// Hard Invariant #4 guard: updates `current` & `current_label` and returns `true`
/// ONLY when the formatted RAM/CPU string actually changes.
pub(crate) fn update_footprint_if_changed(
    current: &mut FootprintSnapshot,
    current_label: &mut String,
    next: FootprintSnapshot,
) -> bool {
    let next_label = format_footprint(&next);
    if next_label == *current_label {
        return false;
    }
    *current = next;
    *current_label = next_label;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui_kit::test]
    fn reminder_timer_rearms_cancels_and_wakes_overdue_snooze(cx: &mut gpui_kit::TestAppContext) {
        cx.update(|cx| {
            SharedAppStore::init(cx, MultibotSettings::default());
            let now = now_unix_ms();
            cx.global_mut::<SharedAppStore>()
                .workspace_store
                .set_reminder("a", Some(now + 60_000), None);
            SharedAppStore::schedule_reminder_timer(cx);
            assert_eq!(
                cx.global::<SharedAppStore>().reminder_deadline_ms,
                Some(now + 60_000)
            );
            cx.global_mut::<SharedAppStore>()
                .workspace_store
                .set_reminder("b", Some(now + 30_000), None);
            SharedAppStore::schedule_reminder_timer(cx);
            assert_eq!(
                cx.global::<SharedAppStore>().reminder_deadline_ms,
                Some(now + 30_000)
            );
            cx.global_mut::<SharedAppStore>()
                .workspace_store
                .set_reminder("a", None, None);
            cx.global_mut::<SharedAppStore>()
                .workspace_store
                .set_reminder("b", None, None);
            SharedAppStore::schedule_reminder_timer(cx);
            assert!(cx.global::<SharedAppStore>().reminder_timer.is_none());
            cx.global_mut::<SharedAppStore>()
                .workspace_store
                .snooze_workspace(
                    multibot_core::workspaces::DEFAULT_WORKSPACE_ID,
                    now - 1,
                    now - 2,
                )
                .unwrap();
            SharedAppStore::schedule_reminder_timer(cx);
        });
        cx.run_until_parked();
        cx.update(|cx| {
            let store = cx.global::<SharedAppStore>();
            assert!(store.reminder_deadline_ms.is_none());
            assert!(store.reminder_timer.is_none());
            assert!(
                store
                    .workspace_store
                    .snoozed_workspaces(now_unix_ms())
                    .is_empty()
            );
            assert_eq!(store.reminder_wake_seq, 1);
        });
    }

    #[test]
    fn identical_values_do_not_trigger_notify() {
        let mut snap = FootprintSnapshot {
            memory_mb: 42.3,
            cpu_percent: 0.0,
        };
        let mut label = format_footprint(&snap);
        let changed = update_footprint_if_changed(
            &mut snap,
            &mut label,
            FootprintSnapshot {
                memory_mb: 42.3,
                cpu_percent: 0.0,
            },
        );
        assert!(!changed);
        assert_eq!(label, "42.3 MB · 0.0% CPU");
    }

    #[test]
    fn sub_tenth_jitter_does_not_trigger_notify() {
        let mut snap = FootprintSnapshot {
            memory_mb: 42.31,
            cpu_percent: 0.02,
        };
        let mut label = format_footprint(&snap);
        assert_eq!(label, "42.3 MB · 0.0% CPU");

        let changed = update_footprint_if_changed(
            &mut snap,
            &mut label,
            FootprintSnapshot {
                memory_mb: 42.34,
                cpu_percent: 0.04,
            },
        );
        assert!(!changed);
        assert_eq!(label, "42.3 MB · 0.0% CPU");
    }

    #[test]
    fn visible_memory_or_cpu_change_triggers_notify() {
        let mut snap = FootprintSnapshot {
            memory_mb: 42.31,
            cpu_percent: 0.0,
        };
        let mut label = format_footprint(&snap);

        assert!(update_footprint_if_changed(
            &mut snap,
            &mut label,
            FootprintSnapshot {
                memory_mb: 42.41,
                cpu_percent: 0.0,
            },
        ));
        assert_eq!(label, "42.4 MB · 0.0% CPU");

        assert!(update_footprint_if_changed(
            &mut snap,
            &mut label,
            FootprintSnapshot {
                memory_mb: 42.41,
                cpu_percent: 0.2,
            },
        ));
        assert_eq!(label, "42.4 MB · 0.2% CPU");
    }

    #[test]
    fn negative_zero_normalizes_cleanly() {
        let label = format_footprint(&FootprintSnapshot {
            memory_mb: -0.0,
            cpu_percent: -0.0,
        });
        assert_eq!(label, "0.0 MB · 0.0% CPU");
    }

    #[test]
    fn non_finite_and_negative_values_clamp_and_suppress_redraws() {
        let cases = [
            FootprintSnapshot {
                memory_mb: f64::INFINITY,
                cpu_percent: f32::INFINITY,
            },
            FootprintSnapshot {
                memory_mb: f64::NEG_INFINITY,
                cpu_percent: f32::NEG_INFINITY,
            },
            FootprintSnapshot {
                memory_mb: f64::NAN,
                cpu_percent: f32::NAN,
            },
            FootprintSnapshot {
                memory_mb: -12.5,
                cpu_percent: -3.4,
            },
        ];

        for snap in cases {
            assert_eq!(format_footprint(&snap), "0.0 MB · 0.0% CPU");
        }

        let mut current = FootprintSnapshot {
            memory_mb: 0.0,
            cpu_percent: 0.0,
        };
        let mut label = format_footprint(&current);
        assert_eq!(label, "0.0 MB · 0.0% CPU");

        for snap in cases {
            assert!(
                !update_footprint_if_changed(&mut current, &mut label, snap),
                "repeated non-finite/negative snapshot should not trigger cx.notify()"
            );
            assert_eq!(label, "0.0 MB · 0.0% CPU");
        }
    }

    #[test]
    fn shared_app_store_defaults() {
        let store = SharedAppStore::new(MultibotSettings::default());
        assert_eq!(store.window_count, 0);
        assert!(store.sessions.is_empty());
        assert_eq!(store.settings, MultibotSettings::default());
    }
}
