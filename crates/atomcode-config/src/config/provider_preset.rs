//! Compiled, read-only provider presets.
//!
//! A preset carries stable per-vendor connection + auth defaults so the
//! `/provider` flow can offer "pick a vendor, add a key" instead of asking for
//! low-level fields. Presets are NOT persistence owners: a provider *account*
//! (see [`super::provider`]) references a preset by `id`, and every preset field
//! remains overridable by that account. Model recommendation / discovery data is
//! deliberately kept out of this module (it lives in the future model catalog);
//! presets hold only stable connection metadata.
//!
//! The registry is curated, not exhaustive. An unknown vendor id resolves to the
//! generic OpenAI-compatible preset via [`preset_or_compatible`], so a custom
//! endpoint is always reachable without waiting for a release.

/// Wire protocol a preset speaks. Maps 1:1 to the `type = "..."` string that
/// `provider_factory` dispatches on (`ProviderConfig.provider_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderType {
    OpenAi,
    Anthropic,
    Ollama,
}

impl ProviderType {
    /// The wire `type` string used by the provider factory and persisted config.
    pub fn wire(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::Ollama => "ollama",
        }
    }
}

/// How an account authenticates against a preset. `OAuth` is reserved for future
/// adapters (e.g. GitHub Copilot, deferred); no current preset uses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    ApiKey,
    OAuth,
    None,
}

/// Where a preset's selectable model list comes from. This is only a hint for the
/// wizard/discovery layer; the actual model data is populated elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSource {
    Embedded,
    DiscoveryApi,
    Manual,
}

/// A compiled vendor preset. All fields are `'static` — presets live in the
/// binary and are never mutated or persisted.
#[derive(Debug, Clone, Copy)]
pub struct ProviderPreset {
    pub id: &'static str,
    pub display_name: &'static str,
    pub provider_type: ProviderType,
    pub default_base_url: Option<&'static str>,
    pub auth_kind: AuthKind,
    pub api_key_env: Option<&'static str>,
    pub model_source: ModelSource,
}

/// Generic OpenAI-compatible custom endpoint, and the fallback for unknown ids.
pub const OPENAI_COMPATIBLE: ProviderPreset = ProviderPreset {
    id: "openai-compatible",
    display_name: "OpenAI-compatible endpoint",
    provider_type: ProviderType::OpenAi,
    default_base_url: None,
    auth_kind: AuthKind::ApiKey,
    api_key_env: None,
    model_source: ModelSource::Manual,
};

/// Generic Anthropic-compatible custom endpoint.
pub const ANTHROPIC_COMPATIBLE: ProviderPreset = ProviderPreset {
    id: "anthropic-compatible",
    display_name: "Anthropic-compatible endpoint",
    provider_type: ProviderType::Anthropic,
    default_base_url: None,
    auth_kind: AuthKind::ApiKey,
    api_key_env: None,
    model_source: ModelSource::Manual,
};

/// The curated preset registry, in display order. Curated, not exhaustive —
/// every field is overridable by the referencing account, and an unlisted
/// vendor falls back to [`OPENAI_COMPATIBLE`]. Default base URLs are best-effort
/// vendor defaults and may be overridden per account.
pub const PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        id: "atomgit",
        display_name: "AtomGit",
        provider_type: ProviderType::OpenAi,
        default_base_url: Some("https://llm-api.atomgit.com/v1"),
        auth_kind: AuthKind::ApiKey,
        api_key_env: None,
        model_source: ModelSource::DiscoveryApi,
    },
    ProviderPreset {
        id: "taotoken",
        display_name: "TaoToken",
        provider_type: ProviderType::OpenAi,
        default_base_url: Some("https://taotoken.net/api/v1"),
        auth_kind: AuthKind::ApiKey,
        api_key_env: None,
        model_source: ModelSource::Manual,
    },
    ProviderPreset {
        id: "aliyun",
        display_name: "Alibaba Cloud Model Studio",
        provider_type: ProviderType::OpenAi,
        default_base_url: Some("https://dashscope.aliyuncs.com/compatible-mode/v1"),
        auth_kind: AuthKind::ApiKey,
        api_key_env: Some("DASHSCOPE_API_KEY"),
        model_source: ModelSource::Manual,
    },
    ProviderPreset {
        id: "volcengine",
        display_name: "Volcengine Ark",
        provider_type: ProviderType::OpenAi,
        default_base_url: Some("https://ark.cn-beijing.volces.com/api/v3"),
        auth_kind: AuthKind::ApiKey,
        api_key_env: Some("ARK_API_KEY"),
        model_source: ModelSource::Manual,
    },
    ProviderPreset {
        id: "xiaomi-mimo",
        display_name: "Xiaomi MiMo",
        provider_type: ProviderType::OpenAi,
        default_base_url: None,
        auth_kind: AuthKind::ApiKey,
        api_key_env: None,
        model_source: ModelSource::Manual,
    },
    ProviderPreset {
        id: "deepseek",
        display_name: "DeepSeek",
        provider_type: ProviderType::OpenAi,
        default_base_url: Some("https://api.deepseek.com/v1"),
        auth_kind: AuthKind::ApiKey,
        api_key_env: Some("DEEPSEEK_API_KEY"),
        model_source: ModelSource::DiscoveryApi,
    },
    ProviderPreset {
        id: "zhipu",
        display_name: "Zhipu AI",
        provider_type: ProviderType::OpenAi,
        default_base_url: Some("https://open.bigmodel.cn/api/paas/v4"),
        auth_kind: AuthKind::ApiKey,
        api_key_env: Some("ZHIPUAI_API_KEY"),
        model_source: ModelSource::Manual,
    },
    ProviderPreset {
        id: "moonshot",
        display_name: "Moonshot",
        provider_type: ProviderType::OpenAi,
        default_base_url: Some("https://api.moonshot.cn/v1"),
        auth_kind: AuthKind::ApiKey,
        api_key_env: Some("MOONSHOT_API_KEY"),
        model_source: ModelSource::DiscoveryApi,
    },
    ProviderPreset {
        id: "minimax",
        display_name: "MiniMax",
        provider_type: ProviderType::OpenAi,
        default_base_url: Some("https://api.minimaxi.com/v1"),
        auth_kind: AuthKind::ApiKey,
        api_key_env: Some("MINIMAX_API_KEY"),
        model_source: ModelSource::Manual,
    },
    ProviderPreset {
        id: "siliconflow",
        display_name: "SiliconFlow",
        provider_type: ProviderType::OpenAi,
        default_base_url: Some("https://api.siliconflow.cn/v1"),
        auth_kind: AuthKind::ApiKey,
        api_key_env: Some("SILICONFLOW_API_KEY"),
        model_source: ModelSource::DiscoveryApi,
    },
    ProviderPreset {
        id: "openrouter",
        display_name: "OpenRouter",
        provider_type: ProviderType::OpenAi,
        default_base_url: Some("https://openrouter.ai/api/v1"),
        auth_kind: AuthKind::ApiKey,
        api_key_env: Some("OPENROUTER_API_KEY"),
        model_source: ModelSource::DiscoveryApi,
    },
    ProviderPreset {
        id: "opencode",
        display_name: "OpenCode Zen",
        provider_type: ProviderType::OpenAi,
        default_base_url: Some("https://opencode.ai/zen/v1"),
        auth_kind: AuthKind::ApiKey,
        api_key_env: Some("OPENCODE_API_KEY"),
        model_source: ModelSource::Manual,
    },
    ProviderPreset {
        id: "openai",
        display_name: "OpenAI",
        provider_type: ProviderType::OpenAi,
        default_base_url: Some("https://api.openai.com/v1"),
        auth_kind: AuthKind::ApiKey,
        api_key_env: Some("OPENAI_API_KEY"),
        model_source: ModelSource::DiscoveryApi,
    },
    ProviderPreset {
        id: "anthropic",
        display_name: "Anthropic",
        provider_type: ProviderType::Anthropic,
        default_base_url: Some("https://api.anthropic.com"),
        auth_kind: AuthKind::ApiKey,
        api_key_env: Some("ANTHROPIC_API_KEY"),
        model_source: ModelSource::Manual,
    },
    ProviderPreset {
        id: "ollama",
        display_name: "Ollama",
        provider_type: ProviderType::Ollama,
        default_base_url: Some("http://localhost:11434"),
        auth_kind: AuthKind::None,
        api_key_env: None,
        model_source: ModelSource::DiscoveryApi,
    },
    OPENAI_COMPATIBLE,
    ANTHROPIC_COMPATIBLE,
];

/// Exact lookup of a preset by its `id`.
pub fn preset(id: &str) -> Option<&'static ProviderPreset> {
    PRESETS.iter().find(|p| p.id == id)
}

/// Resolve a preset by id, falling back to the generic OpenAI-compatible preset
/// for unknown ids (custom endpoints / unrecognised vendors always resolve).
pub fn preset_or_compatible(id: &str) -> &'static ProviderPreset {
    preset(id).unwrap_or(&OPENAI_COMPATIBLE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// The curated vendor set (14 vendors + 2 generic compatible presets) must
    /// all be present and resolvable by id.
    #[test]
    fn registry_covers_the_curated_vendors() {
        assert!(
            PRESETS.len() >= 16,
            "expected the curated vendor set (>=16), got {}",
            PRESETS.len()
        );
        for id in [
            "atomgit",
            "aliyun",
            "volcengine",
            "xiaomi-mimo",
            "deepseek",
            "zhipu",
            "moonshot",
            "minimax",
            "siliconflow",
            "openrouter",
            "opencode",
            "taotoken",
            "openai",
            "anthropic",
            "ollama",
            "openai-compatible",
            "anthropic-compatible",
        ] {
            assert!(preset(id).is_some(), "missing preset: {id}");
        }
    }

    #[test]
    fn preset_ids_are_unique() {
        let mut seen = HashSet::new();
        for p in PRESETS {
            assert!(seen.insert(p.id), "duplicate preset id: {}", p.id);
        }
    }

    #[test]
    fn taotoken_uses_its_openai_compatible_endpoint() {
        let taotoken = preset("taotoken").expect("taotoken preset");
        assert_eq!(taotoken.display_name, "TaoToken");
        assert_eq!(taotoken.provider_type, ProviderType::OpenAi);
        assert_eq!(
            taotoken.default_base_url,
            Some("https://taotoken.net/api/v1")
        );
        assert_eq!(taotoken.auth_kind, AuthKind::ApiKey);
        assert_eq!(taotoken.model_source, ModelSource::Manual);
    }

    #[test]
    fn opencode_uses_its_openai_compatible_endpoint() {
        let opencode = preset("opencode").expect("opencode preset");
        assert_eq!(opencode.display_name, "OpenCode Zen");
        assert_eq!(opencode.provider_type, ProviderType::OpenAi);
        assert_eq!(
            opencode.default_base_url,
            Some("https://opencode.ai/zen/v1")
        );
        assert_eq!(opencode.auth_kind, AuthKind::ApiKey);
        assert_eq!(opencode.api_key_env, Some("OPENCODE_API_KEY"));
        assert_eq!(opencode.model_source, ModelSource::Manual);
    }

    #[test]
    fn every_preset_has_required_stable_fields() {
        for p in PRESETS {
            assert!(!p.id.is_empty(), "empty id");
            assert!(
                !p.display_name.is_empty(),
                "empty display_name for {}",
                p.id
            );
            // Concrete hosted vendors ship a default base_url. The generic
            // `*-compatible` presets and open-weights models without a fixed
            // public endpoint (Xiaomi MiMo) legitimately leave it unset.
            let endpoint_optional = matches!(
                p.id,
                "openai-compatible" | "anthropic-compatible" | "xiaomi-mimo"
            );
            if !endpoint_optional {
                assert!(
                    p.default_base_url.is_some(),
                    "{} must ship a default base_url",
                    p.id
                );
            }
            // Ollama is keyless local; every hosted vendor here uses an API key
            // (OAuth presets are deferred with GitHub Copilot).
            match p.provider_type {
                ProviderType::Ollama => assert_eq!(p.auth_kind, AuthKind::None, "{}", p.id),
                _ => assert_eq!(p.auth_kind, AuthKind::ApiKey, "{}", p.id),
            }
        }
    }

    #[test]
    fn lookup_by_id_returns_the_matching_preset() {
        let ds = preset("deepseek").expect("deepseek preset");
        assert_eq!(ds.provider_type, ProviderType::OpenAi);
        assert_eq!(ds.provider_type.wire(), "openai");
        assert!(
            ds.default_base_url.unwrap().contains("deepseek"),
            "deepseek base_url looks wrong: {:?}",
            ds.default_base_url
        );
        let an = preset("anthropic").expect("anthropic preset");
        assert_eq!(an.provider_type.wire(), "anthropic");
    }

    #[test]
    fn unknown_id_falls_back_to_openai_compatible() {
        let p = preset_or_compatible("some-unlisted-vendor");
        assert_eq!(p.id, "openai-compatible");
        assert_eq!(p.provider_type, ProviderType::OpenAi);
        assert!(p.default_base_url.is_none());
        // A known id resolves to itself, not the fallback.
        assert_eq!(preset_or_compatible("deepseek").id, "deepseek");
    }
}
