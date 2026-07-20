//! Resolve the `task` subagent's fast / capable tier keys from `Config.providers`, ranked by
//! each provider's `capable_model` (higher = more capable). A provider WITHOUT `capable_model`
//! set does not participate. Routing engages only when the HOST model itself participates AND
//! there are ≥2 participants; otherwise both tiers collapse to the host's own key so the
//! subagent runs on the current model (the self-configured / single-model case).

use atomcode_config::config::Config;

/// Resolve `Some((fast_key, capable_key))` for the subagent tiers, given the current
/// `host_model`, or `None` when routing should NOT engage (⇒ the subagent uses the current
/// host provider for both tiers).
/// - Only providers with `capable_model` set participate; higher rank ⇒ more capable.
/// - `fast` = the lowest-ranked participant, `capable` = the highest-ranked.
/// - Returns `None` when the host model doesn't itself participate (a self-configured model),
///   or when there are fewer than 2 participants. Returning `None` (rather than the host's
///   key) avoids a subtle bug: if the host model isn't in `providers`, there is no reliable
///   "host key" to collapse to.
/// Deterministic: ties in rank are broken by provider key.
pub fn resolve_tier_keys(config: &Config, host_model: &str) -> Option<(String, String)> {
    // A self-configured host (no `capable_model`) never routes, even if other providers
    // carry a rank.
    let host_participates = config
        .providers
        .values()
        .any(|pc| pc.model == host_model && pc.capable_model.is_some());
    if !host_participates {
        return None;
    }

    // Rank the participating providers (ascending capability; ties broken by key).
    let mut ranked: Vec<(&String, i64)> = config
        .providers
        .iter()
        .filter_map(|(k, pc)| pc.capable_model.map(|c| (k, c)))
        .collect();
    if ranked.len() < 2 {
        return None;
    }
    ranked.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(b.0)));
    Some((
        ranked.first().unwrap().0.clone(),
        ranked.last().unwrap().0.clone(),
    ))
}

#[cfg(test)]
mod tests {
    use super::resolve_tier_keys;
    use atomcode_config::config::provider::ProviderConfig;
    use atomcode_config::config::Config;

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
        c.providers
            .insert("p-ds".into(), pc("deepseek-v4-flash", Some(0)));
        c.providers.insert("p-glm".into(), pc("GLM-5.2", Some(1)));
        c.default_provider = "p-glm".into();
        // host = GLM (participates, highest rank) ⇒ fast = deepseek (min), capable = GLM (max).
        assert_eq!(
            resolve_tier_keys(&c, "GLM-5.2"),
            Some(("p-ds".to_string(), "p-glm".to_string()))
        );
    }

    #[test]
    fn self_config_host_does_not_route() {
        // Host is a self-configured model (no capable_model) even though AtomGit models WITH
        // capable_model are also present ⇒ None (subagent uses the current model).
        let mut c = Config::default();
        c.providers
            .insert("mine".into(), pc("my-local-model", None));
        c.providers
            .insert("p-ds".into(), pc("deepseek-v4-flash", Some(0)));
        c.providers.insert("p-glm".into(), pc("GLM-5.2", Some(1)));
        c.default_provider = "mine".into();
        assert_eq!(resolve_tier_keys(&c, "my-local-model"), None);
    }

    #[test]
    fn single_participant_does_not_route() {
        let mut c = Config::default();
        c.providers.insert("p-glm".into(), pc("GLM-5.2", Some(1)));
        c.default_provider = "p-glm".into();
        assert_eq!(resolve_tier_keys(&c, "GLM-5.2"), None);
    }

    #[test]
    fn three_participants_use_rank_extremes() {
        let mut c = Config::default();
        c.providers.insert("a".into(), pc("m-a", Some(0)));
        c.providers.insert("b".into(), pc("m-b", Some(5)));
        c.providers.insert("c".into(), pc("m-c", Some(2)));
        c.default_provider = "b".into();
        // lowest rank = fast, highest = capable; middle ignored.
        assert_eq!(
            resolve_tier_keys(&c, "m-b"),
            Some(("a".to_string(), "b".to_string()))
        );
    }

    #[test]
    fn no_participants_does_not_route() {
        let mut c = Config::default();
        c.providers.insert("mine".into(), pc("x", None));
        c.default_provider = "mine".into();
        assert_eq!(resolve_tier_keys(&c, "x"), None);
    }
}
