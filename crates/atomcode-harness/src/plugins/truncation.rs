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
//! A response the provider ended normally after reasoning alone is handled
//! here too: nothing was produced the turn can act on, which is the same
//! situation as a reasoning-only cut, and replaying the request only reasons
//! the same way again. It gets the same small budget of nudges.
//!
//! Both are listeners, so a deployment that would rather see the raw truncation
//! removes the row.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_kernel::provider::ReasoningEffort;
use atomcode_kernel::session::RetryAttempt;
use atomcode_kernel::tool::ToolResult;
use atomcode_plexus::{Context, Next, Plugin, Waterfall};
use serde::Deserialize;
use serde_json::Value;

use crate::events::{
    AgentChange, AgentRemoved, AgentRequest, ModelRequest, ModelResponse, RequestError, ToolBatch,
    ToolsExecuteBatch, TurnStart, TurnStarted,
};
use crate::seams::SessionSvc;
use crate::session::{InjectionOrigin, SessionEvent};

const RESUME_NUDGE: &str = "\
Output limit hit — your last response was cut off before finishing. If the task is already \
complete, reply with a short summary and stop (no tool calls). Otherwise resume where you left \
off, writing the remaining content INCREMENTALLY to a file (append the next section with \
`edit_file`) rather than re-emitting it all in one response.";

const REASONING_NUDGE: &str =
    "Your last response exhausted its output budget during reasoning without producing an answer. \
Use the reasoning already done to take the next concrete step or give a concise answer. \
Avoid restarting the analysis or repeating it.";

const ENDED_IN_REASONING_NUDGE: &str = "\
Your last response ended after reasoning, with no answer and no tool call, so nothing was done. \
Do not restart or repeat the analysis. Take the next concrete step now: call a tool or give a \
concise answer. If the work is large, do only its first part in this response (for example, \
write the first section of a file and add the rest with later edits) instead of working \
everything out before acting.";

fn no_visible_progress(response: &ModelResponse) -> bool {
    response.text.trim().is_empty()
        && response
            .tool_calls
            .iter()
            .all(|call| !looks_complete(&call.arguments))
}

fn reasoning_only(response: &ModelResponse) -> bool {
    response.text.trim().is_empty()
        && response.tool_calls.is_empty()
        && (!response.reasoning.trim().is_empty() || !response.reasoning_blocks.is_empty())
}

fn resume_nudge(response: &ModelResponse) -> &'static str {
    if response.ended_in_reasoning {
        ENDED_IN_REASONING_NUDGE
    } else if reasoning_only(response) {
        REASONING_NUDGE
    } else if no_visible_progress(response) {
        "Your last response reached its output limit without a usable answer or a complete tool call. \
Take one concrete next step or give a concise answer based on the work already done. \
Do not restart the analysis or emit the same oversized tool call."
    } else {
        RESUME_NUDGE
    }
}

const TRUNCATED_CALL_COACH: &str = "\
Tool call not executed: the response hit its output-token limit before the arguments finished \
streaming, so the recorded arguments are truncated and unsafe to run. Do NOT retry by \
re-emitting the same large payload — split the work into smaller calls (for write_file / \
edit_file: write the first chunk, then append the rest with successive edit_file calls).";

#[derive(Debug, Deserialize)]
struct Row {
    /// How many consecutive truncations may be continued before the
    /// nudging itself becomes the loop.
    #[serde(default = "default_max")]
    max_continuations: u32,
    /// No visible progress (including hidden reasoning) gets a smaller automatic budget.
    #[serde(default = "default_reasoning_max")]
    max_reasoning_continuations: u32,
    /// Ask the person before giving up on a cut-off answer: going on resumes it
    /// once more, anything else ends the turn on what it has. For a front end
    /// that draws the question; off, giving up says so as a warning.
    #[serde(default)]
    checkpoint: bool,
    /// Reasoning that ran out of output budget gets the next request's budget
    /// doubled once, up to this ceiling (and a quarter of the context window).
    /// The cut reasoning is usually not seen again, so the model thinks afresh;
    /// more room is what lets that second attempt finish.
    #[serde(default = "default_reasoning_output_ceiling")]
    max_reasoning_output_tokens: u32,
}

impl Default for Row {
    fn default() -> Self {
        Self {
            max_continuations: default_max(),
            max_reasoning_continuations: default_reasoning_max(),
            checkpoint: false,
            max_reasoning_output_tokens: default_reasoning_output_ceiling(),
        }
    }
}

/// What the person is told when a turn ends with its answer still cut off.
const LEFT_CUT_OFF: &str =
    "模型这次回复达到了长度上限，内容可能没写完。可以让它「继续」，会接着把剩下的部分补完。";

fn default_reasoning_max() -> u32 {
    1
}

/// A provider refusing the request because of its output-token field — the
/// raised budget exceeds this endpoint's own limit.
fn rejects_output_budget(error: &RequestError) -> bool {
    let message = error.message.to_ascii_lowercase();
    matches!(error.http_status, Some(400 | 422))
        && [
            "max_tokens",
            "max_completion_tokens",
            "max_output_tokens",
            "max_new_tokens",
        ]
        .iter()
        .any(|field| message.contains(field))
}

fn default_reasoning_output_ceiling() -> u32 {
    65_536
}

/// The budget for the request after reasoning ran out of `current` (`None` = the
/// adapter's default): double, never above `ceiling` or a quarter of `window`.
/// `None` when that is no raise at all.
fn raised_output_budget(current: Option<u32>, ceiling: u32, window: u32) -> Option<u32> {
    let current = current.unwrap_or(atomcode_kernel::agent::DEFAULT_MAX_OUTPUT_TOKENS);
    let ceiling = if window > 0 {
        ceiling.min(window / 4)
    } else {
        ceiling
    };
    let next = current.saturating_mul(2).min(ceiling);
    (next > current).then_some(next)
}

pub(crate) fn lower_effort(
    current: Option<ReasoningEffort>,
    supported: &[String],
) -> Option<ReasoningEffort> {
    let levels = [
        ReasoningEffort::Low,
        ReasoningEffort::Medium,
        ReasoningEffort::High,
        ReasoningEffort::XHigh,
        ReasoningEffort::Max,
    ];
    let index = levels.iter().position(|level| Some(*level) == current)?;
    levels[..index]
        .iter()
        .rev()
        .copied()
        .find(|level| supported.iter().any(|s| s == level.as_str()))
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

/// Ephemeral recovery state belongs to one agent's session, never to the tree.
#[derive(Default)]
struct RecoveryState {
    seen: u32,
    no_progress_seen: u32,
    next_effort: Option<ReasoningEffort>,
    next_max_tokens: Option<u32>,
    last: Option<String>,
}

/// Notices a truncated response and queues the resume nudge as a logged fact.
struct OnTruncation {
    ctx: Context,
    max_continuations: u32,
    max_reasoning_continuations: u32,
    checkpoint: bool,
    max_reasoning_output_tokens: u32,
    states: Mutex<HashMap<String, Arc<Mutex<RecoveryState>>>>,
}

impl OnTruncation {
    fn state(&self, session: &str) -> Arc<Mutex<RecoveryState>> {
        self.states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(session.to_owned())
            .or_default()
            .clone()
    }

    /// New turns, completed responses and removal clear only this session.
    fn reset(&self, session: &str) {
        self.states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session);
    }

    /// Resuming has stopped working. Decide what the response becomes: `true`
    /// keeps it cut off, so the loop asks once more; `false` lets the turn end
    /// on what it has.
    ///
    /// With the checkpoint, the person decides, and whatever they decide they
    /// already saw — no warning follows. Without it, the turn ends and says so.
    async fn give_up(
        &self,
        attempts: u32,
        limit: u32,
        response: &ModelResponse,
        req: &ModelRequest,
        state: &Arc<Mutex<RecoveryState>>,
    ) -> bool {
        let scoped = crate::agent::scoped(&self.ctx);
        let Some(session) = scoped.service::<SessionSvc>() else {
            return false;
        };
        // A normal reasoning-only stop did not hit an output limit. Once its
        // small recovery budget is spent, end it rather than asking the person
        // an output-truncation question with a false premise.
        if self.checkpoint && !response.ended_in_reasoning {
            let lower = if no_visible_progress(response) {
                scoped
                    .service::<crate::seams::LlmSvc>()
                    .and_then(|provider| {
                        lower_effort(req.options.reasoning_effort, &provider.effort_levels())
                    })
            } else {
                None
            };
            let requester = scoped
                .service::<crate::seams::ToolDriverSvc>()
                .and_then(|driver| driver.requester(session.id()));
            let answer = match requester {
                Some(requester) => {
                    requester
                        .request(
                            atomcode_kernel::event::OUTPUT_TRUNCATION_CHECKPOINT_KIND,
                            serde_json::json!({
                                "attempts": attempts,
                                "max_attempts": limit,
                                "lower_effort": lower.map(ReasoningEffort::as_str),
                                "reasoning_only": reasoning_only(response),
                                "no_visible_progress": no_visible_progress(response),
                            }),
                        )
                        .await
                }
                None => Value::Null,
            };
            let valid_lower_choice = answer.get("lower_effort").map_or(true, Value::is_boolean);
            let wants_lower = answer.get("lower_effort").and_then(Value::as_bool) == Some(true);
            let go_on = answer.get("continue").and_then(Value::as_bool) == Some(true)
                && valid_lower_choice
                && (!wants_lower || lower.is_some());
            if go_on && wants_lower {
                state.lock().unwrap_or_else(|e| e.into_inner()).next_effort = lower;
            }
            if go_on {
                crate::session::commit(
                    &self.ctx,
                    &session,
                    SessionEvent::Injected {
                        turn: session.current_turn(),
                        text: resume_nudge(response).to_string(),
                        origin: InjectionOrigin::Continuation,
                    },
                );
            }
            return go_on;
        }
        crate::session::commit(
            &self.ctx,
            &session,
            SessionEvent::Notice {
                turn: session.current_turn(),
                notice: crate::session::NoticeKind::OutputLeftCutOff,
                detail: if response.ended_in_reasoning {
                    "模型这次只输出了思考，没有给出正文或工具调用，本轮已结束。".to_string()
                } else if reasoning_only(response) {
                    "模型思考已达到输出上限，尚未生成正文。可以调整输出预算或思考强度后重试。"
                        .to_string()
                } else if no_visible_progress(response) {
                    "模型输出已达到长度上限，尚未生成正文或完整工具调用。可以调整输出预算后重试。"
                        .to_string()
                } else {
                    LEFT_CUT_OFF.to_string()
                },
                retry: None,
            },
        );
        false
    }
}

#[async_trait]
impl Waterfall<AgentRequest> for OnTruncation {
    async fn handle(
        &self,
        req: &mut ModelRequest,
        next: Next<'_, AgentRequest>,
    ) -> Result<ModelResponse, RequestError> {
        let session = crate::agent::scoped(&self.ctx)
            .service::<SessionSvc>()
            .ok_or_else(|| RequestError::message("truncation recovery requires a session"))?;
        let state = self.state(session.id());
        if let Some(effort) = state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .next_effort
            .take()
        {
            req.options.reasoning_effort = Some(effort);
        }
        let raised_from = state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .next_max_tokens
            .take()
            .map(|budget| std::mem::replace(&mut req.options.max_tokens, Some(budget)));
        let mut response = match (next.run(req).await, raised_from) {
            // The raise went past what this endpoint accepts: ask once more as before.
            (Err(error), Some(original)) if rejects_output_budget(&error) => {
                req.options.max_tokens = original;
                next.run(req).await?
            }
            (result, _) => result?,
        };
        // A reasoning-only reply to the harness's own note is the loop's to
        // settle as "nothing to add"; nudging it again would answer a nudge
        // with a nudge.
        let ended_in_reasoning = response.ended_in_reasoning && !req.answering_a_nudge;
        if !response.truncated && !ended_in_reasoning {
            self.reset(session.id());
            return Ok(response);
        }

        // Compare only responses from this agent's consecutive streak.
        let redump = {
            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
            let redump = state
                .last
                .as_deref()
                .is_some_and(|prev| truncation_is_redump(prev, &response.text));
            state.last = Some(response.text.clone());
            redump
        };
        if redump {
            let attempts = state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .seen
                .min(self.max_continuations);
            response.truncated = self
                .give_up(attempts, self.max_continuations, &response, req, &state)
                .await;
            return Ok(response);
        }

        let (seen, attempt, limit) = {
            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
            state.seen = state.seen.saturating_add(1);
            let (attempt, limit) = if no_visible_progress(&response) {
                state.no_progress_seen = state.no_progress_seen.saturating_add(1);
                (
                    state.no_progress_seen,
                    self.max_reasoning_continuations.min(self.max_continuations),
                )
            } else {
                state.no_progress_seen = 0;
                (state.seen, self.max_continuations)
            };
            (state.seen, attempt, limit)
        };
        if seen > self.max_continuations || attempt > limit {
            response.truncated = self
                .give_up(
                    attempt.saturating_sub(1).min(limit),
                    limit,
                    &response,
                    req,
                    &state,
                )
                .await;
            if !response.truncated && no_visible_progress(&response) {
                // These calls have no complete arguments and cannot run. Keeping
                // them would owe another tool round even though recovery stopped.
                response.tool_calls.clear();
            }
            return Ok(response);
        }

        // A tool call that was cut mid-arguments cannot be trusted, and the
        // model needs to know *that* rather than "your call failed".
        let cut_a_call = response
            .tool_calls
            .last()
            .map(|call| !looks_complete(&call.arguments))
            .unwrap_or(false);

        let raised = if reasoning_only(&response) && !response.ended_in_reasoning {
            let window = crate::agent::scoped(&self.ctx)
                .service::<crate::seams::LlmSvc>()
                .map_or(0, |provider| provider.context_window());
            raised_output_budget(
                req.options.max_tokens,
                self.max_reasoning_output_tokens,
                window,
            )
        } else {
            None
        };
        if let Some(budget) = raised {
            state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .next_max_tokens = Some(budget);
        }

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
                        "{}（连续恢复第 {attempt}/{limit} 次）",
                        if let Some(budget) = raised {
                            format!("模型思考达到输出上限，正在把输出预算提高到 {budget} 后继续")
                        } else if response.ended_in_reasoning {
                            "模型思考后没有给出正文，正在请它直接推进任务".to_string()
                        } else if reasoning_only(&response) {
                            "模型思考达到输出上限，正在请求直接推进任务".to_string()
                        } else if no_visible_progress(&response) {
                            "输出达到上限，尚无正文或完整工具调用，正在请求直接推进任务".to_string()
                        } else {
                            "输出被截断，正在请模型接着写".to_string()
                        },
                    ),
                    retry: Some(RetryAttempt {
                        attempt,
                        max_attempts: limit,
                        backoff_secs: 0,
                    }),
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
                        resume_nudge(&response).to_string()
                    },
                    origin: InjectionOrigin::Continuation,
                },
            );
        }
        // Kept cut off so the loop asks once more.
        response.truncated = true;
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
    fn uses(&self) -> &'static [&'static str] {
        // Only with `checkpoint`: whoever drives the agent is asked.
        &["tool-driver", "llm"]
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
            max_reasoning_continuations: row.max_reasoning_continuations,
            checkpoint: row.checkpoint,
            max_reasoning_output_tokens: row.max_reasoning_output_tokens,
            states: Mutex::new(HashMap::new()),
        });
        let per_turn = on_truncation.clone();
        let _ = ctx.on_emit::<TurnStart>(move |_: &TurnStarted| {
            if let Some(session) = crate::agent::scoped(&per_turn.ctx).service::<SessionSvc>() {
                per_turn.reset(session.id());
            }
        });
        let removed = on_truncation.clone();
        let _ =
            ctx.on_emit::<AgentRemoved>(move |agent: &AgentChange| removed.reset(&agent.session));
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

#[cfg(test)]
mod budget_tests {
    use super::{raised_output_budget, rejects_output_budget};
    use crate::events::RequestError;
    use atomcode_kernel::agent::DEFAULT_MAX_OUTPUT_TOKENS;

    #[test]
    fn reasoning_out_of_budget_gets_double_within_the_ceilings() {
        assert_eq!(
            raised_output_budget(Some(16_000), 65_536, 512_000),
            Some(32_000)
        );
        assert_eq!(
            raised_output_budget(None, 65_536, 0),
            Some(DEFAULT_MAX_OUTPUT_TOKENS * 2)
        );
        // Never past the configured ceiling, nor a quarter of the window.
        assert_eq!(
            raised_output_budget(Some(48_000), 65_536, 1_000_000),
            Some(65_536)
        );
        assert_eq!(raised_output_budget(Some(16_000), 65_536, 64_000), None);
        // Already at the ceiling (e.g. a declared 128K): no raise.
        assert_eq!(raised_output_budget(Some(128_000), 65_536, 1_000_000), None);
    }

    #[test]
    fn only_a_refused_output_field_counts_as_a_rejected_budget() {
        let error = |status, message: &str| RequestError {
            http_status: status,
            ..RequestError::message(message)
        };
        assert!(rejects_output_budget(&error(
            Some(400),
            "max_tokens must be less than or equal to 16000"
        )));
        assert!(rejects_output_budget(&error(
            Some(422),
            "Invalid max_completion_tokens"
        )));
        assert!(!rejects_output_budget(&error(
            Some(400),
            "invalid tool schema"
        )));
        assert!(!rejects_output_budget(&error(
            Some(500),
            "max_tokens internal"
        )));
        assert!(!rejects_output_budget(&error(None, "max_tokens")));
    }
}
