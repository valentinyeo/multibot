use gpui_kit::{App, Global};
use multibot_core::outbox::ChatOutbox;
use multibot_core::workspaces::{PaletteStats, WorkspaceStore};
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

/// Event fanned out over `SharedAppStore.daemon_event_tx` to all open `Ctrl+N` windows.
#[derive(Debug, Clone)]
pub(crate) struct DaemonBroadcastEvent {
    /// `0` for App-scoped daemon consumer; non-zero (`view_id`) for direct single-view test calls.
    pub origin_view_id: u64,
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
        gpu: a_gpu,
        ssh_hosts: a_hosts,
        selected_host: a_sel,
        sidebar_mode: a_sb,
        chat_font_size_pt: a_font,
        ui_scale_pct: a_scale,
        dismissed_update_version: a_dis,
        last_update_etag: _,
        auto_check_updates: a_auto,
    } = a;
    let MultibotSettings {
        gpu: b_gpu,
        ssh_hosts: b_hosts,
        selected_host: b_sel,
        sidebar_mode: b_sb,
        chat_font_size_pt: b_font,
        ui_scale_pct: b_scale,
        dismissed_update_version: b_dis,
        last_update_etag: _,
        auto_check_updates: b_auto,
    } = b;
    a_gpu == b_gpu
        && a_hosts == b_hosts
        && a_sel == b_sel
        && a_sb == b_sb
        && a_font == b_font
        && a_scale == b_scale
        && a_dis == b_dis
        && a_auto == b_auto
}

/// Shared multi-window GPUI `Global` store synchronized across all OS windows (`Ctrl+N`).
#[derive(Debug)]
pub(crate) struct SharedAppStore {
    pub settings: MultibotSettings,
    pub sessions: Vec<SessionSummary>,
    pub workspace_store: WorkspaceStore,
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
    pub persist_to_disk: bool,
    pub window_count: usize,
    pub revision: u64,
}

impl Global for SharedAppStore {}

impl SharedAppStore {
    pub(crate) fn new(settings: MultibotSettings) -> Self {
        let (daemon_event_tx, _) = broadcast::channel(256);
        Self {
            settings,
            sessions: Vec::new(),
            workspace_store: WorkspaceStore::new(),
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
            settings,
            sessions: Vec::new(),
            workspace_store: WorkspaceStore::load(),
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
