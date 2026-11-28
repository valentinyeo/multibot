use super::*;
use gpui_kit::{AppContext, TestAppContext, WindowOptions, test::TestWindowExt};
use multibot_proto::{AgentKind, PromptOption};
use std::sync::Arc;

// ------------------------------------------------------------------------
// Pure Unit Tests
// ------------------------------------------------------------------------

#[test]
fn extract_latest_code_block_strips_backtick_and_tilde_fences_and_preserves_blank_lines() {
    let msgs = vec![
        ChatMessage {
            id: "m1".into(),
            role: MessageRole::Assistant,
            text: "Here is rust:\n```rust\nfn a() {\n\n    println!(\"hi\");\n}\n```\n".into(),
            ..Default::default()
        },
        ChatMessage {
            id: "m2".into(),
            role: MessageRole::Assistant,
            text:
                "And python:\n~~~python\ndef f():\n\n    return 1\n~~~\nAnd empty:\n```\n   \n```"
                    .into(),
            ..Default::default()
        },
    ];
    assert_eq!(
        extract_latest_code_block(&msgs).as_deref(),
        Some("def f():\n\n    return 1")
    );

    let no_code = vec![ChatMessage {
        id: "m3".into(),
        role: MessageRole::Assistant,
        text: "Plain prose only.".into(),
        ..Default::default()
    }];
    assert_eq!(extract_latest_code_block(&no_code), None);
}

#[test]
fn copy_latest_code_block_chord_rejects_altgr_ctrl_alt() {
    assert!(is_copy_latest_code_block_modifiers(
        true, true, false, false, "d"
    ));
    assert!(is_copy_latest_code_block_modifiers(
        true, true, false, false, "D"
    ));
    assert!(!is_copy_latest_code_block_modifiers(
        true, true, true, false, "d"
    ));
    assert!(!is_copy_latest_code_block_modifiers(
        true, false, true, false, "d"
    ));
    assert!(!is_copy_latest_code_block_modifiers(
        true, false, false, false, "d"
    ));
    assert!(!is_copy_latest_code_block_modifiers(
        true, true, false, true, "d"
    ));
}

#[test]
fn diff_classification_and_fold_expand_counts() {
    assert_eq!(classify_diff_line("+added"), DiffLineKind::Added);
    assert_eq!(classify_diff_line("+++ b/file"), DiffLineKind::Context);
    assert_eq!(classify_diff_line("-removed"), DiffLineKind::Removed);
    assert_eq!(classify_diff_line("--- a/file"), DiffLineKind::Context);
    assert_eq!(classify_diff_line(" context"), DiffLineKind::Context);

    let diff = DiffBlock {
        path: Some("src/lib.rs".into()),
        added: 25,
        removed: 25,
        lines: (0..50).map(|i| format!("+line {i}")).collect(),
        truncated_lines: 12,
    };
    let (folded, folded_rem) = visible_diff_slice(&diff, false);
    assert_eq!(folded.len(), DIFF_FOLDED_ROWS);
    assert_eq!(folded_rem, 50 - 6 + 12);

    let (expanded, expanded_rem) = visible_diff_slice(&diff, true);
    assert_eq!(expanded.len(), DIFF_EXPANDED_ROWS);
    assert_eq!(expanded_rem, 50 - 40 + 12);
}

#[test]
fn effective_todos_prefers_latest_message_todos_and_counts_done() {
    let msgs = vec![
        ChatMessage {
            id: "m1".into(),
            todos: vec![TodoItem {
                content: "old".into(),
                status: TodoStatus::Pending,
            }],
            ..Default::default()
        },
        ChatMessage {
            id: "m2".into(),
            todos: vec![
                TodoItem {
                    content: "a".into(),
                    status: TodoStatus::Completed,
                },
                TodoItem {
                    content: "b".into(),
                    status: TodoStatus::InProgress,
                },
                TodoItem {
                    content: "c".into(),
                    status: TodoStatus::Completed,
                },
            ],
            ..Default::default()
        },
    ];
    let (todos, done, total) = effective_todos(&msgs, None, None);
    assert_eq!(todos.len(), 3);
    assert_eq!(done, 2);
    assert_eq!(total, 3);
}

#[test]
fn structured_question_prompt_validates_limits_and_formats_single_and_multi_select() {
    let q = PromptQuestion {
        question: "Pick targets".into(),
        header: "Targets".into(),
        multi_select: true,
        options: vec![
            PromptOption {
                label: "Linux".into(),
                description: String::new(),
            },
            PromptOption {
                label: "Windows".into(),
                description: String::new(),
            },
            PromptOption {
                label: "macOS".into(),
                description: String::new(),
            },
        ],
    };
    assert!(validate_question_prompt("p1", std::slice::from_ref(&q)).is_some());
    assert!(validate_question_prompt("", std::slice::from_ref(&q)).is_none());
    assert!(validate_question_prompt("p1", &[]).is_none());
    let five_qs = vec![q.clone(); 5];
    assert!(validate_question_prompt("p1", &five_qs).is_none());

    assert_eq!(format_single_option_reply(1, "Windows"), "2: Windows");
    assert_eq!(
        format_multi_option_reply(&q, (1 << 0) | (1 << 2)).as_deref(),
        Some("1: Linux, 3: macOS")
    );
    assert_eq!(format_multi_option_reply(&q, 0), None);
}

#[test]
fn prose_option_fallback_only_matches_latest_assistant_outside_code_blocks() {
    let assistant_abc = vec![ChatMessage {
        id: "a1".into(),
        role: MessageRole::Assistant,
        text: "How should we proceed?\nA) Keep cache\nB) Rebuild index\nC) Skip".into(),
        ..Default::default()
    }];
    let opts = detect_trailing_prose_options(&assistant_abc).expect("should match A/B/C");
    assert_eq!(opts.len(), 3);
    assert_eq!(opts[0].label, "A");
    assert_eq!(opts[2].title, "Skip");

    // Older assistant message followed by user reply -> MUST return None.
    let mut with_user_after = assistant_abc.clone();
    with_user_after.push(ChatMessage {
        id: "u1".into(),
        role: MessageRole::User,
        text: "A".into(),
        ..Default::default()
    });
    assert!(detect_trailing_prose_options(&with_user_after).is_none());

    // Inside fenced code block -> MUST return None.
    let inside_code = vec![ChatMessage {
        id: "a2".into(),
        role: MessageRole::Assistant,
        text: "Example:\n```text\nA) Foo\nB) Bar\n```".into(),
        ..Default::default()
    }];
    assert!(detect_trailing_prose_options(&inside_code).is_none());

    // Numbered list WITH question cue matches; >4 items rejected.
    let numbered = vec![ChatMessage {
        id: "a3".into(),
        role: MessageRole::Assistant,
        text: "Which option do you prefer?\n1. Fast path\n2. Thorough path".into(),
        ..Default::default()
    }];
    assert_eq!(detect_trailing_prose_options(&numbered).unwrap().len(), 2);

    let five_items = vec![ChatMessage {
        id: "a4".into(),
        role: MessageRole::Assistant,
        text: "Pick:\nA) 1\nB) 2\nC) 3\nD) 4\nE) 5".into(),
        ..Default::default()
    }];
    assert!(detect_trailing_prose_options(&five_items).is_none());
}

#[test]
fn compose_quoted_reply_caps_at_10_lines_and_hoists_skill_not_filepath() {
    let twelve_lines = (1..=12)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let block = format_quote_block(&twelve_lines);
    let lines: Vec<&str> = block.lines().collect();
    assert_eq!(lines.len(), 11);
    assert_eq!(lines[0], "> line 1");
    assert_eq!(lines[9], "> line 10");
    assert_eq!(lines[10], "> …");

    // MUBO-457: leading /skill hoisted above quote.
    assert_eq!(
        compose_quoted_reply(Some("error here"), "/pstack:poteto-help why?\nmore details"),
        "/pstack:poteto-help why?\n> error here\n\nmore details"
    );
    assert_eq!(
        compose_quoted_reply(Some("error here"), "/plan"),
        "/plan\n> error here"
    );

    // File paths like /tmp/foo or /usr/bin/env MUST NOT be hoisted.
    assert_eq!(
        compose_quoted_reply(Some("error here"), "/tmp/foo/bar.txt check this"),
        "> error here\n\n/tmp/foo/bar.txt check this"
    );
    assert_eq!(
        compose_quoted_reply(Some("error here"), "/usr/bin/env bash"),
        "> error here\n\n/usr/bin/env bash"
    );
    assert_eq!(compose_quoted_reply(Some("quoted"), "   "), "");
}

#[test]
fn parse_quoted_message_and_source_match() {
    let composed = compose_quoted_reply(Some("First line\nSecond line"), "My reply");
    let parsed = parse_quoted_message(&composed).expect("should parse quote");
    assert_eq!(parsed.preview, "First line");
    assert_eq!(parsed.body, "My reply");
    assert!(quote_source_matches(
        "First   line\nSecond line extra",
        &parsed.source
    ));

    // Standalone blockquote with no blank line + body rejected.
    assert!(parse_quoted_message("> just a quote").is_none());
    assert!(parse_quoted_message("> quote\nno blank line").is_none());
}

#[test]
fn empty_composer_nav_state_machine_up_down_enter_delete_esc() {
    let mut st = ChatViewState::default();
    // Non-empty composer ignores Up
    assert_eq!(
        st.handle_empty_composer_nav_key("up", false, 3, false, false),
        MessageNavOutcome::Ignored
    );
    // Empty composer Up focuses newest (index 2)
    assert_eq!(
        st.handle_empty_composer_nav_key("up", true, 3, false, false),
        MessageNavOutcome::FocusMoved(Some(2))
    );
    // Enter opens menu
    assert_eq!(
        st.handle_empty_composer_nav_key("enter", true, 3, false, false),
        MessageNavOutcome::MenuOpened(2)
    );
    // Copy is the first action for every element, including tools.
    assert_eq!(
        st.handle_empty_composer_nav_key("enter", true, 3, true, false),
        MessageNavOutcome::ExecuteAction {
            message_idx: 2,
            action: MessageMenuAction::Copy
        }
    );
    // Delete on non-waiting ignored; on waiting cancels
    assert_eq!(
        st.handle_empty_composer_nav_key("delete", true, 3, false, false),
        MessageNavOutcome::Ignored
    );
    assert_eq!(
        st.handle_empty_composer_nav_key("delete", true, 3, false, true),
        MessageNavOutcome::CancelWaiting(2)
    );
    // Down from newest returns to composer
    assert_eq!(
        st.handle_empty_composer_nav_key("down", true, 3, false, false),
        MessageNavOutcome::ReturnedToComposer
    );
    // Esc dismisses quote banner when unfocused
    st.reply_quote = Some(ReplyQuote {
        message_id: "m1".into(),
        preview: "hi".into(),
        full_text: "hi".into(),
    });
    assert_eq!(
        st.handle_empty_composer_nav_key("escape", true, 3, false, false),
        MessageNavOutcome::DismissedQuote
    );
}

#[test]
fn reaction_format_parse_and_stream_hiding_folds_badges() {
    // Exact FNV-1a legacy test vectors
    assert_eq!(reaction_reference("hello"), 0xa430d84680aabd0b);
    assert_eq!(reaction_reference("msg_first"), 0x6d8e353d25c5fe23);
    assert_eq!(reaction_reference("msg_second"), 0x983056b4a6c7ffef);
    assert_eq!(reaction_reference("réponse-🙂"), 0xe9f223bf60c17fff);

    let wire =
        format_reaction_message(ReactionEmoji::Like, "msg_first", "Same first words").unwrap();
    assert_eq!(wire, "👍 to: Same first words [mp:6d8e353d25c5fe23]");
    let parsed = parse_reaction_message(&wire).unwrap();
    assert_eq!(parsed.emoji, ReactionEmoji::Like);
    assert_eq!(parsed.reference, Some(0x6d8e353d25c5fe23));

    // Spoken form parses with reference == None
    let spoken = parse_reaction_message("❤️ to: Great answer").unwrap();
    assert_eq!(spoken.emoji, ReactionEmoji::Heart);
    assert_eq!(spoken.reference, None);

    // Stream hiding folds badges onto target and hides reaction user message + lone ack
    let msgs = vec![
        ChatMessage {
            id: "msg_first".into(),
            role: MessageRole::Assistant,
            text: "Same first words here".into(),
            ..Default::default()
        },
        ChatMessage {
            id: "u_react".into(),
            role: MessageRole::User,
            text: wire,
            ..Default::default()
        },
        ChatMessage {
            id: "a_ack".into(),
            role: MessageRole::Assistant,
            text: "Glad that helped!".into(),
            ..Default::default()
        },
        ChatMessage {
            id: "u_heart".into(),
            role: MessageRole::User,
            text: "❤️ to: Same first words".into(),
            ..Default::default()
        },
    ];
    let visible = project_visible_messages(&msgs, &HashMap::new());
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].message.id, "msg_first");
    assert_eq!(
        visible[0].reactions,
        vec![(ReactionEmoji::Like, 1), (ReactionEmoji::Heart, 1)]
    );
}

#[test]
fn double_click_like_respects_drag_selection_threshold() {
    assert!(should_toggle_double_click_like(
        2,
        (10.0, 20.0),
        (13.0, 22.0),
        200
    ));
    assert!(should_toggle_double_click_like(
        2,
        (10.0, 20.0),
        (14.0, 24.0),
        500
    ));
    // Drag > 4px -> MUST NOT trigger
    assert!(!should_toggle_double_click_like(
        2,
        (10.0, 20.0),
        (15.0, 20.0),
        100
    ));
    // Too slow (> 500ms) -> MUST NOT trigger
    assert!(!should_toggle_double_click_like(
        2,
        (10.0, 20.0),
        (10.0, 20.0),
        501
    ));
    // Single click -> MUST NOT trigger
    assert!(!should_toggle_double_click_like(
        1,
        (10.0, 20.0),
        (10.0, 20.0),
        100
    ));
}

#[test]
fn attachment_classifier_enforces_10_image_10_file_and_size_caps() {
    assert!(is_supported_image_ext("/tmp/shot.PNG"));
    assert!(is_supported_image_ext("C:\\pics\\a.webp"));
    assert!(!is_supported_image_ext("C:\\dir.png\\noext"));
    assert!(!is_supported_image_ext("a.png.exe"));
    assert!(!is_supported_image_ext("a.png?v=1"));

    let mut items = Vec::new();
    for i in 0..12 {
        items.push(AttachmentCandidate {
            path: PathBuf::from(format!("img_{i}.png")),
            size_bytes: 1024,
        });
    }
    items.push(AttachmentCandidate {
        path: PathBuf::from("huge.jpg"),
        size_bytes: MAX_IMAGE_BYTES + 1,
    });
    for i in 0..11 {
        items.push(AttachmentCandidate {
            path: PathBuf::from(format!("doc_{i}.pdf")),
            size_bytes: 2048,
        });
    }
    items.push(AttachmentCandidate {
        path: PathBuf::from("huge.bin"),
        size_bytes: MAX_FILE_BYTES + 1,
    });

    let c = classify_and_validate_attachments(&items);
    assert_eq!(c.images.len(), MAX_IMAGE_ATTACHMENTS);
    assert_eq!(c.files.len(), MAX_FILE_ATTACHMENTS);
    assert_eq!(c.rejected_oversize.len(), 2);
    assert_eq!(c.rejected_too_many, 3);
}

#[test]
fn ssh_upload_command_sanitizes_hostile_names_and_uses_multibot_dir() {
    let cmd = build_ssh_upload_command("../sess $(pwn)", "..'evil' `rm -rf`.png", 4242);
    assert!(cmd.contains("$HOME/.multibot/uploads/"));
    assert!(!cmd.contains(".multiprompt"));
    assert!(!cmd.contains('"'), "must contain zero double quotes");
    assert!(cmd.contains("umask 077"));
    assert!(cmd.contains("mktemp -d"));
    assert!(cmd.contains("wc -c"));
    assert!(cmd.contains("ln -T"));
    assert!(cmd.contains("4242"));
    assert!(!cmd.contains("$(") || cmd.contains("$(mktemp") || cmd.contains("$(wc"));
    assert!(!cmd.contains("pwn)"));
    assert!(!cmd.contains("'evil'"));
    assert!(!cmd.contains('`'));
    assert!(cmd.contains("___evil___rm_-rf_.png"));
}

#[test]
fn single_space_caret_insertion_handles_edges_selection_and_unicode() {
    let r1 = plan_image_insert("", 0, 0, "shot.png").unwrap();
    assert_eq!((r1.text.as_str(), r1.caret_byte), ("shot.png ", 9));

    let r2 = plan_image_insert("look   here", 4, 4, "shot.png  ").unwrap();
    assert_eq!(
        (r2.text.as_str(), r2.caret_byte),
        ("look shot.png here", 14)
    );

    // Reversed selection
    let r3 = plan_image_insert("look old here", 8, 5, "shot.png").unwrap();
    assert_eq!(
        (r3.text.as_str(), r3.caret_byte),
        ("look shot.png here", 14)
    );

    // Preserves \r\n after caret
    let r4 = plan_image_insert("\r\nnext", 0, 0, "shot.png").unwrap();
    assert_eq!((r4.text.as_str(), r4.caret_byte), ("shot.png \r\nnext", 9));

    // Blank ref rejected
    assert!(plan_image_insert("hi", 2, 2, "   ").is_none());
}

#[test]
fn image_cache_path_uses_sha256_and_sanitized_ext() {
    let dir = PathBuf::from("/tmp/imgcache");
    let p = image_cache_path_in(&dir, "https://example.com/a.png", "WEBP");
    let name = p.file_name().unwrap().to_string_lossy();
    assert!(name.ends_with(".webp"));
    assert_eq!(name.len(), 64 + 1 + 4);

    let fallback = image_cache_path_in(&dir, "k", "../exe");
    assert!(fallback.to_string_lossy().ends_with(".png"));
}

#[test]
fn hidden_tool_only_messages_do_not_offset_find_quotes_or_navigation() {
    let tool = ChatMessage {
        id: "tool-only".into(),
        role: MessageRole::Assistant,
        blocks: vec![MessageBlock::ToolUse {
            call_id: "call".into(),
            name: "exec".into(),
            summary: "Run".into(),
            detail: "details".into(),
            status: ToolStatus::Completed,
            diff: None,
            output: None,
        }],
        ..Default::default()
    };
    let messages = vec![
        tool,
        ChatMessage {
            id: "answer".into(),
            role: MessageRole::Assistant,
            text: "Visible answer".into(),
            ..Default::default()
        },
    ];
    let empty = HashMap::new();
    for (show, expected) in [(false, 0), (true, 1)] {
        let visible = project_visible_messages_with_overrides(&messages, &empty, &empty, show);
        assert_eq!(visible.len(), expected + 1);
        let mut state = ChatViewState::default();
        state.update_find_query("Visible answer", &messages, show);
        assert_eq!(state.find_matches, vec![expected]);
        assert_eq!(
            find_visible_quote_target(&messages, &empty, &empty, Some("answer"), "", show),
            Some(expected)
        );
        assert_eq!(
            state.handle_empty_composer_nav_key("up", true, visible.len(), false, false),
            MessageNavOutcome::FocusMoved(Some(expected))
        );
    }
}

#[test]
fn chat_find_computes_matches_and_wraps_next_prev() {
    let msgs = vec![
        ChatMessage {
            id: "1".into(),
            text: "Hello Alpha".into(),
            ..Default::default()
        },
        ChatMessage {
            id: "2".into(),
            text: "Beta".into(),
            ..Default::default()
        },
        ChatMessage {
            id: "3".into(),
            text: "alpha again".into(),
            ..Default::default()
        },
    ];
    let mut st = ChatViewState::default();
    st.open_find(&msgs, true);
    st.update_find_query("ALPHA", &msgs, true);
    assert_eq!(st.find_matches, vec![0, 2]);
    assert_eq!(st.active_find_message_index(), Some(0));
    assert_eq!(st.find_next(), Some(2));
    assert_eq!(st.find_next(), Some(0));
    assert_eq!(st.find_prev(), Some(2));
    st.close_find();
    assert!(!st.find_open);
    assert!(st.find_matches.is_empty());
}

#[test]
fn double_g_requires_empty_composer_and_500ms_window() {
    let mut chord = NewestChord::default();
    // Empty composer -> first G arms, second G within 500ms fires
    assert!(!chord.on_char('G', "", 1000, true, false));
    assert!(chord.on_char('G', "G", 1499, true, false));
    // Third G must not immediately fire
    assert!(!chord.on_char('G', "G", 1650, true, false));

    // > 500ms does NOT fire
    assert!(!chord.on_char('G', "", 2000, true, false));
    assert!(!chord.on_char('G', "G", 2501, true, false));

    // Non-empty composer NEVER fires
    assert!(!chord.on_char('G', "hello", 3000, true, false));
    assert!(!chord.on_char('G', "helloG", 3100, true, false));
}

#[test]
fn font_zoom_clamps_10_to_28_and_resets_to_14() {
    let mut st = ChatViewState::default();
    assert_eq!(st.font_size_pt, 14);
    assert_eq!(st.zoom_font(100), 28);
    assert_eq!(st.zoom_font(-100), 10);
    assert_eq!(st.reset_font(), 14);
}

// ------------------------------------------------------------------------
// Headless GPUI Kit Integration Tests
// ------------------------------------------------------------------------

fn open_test_chat_window(
    cx: &mut TestAppContext,
) -> (gpui_kit::AnyWindowHandle, gpui_kit::Entity<MultibotView>) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        crate::theme::apply_multibot_dark_theme(cx);
        crate::bind_multibot_keys(cx);
    });
    let rt = Arc::new(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap(),
    );
    cx.update(|cx| {
        gpui_kit::open_window(WindowOptions::default(), cx, move |window, cx| {
            cx.new(|cx| MultibotView::new(rt, window, cx))
        })
        .unwrap()
    })
}

fn chat_scroll_messages() -> Vec<ChatMessage> {
    (0..40)
        .map(|i| ChatMessage {
            id: format!("scroll-{i}"),
            role: MessageRole::User,
            text: format!("Message {i}\nSecond line\nThird line"),
            timestamp_ms: Some(i * 100),
            ..Default::default()
        })
        .collect()
}

#[gpui_kit::test]
fn sending_prompt_adds_target_session_to_frequent(cx: &mut TestAppContext) {
    use multibot_core::workspaces::{ALL_WORKSPACES_ID, SessionInput, WorkspaceView};
    let (handle, view) = open_test_chat_window(cx);
    cx.update_window(handle, |_, _, cx| {
        view.update(cx, |v, cx| {
            let session = SessionSummary {
                id: "frequent-prompt".into(),
                agent: AgentKind::Claude,
                ..Default::default()
            };
            let key = canonical_session_key("target-host", session.agent, &session.id);
            v.dispatch_text_prompt_for_session(
                "target-host",
                session.clone(),
                "Hello".into(),
                None,
                cx,
            );
            let inputs = [SessionInput {
                key: key.clone(),
                host: "target-host",
                summary: &session,
            }];
            let layout = v
                .sidebar_state
                .workspace_store
                .compute_sidebar_layout_with_options(
                    ALL_WORKSPACES_ID,
                    WorkspaceView::Frequent,
                    &inputs,
                    false,
                    None,
                    now_unix_ms(),
                );
            assert_eq!(layout.flat_display_order, vec![key.clone()]);
            assert!(
                v.sidebar_state.workspace_store.session_meta[&key]
                    .last_input_ms
                    .is_some()
            );
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn chat_scroll_late_history_follow_send_and_jump(cx: &mut TestAppContext) {
    use multibot_proto::ServerEvent;
    let (handle, view) = open_test_chat_window(cx);
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.selection.selected_session = Some(SessionSummary {
                id: "scroll".into(),
                ..Default::default()
            });
            cx.notify();
        });
        window.render_frame(cx);
        view.update(cx, |v, cx| {
            v.handle_daemon_server_event(
                ServerEvent::History {
                    session_id: "scroll".into(),
                    messages: chat_scroll_messages(),
                    oldest_offset: None,
                    latest_offset: None,
                    has_more: false,
                    file_size: None,
                    file_mtime_ms: None,
                    probe_sha256: None,
                    todos: vec![],
                },
                cx,
            )
        });
        window.render_frame(cx);
        let scroll = view.read(cx).chat_state.scroll_handle.clone();
        assert!(scroll.max_offset().y > px(100.0));
        assert_eq!(scroll.offset().y, -scroll.max_offset().y);
        assert_eq!(scroll.children_count(), 40);
        assert_eq!(
            view.read(cx).chat_state.scroll.pending_scroll,
            PendingScrollTarget::None
        );

        let append = |id: &str| ServerEvent::HistoryAppend {
            session_id: "scroll".into(),
            messages: vec![ChatMessage {
                id: id.into(),
                role: MessageRole::Assistant,
                text: "New reply".into(),
                ..Default::default()
            }],
            latest_offset: None,
            working: None,
            todos: vec![],
            probe_sha256: None,
        };
        view.update(cx, |v, cx| {
            v.handle_daemon_server_event(append("reply-1"), cx)
        });
        window.render_frame(cx);
        assert_eq!(scroll.offset().y, -scroll.max_offset().y);

        view.update(cx, |_, cx| {
            scroll.set_offset(gpui_kit::point(px(0.0), px(-100.0)));
            cx.notify();
        });
        window.render_frame(cx);
        assert!(view.read(cx).chat_state.scroll.scrolled_up);
        let old_offset = scroll.offset();
        view.update(cx, |v, cx| {
            v.handle_daemon_server_event(append("reply-2"), cx)
        });
        window.render_frame(cx);
        assert_eq!(scroll.offset(), old_offset);
        assert_eq!(
            view.read(cx)
                .chat_state
                .scroll
                .new_messages_while_scrolled_up,
            1
        );
        window.click("chat-jump-to-bottom-btn", cx);
        window.render_frame(cx);
        assert_eq!(scroll.offset().y, -scroll.max_offset().y);
        assert_eq!(
            view.read(cx)
                .chat_state
                .scroll
                .new_messages_while_scrolled_up,
            0
        );

        view.update(cx, |v, cx| {
            scroll.set_offset(gpui_kit::point(px(0.0), px(-100.0)));
            v.chat_state.scroll.sync_position(200.0);
            v.chat_state.scroll.unread_divider_msg_idx = Some(10);
            v.dispatch_text_prompt("Sent message".into(), None, cx);
        });
        window.render_frame(cx);
        assert_eq!(scroll.offset().y, -scroll.max_offset().y);
        assert!(view.read(cx).chat_state.scroll.follow_bottom);
        assert_eq!(view.read(cx).chat_state.scroll.unread_divider_msg_idx, None);
        assert_eq!(
            view.read(cx).chat_state.scroll.pending_scroll,
            PendingScrollTarget::None
        );
        assert!(window.try_find("chat-unread-divider").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn chat_info_palette_title_copy_and_escape(cx: &mut TestAppContext) {
    let (handle, view) = open_test_chat_window(cx);
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.selection.selected_session = Some(SessionSummary {
                id: "info-session".into(),
                title: "Daily work".into(),
                cwd: Some("/srv/multibot".into()),
                model: Some("gpt-5.3-codex".into()),
                agent: AgentKind::Codex,
                effort: Some(EffortLevel::High),
                context_tokens: Some(42_000),
                context_limit: Some(200_000),
                ..Default::default()
            });
            v.connection.connected_host = Some("work-vps".into());
            cx.notify();
        });
        window.render_frame(cx);
        window.press("ctrl-k", cx);
        window.input("Chat info", cx);
        window.press("enter", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).active_modal, ActiveModal::ChatInfo);
        assert!(window.find("chat-info-panel").visible());
        let row_text = |id: &str| {
            gpui_kit::base::test_support::texts(window)
                .into_iter()
                .filter(|text| {
                    text.path()
                        .contains(&gpui_kit::ElementId::from(SharedString::from(
                            id.to_string(),
                        )))
                })
                .map(|text| text.text().to_string())
                .collect::<String>()
        };
        for (id, expected) in [
            ("chat-info-title", "Daily work"),
            ("chat-info-id", "info-session"),
            ("chat-info-cwd", "/srv/multibot"),
            ("chat-info-runtime", "Codex · work-vps"),
            ("chat-info-model", "gpt-5.3-codex"),
            ("chat-info-effort", "High"),
            ("chat-info-context", "42k / 200k (21%)"),
        ] {
            assert_eq!(row_text(id), expected);
        }
        window.click("copy-chat-cwd", cx);
        assert_eq!(
            cx.read_from_clipboard().and_then(|c| c.text()).as_deref(),
            Some("/srv/multibot")
        );
        window.press("escape", cx);
        window.render_frame(cx);
        assert!(window.try_find("chat-info-panel").is_none());
        window.click("header-title-pill", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).active_modal, ActiveModal::ChatInfo);
        assert!(window.find("chat-info-panel").visible());
        window.press("escape", cx);
        window.render_frame(cx);
        assert!(window.try_find("chat-info-panel").is_none());

        view.update(cx, |v, cx| {
            v.selection.selected_session.as_mut().unwrap().agent = AgentKind::Shell;
            cx.notify();
        });
        window.render_frame(cx);
        window.click("shell-title", cx);
        window.render_frame(cx);
        assert!(window.find("chat-info-panel").visible());
        window.click("close-chat-info", cx);
        window.render_frame(cx);
        assert!(window.try_find("chat-info-panel").is_none());
        window.click("shell-title", cx);
        window.render_frame(cx);
        window.click_at("chat-info-backdrop", gpui_kit::point(px(2.0), px(2.0)), cx);
        window.render_frame(cx);
        assert!(window.try_find("chat-info-panel").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn dictation_reference_controls_toggle_and_render(cx: &mut TestAppContext) {
    let (handle, view) = open_test_chat_window(cx);
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.selection.selected_session = Some(SessionSummary {
                id: "voice-ui".into(),
                ..Default::default()
            });
            v.settings.voice.api_key = "invalid key".into();
            cx.notify();
        });
        window.render_frame(cx);
        let mic = window.find("composer-voice").bounds();
        let send = window.find("send-prompt").bounds();
        assert_eq!(mic.size, gpui_kit::size(px(32.0), px(32.0)));
        assert_eq!(send.size, gpui_kit::size(px(32.0), px(32.0)));
        assert_eq!(window.find("send-prompt").label(), Some("Send"));
        for (bounds, color) in [(mic, 0x262626), (send, 0xA66335)] {
            assert!(
                window.painted_quads().into_iter().any(|q| {
                    q.bounds == bounds.scale(window.scale_factor())
                        && q.background == gpui_kit::Background::from(rgb(color))
                        && q.corner_radii.top_left >= q.bounds.size.height / 2.0
                }),
                "painted circular control in the reference color"
            );
        }
        assert!(window.try_find("dictation-status").is_none());
        window.click("composer-voice", cx);
        assert!(view.read(cx).notice.is_some());
        window.render_frame(cx);
        assert!(window.find("dictation-status").visible());
        view.update(cx, |v, cx| {
            v.begin_listening(window, cx);
            v.voice.started_at = Some(Instant::now() - std::time::Duration::from_secs(5));
            cx.notify();
        });
        window.render_frame(cx);
        assert!(window.find("dictation-stop").visible());
        assert!(window.find("dictation-status").bounds().size.width > px(32.0));
        let text: String = gpui_kit::base::test_support::texts(window)
            .into_iter()
            .filter(|text| {
                text.path()
                    .contains(&gpui_kit::ElementId::from("dictation-status"))
            })
            .map(|text| text.text().to_string())
            .collect();
        assert_eq!(text, "0:05·····");
        window.click("composer-voice", cx);
        assert_eq!(
            view.read(cx).voice.phase,
            multibot_core::voice::DictationPhase::Finishing
        );
        window.render_frame(cx);
        assert!(window.try_find("dictation-stop").is_none());
        window.press("escape", cx);
        window.render_frame(cx);
        assert!(window.try_find("dictation-status").is_none());
        view.update(cx, |v, cx| {
            v.selection.selected_session.as_mut().unwrap().working = true;
            cx.notify();
        });
        window.render_frame(cx);
        assert_eq!(window.find("send-prompt").label(), Some("Queue"));
        assert!(window.find("stop-agent").visible());
    })
    .unwrap();
}

#[gpui_kit::test]
fn pasted_and_dictated_text_follow_bottom_but_reading_history_does_not(cx: &mut TestAppContext) {
    let (handle, view) = open_test_chat_window(cx);
    for paste in [true, false] {
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.selection.selected_session = Some(SessionSummary {
                    id: "paste-scroll".into(),
                    ..Default::default()
                });
                v.history.messages = chat_scroll_messages();
                v.inputs.prompt_input.update(cx, |input, cx| {
                    input.set_value("", window, cx);
                    input.focus(window, cx);
                });
                cx.notify();
            });
            window.render_frame(cx);
            view.update(cx, |v, cx| {
                v.chat_state
                    .scroll_handle
                    .set_offset(gpui_kit::point(px(0.0), px(-100.0)));
                v.chat_state.scroll.sync_position(200.0);
                v.chat_state.scroll.appended(2);
                cx.notify();
            });
            window.render_frame(cx);
            assert!(window.try_find("chat-jump-to-bottom-btn").is_some());
            if paste {
                cx.write_to_clipboard(ClipboardItem::new_string("Pasted text".into()));
                window.press("ctrl-v", cx);
            } else {
                view.update(cx, |v, cx| {
                    v.inputs.prompt_input.update(cx, |input, cx| {
                        input.replace_all("Dictated text", window, cx)
                    })
                });
            }
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let scroll = view.read(cx).chat_state.scroll_handle.clone();
            assert_eq!(scroll.offset().y, -scroll.max_offset().y);
            assert!(window.try_find("chat-jump-to-bottom-btn").is_none());
            assert!(
                !view
                    .read(cx)
                    .inputs
                    .prompt_input
                    .read(cx)
                    .value()
                    .is_empty()
            );
            view.update(cx, |v, cx| {
                scroll.set_offset(gpui_kit::point(px(0.0), px(-100.0)));
                v.chat_state.scroll.sync_position(200.0);
                v.chat_state.scroll.appended(1);
                cx.notify();
            });
            window.render_frame(cx);
            assert_eq!(scroll.offset().y, px(-100.0));
            assert!(window.find("chat-jump-to-bottom-btn").visible());
        })
        .unwrap();
    }
}

#[gpui_kit::test]
fn composer_entry_and_send_follow_bottom_without_new_message_pill(cx: &mut TestAppContext) {
    use multibot_proto::ServerEvent;
    let (handle, view) = open_test_chat_window(cx);
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.selection.selected_session = Some(SessionSummary {
                id: "entry-scroll".into(),
                ..Default::default()
            });
            v.history.messages = chat_scroll_messages();
            cx.notify();
        });
        window.render_frame(cx);
        let scroll = view.read(cx).chat_state.scroll_handle.clone();
        view.update(cx, |v, cx| {
            scroll.set_offset(gpui_kit::point(px(0.0), px(-100.0)));
            v.chat_state.scroll.sync_position(200.0);
            v.chat_state.scroll.appended(2);
            v.chat_state.scroll.unread_divider_msg_idx = Some(10);
            v.inputs
                .prompt_input
                .update(cx, |input, cx| input.focus(window, cx));
            cx.notify();
        });
        window.render_frame(cx);
        assert!(window.try_find("chat-jump-to-bottom-btn").is_some());
        window.input("Hello", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let scroll = view.read(cx).chat_state.scroll_handle.clone();
        assert_eq!(scroll.offset().y, -scroll.max_offset().y);
        assert!(window.try_find("chat-jump-to-bottom-btn").is_none());
        assert!(window.try_find("chat-unread-divider").is_none());

        // Even a populated draft can be sent after deliberately scrolling back up.
        view.update(cx, |v, cx| {
            scroll.set_offset(gpui_kit::point(px(0.0), px(-100.0)));
            v.chat_state.scroll.sync_position(200.0);
            v.chat_state.scroll.appended(2);
            cx.notify();
        });
        window.render_frame(cx);
        window.click("send-prompt", cx);
        window.render_frame(cx);
        assert_eq!(scroll.offset().y, -scroll.max_offset().y);
        assert!(window.try_find("chat-jump-to-bottom-btn").is_none());
        for text in [
            "Streaming reply".to_string(),
            "Long streaming reply. ".repeat(400),
        ] {
            view.update(cx, |v, cx| {
                v.handle_daemon_server_event(
                    ServerEvent::HistoryAppend {
                        session_id: "entry-scroll".into(),
                        messages: vec![ChatMessage {
                            id: "stream".into(),
                            role: MessageRole::Assistant,
                            text,
                            ..Default::default()
                        }],
                        latest_offset: None,
                        working: Some(true),
                        todos: vec![],
                        probe_sha256: None,
                    },
                    cx,
                )
            });
            window.render_frame(cx);
            assert_eq!(scroll.offset().y, -scroll.max_offset().y);
            assert!(window.try_find("chat-jump-to-bottom-btn").is_none());
        }
    })
    .unwrap();
}

#[gpui_kit::test]
fn chat_scroll_first_visit_unread_tall_reply_aligns_new_divider_at_top(cx: &mut TestAppContext) {
    use multibot_core::cache::{CacheKey, CachedSession};
    use multibot_proto::ServerEvent;

    for cached in [false, true] {
        let (handle, view) = open_test_chat_window(cx);
        cx.update_window(handle, |_, window, cx| {
            let session = SessionSummary {
                id: format!("first-unread-{cached}"),
                agent: AgentKind::Claude,
                needs_attention: true,
                last_assistant_id: Some("reply-tail".into()),
                ..Default::default()
            };
            let mut messages = chat_scroll_messages();
            messages.extend([
                ChatMessage {
                    id: "tall-reply".into(),
                    role: MessageRole::Assistant,
                    text: "Start of the unread reply.\n\n".repeat(100),
                    ..Default::default()
                },
                ChatMessage {
                    id: "reply-tail".into(),
                    role: MessageRole::Assistant,
                    text: "End of the unread turn.".into(),
                    ..Default::default()
                },
            ]);
            view.update(cx, |v, cx| {
                if cached {
                    v.history.chat_cache.put(
                        CacheKey::new("local", session.agent, &session.id),
                        CachedSession {
                            messages: messages.clone(),
                            ..Default::default()
                        },
                    );
                }
                v.open_session_on_host("local".into(), session.clone(), false, cx);
            });
            window.render_frame(cx);
            if cached {
                assert_eq!(
                    view.read(cx).chat_state.scroll.unread_divider_msg_idx,
                    Some(40)
                );
                assert_eq!(
                    window.find("chat-unread-divider").bounds().top(),
                    view.read(cx).chat_state.scroll_handle.bounds().top(),
                );
            }
            view.update(cx, |v, cx| {
                v.handle_daemon_server_event(
                    ServerEvent::History {
                        session_id: session.id.clone(),
                        messages,
                        oldest_offset: None,
                        latest_offset: None,
                        has_more: false,
                        file_size: None,
                        file_mtime_ms: None,
                        probe_sha256: None,
                        todos: vec![],
                    },
                    cx,
                );
            });
            window.render_frame(cx);
            let scroll = view.read(cx).chat_state.scroll_handle.clone();
            assert_eq!(scroll.children_count(), 42);
            assert_eq!(
                view.read(cx).chat_state.scroll.unread_divider_msg_idx,
                Some(40)
            );
            let row = scroll.bounds_for_item(40).unwrap();
            assert!(row.size.height > scroll.bounds().size.height);
            assert_eq!(row.top() + scroll.offset().y, scroll.bounds().top());
            let divider = window.find("chat-unread-divider");
            assert!(divider.visible());
            assert_eq!(divider.bounds().top(), scroll.bounds().top());
            let bubble = window.find("msg-bubble-40").bounds();
            assert!(bubble.top() >= divider.bounds().bottom());
            assert!(bubble.top() < scroll.bounds().bottom());
            assert!(scroll.offset().y > -scroll.max_offset().y);
            assert_eq!(
                view.read(cx).chat_state.scroll.pending_scroll,
                PendingScrollTarget::None
            );
            window.render_frame(cx);
            assert_eq!(
                window.find("chat-unread-divider").bounds().top(),
                scroll.bounds().top()
            );

            window.click("chat-jump-to-bottom-btn", cx);
            window.render_frame(cx);
            assert_eq!(scroll.offset().y, -scroll.max_offset().y);
            assert_eq!(
                view.read(cx).chat_state.scroll.pending_scroll,
                PendingScrollTarget::None
            );

            view.update(cx, |v, cx| {
                v.open_session_on_host("local".into(), session, false, cx);
            });
            window.render_frame(cx);
            let scroll = &view.read(cx).chat_state.scroll_handle;
            assert_eq!(scroll.offset().y, -scroll.max_offset().y);
            assert_eq!(view.read(cx).chat_state.scroll.unread_divider_msg_idx, None);
            assert!(window.try_find("chat-unread-divider").is_none());
        })
        .unwrap();
    }
}

#[gpui_kit::test]
fn chat_scroll_unread_cache_and_older_page_keep_the_cutoff(cx: &mut TestAppContext) {
    use multibot_core::{
        cache::{CacheKey, CachedSession},
        workspaces::{UnreadBoundary, canonical_session_key},
    };
    use multibot_proto::ServerEvent;
    let (handle, view) = open_test_chat_window(cx);
    cx.update_window(handle, |_, window, cx| {
        let session = SessionSummary {
            id: "unread-scroll".into(),
            agent: AgentKind::Claude,
            needs_attention: true,
            last_assistant_id: Some("scroll-39".into()),
            ..Default::default()
        };
        view.update(cx, |v, cx| {
            let mut messages = chat_scroll_messages();
            messages[10].role = MessageRole::Assistant;
            messages[10].text = "Start of the returning unread reply.\n\n".repeat(100);
            v.history.chat_cache.put(
                CacheKey::new("local", session.agent, &session.id),
                CachedSession {
                    messages,
                    oldest_offset: 100,
                    consumed_offset: 200,
                    file_size: 200,
                    has_more: true,
                    ..Default::default()
                },
            );
            let key = canonical_session_key("local", session.agent, &session.id);
            v.sidebar_state.workspace_store.unread_boundaries.insert(
                key.clone(),
                UnreadBoundary {
                    last_read_assistant_id: Some("scroll-9".into()),
                    ..Default::default()
                },
            );
            v.open_session_on_host("local".into(), session.clone(), false, cx);
            assert_eq!(v.chat_state.scroll.unread_divider_msg_idx, Some(10));
            assert_eq!(
                v.sidebar_state.workspace_store.unread_boundaries[&key].last_read_assistant_id,
                session.last_assistant_id
            );
        });
        window.render_frame(cx);
        let scroll = view.read(cx).chat_state.scroll_handle.clone();
        let cutoff = scroll.bounds_for_item(10).unwrap();
        let cutoff_top = cutoff.top();
        assert!(cutoff.size.height > scroll.bounds().size.height);
        assert_eq!(cutoff_top + scroll.offset().y, scroll.bounds().top());
        assert!(window.find("chat-unread-divider").visible());
        assert_eq!(
            window.find("chat-unread-divider").bounds().top(),
            scroll.bounds().top()
        );
        assert!(view.read(cx).chat_state.scroll.scrolled_up);
        let previous_cutoff_screen_y = cutoff_top + scroll.offset().y;
        view.update(cx, |v, cx| {
            v.handle_daemon_server_event_with_hint(
                "local",
                ServerEvent::History {
                    session_id: session.id.clone(),
                    messages: vec![ChatMessage {
                        id: "older".into(),
                        role: MessageRole::User,
                        text: "Earlier message\nMore history".into(),
                        ..Default::default()
                    }],
                    oldest_offset: Some(0),
                    latest_offset: Some(100),
                    has_more: false,
                    file_size: Some(200),
                    file_mtime_ms: None,
                    probe_sha256: None,
                    todos: vec![],
                },
                true,
                cx,
            )
        });
        window.render_frame(cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).chat_state.scroll.unread_divider_msg_idx,
            Some(11)
        );
        assert_eq!(
            scroll.bounds_for_item(11).unwrap().top() + scroll.offset().y,
            previous_cutoff_screen_y
        );
        assert_eq!(scroll.children_count(), 41);
    })
    .unwrap();
}

#[gpui_kit::test]
fn chat_scroll_shift_gg_capture_preserves_typing_and_drafts(cx: &mut TestAppContext) {
    let (handle, view) = open_test_chat_window(cx);
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.history.messages = chat_scroll_messages();
            v.inputs.prompt_input.update(cx, |input, cx| {
                input.set_value("", window, cx);
                input.focus(window, cx);
            });
            v.chat_state.focused_message_idx = Some(0);
            cx.notify();
        });
        window.render_frame(cx);
        let scroll = view.read(cx).chat_state.scroll_handle.clone();
        view.update(cx, |v, cx| {
            scroll.set_offset(gpui_kit::point(px(0.0), px(-100.0)));
            v.chat_state.scroll.sync_position(200.0);
            cx.notify();
        });
        window.render_frame(cx);
        window.press("shift-g", cx);
        assert_eq!(
            view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
            "G"
        );
        window.press("shift-g", cx);
        window.render_frame(cx);
        assert_eq!(scroll.offset().y, -scroll.max_offset().y);
        assert_eq!(
            view.read(cx).chat_state.scroll.pending_scroll,
            PendingScrollTarget::None
        );
        assert_eq!(
            view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
            ""
        );
        assert_eq!(view.read(cx).chat_state.focused_message_idx, None);
        window.press("shift-g", cx);
        window.press("o", cx);
        window.press("o", cx);
        window.press("d", cx);
        assert_eq!(
            view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
            "Good"
        );
        view.update(cx, |v, cx| {
            v.focus_chat_panel(window, cx);
            v.chat_state.focused_message_idx = Some(0);
            cx.notify();
        });
        window.render_frame(cx);
        window.press("shift-g", cx);
        window.press("shift-g", cx);
        assert_eq!(view.read(cx).chat_state.focused_message_idx, None);
        assert_eq!(
            view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
            "Good"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn composer_options_clicks_preserve_draft_and_forward_send_queue_and_spawn(
    cx: &mut TestAppContext,
) {
    let (handle, view) = open_test_chat_window(cx);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let session = SessionSummary {
        id: "composer-controls".into(),
        agent: AgentKind::Claude,
        ..Default::default()
    };
    let expected = SessionOptions {
        model: Some("opus".into()),
        effort: Some(EffortLevel::High),
        permission: Some(PermissionLevel::Auto),
    };
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.connection.daemon_tx = Some(tx.clone());
            v.connection.sessions = vec![session.clone()];
            v.selection.selected_session = Some(session.clone());
            v.inputs.prompt_input.update(cx, |input, cx| input.set_value("my draft", window, cx));
            cx.notify();
        });
        window.render_frame(cx);
        assert!(window.try_find("composer-options").is_none());
        assert!(window.try_find("composer-options-summary").is_none());
        window.click("attach-file-btn", cx);
        window.render_frame(cx);
        assert!(window.find("composer-options").visible());
        for (id, default_id) in [
            ("composer-model-1", "composer-model-0"),
            ("composer-effort-3", "composer-effort-0"),
            ("composer-permission-2", "composer-permission-0"),
        ] {
            assert_eq!(window.find(default_id).checked(), Some(true));
            assert_eq!(window.find(id).checked(), Some(false));
            window.click(id, cx);
            window.render_frame(cx);
            assert_eq!(window.find(id).checked(), Some(true));
            assert_eq!(window.find(default_id).checked(), Some(false));
        }
        assert!(window.try_find("composer-options-summary").is_some());
        view.update(cx, |v, cx| {
            assert_eq!(v.active_composer_options(), expected);
            assert_eq!(v.inputs.prompt_input.read(cx).value().as_ref(), "my draft");
            assert!(!v.slash_picker_state.open);
        });
        window.click("attach-file-btn", cx);
        window.click("send-prompt", cx);
        assert!(matches!(rx.try_recv(), Ok(ClientRequest::SendPrompt { text, options: Some(ref o), .. }) if text == "my draft" && o == &expected));
        view.update(cx, |v, cx| {
            let mut working = session.clone();
            working.working = true;
            v.settings.queue_while_working = true;
            v.send_or_queue_prompt(&working, "q1".into(), "queued".into());
            assert!(rx.try_recv().is_err());
            assert_eq!(v.outbox.chat_outbox.entries.last().unwrap().options.as_ref(), Some(&expected));
            assert!(v.spawn_new_session(AgentKind::Claude, ".".into(), None, Some("new chat".into()), cx));
            assert!(matches!(rx.try_recv(), Ok(ClientRequest::SpawnSession { options: Some(ref o), .. }) if o == &expected));
            let spawned = SessionSummary { id: "new-composer-session".into(), agent: AgentKind::Claude, ..Default::default() };
            v.handle_daemon_server_event(multibot_proto::ServerEvent::SessionSpawned { session: spawned.clone() }, cx);
            assert_eq!(v.composer_options_for(&spawned), Some(expected.clone()));
            assert_eq!(v.spawn_composer_options(AgentKind::Codex), None, "Never send Claude model overrides to Codex");
        });
    }).unwrap();
}

#[gpui_kit::test]
fn composer_options_are_session_and_host_scoped_and_reset_to_defaults(cx: &mut TestAppContext) {
    let (handle, view) = open_test_chat_window(cx);
    let session = SessionSummary {
        id: "same-id".into(),
        agent: AgentKind::Codex,
        ..Default::default()
    };
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.selection.selected_session = Some(session.clone());
            v.connection.connected_host = Some("host-a".into());
            cx.notify();
        });
        window.render_frame(cx);
        window.click("attach-file-btn", cx);
        for id in [
            "composer-model-2",
            "composer-effort-4",
            "composer-permission-3",
        ] {
            window.click(id, cx);
        }
        let selected = SessionOptions {
            model: Some("gpt-5.3-codex".into()),
            effort: Some(EffortLevel::Max),
            permission: Some(PermissionLevel::FullAccess),
        };
        view.update(cx, |v, cx| {
            assert_eq!(v.active_composer_options(), selected);
            assert!(v.inputs.prompt_input.read(cx).value().is_empty());
            v.selection.selected_session.as_mut().unwrap().id = "another-session".into();
            assert!(v.active_composer_options().is_default());
            v.selection.selected_session = Some(session.clone());
            v.connection.connected_host = Some("host-b".into());
            assert!(v.active_composer_options().is_default());
            v.connection.connected_host = Some("host-a".into());
            assert_eq!(v.active_composer_options(), selected);
            cx.notify();
        });
        window.render_frame(cx);
        for id in [
            "composer-model-0",
            "composer-effort-0",
            "composer-permission-0",
        ] {
            window.click(id, cx);
        }
        window.render_frame(cx);
        assert!(window.try_find("composer-options-summary").is_none());
        view.update(cx, |v, cx| {
            assert_eq!(v.composer_options_for(&session), None);
            assert!(v.inputs.prompt_input.read(cx).value().is_empty());
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn composer_options_without_a_session_apply_to_new_claude_chats(cx: &mut TestAppContext) {
    let (handle, view) = open_test_chat_window(cx);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("attach-file-btn", cx);
        window.click("composer-model-2", cx);
        window.click("composer-effort-1", cx);
        window.click("composer-permission-1", cx);
        view.update(cx, |v, cx| {
            let expected = SessionOptions {
                model: Some("sonnet".into()),
                effort: Some(EffortLevel::Low),
                permission: Some(PermissionLevel::Ask),
            };
            assert_eq!(v.spawn_composer_options(AgentKind::Claude), Some(expected));
            assert_eq!(v.spawn_composer_options(AgentKind::Codex), None);
            assert_eq!(v.spawn_composer_options(AgentKind::Antigravity), None);
            assert!(v.inputs.prompt_input.read(cx).value().is_empty());
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn composer_options_offline_snapshot_and_unsupported_provider(cx: &mut TestAppContext) {
    let (handle, view) = open_test_chat_window(cx);
    let session = SessionSummary {
        id: "offline-controls".into(),
        agent: AgentKind::Claude,
        ..Default::default()
    };
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.selection.selected_session = Some(session.clone());
            v.connection.sessions = vec![session.clone()];
            v.connection.daemon_tx = None;
            v.outbox.persist_outbox_to_disk = false;
            v.outbox.shared_chat_outbox.lock().unwrap().persist_to_disk = false;
            v.update_composer_options(|o| o.model = Some("sonnet".into()), cx);
            v.send_or_queue_prompt(&session, "offline-message".into(), "offline draft".into());
            v.update_composer_options(|o| o.model = Some("opus".into()), cx);
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            v.connection.daemon_tx = Some(tx);
            assert_eq!(v.flush_outbox_for_current_host(), 1);
            assert!(matches!(rx.try_recv(), Ok(ClientRequest::SendPrompt { options: Some(SessionOptions { model: Some(ref m), .. }), .. }) if m == "sonnet"));
            v.selection.selected_session = Some(SessionSummary { id: "agy-controls".into(), agent: AgentKind::Antigravity, ..Default::default() });
            cx.notify();
        });
        window.render_frame(cx);
        window.click("attach-file-btn", cx);
        window.render_frame(cx);
        for id in ["composer-model-0", "composer-effort-1", "composer-permission-3"] {
            assert_eq!(window.find(id).disabled(), Some(true));
        }
        view.update(cx, |v, _| assert!(v.active_composer_options().is_default()));
    }).unwrap();
}

#[gpui_kit::test]
fn tool_calls_are_absent_until_ctrl_k_reveals_expandable_clusters(cx: &mut TestAppContext) {
    let (handle, view) = open_test_chat_window(cx);
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.selection.selected_session = Some(SessionSummary {
                id: "cluster-chat".into(),
                agent: AgentKind::Claude,
                ..Default::default()
            });
            let tool = |id: &str| MessageBlock::ToolUse {
                call_id: id.into(),
                name: "exec".into(),
                summary: "Run command".into(),
                detail: "secret tool details".into(),
                status: ToolStatus::Completed,
                diff: None,
                output: None,
            };
            v.history.messages = vec![
                ChatMessage {
                    id: "mixed".into(),
                    role: MessageRole::Assistant,
                    blocks: vec![
                        tool("c1"),
                        MessageBlock::Text {
                            text: "Answer stays visible".into(),
                        },
                    ],
                    ..Default::default()
                },
                ChatMessage {
                    id: "tool-only".into(),
                    role: MessageRole::Assistant,
                    blocks: vec![tool("c2"), MessageBlock::Text { text: "".into() }],
                    ..Default::default()
                },
            ];
            cx.notify();
        });
        window.render_frame(cx);
        assert!(window.try_find("msg-bubble-0").is_some());
        assert!(window.try_find("msg-bubble-1").is_none());
        for i in 0..2 {
            assert!(window.try_find(format!("tool-cluster-{i}")).is_none());
            assert!(window.try_find(format!("tool-row-{i}-0")).is_none());
        }
        for show in [true, false, true] {
            window.press("ctrl-k", cx);
            window.render_frame(cx);
            view.update(cx, |v, cx| {
                v.inputs
                    .palette_input
                    .update(cx, |input, cx| input.set_value("tool calls", window, cx));
            });
            window.render_frame(cx);
            window.press("enter", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).active_modal, ActiveModal::None);
            assert_eq!(window.try_find("tool-cluster-0").is_some(), show);
            assert_eq!(window.try_find("msg-bubble-1").is_some(), show);
            assert!(window.try_find("tool-row-0-0").is_none());
            if show {
                assert_eq!(window.find("tool-cluster-0").expanded(), Some(false));
                window.click("tool-cluster-0", cx);
                window.render_frame(cx);
                assert_eq!(window.find("tool-row-0-0").expanded(), Some(false));
                window.click("tool-row-0-0", cx);
                window.render_frame(cx);
                assert_eq!(window.find("tool-row-0-0").expanded(), Some(true));
            }
        }
        view.update(cx, |v, cx| {
            v.selection.selected_session.as_mut().unwrap().id = "other-chat".into();
            cx.notify();
        });
        window.render_frame(cx);
        assert!(window.try_find("tool-cluster-0").is_none());
        assert!(window.try_find("tool-row-0-0").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn chat_urls_open_on_regular_click_and_reject_unsafe_schemes(cx: &mut TestAppContext) {
    let (handle, view) = open_test_chat_window(cx);
    cx.update_window(handle, |_, window, cx| {
        crate::external_url::take_opened_urls();
        for role in [MessageRole::Assistant, MessageRole::User] {
            for with_blocks in [false, true] {
                for (text, expected) in [
                    (
                        "[Browser](https://example.com/a?x=1&y=2)",
                        Some("https://example.com/a?x=1&y=2"),
                    ),
                    (
                        "https://example.com/a_(b)).,;",
                        Some("https://example.com/a_(b)"),
                    ),
                    ("http://example.com/path.", Some("http://example.com/path")),
                    ("[Unsafe](javascript:alert%281%29)", None),
                    ("[Local](file:///etc/passwd)", None),
                ] {
                    view.update(cx, |v, cx| {
                        v.history.messages = vec![ChatMessage {
                            id: format!("url-{role:?}-{with_blocks}-{text}"),
                            role,
                            text: text.into(),
                            blocks: if with_blocks {
                                vec![MessageBlock::Text { text: text.into() }]
                            } else {
                                vec![]
                            },
                            ..Default::default()
                        }];
                        cx.notify();
                    });
                    window.render_frame(cx);
                    if expected.is_some() {
                        window.click_with_options(
                            "msg-bubble-0",
                            gpui_kit::test::ClickOptions::new()
                                .with_offset(gpui_kit::point(px(24.0), px(20.0)))
                                .with_button(MouseButton::Right),
                            cx,
                        );
                        assert!(crate::external_url::take_opened_urls().is_empty());
                        let bounds = window.find("msg-bubble-0").bounds();
                        let from = bounds.origin + gpui_kit::point(px(24.0), px(20.0));
                        window.drag(from, from + gpui_kit::point(px(30.0), px(0.0)), cx);
                        assert!(
                            crate::external_url::take_opened_urls().is_empty(),
                            "dragging selects without opening"
                        );
                    }
                    window.click_at("msg-bubble-0", gpui_kit::point(px(24.0), px(20.0)), cx);
                    assert_eq!(
                        crate::external_url::take_opened_urls(),
                        expected.into_iter().collect::<Vec<_>>(),
                        "{role:?}, blocks={with_blocks}, {text}"
                    );
                }
            }
        }
    })
    .unwrap();
}

#[gpui_kit::test]
fn renders_rich_chat_blocks_and_toggles_tools_diff_and_todo_drawer(cx: &mut TestAppContext) {
    let (handle, view) = open_test_chat_window(cx);
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.selection.selected_session = Some(SessionSummary {
                id: "sess-rich".into(),
                agent: AgentKind::Claude,
                title: "Rich Chat Test".into(),
                working: true,
                todos_done: 1,
                todos_total: 2,
                ..Default::default()
            });
            let react_wire =
                format_reaction_message(ReactionEmoji::Heart, "a1", "Implementing rich chat")
                    .unwrap();
            v.history.messages = vec![
                ChatMessage {
                    id: "a1".into(),
                    role: MessageRole::Assistant,
                    text: "Implementing rich chat".into(),
                    blocks: vec![
                        MessageBlock::Text {
                            text: "Implementing rich chat\n```rust\nfn answer() -> u32 { 42 }\n```"
                                .into(),
                        },
                        MessageBlock::ToolUse {
                            call_id: "call-1".into(),
                            name: "Edit".into(),
                            summary: "Edited chat_view.rs".into(),
                            detail: "applied patch".into(),
                            status: ToolStatus::Completed,
                            diff: Some(DiffBlock {
                                path: Some("src/chat_view.rs".into()),
                                added: 8,
                                removed: 2,
                                lines: (0..10).map(|i| format!("+added line {i}")).collect(),
                                truncated_lines: 0,
                            }),
                            output: Some("done".into()),
                        },
                    ],
                    todos: vec![
                        TodoItem {
                            content: "Step 1".into(),
                            status: TodoStatus::Completed,
                        },
                        TodoItem {
                            content: "Step 2".into(),
                            status: TodoStatus::InProgress,
                        },
                    ],
                    ..Default::default()
                },
                ChatMessage {
                    id: "u_react".into(),
                    role: MessageRole::User,
                    text: react_wire,
                    ..Default::default()
                },
                ChatMessage {
                    id: "u_fail".into(),
                    role: MessageRole::User,
                    text: "Failed message".into(),
                    ..Default::default()
                },
            ];
            v.record_send_failure(
                "u_fail".into(),
                "Failed message".into(),
                "Offline".into(),
                cx,
            );
        });

        window.render_frame(cx);
        assert!(window.try_find("chat-busy-dots").is_some());
        assert!(window.try_find("chat-todo-pill").is_some());
        assert!(window.try_find("chat-todo-drawer").is_none());
        assert!(window.try_find("retry-send-2").is_some());

        // Open todo drawer
        window.click("chat-todo-pill", cx);
        window.render_frame(cx);
        assert!(window.try_find("chat-todo-drawer").is_some());

        assert!(window.try_find("tool-cluster-0").is_none());
        view.update(cx, |v, cx| {
            assert!(v.toggle_all_tool_steps(cx));
        });
        window.render_frame(cx);
        assert!(window.try_find("tool-cluster-0").is_some());
        assert!(window.try_find("tool-row-0-1").is_none());
        window.click("tool-cluster-0", cx);
        window.render_frame(cx);
        assert!(window.try_find("tool-row-0-1").is_some());

        // Expand tool row -> diff toggle appears -> expand diff
        window.click("tool-row-0-1", cx);
        window.render_frame(cx);
        assert!(window.try_find("diff-toggle-0-1").is_some());
        window.click("diff-toggle-0-1", cx);
        window.render_frame(cx);

        // Fold all tool steps
        view.update(cx, |v, cx| {
            assert!(!v.toggle_all_tool_steps(cx));
        });
        window.render_frame(cx);
        assert!(window.try_find("diff-toggle-0-1").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn interactive_option_cards_and_copy_latest_code(cx: &mut TestAppContext) {
    let (handle, view) = open_test_chat_window(cx);
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.selection.selected_session = Some(SessionSummary {
                id: "sess-opt".into(),
                agent: AgentKind::Claude,
                title: "Options Test".into(),
                ..Default::default()
            });
            v.history.messages = vec![ChatMessage {
                id: "a-opt".into(),
                role: MessageRole::Assistant,
                text: "Pick an option".into(),
                blocks: vec![
                    MessageBlock::Text {
                        text: "```rust\nlet x = 7;\n```".into(),
                    },
                    MessageBlock::QuestionPrompt {
                        prompt_id: "prompt-42".into(),
                        provider: "claude".into(),
                        questions: vec![PromptQuestion {
                            question: "Choose mode".into(),
                            header: "Mode".into(),
                            multi_select: false,
                            options: vec![
                                PromptOption {
                                    label: "Fast".into(),
                                    description: "Quick run".into(),
                                },
                                PromptOption {
                                    label: "Deep".into(),
                                    description: "Full check".into(),
                                },
                            ],
                        }],
                    },
                ],
                ..Default::default()
            }];
            cx.notify();
        });

        window.render_frame(cx);
        assert!(window.try_find("opt-card-0-0").is_some());
        assert!(window.try_find("opt-other-0").is_some());

        // Copy latest code block strips fences
        let copied = view.update(cx, |v, cx| v.copy_latest_code_block(cx));
        assert!(copied);
        assert_eq!(
            cx.read_from_clipboard().and_then(|c| c.text()).as_deref(),
            Some("let x = 7;")
        );

        // Double-click message bubble padding without drag toggles 👍 reaction
        // (must NOT trigger option card inside bubble).
        window.click_with_options(
            "msg-bubble-0",
            gpui_kit::test::ClickOptions::new()
                .with_count(2)
                .with_offset(gpui_kit::point(px(10.0), px(10.0))),
            cx,
        );
        window.render_frame(cx);
        let has_like = view
            .read(cx)
            .chat_state
            .local_reactions
            .get("a-opt")
            .is_some_and(|r| r.contains(&ReactionEmoji::Like));
        assert!(has_like);
        assert!(
            !view
                .read(cx)
                .chat_state
                .answered_prompts
                .contains("prompt-42"),
            "double-clicking bubble padding must not answer option prompt"
        );

        // Clicking single-select option marks prompt answered
        window.click("opt-card-0-0", cx);
        window.render_frame(cx);
        let answered = view
            .read(cx)
            .chat_state
            .answered_prompts
            .contains("prompt-42");
        assert!(answered);
    })
    .unwrap();
}

#[test]
fn unicode_case_insensitive_find_excludes_hidden_reactions_and_aligns_visible_indices() {
    let react_wire = format_reaction_message(ReactionEmoji::Like, "m0", "Café au lait").unwrap();
    let msgs = vec![
        ChatMessage {
            id: "m0".into(),
            role: MessageRole::Assistant,
            text: "Café au lait".into(),
            ..Default::default()
        },
        // Hidden reaction + hidden lone assistant ack between m0 and m3
        ChatMessage {
            id: "u_react".into(),
            role: MessageRole::User,
            text: react_wire,
            ..Default::default()
        },
        ChatMessage {
            id: "a_ack".into(),
            role: MessageRole::Assistant,
            text: "Glad Café helped!".into(),
            ..Default::default()
        },
        ChatMessage {
            id: "m1".into(),
            role: MessageRole::User,
            text: "Über alles".into(),
            ..Default::default()
        },
        ChatMessage {
            id: "m2".into(),
            role: MessageRole::Assistant,
            text: "Москва".into(),
            ..Default::default()
        },
        ChatMessage {
            id: "m3".into(),
            role: MessageRole::Assistant,
            text: "Ελλάδα".into(),
            ..Default::default()
        },
    ];

    let mut st = ChatViewState::default();
    st.open_find(&msgs, true);
    for (query, expected_vis_idx) in [
        ("CAFÉ", 0usize),
        ("über", 1usize),
        ("москва", 2usize),
        ("ελλάδα", 3usize),
    ] {
        st.update_find_query(query, &msgs, true);
        assert_eq!(
            st.find_matches,
            vec![expected_vis_idx],
            "query {query:?} must match visible index {expected_vis_idx} (excluding hidden reaction & ack)"
        );
    }

    // Quote-jump resolves to visible index (1, NOT raw index 3)
    let target = find_visible_quote_target(
        &msgs,
        &HashMap::new(),
        &HashMap::new(),
        Some("m1"),
        "Über alles",
        true,
    );
    assert_eq!(target, Some(1));
}

#[test]
fn optimistic_local_reactions_survive_daemon_echo_without_canceling() {
    let wire = format_reaction_message(ReactionEmoji::Like, "m1", "Great answer").unwrap();
    let msgs = vec![
        ChatMessage {
            id: "m1".into(),
            role: MessageRole::Assistant,
            text: "Great answer".into(),
            ..Default::default()
        },
        ChatMessage {
            id: "u_echo".into(),
            role: MessageRole::User,
            text: wire,
            ..Default::default()
        },
    ];
    let mut local = HashMap::new();
    local.insert("m1".into(), vec![ReactionEmoji::Like]);
    let vis = project_visible_messages(&msgs, &local);
    assert_eq!(vis.len(), 1);
    assert_eq!(vis[0].reactions, vec![(ReactionEmoji::Like, 1)]);

    // Reconciling clears the confirmed optimistic entry while keeping server badge intact.
    let mut st = ChatViewState::default();
    st.local_reactions = local;
    st.reconcile_local_reactions(&msgs);
    assert!(st.local_reactions.is_empty());
    let vis_after = project_visible_messages(&msgs, &st.local_reactions);
    assert_eq!(vis_after[0].reactions, vec![(ReactionEmoji::Like, 1)]);
}

#[test]
fn reactions_and_quote_match_work_on_structured_message_block_text() {
    let block_msg = ChatMessage {
        id: "b1".into(),
        role: MessageRole::Assistant,
        text: String::new(),
        blocks: vec![MessageBlock::Text {
            text: "Answer inside block".into(),
        }],
        ..Default::default()
    };
    assert_eq!(message_visible_text(&block_msg), "Answer inside block");

    let msgs = vec![
        block_msg,
        ChatMessage {
            id: "u1".into(),
            role: MessageRole::User,
            text: format_reaction_message(ReactionEmoji::Heart, "b1", "Answer inside block")
                .unwrap(),
            ..Default::default()
        },
    ];
    let vis = project_visible_messages(&msgs, &HashMap::new());
    assert_eq!(vis.len(), 1);
    assert_eq!(vis[0].reactions, vec![(ReactionEmoji::Heart, 1)]);

    // Quote-source jump also matches structured MessageBlock::Text
    let q_target = find_visible_quote_target(
        &msgs,
        &HashMap::new(),
        &HashMap::new(),
        None,
        "Answer inside block",
        true,
    );
    assert_eq!(q_target, Some(0));
}

#[test]
fn rejects_spoofed_zero_mp_tag_and_matches_spoken_reactions_newest_first_prefix_only() {
    // Explicit [mp:0000000000000000] and malformed [mp:...] tags MUST return None
    assert!(parse_reaction_message("👍 to: Hello [mp:0000000000000000]").is_none());
    assert!(parse_reaction_message("👍 to: Hello [mp:nothex]").is_none());
    assert!(parse_reaction_message("👍 to: Hello [mp:1234]").is_none());

    // Spoofed zero tag is NOT hidden and does NOT attach to unrelated message
    let spoof_msgs = vec![
        ChatMessage {
            id: "real_msg_id".into(),
            role: MessageRole::Assistant,
            text: "Hello world".into(),
            ..Default::default()
        },
        ChatMessage {
            id: "u_spoof".into(),
            role: MessageRole::User,
            text: "👍 to: Hello [mp:0000000000000000]".into(),
            ..Default::default()
        },
    ];
    let spoof_vis = project_visible_messages(&spoof_msgs, &HashMap::new());
    assert_eq!(spoof_vis.len(), 2);
    assert!(spoof_vis[0].reactions.is_empty());

    // Untagged spoken reaction searches newest-first by prefix, never mid-sentence .contains()
    let spoken_msgs = vec![
        ChatMessage {
            id: "old_mid".into(),
            role: MessageRole::Assistant,
            text: "We saw the quick fox".into(),
            ..Default::default()
        },
        ChatMessage {
            id: "older_prefix".into(),
            role: MessageRole::Assistant,
            text: "the older match".into(),
            ..Default::default()
        },
        ChatMessage {
            id: "newest_prefix".into(),
            role: MessageRole::Assistant,
            text: "the".into(),
            ..Default::default()
        },
        ChatMessage {
            id: "u_spoken".into(),
            role: MessageRole::User,
            text: "👍 to: the".into(),
            ..Default::default()
        },
    ];
    let vis = project_visible_messages(&spoken_msgs, &HashMap::new());
    assert_eq!(vis.len(), 3);
    assert!(vis[0].reactions.is_empty());
    assert!(vis[1].reactions.is_empty());
    assert_eq!(vis[2].message.id, "newest_prefix");
    assert_eq!(vis[2].reactions, vec![(ReactionEmoji::Like, 1)]);
}

#[test]
fn double_click_rejects_triple_click_cross_bubble_and_click_drift() {
    // Triple-click MUST NOT toggle 👍
    assert!(!should_toggle_double_click_like(
        3,
        (0.0, 0.0),
        (0.0, 0.0),
        100
    ));
    assert!(!should_toggle_double_click_like(
        4,
        (0.0, 0.0),
        (0.0, 0.0),
        100
    ));

    // Valid same-bubble double-click within 4px and 500ms
    assert!(should_toggle_double_click_on_bubble(
        2,
        "m1",
        Some(("m1", 10.0, 10.0)),
        Some(("m1", 11.0, 11.0, 180)),
        (12.0, 12.0),
    ));
    // Missing prev_click -> rejected (never treated as 0ms!)
    assert!(!should_toggle_double_click_on_bubble(
        2,
        "m1",
        Some(("m1", 10.0, 10.0)),
        None,
        (10.0, 10.0),
    ));
    // Previous click on DIFFERENT bubble -> rejected
    assert!(!should_toggle_double_click_on_bubble(
        2,
        "m2",
        Some(("m2", 10.0, 10.0)),
        Some(("m1", 10.0, 10.0, 50)),
        (10.0, 10.0),
    ));
    // Click-to-click drift > 4px -> rejected even if second mousedown->mouseup was 0px
    assert!(!should_toggle_double_click_on_bubble(
        2,
        "m1",
        Some(("m1", 30.0, 10.0)),
        Some(("m1", 10.0, 10.0, 100)),
        (30.0, 10.0),
    ));
}

#[test]
fn zoom_font_saturates_extreme_deltas_and_image_helpers_handle_spaces_and_query_urls() {
    let mut st = ChatViewState::default();
    assert_eq!(st.zoom_font(i16::MAX), MAX_CHAT_FONT_SIZE_PT);
    assert_eq!(st.zoom_font(i16::MIN), MIN_CHAT_FONT_SIZE_PT);

    // plan_image_insert trims BOTH leading and trailing spaces
    let ins = plan_image_insert("hi", 2, 2, "  img.png").unwrap();
    assert_eq!(ins.text, "hi img.png ");
    assert_eq!(ins.caret_byte, 11);

    // image_ext_from_url strips ?query and #fragment
    assert_eq!(image_ext_from_url("https://host/img.webp?sig=abc"), "webp");
    assert_eq!(
        image_ext_from_url("https://host/img.jpeg?x=1#section"),
        "jpeg"
    );
    assert_eq!(image_ext_from_url("https://host.com/noext?a=1.jpg"), "png");
}

#[test]
fn newest_chord_reset_disarms_on_intervening_key() {
    let mut chord = NewestChord::default();
    assert!(!chord.on_char('G', "", 1000, true, false));
    chord.reset();
    // Even if composer is back to "G" within 500ms, reset disarmed the chord.
    assert!(!chord.on_char('G', "G", 1100, true, false));
}

#[gpui_kit::test]
fn multiple_question_prompts_have_unique_ids_and_multi_question_stays_interactive(
    cx: &mut TestAppContext,
) {
    let (handle, view) = open_test_chat_window(cx);
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.selection.selected_session = Some(SessionSummary {
                id: "sess-multi-q".into(),
                agent: AgentKind::Claude,
                title: "Multi Question Test".into(),
                ..Default::default()
            });
            v.history.messages = vec![
                // Historical QuestionPrompt message
                ChatMessage {
                    id: "a-old".into(),
                    role: MessageRole::Assistant,
                    text: "Old question".into(),
                    blocks: vec![MessageBlock::QuestionPrompt {
                        prompt_id: "prompt-old".into(),
                        provider: "claude".into(),
                        questions: vec![PromptQuestion {
                            question: "Old Q".into(),
                            header: "Old".into(),
                            multi_select: false,
                            options: vec![
                                PromptOption {
                                    label: "Old A".into(),
                                    description: "".into(),
                                },
                                PromptOption {
                                    label: "Old B".into(),
                                    description: "".into(),
                                },
                            ],
                        }],
                    }],
                    ..Default::default()
                },
                // Latest multi-question QuestionPrompt message (Q0 and Q1)
                ChatMessage {
                    id: "a-new".into(),
                    role: MessageRole::Assistant,
                    text: "Two questions".into(),
                    blocks: vec![MessageBlock::QuestionPrompt {
                        prompt_id: "prompt-new".into(),
                        provider: "claude".into(),
                        questions: vec![
                            PromptQuestion {
                                question: "Question 1".into(),
                                header: "Q1".into(),
                                multi_select: false,
                                options: vec![
                                    PromptOption {
                                        label: "Alpha".into(),
                                        description: "".into(),
                                    },
                                    PromptOption {
                                        label: "Beta".into(),
                                        description: "".into(),
                                    },
                                ],
                            },
                            PromptQuestion {
                                question: "Question 2".into(),
                                header: "Q2".into(),
                                multi_select: false,
                                options: vec![
                                    PromptOption {
                                        label: "Gamma".into(),
                                        description: "".into(),
                                    },
                                    PromptOption {
                                        label: "Delta".into(),
                                        description: "".into(),
                                    },
                                ],
                            },
                        ],
                    }],
                    ..Default::default()
                },
            ];
            cx.notify();
        });

        window.render_frame(cx);
        // Zero ambiguous ElementId: historical uses opt-card-hist-0-0-0-0, latest uses opt-card-0-0
        assert!(window.try_find("opt-card-hist-0-0-0-0").is_some());
        assert!(window.try_find("opt-card-0-0").is_some());
        assert!(window.try_find("opt-other-0").is_some());
        assert!(window.try_find("opt-card-1-0").is_some());
        assert!(window.try_find("opt-other-1").is_some());

        // Answering Q0 locks ONLY Q0; Q1 remains interactive!
        window.click("opt-card-0-0", cx);
        window.render_frame(cx);
        assert!(
            !view
                .read(cx)
                .chat_state
                .answered_prompts
                .contains("prompt-new"),
            "prompt-new must not be fully answered until Q1 is answered"
        );
        assert!(window.try_find("opt-other-0").is_none());
        assert!(window.try_find("opt-other-1").is_some());
        assert!(window.try_find("opt-card-1-0").is_some());

        // Answering Q1 completes prompt-new
        window.click("opt-card-1-0", cx);
        window.render_frame(cx);
        assert!(
            view.read(cx)
                .chat_state
                .answered_prompts
                .contains("prompt-new")
        );
        assert!(window.try_find("opt-other-1").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn mid_sentence_slash_and_codex_dollar_triggers_open_slash_picker(cx: &mut TestAppContext) {
    let (handle, view) = open_test_chat_window(cx);
    cx.update_window(handle, |_, window, cx| {
        // 1. Codex mid-sentence `$` trigger: "run $ve" + 'r' -> picker opens
        view.update(cx, |v, cx| {
            v.selection.selected_session = Some(SessionSummary {
                id: "sess-codex".into(),
                agent: AgentKind::Codex,
                title: "Codex".into(),
                ..Default::default()
            });
            v.inputs.prompt_input.update(cx, |input, cx| {
                input.set_value("run $ve", window, cx);
                input.focus(window, cx);
            });
            cx.notify();
        });
        window.render_frame(cx);
        window.press("r", cx);
        window.render_frame(cx);
        assert!(
            view.read(cx).slash_picker_state.is_open(),
            "mid-sentence Codex $ trigger must open slash picker"
        );

        // 2. Claude mid-sentence `/` trigger: "please /p" + 'l' -> picker opens
        view.update(cx, |v, cx| {
            v.slash_picker_state.close();
            v.selection.selected_session = Some(SessionSummary {
                id: "sess-claude".into(),
                agent: AgentKind::Claude,
                title: "Claude".into(),
                ..Default::default()
            });
            v.inputs.prompt_input.update(cx, |input, cx| {
                input.set_value("please /p", window, cx);
                input.focus(window, cx);
            });
            cx.notify();
        });
        window.render_frame(cx);
        window.press("l", cx);
        window.render_frame(cx);
        assert!(
            view.read(cx).slash_picker_state.is_open(),
            "mid-sentence Claude / trigger must open slash picker"
        );
    })
    .unwrap();
}

#[test]
fn plan_image_insert_trims_all_whitespace_and_rejects_control_ws_only() {
    assert_eq!(plan_image_insert("hi", 2, 2, "\t\r\n"), None);
    assert_eq!(plan_image_insert("hi", 2, 2, " \t\r\n "), None);

    let ssh_stdout = plan_image_insert("hi", 2, 2, "/home/u/.multibot/uploads/s/pic.png\n")
        .expect("ssh stdout with trailing newline must trim cleanly");
    assert_eq!(ssh_stdout.text, "hi /home/u/.multibot/uploads/s/pic.png ");
    assert_eq!(ssh_stdout.caret_byte, ssh_stdout.text.len());

    let padded = plan_image_insert("hi", 2, 2, " \t img.png \r\n ")
        .expect("tabs/newlines around image ref must trim");
    assert_eq!(padded.text, "hi img.png ");
    assert_eq!(padded.caret_byte, 11);
}

#[test]
fn all_three_reaction_emojis_toggle_on_and_off_cleanly_in_server_masks() {
    for emoji in ReactionEmoji::all() {
        let wire = format_reaction_message(emoji, "m1", "Great answer").unwrap();
        let msgs_on = vec![
            ChatMessage {
                id: "m1".into(),
                role: MessageRole::Assistant,
                text: "Great answer".into(),
                ..Default::default()
            },
            ChatMessage {
                id: "u1".into(),
                role: MessageRole::User,
                text: wire.clone(),
                ..Default::default()
            },
        ];
        let vis_on = project_visible_messages(&msgs_on, &HashMap::new());
        assert_eq!(vis_on.len(), 1);
        assert_eq!(vis_on[0].reactions, vec![(emoji, 1)]);

        let mut msgs_off = msgs_on;
        msgs_off.push(ChatMessage {
            id: "u2".into(),
            role: MessageRole::User,
            text: wire,
            ..Default::default()
        });
        let vis_off = project_visible_messages(&msgs_off, &HashMap::new());
        assert_eq!(vis_off.len(), 1);
        assert!(
            vis_off[0].reactions.is_empty(),
            "second {emoji:?} reaction must toggle off server badge"
        );
    }
}

#[test]
fn trailing_hidden_reaction_preserves_prose_options_while_real_user_reply_clears_them() {
    let assistant_text = "Pick one:\nA) Keep\nB) Drop";
    let reaction_wire = format_reaction_message(ReactionEmoji::Like, "a1", assistant_text).unwrap();
    let with_reaction = vec![
        ChatMessage {
            id: "a1".into(),
            role: MessageRole::Assistant,
            text: assistant_text.into(),
            ..Default::default()
        },
        ChatMessage {
            id: "u_react".into(),
            role: MessageRole::User,
            text: reaction_wire.clone(),
            ..Default::default()
        },
    ];
    let opts = detect_trailing_prose_options(&with_reaction)
        .expect("trailing hidden reaction must not erase prose options");
    assert_eq!(opts.len(), 2);
    assert_eq!(
        (opts[0].label.as_str(), opts[0].title.as_str()),
        ("A", "Keep")
    );
    assert_eq!(
        (opts[1].label.as_str(), opts[1].title.as_str()),
        ("B", "Drop")
    );

    // Even with a hidden lone assistant ack after the reaction, prose options remain active.
    let mut with_hidden_ack = with_reaction.clone();
    with_hidden_ack.push(ChatMessage {
        id: "a_ack".into(),
        role: MessageRole::Assistant,
        text: "Noted!".into(),
        ..Default::default()
    });
    assert!(detect_trailing_prose_options(&with_hidden_ack).is_some());

    // A real non-reaction user reply at the tail still clears prose options.
    let mut with_real_reply = with_reaction;
    with_real_reply.push(ChatMessage {
        id: "u_real".into(),
        role: MessageRole::User,
        text: "Let's go with A".into(),
        ..Default::default()
    });
    assert!(detect_trailing_prose_options(&with_real_reply).is_none());
}

#[gpui_kit::test]
fn session_mentions_picker_binds_duplicate_names_and_sends_exact_id(cx: &mut TestAppContext) {
    let (handle, view) = open_test_chat_window(cx);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            let sender = SessionSummary {
                id: "sender".into(),
                title: "Sender".into(),
                ..Default::default()
            };
            v.connection.sessions = vec![
                sender.clone(),
                SessionSummary {
                    id: "dup-a".into(),
                    title: "Same Name".into(),
                    ..Default::default()
                },
                SessionSummary {
                    id: "dup-b".into(),
                    title: "Same Name".into(),
                    ..Default::default()
                },
            ];
            v.selection.selected_session = Some(sender);
            v.connection.daemon_tx = Some(tx);
            v.inputs
                .prompt_input
                .update(cx, |input, cx| input.focus(window, cx));
            cx.notify();
        });
        window.render_frame(cx);
        window.press("@", cx);
        window.render_frame(cx);
        assert!(window.find("mention-picker").visible());
        assert_eq!(view.read(cx).mention_picker_state.filtered.len(), 3);
        window.press("s", cx);
        window.press("a", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).mention_picker_state.filtered.len(), 2);
        window.press("down", cx);
        window.press("tab", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
            "@Same Name "
        );
        assert_eq!(view.read(cx).inputs.prompt_input.read(cx).tokens().len(), 1);
        assert_eq!(
            view.read(cx).bound_mention_prompt(cx).trim(),
            "@Same Name{dup-b}"
        );
        // Undo/redo must preserve the target, not guess from the duplicate display name.
        window.press("ctrl-z", cx);
        window.press("ctrl-y", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).bound_mention_prompt(cx).trim(),
            "@Same Name{dup-b}"
        );
        window.press("ctrl-enter", cx);
        window.render_frame(cx);
        let mut saw_prompt = false;
        let mut saw_delivery = false;
        while let Ok(request) = rx.try_recv() {
            match request {
                ClientRequest::SendPrompt { text, .. } => {
                    assert_eq!(text, "@Same Name{dup-b}");
                    saw_prompt = true;
                }
                ClientRequest::SendCrossSession {
                    target_session_id, ..
                } => {
                    assert_eq!(target_session_id, "dup-b");
                    saw_delivery = true;
                }
                _ => {}
            }
        }
        assert!(saw_prompt && saw_delivery);
        window.press("@", cx);
        window.render_frame(cx);
        assert!(view.read(cx).mention_picker_state.is_open());
        window.press("ctrl-f", cx);
        window.render_frame(cx);
        assert!(view.read(cx).chat_state.find_open);
        assert!(!view.read(cx).mention_picker_state.is_open());
        window.press("escape", cx);
        window.press("ctrl-/", cx);
        window.render_frame(cx);
        assert!(view.read(cx).slash_picker_state.browse_mode);
        assert!(!view.read(cx).mention_picker_state.is_open());
    })
    .unwrap();
}

#[gpui_kit::test]
fn session_mentions_escape_keeps_text_and_click_jumps_exact_host_session(cx: &mut TestAppContext) {
    let (handle, view) = open_test_chat_window(cx);
    cx.update_window(handle, |_, window, cx| {
        let source = SessionSummary {
            id: "source".into(),
            title: "Source".into(),
            ..Default::default()
        };
        let first = SessionSummary {
            id: "first".into(),
            title: "Same".into(),
            ..Default::default()
        };
        let second = SessionSummary {
            id: "second".into(),
            title: "Same".into(),
            ..Default::default()
        };
        view.update(cx, |v, cx| {
            v.connection.host_sessions = vec![
                ("host-a".into(), source.clone()),
                ("host-a".into(), first.clone()),
                ("host-b".into(), second.clone()),
            ];
            v.selection.selected_session = Some(source.clone());
            v.selection.selected_session_host = Some("host-a".into());
            v.inputs.prompt_input.update(cx, |input, cx| {
                input.set_value("@", window, cx);
                input.focus(window, cx);
            });
            cx.notify();
        });
        window.render_frame(cx);
        window.press("escape", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
            "@"
        );
        assert!(!view.read(cx).mention_picker_state.is_open());
        for role in [MessageRole::User, MessageRole::Assistant] {
            for with_blocks in [false, true] {
                view.update(cx, |v, cx| {
                    v.selection.selected_session = Some(source.clone());
                    v.selection.selected_session_host = Some("host-a".into());
                    let text = "@Same{second}";
                    v.history.messages = vec![ChatMessage {
                        id: "mention-link".into(),
                        role,
                        text: text.into(),
                        blocks: if with_blocks {
                            vec![MessageBlock::Text { text: text.into() }]
                        } else {
                            vec![]
                        },
                        ..Default::default()
                    }];
                    cx.notify();
                });
                window.render_frame(cx);
                window.click_at("msg-bubble-0", gpui_kit::point(px(24.0), px(20.0)), cx);
                assert_eq!(
                    view.read(cx)
                        .selection
                        .selected_session
                        .as_ref()
                        .unwrap()
                        .id,
                    "second",
                    "{role:?}, blocks={with_blocks}"
                );
                assert_eq!(view.read(cx).active_session_host(), "host-b");
            }
        }
    })
    .unwrap();
}

#[gpui_kit::test]
fn live_slash_picker_zero_lag_sync_tab_enter_replace_and_browse_mode(cx: &mut TestAppContext) {
    let (handle, view) = open_test_chat_window(cx);
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.selection.selected_session = Some(SessionSummary {
                id: "sess-slash-live".into(),
                agent: AgentKind::Claude,
                title: "Claude".into(),
                ..Default::default()
            });
            v.inputs.prompt_input.update(cx, |input, cx| {
                input.set_value("", window, cx);
                input.focus(window, cx);
            });
            cx.notify();
        });
        window.render_frame(cx);

        // 1. Typing "/" immediately opens picker on frame 0 (zero 1-keystroke lag)
        window.press("/", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
            "/"
        );
        assert!(
            view.read(cx).slash_picker_state.is_open(),
            "typing '/' must open slash picker immediately"
        );
        assert_eq!(
            view.read(cx).slash_picker_state.trigger.as_ref().map(|t| (
                t.start_byte,
                t.end_byte,
                t.query.as_str()
            )),
            Some((0, 1, ""))
        );

        // 2. Typing "p", "l" immediately filters on "pl" with top item "plan" (NOT "permissions")
        window.press("p", cx);
        window.render_frame(cx);
        window.press("l", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
            "/pl"
        );
        assert_eq!(
            view.read(cx).slash_picker_state.trigger.as_ref().map(|t| (
                t.start_byte,
                t.end_byte,
                t.query.as_str()
            )),
            Some((0, 3, "pl"))
        );
        assert_eq!(
            view.read(cx)
                .slash_picker_state
                .filtered
                .first()
                .map(|i| i.name.as_str()),
            Some("plan")
        );

        // 3. Pressing Enter replaces "/pl" with "/plan " (NOT "/pl /plan "), closes picker, does NOT send
        window.press("enter", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
            "/plan "
        );
        assert!(!view.read(cx).slash_picker_state.is_open());
        assert!(view.read(cx).history.messages.is_empty());

        // 4. Typing "/pl" then pressing Tab replaces "/pl" with "/plan "
        view.update(cx, |v, cx| {
            v.inputs.prompt_input.update(cx, |input, cx| {
                input.set_value("", window, cx);
                input.focus(window, cx);
            });
            cx.notify();
        });
        window.render_frame(cx);
        window.press("/", cx);
        window.press("p", cx);
        window.press("l", cx);
        window.render_frame(cx);
        assert!(view.read(cx).slash_picker_state.is_open());
        window.press("tab", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
            "/plan "
        );
        assert!(!view.read(cx).slash_picker_state.is_open());

        // 5. Pressing Escape closes picker and stays closed (KeyUp must NOT reopen it!)
        view.update(cx, |v, cx| {
            v.inputs.prompt_input.update(cx, |input, cx| {
                input.set_value("", window, cx);
                input.focus(window, cx);
            });
            cx.notify();
        });
        window.render_frame(cx);
        window.press("/", cx);
        window.press("p", cx);
        window.press("l", cx);
        window.render_frame(cx);
        assert!(view.read(cx).slash_picker_state.is_open());
        window.press("escape", cx);
        window.render_frame(cx);
        assert!(!view.read(cx).slash_picker_state.is_open());
        assert_eq!(
            view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
            "/pl"
        );

        // 6. Mid-sentence Claude "please /pl" + Enter -> "please /plan "
        view.update(cx, |v, cx| {
            v.inputs.prompt_input.update(cx, |input, cx| {
                input.select_all(window, cx);
                input.replace("please ", window, cx);
                input.focus(window, cx);
            });
            cx.notify();
        });
        window.render_frame(cx);
        window.press("/", cx);
        window.press("p", cx);
        window.press("l", cx);
        window.press("enter", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
            "please /plan "
        );

        // 7. Codex "run $ver" + Tab -> "run $verify-multibot "
        view.update(cx, |v, cx| {
            v.selection.selected_session = Some(SessionSummary {
                id: "sess-codex-live".into(),
                agent: AgentKind::Codex,
                title: "Codex".into(),
                ..Default::default()
            });
            v.slash_picker_state.set_daemon_commands(
                AgentKind::Codex,
                vec![multibot_proto::SlashCommandItem {
                    name: "verify-multibot".into(),
                    description: "Run verify suite".into(),
                    trigger: "$".into(),
                    source: "skill".into(),
                    opens_in_terminal: false,
                }],
            );
            v.inputs.prompt_input.update(cx, |input, cx| {
                input.select_all(window, cx);
                input.replace("run ", window, cx);
                input.focus(window, cx);
            });
            cx.notify();
        });
        window.render_frame(cx);
        window.press("$", cx);
        window.press("v", cx);
        window.press("e", cx);
        window.press("r", cx);
        window.render_frame(cx);
        assert!(view.read(cx).slash_picker_state.is_open());
        window.press("tab", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
            "run $verify-multibot "
        );
        assert!(!view.read(cx).slash_picker_state.is_open());

        // 8. Ctrl+/ browse mode: "hello " -> Ctrl+/ -> "pl" -> Enter -> "hello /plan "
        view.update(cx, |v, cx| {
            v.selection.selected_session = Some(SessionSummary {
                id: "sess-claude-browse".into(),
                agent: AgentKind::Claude,
                title: "Claude".into(),
                ..Default::default()
            });
            v.inputs.prompt_input.update(cx, |input, cx| {
                input.select_all(window, cx);
                input.replace("hello ", window, cx);
                input.focus(window, cx);
            });
            cx.notify();
        });
        window.render_frame(cx);
        window.press("ctrl-/", cx);
        window.render_frame(cx);
        assert!(view.read(cx).slash_picker_state.is_open());
        assert_eq!(view.read(cx).slash_picker_state.browse_anchor_byte, Some(6));
        window.press("p", cx);
        window.press("l", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx)
                .slash_picker_state
                .filtered
                .first()
                .map(|i| i.name.as_str()),
            Some("plan")
        );
        window.press("enter", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
            "hello /plan "
        );
        assert!(!view.read(cx).slash_picker_state.is_open());

        // 9. Ctrl+/ browse mode: "hello " -> Ctrl+/ -> Backspace past anchor immediately closes picker
        view.update(cx, |v, cx| {
            v.inputs.prompt_input.update(cx, |input, cx| {
                input.select_all(window, cx);
                input.replace("hello ", window, cx);
                input.focus(window, cx);
            });
            cx.notify();
        });
        window.render_frame(cx);
        window.press("ctrl-/", cx);
        window.render_frame(cx);
        assert!(view.read(cx).slash_picker_state.is_open());
        window.press("backspace", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
            "hello"
        );
        assert!(
            !view.read(cx).slash_picker_state.is_open(),
            "backspacing past Ctrl+/ browse anchor must immediately close picker"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn live_escape_closes_find_action_menu_focused_nav_and_reply_quote_with_composer_focused(
    cx: &mut TestAppContext,
) {
    let (handle, view) = open_test_chat_window(cx);
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.selection.selected_session = Some(SessionSummary {
                id: "sess-esc".into(),
                agent: AgentKind::Claude,
                title: "Escape Test".into(),
                ..Default::default()
            });
            v.history.messages = vec![
                ChatMessage {
                    id: "m0".into(),
                    role: MessageRole::User,
                    text: "First message".into(),
                    ..Default::default()
                },
                ChatMessage {
                    id: "m1".into(),
                    role: MessageRole::Assistant,
                    text: "Second assistant message".into(),
                    ..Default::default()
                },
            ];
            v.inputs.prompt_input.update(cx, |input, cx| {
                input.set_value("", window, cx);
                input.focus(window, cx);
            });
            cx.notify();
        });
        window.render_frame(cx);

        // 1. Ctrl+F -> Escape closes Find even while composer is focused
        window.press("ctrl-f", cx);
        window.render_frame(cx);
        assert!(view.read(cx).chat_state.find_open);
        window.press("escape", cx);
        window.render_frame(cx);
        assert!(
            !view.read(cx).chat_state.find_open,
            "Escape must close Chat Find even while composer is focused"
        );

        // 2. Empty composer Up -> Enter opens message action menu; Escape closes menu,
        //    second Escape clears focused message nav.
        window.press("up", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).chat_state.focused_message_idx, Some(1));
        window.press("enter", cx);
        window.render_frame(cx);
        assert!(view.read(cx).chat_state.message_action_menu_open);

        window.press("escape", cx);
        window.render_frame(cx);
        assert!(!view.read(cx).chat_state.message_action_menu_open);
        assert_eq!(view.read(cx).chat_state.focused_message_idx, Some(1));

        window.press("escape", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).chat_state.focused_message_idx, None);

        // 3. Reply quote banner dismissed by Escape while composer is focused
        view.update(cx, |v, cx| {
            v.set_reply_quote("m1".into(), "Second assistant message".into(), cx);
        });
        window.render_frame(cx);
        assert!(view.read(cx).chat_state.reply_quote.is_some());
        window.press("escape", cx);
        window.render_frame(cx);
        assert!(
            view.read(cx).chat_state.reply_quote.is_none(),
            "Escape must dismiss reply quote banner while composer is focused"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn live_dislike_and_heart_reaction_full_add_echo_remove_echo_lifecycle(cx: &mut TestAppContext) {
    let (handle, view) = open_test_chat_window(cx);
    cx.update_window(handle, |_, window, cx| {
        for emoji in ReactionEmoji::all() {
            view.update(cx, |v, cx| {
                v.chat_state.local_reactions.clear();
                v.chat_state.local_reaction_removals.clear();
                v.history.messages = vec![ChatMessage {
                    id: "m1".into(),
                    role: MessageRole::Assistant,
                    text: "Great answer".into(),
                    ..Default::default()
                }];
                v.toggle_local_reaction("m1", "Great answer", emoji, cx);
            });
            window.render_frame(cx);

            // Optimistic badge visible
            let vis_opt = project_visible_messages_with_overrides(
                &view.read(cx).history.messages,
                &view.read(cx).chat_state.local_reactions,
                &view.read(cx).chat_state.local_reaction_removals,
                true,
            );
            assert_eq!(vis_opt[0].reactions, vec![(emoji, 1)]);

            // Daemon echoes add -> reconcile clears local_reactions, server badge intact
            let wire = format_reaction_message(emoji, "m1", "Great answer").unwrap();
            view.update(cx, |v, cx| {
                v.history.messages.push(ChatMessage {
                    id: "u_add".into(),
                    role: MessageRole::User,
                    text: wire.clone(),
                    ..Default::default()
                });
                let msgs = v.history.messages.clone();
                v.chat_state.reconcile_local_reactions(&msgs);
                cx.notify();
            });
            window.render_frame(cx);
            assert!(view.read(cx).chat_state.local_reactions.is_empty());
            assert!(view.read(cx).chat_state.local_reaction_removals.is_empty());
            assert_eq!(
                project_visible_messages(&view.read(cx).history.messages, &HashMap::new())[0]
                    .reactions,
                vec![(emoji, 1)]
            );

            // User toggles OFF -> optimistic removal hides badge
            view.update(cx, |v, cx| {
                v.toggle_local_reaction("m1", "Great answer", emoji, cx);
            });
            window.render_frame(cx);
            assert!(
                project_visible_messages_with_overrides(
                    &view.read(cx).history.messages,
                    &view.read(cx).chat_state.local_reactions,
                    &view.read(cx).chat_state.local_reaction_removals,
                    true
                )[0]
                .reactions
                .is_empty()
            );

            // Daemon echoes removal -> reconcile clears local_reaction_removals, zero phantom badges
            view.update(cx, |v, cx| {
                v.history.messages.push(ChatMessage {
                    id: "u_rem".into(),
                    role: MessageRole::User,
                    text: wire,
                    ..Default::default()
                });
                let msgs = v.history.messages.clone();
                v.chat_state.reconcile_local_reactions(&msgs);
                cx.notify();
            });
            window.render_frame(cx);
            assert!(
                view.read(cx).chat_state.local_reactions.is_empty(),
                "local_reactions must be empty after {emoji:?} remove echo"
            );
            assert!(
                view.read(cx).chat_state.local_reaction_removals.is_empty(),
                "local_reaction_removals must be empty after {emoji:?} remove echo"
            );
            assert!(
                project_visible_messages(&view.read(cx).history.messages, &HashMap::new())[0]
                    .reactions
                    .is_empty(),
                "fresh projection must have zero phantom badges after {emoji:?} toggle-off"
            );
        }
    })
    .unwrap();
}

#[gpui_kit::test]
fn trailing_hidden_reaction_keeps_question_prompt_and_prose_options_active_and_dual_blocks_have_unique_ids(
    cx: &mut TestAppContext,
) {
    let (handle, view) = open_test_chat_window(cx);
    cx.update_window(handle, |_, window, cx| {
        // 1. Hidden reaction at tail of messages must NOT lock out active QuestionPrompt
        let react_wire =
            format_reaction_message(ReactionEmoji::Like, "a-qp", "Please pick an option").unwrap();
        view.update(cx, |v, cx| {
            v.selection.selected_session = Some(SessionSummary {
                id: "sess-tail-react".into(),
                agent: AgentKind::Claude,
                title: "Tail React".into(),
                ..Default::default()
            });
            v.history.messages = vec![
                ChatMessage {
                    id: "a-qp".into(),
                    role: MessageRole::Assistant,
                    text: "Please pick an option".into(),
                    blocks: vec![MessageBlock::QuestionPrompt {
                        prompt_id: "p-active".into(),
                        provider: "claude".into(),
                        questions: vec![PromptQuestion {
                            question: "Choose one".into(),
                            header: "Choice".into(),
                            multi_select: false,
                            options: vec![
                                PromptOption {
                                    label: "Yes".into(),
                                    description: "".into(),
                                },
                                PromptOption {
                                    label: "No".into(),
                                    description: "".into(),
                                },
                            ],
                        }],
                    }],
                    ..Default::default()
                },
                ChatMessage {
                    id: "u-react".into(),
                    role: MessageRole::User,
                    text: react_wire,
                    ..Default::default()
                },
            ];
            cx.notify();
        });
        window.render_frame(cx);
        assert!(
            window.try_find("opt-card-0-0").is_some(),
            "trailing hidden reaction must not lock out QuestionPrompt option cards"
        );
        assert!(window.try_find("opt-other-0").is_some());

        // 2. Trailing hidden reaction also keeps prose A)/B) option cards rendered
        let prose_text = "Pick one:\nA) Keep\nB) Drop";
        let prose_react =
            format_reaction_message(ReactionEmoji::Heart, "a-prose", prose_text).unwrap();
        view.update(cx, |v, cx| {
            v.history.messages = vec![
                ChatMessage {
                    id: "a-prose".into(),
                    role: MessageRole::Assistant,
                    text: prose_text.into(),
                    ..Default::default()
                },
                ChatMessage {
                    id: "u-prose-react".into(),
                    role: MessageRole::User,
                    text: prose_react,
                    ..Default::default()
                },
            ];
            cx.notify();
        });
        window.render_frame(cx);
        assert!(window.try_find("prose-opt-0").is_some());
        assert!(window.try_find("prose-opt-1").is_some());

        // 3. Two QuestionPrompt blocks + two box-drawing Text blocks in the SAME latest
        //    assistant message must produce distinct, non-colliding ElementIds.
        let box_tbl = "┌───┬───┐\n│ K │ V │\n├───┼───┤\n│ a │ 1 │\n└───┴───┘";
        view.update(cx, |v, cx| {
            v.chat_state.answered_prompts.clear();
            v.chat_state.answered_questions.clear();
            v.history.messages = vec![ChatMessage {
                id: "a-dual".into(),
                role: MessageRole::Assistant,
                text: String::new(),
                blocks: vec![
                    MessageBlock::QuestionPrompt {
                        prompt_id: "p-block-0".into(),
                        provider: "claude".into(),
                        questions: vec![PromptQuestion {
                            question: "First prompt block".into(),
                            header: "B0".into(),
                            multi_select: false,
                            options: vec![
                                PromptOption {
                                    label: "B0 Opt1".into(),
                                    description: "".into(),
                                },
                                PromptOption {
                                    label: "B0 Opt2".into(),
                                    description: "".into(),
                                },
                            ],
                        }],
                    },
                    MessageBlock::QuestionPrompt {
                        prompt_id: "p-block-1".into(),
                        provider: "claude".into(),
                        questions: vec![PromptQuestion {
                            question: "Second prompt block".into(),
                            header: "B1".into(),
                            multi_select: false,
                            options: vec![
                                PromptOption {
                                    label: "B1 Opt1".into(),
                                    description: "".into(),
                                },
                                PromptOption {
                                    label: "B1 Opt2".into(),
                                    description: "".into(),
                                },
                            ],
                        }],
                    },
                    MessageBlock::Text {
                        text: box_tbl.into(),
                    },
                    MessageBlock::Text {
                        text: box_tbl.into(),
                    },
                ],
                ..Default::default()
            }];
            cx.notify();
        });
        window.render_frame(cx);

        // Primary block uses opt-card-0-0 / opt-other-0; secondary uses opt-card-b1-0-0 / opt-other-b1-0
        assert!(window.try_find("opt-card-0-0").is_some());
        assert!(window.try_find("opt-other-0").is_some());
        assert!(window.try_find("opt-card-b1-0-0").is_some());
        assert!(window.try_find("opt-other-b1-0").is_some());

        // Two Text blocks with box-drawing tables produce distinct scoped IDs
        assert!(window.try_find("expand-table-box-msg-md-0-2-0").is_some());
        assert!(window.try_find("expand-table-box-msg-md-0-3-0").is_some());

        // Clicking primary opt-card-0-0 answers ONLY p-block-0 (not shadowed by p-block-1!)
        window.click("opt-card-0-0", cx);
        window.render_frame(cx);
        assert!(
            view.read(cx)
                .chat_state
                .answered_prompts
                .contains("p-block-0")
        );
        assert!(
            !view
                .read(cx)
                .chat_state
                .answered_prompts
                .contains("p-block-1")
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn live_capture_action_propagation_enter_routing_and_modal_precedence(cx: &mut TestAppContext) {
    use gpui_kit::component::input::InputEvent;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let (handle, view) = open_test_chat_window(cx);
    let press_enter_count = Arc::new(AtomicUsize::new(0));
    let (daemon_tx, mut daemon_rx) = tokio::sync::mpsc::unbounded_channel();

    let _sub = cx
        .update_window(handle, |_, window, cx| {
            let prompt = view.read(cx).inputs.prompt_input.clone();
            let counter = press_enter_count.clone();
            let sub = cx.subscribe(&prompt, move |_input, ev: &InputEvent, _cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) {
                    counter.fetch_add(1, Ordering::SeqCst);
                }
            });

            view.update(cx, |v, cx| {
                v.selection.selected_session = Some(SessionSummary {
                    id: "sess-r4".into(),
                    agent: AgentKind::Claude,
                    title: "R4 Regression".into(),
                    ..Default::default()
                });
                v.connection.daemon_tx = Some(daemon_tx);
                v.history.messages.clear();
                v.inputs.prompt_input.update(cx, |input, cx| {
                    input.set_value("", window, cx);
                    input.focus(window, cx);
                });
                cx.notify();
            });
            window.render_frame(cx);

            // 1. Slash picker navigation: Down steps by 1 (0 -> 1, NEVER 2),
            //    PageDown steps by 1 page (1 -> 9, NEVER 16), Up steps by 1 (9 -> 8),
            //    PageUp steps by 1 page (8 -> 0).
            window.press("/", cx);
            window.render_frame(cx);
            assert!(view.read(cx).slash_picker_state.is_open());
            assert!(view.read(cx).slash_picker_state.filtered.len() > 16);
            assert_eq!(view.read(cx).slash_picker_state.selected, 0);

            window.press("down", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).slash_picker_state.selected,
                1,
                "Down must step slash picker by exactly 1 row (never double-step)"
            );

            window.press("pagedown", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).slash_picker_state.selected,
                9,
                "PageDown must step slash picker by exactly 1 page (8 rows, never 16)"
            );

            window.press("up", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).slash_picker_state.selected, 8);

            window.press("pageup", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).slash_picker_state.selected, 0);

            // 2. Slash picker Enter & Tab: replace span to "/plan ", emit ZERO PressEnter,
            //    and do NOT send a message.
            window.press("p", cx);
            window.press("l", cx);
            window.render_frame(cx);
            window.press("enter", cx);
            window.render_frame(cx);
            assert!(!view.read(cx).slash_picker_state.is_open());
            assert_eq!(
                view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
                "/plan "
            );
            assert_eq!(
                press_enter_count.load(Ordering::SeqCst),
                0,
                "Slash picker Enter must stop propagation and emit 0 InputEvent::PressEnter"
            );
            assert!(view.read(cx).history.messages.is_empty());
            assert!(daemon_rx.try_recv().is_err());

            // Tab also replaces cleanly with 0 PressEnter
            view.update(cx, |v, cx| {
                v.inputs.prompt_input.update(cx, |input, cx| {
                    input.set_value("", window, cx);
                    input.focus(window, cx);
                });
            });
            window.render_frame(cx);
            window.press("/", cx);
            window.press("p", cx);
            window.press("l", cx);
            window.render_frame(cx);
            window.press("tab", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
                "/plan "
            );
            assert_eq!(press_enter_count.load(Ordering::SeqCst), 0);

            // 3. Chat Find (Ctrl+F) Backspace & Enter:
            //    Backspace edits find_query while preserving composer "my draft" 100%,
            //    Enter / Shift+Enter advance find_current with 0 PressEnter and 0 sends.
            view.update(cx, |v, cx| {
                v.history.messages = vec![
                    ChatMessage {
                        id: "f0".into(),
                        role: MessageRole::Assistant,
                        text: "first needle match".into(),
                        ..Default::default()
                    },
                    ChatMessage {
                        id: "f1".into(),
                        role: MessageRole::Assistant,
                        text: "second needle match".into(),
                        ..Default::default()
                    },
                ];
                v.inputs.prompt_input.update(cx, |input, cx| {
                    input.set_value("my draft", window, cx);
                    input.focus(window, cx);
                });
                cx.notify();
            });
            window.render_frame(cx);

            window.press("ctrl-f", cx);
            window.press("n", cx);
            window.press("e", cx);
            window.render_frame(cx);
            assert!(view.read(cx).chat_state.find_open);
            assert_eq!(view.read(cx).chat_state.find_query, "ne");
            assert_eq!(view.read(cx).chat_state.find_matches.len(), 2);

            window.press("backspace", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).chat_state.find_query, "n");
            assert_eq!(
                view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
                "my draft",
                "Chat Find Backspace must stop propagation and never delete composer draft"
            );

            assert_eq!(view.read(cx).chat_state.find_current, 0);
            window.press("enter", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).chat_state.find_current, 1);
            assert_eq!(
                press_enter_count.load(Ordering::SeqCst),
                0,
                "Chat Find Enter must stop propagation and emit 0 PressEnter"
            );
            assert_eq!(
                view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
                "my draft",
                "Chat Find Enter must never send unsent composer draft"
            );
            assert!(daemon_rx.try_recv().is_err());

            window.press("shift-enter", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).chat_state.find_current, 0);
            assert_eq!(press_enter_count.load(Ordering::SeqCst), 0);

            // Escape closes Find
            window.press("escape", cx);
            window.render_frame(cx);
            assert!(!view.read(cx).chat_state.find_open);

            // 4. Empty-composer nav Enter opens action menu with 0 PressEnter
            view.update(cx, |v, cx| {
                v.inputs.prompt_input.update(cx, |input, cx| {
                    input.set_value("", window, cx);
                    input.focus(window, cx);
                });
            });
            window.render_frame(cx);
            window.press("up", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).chat_state.focused_message_idx, Some(1));
            window.press("enter", cx);
            window.render_frame(cx);
            assert!(view.read(cx).chat_state.message_action_menu_open);
            assert_eq!(
                press_enter_count.load(Ordering::SeqCst),
                0,
                "Empty-composer nav Enter must stop propagation and emit 0 PressEnter"
            );
            window.press("escape", cx);
            window.press("escape", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).chat_state.focused_message_idx, None);

            // 5. Ctrl+Enter with slash picker closed sends prompt cleanly ONCE
            view.update(cx, |v, cx| {
                v.history.messages.clear();
                v.inputs.prompt_input.update(cx, |input, cx| {
                    input.set_value("hello world", window, cx);
                    input.focus(window, cx);
                });
                cx.notify();
            });
            window.render_frame(cx);
            window.press("ctrl-enter", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
                ""
            );
            assert_eq!(view.read(cx).history.messages.len(), 1);
            assert_eq!(view.read(cx).history.messages[0].text, "hello world");
            match daemon_rx.try_recv() {
                Ok(ClientRequest::SendPrompt { text, .. }) => {
                    assert_eq!(text, "hello world");
                }
                other => panic!("expected ClientRequest::SendPrompt, got {other:?}"),
            }
            assert!(daemon_rx.try_recv().is_err());
            assert_eq!(press_enter_count.load(Ordering::SeqCst), 0);

            // 6. Modal precedence: when slash_picker_state.is_open() AND table_modal_state.is_open()
            //    with prompt_input still focused, Tab/Enter/Up/Down/Backspace do NOT mutate
            //    hidden slash picker or composer, and Escape closes table_modal_state first!
            window.press("/", cx);
            window.press("p", cx);
            window.press("l", cx);
            window.render_frame(cx);
            assert!(view.read(cx).slash_picker_state.is_open());
            let sel_before = view.read(cx).slash_picker_state.selected;
            let table = crate::table_modal::parse_table("| A | B |\n|---|---|\n| 1 | 2 |")
                .expect("valid table");
            view.update(cx, |v, cx| {
                v.open_table_modal(table, "Modal Precedence Test", cx);
            });
            window.render_frame(cx);
            assert!(view.read(cx).table_modal_state.is_open());

            for key in ["tab", "enter", "up", "down", "pagedown", "backspace"] {
                window.press(key, cx);
                window.render_frame(cx);
                assert!(
                    view.read(cx).table_modal_state.is_open(),
                    "key {key} must not close table modal"
                );
                assert!(
                    view.read(cx).slash_picker_state.is_open(),
                    "key {key} must not accept/close hidden slash picker behind table modal"
                );
                assert_eq!(
                    view.read(cx).slash_picker_state.selected,
                    sel_before,
                    "key {key} must not mutate hidden slash picker selection behind table modal"
                );
                assert_eq!(
                    view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
                    "/pl",
                    "key {key} must not mutate composer behind table modal"
                );
            }
            assert_eq!(press_enter_count.load(Ordering::SeqCst), 0);

            // Escape closes table_modal_state first while preserving slash_picker & composer
            window.press("escape", cx);
            window.render_frame(cx);
            assert!(
                !view.read(cx).table_modal_state.is_open(),
                "Escape must close table_modal_state first"
            );
            assert!(
                view.read(cx).slash_picker_state.is_open(),
                "Closing table modal via Escape must preserve underlying slash picker"
            );
            assert_eq!(
                view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
                "/pl"
            );

            // Shortcuts modal also blocks Find Backspace/Enter behind the modal
            window.press("escape", cx);
            window.press("ctrl-f", cx);
            window.press("x", cx);
            window.render_frame(cx);
            assert!(view.read(cx).chat_state.find_open);
            assert_eq!(view.read(cx).chat_state.find_query, "x");
            view.update(cx, |v, cx| {
                v.active_modal = ActiveModal::Shortcuts;
                cx.notify();
            });
            window.render_frame(cx);
            window.press("backspace", cx);
            window.press("enter", cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).chat_state.find_query,
                "x",
                "ActiveModal::Shortcuts must prevent Find Backspace behind modal"
            );
            assert_eq!(
                view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
                "/pl",
                "ActiveModal::Shortcuts must prevent composer mutation behind modal"
            );

            sub
        })
        .unwrap();
}

#[gpui_kit::test]
fn live_modal_printable_guard_and_find_slash_routing_regressions(cx: &mut TestAppContext) {
    let (handle, view) = open_test_chat_window(cx);

    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.selection.selected_session = Some(SessionSummary {
                id: "sess-r5".into(),
                agent: AgentKind::Claude,
                title: "R5 Regression".into(),
                ..Default::default()
            });
            v.history.messages = vec![
                ChatMessage {
                    id: "m0".into(),
                    role: MessageRole::Assistant,
                    text: "first needle line".into(),
                    ..Default::default()
                },
                ChatMessage {
                    id: "m1".into(),
                    role: MessageRole::Assistant,
                    text: "second needle line".into(),
                    ..Default::default()
                },
            ];
            v.inputs.prompt_input.update(cx, |input, cx| {
                input.set_value("", window, cx);
                input.focus(window, cx);
            });
            cx.notify();
        });
        window.render_frame(cx);

        // 1. Printable characters behind table_modal_state and ActiveModal::Shortcuts
        //    must leave prompt_input 100% untouched.
        window.press("/", cx);
        window.press("p", cx);
        window.press("l", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
            "/pl"
        );
        assert!(view.read(cx).slash_picker_state.is_open());

        let table = crate::table_modal::parse_table("| Op | Desc |\n|---|---|\n| `|` | OR |")
            .expect("valid table");
        view.update(cx, |v, cx| {
            v.open_table_modal(table, "Printable Guard Test", cx);
        });
        window.render_frame(cx);
        assert!(view.read(cx).table_modal_state.is_open());

        window.press("a", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
            "/pl",
            "Typing 'a' behind open table modal must not mutate prompt_input"
        );

        // Escape still closes table modal cleanly
        window.press("escape", cx);
        window.render_frame(cx);
        assert!(!view.read(cx).table_modal_state.is_open());

        // Shortcuts modal also blocks printable 'z' behind modal
        view.update(cx, |v, cx| {
            v.slash_picker_state.close();
            v.inputs.prompt_input.update(cx, |input, cx| {
                input.set_value("draft", window, cx);
                input.focus(window, cx);
            });
            v.active_modal = ActiveModal::Shortcuts;
            cx.notify();
        });
        window.render_frame(cx);
        window.press("z", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
            "draft",
            "Typing 'z' behind ActiveModal::Shortcuts must not mutate prompt_input"
        );
        window.press("escape", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).active_modal, ActiveModal::None);

        // 2. Shift+Enter, Ctrl+Enter, and Alt+Enter while slash picker is open must NOT
        //    accept "/plan " and must keep slash_picker_state.is_open() == true.
        view.update(cx, |v, cx| {
            v.inputs.prompt_input.update(cx, |input, cx| {
                input.set_value("", window, cx);
                input.focus(window, cx);
            });
            cx.notify();
        });
        window.render_frame(cx);
        window.press("/", cx);
        window.press("p", cx);
        window.press("l", cx);
        window.render_frame(cx);
        assert!(view.read(cx).slash_picker_state.is_open());
        assert_eq!(
            view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
            "/pl"
        );

        for chord in ["shift-enter", "alt-enter"] {
            window.press(chord, cx);
            window.render_frame(cx);
            assert!(
                view.read(cx).slash_picker_state.is_open(),
                "{chord} must keep slash_picker_state open"
            );
            assert_ne!(
                view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
                "/plan ",
                "{chord} must NOT accept '/plan ' from slash picker"
            );
        }

        // Reset composer cleanly to "/pl" with slash picker open
        view.update(cx, |v, cx| {
            v.inputs.prompt_input.update(cx, |input, cx| {
                input.set_value("", window, cx);
                input.focus(window, cx);
            });
            cx.notify();
        });
        window.render_frame(cx);
        window.press("/", cx);
        window.press("p", cx);
        window.press("l", cx);
        window.render_frame(cx);
        assert!(view.read(cx).slash_picker_state.is_open());
        assert_eq!(
            view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
            "/pl"
        );

        // 3. Ctrl+F while slash picker is open closes slash_picker_state so Find Enter
        //    advances find_current and leaves composer "/pl" untouched; Ctrl+/ while Find
        //    is open closes Find.
        window.press("ctrl-f", cx);
        window.render_frame(cx);
        assert!(view.read(cx).chat_state.find_open);
        assert!(
            !view.read(cx).slash_picker_state.is_open(),
            "Ctrl+F must close slash_picker_state"
        );

        window.press("n", cx);
        window.press("e", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).chat_state.find_query, "ne");
        assert_eq!(view.read(cx).chat_state.find_matches.len(), 2);
        assert_eq!(view.read(cx).chat_state.find_current, 0);

        window.press("enter", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).chat_state.find_current,
            1,
            "Find Enter must advance find_current when opened over slash picker"
        );
        assert_eq!(
            view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
            "/pl",
            "Find Enter must leave composer '/pl' untouched"
        );

        window.press("ctrl-/", cx);
        window.render_frame(cx);
        assert!(
            !view.read(cx).chat_state.find_open,
            "Ctrl+/ must close Chat Find"
        );
        assert!(
            view.read(cx).slash_picker_state.is_open(),
            "Ctrl+/ must open slash command browser"
        );
        window.press("escape", cx);
        window.render_frame(cx);

        // 4. Ctrl+Backspace (DeleteToPreviousWordStart) and Home+Delete (Delete) while
        //    Chat Find is open must leave composer drafts 100% untouched.
        view.update(cx, |v, cx| {
            v.inputs.prompt_input.update(cx, |input, cx| {
                input.set_value("hello world", window, cx);
                input.focus(window, cx);
            });
            cx.notify();
        });
        window.render_frame(cx);
        window.press("ctrl-f", cx);
        window.press("a", cx);
        window.render_frame(cx);
        assert!(view.read(cx).chat_state.find_open);

        window.press("ctrl-backspace", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
            "hello world",
            "Ctrl+Backspace while Find is open must NOT delete words from composer draft"
        );

        window.press("escape", cx);
        window.render_frame(cx);
        view.update(cx, |v, cx| {
            v.inputs.prompt_input.update(cx, |input, cx| {
                input.set_value("hello draft", window, cx);
                input.focus(window, cx);
            });
            cx.notify();
        });
        window.render_frame(cx);
        window.press("ctrl-f", cx);
        window.press("n", cx);
        window.render_frame(cx);

        window.press("home", cx);
        window.press("delete", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
            "hello draft",
            "Home+Delete while Find is open must NOT delete characters from composer draft"
        );

        // 5. Multi-word Chat Find search ("first needle") via Spacebar must append space
        //    to find_query, match message 0, and leave composer draft untouched (no ' ' leak).
        window.press("escape", cx);
        window.render_frame(cx);
        view.update(cx, |v, cx| {
            v.inputs.prompt_input.update(cx, |input, cx| {
                input.set_value("my draft", window, cx);
                input.focus(window, cx);
            });
            cx.notify();
        });
        window.render_frame(cx);

        window.press("ctrl-f", cx);
        for k in [
            "f", "i", "r", "s", "t", "space", "n", "e", "e", "d", "l", "e",
        ] {
            window.press(k, cx);
        }
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).chat_state.find_query,
            "first needle",
            "Spacebar must append space into multi-word Chat Find query"
        );
        assert_eq!(
            view.read(cx).chat_state.find_matches,
            vec![0],
            "Multi-word Chat Find query 'first needle' must match message 0"
        );
        assert_eq!(
            view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
            "my draft",
            "Spacebar inside Chat Find must NOT leak ' ' into composer draft"
        );

        // 6. With empty composer + Find open, Up/Down/End must NOT fall through into
        //    empty-composer message navigation behind Find.
        window.press("escape", cx);
        window.render_frame(cx);
        view.update(cx, |v, cx| {
            v.chat_state.focused_message_idx = None;
            v.inputs.prompt_input.update(cx, |input, cx| {
                input.set_value("", window, cx);
                input.focus(window, cx);
            });
            cx.notify();
        });
        window.render_frame(cx);

        window.press("ctrl-f", cx);
        window.render_frame(cx);
        assert!(view.read(cx).chat_state.find_open);
        for nav_key in ["up", "down", "end"] {
            window.press(nav_key, cx);
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).chat_state.focused_message_idx,
                None,
                "{nav_key} while Find is open must NOT fall through into empty-composer nav"
            );
        }
    })
    .unwrap();
}

#[test]
fn reaction_hides_plain_text_block_assistant_ack_but_preserves_tool_use() {
    let base_msg = ChatMessage {
        id: "a1".into(),
        role: MessageRole::Assistant,
        text: "Hello world".into(),
        blocks: vec![MessageBlock::Text {
            text: "Hello world".into(),
        }],
        ..Default::default()
    };
    let wire = format_reaction_message(ReactionEmoji::Like, "a1", "Hello world").unwrap();
    let react_msg = ChatMessage {
        id: "u1".into(),
        role: MessageRole::User,
        text: wire.clone(),
        blocks: vec![MessageBlock::Text { text: wire }],
        ..Default::default()
    };
    let plain_ack = ChatMessage {
        id: "a2".into(),
        role: MessageRole::Assistant,
        text: "You're welcome!".into(),
        blocks: vec![MessageBlock::Text {
            text: "You're welcome!".into(),
        }],
        ..Default::default()
    };

    let empty_local = HashMap::new();
    let msgs_plain = [base_msg.clone(), react_msg.clone(), plain_ack];
    let visible = project_visible_messages(&msgs_plain, &empty_local);
    assert_eq!(
        visible.len(),
        1,
        "lone assistant ack with MessageBlock::Text must be hidden"
    );
    assert_eq!(visible[0].message.id, "a1");
    assert_eq!(visible[0].reactions, vec![(ReactionEmoji::Like, 1)]);

    let tool_turn = ChatMessage {
        id: "a3".into(),
        role: MessageRole::Assistant,
        text: "Running check".into(),
        blocks: vec![
            MessageBlock::Text {
                text: "Running check".into(),
            },
            MessageBlock::ToolUse {
                call_id: "c1".into(),
                name: "Bash".into(),
                summary: "Run cargo test".into(),
                detail: "cargo test".into(),
                status: ToolStatus::Completed,
                diff: None,
                output: Some("ok".into()),
            },
        ],
        ..Default::default()
    };
    let msgs_tool = [base_msg, react_msg, tool_turn];
    let visible_with_tool = project_visible_messages(&msgs_tool, &empty_local);
    assert_eq!(
        visible_with_tool.len(),
        2,
        "assistant turn containing ToolUse after reaction must NOT be hidden"
    );
    assert_eq!(visible_with_tool[1].message.id, "a3");
}

#[gpui_kit::test]
fn quoted_reply_with_text_blocks_renders_body_once_without_duplicate_blockquote(
    cx: &mut TestAppContext,
) {
    assert_eq!(
        strip_rendered_quote_prefix("> Original line\n\nMy reply"),
        "My reply"
    );
    assert_eq!(strip_rendered_quote_prefix("> Original line"), "");

    let (handle, view) = open_test_chat_window(cx);
    let sess = SessionSummary {
        id: "quote-blocks-1".into(),
        agent: AgentKind::Claude,
        title: "Quote Blocks".into(),
        ..Default::default()
    };
    let quoted_raw = "> Original line\n\nMy reply".to_string();

    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.connection.sessions = vec![sess.clone()];
            v.selection.selected_session = Some(sess.clone());
            v.history.messages = vec![
                ChatMessage {
                    id: "a1".into(),
                    role: MessageRole::Assistant,
                    text: "Original line".into(),
                    blocks: vec![MessageBlock::Text {
                        text: "Original line".into(),
                    }],
                    ..Default::default()
                },
                ChatMessage {
                    id: "u1".into(),
                    role: MessageRole::User,
                    text: quoted_raw.clone(),
                    blocks: vec![MessageBlock::Text { text: quoted_raw }],
                    quoted_message_id: Some("a1".into()),
                    ..Default::default()
                },
            ];
            cx.notify();
        });
        window.render_frame(cx);

        assert!(
            window.try_find("quote-ref-1").is_some(),
            "compact clickable quote header must be rendered"
        );
        assert!(
            window.try_find("msg-user-text-1-0").is_some(),
            "user reply body must render consistently as user text"
        );
        assert!(
            window.try_find("msg-md-1-0").is_none(),
            "quoted user reply with MessageBlock::Text must not render raw markdown blockquote duplicate"
        );
    })
    .unwrap();
}

fn exercise_provider_switch(cx: &mut TestAppContext, composer: bool) {
    use multibot_proto::{HandoverContext, HarnessChoice};
    let mut choices = HarnessChoice::defaults();
    choices.push(HarnessChoice {
        agent: AgentKind::Pi,
        model: Some("custom/fast".into()),
        label: "Pi · custom · fast".into(),
    });
    for (index, target) in choices.iter().cloned().enumerate() {
        let (handle, view) = open_test_chat_window(cx);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        cx.update_window(handle, |_, window, cx| {
            let previous = SessionSummary {
                id: format!("old-{index}"),
                agent: if target.agent == AgentKind::Claude {
                    AgentKind::Codex
                } else {
                    AgentKind::Claude
                },
                title: "Release planning".into(),
                cwd: Some("/repo/multibot".into()),
                workspace: Some("Build".into()),
                ..Default::default()
            };
            let history = vec![ChatMessage {
                id: "old-message".into(),
                text: "Keep the GPU invariant.".into(),
                ..Default::default()
            }];
            let workspace_id = view.update(cx, |v, cx| {
                v.connection.daemon_tx = Some(tx.clone());
                v.chat_state
                    .harness_choices
                    .insert("local".into(), choices.clone());
                v.connection.sessions = vec![previous.clone()];
                v.selection.selected_session = Some(previous.clone());
                v.history.messages = history.clone();
                let ws = v
                    .sidebar_state
                    .workspace_store
                    .create_workspace("Build", None)
                    .unwrap();
                let key = v.session_key_for(&previous);
                v.sidebar_state
                    .workspace_store
                    .assign_session_workspace(&key, &ws.id);
                v.sidebar_state.workspace_store.favorites.insert(key);
                v.sync_sessions_to_shared(cx);
                v.sync_workspace_to_shared(cx);
                v.inputs.prompt_input.update(cx, |input, cx| {
                    input.set_value("my draft", window, cx);
                    input.focus(window, cx);
                });
                cx.notify();
                ws.id
            });
            window.render_frame(cx);
            if composer {
                window.click("attach-file-btn", cx);
                window.render_frame(cx);
                assert!(window.find("composer-provider").visible());
                window.click("composer-provider", cx);
            } else {
                window.press("ctrl-k", cx);
                window.render_frame(cx);
                window.input("switch provider", cx);
                window.render_frame(cx);
                window.press("enter", cx);
            }
            window.render_frame(cx);
            assert!(view.read(cx).palette_state.provider_picker);
            for i in 0..choices.len() {
                assert!(window.find(format!("pal-provider-{i}")).visible());
            }
            for _ in 0..index {
                window.press("down", cx);
            }
            window.press("enter", cx);
            let mut switch = None;
            while let Ok(request) = rx.try_recv() {
                if let ClientRequest::SwitchHarness {
                    session,
                    target: chosen,
                    context,
                } = request
                {
                    assert_eq!(chosen, target);
                    assert_eq!(session.title, previous.title);
                    assert_eq!(session.cwd, previous.cwd);
                    assert_eq!(session.workspace, previous.workspace);
                    assert_eq!(context, HandoverContext::from_messages(&history));
                    switch = Some(session);
                }
            }
            assert!(switch.is_some());
            let next = SessionSummary {
                id: format!("new-{index}"),
                agent: target.agent,
                model: target.model.clone(),
                ..previous.clone()
            };
            view.update(cx, |v, cx| {
                v.handle_daemon_server_event(
                    multibot_proto::ServerEvent::HarnessSwitched {
                        previous: previous.clone(),
                        session: next.clone(),
                    },
                    cx,
                )
            });
            window.render_frame(cx);
            let v = view.read(cx);
            assert_eq!(v.selection.selected_session.as_ref(), Some(&next));
            assert_eq!(v.history.messages, history);
            assert_eq!(v.inputs.prompt_input.read(cx).value().as_ref(), "my draft");
            let key = v.session_key_for(&next);
            assert_eq!(
                v.sidebar_state.workspace_store.effective_workspace_id(&key),
                workspace_id
            );
            assert!(v.sidebar_state.workspace_store.favorites.contains(&key));
            assert!(!v.connection.sessions.iter().any(|s| s.id == previous.id));
            view.update(cx, |v, cx| v.send_current_prompt(window, cx));
            let mut sent = false;
            while let Ok(request) = rx.try_recv() {
                if let ClientRequest::SendPrompt {
                    session_id,
                    text,
                    options,
                    ..
                } = request
                {
                    assert_eq!(session_id, next.id);
                    assert_eq!(text, "my draft");
                    assert_eq!(options.and_then(|o| o.model), target.model);
                    sent = true;
                }
            }
            assert!(sent);
        })
        .unwrap();
    }
}

#[gpui_kit::test]
fn provider_switch_palette_preserves_chat_and_workspace(cx: &mut TestAppContext) {
    exercise_provider_switch(cx, false);
}

#[gpui_kit::test]
fn provider_switch_composer_preserves_chat_and_workspace(cx: &mut TestAppContext) {
    exercise_provider_switch(cx, true);
}

#[gpui_kit::test]
fn message_navigation_scroll_menu_copy_reply_quote_and_focus(cx: &mut TestAppContext) {
    let (handle, view) = open_test_chat_window(cx);
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.history.messages = (0..30)
                .map(|i| ChatMessage {
                    id: format!("nav-{i}"),
                    role: if i % 2 == 0 {
                        MessageRole::User
                    } else {
                        MessageRole::Assistant
                    },
                    text: format!("Element {i}\nSecond line"),
                    ..Default::default()
                })
                .collect();
            v.inputs.prompt_input.update(cx, |input, cx| {
                input.set_value("", window, cx);
                input.focus(window, cx);
            });
            cx.notify();
        });
        window.render_frame(cx);
        for index in (0..30).rev() {
            window.press("up", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).chat_state.focused_message_idx, Some(index));
            assert!(window.find(format!("msg-bubble-{index}")).visible());
        }
        window.press("up", cx);
        assert_eq!(view.read(cx).chat_state.focused_message_idx, Some(0));
        for index in 1..30 {
            window.press("down", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).chat_state.focused_message_idx, Some(index));
        }
        window.press("down", cx);
        window.render_frame(cx);
        assert!(
            view.read(cx)
                .inputs
                .prompt_input
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
        );
        assert_eq!(view.read(cx).chat_state.focused_message_idx, None);
        window.press("up", cx);
        window.press("enter", cx);
        window.render_frame(cx);
        assert!(window.find("message-action-menu").visible());
        window.press("enter", cx);
        window.render_frame(cx);
        assert_eq!(
            cx.read_from_clipboard().unwrap().text().unwrap(),
            "Element 29\nSecond line"
        );
        assert!(!view.read(cx).chat_state.message_action_menu_open);
        assert_eq!(view.read(cx).chat_state.focused_message_idx, Some(29));
        window.press("enter", cx);
        window.press("down", cx);
        window.press("enter", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx)
                .chat_state
                .reply_quote
                .as_ref()
                .unwrap()
                .message_id,
            "nav-29"
        );
        assert!(
            view.read(cx)
                .inputs
                .prompt_input
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
        );
        view.update(cx, |v, _| v.chat_state.reply_quote = None);
        window.press("up", cx);
        window.press("enter", cx);
        window.press("down", cx);
        window.press("down", cx);
        window.press("enter", cx);
        window.render_frame(cx);
        let input = view.read(cx).inputs.prompt_input.read(cx);
        assert_eq!(input.value().as_ref(), "> Element 29\n> Second line\n\n");
        assert_eq!(input.cursor(), input.value().len());
        assert!(input.focus_handle(cx).is_focused(window));
        view.update(cx, |v, cx| {
            v.inputs.prompt_input.update(cx, |input, cx| {
                input.set_value("draft", window, cx);
                input.set_selected_range(0..0, cx);
            });
        });
        window.press("up", cx);
        window.press("enter", cx);
        window.press("escape", cx);
        window.render_frame(cx);
        assert!(!view.read(cx).chat_state.message_action_menu_open);
        assert_eq!(view.read(cx).chat_state.focused_message_idx, Some(29));
        window.press("escape", cx);
        window.render_frame(cx);
        assert!(
            view.read(cx)
                .inputs
                .prompt_input
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
        );
        assert_eq!(
            view.read(cx).inputs.prompt_input.read(cx).value().as_ref(),
            "draft"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn focused_composer_capture_actions_nav_paste_end_and_normal_editing(cx: &mut TestAppContext) {
    let (handle, view) = open_test_chat_window(cx);
    let sess = SessionSummary {
        id: "composer-nav-1".into(),
        agent: AgentKind::Claude,
        title: "Composer Nav".into(),
        ..Default::default()
    };

    let tmp_dir = std::env::temp_dir().join(format!("multibot-paste-cap-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp_dir);
    let png_path = tmp_dir.join("pasted.png");
    std::fs::write(&png_path, b"\x89PNG\r\n\x1a\n12345678").unwrap();

    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.connection.sessions = vec![sess.clone()];
            v.selection.selected_session = Some(sess.clone());
            v.history.messages = vec![
                ChatMessage {
                    id: "m0".into(),
                    role: MessageRole::Assistant,
                    text: "First message".into(),
                    ..Default::default()
                },
                ChatMessage {
                    id: "m1".into(),
                    role: MessageRole::User,
                    text: "Waiting message".into(),
                    ..Default::default()
                },
            ];
            v.chat_state.failed_sends.insert(
                "m1".into(),
                FailedSend {
                    message_id: "m1".into(),
                    text: "Waiting message".into(),
                    reason: "offline".into(),
                },
            );
            v.inputs.prompt_input.update(cx, |inp, cx| {
                inp.set_value("", window, cx);
                inp.focus(window, cx);
            });
            cx.notify();
        });
        window.render_frame(cx);

        // 1. Up/Down/Delete directly while prompt-input is focused (NO ctrl-f!)
        window.press("up", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).chat_state.focused_message_idx,
            Some(1),
            "Up while empty prompt-input is focused must focus newest message"
        );

        window.press("up", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).chat_state.focused_message_idx, Some(0));

        window.press("down", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).chat_state.focused_message_idx, Some(1));

        window.press("delete", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).history.messages.len(),
            1,
            "Delete on focused waiting message while prompt-input is focused must cancel it"
        );

        // 2. End on empty focused prompt-input jumps to newest (clears focus)
        window.press("up", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).chat_state.focused_message_idx, Some(0));
        window.press("end", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).chat_state.focused_message_idx,
            None,
            "End on empty focused prompt-input must jump to newest"
        );

        // 3. Ctrl+V with ExternalPaths stages image while prompt-input is focused
        cx.write_to_clipboard(ClipboardItem {
            entries: vec![ClipboardEntry::ExternalPaths(ExternalPaths(
                [png_path.clone()].into_iter().collect(),
            ))],
        });
        window.press("ctrl-v", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).chat_state.pending_attachments.len(),
            1,
            "Ctrl+V with ExternalPaths while prompt-input is focused must stage the image"
        );
        assert!(
            view.read(cx)
                .inputs
                .prompt_input
                .read(cx)
                .value()
                .contains("pasted.png")
        );

        // 4. Normal text paste & non-empty Up stay untouched
        cx.write_to_clipboard(ClipboardItem::new_string(" hello".into()));
        window.press("ctrl-v", cx);
        window.render_frame(cx);
        assert!(
            view.read(cx)
                .inputs
                .prompt_input
                .read(cx)
                .value()
                .contains("hello"),
            "Ctrl+V with text clipboard must paste text normally"
        );
        window.press("up", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).chat_state.focused_message_idx,
            None,
            "Up with non-empty prompt-input must NOT hijack message navigation"
        );
    })
    .unwrap();

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[gpui_kit::test]
fn cumulative_attachment_caps_enforce_max_10_images_and_10_files_across_batches(
    cx: &mut TestAppContext,
) {
    let (handle, view) = open_test_chat_window(cx);
    let sess = SessionSummary {
        id: "cap-1".into(),
        agent: AgentKind::Claude,
        title: "Cap Session".into(),
        ..Default::default()
    };

    let tmp_dir = std::env::temp_dir().join(format!("multibot-attach-cap-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp_dir);
    let mut pngs = Vec::new();
    let mut txts = Vec::new();
    for i in 0..12 {
        let p = tmp_dir.join(format!("img-{i}.png"));
        std::fs::write(&p, b"\x89PNG\r\n\x1a\n1234").unwrap();
        pngs.push(p);
        let f = tmp_dir.join(format!("doc-{i}.txt"));
        std::fs::write(&f, b"hello attachment").unwrap();
        txts.push(f);
    }

    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.connection.sessions = vec![sess.clone()];
            v.selection.selected_session = Some(sess.clone());
            v.stage_dropped_paths(&pngs[0..6], window, cx);
            assert_eq!(v.chat_state.pending_attachments.len(), 6);
            assert!(v.notice.is_none());

            // Second batch of 6 images: only 4 accepted (total 10), 2 rejected with notice
            v.stage_dropped_paths(&pngs[6..12], window, cx);
            let img_count = v
                .chat_state
                .pending_attachments
                .iter()
                .filter(|a| a.is_image)
                .count();
            assert_eq!(
                img_count, MAX_IMAGE_ATTACHMENTS,
                "cumulative image staging must cap at 10 across multiple drops"
            );
            assert!(
                v.notice
                    .as_deref()
                    .is_some_and(|n| n.contains("2 extra attachment")),
                "expected notice reporting 2 skipped images, got {:?}",
                v.notice
            );

            // Files are counted against their own separate 10-file cap
            v.notice = None;
            v.stage_dropped_paths(&txts[0..6], window, cx);
            v.stage_dropped_paths(&txts[6..12], window, cx);
            let file_count = v
                .chat_state
                .pending_attachments
                .iter()
                .filter(|a| !a.is_image)
                .count();
            assert_eq!(
                file_count, MAX_FILE_ATTACHMENTS,
                "cumulative file staging must cap at 10 independently of images"
            );
            assert!(
                v.notice
                    .as_deref()
                    .is_some_and(|n| n.contains("2 extra attachment"))
            );
        });
    })
    .unwrap();

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[test]
fn ssh_upload_command_streams_file_and_substitutes_remote_path() {
    let substituted = substitute_uploaded_attachment_paths(
        "inspect shot.png and shot.png please",
        &[
            (
                "shot.png".into(),
                "/home/u/.multibot/uploads/s1/shot.png".into(),
            ),
            (
                "shot.png".into(),
                "/home/u/.multibot/uploads/s1/1-shot.png".into(),
            ),
        ],
    );
    assert_eq!(
        substituted,
        "inspect /home/u/.multibot/uploads/s1/shot.png and /home/u/.multibot/uploads/s1/1-shot.png please"
    );

    let appended = substitute_uploaded_attachment_paths(
        "describe this",
        &[(
            "removed.png".into(),
            "/home/u/.multibot/uploads/s1/removed.png".into(),
        )],
    );
    assert_eq!(
        appended,
        "describe this\n/home/u/.multibot/uploads/s1/removed.png"
    );

    #[cfg(unix)]
    {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let tmp_dir =
            std::env::temp_dir().join(format!("multibot-ssh-upload-test-{}", std::process::id()));
        let fake_home = tmp_dir.join("home");
        let _ = std::fs::create_dir_all(&fake_home);
        let local_file = tmp_dir.join("shot.png");
        let payload = b"\x89PNG\r\n\x1a\nmultibot-upload-payload";
        std::fs::write(&local_file, payload).unwrap();

        let cmd_str = build_ssh_upload_command("sess-42", "shot.png", payload.len() as u64);
        let (first_remote, second_remote) = rt.block_on(async {
            let mut cmd1 = tokio::process::Command::new("sh");
            cmd1.args(["-c", &cmd_str]).env("HOME", &fake_home);
            let p1 = multibot_core::daemon_deploy::upload_file_to_command(&mut cmd1, &local_file)
                .await
                .expect("first upload must succeed");

            let mut cmd2 = tokio::process::Command::new("sh");
            cmd2.args(["-c", &cmd_str]).env("HOME", &fake_home);
            let p2 = multibot_core::daemon_deploy::upload_file_to_command(&mut cmd2, &local_file)
                .await
                .expect("second upload collision dedup must succeed");
            (p1, p2)
        });

        assert!(first_remote.ends_with("/.multibot/uploads/sess-42/shot.png"));
        assert!(second_remote.ends_with("/.multibot/uploads/sess-42/1-shot.png"));
        assert_eq!(std::fs::read(&first_remote).unwrap(), payload);
        assert_eq!(std::fs::read(&second_remote).unwrap(), payload);

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}

#[gpui_kit::test]
fn stage_dropped_paths_rejects_directories_and_missing_paths_with_notice(cx: &mut TestAppContext) {
    let (handle, view) = open_test_chat_window(cx);
    let sess = SessionSummary {
        id: "dir-check-1".into(),
        agent: AgentKind::Claude,
        title: "Dir Check".into(),
        ..Default::default()
    };

    let tmp_dir =
        std::env::temp_dir().join(format!("multibot-attach-dir-check-{}", std::process::id()));
    let sub_dir = tmp_dir.join("folder-not-file");
    let _ = std::fs::create_dir_all(&sub_dir);
    let missing_path = tmp_dir.join("does-not-exist.png");
    let valid_file = tmp_dir.join("real.txt");
    std::fs::write(&valid_file, b"valid-bytes").unwrap();

    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.connection.sessions = vec![sess.clone()];
            v.selection.selected_session = Some(sess.clone());

            // Directory + missing path must be rejected with a clear notice and 0 staged.
            v.stage_dropped_paths(&[sub_dir.clone(), missing_path.clone()], window, cx);
            assert!(v.chat_state.pending_attachments.is_empty());
            assert!(v.inputs.prompt_input.read(cx).value().is_empty());
            assert!(
                v.notice
                    .as_deref()
                    .is_some_and(|n| n.contains("2 non-file or inaccessible")),
                "expected notice for rejected directory/missing path, got {:?}",
                v.notice
            );

            // Mixed batch: regular file staged, directory skipped with notice.
            v.notice = None;
            v.stage_dropped_paths(&[sub_dir.clone(), valid_file.clone()], window, cx);
            assert_eq!(v.chat_state.pending_attachments.len(), 1);
            assert_eq!(v.chat_state.pending_attachments[0].path, valid_file);
            assert_eq!(v.inputs.prompt_input.read(cx).value().as_ref(), "real.txt ");
            assert!(
                v.notice
                    .as_deref()
                    .is_some_and(|n| n.contains("1 non-file or inaccessible"))
            );
        });
    })
    .unwrap();

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[test]
fn prepare_attachment_upload_command_uses_fresh_stat_after_file_edit() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let tmp_dir = std::env::temp_dir().join(format!("multibot-fresh-stat-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp_dir);
    let file_path = tmp_dir.join("notes.txt");
    std::fs::write(&file_path, b"short").unwrap();

    let att = StagedAttachment {
        path: file_path.clone(),
        is_image: false,
        size_bytes: 5,
        inserted_token: "notes.txt".into(),
    };

    // Modify file after staging so its byte length grows from 5 to 31.
    let updated = b"updated-content-longer-than-5!!";
    std::fs::write(&file_path, updated).unwrap();

    let cmd_str = rt
        .block_on(prepare_attachment_upload_command("target-session-99", &att))
        .expect("fresh stat command should succeed");
    assert!(
        cmd_str.contains(&format!("-eq {}", updated.len())),
        "expected command to use fresh size {}, got: {cmd_str}",
        updated.len()
    );
    assert!(
        cmd_str.contains("$HOME/.multibot/uploads/target-session-99"),
        "expected command to target actual session dir, got: {cmd_str}"
    );

    #[cfg(unix)]
    {
        let fake_home = tmp_dir.join("home");
        let _ = std::fs::create_dir_all(&fake_home);
        let remote_path = rt.block_on(async {
            let mut cmd = tokio::process::Command::new("sh");
            cmd.args(["-c", &cmd_str]).env("HOME", &fake_home);
            multibot_core::daemon_deploy::upload_file_to_command(&mut cmd, &file_path)
                .await
                .expect("upload with fresh stat size must succeed")
        });
        assert_eq!(std::fs::read(&remote_path).unwrap(), updated);
    }

    // Replacing the file with a directory or deleting it must fail before upload.
    std::fs::remove_file(&file_path).unwrap();
    assert!(
        rt.block_on(prepare_attachment_upload_command("target-session-99", &att))
            .is_err()
    );
    std::fs::create_dir_all(&file_path).unwrap();
    assert!(
        rt.block_on(prepare_attachment_upload_command("target-session-99", &att))
            .is_err()
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[gpui_kit::test]
fn failed_upload_restores_prompt_quote_and_attachments_safely(cx: &mut TestAppContext) {
    let (handle, view) = open_test_chat_window(cx);
    let sess_a = SessionSummary {
        id: "upload-restore-a".into(),
        agent: AgentKind::Claude,
        title: "Session A".into(),
        ..Default::default()
    };
    let sess_b = SessionSummary {
        id: "upload-restore-b".into(),
        agent: AgentKind::Claude,
        title: "Session B".into(),
        ..Default::default()
    };
    let quote = ReplyQuote {
        message_id: "m1".into(),
        preview: "prior answer".into(),
        full_text: "prior answer".into(),
    };
    let staged = vec![StagedAttachment {
        path: PathBuf::from("/tmp/shot.png"),
        is_image: true,
        size_bytes: 123,
        inserted_token: "shot.png".into(),
    }];

    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.connection.sessions = vec![sess_a.clone(), sess_b.clone()];
            v.selection.selected_session = Some(sess_a.clone());

            // 1. Same session, empty draft -> restores prompt, quote, and attachments.
            v.restore_failed_upload_draft(
                &sess_a,
                "please check shot.png".into(),
                Some(quote.clone()),
                staged.clone(),
                "Attachment upload failed: connection reset".into(),
                window,
                cx,
            );
            assert_eq!(
                v.inputs.prompt_input.read(cx).value().as_ref(),
                "please check shot.png"
            );
            assert_eq!(
                v.chat_state
                    .reply_quote
                    .as_ref()
                    .map(|q| q.message_id.as_str()),
                Some("m1")
            );
            assert_eq!(v.chat_state.pending_attachments.len(), 1);
            assert!(
                v.notice
                    .as_deref()
                    .is_some_and(|n| n.contains("connection reset"))
            );

            // 2. Same session, user already typed a new draft -> must NOT clobber new draft.
            v.chat_state.pending_attachments.clear();
            v.chat_state.reply_quote = None;
            v.inputs
                .prompt_input
                .update(cx, |inp, cx| inp.set_value("new draft", window, cx));
            v.restore_failed_upload_draft(
                &sess_a,
                "old failed prompt".into(),
                Some(quote.clone()),
                staged.clone(),
                "Attachment upload failed: timeout".into(),
                window,
                cx,
            );
            assert_eq!(v.inputs.prompt_input.read(cx).value().as_ref(), "new draft");
            assert!(v.chat_state.pending_attachments.is_empty());
            assert!(v.chat_state.reply_quote.is_none());
            assert!(v.notice.as_deref().is_some_and(|n| n.contains("timeout")));

            // 3. Switched to Session B -> must NOT leak Session A's draft/attachments into Session B.
            v.selection.selected_session = Some(sess_b.clone());
            v.inputs
                .prompt_input
                .update(cx, |inp, cx| inp.set_value("", window, cx));
            v.restore_failed_upload_draft(
                &sess_a,
                "session a prompt".into(),
                Some(quote.clone()),
                staged.clone(),
                "Attachment upload failed: broken pipe".into(),
                window,
                cx,
            );
            assert!(v.inputs.prompt_input.read(cx).value().is_empty());
            assert!(v.chat_state.pending_attachments.is_empty());
            assert!(v.chat_state.reply_quote.is_none());
            assert!(
                v.notice
                    .as_deref()
                    .is_some_and(|n| n.contains("broken pipe"))
            );
        });
    })
    .unwrap();
}
