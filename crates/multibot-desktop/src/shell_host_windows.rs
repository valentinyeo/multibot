use super::{ShellLaunchSpec, ShellViewportRect, forwarded_shortcut, windows_quote};
use anyhow::{Result, bail};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, WAIT_OBJECT_0, WPARAM},
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

thread_local! {
    static VISIBLE: RefCell<HashMap<usize, (usize, tokio::sync::mpsc::UnboundedSender<gpui_kit::Keystroke>)>> = RefCell::new(HashMap::new());
    static FORWARDED: RefCell<HashMap<u32, usize>> = RefCell::new(HashMap::new());
}

pub(super) struct NativeHost {
    pub(super) parent: usize,
    pub(super) job: Arc<Job>,
    hook: usize,
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
            VISIBLE.with(|v| v.borrow_mut().insert(parent, (0, shortcuts)));
            Ok(Self {
                parent,
                job,
                hook: hook as usize,
            })
        }
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
                if let Some(chord) = forwarded_shortcut(key, ctrl, alt, shift) {
                    let foreground = GetForegroundWindow();
                    let root = GetAncestor(foreground, GA_ROOT) as usize;
                    let parent = VISIBLE.with(|v| {
                        v.borrow()
                            .get(&root)
                            .filter(|(hwnd, _)| *hwnd != 0 && !crate::browser_pane::native::has_focus(root))
                            .map(|_| root)
                    });
                    if let Some(parent) = parent {
                        SetForegroundWindow(parent as HWND);
                        SetFocus(parent as HWND);
                        if let Ok(stroke) = gpui_kit::Keystroke::parse(chord) {
                            VISIBLE.with(|v| {
                                if let Some((_, sender)) = v.borrow().get(&parent) {
                                    let _ = sender.send(stroke);
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

pub(super) struct EmbeddedShellHandle {
    pub(super) process: Arc<Process>,
    pub(super) generation: u64,
    hwnd: usize,
    parent: usize,
    visible: bool,
    rect: Option<ShellViewportRect>,
    _job: Arc<Job>,
}
impl EmbeddedShellHandle {
    pub(super) fn reconcile(
        &mut self,
        parent: usize,
        active: bool,
        rect: Option<ShellViewportRect>,
    ) {
        unsafe {
            if !active {
                if self.visible {
                    ShowWindow(self.hwnd as HWND, SW_HIDE);
                    self.visible = false;
                    VISIBLE.with(|v| {
                        if let Some((hwnd, _)) = v.borrow_mut().get_mut(&parent) {
                            if *hwnd == self.hwnd {
                                *hwnd = 0;
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
                self.reconcile(parent, false, None);
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
                SetFocus(self.hwnd as HWND);
                self.visible = true;
                VISIBLE.with(|v| {
                    if let Some((hwnd, _)) = v.borrow_mut().get_mut(&parent) {
                        *hwnd = self.hwnd;
                    }
                });
            }
        }
    }
}
impl Drop for EmbeddedShellHandle {
    fn drop(&mut self) {
        VISIBLE.with(|v| {
            if let Some((hwnd, _)) = v.borrow_mut().get_mut(&self.parent) {
                if *hwnd == self.hwnd {
                    *hwnd = 0;
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
            parent,
            visible: false,
            rect: None,
            _job: job,
        })
    }
}
