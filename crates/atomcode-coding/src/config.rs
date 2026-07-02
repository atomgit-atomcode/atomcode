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
    /// Liveness: max wait for a driver approval response before it degrades to deny.
    /// `Some(d)` ⇒ fail-closed after `d` — for HEADLESS / no-human drivers where a never-
    /// answered approval must not park a turn forever. `None` ⇒ PARK: block until the driver
    /// answers (or the turn is cancelled / the driver dies) — for INTERACTIVE drivers, so a
    /// present human is never auto-denied for thinking too long. Default `Some(300s)`.
    /// NOTE: approval is the only driver round-trip in this stack, so this is effectively the
    /// approval timeout.
    pub request_timeout: Option<Duration>,
    /// Safety fuse: max edit-then-verify continuations per turn (kernel default is 50).
    pub max_continuations: u32,
    /// Goal-mode round cap (0 = unbounded). Override via `ATOMCODE_GOAL_MAX_ROUNDS`.
    pub goal_max_rounds: u32,
    /// Goal-mode wall-clock cap in seconds (0 = unbounded). Override via
    /// `ATOMCODE_GOAL_MAX_DURATION_SECS`.
    pub goal_max_duration_secs: u64,
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
    /// Provider adapter kind: `"openai"` (default, OpenAI-compatible), `"claude"`
    /// (Anthropic Messages API), or `"ollama"`. Selects which v2 provider adapter the
    /// builder constructs — mirrors v1's `provider_type` dispatch. Empty/unknown ⇒ openai.
    pub provider_type: String,
    /// Extended-thinking toggle for the Anthropic adapter (`/think on|off`). `Some(true)`
    /// ⇒ `thinking: {type:"adaptive"}` on the wire. `None`/`Some(false)` ⇒ off. (v2 uses
    /// adaptive thinking, so v1's `thinking_budget` has no direct mapping and is dropped.)
    pub thinking_enabled: Option<bool>,
    /// Kimi-family thinking control for the OpenAI-compatible adapter: `thinking.type`
    /// (`"enabled"`/`"disabled"`). `None` ⇒ omit.
    pub thinking_type: Option<String>,
    /// Kimi K2.6 preserved thinking: `thinking.keep`. `None` ⇒ omit.
    pub thinking_keep: Option<String>,
    /// Auto-compaction trigger as a fraction of the context window (real utilization
    /// from the provider's reported prompt tokens). At/above this, the task-boundary
    /// trigger runs [`StubCompaction`] to stub old tool results. Default `0.7` (the
    /// normal-path threshold ported from core). Set `>= 1.0` to effectively disable.
    ///
    /// [`StubCompaction`]: atomcode_capabilities::compaction::StubCompaction
    pub compact_threshold: f32,
    /// `web_search` backend: `"exa"` (default, globally reachable, keyless) or
    /// `"duckduckgo"`/`"ddg"` (legacy HTML scraping, blocked in some regions). `None`/empty
    /// /unknown ⇒ Exa. Mirrors v1's `[web_search] provider` config knob — without this the
    /// tool was hardwired to Exa with no way to opt into DDG.
    pub web_search_provider: Option<String>,
    /// On Ctrl-C / cancel: `false` (default) ⇒ CANCEL = UNDO (roll back the interrupted
    /// turn). `true` ⇒ PRESERVE the partial turn + backfill dangling tool_calls + inject
    /// an interruption marker, forwarded to the kernel `Agent` builder
    /// (`keep_interrupted_context`). Sourced from `Config::keep_interrupted_context`.
    pub keep_interrupted_context: bool,
    /// Per-provider User-Agent override (`ProviderConfig::user_agent`). `None` ⇒
    /// `build_provider` falls back to the product `atomcode/<version>` so the gateway
    /// can attribute/slice traffic by version. Restores parity with v1's
    /// `build_http_client`, which the v2 adapters had dropped.
    pub user_agent: Option<String>,
    /// Disable TLS certificate verification (self-signed / internal gateways).
    /// Sourced from `ProviderConfig::skip_tls_verify`; default false.
    pub skip_tls_verify: bool,
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
fn default_goal_max_rounds() -> u32 {
    std::env::var("ATOMCODE_GOAL_MAX_ROUNDS")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(200)
}
fn default_goal_max_duration_secs() -> u64 {
    std::env::var("ATOMCODE_GOAL_MAX_DURATION_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(7200)
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
            request_timeout: Some(Duration::from_secs(300)),
            max_continuations: 50,
            goal_max_rounds: default_goal_max_rounds(),
            goal_max_duration_secs: default_goal_max_duration_secs(),
            chat_options: Default::default(),
            telemetry: None,
            reasoning_history: None,
            provider_type: "openai".into(),
            thinking_enabled: None,
            thinking_type: None,
            thinking_keep: None,
            compact_threshold: 0.7,
            web_search_provider: None,
            keep_interrupted_context: false,
            user_agent: None,
            skip_tls_verify: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_caps_have_generous_defaults() {
        let c = CodingAgentConfig::new("k", "https://x/v1", "m", "/tmp");
        assert_eq!(c.goal_max_rounds, 200);
        assert_eq!(c.goal_max_duration_secs, 7200);
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
            .field("goal_max_rounds", &self.goal_max_rounds)
            .field("goal_max_duration_secs", &self.goal_max_duration_secs)
            .field("chat_options", &self.chat_options)
            .field("telemetry", &self.telemetry.is_some())
            .finish_non_exhaustive()
    }
}
