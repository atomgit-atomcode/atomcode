//! Helpers for downstream registries (skill / commands / hooks) to discover
//! every installed plugin's asset directories in one pass.

use std::path::{Path, PathBuf};

use super::manifest::{load_plugin_manifest, CCHooksMap, PluginManifest};
use super::paths;
use super::state::{load_installed_plugins_file, InstallScope};

#[derive(Debug, Clone)]
pub struct InstalledPluginAssets {
    pub plugin: String,
    pub marketplace: String,
    pub plugin_dir: PathBuf,
    pub manifest: PluginManifest,
    /// Installation scope.
    pub scope: InstallScope,
}

impl InstalledPluginAssets {
    /// Primary skills directory — the first entry from the manifest's
    /// `skills` field, or the default `"skills"` when absent.
    pub fn skills_dir(&self) -> PathBuf {
        self.plugin_dir.join(self.manifest.skills_path())
    }
    /// All skills directories declared in the manifest.
    ///
    /// When `skills` is absent this returns a single default `"skills"`
    /// entry (same as `skills_dir()`). When it is a CC-style array
    /// (`["./skills/foo", "./skills/bar"]`) each entry is resolved
    /// relative to `plugin_dir`, allowing multiple skill directories
    /// to contribute.
    pub fn skills_dirs(&self) -> Vec<PathBuf> {
        self.manifest
            .skills_paths()
            .into_iter()
            .map(|p| self.plugin_dir.join(p))
            .collect()
    }
    pub fn commands_dir(&self) -> PathBuf {
        self.plugin_dir.join(self.manifest.commands_path())
    }
    pub fn hooks_file(&self) -> PathBuf {
        self.plugin_dir.join(self.manifest.hooks_path())
    }
}

/// A single CC hook contributed INLINE by an installed plugin's `plugin.json`, in a
/// neutral (engine-agnostic) shape. Host adapters map this DTO onto
/// `atomcode_capabilities::cc_hooks::HookConfig` before starting CodingRuntime, so
/// the kernel stack does not depend on this crate's manifest types.
#[derive(Debug, Clone)]
pub struct PluginCcHook {
    /// CC PascalCase event name (`PreToolUse`, `UserPromptSubmit`, ...).
    pub event: String,
    /// Optional tool-name matcher for `PreToolUse`/`PostToolUse`.
    pub matcher: Option<String>,
    /// The shell command to run.
    pub command: String,
    /// CC timeout in SECONDS (the consumer converts to ms; `None` ⇒ its default).
    pub timeout_secs: Option<u64>,
    /// Plugin install dir — exported as `CLAUDE_PLUGIN_ROOT`/`ATOMCODE_PLUGIN_ROOT`.
    pub plugin_root: PathBuf,
}

/// Expand a CC hooks map (`{Event: [{matcher?, hooks:[{type,command,timeout}]}]}`)
/// into neutral `PluginCcHook` specs. Only `type: "command"` specs are kept.
fn expand_cc_hooks(cc_map: &CCHooksMap, plugin_root: &Path) -> Vec<PluginCcHook> {
    let mut out = Vec::new();
    for (event, groups) in cc_map {
        for group in groups {
            for spec in &group.hooks {
                if spec.kind != "command" {
                    continue;
                }
                out.push(PluginCcHook {
                    event: event.clone(),
                    matcher: group.matcher.clone(),
                    command: spec.command.clone(),
                    timeout_secs: spec.timeout,
                    plugin_root: plugin_root.to_path_buf(),
                });
            }
        }
    }
    out
}

/// Parse a CC-format hooks file. `None` on missing/malformed (skip, never wedge).
fn load_cc_hooks_file(path: &Path) -> Option<CCHooksMap> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<CCHooksMap>(&raw).ok()
}

/// File-based plugin hooks, drilling the CC default layout: `hooks/hooks.json`
/// (CC convention) then `hooks.json` (legacy flat). First existing+parseable wins.
pub fn plugin_file_cc_hooks(plugin_dir: &Path) -> Vec<PluginCcHook> {
    for rel in ["hooks/hooks.json", "hooks.json"] {
        let path = plugin_dir.join(rel);
        if path.exists() {
            if let Some(map) = load_cc_hooks_file(&path) {
                return expand_cc_hooks(&map, plugin_dir);
            }
        }
    }
    Vec::new()
}

/// Drop duplicate hooks by `(event, matcher, command)`, keeping first occurrence —
/// so a plugin declaring the same hook both inline and in a file never double-fires.
/// The matcher key normalizes `None` and `Some("")` to the same empty string so they
/// collapse correctly (matching the hash normalization in `plugin_hook_set_hash`).
fn dedup_hooks(hooks: Vec<PluginCcHook>) -> Vec<PluginCcHook> {
    let mut seen = std::collections::HashSet::new();
    hooks
        .into_iter()
        .filter(|h| {
            seen.insert((
                h.event.clone(),
                h.matcher.clone().unwrap_or_default(),
                h.command.clone(),
            ))
        })
        .collect()
}

/// Like `plugin_file_cc_hooks` but honors a `plugin.json`-declared custom hooks
/// path (`"hooks": "custom/x.json"`) first, then falls back to the default drill.
fn plugin_file_cc_hooks_for(plugin_dir: &Path, manifest: &PluginManifest) -> Vec<PluginCcHook> {
    if let Some(super::manifest::HooksField::Path(p)) = &manifest.hooks {
        let path = plugin_dir.join(p);
        if path.exists() {
            // Declared path is authoritative when present — do NOT fall back to
            // the default drill if it fails to parse (that would load hooks the
            // plugin never declared under this path).
            return load_cc_hooks_file(&path)
                .map(|m| expand_cc_hooks(&m, plugin_dir))
                .unwrap_or_default();
        }
    }
    plugin_file_cc_hooks(plugin_dir)
}

/// All CC hooks a plugin contributes: inline (`plugin.json` hooks) + file-based
/// (`hooks/hooks.json` / `hooks.json`), deduped by `(event, matcher, command)`.
fn plugin_all_cc_hooks(assets: &InstalledPluginAssets) -> Vec<PluginCcHook> {
    let mut hooks = Vec::new();
    if let Some(cc_map) = assets.manifest.inline_cc_hooks() {
        hooks.extend(expand_cc_hooks(cc_map, &assets.plugin_dir));
    }
    hooks.extend(plugin_file_cc_hooks_for(
        &assets.plugin_dir,
        &assets.manifest,
    ));
    dedup_hooks(hooks)
}

/// Flatten every installed plugin's CC hooks (inline + file) — but ONLY for
/// plugins whose current hook-set hash the user has trusted. Untrusted plugins'
/// hooks are withheld (see `installed_plugin_hook_trust_status` for surfacing).
pub fn installed_plugin_cc_hooks() -> Vec<PluginCcHook> {
    let trust = crate::plugin::hook_trust::load_trust();
    let mut out = Vec::new();
    for assets in iter_installed_plugin_assets() {
        let hooks = plugin_all_cc_hooks(&assets);
        if hooks.is_empty() {
            continue;
        }
        let hash = crate::plugin::hook_trust::plugin_hook_set_hash(&hooks);
        let id = crate::plugin::hook_trust::plugin_id(&assets.plugin, &assets.marketplace);
        if crate::plugin::hook_trust::is_trusted(&trust, &id, &hash) {
            out.extend(hooks);
        }
    }
    out
}

/// Per-plugin hook trust status for surfacing (install notice / `hooks list` /
/// `plugin trust`). Only plugins that actually ship hooks appear.
#[derive(Debug, Clone)]
pub struct PluginHookTrust {
    pub plugin: String,
    pub marketplace: String,
    pub plugin_id: String,
    pub hook_count: usize,
    pub events: Vec<String>,
    pub hash: String,
    pub trusted: bool,
}

pub fn installed_plugin_hook_trust_status() -> Vec<PluginHookTrust> {
    let trust = crate::plugin::hook_trust::load_trust();
    let mut out = Vec::new();
    for assets in iter_installed_plugin_assets() {
        let hooks = plugin_all_cc_hooks(&assets);
        if hooks.is_empty() {
            continue;
        }
        let hash = crate::plugin::hook_trust::plugin_hook_set_hash(&hooks);
        let id = crate::plugin::hook_trust::plugin_id(&assets.plugin, &assets.marketplace);
        let mut events: Vec<String> = hooks.iter().map(|h| h.event.clone()).collect();
        events.sort();
        events.dedup();
        out.push(PluginHookTrust {
            trusted: crate::plugin::hook_trust::is_trusted(&trust, &id, &hash),
            plugin: assets.plugin,
            marketplace: assets.marketplace,
            plugin_id: id,
            hook_count: hooks.len(),
            events,
            hash,
        });
    }
    out
}

/// Iterate over every installed plugin across all scopes. Returns empty Vec when state file is
/// missing or the plugin home is not configured. Skips entries whose
/// plugin_dir does not exist on disk (keeps reload resilient to deletions).
pub fn iter_installed_plugin_assets() -> Vec<InstalledPluginAssets> {
    let mut result = Vec::new();

    // User scope (global).
    if let Some(state_path) = paths::installed_plugins_file() {
        if let Ok(state) = load_installed_plugins_file(&state_path) {
            if let Some(plugins_root) = paths::plugins_root() {
                for e in state.plugins.into_values() {
                    let abs = plugins_root.join(&e.plugin_dir);
                    if !abs.exists() {
                        continue;
                    }
                    let mut manifest = load_plugin_manifest(&abs).unwrap_or_default();
                    // Auto-detect: when no plugin.json was found (manifest is default)
                    // AND the plugin_dir itself contains a SKILL.md, the directory IS
                    // the skill (common with git-subdir installs from CC marketplaces
                    // like claude-plugins-official). Without this, skills_path()
                    // defaults to "skills" and the loader looks for <dir>/skills/ —
                    // which doesn't exist, so the installed skill is silently ignored.
                    if manifest.skills.is_none() && abs.join("SKILL.md").exists() {
                        manifest.skills = Some(super::manifest::PathOrList::One("./".into()));
                    }
                    result.push(InstalledPluginAssets {
                        plugin: e.plugin,
                        marketplace: e.marketplace,
                        plugin_dir: abs,
                        manifest,
                        scope: e.scope,
                    });
                }
            }
        }
    }

    // Project and Local scopes.
    let working_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    for scope in [InstallScope::Project, InstallScope::Local] {
        if let Some(project_root) = paths::project_plugins_root(&working_dir, &scope) {
            if let Some(state_path) = paths::project_installed_plugins_file(&working_dir, &scope) {
                if state_path.exists() {
                    if let Ok(state) = load_installed_plugins_file(&state_path) {
                        for e in state.plugins.into_values() {
                            let abs = project_root.join(&e.plugin_dir);
                            if !abs.exists() {
                                continue;
                            }
                            let mut manifest = load_plugin_manifest(&abs).unwrap_or_default();
                            if manifest.skills.is_none() && abs.join("SKILL.md").exists() {
                                manifest.skills =
                                    Some(super::manifest::PathOrList::One("./".into()));
                            }
                            result.push(InstalledPluginAssets {
                                plugin: e.plugin,
                                marketplace: e.marketplace,
                                plugin_dir: abs,
                                manifest,
                                scope: e.scope,
                            });
                        }
                    }
                }
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::installer::install;
    use crate::plugin::marketplace::add_marketplace;
    use crate::plugin::test_support::isolated_home;
    use std::path::PathBuf;
    use std::process::Command;

    #[test]
    fn file_cc_hooks_parses_cc_schema_from_hooks_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::create_dir_all(dir.join("hooks")).unwrap();
        std::fs::write(
            dir.join("hooks/hooks.json"),
            r#"{"SessionStart":[{"hooks":[{"type":"command","command":"echo hi"}]}]}"#,
        )
        .unwrap();
        let hooks = plugin_file_cc_hooks(dir);
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].event, "SessionStart");
        assert_eq!(hooks[0].command, "echo hi");
        assert_eq!(hooks[0].plugin_root, dir);
    }

    #[test]
    fn file_cc_hooks_falls_back_to_root_hooks_json() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("hooks.json"),
            r#"{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"c"}]}]}"#,
        )
        .unwrap();
        let hooks = plugin_file_cc_hooks(tmp.path());
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].event, "PreToolUse");
        assert_eq!(hooks[0].matcher.as_deref(), Some("Bash"));
    }

    #[test]
    fn file_cc_hooks_none_when_absent_or_malformed() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(plugin_file_cc_hooks(tmp.path()).is_empty());
        std::fs::create_dir_all(tmp.path().join("hooks")).unwrap();
        std::fs::write(tmp.path().join("hooks/hooks.json"), "{ not json").unwrap();
        assert!(plugin_file_cc_hooks(tmp.path()).is_empty());
    }

    #[test]
    fn dedup_hooks_drops_identical_event_matcher_command() {
        let mk = |c: &str| PluginCcHook {
            event: "SessionStart".into(),
            matcher: None,
            command: c.into(),
            timeout_secs: None,
            plugin_root: PathBuf::from("/x"),
        };
        let out = dedup_hooks(vec![mk("a"), mk("a"), mk("b")]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].command, "a");
        assert_eq!(out[1].command, "b");
    }

    #[test]
    fn custom_hooks_path_malformed_does_not_fall_back_to_default() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::create_dir_all(dir.join("cfg")).unwrap();
        std::fs::write(dir.join("cfg/h.json"), "{ not json").unwrap();
        std::fs::create_dir_all(dir.join("hooks")).unwrap();
        std::fs::write(
            dir.join("hooks/hooks.json"),
            r#"{"SessionStart":[{"hooks":[{"type":"command","command":"echo default"}]}]}"#,
        )
        .unwrap();
        let manifest: crate::plugin::manifest::PluginManifest =
            serde_json::from_str(r#"{"name":"p","hooks":"cfg/h.json"}"#).unwrap();
        let hooks = plugin_file_cc_hooks_for(dir, &manifest);
        assert!(
            hooks.is_empty(),
            "malformed declared path must not fall back to default drill"
        );
    }

    #[test]
    fn dedup_collapses_none_and_empty_matcher() {
        let mk = |m: Option<&str>| PluginCcHook {
            event: "SessionStart".into(),
            matcher: m.map(|s| s.into()),
            command: "c".into(),
            timeout_secs: None,
            plugin_root: std::path::PathBuf::from("/x"),
        };
        let out = dedup_hooks(vec![mk(None), mk(Some(""))]);
        assert_eq!(out.len(), 1);
    }

    fn make_repo(name: &str) -> PathBuf {
        let work = tempfile::tempdir().unwrap().keep();
        let repo = work.join(name);
        std::fs::create_dir_all(&repo).unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "t@t"])
            .current_dir(&repo)
            .status()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "t"])
            .current_dir(&repo)
            .status()
            .unwrap();
        std::fs::create_dir_all(repo.join("skills/foo")).unwrap();
        std::fs::write(
            repo.join("skills/foo/SKILL.md"),
            "---\nname: foo\ndescription: f\n---\nbody",
        )
        .unwrap();
        Command::new("git")
            .args(["add", "-A"])
            .current_dir(&repo)
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "-q", "-m", "init"])
            .current_dir(&repo)
            .status()
            .unwrap();
        repo
    }

    #[test]
    #[serial_test::serial]
    fn iter_yields_installed() {
        let _home = isolated_home();
        let repo = make_repo("p");
        add_marketplace(&format!("file://{}", repo.display())).unwrap();
        install("p", "p", InstallScope::User).unwrap();
        let assets = iter_installed_plugin_assets();
        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].plugin, "p");
        assert!(assets[0].skills_dir().exists());
        assert_eq!(assets[0].scope, InstallScope::User);
    }

    #[test]
    #[serial_test::serial]
    fn cc_hooks_filtered_by_trust() {
        let _home = crate::plugin::test_support::isolated_home();
        // repo with a CC file-based SessionStart hook
        let work = tempfile::tempdir().unwrap().keep();
        let repo = work.join("hp");
        std::fs::create_dir_all(repo.join("hooks")).unwrap();
        std::fs::write(
            repo.join("hooks/hooks.json"),
            r#"{"SessionStart":[{"hooks":[{"type":"command","command":"echo hi"}]}]}"#,
        )
        .unwrap();
        for a in [["init", "-q"].as_slice()] {
            std::process::Command::new("git")
                .args(a)
                .current_dir(&repo)
                .status()
                .unwrap();
        }
        std::process::Command::new("git")
            .args(["config", "user.email", "t@t"])
            .current_dir(&repo)
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "t"])
            .current_dir(&repo)
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&repo)
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-q", "-m", "i"])
            .current_dir(&repo)
            .status()
            .unwrap();
        crate::plugin::marketplace::add_marketplace(&format!("file://{}", repo.display())).unwrap();
        crate::plugin::installer::install("hp", "hp", InstallScope::User).unwrap();

        // Untrusted by default → no hooks loaded, but status reports it.
        assert!(installed_plugin_cc_hooks().is_empty());
        let status = installed_plugin_hook_trust_status();
        let e = status
            .iter()
            .find(|s| s.plugin == "hp")
            .expect("status entry");
        assert_eq!(e.hook_count, 1);
        assert!(!e.trusted);
        assert_eq!(e.events, vec!["SessionStart".to_string()]);

        // Trust → hooks load.
        crate::plugin::hook_trust::trust(&e.plugin_id, &e.hash).unwrap();
        assert_eq!(installed_plugin_cc_hooks().len(), 1);
    }

    #[test]
    #[serial_test::serial]
    fn grandfather_blesses_existing_then_new_installs_stay_untrusted() {
        use crate::plugin::hook_trust::ensure_migrated;
        let _home = crate::plugin::test_support::isolated_home();
        // helper: a git repo shipping a file-based SessionStart hook
        let mk_hook_repo = |name: &str, cmd: &str| {
            let work = tempfile::tempdir().unwrap().keep();
            let repo = work.join(name);
            std::fs::create_dir_all(repo.join("hooks")).unwrap();
            std::fs::write(
                repo.join("hooks/hooks.json"),
                format!(
                    r#"{{"SessionStart":[{{"hooks":[{{"type":"command","command":"{cmd}"}}]}}]}}"#
                ),
            )
            .unwrap();
            for args in [
                ["init", "-q"].as_slice(),
                &["config", "user.email", "t@t"],
                &["config", "user.name", "t"],
                &["add", "-A"],
                &["commit", "-q", "-m", "i"],
            ] {
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(&repo)
                    .status()
                    .unwrap();
            }
            repo
        };
        // Existing plugin installed BEFORE migration.
        let repo1 = mk_hook_repo("hp1", "echo one");
        crate::plugin::marketplace::add_marketplace(&format!("file://{}", repo1.display()))
            .unwrap();
        crate::plugin::installer::install("hp1", "hp1", InstallScope::User).unwrap();
        assert!(
            installed_plugin_cc_hooks().is_empty(),
            "untrusted before migration"
        );

        // Upgrade boundary → grandfather blesses the existing plugin.
        ensure_migrated();
        assert_eq!(
            installed_plugin_cc_hooks().len(),
            1,
            "existing plugin grandfathered"
        );

        // A NEW plugin installed AFTER migration stays untrusted (marker set).
        let repo2 = mk_hook_repo("hp2", "echo two");
        crate::plugin::marketplace::add_marketplace(&format!("file://{}", repo2.display()))
            .unwrap();
        crate::plugin::installer::install("hp2", "hp2", InstallScope::User).unwrap();
        ensure_migrated(); // idempotent no-op (marker exists)
        let loaded = installed_plugin_cc_hooks();
        assert_eq!(
            loaded.len(),
            1,
            "new post-migration plugin NOT auto-trusted"
        );
        assert!(loaded.iter().all(|h| h.command == "echo one"));
    }

    #[test]
    #[serial_test::serial]
    fn custom_hooks_path_from_manifest_is_honored() {
        let _home = crate::plugin::test_support::isolated_home();
        let work = tempfile::tempdir().unwrap().keep();
        let repo = work.join("cp");
        std::fs::create_dir_all(repo.join("cfg")).unwrap();
        std::fs::write(
            repo.join("plugin.json"),
            r#"{"name":"cp","hooks":"cfg/h.json"}"#,
        )
        .unwrap();
        std::fs::write(
            repo.join("cfg/h.json"),
            r#"{"SessionStart":[{"hooks":[{"type":"command","command":"echo custom"}]}]}"#,
        )
        .unwrap();
        for args in [
            ["init", "-q"].as_slice(),
            &["config", "user.email", "t@t"],
            &["config", "user.name", "t"],
            &["add", "-A"],
            &["commit", "-q", "-m", "i"],
        ] {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .status()
                .unwrap();
        }
        crate::plugin::marketplace::add_marketplace(&format!("file://{}", repo.display())).unwrap();
        crate::plugin::installer::install("cp", "cp", InstallScope::User).unwrap();
        let status = installed_plugin_hook_trust_status();
        let e = status.iter().find(|s| s.plugin == "cp").expect("has hooks");
        crate::plugin::hook_trust::trust(&e.plugin_id, &e.hash).unwrap();
        let loaded = installed_plugin_cc_hooks();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].command, "echo custom");
    }

    /// Debug test: dump the real-world installed plugins + skill loading.
    #[test]
    fn debug_real_world_plugins() {
        let assets = iter_installed_plugin_assets();
        eprintln!("=== DEBUG: {} installed plugin assets ===", assets.len());
        for a in &assets {
            eprintln!(
                "  plugin={} marketplace={} plugin_dir={:?} skills_path={:?} skills_dirs={:?}",
                a.plugin,
                a.marketplace,
                a.plugin_dir,
                a.manifest.skills_path(),
                a.skills_dirs()
            );
            for sd in a.skills_dirs() {
                eprintln!("    skills_dir {:?} exists={}", sd, sd.exists());
                if sd.is_dir() {
                    for entry in std::fs::read_dir(&sd).unwrap().flatten() {
                        let p = entry.path();
                        let name = p.file_name().unwrap().to_string_lossy();
                        let is_dir = p.is_dir();
                        let has_skill_md = p.join("SKILL.md").exists();
                        eprintln!(
                            "      {} is_dir={} has_skill_md={}",
                            name, is_dir, has_skill_md
                        );
                        if is_dir && has_skill_md {
                            let content = std::fs::read_to_string(p.join("SKILL.md")).unwrap();
                            eprintln!(
                                "        SKILL.md first 100 chars: {:?}",
                                &content.chars().take(100).collect::<String>()
                            );
                            let _result = crate::skill::SkillRegistry::new();
                            // Try parsing just this one skill
                            let mut tmp_reg = crate::skill::SkillRegistry::new();
                            let mut warnings = Vec::new();
                            tmp_reg.load_skills_dir(&sd, Some("__test__"), &mut warnings);
                            for w in &warnings {
                                eprintln!("        WARNING: {}", w);
                            }
                        }
                    }
                }
            }
        }
        let mut reg = crate::skill::SkillRegistry::new();
        let warnings = reg.reload(std::path::Path::new("/tmp"));
        eprintln!(
            "=== DEBUG: {} skills loaded, {} warnings ===",
            reg.all().count(),
            warnings.len()
        );
        for w in &warnings {
            eprintln!("  WARNING: {}", w);
        }
        for s in reg.all() {
            eprintln!(
                "  SKILL: {} - {}",
                s.name,
                s.description.chars().take(60).collect::<String>()
            );
        }
    }
}
