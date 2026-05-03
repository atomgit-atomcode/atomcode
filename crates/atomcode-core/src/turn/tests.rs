//! Tests for TurnRunner, discipline logic, and approval flow.

use anyhow::Result;
use async_trait::async_trait;
use futures::stream;
use futures::Stream;
use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::provider::ProviderConfig;
use crate::config::Config;
use crate::conversation::message::Message;
use crate::conversation::Conversation;
use crate::provider::LlmProvider;
use crate::stream::{StreamEvent, TokenUsage};
use crate::tool::{
    ApprovalRequirement, PermissionDecision, Tool, ToolCall, ToolContext, ToolDef, ToolRegistry,
    ToolResult,
};

use super::event::{TurnEvent, TurnResult};
use super::permission::{AutoPermissionDecider, AutoPermissionMode, InteractivePermissionDecider};
use super::runner::TurnRunner;

// ---------------------------------------------------------------------------
// Test helpers: Mock LlmProvider
// ---------------------------------------------------------------------------

/// A mock LLM provider that returns a predefined sequence of StreamEvents.
struct MockProvider {
    events: Vec<StreamEvent>,
}

impl MockProvider {
    fn text_only(text: &str) -> Self {
        Self {
            events: vec![
                StreamEvent::Delta(text.to_string()),
                StreamEvent::Usage(TokenUsage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    cached_tokens: 0,
                }),
                StreamEvent::Done { truncated: false },
            ],
        }
    }

    fn with_tool_call(tool_name: &str, args: &str) -> Self {
        Self {
            events: vec![
                StreamEvent::ToolCallStart {
                    id: "call_1".to_string(),
                    name: tool_name.to_string(),
                },
                StreamEvent::ToolCallDelta(args.to_string()),
                StreamEvent::ToolCallDone(ToolCall {
                    id: "call_1".to_string(),
                    name: tool_name.to_string(),
                    arguments: args.to_string(),
                }),
                StreamEvent::Usage(TokenUsage {
                    prompt_tokens: 10,
                    completion_tokens: 8,
                    cached_tokens: 0,
                }),
                StreamEvent::Done { truncated: false },
            ],
        }
    }

    fn with_error(msg: &str) -> Self {
        Self {
            events: vec![StreamEvent::Error(msg.to_string())],
        }
    }

    fn empty() -> Self {
        Self {
            events: vec![StreamEvent::Done { truncated: false }],
        }
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: Option<&[ToolDef]>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let events: Vec<Result<StreamEvent>> = self.events.iter().cloned().map(Ok).collect();
        Ok(Box::pin(stream::iter(events)))
    }

    fn model_name(&self) -> &str {
        "mock-model"
    }
}

// ---------------------------------------------------------------------------
// Test helpers: Mock Tools
// ---------------------------------------------------------------------------

/// A simple tool that always succeeds and returns its name.
struct EchoTool {
    name: &'static str,
}

#[async_trait]
impl Tool for EchoTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: self.name,
            description: format!("Echo tool: {}", self.name),
            parameters: serde_json::json!({"type": "object"}),
        }
    }
    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::AutoApprove
    }
    async fn execute(&self, args: &str, _ctx: &ToolContext) -> Result<ToolResult> {
        Ok(ToolResult {
            call_id: String::new(),
            output: format!("executed {} with {}", self.name, args),
            success: true,
        })
    }
}

/// A tool that requires user approval.
struct DangerousTool;

#[async_trait]
impl Tool for DangerousTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "dangerous",
            description: "Requires approval".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }
    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::RequireApproval("This is dangerous".to_string())
    }
    async fn execute(&self, _args: &str, _ctx: &ToolContext) -> Result<ToolResult> {
        Ok(ToolResult {
            call_id: String::new(),
            output: "dangerous action done".to_string(),
            success: true,
        })
    }
}

/// A tool that only requires approval when it can inspect the current context.
struct ContextDangerousTool;

#[async_trait]
impl Tool for ContextDangerousTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "context_dangerous",
            description: "Requires context-aware approval".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }
    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::AutoApprove
    }
    fn approval_with_context(&self, _args: &str, _ctx: &ToolContext) -> ApprovalRequirement {
        ApprovalRequirement::RequireApproval("Needs context-aware confirmation".to_string())
    }
    async fn execute(&self, _args: &str, _ctx: &ToolContext) -> Result<ToolResult> {
        Ok(ToolResult {
            call_id: String::new(),
            output: "context-aware action done".to_string(),
            success: true,
        })
    }
}

// ---------------------------------------------------------------------------
// Test helpers: Config / Context
// ---------------------------------------------------------------------------

fn test_config() -> Config {
    let mut providers = HashMap::new();
    providers.insert(
        "test".to_string(),
        ProviderConfig {
            provider_type: "mock".to_string(),
            api_key: None,
            model: "mock-model".to_string(),
            base_url: None,
            system_prompt: None,
            user_agent: None,
            context_window: 16000,
            max_tokens: None,
            thinking_type: None,
            thinking_keep: None,
            reasoning_history: None,
            thinking_enabled: None,
            thinking_budget: None,
            skip_tls_verify: false,
            ephemeral: false,
        },
    );
    Config {
        default_provider: "test".to_string(),
        default_workdir: None,
        providers,
        datalog: Default::default(),
        notifications: Default::default(),
        auto_update: false,
        reflection_cadence: 7,
        telemetry: Default::default(),
        lsp: Default::default(),
        auto_commit: false,
        subagent: Default::default(),
    }
}

fn test_context() -> ToolContext {
    ToolContext::new(PathBuf::from("/tmp/test"))
}

fn make_runner(
    provider: MockProvider,
    tools: ToolRegistry,
    permission: Box<dyn super::permission::PermissionDecider>,
) -> TurnRunner {
    // Tests don't set up real ProviderConfig, so construct a DefaultCtx
    // directly with a generous window (matches test_config's implicit budget).
    let test_provider = crate::config::provider::ProviderConfig {
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
        thinking_enabled: None,
        thinking_budget: None,
        skip_tls_verify: false,
        ephemeral: true,
    };
    let test_ctx: std::sync::Arc<dyn crate::ctx::CtxBuilder> =
        std::sync::Arc::new(crate::ctx::DefaultCtx::new(&test_provider));

    TurnRunner {
        provider: std::sync::Arc::new(provider),
        tools: std::sync::Arc::new(tools),
        context: test_context(),
        config: test_config(),
        ctx: test_ctx,
        permission,
        recently_edited_files: Vec::new(),
        recent_calls: Vec::new(),
        file_read_counts: std::collections::HashMap::new(),
        hook_executor: std::sync::Arc::new(
            crate::hook::executor::HookExecutor::empty()
        ),
    }
}

fn auto_bypass() -> Box<dyn super::permission::PermissionDecider> {
    Box::new(AutoPermissionDecider::new(AutoPermissionMode::BypassAll))
}

fn auto_deny() -> Box<dyn super::permission::PermissionDecider> {
    Box::new(AutoPermissionDecider::new(AutoPermissionMode::DenyAll))
}

// ===========================================================================
// 1. TurnRunner tests
// ===========================================================================

#[tokio::test]
async fn test_turn_runner_text_only_response() {
    let mut runner = make_runner(
        MockProvider::text_only("Hello, world!"),
        ToolRegistry::new(),
        auto_bypass(),
    );
    let mut conv = Conversation::new();
    conv.add_user_message("Hi");
    let (tx, _rx) = mpsc::unbounded_channel();

    let result = runner
        .run(&mut conv, "system", &tx, CancellationToken::new())
        .await;

    match result {
        TurnResult::Responded { text, tokens, .. } => {
            assert_eq!(text, "Hello, world!");
            assert!(tokens > 0);
        }
        other => panic!("Expected Responded, got {:?}", other),
    }
}

#[tokio::test]
async fn test_turn_runner_empty_response_is_failure() {
    let mut runner = make_runner(MockProvider::empty(), ToolRegistry::new(), auto_bypass());
    let mut conv = Conversation::new();
    conv.add_user_message("Hi");
    let (tx, _rx) = mpsc::unbounded_channel();

    let result = runner
        .run(&mut conv, "system", &tx, CancellationToken::new())
        .await;

    match result {
        TurnResult::Failed(msg) => {
            assert!(msg.contains("empty response"));
        }
        other => panic!("Expected Failed, got {:?}", other),
    }
}

#[tokio::test]
async fn test_turn_runner_emits_text_delta_events() {
    let mut runner = make_runner(
        MockProvider::text_only("Hello"),
        ToolRegistry::new(),
        auto_bypass(),
    );
    let mut conv = Conversation::new();
    conv.add_user_message("Hi");
    let (tx, mut rx) = mpsc::unbounded_channel();

    runner
        .run(&mut conv, "system", &tx, CancellationToken::new())
        .await;

    // Collect events
    drop(tx);
    let mut got_text_delta = false;
    while let Some(event) = rx.recv().await {
        if matches!(event, TurnEvent::TextDelta(_)) {
            got_text_delta = true;
        }
    }
    assert!(got_text_delta, "Expected at least one TextDelta event");
}

#[tokio::test]
async fn test_turn_runner_executes_tool_call() {
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(EchoTool { name: "grep" })).await;

    let provider = MockProvider::with_tool_call("grep", r#"{"pattern":"foo"}"#);
    let mut runner = make_runner(provider, tools, auto_bypass());
    let mut conv = Conversation::new();
    conv.add_user_message("search for foo");
    let (tx, _rx) = mpsc::unbounded_channel();

    let result = runner
        .run(&mut conv, "system", &tx, CancellationToken::new())
        .await;

    match result {
        TurnResult::UsedTools { tool_count, .. } => {
            assert_eq!(tool_count, 1);
        }
        other => panic!("Expected UsedTools, got {:?}", other),
    }

    // Verify tool result was added to conversation
    let last = conv.messages.last().unwrap();
    assert!(matches!(
        last.content,
        crate::conversation::message::MessageContent::ToolResult(_)
    ));
}

#[tokio::test]
async fn test_turn_runner_emits_tool_events() {
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(EchoTool { name: "grep" })).await;

    let provider = MockProvider::with_tool_call("grep", r#"{"pattern":"foo"}"#);
    let mut runner = make_runner(provider, tools, auto_bypass());
    let mut conv = Conversation::new();
    conv.add_user_message("search");
    let (tx, mut rx) = mpsc::unbounded_channel();

    runner
        .run(&mut conv, "system", &tx, CancellationToken::new())
        .await;

    drop(tx);
    let mut got_started = false;
    let mut got_result = false;
    while let Some(event) = rx.recv().await {
        match event {
            TurnEvent::ToolCallStarted { name, .. } if name == "grep" => got_started = true,
            TurnEvent::ToolCallResult { name, success, .. } if name == "grep" => {
                got_result = true;
                assert!(success);
            }
            _ => {}
        }
    }
    assert!(got_started, "Expected ToolCallStarted event");
    assert!(got_result, "Expected ToolCallResult event");
}

#[tokio::test]
async fn test_turn_runner_unknown_tool_returns_error_result() {
    // Provider asks to call a tool that isn't registered
    let provider = MockProvider::with_tool_call("nonexistent", "{}");
    let mut runner = make_runner(provider, ToolRegistry::new(), auto_bypass());
    let mut conv = Conversation::new();
    conv.add_user_message("do something");
    let (tx, _rx) = mpsc::unbounded_channel();

    let result = runner
        .run(&mut conv, "system", &tx, CancellationToken::new())
        .await;

    match result {
        TurnResult::UsedTools { tool_count, .. } => {
            assert_eq!(tool_count, 1);
            // Last message should be a failed tool result
            let last = conv.messages.last().unwrap();
            if let crate::conversation::message::MessageContent::ToolResult(ref r) = last.content {
                assert!(!r.success);
                assert!(r.output.contains("unknown tool"));
            } else {
                panic!("Expected ToolResult message");
            }
        }
        other => panic!("Expected UsedTools, got {:?}", other),
    }
}

#[tokio::test]
async fn test_turn_runner_handles_stream_error() {
    let provider = MockProvider::with_error("API rate limit exceeded");
    let mut runner = make_runner(provider, ToolRegistry::new(), auto_bypass());
    let mut conv = Conversation::new();
    conv.add_user_message("Hi");
    let (tx, _rx) = mpsc::unbounded_channel();

    let result = runner
        .run(&mut conv, "system", &tx, CancellationToken::new())
        .await;

    match result {
        TurnResult::Failed(e) => {
            assert!(e.contains("rate limit"), "Error was: {}", e);
        }
        other => panic!("Expected Failed, got {:?}", other),
    }
}

#[tokio::test]
async fn test_turn_runner_cancellation() {
    let provider = MockProvider::text_only("This should be cancelled");
    let mut runner = make_runner(provider, ToolRegistry::new(), auto_bypass());
    let mut conv = Conversation::new();
    conv.add_user_message("Hi");
    let (tx, _rx) = mpsc::unbounded_channel();

    let cancel = CancellationToken::new();
    cancel.cancel(); // Cancel immediately

    let result = runner.run(&mut conv, "system", &tx, cancel).await;

    assert!(matches!(result, TurnResult::Cancelled));
}

// ===========================================================================
// 2. Permission / Approval tests
// ===========================================================================

#[tokio::test]
async fn test_turn_runner_auto_deny_blocks_dangerous_tool() {
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(DangerousTool)).await;

    let provider = MockProvider::with_tool_call("dangerous", "{}");
    let mut runner = make_runner(provider, tools, auto_deny());
    let mut conv = Conversation::new();
    conv.add_user_message("do dangerous thing");
    let (tx, _rx) = mpsc::unbounded_channel();

    let result = runner
        .run(&mut conv, "system", &tx, CancellationToken::new())
        .await;

    // Tool should be denied, but turn still returns UsedTools (with denied result)
    match result {
        TurnResult::UsedTools { .. } => {
            let last = conv.messages.last().unwrap();
            if let crate::conversation::message::MessageContent::ToolResult(ref r) = last.content {
                assert!(!r.success);
                assert!(r.output.contains("denied"));
            } else {
                panic!("Expected ToolResult");
            }
        }
        other => panic!("Expected UsedTools, got {:?}", other),
    }
}

#[tokio::test]
async fn test_turn_runner_auto_bypass_allows_dangerous_tool() {
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(DangerousTool)).await;

    let provider = MockProvider::with_tool_call("dangerous", "{}");
    let mut runner = make_runner(provider, tools, auto_bypass());
    let mut conv = Conversation::new();
    conv.add_user_message("do dangerous thing");
    let (tx, _rx) = mpsc::unbounded_channel();

    let result = runner
        .run(&mut conv, "system", &tx, CancellationToken::new())
        .await;

    match result {
        TurnResult::UsedTools { .. } => {
            let last = conv.messages.last().unwrap();
            if let crate::conversation::message::MessageContent::ToolResult(ref r) = last.content {
                assert!(r.success);
                assert!(r.output.contains("dangerous action done"));
            } else {
                panic!("Expected ToolResult");
            }
        }
        other => panic!("Expected UsedTools, got {:?}", other),
    }
}

#[tokio::test]
async fn test_turn_runner_interactive_approval_allow() {
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(DangerousTool)).await;

    let (req_tx, mut req_rx) = mpsc::unbounded_channel();
    let (resp_tx, resp_rx) = mpsc::unbounded_channel();
    let store = std::sync::Arc::new(std::sync::RwLock::new(crate::tool::PermissionStore::new()));
    let permission = Box::new(InteractivePermissionDecider::new(req_tx, resp_rx, store));

    let provider = MockProvider::with_tool_call("dangerous", "{}");
    let mut runner = make_runner(provider, tools, permission);
    let mut conv = Conversation::new();
    conv.add_user_message("do it");
    let (tx, _rx) = mpsc::unbounded_channel();

    // Spawn responder: auto-approve when request arrives
    tokio::spawn(async move {
        if let Some(_req) = req_rx.recv().await {
            resp_tx.send(PermissionDecision::Allow).unwrap();
        }
    });

    let result = runner
        .run(&mut conv, "system", &tx, CancellationToken::new())
        .await;

    match result {
        TurnResult::UsedTools { .. } => {
            let last = conv.messages.last().unwrap();
            if let crate::conversation::message::MessageContent::ToolResult(ref r) = last.content {
                assert!(r.success, "Tool should have been approved and executed");
            } else {
                panic!("Expected ToolResult");
            }
        }
        other => panic!("Expected UsedTools, got {:?}", other),
    }
}

#[tokio::test]
async fn test_turn_runner_interactive_approval_deny() {
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(DangerousTool)).await;

    let (req_tx, mut req_rx) = mpsc::unbounded_channel();
    let (resp_tx, resp_rx) = mpsc::unbounded_channel();
    let store = std::sync::Arc::new(std::sync::RwLock::new(crate::tool::PermissionStore::new()));
    let permission = Box::new(InteractivePermissionDecider::new(req_tx, resp_rx, store));

    let provider = MockProvider::with_tool_call("dangerous", "{}");
    let mut runner = make_runner(provider, tools, permission);
    let mut conv = Conversation::new();
    conv.add_user_message("do it");
    let (tx, _rx) = mpsc::unbounded_channel();

    // Spawn responder: deny when request arrives
    tokio::spawn(async move {
        if let Some(_req) = req_rx.recv().await {
            resp_tx.send(PermissionDecision::Deny).unwrap();
        }
    });

    let result = runner
        .run(&mut conv, "system", &tx, CancellationToken::new())
        .await;

    match result {
        TurnResult::UsedTools { .. } => {
            let last = conv.messages.last().unwrap();
            if let crate::conversation::message::MessageContent::ToolResult(ref r) = last.content {
                assert!(!r.success, "Tool should have been denied");
                assert!(r.output.contains("denied"));
            } else {
                panic!("Expected ToolResult");
            }
        }
        other => panic!("Expected UsedTools, got {:?}", other),
    }
}

#[tokio::test]
async fn test_turn_runner_uses_context_aware_approval() {
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(ContextDangerousTool)).await;

    let provider = MockProvider::with_tool_call("context_dangerous", "{}");
    let mut runner = make_runner(provider, tools, auto_deny());
    let mut conv = Conversation::new();
    conv.add_user_message("do it");
    let (tx, _rx) = mpsc::unbounded_channel();

    let result = runner
        .run(&mut conv, "system", &tx, CancellationToken::new())
        .await;

    match result {
        TurnResult::UsedTools { .. } => {
            let last = conv.messages.last().unwrap();
            if let crate::conversation::message::MessageContent::ToolResult(ref r) = last.content {
                assert!(!r.success, "Tool should have been denied");
                assert!(r.output.contains("denied"));
            } else {
                panic!("Expected ToolResult");
            }
        }
        other => panic!("Expected UsedTools, got {:?}", other),
    }
}

// ===========================================================================
// 3. Discipline logic tests (step limit, reminders)
// ===========================================================================

#[test]
fn test_check_step_limit_under_limit() {
    // step limit = 50 + 5*0 = 50
    assert!(!check_step_limit_impl(30, 0));
}

#[test]
fn test_check_step_limit_at_limit() {
    // step limit = 50 + 5*0 = 50
    assert!(check_step_limit_impl(50, 0));
}

#[test]
fn test_check_step_limit_with_edits_extends() {
    // step limit = 50 + 5*5 = 75
    assert!(!check_step_limit_impl(70, 5));
    assert!(check_step_limit_impl(75, 5));
}

#[test]
fn test_check_step_limit_hard_cap_100() {
    // step limit = 50 + 5*20 = 150, min(150, 100) = 100
    assert!(!check_step_limit_impl(99, 20));
    assert!(check_step_limit_impl(100, 20));
}

/// Standalone reimplementation of check_step_limit logic for unit testing.
fn check_step_limit_impl(tool_call_count: usize, files_edited_count: usize) -> bool {
    let dynamic_limit = 50 + (5 * files_edited_count);
    let hard_limit = dynamic_limit.min(100);
    tool_call_count >= hard_limit
}

// ---------------------------------------------------------------------------
// Turn limit tests (mirror of check_turn_limit in agent/discipline.rs)
// ---------------------------------------------------------------------------

#[test]
fn test_check_turn_limit_none_unbounded() {
    // No cap set → never stops regardless of turn_count.
    assert!(!check_turn_limit_impl(0, None));
    assert!(!check_turn_limit_impl(1, None));
    assert!(!check_turn_limit_impl(1_000_000, None));
}

#[test]
fn test_check_turn_limit_under_limit() {
    // cap = 3: turns 0, 1, 2 all still "under" → loop continues.
    assert!(!check_turn_limit_impl(0, Some(3)));
    assert!(!check_turn_limit_impl(1, Some(3)));
    assert!(!check_turn_limit_impl(2, Some(3)));
}

#[test]
fn test_check_turn_limit_at_or_over_limit() {
    // cap = 3: at turn_count == 3, loop should stop (before running a 4th turn).
    assert!(check_turn_limit_impl(3, Some(3)));
    assert!(check_turn_limit_impl(4, Some(3)));
    assert!(check_turn_limit_impl(100, Some(3)));
}

#[test]
fn test_check_turn_limit_zero_stops_immediately() {
    // Degenerate but valid: cap = 0 means "run zero turns".
    assert!(check_turn_limit_impl(0, Some(0)));
}

/// Standalone reimplementation of check_turn_limit for unit testing.
/// Must match the formula used by AgentLoop::check_turn_limit in
/// agent/discipline.rs. If you change one, change both.
fn check_turn_limit_impl(turn_count: usize, max_turns: Option<usize>) -> bool {
    max_turns.map_or(false, |m| turn_count >= m)
}

#[test]
fn test_discipline_reminder_triggers_every_4_steps() {
    // Reminders should fire at steps 4, 8, 12, 16...
    assert!(should_inject_reminder(4));
    assert!(should_inject_reminder(8));
    assert!(should_inject_reminder(12));
    assert!(!should_inject_reminder(3));
    assert!(!should_inject_reminder(5));
    assert!(!should_inject_reminder(0));
}

fn should_inject_reminder(tool_call_count: usize) -> bool {
    tool_call_count > 0 && tool_call_count % 4 == 0
}

// ===========================================================================
// 4. Token usage tracking
// ===========================================================================

#[tokio::test]
async fn test_turn_runner_reports_token_usage() {
    let mut runner = make_runner(
        MockProvider::text_only("Hello"),
        ToolRegistry::new(),
        auto_bypass(),
    );
    let mut conv = Conversation::new();
    conv.add_user_message("Hi");
    let (tx, mut rx) = mpsc::unbounded_channel();

    runner
        .run(&mut conv, "system", &tx, CancellationToken::new())
        .await;

    drop(tx);
    let mut got_usage = false;
    while let Some(event) = rx.recv().await {
        if let TurnEvent::TokenUsage { total_tokens, .. } = event {
            assert!(total_tokens > 0);
            got_usage = true;
        }
    }
    assert!(got_usage, "Expected TokenUsage event");
}

// ===========================================================================
// 5. Conversation state correctness
// ===========================================================================

#[tokio::test]
async fn test_turn_runner_adds_assistant_message_on_text_response() {
    let mut runner = make_runner(
        MockProvider::text_only("Hello!"),
        ToolRegistry::new(),
        auto_bypass(),
    );
    let mut conv = Conversation::new();
    conv.add_user_message("Hi");
    let (tx, _rx) = mpsc::unbounded_channel();
    let msg_count_before = conv.messages.len();

    runner
        .run(&mut conv, "system", &tx, CancellationToken::new())
        .await;

    // Should have added an assistant text message
    assert_eq!(conv.messages.len(), msg_count_before + 1);
    let last = conv.messages.last().unwrap();
    assert!(matches!(
        last.role,
        crate::conversation::message::Role::Assistant
    ));
    assert_eq!(last.text(), Some("Hello!"));
}

#[tokio::test]
async fn test_turn_runner_adds_tool_call_and_result_messages() {
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(EchoTool { name: "grep" })).await;

    let provider = MockProvider::with_tool_call("grep", "{}");
    let mut runner = make_runner(provider, tools, auto_bypass());
    let mut conv = Conversation::new();
    conv.add_user_message("search");
    let (tx, _rx) = mpsc::unbounded_channel();
    let msg_count_before = conv.messages.len();

    runner
        .run(&mut conv, "system", &tx, CancellationToken::new())
        .await;

    // Should have: AssistantWithToolCalls + ToolResult = 2 new messages
    assert_eq!(conv.messages.len(), msg_count_before + 2);

    let assistant_msg = &conv.messages[msg_count_before];
    assert!(matches!(
        assistant_msg.content,
        crate::conversation::message::MessageContent::AssistantWithToolCalls { .. }
    ));

    let tool_msg = &conv.messages[msg_count_before + 1];
    assert!(matches!(
        tool_msg.content,
        crate::conversation::message::MessageContent::ToolResult(_)
    ));
}

/// Verify that tool results contain correct content and are properly linked
/// to their tool calls via call_id, so the next LLM turn sees a coherent
/// conversation (AssistantWithToolCalls → matching ToolResult).
#[tokio::test]
async fn test_tool_result_content_in_llm_context() {
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(EchoTool { name: "grep" })).await;

    let provider = MockProvider::with_tool_call("grep", r#"{"pattern":"foo"}"#);
    let mut runner = make_runner(provider, tools, auto_bypass());
    let mut conv = Conversation::new();
    conv.add_user_message("search for foo");
    let (tx, _rx) = mpsc::unbounded_channel();

    runner
        .run(&mut conv, "system prompt", &tx, CancellationToken::new())
        .await;

    // Build provider messages as TurnRunner would for the next LLM call.
    let provider_msgs = conv.to_provider_messages("system prompt");

    // Structure: [System, User, AssistantWithToolCalls, ToolResult]
    assert_eq!(provider_msgs.len(), 4);

    // 1. System prompt
    assert!(matches!(
        provider_msgs[0].role,
        crate::conversation::message::Role::System
    ));
    assert_eq!(provider_msgs[0].text(), Some("system prompt"));

    // 2. User message preserved
    assert!(matches!(
        provider_msgs[1].role,
        crate::conversation::message::Role::User
    ));
    assert_eq!(provider_msgs[1].text(), Some("search for foo"));

    // 3. Assistant message with tool call — call_id and arguments preserved
    if let crate::conversation::message::MessageContent::AssistantWithToolCalls {
        text: _,
        ref tool_calls,
        ..
    } = provider_msgs[2].content
    {
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].name, "grep");
        assert_eq!(tool_calls[0].arguments, r#"{"pattern":"foo"}"#);
        assert_eq!(tool_calls[0].id, "call_1");
    } else {
        panic!(
            "Expected AssistantWithToolCalls, got {:?}",
            provider_msgs[2].content
        );
    }

    // 4. Tool result — call_id matches, output contains actual tool execution result
    if let crate::conversation::message::MessageContent::ToolResult(ref result) =
        provider_msgs[3].content
    {
        assert_eq!(result.call_id, "call_1", "call_id must match the tool call");
        assert!(result.success);
        assert!(
            result.output.contains("executed grep"),
            "Tool output missing: {}",
            result.output
        );
        assert!(
            result.output.contains(r#"{"pattern":"foo"}"#),
            "Args missing from output: {}",
            result.output
        );
    } else {
        panic!("Expected ToolResult, got {:?}", provider_msgs[3].content);
    }
}

/// Verify that multiple tool calls in one turn each get their own result
/// with correct call_id linkage in the conversation.
#[tokio::test]
async fn test_multiple_tool_calls_results_in_context() {
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(EchoTool { name: "grep" })).await;
    tools.register(Box::new(EchoTool { name: "read_file" })).await;

    // Provider returns two tool calls in sequence
    let provider = MockProvider {
        events: vec![
            StreamEvent::ToolCallStart {
                id: "c1".into(),
                name: "grep".into(),
            },
            StreamEvent::ToolCallDelta(r#"{"pattern":"foo"}"#.into()),
            StreamEvent::ToolCallDone(ToolCall {
                id: "c1".into(),
                name: "grep".into(),
                arguments: r#"{"pattern":"foo"}"#.into(),
            }),
            StreamEvent::ToolCallStart {
                id: "c2".into(),
                name: "read_file".into(),
            },
            StreamEvent::ToolCallDelta(r#"{"file_path":"/tmp/x"}"#.into()),
            StreamEvent::ToolCallDone(ToolCall {
                id: "c2".into(),
                name: "read_file".into(),
                arguments: r#"{"file_path":"/tmp/x"}"#.into(),
            }),
            StreamEvent::Usage(TokenUsage {
                prompt_tokens: 20,
                completion_tokens: 10,
                cached_tokens: 0,
            }),
            StreamEvent::Done { truncated: false },
        ],
    };

    let mut runner = make_runner(provider, tools, auto_bypass());
    let mut conv = Conversation::new();
    conv.add_user_message("search and read");
    let (tx, _rx) = mpsc::unbounded_channel();

    let result = runner
        .run(&mut conv, "sys", &tx, CancellationToken::new())
        .await;

    // Should report 2 tool calls
    match result {
        TurnResult::UsedTools { tool_count, .. } => assert_eq!(tool_count, 2),
        other => panic!("Expected UsedTools, got {:?}", other),
    }

    // Build provider messages for next turn
    let msgs = conv.to_provider_messages("sys");
    // [System, User, AssistantWithToolCalls, ToolResult(c1), ToolResult(c2)]
    assert_eq!(msgs.len(), 5);

    // Verify AssistantWithToolCalls has both calls
    if let crate::conversation::message::MessageContent::AssistantWithToolCalls {
        ref tool_calls,
        ..
    } = msgs[2].content
    {
        assert_eq!(tool_calls.len(), 2);
        assert_eq!(tool_calls[0].id, "c1");
        assert_eq!(tool_calls[0].name, "grep");
        assert_eq!(tool_calls[1].id, "c2");
        assert_eq!(tool_calls[1].name, "read_file");
    } else {
        panic!("Expected AssistantWithToolCalls");
    }

    // Verify each ToolResult has correct call_id and content
    if let crate::conversation::message::MessageContent::ToolResult(ref r) = msgs[3].content {
        assert_eq!(r.call_id, "c1");
        assert!(r.output.contains("executed grep"));
    } else {
        panic!("Expected ToolResult for c1");
    }

    if let crate::conversation::message::MessageContent::ToolResult(ref r) = msgs[4].content {
        assert_eq!(r.call_id, "c2");
        assert!(r.output.contains("executed read_file"));
    } else {
        panic!("Expected ToolResult for c2");
    }
}

/// Verify that a denied tool call still produces a ToolResult in the context
/// (with success=false), so the LLM knows the tool was denied and can adjust.
#[tokio::test]
async fn test_denied_tool_result_in_llm_context() {
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(DangerousTool)).await;

    let provider = MockProvider::with_tool_call("dangerous", "{}");
    let mut runner = make_runner(provider, tools, auto_deny());
    let mut conv = Conversation::new();
    conv.add_user_message("do it");
    let (tx, _rx) = mpsc::unbounded_channel();

    runner
        .run(&mut conv, "sys", &tx, CancellationToken::new())
        .await;

    let msgs = conv.to_provider_messages("sys");
    // [System, User, AssistantWithToolCalls, ToolResult(denied)]
    assert_eq!(msgs.len(), 4);

    if let crate::conversation::message::MessageContent::ToolResult(ref r) = msgs[3].content {
        assert_eq!(r.call_id, "call_1");
        assert!(!r.success, "Denied tool should have success=false");
        assert!(
            r.output.contains("denied"),
            "Should indicate denial: {}",
            r.output
        );
    } else {
        panic!("Expected ToolResult for denied call");
    }
}

// ===========================================================================
// Prompt caching tests
// ===========================================================================

/// Turn reminder should be injected into the last user message (copy only),
/// not into the conversation history.
#[tokio::test]
async fn test_turn_reminder_injected_into_last_user_message() {
    let mut runner = make_runner(
        MockProvider::text_only("ok"),
        ToolRegistry::new(),
        auto_bypass(),
    );
    let mut conv = Conversation::new();
    conv.add_user_message("fix the bug");
    let (tx, _rx) = mpsc::unbounded_channel();

    let reminder = "<system-reminder>\nCurrent task: fix the bug\n</system-reminder>";
    runner
        .run_with_filter(
            &mut conv,
            "system",
            reminder,
            &tx,
            CancellationToken::new(),
            None,
        )
        .await;

    // Conversation history should NOT contain the reminder
    for msg in &conv.messages {
        if let crate::conversation::message::MessageContent::Text(ref text) = msg.content {
            assert!(
                !text.contains("system-reminder"),
                "Turn reminder leaked into conversation history: {}",
                text
            );
        }
    }
}

/// Empty turn reminder should not modify messages.
#[tokio::test]
async fn test_empty_turn_reminder_is_noop() {
    let mut runner = make_runner(
        MockProvider::text_only("ok"),
        ToolRegistry::new(),
        auto_bypass(),
    );
    let mut conv = Conversation::new();
    conv.add_user_message("hello");
    let (tx, _rx) = mpsc::unbounded_channel();

    // Empty reminder — should work exactly like run()
    runner
        .run_with_filter(&mut conv, "system", "", &tx, CancellationToken::new(), None)
        .await;

    // Should have completed normally
    assert!(conv.messages.len() >= 2); // user + assistant
}

/// ToolRegistry should return definitions in stable (sorted) order.
#[tokio::test]
async fn test_tool_registry_stable_order() {
    let mut registry = ToolRegistry::new();
    // Register in reverse alphabetical order
    registry.register(Box::new(EchoTool { name: "write_file" })).await;
    registry.register(Box::new(EchoTool { name: "bash" })).await;
    registry.register(Box::new(EchoTool { name: "read_file" })).await;
    registry.register(Box::new(EchoTool { name: "grep" })).await;
    registry.register(Box::new(EchoTool { name: "edit_file" })).await;

    let defs = registry.get_definitions().await;
    let names: Vec<&str> = defs.iter().map(|d| d.name).collect();

    // BTreeMap should give alphabetical order regardless of insertion order
    assert_eq!(
        names,
        vec!["bash", "edit_file", "grep", "read_file", "write_file"]
    );

    // Call again — order must be identical
    let defs2 = registry.get_definitions().await;
    let names2: Vec<&str> = defs2.iter().map(|d| d.name).collect();
    assert_eq!(names, names2, "Tool order must be stable across calls");
}

/// UNIFIED_PROMPT should not contain tool-specific usage descriptions
/// (those belong in tool definitions, not system prompt).
#[test]
fn test_rules_no_tool_descriptions() {
    let rules = crate::config::prompt_sections::build_rules();

    // Should NOT contain tool usage descriptions (removed for token savings)
    assert!(
        !rules.contains("Search code: grep"),
        "Rules should not describe grep usage"
    );
    assert!(
        !rules.contains("Find files: glob"),
        "Rules should not describe glob usage"
    );
    assert!(
        !rules.contains("Read code: read_file"),
        "Rules should not describe read_file usage"
    );
    assert!(
        !rules.contains("Edit files: edit_file"),
        "Rules should not describe edit_file usage"
    );
    assert!(
        !rules.contains("Create files: write_file"),
        "Rules should not describe write_file usage"
    );
    assert!(
        !rules.contains("Run commands: bash"),
        "Rules should not describe bash usage"
    );

    // SHOULD still contain tool discipline rules
    assert!(
        rules.contains("Call multiple tools in ONE turn"),
        "Rules must contain batch tool call discipline"
    );
}

/// System prompt should not contain dynamic content (date, git status, etc.)
/// that would break prompt caching.
#[test]
fn test_rules_no_dynamic_content() {
    let rules = crate::config::prompt_sections::build_rules();

    assert!(!rules.contains("Date:"), "Rules should not contain date");
    assert!(
        !rules.contains("Git:"),
        "Rules should not contain git status"
    );
    assert!(
        !rules.contains("Recent activity"),
        "Rules should not contain recent activity"
    );
}

// ===========================================================================
// detect_call_loop — Pattern 1 (per-region read saturation) integration tests
// ===========================================================================

/// Scanning 5 distinct regions of a large file must NOT trip the cap.
/// This was the regression captured in the v4.20 screenshot: the model read
/// multiple function bodies of a 1592-line render.rs with correct offsets and
/// hit the region cap on the Nth read even though every region was different.
#[test]
fn detect_call_loop_does_not_block_scanning_distinct_regions() {
    let mut runner = make_runner(
        MockProvider::text_only(""),
        ToolRegistry::new(),
        auto_bypass(),
    );

    // Five reads with offsets spaced well apart (>> READ_REGION_BUCKET).
    let offsets = [1, 200, 400, 700, 1000];
    for off in offsets {
        let args = format!(
            r#"{{"file_path":"/p/render.rs","offset":{},"limit":50}}"#,
            off
        );
        let verdict = runner.detect_call_loop("read_file", &args);
        assert!(
            verdict.is_none(),
            "offset {} should not block — it's a new region\nGot: {:?}",
            off,
            verdict
        );
    }
}

/// Re-reading the SAME region 3 times MUST trip Pattern 1. Each call tweaks
/// `limit` so the exact-args Pattern 2 guard does NOT fire first — this
/// isolates Pattern 1's semantics ("same bucket, any args"). The realistic
/// triggering pattern is the model cycling limit on an unreadable file
/// trying to coax different output from the same slice.
#[test]
fn detect_call_loop_blocks_three_reads_of_same_region() {
    let mut runner = make_runner(
        MockProvider::text_only(""),
        ToolRegistry::new(),
        auto_bypass(),
    );

    // Same offset → same bucket, but varying limit so args_hash differs and
    // Pattern 2 (identical-args) stays dormant.
    for i in 1..=2 {
        let args = format!(
            r#"{{"file_path":"/p/doc.docx","offset":1,"limit":{}}}"#,
            40 + i * 10
        );
        assert!(
            runner.detect_call_loop("read_file", &args).is_none(),
            "call #{} of same region should not block yet",
            i
        );
    }
    // 3rd read still in the same bucket → Pattern 1 fires.
    let third = r#"{"file_path":"/p/doc.docx","offset":1,"limit":95}"#;
    let blocked = runner.detect_call_loop("read_file", third);
    let msg = blocked.expect("3rd call to same region must be blocked by Pattern 1");
    assert!(
        msg.contains("SAME region"),
        "block message must mention 'SAME region' so the model understands why\nGot: {}",
        msg
    );
    assert!(msg.contains("doc.docx"), "block message must name the file");
}

/// A successful edit on the same file clears every region's counter so
/// post-edit verification reads aren't blocked even if some region was near
/// the cap pre-edit.
#[test]
fn detect_call_loop_edit_resets_all_regions_for_file() {
    let mut runner = make_runner(
        MockProvider::text_only(""),
        ToolRegistry::new(),
        auto_bypass(),
    );

    // Two reads of the same bucket (one shy of the 3-call cap, varying limit
    // so Pattern 2 stays dormant).
    for i in 1..=2 {
        let args = format!(
            r#"{{"file_path":"/p/x.rs","offset":1,"limit":{}}}"#,
            30 + i * 10
        );
        assert!(runner.detect_call_loop("read_file", &args).is_none());
    }

    // Edit on the same file → reset all region counts for x.rs.
    let edit_args = r#"{"file_path":"/p/x.rs","old_string":"a","new_string":"b"}"#;
    assert!(runner.detect_call_loop("edit_file", edit_args).is_none());

    // The previously-hot bucket should now be freshly empty; 2 more reads
    // stay unblocked post-edit (verifying the `retain` cleared the entry).
    for i in 1..=2 {
        let args = format!(
            r#"{{"file_path":"/p/x.rs","offset":1,"limit":{}}}"#,
            100 + i * 10
        );
        assert!(
            runner.detect_call_loop("read_file", &args).is_none(),
            "post-edit read #{} should not be blocked — edit reset the counter",
            i
        );
    }
}

// ===========================================================================
// validate_args gate: malformed tool-call args bounce back to the model
// without prompting the user or running execute.
// ===========================================================================

#[tokio::test]
async fn malformed_write_file_args_short_circuit_without_approval() {
    use crate::tool::write::WriteFileTool;
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(WriteFileTool)).await;

    // 2026-05-02 datalog `atomgr/2026-05-02_20-23-21.md` line 330:
    // provider stream truncated mid-args, closing bracket wrong, no
    // `content` field. Pre-fix, this would (a) fail json_repair, (b)
    // fall into write_file's fail-closed approval branch which prompts
    // the user, (c) the user approves, (d) execute() re-parses and
    // returns the same missing-field error. Post-fix the runner's
    // validate_args gate short-circuits to a tool-result error and
    // approval/execute are never reached.
    let bad_args = r#"{"file_path": "/tmp/x.rs"]"#;
    let provider = MockProvider::with_tool_call("write_file", bad_args);
    // Use a permission decider that PANICS if asked — proves no
    // approval round-trip happened.
    struct PanicOnApproval;
    #[async_trait]
    impl super::permission::PermissionDecider for PanicOnApproval {
        async fn decide(
            &self,
            _call: &crate::tool::ToolCall,
            _reason: &str,
        ) -> crate::tool::PermissionDecision {
            panic!("validate_args gate must short-circuit before approval is requested");
        }
    }
    let mut runner = make_runner(provider, tools, Box::new(PanicOnApproval));
    let mut conv = Conversation::new();
    conv.add_user_message("write a file");
    let (tx, mut rx) = mpsc::unbounded_channel();

    let _ = runner
        .run(&mut conv, "system", &tx, CancellationToken::new())
        .await;
    drop(tx);

    let mut got_error_result = false;
    while let Some(event) = rx.recv().await {
        if let TurnEvent::ToolCallResult {
            name,
            success,
            output,
            ..
        } = event
        {
            if name == "write_file" {
                assert!(!success, "validate-fail must surface as success=false");
                assert!(
                    output.to_lowercase().contains("missing field")
                        || output.to_lowercase().contains("re-issue"),
                    "tool result must carry the structured retry hint, got: {output}"
                );
                got_error_result = true;
            }
        }
    }
    assert!(
        got_error_result,
        "validate-fail must still emit a ToolCallResult so the model can retry"
    );
}

#[test]
fn write_file_validate_args_catches_real_datalog_fixtures() {
    use crate::tool::write::WriteFileTool;
    use crate::tool::Tool as _;
    let tool = WriteFileTool;

    // 2026-05-02 datalog 10-37-51.md line 225: stream cut at `{`.
    assert!(
        tool.validate_args("{").is_err(),
        "single-brace truncation must reject"
    );
    // 2026-05-02 datalog 20-23-21.md line 330: closing `]`, no content.
    assert!(
        tool.validate_args(r#"{"file_path": "/tmp/x.rs"]"#).is_err(),
        "closing-bracket-wrong + missing field must reject"
    );
    // Empty args.
    assert!(tool.validate_args("").is_err());
    assert!(tool.validate_args("{}").is_err(), "empty object must reject");
    // Valid call passes.
    assert!(
        tool.validate_args(r#"{"file_path":"/tmp/x.rs","content":"hi"}"#)
            .is_ok()
    );
}

#[test]
fn edit_file_validate_args_rejects_missing_fields() {
    use crate::tool::edit::EditFileTool;
    use crate::tool::Tool as _;
    let tool = EditFileTool;
    assert!(tool.validate_args("{}").is_err());
    assert!(
        tool.validate_args(r#"{"file_path":"/x.rs"}"#).is_err(),
        "missing old_string + new_string must reject"
    );
    assert!(
        tool.validate_args(
            r#"{"file_path":"/x.rs","old_string":"a","new_string":"b"}"#
        )
        .is_ok()
    );
}

#[test]
fn search_replace_validate_args_rejects_missing_fields() {
    use crate::tool::search_replace::SearchReplaceTool;
    use crate::tool::Tool as _;
    let tool = SearchReplaceTool;
    assert!(tool.validate_args("{}").is_err());
    assert!(
        tool.validate_args(r#"{"search":"a","replace":"b"}"#).is_ok()
    );
}

// ===========================================================================
// Telemetry integration tests
// ===========================================================================

#[cfg(test)]
mod telemetry_tests {
    use super::*;
    use crate::tool::ToolContext;
    use atomcode_telemetry::{Event, Telemetry};
    use std::path::PathBuf;

    /// Build a TurnRunner wired to a real in-memory Telemetry handle so we can
    /// assert on the emitted events.
    fn make_runner_with_telemetry(
        provider: MockProvider,
        tools: ToolRegistry,
    ) -> (
        TurnRunner,
        std::sync::Arc<tokio::sync::Mutex<Vec<atomcode_telemetry::Record>>>,
    ) {
        let (tel, captured) = Telemetry::in_memory("test".into());
        let ctx = ToolContext::with_telemetry(PathBuf::from("/tmp/test"), "session-1", tel);

        let test_provider_cfg = crate::config::provider::ProviderConfig {
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
            thinking_enabled: None,
            thinking_budget: None,
            skip_tls_verify: false,
            ephemeral: true,
        };
        let test_ctx: std::sync::Arc<dyn crate::ctx::CtxBuilder> =
            std::sync::Arc::new(crate::ctx::DefaultCtx::new(&test_provider_cfg));

        let runner = TurnRunner {
            provider: std::sync::Arc::new(provider),
            tools: std::sync::Arc::new(tools),
            context: ctx,
            config: test_config(),
            ctx: test_ctx,
            permission: Box::new(AutoPermissionDecider::new(AutoPermissionMode::BypassAll)),
            recently_edited_files: Vec::new(),
            recent_calls: Vec::new(),
            file_read_counts: std::collections::HashMap::new(),
            hook_executor: std::sync::Arc::new(
                crate::hook::executor::HookExecutor::empty()
            ),
        };
        (runner, captured)
    }

    #[tokio::test]
    async fn turn_emits_exactly_one_llm_chat_for_text_only_turn() {
        let (mut runner, captured) =
            make_runner_with_telemetry(MockProvider::text_only("Hello"), ToolRegistry::new());
        let mut conv = Conversation::new();
        conv.add_user_message("Hi");
        let (tx, _rx) = mpsc::unbounded_channel();

        runner
            .run(&mut conv, "system", &tx, CancellationToken::new())
            .await;

        // Give the background task a moment to drain the channel.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let events = captured.lock().await;
        let llm_chats: Vec<_> = events
            .iter()
            .filter(|r| matches!(r.event, Event::LlmChat { .. }))
            .collect();

        assert_eq!(
            llm_chats.len(),
            1,
            "expected exactly one LlmChat per turn, got {}",
            llm_chats.len()
        );

        // turn_id must be populated by the scope in run_with_filter.
        assert!(
            llm_chats[0].envelope.turn_id.is_some(),
            "LlmChat envelope must carry a turn_id"
        );

        // Basic payload sanity.
        if let Event::LlmChat {
            tool_calls_count,
            had_error,
            output_tokens,
            ..
        } = llm_chats[0].event
        {
            assert_eq!(tool_calls_count, 0, "text-only turn has no tool calls");
            assert!(!had_error, "successful turn must not set had_error");
            assert!(
                output_tokens > 0,
                "output_tokens should be non-zero (usage reported by mock)"
            );
        }
    }

    #[tokio::test]
    async fn turn_emits_llm_chat_with_tool_calls_count() {
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(EchoTool { name: "echo" }));

        let (mut runner, captured) = make_runner_with_telemetry(
            MockProvider::with_tool_call("echo", r#"{"msg":"hi"}"#),
            tools,
        );
        let mut conv = Conversation::new();
        conv.add_user_message("Use echo");
        let (tx, _rx) = mpsc::unbounded_channel();

        runner
            .run(&mut conv, "system", &tx, CancellationToken::new())
            .await;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let events = captured.lock().await;
        let llm_chats: Vec<_> = events
            .iter()
            .filter(|r| matches!(r.event, Event::LlmChat { .. }))
            .collect();

        assert_eq!(llm_chats.len(), 1, "expected one LlmChat");

        if let Event::LlmChat {
            tool_calls_count,
            had_error,
            ..
        } = llm_chats[0].event
        {
            assert_eq!(tool_calls_count, 1, "tool turn should report 1 tool call");
            assert!(!had_error);
        }
    }

    #[tokio::test]
    async fn turn_emits_llm_chat_with_had_error_on_failure() {
        let (mut runner, captured) = make_runner_with_telemetry(
            MockProvider::with_error("provider blew up"),
            ToolRegistry::new(),
        );
        let mut conv = Conversation::new();
        conv.add_user_message("Hi");
        let (tx, _rx) = mpsc::unbounded_channel();

        runner
            .run(&mut conv, "system", &tx, CancellationToken::new())
            .await;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let events = captured.lock().await;
        let llm_chats: Vec<_> = events
            .iter()
            .filter(|r| matches!(r.event, Event::LlmChat { .. }))
            .collect();

        assert_eq!(llm_chats.len(), 1, "even failed turns emit one LlmChat");
        if let Event::LlmChat { had_error, .. } = llm_chats[0].event {
            assert!(had_error, "failed turn must set had_error=true");
        }
    }
}
