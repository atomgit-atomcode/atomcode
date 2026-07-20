use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// Installation scope for a plugin — determines where the plugin files
/// are stored and who can see them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum InstallScope {
    /// User-global: installed to `~/.atomcode/plugins/`, visible in all projects.
    User,
    /// Project-shared: installed to `.atomcode/plugins/`, committed to git,
    /// visible to all collaborators.
    Project,
    /// Local-only: installed to `.atomcode/plugins/local/`, git-ignored,
    /// visible only to the current user in the current project.
    Local,
}

impl Default for InstallScope {
    fn default() -> Self {
        Self::User
    }
}

impl std::fmt::Display for InstallScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstallScope::User => write!(f, "user"),
            InstallScope::Project => write!(f, "project"),
            InstallScope::Local => write!(f, "local"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplacesFile {
    pub version: u32,
    #[serde(default)]
    pub marketplaces: BTreeMap<String, MarketplaceEntry>,
}

impl Default for MarketplacesFile {
    fn default() -> Self {
        Self { version: 1, marketplaces: BTreeMap::new() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceEntry {
    pub source: String,
    pub added_at: String,
    pub git_commit: String,
    #[serde(default)]
    pub plugins: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledPluginsFile {
    pub version: u32,
    #[serde(default)]
    pub plugins: BTreeMap<String, InstalledPluginEntry>,
}

impl Default for InstalledPluginsFile {
    fn default() -> Self {
        Self { version: 1, plugins: BTreeMap::new() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledPluginEntry {
    pub marketplace: String,
    pub plugin: String,
    /// Relative to the plugins root (`$ATOMCODE_HOME/plugins/`) (e.g. `marketplaces/foo/plugin-a`).
    pub plugin_dir: String,
    pub installed_at: String,
    /// Installation scope — determines where the plugin lives and who can see it.
    #[serde(default)]
    pub scope: InstallScope,
}

pub fn load_marketplaces_file(path: &Path) -> Result<MarketplacesFile> {
    if !path.exists() {
        return Ok(MarketplacesFile::default());
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;
    let parsed: MarketplacesFile = serde_json::from_str(&raw)
        .with_context(|| format!("parse {}", path.display()))?;
    Ok(parsed)
}

pub fn save_marketplaces_file(path: &Path, file: &MarketplacesFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let raw = serde_json::to_string_pretty(file)?;
    std::fs::write(path, raw)?;
    Ok(())
}

pub fn load_installed_plugins_file(path: &Path) -> Result<InstalledPluginsFile> {
    if !path.exists() {
        return Ok(InstalledPluginsFile::default());
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;
    let parsed: InstalledPluginsFile = serde_json::from_str(&raw)
        .with_context(|| format!("parse {}", path.display()))?;
    Ok(parsed)
}

pub fn save_installed_plugins_file(path: &Path, file: &InstalledPluginsFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let raw = serde_json::to_string_pretty(file)?;
    std::fs::write(path, raw)?;
    Ok(())
}

/// Build the canonical `<plugin>@<marketplace>` key.
pub fn plugin_id(plugin: &str, marketplace: &str) -> String {
    format!("{}@{}", plugin, marketplace)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_marketplaces_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mp.json");
        let mut f = MarketplacesFile::default();
        f.marketplaces.insert(
            "x".into(),
            MarketplaceEntry {
                source: "https://e/r.git".into(),
                added_at: "now".into(),
                git_commit: "abc".into(),
                plugins: vec!["p".into()],
            },
        );
        save_marketplaces_file(&path, &f).unwrap();
        let loaded = load_marketplaces_file(&path).unwrap();
        assert_eq!(loaded.marketplaces.len(), 1);
        assert_eq!(loaded.marketplaces["x"].git_commit, "abc");
    }

    #[test]
    fn missing_file_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        let f = load_marketplaces_file(&tmp.path().join("none")).unwrap();
        assert!(f.marketplaces.is_empty());
        let f = load_installed_plugins_file(&tmp.path().join("none")).unwrap();
        assert!(f.plugins.is_empty());
    }

    #[test]
    fn plugin_id_format() {
        assert_eq!(plugin_id("a", "b"), "a@b");
    }
}
