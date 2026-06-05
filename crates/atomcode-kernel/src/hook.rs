use crate::message::Conversation;
use async_trait::async_trait;

/// Turn-level injection seam — the "inject into the loop" side, distinct from the
/// read-only AgentEvent stream (the "perceive" side). All methods default to
/// no-op, so a neutral kernel pays nothing. The full design has more points
/// (session_start/end, on_error, user_prompt_submit, pre_request); the spike
/// validates turn_start + turn_end.
#[async_trait]
pub trait LifecycleHooks: Send + Sync {
    /// Before the LLM is called for a turn. May mutate the conversation to inject
    /// context / reminders.
    async fn turn_start(&self, _convo: &mut Conversation) {}

    /// When the model produces no tool calls (wants to stop). Return Some(text) to
    /// inject a follow-up user message and CONTINUE the loop (e.g. discipline
    /// "you're not done"); return None to let the turn complete.
    async fn turn_end(&self, _convo: &Conversation) -> Option<String> {
        None
    }
}

/// Default no-op hooks — a neutral kernel installs these.
pub struct NoopHooks;

impl LifecycleHooks for NoopHooks {}
