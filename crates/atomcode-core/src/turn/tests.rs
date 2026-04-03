//! Tests for TurnRunner, discipline logic, and approval flow.

use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use anyhow::Result;
use async_trait::async_trait;
use futures::stream;
use futures::Stream;
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
                }),
                StreamEvent::Done,
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
                }),
                StreamEvent::Done,
            ],
        }
    }

    fn with_error(msg: &str) -> Self {
        Self {
            events: vec![StreamEvent::Error(msg.to_string())],
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
        },
    );
    Config {
        default_provider: "test".to_string(),
        default_workdir: None,
        providers,
    }
}

fn test_context() -> ToolContext {
    ToolContext::new(PathBuf::from("/tmp/test"))
}

fn make_runner(provider: MockProvider, tools: ToolRegistry, permission: Box<dyn super::permission::PermissionDecider>) -> TurnRunner {
    TurnRunner {
        provider: Box::new(provider),
        tools: std::sync::Arc::new(tools),
        context: test_context(),
        config: test_config(),
        permission,
        result_store: crate::tool::result_store::ToolResultStore::new(
            crate::tool::result_store::ToolResultStore::default_dir()
        ),
        recently_edited_files: Vec::new(),
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
    let mut runner = make_runner(MockProvider::text_only("Hello, world!"), ToolRegistry::new(), auto_bypass());
    let mut conv = Conversation::new();
    conv.add_user_message("Hi");
    let (tx, _rx) = mpsc::unbounded_channel();

    let result = runner.run(&mut conv, "system", &tx, CancellationToken::new()).await;

    match result {
        TurnResult::Responded { text, tokens } => {
            assert_eq!(text, "Hello, world!");
            assert!(tokens > 0);
        }
        other => panic!("Expected Responded, got {:?}", other),
    }
}

#[tokio::test]
async fn test_turn_runner_emits_text_delta_events() {
    let mut runner = make_runner(MockProvider::text_only("Hello"), ToolRegistry::new(), auto_bypass());
    let mut conv = Conversation::new();
    conv.add_user_message("Hi");
    let (tx, mut rx) = mpsc::unbounded_channel();

    runner.run(&mut conv, "system", &tx, CancellationToken::new()).await;

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
    tools.register(Box::new(EchoTool { name: "grep" }));

    let provider = MockProvider::with_tool_call("grep", r#"{"pattern":"foo"}"#);
    let mut runner = make_runner(provider, tools, auto_bypass());
    let mut conv = Conversation::new();
    conv.add_user_message("search for foo");
    let (tx, _rx) = mpsc::unbounded_channel();

    let result = runner.run(&mut conv, "system", &tx, CancellationToken::new()).await;

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
    tools.register(Box::new(EchoTool { name: "grep" }));

    let provider = MockProvider::with_tool_call("grep", r#"{"pattern":"foo"}"#);
    let mut runner = make_runner(provider, tools, auto_bypass());
    let mut conv = Conversation::new();
    conv.add_user_message("search");
    let (tx, mut rx) = mpsc::unbounded_channel();

    runner.run(&mut conv, "system", &tx, CancellationToken::new()).await;

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

    let result = runner.run(&mut conv, "system", &tx, CancellationToken::new()).await;

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

    let result = runner.run(&mut conv, "system", &tx, CancellationToken::new()).await;

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
    tools.register(Box::new(DangerousTool));

    let provider = MockProvider::with_tool_call("dangerous", "{}");
    let mut runner = make_runner(provider, tools, auto_deny());
    let mut conv = Conversation::new();
    conv.add_user_message("do dangerous thing");
    let (tx, _rx) = mpsc::unbounded_channel();

    let result = runner.run(&mut conv, "system", &tx, CancellationToken::new()).await;

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
    tools.register(Box::new(DangerousTool));

    let provider = MockProvider::with_tool_call("dangerous", "{}");
    let mut runner = make_runner(provider, tools, auto_bypass());
    let mut conv = Conversation::new();
    conv.add_user_message("do dangerous thing");
    let (tx, _rx) = mpsc::unbounded_channel();

    let result = runner.run(&mut conv, "system", &tx, CancellationToken::new()).await;

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
    tools.register(Box::new(DangerousTool));

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

    let result = runner.run(&mut conv, "system", &tx, CancellationToken::new()).await;

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
    tools.register(Box::new(DangerousTool));

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

    let result = runner.run(&mut conv, "system", &tx, CancellationToken::new()).await;

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
    // step limit = 35 + 5*0 = 35, min(35, 60) = 35
    // tool_call_count = 10 < 35 → false
    assert!(!check_step_limit_impl(10, 0));
}

#[test]
fn test_check_step_limit_at_limit() {
    // step limit = 35 + 5*0 = 35
    assert!(check_step_limit_impl(35, 0));
}

#[test]
fn test_check_step_limit_with_edits_extends() {
    // step limit = 35 + 5*3 = 50
    assert!(!check_step_limit_impl(40, 3));
    assert!(check_step_limit_impl(50, 3));
}

#[test]
fn test_check_step_limit_hard_cap_60() {
    // step limit = 35 + 5*10 = 85, min(85, 60) = 60
    assert!(!check_step_limit_impl(59, 10));
    assert!(check_step_limit_impl(60, 10));
}

/// Standalone reimplementation of check_step_limit logic for unit testing.
/// (AgentLoop::check_step_limit is not easily callable from tests.)
fn check_step_limit_impl(tool_call_count: usize, files_edited_count: usize) -> bool {
    let dynamic_limit = 35 + (5 * files_edited_count);
    let hard_limit = dynamic_limit.min(60);
    tool_call_count >= hard_limit
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
    let mut runner = make_runner(MockProvider::text_only("Hello"), ToolRegistry::new(), auto_bypass());
    let mut conv = Conversation::new();
    conv.add_user_message("Hi");
    let (tx, mut rx) = mpsc::unbounded_channel();

    runner.run(&mut conv, "system", &tx, CancellationToken::new()).await;

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
    let mut runner = make_runner(MockProvider::text_only("Hello!"), ToolRegistry::new(), auto_bypass());
    let mut conv = Conversation::new();
    conv.add_user_message("Hi");
    let (tx, _rx) = mpsc::unbounded_channel();
    let msg_count_before = conv.messages.len();

    runner.run(&mut conv, "system", &tx, CancellationToken::new()).await;

    // Should have added an assistant text message
    assert_eq!(conv.messages.len(), msg_count_before + 1);
    let last = conv.messages.last().unwrap();
    assert!(matches!(last.role, crate::conversation::message::Role::Assistant));
    assert_eq!(last.text(), Some("Hello!"));
}

#[tokio::test]
async fn test_turn_runner_adds_tool_call_and_result_messages() {
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(EchoTool { name: "grep" }));

    let provider = MockProvider::with_tool_call("grep", "{}");
    let mut runner = make_runner(provider, tools, auto_bypass());
    let mut conv = Conversation::new();
    conv.add_user_message("search");
    let (tx, _rx) = mpsc::unbounded_channel();
    let msg_count_before = conv.messages.len();

    runner.run(&mut conv, "system", &tx, CancellationToken::new()).await;

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
    tools.register(Box::new(EchoTool { name: "grep" }));

    let provider = MockProvider::with_tool_call("grep", r#"{"pattern":"foo"}"#);
    let mut runner = make_runner(provider, tools, auto_bypass());
    let mut conv = Conversation::new();
    conv.add_user_message("search for foo");
    let (tx, _rx) = mpsc::unbounded_channel();

    runner.run(&mut conv, "system prompt", &tx, CancellationToken::new()).await;

    // Build provider messages as TurnRunner would for the next LLM call.
    let provider_msgs = conv.to_provider_messages("system prompt");

    // Structure: [System, User, AssistantWithToolCalls, ToolResult]
    assert_eq!(provider_msgs.len(), 4);

    // 1. System prompt
    assert!(matches!(provider_msgs[0].role, crate::conversation::message::Role::System));
    assert_eq!(provider_msgs[0].text(), Some("system prompt"));

    // 2. User message preserved
    assert!(matches!(provider_msgs[1].role, crate::conversation::message::Role::User));
    assert_eq!(provider_msgs[1].text(), Some("search for foo"));

    // 3. Assistant message with tool call — call_id and arguments preserved
    if let crate::conversation::message::MessageContent::AssistantWithToolCalls { text: _, ref tool_calls } = provider_msgs[2].content {
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].name, "grep");
        assert_eq!(tool_calls[0].arguments, r#"{"pattern":"foo"}"#);
        assert_eq!(tool_calls[0].id, "call_1");
    } else {
        panic!("Expected AssistantWithToolCalls, got {:?}", provider_msgs[2].content);
    }

    // 4. Tool result — call_id matches, output contains actual tool execution result
    if let crate::conversation::message::MessageContent::ToolResult(ref result) = provider_msgs[3].content {
        assert_eq!(result.call_id, "call_1", "call_id must match the tool call");
        assert!(result.success);
        assert!(result.output.contains("executed grep"), "Tool output missing: {}", result.output);
        assert!(result.output.contains(r#"{"pattern":"foo"}"#), "Args missing from output: {}", result.output);
    } else {
        panic!("Expected ToolResult, got {:?}", provider_msgs[3].content);
    }
}

/// Verify that multiple tool calls in one turn each get their own result
/// with correct call_id linkage in the conversation.
#[tokio::test]
async fn test_multiple_tool_calls_results_in_context() {
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(EchoTool { name: "grep" }));
    tools.register(Box::new(EchoTool { name: "read_file" }));

    // Provider returns two tool calls in sequence
    let provider = MockProvider {
        events: vec![
            StreamEvent::ToolCallStart { id: "c1".into(), name: "grep".into() },
            StreamEvent::ToolCallDelta(r#"{"pattern":"foo"}"#.into()),
            StreamEvent::ToolCallDone(ToolCall { id: "c1".into(), name: "grep".into(), arguments: r#"{"pattern":"foo"}"#.into() }),
            StreamEvent::ToolCallStart { id: "c2".into(), name: "read_file".into() },
            StreamEvent::ToolCallDelta(r#"{"file_path":"/tmp/x"}"#.into()),
            StreamEvent::ToolCallDone(ToolCall { id: "c2".into(), name: "read_file".into(), arguments: r#"{"file_path":"/tmp/x"}"#.into() }),
            StreamEvent::Usage(TokenUsage { prompt_tokens: 20, completion_tokens: 10 }),
            StreamEvent::Done,
        ],
    };

    let mut runner = make_runner(provider, tools, auto_bypass());
    let mut conv = Conversation::new();
    conv.add_user_message("search and read");
    let (tx, _rx) = mpsc::unbounded_channel();

    let result = runner.run(&mut conv, "sys", &tx, CancellationToken::new()).await;

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
    if let crate::conversation::message::MessageContent::AssistantWithToolCalls { ref tool_calls, .. } = msgs[2].content {
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
    tools.register(Box::new(DangerousTool));

    let provider = MockProvider::with_tool_call("dangerous", "{}");
    let mut runner = make_runner(provider, tools, auto_deny());
    let mut conv = Conversation::new();
    conv.add_user_message("do it");
    let (tx, _rx) = mpsc::unbounded_channel();

    runner.run(&mut conv, "sys", &tx, CancellationToken::new()).await;

    let msgs = conv.to_provider_messages("sys");
    // [System, User, AssistantWithToolCalls, ToolResult(denied)]
    assert_eq!(msgs.len(), 4);

    if let crate::conversation::message::MessageContent::ToolResult(ref r) = msgs[3].content {
        assert_eq!(r.call_id, "call_1");
        assert!(!r.success, "Denied tool should have success=false");
        assert!(r.output.contains("denied"), "Should indicate denial: {}", r.output);
    } else {
        panic!("Expected ToolResult for denied call");
    }
}
