use crate::app::MultibotView;
use crate::state::SharedAppStore;
use gpui_kit::{Context, Window};
use multibot_core::cache::{CacheKey, CachedSession};
use multibot_core::workspaces::{PaletteCategory, PaletteRow, canonical_session_key};
use multibot_proto::{
    AgentKind, ClientRequest, HandoverContext, HarnessChoice, SessionOptions, SessionSummary,
};

impl MultibotView {
    pub(crate) fn provider_choices(&self) -> Vec<HarnessChoice> {
        self.chat_state
            .harness_choices
            .get(self.active_session_host())
            .cloned()
            .unwrap_or_else(HarnessChoice::defaults)
    }

    pub(crate) fn open_provider_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .selected_session
            .as_ref()
            .is_none_or(|s| s.agent == AgentKind::Shell)
        {
            self.notice = Some("Open a chat before switching provider / harness.".into());
            cx.notify();
            return;
        }
        self.chat_state.composer_menu_open = false;
        self.open_palette(window, cx);
        self.palette_state.provider_picker = true;
        if let Some(tx) = self.active_session_tx() {
            let _ = tx.send(ClientRequest::ListHarnesses);
        }
        cx.notify();
    }

    pub(crate) fn provider_picker_rows(&self, query: &str) -> Vec<PaletteRow> {
        let query = query.to_lowercase();
        self.provider_choices()
            .into_iter()
            .enumerate()
            .filter(|(_, choice)| choice.label.to_lowercase().contains(&query))
            .map(|(index, choice)| PaletteRow::Command {
                id: format!("pal-provider-{index}"),
                label: choice.label,
                chord: String::new(),
                category: PaletteCategory::Sessions,
                destructive: false,
                match_rank: 0,
                frecency: 0,
            })
            .collect()
    }

    pub(crate) fn request_harness_switch(&mut self, target: HarnessChoice, cx: &mut Context<Self>) {
        let Some(mut session) = self.selected_session.clone() else {
            return;
        };
        if session.working || self.chat_state.pending_harness_switch.is_some() {
            self.notice = Some("Wait for the current turn before switching.".into());
            cx.notify();
            return;
        }
        if target.agent == session.agent && target.model == self.active_composer_options().model {
            return;
        }
        let key = self.session_key_for(&session);
        session.title = self
            .sidebar_state
            .workspace_store
            .effective_title(&key, &session.title)
            .to_string();
        session.workspace = self
            .sidebar_state
            .workspace_store
            .workspace_by_id(
                &self
                    .sidebar_state
                    .workspace_store
                    .effective_workspace_id(&key),
            )
            .map(|w| w.name.clone());
        let request = ClientRequest::SwitchHarness {
            session: session.clone(),
            target,
            context: HandoverContext::from_messages(&self.messages),
        };
        if self
            .active_session_tx()
            .is_some_and(|tx| tx.send(request).is_ok())
        {
            self.chat_state.pending_harness_switch = Some(session.id);
            self.notice = Some("Switching provider / harness…".into());
        } else {
            self.notice = Some("Reconnect before switching provider / harness.".into());
        }
        cx.notify();
    }

    pub(crate) fn apply_harness_switch(
        &mut self,
        host: &str,
        previous: SessionSummary,
        session: SessionSummary,
        cx: &mut Context<Self>,
    ) {
        let old = canonical_session_key(host, previous.agent, &previous.id);
        let new = canonical_session_key(host, session.agent, &session.id);
        self.sidebar_state
            .workspace_store
            .replace_session_key(&old, &new);
        if cx.has_global::<SharedAppStore>() {
            cx.global_mut::<SharedAppStore>()
                .apply_harness_switch(host, &previous, &session);
            self.sync_from_shared_store(cx);
        } else {
            self.sessions
                .retain(|s| s.id != previous.id || s.agent != previous.agent);
            self.sessions.insert(0, session.clone());
        }
        if self.chat_state.pending_harness_switch.as_deref() == Some(&previous.id) {
            self.chat_state.pending_harness_switch = None;
        }
        if self.active_session_host() == host
            && self
                .selected_session
                .as_ref()
                .is_some_and(|s| s.id == previous.id && s.agent == previous.agent)
        {
            let old_cache = CacheKey::new(host, previous.agent, &previous.id);
            let mut cached =
                self.chat_cache
                    .get(&old_cache)
                    .cloned()
                    .unwrap_or_else(|| CachedSession {
                        messages: self.messages.clone(),
                        ..Default::default()
                    });
            cached.source_path = session.source_path.clone();
            cached.probe_sha256 = None;
            self.chat_cache
                .put(CacheKey::new(host, session.agent, &session.id), cached);
            if let Some(tx) = self.active_session_tx() {
                let _ = tx.send(ClientRequest::UnwatchSession {
                    session_id: Some(previous.id.clone()),
                });
            }
            let attachments = std::mem::take(&mut self.chat_state.pending_attachments);
            let reply_quote = self.chat_state.reply_quote.take();
            self.open_session_on_host(host.into(), session.clone(), false, cx);
            self.chat_state.pending_attachments = attachments;
            self.chat_state.reply_quote = reply_quote;
            self.chat_state.composer_options.insert(
                new,
                SessionOptions {
                    model: session.model.clone(),
                    ..Default::default()
                },
            );
            self.notice = Some(format!(
                "Switched to {}. Conversation preserved.",
                session.agent.label()
            ));
        }
        self.persist_workspace_store_if_enabled();
        cx.notify();
    }
}
