//! `atomcode import` — look at the assets another agent left behind.
//!
//! Read-only by default: it reports. `--apply` is the second phase, and it is
//! the only path that writes.

use atomcode_capabilities::migration::{
    self, ApiKeyRef, AssetKind, AssetStatus, MigrationReport, ProviderAccountDraft, ScanContext,
};
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

/// Import the provider accounts an external agent has configured.
///
/// **The signature is final** — Task 8 fills in the body without changing
/// it, so the caller never has to move. The second number is how many items
/// were left out on purpose; something left out is not a success.
pub fn apply_provider_accounts(
    _sources: &[Box<dyn atomcode_capabilities::migration::MigrationSource>],
    _ctx: &ScanContext,
    _yes: bool,
    _config_path: &std::path::Path,
    _interactive: bool,
) -> Result<(String, usize), String> {
    Err("provider import is not wired up yet".into())
}
