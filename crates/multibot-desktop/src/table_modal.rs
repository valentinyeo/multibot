use crate::app::MultibotView;
use crate::icons::{self, lucide_icon};
use crate::theme::{self, RADIUS_CHIP, RADIUS_MODAL, RADIUS_ROW};
use gpui_kit::{
    AnyElement, Context, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent, MouseButton,
    ParentElement, Role, ScrollDelta, ScrollEvent, SharedString, StatefulInteractiveElement,
    Styled, TestSupportExt, Window,
    component::{Sizable, button::Button, h_flex, text::TableData, v_flex},
    div, hsla,
    prelude::FluentBuilder,
    px,
};
use multibot_proto::{ChatMessage, MessageBlock};

pub const MAX_TABLE_COLUMNS: usize = 16;
pub const DEFAULT_MODAL_LINE_STEP: i32 = 54;
pub const FAR_SCROLL: i32 = 1 << 28;
const INLINE_PREVIEW_MAX_ROWS: usize = 20;
const MODAL_DEFAULT_VIEW_W: i32 = 960;
const MODAL_DEFAULT_VIEW_H: i32 = 520;
const ESTIMATED_ROW_HEIGHT: i32 = 34;
const ESTIMATED_CHAR_WIDTH: i32 = 8;
const MIN_COL_WIDTH: i32 = 96;
const MAX_COL_WIDTH: i32 = 340;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableSource {
    Pipe,
    Box,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColumnAlign {
    #[default]
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTable {
    pub source: TableSource,
    pub columns: usize,
    pub alignments: Vec<ColumnAlign>,
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub raw_markdown: String,
}

impl ParsedTable {
    /// Builds a `ParsedTable` from GPUI's `TableData` (preferring its canonical GFM
    /// markdown so alignments are preserved, falling back to plain headers/rows).
    pub fn from_gpui_table_data(data: &TableData) -> Option<Self> {
        if !data.markdown.trim().is_empty()
            && let Some(parsed) = parse_table(&data.markdown)
        {
            return Some(parsed);
        }
        let cols = data.headers.len().min(MAX_TABLE_COLUMNS);
        if cols == 0 {
            return None;
        }
        let headers: Vec<String> = data.headers.iter().take(cols).cloned().collect();
        let rows: Vec<Vec<String>> = data
            .rows
            .iter()
            .map(|r| {
                let mut normalized: Vec<String> = r.iter().take(cols).cloned().collect();
                while normalized.len() < cols {
                    normalized.push(String::new());
                }
                normalized
            })
            .collect();
        Some(Self {
            source: TableSource::Pipe,
            columns: cols,
            alignments: vec![ColumnAlign::Left; cols],
            headers,
            rows,
            raw_markdown: data.markdown.clone(),
        })
    }

    pub fn column_widths(&self) -> Vec<i32> {
        let mut widths = vec![MIN_COL_WIDTH; self.columns];
        for (c, h) in self.headers.iter().enumerate().take(self.columns) {
            let est = (h.chars().count() as i32) * ESTIMATED_CHAR_WIDTH + 32;
            widths[c] = widths[c].max(est).min(MAX_COL_WIDTH);
        }
        for row in &self.rows {
            for (c, cell) in row.iter().enumerate().take(self.columns) {
                let est = (cell.chars().count() as i32) * ESTIMATED_CHAR_WIDTH + 28;
                widths[c] = widths[c].max(est).min(MAX_COL_WIDTH);
            }
        }
        widths
    }

    pub fn estimated_content_size(&self) -> (i32, i32) {
        let widths = self.column_widths();
        let content_w: i32 = widths.into_iter().sum::<i32>().max(MIN_COL_WIDTH);
        let total_rows = (self.rows.len() + 1) as i32;
        let content_h = (total_rows * ESTIMATED_ROW_HEIGHT).max(ESTIMATED_ROW_HEIGHT);
        (content_w, content_h)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedTable {
    pub byte_start: usize,
    pub byte_end: usize,
    pub table: ParsedTable,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TableViewportCandidate {
    pub index: usize,
    pub top: f32,
    pub bottom: f32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TableScroll {
    pub x: i32,
    pub y: i32,
}

impl TableScroll {
    /// Clamps scroll offsets into `[0, max(0, content - view)]` using `i64` arithmetic.
    /// Returns `true` iff `x` or `y` actually changed.
    pub fn scroll_by(
        &mut self,
        content_w: i32,
        content_h: i32,
        view_w: i32,
        view_h: i32,
        dx: i32,
        dy: i32,
    ) -> bool {
        let max_x = (content_w as i64 - view_w as i64).max(0);
        let max_y = (content_h as i64 - view_h as i64).max(0);
        let next_x = (self.x as i64 + dx as i64).clamp(0, max_x) as i32;
        let next_y = (self.y as i64 + dy as i64).clamp(0, max_y) as i32;
        let moved = next_x != self.x || next_y != self.y;
        self.x = next_x;
        self.y = next_y;
        moved
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableModalCommand {
    None,
    Close,
    Scroll { dx: i32, dy: i32 },
}

#[derive(Debug, Clone, Default)]
pub(crate) struct TableModalState {
    pub open: bool,
    pub title: String,
    pub table: Option<ParsedTable>,
    pub scroll: TableScroll,
    pub content_w: i32,
    pub content_h: i32,
    pub view_w: i32,
    pub view_h: i32,
    pub focus_handle: Option<FocusHandle>,
}

impl TableModalState {
    pub(crate) fn is_open(&self) -> bool {
        self.open && self.table.is_some()
    }

    pub(crate) fn close(&mut self) {
        self.open = false;
        self.table = None;
        self.scroll = TableScroll::default();
    }

    pub(crate) fn open_table(&mut self, table: ParsedTable, title: impl Into<String>) {
        let (content_w, content_h) = table.estimated_content_size();
        self.view_w = content_w.clamp(360, MODAL_DEFAULT_VIEW_W);
        self.view_h = content_h.clamp(160, MODAL_DEFAULT_VIEW_H);
        self.content_w = content_w;
        self.content_h = content_h;
        self.title = title.into();
        self.table = Some(table);
        self.scroll = TableScroll::default();
        self.open = true;
    }

    pub(crate) fn handle_key_command(&self, key: &str, line_step: i32) -> TableModalCommand {
        let step = line_step.max(1);
        let page = self.view_h.max(step);
        match key.to_ascii_lowercase().as_str() {
            "escape" => TableModalCommand::Close,
            "up" => TableModalCommand::Scroll { dx: 0, dy: -step },
            "down" => TableModalCommand::Scroll { dx: 0, dy: step },
            "pageup" => TableModalCommand::Scroll { dx: 0, dy: -page },
            "pagedown" => TableModalCommand::Scroll { dx: 0, dy: page },
            "home" => TableModalCommand::Scroll {
                dx: 0,
                dy: -FAR_SCROLL,
            },
            "end" => TableModalCommand::Scroll {
                dx: 0,
                dy: FAR_SCROLL,
            },
            "left" => TableModalCommand::Scroll { dx: -step, dy: 0 },
            "right" => TableModalCommand::Scroll { dx: step, dy: 0 },
            _ => TableModalCommand::None,
        }
    }

    pub(crate) fn apply_command(&mut self, cmd: TableModalCommand) -> bool {
        match cmd {
            TableModalCommand::None => false,
            TableModalCommand::Close => {
                let was_open = self.open;
                self.close();
                was_open
            }
            TableModalCommand::Scroll { dx, dy } => self.scroll.scroll_by(
                self.content_w,
                self.content_h,
                self.view_w,
                self.view_h,
                dx,
                dy,
            ),
        }
    }

    pub(crate) fn handle_wheel(
        &mut self,
        delta_x: f32,
        delta_y: f32,
        shift: bool,
        step: i32,
    ) -> bool {
        let scale = step.max(1) as f32;
        let (dx, dy) = if shift || delta_x.abs() > delta_y.abs() {
            let raw = if shift && delta_x == 0.0 {
                -delta_y
            } else {
                -delta_x
            };
            ((raw * scale).round() as i32, 0)
        } else {
            (0, (-delta_y * scale).round() as i32)
        };
        self.scroll.scroll_by(
            self.content_w,
            self.content_h,
            self.view_w,
            self.view_h,
            dx,
            dy,
        )
    }
}

/// Detects a CommonMark opening/closing code fence (3+ `` ` `` or `~` after up to 3 spaces).
fn detect_fence(line: &str) -> Option<(char, usize, &str)> {
    let trimmed_cr = line.strip_suffix('\r').unwrap_or(line);
    let leading_spaces = trimmed_cr.bytes().take_while(|&b| b == b' ').count();
    if leading_spaces > 3 {
        return None;
    }
    let rest = &trimmed_cr[leading_spaces..];
    let first = rest.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let count = rest.chars().take_while(|&c| c == first).count();
    if count < 3 {
        return None;
    }
    let info = &rest[count..];
    if first == '`' && info.contains('`') {
        return None;
    }
    Some((first, count, info))
}

fn next_line_span(text: &str, start: usize) -> (usize, usize) {
    match text[start..].find('\n') {
        Some(rel) => (start + rel, start + rel + 1),
        None => (text.len(), text.len()),
    }
}

/// Extracts all GFM pipe tables and Unicode box-drawing tables from `text`,
/// strictly ignoring tables inside `` ``` `` or `~~~` fenced code blocks.
pub fn extract_tables_from_markdown(text: &str) -> Vec<ExtractedTable> {
    let mut results = Vec::new();
    let mut pos = 0usize;
    let mut fence: Option<(char, usize)> = None;

    while pos < text.len() {
        let (line_end, next_pos) = next_line_span(text, pos);
        let line = &text[pos..line_end];

        if let Some((fch, flen)) = fence {
            if let Some((ch, count, info)) = detect_fence(line)
                && ch == fch
                && count >= flen
                && info.trim().is_empty()
            {
                fence = None;
            }
            pos = next_pos;
            continue;
        }

        if let Some((ch, count, _)) = detect_fence(line) {
            fence = Some((ch, count));
            pos = next_pos;
            continue;
        }

        if let Some((table, consumed)) = parse_table_prefix(&text[pos..]) {
            let byte_end = pos + consumed;
            results.push(ExtractedTable {
                byte_start: pos,
                byte_end,
                table,
            });
            pos = if byte_end < text.len() && text.as_bytes()[byte_end] == b'\n' {
                byte_end + 1
            } else if byte_end > pos {
                byte_end
            } else {
                next_pos
            };
        } else {
            pos = next_pos;
        }
    }

    results
}

/// Parses a standalone GFM pipe or Unicode box-drawing table.
pub fn parse_table(text: &str) -> Option<ParsedTable> {
    let trimmed_start = text.trim_start_matches(['\r', '\n']);
    let (table, consumed) = parse_table_prefix(trimmed_start)?;
    if !trimmed_start[consumed..].trim().is_empty() {
        return None;
    }
    Some(table)
}

pub fn parse_table_prefix(slice: &str) -> Option<(ParsedTable, usize)> {
    parse_box_table_prefix(slice).or_else(|| parse_pipe_table_prefix(slice))
}

fn trim_cell_ws(s: &str) -> &str {
    s.trim_matches([' ', '\t', '\r'])
}

/// Returns `true` if `pos` sits immediately after an unescaped `|` plus optional
/// horizontal ASCII whitespace (`' '` / `'\t'`), i.e., at the start of a subsequent
/// pipe-table cell.
fn is_at_pipe_cell_start(bytes: &[u8], pos: usize) -> bool {
    let mut k = pos;
    while k > 0 && matches!(bytes[k - 1], b' ' | b'\t') {
        k -= 1;
    }
    if k == 0 || bytes[k - 1] != b'|' {
        return false;
    }
    let pipe_idx = k - 1;
    let mut backslashes = 0usize;
    let mut idx = pipe_idx;
    while idx > 0 && bytes[idx - 1] == b'\\' {
        backslashes += 1;
        idx -= 1;
    }
    backslashes % 2 == 0
}

/// Returns `true` if `byte` can immediately follow a closing inline code span backtick
/// run (ASCII whitespace, `|`, closing/separator/Markdown wrapper punctuation, or
/// non-ASCII UTF-8 bytes), distinguishing a real closing run like `` `|` operator ``,
/// `` (`|`) ``, `` **`|`** ``, or `` `|`—or `` from a new cell's opening run like
/// `` | `b `` or `` | `b` ``.
fn is_code_span_close_follower(byte: u8) -> bool {
    matches!(
        byte,
        b' ' | b'\t'
            | b'|'
            | b','
            | b'.'
            | b';'
            | b':'
            | b')'
            | b']'
            | b'}'
            | b'>'
            | b'!'
            | b'?'
            | b'*'
            | b'_'
            | b'~'
            | b'"'
            | b'\''
            | b'/'
            | b'-'
    ) || !byte.is_ascii()
}

/// Returns `true` iff `bytes[after_open..]` contains a contiguous closing run of backticks
/// of exact length `run` (per CommonMark inline code span rules), rejecting any candidate
/// matching-length run that sits at the start of a later pipe-table cell (e.g. `| `b`).
fn has_closing_backtick_run(bytes: &[u8], after_open: usize, run: usize) -> bool {
    let mut j = after_open;
    while j < bytes.len() {
        if bytes[j] == b'`' {
            let start = j;
            while j < bytes.len() && bytes[j] == b'`' {
                j += 1;
            }
            if j - start == run {
                if is_at_pipe_cell_start(bytes, start)
                    && j < bytes.len()
                    && !is_code_span_close_follower(bytes[j])
                {
                    return false;
                }
                return true;
            }
        } else {
            j += 1;
        }
    }
    false
}

fn has_closing_backtick_run_chars(chars: &[char], after_open: usize, run: usize) -> bool {
    let mut j = after_open;
    while j < chars.len() {
        if chars[j] == '`' {
            let start = j;
            while j < chars.len() && chars[j] == '`' {
                j += 1;
            }
            if j - start == run {
                return true;
            }
        } else {
            j += 1;
        }
    }
    false
}

/// Splits a GFM pipe table row into cells, respecting escaped `\|` and inline
/// backtick code spans (e.g. `` `a|b` `` or ``` ``a|b`` ```). Unclosed backtick runs
/// are emitted literally per CommonMark so they never swallow subsequent column pipes.
fn split_pipe_cells(line: &str) -> Vec<String> {
    let trimmed = trim_cell_ws(line);
    if trimmed.is_empty() {
        return Vec::new();
    }

    // Strip optional leading `|`
    let inner = trimmed.strip_prefix('|').unwrap_or(trimmed);

    // Strip optional trailing unescaped `|` that is not inside a closed backtick span
    let bytes = inner.as_bytes();
    let mut end_limit = bytes.len();
    if bytes.last() == Some(&b'|') {
        let mut backslashes = 0usize;
        let mut idx = bytes.len().saturating_sub(1);
        while idx > 0 && bytes[idx - 1] == b'\\' {
            backslashes += 1;
            idx -= 1;
        }
        if backslashes % 2 == 0 {
            let prefix = &bytes[..bytes.len() - 1];
            let mut in_ticks: Option<usize> = None;
            let mut i = 0usize;
            while i < prefix.len() {
                if in_ticks.is_none() && prefix[i] == b'\\' && i + 1 < prefix.len() {
                    i += 2;
                    continue;
                }
                if prefix[i] == b'`' {
                    let start_t = i;
                    while i < prefix.len() && prefix[i] == b'`' {
                        i += 1;
                    }
                    let run = i - start_t;
                    match in_ticks {
                        None => {
                            if has_closing_backtick_run(prefix, i, run) {
                                in_ticks = Some(run);
                            }
                        }
                        Some(open) if open == run => in_ticks = None,
                        _ => {}
                    }
                    continue;
                }
                i += 1;
            }
            if in_ticks.is_none() {
                end_limit = bytes.len() - 1;
            }
        }
    }

    let body = &inner[..end_limit];
    let b = body.as_bytes();
    let mut cells = Vec::new();
    let mut cell_start = 0usize;
    let mut i = 0usize;
    let mut in_ticks: Option<usize> = None;

    while i < b.len() {
        if in_ticks.is_none() && b[i] == b'\\' && i + 1 < b.len() {
            i += 2;
            continue;
        }
        if b[i] == b'`' {
            let start_t = i;
            while i < b.len() && b[i] == b'`' {
                i += 1;
            }
            let run = i - start_t;
            match in_ticks {
                None => {
                    if has_closing_backtick_run(b, i, run) {
                        in_ticks = Some(run);
                    }
                }
                Some(open) if open == run => in_ticks = None,
                _ => {}
            }
            continue;
        }
        if in_ticks.is_none() && b[i] == b'|' {
            cells.push(unescape_pipe_cell(trim_cell_ws(&body[cell_start..i])));
            cell_start = i + 1;
        }
        i += 1;
    }
    cells.push(unescape_pipe_cell(trim_cell_ws(&body[cell_start..])));
    cells
}

fn unescape_pipe_cell(cell: &str) -> String {
    if !cell.contains("\\|") {
        return cell.to_string();
    }
    let mut out = String::with_capacity(cell.len());
    let chars: Vec<char> = cell.chars().collect();
    let mut i = 0usize;
    let mut in_ticks: Option<usize> = None;
    while i < chars.len() {
        if chars[i] == '`' {
            let start = i;
            while i < chars.len() && chars[i] == '`' {
                out.push('`');
                i += 1;
            }
            let run = i - start;
            match in_ticks {
                None => {
                    if has_closing_backtick_run_chars(&chars, i, run) {
                        in_ticks = Some(run);
                    }
                }
                Some(open) if open == run => in_ticks = None,
                _ => {}
            }
            continue;
        }
        if in_ticks.is_none() && chars[i] == '\\' && i + 1 < chars.len() && chars[i + 1] == '|' {
            out.push('|');
            i += 2;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn parse_delimiter_cell(cell: &str) -> Option<ColumnAlign> {
    let trimmed = trim_cell_ws(cell);
    let left_colon = trimmed.starts_with(':');
    let right_colon = trimmed.ends_with(':');
    let inner = trimmed.strip_prefix(':').unwrap_or(trimmed);
    let inner = inner.strip_suffix(':').unwrap_or(inner);
    if inner.len() < 3 || !inner.bytes().all(|b| b == b'-') {
        return None;
    }
    let align = match (left_colon, right_colon) {
        (true, true) => ColumnAlign::Center,
        (false, true) => ColumnAlign::Right,
        _ => ColumnAlign::Left,
    };
    Some(align)
}

/// Parses a GFM pipe table at the start of `slice`.
pub fn parse_pipe_table_prefix(slice: &str) -> Option<(ParsedTable, usize)> {
    let (h_end, l1_start) = next_line_span(slice, 0);
    if l1_start >= slice.len() {
        return None;
    }
    let header_line = trim_cell_ws(&slice[..h_end]);
    if header_line.is_empty() || !header_line.contains('|') {
        return None;
    }

    let (d_end, mut pos) = next_line_span(slice, l1_start);
    let delim_line = trim_cell_ws(&slice[l1_start..d_end]);
    if delim_line.is_empty() || !delim_line.contains('|') {
        return None;
    }
    let delim_cells = split_pipe_cells(delim_line);
    let columns = delim_cells.len();
    if columns == 0 || columns > MAX_TABLE_COLUMNS {
        return None;
    }
    let mut alignments = Vec::with_capacity(columns);
    for cell in &delim_cells {
        alignments.push(parse_delimiter_cell(cell)?);
    }

    let headers = split_pipe_cells(header_line);
    if headers.len() != columns {
        return None;
    }

    // Reject a delimiter-only first line with no real header (e.g. `|---|---|` alone).
    if headers.iter().all(|h| parse_delimiter_cell(h).is_some()) {
        return None;
    }

    let mut rows = Vec::new();
    let mut consumed_end = d_end;

    while pos < slice.len() {
        let (row_end, next_pos) = next_line_span(slice, pos);
        let row_line = trim_cell_ws(&slice[pos..row_end]);
        if row_line.is_empty() || !row_line.contains('|') {
            break;
        }
        let mut cells = split_pipe_cells(row_line);
        cells.truncate(columns);
        while cells.len() < columns {
            cells.push(String::new());
        }
        rows.push(cells);
        consumed_end = row_end;
        pos = next_pos;
    }

    if headers.iter().all(|h| h.is_empty()) && rows.is_empty() {
        return None;
    }

    let raw_markdown = slice[..consumed_end].to_string();
    Some((
        ParsedTable {
            source: TableSource::Pipe,
            columns,
            alignments,
            headers,
            rows,
            raw_markdown,
        },
        consumed_end,
    ))
}

fn box_border_columns(line: &str, left: char, joint: char, right: char) -> usize {
    let trimmed = trim_cell_ws(line);
    let mut chars = trimmed.chars();
    if chars.next() != Some(left) {
        return 0;
    }
    let rest: Vec<char> = chars.collect();
    if rest.len() < 2 || *rest.last().unwrap() != right {
        return 0;
    }
    let inner = &rest[..rest.len() - 1];
    let mut columns = 1usize;
    let mut dashes = 0usize;
    for &ch in inner {
        if ch == '─' {
            dashes += 1;
        } else if ch == joint {
            if dashes == 0 {
                return 0;
            }
            columns += 1;
            dashes = 0;
        } else {
            return 0;
        }
    }
    if dashes == 0 || columns > MAX_TABLE_COLUMNS {
        return 0;
    }
    columns
}

fn split_box_cells(line: &str, expected_columns: usize) -> Option<Vec<String>> {
    let trimmed = trim_cell_ws(line);
    if !trimmed.starts_with('│') || !trimmed.ends_with('│') {
        return None;
    }
    let bar_count = trimmed.chars().filter(|&c| c == '│').count();
    if bar_count != expected_columns + 1 {
        return None;
    }
    let inner = &trimmed['│'.len_utf8()..trimmed.len() - '│'.len_utf8()];
    let cells: Vec<String> = inner
        .split('│')
        .map(|s| trim_cell_ws(s).to_string())
        .collect();
    if cells.len() != expected_columns {
        return None;
    }
    Some(cells)
}

/// Parses a Unicode box-drawing table (`┌─┬─┐` / `│` / `├─┼─┤` / `└─┴─┘`) at the start of `slice`.
pub fn parse_box_table_prefix(slice: &str) -> Option<(ParsedTable, usize)> {
    let (top_end, mut pos) = next_line_span(slice, 0);
    let columns = box_border_columns(&slice[..top_end], '┌', '┬', '┐');
    if columns == 0 || columns > MAX_TABLE_COLUMNS {
        return None;
    }

    enum BoxLine {
        Separator,
        Data(Vec<String>),
    }

    let mut lines: Vec<BoxLine> = Vec::new();
    let mut separators = 0usize;
    let mut data_in_group = 0usize;
    let mut consumed_end = 0usize;
    let mut closed = false;

    while pos < slice.len() {
        let (line_end, next_pos) = next_line_span(slice, pos);
        let line = &slice[pos..line_end];

        if box_border_columns(line, '└', '┴', '┘') == columns {
            if data_in_group == 0 {
                return None;
            }
            consumed_end = line_end;
            closed = true;
            break;
        } else if box_border_columns(line, '├', '┼', '┤') == columns {
            if data_in_group == 0 {
                return None;
            }
            separators += 1;
            data_in_group = 0;
            lines.push(BoxLine::Separator);
        } else if let Some(cells) = split_box_cells(line, columns) {
            data_in_group += 1;
            lines.push(BoxLine::Data(cells));
        } else {
            return None;
        }

        pos = next_pos;
    }

    if !closed || lines.is_empty() {
        return None;
    }

    let mut all_rows: Vec<Vec<String>> = Vec::new();
    let mut group = 0usize;
    let mut new_row = true;

    for item in lines {
        match item {
            BoxLine::Separator => {
                group += 1;
                new_row = true;
            }
            BoxLine::Data(cells) => {
                if new_row || (separators <= 1 && group > 0) || all_rows.is_empty() {
                    all_rows.push(cells);
                } else if let Some(last) = all_rows.last_mut() {
                    for (col, frag) in cells.into_iter().enumerate() {
                        if !frag.is_empty() {
                            if !last[col].is_empty() {
                                last[col].push(' ');
                            }
                            last[col].push_str(&frag);
                        }
                    }
                }
                new_row = false;
            }
        }
    }

    if all_rows.is_empty() {
        return None;
    }

    let headers = all_rows.remove(0);
    let raw_markdown = slice[..consumed_end].to_string();
    Some((
        ParsedTable {
            source: TableSource::Box,
            columns,
            alignments: vec![ColumnAlign::Left; columns],
            headers,
            rows: all_rows,
            raw_markdown,
        },
        consumed_end,
    ))
}

/// Picks the most visible table in `[view_top, view_bottom]`, else the nearest
/// off-screen table, else `None` when `candidates` is empty.
/// Ties break in favor of the later candidate.
#[allow(dead_code)]
pub fn select_table_for_expand(
    view_top: f32,
    view_bottom: f32,
    candidates: &[TableViewportCandidate],
) -> Option<usize> {
    let mut best_index: Option<usize> = None;
    let mut best_shown: f32 = 0.0;
    let mut best_gap: f32 = f32::INFINITY;

    for c in candidates {
        let shown = c.bottom.min(view_bottom) - c.top.max(view_top);
        if shown > 0.0 {
            if shown >= best_shown {
                best_index = Some(c.index);
                best_shown = shown;
            }
            continue;
        }
        if best_shown > 0.0 {
            continue;
        }
        let gap = if c.bottom <= view_top {
            view_top - c.bottom
        } else {
            c.top - view_bottom
        };
        if gap <= best_gap {
            best_index = Some(c.index);
            best_gap = gap;
        }
    }
    best_index
}

/// Scans `messages` from newest to oldest and returns the latest parsed table.
#[allow(dead_code)]
pub fn find_latest_table_in_messages(messages: &[ChatMessage]) -> Option<ParsedTable> {
    for msg in messages.iter().rev() {
        if !msg.blocks.is_empty() {
            for block in msg.blocks.iter().rev() {
                if let MessageBlock::Text { text } = block {
                    let tables = extract_tables_from_markdown(text);
                    if let Some(last) = tables.into_iter().last() {
                        return Some(last.table);
                    }
                }
            }
        }
        let tables = extract_tables_from_markdown(&msg.text);
        if let Some(last) = tables.into_iter().last() {
            return Some(last.table);
        }
    }
    None
}

impl MultibotView {
    pub(crate) fn ensure_table_modal_focus_handle(
        &mut self,
        cx: &mut Context<Self>,
    ) -> FocusHandle {
        if let Some(ref fh) = self.table_modal_state.focus_handle {
            return fh.clone();
        }
        let fh = cx.focus_handle();
        self.table_modal_state.focus_handle = Some(fh.clone());
        fh
    }

    pub(crate) fn focus_table_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let fh = self.ensure_table_modal_focus_handle(cx);
        window.focus(&fh, cx);
    }

    pub(crate) fn open_table_modal(
        &mut self,
        table: ParsedTable,
        title: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        let _ = self.ensure_table_modal_focus_handle(cx);
        self.table_modal_state.open_table(table, title);
        cx.notify();
    }

    pub(crate) fn open_table_from_gpui_data(&mut self, data: &TableData, cx: &mut Context<Self>) {
        if let Some(table) = ParsedTable::from_gpui_table_data(data) {
            let title = format!("Table ({} cols · {} rows)", table.columns, table.rows.len());
            self.open_table_modal(table, title, cx);
        }
    }

    /// Expands the best/latest table in the active chat (`Ctrl+K -> Chat View: Expand table`).
    pub(crate) fn expand_best_table(&mut self, cx: &mut Context<Self>) -> bool {
        if let Some(table) = find_latest_table_in_messages(&self.history.messages) {
            let title = format!("Table ({} cols · {} rows)", table.columns, table.rows.len());
            self.open_table_modal(table, title, cx);
            true
        } else {
            self.notice = Some("No table found in this chat.".into());
            cx.notify();
            false
        }
    }

    /// Renders a compact inline table card with a hover/corner `Expand` button.
    pub(crate) fn render_inline_table_card(
        &self,
        id: &str,
        table: &ParsedTable,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let widths = table.column_widths();
        let table_clone = table.clone();
        let title = format!("Table ({} cols · {} rows)", table.columns, table.rows.len());
        let expand_id = SharedString::from(format!("expand-table-{id}"));
        let scroll_id = SharedString::from(format!("inline-table-scroll-{id}"));

        let mut grid = v_flex().w_full();
        grid = grid.child(render_table_row(
            &table.headers,
            &table.alignments,
            &widths,
            true,
            false,
        ));
        for (r_idx, row) in table.rows.iter().take(INLINE_PREVIEW_MAX_ROWS).enumerate() {
            grid = grid.child(render_table_row(
                row,
                &table.alignments,
                &widths,
                false,
                r_idx % 2 == 1,
            ));
        }
        let hidden_rows = table.rows.len().saturating_sub(INLINE_PREVIEW_MAX_ROWS);

        v_flex()
            .w_full()
            .rounded(RADIUS_ROW)
            .bg(theme::bg_surface())
            .border_1()
            .border_color(theme::border_subtle())
            .overflow_hidden()
            .child(
                h_flex()
                    .w_full()
                    .px_3()
                    .py_1p5()
                    .justify_between()
                    .items_center()
                    .bg(theme::bg_canvas())
                    .border_b_1()
                    .border_color(theme::border_subtle())
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::text_muted())
                            .child(format!("{} × {} table", table.columns, table.rows.len())),
                    )
                    .child(
                        Button::new(expand_id)
                            .xsmall()
                            .label("Expand")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_table_modal(table_clone.clone(), title.clone(), cx);
                                this.focus_table_modal(window, cx);
                            })),
                    ),
            )
            .child(div().id(scroll_id).w_full().overflow_x_scroll().child(grid))
            .when(hidden_rows > 0, |el| {
                el.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .text_color(theme::text_muted())
                        .child(format!(
                            "… {hidden_rows} more rows · Click Expand to view all"
                        )),
                )
            })
            .into_any_element()
    }

    /// Centered obsidian dark expanded Markdown table modal overlay.
    pub(crate) fn render_table_modal(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.table_modal_state.is_open() {
            return None;
        }
        let table = self.table_modal_state.table.as_ref()?;
        let widths = table.column_widths();
        let scroll = self.table_modal_state.scroll;
        let view_w = self.table_modal_state.view_w;
        let view_h = self.table_modal_state.view_h;
        let content_w = self.table_modal_state.content_w;
        let content_h = self.table_modal_state.content_h;
        let can_scroll_x = content_w > view_w;
        let can_scroll_y = content_h > view_h;

        let mut grid = v_flex()
            .w(px(content_w as f32))
            .ml(px(-(scroll.x as f32)))
            .mt(px(-(scroll.y as f32)));
        grid = grid.child(render_table_row(
            &table.headers,
            &table.alignments,
            &widths,
            true,
            false,
        ));
        for (r_idx, row) in table.rows.iter().enumerate() {
            grid = grid.child(render_table_row(
                row,
                &table.alignments,
                &widths,
                false,
                r_idx % 2 == 1,
            ));
        }

        let scroll_badge = if can_scroll_x || can_scroll_y {
            Some(format!(
                "x:{} y:{} · Arrow keys / Shift+Wheel",
                scroll.x, scroll.y
            ))
        } else {
            None
        };

        Some(
            div()
                .id("table-modal-backdrop")
                .when_some(self.table_modal_state.focus_handle.as_ref(), |el, fh| {
                    el.track_focus(fh)
                })
                .absolute()
                .inset_0()
                .bg(hsla(0.0, 0.0, 0.0, 0.65))
                .flex()
                .items_center()
                .justify_center()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        this.table_modal_state.close();
                        this.focus_active_surface_default(window, cx);
                        cx.notify();
                    }),
                )
                .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                    let cmd = this
                        .table_modal_state
                        .handle_key_command(&ev.keystroke.key, DEFAULT_MODAL_LINE_STEP);
                    if this.table_modal_state.apply_command(cmd) {
                        if !this.table_modal_state.is_open() {
                            this.focus_active_surface_default(window, cx);
                        }
                        cx.stop_propagation();
                        cx.notify();
                    }
                }))
                .child(
                    v_flex()
                        .id("table-modal-card")
                        .w(px((view_w + 32) as f32))
                        .max_w(px(1100.0))
                        .max_h(px(680.0))
                        .rounded(RADIUS_MODAL)
                        .bg(theme::bg_surface())
                        .border_1()
                        .border_color(theme::border_subtle())
                        .overflow_hidden()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| {
                            cx.stop_propagation();
                        })
                        .on_scroll(cx.listener(|this, ev: &ScrollEvent, _, cx| {
                            let (dx, dy, step) = match ev.delta {
                                ScrollDelta::Lines(pt) => (pt.x, pt.y, DEFAULT_MODAL_LINE_STEP),
                                ScrollDelta::Pixels(pt) => {
                                    let px_x: f32 = pt.x.into();
                                    let px_y: f32 = pt.y.into();
                                    (px_x, px_y, 1)
                                }
                            };
                            if this
                                .table_modal_state
                                .handle_wheel(dx, dy, ev.modifiers.shift, step)
                            {
                                cx.stop_propagation();
                                cx.notify();
                            }
                        }))
                        .child(
                            h_flex()
                                .w_full()
                                .px_4()
                                .py_3()
                                .justify_between()
                                .items_center()
                                .border_b_1()
                                .border_color(theme::border_subtle())
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .items_center()
                                        .child(
                                            div()
                                                .text_sm()
                                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                                .text_color(theme::text_primary())
                                                .child(self.table_modal_state.title.clone()),
                                        )
                                        .when_some(scroll_badge, |el, badge| {
                                            el.child(
                                                div()
                                                    .px_2()
                                                    .py_0p5()
                                                    .rounded(RADIUS_CHIP)
                                                    .bg(theme::bg_hover())
                                                    .text_xs()
                                                    .text_color(theme::text_muted())
                                                    .child(badge),
                                            )
                                        }),
                                )
                                .child(
                                    div()
                                        .id("close-table-modal")
                                        .tooltip(|window, cx| {
                                            gpui_kit::component::tooltip::Tooltip::new(
                                                crate::icons::shortcut_tooltip(
                                                    "Close",
                                                    "system.escape",
                                                ),
                                            )
                                            .build(window, cx)
                                        })
                                        .role(Role::Button)
                                        .test_support()
                                        .px_2()
                                        .py_1()
                                        .rounded(RADIUS_ROW)
                                        .hover(|s| s.bg(theme::bg_hover()))
                                        .cursor_pointer()
                                        .text_sm()
                                        .text_color(theme::text_secondary())
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(|this, _, window, cx| {
                                                cx.stop_propagation();
                                                this.table_modal_state.close();
                                                this.focus_active_surface_default(window, cx);
                                                cx.notify();
                                            }),
                                        )
                                        .child(lucide_icon(icons::X, 12.0)),
                                ),
                        )
                        .child(
                            div()
                                .id("table-modal-viewport")
                                .w(px(view_w as f32))
                                .h(px(view_h as f32))
                                .mx_4()
                                .my_3()
                                .rounded(RADIUS_ROW)
                                .bg(theme::bg_canvas())
                                .border_1()
                                .border_color(theme::border_subtle())
                                .overflow_hidden()
                                .child(grid),
                        ),
                )
                .into_any_element(),
        )
    }
}

fn render_table_row(
    cells: &[String],
    alignments: &[ColumnAlign],
    widths: &[i32],
    is_header: bool,
    alt_row: bool,
) -> impl IntoElement {
    let mut row_el = h_flex()
        .w_full()
        .min_h(px(ESTIMATED_ROW_HEIGHT as f32))
        .border_b_1()
        .border_color(theme::border_subtle())
        .bg(if is_header {
            theme::bg_hover()
        } else if alt_row {
            theme::bg_sidebar()
        } else {
            theme::bg_canvas()
        });

    for (c_idx, cell) in cells.iter().enumerate() {
        let col_w = widths.get(c_idx).copied().unwrap_or(MIN_COL_WIDTH);
        let align = alignments.get(c_idx).copied().unwrap_or(ColumnAlign::Left);
        let mut cell_el = div()
            .w(px(col_w as f32))
            .flex_shrink_0()
            .px_2p5()
            .py_1p5()
            .border_r_1()
            .border_color(theme::border_subtle())
            .flex()
            .items_center()
            .text_xs()
            .text_color(if is_header {
                theme::text_primary()
            } else {
                theme::text_secondary()
            });
        cell_el = match align {
            ColumnAlign::Left => cell_el.justify_start(),
            ColumnAlign::Center => cell_el.justify_center(),
            ColumnAlign::Right => cell_el.justify_end(),
        };
        row_el = row_el.child(cell_el.child(cell.clone()));
    }

    row_el
}

#[cfg(test)]
mod tests {
    use super::*;
    use multibot_proto::MessageRole;

    #[test]
    fn parses_gfm_pipe_table_with_alignments_escapes_backticks_and_no_outer_pipes() {
        let md = "\
| Name | Code | Score |
|:-----|:----:|------:|
| a\\|b | `x|y` | 100 |
| c | ``p|q`` | 200 |";
        let table = parse_table(md).expect("valid GFM pipe table");
        assert_eq!(table.source, TableSource::Pipe);
        assert_eq!(table.columns, 3);
        assert_eq!(
            table.alignments,
            vec![ColumnAlign::Left, ColumnAlign::Center, ColumnAlign::Right]
        );
        assert_eq!(table.headers, vec!["Name", "Code", "Score"]);
        assert_eq!(table.rows.len(), 2);
        assert_eq!(table.rows[0], vec!["a|b", "`x|y`", "100"]);
        assert_eq!(table.rows[1], vec!["c", "``p|q``", "200"]);

        // No outer pipes
        let bare = "a | b\n---|---\nx | y";
        let bare_table = parse_table(bare).expect("no-outer-pipes GFM table");
        assert_eq!(bare_table.headers, vec!["a", "b"]);
        assert_eq!(bare_table.rows, vec![vec!["x", "y"]]);
    }

    #[test]
    fn ragged_rows_padded_clamped_and_invalid_tables_rejected() {
        let ragged = "\
| H1 | H2 | H3 |
|---|---|---|
| only1 |
| a | b | c | extra |";
        let table = parse_table(ragged).expect("ragged rows normalized");
        assert_eq!(table.rows[0], vec!["only1", "", ""]);
        assert_eq!(table.rows[1], vec!["a", "b", "c"]);

        // < 3 dashes rejected
        assert!(parse_table("| A | B |\n|---|--|\n| 1 | 2 |").is_none());
        // Lone separator without header rejected
        assert!(parse_table("|---|---|\n| 1 | 2 |").is_none());

        // 16 columns accepted, 17 columns rejected
        let hdr16 = (0..16)
            .map(|i| format!("c{i}"))
            .collect::<Vec<_>>()
            .join("|");
        let sep16 = (0..16).map(|_| "---").collect::<Vec<_>>().join("|");
        let t16 = parse_table(&format!("|{hdr16}|\n|{sep16}|")).expect("16 cols ok");
        assert_eq!(t16.columns, 16);

        let hdr17 = (0..17)
            .map(|i| format!("c{i}"))
            .collect::<Vec<_>>()
            .join("|");
        let sep17 = (0..17).map(|_| "---").collect::<Vec<_>>().join("|");
        assert!(parse_table(&format!("|{hdr17}|\n|{sep17}|")).is_none());
    }

    #[test]
    fn parses_unicode_box_drawing_table_and_multiline_cells() {
        let box_md = "\
┌──────┬───────────┐
│ Host │ Spec      │
├──────┼───────────┤
│ vps1 │ Phone, 4x │
│      │ CPU       │
├──────┼───────────┤
│ vps2 │ 8x CPU    │
└──────┴───────────┘";
        let table = parse_table(box_md).expect("box table should parse");
        assert_eq!(table.source, TableSource::Box);
        assert_eq!(table.columns, 2);
        assert_eq!(table.headers, vec!["Host", "Spec"]);
        assert_eq!(table.rows.len(), 2);
        assert_eq!(table.rows[0], vec!["vps1", "Phone, 4x CPU"]);
        assert_eq!(table.rows[1], vec!["vps2", "8x CPU"]);

        // Unseparated body lines after single header separator become distinct rows
        let simple_box = "\
┌───┬───┐
│ K │ V │
├───┼───┤
│ a │ 1 │
│ b │ 2 │
└───┴───┘";
        let simple = parse_table(simple_box).expect("unseparated rows ok");
        assert_eq!(simple.rows, vec![vec!["a", "1"], vec!["b", "2"]]);

        // Malformed box tables rejected
        assert!(parse_table("┌───┬───┐\n├───┼───┤\n└───┴───┘").is_none());
        assert!(parse_table("┌───┬───┐\n│ A │ B │").is_none());
    }

    #[test]
    fn fenced_code_blocks_exclude_tables() {
        let md = "\
Intro text.

```markdown
| Fake | Table |
|---|---|
| 1 | 2 |
```

~~~
┌───┬───┐
│ X │ Y │
├───┼───┤
│ 1 │ 2 │
└───┴───┘
~~~

````markdown
```
| Still | Fenced |
|---|---|
| a | b |
````

| Real | Table |
|---|---|
| ok | yes |
";
        let extracted = extract_tables_from_markdown(md);
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0].table.headers, vec!["Real", "Table"]);
        assert_eq!(extracted[0].table.rows, vec![vec!["ok", "yes"]]);
    }

    #[test]
    fn target_selector_most_visible_nearest_and_latest_fallback() {
        assert_eq!(select_table_for_expand(0.0, 500.0, &[]), None);

        // Most visible wins; ties break to later table.
        let visible = vec![
            TableViewportCandidate {
                index: 0,
                top: 0.0,
                bottom: 100.0,
            },
            TableViewportCandidate {
                index: 1,
                top: 200.0,
                bottom: 400.0,
            },
            TableViewportCandidate {
                index: 2,
                top: 300.0,
                bottom: 500.0,
            },
        ];
        assert_eq!(select_table_for_expand(0.0, 500.0, &visible), Some(2));

        // Nearest off-screen wins; ties break to later table.
        let offscreen = vec![
            TableViewportCandidate {
                index: 0,
                top: 0.0,
                bottom: 100.0,
            },
            TableViewportCandidate {
                index: 1,
                top: 600.0,
                bottom: 700.0,
            },
        ];
        assert_eq!(select_table_for_expand(200.0, 500.0, &offscreen), Some(1));

        // find_latest_table_in_messages picks latest table across messages.
        let msgs = vec![
            ChatMessage {
                id: "m1".into(),
                role: MessageRole::Assistant,
                text: "| First | Val |\n|---|---|\n| 1 | 2 |".into(),
                ..Default::default()
            },
            ChatMessage {
                id: "m2".into(),
                role: MessageRole::Assistant,
                text: "| Second | Val |\n|---|---|\n| 3 | 4 |".into(),
                ..Default::default()
            },
        ];
        let latest = find_latest_table_in_messages(&msgs).unwrap();
        assert_eq!(latest.headers, vec!["Second", "Val"]);
    }

    #[test]
    fn modal_state_scroll_clamping_keyboard_and_wheel() {
        let headers = (0..12)
            .map(|i| format!("Wide_Column_{i}"))
            .collect::<Vec<_>>();
        let rows = (0..40)
            .map(|r| {
                (0..12)
                    .map(|c| format!("table-cell-value-{r}-{c}"))
                    .collect()
            })
            .collect();
        let table = ParsedTable {
            source: TableSource::Pipe,
            columns: 12,
            alignments: vec![ColumnAlign::Left; 12],
            headers,
            rows,
            raw_markdown: String::new(),
        };

        let mut state = TableModalState::default();
        assert!(!state.is_open());
        state.open_table(table, "Wide Table");
        assert!(state.is_open());
        assert_eq!(state.scroll, TableScroll { x: 0, y: 0 });

        // Vertical scroll down, page down, end, home
        let cmd = state.handle_key_command("down", DEFAULT_MODAL_LINE_STEP);
        assert!(state.apply_command(cmd));
        assert_eq!(state.scroll.y, DEFAULT_MODAL_LINE_STEP);

        let cmd_end = state.handle_key_command("end", DEFAULT_MODAL_LINE_STEP);
        assert!(state.apply_command(cmd_end));
        assert_eq!(state.scroll.y, (state.content_h - state.view_h).max(0));

        let cmd_home = state.handle_key_command("home", DEFAULT_MODAL_LINE_STEP);
        assert!(state.apply_command(cmd_home));
        assert_eq!(state.scroll.y, 0);

        // Horizontal scroll right / left + Shift+Wheel
        let cmd_right = state.handle_key_command("right", DEFAULT_MODAL_LINE_STEP);
        assert!(state.apply_command(cmd_right));
        assert!(state.scroll.x > 0);

        assert!(state.handle_wheel(0.0, -2.0, true, DEFAULT_MODAL_LINE_STEP));
        let after_shift_wheel = state.scroll.x;
        assert!(after_shift_wheel > DEFAULT_MODAL_LINE_STEP);

        // Escape closes cleanly
        let cmd_esc = state.handle_key_command("escape", DEFAULT_MODAL_LINE_STEP);
        assert_eq!(cmd_esc, TableModalCommand::Close);
        assert!(state.apply_command(cmd_esc));
        assert!(!state.is_open());
    }

    #[test]
    fn unclosed_backtick_does_not_swallow_pipe_table_columns() {
        // Body row unclosed backtick
        let md_body = "| Key | Value |\n|---|---|\n| it's `raw | 42 |";
        let t_body = parse_table(md_body).expect("body row unclosed backtick must parse");
        assert_eq!(t_body.headers, vec!["Key", "Value"]);
        assert_eq!(t_body.rows, vec![vec!["it's `raw", "42"]]);

        // Header row unclosed backtick
        let md_header = "| `Key | Value |\n|---|---|\n| a | b |";
        let t_header = parse_table(md_header).expect("header row unclosed backtick must parse");
        assert_eq!(t_header.headers, vec!["`Key", "Value"]);
        assert_eq!(t_header.rows, vec![vec!["a", "b"]]);

        // Closed code spans containing pipes still stay intact
        let md_closed = "| Expr | Out |\n|---|---|\n| ``a`b|c`` | ok |";
        let t_closed = parse_table(md_closed).expect("closed multi-backtick span must parse");
        assert_eq!(t_closed.rows, vec![vec!["``a`b|c``", "ok"]]);
    }

    #[test]
    fn rejects_malformed_empty_pipe_table() {
        assert!(parse_table("|\n|---|").is_none());
        assert!(parse_table("| |\n|---|").is_none());
        assert!(parse_table("| | |\n|---|---|").is_none());
        // Empty header WITH real data rows remains valid
        let with_rows = parse_table("| | |\n|---|---|\n| a | b |").expect("rows present");
        assert_eq!(with_rows.rows, vec![vec!["a", "b"]]);
    }

    #[test]
    fn cross_column_unclosed_backticks_do_not_swallow_columns_while_preserving_closed_spans() {
        // 1. 2-col header with unclosed backticks in both columns
        let t_hdr = parse_table("| `a | `b |\n|---|---|\n| 1 | 2 |")
            .expect("2-col header with cross-column unclosed backticks must parse");
        assert_eq!(t_hdr.headers, vec!["`a", "`b"]);
        assert_eq!(t_hdr.rows, vec![vec!["1", "2"]]);

        // 2. 2-col body row with unclosed backticks in both columns
        let t_body2 = parse_table("| Key | Value |\n|---|---|\n| `a | `b |")
            .expect("2-col body with cross-column unclosed backticks must parse");
        assert_eq!(t_body2.headers, vec!["Key", "Value"]);
        assert_eq!(t_body2.rows, vec![vec!["`a", "`b"]]);

        // 3. 3-col body row with unclosed backticks in col 1 and col 3
        let t_body3 = parse_table("| C1 | C2 | C3 |\n|---|---|---|\n| `a | middle | `c |")
            .expect("3-col body with cross-column unclosed backticks must parse");
        assert_eq!(t_body3.headers, vec!["C1", "C2", "C3"]);
        assert_eq!(t_body3.rows, vec![vec!["`a", "middle", "`c"]]);

        // 4. Closed single-backtick code span containing `|` preserved
        let t_closed1 = parse_table("| Key | Value |\n|---|---|\n| `a|b` | ok |")
            .expect("closed single-backtick code span with pipe must stay intact");
        assert_eq!(t_closed1.rows, vec![vec!["`a|b`", "ok"]]);

        // 5. Closed multi-backtick code span containing `|` preserved
        let t_closed2 = parse_table("| Key | Value |\n|---|---|\n| ``a`b|c`` | ok |")
            .expect("closed multi-backtick code span with pipe must stay intact");
        assert_eq!(t_closed2.rows, vec![vec!["``a`b|c``", "ok"]]);

        // 6. Single unclosed backtick preserved
        let t_single = parse_table("| Key | Value |\n|---|---|\n| it's `raw | 42 |")
            .expect("single unclosed backtick must parse");
        assert_eq!(t_single.rows, vec![vec!["it's `raw", "42"]]);
    }

    #[test]
    fn pipe_table_backtick_cell_start_detection_all_cases() {
        // 1. Ragged 2-col row with valid closed code span containing `|`
        let t1 = parse_table("| C1 | C2 |\n|---|---|\n| `a|b` |")
            .expect("case 1: ragged closed code span");
        assert_eq!(t1.rows, vec![vec!["`a|b`", ""]]);

        // 2. Ragged 3-col row with cross-column unclosed backticks
        let t2 = parse_table("| C1 | C2 | C3 |\n|---|---|---|\n| `a | `b |")
            .expect("case 2: ragged cross-column unclosed backticks");
        assert_eq!(t2.rows, vec![vec!["`a", "`b", ""]]);

        // 3. Mixed closed code span with `|` + cross-column unclosed backticks
        let t3 = parse_table("| C1 | C2 | C3 |\n|---|---|---|\n| `x|y` | `a | `b |")
            .expect("case 3: closed then cross-column unclosed");
        assert_eq!(t3.rows, vec![vec!["`x|y`", "`a", "`b"]]);

        // 4. Cross-column unclosed backticks BEFORE closed code span with `|`
        let t4 = parse_table("| C1 | C2 | C3 | C4 |\n|---|---|---|---|\n| `a | `b | `x|y` | ok |")
            .expect("case 4: unclosed before closed");
        assert_eq!(t4.rows, vec![vec!["`a", "`b", "`x|y`", "ok"]]);

        // 5. Header row with cross-column unclosed backticks
        let t5 = parse_table("| `a | `b |\n|---|---|\n| 1 | 2 |")
            .expect("case 5: header cross-column unclosed");
        assert_eq!(t5.headers, vec!["`a", "`b"]);
        assert_eq!(t5.rows, vec![vec!["1", "2"]]);

        // 6. 2-col body with cross-column unclosed backticks
        let t6 = parse_table("| Key | Value |\n|---|---|\n| `a | `b |")
            .expect("case 6: 2-col body unclosed");
        assert_eq!(t6.rows, vec![vec!["`a", "`b"]]);

        // 7. 3-col body with unclosed backticks in col 1 and col 3
        let t7 = parse_table("| C1 | C2 | C3 |\n|---|---|---|\n| `a | middle | `c |")
            .expect("case 7: 3-col non-adjacent unclosed");
        assert_eq!(t7.rows, vec![vec!["`a", "middle", "`c"]]);

        // 8. Closed single-backtick code span containing `|`
        let t8 = parse_table("| Key | Value |\n|---|---|\n| `a|b` | ok |")
            .expect("case 8: closed single-backtick span");
        assert_eq!(t8.rows, vec![vec!["`a|b`", "ok"]]);

        // 9. Closed double-backtick code span containing single backtick and `|`
        let t9 = parse_table("| Key | Value |\n|---|---|\n| ``a`b|c`` | ok |")
            .expect("case 9: closed double-backtick span");
        assert_eq!(t9.rows, vec![vec!["``a`b|c``", "ok"]]);

        // 10. Closed code span with spaces around `|`
        let t10 = parse_table("| Key | Value |\n|---|---|\n| `foo | bar` | ok |")
            .expect("case 10: spaced pipe inside closed code span");
        assert_eq!(t10.rows, vec![vec!["`foo | bar`", "ok"]]);

        // 11. Escaped pipes outside code spans
        let t11 = parse_table("| Key | Value |\n|---|---|\n| a\\|b | c\\|d |")
            .expect("case 11: escaped pipes");
        assert_eq!(t11.rows, vec![vec!["a|b", "c|d"]]);

        // 12. No-outer-pipes cross-column unclosed backticks
        let t12 = parse_table("`a | `b\n---|---\n1 | 2")
            .expect("case 12: no outer pipes cross-column unclosed");
        assert_eq!(t12.headers, vec!["`a", "`b"]);
        assert_eq!(t12.rows, vec![vec!["1", "2"]]);

        // Bonus edge cases: code spans containing/ending with `|` and unclosed followed by closed
        let t13 =
            parse_table("| Key | Value |\n|---|---|\n| `|` | ok |\n| `foo|` | ok |\n| `a | `b` |")
                .expect("bonus edge cases");
        assert_eq!(
            t13.rows,
            vec![vec!["`|`", "ok"], vec!["`foo|`", "ok"], vec!["`a", "`b`"],]
        );
    }

    #[test]
    fn pipe_table_code_spans_ending_with_pipe_followed_by_prose_or_punctuation() {
        // 1. Single `|` code span followed by prose
        let t1 = parse_table("| Op | Desc |\n|---|---|\n| `|` operator | bitwise OR |")
            .expect("single pipe code span followed by prose");
        assert_eq!(t1.rows, vec![vec!["`|` operator", "bitwise OR"]]);

        // 2. Double `||` code span followed by prose
        let t2 = parse_table("| Op | Desc |\n|---|---|\n| `||` operator | logical OR |")
            .expect("double pipe code span followed by prose");
        assert_eq!(t2.rows, vec![vec!["`||` operator", "logical OR"]]);

        // 3. Multi-char code span ending with `|` followed by prose
        let t3 = parse_table("| Op | Desc |\n|---|---|\n| `foo|` note | desc |")
            .expect("code span ending in pipe followed by prose");
        assert_eq!(t3.rows, vec![vec!["`foo|` note", "desc"]]);

        // 4. Parenthesized `|` code span
        let t4 = parse_table("| Op | Desc |\n|---|---|\n| (`|`) | bitwise OR |")
            .expect("parenthesized pipe code span");
        assert_eq!(t4.rows, vec![vec!["(`|`)", "bitwise OR"]]);

        // 5. Comma-separated code spans starting with `|`
        let t5 = parse_table("| Op | Desc |\n|---|---|\n| `|`, `&&` | shell ops |")
            .expect("comma-separated pipe code span");
        assert_eq!(t5.rows, vec![vec!["`|`, `&&`", "shell ops"]]);

        // 6. Double-backtick `|` code span followed by prose
        let t6 = parse_table("| Op | Desc |\n|---|---|\n| ``|`` operator | bitwise OR |")
            .expect("double-backtick pipe code span followed by prose");
        assert_eq!(t6.rows, vec![vec!["``|`` operator", "bitwise OR"]]);

        // 7. Header containing `|` code span followed by prose
        let t7 = parse_table("| `|` op | Desc |\n|---|---|\n| 1 | 2 |")
            .expect("header with pipe code span followed by prose");
        assert_eq!(t7.headers, vec!["`|` op", "Desc"]);
        assert_eq!(t7.rows, vec![vec!["1", "2"]]);

        // 8. Bold-wrapped `|` code span
        let t8 = parse_table("| Op | Desc |\n|---|---|\n| **`|`** | bold bitwise OR |")
            .expect("bold-wrapped pipe code span");
        assert_eq!(t8.rows, vec![vec!["**`|`**", "bold bitwise OR"]]);

        // 9. Slash-joined `|` and `&` code spans
        let t9 = parse_table("| Op | Desc |\n|---|---|\n| `|`/`&` | bitwise ops |")
            .expect("slash-joined pipe code spans");
        assert_eq!(t9.rows, vec![vec!["`|`/`&`", "bitwise ops"]]);

        // 10. Hyphen-joined `|` code span
        let t10 = parse_table("| Op | Desc |\n|---|---|\n| `|`-delimited | csv alt |")
            .expect("hyphen-joined pipe code span");
        assert_eq!(t10.rows, vec![vec!["`|`-delimited", "csv alt"]]);

        // 11. Quoted `|` code span
        let t11 = parse_table("| Op | Desc |\n|---|---|\n| \"`|`\" | quoted OR |")
            .expect("quoted pipe code span");
        assert_eq!(t11.rows, vec![vec!["\"`|`\"", "quoted OR"]]);

        // 12. Non-ASCII em-dash-followed `|` code span
        let t12 = parse_table("| Op | Desc |\n|---|---|\n| `|`—or | unicode |")
            .expect("em-dash-followed pipe code span");
        assert_eq!(t12.rows, vec![vec!["`|`—or", "unicode"]]);
    }
}
