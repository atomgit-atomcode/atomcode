//! Configuration for assembling a coding agent.

use std::path::PathBuf;
use std::time::Duration;

/// Everything [`build_coding_agent`](crate::build_coding_agent) needs: provider
/// credentials, the working directory the tools are scoped to, and liveness bounds.
///
/// Timeouts default to sane non-infinite values — the kernel itself defaults to
/// unbounded, and the assembly map flagged "L2 MUST set stream/request timeouts" so a
/// stalled provider or silent driver can never park a turn forever.
#[derive(Clone)]
pub struct CodingAgentConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    /// Directory the agent's tools see as their working dir — PINNED (via the kernel
    /// `working_dir` seam), not the process-global cwd, so concurrent agents don't race.
    pub working_dir: PathBuf,
    /// Model context window in tokens (forwarded to the provider). Default 128k.
    pub context_window: u32,
    /// Liveness: max byte-idle wait for the next stream event (first-token + inter-token).
    /// Default 300s, override via `ATOMCODE_STREAM_TIMEOUT_SECS`. Thinking models go quiet
    /// for a long stretch after a large (~200K) prompt before the first reasoning byte; the
    /// old 120s cut them off mid-think and surfaced as a spurious "stream timeout".
    pub stream_timeout: Duration,
    /// Liveness: max wait for a driver approval response before it degrades to deny. Default 300s.
    pub request_timeout: Duration,
    /// Safety fuse: max edit-then-verify continuations per turn (kernel default is 50).
    pub max_continuations: u32,
    /// Per-call provider options (reasoning effort / max_tokens / temperature).
    /// Default = no opinion. A respawn (re-`assemble` on the same parts) picks up
    /// changes — how a driver implements `/effort`.
    pub chat_options: atomcode_kernel::provider::ChatOptions,
    /// Optional telemetry sink. `Some` ⇒ `prepare` registers a [`TelemetryHook`]
    /// that emits `LlmChat` per round (the kernel's neutral telemetry seam). `None`
    /// (default) ⇒ no telemetry — the kernel stays zero-telemetry.
    ///
    /// [`TelemetryHook`]: crate::TelemetryHook
    pub telemetry: Option<std::sync::Arc<atomcode_telemetry::Telemetry>>,
    /// Provider `reasoning_history` override (`"include"` | `"exclude"`), passed
    /// through verbatim to the provider builder. `None`/empty (default) ⇒ the
    /// adapter's per-model auto-detect ([`ReasoningPolicy::derive`]). This is the
    /// config knob, not a code default — the heuristic only applies when it's unset.
    ///
    /// [`ReasoningPolicy::derive`]: atomcode_capabilities::provider::ReasoningPolicy::derive
    pub reasoning_history: Option<String>,
    /// Auto-compaction trigger as a fraction of the context window (real utilization
    /// from the provider's reported prompt tokens). At/above this, the task-boundary
    /// trigger runs [`StubCompaction`] to stub old tool results. Default `0.7` (the
    /// normal-path threshold ported from core). Set `>= 1.0` to effectively disable.
    ///
    /// [`StubCompaction`]: atomcode_capabilities::compaction::StubCompaction
    pub compact_threshold: f32,
}

/// The default byte-idle stream timeout: `ATOMCODE_STREAM_TIMEOUT_SECS` if set to a valid
/// positive integer, else 300s. Ported from core's env-configurable liveness knob.
fn default_stream_timeout() -> Duration {
    std::env::var("ATOMCODE_STREAM_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(300))
}

impl CodingAgentConfig {
    /// Construct with the required fields and sane defaults for the rest.
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        working_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: base_url.into(),
            model: model.into(),
            working_dir: working_dir.into(),
            context_window: 128_000,
            stream_timeout: default_stream_timeout(),
            request_timeout: Duration::from_secs(300),
            max_continuations: 50,
            chat_options: Default::default(),
            telemetry: None,
            reasoning_history: None,
            compact_threshold: 0.7,
        }
    }
}

// Manual Debug: `atomcode_telemetry::Telemetry` is not `Debug`. Skip it; redact the
// api_key while we're here.
impl std::fmt::Debug for CodingAgentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodingAgentConfig")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("working_dir", &self.working_dir)
            .field("context_window", &self.context_window)
            .field("stream_timeout", &self.stream_timeout)
            .field("request_timeout", &self.request_timeout)
            .field("max_continuations", &self.max_continuations)
            .field("chat_options", &self.chat_options)
            .field("telemetry", &self.telemetry.is_some())
            .finish_non_exhaustive()
    }
}
