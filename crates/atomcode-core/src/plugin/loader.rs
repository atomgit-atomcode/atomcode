//! Helpers for downstream registries (skill / commands / hooks) to discover
//! every installed plugin's asset directories in one pass.

use std::path::PathBuf;

use super::manifest::{load_plugin_manifest, PluginManifest};
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
/// neutral (engine-agnostic) shape. The legacy engine maps these via
/// `crate::hook::json_config::cc_hooks_to_atomcode`; the new (kernel) stack — which
/// cannot depend on this crate's manifest types — consumes this DTO through the bridge
/// and maps it onto `atomcode_capabilities::cc_hooks::HookConfig`.
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

/// Flatten every installed plugin's INLINE CC hooks (`plugin.json` `hooks` blocks) into
/// neutral [`PluginCcHook`] specs. Only `type: "command"` specs are returned. Plugins
/// that ship a legacy hooks.json file (rather than inline hooks) are NOT included here —
/// the legacy engine handles those separately. Empty when no plugin contributes inline
/// hooks, so the caller can skip wiring entirely.
pub fn installed_plugin_cc_hooks() -> Vec<PluginCcHook> {
    let mut out = Vec::new();
    for assets in iter_installed_plugin_assets() {
        let Some(cc_map) = assets.manifest.inline_cc_hooks() else {
            continue;
        };
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
                        plugin_root: assets.plugin_dir.clone(),
                    });
                }
            }
        }
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

    fn make_repo(name: &str) -> PathBuf {
        let work = tempfile::tempdir().unwrap().keep();
        let repo = work.join(name);
        std::fs::create_dir_all(&repo).unwrap();
        Command::new("git").args(["init", "-q"]).current_dir(&repo).status().unwrap();
        Command::new("git").args(["config", "user.email", "t@t"]).current_dir(&repo).status().unwrap();
        Command::new("git").args(["config", "user.name", "t"]).current_dir(&repo).status().unwrap();
        std::fs::create_dir_all(repo.join("skills/foo")).unwrap();
        std::fs::write(
            repo.join("skills/foo/SKILL.md"),
            "---\nname: foo\ndescription: f\n---\nbody",
        )
        .unwrap();
        Command::new("git").args(["add", "-A"]).current_dir(&repo).status().unwrap();
        Command::new("git").args(["commit", "-q", "-m", "init"]).current_dir(&repo).status().unwrap();
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

    /// Debug test: dump the real-world installed plugins + skill loading.
    #[test]
    fn debug_real_world_plugins() {
        let assets = iter_installed_plugin_assets();
        eprintln!("=== DEBUG: {} installed plugin assets ===", assets.len());
        for a in &assets {
            eprintln!("  plugin={} marketplace={} plugin_dir={:?} skills_path={:?} skills_dirs={:?}",
                a.plugin, a.marketplace, a.plugin_dir, a.manifest.skills_path(), a.skills_dirs());
            for sd in a.skills_dirs() {
                eprintln!("    skills_dir {:?} exists={}", sd, sd.exists());
                if sd.is_dir() {
                    for entry in std::fs::read_dir(&sd).unwrap().flatten() {
                        let p = entry.path();
                        let name = p.file_name().unwrap().to_string_lossy();
                        let is_dir = p.is_dir();
                        let has_skill_md = p.join("SKILL.md").exists();
                        eprintln!("      {} is_dir={} has_skill_md={}", name, is_dir, has_skill_md);
                        if is_dir && has_skill_md {
                            let content = std::fs::read_to_string(p.join("SKILL.md")).unwrap();
                            eprintln!("        SKILL.md first 100 chars: {:?}", &content.chars().take(100).collect::<String>());
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
        eprintln!("=== DEBUG: {} skills loaded, {} warnings ===", reg.all().count(), warnings.len());
        for w in &warnings {
            eprintln!("  WARNING: {}", w);
        }
        for s in reg.all() {
            eprintln!("  SKILL: {} - {}", s.name, s.description.chars().take(60).collect::<String>());
        }
    }
}
