use super::{
    ShellLaunchSpec, ShellShortcut, ShellViewportRect, compute_console_grid, console_paste_chunks,
    console_wheel_params, shell_hooks_needed, shell_mouse_hook_interested, shell_shortcut,
    windows_quote,
};
use anyhow::{Result, bail};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ptr::{null, null_mut};
use std::sync::{
    Arc, LazyLock, Mutex, Weak,
    atomic::{AtomicBool, AtomicU32, Ordering},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, WAIT_OBJECT_0, WPARAM},
    Graphics::Gdi::ScreenToClient,
    System::{Console::*, JobObjects::*, LibraryLoader::GetModuleHandleW, Threading::*},
    UI::{
        Input::KeyboardAndMouse::{GetAsyncKeyState, SetFocus},
        WindowsAndMessaging::*,
    },
};

pub(super) struct Job(usize);
impl Drop for Job {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0 as HANDLE);
        }
    }
}
#[derive(Default)]
pub(super) struct SpawnState {
    cancelled: bool,
    process: Option<Arc<Process>>,
}

#[derive(Default)]
pub(super) struct SpawnControl(pub(super) Arc<Mutex<SpawnState>>);

impl Drop for SpawnControl {
    fn drop(&mut self) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.cancelled = true;
        if let Some(process) = &state.process {
            unsafe {
                TerminateProcess(process.0 as HANDLE, 0);
            }
        }
    }
}

pub(super) struct Process(usize);
impl Process {
    pub(super) fn wait(&self) {
        unsafe {
            WaitForSingleObject(self.0 as HANDLE, INFINITE);
        }
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0 as HANDLE);
        }
    }
}

#[derive(Clone)]
struct VisibleShell {
    hwnd: usize,
    pid: u32,
    shortcuts: tokio::sync::mpsc::UnboundedSender<gpui_kit::Keystroke>,
    urls: std::sync::mpsc::Sender<ConsoleClick>,
    pastes: std::sync::mpsc::Sender<ConsolePaste>,
    resizes: std::sync::mpsc::Sender<ConsoleResize>,
    text_input_focused: bool,
    confirming_kill: bool,
}

struct ConsolePaste {
    hwnd: usize,
    pid: u32,
    parent: usize,
}

struct ConsoleResize {
    pid: u32,
    hwnd: usize,
    width: i32,
    height: i32,
}

struct ConsoleClick {
    hwnd: usize,
    pid: u32,
    x: i32,
    y: i32,
}

thread_local! {
    static URL_CLICK: Cell<bool> = const { Cell::new(false) };
    static FORWARDED: RefCell<HashMap<u32, usize>> = RefCell::new(HashMap::new());
}

static VISIBLE: LazyLock<Arc<Mutex<HashMap<usize, VisibleShell>>>> =
    LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));
static HAS_VISIBLE_SHELL: AtomicBool = AtomicBool::new(false);
static HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);
static HOOK_ERROR: AtomicU32 = AtomicU32::new(0);
static HOOKS: Mutex<Weak<InputHooks>> = Mutex::new(Weak::new());
const UPDATE_HOOKS: u32 = WM_APP + 1;

struct InputHooks {
    thread_id: u32,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl InputHooks {
    fn shared() -> Result<Arc<Self>> {
        let mut shared = HOOKS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(hooks) = shared.upgrade() {
            return Ok(hooks);
        }
        let (ready, started) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("shell-input-hooks".into())
            .spawn(move || unsafe {
                let mut message = MSG::default();
                // PostThreadMessage requires a message queue before publishing the thread ID.
                PeekMessageW(&mut message, null_mut(), 0, 0, PM_NOREMOVE);
                let _ = ready.send(GetCurrentThreadId());
                let mut keyboard: HHOOK = null_mut();
                let mut mouse: HHOOK = null_mut();
                while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
                    if message.message == UPDATE_HOOKS {
                        if HAS_VISIBLE_SHELL.load(Ordering::Relaxed) {
                            if keyboard.is_null() {
                                keyboard = SetWindowsHookExW(
                                    WH_KEYBOARD_LL,
                                    Some(keyboard_hook),
                                    GetModuleHandleW(null()),
                                    0,
                                );
                                mouse = SetWindowsHookExW(
                                    WH_MOUSE_LL,
                                    Some(mouse_hook),
                                    GetModuleHandleW(null()),
                                    0,
                                );
                                if keyboard.is_null() || mouse.is_null() {
                                    HOOK_ERROR.store(
                                        windows_sys::Win32::Foundation::GetLastError(),
                                        Ordering::Relaxed,
                                    );
                                    UnhookWindowsHookEx(keyboard);
                                    UnhookWindowsHookEx(mouse);
                                    keyboard = null_mut();
                                    mouse = null_mut();
                                }
                            }
                        } else {
                            UnhookWindowsHookEx(keyboard);
                            UnhookWindowsHookEx(mouse);
                            keyboard = null_mut();
                            mouse = null_mut();
                            FORWARDED.with(|v| v.borrow_mut().clear());
                            URL_CLICK.set(false);
                        }
                    } else {
                        TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
                UnhookWindowsHookEx(keyboard);
                UnhookWindowsHookEx(mouse);
            })?;
        let thread_id = started.recv()?;
        HOOK_THREAD_ID.store(thread_id, Ordering::Release);
        let hooks = Arc::new(Self {
            thread_id,
            thread: Some(thread),
        });
        *shared = Arc::downgrade(&hooks);
        Ok(hooks)
    }
}

impl Drop for InputHooks {
    fn drop(&mut self) {
        HOOK_THREAD_ID.store(0, Ordering::Release);
        unsafe {
            PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn update_visible(update: impl FnOnce(&mut HashMap<usize, VisibleShell>)) {
    let mut visible = VISIBLE.lock().unwrap_or_else(|e| e.into_inner());
    update(&mut visible);
    let needed = shell_hooks_needed(visible.values().map(|shell| shell.hwnd));
    let changed = HAS_VISIBLE_SHELL.swap(needed, Ordering::Relaxed) != needed;
    drop(visible);
    if changed {
        unsafe {
            PostThreadMessageW(HOOK_THREAD_ID.load(Ordering::Acquire), UPDATE_HOOKS, 0, 0);
        }
    }
    let error = HOOK_ERROR.swap(0, Ordering::Relaxed);
    if error != 0 {
        eprintln!(
            "Could not install shell input hooks: {}",
            std::io::Error::from_raw_os_error(error as i32)
        );
    }
}

pub(super) struct NativeHost {
    pub(super) parent: usize,
    pub(super) job: Arc<Job>,
    _hooks: Arc<InputHooks>,
    pastes: std::sync::mpsc::Sender<ConsolePaste>,
}
impl NativeHost {
    pub(super) fn new(
        parent: usize,
        shortcuts: tokio::sync::mpsc::UnboundedSender<gpui_kit::Keystroke>,
    ) -> Result<Self> {
        unsafe {
            let job = CreateJobObjectW(null(), null());
            if job.is_null() {
                return Err(std::io::Error::last_os_error().into());
            }
            let job = Arc::new(Job(job as usize));
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job.0 as HANDLE,
                JobObjectExtendedLimitInformation,
                &info as *const _ as _,
                size_of_val(&info) as u32,
            ) == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            let hooks = InputHooks::shared()?;
            let (urls, requests) = std::sync::mpsc::channel::<ConsoleClick>();
            if let Err(error) = std::thread::Builder::new()
                .name("console-links".into())
                .spawn(move || {
                    for click in requests {
                        if let Some(url) = read_console_url(click) {
                            if let Err(error) = crate::external_url::open_external_url(&url) {
                                eprintln!("Could not open shell link: {error}");
                            }
                        }
                    }
                })
            {
                return Err(error.into());
            }
            let (pastes, requests) = std::sync::mpsc::channel::<ConsolePaste>();
            if let Err(error) = std::thread::Builder::new()
                .name("console-pastes".into())
                .spawn(move || {
                    for paste in requests {
                        if let Err(error) = paste_clipboard_to_console(paste) {
                            eprintln!("Could not paste into shell: {error}");
                        }
                    }
                })
            {
                return Err(error.into());
            }
            let (resizes, requests) = std::sync::mpsc::channel::<ConsoleResize>();
            if let Err(error) = std::thread::Builder::new()
                .name("console-resizes".into())
                .spawn(move || {
                    for resize in requests {
                        if let Err(error) = resize_console(resize) {
                            eprintln!("Could not resize shell console: {error}");
                        }
                    }
                })
            {
                return Err(error.into());
            }
            let host = Self {
                parent,
                job,
                _hooks: hooks,
                pastes,
            };
            update_visible(|visible| {
                visible.insert(
                    parent,
                    VisibleShell {
                        hwnd: 0,
                        pid: 0,
                        shortcuts,
                        urls,
                        pastes: host.pastes.clone(),
                        resizes,
                        text_input_focused: false,
                        confirming_kill: false,
                    },
                );
            });
            Ok(host)
        }
    }
    pub(super) fn focus_visible(&self) {
        let hwnd = VISIBLE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&self.parent)
            .map(|s| s.hwnd)
            .unwrap_or(0);
        unsafe {
            if hwnd != 0
                && IsWindowVisible(hwnd as HWND) != 0
                && GetAncestor(GetForegroundWindow(), GA_ROOT) == self.parent as HWND
                && !crate::browser_pane::native::has_focus(self.parent)
            {
                SetFocus(hwnd as HWND);
            }
        }
    }

    pub(super) fn set_text_input_focus(&self, focused: bool) {
        update_visible(|visible| {
            if let Some(shell) = visible.get_mut(&self.parent) {
                shell.text_input_focused = focused;
            }
        });
    }
    pub(super) fn set_kill_confirmation(&self, confirming: bool) {
        update_visible(|visible| {
            if let Some(shell) = visible.get_mut(&self.parent) {
                shell.confirming_kill = confirming;
            }
        });
    }
    pub(super) fn focus_parent(&self) {
        unsafe {
            SetFocus(self.parent as HWND);
        }
    }
}
impl Drop for NativeHost {
    fn drop(&mut self) {
        update_visible(|visible| {
            visible.remove(&self.parent);
        });
    }
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 || !HAS_VISIBLE_SHELL.load(Ordering::Relaxed) {
        return unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) };
    }
    unsafe {
        let event = &*(lparam as *const KBDLLHOOKSTRUCT);
        let key = event.vkCode;
        if matches!(wparam as u32, WM_KEYUP | WM_SYSKEYUP) {
            if FORWARDED.with(|v| v.borrow_mut().remove(&key)).is_some() {
                return 1;
            }
        } else if matches!(wparam as u32, WM_KEYDOWN | WM_SYSKEYDOWN) {
            let ctrl = GetAsyncKeyState(0x11) < 0;
            let alt = GetAsyncKeyState(0x12) < 0;
            let shift = GetAsyncKeyState(0x10) < 0;
            let parent = GetAncestor(GetForegroundWindow(), GA_ROOT) as usize;
            let shell = VISIBLE
                .try_lock()
                .ok()
                .and_then(|v| v.get(&parent).cloned());
            if let Some(shell) = shell.filter(|s| s.hwnd != 0) {
                let focus = thread_focus(parent as HWND);
                // Browser state is UI-thread-local; use the actual Win32 focus instead.
                let foreign_focus = !focus.is_null()
                    && focus != parent as HWND
                    && focus != shell.hwnd as HWND
                    && IsChild(shell.hwnd as HWND, focus) == 0;
                if foreign_focus {
                    return CallNextHookEx(null_mut(), code, wparam, lparam);
                }
                let shortcut = if key == 0x1B && !ctrl && !alt && !shift && shell.confirming_kill {
                    Some(ShellShortcut::Forward("escape"))
                } else {
                    shell_shortcut(key, ctrl, alt, shift)
                };
                if let Some(shortcut) = shortcut {
                    if key == 0x57
                        && ctrl
                        && shift
                        && FORWARDED.with(|v| v.borrow().contains_key(&key))
                    {
                        return 1;
                    }
                    let sent = match shortcut {
                        ShellShortcut::Paste => {
                            if shell.text_input_focused && focus != shell.hwnd as HWND {
                                return CallNextHookEx(null_mut(), code, wparam, lparam);
                            }
                            shell
                                .pastes
                                .send(ConsolePaste {
                                    hwnd: shell.hwnd,
                                    pid: shell.pid,
                                    parent,
                                })
                                .is_ok()
                        }
                        ShellShortcut::Forward(chord) => gpui_kit::Keystroke::parse(chord)
                            .is_ok_and(|stroke| shell.shortcuts.send(stroke).is_ok()),
                    };
                    if sent {
                        FORWARDED.with(|v| v.borrow_mut().insert(key, parent));
                        return 1;
                    }
                }
            }
        }
        CallNextHookEx(null_mut(), code, wparam, lparam)
    }
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if !shell_mouse_hook_interested(
        code,
        wparam as u32,
        HAS_VISIBLE_SHELL.load(Ordering::Relaxed),
    ) {
        return unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) };
    }
    unsafe {
        if wparam as u32 == WM_LBUTTONUP {
            return if URL_CLICK.replace(false) {
                1
            } else {
                CallNextHookEx(null_mut(), code, wparam, lparam)
            };
        }
        let event = &*(lparam as *const MSLLHOOKSTRUCT);
        let target = WindowFromPoint(event.pt);
        let shell = VISIBLE.try_lock().ok().and_then(|visible| {
            visible
                .iter()
                .find(|(_, shell)| {
                    let hwnd = shell.hwnd as HWND;
                    !hwnd.is_null() && (target == hwnd || IsChild(hwnd, target) != 0)
                })
                .map(|(parent, shell)| (*parent, shell.clone()))
        });
        if let Some((parent, shell)) = shell {
            let hwnd = shell.hwnd as HWND;
            // The native button-down focuses the child console. Never SetFocus across threads here.
            if wparam as u32 == WM_MOUSEWHEEL
                && GetAncestor(GetForegroundWindow(), GA_ROOT) == parent as HWND
                && thread_focus(hwnd) != hwnd
            {
                let (w, l) = console_wheel_params(
                    (event.mouseData >> 16) as i16,
                    GetAsyncKeyState(0x11) < 0,
                    GetAsyncKeyState(0x10) < 0,
                    event.pt.x,
                    event.pt.y,
                );
                if PostMessageW(hwnd, WM_MOUSEWHEEL, w, l) != 0 {
                    return 1;
                }
            }
            if wparam as u32 == WM_LBUTTONDOWN && GetAsyncKeyState(0x11) < 0 {
                let mut point = event.pt;
                if ScreenToClient(hwnd, &mut point) != 0
                    && shell
                        .urls
                        .send(ConsoleClick {
                            hwnd: shell.hwnd,
                            pid: shell.pid,
                            x: point.x,
                            y: point.y,
                        })
                        .is_ok()
                {
                    URL_CLICK.set(true);
                    return 1;
                }
            }
        }
        CallNextHookEx(null_mut(), code, wparam, lparam)
    }
}

unsafe fn thread_focus(hwnd: HWND) -> HWND {
    unsafe {
        let thread = GetWindowThreadProcessId(hwnd, null_mut());
        let mut info = GUITHREADINFO::default();
        info.cbSize = size_of_val(&info) as u32;
        if thread != 0 && GetGUIThreadInfo(thread, &mut info) != 0 {
            info.hwndFocus
        } else {
            null_mut()
        }
    }
}

fn clipboard_text() -> Result<Vec<u16>> {
    use windows_sys::Win32::{
        Globalization::{CP_ACP, MultiByteToWideChar},
        System::{
            DataExchange::*,
            Memory::{GlobalLock, GlobalSize, GlobalUnlock},
        },
    };
    unsafe {
        let mut opened = false;
        for _ in 0..10 {
            if OpenClipboard(null_mut()) != 0 {
                opened = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        if !opened {
            return Err(std::io::Error::last_os_error().into());
        }
        let result = (|| {
            let unicode = IsClipboardFormatAvailable(13) != 0; // CF_UNICODETEXT
            let handle = GetClipboardData(if unicode { 13 } else { 1 }); // CF_TEXT
            if handle.is_null() {
                return Ok(Vec::new());
            }
            let size = GlobalSize(handle);
            if size == 0 || size > 16 * 1024 * 1024 {
                bail!("Invalid clipboard text size");
            }
            let data = GlobalLock(handle);
            if data.is_null() {
                return Err(std::io::Error::last_os_error().into());
            }
            let text = (|| {
                if unicode {
                    if size % 2 != 0 {
                        bail!("Invalid UTF-16 clipboard buffer");
                    }
                    let units = std::slice::from_raw_parts(data as *const u16, size / 2);
                    let end = units
                        .iter()
                        .position(|&c| c == 0)
                        .ok_or_else(|| anyhow::anyhow!("Unterminated clipboard text"))?;
                    Ok(units[..end].to_vec())
                } else {
                    let bytes = std::slice::from_raw_parts(data as *const u8, size);
                    let end = bytes
                        .iter()
                        .position(|&c| c == 0)
                        .ok_or_else(|| anyhow::anyhow!("Unterminated clipboard text"))?;
                    if end == 0 {
                        return Ok(Vec::new());
                    }
                    let count =
                        MultiByteToWideChar(CP_ACP, 0, bytes.as_ptr(), end as i32, null_mut(), 0);
                    if count == 0 {
                        return Err(std::io::Error::last_os_error().into());
                    }
                    let mut text = vec![0; count as usize];
                    if MultiByteToWideChar(
                        CP_ACP,
                        0,
                        bytes.as_ptr(),
                        end as i32,
                        text.as_mut_ptr(),
                        count,
                    ) == 0
                    {
                        return Err(std::io::Error::last_os_error().into());
                    }
                    Ok(text)
                }
            })();
            GlobalUnlock(handle);
            text
        })();
        CloseClipboard();
        result
    }
}

fn paste_clipboard_to_console(paste: ConsolePaste) -> Result<()> {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING},
    };
    let text = clipboard_text()?;
    if text.is_empty() {
        return Ok(());
    }
    let _guard = CONSOLE_ATTACH.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        if IsWindow(paste.hwnd as HWND) == 0
            || GetParent(paste.hwnd as HWND) != paste.parent as HWND
        {
            return Ok(());
        }
        FreeConsole();
        if AttachConsole(paste.pid) == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let result = (|| {
            let name: Vec<u16> = "CONIN$\0".encode_utf16().collect();
            let input = CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                0,
                null_mut(),
            );
            if input == INVALID_HANDLE_VALUE {
                return Err(std::io::Error::last_os_error().into());
            }
            let _input = OwnedHandle::from_raw_handle(input);
            let mut selection = CONSOLE_SELECTION_INFO::default();
            if GetConsoleSelectionInfo(&mut selection) != 0 && selection.dwFlags & 0x7 != 0 {
                write_console_keys(input, &[27], 0x1B)?;
                std::thread::sleep(std::time::Duration::from_millis(30));
            }
            for chunk in console_paste_chunks(&text) {
                write_console_keys(input, &chunk, 0xE7)?; // VK_PACKET
            }
            Ok(())
        })();
        FreeConsole();
        result
    }
}

unsafe fn write_console_keys(input: HANDLE, text: &[u16], virtual_key: u16) -> Result<()> {
    unsafe {
        let mut records = Vec::with_capacity(text.len() * 2);
        for &c in text {
            for down in [1, 0] {
                let mut record = INPUT_RECORD::default();
                record.EventType = KEY_EVENT as u16;
                record.Event.KeyEvent = KEY_EVENT_RECORD {
                    bKeyDown: down,
                    wRepeatCount: 1,
                    wVirtualKeyCode: virtual_key,
                    wVirtualScanCode: 0,
                    uChar: KEY_EVENT_RECORD_0 { UnicodeChar: c },
                    dwControlKeyState: 0,
                };
                records.push(record);
            }
        }
        let mut offset = 0;
        while offset < records.len() {
            let mut written = 0;
            if WriteConsoleInputW(
                input,
                records[offset..].as_ptr(),
                (records.len() - offset) as u32,
                &mut written,
            ) == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            if written == 0 {
                bail!("Console accepted no paste input");
            }
            offset += written as usize;
        }
        Ok(())
    }
}

fn read_console_url(click: ConsoleClick) -> Option<String> {
    // A low-level mouse hook must return promptly. Console attachment and browser I/O
    // run on the event-driven reader, using the same attachment lock as shell startup.
    let _guard = CONSOLE_ATTACH.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        if IsWindow(click.hwnd as HWND) == 0 {
            return None;
        }
        FreeConsole();
        if AttachConsole(click.pid) == 0 {
            return None;
        }
        let url = (|| {
            use std::os::windows::io::{FromRawHandle, OwnedHandle};
            use windows_sys::Win32::Foundation::{GENERIC_READ, INVALID_HANDLE_VALUE};
            use windows_sys::Win32::Storage::FileSystem::{
                CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
            };
            // CONOUT$ still names the screen buffer when app stdout was redirected.
            let name: Vec<u16> = "CONOUT$\0".encode_utf16().collect();
            let output = CreateFileW(
                name.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                0,
                null_mut(),
            );
            if output == INVALID_HANDLE_VALUE {
                return None;
            }
            let _output = OwnedHandle::from_raw_handle(output);
            let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
            let mut font = CONSOLE_FONT_INFO::default();
            if GetConsoleScreenBufferInfo(output, &mut info) == 0
                || GetCurrentConsoleFont(output, 0, &mut font) == 0
                || font.dwFontSize.X <= 0
                || font.dwFontSize.Y <= 0
                || click.x < 0
                || click.y < 0
            {
                return None;
            }
            let col = click.x / font.dwFontSize.X as i32;
            let row = click.y / font.dwFontSize.Y as i32;
            let cols = info.dwSize.X as usize;
            let rows = (info.srWindow.Bottom - info.srWindow.Top + 1) as usize;
            if col > (info.srWindow.Right - info.srWindow.Left) as i32 || row as usize >= rows {
                return None;
            }
            let mut cells = vec![0u16; cols * rows];
            let mut read = 0;
            if ReadConsoleOutputCharacterW(
                output,
                cells.as_mut_ptr(),
                cells.len() as u32,
                COORD {
                    X: 0,
                    Y: info.srWindow.Top,
                },
                &mut read,
            ) == 0
            {
                return None;
            }
            cells.truncate(read as usize);
            super::console_url_at_cell(
                &cells,
                cols,
                row as usize,
                col as usize + info.srWindow.Left as usize,
            )
        })();
        FreeConsole();
        url
    }
}

fn resize_console(resize: ConsoleResize) -> Result<()> {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE, RECT},
        Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING},
    };
    if resize.width <= 0 || resize.height <= 0 {
        return Ok(());
    }
    let _guard = CONSOLE_ATTACH.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        let hwnd = resize.hwnd as HWND;
        if IsWindow(hwnd) == 0 || IsWindowVisible(hwnd) == 0 {
            return Ok(());
        }
        FreeConsole();
        if AttachConsole(resize.pid) == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let result = (|| {
            if GetConsoleWindow() != hwnd {
                return Ok(());
            }
            let name: Vec<u16> = "CONOUT$\0".encode_utf16().collect();
            let output = CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                0,
                null_mut(),
            );
            if output == INVALID_HANDLE_VALUE {
                return Err(std::io::Error::last_os_error().into());
            }
            let _output = OwnedHandle::from_raw_handle(output);
            let mut rect = RECT::default();
            let mut font = CONSOLE_FONT_INFO::default();
            let mut info = CONSOLE_SCREEN_BUFFER_INFOEX::default();
            info.cbSize = size_of_val(&info) as u32;
            if GetClientRect(hwnd, &mut rect) == 0
                || GetCurrentConsoleFont(output, 0, &mut font) == 0
                || GetConsoleScreenBufferInfoEx(output, &mut info) == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            let Some((cols, rows)) = compute_console_grid(
                rect.right - rect.left,
                rect.bottom - rect.top,
                i32::from(font.dwFontSize.X),
                i32::from(font.dwFontSize.Y),
            ) else {
                return Ok(());
            };
            let visible_rows = i32::from(info.srWindow.Bottom) - i32::from(info.srWindow.Top) + 1;
            if info.dwSize.X == cols && visible_rows == i32::from(rows) {
                return Ok(());
            }
            let at_bottom = info.srWindow.Bottom >= info.dwSize.Y - 2;
            info.dwSize.X = cols;
            info.dwSize.Y = info.dwSize.Y.max(rows);
            let top = if at_bottom {
                info.dwSize.Y - rows
            } else {
                info.srWindow.Top.clamp(0, info.dwSize.Y - rows)
            };
            info.srWindow.Left = 0;
            info.srWindow.Top = top;
            // SetConsoleScreenBufferInfoEx takes exclusive edges, unlike its getter.
            info.srWindow.Right = cols;
            info.srWindow.Bottom = top + rows;
            if SetConsoleScreenBufferInfoEx(output, &info) == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(())
        })();
        FreeConsole();
        result
    }
}

pub(super) struct EmbeddedShellHandle {
    pub(super) process: Arc<Process>,
    pub(super) generation: u64,
    hwnd: usize,
    pid: u32,
    parent: usize,
    visible: bool,
    rect: Option<ShellViewportRect>,
    _job: Arc<Job>,
}
impl EmbeddedShellHandle {
    pub(super) fn focus(&self) {
        if self.visible {
            unsafe {
                SetFocus(self.hwnd as HWND);
            }
        }
    }

    pub(super) fn reconcile(
        &mut self,
        parent: usize,
        active: bool,
        rect: Option<ShellViewportRect>,
        focus_on_show: bool,
    ) {
        unsafe {
            if !active {
                if self.visible {
                    ShowWindow(self.hwnd as HWND, SW_HIDE);
                    self.visible = false;
                    update_visible(|visible| {
                        if let Some(shell) = visible.get_mut(&parent) {
                            if shell.hwnd == self.hwnd {
                                shell.hwnd = 0;
                            }
                        }
                    });
                }
                return;
            }
            let Some(rect) = rect.or(self.rect) else {
                return;
            };
            if rect.width == 0 || rect.height == 0 {
                self.reconcile(parent, false, None, false);
                return;
            }
            let resize = self.rect != Some(rect);
            if resize {
                SetWindowPos(
                    self.hwnd as HWND,
                    null_mut(),
                    rect.x,
                    rect.y,
                    rect.width,
                    rect.height,
                    SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                );
                self.rect = Some(rect);
            }
            if !self.visible {
                ShowWindow(self.hwnd as HWND, SW_SHOWNA);
                self.visible = true;
                if focus_on_show {
                    self.focus();
                }
                update_visible(|visible| {
                    if let Some(shell) = visible.get_mut(&parent) {
                        shell.hwnd = self.hwnd;
                        shell.pid = self.pid;
                    }
                });
            }
            if resize {
                {
                    let visible = VISIBLE.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(shell) = visible.get(&parent) {
                        let _ = shell.resizes.send(ConsoleResize {
                            pid: self.pid,
                            hwnd: self.hwnd,
                            width: rect.width,
                            height: rect.height,
                        });
                    }
                }
            }
        }
    }
}
impl Drop for EmbeddedShellHandle {
    fn drop(&mut self) {
        update_visible(|visible| {
            if let Some(shell) = visible.get_mut(&self.parent) {
                if shell.hwnd == self.hwnd {
                    shell.hwnd = 0;
                }
            }
        });
        unsafe {
            ShowWindow(self.hwnd as HWND, SW_HIDE);
            TerminateProcess(self.process.0 as HANDLE, 0);
        }
    }
}

// AttachConsole affects the process, not the calling thread. Never attach two at once.
static CONSOLE_ATTACH: Mutex<()> = Mutex::new(());

pub(super) fn spawn(
    spec: ShellLaunchSpec,
    job: Arc<Job>,
    parent: usize,
    control: Arc<Mutex<SpawnState>>,
) -> Result<EmbeddedShellHandle> {
    let command = std::iter::once(spec.program.as_str())
        .chain(spec.args.iter().map(String::as_str))
        .map(windows_quote)
        .collect::<Vec<_>>()
        .join(" ");
    let mut command: Vec<u16> = command.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let mut startup = STARTUPINFOW::default();
        startup.cb = size_of::<STARTUPINFOW>() as u32;
        startup.dwFlags = STARTF_USESHOWWINDOW;
        startup.wShowWindow = SW_HIDE as u16;
        let mut info = PROCESS_INFORMATION::default();
        let mut spawn_state = control.lock().unwrap_or_else(|e| e.into_inner());
        if spawn_state.cancelled {
            bail!("Shell connection cancelled");
        }
        if CreateProcessW(
            null(),
            command.as_mut_ptr(),
            null(),
            null(),
            0,
            CREATE_NEW_CONSOLE | CREATE_SUSPENDED,
            null(),
            null(),
            &startup,
            &mut info,
        ) == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let process = Arc::new(Process(info.hProcess as usize));
        spawn_state.process = Some(process.clone());
        drop(spawn_state);
        if AssignProcessToJobObject(job.0 as HANDLE, info.hProcess) == 0 {
            let error = std::io::Error::last_os_error();
            TerminateProcess(info.hProcess, 1);
            CloseHandle(info.hThread);
            return Err(error.into());
        }
        let resumed = ResumeThread(info.hThread);
        CloseHandle(info.hThread);
        if resumed == u32::MAX {
            TerminateProcess(info.hProcess, 1);
            bail!("Could not start SSH");
        }
        let mut hwnd = null_mut();
        {
            let _guard = CONSOLE_ATTACH.lock().unwrap_or_else(|e| e.into_inner());
            for _ in 0..50 {
                if control.lock().unwrap_or_else(|e| e.into_inner()).cancelled {
                    bail!("Shell connection cancelled");
                }
                FreeConsole();
                if AttachConsole(info.dwProcessId) != 0 {
                    hwnd = GetConsoleWindow();
                    let output = GetStdHandle(STD_OUTPUT_HANDLE);
                    let mut palette = CONSOLE_SCREEN_BUFFER_INFOEX::default();
                    palette.cbSize = size_of_val(&palette) as u32;
                    if GetConsoleScreenBufferInfoEx(output, &mut palette) != 0 {
                        palette.ColorTable[0] = 0x000D0D0D;
                        palette.ColorTable[7] = 0x00F0E8E2;
                        palette.srWindow.Right += 1;
                        palette.srWindow.Bottom += 1;
                        SetConsoleScreenBufferInfoEx(output, &palette);
                    }
                    FreeConsole();
                    if !hwnd.is_null() {
                        break;
                    }
                }
                if WaitForSingleObject(info.hProcess, 100) == WAIT_OBJECT_0 {
                    break;
                }
            }
        }
        if hwnd.is_null() {
            TerminateProcess(info.hProcess, 1);
            bail!("SSH did not expose a native console window");
        }
        ShowWindow(hwnd, SW_HIDE);
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        SetWindowLongPtrW(
            hwnd,
            GWL_STYLE,
            ((style
                & !(WS_CAPTION
                    | WS_THICKFRAME
                    | WS_BORDER
                    | WS_MINIMIZEBOX
                    | WS_MAXIMIZEBOX
                    | WS_VSCROLL
                    | WS_HSCROLL
                    | WS_SYSMENU
                    | WS_POPUP))
                | WS_CHILD) as isize,
        );
        windows_sys::Win32::Foundation::SetLastError(0);
        let previous = SetParent(hwnd, parent as HWND);
        if previous.is_null() && windows_sys::Win32::Foundation::GetLastError() != 0 {
            TerminateProcess(info.hProcess, 1);
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(EmbeddedShellHandle {
            process,
            generation: 0,
            hwnd: hwnd as usize,
            pid: info.dwProcessId,
            parent,
            visible: false,
            rect: None,
            _job: job,
        })
    }
}
