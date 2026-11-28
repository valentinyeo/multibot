use super::*;

pub(crate) struct ViewFocusState {
    pub(crate) root_focus_handle: FocusHandle,
    pub(crate) last_focus_layout_sig: Option<FocusLayoutSignature>,
    pub(crate) settings_focus_handle: FocusHandle,
    pub(crate) pending_focus_input: bool,
}

pub(crate) struct ViewConnectionState {
    pub(crate) connected_host: Option<String>,
    pub(crate) connection_status: String,
    pub(crate) daemon_tx: Option<mpsc::UnboundedSender<ClientRequest>>,
    pub(crate) sessions: Vec<SessionSummary>,
    pub(crate) host_sessions: Vec<(String, SessionSummary)>,
    pub(crate) host_txs: HashMap<String, mpsc::UnboundedSender<ClientRequest>>,
    pub(crate) host_statuses: Vec<HostStatusSnapshot>,
    pub(crate) daemon_version: Option<String>,
    pub(crate) daemon_binary_hash: Option<String>,
}

pub(crate) struct SessionSelectionState {
    pub(crate) selected_session_host: Option<String>,
    pub(crate) pending_spawn_host: Option<String>,
    pub(crate) selected_session: Option<SessionSummary>,
    pub(crate) closed_sessions: Vec<(String, SessionSummary)>,
    pub(crate) pending_spawn_open: bool,
    pub(crate) last_dispatched_prompt: Option<(String, String)>,
}

pub(crate) struct PendingSentMessage {
    pub(crate) message: ChatMessage,
    pub(crate) awaiting_reply: bool,
    pub(crate) baseline_server_user_count: usize,
}

pub(crate) struct TranscriptState {
    pub(crate) pending_sent_messages: HashMap<String, Vec<PendingSentMessage>>,
    pub(crate) next_local_message_id: u64,
    pub(crate) messages: Vec<ChatMessage>,
    pub(crate) chat_cache: ChatCache,
    pub(crate) active_switch_token: Option<SwitchToken>,
    pub(crate) pending_switch_tokens: HashMap<String, SwitchToken>,
    pub(crate) pending_older_page_before: HashMap<String, u64>,
    pub(crate) auto_backfill_pages: u8,
    pub(crate) next_search_request_seq: u64,
    pub(crate) remote_search_request_id: Option<String>,
    pub(crate) remote_search_hits: Vec<SearchHit>,
    pub(crate) remote_search_truncated: bool,
}

pub(crate) struct ViewOutboxState {
    pub(crate) shared_chat_outbox: Arc<Mutex<SharedChatOutbox>>,
    pub(crate) chat_outbox: ChatOutbox,
    pub(crate) persist_outbox_to_disk: bool,
    pub(crate) outbox_msg_map: HashMap<String, String>,
}

pub(crate) struct ViewSyncState {
    #[cfg(test)]
    pub(crate) view_id: u64,
    pub(crate) last_synced_revision: u64,
    pub(crate) last_synced_connection_gen: u64,
    pub(crate) last_daemon_notice_seq: u64,
}

pub(crate) struct ViewSubscriptions {
    pub(crate) _shared_store_sub: Subscription,
    pub(crate) _prompt_focus_sub: Subscription,
    pub(crate) _prompt_change_sub: Subscription,
    pub(crate) _window_activation_sub: Subscription,
    pub(crate) _host_focus_sub: Subscription,
    pub(crate) _daemon_event_task: Task<()>,
    pub(crate) _shortcuts_search_sub: Subscription,
}

pub(crate) struct UpdaterRuntimeState {
    pub(crate) update_action_tx: Option<mpsc::UnboundedSender<UpdateUiAction>>,
    pub(crate) preserved_cli_args: Vec<String>,
    pub(crate) update_in_flight: bool,
    pub(crate) last_update_notice_seq: u64,
}

pub(crate) struct ViewInputs {
    pub(crate) deepgram_key_input: Entity<InputState>,
    pub(crate) deepgram_language_input: Entity<InputState>,
    pub(crate) prompt_input: Entity<TextareaState>,
    pub(crate) shortcuts_search_input: Entity<InputState>,
    pub(crate) host_input: Entity<InputState>,
    pub(crate) palette_input: Entity<InputState>,
}
