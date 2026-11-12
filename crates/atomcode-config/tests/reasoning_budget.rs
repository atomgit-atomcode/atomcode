use atomcode_config::Config;

#[test]
fn model_budgets_round_trip_through_resolution_and_legacy_projection() {
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
    let config: Config = toml::from_str(&saved).unwrap();
    for (selection, seconds) in [("legacy", 0), ("custom/slow", 900)] {
        let model = config.resolve_model(Some(selection)).unwrap();
        assert_eq!(model.reasoning_timeout_secs, Some(seconds));
        assert_eq!(
            model.to_provider_config().reasoning_timeout_secs,
            Some(seconds)
        );
    }
}

#[test]
fn a_negative_model_budget_is_rejected_instead_of_disabling_the_limit() {
    assert!(toml::from_str::<Config>(
        "[providers.legacy]\ntype='openai'\nmodel='legacy'\nreasoning_timeout_secs=-1"
    )
    .is_err());
}
