#[path = "palette_catalog.rs"]
mod catalog;
#[path = "palette_commands.rs"]
mod commands;
#[path = "hibernate_time.rs"]
mod hibernate_time;

use hibernate_time::{HIBERNATE_PRESETS, parse_hibernate_query};
#[path = "shortcuts_modal.rs"]
mod shortcuts_modal;

use crate::app::{CloseOverlay, MultibotView};
use crate::icons::{self, lucide_icon};
use crate::sidebar::InlineRenameState;
use crate::state::{ActiveModal, MainSurface};
use crate::theme::{self, RADIUS_CHIP, RADIUS_MODAL, RADIUS_ROW};
use gpui_kit::{
    ClipboardItem, Context, Focusable, FontWeight, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, ParentElement, Role, ScrollHandle, SharedString, StatefulInteractiveElement,
    Styled, Subscription, TestSupportExt, Window,
    base::TestTextExt,
    component::{
        Sizable,
        button::Button,
        h_flex,
        input::{self, Input, InputEvent},
        label::Label,
        v_flex,
    },
    div, hsla,
    prelude::FluentBuilder,
    px,
};
use multibot_core::SidebarMode;
use multibot_core::workspaces::{
    DEFAULT_WORKSPACE_NAME, PaletteCategory, PaletteCommandSpec, PaletteMode, PaletteRow,
    PaletteStats, STARRED_WORKSPACE_ID, now_unix_ms, rank_palette,
};
use multibot_keys::{ALL_CATEGORIES, by_category};
use multibot_proto::{AgentKind, ClientRequest, PermissionLevel};

const PERMISSION_CHOICES: [(&str, &str, Option<PermissionLevel>, &str); 3] = [
    (
        "pal-permission-full-access",
        "Full access (default)",
        None,
        "Bypass all approvals",
    ),
    (
        "pal-permission-auto",
        "Auto",
        Some(PermissionLevel::Auto),
        "Auto-approve safe edits",
    ),
    (
        "pal-permission-ask",
        "Ask",
        Some(PermissionLevel::Ask),
        "Require approval",
    ),
];

/// Inset from the palette modal's inner edge to the one leading edge that the
/// search text, banner text, section headers, row icons, and row labels share.
/// List rows reach it through the list's 8px padding plus each row's 12px padding.
const PALETTE_LEAD_INSET: f32 = 20.0;

/// Leading icon column of a session row. Rows without an icon reserve the same
/// width, so every label in the main palette starts on one vertical line.
const PALETTE_ICON_COLUMN: f32 = 22.0;

pub(crate) struct SessionDirectoryPicker {
    agent: AgentKind,
    directories: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) enum ReminderTarget {
    Session(String),
    Workspace(String),
}

#[derive(Default)]
pub(crate) struct PaletteState {
    pub server_shells: bool,
    pub shell_kill_target: Option<String>,
    pub stats: PaletteStats,
    pub reminder_target: Option<ReminderTarget>,
    pub reminder_query_clock: Option<(String, i64)>,
    pub hibernate_wake_on_reply: bool,
    pub directory_picker: Option<SessionDirectoryPicker>,
    pub dissolve_workspace_id: Option<String>,
    pub provider_picker: bool,
    pub permission_picker: bool,
    pub selected_index: usize,
    pub mode: PaletteMode,
    pub input_sub: Option<Subscription>,
    pub persist_to_disk: bool,
    pub scroll_handle: ScrollHandle,
}

impl PaletteState {
    /// Whether the rows lead with a leading glyph column. The main palette does:
    /// session rows carry an agent glyph, and every command and workspace row
    /// reserves the same column so all its labels share one line, including
    /// while a query reorders the list. Sub-views that never list a session
    /// (Workspaces, the provider, directory, reminder, dissolve, and end-shell
    /// pickers) start their text on the lead line instead.
    fn leads_with_icon_column(&self) -> bool {
        self.mode == PaletteMode::Global
            && !self.provider_picker
            && self.directory_picker.is_none()
            && self.reminder_target.is_none()
            && self.dissolve_workspace_id.is_none()
            && self.shell_kill_target.is_none()
    }
}

impl MultibotView {
    pub(crate) fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.table_modal_state.is_open() {
            self.table_modal_state.close();
        }
        #[cfg(windows)]
        self.hide_browser_pane();
        self.active_modal = ActiveModal::Palette;
        self.palette_state.provider_picker = false;
        self.palette_state.permission_picker = false;
        self.palette_state.mode = PaletteMode::Global;
        self.palette_state.server_shells = false;
        self.palette_state.shell_kill_target = None;
        self.palette_state.reminder_target = None;
        self.palette_state.directory_picker = None;
        self.palette_state.dissolve_workspace_id = None;
        self.palette_state.selected_index = 0;
        self.palette_state.scroll_handle.scroll_to_item(0);
        self.ensure_palette_input_subscription(window, cx);
        self.inputs.palette_input.update(cx, |state, cx| {
            state.set_placeholder("Search sessions and commands...", window, cx);
            state.set_value("", window, cx);
            state.focus(window, cx);
        });
        cx.notify();
    }

    pub(crate) fn open_workspace_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_state.rename = None;
        self.open_palette(window, cx);
        self.palette_state.mode = PaletteMode::Workspaces;
    }

    pub(crate) fn request_dissolve_workspace(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.sidebar_state.active_workspace_id.clone();
        if self
            .sidebar_state
            .workspace_store
            .workspace_by_id(&id)
            .is_none()
        {
            self.notice = Some("Choose a workspace before dissolving it.".into());
            cx.notify();
            return;
        }
        if self.workspace_live_session_keys(&id).is_empty() {
            self.dissolve_workspace(&id, cx);
            return;
        }
        self.open_palette(window, cx);
        self.palette_state.dissolve_workspace_id = Some(id);
        cx.notify();
    }

    pub(crate) fn open_palette_tab_context(
        &mut self,
        session_key: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.table_modal_state.is_open() {
            self.table_modal_state.close();
        }
        #[cfg(windows)]
        self.hide_browser_pane();
        self.active_modal = ActiveModal::Palette;
        self.palette_state.provider_picker = false;
        self.palette_state.permission_picker = false;
        self.palette_state.reminder_target = None;
        self.palette_state.directory_picker = None;
        self.palette_state.dissolve_workspace_id = None;
        self.palette_state.mode = PaletteMode::TabContext {
            target_session_key: session_key,
        };
        self.palette_state.selected_index = 0;
        self.palette_state.scroll_handle.scroll_to_item(0);
        self.ensure_palette_input_subscription(window, cx);
        self.inputs.palette_input.update(cx, |state, cx| {
            state.set_placeholder("Search sessions and commands...", window, cx);
            state.set_value("", window, cx);
            state.focus(window, cx);
        });
        cx.notify();
    }

    pub(crate) fn open_reminder_picker(
        &mut self,
        target: ReminderTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = match target {
            ReminderTarget::Workspace(id) => {
                let id = if self
                    .sidebar_state
                    .workspace_store
                    .workspace_by_id(&id)
                    .is_some()
                {
                    id
                } else if let Some(key) = self.target_triage_session_key() {
                    self.sidebar_state
                        .workspace_store
                        .effective_workspace_id(&key)
                } else {
                    return;
                };
                ReminderTarget::Workspace(id)
            }
            target => target,
        };
        self.open_palette(window, cx);
        self.palette_state.hibernate_wake_on_reply = false;
        self.palette_state.reminder_query_clock = None;
        self.palette_state.reminder_target = Some(target);
        self.inputs.palette_input.update(cx, |state, cx| {
            state.set_placeholder("Try: 8 am, 3 days, aug 7", window, cx);
        });
        self.palette_state.directory_picker = None;
        self.palette_state.dissolve_workspace_id = None;
    }

    fn ensure_palette_input_subscription(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette_state.input_sub.is_some() {
            return;
        }
        let sub = cx.subscribe_in(
            &self.inputs.palette_input,
            window,
            |this, _input, event, window, cx| {
                if this.active_modal != ActiveModal::Palette {
                    return;
                }
                match event {
                    InputEvent::Change => {
                        if this.palette_state.reminder_target.is_some() {
                            this.palette_state.reminder_query_clock = Some((
                                this.inputs.palette_input.read(cx).value().to_string(),
                                now_unix_ms(),
                            ));
                        }
                        this.palette_state.selected_index = 0;
                        this.palette_state.scroll_handle.scroll_to_item(0);
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } => {
                        this.execute_selected_palette_row(Some(window), cx);
                    }
                    InputEvent::Focus | InputEvent::Blur => {}
                }
            },
        );
        self.palette_state.input_sub = Some(sub);
    }

    fn persist_palette_stats_if_enabled(&self) {
        if self.palette_state.persist_to_disk {
            let _ = self.palette_state.stats.save();
        }
    }

    pub(crate) fn build_palette_rows(&self, query: &str, now_ms: i64) -> Vec<PaletteRow> {
        if self.palette_state.provider_picker {
            return self.provider_picker_rows(query);
        }
        if self.palette_state.permission_picker {
            let query = query.trim().to_lowercase();
            let active = self
                .active_composer_options()
                .permission
                .filter(|p| *p != PermissionLevel::FullAccess);
            return PERMISSION_CHOICES
                .into_iter()
                .filter(|(_, label, _, description)| {
                    format!("{label} {description}")
                        .to_lowercase()
                        .contains(&query)
                })
                .map(|(id, label, permission, description)| PaletteRow::Command {
                    id: id.into(),
                    label: format!("{label} — {description}"),
                    chord: if active == permission { "Active" } else { "" }.into(),
                    category: PaletteCategory::Sessions,
                    destructive: false,
                    match_rank: 0,
                    frecency: 0,
                })
                .collect();
        }
        if self.palette_state.shell_kill_target.is_some() {
            return [
                ("pal-shell-kill-confirm", "End shell permanently", true),
                ("pal-shell-kill-cancel", "Cancel", false),
            ]
            .into_iter()
            .map(|(id, label, destructive)| PaletteRow::Command {
                id: id.into(),
                label: label.into(),
                chord: "Enter · Esc cancels".into(),
                category: PaletteCategory::Sessions,
                destructive,
                match_rank: 0,
                frecency: 0,
            })
            .collect();
        }
        if self.palette_state.server_shells {
            let query = query.trim().to_lowercase();
            return self
                .session_inputs()
                .into_iter()
                .filter(|input| {
                    input.summary.agent == AgentKind::Shell
                        && (!input.summary.archived || input.summary.shell_attached.is_some())
                        && self.tx_for_host(input.host).is_some()
                })
                .filter_map(|input| {
                    let shell = input.summary;
                    let title = self
                        .sidebar_state
                        .workspace_store
                        .effective_title(&input.key, &shell.title);
                    let preview = shell
                        .last_assistant_preview
                        .clone()
                        .or_else(|| shell.cwd.clone())
                        .unwrap_or_default();
                    let attached = match shell.shell_attached {
                        Some(true) => "Active",
                        Some(false) => "Background",
                        None => "Live",
                    };
                    let title = format!("{title} · {}", shell.id);
                    let host = format!("{} · {attached}", input.host);
                    if !format!("{title} {host} {preview}")
                        .to_lowercase()
                        .contains(&query)
                    {
                        return None;
                    }
                    Some(PaletteRow::Session {
                        session_key: input.key,
                        title,
                        agent: AgentKind::Shell,
                        cwd: Some(preview),
                        workspace_name: host,
                        match_rank: 0,
                        frecency: 0,
                    })
                })
                .collect();
        }
        if self.palette_state.dissolve_workspace_id.is_some() {
            return vec![PaletteRow::Command {
                id: "workspace.dissolve.confirm".into(),
                label: "Dissolve workspace and stop agents".into(),
                chord: "Enter · Esc cancels".into(),
                category: PaletteCategory::WorkspacesAndWindows,
                destructive: true,
                match_rank: 0,
                frecency: 0,
            }];
        }
        if let Some(picker) = &self.palette_state.directory_picker {
            let query = query.trim().to_lowercase();
            return picker
                .directories
                .iter()
                .enumerate()
                .filter(|(_, cwd)| cwd.to_lowercase().contains(&query))
                .map(|(idx, cwd)| PaletteRow::Command {
                    id: format!("pal-spawn-directory-{idx}"),
                    label: cwd.clone(),
                    chord: String::new(),
                    category: PaletteCategory::Sessions,
                    destructive: false,
                    match_rank: 0,
                    frecency: 0,
                })
                .collect();
        }
        if let Some(target) = &self.palette_state.reminder_target {
            let clear = match target {
                ReminderTarget::Session(_) => "Move to Inbox",
                ReminderTarget::Workspace(_) => "Unsnooze",
            };
            let Some(now) = chrono::DateTime::from_timestamp_millis(now_ms)
                .map(|now| now.with_timezone(&chrono::Local))
            else {
                return Vec::new();
            };
            let command = |id: String, label: String, chord: String| PaletteRow::Command {
                id,
                label,
                chord,
                category: PaletteCategory::Sessions,
                destructive: false,
                match_rank: 0,
                frecency: 0,
            };
            let presets: Vec<_> = HIBERNATE_PRESETS
                .iter()
                .filter_map(|(id, label)| {
                    let resolved = parse_hibernate_query(label, now)?;
                    Some(command((*id).into(), (*label).into(), resolved.preview))
                })
                .chain(std::iter::once(command(
                    "pal-reminder-4".into(),
                    clear.into(),
                    String::new(),
                )))
                .collect();
            // Anchor relative input to its edit, so Enter confirms the displayed timestamp.
            let query_now = self
                .palette_state
                .reminder_query_clock
                .as_ref()
                .filter(|(text, _)| text == query)
                .and_then(|(_, timestamp)| chrono::DateTime::from_timestamp_millis(*timestamp))
                .map(|time| time.with_timezone(&chrono::Local))
                .unwrap_or(now);
            let query = query.trim().to_lowercase();
            if query.is_empty() {
                return presets;
            }
            let mut rows = Vec::new();
            if let Some(m) = parse_hibernate_query(&query, query_now) {
                rows.push(command(
                    format!("pal-reminder-custom:{}", m.timestamp_ms),
                    m.label,
                    m.preview,
                ));
            }
            rows.extend(
                presets
                    .iter()
                    .filter(|row| {
                        let PaletteRow::Command { label, id, .. } = row else {
                            return false;
                        };
                        label.to_lowercase().contains(&query)
                            || (id == "pal-reminder-4" && query == "unsnooze")
                    })
                    .cloned(),
            );
            return if rows.is_empty() { presets } else { rows };
        }
        let inputs = self.session_inputs();
        let commands = self.build_palette_command_catalog(now_ms);
        let current_key = self
            .selection
            .selected_session
            .as_ref()
            .map(|s| self.session_key_for(s));

        rank_palette(
            &self.sidebar_state.workspace_store,
            &self.palette_state.stats,
            &inputs,
            &commands,
            query,
            &self.palette_state.mode,
            current_key.as_deref(),
            self.sidebar_state.previous_session_key.as_deref(),
            now_ms,
        )
    }

    fn scroll_palette_to_selected(&self, rows: &[PaletteRow]) {
        let mut selectable_idx = 0usize;
        for (raw_idx, row) in rows.iter().enumerate() {
            if !matches!(row, PaletteRow::Header(_)) {
                if selectable_idx == self.palette_state.selected_index {
                    self.palette_state.scroll_handle.scroll_to_item(raw_idx);
                    return;
                }
                selectable_idx += 1;
            }
        }
    }

    fn execute_selected_palette_row(
        &mut self,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        let query = self.inputs.palette_input.read(cx).value().to_string();
        let now = now_unix_ms();
        let rows = self.build_palette_rows(&query, now);
        let selectable: Vec<PaletteRow> = rows
            .into_iter()
            .filter(|r| !matches!(r, PaletteRow::Header(_)))
            .collect();
        if selectable.is_empty() {
            return;
        }
        let idx = self.palette_state.selected_index.min(selectable.len() - 1);
        let chosen = selectable[idx].clone();
        self.execute_palette_row(chosen, now, window, cx);
    }

    fn palette_move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let query = self.inputs.palette_input.read(cx).value().to_string();
        let now = now_unix_ms();
        let rows = self.build_palette_rows(&query, now);
        let selectable_count = rows
            .iter()
            .filter(|r| !matches!(r, PaletteRow::Header(_)))
            .count();
        if delta < 0 {
            self.palette_state.selected_index = self
                .palette_state
                .selected_index
                .min(selectable_count.saturating_sub(1))
                .saturating_sub((-delta) as usize);
            self.scroll_palette_to_selected(&rows);
        } else if delta > 0 && selectable_count > 0 {
            let max_idx = selectable_count - 1;
            self.palette_state.selected_index = self
                .palette_state
                .selected_index
                .min(max_idx)
                .saturating_add(delta as usize)
                .min(max_idx);
            self.scroll_palette_to_selected(&rows);
        }
        cx.notify();
    }

    fn palette_try_exit_submenu(&mut self, cx: &mut Context<Self>) -> bool {
        let query = self.inputs.palette_input.read(cx).value().to_string();
        if query.is_empty()
            && (self.palette_state.provider_picker
                || self.palette_state.permission_picker
                || self.palette_state.server_shells
                || self.palette_state.reminder_target.is_some()
                || self.palette_state.directory_picker.is_some()
                || self.palette_state.dissolve_workspace_id.is_some()
                || self.palette_state.mode != PaletteMode::Global)
        {
            self.palette_state.provider_picker = false;
            self.palette_state.permission_picker = false;
            self.palette_state.mode = PaletteMode::Global;
            self.palette_state.server_shells = false;
            self.palette_state.shell_kill_target = None;
            self.palette_state.reminder_target = None;
            self.palette_state.directory_picker = None;
            self.palette_state.dissolve_workspace_id = None;
            self.palette_state.selected_index = 0;
            self.palette_state.scroll_handle.scroll_to_item(0);
            cx.notify();
            return true;
        }
        false
    }

    fn handle_palette_capture_keydown(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = ev.keystroke.key.to_ascii_lowercase();
        if self.table_modal_state.is_open() {
            if key == "escape" {
                self.table_modal_state.close();
                self.focus_active_surface_default(window, cx);
                cx.stop_propagation();
                cx.notify();
            } else {
                cx.stop_propagation();
                window.prevent_default();
            }
            return;
        }

        if key == "tab"
            && matches!(
                self.palette_state.reminder_target,
                Some(ReminderTarget::Session(_))
            )
        {
            self.palette_state.hibernate_wake_on_reply =
                !self.palette_state.hibernate_wake_on_reply;
            cx.stop_propagation();
            window.prevent_default();
            cx.notify();
            return;
        }
        match key.as_str() {
            "up" => {
                self.palette_move_selection(-1, cx);
                cx.stop_propagation();
            }
            "down" => {
                self.palette_move_selection(1, cx);
                cx.stop_propagation();
            }
            "enter" => {
                self.execute_selected_palette_row(Some(window), cx);
                cx.stop_propagation();
            }
            "escape" => {
                if self.table_modal_state.is_open() {
                    self.table_modal_state.close();
                    self.focus_active_surface_default(window, cx);
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                self.active_modal = ActiveModal::None;
                self.palette_state.mode = PaletteMode::Global;
                self.palette_state.reminder_target = None;
                self.palette_state.directory_picker = None;
                self.palette_state.dissolve_workspace_id = None;
                self.focus_active_surface_default(window, cx);
                cx.stop_propagation();
                cx.notify();
            }
            "backspace" if self.palette_try_exit_submenu(cx) => {
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    pub(crate) fn render_palette_modal(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.inputs.palette_input.read(cx).value().to_string();
        let now = now_unix_ms();
        let rows = self.build_palette_rows(&query, now);
        let selectable_total = rows
            .iter()
            .filter(|r| !matches!(r, PaletteRow::Header(_)))
            .count();
        let clamped_selected = if selectable_total == 0 {
            0
        } else {
            self.palette_state.selected_index.min(selectable_total - 1)
        };

        // Direct scrollable container so `scroll_handle.scroll_to_item(raw_idx)` aligns 1:1 with `rows`
        let mut list = v_flex()
            .id("palette-list-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.palette_state.scroll_handle)
            .gap_1()
            .p_2();
        let has_icon_column = self.palette_state.leads_with_icon_column();
        let mut selectable_idx = 0usize;
        let mut session_el_counter = 0usize;
        let mut ws_el_counter = 0usize;

        if rows.is_empty() {
            list = list.child(
                div()
                    .px_3()
                    .py_4()
                    .text_xs()
                    .text_color(theme::text_muted())
                    .child("No matching sessions, commands, or workspaces."),
            );
        } else {
            for row in rows {
                match row {
                    PaletteRow::Header(title) => {
                        list = list.child(
                            div()
                                .px_3()
                                .pt_1p5()
                                .pb_0p5()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme::text_muted())
                                .child(title.clone().test_text(title)),
                        );
                    }
                    PaletteRow::Session {
                        ref session_key,
                        ref title,
                        agent,
                        ref workspace_name,
                        ..
                    } => {
                        let is_selected = selectable_idx == clamped_selected;
                        selectable_idx += 1;
                        let s_idx = session_el_counter;
                        session_el_counter += 1;
                        let row_clone = row.clone();
                        let title_owned = title.clone();
                        let ws_owned = workspace_name.clone();
                        let shell_key = session_key.clone();
                        let shell_preview = match &row {
                            PaletteRow::Session { cwd, .. } => cwd.clone().unwrap_or_default(),
                            _ => String::new(),
                        };

                        list = list.child(
                            div()
                                .id(SharedString::from(format!("pal-sess-{s_idx}")))
                                .role(Role::Button)
                                .test_support()
                                .px_3()
                                .py_2()
                                .rounded(RADIUS_ROW)
                                .bg(if is_selected {
                                    theme::bg_active_row()
                                } else {
                                    theme::bg_surface()
                                })
                                .hover(|s| s.bg(theme::bg_hover()))
                                .cursor_pointer()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, window, cx| {
                                        this.execute_palette_row(
                                            row_clone.clone(),
                                            now_unix_ms(),
                                            Some(window),
                                            cx,
                                        );
                                    }),
                                )
                                .child(
                                    h_flex()
                                        .justify_between()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            h_flex()
                                                .flex_1()
                                                .min_w_0()
                                                .gap_2()
                                                .items_center()
                                                .child(
                                                    div()
                                                        .w(px(PALETTE_ICON_COLUMN))
                                                        .h(px(PALETTE_ICON_COLUMN))
                                                        .rounded(RADIUS_CHIP)
                                                        .bg(Self::agent_color(agent))
                                                        .flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .text_xs()
                                                        .text_color(theme::bg_canvas())
                                                        .child(agent.glyph()),
                                                )
                                                .child(
                                                    div()
                                                        .text_sm()
                                                        .text_color(theme::text_primary())
                                                        .truncate()
                                                        .child(
                                                            title_owned
                                                                .clone()
                                                                .test_text(title_owned),
                                                        ),
                                                ),
                                        )
                                        .child(
                                            div()
                                                .px_2()
                                                .py_0p5()
                                                .rounded(RADIUS_CHIP)
                                                .bg(theme::bg_hover())
                                                .text_xs()
                                                .text_color(theme::text_muted())
                                                .child(ws_owned),
                                        )
                                        .when(self.palette_state.server_shells, |el| {
                                            el.child(
                                                Button::new(format!("pal-shell-end-{s_idx}"))
                                                    .small()
                                                    .icon(lucide_icon(icons::SQUARE_FILLED, 12.0))
                                                    .accessibility_label("End shell")
                                                    .tooltip("Kill shell")
                                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                                        cx.stop_propagation()
                                                    })
                                                    .on_click(cx.listener(
                                                        move |this, _, window, cx| {
                                                            cx.stop_propagation();
                                                            this.palette_state.shell_kill_target =
                                                                Some(shell_key.clone());
                                                            this.palette_state.selected_index = 0;
                                                            this.inputs.palette_input.update(
                                                                cx,
                                                                |input, cx| {
                                                                    input.set_value("", window, cx);
                                                                    input.focus(window, cx);
                                                                },
                                                            );
                                                            cx.notify();
                                                        },
                                                    )),
                                            )
                                        }),
                                )
                                .when(self.palette_state.server_shells, |el| {
                                    el.child(
                                        div()
                                            .text_xs()
                                            .text_color(theme::text_muted())
                                            .child(shell_preview),
                                    )
                                }),
                        );
                    }
                    PaletteRow::Command {
                        ref id,
                        ref label,
                        ref chord,
                        destructive,
                        ..
                    } => {
                        let is_selected = selectable_idx == clamped_selected;
                        selectable_idx += 1;
                        let row_clone = row.clone();
                        let el_id = SharedString::from(id.clone());
                        let label_owned = label.clone();
                        let chord_owned = chord.clone();

                        list = list.child(
                            div()
                                .id(el_id)
                                .role(Role::Button)
                                .test_support()
                                .px_3()
                                .py_2()
                                .rounded(RADIUS_ROW)
                                .bg(if is_selected {
                                    theme::bg_active_row()
                                } else {
                                    theme::bg_surface()
                                })
                                .hover(|s| s.bg(theme::bg_hover()))
                                .cursor_pointer()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, window, cx| {
                                        this.execute_palette_row(
                                            row_clone.clone(),
                                            now_unix_ms(),
                                            Some(window),
                                            cx,
                                        );
                                    }),
                                )
                                .child(
                                    h_flex()
                                        .justify_between()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            h_flex()
                                                .flex_1()
                                                .min_w_0()
                                                .gap_2()
                                                .items_center()
                                                .when(has_icon_column, |el| {
                                                    el.child(
                                                        div()
                                                            .w(px(PALETTE_ICON_COLUMN))
                                                            .flex_shrink_0(),
                                                    )
                                                })
                                                .child(
                                                    div()
                                                        .text_sm()
                                                        .text_color(if destructive {
                                                            theme::accent_amber()
                                                        } else {
                                                            theme::text_primary()
                                                        })
                                                        .truncate()
                                                        .child(
                                                            label_owned
                                                                .clone()
                                                                .test_text(label_owned),
                                                        ),
                                                ),
                                        )
                                        .when(!chord_owned.is_empty(), |el| {
                                            el.child(
                                                div()
                                                    .px_2()
                                                    .py_0p5()
                                                    .rounded(RADIUS_CHIP)
                                                    .bg(theme::bg_hover())
                                                    .text_xs()
                                                    .text_color(theme::text_muted())
                                                    .child(chord_owned),
                                            )
                                        }),
                                ),
                        );
                    }
                    PaletteRow::Workspace {
                        ref name, archived, ..
                    } => {
                        let is_selected = selectable_idx == clamped_selected;
                        selectable_idx += 1;
                        let w_idx = ws_el_counter;
                        ws_el_counter += 1;
                        let row_clone = row.clone();
                        let name_owned = name.clone();

                        list = list.child(
                            div()
                                .id(SharedString::from(format!("pal-ws-{w_idx}")))
                                .role(Role::Button)
                                .test_support()
                                .px_3()
                                .py_2()
                                .rounded(RADIUS_ROW)
                                .bg(if is_selected {
                                    theme::bg_active_row()
                                } else {
                                    theme::bg_surface()
                                })
                                .hover(|s| s.bg(theme::bg_hover()))
                                .cursor_pointer()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, window, cx| {
                                        this.execute_palette_row(
                                            row_clone.clone(),
                                            now_unix_ms(),
                                            Some(window),
                                            cx,
                                        );
                                    }),
                                )
                                .child(
                                    h_flex()
                                        .justify_between()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            h_flex()
                                                .flex_1()
                                                .min_w_0()
                                                .gap_2()
                                                .items_center()
                                                .when(has_icon_column, |el| {
                                                    el.child(
                                                        div()
                                                            .w(px(PALETTE_ICON_COLUMN))
                                                            .flex_shrink_0(),
                                                    )
                                                })
                                                .child(
                                                    div()
                                                        .text_sm()
                                                        .text_color(theme::text_primary())
                                                        .truncate()
                                                        .child(
                                                            name_owned
                                                                .clone()
                                                                .test_text(name_owned),
                                                        ),
                                                ),
                                        )
                                        .child(
                                            div()
                                                .px_2()
                                                .py_0p5()
                                                .rounded(RADIUS_CHIP)
                                                .bg(theme::bg_hover())
                                                .text_xs()
                                                .text_color(theme::text_muted())
                                                .child(if archived {
                                                    "Archived Workspace"
                                                } else {
                                                    "Workspace"
                                                }),
                                        ),
                                ),
                        );
                    }
                }
            }
        }

        let hibernate = matches!(
            self.palette_state.reminder_target,
            Some(ReminderTarget::Session(_))
        );
        let (title, hint): (String, &str) = if self.palette_state.permission_picker {
            ("Permission mode".into(), "Up/Down · Enter · Backspace")
        } else if self.palette_state.provider_picker {
            (
                "Switch provider / harness".into(),
                "Up/Down · Enter · Backspace",
            )
        } else if self.palette_state.shell_kill_target.is_some() {
            (
                "End server shell permanently? Running work will stop.".into(),
                "Enter · Esc",
            )
        } else if self.palette_state.server_shells {
            ("Server shells".into(), "Enter to attach · End to kill")
        } else if let Some(id) = &self.palette_state.dissolve_workspace_id {
            let name = self
                .sidebar_state
                .workspace_store
                .workspace_by_id(id)
                .map(|w| w.name.as_str())
                .unwrap_or("workspace");
            (
                format!(
                    "Dissolve {name}? Stop {} live agents; keep chat logs.",
                    self.workspace_live_session_keys(id).len()
                ),
                "Enter · Esc",
            )
        } else if let Some(picker) = &self.palette_state.directory_picker {
            (
                format!("New {} session directory", picker.agent.label()),
                "Up/Down · Enter · Backspace",
            )
        } else if hibernate {
            ("Hibernate".into(), "Tab toggles · Up/Down · Enter")
        } else if self.palette_state.reminder_target.is_some() {
            ("Snooze workspace".into(), "Up/Down · Enter · Backspace")
        } else if self.palette_state.mode == PaletteMode::Workspaces {
            ("Workspaces".into(), "Up/Down · Enter · Backspace")
        } else if let Some(key) = self.palette_state.mode.target_session_key() {
            let title = self
                .find_session_by_key(key)
                .map(|s| {
                    self.sidebar_state
                        .workspace_store
                        .effective_title(key, &s.title)
                        .to_string()
                })
                .unwrap_or_else(|| key.to_string());
            (format!("Session actions: {title}"), "Backspace to return")
        } else {
            ("Command palette".into(), "Up/Down · Enter · Esc")
        };

        div()
            .id("palette-backdrop")
            .absolute()
            .inset_0()
            .bg(hsla(0.0, 0.0, 0.0, 0.58))
            .flex()
            .items_start()
            .justify_center()
            .pt(px(88.0))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.active_modal = ActiveModal::None;
                    this.palette_state.mode = PaletteMode::Global;
                    this.palette_state.reminder_target = None;
                    this.palette_state.directory_picker = None;
                    this.palette_state.dissolve_workspace_id = None;
                    this.focus_active_surface_default(window, cx);
                    cx.notify();
                }),
            )
            .child(
                v_flex()
                    .id("palette-modal")
                    .w(px(560.0))
                    .max_h(px(520.0))
                    .rounded(RADIUS_MODAL)
                    .bg(theme::bg_surface())
                    .border_1()
                    .border_color(theme::border_subtle())
                    .on_action(cx.listener(|this, _: &CloseOverlay, window, cx| {
                        if this.table_modal_state.is_open() {
                            this.table_modal_state.close();
                            this.focus_active_surface_default(window, cx);
                            cx.notify();
                            return;
                        }
                        this.palette_state.mode = PaletteMode::Global;
                        this.palette_state.reminder_target = None;
                        this.palette_state.directory_picker = None;
                        this.palette_state.dissolve_workspace_id = None;
                        this.active_modal = ActiveModal::None;
                        this.focus_active_surface_default(window, cx);
                        cx.notify();
                    }))
                    .capture_action(cx.listener(|this, _: &input::MoveUp, _, cx| {
                        if this.table_modal_state.is_open() {
                            cx.stop_propagation();
                            return;
                        }
                        this.palette_move_selection(-1, cx);
                        cx.stop_propagation();
                    }))
                    .capture_action(cx.listener(|this, _: &input::MoveDown, _, cx| {
                        if this.table_modal_state.is_open() {
                            cx.stop_propagation();
                            return;
                        }
                        this.palette_move_selection(1, cx);
                        cx.stop_propagation();
                    }))
                    .when(
                        matches!(
                            self.palette_state.reminder_target,
                            Some(ReminderTarget::Session(_))
                        ),
                        |el| el.key_context("HibernatePicker"),
                    )
                    .capture_action(cx.listener(
                        |this, _: &crate::app::ToggleHibernateMode, _, cx| {
                            if matches!(
                                this.palette_state.reminder_target,
                                Some(ReminderTarget::Session(_))
                            ) {
                                this.palette_state.hibernate_wake_on_reply =
                                    !this.palette_state.hibernate_wake_on_reply;
                                cx.stop_propagation();
                                cx.notify();
                            }
                        },
                    ))
                    .capture_action(cx.listener(|this, _: &input::Enter, window, cx| {
                        if this.table_modal_state.is_open() {
                            cx.stop_propagation();
                            return;
                        }
                        this.execute_selected_palette_row(Some(window), cx);
                        cx.stop_propagation();
                    }))
                    .capture_action(cx.listener(|this, _: &input::Backspace, _, cx| {
                        if this.table_modal_state.is_open() {
                            cx.stop_propagation();
                            return;
                        }
                        if this.palette_try_exit_submenu(cx) {
                            cx.stop_propagation();
                        }
                    }))
                    .capture_action(cx.listener(|this, _: &input::Escape, window, cx| {
                        if this.table_modal_state.is_open() {
                            this.table_modal_state.close();
                            this.focus_active_surface_default(window, cx);
                            cx.stop_propagation();
                            cx.notify();
                            return;
                        }
                        this.palette_state.mode = PaletteMode::Global;
                        this.palette_state.reminder_target = None;
                        this.palette_state.directory_picker = None;
                        this.palette_state.dissolve_workspace_id = None;
                        this.active_modal = ActiveModal::None;
                        this.focus_active_surface_default(window, cx);
                        cx.stop_propagation();
                        cx.notify();
                    }))
                    .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                        this.handle_palette_capture_keydown(ev, window, cx);
                    }))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .child(
                        h_flex()
                            .id("palette-context-banner")
                            .test_support()
                            .justify_between()
                            .items_center()
                            .gap_3()
                            .pl(px(PALETTE_LEAD_INSET))
                            .pr_4()
                            .pt_3()
                            .pb_2()
                            .border_b_1()
                            .border_color(theme::border_subtle())
                            .child(
                                h_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_2()
                                    .items_center()
                                    .child(
                                        div()
                                            .id("palette-title")
                                            .test_support()
                                            .min_w_0()
                                            .truncate()
                                            .text_sm()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(theme::text_primary())
                                            .child(title.clone().test_text(title)),
                                    )
                                    .when(hibernate, |el| {
                                        el.child(
                                            div()
                                                .id("hibernate-wake-mode")
                                                .test_support()
                                                .role(Role::Button)
                                                .flex_shrink_0()
                                                .whitespace_nowrap()
                                                .px_2p5()
                                                .py_1()
                                                .rounded(RADIUS_CHIP)
                                                .bg(theme::bg_active_row())
                                                .border_1()
                                                .border_color(theme::accent_blue())
                                                .text_sm()
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(theme::text_primary())
                                                .cursor_pointer()
                                                .hover(|s| s.bg(theme::bg_hover()))
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.palette_state.hibernate_wake_on_reply =
                                                        !this.palette_state.hibernate_wake_on_reply;
                                                    cx.notify();
                                                }))
                                                .child(Label::new(
                                                    if self.palette_state.hibernate_wake_on_reply {
                                                        "Wake on reply"
                                                    } else {
                                                        "Stay hidden"
                                                    },
                                                )),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .id("palette-keyboard-hint")
                                    .test_support()
                                    .flex_shrink_0()
                                    .text_xs()
                                    .text_color(theme::text_muted())
                                    .child(hint),
                            ),
                    )
                    .child(
                        div()
                            .id("palette-search-row")
                            .test_support()
                            .pl(px(PALETTE_LEAD_INSET))
                            .pr_3()
                            .py_2p5()
                            .border_b_1()
                            .border_color(theme::border_subtle())
                            .child(
                                Input::new(&self.inputs.palette_input)
                                    .id("palette-search")
                                    // The row owns the gutter, so the caret and
                                    // placeholder sit on the shared lead line.
                                    .pl(px(0.0))
                                    .appearance(false),
                            ),
                    )
                    .child(list),
            )
    }
}

#[cfg(test)]
#[path = "palette_tests.rs"]
mod tests;
