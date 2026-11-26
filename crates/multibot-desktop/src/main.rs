#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod app;

use app::{MultibotView, NewWindow, ToggleSettings, ToggleSidebar};
use gpui_kit::{
    AppContext, Bounds, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, px, size,
};
use multibot_core::MultibotSettings;
use multibot_gpu::{ENV_GPU_LUID, ENV_GPU_MODE, ENV_GPU_REPORT_PATH, GpuPreference};

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

fn main() {
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
        cx.bind_keys([
            KeyBinding::new("ctrl-,", ToggleSettings, None),
            KeyBinding::new("ctrl-shift-b", ToggleSidebar, None),
            KeyBinding::new("ctrl-n", NewWindow, None),
        ]);

        let bounds = Bounds::centered(None, size(px(1220.0), px(780.0)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("Multibot".into()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let rt = runtime.clone();
        gpui_kit::open_window(options, cx, move |window, cx| {
            cx.new(|cx| MultibotView::new(rt, window, cx))
        })
        .expect("failed to open Multibot window");
    });
}
