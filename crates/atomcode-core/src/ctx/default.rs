//! [`DefaultCtx`] — the fallback [`CtxBuilder`] implementation.
//!
//! Thin wrapper around [`crate::ctx::render`]: every method delegates
//! to the default render/compression policy, parameterized only by
//! `ctx_window`. Any model not matched by a rule in
//! [`super::for_provider`] lands here.

use super::CtxBuilder;
use crate::conversation::message::Message;
use crate::conversation::{ContextStats, Conversation};
use crate::tool::ToolResult;
use atomcode_config::config::provider::ProviderConfig;

/// Fallback strategy — matches legacy behavior byte-for-byte.
#[derive(Debug, Clone)]
pub struct DefaultCtx {
    /// Token budget for this provider (from `ProviderConfig.context_window`,
    /// clamped to a defensive minimum of 8000 to avoid divide-by-zero
    /// and thrashing in pathological configs).
    pub ctx_window: usize,

    /// Lowercased model id. Used by
    /// [`crate::ctx::render::apply_model_directives`] to decide whether
    /// to append CJK language lock / MiniMax thinking discipline to the
    /// system prompt. Captured at construction time so `build_messages`
    /// stays `&self`.
    model_id: String,
}

impl DefaultCtx {
    /// Construct a `DefaultCtx` from a provider config.
    pub fn new(provider: &ProviderConfig) -> Self {
        Self {
            ctx_window: provider.context_window.max(8000),
            model_id: provider.model.to_lowercase(),
        }
    }
}

impl CtxBuilder for DefaultCtx {
    fn build_messages(
        &self,
        conv: &Conversation,
        system_prompt: &str,
        turn_reminder: &str,
    ) -> (Vec<Message>, ContextStats) {
        let sys = crate::ctx::render::apply_model_directives(system_prompt, &self.model_id);
        crate::ctx::render::build_messages(conv, &sys, self.ctx_window, turn_reminder)
    }

    fn needs_compression(&self, conv: &Conversation, system_tokens: usize) -> bool {
        crate::ctx::render::needs_compression(conv, system_tokens, self.ctx_window)
    }

    fn compression_plan(
        &self,
        conv: &Conversation,
        keep_ceiling: usize,
    ) -> Option<(String, usize)> {
        let (content, n) = crate::ctx::render::build_compression_content(conv, keep_ceiling);
        if content.is_empty() || n == 0 {
            None
        } else {
            Some((content, n))
        }
    }

    fn truncate_tool_output(&self, result: &mut ToolResult, tool_name: &str) {
        crate::ctx::truncate::truncate_output(result, tool_name, self.ctx_window);
    }

    fn ctx_window(&self) -> usize {
        self.ctx_window
    }

    fn name(&self) -> &'static str {
        "default"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_config::config::provider::ProviderConfig;

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
            thinking_type: None,
            thinking_keep: None,
            reasoning_history: None,
            reasoning_effort: None,
            thinking_enabled: None,
            thinking_budget: None,
            skip_tls_verify: false,
            ephemeral: false,
            capable_model: None,
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
        let (msgs, _stats) = d.build_messages(&conv, "SYS", "");
        assert_eq!(msgs.len(), 1);
        assert!(matches!(
            msgs[0].role,
            crate::conversation::message::Role::System
        ));
    }

    #[test]
    fn compression_plan_none_below_threshold() {
        let d = DefaultCtx::new(&test_provider(128_000));
        let conv = Conversation::new();
        assert!(d.compression_plan(&conv, usize::MAX).is_none());
    }
}
