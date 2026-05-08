//! Bridge between AtomCode's skill system and Claude Code's installed
//! plugins.  Reads `~/.claude/plugins/installed_plugins.json` to discover
//! plugins that Claude Code has downloaded into its plugin cache, and
//! exposes their `skills/` and `commands/` directories for loading.
//!
//! This is a **read-only** shim — it never writes to or modifies Claude
//! Code's state.  Plugins are loaded in their *original* namespace
//! (e.g. `superpowers:brainstorming`) so that `/skills` lists them
//! alongside AtomCode-native skills.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;

// ── JSON schema of `installed_plugins.json` ────────────────────────────

/// Top-level structure of Claude Code's `installed_plugins.json`.
#[derive(Debug, Deserialize)]
struct ClaudeInstalledPluginsFile {
    #[allow(dead_code)]
    version: u32,
    /// Map from `"<plugin>@<marketplace>"` → ordered list of install
    /// records (one per scope: user, project, …).
    plugins: BTreeMap<String, Vec<ClaudeInstalledPlugin>>,
}

/// One install record inside the `plugins` map.
#[derive(Debug, Deserialize)]
struct ClaudeInstalledPlugin {
    #[serde(rename = "installPath")]
    install_path: String,

    #[allow(dead_code)]
    scope: String,

    #[allow(dead_code)]
    version: String,
}

// ── Public API ─────────────────────────────────────────────────────────

/// Asset directories discovered from a single Claude Code plugin install.
#[derive(Debug)]
pub struct ClaudePluginAssets {
    /// Plugin name (e.g. `"superpowers"`, `"frontend-design"`).
    pub plugin: String,

    /// Marketplace source (e.g. `"claude-plugins-official"`).
    #[allow(dead_code)]
    pub marketplace: String,

    /// Absolute path to the versioned plugin root.
    pub plugin_dir: PathBuf,
}

impl ClaudePluginAssets {
    /// Path to the `skills/` sub-directory (may not exist).
    pub fn skills_dir(&self) -> PathBuf {
        self.plugin_dir.join("skills")
    }

    /// Path to the `commands/` sub-directory (may not exist).
    pub fn commands_dir(&self) -> PathBuf {
        self.plugin_dir.join("commands")
    }
}

/// Scan all plugins that Claude Code has installed and return those that
/// have at least one asset directory (`skills/` or `commands/`) on disk.
pub fn iter_claude_code_plugins() -> Vec<ClaudePluginAssets> {
    let home = match crate::tool::real_home_dir() {
        Some(h) => h,
        None => return vec![],
    };

    let state_path = home
        .join(".claude")
        .join("plugins")
        .join("installed_plugins.json");

    if !state_path.exists() {
        return vec![];
    }

    let raw = match std::fs::read_to_string(&state_path) {
        Ok(s) => s,
        Err(_) => return vec![],
    };

    let file: ClaudeInstalledPluginsFile = match serde_json::from_str(&raw) {
        Ok(f) => f,
        Err(_) => return vec![],
    };

    let mut result = Vec::new();

    for (plugin_id, entries) in &file.plugins {
        // plugin_id format: "<plugin>@<marketplace>"
        let entry = match entries.first() {
            Some(e) => e,
            None => continue,
        };

        let install_path = PathBuf::from(&entry.install_path);
        if !install_path.is_dir() {
            continue;
        }

        let (plugin, marketplace) = match plugin_id.split_once('@') {
            Some((p, m)) => (p.to_string(), m.to_string()),
            None => continue,
        };

        // Only include plugins that actually contribute skills or commands.
        if install_path.join("skills").is_dir() || install_path.join("commands").is_dir() {
            result.push(ClaudePluginAssets {
                plugin,
                marketplace,
                plugin_dir: install_path,
            });
        }
    }

    result
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_claude_state_file() {
        let json = r#"{
          "version": 2,
          "plugins": {
            "superpowers@claude-plugins-official": [
              {
                "scope": "user",
                "installPath": "/home/user/.claude/plugins/cache/claude-plugins-official/superpowers/5.0.7",
                "version": "5.0.7",
                "installedAt": "2025-01-01T00:00:00.000Z",
                "lastUpdated": "2025-01-01T00:00:00.000Z"
              }
            ]
          }
        }"#;

        let file: ClaudeInstalledPluginsFile =
            serde_json::from_str(json).expect("should parse valid JSON");

        assert_eq!(file.plugins.len(), 1);

        let entry = &file.plugins["superpowers@claude-plugins-official"][0];
        assert_eq!(
            entry.install_path,
            "/home/user/.claude/plugins/cache/claude-plugins-official/superpowers/5.0.7"
        );
        assert_eq!(entry.scope, "user");
        assert_eq!(entry.version, "5.0.7");
    }

    #[test]
    fn parse_handles_unknown_fields() {
        // Extra fields that Claude Code might add in the future must not
        // cause a parse failure (serde ignores unknown fields by default).
        let json = r#"{
          "version": 2,
          "plugins": {
            "test@example": [
              {
                "scope": "user",
                "installPath": "/tmp/test",
                "version": "1.0.0",
                "extraField": "ignored"
              }
            ]
          }
        }"#;

        let file: ClaudeInstalledPluginsFile =
            serde_json::from_str(json).expect("should tolerate unknown extra fields");

        let entry = &file.plugins["test@example"][0];
        assert_eq!(entry.install_path, "/tmp/test");
    }

    #[test]
    fn iter_skips_missing_state_file() {
        // No file → empty result, no panic.
        let results = iter_claude_code_plugins();
        // Can't assert on length here because `real_home_dir()` may point
        // at the actual user's home which *does* have the file.  Just
        // verify it doesn't crash.
        let _ = results;
    }

    #[test]
    fn iter_skips_missing_install_path() {
        // Install path in JSON but directory doesn't exist → skip.
        let dir = tempfile::tempdir().expect("tempdir");
        let state_path = dir.path().join("installed_plugins.json");
        let json = format!(
            r#"{{
            "version": 2,
            "plugins": {{
                "missing-dir@test": [
                    {{
                        "scope": "user",
                        "installPath": "{}/nonexistent",
                        "version": "1.0.0"
                    }}
                ]
            }}
        }}"#,
            dir.path().display().to_string().replace('\\', "/")
        );
        std::fs::write(&state_path, &json).expect("write state file");

        // The function uses real_home_dir() which we can't mock, so we
        // test the filtering logic inline instead.
        let raw = std::fs::read_to_string(&state_path).unwrap();
        let file: ClaudeInstalledPluginsFile = serde_json::from_str(&raw).unwrap();
        let results: Vec<_> = file
            .plugins
            .iter()
            .filter_map(|(id, entries)| {
                let entry = entries.first()?;
                let install_path = PathBuf::from(&entry.install_path);
                if !install_path.is_dir() {
                    return None;
                }
                if !install_path.join("skills").is_dir() && !install_path.join("commands").is_dir()
                {
                    return None;
                }
                let (plugin, marketplace) = id.split_once('@')?;
                Some(ClaudePluginAssets {
                    plugin: plugin.to_string(),
                    marketplace: marketplace.to_string(),
                    plugin_dir: install_path,
                })
            })
            .collect();
        assert!(results.is_empty(), "nonexistent dir should be skipped");
    }
}
