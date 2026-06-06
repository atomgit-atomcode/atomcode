use crate::message::{Conversation, Message};
use async_trait::async_trait;

/// Per-LLM-call execution context handed to hooks that need to know where in the
/// loop they are (e.g. to project round budget to the LLM).
#[derive(Clone, Copy, Debug, Default)]
pub struct TurnCtx {
    /// 1-based index of the LLM call about to execute this turn.
    pub round: u32,
    /// Optional hard cap on rounds per turn (None = unlimited).
    pub max_rounds: Option<u32>,
}

/// TURN-level lifecycle seam (session / turn / request / response / error). The
/// "inject into the loop" side, distinct from the read-only AgentEvent stream.
/// TOOL-level concerns (gate/rewrite/transform a tool call) live in
/// `ToolMiddleware`, not here. Every method defaults to no-op.
///
/// Each method states whether a mutation is PERMANENT (written into stored
/// conversation history) or EPHEMERAL (affects only the current request).
#[async_trait]
pub trait LifecycleHooks: Send + Sync {
    /// Session begins, before any turn. Mutate the conversation to inject seed
    /// context / persona. PERMANENT (stored).
    async fn session_start(&self, _convo: &mut Conversation) {}

    /// A user message is about to enter the loop. Rewrite / augment the text, or
    /// return `Err(reason)` to BLOCK the prompt — it never enters the conversation
    /// and no turn runs (the driver gets an Error + TurnComplete). PERMANENT (the
    /// rewritten text is stored when allowed).
    async fn user_prompt_submit(&self, _text: &mut String) -> Result<(), String> {
        Ok(())
    }

    /// Before a turn's first LLM call — fires ONCE per user message, not per round.
    /// Mutate the conversation. PERMANENT (stored).
    async fn turn_start(&self, _convo: &mut Conversation) {}

    /// Before EACH LLM request (every round). Mutate the OUTGOING messages.
    /// EPHEMERAL: operates on a per-request clone, NOT stored — projections never
    /// poison the prefix cache. `ctx` carries round / max_rounds.
    async fn pre_request(&self, _messages: &mut Vec<Message>, _ctx: &TurnCtx) {}

    /// After the model response: the assistant message (text + tool_calls +
    /// kernel-filled `meta`) is built but not yet stored. Observe or TRANSFORM it —
    /// including dropping/rewriting `tool_calls`, which the kernel HONORS.
    /// PERMANENT (stored). `meta` is kernel-owned — don't fabricate it.
    async fn on_model_response(&self, _response: &mut Message) {}

    /// The model produced no tool calls (wants to stop). Return `Some(text)` to
    /// inject a follow-up USER message and CONTINUE; `None` to complete. Read-only.
    async fn turn_end(&self, _convo: &Conversation) -> Option<String> {
        None
    }

    /// Observe an error: a tool returned `is_error`, or an unknown/unmounted tool
    /// was called. PURE OBSERVATION — cannot alter flow. (Provider/stream errors
    /// are not routed here in this build.)
    async fn on_error(&self, _error: &str) {}

    /// Session ends (any exit path). Read-only conversation for cleanup / telemetry.
    async fn session_end(&self, _convo: &Conversation) {}
}

/// Default no-op hooks — a neutral kernel installs these.
pub struct NoopHooks;

impl LifecycleHooks for NoopHooks {}
