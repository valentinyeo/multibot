use crate::app::MultibotView;
use crate::theme::{self, RADIUS_CHIP, RADIUS_MODAL, RADIUS_ROW};
use gpui_kit::{
    AnyElement, Context, InteractiveElement, IntoElement, MouseButton, ParentElement, Role,
    SharedString, StatefulInteractiveElement, Styled, TestSupportExt, Window,
    component::{h_flex, v_flex},
    div,
    prelude::FluentBuilder,
    px,
};
use multibot_proto::{AgentKind, SlashCommandItem};
use std::collections::HashSet;

const VISIBLE_ROWS: usize = 8;
const MAX_MRU: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerMode {
    /// `/` command trigger (any agent; start-of-composer for Codex, start or post-whitespace for Claude/Antigravity).
    Slash,
    /// `$` skill trigger (Codex skills ONLY).
    Dollar,
    /// Full command browser opened via `Ctrl+/`.
    Browse,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashTrigger {
    pub mode: TriggerMode,
    pub start_byte: usize,
    pub end_byte: usize,
    pub query: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashKeyOutcome {
    Ignored,
    Moved,
    Closed,
    Accepted(SlashCommandItem, Option<SlashTrigger>),
}

#[derive(Debug, Clone)]
pub(crate) struct SlashPickerState {
    pub open: bool,
    pub browse_mode: bool,
    pub browse_anchor_byte: Option<usize>,
    pub agent: AgentKind,
    pub trigger: Option<SlashTrigger>,
    pub daemon_commands: Vec<SlashCommandItem>,
    pub daemon_agent: Option<AgentKind>,
    pub filtered: Vec<SlashCommandItem>,
    pub selected: usize,
    pub scroll_top: usize,
    pub mru: Vec<String>,
}

impl Default for SlashPickerState {
    fn default() -> Self {
        Self {
            open: false,
            browse_mode: false,
            browse_anchor_byte: None,
            agent: AgentKind::Claude,
            trigger: None,
            daemon_commands: Vec::new(),
            daemon_agent: None,
            filtered: Vec::new(),
            selected: 0,
            scroll_top: 0,
            mru: Vec::new(),
        }
    }
}

impl SlashPickerState {
    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    pub(crate) fn close(&mut self) {
        self.open = false;
        self.browse_mode = false;
        self.browse_anchor_byte = None;
        self.trigger = None;
        self.selected = 0;
        self.scroll_top = 0;
    }

    /// Updates cached daemon commands from `ServerEvent::Commands` (wired in M6).
    #[allow(dead_code)]
    pub(crate) fn set_daemon_commands(&mut self, agent: AgentKind, items: Vec<SlashCommandItem>) {
        self.daemon_agent = Some(agent);
        self.daemon_commands = items;
        if self.open && self.agent == agent {
            self.refresh_ranked();
        }
    }

    pub(crate) fn sync_from_composer(&mut self, text: &str, caret_byte: usize, agent: AgentKind) {
        self.agent = agent;
        if self.browse_mode {
            let clamped = clamp_char_boundary(text, caret_byte);
            let anchor = self.browse_anchor_byte.unwrap_or(0);
            if clamped < anchor || !text.is_char_boundary(anchor) {
                self.close();
                return;
            }
            let query = text[anchor..clamped]
                .trim()
                .trim_start_matches(['/', '$'])
                .to_string();
            self.trigger = Some(SlashTrigger {
                mode: TriggerMode::Browse,
                start_byte: anchor,
                end_byte: clamped,
                query,
            });
            self.refresh_ranked();
            return;
        }

        if let Some(trigger) = detect_trigger(text, caret_byte, agent) {
            self.open = true;
            self.trigger = Some(trigger);
            self.refresh_ranked();
        } else {
            self.close();
        }
    }

    /// Opens the full command browser (`Ctrl+/`).
    pub(crate) fn open_browser(
        &mut self,
        agent: AgentKind,
        current_text: &str,
        caret_byte: usize,
    ) {
        let clamped = clamp_char_boundary(current_text, caret_byte);
        self.open = true;
        self.browse_mode = true;
        self.browse_anchor_byte = Some(clamped);
        self.agent = agent;
        self.trigger = Some(SlashTrigger {
            mode: TriggerMode::Browse,
            start_byte: clamped,
            end_byte: clamped,
            query: String::new(),
        });
        self.refresh_ranked();
    }

    /// Updates the filter query while in `Ctrl+/` browse mode (wired in M6).
    #[allow(dead_code)]
    pub(crate) fn set_browse_query(&mut self, query: &str) {
        if let Some(ref mut trigger) = self.trigger {
            trigger.query = query.to_string();
            self.refresh_ranked();
        }
    }

    pub(crate) fn effective_catalog(
        &self,
        agent: AgentKind,
        mode: TriggerMode,
    ) -> Vec<SlashCommandItem> {
        let mut seen: HashSet<(String, String)> = HashSet::new();
        let mut combined = Vec::new();

        if self.daemon_agent == Some(agent) {
            for item in &self.daemon_commands {
                let trigger = if item.trigger.is_empty() {
                    "/".to_string()
                } else {
                    item.trigger.clone()
                };
                let key = (trigger.clone(), item.name.to_ascii_lowercase());
                if seen.insert(key) {
                    let mut cloned = item.clone();
                    cloned.trigger = trigger;
                    combined.push(cloned);
                }
            }
        }

        for item in builtin_commands_for(agent) {
            let key = (item.trigger.clone(), item.name.to_ascii_lowercase());
            if seen.insert(key) {
                combined.push(item);
            }
        }

        combined
            .into_iter()
            .filter(|item| match mode {
                TriggerMode::Dollar => item.trigger == "$",
                TriggerMode::Slash => {
                    item.trigger == "/"
                        || (agent == AgentKind::Codex && item.trigger == "$")
                }
                TriggerMode::Browse => true,
            })
            .collect()
    }

    fn refresh_ranked(&mut self) {
        let prev_key = self
            .filtered
            .get(self.selected)
            .map(|item| (item.trigger.clone(), item.name.to_ascii_lowercase()));

        let (mode, query) = match &self.trigger {
            Some(t) => (t.mode, t.query.as_str()),
            None => (TriggerMode::Browse, ""),
        };

        let catalog = self.effective_catalog(self.agent, mode);
        let mut scored: Vec<(u16, usize, String, SlashCommandItem)> = catalog
            .into_iter()
            .filter_map(|item| {
                let score = score_command(&item.name, &item.description, query)?;
                let mru_rank = self
                    .mru
                    .iter()
                    .position(|m| m.eq_ignore_ascii_case(&item.name))
                    .unwrap_or(usize::MAX);
                let name_l = item.name.to_ascii_lowercase();
                Some((score, mru_rank, name_l, item))
            })
            .collect();

        scored.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| a.2.cmp(&b.2))
        });

        self.filtered = scored.into_iter().map(|(_, _, _, item)| item).collect();

        if self.filtered.is_empty() {
            self.selected = 0;
            self.scroll_top = 0;
            return;
        }

        if let Some((prev_trigger, prev_name)) = prev_key
            && let Some(idx) = self.filtered.iter().position(|item| {
                item.trigger == prev_trigger && item.name.to_ascii_lowercase() == prev_name
            })
        {
            self.selected = idx;
        } else {
            self.selected = 0;
        }
        self.keep_selection_in_view();
    }

    fn keep_selection_in_view(&mut self) {
        let max_scroll_top = self.filtered.len().saturating_sub(VISIBLE_ROWS);
        self.scroll_top = self.scroll_top.min(max_scroll_top);
        if self.selected < self.scroll_top {
            self.scroll_top = self.selected;
        } else if self.selected >= self.scroll_top + VISIBLE_ROWS {
            self.scroll_top = self.selected + 1 - VISIBLE_ROWS;
        }
        self.scroll_top = self.scroll_top.min(max_scroll_top);
    }

    fn promote_mru(&mut self, name: &str) {
        self.mru.retain(|existing| !existing.eq_ignore_ascii_case(name));
        self.mru.insert(0, name.to_string());
        self.mru.truncate(MAX_MRU);
    }

    pub(crate) fn handle_key(&mut self, key: &str) -> SlashKeyOutcome {
        if !self.is_open() {
            return SlashKeyOutcome::Ignored;
        }
        match key.to_ascii_lowercase().as_str() {
            "escape" => {
                self.close();
                SlashKeyOutcome::Closed
            }
            "up" => {
                let len = self.filtered.len();
                if len == 0 {
                    return SlashKeyOutcome::Ignored;
                }
                self.selected = (self.selected + len - 1) % len;
                self.keep_selection_in_view();
                SlashKeyOutcome::Moved
            }
            "down" => {
                let len = self.filtered.len();
                if len == 0 {
                    return SlashKeyOutcome::Ignored;
                }
                self.selected = (self.selected + 1) % len;
                self.keep_selection_in_view();
                SlashKeyOutcome::Moved
            }
            "pageup" => {
                let len = self.filtered.len();
                if len == 0 {
                    return SlashKeyOutcome::Ignored;
                }
                let step = VISIBLE_ROWS.min(len);
                self.selected = (self.selected + len - step) % len;
                self.keep_selection_in_view();
                SlashKeyOutcome::Moved
            }
            "pagedown" => {
                let len = self.filtered.len();
                if len == 0 {
                    return SlashKeyOutcome::Ignored;
                }
                let step = VISIBLE_ROWS.min(len);
                self.selected = (self.selected + step) % len;
                self.keep_selection_in_view();
                SlashKeyOutcome::Moved
            }
            "tab" | "enter" => {
                let Some(item) = self.filtered.get(self.selected).cloned() else {
                    return SlashKeyOutcome::Ignored;
                };
                let trigger = self.trigger.clone();
                self.promote_mru(&item.name);
                self.close();
                SlashKeyOutcome::Accepted(item, trigger)
            }
            _ => SlashKeyOutcome::Ignored,
        }
    }
}

fn clamp_char_boundary(s: &str, mut byte_idx: usize) -> usize {
    byte_idx = byte_idx.min(s.len());
    while byte_idx > 0 && !s.is_char_boundary(byte_idx) {
        byte_idx -= 1;
    }
    byte_idx
}

/// Detects an active `/` (any agent) or `$` (Codex ONLY) command trigger at `caret_byte`.
pub fn detect_trigger(text: &str, caret_byte: usize, agent: AgentKind) -> Option<SlashTrigger> {
    let caret = clamp_char_boundary(text, caret_byte);
    let prefix = &text[..caret];
    let mut start = caret;
    for (idx, ch) in prefix.char_indices().rev() {
        if matches!(ch, ' ' | '\t' | '\r' | '\n') {
            break;
        }
        start = idx;
    }
    if start == caret {
        return None;
    }

    let token = &text[start..caret];
    let first = token.chars().next()?;
    match first {
        '/' => {
            if agent == AgentKind::Codex && start != 0 {
                return None;
            }
            Some(SlashTrigger {
                mode: TriggerMode::Slash,
                start_byte: start,
                end_byte: caret,
                query: token[1..].to_string(),
            })
        }
        '$' => {
            if agent != AgentKind::Codex {
                return None;
            }
            Some(SlashTrigger {
                mode: TriggerMode::Dollar,
                start_byte: start,
                end_byte: caret,
                query: token[1..].to_string(),
            })
        }
        _ => None,
    }
}

/// 7-tier fuzzy/prefix command scorer matching legacy `chat_complete.zig`.
/// Lower score is better (`0` = exact match, `6` = empty query).
pub fn score_command(name: &str, description: &str, query: &str) -> Option<u16> {
    if query.is_empty() {
        return Some(6);
    }
    if name.eq_ignore_ascii_case(query) {
        return Some(0);
    }
    let name_l = name.to_ascii_lowercase();
    let query_l = query.to_ascii_lowercase();
    if name_l.starts_with(&query_l) {
        return Some(1);
    }
    if starts_with_one_swap(&name_l, &query_l)
        || prefix_at_word_boundary(&name_l, &query_l, ":-_/")
    {
        return Some(2);
    }
    if name_l.contains(&query_l) {
        return Some(3);
    }
    let desc_l = description.to_ascii_lowercase();
    if desc_l.starts_with(&query_l)
        || prefix_at_word_boundary(&desc_l, &query_l, " \t.,:;()")
    {
        return Some(4);
    }
    let mut q_chars = query_l.chars().peekable();
    for ch in name_l.chars() {
        if q_chars.peek() == Some(&ch) {
            q_chars.next();
        }
    }
    if q_chars.peek().is_none() {
        Some(5)
    } else {
        None
    }
}

fn starts_with_one_swap(name_l: &str, query_l: &str) -> bool {
    let nb = name_l.as_bytes();
    let qb = query_l.as_bytes();
    if qb.len() < 3 || nb.len() < qb.len() {
        return false;
    }
    for swap_idx in 0..qb.len() - 1 {
        let mut matched = true;
        for i in 0..qb.len() {
            let expected = if i == swap_idx {
                qb[swap_idx + 1]
            } else if i == swap_idx + 1 {
                qb[swap_idx]
            } else {
                qb[i]
            };
            if nb[i] != expected {
                matched = false;
                break;
            }
        }
        if matched {
            return true;
        }
    }
    false
}

fn prefix_at_word_boundary(haystack_l: &str, needle_l: &str, separators: &str) -> bool {
    let mut prev_is_sep = false;
    for (idx, ch) in haystack_l.char_indices() {
        if prev_is_sep && haystack_l[idx..].starts_with(needle_l) {
            return true;
        }
        prev_is_sep = separators.contains(ch);
    }
    false
}

/// Inserts a selected slash/skill command into `current` without sending,
/// preserving trailing arguments cleanly without creating double spaces.
pub fn insert_slash_command(
    current: &str,
    trigger: &SlashTrigger,
    item: &SlashCommandItem,
) -> (String, usize) {
    let start = clamp_char_boundary(current, trigger.start_byte);
    let end = clamp_char_boundary(current, trigger.end_byte).max(start);

    let prefix = if !item.trigger.is_empty() {
        item.trigger.as_str()
    } else if trigger.mode == TriggerMode::Dollar {
        "$"
    } else {
        "/"
    };

    let before = &current[..start];
    let after = &current[end..];
    let needs_leading_space = trigger.mode == TriggerMode::Browse
        && !before.is_empty()
        && !before.ends_with([' ', '\t', '\r', '\n']);

    let after_trimmed = after.strip_prefix(' ').unwrap_or(after);

    let mut out = String::with_capacity(current.len() + item.name.len() + 4);
    out.push_str(before);
    if needs_leading_space {
        out.push(' ');
    }
    out.push_str(prefix);
    out.push_str(&item.name);
    out.push(' ');
    let caret = out.len();
    out.push_str(after_trimmed);
    (out, caret)
}

fn make_builtin(name: &str, description: &str, opens_in_terminal: bool) -> SlashCommandItem {
    SlashCommandItem {
        name: name.to_string(),
        description: description.to_string(),
        trigger: "/".to_string(),
        source: "builtin".to_string(),
        opens_in_terminal,
    }
}

/// Offline built-in command catalog per agent (used when `multibotd` commands
/// have not arrived yet or as a fallback merged underneath daemon items).
pub fn builtin_commands_for(agent: AgentKind) -> Vec<SlashCommandItem> {
    match agent {
        AgentKind::Claude => vec![
            make_builtin("rename", "Rename this chat session", false),
            make_builtin("clear", "Clear conversation and start fresh", false),
            make_builtin("plan", "Switch to or toggle Plan mode", false),
            make_builtin("compact", "Summarize conversation to free context", false),
            make_builtin("model", "Choose model", false),
            make_builtin("permissions", "Manage tool permission rules", false),
            make_builtin("login", "Sign in with Anthropic account", false),
            make_builtin("context", "Inspect context window usage", false),
            make_builtin("cost", "Show token and cost usage", false),
            make_builtin("review", "Review current changes", false),
            make_builtin("init", "Initialize CLAUDE.md project instructions", false),
            make_builtin("doctor", "Check Claude Code environment health", false),
            make_builtin("mcp", "Manage MCP servers", false),
            make_builtin("add-dir", "Add a working directory to the session", true),
            make_builtin("branch", "Inspect or switch git branches", true),
            make_builtin("btw", "Ask a quick side question", true),
            make_builtin("bug", "Report a bug", true),
            make_builtin("cd", "Change working directory", true),
            make_builtin("copy", "Copy latest output", true),
            make_builtin("diff", "Show current diff", true),
            make_builtin("exit", "Exit the CLI session", true),
            make_builtin("export", "Export conversation transcript", true),
            make_builtin("feedback", "Send feedback", true),
            make_builtin("fork", "Fork current conversation", true),
            make_builtin("help", "Show help", true),
            make_builtin("hooks", "Manage hooks", true),
            make_builtin("ide", "Connect IDE integration", true),
            make_builtin("keybindings", "Inspect CLI keybindings", true),
            make_builtin("logout", "Sign out", true),
            make_builtin("memory", "Edit session memory", true),
            make_builtin("plugin", "Manage plugins", true),
            make_builtin("resume", "Resume a prior session", true),
            make_builtin("rewind", "Rewind conversation turns", true),
            make_builtin("sandbox", "Configure sandbox settings", true),
            make_builtin("skills", "List available skills", true),
            make_builtin("status", "Show session status", true),
            make_builtin("tasks", "View background tasks", true),
            make_builtin("theme", "Change CLI theme", true),
            make_builtin("upgrade", "Upgrade Claude Code", true),
            make_builtin("voice", "Toggle voice mode", true),
            make_builtin("workflows", "Browse workflows", true),
        ],
        AgentKind::Codex => vec![
            make_builtin("rename", "Rename this chat session", false),
            make_builtin("clear", "Clear conversation and start fresh", false),
            make_builtin("plan", "Switch to or toggle Plan mode", false),
            make_builtin("compact", "Summarize conversation to free context", false),
            make_builtin("fast", "Toggle 1.5x fast mode", false),
            make_builtin("review", "Review current changes and find issues", false),
            make_builtin("diff", "Show git diff including untracked files", false),
            make_builtin("init", "Create AGENTS.md", false),
            make_builtin("recap", "Summarize current conversation", false),
            make_builtin("status", "Show session config and token usage", false),
            make_builtin("copy", "Copy latest output", false),
            make_builtin("export", "Export session transcript", false),
            make_builtin("cd", "Change working directory", false),
            make_builtin("pwd", "Print working directory", false),
            make_builtin("ps", "Show running processes", false),
            make_builtin("stop", "Stop active turn", false),
            make_builtin("exit", "Exit Codex session", false),
            make_builtin("model", "Choose Codex model", true),
            make_builtin("permissions", "Manage approval permissions", true),
            make_builtin("skills", "Browse Codex skills", true),
            make_builtin("mcp", "Manage MCP servers", true),
            make_builtin("memories", "Manage Codex memories", true),
            make_builtin("hooks", "Manage hooks", true),
            make_builtin("ide", "Connect IDE", true),
            make_builtin("new", "Start a new session", true),
            make_builtin("resume", "Resume a prior session", true),
            make_builtin("fork", "Fork current session", true),
            make_builtin("archive", "Archive session", true),
            make_builtin("delete", "Delete session", true),
            make_builtin("goal", "Run toward a goal", true),
            make_builtin("agents", "Inspect active agents", true),
            make_builtin("side", "Start side conversation", true),
            make_builtin("usage", "Show usage summary", true),
            make_builtin("personality", "Configure assistant style", true),
            make_builtin("subagents", "Inspect subagents", true),
        ],
        AgentKind::Antigravity => vec![
            make_builtin("rename", "Rename this chat session", false),
            make_builtin("clear", "Clear conversation and start fresh", false),
            make_builtin("plan", "Plan complex work step by step before executing", false),
            make_builtin("compact", "Summarize conversation to free context", false),
            make_builtin("goal", "Run thoroughly until the goal is fully achieved", false),
            make_builtin("schedule", "Run an instruction on a schedule or timer", false),
            make_builtin("browser", "Browse the web or interact with web apps", false),
            make_builtin("grill-me", "Align on a plan through an interactive interview", false),
            make_builtin("teamwork", "Coordinate a team of agents on a large project", false),
            make_builtin("learn", "Save a workflow or correction for future tasks", false),
            make_builtin("boost", "Deep thinking, planning, and rigorous verification", false),
        ],
    }
}

impl MultibotView {
    /// Opens the full command browser (`Ctrl+/`) anchored above the composer.
    pub(crate) fn open_slash_browser(&mut self, cx: &mut Context<Self>) {
        let agent = self
            .selected_session
            .as_ref()
            .map(|s| s.agent)
            .unwrap_or(AgentKind::Claude);
        let current = self.prompt_input.read(cx).value().to_string();
        let caret = current.len();
        self.slash_picker_state.open_browser(agent, &current, caret);
        cx.notify();
    }

    /// Inserts a picked slash/skill command into the composer WITHOUT sending.
    pub(crate) fn accept_slash_command(
        &mut self,
        item: SlashCommandItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let trigger = self.slash_picker_state.trigger.clone();
        self.accept_slash_command_with_trigger(item, trigger, window, cx);
    }

    /// Inserts a picked slash/skill command using a pre-captured trigger span (preserved
    /// before `SlashPickerState::close()` clears `self.trigger` on keyboard `Tab`/`Enter`).
    pub(crate) fn accept_slash_command_with_trigger(
        &mut self,
        item: SlashCommandItem,
        trigger: Option<SlashTrigger>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self.prompt_input.read(cx).value().to_string();
        let trigger = trigger.unwrap_or(SlashTrigger {
            mode: TriggerMode::Browse,
            start_byte: current.len(),
            end_byte: current.len(),
            query: String::new(),
        });
        let (next_text, _caret) = insert_slash_command(&current, &trigger, &item);
        self.slash_picker_state.promote_mru(&item.name);
        self.slash_picker_state.close();
        self.prompt_input.update(cx, |state, cx| {
            state.set_value(next_text, window, cx);
            state.focus(window, cx);
        });
        cx.notify();
    }

    /// Anchored `/` and `$` command autocomplete popover above the chat composer.
    pub(crate) fn render_slash_picker(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.slash_picker_state.is_open() {
            return None;
        }

        let mode = self
            .slash_picker_state
            .trigger
            .as_ref()
            .map(|t| t.mode)
            .unwrap_or(TriggerMode::Browse);
        let mode_label = match mode {
            TriggerMode::Slash => "/ Commands",
            TriggerMode::Dollar => "$ Codex Skills",
            TriggerMode::Browse => "Commands · Ctrl+/",
        };
        let agent = self.slash_picker_state.agent;

        let mut list = v_flex().p_1p5().gap_0p5();
        if self.slash_picker_state.filtered.is_empty() {
            let empty_copy = if mode == TriggerMode::Dollar {
                "No Codex skills discovered yet."
            } else {
                "No matching commands."
            };
            list = list.child(
                div()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .text_color(theme::text_muted())
                    .child(empty_copy),
            );
        } else {
            let start = self.slash_picker_state.scroll_top;
            let end = (start + VISIBLE_ROWS).min(self.slash_picker_state.filtered.len());
            for idx in start..end {
                let item = &self.slash_picker_state.filtered[idx];
                let selected = idx == self.slash_picker_state.selected;
                let item_clone = item.clone();
                let token = format!("{}{}", item.trigger, item.name);
                let show_source = !item.source.is_empty() && item.source != "builtin";

                list = list.child(
                    div()
                        .id(SharedString::from(format!("slash-item-{idx}")))
                        .role(Role::Button)
                        .test_support()
                        .w_full()
                        .px_3()
                        .py_1p5()
                        .rounded(RADIUS_ROW)
                        .bg(if selected {
                            theme::bg_active_row()
                        } else {
                            theme::bg_surface()
                        })
                        .hover(|s| s.bg(theme::bg_hover()))
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.accept_slash_command(item_clone.clone(), window, cx);
                            }),
                        )
                        .child(
                            h_flex()
                                .w_full()
                                .justify_between()
                                .items_center()
                                .gap_3()
                                .child(
                                    h_flex()
                                        .gap_1p5()
                                        .items_center()
                                        .child(
                                            div()
                                                .text_sm()
                                                .text_color(theme::text_primary())
                                                .child(token),
                                        )
                                        .when(show_source, |el| {
                                            el.child(
                                                div()
                                                    .px_1p5()
                                                    .py_0p5()
                                                    .rounded(RADIUS_CHIP)
                                                    .bg(theme::bg_hover())
                                                    .text_xs()
                                                    .text_color(theme::text_muted())
                                                    .child(item.source.clone()),
                                            )
                                        })
                                        .when(item.opens_in_terminal, |el| {
                                            el.child(
                                                div()
                                                    .px_1p5()
                                                    .py_0p5()
                                                    .rounded(RADIUS_CHIP)
                                                    .bg(theme::bg_hover())
                                                    .text_xs()
                                                    .text_color(theme::accent_amber())
                                                    .child("terminal"),
                                            )
                                        }),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme::text_muted())
                                        .truncate()
                                        .child(item.description.clone()),
                                ),
                        ),
                );
            }
        }

        Some(
            div()
                .w_full()
                .max_w(px(760.0))
                .mx_auto()
                .px_5()
                .pb_1()
                .child(
                    v_flex()
                        .id("slash-picker-popover")
                        .w_full()
                        .max_h(px(320.0))
                        .rounded(RADIUS_MODAL)
                        .bg(theme::bg_surface())
                        .border_1()
                        .border_color(theme::border_subtle())
                        .overflow_hidden()
                        .child(
                            h_flex()
                                .w_full()
                                .px_3p5()
                                .py_2()
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
                                                .text_xs()
                                                .text_color(theme::text_primary())
                                                .child(mode_label),
                                        )
                                        .child(
                                            div()
                                                .px_2()
                                                .py_0p5()
                                                .rounded(RADIUS_CHIP)
                                                .bg(theme::bg_hover())
                                                .text_xs()
                                                .text_color(theme::text_secondary())
                                                .child(format!(
                                                    "{} {}",
                                                    agent.glyph(),
                                                    agent.label()
                                                )),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme::text_muted())
                                        .child("↑↓ select · Tab/Enter insert · Esc close"),
                                ),
                        )
                        .child(list),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_detection_gates_slash_and_dollar_by_agent() {
        // `/` at start works for all agents.
        for agent in [AgentKind::Claude, AgentKind::Antigravity, AgentKind::Codex] {
            let t = detect_trigger("/pl", 3, agent).expect("/ at start triggers");
            assert_eq!(t.mode, TriggerMode::Slash);
            assert_eq!(t.start_byte, 0);
            assert_eq!(t.end_byte, 3);
            assert_eq!(t.query, "pl");
        }

        // `/` mid-message works for Claude & Antigravity, rejected for Codex.
        assert!(detect_trigger("please /plan", 12, AgentKind::Claude).is_some());
        assert!(detect_trigger("please /plan", 12, AgentKind::Antigravity).is_some());
        assert!(detect_trigger("please /plan", 12, AgentKind::Codex).is_none());

        // `$` ONLY triggers for Codex skills (rejected for Claude & Antigravity).
        let codex_skill = detect_trigger("run $ver", 8, AgentKind::Codex).expect("codex $ skill");
        assert_eq!(codex_skill.mode, TriggerMode::Dollar);
        assert_eq!(codex_skill.query, "ver");
        assert!(detect_trigger("$ver", 4, AgentKind::Claude).is_none());
        assert!(detect_trigger("$ver", 4, AgentKind::Antigravity).is_none());
    }

    #[test]
    fn fuzzy_prefix_scoring_and_mru_ranking() {
        assert_eq!(score_command("plan", "Plan mode", "plan"), Some(0));
        assert_eq!(score_command("plan", "Plan mode", "pl"), Some(1));
        assert_eq!(
            score_command("verify-multibot", "Verify app", "verfiy"),
            Some(2)
        );
        assert_eq!(
            score_command("grill-me", "Interview", "me"),
            Some(2)
        );
        assert_eq!(
            score_command("compact", "Summarize", "mpa"),
            Some(3)
        );
        assert_eq!(
            score_command("cost", "Show token usage", "tok"),
            Some(4)
        );
        assert_eq!(
            score_command("compact", "Summarize", "cpt"),
            Some(5)
        );
        assert_eq!(score_command("plan", "Plan mode", "zzz"), None);

        // MRU tie-breaker puts recently accepted command first on equal score.
        let mut picker = SlashPickerState::default();
        picker.open_browser(AgentKind::Antigravity, "", 0);
        picker.promote_mru("teamwork");
        picker.open_browser(AgentKind::Antigravity, "", 0);
        assert_eq!(picker.filtered[0].name, "teamwork");
    }

    #[test]
    fn offline_fallback_builtins_and_daemon_merging() {
        for agent in [AgentKind::Claude, AgentKind::Codex, AgentKind::Antigravity] {
            let catalog = builtin_commands_for(agent);
            let names: Vec<&str> = catalog.iter().map(|c| c.name.as_str()).collect();
            assert!(names.contains(&"rename"));
            assert!(names.contains(&"clear"));
            assert!(names.contains(&"plan"));
        }

        // Antigravity specific builtins present offline.
        let agy_names: Vec<String> = builtin_commands_for(AgentKind::Antigravity)
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert!(agy_names.contains(&"grill-me".to_string()));
        assert!(agy_names.contains(&"teamwork".to_string()));

        // Daemon commands override builtins without duplicates, and Codex `$` filters to `$` skills.
        let mut picker = SlashPickerState::default();
        picker.set_daemon_commands(
            AgentKind::Codex,
            vec![
                SlashCommandItem {
                    name: "plan".into(),
                    description: "Custom daemon plan".into(),
                    trigger: "/".into(),
                    source: "project".into(),
                    opens_in_terminal: false,
                },
                SlashCommandItem {
                    name: "verify-multibot".into(),
                    description: "Run verify suite".into(),
                    trigger: "$".into(),
                    source: "skill".into(),
                    opens_in_terminal: false,
                },
            ],
        );

        let slash_items = picker.effective_catalog(AgentKind::Codex, TriggerMode::Slash);
        let plan_matches: Vec<&SlashCommandItem> =
            slash_items.iter().filter(|i| i.name == "plan").collect();
        assert_eq!(plan_matches.len(), 1);
        assert_eq!(plan_matches[0].description, "Custom daemon plan");

        let dollar_items = picker.effective_catalog(AgentKind::Codex, TriggerMode::Dollar);
        assert_eq!(dollar_items.len(), 1);
        assert_eq!(dollar_items[0].name, "verify-multibot");
        assert_eq!(dollar_items[0].trigger, "$");
    }

    #[test]
    fn keyboard_navigation_accepts_without_sending_and_escapes() {
        let mut picker = SlashPickerState::default();
        picker.sync_from_composer("/re", 3, AgentKind::Claude);
        assert!(picker.is_open());
        assert!(picker.filtered.len() >= 2);
        let first_name = picker.filtered[0].name.clone();
        let second_name = picker.filtered[1].name.clone();

        assert_eq!(picker.handle_key("down"), SlashKeyOutcome::Moved);
        assert_eq!(picker.selected, 1);

        assert_eq!(picker.handle_key("up"), SlashKeyOutcome::Moved);
        assert_eq!(picker.selected, 0);

        // Up from 0 wraps to last.
        assert_eq!(picker.handle_key("up"), SlashKeyOutcome::Moved);
        assert_eq!(picker.selected, picker.filtered.len() - 1);

        // Move back to index 1 and accept via Tab (without sending), preserving trigger span.
        picker.selected = 1;
        match picker.handle_key("tab") {
            SlashKeyOutcome::Accepted(item, trig) => {
                assert_eq!(item.name, second_name);
                let trig = trig.expect("accepted outcome must preserve SlashTrigger before close()");
                assert_eq!((trig.start_byte, trig.end_byte, trig.query.as_str()), (0, 3, "re"));
            }
            other => panic!("expected Accepted, got {other:?}"),
        }
        assert!(!picker.is_open());
        assert!(picker.trigger.is_none());

        // Reopen and close via Escape.
        picker.sync_from_composer("/re", 3, AgentKind::Claude);
        assert!(picker.is_open());
        assert_ne!(first_name, second_name);
        assert_eq!(picker.handle_key("escape"), SlashKeyOutcome::Closed);
        assert!(!picker.is_open());
    }

    #[test]
    fn insert_slash_command_preserves_trailing_args_and_codex_dollar_trigger() {
        let trig = detect_trigger("/pl arg1 arg2", 3, AgentKind::Claude).unwrap();
        let item = make_builtin("plan", "Plan mode", false);
        let (out, caret) = insert_slash_command("/pl arg1 arg2", &trig, &item);
        assert_eq!(out, "/plan arg1 arg2");
        assert_eq!(caret, 6);

        let skill_trig = detect_trigger("please $ver now", 11, AgentKind::Codex).unwrap();
        let skill_item = SlashCommandItem {
            name: "verify-multibot".into(),
            description: "Verify".into(),
            trigger: "$".into(),
            source: "skill".into(),
            opens_in_terminal: false,
        };
        let (out2, caret2) = insert_slash_command("please $ver now", &skill_trig, &skill_item);
        assert_eq!(out2, "please $verify-multibot now");
        assert_eq!(caret2, "please $verify-multibot ".len());

        // Browse mode mid-text adds separating space when needed.
        let browse_trig = SlashTrigger {
            mode: TriggerMode::Browse,
            start_byte: 5,
            end_byte: 5,
            query: String::new(),
        };
        let (out3, caret3) = insert_slash_command("hello", &browse_trig, &item);
        assert_eq!(out3, "hello /plan ");
        assert_eq!(caret3, out3.len());
    }

    #[test]
    fn score_command_matches_first_word_of_description() {
        assert_eq!(
            score_command("compact", "Summarize conversation to free context", "summ"),
            Some(4)
        );
        assert_eq!(
            score_command("model", "Choose model", "cho"),
            Some(4)
        );
        assert_eq!(
            score_command("permissions", "Manage tool permission rules", "man"),
            Some(4)
        );
        assert_eq!(
            score_command("cost", "Show token and cost usage", "sho"),
            Some(4)
        );
    }

    #[test]
    fn browse_mode_replaces_only_typed_filter_and_clamps_scroll_top() {
        // 1. Empty composer Ctrl+/ -> type "pl" -> Enter -> "/plan " (no leftover "pl")
        let mut picker = SlashPickerState::default();
        picker.open_browser(AgentKind::Claude, "", 0);
        assert_eq!(picker.browse_anchor_byte, Some(0));
        picker.sync_from_composer("pl", 2, AgentKind::Claude);
        let SlashKeyOutcome::Accepted(item, captured_trig) = picker.handle_key("enter") else {
            panic!("expected accepted plan command");
        };
        assert_eq!(item.name, "plan");
        let trig = captured_trig.expect("handle_key must return captured trigger before close()");
        assert_eq!((trig.start_byte, trig.end_byte, trig.query.as_str()), (0, 2, "pl"));
        let (out, caret) = insert_slash_command("pl", &trig, &item);
        assert_eq!(out, "/plan ");
        assert_eq!(caret, 6);

        // 2. Prefixed composer "hello " -> Ctrl+/ -> type "pl" -> Enter -> "hello /plan "
        picker.open_browser(AgentKind::Claude, "hello ", 6);
        assert_eq!(picker.browse_anchor_byte, Some(6));
        picker.sync_from_composer("hello pl", 8, AgentKind::Claude);
        let SlashKeyOutcome::Accepted(item2, captured_trig2) = picker.handle_key("enter") else {
            panic!("expected accepted plan command");
        };
        assert_eq!(item2.name, "plan");
        let trig2 = captured_trig2.expect("handle_key must return captured browse trigger");
        assert_eq!(
            (trig2.start_byte, trig2.end_byte, trig2.query.as_str()),
            (6, 8, "pl")
        );
        let (out2, caret2) = insert_slash_command("hello pl", &trig2, &item2);
        assert_eq!(out2, "hello /plan ");
        assert_eq!(caret2, "hello /plan ".len());

        // 3. Backspacing past anchor closes browse mode cleanly.
        picker.open_browser(AgentKind::Claude, "hello ", 6);
        picker.sync_from_composer("hello", 5, AgentKind::Claude);
        assert!(!picker.is_open());

        // 4. Shrinking filtered list clamps scroll_top so full visible window is shown.
        picker.open_browser(AgentKind::Claude, "", 0);
        picker.scroll_top = 20;
        picker.selected = 22;
        picker.sync_from_composer("re", 2, AgentKind::Claude);
        assert!(
            picker.scroll_top <= picker.filtered.len().saturating_sub(VISIBLE_ROWS),
            "scroll_top {} exceeded max {}",
            picker.scroll_top,
            picker.filtered.len().saturating_sub(VISIBLE_ROWS)
        );
    }
}
