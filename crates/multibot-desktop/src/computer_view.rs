//! Slice 5 — lazy-loaded Computer View pane (MBRU-7).
//!
//! A right-hand split pane beside the chat that shows a remote desktop (noVNC or a
//! local preview) in a real WebView2 surface on Windows, and in a clean dark preview
//! surface everywhere else (Linux dev builds, headless GPUI tests).
//!
//! Hard Invariant #4 is why this module exists as its own file: a closed pane owns no
//! backend, no thread, no child process and no layout node. [`ComputerViewState::Closed`]
//! is the only state the app holds until the user opens the pane, and closing returns
//! to exactly that state — the WebView2 controller, its environment and its
//! `msedgewebview2.exe` children all go away with the drop.
//!
//! Hard Invariant #1 and #2 shape the Windows backend: the WebView2 environment uses an
//! explicit user-data-dir under `%APPDATA%\Multibot\webview2`
//! (`MULTIBOT_DATA_DIR\webview2` when that override is set for isolated test profiles),
//! never under `%APPDATA%\ZigShell`, and Chromium's GPU stack is disabled inside the
//! pane so it can never wake the NVIDIA adapter the app deliberately avoids.
//!
//! Hard Invariant #3: no new chord. `Ctrl+K → "Toggle Computer View"` is the entry
//! point, with the same row in the chat `...` menu.

use crate::icons::{self, lucide_icon};
use std::path::{Path, PathBuf};

use gpui_kit::{
    AppContext, Context, Entity, FocusHandle, Focusable as _, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, ParentElement, Role, StatefulInteractiveElement, Styled,
    Subscription, TestSupportExt, Window,
    component::{
        Sizable,
        button::Button,
        h_flex,
        input::{Input, InputEvent, InputState},
        v_flex,
    },
    div, px,
};

use crate::theme::{self, RADIUS_CHIP};

/// Folder under the Multibot data dir that holds the Computer View's WebView2 profile.
/// Always a child folder, so the legacy `%APPDATA%\ZigShell` install is never touched.
pub(crate) const WEBVIEW2_DIR_NAME: &str = "webview2";

/// Default URL offered in the connect prompt: the noVNC web server a desktop preview
/// normally binds to.
pub(crate) const DEFAULT_COMPUTER_URL: &str = "http://localhost:6080";

/// Refuse absurdly long URLs before they reach a webview or a log line.
pub(crate) const MAX_URL_LEN: usize = 2048;

/// Compact strip above the surface.
const COMPUTER_HEADER_HEIGHT_PX: f32 = 34.0;

/// Width of the split pane. Fixed so opening it never disturbs the chat column's own
/// layout maths, and so GPUI resolves it without a measurement pass.
pub(crate) const COMPUTER_PANE_WIDTH_PX: f32 = 520.0;

/// Why a typed URL was rejected. Carries the user-facing sentence because the same
/// text is rendered in the toolbar and asserted by the tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UrlError {
    Empty,
    TooLong,
    ControlCharacters,
    NotAllowed,
    MissingScheme,
    MissingHost,
    NotHttp,
}

impl UrlError {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::Empty => "Enter a URL to connect.",
            Self::TooLong => "That URL is too long.",
            Self::ControlCharacters => "That URL contains control characters.",
            Self::NotAllowed => "Only http:// and https:// links are allowed.",
            Self::MissingScheme => "URLs need http:// or https:// in front.",
            Self::MissingHost => "That URL has no host.",
            Self::NotHttp => "Only http:// and https:// links are allowed.",
        }
    }
}

/// The scheme a URL settled on, kept beside the normalised string so the toolbar and
/// the WebView2 backend never re-parse it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UrlScheme {
    Http,
    Https,
}

impl UrlScheme {
    fn parse(scheme: &str) -> Option<Self> {
        if scheme.eq_ignore_ascii_case("http") {
            Some(Self::Http)
        } else if scheme.eq_ignore_ascii_case("https") {
            Some(Self::Https)
        } else {
            None
        }
    }

    /// Shown in the toolbar so an `https://` page is visibly not an `http://` one.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }
}

/// A URL that passed validation. Only constructible through [`validate_url`] or
/// [`normalise_user_input`], so nothing downstream can hand a `file://` or
/// `javascript:` string to a webview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SafeUrl {
    scheme: UrlScheme,
    normalised: String,
    host: String,
}

impl SafeUrl {
    pub(crate) fn scheme(&self) -> UrlScheme {
        self.scheme
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.normalised
    }

    pub(crate) fn host(&self) -> &str {
        &self.host
    }

    fn from_parts(scheme: UrlScheme, normalised: &str, host: &str) -> Self {
        Self {
            scheme,
            normalised: normalised.to_string(),
            host: host.to_string(),
        }
    }
}

/// Validates a user-typed URL and returns only http/https links that have a host.
///
/// Rejects, in order: empty input, over-long input, any control character (including
/// the newline and carriage return that traffic a URL bar), a missing scheme, and any
/// scheme that is not `http` or `https` — which covers `file:`, `javascript:`,
/// `data:`, `about:`, `ws:` and the rest.
pub(crate) fn validate_url(input: &str) -> Result<SafeUrl, UrlError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(UrlError::Empty);
    }
    if trimmed.len() > MAX_URL_LEN {
        return Err(UrlError::TooLong);
    }
    if trimmed.chars().any(|c| c.is_control()) {
        return Err(UrlError::ControlCharacters);
    }
    let Some((scheme_part, rest)) = trimmed.split_once("://") else {
        // A URL with a scheme but no `//` (`javascript:x`, `mailto:x`, `file:/x`)
        // is a scheme we refuse, not a missing one. Saying "add http://" to someone
        // who typed `javascript:` would be nonsense.
        return Err(if looks_like_bare_scheme(trimmed) {
            UrlError::NotHttp
        } else {
            UrlError::MissingScheme
        });
    };
    let scheme = UrlScheme::parse(scheme_part).ok_or(UrlError::NotAllowed)?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority_host(authority);
    if host.is_empty() {
        return Err(UrlError::MissingHost);
    }
    Ok(SafeUrl::from_parts(scheme, trimmed, host))
}

/// Accepts a full URL, or a bare `host:port` typed by someone who did not want to type
/// a scheme: `localhost:6080` becomes `http://localhost:6080`.
pub(crate) fn normalise_user_input(input: &str) -> Result<SafeUrl, UrlError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(UrlError::Empty);
    }
    if trimmed.contains("://") {
        return validate_url(trimmed);
    }
    if looks_like_bare_scheme(trimmed) {
        return Err(UrlError::NotHttp);
    }
    validate_url(&format!("http://{trimmed}"))
}

/// True when the text starts with something that reads as a URL scheme rather than a
/// host, e.g. `javascript:alert(1)` or `file:/etc/passwd`.
fn looks_like_bare_scheme(text: &str) -> bool {
    let Some((head, _)) = text.split_once(':') else {
        return false;
    };
    !head.is_empty()
        && head.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        && head.chars().any(|c| c.is_ascii_alphabetic())
        // `localhost:6080` has a dot-free head but is a host, not a scheme: a real
        // scheme is followed by something that is not an all-digit port.
        && !text
            .split_once(':')
            .map(|(_, tail)| tail.chars().all(|c| c.is_ascii_digit() || c == '/'))
            .unwrap_or(false)
}

/// The host part of a URL authority, dropping any `user:password@` prefix and port.
///
/// A bracketed IPv6 literal keeps its brackets and everything inside them, so
/// `http://[::1]:6080` reports `[::1]` and not `[`.
fn authority_host(authority: &str) -> &str {
    let after_credentials = authority.rsplit('@').next().unwrap_or(authority);
    let trimmed = after_credentials.trim();
    if let Some(rest) = trimmed.strip_prefix('[')
        && let Some(end) = rest.find(']')
    {
        // Include both brackets: `[` + the literal + `]`.
        return &trimmed[..end + 2];
    }
    trimmed.split(':').next().unwrap_or(trimmed)
}

/// Resolves the WebView2 user-data-dir.
///
/// Honors `MULTIBOT_DATA_DIR` for isolated test profiles (the same override the settings
/// store uses), then falls back to the app data dir. Always appends [`WEBVIEW2_DIR_NAME`]
/// so the result can never be a bare data dir that the rest of the app also writes into.
pub(crate) fn webview2_data_dir() -> PathBuf {
    let base = std::env::var_os("MULTIBOT_DATA_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(multibot_core::MultibotSettings::data_dir);
    base.join(WEBVIEW2_DIR_NAME)
}

/// Rejects a data dir that escaped the Multibot tree, e.g. an override pointing at the
/// legacy install. [`webview2_data_dir`] already only appends a child folder; this is
/// the guard that keeps a future change honest.
pub(crate) fn data_dir_is_isolated(dir: &Path) -> bool {
    let text = dir.to_string_lossy().to_ascii_lowercase();
    !text.contains("zigshell") && !text.contains("multiprompt")
}

/// Which engine presents the pane. Tests assert on this instead of probing the OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComputerBackend {
    /// A live WebView2 controller exists (Windows only).
    WebView2,
    /// Dark preview surface: Linux dev builds and headless tests.
    Preview,
}

impl ComputerBackend {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::WebView2 => "WebView2",
            Self::Preview => "Preview",
        }
    }

    /// True when this backend can hold an OS-level browser process worth reclaiming.
    pub(crate) fn owns_process(self) -> bool {
        matches!(self, Self::WebView2)
    }
}

fn current_backend() -> ComputerBackend {
    if cfg!(target_os = "windows") {
        ComputerBackend::WebView2
    } else {
        ComputerBackend::Preview
    }
}

/// What the toolbar and status line report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ComputerStatus {
    /// No URL loaded yet: the pane shows the connect prompt and owns no backend.
    NeedsUrl,
    Connecting,
    Loaded,
    Failed(String),
}

impl ComputerStatus {
    pub(crate) fn label(&self) -> String {
        match self {
            Self::NeedsUrl => "Enter a URL to start".to_string(),
            Self::Connecting => "Connecting…".to_string(),
            Self::Loaded => "Connected".to_string(),
            Self::Failed(error) => format!("Could not connect: {error}"),
        }
    }
}

/// The pane's whole lifecycle in one type.
///
/// `Closed` carries no URL string, no focus handle, no input entity and no backend,
/// which is the "0 MB when closed" promise made concrete. `Open` allocates only once
/// the user asks for the pane.
#[derive(Default)]
pub(crate) enum ComputerViewState {
    #[default]
    Closed,
    Open(Box<ComputerViewOpen>),
}

pub(crate) struct ComputerViewOpen {
    /// URL currently loaded in the backend, if any. `None` while the connect prompt is
    /// up: the pane can be open with no webview at all.
    pub url: Option<SafeUrl>,
    pub status: ComputerStatus,
    pub backend: ComputerBackend,
    pub focus_handle: FocusHandle,
    pub url_input: Entity<InputState>,
    /// Keeps the URL field's Enter handler alive exactly as long as the pane.
    pub _input_sub: Subscription,
    /// Measured size of the surface, refreshed by the prepaint hook.
    pub bounds: Option<(f32, f32)>,
    backend_state: BackendState,
}

impl ComputerViewOpen {
    /// True when a backend is holding resources right now. A pane open on the connect
    /// prompt is `false`: nothing has been initialized.
    pub(crate) fn backend_live(&self) -> bool {
        self.backend_state.is_live()
    }
}

impl ComputerViewState {
    pub(crate) fn is_open(&self) -> bool {
        matches!(self, Self::Open(_))
    }

    pub(crate) fn open(&self) -> Option<&ComputerViewOpen> {
        match self {
            Self::Open(open) => Some(open),
            Self::Closed => None,
        }
    }

    pub(crate) fn open_mut(&mut self) -> Option<&mut ComputerViewOpen> {
        match self {
            Self::Open(open) => Some(open),
            Self::Closed => None,
        }
    }

    /// Number of live backend handles. Zero whenever the pane is closed, and zero while
    /// it is open on the connect prompt.
    #[cfg(test)]
    pub(crate) fn backend_handle_count(&self) -> usize {
        match self {
            Self::Closed => 0,
            Self::Open(open) => usize::from(open.backend_state.is_live()),
        }
    }

    #[cfg(test)]
    pub(crate) fn backend(&self) -> Option<ComputerBackend> {
        self.open().map(|open| open.backend)
    }
}

impl Drop for ComputerViewState {
    fn drop(&mut self) {
        // Taking the backend out runs its own `Drop`, which is where WebView2 teardown
        // lives. Explicit so a closed pane can never leave a process behind.
        if let Self::Open(open) = self {
            open.backend_state.teardown();
        }
    }
}

/// Platform backend holder. The Windows variant owns the WebView2 webview; the other
/// variant is a placeholder so Linux builds and headless tests compile and prove the
/// same lifecycle code.
enum BackendState {
    Detached,
    #[cfg(target_os = "windows")]
    WebView2(Box<WebView2Backend>),
    #[cfg(not(target_os = "windows"))]
    Preview(PreviewBackend),
}

impl BackendState {
    fn is_live(&self) -> bool {
        match self {
            Self::Detached => false,
            #[cfg(target_os = "windows")]
            Self::WebView2(_) => true,
            #[cfg(not(target_os = "windows"))]
            Self::Preview(_) => true,
        }
    }

    fn teardown(&mut self) {
        // Assigning over the variant drops the old backend immediately. On Windows that
        // closes the controller, destroys the child window and lets `msedgewebview2.exe`
        // exit; on Linux it just frees the bounds bookkeeping.
        *self = Self::Detached;
    }

    #[allow(unused_variables)]
    fn set_bounds(&mut self, bounds: (f32, f32)) {
        match self {
            Self::Detached => {}
            #[cfg(target_os = "windows")]
            Self::WebView2(backend) => backend.set_bounds(bounds),
            #[cfg(not(target_os = "windows"))]
            Self::Preview(backend) => backend.set_bounds(bounds.0, bounds.1),
        }
    }

    fn navigate(&self, url: &str) -> Result<(), String> {
        match self {
            Self::Detached => Err("The pane is not connected yet.".to_string()),
            #[cfg(target_os = "windows")]
            Self::WebView2(backend) => backend.navigate(url),
            #[cfg(not(target_os = "windows"))]
            Self::Preview(_) => {
                let _ = url;
                Ok(())
            }
        }
    }

    fn reload(&self) -> Result<(), String> {
        match self {
            Self::Detached => Err("The pane is not connected yet.".to_string()),
            #[cfg(target_os = "windows")]
            Self::WebView2(backend) => backend.reload(),
            #[cfg(not(target_os = "windows"))]
            Self::Preview(_) => Ok(()),
        }
    }

    /// The WebView2 profile folder, for tests and status text. `None` on backends that
    /// keep no profile on disk.
    #[cfg(test)]
    fn data_dir(&self) -> Option<&Path> {
        match self {
            Self::Detached => None,
            #[cfg(target_os = "windows")]
            Self::WebView2(backend) => Some(backend.data_dir()),
            #[cfg(not(target_os = "windows"))]
            Self::Preview(_) => None,
        }
    }
}

/// Linux / headless backend. Holds nothing but paint bookkeeping, so the
/// open/navigate/close lifecycle is fully testable without an OS browser.
#[cfg(not(target_os = "windows"))]
pub(crate) struct PreviewBackend {
    /// Last bounds the pane was laid out at, so the preview keeps up with resizes
    /// exactly like the real backend does.
    bounds: Option<(f32, f32)>,
}

#[cfg(not(target_os = "windows"))]
impl PreviewBackend {
    fn new() -> Self {
        Self { bounds: None }
    }

    fn set_bounds(&mut self, width: f32, height: f32) {
        self.bounds = Some((width, height));
    }

    #[cfg(test)]
    fn bounds(&self) -> Option<(f32, f32)> {
        self.bounds
    }
}

/// Windows WebView2 backend, kept in its own module so the unsafe COM glue stays out of
/// the pane's UI code.
#[cfg(target_os = "windows")]
mod windows_backend {
    use std::path::{Path, PathBuf};

    use wry::WebViewBuilderExtWindows;

    /// Chromium flags for the pane. The GPU stack is disabled on purpose: a webview
    /// that enumerates adapters can wake the discrete adapter the app skips for its own
    /// rendering, which would break Hard Invariant #1 from a side process.
    const BROWSER_ARGS: &str = "--disable-gpu \
        --disable-gpu-compositing \
        --disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection \
        --autoplay-policy=no-user-gesture-required";

    /// The live WebView2 webview plus the context that owns its user data.
    ///
    /// Field order matters: the webview is created from the context and must be dropped
    /// before it, so it is declared first.
    pub(crate) struct WebView2Backend {
        webview: Option<wry::WebView>,
        context: Option<wry::WebContext>,
        data_dir: PathBuf,
        bounds: Option<(f32, f32)>,
        scale_factor: f32,
    }

    impl WebView2Backend {
        /// Creates the webview as a child of `window`, loading `url`.
        ///
        /// This is the only place in the app that starts a browser process, and it is
        /// only reached from an explicit user action.
        pub(crate) fn create(
            window: &gpui_kit::Window,
            url: &str,
            data_dir: PathBuf,
            bounds: (f32, f32),
        ) -> Result<Self, String> {
            let scale_factor = window.scale_factor();
            let rect = rect_from_bounds(bounds, scale_factor);
            let mut context = wry::WebContext::new(Some(data_dir.clone()));
            let webview = wry::WebViewBuilder::new_with_web_context(&mut context)
                .with_url(url)
                .with_bounds(rect)
                // Dark canvas behind the page so a slow first paint does not flash
                // white in an OLED-dark app.
                .with_background_color((0x0D, 0x0D, 0x0D, 255))
                .with_devtools(false)
                .with_hotkeys_zoom(false)
                .with_additional_browser_args(BROWSER_ARGS)
                .build_as_child(window)
                .map_err(|error| format!("Could not start WebView2: {error}"))?;
            Ok(Self {
                webview: Some(webview),
                context: Some(context),
                data_dir,
                bounds: Some(bounds),
                scale_factor,
            })
        }

        pub(crate) fn navigate(&self, url: &str) -> Result<(), String> {
            match self.webview.as_ref() {
                Some(webview) => webview
                    .load_url(url)
                    .map_err(|error| format!("Navigation failed: {error}")),
                None => Err("The webview is no longer running.".to_string()),
            }
        }

        pub(crate) fn reload(&self) -> Result<(), String> {
            match self.webview.as_ref() {
                Some(webview) => webview
                    .reload()
                    .map_err(|error| format!("Reload failed: {error}")),
                None => Err("The webview is no longer running.".to_string()),
            }
        }

        /// Re-fits the webview to the pane. No-op when the bounds are unchanged, so a
        /// repaint never touches the native window.
        pub(crate) fn set_bounds(&mut self, bounds: (f32, f32)) {
            if self.bounds == Some(bounds) {
                return;
            }
            if let Some(webview) = self.webview.as_ref() {
                let _ = webview.set_bounds(rect_from_bounds(bounds, self.scale_factor));
            }
            self.bounds = Some(bounds);
        }

        pub(crate) fn data_dir(&self) -> &Path {
            &self.data_dir
        }

        #[cfg(test)]
        pub(crate) fn bounds(&self) -> Option<(f32, f32)> {
            self.bounds
        }
    }

    impl Drop for WebView2Backend {
        fn drop(&mut self) {
            // Explicit order: close the controller before the environment it came from,
            // so `msedgewebview2.exe` sees a clean shutdown and exits.
            self.webview = None;
            self.context = None;
        }
    }

    /// Maps logical pane pixels to the physical `wry::Rect` the child window expects.
    pub(crate) fn rect_from_bounds(bounds: (f32, f32), scale_factor: f32) -> wry::Rect {
        let width = if bounds.0 > 0.0 { bounds.0 } else { 1.0 };
        let height = if bounds.1 > 0.0 { bounds.1 } else { 1.0 };
        wry::Rect {
            position: wry::dpi::LogicalPosition::new(0.0, 0.0).into(),
            size: wry::dpi::LogicalSize::new(width, height)
                .to_physical::<u32>(scale_factor as f64)
                .into(),
        }
    }
}

#[cfg(target_os = "windows")]
use windows_backend::WebView2Backend;

/// Creates the platform backend. Split out so the Windows/Linux divide lives in one
/// place and the lifecycle code above stays platform-free.
#[cfg(target_os = "windows")]
fn create_backend(
    window: &mut Window,
    url: &str,
    bounds: (f32, f32),
) -> Result<BackendState, String> {
    let backend = WebView2Backend::create(window, url, webview2_data_dir(), bounds)?;
    Ok(BackendState::WebView2(Box::new(backend)))
}

#[cfg(not(target_os = "windows"))]
fn create_backend(
    _window: &mut Window,
    url: &str,
    bounds: (f32, f32),
) -> Result<BackendState, String> {
    let _ = url;
    let mut backend = PreviewBackend::new();
    backend.set_bounds(bounds.0, bounds.1);
    Ok(BackendState::Preview(backend))
}

/// Launches the OS browser for an already-validated http/https URL.
///
/// The URL is passed as its own argv element (never through a shell) and was re-checked
/// by the caller, so an injected flag cannot reach a command interpreter.
pub(crate) fn open_in_external_browser(url: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        // Opens the default browser without a shell and refuses anything but a URL.
        std::process::Command::new("rundll32")
            .arg("url.dll,FileProtocolHandler")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("Could not open a browser: {error}"))
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("Could not open a browser: {error}"))
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("Could not open a browser: {error}"))
    }
}

impl crate::app::MultibotView {
    /// `Ctrl+K → Toggle Computer View` and the chat `...` menu.
    pub(crate) fn toggle_computer_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.computer.is_open() {
            self.close_computer_view(cx);
        } else {
            self.open_computer_view(window, cx);
        }
        cx.notify();
    }

    /// Opens the pane on the session's configured URL when it has one, else on the
    /// connect prompt. Opening alone never starts a browser process.
    pub(crate) fn open_computer_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.computer.is_open() {
            return;
        }
        // The pane splits the chat column, so opening it from the Inbox brings the chat
        // surface back instead of docking beside something invisible.
        if self.main_surface == crate::state::MainSurface::Inbox {
            self.set_main_surface(crate::state::MainSurface::Chat);
        }
        let focus_handle = cx.focus_handle();
        let url_input = cx.new(|cx| InputState::new(window, cx).placeholder(DEFAULT_COMPUTER_URL));
        let input_sub = cx.subscribe(&url_input, |this, _input, event, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.connect_computer_view(cx);
            }
        });
        // The last URL this pane connected to is offered again, but the pane still
        // starts nothing until the user presses Enter: remembering an address is not
        // permission to launch a browser.
        let remembered = self.last_computer_url.clone();
        let open = ComputerViewOpen {
            url: None,
            status: ComputerStatus::NeedsUrl,
            backend: current_backend(),
            focus_handle: focus_handle.clone(),
            url_input: url_input.clone(),
            _input_sub: input_sub,
            bounds: None,
            backend_state: BackendState::Detached,
        };
        self.computer = ComputerViewState::Open(Box::new(open));
        url_input.update(cx, |state, cx| {
            if let Some(safe) = remembered.as_ref() {
                state.set_value(safe.as_str(), window, cx);
            }
        });
        // Focus lands in the URL bar so the first keystroke is the address, not a Tab.
        let input_focus = url_input.read(cx).focus_handle(cx);
        self.shell_host.focus_parent();
        window.focus(&input_focus, cx);
        let _ = focus_handle;
        cx.notify();
    }

    /// Closes the pane and destroys the backend, so a closed Computer View owns no
    /// process, no thread and no native window.
    pub(crate) fn close_computer_view(&mut self, cx: &mut Context<Self>) {
        self.teardown_computer_view();
        self.focus.pending_focus_input = true;
        cx.notify();
    }

    /// Drops the pane's state without a notification. Used by surface transitions that
    /// already repaint (the Inbox unmount owns no `Context`) and by
    /// [`Self::close_computer_view`].
    pub(crate) fn teardown_computer_view(&mut self) {
        if let Some(open) = self.computer.open_mut() {
            open.backend_state.teardown();
        }
        // Dropping the state also drops the URL input entity and its subscription.
        self.computer = ComputerViewState::Closed;
    }

    /// Records a successful connection so the next open can offer the same address.
    fn remember_computer_url(&mut self, url: &SafeUrl) {
        self.last_computer_url = Some(url.clone());
    }

    /// Toolbar Connect: validates the typed URL, then creates or navigates the backend.
    pub(crate) fn connect_computer_view(&mut self, cx: &mut Context<Self>) {
        let Some(open) = self.computer.open() else {
            return;
        };
        let typed = open.url_input.read(cx).value().to_string();
        match normalise_user_input(&typed) {
            Ok(url) => self.navigate_computer_view(url, cx),
            Err(error) => {
                if let Some(open) = self.computer.open_mut() {
                    open.status = ComputerStatus::Failed(error.message().to_string());
                }
                cx.notify();
            }
        }
    }

    /// Navigates an already-validated URL, creating the backend when there is none.
    ///
    /// `window` comes from whichever entry point has one (the `Ctrl+K` action, the
    /// palette row, the toolbar button). The non-window overload is for callers inside a
    /// key handler that already holds one.
    pub(crate) fn navigate_computer_view(&mut self, url: SafeUrl, cx: &mut Context<Self>) {
        self.navigate_computer_view_with_window(url, None, cx);
    }

    fn navigate_computer_view_with_window(
        &mut self,
        url: SafeUrl,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.computer.open_mut() else {
            return;
        };
        if !open.backend_state.is_live() {
            let bounds = open.bounds.unwrap_or((0.0, 0.0));
            open.status = ComputerStatus::Connecting;
            // Without a window we cannot create a webview; report it instead of
            // pretending the pane is connected.
            let Some(window) = window else {
                open.status = ComputerStatus::Failed(
                    "Open the pane from the window before connecting.".to_string(),
                );
                cx.notify();
                return;
            };
            match create_backend(window, url.as_str(), bounds) {
                Ok(state) => {
                    self.remember_computer_url(&url);
                    if let Some(open) = self.computer.open_mut() {
                        open.backend_state = state;
                        open.url = Some(url);
                        open.status = ComputerStatus::Loaded;
                    }
                }
                Err(error) => {
                    if let Some(open) = self.computer.open_mut() {
                        open.backend_state = BackendState::Detached;
                        open.status = ComputerStatus::Failed(error);
                    }
                }
            }
            cx.notify();
            return;
        }
        let result = open.backend_state.navigate(url.as_str());
        let navigated = result.is_ok();
        if navigated {
            let url = url.clone();
            self.remember_computer_url(&url);
        }
        if let Some(open) = self.computer.open_mut() {
            match result {
                Ok(()) => {
                    open.url = Some(url);
                    open.status = ComputerStatus::Loaded;
                }
                Err(error) => open.status = ComputerStatus::Failed(error),
            }
        }
        cx.notify();
    }

    /// Connect from a key handler or a click that already has the window.
    pub(crate) fn connect_computer_view_with_window(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.computer.open() else {
            return;
        };
        let typed = open.url_input.read(cx).value().to_string();
        match normalise_user_input(&typed) {
            Ok(url) => self.navigate_computer_view_with_window(url, Some(window), cx),
            Err(error) => {
                if let Some(open) = self.computer.open_mut() {
                    open.status = ComputerStatus::Failed(error.message().to_string());
                }
                cx.notify();
            }
        }
    }

    pub(crate) fn reload_computer_view(&mut self, cx: &mut Context<Self>) {
        let Some(open) = self.computer.open_mut() else {
            return;
        };
        if !open.backend_state.is_live() {
            if let Some(url) = open.url.clone() {
                self.navigate_computer_view(url, cx);
            }
            return;
        }
        let result = open.backend_state.reload();
        if let Some(open) = self.computer.open_mut() {
            match result {
                Ok(()) => open.status = ComputerStatus::Loaded,
                Err(error) => open.status = ComputerStatus::Failed(error),
            }
        }
        cx.notify();
    }

    /// Opens the loaded page in the user's normal browser. Starts nothing while the pane
    /// has no URL.
    pub(crate) fn open_computer_view_externally(&mut self, cx: &mut Context<Self>) {
        let Some(url) = self
            .computer
            .open()
            .and_then(|open| open.url.as_ref())
            .map(|url| url.as_str().to_string())
        else {
            return;
        };
        // Re-validate at the boundary: only http/https ever reaches the OS.
        let Ok(safe) = validate_url(&url) else {
            return;
        };
        match open_in_external_browser(safe.as_str()) {
            Ok(()) => {
                self.notice = Some(format!("Opened {} in your browser.", safe.host()));
            }
            Err(error) => {
                if let Some(open) = self.computer.open_mut() {
                    open.status = ComputerStatus::Failed(error);
                }
            }
        }
        cx.notify();
    }

    /// Re-fits the backend to the pane measured this frame. Called from `render`, so it
    /// never notifies: the frame being built already carries the new size.
    pub(crate) fn sync_computer_view_bounds(&mut self) {
        let Some(open) = self.computer.open_mut() else {
            return;
        };
        let Some(bounds) = open.bounds else {
            return;
        };
        open.backend_state.set_bounds(bounds);
    }

    /// Escape closes the pane only while the URL field has focus, so the global
    /// `CloseOverlay` still owns Escape everywhere else.
    pub(crate) fn handle_computer_view_key_down(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.active_modal != crate::state::ActiveModal::None {
            return false;
        }
        let Some(open) = self.computer.open() else {
            return false;
        };
        if !open.url_input.read(cx).focus_handle(cx).is_focused(window) {
            return false;
        }
        if ev.keystroke.key.eq_ignore_ascii_case("escape") {
            self.close_computer_view(cx);
            window.prevent_default();
            cx.stop_propagation();
            return true;
        }
        false
    }

    /// Renders the pane, or `None` when it is closed so a closed Computer View adds no
    /// layout node and no allocation.
    pub(crate) fn render_computer_view(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<gpui_kit::AnyElement> {
        if !self.computer.is_open() {
            return None;
        }
        let open = self.computer.open()?;
        let status = open.status.clone();
        let backend = open.backend;
        // The scheme leads the readable URL so an `https://` page is visibly not an
        // `http://` one in the status line.
        let url_text = open
            .url
            .as_ref()
            .map(|url| format!("{}://{}", url.scheme().label(), url.host()))
            .unwrap_or_else(|| "No URL loaded".to_string());
        let live = open.backend_live();
        let input = open.url_input.clone();
        let focus_handle = open.focus_handle.clone();
        let dot = match &status {
            ComputerStatus::Loaded => theme::accent_green(),
            ComputerStatus::Connecting => theme::accent_amber(),
            ComputerStatus::Failed(_) => theme::accent_amber(),
            ComputerStatus::NeedsUrl => theme::text_muted(),
        };
        let status_color = match &status {
            ComputerStatus::NeedsUrl => theme::text_muted(),
            _ => theme::text_secondary(),
        };
        let status_label = status.label();

        let header = h_flex()
            .id("computer-header")
            .test_support()
            .w_full()
            .h(px(COMPUTER_HEADER_HEIGHT_PX))
            .flex_shrink_0()
            .items_center()
            .gap_2()
            .px_3()
            .bg(theme::bg_sidebar())
            .border_b_1()
            .border_color(theme::border_subtle())
            .child(
                div()
                    .w(px(7.0))
                    .h(px(7.0))
                    .flex_shrink_0()
                    .rounded(RADIUS_CHIP)
                    .bg(dot),
            )
            .child(
                div()
                    .id("computer-status")
                    .test_support()
                    .flex_shrink_0()
                    .text_size(px(11.5))
                    .text_color(status_color)
                    // Advertised so headless tests can read the pane's state without a
                    // screen reader or a screenshot.
                    .aria_value(status_label.clone())
                    .child(status_label),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .rounded(RADIUS_CHIP)
                    .bg(theme::bg_canvas())
                    .border_1()
                    .border_color(theme::border_subtle())
                    .px_2()
                    .child(
                        Input::new(&input)
                            .id("computer-url-input")
                            .appearance(false),
                    ),
            )
            .child(
                Button::new("computer-reload")
                    .xsmall()
                    .icon(lucide_icon(icons::REFRESH_CW, 16.0))
                    .label("Reload")
                    .accessibility_label("Reload Computer View")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.reload_computer_view(cx);
                    })),
            )
            .child(
                Button::new("computer-external")
                    .xsmall()
                    .icon(lucide_icon(icons::EXTERNAL_LINK, 16.0))
                    .label("Browser")
                    .accessibility_label("Open in external browser")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_computer_view_externally(cx);
                    })),
            )
            .child(
                Button::new("computer-close")
                    .xsmall()
                    .icon(lucide_icon(icons::X, 12.0))
                    .label("Close")
                    .accessibility_label("Close Computer View")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.close_computer_view(cx);
                    })),
            );

        let body: gpui_kit::AnyElement = if live {
            // The native webview paints over this area as a child window, so the body
            // reserves the space and advertises the URL underneath it.
            div()
                .id("computer-surface")
                .test_support()
                .flex_1()
                .min_h_0()
                .bg(theme::bg_canvas())
                .aria_value(url_text.clone())
                .child(
                    div()
                        .p_3()
                        .text_size(px(11.5))
                        .text_color(theme::text_muted())
                        .child(format!("{} · {url_text}", backend.label())),
                )
                .into_any_element()
        } else {
            let hint = match &status {
                ComputerStatus::Failed(_) => "Check the URL, then press Enter.",
                _ => "Type a noVNC or preview URL, then press Enter.",
            };
            v_flex()
                .id("computer-connect-prompt")
                .test_support()
                .flex_1()
                .min_h_0()
                .items_center()
                .justify_center()
                .gap_2()
                .p_6()
                .bg(theme::bg_canvas())
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(theme::text_primary())
                        .child("Computer View"),
                )
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(theme::text_muted())
                        .child(hint),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme::text_muted())
                        .child(format!("Example: {DEFAULT_COMPUTER_URL}")),
                )
                .child(
                    div()
                        .id("computer-connect")
                        .role(Role::Button)
                        .test_support()
                        .rounded(RADIUS_CHIP)
                        .border_1()
                        .border_color(theme::border_subtle())
                        .bg(theme::bg_surface())
                        .px_3()
                        .py_1()
                        .text_size(px(11.5))
                        .text_color(theme::text_primary())
                        .cursor_pointer()
                        .hover(|style| style.bg(theme::bg_hover()))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                cx.stop_propagation();
                                if let Some(open) = this.computer.open() {
                                    open.url_input.update(cx, |state, cx| {
                                        state.set_value(DEFAULT_COMPUTER_URL, window, cx);
                                    });
                                }
                                this.connect_computer_view_with_window(window, cx);
                            }),
                        )
                        .child(format!("Connect to {DEFAULT_COMPUTER_URL}")),
                )
                .into_any_element()
        };

        let weak = cx.entity().downgrade();
        Some(
            v_flex()
                .w(px(COMPUTER_PANE_WIDTH_PX))
                .h_full()
                .flex_shrink_0()
                .min_h_0()
                .bg(theme::bg_canvas())
                .border_l_1()
                .border_color(theme::border_subtle())
                .track_focus(&focus_handle)
                .child(header)
                .child(body)
                .on_children_prepainted(move |child_bounds, _window, cx| {
                    // The surface body is the measured child; its size is what the
                    // backend should fill. No notify: layout already changed, so this
                    // frame carries the resize and the next one re-fits the webview.
                    let Some(bounds) = child_bounds.last() else {
                        return;
                    };
                    let size = (bounds.size.width.as_f32(), bounds.size.height.as_f32());
                    let _ = weak.update(cx, |view, _cx| {
                        if let Some(open) = view.computer.open_mut() {
                            open.bounds = Some(size);
                        }
                        view.sync_computer_view_bounds();
                    });
                })
                .id("computer-pane")
                .test_support()
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expect_ok(input: &str) -> SafeUrl {
        validate_url(input).unwrap_or_else(|error| panic!("{input} must validate: {error:?}"))
    }

    #[test]
    fn http_and_https_urls_are_allowed() {
        assert_eq!(expect_ok("http://localhost:6080").scheme(), UrlScheme::Http);
        assert_eq!(
            expect_ok("https://vnc.example.com").scheme(),
            UrlScheme::Https
        );
        assert_eq!(
            expect_ok("  https://vnc.example.com/a/b  ").as_str(),
            "https://vnc.example.com/a/b"
        );
        assert_eq!(
            expect_ok("http://127.0.0.1:6080/vnc.html?autoconnect=1").host(),
            "127.0.0.1"
        );
        assert_eq!(
            expect_ok("https://user:pw@vnc.example.com").host(),
            "vnc.example.com"
        );
        assert_eq!(expect_ok("http://[::1]:6080").host(), "[::1]");
        assert_eq!(expect_ok("http://user@[fe80::1]:80/x").host(), "[fe80::1]");
    }

    #[test]
    fn dangerous_schemes_are_rejected() {
        for bad in [
            "javascript:alert(1)",
            "file:///c:/windows/system32/calc.exe",
            "data:text/html,<script>alert(1)</script>",
            "about:blank",
            "ws://localhost:6080",
            "vnc://localhost:6080",
        ] {
            assert!(validate_url(bad).is_err(), "{bad} must be rejected");
        }
        assert_eq!(validate_url("javascript:alert(1)"), Err(UrlError::NotHttp));
        assert_eq!(validate_url("file:///c:/x"), Err(UrlError::NotAllowed));
        assert_eq!(validate_url("data:text/html,x"), Err(UrlError::NotHttp));
    }

    #[test]
    fn control_characters_and_empty_input_are_rejected() {
        assert_eq!(validate_url("   "), Err(UrlError::Empty));
        assert_eq!(
            validate_url("http://localhost:6080\r\nHost: evil"),
            Err(UrlError::ControlCharacters)
        );
        assert_eq!(
            validate_url("http://local\u{0}host"),
            Err(UrlError::ControlCharacters)
        );
        assert_eq!(
            validate_url("http://local\thost"),
            Err(UrlError::ControlCharacters)
        );
    }

    #[test]
    fn over_long_and_hostless_urls_are_rejected() {
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_LEN));
        assert_eq!(validate_url(&long), Err(UrlError::TooLong));
        assert_eq!(validate_url("http://"), Err(UrlError::MissingHost));
        assert_eq!(validate_url("https:///path"), Err(UrlError::MissingHost));
        assert_eq!(validate_url("http://:6080"), Err(UrlError::MissingHost));
        assert_eq!(
            validate_url("http://user@:6080"),
            Err(UrlError::MissingHost)
        );
        assert_eq!(validate_url("localhost:6080"), Err(UrlError::MissingScheme));
    }

    #[test]
    fn bare_host_and_port_gain_an_http_scheme() {
        assert_eq!(
            normalise_user_input("localhost:6080").unwrap().as_str(),
            "http://localhost:6080"
        );
        assert_eq!(
            normalise_user_input("127.0.0.1:6080/vnc.html")
                .unwrap()
                .as_str(),
            "http://127.0.0.1:6080/vnc.html"
        );
        assert_eq!(
            normalise_user_input("https://vnc.example.com")
                .unwrap()
                .as_str(),
            "https://vnc.example.com"
        );
    }

    #[test]
    fn bare_input_cannot_smuggle_a_scheme() {
        assert_eq!(
            normalise_user_input("javascript:alert(1)"),
            Err(UrlError::NotHttp)
        );
        assert_eq!(
            normalise_user_input("file:/etc/passwd"),
            Err(UrlError::NotHttp)
        );
        assert!(normalise_user_input("data:text/html,x").is_err());
        assert_eq!(normalise_user_input("  "), Err(UrlError::Empty));
    }

    #[test]
    fn webview2_data_dir_lives_inside_the_multibot_tree() {
        let dir = webview2_data_dir();
        assert!(dir.ends_with(WEBVIEW2_DIR_NAME), "unexpected dir: {dir:?}");
        assert!(
            data_dir_is_isolated(&dir),
            "data dir must stay isolated: {dir:?}"
        );
        let lower = dir.to_string_lossy().to_ascii_lowercase();
        assert!(
            !lower.contains("zigshell"),
            "leaked legacy profile path: {dir:?}"
        );
    }

    #[test]
    fn webview2_data_dir_honors_the_isolated_profile_override() {
        // A child process keeps the override away from the parallel test process.
        let dir = std::env::temp_dir().join("multibot-computer-view-profile-test");
        if std::env::var_os("MULTIBOT_COMPUTER_VIEW_TEST_CHILD").is_some() {
            let resolved = webview2_data_dir();
            assert!(resolved.starts_with(&dir), "override ignored: {resolved:?}");
            assert!(resolved.ends_with(WEBVIEW2_DIR_NAME));
            return;
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "computer_view::tests::webview2_data_dir_honors_the_isolated_profile_override",
            ])
            .env("MULTIBOT_DATA_DIR", &dir)
            .env("MULTIBOT_COMPUTER_VIEW_TEST_CHILD", "1")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn closed_pane_owns_no_backend_handle() {
        let state = ComputerViewState::default();
        assert!(!state.is_open());
        assert_eq!(state.backend_handle_count(), 0);
        assert!(state.backend().is_none());
        assert!(state.open().is_none());
    }

    #[test]
    fn backend_labels_and_process_ownership_are_stable() {
        assert_eq!(ComputerBackend::WebView2.label(), "WebView2");
        assert_eq!(ComputerBackend::Preview.label(), "Preview");
        assert!(ComputerBackend::WebView2.owns_process());
        assert!(!ComputerBackend::Preview.owns_process());
        assert_eq!(
            current_backend(),
            if cfg!(target_os = "windows") {
                ComputerBackend::WebView2
            } else {
                ComputerBackend::Preview
            }
        );
    }

    #[test]
    fn status_lines_read_plainly() {
        assert_eq!(ComputerStatus::NeedsUrl.label(), "Enter a URL to start");
        assert_eq!(ComputerStatus::Loaded.label(), "Connected");
        assert_eq!(
            ComputerStatus::Failed("boom".into()).label(),
            "Could not connect: boom"
        );
    }

    #[test]
    fn url_errors_say_what_to_do_next() {
        assert_eq!(UrlError::Empty.message(), "Enter a URL to connect.");
        assert_eq!(
            UrlError::NotAllowed.message(),
            "Only http:// and https:// links are allowed."
        );
        assert_eq!(UrlError::MissingHost.message(), "That URL has no host.");
    }

    #[test]
    fn detached_backend_refuses_work_and_holds_no_dir() {
        let mut backend = BackendState::Detached;
        assert!(!backend.is_live());
        assert!(backend.navigate("http://localhost:6080").is_err());
        assert!(backend.reload().is_err());
        assert!(backend.data_dir().is_none());
        backend.teardown();
        assert!(!backend.is_live());
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn preview_backend_tracks_bounds_and_is_torn_down() {
        let mut backend = BackendState::Preview(PreviewBackend::new());
        assert!(backend.is_live());
        backend.set_bounds((640.0, 480.0));
        match &backend {
            BackendState::Preview(preview) => assert_eq!(preview.bounds(), Some((640.0, 480.0))),
            _ => panic!("expected the preview backend"),
        }
        assert!(backend.navigate("http://localhost:6080").is_ok());
        assert!(backend.reload().is_ok());
        backend.teardown();
        assert!(!backend.is_live());
    }
}

#[cfg(all(test, target_os = "windows"))]
mod windows_tests {
    use super::*;

    #[test]
    fn webview2_bounds_convert_to_physical_pixels() {
        let rect = windows_backend::rect_from_bounds((300.0, 200.0), 2.0);
        let size = match rect.size {
            wry::dpi::Size::Physical(size) => size,
            other => panic!("expected a physical size, got {other:?}"),
        };
        assert_eq!((size.width, size.height), (600, 400));
    }

    #[test]
    fn zero_bounds_never_ask_for_a_zero_sized_window() {
        let rect = windows_backend::rect_from_bounds((0.0, 0.0), 1.0);
        let size = match rect.size {
            wry::dpi::Size::Physical(size) => size,
            other => panic!("expected a physical size, got {other:?}"),
        };
        assert_eq!((size.width, size.height), (1, 1));
    }
}
