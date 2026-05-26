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
use std::path::{Path, PathBuf};

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
    /// Reserved for future use: displaying the source marketplace in the
    /// `/skills` UI or diagnostics.
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

/// Scan all plugins that Claude Code has installed under `home` and return
/// those that have at least one asset directory (`skills/` or `commands/`)
/// on disk.
///
/// This is the testable core that accepts an explicit home directory.
/// The public [`get_claude_code_plugins`] wraps it with the real home dir.
fn get_claude_code_plugins_from(home: &Path) -> Vec<ClaudePluginAssets> {
    let state_path = home
        .join(".claude")
        .join("plugins")
        .join("installed_plugins.json");

    if !state_path.exists() {
        return vec![];
    }

    let raw = match std::fs::read_to_string(&state_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!("failed to read Claude Code plugins state file {}: {}", state_path.display(), e);
            return vec![];
        }
    };

    let file: ClaudeInstalledPluginsFile = match serde_json::from_str(&raw) {
        Ok(f) => f,
        Err(e) => {
            tracing::debug!("failed to parse Claude Code plugins state file {}: {}", state_path.display(), e);
            return vec![];
        }
    };

    let mut result = Vec::new();

    for (plugin_id, entries) in &file.plugins {
        // plugin_id format: "<plugin>@<marketplace>"
        let entry = match pick_best_entry(entries) {
            Some(e) => e,
            None => continue,
        };

        // Resolve install_path: could be absolute or relative (relative to
        // ~/.claude/plugins/cache/).
        let install_path = PathBuf::from(&entry.install_path);
        let install_path = if install_path.is_absolute() {
            install_path
        } else {
            home.join(".claude/plugins/cache").join(&install_path)
        };

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

/// Scan all plugins that Claude Code has installed and return those that
/// have at least one asset directory (`skills/` or `commands/`) on disk.
///
/// Delegates to [`get_claude_code_plugins_from`] with the real home dir.
pub fn get_claude_code_plugins() -> Vec<ClaudePluginAssets> {
    let home = match crate::tool::real_home_dir() {
        Some(h) => h,
        None => return vec![],
    };

    get_claude_code_plugins_from(&home)
}

/// Choose the best install entry from a list of scoped records.
///
/// Priority: `user` > `project` > last entry (fallback).
/// This mirrors Claude Code's own behaviour where `user`-scoped installs
/// take precedence over `project`-scoped ones.
fn pick_best_entry(entries: &[ClaudeInstalledPlugin]) -> Option<&ClaudeInstalledPlugin> {
    if entries.is_empty() {
        return None;
    }
    // Prefer user scope, then project scope, then fall back to the last entry.
    entries
        .iter()
        .rev()
        .find(|e| e.scope == "user")
        .or_else(|| entries.iter().rev().find(|e| e.scope == "project"))
        .or_else(|| entries.last())
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
    fn get_skips_missing_state_file() {
        // When the state file does not exist, the function returns an empty
        // vector without panicking.  Uses get_claude_code_plugins_from with
        // a fake home dir that has no `.claude` directory.
        let fake_home = tempfile::tempdir().expect("tempdir");
        let results = get_claude_code_plugins_from(fake_home.path());
        assert!(results.is_empty(), "should be empty when no Claude state file exists");
    }

    #[test]
    fn get_skips_missing_install_path() {
        // Install path in JSON but directory doesn't exist → skip.
        let dir = tempfile::tempdir().expect("tempdir");
        let claude_dir = dir.path().join(".claude/plugins");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let state_path = claude_dir.join("installed_plugins.json");
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

        // Use get_claude_code_plugins_from to test the actual function.
        let results = get_claude_code_plugins_from(dir.path());
        assert!(results.is_empty(), "nonexistent dir should be skipped");
    }

    #[test]
    fn get_skips_plugin_without_skills_or_commands() {
        // Plugin dir exists but has neither skills/ nor commands/ → skip.
        let dir = tempfile::tempdir().expect("tempdir");
        let plugin_dir = dir.path().join(".claude/plugins/cache/mkt/test/1.0.0");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        let claude_dir = dir.path().join(".claude/plugins");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let state_path = claude_dir.join("installed_plugins.json");
        let json = format!(
            r#"{{
            "version": 2,
            "plugins": {{
                "test@mkt": [
                    {{
                        "scope": "user",
                        "installPath": "{}",
                        "version": "1.0.0"
                    }}
                ]
            }}
        }}"#,
            plugin_dir.display().to_string().replace('\\', "/")
        );
        std::fs::write(&state_path, &json).unwrap();

        let results = get_claude_code_plugins_from(dir.path());
        assert!(results.is_empty(), "plugin without skills/ or commands/ should be skipped");
    }

    #[test]
    fn picks_user_scope_over_project_scope() {
        let entries_str = r#"[
            {"scope": "project", "installPath": "/project/path", "version": "1.0.0"},
            {"scope": "user", "installPath": "/user/path", "version": "1.0.0"}
        ]"#;
        let entries: Vec<ClaudeInstalledPlugin> = serde_json::from_str(entries_str).unwrap();
        let picked = pick_best_entry(&entries).unwrap();
        assert_eq!(picked.scope, "user");
        assert_eq!(picked.install_path, "/user/path");
    }

    #[test]
    fn picks_project_scope_when_no_user_scope() {
        let entries_str = r#"[
            {"scope": "project", "installPath": "/project/path", "version": "1.0.0"}
        ]"#;
        let entries: Vec<ClaudeInstalledPlugin> = serde_json::from_str(entries_str).unwrap();
        let picked = pick_best_entry(&entries).unwrap();
        assert_eq!(picked.scope, "project");
    }

    #[test]
    fn falls_back_to_last_entry_for_unknown_scope() {
        let entries_str = r#"[
            {"scope": "unknown", "installPath": "/unknown/path", "version": "1.0.0"}
        ]"#;
        let entries: Vec<ClaudeInstalledPlugin> = serde_json::from_str(entries_str).unwrap();
        let picked = pick_best_entry(&entries).unwrap();
        assert_eq!(picked.install_path, "/unknown/path");
    }

    #[test]
    fn resolves_relative_install_path() {
        // When installPath is relative, it should be resolved against
        // ~/.claude/plugins/cache/.
        let dir = tempfile::tempdir().expect("tempdir");
        let plugin_dir = dir.path().join(".claude/plugins/cache/mkt/test-plugin/1.0.0");
        std::fs::create_dir_all(plugin_dir.join("skills")).unwrap();
        let claude_dir = dir.path().join(".claude/plugins");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let state_path = claude_dir.join("installed_plugins.json");
        let json = r#"{
            "version": 2,
            "plugins": {
                "test-plugin@mkt": [
                    {
                        "scope": "user",
                        "installPath": "mkt/test-plugin/1.0.0",
                        "version": "1.0.0"
                    }
                ]
            }
        }"#;
        std::fs::write(&state_path, json).unwrap();

        let results = get_claude_code_plugins_from(dir.path());
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].plugin, "test-plugin");
        assert!(results[0].plugin_dir.is_absolute());
        assert!(results[0].skills_dir().is_dir());
    }

    #[test]
    fn loads_commands_dir() {
        // Verify that a plugin with a commands/ directory (but no skills/)
        // is discovered and its commands_dir() points to the right place.
        let dir = tempfile::tempdir().expect("tempdir");
        let plugin_dir = dir.path().join(".claude/plugins/cache/mkt/cmd-plugin/1.0.0");
        std::fs::create_dir_all(plugin_dir.join("commands")).unwrap();
        let claude_dir = dir.path().join(".claude/plugins");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let state_path = claude_dir.join("installed_plugins.json");
        let json = format!(
            r#"{{
            "version": 2,
            "plugins": {{
                "cmd-plugin@mkt": [
                    {{
                        "scope": "user",
                        "installPath": "{}",
                        "version": "1.0.0"
                    }}
                ]
            }}
        }}"#,
            plugin_dir.display().to_string().replace('\\', "/")
        );
        std::fs::write(&state_path, &json).unwrap();

        let results = get_claude_code_plugins_from(dir.path());
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].plugin, "cmd-plugin");
        assert!(results[0].commands_dir().is_dir());
        assert!(!results[0].skills_dir().is_dir());
    }
}
