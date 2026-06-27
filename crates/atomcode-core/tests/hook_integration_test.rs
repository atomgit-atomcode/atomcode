//! Hook 集成测试 - 验证 Hook 在完整 Turn 流程中的行为

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::pin::Pin;

use anyhow::Result;
use async_trait::async_trait;
use futures::stream;
use futures::Stream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use atomcode_core::config::provider::ProviderConfig;
use atomcode_core::config::Config;
use atomcode_core::conversation::message::Message;
use atomcode_core::conversation::Conversation;
use atomcode_core::ctx::default::DefaultCtx;
use atomcode_core::provider::LlmProvider;
use atomcode_core::stream::StreamEvent;
use atomcode_core::tool::{
    ApprovalRequirement, Tool, ToolCall, ToolContext, ToolDef, ToolRegistry, ToolResult,
};
use atomcode_core::turn::event::TurnResult;
use atomcode_core::turn::permission::{AutoPermissionDecider, AutoPermissionMode};
use atomcode_core::turn::runner::TurnRunner;
use atomcode_core::hook::{Hook, HookCtx, HookEngine, HookResult, PreToolExecutionHook, PostToolExecutionHook, ToolResultContext};

// ===========================================================================
// Mock Provider
// ===========================================================================

struct MockProvider {
    events: Vec<StreamEvent>,
}

impl MockProvider {
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
                StreamEvent::Usage(atomcode_core::stream::TokenUsage {
                    prompt_tokens: 10,
                    completion_tokens: 8,
                    cached_tokens: 0,
                }),
                StreamEvent::Done { truncated: false },
            ],
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
        "mock"
    }
}

// ===========================================================================
// Mock Tool
// ===========================================================================

struct MockEchoTool;

#[async_trait]
impl Tool for MockEchoTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "echo",
            description: "Echo tool for testing".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "message": {"type": "string"}
                }
            }),
        }
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::AutoApprove
    }

    async fn execute(&self, args: &str, _ctx: &ToolContext) -> Result<ToolResult> {
        let args_val: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
        let msg = args_val.get("message").and_then(|m| m.as_str()).unwrap_or("empty");
        
        Ok(ToolResult {
            call_id: String::new(),
            output: format!("Echo: {}", msg),
            success: true,
        })
    }
}

// ===========================================================================
// Test Hooks
// ===========================================================================

struct TrackingPreHook {
    call_count: Arc<AtomicUsize>,
    last_tool_name: Arc<std::sync::Mutex<String>>,
}

#[async_trait]
impl Hook for TrackingPreHook {
    fn name(&self) -> &str {
        "tracking-pre-hook"
    }
}

#[async_trait]
impl PreToolExecutionHook for TrackingPreHook {
    async fn on_pre_execute(&self, ctx: &HookCtx) -> HookResult {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        *self.last_tool_name.lock().unwrap() = ctx.tool_name.clone();
        HookResult::Ok
    }
}

struct TrackingPostHook {
    call_count: Arc<AtomicUsize>,
    last_result: Arc<std::sync::Mutex<String>>,
}

#[async_trait]
impl Hook for TrackingPostHook {
    fn name(&self) -> &str {
        "tracking-post-hook"
    }
}

#[async_trait]
impl PostToolExecutionHook for TrackingPostHook {
    async fn on_post_execute(&self, _ctx: &HookCtx, result_ctx: &ToolResultContext) -> HookResult {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        *self.last_result.lock().unwrap() = result_ctx.result.clone();
        HookResult::Ok
    }
}

// ===========================================================================
// Helpers
// ===========================================================================

fn test_config() -> Config {
    let mut providers = HashMap::new();
    providers.insert(
        "mock".to_string(),
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
            reasoning_effort: None,
            thinking_enabled: None,
            thinking_budget: None,
            skip_tls_verify: false,
            ephemeral: false,
        },
    );
    Config {
        auto_update: false,
        providers,
        ..Config::with_default_provider("mock")
    }
}

fn test_context() -> ToolContext {
    ToolContext::new(PathBuf::from("/tmp/test"))
}

async fn create_test_runner(
    provider: MockProvider,
    hook_engine: std::sync::Arc<atomcode_core::hook::HookEngine>,
) -> TurnRunner {
    let registry = ToolRegistry::new();
    registry.register(Box::new(MockEchoTool)).await;
    TurnRunner {
        provider: std::sync::Arc::new(provider),
        tools: std::sync::Arc::new(registry),
        context: test_context(),
        config: test_config(),
        permission: Box::new(AutoPermissionDecider::new(AutoPermissionMode::BypassAll)),
        hook_engine,
        recently_edited_files: Vec::new(),
        ctx: std::sync::Arc::new(DefaultCtx::new(&test_config().providers.get("mock").unwrap())),
        loop_guard: Default::default(),
        current_turn_number: 0,
    }
}

// ===========================================================================
// Integration Tests
// ===========================================================================

#[tokio::test]
#[ignore = "需要适配 release 分支的 ToolCallStreamFilter 流处理机制；MockProvider 事件格式需更新以匹配 run_with_filter 的流处理逻辑"]
async fn test_hooks_fire_during_turn() {
    let pre_count = Arc::new(AtomicUsize::new(0));
    let post_count = Arc::new(AtomicUsize::new(0));
    let last_tool = Arc::new(std::sync::Mutex::new(String::new()));
    let last_result = Arc::new(std::sync::Mutex::new(String::new()));
    
    let mut hooks = HookEngine::new();
    hooks.register_pre_tool_hook(Arc::new(TrackingPreHook {
        call_count: pre_count.clone(),
        last_tool_name: last_tool.clone(),
    }));
    hooks.register_post_tool_hook(Arc::new(TrackingPostHook {
        call_count: post_count.clone(),
        last_result: last_result.clone(),
    }));
    
    let provider = MockProvider::with_tool_call(
        "echo",
        r#"{"message": "hello hooks"}"#,
    );
    let mut runner = create_test_runner(provider, std::sync::Arc::new(hooks)).await;
    
    let mut conv = Conversation::new();
    conv.add_user_message("Test the echo tool");
    let (tx, _rx) = mpsc::unbounded_channel();
    
    let result = runner.run(&mut conv, "system", &tx, CancellationToken::new()).await;
    
    match result {
        TurnResult::UsedTools { tool_count, .. } => {
            assert_eq!(tool_count, 1);
            assert_eq!(pre_count.load(Ordering::SeqCst), 1, "Pre-hook should be called once");
            assert_eq!(post_count.load(Ordering::SeqCst), 1, "Post-hook should be called once");
            assert_eq!(*last_tool.lock().unwrap(), "echo");
            assert!(last_result.lock().unwrap().contains("Echo: hello hooks"));
        }
        other => panic!("Expected UsedTools, got {:?}", other),
    }
}
