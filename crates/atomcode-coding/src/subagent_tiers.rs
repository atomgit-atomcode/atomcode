//! Resolve the `task` subagent's fast / capable tier keys from `Config.providers`, ranked by
//! each provider's `capable_model` (higher = more capable). A provider WITHOUT `capable_model`
//! set does not participate. Routing engages only when the HOST model itself participates AND
//! there are ≥2 participants; otherwise both tiers collapse to the host's own key so the
//! subagent runs on the current model (the self-configured / single-model case).

use atomcode_core::config::Config;

/// Resolve `(fast_key, capable_key)` for the subagent tiers, given the current `host_model`.
/// - Only providers with `capable_model` set participate; higher rank ⇒ more capable.
/// - `fast` = the lowest-ranked participant, `capable` = the highest-ranked.
/// - If the host doesn't participate, or there are fewer than 2 participants, BOTH tiers
///   collapse to the host provider's own key (⇒ the bridge's `tier_builder` returns `None`
///   ⇒ the subagent reuses the host provider).
/// Deterministic: ties in rank are broken by provider key.
pub fn resolve_tier_keys(config: &Config, host_model: &str) -> (String, String) {
    // The key of the provider that IS the current host (its model matches `host_model`).
    // Returning this for both tiers makes `tier_builder` collapse to the host slot.
    let host_key = config
        .providers
        .iter()
        .find(|(_, pc)| pc.model == host_model)
        .map(|(k, _)| k.clone())
        .unwrap_or_else(|| config.default_provider.clone());

    // Does the host model itself opt into auto-routing? (A self-configured host — no
    // `capable_model` — never routes, even if other providers carry a rank.)
    let host_participates = config
        .providers
        .values()
        .any(|pc| pc.model == host_model && pc.capable_model.is_some());

    // Rank the participating providers (ascending capability; ties broken by key).
    let mut ranked: Vec<(&String, i64)> = config
        .providers
        .iter()
        .filter_map(|(k, pc)| pc.capable_model.map(|c| (k, c)))
        .collect();
    ranked.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(b.0)));

    if !host_participates || ranked.len() < 2 {
        return (host_key.clone(), host_key);
    }
    let fast = ranked.first().unwrap().0.clone();
    let capable = ranked.last().unwrap().0.clone();
    (fast, capable)
}

#[cfg(test)]
mod tests {
    use super::resolve_tier_keys;
    use atomcode_core::config::provider::ProviderConfig;
    use atomcode_core::config::Config;

    fn pc(model: &str, capable: Option<i64>) -> ProviderConfig {
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
            capable_model: capable,
        }
    }

    #[test]
    fn routes_by_capable_rank_when_host_participates() {
        let mut c = Config::default();
        c.providers.insert("p-ds".into(), pc("deepseek-v4-flash", Some(0)));
        c.providers.insert("p-glm".into(), pc("GLM-5.2", Some(1)));
        c.default_provider = "p-glm".into();
        // host = GLM (participates, highest rank) ⇒ fast = deepseek (min), capable = GLM (max).
        let (fast, capable) = resolve_tier_keys(&c, "GLM-5.2");
        assert_eq!(fast, "p-ds");
        assert_eq!(capable, "p-glm");
    }

    #[test]
    fn self_config_host_collapses_to_current_model() {
        // Host is a self-configured model (no capable_model) even though AtomGit models WITH
        // capable_model are also present ⇒ subagent uses the current model (both tiers = host key).
        let mut c = Config::default();
        c.providers.insert("mine".into(), pc("my-local-model", None));
        c.providers.insert("p-ds".into(), pc("deepseek-v4-flash", Some(0)));
        c.providers.insert("p-glm".into(), pc("GLM-5.2", Some(1)));
        c.default_provider = "mine".into();
        let (fast, capable) = resolve_tier_keys(&c, "my-local-model");
        assert_eq!(fast, "mine");
        assert_eq!(capable, "mine");
    }

    #[test]
    fn single_participant_collapses_to_host() {
        let mut c = Config::default();
        c.providers.insert("p-glm".into(), pc("GLM-5.2", Some(1)));
        c.default_provider = "p-glm".into();
        let (fast, capable) = resolve_tier_keys(&c, "GLM-5.2");
        assert_eq!(fast, "p-glm");
        assert_eq!(capable, "p-glm");
    }

    #[test]
    fn three_participants_use_rank_extremes() {
        let mut c = Config::default();
        c.providers.insert("a".into(), pc("m-a", Some(0)));
        c.providers.insert("b".into(), pc("m-b", Some(5)));
        c.providers.insert("c".into(), pc("m-c", Some(2)));
        c.default_provider = "b".into();
        let (fast, capable) = resolve_tier_keys(&c, "m-b");
        assert_eq!(fast, "a", "lowest rank is fast");
        assert_eq!(capable, "b", "highest rank is capable");
    }

    #[test]
    fn no_participants_collapses_to_host() {
        let mut c = Config::default();
        c.providers.insert("mine".into(), pc("x", None));
        c.default_provider = "mine".into();
        let (fast, capable) = resolve_tier_keys(&c, "x");
        assert_eq!(fast, "mine");
        assert_eq!(capable, "mine");
    }
}
