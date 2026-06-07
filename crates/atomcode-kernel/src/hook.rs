use crate::message::{Conversation, Message};
use crate::provider::ChatOptions;
use crate::tool::ToolDef;
use async_trait::async_trait;
use std::sync::Arc;

/// Per-LLM-call execution context handed to hooks that need to know where in the
/// loop they are (e.g. to project round budget to the LLM).
#[derive(Clone, Copy, Debug, Default)]
pub struct TurnCtx {
    /// 1-based index of the LLM call about to execute this turn.
    pub round: u32,
    /// Optional hard cap on rounds per turn (None = unlimited).
    pub max_rounds: Option<u32>,
    /// The conversation's current PREFIX-GENERATION marker (`Conversation::cache_epoch`).
    /// Carried so a `pre_request` projection can be epoch-deterministic later (e.g.
    /// only re-render an expensive view when the epoch changed). A committed
    /// compaction bumps it; an append-only round does not.
    pub cache_epoch: u64,
}

/// TURN-level lifecycle seam (session / turn / request / response / error). The
/// "inject into the loop" side, distinct from the read-only AgentEvent stream.
/// TOOL-level concerns (gate/rewrite/transform a tool call) live in
/// `ToolMiddleware`, not here. Every method defaults to no-op.
///
/// Each method states whether a mutation is PERMANENT (written into stored
/// conversation history) or EPHEMERAL (affects only the current request).
///
/// # PANIC CONTRACT (must-not-panic)
///
/// An implementation **MUST NOT panic**. The kernel does **NOT** isolate panics:
/// under the workspace `panic = "abort"` profile a panic ABORTS THE HOST PROCESS
/// (and `catch_unwind` is a no-op there), and under an unwind profile a panicking
/// hook is not currently caught either — so a panicking hook takes down the whole
/// session / process. Treat all injected code as must-not-panic — the SAME trust
/// posture as the tool-sandbox contract (see [`crate::tool`]): the kernel hosts
/// your code with full ambient authority and does not confine its failures.
#[async_trait]
pub trait LifecycleHooks: Send + Sync {
    /// Session begins, before any turn. Mutate the conversation to inject seed
    /// context / persona. PERMANENT (stored).
    ///
    /// `resumed` is `true` iff this session was SEEDED FROM A SNAPSHOT (a `.resume`
    /// whose version the kernel supports actually re-hydrated history) — `false` for
    /// a fresh session. A SEEDING hook (one that injects context the snapshot would
    /// already carry) MUST early-return `if resumed` to avoid DOUBLE-SEEDING on top
    /// of the restored snapshot: `if resumed { return; }`.
    async fn session_start(&self, _convo: &mut Conversation, _resumed: bool) {}

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

    /// READ-ONLY wire observation, fired AFTER `pre_request` projects and just
    /// BEFORE the provider call — so the observer sees the EXACT FINAL outgoing
    /// request: post-projection `messages`, the frozen `tools` block, the sideband
    /// `options`, and round/epoch via `ctx`. This is the kernel HOME for the
    /// project's telemetry / datalog / prefix-cache-RCA discipline (e.g. hash the
    /// prefix, dump the bytes). It is `&` (read-only) ON PURPOSE: it MUST NOT mutate
    /// the outgoing wire — mutation is `pre_request`'s job (the ephemeral clone),
    /// which keeps the prefix-cache contract owned by exactly one seam.
    async fn on_request(
        &self,
        _messages: &[Message],
        _tools: &[ToolDef],
        _options: &ChatOptions,
        _ctx: &TurnCtx,
    ) {
    }

    /// Transform EACH streamed text chunk IN PLACE just before it is emitted to the
    /// driver AND accumulated into the stored assistant message. EPHEMERAL per call
    /// but the post-hook bytes flow to BOTH the live stream and storage, so a
    /// redaction here is CONSISTENT (unlike `on_model_response`, which transforms
    /// only the already-assembled STORED message AFTER the un-redacted bytes have
    /// already streamed to the driver/UI).
    ///
    /// DELIVERY GUARANTEE: a hook sees ONE chunk at a time. Cross-chunk redaction (a
    /// secret SPLIT across two deltas) is the HOOK's responsibility — it must buffer
    /// internally, or clear a chunk (`delta.clear()`) to suppress it — the kernel
    /// guarantees only per-chunk delivery before emit, nothing about chunk boundaries.
    async fn on_text_delta(&self, _delta: &mut String) {}

    /// After the model response: the assistant message (text + tool_calls +
    /// kernel-filled `meta`) is built but not yet stored. Observe or TRANSFORM it —
    /// including dropping/rewriting `tool_calls`, which the kernel HONORS.
    /// PERMANENT (stored). `meta` is kernel-owned — don't fabricate it.
    ///
    /// This transforms STORAGE ONLY (it runs POST-stream, after every delta has
    /// already been emitted live). To transform the STREAMED output (so the
    /// redaction reaches the driver/UI too, not just history), use `on_text_delta`.
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

/// Composes MANY `LifecycleHooks` into one by FANNING OUT each method over an
/// ordered list. This is the seam that lets independent capabilities coexist —
/// codeintel + compaction + redaction can each register a hook instead of fighting
/// over a single slot. A `HookChain` itself implements `LifecycleHooks`, so the
/// Agent still holds exactly one `Arc<dyn LifecycleHooks>` and every call site in
/// the run loop stays unchanged.
///
/// COMPOSITION CONTRACT (per method) — the load-bearing part of this type:
/// - `session_start`, `turn_start`: run ALL hooks in REGISTRATION ORDER; each
///   mutates the conversation in turn (chained — later hooks see earlier hooks'
///   edits). `session_start` FORWARDS the `resumed` flag UNCHANGED to every hook.
/// - `pre_request`: run ALL in registration order; each mutates the (ephemeral
///   clone of) outgoing messages in turn. The kernel passes the per-request clone,
///   NEVER stored history → prefix-cache discipline is preserved.
/// - `on_request`: run ALL in registration order (pure read-only observation of the
///   FINAL outgoing wire). Cannot mutate (it gets `&`) → cache discipline intact.
/// - `on_text_delta`: run ALL in registration order; each TRANSFORMS the streamed
///   chunk in turn (a later hook sees the earlier hook's rewrite), then the
///   post-chain bytes are emitted AND accumulated.
/// - `on_model_response`: run ALL in registration order; each TRANSFORMS the
///   response message in turn (e.g. redaction then truncation compose; a later
///   hook sees the earlier hook's rewrite).
/// - `user_prompt_submit`: run in registration order; SHORT-CIRCUIT on the FIRST
///   `Err(reason)` (a block) — the remaining hooks are NOT run and the `Err`
///   propagates. Text mutations from earlier hooks chain into later ones until/
///   unless one blocks.
/// - `turn_end`: run ALL hooks in registration order so each OBSERVES the turn end;
///   the FIRST hook (in order) that returns `Some(text)` provides the continuation.
///   Later `Some(_)` values are IGNORED this round (the loop injects exactly one
///   follow-up). Returns that first `Some`, else `None`.
/// - `on_error`, `session_end`: run ALL in registration order (pure observation).
///
/// An EMPTY `HookChain` behaves exactly like `NoopHooks`: every method is a no-op,
/// `user_prompt_submit` → `Ok(())`, `turn_end` → `None`.
pub struct HookChain {
    hooks: Vec<Arc<dyn LifecycleHooks>>,
}

impl HookChain {
    pub fn new(hooks: Vec<Arc<dyn LifecycleHooks>>) -> Self {
        Self { hooks }
    }
}

#[async_trait]
impl LifecycleHooks for HookChain {
    async fn session_start(&self, convo: &mut Conversation, resumed: bool) {
        for h in &self.hooks {
            h.session_start(convo, resumed).await;
        }
    }

    async fn user_prompt_submit(&self, text: &mut String) -> Result<(), String> {
        // Registration order; SHORT-CIRCUIT on the first block. Earlier hooks'
        // text rewrites have already landed in `text` when a later hook runs.
        for h in &self.hooks {
            h.user_prompt_submit(text).await?;
        }
        Ok(())
    }

    async fn turn_start(&self, convo: &mut Conversation) {
        for h in &self.hooks {
            h.turn_start(convo).await;
        }
    }

    async fn pre_request(&self, messages: &mut Vec<Message>, ctx: &TurnCtx) {
        for h in &self.hooks {
            h.pre_request(messages, ctx).await;
        }
    }

    async fn on_request(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        options: &ChatOptions,
        ctx: &TurnCtx,
    ) {
        for h in &self.hooks {
            h.on_request(messages, tools, options, ctx).await;
        }
    }

    async fn on_text_delta(&self, delta: &mut String) {
        for h in &self.hooks {
            h.on_text_delta(delta).await;
        }
    }

    async fn on_model_response(&self, response: &mut Message) {
        for h in &self.hooks {
            h.on_model_response(response).await;
        }
    }

    async fn turn_end(&self, convo: &Conversation) -> Option<String> {
        // ALL hooks observe (in order); the FIRST `Some` wins, later `Some` ignored.
        let mut continuation = None;
        for h in &self.hooks {
            let r = h.turn_end(convo).await;
            if continuation.is_none() {
                continuation = r;
            }
        }
        continuation
    }

    async fn on_error(&self, error: &str) {
        for h in &self.hooks {
            h.on_error(error).await;
        }
    }

    async fn session_end(&self, convo: &Conversation) {
        for h in &self.hooks {
            h.session_end(convo).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_hookchain_is_noop() {
        let chain = HookChain::new(vec![]);
        let mut text = "hi".to_string();
        assert!(chain.user_prompt_submit(&mut text).await.is_ok(), "empty chain must not block");
        assert_eq!(text, "hi", "empty chain must not mutate the prompt");
        let convo = Conversation::new();
        assert_eq!(chain.turn_end(&convo).await, None, "empty chain turn_end must be None");
    }
}
