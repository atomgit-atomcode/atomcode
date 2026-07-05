//! 从登录后的 `Config.providers`(每个 plan-available 模型都在这)解析出
//! fast / capable 两档各自的 provider key。单模型自动塌缩到默认 provider。

use atomcode_core::config::Config;
use atomcode_core::provider::{model_speed_tier, SpeedTier};

/// 返回 `(fast_key, capable_key)`。规则:
/// - fast = 第一个名字判为 Fast 的 provider,无则回落 `default_provider`;
/// - capable = 若 `default_provider` 本身是 Capable 则用它,否则第一个 Capable,
///   再无则回落 `default_provider`。
/// key 排序后遍历以保证选择确定(prompt-cache / 可复现)。
pub fn resolve_tier_keys(config: &Config) -> (String, String) {
    let default = config.default_provider.clone();
    let mut keys: Vec<&String> = config.providers.keys().collect();
    keys.sort();

    let first_of = |tier: SpeedTier| -> Option<String> {
        keys.iter()
            .find(|k| model_speed_tier(&config.providers[**k].model) == tier)
            .map(|k| (*k).clone())
    };

    let fast = first_of(SpeedTier::Fast).unwrap_or_else(|| default.clone());

    let default_is_capable = config
        .providers
        .get(&default)
        .map(|pc| model_speed_tier(&pc.model) == SpeedTier::Capable)
        .unwrap_or(false);
    let capable = if default_is_capable {
        default.clone()
    } else {
        first_of(SpeedTier::Capable).unwrap_or_else(|| default.clone())
    };

    (fast, capable)
}

#[cfg(test)]
mod tests {
    use super::resolve_tier_keys;
    use atomcode_core::config::Config;
    use atomcode_core::config::provider::ProviderConfig;

    fn pc(model: &str) -> ProviderConfig {
        ProviderConfig {
            provider_type: "openai".into(),
            api_key: Some("sk-x".into()),
            model: model.into(),
            base_url: Some("https://gw.example/v1".into()),
            system_prompt: None,
            user_agent: None,
            context_window: 65536,
            max_tokens: None,
            thinking_type: None,
            thinking_keep: None,
            reasoning_history: None,
            reasoning_effort: None,
            thinking_enabled: None,
            thinking_budget: None,
            skip_tls_verify: false,
            ephemeral: false,
        }
    }

    #[test]
    fn two_models_split_by_tier() {
        let mut c = Config::default();
        c.providers.insert("p-ds".into(), pc("deepseek-v4-flash"));
        c.providers.insert("p-glm".into(), pc("GLM-5.2"));
        c.default_provider = "p-glm".into();
        let (fast, capable) = resolve_tier_keys(&c);
        assert_eq!(fast, "p-ds");
        assert_eq!(capable, "p-glm");
    }

    #[test]
    fn single_model_collapses_both_tiers() {
        let mut c = Config::default();
        c.providers.insert("p-ds".into(), pc("deepseek-v4-flash"));
        c.default_provider = "p-ds".into();
        let (fast, capable) = resolve_tier_keys(&c);
        assert_eq!(fast, "p-ds");
        assert_eq!(capable, "p-ds"); // no capable model → collapse to the only model
    }
}
