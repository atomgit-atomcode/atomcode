//! Configuration for assembling a coding agent.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use atomcode_config::locale::Locale;
use atomcode_kernel::agent::ToolLoopPolicy;

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
    /// Final image-input capability resolved from the model profile override or
    /// the backwards-compatible Auto heuristic.
    pub supports_vision: bool,
    /// Whether this concrete endpoint accepts a reasoning-effort control.
    /// Kept separate from `chat_options.reasoning_effort`: API-default effort is
    /// still a supported endpoint with no per-call value.
    pub supports_reasoning_effort: bool,
    /// Preferred language for natural-language commit subjects and bodies.
    /// `None` means follow the current conversation language.
    pub preferred_language: Option<Locale>,
    /// Resolved `[tools.todo]` policy for this runtime generation.
    pub todo: atomcode_config::config::TodoToolConfig,
    /// Stable config/provider registry key exposed to drivers. This is distinct
    /// from `provider_type`, which selects the adapter implementation.
    pub provider_name: String,
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
    /// Applies to every kernel request/respond round-trip, including approvals and
    /// structured `request_user_input` prompts.
    pub request_timeout: Option<Duration>,
    /// Safety fuse: max edit-then-verify continuations per turn (kernel default is 50).
    pub max_continuations: u32,
    /// Coarse safety fuse for LLM/tool rounds in one turn (`0` = unbounded).
    /// This bounds varying-call runaways that the kernel's repetition guards cannot catch.
    /// It is deliberately generous, produces an explicit incomplete terminal, and may be
    /// overridden with `ATOMCODE_TURN_MAX_ROUNDS`.
    pub max_rounds: u32,
    /// When true, the kernel turns the `max_rounds` cap into an interactive
    /// checkpoint (see AgentBuilder). Default false; only the TUI driver sets it.
    pub round_cap_checkpoint: bool,
    /// Generate an ephemeral next-prompt suggestion after a naturally completed
    /// turn. The coding runtime owns the auxiliary request and cancellation;
    /// drivers only project the resulting neutral event. Default false so
    /// headless/daemon/ACP paths do not incur a hidden model request before they
    /// implement the corresponding UI. The interactive TUI opts in explicitly.
    pub next_prompt_suggestions: bool,
    /// Exact no-progress loop policy. `None` disables it for explicitly intentional
    /// identical repetition. Defaults to 3/4 and is configurable through
    /// `ATOMCODE_TOOL_LOOP_WARNING_THRESHOLD` / `ATOMCODE_TOOL_LOOP_STOP_THRESHOLD`;
    /// a stop threshold of `0` disables the policy.
    pub tool_loop_policy: Option<ToolLoopPolicy>,
    /// Goal-mode round cap (0 = unbounded). Override via `ATOMCODE_GOAL_MAX_ROUNDS`.
    pub goal_max_rounds: u32,
    /// Goal-mode wall-clock cap in seconds (0 = unbounded). Override via
    /// `ATOMCODE_GOAL_MAX_DURATION_SECS`.
    pub goal_max_duration_secs: u64,
    /// Self-paced `/loop` round cap. Default 100; the runtime overrides it from
    /// `[loop_config] max_rounds`. Env override `ATOMCODE_LOOP_MAX_ROUNDS`.
    pub loop_max_rounds: u32,
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
    /// Best-effort per-turn Markdown + per-round JSONL logging.
    pub datalog: atomcode_config::config::DatalogConfig,
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
    /// Opt-in read-only LSP policy. The manager is created by this runtime's tool
    /// assembly, so provider/session reloads cannot create a second hidden owner.
    pub lsp: atomcode_capabilities::codeintel::LspSettings,
    /// On Ctrl-C / cancel: `false` (default) ⇒ CANCEL = UNDO (roll back the interrupted
    /// turn). `true` ⇒ PRESERVE the partial turn + backfill dangling tool_calls + inject
    /// an interruption marker, forwarded to the kernel `Agent` builder
    /// (`keep_interrupted_context`). Sourced from `Config::keep_interrupted_context`.
    pub keep_interrupted_context: bool,
    pub credential_shell_policy: atomcode_capabilities::tools::CredentialShellPolicy,
    /// Per-provider User-Agent override (`ProviderConfig::user_agent`). `None` ⇒
    /// `build_provider` falls back to the product `atomcode/<version>` so the gateway
    /// can attribute/slice traffic by version. Restores parity with v1's
    /// `build_http_client`, which the v2 adapters had dropped.
    pub user_agent: Option<String>,
    /// Disable TLS certificate verification (self-signed / internal gateways).
    /// Sourced from `ProviderConfig::skip_tls_verify`; default false.
    pub skip_tls_verify: bool,
    /// Full provider registry used to resolve task-tool fast/capable tiers.
    pub subagent_config: Option<Arc<atomcode_config::config::Config>>,
    /// Swap-aware, lazily-built FAST-tier provider for the `task` tool. `None` ⇒ the fast
    /// tier reuses the host provider slot. Set by the runtime as a SHARED cell ([`TierProvider`])
    /// so a mid-session `/model` swap can `reset()` it — re-resolve the tier against the new
    /// host and drop the cache — and the already-built TaskTool picks up the new routing on its
    /// next dispatch (no `prepare` rerun). Built ON FIRST use, so startup stays cheap. NOT in
    /// the manual `Debug` impl.
    pub subagent_fast_provider: Option<Arc<TierProvider>>,
    /// Swap-aware, lazily-built CAPABLE-tier provider (same contract as above).
    pub subagent_capable_provider: Option<Arc<TierProvider>>,
}

/// Host-resolved inputs shared by CLI and daemon runtime construction.
/// This is a driver configuration object, not a legacy command protocol.
#[derive(Clone)]
pub struct CodingRuntimeConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub preferred_language: Option<Locale>,
    pub todo: atomcode_config::config::TodoToolConfig,
    pub provider_name: String,
    pub working_dir: PathBuf,
    pub context_window: u32,
    pub max_tokens: Option<u32>,
    pub mcp: bool,
    pub telemetry: Option<Arc<atomcode_telemetry::Telemetry>>,
    pub datalog: atomcode_config::config::DatalogConfig,
    pub reasoning_history: Option<String>,
    pub reasoning_effort: Option<String>,
    pub provider_type: String,
    pub thinking_enabled: Option<bool>,
    pub thinking_type: Option<String>,
    pub thinking_keep: Option<String>,
    pub dangerously_skip_permissions: bool,
    pub interactive: bool,
    pub keep_interrupted_context: bool,
    pub credential_shell_policy: atomcode_capabilities::tools::CredentialShellPolicy,
    pub user_agent: Option<String>,
    pub skip_tls_verify: bool,
    pub loop_max_rounds: u32,
    pub turn_max_rounds: u32,
    pub subagent_config: Option<Arc<atomcode_config::config::Config>>,
    /// When true, a `max_rounds` hit becomes an interactive continue/stop
    /// checkpoint (the kernel sends a `ROUND_CAP_CHECKPOINT_KIND` Request)
    /// instead of a hard error. Only the TUI implements the picker, so this
    /// must stay `false` for headless / ACP / daemon runtimes (there is no
    /// requester to answer the Request → the kernel fail-closes to a stop).
    pub round_cap_checkpoint: bool,
    /// Driver opt-in for ephemeral next-prompt sampling. Default false;
    /// currently only the interactive TUI renders and accepts the result.
    pub next_prompt_suggestions: bool,
    pub supports_vision: bool,
    pub lsp: atomcode_capabilities::codeintel::LspSettings,
}

pub fn lsp_settings_from_config(
    config: &atomcode_config::config::LspConfig,
) -> atomcode_capabilities::codeintel::LspSettings {
    atomcode_capabilities::codeintel::LspSettings {
        enabled: config.enabled,
        auto_detect: config.auto_detect,
        settle_delay_ms: config.diagnostics_settle_delay_ms,
        servers: config
            .servers
            .iter()
            .map(|(extension, server)| {
                (
                    extension.clone(),
                    atomcode_capabilities::codeintel::LspServerSetting {
                        command: server.command.clone(),
                        args: server.args.clone(),
                        root_markers: server.root_markers.clone(),
                    },
                )
            })
            .collect(),
    }
}

pub fn credential_shell_policy_from_config(
    policy: atomcode_config::config::ShellGuardPolicy,
) -> atomcode_capabilities::tools::CredentialShellPolicy {
    match policy {
        atomcode_config::config::ShellGuardPolicy::Off => {
            atomcode_capabilities::tools::CredentialShellPolicy::Off
        }
        atomcode_config::config::ShellGuardPolicy::Prompt => {
            atomcode_capabilities::tools::CredentialShellPolicy::Prompt
        }
        atomcode_config::config::ShellGuardPolicy::Strict => {
            atomcode_capabilities::tools::CredentialShellPolicy::Strict
        }
    }
}

impl CodingRuntimeConfig {
    pub fn from_config(
        config: &atomcode_config::config::Config,
        working_dir: &std::path::Path,
        provider_override: Option<&str>,
        telemetry: Option<Arc<atomcode_telemetry::Telemetry>>,
        dangerously_skip_permissions: bool,
        interactive: bool,
    ) -> Self {
        // Resolve through the single boundary (design §14.1). The override is a
        // model-selection id (a legacy provider name still resolves via
        // projection); without one, the active `default_model`/`default_provider`
        // selection is used. Fall back to the first catalog model so a missing or
        // invalid selection still starts something — parity with the old
        // `providers.keys().min()` fallback. For a legacy config the resolved
        // `selection_id` equals the old provider key, so every field below is
        // byte-identical to the previous `providers.get(name)` extraction.
        let requested = provider_override
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| config.effective_model_selection().unwrap_or_default());
        let resolved = config.resolve_model(Some(&requested)).ok().or_else(|| {
            let mut ids: Vec<String> = config.logical_models().into_keys().collect();
            ids.sort();
            ids.into_iter()
                .find_map(|id| config.resolve_model(Some(&id)).ok())
        });
        let r = resolved.as_ref();
        Self {
            api_key: r.and_then(|r| r.api_key.clone()).unwrap_or_default(),
            base_url: r.and_then(|r| r.base_url.clone()).unwrap_or_default(),
            model: r.map(|r| r.model.clone()).unwrap_or_default(),
            supports_vision: r.map(|r| r.supports_vision).unwrap_or(false),
            preferred_language: Some(atomcode_config::i18n::resolve_initial_locale(
                None,
                config.language,
            )),
            todo: config.tools.todo.clone(),
            provider_name: r.map(|r| r.selection_id.clone()).unwrap_or_default(),
            working_dir: working_dir.to_path_buf(),
            context_window: r.map(|r| r.context_window as u32).unwrap_or(128_000),
            max_tokens: r.and_then(|r| r.max_tokens).map(|value| value as u32),
            mcp: true,
            telemetry,
            datalog: config.datalog.clone(),
            reasoning_history: r.and_then(|r| r.reasoning_history.clone()),
            reasoning_effort: r.and_then(|r| r.reasoning_effort.clone()),
            provider_type: r
                .map(|r| r.provider_type.clone())
                .unwrap_or_else(|| "openai".into()),
            thinking_enabled: r.and_then(|r| r.thinking_enabled),
            thinking_type: r.and_then(|r| r.thinking_type.clone()),
            thinking_keep: r.and_then(|r| r.thinking_keep.clone()),
            dangerously_skip_permissions,
            interactive,
            keep_interrupted_context: config.keep_interrupted_context,
            credential_shell_policy: credential_shell_policy_from_config(
                config.coding.shell_guard_policy,
            ),
            user_agent: r.and_then(|r| r.user_agent.clone()),
            skip_tls_verify: r.map(|r| r.skip_tls_verify).unwrap_or(false),
            loop_max_rounds: resolve_loop_max_rounds(
                config.loop_config.max_rounds,
                std::env::var("ATOMCODE_LOOP_MAX_ROUNDS").ok().as_deref(),
            ),
            turn_max_rounds: resolve_turn_max_rounds(
                config.coding.max_rounds,
                std::env::var("ATOMCODE_TURN_MAX_ROUNDS").ok().as_deref(),
            ),
            subagent_config: Some(Arc::new(config.clone())),
            // Default off; only the interactive TUI opts in (see the CLI's
            // TUI spawn sites and `event_loop::reload_runtime_provider_from`).
            round_cap_checkpoint: false,
            next_prompt_suggestions: false,
            lsp: lsp_settings_from_config(&config.lsp),
        }
    }

    pub fn agent_config(&self) -> CodingAgentConfig {
        let mut config = CodingAgentConfig::new(
            &self.api_key,
            &self.base_url,
            &self.model,
            &self.working_dir,
        );
        config.context_window = self.context_window;
        config.supports_vision = self.supports_vision;
        config.supports_reasoning_effort = self.reasoning_effort.is_some()
            || (atomcode_config::config::is_codingplan_provider_name(&self.provider_name)
                && self.model.eq_ignore_ascii_case("deepseek-v4-flash"));
        config.preferred_language = self.preferred_language;
        config.todo = self.todo.clone();
        config.provider_name = self.provider_name.clone();
        config.chat_options.max_tokens = self.max_tokens;
        config.telemetry = self.telemetry.clone();
        config.datalog = self.datalog.clone();
        config.reasoning_history = self.reasoning_history.clone();
        config.chat_options.reasoning_effort =
            atomcode_kernel::provider::ReasoningEffort::from_config(
                self.reasoning_effort.as_deref(),
            );
        config.provider_type = self.provider_type.clone();
        config.thinking_enabled = self.thinking_enabled;
        config.thinking_type = self.thinking_type.clone();
        config.thinking_keep = self.thinking_keep.clone();
        config.user_agent = self.user_agent.clone();
        config.skip_tls_verify = self.skip_tls_verify;
        config.loop_max_rounds = self.loop_max_rounds;
        config.max_rounds = self.turn_max_rounds;
        config.subagent_config = self.subagent_config.clone();
        if self.interactive {
            config.request_timeout = None;
        }
        config.keep_interrupted_context = self.keep_interrupted_context;
        config.credential_shell_policy = self.credential_shell_policy;
        config.round_cap_checkpoint = self.round_cap_checkpoint;
        config.next_prompt_suggestions = self.next_prompt_suggestions;
        config.lsp = self.lsp.clone();
        config
    }
}

pub fn apply_provider_config(
    config: &mut CodingAgentConfig,
    provider: &atomcode_config::config::provider::ProviderConfig,
) {
    config.model = provider.model.clone();
    config.supports_vision = provider.accepts_images();
    if let Some(base_url) = &provider.base_url {
        config.base_url = base_url.clone();
    }
    if let Some(api_key) = provider.resolved_api_key() {
        config.api_key = api_key;
    }
    config.context_window = provider.context_window as u32;
    config.chat_options.max_tokens = provider.max_tokens.map(|value| value as u32);
    config.chat_options.reasoning_effort = atomcode_kernel::provider::ReasoningEffort::from_config(
        provider.reasoning_effort.as_deref(),
    );
    config.supports_reasoning_effort = provider.reasoning_effort.is_some();
    config.provider_type = provider.provider_type.clone();
    config.reasoning_history = provider.reasoning_history.clone();
    config.thinking_enabled = provider.thinking_enabled;
    config.thinking_type = provider.thinking_type.clone();
    config.thinking_keep = provider.thinking_keep.clone();
    config.user_agent = provider.user_agent.clone();
    config.skip_tls_verify = provider.skip_tls_verify;
}

/// A thunk the runtime supplies that constructs a (gateway-signed) tier provider. `Some` on
/// success, `None` if construction failed (⇒ the tier falls back to the host provider).
pub type SubagentProvider =
    Arc<dyn Fn() -> Option<Arc<dyn atomcode_kernel::provider::LlmProvider>> + Send + Sync>;

/// A `task`-tier provider cell: lazily built and SWAP-AWARE. Holds a `thunk` (re-resolvable
/// on a `/model` swap) plus a lazily-populated build `cache`. `get()` builds on first use and
/// caches (keeps startup cheap — no reqwest client until the first `task`); `reset()` re-points
/// the thunk and drops the cache. Shared as an `Arc` between [`CodingAgentConfig`] and the
/// already-built TaskTool, so the runtime can update tier routing on a model swap in place.
struct TierInner {
    thunk: SubagentProvider,
    /// `None` = not built yet; `Some(inner)` = built exactly once (`inner == None` means the
    /// thunk yielded no provider — host-equal or a failed build — so we do NOT retry the build
    /// on every dispatch). One `Mutex` over both fields makes `get`/`reset` atomic and prevents
    /// a concurrent double-build.
    cache: Option<Option<Arc<dyn atomcode_kernel::provider::LlmProvider>>>,
    /// The parent conversation's `x-atomcode-session-id` (set once at assemble). Bound onto the
    /// tier provider when it's built so a `task` fan-out's children send the SAME session id as
    /// the main conversation — the AtomGit gateway then treats them as one window and permits
    /// their concurrent requests (GLM-5.2 rejects concurrent DISTINCT-session requests, which
    /// otherwise forces the strong-tier subtasks to run serially). Survives `reset` (a `/model`
    /// swap changes the tier model, not the conversation identity).
    session_id: Option<String>,
    usage_recorder: Option<atomcode_capabilities::session::DetachedUsageRecorder>,
}

pub struct TierProvider {
    inner: std::sync::Mutex<TierInner>,
}

impl TierProvider {
    pub fn new(thunk: SubagentProvider) -> Arc<Self> {
        Arc::new(Self {
            inner: std::sync::Mutex::new(TierInner {
                thunk,
                cache: None,
                session_id: None,
                usage_recorder: None,
            }),
        })
    }

    /// The built provider (built lazily on first call, then cached — success OR a `None`
    /// result is remembered, so a failing build isn't re-attempted every dispatch), or `None`
    /// if the thunk yields none (⇒ the caller falls back to the host slot). Lock poisoning
    /// cannot occur under the workspace `panic = "abort"` profile, so `unwrap` is unreachable.
    pub fn get(&self) -> Option<Arc<dyn atomcode_kernel::provider::LlmProvider>> {
        let mut g = self.inner.lock().unwrap();
        if let Some(cached) = &g.cache {
            return cached.clone();
        }
        let mut built = (g.thunk)();
        if let Some(recorder) = g.usage_recorder.clone() {
            if let Some(provider) = built.take() {
                built = Some(Arc::new(
                    atomcode_capabilities::session::UsageRecordingProvider::new(provider, recorder),
                ));
            }
        }
        // Bind the parent session id onto the freshly-built provider so subtask children carry
        // the main conversation's `x-atomcode-session-id` (one gateway window ⇒ concurrent OK).
        if let (Some(sid), Some(p)) = (&g.session_id, &built) {
            p.bind_session_id(sid);
        }
        g.cache = Some(built.clone());
        built
    }

    pub fn set_usage_recorder(
        &self,
        recorder: atomcode_capabilities::session::DetachedUsageRecorder,
    ) {
        let mut g = self.inner.lock().unwrap();
        g.usage_recorder = Some(recorder);
        // A model/provider reload may change attribution. Rebuild lazily so a
        // cached provider can never keep writing under the previous identity.
        g.cache = None;
    }

    /// Record the parent conversation's session id, to be bound onto the tier provider when
    /// built (see [`TierInner::session_id`]). Set once at assemble, BEFORE the first `get()`; if
    /// a provider is somehow already cached, bind immediately too (idempotent — the adapter's
    /// `bind_session_id` is a one-shot `OnceLock`).
    pub fn set_session_id(&self, session_id: &str) {
        let mut g = self.inner.lock().unwrap();
        g.session_id = Some(session_id.to_string());
        if let Some(Some(p)) = &g.cache {
            p.bind_session_id(session_id);
        }
    }

    /// Re-point at a freshly-resolved thunk and drop the cache — the next `get()` rebuilds.
    /// Called by the runtime on a `/model` swap so tier routing re-resolves against the new host.
    /// The recorded `session_id` PERSISTS (a model swap changes the tier model, not the
    /// conversation), so the rebuilt provider is re-bound to the same window on the next `get()`.
    pub fn reset(&self, thunk: SubagentProvider) {
        let mut g = self.inner.lock().unwrap();
        g.thunk = thunk;
        g.cache = None;
    }
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
/// Share of the CodingPlan 5h rolling `call_limit` a single `/goal` may consume
/// (percent). A goal that eats more than this starves the user's interactive work
/// and other controllers within the same rolling window.
const GOAL_ROUND_SHARE_PERCENT: i64 = 30;
/// Floor so a micro plan still yields a usable goal budget.
const GOAL_ROUND_FLOOR: u32 = 50;
/// Fallback when there is no CodingPlan `call_limit` to derive from
/// (non-CodingPlan provider, offline, pre-login) and no explicit env override.
const GOAL_ROUND_FALLBACK: u32 = 300;

/// Explicit `ATOMCODE_GOAL_MAX_ROUNDS` override, if set and parseable.
pub fn goal_max_rounds_env() -> Option<u32> {
    std::env::var("ATOMCODE_GOAL_MAX_ROUNDS")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
}

/// Resolve the `/goal` round cap. Precedence: explicit env override → a share of
/// the CodingPlan binding-window `call_limit` (Pro 1000 → 300, Lite 800 → 240) →
/// a flat fallback. Pure so the host can call it once `call_limit` is known
/// without threading config plumbing.
pub fn derive_goal_max_rounds(env_override: Option<u32>, call_limit: Option<i64>) -> u32 {
    if let Some(explicit) = env_override {
        return explicit;
    }
    match call_limit {
        Some(limit) if limit > 0 => {
            u32::try_from(limit.saturating_mul(GOAL_ROUND_SHARE_PERCENT) / 100)
                .unwrap_or(GOAL_ROUND_FALLBACK)
                .max(GOAL_ROUND_FLOOR)
        }
        _ => GOAL_ROUND_FALLBACK,
    }
}

fn default_goal_max_rounds() -> u32 {
    // Construction happens before CodingPlan `call_limit` is known; the host
    // re-derives with the real limit after login via `derive_goal_max_rounds`.
    derive_goal_max_rounds(goal_max_rounds_env(), None)
}
fn default_turn_max_rounds() -> u32 {
    std::env::var("ATOMCODE_TURN_MAX_ROUNDS")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(200)
}

fn default_tool_loop_policy() -> Option<ToolLoopPolicy> {
    resolve_tool_loop_policy(
        std::env::var("ATOMCODE_TOOL_LOOP_WARNING_THRESHOLD")
            .ok()
            .as_deref(),
        std::env::var("ATOMCODE_TOOL_LOOP_STOP_THRESHOLD")
            .ok()
            .as_deref(),
    )
}

fn resolve_tool_loop_policy(
    warning_env: Option<&str>,
    stop_env: Option<&str>,
) -> Option<ToolLoopPolicy> {
    let requested_stop = stop_env.and_then(|value| value.trim().parse::<u32>().ok());
    if requested_stop == Some(0) {
        return None;
    }
    // Values below 3 cannot satisfy the public policy invariant (warning >= 2
    // and warning < stop), so malformed/unsafe external input retains the shipped
    // 3/4 policy instead of panicking or silently disabling protection.
    let stop = requested_stop.filter(|value| *value >= 3).unwrap_or(4);
    let fallback_warning = 3.min(stop - 1).max(2);
    let warning = warning_env
        .and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|value| *value >= 2 && *value < stop)
        .unwrap_or(fallback_warning);
    Some(
        ToolLoopPolicy::new(warning, stop)
            .expect("resolved tool-loop thresholds satisfy the policy invariant"),
    )
}
fn default_goal_max_duration_secs() -> u64 {
    // Wall-clock is a poor bound for an autonomous goal: it kills slow-but-productive
    // work and lets fast runaways burn a full window well inside the limit, and it is
    // only checked between rounds so a single long round sails past it. Default OFF
    // (0 = disabled); the goal is bounded by the round cap + evaluator. Re-enable
    // explicitly via ATOMCODE_GOAL_MAX_DURATION_SECS if a hard time cap is ever wanted.
    std::env::var("ATOMCODE_GOAL_MAX_DURATION_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}
fn default_loop_max_rounds() -> u32 {
    resolve_loop_max_rounds(
        100,
        std::env::var("ATOMCODE_LOOP_MAX_ROUNDS").ok().as_deref(),
    )
}

/// Resolve the product-level `/loop` round high-water mark.
///
/// Drivers with their own loop controller must use this resolver too so the
/// `ATOMCODE_LOOP_MAX_ROUNDS` override, including `0 = unbounded`, has one
/// meaning across runtime-owned and driver-owned loop modes.
pub fn resolve_loop_max_rounds(configured: u32, env: Option<&str>) -> u32 {
    env.and_then(|value| value.trim().parse::<u32>().ok())
        .unwrap_or(configured)
}

/// Resolve the per-turn round cap.
///
/// Env `ATOMCODE_TURN_MAX_ROUNDS` (if a valid u32) takes priority over the
/// TOML `[coding] max_rounds` value. `0` is preserved (means unbounded).
/// Non-parseable env values fall back to the TOML-configured value.
/// Same shape as `resolve_loop_max_rounds`.
pub fn resolve_turn_max_rounds(configured: u32, env: Option<&str>) -> u32 {
    env.and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(configured)
}

impl CodingAgentConfig {
    /// Construct with the required fields and sane defaults for the rest.
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        working_dir: impl Into<PathBuf>,
    ) -> Self {
        let model = model.into();
        Self {
            api_key: api_key.into(),
            base_url: base_url.into(),
            provider_name: model.clone(),
            supports_vision: atomcode_capabilities::provider::model_suggests_vision(&model),
            supports_reasoning_effort: false,
            model,
            preferred_language: None,
            todo: Default::default(),
            working_dir: working_dir.into(),
            context_window: 128_000,
            stream_timeout: default_stream_timeout(),
            request_timeout: Some(Duration::from_secs(300)),
            max_continuations: 50,
            max_rounds: default_turn_max_rounds(),
            round_cap_checkpoint: false,
            next_prompt_suggestions: false,
            tool_loop_policy: default_tool_loop_policy(),
            goal_max_rounds: default_goal_max_rounds(),
            goal_max_duration_secs: default_goal_max_duration_secs(),
            loop_max_rounds: default_loop_max_rounds(),
            chat_options: Default::default(),
            telemetry: None,
            datalog: atomcode_config::config::DatalogConfig::default(),
            reasoning_history: None,
            provider_type: "openai".into(),
            thinking_enabled: None,
            thinking_type: None,
            thinking_keep: None,
            compact_threshold: 0.7,
            web_search_provider: None,
            lsp: Default::default(),
            keep_interrupted_context: false,
            credential_shell_policy: Default::default(),
            user_agent: None,
            skip_tls_verify: false,
            subagent_config: None,
            subagent_fast_provider: None,
            subagent_capable_provider: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_guard_policy_maps_at_the_coding_boundary() {
        assert_eq!(
            credential_shell_policy_from_config(atomcode_config::config::ShellGuardPolicy::Off),
            atomcode_capabilities::tools::CredentialShellPolicy::Off
        );
        assert_eq!(
            credential_shell_policy_from_config(atomcode_config::config::ShellGuardPolicy::Prompt),
            atomcode_capabilities::tools::CredentialShellPolicy::Prompt
        );
        assert_eq!(
            credential_shell_policy_from_config(atomcode_config::config::ShellGuardPolicy::Strict),
            atomcode_capabilities::tools::CredentialShellPolicy::Strict
        );
    }

    #[test]
    fn round_caps_have_generous_defaults() {
        let c = CodingAgentConfig::new("k", "https://x/v1", "m", "/tmp");
        assert_eq!(c.max_rounds, 200);
        // No CodingPlan info at construction → the non-CodingPlan fallback.
        assert_eq!(c.goal_max_rounds, 300);
        // The wall-clock cap is OFF by default (0 = disabled); the goal is bounded
        // by the round cap + evaluator instead. Re-enable via env if ever needed.
        assert_eq!(c.goal_max_duration_secs, 0);
    }

    #[test]
    fn lsp_config_maps_to_runtime_and_agent_without_becoming_default_on() {
        let mut config = atomcode_config::config::Config::default();
        assert!(!config.lsp.enabled);
        config.lsp.enabled = true;
        config.lsp.auto_detect = false;
        config.lsp.diagnostics_settle_delay_ms = 725;
        config.lsp.servers.insert(
            ".rs".into(),
            atomcode_config::lsp_registry::LspServerConfig {
                command: "custom-ra".into(),
                args: vec!["--stdio".into()],
                root_markers: vec!["Cargo.toml".into()],
            },
        );
        let runtime = CodingRuntimeConfig::from_config(
            &config,
            std::path::Path::new("/workspace"),
            None,
            None,
            false,
            false,
        );
        assert!(runtime.lsp.enabled);
        assert!(!runtime.lsp.auto_detect);
        assert_eq!(runtime.lsp.settle_delay_ms, 725);
        let agent = runtime.agent_config();
        let rust = agent.lsp.servers.get(".rs").unwrap();
        assert_eq!(rust.command, "custom-ra");
        assert_eq!(rust.args, vec!["--stdio"]);

        let defaults = CodingAgentConfig::new("", "", "", "/workspace");
        assert!(!defaults.lsp.enabled);
    }

    #[test]
    fn derive_goal_rounds_scales_with_plan_call_limit() {
        // 30% of the binding 5h window's call_limit. Pro=1000 → 300, Lite=800 → 240.
        assert_eq!(derive_goal_max_rounds(None, Some(1000)), 300);
        assert_eq!(derive_goal_max_rounds(None, Some(800)), 240);
    }

    #[test]
    fn derive_goal_rounds_env_override_wins_over_plan() {
        // An explicit ATOMCODE_GOAL_MAX_ROUNDS is the user's word — it beats the
        // plan-derived value regardless of call_limit.
        assert_eq!(derive_goal_max_rounds(Some(150), Some(1000)), 150);
        assert_eq!(derive_goal_max_rounds(Some(1), None), 1);
    }

    #[test]
    fn derive_goal_rounds_falls_back_without_plan() {
        // Non-CodingPlan / offline / unknown call_limit → flat fallback, never a
        // hardcoded 200 tied to one plan tier.
        assert_eq!(derive_goal_max_rounds(None, None), 300);
        assert_eq!(derive_goal_max_rounds(None, Some(0)), 300);
        assert_eq!(derive_goal_max_rounds(None, Some(-5)), 300);
    }

    #[test]
    fn derive_goal_rounds_floors_tiny_plans() {
        // A micro window (30% = 30) must still leave a usable goal budget.
        assert_eq!(derive_goal_max_rounds(None, Some(100)), 50);
    }

    // Fix #4: saturating_mul prevents a debug-panic on adversarial i64::MAX
    // call_limit (plain * would overflow in debug builds). After saturation the
    // i64→u32 try_from fails and falls back to GOAL_ROUND_FALLBACK (300).
    #[test]
    fn derive_goal_rounds_saturating_mul_no_panic_on_i64_max() {
        // Must not panic in debug builds, and must return GOAL_ROUND_FALLBACK.
        let result = derive_goal_max_rounds(None, Some(i64::MAX));
        assert_eq!(
            result, 300,
            "i64::MAX call_limit must fall back to GOAL_ROUND_FALLBACK (300)"
        );
    }

    #[test]
    fn runtime_config_passes_preferred_language_to_agent() {
        let mut source = atomcode_config::config::Config::default();
        source.language = Some(Locale::ZhCn);
        let runtime = CodingRuntimeConfig::from_config(
            &source,
            std::path::Path::new("/tmp"),
            None,
            None,
            false,
            true,
        );

        assert_eq!(runtime.preferred_language, Some(Locale::ZhCn));
        assert_eq!(
            runtime.agent_config().preferred_language,
            Some(Locale::ZhCn)
        );
    }

    #[test]
    fn runtime_config_passes_datalog_settings_to_agent() {
        let mut source = atomcode_config::config::Config::default();
        source.datalog = atomcode_config::config::DatalogConfig {
            enabled: false,
            dir: Some("/var/tmp/atomcode-datalog".into()),
        };
        let runtime = CodingRuntimeConfig::from_config(
            &source,
            std::path::Path::new("/tmp"),
            None,
            None,
            false,
            true,
        );

        assert!(!runtime.datalog.enabled);
        assert_eq!(
            runtime.agent_config().datalog.dir.as_deref(),
            Some("/var/tmp/atomcode-datalog")
        );
    }

    #[test]
    fn runtime_config_passes_todo_policy_to_agent() {
        let mut source = atomcode_config::config::Config::default();
        source.tools.todo.enabled = false;
        source.tools.todo.eager = atomcode_config::config::TodoEagerness::Always;
        let runtime = CodingRuntimeConfig::from_config(
            &source,
            std::path::Path::new("/tmp"),
            None,
            None,
            false,
            true,
        );

        assert!(!runtime.todo.enabled);
        let agent = runtime.agent_config();
        assert!(!agent.todo.enabled);
        assert_eq!(
            agent.todo.eager,
            atomcode_config::config::TodoEagerness::Always
        );
    }

    #[test]
    fn from_config_resolves_a_legacy_provider_unchanged() {
        let source: atomcode_config::config::Config = serde_json::from_value(serde_json::json!({
            "default_provider": "MyDS",
            "providers": {
                "MyDS": {
                    "type": "openai",
                    "base_url": "https://api.deepseek.com/v1",
                    "api_key": "sk-legacy",
                    "model": "deepseek-chat",
                    "context_window": 128000
                }
            }
        }))
        .unwrap();
        let rt = CodingRuntimeConfig::from_config(
            &source,
            std::path::Path::new("/tmp"),
            None,
            None,
            false,
            true,
        );
        assert_eq!(rt.provider_name, "MyDS");
        assert_eq!(rt.base_url, "https://api.deepseek.com/v1");
        assert_eq!(rt.api_key, "sk-legacy");
        assert_eq!(rt.model, "deepseek-chat");
        assert_eq!(rt.context_window, 128000);
        assert_eq!(rt.provider_type, "openai");
    }

    #[test]
    fn from_config_builds_a_new_schema_model_profile() {
        // One account, and a model profile selected by its `<account>/<model>` id
        // — the "one provider, multiple models" capability, resolved at the
        // runtime build seam without any legacy `[providers.*]`.
        let source: atomcode_config::config::Config = serde_json::from_value(serde_json::json!({
            "default_model": "acc/coder",
            "provider_accounts": { "acc": { "provider": "deepseek", "api_key": "sk-acc" } },
            "models": {
                "acc/coder": { "account": "acc", "model": "deepseek-coder", "context_window": 131072 },
                "acc/chat": { "account": "acc", "model": "deepseek-chat", "context_window": 131072 }
            }
        }))
        .unwrap();
        // Default selection (acc/coder).
        let rt = CodingRuntimeConfig::from_config(
            &source,
            std::path::Path::new("/tmp"),
            None,
            None,
            false,
            true,
        );
        assert_eq!(rt.provider_name, "acc/coder");
        assert_eq!(rt.model, "deepseek-coder");
        assert_eq!(rt.base_url, "https://api.deepseek.com/v1"); // preset default
        assert_eq!(rt.api_key, "sk-acc"); // shared account credential
        assert_eq!(rt.context_window, 131072);
        // The second model on the SAME account, selected by id — no duplicated
        // connection settings.
        let rt2 = CodingRuntimeConfig::from_config(
            &source,
            std::path::Path::new("/tmp"),
            Some("acc/chat"),
            None,
            false,
            true,
        );
        assert_eq!(rt2.model, "deepseek-chat");
        assert_eq!(rt2.api_key, "sk-acc");
        assert_eq!(rt2.base_url, "https://api.deepseek.com/v1");
    }

    #[test]
    fn explicit_vision_override_reaches_runtime_agent_config() {
        let source: atomcode_config::config::Config = serde_json::from_value(serde_json::json!({
            "default_model": "custom/qwen",
            "provider_accounts": {
                "custom": {
                    "provider": "openai",
                    "api_key": "sk-custom",
                    "base_url": "https://example.invalid/v1"
                }
            },
            "models": {
                "custom/qwen": {
                    "account": "custom",
                    "model": "qwen3.8max",
                    "supports_vision": true
                }
            }
        }))
        .unwrap();

        let runtime = CodingRuntimeConfig::from_config(
            &source,
            std::path::Path::new("/tmp"),
            None,
            None,
            false,
            true,
        );

        assert!(runtime.supports_vision);
        assert!(runtime.agent_config().supports_vision);
    }

    #[test]
    fn turn_max_rounds_env_overrides_toml() {
        assert_eq!(resolve_turn_max_rounds(200, Some("500")), 500);
        assert_eq!(resolve_turn_max_rounds(200, Some("0")), 0); // 0 关闭保留
        assert_eq!(resolve_turn_max_rounds(300, Some("bad")), 300); // 非法回退 TOML
        assert_eq!(resolve_turn_max_rounds(300, None), 300);
    }

    #[test]
    fn loop_round_env_override_wins_over_toml_and_preserves_zero() {
        assert_eq!(resolve_loop_max_rounds(100, Some("250")), 250);
        assert_eq!(resolve_loop_max_rounds(100, Some("0")), 0);
        assert_eq!(resolve_loop_max_rounds(80, Some("invalid")), 80);
        assert_eq!(resolve_loop_max_rounds(80, None), 80);
    }

    #[test]
    fn tool_loop_env_policy_is_validated_and_can_be_disabled() {
        let policy = resolve_tool_loop_policy(Some("10"), Some("12")).unwrap();
        assert_eq!(policy.warning_threshold(), 10);
        assert_eq!(policy.stop_threshold(), 12);
        assert!(resolve_tool_loop_policy(Some("10"), Some("0")).is_none());

        let fallback = resolve_tool_loop_policy(Some("99"), Some("4")).unwrap();
        assert_eq!(fallback.warning_threshold(), 3);
        assert_eq!(fallback.stop_threshold(), 4);
    }

    #[test]
    fn coding_cfg_new_defaults_subagent_providers_none() {
        let c = CodingAgentConfig::new("k", "https://api.example.com/v1", "m", "/tmp");
        assert!(c.subagent_fast_provider.is_none());
        assert!(c.subagent_capable_provider.is_none());
    }

    #[test]
    fn tier_provider_builds_once_then_reset_rebuilds() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct StubP(&'static str);
        #[async_trait::async_trait]
        impl atomcode_kernel::provider::LlmProvider for StubP {
            fn model_name(&self) -> &str {
                self.0
            }
            async fn chat_stream(
                &self,
                _m: &[atomcode_kernel::message::Message],
                _t: &[atomcode_kernel::tool::ToolDef],
                _o: &atomcode_kernel::provider::ChatOptions,
            ) -> Result<
                futures::stream::BoxStream<'static, atomcode_kernel::stream::StreamEvent>,
                atomcode_kernel::stream::ProviderError,
            > {
                unreachable!("not called in this test")
            }
        }

        let builds = Arc::new(AtomicUsize::new(0));
        let mk = |name: &'static str, builds: Arc<AtomicUsize>| -> SubagentProvider {
            Arc::new(move || {
                builds.fetch_add(1, Ordering::SeqCst);
                Some(Arc::new(StubP(name)) as Arc<dyn atomcode_kernel::provider::LlmProvider>)
            })
        };

        let cell = TierProvider::new(mk("deepseek", builds.clone()));
        // Lazy + cached: two gets, one build.
        assert_eq!(cell.get().unwrap().model_name(), "deepseek");
        assert_eq!(cell.get().unwrap().model_name(), "deepseek");
        assert_eq!(builds.load(Ordering::SeqCst), 1, "built once, then cached");

        // A /model swap resets the cell: new thunk + dropped cache → next get rebuilds.
        cell.reset(mk("glm", builds.clone()));
        assert_eq!(cell.get().unwrap().model_name(), "glm");
        assert_eq!(
            builds.load(Ordering::SeqCst),
            2,
            "reset forces a rebuild with the new model"
        );
    }

    #[test]
    fn tier_provider_caches_none_result_no_retry() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        // A thunk that yields no provider (host-equal or a failed build) must be called ONCE,
        // then its `None` is remembered — not re-attempted (which would re-run build_provider)
        // every dispatch.
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let thunk: SubagentProvider = Arc::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
            None
        });
        let cell = TierProvider::new(thunk);
        assert!(cell.get().is_none());
        assert!(cell.get().is_none());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "None result must be cached, thunk called once"
        );
    }

    #[test]
    fn tier_provider_binds_parent_session_id_on_build() {
        use std::sync::Mutex;
        // A provider that records what session id it was bound with.
        struct RecP(Arc<Mutex<Option<String>>>);
        #[async_trait::async_trait]
        impl atomcode_kernel::provider::LlmProvider for RecP {
            fn model_name(&self) -> &str {
                "rec"
            }
            fn bind_session_id(&self, id: &str) {
                *self.0.lock().unwrap() = Some(id.to_string());
            }
            async fn chat_stream(
                &self,
                _m: &[atomcode_kernel::message::Message],
                _t: &[atomcode_kernel::tool::ToolDef],
                _o: &atomcode_kernel::provider::ChatOptions,
            ) -> Result<
                futures::stream::BoxStream<'static, atomcode_kernel::stream::StreamEvent>,
                atomcode_kernel::stream::ProviderError,
            > {
                unreachable!("not called in this test")
            }
        }
        let bound = Arc::new(Mutex::new(None));
        let b2 = bound.clone();
        let thunk: SubagentProvider = Arc::new(move || {
            Some(Arc::new(RecP(b2.clone())) as Arc<dyn atomcode_kernel::provider::LlmProvider>)
        });
        let cell = TierProvider::new(thunk);
        // Set the parent session id BEFORE the first build (as `assemble` does).
        cell.set_session_id("parent-sess-123");
        let _ = cell.get(); // first get builds the provider → binds the id
        assert_eq!(
            bound.lock().unwrap().as_deref(),
            Some("parent-sess-123"),
            "the tier provider must bind the parent session id when built"
        );
    }
}

// Manual Debug: `atomcode_telemetry::Telemetry` is not `Debug`. Skip it; redact the
// api_key while we're here.
impl std::fmt::Debug for CodingAgentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodingAgentConfig")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("provider_name", &self.provider_name)
            .field("working_dir", &self.working_dir)
            .field("context_window", &self.context_window)
            .field("stream_timeout", &self.stream_timeout)
            .field("request_timeout", &self.request_timeout)
            .field("max_continuations", &self.max_continuations)
            .field("max_rounds", &self.max_rounds)
            .field("tool_loop_policy", &self.tool_loop_policy)
            .field("goal_max_rounds", &self.goal_max_rounds)
            .field("goal_max_duration_secs", &self.goal_max_duration_secs)
            .field("chat_options", &self.chat_options)
            .field("telemetry", &self.telemetry.is_some())
            .finish_non_exhaustive()
    }
}
