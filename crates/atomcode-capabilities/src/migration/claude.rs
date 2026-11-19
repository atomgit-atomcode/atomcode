//! Claude Code's assets.
//!
//! What AtomCode reads from a Claude Code tree today, and what it does not:
//!
//! | asset                          | user level      | project level |
//! |--------------------------------|-----------------|---------------|
//! | instructions (`CLAUDE.md`)     | not read        | read          |
//! | skills / commands              | read            | read          |
//! | permissions (`settings.json`)  | not read        | read          |
//! | MCP (`~/.claude.json`)         | not read        | read          |
//!
//! The global instruction tier reads `<product user tree>/ATOMCODE.md` and the
//! permission loader reads `<product user tree>` — neither is `~/.claude`. So
//! the user-level ones are `Actionable` (copy them over), not `Active`.
//!
//! Read-only: this looks and reports, it never writes.

use super::provider::{ApiKeyRef, ProviderAccountDraft};
use super::report::{AssetKind, AssetReport, AssetScope, AssetStatus};
// `probe_file` / `probe_dir` live in the parent module. A child module may
// reach a parent's private items, but it has to **name** them — an unqualified
// `probe_file(...)` would not resolve.
use super::{probe_dir, probe_file, MigrationSource, ScanContext, SkillTruth};
use std::path::Path;

pub struct ClaudeSource;

impl MigrationSource for ClaudeSource {
    fn id(&self) -> &'static str {
        "claude-code"
    }

    fn display_name(&self) -> &'static str {
        "Claude Code"
    }

    fn detect(&self, ctx: &ScanContext, truth: &SkillTruth) -> Vec<AssetReport> {
        let mut out = Vec::new();
        let user = ctx.os_home.join(".claude");
        let project_claude = ctx.project.join(".claude");
        // The registry's facts come from the ONE load `scan()` did — not a
        // load of our own. Two sources in one report must not see two
        // different moments of the same disk.
        let skills = truth;

        // Instructions. The verdict comes from the **real loader** —
        // `crate::instructions::load_instructions` — not from a hand-rolled
        // "is it empty / is it shadowed" check. The loader knows the whole
        // order (`.atomcode.md`, `ATOMCODE.md`, `AGENTS.md`, `CLAUDE.md`,
        // `claude.md` — `instructions.rs:17`), what "shadowed" means, and that
        // an empty file loads as `Empty` rather than `Loaded`
        // (`instructions.rs:173`). Re-implementing those rules is how a report
        // ends up saying "already in effect" about a file that is not.
        // **Both spellings.** The loader's project tier knows five names
        // (`instructions.rs:17`), and two of them are Claude's: `CLAUDE.md`
        // and `claude.md`. On a case-sensitive filesystem only the lowercase
        // one may exist — and the loader reads it. Asking for one spelling
        // would leave the other unmentioned: the file is there, it IS in
        // effect, and the report would not say so.
        //
        // The loader deduplicates the two when they name the same file
        // (`instructions.rs:132`), so the report must too — otherwise one
        // file would get two rows.
        for name in ["CLAUDE.md", "claude.md"] {
            let project_instructions = ctx.project.join(name);
            if let Some(source) = super::instruction_verdict(ctx, &project_instructions) {
                // Only report instruction files the loader found.
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
        }
        let user_instructions = user.join("CLAUDE.md");
        // **Fallible**: a file we cannot stat is not the same as a file that
        // is not there.
        //
        // **The same file can be both.** `ctx.project` may BE `~/.claude`
        // (a project that happens to live there) — then the user-level path
        // and the project-level path are the same file, and reporting it
        // twice would say both "already in effect" and "can be brought
        // over" about one file.
        //
        // **One file, one row** — and the check covers BOTH branches below,
        // because the two can fail differently: the project tier may have
        // read the file fine while the user-level probe hits a permission
        // error (or the file changed between the two probes). Comparing by
        // **file identity** (the same path spelled two ways, or a hard link
        // under a different name) settles it — a string comparison would
        // call a hard link two files.
        let already_reported = |path: &std::path::Path| {
            out.iter().any(|row: &AssetReport| {
                row.kind == AssetKind::Instructions && super::same_file(&row.path, path)
            })
        };
        match probe_file(&user_instructions) {
            Ok(true) => {
                // Already reported as the project-level file: it is one file,
                // and it already has a row.
                if !already_reported(&user_instructions) {
                    out.push(AssetReport {
                        kind: AssetKind::Instructions,
                        name: "CLAUDE.md".into(),
                        path: user_instructions,
                        scope: AssetScope::User,
                        status: AssetStatus::Actionable,
                        detail: super::text::instructions_copyable(
                            &ctx.product.user().join("ATOMCODE.md"),
                        ),
                    });
                }
            }
            Ok(false) => {}
            Err(reason) => {
                // The file could not be read. If the project tier already
                // reported it, that row stands — one file, one row. If not,
                // this is the only thing we know about it, and the error is
                // the reason it is not in effect.
                if !already_reported(&user_instructions) {
                    out.push(AssetReport {
                        kind: AssetKind::Instructions,
                        name: "CLAUDE.md".into(),
                        path: user_instructions,
                        scope: AssetScope::User,
                        status: AssetStatus::Blocked,
                        detail: super::text::unreadable_file(&reason),
                    });
                }
            }
        }

        // Skills and commands: the standard paths are in our own scan list.
        for (kind, dir) in [
            (AssetKind::Skills, "skills"),
            (AssetKind::Commands, "commands"),
        ] {
            for (scope, root) in [
                (AssetScope::User, user.join(dir)),
                (AssetScope::Project, project_claude.join(dir)),
            ] {
                // These paths ARE ours — the registry scans them, so its
                // verdict says whether each one is really in effect.
                out.extend(super::skill_rows(
                    kind,
                    scope,
                    &root,
                    &skills,
                    super::SkillTree::Scanned,
                ));
            }
        }

        // Permissions. A file that cannot be parsed is NOT "in effect" — our
        // own loader refuses a broken policy (`permission_settings.rs:296`), so
        // it must not be reported as active either.
        for (scope, root) in [
            (AssetScope::User, user.clone()),
            (AssetScope::Project, project_claude.clone()),
        ] {
            for name in ["settings.json", "settings.local.json"] {
                let path = root.join(name);
                match probe_file(&path) {
                    Ok(true) => {}
                    // Not there: nothing to report.
                    Ok(false) => continue,
                    // There, but we cannot read it: that IS an asset, and it
                    // is blocked.
                    Err(reason) => {
                        out.push(AssetReport {
                            kind: AssetKind::Permissions,
                            name: name.into(),
                            path,
                            scope,
                            status: AssetStatus::Blocked,
                            detail: super::text::unreadable_file(&reason),
                        });
                        continue;
                    }
                }
                let parsed = parse_json(&path);
                let (status, detail) = match parsed {
                    Err(reason) => (AssetStatus::Blocked, super::text::unreadable_file(&reason)),
                    // A file that parses but whose rules our loader would
                    // REJECT is not "in effect" either — the loader fails the
                    // whole file (`permission_settings.rs:345-353`), so none
                    // of its rules apply.
                    Ok(value) if !permission_rules_are_valid(&value) => {
                        (AssetStatus::Blocked, super::text::rules_rejected())
                    }
                    Ok(_) => match scope {
                        // The project one is merged into our own permission rules.
                        AssetScope::Project => {
                            (AssetStatus::Active, super::text::permissions_active())
                        }
                        // The user one is not read: the loader reads the product tree.
                        AssetScope::User => (
                            AssetStatus::Actionable,
                            super::text::permissions_copyable(
                                &ctx.product.user().join("settings.json"),
                            ),
                        ),
                    },
                };
                out.push(AssetReport {
                    kind: AssetKind::Permissions,
                    name: name.into(),
                    path,
                    scope,
                    status,
                    detail,
                });
            }
        }

        // MCP servers. The project's own `.mcp.json` is read; the global
        // `~/.claude.json` is not, so it can only be brought over.
        let project_mcp = ctx.project.join(".mcp.json");
        if probe_file(&project_mcp).unwrap_or(false) {
            // **One parse.** The loader's verdict and the count come from
            // the same parse, so they cannot disagree about the same file.
            match mcp_servers(&project_mcp) {
                Ok(counts) if counts.total > 0 => {
                    if counts.enabled > 0 {
                        out.push(AssetReport {
                            kind: AssetKind::McpServers,
                            name: "mcpServers".into(),
                            path: project_mcp.clone(),
                            scope: AssetScope::Project,
                            status: AssetStatus::Active,
                            detail: super::text::mcp_active(counts.enabled),
                        });
                    } else {
                        // Configured, but every server is disabled: the
                        // definitions are there, none of them run.
                        out.push(AssetReport {
                            kind: AssetKind::McpServers,
                            name: "mcpServers".into(),
                            path: project_mcp.clone(),
                            scope: AssetScope::Project,
                            status: AssetStatus::Blocked,
                            detail: super::text::mcp_all_disabled(counts.total),
                        });
                    }
                }
                // A file that parses and declares nothing is not an asset.
                Ok(_) => {}
                Err(reason) => out.push(AssetReport {
                    kind: AssetKind::McpServers,
                    name: ".mcp.json".into(),
                    path: project_mcp,
                    scope: AssetScope::Project,
                    status: AssetStatus::Blocked,
                    detail: super::text::unreadable_file(&reason),
                }),
            }
        }
        let global_state = ctx.os_home.join(".claude.json");
        if probe_file(&global_state).unwrap_or(false) {
            // The same helper as the project's file — one parse, and the
            // counts come from the loader.
            match mcp_servers(&global_state) {
                // The definitions can be brought over, so the count is the
                // TOTAL: a disabled server is still a definition to move.
                Ok(counts) if counts.total > 0 => out.push(AssetReport {
                    kind: AssetKind::McpServers,
                    name: "mcpServers".into(),
                    path: global_state,
                    scope: AssetScope::User,
                    status: AssetStatus::Actionable,
                    detail: super::text::mcp_copyable(
                        counts.total,
                        &ctx.product.user().join("mcp.json"),
                    ),
                }),
                Ok(_) => {}
                Err(reason) => out.push(AssetReport {
                    kind: AssetKind::McpServers,
                    name: ".claude.json".into(),
                    path: global_state,
                    scope: AssetScope::User,
                    status: AssetStatus::Blocked,
                    detail: super::text::unreadable_file(&reason),
                }),
            }
        }

        // Hooks live in the user settings file.
        let user_settings = user.join("settings.json");
        if probe_file(&user_settings).unwrap_or(false) {
            match settings_hooks(&user_settings) {
                Ok(true) => out.push(AssetReport {
                    kind: AssetKind::Hooks,
                    name: "hooks".into(),
                    path: user_settings,
                    scope: AssetScope::User,
                    status: AssetStatus::Actionable,
                    detail: super::text::hooks_copyable(&ctx.product.user().join("hooks.json")),
                }),
                Ok(false) => {}
                Err(reason) => out.push(AssetReport {
                    kind: AssetKind::Hooks,
                    name: "settings.json".into(),
                    path: user_settings,
                    scope: AssetScope::User,
                    status: AssetStatus::Blocked,
                    detail: super::text::unreadable_file(&reason),
                }),
            }
        }

        // Subagent definitions: our role files need frontmatter Claude's
        // subagent files do not have, so they cannot come over.
        //
        // A directory that exists but cannot be read is a `Blocked` row, not
        // silence — "we found nothing" and "we could not look" are different
        // answers, and only one of them is true.
        let agents = user.join("agents");
        if probe_dir(&agents).unwrap_or(false) {
            match std::fs::read_dir(&agents) {
                Ok(entries) => {
                    for entry in entries {
                        // An entry that cannot be read is a row, not a silent
                        // skip: a subagent file we could not look at is one we
                        // could not say anything true about.
                        let entry = match entry {
                            Ok(entry) => entry,
                            Err(error) => {
                                out.push(AssetReport {
                                    kind: AssetKind::Agents,
                                    name: "agents".into(),
                                    path: agents.clone(),
                                    scope: AssetScope::User,
                                    status: AssetStatus::Blocked,
                                    detail: super::text::unreadable_file(&error.to_string()),
                                });
                                continue;
                            }
                        };
                        let path = entry.path();
                        if path.extension().and_then(|e| e.to_str()) != Some("md") {
                            continue;
                        }
                        out.push(AssetReport {
                            kind: AssetKind::Agents,
                            name: entry.file_name().to_string_lossy().into_owned(),
                            path,
                            scope: AssetScope::User,
                            status: AssetStatus::Blocked,
                            detail: super::text::agents_blocked(),
                        });
                    }
                }
                Err(error) => out.push(AssetReport {
                    kind: AssetKind::Agents,
                    name: "agents".into(),
                    path: agents,
                    scope: AssetScope::User,
                    status: AssetStatus::Blocked,
                    detail: super::text::unreadable_file(&error.to_string()),
                }),
            }
        }

        // Plugins: the manifest format is theirs, not ours.
        let plugins = user.join("plugins");
        if plugins.is_dir() {
            out.push(AssetReport {
                kind: AssetKind::Plugins,
                name: "plugins".into(),
                path: plugins,
                scope: AssetScope::User,
                status: AssetStatus::Blocked,
                detail: super::text::plugins_blocked(),
            });
        }

        // Sessions: the history is lossy to convert, and this release does not.
        let projects = user.join("projects");
        if projects.is_dir() {
            match session_file_count(&projects) {
                Ok(count) if count > 0 => out.push(AssetReport {
                    kind: AssetKind::Sessions,
                    name: "sessions".into(),
                    path: projects,
                    scope: AssetScope::User,
                    status: AssetStatus::Blocked,
                    detail: super::text::sessions_blocked(count),
                }),
                Ok(_) => {}
                Err(reason) => out.push(AssetReport {
                    kind: AssetKind::Sessions,
                    name: "sessions".into(),
                    path: projects,
                    scope: AssetScope::User,
                    status: AssetStatus::Blocked,
                    detail: super::text::unreadable_file(&reason),
                }),
            }
        }

        out
    }

    fn scan_providers(&self, ctx: &ScanContext) -> super::ProviderScan {
        let source_path = ctx.os_home.join(".claude").join("settings.json");
        let config = match super::provider::read_config(&source_path, |raw| {
            let value: serde_json::Value = serde_json::from_str(raw).ok()?;
            (value.is_object() && value.get("env").is_none_or(|env| env.is_object()))
                .then_some(value)
        }) {
            Ok(value) => value,
            Err(()) => {
                return super::ProviderScan {
                    failures: vec![super::provider::unreadable_provider_file(
                        &source_path,
                        AssetScope::User,
                    )],
                    ..Default::default()
                };
            }
        };
        let Some(env) = config.as_ref().and_then(|config| config.get("env")) else {
            return super::ProviderScan::default();
        };
        let value = |name: &str| {
            env.get(name)
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|v| !v.is_empty())
        };
        let base_url = value("ANTHROPIC_BASE_URL");
        let api_key = value("ANTHROPIC_API_KEY");
        let token = value("ANTHROPIC_AUTH_TOKEN");
        if base_url.is_none() && api_key.is_none() && token.is_none() {
            return super::ProviderScan::default();
        }
        let protocol = if base_url.is_some() {
            "anthropic-compatible"
        } else {
            "anthropic"
        };
        let blocked =
            (api_key.is_some() && token.is_some()).then(super::text::ambiguous_claude_auth);
        let api_key = token
            .map(|secret| ApiKeyRef::Bearer(secret.to_owned()))
            .or_else(|| api_key.map(|secret| ApiKeyRef::Plaintext(secret.to_owned())));
        super::ProviderScan {
            drafts: vec![ProviderAccountDraft {
                id: super::provider::account_id_for("claude").unwrap_or_else(|| "claude".into()),
                source_name: "claude".into(),
                protocol: protocol.into(),
                base_url: super::provider::effective_endpoint(protocol, base_url),
                api_key,
                models: Vec::new(),
                source_path,
                scope: AssetScope::User,
                blocked,
            }],
            ..Default::default()
        }
    }
}

/// 仅非空对象或数组声明了 hooks。读取或解析失败由调用方报告。
fn settings_hooks(path: &Path) -> Result<bool, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    Ok(match value.get("hooks") {
        Some(serde_json::Value::Object(hooks)) => !hooks.is_empty(),
        Some(serde_json::Value::Array(hooks)) => !hooks.is_empty(),
        _ => false,
    })
}

/// Read a JSON file. `Err` carries the reason, because "this file is broken"
/// and "this file says nothing" are different reports.
fn parse_json(path: &Path) -> Result<serde_json::Value, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    serde_json::from_str(&raw).map_err(|e| e.to_string())
}

/// Whether our own loader would ACCEPT this permissions file.
///
/// It rejects the WHOLE file when any rule is malformed
/// (`permission_settings.rs:345-353`: `PermissionRules::parse` returns the
/// invalid ones, and a non-empty list fails the load). So a file that parses
/// as JSON but has a bad rule in it applies NONE of its rules — reporting it
/// as "already in effect" would be wrong.
///
/// The check reuses the same rule parser our loader uses, so the two cannot
/// drift.
fn permission_rules_are_valid(value: &serde_json::Value) -> bool {
    // The top level must be an object (`permission_settings.rs:328`: "expected
    // a JSON object"). A `[]`, `null` or a scalar has no `permissions` key,
    // so without this check it would sail through as valid.
    if !value.is_object() {
        return false;
    }
    let rules = value.get("permissions");
    let Some(rules) = rules else {
        // No `permissions` key at all: nothing to reject.
        return true;
    };
    let Some(rules) = rules.as_object() else {
        // `permissions` must be an object (`permission_settings.rs:228`).
        return false;
    };
    let list = |key: &str| -> Vec<String> {
        rules
            .get(key)
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    // `allow` and `deny` must be arrays of strings when present.
    for key in ["allow", "deny"] {
        match rules.get(key) {
            None => {}
            Some(value) => {
                let Some(items) = value.as_array() else {
                    return false;
                };
                if items.iter().any(|v| !v.is_string()) {
                    return false;
                }
            }
        }
    }
    let allow = list("allow");
    let deny = list("deny");
    let (_, invalid) = crate::tools::permission_rules::PermissionRules::parse(&allow, &deny);
    invalid.is_empty()
}

/// What an MCP file declares: how many servers, and how many of them are
/// ENABLED.
///
/// **This calls the real loader** — `crate::mcp::parse_mcp_servers` — rather
/// than re-checking the shape by hand. The loader does typed deserialization,
/// `requirements::validate` for experimental capabilities, and HTTP auth
/// validation; a hand-rolled check that only looked for `command`/`url` would
/// call a file "in effect" that the loader rejects. It also reads the legacy
/// `servers` key as an alias for `mcpServers` (`mcp/config.rs:140`), so
/// counting is left to it too.
///
/// A disabled server is configured but not running, so it must not be counted
/// as "in effect" (`mcp/config.rs:320` filters `!c.disabled`) — but it is
/// still an asset the person configured, and hiding it would make a
/// configured-but-disabled server vanish from the report entirely.
fn mcp_servers(path: &Path) -> Result<McpCounts, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let servers = crate::mcp::parse_mcp_servers(&raw, crate::mcp::McpConfigSource::Project)
        .map_err(|e| e.to_string())?;
    Ok(McpCounts {
        total: servers.len(),
        enabled: servers.iter().filter(|server| !server.disabled).count(),
    })
}

/// How many MCP servers a file declares, and how many run.
struct McpCounts {
    total: usize,
    enabled: usize,
}

/// The provider Claude was pointed at, and how its credential is written.
///
/// `Ok(None)` = the settings file says nothing about a provider. `Err` = the
/// file could not be parsed.
/// How many session files there are under `projects/`.
///
/// `Err` means a directory could not be read — which is **not** the same as
/// "there are none", and the caller reports it as such.
fn session_file_count(projects: &Path) -> Result<usize, String> {
    let mut count = 0usize;
    // **The visited set is what stops a symlink cycle.** `is_dir()` follows
    // links, so a directory that links back to an ancestor would otherwise
    // be walked forever and the read-only report would never finish.
    let mut visited: std::collections::BTreeSet<std::path::PathBuf> =
        std::collections::BTreeSet::new();
    let mut stack = vec![projects.to_path_buf()];
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
                Err(error) => return Err(error),
            }
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::report::{AssetKind, AssetScope, AssetStatus};
    // `PathBuf` is not in scope through `super::*` — the module under test
    // imports only `Path`.
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
    ///
    /// **The same load the report uses** — so a fixture sees what the report
    /// sees. A fixture that loaded its own registry could disagree with the
    /// report for reasons that have nothing to do with the behaviour under
    /// test.
    fn truth(home: &std::path::Path, project: &std::path::Path) -> SkillTruth {
        super::super::SkillTruth::load(&ctx(home, project))
    }

    fn skill_at(root: &std::path::Path, name: &str) {
        let dir = root.join("skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "# skill").unwrap();
    }

    /// Write a file, creating its parent directory first.
    ///
    /// A fixture that writes `<home>/.claude/settings.json` into a directory
    /// that does not exist yet fails at the write, not at the assertion — and
    /// the failure says nothing about the behaviour being checked.
    fn write_file(path: &std::path::Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn a_user_skill_is_active_because_we_read_that_path_today() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        skill_at(&home.path().join(".claude"), "web-access");

        let assets = ClaudeSource.detect(
            &ctx(home.path(), project.path()),
            &truth(home.path(), project.path()),
        );
        let skill = assets
            .iter()
            .find(|a| a.kind == AssetKind::Skills && a.name == "web-access")
            .expect("the skill is reported");
        assert_eq!(skill.status, AssetStatus::Active);
        assert_eq!(skill.scope, AssetScope::User);
    }

    #[test]
    fn a_project_instruction_file_is_active() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_file(&project.path().join("CLAUDE.md"), "# rules");

        let assets = ClaudeSource.detect(
            &ctx(home.path(), project.path()),
            &truth(home.path(), project.path()),
        );
        let instructions = assets
            .iter()
            .find(|a| a.kind == AssetKind::Instructions)
            .expect("the instructions are reported");
        assert_eq!(instructions.status, AssetStatus::Active);
        assert_eq!(instructions.scope, AssetScope::Project);
    }

    #[test]
    fn a_user_instruction_file_is_actionable_because_the_global_layer_cannot_see_it() {
        // The global instruction tier reads <product user tree>/ATOMCODE.md, not
        // ~/.claude/CLAUDE.md — so this one has to be copied, not merely read.
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_file(&home.path().join(".claude").join("CLAUDE.md"), "# rules");

        let assets = ClaudeSource.detect(
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
    fn a_project_instruction_file_shadowed_by_agents_md_is_not_active() {
        // The project tier takes the FIRST existing name in order, so a
        // CLAUDE.md next to an AGENTS.md is never loaded. Calling it "already
        // in effect" would be a lie.
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_file(&project.path().join("AGENTS.md"), "# rules");
        write_file(&project.path().join("CLAUDE.md"), "# rules");

        let assets = ClaudeSource.detect(
            &ctx(home.path(), project.path()),
            &truth(home.path(), project.path()),
        );
        let instructions = assets
            .iter()
            .find(|a| a.kind == AssetKind::Instructions)
            .expect("the instructions are reported");
        assert_eq!(instructions.status, AssetStatus::Blocked);
        assert!(
            instructions.detail.contains("AGENTS.md"),
            "it says what shadows it: {}",
            instructions.detail
        );
    }

    #[test]
    fn a_user_settings_file_is_actionable_and_the_project_one_is_active() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_file(&home.path().join(".claude").join("settings.json"), "{}");
        write_file(&project.path().join(".claude").join("settings.json"), "{}");

        let assets = ClaudeSource.detect(
            &ctx(home.path(), project.path()),
            &truth(home.path(), project.path()),
        );
        let permissions: Vec<_> = assets
            .iter()
            .filter(|a| a.kind == AssetKind::Permissions)
            .collect();
        assert_eq!(permissions.len(), 2);
        assert!(permissions
            .iter()
            .any(|p| p.scope == AssetScope::User && p.status == AssetStatus::Actionable));
        assert!(permissions
            .iter()
            .any(|p| p.scope == AssetScope::Project && p.status == AssetStatus::Active));
    }

    #[test]
    fn a_broken_settings_file_is_blocked_not_active() {
        // Our own loader refuses a broken policy (`permission_settings.rs:296`),
        // so a broken file must not be reported as already in effect.
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_file(
            &project.path().join(".claude").join("settings.json"),
            "{not json",
        );

        let assets = ClaudeSource.detect(
            &ctx(home.path(), project.path()),
            &truth(home.path(), project.path()),
        );
        let permissions = assets
            .iter()
            .find(|a| a.kind == AssetKind::Permissions)
            .expect("the broken file is reported");
        assert_eq!(permissions.status, AssetStatus::Blocked);
    }

    #[test]
    fn only_nonempty_hook_collections_are_reported() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let ctx = ctx(home.path(), project.path());
        let truth = truth(home.path(), project.path());
        let settings = home.path().join(".claude/settings.json");
        for (hooks, expected) in [
            ("{}", false),
            ("[]", false),
            ("true", false),
            (
                r#"{"PreToolUse":[{"hooks":[{"type":"command","command":"echo test"}]}]}"#,
                true,
            ),
            (r#"[{"type":"command","command":"echo test"}]"#, true),
        ] {
            write_file(&settings, &format!(r#"{{"hooks":{hooks}}}"#));
            let rows = ClaudeSource.detect(&ctx, &truth);
            let status = rows
                .iter()
                .find(|row| row.kind == AssetKind::Hooks)
                .map(|row| row.status);
            assert_eq!(
                status,
                expected.then_some(AssetStatus::Actionable),
                "{hooks}"
            );
        }
    }

    #[test]
    fn a_project_mcp_file_is_active_and_the_global_one_is_actionable() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_file(
            &project.path().join(".mcp.json"),
            r#"{"mcpServers":{"fs":{"command":"mcp-fs"}}}"#,
        );
        write_file(
            &home.path().join(".claude.json"),
            r#"{"mcpServers":{"gh":{"command":"mcp-gh"}}}"#,
        );

        let assets = ClaudeSource.detect(
            &ctx(home.path(), project.path()),
            &truth(home.path(), project.path()),
        );
        let mcp: Vec<_> = assets
            .iter()
            .filter(|a| a.kind == AssetKind::McpServers)
            .collect();
        assert_eq!(mcp.len(), 2);
        assert!(mcp
            .iter()
            .any(|m| m.scope == AssetScope::Project && m.status == AssetStatus::Active));
        assert!(mcp
            .iter()
            .any(|m| m.scope == AssetScope::User && m.status == AssetStatus::Actionable));
    }

    #[test]
    fn an_actionable_row_names_the_path_to_copy_it_to() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_file(&home.path().join(".claude").join("CLAUDE.md"), "# rules");

        let assets = ClaudeSource.detect(
            &ctx(home.path(), project.path()),
            &truth(home.path(), project.path()),
        );
        let instructions = assets
            .iter()
            .find(|a| a.kind == AssetKind::Instructions)
            .expect("the instructions are reported");
        assert!(
            instructions.detail.contains("ATOMCODE.md"),
            "the row says where to put it: {}",
            instructions.detail
        );
    }

    #[test]
    fn a_subagent_definition_is_blocked_because_the_formats_differ() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let dir = home.path().join(".claude").join("agents");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("reviewer.md"), "---").unwrap();

        let assets = ClaudeSource.detect(
            &ctx(home.path(), project.path()),
            &truth(home.path(), project.path()),
        );
        let agent = assets
            .iter()
            .find(|a| a.kind == AssetKind::Agents)
            .expect("the agent definition is reported");
        assert_eq!(agent.status, AssetStatus::Blocked);
    }

    #[test]
    fn a_broken_settings_file_does_not_stop_the_scan() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_file(
            &home.path().join(".claude").join("settings.json"),
            "{not json",
        );
        skill_at(&home.path().join(".claude"), "web-access");

        let assets = ClaudeSource.detect(
            &ctx(home.path(), project.path()),
            &truth(home.path(), project.path()),
        );
        // The skill is still found, and the broken file is reported as blocked
        // rather than taking the whole scan down.
        assert!(assets.iter().any(|a| a.kind == AssetKind::Skills));
        assert!(assets
            .iter()
            .any(|a| a.kind == AssetKind::Permissions && a.status == AssetStatus::Blocked));
    }

    #[cfg(unix)]
    #[test]
    fn a_session_subtree_probe_error_is_reported() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let sessions = home.path().join(".claude/projects");
        std::fs::create_dir_all(&sessions).unwrap();
        // 自指链接查询元数据时稳定失败，不依赖运行测试的用户权限。
        let child = sessions.join("loop");
        std::os::unix::fs::symlink("loop", &child).unwrap();
        let reason = probe_dir(&child).unwrap_err();

        let rows = ClaudeSource.detect(
            &ctx(home.path(), project.path()),
            &truth(home.path(), project.path()),
        );
        let row = rows
            .iter()
            .find(|row| row.kind == AssetKind::Sessions)
            .unwrap();
        assert_eq!(row.path, sessions);
        assert_eq!(row.status, AssetStatus::Blocked);
        assert_eq!(row.detail, super::super::text::unreadable_file(&reason));
    }

    #[test]
    fn no_claude_directory_means_no_rows() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let assets = ClaudeSource.detect(
            &ctx(home.path(), project.path()),
            &truth(home.path(), project.path()),
        );
        assert!(assets.is_empty());
    }
}
