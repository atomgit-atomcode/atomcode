use std::sync::Arc;

use atomcode_capabilities::provider::{
    atomgit_request_signer, is_atomgit_gateway, signer_available, AnthropicConfig,
    AnthropicProvider, OllamaConfig, OllamaProvider, OpenAiCompatConfig, OpenAiCompatProvider,
    ReasoningPolicy, RequestSigner,
};
use atomcode_kernel::provider::LlmProvider;

use crate::CodingAgentConfig;
use crate::{SubagentProvider, TierProvider};

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
    fn request_signer(
        &self,
        base_url: &str,
    ) -> Result<Option<Arc<dyn RequestSigner>>, ProviderBuildError>;
}

pub struct AtomGitProviderAuthenticator;

impl ProviderAuthenticator for AtomGitProviderAuthenticator {
    fn request_signer(
        &self,
        base_url: &str,
    ) -> Result<Option<Arc<dyn RequestSigner>>, ProviderBuildError> {
        if !is_atomgit_gateway(base_url) {
            return Ok(None);
        }
        if !signer_available() {
            return Err(ProviderBuildError::SourceBuildGatewayUnsupported {
                base_url: base_url.to_string(),
            });
        }
        atomgit_request_signer(base_url)
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
        let provider: Arc<dyn LlmProvider> = match cfg.provider_type.as_str() {
            "claude" | "anthropic" => {
                let mut ac = AnthropicConfig::new(&cfg.api_key, &cfg.base_url, &cfg.model);
                ac.context_window = cfg.context_window;
                ac.idle_timeout = cfg.stream_timeout;
                ac.max_tokens = default_max_tokens(cfg.context_window);
                ac.thinking = cfg.thinking_enabled.unwrap_or(false);
                ac.user_agent = Some(ua.clone());
                ac.skip_tls_verify = cfg.skip_tls_verify;
                Arc::new(
                    AnthropicProvider::new(ac)
                        .map_err(|e| ProviderBuildError::Adapter(e.message))?,
                )
            }
            "ollama" => {
                let mut oc = OllamaConfig::new(&cfg.base_url, &cfg.model);
                oc.api_key = cfg.api_key.clone();
                oc.context_window = cfg.context_window;
                oc.idle_timeout = cfg.stream_timeout;
                oc.max_tokens = Some(default_max_tokens(cfg.context_window));
                oc.think = cfg.thinking_enabled.unwrap_or(false);
                oc.user_agent = Some(ua.clone());
                oc.skip_tls_verify = cfg.skip_tls_verify;
                Arc::new(
                    OllamaProvider::new(oc).map_err(|e| ProviderBuildError::Adapter(e.message))?,
                )
            }
            _ => {
                let mut pc = OpenAiCompatConfig::new(&cfg.api_key, &cfg.base_url, &cfg.model);
                pc.context_window = cfg.context_window;
                pc.idle_timeout = cfg.stream_timeout;
                pc.supports_vision =
                    atomcode_capabilities::provider::model_suggests_vision(&cfg.model);
                pc.max_tokens = Some(default_max_tokens(cfg.context_window));
                pc.reasoning_policy =
                    ReasoningPolicy::from_config(cfg.reasoning_history.as_deref())
                        .map_err(ProviderBuildError::Adapter)?;
                pc.thinking_type = cfg.thinking_type.clone();
                pc.thinking_keep = cfg.thinking_keep.clone();
                pc.user_agent = Some(ua);
                pc.skip_tls_verify = cfg.skip_tls_verify;
                if let Some(authenticator) = &self.authenticator {
                    pc.request_signer = authenticator.request_signer(&cfg.base_url)?;
                }
                Arc::new(
                    OpenAiCompatProvider::new(pc)
                        .map_err(|e| ProviderBuildError::Adapter(e.message))?,
                )
            }
        };
        if let Some(session_id) = session_id {
            provider.bind_session_id(session_id);
        }
        Ok(provider)
    }
}

pub fn default_max_tokens(context_window: u32) -> u32 {
    (context_window / 4).clamp(8_000, 16_384)
}

pub fn derive_tier_config(
    base: &CodingAgentConfig,
    provider_name: &str,
    provider: &atomcode_config::config::provider::ProviderConfig,
) -> CodingAgentConfig {
    let mut tier = base.clone();
    tier.model = provider.model.clone();
    tier.provider_name = provider_name.to_string();
    tier.pricing = crate::resolve_provider_pricing(provider_name, provider);
    if let Some(base_url) = &provider.base_url {
        tier.base_url = base_url.clone();
    }
    if let Some(api_key) = provider.resolved_api_key() {
        tier.api_key = api_key;
    }
    tier.provider_type = provider.provider_type.clone();
    tier.context_window = provider.context_window as u32;
    tier.chat_options.max_tokens = provider.max_tokens.map(|value| value as u32);
    tier.thinking_type = provider.thinking_type.clone();
    tier.thinking_keep = provider.thinking_keep.clone();
    tier.reasoning_history = provider.reasoning_history.clone();
    tier.thinking_enabled = provider.thinking_enabled;
    tier.user_agent = provider.user_agent.clone();
    tier.skip_tls_verify = provider.skip_tls_verify;
    tier.subagent_fast_provider = None;
    tier.subagent_capable_provider = None;
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
    let mut tier = base.clone();
    tier.model = resolved.model.clone();
    tier.provider_name = resolved.selection_id.clone();
    tier.pricing = crate::resolve_resolved_pricing(resolved);
    if let Some(base_url) = &resolved.base_url {
        tier.base_url = base_url.clone();
    }
    if let Some(api_key) = &resolved.api_key {
        tier.api_key = api_key.clone();
    }
    tier.provider_type = resolved.provider_type.clone();
    tier.context_window = resolved.context_window as u32;
    tier.chat_options.max_tokens = resolved.max_tokens.map(|value| value as u32);
    tier.thinking_type = resolved.thinking_type.clone();
    tier.thinking_keep = resolved.thinking_keep.clone();
    tier.reasoning_history = resolved.reasoning_history.clone();
    tier.thinking_enabled = resolved.thinking_enabled;
    tier.user_agent = resolved.user_agent.clone();
    tier.skip_tls_verify = resolved.skip_tls_verify;
    tier.subagent_fast_provider = None;
    tier.subagent_capable_provider = None;
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

pub fn refresh_subagent_tiers(
    factory: Arc<dyn CodingProviderFactory>,
    coding: &CodingAgentConfig,
    config: &atomcode_config::config::Config,
) {
    if coding.subagent_fast_provider.is_none() && coding.subagent_capable_provider.is_none() {
        return;
    }
    let (fast, capable) = resolve_subagent_tier_thunks(factory, coding, &coding.model, config);
    if let Some(cell) = &coding.subagent_fast_provider {
        cell.reset(fast);
    }
    if let Some(cell) = &coding.subagent_capable_provider {
        cell.reset(capable);
    }
}

pub fn install_subagent_tiers(
    factory: Arc<dyn CodingProviderFactory>,
    coding: &mut CodingAgentConfig,
    config: &atomcode_config::config::Config,
) {
    let (fast, capable) = resolve_subagent_tier_thunks(factory, coding, &coding.model, config);
    coding.subagent_fast_provider = Some(TierProvider::new(fast));
    coding.subagent_capable_provider = Some(TierProvider::new(capable));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn config(provider_type: &str) -> CodingAgentConfig {
        let mut cfg = CodingAgentConfig::new(
            "key",
            "http://localhost:11434/v1",
            "model",
            PathBuf::from("."),
        );
        cfg.provider_type = provider_type.to_string();
        cfg.context_window = 64_000;
        cfg.user_agent = Some("test-agent".into());
        cfg
    }

    #[test]
    fn dispatches_all_supported_provider_types() {
        let factory = DefaultCodingProviderFactory::new("fallback-agent");
        for kind in ["openai", "claude", "ollama"] {
            assert!(
                factory.build(&config(kind), None).is_ok(),
                "provider type {kind}"
            );
        }
    }

    #[test]
    fn default_output_cap_matches_legacy_bounds() {
        assert_eq!(default_max_tokens(16_000), 8_000);
        assert_eq!(default_max_tokens(64_000), 16_000);
        assert_eq!(default_max_tokens(200_000), 16_384);
    }
}
