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
/// no-op, so a neutral kernel pays nothing. The kernel calls each of these at the
/// matching point in the loop; specializations override what they need.
#[async_trait]
pub trait LifecycleHooks: Send + Sync {
    /// Session begins. Inject seed context / persona.
    async fn session_start(&self, _convo: &mut Conversation) {}

    /// A user message is about to enter the loop. Mutate to rewrite / augment.
    async fn user_prompt_submit(&self, _text: &mut String) {}

    /// Before the LLM is called for a turn. Mutate the conversation to inject
    /// context / reminders.
    async fn turn_start(&self, _convo: &mut Conversation) {}

    /// Just before the provider request is built — last chance to mutate the
    /// outgoing messages (production `ctx/render` lives here; mind prefix-cache).
    /// `ctx` carries the current round / max so a hook can project budget pressure.
    async fn pre_request(&self, _messages: &mut Vec<Message>, _ctx: &TurnCtx) {}

    /// After the model response: the kernel has built the assistant message
    /// (`text` + `tool_calls` + kernel-filled `meta`) but not yet stored it. The
    /// hook may observe or TRANSFORM the response (e.g. redact/truncate text).
    /// `meta` is kernel-owned measurement — don't fabricate it.
    async fn on_model_response(&self, _response: &mut Message) {}

    /// Before a tool executes. Return `Err(reason)` to block it. (`ToolMiddleware`
    /// is the composable, per-tool form of this same point.)
    async fn pre_tool(&self, _call: &ToolCall) -> Result<(), String> {
        Ok(())
    }

    /// After a tool executes. Mutate the result (truncate / transform / redact).
    async fn post_tool(&self, _result: &mut ToolResult) {}

    /// The model produced no tool calls (wants to stop). Return `Some(text)` to
    /// inject a follow-up user message and CONTINUE the loop; `None` to complete.
    async fn turn_end(&self, _convo: &Conversation) -> Option<String> {
        None
    }

    /// An error occurred (tool error / unknown tool / provider failure). Observe
    /// or decide recovery.
    async fn on_error(&self, _error: &str) {}

    /// Session ends. Cleanup / final telemetry.
    async fn session_end(&self, _convo: &Conversation) {}
}

/// Default no-op hooks — a neutral kernel installs these.
pub struct NoopHooks;

impl LifecycleHooks for NoopHooks {}
