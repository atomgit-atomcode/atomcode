use crate::message::{Conversation, Message};
use crate::tool::{ToolCall, ToolResult};
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

/// Turn-level injection seam — the "inject into the loop" side, distinct from the
/// read-only AgentEvent stream (the "perceive" side). Every method defaults to
/// no-op, so a neutral kernel pays nothing. The kernel calls each at the matching
/// point in the loop; specializations override what they need.
///
/// Each method states whether a mutation is PERMANENT (written into stored
/// conversation history) or EPHEMERAL (affects only the current request, not
/// stored) — important for prefix-cache discipline.
#[async_trait]
pub trait LifecycleHooks: Send + Sync {
    /// Session begins, before any turn. Mutate the conversation to inject seed
    /// context / persona. PERMANENT (stored).
    async fn session_start(&self, _convo: &mut Conversation) {}

    /// A user message is about to enter the loop. Rewrite / augment the text.
    /// PERMANENT (the rewritten text is stored). Minimal form: cannot block or see
    /// history.
    async fn user_prompt_submit(&self, _text: &mut String) {}

    /// Before a turn's first LLM call — fires ONCE per user message, not per round.
    /// Mutate the conversation. PERMANENT (stored).
    async fn turn_start(&self, _convo: &mut Conversation) {}

    /// Before EACH LLM request (every round). Mutate the OUTGOING messages.
    /// EPHEMERAL: operates on a per-request clone, NOT stored — so projections
    /// (e.g. budget reminders) never poison the prefix cache. `ctx` carries
    /// round / max_rounds.
    async fn pre_request(&self, _messages: &mut Vec<Message>, _ctx: &TurnCtx) {}

    /// After the model response: the assistant message (text + tool_calls +
    /// kernel-filled `meta`) is built but not yet stored. Observe or TRANSFORM it —
    /// including dropping/rewriting `tool_calls`, which the kernel HONORS (a dropped
    /// call will NOT execute). PERMANENT (the transformed message is stored).
    /// `meta` is kernel-owned measurement — don't fabricate it.
    async fn on_model_response(&self, _response: &mut Message) {}

    /// Before a tool executes — runs BEFORE tool lookup and BEFORE the `ToolStarted`
    /// event, so blocked/unknown tools emit no ghost "started" row. May rewrite the
    /// call (args/name — e.g. sanitize a path, fix a typo'd tool name) or block it
    /// via `Err(reason)`. The rewrite affects EXECUTION (and the tool result); the
    /// stored assistant message keeps whatever `on_model_response` left.
    async fn pre_tool(&self, _call: &mut ToolCall) -> Result<(), String> {
        Ok(())
    }

    /// After a tool executes (or is blocked). Mutate the result (truncate /
    /// transform / redact). PERMANENT (stored). Setting/clearing `is_error`
    /// controls whether `on_error` fires.
    async fn post_tool(&self, _result: &mut ToolResult) {}

    /// The model produced no tool calls (wants to stop). Return `Some(text)` to
    /// inject a follow-up USER message and CONTINUE the loop; `None` to complete.
    /// Read-only conversation.
    async fn turn_end(&self, _convo: &Conversation) -> Option<String> {
        None
    }

    /// Observe an error: a tool returned `is_error`, or an unknown/unmounted tool
    /// was called. PURE OBSERVATION — it cannot alter flow or recover. (Provider /
    /// stream errors are not routed here in this build.)
    async fn on_error(&self, _error: &str) {}

    /// Session ends (any exit path). Read-only conversation for cleanup / final
    /// telemetry.
    async fn session_end(&self, _convo: &Conversation) {}
}

/// Default no-op hooks — a neutral kernel installs these.
pub struct NoopHooks;

impl LifecycleHooks for NoopHooks {}
