pub(crate) use actions::*;
use focus::FocusLayoutSignature;
#[cfg(test)]
use outbox::parse_outbox_session_key;
use shared::backfill_zigshel_workspaces;
#[cfg(test)]
#[path = "app_test_support.rs"]
mod test_support;
#[path = "app_state.rs"]
mod view_state;
use view_state::*;
#[path = "app_actions.rs"]
mod actions;
#[path = "app_daemon.rs"]
mod daemon;
#[path = "app_events.rs"]
mod events;
#[path = "app_focus.rs"]
mod focus;
#[path = "app_history.rs"]
mod history;
#[path = "app_host_events.rs"]
mod host_events;
#[cfg(test)]
#[path = "app_legacy_tests.rs"]
mod legacy_events;
#[path = "app_outbox.rs"]
mod outbox;
#[path = "app_sessions.rs"]
mod sessions;
#[path = "app_shared.rs"]
mod shared;
#[path = "app_surfaces.rs"]
mod surfaces;
#[path = "app_updates.rs"]
mod updates;
#[cfg(test)]
use updates::{DesktopUpdaterPlatform, GENERIC_UPDATE_ERROR_COPY, sanitize_update_error_copy};

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
    IntoElement, KeyDownEvent, ModifiersChangedEvent, ParentElement, Render,
    StatefulInteractiveElement, Styled, Subscription, Task, TestSupportExt, TitlebarOptions,
    Window, WindowBounds, WindowOptions, actions,
    component::{
        Sizable,
        button::{Button, ButtonCustomVariant, ButtonVariants},
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

/// Sessions the desktop asks for at connect time. The daemon returns only chats something
/// still holds open, so this is headroom for a lot of live work, not the payload size.
const SESSION_LIST_LIMIT: usize = 500;

pub struct MultibotView {
    pub(crate) runtime: Arc<tokio::runtime::Runtime>,
    pub(crate) focus: ViewFocusState,
    pub(crate) settings: MultibotSettings,
    pub(crate) gpu_report: GpuRuntimeReport,
    pub(crate) footprint: FootprintSnapshot,
    pub(crate) footprint_label: String,
    pub(crate) main_surface: MainSurface,
    pub(crate) settings_tab: crate::settings::SettingsTab,
    pub(crate) active_modal: ActiveModal,
    pub(crate) sidebar_state: SidebarState,
    pub(crate) inbox_state: InboxState,
    pub(crate) palette_state: PaletteState,
    pub(crate) chat_state: ChatViewState,
    pub(crate) update_ui: UpdateUiState,
    pub(crate) slash_picker_state: SlashPickerState,
    pub(crate) mention_picker_state: crate::mention_picker::MentionPickerState,
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
    pub(crate) connection: ViewConnectionState,
    pub(crate) selection: SessionSelectionState,
    pub(crate) history: TranscriptState,
    pub(crate) sync: ViewSyncState,
    pub(crate) _subscriptions: ViewSubscriptions,
    pub(crate) updater: UpdaterRuntimeState,
    pub(crate) outbox: ViewOutboxState,
    pub(crate) voice: crate::voice::VoiceState,
    pub(crate) inputs: ViewInputs,
    pub(crate) notice: Option<String>,
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
            if matches!(event, gpui_kit::component::input::InputEvent::Change) {
                this.sync_mention_picker(cx);
            }
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
            focus: ViewFocusState {
                root_focus_handle,
                last_focus_layout_sig: None,
                settings_focus_handle: cx.focus_handle(),
                pending_focus_input: false,
            },
            settings,
            gpu_report,
            footprint,
            footprint_label,
            main_surface: MainSurface::Chat,
            settings_tab: crate::settings::SettingsTab::default(),
            active_modal: ActiveModal::None,
            sidebar_state,
            inbox_state: InboxState::default(),
            palette_state,
            chat_state,
            update_ui,
            slash_picker_state: SlashPickerState::default(),
            mention_picker_state: Default::default(),
            table_modal_state: TableModalState::default(),
            terminal: TerminalPaneState::default(),
            #[cfg(windows)]
            browser: Default::default(),
            shell_host: crate::shell_host::ShellHost::default(),
            computer: ComputerViewState::default(),
            last_computer_url: None,
            connection: ViewConnectionState {
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
                daemon_version,
                daemon_binary_hash,
            },
            selection: SessionSelectionState {
                selected_session_host: None,
                pending_spawn_host: None,
                selected_session: None,
                closed_sessions: Vec::new(),
                pending_spawn_open: false,
                last_dispatched_prompt: None,
            },
            history: TranscriptState {
                pending_sent_messages: HashMap::new(),
                next_local_message_id: 0,
                messages: Vec::new(),
                chat_cache: ChatCache::default(),
                active_switch_token: None,
                pending_switch_tokens: HashMap::new(),
                pending_older_page_before: HashMap::new(),
                next_search_request_seq: 0,
                remote_search_request_id: None,
                remote_search_hits: Vec::new(),
                remote_search_truncated: false,
            },
            sync: ViewSyncState {
                #[cfg(test)]
                view_id,
                last_synced_revision: revision,
                last_synced_connection_gen: connection_gen,
                last_daemon_notice_seq: daemon_notice_seq,
            },
            _subscriptions: ViewSubscriptions {
                _shared_store_sub,
                _prompt_focus_sub,
                _prompt_change_sub,
                _window_activation_sub,
                _host_focus_sub,
                _daemon_event_task,
                _shortcuts_search_sub: shortcuts_search_sub,
            },
            updater: UpdaterRuntimeState {
                update_action_tx,
                preserved_cli_args,
                update_in_flight,
                last_update_notice_seq: update_notice_seq,
            },
            outbox: ViewOutboxState {
                shared_chat_outbox,
                chat_outbox: initial_chat_outbox,
                persist_outbox_to_disk: false,
                outbox_msg_map: HashMap::new(),
            },
            voice: crate::voice::VoiceState::default(),
            inputs: ViewInputs {
                deepgram_key_input,
                deepgram_language_input,
                prompt_input,
                shortcuts_search_input,
                host_input,
                palette_input,
            },
            notice: None,
        };
        view.start_account_once(cx);
        view
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
            .child(div().w(px(80.0)).flex_shrink_0())
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
                    .when_some(self.render_update_pill(cx), |el, pill| el.child(pill)),
            )
            .child(
                h_flex()
                    .w(px(80.0))
                    .flex_shrink_0()
                    .justify_end()
                    .items_center()
                    .gap_1p5()
                    .child({
                        Button::new("toggle-inbox")
                            .small()
                            .size(px(32.0))
                            .rounded(RADIUS_CHIP)
                            .custom(
                                ButtonCustomVariant::new(cx)
                                    .color(theme::bg_surface())
                                    .foreground(theme::text_primary())
                                    .hover(theme::bg_hover())
                                    .active(theme::bg_active_row()),
                            )
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
                    .child(
                        Button::new("chat-menu")
                            .small()
                            .size(px(32.0))
                            .rounded(RADIUS_CHIP)
                            .custom(
                                ButtonCustomVariant::new(cx)
                                    .color(theme::bg_surface())
                                    .foreground(theme::text_primary())
                                    .hover(theme::bg_hover())
                                    .active(theme::bg_active_row()),
                            )
                            .icon(lucide_icon(icons::MORE_HORIZONTAL, 16.0))
                            .accessibility_label("Chat menu")
                            .tooltip("Chat menu")
                            .on_click(cx.listener(|this, _, window, cx| {
                                if let Some(session) = &this.selection.selected_session {
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
}

impl Render for MultibotView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.apply_pending_draft(window, cx);
        self.sync_busy_dots_timer(cx);
        if self.active_modal == ActiveModal::None
            && (self.sidebar_state.rename.is_some()
                || self.sidebar_state.new_workspace_draft.is_some())
            && (self
                .inputs
                .prompt_input
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
                || self
                    .inputs
                    .host_input
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window))
        {
            self.sidebar_state.rename = None;
            self.sidebar_state.new_workspace_draft = None;
        }
        #[cfg(windows)]
        self.sync_browser_pane(window, cx);
        self.sync_embedded_shell(window, cx);
        let current_sig = self.focus_layout_signature();
        let prev_sig = self
            .focus
            .last_focus_layout_sig
            .replace(current_sig.clone());
        let layout_changed = prev_sig.as_ref().is_some_and(|prev| prev != &current_sig);
        if self.active_modal == ActiveModal::Shortcuts
            && prev_sig
                .as_ref()
                .is_none_or(|prev| prev.active_modal != ActiveModal::Shortcuts)
        {
            self.inputs
                .shortcuts_search_input
                .update(cx, |state, cx| state.set_value("", window, cx));
            self.focus_active_surface_default(window, cx);
        }
        if std::mem::take(&mut self.focus.pending_focus_input)
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
        let root = div()
            .id("multibot-root")
            .track_focus(&self.focus.root_focus_handle)
            .on_mouse_down(
                gpui_kit::MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.shell_host.focus_parent();
                }),
            )
            .relative()
            .size_full()
            .bg(theme::bg_canvas())
            .text_color(theme::text_primary());
        self.register_actions(root, cx)
            .child(
                h_flex()
                    .size_full()
                    .when(self.sidebar_rendered(), |el| {
                        el.child(self.render_sidebar(cx))
                    })
                    .child({
                        let surface = match self.main_surface {
                            MainSurface::Chat
                                if self.selection.selected_session.as_ref().is_some_and(|s| {
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
            .when_some(self.render_image_viewer(window, cx), |el, modal| el.child(modal))
    }
}

#[cfg(test)]
#[path = "app_tests.rs"]
pub(crate) mod tests;
