#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod account;
mod app;
mod browser_pane;
mod chat_view;
mod computer_view;
mod inbox;
mod palette;
mod settings;
mod shell_host;
mod sidebar;
mod slash_picker;
mod state;
mod table_modal;
mod terminal_pane;
mod theme;
mod update_ui;
mod voice;

use app::{
    ArchiveSession, BrowseCommands, CaptureBrowserPane, CloseOverlay, CopyLatestCodeBlock,
    DeleteSession, FindInChat, FocusBrowserPane,
    FontZoomIn, FontZoomOut, FontZoomReset, InboxNext, InboxNextSplit, InboxPrev, InboxPrevSplit,
    JumpSession1, JumpSession2, JumpSession3, JumpSession4, JumpSession5, JumpSession6,
    JumpSession7, JumpSession8, JumpSession9, JumpWorkspace1, JumpWorkspace2, JumpWorkspace3,
    JumpWorkspace4, JumpWorkspace5, JumpWorkspace6, JumpWorkspace7, JumpWorkspace8, JumpWorkspace9,
    MarkSessionUnread, MoveSessionDown, MoveSessionUp, MoveSessionWaiting, MultibotView,
    NewSession, NewWindow, NextSession, NextWorkspace, OpenInbox, OpenPalette, OpenShortcuts,
    PrevSession, PrevWorkspace, ReconnectSession, RemindSession, RenameActiveSession, ReopenClosedSession,
    SnoozeWorkspace, ToggleFavorite, ToggleGroup, ToggleInbox, ToggleSettings, ToggleSidebar,
    ToggleBrowserPane, ToggleTerminal, VoiceDictation,
    ToggleUnreadOnly, UndoWaiting, WorkspaceViewAll, WorkspaceViewFavorites, WorkspaceViewFrequent,
};
use gpui_kit::{
    App, AppContext, Bounds, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, px, size,
};
use multibot_core::MultibotSettings;
use multibot_gpu::{ENV_GPU_LUID, ENV_GPU_MODE, ENV_GPU_REPORT_PATH, GpuPreference};
use multibot_updater::{
    UPDATE_READY_ARG, cleanup_old_update_artifacts, filter_preserved_cli_args, signal_update_ready,
};

fn parse_cli_gpu_override() -> Option<GpuPreference> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if let Some(val) = arg.strip_prefix("--gpu=") {
            return Some(GpuPreference::from_cli_arg(val));
        }
        if arg == "--gpu"
            && let Some(val) = args.next()
        {
            return Some(GpuPreference::from_cli_arg(&val));
        }
    }
    None
}

fn configure_gpu_env_before_gpui(settings: &MultibotSettings) {
    let pref = parse_cli_gpu_override().unwrap_or_else(|| settings.gpu.clone());
    let report_path = MultibotSettings::gpu_report_path();
    let _ = std::fs::remove_file(&report_path);

    // SAFETY: called once at the start of `main()` before GPUI or Tokio spawns any threads.
    unsafe {
        std::env::set_var(ENV_GPU_MODE, pref.env_mode_value());
        if let Some(luid) = pref.env_luid_value() {
            std::env::set_var(ENV_GPU_LUID, luid);
        } else {
            std::env::remove_var(ENV_GPU_LUID);
        }
        std::env::set_var(ENV_GPU_REPORT_PATH, &report_path);
    }
}

pub fn bind_multibot_keys(cx: &mut App) {
    cx.bind_keys([
        // Slice 1 (20 bindings)
        KeyBinding::new("ctrl-k", OpenPalette, None),
        KeyBinding::new("ctrl-shift-k", OpenPalette, None),
        KeyBinding::new("alt-i", OpenInbox, None),
        KeyBinding::new("ctrl-shift-i", ToggleInbox, None),
        KeyBinding::new("ctrl-,", ToggleSettings, None),
        KeyBinding::new("ctrl-shift-b", ToggleSidebar, None),
        // Slice 5 — built-in terminal pane
        KeyBinding::new("alt-c", ToggleTerminal, None),
        KeyBinding::new("alt-a", ToggleBrowserPane, None),
        KeyBinding::new("alt-q", FocusBrowserPane, None),
        KeyBinding::new("ctrl-shift-s", CaptureBrowserPane, None),
        KeyBinding::new("ctrl-n", NewWindow, None),
        KeyBinding::new("f1", OpenShortcuts, None),
        KeyBinding::new("escape", CloseOverlay, None),
        // Ctrl+D is captured in Chat View before the composer; terminal Ctrl+D stays local.
        KeyBinding::new("ctrl-shift-space", VoiceDictation, None),
        KeyBinding::new("ctrl-tab", NextSession, None),
        KeyBinding::new("ctrl-shift-tab", PrevSession, None),
        KeyBinding::new("ctrl-1", JumpSession1, None),
        KeyBinding::new("ctrl-2", JumpSession2, None),
        KeyBinding::new("ctrl-3", JumpSession3, None),
        KeyBinding::new("ctrl-4", JumpSession4, None),
        KeyBinding::new("ctrl-5", JumpSession5, None),
        KeyBinding::new("ctrl-6", JumpSession6, None),
        KeyBinding::new("ctrl-7", JumpSession7, None),
        KeyBinding::new("ctrl-8", JumpSession8, None),
        KeyBinding::new("ctrl-9", JumpSession9, None),
        // Slice 3 — Session lifecycle & sidebar structure (9 bindings)
        KeyBinding::new("ctrl-t", NewSession, None),
        KeyBinding::new("alt-t", NewSession, None),
        KeyBinding::new("ctrl-shift-w", DeleteSession, None),
        KeyBinding::new("ctrl-shift-e", ArchiveSession, None),
        KeyBinding::new("ctrl-shift-t", ReopenClosedSession, None),
        KeyBinding::new("ctrl-shift-r", ReconnectSession, None),
        KeyBinding::new("ctrl-shift-up", MoveSessionUp, None),
        KeyBinding::new("ctrl-shift-down", MoveSessionDown, None),
        KeyBinding::new("ctrl-g", ToggleGroup, None),
        // Slice 3 — Inbox & session triage (6 bindings)
        KeyBinding::new("alt-j", InboxNext, None),
        KeyBinding::new("alt-k", InboxPrev, None),
        KeyBinding::new("ctrl-shift-u", ToggleUnreadOnly, None),
        KeyBinding::new("alt-u", MarkSessionUnread, None),
        KeyBinding::new("alt-e", MoveSessionWaiting, None),
        KeyBinding::new("ctrl-h", RemindSession, None),
        KeyBinding::new("alt-h", RemindSession, None),
        KeyBinding::new("alt-z", UndoWaiting, None),
        // Slice 3 — Workspaces & views (16 bindings)
        KeyBinding::new("alt-s", ToggleFavorite, None),
        KeyBinding::new("ctrl-shift-y", SnoozeWorkspace, None),
        KeyBinding::new("alt-1", JumpWorkspace1, None),
        KeyBinding::new("alt-2", JumpWorkspace2, None),
        KeyBinding::new("alt-3", JumpWorkspace3, None),
        KeyBinding::new("alt-4", JumpWorkspace4, None),
        KeyBinding::new("alt-5", JumpWorkspace5, None),
        KeyBinding::new("alt-6", JumpWorkspace6, None),
        KeyBinding::new("alt-7", JumpWorkspace7, None),
        KeyBinding::new("alt-8", JumpWorkspace8, None),
        KeyBinding::new("alt-9", JumpWorkspace9, None),
        KeyBinding::new("alt-left", PrevWorkspace, None),
        KeyBinding::new("alt-right", NextWorkspace, None),
        KeyBinding::new("alt-shift-1", WorkspaceViewAll, None),
        KeyBinding::new("alt-shift-2", WorkspaceViewFrequent, None),
        KeyBinding::new("alt-shift-3", WorkspaceViewFavorites, None),
        // Slice 3 — System inline rename (1 binding)
        KeyBinding::new("f2", RenameActiveSession, None),
        // Slice 4 — Rich Chat View & font zoom (7 bindings)
        KeyBinding::new("ctrl-shift-d", CopyLatestCodeBlock, None),
        KeyBinding::new("ctrl-f", FindInChat, None),
        KeyBinding::new("ctrl-/", BrowseCommands, None),
        KeyBinding::new("ctrl-=", FontZoomIn, None),
        KeyBinding::new("ctrl-+", FontZoomIn, None),
        KeyBinding::new("ctrl--", FontZoomOut, None),
        KeyBinding::new("ctrl-0", FontZoomReset, None),
        // Inbox split cycling (scoped to "Inbox" context to override gpui_base::Root Tab/TabPrev)
        KeyBinding::new("tab", InboxNextSplit, Some("Inbox")),
        KeyBinding::new("shift-tab", InboxPrevSplit, Some("Inbox")),
    ]);
}

fn main() {
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    if raw_args.iter().any(|a| a == "--version" || a == "-V") {
        println!("multibot {}", multibot_proto::build_version());
        return;
    }

    let is_update_ready_launch = raw_args
        .iter()
        .any(|a| a == UPDATE_READY_ARG || a.starts_with("--update-ready="));
    let preserved_cli_args = filter_preserved_cli_args(&raw_args);

    // Normal startup: silently remove stale `.old`/`.previous`/`.failed`/`.tmp` binaries
    // and leftover `staging-*` directories while preserving `rejected.txt`.
    // On `--update-ready` launches, defer cleanup until AFTER window creation + readiness signal
    // so the previous updating process can observe `.multibot-update-ready` and exit first.
    if !is_update_ready_launch
        && let Ok(exe) = std::env::current_exe()
        && let Some(exe_dir) = exe.parent()
    {
        let _ = cleanup_old_update_artifacts(exe_dir, &MultibotSettings::updates_dir());
    }

    let settings = MultibotSettings::load();
    configure_gpu_env_before_gpui(&settings);

    let runtime = std::sync::Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .build()
            .expect("failed to start tokio runtime"),
    );

    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        theme::apply_multibot_dark_theme(cx);
        bind_multibot_keys(cx);
        state::SharedAppStore::init_persisted_with_args(cx, settings.clone(), preserved_cli_args);

        let bounds = Bounds::centered(None, size(px(1260.0), px(800.0)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("Multibot".into()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let rt = runtime.clone();
        let (_handle, view) = gpui_kit::open_window(options, cx, move |window, cx| {
            cx.new(|cx| MultibotView::new(rt, window, cx))
        })
        .expect("failed to open Multibot window");

        view.update(cx, |v, cx| {
            v.load_persisted_workspace_state(cx);
            v.sync_workspace_to_shared(cx);
            v.enable_disk_persistence();
            v.start_update_runtime(cx);
            v.auto_connect_configured_hosts(cx);
        });

        // Signal update readiness ONLY AFTER GPUI/DXGI window creation succeeded!
        if is_update_ready_launch
            && let Ok(exe) = std::env::current_exe()
        {
            let _ = signal_update_ready(&exe);
            if let Some(exe_dir) = exe.parent().map(std::path::Path::to_path_buf) {
                let updates_dir = MultibotSettings::updates_dir();
                runtime.spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    let _ = tokio::task::spawn_blocking(move || {
                        let _ = cleanup_old_update_artifacts(&exe_dir, &updates_dir);
                    })
                    .await;
                });
            }
        }
    });
}
