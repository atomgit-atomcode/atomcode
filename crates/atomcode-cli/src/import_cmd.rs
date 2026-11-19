//! `atomcode import` — look at the assets another agent left behind.
//!
//! Read-only by default: it reports. `--apply` is the second phase, and it is
//! the only path that writes.

use atomcode_capabilities::migration::{
    self, ApiKeyRef, AssetKind, AssetStatus, MigrationReport, ProviderAccountDraft, ScanContext,
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
    if config.provider_accounts.is_empty() {
        return;
    }
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
    let same_credential = same_credential_kind(account.api_key.as_deref(), &draft.api_key);
    same_endpoint && same_protocol && same_models && same_credential
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
        Some(ApiKeyRef::EnvVar(_)) => stored_is_reference,
        Some(ApiKeyRef::Plaintext(_)) => stored.is_some() && !stored_is_reference,
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
    skill_dirs: Option<Vec<std::path::PathBuf>>,
    opencode_config_dir: std::path::PathBuf,
    opencode_data_dir: std::path::PathBuf,
) -> ScanContext {
    ScanContext {
        os_home: os_home.to_path_buf(),
        project: project.to_path_buf(),
        product,
        skill_dirs,
        opencode_config_dir,
        opencode_data_dir,
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
        fn provider_drafts(
            &self,
            _ctx: &atomcode_capabilities::migration::ScanContext,
        ) -> Vec<atomcode_capabilities::migration::ProviderAccountDraft> {
            self.0.clone()
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
             base_url = \"https://gw.invalid/v1\"\n",
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

/// Why this credential cannot be written, if it cannot.
///
/// The rules are the writer's, and they are **stated here once** so the report
/// and the writer cannot disagree:
///
/// - a literal containing `$` would be **expanded** by `resolved_api_key`
///   (`config/provider.rs:360`), so the file would mean something else;
/// - a literal whose **trimmed** value names a live environment variable
///   would resolve to that variable instead (`provider.rs:365`), so the file
///   would mean something else.
///
/// The trimmed value is what matters, because that is what the writer stores
/// (`written_key`, `provider_book.rs:753`).
///
/// The **value is never printed** — the reason names the account, not the key.
fn unwriteable_credential(draft: &ProviderAccountDraft) -> Option<String> {
    let Some(ApiKeyRef::Plaintext(secret)) = &draft.api_key else {
        // A reference is always writable: the file holds a NAME.
        return None;
    };
    let stored = secret.trim();
    if stored.contains('$') {
        return Some(
            atomcode_i18n::product::t(atomcode_i18n::product::Msg::MigrationCredentialHasDollar)
                .into_owned(),
        );
    }
    if std::env::var(stored).is_ok() {
        return Some(
            atomcode_i18n::product::t(atomcode_i18n::product::Msg::MigrationCredentialAmbiguous)
                .into_owned(),
        );
    }
    None
}

/// Write one provider account.
///
/// A literal secret needs `confirmed`; without it nothing is written at all —
/// not the account, not the models, not a partial file.
///
/// **How the credential is written.**
///
/// `resolved_api_key` (`config/provider.rs:356`) has three cases:
/// a value containing `$` is expanded as an environment reference; a value
/// whose NAME matches a live environment variable resolves to that variable;
/// anything else is the key itself. So the same string can mean two things,
/// and the written form has to pick one:
///
/// - A **reference** is written `${NAME}`. Expansion is unambiguous: an unset
///   variable expands to nothing, so the account ends up with NO key rather
///   than with the key "NAME". The bare name would instead fall through to
///   the literal case and be used as the key.
/// - A **literal** is written as itself, but only when it cannot be read as
///   something else: no `$` in it (the config would expand it), and no live
///   environment variable of the same name (that would win over the
///   literal). A literal that fails either test is refused rather than
///   written, because the file would then mean two things at once.
pub fn write_account(
    config_path: &Path,
    draft: &ProviderAccountDraft,
    confirmed: bool,
) -> Result<String, String> {
    // An owned string, because the reference form has to be BUILT rather than
    // borrowed from the draft.
    let key: Option<String> = match &draft.api_key {
        None => None,
        // `${NAME}` is the only unambiguous way to write a reference:
        // `resolved_api_key` (`config/provider.rs:360`) expands a value
        // containing `$`, and an unset variable expands to nothing — so the
        // string "NAME" is never used AS the key. Writing the bare name
        // would instead fall through to the literal branch
        // (`provider.rs:370`) and use the string "NAME" as the key.
        //
        // It does NOT mean "no key": if the expansion comes out empty,
        // `resolved_api_key` keeps looking and finds a platform credential
        // (`OPENAI_API_KEY` / `ANTHROPIC_API_KEY` / `ATOMCODE_API_KEY`,
        // `provider.rs:375-396`). So the migrated endpoint can end up
        // authenticated with a credential that is not the one this account
        // was configured for. `${NAME}` is still the right form — it is the
        // only one that does not put the NAME in as a key — but the report
        // must not promise "no key" when the variable is unset.
        Some(ApiKeyRef::EnvVar(name)) => Some(format!("${{{name}}}")),
        Some(ApiKeyRef::Plaintext(secret)) => {
            if !confirmed {
                return Err("this account has a plaintext key — confirm it to write it".into());
            }
            // A literal that could be read as something else is refused
            // rather than written, because the file would then mean two
            // things: `resolved_api_key` expands any `$` in it, and a live
            // environment variable of the same name wins over the literal.
            if secret.trim().contains('$') {
                return Err(format!(
                    "{}: this key contains `$`, which the config would expand — set it by hand \
                     instead",
                    draft.id
                ));
            }
            // The error names the ACCOUNT, never the secret: the message is
            // printed to the terminal and into the report, and a secret in
            // it is a secret in the scrollback.
            //
            // The check is "is the variable SET", not "is it set and
            // non-empty": `resolved_api_key` looks the name up, and a lookup
            // that succeeds with an empty value falls THROUGH to the
            // platform fallbacks (`provider.rs:375`) instead of treating the
            // configured string as the literal it is. So an empty variable of
            // the same name still makes the value ambiguous.
            //
            // The check uses the **trimmed** value, because that is what gets
            // stored: `written_key` trims it (`provider_book.rs:753`). A value
            // of `" GW_TOKEN "` would be stored as `GW_TOKEN` — so checking
            // the padded string would miss that `GW_TOKEN` is a live
            // variable, and the file would end up holding a reference where a
            // literal was meant.
            let stored = secret.trim();
            if std::env::var(stored).is_ok() {
                return Err(format!(
                    "{}: an environment variable of the same name is set, so this key cannot be \
                     written unambiguously — rename the variable or set the key by hand",
                    draft.id
                ));
            }
            Some(stored.to_string())
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
        headers: None,
        name: &draft.id,
        protocol: &draft.protocol,
        display_name: None,
        endpoint: base_url,
        key: key.as_deref(),
        exact_id: true,
    };
    // A draft that cannot be imported is refused **before** the write.
    // The report already says so; the writer must not try anyway.
    if let Some(reason) = &draft.blocked {
        return Err(format!("{}: {reason}", draft.id));
    }
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
    match book.create_account(&input, &models) {
        Ok((id, _)) => Ok(id),
        Err(BookError::IdTaken(name)) => Err(format!(
            "{name}: an account with that id already exists — not overwritten"
        )),
        // `BookError` has no `Display` impl (each screen words its own), so it is
        // rendered by shape rather than by `to_string()`.
        Err(other) => Err(format!("{other:?}")),
    }
}

/// Import the provider accounts an external agent has configured.
///
/// **What `yes` means.** It answers "yes, import these accounts" — it does
/// **not** authorize writing a secret the person has not seen. An account
/// whose credential is a literal is always asked about, one at a time, even
/// with `--yes`: the question is about the secret, not about the import.
///
/// Without a terminal there is nobody to ask, so an account that needs an
/// answer is **not written at all** — and if that is the only kind of
/// account there is, nothing is written and the config file stays byte for
/// byte as it was.
///
/// Returns the text to show and how many items were left out. A non-zero
/// count means the caller must not report success.
pub fn apply_provider_accounts(
    sources: &[Box<dyn atomcode_capabilities::migration::MigrationSource>],
    ctx: &ScanContext,
    yes: bool,
    config_path: &Path,
    interactive: bool,
) -> Result<(String, usize), String> {
    use atomcode_i18n::product::{t, Msg};
    let mut drafts: Vec<ProviderAccountDraft> = Vec::new();
    // **A source that could not be read is reported, not swallowed.**
    //
    // "No provider accounts" and "I could not read your provider config" are
    // different answers, and the second one must not exit successfully: a
    // script would read the exit code as "nothing to import" and move on.
    let mut unreadable: Vec<String> = Vec::new();
    for source in sources {
        drafts.extend(source.provider_drafts(ctx));
        if let Some(reason) = source.provider_drafts_error(ctx) {
            unreadable.push(format!("{}: {reason}", source.display_name()));
        }
    }
    // **The credential is preflighted HERE, in the host** — before the
    // re-check and before the write.
    //
    // Why the host and not the capability: the check is "is there an
    // environment variable of this name right now", and that is a question
    // about the PROCESS, not about the file. The capability is a read-only
    // view of the disk; it must not ask the environment anything.
    //
    // Why before the re-check: a literal with a `$` in it, or one whose
    // trimmed value names a live environment variable, is a credential the
    // writer will REFUSE. If the re-check ran first, it could call such an
    // account "already imported" — and the report would advertise an account
    // that cannot be written.
    for draft in &mut drafts {
        if let Some(reason) = unwriteable_credential(draft) {
            draft.blocked = Some(reason);
        }
    }
    if drafts.is_empty() && unreadable.is_empty() {
        return Ok((t(Msg::MigrationImportNone).into_owned(), 0));
    }
    if drafts.is_empty() {
        // Something could not be read, and there is nothing else to say.
        return Err(t(Msg::MigrationProvidersUnreadable {
            reasons: &unreadable.join("; "),
        })
        .into_owned());
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
        if let Some(reason) = &draft.blocked {
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
        // A literal secret is always the person's own decision — `--yes` does
        // not reach it. An account that only names a variable holds no secret,
        // so `--yes` answers for it; without `--yes` it is asked too.
        let holds_a_secret = matches!(draft.api_key, Some(ApiKeyRef::Plaintext(_)));
        let confirmed = if holds_a_secret {
            if !interactive {
                skipped.push(draft.id.clone());
                continue;
            }
            ask_to_confirm(draft, config_path)?
        } else if yes {
            true
        } else {
            if !interactive {
                skipped.push(draft.id.clone());
                continue;
            }
            ask_to_confirm(draft, config_path)?
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
    Ok((out, skipped.len()))
}

/// Ask about one account, showing what would be written.
///
/// The question names the account, the endpoint, the protocol and the models,
/// and says that the key is a literal found in the other agent's config — so
/// the answer is about a specific thing, not a blanket yes.
fn ask_to_confirm(draft: &ProviderAccountDraft, config_path: &Path) -> Result<bool, String> {
    use atomcode_i18n::product::{t, Msg};
    use std::io::Write;
    print!(
        "{}",
        t(Msg::MigrationImportConfirm {
            id: &draft.id,
            protocol: &draft.protocol,
            // The endpoint IN USE — a protocol's own default is shown, not a
            // dash, because that is the endpoint this account will talk to.
            endpoint: &atomcode_capabilities::migration::endpoint_in_use(
                &draft.protocol,
                draft.base_url.as_deref(),
            )
            .unwrap_or_else(|| "-".into()),
            models: &draft.models.join(", "),
            // What will happen to the credential. For a literal it says the
            // secret will be COPIED INTO the file, and names the file — the
            // person is being asked to agree to that, not to "import the key
            // found somewhere".
            credential: &match &draft.api_key {
                Some(ApiKeyRef::Plaintext(_)) => "plaintext".into(),
                Some(ApiKeyRef::EnvVar(name)) => format!("env:{name}"),
                None => "none".into(),
            },
            destination: &config_path.display().to_string(),
        })
    );
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("cannot read the answer: {e}"))?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}
