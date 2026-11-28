use crate::app::MultibotView;
use crate::icons::{self, lucide_icon};
use crate::state::ActiveModal;
use crate::theme::{self, RADIUS_MODAL};
use gpui_kit::{
    ClipboardItem, Context, InteractiveElement, IntoElement, MouseButton, ParentElement,
    StatefulInteractiveElement, Styled, TestSupportExt, Window,
    component::{Sizable, button::Button, h_flex, label::Label, v_flex},
    div, px,
};
use multibot_proto::{AgentKind, EffortLevel};

fn context_label(used: Option<u64>, limit: Option<u64>) -> String {
    let tokens = |n: u64| {
        if n >= 1000 && n % 1000 == 0 {
            format!("{}k", n / 1000)
        } else {
            n.to_string()
        }
    };
    match (used, limit.filter(|n| *n > 0)) {
        (Some(used), Some(limit)) => format!(
            "{} / {} ({}%)",
            tokens(used),
            tokens(limit),
            used.saturating_mul(100) / limit
        ),
        (Some(used), None) => format!("{} used · limit not reported", tokens(used)),
        _ => "Not reported".into(),
    }
}

impl MultibotView {
    pub(crate) fn toggle_chat_info(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selection.selected_session.is_none() {
            return;
        }
        self.active_modal = if self.active_modal == ActiveModal::ChatInfo {
            ActiveModal::None
        } else {
            ActiveModal::ChatInfo
        };
        self.shell_host.focus_parent();
        self.focus_active_surface_default(window, cx);
        cx.notify();
    }

    pub(crate) fn render_chat_info(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let session = self.selection.selected_session.as_ref();
        let options = session
            .and_then(|s| self.composer_options_for(s))
            .unwrap_or_default();
        let model = options
            .model
            .as_deref()
            .or_else(|| session.and_then(|s| s.model.as_deref()))
            .unwrap_or("Default");
        let effort = match options.effort.or_else(|| session.and_then(|s| s.effort)) {
            Some(EffortLevel::Low) => "Low",
            Some(EffortLevel::Medium) => "Medium",
            Some(EffortLevel::High) => "High",
            Some(EffortLevel::Max) => "Max",
            None => "Default",
        };
        let cwd = session.and_then(|s| s.cwd.clone());
        let runtime = match session.map(|s| s.agent) {
            Some(AgentKind::Claude) => "Claude Code",
            Some(AgentKind::Shell) => "Live Shell",
            Some(agent) => agent.label(),
            None => "Not reported",
        };
        let host = self
            .selection
            .selected_session_host
            .as_deref()
            .or(self.connection.connected_host.as_deref())
            .unwrap_or("Local");
        let title = session
            .map(|s| {
                let key = self.session_key_for(s);
                self.sidebar_state
                    .workspace_store
                    .effective_title(&key, &s.title)
                    .to_string()
            })
            .unwrap_or_default();
        let rows = [
            ("chat-info-title", "Session", title),
            (
                "chat-info-id",
                "Session ID",
                session.map(|s| s.id.clone()).unwrap_or_default(),
            ),
            (
                "chat-info-cwd",
                "Working directory",
                cwd.clone().unwrap_or_else(|| "Not reported".into()),
            ),
            (
                "chat-info-runtime",
                "Runtime / Harness",
                format!("{runtime} · {host}"),
            ),
            ("chat-info-model", "LLM / Model", model.to_string()),
            ("chat-info-effort", "Effort", effort.to_string()),
            (
                "chat-info-context",
                "Context window",
                context_label(
                    session.and_then(|s| s.context_tokens),
                    session.and_then(|s| s.context_limit),
                ),
            ),
        ];
        div()
            .id("chat-info-backdrop")
            .test_support()
            .absolute()
            .inset_0()
            .bg(gpui_kit::hsla(0.0, 0.0, 0.0, 0.58))
            .flex()
            .items_center()
            .justify_center()
            .p_6()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.active_modal = ActiveModal::None;
                    this.focus_active_surface_default(window, cx);
                    cx.notify();
                }),
            )
            .child(
                v_flex()
                    .id("chat-info-panel")
                    .test_support()
                    .w(px(560.0))
                    .max_w(gpui_kit::relative(1.0))
                    .max_h(gpui_kit::relative(0.9))
                    .rounded(RADIUS_MODAL)
                    .bg(theme::bg_surface())
                    .border_1()
                    .border_color(theme::border_subtle())
                    .p_5()
                    .gap_4()
                    .overflow_y_scroll()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        h_flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .text_color(theme::text_primary())
                                    .child("Chat Info"),
                            )
                            .child(
                                Button::new("close-chat-info")
                                    .xsmall()
                                    .icon(lucide_icon(icons::X, 14.0))
                                    .accessibility_label("Close")
                                    .tooltip(crate::icons::shortcut_tooltip(
                                        "Close",
                                        "system.escape",
                                    ))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.active_modal = ActiveModal::None;
                                        this.focus_active_surface_default(window, cx);
                                        cx.notify();
                                    })),
                            ),
                    )
                    .children(rows.into_iter().map(|(id, label, value)| {
                        use gpui_kit::prelude::FluentBuilder;
                        v_flex()
                            .gap_1()
                            .child(div().text_xs().text_color(theme::text_muted()).child(label))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_start()
                                    .child(
                                        div()
                                            .id(id)
                                            .test_support()
                                            .flex_1()
                                            .min_w_0()
                                            .text_sm()
                                            .text_color(theme::text_primary())
                                            .child(Label::new(value)),
                                    )
                                    .when(id == "chat-info-cwd" && cwd.is_some(), |el| {
                                        let cwd = cwd.clone().unwrap_or_default();
                                        el.child(
                                            Button::new("copy-chat-cwd")
                                                .xsmall()
                                                .icon(lucide_icon(icons::COPY, 14.0))
                                                .accessibility_label("Copy")
                                                .tooltip("Copy")
                                                .on_click(move |_, _, cx| {
                                                    cx.write_to_clipboard(
                                                        ClipboardItem::new_string(cwd.clone()),
                                                    )
                                                }),
                                        )
                                    }),
                            )
                    })),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_uses_only_reported_values() {
        assert_eq!(
            context_label(Some(42_000), Some(200_000)),
            "42k / 200k (21%)"
        );
        assert_eq!(context_label(None, None), "Not reported");
        assert_eq!(
            context_label(Some(42_000), Some(0)),
            "42k used · limit not reported"
        );
    }
}
