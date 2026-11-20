use std::sync::Arc;

use atomcode_capabilities::provider::{
    atomgit_request_signer, is_atomgit_gateway, signer_available, AnthropicConfig,
    AnthropicProvider, OllamaConfig, OllamaProvider, OpenAiCompatConfig, OpenAiCompatProvider,
    ReasoningPolicy, RequestSigner, ResponsesProvider, RetryPolicy,
};
use atomcode_kernel::provider::{ChatOptions, LlmProvider, ReasoningEffort};
use tracing::Instrument;

use crate::CodingAgentConfig;
use crate::{SubagentModelProviders, SubagentModelResolver, SubagentProvider, TierProvider};

#[derive(Debug)]
pub enum ProviderBuildError {
    Adapter(String),
    Authentication(String),
    SourceBuildGatewayUnsupported { base_url: String },
}

impl std::fmt::Display for ProviderBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Adapter(message) | Self::Authentication(message) => f.write_str(message),
            Self::SourceBuildGatewayUnsupported { base_url } => write!(
                f,
                "gateway authentication is unavailable in this build: {base_url}"
            ),
        }
    }
}

impl std::error::Error for ProviderBuildError {}

/// Host seam for endpoint-specific request authentication. Returning `None` means the endpoint
/// uses the configured static API key. The implementation owns gateway identification as well as
/// signer construction, keeping auth and stored-credential access out of the coding layer.
pub trait ProviderAuthenticator: Send + Sync {
    /// `dirs` is the runtime's own — the user tree a stored login is read from.
    fn request_signer(
        &self,
        base_url: &str,
        dirs: &atomcode_capabilities::ProductDirs,
    ) -> Result<Option<Arc<dyn RequestSigner>>, ProviderBuildError>;
}

pub struct AtomGitProviderAuthenticator;

impl ProviderAuthenticator for AtomGitProviderAuthenticator {
    fn request_signer(
        &self,
        base_url: &str,
        dirs: &atomcode_capabilities::ProductDirs,
    ) -> Result<Option<Arc<dyn RequestSigner>>, ProviderBuildError> {
        if !is_atomgit_gateway(base_url) {
            return Ok(None);
        }
        if !signer_available() {
            return Err(ProviderBuildError::SourceBuildGatewayUnsupported {
                base_url: base_url.to_string(),
            });
        }
        atomgit_request_signer(base_url, dirs.user())
            .map(Some)
            .map_err(ProviderBuildError::Authentication)
    }
}

pub fn atomgit_provider_factory(
    default_user_agent: impl Into<String>,
) -> Arc<dyn CodingProviderFactory> {
    Arc::new(
        DefaultCodingProviderFactory::new(default_user_agent)
            .with_authenticator(Arc::new(AtomGitProviderAuthenticator)),
    )
}

pub trait CodingProviderFactory: Send + Sync {
    fn build(
        &self,
        config: &CodingAgentConfig,
        session_id: Option<&str>,
    ) -> Result<Arc<dyn LlmProvider>, ProviderBuildError>;
}

#[derive(Clone)]
pub struct DefaultCodingProviderFactory {
    default_user_agent: String,
    authenticator: Option<Arc<dyn ProviderAuthenticator>>,
}

impl DefaultCodingProviderFactory {
    pub fn new(default_user_agent: impl Into<String>) -> Self {
        Self {
            default_user_agent: default_user_agent.into(),
            authenticator: None,
        }
    }

    pub fn with_authenticator(mut self, authenticator: Arc<dyn ProviderAuthenticator>) -> Self {
        self.authenticator = Some(authenticator);
        self
    }
}

/// Immutable defaults of THIS model selection, shared by main and direct side
/// calls. The runtime still owns provider rebuilding; adapters still own wire
/// capability checks and rejection latches. Explicit per-call values win.
struct ModelDefaultsProvider {
    inner: Arc<dyn LlmProvider>,
    reasoning_effort: Option<ReasoningEffort>,
}

#[async_trait::async_trait]
impl LlmProvider for ModelDefaultsProvider {
    fn model_name(&self) -> &str {
        self.inner.model_name()
    }
    fn context_window(&self) -> u32 {
        self.inner.context_window()
    }
    fn supports_vision(&self) -> bool {
        self.inner.supports_vision()
    }
    fn effort_levels(&self) -> Vec<String> {
        self.inner.effort_levels()
    }
    fn replays_reasoning(&self) -> bool {
        self.inner.replays_reasoning()
    }
    fn bind_session_id(&self, id: &str) {
        self.inner.bind_session_id(id);
    }

    async fn chat_stream(
        &self,
        messages: &[atomcode_kernel::message::Message],
        tools: &[atomcode_kernel::tool::ToolDef],
        options: &ChatOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, atomcode_kernel::stream::StreamEvent>,
        atomcode_kernel::stream::ProviderError,
    > {
        let mut resolved = options.clone();
        resolved.reasoning_effort = options.reasoning_effort.or(self.reasoning_effort);
        let source = match options.reasoning_effort {
            Some(value) if Some(value) != self.reasoning_effort => "call_override",
            // Main middleware may already have filled the same model default;
            // do not label an equal value as a deliberate downgrade/override.
            Some(_) => "request_value",
            None if self.reasoning_effort.is_some() => "model_default",
            None => "api_default",
        };
        let span = tracing::info_span!(
            "model_request_options",
            effort_source = source,
            requested_effort = options
                .reasoning_effort
                .map(ReasoningEffort::as_str)
                .unwrap_or("omitted"),
            model_default_effort = self
                .reasoning_effort
                .map(ReasoningEffort::as_str)
                .unwrap_or("omitted"),
            resolved_effort = resolved
                .reasoning_effort
                .map(ReasoningEffort::as_str)
                .unwrap_or("omitted")
        );
        self.inner
            .chat_stream(messages, tools, &resolved)
            .instrument(span)
            .await
    }
}

/// Model/provider overrides win over the process-wide fallback. Never disable
/// the deadline with zero or an invalid environment value.
fn response_header_timeout(cfg: &CodingAgentConfig) -> std::time::Duration {
    resolve_response_header_timeout(
        cfg.response_header_timeout_secs,
        std::env::var("ATOMCODE_RESPONSE_HEADER_TIMEOUT_SECS")
            .ok()
            .as_deref(),
    )
}
fn resolve_response_header_timeout(
    configured: Option<std::num::NonZeroU64>,
    env: Option<&str>,
) -> std::time::Duration {
    std::time::Duration::from_secs(
        configured
            .map(|n| n.get())
            .or_else(|| {
                env.and_then(|s| s.trim().parse::<u64>().ok())
                    .filter(|n| *n > 0)
            })
            .unwrap_or(300),
    )
}

impl CodingProviderFactory for DefaultCodingProviderFactory {
    fn build(
        &self,
        cfg: &CodingAgentConfig,
        session_id: Option<&str>,
    ) -> Result<Arc<dyn LlmProvider>, ProviderBuildError> {
        let ua = cfg
            .user_agent
            .clone()
            .unwrap_or_else(|| self.default_user_agent.clone());
        // Direct provider calls may omit ChatOptions::max_tokens. They must
        // inherit the selected model's budget too, just like the main loop.
        let max_tokens = cfg
            .chat_options
            .max_tokens
            .unwrap_or_else(|| default_max_tokens(cfg.context_window));
        let provider: Arc<dyn LlmProvider> = match cfg.provider_type.as_str() {
            "claude" | "anthropic" | "deepseek-messages" => {
                let mut ac = AnthropicConfig::new(&cfg.api_key, &cfg.base_url, &cfg.model);
                ac.deepseek_messages = cfg.provider_type == "deepseek-messages";
                ac.wire_dump_dir = Some(cfg.dirs.user().join("wire-dump"));
                ac.context_window = cfg.context_window;
                ac.idle_timeout = cfg.stream_timeout;
                ac.first_token_timeout = cfg.first_token_timeout;
                ac.open_timeout = response_header_timeout(cfg);
                ac.max_tokens = max_tokens;
                ac.supports_vision = cfg.supports_vision;
                ac.effort_levels = cfg.effort_levels.clone();
                ac.thinking = cfg.thinking_enabled.unwrap_or(false);
                ac.user_agent = Some(ua.clone());
                ac.headers = cfg.headers.clone();
                ac.skip_tls_verify = cfg.skip_tls_verify;
                ac.retry = retry_policy_for(cfg.retry_max_attempts)?;
                Arc::new(
                    AnthropicProvider::new(ac)
                        .map_err(|e| ProviderBuildError::Adapter(e.message))?,
                )
            }
            "ollama" => {
                let mut oc = OllamaConfig::new(&cfg.base_url, &cfg.model);
                oc.wire_dump_dir = Some(cfg.dirs.user().join("wire-dump"));
                oc.api_key = cfg.api_key.clone();
                oc.context_window = cfg.context_window;
                oc.idle_timeout = cfg.stream_timeout;
                oc.first_token_timeout = cfg.first_token_timeout;
                oc.max_tokens = Some(max_tokens);
                // Explicit opt-in: pin the Ollama runtime window only when the user set
                // ATOMCODE_OLLAMA_NUM_CTX. Unset ⇒ leave the daemon's own default alone.
                oc.num_ctx =
                    parse_ollama_num_ctx(std::env::var("ATOMCODE_OLLAMA_NUM_CTX").ok().as_deref());
                oc.supports_vision = cfg.supports_vision;
                oc.effort_levels = cfg.effort_levels.clone();
                oc.think = cfg.thinking_enabled.unwrap_or(false);
                oc.user_agent = Some(ua.clone());
                oc.headers = cfg.headers.clone();
                oc.skip_tls_verify = cfg.skip_tls_verify;
                oc.retry = retry_policy_for(cfg.retry_max_attempts)?;
                Arc::new(
                    OllamaProvider::new(oc).map_err(|e| ProviderBuildError::Adapter(e.message))?,
                )
            }
            "responses" => {
                // OpenAI Responses API wire (`/responses`). Shares the
                // OpenAiCompatConfig shape (same auth/timeouts/signer fields);
                // only the URL path + codecs differ.
                let mut pc = OpenAiCompatConfig::new(&cfg.api_key, &cfg.base_url, &cfg.model);
                pc.wire_dump_dir = Some(cfg.dirs.user().join("wire-dump"));
                pc.context_window = cfg.context_window;
                pc.idle_timeout = cfg.stream_timeout;
                pc.first_token_timeout = cfg.first_token_timeout;
                pc.open_timeout = response_header_timeout(cfg);
                pc.supports_vision = cfg.supports_vision;
                pc.effort_levels = cfg.effort_levels.clone();
                pc.max_tokens = Some(max_tokens);
                pc.supports_reasoning_effort = supports_reasoning_effort(cfg);
                pc.reasoning_policy =
                    ReasoningPolicy::from_config(cfg.reasoning_history.as_deref())
                        .map_err(ProviderBuildError::Adapter)?;
                pc.user_agent = Some(ua);
                pc.headers = cfg.headers.clone();
                pc.skip_tls_verify = cfg.skip_tls_verify;
                pc.retry = retry_policy_for(cfg.retry_max_attempts)?;
                if let Some(authenticator) = &self.authenticator {
                    pc.request_signer = authenticator.request_signer(&cfg.base_url, &cfg.dirs)?;
                    if pc.request_signer.is_some() && !pc.headers.is_empty() {
                        return Err(ProviderBuildError::Authentication(
                            "Managed signed accounts do not support custom HTTP headers".into(),
                        ));
                    }
                }
                Arc::new(
                    ResponsesProvider::new(pc)
                        .map_err(|e| ProviderBuildError::Adapter(e.message))?,
                )
            }
            _ => {
                let mut pc = OpenAiCompatConfig::new(&cfg.api_key, &cfg.base_url, &cfg.model);
                pc.wire_dump_dir = Some(cfg.dirs.user().join("wire-dump"));
                pc.context_window = cfg.context_window;
                pc.idle_timeout = cfg.stream_timeout;
                pc.first_token_timeout = cfg.first_token_timeout;
                pc.open_timeout = response_header_timeout(cfg);
                pc.supports_vision = cfg.supports_vision;
                pc.effort_levels = cfg.effort_levels.clone();
                pc.max_tokens = Some(max_tokens);
                // An explicit per-model default is also an explicit capability
                // declaration. CodingPlan's DeepSeek V4 Flash predates server-side
                // capability metadata, so keep that one exact built-in fallback.
                pc.supports_reasoning_effort = supports_reasoning_effort(cfg);
                pc.reasoning_policy =
                    ReasoningPolicy::from_config(cfg.reasoning_history.as_deref())
                        .map_err(ProviderBuildError::Adapter)?;
                pc.thinking_type = cfg.thinking_type.clone();
                pc.thinking_keep = cfg.thinking_keep.clone();
                pc.user_agent = Some(ua);
                pc.headers = cfg.headers.clone();
                pc.skip_tls_verify = cfg.skip_tls_verify;
                pc.retry = retry_policy_for(cfg.retry_max_attempts)?;
                if let Some(authenticator) = &self.authenticator {
                    pc.request_signer = authenticator.request_signer(&cfg.base_url, &cfg.dirs)?;
                    if pc.request_signer.is_some() && !pc.headers.is_empty() {
                        return Err(ProviderBuildError::Authentication(
                            "Managed signed accounts do not support custom HTTP headers".into(),
                        ));
                    }
                }
                Arc::new(
                    OpenAiCompatProvider::new(pc)
                        .map_err(|e| ProviderBuildError::Adapter(e.message))?,
                )
            }
        };
        let provider: Arc<dyn LlmProvider> = Arc::new(ModelDefaultsProvider {
            inner: provider,
            reasoning_effort: cfg.chat_options.reasoning_effort,
        });
        if let Some(session_id) = session_id {
            provider.bind_session_id(session_id);
        }
        Ok(provider)
    }
}

fn supports_reasoning_effort(cfg: &CodingAgentConfig) -> bool {
    // The capability is already resolved from CONFIG when the CodingAgentConfig is built
    // (`agent_config` / `apply_provider_config` via `endpoint_supports_reasoning_effort`),
    // so just read it — no hardcoded model name.
    cfg.supports_reasoning_effort
}

pub fn default_max_tokens(context_window: u32) -> u32 {
    // Output-token fallback cap when the server does not advertise a per-model
    // max_tokens. A quarter of the context window, floored at 8K so tiny windows
    // still get a usable response, ceilinged at 32K so a single large response
    // (e.g. scaffolding a whole file in one turn) has room to finish before it
    // trips finish_reason=length. A higher ceiling means fewer unrecoverable
    // truncations to warn about.
    (context_window / 4).clamp(8_000, 32_768)
}

/// Parse the `ATOMCODE_OLLAMA_NUM_CTX` override → `OllamaConfig::num_ctx`. `None` (unset,
/// blank, zero, or unparseable) ⇒ OMIT the knob and leave the Ollama daemon's own default
/// (`OLLAMA_CONTEXT_LENGTH`) untouched — sending a value here FORCES the runtime window, so
/// a bad/empty override must never shrink a server configured for a larger one. Explicit
/// opt-in ONLY; deliberately NOT derived from `context_window` (whose fallback would shrink
/// the daemon). Pinning matters because Ollama truncates an over-long prompt SILENTLY (no
/// error), so the kernel's context-overflow ladder never fires — see `OllamaConfig::num_ctx`.
fn parse_ollama_num_ctx(raw: Option<&str>) -> Option<u32> {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|&n| n > 0)
}

/// Build the adapter retry policy from the user's `retry_max_attempts` config
/// knob. `None` ⇒ the adapter default (3 attempts incl. the first request);
/// `Some(n)` ⇒ exactly `n` attempts. `1` disables automatic retries.
/// Reject zero instead of silently rewriting it: accepting `0` in
/// `config.toml` but executing one request makes the persisted configuration
/// disagree with the runtime and is especially surprising during `/reload`.
fn retry_policy_for(max_attempts: Option<u32>) -> Result<RetryPolicy, ProviderBuildError> {
    match max_attempts {
        None => Ok(RetryPolicy::default_policy()),
        Some(0) => Err(ProviderBuildError::Adapter(
            "invalid retry_max_attempts: expected at least 1 (1 disables automatic retries)".into(),
        )),
        Some(n) => Ok(RetryPolicy {
            max_attempts: n,
            ..RetryPolicy::default_policy()
        }),
    }
}

pub fn derive_tier_config(
    base: &CodingAgentConfig,
    provider_name: &str,
    provider: &atomcode_config::config::provider::ProviderConfig,
) -> CodingAgentConfig {
    // `base.clone()` carries `atomgit_enabled` from the host agent — subagents
    // share the switch with their parent (no main-off / sub-on split).
    let mut tier = base.clone();
    tier.model = provider.model.clone();
    tier.system_prompt = provider.system_prompt.clone();
    tier.supports_vision = provider.accepts_images();
    tier.provider_name = provider_name.to_string();
    if let Some(base_url) = &provider.base_url {
        tier.base_url = base_url.clone();
    }
    tier.api_key = provider.resolved_api_key().unwrap_or_default();
    tier.provider_type = provider.provider_type.clone();
    tier.context_window = provider.context_window as u32;
    tier.chat_options.max_tokens = provider.max_tokens.map(|value| value as u32);
    tier.thinking_type = provider.thinking_type.clone();
    tier.thinking_keep = provider.thinking_keep.clone();
    tier.reasoning_history = provider.reasoning_history.clone();
    tier.chat_options.reasoning_effort = atomcode_kernel::provider::ReasoningEffort::from_config(
        provider.reasoning_effort.as_deref(),
    );
    tier.supports_reasoning_effort = atomcode_config::config::endpoint_supports_reasoning_effort(
        provider.reasoning_effort.as_deref(),
        provider.reasoning_effort_levels.as_deref(),
    );
    tier.effort_levels = crate::config::effort_levels_for(
        tier.supports_reasoning_effort,
        provider.reasoning_effort_levels.as_deref(),
    );
    tier.thinking_enabled = provider.thinking_enabled;
    tier.user_agent = provider.user_agent.clone();
    tier.headers = provider.headers.clone();
    tier.skip_tls_verify = provider.skip_tls_verify;
    tier.retry_max_attempts = provider.retry_max_attempts;
    tier.response_header_timeout_secs = provider.response_header_timeout_secs;
    tier.subagent_fast_provider = None;
    tier.subagent_capable_provider = None;
    tier.subagent_model_providers = None;
    tier.subagent_config = None;
    tier
}

pub fn tier_provider_builder(
    factory: Arc<dyn CodingProviderFactory>,
    base: &CodingAgentConfig,
    host_model: &str,
    provider_name: &str,
    provider: &atomcode_config::config::provider::ProviderConfig,
) -> Option<SubagentProvider> {
    if provider.model == host_model {
        return None;
    }
    let tier = derive_tier_config(base, provider_name, provider);
    Some(Arc::new(move || factory.build(&tier, None).ok()))
}

/// Build a tier [`CodingAgentConfig`] from an already-resolved model selection
/// (design §14.2). Mirrors [`derive_tier_config`] but reads the flattened
/// [`ResolvedModelConfig`], so a tier can be a model profile on any account.
pub fn derive_tier_config_from_resolved(
    base: &CodingAgentConfig,
    resolved: &atomcode_config::config::provider::ResolvedModelConfig,
) -> CodingAgentConfig {
    // `base.clone()` carries `atomgit_enabled` from the host agent — subagents
    // share the switch with their parent (no main-off / sub-on split).
    let mut tier = base.clone();
    tier.model = resolved.model.clone();
    tier.system_prompt = resolved.system_prompt.clone();
    tier.supports_vision = resolved.supports_vision;
    tier.provider_name = resolved.selection_id.clone();
    if let Some(base_url) = &resolved.base_url {
        tier.base_url = base_url.clone();
    }
    tier.api_key = resolved.api_key.clone().unwrap_or_default();
    tier.provider_type = resolved.provider_type.clone();
    tier.context_window = resolved.context_window as u32;
    tier.chat_options.max_tokens = resolved.max_tokens.map(|value| value as u32);
    tier.thinking_type = resolved.thinking_type.clone();
    tier.thinking_keep = resolved.thinking_keep.clone();
    tier.reasoning_history = resolved.reasoning_history.clone();
    tier.chat_options.reasoning_effort = atomcode_kernel::provider::ReasoningEffort::from_config(
        resolved.reasoning_effort.as_deref(),
    );
    tier.supports_reasoning_effort = atomcode_config::config::endpoint_supports_reasoning_effort(
        resolved.reasoning_effort.as_deref(),
        resolved.reasoning_effort_levels.as_deref(),
    );
    tier.effort_levels = crate::config::effort_levels_for(
        tier.supports_reasoning_effort,
        resolved.reasoning_effort_levels.as_deref(),
    );
    tier.thinking_enabled = resolved.thinking_enabled;
    tier.user_agent = resolved.user_agent.clone();
    tier.headers = resolved.headers.clone();
    tier.skip_tls_verify = resolved.skip_tls_verify;
    tier.retry_max_attempts = resolved.retry_max_attempts;
    tier.response_header_timeout_secs = resolved.response_header_timeout_secs;
    tier.subagent_fast_provider = None;
    tier.subagent_capable_provider = None;
    tier.subagent_model_providers = None;
    tier.subagent_config = None;
    tier
}

/// [`tier_provider_builder`] for a resolved model selection.
pub fn tier_provider_builder_from_resolved(
    factory: Arc<dyn CodingProviderFactory>,
    base: &CodingAgentConfig,
    host_model: &str,
    resolved: &atomcode_config::config::provider::ResolvedModelConfig,
) -> Option<SubagentProvider> {
    if resolved.model == host_model {
        return None;
    }
    let tier = derive_tier_config_from_resolved(base, resolved);
    Some(Arc::new(move || factory.build(&tier, None).ok()))
}

pub fn resolve_subagent_tier_thunks(
    factory: Arc<dyn CodingProviderFactory>,
    base: &CodingAgentConfig,
    host_model: &str,
    config: &atomcode_config::config::Config,
) -> (SubagentProvider, SubagentProvider) {
    let none = || -> SubagentProvider { Arc::new(|| None) };
    let Some((fast_key, capable_key)) =
        crate::subagent_tiers::resolve_tier_keys(config, host_model)
    else {
        return (none(), none());
    };
    let thunk_for = |key: &str| -> SubagentProvider {
        config
            .resolve_model(Some(key))
            .ok()
            .and_then(|resolved| {
                tier_provider_builder_from_resolved(factory.clone(), base, host_model, &resolved)
            })
            .unwrap_or_else(none)
    };
    (thunk_for(&fast_key), thunk_for(&capable_key))
}

pub fn resolve_subagent_model_thunk(
    factory: Arc<dyn CodingProviderFactory>,
    base: &CodingAgentConfig,
    config: &atomcode_config::config::Config,
) -> SubagentModelResolver {
    let base = base.clone();
    let config = config.clone();
    Arc::new(move |selection| {
        let resolved = config
            .resolve_model(Some(selection))
            .map_err(|error| error.to_string())?;
        if resolved.selection_id == base.provider_name {
            return Ok(None);
        }
        let tier = derive_tier_config_from_resolved(&base, &resolved);
        factory
            .build(&tier, None)
            .map(Some)
            .map_err(|error| error.to_string())
    })
}

pub fn refresh_subagent_tiers(
    factory: Arc<dyn CodingProviderFactory>,
    coding: &CodingAgentConfig,
    config: &atomcode_config::config::Config,
) {
    if coding.subagent_fast_provider.is_none()
        && coding.subagent_capable_provider.is_none()
        && coding.subagent_model_providers.is_none()
    {
        return;
    }
    let (fast, capable) =
        resolve_subagent_tier_thunks(factory.clone(), coding, &coding.model, config);
    if let Some(cell) = &coding.subagent_fast_provider {
        cell.reset(fast);
    }
    if let Some(cell) = &coding.subagent_capable_provider {
        cell.reset(capable);
    }
    if let Some(models) = &coding.subagent_model_providers {
        models.reset(resolve_subagent_model_thunk(factory, coding, config));
    }
}

pub fn install_subagent_tiers(
    factory: Arc<dyn CodingProviderFactory>,
    coding: &mut CodingAgentConfig,
    config: &atomcode_config::config::Config,
) {
    let (fast, capable) =
        resolve_subagent_tier_thunks(factory.clone(), coding, &coding.model, config);
    coding.subagent_fast_provider = Some(TierProvider::new(fast));
    coding.subagent_capable_provider = Some(TierProvider::new(capable));
    coding.subagent_model_providers = Some(SubagentModelProviders::new(
        resolve_subagent_model_thunk(factory, coding, config),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn parse_ollama_num_ctx_only_accepts_a_positive_integer() {
        // Unset / blank ⇒ leave the daemon's own default alone (None), so we never
        // shrink a server configured for a larger window.
        assert_eq!(parse_ollama_num_ctx(None), None);
        assert_eq!(parse_ollama_num_ctx(Some("")), None);
        assert_eq!(parse_ollama_num_ctx(Some("   ")), None);
        // Zero and garbage are not a window ⇒ None (omit the knob).
        assert_eq!(parse_ollama_num_ctx(Some("0")), None);
        assert_eq!(parse_ollama_num_ctx(Some("abc")), None);
        assert_eq!(parse_ollama_num_ctx(Some("-1")), None);
        // A real value pins it (whitespace tolerated).
        assert_eq!(parse_ollama_num_ctx(Some("32768")), Some(32_768));
        assert_eq!(parse_ollama_num_ctx(Some("  8192 ")), Some(8_192));
    }

    #[derive(Default)]
    struct CapturedProvider {
        calls: std::sync::Mutex<Vec<ChatOptions>>,
        bindings: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for CapturedProvider {
        fn model_name(&self) -> &str {
            "configured-model"
        }
        fn context_window(&self) -> u32 {
            123_456
        }
        fn supports_vision(&self) -> bool {
            true
        }
        fn effort_levels(&self) -> Vec<String> {
            vec!["low".into(), "high".into(), "max".into()]
        }
        fn bind_session_id(&self, id: &str) {
            self.bindings.lock().unwrap().push(id.into());
        }
        async fn chat_stream(
            &self,
            _: &[atomcode_kernel::message::Message],
            _: &[atomcode_kernel::tool::ToolDef],
            options: &ChatOptions,
        ) -> Result<
            futures::stream::BoxStream<'static, atomcode_kernel::stream::StreamEvent>,
            atomcode_kernel::stream::ProviderError,
        > {
            self.calls.lock().unwrap().push(options.clone());
            Ok(Box::pin(futures::stream::empty()))
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn direct_calls_inherit_model_effort_and_explicit_calls_override_it() {
        let inner = Arc::new(CapturedProvider::default());
        let provider = ModelDefaultsProvider {
            inner: inner.clone(),
            reasoning_effort: Some(ReasoningEffort::High),
        };
        let options = ChatOptions {
            max_tokens: Some(77),
            temperature: Some(0.25),
            tool_choice: atomcode_kernel::provider::ToolChoice::None,
            ..Default::default()
        };
        for effort in [
            None,
            None,
            Some(ReasoningEffort::Low),
            Some(ReasoningEffort::Max),
        ] {
            let mut request = options.clone();
            request.reasoning_effort = effort;
            drop(provider.chat_stream(&[], &[], &request).await.unwrap());
            assert_eq!(
                request.reasoning_effort, effort,
                "caller-owned options are not mutated"
            );
        }
        let calls = inner.calls.lock().unwrap();
        assert_eq!(
            calls.iter().map(|c| c.reasoning_effort).collect::<Vec<_>>(),
            vec![
                Some(ReasoningEffort::High),
                Some(ReasoningEffort::High),
                Some(ReasoningEffort::Low),
                Some(ReasoningEffort::Max)
            ]
        );
        for call in calls.iter() {
            assert_eq!(call.max_tokens, options.max_tokens);
            assert_eq!(call.temperature, options.temperature);
            assert_eq!(call.tool_choice, options.tool_choice);
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn rebuilding_with_api_default_and_other_model_defaults_keeps_them_independent() {
        let inner = Arc::new(CapturedProvider::default());
        for effort in [
            Some(ReasoningEffort::High),
            None,
            Some(ReasoningEffort::Low),
        ] {
            let provider = ModelDefaultsProvider {
                inner: inner.clone(),
                reasoning_effort: effort,
            };
            drop(
                provider
                    .chat_stream(&[], &[], &ChatOptions::default())
                    .await
                    .unwrap(),
            );
        }
        assert_eq!(
            inner
                .calls
                .lock()
                .unwrap()
                .iter()
                .map(|c| c.reasoning_effort)
                .collect::<Vec<_>>(),
            vec![
                Some(ReasoningEffort::High),
                None,
                Some(ReasoningEffort::Low)
            ]
        );
    }

    #[test]
    fn model_defaults_forward_capabilities_and_session_affinity() {
        let inner = Arc::new(CapturedProvider::default());
        let provider = ModelDefaultsProvider {
            inner: inner.clone(),
            reasoning_effort: Some(ReasoningEffort::High),
        };
        assert_eq!(provider.model_name(), inner.model_name());
        assert_eq!(provider.context_window(), inner.context_window());
        assert_eq!(provider.supports_vision(), inner.supports_vision());
        assert_eq!(provider.effort_levels(), inner.effort_levels());
        provider.bind_session_id("test-session");
        assert_eq!(*inner.bindings.lock().unwrap(), ["test-session"]);
    }

    fn config(provider_type: &str) -> CodingAgentConfig {
        let mut cfg = CodingAgentConfig::new(
            "key",
            "http://localhost:11434/v1",
            "model",
            PathBuf::from("."),
            crate::config::product_dirs_from_env(),
        );
        cfg.provider_type = provider_type.to_string();
        cfg.context_window = 64_000;
        cfg.user_agent = Some("test-agent".into());
        cfg
    }

    #[test]
    fn dispatches_all_supported_provider_types() {
        let factory = DefaultCodingProviderFactory::new("fallback-agent");
        for kind in ["openai", "claude", "ollama", "deepseek-messages"] {
            assert!(
                factory.build(&config(kind), None).is_ok(),
                "provider type {kind}"
            );
        }
    }

    #[tokio::test]
    async fn deepseek_messages_dispatch_does_not_fall_back_to_openai() {
        let mut cfg = config("deepseek-messages");
        cfg.model = "deepseek-flash".into();
        cfg.thinking_enabled = Some(true);
        let provider = DefaultCodingProviderFactory::new("test")
            .build(&cfg, None)
            .unwrap();
        let options = atomcode_kernel::provider::ChatOptions {
            reasoning_effort: Some(atomcode_kernel::provider::ReasoningEffort::Medium),
            ..Default::default()
        };
        let Err(error) = provider.chat_stream(&[], &[], &options).await else {
            panic!("invalid effort accepted")
        };
        assert_eq!(
            error.message,
            "DeepSeek Messages supports reasoning effort low, high or max"
        );
        assert!(!error.retryable);
    }

    #[test]
    fn default_output_cap_matches_bounds() {
        assert_eq!(
            default_max_tokens(16_000),
            8_000,
            "tiny window floors at 8K"
        );
        assert_eq!(
            default_max_tokens(64_000),
            16_000,
            "mid window is a quarter"
        );
        // Large windows ceiling at 32K (raised from 16K) so a single big response
        // has room to finish before finish_reason=length.
        assert_eq!(default_max_tokens(200_000), 32_768);
        assert_eq!(
            default_max_tokens(128_000),
            32_000,
            "128K/4 sits just under the ceiling"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn configured_output_budget_reaches_every_wire_format() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // This tests real local HTTP serialization, not timeout behavior.
        tokio::time::resume();
        for protocol in [
            "openai",
            "responses",
            "anthropic",
            "deepseek-messages",
            "ollama",
        ] {
            for budget in [None, Some(4096_u32), Some(393216)] {
                for call_override in [None, Some(1024_u32)] {
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let base_url = format!("http://{}", listener.local_addr().unwrap());
                    let server = tokio::spawn(async move {
                        let (mut socket, _) = listener.accept().await.unwrap();
                        let mut bytes = Vec::new();
                        let (body_start, length) = loop {
                            let mut chunk = [0_u8; 4096];
                            let n = socket.read(&mut chunk).await.unwrap();
                            assert!(n > 0, "request ended before headers");
                            bytes.extend_from_slice(&chunk[..n]);
                            if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                                let headers =
                                    String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                                let length: usize = headers
                                    .lines()
                                    .find_map(|l| l.strip_prefix("content-length:"))
                                    .unwrap()
                                    .trim()
                                    .parse()
                                    .unwrap();
                                break (end + 4, length);
                            }
                        };
                        while bytes.len() < body_start + length {
                            let mut chunk = [0_u8; 4096];
                            let n = socket.read(&mut chunk).await.unwrap();
                            assert!(n > 0, "request ended before body");
                            bytes.extend_from_slice(&chunk[..n]);
                        }
                        let body: serde_json::Value =
                            serde_json::from_slice(&bytes[body_start..body_start + length])
                                .unwrap();
                        // A terminal response is sufficient: assertions concern the request.
                        socket.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                        body
                    });
                    let catalog: atomcode_config::config::Config = serde_json::from_value(serde_json::json!({
                        "providers": {"selected": {
                            "type": protocol, "model": "test-model", "base_url": base_url,
                            "context_window": 512000, "max_tokens": budget, "retry_max_attempts": 1
                        }}
                    })).unwrap();
                    let selected = catalog.resolve_model(Some("selected")).unwrap();
                    let cfg = derive_tier_config_from_resolved(&config("openai"), &selected);
                    let provider = DefaultCodingProviderFactory::new("test")
                        .build(&cfg, None)
                        .unwrap();
                    let result = provider
                        .chat_stream(
                            &[atomcode_kernel::message::Message::user("hi")],
                            &[],
                            &ChatOptions {
                                max_tokens: call_override,
                                ..Default::default()
                            },
                        )
                        .await;
                    assert!(result.is_err());
                    let body = server.await.unwrap();
                    let sent = match protocol {
                        "responses" => &body["max_output_tokens"],
                        "ollama" => &body["options"]["num_predict"],
                        _ => &body["max_tokens"],
                    };
                    let expected = call_override.or(budget).unwrap_or(32768);
                    assert_eq!(
                        sent.as_u64(),
                        Some(u64::from(expected)),
                        "{protocol}, {budget:?}, {call_override:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn retry_policy_uses_default_accepts_explicit_values_and_rejects_zero() {
        assert_eq!(retry_policy_for(None).unwrap().max_attempts, 3);
        assert_eq!(retry_policy_for(Some(1)).unwrap().max_attempts, 1);
        assert_eq!(retry_policy_for(Some(5)).unwrap().max_attempts, 5);

        let error = retry_policy_for(Some(0)).unwrap_err().to_string();
        assert!(error.contains("expected at least 1"), "{error}");
    }

    #[test]
    fn supports_reasoning_effort_reads_the_config_resolved_capability() {
        // The capability is resolved from CONFIG when the CodingAgentConfig is built
        // (via `endpoint_supports_reasoning_effort`), so this reader just reflects the
        // field — NO hardcoded model name. (The config-driven derivation itself is
        // covered by `endpoint_supports_reasoning_effort_is_config_driven` in the config
        // crate and by `tier_configs_preserve_default_effort_and_capability` below.)
        let mut cfg = config("openai");
        cfg.model = "qwen3.8-27b".into();
        cfg.provider_name = "AtomGit-qwen3.8-27b".into();
        cfg.supports_reasoning_effort = false;
        assert!(!supports_reasoning_effort(&cfg));
        cfg.supports_reasoning_effort = true;
        assert!(supports_reasoning_effort(&cfg));
    }

    #[test]
    fn a_header_only_tier_never_inherits_its_parents_credentials() {
        let catalog: atomcode_config::config::Config = serde_json::from_value(serde_json::json!({
            "providers": { "target": { "type": "ollama", "model": "test", "base_url": "http://localhost:11434", "headers": { "X-Target-Token": "target-secret" } } }
        })).unwrap();
        let mut base = config("openai");
        base.api_key = "parent-secret".into();
        base.headers =
            serde_json::from_value(serde_json::json!({"X-Parent-Token":"parent-header-secret"}))
                .unwrap();
        let resolved = catalog.resolve_model(Some("target")).unwrap();
        for tier in [
            derive_tier_config(&base, "target", &catalog.providers["target"]),
            derive_tier_config_from_resolved(&base, &resolved),
        ] {
            assert!(tier.api_key.is_empty());
            assert_eq!(tier.headers.names(), vec!["x-target-token"]);
            assert_eq!(
                tier.headers.iter().next(),
                Some(("x-target-token", "target-secret"))
            );
        }
    }

    #[test]
    fn tier_configs_preserve_default_effort_and_capability() {
        let catalog: atomcode_config::config::Config = serde_json::from_value(serde_json::json!({
            "provider_accounts": {
                "custom": {
                    "provider": "openai-compatible",
                    "base_url": "https://example.invalid/v1"
                }
            },
            "models": {
                "custom/model": {
                    "account": "custom",
                    "model": "vendor-model",
                    "reasoning_effort": "medium",
                    "retry_max_attempts": 7,
                    "reasoning_timeout_secs": 900
                }
            },
            "default_model": "custom/model"
        }))
        .unwrap();
        let resolved = catalog.resolve_model(Some("custom/model")).unwrap();
        let tier = derive_tier_config_from_resolved(&config("openai"), &resolved);

        assert!(tier.supports_reasoning_effort);
        assert_eq!(
            tier.chat_options.reasoning_effort,
            Some(atomcode_kernel::provider::ReasoningEffort::Medium)
        );
        assert_eq!(tier.retry_max_attempts, Some(7));
    }

    #[test]
    fn tier_capability_follows_server_advertised_levels_without_an_explicit_effort() {
        // The qwen3.8-27b case: the endpoint advertises effort LEVELS but no effort is set
        // yet. `supports_reasoning_effort` must be TRUE (so the wire will send the effort
        // once the user picks one) — driven by config, not a hardcoded deepseek name.
        let catalog: atomcode_config::config::Config = serde_json::from_value(serde_json::json!({
            "provider_accounts": {
                "custom": { "provider": "openai-compatible", "base_url": "https://example.invalid/v1" }
            },
            "models": {
                "custom/model": {
                    "account": "custom", "model": "vendor-model",
                    "reasoning_effort_levels": ["low", "medium", "xhigh"]
                }
            },
            "default_model": "custom/model"
        }))
        .unwrap();
        let resolved = catalog.resolve_model(Some("custom/model")).unwrap();
        let tier = derive_tier_config_from_resolved(&config("openai"), &resolved);
        assert!(
            tier.supports_reasoning_effort,
            "advertised levels → wire capability on, even with no explicit effort"
        );
        assert_eq!(tier.chat_options.reasoning_effort, None);
    }

    #[test]
    fn legacy_tier_config_preserves_retry_attempts() {
        let provider: atomcode_config::config::provider::ProviderConfig =
            serde_json::from_value(serde_json::json!({
                "type": "openai",
                "model": "tier-model",
                "base_url": "https://example.invalid/v1",
                "retry_max_attempts": 6,
                "reasoning_timeout_secs": 0
            }))
            .unwrap();

        let tier = derive_tier_config(&config("openai"), "tier-provider", &provider);
        assert_eq!(tier.retry_max_attempts, Some(6));
    }
}

#[cfg(test)]
mod response_header_timeout_tests {
    use super::*;
    #[test]
    fn runtime_and_tiers_take_the_selected_models_timeout() {
        let source: atomcode_config::Config = toml::from_str(
            r#"
[providers.slow]
type = "anthropic"
model = "slow"
response_header_timeout_secs = 600
[providers.fast]
type = "openai"
model = "fast"
"#,
        )
        .unwrap();
        let runtime = crate::CodingRuntimeConfig::from_config(
            &source,
            std::path::Path::new("/tmp"),
            crate::config::product_dirs_from_env(),
            Some("slow"),
            None,
            false,
            true,
        );
        let base = runtime.agent_config();
        assert_eq!(
            response_header_timeout(&base),
            std::time::Duration::from_secs(600)
        );
        let tier = derive_tier_config(&base, "fast", &source.providers["fast"]);
        assert!(
            tier.response_header_timeout_secs.is_none(),
            "do not inherit the host override"
        );
        let resolved = source.resolve_model(Some("fast")).unwrap();
        assert!(derive_tier_config_from_resolved(&base, &resolved)
            .response_header_timeout_secs
            .is_none());
        let mut switched = base;
        crate::config::apply_provider_config(&mut switched, &resolved.to_provider_config());
        assert!(switched.response_header_timeout_secs.is_none());
    }

    #[test]
    fn provider_override_wins_and_invalid_env_cannot_disable_the_deadline() {
        use std::{num::NonZeroU64, time::Duration};
        assert_eq!(
            resolve_response_header_timeout(None, None),
            Duration::from_secs(300)
        );
        assert_eq!(
            resolve_response_header_timeout(None, Some("600")),
            Duration::from_secs(600)
        );
        assert_eq!(
            resolve_response_header_timeout(NonZeroU64::new(120), Some("600")),
            Duration::from_secs(120)
        );
        for invalid in ["0", "-1", "invalid", ""] {
            assert_eq!(
                resolve_response_header_timeout(None, Some(invalid)),
                Duration::from_secs(300)
            );
        }
        assert_eq!(
            AnthropicConfig::new("", "http://localhost", "test").open_timeout,
            Duration::from_secs(300)
        );
        assert_eq!(
            OpenAiCompatConfig::new("", "http://localhost", "test").open_timeout,
            Duration::from_secs(300)
        );
    }
}
