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

    #[test]
    fn compression_plan_none_below_threshold() {
        let d = DefaultCtx::new(&test_provider(128_000));
        let conv = Conversation::new();
        assert!(d.compression_plan(&conv).is_none());
    }
}
