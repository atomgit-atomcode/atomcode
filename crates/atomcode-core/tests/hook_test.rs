//! Simplified HookEngine integration tests.
//!
//! These tests validate the unified HookEngine replaces HookRegistry's core
//! registration and trigger paths. Old HookRegistry-only features (stats(),
//! trigger_on_message_received, etc.) are not in HookEngine and their tests
//! have been removed.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use atomcode_core::hook::{
    Hook, HookCtx, HookEngine, HookResult,
    PreToolExecutionHook, PostToolExecutionHook, PostTurnHook, SystemPromptHook,
    OnSessionStartHook,
    ToolResultContext, SessionContext,
};

// ============================================================================
// Test helpers
// ============================================================================

struct CountingPreHook {
    count: Arc<AtomicUsize>,
}

#[async_trait]
impl Hook for CountingPreHook {
    fn name(&self) -> &str { "counting-pre" }
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
    fn name(&self) -> &str { "counting-post" }
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
    fn name(&self) -> &str { "counting-post-turn" }
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
    fn name(&self) -> &str { "test-system-prompt" }
}

#[async_trait]
impl SystemPromptHook for TestSystemPromptHook {
    async fn extend_system_prompt(&self) -> Option<String> {
        Some(self.content.clone())
    }
}

// ============================================================================
// Tests
// ============================================================================

#[tokio::test]
async fn test_hook_engine_basic() {
    let mut engine = HookEngine::new();
    
    let pre_count = Arc::new(AtomicUsize::new(0));
    let post_count = Arc::new(AtomicUsize::new(0));
    let post_turn_count = Arc::new(AtomicUsize::new(0));
    
    let pre_hook = Arc::new(CountingPreHook { count: pre_count.clone() });
    let post_hook = Arc::new(CountingPostHook { count: post_count.clone() });
    let post_turn_hook = Arc::new(CountingPostTurnHook { count: post_turn_count.clone() });
    
    engine.register_pre_tool_hook(pre_hook);
    engine.register_post_tool_hook(post_hook);
    engine.register_post_turn_hook(post_turn_hook);
    
    // Test pre-tool hooks
    let ctx = HookCtx::new("test_tool".to_string(), "{}".to_string(), "/tmp".to_string());
    let result = engine.trigger_pre_tool_use(&ctx).await;
    assert!(result.is_ok());
    assert_eq!(pre_count.load(Ordering::SeqCst), 1);
    
    // Test post-tool hooks
    let result_ctx = ToolResultContext {
        tool_name: "test_tool".to_string(),
        tool_args: "{}".to_string(),
        result: "success".to_string(),
        success: true,
        duration_ms: 100,
    };
    engine.trigger_post_tool_use(&ctx, &result_ctx).await;
    assert_eq!(post_count.load(Ordering::SeqCst), 1);
    
    // Test post-turn hooks
    engine.trigger_post_turn(&ctx, "Responded").await;
    assert_eq!(post_turn_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_hook_engine_deny() {
    let mut engine = HookEngine::new();
    
    struct DenyingHook;
    
    #[async_trait]
    impl Hook for DenyingHook {
        fn name(&self) -> &str { "denying-hook" }
    }
    
    #[async_trait]
    impl PreToolExecutionHook for DenyingHook {
        async fn on_pre_execute(&self, _ctx: &HookCtx) -> HookResult {
            HookResult::Denied("Security policy violation".to_string())
        }
    }
    
    engine.register_pre_tool_hook(Arc::new(DenyingHook));
    
    let ctx = HookCtx::new("bash".to_string(), "rm -rf /".to_string(), "/tmp".to_string());
    let result = engine.trigger_pre_tool_use(&ctx).await;
    
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Security policy violation"));
}

#[tokio::test]
async fn test_hook_engine_modify_args() {
    let mut engine = HookEngine::new();
    
    struct ModifyingHook;
    
    #[async_trait]
    impl Hook for ModifyingHook {
        fn name(&self) -> &str { "modifying-hook" }
    }
    
    #[async_trait]
    impl PreToolExecutionHook for ModifyingHook {
        async fn on_pre_execute(&self, _ctx: &HookCtx) -> HookResult {
            HookResult::Modified("{\"modified\": true}".to_string())
        }
    }
    
    engine.register_pre_tool_hook(Arc::new(ModifyingHook));
    
    let ctx = HookCtx::new("edit_file".to_string(), "{}".to_string(), "/tmp".to_string());
    let result = engine.trigger_pre_tool_use(&ctx).await;
    
    assert!(result.is_ok());
    let (modified_args, _) = result.unwrap();
    assert!(modified_args.is_some());
    assert_eq!(modified_args.unwrap(), "{\"modified\": true}");
}

/// Issue #512: PreToolUse hook returning "allow" must set the
/// `explicitly_allowed` flag so the caller can skip the approval prompt.
#[tokio::test]
async fn test_hook_engine_explicit_allow() {
    let mut engine = HookEngine::new();

    struct AllowingHook;

    #[async_trait]
    impl Hook for AllowingHook {
        fn name(&self) -> &str { "allowing-hook" }
    }

    #[async_trait]
    impl PreToolExecutionHook for AllowingHook {
        async fn on_pre_execute(&self, _ctx: &HookCtx) -> HookResult {
            HookResult::ExplicitAllow
        }
    }

    engine.register_pre_tool_hook(Arc::new(AllowingHook));

    let ctx = HookCtx::new("bash".to_string(), "{}".to_string(), "/tmp".to_string());
    let result = engine.trigger_pre_tool_use(&ctx).await;

    assert!(result.is_ok());
    let (args, allowed) = result.unwrap();
    assert!(args.is_none());   // no arg modification
    assert!(allowed);          // explicit allow → skip approval
}

#[tokio::test]
async fn test_hook_engine_system_prompt() {
    let mut engine = HookEngine::new();
    
    let hook1 = Arc::new(TestSystemPromptHook { content: "Rule 1".into() });
    let hook2 = Arc::new(TestSystemPromptHook { content: "Rule 2".into() });
    
    engine.register_system_prompt_hook(hook1);
    engine.register_system_prompt_hook(hook2);
    
    let extensions = engine.collect_system_prompt_extensions().await;
    
    assert_eq!(extensions.len(), 2);
    assert_eq!(extensions[0], "Rule 1");
    assert_eq!(extensions[1], "Rule 2");
}

#[tokio::test]
async fn test_hook_engine_has_any() {
    let engine = HookEngine::new();
    assert!(!engine.has_any());
    
    let mut engine = HookEngine::new();
    let pre_hook = Arc::new(CountingPreHook { count: Arc::new(AtomicUsize::new(0)) });
    engine.register_pre_tool_hook(pre_hook);
    assert!(engine.has_any());
}

#[tokio::test]
async fn test_hook_engine_disabled_hook_not_registered() {
    struct DisabledHook;
    
    #[async_trait]
    impl Hook for DisabledHook {
        fn name(&self) -> &str { "disabled" }
        fn is_enabled(&self) -> bool { false }
    }
    
    #[async_trait]
    impl PreToolExecutionHook for DisabledHook {
        async fn on_pre_execute(&self, _ctx: &HookCtx) -> HookResult {
            HookResult::Ok
        }
    }
    
    let mut engine = HookEngine::new();
    engine.register_pre_tool_hook(Arc::new(DisabledHook));
    // Disabled hooks should not be registered
    assert!(!engine.has_any());
}

#[tokio::test]
async fn test_hook_engine_session_lifecycle() {
    let mut engine = HookEngine::new();
    
    let count = Arc::new(AtomicUsize::new(0));
    
    struct SessionHook {
        count: Arc<AtomicUsize>,
    }
    
    #[async_trait]
    impl Hook for SessionHook {
        fn name(&self) -> &str { "session-hook" }
    }
    
    #[async_trait]
    impl OnSessionStartHook for SessionHook {
        async fn on_session_start(&self, _ctx: &SessionContext) -> HookResult {
            self.count.fetch_add(1, Ordering::SeqCst);
            HookResult::Ok
        }
    }
    
    let hook = Arc::new(SessionHook { count: count.clone() });
    engine.register_on_session_start_hook(hook);
    
    let ctx = atomcode_core::hook::SessionContext {
        session_id: "test-s1".into(),
        working_dir: "/tmp".into(),
        model_name: "test-model".into(),
        provider_name: "mock".into(),
    };
    engine.trigger_session_start(&ctx).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
}
