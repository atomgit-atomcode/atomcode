use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;

use atomcode_core::hook::{
    Hook, HookContext, HookRegistry, HookResult, PostToolExecutionHook, PostTurnHook,
    PreToolExecutionHook, SystemPromptHook, ToolResultContext,
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
    async fn on_pre_execute(&self, _ctx: &HookContext) -> HookResult {
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
    async fn on_post_execute(
        &self,
        _ctx: &HookContext,
        _result_ctx: &ToolResultContext,
    ) -> HookResult {
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
    async fn on_post_turn(&self, _ctx: &HookContext, _turn_result: &str) -> HookResult {
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

    let pre_hook = Arc::new(CountingPreHook {
        count: pre_count.clone(),
    });
    let post_hook = Arc::new(CountingPostHook {
        count: post_count.clone(),
    });
    let post_turn_hook = Arc::new(CountingPostTurnHook {
        count: post_turn_count.clone(),
    });

    registry.register_pre_tool_hook(pre_hook);
    registry.register_post_tool_hook(post_hook);
    registry.register_post_turn_hook(post_turn_hook);

    // Verify stats
    let stats = registry.stats();
    assert_eq!(stats.pre_tool_hooks, 1);
    assert_eq!(stats.post_tool_hooks, 1);
    assert_eq!(stats.post_turn_hooks, 1);

    // Test pre-tool hooks
    let ctx = HookContext::new(
        "test_tool".to_string(),
        "{}".to_string(),
        "/tmp".to_string(),
    );
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
        async fn on_pre_execute(&self, _ctx: &HookContext) -> HookResult {
            HookResult::Denied("Security policy violation".to_string())
        }
    }

    registry.register_pre_tool_hook(Arc::new(DenyingHook));

    let ctx = HookContext::new(
        "bash".to_string(),
        "rm -rf /".to_string(),
        "/tmp".to_string(),
    );
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
        async fn on_pre_execute(&self, _ctx: &HookContext) -> HookResult {
            HookResult::Modified("{\"modified\": true}".to_string())
        }
    }

    registry.register_pre_tool_hook(Arc::new(ModifyingHook));

    let ctx = HookContext::new(
        "edit_file".to_string(),
        "{}".to_string(),
        "/tmp".to_string(),
    );
    let result = registry.trigger_pre_tool_hooks(&ctx).await;

    assert!(result.is_ok());
    let modified_args = result.unwrap();
    assert!(modified_args.is_some());
    assert_eq!(modified_args.unwrap(), "{\"modified\": true}");
}

#[tokio::test]
async fn test_system_prompt_hook() {
    let mut registry = HookRegistry::new();

    let hook1 = Arc::new(TestSystemPromptHook {
        content: "Rule 1".to_string(),
    });
    let hook2 = Arc::new(TestSystemPromptHook {
        content: "Rule 2".to_string(),
    });

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
        async fn on_pre_execute(&self, _ctx: &HookContext) -> HookResult {
            self.order.lock().unwrap().push(self.priority);
            HookResult::Ok
        }
    }

    // Register hooks with different priorities (should execute in ascending order)
    registry.register_pre_tool_hook(Arc::new(PriorityHook {
        priority: 30,
        order: call_order.clone(),
    }));
    registry.register_pre_tool_hook(Arc::new(PriorityHook {
        priority: 10,
        order: call_order.clone(),
    }));
    registry.register_pre_tool_hook(Arc::new(PriorityHook {
        priority: 20,
        order: call_order.clone(),
    }));

    let ctx = HookContext::new("test".to_string(), "{}".to_string(), "/tmp".to_string());
    let _ = registry.trigger_pre_tool_hooks(&ctx).await;

    let order = call_order.lock().unwrap();
    assert_eq!(*order, vec![10, 20, 30]);
}
