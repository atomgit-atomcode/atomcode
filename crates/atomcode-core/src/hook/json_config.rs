//! Hooks JSON configuration loading — mirrors the MCP config pattern.
//!
//! Hooks are configured in JSON files:
//! - `~/.atomcode/hooks.json`       — global hooks
//! - `<project>/.hooks.json`        — project-level hooks (override global by name)
//!
//! Project hooks override global hooks with the same name. Hooks with
//! `"disabled": true` are skipped.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use super::{HookConfig, HookEvent};

/// Top-level JSON structure for a hooks config file.
#[derive(Debug, Deserialize)]
struct HooksFile {
    #[serde(default)]
    hooks: BTreeMap<String, HookEntry>,
}

/// A single hook entry in the JSON config.
#[derive(Debug, Deserialize)]
struct HookEntry {
    pub event: String,
    #[serde(default)]
    pub matcher: Option<String>,
    pub command: String,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub disabled: bool,
}

fn default_timeout() -> u64 {
    10_000
}

/// Load and merge hooks from global (`~/.atomcode/hooks.json`) and project
/// (`.hooks.json`) config files.
///
/// Project hooks override global hooks with the same name. Disabled hooks
/// are filtered out.
pub fn load_hooks_config(project_dir: &Path) -> Vec<HookConfig> {
    let global_path = dirs::home_dir()
        .map(|h| h.join(".atomcode/hooks.json"))
        .unwrap_or_default();
    let project_path = project_dir.join(".hooks.json");

    let mut merged: BTreeMap<String, HookConfig> = BTreeMap::new();

    // Load global hooks first.
    if let Ok(hooks) = load_hooks_file(&global_path) {
        for (name, hook) in hooks {
            merged.insert(name, hook);
        }
    }

    // Load project hooks — override global by name.
    if let Ok(hooks) = load_hooks_file(&project_path) {
        for (name, hook) in hooks {
            merged.insert(name, hook);
        }
    }

    merged.into_values().collect()
}

/// Parse a single hooks JSON file and return named hook configs.
///
/// Disabled hooks are filtered out. Missing files return an empty vec
/// (not an error).
fn load_hooks_file(path: &Path) -> Result<Vec<(String, HookConfig)>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read hooks config from {}", path.display()))?;
    let raw: HooksFile = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse hooks config from {}", path.display()))?;

    let mut configs = Vec::new();
    for (name, entry) in raw.hooks {
        if entry.disabled {
            continue;
        }
        let event: HookEvent =
            serde_json::from_value(serde_json::Value::String(entry.event.clone()))
                .unwrap_or(HookEvent::PreToolUse);
        configs.push((
            name,
            HookConfig {
                event,
                matcher: entry.matcher,
                command: entry.command,
                timeout_ms: entry.timeout_ms,
            },
        ));
    }
    Ok(configs)
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Parse a minimal hooks JSON with one entry.
    #[test]
    fn parse_single_hook() {
        let json = r#"{
            "hooks": {
                "audit-all": {
                    "event": "pre_tool_use",
                    "command": "echo audit"
                }
            }
        }"#;
        let raw: HooksFile = serde_json::from_str(json).unwrap();
        assert_eq!(raw.hooks.len(), 1);
        let entry = &raw.hooks["audit-all"];
        assert_eq!(entry.event, "pre_tool_use");
        assert_eq!(entry.command, "echo audit");
        assert_eq!(entry.timeout_ms, 10_000);
        assert!(!entry.disabled);
    }

    /// Parse multiple hooks with matcher and timeout.
    #[test]
    fn parse_multiple_hooks() {
        let json = r#"{
            "hooks": {
                "audit": {
                    "event": "pre_tool_use",
                    "command": "echo audit"
                },
                "block-rm": {
                    "event": "pre_tool_use",
                    "matcher": "bash",
                    "command": "safety-check.sh",
                    "timeout_ms": 5000
                },
                "auto-format": {
                    "event": "post_tool_use",
                    "matcher": "edit_*",
                    "command": "cargo fmt"
                }
            }
        }"#;
        let raw: HooksFile = serde_json::from_str(json).unwrap();
        assert_eq!(raw.hooks.len(), 3);
        assert_eq!(raw.hooks["block-rm"].timeout_ms, 5000);
        assert_eq!(
            raw.hooks["block-rm"].matcher.as_deref(),
            Some("bash")
        );
        assert_eq!(raw.hooks["auto-format"].event, "post_tool_use");
    }

    /// Disabled hooks are filtered out when loading.
    #[test]
    fn disabled_hooks_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        let json = r#"{
            "hooks": {
                "active": {
                    "event": "pre_tool_use",
                    "command": "echo yes"
                },
                "inactive": {
                    "event": "pre_tool_use",
                    "command": "echo no",
                    "disabled": true
                }
            }
        }"#;
        std::fs::write(&path, json).unwrap();
        let hooks = load_hooks_file(&path).unwrap();
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].0, "active");
    }

    /// Missing file returns empty vec, not error.
    #[test]
    fn missing_file_returns_empty() {
        let path = std::path::Path::new("/nonexistent/hooks.json");
        let hooks = load_hooks_file(path).unwrap();
        assert!(hooks.is_empty());
    }

    /// Empty hooks object parses fine.
    #[test]
    fn empty_hooks_object() {
        let json = r#"{ "hooks": {} }"#;
        let raw: HooksFile = serde_json::from_str(json).unwrap();
        assert!(raw.hooks.is_empty());
    }

    /// Project hooks override global hooks with the same name.
    #[test]
    fn project_overrides_global_by_name() {
        let dir = tempfile::tempdir().unwrap();

        // Simulate global config dir
        let global_dir = dir.path().join("global");
        std::fs::create_dir_all(&global_dir).unwrap();
        let global_path = global_dir.join("hooks.json");
        std::fs::write(
            &global_path,
            r#"{
                "hooks": {
                    "audit": {
                        "event": "pre_tool_use",
                        "command": "echo global-audit"
                    },
                    "global-only": {
                        "event": "session_start",
                        "command": "echo global-only"
                    }
                }
            }"#,
        )
        .unwrap();

        // Project hooks
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let project_path = project_dir.join(".hooks.json");
        std::fs::write(
            &project_path,
            r#"{
                "hooks": {
                    "audit": {
                        "event": "pre_tool_use",
                        "command": "echo project-audit"
                    },
                    "project-only": {
                        "event": "post_tool_use",
                        "command": "echo project-only"
                    }
                }
            }"#,
        )
        .unwrap();

        // Load and merge manually (since load_hooks_config uses hardcoded paths)
        let mut merged: BTreeMap<String, HookConfig> = BTreeMap::new();
        for (name, hook) in load_hooks_file(&global_path).unwrap() {
            merged.insert(name, hook);
        }
        for (name, hook) in load_hooks_file(&project_path).unwrap() {
            merged.insert(name, hook);
        }

        assert_eq!(merged.len(), 3);

        // "audit" should be the project version
        let audit = &merged["audit"];
        assert_eq!(audit.command, "echo project-audit");

        // "global-only" should survive
        assert!(merged.contains_key("global-only"));

        // "project-only" should be present
        assert!(merged.contains_key("project-only"));
    }

    /// Event strings map correctly to HookEvent variants.
    #[test]
    fn event_string_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        let json = r#"{
            "hooks": {
                "h1": { "event": "pre_tool_use", "command": "a" },
                "h2": { "event": "post_tool_use", "command": "b" },
                "h3": { "event": "session_start", "command": "c" },
                "h4": { "event": "session_end", "command": "d" }
            }
        }"#;
        std::fs::write(&path, json).unwrap();
        let hooks = load_hooks_file(&path).unwrap();
        let map: BTreeMap<String, HookConfig> = hooks.into_iter().collect();
        assert_eq!(map["h1"].event, HookEvent::PreToolUse);
        assert_eq!(map["h2"].event, HookEvent::PostToolUse);
        assert_eq!(map["h3"].event, HookEvent::SessionStart);
        assert_eq!(map["h4"].event, HookEvent::SessionEnd);
    }

    /// Malformed JSON returns an error, not a panic.
    #[test]
    fn malformed_json_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        std::fs::write(&path, "not valid json").unwrap();
        let result = load_hooks_file(&path);
        assert!(result.is_err());
    }

    /// Default timeout is 10000 when not specified.
    #[test]
    fn default_timeout_is_10000() {
        let json = r#"{
            "hooks": {
                "test": {
                    "event": "pre_tool_use",
                    "command": "echo test"
                }
            }
        }"#;
        let raw: HooksFile = serde_json::from_str(json).unwrap();
        assert_eq!(raw.hooks["test"].timeout_ms, 10_000);
    }

    /// Custom timeout_ms is preserved.
    #[test]
    fn custom_timeout_is_preserved() {
        let json = r#"{
            "hooks": {
                "fast": {
                    "event": "pre_tool_use",
                    "command": "echo fast",
                    "timeout_ms": 500
                }
            }
        }"#;
        let raw: HooksFile = serde_json::from_str(json).unwrap();
        assert_eq!(raw.hooks["fast"].timeout_ms, 500);
    }
}
