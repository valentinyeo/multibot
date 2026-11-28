use crate::chat_view::{ChatViewState, FailedSend};
use crate::computer_view::ComputerViewState;
use crate::icons::{self, lucide_icon};
use crate::inbox::InboxState;
use crate::palette::PaletteState;
use crate::sidebar::{InlineRenameState, RenameKeyOutcome, SidebarState};
use crate::slash_picker::SlashPickerState;
use crate::state::{
    ActiveModal, DaemonBroadcastEvent, HostConnectionState, HostStatusSnapshot, MainSurface,
    SharedAppStore, SharedChatOutbox, UiBridgeEvent, UpdateBridgeEvent, format_footprint,
    settings_visible_eq, update_footprint_if_changed,
};
use crate::table_modal::TableModalState;
use crate::terminal_pane::TerminalPaneState;
use crate::theme::{self, RADIUS_CHIP};
use crate::update_ui::{UpdateUiAction, UpdateUiState};
use gpui_kit::{
    App, AppContext, Bounds, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, ModifiersChangedEvent, ParentElement, Render, StatefulInteractiveElement, Styled, Subscription,
    Task, TestSupportExt, TitlebarOptions, Window, WindowBounds, WindowOptions, actions,
    component::{
        Sizable,
        button::Button,
        h_flex,
        input::{InputEvent, InputState, TextareaState},
    },
    div,
    prelude::FluentBuilder,
    px, size,
};
use multibot_core::cache::{
    CacheKey, ChatCache, MAX_SESSION_CACHE_BYTES, ReconcileOutcome, SwitchToken,
};
use multibot_core::chat_scroll::{PendingScrollTarget, UnreadVisit};
use multibot_core::outbox::ChatOutbox;
use multibot_core::settings::{MAX_CHAT_FONT_SIZE_PT, MIN_CHAT_FONT_SIZE_PT};
use multibot_core::workspaces::{
    ReorderDirection, WorkspaceStore, WorkspaceView, canonical_session_key, now_unix_ms,
};
use multibot_core::zigshel_import::{
    import_zigshel_bundle_readonly, legacy_zigshel_dir, merge_zigshel_bundle,
};
use multibot_core::{
    DaemonConnection, DaemonTarget, FootprintSnapshot, MultibotSettings, SidebarMode,
    sample_self_footprint,
};
use multibot_gpu::{GpuRuntimeReport, read_gpu_report_file};
use multibot_proto::{
    AgentKind, ChatMessage, ClientRequest, MessageRole, SearchHit, ServerEvent, SessionSummary,
};
use multibot_updater::{
    AtomicSwapper, CheckOutcome, DEFAULT_POLL_INTERVAL, DefaultFileSwapper, DefaultProcessLauncher,
    FileSwapper, ProcessLauncher, UpdateClient, UpdateClientConfig, UpdateState, UpdaterPlatform,
    contains_forbidden_duration_wording, filter_preserved_cli_args, has_forbidden_control_or_bidi,
};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use sysinfo::System;
use tokio::sync::{broadcast, mpsc, oneshot};

static NEXT_VIEW_ID: AtomicU64 = AtomicU64::new(1);

actions!(
    multibot,
    [
        // Slice 1 (existing 19 actions)
        OpenPalette,
        OpenWorkspaces,
        OpenInbox,
        ToggleInbox,
        ToggleSettings,
        ToggleSidebar,
        ToggleFullscreen,
        NewWindow,
        OpenShortcuts,
        CloseOverlay,
        VoiceDictation,
        NextSession,
        PrevSession,
        JumpSession1,
        JumpSession2,
        JumpSession3,
        JumpSession4,
        JumpSession5,
        JumpSession6,
        JumpSession7,
        JumpSession8,
        JumpSession9,
        // Slice 3 — Session lifecycle & sidebar structure (8 actions)
        NewSession,
        DeleteSession,
        ArchiveSession,
        ReopenClosedSession,
        ReconnectSession,
        MoveSessionUp,
        MoveSessionDown,
        ToggleGroup,
        // Slice 3 — Inbox & session triage (6 actions)
        InboxNext,
        InboxPrev,
        ToggleUnreadOnly,
        MarkSessionUnread,
        MoveSessionWaiting,
        RemindSession,
        UndoWaiting,
        OpenDone,
        OpenHibernated,
        OpenDissolved,
        ToggleHibernateMode,
        // Slice 3 — Workspaces & views (16 actions)
        ToggleFavorite,
        SnoozeWorkspace,
        JumpWorkspace1,
        JumpWorkspace2,
        JumpWorkspace3,
        JumpWorkspace4,
        JumpWorkspace5,
        JumpWorkspace6,
        JumpWorkspace7,
        JumpWorkspace8,
        JumpWorkspace9,
        PrevWorkspace,
        NextWorkspace,
        WorkspaceViewAll,
        WorkspaceViewFrequent,
        WorkspaceViewFavorites,
        // Slice 3 — System inline rename (1 action)
        RenameActiveSession,
        // Slice 4 — Rich Chat View & font zoom (6 actions)
        CopyLatestCodeBlock,
        FindInChat,
        BrowseCommands,
        FontZoomIn,
        FontZoomOut,
        FontZoomReset,
        // Inbox split cycling (scoped to "Inbox" context)
        InboxNextSplit,
        InboxPrevSplit,
        // Slice 5 — built-in native terminal pane (`Alt+C`)
        ToggleTerminal,
        ToggleBrowserPane,
        FocusBrowserPane,
        CaptureBrowserPane,
    ]
);

/// Production `UpdaterPlatform` implementation that preserves explicit CLI flags (e.g. `--gpu=...`).
struct DesktopUpdaterPlatform {
    updates_dir: PathBuf,
    preserved_args: Vec<String>,
    swapper: DefaultFileSwapper,
    launcher: DefaultProcessLauncher,
}

impl UpdaterPlatform for DesktopUpdaterPlatform {
    fn current_exe(&self) -> anyhow::Result<PathBuf> {
        std::env::current_exe().map_err(Into::into)
    }

    fn updates_dir(&self) -> PathBuf {
        self.updates_dir.clone()
    }

    fn preserved_cli_args(&self) -> Vec<String> {
        filter_preserved_cli_args(&self.preserved_args)
    }

    fn file_swapper(&self) -> &dyn FileSwapper {
        &self.swapper
    }

    fn process_launcher(&self) -> &dyn ProcessLauncher {
        &self.launcher
    }
}

const GENERIC_UPDATE_ERROR_COPY: &str =
    "Could not complete update. Check your connection and try again.";

/// Sessions the desktop asks for at connect time. The daemon returns only chats something
/// still holds open, so this is headroom for a lot of live work, not the payload size.
const SESSION_LIST_LIMIT: usize = 500;

/// Detects millisecond/timeout or compact duration tokens (e.g. `15000ms`, `500 ms`, `30s`,
/// `timed out`, `timeout`) that must never appear in user-facing update UI copy.
fn has_millisecond_or_timeout_pattern(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if lower.contains("timed out") || lower.contains("timeout") {
        return true;
    }
    let bytes = lower.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            let mut u = j;
            while u < bytes.len() && bytes[u].is_ascii_whitespace() {
                u += 1;
            }
            let rest = &lower[u..];
            let unit_len =
                if rest.starts_with("ms") || rest.starts_with("us") || rest.starts_with("ns") {
                    2
                } else if rest.starts_with("sec") || rest.starts_with("min") {
                    3
                } else if u == j
                    && (rest.starts_with('s') || rest.starts_with('m') || rest.starts_with('h'))
                {
                    1
                } else {
                    0
                };
            if unit_len > 0 {
                let after = u + unit_len;
                if after >= bytes.len() || !bytes[after].is_ascii_alphanumeric() {
                    return true;
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    false
}

/// Defense-in-depth sanitizer guaranteeing no updater error copy ever contains forbidden
/// duration/time wording, millisecond/timeout tokens, or control/bidi characters — even after
/// truncating to 180 characters.
pub(crate) fn sanitize_update_error_copy(err: &anyhow::Error) -> String {
    let raw = err.to_string();
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || has_forbidden_control_or_bidi(trimmed)
        || contains_forbidden_duration_wording(trimmed)
        || has_millisecond_or_timeout_pattern(trimmed)
    {
        return GENERIC_UPDATE_ERROR_COPY.to_string();
    }
    let truncated: String = trimmed.chars().take(180).collect();
    if contains_forbidden_duration_wording(&truncated)
        || has_millisecond_or_timeout_pattern(&truncated)
    {
        GENERIC_UPDATE_ERROR_COPY.to_string()
    } else {
        truncated
    }
}

/// Lossless parser for `ChatOutbox::session_key` (`"{host_len}:{host}|{agent}|{sid_len}:{sid}"`).
pub(crate) fn parse_outbox_session_key(key: &str) -> Option<(&str, AgentKind, &str)> {
    let (host_len_str, rest) = key.split_once(':')?;
    let host_len: usize = host_len_str.parse().ok()?;
    let host = rest.get(..host_len)?;
    let after_host = rest.get(host_len..)?.strip_prefix('|')?;
    let (agent_str, after_agent) = after_host.split_once('|')?;
    let agent = match agent_str {
        "claude" => AgentKind::Claude,
        "antigravity" => AgentKind::Antigravity,
        "codex" => AgentKind::Codex,
        "shell" => AgentKind::Shell,
        "pi" => AgentKind::Pi,
        "cursor" => AgentKind::Cursor,
        _ => return None,
    };
    let (sid_len_str, sid) = after_agent.split_once(':')?;
    let sid_len: usize = sid_len_str.parse().ok()?;
    if sid.len() != sid_len {
        return None;
    }
    Some((host, agent, sid))
}

/// Compact fingerprint of all conditional UI subtrees that can mount/unmount focusable
/// elements (including opaque `Button` internal `FocusHandle`s). When this signature changes
/// between rendered frames while focus is not on a currently-mounted known handle,
/// `MultibotView::render` restores focus via `focus_active_surface_default`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FocusLayoutSignature {
    main_surface: MainSurface,
    active_modal: ActiveModal,
    sidebar_mode: SidebarMode,
    table_modal_open: bool,
    find_open: bool,
    update_card_kind: u8,
    selected_session_id: Option<String>,
    slash_picker_open: bool,
    rename_active: bool,
    sidebar_workspace_draft_active: bool,
    reply_quote_active: bool,
    has_pending_attachments: bool,
    message_action_menu_open: bool,
    focused_message_idx: Option<usize>,
    message_count: usize,
    answered_question_count: usize,
    gpu_adapter_count: usize,
    terminal_open: bool,
}

pub struct MultibotView {
    pub(crate) runtime: Arc<tokio::runtime::Runtime>,
    pub(crate) root_focus_handle: FocusHandle,
    pub(crate) last_focus_layout_sig: Option<FocusLayoutSignature>,
    pub(crate) settings: MultibotSettings,
    pub(crate) gpu_report: GpuRuntimeReport,
    pub(crate) footprint: FootprintSnapshot,
    pub(crate) footprint_label: String,

    // Navigation & overlays
    pub(crate) main_surface: MainSurface,
    pub(crate) settings_tab: crate::settings::SettingsTab,
    pub(crate) settings_focus_handle: FocusHandle,
    pub(crate) active_modal: ActiveModal,
    pub(crate) sidebar_state: SidebarState,
    pub(crate) inbox_state: InboxState,
    pub(crate) palette_state: PaletteState,
    pub(crate) chat_state: ChatViewState,
    pub(crate) update_ui: UpdateUiState,
    pub(crate) slash_picker_state: SlashPickerState,
    pub(crate) table_modal_state: TableModalState,
    /// Built-in native GPU terminal pane (`Alt+C`). Owns no PTY while closed.
    pub(crate) terminal: TerminalPaneState,
    #[cfg(windows)]
    pub(crate) browser: crate::browser_pane::DesktopBrowserPane,
    pub(crate) shell_host: crate::shell_host::ShellHost,
    /// Lazy-loaded Computer View pane (MBRU-7). Owns no webview while closed.
    pub(crate) computer: ComputerViewState,
    /// Last Computer View URL this window connected to, offered again on reopen. A
    /// remembered address never starts a browser on its own.
    pub(crate) last_computer_url: Option<crate::computer_view::SafeUrl>,

    // Daemon & chat state
    pub(crate) connected_host: Option<String>,
    pub(crate) connection_status: String,
    pub(crate) daemon_tx: Option<mpsc::UnboundedSender<ClientRequest>>,
    pub(crate) sessions: Vec<SessionSummary>,
    pub(crate) host_sessions: Vec<(String, SessionSummary)>,
    pub(crate) host_txs: HashMap<String, mpsc::UnboundedSender<ClientRequest>>,
    pub(crate) host_statuses: Vec<HostStatusSnapshot>,
    pub(crate) selected_session_host: Option<String>,
    pub(crate) pending_spawn_host: Option<String>,
    pub(crate) selected_session: Option<SessionSummary>,
    pub(crate) messages: Vec<ChatMessage>,

    // Session lifecycle & multi-window SharedAppStore sync
    pub(crate) view_id: u64,
    pub(crate) closed_sessions: Vec<(String, SessionSummary)>,
    pub(crate) last_synced_revision: u64,
    pub(crate) last_synced_connection_gen: u64,
    pub(crate) last_daemon_notice_seq: u64,
    pub(crate) pending_spawn_open: bool,
    pub(crate) _shared_store_sub: Subscription,
    pub(crate) _prompt_focus_sub: Subscription,
    _prompt_change_sub: Subscription,
    _window_activation_sub: Subscription,
    pub(crate) _host_focus_sub: Subscription,
    pub(crate) _daemon_event_task: Task<()>,

    // Updater runtime
    pub(crate) update_action_tx: Option<mpsc::UnboundedSender<UpdateUiAction>>,
    pub(crate) preserved_cli_args: Vec<String>,
    pub(crate) update_in_flight: bool,
    pub(crate) last_update_notice_seq: u64,

    // Daemon metadata synced from App-scoped SharedAppStore
    pub(crate) daemon_version: Option<String>,
    pub(crate) daemon_binary_hash: Option<String>,

    // M5 two-tier cache & offline outbox
    pub(crate) chat_cache: ChatCache,
    pub(crate) active_switch_token: Option<SwitchToken>,
    pub(crate) pending_switch_tokens: HashMap<String, SwitchToken>,
    pub(crate) pending_older_page_before: HashMap<String, u64>,
    pub(crate) shared_chat_outbox: Arc<Mutex<SharedChatOutbox>>,
    pub(crate) chat_outbox: ChatOutbox,
    pub(crate) persist_outbox_to_disk: bool,
    pub(crate) outbox_msg_map: HashMap<String, String>,
    pub(crate) last_dispatched_prompt: Option<(String, String)>,

    // M3 remote whole-history search state
    pub(crate) next_search_request_seq: u64,
    pub(crate) remote_search_request_id: Option<String>,
    pub(crate) remote_search_hits: Vec<SearchHit>,
    pub(crate) remote_search_truncated: bool,

    pub(crate) voice: crate::voice::VoiceState,
    pub(crate) deepgram_key_input: Entity<InputState>,
    pub(crate) deepgram_language_input: Entity<InputState>,

    // Inputs
    pub(crate) prompt_input: Entity<TextareaState>,
    pub(crate) pending_focus_input: bool,
    pub(crate) shortcuts_search_input: Entity<InputState>,
    pub(crate) _shortcuts_search_sub: Subscription,
    pub(crate) host_input: Entity<InputState>,
    pub(crate) palette_input: Entity<InputState>,
    pub(crate) notice: Option<String>,
}

fn backfill_zigshel_workspaces(store: &mut WorkspaceStore, dir: &std::path::Path) -> bool {
    if !dir.exists() {
        return false;
    }
    let mut bundle = import_zigshel_bundle_readonly(Some(dir));
    if store
        .session_meta
        .values()
        .any(|m| m.workspace_id.is_some())
    {
        // Existing installs already imported chats, but older builds misclassified tmux tabs.
        let is_shell = |key: &str| {
            multibot_core::workspaces::parse_session_key(key)
                .is_some_and(|(_, agent, _)| agent == "shell")
        };
        bundle.sessions.retain(|s| {
            is_shell(&s.session_key) && !store.session_titles.contains_key(&s.session_key)
        });
        bundle.favorites.retain(|s| {
            bundle
                .sessions
                .iter()
                .any(|session| session.session_key == s.session_key)
        });
        if bundle.sessions.is_empty() {
            return false;
        }
    }
    let stats = merge_zigshel_bundle(store, &bundle);
    stats.sessions_assigned > 0 || stats.favorites_added > 0 || stats.workspaces_added > 0
}

impl MultibotView {
    pub fn new(
        runtime: Arc<tokio::runtime::Runtime>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        if !cx.has_global::<SharedAppStore>() {
            let loaded = MultibotSettings::load();
            SharedAppStore::init(cx, loaded);
        }

        let view_id = NEXT_VIEW_ID.fetch_add(1, Ordering::Relaxed);
        let (
            settings,
            sessions,
            workspace_store,
            palette_stats,
            unread_only,
            connected_host,
            connection_status,
            daemon_tx,
            mut daemon_event_rx,
            shared_chat_outbox,
            connection_gen,
            daemon_version,
            daemon_binary_hash,
            daemon_notice_seq,
            preserved_cli_args,
            update_action_tx,
            update_in_flight,
            update_last_release,
            update_state,
            update_notice_seq,
            persist_to_disk,
            revision,
        ) = {
            let store = cx.global_mut::<SharedAppStore>();
            if store.persist_to_disk
                && store.window_count == 0
                && backfill_zigshel_workspaces(&mut store.workspace_store, &legacy_zigshel_dir())
            {
                let _ = store.workspace_store.save();
            }
            store.window_count = store.window_count.saturating_add(1);
            (
                store.settings.clone(),
                store.sessions.clone(),
                store.workspace_store.clone(),
                store.palette_stats.clone(),
                store.unread_only,
                store.connected_host.clone(),
                store.connection_status.clone(),
                store.daemon_tx.clone(),
                store.daemon_event_tx.subscribe(),
                store.shared_chat_outbox.clone(),
                store.connection_gen,
                store.daemon_version.clone(),
                store.daemon_binary_hash.clone(),
                store.daemon_notice_seq,
                store.preserved_cli_args.clone(),
                store.update_action_tx.clone(),
                store.update_in_flight,
                store.update_last_release.clone(),
                store.update_state.clone(),
                store.update_notice_seq,
                store.persist_to_disk,
                store.revision,
            )
        };
        let initial_chat_outbox = shared_chat_outbox
            .lock()
            .map(|s| s.outbox.clone())
            .unwrap_or_default();

        let gpu_report =
            read_gpu_report_file(&MultibotSettings::gpu_report_path()).unwrap_or_default();

        let mut sys = System::new();
        let footprint = sample_self_footprint(&mut sys);
        let footprint_label = format_footprint(&footprint);
        let mut last_footprint_label = footprint_label.clone();

        let prompt_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Message this session · Ctrl+Enter to send")
                .auto_grow(1, 10)
                .soft_wrap(true)
        });
        let shortcuts_search_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search shortcuts..."));
        let shortcuts_search_sub = cx.subscribe(&shortcuts_search_input, |_, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let host_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("user@vps-host (or user@host:22)"));
        let palette_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search sessions and commands..."));

        let deepgram_key_input = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder("Deepgram API key");
            input.set_value(settings.voice.api_key.clone(), window, cx);
            input.set_masked(true, window, cx);
            input
        });
        let deepgram_language_input = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder("en, de, en-US…");
            input.set_value(settings.voice.language.clone(), window, cx);
            input
        });

        let root_focus_handle = cx.focus_handle();
        window.focus(&root_focus_handle, cx);

        let mut sidebar_state = SidebarState::default();
        sidebar_state.mode = settings.sidebar_mode;
        sidebar_state.active_workspace_id =
            multibot_core::workspaces::ALL_WORKSPACES_ID.to_string();
        sidebar_state.recents_collapsed = workspace_store.recents_section_collapsed;
        sidebar_state.unread_only = unread_only;
        sidebar_state.workspace_store = workspace_store;
        sidebar_state.persist_to_disk = persist_to_disk;

        let mut palette_state = PaletteState::default();
        palette_state.stats = palette_stats;
        palette_state.persist_to_disk = persist_to_disk;

        let mut chat_state = ChatViewState::default();
        chat_state.font_size_pt = settings
            .chat_font_size_pt
            .clamp(MIN_CHAT_FONT_SIZE_PT, MAX_CHAT_FONT_SIZE_PT);
        chat_state.focus_handle = Some(cx.focus_handle());

        let mut update_ui = UpdateUiState::default();
        update_ui.last_release = update_last_release;
        update_ui.set_update_state(update_state);

        // Observe SharedAppStore across OS windows (Ctrl+N) without triggering idle redraws.
        let mut reminder_wake_seq = cx.global::<SharedAppStore>().reminder_wake_seq;
        let mut account_status = cx.global::<SharedAppStore>().account.status.clone();
        let _shared_store_sub = cx.observe_global::<SharedAppStore>(move |this, cx| {
            this.sync_from_shared_store(cx);
            let next_status = &cx.global::<SharedAppStore>().account.status;
            if account_status != *next_status {
                account_status = next_status.clone();
                cx.notify();
            }
            let next = cx.global::<SharedAppStore>().reminder_wake_seq;
            if reminder_wake_seq != next {
                reminder_wake_seq = next;
                cx.notify();
            }
        });
        let _window_activation_sub = cx.observe_window_activation(window, |this, window, cx| {
            this.restore_embedded_shell_focus(window, cx);
        });
        let _prompt_change_sub = cx.subscribe(&prompt_input, |this, input, event, cx| {
            if matches!(event, gpui_kit::component::input::InputEvent::Change)
                && this.main_surface == MainSurface::Chat
                && !input.read(cx).value().is_empty()
            {
                this.follow_chat_from_composer(cx);
            }
        });
        let prompt_fh = prompt_input.read(cx).focus_handle(cx);
        let _prompt_focus_sub = cx.on_focus(&prompt_fh, window, |this, _window, cx| {
            if this.sidebar_state.rename.is_some()
                || this.sidebar_state.new_workspace_draft.is_some()
            {
                this.sidebar_state.rename = None;
                this.sidebar_state.new_workspace_draft = None;
                cx.notify();
            }
        });
        let host_fh = host_input.read(cx).focus_handle(cx);
        let _host_focus_sub = cx.on_focus(&host_fh, window, |this, _window, cx| {
            if this.sidebar_state.rename.is_some()
                || this.sidebar_state.new_workspace_draft.is_some()
            {
                this.sidebar_state.rename = None;
                this.sidebar_state.new_workspace_draft = None;
                cx.notify();
            }
        });

        // Subscribe to App-scoped daemon ServerEvent broadcast (auto-cancelled on view drop -> zero leaks).
        let _daemon_event_task = cx.spawn(async move |this, cx| {
            loop {
                match daemon_event_rx.recv().await {
                    Ok(DaemonBroadcastEvent {
                        origin_view_id,
                        host_label,
                        is_older_page_hint,
                        event,
                    }) => {
                        if origin_view_id == view_id {
                            continue;
                        }
                        let res = this.update(cx, |view, cx| {
                            view.handle_daemon_server_event_with_hint(
                                &host_label,
                                event,
                                is_older_page_hint,
                                cx,
                            );
                        });
                        if res.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        // Decrement window_count cleanly when this window/view releases (no revision bump).
        // Only when the LAST window closes (`window_count == 0`) do we tear down the shared
        // App-scoped updater worker AND App-scoped daemon connection (`connection_shutdown_tx`
        // + `daemon_tx`) so closing Window 1 while Window 2 stays open keeps the connection live.
        cx.on_release(|_view, cx| {
            if cx.has_global::<SharedAppStore>() {
                let store = cx.global_mut::<SharedAppStore>();
                store.window_count = store.window_count.saturating_sub(1);
                if store.window_count == 0 {
                    store.update_action_tx = None;
                    store.update_poller_started = false;
                    store.update_in_flight = false;
                    store.host_connections.clear();
                    store.rebuild_merged_sessions();
                    store.connection_shutdown_tx = None;
                    store.daemon_tx = None;
                    store.pending_older_page_before.clear();
                }
            }
        })
        .detach();

        // Lightweight 5-second single-PID footprint refresh (zero GPU/CPU spin).
        // Hard Invariant #4: only update & call cx.notify() when formatted RAM/CPU text changes.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(5)).await;
                if this.upgrade().is_none() {
                    break;
                }
                let snap = sample_self_footprint(&mut sys);
                let next_label = format_footprint(&snap);
                if next_label == last_footprint_label {
                    continue;
                }
                last_footprint_label = next_label;
                let ok = this.update(cx, |view, cx| {
                    if update_footprint_if_changed(
                        &mut view.footprint,
                        &mut view.footprint_label,
                        snap,
                    ) {
                        cx.notify();
                    }
                });
                if ok.is_err() {
                    break;
                }
            }
        })
        .detach();

        SharedAppStore::schedule_reminder_timer(cx);

        let mut view = Self {
            runtime,
            root_focus_handle,
            last_focus_layout_sig: None,
            settings,
            gpu_report,
            footprint,
            footprint_label,
            main_surface: MainSurface::Chat,
            settings_tab: crate::settings::SettingsTab::default(),
            settings_focus_handle: cx.focus_handle(),
            active_modal: ActiveModal::None,
            sidebar_state,
            inbox_state: InboxState::default(),
            palette_state,
            chat_state,
            update_ui,
            slash_picker_state: SlashPickerState::default(),
            table_modal_state: TableModalState::default(),
            terminal: TerminalPaneState::default(),
            #[cfg(windows)]
            browser: Default::default(),
            shell_host: crate::shell_host::ShellHost::default(),
            computer: ComputerViewState::default(),
            last_computer_url: None,
            connected_host,
            connection_status,
            daemon_tx,
            sessions,
            host_sessions: cx.global::<SharedAppStore>().host_sessions.clone(),
            host_txs: cx
                .global::<SharedAppStore>()
                .host_connections
                .iter()
                .filter_map(|(h, c)| c.daemon_tx.clone().map(|tx| (h.clone(), tx)))
                .collect(),
            host_statuses: cx.global::<SharedAppStore>().host_status_snapshots(),
            selected_session_host: None,
            pending_spawn_host: None,
            selected_session: None,
            messages: Vec::new(),
            view_id,
            closed_sessions: Vec::new(),
            last_synced_revision: revision,
            last_synced_connection_gen: connection_gen,
            last_daemon_notice_seq: daemon_notice_seq,
            pending_spawn_open: false,
            _shared_store_sub,
            _prompt_focus_sub,
            _prompt_change_sub,
            _window_activation_sub,
            _host_focus_sub,
            _daemon_event_task,
            update_action_tx,
            preserved_cli_args,
            update_in_flight,
            last_update_notice_seq: update_notice_seq,
            daemon_version,
            daemon_binary_hash,
            chat_cache: ChatCache::default(),
            active_switch_token: None,
            pending_switch_tokens: HashMap::new(),
            pending_older_page_before: HashMap::new(),
            shared_chat_outbox,
            chat_outbox: initial_chat_outbox,
            persist_outbox_to_disk: false,
            outbox_msg_map: HashMap::new(),
            last_dispatched_prompt: None,
            next_search_request_seq: 0,
            remote_search_request_id: None,
            remote_search_hits: Vec::new(),
            remote_search_truncated: false,
            voice: crate::voice::VoiceState::default(),
            deepgram_key_input,
            deepgram_language_input,
            prompt_input,
            pending_focus_input: false,
            shortcuts_search_input,
            _shortcuts_search_sub: shortcuts_search_sub,
            host_input,
            palette_input,
            notice: None,
        };
        view.start_account_once(cx);
        view
    }

    /// Enables disk persistence for Tier-2 chat cache and offline outbox (production `main.rs`).
    /// Loads the shared App-scoped `ChatOutbox` from disk ONCE so `Ctrl+N` sibling windows share
    /// the same queue without reloading or double-flushing entries on `HelloAck`.
    pub(crate) fn enable_disk_persistence(&mut self) {
        self.chat_cache.dir = Some(MultibotSettings::chat_cache_dir());
        self.persist_outbox_to_disk = true;
        if let Ok(mut shared) = self.shared_chat_outbox.lock() {
            if !shared.loaded_from_disk {
                let disk_outbox = ChatOutbox::load();
                for entry in disk_outbox.entries {
                    let _ = shared.outbox.enqueue_entry(entry);
                }
                shared.loaded_from_disk = true;
            }
            shared.persist_to_disk = true;
            self.chat_outbox = shared.outbox.clone();
        }
    }

    /// True only when disk persistence is explicitly enabled (production builds), ensuring unit
    /// tests never touch real `~/.config/Multibot/settings.json`.
    pub(crate) fn is_disk_persistence_enabled(&self, cx: &App) -> bool {
        self.sidebar_state.persist_to_disk
            || cx
                .try_global::<SharedAppStore>()
                .is_some_and(|s| s.persist_to_disk)
    }

    // =========================================================================
    // Cross-Window SharedAppStore Synchronization
    // =========================================================================

    pub(crate) fn sync_settings_to_shared(&mut self, cx: &mut Context<Self>) {
        if cx.has_global::<SharedAppStore>() {
            let rev = {
                let store = cx.global_mut::<SharedAppStore>();
                store.settings = self.settings.clone();
                store.bump_revision()
            };
            self.last_synced_revision = rev;
        }
    }

    pub(crate) fn sync_sessions_to_shared(&mut self, cx: &mut Context<Self>) {
        if cx.has_global::<SharedAppStore>() {
            let rev = {
                let store = cx.global_mut::<SharedAppStore>();
                if !store.host_connections.is_empty() && !self.host_statuses.is_empty() {
                    for (host, entry) in &mut store.host_connections {
                        entry.sessions = self
                            .host_sessions
                            .iter()
                            .filter(|(h, _)| h == host)
                            .map(|(_, s)| s.clone())
                            .collect();
                    }
                    store.rebuild_merged_sessions();
                    store.refresh_connection_summary();
                    self.sessions = store.sessions.clone();
                    self.host_statuses = store.host_status_snapshots();
                    self.connection_status = store.connection_status.clone();
                    store.bump_revision()
                } else {
                    store.sessions = self.sessions.clone();
                    store.connected_host = self.connected_host.clone();
                    store.connection_status = self.connection_status.clone();
                    store.daemon_tx = self.daemon_tx.clone();
                    store.daemon_version = self.daemon_version.clone();
                    store.daemon_binary_hash = self.daemon_binary_hash.clone();
                    store.bump_revision()
                }
            };
            self.last_synced_revision = rev;
        }
    }

    pub(crate) fn sync_workspace_to_shared(&mut self, cx: &mut Context<Self>) {
        if cx.has_global::<SharedAppStore>() {
            let rev = {
                let store = cx.global_mut::<SharedAppStore>();
                store.workspace_store = self.sidebar_state.workspace_store.clone();
                store.palette_stats = self.palette_state.stats.clone();
                store.unread_only = self.sidebar_state.unread_only;
                store.bump_revision()
            };
            self.last_synced_revision = rev;
            SharedAppStore::schedule_reminder_timer(cx);
        }
    }

    pub(crate) fn sync_update_state_to_shared(&mut self, cx: &mut Context<Self>) {
        if cx.has_global::<SharedAppStore>() {
            let rev = {
                let store = cx.global_mut::<SharedAppStore>();
                store.update_state = self.update_ui.state.clone();
                store.update_last_release = self.update_ui.last_release.clone();
                store.update_in_flight = self.update_in_flight;
                store.bump_revision()
            };
            self.last_synced_revision = rev;
        }
    }

    pub(crate) fn sync_from_shared_store(&mut self, cx: &mut Context<Self>) {
        if !cx.has_global::<SharedAppStore>() {
            return;
        }
        let mut changed = false;

        let (
            rev,
            settings_diff,
            next_settings,
            update_in_flight,
            update_action_tx,
            update_last_release,
            update_state,
        ) = {
            let store = cx.global::<SharedAppStore>();
            (
                store.revision,
                store.settings != self.settings,
                store.settings.clone(),
                store.update_in_flight,
                store.update_action_tx.clone(),
                store.update_last_release.clone(),
                store.update_state.clone(),
            )
        };

        self.update_in_flight = update_in_flight;
        if !update_in_flight {
            self.update_ui.pending_action = None;
        }
        if update_action_tx.is_some() {
            self.update_action_tx = update_action_tx;
        }

        if settings_diff {
            let mut next_settings = next_settings;
            if self.selected_session.is_some() {
                next_settings.shell_fullscreen = self.settings.shell_fullscreen;
            }
            let visible_changed = !settings_visible_eq(&self.settings, &next_settings);
            self.settings = next_settings;
            if visible_changed {
                self.sidebar_state.set_mode(if self.settings.shell_fullscreen {
                    SidebarMode::Hidden
                } else { self.settings.sidebar_mode });
                self.cleanup_unmounted_surface_state();
                self.chat_state.font_size_pt = self
                    .settings
                    .chat_font_size_pt
                    .clamp(MIN_CHAT_FONT_SIZE_PT, MAX_CHAT_FONT_SIZE_PT);
                changed = true;
            }
        }

        // Always sync update_ui state/release from App-scoped SharedAppStore even when
        // store.revision did not bump (e.g. background UpdateAvailable while sidebar is
        // Collapsed/Hidden), while gating cx.notify() on actual visible UI impact so
        // Collapsed/Hidden sidebars incur ZERO idle redraws (Hard Invariant #4).
        if update_last_release.is_some() && self.update_ui.last_release != update_last_release {
            self.update_ui.last_release = update_last_release;
        }
        let was_card_visible = self.update_ui.is_card_visible(
            self.settings.sidebar_mode,
            self.sidebar_state.is_shown(),
            self.settings.dismissed_update_version.as_deref(),
        );
        let update_state_changed = self.update_ui.set_update_state(update_state);
        let now_card_visible = self.update_ui.is_card_visible(
            self.settings.sidebar_mode,
            self.sidebar_state.is_shown(),
            self.settings.dismissed_update_version.as_deref(),
        );
        if update_state_changed
            && (was_card_visible || now_card_visible || self.active_modal == ActiveModal::Palette)
        {
            changed = true;
        }

        if rev != self.last_synced_revision {
            let store = cx.global::<SharedAppStore>();
            if self.host_sessions != store.host_sessions
                || self.host_statuses != store.host_status_snapshots()
            {
                changed = true;
            }
            self.host_sessions = store.host_sessions.clone();
            self.host_statuses = store.host_status_snapshots();
            self.host_txs = store
                .host_connections
                .iter()
                .filter_map(|(host, entry)| entry.daemon_tx.clone().map(|tx| (host.clone(), tx)))
                .collect();
            let (
                sessions,
                workspace_store,
                palette_stats,
                unread_only,
                connected_host,
                connection_status,
                daemon_tx,
                connection_gen,
                daemon_version,
                daemon_binary_hash,
                daemon_notice,
                daemon_notice_seq,
                update_notice,
                update_notice_seq,
            ) = {
                let store = cx.global::<SharedAppStore>();
                (
                    store.sessions.clone(),
                    store.workspace_store.clone(),
                    store.palette_stats.clone(),
                    store.unread_only,
                    store.connected_host.clone(),
                    store.connection_status.clone(),
                    store.daemon_tx.clone(),
                    store.connection_gen,
                    store.daemon_version.clone(),
                    store.daemon_binary_hash.clone(),
                    store.daemon_notice.clone(),
                    store.daemon_notice_seq,
                    store.update_notice.clone(),
                    store.update_notice_seq,
                )
            };

            if connection_gen != self.last_synced_connection_gen
                && cx.global::<SharedAppStore>().host_connections.is_empty()
            {
                self.last_synced_connection_gen = connection_gen;
                self.clear_active_session_view();
                self.pending_switch_tokens.clear();
                self.pending_older_page_before.clear();
                self.pending_spawn_open = false;
                self.notice = None;
                changed = true;
            }
            if self.sessions != sessions {
                self.sessions = sessions;
                if let Some(ref sel) = self.selected_session.clone()
                    && let Some((_, fresh)) = self.host_for_session_key(&canonical_session_key(
                        self.active_session_host(),
                        sel.agent,
                        &sel.id,
                    ))
                {
                    self.selected_session = Some(fresh);
                }
                changed = true;
            }
            if self.sidebar_state.workspace_store != workspace_store {
                self.sidebar_state.recents_collapsed = workspace_store.recents_section_collapsed;
                self.sidebar_state.workspace_store = workspace_store;
                if let Some(session) = &self.selected_session {
                    let full_width = self.sidebar_state.workspace_store.session_meta
                        .get(&self.session_key_for(session)).is_some_and(|meta| meta.full_width);
                    if full_width != self.settings.shell_fullscreen {
                        self.apply_session_full_width(full_width);
                    }
                }
                changed = true;
            }
            if self.palette_state.stats != palette_stats {
                self.palette_state.stats = palette_stats;
                changed = true;
            }
            if self.sidebar_state.unread_only != unread_only {
                self.sidebar_state.unread_only = unread_only;
                changed = true;
            }
            if self.connected_host != connected_host {
                self.connected_host = connected_host;
                changed = true;
            }
            if self.connection_status != connection_status {
                self.connection_status = connection_status;
                changed = true;
            }
            if self.daemon_tx.is_some() != daemon_tx.is_some() {
                self.daemon_tx = daemon_tx;
                changed = true;
            } else if daemon_tx.is_some() {
                self.daemon_tx = daemon_tx;
            }
            if self.daemon_version != daemon_version {
                self.daemon_version = daemon_version;
                changed = true;
            }
            if self.daemon_binary_hash != daemon_binary_hash {
                self.daemon_binary_hash = daemon_binary_hash;
                changed = true;
            }
            if daemon_notice_seq != self.last_daemon_notice_seq {
                self.last_daemon_notice_seq = daemon_notice_seq;
                if let Some(msg) = daemon_notice {
                    self.notice = Some(msg);
                    changed = true;
                }
            }
            if update_notice_seq != self.last_update_notice_seq {
                self.last_update_notice_seq = update_notice_seq;
                if let Some(msg) = update_notice {
                    self.notice = Some(msg);
                    changed = true;
                }
            }

            self.last_synced_revision = rev;
        }

        if self.chat_state.pending_harness_switch.is_some()
            && self.active_session_tx().is_none_or(|tx| tx.is_closed())
        {
            self.chat_state.pending_harness_switch = None;
        }
        if changed {
            self.cleanup_unmounted_surface_state();
            cx.notify();
        }
    }

    /// Returns `true` when any Chat-local transient overlay is mounted and active.
    pub(crate) fn has_mounted_chat_overlay(&self) -> bool {
        self.main_surface == MainSurface::Chat
            && (self.slash_picker_state.is_open()
                || self.chat_state.find_open
                || self.chat_state.message_action_menu_open
                || self.chat_state.focused_message_idx.is_some()
                || self.chat_state.reply_quote.is_some())
    }

    /// Returns `true` if `session_key` is currently rendered as a visible row or pinned tile
    /// inside `#sidebar-open` (accounting for active workspace/view, `unread_only`, collapsed
    /// pinned/group/recents sections, and existence as a non-archived session in `self.sessions`).
    pub(crate) fn is_session_rendered_in_open_sidebar(&self, session_key: &str) -> bool {
        if !self
            .find_session_by_key(session_key)
            .is_some_and(|s| !s.archived)
        {
            return false;
        }
        let now = now_unix_ms();
        let inputs = self.session_inputs();
        let active_key = self
            .selected_session
            .as_ref()
            .map(|s| self.session_key_for(s));
        let layout = self
            .sidebar_state
            .workspace_store
            .compute_sidebar_layout_with_options(
                &self.sidebar_state.active_workspace_id,
                self.sidebar_state.active_workspace_view,
                &inputs,
                self.sidebar_state.unread_only,
                active_key.as_deref(),
                now,
            );
        if layout
            .pinned_tiles
            .iter()
            .any(|r| r.session_key == session_key)
        {
            return true;
        }
        for gv in &layout.groups {
            if !gv.group.collapsed && gv.items.iter().any(|r| r.session_key == session_key) {
                return true;
            }
        }
        if layout.recents.iter().any(|r| r.session_key == session_key) {
            return true;
        }
        false
    }

    /// Returns `true` when the sidebar is actually drawn for the current surface.
    /// The Inbox is keyboard-first and minimal: it never renders the sidebar, while the
    /// user's `sidebar_mode` preference stays untouched and reappears when leaving Inbox.
    pub(crate) fn sidebar_rendered(&self) -> bool {
        self.sidebar_state.is_shown() && self.main_surface != MainSurface::Inbox
    }

    /// Returns `true` if `session_key` can mount an inline sidebar or top-pill rename input
    /// in the current surface and sidebar visibility state.
    pub(crate) fn can_mount_sidebar_rename(&self, session_key: &str) -> bool {
        if self.main_surface == MainSurface::Inbox {
            // The minimal Inbox never draws the sidebar, so no sidebar rename can mount there.
            return false;
        }
        match self.sidebar_state.mode {
            SidebarMode::Open => self.is_session_rendered_in_open_sidebar(session_key),
            SidebarMode::Collapsed | SidebarMode::Hidden => {
                self.main_surface == MainSurface::Chat
                    && self
                        .selected_session
                        .as_ref()
                        .is_some_and(|s| self.session_key_for(s) == session_key)
                    && self
                        .find_session_by_key(session_key)
                        .is_some_and(|s| !s.archived)
            }
        }
    }

    /// Returns `true` when `sidebar_state.rename` has a visibly mounted input:
    /// either on a rendered row/tile inside `#sidebar-open`, or in `#header-rename-input`
    /// for the currently selected non-archived session on `MainSurface::Chat` when the
    /// sidebar is Collapsed/Hidden.
    pub(crate) fn is_sidebar_rename_mounted(&self) -> bool {
        self.sidebar_state
            .rename
            .as_ref()
            .is_some_and(|r| self.can_mount_sidebar_rename(&r.session_key))
    }

    /// Returns `true` when `sidebar_state.new_workspace_draft` has a mounted input
    /// (only inside a rendered `#sidebar-open`).
    pub(crate) fn is_sidebar_workspace_draft_mounted(&self) -> bool {
        self.sidebar_state.new_workspace_draft.is_some()
            && self.sidebar_state.mode == SidebarMode::Open
            && self.sidebar_rendered()
    }

    /// Returns `true` when an Open-sidebar inline edit (rename or new-workspace draft) is
    /// currently mounted inside `#sidebar-open`.
    pub(crate) fn is_sidebar_inline_edit_mounted(&self) -> bool {
        self.sidebar_state.mode == SidebarMode::Open
            && self.sidebar_rendered()
            && (self.is_sidebar_rename_mounted() || self.is_sidebar_workspace_draft_mounted())
    }

    /// Returns `true` if `session_key` is currently rendered in the visible Inbox rows.
    pub(crate) fn is_session_rendered_in_inbox(&self, session_key: &str) -> bool {
        self.find_session_by_key(session_key)
            .is_some_and(|s| !s.archived)
            && self
                .compute_inbox_rows(now_unix_ms())
                .iter()
                .any(|row| row.session_key == session_key)
    }

    /// Returns `true` when `inbox_state.rename` has a visibly mounted input on a
    /// currently visible row inside `#inbox-root`.
    pub(crate) fn is_inbox_rename_mounted(&self) -> bool {
        self.main_surface == MainSurface::Inbox
            && self
                .inbox_state
                .rename
                .as_ref()
                .is_some_and(|r| self.is_session_rendered_in_inbox(&r.session_key))
    }

    /// Clears transient surface- or sidebar-local inline edit states whose UI subtree or
    /// target row is no longer visibly mounted.
    pub(crate) fn cleanup_unmounted_surface_state(&mut self) {
        if self.main_surface != MainSurface::Inbox {
            self.inbox_state.filter_focused = false;
            self.inbox_state.inbox_search_open = false;
            self.inbox_state.filter_query.clear();
        }
        if !self.is_inbox_rename_mounted() {
            self.inbox_state.rename = None;
        }
        if !self.is_sidebar_workspace_draft_mounted() {
            self.sidebar_state.new_workspace_draft = None;
        }
        if !self.is_sidebar_rename_mounted() {
            self.sidebar_state.rename = None;
        }
    }

    /// Transitions `self.main_surface` to `next` and clears any transient inline edit state
    /// that is no longer mounted or would lose focus across the surface transition.
    pub(crate) fn set_main_surface(&mut self, next: MainSurface) {
        self.chat_state.composer_menu_open = false;
        if self.main_surface != next {
            #[cfg(windows)]
            if next != MainSurface::Chat {
                self.hide_browser_pane();
            }
            self.voice.cancel();
            self.sidebar_state.rename = None;
            self.sidebar_state.new_workspace_draft = None;
            // Hard Invariant #4: the Computer View pane docks beside the chat, which the
            // Inbox does not render, so its backend and its browser processes must go with
            // the unmount.
            if next == MainSurface::Inbox && self.computer.is_open() {
                self.teardown_computer_view();
            }
            // Hard Invariant #4: the Inbox surface does not render the terminal pane, so
            // its PTY, child process and reader thread must go with the unmount.
            if next == MainSurface::Inbox && self.terminal.is_open() {
                if let Some(session) = self.terminal.session.take() {
                    session.request_reader_stop();
                    drop(session);
                }
                self.terminal.screen = None;
                self.terminal.error = None;
                self.terminal.wake_task = None;
                self.terminal.wake_tx = None;
                self.terminal.wake_rx = None;
            }
        }
        if next == MainSurface::Inbox && self.main_surface != MainSurface::Inbox {
            // Every fresh Inbox open starts clean: search bar hidden, no query.
            self.inbox_state.close_search();
        }
        if next == MainSurface::Inbox {
            // Never open the Inbox on a split that is hidden because it holds 0 sessions
            let splits = self.inbox_visible_splits(now_unix_ms());
            self.inbox_state.normalize_active_split(&splits);
        }
        self.main_surface = next;
        self.cleanup_unmounted_surface_state();
    }

    pub(crate) fn set_sidebar_mode_synced(&mut self, mode: SidebarMode, cx: &mut Context<Self>) {
        self.sidebar_state.set_mode(mode);
        self.cleanup_unmounted_surface_state();
        self.settings.sidebar_mode = mode;
        self.sync_settings_to_shared(cx);
        if self.sidebar_state.persist_to_disk {
            let _ = self.settings.save();
        }
        cx.notify();
    }

    pub(crate) fn toggle_shell_fullscreen(&mut self, cx: &mut Context<Self>) {
        if self.main_surface != MainSurface::Chat {
            return;
        }
        let Some(session) = self.selected_session.as_ref() else {
            return;
        };
        let key = self.session_key_for(session);
        let meta = self.sidebar_state.workspace_store.session_meta.entry(key).or_default();
        meta.full_width = !meta.full_width;
        let full_width = meta.full_width;
        self.apply_session_full_width(full_width);
        self.persist_workspace_store_if_enabled();
        self.sync_workspace_to_shared(cx);
        cx.notify();
    }

    fn apply_session_full_width(&mut self, full_width: bool) {
        self.settings.shell_fullscreen = full_width;
        self.sidebar_state.set_mode(if full_width {
            SidebarMode::Hidden
        } else if self.settings.sidebar_mode == SidebarMode::Hidden {
            SidebarMode::Open
        } else {
            self.settings.sidebar_mode
        });
        self.cleanup_unmounted_surface_state();
    }

    pub(crate) fn cycle_sidebar_mode_synced(&mut self, cx: &mut Context<Self>) {
        self.sidebar_state.cycle();
        let next = self.sidebar_state.mode;
        self.set_sidebar_mode_synced(next, cx);
    }

    // =========================================================================
    // Auto-Updater Background Runtime & Interactive Actions
    // =========================================================================

    pub(crate) fn dispatch_update_ui_action(
        &mut self,
        action: UpdateUiAction,
        cx: &mut Context<Self>,
    ) {
        let tx = self.update_action_tx.clone().or_else(|| {
            cx.try_global::<SharedAppStore>()
                .and_then(|s| s.update_action_tx.clone())
        });
        if let Some(tx) = tx {
            self.update_action_tx = Some(tx.clone());
            self.update_in_flight = true;
            if cx.has_global::<SharedAppStore>() {
                cx.global_mut::<SharedAppStore>().update_in_flight = true;
            }
            let _ = tx.send(action);
        }
    }

    pub(crate) fn start_update_runtime(&mut self, cx: &mut Context<Self>) {
        if cx.has_global::<SharedAppStore>() && cx.global::<SharedAppStore>().update_poller_started
        {
            self.update_action_tx = cx.global::<SharedAppStore>().update_action_tx.clone();
            return;
        }
        let Ok(config) = UpdateClientConfig::from_env(multibot_proto::build_version()) else {
            return;
        };
        let platform: Arc<dyn UpdaterPlatform> = Arc::new(DesktopUpdaterPlatform {
            updates_dir: MultibotSettings::updates_dir(),
            preserved_args: self.preserved_cli_args.clone(),
            swapper: DefaultFileSwapper,
            launcher: DefaultProcessLauncher,
        });
        self.start_update_runtime_with_config(
            config,
            DEFAULT_POLL_INTERVAL,
            Duration::from_secs(10),
            platform,
            cx,
        );
    }

    pub(crate) fn start_update_runtime_with_config(
        &mut self,
        config: UpdateClientConfig,
        poll_interval: Duration,
        initial_delay: Duration,
        platform: Arc<dyn UpdaterPlatform>,
        cx: &mut Context<Self>,
    ) {
        let (action_tx, mut action_rx) = mpsc::unbounded_channel::<UpdateUiAction>();
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<UpdateBridgeEvent>();

        self.update_action_tx = Some(action_tx.clone());
        if cx.has_global::<SharedAppStore>() {
            let store = cx.global_mut::<SharedAppStore>();
            store.update_action_tx = Some(action_tx);
            store.update_poller_started = true;
        }

        let client = Arc::new(UpdateClient::new(config));
        let updates_dir = platform.updates_dir();

        // 1. App-scoped GPUI bridge consumer (survives closing Window 1 while Window 2 is open;
        //    exits cleanly only when SharedAppStore.window_count == 0).
        //    Strictly demand-driven: zero redraws on 304/silent failures/etag-only updates.
        let app: &mut App = cx;
        app.spawn(async move |cx| {
            while let Some(ev) = ev_rx.recv().await {
                let alive = cx.update(|cx| {
                    if !cx.has_global::<SharedAppStore>()
                        || cx.global::<SharedAppStore>().window_count == 0
                    {
                        return false;
                    }
                    Self::apply_update_bridge_event_to_app(ev, cx);
                    true
                });
                if !alive {
                    break;
                }
            }
        })
        .detach();

        // 2. App-scoped demand-friendly 5-minute background poller with rate-limit backoff.
        let poll_client = client.clone();
        let poll_updates_dir = updates_dir.clone();
        let poll_ev_tx = ev_tx.clone();
        let poll_rt = self.runtime.clone();
        app.spawn(async move |cx| {
            if !initial_delay.is_zero() {
                cx.background_executor().timer(initial_delay).await;
            }
            loop {
                let state_opt = cx.update(|cx| {
                    if !cx.has_global::<SharedAppStore>() {
                        return None;
                    }
                    let store = cx.global::<SharedAppStore>();
                    if store.window_count == 0 {
                        return None;
                    }
                    Some((
                        store.settings.auto_check_updates,
                        store.settings.last_update_etag.clone(),
                        store.update_in_flight,
                    ))
                });
                let Some((auto_check, etag, in_flight)) = state_opt else {
                    break;
                };

                let mut next_sleep = poll_interval;
                if auto_check && !in_flight {
                    let c = poll_client.clone();
                    let dir = poll_updates_dir.clone();
                    let res = poll_rt
                        .spawn(async move {
                            tokio::task::spawn_blocking(move || {
                                c.check_for_update(etag.as_deref(), &dir)
                            })
                            .await
                        })
                        .await;
                    match res {
                        Ok(Ok(Ok(CheckOutcome::RateLimited { retry_after, etag }))) => {
                            next_sleep = retry_after.max(poll_interval);
                            let _ = poll_ev_tx.send(UpdateBridgeEvent::CheckCompleted {
                                outcome: CheckOutcome::RateLimited { retry_after, etag },
                                manual: false,
                            });
                        }
                        Ok(Ok(Ok(outcome))) => {
                            let _ = poll_ev_tx.send(UpdateBridgeEvent::CheckCompleted {
                                outcome,
                                manual: false,
                            });
                        }
                        Ok(Ok(Err(err))) => {
                            let _ = poll_ev_tx.send(UpdateBridgeEvent::CheckFailed {
                                message: sanitize_update_error_copy(&err),
                                manual: false,
                            });
                        }
                        _ => {}
                    }
                }
                cx.background_executor().timer(next_sleep).await;
            }
        })
        .detach();

        // 3. Tokio action worker for interactive Check / Download+Swap+Relaunch / Restart.
        let worker_client = client;
        let worker_updates_dir = updates_dir;
        let worker_ev_tx = ev_tx;
        let worker_platform = platform;
        self.runtime.spawn(async move {
            while let Some(action) = action_rx.recv().await {
                match action {
                    UpdateUiAction::CheckForUpdates | UpdateUiAction::RetryUpdate => {
                        let c = worker_client.clone();
                        let dir = worker_updates_dir.clone();
                        let res =
                            tokio::task::spawn_blocking(move || c.check_for_update(None, &dir))
                                .await;
                        match res {
                            Ok(Ok(outcome)) => {
                                let _ = worker_ev_tx.send(UpdateBridgeEvent::CheckCompleted {
                                    outcome,
                                    manual: true,
                                });
                            }
                            Ok(Err(err)) => {
                                let _ = worker_ev_tx.send(UpdateBridgeEvent::CheckFailed {
                                    message: sanitize_update_error_copy(&err),
                                    manual: true,
                                });
                            }
                            Err(_) => {
                                let _ = worker_ev_tx.send(UpdateBridgeEvent::CheckFailed {
                                    message: "Could not check for updates.".to_string(),
                                    manual: true,
                                });
                            }
                        }
                    }
                    UpdateUiAction::DownloadAndRestart(release) => {
                        let c = worker_client.clone();
                        let dir = worker_updates_dir.clone();
                        let plat = worker_platform.clone();
                        let tx_prog = worker_ev_tx.clone();
                        let res = tokio::task::spawn_blocking(move || {
                            let version = UpdateUiState::normalize_version(&release.version);
                            let mut last_pct: Option<u8> = None;
                            let mut last_reported_bytes: u64 = 0;
                            let staged =
                                c.download_and_stage_update(&release, &dir, |done, total| {
                                    let should_emit = match total {
                                        Some(t) if t > 0 => {
                                            let pct =
                                                ((done.saturating_mul(100)) / t).min(100) as u8;
                                            if last_pct != Some(pct) {
                                                last_pct = Some(pct);
                                                true
                                            } else {
                                                false
                                            }
                                        }
                                        _ => {
                                            if done.saturating_sub(last_reported_bytes)
                                                >= 256 * 1024
                                            {
                                                last_reported_bytes = done;
                                                true
                                            } else {
                                                false
                                            }
                                        }
                                    };
                                    if should_emit {
                                        let _ = tx_prog.send(UpdateBridgeEvent::DownloadProgress {
                                            version: version.clone(),
                                            bytes_done: done,
                                            bytes_total: total,
                                        });
                                        if total.is_some_and(|t| t > 0 && done >= t) {
                                            let _ = tx_prog.send(UpdateBridgeEvent::Verifying {
                                                version: version.clone(),
                                            });
                                        }
                                    }
                                })?;

                            let _ = tx_prog.send(UpdateBridgeEvent::StagedReady(staged.clone()));
                            AtomicSwapper::default().apply_staged_update(plat.as_ref(), &staged)?;
                            Ok::<(), anyhow::Error>(())
                        })
                        .await;
                        match res {
                            Ok(Ok(())) => {
                                let _ = worker_ev_tx.send(UpdateBridgeEvent::ApplySucceeded);
                            }
                            Ok(Err(err)) => {
                                let retryable = !err.to_string().contains("not writable");
                                let _ = worker_ev_tx.send(UpdateBridgeEvent::Failed {
                                    message: sanitize_update_error_copy(&err),
                                    retryable,
                                });
                            }
                            Err(_) => {
                                let _ = worker_ev_tx.send(UpdateBridgeEvent::Failed {
                                    message: "Could not complete update.".to_string(),
                                    retryable: true,
                                });
                            }
                        }
                    }
                    UpdateUiAction::ApplyAndRestart(staged) => {
                        let plat = worker_platform.clone();
                        let res = tokio::task::spawn_blocking(move || {
                            AtomicSwapper::default().apply_staged_update(plat.as_ref(), &staged)
                        })
                        .await;
                        match res {
                            Ok(Ok(_)) => {
                                let _ = worker_ev_tx.send(UpdateBridgeEvent::ApplySucceeded);
                            }
                            Ok(Err(err)) => {
                                let retryable = !err.to_string().contains("not writable");
                                let _ = worker_ev_tx.send(UpdateBridgeEvent::Failed {
                                    message: sanitize_update_error_copy(&err),
                                    retryable,
                                });
                            }
                            Err(_) => {
                                let _ = worker_ev_tx.send(UpdateBridgeEvent::Failed {
                                    message: "Could not complete update.".to_string(),
                                    retryable: true,
                                });
                            }
                        }
                    }
                    UpdateUiAction::RestartApp => {
                        let plat = worker_platform.clone();
                        let res = tokio::task::spawn_blocking(move || {
                            let exe = plat.current_exe()?;
                            let args = plat.preserved_cli_args();
                            plat.process_launcher().spawn_relaunch(&exe, &args)
                        })
                        .await;
                        match res {
                            Ok(Ok(_)) => {
                                let _ = worker_ev_tx.send(UpdateBridgeEvent::RestartSucceeded);
                            }
                            Ok(Err(err)) => {
                                let _ = worker_ev_tx.send(UpdateBridgeEvent::Failed {
                                    message: sanitize_update_error_copy(&err),
                                    retryable: true,
                                });
                            }
                            Err(_) => {
                                let _ = worker_ev_tx.send(UpdateBridgeEvent::Failed {
                                    message: "Could not restart Multi Bot.".to_string(),
                                    retryable: true,
                                });
                            }
                        }
                    }
                }
            }
        });
    }

    #[allow(dead_code)]
    pub(crate) fn handle_update_bridge_event(
        &mut self,
        event: UpdateBridgeEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            UpdateBridgeEvent::CheckCompleted { outcome, manual } => {
                self.update_in_flight = false;
                self.update_ui.pending_action = None;
                match outcome {
                    CheckOutcome::NotModified => {
                        if !manual {
                            // Hard Invariant #4: 304 Not Modified triggers ZERO UI redraws!
                            return;
                        }
                        if let Some(rel) = self.update_ui.last_release.clone() {
                            self.update_ui.set_update_state(UpdateState::Available(rel));
                        } else {
                            self.update_ui.set_update_state(UpdateState::UpToDate);
                            let cur =
                                UpdateUiState::normalize_version(multibot_proto::build_version());
                            self.notice = Some(format!("Multibot v{cur} is up to date."));
                        }
                        self.sync_update_state_to_shared(cx);
                        cx.notify();
                    }
                    CheckOutcome::UpToDate { etag } => {
                        if etag.is_some() && etag != self.settings.last_update_etag {
                            self.settings.last_update_etag = etag;
                            self.persist_update_settings(cx);
                        }
                        let was_visible = self.update_ui.is_card_visible(
                            self.settings.sidebar_mode,
                            self.sidebar_state.is_shown(),
                            self.settings.dismissed_update_version.as_deref(),
                        );
                        let changed = self.update_ui.set_update_state(UpdateState::UpToDate);
                        if manual {
                            let cur =
                                UpdateUiState::normalize_version(multibot_proto::build_version());
                            self.notice = Some(format!("Multibot v{cur} is up to date."));
                            self.sync_update_state_to_shared(cx);
                            cx.notify();
                        } else if changed && was_visible {
                            self.sync_update_state_to_shared(cx);
                            cx.notify();
                        }
                    }
                    CheckOutcome::RateLimited {
                        retry_after: _,
                        etag,
                    } => {
                        if etag.is_some() && etag != self.settings.last_update_etag {
                            self.settings.last_update_etag = etag;
                            self.persist_update_settings(cx);
                        }
                        if !manual {
                            // Background 403/429 backoff stays silent (zero UI redraw).
                            return;
                        }
                        if let Some(rel) = self.update_ui.last_release.clone() {
                            self.update_ui.set_update_state(UpdateState::Available(rel));
                        } else {
                            self.update_ui.set_update_state(UpdateState::Idle);
                        }
                        self.notice = Some(
                            "Update check temporarily rate-limited. Please try again later."
                                .to_string(),
                        );
                        self.sync_update_state_to_shared(cx);
                        cx.notify();
                    }
                    CheckOutcome::UpdateAvailable { release, etag } => {
                        if etag.is_some() && etag != self.settings.last_update_etag {
                            self.settings.last_update_etag = etag;
                            self.persist_update_settings(cx);
                        }
                        if manual
                            && UpdateUiState::is_version_dismissed(
                                &release.version,
                                self.settings.dismissed_update_version.as_deref(),
                            )
                        {
                            self.settings.dismissed_update_version = None;
                            self.persist_update_settings(cx);
                        }
                        let was_visible = self.update_ui.is_card_visible(
                            self.settings.sidebar_mode,
                            self.sidebar_state.is_shown(),
                            self.settings.dismissed_update_version.as_deref(),
                        );
                        let changed = self
                            .update_ui
                            .set_update_state(UpdateState::Available(release.clone()));
                        let now_visible = self.update_ui.is_card_visible(
                            self.settings.sidebar_mode,
                            self.sidebar_state.is_shown(),
                            self.settings.dismissed_update_version.as_deref(),
                        );
                        if manual {
                            let ver = UpdateUiState::normalize_version(&release.version);
                            self.notice = Some(format!("Multibot v{ver} is ready."));
                            self.sync_update_state_to_shared(cx);
                            cx.notify();
                        } else if changed && (was_visible || now_visible) {
                            self.sync_update_state_to_shared(cx);
                            cx.notify();
                        }
                    }
                }
            }
            UpdateBridgeEvent::CheckFailed { message, manual } => {
                self.update_in_flight = false;
                self.update_ui.pending_action = None;
                if !manual {
                    // Offline or transient background failure stays silent (zero redraw).
                    return;
                }
                self.update_ui.set_update_state(UpdateState::Error {
                    message,
                    retryable: true,
                });
                self.sync_update_state_to_shared(cx);
                cx.notify();
            }
            UpdateBridgeEvent::DownloadProgress {
                version,
                bytes_done,
                bytes_total,
            } => {
                self.set_update_state(
                    UpdateState::Downloading {
                        version,
                        bytes_done,
                        bytes_total,
                    },
                    cx,
                );
                self.sync_update_state_to_shared(cx);
            }
            UpdateBridgeEvent::Verifying { version } => {
                self.set_update_state(UpdateState::Verifying { version }, cx);
                self.sync_update_state_to_shared(cx);
            }
            UpdateBridgeEvent::StagedReady(staged) => {
                let ver = UpdateUiState::normalize_version(&staged.version);
                self.update_ui.set_update_state(UpdateState::Applying {
                    version: ver.clone(),
                });
                self.notice = Some(format!("Restarting to apply v{ver}..."));
                self.sync_update_state_to_shared(cx);
                cx.notify();
            }
            UpdateBridgeEvent::ApplySucceeded | UpdateBridgeEvent::RestartSucceeded => {
                self.update_in_flight = false;
                self.update_ui.pending_action = None;
                cx.quit();
            }
            UpdateBridgeEvent::Failed { message, retryable } => {
                self.update_in_flight = false;
                self.update_ui.pending_action = None;
                self.set_update_state(UpdateState::Error { message, retryable }, cx);
                self.sync_update_state_to_shared(cx);
            }
        }
    }

    fn persist_shared_update_settings(store: &SharedAppStore) {
        let _ = match store.update_settings_path_override.as_deref() {
            Some(path) => store.settings.save_to(path),
            None if store.persist_to_disk => store.settings.save(),
            None => Ok(()),
        };
    }

    fn is_update_state_visible_for_settings(
        state: &UpdateState,
        settings: &MultibotSettings,
    ) -> bool {
        let mut probe = UpdateUiState::default();
        probe.state = state.clone();
        probe.is_card_visible(
            settings.sidebar_mode,
            settings.sidebar_mode != SidebarMode::Hidden,
            settings.dismissed_update_version.as_deref(),
        )
    }

    /// Applies an [`UpdateBridgeEvent`] at App scope onto [`SharedAppStore`] so closing Window 1
    /// never bricks Window 2's updater actions or `cx.quit()`, while guaranteeing ZERO sibling
    /// `cx.notify()` redraws on silent background `304`, `RateLimited`, `CheckFailed`, or
    /// `last_update_etag`-only updates.
    pub(crate) fn apply_update_bridge_event_to_app(event: UpdateBridgeEvent, cx: &mut App) {
        if !cx.has_global::<SharedAppStore>() {
            return;
        }
        match event {
            UpdateBridgeEvent::CheckCompleted { outcome, manual } => match outcome {
                CheckOutcome::NotModified => {
                    if !manual {
                        // Hard Invariant #4: 304 Not Modified triggers ZERO UI redraws!
                        if cx.global::<SharedAppStore>().update_in_flight {
                            cx.global_mut::<SharedAppStore>().update_in_flight = false;
                        }
                        return;
                    }
                    let store = cx.global_mut::<SharedAppStore>();
                    store.update_in_flight = false;
                    if let Some(rel) = store.update_last_release.clone() {
                        store.update_state = UpdateState::Available(rel);
                    } else {
                        store.update_state = UpdateState::UpToDate;
                        let cur = UpdateUiState::normalize_version(multibot_proto::build_version());
                        store.update_notice = Some(format!("Multibot v{cur} is up to date."));
                        store.update_notice_seq = store.update_notice_seq.wrapping_add(1);
                    }
                    store.bump_revision();
                }
                CheckOutcome::UpToDate { etag } => {
                    let store = cx.global_mut::<SharedAppStore>();
                    store.update_in_flight = false;
                    if etag.is_some() && etag != store.settings.last_update_etag {
                        store.settings.last_update_etag = etag;
                        Self::persist_shared_update_settings(store);
                    }
                    let was_visible = Self::is_update_state_visible_for_settings(
                        &store.update_state,
                        &store.settings,
                    );
                    let changed = store.update_state != UpdateState::UpToDate;
                    store.update_state = UpdateState::UpToDate;
                    if manual {
                        let cur = UpdateUiState::normalize_version(multibot_proto::build_version());
                        store.update_notice = Some(format!("Multibot v{cur} is up to date."));
                        store.update_notice_seq = store.update_notice_seq.wrapping_add(1);
                        store.bump_revision();
                    } else if changed && was_visible {
                        store.bump_revision();
                    }
                }
                CheckOutcome::RateLimited {
                    retry_after: _,
                    etag,
                } => {
                    if !manual {
                        let needs_etag = etag.is_some()
                            && etag != cx.global::<SharedAppStore>().settings.last_update_etag;
                        let was_in_flight = cx.global::<SharedAppStore>().update_in_flight;
                        if needs_etag || was_in_flight {
                            let store = cx.global_mut::<SharedAppStore>();
                            store.update_in_flight = false;
                            if needs_etag {
                                store.settings.last_update_etag = etag;
                                Self::persist_shared_update_settings(store);
                            }
                        }
                        return;
                    }
                    let store = cx.global_mut::<SharedAppStore>();
                    store.update_in_flight = false;
                    if etag.is_some() && etag != store.settings.last_update_etag {
                        store.settings.last_update_etag = etag;
                        Self::persist_shared_update_settings(store);
                    }
                    if let Some(rel) = store.update_last_release.clone() {
                        store.update_state = UpdateState::Available(rel);
                    } else {
                        store.update_state = UpdateState::Idle;
                    }
                    store.update_notice = Some(
                        "Update check temporarily rate-limited. Please try again later."
                            .to_string(),
                    );
                    store.update_notice_seq = store.update_notice_seq.wrapping_add(1);
                    store.bump_revision();
                }
                CheckOutcome::UpdateAvailable { release, etag } => {
                    let store = cx.global_mut::<SharedAppStore>();
                    store.update_in_flight = false;
                    let mut settings_dirty = false;
                    if etag.is_some() && etag != store.settings.last_update_etag {
                        store.settings.last_update_etag = etag;
                        settings_dirty = true;
                    }
                    if manual
                        && UpdateUiState::is_version_dismissed(
                            &release.version,
                            store.settings.dismissed_update_version.as_deref(),
                        )
                    {
                        store.settings.dismissed_update_version = None;
                        settings_dirty = true;
                    }
                    if settings_dirty {
                        Self::persist_shared_update_settings(store);
                    }
                    let was_visible = Self::is_update_state_visible_for_settings(
                        &store.update_state,
                        &store.settings,
                    );
                    let next_state = UpdateState::Available(release.clone());
                    let changed = store.update_state != next_state;
                    store.update_last_release = Some(release.clone());
                    store.update_state = next_state;
                    let now_visible = Self::is_update_state_visible_for_settings(
                        &store.update_state,
                        &store.settings,
                    );
                    if manual {
                        let ver = UpdateUiState::normalize_version(&release.version);
                        store.update_notice = Some(format!("Multibot v{ver} is ready."));
                        store.update_notice_seq = store.update_notice_seq.wrapping_add(1);
                        store.bump_revision();
                    } else if changed && (was_visible || now_visible) {
                        store.bump_revision();
                    }
                }
            },
            UpdateBridgeEvent::CheckFailed { message, manual } => {
                if !manual {
                    if cx.global::<SharedAppStore>().update_in_flight {
                        cx.global_mut::<SharedAppStore>().update_in_flight = false;
                    }
                    return;
                }
                let store = cx.global_mut::<SharedAppStore>();
                store.update_in_flight = false;
                store.update_state = UpdateState::Error {
                    message,
                    retryable: true,
                };
                store.bump_revision();
            }
            UpdateBridgeEvent::DownloadProgress {
                version,
                bytes_done,
                bytes_total,
            } => {
                let next = UpdateState::Downloading {
                    version,
                    bytes_done,
                    bytes_total,
                };
                let store = cx.global_mut::<SharedAppStore>();
                if store.update_state != next {
                    store.update_state = next;
                    store.bump_revision();
                }
            }
            UpdateBridgeEvent::Verifying { version } => {
                let next = UpdateState::Verifying { version };
                let store = cx.global_mut::<SharedAppStore>();
                if store.update_state != next {
                    store.update_state = next;
                    store.bump_revision();
                }
            }
            UpdateBridgeEvent::StagedReady(staged) => {
                let ver = UpdateUiState::normalize_version(&staged.version);
                let store = cx.global_mut::<SharedAppStore>();
                store.update_state = UpdateState::Applying {
                    version: ver.clone(),
                };
                store.update_notice = Some(format!("Restarting to apply v{ver}..."));
                store.update_notice_seq = store.update_notice_seq.wrapping_add(1);
                store.bump_revision();
            }
            UpdateBridgeEvent::ApplySucceeded | UpdateBridgeEvent::RestartSucceeded => {
                cx.global_mut::<SharedAppStore>().update_in_flight = false;
                cx.quit();
            }
            UpdateBridgeEvent::Failed { message, retryable } => {
                let store = cx.global_mut::<SharedAppStore>();
                store.update_in_flight = false;
                store.update_state = UpdateState::Error { message, retryable };
                store.bump_revision();
            }
        }
    }

    // =========================================================================
    // Live Daemon Connection, Cache, Outbox & M3/M5 RPC Helpers
    // =========================================================================

    pub(crate) fn auto_connect_configured_hosts(&mut self, cx: &mut Context<Self>) {
        let targets: Vec<_> = if self.settings.ssh_hosts.is_empty() {
            Self::find_local_daemon()
                .map(|bin| ("localhost".to_string(), DaemonTarget::LocalBinary(bin)))
                .into_iter()
                .collect()
        } else {
            self.settings
                .ssh_hosts
                .iter()
                .cloned()
                .map(|host| {
                    (
                        host.name.clone(),
                        DaemonTarget::RemoteSsh {
                            host,
                            bundled_linux_binary: Self::find_bundled_linux_daemon(),
                        },
                    )
                })
                .collect()
        };
        for (label, target) in targets {
            let skip = cx
                .global::<SharedAppStore>()
                .host_connections
                .get(&label)
                .is_some_and(|h| {
                    h.manually_disconnected
                        || h.shutdown_tx.is_some()
                        || h.daemon_tx.as_ref().is_some_and(|tx| !tx.is_closed())
                });
            if !skip {
                Self::connect_host_on_app(self.runtime.clone(), label, target, true, cx);
            }
        }
        self.sync_from_shared_store(cx);
    }

    pub(crate) fn disconnect_host_on_app(host_label: &str, cx: &mut App) {
        let store = cx.global_mut::<SharedAppStore>();
        if let Some(entry) = store.host_connections.get_mut(host_label) {
            entry.manually_disconnected = true;
            entry.connection_gen = entry.connection_gen.wrapping_add(1);
            entry.shutdown_tx = None;
            entry.daemon_tx = None;
            entry.connected = false;
            entry.status = "Disconnected".into();
            entry.sessions.clear();
            let history_prefix = format!("{}:{host_label}", host_label.len());
            store
                .pending_older_page_before
                .retain(|key, _| !key.starts_with(&history_prefix));
            store.rebuild_merged_sessions();
            store.refresh_connection_summary();
            store.bump_revision();
        }
    }

    pub(crate) fn disconnect_all_hosts(cx: &mut App) {
        let hosts: Vec<_> = cx
            .global::<SharedAppStore>()
            .host_connections
            .keys()
            .cloned()
            .collect();
        for host in hosts {
            Self::disconnect_host_on_app(&host, cx);
        }
    }

    pub(crate) fn refresh_all_hosts(&self) -> bool {
        if self.host_txs.is_empty() && self.host_statuses.is_empty() {
            return self.daemon_tx.as_ref().is_some_and(|tx| {
                tx.send(ClientRequest::ListLifecycleSessions {
                    limit: Some(SESSION_LIST_LIMIT),
                })
                .is_ok()
            });
        }
        let mut refreshed = false;
        for tx in self.host_txs.values() {
            refreshed |= tx
                .send(ClientRequest::ListLifecycleSessions {
                    limit: Some(SESSION_LIST_LIMIT),
                })
                .is_ok();
        }
        refreshed
    }

    pub(crate) fn reconnect_all_hosts(&mut self, cx: &mut Context<Self>) {
        let refreshed = self.refresh_all_hosts();
        let targets: Vec<_> = cx
            .global::<SharedAppStore>()
            .host_connections
            .iter()
            .filter(|(_, h)| h.daemon_tx.as_ref().is_none_or(|tx| tx.is_closed()))
            .map(|(label, h)| (label.clone(), h.target.clone()))
            .collect();
        for (label, target) in targets {
            Self::connect_host_on_app(self.runtime.clone(), label, target, true, cx);
        }
        if !self.settings.ssh_hosts.is_empty() || self.host_statuses.is_empty() {
            self.auto_connect_configured_hosts(cx);
        } else {
            self.sync_from_shared_store(cx);
        }
        if let Some(s) = &self.selected_session
            && let Some(tx) = self.active_session_tx()
        {
            let _ = tx.send(ClientRequest::WatchSession {
                session_id: s.id.clone(),
                source_path: s.source_path.clone(),
                agent: s.agent,
            });
        }
        if refreshed {
            self.notice = Some("Refreshed daemon sessions.".into());
        }
        cx.notify();
    }

    pub(crate) fn connect_to_target(
        &mut self,
        label: String,
        target: DaemonTarget,
        cx: &mut Context<Self>,
    ) {
        self.connect_to_target_internal(label, target, true, cx);
    }

    pub(crate) fn connect_to_target_internal(
        &mut self,
        label: String,
        target: DaemonTarget,
        reset_upgrade_guard: bool,
        cx: &mut Context<Self>,
    ) {
        let rt = self.runtime.clone();
        Self::connect_to_target_on_app(rt, label, target, reset_upgrade_guard, cx);
        self.sync_from_shared_store(cx);
    }

    pub(crate) fn connect_to_target_on_app(
        runtime: Arc<tokio::runtime::Runtime>,
        label: String,
        target: DaemonTarget,
        reset_upgrade_guard: bool,
        cx: &mut App,
    ) -> u64 {
        Self::connect_host_on_app(runtime, label, target, reset_upgrade_guard, cx)
    }

    pub(crate) fn connect_host_on_app(
        runtime: Arc<tokio::runtime::Runtime>,
        label: String,
        target: DaemonTarget,
        reset_upgrade_guard: bool,
        cx: &mut App,
    ) -> u64 {
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
        let store = cx.global_mut::<SharedAppStore>();
        let entry = store
            .host_connections
            .entry(label.clone())
            .or_insert_with(|| HostConnectionState {
                host_label: label.clone(),
                target: target.clone(),
                status: String::new(),
                connected: false,
                manually_disconnected: false,
                reconnect_attempt: 0,
                daemon_tx: None,
                shutdown_tx: None,
                connection_gen: 0,
                daemon_version: None,
                daemon_binary_hash: None,
                daemon_upgrade_attempted: false,
                sessions: Vec::new(),
            });
        entry.connection_gen = entry.connection_gen.wrapping_add(1);
        let conn_gen = entry.connection_gen;
        entry.shutdown_tx = Some(shutdown_tx);
        entry.target = target.clone();
        entry.status = format!("Connecting to {label}...");
        entry.connected = false;
        entry.manually_disconnected = false;
        entry.daemon_tx = None;
        entry.daemon_version = None;
        entry.daemon_binary_hash = None;
        entry.sessions.clear();
        if reset_upgrade_guard {
            entry.daemon_upgrade_attempted = false;
            entry.reconnect_attempt = 0;
        }
        // Legacy fields remain available to single-host callers.
        store.connection_gen = store.connection_gen.wrapping_add(1);
        store.connection_shutdown_tx = None;
        let history_prefix = format!("{}:{label}", label.len());
        store
            .pending_older_page_before
            .retain(|key, _| !key.starts_with(&history_prefix));
        store.rebuild_merged_sessions();
        store.refresh_connection_summary();
        store.bump_revision();

        let (bridge_tx, mut bridge_rx) = mpsc::unbounded_channel::<UiBridgeEvent>();
        let rt_for_conn = runtime.clone();

        let event_host = label.clone();
        rt_for_conn.spawn(async move {
            let result = tokio::select! {
                _ = &mut shutdown_rx => return,
                result = DaemonConnection::connect(target) => result,
            };
            match result {
                Ok(mut conn) => {
                    let (req_tx, mut req_rx) = mpsc::unbounded_channel::<ClientRequest>();
                    if bridge_tx
                        .send(UiBridgeEvent::Connected {
                            host_label: label.clone(),
                            req_tx,
                        })
                        .is_err()
                    {
                        return;
                    }
                    let _ = conn.send(ClientRequest::WatchDirectory);
                    let _ = conn.send(ClientRequest::ListLifecycleSessions {
                        limit: Some(SESSION_LIST_LIMIT),
                    });

                    loop {
                        tokio::select! {
                            _ = &mut shutdown_rx => {
                                break;
                            }
                            maybe_req = req_rx.recv() => {
                                match maybe_req {
                                    Some(req) => {
                                        if conn.send(req).is_err() {
                                            break;
                                        }
                                    }
                                    None => break,
                                }
                            }
                            maybe_ev = conn.next_event() => {
                                match maybe_ev {
                                    Some(ev) => {
                                        if bridge_tx.send(UiBridgeEvent::Server(ev)).is_err() {
                                            break;
                                        }
                                    }
                                    None => break,
                                }
                            }
                        }
                    }
                    let _ = bridge_tx.send(UiBridgeEvent::Disconnected(format!(
                        "Disconnected from {label}"
                    )));
                }
                Err(err) => {
                    let _ = bridge_tx.send(UiBridgeEvent::Disconnected(format!(
                        "Connection failed: {err}"
                    )));
                }
            }
        });

        let rt_for_events = runtime;
        cx.spawn(async move |cx| {
            while let Some(event) = bridge_rx.recv().await {
                let keep_going = cx.update(|cx| {
                    Self::apply_host_bridge_event_to_app(
                        &event_host,
                        conn_gen,
                        event,
                        &rt_for_events,
                        cx,
                    )
                });
                if !keep_going {
                    break;
                }
            }
        })
        .detach();

        conn_gen
    }

    pub(crate) fn apply_host_bridge_event_to_app(
        host_label: &str,
        conn_gen: u64,
        event: UiBridgeEvent,
        runtime: &Arc<tokio::runtime::Runtime>,
        cx: &mut App,
    ) -> bool {
        if !cx.has_global::<SharedAppStore>() {
            return false;
        }
        {
            let store = cx.global::<SharedAppStore>();
            if store.window_count == 0
                || store
                    .host_connections
                    .get(host_label)
                    .is_none_or(|h| h.connection_gen != conn_gen || h.manually_disconnected)
            {
                return false;
            }
        }

        match event {
            UiBridgeEvent::Connected { req_tx, .. } => {
                let store = cx.global_mut::<SharedAppStore>();
                let entry = store.host_connections.get_mut(host_label).unwrap();
                entry.daemon_tx = Some(req_tx);
                entry.connected = true;
                entry.reconnect_attempt = 0;
                entry.status = format!("Connected: {host_label}");
                store.refresh_connection_summary();
                store.bump_revision();
            }
            UiBridgeEvent::Disconnected(reason) => {
                let store = cx.global_mut::<SharedAppStore>();
                let entry = store.host_connections.get_mut(host_label).unwrap();
                if !entry.connected && entry.shutdown_tx.is_none() {
                    return false;
                }
                entry.daemon_tx = None;
                entry.shutdown_tx = None;
                entry.connected = false;
                entry.status = reason;
                entry.sessions.clear();
                let delay = [5, 10, 30, 60][(entry.reconnect_attempt as usize).min(3)];
                entry.reconnect_attempt = entry.reconnect_attempt.saturating_add(1);
                let target = entry.target.clone();
                store.rebuild_merged_sessions();
                store.refresh_connection_summary();
                store.bump_revision();
                let host = host_label.to_string();
                let rt_timer = runtime.clone();
                let rt_connect = runtime.clone();
                cx.spawn(async move |cx| {
                    let _ = rt_timer
                        .spawn(async move {
                            tokio::time::sleep(Duration::from_secs(delay)).await;
                        })
                        .await;
                    cx.update(|cx| {
                        let store = cx.global::<SharedAppStore>();
                        if store.window_count > 0
                            && store.host_connections.get(&host).is_some_and(|h| {
                                h.connection_gen == conn_gen
                                    && !h.manually_disconnected
                                    && !h.connected
                            })
                        {
                            Self::connect_host_on_app(rt_connect, host, target, false, cx);
                        }
                    });
                })
                .detach();
            }
            UiBridgeEvent::Server(server_ev) => {
                if matches!(server_ev, ServerEvent::Pong { .. }) {
                    return true;
                }
                if let Ok(mut shared) = cx.global::<SharedAppStore>().shared_chat_outbox.lock() {
                    shared.outbox.observe_server_event(host_label, &server_ev);
                }
                match &server_ev {
                    ServerEvent::HelloAck {
                        hostname,
                        daemon_version,
                        binary_hash,
                        ..
                    } => {
                        let expected_ver = multibot_proto::build_version();
                        let upgrade_job = {
                            let store = cx.global_mut::<SharedAppStore>();
                            let entry = store.host_connections.get_mut(host_label).unwrap();
                            entry.reconnect_attempt = 0;
                            entry.daemon_version = Some(daemon_version.clone());
                            entry.daemon_binary_hash = Some(binary_hash.clone());
                            entry.status = format!("{hostname} · multibotd v{daemon_version}");

                            let bundled_path = match &entry.target {
                                DaemonTarget::RemoteSsh {
                                    bundled_linux_binary,
                                    ..
                                } => bundled_linux_binary.clone(),
                                _ => None,
                            };
                            let expected_hash = bundled_path
                                .as_deref()
                                .and_then(|p| {
                                    multibot_core::daemon_deploy::sha256_file_prefix_12(p).ok()
                                })
                                .unwrap_or_default();

                            let mut job = None;
                            if multibot_core::daemon_deploy::needs_remote_upgrade(
                                daemon_version,
                                binary_hash,
                                expected_ver,
                                &expected_hash,
                            ) {
                                if !entry.daemon_upgrade_attempted
                                    && let Some(DaemonTarget::RemoteSsh {
                                        host,
                                        bundled_linux_binary: Some(local_bin),
                                    }) = Some(entry.target.clone())
                                {
                                    entry.daemon_upgrade_attempted = true;
                                    store.daemon_notice = Some(format!(
                                        "Upgrading remote multibotd to v{expected_ver}..."
                                    ));
                                    store.daemon_notice_seq =
                                        store.daemon_notice_seq.wrapping_add(1);
                                    let host_label = host_label.to_string();
                                    let expected_target = Some(entry.target.clone());
                                    job = Some((host, local_bin, host_label, expected_target));
                                } else {
                                    store.daemon_notice = Some(format!(
                                        "Remote multibotd v{daemon_version} differs from desktop v{expected_ver}."
                                    ));
                                    store.daemon_notice_seq =
                                        store.daemon_notice_seq.wrapping_add(1);
                                }
                            }
                            store.bump_revision();
                            job
                        };

                        if let Some((host, local_bin, host_label, expected_target)) = upgrade_job {
                            let rt_deploy = runtime.clone();
                            let rt_reconnect = runtime.clone();
                            let host_c = host.clone();
                            let bin_c = local_bin.clone();
                            cx.spawn(async move |cx| {
                                let res = rt_deploy
                                    .spawn(async move {
                                        multibot_core::daemon_deploy::ensure_remote_daemon_deployed(
                                            &host_c, &bin_c,
                                        )
                                        .await
                                    })
                                    .await;
                                cx.update(|cx| {
                                    if !cx.has_global::<SharedAppStore>() {
                                        return;
                                    }
                                    {
                                        let store = cx.global::<SharedAppStore>();
                                        if store.window_count == 0
                                            || store.host_connections.get(&host_label).is_none_or(
                                                |h| {
                                                    h.connection_gen != conn_gen
                                                        || h.manually_disconnected
                                                        || Some(h.target.clone()) != expected_target
                                                },
                                            )
                                        {
                                            return;
                                        }
                                    }
                                    match res {
                                        Ok(Ok(_)) => {
                                            Self::connect_to_target_on_app(
                                                rt_reconnect,
                                                host_label,
                                                DaemonTarget::RemoteSsh {
                                                    host,
                                                    bundled_linux_binary: Some(local_bin),
                                                },
                                                false,
                                                cx,
                                            );
                                        }
                                        Ok(Err(err)) => {
                                            let store = cx.global_mut::<SharedAppStore>();
                                            store.daemon_notice = Some(format!(
                                                "Remote multibotd upgrade failed: {err}"
                                            ));
                                            store.daemon_notice_seq =
                                                store.daemon_notice_seq.wrapping_add(1);
                                            store.bump_revision();
                                        }
                                        Err(err) => {
                                            let store = cx.global_mut::<SharedAppStore>();
                                            store.daemon_notice = Some(format!(
                                                "Remote multibotd upgrade failed: {err}"
                                            ));
                                            store.daemon_notice_seq =
                                                store.daemon_notice_seq.wrapping_add(1);
                                            store.bump_revision();
                                        }
                                    }
                                });
                            })
                            .detach();
                        }
                    }
                    ServerEvent::Sessions { sessions } => {
                        let store = cx.global_mut::<SharedAppStore>();
                        store.host_connections.get_mut(host_label).unwrap().sessions =
                            sessions.clone();
                        let tx = store
                            .host_connections
                            .get(host_label)
                            .and_then(|h| h.daemon_tx.clone());
                        for session in
                            &mut store.host_connections.get_mut(host_label).unwrap().sessions
                        {
                            let key = canonical_session_key(host_label, session.agent, &session.id);
                            if store
                                .workspace_store
                                .auto_unarchive_waiting_if_active(&key, session)
                                && session.archived
                            {
                                if let Some(tx) = &tx {
                                    if tx
                                        .send(ClientRequest::UnarchiveSession {
                                            session_id: session.id.clone(),
                                        })
                                        .is_ok()
                                    {
                                        session.archived = false;
                                    }
                                }
                            }
                        }
                        store.rebuild_merged_sessions();
                        store.workspace_store.reconcile_live_sessions(
                            store
                                .host_sessions
                                .iter()
                                .map(|(host, summary)| (host.as_str(), summary)),
                        );
                        if store.persist_to_disk {
                            let _ = store.workspace_store.save();
                        }
                        store.bump_revision();
                    }
                    ServerEvent::SessionUpdated { session } => {
                        let store = cx.global_mut::<SharedAppStore>();
                        if let Some(existing) = store
                            .host_connections
                            .get_mut(host_label)
                            .unwrap()
                            .sessions
                            .iter_mut()
                            .find(|s| s.id == session.id && s.agent == session.agent)
                        {
                            *existing = session.clone();
                        } else {
                            store
                                .host_connections
                                .get_mut(host_label)
                                .unwrap()
                                .sessions
                                .insert(0, session.clone());
                        }
                        let key = canonical_session_key(host_label, session.agent, &session.id);
                        if store
                            .workspace_store
                            .auto_unarchive_waiting_if_active(&key, session)
                            && session.archived
                        {
                            if let Some(tx) = store
                                .host_connections
                                .get(host_label)
                                .and_then(|h| h.daemon_tx.as_ref())
                                .or(store.daemon_tx.as_ref())
                            {
                                let _ = tx.send(ClientRequest::UnarchiveSession {
                                    session_id: session.id.clone(),
                                });
                            }
                        }
                        store
                            .workspace_store
                            .reconcile_session(host_label, &session);
                        if store.persist_to_disk {
                            let _ = store.workspace_store.save();
                        }
                        store.bump_revision();
                    }
                    ServerEvent::HarnessSwitched { previous, session } => {
                        cx.global_mut::<SharedAppStore>()
                            .apply_harness_switch(host_label, previous, session);
                    }
                    ServerEvent::SessionSpawned { session } => {
                        let store = cx.global_mut::<SharedAppStore>();
                        if let Some(existing) = store
                            .host_connections
                            .get_mut(host_label)
                            .unwrap()
                            .sessions
                            .iter_mut()
                            .find(|s| s.id == session.id && s.agent == session.agent)
                        {
                            *existing = session.clone();
                        } else {
                            store
                                .host_connections
                                .get_mut(host_label)
                                .unwrap()
                                .sessions
                                .insert(0, session.clone());
                        }
                        let moved = store
                            .workspace_store
                            .reconcile_session(host_label, &session);
                        if moved > 0 && store.persist_to_disk {
                            let _ = store.workspace_store.save();
                        }
                        store.daemon_notice = Some(format!(
                            "Started {} session: {}",
                            session.agent.label(),
                            session.title
                        ));
                        store.daemon_notice_seq = store.daemon_notice_seq.wrapping_add(1);
                        store.bump_revision();
                    }
                    ServerEvent::SessionArchived {
                        session_id,
                        archived,
                    } => {
                        let store = cx.global_mut::<SharedAppStore>();
                        for s in &mut store.host_connections.get_mut(host_label).unwrap().sessions {
                            if s.id == *session_id {
                                s.archived = *archived;
                                s.archive_kill_process = *archived;
                            }
                        }
                        store.bump_revision();
                    }
                    ServerEvent::SessionInterrupted {
                        session_id,
                        ok,
                        detail,
                    } => {
                        let store = cx.global_mut::<SharedAppStore>();
                        if *ok {
                            for s in
                                &mut store.host_connections.get_mut(host_label).unwrap().sessions
                            {
                                if s.id == *session_id {
                                    s.working = false;
                                }
                            }
                        }
                        store.daemon_notice = Some(if detail.is_empty() {
                            if *ok {
                                "Interrupted session.".to_string()
                            } else {
                                format!("Could not interrupt {session_id}.")
                            }
                        } else {
                            detail.clone()
                        });
                        store.daemon_notice_seq = store.daemon_notice_seq.wrapping_add(1);
                        store.bump_revision();
                    }
                    ServerEvent::HistoryAppend {
                        session_id,
                        messages,
                        working,
                        ..
                    } => {
                        let mut sessions_changed = false;
                        let store = cx.global_mut::<SharedAppStore>();
                        if let Some(is_working) = working {
                            for s in
                                &mut store.host_connections.get_mut(host_label).unwrap().sessions
                            {
                                if s.id == *session_id && s.working != *is_working {
                                    s.working = *is_working;
                                    sessions_changed = true;
                                }
                            }
                        }
                        if let Some(last_asst) = messages
                            .iter()
                            .rev()
                            .find(|m| m.role == MessageRole::Assistant)
                        {
                            let preview: String = last_asst.text.trim().chars().take(120).collect();
                            for s in
                                &mut store.host_connections.get_mut(host_label).unwrap().sessions
                            {
                                if s.id == *session_id {
                                    s.last_assistant_id = Some(last_asst.id.clone());
                                    if !preview.is_empty() {
                                        s.last_assistant_preview = Some(preview.clone());
                                    }
                                    if let Some(ts) = last_asst.timestamp_ms {
                                        s.last_assistant_ms = Some(ts);
                                        s.updated_at_ms = s.updated_at_ms.max(ts);
                                    }
                                    sessions_changed = true;
                                }
                            }
                        }
                        if sessions_changed {
                            store.bump_revision();
                        }
                    }
                    _ => {}
                }

                {
                    let store = cx.global_mut::<SharedAppStore>();
                    store.rebuild_merged_sessions();
                    store.refresh_connection_summary();
                }

                // Match & consume App-scoped `pending_older_page_before` for `ServerEvent::History`
                // (pruning stale entries where the file truncated past `req_before`) so ALL sibling
                // windows receive an exact `is_older_page_hint` without misclassifying partial
                // trailing `.jsonl` lines.
                let is_older_page_hint =
                    Self::consume_host_older_page_hint(cx, host_label, &server_ev);

                // Broadcast uniformly to ALL open windows (origin_id = 0 != any view_id >= 1).
                let tx = &cx.global::<SharedAppStore>().daemon_event_tx;
                if tx.receiver_count() > 0 {
                    let _ = tx.send(DaemonBroadcastEvent {
                        origin_view_id: 0,
                        host_label: host_label.to_string(),
                        is_older_page_hint,
                        event: server_ev,
                    });
                }
            }
        }
        true
    }

    #[allow(dead_code)]
    pub(crate) fn apply_daemon_bridge_event_to_app(
        conn_gen: u64,
        event: UiBridgeEvent,
        runtime: &Arc<tokio::runtime::Runtime>,
        cx: &mut App,
    ) -> bool {
        if !cx.has_global::<SharedAppStore>() {
            return false;
        }
        let store = cx.global::<SharedAppStore>();
        let host = store
            .connected_host
            .clone()
            .unwrap_or_else(|| "local".into());
        #[cfg(test)]
        if store
            .host_connections
            .get(&host)
            .is_none_or(|h| h.connection_gen != store.connection_gen)
        {
            return Self::apply_legacy_bridge_event_to_app(conn_gen, event, runtime, cx);
        }
        Self::apply_host_bridge_event_to_app(&host, conn_gen, event, runtime, cx)
    }

    #[cfg(test)]
    /// Applies a daemon [`UiBridgeEvent`] at App scope onto [`SharedAppStore`], gated on
    /// `store.connection_gen == conn_gen` and `store.window_count > 0`, and broadcasts
    /// [`ServerEvent`]s uniformly to all open windows via `SharedAppStore.daemon_event_tx`.
    /// Returns `false` if this connection generation is stale or all windows have closed.
    fn apply_legacy_bridge_event_to_app(
        conn_gen: u64,
        event: UiBridgeEvent,
        runtime: &Arc<tokio::runtime::Runtime>,
        cx: &mut App,
    ) -> bool {
        if !cx.has_global::<SharedAppStore>() {
            return false;
        }
        {
            let store = cx.global::<SharedAppStore>();
            if store.window_count == 0 || store.connection_gen != conn_gen {
                return false;
            }
        }

        match event {
            UiBridgeEvent::Connected { host_label, req_tx } => {
                let store = cx.global_mut::<SharedAppStore>();
                store.daemon_tx = Some(req_tx);
                store.connection_status = format!("Connected: {host_label}");
                store.bump_revision();
            }
            UiBridgeEvent::Disconnected(reason) => {
                let store = cx.global_mut::<SharedAppStore>();
                store.daemon_tx = None;
                store.connection_status = reason;
                store.bump_revision();
            }
            UiBridgeEvent::Server(server_ev) => {
                if matches!(server_ev, ServerEvent::Pong { .. }) {
                    return true;
                }
                {
                    let store = cx.global::<SharedAppStore>();
                    if let Ok(mut shared) = store.shared_chat_outbox.lock() {
                        shared.outbox.observe_server_event(store.connected_host.as_deref().unwrap_or("local"), &server_ev);
                    }
                }
                match &server_ev {
                    ServerEvent::HelloAck {
                        hostname,
                        daemon_version,
                        binary_hash,
                        ..
                    } => {
                        let expected_ver = multibot_proto::build_version();
                        let upgrade_job = {
                            let store = cx.global_mut::<SharedAppStore>();
                            store.daemon_version = Some(daemon_version.clone());
                            store.daemon_binary_hash = Some(binary_hash.clone());
                            store.connection_status =
                                format!("{hostname} · multibotd v{daemon_version}");

                            let bundled_path = match &store.connected_target {
                                Some(DaemonTarget::RemoteSsh {
                                    bundled_linux_binary,
                                    ..
                                }) => bundled_linux_binary.clone(),
                                _ => None,
                            };
                            let expected_hash = bundled_path
                                .as_deref()
                                .and_then(|p| {
                                    multibot_core::daemon_deploy::sha256_file_prefix_12(p).ok()
                                })
                                .unwrap_or_default();

                            let mut job = None;
                            if multibot_core::daemon_deploy::needs_remote_upgrade(
                                daemon_version,
                                binary_hash,
                                expected_ver,
                                &expected_hash,
                            ) {
                                if !store.daemon_upgrade_attempted
                                    && let Some(DaemonTarget::RemoteSsh {
                                        host,
                                        bundled_linux_binary: Some(local_bin),
                                    }) = store.connected_target.clone()
                                {
                                    store.daemon_upgrade_attempted = true;
                                    store.daemon_notice = Some(format!(
                                        "Upgrading remote multibotd to v{expected_ver}..."
                                    ));
                                    store.daemon_notice_seq =
                                        store.daemon_notice_seq.wrapping_add(1);
                                    let host_label = store
                                        .connected_host
                                        .clone()
                                        .unwrap_or_else(|| host.name.clone());
                                    let expected_target = store.connected_target.clone();
                                    job = Some((host, local_bin, host_label, expected_target));
                                } else {
                                    store.daemon_notice = Some(format!(
                                        "Remote multibotd v{daemon_version} differs from desktop v{expected_ver}."
                                    ));
                                    store.daemon_notice_seq =
                                        store.daemon_notice_seq.wrapping_add(1);
                                }
                            }
                            store.bump_revision();
                            job
                        };

                        if let Some((host, local_bin, host_label, expected_target)) = upgrade_job {
                            let rt_deploy = runtime.clone();
                            let rt_reconnect = runtime.clone();
                            let host_c = host.clone();
                            let bin_c = local_bin.clone();
                            cx.spawn(async move |cx| {
                                let res = rt_deploy
                                    .spawn(async move {
                                        multibot_core::daemon_deploy::ensure_remote_daemon_deployed(
                                            &host_c, &bin_c,
                                        )
                                        .await
                                    })
                                    .await;
                                cx.update(|cx| {
                                    if !cx.has_global::<SharedAppStore>() {
                                        return;
                                    }
                                    {
                                        let store = cx.global::<SharedAppStore>();
                                        if store.window_count == 0
                                            || store.connection_gen != conn_gen
                                            || store.connected_target != expected_target
                                        {
                                            return;
                                        }
                                    }
                                    match res {
                                        Ok(Ok(_)) => {
                                            Self::connect_to_target_on_app(
                                                rt_reconnect,
                                                host_label,
                                                DaemonTarget::RemoteSsh {
                                                    host,
                                                    bundled_linux_binary: Some(local_bin),
                                                },
                                                false,
                                                cx,
                                            );
                                        }
                                        Ok(Err(err)) => {
                                            let store = cx.global_mut::<SharedAppStore>();
                                            store.daemon_notice = Some(format!(
                                                "Remote multibotd upgrade failed: {err}"
                                            ));
                                            store.daemon_notice_seq =
                                                store.daemon_notice_seq.wrapping_add(1);
                                            store.bump_revision();
                                        }
                                        Err(err) => {
                                            let store = cx.global_mut::<SharedAppStore>();
                                            store.daemon_notice = Some(format!(
                                                "Remote multibotd upgrade failed: {err}"
                                            ));
                                            store.daemon_notice_seq =
                                                store.daemon_notice_seq.wrapping_add(1);
                                            store.bump_revision();
                                        }
                                    }
                                });
                            })
                            .detach();
                        }
                    }
                    ServerEvent::Sessions { sessions } => {
                        let store = cx.global_mut::<SharedAppStore>();
                        store.sessions = sessions.clone();
                        let host = store.connected_host.as_deref().unwrap_or("local");
                        for session in &mut store.sessions {
                            let key = canonical_session_key(host, session.agent, &session.id);
                            if store
                                .workspace_store
                                .auto_unarchive_waiting_if_active(&key, session)
                                && session.archived
                            {
                                if let Some(tx) = &store.daemon_tx {
                                    if tx
                                        .send(ClientRequest::UnarchiveSession {
                                            session_id: session.id.clone(),
                                        })
                                        .is_ok()
                                    {
                                        session.archived = false;
                                    }
                                }
                            }
                        }
                        store.workspace_store.reconcile_live_sessions(
                            sessions.iter().map(|summary| (host, summary)),
                        );
                        if store.persist_to_disk {
                            let _ = store.workspace_store.save();
                        }
                        store.bump_revision();
                    }
                    ServerEvent::SessionUpdated { session } => {
                        let store = cx.global_mut::<SharedAppStore>();
                        if let Some(existing) = store
                            .sessions
                            .iter_mut()
                            .find(|s| s.id == session.id && s.agent == session.agent)
                        {
                            *existing = session.clone();
                        } else {
                            store.sessions.insert(0, session.clone());
                        }
                        let host_label = store.connected_host.as_deref().unwrap_or("local");
                        let key = canonical_session_key(host_label, session.agent, &session.id);
                        if store
                            .workspace_store
                            .auto_unarchive_waiting_if_active(&key, session)
                            && session.archived
                        {
                            if let Some(tx) = store
                                .host_connections
                                .get(host_label)
                                .and_then(|h| h.daemon_tx.as_ref())
                                .or(store.daemon_tx.as_ref())
                            {
                                let _ = tx.send(ClientRequest::UnarchiveSession {
                                    session_id: session.id.clone(),
                                });
                            }
                        }
                        store
                            .workspace_store
                            .reconcile_session(host_label, &session);
                        if store.persist_to_disk {
                            let _ = store.workspace_store.save();
                        }
                        store.bump_revision();
                    }
                    ServerEvent::HarnessSwitched { previous, session } => {
                        let host = cx
                            .global::<SharedAppStore>()
                            .connected_host
                            .clone()
                            .unwrap_or_else(|| "local".into());
                        cx.global_mut::<SharedAppStore>()
                            .apply_harness_switch(&host, previous, session);
                    }
                    ServerEvent::SessionSpawned { session } => {
                        let store = cx.global_mut::<SharedAppStore>();
                        if let Some(existing) = store
                            .sessions
                            .iter_mut()
                            .find(|s| s.id == session.id && s.agent == session.agent)
                        {
                            *existing = session.clone();
                        } else {
                            store.sessions.insert(0, session.clone());
                        }
                        let host_label = store.connected_host.as_deref().unwrap_or("local");
                        let moved = store
                            .workspace_store
                            .reconcile_session(host_label, &session);
                        if moved > 0 && store.persist_to_disk {
                            let _ = store.workspace_store.save();
                        }
                        store.daemon_notice = Some(format!(
                            "Started {} session: {}",
                            session.agent.label(),
                            session.title
                        ));
                        store.daemon_notice_seq = store.daemon_notice_seq.wrapping_add(1);
                        store.bump_revision();
                    }
                    ServerEvent::SessionArchived {
                        session_id,
                        archived,
                    } => {
                        let store = cx.global_mut::<SharedAppStore>();
                        for s in &mut store.sessions {
                            if s.id == *session_id {
                                s.archived = *archived;
                                s.archive_kill_process = *archived;
                            }
                        }
                        store.bump_revision();
                    }
                    ServerEvent::SessionInterrupted {
                        session_id,
                        ok,
                        detail,
                    } => {
                        let store = cx.global_mut::<SharedAppStore>();
                        if *ok {
                            for s in &mut store.sessions {
                                if s.id == *session_id {
                                    s.working = false;
                                }
                            }
                        }
                        store.daemon_notice = Some(if detail.is_empty() {
                            if *ok {
                                "Interrupted session.".to_string()
                            } else {
                                format!("Could not interrupt {session_id}.")
                            }
                        } else {
                            detail.clone()
                        });
                        store.daemon_notice_seq = store.daemon_notice_seq.wrapping_add(1);
                        store.bump_revision();
                    }
                    ServerEvent::HistoryAppend {
                        session_id,
                        messages,
                        working,
                        ..
                    } => {
                        let mut sessions_changed = false;
                        let store = cx.global_mut::<SharedAppStore>();
                        if let Some(is_working) = working {
                            for s in &mut store.sessions {
                                if s.id == *session_id && s.working != *is_working {
                                    s.working = *is_working;
                                    sessions_changed = true;
                                }
                            }
                        }
                        if let Some(last_asst) = messages
                            .iter()
                            .rev()
                            .find(|m| m.role == MessageRole::Assistant)
                        {
                            let preview: String = last_asst.text.trim().chars().take(120).collect();
                            for s in &mut store.sessions {
                                if s.id == *session_id {
                                    s.last_assistant_id = Some(last_asst.id.clone());
                                    if !preview.is_empty() {
                                        s.last_assistant_preview = Some(preview.clone());
                                    }
                                    if let Some(ts) = last_asst.timestamp_ms {
                                        s.last_assistant_ms = Some(ts);
                                        s.updated_at_ms = s.updated_at_ms.max(ts);
                                    }
                                    sessions_changed = true;
                                }
                            }
                        }
                        if sessions_changed {
                            store.bump_revision();
                        }
                    }
                    _ => {}
                }

                // Match & consume App-scoped `pending_older_page_before` for `ServerEvent::History`
                // (pruning stale entries where the file truncated past `req_before`) so ALL sibling
                // windows receive an exact `is_older_page_hint` without misclassifying partial
                // trailing `.jsonl` lines.
                let is_older_page_hint = Self::consume_shared_older_page_hint(cx, &server_ev);

                // Broadcast uniformly to ALL open windows (origin_id = 0 != any view_id >= 1).
                let tx = &cx.global::<SharedAppStore>().daemon_event_tx;
                if tx.receiver_count() > 0 {
                    let _ = tx.send(DaemonBroadcastEvent {
                        origin_view_id: 0,
                        host_label: cx
                            .global::<SharedAppStore>()
                            .connected_host
                            .clone()
                            .unwrap_or_else(|| "local".into()),
                        is_older_page_hint,
                        event: server_ev,
                    });
                }
            }
        }
        let store = cx.global_mut::<SharedAppStore>();
        if let Some(host) = &store.connected_host
            && let Some(entry) = store.host_connections.get_mut(host)
        {
            entry.daemon_tx = store.daemon_tx.clone();
            entry.connected = entry.daemon_tx.is_some();
            entry.sessions = store.sessions.clone();
            entry.status = store.connection_status.clone();
            store.rebuild_merged_sessions();
        }
        true
    }

    /// Matches and consumes an App-scoped `pending_older_page_before` entry for a `ServerEvent::History`.
    ///
    /// First prunes any stale queued `req_before` entries where the transcript was truncated/rotated
    /// past `req_before` (`file_size < req_before`). Then pops a matching entry (preferring an exact
    /// `lo == req_before` match, falling back to `lo <= req_before`) and returns `true`.
    fn consume_shared_older_page_hint(cx: &mut App, server_ev: &ServerEvent) -> bool {
        let ServerEvent::History {
            session_id,
            latest_offset,
            file_size,
            ..
        } = server_ev
        else {
            return false;
        };
        if !cx.has_global::<SharedAppStore>() {
            return false;
        }
        Self::consume_older_page_hint(cx, session_id, *latest_offset, *file_size)
    }

    fn consume_host_older_page_hint(cx: &mut App, host: &str, event: &ServerEvent) -> bool {
        let ServerEvent::History {
            session_id,
            latest_offset,
            file_size,
            ..
        } = event
        else {
            return false;
        };
        let key = format!("{}:{host}{session_id}", host.len());
        Self::consume_older_page_hint(cx, &key, *latest_offset, *file_size)
    }

    fn consume_older_page_hint(
        cx: &mut App,
        session_id: &str,
        latest_offset: Option<u64>,
        file_size: Option<u64>,
    ) -> bool {
        let store = cx.global_mut::<SharedAppStore>();
        let Some(queue) = store.pending_older_page_before.get_mut(session_id) else {
            return false;
        };
        if let Some(fs) = file_size {
            queue.retain(|&req_before| req_before <= fs);
        }
        let matched = if let Some(lo) = latest_offset {
            if let Some(pos) = queue
                .iter()
                .position(|&req_before| lo == req_before)
                .or_else(|| queue.iter().position(|&req_before| lo <= req_before))
            {
                queue.remove(pos);
                true
            } else {
                false
            }
        } else {
            false
        };
        if queue.is_empty() {
            store.pending_older_page_before.remove(session_id);
        }
        matched
    }

    fn preserve_failed_local_messages(&self, server_msgs: Vec<ChatMessage>) -> Vec<ChatMessage> {
        if self.chat_state.failed_sends.is_empty() {
            return server_msgs;
        }
        let mut out = server_msgs;
        let existing_ids: HashSet<String> = out.iter().map(|m| m.id.clone()).collect();
        for msg in &self.messages {
            if self.chat_state.failed_sends.contains_key(&msg.id) && !existing_ids.contains(&msg.id)
            {
                out.push(msg.clone());
            }
        }
        out
    }

    #[allow(dead_code)]
    pub(crate) fn handle_bridge_event(&mut self, event: UiBridgeEvent, cx: &mut Context<Self>) {
        let mut is_older_page_hint = false;
        if let UiBridgeEvent::Server(ref server_ev) = event
            && !matches!(server_ev, ServerEvent::Pong { .. })
            && cx.has_global::<SharedAppStore>()
        {
            is_older_page_hint = Self::consume_shared_older_page_hint(cx, server_ev);
            let tx = &cx.global::<SharedAppStore>().daemon_event_tx;
            if tx.receiver_count() > 1 {
                let _ = tx.send(DaemonBroadcastEvent {
                    origin_view_id: self.view_id,
                    host_label: self.current_host_label().to_string(),
                    is_older_page_hint,
                    event: server_ev.clone(),
                });
            }
        }

        if let UiBridgeEvent::Server(ref server_event) = event {
            let host = self.current_host_label().to_string();
            if let Ok(mut shared) = self.shared_chat_outbox.lock() {
                shared.outbox.observe_server_event(&host, server_event);
            }
            self.handle_outbox_event(&host, server_event);
        }
        match event {
            UiBridgeEvent::Connected { host_label, req_tx } => {
                self.daemon_tx = Some(req_tx);
                self.connection_status = format!("Connected: {host_label}");
                self.sync_sessions_to_shared(cx);
            }
            UiBridgeEvent::Disconnected(reason) => {
                self.daemon_tx = None;
                self.connection_status = reason;
                self.sync_sessions_to_shared(cx);
            }
            UiBridgeEvent::Server(server_ev) => match server_ev {
                ServerEvent::BrowserCommand { request_id, session_id, binding_id, command } => {
                    self.handle_browser_command(self.current_host_label(), &session_id, &binding_id, request_id, command);
                    return;
                }
                ServerEvent::Pong { .. } => {
                    return;
                }
                ServerEvent::HelloAck {
                    hostname,
                    daemon_version,
                    binary_hash,
                    ..
                } => {
                    self.daemon_version = Some(daemon_version.clone());
                    self.daemon_binary_hash = Some(binary_hash.clone());
                    self.connection_status = format!("{hostname} · multibotd v{daemon_version}");

                    // Check remote daemon version/hash drift against desktop build + bundled linux binary
                    let expected_ver = multibot_proto::build_version();
                    let (connected_target, upgrade_attempted, expected_gen) = cx
                        .try_global::<SharedAppStore>()
                        .map(|s| {
                            (
                                s.connected_target.clone(),
                                s.daemon_upgrade_attempted,
                                s.connection_gen,
                            )
                        })
                        .unwrap_or((None, false, 0));
                    let bundled_path = match &connected_target {
                        Some(DaemonTarget::RemoteSsh {
                            bundled_linux_binary,
                            ..
                        }) => bundled_linux_binary.clone(),
                        _ => None,
                    };
                    let expected_hash = bundled_path
                        .as_deref()
                        .and_then(|p| multibot_core::daemon_deploy::sha256_file_prefix_12(p).ok())
                        .unwrap_or_default();

                    if multibot_core::daemon_deploy::needs_remote_upgrade(
                        &daemon_version,
                        &binary_hash,
                        expected_ver,
                        &expected_hash,
                    ) {
                        if !upgrade_attempted
                            && let Some(DaemonTarget::RemoteSsh {
                                host,
                                bundled_linux_binary: Some(local_bin),
                            }) = connected_target.clone()
                        {
                            if cx.has_global::<SharedAppStore>() {
                                cx.global_mut::<SharedAppStore>().daemon_upgrade_attempted = true;
                            }
                            self.notice =
                                Some(format!("Upgrading remote multibotd to v{expected_ver}..."));
                            let host_label = self.current_host_label().to_string();
                            let expected_target = connected_target;
                            let rt_deploy = self.runtime.clone();
                            let rt_reconnect = self.runtime.clone();
                            let host_c = host.clone();
                            let bin_c = local_bin.clone();
                            let app: &mut App = cx;
                            app.spawn(async move |cx| {
                                let res = rt_deploy
                                    .spawn(async move {
                                        multibot_core::daemon_deploy::ensure_remote_daemon_deployed(
                                            &host_c, &bin_c,
                                        )
                                        .await
                                    })
                                    .await;
                                cx.update(|cx| {
                                    if !cx.has_global::<SharedAppStore>() {
                                        return;
                                    }
                                    {
                                        let store = cx.global::<SharedAppStore>();
                                        if store.window_count == 0
                                            || store.connection_gen != expected_gen
                                            || store.connected_target != expected_target
                                        {
                                            return;
                                        }
                                    }
                                    match res {
                                        Ok(Ok(_)) => {
                                            Self::connect_to_target_on_app(
                                                rt_reconnect,
                                                host_label,
                                                DaemonTarget::RemoteSsh {
                                                    host,
                                                    bundled_linux_binary: Some(local_bin),
                                                },
                                                false,
                                                cx,
                                            );
                                        }
                                        Ok(Err(err)) => {
                                            let store = cx.global_mut::<SharedAppStore>();
                                            store.daemon_notice = Some(format!(
                                                "Remote multibotd upgrade failed: {err}"
                                            ));
                                            store.daemon_notice_seq =
                                                store.daemon_notice_seq.wrapping_add(1);
                                            store.bump_revision();
                                        }
                                        Err(err) => {
                                            let store = cx.global_mut::<SharedAppStore>();
                                            store.daemon_notice = Some(format!(
                                                "Remote multibotd upgrade failed: {err}"
                                            ));
                                            store.daemon_notice_seq =
                                                store.daemon_notice_seq.wrapping_add(1);
                                            store.bump_revision();
                                        }
                                    }
                                });
                            })
                            .detach();
                        } else {
                            self.notice = Some(format!(
                                "Remote multibotd v{daemon_version} differs from desktop v{expected_ver}."
                            ));
                        }
                    }

                    // Automatic FIFO reconnect flush of queued offline messages for this host
                    self.flush_outbox_for_current_host();

                    // Refresh slash commands for active session if one is selected
                    if let Some(ref sel) = self.selected_session {
                        self.request_daemon_commands(sel.agent, sel.cwd.clone());
                    }
                    self.sync_sessions_to_shared(cx);
                }
                ServerEvent::Sessions { sessions } => {
                    self.sessions = sessions;
                    for session in self.sessions.clone() {
                        let key = self.session_key_for(&session);
                        if self
                            .sidebar_state
                            .workspace_store
                            .auto_unarchive_waiting_if_active(&key, &session)
                            && session.archived
                        {
                            let host = self.host_for_summary(&session);
                            self.unarchive_session_on_host(&host, session.id.clone(), cx);
                        }
                    }
                    let live: Vec<(String, multibot_proto::SessionSummary)> = self
                        .sessions
                        .iter()
                        .map(|s| (self.host_for_summary(s), s.clone()))
                        .collect();
                    self.sidebar_state
                        .workspace_store
                        .reconcile_live_sessions(live.iter().map(|(host, s)| (host.as_str(), s)));
                    self.persist_workspace_store_if_enabled();
                    self.sync_workspace_to_shared(cx);
                    if let Some(ref sel) = self.selected_session.clone()
                        && let Some(fresh) = self
                            .sessions
                            .iter()
                            .find(|s| s.id == sel.id && s.agent == sel.agent)
                            .cloned()
                    {
                        self.selected_session = Some(fresh);
                    }
                    self.sync_sessions_to_shared(cx);
                    if self.selected_session.is_none()
                        && let Some(first) = self
                            .sessions
                            .iter()
                            .find(|s| {
                                self.sidebar_state.workspace_store.session_lifecycle(
                                    &self.session_key_for(s),
                                    s,
                                    now_unix_ms(),
                                ) == multibot_core::workspaces::SessionLifecycle::Active
                            })
                            .cloned()
                    {
                        self.open_session_with_tracking(first, false, cx);
                    }
                    self.flush_outbox_for_current_host();
                }
                ServerEvent::SessionUpdated { session } => {
                    if let Some(existing) = self
                        .sessions
                        .iter_mut()
                        .find(|s| s.id == session.id && s.agent == session.agent)
                    {
                        *existing = session.clone();
                    } else {
                        self.sessions.insert(0, session.clone());
                    }
                    let key = self.session_key_for(&session);
                    if self
                        .sidebar_state
                        .workspace_store
                        .auto_unarchive_waiting_if_active(&key, &session)
                        && session.archived
                    {
                        let host = self.active_session_host().to_string();
                        self.unarchive_session_on_host(&host, session.id.clone(), cx);
                    }
                    if self
                        .selected_session
                        .as_ref()
                        .is_some_and(|s| s.id == session.id && s.agent == session.agent)
                    {
                        self.selected_session = Some(session.clone());
                        if self.main_surface == MainSurface::Chat {
                            self.sidebar_state
                                .workspace_store
                                .mark_session_read(&key, &session);
                        }
                    }
                    self.persist_workspace_store_if_enabled();
                    self.sync_sessions_to_shared(cx);
                    self.sync_workspace_to_shared(cx);
                }
                ServerEvent::Harnesses { choices } => {
                    self.chat_state
                        .harness_choices
                        .insert(self.active_session_host().into(), choices);
                }
                ServerEvent::HarnessSwitched { previous, session } => {
                    let host = self.active_session_host().to_string();
                    self.apply_harness_switch(&host, previous, session, cx);
                }
                ServerEvent::SessionSpawned { session } => {
                    if let Some(existing) = self
                        .sessions
                        .iter_mut()
                        .find(|s| s.id == session.id && s.agent == session.agent)
                    {
                        *existing = session.clone();
                    } else {
                        self.sessions.insert(0, session.clone());
                    }
                    self.sync_sessions_to_shared(cx);
                    let title = session.title.clone();
                    let agent_label = session.agent.label();
                    if let Some(options) = self.chat_state.pending_spawn_options.take() {
                        let key = self.session_key_for(&session);
                        self.chat_state.composer_options.insert(key, options);
                    }
                    self.pending_spawn_open = false;
                    self.open_session_with_tracking(session, false, cx);
                    self.notice = Some(format!("Started {agent_label} session: {title}"));
                }
                ServerEvent::SessionArchived {
                    session_id,
                    archived,
                } => {
                    for s in &mut self.sessions {
                        if s.id == session_id {
                            s.archived = archived;
                            s.archive_kill_process = archived;
                        }
                    }
                    if let Some(ref mut sel) = self.selected_session
                        && sel.id == session_id
                    {
                        sel.archived = archived;
                        sel.archive_kill_process = archived;
                    }
                    self.sync_sessions_to_shared(cx);
                }
                ServerEvent::SessionInterrupted {
                    session_id,
                    ok,
                    detail,
                } => {
                    if ok {
                        for s in &mut self.sessions {
                            if s.id == session_id {
                                s.working = false;
                            }
                        }
                        if let Some(ref mut sel) = self.selected_session
                            && sel.id == session_id
                        {
                            sel.working = false;
                        }
                        self.sync_sessions_to_shared(cx);
                    }
                    self.notice = Some(if detail.is_empty() {
                        if ok {
                            "Interrupted session.".to_string()
                        } else {
                            format!("Could not interrupt {session_id}.")
                        }
                    } else {
                        detail
                    });
                }
                ev @ ServerEvent::History { .. } => {
                    self.apply_history_event(ev, is_older_page_hint);
                }
                ev @ ServerEvent::HistoryAppend { .. } => {
                    let (_, sessions_changed) = self.apply_history_append_event(ev);
                    if sessions_changed {
                        self.sync_sessions_to_shared(cx);
                    }
                }
                ServerEvent::SendAccepted { .. } => {
                    self.last_dispatched_prompt = None;
                }
                ServerEvent::PromptStatus {
                    session_id,
                    message_id,
                    status,
                    detail,
                } => {
                    self.track_queued_prompt(&session_id, &message_id, &status);
                    match status.as_str() {
                        "failed" => {
                            let (msg_id, text) = if !message_id.is_empty() {
                                let matched_text = self
                                    .last_dispatched_prompt
                                    .as_ref()
                                    .filter(|(id, _)| id == &message_id)
                                    .map(|(_, t)| t.clone())
                                    .or_else(|| {
                                        self.messages
                                            .iter()
                                            .rev()
                                            .find(|m| m.id == message_id)
                                            .map(|m| m.text.clone())
                                    })
                                    .unwrap_or_default();
                                (message_id, matched_text)
                            } else if let Some((id, t)) = self.last_dispatched_prompt.take() {
                                (id, t)
                            } else {
                                (format!("failed-{}", self.messages.len()), String::new())
                            };
                            let reason = if detail.is_empty() {
                                "Message delivery failed".to_string()
                            } else {
                                detail
                            };
                            self.record_send_failure(msg_id, text, reason, cx);
                        }
                        "cancelled" => {
                            self.notice = Some("Cancelled prompt.".to_string());
                        }
                        "sent" => {
                            self.last_dispatched_prompt = None;
                        }
                        "queued" => {
                            self.chat_state.advance_busy_tick();
                        }
                        "not_found" => {
                            if !detail.is_empty() {
                                self.notice = Some(detail);
                            }
                        }
                        _ => {}
                    }
                }
                ServerEvent::SearchResults {
                    request_id,
                    session_id,
                    hits,
                    truncated,
                } => {
                    if self.remote_search_request_id.as_deref() != Some(request_id.as_str()) {
                        return;
                    }
                    if self
                        .selected_session
                        .as_ref()
                        .is_some_and(|s| s.id == session_id)
                    {
                        self.remote_search_hits = hits;
                        self.remote_search_truncated = truncated;
                        let current_oldest = self
                            .active_switch_token
                            .as_ref()
                            .and_then(|t| self.chat_cache.get(&t.key))
                            .map(|c| c.oldest_offset)
                            .unwrap_or(0);
                        if self.chat_state.find_matches.is_empty()
                            && self
                                .remote_search_hits
                                .iter()
                                .any(|h| h.byte_offset < current_oldest)
                        {
                            self.load_older_history(cx);
                        }
                    }
                }
                ServerEvent::Commands {
                    agent,
                    cwd: _,
                    items,
                } => {
                    self.slash_picker_state.set_daemon_commands(agent, items);
                }
                ev @ ServerEvent::Error { .. } => {
                    let ServerEvent::Error {
                        ref code,
                        ref message,
                    } = ev
                    else {
                        unreachable!();
                    };
                    if (code == "read_history_failed" || code == "watch_failed")
                        && let Some(token) = self.active_switch_token.clone()
                    {
                        let _ = self.chat_cache.reconcile_server_event(&token, "", &ev);
                    }
                    if code == "send_failed"
                        && let Some((msg_id, text)) = self.last_dispatched_prompt.take()
                    {
                        self.record_send_failure(msg_id, text, message.clone(), cx);
                    }
                    if code == "switch_shell_client_failed" {
                        self.shell_host.switch_failed(message);
                        cx.notify();
                        return;
                    }
                    if code == "switch_harness_failed" {
                        self.chat_state.pending_harness_switch = None;
                    }
                    self.notice = Some(format!("[{code}] {message}"));
                }
            },
        }
        cx.notify();
    }

    fn apply_history_event(&mut self, ev: ServerEvent, is_older_page_hint: bool) -> bool {
        let ServerEvent::History {
            ref session_id,
            ref messages,
            oldest_offset,
            latest_offset,
            has_more,
            file_size,
            ref todos,
            ..
        } = ev
        else {
            return false;
        };

        let cache_key = self
            .active_switch_token
            .as_ref()
            .filter(|t| t.key.session_id == *session_id)
            .map(|t| t.key.clone())
            .or_else(|| {
                self.pending_switch_tokens
                    .get(session_id)
                    .map(|t| t.key.clone())
            })
            .or_else(|| {
                self.session_inputs()
                    .into_iter()
                    .find(|s| s.host == self.active_session_host() && s.summary.id == *session_id)
                    .map(|s| CacheKey::new(s.host, s.summary.agent, &s.summary.id))
                    .or_else(|| {
                        self.selected_session
                            .as_ref()
                            .filter(|s| s.id == *session_id)
                            .map(|s| CacheKey::new(self.active_session_host(), s.agent, &s.id))
                    })
            });
        let cached_entry = cache_key.as_ref().and_then(|k| self.chat_cache.get(k));
        let cached_oldest_offset = cached_entry.map(|c| c.oldest_offset);
        let cached_file_size = cached_entry.map(|c| c.file_size);
        let is_truncated_eof_tail =
            file_size.is_some_and(|fs| cached_file_size.is_some_and(|cfs| cfs > 0 && fs < cfs));

        // Older-page pagination detection (works across ALL multi-window broadcast receivers
        // via App-scoped `is_older_page_hint` without misclassifying active `.jsonl` transcripts
        // that end with an unterminated partial trailing line where `latest_offset < file_size`).
        // Truncated/rotated transcripts ALWAYS replace stale cache instead of prepending.
        let is_older_page = !is_truncated_eof_tail
            && (is_older_page_hint
                || self
                    .pending_older_page_before
                    .get(session_id)
                    .copied()
                    .is_some_and(|req_before| {
                        file_size.is_none_or(|fs| req_before <= fs)
                            && latest_offset.is_some_and(|lo| lo <= req_before)
                    })
                || cached_oldest_offset
                    .is_some_and(|co| co > 0 && latest_offset.is_some_and(|lo| lo <= co)));

        if is_older_page {
            self.pending_older_page_before.remove(session_id);
            let Some(key) = cache_key else {
                return false;
            };
            let Some(mut cached) = self.chat_cache.get(&key).cloned() else {
                // This window does not hold the cached tail for this session: ignore cleanly
                // without overwriting Tier-2 disk cache or touching this window's active view.
                return false;
            };
            let Some(lo) = latest_offset else {
                return false;
            };
            // Enforce exact boundary contiguity: ignore already-prepended pages (`oldest_offset < lo`)
            // AND non-contiguous pages from sibling windows at a deeper pagination offset
            // (`oldest_offset > lo`) so no permanent unreachable middle gap can ever be created.
            if cached.oldest_offset != lo {
                return false;
            }

            let existing_ids: HashSet<String> =
                cached.messages.iter().map(|m| m.id.clone()).collect();
            let mut combined: Vec<ChatMessage> = messages
                .iter()
                .filter(|m| !existing_ids.contains(&m.id))
                .cloned()
                .collect();
            let added_count = combined.len();
            combined.extend(cached.messages.drain(..));
            cached.messages = combined;
            if let Some(oo) = oldest_offset {
                cached.oldest_offset = oo;
            }
            cached.has_more = has_more;
            // Preserve `consumed_offset`, `file_size`, and `probe_sha256` (EOF probe).
            // `ChatCache::put` updates Tier-1 memory LRU ONLY (never overwrites Tier-2 disk).
            self.chat_cache.put(key, cached.clone());

            if self
                .selected_session
                .as_ref()
                .is_some_and(|s| s.id == *session_id)
            {
                if added_count > 0 {
                    if self.chat_state.scroll.pending_scroll == PendingScrollTarget::None {
                        let handle = &self.chat_state.scroll_handle;
                        self.chat_state.prepend_scroll_anchor =
                            Some((handle.max_offset().y, handle.offset()));
                    }
                    self.chat_state.scroll.prepended(added_count);
                }
                self.messages = self.preserve_failed_local_messages(cached.messages);
                self.chat_state.reconcile_local_reactions(&self.messages);
                if self.chat_state.find_open {
                    let q = self.chat_state.find_query.clone();
                    self.chat_state
                        .update_find_query(q, &self.messages, self.show_tool_calls());
                }
                return added_count > 0 || oldest_offset.is_some();
            }
            return false;
        }

        // Non-older tail History: clear any stale pending older-page marker so a subsequent
        // truncated/rotated tail can never be misclassified as an older page.
        self.pending_older_page_before.remove(session_id);

        let source_path = self
            .session_inputs()
            .into_iter()
            .find(|s| s.host == self.active_session_host() && s.summary.id == *session_id)
            .map(|s| s.summary.source_path.clone())
            .or_else(|| {
                self.selected_session
                    .as_ref()
                    .map(|s| s.source_path.clone())
            })
            .unwrap_or_default();

        let token = self
            .pending_switch_tokens
            .get(session_id)
            .cloned()
            .or_else(|| {
                self.active_switch_token
                    .as_ref()
                    .filter(|t| t.key.session_id == *session_id)
                    .cloned()
            });

        if let Some(token) = token {
            match self
                .chat_cache
                .reconcile_server_event(&token, source_path, &ev)
            {
                Some(ReconcileOutcome::AppliedToActive) => {
                    if let Some(cached) = self.chat_cache.get(&token.key) {
                        self.messages =
                            self.preserve_failed_local_messages(cached.messages.clone());
                    }
                    self.chat_state.scroll.history_loaded(&self.messages, false);
                    self.chat_state.reconcile_local_reactions(&self.messages);
                    if !todos.is_empty() || self.chat_state.session_todos.contains_key(session_id) {
                        self.chat_state
                            .session_todos
                            .insert(session_id.clone(), todos.clone());
                    }
                    if self.chat_state.find_open {
                        let q = self.chat_state.find_query.clone();
                        self.chat_state.update_find_query(
                            q,
                            &self.messages,
                            self.show_tool_calls(),
                        );
                    }
                    return true;
                }
                Some(ReconcileOutcome::UpdatedBackgroundOnly)
                | Some(ReconcileOutcome::PreservedCachedOnError)
                | Some(ReconcileOutcome::UncachedError { .. })
                | None => return false,
            }
        }

        if self
            .selected_session
            .as_ref()
            .is_some_and(|s| s.id == *session_id)
        {
            self.messages = self.preserve_failed_local_messages(messages.clone());
            self.chat_state.scroll.history_loaded(&self.messages, false);
            self.chat_state.reconcile_local_reactions(&self.messages);
            if !todos.is_empty() {
                self.chat_state
                    .session_todos
                    .insert(session_id.clone(), todos.clone());
            }
            if self.chat_state.find_open {
                let q = self.chat_state.find_query.clone();
                self.chat_state
                    .update_find_query(q, &self.messages, self.show_tool_calls());
            }
            return true;
        }
        false
    }

    /// Reconciles a `ServerEvent::HistoryAppend`, returning `(visible_change, sessions_changed)`.
    fn apply_history_append_event(&mut self, ev: ServerEvent) -> (bool, bool) {
        let ServerEvent::HistoryAppend {
            ref session_id,
            ref messages,
            working,
            ref todos,
            ..
        } = ev
        else {
            return (false, false);
        };

        let is_active = self
            .selected_session
            .as_ref()
            .is_some_and(|s| s.id == *session_id);
        let added_count = if is_active {
            if self.chat_state.scroll.pending_scroll == PendingScrollTarget::None {
                let handle = &self.chat_state.scroll_handle;
                let distance = f32::from(handle.max_offset().y + handle.offset().y);
                self.chat_state.scroll.sync_position(distance);
            }
            let existing: HashSet<&str> = self.messages.iter().map(|m| m.id.as_str()).collect();
            messages
                .iter()
                .filter(|m| !existing.contains(m.id.as_str()))
                .map(|m| m.id.as_str())
                .collect::<HashSet<_>>()
                .len()
        } else {
            0
        };
        let mut visible_changed = false;
        let source_path = self
            .session_inputs()
            .into_iter()
            .find(|s| s.host == self.active_session_host() && s.summary.id == *session_id)
            .map(|s| s.summary.source_path.clone())
            .or_else(|| {
                self.selected_session
                    .as_ref()
                    .map(|s| s.source_path.clone())
            })
            .unwrap_or_default();

        let token = self
            .pending_switch_tokens
            .get(session_id)
            .cloned()
            .or_else(|| {
                self.active_switch_token
                    .as_ref()
                    .filter(|t| t.key.session_id == *session_id)
                    .cloned()
            });

        if let Some(token) = token {
            if matches!(
                self.chat_cache
                    .reconcile_server_event(&token, source_path, &ev),
                Some(ReconcileOutcome::AppliedToActive)
            ) {
                if let Some(cached) = self.chat_cache.get(&token.key) {
                    self.messages = self.preserve_failed_local_messages(cached.messages.clone());
                }
                self.chat_state.reconcile_local_reactions(&self.messages);
                if !todos.is_empty() {
                    self.chat_state
                        .session_todos
                        .insert(session_id.clone(), todos.clone());
                }
                if self.chat_state.find_open {
                    let q = self.chat_state.find_query.clone();
                    self.chat_state
                        .update_find_query(q, &self.messages, self.show_tool_calls());
                }
                visible_changed = true;
            }
        } else if self
            .selected_session
            .as_ref()
            .is_some_and(|s| s.id == *session_id)
        {
            let existing_ids: HashSet<String> =
                self.messages.iter().map(|m| m.id.clone()).collect();
            for msg in messages {
                if !existing_ids.contains(&msg.id) {
                    self.messages.push(msg.clone());
                    visible_changed = true;
                }
            }
            self.chat_state.reconcile_local_reactions(&self.messages);
            if !todos.is_empty() {
                self.chat_state
                    .session_todos
                    .insert(session_id.clone(), todos.clone());
                visible_changed = true;
            }
        }

        let mut sessions_changed = false;
        if let Some(is_working) = working {
            for s in self
                .sessions
                .iter_mut()
                .filter(|_| self.host_statuses.is_empty())
            {
                if s.id == *session_id {
                    s.working = is_working;
                    sessions_changed = true;
                }
            }
            if let Some(ref mut sel) = self.selected_session
                && sel.id == *session_id
            {
                sel.working = is_working;
                visible_changed = true;
            }
            if is_working {
                self.chat_state.advance_busy_tick();
            }
        }

        // Keep sidebar/Inbox previews live when an assistant reply arrives
        if let Some(last_asst) = messages
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::Assistant)
        {
            let preview: String = last_asst.text.trim().chars().take(120).collect();
            for s in self
                .sessions
                .iter_mut()
                .filter(|_| self.host_statuses.is_empty())
            {
                if s.id == *session_id {
                    s.last_assistant_id = Some(last_asst.id.clone());
                    if !preview.is_empty() {
                        s.last_assistant_preview = Some(preview.clone());
                    }
                    if let Some(ts) = last_asst.timestamp_ms {
                        s.last_assistant_ms = Some(ts);
                        s.updated_at_ms = s.updated_at_ms.max(ts);
                    }
                    sessions_changed = true;
                }
            }
            if let Some(ref mut sel) = self.selected_session
                && sel.id == *session_id
            {
                sel.last_assistant_id = Some(last_asst.id.clone());
                if !preview.is_empty() {
                    sel.last_assistant_preview = Some(preview);
                }
                if let Some(ts) = last_asst.timestamp_ms {
                    sel.last_assistant_ms = Some(ts);
                    sel.updated_at_ms = sel.updated_at_ms.max(ts);
                }
                visible_changed = true;
            }
        }

        if is_active && visible_changed {
            self.chat_state.scroll.appended(added_count);
        }
        (visible_changed, sessions_changed)
    }

    /// Handles daemon `ServerEvent`s fanned out from the App-scoped connection consumer (or from
    /// another `Ctrl+N` window in tests) uniformly across ALL open windows without spurious redraws.
    #[allow(dead_code)]
    pub(crate) fn handle_daemon_server_event(
        &mut self,
        event: ServerEvent,
        cx: &mut Context<Self>,
    ) {
        let host = self.active_session_host().to_string();
        self.handle_daemon_server_event_with_hint(&host, event, false, cx);
    }

    pub(crate) fn handle_daemon_server_event_with_hint(
        &mut self,
        host_label: &str,
        event: ServerEvent,
        is_older_page_hint: bool,
        cx: &mut Context<Self>,
    ) {
        self.sync_from_shared_store(cx);
        self.handle_outbox_event(host_label, &event);
        if let ServerEvent::Error { code, message } = &event
            && code == "switch_shell_client_failed"
        {
            if self.shell_host.switch_failed(message) {
                cx.notify();
            }
            return;
        }
        let matches_active_host = self
            .selected_session_host
            .as_deref()
            .is_none_or(|h| h == host_label);
        if !matches_active_host
            && matches!(
                &event,
                ServerEvent::History { .. }
                    | ServerEvent::HistoryAppend { .. }
                    | ServerEvent::SearchResults { .. }
                    | ServerEvent::PromptStatus { .. }
                    | ServerEvent::SendAccepted { .. }
                    | ServerEvent::Commands { .. }
                    | ServerEvent::Error { .. }
            )
        {
            return;
        }
        let mut changed = false;
        match event {
            ServerEvent::BrowserCommand { request_id, session_id, binding_id, command } => {
                self.handle_browser_command(host_label, &session_id, &binding_id, request_id, command);
                return;
            }
            ServerEvent::HelloAck { .. } => {
                self.rebind_browsers(host_label);
                self.flush_outbox_for_host(host_label);
                if matches_active_host && let Some(ref sel) = self.selected_session {
                    self.request_daemon_commands(sel.agent, sel.cwd.clone());
                }
            }
            ServerEvent::Sessions { .. } => {
                if self.selected_session.is_none()
                    && let Some(first) = self
                        .sessions
                        .iter()
                        .find(|s| {
                            self.sidebar_state.workspace_store.session_lifecycle(
                                &self.session_key_for(s),
                                s,
                                now_unix_ms(),
                            ) == multibot_core::workspaces::SessionLifecycle::Active
                        })
                        .cloned()
                {
                    let host = self.host_for_summary(&first);
                    self.open_session_on_host(host, first, false, cx);
                }
                self.flush_outbox_for_host(host_label);
            }
            ServerEvent::SessionUpdated { session } => {
                if matches_active_host
                    && self
                        .selected_session
                        .as_ref()
                        .is_some_and(|s| s.id == session.id && s.agent == session.agent)
                    && self.main_surface == MainSurface::Chat
                {
                    let key = self.session_key_for(&session);
                    let prev_boundary = self
                        .sidebar_state
                        .workspace_store
                        .unread_boundaries
                        .get(&key)
                        .cloned();
                    self.sidebar_state
                        .workspace_store
                        .mark_session_read(&key, &session);
                    if self
                        .sidebar_state
                        .workspace_store
                        .unread_boundaries
                        .get(&key)
                        != prev_boundary.as_ref()
                    {
                        self.persist_workspace_store_if_enabled();
                        self.sync_workspace_to_shared(cx);
                        changed = true;
                    }
                }
            }
            ServerEvent::Harnesses { choices } => {
                self.chat_state
                    .harness_choices
                    .insert(host_label.into(), choices);
                changed =
                    self.active_modal == ActiveModal::Palette && self.palette_state.provider_picker;
            }
            ServerEvent::HarnessSwitched { previous, session } => {
                self.apply_harness_switch(host_label, previous, session, cx);
                changed = true;
            }
            ServerEvent::SessionSpawned { session } => {
                if (self.pending_spawn_open
                    && self
                        .pending_spawn_host
                        .as_deref()
                        .is_none_or(|h| h == host_label))
                    || self.selected_session.is_none()
                {
                    self.pending_spawn_open = false;
                    self.pending_spawn_host = None;
                    if let Some(options) = self.chat_state.pending_spawn_options.take() {
                        let key = canonical_session_key(host_label, session.agent, &session.id);
                        self.chat_state.composer_options.insert(key, options);
                    }
                    self.open_session_on_host(host_label.to_string(), session, false, cx);
                }
            }
            ev @ ServerEvent::History { .. } => {
                changed = self.apply_history_event(ev, is_older_page_hint);
            }
            ev @ ServerEvent::HistoryAppend { .. } => {
                let (vis, _) = self.apply_history_append_event(ev);
                changed = vis;
            }
            ServerEvent::Commands {
                agent,
                cwd: _,
                items,
            } => {
                self.slash_picker_state.set_daemon_commands(agent, items);
                changed = true;
            }
            ServerEvent::SearchResults {
                request_id,
                session_id,
                hits,
                truncated,
            } => {
                if self.remote_search_request_id.as_deref() == Some(request_id.as_str())
                    && self
                        .selected_session
                        .as_ref()
                        .is_some_and(|s| s.id == session_id)
                {
                    self.remote_search_hits = hits;
                    self.remote_search_truncated = truncated;
                    let current_oldest = self
                        .active_switch_token
                        .as_ref()
                        .and_then(|t| self.chat_cache.get(&t.key))
                        .map(|c| c.oldest_offset)
                        .unwrap_or(0);
                    if self.chat_state.find_matches.is_empty()
                        && self
                            .remote_search_hits
                            .iter()
                            .any(|h| h.byte_offset < current_oldest)
                    {
                        self.load_older_history(cx);
                    }
                    changed = true;
                }
            }
            ServerEvent::SendAccepted { .. } => {
                if self.last_dispatched_prompt.is_some() {
                    self.last_dispatched_prompt = None;
                    changed = true;
                }
            }
            ServerEvent::PromptStatus {
                session_id,
                message_id,
                status,
                detail,
            } => {
                self.track_queued_prompt(&session_id, &message_id, &status);
                if matches_active_host
                    && self
                        .selected_session
                        .as_ref()
                        .is_some_and(|s| s.id == session_id)
                {
                    match status.as_str() {
                        "failed"
                            if self.last_dispatched_prompt.is_some()
                                || (!message_id.is_empty()
                                    && self.messages.iter().any(|m| m.id == message_id)) =>
                        {
                            let (msg_id, text) = if !message_id.is_empty() {
                                let matched_text = self
                                    .last_dispatched_prompt
                                    .as_ref()
                                    .filter(|(id, _)| id == &message_id)
                                    .map(|(_, t)| t.clone())
                                    .or_else(|| {
                                        self.messages
                                            .iter()
                                            .rev()
                                            .find(|m| m.id == message_id)
                                            .map(|m| m.text.clone())
                                    })
                                    .unwrap_or_default();
                                if self
                                    .last_dispatched_prompt
                                    .as_ref()
                                    .is_some_and(|(id, _)| id == &message_id)
                                {
                                    self.last_dispatched_prompt = None;
                                }
                                (message_id, matched_text)
                            } else {
                                self.last_dispatched_prompt.take().unwrap_or((
                                    format!("failed-{}", self.messages.len()),
                                    String::new(),
                                ))
                            };
                            let reason = if detail.is_empty() {
                                "Message delivery failed".to_string()
                            } else {
                                detail
                            };
                            self.record_send_failure(msg_id, text, reason, cx);
                            changed = true;
                        }
                        "cancelled" => {
                            self.notice = Some("Cancelled prompt.".to_string());
                            changed = true;
                        }
                        "sent" => {
                            self.last_dispatched_prompt = None;
                        }
                        "queued" => {
                            self.chat_state.advance_busy_tick();
                            changed = true;
                        }
                        "not_found" => {
                            if !detail.is_empty() {
                                self.notice = Some(detail);
                                changed = true;
                            }
                        }
                        _ => {}
                    }
                }
            }
            ev @ ServerEvent::Error { .. } => {
                let ServerEvent::Error {
                    ref code,
                    ref message,
                } = ev
                else {
                    unreachable!();
                };
                if (code == "read_history_failed" || code == "watch_failed")
                    && let Some(token) = self.active_switch_token.clone()
                {
                    let _ = self.chat_cache.reconcile_server_event(&token, "", &ev);
                }
                if code == "send_failed"
                    && let Some((msg_id, text)) = self.last_dispatched_prompt.take()
                {
                    self.record_send_failure(msg_id, text, message.clone(), cx);
                }
                if code == "switch_harness_failed" {
                    self.chat_state.pending_harness_switch = None;
                }
                self.notice = Some(format!("[{code}] {message}"));
                changed = true;
            }
            // Session list / archive / interrupt metadata events are already applied via SharedAppStore.
            ServerEvent::Pong { .. }
            | ServerEvent::SessionArchived { .. }
            | ServerEvent::SessionInterrupted { .. } => {}
        }
        if changed {
            self.cleanup_unmounted_surface_state();
            cx.notify();
        }
    }

    pub(crate) fn clear_active_session_view(&mut self) {
        self.chat_state.pending_harness_switch = None;
        self.voice.cancel();
        self.selected_session = None;
        self.selected_session_host = None;
        self.sidebar_state.rename = None;
        self.active_switch_token = None;
        self.messages.clear();
        self.chat_state.close_find();
        self.slash_picker_state.close();
        self.chat_state.reply_quote = None;
        self.chat_state.pending_attachments.clear();
        self.chat_state.focused_message_idx = None;
        self.chat_state.message_action_menu_open = false;
        self.chat_state.message_action_index = 0;
        self.remote_search_hits.clear();
        self.remote_search_truncated = false;
        self.cleanup_unmounted_surface_state();
    }

    pub(crate) fn select_session(&mut self, session: SessionSummary, cx: &mut Context<Self>) {
        let host = self.host_for_summary(&session);
        self.select_session_on_host(host, session, cx);
    }

    pub(crate) fn select_session_on_host(
        &mut self,
        host: String,
        session: SessionSummary,
        cx: &mut Context<Self>,
    ) {
        self.sidebar_state.pending_shell_dissolve = None;
        if self.selected_session_host.as_deref() != Some(host.as_str())
            || self
                .selected_session
                .as_ref()
                .is_none_or(|selected| selected.id != session.id || selected.agent != session.agent)
        {
            self.notice = None;
        }
        self.voice.cancel();
        if self
            .selected_session_host
            .as_deref()
            .is_some_and(|h| h != host)
        {
            self.pending_switch_tokens.clear();
            self.pending_older_page_before.clear();
            self.chat_state.session_todos.clear();
        }
        self.selected_session_host = Some(host.clone());
        self.chat_state.scroll.open_visit(UnreadVisit::Read);
        self.chat_state.scroll_handle = gpui_kit::ScrollHandle::default();
        self.chat_state.prepend_scroll_anchor = None;
        self.chat_state.newest_chord.reset();
        if session.agent == AgentKind::Shell && !session.archived {
            self.shell_host.prepare_selection(&host);
            self.select_embedded_shell(&host, &session.id);
        }
        self.chat_state.composer_menu_open = false;
        // Reset per-session transient Chat View overlays, active inline rename & stale pagination
        self.sidebar_state.rename = None;
        self.pending_older_page_before.remove(&session.id);
        self.chat_state.close_find();
        self.slash_picker_state.close();
        self.chat_state.reply_quote = None;
        self.chat_state.pending_attachments.clear();
        self.chat_state.focused_message_idx = None;
        self.chat_state.message_action_menu_open = false;
        self.chat_state.message_action_index = 0;

        let key = CacheKey::new(&host, session.agent, &session.id);

        // Frame-0 synchronous switch: Tier 1 memory (<1ms) -> Tier 2 bounded disk (<5ms)
        let (token, mem_hit) = self.chat_cache.switch_memory(key.clone());
        let cached = match mem_hit {
            Some(s) => Some(s.clone()),
            None => self.chat_cache.get_or_load(&key).cloned(),
        };
        let known_probe = cached.as_ref().and_then(|s| s.probe_sha256.clone());

        if let Some(snapshot) = cached {
            self.messages = snapshot.messages;
            if !snapshot.todos.is_empty() {
                self.chat_state
                    .session_todos
                    .insert(session.id.clone(), snapshot.todos);
            }
        } else {
            self.messages.clear();
        }
        self.chat_state.reconcile_local_reactions(&self.messages);

        self.active_switch_token = Some(token.clone());
        self.pending_switch_tokens.insert(session.id.clone(), token);
        self.selected_session = Some(session.clone());
        let full_width = self.sidebar_state.workspace_store.session_meta
            .get(&canonical_session_key(&host, session.agent, &session.id))
            .is_some_and(|meta| meta.full_width);
        self.apply_session_full_width(full_width);
        self.pending_focus_input = true;
        self.set_main_surface(MainSurface::Chat);
        self.active_modal = ActiveModal::None;
        self.remote_search_hits.clear();
        self.remote_search_truncated = false;

        if let Some(tx) = self.active_session_tx() {
            let _ = tx.send(ClientRequest::ReadHistory {
                session_id: session.id.clone(),
                source_path: session.source_path.clone(),
                agent: session.agent,
                tail: Some(200),
                before_offset: None,
                max_bytes: Some(MAX_SESSION_CACHE_BYTES),
                known_probe,
            });
            if self.sidebar_state.workspace_store.session_lifecycle(
                &self.session_key_for(&session),
                &session,
                now_unix_ms(),
            ) != multibot_core::workspaces::SessionLifecycle::Dissolved
            {
                let _ = tx.send(ClientRequest::WatchSession {
                    session_id: session.id.clone(),
                    source_path: session.source_path.clone(),
                    agent: session.agent,
                });
                let _ = tx.send(ClientRequest::ListCommands {
                    agent: session.agent,
                    cwd: session.cwd.clone(),
                });
            }
        }
        cx.notify();
    }

    /// Sends an idle session's prompt, or persists it in the shared FIFO while busy or offline.
    pub(crate) fn send_or_queue_prompt(
        &mut self,
        session: &SessionSummary,
        msg_id: String,
        text: String,
    ) {
        let host = self.active_session_host().to_string();
        self.send_or_queue_prompt_on_host(&host, session, msg_id, text);
    }

    pub(crate) fn send_or_queue_prompt_on_host(
        &mut self,
        host: &str,
        session: &SessionSummary,
        msg_id: String,
        text: String,
    ) {
        let key = canonical_session_key(host, session.agent, &session.id);
        let options = self
            .chat_state
            .composer_options
            .get(&key)
            .filter(|options| !options.is_default())
            .cloned();
        let outbox_key = ChatOutbox::session_key(host, session.agent, &session.id);
        if let Ok(mut shared) = self.shared_chat_outbox.lock() {
            let busy = session.working || shared.outbox.dispatching.contains_key(&outbox_key);
            if !busy && shared.outbox.count_for_session(&outbox_key) == 0
                && let Some(tx) = self.tx_for_host(host)
                && tx.send(ClientRequest::SendPrompt {
                    session_id: session.id.clone(),
                    agent: session.agent,
                    cwd: session.cwd.clone(),
                    text: text.clone(),
                    options: options.clone(),
                }).is_ok()
            {
                shared.outbox.mark_dispatched(&outbox_key, msg_id.clone());
                if host == self.active_session_host() && self.selected_session.as_ref()
                    .is_some_and(|selected| selected.id == session.id && selected.agent == session.agent) {
                    self.last_dispatched_prompt = Some((msg_id, text));
                }
                return;
            }
        }

        let enqueue_res = if let Ok(mut shared) = self.shared_chat_outbox.lock() {
            let res = shared.outbox.enqueue_with_options(
                &outbox_key,
                &text,
                now_unix_ms(),
                options.clone(),
            );
            if res.is_ok() {
                if shared.persist_to_disk || self.persist_outbox_to_disk {
                    let _ = shared.outbox.save();
                }
                self.chat_outbox = shared.outbox.clone();
            }
            res
        } else {
            let res = self.chat_outbox.enqueue_with_options(
                &outbox_key,
                &text,
                now_unix_ms(),
                options.clone(),
            );
            if res.is_ok() && self.persist_outbox_to_disk {
                let _ = self.chat_outbox.save();
            }
            res
        };
        match enqueue_res {
            Ok(entry) => {
                self.outbox_msg_map.insert(msg_id, entry.id);
                self.flush_outbox_for_host(host);
            }
            Err(err) => {
                self.chat_state.failed_sends.insert(
                    msg_id.clone(),
                    FailedSend {
                        message_id: msg_id,
                        text,
                        reason: err.to_string(),
                    },
                );
            }
        }
    }

    pub(crate) fn cancel_outbox_message(&mut self, msg_id: &str) {
        if let Some(outbox_id) = self.outbox_msg_map.remove(msg_id) {
            if let Ok(mut shared) = self.shared_chat_outbox.lock() {
                shared.outbox.remove_by_id(&outbox_id);
                if shared.persist_to_disk || self.persist_outbox_to_disk {
                    let _ = shared.outbox.save();
                }
                self.chat_outbox = shared.outbox.clone();
            } else {
                self.chat_outbox.remove_by_id(&outbox_id);
                if self.persist_outbox_to_disk {
                    let _ = self.chat_outbox.save();
                }
            }
        }
    }

    pub(crate) fn flush_outbox_for_current_host(&mut self) -> usize {
        let host = self.current_host_label().to_string();
        self.flush_outbox_for_host(&host)
    }

    pub(crate) fn flush_outbox_for_host(&mut self, host: &str) -> usize {
        let Some(tx) = self.tx_for_host(host).cloned() else {
            return 0;
        };
        let Ok(mut shared) = self.shared_chat_outbox.lock() else {
            return 0;
        };
        let keys: std::collections::HashSet<_> = shared.outbox.entries.iter()
            .map(|entry| entry.session_key.clone()).collect();
        let mut flushed = 0;
        for outbox_key in keys {
            let Some((entry_host, agent, sid)) = parse_outbox_session_key(&outbox_key) else {
                continue;
            };
            if entry_host != host {
                continue;
            }
            let session = if self.host_statuses.is_empty() {
                self.sessions.iter().find(|s| s.id == sid && s.agent == agent)
            } else {
                self.host_sessions.iter().find(|(h, s)| h == host && s.id == sid && s.agent == agent)
                    .map(|(_, s)| s)
            };
            let Some(session) = session else { continue; };
            let key = canonical_session_key(host, agent, sid);
            if session.archived || self.sidebar_state.workspace_store.session_meta.get(&key)
                .is_some_and(|meta| meta.dissolved_at_ms.is_some()) {
                continue;
            }
            let Some(entry) = shared.outbox.take_next_for_dispatch(&outbox_key, session.working) else {
                continue;
            };
            if (shared.persist_to_disk || self.persist_outbox_to_disk)
                && let Err(err) = shared.outbox.save() {
                shared.outbox.dispatching.remove(&outbox_key);
                shared.outbox.entries.insert(0, entry);
                self.notice = Some(err.to_string());
                break;
            }
            if tx.send(ClientRequest::SendPrompt {
                session_id: sid.into(), agent, cwd: session.cwd.clone(),
                text: entry.text.clone(), options: entry.options.clone(),
            }).is_err() {
                shared.outbox.dispatching.remove(&outbox_key);
                shared.outbox.entries.insert(0, entry);
                break;
            }
            flushed += 1;
        }
        if flushed > 0 && (shared.persist_to_disk || self.persist_outbox_to_disk) {
            let _ = shared.outbox.save();
        }
        self.outbox_msg_map.retain(|_, id| shared.outbox.entries.iter().any(|e| &e.id == id));
        self.chat_outbox = shared.outbox.clone();
        flushed
    }

    fn handle_outbox_event(&mut self, host: &str, event: &ServerEvent) {
        let updates: Vec<_> = match event {
            ServerEvent::SessionUpdated { session } => vec![(session.id.clone(), Some(session.agent), session.working)],
            ServerEvent::HistoryAppend { session_id, working: Some(working), .. } =>
                vec![(session_id.clone(), None, *working)],
            ServerEvent::Sessions { sessions } => sessions.iter()
                .map(|s| (s.id.clone(), Some(s.agent), s.working)).collect(),
            _ => return,
        };
        for (id, agent, working) in updates {
            let agent = agent.or_else(|| self.host_sessions.iter()
                .find(|(h, s)| h == host && s.id == id).map(|(_, s)| s.agent))
                .or_else(|| self.sessions.iter().find(|s| s.id == id).map(|s| s.agent));
            let Some(agent) = agent else { continue; };
            for (h, session) in &mut self.host_sessions {
                if h == host && session.id == id && session.agent == agent {
                    session.working = working;
                }
            }
            if self.host_statuses.is_empty() {
                for session in &mut self.sessions {
                    if session.id == id && session.agent == agent { session.working = working; }
                }
            }
            if self.active_session_host() == host
                && let Some(session) = self.selected_session.as_mut()
                && session.id == id && session.agent == agent {
                session.working = working;
            }
        }
        self.flush_outbox_for_host(host);
    }

    pub(crate) fn remove_queued_entry(&mut self, id: &str) -> Option<multibot_core::outbox::OutboxEntry> {
        let mut shared = self.shared_chat_outbox.lock().ok()?;
        let entry = shared.outbox.remove_by_id(id)?;
        if shared.persist_to_disk || self.persist_outbox_to_disk {
            if let Err(err) = shared.outbox.save() {
                self.notice = Some(err.to_string());
            }
        }
        self.chat_outbox = shared.outbox.clone();
        let message_ids: Vec<_> = self.outbox_msg_map.iter()
            .filter(|(_, entry_id)| entry_id.as_str() == id).map(|(id, _)| id.clone()).collect();
        self.messages.retain(|message| !message_ids.contains(&message.id));
        self.outbox_msg_map.retain(|_, entry_id| entry_id != id);
        Some(entry)
    }

    pub(crate) fn force_send_queued_entry(&mut self, id: &str, cx: &mut Context<Self>) {
        let entry = self.shared_chat_outbox.lock().ok()
            .and_then(|shared| shared.outbox.entries.iter().find(|e| e.id == id).cloned());
        let Some(entry) = entry else { return; };
        let Some((host, agent, sid)) = parse_outbox_session_key(&entry.session_key) else { return; };
        let key = canonical_session_key(host, agent, sid);
        let Some((_, session)) = self.host_for_session_key(&key) else { return; };
        let Some(tx) = self.tx_for_host(host).cloned() else { return; };
        let Ok(mut shared) = self.shared_chat_outbox.lock() else { return; };
        let Some(entry) = shared.outbox.remove_by_id(id) else { return; };
        shared.outbox.mark_dispatched(&entry.session_key, entry.id.clone());
        if (shared.persist_to_disk || self.persist_outbox_to_disk)
            && let Err(err) = shared.outbox.save() {
            shared.outbox.dispatching.remove(&entry.session_key);
            shared.outbox.entries.insert(0, entry);
            self.chat_outbox = shared.outbox.clone();
            self.notice = Some(err.to_string());
            cx.notify();
            return;
        }
        let sent = tx.send(ClientRequest::InterruptSession { session_id: sid.into(), agent }).is_ok()
            && tx.send(ClientRequest::SendPrompt {
                session_id: sid.into(), agent, cwd: session.cwd,
                text: entry.text.clone(), options: entry.options.clone(),
            }).is_ok();
        if !sent {
            shared.outbox.dispatching.remove(&entry.session_key);
            shared.outbox.entries.insert(0, entry);
            self.notice = Some("Could not force send. Message remains queued.".into());
        }
        if shared.persist_to_disk || self.persist_outbox_to_disk {
            if let Err(err) = shared.outbox.save() { self.notice = Some(err.to_string()); }
        }
        self.chat_outbox = shared.outbox.clone();
        cx.notify();
    }

    pub(crate) fn default_spawn_cwd(&self) -> String {
        self.selected_session
            .as_ref()
            .and_then(|s| s.cwd.clone())
            .filter(|c| !c.trim().is_empty())
            .unwrap_or_else(|| ".".to_string())
    }

    pub(crate) fn interrupt_active_session(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(ref sess) = self.selected_session else {
            return false;
        };
        let Some(tx) = self.active_session_tx().cloned() else {
            self.notice = Some("Connect to a host first.".to_string());
            cx.notify();
            return false;
        };
        let ok = tx
            .send(ClientRequest::InterruptSession {
                session_id: sess.id.clone(),
                agent: sess.agent,
            })
            .is_ok();
        if ok {
            self.notice = Some(format!("Stop requested for {}.", sess.title));
        }
        cx.notify();
        ok
    }

    #[allow(dead_code)]
    pub(crate) fn steer_active_session(&mut self, text: String, cx: &mut Context<Self>) -> bool {
        let Some(ref sess) = self.selected_session else {
            return false;
        };
        let Some(tx) = self.active_session_tx().cloned() else {
            return false;
        };
        let ok = tx
            .send(ClientRequest::SteerSession {
                session_id: sess.id.clone(),
                agent: sess.agent,
                cwd: sess.cwd.clone(),
                text,
                options: self.composer_options_for(sess),
            })
            .is_ok();
        cx.notify();
        ok
    }

    pub(crate) fn spawn_new_session(
        &mut self,
        agent: AgentKind,
        cwd: String,
        title: Option<String>,
        initial_prompt: Option<String>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(tx) = self.active_session_tx().cloned() else {
            self.notice = Some("Connect to a host before starting a new session.".to_string());
            cx.notify();
            return false;
        };
        let options = self.spawn_composer_options(agent);
        let ok = tx
            .send(ClientRequest::SpawnSession {
                agent,
                cwd,
                title,
                initial_prompt,
                options: options.clone(),
            })
            .is_ok();
        if ok {
            self.pending_spawn_open = true;
            self.chat_state.pending_spawn_options = options;
            self.pending_spawn_host = Some(self.active_session_host().to_string());
            self.notice = Some(format!("Starting new {} session...", agent.label()));
        }
        cx.notify();
        ok
    }

    pub(crate) fn unarchive_session_on_host(
        &mut self,
        host: &str,
        session_id: String,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(tx) = self.tx_for_host(host) else {
            return false;
        };
        let ok = tx
            .send(ClientRequest::UnarchiveSession { session_id })
            .is_ok();
        cx.notify();
        ok
    }

    pub(crate) fn search_active_session_history(
        &mut self,
        query: String,
        cx: &mut Context<Self>,
    ) -> bool {
        let trimmed = query.trim();
        if trimmed.is_empty() || trimmed.chars().count() > 200 {
            return false;
        }
        let Some(ref sess) = self.selected_session else {
            return false;
        };
        let Some(tx) = self.active_session_tx().cloned() else {
            return false;
        };
        self.next_search_request_seq = self.next_search_request_seq.wrapping_add(1);
        let request_id = format!("search-{}", self.next_search_request_seq);
        self.remote_search_request_id = Some(request_id.clone());
        let ok = tx
            .send(ClientRequest::SearchHistory {
                request_id,
                session_id: sess.id.clone(),
                source_path: sess.source_path.clone(),
                agent: sess.agent,
                query: trimmed.to_string(),
            })
            .is_ok();
        cx.notify();
        ok
    }

    pub(crate) fn has_unloaded_older_history(&self) -> bool {
        self.active_switch_token
            .as_ref()
            .and_then(|t| self.chat_cache.get(&t.key))
            .is_some_and(|c| c.has_more && c.oldest_offset > 0)
    }

    pub(crate) fn load_older_history(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(ref sess) = self.selected_session else {
            return false;
        };
        let Some(tx) = self.active_session_tx().cloned() else {
            return false;
        };
        let Some(token) = self.active_switch_token.as_ref() else {
            return false;
        };
        let Some(cached) = self.chat_cache.get(&token.key) else {
            return false;
        };
        if !cached.has_more || cached.oldest_offset == 0 {
            return false;
        }
        let before = cached.oldest_offset;
        let sess_id = sess.id.clone();
        let ok = tx
            .send(ClientRequest::ReadHistory {
                session_id: sess_id.clone(),
                source_path: sess.source_path.clone(),
                agent: sess.agent,
                tail: Some(200),
                before_offset: Some(before),
                max_bytes: Some(MAX_SESSION_CACHE_BYTES),
                // `cached.probe_sha256` is the EOF probe at `latest_offset`, NOT at `before_offset`.
                // Passing `None` ensures `multibotd` honors `before_offset` cleanly.
                known_probe: None,
            })
            .is_ok();
        if ok {
            self.pending_older_page_before
                .insert(sess_id.clone(), before);
            if cx.has_global::<SharedAppStore>() {
                let store = cx.global_mut::<SharedAppStore>();
                let queue = store
                    .pending_older_page_before
                    .entry(if self.host_statuses.is_empty() {
                        sess_id
                    } else {
                        format!(
                            "{}:{}{}",
                            self.active_session_host().len(),
                            self.active_session_host(),
                            sess_id
                        )
                    })
                    .or_default();
                if queue.len() >= 64 {
                    queue.pop_front();
                }
                queue.push_back(before);
            }
        }
        cx.notify();
        ok
    }

    pub(crate) fn request_daemon_commands(&self, agent: AgentKind, cwd: Option<String>) {
        if let Some(tx) = self.active_session_tx() {
            let _ = tx.send(ClientRequest::ListCommands { agent, cwd });
        }
    }

    // =========================================================================
    // Workspace-Ordered Session Jumping & Cycling
    // =========================================================================

    pub(crate) fn active_ctrl_digit_order(&self) -> Vec<String> {
        let inputs = self.session_inputs();
        let active_key = self
            .selected_session
            .as_ref()
            .map(|s| self.session_key_for(s));
        let layout = self
            .sidebar_state
            .workspace_store
            .compute_sidebar_layout_with_options(
                &self.sidebar_state.active_workspace_id,
                self.sidebar_state.active_workspace_view,
                &inputs,
                self.sidebar_state.unread_only,
                active_key.as_deref(),
                now_unix_ms(),
            );
        layout.ctrl_digit_order
    }

    pub(crate) fn jump_session_index(&mut self, index: usize, cx: &mut Context<Self>) {
        let order = self.active_ctrl_digit_order();
        if let Some(key) = order.get(index) {
            self.open_session_key_with_tracking(key, false, cx);
            self.sync_workspace_to_shared(cx);
        } else if order.is_empty()
            && let Some(session) = self.sessions.get(index).cloned()
        {
            self.open_session_with_tracking(session, false, cx);
            self.sync_workspace_to_shared(cx);
        }
    }

    pub(crate) fn cycle_session(&mut self, delta: isize, cx: &mut Context<Self>) {
        let order = self.active_ctrl_digit_order();
        if order.is_empty() {
            return;
        }
        let active_key = self
            .selected_session
            .as_ref()
            .map(|s| self.session_key_for(s));
        let current = active_key
            .as_deref()
            .and_then(|k| order.iter().position(|x| x == k));
        let next = current
            .map(|current| (current as isize + delta).rem_euclid(order.len() as isize) as usize)
            .unwrap_or(if delta < 0 { order.len() - 1 } else { 0 });
        if let Some((host, session)) = self.host_for_session_key(&order[next]) {
            if active_key.as_deref() != Some(order[next].as_str()) {
                self.sidebar_state.previous_session_key = active_key;
                // Frequent navigation must not change the ranking it is traversing.
                if self.sidebar_state.active_workspace_view != WorkspaceView::Frequent {
                    self.sidebar_state
                        .workspace_store
                        .record_frequent_visit(&order[next], now_unix_ms());
                    self.persist_workspace_store_if_enabled();
                    self.sync_workspace_to_shared(cx);
                }
            }
            self.inbox_state.step_index = None;
            self.inbox_state.step_order.clear();
            // Tracking a view changes MRU order; marking read also removes rows in unread-only mode.
            self.select_session_on_host(host, session, cx);
        }
    }

    pub(crate) fn open_extra_window(&self, cx: &mut App) {
        let bounds = Bounds::centered(None, size(px(1140.0), px(740.0)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some(format!("{} — Workspace", crate::window_title()).into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let rt = self.runtime.clone();
        let persist_to_disk = cx
            .try_global::<SharedAppStore>()
            .is_some_and(|s| s.persist_to_disk);
        let _ = gpui_kit::open_window(options, cx, move |window, cx| {
            cx.new(|cx| {
                let mut view = MultibotView::new(rt, window, cx);
                if persist_to_disk {
                    view.enable_disk_persistence();
                }
                view
            })
        });
    }

    pub(crate) fn find_bundled_linux_daemon() -> Option<PathBuf> {
        multibot_core::daemon_client::discover_bundled_linux_daemon()
    }

    pub(crate) fn find_local_daemon() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let dir = exe.parent()?;
        #[cfg(target_os = "windows")]
        let name = "multibotd.exe";
        #[cfg(not(target_os = "windows"))]
        let name = "multibotd";
        let candidate = dir.join(name);
        candidate.is_file().then_some(candidate)
    }

    pub(crate) fn agent_color(agent: AgentKind) -> gpui_kit::Hsla {
        match agent {
            AgentKind::Claude => gpui_kit::rgb(0xD97757).into(),
            AgentKind::Antigravity => gpui_kit::rgb(0x60A5FA).into(),
            AgentKind::Codex => gpui_kit::rgb(0x34D399).into(),
            AgentKind::Shell => gpui_kit::rgb(0xE2E8F0).into(),
            AgentKind::Pi => gpui_kit::rgb(0xC084FC).into(),
            AgentKind::Cursor => gpui_kit::rgb(0xFACC15).into(),
        }
    }

    pub(crate) fn selected_inbox_session_key(&self) -> Option<String> {
        let rows = self.compute_inbox_rows(now_unix_ms());
        rows.get(self.inbox_state.selected_row)
            .or_else(|| rows.first())
            .map(|r| r.session_key.clone())
    }

    pub(crate) fn target_triage_session_key(&self) -> Option<String> {
        if self.main_surface == MainSurface::Inbox {
            self.selected_inbox_session_key()
        } else {
            self.selected_session
                .as_ref()
                .map(|s| self.session_key_for(s))
        }
    }

    /// Returns `true` when the floating top header bar is drawn for the current surface.
    /// The Inbox is keyboard-first: it hides the whole floating header (title pill and the
    /// Inbox / menu buttons) because the user already knows where they are.
    pub(crate) fn top_floating_bar_rendered(&self) -> bool {
        self.main_surface != MainSurface::Inbox
    }

    pub(crate) fn render_top_floating_bar(
        &self,
        title: String,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let header_rename_draft =
            if self.sidebar_state.mode != SidebarMode::Open && self.is_sidebar_rename_mounted() {
                self.sidebar_state
                    .rename
                    .as_ref()
                    .map(InlineRenameState::render_text)
            } else {
                None
            };

        h_flex()
            .w_full()
            .px_5()
            .pt_3()
            .pb_2()
            .items_center()
            .justify_between()
            .child(div().w(px(76.0)).flex_shrink_0())
            .child(
                gpui_kit::component::v_flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap_1()
                    .child(if let Some(draft) = header_rename_draft {
                        div()
                            .id("header-rename-input")
                            .test_support()
                            .px_4()
                            .py_1p5()
                            .rounded(RADIUS_CHIP)
                            .bg(theme::bg_surface())
                            .border_1()
                            .border_color(theme::accent_blue())
                            .text_sm()
                            .text_color(theme::text_primary())
                            .child(draft)
                    } else {
                        div()
                            .id("header-title-pill")
                            .test_support()
                            .cursor_pointer()
                            .hover(|s| s.bg(theme::bg_hover()))
                            .tooltip(|window, cx| {
                                gpui_kit::component::tooltip::Tooltip::new("Show chat info")
                                    .build(window, cx)
                            })
                            .on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.toggle_chat_info(window, cx)
                                }),
                            )
                            .px_4()
                            .py_1p5()
                            .rounded(RADIUS_CHIP)
                            .bg(theme::bg_surface())
                            .border_1()
                            .border_color(theme::border_subtle())
                            .text_sm()
                            .text_color(theme::text_primary())
                            .max_w_full()
                            .truncate()
                            .child(title)
                    })
                    .when_some(self.render_update_pill(cx), |el, pill| el.child(pill))
                    .when(
                        self.main_surface == MainSurface::Chat
                            && self.selected_session.as_ref().is_some_and(|s| s.working),
                        |el| {
                            el.child(
                                div()
                                    .text_xs()
                                    .text_color(theme::text_muted())
                                    .child("Agent is working"),
                            )
                        },
                    ),
            )
            .child(
                h_flex()
                    .w(px(76.0))
                    .flex_shrink_0()
                    .justify_end()
                    .items_center()
                    .gap_1p5()
                    .child({
                        Button::new("toggle-inbox")
                            .xsmall()
                            .icon(lucide_icon(icons::INBOX, 16.0))
                            .accessibility_label("Inbox")
                            .tooltip("Inbox · Alt+I")
                            .on_click(cx.listener(|this, _, window, cx| {
                                if this.table_modal_state.is_open() {
                                    this.table_modal_state.close();
                                }
                                this.active_modal = ActiveModal::None;
                                this.set_main_surface(MainSurface::Inbox);
                                this.inbox_state.select_all_split();
                                this.focus_inbox(window, cx);
                                cx.notify();
                            }))
                    })
                    .when(
                        self.sidebar_state
                            .workspace_store
                            .inbox_unread_count_at(&self.session_inputs(), now_unix_ms())
                            > 0,
                        |el| {
                            el.child(
                                div()
                                    .w(px(5.0))
                                    .h(px(5.0))
                                    .rounded(RADIUS_CHIP)
                                    .bg(theme::accent_blue()),
                            )
                        },
                    )
                    .child(
                        Button::new("chat-menu")
                            .xsmall()
                            .icon(lucide_icon(icons::MORE_HORIZONTAL, 16.0))
                            .accessibility_label("Chat menu")
                            .tooltip("Chat menu")
                            .on_click(cx.listener(|this, _, window, cx| {
                                if let Some(session) = &this.selected_session {
                                    this.open_palette_tab_context(
                                        this.session_key_for(session),
                                        window,
                                        cx,
                                    );
                                } else {
                                    this.open_palette(window, cx);
                                }
                            })),
                    ),
            )
    }

    /// Computes the current conditional-subtree visibility signature for structural focus recovery.
    fn focus_layout_signature(&self) -> FocusLayoutSignature {
        let update_card_visible = self.update_ui.is_card_visible(
            self.settings.sidebar_mode,
            self.sidebar_state.is_shown(),
            self.settings.dismissed_update_version.as_deref(),
        );
        let update_card_kind = if !update_card_visible {
            0
        } else {
            match &self.update_ui.state {
                UpdateState::Available(_) | UpdateState::ReadyToRestart(_) => 1,
                UpdateState::Downloading { .. } => 2,
                UpdateState::Verifying { .. } | UpdateState::Applying { .. } => 3,
                UpdateState::Error {
                    retryable: false, ..
                } => 4,
                UpdateState::Error {
                    retryable: true, ..
                } => 5,
                UpdateState::Idle | UpdateState::Checking | UpdateState::UpToDate => 0,
            }
        };
        let answered_question_count = self.chat_state.answered_prompts.len()
            + self
                .chat_state
                .answered_questions
                .values()
                .map(HashSet::len)
                .sum::<usize>();
        FocusLayoutSignature {
            main_surface: self.main_surface,
            active_modal: self.active_modal,
            sidebar_mode: self.sidebar_state.mode,
            table_modal_open: self.table_modal_state.is_open(),
            find_open: self.main_surface == MainSurface::Chat && self.chat_state.find_open,
            update_card_kind,
            selected_session_id: self.selected_session.as_ref().map(|s| s.id.clone()),
            slash_picker_open: self.main_surface == MainSurface::Chat
                && self.slash_picker_state.is_open(),
            rename_active: self.is_sidebar_rename_mounted() || self.is_inbox_rename_mounted(),
            sidebar_workspace_draft_active: self.is_sidebar_workspace_draft_mounted(),
            reply_quote_active: self.main_surface == MainSurface::Chat
                && self.chat_state.reply_quote.is_some(),
            has_pending_attachments: self.main_surface == MainSurface::Chat
                && !self.chat_state.pending_attachments.is_empty(),
            message_action_menu_open: self.main_surface == MainSurface::Chat
                && self.chat_state.message_action_menu_open,
            focused_message_idx: if self.main_surface == MainSurface::Chat {
                self.chat_state.focused_message_idx
            } else {
                None
            },
            message_count: self.messages.len(),
            answered_question_count,
            gpu_adapter_count: self.gpu_report.discovered.len(),
            terminal_open: self.terminal.is_open(),
        }
    }

    /// Returns `true` when focus is currently on one of the known view-owned `FocusHandle`s
    /// whose subtree is mounted in the current render state.
    pub(crate) fn is_focused_on_mounted_known_handle(&self, window: &Window, cx: &App) -> bool {
        self.root_focus_handle.is_focused(window)
            || (self.active_modal == ActiveModal::Palette
                && self.palette_input.read(cx).focus_handle(cx).is_focused(window))
            || (self.active_modal == ActiveModal::Shortcuts
                && self
                    .shortcuts_search_input
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window))
            || (self.table_modal_state.is_open()
                && self
                    .table_modal_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window)))
            || (self.main_surface == MainSurface::Inbox
                && self
                    .inbox_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window)))
            || (self.main_surface == MainSurface::Chat
                && (self
                    .chat_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window))
                    || self
                        .prompt_input
                        .read(cx)
                        .focus_handle(cx)
                        .is_focused(window)))
            || (self.main_surface == MainSurface::Settings
                && self.settings_focus_handle.is_focused(window))
            || (self.main_surface == MainSurface::Settings
                && self.settings_tab == crate::settings::SettingsTab::Hosts
                && self.host_input.read(cx).focus_handle(cx).is_focused(window))
            || (self.is_sidebar_inline_edit_mounted()
                && !self.has_mounted_chat_overlay()
                && self
                    .sidebar_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window)))
    }

    /// Restores focus to the active surface's default mounted `FocusHandle`:
    /// - `table_modal_state.focus_handle` if the expanded table modal is open
    /// - `palette_input` if the Command Palette modal is open
    /// - `shortcuts_search_input` if the Shortcuts modal is open
    /// - `prompt_input` for chat sessions, or the native input for shell sessions
    /// - `chat_state.focus_handle` (`#chat-panel-root`) if on `MainSurface::Chat` with no modal
    ///   open and Chat Find or Message Navigation / Action Menu is active
    /// - `sidebar_state.focus_handle` if no modal/chat overlay is active and an Open-sidebar
    ///   inline rename or new-workspace draft is visibly mounted
    /// - `inbox_state.focus_handle` if on `MainSurface::Inbox` with no modal open
    /// - `root_focus_handle` otherwise
    pub(crate) fn focus_active_surface_default(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.table_modal_state.is_open() {
            self.focus_table_modal(window, cx);
        } else if self.active_modal == ActiveModal::Palette {
            self.palette_input.update(cx, |state, cx| {
                state.focus(window, cx);
            });
        } else if self.active_modal == ActiveModal::Shortcuts {
            self.shell_host.focus_parent();
            self.shortcuts_search_input
                .update(cx, |state, cx| state.focus(window, cx));
        } else if self.active_modal == ActiveModal::None
            && self.main_surface == MainSurface::Settings
        {
            window.focus(&self.settings_focus_handle, cx);
        } else if self.active_modal == ActiveModal::None
            && self.main_surface == MainSurface::Chat
            && self.slash_picker_state.is_open()
        {
            self.prompt_input.update(cx, |state, cx| {
                state.focus(window, cx);
            });
        } else if self.active_modal == ActiveModal::None
            && self.main_surface == MainSurface::Chat
            && (self.chat_state.find_open
                || self.chat_state.message_action_menu_open
                || self.chat_state.focused_message_idx.is_some())
        {
            self.focus_chat_panel(window, cx);
        } else if self.active_modal == ActiveModal::None
            && !self.has_mounted_chat_overlay()
            && self.is_sidebar_inline_edit_mounted()
        {
            self.focus_sidebar(window, cx);
        } else if self.main_surface == MainSurface::Inbox && self.active_modal == ActiveModal::None
        {
            self.focus_inbox(window, cx);
        } else if self.main_surface == MainSurface::Chat && self.active_modal == ActiveModal::None {
            if let Some(open) = self.computer.open() {
                self.shell_host.focus_parent();
                open.url_input
                    .update(cx, |state, cx| state.focus(window, cx));
            } else if let Some(session) = &self.selected_session {
                if session.agent == AgentKind::Shell {
                    let host = self.active_session_host().to_string();
                    self.focus_chat_panel(window, cx);
                    self.shell_host.focus_session(&host);
                } else {
                    self.shell_host.focus_parent();
                    self.prompt_input
                        .update(cx, |state, cx| state.focus(window, cx));
                }
            } else {
                window.focus(&self.root_focus_handle, cx);
            }
        } else {
            window.focus(&self.root_focus_handle, cx);
        }
    }

    /// Returns `true` when focus is currently stranded on a child `FocusHandle` whose subtree
    /// is unmounted in the current state (which would otherwise cause GPUI to fall back to
    /// `gpui_base::Root` above `#multibot-root`, breaking all global shortcuts).
    fn has_focused_unmounted_child(&self, window: &Window, cx: &Context<Self>) -> bool {
        (self.active_modal != ActiveModal::Shortcuts
            && self
                .shortcuts_search_input
                .read(cx)
                .focus_handle(cx)
                .is_focused(window))
            || (self.active_modal != ActiveModal::Palette
                && self
                    .palette_input
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window))
            || (!self.table_modal_state.is_open()
                && self
                    .table_modal_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window)))
            || (self.main_surface != MainSurface::Inbox
                && self
                    .inbox_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window)))
            || (self.main_surface != MainSurface::Chat
                && (self
                    .chat_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window))
                    || self
                        .prompt_input
                        .read(cx)
                        .focus_handle(cx)
                        .is_focused(window)))
            || (self.main_surface != MainSurface::Settings
                && self.settings_focus_handle.is_focused(window))
            || ((self.main_surface != MainSurface::Settings
                || self.settings_tab != crate::settings::SettingsTab::Hosts)
                && self.host_input.read(cx).focus_handle(cx).is_focused(window))
            || ((self.sidebar_state.mode != SidebarMode::Open || !self.sidebar_rendered())
                && self
                    .sidebar_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window)))
            || (!self.terminal.is_open()
                && self
                    .terminal
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window)))
    }
}

impl Render for MultibotView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.active_modal == ActiveModal::None
            && (self.sidebar_state.rename.is_some()
                || self.sidebar_state.new_workspace_draft.is_some())
            && (self
                .prompt_input
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
                || self.host_input.read(cx).focus_handle(cx).is_focused(window))
        {
            self.sidebar_state.rename = None;
            self.sidebar_state.new_workspace_draft = None;
        }
        #[cfg(windows)]
        self.sync_browser_pane(window, cx);
        self.sync_embedded_shell(window, cx);
        let current_sig = self.focus_layout_signature();
        let prev_sig = self.last_focus_layout_sig.replace(current_sig.clone());
        let layout_changed = prev_sig.as_ref().is_some_and(|prev| prev != &current_sig);
        if self.active_modal == ActiveModal::Shortcuts
            && prev_sig
                .as_ref()
                .is_none_or(|prev| prev.active_modal != ActiveModal::Shortcuts)
        {
            self.shortcuts_search_input
                .update(cx, |state, cx| state.set_value("", window, cx));
            self.focus_active_surface_default(window, cx);
        }
        if std::mem::take(&mut self.pending_focus_input)
            && self.main_surface == MainSurface::Chat
            && self.active_modal == ActiveModal::None
            && !self.table_modal_state.is_open()
            && !self.chat_state.find_open
            && self.chat_state.focused_message_idx.is_none()
            && !self.chat_state.message_action_menu_open
            && self.sidebar_state.rename.is_none()
            && self.sidebar_state.new_workspace_draft.is_none()
            && !self.computer.is_open()
        {
            self.focus_active_surface_default(window, cx);
        }
        let stale_sidebar_focus = self.active_modal == ActiveModal::None
            && !self.table_modal_state.is_open()
            && (!self.is_sidebar_inline_edit_mounted() || self.has_mounted_chat_overlay())
            && self
                .sidebar_state
                .focus_handle
                .as_ref()
                .is_some_and(|fh| fh.is_focused(window));
        if self.has_focused_unmounted_child(window, cx)
            || stale_sidebar_focus
            || (layout_changed
                && (!self.is_focused_on_mounted_known_handle(window, cx)
                    || window.focused(cx).is_none()))
        {
            self.focus_active_surface_default(window, cx);
        }
        #[cfg(windows)]
        let browser_pane = self.render_browser_pane(cx);
        div()
            .id("multibot-root")
            .track_focus(&self.root_focus_handle)
            .on_mouse_down(
                gpui_kit::MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.shell_host.focus_parent();
                }),
            )
            .relative()
            .size_full()
            .bg(theme::bg_canvas())
            .text_color(theme::text_primary())
            .on_modifiers_changed(cx.listener(|this, ev: &ModifiersChangedEvent, _, cx| {
                this.handle_modifiers_changed(ev, cx);
            }))
            .capture_action(cx.listener(|this, _: &CloseOverlay, _, cx| {
                if this.sidebar_state.dev_stats_expanded
                    && this.active_modal == ActiveModal::None
                    && !this.table_modal_state.is_open()
                {
                    this.sidebar_state.dev_stats_expanded = false;
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .capture_action(
                cx.listener(|this, _: &gpui_kit::component::input::Escape, _, cx| {
                    if this.sidebar_state.dev_stats_expanded
                        && this.active_modal == ActiveModal::None
                        && !this.table_modal_state.is_open()
                    {
                        this.sidebar_state.dev_stats_expanded = false;
                        cx.stop_propagation();
                        cx.notify();
                    }
                }),
            )
            .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                if this.active_modal == ActiveModal::Shortcuts
                    && !this.table_modal_state.is_open()
                    && ev.keystroke.key == "escape"
                {
                    this.active_modal = ActiveModal::None;
                    this.focus_active_surface_default(window, cx);
                    cx.stop_propagation();
                    window.prevent_default();
                    cx.notify();
                    return;
                }
                if this.handle_lifecycle_keydown(ev, window, cx) {
                    return;
                }
                // Top-pill inline rename fallback when sidebar is Collapsed/Hidden (`F2`).
                // `#header-rename-input` is only mounted on `MainSurface::Chat` for the active session.
                if this.active_modal == ActiveModal::None
                    && !this.table_modal_state.is_open()
                    && !this.has_mounted_chat_overlay()
                    && this.sidebar_state.mode != SidebarMode::Open
                    && this.is_sidebar_rename_mounted()
                {
                    let outcome = this.sidebar_state.rename.as_mut().map(|rename| {
                        rename.handle_key(
                            &ev.keystroke.key,
                            ev.keystroke.key_char.as_deref(),
                            ev.keystroke.modifiers,
                        )
                    });
                    match outcome {
                        Some(RenameKeyOutcome::Commit) => {
                            if let Some(rename) = this.sidebar_state.rename.take() {
                                this.commit_session_rename(&rename.session_key, &rename.draft, cx);
                            }
                            cx.stop_propagation();
                            window.prevent_default();
                        }
                        Some(RenameKeyOutcome::Cancel) => {
                            this.sidebar_state.rename = None;
                            cx.stop_propagation();
                            window.prevent_default();
                            cx.notify();
                        }
                        Some(RenameKeyOutcome::Consumed) => {
                            cx.stop_propagation();
                            window.prevent_default();
                            cx.notify();
                        }
                        Some(RenameKeyOutcome::Ignored) | None => {}
                    }
                }
            }))
            // =================================================================
            // Slice 1 Actions
            // =================================================================
            .on_action(cx.listener(|this, _: &OpenPalette, window, cx| {
                if this.table_modal_state.is_open() {
                    this.table_modal_state.close();
                }
                this.open_palette(window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenWorkspaces, window, cx| {
                this.open_workspace_switcher(window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenInbox, window, cx| {
                if this.table_modal_state.is_open() {
                    this.table_modal_state.close();
                }
                this.active_modal = ActiveModal::None;
                this.sidebar_state.rename = None;
                this.sidebar_state.new_workspace_draft = None;
                this.set_main_surface(MainSurface::Inbox);
                this.inbox_state.select_all_split();
                this.inbox_state.lifecycle = multibot_core::workspaces::SessionLifecycle::Active;
                this.focus_inbox(window, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &OpenDone, window, cx| {
                this.open_lifecycle_view(
                    multibot_core::workspaces::SessionLifecycle::Done,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &OpenHibernated, window, cx| {
                this.open_lifecycle_view(
                    multibot_core::workspaces::SessionLifecycle::Hibernated,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &OpenDissolved, window, cx| {
                this.open_lifecycle_view(
                    multibot_core::workspaces::SessionLifecycle::Dissolved,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &ToggleInbox, window, cx| {
                if this.table_modal_state.is_open() {
                    this.table_modal_state.close();
                }
                this.active_modal = ActiveModal::None;
                if this.main_surface == MainSurface::Inbox {
                    this.set_main_surface(MainSurface::Chat);
                    this.focus_active_surface_default(window, cx);
                } else {
                    this.set_main_surface(MainSurface::Inbox);
                    this.focus_inbox(window, cx);
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleSettings, window, cx| {
                if this.table_modal_state.is_open() {
                    this.table_modal_state.close();
                }
                this.active_modal = ActiveModal::None;
                let next = if this.main_surface == MainSurface::Settings {
                    MainSurface::Chat
                } else {
                    MainSurface::Settings
                };
                this.set_main_surface(next);
                this.focus_active_surface_default(window, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleFullscreen, _, cx| {
                if this.active_modal == ActiveModal::None && !this.table_modal_state.is_open() {
                    this.toggle_shell_fullscreen(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, window, cx| {
                this.cycle_sidebar_mode_synced(cx);
                if this.active_modal == ActiveModal::None
                    && !this.table_modal_state.is_open()
                    && !this.has_mounted_chat_overlay()
                    && this.is_sidebar_inline_edit_mounted()
                {
                    this.focus_sidebar(window, cx);
                } else if this.sidebar_state.mode != SidebarMode::Open
                    && !this.is_focused_on_mounted_known_handle(window, cx)
                {
                    this.focus_active_surface_default(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleBrowserPane, window, cx| {
                this.toggle_browser_pane(window, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusBrowserPane, window, cx| {
                this.focus_browser_pane(window, cx);
            }))
            .on_action(cx.listener(|this, _: &CaptureBrowserPane, window, cx| {
                this.capture_browser_pane(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleTerminal, window, cx| {
                // Modals own the keyboard; the pane never opens over one.
                if this.active_modal != ActiveModal::None || this.table_modal_state.is_open() {
                    return;
                }
                // Alt+C always wins: it opens the pane, or closes it even while the grid
                // has focus, dropping the PTY and its child process immediately.
                this.toggle_terminal_pane(cx);
                if this.terminal.is_open() {
                    if let Some(focus_handle) = this.terminal.focus_handle.clone() {
                        window.focus(&focus_handle, cx);
                    }
                } else {
                    this.focus_active_surface_default(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &NewWindow, _, cx| {
                this.open_extra_window(cx);
            }))
            .on_action(cx.listener(|this, _: &OpenShortcuts, window, cx| {
                if this.table_modal_state.is_open() {
                    this.table_modal_state.close();
                }
                this.active_modal = match this.active_modal {
                    ActiveModal::Shortcuts => ActiveModal::None,
                    _ => ActiveModal::Shortcuts,
                };
                #[cfg(windows)]
                if this.active_modal == ActiveModal::Shortcuts {
                    this.hide_browser_pane();
                }
                this.focus_active_surface_default(window, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &VoiceDictation, window, cx| {
                this.toggle_dictation(window, cx);
            }))
            .on_action(cx.listener(|this, _: &CloseOverlay, window, cx| {
                if this.sidebar_state.pending_shell_dissolve.take().is_some() {
                    this.notice = None;
                } else if this.active_modal == ActiveModal::ChatInfo {
                    this.active_modal = ActiveModal::None;
                    this.focus_active_surface_default(window, cx);
                } else if this.voice.phase != multibot_core::voice::DictationPhase::Off {
                    this.voice.cancel();
                } else if this.table_modal_state.is_open() {
                    // 1. Topmost overlay: expanded table modal
                    this.table_modal_state.close();
                    this.focus_active_surface_default(window, cx);
                } else if this.active_modal != ActiveModal::None {
                    // 2. Modal dialogs (Command Palette / F1 Shortcuts)
                    this.active_modal = ActiveModal::None;
                    this.palette_state.mode = multibot_core::workspaces::PaletteMode::Global;
                    this.palette_state.reminder_target = None;
                    this.palette_state.dissolve_workspace_id = None;
                    this.focus_active_surface_default(window, cx);
                } else if this.chat_state.composer_menu_open {
                    this.chat_state.composer_menu_open = false;
                    this.focus_active_surface_default(window, cx);
                } else if this.main_surface == MainSurface::Chat
                    && this.slash_picker_state.is_open()
                {
                    // 3. Slash / skill autocomplete popover (Chat surface only)
                    this.chat_state.slash_sync_pending = false;
                    this.slash_picker_state.close();
                } else if this.main_surface == MainSurface::Chat && this.chat_state.find_open {
                    // 4. In-chat Find bar (Chat surface only)
                    this.chat_state.close_find();
                    if !this.is_focused_on_mounted_known_handle(window, cx) {
                        this.focus_active_surface_default(window, cx);
                    }
                } else if this.main_surface == MainSurface::Chat
                    && this.chat_state.message_action_menu_open
                {
                    // 5. Focused message action menu (Chat surface only)
                    this.chat_state.message_action_menu_open = false;
                    this.chat_state.message_action_index = 0;
                } else if this.main_surface == MainSurface::Chat
                    && this.chat_state.focused_message_idx.is_some()
                {
                    // 6. Focused message highlight (Chat surface only)
                    this.chat_state.focused_message_idx = None;
                } else if this.main_surface == MainSurface::Chat
                    && this.chat_state.reply_quote.is_some()
                {
                    // 7. Reply quote banner (Chat surface only)
                    this.chat_state.reply_quote = None;
                } else if this.is_sidebar_rename_mounted() {
                    // 8. Inline sidebar / top-pill rename (only when mounted)
                    this.sidebar_state.rename = None;
                    this.focus_active_surface_default(window, cx);
                } else if this.is_sidebar_workspace_draft_mounted() {
                    // 9. Inline new workspace draft (only when Open sidebar is rendered)
                    this.sidebar_state.new_workspace_draft = None;
                    this.focus_active_surface_default(window, cx);
                } else if this.is_inbox_rename_mounted() {
                    // 10. Inline Inbox rename (Inbox surface only)
                    this.inbox_state.rename = None;
                } else if this.main_surface == MainSurface::Inbox
                    && this.inbox_state.is_search_visible()
                {
                    // 11. Inbox search bar: close + clear + keep Inbox open
                    this.inbox_state.close_search();
                } else if this.main_surface != MainSurface::Chat {
                    // 12. Return from Inbox / Settings surface to Chat
                    this.set_main_surface(MainSurface::Chat);
                    this.focus_active_surface_default(window, cx);
                } else if let Some(ref sess) = this.selected_session
                    && sess.working
                {
                    // 13. Interrupt working agent on Esc in Chat View
                    if let Some(tx) = this.active_session_tx() {
                        let _ = tx.send(ClientRequest::InterruptSession {
                            session_id: sess.id.clone(),
                            agent: sess.agent,
                        });
                        this.notice = Some(format!("Stop requested for {}.", sess.title));
                    }
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &NextSession, _, cx| {
                if this.table_modal_state.is_open() || this.active_modal != ActiveModal::None {
                    return;
                }
                this.cycle_session(1, cx);
            }))
            .on_action(cx.listener(|this, _: &PrevSession, _, cx| {
                if this.table_modal_state.is_open() || this.active_modal != ActiveModal::None {
                    return;
                }
                this.cycle_session(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &JumpSession1, _, cx| {
                if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                    this.jump_session_index(0, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &JumpSession2, _, cx| {
                if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                    this.jump_session_index(1, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &JumpSession3, _, cx| {
                if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                    this.jump_session_index(2, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &JumpSession4, _, cx| {
                if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                    this.jump_session_index(3, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &JumpSession5, _, cx| {
                if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                    this.jump_session_index(4, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &JumpSession6, _, cx| {
                if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                    this.jump_session_index(5, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &JumpSession7, _, cx| {
                if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                    this.jump_session_index(6, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &JumpSession8, _, cx| {
                if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                    this.jump_session_index(7, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &JumpSession9, _, cx| {
                if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                    this.jump_session_index(8, cx);
                }
            }))
            // =================================================================
            // Slice 3 — Session Lifecycle & Sidebar Structure Actions
            // =================================================================
            .on_action(cx.listener(|this, _: &NewSession, _, cx| {
                if this.table_modal_state.is_open() || this.active_modal != ActiveModal::None {
                    return;
                }
                let cwd = this.default_spawn_cwd();
                this.spawn_new_session(AgentKind::Claude, cwd, None, None, cx);
            }))
            .on_action(cx.listener(|this, _: &DeleteSession, _, cx| {
                if let Some(key) = this.target_triage_session_key() {
                    this.dissolve_session_key(&key, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ArchiveSession, _, cx| {
                if let Some(key) = this.target_triage_session_key() {
                    this.mark_done_or_inbox(&key, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ReopenClosedSession, _, cx| {
                this.reactivate_last_dissolved(cx);
            }))
            .on_action(cx.listener(|this, _: &ReconnectSession, _, cx| {
                if this.table_modal_state.is_open() || this.active_modal != ActiveModal::None {
                    return;
                }
                if this
                    .selected_session
                    .as_ref()
                    .is_some_and(|s| s.agent == AgentKind::Shell)
                {
                    this.reconnect_embedded_shell(cx);
                    return;
                }
                if !this.host_statuses.is_empty() || !this.settings.ssh_hosts.is_empty() {
                    this.reconnect_all_hosts(cx);
                    return;
                }
                if let Some(ref tx) = this.daemon_tx
                    && !tx.is_closed()
                    && tx
                        .send(ClientRequest::ListLifecycleSessions {
                            limit: Some(SESSION_LIST_LIMIT),
                        })
                        .is_ok()
                {
                    if let Some(ref s) = this.selected_session {
                        let _ = tx.send(ClientRequest::WatchSession {
                            session_id: s.id.clone(),
                            source_path: s.source_path.clone(),
                            agent: s.agent,
                        });
                    }
                    this.notice = Some("Refreshed daemon sessions.".to_string());
                    cx.notify();
                    return;
                }

                // Channel is absent or closed: reconnect using SharedAppStore.connected_target or
                // configured/selected host instead of falsely reporting a refresh.
                let shared_target = cx
                    .try_global::<SharedAppStore>()
                    .and_then(|s| s.connected_target.clone());
                let host_label = this
                    .connected_host
                    .clone()
                    .or_else(|| this.settings.selected_host.clone());

                if let Some(target) = shared_target {
                    let label = host_label.unwrap_or_else(|| match &target {
                        DaemonTarget::RemoteSsh { host, .. } => host.name.clone(),
                        DaemonTarget::LocalBinary(_) => "localhost".to_string(),
                    });
                    this.connect_to_target(label, target, cx);
                } else if let Some(host_name) = host_label {
                    if let Some(host) = this
                        .settings
                        .ssh_hosts
                        .iter()
                        .find(|h| h.name == host_name || h.hostname == host_name)
                        .cloned()
                    {
                        this.connect_to_target(
                            host.name.clone(),
                            DaemonTarget::RemoteSsh {
                                host,
                                bundled_linux_binary: Self::find_bundled_linux_daemon(),
                            },
                            cx,
                        );
                    } else if host_name == "localhost"
                        && let Some(bin) = Self::find_local_daemon()
                    {
                        this.connect_to_target(host_name, DaemonTarget::LocalBinary(bin), cx);
                    }
                }
            }))
            .on_action(cx.listener(|this, _: &MoveSessionUp, _, cx| {
                if this.table_modal_state.is_open() || this.active_modal != ActiveModal::None {
                    return;
                }
                this.reorder_active_session(ReorderDirection::Up, cx);
            }))
            .on_action(cx.listener(|this, _: &MoveSessionDown, _, cx| {
                if this.table_modal_state.is_open() || this.active_modal != ActiveModal::None {
                    return;
                }
                this.reorder_active_session(ReorderDirection::Down, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleGroup, _, cx| {
                if this.table_modal_state.is_open() || this.active_modal != ActiveModal::None {
                    return;
                }
                this.toggle_active_group(cx);
            }))
            // =================================================================
            // Slice 3 — Inbox & Session Triage Actions
            // =================================================================
            .on_action(cx.listener(|this, _: &InboxNext, _, cx| {
                if this.table_modal_state.is_open()
                    || this.active_modal != ActiveModal::None
                    || this.is_sidebar_rename_mounted()
                    || this.is_inbox_rename_mounted()
                {
                    return;
                }
                if this.main_surface == MainSurface::Inbox {
                    let count = this.compute_inbox_rows(now_unix_ms()).len();
                    if count > 0 {
                        this.inbox_state.selected_row =
                            (this.inbox_state.selected_row + 1).min(count - 1);
                    }
                    cx.notify();
                } else {
                    if !this.step_inbox_session(1, cx) {
                        this.cycle_workspace(1, cx);
                    }
                    this.sync_workspace_to_shared(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &InboxPrev, _, cx| {
                if this.table_modal_state.is_open()
                    || this.active_modal != ActiveModal::None
                    || this.is_sidebar_rename_mounted()
                    || this.is_inbox_rename_mounted()
                {
                    return;
                }
                if this.main_surface == MainSurface::Inbox {
                    let count = this.compute_inbox_rows(now_unix_ms()).len();
                    if count > 0 {
                        this.inbox_state.selected_row = this
                            .inbox_state
                            .selected_row
                            .min(count - 1)
                            .saturating_sub(1);
                    } else {
                        this.inbox_state.selected_row = 0;
                    }
                    cx.notify();
                } else {
                    if !this.step_inbox_session(-1, cx) {
                        this.cycle_workspace(-1, cx);
                    }
                    this.sync_workspace_to_shared(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleUnreadOnly, _, cx| {
                if this.table_modal_state.is_open() || this.active_modal != ActiveModal::None {
                    return;
                }
                this.toggle_unread_only(cx);
            }))
            .on_action(cx.listener(|this, _: &MarkSessionUnread, _, cx| {
                if this.table_modal_state.is_open()
                    || this.active_modal != ActiveModal::None
                    || this.is_sidebar_rename_mounted()
                    || this.is_inbox_rename_mounted()
                {
                    return;
                }
                if let Some(key) = this.target_triage_session_key()
                    && let Some(sess) = this.find_session_by_key(&key)
                {
                    this.sidebar_state
                        .workspace_store
                        .toggle_session_unread(&key, &sess);
                    this.persist_workspace_store_if_enabled();
                    this.sync_workspace_to_shared(cx);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &RemindSession, window, cx| {
                if this.active_modal == ActiveModal::None
                    && !this.table_modal_state.is_open()
                    && let Some(key) = this.target_triage_session_key()
                {
                    this.open_reminder_picker(
                        crate::palette::ReminderTarget::Session(key),
                        window,
                        cx,
                    );
                }
            }))
            .on_action(cx.listener(|this, _: &MoveSessionWaiting, _, cx| {
                if this.table_modal_state.is_open()
                    || this.active_modal != ActiveModal::None
                    || this.is_sidebar_rename_mounted()
                    || this.is_inbox_rename_mounted()
                {
                    return;
                }
                if let Some(key) = this.target_triage_session_key() {
                    this.mark_done_or_inbox(&key, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &UndoWaiting, _, cx| {
                if this.table_modal_state.is_open()
                    || this.active_modal != ActiveModal::None
                    || this.is_sidebar_rename_mounted()
                    || this.is_inbox_rename_mounted()
                {
                    return;
                }
                if let Some(key) = this.sidebar_state.workspace_store.undo_last_waiting() {
                    this.reactivate_session_key(&key, cx);
                    this.persist_workspace_store_if_enabled();
                    this.sync_workspace_to_shared(cx);
                    cx.notify();
                }
            }))
            // =================================================================
            // Slice 3 — Workspaces & Views Actions
            // =================================================================
            .on_action(cx.listener(|this, _: &ToggleFavorite, _, cx| {
                if this.table_modal_state.is_open() || this.active_modal != ActiveModal::None {
                    return;
                }
                if this.main_surface == MainSurface::Inbox {
                    if let Some(key) = this.selected_inbox_session_key() {
                        this.sidebar_state.workspace_store.toggle_favorite(&key);
                        this.cleanup_unmounted_surface_state();
                        this.persist_workspace_store_if_enabled();
                        this.sync_workspace_to_shared(cx);
                        cx.notify();
                    }
                } else {
                    this.toggle_active_favorite(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &SnoozeWorkspace, window, cx| {
                if this.table_modal_state.is_open() || this.active_modal != ActiveModal::None {
                    return;
                }
                let id = this.sidebar_state.active_workspace_id.clone();
                this.open_reminder_picker(
                    crate::palette::ReminderTarget::Workspace(id),
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &JumpWorkspace1, _, cx| {
                if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                    this.switch_workspace_digit(1, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &JumpWorkspace2, _, cx| {
                if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                    this.switch_workspace_digit(2, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &JumpWorkspace3, _, cx| {
                if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                    this.switch_workspace_digit(3, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &JumpWorkspace4, _, cx| {
                if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                    this.switch_workspace_digit(4, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &JumpWorkspace5, _, cx| {
                if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                    this.switch_workspace_digit(5, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &JumpWorkspace6, _, cx| {
                if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                    this.switch_workspace_digit(6, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &JumpWorkspace7, _, cx| {
                if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                    this.switch_workspace_digit(7, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &JumpWorkspace8, _, cx| {
                if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                    this.switch_workspace_digit(8, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &JumpWorkspace9, _, cx| {
                if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                    this.switch_workspace_digit(9, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &PrevWorkspace, _, cx| {
                if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                    this.cycle_workspace(-1, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &NextWorkspace, _, cx| {
                if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                    this.cycle_workspace(1, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &WorkspaceViewAll, _, cx| {
                if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                    this.set_workspace_view(WorkspaceView::All, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &WorkspaceViewFrequent, _, cx| {
                if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                    this.set_workspace_view(WorkspaceView::Frequent, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &WorkspaceViewFavorites, _, cx| {
                if !this.table_modal_state.is_open() && this.active_modal == ActiveModal::None {
                    this.set_workspace_view(WorkspaceView::Favorites, cx);
                }
            }))
            // =================================================================
            // Slice 3 — System Inline Rename (`F2`)
            // =================================================================
            .on_action(cx.listener(|this, _: &RenameActiveSession, window, cx| {
                if this.table_modal_state.is_open()
                    || this.active_modal != ActiveModal::None
                    || this.is_sidebar_rename_mounted()
                    || this.is_inbox_rename_mounted()
                {
                    return;
                }
                if this.main_surface == MainSurface::Inbox {
                    if let Some(key) = this.selected_inbox_session_key()
                        && this.is_session_rendered_in_inbox(&key)
                        && let Some(sess) = this.find_session_by_key(&key)
                    {
                        let current = this
                            .sidebar_state
                            .workspace_store
                            .effective_title(&key, &sess.title)
                            .to_string();
                        this.sidebar_state.rename = None;
                        this.inbox_state.rename =
                            Some(InlineRenameState::new(key, sess.id, current));
                        this.focus_inbox(window, cx);
                        cx.notify();
                    }
                } else if let Some(sel) = this.selected_session.clone() {
                    let key = this.session_key_for(&sel);
                    if this.can_mount_sidebar_rename(&key) {
                        if this.main_surface == MainSurface::Chat {
                            this.dismiss_conflicting_chat_overlays_for_rename();
                        }
                        this.start_sidebar_rename_with_window(key, window, cx);
                    }
                }
            }))
            // =================================================================
            // Slice 4 — Rich Chat View & Font Zoom Actions
            // =================================================================
            .on_action(cx.listener(|this, _: &CopyLatestCodeBlock, _, cx| {
                if this.table_modal_state.is_open() || this.active_modal != ActiveModal::None {
                    return;
                }
                if this.main_surface == MainSurface::Chat {
                    this.chat_state.slash_sync_pending = false;
                    this.copy_latest_code_block(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &FindInChat, window, cx| {
                if this.table_modal_state.is_open() || this.active_modal != ActiveModal::None {
                    return;
                }
                if this.main_surface == MainSurface::Inbox {
                    this.inbox_state.rename = None;
                    this.sidebar_state.rename = None;
                    this.sidebar_state.new_workspace_draft = None;
                    this.inbox_state.open_search();
                    this.focus_inbox(window, cx);
                    cx.notify();
                } else if this.main_surface == MainSurface::Chat {
                    this.sidebar_state.rename = None;
                    this.sidebar_state.new_workspace_draft = None;
                    this.chat_state.slash_sync_pending = false;
                    this.slash_picker_state.close();
                    let msgs = this.messages.clone();
                    this.chat_state.open_find(&msgs, this.show_tool_calls());
                    this.focus_chat_panel(window, cx);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &BrowseCommands, window, cx| {
                if this.table_modal_state.is_open() || this.active_modal != ActiveModal::None {
                    return;
                }
                if this.main_surface == MainSurface::Chat {
                    this.sidebar_state.rename = None;
                    this.sidebar_state.new_workspace_draft = None;
                    this.chat_state.slash_sync_pending = false;
                    this.chat_state.close_find();
                    if let Some(ref sel) = this.selected_session {
                        this.request_daemon_commands(sel.agent, sel.cwd.clone());
                    }
                    this.open_slash_browser(cx);
                    this.prompt_input.update(cx, |state, cx| {
                        state.focus(window, cx);
                    });
                }
            }))
            .on_action(cx.listener(|this, _: &FontZoomIn, _, cx| {
                if this.table_modal_state.is_open() || this.active_modal != ActiveModal::None {
                    return;
                }
                this.chat_state.slash_sync_pending = false;
                this.zoom_chat_font(1, cx);
                this.sync_settings_to_shared(cx);
                if this.sidebar_state.persist_to_disk {
                    let _ = this.settings.save();
                }
            }))
            .on_action(cx.listener(|this, _: &FontZoomOut, _, cx| {
                if this.table_modal_state.is_open() || this.active_modal != ActiveModal::None {
                    return;
                }
                this.chat_state.slash_sync_pending = false;
                this.zoom_chat_font(-1, cx);
                this.sync_settings_to_shared(cx);
                if this.sidebar_state.persist_to_disk {
                    let _ = this.settings.save();
                }
            }))
            .on_action(cx.listener(|this, _: &FontZoomReset, _, cx| {
                if this.table_modal_state.is_open() || this.active_modal != ActiveModal::None {
                    return;
                }
                this.chat_state.slash_sync_pending = false;
                this.reset_chat_font(cx);
                this.sync_settings_to_shared(cx);
                if this.sidebar_state.persist_to_disk {
                    let _ = this.settings.save();
                }
            }))
            .child(
                h_flex()
                    .size_full()
                    .when(self.sidebar_rendered(), |el| {
                        el.child(self.render_sidebar(cx))
                    })
                    .child({
                        let surface = match self.main_surface {
                            MainSurface::Chat
                                if self.selected_session.as_ref().is_some_and(|s| {
                                    s.agent == AgentKind::Shell && !s.archived
                                }) =>
                            {
                                self.render_shell_panel(cx).into_any_element()
                            }
                            MainSurface::Chat => self.render_chat_panel(cx).into_any_element(),
                            MainSurface::Inbox => self.render_inbox_panel(cx).into_any_element(),
                            MainSurface::Settings => self.render_chat_panel(cx).into_any_element(),
                        };
                        #[cfg(windows)]
                        let surface = if let Some(pane) = browser_pane {
                            h_flex()
                                .flex_1()
                                .min_w_0()
                                .h_full()
                                .child(surface)
                                .child(pane)
                                .into_any_element()
                        } else {
                            surface
                        };
                        surface
                    })
                    // Slice 5 — Computer View docks as a right-hand split beside the chat.
                    // Mounted only while open, so a closed pane adds no layout node, no
                    // webview and no browser process.
                    .when_some(self.render_computer_view(cx), |el, pane| el.child(pane)),
            )
            .when(self.main_surface == MainSurface::Settings, |el| {
                el.child(self.render_settings_panel(cx))
            })
            .when(self.active_modal == ActiveModal::Palette, |el| {
                el.child(self.render_palette_modal(cx))
            })
            .when(self.active_modal == ActiveModal::Shortcuts, |el| {
                el.child(self.render_shortcuts_modal(cx))
            })
            .when(self.active_modal == ActiveModal::Changelog, |el| {
                el.child(self.render_changelog_modal(cx))
            })
            .when(self.active_modal == ActiveModal::ChatInfo, |el| {
                el.child(self.render_chat_info(cx))
            })
            .when_some(self.render_table_modal(cx), |el, modal| el.child(modal))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::slash_picker::TriggerMode;
    use gpui_kit::{TestAppContext, test::TestWindowExt};
    use multibot_core::cache::CachedSession;
    use multibot_core::workspaces::{PaletteCategory, PaletteRow};
    use multibot_proto::{MessageBlock, SlashCommandItem, TodoItem, TodoStatus, ToolStatus};
    use multibot_updater::{ReleaseInfo, WindowsAssetInfo};
    use std::sync::atomic::{AtomicU64, Ordering};

    static APP_TEST_SEQ: AtomicU64 = AtomicU64::new(1);

    pub(crate) fn open_test_view_with_entity(
        cx: &mut TestAppContext,
    ) -> (gpui_kit::AnyWindowHandle, Entity<MultibotView>) {
        open_test_view_with_settings(cx, MultibotSettings::default(), WindowOptions::default())
    }

    fn open_test_view_with_settings(
        cx: &mut TestAppContext,
        settings: MultibotSettings,
        options: WindowOptions,
    ) -> (gpui_kit::AnyWindowHandle, Entity<MultibotView>) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply_multibot_dark_theme(cx);
            crate::bind_multibot_keys(cx);
            SharedAppStore::init(cx, settings);
        });
        let rt = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        cx.update(|cx| {
            gpui_kit::open_window(options, cx, move |window, cx| {
                cx.new(|cx| MultibotView::new(rt, window, cx))
            })
            .unwrap()
        })
    }

    fn open_test_view(cx: &mut TestAppContext) -> gpui_kit::AnyWindowHandle {
        open_test_view_with_entity(cx).0
    }

    fn make_session(id: &str, agent: AgentKind, title: &str, working: bool) -> SessionSummary {
        SessionSummary {
            id: id.into(),
            agent,
            title: title.into(),
            cwd: Some("/srv/app".into()),
            updated_at_ms: 1_700_000_000_000,
            working,
            source_path: format!("/tmp/{id}.jsonl"),
            ..Default::default()
        }
    }

    #[gpui_kit::test]
    fn busy_composer_queues_fifo_once_for_every_chat_client(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        for agent in [AgentKind::Claude, AgentKind::Codex, AgentKind::Antigravity, AgentKind::Pi, AgentKind::Cursor] {
            let (tx, mut rx) = mpsc::unbounded_channel();
            cx.update_window(handle, |_, window, cx| {
                let mut session = make_session(agent.label(), agent, agent.label(), true);
                view.update(cx, |v, cx| {
                    v.daemon_tx = Some(tx);
                    v.sessions = vec![session.clone()];
                    v.select_session(session.clone(), cx);
                });
                while rx.try_recv().is_ok() {}
                for text in ["first follow-up", "second follow-up"] {
                    view.update(cx, |v, cx| v.prompt_input.update(cx, |input, cx| {
                        input.set_value(text, window, cx);
                        input.focus(window, cx);
                    }));
                    window.render_frame(cx);
                    window.press("ctrl-enter", cx);
                    window.render_frame(cx);
                    assert!(view.read(cx).prompt_input.read(cx).value().is_empty());
                    assert!(window.find("stop-agent").visible());
                    assert_eq!(window.find("send-prompt").label(), Some("Queue"));
                    let tray = window.find("queued-messages-bar").bounds();
                    assert!(tray.bottom() <= window.find("prompt-input").bounds().top());
                }
                assert!(rx.try_recv().is_err());
                session.working = false;
                for _ in 0..3 {
                    view.update(cx, |v, cx| v.handle_bridge_event(UiBridgeEvent::Server(
                        ServerEvent::SessionUpdated { session: session.clone() }), cx));
                }
                assert!(matches!(rx.try_recv(), Ok(ClientRequest::SendPrompt { text, agent: actual, .. })
                    if text == "first follow-up" && actual == agent));
                assert!(rx.try_recv().is_err(), "duplicate idle events must not drain the queue");
                view.update(cx, |v, cx| {
                    v.handle_bridge_event(UiBridgeEvent::Server(ServerEvent::HistoryAppend {
                        session_id: session.id.clone(), messages: vec![], latest_offset: None,
                        working: Some(true), todos: vec![], probe_sha256: None,
                    }), cx);
                    for _ in 0..3 {
                        v.handle_bridge_event(UiBridgeEvent::Server(ServerEvent::HistoryAppend {
                            session_id: session.id.clone(), messages: vec![], latest_offset: None,
                            working: Some(false), todos: vec![], probe_sha256: None,
                        }), cx);
                    }
                });
                assert!(matches!(rx.try_recv(), Ok(ClientRequest::SendPrompt { text, .. }) if text == "second follow-up"));
                assert!(rx.try_recv().is_err());
                window.render_frame(cx);
                assert!(window.try_find("queued-messages-bar").is_none());
            }).unwrap();
        }
    }

    #[gpui_kit::test]
    fn queued_message_edit_remove_and_force_send_are_clickable(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        let (tx, mut rx) = mpsc::unbounded_channel();
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                let session = make_session("queue-actions", AgentKind::Claude, "Queue actions", true);
                v.sessions = vec![session.clone()];
                v.daemon_tx = Some(tx);
                v.select_session(session.clone(), cx);
                v.send_or_queue_prompt(&session, "edit".into(), "edit this".into());
                v.send_or_queue_prompt(&session, "remove".into(), "remove this".into());
                v.send_or_queue_prompt(&session, "force".into(), "urgent steering".into());
                v.send_or_queue_prompt(&session, "next".into(), "after forced turn".into());
            });
            while rx.try_recv().is_ok() {}
            window.render_frame(cx);
            window.click("queue-edit-0", cx);
            assert_eq!(view.read(cx).prompt_input.read(cx).value().as_ref(), "edit this");
            assert!(view.read(cx).prompt_input.read(cx).focus_handle(cx).is_focused(window));
            window.render_frame(cx);
            window.click("queue-remove-0", cx);
            window.render_frame(cx);
            let force_id = view.read(cx).chat_outbox.entries[0].id.clone();
            window.click("queue-send-0", cx);
            window.render_frame(cx);
            assert!(matches!(rx.try_recv(), Ok(ClientRequest::InterruptSession { session_id, agent: AgentKind::Claude }) if session_id == "queue-actions"));
            assert!(matches!(rx.try_recv(), Ok(ClientRequest::SendPrompt { text, .. }) if text == "urgent steering"));
            assert_eq!(view.read(cx).shared_chat_outbox.lock().unwrap().outbox.entries.len(), 1);
            view.update(cx, |v, cx| {
                v.force_send_queued_entry(&force_id, cx);
                assert_eq!(v.flush_outbox_for_current_host(), 0);
            });
            assert!(rx.try_recv().is_err());
            view.update(cx, |v, cx| {
                for working in [false, false, true, false, false] {
                    let mut session = v.selected_session.clone().unwrap();
                    session.working = working;
                    v.handle_bridge_event(UiBridgeEvent::Server(ServerEvent::SessionUpdated { session }), cx);
                }
            });
            assert!(matches!(rx.try_recv(), Ok(ClientRequest::SendPrompt { text, .. }) if text == "after forced turn"));
            assert!(rx.try_recv().is_err());
            window.render_frame(cx);
            assert!(window.try_find("queued-messages-bar").is_none());
            view.update(cx, |v, _| {
                let mut session = v.selected_session.clone().unwrap();
                session.working = true;
                v.send_or_queue_prompt(&session, "edit-with-draft".into(), "queued edit".into());
            });
            window.render_frame(cx);
            window.click("queue-edit-0", cx);
            assert_eq!(view.read(cx).prompt_input.read(cx).value().as_ref(), "queued edit");
            assert_eq!(view.read(cx).chat_outbox.entries[0].text, "edit this");
        }).unwrap();
    }

    #[gpui_kit::test]
    fn full_width_roundtrip_restores_btop_in_a_fresh_view(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        let btop = make_session("btop", AgentKind::Shell, "BTOP", false);
        let normal = make_session("normal-width", AgentKind::Claude, "Normal", false);
        let path = std::env::temp_dir().join(format!("mbru-full-width-{}-{}.json", std::process::id(), APP_TEST_SEQ.fetch_add(1, Ordering::Relaxed)));
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.sessions = vec![btop.clone(), normal.clone()];
                v.set_sidebar_mode_synced(SidebarMode::Collapsed, cx);
                v.select_session(btop.clone(), cx);
            });
            window.render_frame(cx);
            window.press("ctrl-shift-f", cx);
            assert_eq!(view.read(cx).sidebar_state.mode, SidebarMode::Hidden);
            view.read(cx).sidebar_state.workspace_store.save_to(&path).unwrap();
            view.update(cx, |v, cx| v.select_session(normal.clone(), cx));
            assert_eq!(view.read(cx).sidebar_state.mode, SidebarMode::Collapsed);
        }).unwrap();
        let loaded = multibot_core::workspaces::WorkspaceStore::load_from(&path).unwrap();
        let rt = cx.read(|cx| view.read(cx).runtime.clone());
        let (fresh_handle, fresh) = cx.update(|cx| {
            cx.global_mut::<SharedAppStore>().workspace_store = loaded;
            gpui_kit::open_window(WindowOptions::default(), cx, move |window, cx| {
                cx.new(|cx| MultibotView::new(rt, window, cx))
            }).unwrap()
        });
        cx.update_window(fresh_handle, |_, window, cx| {
            fresh.update(cx, |v, cx| v.select_session(btop.clone(), cx));
            window.render_frame(cx);
            assert_eq!(fresh.read(cx).sidebar_state.mode, SidebarMode::Hidden);
            assert!(fresh.read(cx).settings.shell_fullscreen);
            fresh.update(cx, |v, cx| v.select_session(normal, cx));
            assert_eq!(fresh.read(cx).sidebar_state.mode, SidebarMode::Collapsed);
            assert!(!fresh.read(cx).settings.shell_fullscreen);
        }).unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[gpui_kit::test]
    fn server_shell_manager_recovers_and_ends_on_the_owning_host(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        let (mut rx_a, _) = cx.update(|cx| install_test_host(cx, "host-a"));
        let (mut rx_b, _) = cx.update(|cx| install_test_host(cx, "host-b"));
        let mut a = make_session("same-id", AgentKind::Shell, "Btop A", false);
        a.shell_attached = Some(true);
        a.source_path = "tmux:same-id".into();
        a.last_assistant_preview = Some("btop · projects".into());
        let mut b = a.clone();
        b.title = "Btop B".into();
        b.archived = true;
        b.shell_attached = Some(false);
        cx.update(|cx| {
            let store = cx.global_mut::<SharedAppStore>();
            store.host_connections.get_mut("host-a").unwrap().sessions = vec![a.clone()];
            store.host_connections.get_mut("host-b").unwrap().sessions = vec![b.clone()];
            store.host_sessions = vec![("host-a".into(), a.clone()), ("host-b".into(), b.clone())];
            store.sessions = vec![a.clone(), b.clone()];
            store.workspace_store.dissolve_session(&canonical_session_key("host-b", AgentKind::Shell, "same-id"), now_unix_ms());
            store.bump_revision();
        });
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| v.sync_from_shared_store(cx));
            window.press("ctrl-k", cx);
            window.input("server shells", cx);
            window.render_frame(cx);
            window.click("pal-cmd-server-shells", cx);
            window.render_frame(cx);
            let rows = view.read(cx).build_palette_rows("", now_unix_ms());
            assert_eq!(rows.len(), 2);
            for (host, attached) in [("host-a", "Attached"), ("host-b", "Detached")] {
                assert!(rows.iter().any(|row| matches!(row, PaletteRow::Session { title, workspace_name, cwd, .. }
                    if title.contains("same-id") && workspace_name.contains(host) && workspace_name.contains(attached)
                        && cwd.as_deref() == Some("btop · projects"))));
            }
            window.input("Btop B", cx);
            window.render_frame(cx);
            window.press("enter", cx);
            assert_eq!(view.read(cx).selected_session_host.as_deref(), Some("host-b"));
            assert!(!view.read(cx).selected_session.as_ref().unwrap().archived);
            while rx_a.try_recv().is_ok() {}
            while rx_b.try_recv().is_ok() {}
            window.press("ctrl-k", cx);
            window.input("server shells", cx);
            window.render_frame(cx);
            window.click("pal-cmd-server-shells", cx);
            window.input("Btop A", cx);
            window.render_frame(cx);
            window.click("pal-shell-end-0", cx);
            window.render_frame(cx);
            assert!(!std::iter::from_fn(|| rx_a.try_recv().ok()).any(|req| matches!(req, ClientRequest::ArchiveSession { .. })));
            window.click("pal-shell-kill-confirm", cx);
            assert!(std::iter::from_fn(|| rx_a.try_recv().ok()).any(|req| matches!(req,
                ClientRequest::ArchiveSession { session_id, kill_process: true, agent: AgentKind::Shell, source_path }
                    if session_id == "same-id" && source_path == "tmux:same-id")));
            assert!(!std::iter::from_fn(|| rx_b.try_recv().ok()).any(|req| matches!(req, ClientRequest::ArchiveSession { .. })));
        }).unwrap();
    }

    #[gpui_kit::test]
    fn daily_driver_session_switch_paths_focus_composer(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            let alpha = make_session("focus-alpha", AgentKind::Claude, "Focus Alpha", false);
            let beta = make_session("focus-beta", AgentKind::Codex, "Focus Beta", false);
            let shell = make_session("focus-shell", AgentKind::Shell, "Focus Shell", false);
            view.update(cx, |v, cx| {
                v.sessions = vec![alpha.clone(), beta.clone(), shell.clone()];
                v.set_sidebar_mode_synced(SidebarMode::Open, cx);
            });
            for path in [
                "sidebar",
                "next",
                "previous",
                "digit",
                "palette",
                "inbox-enter",
                "inbox-click",
                "shell-to-chat",
            ] {
                view.update(cx, |v, cx| {
                    v.select_session(
                        match path {
                            "shell-to-chat" => shell.clone(),
                            "previous" => beta.clone(),
                            _ => alpha.clone(),
                        },
                        cx,
                    );
                    v.prompt_input
                        .update(cx, |input, cx| input.set_value("", window, cx));
                });
                window.render_frame(cx);
                view.update(cx, |v, cx| {
                    window.focus(&v.root_focus_handle, cx);
                    cx.notify();
                });
                window.render_frame(cx);
                match path {
                    "sidebar" => window.click("session-1", cx),
                    "next" => window.press("ctrl-tab", cx),
                    "previous" => window.press("ctrl-shift-tab", cx),
                    "digit" => window.press("ctrl-2", cx),
                    "palette" => {
                        window.press("ctrl-k", cx);
                        window.input("Focus Beta", cx);
                        window.press("enter", cx);
                    }
                    "inbox-enter" | "inbox-click" => {
                        window.press("alt-i", cx);
                        window.render_frame(cx);
                        if path == "inbox-enter" {
                            window.press("enter", cx);
                        } else {
                            window.click("inbox-row-1", cx);
                        }
                    }
                    "shell-to-chat" => window.press("ctrl-1", cx),
                    _ => unreachable!(),
                }
                window.render_frame(cx);
                assert_eq!(view.read(cx).main_surface, MainSurface::Chat, "{path}");
                assert!(
                    view.read(cx)
                        .prompt_input
                        .read(cx)
                        .focus_handle(cx)
                        .is_focused(window),
                    "{path}"
                );
                window.input("ready", cx);
                assert_eq!(
                    view.read(cx).prompt_input.read(cx).value(),
                    "ready",
                    "{path}"
                );
            }
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn composer_enter_inserts_newline_without_sending(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        let (tx, mut rx) = mpsc::unbounded_channel();
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                let session = make_session("newline", AgentKind::Claude, "Newline", false);
                v.daemon_tx = Some(tx);
                v.sessions = vec![session.clone()];
                v.select_session(session, cx);
            });
            window.render_frame(cx);
            while rx.try_recv().is_ok() {}
            window.input("first", cx);
            window.press("enter", cx);
            window.input("second", cx);
            window.press("shift-enter", cx);
            window.input("third", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).prompt_input.read(cx).value(), "first\nsecond\nthird");
            assert!(view.read(cx).messages.is_empty());
            assert!(rx.try_recv().is_err());
        }).unwrap();
    }

    #[gpui_kit::test]
    fn composer_ctrl_enter_sends_multiline_message(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        let (tx, mut rx) = mpsc::unbounded_channel();
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                let session = make_session("multiline", AgentKind::Claude, "Multiline", false);
                v.daemon_tx = Some(tx);
                v.sessions = vec![session.clone()];
                v.select_session(session, cx);
            });
            window.render_frame(cx);
            while rx.try_recv().is_ok() {}
            window.input("first", cx);
            window.press("enter", cx);
            window.input("second", cx);
            window.press("ctrl-enter", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).prompt_input.read(cx).value(), "");
            assert!(matches!(rx.try_recv(), Ok(ClientRequest::SendPrompt { text, .. }) if text == "first\nsecond"));
            assert!(rx.try_recv().is_err());
        }).unwrap();
    }

    #[gpui_kit::test]
    fn daily_driver_composer_grows_upward_caps_scrolls_and_sends(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        let (tx, mut rx) = mpsc::unbounded_channel();
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                let session = make_session("growing", AgentKind::Claude, "Growing", false);
                v.daemon_tx = Some(tx);
                v.sessions = vec![session.clone()];
                v.select_session(session, cx);
            });
            window.render_frame(cx);
            let initial = window.find("prompt-input").bounds();
            window.input("first", cx);
            window.press("shift-enter", cx);
            window.input("second", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).prompt_input.read(cx).value(), "first\nsecond");
            let grown = window.find("prompt-input").bounds();
            assert!(grown.size.height > initial.size.height);
            assert!((grown.bottom() - initial.bottom()).abs() <= px(1.0));
            while let Ok(request) = rx.try_recv() {
                assert!(!matches!(request, ClientRequest::SendPrompt { .. }));
            }
            window.press("ctrl-enter", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).prompt_input.read(cx).value(), "");
            let mut sent = false;
            while let Ok(request) = rx.try_recv() {
                sent |= matches!(request, ClientRequest::SendPrompt { text, .. } if text == "first\nsecond");
            }
            assert!(sent);
            assert_eq!(window.find("prompt-input").bounds().size.height, initial.size.height);

            window.input(&"dictated words ".repeat(50), cx);
            window.render_frame(cx);
            assert!(window.find("prompt-input").bounds().size.height > initial.size.height);
            view.update(cx, |v, cx| {
                v.prompt_input.update(cx, |input, cx| input.replace_all((1..=10).map(|n| n.to_string()).collect::<Vec<_>>().join("\n"), window, cx));
            });
            window.render_frame(cx);
            let capped = window.find("prompt-input").bounds().size.height;
            window.press("ctrl-end", cx);
            for _ in 0..8 {
                window.press("shift-enter", cx);
                window.input("more", cx);
            }
            window.render_frame(cx);
            assert_eq!(window.find("prompt-input").bounds().size.height, capped);
            let input = view.read(cx).prompt_input.read(cx);
            assert!(input.scroll_offset().y < px(0.0));
            let caret = input.cursor_layout().unwrap().0;
            assert!(caret.top() + input.scroll_offset().y >= input.input_bounds().top() - px(1.0));
            assert!(caret.bottom() + input.scroll_offset().y <= input.input_bounds().bottom() + px(1.0));
            view.update(cx, |v, cx| v.append_dictation_transcript(&"dictation continues ".repeat(80), window, cx));
            window.render_frame(cx);
            let input = view.read(cx).prompt_input.read(cx);
            assert_eq!(input.cursor(), input.value().len());
            assert_eq!(window.find("prompt-input").bounds().size.height, capped);
            let caret = input.cursor_layout().unwrap().0;
            assert!(caret.top() + input.scroll_offset().y >= input.input_bounds().top() - px(1.0));
            assert!(caret.bottom() + input.scroll_offset().y <= input.input_bounds().bottom() + px(1.0), "caret={caret:?} scroll={:?} bounds={:?}", input.scroll_offset(), input.input_bounds());
        }).unwrap();
    }

    #[gpui_kit::test]
    fn daily_driver_shell_and_computer_view_share_the_split(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                let session = make_session("split-shell", AgentKind::Shell, "Split Shell", false);
                v.sessions = vec![session.clone()];
                v.select_session(session, cx);
            });
            window.render_frame(cx);
            let full_width = window.find("server-shell-panel").bounds().size.width;
            window.press("ctrl-k", cx);
            window.input("Toggle Computer View", cx);
            window.press("enter", cx);
            window.render_frame(cx);
            assert!(view.read(cx).embedded_shell_visible());
            assert!(window.find("computer-header").visible());
            let shell = window.find("server-shell-panel").bounds();
            let computer = window.find("computer-pane").bounds();
            assert!(shell.size.width < full_width);
            assert!(shell.right() <= computer.left() + px(1.0));
            let input = view.read(cx).computer.open().unwrap().url_input.clone();
            assert!(input.read(cx).focus_handle(cx).is_focused(window));
            window.input("https://example.com", cx);
            assert_eq!(input.read(cx).value(), "https://example.com");
            window.render_frame(cx);
            assert!(input.read(cx).focus_handle(cx).is_focused(window));
            window.click("computer-close", cx);
            window.render_frame(cx);
            assert!(!view.read(cx).computer.is_open());
            assert!(view.read(cx).embedded_shell_visible());
            assert_eq!(
                window.find("server-shell-panel").bounds().size.width,
                full_width
            );
            assert!(
                view.read(cx)
                    .chat_state
                    .focus_handle
                    .as_ref()
                    .unwrap()
                    .is_focused(window)
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn daily_driver_shortcuts_search_filters_and_never_changes_the_draft(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                let session = make_session("search-shortcuts", AgentKind::Claude, "Shortcuts", false);
                v.select_session(session, cx);
                v.prompt_input
                    .update(cx, |input, cx| input.set_value("unsent draft", window, cx));
            });
            window.render_frame(cx);
            for open_from_palette in [false, true] {
                if open_from_palette {
                    window.press("ctrl-k", cx);
                    window.input("Show Keyboard Shortcuts", cx);
                    window.press("enter", cx);
                } else {
                    window.press("f1", cx);
                }
                window.render_frame(cx);
                let input = view.read(cx).shortcuts_search_input.clone();
                assert!(input.read(cx).focus_handle(cx).is_focused(window));
                assert_eq!(input.read(cx).value(), "");
                for spec in multibot_keys::SHORTCUTS {
                    assert!(
                        window.try_find(format!("shortcut-{}", spec.id)).is_some(),
                        "{}",
                        spec.id
                    );
                }
                for query in [
                    "workspace",
                    "CTRL+SHIFT+W",
                    "ctrl-shift-w",
                    "workspace switch",
                    "no-such-shortcut",
                ] {
                    window.press("ctrl-a", cx);
                    window.input(query, cx);
                    window.render_frame(cx);
                    let tokens: Vec<_> = query
                        .to_lowercase()
                        .split_whitespace()
                        .map(str::to_string)
                        .collect();
                    for spec in multibot_keys::SHORTCUTS {
                        let searchable = format!(
                            "{} {} {} {}",
                            spec.description,
                            spec.category.label(),
                            spec.display,
                            spec.gpui_key
                        )
                        .to_lowercase();
                        let expected = tokens.iter().all(|t| searchable.contains(t));
                        assert_eq!(
                            window.try_find(format!("shortcut-{}", spec.id)).is_some(),
                            expected,
                            "{query}: {}",
                            spec.id
                        );
                    }
                }
                assert!(window.find("shortcuts-empty").visible());
                window.press("ctrl-a", cx);
                window.press("backspace", cx);
                window.render_frame(cx);
                assert!(window.try_find("shortcuts-empty").is_none());
                for spec in multibot_keys::SHORTCUTS {
                    assert!(window.try_find(format!("shortcut-{}", spec.id)).is_some());
                }
                window.input("workspace", cx);
                window.press("escape", cx);
                window.render_frame(cx);
                assert_eq!(view.read(cx).active_modal, ActiveModal::None);
                assert_eq!(view.read(cx).prompt_input.read(cx).value(), "unsent draft");
                assert!(
                    view.read(cx)
                        .prompt_input
                        .read(cx)
                        .focus_handle(cx)
                        .is_focused(window)
                );
            }
            window.press("f1", cx);
            window.input("workspace", cx);
            window.press("f1", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::None);
            assert!(
                view.read(cx)
                    .prompt_input
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window)
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn daily_driver_spawned_chat_and_shell_switches_focus_the_active_input(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            let first = make_session("shell-one", AgentKind::Shell, "Shell One", false);
            let second = make_session("shell-two", AgentKind::Shell, "Shell Two", false);
            view.update(cx, |v, cx| {
                v.sessions = vec![first.clone(), second.clone()];
                v.select_session(first.clone(), cx);
            });
            window.render_frame(cx);
            for session in [second, first] {
                view.update(cx, |v, cx| v.select_session(session.clone(), cx));
                window.render_frame(cx);
                assert!(
                    view.read(cx)
                        .chat_state
                        .focus_handle
                        .as_ref()
                        .unwrap()
                        .is_focused(window)
                );
                assert!(!view.read(cx).pending_focus_input);
            }
            let (tx, _rx) = mpsc::unbounded_channel();
            view.update(cx, |v, cx| {
                v.daemon_tx = Some(tx);
                assert!(v.spawn_new_session(AgentKind::Claude, "/srv/app".into(), None, None, cx));
                v.handle_daemon_server_event(
                    ServerEvent::SessionSpawned {
                        session: make_session("spawned-chat", AgentKind::Claude, "Spawned", false),
                    },
                    cx,
                );
            });
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).selected_session.as_ref().unwrap().id,
                "spawned-chat"
            );
            assert!(
                view.read(cx)
                    .prompt_input
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window)
            );
            window.input("new chat ready", cx);
            assert_eq!(
                view.read(cx).prompt_input.read(cx).value(),
                "new chat ready"
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn daily_driver_pending_focus_yields_to_find_rename_and_computer(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            let session = make_session("focus-guards", AgentKind::Claude, "Focus Guards", false);
            view.update(cx, |v, cx| {
                v.sessions = vec![session.clone()];
                v.select_session(session.clone(), cx);
            });
            window.render_frame(cx);
            window.press("ctrl-f", cx);
            window.render_frame(cx);
            view.update(cx, |v, cx| {
                v.pending_focus_input = true;
                cx.notify();
            });
            window.render_frame(cx);
            assert!(view.read(cx).chat_state.find_open);
            assert!(
                view.read(cx)
                    .chat_state
                    .focus_handle
                    .as_ref()
                    .unwrap()
                    .is_focused(window)
            );
            window.press("escape", cx);
            window.press("f2", cx);
            window.render_frame(cx);
            view.update(cx, |v, cx| {
                v.pending_focus_input = true;
                cx.notify();
            });
            window.render_frame(cx);
            assert!(view.read(cx).sidebar_state.rename.is_some());
            assert!(
                !view
                    .read(cx)
                    .prompt_input
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window)
            );
            window.press("escape", cx);
            view.update(cx, |v, cx| {
                v.open_computer_view(window, cx);
                v.select_session(session, cx);
            });
            window.render_frame(cx);
            let input = view.read(cx).computer.open().unwrap().url_input.clone();
            assert!(input.read(cx).focus_handle(cx).is_focused(window));
            window.input("https://example.com", cx);
            assert_eq!(input.read(cx).value(), "https://example.com");
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn lifecycle_each_view_keeps_workspace_and_favorites_splits(cx: &mut TestAppContext) {
        use multibot_core::workspaces::{InboxSplit, SessionLifecycle};
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            let workspace = view.update(cx, |v, cx| {
                let workspace = v
                    .sidebar_state
                    .workspace_store
                    .create_workspace("Project", None)
                    .unwrap();
                let now = now_unix_ms();
                for i in 0..4 {
                    let mut session =
                        make_session(&format!("view-{i}"), AgentKind::Claude, "View", false);
                    let key = v.session_key_for(&session);
                    let store = &mut v.sidebar_state.workspace_store;
                    store.assign_session_workspace(&key, &workspace.id);
                    store.toggle_favorite(&key);
                    match i {
                        1 => store.archive_to_waiting_session(&key, now, &session),
                        2 => {
                            store.hibernate_session(&key, &session, Some(now + 60_000), None, true)
                        }
                        3 => {
                            store.dissolve_session(&key, now);
                            session.archived = true;
                            session.archive_kill_process = true;
                        }
                        _ => {}
                    }
                    v.sessions.push(session);
                }
                v.sync_sessions_to_shared(cx);
                v.sync_workspace_to_shared(cx);
                workspace.id
            });
            for lifecycle in SessionLifecycle::ALL {
                view.update(cx, |v, cx| v.open_lifecycle_view(lifecycle, window, cx));
                window.render_frame(cx);
                assert!(window.find("inbox-tab-all").visible());
                assert!(window.find("inbox-split-favorites").visible());
                assert!(window.find("inbox-split-ws-0").visible());
                window.click("inbox-split-ws-0", cx);
                window.render_frame(cx);
                view.read_with(cx, |v, _| {
                    assert_eq!(
                        v.inbox_state.active_split,
                        InboxSplit::Workspace(workspace.clone())
                    );
                    let rows = v.compute_inbox_rows(now_unix_ms());
                    assert_eq!(rows.len(), 1);
                    assert_eq!(rows[0].workspace_id, workspace);
                    assert!(rows[0].favorite);
                });
                if lifecycle == SessionLifecycle::Done {
                    window.press("e", cx);
                    window.render_frame(cx);
                    assert!(view.read(cx).compute_inbox_rows(now_unix_ms()).is_empty());
                } else if lifecycle == SessionLifecycle::Active {
                    window.press("h", cx);
                    window.render_frame(cx);
                    assert!(window.find("pal-reminder-0").visible());
                    window.press("escape", cx);
                    window.render_frame(cx);
                }
            }
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn lifecycle_single_chords_and_removed_leader_preserve_composer(cx: &mut TestAppContext) {
        use multibot_core::workspaces::SessionLifecycle;
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                let session = make_session("go", AgentKind::Claude, "Go", false);
                v.sessions = vec![session.clone()];
                v.select_session(session, cx);
                v.prompt_input
                    .update(cx, |input, cx| input.replace("draft", window, cx));
                let focus = v.prompt_input.read(cx).focus_handle(cx);
                window.focus(&focus, cx);
            });
            window.render_frame(cx);
            for (chord, lifecycle) in [
                ("alt-shift-e", SessionLifecycle::Done),
                ("alt-shift-h", SessionLifecycle::Hibernated),
                ("alt-shift-d", SessionLifecycle::Dissolved),
            ] {
                view.update(cx, |v, cx| {
                    v.set_main_surface(MainSurface::Chat);
                    let focus = v.prompt_input.read(cx).focus_handle(cx);
                    window.focus(&focus, cx);
                });
                window.render_frame(cx);
                window.press(chord, cx);
                window.render_frame(cx);
                assert_eq!(view.read(cx).inbox_state.lifecycle, lifecycle);
                assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);
                assert_eq!(
                    view.read(cx).prompt_input.read(cx).value().as_ref(),
                    "draft"
                );
                for id in [
                    "inbox-view-inbox",
                    "inbox-view-done",
                    "inbox-view-hibernated",
                    "inbox-view-dissolved",
                ] {
                    assert!(window.find(id).visible());
                }
            }
            view.update(cx, |v, cx| {
                v.set_main_surface(MainSurface::Chat);
                let focus = v.prompt_input.read(cx).focus_handle(cx);
                window.focus(&focus, cx);
            });
            window.render_frame(cx);
            window.press("alt-g", cx);
            window.press("e", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Chat);
            assert_eq!(
                view.read(cx).prompt_input.read(cx).value().as_ref(),
                "drafte"
            );
            window.press("alt-i", cx);
            window.render_frame(cx);
            window.press("g", cx);
            window.render_frame(cx);
            window.press("e", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).inbox_state.lifecycle,
                SessionLifecycle::Active
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn lifecycle_done_hibernate_modes_peek_reply_and_timer(cx: &mut TestAppContext) {
        use multibot_core::workspaces::SessionLifecycle;
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                let mut session = make_session("hidden", AgentKind::Claude, "Hidden", false);
                session.last_assistant_id = Some("reply-1".into());
                session.last_assistant_ms = Some(100);
                v.sessions = vec![session.clone()];
                v.select_session(session, cx);
                let focus = v.prompt_input.read(cx).focus_handle(cx);
                window.focus(&focus, cx);
            });
            window.render_frame(cx);
            window.press("alt-e", cx);
            window.press("alt-shift-e", cx);
            window.render_frame(cx);
            assert!(window.find("inbox-row-0").visible());
            window.click("inbox-row-0", cx);
            window.render_frame(cx);
            assert!(window.find("lifecycle-peek-banner").visible());
            view.read_with(cx, |v, _| {
                let s = v.selected_session.as_ref().unwrap();
                let key = v.session_key_for(s);
                assert_eq!(
                    v.sidebar_state
                        .workspace_store
                        .session_lifecycle(&key, s, now_unix_ms()),
                    SessionLifecycle::Done
                );
                assert_eq!(
                    v.sidebar_state
                        .workspace_store
                        .compute_sidebar_layout(
                            &v.sidebar_state.active_workspace_id,
                            &v.session_inputs(),
                            false,
                            now_unix_ms()
                        )
                        .total_visible_sessions(),
                    0
                );
            });
            window.press("alt-z", cx);
            window.render_frame(cx);
            assert!(window.try_find("lifecycle-peek-banner").is_none());
            for wake in [false, true] {
                window.press("ctrl-h", cx);
                window.render_frame(cx);
                assert!(view.read(cx).palette_state.hibernate_wake_on_reply);
                if !wake {
                    window.press("tab", cx);
                    window.render_frame(cx);
                }
                assert_eq!(view.read(cx).palette_state.hibernate_wake_on_reply, wake);
                window.click("pal-reminder-0", cx);
                window.render_frame(cx);
                window.press("alt-shift-h", cx);
                window.render_frame(cx);
                assert!(window.find("inbox-row-0").visible());
                window.click("inbox-row-0", cx);
                window.render_frame(cx);
                view.update(cx, |v, cx| {
                    let mut s = v.selected_session.clone().unwrap();
                    let key = v.session_key_for(&s);
                    assert_eq!(
                        v.sidebar_state
                            .workspace_store
                            .session_lifecycle(&key, &s, now_unix_ms()),
                        SessionLifecycle::Hibernated
                    );
                    s.last_assistant_id = Some(format!("reply-{wake}"));
                    s.last_assistant_ms = Some(200);
                    v.handle_bridge_event(
                        UiBridgeEvent::Server(ServerEvent::SessionUpdated { session: s.clone() }),
                        cx,
                    );
                    assert_eq!(
                        v.sidebar_state
                            .workspace_store
                            .session_lifecycle(&key, &s, now_unix_ms()),
                        if wake {
                            SessionLifecycle::Active
                        } else {
                            SessionLifecycle::Hibernated
                        }
                    );
                    if !wake {
                        assert!(
                            v.sidebar_state
                                .workspace_store
                                .wake_due_sessions(now_unix_ms() + 3_600_000)
                        );
                        v.sync_workspace_to_shared(cx);
                    }
                });
                window.render_frame(cx);
            }
            view.update(cx, |v, cx| {
                let mut session = v.selected_session.clone().unwrap();
                let key = v.session_key_for(&session);
                v.sidebar_state.workspace_store.archive_to_waiting_session(
                    &key,
                    now_unix_ms(),
                    &session,
                );
                v.sync_workspace_to_shared(cx);
                session.last_assistant_id = Some("reply-after-reconnect".into());
                v.handle_bridge_event(
                    UiBridgeEvent::Server(ServerEvent::Sessions {
                        sessions: vec![session.clone()],
                    }),
                    cx,
                );
                assert_eq!(
                    v.sidebar_state.workspace_store.session_lifecycle(
                        &key,
                        &session,
                        now_unix_ms()
                    ),
                    SessionLifecycle::Active
                );
                assert!(
                    !cx.global::<SharedAppStore>()
                        .workspace_store
                        .waiting_sessions
                        .contains_key(&key)
                );
            });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn lifecycle_dissolved_peek_keeps_workspace_and_reactivates_exact_history(
        cx: &mut TestAppContext,
    ) {
        use multibot_core::workspaces::SessionLifecycle;
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            let (tx, mut rx) = mpsc::unbounded_channel();
            view.update(cx, |v, cx| {
                let session = make_session("saved", AgentKind::Claude, "Saved", false);
                let key = v.session_key_for(&session);
                let workspace = v.sidebar_state.workspace_store.create_workspace("Saved workspace", None).unwrap();
                v.sidebar_state.workspace_store.assign_session_workspace(&key, &workspace.id);
                v.sessions = vec![session.clone()];
                v.daemon_tx = Some(tx);
                v.chat_cache.put(CacheKey::new("local", AgentKind::Claude, "saved"), CachedSession {
                    source_path: session.source_path.clone(), messages: vec![ChatMessage {
                        id: "saved-reply".into(), role: MessageRole::Assistant, text: "Saved full history".into(), ..Default::default()
                    }], ..Default::default()
                });
                v.select_session(session, cx);
                v.sync_sessions_to_shared(cx);
            });
            window.render_frame(cx);
            while rx.try_recv().is_ok() {}
            window.press("ctrl-shift-w", cx);
            window.render_frame(cx);
            assert!(matches!(rx.try_recv(), Ok(ClientRequest::ArchiveSession { ref session_id, kill_process: true, .. }) if session_id == "saved"));
            window.press("alt-shift-d", cx);
            window.render_frame(cx);
            assert!(window.find("inbox-row-0").visible());
            window.click("inbox-row-0", cx);
            window.render_frame(cx);
            assert!(window.find("move-chat-to-inbox").visible());
            view.read_with(cx, |v, _| {
                let session = v.selected_session.as_ref().unwrap();
                let key = v.session_key_for(session);
                assert_eq!(v.messages[0].text, "Saved full history");
                let serialized = serde_json::to_string(&v.sidebar_state.workspace_store).unwrap();
                let restored: multibot_core::workspaces::WorkspaceStore = serde_json::from_str(&serialized).unwrap();
                assert_eq!(restored.session_lifecycle(&key, session, now_unix_ms()), SessionLifecycle::Dissolved);
                assert_eq!(restored.workspace_by_id(&restored.effective_workspace_id(&key)).unwrap().name, "Saved workspace");
            });
            while let Ok(request) = rx.try_recv() {
                assert!(matches!(request, ClientRequest::ReadHistory { .. }), "peek must only read history, got {request:?}");
            }
            view.update(cx, |v, _| {
                let session = v.selected_session.as_ref().unwrap();
                let key = ChatOutbox::session_key("local", session.agent, &session.id);
                v.shared_chat_outbox.lock().unwrap().outbox.enqueue(&key, "queued before dissolve", now_unix_ms()).unwrap();
                assert_eq!(v.flush_outbox_for_host("local"), 0);
            });
            assert!(rx.try_recv().is_err());
            window.click("move-chat-to-inbox", cx);
            window.render_frame(cx);
            assert!(matches!(rx.try_recv(), Ok(ClientRequest::UnarchiveSession { ref session_id }) if session_id == "saved"));
            assert!(!view.read(cx).selected_session.as_ref().unwrap().archived);
            window.press("ctrl-shift-w", cx);
            window.press("ctrl-shift-t", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).selected_session.as_ref().unwrap().id, "saved");
            assert!(!view.read(cx).selected_session.as_ref().unwrap().archived);
            assert_eq!(view.read(cx).messages[0].text, "Saved full history");
            while rx.try_recv().is_ok() {}
            window.press("ctrl-shift-w", cx);
            window.press("alt-shift-d", cx);
            window.render_frame(cx);
            window.click("inbox-row-0", cx);
            window.render_frame(cx);
            while rx.try_recv().is_ok() {}
            view.update(cx, |v, cx| {
                v.prompt_input.update(cx, |input, cx| { input.set_value("Continue saved chat", window, cx); input.focus(window, cx); });
            });
            window.render_frame(cx);
            window.press("ctrl-enter", cx);
            window.render_frame(cx);
            assert!(matches!(rx.try_recv(), Ok(ClientRequest::UnarchiveSession { ref session_id }) if session_id == "saved"));
            assert!(matches!(rx.try_recv(), Ok(ClientRequest::SendPrompt { ref session_id, ref text, .. }) if session_id == "saved" && text == "queued before dissolve"));
            view.update(cx, |v, cx| {
                for working in [true, false] {
                    let mut session = v.selected_session.clone().unwrap();
                    session.working = working;
                    v.handle_bridge_event(UiBridgeEvent::Server(ServerEvent::SessionUpdated { session }), cx);
                }
            });
            assert!(matches!(rx.try_recv(), Ok(ClientRequest::SendPrompt { ref session_id, ref text, .. }) if session_id == "saved" && text == "Continue saved chat"));
            assert!(!view.read(cx).selected_session.as_ref().unwrap().archived);

            view.update(cx, |v, cx| {
                let session = v.selected_session.clone().unwrap();
                let key = v.session_key_for(&session);
                v.dissolve_session_key(&key, cx);
                let (host, session) = v.host_for_session_key(&key).unwrap();
                v.open_session_on_host(host, session, true, cx);
                while rx.try_recv().is_ok() {}
                v.chat_state.failed_sends.insert("retry-saved".into(), FailedSend {
                    message_id: "retry-saved".into(), text: "Retry saved chat".into(), reason: "offline".into(),
                });
                assert!(v.retry_failed_send("retry-saved", cx));
                assert!(!v.selected_session.as_ref().unwrap().archived);
                assert!(matches!(rx.try_recv(), Ok(ClientRequest::UnarchiveSession { ref session_id }) if session_id == "saved"));
                assert!(matches!(rx.try_recv(), Ok(ClientRequest::SendPrompt { ref session_id, ref text, .. }) if session_id == "saved" && text == "Retry saved chat"));
            });
        }).unwrap();
    }

    #[gpui_kit::test]
    fn reminder_and_snooze_shortcuts_presets_and_inbox_dismiss(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                let session = make_session("reminder", AgentKind::Claude, "Follow up", false);
                v.sessions = vec![session.clone()];
                v.selected_session = Some(session);
                v.sync_sessions_to_shared(cx);
                cx.notify();
            });
            window.render_frame(cx);
            window.press("ctrl-h", cx);
            window.render_frame(cx);
            assert!(window.find("pal-reminder-0").visible());
            window.click("pal-reminder-0", cx);
            window.render_frame(cx);
            view.read_with(cx, |v, cx| {
                let key = v.session_key_for(v.selected_session.as_ref().unwrap());
                assert!(
                    v.sidebar_state.workspace_store.session_meta[&key]
                        .remind_at_ms
                        .unwrap()
                        > now_unix_ms()
                );
                assert!(cx.global::<SharedAppStore>().reminder_timer.is_some());
            });
            window.press("alt-h", cx);
            window.render_frame(cx);
            window.click("pal-reminder-4", cx);
            window.render_frame(cx);
            assert!(cx.global::<SharedAppStore>().reminder_deadline_ms.is_none());

            window.press("ctrl-shift-y", cx);
            window.render_frame(cx);
            window.click("pal-reminder-0", cx);
            window.render_frame(cx);
            assert!(
                view.read(cx)
                    .sidebar_state
                    .workspace_store
                    .visible_workspaces(now_unix_ms())
                    .is_empty()
            );
            view.update(cx, |v, cx| {
                let key = v.session_key_for(v.selected_session.as_ref().unwrap());
                let ws = v.sidebar_state.workspace_store.effective_workspace_id(&key);
                v.set_workspace_snooze(&ws, None, cx);
                v.sidebar_state.workspace_store.set_reminder(
                    &key,
                    Some(now_unix_ms() - 1),
                    Some("Follow up"),
                );
                v.main_surface = MainSurface::Inbox;
                v.inbox_state.select_all_split();
                v.sidebar_state.unread_only = true;
                v.sync_workspace_to_shared(cx);
                cx.notify();
            });
            window.render_frame(cx);
            assert!(window.find("dismiss-reminder-0").visible());
            window.click("dismiss-reminder-0", cx);
            window.render_frame(cx);
            view.read_with(cx, |v, _| {
                let key = v.session_key_for(v.selected_session.as_ref().unwrap());
                assert!(
                    !v.sidebar_state
                        .workspace_store
                        .reminder_due(&key, now_unix_ms())
                );
            });
        })
        .unwrap();
    }

    fn install_test_host(
        cx: &mut App,
        label: &str,
    ) -> (
        mpsc::UnboundedReceiver<ClientRequest>,
        oneshot::Receiver<()>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let store = cx.global_mut::<SharedAppStore>();
        store.host_connections.insert(
            label.into(),
            HostConnectionState {
                host_label: label.into(),
                target: DaemonTarget::LocalBinary("/unused-in-headless-tests".into()),
                status: "Connected".into(),
                connected: true,
                manually_disconnected: false,
                reconnect_attempt: 0,
                daemon_tx: Some(tx),
                shutdown_tx: Some(shutdown_tx),
                connection_gen: 1,
                daemon_version: Some("0.1.0".into()),
                daemon_binary_hash: None,
                daemon_upgrade_attempted: false,
                sessions: Vec::new(),
            },
        );
        store.refresh_connection_summary();
        store.bump_revision();
        (rx, shutdown_rx)
    }

    #[gpui_kit::test]
    fn multi_host_sessions_merge_with_real_host_keys_and_workspace_metadata(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        let rt = cx
            .update_window(handle, |_, _, cx| view.read(cx).runtime.clone())
            .unwrap();
        let (_rx_a, _shutdown_a) = cx.update(|cx| install_test_host(cx, "host-a"));
        let (_rx_b, _shutdown_b) = cx.update(|cx| install_test_host(cx, "host-b"));
        let a = make_session("shared-session-id", AgentKind::Claude, "A", false);
        let mut b = a.clone();
        b.title = "B".into();
        b.updated_at_ms += 1;
        let key_a = canonical_session_key("host-a", a.agent, &a.id);
        let key_b = canonical_session_key("host-b", b.agent, &b.id);
        cx.update(|cx| {
            let store = cx.global_mut::<SharedAppStore>();
            let workspace_a = store.workspace_store.create_workspace("A", None).unwrap();
            let workspace_b = store.workspace_store.create_workspace("B", None).unwrap();
            store
                .workspace_store
                .assign_session_workspace(&key_a, &workspace_a.id);
            store
                .workspace_store
                .assign_session_workspace(&key_b, &workspace_b.id);
            for (host, session) in [("host-a", a.clone()), ("host-b", b.clone())] {
                assert!(MultibotView::apply_host_bridge_event_to_app(
                    host,
                    1,
                    UiBridgeEvent::Server(ServerEvent::Sessions {
                        sessions: vec![session]
                    }),
                    &rt,
                    cx
                ));
            }
            let store = cx.global::<SharedAppStore>();
            assert_eq!(store.connection_status, "2/2 hosts · 2 sessions");
            assert_eq!(
                store.host_sessions,
                vec![("host-b".into(), b.clone()), ("host-a".into(), a.clone())]
            );
            assert_eq!(store.sessions, vec![b, a]);
            assert_eq!(
                store.workspace_store.session_meta[&key_a]
                    .workspace_id
                    .as_deref(),
                Some(workspace_a.id.as_str())
            );
            assert_eq!(
                store.workspace_store.session_meta[&key_b]
                    .workspace_id
                    .as_deref(),
                Some(workspace_b.id.as_str())
            );
        });
        cx.run_until_parked();
        cx.update_window(handle, |_, _, cx| {
            view.update(cx, |v, cx| {
                v.sync_from_shared_store(cx);
                let inputs = v.session_inputs();
                assert_eq!(
                    inputs.iter().map(|s| s.host).collect::<Vec<_>>(),
                    vec!["host-b", "host-a"]
                );
                assert_eq!(
                    inputs.iter().map(|s| s.key.as_str()).collect::<Vec<_>>(),
                    vec![key_b.as_str(), key_a.as_str()]
                );
            });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn multi_host_chat_routes_identical_session_ids_and_rejects_other_host_history(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        let rt = cx
            .update_window(handle, |_, _, cx| view.read(cx).runtime.clone())
            .unwrap();
        let (mut rx_a, _shutdown_a) = cx.update(|cx| install_test_host(cx, "host-a"));
        let (mut rx_b, _shutdown_b) = cx.update(|cx| install_test_host(cx, "host-b"));
        let session = make_session("same-session", AgentKind::Claude, "Same", false);
        cx.update(|cx| {
            for host in ["host-a", "host-b"] {
                MultibotView::apply_host_bridge_event_to_app(
                    host,
                    1,
                    UiBridgeEvent::Server(ServerEvent::Sessions {
                        sessions: vec![session.clone()],
                    }),
                    &rt,
                    cx,
                );
            }
        });
        cx.run_until_parked();
        while rx_a.try_recv().is_ok() {}
        while rx_b.try_recv().is_ok() {}
        for host in ["host-a", "host-b"] {
            cx.update_window(handle, |_, _, cx| {
                view.update(cx, |v, cx| {
                    let key = canonical_session_key(host, session.agent, &session.id);
                    v.open_session_key_with_tracking(&key, false, cx);
                    assert_eq!(v.selected_session_host.as_deref(), Some(host));
                    assert_eq!(v.active_switch_token.as_ref().unwrap().key.host, host);
                    v.send_or_queue_prompt(&session, "msg".into(), "hello".into());
                    assert!(v.interrupt_active_session(cx));
                });
            })
            .unwrap();
            let (rx, other) = if host == "host-a" {
                (&mut rx_a, &mut rx_b)
            } else {
                (&mut rx_b, &mut rx_a)
            };
            assert!(matches!(
                rx.try_recv().unwrap(),
                ClientRequest::ReadHistory { .. }
            ));
            assert!(matches!(
                rx.try_recv().unwrap(),
                ClientRequest::WatchSession { .. }
            ));
            assert!(matches!(
                rx.try_recv().unwrap(),
                ClientRequest::ListCommands { .. }
            ));
            assert!(
                matches!(rx.try_recv().unwrap(), ClientRequest::SendPrompt { text, .. } if text == "hello")
            );
            assert!(matches!(
                rx.try_recv().unwrap(),
                ClientRequest::InterruptSession { .. }
            ));
            assert!(other.try_recv().is_err());
        }
        cx.update_window(handle, |_, _, cx| {
            view.update(cx, |v, cx| {
                let message = ChatMessage {
                    id: "keep-b".into(),
                    role: MessageRole::Assistant,
                    text: "B".into(),
                    ..Default::default()
                };
                v.messages = vec![message.clone()];
                v.handle_daemon_server_event_with_hint(
                    "host-a",
                    ServerEvent::History {
                        session_id: session.id.clone(),
                        messages: Vec::new(),
                        todos: Vec::new(),
                        oldest_offset: None,
                        latest_offset: None,
                        has_more: false,
                        file_size: None,
                        file_mtime_ms: None,
                        probe_sha256: None,
                    },
                    false,
                    cx,
                );
                assert_eq!(v.messages, vec![message]);
                MultibotView::disconnect_host_on_app("host-b", cx);
                v.sync_from_shared_store(cx);
                v.send_or_queue_prompt(&session, "offline".into(), "only for B".into());
                assert!(v.chat_outbox.entries.iter().any(|e| e.session_key
                    == ChatOutbox::session_key("host-b", session.agent, &session.id)));
                assert_eq!(v.flush_outbox_for_host("host-a"), 0);
                assert!(rx_a.try_recv().is_err());
            });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn multi_host_composer_options_follow_prompt_queue_and_spawn(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        let (mut rx_a, _shutdown_a) = cx.update(|cx| install_test_host(cx, "host-a"));
        let (mut rx_b, _shutdown_b) = cx.update(|cx| install_test_host(cx, "host-b"));
        let session = make_session("same-session", AgentKind::Claude, "Same", false);
        cx.update(|cx| {
            let store = cx.global_mut::<SharedAppStore>();
            for host in ["host-a", "host-b"] {
                store.host_connections.get_mut(host).unwrap().sessions = vec![session.clone()];
            }
            store.rebuild_merged_sessions();
            store.bump_revision();
        });
        let options_a = multibot_proto::SessionOptions {
            model: Some("opus".into()),
            effort: Some(multibot_proto::EffortLevel::High),
            permission: Some(multibot_proto::PermissionLevel::Auto),
        };
        let options_b = multibot_proto::SessionOptions {
            model: Some("sonnet".into()),
            effort: Some(multibot_proto::EffortLevel::Low),
            permission: Some(multibot_proto::PermissionLevel::Ask),
        };
        cx.update_window(handle, |_, _, cx| {
            view.update(cx, |v, cx| {
                v.sync_from_shared_store(cx);
                v.selected_session = Some(session.clone());
                for (host, options) in [("host-a", &options_a), ("host-b", &options_b)] {
                    let (rx, other) = if host == "host-a" {
                        (&mut rx_a, &mut rx_b)
                    } else {
                        (&mut rx_b, &mut rx_a)
                    };
                    v.selected_session_host = Some(host.into());
                    let key = canonical_session_key(host, session.agent, &session.id);
                    v.chat_state.composer_options.insert(key, options.clone());
                    v.send_or_queue_prompt(&session, "send".into(), "hello".into());
                    assert!(matches!(rx.try_recv(), Ok(ClientRequest::SendPrompt { options: Some(ref o), .. }) if o == options));
                    assert!(v.steer_active_session("steer".into(), cx));
                    assert!(matches!(rx.try_recv(), Ok(ClientRequest::SteerSession { options: Some(ref o), .. }) if o == options));
                    assert!(v.spawn_new_session(session.agent, ".".into(), None, None, cx));
                    assert!(matches!(rx.try_recv(), Ok(ClientRequest::SpawnSession { options: Some(ref o), .. }) if o == options));
                    assert!(other.try_recv().is_err());
                }
                v.send_or_queue_prompt_on_host("host-a", &session, "delayed".into(), "uploaded".into());
                assert!(rx_a.try_recv().is_err());
                assert_eq!(v.chat_outbox.entries.last().unwrap().options.as_ref(), Some(&options_a));
                for working in [true, false] {
                    let mut updated = session.clone();
                    updated.working = working;
                    let event = ServerEvent::SessionUpdated { session: updated };
                    MultibotView::apply_host_bridge_event_to_app("host-a", 1,
                        UiBridgeEvent::Server(event.clone()), &v.runtime, cx);
                    v.handle_daemon_server_event_with_hint("host-a", event, false, cx);
                }
                assert!(matches!(rx_a.try_recv(), Ok(ClientRequest::SendPrompt { options: Some(ref o), .. }) if o == &options_a));
                assert!(rx_b.try_recv().is_err());

                v.selected_session_host = Some("host-a".into());
                let spawned = make_session("spawned", session.agent, "New", false);
                v.handle_daemon_server_event_with_hint(
                    "host-a",
                    ServerEvent::SessionSpawned { session: spawned.clone() },
                    false,
                    cx,
                );
                assert_eq!(v.pending_spawn_host.as_deref(), Some("host-b"));
                assert_eq!(v.chat_state.pending_spawn_options, Some(options_b.clone()));
                v.handle_daemon_server_event_with_hint(
                    "host-b",
                    ServerEvent::SessionSpawned { session: spawned.clone() },
                    false,
                    cx,
                );
                assert_eq!(v.selected_session_host.as_deref(), Some("host-b"));
                assert_eq!(v.composer_options_for(&spawned), Some(options_b.clone()));
                assert!(!v.chat_state.composer_options.contains_key(
                    &canonical_session_key("host-a", spawned.agent, &spawned.id)
                ));
                assert!(v.chat_state.pending_spawn_options.is_none());
                assert!(v.pending_spawn_host.is_none());
                while rx_b.try_recv().is_ok() {}

                MultibotView::disconnect_host_on_app("host-b", cx);
                v.sync_from_shared_store(cx);
                v.send_or_queue_prompt(&spawned, "offline".into(), "queued".into());
                assert_eq!(v.chat_outbox.entries.last().unwrap().options, Some(options_b));
                assert_eq!(v.flush_outbox_for_host("host-a"), 0);
                assert!(rx_a.try_recv().is_err());
            });
        }).unwrap();
    }

    #[gpui_kit::test]
    fn multi_host_disconnect_preserves_other_host_and_rejects_stale_events(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        let rt = cx
            .update_window(handle, |_, _, cx| view.read(cx).runtime.clone())
            .unwrap();
        let (_rx_a, mut shutdown_a) = cx.update(|cx| install_test_host(cx, "host-a"));
        let (_rx_b, mut shutdown_b) = cx.update(|cx| install_test_host(cx, "host-b"));
        cx.update(|cx| {
            for host in ["host-a", "host-b"] {
                MultibotView::apply_host_bridge_event_to_app(
                    host,
                    1,
                    UiBridgeEvent::Server(ServerEvent::Sessions {
                        sessions: vec![make_session("same", AgentKind::Claude, host, false)],
                    }),
                    &rt,
                    cx,
                );
            }
            MultibotView::disconnect_host_on_app("host-a", cx);
            assert!(!MultibotView::apply_host_bridge_event_to_app(
                "host-a",
                1,
                UiBridgeEvent::Server(ServerEvent::Sessions {
                    sessions: Vec::new()
                }),
                &rt,
                cx
            ));
            let store = cx.global::<SharedAppStore>();
            assert_eq!(store.connection_status, "1/2 hosts · 1 sessions");
            assert_eq!(store.host_sessions.len(), 1);
            assert_eq!(store.host_sessions[0].0, "host-b");
            assert!(store.host_connections["host-a"].manually_disconnected);
            assert!(store.host_connections["host-b"].connected);
            assert!(store.host_connections["host-b"].daemon_tx.is_some());
        });
        assert!(matches!(
            shutdown_a.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
        assert!(matches!(
            shutdown_b.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
    }

    #[gpui_kit::test]
    fn multi_host_connect_and_reconnect_replace_only_the_requested_host(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        let rt = cx
            .update_window(handle, |_, _, cx| view.read(cx).runtime.clone())
            .unwrap();
        let (_rx_a, mut shutdown_a) = cx.update(|cx| install_test_host(cx, "host-a"));
        let (_rx_b, mut shutdown_b) = cx.update(|cx| install_test_host(cx, "host-b"));
        cx.update(|cx| {
            let generation = MultibotView::connect_host_on_app(
                rt.clone(),
                "host-b".into(),
                DaemonTarget::LocalBinary("/unused-in-headless-tests".into()),
                false,
                cx,
            );
            assert_eq!(generation, 2);
            let store = cx.global::<SharedAppStore>();
            assert_eq!(store.host_connections["host-a"].connection_gen, 1);
            assert!(store.host_connections["host-a"].connected);
            assert!(store.host_connections["host-a"].daemon_tx.is_some());
            assert_eq!(store.connection_status, "1/2 hosts · 0 sessions");
            assert!(!MultibotView::apply_host_bridge_event_to_app(
                "host-b",
                1,
                UiBridgeEvent::Disconnected("stale".into()),
                &rt,
                cx
            ));
            let generation = MultibotView::connect_host_on_app(
                rt.clone(),
                "host-b".into(),
                DaemonTarget::LocalBinary("/unused-in-headless-tests".into()),
                false,
                cx,
            );
            assert_eq!(generation, 3);
            assert_eq!(
                cx.global::<SharedAppStore>().host_connections["host-a"].connection_gen,
                1
            );
        });
        assert!(matches!(
            shutdown_a.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            shutdown_b.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
    }

    #[gpui_kit::test]
    fn multi_host_connections_live_until_last_window_closes(cx: &mut TestAppContext) {
        let (handle1, view1) = open_test_view_with_entity(cx);
        let (handle2, view2) = open_test_view_with_entity(cx);
        let (_rx_a, mut shutdown_a) = cx.update(|cx| install_test_host(cx, "host-a"));
        let (_rx_b, mut shutdown_b) = cx.update(|cx| install_test_host(cx, "host-b"));
        drop(view1);
        cx.update_window(handle1, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            let store = cx.global::<SharedAppStore>();
            assert_eq!(store.window_count, 1);
            assert_eq!(store.host_connections.len(), 2);
            assert!(
                store
                    .host_connections
                    .values()
                    .all(|h| h.connected && h.daemon_tx.is_some())
            );
        });
        assert!(matches!(
            shutdown_a.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            shutdown_b.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        drop(view2);
        cx.update_window(handle2, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            let store = cx.global::<SharedAppStore>();
            assert_eq!(store.window_count, 0);
            assert!(store.host_connections.is_empty());
            assert!(store.host_sessions.is_empty());
            assert!(store.daemon_tx.is_none());
        });
        assert!(matches!(
            shutdown_a.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
        assert!(matches!(
            shutdown_b.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
    }

    #[gpui_kit::test]
    fn wave1_general_preferences_control_send_key_and_queue_delivery(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        let (tx, mut rx) = mpsc::unbounded_channel();
        cx.update_window(handle, |_, window, cx| {
            let session = make_session("prefs", AgentKind::Claude, "Preferences", true);
            {
                let store = cx.global_mut::<SharedAppStore>();
                store.daemon_tx = Some(tx.clone());
                store.sessions = vec![session.clone()];
                store.bump_revision();
            }
            view.update(cx, |v, cx| {
                v.sync_from_shared_store(cx);
                v.selected_session = Some(session);
            });
            window.render_frame(cx);
            window.press("ctrl-,", cx);
            window.render_frame(cx);
            window.render_frame(cx);
            for _ in 0..20 {
                window.press("tab", cx);
                window.render_frame(cx);
                assert!(view.read(cx).settings_focus_handle.contains_focused(window, cx));
            }
            window.press("escape", cx);
            window.render_frame(cx);
            window.click("prompt-input", cx);
            window.input("Send now", cx);
            window.press("enter", cx);
            window.render_frame(cx);
            assert!(rx.try_recv().is_err(), "Enter must not send in Ctrl+Enter mode");
            window.press("ctrl-enter", cx);
            window.render_frame(cx);
            assert!(rx.try_recv().is_err());
            assert_eq!(view.read(cx).chat_outbox.entries[0].text, "Send now");
            view.update(cx, |v, cx| {
                v.settings.queue_while_working = true;
                v.prompt_input.update(cx, |input, cx| input.set_value("Wait for agent", window, cx));
            });
            window.press("ctrl-enter", cx);
            window.render_frame(cx);
            assert!(rx.try_recv().is_err());
            assert_eq!(view.read(cx).chat_outbox.entries[1].text, "Wait for agent");
        }).unwrap();
    }

    #[gpui_kit::test]
    fn wave1_queue_bar_tracks_daemon_acknowledgements_and_offline_outbox(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.sessions = vec![make_session("queued", AgentKind::Claude, "Queue", true)];
                v.selected_session = Some(v.sessions[0].clone());
                cx.notify();
            });
            window.render_frame(cx);
            assert!(window.try_find("queued-messages-bar").is_none());
            view.update(cx, |v, cx| {
                v.handle_daemon_server_event(
                    ServerEvent::PromptStatus {
                        session_id: "queued".into(),
                        message_id: "q-1".into(),
                        status: "queued".into(),
                        detail: String::new(),
                    },
                    cx,
                )
            });
            window.render_frame(cx);
            assert!(window.try_find("queued-messages-bar").is_some());
            view.update(cx, |v, cx| {
                v.handle_daemon_server_event(
                    ServerEvent::PromptStatus {
                        session_id: "queued".into(),
                        message_id: "q-1".into(),
                        status: "sent".into(),
                        detail: String::new(),
                    },
                    cx,
                )
            });
            window.render_frame(cx);
            assert!(window.try_find("queued-messages-bar").is_none());
            view.update(cx, |v, cx| {
                let session = v.selected_session.clone().unwrap();
                v.send_or_queue_prompt(&session, "offline-1".into(), "Offline draft".into());
                cx.notify();
            });
            window.render_frame(cx);
            assert!(window.try_find("queued-messages-bar").is_some());
            view.update(cx, |v, cx| {
                v.cancel_outbox_message("offline-1");
                cx.notify();
            });
            window.render_frame(cx);
            assert!(window.try_find("queued-messages-bar").is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn wave1_updates_offers_install_even_with_hidden_sidebar_and_dismissed_card(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.set_sidebar_mode_synced(SidebarMode::Hidden, cx);
                v.settings.dismissed_update_version = Some("9.9.9".into());
                v.update_ui
                    .set_update_state(UpdateState::Available(ReleaseInfo {
                        version: "9.9.9".into(),
                        tag: "v9.9.9".into(),
                        published_at: String::new(),
                        notes: vec![],
                        asset: WindowsAssetInfo {
                            asset_name: "multibot-windows-x64.zip".into(),
                            url: "https://example.invalid/release.zip".into(),
                            sha256: "a".repeat(64),
                            size_bytes: 1024,
                            multibot_exe_sha256: "b".repeat(64),
                            multibotd_linux_x86_64_sha256: "c".repeat(64),
                        },
                    }));
                cx.notify();
            });
            window.render_frame(cx);
            assert!(window.try_find("update-card").is_none());
            window.press("ctrl-,", cx);
            window.render_frame(cx);
            window.click("settings-tab-updates", cx);
            window.render_frame(cx);
            assert!(window.try_find("settings-version-label").is_some());
            assert!(window.try_find("update-primary-btn").is_some());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn account_settings_and_palette_keep_secrets_out_of_saved_voice_settings(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            window.press("ctrl-,", cx);
            window.render_frame(cx);
            window.click("settings-tab-account", cx);
            window.render_frame(cx);
            assert!(window.try_find("account-sign-in").is_some());
            assert!(window.try_find("account-sign-out").is_none());
            let catalog = view.read(cx).build_palette_command_catalog(now_unix_ms());
            assert!(
                catalog
                    .iter()
                    .any(|c| c.label == "Account: Sign in to Multibot")
            );
            assert!(catalog.iter().any(|c| c.label == "Account: Sign out"));
            let local_key = view.read(cx).settings.voice.api_key.clone();
            {
                let store = cx.global_mut::<SharedAppStore>();
                store.account.email = Some("user@example.com".into());
                store.account.deepgram_key = Some("account-test-key".into());
                store.account.status = "Deepgram key synced from account".into();
                store.bump_revision();
            }
            view.update(cx, |_, cx| cx.notify());
            window.render_frame(cx);
            assert!(window.try_find("account-email").is_some());
            assert!(window.try_find("account-sync").is_some());
            assert!(window.try_find("account-sign-out").is_some());
            assert!(window.try_find("account-sign-in").is_none());
            assert_eq!(view.read(cx).settings.voice.api_key, local_key);
            assert!(
                !format!("{:?}", cx.global::<SharedAppStore>().account)
                    .contains("account-test-key")
            );
            view.update(cx, |v, cx| {
                cx.global_mut::<SharedAppStore>().account.busy = true;
                v.start_account_job(crate::account::AccountJob::SignOut, cx);
            });
            assert!(cx.global::<SharedAppStore>().account.email.is_some());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn focused_composer_ctrl_d_dictates_and_shift_d_still_copies_code(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                // An invalid manual override makes this independent of host credentials/audio.
                v.settings.voice.api_key = "invalid key".into();
                v.messages = vec![ChatMessage {
                    role: MessageRole::Assistant,
                    text: "```rust\nlet answer = 42;\n```".into(),
                    ..Default::default()
                }];
                v.prompt_input.update(cx, |input, cx| {
                    input.set_value("Unsent draft", window, cx);
                    input.focus(window, cx);
                });
                cx.notify();
            });
            window.render_frame(cx);
            window.press("ctrl-d", cx);
            assert!(view.read(cx).notice.as_deref().unwrap().contains("API key"));
            assert_eq!(
                view.read(cx).prompt_input.read(cx).value().as_str(),
                "Unsent draft"
            );
            view.update(cx, |v, _| {
                v.notice = None;
                v.voice.phase.start();
            });
            window.press("ctrl-shift-d", cx);
            assert_eq!(
                cx.read_from_clipboard().and_then(|c| c.text()).as_deref(),
                Some("let answer = 42;")
            );
            assert_eq!(
                view.read(cx).voice.phase,
                multibot_core::voice::DictationPhase::Listening
            );
            window.press("ctrl-alt-d", cx);
            assert_eq!(
                view.read(cx).voice.phase,
                multibot_core::voice::DictationPhase::Listening
            );
            window.press("ctrl-d", cx);
            assert_eq!(
                view.read(cx).voice.phase,
                multibot_core::voice::DictationPhase::Finishing
            );
            assert_eq!(
                view.read(cx).prompt_input.read(cx).value().as_str(),
                "Unsent draft"
            );
            view.update(cx, |v, _| v.voice.phase.start());
            window.press("ctrl-shift-space", cx);
            assert_eq!(
                view.read(cx).voice.phase,
                multibot_core::voice::DictationPhase::Finishing
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn dictation_shortcut_and_mic_report_invalid_key_without_changing_draft(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.settings.voice.api_key = "invalid key".into();
                v.prompt_input
                    .update(cx, |input, cx| input.set_value("Unsent draft", window, cx));
            });
            window.press("ctrl-shift-space", cx);
            assert!(view.read(cx).notice.as_deref().unwrap().contains("API key"));
            assert_eq!(
                view.read(cx).voice.phase,
                multibot_core::voice::DictationPhase::Off
            );
            view.update(cx, |v, _| v.notice = None);
            window.render_frame(cx);
            window.click("composer-voice", cx);
            assert!(view.read(cx).notice.as_deref().unwrap().contains("API key"));
            assert_eq!(
                view.read(cx).prompt_input.read(cx).value().as_str(),
                "Unsent draft"
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn dictation_settings_save_language_and_key_and_escape_stops_listening(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            window.press("ctrl-,", cx);
            window.render_frame(cx);
            window.click("settings-tab-voice", cx);
            window.render_frame(cx);
            assert!(window.try_find("deepgram-api-key").is_some());
            assert!(window.try_find("deepgram-language").is_some());
            view.update(cx, |v, cx| {
                v.deepgram_key_input
                    .update(cx, |input, cx| input.set_value("test-key", window, cx));
                v.deepgram_language_input
                    .update(cx, |input, cx| input.set_value("de", window, cx));
            });
            window.click("save-deepgram-settings", cx);
            assert_eq!(view.read(cx).settings.voice.api_key, "test-key");
            assert_eq!(view.read(cx).settings.voice.language, "de");
            assert_eq!(cx.global::<SharedAppStore>().settings.voice.language, "de");
            window.press("escape", cx);
            view.update(cx, |v, cx| {
                v.voice.phase.start();
                v.prompt_input.update(cx, |input, cx| {
                    input.set_value("Keep this draft", window, cx);
                    input.focus(window, cx);
                });
            });
            window.press("escape", cx);
            assert_eq!(
                view.read(cx).voice.phase,
                multibot_core::voice::DictationPhase::Off
            );
            assert_eq!(
                view.read(cx).prompt_input.read(cx).value().as_str(),
                "Keep this draft"
            );
            view.update(cx, |v, _| {
                v.voice.phase.start();
                v.set_main_surface(MainSurface::Inbox);
                assert_eq!(v.voice.phase, multibot_core::voice::DictationPhase::Off);
            });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn shell_fullscreen_restores_on_first_frame_and_shortcut_saves_both_choices(
        cx: &mut TestAppContext,
    ) {
        let settings: MultibotSettings =
            serde_json::from_str(r#"{"shell_fullscreen":true}"#).unwrap();
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                gpui_kit::point(px(0.0), px(0.0)),
                size(px(1440.0), px(900.0)),
            ))),
            ..Default::default()
        };
        let (handle, view) = open_test_view_with_settings(cx, settings, options);
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                let shell = make_session(
                    "term-56d16ef56ce668ab2d7235d20e7ecd75",
                    AgentKind::Shell,
                    "Multi Bot AGY Shell",
                    false,
                );
                v.sessions = vec![shell.clone()];
                let key = v.session_key_for(&shell);
                v.sidebar_state.workspace_store.session_meta.entry(key).or_default().full_width = true;
                v.select_session(shell, cx);
            });
            window.render_frame(cx);
            let wide = window.find("server-shell-panel").bounds().size.width;
            assert!(
                wide > px(760.0),
                "saved full-width must apply before the first frame: {wide:?}"
            );
            assert!(view.read(cx).settings.shell_fullscreen);
            window.press("ctrl-shift-f", cx);
            window.render_frame(cx);
            assert!(!view.read(cx).settings.shell_fullscreen);
            assert!(!cx.global::<SharedAppStore>().workspace_store.session_meta.values().any(|meta| meta.full_width));
            assert_eq!(
                window.find("server-shell-panel").bounds().size.width,
                px(760.0)
            );
            window.press("ctrl-shift-f", cx);
            window.render_frame(cx);
            assert_eq!(window.find("server-shell-panel").bounds().size.width, wide);
            assert!(cx.global::<SharedAppStore>().workspace_store.session_meta.values().any(|meta| meta.full_width));
            window.press("ctrl-k", cx);
            window.input("Toggle Full-Width", cx);
            window.render_frame(cx);
            window.click("pal-cmd-fullwidth", cx);
            window.render_frame(cx);
            assert!(!view.read(cx).settings.shell_fullscreen);
            assert_eq!(
                window.find("server-shell-panel").bounds().size.width,
                px(760.0)
            );
            view.update(cx, |v, cx| {
                v.select_session(make_session("chat", AgentKind::Claude, "Chat", false), cx)
            });
            window.render_frame(cx);
            window.press("ctrl-shift-f", cx);
            assert!(view.read(cx).settings.shell_fullscreen);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn shell_fullscreen_defaults_to_regular_width_on_initial_load(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                let shell = make_session("term-deadbeef", AgentKind::Shell, "Shell", false);
                v.sessions = vec![shell.clone()];
                v.select_session(shell, cx);
            });
            window.render_frame(cx);
            assert!(!view.read(cx).settings.shell_fullscreen);
            assert!(window.find("server-shell-panel").bounds().size.width <= px(760.0));
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn dev_stats_start_compact_and_toggle_by_click_and_escape_in_both_sidebar_modes(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, _| {
                v.gpu_report = multibot_gpu::parse_gpu_report("MODE\tauto_integrated\nACTIVE\t00000000:00012345\t8086\t9a49\t128\t0\tIntel(R) Arc(TM) Pro Graphics\n");
                v.footprint_label = "120.5 MB · 2.2% CPU".into();
                v.connection_status = "3/3 hosts · 106 sessions".into();
            });
            for mode in [SidebarMode::Open, SidebarMode::Collapsed] {
                view.update(cx, |v, cx| v.set_sidebar_mode_synced(mode, cx));
                window.render_frame(cx);
                assert!(!view.read(cx).sidebar_state.dev_stats_expanded);
                assert!(window.try_find("dev-stats-details").is_none());
                assert!(window.find("gpu-status-card").bounds().size.height <= px(24.0));
                window.click("gpu-status-card", cx);
                window.render_frame(cx);
                for id in ["dev-stats-details", "dev-stats-gpu", "dev-stats-footprint", "dev-stats-connection"] {
                    assert!(window.try_find(id).is_some(), "missing {id}");
                }
                let text = gpui_kit::base::test_support::texts(window).iter().map(|t| t.text().to_string()).collect::<Vec<_>>().join(" ");
                assert!(text.contains("GPU: Intel(R) Arc(TM) Pro Graphics (iGPU · 0% NVIDIA)"));
                assert!(text.contains("120.5 MB"));
                assert!(text.contains("2.2% CPU"));
                assert!(text.contains("3/3 hosts"));
                assert!(text.contains("106 sessions"));
                assert_eq!(view.read(cx).main_surface, MainSurface::Chat);
                window.click("gpu-status-card", cx);
                window.render_frame(cx);
                assert!(window.try_find("dev-stats-details").is_none());
                window.click("gpu-status-card", cx);
                window.render_frame(cx);
                window.press("escape", cx);
                window.render_frame(cx);
                assert!(!view.read(cx).sidebar_state.dev_stats_expanded);
                assert!(window.try_find("dev-stats-details").is_none());
                assert_eq!(view.read(cx).main_surface, MainSurface::Chat);
            }
        }).unwrap();
    }

    #[gpui_kit::test]
    fn wave1_clean_chrome_keeps_status_and_keyboard_entry_points(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            for id in [
                "sidebar-hosts-header",
                "connect-local",
                "import-zigshel-sidebar",
                "open-palette-btn",
                "new-window",
                "sidebar-inbox-btn",
                "toggle-sidebar-btn",
                "toggle-settings",
                "toggle-shortcuts",
            ] {
                assert!(
                    window.try_find(id).is_none(),
                    "Removed chrome still visible: {id}"
                );
            }
            for id in [
                "workspace-switcher",
                "gpu-status-card",
                "header-title-pill",
                "toggle-inbox",
                "chat-menu",
                "attach-file-btn",
                "prompt-input",
                "composer-voice",
                "send-prompt",
            ] {
                assert!(
                    window.try_find(id).is_some(),
                    "Missing minimal control: {id}"
                );
            }
            window.click("gpu-status-card", cx);
            window.render_frame(cx);
            assert!(window.try_find("dev-stats-details").is_some());
            window.click("dev-stats-gpu-settings", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).settings_tab,
                crate::settings::SettingsTab::Gpu
            );
            assert!(window.try_find("gpu-mode-auto").is_some());
            window.press("escape", cx);
            window.render_frame(cx);
            window.press("ctrl-k", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::Palette);
            window.press("escape", cx);
            window.press("f1", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::Shortcuts);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn workspace_header_palette_filters_all_favorites_and_named_workspace(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                let mut first = make_session("first", AgentKind::Claude, "First", false);
                first.updated_at_ms = 10;
                let mut second = make_session("second", AgentKind::Codex, "Second", false);
                second.updated_at_ms = 20;
                v.sessions = vec![first, second];
                let workspace = v
                    .sidebar_state
                    .workspace_store
                    .create_workspace("Work", None)
                    .unwrap();
                let key = v.session_key_for(&v.sessions[0]);
                v.sidebar_state
                    .workspace_store
                    .assign_session_workspace(&key, &workspace.id);
                v.sidebar_state.workspace_store.toggle_favorite(&key);
                v.open_session_with_tracking(v.sessions[0].clone(), false, cx);
                cx.notify();
            });
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).sidebar_state.active_workspace_id,
                multibot_core::workspaces::ALL_WORKSPACES_ID
            );
            assert!(window.try_find("session-0").is_some());
            assert!(window.try_find("session-1").is_some());
            assert!(window.try_find("workspace-menu").is_none());
            window.click("workspace-switcher", cx);
            window.render_frame(cx);
            assert!(window.try_find("workspace-menu").is_none());
            assert!(window.try_find("workspace-new").is_none());
            assert!(window.try_find("workspace-option-0").is_none());
            assert_eq!(view.read(cx).active_modal, ActiveModal::Palette);
            let rows = view.read(cx).build_palette_rows("", now_unix_ms());
            let names: Vec<_> = rows
                .iter()
                .map(|row| match row {
                    multibot_core::workspaces::PaletteRow::Workspace { name, .. } => name.as_str(),
                    other => panic!("workspace submenu contains {other:?}"),
                })
                .collect();
            assert_eq!(&names[..3], &["All", "Frequent", "Favorites"]);
            window.click("pal-ws-1", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).sidebar_state.active_workspace_view,
                WorkspaceView::Frequent
            );
            assert!(window.try_find("session-0").is_some());
            assert!(window.try_find("session-1").is_none());
            window.click("workspace-switcher", cx);
            window.render_frame(cx);
            window.click("pal-ws-2", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).sidebar_state.active_workspace_view,
                WorkspaceView::Favorites
            );
            assert!(window.try_find("workspace-menu").is_none());
            assert!(window.try_find("session-0").is_some());
            assert!(window.try_find("session-1").is_none());
            window.click("workspace-switcher", cx);
            window.render_frame(cx);
            window.click("pal-ws-4", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx)
                    .sidebar_state
                    .workspace_store
                    .workspace_by_id(&view.read(cx).sidebar_state.active_workspace_id)
                    .unwrap()
                    .name,
                "Work"
            );
            assert_eq!(
                view.read(cx).sidebar_state.active_workspace_view,
                WorkspaceView::All
            );
            assert!(window.try_find("session-0").is_some());
            assert!(window.try_find("session-1").is_none());
            let workspace_id = view.read(cx).sidebar_state.active_workspace_id.clone();
            window.press("alt-shift-2", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).sidebar_state.active_workspace_id,
                workspace_id
            );
            assert_eq!(
                view.read(cx).sidebar_state.active_workspace_view,
                WorkspaceView::Frequent
            );
            assert!(window.try_find("session-0").is_some());
            assert!(window.try_find("session-1").is_none());
            window.click("workspace-switcher", cx);
            window.render_frame(cx);
            window.click("pal-ws-0", cx);
            window.render_frame(cx);
            assert!(window.try_find("session-1").is_some());
            window.click("workspace-switcher", cx);
            window.render_frame(cx);
            window.press("escape", cx);
            window.render_frame(cx);
            assert!(window.try_find("workspace-menu").is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn wave1_settings_console_tabs_hosts_and_canonical_shortcuts(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, _| {
                v.settings.ssh_hosts.push(multibot_core::SshHostConfig {
                    name: "Saved VPS".into(),
                    hostname: "example.invalid".into(),
                    user: "user".into(),
                    port: 22,
                    identity_file: None,
                });
            });
            window.render_frame(cx);
            window.press("ctrl-,", cx);
            window.render_frame(cx);
            assert!(window.try_find("settings-console").is_some());
            assert!(window.try_find("settings-nav").is_some());
            assert!(window.try_find("settings-content").is_some());
            assert_eq!(
                view.read(cx).settings_tab,
                crate::settings::SettingsTab::General
            );
            assert!(window.try_find("composer-send-help").is_some());
            for (tab, control) in [
                ("settings-tab-hosts", "connect-local"),
                ("settings-tab-gpu", "gpu-mode-auto"),
                ("settings-tab-shortcuts", "settings-shortcut-0"),
                ("settings-tab-updates", "settings-version-label"),
                ("settings-tab-workspaces", "settings-new-workspace"),
            ] {
                window.click(tab, cx);
                window.render_frame(cx);
                assert!(window.try_find(control).is_some(), "Missing {control}");
                if tab == "settings-tab-hosts" {
                    assert!(window.try_find("host-0").is_some());
                    assert!(window.try_find("import-zigshel").is_some());
                    assert!(window.try_find("add-host").is_some());
                }
            }
            window.click("settings-tab-shortcuts", cx);
            window.render_frame(cx);
            for index in 0..multibot_keys::SHORTCUTS.len() {
                // Scrolled-off rows still exist in this non-virtualized reference.
                assert!(
                    window
                        .try_find(gpui_kit::SharedString::from(format!(
                            "settings-shortcut-{index}"
                        )))
                        .is_some()
                );
            }
            window.press("escape", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Chat);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn wave1_composer_controls_preserve_drafts_and_stop_working_session(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        let (tx, mut rx) = mpsc::unbounded_channel();
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.sessions = vec![make_session("active", AgentKind::Claude, "Active", true)];
                v.selected_session = Some(v.sessions[0].clone());
                v.daemon_tx = Some(tx);
                v.prompt_input.update(cx, |input, cx| input.set_value("Unsent draft", window, cx));
                cx.notify();
            });
            window.render_frame(cx);
            window.click("attach-file-btn", cx);
            window.render_frame(cx);
            for id in ["composer-options", "composer-model-1", "composer-effort-3", "composer-permission-2"] {
                assert!(window.try_find(id).is_some(), "Missing {id}");
            }
            window.click("composer-effort-3", cx);
            window.render_frame(cx);
            assert_eq!(window.find("composer-effort-3").checked(), Some(true));
            assert_eq!(view.read(cx).prompt_input.read(cx).value().as_str(), "Unsent draft");
            window.click("stop-agent", cx);
            window.render_frame(cx);
            assert!(matches!(rx.try_recv(), Ok(ClientRequest::InterruptSession { session_id, .. }) if session_id == "active"));
            assert_eq!(view.read(cx).prompt_input.read(cx).value().as_str(), "Unsent draft");
            view.update(cx, |v, cx| v.prompt_input.update(cx, |input, cx| input.set_value("", window, cx)));
            window.click("attach-file-btn", cx);
            window.render_frame(cx);
            assert!(window.try_find("composer-options").is_none());
            window.click("attach-file-btn", cx);
            window.render_frame(cx);
            window.click("composer-model-1", cx);
            window.render_frame(cx);
            assert_eq!(window.find("composer-model-1").checked(), Some(true));
            assert_eq!(window.find("composer-effort-3").checked(), Some(true));
            assert!(view.read(cx).prompt_input.read(cx).value().is_empty());
            let v = view.read(cx);
            let options = v.composer_options_for(v.selected_session.as_ref().unwrap()).unwrap();
            assert_eq!(options.model.as_deref(), Some("opus"));
            assert_eq!(options.effort, Some(multibot_proto::EffortLevel::High));
        }).unwrap();
    }

    // =====================================================================
    // Slice 5 — built-in Alt+C terminal pane (lazy lifecycle + obsidian grid)
    // =====================================================================

    /// Stub shell for headless tests: prints a marker and stays alive so the pane keeps a
    /// live child until the test closes it.
    fn stub_terminal_command() -> crate::terminal_pane::TerminalCommand {
        crate::terminal_pane::TerminalCommand {
            program: if cfg!(windows) {
                "cmd.exe".to_string()
            } else {
                "/bin/sh".to_string()
            },
            args: if cfg!(windows) {
                vec![
                    "/C".to_string(),
                    "echo mbru-6-terminal-ready & ping -n 30 127.0.0.1 >NUL".to_string(),
                ]
            } else {
                vec![
                    "-c".to_string(),
                    "echo mbru-6-terminal-ready; sleep 30".to_string(),
                ]
            },
            cwd: None,
        }
    }

    /// Waits (bounded) for the pane's grid to show `needle`. The PTY reader is a real OS
    /// thread, so the test must let it deliver bytes before asserting.
    fn wait_for_terminal_output(
        window: &mut gpui_kit::Window,
        cx: &mut gpui_kit::App,
        view: &Entity<MultibotView>,
        needle: &str,
    ) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            // The PTY reader thread cannot wake a headless GPUI scheduler, so pull the
            // emulator snapshot ourselves before each frame.
            view.update(cx, |view, _cx| view.refresh_terminal_screen());
            window.render_frame(cx);
            let value = window
                .try_find("terminal-grid")
                .and_then(|snapshot| snapshot.value().map(str::to_string))
                .unwrap_or_default();
            if value.contains(needle) || std::time::Instant::now() >= deadline {
                return value;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[gpui_kit::test]
    fn terminal_pane_stays_closed_and_spawns_no_process_until_alt_c(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            // Hard Invariant #4: closed pane owns no PTY, no child and no layout node.
            assert_eq!(view.read(cx).terminal.live_process_count(), 0);
            assert!(view.read(cx).terminal.session.is_none());
            assert!(!view.read(cx).terminal.is_open());
            assert!(window.try_find("terminal-pane").is_none());
            assert!(window.try_find("terminal-grid").is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn alt_c_opens_terminal_pane_and_renders_output_in_obsidian_theme(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        let _guard = crate::terminal_pane::test_pty_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            view.update(cx, |v, _cx| {
                v.terminal.command_override = Some(stub_terminal_command());
            });

            window.press("alt-c", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).terminal.live_process_count(), 1);
            assert!(view.read(cx).terminal.is_open());
            assert!(window.try_find("terminal-pane").is_some());
            assert!(window.try_find("terminal-header").is_some());
            assert!(window.try_find("terminal-grid").is_some());
            for control in ["terminal-clear", "terminal-reconnect", "terminal-close"] {
                assert!(window.try_find(control).is_some(), "missing {control}");
            }
            // The grid advertises its text, proving real PTY output reached the emulator.
            let value = wait_for_terminal_output(window, cx, &view, "mbru-6-terminal-ready");
            assert!(
                value.contains("mbru-6-terminal-ready"),
                "terminal grid must render PTY output, got: {value:?}"
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn pressing_alt_c_again_closes_and_cleans_up_the_terminal_pane(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        let _guard = crate::terminal_pane::test_pty_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            view.update(cx, |v, _cx| {
                v.terminal.command_override = Some(stub_terminal_command());
            });
            window.press("alt-c", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).terminal.live_process_count(), 1);

            window.press("alt-c", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).terminal.live_process_count(), 0);
            assert!(view.read(cx).terminal.session.is_none());
            assert!(view.read(cx).terminal.screen.is_none());
            assert!(view.read(cx).terminal.wake_task.is_none());
            assert!(window.try_find("terminal-pane").is_none());
            assert!(window.try_find("terminal-grid").is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn alt_c_closes_the_pane_even_while_the_grid_has_focus(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        let _guard = crate::terminal_pane::test_pty_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            view.update(cx, |v, _cx| {
                v.terminal.command_override = Some(stub_terminal_command());
            });
            window.press("alt-c", cx);
            window.render_frame(cx);
            let focus_handle = view.read(cx).terminal.focus_handle.clone();
            let focus_handle = focus_handle.expect("terminal must own a focus handle");
            window.focus(&focus_handle, cx);
            window.render_frame(cx);
            assert!(focus_handle.is_focused(window));
            window.press("alt-c", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).terminal.live_process_count(), 0);
            assert!(window.try_find("terminal-pane").is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn ctrl_k_palette_offers_toggle_terminal_with_alt_c(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        let _guard = crate::terminal_pane::test_pty_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.press("ctrl-k", cx);
            window.render_frame(cx);

            let catalog = view.read(cx).build_palette_command_catalog(now_unix_ms());
            let entry = catalog
                .iter()
                .find(|c| c.id == "pal-cmd-toggle-terminal")
                .expect("Ctrl+K must offer 'Toggle Terminal'");
            assert_eq!(entry.label, "Toggle Terminal");
            assert_eq!(entry.chord, "Alt+C");

            // Running the palette row toggles exactly like the chord does.
            view.update(cx, |v, _cx| {
                v.terminal.command_override = Some(stub_terminal_command());
            });
            view.update(cx, |v, cx| {
                v.execute_palette_row(
                    PaletteRow::Command {
                        id: "pal-cmd-toggle-terminal".into(),
                        label: "Toggle Terminal".into(),
                        chord: "Alt+C".into(),
                        category: PaletteCategory::SidebarAndTabs,
                        destructive: false,
                        match_rank: 0,
                        frecency: 0,
                    },
                    now_unix_ms(),
                    Some(window),
                    cx,
                );
            });
            window.render_frame(cx);
            assert_eq!(view.read(cx).terminal.live_process_count(), 1);
            assert!(window.try_find("terminal-pane").is_some());

            // And closes it again through the same entry.
            view.update(cx, |v, cx| {
                v.execute_palette_row(
                    PaletteRow::Command {
                        id: "pal-cmd-toggle-terminal".into(),
                        label: "Toggle Terminal".into(),
                        chord: "Alt+C".into(),
                        category: PaletteCategory::SidebarAndTabs,
                        destructive: false,
                        match_rank: 0,
                        frecency: 0,
                    },
                    now_unix_ms(),
                    Some(window),
                    cx,
                );
            });
            window.render_frame(cx);
            assert_eq!(view.read(cx).terminal.live_process_count(), 0);
        })
        .unwrap();
    }

    // =====================================================================
    // Slice 5 — lazy Computer View pane (MBRU-7)
    // =====================================================================

    /// Ids the pane mounts while it is open, and which must be absent when it closes.
    const COMPUTER_VIEW_IDS: [&str; 5] = [
        "computer-pane",
        "computer-header",
        "computer-status",
        "computer-url-input",
        "computer-close",
    ];

    fn assert_computer_view_unmounted(window: &mut gpui_kit::Window) {
        for id in COMPUTER_VIEW_IDS {
            assert!(
                window.try_find(id).is_none(),
                "closed Computer View must not mount {id}"
            );
        }
    }

    #[gpui_kit::test]
    fn computer_view_starts_closed_and_owns_no_backend(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            // Hard Invariant #4: a closed pane owns no backend, no webview and no layout.
            let computer = &view.read(cx).computer;
            assert!(!computer.is_open());
            assert_eq!(computer.backend_handle_count(), 0);
            assert!(computer.backend().is_none());
            assert_computer_view_unmounted(window);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn ctrl_k_offers_toggle_computer_view_and_opens_the_pane(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.press("ctrl-k", cx);
            window.render_frame(cx);

            let catalog = view.read(cx).build_palette_command_catalog(now_unix_ms());
            let entry = catalog
                .iter()
                .find(|c| c.id == "pal-cmd-toggle-computer-view")
                .expect("Ctrl+K must offer 'Toggle Computer View'");
            assert_eq!(entry.label, "Toggle Computer View");
            // No new chord: the palette row is the entry point, by design.
            assert!(entry.chord.is_empty());
            assert!(entry.aliases.iter().any(|alias| alias == "vnc"));

            view.update(cx, |v, cx| {
                v.execute_palette_row(
                    PaletteRow::Command {
                        id: "pal-cmd-toggle-computer-view".into(),
                        label: "Toggle Computer View".into(),
                        chord: String::new(),
                        category: PaletteCategory::LookAndSettings,
                        destructive: false,
                        match_rank: 0,
                        frecency: 0,
                    },
                    now_unix_ms(),
                    Some(window),
                    cx,
                );
            });
            window.render_frame(cx);

            let computer = &view.read(cx).computer;
            assert!(computer.is_open());
            // The pane opens on the connect prompt, so it holds no backend yet.
            assert_eq!(computer.backend_handle_count(), 0);
            assert!(window.try_find("computer-pane").is_some());
            assert!(window.try_find("computer-header").is_some());
            assert!(window.try_find("computer-url-input").is_some());
            assert!(window.try_find("computer-connect-prompt").is_some());
            for control in ["computer-reload", "computer-external", "computer-close"] {
                assert!(window.try_find(control).is_some(), "missing {control}");
            }
            assert_eq!(
                window.find("computer-status").value(),
                Some("Enter a URL to start")
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn closing_computer_view_destroys_the_backend_and_restores_the_chat(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            view.update(cx, |v, cx| {
                v.open_computer_view(window, cx);
            });
            window.render_frame(cx);
            assert!(view.read(cx).computer.is_open());
            // The pane takes real horizontal space, so the centred header pill sits left
            // of where it sits without the pane.
            let pill_x_open = window.find("header-title-pill").bounds().origin.x;

            view.update(cx, |v, cx| {
                v.computer
                    .open()
                    .unwrap()
                    .url_input
                    .update(cx, |state, cx| {
                        state.set_value("http://localhost:6080", window, cx);
                    });
                v.connect_computer_view_with_window(window, cx);
            });
            window.render_frame(cx);
            assert_eq!(view.read(cx).computer.backend_handle_count(), 1);
            assert_eq!(
                view.read(cx)
                    .computer
                    .open()
                    .map(|open| open.status.clone()),
                Some(crate::computer_view::ComputerStatus::Loaded)
            );

            window.click("computer-close", cx);
            window.render_frame(cx);
            assert!(!view.read(cx).computer.is_open());
            assert_eq!(view.read(cx).computer.backend_handle_count(), 0);
            assert_computer_view_unmounted(window);
            // The chat column gets its full width back.
            let pill_x_closed = window.find("header-title-pill").bounds().origin.x;
            assert!(
                pill_x_closed > pill_x_open,
                "closing the pane must widen the chat (pill x {pill_x_closed:?} vs {pill_x_open:?})"
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn computer_view_rejects_a_dangerous_url_without_starting_a_backend(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            view.update(cx, |v, cx| {
                v.open_computer_view(window, cx);
            });
            window.render_frame(cx);
            for bad in [
                "javascript:alert(1)",
                "file:///c:/windows/system32/calc.exe",
                "data:text/html,x",
            ] {
                view.update(cx, |v, cx| {
                    v.computer
                        .open()
                        .unwrap()
                        .url_input
                        .update(cx, |state, cx| {
                            state.set_value(bad, window, cx);
                        });
                    v.connect_computer_view_with_window(window, cx);
                });
                window.render_frame(cx);
                let computer = &view.read(cx).computer;
                assert_eq!(
                    computer.backend_handle_count(),
                    0,
                    "{bad} must not start a backend"
                );
                assert!(matches!(
                    computer.open().map(|open| open.status.clone()),
                    Some(crate::computer_view::ComputerStatus::Failed(_))
                ));
            }
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn reopening_computer_view_offers_the_last_url_without_starting_a_backend(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            view.update(cx, |v, cx| {
                v.open_computer_view(window, cx);
                v.computer
                    .open()
                    .unwrap()
                    .url_input
                    .update(cx, |state, cx| {
                        state.set_value("https://vnc.example.com:6080", window, cx);
                    });
                v.connect_computer_view_with_window(window, cx);
            });
            window.render_frame(cx);
            assert_eq!(view.read(cx).computer.backend_handle_count(), 1);
            view.update(cx, |v, cx| {
                v.close_computer_view(cx);
            });
            window.render_frame(cx);
            assert_eq!(view.read(cx).computer.backend_handle_count(), 0);

            // Reopening offers the same address but starts nothing: remembering an
            // address is not permission to launch a browser.
            view.update(cx, |v, cx| {
                v.open_computer_view(window, cx);
            });
            window.render_frame(cx);
            let computer = &view.read(cx).computer;
            assert_eq!(computer.backend_handle_count(), 0);
            assert_eq!(
                computer
                    .open()
                    .map(|open| open.url_input.read(cx).value().to_string()),
                Some("https://vnc.example.com:6080".to_string())
            );
            assert!(matches!(
                computer.open().map(|open| open.status.clone()),
                Some(crate::computer_view::ComputerStatus::NeedsUrl)
            ));
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn computer_view_closes_when_the_inbox_replaces_the_chat(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            view.update(cx, |v, cx| {
                v.open_computer_view(window, cx);
            });
            window.render_frame(cx);
            assert!(view.read(cx).computer.is_open());

            window.press("alt-i", cx);
            window.render_frame(cx);
            // The Inbox does not render the chat column, so the pane and its backend go.
            assert!(!view.read(cx).computer.is_open());
            assert_eq!(view.read(cx).computer.backend_handle_count(), 0);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn wave1_palette_keeps_removed_actions_discoverable(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.press("ctrl-k", cx);
            window.render_frame(cx);
            let catalog = view.read(cx).build_palette_command_catalog(now_unix_ms());
            for id in [
                "pal-cmd-settings",
                "pal-cmd-import",
                "pal-cmd-hosts",
                "pal-cmd-sidebar",
                "pal-cmd-inbox",
                "pal-cmd-shortcuts",
                "pal-cmd-spawn-claude",
                "pal-cmd-spawn-antigravity",
                "pal-cmd-spawn-codex",
                "pal-cmd-window",
            ] {
                assert!(catalog.iter().any(|c| c.id == id), "Missing command: {id}");
            }
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn renders_dark_shell_and_toggles_gpu_settings(cx: &mut TestAppContext) {
        let handle = open_test_view(cx);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("gpu-status-card", cx);
            window.render_frame(cx);
            window.click("dev-stats-gpu-settings", cx);
            window.render_frame(cx);
            window.click("gpu-mode-software", cx);
            window.render_frame(cx);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn opens_command_palette_and_inbox_and_shortcuts(cx: &mut TestAppContext) {
        let handle = open_test_view(cx);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.press("ctrl-k", cx);
            window.render_frame(cx);
            window.click("pal-cmd-inbox", cx);
            window.render_frame(cx);
            // Minimal Inbox: only non-empty split pills render, so a `0`-session Favorites
            // split is hidden and `All` stays clickable
            assert!(
                window.try_find("inbox-split-favorites").is_none(),
                "0-session Favorites split must be hidden from the Inbox split bar"
            );
            window.click("inbox-tab-all", cx);
            window.render_frame(cx);
            assert!(window.try_find("inbox-tab-all").is_some());
            window.press("f1", cx);
            window.render_frame(cx);
            window.click("close-shortcuts", cx);
            window.render_frame(cx);
            window.press("ctrl-shift-b", cx);
            window.render_frame(cx);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn m6_keyboard_actions_and_palette_m5_commands_work_end_to_end(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            let s1 = make_session("s1", AgentKind::Claude, "Alpha", false);
            let s2 = make_session("s2", AgentKind::Codex, "Beta", false);
            view.update(cx, |v, cx| {
                v.sessions = vec![s1.clone(), s2.clone()];
                v.selected_session = Some(s1.clone());
                v.messages = vec![ChatMessage {
                    id: "m1".into(),
                    role: MessageRole::Assistant,
                    text: "```rust\nfn main() {}\n```\n\n| ColA | ColB |\n| --- | --- |\n| 1 | 2 |"
                        .into(),
                    blocks: vec![MessageBlock::ToolUse {
                        call_id: "c1".into(),
                        name: "Bash".into(),
                        summary: "cargo test".into(),
                        detail: "cargo test".into(),
                        status: ToolStatus::Completed,
                        diff: None,
                        output: Some("ok".into()),
                    }],
                    todos: vec![TodoItem {
                        content: "Ship M6".into(),
                        status: TodoStatus::InProgress,
                    }],
                    ..Default::default()
                }];
                cx.notify();
            });
            window.render_frame(cx);

            // 1. Ctrl+Shift+B cycles 3-state sidebar AND keeps settings.sidebar_mode in sync
            assert_eq!(view.read(cx).sidebar_state.mode, SidebarMode::Open);
            assert_eq!(view.read(cx).settings.sidebar_mode, SidebarMode::Open);
            window.press("ctrl-shift-b", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).sidebar_state.mode, SidebarMode::Collapsed);
            assert_eq!(view.read(cx).settings.sidebar_mode, SidebarMode::Collapsed);
            window.press("ctrl-shift-b", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).sidebar_state.mode, SidebarMode::Hidden);
            assert_eq!(view.read(cx).settings.sidebar_mode, SidebarMode::Hidden);
            window.press("ctrl-shift-b", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).sidebar_state.mode, SidebarMode::Open);
            assert_eq!(view.read(cx).settings.sidebar_mode, SidebarMode::Open);

            // 2. ToggleFavorite (Alt+S), JumpWorkspace1 (Starred), PrevWorkspace/NextWorkspace, WorkspaceView*
            window.press("alt-s", cx);
            window.render_frame(cx);
            let s1_key = view.read(cx).session_key_for(&s1);
            assert!(
                view.read(cx)
                    .sidebar_state
                    .workspace_store
                    .is_favorite(&s1_key)
            );
            window.press("alt-1", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).sidebar_state.active_workspace_id,
                multibot_core::workspaces::STARRED_WORKSPACE_ID
            );
            window.press("alt-right", cx);
            window.press("alt-shift-2", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).sidebar_state.active_workspace_view,
                WorkspaceView::Frequent
            );
            window.press("alt-shift-3", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).sidebar_state.active_workspace_view,
                WorkspaceView::Favorites
            );
            window.press("alt-shift-1", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).sidebar_state.active_workspace_view,
                WorkspaceView::All
            );

            // 3. ToggleUnreadOnly (Ctrl+Shift+U)
            window.press("ctrl-shift-u", cx);
            window.render_frame(cx);
            assert!(view.read(cx).sidebar_state.unread_only);
            window.press("ctrl-shift-u", cx);
            window.render_frame(cx);
            assert!(!view.read(cx).sidebar_state.unread_only);

            // 4. FontZoomIn / FontZoomOut / FontZoomReset clamped to 10..=28pt
            for _ in 0..20 {
                window.press("ctrl-=", cx);
            }
            window.render_frame(cx);
            assert_eq!(view.read(cx).chat_state.font_size_pt, MAX_CHAT_FONT_SIZE_PT);
            assert_eq!(
                view.read(cx).settings.chat_font_size_pt,
                MAX_CHAT_FONT_SIZE_PT
            );
            for _ in 0..30 {
                window.press("ctrl--", cx);
            }
            window.render_frame(cx);
            assert_eq!(view.read(cx).chat_state.font_size_pt, MIN_CHAT_FONT_SIZE_PT);
            assert_eq!(
                view.read(cx).settings.chat_font_size_pt,
                MIN_CHAT_FONT_SIZE_PT
            );
            window.press("ctrl-0", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).chat_state.font_size_pt,
                multibot_core::settings::DEFAULT_CHAT_FONT_SIZE_PT
            );

            // 5. FindInChat (Ctrl+F) & BrowseCommands (Ctrl+/)
            window.press("ctrl-f", cx);
            window.render_frame(cx);
            assert!(view.read(cx).chat_state.find_open);
            window.press("ctrl-/", cx);
            window.render_frame(cx);
            assert!(!view.read(cx).chat_state.find_open);
            assert!(view.read(cx).slash_picker_state.is_open());
            window.press("escape", cx);
            window.render_frame(cx);
            assert!(!view.read(cx).slash_picker_state.is_open());

            // 6. Palette M5 commands: copy code, expand table, toggle tools, toggle todos
            let now = now_unix_ms();
            let mk_cmd = |id: &str, label: &str, chord: &str| PaletteRow::Command {
                id: id.into(),
                label: label.into(),
                chord: chord.into(),
                category: PaletteCategory::ChatView,
                destructive: false,
                match_rank: 0,
                frecency: 0,
            };
            view.update(cx, |v, cx| {
                v.execute_palette_row(
                    mk_cmd("pal-cmd-expand-table", "Expand Table", ""),
                    now,
                    Some(window),
                    cx,
                );
            });
            window.render_frame(cx);
            assert!(view.read(cx).table_modal_state.is_open());
            window.press("escape", cx);
            window.render_frame(cx);
            assert!(!view.read(cx).table_modal_state.is_open());

            view.update(cx, |v, cx| {
                v.execute_palette_row(
                    mk_cmd("pal-cmd-toggle-todos", "Toggle Todo List", ""),
                    now,
                    Some(window),
                    cx,
                );
                v.execute_palette_row(
                    mk_cmd(
                        "pal-cmd-toggle-tools",
                        "Chat: Show / hide tool call details",
                        "",
                    ),
                    now,
                    Some(window),
                    cx,
                );
                v.execute_palette_row(
                    mk_cmd(
                        "pal-cmd-copy-code",
                        "Copy Latest Code Block",
                        "Ctrl+Shift+D",
                    ),
                    now,
                    Some(window),
                    cx,
                );
            });
            assert!(
                view.read(cx)
                    .chat_state
                    .todo_drawer_open
                    .get("s1")
                    .copied()
                    .unwrap_or(false)
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn close_overlay_respects_exact_z_order_and_interrupts_working_agent(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        let (req_tx, mut req_rx) = mpsc::unbounded_channel::<ClientRequest>();

        cx.update_window(handle, |_, window, cx| {
            let working_sess = make_session("w1", AgentKind::Claude, "Working", true);
            view.update(cx, |v, cx| {
                v.daemon_tx = Some(req_tx);
                v.sessions = vec![working_sess.clone()];
                v.selected_session = Some(working_sess);
                v.messages = vec![ChatMessage {
                    id: "m1".into(),
                    role: MessageRole::Assistant,
                    text: "| A | B |\n| --- | --- |\n| 1 | 2 |".into(),
                    ..Default::default()
                }];
                v.main_surface = MainSurface::Inbox;
                v.chat_state.reply_quote = Some(crate::chat_view::ReplyQuote {
                    message_id: "m1".into(),
                    preview: "quote".into(),
                    full_text: "quote".into(),
                });
                v.chat_state.find_open = true;
                v.slash_picker_state.open_browser(AgentKind::Claude, "", 0);
                v.active_modal = ActiveModal::Shortcuts;
                v.expand_best_table(cx);
                cx.notify();
            });
            window.render_frame(cx);

            // 1. Table modal closes first
            window.press("escape", cx);
            window.render_frame(cx);
            assert!(!view.read(cx).table_modal_state.is_open());
            assert_eq!(view.read(cx).active_modal, ActiveModal::Shortcuts);

            // 2. Active modal closes next while Inbox stays mounted
            window.press("escape", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::None);
            assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);
            // Unmounted Chat overlays must NOT have been touched yet
            assert!(view.read(cx).slash_picker_state.is_open());

            // 3. On Inbox, Esc returns to Chat immediately instead of swallowing Esc on unmounted Chat overlays
            window.press("escape", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Chat);
            assert!(view.read(cx).slash_picker_state.is_open());
            assert!(view.read(cx).chat_state.find_open);
            assert!(view.read(cx).chat_state.reply_quote.is_some());

            // 4. Back on Chat, Slash picker closes before Find
            window.press("escape", cx);
            window.render_frame(cx);
            assert!(!view.read(cx).slash_picker_state.is_open());
            assert!(view.read(cx).chat_state.find_open);

            // 5. Find closes before reply quote
            window.press("escape", cx);
            window.render_frame(cx);
            assert!(!view.read(cx).chat_state.find_open);
            assert!(view.read(cx).chat_state.reply_quote.is_some());

            // 6. Reply quote closes before interrupting working agent
            window.press("escape", cx);
            window.render_frame(cx);
            assert!(view.read(cx).chat_state.reply_quote.is_none());
            assert!(req_rx.try_recv().is_err());

            // 7. Esc in Chat on working agent sends InterruptSession
            window.press("escape", cx);
            window.render_frame(cx);
            match req_rx.try_recv().expect("expected InterruptSession") {
                ClientRequest::InterruptSession { session_id, agent } => {
                    assert_eq!(session_id, "w1");
                    assert_eq!(agent, AgentKind::Claude);
                }
                other => panic!("unexpected request: {other:?}"),
            }
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn shared_app_store_syncs_across_ctrl_n_windows_and_decrements_count_on_close(
        cx: &mut TestAppContext,
    ) {
        let (handle1, view1) = open_test_view_with_entity(cx);
        let (handle2, view2) = open_test_view_with_entity(cx);

        assert_eq!(
            cx.update(|cx| cx.global::<SharedAppStore>().window_count),
            2
        );

        let s1 = make_session("sync-1", AgentKind::Claude, "Synced", false);
        cx.update_window(handle1, |_, _window, cx| {
            view1.update(cx, |v, cx| {
                v.sessions = vec![s1.clone()];
                v.selected_session = Some(s1.clone());
                v.sync_sessions_to_shared(cx);
                v.toggle_active_favorite(cx);
                v.toggle_unread_only(cx);
            });
        })
        .unwrap();

        cx.run_until_parked();

        cx.update_window(handle2, |_, _window, cx| {
            let v2 = view2.read(cx);
            assert_eq!(v2.sessions.len(), 1);
            assert_eq!(v2.sessions[0].id, "sync-1");
            assert!(v2.sidebar_state.unread_only);
            let key = v2.session_key_for(&s1);
            assert!(v2.sidebar_state.workspace_store.is_favorite(&key));
        })
        .unwrap();

        drop(view2);
        cx.update_window(handle2, |_, window, _cx| {
            window.remove_window();
        })
        .unwrap();
        cx.run_until_parked();

        assert_eq!(
            cx.update(|cx| cx.global::<SharedAppStore>().window_count),
            1
        );
        let _ = (handle1, view1);
    }

    #[test]
    fn startup_backfill_restores_workspaces_after_session_view_history() {
        let seq = APP_TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("multibot-backfill-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let source = r#"{"workspaces":[{"id":"work","name":"Work"}],"tabs":[{"host":"vps0","csid":"legacy-session-id","workspace":0}]}"#;
        std::fs::write(root.join("session.json"), source).unwrap();
        let mut store = WorkspaceStore::new();
        store.record_session_view("vps0:claude:legacy-session-id", 123);
        assert!(
            store.session_meta["vps0:claude:legacy-session-id"]
                .workspace_id
                .is_none()
        );
        assert!(backfill_zigshel_workspaces(&mut store, &root));
        assert_eq!(
            store.session_meta["vps0:claude:legacy-session-id"]
                .workspace_id
                .as_deref(),
            Some("work")
        );
        assert_eq!(
            store.session_meta["vps0:claude:legacy-session-id"].last_used_ms,
            123
        );
        let before = store.clone();
        assert!(!backfill_zigshel_workspaces(&mut store, &root));
        assert_eq!(store, before);
        assert_eq!(
            std::fs::read_to_string(root.join("session.json")).unwrap(),
            source
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[gpui_kit::test]
    fn dissolved_shell_stays_hidden_after_reload_and_fresh_session_lists(cx: &mut TestAppContext) {
        use multibot_core::workspaces::SessionLifecycle;
        let (handle, view) = open_test_view_with_entity(cx);
        let dir = std::env::temp_dir().join(format!(
            "multibot-shell-reload-{}-{}",
            std::process::id(),
            APP_TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("workspaces.json");
        cx.update_window(handle, |_, window, cx| {
            let dead = make_session("dead-shell", AgentKind::Shell, "Dissolved shell", false);
            let active = make_session("live-shell", AgentKind::Shell, "Active shell", false);
            let key = canonical_session_key("VPS", AgentKind::Shell, &dead.id);
            let active_key = canonical_session_key("VPS", AgentKind::Shell, &active.id);
            let (tx, _rx) = mpsc::unbounded_channel();
            view.update(cx, |v, cx| {
                v.connected_host = Some("VPS".into());
                v.daemon_tx = Some(tx);
                v.sessions = vec![dead.clone(), active.clone()];
                v.sync_sessions_to_shared(cx);
                v.dissolve_session_key(&key, cx);
                v.dissolve_session_key(&key, cx);
                v.sidebar_state.workspace_store.save_to(&path).unwrap();
            });
            for app_scoped in [false, true] {
                view.update(cx, |v, cx| {
                    v.clear_active_session_view();
                    v.sessions.clear();
                    v.sidebar_state.workspace_store = WorkspaceStore::load_from(&path).unwrap();
                    v.sync_workspace_to_shared(cx);
                    v.sync_sessions_to_shared(cx);
                    // A stale, unarchived discovery response must not undo a local dissolve.
                    let event = UiBridgeEvent::Server(ServerEvent::Sessions {
                        sessions: vec![dead.clone(), active.clone()],
                    });
                    if app_scoped {
                        let generation = cx.global::<SharedAppStore>().connection_gen;
                        assert!(MultibotView::apply_daemon_bridge_event_to_app(
                            generation, event, &v.runtime, cx
                        ));
                        v.handle_daemon_server_event_with_hint(
                            "VPS",
                            ServerEvent::Sessions {
                                sessions: vec![dead.clone(), active.clone()],
                            },
                            false,
                            cx,
                        );
                    } else {
                        v.handle_bridge_event(event, cx);
                    }
                    assert_eq!(v.selected_session.as_ref().unwrap().id, active.id);
                    assert_eq!(
                        v.sidebar_state.workspace_store.session_lifecycle(
                            &key,
                            &dead,
                            now_unix_ms()
                        ),
                        SessionLifecycle::Dissolved
                    );
                    assert_eq!(
                        v.sidebar_state.workspace_store.session_lifecycle(
                            &active_key,
                            &active,
                            now_unix_ms()
                        ),
                        SessionLifecycle::Active
                    );
                    assert_eq!(v.active_ctrl_digit_order(), vec![active_key.clone()]);
                });
                window.render_frame(cx);
                assert!(window.try_find("session-0").is_some());
                assert!(window.try_find("session-1").is_none());
            }
        })
        .unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[gpui_kit::test]
    fn session_lists_reconcile_imported_keys_in_app_and_view_paths(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, _window, cx| {
            view.update(cx, |v, cx| {
                let sid = "c0a80101-1234-4567-89ab-0123456789ab";
                let live = format!("vps hetzner:codex:{sid}");
                for app_scoped in [false, true] {
                    let mut store = WorkspaceStore::new();
                    let ws = store.create_workspace("Work", None).unwrap();
                    let imported = format!("vps0:claude:{sid}");
                    store.assign_session_workspace(&imported, &ws.id);
                    store.favorites.insert(imported);
                    store.record_session_view(&live, 123);
                    v.sidebar_state.workspace_store = store;
                    v.connected_host = Some("VPS Hetzner".into());
                    v.sync_workspace_to_shared(cx);
                    v.sync_sessions_to_shared(cx);
                    let event = UiBridgeEvent::Server(ServerEvent::Sessions {
                        sessions: vec![make_session(sid, AgentKind::Codex, "Live", false)],
                    });
                    if app_scoped {
                        let connection_gen = cx.global::<SharedAppStore>().connection_gen;
                        assert!(MultibotView::apply_daemon_bridge_event_to_app(
                            connection_gen,
                            event,
                            &v.runtime,
                            cx
                        ));
                        v.sync_from_shared_store(cx);
                    } else {
                        v.handle_bridge_event(event, cx);
                    }
                    assert_eq!(
                        v.sidebar_state.workspace_store.session_meta[&live]
                            .workspace_id
                            .as_deref(),
                        Some(ws.id.as_str())
                    );
                    assert!(v.sidebar_state.workspace_store.is_favorite(&live));
                    let shared = &cx.global::<SharedAppStore>().workspace_store;
                    assert_eq!(
                        shared.session_meta[&live].workspace_id.as_deref(),
                        Some(ws.id.as_str())
                    );
                    assert!(shared.is_favorite(&live));
                }
            });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn server_workspace_hint_lands_in_app_and_view_paths_and_keeps_manual_moves(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, _window, cx| {
            view.update(cx, |v, cx| {
                let sid = "e2f2f8d3-b2de-4c17-9727-d3beef065f6d";
                let mut summary = make_session(sid, AgentKind::Claude, "HT Product Lead", false);
                summary.workspace = Some("HT Product".into());
                v.connected_host = Some("VPS Hetzner".into());
                for app_scoped in [false, true] {
                    // Fresh store per path so the second iteration starts unassigned.
                    v.sidebar_state.workspace_store = WorkspaceStore::new();
                    v.sync_workspace_to_shared(cx);
                    let event = UiBridgeEvent::Server(ServerEvent::Sessions {
                        sessions: vec![summary.clone()],
                    });
                    if app_scoped {
                        let connection_gen = cx.global::<SharedAppStore>().connection_gen;
                        assert!(MultibotView::apply_daemon_bridge_event_to_app(
                            connection_gen,
                            event,
                            &v.runtime,
                            cx
                        ));
                        v.sync_from_shared_store(cx);
                    } else {
                        v.handle_bridge_event(event, cx);
                    }

                    let key = canonical_session_key("VPS Hetzner", AgentKind::Claude, sid);
                    let place = |store: &WorkspaceStore| {
                        let ws_id = store.session_meta[&key].workspace_id.clone().unwrap();
                        store.workspace_by_id(&ws_id).unwrap().name.clone()
                    };
                    assert_eq!(place(&v.sidebar_state.workspace_store), "HT Product");
                    assert_eq!(
                        place(&cx.global::<SharedAppStore>().workspace_store),
                        "HT Product"
                    );

                    // The user moves the session somewhere else; the next refresh must
                    // not drag it back to the server's workspace.
                    let mine = v
                        .sidebar_state
                        .workspace_store
                        .create_workspace("Mine", None)
                        .unwrap();
                    v.sidebar_state
                        .workspace_store
                        .assign_session_workspace(&key, &mine.id);
                    v.sync_workspace_to_shared(cx);
                    let event = UiBridgeEvent::Server(ServerEvent::Sessions {
                        sessions: vec![summary.clone()],
                    });
                    if app_scoped {
                        let connection_gen = cx.global::<SharedAppStore>().connection_gen;
                        assert!(MultibotView::apply_daemon_bridge_event_to_app(
                            connection_gen,
                            event,
                            &v.runtime,
                            cx
                        ));
                        v.sync_from_shared_store(cx);
                    } else {
                        v.handle_bridge_event(event, cx);
                    }
                    assert_eq!(place(&v.sidebar_state.workspace_store), "Mine");
                }
            });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn manual_import_reconciles_already_loaded_sessions(cx: &mut TestAppContext) {
        let seq = APP_TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("multibot-live-import-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let source = r#"{"workspaces":[{"id":"work","name":"Work"}],"tabs":[{"host":"remote-1","csid":"c0a80101-1234-4567-89ab-0123456789ab","workspace":0,"favorite":true,"pinned_top":true}]}"#;
        std::fs::write(root.join("session.json"), source).unwrap();
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, _window, cx| {
            view.update(cx, |v, cx| {
                let sid = "c0a80101-1234-4567-89ab-0123456789ab";
                let live = format!("vps hetzner:codex:{sid}");
                v.connected_host = Some("VPS Hetzner".into());
                v.sessions = vec![make_session(sid, AgentKind::Codex, "Live", false)];
                v.sidebar_state
                    .workspace_store
                    .record_session_view(&live, 123);
                v.import_from_zigshel_dir(Some(&root), cx);
                let store = &v.sidebar_state.workspace_store;
                assert_eq!(
                    store.session_meta[&live].workspace_id.as_deref(),
                    Some("work")
                );
                assert!(store.session_meta[&live].pinned);
                assert!(store.is_favorite(&live));
                assert_eq!(cx.global::<SharedAppStore>().workspace_store, *store);
            });
        })
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("session.json")).unwrap(),
            source
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[gpui_kit::test]
    fn settings_shows_build_version_and_imports_zigshel_bundle_readonly(cx: &mut TestAppContext) {
        let seq = APP_TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "multibot-desktop-zigshel-test-{}-{seq}",
            std::process::id()
        ));
        let ssh_dir = root.join("ssh-key");
        std::fs::create_dir_all(&ssh_dir).unwrap();
        std::fs::write(
            ssh_dir.join("config"),
            "Host vps1\n  HostName 10.9.0.1\n  User ubuntu\n  IdentityFile id_ed25519\n",
        )
        .unwrap();
        std::fs::write(
            root.join("session.json"),
            r#"{"workspaces":[{"id":"work","name":"Work"}],"tabs":[{"host":"vps1","csid":"s1","favorite":true},{"host":"vps1","csid":"s2","name":"Ordinary session"}]}"#,
        )
        .unwrap();

        let before_hosts = std::fs::read(ssh_dir.join("config")).unwrap();
        let before_workspaces = std::fs::read(root.join("session.json")).unwrap();
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.press("ctrl-,", cx);
            window.render_frame(cx);
            window.click("settings-tab-updates", cx);
            window.render_frame(cx);
            assert!(window.try_find("settings-version-label").is_some());
            assert!(window.try_find("settings-check-updates-btn").is_some());

            view.update(cx, |v, cx| {
                v.import_from_zigshel_dir(Some(&root), cx);
            });
            let v = view.read(cx);
            assert_eq!(v.settings.ssh_hosts.len(), 1);
            assert_eq!(
                v.settings.ssh_hosts[0].identity_file.as_deref(),
                Some(ssh_dir.join("id_ed25519").to_string_lossy().as_ref())
            );
            assert!(
                v.sidebar_state
                    .workspace_store
                    .workspaces
                    .iter()
                    .any(|w| w.name == "Work")
            );
            assert!(
                v.sidebar_state
                    .workspace_store
                    .is_favorite("vps1:claude:s1")
            );
            assert_eq!(
                v.sidebar_state.workspace_store.session_meta["vps1:claude:s2"]
                    .workspace_id.as_deref(),
                Some("work")
            );
            assert_eq!(
                v.notice.as_deref(),
                Some("Imported 1 host(s), 1 workspace(s), 2 session assignment(s), and 1 favorite(s) from ZigShell (read-only).")
            );
        })
        .unwrap();

        assert_eq!(std::fs::read(ssh_dir.join("config")).unwrap(), before_hosts);
        assert_eq!(
            std::fs::read(root.join("session.json")).unwrap(),
            before_workspaces
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[gpui_kit::test]
    fn update_poller_304_triggers_zero_state_changes_and_persists_etag_and_preserves_gpu_args(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        let seq = APP_TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp_settings = std::env::temp_dir().join(format!(
            "multibot-update-etag-{}-{seq}.json",
            std::process::id()
        ));

        cx.update_window(handle, |_, _window, cx| {
            view.update(cx, |v, cx| {
                v.update_ui.settings_path_override = Some(tmp_settings.clone());
                v.handle_update_bridge_event(
                    UpdateBridgeEvent::CheckCompleted {
                        outcome: CheckOutcome::UpToDate {
                            etag: Some("\"etag-1\"".into()),
                        },
                        manual: false,
                    },
                    cx,
                );
                assert_eq!(v.settings.last_update_etag.as_deref(), Some("\"etag-1\""));
                assert_eq!(v.update_ui.state, UpdateState::UpToDate);
                assert!(v.notice.is_none());

                // 304 Not Modified in background must NOT alter state or notice.
                v.handle_update_bridge_event(
                    UpdateBridgeEvent::CheckCompleted {
                        outcome: CheckOutcome::NotModified,
                        manual: false,
                    },
                    cx,
                );
                assert_eq!(v.update_ui.state, UpdateState::UpToDate);
                assert!(v.notice.is_none());

                // Background rate-limit and offline check failures stay silent.
                v.handle_update_bridge_event(
                    UpdateBridgeEvent::CheckCompleted {
                        outcome: CheckOutcome::RateLimited {
                            retry_after: Duration::from_secs(900),
                            etag: None,
                        },
                        manual: false,
                    },
                    cx,
                );
                v.handle_update_bridge_event(
                    UpdateBridgeEvent::CheckFailed {
                        message: "offline".into(),
                        manual: false,
                    },
                    cx,
                );
                assert_eq!(v.update_ui.state, UpdateState::UpToDate);
                assert!(v.notice.is_none());

                // Manual rate-limit shows duration-free copy.
                v.handle_update_bridge_event(
                    UpdateBridgeEvent::CheckCompleted {
                        outcome: CheckOutcome::RateLimited {
                            retry_after: Duration::from_secs(900),
                            etag: None,
                        },
                        manual: true,
                    },
                    cx,
                );
                let note = v.notice.as_deref().unwrap_or("");
                assert!(!note.is_empty());
                assert!(!contains_forbidden_duration_wording(note));

                // Available update copy & DesktopUpdaterPlatform preserves `--gpu=...` args
                let rel = ReleaseInfo {
                    version: "9.9.9".into(),
                    tag: "v9.9.9".into(),
                    published_at: "2026-11-26T00:00:00Z".into(),
                    notes: vec!["Faster chat switching".into()],
                    asset: WindowsAssetInfo {
                        asset_name: "multibot-windows-x64.zip".into(),
                        url: "https://github.com/valentinyeo/multibot/releases/download/v9.9.9/multibot-windows-x64.zip".into(),
                        sha256: "a".repeat(64),
                        size_bytes: 1024,
                        multibot_exe_sha256: "b".repeat(64),
                        multibotd_linux_x86_64_sha256: "c".repeat(64),
                    },
                };
                v.handle_update_bridge_event(
                    UpdateBridgeEvent::CheckCompleted {
                        outcome: CheckOutcome::UpdateAvailable {
                            release: rel,
                            etag: Some("\"etag-2\"".into()),
                        },
                        manual: true,
                    },
                    cx,
                );
                assert!(matches!(v.update_ui.state, UpdateState::Available(_)));
                assert!(!contains_forbidden_duration_wording(
                    v.notice.as_deref().unwrap_or("")
                ));
            });
        })
        .unwrap();

        let plat = DesktopUpdaterPlatform {
            updates_dir: PathBuf::from("/tmp/updates"),
            preserved_args: vec![
                "--gpu=software".into(),
                "--update-ready".into(),
                "--version".into(),
            ],
            swapper: DefaultFileSwapper,
            launcher: DefaultProcessLauncher,
        };
        assert_eq!(
            plat.preserved_cli_args(),
            vec!["--gpu=software".to_string()]
        );
        let sanitized =
            sanitize_update_error_copy(&anyhow::anyhow!("request timed out after 30 seconds"));
        assert!(!contains_forbidden_duration_wording(&sanitized));

        let _ = std::fs::remove_file(&tmp_settings);
    }

    #[gpui_kit::test]
    fn select_session_uses_chat_cache_frame0_and_discards_late_stale_history(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, _window, cx| {
            let sa = make_session("sa", AgentKind::Claude, "Session A", false);
            let sb = make_session("sb", AgentKind::Claude, "Session B", false);

            view.update(cx, |v, cx| {
                v.sessions = vec![sa.clone(), sb.clone()];
                let key_a = CacheKey::new("local", AgentKind::Claude, "sa");
                let key_b = CacheKey::new("local", AgentKind::Claude, "sb");
                v.chat_cache.put(
                    key_a,
                    CachedSession {
                        source_path: sa.source_path.clone(),
                        messages: vec![ChatMessage {
                            id: "a1".into(),
                            role: MessageRole::Assistant,
                            text: "Cached A".into(),
                            ..Default::default()
                        }],
                        ..Default::default()
                    },
                );
                v.chat_cache.put(
                    key_b,
                    CachedSession {
                        source_path: sb.source_path.clone(),
                        messages: vec![ChatMessage {
                            id: "b1".into(),
                            role: MessageRole::Assistant,
                            text: "Cached B".into(),
                            ..Default::default()
                        }],
                        ..Default::default()
                    },
                );

                // Switch A -> B -> A synchronously from memory cache
                v.select_session(sa.clone(), cx);
                assert_eq!(v.messages.len(), 1);
                assert_eq!(v.messages[0].text, "Cached A");

                v.select_session(sb.clone(), cx);
                assert_eq!(v.messages[0].text, "Cached B");

                v.select_session(sa.clone(), cx);
                assert_eq!(v.messages[0].text, "Cached A");

                // Late ServerEvent::History for B updates B's cache only and NEVER corrupts active A
                v.handle_bridge_event(
                    UiBridgeEvent::Server(ServerEvent::History {
                        session_id: "sb".into(),
                        messages: vec![ChatMessage {
                            id: "b2".into(),
                            role: MessageRole::Assistant,
                            text: "Fresh B".into(),
                            ..Default::default()
                        }],
                        oldest_offset: Some(0),
                        latest_offset: Some(128),
                        has_more: false,
                        file_size: Some(128),
                        file_mtime_ms: Some(1000),
                        probe_sha256: Some("abc".into()),
                        todos: Vec::new(),
                    }),
                    cx,
                );
                assert_eq!(v.messages.len(), 1);
                assert_eq!(v.messages[0].text, "Cached A");
                let cached_b = v
                    .chat_cache
                    .get(&CacheKey::new("local", AgentKind::Claude, "sb"))
                    .unwrap();
                assert_eq!(cached_b.messages[0].text, "Fresh B");
            });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn offline_send_queues_in_outbox_and_flushes_fifo_on_hello_ack(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        let (req_tx, mut req_rx) = mpsc::unbounded_channel::<ClientRequest>();

        cx.update_window(handle, |_, _window, cx| {
            let sess = make_session("off1", AgentKind::Claude, "Offline Session", false);
            view.update(cx, |v, cx| {
                v.sessions = vec![sess.clone()];
                v.selected_session = Some(sess.clone());
                v.daemon_tx = None;

                v.send_or_queue_prompt(&sess, "local-0".into(), "First message".into());
                v.send_or_queue_prompt(&sess, "local-1".into(), "Second message".into());
                assert_eq!(v.chat_outbox.entries.len(), 2);

                // Reconnect + HelloAck flushes FIFO in exact order
                v.handle_bridge_event(
                    UiBridgeEvent::Connected {
                        host_label: "local".into(),
                        req_tx,
                    },
                    cx,
                );
                v.handle_bridge_event(
                    UiBridgeEvent::Server(ServerEvent::HelloAck {
                        protocol_version: 1,
                        hostname: "devbox".into(),
                        daemon_version: multibot_proto::build_version().into(),
                        binary_hash: "0123456789ab".into(),
                        pid: 1234,
                    }),
                    cx,
                );
                assert_eq!(v.chat_outbox.entries.len(), 1);
                for working in [true, false] {
                    let mut session = sess.clone();
                    session.working = working;
                    v.handle_bridge_event(UiBridgeEvent::Server(ServerEvent::SessionUpdated { session }), cx);
                }
                assert!(v.chat_outbox.entries.is_empty());
            });
        })
        .unwrap();

        let mut sent_texts = Vec::new();
        while let Ok(req) = req_rx.try_recv() {
            if let ClientRequest::SendPrompt { text, .. } = req {
                sent_texts.push(text);
            }
        }
        assert_eq!(sent_texts, vec!["First message", "Second message"]);
    }

    #[gpui_kit::test]
    fn new_antigravity_empty_history_can_send_and_render_first_reply(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let session = make_session("new-agy", AgentKind::Antigravity, "New AGY", false);
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.daemon_tx = Some(tx);
                assert!(v.spawn_new_session(
                    AgentKind::Antigravity,
                    "/repo".into(),
                    None,
                    None,
                    cx
                ));
                assert!(matches!(
                    rx.try_recv(),
                    Ok(ClientRequest::SpawnSession {
                        agent: AgentKind::Antigravity,
                        initial_prompt: None,
                        ..
                    })
                ));
                v.handle_bridge_event(
                    UiBridgeEvent::Server(ServerEvent::SessionSpawned {
                        session: session.clone(),
                    }),
                    cx,
                );
                v.handle_bridge_event(
                    UiBridgeEvent::Server(ServerEvent::History {
                        session_id: session.id.clone(),
                        messages: Vec::new(),
                        oldest_offset: Some(0),
                        latest_offset: Some(0),
                        has_more: false,
                        file_size: Some(0),
                        file_mtime_ms: None,
                        probe_sha256: None,
                        todos: Vec::new(),
                    }),
                    cx,
                );
                assert_eq!(v.selected_session.as_ref().unwrap().id, session.id);
                assert!(v.messages.is_empty());
                v.prompt_input.update(cx, |input, cx| {
                    input.set_value("Hello new AGY", window, cx);
                    input.focus(window, cx);
                });
            });
            window.render_frame(cx);
            window.press("ctrl-enter", cx);
            window.render_frame(cx);
            let mut sent = false;
            while let Ok(request) = rx.try_recv() {
                if let ClientRequest::SendPrompt {
                    agent,
                    session_id,
                    text,
                    ..
                } = request
                {
                    assert_eq!(agent, AgentKind::Antigravity);
                    assert_eq!(session_id, session.id);
                    assert_eq!(text, "Hello new AGY");
                    sent = true;
                }
            }
            assert!(sent, "first prompt must reach the new AGY session");
            view.update(cx, |v, cx| {
                v.handle_bridge_event(
                    UiBridgeEvent::Server(ServerEvent::HistoryAppend {
                        session_id: session.id.clone(),
                        messages: vec![ChatMessage {
                            id: "first-reply".into(),
                            role: MessageRole::Assistant,
                            text: "First AGY reply".into(),
                            ..Default::default()
                        }],
                        latest_offset: Some(300),
                        working: Some(false),
                        probe_sha256: None,
                        todos: Vec::new(),
                    }),
                    cx,
                );
                assert!(v.messages.iter().any(|m| m.text == "First AGY reply"));
                assert!(
                    !v.notice
                        .as_deref()
                        .unwrap_or("")
                        .contains("read_history_failed")
                );
            });
            window.render_frame(cx);
            let reply_index = view
                .read(cx)
                .messages
                .iter()
                .position(|m| m.id == "first-reply")
                .unwrap();
            assert!(window.find(format!("msg-bubble-{reply_index}")).visible());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn server_events_update_sessions_commands_search_and_interrupt(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, _window, cx| {
            let s1 = make_session("ev1", AgentKind::Claude, "Event Session", true);
            view.update(cx, |v, cx| {
                v.handle_bridge_event(
                    UiBridgeEvent::Server(ServerEvent::SessionSpawned {
                        session: s1.clone(),
                    }),
                    cx,
                );
                assert_eq!(v.sessions.len(), 1);
                assert_eq!(v.selected_session.as_ref().unwrap().id, "ev1");

                v.handle_bridge_event(
                    UiBridgeEvent::Server(ServerEvent::SessionInterrupted {
                        session_id: "ev1".into(),
                        ok: true,
                        detail: String::new(),
                    }),
                    cx,
                );
                assert!(!v.selected_session.as_ref().unwrap().working);

                v.handle_bridge_event(
                    UiBridgeEvent::Server(ServerEvent::Commands {
                        agent: AgentKind::Claude,
                        cwd: Some("/srv/app".into()),
                        items: vec![SlashCommandItem {
                            name: "deploy".into(),
                            description: "Deploy app".into(),
                            trigger: "/".into(),
                            source: "project".into(),
                            opens_in_terminal: false,
                        }],
                    }),
                    cx,
                );
                assert!(
                    v.slash_picker_state
                        .effective_catalog(AgentKind::Claude, TriggerMode::Slash)
                        .iter()
                        .any(|c| c.name == "deploy")
                );

                v.remote_search_request_id = Some("search-1".into());
                v.handle_bridge_event(
                    UiBridgeEvent::Server(ServerEvent::SearchResults {
                        request_id: "search-1".into(),
                        session_id: "ev1".into(),
                        hits: vec![SearchHit {
                            message_id: "m1".into(),
                            byte_offset: 42,
                            snippet: "needle".into(),
                            role: MessageRole::Assistant,
                        }],
                        truncated: false,
                    }),
                    cx,
                );
                assert_eq!(v.remote_search_hits.len(), 1);

                v.last_dispatched_prompt = Some(("local-9".into(), "retry me".into()));
                v.handle_bridge_event(
                    UiBridgeEvent::Server(ServerEvent::PromptStatus {
                        session_id: "ev1".into(),
                        message_id: "local-9".into(),
                        status: "failed".into(),
                        detail: "pipe closed".into(),
                    }),
                    cx,
                );
                assert!(v.chat_state.failed_sends.contains_key("local-9"));
            });
        })
        .unwrap();
    }

    #[test]
    fn parse_outbox_session_key_handles_pipe_and_colon_delimiters() {
        let key = ChatOutbox::session_key("host:22|a", AgentKind::Codex, "sid|1:2");
        let parsed = parse_outbox_session_key(&key).expect("must parse lossless outbox key");
        assert_eq!(parsed, ("host:22|a", AgentKind::Codex, "sid|1:2"));
    }

    #[gpui_kit::test]
    fn load_older_history_sends_none_probe_and_clears_pending_on_select_or_tail(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        let (req_tx, mut req_rx) = mpsc::unbounded_channel::<ClientRequest>();

        cx.update_window(handle, |_, _window, cx| {
            let s1 = make_session("p1", AgentKind::Claude, "Paged", false);
            view.update(cx, |v, cx| {
                v.daemon_tx = Some(req_tx);
                v.sessions = vec![s1.clone()];
                let key = CacheKey::new("local", AgentKind::Claude, "p1");
                v.chat_cache.put(
                    key,
                    CachedSession {
                        source_path: s1.source_path.clone(),
                        messages: vec![ChatMessage {
                            id: "m-new".into(),
                            role: MessageRole::Assistant,
                            text: "Newest".into(),
                            ..Default::default()
                        }],
                        oldest_offset: 1000,
                        consumed_offset: 2000,
                        has_more: true,
                        probe_sha256: Some("eof-probe-at-2000".into()),
                        ..Default::default()
                    },
                );
                v.select_session(s1.clone(), cx);
                // Drain initial select_session requests
                while req_rx.try_recv().is_ok() {}

                assert!(v.load_older_history(cx));
                match req_rx.try_recv().expect("expected ReadHistory") {
                    ClientRequest::ReadHistory {
                        before_offset,
                        known_probe,
                        ..
                    } => {
                        assert_eq!(before_offset, Some(1000));
                        assert_eq!(
                            known_probe, None,
                            "load_older_history must NOT pass EOF probe_sha256 as known_probe"
                        );
                    }
                    other => panic!("unexpected request: {other:?}"),
                }

                // Older page response prepends cleanly and clears pending_older_page_before
                v.handle_bridge_event(
                    UiBridgeEvent::Server(ServerEvent::History {
                        session_id: "p1".into(),
                        messages: vec![ChatMessage {
                            id: "m-old".into(),
                            role: MessageRole::User,
                            text: "Older".into(),
                            ..Default::default()
                        }],
                        oldest_offset: Some(0),
                        latest_offset: Some(1000),
                        has_more: false,
                        file_size: Some(2000),
                        file_mtime_ms: Some(1000),
                        probe_sha256: None,
                        todos: Vec::new(),
                    }),
                    cx,
                );
                assert_eq!(
                    v.messages.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
                    vec!["m-old", "m-new"]
                );
                assert!(!v.pending_older_page_before.contains_key("p1"));

                // Stale pending_older_page_before is cleared by non-older tail History
                v.pending_older_page_before.insert("p1".into(), 1000);
                v.handle_bridge_event(
                    UiBridgeEvent::Server(ServerEvent::History {
                        session_id: "p1".into(),
                        messages: vec![ChatMessage {
                            id: "m-tail".into(),
                            role: MessageRole::Assistant,
                            text: "Fresh tail".into(),
                            ..Default::default()
                        }],
                        oldest_offset: Some(1500),
                        latest_offset: Some(2500),
                        has_more: true,
                        file_size: Some(2500),
                        file_mtime_ms: Some(2000),
                        probe_sha256: Some("probe-2500".into()),
                        todos: Vec::new(),
                    }),
                    cx,
                );
                assert!(!v.pending_older_page_before.contains_key("p1"));
                assert_eq!(
                    v.messages.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
                    vec!["m-tail"]
                );

                // Stale pending_older_page_before is also cleared on select_session
                v.pending_older_page_before.insert("p1".into(), 1500);
                v.select_session(s1, cx);
                assert!(!v.pending_older_page_before.contains_key("p1"));
            });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn hello_ack_remote_upgrade_is_strictly_one_shot_and_guards_connection_gen(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, _window, cx| {
            view.update(cx, |v, cx| {
                let target = DaemonTarget::RemoteSsh {
                    host: multibot_core::SshHostConfig {
                        name: "vps".into(),
                        hostname: "10.0.0.2".into(),
                        user: "ubuntu".into(),
                        port: 22,
                        identity_file: None,
                    },
                    bundled_linux_binary: Some(PathBuf::from(
                        "/nonexistent/multibotd-linux-x86_64",
                    )),
                };

                // Simulate post-upgrade reconnect preserving daemon_upgrade_attempted = true
                let (old_tx, mut old_rx) = oneshot::channel::<()>();
                let gen_before = {
                    let store = cx.global_mut::<SharedAppStore>();
                    store.daemon_upgrade_attempted = true;
                    store.connection_shutdown_tx = Some(old_tx);
                    store.connection_gen
                };

                v.connect_to_target_internal("vps".into(), target, false, cx);
                {
                    let store = cx.global::<SharedAppStore>();
                    assert_eq!(store.connection_gen, gen_before + 1);
                    assert!(
                        store.daemon_upgrade_attempted,
                        "post-upgrade reconnect must preserve daemon_upgrade_attempted = true"
                    );
                }
                assert!(
                    matches!(old_rx.try_recv(), Err(oneshot::error::TryRecvError::Closed)),
                    "reconnecting must drop previous shutdown_tx to terminate superseded task"
                );

                // Second mismatched HelloAck must NOT re-trigger upgrade (shows drift notice only)
                v.handle_bridge_event(
                    UiBridgeEvent::Server(ServerEvent::HelloAck {
                        protocol_version: 1,
                        hostname: "vps".into(),
                        daemon_version: "0.0.1".into(),
                        binary_hash: "deadbeef1234".into(),
                        pid: 99,
                    }),
                    cx,
                );
                let notice = v.notice.as_deref().unwrap_or("");
                assert!(
                    notice.contains("differs from desktop"),
                    "expected drift notice without infinite upgrade loop, got: {notice:?}"
                );
            });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn esc_modal_precedence_table_modal_over_palette_and_find_over_sidebar_inbox_rename(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            let s1 = make_session("esc1", AgentKind::Claude, "Alpha", false);
            view.update(cx, |v, cx| {
                v.sessions = vec![s1.clone()];
                v.selected_session = Some(s1.clone());
                v.messages = vec![ChatMessage {
                    id: "m1".into(),
                    role: MessageRole::Assistant,
                    text: "| A | B |\n| --- | --- |\n| 1 | 2 |".into(),
                    ..Default::default()
                }];
                // Stack Palette AND topmost table_modal simultaneously, with focus in palette_input
                v.open_palette(window, cx);
                v.expand_best_table(cx);
                v.palette_input.update(cx, |input, cx| {
                    input.focus(window, cx);
                });
                cx.notify();
            });
            window.render_frame(cx);
            assert!(view.read(cx).table_modal_state.is_open());
            assert_eq!(view.read(cx).active_modal, ActiveModal::Palette);

            // First Esc must close table_modal FIRST while keeping Palette open
            window.press("escape", cx);
            window.render_frame(cx);
            assert!(
                !view.read(cx).table_modal_state.is_open(),
                "Esc must close topmost table_modal first"
            );
            assert_eq!(
                view.read(cx).active_modal,
                ActiveModal::Palette,
                "Palette must remain open after closing topmost table_modal"
            );

            // Second Esc closes Palette
            window.press("escape", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::None);

            // Now open Chat Find (#4) AND sidebar rename (#8) with focus in sidebar-root
            let k1 = view.read(cx).session_key_for(&s1);
            view.update(cx, |v, cx| {
                v.chat_state.find_open = true;
                v.start_sidebar_rename_with_window(k1.clone(), window, cx);
                cx.notify();
            });
            window.render_frame(cx);
            assert!(view.read(cx).chat_state.find_open);
            assert!(view.read(cx).sidebar_state.rename.is_some());

            // First Esc must close higher-priority Chat Find (#4), NOT sidebar rename (#8)
            window.press("escape", cx);
            window.render_frame(cx);
            assert!(
                !view.read(cx).chat_state.find_open,
                "Esc must close Chat Find before sidebar rename"
            );
            assert!(
                view.read(cx).sidebar_state.rename.is_some(),
                "Sidebar rename must remain open until higher-priority Find closes"
            );

            // Second Esc cancels sidebar rename (#8)
            window.press("escape", cx);
            window.render_frame(cx);
            assert!(view.read(cx).sidebar_state.rename.is_none());

            // Also verify on Inbox that mounted Open-sidebar rename (#8) yields precedence over
            // Inbox rename (#10), while unmounted Chat reply_quote does NOT swallow Esc on Inbox.
            view.update(cx, |v, cx| {
                v.main_surface = MainSurface::Inbox;
                v.chat_state.reply_quote = Some(crate::chat_view::ReplyQuote {
                    message_id: "m1".into(),
                    preview: "q".into(),
                    full_text: "q".into(),
                });
                v.sidebar_state.rename = Some(InlineRenameState::new(
                    k1.clone(),
                    "esc1".into(),
                    "Alpha".into(),
                ));
                v.inbox_state.rename =
                    Some(InlineRenameState::new(k1, "esc1".into(), "Alpha".into()));
                v.focus_inbox(window, cx);
                cx.notify();
            });
            window.render_frame(cx);

            // Minimal Inbox: a stale sidebar rename can never mount there, so the first Esc cancels
            // the Inbox rename (#10) and leaves the unmounted Chat reply_quote alone.
            window.press("escape", cx);
            window.render_frame(cx);
            assert!(view.read(cx).inbox_state.rename.is_none());
            assert!(
                !view.read(cx).is_sidebar_rename_mounted(),
                "A sidebar rename must never be mounted while the minimal Inbox hides the sidebar"
            );
            assert!(view.read(cx).chat_state.reply_quote.is_some());
            assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn app_scoped_updater_survives_window1_close_and_etag_only_triggers_zero_sibling_redraws(
        cx: &mut TestAppContext,
    ) {
        let (handle1, view1) = open_test_view_with_entity(cx);
        let (handle2, view2) = open_test_view_with_entity(cx);

        let notify_count = Arc::new(AtomicU64::new(0));
        let notify_count_c = notify_count.clone();
        let _obs = cx.update(|cx| {
            cx.observe(&view2, move |_, _| {
                notify_count_c.fetch_add(1, Ordering::SeqCst);
            })
        });

        // Background 304, UpToDate (new etag), RateLimited (new etag), and CheckFailed
        // must produce ZERO cx.notify() redraws on Window 2!
        cx.update(|cx| {
            MultibotView::apply_update_bridge_event_to_app(
                UpdateBridgeEvent::CheckCompleted {
                    outcome: CheckOutcome::NotModified,
                    manual: false,
                },
                cx,
            );
            MultibotView::apply_update_bridge_event_to_app(
                UpdateBridgeEvent::CheckCompleted {
                    outcome: CheckOutcome::UpToDate {
                        etag: Some("\"etag-bg-1\"".into()),
                    },
                    manual: false,
                },
                cx,
            );
            MultibotView::apply_update_bridge_event_to_app(
                UpdateBridgeEvent::CheckCompleted {
                    outcome: CheckOutcome::RateLimited {
                        retry_after: Duration::from_secs(300),
                        etag: Some("\"etag-bg-2\"".into()),
                    },
                    manual: false,
                },
                cx,
            );
            MultibotView::apply_update_bridge_event_to_app(
                UpdateBridgeEvent::CheckFailed {
                    message: "offline".into(),
                    manual: false,
                },
                cx,
            );
        });
        cx.run_until_parked();

        assert_eq!(
            notify_count.load(Ordering::SeqCst),
            0,
            "silent background 304/etag/rate-limit/error updates must trigger ZERO sibling redraws"
        );
        cx.update_window(handle2, |_, _window, cx| {
            assert_eq!(
                view2.read(cx).settings.last_update_etag.as_deref(),
                Some("\"etag-bg-2\""),
                "etag must still sync in memory without redraw"
            );
        })
        .unwrap();

        // Close Window 1 while Window 2 remains open
        drop(view1);
        cx.update_window(handle1, |_, window, _cx| {
            window.remove_window();
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(
            cx.update(|cx| cx.global::<SharedAppStore>().window_count),
            1
        );

        // App-scoped updater still delivers updates to surviving Window 2!
        let rel = ReleaseInfo {
            version: "9.9.9".into(),
            tag: "v9.9.9".into(),
            published_at: "2026-11-26T00:00:00Z".into(),
            notes: vec!["Survives closing Window 1".into()],
            asset: WindowsAssetInfo {
                asset_name: "multibot-windows-x64.zip".into(),
                url: "https://github.com/valentinyeo/multibot/releases/download/v9.9.9/multibot-windows-x64.zip".into(),
                sha256: "a".repeat(64),
                size_bytes: 2048,
                multibot_exe_sha256: "b".repeat(64),
                multibotd_linux_x86_64_sha256: "c".repeat(64),
            },
        };
        cx.update(|cx| {
            MultibotView::apply_update_bridge_event_to_app(
                UpdateBridgeEvent::CheckCompleted {
                    outcome: CheckOutcome::UpdateAvailable {
                        release: rel,
                        etag: Some("\"etag-bg-3\"".into()),
                    },
                    manual: true,
                },
                cx,
            );
        });
        cx.run_until_parked();

        cx.update_window(handle2, |_, _window, cx| {
            let v2 = view2.read(cx);
            assert!(matches!(v2.update_ui.state, UpdateState::Available(_)));
            assert_eq!(v2.notice.as_deref(), Some("Multibot v9.9.9 is ready."));
        })
        .unwrap();
        assert!(notify_count.load(Ordering::SeqCst) > 0);
        drop(_obs);
        let _ = (handle2, view2);
    }

    #[gpui_kit::test]
    fn delete_and_archive_session_switch_transcript_cleanly_without_stale_leak(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            let (tx, _rx) = mpsc::unbounded_channel();
            view.update(cx, |v, _| {
                v.daemon_tx = Some(tx);
            });
            let s1 = make_session("del1", AgentKind::Claude, "Alpha", false);
            let s2 = make_session("del2", AgentKind::Claude, "Beta", false);
            view.update(cx, |v, cx| {
                v.sessions = vec![s1.clone(), s2.clone()];
                v.chat_cache.put(
                    CacheKey::new("local", AgentKind::Claude, "del1"),
                    CachedSession {
                        source_path: s1.source_path.clone(),
                        messages: vec![ChatMessage {
                            id: "a1".into(),
                            role: MessageRole::Assistant,
                            text: "Alpha secret transcript".into(),
                            ..Default::default()
                        }],
                        ..Default::default()
                    },
                );
                v.chat_cache.put(
                    CacheKey::new("local", AgentKind::Claude, "del2"),
                    CachedSession {
                        source_path: s2.source_path.clone(),
                        messages: vec![ChatMessage {
                            id: "b1".into(),
                            role: MessageRole::Assistant,
                            text: "Beta transcript".into(),
                            ..Default::default()
                        }],
                        ..Default::default()
                    },
                );
                v.select_session(s1.clone(), cx);
            });
            window.render_frame(cx);
            assert_eq!(view.read(cx).messages[0].text, "Alpha secret transcript");

            // DeleteSession (Ctrl+Shift+W) switches transcript cleanly to Beta (zero stale Alpha leak)
            window.press("ctrl-shift-w", cx);
            window.render_frame(cx);
            {
                let v = view.read(cx);
                assert_eq!(v.selected_session.as_ref().unwrap().id, "del2");
                assert_eq!(v.messages.len(), 1);
                assert_eq!(v.messages[0].text, "Beta transcript");
                assert_eq!(
                    v.active_switch_token.as_ref().unwrap().key.session_id,
                    "del2"
                );
            }

            window.press("ctrl-shift-e", cx);
            window.press("ctrl-shift-e", cx);
            window.render_frame(cx);
            {
                let v = view.read(cx);
                assert!(
                    v.selected_session.is_none(),
                    "Done removes the final active chat"
                );
                assert!(
                    v.messages.is_empty(),
                    "the final chat must not leave a stale transcript"
                );
                assert!(v.active_switch_token.is_none());
                let key = v.session_key_for(&s2);
                assert!(
                    v.sidebar_state
                        .workspace_store
                        .waiting_sessions
                        .contains_key(&key)
                );
            }
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn multi_window_daemon_event_fanout_and_unread_pill_sync(cx: &mut TestAppContext) {
        let (handle1, view1) = open_test_view_with_entity(cx);
        let (handle2, view2) = open_test_view_with_entity(cx);

        let s1 = make_session("mw1", AgentKind::Claude, "Win1 Sess", false);
        let s2 = make_session("mw2", AgentKind::Claude, "Win2 Sess", false);

        cx.update_window(handle1, |_, _window, cx| {
            view1.update(cx, |v, cx| {
                v.sessions = vec![s1.clone(), s2.clone()];
                v.select_session(s1.clone(), cx);
                v.sync_sessions_to_shared(cx);
                v.toggle_unread_only(cx);
            });
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle2, |_, _window, cx| {
            view2.update(cx, |v, cx| {
                v.select_session(s2.clone(), cx);
            });
            assert!(view2.read(cx).sidebar_state.unread_only);
        })
        .unwrap();

        // Daemon History, HistoryAppend, and Commands arriving on Window 1 fan out to Window 2
        cx.update_window(handle1, |_, window, cx| {
            view1.update(cx, |v, cx| {
                v.handle_bridge_event(
                    UiBridgeEvent::Server(ServerEvent::History {
                        session_id: "mw2".into(),
                        messages: vec![ChatMessage {
                            id: "w2-m1".into(),
                            role: MessageRole::Assistant,
                            text: "Hello Window 2".into(),
                            ..Default::default()
                        }],
                        oldest_offset: Some(0),
                        latest_offset: Some(64),
                        has_more: false,
                        file_size: Some(64),
                        file_mtime_ms: Some(1000),
                        probe_sha256: Some("p2".into()),
                        todos: Vec::new(),
                    }),
                    cx,
                );
                v.handle_bridge_event(
                    UiBridgeEvent::Server(ServerEvent::HistoryAppend {
                        session_id: "mw2".into(),
                        messages: vec![ChatMessage {
                            id: "w2-m2".into(),
                            role: MessageRole::Assistant,
                            text: "Live append Window 2".into(),
                            ..Default::default()
                        }],
                        latest_offset: Some(128),
                        working: Some(false),
                        todos: Vec::new(),
                        probe_sha256: Some("p2b".into()),
                    }),
                    cx,
                );
                v.handle_bridge_event(
                    UiBridgeEvent::Server(ServerEvent::Commands {
                        agent: AgentKind::Claude,
                        cwd: Some("/srv/app".into()),
                        items: vec![SlashCommandItem {
                            name: "fanout-cmd".into(),
                            description: "Synced command".into(),
                            trigger: "/".into(),
                            source: "project".into(),
                            opens_in_terminal: false,
                        }],
                    }),
                    cx,
                );
            });
            // Clicking #sidebar-unread-only-pill clears unread_only AND syncs to Window 2
            window.render_frame(cx);
            window.click("sidebar-unread-only-pill", cx);
            window.render_frame(cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle2, |_, _window, cx| {
            let v2 = view2.read(cx);
            assert_eq!(
                v2.messages
                    .iter()
                    .map(|m| m.text.as_str())
                    .collect::<Vec<_>>(),
                vec!["Hello Window 2", "Live append Window 2"]
            );
            assert!(
                v2.slash_picker_state
                    .effective_catalog(AgentKind::Claude, TriggerMode::Slash)
                    .iter()
                    .any(|c| c.name == "fanout-cmd")
            );
            assert!(
                !v2.sidebar_state.unread_only,
                "clicking #sidebar-unread-only-pill must sync to sibling Ctrl+N windows"
            );
        })
        .unwrap();
        let _ = (handle1, view1, handle2, view2);
    }

    #[gpui_kit::test]
    fn inbox_prev_clamp_rename_guards_find_focus_and_update_error_sanitizer(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            let s1 = make_session("p1", AgentKind::Claude, "One", false);
            let s2 = make_session("p2", AgentKind::Claude, "Two", false);
            let s3 = make_session("p3", AgentKind::Claude, "Three", false);

            // 1. InboxPrev (Alt+K) clamps out-of-range selected_row before decrementing
            view.update(cx, |v, cx| {
                v.sessions = vec![s1.clone(), s2.clone(), s3.clone()];
                v.selected_session = Some(s1.clone());
                v.main_surface = MainSurface::Inbox;
                v.inbox_state.select_all_split();
                v.inbox_state.selected_row = 99;
                cx.notify();
            });
            window.render_frame(cx);
            window.press("alt-k", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).inbox_state.selected_row,
                1,
                "Alt+K must clamp selected_row=99 to count-1 (2) before decrementing to 1"
            );

            // 2. F2, Alt+J, Alt+K must NOT clobber draft or move selection while inline rename is active
            let k1 = view.read(cx).session_key_for(&s1);
            view.update(cx, |v, cx| {
                v.inbox_state.rename = Some(InlineRenameState::new(
                    k1,
                    "p1".into(),
                    "In-progress draft".into(),
                ));
                cx.notify();
            });
            window.render_frame(cx);
            window.press("f2", cx);
            window.press("alt-j", cx);
            window.press("alt-k", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).inbox_state.rename.as_ref().unwrap().draft,
                "In-progress draft"
            );
            assert_eq!(view.read(cx).inbox_state.selected_row, 1);

            // 3. Ctrl+F while prompt_input is focused moves focus away from composer so typing
            //    goes straight to Find bar without leaking into composer Input
            view.update(cx, |v, cx| {
                v.inbox_state.rename = None;
                v.main_surface = MainSurface::Chat;
                v.messages = vec![ChatMessage {
                    id: "m1".into(),
                    role: MessageRole::Assistant,
                    text: "Find this needle here".into(),
                    ..Default::default()
                }];
                v.prompt_input.update(cx, |input, cx| {
                    input.set_value("keep draft", window, cx);
                    input.focus(window, cx);
                });
                cx.notify();
            });
            window.render_frame(cx);
            window.press("ctrl-f", cx);
            for k in ["n", "e", "e", "d", "l", "e"] {
                window.press(k, cx);
            }
            window.render_frame(cx);
            assert!(view.read(cx).chat_state.find_open);
            assert_eq!(view.read(cx).chat_state.find_query, "needle");
            assert_eq!(view.read(cx).chat_state.find_matches, vec![0]);
            assert_eq!(
                view.read(cx).prompt_input.read(cx).value().as_ref(),
                "keep draft",
                "Ctrl+F typing must never leak into composer prompt_input"
            );
        })
        .unwrap();

        // 4. sanitize_update_error_copy catches millisecond/timeout patterns AND post-truncation words
        let ms_err = sanitize_update_error_copy(&anyhow::anyhow!(
            "new binary did not signal readiness within 15000ms"
        ));
        assert_eq!(ms_err, GENERIC_UPDATE_ERROR_COPY);
        assert!(!ms_err.contains("15000ms"));
        assert!(!contains_forbidden_duration_wording(&ms_err));

        // 173 'a's + " secondaries" -> first 180 chars end in " second" (forbidden after truncation!)
        let tricky = format!("{} secondaries failed", "a".repeat(173));
        assert!(!contains_forbidden_duration_wording(&tricky));
        let post_trunc = sanitize_update_error_copy(&anyhow::anyhow!("{tricky}"));
        assert_eq!(post_trunc, GENERIC_UPDATE_ERROR_COPY);
        assert!(!contains_forbidden_duration_wording(&post_trunc));

        // Plain non-duration error passes through untouched
        let plain =
            sanitize_update_error_copy(&anyhow::anyhow!("checksum mismatch on multibot.exe"));
        assert_eq!(plain, "checksum mismatch on multibot.exe");
    }

    #[gpui_kit::test]
    fn app_scoped_daemon_connection_survives_window1_close_guards_stale_disconnect_and_reconnects_closed_tx(
        cx: &mut TestAppContext,
    ) {
        let (handle1, view1) = open_test_view_with_entity(cx);
        let (handle2, view2) = open_test_view_with_entity(cx);
        let rt = cx
            .update_window(handle1, |_, _, cx| view1.read(cx).runtime.clone())
            .unwrap();

        let target_a = DaemonTarget::RemoteSsh {
            host: multibot_core::SshHostConfig {
                name: "host-a".into(),
                hostname: "10.0.0.11".into(),
                user: "ubuntu".into(),
                port: 22,
                identity_file: None,
            },
            bundled_linux_binary: None,
        };
        let target_b = DaemonTarget::RemoteSsh {
            host: multibot_core::SshHostConfig {
                name: "host-b".into(),
                hostname: "10.0.0.22".into(),
                user: "ubuntu".into(),
                port: 22,
                identity_file: None,
            },
            bundled_linux_binary: None,
        };

        // Part (b): Window 1 connects to Host A (conn_gen = 1), then Window 2 switches to Host B
        // (conn_gen = 2). A late Disconnected from Host A (conn_gen = 1) MUST be ignored!
        let (shutdown_a_tx, mut shutdown_a_rx) = oneshot::channel::<()>();
        let (req_a_tx, _req_a_rx) = mpsc::unbounded_channel::<ClientRequest>();
        cx.update(|cx| {
            let store = cx.global_mut::<SharedAppStore>();
            store.connection_gen = 1;
            store.connection_shutdown_tx = Some(shutdown_a_tx);
            store.connected_target = Some(target_a.clone());
            store.connected_host = Some("host-a".into());
            store.bump_revision();
            assert!(MultibotView::apply_daemon_bridge_event_to_app(
                1,
                UiBridgeEvent::Connected {
                    host_label: "host-a".into(),
                    req_tx: req_a_tx,
                },
                &rt,
                cx,
            ));
        });
        cx.run_until_parked();

        // Window 2 switches to Host B -> global connection_gen increments to 2 & drops Host A shutdown_tx
        cx.update_window(handle2, |_, _window, cx| {
            view2.update(cx, |v, cx| {
                v.connect_to_target("host-b".into(), target_b.clone(), cx);
            });
        })
        .unwrap();
        assert!(
            matches!(
                shutdown_a_rx.try_recv(),
                Err(oneshot::error::TryRecvError::Closed)
            ),
            "switching to Host B from Window 2 must drop Host A's global shutdown_tx"
        );
        let gen_b = cx.update(|cx| cx.global::<SharedAppStore>().connection_gen);
        assert_eq!(gen_b, 2);

        // Install a controllable shutdown handle + live daemon_tx for Host B (conn_gen = 2)
        let (shutdown_b_tx, mut shutdown_b_rx) = oneshot::channel::<()>();
        let (req_b_tx, req_b_rx) = mpsc::unbounded_channel::<ClientRequest>();
        cx.update(|cx| {
            cx.global_mut::<SharedAppStore>().connection_shutdown_tx = Some(shutdown_b_tx);
            assert!(MultibotView::apply_daemon_bridge_event_to_app(
                gen_b,
                UiBridgeEvent::Connected {
                    host_label: "host-b".into(),
                    req_tx: req_b_tx,
                },
                &rt,
                cx,
            ));
        });
        cx.run_until_parked();

        // Late UiBridgeEvent::Disconnected from Host A (conn_gen = 1) must be IGNORED!
        let stale_accepted = cx.update(|cx| {
            MultibotView::apply_daemon_bridge_event_to_app(
                1,
                UiBridgeEvent::Disconnected("Disconnected from host-a".into()),
                &rt,
                cx,
            )
        });
        assert!(!stale_accepted, "stale conn_gen=1 event must be rejected");
        cx.run_until_parked();
        cx.update(|cx| {
            let store = cx.global::<SharedAppStore>();
            assert!(
                store.daemon_tx.as_ref().is_some_and(|tx| !tx.is_closed()),
                "stale Host A Disconnected must NOT wipe Host B's live daemon_tx"
            );
            assert_eq!(store.connection_status, "Connected: host-b");
        });

        // Part (a): Close Window 1 while Window 2 stays open -> SharedAppStore connection stays LIVE!
        drop(view1);
        cx.update_window(handle1, |_, window, _cx| {
            window.remove_window();
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(
            cx.update(|cx| cx.global::<SharedAppStore>().window_count),
            1
        );
        assert!(
            matches!(
                shutdown_b_rx.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ),
            "closing Window 1 while Window 2 is open must NOT drop SharedAppStore.connection_shutdown_tx"
        );
        cx.update(|cx| {
            let store = cx.global::<SharedAppStore>();
            assert!(store.connection_shutdown_tx.is_some());
            assert!(store.daemon_tx.as_ref().is_some_and(|tx| !tx.is_closed()));
        });

        // Surviving Window 2 still receives App-scoped ServerEvents cleanly
        let live_sess = make_session(
            "survive-1",
            AgentKind::Claude,
            "Alive After W1 Close",
            false,
        );
        cx.update(|cx| {
            assert!(MultibotView::apply_daemon_bridge_event_to_app(
                gen_b,
                UiBridgeEvent::Server(ServerEvent::Sessions {
                    sessions: vec![live_sess.clone()],
                }),
                &rt,
                cx,
            ));
        });
        cx.run_until_parked();
        cx.update_window(handle2, |_, _window, cx| {
            let v2 = view2.read(cx);
            assert_eq!(v2.sessions.len(), 1);
            assert_eq!(v2.sessions[0].id, "survive-1");
            assert!(v2.daemon_tx.as_ref().is_some_and(|tx| !tx.is_closed()));
        })
        .unwrap();

        // If daemon_tx IS closed, Ctrl+Shift+R (ReconnectSession) must NOT falsely report
        // "Refreshed daemon sessions." — it must reconnect via SharedAppStore.connected_target!
        drop(req_b_rx);
        cx.update_window(handle2, |_, window, cx| {
            assert!(
                view2
                    .read(cx)
                    .daemon_tx
                    .as_ref()
                    .is_some_and(|tx| tx.is_closed())
            );
            window.render_frame(cx);
            window.press("ctrl-shift-r", cx);
            window.render_frame(cx);
            let v2 = view2.read(cx);
            assert_ne!(
                v2.notice.as_deref(),
                Some("Refreshed daemon sessions."),
                "ReconnectSession must not falsely report refresh on a closed daemon_tx"
            );
            assert!(
                v2.connection_status.starts_with("Connecting to host-b"),
                "ReconnectSession must reconnect closed daemon_tx, got: {}",
                v2.connection_status
            );
        })
        .unwrap();
        assert_eq!(
            cx.update(|cx| cx.global::<SharedAppStore>().connection_gen),
            gen_b + 1
        );

        // Closing the LAST remaining window (window_count == 0) tears down global shutdown_tx & daemon_tx
        drop(view2);
        cx.update_window(handle2, |_, window, _cx| {
            window.remove_window();
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            let store = cx.global::<SharedAppStore>();
            assert_eq!(store.window_count, 0);
            assert!(store.connection_shutdown_tx.is_none());
            assert!(store.daemon_tx.is_none());
        });
    }

    #[gpui_kit::test]
    fn multi_window_structural_older_page_pagination_prepends_without_clobbering_tail_or_disk_cache(
        cx: &mut TestAppContext,
    ) {
        let seq = APP_TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let cache_dir = std::env::temp_dir().join(format!(
            "multibot-m6-r3-pagination-cache-{}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&cache_dir).unwrap();

        let (handle1, view1) = open_test_view_with_entity(cx);
        let (handle2, view2) = open_test_view_with_entity(cx);
        let rt = cx
            .update_window(handle1, |_, _, cx| view1.read(cx).runtime.clone())
            .unwrap();
        let (req_tx, mut req_rx) = mpsc::unbounded_channel::<ClientRequest>();

        let sess_a = make_session("sess-a", AgentKind::Claude, "Session A", false);
        let sess_b = make_session("sess-b", AgentKind::Claude, "Session B", false);

        // Configure shared connection + Tier-2 disk cache directory on both windows
        cx.update(|cx| {
            let store = cx.global_mut::<SharedAppStore>();
            store.connection_gen = 1;
            store.connected_host = Some("local".into());
            store.daemon_tx = Some(req_tx);
            store.sessions = vec![sess_a.clone(), sess_b.clone()];
            store.bump_revision();
        });
        cx.run_until_parked();

        cx.update_window(handle1, |_, _window, cx| {
            view1.update(cx, |v, cx| {
                v.chat_cache.dir = Some(cache_dir.clone());
                v.select_session(sess_a.clone(), cx);
            });
        })
        .unwrap();
        cx.update_window(handle2, |_, _window, cx| {
            view2.update(cx, |v, cx| {
                v.chat_cache.dir = Some(cache_dir.clone());
                v.select_session(sess_a.clone(), cx);
            });
        })
        .unwrap();
        while req_rx.try_recv().is_ok() {}

        // Step 1: Both windows receive Session A tail (oldest=10920, latest=11700, file_size=11700)
        cx.update(|cx| {
            assert!(MultibotView::apply_daemon_bridge_event_to_app(
                1,
                UiBridgeEvent::Server(ServerEvent::History {
                    session_id: "sess-a".into(),
                    messages: vec![
                        ChatMessage {
                            id: "msg-029".into(),
                            role: MessageRole::User,
                            text: "Message 29".into(),
                            ..Default::default()
                        },
                        ChatMessage {
                            id: "msg-030".into(),
                            role: MessageRole::Assistant,
                            text: "Message 30".into(),
                            ..Default::default()
                        },
                    ],
                    oldest_offset: Some(10920),
                    latest_offset: Some(11700),
                    has_more: true,
                    file_size: Some(11700),
                    file_mtime_ms: Some(1_700_000_000_000),
                    probe_sha256: Some("b2ddfa98de80".into()),
                    todos: Vec::new(),
                }),
                &rt,
                cx,
            ));
        });
        cx.run_until_parked();

        // Step 2: Window 2 initiates older-page pagination (oldest=10140, latest=10920, file_size=11700)
        cx.update_window(handle2, |_, _window, cx| {
            view2.update(cx, |v, cx| {
                assert!(v.load_older_history(cx));
            });
        })
        .unwrap();
        cx.update(|cx| {
            assert!(MultibotView::apply_daemon_bridge_event_to_app(
                1,
                UiBridgeEvent::Server(ServerEvent::History {
                    session_id: "sess-a".into(),
                    messages: vec![
                        ChatMessage {
                            id: "msg-027".into(),
                            role: MessageRole::User,
                            text: "Message 27".into(),
                            ..Default::default()
                        },
                        ChatMessage {
                            id: "msg-028".into(),
                            role: MessageRole::Assistant,
                            text: "Message 28".into(),
                            ..Default::default()
                        },
                    ],
                    oldest_offset: Some(10140),
                    latest_offset: Some(10920),
                    has_more: true,
                    file_size: Some(11700),
                    file_mtime_ms: Some(1_700_000_000_000),
                    probe_sha256: Some("e3ea997a5645".into()),
                    todos: Vec::new(),
                }),
                &rt,
                cx,
            ));
        });
        cx.run_until_parked();

        // BOTH Window 1 and Window 2 must have prepended [msg-027, msg-028] ahead of [msg-029, msg-030]!
        let key_a = CacheKey::new("local", AgentKind::Claude, "sess-a");
        for (label, handle, view) in [("Window 1", handle1, &view1), ("Window 2", handle2, &view2)]
        {
            cx.update_window(handle, |_, _window, cx| {
                let v = view.read(cx);
                let ids: Vec<&str> = v.messages.iter().map(|m| m.id.as_str()).collect();
                assert_eq!(
                    ids,
                    vec!["msg-027", "msg-028", "msg-029", "msg-030"],
                    "{label} must preserve newest tail and prepend older page"
                );
                let cached = v.chat_cache.get(&key_a).expect("cached tail present");
                assert_eq!(cached.oldest_offset, 10140);
                assert_eq!(cached.consumed_offset, 11700);
                assert_eq!(cached.probe_sha256.as_deref(), Some("b2ddfa98de80"));
            })
            .unwrap();
        }

        // Verify Tier-2 disk cache STILL holds the newest tail (NOT clobbered by older page!)
        let mut disk_checker = ChatCache::new(cache_dir.clone());
        match disk_checker.load_from_disk(&key_a) {
            multibot_core::cache::DiskLoadResult::Hit(disk_sess) => {
                let disk_ids: Vec<&str> =
                    disk_sess.messages.iter().map(|m| m.id.as_str()).collect();
                assert_eq!(disk_ids, vec!["msg-029", "msg-030"]);
                assert_eq!(disk_sess.consumed_offset, 11700);
                assert_eq!(disk_sess.probe_sha256.as_deref(), Some("b2ddfa98de80"));
            }
            other => panic!("expected Tier-2 disk cache hit for Session A, got: {other:?}"),
        }

        // Step 3: Window 1 switches to Session B and clears Session A from its memory cache
        // (while pending_switch_tokens still holds Session A). Window 2 loads another older page
        // [msg-025, msg-026] -> Window 1's view AND Session A's Tier-2 disk cache stay intact!
        cx.update_window(handle1, |_, _window, cx| {
            view1.update(cx, |v, cx| {
                v.select_session(sess_b.clone(), cx);
                v.messages = vec![ChatMessage {
                    id: "b-only".into(),
                    role: MessageRole::Assistant,
                    text: "Session B message".into(),
                    ..Default::default()
                }];
                v.chat_cache.remove_memory(&key_a);
            });
        })
        .unwrap();
        cx.update_window(handle2, |_, _window, cx| {
            view2.update(cx, |v, cx| {
                assert!(v.load_older_history(cx));
            });
        })
        .unwrap();
        cx.update(|cx| {
            assert!(MultibotView::apply_daemon_bridge_event_to_app(
                1,
                UiBridgeEvent::Server(ServerEvent::History {
                    session_id: "sess-a".into(),
                    messages: vec![
                        ChatMessage {
                            id: "msg-025".into(),
                            role: MessageRole::User,
                            text: "Message 25".into(),
                            ..Default::default()
                        },
                        ChatMessage {
                            id: "msg-026".into(),
                            role: MessageRole::Assistant,
                            text: "Message 26".into(),
                            ..Default::default()
                        },
                    ],
                    oldest_offset: Some(9360),
                    latest_offset: Some(10140),
                    has_more: true,
                    file_size: Some(11700),
                    file_mtime_ms: Some(1_700_000_000_000),
                    probe_sha256: Some("aaaa11112222".into()),
                    todos: Vec::new(),
                }),
                &rt,
                cx,
            ));
        });
        cx.run_until_parked();

        // Window 1 stayed cleanly on Session B and did NOT cache truncated Session A
        cx.update_window(handle1, |_, _window, cx| {
            let v1 = view1.read(cx);
            assert_eq!(v1.selected_session.as_ref().unwrap().id, "sess-b");
            assert_eq!(
                v1.messages
                    .iter()
                    .map(|m| m.id.as_str())
                    .collect::<Vec<_>>(),
                vec!["b-only"]
            );
            assert!(v1.chat_cache.get(&key_a).is_none());
        })
        .unwrap();
        // Window 2 now has all 6 messages
        cx.update_window(handle2, |_, _window, cx| {
            let v2 = view2.read(cx);
            assert_eq!(
                v2.messages
                    .iter()
                    .map(|m| m.id.as_str())
                    .collect::<Vec<_>>(),
                vec![
                    "msg-025", "msg-026", "msg-027", "msg-028", "msg-029", "msg-030"
                ]
            );
        })
        .unwrap();
        // Tier-2 disk cache for Session A is STILL completely uncorrupted!
        match disk_checker.load_from_disk(&key_a) {
            multibot_core::cache::DiskLoadResult::Hit(disk_sess) => {
                let disk_ids: Vec<&str> =
                    disk_sess.messages.iter().map(|m| m.id.as_str()).collect();
                assert_eq!(disk_ids, vec!["msg-029", "msg-030"]);
                assert_eq!(disk_sess.consumed_offset, 11700);
                assert_eq!(disk_sess.probe_sha256.as_deref(), Some("b2ddfa98de80"));
            }
            other => panic!("expected intact Tier-2 disk cache for Session A, got: {other:?}"),
        }

        let _ = (handle1, view1, handle2, view2);
        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    #[gpui_kit::test]
    fn collapsed_sidebar_background_update_available_syncs_silently_for_palette_and_expand(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);

        // Collapse the sidebar (Open -> Collapsed)
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.press("ctrl-shift-b", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).sidebar_state.mode, SidebarMode::Collapsed);
            assert_eq!(view.read(cx).settings.sidebar_mode, SidebarMode::Collapsed);
        })
        .unwrap();
        cx.run_until_parked();

        // Observe cx.notify() redraws AFTER collapsing
        let notify_count = Arc::new(AtomicU64::new(0));
        let notify_count_c = notify_count.clone();
        let _obs = cx.update(|cx| {
            cx.observe(&view, move |_, _| {
                notify_count_c.fetch_add(1, Ordering::SeqCst);
            })
        });

        let rel = ReleaseInfo {
            version: "0.2.0".into(),
            tag: "v0.2.0".into(),
            published_at: "2026-11-26T00:00:00Z".into(),
            notes: vec!["Background release note".into()],
            asset: WindowsAssetInfo {
                asset_name: "multibot-windows-x64.zip".into(),
                url: "https://github.com/valentinyeo/multibot/releases/download/v0.2.0/multibot-windows-x64.zip".into(),
                sha256: "a".repeat(64),
                size_bytes: 2048,
                multibot_exe_sha256: "b".repeat(64),
                multibotd_linux_x86_64_sha256: "c".repeat(64),
            },
        };

        // Background poller discovers v0.2.0 while sidebar is Collapsed
        cx.update(|cx| {
            MultibotView::apply_update_bridge_event_to_app(
                UpdateBridgeEvent::CheckCompleted {
                    outcome: CheckOutcome::UpdateAvailable {
                        release: rel.clone(),
                        etag: Some("\"etag-v020\"".into()),
                    },
                    manual: false,
                },
                cx,
            );
        });
        cx.run_until_parked();

        // 1. ZERO cx.notify() redraws while sidebar is Collapsed!
        assert_eq!(
            notify_count.load(Ordering::SeqCst),
            0,
            "background UpdateAvailable while sidebar is Collapsed must trigger ZERO redraws"
        );

        // 2. State IS synced in memory so Ctrl+K Palette immediately shows Download & Relaunch + Changelog!
        cx.update_window(handle, |_, window, cx| {
            {
                let v = view.read(cx);
                assert_eq!(v.update_ui.state, UpdateState::Available(rel.clone()));
                assert_eq!(
                    v.update_ui
                        .last_release
                        .as_ref()
                        .map(|r| r.version.as_str()),
                    Some("0.2.0")
                );
                let entries = v.update_ui.palette_entries(multibot_proto::build_version());
                assert!(
                    entries
                        .iter()
                        .any(|e| e.label == "Update: Download & Relaunch"),
                    "Ctrl+K must immediately offer 'Update: Download & Relaunch', got: {entries:?}"
                );
                assert!(!v.update_ui.is_card_visible(
                    v.settings.sidebar_mode,
                    v.sidebar_state.is_shown(),
                    v.settings.dismissed_update_version.as_deref(),
                ));
            }

            // Open Ctrl+K and verify Changelog opens independently of update notes
            window.press("ctrl-k", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::Palette);
            view.update(cx, |v, cx| {
                assert!(v.execute_update_palette_command(
                    crate::update_ui::PALETTE_CMD_UPDATE_CHANGELOG,
                    cx
                ));
            });
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::Changelog);
            assert!(window.find("changelog-modal").visible());
            window.press("escape", cx);
            window.render_frame(cx);

            // 3. Expand sidebar back to Open (Collapsed -> Hidden -> Open via Ctrl+Shift+B x2)
            window.press("ctrl-shift-b", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).sidebar_state.mode, SidebarMode::Hidden);
            window.press("ctrl-shift-b", cx);
            window.render_frame(cx);

            let v = view.read(cx);
            assert_eq!(v.sidebar_state.mode, SidebarMode::Open);
            assert_eq!(v.update_ui.state, UpdateState::Available(rel));
            assert!(
                v.update_ui.is_card_visible(
                    v.settings.sidebar_mode,
                    v.sidebar_state.is_shown(),
                    v.settings.dismissed_update_version.as_deref(),
                ),
                "Update Card must immediately become visible when expanding sidebar to Open"
            );
        })
        .unwrap();

        drop(_obs);
        let _ = (handle, view);
    }

    #[gpui_kit::test]
    fn partial_trailing_line_history_loads_cold_warm_and_truncated_reset(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        let rt = cx
            .update_window(handle, |_, _, cx| view.read(cx).runtime.clone())
            .unwrap();
        let (req_tx, mut req_rx) = mpsc::unbounded_channel::<ClientRequest>();

        let sess = make_session("sess-partial", AgentKind::Claude, "Active Partial", true);
        let key = CacheKey::new("local", AgentKind::Claude, "sess-partial");

        cx.update(|cx| {
            let store = cx.global_mut::<SharedAppStore>();
            store.connection_gen = 1;
            store.connected_host = Some("local".into());
            store.daemon_tx = Some(req_tx);
            store.sessions = vec![sess.clone()];
            store.bump_revision();
        });
        cx.run_until_parked();

        // 1. Cold-open uncached session with partial trailing line (latest_offset=138 < file_size=214)
        cx.update_window(handle, |_, _window, cx| {
            view.update(cx, |v, cx| {
                v.select_session(sess.clone(), cx);
            });
        })
        .unwrap();
        while req_rx.try_recv().is_ok() {}

        cx.update(|cx| {
            assert!(MultibotView::apply_daemon_bridge_event_to_app(
                1,
                UiBridgeEvent::Server(ServerEvent::History {
                    session_id: "sess-partial".into(),
                    messages: vec![
                        ChatMessage {
                            id: "u1".into(),
                            role: MessageRole::User,
                            text: "Hello agent".into(),
                            ..Default::default()
                        },
                        ChatMessage {
                            id: "a1".into(),
                            role: MessageRole::Assistant,
                            text: "Working on it...".into(),
                            ..Default::default()
                        },
                    ],
                    oldest_offset: Some(0),
                    latest_offset: Some(138),
                    has_more: false,
                    file_size: Some(214),
                    file_mtime_ms: Some(1_700_000_000_000),
                    probe_sha256: Some("probe-138".into()),
                    todos: Vec::new(),
                }),
                &rt,
                cx,
            ));
        });
        cx.run_until_parked();

        cx.update_window(handle, |_, _window, cx| {
            let v = view.read(cx);
            let ids: Vec<&str> = v.messages.iter().map(|m| m.id.as_str()).collect();
            assert_eq!(
                ids,
                vec!["u1", "a1"],
                "cold-open uncached session with partial trailing line must NOT be dropped"
            );
            let cached = v
                .chat_cache
                .get(&key)
                .expect("cold-open must populate Tier-1 cache");
            assert_eq!(cached.oldest_offset, 0);
            assert_eq!(cached.consumed_offset, 138);
            assert_eq!(cached.file_size, 214);
        })
        .unwrap();

        // 2. Warm-refresh cached session where file grew with partial trailing line (500 < 530)
        cx.update(|cx| {
            assert!(MultibotView::apply_daemon_bridge_event_to_app(
                1,
                UiBridgeEvent::Server(ServerEvent::History {
                    session_id: "sess-partial".into(),
                    messages: vec![
                        ChatMessage {
                            id: "u1".into(),
                            role: MessageRole::User,
                            text: "Hello agent".into(),
                            ..Default::default()
                        },
                        ChatMessage {
                            id: "a1".into(),
                            role: MessageRole::Assistant,
                            text: "Working on it...".into(),
                            ..Default::default()
                        },
                        ChatMessage {
                            id: "u2".into(),
                            role: MessageRole::User,
                            text: "Follow up".into(),
                            ..Default::default()
                        },
                        ChatMessage {
                            id: "a2".into(),
                            role: MessageRole::Assistant,
                            text: "Second reply".into(),
                            ..Default::default()
                        },
                    ],
                    oldest_offset: Some(0),
                    latest_offset: Some(500),
                    has_more: false,
                    file_size: Some(530),
                    file_mtime_ms: Some(1_700_000_001_000),
                    probe_sha256: Some("probe-500".into()),
                    todos: Vec::new(),
                }),
                &rt,
                cx,
            ));
        });
        cx.run_until_parked();

        cx.update_window(handle, |_, _window, cx| {
            let v = view.read(cx);
            let ids: Vec<&str> = v.messages.iter().map(|m| m.id.as_str()).collect();
            assert_eq!(
                ids,
                vec!["u1", "a1", "u2", "a2"],
                "warm-refresh with partial trailing line must update messages cleanly"
            );
            let cached = v.chat_cache.get(&key).expect("cached session present");
            assert_eq!(cached.consumed_offset, 500);
            assert_eq!(cached.file_size, 530);
        })
        .unwrap();

        // 3. Truncated/rotated reset with partial trailing line (cached_file_size=11700 -> 119 < 156)
        cx.update(|cx| {
            assert!(MultibotView::apply_daemon_bridge_event_to_app(
                1,
                UiBridgeEvent::Server(ServerEvent::History {
                    session_id: "sess-partial".into(),
                    messages: vec![
                        ChatMessage {
                            id: "old-29".into(),
                            role: MessageRole::User,
                            text: "Old 29".into(),
                            ..Default::default()
                        },
                        ChatMessage {
                            id: "old-30".into(),
                            role: MessageRole::Assistant,
                            text: "Old 30".into(),
                            ..Default::default()
                        },
                    ],
                    oldest_offset: Some(10920),
                    latest_offset: Some(11700),
                    has_more: true,
                    file_size: Some(11700),
                    file_mtime_ms: Some(1_700_000_002_000),
                    probe_sha256: Some("probe-11700".into()),
                    todos: Vec::new(),
                }),
                &rt,
                cx,
            ));
        });
        cx.run_until_parked();

        cx.update(|cx| {
            assert!(MultibotView::apply_daemon_bridge_event_to_app(
                1,
                UiBridgeEvent::Server(ServerEvent::History {
                    session_id: "sess-partial".into(),
                    messages: vec![ChatMessage {
                        id: "rot-1".into(),
                        role: MessageRole::User,
                        text: "Fresh rotated partial".into(),
                        ..Default::default()
                    }],
                    oldest_offset: Some(0),
                    latest_offset: Some(119),
                    has_more: false,
                    file_size: Some(156),
                    file_mtime_ms: Some(1_700_000_003_000),
                    probe_sha256: Some("probe-119".into()),
                    todos: Vec::new(),
                }),
                &rt,
                cx,
            ));
        });
        cx.run_until_parked();

        cx.update_window(handle, |_, _window, cx| {
            let v = view.read(cx);
            let ids: Vec<&str> = v.messages.iter().map(|m| m.id.as_str()).collect();
            assert_eq!(
                ids,
                vec!["rot-1"],
                "truncated/rotated reset with partial trailing line must replace stale cache cleanly"
            );
            let cached = v.chat_cache.get(&key).expect("cached session present");
            assert_eq!(cached.oldest_offset, 0);
            assert_eq!(cached.consumed_offset, 119);
            assert_eq!(cached.file_size, 156);
            assert!(!cached.has_more);
        })
        .unwrap();

        let _ = (handle, view);
    }

    #[gpui_kit::test]
    fn rotated_truncated_fallback_during_pending_older_page_replaces_stale_transcript(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        let rt = cx
            .update_window(handle, |_, _, cx| view.read(cx).runtime.clone())
            .unwrap();
        let (req_tx, mut req_rx) = mpsc::unbounded_channel::<ClientRequest>();

        let sess = make_session("sess-rot", AgentKind::Claude, "Rotated Session", false);
        let key = CacheKey::new("local", AgentKind::Claude, "sess-rot");

        cx.update(|cx| {
            let store = cx.global_mut::<SharedAppStore>();
            store.connection_gen = 1;
            store.connected_host = Some("local".into());
            store.daemon_tx = Some(req_tx);
            store.sessions = vec![sess.clone()];
            store.bump_revision();
        });
        cx.run_until_parked();

        cx.update_window(handle, |_, _window, cx| {
            view.update(cx, |v, cx| {
                v.select_session(sess.clone(), cx);
            });
        })
        .unwrap();
        while req_rx.try_recv().is_ok() {}

        // Test BOTH newline-terminated (149/149) AND partial-trailing-line (119/156) rotated fallbacks
        for (variant, latest, fsize, msg_id) in [
            ("newline-terminated", 149u64, 149u64, "rotated-001"),
            (
                "partial-trailing-line",
                119u64,
                156u64,
                "rotated-partial-001",
            ),
        ] {
            // Seed pre-truncation tail (oldest=11667, latest=12501, file_size=12501)
            cx.update(|cx| {
                assert!(MultibotView::apply_daemon_bridge_event_to_app(
                    1,
                    UiBridgeEvent::Server(ServerEvent::History {
                        session_id: "sess-rot".into(),
                        messages: vec![
                            ChatMessage {
                                id: "old-029".into(),
                                role: MessageRole::User,
                                text: "Old 29".into(),
                                ..Default::default()
                            },
                            ChatMessage {
                                id: "old-030".into(),
                                role: MessageRole::Assistant,
                                text: "Old 30".into(),
                                ..Default::default()
                            },
                        ],
                        oldest_offset: Some(11667),
                        latest_offset: Some(12501),
                        has_more: true,
                        file_size: Some(12501),
                        file_mtime_ms: Some(1_700_000_000_000),
                        probe_sha256: Some("probe-12501".into()),
                        todos: Vec::new(),
                    }),
                    &rt,
                    cx,
                ));
            });
            cx.run_until_parked();

            // Initiate load_older_history (records 11667 in BOTH local AND App-scoped pending queues)
            cx.update_window(handle, |_, _window, cx| {
                view.update(cx, |v, cx| {
                    assert!(v.load_older_history(cx));
                    assert_eq!(v.pending_older_page_before.get("sess-rot"), Some(&11667));
                });
            })
            .unwrap();
            assert_eq!(
                cx.update(|cx| {
                    cx.global::<SharedAppStore>()
                        .pending_older_page_before
                        .get("sess-rot")
                        .cloned()
                        .map(|q| q.into_iter().collect::<Vec<_>>())
                }),
                Some(vec![11667])
            );
            while req_rx.try_recv().is_ok() {}

            // Transcript rotates/truncates while older-page request is in flight: daemon falls back
            cx.update(|cx| {
                assert!(MultibotView::apply_daemon_bridge_event_to_app(
                    1,
                    UiBridgeEvent::Server(ServerEvent::History {
                        session_id: "sess-rot".into(),
                        messages: vec![ChatMessage {
                            id: msg_id.into(),
                            role: MessageRole::User,
                            text: format!("Rotated ({variant})"),
                            ..Default::default()
                        }],
                        oldest_offset: Some(0),
                        latest_offset: Some(latest),
                        has_more: false,
                        file_size: Some(fsize),
                        file_mtime_ms: Some(1_700_000_005_000),
                        probe_sha256: Some(format!("probe-{latest}")),
                        todos: Vec::new(),
                    }),
                    &rt,
                    cx,
                ));
            });
            cx.run_until_parked();

            // Must REPLACE stale transcript cleanly (ZERO prepending ahead of old-029/old-030)
            cx.update_window(handle, |_, _window, cx| {
                let v = view.read(cx);
                let ids: Vec<&str> = v.messages.iter().map(|m| m.id.as_str()).collect();
                assert_eq!(
                    ids,
                    vec![msg_id],
                    "variant={variant}: rotated fallback during pending load_older_history must replace stale transcript"
                );
                let cached = v.chat_cache.get(&key).expect("cached session present");
                assert_eq!(cached.oldest_offset, 0);
                assert_eq!(cached.consumed_offset, latest);
                assert_eq!(cached.file_size, fsize);
                assert!(!cached.has_more);
                assert!(
                    !v.pending_older_page_before.contains_key("sess-rot"),
                    "variant={variant}: local pending_older_page_before must be cleared"
                );
            })
            .unwrap();
            assert!(
                cx.update(|cx| {
                    !cx.global::<SharedAppStore>()
                        .pending_older_page_before
                        .contains_key("sess-rot")
                }),
                "variant={variant}: App-scoped pending_older_page_before must be pruned"
            );
        }

        let _ = (handle, view);
    }

    #[gpui_kit::test]
    fn non_contiguous_sibling_older_page_ignored_and_evicted_sibling_preserves_disk_cache(
        cx: &mut TestAppContext,
    ) {
        let seq = APP_TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let cache_dir = std::env::temp_dir().join(format!(
            "multibot-m6-r4-noncontig-cache-{}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&cache_dir).unwrap();

        let (handle1, view1) = open_test_view_with_entity(cx);
        let (handle2, view2) = open_test_view_with_entity(cx);
        let (handle3, view3) = open_test_view_with_entity(cx);
        let rt = cx
            .update_window(handle1, |_, _, cx| view1.read(cx).runtime.clone())
            .unwrap();
        let (req_tx, mut req_rx) = mpsc::unbounded_channel::<ClientRequest>();

        let sess_a = make_session("sess-a", AgentKind::Claude, "Session A", false);
        let sess_b = make_session("sess-b", AgentKind::Claude, "Session B", false);
        let key_a = CacheKey::new("local", AgentKind::Claude, "sess-a");

        cx.update(|cx| {
            let store = cx.global_mut::<SharedAppStore>();
            store.connection_gen = 1;
            store.connected_host = Some("local".into());
            store.daemon_tx = Some(req_tx);
            store.sessions = vec![sess_a.clone(), sess_b.clone()];
            store.bump_revision();
        });
        cx.run_until_parked();

        // All 3 windows select Session A and share the isolated Tier-2 disk cache directory
        for (handle, view) in [(handle1, &view1), (handle2, &view2), (handle3, &view3)] {
            cx.update_window(handle, |_, _window, cx| {
                view.update(cx, |v, cx| {
                    v.chat_cache.dir = Some(cache_dir.clone());
                    v.select_session(sess_a.clone(), cx);
                });
            })
            .unwrap();
        }
        while req_rx.try_recv().is_ok() {}

        // Step 1: Initial tail [msg-029, msg-030] at [10920..11700] arrives on all 3 windows
        cx.update(|cx| {
            assert!(MultibotView::apply_daemon_bridge_event_to_app(
                1,
                UiBridgeEvent::Server(ServerEvent::History {
                    session_id: "sess-a".into(),
                    messages: vec![
                        ChatMessage {
                            id: "msg-029".into(),
                            role: MessageRole::User,
                            text: "Message 29".into(),
                            ..Default::default()
                        },
                        ChatMessage {
                            id: "msg-030".into(),
                            role: MessageRole::Assistant,
                            text: "Message 30".into(),
                            ..Default::default()
                        },
                    ],
                    oldest_offset: Some(10920),
                    latest_offset: Some(11700),
                    has_more: true,
                    file_size: Some(11700),
                    file_mtime_ms: Some(1_700_000_000_000),
                    probe_sha256: Some("eof-probe-11700".into()),
                    todos: Vec::new(),
                }),
                &rt,
                cx,
            ));
        });
        cx.run_until_parked();

        // Step 2: Window 1 already paginated down to oldest_offset=10140 ([msg-027..msg-030]);
        // Window 2 stays at shallower oldest_offset=10920 ([msg-029, msg-030]);
        // Window 3 switches to Session B and evicts Session A from memory cache.
        cx.update_window(handle1, |_, _window, cx| {
            view1.update(cx, |v, _| {
                let mut cached = v.chat_cache.get(&key_a).cloned().unwrap();
                cached.messages = vec![
                    ChatMessage {
                        id: "msg-027".into(),
                        role: MessageRole::User,
                        text: "Message 27".into(),
                        ..Default::default()
                    },
                    ChatMessage {
                        id: "msg-028".into(),
                        role: MessageRole::Assistant,
                        text: "Message 28".into(),
                        ..Default::default()
                    },
                    ChatMessage {
                        id: "msg-029".into(),
                        role: MessageRole::User,
                        text: "Message 29".into(),
                        ..Default::default()
                    },
                    ChatMessage {
                        id: "msg-030".into(),
                        role: MessageRole::Assistant,
                        text: "Message 30".into(),
                        ..Default::default()
                    },
                ];
                cached.oldest_offset = 10140;
                v.messages = cached.messages.clone();
                v.chat_cache.put(key_a.clone(), cached);
            });
        })
        .unwrap();
        cx.update_window(handle3, |_, _window, cx| {
            view3.update(cx, |v, cx| {
                v.select_session(sess_b.clone(), cx);
                v.messages = vec![ChatMessage {
                    id: "b-only".into(),
                    role: MessageRole::Assistant,
                    text: "Session B".into(),
                    ..Default::default()
                }];
                v.chat_cache.remove_memory(&key_a);
            });
        })
        .unwrap();
        while req_rx.try_recv().is_ok() {}

        // Step 3: Window 1 calls load_older_history (before_offset = 10140) and receives [9360..10140]
        cx.update_window(handle1, |_, _window, cx| {
            view1.update(cx, |v, cx| {
                assert!(v.load_older_history(cx));
            });
        })
        .unwrap();
        cx.update(|cx| {
            assert!(MultibotView::apply_daemon_bridge_event_to_app(
                1,
                UiBridgeEvent::Server(ServerEvent::History {
                    session_id: "sess-a".into(),
                    messages: vec![
                        ChatMessage {
                            id: "msg-025".into(),
                            role: MessageRole::User,
                            text: "Message 25".into(),
                            ..Default::default()
                        },
                        ChatMessage {
                            id: "msg-026".into(),
                            role: MessageRole::Assistant,
                            text: "Message 26".into(),
                            ..Default::default()
                        },
                    ],
                    oldest_offset: Some(9360),
                    latest_offset: Some(10140),
                    has_more: true,
                    file_size: Some(11700),
                    file_mtime_ms: Some(1_700_000_000_000),
                    probe_sha256: Some("probe-10140".into()),
                    todos: Vec::new(),
                }),
                &rt,
                cx,
            ));
        });
        cx.run_until_parked();

        // Window 1 prepended [msg-025, msg-026] cleanly (oldest_offset = 9360)
        cx.update_window(handle1, |_, _window, cx| {
            let v1 = view1.read(cx);
            let ids: Vec<&str> = v1.messages.iter().map(|m| m.id.as_str()).collect();
            assert_eq!(
                ids,
                vec![
                    "msg-025", "msg-026", "msg-027", "msg-028", "msg-029", "msg-030"
                ]
            );
            let c1 = v1.chat_cache.get(&key_a).unwrap();
            assert_eq!(c1.oldest_offset, 9360);
            assert_eq!(c1.consumed_offset, 11700);
        })
        .unwrap();

        // Window 2 at shallower oldest_offset=10920 MUST ignore non-contiguous [9360..10140] (ZERO middle gap!)
        cx.update_window(handle2, |_, _window, cx| {
            let v2 = view2.read(cx);
            let ids: Vec<&str> = v2.messages.iter().map(|m| m.id.as_str()).collect();
            assert_eq!(
                ids,
                vec!["msg-029", "msg-030"],
                "Window 2 must ignore non-contiguous older page and avoid creating an unreachable middle gap"
            );
            let c2 = v2.chat_cache.get(&key_a).unwrap();
            assert_eq!(c2.oldest_offset, 10920);
            assert_eq!(c2.consumed_offset, 11700);
        })
        .unwrap();

        // Window 3 (evicted Session A from memory) ignored the older page via is_older_page_hint=true
        cx.update_window(handle3, |_, _window, cx| {
            let v3 = view3.read(cx);
            assert_eq!(v3.selected_session.as_ref().unwrap().id, "sess-b");
            let ids: Vec<&str> = v3.messages.iter().map(|m| m.id.as_str()).collect();
            assert_eq!(ids, vec!["b-only"]);
            assert!(v3.chat_cache.get(&key_a).is_none());
        })
        .unwrap();

        // Tier-2 disk cache for Session A is completely intact (NOT clobbered by older-only slice!)
        let mut disk_checker = ChatCache::new(cache_dir.clone());
        match disk_checker.load_from_disk(&key_a) {
            multibot_core::cache::DiskLoadResult::Hit(disk_sess) => {
                let disk_ids: Vec<&str> =
                    disk_sess.messages.iter().map(|m| m.id.as_str()).collect();
                assert_eq!(disk_ids, vec!["msg-029", "msg-030"]);
                assert_eq!(disk_sess.consumed_offset, 11700);
                assert_eq!(disk_sess.probe_sha256.as_deref(), Some("eof-probe-11700"));
            }
            other => panic!("expected intact Tier-2 disk cache for Session A, got: {other:?}"),
        }

        // App-scoped pending_older_page_before was cleanly consumed
        assert!(cx.update(|cx| {
            cx.global::<SharedAppStore>()
                .pending_older_page_before
                .is_empty()
        }));

        let _ = (handle1, view1, handle2, view2, handle3, view3);
        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    #[gpui_kit::test]
    fn multi_window_hello_ack_flushes_shared_chat_outbox_exactly_once(cx: &mut TestAppContext) {
        let (handle1, view1) = open_test_view_with_entity(cx);
        let (handle2, view2) = open_test_view_with_entity(cx);
        let rt = cx
            .update_window(handle1, |_, _, cx| view1.read(cx).runtime.clone())
            .unwrap();
        let (req_tx, mut req_rx) = mpsc::unbounded_channel::<ClientRequest>();

        let sess = make_session("off-mw", AgentKind::Claude, "Multi-Window Offline", false);

        // Start offline with both windows sharing SharedAppStore
        cx.update(|cx| {
            let store = cx.global_mut::<SharedAppStore>();
            store.connection_gen = 1;
            store.connected_host = Some("local".into());
            store.daemon_tx = None;
            store.sessions = vec![sess.clone()];
            store.bump_revision();
        });
        cx.run_until_parked();

        // Queue offline prompts from BOTH Window 1 and Window 2
        cx.update_window(handle1, |_, _window, cx| {
            view1.update(cx, |v, _| {
                v.selected_session = Some(sess.clone());
                v.send_or_queue_prompt(&sess, "w1-local-0".into(), "Offline prompt 1".into());
            });
        })
        .unwrap();
        cx.update_window(handle2, |_, _window, cx| {
            view2.update(cx, |v, _| {
                v.selected_session = Some(sess.clone());
                v.send_or_queue_prompt(&sess, "w2-local-0".into(), "Offline prompt 2".into());
                v.send_or_queue_prompt(&sess, "w2-local-1".into(), "Offline prompt 3".into());
            });
        })
        .unwrap();

        assert_eq!(
            cx.update(|cx| {
                cx.global::<SharedAppStore>()
                    .shared_chat_outbox
                    .lock()
                    .unwrap()
                    .outbox
                    .entries
                    .len()
            }),
            3,
            "both windows must enqueue into the single App-scoped SharedChatOutbox"
        );

        // Reconnect + HelloAck + Sessions broadcast to BOTH windows via App-scoped bridge
        cx.update(|cx| {
            assert!(MultibotView::apply_daemon_bridge_event_to_app(
                1,
                UiBridgeEvent::Connected {
                    host_label: "local".into(),
                    req_tx,
                },
                &rt,
                cx,
            ));
            assert!(MultibotView::apply_daemon_bridge_event_to_app(
                1,
                UiBridgeEvent::Server(ServerEvent::HelloAck {
                    protocol_version: 1,
                    hostname: "devbox".into(),
                    daemon_version: multibot_proto::build_version().into(),
                    binary_hash: "0123456789ab".into(),
                    pid: 4321,
                }),
                &rt,
                cx,
            ));
            assert!(MultibotView::apply_daemon_bridge_event_to_app(
                1,
                UiBridgeEvent::Server(ServerEvent::Sessions {
                    sessions: vec![sess.clone()],
                }),
                &rt,
                cx,
            ));
        });
        cx.run_until_parked();

        for remaining in [1, 0] {
            for working in [true, false] {
                cx.update(|cx| {
                    let mut session = sess.clone();
                    session.working = working;
                    MultibotView::apply_daemon_bridge_event_to_app(1,
                        UiBridgeEvent::Server(ServerEvent::SessionUpdated { session }), &rt, cx);
                });
            }
            cx.run_until_parked();
            assert_eq!(cx.update(|cx| cx.global::<SharedAppStore>().shared_chat_outbox.lock().unwrap().outbox.entries.len()), remaining);
        }

        // Each queued prompt MUST have been sent EXACTLY ONCE over daemon_tx (zero duplicates!)
        let mut sent_texts = Vec::new();
        while let Ok(req) = req_rx.try_recv() {
            if let ClientRequest::SendPrompt { text, .. } = req {
                sent_texts.push(text);
            }
        }
        assert_eq!(
            sent_texts,
            vec!["Offline prompt 1", "Offline prompt 2", "Offline prompt 3"],
            "multi-window HelloAck must flush each queued offline prompt exactly once in FIFO order"
        );

        // Shared outbox and both windows' local snapshots/maps are cleanly drained
        assert!(cx.update(|cx| {
            cx.global::<SharedAppStore>()
                .shared_chat_outbox
                .lock()
                .unwrap()
                .outbox
                .entries
                .is_empty()
        }));
        for (handle, view) in [(handle1, &view1), (handle2, &view2)] {
            cx.update_window(handle, |_, _window, cx| {
                let v = view.read(cx);
                assert!(v.chat_outbox.entries.is_empty());
                assert!(v.outbox_msg_map.is_empty());
            })
            .unwrap();
        }

        let _ = (handle1, view1, handle2, view2);
    }

    #[gpui_kit::test]
    fn f2_and_palette_rename_in_collapsed_sidebar_with_focused_composer_and_overlays(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        let sess = make_session("ren-col-1", AgentKind::Claude, "Original", false);

        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.sessions = vec![sess.clone()];
                v.selected_session = Some(sess.clone());
                v.sidebar_state.mode = SidebarMode::Collapsed;
                v.settings.sidebar_mode = SidebarMode::Collapsed;
                v.chat_state.find_open = true;
                v.chat_state.focused_message_idx = Some(0);
                v.chat_state.reply_quote = Some(crate::chat_view::ReplyQuote {
                    message_id: "m0".into(),
                    preview: "q".into(),
                    full_text: "q".into(),
                });
                v.prompt_input.update(cx, |inp, cx| {
                    inp.set_value("hello", window, cx);
                    inp.focus(window, cx);
                });
                cx.notify();
            });
            window.render_frame(cx);

            // Press real F2 while sidebar is Collapsed, prompt-input is focused, and overlays are open
            window.press("f2", cx);
            window.render_frame(cx);
            assert!(view.read(cx).sidebar_state.rename.is_some());
            assert!(!view.read(cx).chat_state.find_open);
            assert!(view.read(cx).chat_state.reply_quote.is_none());
            assert_eq!(view.read(cx).chat_state.focused_message_idx, None);

            // Backspace clears the selected name, not the composer.
            window.press("backspace", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).prompt_input.read(cx).value().as_ref(),
                "hello",
                "Backspace during Collapsed-sidebar F2 rename must NOT edit prompt-input"
            );
            assert_eq!(
                view.read(cx)
                    .sidebar_state
                    .rename
                    .as_ref()
                    .map(|r| r.draft.as_str()),
                Some("")
            );

            // Real Enter must commit rename, NOT send "hello" as a chat message!
            window.press("x", cx);
            window.press("enter", cx);
            window.render_frame(cx);
            assert!(view.read(cx).sidebar_state.rename.is_none());
            assert!(
                view.read(cx).messages.is_empty(),
                "Enter during Collapsed-sidebar F2 rename must NOT send prompt-input"
            );
            let key = view.read(cx).session_key_for(&sess);
            assert_eq!(
                view.read(cx)
                    .sidebar_state
                    .workspace_store
                    .effective_title(&key, "Original"),
                "x"
            );

            // Also verify Ctrl+K -> Rename session in Hidden sidebar mode with focused composer
            view.update(cx, |v, cx| {
                v.sidebar_state.mode = SidebarMode::Hidden;
                v.settings.sidebar_mode = SidebarMode::Hidden;
                v.prompt_input.update(cx, |inp, cx| {
                    inp.set_value("keepme", window, cx);
                    inp.focus(window, cx);
                });
                cx.notify();
            });
            window.render_frame(cx);

            window.press("ctrl-k", cx);
            window.render_frame(cx);
            view.update(cx, |v, cx| {
                v.palette_input.update(cx, |inp, cx| {
                    inp.set_value("rename session", window, cx);
                    inp.focus(window, cx);
                });
            });
            window.render_frame(cx);
            window.press("enter", cx);
            window.render_frame(cx);
            assert!(view.read(cx).sidebar_state.rename.is_some());

            window.press("backspace", cx);
            window.press("y", cx);
            window.press("enter", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).prompt_input.read(cx).value().as_ref(),
                "keepme"
            );
            assert!(view.read(cx).messages.is_empty());
            assert_eq!(
                view.read(cx)
                    .sidebar_state
                    .workspace_store
                    .effective_title(&key, "Original"),
                "y"
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn multi_window_workspace_create_waiting_and_unread_only_stay_synced(cx: &mut TestAppContext) {
        let (handle1, view1) = open_test_view_with_entity(cx);
        let (handle2, view2) = open_test_view_with_entity(cx);
        let mut sess = make_session("mw-ws-1", AgentKind::Claude, "MultiWin Session", false);
        sess.needs_attention = true;
        sess.last_assistant_id = Some("a-unread-1".into());
        sess.last_assistant_ms = Some(20_000);

        cx.update_window(handle1, |_, window, cx| {
            view1.update(cx, |v, cx| {
                v.sessions = vec![sess.clone()];
                v.selected_session = Some(sess.clone());
                v.sync_sessions_to_shared(cx);
                v.sync_workspace_to_shared(cx);
                cx.notify();
            });
            window.render_frame(cx);
            // Create Workspace 2 through the main palette in Window 1
            window.press("ctrl-k", cx);
            window.input("New workspace", cx);
            window.press("enter", cx);
            window.render_frame(cx);
            assert_eq!(
                view1
                    .read(cx)
                    .sidebar_state
                    .workspace_store
                    .workspaces
                    .len(),
                2
            );
        })
        .unwrap();
        cx.run_until_parked();

        // Window 2 MUST see Workspace 2 immediately AND toggling favorite in Window 2 must NOT wipe it!
        let key = cx
            .update_window(handle2, |_, window, cx| {
                window.render_frame(cx);
                assert_eq!(
                    view2
                        .read(cx)
                        .sidebar_state
                        .workspace_store
                        .workspaces
                        .len(),
                    2,
                    "Window 2 must receive Workspace 2 created via the palette in Window 1"
                );
                let key = view2.read(cx).session_key_for(&sess);
                view2.update(cx, |v, cx| {
                    v.selected_session = Some(sess.clone());
                    v.toggle_active_favorite(cx);
                });
                window.render_frame(cx);
                key
            })
            .unwrap();
        cx.run_until_parked();

        // Window 1 retains Workspace 2 AND sees Window 2's favorite!
        cx.update_window(handle1, |_, window, cx| {
            window.render_frame(cx);
            let v1 = view1.read(cx);
            assert_eq!(
                v1.sidebar_state.workspace_store.workspaces.len(),
                2,
                "Window 2 favorite sync must NOT overwrite Window 1's Workspace 2"
            );
            assert!(v1.sidebar_state.workspace_store.is_favorite(&key));

            // Clicking Recents in Window 1 must not collapse it.
            window.click("sidebar-recents-header", cx);
            window.render_frame(cx);

            // Switch Window 1 to Inbox (All split), toggle unread_only via Ctrl+Shift+U (the pill is
            // gone in minimal Inbox), and mark the session Waiting via 'e'
            view1.update(cx, |v, cx| {
                v.main_surface = MainSurface::Inbox;
                v.inbox_state.select_all_split();
                v.focus_inbox(window, cx);
                cx.notify();
            });
            window.render_frame(cx);
            window.press("ctrl-shift-u", cx);
            window.render_frame(cx);
            assert!(view1.read(cx).sidebar_state.unread_only);
            view1.update(cx, |v, cx| {
                v.focus_inbox(window, cx);
            });
            window.render_frame(cx);
            window.press("e", cx);
            window.render_frame(cx);
            assert!(
                view1
                    .read(cx)
                    .sidebar_state
                    .workspace_store
                    .waiting_sessions
                    .contains_key(&key)
            );
        })
        .unwrap();
        cx.run_until_parked();

        // Window 2 keeps Recents open and sees unread_only and waiting_sessions.
        cx.update_window(handle2, |_, window, cx| {
            window.render_frame(cx);
            let v2 = view2.read(cx);
            assert!(!v2.sidebar_state.recents_collapsed);
            assert!(v2.sidebar_state.unread_only);
            assert!(
                v2.sidebar_state
                    .workspace_store
                    .waiting_sessions
                    .contains_key(&key)
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn pending_attachments_cleared_on_select_session_and_clear_active_session_view(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        let tmp_dir =
            std::env::temp_dir().join(format!("multibot-app-att-clear-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp_dir);
        let secret_png = tmp_dir.join("secret.png");
        std::fs::write(&secret_png, b"\x89PNG\r\n\x1a\nsecret").unwrap();

        let sess_a = SessionSummary {
            id: "sess-a".into(),
            agent: AgentKind::Claude,
            title: "Session A".into(),
            ..Default::default()
        };
        let sess_b = SessionSummary {
            id: "sess-b".into(),
            agent: AgentKind::Codex,
            title: "Session B".into(),
            ..Default::default()
        };

        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.sessions = vec![sess_a.clone(), sess_b.clone()];
                v.select_session(sess_a.clone(), cx);
                v.stage_dropped_paths(&[secret_png.clone()], window, cx);
                assert_eq!(v.chat_state.pending_attachments.len(), 1);

                // Switching sessions must clear pending_attachments so Session A files never leak into Session B.
                v.select_session(sess_b.clone(), cx);
                assert!(
                    v.chat_state.pending_attachments.is_empty(),
                    "select_session must clear staged pending_attachments"
                );

                // Switching/disconnecting hosts calls clear_active_session_view, which must also clear pending_attachments.
                v.stage_dropped_paths(&[secret_png.clone()], window, cx);
                assert_eq!(v.chat_state.pending_attachments.len(), 1);
                v.clear_active_session_view();
                assert!(
                    v.chat_state.pending_attachments.is_empty(),
                    "clear_active_session_view must clear staged pending_attachments"
                );
            });
        })
        .unwrap();

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[gpui_kit::test]
    fn global_shortcuts_survive_unmounted_child_focus_transitions_and_inbox_buttons_focus_inbox(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        let sess = SessionSummary {
            id: "focus-s1".into(),
            agent: AgentKind::Claude,
            title: "Focus Test".into(),
            ..Default::default()
        };

        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.sessions = vec![sess.clone()];
                v.select_session(sess.clone(), cx);
            });
            window.render_frame(cx);

            // 1. Alt+I -> Esc -> Ctrl+K (leaving Inbox must not strand focus on unmounted inbox-root)
            window.press("alt-i", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);
            window.press("escape", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Chat);
            window.press("ctrl-k", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).active_modal,
                ActiveModal::Palette,
                "Ctrl+K must work after leaving Inbox with Escape"
            );
            window.press("escape", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::None);

            // 2. Focus prompt_input -> Ctrl+, (Settings) -> Ctrl+, (back to Chat) -> focus host_input -> Esc -> Ctrl+K
            view.update(cx, |v, cx| {
                v.prompt_input.update(cx, |inp, cx| inp.focus(window, cx));
            });
            window.render_frame(cx);
            window.press("ctrl-,", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Settings);
            window.press("ctrl-,", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).main_surface,
                MainSurface::Chat,
                "Ctrl+, must close Settings even when opened from focused prompt_input"
            );

            // Also verify leaving Settings while host_input is focused preserves global shortcuts
            window.press("ctrl-,", cx);
            window.render_frame(cx);
            view.update(cx, |v, cx| {
                v.host_input.update(cx, |inp, cx| inp.focus(window, cx));
            });
            window.render_frame(cx);
            window.press("ctrl-,", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Chat);
            window.press("ctrl-k", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).active_modal,
                ActiveModal::Palette,
                "Ctrl+K must work after closing Settings with focused host_input"
            );
            window.press("escape", cx);
            window.render_frame(cx);

            // 3. F2 -> Esc -> Ctrl+Shift+B (collapse sidebar) -> Ctrl+K
            window.press("f2", cx);
            window.render_frame(cx);
            assert!(view.read(cx).sidebar_state.rename.is_some());
            window.press("escape", cx);
            window.render_frame(cx);
            assert!(view.read(cx).sidebar_state.rename.is_none());
            window.press("ctrl-shift-b", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).sidebar_state.mode, SidebarMode::Collapsed);
            window.press("ctrl-k", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).active_modal,
                ActiveModal::Palette,
                "Ctrl+K must work after collapsing Sidebar while sidebar focus_handle was focused"
            );
            window.press("escape", cx);
            window.render_frame(cx);

            // Restore Open sidebar and test clicking #sidebar-inbox-btn focuses Inbox cleanly
            view.update(cx, |v, cx| {
                v.sidebar_state.mode = SidebarMode::Open;
                v.main_surface = MainSurface::Chat;
                cx.notify();
            });
            window.render_frame(cx);
            window.press("alt-i", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);
            assert!(
                view.read(cx)
                    .inbox_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window)),
                "clicking #sidebar-inbox-btn must focus inbox-root"
            );

            // Minimal Inbox hides the whole header: leave with Ctrl+Shift+I and re-enter with Alt+I
            window.press("ctrl-shift-i", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Chat);
            window.press("alt-i", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);
            assert!(
                view.read(cx)
                    .inbox_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window)),
                "Alt+I must focus inbox-root"
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn ctrl_slash_from_root_focuses_prompt_input_so_down_enter_and_tab_work(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        let sess = SessionSummary {
            id: "slash-browse-1".into(),
            agent: AgentKind::Claude,
            title: "Slash Browse".into(),
            ..Default::default()
        };

        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.sessions = vec![sess.clone()];
                v.select_session(sess.clone(), cx);
                window.focus(&v.root_focus_handle, cx);
                cx.notify();
            });
            window.render_frame(cx);

            // Press Ctrl+/ while focus is on root_focus_handle (NOT prompt_input)
            window.press("ctrl-/", cx);
            window.render_frame(cx);
            assert!(
                view.read(cx).slash_picker_state.is_open(),
                "Ctrl+/ must open slash command browser"
            );
            assert!(
                view.read(cx)
                    .prompt_input
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window),
                "Ctrl+/ must focus prompt_input so arrow/Enter/Tab keys work immediately"
            );
            assert_eq!(view.read(cx).slash_picker_state.selected, 0);

            // Press Down to select second command, then Enter to insert it
            window.press("down", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).slash_picker_state.selected,
                1,
                "Down must advance slash browser selection immediately after Ctrl+/"
            );
            let expected_second = {
                let st = &view.read(cx).slash_picker_state;
                st.filtered
                    .get(st.selected)
                    .map(|item| item.name.clone())
                    .expect("expected second slash command item")
            };

            window.press("enter", cx);
            window.render_frame(cx);
            assert!(!view.read(cx).slash_picker_state.is_open());
            assert!(
                view.read(cx)
                    .prompt_input
                    .read(cx)
                    .value()
                    .contains(&expected_second),
                "Enter must insert the selected slash command ({expected_second:?}), got {:?}",
                view.read(cx).prompt_input.read(cx).value()
            );

            // Also verify Ctrl+F -> Ctrl+/ -> Tab inserts the first command cleanly
            view.update(cx, |v, cx| {
                v.prompt_input
                    .update(cx, |inp, cx| inp.set_value("", window, cx));
            });
            window.render_frame(cx);
            window.press("ctrl-f", cx);
            window.render_frame(cx);
            assert!(view.read(cx).chat_state.find_open);

            window.press("ctrl-/", cx);
            window.render_frame(cx);
            assert!(!view.read(cx).chat_state.find_open);
            assert!(view.read(cx).slash_picker_state.is_open());
            let expected_first = {
                let st = &view.read(cx).slash_picker_state;
                st.filtered
                    .get(st.selected)
                    .map(|item| item.name.clone())
                    .expect("expected first slash command item")
            };

            window.press("tab", cx);
            window.render_frame(cx);
            assert!(!view.read(cx).slash_picker_state.is_open());
            assert!(
                view.read(cx)
                    .prompt_input
                    .read(cx)
                    .value()
                    .contains(&expected_first),
                "Tab after Ctrl+F -> Ctrl+/ must insert {expected_first:?}, got {:?}",
                view.read(cx).prompt_input.read(cx).value()
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn unmounted_button_focus_recovers_for_shortcuts_sidebar_settings_find_and_update_card(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        let s1 = make_session("btn-s1", AgentKind::Claude, "Alpha", false);
        let s2 = make_session("btn-s2", AgentKind::Codex, "Beta", false);

        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.sessions = vec![s1.clone(), s2.clone()];
                v.select_session(s1.clone(), cx);
                v.messages = vec![ChatMessage {
                    id: "m1".into(),
                    role: MessageRole::Assistant,
                    text: "alpha needle beta".into(),
                    ..Default::default()
                }];
                window.focus(&v.root_focus_handle, cx);
                cx.notify();
            });
            window.render_frame(cx);

            // 1a. F1 -> Shift+Tab (focuses close-shortcuts Button) -> F1 (toggle-close) -> Ctrl+K
            window.press("f1", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::Shortcuts);
            window.press("shift-tab", cx);
            window.render_frame(cx);
            assert!(
                !view.read_with(cx, |v, cx| v.is_focused_on_mounted_known_handle(window, cx)),
                "Shift+Tab in F1 Shortcuts must keep focus on opaque close-shortcuts Button on steady-state render"
            );
            window.press("f1", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::None);
            window.press("ctrl-k", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).active_modal,
                ActiveModal::Palette,
                "Ctrl+K must work after closing F1 Shortcuts while close-shortcuts Button was focused"
            );
            window.press("escape", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::None);

            // 1b. F1 -> Shift+Tab (focuses close-shortcuts Button) -> Enter (activates button) -> Ctrl+K
            window.press("f1", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::Shortcuts);
            window.press("shift-tab", cx);
            window.render_frame(cx);
            window.press("enter", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).active_modal,
                ActiveModal::None,
                "Enter on focused close-shortcuts Button must close Shortcuts modal"
            );
            window.press("ctrl-k", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).active_modal,
                ActiveModal::Palette,
                "Ctrl+K must work after activating close-shortcuts Button via Enter"
            );
            window.press("escape", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::None);

            // 2. SidebarMode::Open -> Tab, Tab (focuses sidebar Button) -> Ctrl+Shift+B -> Ctrl+K & reopen
            assert_eq!(view.read(cx).sidebar_state.mode, SidebarMode::Open);
            window.press("tab", cx);
            window.press("tab", cx);
            window.render_frame(cx);
            assert!(
                !view.read_with(cx, |v, cx| v.is_focused_on_mounted_known_handle(window, cx)),
                "Tab, Tab in Open sidebar must focus a sidebar Button"
            );
            window.press("ctrl-shift-b", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).sidebar_state.mode, SidebarMode::Collapsed);
            window.press("ctrl-k", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).active_modal,
                ActiveModal::Palette,
                "Ctrl+K must work after collapsing sidebar while sidebar Button was focused"
            );
            window.press("escape", cx);
            window.render_frame(cx);
            window.press("ctrl-shift-b", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).sidebar_state.mode, SidebarMode::Hidden);
            window.press("ctrl-shift-b", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).sidebar_state.mode,
                SidebarMode::Open,
                "Ctrl+Shift+B must reopen sidebar after button-focus recovery"
            );

            // 3. Ctrl+, (Settings) -> Tab onto Settings Button -> Ctrl+1 (JumpSession1 -> Chat) -> Ctrl+K
            window.press("ctrl-,", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Settings);
            window.press("shift-tab", cx);
            window.render_frame(cx);
            assert!(
                !view.read_with(cx, |v, cx| v.is_focused_on_mounted_known_handle(window, cx)),
                "Shift+Tab in Settings must focus a Settings Button"
            );
            window.press("ctrl-1", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Chat);
            window.press("ctrl-k", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).active_modal,
                ActiveModal::Palette,
                "Ctrl+K must work after Ctrl+1 switches Settings -> Chat while Settings Button was focused"
            );
            window.press("escape", cx);
            window.render_frame(cx);

            // 4. Ctrl+F (Find bar) -> Tab onto chat-find-close -> Enter -> Ctrl+K
            window.press("ctrl-f", cx);
            window.render_frame(cx);
            assert!(view.read(cx).chat_state.find_open);
            // Two header icon buttons precede the three Find controls.
            view.update(cx, |v, cx| {
                if let Some(fh) = v.chat_state.focus_handle.clone() {
                    window.focus(&fh, cx);
                }
            });
            window.render_frame(cx);
            for _ in 0..5 {
                window.press("tab", cx);
                window.render_frame(cx);
            }
            assert!(
                !view.read_with(cx, |v, cx| v.is_focused_on_mounted_known_handle(window, cx)),
                "Tab from chat-panel must focus chat-find-close Button"
            );
            window.press("enter", cx);
            window.render_frame(cx);
            assert!(
                !view.read(cx).chat_state.find_open,
                "Enter on focused chat-find-close Button must close the Find bar"
            );
            window.press("ctrl-k", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).active_modal,
                ActiveModal::Palette,
                "Ctrl+K must work after closing Find bar via focused chat-find-close Button"
            );
            window.press("escape", cx);
            window.render_frame(cx);

            // 5a. Update Card visible -> Tab onto update-later-btn -> Enter -> Ctrl+K
            let rel = ReleaseInfo {
                version: "9.8.7".into(),
                tag: "v9.8.7".into(),
                published_at: "2026-11-26T00:00:00Z".into(),
                notes: vec!["Faster switching".into()],
                asset: WindowsAssetInfo {
                    asset_name: "multibot-windows-x64.zip".into(),
                    url: "https://github.com/valentinyeo/multibot/releases/download/v9.8.7/multibot-windows-x64.zip".into(),
                    sha256: "a".repeat(64),
                    size_bytes: 1024,
                    multibot_exe_sha256: "b".repeat(64),
                    multibotd_linux_x86_64_sha256: "c".repeat(64),
                },
            };
            view.update(cx, |v, cx| {
                v.update_ui.set_update_state(UpdateState::Available(rel.clone()));
                v.focus_sidebar(window, cx);
                cx.notify();
            });
            window.render_frame(cx);
            assert!(view.read(cx).update_ui.is_card_visible(
                view.read(cx).settings.sidebar_mode,
                view.read(cx).sidebar_state.is_shown(),
                view.read(cx).settings.dismissed_update_version.as_deref(),
            ));
            // Reach the action through keyboard navigation, independent of chrome count.
            for _ in 0..12 {
                if window.find("update-later-btn").focused() == Some(true) { break; }
                window.press("tab", cx);
                window.render_frame(cx);
            }
            assert_eq!(window.find("update-later-btn").focused(), Some(true));
            window.press("enter", cx);
            window.render_frame(cx);
            assert!(
                !view.read(cx).update_ui.is_card_visible(
                    view.read(cx).settings.sidebar_mode,
                    view.read(cx).sidebar_state.is_shown(),
                    view.read(cx).settings.dismissed_update_version.as_deref(),
                ),
                "Enter on focused update-later-btn must dismiss the Update Card"
            );
            window.press("ctrl-k", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).active_modal,
                ActiveModal::Palette,
                "Ctrl+K must work after dismissing Update Card via focused update-later-btn"
            );
            window.press("escape", cx);
            window.render_frame(cx);

            // 5b. Structural FocusLayoutSignature recovery when Update Card unmounts without click handler
            let mut rel2 = rel;
            rel2.version = "9.9.9".into();
            rel2.tag = "v9.9.9".into();
            view.update(cx, |v, cx| {
                v.update_ui.set_update_state(UpdateState::Available(rel2));
                v.focus_sidebar(window, cx);
                cx.notify();
            });
            window.render_frame(cx);
            for _ in 0..12 {
                if window.find("update-later-btn").focused() == Some(true) { break; }
                window.press("tab", cx);
                window.render_frame(cx);
            }
            assert_eq!(window.find("update-later-btn").focused(), Some(true));
            // Background state change unmounts update card without any button click handler running
            view.update(cx, |v, cx| {
                v.update_ui.set_update_state(UpdateState::UpToDate);
                cx.notify();
            });
            window.render_frame(cx);
            window.press("ctrl-k", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).active_modal,
                ActiveModal::Palette,
                "Ctrl+K must recover via FocusLayoutSignature even when Update Card unmounts externally"
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn inbox_modal_dismissal_and_palette_commands_restore_inbox_focus(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        let mut s1 = make_session("inb-s1", AgentKind::Claude, "Inbox Alpha", false);
        s1.updated_at_ms = 2_000_000_000_000;
        s1.last_assistant_id = Some("a1".into());
        let mut s2 = make_session("inb-s2", AgentKind::Codex, "Inbox Beta", false);
        s2.updated_at_ms = 1_900_000_000_000;
        s2.last_assistant_id = Some("a2".into());

        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.sessions = vec![s1.clone(), s2.clone()];
                v.select_session(s1.clone(), cx);
                let now = now_unix_ms();
                let k1 = v.session_key_for(&s1);
                let k2 = v.session_key_for(&s2);
                // Mark both sessions as frequent AND unread so Frequent split & Unread-Only both have >=2 rows.
                v.sidebar_state.workspace_store.record_session_input(&k1, now);
                v.sidebar_state.workspace_store.record_session_input(&k2, now);
                v.sidebar_state.workspace_store.toggle_session_unread(&k1, &s1);
                v.sidebar_state.workspace_store.toggle_session_unread(&k2, &s2);
                window.focus(&v.root_focus_handle, cx);
                cx.notify();
            });
            window.render_frame(cx);

            // 1. Alt+I -> Ctrl+K -> Escape -> j advances row, Tab advances split, q closes Inbox
            window.press("alt-i", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);
            assert_eq!(view.read(cx).inbox_state.selected_row, 0);

            window.press("ctrl-k", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::Palette);

            window.press("escape", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::None);
            assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);

            window.press("j", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).inbox_state.selected_row,
                1,
                "j must advance inbox_state.selected_row after Alt+I -> Ctrl+K -> Escape"
            );

            let split_before = view.read(cx).inbox_state.active_split.clone();
            window.press("tab", cx);
            window.render_frame(cx);
            assert_ne!(
                view.read(cx).inbox_state.active_split,
                split_before,
                "Tab must advance active_split after Alt+I -> Ctrl+K -> Escape"
            );

            window.press("q", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).main_surface,
                MainSurface::Chat,
                "q must close Inbox after Alt+I -> Ctrl+K -> Escape"
            );

            // 2. Alt+I -> F1 -> Escape -> j advances row (and F1 -> Shift+Tab -> F1 -> j)
            window.press("alt-i", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);
            view.update(cx, |v, cx| {
                v.inbox_state.select_all_split();
                v.inbox_state.selected_row = 0;
                cx.notify();
            });
            window.render_frame(cx);

            window.press("f1", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::Shortcuts);

            window.press("escape", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::None);
            assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);

            window.press("j", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).inbox_state.selected_row,
                1,
                "j must advance inbox_state.selected_row after Alt+I -> F1 -> Escape"
            );

            // F1 -> Shift+Tab (focuses close-shortcuts) -> F1 -> j
            view.update(cx, |v, cx| {
                v.inbox_state.selected_row = 0;
                cx.notify();
            });
            window.render_frame(cx);
            window.press("f1", cx);
            window.render_frame(cx);
            window.press("shift-tab", cx);
            window.render_frame(cx);
            window.press("f1", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::None);
            assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);
            window.press("j", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).inbox_state.selected_row,
                1,
                "j must advance inbox_state.selected_row after Alt+I -> F1 -> Shift+Tab -> F1"
            );

            // 3a. Alt+I -> Ctrl+K -> execute non-surface-changing command ("Copy Chat ID") -> j advances row
            view.update(cx, |v, cx| {
                v.inbox_state.selected_row = 0;
                v.notice = None;
                cx.notify();
            });
            window.render_frame(cx);

            window.press("ctrl-k", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::Palette);
            view.update(cx, |v, cx| {
                v.palette_input
                    .update(cx, |inp, cx| inp.set_value("Copy Chat ID", window, cx));
                v.palette_state.selected_index = 0;
                cx.notify();
            });
            window.render_frame(cx);
            window.press("enter", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::None);
            assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);
            assert!(
                view.read(cx)
                    .notice
                    .as_deref()
                    .is_some_and(|n| n.starts_with("Session ID:")),
                "Expected Copy Chat ID command to execute, got notice={:?}",
                view.read(cx).notice
            );
            window.press("j", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).inbox_state.selected_row,
                1,
                "j must advance inbox_state.selected_row after executing Copy Chat ID from Inbox palette"
            );

            // 3b. Alt+I -> Ctrl+K -> execute "Toggle Unread Only" -> j advances row
            view.update(cx, |v, cx| {
                v.inbox_state.selected_row = 0;
                v.sidebar_state.unread_only = false;
                cx.notify();
            });
            window.render_frame(cx);

            window.press("ctrl-k", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::Palette);
            view.update(cx, |v, cx| {
                v.palette_input.update(cx, |inp, cx| {
                    inp.set_value("Toggle Unread-Only Filter", window, cx)
                });
                v.palette_state.selected_index = 0;
                cx.notify();
            });
            window.render_frame(cx);
            window.press("enter", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::None);
            assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);
            assert!(
                view.read(cx).sidebar_state.unread_only,
                "Expected Toggle Unread Only command to flip unread_only=true"
            );
            window.press("j", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).inbox_state.selected_row,
                1,
                "j must advance inbox_state.selected_row after executing Toggle Unread Only from Inbox palette"
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn test_round4_find_bar_tab_focused_buttons_enter_and_space_no_double_fire(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            let sess = make_session("s-find-btn", AgentKind::Claude, "Find Buttons", false);
            view.update(cx, |v, cx| {
                v.sessions = vec![sess.clone()];
                v.selected_session = Some(sess);
                v.messages = vec![
                    ChatMessage {
                        id: "m0".into(),
                        role: MessageRole::Assistant,
                        text: "First needle hit".into(),
                        timestamp_ms: Some(1_700_000_001_000),
                        blocks: Vec::new(),
                        todos: Vec::new(),
                        source_byte_range: None,
                        quoted_message_id: None,
                    },
                    ChatMessage {
                        id: "m1".into(),
                        role: MessageRole::Assistant,
                        text: "Second needle hit".into(),
                        timestamp_ms: Some(1_700_000_002_000),
                        blocks: Vec::new(),
                        todos: Vec::new(),
                        source_byte_range: None,
                        quoted_message_id: None,
                    },
                    ChatMessage {
                        id: "m2".into(),
                        role: MessageRole::Assistant,
                        text: "Third needle hit".into(),
                        timestamp_ms: Some(1_700_000_003_000),
                        blocks: Vec::new(),
                        todos: Vec::new(),
                        source_byte_range: None,
                        quoted_message_id: None,
                    },
                ];
                window.focus(&v.root_focus_handle, cx);
                cx.notify();
            });
            window.render_frame(cx);

            // Open Find with Ctrl+F and type "needle" via real keypresses
            window.press("ctrl-f", cx);
            window.render_frame(cx);
            assert!(view.read(cx).chat_state.find_open);
            for key in ["n", "e", "e", "d", "l", "e"] {
                window.press(key, cx);
                window.render_frame(cx);
            }
            assert_eq!(view.read(cx).chat_state.find_query, "needle");
            assert_eq!(view.read(cx).chat_state.find_matches, vec![0, 1, 2]);
            assert_eq!(view.read(cx).chat_state.find_current, 0);

            // Focus #chat-panel-root, then Tab 3 times to land on `chat-find-prev`
            // (2 top-bar buttons precede the 3 find-bar buttons: prev, next, close).
            view.update(cx, |v, cx| {
                if let Some(fh) = v.chat_state.focus_handle.clone() {
                    window.focus(&fh, cx);
                }
            });
            window.render_frame(cx);
            for _ in 0..3 {
                window.press("tab", cx);
                window.render_frame(cx);
            }
            assert!(
                !view.read_with(cx, |v, cx| v.is_focused_on_mounted_known_handle(window, cx)),
                "3rd Tab from chat-panel must focus chat-find-prev Button"
            );

            // Enter on `chat-find-prev` must step backward by exactly 1 (0 -> 2), NOT net 0!
            window.press("enter", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).chat_state.find_current,
                2,
                "Enter on Tab-focused chat-find-prev must step -1 (0 -> 2)"
            );
            assert_eq!(view.read(cx).chat_state.find_query, "needle");

            // Space on `chat-find-prev` must step backward by exactly 1 (2 -> 1) WITHOUT appending " "!
            window.press("space", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).chat_state.find_query,
                "needle",
                "Space on Tab-focused Find button must NOT corrupt find_query"
            );
            assert_eq!(
                view.read(cx).chat_state.find_current,
                1,
                "Space on Tab-focused chat-find-prev must step -1 (2 -> 1)"
            );

            // Tab 1 more time onto `chat-find-next`
            window.press("tab", cx);
            window.render_frame(cx);
            assert!(
                !view.read_with(cx, |v, cx| v.is_focused_on_mounted_known_handle(window, cx)),
                "6th Tab from chat-panel must focus chat-find-next Button"
            );

            // Enter on `chat-find-next` must step forward by exactly 1 (1 -> 2), NOT +2!
            window.press("enter", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).chat_state.find_current,
                2,
                "Enter on Tab-focused chat-find-next must step +1 (1 -> 2), not +2"
            );
            assert_eq!(view.read(cx).chat_state.find_query, "needle");

            // Space on `chat-find-next` must step forward by exactly 1 (2 -> 0) WITHOUT appending " "!
            window.press("space", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).chat_state.find_query,
                "needle",
                "Space on Tab-focused chat-find-next must NOT append space to find_query"
            );
            assert_eq!(
                view.read(cx).chat_state.find_current,
                0,
                "Space on Tab-focused chat-find-next must step +1 (2 -> 0)"
            );

            // Tab 1 more time onto `chat-find-close` -> Space closes Find cleanly
            window.press("tab", cx);
            window.render_frame(cx);
            window.press("space", cx);
            window.render_frame(cx);
            assert!(
                !view.read(cx).chat_state.find_open,
                "Space on Tab-focused chat-find-close must close the Find bar"
            );
            assert!(
                view.read(cx).chat_state.find_query.is_empty(),
                "Closing Find via Space must not leave trailing query characters"
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn test_round4_gfm_markdown_pipe_table_expand_click_focuses_modal_and_scrolls(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            let mut md_lines = vec![
                "| Metric | Value | Notes |".to_string(),
                "| :-- | :-- | :-- |".to_string(),
            ];
            for i in 1..=25 {
                md_lines.push(format!("| Row {i:02} | {i}00ms | Detail line {i:02} |"));
            }
            let table_md = md_lines.join("\n");
            let expected_hash = crate::chat_view::fnv1a_64(table_md.as_bytes());
            let expand_btn_id = format!("expand-md-table-0-{expected_hash:016x}");

            let sess = make_session("s-gfm-table", AgentKind::Claude, "GFM Table", false);
            view.update(cx, |v, cx| {
                v.sessions = vec![sess.clone()];
                v.selected_session = Some(sess);
                v.messages = vec![ChatMessage {
                    id: "m-table".into(),
                    role: MessageRole::Assistant,
                    text: table_md.clone(),
                    timestamp_ms: Some(1_700_000_005_000),
                    blocks: Vec::new(),
                    todos: Vec::new(),
                    source_byte_range: None,
                    quoted_message_id: None,
                }];
                window.focus(&v.root_focus_handle, cx);
                cx.notify();
            });
            window.render_frame(cx);
            // Scroll chat-scroll to bottom so the below-table `Expand` action row is in view
            view.update(cx, |v, cx| {
                v.chat_state.scroll_handle.scroll_to_bottom();
                cx.notify();
            });
            window.render_frame(cx);

            assert!(
                window.try_find(expand_btn_id.clone()).is_some(),
                "GFM pipe table must render Expand button `{expand_btn_id}`"
            );

            // Click the GFM pipe table's Expand button
            window.click(expand_btn_id, cx);
            window.render_frame(cx);

            assert!(
                view.read(cx).table_modal_state.is_open(),
                "Clicking GFM pipe table Expand button must open table_modal_state"
            );
            assert!(
                view.read(cx)
                    .table_modal_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window)),
                "Clicking GFM pipe table Expand button must focus table_modal_state.focus_handle"
            );

            // Verify keyboard scrolling works immediately without extra clicks
            let y_before = view.read(cx).table_modal_state.scroll.y;
            window.press("down", cx);
            window.render_frame(cx);
            assert!(
                view.read(cx).table_modal_state.scroll.y > y_before,
                "Pressing Down after clicking GFM pipe table Expand must scroll table_modal_state.scroll.y (before={y_before}, after={})",
                view.read(cx).table_modal_state.scroll.y
            );

            // Escape closes Table Modal cleanly
            window.press("escape", cx);
            window.render_frame(cx);
            assert!(!view.read(cx).table_modal_state.is_open());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn test_round4_table_modal_dismissed_by_global_surface_and_modal_actions(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        cx.update_window(handle, |_, window, cx| {
            let s1 = make_session("s-tm-1", AgentKind::Claude, "Alpha Table", false);
            let s2 = make_session("s-tm-2", AgentKind::Codex, "Beta Table", false);
            let sample_md = (1..=15).fold(
                String::from("| Col A | Col B |\n| --- | --- |\n"),
                |mut acc, i| {
                    acc.push_str(&format!("| A{i} | B{i} |\n"));
                    acc
                },
            );
            let sample_table =
                crate::table_modal::parse_table(&sample_md).expect("valid sample table");

            view.update(cx, |v, cx| {
                v.sessions = vec![s1.clone(), s2.clone()];
                v.selected_session = Some(s1);
                window.focus(&v.root_focus_handle, cx);
                cx.notify();
            });
            window.render_frame(cx);

            let open_and_focus_table = |window: &mut Window, cx: &mut gpui_kit::App| {
                view.update(cx, |v, cx| {
                    v.open_table_modal(sample_table.clone(), "Sample Table", cx);
                    v.focus_table_modal(window, cx);
                });
                window.render_frame(cx);
                assert!(view.read(cx).table_modal_state.is_open());
                assert!(
                    view.read(cx)
                        .table_modal_state
                        .focus_handle
                        .as_ref()
                        .is_some_and(|fh| fh.is_focused(window))
                );
            };

            // 1. Alt+I (OpenInbox) closes Table Modal, opens Inbox, and leaves Inbox j-nav interactive
            open_and_focus_table(window, cx);
            window.press("alt-i", cx);
            window.render_frame(cx);
            assert!(
                !view.read(cx).table_modal_state.is_open(),
                "Alt+I (OpenInbox) must close open Table Modal"
            );
            assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);
            assert!(
                view.read(cx)
                    .inbox_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window)),
                "Alt+I must focus inbox-root"
            );
            view.update(cx, |v, cx| {
                v.inbox_state.selected_row = 0;
                cx.notify();
            });
            window.render_frame(cx);
            window.press("j", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).inbox_state.selected_row,
                1,
                "Inbox j navigation must work immediately after Alt+I dismisses Table Modal"
            );

            // 2a. Ctrl+Shift+I (ToggleInbox) from Inbox with Table Modal open closes modal and returns to Chat
            open_and_focus_table(window, cx);
            window.press("ctrl-shift-i", cx);
            window.render_frame(cx);
            assert!(
                !view.read(cx).table_modal_state.is_open(),
                "Ctrl+Shift+I (ToggleInbox) must close open Table Modal"
            );
            assert_eq!(view.read(cx).main_surface, MainSurface::Chat);
            assert!(view.read(cx).prompt_input.read(cx).focus_handle(cx).is_focused(window));

            // 2b. Ctrl+Shift+I (ToggleInbox) from Chat with Table Modal open closes modal and opens interactive Inbox
            open_and_focus_table(window, cx);
            window.press("ctrl-shift-i", cx);
            window.render_frame(cx);
            assert!(
                !view.read(cx).table_modal_state.is_open(),
                "Ctrl+Shift+I from Chat must close open Table Modal"
            );
            assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);
            assert!(
                view.read(cx)
                    .inbox_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window))
            );
            view.update(cx, |v, cx| {
                v.inbox_state.select_all_split();
                v.inbox_state.selected_row = 0;
                cx.notify();
            });
            window.render_frame(cx);
            window.press("j", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).inbox_state.selected_row,
                1,
                "Inbox j navigation must work immediately after Ctrl+Shift+I dismisses Table Modal"
            );

            // 3. Ctrl+, (ToggleSettings) closes Table Modal, opens Settings, and keeps global shortcuts active
            open_and_focus_table(window, cx);
            window.press("ctrl-,", cx);
            window.render_frame(cx);
            assert!(
                !view.read(cx).table_modal_state.is_open(),
                "Ctrl+, (ToggleSettings) must close open Table Modal"
            );
            assert_eq!(view.read(cx).main_surface, MainSurface::Settings);
            assert!(view.read(cx).settings_focus_handle.is_focused(window));
            window.press("ctrl-k", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).active_modal,
                ActiveModal::Palette,
                "Ctrl+K must work immediately after Ctrl+, dismisses Table Modal"
            );
            window.press("escape", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::None);

            // 4. F1 (OpenShortcuts) closes Table Modal, opens Shortcuts on top, and Escape dismisses Shortcuts cleanly
            open_and_focus_table(window, cx);
            window.press("f1", cx);
            window.render_frame(cx);
            assert!(
                !view.read(cx).table_modal_state.is_open(),
                "F1 (OpenShortcuts) must close open Table Modal"
            );
            assert_eq!(view.read(cx).active_modal, ActiveModal::Shortcuts);
            assert!(view.read(cx).shortcuts_search_input.read(cx).focus_handle(cx).is_focused(window));
            window.press("escape", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::None);
            assert!(!view.read(cx).table_modal_state.is_open());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn test_round5_chat_overlays_do_not_block_inbox_nav_or_swallow_esc_on_inbox_and_settings(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        let s1 = make_session("r5-c1", AgentKind::Claude, "Alpha Chat", false);
        let s2 = make_session("r5-c2", AgentKind::Codex, "Beta Chat", false);
        let s3 = make_session("r5-c3", AgentKind::Antigravity, "Gamma Chat", false);

        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.sessions = vec![s1.clone(), s2.clone(), s3.clone()];
                // Keep Favorites non-empty so the Inbox bar has a second visible split to cycle
                let k1 = v.session_key_for(&s1);
                v.sidebar_state.workspace_store.toggle_favorite(&k1);
                v.selected_session = Some(s1.clone());
                v.messages = vec![
                    ChatMessage {
                        id: "m0".into(),
                        role: MessageRole::User,
                        text: "hello world".into(),
                        ..Default::default()
                    },
                    ChatMessage {
                        id: "m1".into(),
                        role: MessageRole::Assistant,
                        text: "find me in chat".into(),
                        ..Default::default()
                    },
                ];
                cx.notify();
            });
            window.render_frame(cx);

            // 1. In Chat, open Find via real Ctrl+F, plus set reply_quote and slash_picker
            window.press("ctrl-f", cx);
            window.render_frame(cx);
            assert!(view.read(cx).chat_state.find_open);
            view.update(cx, |v, cx| {
                v.chat_state.reply_quote = Some(crate::chat_view::ReplyQuote {
                    message_id: "m1".into(),
                    preview: "find me".into(),
                    full_text: "find me in chat".into(),
                });
                v.slash_picker_state.open_browser(AgentKind::Claude, "", 0);
                cx.notify();
            });
            window.render_frame(cx);
            assert!(view.read(cx).chat_state.find_open);
            assert!(view.read(cx).chat_state.reply_quote.is_some());
            assert!(view.read(cx).slash_picker_state.is_open());

            // 2. Switch to Inbox via real Alt+I
            window.press("alt-i", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);
            assert_eq!(view.read(cx).inbox_state.selected_row, 0);

            // 3. Verify j / k navigation works immediately on Inbox despite unmounted Chat overlays
            window.press("j", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).inbox_state.selected_row,
                1,
                "j must advance Inbox selection even when unmounted Chat overlays are set"
            );
            window.press("k", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).inbox_state.selected_row,
                0,
                "k must move Inbox selection back up"
            );

            // 4. Verify Tab cycles Inbox splits immediately
            let split_before = view.read(cx).inbox_state.active_split.clone();
            window.press("tab", cx);
            window.render_frame(cx);
            assert_ne!(
                view.read(cx).inbox_state.active_split,
                split_before,
                "Tab must cycle Inbox split despite unmounted Chat overlays"
            );

            // 5. Verify Ctrl+F + typing filters Inbox, 1st Esc clears Inbox filter (NOT unmounted Chat flags),
            //    and 2nd Esc exits Inbox back to Chat (NOT silently swallowing Esc on unmounted Chat flags)
            window.press("ctrl-f", cx);
            window.render_frame(cx);
            assert!(view.read(cx).inbox_state.filter_focused);
            window.press("b", cx);
            window.press("e", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).inbox_state.filter_query,
                "be",
                "Typing into focused Inbox filter must work with unmounted Chat overlays set"
            );

            window.press("escape", cx);
            window.render_frame(cx);
            assert!(
                view.read(cx).inbox_state.filter_query.is_empty()
                    && !view.read(cx).inbox_state.filter_focused,
                "1st Esc on Inbox must clear Inbox filter"
            );
            assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);
            assert!(
                view.read(cx).chat_state.find_open
                    && view.read(cx).chat_state.reply_quote.is_some()
                    && view.read(cx).slash_picker_state.is_open(),
                "Unmounted Chat overlays must NOT have been silently consumed by Esc on Inbox"
            );

            window.press("escape", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).main_surface,
                MainSurface::Chat,
                "2nd Esc on Inbox must return cleanly to Chat"
            );
            assert!(
                view.read(cx).chat_state.find_open
                    && view.read(cx).chat_state.reply_quote.is_some()
                    && view.read(cx).slash_picker_state.is_open(),
                "Unmounted Chat overlays must remain intact until returning to Chat"
            );

            // 6. From Chat with Find/reply_quote/slash_picker open, Ctrl+, opens Settings and
            //    a SINGLE Esc returns cleanly from Settings to Chat.
            window.press("ctrl-,", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Settings);

            window.press("escape", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).main_surface,
                MainSurface::Chat,
                "Single Esc on Settings must return to Chat rather than swallowing unmounted Chat flags"
            );
            assert!(view.read(cx).chat_state.find_open);
            assert!(view.read(cx).chat_state.reply_quote.is_some());
            assert!(view.read(cx).slash_picker_state.is_open());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn test_round5_inbox_rename_cleared_on_leaving_inbox_does_not_poison_chat_or_settings(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        let s1 = make_session("r5-ir1", AgentKind::Claude, "Alpha", false);
        let s2 = make_session("r5-ir2", AgentKind::Codex, "Beta", false);

        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                let _ = v
                    .sidebar_state
                    .workspace_store
                    .create_workspace("Second WS", None);
                v.sessions = vec![s1.clone(), s2.clone()];
                v.selected_session = Some(s1.clone());
                cx.notify();
            });
            window.render_frame(cx);
            let k1 = view.read(cx).session_key_for(&s1);

            // A. Leave Inbox via Ctrl+Shift+I (ToggleInbox) during F2 rename
            window.press("alt-i", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);

            window.press("f2", cx);
            window.render_frame(cx);
            assert!(
                view.read(cx).inbox_state.rename.is_some(),
                "F2 on Inbox must start inline Inbox rename"
            );

            window.press("ctrl-shift-i", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Chat);
            assert!(
                view.read(cx).inbox_state.rename.is_none(),
                "Leaving Inbox via Ctrl+Shift+I must clear inbox_state.rename"
            );

            // Verify Alt+U (MarkSessionUnread) works immediately in Chat
            let unread_before = view
                .read(cx)
                .sidebar_state
                .workspace_store
                .is_session_unread(&k1, &s1);
            window.press("alt-u", cx);
            window.render_frame(cx);
            assert_ne!(
                view.read(cx)
                    .sidebar_state
                    .workspace_store
                    .is_session_unread(&k1, &s1),
                unread_before,
                "Alt+U must work immediately in Chat after leaving Inbox during rename"
            );

            // Verify Alt+E (MoveSessionWaiting) and Alt+Z (UndoWaiting) work immediately in Chat
            window.press("alt-e", cx);
            window.render_frame(cx);
            assert!(
                view.read(cx)
                    .sidebar_state
                    .workspace_store
                    .waiting_sessions
                    .contains_key(&k1),
                "Alt+E must archive active session to Waiting in Chat"
            );
            window.press("alt-z", cx);
            window.render_frame(cx);
            assert!(
                !view
                    .read(cx)
                    .sidebar_state
                    .workspace_store
                    .waiting_sessions
                    .contains_key(&k1),
                "Alt+Z must undo Waiting in Chat"
            );

            // Verify Alt+J / Alt+K work immediately in Chat (cycling workspaces when step_order is disarmed)
            let ws_before = view.read(cx).sidebar_state.active_workspace_id.clone();
            window.press("alt-j", cx);
            window.render_frame(cx);
            assert_ne!(
                view.read(cx).sidebar_state.active_workspace_id,
                ws_before,
                "Alt+J must work immediately in Chat after leaving Inbox during rename"
            );
            window.press("alt-k", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).sidebar_state.active_workspace_id,
                ws_before,
                "Alt+K must step back immediately in Chat"
            );

            // Verify F2 starts sidebar rename immediately in Chat, and Esc cancels it
            window.press("f2", cx);
            window.render_frame(cx);
            assert!(
                view.read(cx).sidebar_state.rename.is_some(),
                "F2 must work immediately in Chat after leaving Inbox during rename"
            );
            window.press("escape", cx);
            window.render_frame(cx);
            assert!(view.read(cx).sidebar_state.rename.is_none());

            // B. Leave Inbox via Ctrl+, (ToggleSettings) during F2 rename -> Esc must return to Chat
            window.press("alt-i", cx);
            window.render_frame(cx);
            window.press("f2", cx);
            window.render_frame(cx);
            assert!(view.read(cx).inbox_state.rename.is_some());

            window.press("ctrl-,", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Settings);
            assert!(
                view.read(cx).inbox_state.rename.is_none(),
                "Leaving Inbox via Ctrl+, must clear inbox_state.rename"
            );

            window.press("escape", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).main_surface,
                MainSurface::Chat,
                "Esc in Settings must return cleanly to Chat"
            );

            // C. Leave Inbox via select_session during F2 rename -> inbox_state.rename cleared
            window.press("alt-i", cx);
            window.render_frame(cx);
            window.press("f2", cx);
            window.render_frame(cx);
            assert!(view.read(cx).inbox_state.rename.is_some());

            view.update(cx, |v, cx| {
                v.select_session(s2.clone(), cx);
            });
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Chat);
            assert!(
                view.read(cx).inbox_state.rename.is_none(),
                "Leaving Inbox via select_session must clear inbox_state.rename"
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn test_round5_collapsed_sidebar_top_pill_rename_does_not_hijack_inbox_or_settings(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        let s1 = make_session("r5-tp1", AgentKind::Claude, "Original Title", false);
        let s2 = make_session("r5-tp2", AgentKind::Codex, "Second Chat", false);

        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.sessions = vec![s1.clone(), s2.clone()];
                v.selected_session = Some(s1.clone());
                v.sidebar_state.mode = SidebarMode::Collapsed;
                v.settings.sidebar_mode = SidebarMode::Collapsed;
                cx.notify();
            });
            window.render_frame(cx);
            let k1 = view.read(cx).session_key_for(&s1);

            // 1. Collapsed sidebar on Chat: press F2 -> top-pill rename active
            window.press("f2", cx);
            window.render_frame(cx);
            assert!(
                view.read(cx).sidebar_state.rename.is_some(),
                "F2 in Collapsed sidebar on Chat must start top-pill rename"
            );

            // 2. Press Alt+I to open Inbox -> sidebar_state.rename MUST be cleared
            window.press("alt-i", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);
            assert!(
                view.read(cx).sidebar_state.rename.is_none(),
                "Switching from Chat to Inbox with Collapsed sidebar must clear top-pill rename"
            );

            // 3. Typing j/k navigates Inbox immediately and Enter opens selected row rather than
            //    committing a blind rename to the Chat session
            assert_eq!(view.read(cx).inbox_state.selected_row, 0);
            window.press("j", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).inbox_state.selected_row,
                1,
                "j must navigate Inbox immediately after leaving Collapsed-sidebar top-pill rename"
            );
            window.press("k", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).inbox_state.selected_row, 0);
            window.press("enter", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx)
                    .sidebar_state
                    .workspace_store
                    .effective_title(&k1, "Original Title"),
                "Original Title",
                "Pressing keys + Enter on Inbox must NOT silently rename the Chat session"
            );

            // 4. Hidden sidebar on Chat: press F2, then press Ctrl+, (Settings) -> cleared and typing+Enter does not rename
            view.update(cx, |v, cx| {
                v.select_session(s1.clone(), cx);
                v.sidebar_state.mode = SidebarMode::Hidden;
                v.settings.sidebar_mode = SidebarMode::Hidden;
                cx.notify();
            });
            window.render_frame(cx);

            window.press("f2", cx);
            window.render_frame(cx);
            assert!(view.read(cx).sidebar_state.rename.is_some());

            window.press("ctrl-,", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Settings);
            assert!(
                view.read(cx).sidebar_state.rename.is_none(),
                "Switching from Chat to Settings with Hidden sidebar must clear top-pill rename"
            );

            window.press("x", cx);
            window.press("enter", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx)
                    .sidebar_state
                    .workspace_store
                    .effective_title(&k1, "Original Title"),
                "Original Title",
                "Typing + Enter on Settings must NOT silently rename the Chat session"
            );

            // 5. Open sidebar preference + Inbox: the minimal Inbox hides the sidebar, so a sidebar
            //    rename must NOT mount there; cycling the preference leaves Inbox untouched.
            view.update(cx, |v, cx| {
                v.set_sidebar_mode_synced(SidebarMode::Open, cx);
                v.set_main_surface(MainSurface::Inbox);
                v.start_sidebar_rename_with_window(k1.clone(), window, cx);
            });
            window.render_frame(cx);
            assert!(
                view.read(cx).sidebar_state.rename.is_none(),
                "Open-sidebar rename must NOT mount while the sidebar is hidden in Inbox"
            );

            // Cycling the sidebar preference on Inbox must not resurrect an unmounted rename
            window.press("ctrl-shift-b", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).sidebar_state.mode, SidebarMode::Collapsed);
            assert!(view.read(cx).sidebar_state.rename.is_none());
            assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn test_round6_open_sidebar_rename_cleared_on_cross_surface_transitions(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        let s1 = make_session("r6-cs1", AgentKind::Claude, "Alpha", false);
        let s2 = make_session("r6-cs2", AgentKind::Codex, "Beta", false);

        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.sessions = vec![s1.clone(), s2.clone()];
                // Keep Favorites non-empty so the Inbox bar has a second visible split to cycle
                let k1 = v.session_key_for(&s1);
                v.sidebar_state.workspace_store.toggle_favorite(&k1);
                v.selected_session = Some(s1.clone());
                v.sidebar_state.mode = SidebarMode::Open;
                v.settings.sidebar_mode = SidebarMode::Open;
                cx.notify();
            });
            window.render_frame(cx);

            // 1. Chat (Open sidebar): F2 starts sidebar rename, then Alt+I switches to Inbox
            window.press("f2", cx);
            window.render_frame(cx);
            assert!(view.read(cx).sidebar_state.rename.is_some());
            assert!(view.read(cx).is_sidebar_rename_mounted());

            window.press("alt-i", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);
            assert!(
                view.read(cx).sidebar_state.rename.is_none(),
                "Alt+I from Chat with Open sidebar must clear sidebar_state.rename"
            );

            // Inbox j/k/Tab/F2 must work immediately (not deadlocked!)
            assert_eq!(view.read(cx).inbox_state.selected_row, 0);
            window.press("j", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).inbox_state.selected_row,
                1,
                "j must navigate Inbox immediately after Alt+I during Open-sidebar rename"
            );
            window.press("k", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).inbox_state.selected_row, 0);

            let split_before = view.read(cx).inbox_state.active_split.clone();
            window.press("tab", cx);
            window.render_frame(cx);
            assert_ne!(
                view.read(cx).inbox_state.active_split,
                split_before,
                "Tab must cycle Inbox splits immediately after Alt+I"
            );
            window.press("shift-tab", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).inbox_state.active_split, split_before);

            window.press("f2", cx);
            window.render_frame(cx);
            assert!(
                view.read(cx).inbox_state.rename.is_some(),
                "F2 on Inbox must start Inbox rename immediately after Alt+I"
            );
            window.press("escape", cx);
            window.render_frame(cx);
            assert!(view.read(cx).inbox_state.rename.is_none());
            assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);

            // Return to Chat via Escape
            window.press("escape", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Chat);

            // 2. Chat (Open sidebar): F2 + new_workspace_draft -> Ctrl+Shift+I switches to Inbox & clears both
            window.press("f2", cx);
            view.update(cx, |v, cx| {
                v.sidebar_state.new_workspace_draft = Some("ScratchWS".into());
                cx.notify();
            });
            window.render_frame(cx);
            assert!(view.read(cx).sidebar_state.rename.is_some());
            assert!(view.read(cx).sidebar_state.new_workspace_draft.is_some());

            window.press("ctrl-shift-i", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);
            assert!(
                view.read(cx).sidebar_state.rename.is_none(),
                "Ctrl+Shift+I must clear sidebar_state.rename on cross-surface transition"
            );
            assert!(
                view.read(cx).sidebar_state.new_workspace_draft.is_none(),
                "Ctrl+Shift+I must clear sidebar_state.new_workspace_draft on cross-surface transition"
            );
            window.press("j", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).inbox_state.selected_row, 1);

            // Return to Chat via Ctrl+Shift+I
            window.press("ctrl-shift-i", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Chat);

            // 3. Chat (Open sidebar): F2 -> Ctrl+, switches to Settings & clears rename; single Esc returns to Chat
            window.press("f2", cx);
            window.render_frame(cx);
            assert!(view.read(cx).sidebar_state.rename.is_some());

            window.press("ctrl-,", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Settings);
            assert!(
                view.read(cx).sidebar_state.rename.is_none(),
                "Ctrl+, must clear sidebar_state.rename on cross-surface transition"
            );

            window.press("escape", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).main_surface,
                MainSurface::Chat,
                "Single Esc on Settings must return directly to Chat"
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn test_round6_sidebar_rename_cleared_on_session_switch_delete_and_visibility_changes(
        cx: &mut TestAppContext,
    ) {
        let (handle1, view1) = open_test_view_with_entity(cx);
        let (handle2, view2) = open_test_view_with_entity(cx);

        let mut s1 = make_session("r6-sb1", AgentKind::Claude, "Alpha", false);
        s1.updated_at_ms = 20_000;
        let mut s2 = make_session("r6-sb2", AgentKind::Codex, "Beta", false);
        s2.updated_at_ms = 10_000;

        cx.update_window(handle1, |_, window, cx| {
            let (tx, _rx) = mpsc::unbounded_channel();
            view1.update(cx, |v, _| v.daemon_tx = Some(tx));
            view1.update(cx, |v, cx| {
                v.sessions = vec![s1.clone(), s2.clone()];
                v.selected_session = Some(s1.clone());
                v.sidebar_state.mode = SidebarMode::Collapsed;
                v.settings.sidebar_mode = SidebarMode::Collapsed;
                v.sync_sessions_to_shared(cx);
                cx.notify();
            });
            window.render_frame(cx);
            let k1 = view1.read(cx).session_key_for(&s1);
            let k2 = view1.read(cx).session_key_for(&s2);

            // --- Trace A1: Collapsed sidebar F2 -> Ctrl+2 switches to s2, clearing rename ---
            window.press("f2", cx);
            window.render_frame(cx);
            assert!(view1.read(cx).sidebar_state.rename.is_some());
            assert!(view1.read(cx).is_sidebar_rename_mounted());

            window.press("ctrl-2", cx);
            window.render_frame(cx);
            assert_eq!(
                view1
                    .read(cx)
                    .selected_session
                    .as_ref()
                    .map(|s| s.id.as_str()),
                Some("r6-sb2")
            );
            assert!(
                view1.read(cx).sidebar_state.rename.is_none(),
                "Ctrl+2 session switch must clear Collapsed-sidebar top-pill rename"
            );
            assert!(!view1.read(cx).is_sidebar_rename_mounted());

            // Typing + Enter after Ctrl+2 must NOT silently rename s1 or s2
            for ch in ["x", "y", "enter"] {
                window.press(ch, cx);
            }
            window.render_frame(cx);
            assert_eq!(
                view1
                    .read(cx)
                    .sidebar_state
                    .workspace_store
                    .effective_title(&k1, "Alpha"),
                "Alpha",
                "Typing + Enter after Ctrl+2 must NOT silently rename s1"
            );
            assert_eq!(
                view1
                    .read(cx)
                    .sidebar_state
                    .workspace_store
                    .effective_title(&k2, "Beta"),
                "Beta"
            );

            // --- Trace A2: Hidden sidebar F2 on sole session -> Ctrl+Shift+W deletes & clears rename ---
            view1.update(cx, |v, cx| {
                v.sessions = vec![s1.clone()];
                v.selected_session = Some(s1.clone());
                v.sidebar_state.mode = SidebarMode::Hidden;
                v.settings.sidebar_mode = SidebarMode::Hidden;
                cx.notify();
            });
            window.render_frame(cx);
            window.press("f2", cx);
            window.render_frame(cx);
            assert!(view1.read(cx).sidebar_state.rename.is_some());

            window.press("ctrl-shift-w", cx);
            window.render_frame(cx);
            assert!(view1.read(cx).sessions.iter().all(|s| s.archived));
            assert!(view1.read(cx).selected_session.is_none());
            assert!(
                view1.read(cx).sidebar_state.rename.is_none(),
                "Deleting sole session via Ctrl+Shift+W must clear top-pill rename"
            );
            for ch in ["z", "enter"] {
                window.press(ch, cx);
            }
            window.render_frame(cx);
            assert_eq!(
                view1
                    .read(cx)
                    .sidebar_state
                    .workspace_store
                    .effective_title(&k1, "Alpha"),
                "Alpha"
            );

            // --- Trace B: Open sidebar visibility mutations clear sidebar_state.rename ---
            view1.update(cx, |v, cx| {
                let _ = v
                    .sidebar_state
                    .workspace_store
                    .create_workspace("Second WS", None);
                v.sidebar_state.workspace_store.move_to_inbox(&k1);
                v.sessions = vec![s1.clone(), s2.clone()];
                v.selected_session = Some(s1.clone());
                v.sidebar_state.mode = SidebarMode::Open;
                v.settings.sidebar_mode = SidebarMode::Open;
                v.sidebar_state.workspace_store.toggle_group_for_session(
                    &k1,
                    multibot_core::workspaces::DEFAULT_WORKSPACE_ID,
                    None,
                    "Group One",
                );
                v.sync_sessions_to_shared(cx);
                v.sync_workspace_to_shared(cx);
                cx.notify();
            });
            window.render_frame(cx);

            // B1. Ctrl+Shift+U (ToggleUnreadOnly) clears Open-sidebar rename & keeps F2/Alt+J alive
            window.press("f2", cx);
            window.render_frame(cx);
            assert!(view1.read(cx).sidebar_state.rename.is_some());
            window.press("ctrl-shift-u", cx);
            window.render_frame(cx);
            assert!(
                view1.read(cx).sidebar_state.rename.is_none(),
                "Ctrl+Shift+U must clear Open-sidebar rename"
            );
            let ws_before = view1.read(cx).sidebar_state.active_workspace_id.clone();
            window.press("alt-j", cx);
            window.render_frame(cx);
            assert_ne!(
                view1.read(cx).sidebar_state.active_workspace_id,
                ws_before,
                "Alt+J must remain alive after Ctrl+Shift+U clears rename"
            );
            // Restore workspace + unread_only=false
            window.press("alt-k", cx);
            window.press("ctrl-shift-u", cx);
            window.render_frame(cx);

            // B2. Alt+1 (Starred workspace switch) clears Open-sidebar rename & keeps Alt+J alive
            window.press("f2", cx);
            window.render_frame(cx);
            assert!(view1.read(cx).sidebar_state.rename.is_some());
            window.press("alt-1", cx);
            window.render_frame(cx);
            assert_eq!(
                view1.read(cx).sidebar_state.active_workspace_id,
                multibot_core::workspaces::STARRED_WORKSPACE_ID
            );
            assert!(
                view1.read(cx).sidebar_state.rename.is_none(),
                "Alt+1 workspace switch must clear Open-sidebar rename"
            );
            // Pressing F2 while s1 is hidden in Starred workspace must NOT start an unmounted rename
            window.press("f2", cx);
            window.render_frame(cx);
            assert!(
                view1.read(cx).sidebar_state.rename.is_none(),
                "F2 must NOT start an unmounted rename when active session is not in Starred"
            );
            // Return to default workspace via Alt+2
            window.press("alt-2", cx);
            window.render_frame(cx);

            // B3. Ctrl+G (collapse active group) clears Open-sidebar rename & keeps Alt+J alive
            window.press("f2", cx);
            window.render_frame(cx);
            assert!(view1.read(cx).sidebar_state.rename.is_some());
            window.press("ctrl-g", cx);
            window.render_frame(cx);
            assert!(
                view1.read(cx).sidebar_state.rename.is_none(),
                "Ctrl+G group collapse must clear Open-sidebar rename"
            );
            window.press("f2", cx);
            window.render_frame(cx);
            assert!(
                view1.read(cx).sidebar_state.rename.is_none(),
                "F2 must NOT start an unmounted rename while active session's group is collapsed"
            );
            // Expand group again
            window.press("ctrl-g", cx);
            window.render_frame(cx);

            // B4. DeleteSession (Ctrl+Shift+W) clears Open-sidebar rename & F2 on remaining s2 works
            window.press("f2", cx);
            window.render_frame(cx);
            assert!(view1.read(cx).sidebar_state.rename.is_some());
            window.press("ctrl-shift-w", cx);
            window.render_frame(cx);
            assert!(
                view1.read(cx).sidebar_state.rename.is_none(),
                "Ctrl+Shift+W must clear Open-sidebar rename"
            );
            assert_eq!(
                view1
                    .read(cx)
                    .selected_session
                    .as_ref()
                    .map(|s| s.id.as_str()),
                Some("r6-sb2")
            );
            window.press("f2", cx);
            window.render_frame(cx);
            assert_eq!(
                view1
                    .read(cx)
                    .sidebar_state
                    .rename
                    .as_ref()
                    .map(|r| r.session_key.as_str()),
                Some(k2.as_str()),
                "F2 on remaining session s2 must work immediately after DeleteSession"
            );
        })
        .unwrap();

        // B5. Multi-window sync: Window 2 deletes s2 -> Window 1's active rename for s2 is cleared
        cx.update_window(handle2, |_, window, cx| {
            view2.update(cx, |v, cx| {
                v.sync_from_shared_store(cx);
                v.sessions.clear();
                v.sync_sessions_to_shared(cx);
                cx.notify();
            });
            window.render_frame(cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle1, |_, window, cx| {
            view1.update(cx, |v, cx| {
                v.sync_from_shared_store(cx);
            });
            window.render_frame(cx);
            assert!(
                view1.read(cx).sidebar_state.rename.is_none(),
                "Multi-window sync removing renamed session must clear sidebar_state.rename"
            );
            assert!(!view1.read(cx).is_sidebar_rename_mounted());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn test_round6_inbox_rename_cleared_on_filter_split_changes_and_palette_empty_inbox_safe(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        let mut s1 = make_session("r6-in1", AgentKind::Claude, "Claude Alpha", false);
        s1.updated_at_ms = 30_000;
        let mut s2 = make_session("r6-in2", AgentKind::Codex, "Codex Beta", false);
        s2.updated_at_ms = 20_000;
        let mut s3 = make_session("r6-in3", AgentKind::Codex, "Codex Gamma", false);
        s3.updated_at_ms = 10_000;

        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.sessions = vec![s1.clone(), s2.clone(), s3.clone()];
                v.selected_session = Some(s1.clone());
                cx.notify();
            });
            window.render_frame(cx);
            let k1 = view.read(cx).session_key_for(&s1);
            let k2 = view.read(cx).session_key_for(&s2);
            // Keep Favorites non-empty so its split pill is visible and clickable
            // (0-count splits are hidden from the Inbox split bar)
            view.update(cx, |v, cx| {
                v.sidebar_state.workspace_store.toggle_favorite(&k1);
                cx.notify();
            });
            window.render_frame(cx);

            // 1. Enter Inbox, start F2 rename on Claude Alpha, then click a workspace split
            window.press("alt-i", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);
            window.press("f2", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx)
                    .inbox_state
                    .rename
                    .as_ref()
                    .map(|r| r.session_key.as_str()),
                Some(k1.as_str())
            );

            window.click("inbox-tab-all", cx);
            window.render_frame(cx);
            assert!(
                view.read(cx).inbox_state.rename.is_none(),
                "Clicking #inbox-tab-all must clear inbox_state.rename"
            );

            // j/k/Enter must navigate & open Inbox rows, NOT blindly rename the hidden session!
            assert_eq!(view.read(cx).inbox_state.selected_row, 0);
            window.press("j", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).inbox_state.selected_row,
                1,
                "j must navigate Inbox rows immediately"
            );
            window.press("k", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).inbox_state.selected_row, 0);
            window.press("enter", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Chat);
            assert_eq!(
                view.read(cx).selected_session.as_ref().map(|s| s.id.as_str()),
                Some("r6-in1"),
                "Enter must open the selected Inbox row r6-in1"
            );
            assert_eq!(
                view.read(cx)
                    .sidebar_state
                    .workspace_store
                    .effective_title(&k1, "Claude Alpha"),
                "Claude Alpha",
                "Hidden sessions must NOT be silently renamed by j/k/Enter"
            );

            // 2. Inbox F2 -> Ctrl+Shift+U clears inbox_state.rename
            window.press("alt-i", cx);
            window.render_frame(cx);
            window.press("f2", cx);
            window.render_frame(cx);
            assert!(view.read(cx).inbox_state.rename.is_some());
            window.press("ctrl-shift-u", cx);
            window.render_frame(cx);
            assert!(
                view.read(cx).inbox_state.rename.is_none(),
                "Ctrl+Shift+U must clear inbox_state.rename"
            );
            window.press("ctrl-shift-u", cx);
            window.render_frame(cx);
            window.press("f2", cx);
            window.render_frame(cx);
            assert!(view.read(cx).inbox_state.rename.is_some());

            // 3. Inbox F2 -> click #inbox-split-favorites clears inbox_state.rename
            window.click("inbox-split-favorites", cx);
            window.render_frame(cx);
            assert!(
                view.read(cx).inbox_state.rename.is_none(),
                "Clicking #inbox-split-favorites must clear inbox_state.rename"
            );

            // 4. Empty/filtered Inbox (filter matches no session) + Collapsed sidebar:
            //    Ctrl+K -> Rename session must NOT open an unmounted inbox_state.rename or sidebar_state.rename!
            view.update(cx, |v, cx| {
                v.inbox_state.filter_query = "zzz-no-such-session".into();
                v.set_sidebar_mode_synced(SidebarMode::Collapsed, cx);
            });
            window.render_frame(cx);
            assert!(view.read(cx).compute_inbox_rows(now_unix_ms()).is_empty());

            window.press("ctrl-k", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::Palette);
            view.update(cx, |v, cx| {
                v.palette_input.update(cx, |inp, cx| {
                    inp.set_value("rename session", window, cx);
                });
            });
            window.render_frame(cx);
            window.press("enter", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::None);
            assert!(
                view.read(cx).inbox_state.rename.is_none(),
                "Ctrl+K -> Rename session on empty Inbox must NOT open an unmounted inbox_state.rename"
            );
            assert!(
                view.read(cx).sidebar_state.rename.is_none(),
                "Ctrl+K -> Rename session on empty Inbox with Collapsed sidebar must NOT open an unmounted sidebar_state.rename"
            );

            // 6. Explicit tab-context pal-cmd-rename for s2 while on empty Favorites Inbox (Collapsed sidebar):
            //    must NOT silently rename a wrong row or open unmounted Inbox rename; switches to Chat
            //    for s2 and visibly mounts top-pill rename for s2!
            view.update(cx, |v, cx| {
                v.open_palette_tab_context(k2.clone(), window, cx);
                v.palette_input.update(cx, |inp, cx| {
                    inp.set_value("rename session", window, cx);
                });
            });
            window.render_frame(cx);
            window.press("enter", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Chat);
            assert_eq!(
                view.read(cx).selected_session.as_ref().map(|s| s.id.as_str()),
                Some("r6-in2")
            );
            assert_eq!(
                view.read(cx)
                    .sidebar_state
                    .rename
                    .as_ref()
                    .map(|r| r.session_key.as_str()),
                Some(k2.as_str())
            );
            assert!(view.read(cx).is_sidebar_rename_mounted());
            assert!(view.read(cx).inbox_state.rename.is_none());

            // And on non-empty Inbox (All split, row 0 = s1), explicit tab-context rename for s2
            // renames s2 (NOT row 0 s1) inside Inbox with mounted & focused input!
            view.update(cx, |v, _| v.inbox_state.filter_query.clear());
            window.press("escape", cx);
            window.press("alt-i", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).inbox_state.selected_row, 0);
            view.update(cx, |v, cx| {
                v.open_palette_tab_context(k2.clone(), window, cx);
                v.palette_input.update(cx, |inp, cx| {
                    inp.set_value("rename session", window, cx);
                });
            });
            window.render_frame(cx);
            window.press("enter", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);
            assert_eq!(
                view.read(cx)
                    .inbox_state
                    .rename
                    .as_ref()
                    .map(|r| r.session_key.as_str()),
                Some(k2.as_str()),
                "Tab-context pal-cmd-rename for s2 on Inbox must rename s2, not row 0 s1"
            );
            assert!(view.read(cx).is_inbox_rename_mounted());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn f2_rename_selects_all_and_typing_replaces_name(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);
        let session = make_session("rename-select-all", AgentKind::Claude, "Old 日 name", false);

        cx.update_window(handle, |_, window, cx| {
            for (surface, mode) in [
                (MainSurface::Chat, SidebarMode::Open),
                (MainSurface::Chat, SidebarMode::Collapsed),
                (MainSurface::Chat, SidebarMode::Hidden),
                (MainSurface::Inbox, SidebarMode::Open),
            ] {
                view.update(cx, |v, cx| {
                    v.sessions = vec![session.clone()];
                    v.selected_session = Some(session.clone());
                    v.main_surface = surface;
                    v.sidebar_state.mode = mode;
                    v.settings.sidebar_mode = mode;
                    v.sidebar_state.workspace_store = WorkspaceStore::new();
                    v.inbox_state.active_split = multibot_core::workspaces::InboxSplit::All;
                    v.inbox_state.selected_row = 0;
                    v.prompt_input.update(cx, |input, cx| {
                        input.set_value("composer untouched", window, cx);
                        input.focus(window, cx);
                    });
                    cx.notify();
                });
                window.render_frame(cx);
                window.press("f2", cx);
                window.render_frame(cx);

                let v = view.read(cx);
                let (rename, focused) = if surface == MainSurface::Inbox {
                    (
                        v.inbox_state.rename.as_ref().unwrap(),
                        v.inbox_state
                            .focus_handle
                            .as_ref()
                            .unwrap()
                            .is_focused(window),
                    )
                } else {
                    (
                        v.sidebar_state.rename.as_ref().unwrap(),
                        if mode == SidebarMode::Open {
                            v.sidebar_state
                                .focus_handle
                                .as_ref()
                                .unwrap()
                                .is_focused(window)
                        } else {
                            v.root_focus_handle.is_focused(window)
                        },
                    )
                };
                assert!(focused, "F2 must focus the mounted rename editor");
                assert_eq!(rename.draft, session.title);
                assert!(rename.all_selected, "F2 must select the entire name");

                window.press("n", cx);
                window.press("e", cx);
                window.press("w", cx);
                window.press("enter", cx);
                window.render_frame(cx);
                let v = view.read(cx);
                let key = v.session_key_for(&session);
                assert_eq!(
                    v.sidebar_state
                        .workspace_store
                        .effective_title(&key, &session.title),
                    "new",
                    "Typing after F2 must replace the entire name on {surface:?}/{mode:?}"
                );
                assert_eq!(
                    v.prompt_input.read(cx).value().as_ref(),
                    "composer untouched"
                );
                assert!(v.sidebar_state.rename.is_none());
                assert!(v.inbox_state.rename.is_none());
            }
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn test_round6b_focus_restored_to_sidebar_rename_after_modals_and_palette_expand(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        let mut s1 = make_session("r6b-s1", AgentKind::Claude, "Alpha", false);
        s1.updated_at_ms = 20_000;
        let mut s2 = make_session("r6b-s2", AgentKind::Codex, "Beta", false);
        s2.updated_at_ms = 10_000;

        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.sessions = vec![s1.clone(), s2.clone()];
                v.selected_session = Some(s1.clone());
                v.sidebar_state.mode = SidebarMode::Open;
                v.settings.sidebar_mode = SidebarMode::Open;
                cx.notify();
            });
            window.render_frame(cx);
            let k1 = view.read(cx).session_key_for(&s1);
            let k2 = view.read(cx).session_key_for(&s2);

            // --- Scenario 1: Inbox is minimal (no sidebar), so F2 edits the Inbox row inline and
            //     Ctrl+K / F1 -> Esc restores #inbox-root focus so typing + Enter commits cleanly ---
            window.press("alt-i", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);

            window.press("f2", cx);
            window.render_frame(cx);
            assert!(view.read(cx).is_inbox_rename_mounted());
            assert!(
                !view.read(cx).is_sidebar_rename_mounted(),
                "Open-sidebar rename must never mount in the minimal Inbox"
            );
            assert!(
                view.read(cx)
                    .inbox_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window)),
                "#inbox-root must be focused when the Inbox rename starts"
            );

            // Ctrl+K -> Esc restores #inbox-root focus (no Inbox/sidebar double-deadlock)
            window.press("ctrl-k", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::Palette);
            window.press("escape", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::None);
            assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);
            assert!(view.read(cx).is_inbox_rename_mounted());
            assert!(
                view.read(cx)
                    .inbox_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window)),
                "Esc from Ctrl+K on Inbox must restore #inbox-root focus"
            );

            // F1 -> Esc also restores #inbox-root focus
            window.press("f1", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::Shortcuts);
            window.press("escape", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::None);
            assert!(view.read(cx).is_inbox_rename_mounted());
            assert!(
                view.read(cx)
                    .inbox_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window)),
                "Esc from F1 on Inbox must restore #inbox-root focus"
            );

            // Typing + Enter commits the Inbox rename cleanly
            window.press("x", cx);
            window.press("enter", cx);
            window.render_frame(cx);
            assert!(
                view.read(cx).inbox_state.rename.is_none(),
                "Enter must commit the restored Inbox rename"
            );
            assert_eq!(
                view.read(cx)
                    .sidebar_state
                    .workspace_store
                    .effective_title(&k1, "Alpha"),
                "x",
                "Typing + Enter after Ctrl+K/F1 -> Esc on Inbox must commit renamed title"
            );
            assert!(
                view.read(cx)
                    .inbox_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window)),
                "Focus must return to #inbox-root after the Inbox rename commits"
            );

            // Inbox j/k works immediately after commit
            assert_eq!(view.read(cx).inbox_state.selected_row, 0);
            window.press("j", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).inbox_state.selected_row,
                1,
                "Inbox j must work immediately after the Inbox rename commits"
            );
            window.press("k", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).inbox_state.selected_row,
                0,
                "Inbox k must work immediately after the Inbox rename commits"
            );

            // --- Scenario 2: Chat + Open-sidebar rename -> Ctrl+K / F1 -> Esc restores
            //     #sidebar-open focus so typing + Enter commits cleanly ---
            window.press("escape", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Chat);
            view.update(cx, |v, cx| {
                v.select_session(s2.clone(), cx);
            });
            window.render_frame(cx);

            window.press("f2", cx);
            window.render_frame(cx);
            assert!(view.read(cx).is_sidebar_rename_mounted());
            assert!(
                view.read(cx)
                    .sidebar_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window))
            );

            window.press("ctrl-k", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::Palette);
            window.press("escape", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::None);
            assert!(
                view.read(cx)
                    .sidebar_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window)),
                "Esc from Ctrl+K on Chat must restore focus to #sidebar-open for active rename"
            );

            window.press("f1", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::Shortcuts);
            window.press("escape", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::None);
            assert!(
                view.read(cx)
                    .sidebar_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window)),
                "Esc from F1 on Chat must restore focus to #sidebar-open for active rename"
            );

            window.press("y", cx);
            window.press("enter", cx);
            window.render_frame(cx);
            assert!(view.read(cx).sidebar_state.rename.is_none());
            assert_eq!(
                view.read(cx)
                    .sidebar_state
                    .workspace_store
                    .effective_title(&k2, "Beta"),
                "y",
                "Typing + Enter after Ctrl+K/F1 -> Esc on Chat must commit renamed title"
            );

            // --- Scenario 3: Collapsed-sidebar rename on Chat -> Ctrl+K -> Sidebar: Expand
            //     (pal-cmd-sidebar-open) focuses #sidebar-open so typing + Enter commits cleanly ---
            view.update(cx, |v, cx| {
                v.set_sidebar_mode_synced(SidebarMode::Collapsed, cx);
            });
            window.render_frame(cx);
            assert_eq!(view.read(cx).sidebar_state.mode, SidebarMode::Collapsed);

            window.press("f2", cx);
            window.render_frame(cx);
            assert!(view.read(cx).is_sidebar_rename_mounted());

            window.press("ctrl-k", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::Palette);
            view.update(cx, |v, cx| {
                v.palette_input.update(cx, |inp, cx| {
                    inp.set_value("Expand Sidebar", window, cx);
                });
            });
            window.render_frame(cx);
            window.press("enter", cx);
            window.render_frame(cx);

            assert_eq!(view.read(cx).active_modal, ActiveModal::None);
            assert_eq!(view.read(cx).sidebar_state.mode, SidebarMode::Open);
            assert!(
                view.read(cx).is_sidebar_rename_mounted(),
                "Collapsed-sidebar rename must remain mounted after Sidebar: Expand"
            );
            assert!(
                view.read(cx)
                    .sidebar_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window)),
                "Ctrl+K -> Sidebar: Expand during rename must focus #sidebar-open"
            );

            window.press("z", cx);
            window.press("enter", cx);
            window.render_frame(cx);
            assert!(view.read(cx).sidebar_state.rename.is_none());
            assert_eq!(
                view.read(cx)
                    .sidebar_state
                    .workspace_store
                    .effective_title(&k2, "Beta"),
                "z",
                "Typing + Enter after Ctrl+K -> Sidebar: Expand must commit renamed title"
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn test_round6b_chat_find_and_browse_commands_clear_sidebar_rename_cleanly(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        let s1 = make_session("r6b-f1", AgentKind::Claude, "Alpha", false);

        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.sessions = vec![s1.clone()];
                v.selected_session = Some(s1.clone());
                v.messages = vec![ChatMessage {
                    id: "m1".into(),
                    role: MessageRole::Assistant,
                    text: "Hello searchable chat message".into(),
                    ..Default::default()
                }];
                v.sidebar_state.mode = SidebarMode::Open;
                v.settings.sidebar_mode = SidebarMode::Open;
                cx.notify();
            });
            window.render_frame(cx);
            let k1 = view.read(cx).session_key_for(&s1);

            // 1. Open-sidebar rename on Chat -> Ctrl+F (FindInChat) clears rename cleanly
            window.press("f2", cx);
            window.render_frame(cx);
            assert!(view.read(cx).is_sidebar_rename_mounted());

            window.press("ctrl-f", cx);
            window.render_frame(cx);
            assert!(view.read(cx).chat_state.find_open);
            assert!(
                view.read(cx).sidebar_state.rename.is_none(),
                "Ctrl+F on Chat must clear Open-sidebar rename so it cannot be left stranded unfocused"
            );

            // Esc closes Find and leaves Chat interactive (F2 starts a fresh rename immediately)
            window.press("escape", cx);
            window.render_frame(cx);
            assert!(!view.read(cx).chat_state.find_open);
            assert!(view.read(cx).sidebar_state.rename.is_none());

            window.press("f2", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx)
                    .sidebar_state
                    .rename
                    .as_ref()
                    .map(|r| r.session_key.as_str()),
                Some(k1.as_str()),
                "F2 must work immediately after Ctrl+F -> Esc"
            );
            assert!(
                view.read(cx)
                    .sidebar_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window))
            );

            // 2. Open-sidebar rename on Chat -> Ctrl+/ (BrowseCommands) clears rename cleanly
            window.press("ctrl-/", cx);
            window.render_frame(cx);
            assert!(view.read(cx).slash_picker_state.is_open());
            assert!(
                view.read(cx).sidebar_state.rename.is_none(),
                "Ctrl+/ on Chat must clear Open-sidebar rename so it cannot be left stranded unfocused"
            );

            // Esc closes slash picker and leaves Chat interactive (F2 starts a fresh rename immediately)
            window.press("escape", cx);
            window.render_frame(cx);
            assert!(!view.read(cx).slash_picker_state.is_open());
            assert!(view.read(cx).sidebar_state.rename.is_none());

            window.press("f2", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx)
                    .sidebar_state
                    .rename
                    .as_ref()
                    .map(|r| r.session_key.as_str()),
                Some(k1.as_str()),
                "F2 must work immediately after Ctrl+/ -> Esc"
            );

            // 3. Focusing prompt_input directly (e.g., clicking composer) also clears rename cleanly
            view.update(cx, |v, cx| {
                v.prompt_input.update(cx, |inp, cx| {
                    inp.focus(window, cx);
                });
            });
            window.render_frame(cx);
            assert!(
                view.read(cx).sidebar_state.rename.is_none(),
                "Focusing prompt_input must clear Open-sidebar rename"
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn test_round7_minimal_inbox_hides_sidebar_and_restores_it_on_leave(cx: &mut TestAppContext) {
        let (handle, view) = open_test_view_with_entity(cx);

        let mut s1 = make_session("r7-inb-1", AgentKind::Claude, "Alpha", false);
        s1.updated_at_ms = 30_000;
        s1.last_assistant_id = Some("a1".into());
        let mut s2 = make_session("r7-inb-2", AgentKind::Codex, "Beta", false);
        s2.updated_at_ms = 20_000;
        s2.last_assistant_id = Some("a2".into());

        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                let work_id = v
                    .sidebar_state
                    .workspace_store
                    .create_workspace("Work", None)
                    .expect("Work workspace must be created")
                    .id;
                v.sessions = vec![s1.clone(), s2.clone()];
                // One session in `Work` keeps that split non-empty (0-count splits are hidden)
                let k2 = v.session_key_for(&s2);
                v.sidebar_state
                    .workspace_store
                    .assign_session_workspace(&k2, &work_id);
                v.selected_session = Some(s1.clone());
                v.sidebar_state.mode = SidebarMode::Open;
                v.settings.sidebar_mode = SidebarMode::Open;
                v.sync_sessions_to_shared(cx);
                v.sync_workspace_to_shared(cx);
                cx.notify();
            });
            window.render_frame(cx);
            assert!(
                window.try_find("sidebar-open").is_some(),
                "Chat must render the Open sidebar"
            );

            // --- Alt+I hides the sidebar and the whole floating header, without mutating
            //     the stored sidebar preference ---
            window.press("alt-i", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);
            assert!(
                window.try_find("sidebar-open").is_none(),
                "Minimal Inbox must NOT render #sidebar-open"
            );
            assert!(
                window.try_find("sidebar-rail").is_none(),
                "Minimal Inbox must NOT render the collapsed sidebar rail"
            );
            assert!(!view.read(cx).sidebar_rendered());
            assert!(
                view.read(cx).sidebar_state.mode == SidebarMode::Open,
                "Hiding the sidebar in Inbox must NOT mutate the user's sidebar preference"
            );
            assert!(
                view.read(cx).settings.sidebar_mode == SidebarMode::Open,
                "Hiding the sidebar in Inbox must NOT mutate the persisted sidebar_mode"
            );
            for removed in ["header-title-pill", "toggle-inbox", "chat-menu"] {
                assert!(
                    window.try_find(removed).is_none(),
                    "Minimal Inbox must not render {removed}"
                );
            }

            // Keyboard navigation stays intact: j / k / Tab / Shift+Tab
            assert_eq!(view.read(cx).inbox_state.selected_row, 0);
            window.press("j", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).inbox_state.selected_row, 1);
            window.press("k", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).inbox_state.selected_row, 0);
            let split_before_tab = view.read(cx).inbox_state.active_split.clone();
            window.press("tab", cx);
            window.render_frame(cx);
            assert_ne!(
                view.read(cx).inbox_state.active_split,
                split_before_tab,
                "Tab must cycle Inbox splits in the minimal layout"
            );
            window.press("shift-tab", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).inbox_state.active_split, split_before_tab);

            // Enter still opens the selected row
            window.press("enter", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Chat);
            assert_eq!(
                view.read(cx)
                    .selected_session
                    .as_ref()
                    .map(|s| s.id.as_str()),
                Some("r7-inb-1"),
                "Enter must open the selected Inbox row"
            );
            assert!(
                window.try_find("sidebar-open").is_some(),
                "Leaving Inbox must restore the previous sidebar visibility"
            );
            assert!(view.read(cx).sidebar_rendered());

            // Alt+I again, Esc leaves Inbox and restores the sidebar
            window.press("alt-i", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);
            assert!(window.try_find("sidebar-open").is_none());
            window.press("escape", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Chat);
            assert!(
                window.try_find("sidebar-open").is_some(),
                "Esc must restore the previous sidebar visibility"
            );

            // A Collapsed preference is restored as Collapsed, not Open
            view.update(cx, |v, cx| {
                v.set_sidebar_mode_synced(SidebarMode::Collapsed, cx);
            });
            window.render_frame(cx);
            assert!(window.try_find("sidebar-rail").is_some());
            window.press("alt-i", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Inbox);
            assert!(window.try_find("sidebar-rail").is_none());
            window.press("ctrl-shift-i", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).main_surface, MainSurface::Chat);
            assert!(
                window.try_find("sidebar-rail").is_some(),
                "Leaving Inbox must restore the Collapsed sidebar rail"
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn test_round7_chat_find_slash_picker_and_message_nav_restore_focus_after_modals(
        cx: &mut TestAppContext,
    ) {
        let (handle, view) = open_test_view_with_entity(cx);
        let s1 = make_session("r7-chat-1", AgentKind::Claude, "Chat Focus Test", false);

        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.sessions = vec![s1.clone()];
                v.selected_session = Some(s1.clone());
                v.messages = vec![
                    ChatMessage {
                        id: "m0".into(),
                        role: MessageRole::Assistant,
                        text: "First needle message".into(),
                        ..Default::default()
                    },
                    ChatMessage {
                        id: "m1".into(),
                        role: MessageRole::Assistant,
                        text: "Second needle message".into(),
                        ..Default::default()
                    },
                ];
                v.sidebar_state.mode = SidebarMode::Open;
                v.settings.sidebar_mode = SidebarMode::Open;
                cx.notify();
            });
            window.render_frame(cx);

            // =================================================================
            // Trace 2A: Chat Find (`Ctrl+F`) survives Ctrl+K, F1, Table Modal,
            //           and Update Card button clicks with full live keyboard input
            // =================================================================
            window.press("ctrl-f", cx);
            window.render_frame(cx);
            assert!(view.read(cx).chat_state.find_open);
            for ch in ["n", "e", "e", "d", "l", "e"] {
                window.press(ch, cx);
            }
            window.render_frame(cx);
            assert_eq!(view.read(cx).chat_state.find_query, "needle");
            assert_eq!(view.read(cx).chat_state.find_matches.len(), 2);
            assert_eq!(view.read(cx).chat_state.find_current, 0);

            // 2A-1: Ctrl+K -> Esc restores Find focus
            window.press("ctrl-k", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::Palette);
            window.press("escape", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::None);
            assert!(view.read(cx).chat_state.find_open);
            assert!(
                view.read(cx)
                    .chat_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window)),
                "Ctrl+K -> Esc must restore #chat-panel-root focus while Chat Find is open"
            );
            window.press("enter", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).chat_state.find_current,
                1,
                "Enter must advance Find match immediately after Ctrl+K -> Esc"
            );
            window.press("f3", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).chat_state.find_current,
                0,
                "F3 must advance/wrap Find match immediately after Ctrl+K -> Esc"
            );
            window.press("x", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).chat_state.find_query, "needlex");
            window.press("backspace", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).chat_state.find_query, "needle");

            // 2A-2: F1 (`ActiveModal::Shortcuts`) blocks Find input while open, restores on Esc
            window.press("f1", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::Shortcuts);
            window.press("z", cx);
            window.press("backspace", cx);
            window.press("enter", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).chat_state.find_query,
                "needle",
                "F1 Shortcuts must block Find typing/Backspace while open"
            );
            assert_eq!(
                view.read(cx).chat_state.find_current,
                0,
                "F1 Shortcuts must block Find Enter while open"
            );
            window.press("escape", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::None);
            assert!(view.read(cx).chat_state.find_open);
            assert!(
                view.read(cx)
                    .chat_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window)),
                "F1 -> Esc must restore #chat-panel-root focus while Chat Find is open"
            );
            window.press("enter", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).chat_state.find_current,
                1,
                "Find Enter must work immediately after F1 -> Esc"
            );

            // 2A-3: Table Modal -> Esc restores Find focus
            let table = crate::table_modal::parse_table("| A | B |\n|---|---|\n| 1 | 2 |")
                .expect("valid table");
            view.update(cx, |v, cx| {
                v.open_table_modal(table.clone(), "Find Test Table", cx);
                v.focus_table_modal(window, cx);
            });
            window.render_frame(cx);
            assert!(view.read(cx).table_modal_state.is_open());
            window.press("escape", cx);
            window.render_frame(cx);
            assert!(!view.read(cx).table_modal_state.is_open());
            assert!(view.read(cx).chat_state.find_open);
            window.press("enter", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).chat_state.find_current,
                0,
                "Find Enter must work immediately after Table Modal -> Esc"
            );

            // 2A-4: Clicking Update Card "Later" button restores Find focus
            let rel = ReleaseInfo {
                version: "9.9.7".into(),
                tag: "v9.9.7".into(),
                published_at: "2026-11-26T00:00:00Z".into(),
                notes: vec!["Faster switching".into()],
                asset: WindowsAssetInfo {
                    asset_name: "multibot-windows-x64.zip".into(),
                    url: "https://github.com/valentinyeo/multibot/releases/download/v9.9.7/multibot-windows-x64.zip".into(),
                    sha256: "a".repeat(64),
                    size_bytes: 1024,
                    multibot_exe_sha256: "b".repeat(64),
                    multibotd_linux_x86_64_sha256: "c".repeat(64),
                },
            };
            view.update(cx, |v, cx| {
                v.update_ui.set_update_state(UpdateState::Available(rel));
                cx.notify();
            });
            window.render_frame(cx);
            window.click("update-later-btn", cx);
            window.render_frame(cx);
            assert!(view.read(cx).chat_state.find_open);
            window.press("q", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).chat_state.find_query,
                "needleq",
                "Find typing must work immediately after clicking Update Card Later button"
            );

            // Close Find cleanly
            window.press("escape", cx);
            window.render_frame(cx);
            assert!(!view.read(cx).chat_state.find_open);

            // =================================================================
            // Trace 2B: Slash Picker (`Ctrl+/` and `/`) survives Ctrl+K, F1,
            //           and Table Modal with live Down/Tab/Enter + typing
            // =================================================================
            window.press("ctrl-/", cx);
            window.render_frame(cx);
            assert!(view.read(cx).slash_picker_state.is_open());
            assert!(view.read(cx).slash_picker_state.filtered.len() >= 2);

            // 2B-1: Ctrl+K -> Esc restores prompt_input focus inside #chat-panel-root
            window.press("ctrl-k", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::Palette);
            window.press("escape", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::None);
            assert!(view.read(cx).slash_picker_state.is_open());
            assert!(
                view.read(cx)
                    .prompt_input
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window),
                "Ctrl+K -> Esc must restore prompt_input focus while Slash Picker is open"
            );
            let sel_before = view.read(cx).slash_picker_state.selected;
            window.press("down", cx);
            window.render_frame(cx);
            assert_ne!(
                view.read(cx).slash_picker_state.selected,
                sel_before,
                "Down arrow must navigate Slash Picker immediately after Ctrl+K -> Esc"
            );

            // 2B-2: F1 -> Esc blocks underlying Slash Picker/composer while open, restores on Esc
            let sel_before_f1 = view.read(cx).slash_picker_state.selected;
            window.press("f1", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::Shortcuts);
            window.press("down", cx);
            window.press("c", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).slash_picker_state.selected, sel_before_f1);
            assert_eq!(view.read(cx).prompt_input.read(cx).value().as_ref(), "");
            window.press("escape", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::None);
            assert!(view.read(cx).slash_picker_state.is_open());
            assert!(
                view.read(cx)
                    .prompt_input
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window),
                "F1 -> Esc must restore prompt_input focus while Slash Picker is open"
            );
            // Typing filter characters into composer + Tab accepting must work immediately!
            window.press("/", cx);
            window.press("p", cx);
            window.press("l", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).prompt_input.read(cx).value().as_ref(), "/pl");
            assert!(view.read(cx).slash_picker_state.is_open());

            // 2B-3: Table Modal -> Esc restores Slash Picker focus so Tab accepts cleanly
            view.update(cx, |v, cx| {
                v.open_table_modal(table, "Slash Test Table", cx);
                v.focus_table_modal(window, cx);
            });
            window.render_frame(cx);
            assert!(view.read(cx).table_modal_state.is_open());
            window.press("escape", cx);
            window.render_frame(cx);
            assert!(!view.read(cx).table_modal_state.is_open());
            assert!(view.read(cx).slash_picker_state.is_open());
            assert!(
                view.read(cx)
                    .prompt_input
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window)
            );
            window.press("tab", cx);
            window.render_frame(cx);
            assert!(
                !view.read(cx).slash_picker_state.is_open(),
                "Tab must accept Slash Picker command immediately after Table Modal -> Esc"
            );
            assert!(
                view.read(cx)
                    .prompt_input
                    .read(cx)
                    .value()
                    .starts_with("/plan"),
                "Tab must insert accepted slash command into composer"
            );

            // =================================================================
            // Trace 2C: Empty-composer Message Navigation (`Up`) & Action Menu (`Enter`)
            //           survive Ctrl+K -> Esc and F1 -> Esc with full Up/Down/Enter
            // =================================================================
            view.update(cx, |v, cx| {
                v.prompt_input.update(cx, |inp, cx| {
                    inp.set_value("", window, cx);
                    inp.focus(window, cx);
                });
            });
            window.render_frame(cx);

            window.press("up", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).chat_state.focused_message_idx, Some(1));

            // Ctrl+K -> Esc must keep Message Nav responsive
            window.press("ctrl-k", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::Palette);
            window.press("escape", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::None);
            assert_eq!(view.read(cx).chat_state.focused_message_idx, Some(1));
            assert!(
                view.read(cx)
                    .chat_state
                    .focus_handle
                    .as_ref()
                    .is_some_and(|fh| fh.is_focused(window)),
                "Ctrl+K -> Esc must restore #chat-panel-root focus during Message Navigation"
            );
            window.press("up", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).chat_state.focused_message_idx,
                Some(0),
                "Up must move Message Nav highlight immediately after Ctrl+K -> Esc"
            );
            window.press("enter", cx);
            window.render_frame(cx);
            assert!(
                view.read(cx).chat_state.message_action_menu_open,
                "Enter must open Message Action Menu immediately after Ctrl+K -> Esc"
            );
            assert_eq!(view.read(cx).chat_state.message_action_index, 0);

            // F1 -> Esc while Action Menu is open must keep Down/Enter responsive
            window.press("f1", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::Shortcuts);
            window.press("down", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).chat_state.message_action_index,
                0,
                "F1 Shortcuts must block Action Menu Down while open"
            );
            window.press("escape", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::None);
            assert!(view.read(cx).chat_state.message_action_menu_open);
            window.press("down", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).chat_state.message_action_index,
                1,
                "Down must advance Message Action Menu selection immediately after F1 -> Esc"
            );
            window.press("enter", cx);
            window.render_frame(cx);
            assert!(
                !view.read(cx).chat_state.message_action_menu_open,
                "Enter must execute selected Message Action Menu item immediately after F1 -> Esc"
            );
        })
        .unwrap();
    }
}
