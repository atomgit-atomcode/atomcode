//! `atomcode import` — look at the assets another agent left behind.
//!
//! Read-only by default: it reports. `--apply` is the second phase, and it is
//! the only path that writes.

use atomcode_capabilities::migration::{
    ApiKeyRef, AssetKind, AssetScope, AssetStatus, MigrationReport, ProviderAccountDraft,
    ScanContext,
};
use atomcode_config::provider_book::{AccountInput, BookError, ModelInput, ProviderBook};
use std::path::{Path, PathBuf};

/// The report, as text or as JSON.
pub fn render(report: &MigrationReport, json: bool) -> String {
    if json {
        return serde_json::to_string_pretty(report).unwrap_or_else(|e| e.to_string());
    }
    render_text(report)
}

/// The text form: one section per source, one line per asset.
///
/// Every sentence comes from the product message table, so the CLI and the
/// screen cannot end up saying the same thing two ways.
fn render_text(report: &MigrationReport) -> String {
    use atomcode_i18n::product::{t, Msg};
    if report.sources.is_empty() {
        return t(Msg::MigrationReportEmpty).into_owned();
    }
    let mut out = String::new();
    for source in &report.sources {
        out.push_str(&format!("{}\n", source.display_name));
        for asset in &source.assets {
            out.push_str(&format!(
                "  [{}] {} — {}\n",
                status_word(asset.status),
                asset.name,
                asset.detail
            ));
        }
    }
    out.push_str(&t(Msg::MigrationSummary {
        total: report.total_assets(),
        active: report.assets_with_status(AssetStatus::Active),
        imported: report.assets_with_status(AssetStatus::Imported),
        actionable: report.assets_with_status(AssetStatus::Actionable),
        blocked: report.assets_with_status(AssetStatus::Blocked),
    }));
    out
}

/// The word a status shows as. Localized: the report is read by a person.
fn status_word(status: AssetStatus) -> String {
    use atomcode_i18n::product::{t, Msg};
    match status {
        AssetStatus::Active => t(Msg::MigrationStatusActive).into_owned(),
        AssetStatus::Imported => t(Msg::MigrationStatusImported).into_owned(),
        AssetStatus::Actionable => t(Msg::MigrationStatusActionable).into_owned(),
        AssetStatus::Blocked => t(Msg::MigrationStatusBlocked).into_owned(),
    }
}

/// The re-check: mark the drafts whose equivalent is already in OUR config.
///
/// This is the "回检" half of Codex's loop. Without it a second run says
/// "can be brought over" about an account that is already there, and the
/// person has no way to tell "not yet imported" from "already imported but
/// different".
///
/// Compared: protocol, endpoint, model list, and the credential's KIND.
///
/// The credential is compared by KIND (a reference to a variable against a
/// reference to a variable), never by value: reading our own credential to
/// compare it would put a secret in memory for no reason, and the file's
/// credential may legitimately have been rotated since.
pub fn mark_already_imported(
    report: &mut MigrationReport,
    drafts: &[ProviderAccountDraft],
    config_path: &Path,
) {
    use atomcode_config::provider_book::ProviderBook;
    let config = ProviderBook::new(config_path.to_path_buf()).load();
    let accounts = &config.provider_accounts;
    let models = &config.models;
    for source in &mut report.sources {
        for asset in &mut source.assets {
            if asset.kind != AssetKind::ProviderConfig {
                continue;
            }
            // **The draft must come from THIS source.** Matching on the id
            // alone would let one source's row be judged by another
            // source's account: Claude's account and a Codex provider that
            // both land on the id `claude` would be confused, and one
            // source could be reported as "already imported" because of the
            // other's configuration.
            // **A row that is already `Blocked` is never upgraded.** The
            // blocked reason is the more specific truth: "we will not import
            // this" must not become "this is done".
            if asset.status == AssetStatus::Blocked {
                continue;
            }
            // **The draft must come from THIS source, and the draft must be
            // THE ONE this row is about.**
            //
            // Matching on the id alone would let one source's row be judged
            // by another source's account. But even id + source path is not
            // enough: two definitions in ONE file can normalize to the same
            // id (`"foo bar"` and `"foo-bar"` both become `foo-bar`), so
            // `.find()` would hand both rows the first draft — and a blocked
            // second definition would be judged by an unblocked first one.
            //
            // The row keeps the definition's ORIGINAL name, so the drafts
            // are matched on that too.
            let Some(draft) = drafts
                .iter()
                .find(|d| d.source_name == asset.name && d.source_path == asset.path)
            else {
                continue;
            };
            if let Some(reason) = unwriteable_credential(draft) {
                asset.status = AssetStatus::Blocked;
                asset.detail = reason;
                continue;
            }
            let Some(account) = accounts.get(&draft.id) else {
                continue;
            };
            if same_account(account, models, draft) {
                asset.status = AssetStatus::Imported;
                asset.detail = atomcode_i18n::product::t(
                    atomcode_i18n::product::Msg::MigrationAlreadyImported,
                )
                .into_owned();
            }
        }
    }
}

/// Whether OUR account is the same account as the draft.
///
/// Compared: protocol, endpoint, model list, and the credential's KIND.
///
/// The credential is never compared by value: the file's credential may
/// legitimately have been rotated since, and reading it to compare it would
/// put a secret in memory for no reason.
///
/// **The protocol is compared exactly.** `openai-compatible` is a real preset
/// id, not a wildcard: an account speaking the Responses API is a different
/// account from one speaking chat/completions, even at the same endpoint.
fn same_account(
    account: &atomcode_config::config::provider::ProviderAccountConfig,
    models: &std::collections::HashMap<
        String,
        atomcode_config::config::provider::ModelProfileConfig,
    >,
    draft: &ProviderAccountDraft,
) -> bool {
    // **A blocked draft is never "already imported".** A blocked draft is one
    // we REFUSE to import — an unsupported protocol, a name that cannot
    // become an account id, a definition with nothing to connect to. If our
    // config happened to contain a matching entry (someone made it by hand),
    // the row must stay `Blocked`: the re-check must not turn "we will not
    // import this" into "this is done".
    if draft.blocked.is_some() {
        return false;
    }
    // Both sides are the STORED form: the account's `base_url` is `None`
    // when it follows the protocol's default, and the draft's is `None` for
    // the same reason (`effective_endpoint`).
    let same_endpoint = account.base_url.as_deref() == draft.base_url.as_deref();
    let same_protocol = account.provider == draft.protocol;
    // **The model lists are compared as SETS, in both directions.** A
    // one-way check ("every draft model is in ours") is a CONTAINMENT test,
    // not an equivalence test: an account with the draft's models PLUS three
    // more would pass, and the report would call it "already imported" while
    // our config has models the source never had. The documents promise
    // equivalence, so the check must be equivalence.
    //
    // Model names are compared TRIMMED, because the writer trims them
    // (`plan_models`, `provider_book.rs:623`).
    let ours: std::collections::BTreeSet<String> = models
        .values()
        .filter(|m| m.account == draft.id)
        .map(|m| m.model.trim().to_string())
        .collect();
    let theirs: std::collections::BTreeSet<String> = draft
        .models
        .iter()
        .map(|model| model.trim().to_string())
        .collect();
    let same_models = ours == theirs;
    // The credential is compared by **KIND**, never by value: a reference to
    // a variable is the same kind of credential as another reference, and a
    // literal is the same kind as another literal — but the value itself is
    // never read, because the file's key may legitimately have been rotated
    // since, and reading it would put a secret in memory for no reason.
    let has_authorization = account
        .headers
        .iter()
        .any(|(name, _)| name == "authorization");
    let same_credential = match &draft.api_key {
        Some(ApiKeyRef::Bearer(_)) => has_authorization && account.api_key.is_none(),
        key => !has_authorization && same_credential_kind(account.api_key.as_deref(), key),
    };
    same_endpoint
        && same_protocol
        && same_models
        && same_credential
        && account.disable_api_key_fallback
}

/// Whether our stored credential is the same KIND as the draft's.
///
/// Our stored form is `${NAME}` for a reference and the secret itself for a
/// literal, so the kind is readable from the shape — without touching a
/// secret's value beyond looking at its first character.
fn same_credential_kind(stored: Option<&str>, draft: &Option<ApiKeyRef>) -> bool {
    let stored_is_reference = stored.is_some_and(|key| key.starts_with("${"));
    match draft {
        None => stored.is_none(),
        Some(ApiKeyRef::EnvVar(name)) => stored == Some(&format!("${{{name}}}")),
        Some(ApiKeyRef::Plaintext(_)) => stored.is_some() && !stored_is_reference,
        Some(ApiKeyRef::Bearer(_)) => false,
    }
}

/// Where to look. The host resolves these; the capability resolves nothing.
///
/// The product's trees come from the host too: the report needs to name a
/// target path, and only the host knows where our trees are.
pub fn scan_context(
    os_home: &Path,
    project: &Path,
    product: atomcode_capabilities::ProductDirs,
    config_path: &Path,
) -> ScanContext {
    let config = atomcode_config::config::Config::load(config_path).unwrap_or_default();
    let skill_dirs = atomcode_coding::parts::resolve_skill_dirs_from(
        &config.skills.extra_dirs,
        project,
        &product,
        Some(os_home),
        None,
    )
    .0;
    let opencode_dir = |var, default| {
        std::env::var_os(var)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| os_home.join(default))
            .join("opencode")
    };
    ScanContext {
        os_home: os_home.to_path_buf(),
        project: project.to_path_buf(),
        product,
        skill_dirs: Some(skill_dirs),
        opencode_config_dir: opencode_dir("XDG_CONFIG_HOME", ".config"),
        opencode_data_dir: opencode_dir("XDG_DATA_HOME", ".local/share"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_draft_with_an_env_named_key_is_written_as_a_reference() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let draft = atomcode_capabilities::migration::ProviderAccountDraft {
            id: "gateway".into(),
            protocol: "openai-compatible".into(),
            base_url: Some("https://gw.invalid/v1".into()),
            api_key: Some(atomcode_capabilities::migration::ApiKeyRef::EnvVar(
                "GW_TOKEN".into(),
            )),
            models: vec!["gpt-x".into()],
            source_path: "/home/u/.codex/config.toml".into(),
            scope: AssetScope::User,
            blocked: None,
            source_name: "gateway".into(),
        };
        let written = super::write_account(&config, &draft, true).expect("the account is written");
        let text = std::fs::read_to_string(&config).unwrap();
        // The reference form, not the bare name: a bare name falls through to
        // `resolved_api_key`'s literal case and would be used AS the key.
        assert!(text.contains("${GW_TOKEN}"), "{text}");
        assert!(!text.contains("sk-"), "no secret is written: {text}");
        assert_eq!(written, "gateway");
    }

    #[test]
    fn a_literal_that_would_be_expanded_is_refused() {
        // `resolved_api_key` expands any `$` in the value, so a key with a `$`
        // in it would mean something else at run time.
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let draft = atomcode_capabilities::migration::ProviderAccountDraft {
            id: "gateway".into(),
            protocol: "openai-compatible".into(),
            base_url: Some("https://gw.invalid/v1".into()),
            api_key: Some(atomcode_capabilities::migration::ApiKeyRef::Plaintext(
                "sk-$ecret".into(),
            )),
            models: vec![],
            source_path: "/home/u/.codex/config.toml".into(),
            scope: AssetScope::User,
            blocked: None,
            source_name: "gateway".into(),
        };
        let error = super::write_account(&config, &draft, true)
            .expect_err("a key with a dollar sign cannot be written");
        assert!(error.contains('$'), "{error}");
        assert!(
            !config.exists(),
            "nothing is written when the key cannot be written unambiguously"
        );
    }

    #[test]
    fn a_plaintext_key_is_not_written_without_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let draft = atomcode_capabilities::migration::ProviderAccountDraft {
            id: "gateway".into(),
            protocol: "openai-compatible".into(),
            base_url: Some("https://gw.invalid/v1".into()),
            api_key: Some(atomcode_capabilities::migration::ApiKeyRef::Plaintext(
                "sk-secret".into(),
            )),
            models: vec![],
            source_path: "/home/u/.codex/config.toml".into(),
            scope: AssetScope::User,
            blocked: None,
            source_name: "gateway".into(),
        };
        let error = super::write_account(&config, &draft, false).expect_err("a secret needs a yes");
        assert!(error.contains("confirm"), "{error}");
        assert!(
            !config.exists(),
            "nothing is written when the confirmation is missing"
        );
    }

    #[test]
    fn an_existing_account_id_is_a_conflict_and_does_not_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            "[provider_accounts.gateway]\nprovider = \"openai-compatible\"\n",
        )
        .unwrap();
        let draft = atomcode_capabilities::migration::ProviderAccountDraft {
            id: "gateway".into(),
            protocol: "openai-compatible".into(),
            base_url: Some("https://gw.invalid/v1".into()),
            api_key: None,
            models: vec![],
            source_path: "/home/u/.codex/config.toml".into(),
            scope: AssetScope::User,
            blocked: None,
            source_name: "gateway".into(),
        };
        let error = super::write_account(&config, &draft, true).expect_err("the id is taken");
        assert!(error.contains("gateway"), "{error}");
        let text = std::fs::read_to_string(&config).unwrap();
        assert!(
            !text.contains("gw.invalid"),
            "the existing account is untouched: {text}"
        );
    }

    #[test]
    fn project_overrides_require_an_interactive_yes_even_with_an_env_reference() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut ctx = ctx_for(home.path());
        ctx.project = project.path().to_owned();
        let user_config = home.path().join(".codex/config.toml");
        let project_config = project.path().join(".codex/config.toml");
        for path in [&user_config, &project_config] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        }
        std::fs::write(
            user_config,
            "[model_providers.gateway]\nbase_url = \"https://user.invalid/v1\"\nenv_key = \"USER_KEY\"\n",
        )
        .unwrap();
        std::fs::write(
            project_config,
            "[model_providers.gateway]\nbase_url = \"https://project.invalid/v1\"\n",
        )
        .unwrap();
        ctx.opencode_config_dir = home.path().join("opencode");
        let destination = home.path().join("destination.toml");
        let (_, skipped) = apply_provider_accounts(
            &atomcode_capabilities::migration::builtin_sources(),
            &ctx,
            true,
            &destination,
            false,
        )
        .unwrap();
        assert_eq!(skipped, 1);
        assert!(!destination.exists());
    }

    #[test]
    fn unreadable_sources_count_as_failures_even_when_another_account_is_written() {
        let home = tempfile::tempdir().unwrap();
        let mut ctx = ctx_for(home.path());
        ctx.project = home.path().join("project");
        ctx.opencode_config_dir = home.path().join("opencode");
        let broken = home.path().join(".codex/config.toml");
        std::fs::create_dir_all(broken.parent().unwrap()).unwrap();
        std::fs::write(&broken, "[model_providers.not-valid").unwrap();
        std::fs::create_dir_all(&ctx.opencode_config_dir).unwrap();
        std::fs::write(
            ctx.opencode_config_dir.join("opencode.json"),
            r#"{"provider":{"example":{"options":{"baseURL":"https://example.invalid/v1"},"models":{"demo":{}}}}}"#,
        )
        .unwrap();

        let destination = home.path().join("destination.toml");
        let (message, skipped) = apply_provider_accounts(
            &atomcode_capabilities::migration::builtin_sources(),
            &ctx,
            true,
            &destination,
            false,
        )
        .unwrap();
        assert_eq!(skipped, 1);
        assert!(message.contains(&broken.display().to_string()));
        let config = ProviderBook::new(destination).load();
        assert_eq!(config.logical_models()["example/demo"].model, "demo");
    }

    #[test]
    fn a_claude_bearer_token_stays_in_its_authorization_header() {
        let home = tempfile::tempdir().unwrap();
        let mut ctx = ctx_for(home.path());
        ctx.opencode_config_dir = home.path().join("opencode");
        let settings = home.path().join(".claude/settings.json");
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(
            settings,
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://gateway.invalid","ANTHROPIC_AUTH_TOKEN":"fixture-token"}}"#,
        )
        .unwrap();
        let sources = atomcode_capabilities::migration::builtin_sources();
        let (_, drafts) =
            atomcode_capabilities::migration::scan_with_provider_drafts(&sources, &ctx);
        let draft = drafts.iter().find(|draft| draft.id == "claude").unwrap();
        let destination = home.path().join("destination.toml");

        assert!(write_account(&destination, draft, false).is_err());
        assert!(!destination.exists());
        write_account(&destination, draft, true).unwrap();

        let config = ProviderBook::new(destination).load();
        let endpoint = config.account_endpoint("claude").unwrap();
        assert!(endpoint.api_key.is_none());
        assert_eq!(
            endpoint.headers.iter().collect::<Vec<_>>(),
            [("authorization", "Bearer fixture-token")]
        );
    }

    /// A source that hands back fixed drafts, so the `--apply` flow can be
    /// driven without a real `~/.claude`.
    struct FixedDrafts(Vec<atomcode_capabilities::migration::ProviderAccountDraft>);

    impl atomcode_capabilities::migration::MigrationSource for FixedDrafts {
        fn id(&self) -> &'static str {
            "fixed"
        }
        fn display_name(&self) -> &'static str {
            "Fixed drafts"
        }
        fn detect(
            &self,
            _ctx: &atomcode_capabilities::migration::ScanContext,
            _truth: &atomcode_capabilities::migration::SkillTruth,
        ) -> Vec<atomcode_capabilities::migration::AssetReport> {
            Vec::new()
        }
        fn scan_providers(
            &self,
            _ctx: &atomcode_capabilities::migration::ScanContext,
        ) -> atomcode_capabilities::migration::ProviderScan {
            atomcode_capabilities::migration::ProviderScan {
                drafts: self.0.clone(),
                ..Default::default()
            }
        }
    }

    fn draft(
        id: &str,
        key: Option<atomcode_capabilities::migration::ApiKeyRef>,
    ) -> atomcode_capabilities::migration::ProviderAccountDraft {
        atomcode_capabilities::migration::ProviderAccountDraft {
            id: id.into(),
            protocol: "openai-compatible".into(),
            base_url: Some("https://gw.invalid/v1".into()),
            api_key: key,
            models: vec![],
            source_path: "/home/u/.codex/config.toml".into(),
            scope: AssetScope::User,
            blocked: None,
            source_name: id.into(),
        }
    }

    fn ctx_for(dir: &std::path::Path) -> atomcode_capabilities::migration::ScanContext {
        atomcode_capabilities::migration::ScanContext {
            os_home: dir.to_path_buf(),
            project: dir.to_path_buf(),
            product: atomcode_capabilities::ProductDirs::new(dir.join(".ours"), ".ours"),
            skill_dirs: None,
            opencode_config_dir: PathBuf::new(),
            opencode_data_dir: PathBuf::new(),
        }
    }

    fn sources(
        drafts: Vec<atomcode_capabilities::migration::ProviderAccountDraft>,
    ) -> Vec<Box<dyn atomcode_capabilities::migration::MigrationSource>> {
        vec![Box::new(FixedDrafts(drafts))]
    }

    #[test]
    fn yes_does_not_authorize_writing_a_plaintext_secret() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let sources = sources(vec![draft(
            "gateway",
            Some(atomcode_capabilities::migration::ApiKeyRef::Plaintext(
                "sk-secret".into(),
            )),
        )]);
        // `--yes`, but no terminal to ask in: the secret is not written.
        let (_, skipped) =
            super::apply_provider_accounts(&sources, &ctx_for(dir.path()), true, &config, false)
                .expect("the run finishes");
        assert_eq!(skipped, 1, "the account that holds a secret was left out");
        assert!(!config.exists(), "the config file was not written at all");
    }

    #[test]
    fn an_account_that_only_names_a_variable_is_written_without_asking() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let sources = sources(vec![draft(
            "gateway",
            Some(atomcode_capabilities::migration::ApiKeyRef::EnvVar(
                "GW_TOKEN".into(),
            )),
        )]);
        let (_, skipped) =
            super::apply_provider_accounts(&sources, &ctx_for(dir.path()), true, &config, false)
                .expect("the run finishes");
        assert_eq!(skipped, 0, "nothing was left out");
        let text = std::fs::read_to_string(&config).unwrap();
        assert!(text.contains("${GW_TOKEN}"), "{text}");
        assert!(!text.contains("sk-"), "no secret was written: {text}");
    }

    #[test]
    fn a_batch_with_one_secret_writes_only_the_account_without_one() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let sources = sources(vec![
            draft(
                "with-secret",
                Some(atomcode_capabilities::migration::ApiKeyRef::Plaintext(
                    "sk-secret".into(),
                )),
            ),
            draft(
                "without-secret",
                Some(atomcode_capabilities::migration::ApiKeyRef::EnvVar(
                    "GW_TOKEN".into(),
                )),
            ),
        ]);
        let (_, skipped) =
            super::apply_provider_accounts(&sources, &ctx_for(dir.path()), true, &config, false)
                .expect("the run finishes");
        assert_eq!(skipped, 1, "the one that holds a secret was left out");
        let text = std::fs::read_to_string(&config).unwrap();
        assert!(text.contains("without-secret"), "{text}");
        assert!(
            !text.contains("with-secret"),
            "the account that holds a secret is not written: {text}"
        );
        assert!(!text.contains("sk-secret"), "no secret was written: {text}");
    }

    #[test]
    fn a_config_with_no_eligible_account_is_left_byte_for_byte() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let before = b"[provider_accounts.existing]\nprovider = \"openai-compatible\"\n";
        std::fs::write(&config, before).unwrap();
        let sources = sources(vec![draft(
            "gateway",
            Some(atomcode_capabilities::migration::ApiKeyRef::Plaintext(
                "sk-secret".into(),
            )),
        )]);
        let (_, skipped) =
            super::apply_provider_accounts(&sources, &ctx_for(dir.path()), true, &config, false)
                .expect("the run finishes");
        assert_eq!(skipped, 1);
        assert_eq!(
            std::fs::read(&config).unwrap(),
            before,
            "nothing was eligible, so the file is byte for byte what it was"
        );
    }

    use super::*;
    use atomcode_capabilities::migration::{
        AssetKind, AssetReport, AssetScope, AssetStatus, SourceReport,
    };

    fn sample() -> MigrationReport {
        MigrationReport {
            sources: vec![SourceReport {
                source_id: "claude-code",
                display_name: "Claude Code",
                assets: vec![
                    AssetReport {
                        kind: AssetKind::Skills,
                        name: "web-access".into(),
                        path: "/home/u/.claude/skills/web-access".into(),
                        scope: AssetScope::User,
                        status: AssetStatus::Active,
                        detail: "Read directly".into(),
                    },
                    AssetReport {
                        kind: AssetKind::Sessions,
                        name: "sessions".into(),
                        path: "/home/u/.claude/projects".into(),
                        scope: AssetScope::User,
                        status: AssetStatus::Blocked,
                        detail: "Lossy".into(),
                    },
                ],
            }],
        }
    }

    #[test]
    fn the_text_report_names_the_asset_and_its_state() {
        let _guard = atomcode_i18n::product::test_lock();
        atomcode_i18n::product::set_locale(atomcode_i18n::locale::Locale::En);
        let text = render(&sample(), false);
        assert!(text.contains("Claude Code"), "{text}");
        assert!(text.contains("web-access"), "{text}");
        // The state words come from the message table, so they are compared
        // against the table rather than against a literal in this file.
        assert!(
            text.contains(&*atomcode_i18n::product::t(
                atomcode_i18n::product::Msg::MigrationStatusActive
            )),
            "{text}"
        );
        assert!(
            text.contains(&*atomcode_i18n::product::t(
                atomcode_i18n::product::Msg::MigrationStatusBlocked
            )),
            "{text}"
        );
    }

    #[test]
    fn the_json_report_is_parseable_and_carries_the_same_rows() {
        let json = render(&sample(), true);
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(value["sources"][0]["display_name"], "Claude Code");
        assert_eq!(value["sources"][0]["assets"][0]["name"], "web-access");
        assert_eq!(value["sources"][0]["assets"][0]["status"], "active");
    }

    #[test]
    fn an_empty_report_says_so_instead_of_printing_nothing() {
        let _guard = atomcode_i18n::product::test_lock();
        atomcode_i18n::product::set_locale(atomcode_i18n::locale::Locale::En);
        let text = render(&MigrationReport::default(), false);
        assert!(!text.trim().is_empty());
        assert!(
            text.contains(&*atomcode_i18n::product::t(
                atomcode_i18n::product::Msg::MigrationReportEmpty
            )),
            "{text}"
        );
    }

    #[test]
    fn a_draft_already_in_our_config_is_marked_imported() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            "[provider_accounts.gateway]\nprovider = \"openai-compatible\"\n\
             base_url = \"https://gw.invalid/v1\"\ndisable_api_key_fallback = true\n",
        )
        .unwrap();

        let draft = ProviderAccountDraft {
            id: "gateway".into(),
            protocol: "openai-compatible".into(),
            base_url: Some("https://gw.invalid/v1".into()),
            api_key: None,
            models: vec![],
            source_path: "/home/u/.codex/config.toml".into(),
            scope: AssetScope::User,
            blocked: None,
            source_name: "gateway".into(),
        };
        let mut report = MigrationReport {
            sources: vec![atomcode_capabilities::migration::SourceReport {
                source_id: "codex",
                display_name: "Codex",
                assets: vec![atomcode_capabilities::migration::AssetReport {
                    kind: AssetKind::ProviderConfig,
                    name: "gateway".into(),
                    path: "/home/u/.codex/config.toml".into(),
                    scope: atomcode_capabilities::migration::AssetScope::User,
                    status: AssetStatus::Actionable,
                    detail: "can be imported".into(),
                }],
            }],
        };
        super::mark_already_imported(&mut report, &[draft], &config);
        let asset = &report.sources[0].assets[0];
        assert_eq!(
            asset.status,
            AssetStatus::Imported,
            "it is already there: {asset:?}"
        );
    }

    #[test]
    fn a_draft_that_differs_from_ours_stays_actionable() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            "[provider_accounts.gateway]\nprovider = \"openai-compatible\"\n\
             base_url = \"https://other.invalid/v1\"\n",
        )
        .unwrap();
        let draft = ProviderAccountDraft {
            id: "gateway".into(),
            protocol: "openai-compatible".into(),
            base_url: Some("https://gw.invalid/v1".into()),
            api_key: None,
            models: vec![],
            source_path: "/home/u/.codex/config.toml".into(),
            scope: AssetScope::User,
            blocked: None,
            source_name: "gateway".into(),
        };
        let mut report = MigrationReport {
            sources: vec![atomcode_capabilities::migration::SourceReport {
                source_id: "codex",
                display_name: "Codex",
                assets: vec![atomcode_capabilities::migration::AssetReport {
                    kind: AssetKind::ProviderConfig,
                    name: "gateway".into(),
                    path: "/home/u/.codex/config.toml".into(),
                    scope: atomcode_capabilities::migration::AssetScope::User,
                    status: AssetStatus::Actionable,
                    detail: "can be imported".into(),
                }],
            }],
        };
        super::mark_already_imported(&mut report, &[draft], &config);
        assert_eq!(
            report.sources[0].assets[0].status,
            AssetStatus::Actionable,
            "a different endpoint is not the same account"
        );
    }
}

fn unwriteable_credential(draft: &ProviderAccountDraft) -> Option<String> {
    use atomcode_i18n::product::{t, Msg};
    match &draft.api_key {
        Some(ApiKeyRef::Plaintext(secret)) if secret.trim().contains('$') => {
            Some(t(Msg::MigrationCredentialHasDollar).into_owned())
        }
        _ => None,
    }
}

/// Write the account and all its model profiles in one configuration transaction.
pub fn write_account(
    config_path: &Path,
    draft: &ProviderAccountDraft,
    confirmed: bool,
) -> Result<String, String> {
    if let Some(reason) = &draft.blocked {
        return Err(format!("{}: {reason}", draft.id));
    }
    if let Some(reason) = unwriteable_credential(draft) {
        return Err(format!("{}: {reason}", draft.id));
    }
    if requires_confirmation(draft) && !confirmed {
        return Err("confirm this account before writing it".into());
    }
    let mut headers = None;
    let key = match &draft.api_key {
        None => None,
        Some(ApiKeyRef::EnvVar(name)) => Some(format!("${{{name}}}")),
        Some(ApiKeyRef::Plaintext(secret)) => Some(secret.trim().to_owned()),
        Some(ApiKeyRef::Bearer(token)) => {
            let authorization = format!("Bearer {}", token.trim());
            let changes = atomcode_config::provider_headers::HeaderChanges(
                [("authorization".into(), Some(authorization))].into(),
            );
            changes.validate().map_err(str::to_owned)?;
            headers = Some(changes);
            None
        }
    };
    let base_url = draft.base_url.as_deref().unwrap_or_default();
    if base_url.is_empty() && draft.protocol == "openai-compatible" {
        return Err(format!("{}: no endpoint was found to import", draft.id));
    }
    let models: Vec<ModelInput<'_>> = draft
        .models
        .iter()
        .map(|model| ModelInput {
            account: "",
            model,
            id: None,
            display_name: None,
            window: None,
            max_tokens: None,
            vision: None,
            effort: None,
            levels: None,
            key: None,
            default: false,
        })
        .collect();
    let book = ProviderBook::new(config_path.to_path_buf());
    let input = AccountInput {
        headers: headers.as_ref(),
        name: &draft.id,
        protocol: &draft.protocol,
        display_name: None,
        endpoint: base_url,
        key: key.as_deref(),
        exact_id: true,
    };
    // An unsupported wire protocol is refused **before** the write.
    // `create_account` resolves an unknown preset to `openai-compatible`
    // (`config/provider_preset.rs:381`), so the import would otherwise
    // succeed — against the wrong wire format, and with the file now
    // claiming a protocol it does not speak.
    if atomcode_capabilities::migration::is_unsupported_protocol(&draft.protocol) {
        return Err(format!(
            "{}: this endpoint speaks a protocol we do not support ({}) — not imported",
            draft.id, draft.protocol
        ));
    }
    match book.create_imported_account(&input, &models) {
        Ok((id, _)) => Ok(id),
        Err(BookError::IdTaken(name)) => Err(format!(
            "{name}: an account with that id already exists — not overwritten"
        )),
        // `BookError` has no `Display` impl (each screen words its own), so it is
        // rendered by shape rather than by `to_string()`.
        Err(other) => Err(format!("{other:?}")),
    }
}

fn requires_confirmation(draft: &ProviderAccountDraft) -> bool {
    draft.scope == AssetScope::Project
        || draft
            .api_key
            .as_ref()
            .and_then(ApiKeyRef::literal)
            .is_some()
}

/// Import explicitly approved provider accounts and count the items left out.
pub fn apply_provider_accounts(
    sources: &[Box<dyn atomcode_capabilities::migration::MigrationSource>],
    ctx: &ScanContext,
    yes: bool,
    config_path: &Path,
    interactive: bool,
) -> Result<(String, usize), String> {
    use atomcode_i18n::product::{t, Msg};
    let mut drafts = Vec::new();
    let mut unreadable = Vec::new();
    for source in sources {
        let scan = source.scan_providers(ctx);
        drafts.extend(scan.drafts);
        unreadable.extend(
            scan.failures
                .into_iter()
                .map(|failure| format!("{} ({})", failure.path.display(), failure.detail)),
        );
    }
    if drafts.is_empty() && unreadable.is_empty() {
        return Ok((t(Msg::MigrationImportNone).into_owned(), 0));
    }
    // What is already in OUR configuration. The re-check and the apply share
    // this: an account that is already there is a **successful no-op**, not a
    // conflict — otherwise a second run would try to write it again, get
    // "the id is taken", and exit non-zero for something that is already done.
    let existing =
        atomcode_config::provider_book::ProviderBook::new(config_path.to_path_buf()).load();
    let mut written = Vec::new();
    let mut already = Vec::new();
    let mut skipped = Vec::new();
    for draft in &drafts {
        // A draft we REFUSE to import is refused FIRST — before the
        // existing-account check. Otherwise a hand-made account that happens
        // to match would be reported as "already there", and the real reason
        // (we cannot import this) would never be said.
        if let Some(reason) = draft
            .blocked
            .clone()
            .or_else(|| unwriteable_credential(draft))
        {
            skipped.push(format!("{} ({reason})", draft.id));
            continue;
        }
        if let Some(account) = existing.provider_accounts.get(&draft.id) {
            if same_account(account, &existing.models, draft) {
                already.push(draft.id.clone());
                continue;
            }
            // Same id, different account: a real conflict, not a no-op.
            skipped.push(format!(
                "{} ({})",
                draft.id,
                t(Msg::MigrationImportConflict { id: &draft.id })
            ));
            continue;
        }
        let confirmed = if yes && !requires_confirmation(draft) {
            true
        } else if interactive {
            ask_to_confirm(draft, config_path)?
        } else {
            if draft.scope == AssetScope::Project {
                skipped.push(format!(
                    "{} ({})",
                    draft.id,
                    t(Msg::MigrationProjectConfirmationRequired)
                ));
            } else {
                skipped.push(draft.id.clone());
            }
            continue;
        };
        if !confirmed {
            skipped.push(draft.id.clone());
            continue;
        }
        match write_account(config_path, draft, confirmed) {
            Ok(id) => written.push(id),
            Err(error) => skipped.push(format!("{} ({error})", draft.id)),
        }
    }
    let mut out = String::new();
    if !written.is_empty() {
        out.push_str(&t(Msg::MigrationImportDone {
            imported: &written.join(", "),
        }));
        out.push('\n');
    }
    if !already.is_empty() {
        out.push_str(&t(Msg::MigrationImportAlreadyThere {
            already: &already.join(", "),
        }));
        out.push('\n');
    }
    if !skipped.is_empty() {
        out.push_str(&t(Msg::MigrationImportSkipped {
            skipped: &skipped.join(", "),
        }));
        out.push('\n');
    }
    if !unreadable.is_empty() {
        out.push_str(&t(Msg::MigrationProvidersUnreadable {
            reasons: &unreadable.join(", "),
        }));
        out.push('\n');
    }
    Ok((out, skipped.len() + unreadable.len()))
}

/// Ask about one account, showing what would be written.
///
/// The question names the account, the endpoint, the protocol and the models,
/// and says that the key is a literal found in the other agent's config — so
/// the answer is about a specific thing, not a blanket yes.
fn ask_to_confirm(draft: &ProviderAccountDraft, config_path: &Path) -> Result<bool, String> {
    use atomcode_i18n::product::{t, Msg};
    use std::io::Write;

    // Configuration and path text may come from a project checkout.
    let display = |value: &str| {
        value
            .chars()
            .flat_map(char::escape_debug)
            .collect::<String>()
    };
    let endpoint = atomcode_capabilities::migration::endpoint_in_use(
        &draft.protocol,
        draft.base_url.as_deref(),
    );
    let credential = match &draft.api_key {
        Some(ApiKeyRef::Plaintext(_) | ApiKeyRef::Bearer(_)) => "plaintext".into(),
        Some(ApiKeyRef::EnvVar(name)) => format!("env:{}", display(name)),
        None => "none".into(),
    };
    print!(
        "{}",
        t(Msg::MigrationImportConfirm {
            id: &display(&draft.id),
            protocol: &display(&draft.protocol),
            endpoint: &display(endpoint.as_deref().unwrap_or("-")),
            models: &display(&draft.models.join(", ")),
            credential: &credential,
            destination: &display(&config_path.display().to_string()),
            source: &display(&draft.source_path.display().to_string()),
        })
    );
    std::io::stdout()
        .flush()
        .map_err(|error| error.to_string())?;
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("cannot read the answer: {e}"))?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}
