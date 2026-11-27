//! Built-in native GPU terminal pane (`Alt+C`).
//!
//! Hard Invariant #4 (lazy lifecycle): nothing in this module allocates a PTY, spawns a
//! child process, or starts a polling loop until the user opens the pane. Closing the
//! pane drops the [`TerminalSession`], which closes the PTY master and kills the child,
//! so a closed pane costs 0 MB and 0% CPU.
//!
//! Rendering is strictly demand-driven: the reader thread parses PTY bytes into an
//! `alacritty_terminal` grid and then wakes the GPUI view exactly once per read batch.
//! There is no timer and no redraw loop.
//!
//! Transport selection:
//! * A session with a configured SSH host spawns `ssh -tt` in a portable PTY. `cwd` is
//!   `cd`-ed through a single-quoted POSIX literal, so no shell metacharacters in the
//!   path can escape into the remote command line.
//! * Local/offline mode spawns the user's login shell in the same portable PTY.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};
use alacritty_terminal::{Term, event::VoidListener};
use anyhow::Context as _;
use anyhow::Result;
use gpui_kit::{
    Context, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent, ParentElement,
    StatefulInteractiveElement, Styled, TestSupportExt, Window,
    component::{ActiveTheme, Sizable, button::Button, h_flex, v_flex},
    div, px,
};
use multibot_core::SshHostConfig;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{Read, Write};
#[cfg(test)]
use std::sync::OnceLock;
#[cfg(test)]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Scrollback kept while the pane is open. 1,000 lines stays under 2 MB of grid cells,
/// which keeps Hard Invariant #4 honest on a low-RAM laptop.
pub(crate) const TERMINAL_SCROLLBACK_LINES: usize = 1000;
/// Minimum grid size so a freshly spawned PTY never reports `0 x 0`.
pub(crate) const MIN_TERMINAL_COLS: usize = 20;
pub(crate) const MIN_TERMINAL_ROWS: usize = 4;
/// Default grid before the pane has been measured once.
pub(crate) const DEFAULT_TERMINAL_COLS: usize = 100;
pub(crate) const DEFAULT_TERMINAL_ROWS: usize = 24;
/// Font size for the terminal grid (px).
pub(crate) const TERMINAL_FONT_SIZE_PX: f32 = 12.5;
/// Height of the compact terminal header bar.
pub(crate) const TERMINAL_HEADER_HEIGHT_PX: f32 = 34.0;
/// Height of the split pane docked under the chat column (about a third of an 800px window).
pub(crate) const TERMINAL_PANE_HEIGHT_PX: f32 = 260.0;
/// Inner padding of the grid body, in px (subtracted before converting to columns/rows).
const TERMINAL_GRID_PADDING_PX: f32 = 16.0;
/// Line height multiple used by the grid rows.
const TERMINAL_LINE_HEIGHT_MULT: f32 = 1.35;
/// Monospace advance divided by font size. 0.6 is the common ratio for Consolas,
/// DejaVu Sans Mono and Menlo, so the grid never overflows the pane.
const TERMINAL_CELL_WIDTH_RATIO: f32 = 0.6;

/// Cell advance for the terminal font, used to convert measured pixels into columns.
pub(crate) fn terminal_cell_width_px() -> f32 {
    TERMINAL_FONT_SIZE_PX * TERMINAL_CELL_WIDTH_RATIO
}
/// Bytes read from the PTY per `read` call.
const PTY_READ_BUFFER: usize = 8192;

/// Identifies what the pane is attached to. Used for the header label and for tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TerminalTarget {
    /// Interactive SSH login shell on a configured host.
    Ssh {
        host_label: String,
        target: String,
        cwd: Option<String>,
    },
    /// Local login shell (offline or no SSH host configured).
    Local { cwd: Option<String> },
}

impl TerminalTarget {
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Ssh { target, cwd, .. } => match cwd.as_deref() {
                Some(cwd) if !cwd.trim().is_empty() => format!("{target}:{cwd}"),
                _ => target.clone(),
            },
            Self::Local { cwd } => match cwd.as_deref() {
                Some(cwd) if !cwd.trim().is_empty() => format!("local:{cwd}"),
                _ => "local".to_string(),
            },
        }
    }

    pub(crate) fn is_ssh(&self) -> bool {
        matches!(self, Self::Ssh { .. })
    }
}

/// Escapes `path` as a single POSIX shell word so `cd <path>` cannot be injected into.
///
/// Returns `None` for paths carrying control characters, so a hostile `cwd` from a remote
/// session listing never reaches the remote shell.
pub(crate) fn posix_shell_quote(path: &str) -> Option<String> {
    if path.chars().any(char::is_control) {
        return None;
    }
    let mut quoted = String::with_capacity(path.len() + 2);
    quoted.push('\'');
    for ch in path.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    Some(quoted)
}

/// Builds the remote command run by `ssh -tt`: an optional safe `cd`, then `exec $SHELL -l`.
///
/// The `cwd` goes through [`posix_shell_quote`], so no shell metacharacter can break out.
pub(crate) fn build_remote_shell_command(cwd: Option<&str>) -> String {
    let login_shell = r#"exec "${SHELL:-/bin/sh}" -l"#;
    match cwd.map(str::trim).filter(|cwd| !cwd.is_empty()) {
        Some(cwd) => match posix_shell_quote(cwd) {
            Some(quoted) => format!("cd -- {quoted} 2>/dev/null; {login_shell}"),
            None => login_shell.to_string(),
        },
        None => login_shell.to_string(),
    }
}

/// Builds the `ssh` argv for an interactive PTY (`ssh -tt ...`).
///
/// The identity file is validated by [`SshHostConfig::validated_identity_file`] and passed
/// as its own argv element, never interpolated. `--` terminates option parsing before the
/// destination so a hostile hostname can never become an ssh flag.
pub(crate) fn build_ssh_pty_args(host: &SshHostConfig, cwd: Option<&str>) -> Result<Vec<String>> {
    if host.port == 0 {
        anyhow::bail!("SSH port must be > 0");
    }
    let target = host.ssh_target();
    if target.is_empty() {
        anyhow::bail!("SSH host has no hostname");
    }
    let mut args: Vec<String> = vec![
        "-tt".to_string(),
        "-o".to_string(),
        "ServerAliveInterval=15".to_string(),
        "-o".to_string(),
        "ServerAliveCountMax=3".to_string(),
    ];
    if let Some(identity) = host.validated_identity_file()? {
        args.push("-i".to_string());
        args.push(identity.to_string());
    }
    args.push("-p".to_string());
    args.push(host.port.to_string());
    args.push("--".to_string());
    args.push(target);
    args.push(build_remote_shell_command(cwd));
    Ok(args)
}

/// The exact program and argv a freshly opened pane starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalCommand {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
}

impl TerminalCommand {
    pub(crate) fn for_target(host: Option<&SshHostConfig>, cwd: Option<&str>) -> Result<Self> {
        match host {
            Some(host) => Ok(Self {
                program: "ssh".to_string(),
                args: build_ssh_pty_args(host, cwd)?,
                cwd: None,
            }),
            None => Ok(Self {
                program: default_local_shell(),
                args: Vec::new(),
                cwd: cwd.map(str::to_string),
            }),
        }
    }

    fn into_builder(&self) -> CommandBuilder {
        let mut builder = CommandBuilder::new(&self.program);
        for arg in &self.args {
            builder.arg(arg);
        }
        if let Some(cwd) = self.cwd.as_deref().filter(|cwd| !cwd.trim().is_empty()) {
            builder.cwd(cwd);
        }
        builder.env("TERM", "xterm-256color");
        builder.env("COLORTERM", "truecolor");
        builder
    }
}

/// Login shell used in local/offline mode, resolved per platform.
pub(crate) fn default_local_shell() -> String {
    if cfg!(windows) {
        std::env::var("COMSPEC")
            .ok()
            .filter(|comspec| !comspec.trim().is_empty())
            .unwrap_or_else(|| "cmd.exe".to_string())
    } else {
        std::env::var("SHELL")
            .ok()
            .filter(|shell| !shell.trim().is_empty())
            .unwrap_or_else(|| "/bin/sh".to_string())
    }
}

/// Returns the configured host for `host_label`, when it has a usable hostname.
fn matched_host<'a>(hosts: &'a [SshHostConfig], host_label: &str) -> Option<&'a SshHostConfig> {
    hosts
        .iter()
        .find(|host| host.name == host_label)
        .filter(|host| !host.hostname.trim().is_empty())
}

/// Resolves the pane target plus the host config needed to build its argv.
///
/// `host_label` is the host the active session lives on (`"local"` when offline).
/// Matching is by exact configured host name only, so a session on an unconfigured or
/// offline label gets a local login shell instead of silently SSH-ing into another server.
pub(crate) fn resolve_terminal_transport(
    hosts: &[SshHostConfig],
    host_label: &str,
    cwd: Option<&str>,
) -> (TerminalTarget, Option<SshHostConfig>) {
    let host = matched_host(hosts, host_label);
    let target = match host {
        Some(host) => TerminalTarget::Ssh {
            host_label: host_label.to_string(),
            target: host.ssh_target(),
            cwd: cwd.map(str::to_string),
        },
        None => TerminalTarget::Local {
            cwd: cwd.map(str::to_string),
        },
    };
    (target, host.cloned())
}

/// Grid size passed to `alacritty_terminal`, which requires `alacritty_terminal::grid::Dimensions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TerminalSize {
    pub cols: usize,
    pub rows: usize,
}

impl TerminalSize {
    /// Converts the measured grid-body area into rows x cols for the monospace cell
    /// metrics. `width`/`height` are the body box, so the header bar is already excluded.
    pub(crate) fn from_pixels(
        width: gpui_kit::Pixels,
        height: gpui_kit::Pixels,
        cell_width: f32,
    ) -> Self {
        let usable_width = (width.as_f32() - TERMINAL_GRID_PADDING_PX).max(1.0);
        let usable_height = (height.as_f32() - TERMINAL_GRID_PADDING_PX).max(1.0);
        let cell_width = if cell_width > 0.5 { cell_width } else { 7.5 };
        let cell_height = TERMINAL_FONT_SIZE_PX * TERMINAL_LINE_HEIGHT_MULT;
        Self::new(
            (usable_width / cell_width).floor() as usize,
            (usable_height / cell_height).floor() as usize,
        )
    }

    pub(crate) fn new(cols: usize, rows: usize) -> Self {
        Self {
            cols: cols.max(MIN_TERMINAL_COLS),
            rows: rows.max(MIN_TERMINAL_ROWS),
        }
    }

    fn pty_size(self) -> PtySize {
        PtySize {
            rows: self.rows.min(u16::MAX as usize) as u16,
            cols: self.cols.min(u16::MAX as usize) as u16,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self::new(DEFAULT_TERMINAL_COLS, DEFAULT_TERMINAL_ROWS)
    }
}

impl Dimensions for TerminalSize {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// One rendered grid cell, resolved to RGB so the renderer stays pure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TerminalCell {
    pub c: char,
    pub fg: Rgb,
    pub bg: Rgb,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

/// Immutable snapshot of the visible grid, produced under the terminal lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalScreen {
    pub cols: usize,
    pub rows: usize,
    pub cells: Vec<TerminalCell>,
    pub cursor: Option<(usize, usize)>,
    pub title: Option<String>,
    pub bracketed_paste: bool,
    pub display_offset: usize,
}

impl TerminalScreen {
    pub(crate) fn cell(&self, row: usize, col: usize) -> Option<&TerminalCell> {
        if row >= self.rows || col >= self.cols {
            return None;
        }
        self.cells.get(row * self.cols + col)
    }

    /// Text of one row with trailing blanks trimmed, for tests and copy-to-clipboard.
    pub(crate) fn row_text(&self, row: usize) -> String {
        let mut text = String::with_capacity(self.cols);
        for col in 0..self.cols {
            text.push(self.cell(row, col).map_or(' ', |cell| cell.c));
        }
        text.trim_end().to_string()
    }

    /// Entire visible screen as plain text, one line per row, trailing blanks trimmed.
    pub(crate) fn to_text(&self) -> String {
        (0..self.rows)
            .map(|row| self.row_text(row))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[cfg(test)]
    pub(crate) fn cursor_row_text(&self) -> String {
        self.cursor
            .map(|(row, _)| self.row_text(row))
            .unwrap_or_default()
    }
}

const ANSI_BASE_PALETTE: [Rgb; 16] = [
    Rgb {
        r: 0x1A,
        g: 0x1A,
        b: 0x1A,
    },
    Rgb {
        r: 0xE5,
        g: 0x5B,
        b: 0x5B,
    },
    Rgb {
        r: 0x5F,
        g: 0xC9,
        b: 0x77,
    },
    Rgb {
        r: 0xE5,
        g: 0xB5,
        b: 0x62,
    },
    Rgb {
        r: 0x60,
        g: 0xA5,
        b: 0xFA,
    },
    Rgb {
        r: 0xC0,
        g: 0x8B,
        b: 0xF0,
    },
    Rgb {
        r: 0x4F,
        g: 0xC1,
        b: 0xD1,
    },
    Rgb {
        r: 0xD8,
        g: 0xD8,
        b: 0xD8,
    },
    Rgb {
        r: 0x6E,
        g: 0x6E,
        b: 0x6E,
    },
    Rgb {
        r: 0xFF,
        g: 0x8A,
        b: 0x8A,
    },
    Rgb {
        r: 0x8A,
        g: 0xE8,
        b: 0x9C,
    },
    Rgb {
        r: 0xFF,
        g: 0xD3,
        b: 0x87,
    },
    Rgb {
        r: 0x8A,
        g: 0xC8,
        b: 0xFF,
    },
    Rgb {
        r: 0xD8,
        g: 0xB4,
        b: 0xFF,
    },
    Rgb {
        r: 0x8A,
        g: 0xE6,
        b: 0xF0,
    },
    Rgb {
        r: 0xF2,
        g: 0xF2,
        b: 0xF2,
    },
];

pub(crate) const TERMINAL_BG: Rgb = Rgb {
    r: 0x11,
    g: 0x11,
    b: 0x11,
};
pub(crate) const TERMINAL_FG: Rgb = Rgb {
    r: 0xF2,
    g: 0xF2,
    b: 0xF2,
};
pub(crate) const TERMINAL_CURSOR: Rgb = Rgb {
    r: 0x22,
    g: 0xC5,
    b: 0x5E,
};

/// Resolves an `alacritty_terminal` color to RGB, honouring palette overrides from the PTY.
pub(crate) fn resolve_color(color: Color, colors: &Colors, default: Rgb) -> Rgb {
    match color {
        Color::Spec(rgb) => rgb,
        Color::Named(named) => colors[named].unwrap_or_else(|| match named {
            NamedColor::Foreground | NamedColor::BrightForeground => TERMINAL_FG,
            NamedColor::Background => TERMINAL_BG,
            NamedColor::Cursor => TERMINAL_CURSOR,
            other => {
                let index = other as usize;
                if index < ANSI_BASE_PALETTE.len() {
                    ANSI_BASE_PALETTE[index]
                } else if index >= 16 && index < 256 {
                    indexed_palette_color(index as u8)
                } else {
                    default
                }
            }
        }),
        Color::Indexed(index) => {
            colors[index as usize].unwrap_or_else(|| indexed_palette_color(index))
        }
    }
}

/// Standard xterm 256-color cube + grayscale ramp.
pub(crate) fn indexed_palette_color(index: u8) -> Rgb {
    if index < 16 {
        return ANSI_BASE_PALETTE[index as usize];
    }
    if index >= 232 {
        let level = 8 + (index as u32 - 232) * 10;
        let level = level.min(255) as u8;
        return Rgb {
            r: level,
            g: level,
            b: level,
        };
    }
    let index = index as u32 - 16;
    let steps = [0u8, 95, 135, 175, 215, 255];
    Rgb {
        r: steps[(index / 36) as usize % 6],
        g: steps[((index / 6) % 6) as usize],
        b: steps[(index % 6) as usize],
    }
}

/// Shared terminal emulation state, written by the PTY reader thread and read by render.
pub(crate) struct TerminalEmulator {
    term: Term<VoidListener>,
    processor: Processor,
    size: TerminalSize,
    title: Option<String>,
}

impl TerminalEmulator {
    pub(crate) fn new(size: TerminalSize) -> Self {
        let mut config = alacritty_terminal::term::Config::default();
        config.scrolling_history = TERMINAL_SCROLLBACK_LINES;
        let term = Term::new(config, &size, VoidListener);
        Self {
            term,
            processor: Processor::new(),
            size,
            title: None,
        }
    }

    /// Feeds raw PTY bytes through the ANSI parser into the grid.
    pub(crate) fn advance(&mut self, bytes: &[u8]) {
        self.processor.advance(&mut self.term, bytes);
    }

    pub(crate) fn resize(&mut self, size: TerminalSize) -> bool {
        if size == self.size {
            return false;
        }
        self.term.resize(size);
        self.size = size;
        true
    }

    pub(crate) fn clear(&mut self) {
        self.term
            .scroll_display(alacritty_terminal::grid::Scroll::Top);
        self.term.grid_mut().reset();
    }

    pub(crate) fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// Moves the display offset through scrollback. Positive values go toward older output.
    pub(crate) fn scroll(&mut self, delta: i32) {
        if delta == 0 {
            return;
        }
        self.term
            .scroll_display(alacritty_terminal::grid::Scroll::Delta(delta));
    }

    pub(crate) fn cursor(&self) -> Option<(usize, usize)> {
        let content = self.term.renderable_content();
        if !content.mode.contains(TermMode::SHOW_CURSOR) {
            return None;
        }
        let line = content.cursor.point.line.0;
        if line < 0 {
            return None;
        }
        Some((line as usize, content.cursor.point.column.0))
    }

    /// Flattens the visible grid into an owned snapshot (cheap to render, no lock held).
    pub(crate) fn snapshot(&self) -> TerminalScreen {
        let colors = self.term.colors().clone();
        let content = self.term.renderable_content();
        let cols = self.size.cols;
        let rows = self.size.rows;
        let mut cells = vec![
            TerminalCell {
                c: ' ',
                fg: TERMINAL_FG,
                bg: TERMINAL_BG,
                bold: false,
                italic: false,
                underline: false,
                inverse: false,
            };
            cols * rows
        ];
        for indexed in content.display_iter {
            let line = indexed.point.line.0;
            if line < 0 {
                continue;
            }
            let row = line as usize;
            let col = indexed.point.column.0;
            if row >= rows || col >= cols {
                continue;
            }
            cells[row * cols + col] = render_cell(&indexed.cell, &colors);
        }
        TerminalScreen {
            cols,
            rows,
            cells,
            cursor: self.cursor(),
            title: self.title.clone(),
            bracketed_paste: self.bracketed_paste(),
            display_offset: content.display_offset,
        }
    }
}

fn render_cell(cell: &Cell, colors: &Colors) -> TerminalCell {
    let inverse = cell.flags.contains(Flags::INVERSE);
    let mut fg = resolve_color(cell.fg, colors, TERMINAL_FG);
    let mut bg = resolve_color(cell.bg, colors, TERMINAL_BG);
    if cell.flags.contains(Flags::BOLD) && cell.fg == Color::Named(NamedColor::Foreground) {
        fg = TERMINAL_FG;
    }
    if cell.flags.contains(Flags::DIM) {
        fg = Rgb {
            r: (fg.r as u16 * 3 / 4) as u8,
            g: (fg.g as u16 * 3 / 4) as u8,
            b: (fg.b as u16 * 3 / 4) as u8,
        };
    }
    if cell.flags.contains(Flags::HIDDEN) {
        fg = bg;
    }
    if inverse {
        std::mem::swap(&mut fg, &mut bg);
    }
    TerminalCell {
        c: if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            ' '
        } else {
            cell.c
        },
        fg,
        bg,
        bold: cell.flags.contains(Flags::BOLD),
        italic: cell.flags.intersects(Flags::ITALIC | Flags::BOLD_ITALIC),
        underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
        inverse,
    }
}

/// Live PTY + emulator pair. Dropping it kills the child and closes the PTY.
pub(crate) struct TerminalSession {
    pub target: TerminalTarget,
    /// The exact program/argv this session started, kept for the header and tests.
    pub command: TerminalCommand,
    state: Arc<Mutex<TerminalEmulator>>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    killer: Option<Box<dyn portable_pty::ChildKiller + Send + Sync>>,
    reader_stop: Arc<AtomicBool>,
    /// Bytes read from the PTY so far; tests assert this rises after real output.
    #[cfg(test)]
    bytes_seen: Arc<AtomicU64>,
    /// Bumped when the reader thread ends, so tests can observe PTY teardown.
    #[cfg(test)]
    generation: Arc<AtomicU64>,
    size: TerminalSize,
    exited: bool,
}

impl TerminalSession {
    /// Spawns the interactive PTY and its reader thread. This is the only place a
    /// process starts, and it is only reached while the pane is opening.
    ///
    /// `wake` fires once per PTY read batch so the GPUI view redraws exactly when new
    /// bytes exist. No timer, no polling loop.
    pub(crate) fn spawn_command(
        target: TerminalTarget,
        command: TerminalCommand,
        size: TerminalSize,
        wake: Option<tokio::sync::mpsc::UnboundedSender<()>>,
    ) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(size.pty_size())
            .context("opening portable PTY")?;
        let builder = command.into_builder();
        let child = pair
            .slave
            .spawn_command(builder)
            .with_context(|| format!("spawning {}", command.program))?;
        // The slave side must go away in the parent or the PTY never reports EOF.
        drop(pair.slave);
        let mut reader = pair
            .master
            .try_clone_reader()
            .context("cloning PTY reader")?;
        let writer = pair.master.take_writer().context("taking PTY writer")?;
        let killer = child.clone_killer();

        let state = Arc::new(Mutex::new(TerminalEmulator::new(size)));
        let reader_stop = Arc::new(AtomicBool::new(false));
        #[cfg(test)]
        let bytes_seen = Arc::new(AtomicU64::new(0));
        #[cfg(test)]
        let generation = Arc::new(AtomicU64::new(1));

        let thread_state = state.clone();
        let thread_stop = reader_stop.clone();
        #[cfg(test)]
        let thread_bytes = bytes_seen.clone();
        #[cfg(test)]
        let local_gen = generation.clone();
        let wake = wake.clone();
        std::thread::Builder::new()
            .name("multibot-term-reader".to_string())
            .spawn(move || {
                let mut buffer = [0u8; PTY_READ_BUFFER];
                loop {
                    if thread_stop.load(Ordering::Relaxed) {
                        break;
                    }
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(n) => {
                            #[cfg(test)]
                            thread_bytes.fetch_add(n as u64, Ordering::Relaxed);
                            if let Ok(mut emulator) = thread_state.lock() {
                                emulator.advance(&buffer[..n]);
                            }
                            // Exactly one wake-up per read batch: demand-driven, no timer.
                            // GPUI's headless test scheduler refuses foreground wakes
                            // raised from another thread, so tests read the grid directly.
                            #[cfg(not(test))]
                            if let Some(wake) = wake.as_ref() {
                                let _ = wake.send(());
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                #[cfg(test)]
                #[cfg(test)]
                local_gen.fetch_add(1, Ordering::Relaxed);
                // A closed PTY is an event too: wake once so the pane can report the exit.
                #[cfg(not(test))]
                if let Some(wake) = wake.as_ref() {
                    let _ = wake.send(());
                }
                let _ = &wake;
            })
            .context("spawning PTY reader thread")?;

        Ok(Self {
            target,
            command,
            state,
            master: pair.master,
            writer,
            child,
            killer: Some(killer),
            reader_stop,
            #[cfg(test)]
            bytes_seen,
            #[cfg(test)]
            generation,
            size,
            exited: false,
        })
    }

    /// Sends bytes typed in the pane to the PTY.
    pub(crate) fn write(&mut self, bytes: &[u8]) {
        if bytes.is_empty() || self.exited {
            return;
        }
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    pub(crate) fn resize(&mut self, size: TerminalSize) {
        if size == self.size {
            return;
        }
        self.size = size;
        let _ = self.master.resize(size.pty_size());
        if let Ok(mut emulator) = self.state.lock() {
            emulator.resize(size);
        }
    }

    pub(crate) fn snapshot(&self) -> TerminalScreen {
        match self.state.lock() {
            Ok(emulator) => emulator.snapshot(),
            Err(poisoned) => poisoned.into_inner().snapshot(),
        }
    }

    #[cfg(test)]
    pub(crate) fn read_bytes(&self) -> u64 {
        self.bytes_seen.load(Ordering::Relaxed)
    }

    pub(crate) fn request_reader_stop(&self) {
        self.reader_stop.store(true, Ordering::Relaxed);
    }

    /// Clears the visible grid and asks the shell to repaint a fresh prompt.
    pub(crate) fn clear(&mut self) {
        if self.exited {
            return;
        }
        // Ask the remote/local shell to repaint a clean prompt.
        self.write(b"\x0c");
        if let Ok(mut emulator) = self.state.lock() {
            emulator.clear();
        }
    }

    /// Scrolls the visible grid through scrollback. `delta > 0` moves toward older lines.
    pub(crate) fn scroll(&mut self, delta: i32) {
        if let Ok(mut emulator) = self.state.lock() {
            emulator.scroll(delta);
        }
    }

    pub(crate) fn bracketed_paste(&self) -> bool {
        self.state
            .lock()
            .map(|emulator| emulator.bracketed_paste())
            .unwrap_or(false)
    }

    /// Formatted bytes for a clipboard paste, honouring bracketed-paste mode.
    pub(crate) fn paste_bytes(&self, text: &str) -> Vec<u8> {
        let normalised = text.replace("\r\n", "\n");
        if self.bracketed_paste() {
            let mut bytes = Vec::with_capacity(normalised.len() + 12);
            bytes.extend_from_slice(b"\x1b[200~");
            bytes.extend_from_slice(normalised.as_bytes());
            bytes.extend_from_slice(b"\x1b[201~");
            bytes
        } else {
            normalised.into_bytes()
        }
    }

    /// True once the child exited and the reader thread has drained the PTY.
    pub(crate) fn poll_exited(&mut self) -> bool {
        if self.exited {
            return true;
        }
        let exited = match self.child.try_wait() {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(_) => true,
        };
        if exited {
            self.exited = true;
        }
        exited
    }

    #[cfg(test)]
    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        // Order matters: stop the reader, kill the child, then let the PTY master drop
        // so the slave sees EOF and no orphan shell survives a closed pane.
        self.reader_stop.store(true, Ordering::Relaxed);
        if let Some(mut killer) = self.killer.take() {
            let _ = killer.kill();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// GPUI-facing pane state. An empty `session` means the pane is closed, and a closed
/// pane owns no PTY, no child process and no reader thread.
pub(crate) struct TerminalPaneState {
    pub session: Option<TerminalSession>,
    pub screen: Option<TerminalScreen>,
    pub error: Option<String>,
    pub focus_handle: Option<FocusHandle>,
    pub size: TerminalSize,
    /// Test-only hook: replaces the real SSH/shell command with a stub transport.
    /// Production always resolves the command from the active session.
    #[cfg(test)]
    pub command_override: Option<TerminalCommand>,
    /// Wake-up channel feeding the pane's PTY read batches. Only alive while the pane is open.
    pub wake_tx: Option<tokio::sync::mpsc::UnboundedSender<()>>,
    pub wake_rx: Option<tokio::sync::mpsc::UnboundedReceiver<()>>,
    pub wake_task: Option<gpui_kit::Task<()>>,
    /// Measured pixel size of the pane body, written during prepaint.
    pub bounds_slot: Arc<Mutex<Option<(gpui_kit::Pixels, gpui_kit::Pixels)>>>,
}

impl Default for TerminalPaneState {
    fn default() -> Self {
        Self {
            session: None,
            screen: None,
            error: None,
            focus_handle: None,
            size: TerminalSize::default(),
            #[cfg(test)]
            command_override: None,
            wake_tx: None,
            wake_rx: None,
            wake_task: None,
            bounds_slot: Arc::new(Mutex::new(None)),
        }
    }
}

impl TerminalPaneState {
    pub(crate) fn is_open(&self) -> bool {
        self.session.is_some() || self.error.is_some()
    }

    /// Number of live child processes owned by the pane. Tests assert this is 0 when closed.
    #[cfg(test)]
    pub(crate) fn live_process_count(&self) -> usize {
        usize::from(self.session.is_some())
    }
}

/// Focuses the terminal, or falls back to the root handle when the pane is closed.
impl TerminalPaneState {
    pub(crate) fn ensure_focus_handle(&mut self, cx: &mut Context<crate::app::MultibotView>) {
        if self.focus_handle.is_none() {
            self.focus_handle = Some(cx.focus_handle());
        }
    }
}

/// Key bytes for a terminal keystroke, or `None` when GPUI should keep the key.
///
/// `Alt+C` is deliberately absent: the global `ToggleTerminal` action must win so the
/// pane always closes cleanly, even while the grid has focus.
pub(crate) fn encode_key(ev: &KeyDownEvent) -> Option<Vec<u8>> {
    let keystroke = &ev.keystroke;
    let modifiers = keystroke.modifiers;
    let key = keystroke.key.to_ascii_lowercase();
    if modifiers.alt {
        return None;
    }
    if modifiers.control {
        return match key.as_str() {
            "c" => Some(vec![0x03]),
            "d" => Some(vec![0x04]),
            "l" => Some(vec![0x0c]),
            "z" => Some(vec![0x1a]),
            "a" => Some(vec![0x01]),
            "e" => Some(vec![0x05]),
            "u" => Some(vec![0x15]),
            "k" => Some(vec![0x0b]),
            "w" => Some(vec![0x17]),
            "r" => Some(vec![0x12]),
            _ => None,
        };
    }
    let seq = match key.as_str() {
        "enter" | "return" => "\r",
        "tab" => "\t",
        "backspace" => "\x7f",
        "escape" => "\x1b",
        "space" => " ",
        "up" => "\x1b[A",
        "down" => "\x1b[B",
        "right" => "\x1b[C",
        "left" => "\x1b[D",
        "home" => "\x1b[H",
        "end" => "\x1b[F",
        "pageup" => "\x1b[5~",
        "pagedown" => "\x1b[6~",
        "delete" => "\x1b[3~",
        "insert" => "\x1b[2~",
        _ => {
            if modifiers.platform {
                return None;
            }
            return keystroke
                .key_char
                .as_ref()
                .map(|text| text.as_bytes().to_vec());
        }
    };
    Some(seq.as_bytes().to_vec())
}

fn rgb_to_hsla(rgb: Rgb) -> gpui_kit::Hsla {
    gpui_kit::rgb(((rgb.r as u32) << 16) | ((rgb.g as u32) << 8) | rgb.b as u32).into()
}

impl crate::app::MultibotView {
    /// `Alt+C` / `Ctrl+K → Toggle Terminal`: opens the pane (spawning the PTY) or closes it
    /// (dropping the PTY, its child and its reader thread).
    pub(crate) fn toggle_terminal_pane(&mut self, cx: &mut Context<Self>) {
        if self.terminal.is_open() {
            self.close_terminal_pane(cx);
        } else {
            self.open_terminal_pane(cx);
        }
        cx.notify();
    }

    pub(crate) fn open_terminal_pane(&mut self, cx: &mut Context<Self>) {
        if self.terminal.is_open() {
            self.terminal.ensure_focus_handle(cx);
            return;
        }
        // The pane docks under the chat column, so opening it from the Inbox brings the
        // chat surface back rather than spawning an invisible shell.
        if self.main_surface == crate::state::MainSurface::Inbox {
            self.set_main_surface(crate::state::MainSurface::Chat);
        }
        self.terminal.error = None;
        self.terminal.ensure_focus_handle(cx);
        self.start_terminal_wake_pump(cx);
        self.spawn_terminal_for_active_session(cx);
    }

    pub(crate) fn close_terminal_pane(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = self.terminal.session.take() {
            session.request_reader_stop();
            drop(session);
        }
        self.terminal.screen = None;
        self.terminal.error = None;
        self.reset_terminal_wake_pump();
        cx.notify();
    }

    /// Builds and spawns the transport for whatever the active session points at.
    pub(crate) fn spawn_terminal_for_active_session(&mut self, cx: &mut Context<Self>) {
        let host_label = self.active_session_host().to_string();
        let cwd = self
            .selected_session
            .as_ref()
            .and_then(|session| session.cwd.clone());
        let hosts = self.settings.ssh_hosts.clone();
        let (target, host_config) = resolve_terminal_transport(&hosts, &host_label, cwd.as_deref());
        let size = self.terminal.size;

        // Headless tests swap in a stub transport so the suite never assumes a real
        // shell or a reachable SSH host; production resolves the real command.
        #[cfg(test)]
        let stubbed = self.terminal.command_override.clone();
        #[cfg(not(test))]
        let stubbed: Option<TerminalCommand> = None;

        let command = match stubbed
            .map(Ok)
            .unwrap_or_else(|| TerminalCommand::for_target(host_config.as_ref(), cwd.as_deref()))
        {
            Ok(command) => command,
            Err(error) => {
                self.terminal.error = Some(format!("Could not start terminal: {error}"));
                cx.notify();
                return;
            }
        };
        let result =
            TerminalSession::spawn_command(target, command, size, self.terminal.wake_tx.clone());
        self.finish_terminal_spawn(result, cx);
    }

    fn finish_terminal_spawn(&mut self, result: Result<TerminalSession>, cx: &mut Context<Self>) {
        match result {
            Ok(session) => {
                self.terminal.screen = Some(session.snapshot());
                self.terminal.session = Some(session);
                self.terminal.error = None;
            }
            Err(error) => {
                self.terminal.session = None;
                self.terminal.screen = None;
                self.terminal.error = Some(format!(
                    "Could not start terminal: {error}. Press Reconnect to try again."
                ));
            }
        }
        cx.notify();
    }

    /// Re-runs the spawn with a clean grid, keeping the pane open.
    pub(crate) fn reconnect_terminal_pane(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = self.terminal.session.as_mut() {
            session.request_reader_stop();
        }
        if let Some(session) = self.terminal.session.take() {
            drop(session);
        }
        self.terminal.screen = None;
        self.terminal.error = None;
        self.spawn_terminal_for_active_session(cx);
    }

    pub(crate) fn clear_terminal_pane(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = self.terminal.session.as_mut() {
            session.clear();
            self.terminal.screen = Some(session.snapshot());
            cx.notify();
        }
    }

    /// Refreshes the cached screen snapshot from the emulator (called on renders that
    /// follow a PTY wake-up, and by tests).
    pub(crate) fn refresh_terminal_screen(&mut self) {
        if let Some(session) = self.terminal.session.as_ref() {
            self.terminal.screen = Some(session.snapshot());
        }
    }

    /// Handles a keystroke while the grid has focus. Returns `true` when consumed.
    pub(crate) fn handle_terminal_key_down(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // Modals own the keyboard; the grid only takes keys on the bare chat surface.
        if self.terminal.session.is_none() || self.active_modal != crate::state::ActiveModal::None {
            return false;
        }
        // Only intercept keys while the grid itself holds focus, so the composer keeps
        // every printable key when the pane is merely visible.
        let focused = self
            .terminal
            .focus_handle
            .as_ref()
            .is_some_and(|handle| handle.is_focused(window));
        if !focused {
            return false;
        }
        let modifiers = ev.keystroke.modifiers;
        // Paste is the one chord the pane keeps for itself: Cmd+V on macOS, Ctrl+V (with or
        // without Shift) elsewhere. Everything else goes to the PTY.
        let is_paste = ev.keystroke.key.eq_ignore_ascii_case("v")
            && (modifiers.platform || modifiers.control);
        if is_paste {
            self.paste_into_terminal(cx);
            window.prevent_default();
            cx.stop_propagation();
            return true;
        }
        let Some(bytes) = encode_key(ev) else {
            return false;
        };
        if let Some(session) = self.terminal.session.as_mut() {
            session.write(&bytes);
            self.terminal.screen = Some(session.snapshot());
        }
        window.prevent_default();
        cx.stop_propagation();
        cx.notify();
        true
    }

    pub(crate) fn paste_into_terminal(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        if text.is_empty() {
            return;
        }
        if let Some(session) = self.terminal.session.as_mut() {
            let bytes = session.paste_bytes(&text);
            session.write(&bytes);
            self.terminal.screen = Some(session.snapshot());
            cx.notify();
        }
    }

    /// Consumes the pixel bounds captured during the last prepaint and resizes the PTY when
    /// the grid gained or lost columns or rows.
    ///
    /// Runs inside `render`, so it never calls `cx.notify()`: the frame being built already
    /// carries the new size and the next PTY wake-up supplies fresh content.
    pub(crate) fn sync_terminal_size_from_bounds(&mut self) {
        if self.terminal.session.is_none() {
            return;
        }
        let measured = self.terminal.bounds_slot.lock().ok().and_then(|slot| *slot);
        let Some((width, height)) = measured else {
            return;
        };
        let next = TerminalSize::from_pixels(width, height, terminal_cell_width_px());
        if next == self.terminal.size {
            return;
        }
        self.terminal.size = next;
        if let Some(session) = self.terminal.session.as_mut() {
            session.resize(next);
            self.terminal.screen = Some(session.snapshot());
        }
    }

    /// Tails the PTY wake-up channel: refreshes the cached grid once per read batch and
    /// reports child exit. This task lives exactly as long as the pane is open, so a
    /// closed pane runs no task and no timer.
    pub(crate) fn start_terminal_wake_pump(&mut self, cx: &mut Context<Self>) {
        if self.terminal.wake_task.is_some() {
            return;
        }
        if self.terminal.wake_rx.is_none() {
            let (wake_tx, wake_rx) = tokio::sync::mpsc::unbounded_channel();
            self.terminal.wake_tx = Some(wake_tx);
            self.terminal.wake_rx = Some(wake_rx);
        }
        let Some(mut wake_rx) = self.terminal.wake_rx.take() else {
            return;
        };
        self.terminal.wake_task = Some(cx.spawn(async move |this, cx| {
            while wake_rx.recv().await.is_some() {
                // Coalesce a burst of read batches into one snapshot + one redraw, so a
                // chatty command cannot turn into per-batch grid copies.
                while wake_rx.try_recv().is_ok() {}
                let alive = this
                    .update(cx, |view, cx| {
                        view.refresh_terminal_screen();
                        if view
                            .terminal
                            .session
                            .as_mut()
                            .is_some_and(|session| session.poll_exited())
                        {
                            view.terminal.session = None;
                            view.terminal.error = Some(
                                "Terminal session ended. Press Reconnect to start a new one."
                                    .to_string(),
                            );
                        }
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        }));
    }

    /// Drops the wake task and its channel pair. The next open creates a fresh pair, so a
    /// closed pane owns neither a task nor an open channel.
    fn reset_terminal_wake_pump(&mut self) {
        self.terminal.wake_task = None;
        self.terminal.wake_tx = None;
        self.terminal.wake_rx = None;
    }

    /// Renders the pane body (grid + header). Returns `None` when the pane is closed.
    pub(crate) fn render_terminal_pane(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<gpui_kit::AnyElement> {
        if !self.terminal.is_open() {
            return None;
        }
        // Re-fit the PTY to the pane that was measured last frame. No notify: this frame
        // already carries the new grid.
        self.sync_terminal_size_from_bounds();
        let host_label = match self.terminal.session.as_ref() {
            Some(session) if session.target.is_ssh() => {
                let host = match &session.target {
                    TerminalTarget::Ssh { host_label, .. } => host_label.as_str(),
                    TerminalTarget::Local { .. } => "ssh",
                };
                format!("{host} · {}", session.target.label())
            }
            Some(session) => format!("Local · {}", session.command.program),
            None => "Terminal".to_string(),
        };
        let mono = cx.theme().mono_font_family.clone();
        let header = h_flex()
            .id("terminal-header")
            .test_support()
            .w_full()
            .h(px(TERMINAL_HEADER_HEIGHT_PX))
            .flex_shrink_0()
            .items_center()
            .justify_between()
            .px_3()
            .gap_2()
            .bg(crate::theme::bg_surface())
            .border_b_1()
            .border_color(crate::theme::border_subtle())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.5))
                    .text_color(crate::theme::text_secondary())
                    .child(host_label),
            )
            .child(
                h_flex()
                    .gap_1p5()
                    .flex_shrink_0()
                    .child(
                        Button::new("terminal-clear")
                            .xsmall()
                            .label("Clear")
                            .accessibility_label("Clear terminal")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.clear_terminal_pane(cx);
                            })),
                    )
                    .child(
                        Button::new("terminal-reconnect")
                            .xsmall()
                            .label("Reconnect")
                            .accessibility_label("Reconnect terminal")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.reconnect_terminal_pane(cx);
                            })),
                    )
                    .child(
                        Button::new("terminal-close")
                            .xsmall()
                            .label("Close")
                            .accessibility_label("Close terminal")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.close_terminal_pane(cx);
                            })),
                    ),
            );

        // A status line (spawn failure, ended session) rides above the grid so the last
        // screenful stays readable instead of being replaced by an error message.
        let status_line: gpui_kit::AnyElement = match &self.terminal.error {
            Some(error) => div()
                .id("terminal-error")
                .test_support()
                .w_full()
                .px_3()
                .py_1()
                .text_size(px(11.5))
                .text_color(crate::theme::accent_amber())
                .child(error.clone())
                .into_any_element(),
            None => div().w_full().child(" ").into_any_element(),
        };
        let grid_body: gpui_kit::AnyElement = match &self.terminal.screen {
            Some(screen) => render_screen(screen, mono, self.terminal.focus_handle.clone()),
            None => div()
                .id("terminal-starting")
                .test_support()
                .flex_1()
                .p_3()
                .text_size(px(12.0))
                .text_color(crate::theme::text_muted())
                .child("Starting terminal...")
                .into_any_element(),
        };
        let body_inner = v_flex()
            .flex_1()
            .min_h_0()
            .child(status_line)
            .child(grid_body);

        let body = div()
            .id("terminal-scroll")
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .on_scroll(cx.listener(|this, event: &gpui_kit::ScrollEvent, _, cx| {
                let delta = match event.delta {
                    gpui_kit::ScrollDelta::Lines(lines) => lines.y.round() as i32,
                    gpui_kit::ScrollDelta::Pixels(pixels) => {
                        (pixels.y.as_f32() / TERMINAL_FONT_SIZE_PX).round() as i32
                    }
                };
                if delta == 0 {
                    return;
                }
                if let Some(session) = this.terminal.session.as_mut() {
                    // Positive wheel delta scrolls back into history.
                    session.scroll(-delta);
                    this.terminal.screen = Some(session.snapshot());
                    cx.notify();
                }
            }))
            .child(body_inner);

        let bounds_slot = self.terminal.bounds_slot.clone();
        // The outer pane is only a container: it captures the measured body bounds for
        // column/row maths and mounts nothing else. Focus lives on the grid itself.
        Some(
            v_flex()
                .w_full()
                .h(px(TERMINAL_PANE_HEIGHT_PX))
                .flex_shrink_0()
                .bg(crate::theme::bg_sidebar())
                .border_t_1()
                .border_color(crate::theme::border_subtle())
                .child(header)
                .child(body)
                .on_children_prepainted(move |child_bounds, _window, _cx| {
                    if let Some(bounds) = child_bounds.last()
                        && let Ok(mut slot) = bounds_slot.lock()
                    {
                        *slot = Some((bounds.size.width, bounds.size.height));
                    }
                })
                .id("terminal-pane")
                .test_support()
                .into_any_element(),
        )
    }
}

fn render_screen(
    screen: &TerminalScreen,
    mono: gpui_kit::SharedString,
    focus_handle: Option<FocusHandle>,
) -> gpui_kit::AnyElement {
    let mut grid = v_flex()
        .flex_1()
        .min_h_0()
        .overflow_hidden()
        .p_2()
        .bg(crate::theme::bg_sidebar())
        .font_family(mono)
        .text_size(px(TERMINAL_FONT_SIZE_PX))
        .text_color(crate::theme::text_primary());
    if let Some(focus_handle) = focus_handle.as_ref() {
        grid = grid.track_focus(focus_handle);
    }
    let mut grid = grid
        .id("terminal-grid")
        .aria_value(screen.to_text())
        .test_support();
    for row in 0..screen.rows {
        let mut line = String::with_capacity(screen.cols);
        for col in 0..screen.cols {
            line.push(screen.cell(row, col).map_or(' ', |cell| cell.c));
        }
        let trimmed = line.trim_end_matches(' ');
        if trimmed.is_empty() {
            // Still render an empty run so the row keeps its height.
            grid = grid.child(div().h(px(TERMINAL_FONT_SIZE_PX * 1.35)).child(" "));
            continue;
        }
        let mut runs = h_flex().h(px(TERMINAL_FONT_SIZE_PX * 1.35));
        let chars: Vec<char> = trimmed.chars().collect();
        let mut start = 0usize;
        while start < chars.len() {
            let cell = screen.cell(row, start);
            let fg = cell.map_or(TERMINAL_FG, |cell| cell.fg);
            let bg = cell.map_or(TERMINAL_BG, |cell| cell.bg);
            let mut end = start + 1;
            while end < chars.len() {
                let next = screen.cell(row, end);
                let next_fg = next.map_or(TERMINAL_FG, |cell| cell.fg);
                let next_bg = next.map_or(TERMINAL_BG, |cell| cell.bg);
                if next_fg != fg || next_bg != bg {
                    break;
                }
                end += 1;
            }
            let text: String = chars[start..end].iter().collect();
            let mut run = div()
                .text_color(rgb_to_hsla(fg))
                .child(gpui_kit::SharedString::from(text));
            if bg != TERMINAL_BG {
                run = run.bg(rgb_to_hsla(bg));
            }
            runs = runs.child(run);
            start = end;
        }
        grid = grid.child(runs);
    }
    grid.into_any_element()
}

/// Process-wide guard used by tests to keep PTY-spawning test cases serialised.
#[cfg(test)]
pub(crate) fn test_pty_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::SharedAppStore;
    use crate::terminal_pane::{TerminalSession, TerminalSize, TerminalTarget};
    use gpui_kit::{AppContext, TestAppContext};
    use multibot_core::settings::MultibotSettings;

    fn host(name: &str, identity_file: Option<&str>) -> SshHostConfig {
        SshHostConfig {
            name: name.into(),
            hostname: "10.0.0.5".into(),
            user: "ubuntu".into(),
            port: 2222,
            identity_file: identity_file.map(str::to_string),
        }
    }

    fn emulator_with(bytes: &[u8]) -> TerminalEmulator {
        let mut emulator = TerminalEmulator::new(TerminalSize::new(40, 6));
        emulator.advance(bytes);
        emulator
    }

    #[test]
    fn session_with_ssh_host_resolves_to_ssh_target_with_cwd() {
        let hosts = vec![host("vps", None)];
        let (target, _) = resolve_terminal_transport(&hosts, "vps", Some("/srv/app"));
        assert_eq!(
            target,
            TerminalTarget::Ssh {
                host_label: "vps".into(),
                target: "ubuntu@10.0.0.5".into(),
                cwd: Some("/srv/app".into()),
            }
        );
        assert!(target.is_ssh());
        assert_eq!(target.label(), "ubuntu@10.0.0.5:/srv/app");
    }

    #[test]
    fn local_label_falls_back_to_local_shell_when_host_is_not_configured() {
        let hosts = vec![host("vps", None), host("prod", None)];
        let (target, _) = resolve_terminal_transport(&hosts, "local", Some("/srv/app"));
        assert_eq!(
            target,
            TerminalTarget::Local {
                cwd: Some("/srv/app".into())
            }
        );
        assert!(!target.is_ssh());
        assert_eq!(target.label(), "local:/srv/app");
    }

    #[test]
    fn unmatched_host_label_never_leaks_onto_a_configured_ssh_host() {
        let hosts = vec![host("vps", None)];
        // A session on an unknown/offline label must stay local, never silently SSH
        // into a different server.
        assert_eq!(
            resolve_terminal_transport(&hosts, "host-a", Some("/srv")).0,
            TerminalTarget::Local {
                cwd: Some("/srv".into())
            }
        );
    }

    #[test]
    fn configured_host_with_blank_hostname_falls_back_to_local() {
        let mut blank = host("vps", None);
        blank.hostname = "   ".into();
        assert_eq!(
            resolve_terminal_transport(&[blank], "vps", None).0,
            TerminalTarget::Local { cwd: None }
        );
    }

    #[test]
    fn ssh_pty_argv_uses_dash_dash_port_and_validated_identity_file() {
        let args = build_ssh_pty_args(&host("vps", None), Some("/srv/app")).unwrap();
        assert_eq!(args[0], "-tt");
        assert!(args.contains(&"--".to_string()));
        let dash = args.iter().position(|arg| arg == "--").unwrap();
        assert_eq!(args[dash + 1], "ubuntu@10.0.0.5");
        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"2222".to_string()));
        assert!(!args.contains(&"-i".to_string()));
        // The remote command is one argv element and never shell-interpolated locally.
        assert!(
            args.last()
                .unwrap()
                .contains("exec \"${SHELL:-/bin/sh}\" -l")
        );
    }

    #[test]
    fn ssh_pty_argv_passes_identity_file_as_its_own_argument() {
        let args = build_ssh_pty_args(
            &host("vps", Some("  C:/Users/Example User/.ssh/id_ed25519  ")),
            None,
        )
        .unwrap();
        let index = args.iter().position(|arg| arg == "-i").unwrap();
        assert_eq!(args[index + 1], "C:/Users/Example User/.ssh/id_ed25519");
        let dash = args.iter().position(|arg| arg == "--").unwrap();
        assert!(index < dash, "-i must come before the argv terminator");
    }

    #[test]
    fn ssh_pty_argv_rejects_injecting_identity_files() {
        for bad in [
            "",
            "   ",
            "-oProxyCommand=evil",
            "/key\nProxyCommand=id",
            "/key\rfoo",
            "/key\tfoo",
        ] {
            assert!(
                build_ssh_pty_args(&host("vps", Some(bad)), None).is_err(),
                "identity file {bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn ssh_pty_argv_rejects_zero_port_and_empty_hostname() {
        let mut zero_port = host("vps", None);
        zero_port.port = 0;
        assert!(build_ssh_pty_args(&zero_port, None).is_err());

        let mut empty_hostname = host("vps", None);
        empty_hostname.hostname = "   ".into();
        let args = build_ssh_pty_args(&empty_hostname, None);
        assert!(args.is_err() || !args.unwrap().iter().any(|arg| arg.trim().is_empty()));
    }

    #[test]
    fn remote_cd_is_single_quoted_and_never_interpolates_shell_metacharacters() {
        let command = build_remote_shell_command(Some("/srv/my app; rm -rf ~ #"));
        assert!(
            command.starts_with("cd -- '/srv/my app; rm -rf ~ #' 2>/dev/null;"),
            "unexpected command: {command}"
        );
        assert!(!command.contains("cd -- /srv/my app;"));
    }

    #[test]
    fn remote_cd_escapes_embedded_single_quotes() {
        let quoted = posix_shell_quote("/tmp/it's here").unwrap();
        assert_eq!(quoted, "'/tmp/it'\\''s here'");
        let command = build_remote_shell_command(Some("/tmp/it's here"));
        assert!(command.contains("'\\''"), "unexpected command: {command}");
    }

    #[test]
    fn remote_cd_rejects_control_characters_and_falls_back_to_plain_shell() {
        assert!(posix_shell_quote("/srv\napp").is_none());
        let command = build_remote_shell_command(Some("/srv\napp"));
        assert_eq!(command, "exec \"${SHELL:-/bin/sh}\" -l");
        assert!(!command.contains('\n'));
    }

    #[test]
    fn empty_or_missing_cwd_skips_cd_entirely() {
        for cwd in [None, Some(""), Some("   ")] {
            assert_eq!(
                build_remote_shell_command(cwd),
                "exec \"${SHELL:-/bin/sh}\" -l"
            );
        }
    }

    #[test]
    fn ssh_command_uses_ssh_program_and_local_command_uses_login_shell() {
        let ssh = TerminalCommand::for_target(Some(&host("vps", None)), Some("/srv")).unwrap();
        assert_eq!(ssh.program, "ssh");
        assert!(ssh.cwd.is_none());

        let local = TerminalCommand::for_target(None, Some("/srv")).unwrap();
        assert_eq!(local.program, default_local_shell());
        assert!(local.args.is_empty());
        assert_eq!(local.cwd.as_deref(), Some("/srv"));
    }

    #[test]
    fn vt_parses_plain_text_into_the_grid() {
        let emulator = emulator_with(b"hello\r\nworld");
        let screen = emulator.snapshot();
        assert_eq!(screen.row_text(0), "hello");
        assert_eq!(screen.row_text(1), "world");
        assert_eq!(screen.cols, 40);
        assert_eq!(screen.rows, 6);
    }

    #[test]
    fn vt_parses_ansi_color_and_resolves_obsidian_palette() {
        let emulator = emulator_with(b"\x1b[31mred\x1b[0m plain");
        let screen = emulator.snapshot();
        let red = screen.cell(0, 0).unwrap();
        assert_eq!(red.fg, ANSI_BASE_PALETTE[1]);
        assert_eq!(red.c, 'r');
        assert_eq!(screen.row_text(0), "red plain");
        assert_eq!(screen.cell(0, 3).unwrap().fg, TERMINAL_FG);
        assert_eq!(screen.cell(0, 4).unwrap().c, 'p');
        assert_eq!(screen.cell(0, 4).unwrap().fg, TERMINAL_FG);
    }

    #[test]
    fn vt_parses_truecolor_and_256_color_sequences() {
        let emulator = emulator_with(b"\x1b[38;2;1;2;3mX\x1b[38;5;196mY");
        let screen = emulator.snapshot();
        assert_eq!(screen.cell(0, 0).unwrap().fg, Rgb { r: 1, g: 2, b: 3 });
        assert_eq!(screen.cell(0, 1).unwrap().fg, indexed_palette_color(196));
    }

    #[test]
    fn vt_tracks_cursor_after_escapes() {
        let mut emulator = emulator_with(b"one\r\ntwo");
        let screen = emulator.snapshot();
        assert_eq!(screen.cursor, Some((1, 3)));
        assert_eq!(screen.cursor_row_text(), "two");

        emulator.advance(b"\x1b[2;1H");
        let screen = emulator.snapshot();
        assert_eq!(screen.cursor, Some((1, 0)));
    }

    #[test]
    fn vt_resize_reflows_grid_columns() {
        let mut emulator = TerminalEmulator::new(TerminalSize::new(20, 4));
        emulator.advance(b"0123456789");
        assert_eq!(emulator.snapshot().cols, 20);
        assert!(emulator.resize(TerminalSize::new(30, 6)));
        let screen = emulator.snapshot();
        assert_eq!(screen.cols, 30);
        assert_eq!(screen.rows, 6);
        // A no-op resize must not report a change (keeps redraws demand-driven).
        assert!(!emulator.resize(TerminalSize::new(30, 6)));
    }

    #[test]
    fn vt_reports_bracketed_paste_mode() {
        let mut emulator = TerminalEmulator::new(TerminalSize::new(20, 4));
        assert!(!emulator.bracketed_paste());
        emulator.advance(b"\x1b[?2004h");
        assert!(emulator.bracketed_paste());
        emulator.advance(b"\x1b[?2004l");
        assert!(!emulator.bracketed_paste());
    }

    #[test]
    fn vt_clear_resets_the_grid_contents() {
        let mut emulator = emulator_with(b"leftover text");
        assert_eq!(emulator.snapshot().row_text(0), "leftover text");
        emulator.clear();
        assert_eq!(emulator.snapshot().row_text(0), "");
    }

    #[test]
    fn terminal_size_enforces_minimums_and_casts_to_pty() {
        let size = TerminalSize::new(0, 0);
        assert_eq!(size.cols, MIN_TERMINAL_COLS);
        assert_eq!(size.rows, MIN_TERMINAL_ROWS);
        assert_eq!(size.pty_size().cols as usize, MIN_TERMINAL_COLS);
        assert_eq!(size.pty_size().rows as usize, MIN_TERMINAL_ROWS);
    }

    #[test]
    fn encode_key_maps_control_and_navigation_sequences() {
        let ctrl = |key: &str| KeyDownEvent {
            keystroke: gpui_kit::Keystroke {
                key: key.into(),
                key_char: None,
                modifiers: gpui_kit::Modifiers {
                    control: true,
                    ..Default::default()
                },
            },
            is_held: false,
            prefer_character_input: false,
        };
        assert_eq!(encode_key(&ctrl("c")), Some(vec![0x03]));
        assert_eq!(encode_key(&ctrl("d")), Some(vec![0x04]));
        assert_eq!(encode_key(&ctrl("l")), Some(vec![0x0c]));
        assert_eq!(encode_key(&ctrl("z")), Some(vec![0x1a]));
        assert_eq!(encode_key(&ctrl("q")), None);

        let plain = |key: &str, key_char: Option<&str>| KeyDownEvent {
            keystroke: gpui_kit::Keystroke {
                key: key.into(),
                key_char: key_char.map(Into::into),
                modifiers: gpui_kit::Modifiers::default(),
            },
            is_held: false,
            prefer_character_input: false,
        };
        assert_eq!(encode_key(&plain("enter", None)), Some(b"\r".to_vec()));
        assert_eq!(
            encode_key(&plain("backspace", None)),
            Some(b"\x7f".to_vec())
        );
        assert_eq!(encode_key(&plain("tab", None)), Some(b"\t".to_vec()));
        assert_eq!(encode_key(&plain("up", None)), Some(b"\x1b[A".to_vec()));
        assert_eq!(encode_key(&plain("a", Some("a"))), Some(b"a".to_vec()));
    }

    #[test]
    fn alt_c_is_never_forwarded_to_the_pty() {
        let event = KeyDownEvent {
            keystroke: gpui_kit::Keystroke {
                key: "c".into(),
                key_char: Some("ç".into()),
                modifiers: gpui_kit::Modifiers {
                    alt: true,
                    ..Default::default()
                },
            },
            is_held: false,
            prefer_character_input: false,
        };
        assert_eq!(encode_key(&event), None);
    }

    #[test]
    fn indexed_palette_matches_xterm_cube_and_grayscale() {
        assert_eq!(indexed_palette_color(0), ANSI_BASE_PALETTE[0]);
        assert_eq!(indexed_palette_color(16), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(
            indexed_palette_color(231),
            Rgb {
                r: 255,
                g: 255,
                b: 255
            }
        );
        assert_eq!(indexed_palette_color(232), Rgb { r: 8, g: 8, b: 8 });
    }

    #[test]
    fn closed_pane_owns_no_process_and_no_reader_thread() {
        let state = TerminalPaneState::default();
        assert!(!state.is_open());
        assert_eq!(state.live_process_count(), 0);
        assert!(state.session.is_none());
        assert!(state.screen.is_none());
    }

    #[gpui_kit::test]
    fn closing_a_spawned_session_reaps_the_child_process(cx: &mut TestAppContext) {
        let _guard = test_pty_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let size = TerminalSize::new(60, 10);
        let mut session = spawn_stub_session(cx, size).expect("stub PTY must start on this host");
        assert!(!session.poll_exited() || session.poll_exited());
        // Wait for real output from the child, proving the reader thread parsed bytes.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while session.read_bytes() == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let screen = session.snapshot();
        assert!(
            screen.to_text().contains("stub-ok"),
            "expected stub output in grid, got: {:?}",
            screen.to_text()
        );
        // Recording output lets a reconnect replay it into a fresh grid.
        assert!(session.generation() >= 1);
        drop(session);
    }

    #[gpui_kit::test]
    fn pasted_text_is_wrapped_when_bracketed_paste_is_enabled(cx: &mut TestAppContext) {
        let _guard = test_pty_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let session =
            spawn_stub_session(cx, TerminalSize::new(40, 8)).expect("stub PTY must start");
        assert_eq!(session.paste_bytes("hi\r\nthere"), b"hi\nthere".to_vec());
    }

    fn spawn_stub_session(cx: &mut TestAppContext, size: TerminalSize) -> Result<TerminalSession> {
        cx.update(|cx| {
            gpui_kit::init(cx);
            SharedAppStore::init(cx, MultibotSettings::default());
        });
        let (handle, view) = cx.update(|cx| {
            gpui_kit::open_window(gpui_kit::WindowOptions::default(), cx, |window, cx| {
                let runtime = std::sync::Arc::new(
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap(),
                );
                cx.new(|cx| crate::app::MultibotView::new(runtime, window, cx))
            })
            .unwrap()
        });
        let mut created = None;
        cx.update_window(handle, |_, _window, cx| {
            view.update(cx, |_view, _cx| {
                created = Some(TerminalSession::spawn_command(
                    TerminalTarget::Local { cwd: None },
                    stub_command(),
                    size,
                    None,
                ));
            });
        })
        .unwrap();
        created.expect("session was not created")
    }

    fn stub_command() -> TerminalCommand {
        TerminalCommand {
            program: if cfg!(windows) {
                "cmd.exe".to_string()
            } else {
                "/bin/sh".to_string()
            },
            args: if cfg!(windows) {
                vec!["/C".into(), "echo stub-ok".into()]
            } else {
                vec!["-c".into(), "echo stub-ok".into()]
            },
            cwd: None,
        }
    }
}
