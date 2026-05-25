use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use atomcode_core::hook::{
    Hook, HookCtx, HookRegistry, HookResult, HookStats,
    OnErrorHook, OnMessageReceivedHook, OnModelResponseHook,
    OnSessionEndHook, OnSessionStartHook, OnToolCallStartHook,
    OnTurnCompleteHook, OnTurnStartHook, PostToolExecutionHook,
    PostTurnHook, PreToolExecutionHook, SystemPromptHook,
    ToolResultContext, ErrorContext, SessionContext,
    ToolCallStartContext, TurnCompleteContext, TurnStartContext,
    UserMessageContext,
};

/// Test hook that counts how many times it was called
struct CountingPreHook {
    count: Arc<AtomicUsize>,
}

#[async_trait]
impl Hook for CountingPreHook {
    fn name(&self) -> &str {
        "counting-pre-hook"
    }
}

#[async_trait]
impl PreToolExecutionHook for CountingPreHook {
    async fn on_pre_execute(&self, _ctx: &HookCtx) -> HookResult {
        self.count.fetch_add(1, Ordering::SeqCst);
        HookResult::Ok
    }
}

struct CountingPostHook {
    count: Arc<AtomicUsize>,
}

#[async_trait]
impl Hook for CountingPostHook {
    fn name(&self) -> &str {
        "counting-post-hook"
    }
}

#[async_trait]
impl PostToolExecutionHook for CountingPostHook {
    async fn on_post_execute(&self, _ctx: &HookCtx, _result_ctx: &ToolResultContext) -> HookResult {
        self.count.fetch_add(1, Ordering::SeqCst);
        HookResult::Ok
    }
}

struct CountingPostTurnHook {
    count: Arc<AtomicUsize>,
}

#[async_trait]
impl Hook for CountingPostTurnHook {
    fn name(&self) -> &str {
        "counting-post-turn-hook"
    }
}

#[async_trait]
impl PostTurnHook for CountingPostTurnHook {
    async fn on_post_turn(&self, _ctx: &HookCtx, _turn_result: &str) -> HookResult {
        self.count.fetch_add(1, Ordering::SeqCst);
        HookResult::Ok
    }
}

struct TestSystemPromptHook {
    content: String,
}

#[async_trait]
impl Hook for TestSystemPromptHook {
    fn name(&self) -> &str {
        "test-system-prompt-hook"
    }
}

#[async_trait]
impl SystemPromptHook for TestSystemPromptHook {
    async fn extend_system_prompt(&self) -> Option<String> {
        Some(self.content.clone())
    }
}

#[tokio::test]
async fn test_hook_registry_basic() {
    let mut registry = HookRegistry::new();
    
    let pre_count = Arc::new(AtomicUsize::new(0));
    let post_count = Arc::new(AtomicUsize::new(0));
    let post_turn_count = Arc::new(AtomicUsize::new(0));
    
    let pre_hook = Arc::new(CountingPreHook { count: pre_count.clone() });
    let post_hook = Arc::new(CountingPostHook { count: post_count.clone() });
    let post_turn_hook = Arc::new(CountingPostTurnHook { count: post_turn_count.clone() });
    
    registry.register_pre_tool_hook(pre_hook);
    registry.register_post_tool_hook(post_hook);
    registry.register_post_turn_hook(post_turn_hook);
    
    // Verify stats
    let stats = registry.stats();
    assert_eq!(stats.pre_tool_hooks, 1);
    assert_eq!(stats.post_tool_hooks, 1);
    assert_eq!(stats.post_turn_hooks, 1);
    
    // Test pre-tool hooks
    let ctx = HookCtx::new("test_tool".to_string(), "{}".to_string(), "/tmp".to_string());
    let result = registry.trigger_pre_tool_hooks(&ctx).await;
    assert!(result.is_ok());
    assert_eq!(pre_count.load(Ordering::SeqCst), 1);
    
    // Test post-tool-hooks
    let result_ctx = ToolResultContext {
        tool_name: "test_tool".to_string(),
        tool_args: "{}".to_string(),
        result: "success".to_string(),
        success: true,
        duration_ms: 100,
    };
    registry.trigger_post_tool_hooks(&ctx, &result_ctx).await;
    assert_eq!(post_count.load(Ordering::SeqCst), 1);
    
    // Test post-turn hooks
    registry.trigger_post_turn_hooks(&ctx, "UsedTools").await;
    assert_eq!(post_turn_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_hook_deny_execution() {
    let mut registry = HookRegistry::new();
    
    struct DenyingHook;
    
    #[async_trait]
    impl Hook for DenyingHook {
        fn name(&self) -> &str {
            "denying-hook"
        }
    }
    
    #[async_trait]
    impl PreToolExecutionHook for DenyingHook {
        async fn on_pre_execute(&self, _ctx: &HookCtx) -> HookResult {
            HookResult::Denied("Security policy violation".to_string())
        }
    }
    
    registry.register_pre_tool_hook(Arc::new(DenyingHook));
    
    let ctx = HookCtx::new("bash".to_string(), "rm -rf /".to_string(), "/tmp".to_string());
    let result = registry.trigger_pre_tool_hooks(&ctx).await;
    
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Security policy violation"));
}

#[tokio::test]
async fn test_hook_modify_args() {
    let mut registry = HookRegistry::new();
    
    struct ModifyingHook;
    
    #[async_trait]
    impl Hook for ModifyingHook {
        fn name(&self) -> &str {
            "modifying-hook"
        }
    }
    
    #[async_trait]
    impl PreToolExecutionHook for ModifyingHook {
        async fn on_pre_execute(&self, _ctx: &HookCtx) -> HookResult {
            HookResult::Modified("{\"modified\": true}".to_string())
        }
    }
    
    registry.register_pre_tool_hook(Arc::new(ModifyingHook));
    
    let ctx = HookCtx::new("edit_file".to_string(), "{}".to_string(), "/tmp".to_string());
    let result = registry.trigger_pre_tool_hooks(&ctx).await;
    
    assert!(result.is_ok());
    let modified_args = result.unwrap();
    assert!(modified_args.is_some());
    assert_eq!(modified_args.unwrap(), "{\"modified\": true}");
}

#[tokio::test]
async fn test_system_prompt_hook() {
    let mut registry = HookRegistry::new();
    
    let hook1 = Arc::new(TestSystemPromptHook { content: "Rule 1".to_string() });
    let hook2 = Arc::new(TestSystemPromptHook { content: "Rule 2".to_string() });
    
    registry.register_system_prompt_hook(hook1);
    registry.register_system_prompt_hook(hook2);
    
    let extensions = registry.collect_system_prompt_extensions().await;
    
    assert_eq!(extensions.len(), 2);
    assert!(extensions.contains(&"Rule 1".to_string()));
    assert!(extensions.contains(&"Rule 2".to_string()));
}

#[tokio::test]
async fn test_hook_priority_order() {
    let mut registry = HookRegistry::new();
    let call_order = Arc::new(std::sync::Mutex::new(Vec::new()));
    
    struct PriorityHook {
        priority: i32,
        order: Arc<std::sync::Mutex<Vec<i32>>>,
    }
    
    #[async_trait]
    impl Hook for PriorityHook {
        fn name(&self) -> &str {
            "priority-hook"
        }
        
        fn priority(&self) -> i32 {
            self.priority
        }
    }
    
    #[async_trait]
    impl PreToolExecutionHook for PriorityHook {
        async fn on_pre_execute(&self, _ctx: &HookCtx) -> HookResult {
            self.order.lock().unwrap().push(self.priority);
            HookResult::Ok
        }
    }
    
    // Register hooks with different priorities (should execute in ascending order)
    registry.register_pre_tool_hook(Arc::new(PriorityHook { priority: 30, order: call_order.clone() }));
    registry.register_pre_tool_hook(Arc::new(PriorityHook { priority: 10, order: call_order.clone() }));
    registry.register_pre_tool_hook(Arc::new(PriorityHook { priority: 20, order: call_order.clone() }));
    
    let ctx = HookCtx::new("test".to_string(), "{}".to_string(), "/tmp".to_string());
    let _ = registry.trigger_pre_tool_hooks(&ctx).await;
    
    let order = call_order.lock().unwrap();
    assert_eq!(*order, vec![10, 20, 30]);
}

// ============================================================================
// 缺失的 8 个 Hook Trait 单测
// ============================================================================

/// 通用计数器辅助结构
struct CountingHook {
    count: Arc<AtomicUsize>,
}

#[async_trait]
impl Hook for CountingHook {
    fn name(&self) -> &str {
        "counting-hook"
    }
}

// ── OnTurnStartHook ──────────────────────────────────────────────────────────

#[async_trait]
impl OnTurnStartHook for CountingHook {
    async fn on_turn_start(&self, _ctx: &TurnStartContext) -> HookResult {
        self.count.fetch_add(1, Ordering::SeqCst);
        HookResult::Ok
    }
}

#[tokio::test]
async fn test_on_turn_start_hook() {
    let mut registry = HookRegistry::new();
    let count = Arc::new(AtomicUsize::new(0));
    registry.register_on_turn_start_hook(Arc::new(CountingHook { count: count.clone() }));

    let ctx = TurnStartContext {
        turn_number: 1,
        session_id: None,
        working_dir: "/tmp".into(),
        phase: "execution".into(),
        has_file_context: false,
    };
    registry.trigger_on_turn_start(&ctx).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);

    // 多次触发
    registry.trigger_on_turn_start(&ctx).await;
    assert_eq!(count.load(Ordering::SeqCst), 2);
}

// ── OnToolCallStartHook ──────────────────────────────────────────────────────

/// 拒绝工具调用的 hook
struct DenyingToolCallHook {
    count: Arc<AtomicUsize>,
}

#[async_trait]
impl Hook for DenyingToolCallHook {
    fn name(&self) -> &str {
        "denying-tool-call-hook"
    }
}

#[async_trait]
impl OnToolCallStartHook for DenyingToolCallHook {
    async fn on_tool_call_start(&self, _ctx: &ToolCallStartContext) -> HookResult {
        self.count.fetch_add(1, Ordering::SeqCst);
        HookResult::Denied("tool not allowed".into())
    }
}

#[async_trait]
impl OnToolCallStartHook for CountingHook {
    async fn on_tool_call_start(&self, _ctx: &ToolCallStartContext) -> HookResult {
        self.count.fetch_add(1, Ordering::SeqCst);
        HookResult::Ok
    }
}

#[tokio::test]
async fn test_on_tool_call_start_hook_ok() {
    let mut registry = HookRegistry::new();
    let count = Arc::new(AtomicUsize::new(0));
    registry.register_on_tool_call_start_hook(Arc::new(CountingHook { count: count.clone() }));

    let ctx = ToolCallStartContext {
        tool_name: "bash".into(),
        tool_args: "{}".into(),
        call_id: "call-1".into(),
        turn_number: 1,
    };
    let result = registry.trigger_on_tool_call_start(&ctx).await;
    assert!(result.is_ok());
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_on_tool_call_start_hook_denied() {
    let mut registry = HookRegistry::new();
    let count = Arc::new(AtomicUsize::new(0));
    registry.register_on_tool_call_start_hook(Arc::new(DenyingToolCallHook { count: count.clone() }));

    let ctx = ToolCallStartContext {
        tool_name: "bash".into(),
        tool_args: "rm -rf /".into(),
        call_id: "call-1".into(),
        turn_number: 1,
    };
    let result = registry.trigger_on_tool_call_start(&ctx).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("tool not allowed"));
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

// ── OnTurnCompleteHook ───────────────────────────────────────────────────────

#[async_trait]
impl OnTurnCompleteHook for CountingHook {
    async fn on_turn_complete(&self, _ctx: &TurnCompleteContext) -> HookResult {
        self.count.fetch_add(1, Ordering::SeqCst);
        HookResult::Ok
    }
}

#[tokio::test]
async fn test_on_turn_complete_hook() {
    let mut registry = HookRegistry::new();
    let count = Arc::new(AtomicUsize::new(0));
    registry.register_on_turn_complete_hook(Arc::new(CountingHook { count: count.clone() }));

    let ctx = TurnCompleteContext {
        turn_number: 1,
        result_type: "UsedTools".into(),
        tokens_used: 500,
        tool_calls: 2,
        duration_ms: 1000,
        truncated: false,
        edited_files: vec![],
    };
    registry.trigger_on_turn_complete(&ctx).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

// ── OnMessageReceivedHook ────────────────────────────────────────────────────

/// 修改用户消息的 hook
struct ModifyingMessageHook;

#[async_trait]
impl Hook for ModifyingMessageHook {
    fn name(&self) -> &str {
        "modifying-message-hook"
    }
}

#[async_trait]
impl OnMessageReceivedHook for ModifyingMessageHook {
    async fn on_message_received(&self, ctx: &UserMessageContext) -> HookResult {
        HookResult::Modified(format!("{} [enhanced]", ctx.content))
    }
}

/// 拒绝消息的 hook
struct DenyingMessageHook;

#[async_trait]
impl Hook for DenyingMessageHook {
    fn name(&self) -> &str {
        "denying-message-hook"
    }
}

#[async_trait]
impl OnMessageReceivedHook for DenyingMessageHook {
    async fn on_message_received(&self, _ctx: &UserMessageContext) -> HookResult {
        HookResult::Denied("blocked content".into())
    }
}

#[async_trait]
impl OnMessageReceivedHook for CountingHook {
    async fn on_message_received(&self, _ctx: &UserMessageContext) -> HookResult {
        self.count.fetch_add(1, Ordering::SeqCst);
        HookResult::Ok
    }
}

#[tokio::test]
async fn test_on_message_received_hook_ok() {
    let mut registry = HookRegistry::new();
    let count = Arc::new(AtomicUsize::new(0));
    registry.register_on_message_received_hook(Arc::new(CountingHook { count: count.clone() }));

    let ctx = UserMessageContext {
        content: "hello".into(),
        session_id: None,
        attached_files: vec![],
        timestamp: "2024-01-01T00:00:00Z".into(),
    };
    let result = registry.trigger_on_message_received(&ctx).await;
    assert_eq!(result, None); // 未修改
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_on_message_received_hook_modified() {
    let mut registry = HookRegistry::new();
    registry.register_on_message_received_hook(Arc::new(ModifyingMessageHook));

    let ctx = UserMessageContext {
        content: "hello".into(),
        session_id: None,
        attached_files: vec![],
        timestamp: "2024-01-01T00:00:00Z".into(),
    };
    let result = registry.trigger_on_message_received(&ctx).await;
    assert_eq!(result, Some("hello [enhanced]".into()));
}

#[tokio::test]
async fn test_on_message_received_hook_denied() {
    let mut registry = HookRegistry::new();
    registry.register_on_message_received_hook(Arc::new(DenyingMessageHook));

    let ctx = UserMessageContext {
        content: "bad stuff".into(),
        session_id: None,
        attached_files: vec![],
        timestamp: "2024-01-01T00:00:00Z".into(),
    };
    let result = registry.trigger_on_message_received(&ctx).await;
    assert_eq!(result, None); // Denied 返回 None
}

// ── OnSessionStartHook ───────────────────────────────────────────────────────

#[async_trait]
impl OnSessionStartHook for CountingHook {
    async fn on_session_start(&self, _ctx: &SessionContext) -> HookResult {
        self.count.fetch_add(1, Ordering::SeqCst);
        HookResult::Ok
    }
}

#[tokio::test]
async fn test_on_session_start_hook() {
    let mut registry = HookRegistry::new();
    let count = Arc::new(AtomicUsize::new(0));
    registry.register_on_session_start_hook(Arc::new(CountingHook { count: count.clone() }));

    let ctx = SessionContext {
        session_id: "sess-1".into(),
        working_dir: "/tmp".into(),
        model_name: "gpt-4".into(),
        provider_name: "openai".into(),
    };
    registry.trigger_on_session_start(&ctx).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

// ── OnSessionEndHook ─────────────────────────────────────────────────────────

#[async_trait]
impl OnSessionEndHook for CountingHook {
    async fn on_session_end(&self, _ctx: &SessionContext) -> HookResult {
        self.count.fetch_add(1, Ordering::SeqCst);
        HookResult::Ok
    }
}

#[tokio::test]
async fn test_on_session_end_hook() {
    let mut registry = HookRegistry::new();
    let count = Arc::new(AtomicUsize::new(0));
    registry.register_on_session_end_hook(Arc::new(CountingHook { count: count.clone() }));

    let ctx = SessionContext {
        session_id: "sess-1".into(),
        working_dir: "/tmp".into(),
        model_name: "gpt-4".into(),
        provider_name: "openai".into(),
    };
    registry.trigger_on_session_end(&ctx).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

// ── OnErrorHook ──────────────────────────────────────────────────────────────

#[async_trait]
impl OnErrorHook for CountingHook {
    async fn on_error(&self, _ctx: &ErrorContext) -> HookResult {
        self.count.fetch_add(1, Ordering::SeqCst);
        HookResult::Ok
    }
}

#[tokio::test]
async fn test_on_error_hook() {
    let mut registry = HookRegistry::new();
    let count = Arc::new(AtomicUsize::new(0));
    registry.register_on_error_hook(Arc::new(CountingHook { count: count.clone() }));

    let ctx = ErrorContext {
        error_type: "RateLimit".into(),
        error_message: "too many requests".into(),
        phase: "execution".into(),
        turn_number: Some(1),
    };
    registry.trigger_on_error(&ctx).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

// ── OnModelResponseHook ──────────────────────────────────────────────────────

#[async_trait]
impl OnModelResponseHook for CountingHook {
    async fn on_model_response(&self, _response: &str, _turn_ctx: &TurnStartContext) -> HookResult {
        self.count.fetch_add(1, Ordering::SeqCst);
        HookResult::Ok
    }
}

#[tokio::test]
async fn test_on_model_response_hook() {
    let mut registry = HookRegistry::new();
    let count = Arc::new(AtomicUsize::new(0));
    registry.register_on_model_response_hook(Arc::new(CountingHook { count: count.clone() }));

    let turn_ctx = TurnStartContext {
        turn_number: 1,
        session_id: None,
        working_dir: "/tmp".into(),
        phase: "execution".into(),
        has_file_context: false,
    };
    registry.trigger_on_model_response("Hello world", &turn_ctx).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

// ============================================================================
// HookStats 完整性验证 — 确保所有 12 个字段都被正确统计
// ============================================================================

#[tokio::test]
async fn test_hook_stats_completeness() {
    let mut registry = HookRegistry::new();
    let count = Arc::new(AtomicUsize::new(0));

    // 每个 hook 类型注册一个
    registry.register_pre_tool_hook(Arc::new(CountingPreHook { count: count.clone() }));
    registry.register_post_tool_hook(Arc::new(CountingPostHook { count: count.clone() }));
    registry.register_post_turn_hook(Arc::new(CountingPostTurnHook { count: count.clone() }));
    registry.register_system_prompt_hook(Arc::new(TestSystemPromptHook { content: "rule".into() }));
    registry.register_on_message_received_hook(Arc::new(CountingHook { count: count.clone() }));
    registry.register_on_turn_start_hook(Arc::new(CountingHook { count: count.clone() }));
    registry.register_on_tool_call_start_hook(Arc::new(CountingHook { count: count.clone() }));
    registry.register_on_turn_complete_hook(Arc::new(CountingHook { count: count.clone() }));
    registry.register_on_session_start_hook(Arc::new(CountingHook { count: count.clone() }));
    registry.register_on_session_end_hook(Arc::new(CountingHook { count: count.clone() }));
    registry.register_on_error_hook(Arc::new(CountingHook { count: count.clone() }));
    registry.register_on_model_response_hook(Arc::new(CountingHook { count: count.clone() }));

    let stats = registry.stats();
    assert_eq!(stats.pre_tool_hooks, 1, "pre_tool_hooks");
    assert_eq!(stats.post_tool_hooks, 1, "post_tool_hooks");
    assert_eq!(stats.post_turn_hooks, 1, "post_turn_hooks");
    assert_eq!(stats.system_prompt_hooks, 1, "system_prompt_hooks");
    assert_eq!(stats.on_message_received_hooks, 1, "on_message_received_hooks");
    assert_eq!(stats.on_turn_start_hooks, 1, "on_turn_start_hooks");
    assert_eq!(stats.on_tool_call_start_hooks, 1, "on_tool_call_start_hooks");
    assert_eq!(stats.on_turn_complete_hooks, 1, "on_turn_complete_hooks");
    assert_eq!(stats.on_session_start_hooks, 1, "on_session_start_hooks");
    assert_eq!(stats.on_session_end_hooks, 1, "on_session_end_hooks");
    assert_eq!(stats.on_error_hooks, 1, "on_error_hooks");
    assert_eq!(stats.on_model_response_hooks, 1, "on_model_response_hooks");
}

// ============================================================================
// is_enabled 测试 — disabled hook 不应被注册到 registry 中
// ============================================================================

struct DisabledPreHook;

#[async_trait]
impl Hook for DisabledPreHook {
    fn name(&self) -> &str {
        "disabled-pre-hook"
    }

    fn is_enabled(&self) -> bool {
        false // 显式禁用
    }
}

#[async_trait]
impl PreToolExecutionHook for DisabledPreHook {
    async fn on_pre_execute(&self, _ctx: &HookCtx) -> HookResult {
        HookResult::Denied("should never be called".into())
    }
}

#[tokio::test]
async fn test_disabled_hook_not_registered() {
    let mut registry = HookRegistry::new();

    // 注册一个 disabled hook
    registry.register_pre_tool_hook(Arc::new(DisabledPreHook));

    // 注册一个 enabled hook 来验证 registry 工作正常
    let count = Arc::new(AtomicUsize::new(0));
    registry.register_pre_tool_hook(Arc::new(CountingPreHook { count: count.clone() }));

    // 只有 1 个 hook 被注册（disabled 的被跳过）
    let stats = registry.stats();
    assert_eq!(stats.pre_tool_hooks, 1);

    // 正常触发 enabled hook
    let ctx = HookCtx::new("test".into(), "{}".into(), "/tmp".into());
    let result = registry.trigger_pre_tool_hooks(&ctx).await;
    assert!(result.is_ok());
    assert_eq!(count.load(Ordering::SeqCst), 1);
}
