use crate::{
    app::MultibotView,
    theme::{self, RADIUS_MODAL, RADIUS_ROW},
};
use gpui_kit::base::TestTextExt;
use gpui_kit::{
    AnyElement, Context, InteractiveElement, IntoElement, MouseButton, ParentElement, Role,
    SharedString, StatefulInteractiveElement, Styled, TestSupportExt, Window,
    component::{h_flex, input::InlineToken, v_flex},
    div,
    prelude::FluentBuilder,
    px,
};
use multibot_proto::AgentKind;
use std::ops::Range;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MentionTarget {
    pub name: String,
    pub session_key: String,
    pub session_id: String,
    pub detail: String,
    pub workspace: String,
}

#[derive(Default)]
pub(crate) struct MentionPickerState {
    pub trigger: Option<Range<usize>>,
    pub filtered: Vec<MentionTarget>,
    pub selected: usize,
    dismissed: Option<(String, usize)>,
}

impl MentionPickerState {
    pub fn is_open(&self) -> bool {
        self.trigger.is_some()
    }
    pub fn close(&mut self) {
        self.trigger = None;
        self.filtered.clear();
        self.selected = 0;
    }
    pub fn dismiss(&mut self, text: &str, caret: usize) {
        self.close();
        self.dismissed = Some((text.into(), caret));
    }
    pub fn sync(&mut self, text: &str, caret: usize, targets: Vec<MentionTarget>) {
        if self
            .dismissed
            .as_ref()
            .is_some_and(|(s, c)| s == text && *c == caret)
        {
            return;
        }
        self.dismissed = None;
        let Some(prefix) = text.get(..caret) else {
            self.close();
            return;
        };
        let Some(start) = prefix.rfind('@') else {
            self.close();
            return;
        };
        if (start > 0
            && !prefix[..start]
                .chars()
                .last()
                .is_some_and(char::is_whitespace))
            || prefix[start + 1..].contains(['{', '}'])
            || prefix[start + 1..].chars().any(char::is_whitespace)
            || prefix.chars().last().is_some_and(char::is_whitespace)
        {
            self.close();
            return;
        }
        let query = prefix[start + 1..].to_lowercase();
        let filtered: Vec<_> = targets
            .into_iter()
            .filter(|t| {
                [&t.name, &t.workspace, &t.session_id]
                    .iter()
                    .any(|s| s.to_lowercase().contains(&query))
            })
            .collect();
        if self.filtered != filtered {
            self.selected = 0;
        }
        self.filtered = filtered;
        self.trigger = Some(start..caret);
    }
}

fn boundary(text: &str, end: usize) -> bool {
    text.get(end..)
        .and_then(|s| s.chars().next())
        .is_none_or(|c| !c.is_alphanumeric() && !matches!(c, '_' | '-'))
}

#[derive(Debug)]
pub(crate) struct MentionOccurrence {
    pub range: Range<usize>,
    pub label: String,
    pub target: Option<MentionTarget>,
}

/// Skip code, emails and existing Markdown links. An explicit ID always beats a name.
pub(crate) fn occurrences(text: &str, targets: &[MentionTarget]) -> Vec<MentionOccurrence> {
    let mut found = Vec::new();
    let mut offset = 0;
    let mut fence: Option<char> = None;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let marker = trimmed.chars().next().unwrap();
            if fence == Some(marker) {
                fence = None;
            } else if fence.is_none() {
                fence = Some(marker);
            }
            offset += line.len();
            continue;
        }
        if fence.is_some() {
            offset += line.len();
            continue;
        }
        let mut code_ticks = 0;
        let mut bracket_depth = 0usize;
        let mut i = 0;
        while i < line.len() {
            let ch = line[i..].chars().next().unwrap();
            if ch == '`' {
                let n = line[i..].chars().take_while(|c| *c == '`').count();
                if code_ticks == 0 {
                    code_ticks = n;
                } else if code_ticks == n {
                    code_ticks = 0;
                }
                i += n;
                continue;
            }
            if code_ticks > 0 {
                i += ch.len_utf8();
                continue;
            }
            if ch == '[' {
                bracket_depth += 1;
            }
            if ch == ']' {
                bracket_depth = bracket_depth.saturating_sub(1);
            }
            if ch != '@'
                || bracket_depth > 0
                || (i > 0
                    && line[..i].chars().last().is_some_and(|c| {
                        c.is_alphanumeric() || matches!(c, '_' | '/' | '\\' | '`')
                    }))
            {
                i += ch.len_utf8();
                continue;
            }
            let rest = &line[i + 1..];
            let explicit = rest
                .find('{')
                .filter(|open| *open <= 256 && !rest[..*open].contains(['@', '\n', '[', ']']))
                .and_then(|open| {
                    let end = rest[open + 1..].find('}')? + open + 1;
                    let sid = &rest[open + 1..end];
                    if open == 0
                        || sid.is_empty()
                        || sid.len() > 128
                        || sid
                            .chars()
                            .any(|c| c.is_whitespace() || matches!(c, '{' | '}'))
                    {
                        return None;
                    }
                    Some((
                        end + 2,
                        format!("@{}", &rest[..open]),
                        targets.iter().find(|t| t.session_id == sid).cloned(),
                    ))
                });
            let matched = explicit.or_else(|| {
                let mut matching: Vec<_> = targets
                    .iter()
                    .filter(|t| rest.starts_with(&t.name) && boundary(rest, t.name.len()))
                    .collect();
                matching.sort_by_key(|t| std::cmp::Reverse(t.name.len()));
                let target = matching.first()?;
                if matching.iter().filter(|t| t.name == target.name).count() != 1 {
                    return None;
                }
                Some((
                    target.name.len() + 1,
                    format!("@{}", target.name),
                    Some((*target).clone()),
                ))
            });
            if let Some((length, label, target)) = matched {
                found.push(MentionOccurrence {
                    range: offset + i..offset + i + length,
                    label,
                    target,
                });
                i += length;
            } else {
                i += 1;
            }
        }
        offset += line.len();
    }
    found
}

pub(crate) fn mention_url(key: &str) -> String {
    let hex: String = key.bytes().map(|b| format!("{b:02x}")).collect();
    format!("multibot-session://{hex}")
}

pub(crate) fn key_from_url(url: &str) -> Option<String> {
    let hex = url.strip_prefix("multibot-session://")?;
    if hex.len() > 4096 || hex.len() % 2 != 0 {
        return None;
    }
    let bytes: Option<Vec<u8>> = hex
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            Some((char::from(pair[0]).to_digit(16)? * 16 + char::from(pair[1]).to_digit(16)?) as u8)
        })
        .collect();
    String::from_utf8(bytes?).ok()
}

pub(crate) fn render_mentions(text: &str, targets: &[MentionTarget]) -> String {
    let mut rendered = text.to_string();
    for mention in occurrences(text, targets).into_iter().rev() {
        let label: String = mention
            .label
            .chars()
            .flat_map(|c| {
                if matches!(c, '\\' | '[' | ']' | '*' | '_' | '`') {
                    vec!['\\', c]
                } else {
                    vec![c]
                }
            })
            .collect();
        let replacement = mention.target.map_or(label.clone(), |t| {
            format!("[{label}]({})", mention_url(&t.session_key))
        });
        rendered.replace_range(mention.range, &replacement);
    }
    rendered
}

pub(crate) fn bare_mention(
    label: SharedString,
    key: &str,
    line_height: gpui_kit::Pixels,
) -> gpui_kit::Div {
    let agent = parse_session_key(key).map(|(_, agent, _)| agent);
    let kind = match agent {
        Some("claude") => AgentKind::Claude,
        Some("codex") => AgentKind::Codex,
        Some("antigravity") => AgentKind::Antigravity,
        Some("pi") => AgentKind::Pi,
        Some("cursor") => AgentKind::Cursor,
        _ => AgentKind::Shell,
    };
    let tint = MultibotView::agent_color(kind);
    let initial = label
        .trim_start_matches('@')
        .chars()
        .next()
        .unwrap_or('B')
        .to_uppercase()
        .to_string();
    h_flex()
        .items_center()
        .gap_1()
        .h(line_height)
        .line_height(line_height)
        .text_color(tint)
        .child(
            div()
                .size(px(14.0))
                .flex_shrink_0()
                .rounded_full()
                .bg(tint.opacity(0.16))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(9.0))
                .line_height(px(14.0))
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .child(initial),
        )
        .child(div().child(label.clone()).test_text(label))
}

impl MultibotView {
    pub(crate) fn mention_targets(&self) -> Vec<MentionTarget> {
        let mut targets: Vec<_> = self
            .session_inputs()
            .into_iter()
            .filter(|s| !s.summary.archived)
            .map(|s| {
                let name = self
                    .sidebar_state
                    .workspace_store
                    .effective_title(&s.key, &s.summary.title);
                let name = name
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .replace(['{', '}'], "");
                MentionTarget {
                    name,
                    session_key: s.key.clone(),
                    session_id: s.summary.id.clone(),
                    detail: format!(
                        "{} · {} · {}",
                        s.summary.agent.label(),
                        s.summary.cwd.as_deref().unwrap_or(""),
                        s.host
                    ),
                    workspace: self
                        .sidebar_state
                        .workspace_store
                        .workspace_by_id(
                            &self
                                .sidebar_state
                                .workspace_store
                                .effective_workspace_id(&s.key),
                        )
                        .map(|w| w.name.clone())
                        .or_else(|| s.summary.workspace.clone())
                        .unwrap_or_default(),
                }
            })
            .filter(|t| !t.name.is_empty())
            .collect();
        targets.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then(a.session_key.cmp(&b.session_key))
        });
        targets
    }

    pub(crate) fn sync_mention_picker(&mut self, cx: &mut Context<Self>) {
        if self.main_surface != crate::state::MainSurface::Chat
            || self.chat_state.find_open
            || self.active_modal != crate::state::ActiveModal::None
            || self.slash_picker_state.browse_mode
        {
            self.mention_picker_state.close();
            return;
        }
        let input = self.inputs.prompt_input.read(cx);
        let text = input.value().to_string();
        let caret = input.cursor();
        // Completed atomic mentions must not reopen autocomplete on a caret move.
        if input
            .tokens()
            .iter()
            .any(|s| s.range().start < caret && caret <= s.range().end)
        {
            self.mention_picker_state.close();
            return;
        }
        let targets = self.mention_targets();
        self.mention_picker_state.sync(&text, caret, targets);
        if self.mention_picker_state.is_open() {
            self.slash_picker_state.close();
        }
        cx.notify();
    }

    pub(crate) fn accept_mention(
        &mut self,
        target: MentionTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(range) = self.mention_picker_state.trigger.clone() else {
            return;
        };
        let token = InlineToken::new(target.session_key, format!("@{}", target.name));
        self.inputs.prompt_input.update(cx, |input, cx| {
            if input
                .replace_range_with_token(range, token, window, cx)
                .is_ok()
            {
                input.replace(" ", window, cx);
                input.focus(window, cx);
            }
        });
        self.mention_picker_state.close();
        self.chat_state.slash_sync_pending = false;
        cx.notify();
    }

    pub(crate) fn handle_mention_key(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.mention_picker_state.is_open() {
            return false;
        }
        let state = &mut self.mention_picker_state;
        match key.to_ascii_lowercase().as_str() {
            "escape" => {
                let input = self.inputs.prompt_input.read(cx);
                state.dismiss(input.value().as_ref(), input.cursor());
            }
            "up" => {
                state.selected = state.selected.saturating_sub(1);
            }
            "down" => {
                state.selected = (state.selected + 1).min(state.filtered.len().saturating_sub(1));
            }
            "enter" | "tab" => {
                if let Some(target) = state.filtered.get(state.selected).cloned() {
                    self.accept_mention(target, window, cx);
                }
            }
            _ => return false,
        }
        self.chat_state.slash_sync_pending = false;
        cx.notify();
        true
    }

    pub(crate) fn bound_mention_prompt(&self, cx: &gpui_kit::App) -> String {
        let input = self.inputs.prompt_input.read(cx);
        let mut content = input.content();
        let targets = self.mention_targets();
        for mention in occurrences(&input.value(), &targets) {
            if !input.value()[mention.range.clone()].ends_with('}')
                && !input.tokens().iter().any(|span| {
                    span.range().start < mention.range.end && mention.range.start < span.range().end
                })
                && let Some(target) = mention.target
                && let Ok(bound) = content.clone().with_token(
                    mention.range,
                    InlineToken::new(target.session_key, mention.label),
                )
            {
                content = bound;
            }
        }
        crate::chat_view::compose_snippet_quotes(&content, &self.chat_state.quoted_snippets)
    }

    pub(crate) fn jump_mention_url(&mut self, url: &str, cx: &mut Context<Self>) -> bool {
        let Some(key) = key_from_url(url) else {
            return false;
        };
        self.open_session_key_with_tracking(&key, false, cx);
        true
    }

    pub(crate) fn render_mention_picker(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.mention_picker_state.is_open() {
            return None;
        }
        let state = &self.mention_picker_state;
        let mut list = v_flex().gap_0p5().p_1p5();
        if state.filtered.is_empty() {
            list = list.child(
                div()
                    .px_3()
                    .py_2()
                    .text_sm()
                    .text_color(theme::text_muted())
                    .child("No matching live sessions."),
            );
        }
        let start = state.selected.saturating_sub(7);
        for (index, target) in state.filtered.iter().enumerate().skip(start).take(8) {
            let picked = target.clone();
            list = list.child(
                div()
                    .id(SharedString::from(format!("mention-item-{index}")))
                    .role(Role::Button)
                    .test_support()
                    .px_3()
                    .py_2()
                    .rounded(RADIUS_ROW)
                    .bg(if index == state.selected {
                        theme::bg_active_row()
                    } else {
                        theme::bg_surface()
                    })
                    .hover(|s| s.bg(theme::bg_hover()))
                    .cursor_pointer()
                    .child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme::accent_blue())
                                    .child(format!("@{}", target.name)),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::text_muted())
                                    .child(target.detail.clone()),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::text_muted())
                            .child(target.session_id.clone()),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.accept_mention(picked.clone(), window, cx);
                        }),
                    ),
            );
        }
        Some(
            div()
                .id("mention-picker")
                .test_support()
                .w_full()
                .when(!self.settings.shell_fullscreen, |el| el.max_w(px(960.0)))
                .mx_auto()
                .px_5()
                .child(
                    v_flex()
                        .rounded(RADIUS_MODAL)
                        .bg(theme::bg_surface())
                        .border_1()
                        .border_color(theme::border_subtle())
                        .child(
                            div()
                                .px_3()
                                .pt_2()
                                .text_xs()
                                .text_color(theme::text_muted())
                                .child("Mention a session"),
                        )
                        .child(list)
                        .child(
                            div()
                                .px_3()
                                .pb_2()
                                .text_xs()
                                .text_color(theme::text_muted())
                                .child("↑ ↓ Choose · Enter / Tab Insert · Esc Dismiss"),
                        ),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn target(id: &str) -> MentionTarget {
        MentionTarget {
            name: "Same Name".into(),
            session_id: id.into(),
            session_key: format!("host:claude:{id}"),
            detail: "CL · /repo".into(),
            workspace: "Workspace".into(),
        }
    }
    #[test]
    fn picker_trigger_filter_and_dismissal() {
        let mut state = MentionPickerState::default();
        state.sync("@", 1, vec![target("a"), target("b")]);
        assert!(state.is_open());
        assert_eq!(state.filtered.len(), 2);
        state.sync("hello @WORK", 11, vec![target("a")]);
        assert_eq!(state.filtered.len(), 1);
        state.dismiss("hello @WORK", 11);
        state.sync("hello @WORK", 11, vec![target("a")]);
        assert!(!state.is_open());
        state.sync("a@b", 3, vec![target("a")]);
        assert!(!state.is_open());
    }
    #[test]
    fn explicit_id_renders_clean_and_plain_duplicate_is_not_guessed() {
        let targets = vec![target("a"), target("b")];
        let rendered = render_mentions("Ask @Same Name{b} now", &targets);
        assert_eq!(
            rendered,
            format!("Ask [@Same Name]({}) now", mention_url("host:claude:b"))
        );
        assert!(!rendered.contains("{b}"));
        assert_eq!(render_mentions("@Same Name", &targets), "@Same Name");
        assert_eq!(
            key_from_url(&mention_url("host:claude:b")).unwrap(),
            "host:claude:b"
        );
        let code = "`@Same Name{b}`\n```\n@Same Name{b}\n```\na@Same Name{b}";
        assert_eq!(render_mentions(code, &targets), code);
        assert_eq!(render_mentions("@Offline{gone}", &[]), "@Offline");
    }
}
