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

    fn scan_providers(&self, ctx: &ScanContext) -> super::ProviderScan {
        let mut scan = super::ProviderScan::default();
        let mut definitions = std::collections::BTreeMap::new();
        for file in ["opencode.json", "opencode.jsonc"] {
            let path = ctx.opencode_config_dir.join(file);
            let parsed = super::provider::read_jsonc(&path).and_then(|config| {
                let Some(config) = config else {
                    return Ok(None);
                };
                let Some(providers) = config.get("providers").or_else(|| config.get("provider"))
                else {
                    return Ok(None);
                };
                providers.as_object().cloned().map(Some).ok_or(())
            });
            match parsed {
                Ok(Some(providers)) => {
                    definitions.extend(
                        providers
                            .into_iter()
                            .map(|(id, provider)| (id, (provider, path.clone()))),
                    );
                }
                Ok(None) => {}
                Err(()) => scan
                    .failures
                    .push(super::provider::unreadable_provider_file(
                        &path,
                        AssetScope::User,
                    )),
            }
        }
        if !scan.failures.is_empty() {
            return scan;
        }

        for (name, (provider, source_path)) in definitions {
            let settings = provider.get("settings").or_else(|| provider.get("options"));
            let setting = |key: &str| {
                settings
                    .and_then(|settings| settings.get(key))
                    .and_then(|value| value.as_str())
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
            };
            let base_url = setting("baseURL");
            let key = setting("apiKey");
            if base_url.is_none() && key.is_none() {
                continue;
            }
            let id = super::provider::account_id_for(&name);
            let protocol = protocol_for_opencode(
                provider
                    .get("package")
                    .or_else(|| provider.get("npm"))
                    .and_then(|v| v.as_str()),
            );
            let api_key = key.map(parse_credential).transpose();
            let blocked = if id.is_none() {
                Some(super::text::provider_name_unusable(&name))
            } else if super::is_unsupported_protocol(&protocol) {
                Some(super::text::provider_protocol_unsupported(&protocol))
            } else if api_key.is_err() || base_url.is_some_and(has_reference) {
                Some(super::text::unusable_credential_reference())
            } else if base_url.is_none() && super::provider::protocol_needs_endpoint(&protocol) {
                Some(super::text::provider_nothing_to_connect_to())
            } else {
                None
            };
            let models = provider
                .get("models")
                .and_then(|models| models.as_object())
                .into_iter()
                .flatten()
                .map(|(id, model)| {
                    model
                        .get("modelID")
                        .or_else(|| model.get("id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or(id)
                        .to_owned()
                })
                .collect();
            scan.drafts.push(ProviderAccountDraft {
                id: id.unwrap_or_else(|| name.clone()),
                source_name: name,
                base_url: super::provider::effective_endpoint(&protocol, base_url),
                protocol,
                api_key: api_key.ok().flatten(),
                models,
                source_path,
                scope: AssetScope::User,
                blocked,
            });
        }
        scan
    }
}

fn has_reference(value: &str) -> bool {
    value.contains("{env:") || value.contains("{file:")
}

fn parse_credential(value: &str) -> Result<ApiKeyRef, ()> {
    if let Some(name) = value
        .strip_prefix("{env:")
        .and_then(|value| value.strip_suffix('}'))
    {
        if super::provider::valid_env_name(name) {
            return Ok(ApiKeyRef::EnvVar(name.to_owned()));
        }
    }
    if has_reference(value) {
        return Err(());
    }
    Ok(ApiKeyRef::Plaintext(value.to_owned()))
}

/// The MCP servers OpenCode has configured.
///
/// 与 `scan_providers` 一样先读 `opencode.json`，再读 `opencode.jsonc`。
/// 后一个文件覆盖同名服务，每行记录服务的实际来源。
fn mcp_rows(ctx: &ScanContext) -> Vec<AssetReport> {
    let mut out = Vec::new();
    let mut servers = std::collections::BTreeMap::new();
    for file in ["opencode.json", "opencode.jsonc"] {
        let path = ctx.opencode_config_dir.join(file);
        match super::provider::read_jsonc(&path) {
            Ok(Some(value)) => {
                if let Some(found) = value.get("mcp").and_then(|m| m.as_object()) {
                    servers.extend(
                        found
                            .iter()
                            .map(|(name, server)| (name.clone(), (server.clone(), path.clone()))),
                    );
                }
            }
            Ok(None) => {}
            Err(()) => out.push(AssetReport {
                kind: AssetKind::McpServers,
                ..super::provider::unreadable_provider_file(&path, AssetScope::User)
            }),
        }
    }

    for (name, (server, path)) in servers {
        let Some(server) = server.as_object() else {
            continue;
        };
        // 报告只需判断是否有可执行文件名，参数留在原配置中。
        let command = match server.get("command") {
            Some(serde_json::Value::String(text)) => Some(text.as_str()),
            Some(serde_json::Value::Array(items)) => items.first().and_then(|v| v.as_str()),
            _ => None,
        };
        let has_url = server
            .get("url")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|url| !url.trim().is_empty());
        let (status, detail) = if command.is_some_and(|command| !command.trim().is_empty()) {
            (
                AssetStatus::Actionable,
                super::text::mcp_copyable(1, &ctx.product.user().join("mcp.json")),
            )
        } else if has_url {
            (AssetStatus::Blocked, super::text::mcp_remote_not_copyable())
        } else {
            (AssetStatus::Blocked, super::text::rules_rejected())
        };
        out.push(AssetReport {
            kind: AssetKind::McpServers,
            name,
            path,
            scope: AssetScope::User,
            status,
            detail,
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
        Some("@ai-sdk/openai-compatible" | "@opencode/ai/providers/openai-compatible") => {
            "openai-compatible".into()
        }
        Some("@ai-sdk/anthropic" | "@opencode/ai/providers/anthropic-compatible") => {
            "anthropic-compatible".into()
        }
        Some("@ai-sdk/openai" | "@opencode/ai/providers/openai-compatible/responses") => {
            "openai-responses".into()
        }
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

    #[test]
    fn a_jsonc_file_with_trailing_commas_keeps_providers_from_the_json_file() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let ctx = ctx(home.path(), project.path());
        std::fs::create_dir_all(&ctx.opencode_config_dir).unwrap();
        std::fs::write(
            ctx.opencode_config_dir.join("opencode.json"),
            r#"{"provider":{"demo":{"options":{"baseURL":"https://example.invalid/v1"},
                "models":{"demo-model":{}}}}}"#,
        )
        .unwrap();
        std::fs::write(
            ctx.opencode_config_dir.join("opencode.jsonc"),
            "{\n  // Keep the rest of the settings in a separate file.\n  \"theme\": \"dark\",\n}",
        )
        .unwrap();

        let scan = OpenCodeSource.scan_providers(&ctx);
        assert!(scan.failures.is_empty());
        let [draft] = scan.drafts.as_slice() else {
            panic!("expected the provider from opencode.json");
        };
        assert_eq!(draft.id, "demo");
        assert_eq!(draft.models, ["demo-model"]);
    }

    #[test]
    fn model_ids_and_environment_references_survive_the_scan() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let ctx = ctx(home.path(), project.path());
        std::fs::create_dir_all(&ctx.opencode_config_dir).unwrap();
        std::fs::write(
            ctx.opencode_config_dir.join("opencode.jsonc"),
            r#"{"provider":{
                "env":{"options":{"baseURL":"https://env.invalid/v1","apiKey":"{env:OPENCODE_KEY}"},
                    "models":{"alias":{"modelID":"provider/model"},"plain":{},},},
                "file":{"options":{"baseURL":"https://file.invalid/v1","apiKey":"{file:private-key}"},},
            },}"#,
        )
        .unwrap();

        let scan = OpenCodeSource.scan_providers(&ctx);
        assert!(scan.failures.is_empty());
        let [env, file] = scan.drafts.as_slice() else {
            panic!("expected both providers");
        };
        assert_eq!(env.models, ["provider/model", "plain"]);
        assert_eq!(env.api_key, Some(ApiKeyRef::EnvVar("OPENCODE_KEY".into())));
        assert!(env.blocked.is_none());
        assert!(file.api_key.is_none());
        assert!(file.blocked.is_some());
    }

    /// 数组形式的命令与字符串命令一样会列为可迁移。
    #[test]
    fn an_opencode_mcp_command_array_is_actionable() {
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

    #[test]
    fn mcp_servers_are_merged_by_name_and_keep_their_source_paths() {
        let home = tempfile::tempdir().unwrap();
        let ctx = ctx(home.path(), home.path());
        let dir = &ctx.opencode_config_dir;
        std::fs::create_dir_all(dir).unwrap();
        let json = dir.join("opencode.json");
        let jsonc = dir.join("opencode.jsonc");
        std::fs::write(
            &json,
            r#"{"mcp":{"local":{"command":["node","server.js"]},"shared":{"command":"node"}}}"#,
        )
        .unwrap();
        std::fs::write(
            &jsonc,
            r#"{"mcp":{"shared":{"url":"https://mcp.example.test"},"invalid":{}}}"#,
        )
        .unwrap();

        let rows = mcp_rows(&ctx);
        assert_eq!(rows.len(), 3);
        let local = rows.iter().find(|row| row.name == "local").unwrap();
        assert_eq!(local.path, json);
        assert_eq!(local.status, AssetStatus::Actionable);
        let shared = rows.iter().find(|row| row.name == "shared").unwrap();
        assert_eq!(shared.path, jsonc);
        assert_eq!(shared.status, AssetStatus::Blocked);
        assert_eq!(shared.detail, super::super::text::mcp_remote_not_copyable());
        let invalid = rows.iter().find(|row| row.name == "invalid").unwrap();
        assert_eq!(invalid.status, AssetStatus::Blocked);
        assert_eq!(invalid.detail, super::super::text::rules_rejected());
    }

    #[test]
    fn invalid_json_is_reported_alongside_servers_from_jsonc() {
        let home = tempfile::tempdir().unwrap();
        let ctx = ctx(home.path(), home.path());
        let dir = &ctx.opencode_config_dir;
        std::fs::create_dir_all(dir).unwrap();
        let json = dir.join("opencode.json");
        let jsonc = dir.join("opencode.jsonc");
        std::fs::write(&json, "{").unwrap();
        std::fs::write(&jsonc, r#"{"mcp":{"local":{"command":"node"}}}"#).unwrap();

        let rows = mcp_rows(&ctx);
        assert_eq!(rows.len(), 2);
        let blocked = rows.iter().find(|row| row.path == json).unwrap();
        assert_eq!(blocked.kind, AssetKind::McpServers);
        assert_eq!(blocked.status, AssetStatus::Blocked);
        assert_eq!(blocked.detail, super::super::text::unreadable_config());
        let local = rows.iter().find(|row| row.path == jsonc).unwrap();
        assert_eq!(local.status, AssetStatus::Actionable);
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
