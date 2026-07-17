//! Context construction strategies — one entry point per model/provider.
//!
//! This module owns the [`CtxBuilder`] trait (defined below) and the
//! registry function [`for_provider`] (in [`resolver`]). Each concrete
//! builder lives in its own file to keep [`mod.rs`](self) thin:
//!
//! | File          | Role                                                  |
//! |---------------|-------------------------------------------------------|
//! | `mod.rs`      | `CtxBuilder` trait definition + re-exports            |
//! | `resolver.rs` | `for_provider` dispatch (add new models here)         |
//! | `render.rs`   | Default render / compression-plan policy              |
//! | `default.rs`  | `DefaultCtx` — thin wrapper over `render`             |
//! | `ollama.rs`   | `OllamaCtx` — small-window local models               |
//! | `truncate.rs` | Shared per-tool truncation helpers                    |
//!
//! Adding a new per-model ctx strategy:
//!
//! 1. Create `ctx/<name>.rs` with a `pub struct XxxCtx` and
//!    `impl CtxBuilder for XxxCtx`. Keep all ctor + tests + impl
//!    methods in that single file.
//! 2. Declare `pub mod <name>;` below.
//! 3. Register in `resolver::for_provider`.
//!
//! The trait is narrow on purpose: `build_messages` owns the full
//! render path for its model, including any system-prompt variation,
//! cold-zone handling, or tool-schema trimming. The shared
//! [`render`] and [`truncate`] modules offer building blocks that
//! impls may call or ignore at will.

pub mod default;
pub mod env;
pub mod file_store;
pub mod ollama;
pub mod render;
pub mod resolver;
pub mod stats;
pub mod truncate;

use crate::conversation::message::Message;
use crate::conversation::{ContextStats, Conversation};
use crate::tool::ToolResult;

pub use default::DefaultCtx;
pub use env::EnvSnapshot;
pub use ollama::OllamaCtx;
pub use resolver::for_provider;
pub use stats::{compute_rich_context_stats, RichContextStats};

/// Per-session context construction strategy. Selected once at
/// `AgentLoop::new` via [`for_provider`] and rebuilt on `ReloadConfig`.
pub trait CtxBuilder: Send + Sync {
    /// Build the messages array to send to the LLM for this turn.
    ///
    /// Implementations are free to:
    /// - Transform `system_prompt` (strip tool schemas for small models,
    ///   add cache-friendly markers for Claude, replace entirely for
    ///   fine-tuned models)
    /// - Choose any render pipeline (delegate to
    ///   [`crate::ctx::render::build_messages`], call shared helpers
    ///   directly, or roll their own)
    /// - Decide how to handle `turn_reminder` — per-turn dynamic
    ///   context (git status, current task, prev edited files). The
    ///   default policy prepends it to the last User message for
    ///   system-prompt cache stability; a small-window model might
    ///   drop it to save tokens; a cache-sensitive model might insert
    ///   it as its own System message to keep the stable prefix clean.
    ///   Pass `""` when no reminder applies.
    fn build_messages(
        &self,
        conv: &Conversation,
        system_prompt: &str,
        turn_reminder: &str,
    ) -> (Vec<Message>, ContextStats);

    /// Whether the conversation should be compressed. Default: never.
    fn needs_compression(&self, _conv: &Conversation, _system_tokens: usize) -> bool {
        false
    }

    /// Produce a compression plan `(content_to_summarize,
    /// messages_to_remove)`. `keep_ceiling` is the max tokens the kept tail
    /// may occupy (window minus system/tools/cold-zone/output reserve), so
    /// reasoning-heavy tails get drained to fit. Default: `None`.
    fn compression_plan(
        &self,
        _conv: &Conversation,
        _keep_ceiling: usize,
    ) -> Option<(String, usize)> {
        None
    }

    /// Truncate a single tool output in place.
    fn truncate_tool_output(&self, result: &mut ToolResult, tool_name: &str);

    /// Effective token budget for this strategy.
    ///
    /// Reflects any defensive clamps the impl applies (e.g. `OllamaCtx`
    /// floors at 4K even if `provider.context_window == 0`). Callers
    /// that need the actual budget should use this instead of
    /// `Config::default_context_window()`, which returns the raw,
    /// unclamped value and may diverge for degenerate configs.
    fn ctx_window(&self) -> usize;

    /// Human-readable name for logging / debugging.
    fn name(&self) -> &'static str;
}
