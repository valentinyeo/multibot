use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    ffi::OsString,
    path::{Path, PathBuf},
};

pub const DEFAULT_URL: &str = "about:blank";
pub const GOOGLE_SIGNIN_URL: &str = "https://accounts.google.com";

#[derive(Debug, Default)]
pub struct BrowserTab {
    pub open: bool,
    pub url: Option<String>,
    pub hibernated: bool,
    hidden_since_ms: Option<u64>,
}

#[derive(Debug)]
pub struct BrowserPaneState {
    tabs: HashMap<String, BrowserTab>,
    visible: Option<String>,
    pub split_ratio: f32,
    pub hibernated_after_ms: u64,
}

impl Default for BrowserPaneState {
    fn default() -> Self {
        Self {
            tabs: HashMap::new(),
            visible: None,
            split_ratio: 0.45,
            hibernated_after_ms: 5 * 60 * 1_000,
        }
    }
}

impl BrowserPaneState {
    pub fn tab(&self, session: &str) -> Option<&BrowserTab> {
        self.tabs.get(session)
    }

    pub fn is_open(&self, session: &str) -> bool {
        self.tab(session).is_some_and(|tab| tab.open)
    }

    pub fn toggle(&mut self, session: &str) -> bool {
        let tab = self.tabs.entry(session.to_owned()).or_default();
        tab.open = !tab.open;
        if !tab.open {
            tab.hidden_since_ms = None;
        }
        tab.open
    }

    pub fn close_all(&mut self) {
        for tab in self.tabs.values_mut() {
            tab.open = false;
            tab.hidden_since_ms = None;
        }
        self.visible = None;
    }

    pub fn set_url(&mut self, session: &str, url: String) {
        self.tabs.entry(session.to_owned()).or_default().url = Some(url);
    }

    pub fn select_visible(&mut self, session: Option<&str>, now_ms: u64) {
        let next = session.filter(|key| self.is_open(key));
        if self.visible.as_deref() == next {
            return;
        }
        if let Some(tab) = self.visible.take().and_then(|key| self.tabs.get_mut(&key)) {
            tab.hidden_since_ms = tab.open.then_some(now_ms);
        }
        if let Some(key) = next {
            let tab = self.tabs.get_mut(key).expect("open tab exists");
            tab.hidden_since_ms = None;
            tab.hibernated = false;
            self.visible = Some(key.to_owned());
        }
    }

    pub fn release_all_tabs(&mut self) {
        self.visible = None;
        for tab in self.tabs.values_mut() {
            tab.hidden_since_ms = None;
            tab.hibernated = tab.open;
        }
    }

    pub fn next_hibernation_ms(&self) -> Option<u64> {
        self.tabs
            .values()
            .filter_map(|tab| {
                tab.hidden_since_ms
                    .map(|time| time.saturating_add(self.hibernated_after_ms))
            })
            .min()
    }

    pub fn hibernate_idle(&mut self, now_ms: u64) -> impl Iterator<Item = &str> {
        let timeout = self.hibernated_after_ms;
        self.tabs.iter_mut().filter_map(move |(key, tab)| {
            let expired = tab
                .hidden_since_ms
                .is_some_and(|hidden| now_ms.saturating_sub(hidden) >= timeout);
            if expired {
                tab.hidden_since_ms = None;
                tab.hibernated = true;
            }
            expired.then_some(key.as_str())
        })
    }
}

/// Derives the browser root from the platform's roaming configuration directory.
pub fn browser_profile_root(roaming_app_data: &Path) -> PathBuf {
    roaming_app_data.join("Multibot").join("browser-profile")
}

/// Session keys include server-provided names. A hash keeps them out of path components.
pub fn session_profile(root: &Path, session: &str) -> PathBuf {
    root.join(format!("{:x}", Sha256::digest(session.as_bytes())))
}

/// Chrome first, Edge second. Search explicit installation roots and avoid PATH/current directory.
pub fn discover_browser(roots: &[PathBuf], is_file: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    [
        "Google/Chrome/Application/chrome.exe",
        "Microsoft/Edge/Application/msedge.exe",
    ]
    .into_iter()
    .flat_map(|relative| {
        roots
            .iter()
            .filter(|root| root.is_absolute())
            .map(move |root| root.join(relative))
    })
    .find(|path| is_file(path))
}

pub fn shared_profile(root: &Path) -> PathBuf {
    root.join("default")
}

pub fn beacon_url(nonce: &str) -> String {
    // Encode the title so arbitrary nonces cannot escape the data URL's HTML.
    let escaped = nonce
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let title: String = escaped
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'-' {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    format!(
        "data:text/html,<title>{title}</title><style>html,body{{background:%230d0d0d;margin:0}}</style>"
    )
}

pub fn window_matches_beacon(
    class_name: &str,
    title: &str,
    nonce: &str,
    width: i32,
    height: i32,
) -> bool {
    class_name == "Chrome_WidgetWin_1"
        && !nonce.is_empty()
        && title.contains(nonce)
        && width > 0
        && height > 0
}

pub fn locked_popup_style(style: u32) -> u32 {
    // Win32 style values stay here so docking policy can be tested on Linux.
    (style
        & !(0x00C00000
            | 0x00040000
            | 0x00800000
            | 0x00080000
            | 0x00020000
            | 0x00010000
            | 0x40000000))
        | 0x80000000
}

pub fn toolwindow_ex_style(ex_style: u32) -> u32 {
    (ex_style & !0x00040000) | 0x00000080
}

pub fn client_to_screen_rect(
    rect: BrowserPaneRect,
    client_origin_x: i32,
    client_origin_y: i32,
) -> BrowserPaneRect {
    BrowserPaneRect {
        x: rect.x.saturating_add(client_origin_x),
        y: rect.y.saturating_add(client_origin_y),
        ..rect
    }
}

pub fn signin_launch_args(profile: &Path) -> Vec<OsString> {
    let mut profile_arg = OsString::from("--user-data-dir=");
    profile_arg.push(profile);
    vec![profile_arg, GOOGLE_SIGNIN_URL.into()]
}

pub fn launch_args(profile: &Path, url: &str) -> Vec<OsString> {
    launch_args_with_bounds(profile, url, None)
}

pub fn launch_args_with_bounds(
    profile: &Path,
    url: &str,
    screen_rect: Option<BrowserPaneRect>,
) -> Vec<OsString> {
    let mut profile_arg = OsString::from("--user-data-dir=");
    profile_arg.push(profile);
    let mut args = vec![
        profile_arg,
        "--remote-debugging-address=127.0.0.1".into(),
        "--remote-debugging-port=0".into(),
        "--disable-gpu".into(),
        "--disable-gpu-compositing".into(),
        "--disable-accelerated-video-decode".into(),
        "--disable-background-mode".into(),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--hide-crash-restore-bubble".into(),
    ];
    if let Some(rect) = screen_rect.filter(|rect| rect.width > 0 && rect.height > 0) {
        args.push(format!("--window-position={},{}", rect.x, rect.y).into());
        args.push(format!("--window-size={},{}", rect.width, rect.height).into());
    }
    args.extend(["--new-window".into(), url.into()]);
    args
}

/// Chromium writes the ephemeral loopback port and the browser WebSocket path.
pub fn parse_devtools_active_port(contents: &str) -> Option<(u16, &str)> {
    let mut lines = contents.lines();
    let port = lines
        .next()?
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)?;
    let path = lines.next()?;
    let id = path.strip_prefix("/devtools/browser/")?;
    if id.is_empty() || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') {
        return None;
    }
    if lines.any(|line| !line.is_empty()) {
        return None;
    }
    Some((port, path))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserPaneRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl BrowserPaneRect {
    pub fn split_width(width: f32, ratio: f32) -> f32 {
        width.max(0.) * ratio.clamp(0., 1.)
    }

    pub fn from_bounds(x: f32, y: f32, width: f32, height: f32, scale: f32) -> Self {
        let pixels = |value: f32| (value * scale.max(0.)).round() as i32;
        let left = pixels(x);
        let top = pixels(y);
        Self {
            x: left,
            y: top,
            width: pixels(x + width.max(0.)).saturating_sub(left),
            height: pixels(y + height.max(0.)).saturating_sub(top),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signin_uses_only_shared_profile_and_google_url() {
        let profile = shared_profile(&browser_profile_root(Path::new("roaming path")));
        let args = signin_launch_args(&profile);
        let mut profile_arg = OsString::from("--user-data-dir=");
        profile_arg.push(&profile);
        assert_eq!(args, vec![profile_arg, OsString::from(GOOGLE_SIGNIN_URL)]);
    }

    #[test]
    fn signin_has_no_debugging_automation_or_sync_disabling_flags() {
        for arg in signin_launch_args(Path::new("profile")) {
            let arg = arg.to_string_lossy();
            for forbidden in [
                "--remote-debugging",
                "--enable-automation",
                "--disable-sync",
                "--load-extension",
            ] {
                assert!(!arg.starts_with(forbidden), "forbidden sign-in flag: {arg}");
            }
        }
    }

    #[test]
    fn closing_all_tabs_preserves_urls_and_allows_immediate_reopen() {
        let mut state = BrowserPaneState::default();
        state.toggle("a");
        state.toggle("b");
        state.set_url("a", "https://example.com".into());
        state.select_visible(Some("a"), 0);
        state.select_visible(Some("b"), 10);
        state.close_all();
        assert!(!state.is_open("a"));
        assert!(!state.is_open("b"));
        assert_eq!(state.next_hibernation_ms(), None);
        assert_eq!(
            state.tab("a").unwrap().url.as_deref(),
            Some("https://example.com")
        );
        assert!(state.toggle("a"));
    }

    #[test]
    fn shared_profile_is_always_inside_multibot() {
        let root = browser_profile_root(Path::new("roaming"));
        assert_eq!(
            shared_profile(&root),
            Path::new("roaming/Multibot/browser-profile/default")
        );
    }

    #[test]
    fn beacon_only_matches_a_real_window_for_this_launch() {
        let nonce = "MBPANE-123-4";
        assert!(beacon_url(nonce).contains(&format!("<title>{nonce}</title>")));
        assert!(beacon_url(nonce).contains("background:%230d0d0d"));
        assert!(beacon_url("</title>").contains("%26lt%3B%2Ftitle%26gt%3B"));
        assert!(window_matches_beacon(
            "Chrome_WidgetWin_1",
            &format!("{nonce} - Chrome"),
            nonce,
            800,
            600
        ));
        for (class, title, nonce, w, h) in [
            ("Other", nonce, nonce, 800, 600),
            ("Chrome_WidgetWin_1", "Other", nonce, 800, 600),
            ("Chrome_WidgetWin_1", nonce, "", 800, 600),
            ("Chrome_WidgetWin_1", nonce, nonce, 0, 600),
            ("Chrome_WidgetWin_1", nonce, nonce, 800, -1),
        ] {
            assert!(!window_matches_beacon(class, title, nonce, w, h));
        }
    }

    #[test]
    fn popup_has_no_native_frame_and_is_not_a_child_or_taskbar_window() {
        let removed = 0x00C00000
            | 0x00040000
            | 0x00800000
            | 0x00080000
            | 0x00020000
            | 0x00010000
            | 0x40000000;
        assert_eq!(locked_popup_style(removed | 0x10000000), 0x90000000);
        assert_eq!(locked_popup_style(0), 0x80000000);
        assert_eq!(toolwindow_ex_style(0x00040000 | 0x00000008), 0x00000088);
    }

    #[test]
    fn pane_coordinates_and_launch_bounds_support_negative_screen_origins() {
        let client = BrowserPaneRect {
            x: 400,
            y: 44,
            width: 600,
            height: 700,
        };
        let screen = client_to_screen_rect(client, -1920, 100);
        assert_eq!(
            screen,
            BrowserPaneRect {
                x: -1520,
                y: 144,
                ..client
            }
        );
        let args = launch_args_with_bounds(Path::new("profile"), DEFAULT_URL, Some(screen));
        assert!(args.contains(&"--window-position=-1520,144".into()));
        assert!(args.contains(&"--window-size=600,700".into()));
        assert_eq!(&args[args.len() - 2..], ["--new-window", DEFAULT_URL]);
        assert_eq!(
            launch_args_with_bounds(
                Path::new("profile"),
                DEFAULT_URL,
                Some(BrowserPaneRect { width: 0, ..client })
            ),
            launch_args(Path::new("profile"), DEFAULT_URL)
        );
    }

    #[test]
    fn devtools_active_port_parses_only_loopback_port_and_browser_path() {
        assert_eq!(
            parse_devtools_active_port("9222\n/devtools/browser/abc-123\n"),
            Some((9222, "/devtools/browser/abc-123"))
        );
        assert_eq!(
            parse_devtools_active_port("9222\r\n/devtools/browser/abc-123\r\n"),
            Some((9222, "/devtools/browser/abc-123"))
        );
        for invalid in [
            "",
            "0\n/devtools/browser/id",
            "65536\n/devtools/browser/id",
            "localhost:9222\n/devtools/browser/id",
            "9222\nws://evil",
            "9222\n/devtools/browser/",
            "9222\n/devtools/browser/id?x=1",
            "9222\n/devtools/browser/id\nextra",
        ] {
            assert!(parse_devtools_active_port(invalid).is_none(), "{invalid:?}");
        }
    }

    #[test]
    fn each_chat_remembers_its_url_and_toggle() {
        let mut state = BrowserPaneState::default();
        assert!(state.toggle("host-a/chat"));
        state.set_url("host-a/chat", "https://example.com".into());
        assert!(!state.is_open("host-b/chat"));
        assert!(state.toggle("host-b/chat"));
        assert!(!state.toggle("host-a/chat"));
        assert!(state.is_open("host-b/chat"));
        assert!(state.toggle("host-a/chat"));
        assert_eq!(
            state.tab("host-a/chat").unwrap().url.as_deref(),
            Some("https://example.com")
        );
    }

    #[test]
    fn switching_back_cancels_hibernation_and_reopening_wakes_a_tab() {
        let mut state = BrowserPaneState::default();
        state.hibernated_after_ms = 100;
        state.toggle("a");
        state.toggle("b");
        state.select_visible(Some("a"), 0);
        state.select_visible(Some("b"), 20);
        assert_eq!(state.next_hibernation_ms(), Some(120));
        assert_eq!(state.hibernate_idle(119).count(), 0);
        state.select_visible(Some("a"), 119);
        assert_eq!(state.hibernate_idle(120).count(), 0);
        assert_eq!(state.hibernate_idle(219).collect::<Vec<_>>(), ["b"]);
        assert!(state.tab("b").unwrap().hibernated);
        state.select_visible(Some("b"), 220);
        assert!(!state.tab("b").unwrap().hibernated);
        assert!(state.is_open("b"));
    }

    #[test]
    fn turning_the_pane_off_releases_background_tabs_without_losing_their_addresses() {
        let mut state = BrowserPaneState::default();
        state.toggle("a");
        state.set_url("a", "https://example.com".into());
        state.toggle("b");
        state.select_visible(Some("a"), 0);
        state.select_visible(Some("b"), 1);
        assert!(state.next_hibernation_ms().is_some());

        assert!(!state.toggle("b"));
        state.release_all_tabs();
        assert_eq!(state.next_hibernation_ms(), None);
        assert!(!state.is_open("b"));
        let parked = state.tab("a").unwrap();
        assert!(parked.open && parked.hibernated);
        assert_eq!(parked.url.as_deref(), Some("https://example.com"));
        state.select_visible(Some("a"), 2);
        assert!(!state.tab("a").unwrap().hibernated);
    }

    #[test]
    fn profile_paths_keep_session_names_out_of_the_filesystem() {
        let roaming = Path::new("roaming");
        let root = browser_profile_root(roaming);
        assert_eq!(root, roaming.join("Multibot/browser-profile"));
        for session in ["../ZigShell", "..\\Multiprompt", "/legacy", "C:\\legacy"] {
            let profile = session_profile(&root, session);
            assert_eq!(profile.parent(), Some(root.as_path()));
            assert_eq!(profile.file_name().unwrap().len(), 64);
            let args = launch_args(&profile, DEFAULT_URL);
            assert!(
                args[0]
                    .to_string_lossy()
                    .ends_with(&*profile.to_string_lossy())
            );
            assert!(args.contains(&"--disable-gpu".into()));
        }
        assert_ne!(session_profile(&root, "a"), session_profile(&root, "b"));
    }

    #[test]
    fn discovery_prefers_chrome_across_roots_and_falls_back_to_edge() {
        let root = std::env::current_dir().unwrap().join("Program Files");
        let local = std::env::current_dir().unwrap().join("LocalAppData");
        assert!(discover_browser(&[PathBuf::from(".")], |_| true).is_none());
        assert_eq!(
            discover_browser(&[root.clone(), local.clone()], |path| {
                path == root.join("Microsoft/Edge/Application/msedge.exe")
                    || path == local.join("Google/Chrome/Application/chrome.exe")
            }),
            Some(local.join("Google/Chrome/Application/chrome.exe"))
        );
        assert!(
            discover_browser(std::slice::from_ref(&root), |path| path
                .ends_with("msedge.exe"))
            .unwrap()
            .ends_with("msedge.exe")
        );
        assert!(discover_browser(&[root], |_| false).is_none());
    }

    #[test]
    fn launch_uses_a_full_browser_window_with_software_rendering() {
        for url in [DEFAULT_URL, "https://example.com/path?q=hello%20world"] {
            let args = launch_args_with_bounds(Path::new("profile"), url, None);
            assert_eq!(&args[args.len() - 2..], ["--new-window", url]);
            assert!(
                !args
                    .iter()
                    .any(|arg| arg.to_string_lossy().starts_with("--app="))
            );
            for flag in [
                "--user-data-dir=profile",
                "--remote-debugging-address=127.0.0.1",
                "--remote-debugging-port=0",
                "--disable-gpu",
                "--disable-gpu-compositing",
                "--disable-accelerated-video-decode",
                "--disable-background-mode",
                "--no-first-run",
                "--no-default-browser-check",
                "--hide-crash-restore-bubble",
            ] {
                assert!(args.contains(&flag.into()), "missing {flag}");
            }
        }
    }

    #[test]
    fn split_and_scaled_edges() {
        assert_eq!(
            BrowserPaneRect::split_width(900., BrowserPaneState::default().split_ratio),
            405.
        );
        assert_eq!(
            BrowserPaneRect::from_bounds(0.3, 44., 10.3, 20., 1.25),
            BrowserPaneRect {
                x: 0,
                y: 55,
                width: 13,
                height: 25
            }
        );
        assert_eq!(BrowserPaneRect::from_bounds(10., 44., -1., 0., 2.).width, 0);
    }
}
