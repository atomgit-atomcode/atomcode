//! Kernel-native runtime spawn helper for the daemon.
//!
//! Currently **unused** — scaffolded behind the `ATOMCODE_DAEMON_ENGINE`
//! runtime switch so later tasks can wire it in without touching this module.
//!
//! The spawn pipeline (`prepare → assemble → spawn`) mirrors the working
//! template in `crates/atomcode-cli/src/acp/engine.rs::spawn_session`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use atomcode_bridge::BridgeConfig;
use atomcode_coding::config::CodingAgentConfig;
use atomcode_coding::parts::{assemble, prepare, PrepareOptions};
use atomcode_kernel::agent::AgentHandle;
use atomcode_kernel::provider::LlmProvider;

/// Map a [`BridgeConfig`] to a [`CodingAgentConfig`] for the kernel-native path.
///
/// Mirrors the field-by-field construction in `atomcode_bridge::runtime::Bridge::run`
/// (lines 286-342) so the kernel path honors exactly the same knobs as the bridge path.
/// Fields not present in `BridgeConfig` are left at `CodingAgentConfig::new()`'s defaults
/// (the same fields bridge also leaves at their defaults).
///
/// The subagent tier providers (`subagent_fast_provider` / `subagent_capable_provider`) are
/// NOT wired here: they require loading the full `atomcode_config::Config` and calling
/// `resolve_tier_thunks`, which is a bridge-internal function. A future task will wire those
/// when the kernel path grows full subagent support.
#[allow(dead_code)]
pub fn coding_config_from_bridge(cfg: &BridgeConfig) -> CodingAgentConfig {
    let mut coding_cfg = CodingAgentConfig::new(
        &cfg.api_key,
        &cfg.base_url,
        &cfg.model,
        &cfg.working_dir,
    );
    coding_cfg.context_window = cfg.context_window;
    // User-configured per-call output cap (parity with `apply_reload_provider`); `None` ⇒
    // the per-provider fallback in `build_provider` applies.
    coding_cfg.chat_options.max_tokens = cfg.max_tokens;
    coding_cfg.telemetry = cfg.telemetry.clone();
    coding_cfg.reasoning_history = cfg.reasoning_history.clone();
    // `/effort`: thread the per-provider reasoning_effort into the per-call ChatOptions
    // so the kernel path actually emits it (openai_compat → `reasoning_effort` body field).
    coding_cfg.chat_options.reasoning_effort =
        atomcode_kernel::provider::ReasoningEffort::from_config(cfg.reasoning_effort.as_deref());
    // Adapter selection + thinking controls (so Claude-/Ollama-native + /think work).
    coding_cfg.provider_type = cfg.provider_type.clone();
    coding_cfg.thinking_enabled = cfg.thinking_enabled;
    coding_cfg.thinking_type = cfg.thinking_type.clone();
    coding_cfg.thinking_keep = cfg.thinking_keep.clone();
    // Gateway identity: product UA + TLS-verify toggle.
    coding_cfg.user_agent = cfg.user_agent.clone();
    coding_cfg.skip_tls_verify = cfg.skip_tls_verify;
    coding_cfg.loop_max_rounds = cfg.loop_max_rounds;
    // Interactive drivers PARK approvals (a present human must not be auto-denied for
    // thinking too long); headless keeps the configured fail-closed timeout.
    if cfg.interactive {
        coding_cfg.request_timeout = None;
    }
    coding_cfg.keep_interrupted_context = cfg.keep_interrupted_context;
    coding_cfg
}

/// Returns `true` when `ATOMCODE_DAEMON_ENGINE=kernel` is set in the environment.
///
/// Used as a feature gate: future tasks branch on this to decide whether to
/// drive turns through the kernel-native path or the legacy bridge.
#[allow(dead_code)]
pub fn engine_is_kernel() -> bool {
    std::env::var("ATOMCODE_DAEMON_ENGINE").as_deref() == Ok("kernel")
}

/// Spawn a kernel-native agent for a daemon turn.
///
/// Runs the two-phase `prepare → assemble → spawn` pipeline and returns a live
/// [`AgentHandle`] that a future task's turn executor can drive.
///
/// # Arguments
/// * `cfg` — coding agent configuration (working dir, model, provider, etc.).
/// * `provider` — pre-built (possibly authenticated) LLM provider.
/// * `opts` — prepare options controlling which hooks are loaded.
#[allow(dead_code)]
pub async fn spawn(
    cfg: &CodingAgentConfig,
    provider: Arc<dyn LlmProvider>,
    opts: PrepareOptions,
) -> anyhow::Result<AgentHandle> {
    let mut parts = prepare(cfg, opts).await?;
    let agent = assemble(&mut parts, cfg, provider)
        .map_err(|e| anyhow::anyhow!("daemon kernel assemble failed: {e}"))?;
    Ok(agent.spawn())
}

/// The core (`v1-protocol`) event shape the daemon's SSE layer already forwards.
#[allow(dead_code)]
type CoreEv = atomcode_core::agent::AgentEvent;

/// The kernel event shape the runtime produces.
#[allow(dead_code)]
type KEv = atomcode_kernel::event::AgentEvent;

/// Truncates `s` to at most `max` *chars*, appending an ellipsis when clipped.
///
/// Ported verbatim from `atomcode_bridge::runtime::truncate` (the `ToolCallStreaming`
/// hint mapping depends on the exact `…`-appending behavior).
#[allow(dead_code)]
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

/// Translates kernel [`AgentEvent`](atomcode_kernel::event::AgentEvent)s into the
/// core [`AgentEvent`](atomcode_core::agent::AgentEvent) (`CoreEv`) shapes the
/// daemon's SSE code already forwards.
///
/// One kernel event may fan out to `0..n` `CoreEv`. This is the kernel-native
/// replacement for the streaming/tool/usage arms of
/// `atomcode_bridge::runtime::Bridge::on_kernel_event` — every field mapping is
/// ported verbatim from there (see that function for the authoritative source).
///
/// **Scope:** streaming / tool / usage arms only. The *synthesis* arms
/// (`Snapshot`, `TurnComplete`, `CompactionStarted`, `Compacted`, `Error`,
/// `Request`, `TurnStarted`) are Task 4's territory: they currently return
/// `vec![]` and are marked `// Task 4:`. This struct is unused until Task 6
/// wires it into the turn executor.
#[allow(dead_code)]
#[derive(Default)]
pub struct KernelToWebui {
    /// Per-call timer, keyed by tool `call_id`, started on `ToolStarted` and
    /// consumed on `ToolResult` to fill `ToolCallResult.duration`. Mirrors the
    /// `started` half of bridge's `live_tools` map. (Bridge also caches the tool
    /// name here to recover it on `ToolResult`; the daemon carries the name on
    /// the kernel `ToolResult`'s originating call, so only the timer is needed —
    /// but for parity with bridge we also stash the name.)
    live_tools: HashMap<String, (String, Instant)>,
    /// The most recent `Usage(MessageMeta)`. Kept so Task 4 can synthesize
    /// `ContextStats` on top of `TokenUsage` (the synthesis itself is NOT done
    /// here — see the `Usage` arm).
    last_usage: Option<atomcode_kernel::message::MessageMeta>,
    /// Running per-turn counters (tool calls / rounds / total tokens). Task 4
    /// reads these for `TurnComplete` synthesis; here they are only accumulated.
    tool_calls: usize,
    rounds: usize,
    total_tokens: usize,
}

#[allow(dead_code)]
impl KernelToWebui {
    pub fn new() -> Self {
        Self::default()
    }

    /// Translate one kernel event into `0..n` core events.
    pub fn translate(&mut self, ev: KEv) -> Vec<CoreEv> {
        match ev {
            // ---- streaming ----
            KEv::TextDelta(t) => vec![CoreEv::TextDelta(t)],
            KEv::Reasoning(t) => vec![CoreEv::ReasoningDelta(t)],
            KEv::ToolCallStreaming { name, arguments, .. } => {
                vec![CoreEv::ToolCallStreaming {
                    name: name.unwrap_or_default(),
                    hint: truncate(&arguments, 80),
                }]
            }
            // ---- tool ----
            KEv::ToolStarted { call } => {
                self.tool_calls += 1;
                self.live_tools
                    .insert(call.id.clone(), (call.name.clone(), Instant::now()));
                vec![CoreEv::ToolCallStarted {
                    id: call.id,
                    name: call.name,
                    arguments: call.arguments,
                }]
            }
            KEv::ToolProgress { call_id, message } => {
                vec![CoreEv::ToolOutputChunk { call_id, chunk: message }]
            }
            KEv::ToolResult { result } => {
                // Recover the tool name + start instant recorded on ToolStarted.
                // If the timer entry is missing (result with no prior start),
                // bridge defaults to name "tool" and `Instant::now()` → a
                // ~zero duration; mirror that exactly.
                let (name, started) = self
                    .live_tools
                    .remove(&result.call_id)
                    .unwrap_or_else(|| ("tool".into(), Instant::now()));
                vec![CoreEv::ToolCallResult {
                    call_id: result.call_id,
                    name,
                    output: result.content,
                    success: !result.is_error,
                    duration: started.elapsed(),
                }]
            }
            KEv::ToolBatchStarted { batch_id, calls } => {
                vec![CoreEv::ToolBatchStarted {
                    batch_id,
                    calls: calls
                        .into_iter()
                        .map(|c| atomcode_core::turn::event::ToolBatchCall {
                            id: c.id,
                            name: c.name,
                            arguments: c.arguments,
                        })
                        .collect(),
                }]
            }
            KEv::ToolBatchCompleted { batch_id, ok, total, elapsed_ms } => {
                vec![CoreEv::ToolBatchCompleted { batch_id, ok, total, elapsed_ms }]
            }
            // ---- advisory ----
            KEv::Warning(w) => vec![CoreEv::Warning(w)],
            KEv::RateLimited { reset_at_display, reset_label, secs_until_reset, auto_resuming } => {
                vec![CoreEv::RateLimited {
                    reset_at_display,
                    reset_label,
                    secs_until_reset,
                    auto_resuming,
                }]
            }
            // ---- usage ----
            KEv::Usage(meta) => {
                self.rounds += 1;
                self.total_tokens += (meta.tokens.prompt + meta.tokens.completion) as usize;
                let ev = CoreEv::TokenUsage(atomcode_bridge::convert::usage_to_core(&meta.tokens));
                self.last_usage = Some(meta);
                // Task 4: emit ContextStats synthesized from `last_usage` here.
                vec![ev]
            }
            // ---- Task 4: synthesis arms (Snapshot / TurnComplete / compaction /
            // Error / Request / TurnStarted). Return no events for now; a later
            // task fills these in. Do NOT panic here. ----
            KEv::TurnStarted => vec![], // Task 4:
            KEv::Snapshot { .. } => vec![], // Task 4:
            KEv::TurnComplete { .. } => vec![], // Task 4:
            KEv::CompactionStarted { .. } => vec![], // Task 4:
            KEv::Compacted { .. } => vec![], // Task 4:
            KEv::Error { .. } => vec![], // Task 4:
            KEv::Request { .. } => vec![], // Task 4:
            KEv::Cancelled => vec![], // Task 4:
            // `#[non_exhaustive]` kernel enum: any future variant is silently
            // dropped until a task maps it.
            _ => vec![],
        }
    }
}

#[cfg(test)]
mod kernel_runtime_translate_tests {
    use super::*;
    use atomcode_kernel::event::AgentEvent as KEv;
    use atomcode_kernel::message::MessageMeta;
    use atomcode_kernel::stream::TokenUsage as KTokenUsage;
    use atomcode_kernel::tool::{ToolCall, ToolResult};

    #[test]
    fn text_delta_maps_1to1() {
        let mut t = KernelToWebui::new();
        let out = t.translate(KEv::TextDelta("hello".into()));
        assert!(matches!(&out[..], [CoreEv::TextDelta(s)] if s == "hello"));
    }

    #[test]
    fn reasoning_maps_to_reasoning_delta() {
        let mut t = KernelToWebui::new();
        let out = t.translate(KEv::Reasoning("thinking…".into()));
        assert!(matches!(&out[..], [CoreEv::ReasoningDelta(s)] if s == "thinking…"));
    }

    #[test]
    fn tool_call_streaming_maps_name_and_truncated_hint() {
        let mut t = KernelToWebui::new();
        let long_args = "x".repeat(200);
        let out = t.translate(KEv::ToolCallStreaming {
            index: 0,
            id: Some("c1".into()),
            name: Some("bash".into()),
            arguments: long_args.clone(),
        });
        match &out[..] {
            [CoreEv::ToolCallStreaming { name, hint }] => {
                assert_eq!(name, "bash");
                // 80 chars + ellipsis (bridge `truncate(_, 80)`).
                assert_eq!(hint.chars().count(), 81);
                assert!(hint.ends_with('…'));
            }
            _ => panic!("expected ToolCallStreaming, got {out:?}"),
        }
    }

    #[test]
    fn tool_call_streaming_missing_name_defaults_empty() {
        let mut t = KernelToWebui::new();
        let out = t.translate(KEv::ToolCallStreaming {
            index: 0,
            id: None,
            name: None,
            arguments: "short".into(),
        });
        match &out[..] {
            [CoreEv::ToolCallStreaming { name, hint }] => {
                assert_eq!(name, "");
                assert_eq!(hint, "short");
            }
            _ => panic!("expected ToolCallStreaming, got {out:?}"),
        }
    }

    #[test]
    fn tool_started_maps_and_records_timer() {
        let mut t = KernelToWebui::new();
        let out = t.translate(KEv::ToolStarted {
            call: ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: "{\"command\":\"ls\"}".into(),
            },
        });
        match &out[..] {
            [CoreEv::ToolCallStarted { id, name, arguments }] => {
                assert_eq!(id, "c1");
                assert_eq!(name, "bash");
                assert_eq!(arguments, "{\"command\":\"ls\"}");
            }
            _ => panic!("expected ToolCallStarted, got {out:?}"),
        }
        assert!(t.live_tools.contains_key("c1"));
        assert_eq!(t.tool_calls, 1);
    }

    #[test]
    fn tool_progress_maps_to_output_chunk() {
        let mut t = KernelToWebui::new();
        let out = t.translate(KEv::ToolProgress {
            call_id: "c1".into(),
            message: "working…".into(),
        });
        match &out[..] {
            [CoreEv::ToolOutputChunk { call_id, chunk }] => {
                assert_eq!(call_id, "c1");
                assert_eq!(chunk, "working…");
            }
            _ => panic!("expected ToolOutputChunk, got {out:?}"),
        }
    }

    #[test]
    fn tool_result_fills_duration_from_timer() {
        let mut t = KernelToWebui::new();
        // Start the timer first.
        t.translate(KEv::ToolStarted {
            call: ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: "{}".into(),
            },
        });
        std::thread::sleep(std::time::Duration::from_millis(5));
        let out = t.translate(KEv::ToolResult {
            result: ToolResult {
                call_id: "c1".into(),
                content: "done".into(),
                is_error: false,
                images: vec![],
            },
        });
        match &out[..] {
            [CoreEv::ToolCallResult { call_id, name, output, success, duration }] => {
                assert_eq!(call_id, "c1");
                // Name recovered from the timer entry recorded on ToolStarted.
                assert_eq!(name, "bash");
                assert_eq!(output, "done");
                assert!(*success);
                assert!(duration.as_millis() >= 5, "duration {duration:?} not from timer");
            }
            _ => panic!("expected ToolCallResult, got {out:?}"),
        }
        // Timer entry consumed.
        assert!(!t.live_tools.contains_key("c1"));
    }

    #[test]
    fn tool_result_without_timer_defaults_name_and_zero_duration() {
        let mut t = KernelToWebui::new();
        let out = t.translate(KEv::ToolResult {
            result: ToolResult {
                call_id: "orphan".into(),
                content: "boom".into(),
                is_error: true,
                images: vec![],
            },
        });
        match &out[..] {
            [CoreEv::ToolCallResult { call_id, name, output, success, duration }] => {
                assert_eq!(call_id, "orphan");
                assert_eq!(name, "tool"); // bridge default
                assert_eq!(output, "boom");
                assert!(!*success); // is_error → success == false
                // No prior start → ~zero (Instant::now().elapsed()).
                assert!(duration.as_millis() < 50);
            }
            _ => panic!("expected ToolCallResult, got {out:?}"),
        }
    }

    #[test]
    fn tool_batch_started_and_completed_map_1to1() {
        let mut t = KernelToWebui::new();
        let started = t.translate(KEv::ToolBatchStarted {
            batch_id: "b1".into(),
            calls: vec![atomcode_kernel::event::ToolBatchCall {
                id: "c1".into(),
                name: "read_file".into(),
                arguments: "{}".into(),
            }],
        });
        match &started[..] {
            [CoreEv::ToolBatchStarted { batch_id, calls }] => {
                assert_eq!(batch_id, "b1");
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "read_file");
            }
            _ => panic!("expected ToolBatchStarted, got {started:?}"),
        }
        let done = t.translate(KEv::ToolBatchCompleted {
            batch_id: "b1".into(),
            ok: 3,
            total: 4,
            elapsed_ms: 120,
        });
        assert!(matches!(
            &done[..],
            [CoreEv::ToolBatchCompleted { batch_id, ok: 3, total: 4, elapsed_ms: 120 }]
                if batch_id == "b1"
        ));
    }

    #[test]
    fn warning_maps_1to1() {
        let mut t = KernelToWebui::new();
        let out = t.translate(KEv::Warning("truncated".into()));
        assert!(matches!(&out[..], [CoreEv::Warning(w)] if w == "truncated"));
    }

    #[test]
    fn rate_limited_maps_fields_verbatim() {
        let mut t = KernelToWebui::new();
        let out = t.translate(KEv::RateLimited {
            reset_at_display: "12:00".into(),
            reset_label: "resets".into(),
            secs_until_reset: Some(60),
            auto_resuming: true,
        });
        match &out[..] {
            [CoreEv::RateLimited { reset_at_display, reset_label, secs_until_reset, auto_resuming }] => {
                assert_eq!(reset_at_display, "12:00");
                assert_eq!(reset_label, "resets");
                assert_eq!(*secs_until_reset, Some(60));
                assert!(*auto_resuming);
            }
            _ => panic!("expected RateLimited, got {out:?}"),
        }
    }

    #[test]
    fn usage_maps_to_token_usage_and_stashes_last_usage() {
        let mut t = KernelToWebui::new();
        let meta = MessageMeta {
            tokens: KTokenUsage { prompt: 100, completion: 20, cached: 5, ..Default::default() },
            used_tokens: 120,
            ..Default::default()
        };
        let out = t.translate(KEv::Usage(meta));
        match &out[..] {
            [CoreEv::TokenUsage(u)] => {
                assert_eq!(u.prompt_tokens, 100);
                assert_eq!(u.completion_tokens, 20);
                assert_eq!(u.cached_tokens, 5);
            }
            _ => panic!("expected TokenUsage, got {out:?}"),
        }
        // last_usage stashed for Task 4; counters accumulated.
        assert!(t.last_usage.is_some());
        assert_eq!(t.rounds, 1);
        assert_eq!(t.total_tokens, 120);
    }

    #[test]
    fn task4_synthesis_arms_return_empty() {
        let mut t = KernelToWebui::new();
        assert!(t.translate(KEv::TurnStarted).is_empty());
        assert!(t
            .translate(KEv::TurnComplete { reason: Default::default() })
            .is_empty());
        assert!(t.translate(KEv::Cancelled).is_empty());
        assert!(t
            .translate(KEv::Error { message: "x".into(), http_status: None, code: None })
            .is_empty());
    }
}
