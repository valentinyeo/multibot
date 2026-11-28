use super::{
    ShellLaunchSpec, ShellShortcut, ShellViewportRect, console_paste_chunks, console_wheel_params,
    shell_shortcut, windows_quote,
};
use anyhow::{Result, bail};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex};
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

struct VisibleShell {
    hwnd: usize,
    pid: u32,
    shortcuts: tokio::sync::mpsc::UnboundedSender<gpui_kit::Keystroke>,
    urls: std::sync::mpsc::Sender<ConsoleClick>,
    pastes: std::sync::mpsc::Sender<ConsolePaste>,
    text_input_focused: bool,
    confirming_kill: bool,
}

struct ConsolePaste {
    hwnd: usize,
    pid: u32,
    parent: usize,
}

struct ConsoleClick {
    hwnd: usize,
    pid: u32,
    x: i32,
    y: i32,
}

thread_local! {
    static VISIBLE: RefCell<HashMap<usize, VisibleShell>> = RefCell::new(HashMap::new());
    static URL_CLICK: Cell<bool> = const { Cell::new(false) };
    static FORWARDED: RefCell<HashMap<u32, usize>> = RefCell::new(HashMap::new());
}

pub(super) struct NativeHost {
    pub(super) parent: usize,
    pub(super) job: Arc<Job>,
    hook: usize,
    mouse_hook: usize,
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
            let hook = SetWindowsHookExW(
                WH_KEYBOARD_LL,
                Some(keyboard_hook),
                GetModuleHandleW(null()),
                0,
            );
            if hook.is_null() {
                return Err(std::io::Error::last_os_error().into());
            }
            let mouse_hook =
                SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), GetModuleHandleW(null()), 0);
            if mouse_hook.is_null() {
                UnhookWindowsHookEx(hook);
                return Err(std::io::Error::last_os_error().into());
            }
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
                UnhookWindowsHookEx(hook);
                UnhookWindowsHookEx(mouse_hook);
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
                UnhookWindowsHookEx(hook);
                UnhookWindowsHookEx(mouse_hook);
                return Err(error.into());
            }
            let host = Self {
                parent,
                job,
                hook: hook as usize,
                mouse_hook: mouse_hook as usize,
                pastes,
            };
            VISIBLE.with(|v| {
                v.borrow_mut().insert(
                    parent,
                    VisibleShell {
                        hwnd: 0,
                        pid: 0,
                        shortcuts,
                        urls,
                        pastes: host.pastes.clone(),
                        text_input_focused: false,
                        confirming_kill: false,
                    },
                )
            });
            Ok(host)
        }
    }
    pub(super) fn focus_visible(&self) {
        let hwnd = VISIBLE.with(|v| v.borrow().get(&self.parent).map(|s| s.hwnd).unwrap_or(0));
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
        VISIBLE.with(|visible| {
            if let Some(shell) = visible.borrow_mut().get_mut(&self.parent) {
                shell.text_input_focused = focused;
            }
        });
    }
    pub(super) fn set_kill_confirmation(&self, confirming: bool) {
        VISIBLE.with(|visible| {
            if let Some(shell) = visible.borrow_mut().get_mut(&self.parent) {
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
        VISIBLE.with(|v| v.borrow_mut().remove(&self.parent));
        FORWARDED.with(|v| v.borrow_mut().retain(|_, parent| *parent != self.parent));
        unsafe {
            UnhookWindowsHookEx(self.hook as _);
            UnhookWindowsHookEx(self.mouse_hook as _);
        }
    }
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 {
            let event = &*(lparam as *const KBDLLHOOKSTRUCT);
            let key = event.vkCode;
            let up = matches!(wparam as u32, WM_KEYUP | WM_SYSKEYUP);
            if up {
                if FORWARDED.with(|v| v.borrow_mut().remove(&key)).is_some() {
                    return 1;
                }
            } else if matches!(wparam as u32, WM_KEYDOWN | WM_SYSKEYDOWN) {
                let ctrl = GetAsyncKeyState(0x11) < 0;
                let alt = GetAsyncKeyState(0x12) < 0;
                let shift = GetAsyncKeyState(0x10) < 0;
                let foreground = GetForegroundWindow();
                let root = GetAncestor(foreground, GA_ROOT) as usize;
                let confirming = VISIBLE.with(|v| v.borrow().get(&root).is_some_and(|shell| shell.confirming_kill));
                let shortcut = if key == 0x1B && !ctrl && !alt && !shift && confirming {
                    Some(ShellShortcut::Forward("escape"))
                } else { shell_shortcut(key, ctrl, alt, shift) };
                if let Some(shortcut) = shortcut {
                    // A held key is not the second confirmation press; key-up rearms it.
                    if key == 0x57 && ctrl && shift && FORWARDED.with(|v| v.borrow().contains_key(&key)) {
                        return 1;
                    }
                    let parent = VISIBLE.with(|v| {
                        v.borrow()
                            .get(&root)
                            .filter(|shell| {
                                shell.hwnd != 0 && !crate::browser_pane::native::has_focus(root)
                            })
                            .map(|shell| (root, shell.hwnd))
                    });
                    if let Some((parent, hwnd)) = parent {
                        let chord = match shortcut {
                            ShellShortcut::Paste => {
                                let sent = VISIBLE.with(|visible| {
                                    let visible = visible.borrow();
                                    let Some(shell) = visible.get(&parent) else {
                                        return false;
                                    };
                                    let focus = thread_focus(parent as HWND);
                                    if shell.text_input_focused && focus != hwnd as HWND {
                                        return false;
                                    }
                                    shell
                                        .pastes
                                        .send(ConsolePaste {
                                            hwnd,
                                            pid: shell.pid,
                                            parent,
                                        })
                                        .is_ok()
                                });
                                if sent {
                                    FORWARDED.with(|v| v.borrow_mut().insert(key, parent));
                                    return 1;
                                }
                                return CallNextHookEx(null_mut(), code, wparam, lparam);
                            }
                            ShellShortcut::Forward(chord) => chord,
                        };
                        SetForegroundWindow(parent as HWND);
                        SetFocus(parent as HWND);
                        if let Ok(stroke) = gpui_kit::Keystroke::parse(chord) {
                            VISIBLE.with(|v| {
                                if let Some(shell) = v.borrow().get(&parent) {
                                    let _ = shell.shortcuts.send(stroke);
                                }
                            });
                            FORWARDED.with(|v| v.borrow_mut().insert(key, parent));
                            return 1;
                        }
                    }
                }
            }
        }
        CallNextHookEx(null_mut(), code, wparam, lparam)
    }
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 {
            if wparam as u32 == WM_LBUTTONUP && URL_CLICK.replace(false) {
                return 1;
            }
            if wparam as u32 == WM_LBUTTONDOWN {
                let event = &*(lparam as *const MSLLHOOKSTRUCT);
                let target = WindowFromPoint(event.pt);
                let shell = VISIBLE.with(|visible| {
                    visible.borrow().iter().find_map(|(parent, shell)| {
                        let hwnd = shell.hwnd as HWND;
                        (!hwnd.is_null() && (target == hwnd || IsChild(hwnd, target) != 0))
                            .then_some((*parent, shell.hwnd))
                    })
                });
                if let Some((parent, hwnd)) = shell {
                    SetForegroundWindow(parent as HWND);
                    SetFocus(hwnd as HWND);
                }
            }
            if wparam as u32 == WM_MOUSEWHEEL {
                let event = &*(lparam as *const MSLLHOOKSTRUCT);
                let target = WindowFromPoint(event.pt);
                let root = GetAncestor(GetForegroundWindow(), GA_ROOT) as usize;
                let forwarded = VISIBLE.with(|visible| {
                    let visible = visible.borrow();
                    let Some(shell) = visible.get(&root) else {
                        return false;
                    };
                    let hwnd = shell.hwnd as HWND;
                    if hwnd.is_null()
                        || (target != hwnd && IsChild(hwnd, target) == 0)
                        || crate::browser_pane::native::has_focus(root)
                        || thread_focus(hwnd) == hwnd
                    {
                        return false;
                    }
                    let (w, l) = console_wheel_params(
                        (event.mouseData >> 16) as i16,
                        GetAsyncKeyState(0x11) < 0,
                        GetAsyncKeyState(0x10) < 0,
                        event.pt.x,
                        event.pt.y,
                    );
                    PostMessageW(hwnd, WM_MOUSEWHEEL, w, l) != 0
                });
                if forwarded {
                    return 1;
                }
            }
            if wparam as u32 == WM_LBUTTONDOWN && GetAsyncKeyState(0x11) < 0 {
                let event = &*(lparam as *const MSLLHOOKSTRUCT);
                let target = WindowFromPoint(event.pt);
                let request_sent = VISIBLE.with(|visible| {
                    visible.borrow().values().any(|shell| {
                        let hwnd = shell.hwnd as HWND;
                        if hwnd.is_null() || (target != hwnd && IsChild(hwnd, target) == 0) {
                            return false;
                        }
                        let mut point = event.pt;
                        if ScreenToClient(hwnd, &mut point) == 0 {
                            return false;
                        }
                        shell
                            .urls
                            .send(ConsoleClick {
                                hwnd: shell.hwnd,
                                pid: shell.pid,
                                x: point.x,
                                y: point.y,
                            })
                            .is_ok()
                    })
                });
                if request_sent {
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
                    VISIBLE.with(|v| {
                        if let Some(shell) = v.borrow_mut().get_mut(&parent) {
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
            if self.rect != Some(rect) || !self.visible {
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
                VISIBLE.with(|v| {
                    if let Some(shell) = v.borrow_mut().get_mut(&parent) {
                        shell.hwnd = self.hwnd;
                        shell.pid = self.pid;
                    }
                });
            }
        }
    }
}
impl Drop for EmbeddedShellHandle {
    fn drop(&mut self) {
        VISIBLE.with(|v| {
            if let Some(shell) = v.borrow_mut().get_mut(&self.parent) {
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
