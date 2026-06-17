//! Tests for `AgentCommand::SetMessages` + `TurnTracker::rebuild` — the core
//! mechanism that powers both `/resume` and `atomcode -c` session restoration.
//!
//! Issue #293: `atomcode -c` could not fully restore model context because
//! `SetMessages` only set `conversation.messages` without rebuilding
//! `turn_tracker`. This meant the context builder fell through to the
//! `build_messages_fallback` path (which doesn't have turn-based windowing)
//! and, more critically, the original `-c` codepath never even sent
//! `SetMessages` to the agent at all.
//!
//! These tests verify:
//!   1. `TurnTracker::rebuild` correctly reconstructs turns from saved messages
//!   2. `build_messages` uses turn-based windowing (not fallback) after rebuild
//!   3. Session round-trip (save → load → SetMessages) preserves full context

use atomcode_core::conversation::message::{Message, MessageContent, Role};
use atomcode_core::conversation::turn::{TurnStatus, TurnTracker};
use atomcode_core::conversation::Conversation;
use atomcode_core::tool::{ToolCall, ToolResult};

// ---------------------------------------------------------------------------
// Helper: build a realistic multi-turn conversation
// ---------------------------------------------------------------------------

/// Build a conversation with `n` complete turns, each consisting of:
///   - User message
///   - Assistant message with a tool call
///   - Tool result
///   - Assistant final text
fn build_multi_turn_conversation(n: usize) -> Conversation {
    let mut conv = Conversation::new();
    for t in 0..n {
        conv.add_user_message(&format!("task {}", t));
        // Simulate assistant with tool call
        let msg_idx = conv.messages.len();
        conv.messages.push(Message {
            role: Role::Assistant,
            content: MessageContent::AssistantWithToolCalls {
                text: Some(format!("working on task {}", t)),
                tool_calls: vec![ToolCall {
                    id: format!("call_{}", t),
                    name: "bash".into(),
                    arguments: format!(r#"{{"command":"echo {}"}}"#, t),
                }],
                reasoning_content: None,
                thinking_blocks: Vec::new(),
            },
                    synthetic: false,
        });
        conv.turn_tracker.on_message_added(msg_idx);

        // Tool result
        let msg_idx = conv.messages.len();
        conv.messages.push(Message {
            role: Role::Tool,
            content: MessageContent::ToolResult(ToolResult {
                call_id: format!("call_{}", t),
                output: format!("output {}", t),
                success: true,
            }),
                    synthetic: false,
        });
        conv.turn_tracker.on_message_added(msg_idx);

        // Final assistant text
        let msg_idx = conv.messages.len();
        conv.messages.push(Message::new(
            Role::Assistant,
            &format!("done with task {}", t),
        ));
        conv.turn_tracker.on_message_added(msg_idx);

        // Complete the turn
        conv.turn_tracker.complete_current();
    }
    conv
}

// ---------------------------------------------------------------------------
// Test 1: TurnTracker::rebuild correctly reconstructs turn structure
// ---------------------------------------------------------------------------

#[test]
fn rebuild_produces_correct_turn_count_for_multi_turn_conversation() {
    let conv = build_multi_turn_conversation(3);
    assert_eq!(conv.messages.len(), 12); // 3 turns × 4 msgs

    let tracker = TurnTracker::rebuild(&conv.messages);
    assert_eq!(tracker.turns.len(), 3, "should have 3 turns");
}

#[test]
fn rebuild_sets_completed_status_for_all_but_last_turn() {
    let conv = build_multi_turn_conversation(3);
    let tracker = TurnTracker::rebuild(&conv.messages);

    assert_eq!(
        tracker.turns[0].status,
        TurnStatus::Completed,
        "first turn should be Completed"
    );
    assert_eq!(
        tracker.turns[1].status,
        TurnStatus::Completed,
        "middle turn should be Completed"
    );
    // Last turn stays Active per rebuild convention
    assert_eq!(
        tracker.turns[2].status,
        TurnStatus::Active,
        "last turn should be Active (rebuild convention)"
    );
}

#[test]
fn rebuild_correctly_tracks_message_indices_per_turn() {
    let conv = build_multi_turn_conversation(3);
    let tracker = TurnTracker::rebuild(&conv.messages);

    // Turn 0: msgs 0-3 (user, assistant+tool_call, tool_result, assistant)
    assert_eq!(tracker.turns[0].start_idx, 0);
    assert_eq!(tracker.turns[0].msg_count, 4);
    assert_eq!(tracker.turns[0].end_idx(), 4);

    // Turn 1: msgs 4-7
    assert_eq!(tracker.turns[1].start_idx, 4);
    assert_eq!(tracker.turns[1].msg_count, 4);
    assert_eq!(tracker.turns[1].end_idx(), 8);

    // Turn 2: msgs 8-11
    assert_eq!(tracker.turns[2].start_idx, 8);
    assert_eq!(tracker.turns[2].msg_count, 4);
    assert_eq!(tracker.turns[2].end_idx(), 12);
}

// ---------------------------------------------------------------------------
// Test 2: build_messages uses turn-based windowing after SetMessages + rebuild
// ---------------------------------------------------------------------------

#[test]
fn context_builds_with_turn_tracking_after_set_messages() {
    use atomcode_core::config::provider::ProviderConfig;
    use atomcode_core::ctx::CtxBuilder;
    use atomcode_core::ctx::DefaultCtx;

    let conv = build_multi_turn_conversation(3);

    // Simulate the SetMessages flow: take messages, rebuild tracker
    let messages = conv.messages.clone();
    let turn_tracker = TurnTracker::rebuild(&messages);

    let restored_conv = Conversation {
        messages,
        stream_buffer: None,
        tool_call_buffer: None,
        turn_tracker,
        cold_summaries: Vec::new(),
    };

    // Verify the restored conversation has turns tracked
    assert!(
        !restored_conv.turn_tracker.turns.is_empty(),
        "restored conversation must have turn_tracker populated"
    );
    assert_eq!(restored_conv.turn_tracker.turns.len(), 3);

    // Now verify that build_messages uses the turn-based path (not fallback)
    let provider_config = ProviderConfig {
        provider_type: "test".into(),
        api_key: None,
        model: "test-model".into(),
        base_url: None,
        system_prompt: None,
        user_agent: None,
        context_window: 128_000,
        max_tokens: None,
        thinking_type: None,
        thinking_keep: None,
        reasoning_history: None,
        reasoning_effort: None,
        thinking_enabled: None,
        thinking_budget: None,
        skip_tls_verify: false,
        ephemeral: true,

};
    let ctx_builder = DefaultCtx::new(&provider_config);

    let (built_msgs, stats) =
        ctx_builder.build_messages(&restored_conv, "You are a helpful assistant.", "");

    // With turns tracked, build_messages should produce a non-trivial context
    // (system + all messages that fit in the budget)
    assert!(
        built_msgs.len() > 1,
        "should have system + at least some conversation messages, got {}",
        built_msgs.len()
    );

    // First message should be system prompt
    assert!(matches!(built_msgs[0].role, Role::System));

    // All user messages should be present (128k context window is large enough)
    let user_msgs: Vec<_> = built_msgs
        .iter()
        .filter(|m| matches!(m.role, Role::User))
        .collect();
    assert_eq!(
        user_msgs.len(),
        3,
        "all 3 user messages should be in the built context"
    );

    // Verify that stats reflect turn-based windowing (not fallback)
    // Fallback path returns default ContextStats; turn-based path populates properly.
    // The key invariant: total_messages > 0 when we have turn-tracked messages.
    assert!(
        stats.total_messages > 0,
        "context stats should reflect the tracked messages"
    );
}

// ---------------------------------------------------------------------------
// Test 3: build_messages falls back when turn_tracker is empty (the OLD bug)
// ---------------------------------------------------------------------------

#[test]
fn context_uses_fallback_when_turn_tracker_is_empty() {
    use atomcode_core::config::provider::ProviderConfig;
    use atomcode_core::ctx::CtxBuilder;
    use atomcode_core::ctx::DefaultCtx;

    let conv = build_multi_turn_conversation(3);

    // Simulate the OLD buggy SetMessages flow: set messages but NO turn_tracker rebuild
    let buggy_conv = Conversation {
        messages: conv.messages.clone(),
        stream_buffer: None,
        tool_call_buffer: None,
        turn_tracker: TurnTracker::new(), // Empty tracker — the bug!
        cold_summaries: Vec::new(),
    };

    let provider_config = ProviderConfig {
        provider_type: "test".into(),
        api_key: None,
        model: "test-model".into(),
        base_url: None,
        system_prompt: None,
        user_agent: None,
        context_window: 128_000,
        max_tokens: None,
        thinking_type: None,
        thinking_keep: None,
        reasoning_history: None,
        reasoning_effort: None,
        thinking_enabled: None,
        thinking_budget: None,
        skip_tls_verify: false,
        ephemeral: true,

};
    let ctx_builder = DefaultCtx::new(&provider_config);

    let (built_msgs, _stats) =
        ctx_builder.build_messages(&buggy_conv, "You are a helpful assistant.", "");

    // Even with fallback, messages should still be present
    // (fallback doesn't lose messages, just uses a less precise windowing strategy)
    assert!(
        built_msgs.len() > 1,
        "fallback path should still produce conversation messages"
    );
}

// ---------------------------------------------------------------------------
// Test 4: Session round-trip: save → load → SetMessages preserves context
// ---------------------------------------------------------------------------

#[test]
fn session_round_trip_preserves_messages_and_turn_structure() {
    use atomcode_core::session::{Session, SessionManager};

    let tmp = tempfile::tempdir().unwrap();
    let working_dir = tmp.path().to_path_buf();

    // Build a conversation with 2 turns
    let conv = build_multi_turn_conversation(2);

    // Create and save a session
    let mut session = Session::new(working_dir.clone());
    session.messages = conv.messages.clone();
    session.rename("test-session".into());

    // Save to disk
    let session_manager = SessionManager::new(&working_dir);
    session_manager.save(&session).unwrap();

    // Load from disk
    let loaded = session_manager.load(&session.id).unwrap();

    assert_eq!(
        loaded.messages.len(),
        conv.messages.len(),
        "loaded session should have the same number of messages"
    );

    // Rebuild turn tracker from loaded messages (simulating SetMessages)
    let tracker = TurnTracker::rebuild(&loaded.messages);
    assert_eq!(
        tracker.turns.len(),
        2,
        "rebuild from loaded session should produce 2 turns"
    );

    // Verify message content preservation
    for (i, original) in conv.messages.iter().enumerate() {
        assert_eq!(
            loaded.messages[i].role, original.role,
            "message {} role mismatch",
            i
        );
    }
}

// ---------------------------------------------------------------------------
// Test 5: TurnTracker::rebuild handles conversations with tool calls
// ---------------------------------------------------------------------------

#[test]
fn rebuild_handles_tool_call_turns_correctly() {
    // Build a conversation with tool calls that span multiple messages per turn
    let messages = vec![
        Message::new(Role::User, "search for foo"),
        Message {
            role: Role::Assistant,
            content: MessageContent::AssistantWithToolCalls {
                text: Some("searching...".into()),
                tool_calls: vec![
                    ToolCall {
                        id: "c1".into(),
                        name: "grep".into(),
                        arguments: r#"{"pattern":"foo"}"#.into(),
                    },
                    ToolCall {
                        id: "c2".into(),
                        name: "read_file".into(),
                        arguments: r#"{"file_path":"/tmp/x.rs"}"#.into(),
                    },
                ],
                reasoning_content: None,
                thinking_blocks: Vec::new(),
            },
                    synthetic: false,
        },
        Message {
            role: Role::Tool,
            content: MessageContent::ToolResult(ToolResult {
                call_id: "c1".into(),
                output: "found foo".into(),
                success: true,
            }),
                    synthetic: false,
        },
        Message {
            role: Role::Tool,
            content: MessageContent::ToolResult(ToolResult {
                call_id: "c2".into(),
                output: "file contents".into(),
                success: true,
            }),
                    synthetic: false,
        },
        Message::new(Role::Assistant, "Here's what I found..."),
        Message::new(Role::User, "now edit it"),
        Message {
            role: Role::Assistant,
            content: MessageContent::AssistantWithToolCalls {
                text: None,
                tool_calls: vec![ToolCall {
                    id: "c3".into(),
                    name: "edit_file".into(),
                    arguments: r#"{"file_path":"/tmp/x.rs","old_string":"foo","new_string":"bar"}"#.into(),
                }],
                reasoning_content: None,
                thinking_blocks: Vec::new(),
            },
                    synthetic: false,
        },
        Message {
            role: Role::Tool,
            content: MessageContent::ToolResult(ToolResult {
                call_id: "c3".into(),
                output: "edit applied".into(),
                success: true,
            }),
                    synthetic: false,
        },
    ];

    let tracker = TurnTracker::rebuild(&messages);

    assert_eq!(tracker.turns.len(), 2, "should have 2 turns");

    // Turn 0: msgs 0-4 (user, assistant+2 calls, 2 tool results, assistant)
    assert_eq!(tracker.turns[0].start_idx, 0);
    assert_eq!(tracker.turns[0].msg_count, 5);
    assert_eq!(tracker.turns[0].status, TurnStatus::Completed);

    // Turn 1: msgs 5-7 (user, assistant+1 call, 1 tool result)
    assert_eq!(tracker.turns[1].start_idx, 5);
    assert_eq!(tracker.turns[1].msg_count, 3);
    // Last turn stays Active per rebuild convention
    assert_eq!(tracker.turns[1].status, TurnStatus::Active);
}

// ---------------------------------------------------------------------------
// Test 6: Empty messages edge case
// ---------------------------------------------------------------------------

#[test]
fn set_messages_with_empty_list_produces_empty_tracker() {
    let messages: Vec<Message> = Vec::new();
    let tracker = TurnTracker::rebuild(&messages);
    assert!(tracker.turns.is_empty());
}

#[test]
fn set_messages_with_single_user_message_produces_one_active_turn() {
    let messages = vec![Message::new(Role::User, "hello")];
    let tracker = TurnTracker::rebuild(&messages);
    assert_eq!(tracker.turns.len(), 1);
    assert_eq!(tracker.turns[0].start_idx, 0);
    assert_eq!(tracker.turns[0].msg_count, 1);
    assert_eq!(tracker.turns[0].status, TurnStatus::Active);
}

// ---------------------------------------------------------------------------
// Test 7: Restored context contains the same user messages as original
//         (the core invariant for issue #293)
// ---------------------------------------------------------------------------

#[test]
fn restored_context_contains_same_user_messages_as_original() {
    use atomcode_core::config::provider::ProviderConfig;
    use atomcode_core::ctx::CtxBuilder;
    use atomcode_core::ctx::DefaultCtx;

    let provider_config = ProviderConfig {
        provider_type: "test".into(),
        api_key: None,
        model: "test-model".into(),
        base_url: None,
        system_prompt: None,
        user_agent: None,
        context_window: 128_000,
        max_tokens: None,
        thinking_type: None,
        thinking_keep: None,
        reasoning_history: None,
        reasoning_effort: None,
        thinking_enabled: None,
        thinking_budget: None,
        skip_tls_verify: false,
        ephemeral: true,

};
    let ctx_builder = DefaultCtx::new(&provider_config);
    let system_prompt = "You are a helpful assistant.";

    // Build original conversation and get its context
    let original_conv = build_multi_turn_conversation(3);
    let (original_msgs, _) = ctx_builder.build_messages(&original_conv, system_prompt, "");

    // Simulate the RESTORED conversation via SetMessages + rebuild
    let messages = original_conv.messages.clone();
    let turn_tracker = TurnTracker::rebuild(&messages);
    let restored_conv = Conversation {
        messages,
        stream_buffer: None,
        tool_call_buffer: None,
        turn_tracker,
        cold_summaries: Vec::new(),
    };
    let (restored_msgs, _) = ctx_builder.build_messages(&restored_conv, system_prompt, "");

    // Extract user messages from both
    let original_user_texts: Vec<&str> = original_msgs
        .iter()
        .filter(|m| matches!(m.role, Role::User))
        .filter_map(|m| m.text())
        .collect();

    let restored_user_texts: Vec<&str> = restored_msgs
        .iter()
        .filter(|m| matches!(m.role, Role::User))
        .filter_map(|m| m.text())
        .collect();

    // The core invariant: after restore via SetMessages + rebuild,
    // the LLM sees the same user messages as the original conversation.
    // This is what was broken in issue #293: `-c` showed messages on
    // screen but the LLM context was empty.
    assert_eq!(
        original_user_texts, restored_user_texts,
        "restored context must contain the same user messages as the original"
    );
}

// ---------------------------------------------------------------------------
// Test 8: Context with empty tracker loses turn-based windowing precision
//         (demonstrates the value of the TurnTracker::rebuild fix)
// ---------------------------------------------------------------------------

#[test]
fn empty_turn_tracker_loses_windowing_precision() {
    use atomcode_core::config::provider::ProviderConfig;
    use atomcode_core::ctx::CtxBuilder;
    use atomcode_core::ctx::DefaultCtx;

    let provider_config = ProviderConfig {
        provider_type: "test".into(),
        api_key: None,
        model: "test-model".into(),
        base_url: None,
        system_prompt: None,
        user_agent: None,
        context_window: 128_000,
        max_tokens: None,
        thinking_type: None,
        thinking_keep: None,
        reasoning_history: None,
        reasoning_effort: None,
        thinking_enabled: None,
        thinking_budget: None,
        skip_tls_verify: false,
        ephemeral: true,

    };
    let ctx_builder = DefaultCtx::new(&provider_config);

    let conv = build_multi_turn_conversation(3);

    // With turn tracker rebuilt (the fix)
    let messages = conv.messages.clone();
    let tracker = TurnTracker::rebuild(&messages);
    let restored_conv = Conversation {
        messages: messages.clone(),
        stream_buffer: None,
        tool_call_buffer: None,
        turn_tracker: tracker,
        cold_summaries: Vec::new(),
    };
    let (_, stats_with_tracker) = ctx_builder
        .build_messages(&restored_conv, "You are a helpful assistant.", "");

    // Without turn tracker (the old bug)
    let buggy_conv = Conversation {
        messages,
        stream_buffer: None,
        tool_call_buffer: None,
        turn_tracker: TurnTracker::new(),
        cold_summaries: Vec::new(),
    };
    let (_, stats_without_tracker) = ctx_builder
        .build_messages(&buggy_conv, "You are a helpful assistant.", "");

    // With tracker: total_messages should be populated from turn-based windowing
    // Without tracker: total_messages may be 0 (default) from the fallback path
    // The key difference: the tracker-enabled path has better windowing info
    assert!(
        stats_with_tracker.total_messages > 0,
        "turn-tracked context should report total_messages > 0"
    );

    // Both should produce messages, but the tracked version should be at least
    // as good (and typically better) at including all relevant context
    assert!(
        stats_with_tracker.total_messages >= stats_without_tracker.total_messages,
        "turn-tracked windowing should include at least as many messages as fallback"
    );
}
