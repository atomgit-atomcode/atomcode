use atomcode_config::Config;

#[test]
fn obsolete_model_budgets_are_ignored_and_not_serialized() {
    let source = r#"
[providers.legacy]
type = "openai"
model = "legacy"
reasoning_timeout_secs = 0
[provider_accounts.custom]
provider = "openai-compatible"
[models."custom/slow"]
account = "custom"
model = "slow"
reasoning_timeout_secs = 900
"#;
    let config: Config = toml::from_str(source).unwrap();
    let saved = toml::to_string(&config).unwrap();
    assert!(!saved.contains("reasoning_timeout_secs"));
    let config: Config = toml::from_str(&saved).unwrap();
    for selection in ["legacy", "custom/slow"] {
        let model = config.resolve_model(Some(selection)).unwrap();
        let provider = serde_json::to_value(model.to_provider_config()).unwrap();
        assert!(provider.get("reasoning_timeout_secs").is_none());
    }
}
