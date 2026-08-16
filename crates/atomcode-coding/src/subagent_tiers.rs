//! Resolve the `task` subagent's fast / capable tier selection ids from the unified model
//! catalog (design §14.2), ranked by each model profile's `capable_model` (higher = more
//! capable). A model WITHOUT `capable_model` set does not participate. Routing engages only
//! when the HOST model itself participates AND there are ≥2 participants; otherwise both tiers
//! collapse to the host so the subagent runs on the current model (the self-configured /
//! single-model case). Legacy `[providers.*]` project to catalog models of the same id, so
//! this is behavior-identical to the old provider-map ranking for legacy configs.

use atomcode_config::config::Config;

/// Resolve `Some((fast_id, capable_id))` — model-selection ids for the subagent tiers, given
/// the current `host_model`, or `None` when routing should NOT engage (⇒ the subagent uses the
/// current host model for both tiers).
/// - Only catalog models with `capable_model` set participate; higher rank ⇒ more capable.
/// - `fast` = the lowest-ranked participant, `capable` = the highest-ranked.
/// - Returns `None` when the host model doesn't itself participate (a self-configured model),
///   or when there are fewer than 2 participants.
/// Deterministic: ties in rank are broken by selection id.
pub fn resolve_tier_keys(config: &Config, host_model: &str) -> Option<(String, String)> {
    let models = config.logical_models();
    // A self-configured host (no `capable_model`) never routes, even if other models carry a rank.
    let host_participates = models
        .values()
        .any(|m| m.model == host_model && m.capable_model.is_some());
    if !host_participates {
        return None;
    }

    // Rank the participating models (ascending capability; ties broken by selection id).
    let mut ranked: Vec<(&String, i64)> = models
        .iter()
        .filter_map(|(id, m)| m.capable_model.map(|c| (id, c)))
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
            supports_vision: None,
            user_agent: None,
            context_window: 65536,
            max_tokens: None,
            thinking_type: None,
            thinking_keep: None,
            reasoning_history: None,
            reasoning_effort: None,
            reasoning_effort_levels: None,
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

    #[test]
    fn new_schema_routes_by_per_model_capable_rank_on_one_account() {
        // §14.2: two model profiles on the SAME account, ranked per-model. Tiers
        // resolve to model-selection ids — no duplicated connection settings.
        let c: Config = serde_json::from_value(serde_json::json!({
            "default_model": "acc/cap",
            "provider_accounts": { "acc": { "provider": "deepseek", "api_key": "sk" } },
            "models": {
                "acc/fast": { "account": "acc", "model": "deepseek-flash", "context_window": 131072, "capable_model": 0 },
                "acc/cap":  { "account": "acc", "model": "deepseek-max",   "context_window": 131072, "capable_model": 5 }
            }
        }))
        .unwrap();
        assert_eq!(
            resolve_tier_keys(&c, "deepseek-max"),
            Some(("acc/fast".to_string(), "acc/cap".to_string()))
        );
    }
}
