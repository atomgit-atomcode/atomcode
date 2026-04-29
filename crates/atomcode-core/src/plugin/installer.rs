use anyhow::{anyhow, bail, Result};
use std::path::{Component, Path};

use super::manifest::PluginEntry;
use super::marketplace::sanitize_name;
use super::paths;
use super::state::{
    load_installed_plugins_file, load_marketplaces_file, plugin_id, save_installed_plugins_file,
    InstalledPluginEntry,
};

#[derive(Debug, Clone)]
pub struct InstalledPluginInfo {
    pub plugin: String,
    pub marketplace: String,
    pub plugin_dir: String,
}

/// Validate that a plugin source path (declared in marketplace.json) only
/// contains plain forward components. Reject `..`, absolute paths, and any
/// other non-`Normal` component to prevent escaping the marketplace root.
fn validate_plugin_source(source: &str) -> Result<()> {
    if source.is_empty() {
        return Ok(());
    }
    let p = Path::new(source);
    for comp in p.components() {
        match comp {
            Component::Normal(s) => {
                let s = s.to_string_lossy();
                if s.is_empty() || s == ".." || s.contains('\0') {
                    bail!("plugin source path '{}' contains disallowed components", source);
                }
            }
            Component::CurDir => {
                // "./" is fine; skip.
            }
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("plugin source path '{}' contains disallowed components", source);
            }
        }
    }
    Ok(())
}

pub fn install(plugin: &str, marketplace: &str) -> Result<InstalledPluginInfo> {
    let mp_state = load_marketplaces_file(&paths::marketplaces_file().unwrap())?;
    let entry = mp_state
        .marketplaces
        .get(marketplace)
        .ok_or_else(|| anyhow!("marketplace `{}` not registered", marketplace))?;
    if !entry.plugins.iter().any(|p| p == plugin) {
        bail!("plugin `{}` not found in marketplace `{}`", plugin, marketplace);
    }

    // Resolve plugin source dir relative to marketplace root.
    let mp_root_rel = format!("marketplaces/{}", marketplace);
    let mp_root_abs = paths::plugins_root().unwrap().join(&mp_root_rel);
    let manifest = super::manifest::load_marketplace_manifest(&mp_root_abs)?;
    let plugin_entry: PluginEntry = match manifest {
        Some(m) => m
            .plugins
            .into_iter()
            .find(|p| sanitize_name(&p.name) == plugin || p.name == plugin)
            .ok_or_else(|| anyhow!("plugin `{}` missing from manifest", plugin))?,
        None => PluginEntry {
            name: plugin.to_string(),
            source: "./".into(),
            description: None,
        },
    };

    // Reject path traversal in PluginEntry.source.
    validate_plugin_source(&plugin_entry.source)?;

    let normalized_source = plugin_entry.source.trim_start_matches("./");
    let plugin_dir_rel = if normalized_source.is_empty() {
        mp_root_rel.clone()
    } else {
        format!("{}/{}", mp_root_rel, normalized_source.trim_end_matches('/'))
    };

    // Sanitize the plugin name component of the canonical id; the marketplace
    // is already a sanitized key (enforced in add_marketplace).
    let plugin_key = sanitize_name(plugin);
    if plugin_key.is_empty() {
        bail!("plugin name `{}` sanitized to empty string", plugin);
    }
    let id = plugin_id(&plugin_key, marketplace);
    let installed_path = paths::installed_plugins_file().unwrap();
    let mut installed = load_installed_plugins_file(&installed_path)?;
    if installed.plugins.contains_key(&id) {
        bail!("plugin `{}` already installed; uninstall first", id);
    }
    installed.plugins.insert(
        id.clone(),
        InstalledPluginEntry {
            marketplace: marketplace.to_string(),
            plugin: plugin_key.clone(),
            plugin_dir: plugin_dir_rel.clone(),
            installed_at: chrono::Utc::now().to_rfc3339(),
        },
    );
    save_installed_plugins_file(&installed_path, &installed)?;

    Ok(InstalledPluginInfo {
        plugin: plugin_key,
        marketplace: marketplace.to_string(),
        plugin_dir: plugin_dir_rel,
    })
}

pub fn uninstall(plugin: &str, marketplace: &str) -> Result<()> {
    let plugin_key = sanitize_name(plugin);
    let id = plugin_id(&plugin_key, marketplace);
    let installed_path = paths::installed_plugins_file().unwrap();
    let mut installed = load_installed_plugins_file(&installed_path)?;
    if installed.plugins.remove(&id).is_none() {
        bail!("plugin `{}` not installed", id);
    }
    save_installed_plugins_file(&installed_path, &installed)?;
    Ok(())
}

pub fn list_installed() -> Result<Vec<InstalledPluginInfo>> {
    let installed = load_installed_plugins_file(&paths::installed_plugins_file().unwrap())?;
    Ok(installed
        .plugins
        .into_values()
        .map(|e| InstalledPluginInfo {
            plugin: e.plugin,
            marketplace: e.marketplace,
            plugin_dir: e.plugin_dir,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::marketplace::add_marketplace;
    use crate::plugin::test_support::isolated_home;
    use std::path::PathBuf;
    use std::process::Command;

    fn make_repo(name: &str, manifest: Option<&str>) -> PathBuf {
        let work = tempfile::tempdir().unwrap().into_path();
        let repo = work.join(name);
        std::fs::create_dir_all(&repo).unwrap();
        Command::new("git").args(["init", "-q"]).current_dir(&repo).status().unwrap();
        Command::new("git").args(["config", "user.email", "t@t"]).current_dir(&repo).status().unwrap();
        Command::new("git").args(["config", "user.name", "t"]).current_dir(&repo).status().unwrap();
        if let Some(m) = manifest {
            std::fs::create_dir_all(repo.join(".atomcode-plugin")).unwrap();
            std::fs::write(repo.join(".atomcode-plugin/marketplace.json"), m).unwrap();
        }
        std::fs::write(repo.join("README"), "x").unwrap();
        Command::new("git").args(["add", "-A"]).current_dir(&repo).status().unwrap();
        Command::new("git").args(["commit", "-q", "-m", "init"]).current_dir(&repo).status().unwrap();
        repo
    }

    #[test]
    #[serial_test::serial]
    fn install_single_plugin_fallback() {
        let _home = isolated_home();
        let repo = make_repo("solo", None);
        add_marketplace(&format!("file://{}", repo.display())).unwrap();
        let info = install("solo", "solo").unwrap();
        assert_eq!(info.plugin_dir, "marketplaces/solo");
    }

    #[test]
    #[serial_test::serial]
    fn install_rejects_duplicate() {
        let _home = isolated_home();
        let repo = make_repo("dup", None);
        add_marketplace(&format!("file://{}", repo.display())).unwrap();
        install("dup", "dup").unwrap();
        assert!(install("dup", "dup").is_err());
    }

    #[test]
    #[serial_test::serial]
    fn uninstall_works() {
        let _home = isolated_home();
        let repo = make_repo("u", None);
        add_marketplace(&format!("file://{}", repo.display())).unwrap();
        install("u", "u").unwrap();
        uninstall("u", "u").unwrap();
        assert!(list_installed().unwrap().is_empty());
    }

    #[test]
    #[serial_test::serial]
    fn install_with_subdir_source() {
        let _home = isolated_home();
        let manifest = r#"{"name":"mp","plugins":[{"name":"sub","source":"plugins/sub"}]}"#;
        let repo = make_repo("mp", Some(manifest));
        // Pre-populate the subdirectory so the commit includes it.
        std::fs::create_dir_all(repo.join("plugins/sub")).unwrap();
        std::fs::write(repo.join("plugins/sub/plugin.json"), "{}").unwrap();
        Command::new("git").args(["add", "-A"]).current_dir(&repo).status().unwrap();
        Command::new("git").args(["commit", "-q", "-m", "add sub"]).current_dir(&repo).status().unwrap();
        add_marketplace(&format!("file://{}", repo.display())).unwrap();
        let info = install("sub", "mp").unwrap();
        assert_eq!(info.plugin_dir, "marketplaces/mp/plugins/sub");
    }

    /// B2 regression: a plugin whose `source` contains `..` must be
    /// rejected, otherwise the resulting `plugin_dir` could escape the
    /// marketplace root.
    #[test]
    #[serial_test::serial]
    fn install_rejects_traversal_in_plugin_source() {
        let _home = isolated_home();
        let manifest = r#"{"name":"mp2","plugins":[{"name":"esc","source":"../../etc"}]}"#;
        let repo = make_repo("mp2", Some(manifest));
        add_marketplace(&format!("file://{}", repo.display())).unwrap();
        let err = install("esc", "mp2").unwrap_err();
        assert!(
            err.to_string().contains("disallowed components"),
            "expected traversal rejection, got: {}",
            err
        );
    }

    #[test]
    fn validate_plugin_source_unit() {
        assert!(validate_plugin_source("").is_ok());
        assert!(validate_plugin_source("./").is_ok());
        assert!(validate_plugin_source("plugins/foo").is_ok());
        assert!(validate_plugin_source("./plugins/foo").is_ok());
        assert!(validate_plugin_source("../etc").is_err());
        assert!(validate_plugin_source("plugins/../etc").is_err());
        assert!(validate_plugin_source("/etc/passwd").is_err());
        assert!(validate_plugin_source("plugins/foo/../bar").is_err());
    }
}
