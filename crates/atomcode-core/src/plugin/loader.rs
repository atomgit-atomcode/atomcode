//! Helpers for downstream registries (skill / commands / hooks) to discover
//! every installed plugin's asset directories in one pass.

use std::path::PathBuf;

use super::manifest::{load_plugin_manifest, PluginManifest};
use super::paths;
use super::state::load_installed_plugins_file;

#[derive(Debug, Clone)]
pub struct InstalledPluginAssets {
    pub plugin: String,
    pub marketplace: String,
    pub plugin_dir: PathBuf,
    pub manifest: PluginManifest,
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

/// Iterate over every installed plugin. Returns empty Vec when state file is
/// missing or the plugin home is not configured. Skips entries whose
/// plugin_dir does not exist on disk (keeps reload resilient to deletions).
pub fn iter_installed_plugin_assets() -> Vec<InstalledPluginAssets> {
    let Some(state_path) = paths::installed_plugins_file() else { return vec![]; };
    let state = match load_installed_plugins_file(&state_path) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    let Some(plugins_root) = paths::plugins_root() else { return vec![]; };

    state
        .plugins
        .into_values()
        .filter_map(|e| {
            let abs = plugins_root.join(&e.plugin_dir);
            if !abs.exists() {
                return None;
            }
            let manifest = load_plugin_manifest(&abs).unwrap_or_default();
            Some(InstalledPluginAssets {
                plugin: e.plugin,
                marketplace: e.marketplace,
                plugin_dir: abs,
                manifest,
            })
        })
        .collect()
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
        install("p", "p").unwrap();
        let assets = iter_installed_plugin_assets();
        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].plugin, "p");
        assert!(assets[0].skills_dir().exists());
    }
}
