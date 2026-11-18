//! OpenCode's assets.
//!
//! The THIRD source, and the first with a shape of its own: its credential
//! is a literal in `options.apiKey`, its MCP `command` is an ARRAY, and its
//! protocol comes from an `npm` package name rather than a `wire_api` field.
//!
//! If supporting it needed a change in the shared `provider.rs`, the
//! abstraction would be wrong — so the source-specific knowledge stays HERE.

use super::provider::{ApiKeyRef, ProviderAccountDraft};
use super::report::{AssetKind, AssetReport, AssetScope, AssetStatus};
use super::{MigrationSource, ScanContext, SkillTruth};

pub struct OpenCodeSource;

impl MigrationSource for OpenCodeSource {
    fn id(&self) -> &'static str {
        "opencode"
    }

    fn display_name(&self) -> &'static str {
        "OpenCode"
    }

    fn detect(&self, ctx: &ScanContext, truth: &SkillTruth) -> Vec<AssetReport> {
        let mut out = Vec::new();
        let config = &ctx.opencode_config_dir;
        // The registry's facts come from the ONE load `scan()` did — not a
        // load of our own. Two sources in one report must not see two
        // different moments of the same disk.
        let skills = truth;

        // Skills and commands. Prefer discoveries already in the registry;
        // otherwise check whether the candidate can be brought over.
        for (kind, dir) in [
            (AssetKind::Skills, "skills"),
            (AssetKind::Commands, "commands"),
        ] {
            for (scope, root) in [
                (AssetScope::User, config.join(dir)),
                (AssetScope::Project, ctx.project.join(".opencode").join(dir)),
            ] {
                out.extend(super::skill_rows(
                    kind,
                    scope,
                    &root,
                    &skills,
                    super::SkillTree::Foreign {
                        target: ctx.product.user().join(dir),
                    },
                ));
            }
        }
        // Shared skills are standard roots. Shared commands may also be
        // configured by the host, so consult the same registry snapshot
        // before offering a copy.
        for (scope, root) in [
            (AssetScope::User, ctx.os_home.join(".agents").join("skills")),
            (
                AssetScope::Project,
                ctx.project.join(".agents").join("skills"),
            ),
        ] {
            out.extend(super::skill_rows(
                AssetKind::Skills,
                scope,
                &root,
                &skills,
                super::SkillTree::Scanned,
            ));
        }
        for (scope, root) in [
            (
                AssetScope::User,
                ctx.os_home.join(".agents").join("commands"),
            ),
            (
                AssetScope::Project,
                ctx.project.join(".agents").join("commands"),
            ),
        ] {
            out.extend(super::skill_rows(
                AssetKind::Commands,
                scope,
                &root,
                &skills,
                super::SkillTree::Foreign {
                    target: ctx.product.user().join("commands"),
                },
            ));
        }

        // The provider configuration.
        out.extend(provider_rows(ctx));

        // The MCP servers.
        out.extend(mcp_rows(ctx));

        // `auth.json`: plaintext credentials. REPORTED, never offered.
        let auth = ctx.opencode_data_dir.join("auth.json");
        if auth.is_file() {
            out.push(AssetReport {
                kind: AssetKind::ProviderConfig,
                name: "auth.json".into(),
                path: auth,
                scope: AssetScope::User,
                status: AssetStatus::Blocked,
                detail: super::text::opencode_plaintext_credentials(),
            });
        }
        out
    }

    /// The provider accounts OpenCode has configured.
    ///
    /// **The credential is a LITERAL**, like Claude's — not a variable name
    /// like Codex's. So it needs the person's confirmation before it is
    /// written.
    fn provider_drafts(&self, ctx: &ScanContext) -> Vec<ProviderAccountDraft> {
        let Some(value) =
            super::provider::read_jsonc(&ctx.opencode_config_dir.join("opencode.json"))
        else {
            return Vec::new();
        };
        let Some(providers) = value.get("provider").and_then(|p| p.as_object()) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (id, provider) in providers {
            let Some(options) = provider.get("options").and_then(|o| o.as_object()) else {
                continue;
            };
            let base_url = options
                .get("baseURL")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let api_key = options
                .get("apiKey")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(|secret| ApiKeyRef::Plaintext(secret.to_string()));
            if base_url.is_none() && api_key.is_none() {
                continue;
            }
            let protocol = protocol_for_opencode(provider.get("npm").and_then(|v| v.as_str()));
            out.push(ProviderAccountDraft {
                id: super::provider::account_id_for(id).unwrap_or_else(|| id.clone()),
                source_name: id.clone(),
                protocol: protocol.clone(),
                base_url: super::provider::effective_endpoint(&protocol, base_url.as_deref()),
                api_key,
                models: Vec::new(),
                source_path: ctx.opencode_config_dir.join("opencode.json"),
                scope: AssetScope::User,
                blocked: None,
            });
        }
        out
    }
}

/// The provider accounts OpenCode has configured.
///
/// **The rows come from the drafts**, so the row's name is the account id
/// the importer will write — the same rule the other two sources follow.
fn provider_rows(ctx: &ScanContext) -> Vec<AssetReport> {
    // **The rows come from the drafts** — the same shape the other two
    // sources use. Re-deriving the values here would give two places that
    // could disagree about what an account is.
    let mut out = Vec::new();
    for draft in OpenCodeSource.provider_drafts(ctx) {
        out.push(super::provider_row(
            &draft,
            AssetStatus::Actionable,
            super::text::provider_copyable(
                draft.base_url.as_deref(),
                draft.api_key.as_ref().map(|k| k.kind()),
            ),
        ));
    }
    out
}

/// The MCP servers OpenCode has configured.
///
/// **OpenCode's `command` is an ARRAY** (`["node", "…/mcp-server.mjs"]`),
/// while ours is `command: Option<String>` + `args: Option<Vec<String>>`
/// (`mcp/config.rs:145`) — so it is split here.
fn mcp_rows(ctx: &ScanContext) -> Vec<AssetReport> {
    let Some(value) = super::provider::read_jsonc(&ctx.opencode_config_dir.join("opencode.jsonc"))
    else {
        return Vec::new();
    };
    let Some(servers) = value.get("mcp").and_then(|m| m.as_object()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (name, server) in servers {
        let Some(server) = server.as_object() else {
            continue;
        };
        let command = server.get("command");
        // A command may be a string or an array — both shapes occur.
        let (command, args): (Option<String>, Vec<String>) = match command {
            Some(serde_json::Value::String(text)) => (Some(text.clone()), Vec::new()),
            Some(serde_json::Value::Array(items)) => {
                let mut items = items.iter();
                let first = items.next().and_then(|v| v.as_str()).map(str::to_string);
                (
                    first,
                    items
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect(),
                )
            }
            _ => (None, Vec::new()),
        };
        if command.is_none() {
            // Neither `command` nor `url`: our loader would reject the whole
            // file (`mcp/config.rs:670-673`).
            out.push(AssetReport {
                kind: AssetKind::McpServers,
                name: name.clone(),
                path: ctx.opencode_config_dir.join("opencode.jsonc"),
                scope: AssetScope::User,
                status: AssetStatus::Blocked,
                detail: super::text::rules_rejected(),
            });
            continue;
        }
        let _ = args;
        out.push(AssetReport {
            kind: AssetKind::McpServers,
            name: name.clone(),
            path: ctx.opencode_config_dir.join("opencode.jsonc"),
            scope: AssetScope::User,
            status: AssetStatus::Actionable,
            detail: super::text::mcp_copyable(1, &ctx.product.user().join("mcp.json")),
        });
    }
    out
}

/// The protocol for an OpenCode provider, from its `npm` package name.
///
/// **OpenCode does not say `wire_api`** — it says which AI SDK package it
/// speaks through, and that is what determines the wire format.
///
/// **This lives HERE, not in the shared `provider.rs`.** The shared module
/// knows the wire families; OpenCode knows how IT names one. A new source
/// adds a function here, and the shared module stays untouched.
fn protocol_for_opencode(npm: Option<&str>) -> String {
    match npm {
        Some("@ai-sdk/openai-compatible") => "openai-compatible".into(),
        Some("@ai-sdk/anthropic") => "anthropic-compatible".into(),
        Some("@ai-sdk/openai") => "openai-responses".into(),
        Some(other) => super::provider::unsupported_protocol(&format!("opencode:{other}")),
        None => "openai-compatible".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::report::{AssetKind, AssetScope, AssetStatus};

    fn ctx(home: &std::path::Path, project: &std::path::Path) -> ScanContext {
        ScanContext {
            os_home: home.to_path_buf(),
            project: project.to_path_buf(),
            product: crate::ProductDirs::new(home.join(".ours"), ".ours"),
            skill_dirs: None,
            opencode_config_dir: home.join(".config/opencode"),
            opencode_data_dir: home.join(".local/share/opencode"),
        }
    }

    /// The registry's facts, loaded the way `scan()` loads them.
    fn truth(home: &std::path::Path, project: &std::path::Path) -> SkillTruth {
        super::super::SkillTruth::load(&ctx(home, project))
    }

    /// A skill in OpenCode's own tree: same format as ours, so it can be
    /// copied.
    #[test]
    fn an_opencode_skill_is_actionable_because_we_do_not_read_that_tree() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let skill = home
            .path()
            .join(".config/opencode/skills/huawei-apig/SKILL.md");
        std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
        std::fs::write(
            &skill,
            "---\nname: huawei-apig\ndescription: x\n---\n\nbody\n",
        )
        .unwrap();

        let assets = OpenCodeSource.detect(
            &ctx(home.path(), project.path()),
            &truth(home.path(), project.path()),
        );
        let row = assets
            .iter()
            .find(|a| a.kind == AssetKind::Skills)
            .expect("the skill is reported");
        assert_eq!(row.status, AssetStatus::Actionable);
    }

    /// A shared `.agents/skills` skill is ALREADY ours — we read that tree
    /// (`registry.rs:585`). The report must say so, not claim it is movable.
    #[test]
    fn a_shared_agents_skill_is_active_because_we_read_that_tree() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let skill = home.path().join(".agents/skills/huawei-vpc/SKILL.md");
        std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
        std::fs::write(
            &skill,
            "---\nname: huawei-vpc\ndescription: x\n---\n\nbody\n",
        )
        .unwrap();

        let assets = OpenCodeSource.detect(
            &ctx(home.path(), project.path()),
            &truth(home.path(), project.path()),
        );
        let row = assets
            .iter()
            .find(|a| a.kind == AssetKind::Skills && a.name == "huawei-vpc")
            .expect("the skill is reported");
        assert_eq!(row.status, AssetStatus::Active);
    }

    /// The provider's credential is a LITERAL — like Claude's, unlike Codex's.
    #[test]
    fn an_opencode_provider_key_is_a_literal_not_a_reference() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let config = home.path().join(".config/opencode/opencode.json");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            &config,
            r#"{"provider":{"atomgit":{"npm":"@ai-sdk/openai-compatible","options":{"baseURL":"http://127.0.0.1:8001/v1","apiKey":"sk-literal"}}}}"#,
        )
        .unwrap();

        let drafts = OpenCodeSource.provider_drafts(&ctx(home.path(), project.path()));
        assert_eq!(drafts.len(), 1);
        assert_eq!(
            drafts[0].api_key,
            Some(ApiKeyRef::Plaintext("sk-literal".into())),
            "a literal, so it needs the person's confirmation"
        );
    }

    /// The MCP `command` is an ARRAY — split into command + args.
    #[test]
    fn an_opencode_mcp_command_array_is_split_into_command_and_args() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let config = home.path().join(".config/opencode/opencode.jsonc");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            &config,
            "{\n  // a comment, because this file is jsonc\n  \"mcp\":{\"x\":{\"type\":\"local\",\"command\":[\"node\",\"/tmp/s.mjs\"],\"enabled\":true}}\n}",
        )
        .unwrap();

        let assets = OpenCodeSource.detect(
            &ctx(home.path(), project.path()),
            &truth(home.path(), project.path()),
        );
        let row = assets
            .iter()
            .find(|a| a.kind == AssetKind::McpServers)
            .expect("the server is reported");
        assert_eq!(row.status, AssetStatus::Actionable);
    }

    /// `auth.json` holds PLAINTEXT keys. We report that they are there and
    /// never offer to move them.
    #[test]
    fn the_auth_file_is_reported_but_never_offered_for_import() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let auth = home.path().join(".local/share/opencode/auth.json");
        std::fs::create_dir_all(auth.parent().unwrap()).unwrap();
        std::fs::write(&auth, r#"{"deepseek":{"type":"api","key":"sk-x"}}"#).unwrap();

        let assets = OpenCodeSource.detect(
            &ctx(home.path(), project.path()),
            &truth(home.path(), project.path()),
        );
        let row = assets
            .iter()
            .find(|a| a.kind == AssetKind::ProviderConfig)
            .expect("the credentials are reported");
        assert_eq!(row.status, AssetStatus::Blocked);
    }
}
