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
pub mod ollama;
pub mod render;
pub mod resolver;
pub mod truncate;

use crate::conversation::{ContextStats, Conversation};
use crate::conversation::message::Message;
use crate::tool::ToolResult;

pub use default::DefaultCtx;
pub use ollama::OllamaCtx;
pub use resolver::for_provider;

/// Per-session context construction strategy. Selected once at
/// `AgentLoop::new` via [`for_provider`] and rebuilt on `ReloadConfig`.
pub trait CtxBuilder: Send + Sync {
    /// Build the messages array to send to the LLM for this turn.
    ///
    /// Implementations are free to transform `system_prompt` (strip
    /// tool schemas for small models, add cache-friendly markers for
    /// Claude, replace entirely for fine-tuned models) and to choose
    /// any render pipeline (delegate to [`crate::ctx::render::build_messages`],
    /// call shared helpers directly, or roll their own).
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
