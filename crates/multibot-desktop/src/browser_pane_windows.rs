use anyhow::{Context as _, Result, bail};
use multibot_core::browser_pane::{
    BrowserPaneRect, browser_profile_root, discover_browser, launch_args, session_profile,
};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    io,
    os::windows::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    ptr::{null, null_mut},
};
use tokio::sync::mpsc::UnboundedSender;
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    Storage::Xps::{PW_CLIENTONLY, PrintWindow},
    System::{JobObjects::*, LibraryLoader::GetModuleHandleW, Threading::*},
    UI::{
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

pub(super) struct Process {
    handle: usize,
    pid: u32,
    _profile_lock: std::fs::File,
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
    _job: Handle,
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
    pub(super) fn launch(parent: usize, session: &str, url: &str) -> Result<(Self, Process)> {
        let roots = ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
            .into_iter()
            .filter_map(std::env::var_os)
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        let program = discover_browser(&roots, Path::is_file)
            .context("Install Microsoft Edge or Google Chrome to use the browser pane")?;
        let profile = session_profile(&isolated_root()?, session);
        real_directory(&profile)?;
        // Chromium otherwise forwards a second launch into the first window's process.
        let profile_lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(profile.join("multibot-pane.lock"))?;
        profile_lock.try_lock().context(
            "This session's browser is already open in another window or is still closing",
        )?;
        match std::fs::remove_file(profile.join("DevToolsActivePort")) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        let mut command = command_line(&program, &launch_args(&profile, url));
        let program = wide(program.as_os_str());

        // Assign the suspended process before it can start any Chromium children.
        unsafe {
            let job = Handle(CreateJobObjectW(null(), null()));
            if job.0.is_null() {
                return Err(io::Error::last_os_error().into());
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
                return Err(io::Error::last_os_error().into());
            }
            let mut startup = STARTUPINFOW::default();
            startup.cb = size_of_val(&startup) as u32;
            startup.dwFlags = STARTF_USESHOWWINDOW;
            startup.wShowWindow = SW_HIDE as u16;
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
                pid: info.dwProcessId,
                _profile_lock: profile_lock,
            };
            if AssignProcessToJobObject(job.0, info.hProcess) == 0 {
                let error = io::Error::last_os_error();
                TerminateProcess(info.hProcess, 1);
                return Err(error.into());
            }
            if ResumeThread(thread.0) == u32::MAX {
                return Err(io::Error::last_os_error().into());
            }
            Ok((
                Self {
                    hwnd: None,
                    rect: None,
                    parent,
                    _job: job,
                    cdp: crate::browser_cdp::BrowserController::new(profile),
                    binding_id: format!("{}-{parent}-{}", std::process::id(), info.dwProcessId),
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
            ShowWindowAsync(hwnd, SW_HIDE);
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
            let frame = WS_CAPTION
                | WS_THICKFRAME
                | WS_POPUP
                | WS_SYSMENU
                | WS_MINIMIZEBOX
                | WS_MAXIMIZEBOX
                | WS_VISIBLE;
            SetLastError(0);
            if SetWindowLongPtrW(hwnd, GWL_STYLE, ((style & !frame) | WS_CHILD) as isize) == 0
                && GetLastError() != 0
            {
                return Err(io::Error::last_os_error().into());
            }
            SetLastError(0);
            if SetParent(hwnd, self.parent as HWND).is_null() && GetLastError() != 0 {
                return Err(io::Error::last_os_error().into());
            }
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
        let old = self.rect.take();
        unsafe {
            if let Some(rect) = rect {
                let flags = SWP_ASYNCWINDOWPOS
                    | SWP_NOACTIVATE
                    | SWP_NOZORDER
                    | if old.is_none() {
                        SWP_SHOWWINDOW | SWP_FRAMECHANGED
                    } else {
                        0
                    };
                if SetWindowPos(
                    hwnd as HWND,
                    null_mut(),
                    rect.x,
                    rect.y,
                    rect.width,
                    rect.height,
                    flags,
                ) != 0
                {
                    self.rect = Some(rect);
                }
            } else {
                ShowWindowAsync(hwnd as HWND, SW_HIDE);
            }
        }
    }

    pub(super) fn visible_window(&self) -> Option<usize> {
        self.rect.and(self.hwnd)
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        if let Some((_, session_id, tx)) = self.bridge.take() {
            let _ = tx.send(multibot_proto::ClientRequest::BrowserUnbind {
                session_id,
                binding_id: self.binding_id.clone(),
            });
        }
        self.place(None);
    }
}

impl Process {
    pub(super) fn find_window(&self) -> Result<usize> {
        struct Search {
            pid: u32,
            found: usize,
        }
        unsafe extern "system" fn find(hwnd: HWND, data: LPARAM) -> BOOL {
            unsafe {
                let search = &mut *(data as *mut Search);
                let mut pid = 0;
                GetWindowThreadProcessId(hwnd, &mut pid);
                if pid != search.pid || !GetWindow(hwnd, GW_OWNER).is_null() {
                    return 1;
                }
                let mut class = [0u16; 64];
                let size = GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32);
                if size > 0
                    && String::from_utf16_lossy(&class[..size as usize]) == "Chrome_WidgetWin_1"
                {
                    search.found = hwnd as usize;
                    return 0;
                }
                1
            }
        }

        // Only discovery is polled, off the UI thread, and only during startup.
        for _ in 0..120 {
            let mut search = Search {
                pid: self.pid,
                found: 0,
            };
            unsafe {
                EnumWindows(Some(find), &mut search as *mut Search as LPARAM);
                if search.found != 0 {
                    return Ok(search.found);
                }
                if WaitForSingleObject(self.handle as HANDLE, 50) != WAIT_TIMEOUT {
                    bail!("Browser exited before opening its window");
                }
            }
        }
        bail!("Browser window did not open")
    }

    pub(super) fn wait(&self) {
        unsafe { WaitForSingleObject(self.handle as HANDLE, INFINITE) };
    }
}

type ShortcutSender = UnboundedSender<&'static str>;
thread_local! {
    static VISIBLE: RefCell<HashMap<usize, (usize, ShortcutSender)>> = RefCell::default();
    static HELD: RefCell<HashSet<u32>> = RefCell::default();
}

pub(crate) fn has_focus(parent: usize) -> bool {
    VISIBLE.with(|entries| {
        let entries = entries.borrow();
        let Some((hwnd, _)) = entries.get(&parent) else {
            return false;
        };
        unsafe {
            let mut info = GUITHREADINFO {
                cbSize: size_of::<GUITHREADINFO>() as u32,
                ..Default::default()
            };
            GetGUIThreadInfo(0, &mut info) != 0
                && (info.hwndFocus == *hwnd as HWND || IsChild(*hwnd as HWND, info.hwndFocus) != 0)
        }
    })
}

pub(super) struct Shortcuts {
    parent: usize,
    hook: HHOOK,
    sender: ShortcutSender,
}

impl Shortcuts {
    pub(super) fn new(parent: usize, sender: ShortcutSender) -> Result<Self> {
        let hook = unsafe {
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard), GetModuleHandleW(null()), 0)
        };
        if hook.is_null() {
            return Err(io::Error::last_os_error().into());
        }
        Ok(Self {
            parent,
            hook,
            sender,
        })
    }

    pub(super) fn set_visible(&self, hwnd: Option<usize>) {
        VISIBLE.with(|entries| {
            let mut entries = entries.borrow_mut();
            if let Some(hwnd) = hwnd {
                entries.insert(self.parent, (hwnd, self.sender.clone()));
            } else {
                entries.remove(&self.parent);
            }
        });
    }
}

impl Drop for Shortcuts {
    fn drop(&mut self) {
        self.set_visible(None);
        if VISIBLE.with(|entries| entries.borrow().is_empty()) {
            HELD.with(|keys| keys.borrow_mut().clear());
        }
        unsafe { UnhookWindowsHookEx(self.hook) };
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
                let parent = GetForegroundWindow() as usize;
                if has_focus(parent)
                    && let Some(chord) = crate::shell_host::registered_native_shortcut(
                        key,
                        GetAsyncKeyState(0x11) < 0,
                        GetAsyncKeyState(0x12) < 0,
                        GetAsyncKeyState(0x10) < 0,
                    )
                    && VISIBLE.with(|entries| {
                        entries
                            .borrow()
                            .get(&parent)
                            .is_some_and(|(_, sender)| sender.send(chord.gpui_key).is_ok())
                    })
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
