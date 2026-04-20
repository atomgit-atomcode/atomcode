//! Context construction strategies — one entry point per model/provider.
//!
//! The top of the module exposes a single [`CtxBuilder`] trait. Each
//! concrete impl (e.g. [`DefaultCtx`]) decides how to assemble the
//! messages array sent to the LLM for a given session.
//!
//! When no per-model logic is needed, [`DefaultCtx`] preserves the
//! legacy behavior of `Conversation::to_provider_messages_budgeted`.
//! Users wanting to customize context for a specific model:
//!
//! 1. Add a new file `ctx/foo.rs` with a struct implementing
//!    [`CtxBuilder`].
//! 2. Register it in [`for_provider`] with a prefix / provider match.
//!
//! The trait is narrow on purpose: `build_messages` owns the full
//! render path for its model, including any system-prompt variation,
//! cold-zone handling, or tool-schema trimming. The shared
//! [`crate::ctx::truncate`] module offers building blocks that impls
//! may call or ignore at will.

pub mod default;
pub mod truncate;

use crate::config::provider::ProviderConfig;
use crate::conversation::{ContextStats, Conversation};
use crate::conversation::message::Message;
use crate::tool::ToolResult;

pub use default::DefaultCtx;

/// Per-session context construction strategy. Selected once at
/// `AgentLoop::new` via [`for_provider`] and rebuilt on `ReloadConfig`.
pub trait CtxBuilder: Send + Sync {
    /// Build the messages array to send to the LLM for this turn.
    ///
    /// Implementations are free to transform `system_prompt` (strip
    /// tool schemas for small models, add cache-friendly markers for
    /// Claude, replace entirely for fine-tuned models) and to choose
    /// any render pipeline (delegate to
    /// `Conversation::to_provider_messages_budgeted`, call shared
    /// helpers directly, or roll their own).
    fn build_messages(
        &self,
        conv: &Conversation,
        system_prompt: &str,
    ) -> (Vec<Message>, ContextStats);

    /// Whether the conversation should be compressed. Default: never.
    fn needs_compression(&self, _conv: &Conversation, _system_tokens: usize) -> bool {
        false
    }

    /// Produce a compression plan `(content_to_summarize,
    /// messages_to_remove)`. Default: `None` (no compression).
    fn compression_plan(&self, _conv: &Conversation) -> Option<(String, usize)> {
        None
    }

    /// Truncate a single tool output in place.
    fn truncate_tool_output(&self, result: &mut ToolResult, tool_name: &str);

    /// Human-readable name for logging / debugging.
    fn name(&self) -> &'static str;
}

/// Resolve the [`CtxBuilder`] for a given provider configuration.
///
/// Add new per-model implementations by inserting a match arm BEFORE
/// the final `DefaultCtx` fallback. Example:
///
/// ```text
/// if provider.provider_type == "ollama" {
///     return Box::new(OllamaCtx::new(provider));
/// }
/// if provider.model.starts_with("claude-") {
///     return Box::new(ClaudeCtx::new(provider));
/// }
/// ```
///
/// Unmatched models always fall through to [`DefaultCtx`], which
/// preserves the current behavior unchanged.
pub fn for_provider(provider: &ProviderConfig) -> Box<dyn CtxBuilder> {
    // 未命中任何具体模型规则 → 默认策略
    Box::new(DefaultCtx::new(provider))
}
