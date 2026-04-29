use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Top-level marketplace manifest (`.atomcode-plugin/marketplace.json` or
/// `.claude-plugin/marketplace.json`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MarketplaceManifest {
    pub name: String,
    #[serde(default)]
    pub plugins: Vec<PluginEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PluginEntry {
    pub name: String,
    /// Path relative to marketplace root, e.g. "./" or "plugins/foo".
    #[serde(default = "default_source")]
    pub source: String,
    #[serde(default)]
    pub description: Option<String>,
}

fn default_source() -> String {
    "./".to_string()
}

/// Per-plugin manifest (`<plugin-dir>/plugin.json`). All fields optional.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct PluginManifest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Path to skills dir, default "skills".
    #[serde(default)]
    pub skills: Option<String>,
    /// Path to commands dir, default "commands".
    #[serde(default)]
    pub commands: Option<String>,
    /// Path to hooks json, default "hooks.json".
    #[serde(default)]
    pub hooks: Option<String>,
}

impl PluginManifest {
    pub fn skills_path(&self) -> &str {
        self.skills.as_deref().unwrap_or("skills")
    }
    pub fn commands_path(&self) -> &str {
        self.commands.as_deref().unwrap_or("commands")
    }
    pub fn hooks_path(&self) -> &str {
        self.hooks.as_deref().unwrap_or("hooks.json")
    }
}

/// Try to load a marketplace manifest from a marketplace clone root.
/// Order: `.atomcode-plugin/marketplace.json` → `.claude-plugin/marketplace.json`.
/// Returns `Ok(None)` when neither file exists (single-plugin fallback caller).
/// Returns `Err` when a file exists but cannot be parsed (fail closed).
pub fn load_marketplace_manifest(marketplace_root: &Path) -> Result<Option<MarketplaceManifest>> {
    for rel in [".atomcode-plugin/marketplace.json", ".claude-plugin/marketplace.json"] {
        let path = marketplace_root.join(rel);
        if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            let manifest: MarketplaceManifest = serde_json::from_str(&raw)
                .with_context(|| format!("parse {}", path.display()))?;
            return Ok(Some(manifest));
        }
    }
    Ok(None)
}

/// Load `<plugin-dir>/plugin.json` if present. Returns default when missing.
/// Returns `Err` when present but unparseable (fail closed).
pub fn load_plugin_manifest(plugin_dir: &Path) -> Result<PluginManifest> {
    let path = plugin_dir.join("plugin.json");
    if !path.exists() {
        return Ok(PluginManifest::default());
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("read {}", path.display()))?;
    let manifest: PluginManifest = serde_json::from_str(&raw)
        .with_context(|| format!("parse {}", path.display()))?;
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_atomcode_manifest_with_priority_over_claude() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".atomcode-plugin")).unwrap();
        std::fs::create_dir_all(tmp.path().join(".claude-plugin")).unwrap();
        std::fs::write(
            tmp.path().join(".atomcode-plugin/marketplace.json"),
            r#"{"name":"atom","plugins":[{"name":"a"}]}"#,
        )
        .unwrap();
        std::fs::write(
            tmp.path().join(".claude-plugin/marketplace.json"),
            r#"{"name":"claude","plugins":[{"name":"c"}]}"#,
        )
        .unwrap();
        let m = load_marketplace_manifest(tmp.path()).unwrap().unwrap();
        assert_eq!(m.name, "atom");
        assert_eq!(m.plugins[0].name, "a");
    }

    #[test]
    fn missing_manifest_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load_marketplace_manifest(tmp.path()).unwrap().is_none());
    }

    #[test]
    fn malformed_manifest_returns_err() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".atomcode-plugin")).unwrap();
        std::fs::write(
            tmp.path().join(".atomcode-plugin/marketplace.json"),
            "{ not json",
        )
        .unwrap();
        assert!(load_marketplace_manifest(tmp.path()).is_err());
    }

    #[test]
    fn plugin_manifest_defaults() {
        let m = PluginManifest::default();
        assert_eq!(m.skills_path(), "skills");
        assert_eq!(m.commands_path(), "commands");
        assert_eq!(m.hooks_path(), "hooks.json");
    }

    #[test]
    fn plugin_manifest_loads_overrides() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("plugin.json"),
            r#"{"name":"p","skills":"my_skills"}"#,
        )
        .unwrap();
        let m = load_plugin_manifest(tmp.path()).unwrap();
        assert_eq!(m.skills_path(), "my_skills");
        assert_eq!(m.commands_path(), "commands");
    }

    #[test]
    fn plugin_manifest_missing_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        let m = load_plugin_manifest(tmp.path()).unwrap();
        assert_eq!(m.skills_path(), "skills");
    }
}
