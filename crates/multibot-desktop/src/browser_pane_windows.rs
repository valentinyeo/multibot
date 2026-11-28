use anyhow::{Context as _, Result, bail};
use multibot_core::browser_pane::{
    BrowserPaneRect, beacon_url, browser_profile_root, client_to_screen_rect, discover_browser,
    launch_args_with_bounds, locked_popup_style, shared_profile, toolwindow_ex_style,
    window_matches_beacon,
};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    io,
    os::windows::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    sync::{
        Arc, LazyLock, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{mpsc::UnboundedSender, oneshot};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::{Dwm::*, Gdi::*},
    Storage::Xps::{PW_CLIENTONLY, PrintWindow},
    System::{JobObjects::*, LibraryLoader::GetModuleHandleW, Threading::*},
    UI::{
        Accessibility::*,
        Input::KeyboardAndMouse::{GetAsyncKeyState, SetFocus},
        WindowsAndMessaging::*,
    },
};
use windows_sys::core::BOOL;

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

static LAUNCH_GENERATION: AtomicU64 = AtomicU64::new(1);
// The OS closes this process-lifetime handle on exit, killing the entire shared browser job.
static JOB: OnceLock<Result<usize, String>> = OnceLock::new();

fn shared_job() -> Result<HANDLE> {
    JOB.get_or_init(|| unsafe {
        let job = Handle(CreateJobObjectW(null(), null()));
        if job.0.is_null() {
            return Err(io::Error::last_os_error().to_string());
        }
        let mut limit = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limit.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            &limit as *const _ as _,
            size_of_val(&limit) as u32,
        ) == 0
        {
            return Err(io::Error::last_os_error().to_string());
        }
        let handle = job.0 as usize;
        std::mem::forget(job);
        Ok(handle)
    })
    .as_ref()
    .map(|handle| *handle as HANDLE)
    .map_err(|error| anyhow::anyhow!(error.clone()))
}

#[derive(Default)]
struct LaunchWindow {
    cancelled: AtomicBool,
    hwnd: AtomicUsize,
}

pub(super) struct Process {
    handle: usize,
    parent: usize,
    nonce: String,
    before: HashSet<usize>,
    window: Arc<LaunchWindow>,
}

impl Drop for Process {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle as HANDLE) };
    }
}

pub(super) struct Browser {
    hwnd: Option<usize>,
    rect: Option<BrowserPaneRect>,
    parent: usize,
    window: Arc<LaunchWindow>,
    pub(super) nonce: String,
    pub(super) cdp: crate::browser_cdp::BrowserController,
    pub(super) binding_id: String,
    pub(super) bridge: Option<(
        String,
        String,
        UnboundedSender<multibot_proto::ClientRequest>,
    )>,
}
fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}

// Chromium parses its command line with Windows quoting rules. Keep native
// UTF-16 paths intact, including paths that cannot be represented as UTF-8.
fn command_line(program: &Path, args: &[OsString]) -> Vec<u16> {
    let quote = b'"' as u16;
    let slash = b'\\' as u16;
    let mut command = Vec::new();
    for arg in std::iter::once(program.as_os_str()).chain(args.iter().map(OsString::as_os_str)) {
        if !command.is_empty() {
            command.push(b' ' as u16);
        }
        command.push(quote);
        let mut slashes = 0;
        for unit in arg.encode_wide() {
            if unit == slash {
                slashes += 1;
                continue;
            }
            command.extend(std::iter::repeat_n(slash, slashes));
            if unit == quote {
                command.extend(std::iter::repeat_n(slash, slashes + 1));
            }
            slashes = 0;
            command.push(unit);
        }
        command.extend(std::iter::repeat_n(slash, slashes * 2));
        command.push(quote);
    }
    command.push(0);
    command
}

fn real_directory(path: &Path) -> Result<()> {
    match std::fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            let meta = std::fs::symlink_metadata(path)?;
            if !meta.is_dir() || meta.file_attributes() & 0x400 != 0 {
                bail!("Browser profile directory is redirected");
            }
            Ok(())
        }
        Err(err) => Err(err.into()),
    }
}

fn isolated_root() -> Result<PathBuf> {
    let roaming = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .context("APPDATA is unavailable")?;
    let root = browser_profile_root(&roaming);
    real_directory(&roaming.join("Multibot"))?;
    real_directory(&root)?;
    Ok(root)
}

impl Browser {
    pub(super) fn launch(parent: usize, rect: Option<BrowserPaneRect>) -> Result<(Self, Process)> {
        let roots = ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
            .into_iter()
            .filter_map(std::env::var_os)
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        let program = discover_browser(&roots, Path::is_file)
            .context("Install Google Chrome or Microsoft Edge to use the browser pane")?;
        let profile = shared_profile(&isolated_root()?);
        real_directory(&profile)?;
        let generation = LAUNCH_GENERATION.fetch_add(1, Ordering::Relaxed);
        let nonce = format!("MBPANE-{}-{generation}", std::process::id());
        let screen = rect.and_then(|rect| screen_rect(parent, rect));
        let mut command = command_line(
            &program,
            &launch_args_with_bounds(&profile, &beacon_url(&nonce), screen),
        );
        let program = wide(program.as_os_str());
        let job = shared_job()?;
        let before = chrome_windows().into_iter().collect();
        let window = Arc::new(LaunchWindow::default());
        // Assign before Chromium starts children. Later launchers may forward and exit normally.
        unsafe {
            let mut startup = STARTUPINFOW::default();
            startup.cb = size_of_val(&startup) as u32;
            startup.dwFlags = STARTF_USESHOWWINDOW;
            startup.wShowWindow = SW_SHOWNOACTIVATE as u16;
            let mut info = PROCESS_INFORMATION::default();
            if CreateProcessW(
                program.as_ptr(),
                command.as_mut_ptr(),
                null(),
                null(),
                0,
                CREATE_SUSPENDED,
                null(),
                null(),
                &startup,
                &mut info,
            ) == 0
            {
                return Err(io::Error::last_os_error().into());
            }
            let thread = Handle(info.hThread);
            let process = Process {
                handle: info.hProcess as usize,
                parent,
                nonce: nonce.clone(),
                before,
                window: window.clone(),
            };
            if AssignProcessToJobObject(job, info.hProcess) == 0 {
                let error = io::Error::last_os_error();
                TerminateProcess(info.hProcess, 1);
                return Err(error.into());
            }
            if ResumeThread(thread.0) == u32::MAX {
                let error = io::Error::last_os_error();
                TerminateProcess(info.hProcess, 1);
                return Err(error.into());
            }
            Ok((
                Self {
                    hwnd: None,
                    rect: None,
                    parent,
                    window,
                    nonce: nonce.clone(),
                    cdp: crate::browser_cdp::BrowserController::new(profile),
                    binding_id: nonce,
                    bridge: None,
                },
                process,
            ))
        }
    }

    pub(super) fn bind(
        &mut self,
        host: String,
        session_id: String,
        tx: UnboundedSender<multibot_proto::ClientRequest>,
    ) {
        let _ = tx.send(multibot_proto::ClientRequest::BrowserBind {
            session_id: session_id.clone(),
            binding_id: self.binding_id.clone(),
        });
        self.bridge = Some((host, session_id, tx));
    }

    pub(super) fn attach(&mut self, window: usize) -> Result<()> {
        let hwnd = window as HWND;
        unsafe {
            if self.window.hwnd.load(Ordering::SeqCst) != window || IsWindow(hwnd) == 0 {
                bail!("Browser window closed before docking");
            }
            ShowWindowAsync(hwnd, SW_HIDE);
            set_window_long(
                hwnd,
                GWL_STYLE,
                locked_popup_style(GetWindowLongPtrW(hwnd, GWL_STYLE) as u32) as isize,
            )?;
            set_window_long(
                hwnd,
                GWL_EXSTYLE,
                toolwindow_ex_style(GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32) as isize,
            )?;
            set_window_long(hwnd, GWLP_HWNDPARENT, self.parent as isize)?;
            let corners = 1u32;
            let border = 0xFFFFFFFEu32;
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                &corners as *const _ as _,
                size_of_val(&corners) as u32,
            );
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_BORDER_COLOR as u32,
                &border as *const _ as _,
                size_of_val(&border) as u32,
            );
            SetWindowPos(
                hwnd,
                null_mut(),
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        self.hwnd = Some(window);
        Ok(())
    }

    pub(super) fn place(&mut self, rect: Option<BrowserPaneRect>) {
        let rect = rect.filter(|rect| rect.width > 0 && rect.height > 0);
        if self.rect == rect {
            return;
        }
        let Some(hwnd) = self.hwnd else { return };
        self.rect = rect;
        if self.window.hwnd.load(Ordering::SeqCst) != hwnd {
            return;
        }
        if let Some(dock) = DOCKED_BY_BROWSER_HWND.lock().unwrap().get_mut(&hwnd) {
            dock.client_rect = rect;
        }
        if let Some(rect) = rect {
            position(self.parent, hwnd, rect);
        } else {
            unsafe {
                ShowWindowAsync(hwnd as HWND, SW_HIDE);
            }
        }
    }

    pub(super) fn visible_window(&self) -> Option<usize> {
        self.rect
            .and(self.hwnd)
            .filter(|hwnd| self.window.hwnd.load(Ordering::SeqCst) == *hwnd)
    }

    pub(super) fn close(&mut self) {
        self.window.cancelled.store(true, Ordering::SeqCst);
        self.place(None);
        let hwnd = self.window.hwnd.load(Ordering::SeqCst);
        if hwnd != 0 {
            unsafe {
                PostMessageW(hwnd as HWND, WM_CLOSE, 0, 0);
            }
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        self.close();
        if let Some((_, session_id, tx)) = self.bridge.take() {
            let _ = tx.send(multibot_proto::ClientRequest::BrowserUnbind {
                session_id,
                binding_id: self.binding_id.clone(),
            });
        }
    }
}

fn set_window_long(hwnd: HWND, index: WINDOW_LONG_PTR_INDEX, value: isize) -> Result<()> {
    unsafe {
        SetLastError(0);
        if SetWindowLongPtrW(hwnd, index, value) == 0 && GetLastError() != 0 {
            return Err(io::Error::last_os_error().into());
        }
    }
    Ok(())
}

fn chrome_windows() -> Vec<usize> {
    unsafe extern "system" fn enumerate(hwnd: HWND, data: LPARAM) -> BOOL {
        unsafe {
            let mut class = [0u16; 64];
            let len = GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32);
            if len > 0 && String::from_utf16_lossy(&class[..len as usize]) == "Chrome_WidgetWin_1" {
                (*(data as *mut Vec<usize>)).push(hwnd as usize);
            }
        }
        1
    }
    let mut windows = Vec::new();
    unsafe {
        EnumWindows(Some(enumerate), &mut windows as *mut Vec<usize> as LPARAM);
    }
    windows
}

impl Process {
    pub(super) fn find_window(&self, exit_tx: oneshot::Sender<()>) -> Result<usize> {
        let start = Instant::now();
        // Startup-only discovery runs off the UI thread. Lifetime tracking uses WinEvents.
        while start.elapsed() < Duration::from_secs(10) {
            let cancelled = self.window.cancelled.load(Ordering::SeqCst);
            for hwnd in chrome_windows() {
                if self.before.contains(&hwnd)
                    || DOCKED_BY_BROWSER_HWND.lock().unwrap().contains_key(&hwnd)
                {
                    continue;
                }
                let mut rect = RECT::default();
                let mut title = [0u16; 2048];
                let (title, width, height, fallback) = unsafe {
                    if GetWindowRect(hwnd as HWND, &mut rect) == 0 {
                        continue;
                    }
                    let len = GetWindowTextW(hwnd as HWND, title.as_mut_ptr(), title.len() as i32);
                    let title = String::from_utf16_lossy(&title[..len.max(0) as usize]);
                    let fallback = !cancelled
                        && start.elapsed() >= Duration::from_secs(2)
                        && IsWindowVisible(hwnd as HWND) != 0
                        && GetWindow(hwnd as HWND, GW_OWNER).is_null()
                        && !title.is_empty()
                        && !title.contains("MBPANE-");
                    (
                        title,
                        rect.right - rect.left,
                        rect.bottom - rect.top,
                        fallback,
                    )
                };
                if !window_matches_beacon("Chrome_WidgetWin_1", &title, &self.nonce, width, height)
                    && !(fallback && width >= 100 && height >= 100)
                {
                    continue;
                }
                let mut docks = DOCKED_BY_BROWSER_HWND.lock().unwrap();
                if docks.contains_key(&hwnd) {
                    continue;
                }
                docks.insert(
                    hwnd,
                    Dock {
                        parent: self.parent,
                        client_rect: None,
                        exit_tx: Some(exit_tx),
                        window: self.window.clone(),
                    },
                );
                self.window.hwnd.store(hwnd, Ordering::SeqCst);
                drop(docks);
                if unsafe { IsWindow(hwnd as HWND) } == 0 {
                    DOCKED_BY_BROWSER_HWND.lock().unwrap().remove(&hwnd);
                    self.window.hwnd.store(0, Ordering::SeqCst);
                    bail!("Browser closed before docking");
                }
                if self.window.cancelled.load(Ordering::SeqCst) {
                    unsafe {
                        PostMessageW(hwnd as HWND, WM_CLOSE, 0, 0);
                    }
                    bail!("Browser opening cancelled");
                }
                return Ok(hwnd);
            }
            unsafe {
                let mut code = 0;
                if GetExitCodeProcess(self.handle as HANDLE, &mut code) != 0
                    && code != STILL_ACTIVE as u32
                    && code != 0
                {
                    bail!("Browser exited before opening its window (code {code})");
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        bail!("Browser window did not open")
    }
}

type ShortcutSender = UnboundedSender<&'static str>;
struct Dock {
    parent: usize,
    client_rect: Option<BrowserPaneRect>,
    exit_tx: Option<oneshot::Sender<()>>,
    window: Arc<LaunchWindow>,
}
static DOCKED_BY_BROWSER_HWND: LazyLock<Mutex<HashMap<usize, Dock>>> =
    LazyLock::new(Mutex::default);
struct VisibleDock {
    browser_hwnd: usize,
    client_rect: BrowserPaneRect,
    sender: ShortcutSender,
}
static VISIBLE: LazyLock<Mutex<HashMap<usize, VisibleDock>>> = LazyLock::new(Mutex::default);
static HOOK_THREAD: OnceLock<Result<u32, String>> = OnceLock::new();
const UPDATE_HOOKS: u32 = WM_APP + 2;
thread_local! { static HELD: RefCell<HashSet<u32>> = RefCell::default(); }

fn screen_rect(parent: usize, rect: BrowserPaneRect) -> Option<BrowserPaneRect> {
    let mut origin = POINT::default();
    if unsafe { ClientToScreen(parent as HWND, &mut origin) } == 0 {
        return None;
    }
    Some(client_to_screen_rect(rect, origin.x, origin.y))
}

fn position(parent: usize, browser: usize, rect: BrowserPaneRect) {
    unsafe {
        if IsIconic(parent as HWND) != 0 || IsWindowVisible(parent as HWND) == 0 {
            ShowWindowAsync(browser as HWND, SW_HIDE);
            return;
        }
        let Some(screen) = screen_rect(parent, rect) else {
            return;
        };
        if IsZoomed(browser as HWND) != 0 {
            ShowWindowAsync(browser as HWND, SW_RESTORE);
        }
        SetWindowPos(
            browser as HWND,
            null_mut(),
            screen.x,
            screen.y,
            screen.width,
            screen.height,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_FRAMECHANGED | SWP_ASYNCWINDOWPOS,
        );
    }
}

fn update_hooks() {
    if let Some(Ok(thread)) = HOOK_THREAD.get() {
        unsafe {
            PostThreadMessageW(*thread, UPDATE_HOOKS, 0, 0);
        }
    }
}

fn hook_thread() -> Result<u32> {
    HOOK_THREAD
        .get_or_init(|| {
            let (ready, started) = std::sync::mpsc::sync_channel(1);
            std::thread::Builder::new()
                .name("browser-window-hooks".into())
                .spawn(move || unsafe {
                    let mut message = MSG::default();
                    PeekMessageW(&mut message, null_mut(), 0, 0, PM_NOREMOVE);
                    let process = GetCurrentProcessId();
                    let events = [
                        (EVENT_OBJECT_LOCATIONCHANGE, process),
                        (EVENT_SYSTEM_MINIMIZESTART, process),
                        (EVENT_SYSTEM_MINIMIZEEND, process),
                        (EVENT_SYSTEM_MOVESIZEEND, 0),
                        (EVENT_OBJECT_DESTROY, 0),
                    ];
                    let mut hooks = Vec::new();
                    for (event, pid) in events {
                        let hook = SetWinEventHook(
                            event,
                            event,
                            null_mut(),
                            Some(win_event),
                            pid,
                            0,
                            WINEVENT_OUTOFCONTEXT,
                        );
                        if hook.is_null() {
                            let _ = ready.send(Err(io::Error::last_os_error().to_string()));
                            for hook in hooks {
                                UnhookWinEvent(hook);
                            }
                            return;
                        }
                        hooks.push(hook);
                    }
                    let _ = ready.send(Ok(GetCurrentThreadId()));
                    let mut keyboard_hook: HHOOK = null_mut();
                    while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
                        if message.message == UPDATE_HOOKS {
                            let needed = VISIBLE.lock().unwrap().keys().any(|parent| {
                                IsIconic(*parent as HWND) == 0
                                    && IsWindowVisible(*parent as HWND) != 0
                            });
                            if needed && keyboard_hook.is_null() {
                                keyboard_hook = SetWindowsHookExW(
                                    WH_KEYBOARD_LL,
                                    Some(keyboard),
                                    GetModuleHandleW(null()),
                                    0,
                                );
                                if keyboard_hook.is_null() {
                                    eprintln!(
                                        "Could not install browser shortcuts: {}",
                                        io::Error::last_os_error()
                                    );
                                }
                            } else if !needed && !keyboard_hook.is_null() {
                                UnhookWindowsHookEx(keyboard_hook);
                                keyboard_hook = null_mut();
                                HELD.with(|held| held.borrow_mut().clear());
                            }
                        } else {
                            TranslateMessage(&message);
                            DispatchMessageW(&message);
                        }
                    }
                    if !keyboard_hook.is_null() {
                        UnhookWindowsHookEx(keyboard_hook);
                    }
                    for hook in hooks {
                        UnhookWinEvent(hook);
                    }
                })
                .map_err(|error| error.to_string())?;
            started.recv().map_err(|error| error.to_string())?
        })
        .as_ref()
        .copied()
        .map_err(|error| anyhow::anyhow!(error.clone()))
}

unsafe extern "system" fn win_event(
    _: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    object: i32,
    child: i32,
    _: u32,
    _: u32,
) {
    if matches!(event, EVENT_SYSTEM_MOVESIZEEND | EVENT_OBJECT_DESTROY) {
        let mut docks = DOCKED_BY_BROWSER_HWND.lock().unwrap();
        let Some(dock) = docks.get(&(hwnd as usize)) else {
            return;
        };
        if event == EVENT_OBJECT_DESTROY {
            if object != OBJID_WINDOW || child != 0 {
                return;
            }
            let mut dock = docks.remove(&(hwnd as usize)).unwrap();
            dock.window.hwnd.store(0, Ordering::SeqCst);
            drop(docks);
            let mut visible = VISIBLE.lock().unwrap();
            if visible
                .get(&dock.parent)
                .is_some_and(|dock| dock.browser_hwnd == hwnd as usize)
            {
                visible.remove(&dock.parent);
            }
            drop(visible);
            update_hooks();
            if let Some(tx) = dock.exit_tx.take() {
                let _ = tx.send(());
            }
        } else {
            let (parent, rect) = (dock.parent, dock.client_rect);
            drop(docks);
            if let Some(rect) = rect {
                position(parent, hwnd as usize, rect);
            }
        }
        return;
    }
    if object != OBJID_WINDOW || child != 0 {
        return;
    }
    let visible = VISIBLE
        .lock()
        .unwrap()
        .get(&(hwnd as usize))
        .map(|dock| (dock.browser_hwnd, dock.client_rect));
    let Some((browser, rect)) = visible else {
        return;
    };
    position(hwnd as usize, browser, rect);
    update_hooks();
}

pub(crate) fn has_focus(parent: usize) -> bool {
    let fg = unsafe { GetForegroundWindow() };
    VISIBLE.lock().unwrap().get(&parent).is_some_and(|dock| {
        fg == dock.browser_hwnd as HWND || unsafe { IsChild(dock.browser_hwnd as HWND, fg) } != 0
    })
}

pub(super) struct Shortcuts {
    parent: usize,
    sender: ShortcutSender,
}
impl Shortcuts {
    pub(super) fn new(parent: usize, sender: ShortcutSender) -> Result<Self> {
        hook_thread()?;
        Ok(Self { parent, sender })
    }
    pub(super) fn set_visible(&self, hwnd: Option<usize>) {
        let next = hwnd.and_then(|hwnd| {
            DOCKED_BY_BROWSER_HWND
                .lock()
                .unwrap()
                .get(&hwnd)
                .and_then(|dock| dock.client_rect)
                .map(|rect| (hwnd, rect))
        });
        let mut entries = VISIBLE.lock().unwrap();
        if let Some((hwnd, rect)) = next {
            if let Some(dock) = entries.get_mut(&self.parent)
                && dock.browser_hwnd == hwnd
            {
                dock.client_rect = rect;
                return;
            }
            entries.insert(
                self.parent,
                VisibleDock {
                    browser_hwnd: hwnd,
                    client_rect: rect,
                    sender: self.sender.clone(),
                },
            );
        } else {
            entries.remove(&self.parent);
        }
        drop(entries);
        update_hooks();
    }
}
impl Drop for Shortcuts {
    fn drop(&mut self) {
        self.set_visible(None);
    }
}

unsafe extern "system" fn keyboard(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 {
            let key = (*(lparam as *const KBDLLHOOKSTRUCT)).vkCode;
            if matches!(wparam as u32, WM_KEYUP | WM_SYSKEYUP) {
                if HELD.with(|keys| keys.borrow_mut().remove(&key)) {
                    return 1;
                }
            } else if matches!(wparam as u32, WM_KEYDOWN | WM_SYSKEYDOWN) {
                if HELD.with(|keys| keys.borrow().contains(&key)) {
                    return 1;
                }
                let fg = GetForegroundWindow() as usize;
                let parent = DOCKED_BY_BROWSER_HWND
                    .lock()
                    .unwrap()
                    .get(&fg)
                    .and_then(|dock| dock.client_rect.map(|_| dock.parent));
                let sender = parent.and_then(|parent| {
                    VISIBLE
                        .lock()
                        .unwrap()
                        .get(&parent)
                        .filter(|dock| dock.browser_hwnd == fg)
                        .map(|dock| dock.sender.clone())
                });
                if let Some(sender) = sender
                    && let Some(chord) = crate::shell_host::registered_native_shortcut(
                        key,
                        GetAsyncKeyState(0x11) < 0,
                        GetAsyncKeyState(0x12) < 0,
                        GetAsyncKeyState(0x10) < 0,
                    )
                    && sender.send(chord.gpui_key).is_ok()
                {
                    HELD.with(|keys| keys.borrow_mut().insert(key));
                    return 1;
                }
            }
        }
        CallNextHookEx(null_mut(), code, wparam, lparam)
    }
}

pub(super) fn focus(hwnd: usize) {
    unsafe {
        let hwnd = hwnd as HWND;
        let thread = GetWindowThreadProcessId(hwnd, null_mut());
        let current = GetCurrentThreadId();
        let attached =
            thread != 0 && thread != current && AttachThreadInput(current, thread, 1) != 0;
        SetForegroundWindow(hwnd);
        SetFocus(hwnd);
        if attached {
            AttachThreadInput(current, thread, 0);
        }
    }
}
// Include Chromium's software-composited content in the client image.
const RENDER_FULL_CONTENT: u32 = 0x2;

pub(super) fn capture(hwnd: usize, path: &Path) -> Result<()> {
    let hwnd = hwnd as HWND;
    let mut rect = RECT::default();
    let rgb = unsafe {
        if GetClientRect(hwnd, &mut rect) == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let (width, height) = (rect.right, rect.bottom);
        if width <= 0 || height <= 0 || i64::from(width) * i64::from(height) > 32_000_000 {
            bail!("Browser viewport is outside the supported capture size");
        }
        let dc = CreateCompatibleDC(null_mut());
        if dc.is_null() {
            return Err(io::Error::last_os_error().into());
        }
        let mut info = BITMAPINFO::default();
        info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = width;
        info.bmiHeader.biHeight = -height;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        let mut pixels = null_mut();
        let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut pixels, null_mut(), 0);
        if bitmap.is_null() {
            let error = io::Error::last_os_error();
            DeleteDC(dc);
            return Err(error.into());
        }
        // Initialize the DIB even if the window paints only part of its client area.
        std::ptr::write_bytes(pixels.cast::<u8>(), 0, (width * height * 4) as usize);
        let previous = SelectObject(dc, bitmap);
        let result = if previous.is_null()
            || PrintWindow(hwnd, dc, RENDER_FULL_CONTENT | PW_CLIENTONLY) == 0
        {
            Err(anyhow::anyhow!("Windows could not capture the browser tab"))
        } else {
            let bgra =
                std::slice::from_raw_parts(pixels as *const u8, (width * height * 4) as usize);
            Ok(bgra
                .chunks_exact(4)
                .flat_map(|p| [p[2], p[1], p[0]])
                .collect::<Vec<_>>())
        };
        SelectObject(dc, previous);
        DeleteObject(bitmap);
        DeleteDC(dc);
        result?
    };
    let file = std::fs::File::create_new(path)?;
    let mut encoder = png::Encoder::new(
        std::io::BufWriter::new(file),
        rect.right as u32,
        rect.bottom as u32,
    );
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&rgb)?;
    Ok(())
}

pub(super) fn capture_directory() -> Result<tempfile::TempDir> {
    Ok(tempfile::Builder::new()
        .prefix("screenshots-")
        .tempdir_in(isolated_root()?)?)
}
