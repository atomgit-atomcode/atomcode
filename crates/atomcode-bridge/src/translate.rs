//! Kernel `AgentEvent` → legacy `core::AgentEvent` translation for the STREAM events
//! (text / reasoning / tool stream + result / batches / usage / warning / error).
//!
//! This is the pure, testable half of the bridge's `on_kernel_event`. The LIFECYCLE
//! events that need a command round-trip or async work — `TurnStarted`, `Request`
//! (approval), `Snapshot`, `TurnComplete`, `Compaction*` — stay in the run loop and are
//! NOT handled here (they return an empty vec defensively). Splitting the stream half
//! out gives it a unit-test net and makes the eventual move into the TUI driver a
//! mechanical relocation (the bridge crate's deletion plan, Phase R).
//!
//! Stateful: a `ToolStarted` records the call into [`LiveTools`] so the matching
//! `ToolResult` recovers its name + duration (the kernel result carries neither); a
//! `Usage` tallies [`TurnStats`] and caches the last meta for the synthesized
//! `ContextStats`.

use atomcode_coding::{CodingAgentConfig, LiveTools, TurnStats};
use atomcode_core::agent::{AgentEvent as CoreEv, AgentPhase};
use atomcode_core::conversation::ConversationSnapshot;
use atomcode_kernel::event::AgentEvent as KEv;
use atomcode_kernel::message::MessageMeta;

use crate::convert;
use crate::runtime::{friendly_provider_error, truncate};

/// The synthesized `ContextStats` the bridge emits after each `Usage` (and on a manual
/// `/context` refresh / committed compaction). Most fields are zero — the kernel doesn't
/// break tokens down the way v1's ctx strategy did — but `sent_tokens` + `ctx_window`
/// come from the last provider usage report (falling back to the configured window).
pub(crate) fn context_stats_event(last_usage: Option<&MessageMeta>, cfg: &CodingAgentConfig) -> CoreEv {
    let sent = last_usage.map(|m| m.used_tokens as usize).unwrap_or(0);
    let ctx_window = last_usage
        .map(|m| m.ctx_window as usize)
        .filter(|w| *w > 0)
        .unwrap_or(cfg.context_window as usize);
    CoreEv::ContextStats {
        system_tokens: 0,
        sent_tokens: sent,
        dropped_tokens: 0,
        working_set_tokens: sent,
        total_messages: 0,
        tool_defs_tokens: 0,
        cold_zone_tokens: 0,
        ctx_window,
        ctx_name: "engine-v2".into(),
        system_prompt: String::new(),
    }
}

/// Translate one STREAM kernel event into the legacy core events to emit (0, 1, or 2).
/// Returns an empty vec for lifecycle events the run loop owns.
pub(crate) fn translate_event(
    ev: KEv,
    live: &mut LiveTools,
    stats: &mut TurnStats,
    last_usage: &mut Option<MessageMeta>,
    cfg: &CodingAgentConfig,
) -> Vec<CoreEv> {
    match ev {
        KEv::TextDelta(t) => vec![CoreEv::TextDelta(t)],
        KEv::Reasoning(t) => vec![CoreEv::ReasoningDelta(t)],
        KEv::ToolCallStreaming { name, arguments, .. } => vec![CoreEv::ToolCallStreaming {
            name: name.unwrap_or_default(),
            hint: truncate(&arguments, 80),
        }],
        KEv::ToolStarted { call } => {
            stats.on_tool_started();
            live.record(&call.id, &call.name);
            vec![
                CoreEv::PhaseChange(AgentPhase::CallingTool(call.name.clone())),
                CoreEv::ToolCallStarted { id: call.id, name: call.name, arguments: call.arguments },
            ]
        }
        KEv::ToolProgress { call_id, message } => {
            vec![CoreEv::ToolOutputChunk { call_id, chunk: message }]
        }
        KEv::ToolResult { result } => {
            let (name, duration) = live.resolve(&result.call_id);
            vec![
                CoreEv::ToolCallResult {
                    call_id: result.call_id,
                    name,
                    output: result.content,
                    success: !result.is_error,
                    duration,
                },
                CoreEv::PhaseChange(AgentPhase::Thinking),
            ]
        }
        KEv::ToolBatchStarted { batch_id, calls } => vec![CoreEv::ToolBatchStarted {
            batch_id,
            calls: calls
                .into_iter()
                .map(|c| atomcode_core::turn::event::ToolBatchCall {
                    id: c.id,
                    name: c.name,
                    arguments: c.arguments,
                })
                .collect(),
        }],
        KEv::ToolBatchCompleted { batch_id, ok, total, elapsed_ms } => {
            vec![CoreEv::ToolBatchCompleted { batch_id, ok, total, elapsed_ms }]
        }
        KEv::Usage(meta) => {
            stats.on_usage(meta.tokens.prompt as usize, meta.tokens.completion as usize);
            let usage = CoreEv::TokenUsage(convert::usage_to_core(&meta.tokens));
            *last_usage = Some(meta);
            vec![usage, context_stats_event(last_usage.as_ref(), cfg)]
        }
        KEv::Warning(w) => vec![CoreEv::Warning(w)],
        KEv::Error { message, http_status, .. } => vec![CoreEv::Error {
            error: friendly_provider_error(message, http_status, &cfg.base_url),
            snapshot: ConversationSnapshot::default(),
        }],
        // Lifecycle events (TurnStarted / Request / Snapshot / TurnComplete /
        // Cancelled / Compaction*) are owned by the run loop — it needs the command
        // channel + async to round-trip them. Return nothing here.
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::event::AgentEvent as KE;
    use atomcode_kernel::stream::TokenUsage as KTok;
    use atomcode_kernel::tool::{ToolCall, ToolResult as KToolResult};

    fn cfg() -> CodingAgentConfig {
        CodingAgentConfig::new("sk", "https://api.deepseek.com/v1", "deepseek-chat", ".")
    }

    #[test]
    fn text_and_reasoning_pass_through() {
        let (mut live, mut stats, mut lu) = (LiveTools::new(), TurnStats::new(), None);
        let evs = translate_event(KE::TextDelta("hi".into()), &mut live, &mut stats, &mut lu, &cfg());
        assert!(matches!(evs.as_slice(), [CoreEv::TextDelta(t)] if t == "hi"));

        let evs = translate_event(KE::Reasoning("why".into()), &mut live, &mut stats, &mut lu, &cfg());
        assert!(matches!(evs.as_slice(), [CoreEv::ReasoningDelta(t)] if t == "why"));
    }

    #[test]
    fn tool_call_streaming_defaults_name_and_truncates_hint() {
        let (mut live, mut stats, mut lu) = (LiveTools::new(), TurnStats::new(), None);
        let evs = translate_event(
            KE::ToolCallStreaming { index: 0, id: None, name: Some("bash".into()), arguments: "{}".into() },
            &mut live, &mut stats, &mut lu, &cfg(),
        );
        assert!(matches!(evs.as_slice(), [CoreEv::ToolCallStreaming { name, .. }] if name == "bash"));

        // A later fragment carries no name → empty string (v1 parity).
        let evs = translate_event(
            KE::ToolCallStreaming { index: 0, id: None, name: None, arguments: "{}".into() },
            &mut live, &mut stats, &mut lu, &cfg(),
        );
        assert!(matches!(evs.as_slice(), [CoreEv::ToolCallStreaming { name, .. }] if name.is_empty()));
    }

    #[test]
    fn tool_started_records_live_and_emits_phase_then_started() {
        let (mut live, mut stats, mut lu) = (LiveTools::new(), TurnStats::new(), None);
        let evs = translate_event(
            KE::ToolStarted { call: ToolCall { id: "c1".into(), name: "bash".into(), arguments: "{}".into() } },
            &mut live, &mut stats, &mut lu, &cfg(),
        );
        assert!(matches!(&evs[0], CoreEv::PhaseChange(AgentPhase::CallingTool(n)) if n == "bash"));
        assert!(matches!(&evs[1], CoreEv::ToolCallStarted { id, name, .. } if id == "c1" && name == "bash"));
        assert_eq!(stats.tool_calls(), 1, "ToolStarted tallies a tool call");
    }

    #[test]
    fn tool_result_recovers_name_duration_and_emits_phase() {
        let (mut live, mut stats, mut lu) = (LiveTools::new(), TurnStats::new(), None);
        // Record the start so the result can recover the name.
        let _ = translate_event(
            KE::ToolStarted { call: ToolCall { id: "c1".into(), name: "bash".into(), arguments: "{}".into() } },
            &mut live, &mut stats, &mut lu, &cfg(),
        );
        let evs = translate_event(
            KE::ToolResult { result: KToolResult { call_id: "c1".into(), content: "ok".into(), is_error: false } },
            &mut live, &mut stats, &mut lu, &cfg(),
        );
        assert!(matches!(&evs[0],
            CoreEv::ToolCallResult { call_id, name, output, success, .. }
            if call_id == "c1" && name == "bash" && output == "ok" && *success));
        assert!(matches!(&evs[1], CoreEv::PhaseChange(AgentPhase::Thinking)));
    }

    #[test]
    fn tool_result_error_flag_maps_to_success_false() {
        let (mut live, mut stats, mut lu) = (LiveTools::new(), TurnStats::new(), None);
        let evs = translate_event(
            KE::ToolResult { result: KToolResult { call_id: "x".into(), content: "boom".into(), is_error: true } },
            &mut live, &mut stats, &mut lu, &cfg(),
        );
        assert!(matches!(&evs[0], CoreEv::ToolCallResult { success, .. } if !success));
    }

    #[test]
    fn tool_progress_maps_to_output_chunk() {
        let (mut live, mut stats, mut lu) = (LiveTools::new(), TurnStats::new(), None);
        let evs = translate_event(
            KE::ToolProgress { call_id: "c1".into(), message: "tick".into() },
            &mut live, &mut stats, &mut lu, &cfg(),
        );
        assert!(matches!(evs.as_slice(),
            [CoreEv::ToolOutputChunk { call_id, chunk }] if call_id == "c1" && chunk == "tick"));
    }

    #[test]
    fn usage_tallies_stats_and_emits_token_usage_then_context_stats() {
        let (mut live, mut stats, mut lu) = (LiveTools::new(), TurnStats::new(), None);
        let meta = MessageMeta {
            tokens: KTok { prompt: 10, completion: 5, cached: 0 },
            used_tokens: 100,
            ctx_window: 8000,
            ..Default::default()
        };
        let evs = translate_event(KE::Usage(meta), &mut live, &mut stats, &mut lu, &cfg());
        assert_eq!(stats.rounds(), 1, "Usage counts a round");
        assert_eq!(stats.total_tokens(), 15, "prompt+completion tallied");
        assert!(matches!(&evs[0], CoreEv::TokenUsage(_)));
        assert!(matches!(&evs[1],
            CoreEv::ContextStats { sent_tokens, ctx_window, .. }
            if *sent_tokens == 100 && *ctx_window == 8000));
        assert!(lu.is_some(), "last_usage cached for the next /context refresh");
    }

    #[test]
    fn usage_context_stats_falls_back_to_cfg_window_when_meta_window_zero() {
        let (mut live, mut stats, mut lu) = (LiveTools::new(), TurnStats::new(), None);
        let meta = MessageMeta {
            tokens: KTok { prompt: 1, completion: 1, cached: 0 },
            used_tokens: 0,
            ctx_window: 0,
            ..Default::default()
        };
        let evs = translate_event(KE::Usage(meta), &mut live, &mut stats, &mut lu, &cfg());
        // cfg() default context_window is 128_000.
        assert!(matches!(&evs[1], CoreEv::ContextStats { ctx_window, .. } if *ctx_window == 128_000));
    }

    #[test]
    fn warning_passes_through_and_error_maps_to_core_error() {
        let (mut live, mut stats, mut lu) = (LiveTools::new(), TurnStats::new(), None);
        let evs = translate_event(KE::Warning("careful".into()), &mut live, &mut stats, &mut lu, &cfg());
        assert!(matches!(evs.as_slice(), [CoreEv::Warning(w)] if w == "careful"));

        let evs = translate_event(
            KE::Error { message: "boom".into(), http_status: Some(500), code: None },
            &mut live, &mut stats, &mut lu, &cfg(),
        );
        // Non-AtomGit, non-401 → verbatim diagnostic kept (friendly_provider_error).
        assert!(matches!(evs.as_slice(), [CoreEv::Error { error, .. }] if error.contains("boom")));
    }

    #[test]
    fn batch_started_and_completed_map() {
        let (mut live, mut stats, mut lu) = (LiveTools::new(), TurnStats::new(), None);
        let evs = translate_event(
            KE::ToolBatchStarted {
                batch_id: "b1".into(),
                calls: vec![atomcode_kernel::event::ToolBatchCall {
                    id: "c1".into(), name: "read_file".into(), arguments: "{}".into(),
                }],
            },
            &mut live, &mut stats, &mut lu, &cfg(),
        );
        assert!(matches!(&evs[0], CoreEv::ToolBatchStarted { batch_id, calls } if batch_id == "b1" && calls.len() == 1));

        let evs = translate_event(
            KE::ToolBatchCompleted { batch_id: "b1".into(), ok: 1, total: 1, elapsed_ms: 5 },
            &mut live, &mut stats, &mut lu, &cfg(),
        );
        assert!(matches!(&evs[0], CoreEv::ToolBatchCompleted { batch_id, ok, total, .. } if batch_id == "b1" && *ok == 1 && *total == 1));
    }

    #[test]
    fn lifecycle_events_return_empty_for_the_loop_to_own() {
        let (mut live, mut stats, mut lu) = (LiveTools::new(), TurnStats::new(), None);
        // TurnStarted / TurnComplete / Cancelled are owned by the run loop.
        assert!(translate_event(KE::TurnStarted, &mut live, &mut stats, &mut lu, &cfg()).is_empty());
        assert!(translate_event(
            KE::TurnComplete { reason: atomcode_kernel::event::StopReason::Stopped },
            &mut live, &mut stats, &mut lu, &cfg(),
        ).is_empty());
        assert!(translate_event(KE::Cancelled, &mut live, &mut stats, &mut lu, &cfg()).is_empty());
    }
}
