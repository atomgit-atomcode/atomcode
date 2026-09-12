//! What to do when the model is cut off at its output-token limit.
//!
//! Two distinct failures share one cause, and they need opposite handling:
//!
//! - **A truncated tool call.** The arguments stopped mid-stream, so the
//!   recorded JSON is partial. Running it is unsafe — a `write_file` whose
//!   `content` was cut would silently truncate a real file. The call is refused
//!   and the model is told to split the work, not to re-emit the same payload.
//! - **Truncated text.** Nothing unsafe happened; the answer is simply
//!   unfinished. The turn continues with a nudge to resume incrementally rather
//!   than restart — unless the model answered the last nudge by re-dumping the
//!   same content from the top, in which case nudging again only spends the
//!   budget, and the response is treated as final instead.
//!
//! Both are listeners, so a deployment that would rather see the raw truncation
//! removes the row.

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_kernel::tool::ToolResult;
use atomcode_plexus::{Context, Next, Plugin, Waterfall};
use serde::Deserialize;
use serde_json::Value;

use crate::events::{
    AgentRequest, ModelRequest, ModelResponse, RequestError, ToolBatch, ToolsExecuteBatch,
    TurnStart, TurnStarted,
};
use crate::seams::SessionSvc;
use crate::session::{InjectionOrigin, SessionEvent};

const RESUME_NUDGE: &str = "\
Output limit hit — your last response was cut off before finishing. If the task is already \
complete, reply with a short summary and stop (no tool calls). Otherwise resume where you left \
off, writing the remaining content INCREMENTALLY to a file (append the next section with \
`edit_file`) rather than re-emitting it all in one response.";

const TRUNCATED_CALL_COACH: &str = "\
Tool call not executed: the response hit its output-token limit before the arguments finished \
streaming, so the recorded arguments are truncated and unsafe to run. Do NOT retry by \
re-emitting the same large payload — split the work into smaller calls (for write_file / \
edit_file: write the first chunk, then append the rest with successive edit_file calls).";

#[derive(Debug, Deserialize)]
struct Row {
    /// How many times a turn may be continued after a truncation before the
    /// nudging itself becomes the loop.
    #[serde(default = "default_max")]
    max_continuations: u32,
}

impl Default for Row {
    fn default() -> Self {
        Self {
            max_continuations: default_max(),
        }
    }
}

fn default_max() -> u32 {
    4
}

/// Two consecutive truncated responses that share a long identical leading
/// prefix are the model re-dumping the same content — restarting from the top
/// instead of resuming as the nudge asked. A weak model that ignores the nudge
/// would otherwise burn every continuation re-sending the same text.
///
/// Compared by chars, so a multi-byte prefix is never split. Trivially short
/// rounds never match: two short answers that happen to agree are not evidence
/// of anything.
pub(crate) fn truncation_is_redump(prev: &str, curr: &str) -> bool {
    const MIN_LEN: usize = 64;
    const PREFIX_CHARS: usize = 400;
    let p = prev.trim_start();
    let c = curr.trim_start();
    if p.chars().take(MIN_LEN).count() < MIN_LEN || c.chars().take(MIN_LEN).count() < MIN_LEN {
        return false;
    }
    p.chars()
        .take(PREFIX_CHARS)
        .eq(c.chars().take(PREFIX_CHARS))
}

/// Notices a truncated response and queues the resume nudge as a logged fact.
struct OnTruncation {
    ctx: Context,
    max_continuations: u32,
    /// Continuations spent in the current turn.
    seen: std::sync::atomic::AtomicU32,
    /// Text of the last truncated round this turn, to catch a re-dump.
    last: std::sync::Mutex<Option<String>>,
}

impl OnTruncation {
    /// A new turn starts with a fresh budget and no memory of the last one:
    /// the cap is "per turn", and a re-dump is only meaningful against the
    /// round the nudge answered.
    fn reset(&self) {
        self.seen.store(0, std::sync::atomic::Ordering::SeqCst);
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

#[async_trait]
impl Waterfall<AgentRequest> for OnTruncation {
    async fn handle(
        &self,
        req: &mut ModelRequest,
        next: Next<'_, AgentRequest>,
    ) -> Result<ModelResponse, RequestError> {
        let mut response = next.run(req).await?;
        if !response.truncated {
            return Ok(response);
        }

        // The model answered the last nudge by starting over. Asking again
        // would get the same again; hand the turn on with what it has — the
        // same way a truncation past the cap is handled below.
        let redump = {
            let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
            let redump = last
                .as_deref()
                .is_some_and(|prev| truncation_is_redump(prev, &response.text));
            *last = Some(response.text.clone());
            redump
        };
        if redump {
            response.truncated = false;
            return Ok(response);
        }

        let seen = self.seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        if seen > self.max_continuations {
            // The nudge is not working. Let the turn end on what it has rather
            // than spend the rest of the budget asking again.
            //
            // Clearing the flag is how that decision reaches the loop: the loop
            // keeps asking while a response says it was cut off, so "stop
            // asking" and "this response is final" are the same statement. The
            // cap therefore lives here, once, rather than being re-derived by
            // whoever reads `truncated` next.
            response.truncated = false;
            return Ok(response);
        }

        // A tool call that was cut mid-arguments cannot be trusted, and the
        // model needs to know *that* rather than "your call failed".
        let cut_a_call = response
            .tool_calls
            .last()
            .map(|call| !looks_complete(&call.arguments))
            .unwrap_or(false);

        if let Some(session) = crate::agent::scoped(&self.ctx).service::<SessionSvc>() {
            // Tell the driver as well as the model. The nudge is model-visible;
            // that the answer was cut off and is being resumed is something a
            // person watching the screen should also see.
            crate::session::commit(
                &self.ctx,
                &session,
                SessionEvent::Notice {
                    turn: session.current_turn(),
                    notice: crate::session::NoticeKind::OutputTruncated,
                    detail: format!(
                        "输出被截断，正在请模型接着写（第 {seen}/{} 次）",
                        self.max_continuations
                    ),
                },
            );
            crate::session::commit(
                &self.ctx,
                &session,
                SessionEvent::Injected {
                    turn: session.current_turn(),
                    text: if cut_a_call {
                        TRUNCATED_CALL_COACH.to_string()
                    } else {
                        RESUME_NUDGE.to_string()
                    },
                    origin: InjectionOrigin::Continuation,
                },
            );
        }
        Ok(response)
    }
}

/// Whether a JSON argument string finished streaming.
///
/// Deliberately shallow: a full parse would reject arguments a tool would have
/// accepted, and the question here is only "was this cut off", not "is it
/// valid". Balanced delimiters outside a string is the signal.
fn looks_complete(arguments: &str) -> bool {
    let text = arguments.trim();
    if text.is_empty() {
        return true;
    }
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for ch in text.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '{' | '[' if !in_string => depth += 1,
            '}' | ']' if !in_string => depth -= 1,
            _ => {}
        }
    }
    depth == 0 && !in_string
}

/// Refuses a call whose arguments were cut off, before it can act on them.
struct RefuseTruncatedCalls;

#[async_trait]
impl Waterfall<ToolsExecuteBatch> for RefuseTruncatedCalls {
    async fn handle(
        &self,
        batch: &mut ToolBatch,
        next: Next<'_, ToolsExecuteBatch>,
    ) -> Vec<ToolResult> {
        let cut: Vec<usize> = batch
            .calls
            .iter()
            .enumerate()
            .filter(|(_, call)| !looks_complete(&call.arguments))
            .map(|(i, _)| i)
            .collect();
        if cut.is_empty() {
            return next.run(batch).await;
        }

        // Hold back the truncated ones and let the rest through: a round that
        // also contained good calls should not lose them.
        let refused: Vec<(usize, ToolResult)> = cut
            .iter()
            .map(|&i| {
                (
                    i,
                    ToolResult {
                        call_id: batch.calls[i].id.clone(),
                        content: TRUNCATED_CALL_COACH.to_string(),
                        is_error: true,
                        images: vec![],
                    },
                )
            })
            .collect();
        let survivors: Vec<_> = batch
            .calls
            .iter()
            .enumerate()
            .filter(|(i, _)| !cut.contains(i))
            .map(|(_, call)| call.clone())
            .collect();

        batch.calls = survivors;
        let mut results = if batch.calls.is_empty() {
            Vec::new()
        } else {
            next.run(batch).await
        };
        // Put the refusals back where the model expects them, so the results
        // still line up with the calls it emitted.
        for (index, result) in refused {
            let at = index.min(results.len());
            results.insert(at, result);
        }
        results
    }
}

pub struct TruncationPlugin;

#[async_trait]
impl Plugin for TruncationPlugin {
    fn name(&self) -> &'static str {
        "truncation-recovery"
    }
    fn description(&self) -> &'static str {
        "resume after an output-limit cut (unless the model just re-dumped the same text), and refuse a call whose arguments were cut"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: Row = if config.is_null() {
            Row::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        let on_truncation = Arc::new(OnTruncation {
            ctx: ctx.clone(),
            max_continuations: row.max_continuations,
            seen: std::sync::atomic::AtomicU32::new(0),
            last: std::sync::Mutex::new(None),
        });
        let per_turn = on_truncation.clone();
        let _ = ctx.on_emit::<TurnStart>(move |_: &TurnStarted| per_turn.reset());
        let _ = ctx.on_waterfall::<AgentRequest>(on_truncation, false);
        // Prepended: a truncated call must be caught before any scheduler picks
        // it up, or it runs with partial arguments.
        let _ = ctx.on_waterfall::<ToolsExecuteBatch>(Arc::new(RefuseTruncatedCalls), true);
        Ok(())
    }
}

#[cfg(test)]
mod redump_tests {
    use super::{looks_complete, truncation_is_redump};

    #[test]
    fn an_identical_long_prefix_is_a_redump() {
        // Real truncated responses are large; both share an identical 400-char lead.
        let a = "第 1 节:游戏概述。".repeat(80);
        let b = format!("{a} 但这次又多说了一点点。");
        assert!(truncation_is_redump(&a, &b), "same long prefix = re-dump");
        assert!(truncation_is_redump(&a, &a));
    }

    #[test]
    fn a_genuine_continuation_is_not_a_redump() {
        let prev = "第 1 节:玩家可选性别,只画脸,滚动条调肤色……".repeat(20);
        let curr = "第 2 节:多点触控时其余脸随机肤色,来回判定加分……".repeat(20);
        assert!(
            !truncation_is_redump(&prev, &curr),
            "different content = resume"
        );
    }

    #[test]
    fn trivially_short_rounds_never_match() {
        assert!(!truncation_is_redump("ok", "ok"));
        assert!(!truncation_is_redump("", ""));
    }

    #[test]
    fn a_cut_argument_string_looks_incomplete() {
        assert!(looks_complete(
            r#"{"path": "a.rs", "content": "fn main() {}"}"#
        ));
        assert!(!looks_complete(
            r#"{"path": "a.rs", "content": "fn main() {"#
        ));
    }
}
