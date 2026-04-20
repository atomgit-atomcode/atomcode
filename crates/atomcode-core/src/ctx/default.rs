//! [`DefaultCtx`] — the fallback [`CtxBuilder`] implementation.
//!
//! Preserves the current atomcode context behavior: delegates every
//! method to the legacy [`Conversation`] helpers keyed on
//! `ctx_window`. Any model not matched by a rule in
//! [`super::for_provider`] lands here.

use super::CtxBuilder;
use crate::config::provider::ProviderConfig;
use crate::conversation::{ContextStats, Conversation};
use crate::conversation::message::Message;
use crate::tool::ToolResult;

/// Fallback strategy — matches legacy behavior byte-for-byte.
#[derive(Debug, Clone)]
pub struct DefaultCtx {
    /// Token budget for this provider (from `ProviderConfig.context_window`,
    /// clamped to a defensive minimum of 8000 to avoid divide-by-zero
    /// and thrashing in pathological configs).
    pub ctx_window: usize,
}

impl DefaultCtx {
    /// Construct a `DefaultCtx` from a provider config.
    pub fn new(provider: &ProviderConfig) -> Self {
        Self {
            ctx_window: provider.context_window.max(8000),
        }
    }
}

impl CtxBuilder for DefaultCtx {
    fn build_messages(
        &self,
        conv: &Conversation,
        system_prompt: &str,
    ) -> (Vec<Message>, ContextStats) {
        conv.to_provider_messages_budgeted(system_prompt, self.ctx_window)
    }

    fn needs_compression(&self, conv: &Conversation, system_tokens: usize) -> bool {
        conv.needs_compression(system_tokens, self.ctx_window)
    }

    fn compression_plan(&self, conv: &Conversation) -> Option<(String, usize)> {
        let (content, n) = conv.build_compression_content();
        if content.is_empty() || n == 0 {
            None
        } else {
            Some((content, n))
        }
    }

    fn truncate_tool_output(&self, result: &mut ToolResult, tool_name: &str) {
        super::truncate::truncate_output(result, tool_name, self.ctx_window);
    }

    fn name(&self) -> &'static str {
        "default"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::provider::ProviderConfig;

    fn test_provider(ctx: usize) -> ProviderConfig {
        ProviderConfig {
            provider_type: "test".into(),
            api_key: None,
            model: "test-model".into(),
            base_url: None,
            system_prompt: None,
            user_agent: None,
            context_window: ctx,
            max_tokens: None,
            ephemeral: false,
        }
    }

    #[test]
    fn name_is_default() {
        let d = DefaultCtx::new(&test_provider(128_000));
        assert_eq!(d.name(), "default");
    }

    #[test]
    fn ctx_window_clamped_to_8k_minimum() {
        let d = DefaultCtx::new(&test_provider(0));
        assert_eq!(d.ctx_window, 8000);
        let d = DefaultCtx::new(&test_provider(4_000));
        assert_eq!(d.ctx_window, 8000);
        let d = DefaultCtx::new(&test_provider(32_000));
        assert_eq!(d.ctx_window, 32_000);
    }

    #[test]
    fn build_messages_empty_conv_returns_system_only() {
        let d = DefaultCtx::new(&test_provider(128_000));
        let conv = Conversation::new();
        let (msgs, _stats) = d.build_messages(&conv, "SYS");
        assert_eq!(msgs.len(), 1);
        assert!(matches!(
            msgs[0].role,
            crate::conversation::message::Role::System
        ));
    }

    /// Helper: compare two `Vec<Message>` via serde_json (avoids requiring
    /// PartialEq on Message / MessageContent / ToolCall / ToolResult).
    fn msgs_equal(a: &[Message], b: &[Message]) -> bool {
        a.len() == b.len()
            && serde_json::to_string(a).unwrap() == serde_json::to_string(b).unwrap()
    }

    #[test]
    fn compression_plan_none_below_threshold() {
        let d = DefaultCtx::new(&test_provider(128_000));
        let conv = Conversation::new();
        assert!(d.compression_plan(&conv).is_none());
    }

    // ─────────────────────────────────────────────────────────────
    // Byte-equivalence regression: DefaultCtx MUST produce bit-for-bit
    // the same messages as calling Conversation::to_provider_messages_budgeted
    // directly. If a future change silently alters this, this test
    // fires — a clear signal that the "unmatched model → default
    // behavior" contract in ctx::for_provider has been broken.
    // ─────────────────────────────────────────────────────────────

    fn build_nontrivial_conv() -> Conversation {
        use crate::tool::{ToolCall, ToolResult};

        let mut conv = Conversation::new();
        conv.add_user_message("让我看一下 src/main.rs");
        conv.add_assistant_tool_calls(
            Some("我先读一下文件"),
            vec![ToolCall {
                id: "tc1".into(),
                name: "read_file".into(),
                arguments: r#"{"path":"src/main.rs"}"#.to_string(),
            }],
        );
        conv.add_tool_result(ToolResult {
            call_id: "tc1".into(),
            output: "fn main() {\n    println!(\"hello\");\n}".repeat(200),
            success: true,
        });
        conv.add_user_message("看起来不错,加一个 --verbose flag");
        conv.add_assistant_tool_calls(
            Some("改造 main.rs"),
            vec![ToolCall {
                id: "tc2".into(),
                name: "edit_file".into(),
                arguments: r#"{"path":"src/main.rs"}"#.to_string(),
            }],
        );
        conv.add_tool_result(ToolResult {
            call_id: "tc2".into(),
            output: "Edited src/main.rs: added --verbose handling".into(),
            success: true,
        });
        conv.add_user_message("写一下单元测试");
        conv
    }

    #[test]
    fn default_ctx_matches_legacy_render_byte_for_byte() {
        let conv = build_nontrivial_conv();
        let sys = "System prompt for test";
        let ctx_window = 128_000;

        let (legacy_msgs, legacy_stats) = conv.to_provider_messages_budgeted(sys, ctx_window);
        let d = DefaultCtx::new(&test_provider(ctx_window));
        let (new_msgs, new_stats) = d.build_messages(&conv, sys);

        // 消息序列完全一致(走 serde_json 比较避免引入 PartialEq bloat)
        assert!(
            msgs_equal(&legacy_msgs, &new_msgs),
            "DefaultCtx.build_messages must byte-match legacy render"
        );
        // 统计字段逐项对齐 (ContextStats 没 PartialEq)
        assert_eq!(legacy_stats.system_tokens, new_stats.system_tokens);
        assert_eq!(legacy_stats.sent_tokens, new_stats.sent_tokens);
        assert_eq!(legacy_stats.dropped_tokens, new_stats.dropped_tokens);
        assert_eq!(legacy_stats.total_messages, new_stats.total_messages);
    }

    #[test]
    fn default_ctx_compression_plan_matches_legacy() {
        // Build conv large enough to trigger compression_plan.
        let mut conv = Conversation::new();
        for i in 0..25 {
            conv.add_user_message(&format!("turn {} with some content", i));
            conv.add_assistant_tool_calls(
                Some(&format!("reasoning for turn {}", i)),
                vec![],
            );
        }

        let legacy = conv.build_compression_content();
        let d = DefaultCtx::new(&test_provider(128_000));
        let new = d.compression_plan(&conv);

        match new {
            Some((content, n)) => {
                assert_eq!(legacy.0, content, "compression content must match");
                assert_eq!(legacy.1, n, "messages_to_remove must match");
            }
            None => {
                // DefaultCtx returns None when legacy returned empty/0
                assert!(legacy.0.is_empty() || legacy.1 == 0,
                    "new returned None but legacy returned real plan");
            }
        }
    }
}
