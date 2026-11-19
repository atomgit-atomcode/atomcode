//! Codex's assets.
//!
//! None of them are read by AtomCode today: our skill scan list has no
//! `.codex` entry, our instruction tiers do not look at `~/.codex/AGENTS.md`,
//! and our MCP config is JSON while Codex's is TOML. So every row here is
//! `Actionable` (there is a way to bring it over) except sessions, which are
//! lossy to convert.
//!
//! Read-only: this looks and reports, it never writes.

use super::provider::{ApiKeyRef, ProviderAccountDraft};
use super::report::{AssetKind, AssetReport, AssetScope, AssetStatus};
use super::{probe_dir, MigrationSource, ScanContext, SkillTruth};
use std::path::Path;

/// Codex's layered config, merged the way Codex itself layers it: the
/// project's file wins over the user's, key by key.
struct MergedCodexConfig {
    /// Provider id -> the provider's table, the file it was read from, and
    /// the scope it was read at — so the report can show which file to look
    /// at, and a project provider is not reported as the person's own.
    providers: std::collections::BTreeMap<
        String,
        (toml::Value, std::path::PathBuf, super::report::AssetScope),
    >,
    /// The provider the person selected, if the config names one.
    model_provider: Option<String>,
    /// The model the person was using.
    model: Option<String>,
}

/// Merge the config files in order — later files win.
///
/// Each file comes with the scope it was read at, so a provider that came
/// from the project's config is not reported as the person's own.
///
/// **Tables are merged key by key, not replaced.** Codex merges its config
/// layers recursively (`codex-rs/config/src/merge.rs:59`, `merge_toml_values`
/// walks into nested tables), so a project file that overrides only
/// `base_url` under a provider id keeps the `env_key` and `wire_api` the user
/// file set. Replacing the whole table would drop the credential — the very
/// thing the person most needs to keep.
fn merge_codex_configs(
    files: &[(std::path::PathBuf, super::report::AssetScope)],
) -> MergedCodexConfig {
    // The type is spelled out: `and_modify` below needs to know what the
    // value is.
    let mut providers: std::collections::BTreeMap<
        String,
        (toml::Value, std::path::PathBuf, super::report::AssetScope),
    > = std::collections::BTreeMap::new();
    let mut model_provider = None;
    let mut model = None;
    for (path, scope) in files {
        let Ok(raw) = std::fs::read_to_string(path) else {
            continue;
        };
        // **`toml::from_str`, not `raw.parse::<toml::Value>()`** — the two
        // are NOT the same path. `FromStr` for `toml::Value` rejects a table
        // header with a dot in it, and Codex's own config is full of those:
        // `[model_providers.my-provider]`. The `FromStr` path would read
        // every real Codex config as unparseable, and every provider in it
        // would silently vanish from the report.
        let Ok(value) = toml::from_str::<toml::Value>(&raw) else {
            continue;
        };
        if let Some(table) = value.get("model_providers").and_then(|p| p.as_table()) {
            for (name, provider) in table {
                providers
                    .entry(name.clone())
                    .and_modify(|existing| {
                        merge_table(&mut existing.0, provider);
                        // The overlay file is now the one that decides this
                        // provider's values — so it is also the file the report
                        // should point at.
                        existing.1 = path.clone();
                        existing.2 = *scope;
                    })
                    .or_insert_with(|| (provider.clone(), path.clone(), *scope));
            }
        }
        if let Some(selected) = value.get("model_provider").and_then(|v| v.as_str()) {
            model_provider = Some(selected.to_string());
        }
        if let Some(name) = value.get("model").and_then(|v| v.as_str()) {
            model = Some(name.to_string());
        }
    }
    MergedCodexConfig {
        providers,
        model_provider,
        model,
    }
}

/// Merge `overlay` into `base`, key by key, the way Codex merges its layers.
///
/// A scalar in the overlay replaces the base's; a table in the overlay is
/// merged into the base's table rather than replacing it.
fn merge_table(base: &mut toml::Value, overlay: &toml::Value) {
    // Reborrow, so `base` is still usable in the `else` arm below — matching
    // on `(base, overlay)` would MOVE the reference and leave it unusable.
    let (toml::Value::Table(base_table), toml::Value::Table(overlay_table)) = (&mut *base, overlay)
    else {
        // Not a table: the overlay wins outright.
        *base = overlay.clone();
        return;
    };
    for (key, value) in overlay_table {
        match base_table.get_mut(key) {
            Some(existing) => merge_table(existing, value),
            None => {
                base_table.insert(key.clone(), value.clone());
            }
        }
    }
}

pub struct CodexSource;

impl MigrationSource for CodexSource {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn display_name(&self) -> &'static str {
        "Codex"
    }

    fn detect(&self, ctx: &ScanContext, truth: &SkillTruth) -> Vec<AssetReport> {
        let mut out = Vec::new();
        let user = ctx.os_home.join(".codex");
        let project_codex = ctx.project.join(".codex");

        // Global instructions.
        let agents = user.join("AGENTS.md");
        if agents.is_file() {
            out.push(AssetReport {
                kind: AssetKind::Instructions,
                name: "AGENTS.md".into(),
                path: agents,
                scope: AssetScope::User,
                status: AssetStatus::Actionable,
                detail: super::text::instructions_copyable(&ctx.product.user().join("ATOMCODE.md")),
            });
        }

        // Project-level instructions: the open standard, so we read them —
        // but the SAME real loader decides, so an `AGENTS.md` that is empty,
        // unreadable, or shadowed by a higher-priority file is not called
        // "already in effect". The verdict comes from the loader, not from a
        // hand-rolled check that could drift from it.
        let project_agents = ctx.project.join("AGENTS.md");
        if let Some(source) = super::instruction_verdict(ctx, &project_agents) {
            // A `Missing` file produces NO row — and it cannot be a
            // `continue`, because this is not a loop.
            if source.state != crate::instructions::InstructionState::Missing {
                let (status, detail) = match source.state {
                    crate::instructions::InstructionState::Loaded => {
                        (AssetStatus::Active, super::text::instructions_active())
                    }
                    // The message names the file that WINS, not this one.
                    crate::instructions::InstructionState::Shadowed => (
                        AssetStatus::Blocked,
                        super::text::instructions_shadowed(
                            source.shadowed_by.as_deref().unwrap_or(&source.name),
                        ),
                    ),
                    crate::instructions::InstructionState::Empty => {
                        (AssetStatus::Blocked, super::text::instructions_empty())
                    }
                    crate::instructions::InstructionState::Unreadable => (
                        AssetStatus::Blocked,
                        super::text::unreadable_file("the file could not be read"),
                    ),
                    crate::instructions::InstructionState::Missing => {
                        unreachable!("handled above")
                    }
                };
                out.push(AssetReport {
                    kind: AssetKind::Instructions,
                    name: source.name,
                    path: source.path,
                    scope: source.scope,
                    status,
                    detail,
                });
            }
        }

        // Skills. Codex installs them in GROUPING directories
        // (`~/.codex/skills/.system/<name>/SKILL.md`), so only the immediate
        // children would miss the installed ones — a real skill vanishing
        // from the report.
        //
        // **Through the shared helper, not a walk of our own.** The helper
        // knows how to descend grouping directories, and — more importantly —
        // it asks the registry first: a host that put `~/.codex/skills` in its
        // skill roots has those skills ALREADY in effect, and "copy it over"
        // would be nonsense. A walk of our own that answered `Actionable`
        // unconditionally would say that even about a skill that is running
        // right now.
        for (scope, root) in [
            (AssetScope::User, user.join("skills")),
            (AssetScope::Project, project_codex.join("skills")),
        ] {
            out.extend(super::skill_rows(
                AssetKind::Skills,
                scope,
                &root,
                truth,
                super::SkillTree::Foreign {
                    target: ctx.product.user().join("skills"),
                },
            ));
        }

        // The TOML config: MCP servers and the provider setup.
        for (scope, path) in [
            (AssetScope::User, user.join("config.toml")),
            (AssetScope::Project, project_codex.join("config.toml")),
        ] {
            if !path.is_file() {
                continue;
            }
            // **`toml::from_str`, not `raw.parse::<toml::Value>()`** — the
            // two are NOT the same path. `FromStr` for `toml::Value` rejects
            // a table header with a dot in it, and Codex's own config is
            // full of those: `[mcp_servers.fs]`. The `FromStr` path would
            // report every real Codex config as unparseable — and the
            // person would be told to fix a file that is perfectly fine.
            // The serde path (`from_str`) takes the dotted key as a nested
            // table, which is what the file means.
            match std::fs::read_to_string(&path)
                .ok()
                .and_then(|raw| toml::from_str::<toml::Value>(&raw).ok())
            {
                Some(value) => {
                    if let Some(count) = mcp_server_count(&value) {
                        out.push(AssetReport {
                            kind: AssetKind::McpServers,
                            name: "mcp_servers".into(),
                            path: path.clone(),
                            scope,
                            status: AssetStatus::Actionable,
                            detail: super::text::mcp_copyable(
                                count,
                                &ctx.product.user().join("mcp.json"),
                            ),
                        });
                    }
                    // The provider rows are added below, from the drafts —
                    // one row per account, named by its id.
                }
                None => {
                    out.push(AssetReport {
                        kind: AssetKind::ProviderConfig,
                        name: "config.toml".into(),
                        path: path.clone(),
                        scope,
                        status: AssetStatus::Blocked,
                        detail: super::text::unreadable_config(),
                    });
                }
            }
        }

        // The provider rows. They come from the drafts, so the row's NAME is
        // the account id the importer will write — a row named "config.toml"
        // could never be matched by the re-check.
        //
        // In THIS task `provider_drafts` is still the default (empty), so no
        // provider row appears yet; Task 7 fills the drafts in and the rows
        // appear with it.
        for draft in self.provider_drafts(ctx) {
            out.push(super::provider_row(
                &draft,
                AssetStatus::Actionable,
                super::text::provider_copyable(
                    draft.base_url.as_deref(),
                    draft.api_key.as_ref().map(|k| k.kind()),
                ),
            ));
        }

        // Sessions.
        let sessions = user.join("sessions");
        if sessions.is_dir() {
            match session_file_count(&sessions) {
                Ok(count) if count > 0 => out.push(AssetReport {
                    kind: AssetKind::Sessions,
                    name: "sessions".into(),
                    path: sessions,
                    scope: AssetScope::User,
                    status: AssetStatus::Blocked,
                    detail: super::text::sessions_blocked(count),
                }),
                Ok(_) => {}
                Err(reason) => out.push(AssetReport {
                    kind: AssetKind::Sessions,
                    name: "sessions".into(),
                    path: sessions,
                    scope: AssetScope::User,
                    status: AssetStatus::Blocked,
                    detail: super::text::unreadable_file(&reason),
                }),
            }
        }

        out
    }

    fn provider_drafts(&self, ctx: &ScanContext) -> Vec<ProviderAccountDraft> {
        // Codex layers its config: the project's `.codex/config.toml` wins over
        // the user's `~/.codex/config.toml`. So the two are **merged by id**,
        // project over user — otherwise the same provider id appears twice and
        // the second one is a conflict instead of an override.
        let merged = merge_codex_configs(&[
            (
                ctx.os_home.join(".codex").join("config.toml"),
                super::report::AssetScope::User,
            ),
            (
                ctx.project.join(".codex").join("config.toml"),
                super::report::AssetScope::Project,
            ),
        ]);

        let mut drafts: Vec<ProviderAccountDraft> = merged
            .providers
            .into_iter()
            .map(|(name, (provider, source_path, scope))| {
                let base_url = provider
                    .get("base_url")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                // `env_key` is the NAME of the variable to read, never a secret.
                let key = provider
                    .get("env_key")
                    .and_then(|v| v.as_str())
                    .filter(|v| !v.trim().is_empty())
                    .map(|name| ApiKeyRef::EnvVar(name.to_string()));
                // Codex says its own default: its `WireApi` defaults to
                // Responses (`codex-rs/model-provider-info/src/lib.rs:117`).
                let protocol = super::provider::protocol_for_wire(
                    provider.get("wire_api").and_then(|v| v.as_str()),
                    "openai-responses",
                );
                // **Nothing is dropped here.** Every provider table the
                // source declares becomes a row — a definition that cannot
                // be imported is `Blocked` with the reason, not silence.
                //
                // Normalizing can turn a valid definition into an
                // empty-looking one — a provider that uses the Responses
                // default endpoint has its endpoint stored as `None` — so
                // the decision is made on the RAW definition, never on the
                // normalized one.
                //
                // A name that cannot become an account id at all is a real
                // problem the person should see, not something to paper
                // over with the raw name.
                let id = super::provider::account_id_for(&name);
                // A wire protocol we do not speak is refused before the
                // write (`create_account` would silently downgrade it).
                let blocked = if id.is_none() {
                    Some(super::text::provider_name_unusable(&name))
                } else if super::is_unsupported_protocol(&protocol) {
                    Some(super::text::provider_protocol_unsupported(&protocol))
                } else if base_url.is_none() && key.is_none() {
                    // No endpoint and no credential: there is nothing to
                    // connect to. It is still a definition the person wrote,
                    // so it is reported — as blocked.
                    Some(super::text::provider_nothing_to_connect_to())
                } else {
                    None
                };
                ProviderAccountDraft {
                    // The id the account will get, not the raw name. When the
                    // name cannot become an id, the raw name is kept ONLY so
                    // the row can name the definition — `blocked` says it
                    // cannot be imported.
                    id: id.clone().unwrap_or_else(|| name.clone()),
                    // The name the source wrote it under — the table's key.
                    source_name: name.clone(),
                    protocol: protocol.clone(),
                    // The endpoint as it will be stored.
                    base_url: super::provider::effective_endpoint(&protocol, base_url.as_deref()),
                    api_key: key,
                    source_path: source_path.clone(),
                    // The model belongs to the provider that was actually
                    // selected, not to whichever came first in the table.
                    models: if merged.model_provider.as_deref() == Some(name.as_str()) {
                        merged.model.iter().cloned().collect()
                    } else {
                        Vec::new()
                    },
                    // The scope the config was read at: a provider from the
                    // project's config is not the person's own provider.
                    scope,
                    blocked,
                }
            })
            .collect();

        // A model with NO named provider needs no special handling here: the
        // per-draft `models` above already gives the model to the provider
        // Codex actually selected, and to nobody else.
        //
        // When `model_provider` is absent, Codex was on its built-in `openai`
        // provider (`codex-rs/core/src/config/mod.rs:4133` falls back to
        // `"openai"`). That provider has no entry in `model_providers` and is
        // not ours to import — so the model is left unattached rather than
        // pinned onto "the first account", which would put it on an account
        // the person never selected.
        drafts
    }
}

/// The bound on how deep the Codex skill walk goes.
///
/// The registry's own bound is 8 (`registry.rs:296`); the same number here
/// keeps the two from disagreeing about what exists.
const MAX_SKILL_DEPTH: usize = 8;

/// Find every Codex skill under `root`, including the ones in grouping
/// directories (`~/.codex/skills/.system/<name>/SKILL.md`).
///
/// The walk is **bounded** and **cycle-guarded**: a symlink back to an
/// ancestor would otherwise be walked forever, and the read-only report would
/// never finish.
fn collect_codex_skills(
    dir: &Path,
    depth: usize,
    max_depth: usize,
    ancestors: &mut std::collections::BTreeSet<std::path::PathBuf>,
    found: &mut Vec<(String, std::path::PathBuf)>,
) {
    if depth > max_depth {
        return;
    }
    let identity = match std::fs::canonicalize(dir) {
        Ok(path) => path,
        Err(_) => return,
    };
    if !ancestors.insert(identity.clone()) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    for entry in entries.flatten() {
        paths.push(entry.path());
    }
    paths.sort();
    for path in paths {
        if path.is_dir() {
            if path.join("SKILL.md").is_file() {
                found.push((
                    path.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    path,
                ));
            } else {
                collect_codex_skills(&path, depth + 1, max_depth, ancestors, found);
            }
        }
    }
    ancestors.remove(&identity);
}

/// How many `[mcp_servers.*]` tables the config declares.
fn mcp_server_count(value: &toml::Value) -> Option<usize> {
    let servers = value.get("mcp_servers")?.as_table()?;
    (!servers.is_empty()).then_some(servers.len())
}

/// How many session files there are under `sessions/`.
fn session_file_count(sessions: &Path) -> Result<usize, String> {
    let mut count = 0usize;
    let mut stack = vec![sessions.to_path_buf()];
    // **The visited set stops a symlink cycle.** `is_dir()` follows links, so
    // a directory that links back to an ancestor would be walked forever and
    // the read-only report would never finish.
    let mut visited: std::collections::BTreeSet<std::path::PathBuf> =
        std::collections::BTreeSet::new();
    while let Some(dir) = stack.pop() {
        let identity = match std::fs::canonicalize(&dir) {
            Ok(path) => path,
            Err(error) => return Err(error.to_string()),
        };
        if !visited.insert(identity) {
            continue;
        }
        let entries = std::fs::read_dir(&dir).map_err(|e| e.to_string())?;
        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path();
            // **Fallible**: a directory we cannot stat is not the same as a
            // file that is not there.
            match probe_dir(&path) {
                Ok(true) => stack.push(path),
                Ok(false) => {
                    if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                        count += 1;
                    }
                }
                Err(_) => {}
            }
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::report::{AssetKind, AssetScope, AssetStatus};
    use std::path::PathBuf;

    fn ctx(home: &std::path::Path, project: &std::path::Path) -> ScanContext {
        ScanContext {
            os_home: home.to_path_buf(),
            project: project.to_path_buf(),
            product: crate::ProductDirs::new(home.join(".ours"), ".ours"),
            skill_dirs: None,
            opencode_config_dir: PathBuf::new(),
            opencode_data_dir: PathBuf::new(),
        }
    }

    /// The registry's facts, loaded the way `scan()` loads them.
    fn truth(home: &std::path::Path, project: &std::path::Path) -> SkillTruth {
        super::super::SkillTruth::load(&ctx(home, project))
    }

    /// Write a file, creating its parent directory first.
    ///
    /// A fixture that writes `<home>/.codex/config.toml` into a directory that
    /// does not exist yet fails at the write, not at the assertion — and the
    /// failure says nothing about the behaviour being checked.
    fn write_file(path: &std::path::Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn the_global_agents_file_is_actionable() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_file(&home.path().join(".codex").join("AGENTS.md"), "# rules");

        let assets = CodexSource.detect(
            &ctx(home.path(), project.path()),
            &truth(home.path(), project.path()),
        );
        let instructions = assets
            .iter()
            .find(|a| a.kind == AssetKind::Instructions)
            .expect("the instructions are reported");
        assert_eq!(instructions.status, AssetStatus::Actionable);
        assert_eq!(instructions.scope, AssetScope::User);
    }

    #[test]
    fn codex_skills_are_actionable_because_we_do_not_scan_that_path() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let dir = home.path().join(".codex").join("skills").join("deploy");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "# skill").unwrap();

        let assets = CodexSource.detect(
            &ctx(home.path(), project.path()),
            &truth(home.path(), project.path()),
        );
        let skill = assets
            .iter()
            .find(|a| a.kind == AssetKind::Skills)
            .expect("the skill is reported");
        assert_eq!(skill.status, AssetStatus::Actionable);
        assert_eq!(skill.name, "deploy");
    }

    #[test]
    fn a_broken_config_toml_is_reported_without_taking_the_scan_down() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_file(
            &home.path().join(".codex").join("config.toml"),
            "this is not = = toml",
        );
        let dir = home.path().join(".codex").join("skills").join("deploy");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "# skill").unwrap();

        let assets = CodexSource.detect(
            &ctx(home.path(), project.path()),
            &truth(home.path(), project.path()),
        );
        assert!(assets.iter().any(|a| a.kind == AssetKind::Skills));
        assert!(assets
            .iter()
            .any(|a| a.kind == AssetKind::ProviderConfig && a.status == AssetStatus::Blocked));
    }

    #[test]
    fn mcp_servers_in_the_config_are_actionable() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_file(
            &home.path().join(".codex").join("config.toml"),
            "[mcp_servers.fs]\ncommand = \"mcp-fs\"\n",
        );

        let assets = CodexSource.detect(
            &ctx(home.path(), project.path()),
            &truth(home.path(), project.path()),
        );
        let mcp = assets
            .iter()
            .find(|a| a.kind == AssetKind::McpServers)
            .expect("the MCP servers are reported");
        assert_eq!(mcp.status, AssetStatus::Actionable);
    }

    #[test]
    fn no_codex_directory_means_no_rows() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let assets = CodexSource.detect(
            &ctx(home.path(), project.path()),
            &truth(home.path(), project.path()),
        );
        assert!(assets.is_empty());
    }
}
