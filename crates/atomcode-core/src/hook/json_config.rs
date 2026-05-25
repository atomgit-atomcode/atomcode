//! Hooks JSON configuration loading — mirrors the MCP config pattern.
//!
//! Hooks are configured in JSON files:
//! - `$ATOMCODE_HOME/hooks.json` — global hooks
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

/// Load and merge hooks from global (`$ATOMCODE_HOME/hooks.json`) and project
/// (`.hooks.json`) config files.
///
/// Project hooks override global hooks with the same name. Disabled hooks
/// are filtered out.
pub fn load_hooks_config(project_dir: &Path) -> Vec<HookConfig> {
    let global_path = crate::config::Config::config_dir().join("hooks.json");
    let project_path = project_dir.join(".hooks.json");

    let mut merged: BTreeMap<String, HookConfig> = BTreeMap::new();

    // Load global hooks first.
    if let Ok(hooks) = load_hooks_file(&global_path) {
        for (name, hook) in hooks {
            merged.insert(name, hook);
        }
    }

    // Plugin layer — two flavors per plugin:
    //   1. CC-style inline hooks declared in plugin.json (priority).
    //   2. Legacy atomcode hooks.json file (fallback).
    // CC wins when both exist because plugin authors targeting CC are the
    // common case, and a plugin shipping only a legacy hooks.json would not
    // have a colliding plugin.json hooks block.
    for assets in crate::plugin::loader::iter_installed_plugin_assets() {
        if let Some(cc_map) = assets.manifest.inline_cc_hooks() {
            for (name, hook) in cc_hooks_to_atomcode(cc_map, &assets.plugin_dir) {
                let key = format!("{}:{}", assets.plugin, name);
                merged.insert(key, hook);
            }
            continue;
        }
        let path = assets.hooks_file();
        if let Ok(hooks) = load_hooks_file(&path) {
            for (name, hook) in hooks {
                let key = format!("{}:{}", assets.plugin, name);
                merged.insert(key, hook);
            }
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

/// Translate a Claude-Code-style nested hooks block (as found in
/// `plugin.json`) into our flat `HookConfig` list. Each command spec
/// becomes one named entry; the synthetic name lets multiple hooks under
/// the same event coexist when later merged into the global hook table.
///
/// The plugin install directory is exported by the executor as the
/// `CLAUDE_PLUGIN_ROOT` / `ATOMCODE_PLUGIN_ROOT` environment variables —
/// hook authors reference it via `"${CLAUDE_PLUGIN_ROOT}/script.py"` in
/// the command string. We deliberately do NOT substitute the path into
/// the command at conversion time: paths containing spaces, quotes, `$`
/// or `;` would break shell parsing or open injection. Env vars are
/// also what Claude Code uses, so existing CC plugin scripts work as-is.
pub(crate) fn cc_hooks_to_atomcode(
    cc: &crate::plugin::manifest::CCHooksMap,
    plugin_root: &Path,
) -> Vec<(String, HookConfig)> {
    use crate::plugin::manifest::CCHookGroup;

    let mut out = Vec::new();

    for (event_name, groups) in cc {
        let event = match cc_event_name_to_event(event_name) {
            Some(e) => e,
            None => continue, // unsupported event — skip silently for now.
        };
        for (gi, CCHookGroup { matcher, hooks }) in groups.iter().enumerate() {
            for (hi, spec) in hooks.iter().enumerate() {
                if spec.kind != "command" {
                    continue;
                }
                // CC encodes timeout in seconds; we store ms internally.
                let timeout_ms = spec
                    .timeout
                    .map(|s| s.saturating_mul(1000))
                    .unwrap_or(10_000);
                let name = format!("{}-{}-{}", event_name, gi, hi);
                out.push((
                    name,
                    HookConfig {
                        event: event.clone(),
                        matcher: matcher.clone(),
                        command: spec.command.clone(),
                        timeout_ms,
                        plugin_root: Some(plugin_root.to_path_buf()),
                    },
                ));
            }
        }
    }
    out
}

/// Map CC's PascalCase event names to our `HookEvent` enum. Returns `None`
/// for events we don't yet support (e.g. `Stop`, `PreCompact`,
/// `SubagentStop`); callers skip those entries.
fn cc_event_name_to_event(name: &str) -> Option<HookEvent> {
    Some(match name {
        "PreToolUse" => HookEvent::PreToolUse,
        "PostToolUse" => HookEvent::PostToolUse,
        "SessionStart" => HookEvent::SessionStart,
        "SessionEnd" => HookEvent::SessionEnd,
        "Notification" => HookEvent::Notification,
        "UserPromptSubmit" => HookEvent::UserPromptSubmit,
        _ => return None,
    })
}

fn parse_hook_event(name: &str) -> Option<HookEvent> {
    serde_json::from_value::<HookEvent>(serde_json::Value::String(name.to_string()))
        .ok()
        .or_else(|| cc_event_name_to_event(name))
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
        let Some(event) = parse_hook_event(&entry.event) else {
            continue;
        };
        configs.push((
            name,
            HookConfig {
                event,
                matcher: entry.matcher,
                command: entry.command,
                timeout_ms: entry.timeout_ms,
                // Legacy flat hooks.json doesn't know which plugin owns
                // the hook (the hooks.json layout pre-dates the plugin
                // system). Plugin-contributed hooks come through
                // `cc_hooks_to_atomcode` which sets this.
                plugin_root: None,
            },
        ));
    }
    Ok(configs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cc_hooks_to_atomcode_records_plugin_root_without_substitution() {
        // Regression: we used to substitute `${CLAUDE_PLUGIN_ROOT}` into the
        // command string, which broke under `sh -c` for paths containing
        // spaces / quotes. The contract now is: command is passed through
        // unchanged, plugin_root is set, executor exports the env var.
        use crate::plugin::manifest::{CCHookGroup, CCHookSpec};
        let mut cc: crate::plugin::manifest::CCHooksMap = std::collections::BTreeMap::new();
        cc.insert(
            "UserPromptSubmit".into(),
            vec![CCHookGroup {
                matcher: None,
                hooks: vec![CCHookSpec {
                    kind: "command".into(),
                    command: "python \"${CLAUDE_PLUGIN_ROOT}/h.py\"".into(),
                    timeout: Some(5),
                }],
            }],
        );
        let plugin_root = std::path::Path::new("/opt/x");
        let out = cc_hooks_to_atomcode(&cc, plugin_root);
        assert_eq!(out.len(), 1);
        let (_, h) = &out[0];
        assert_eq!(h.event, HookEvent::UserPromptSubmit);
        // Command unchanged — substitution is the executor's job.
        assert_eq!(h.command, "python \"${CLAUDE_PLUGIN_ROOT}/h.py\"");
        assert_eq!(h.plugin_root.as_deref(), Some(plugin_root));
        assert_eq!(h.timeout_ms, 5_000); // CC seconds → ms
    }

    #[test]
    fn cc_hooks_to_atomcode_skips_unsupported_events() {
        use crate::plugin::manifest::{CCHookGroup, CCHookSpec};
        let mut cc: crate::plugin::manifest::CCHooksMap = std::collections::BTreeMap::new();
        // Stop / SubagentStop / PreCompact are CC events we don't surface yet.
        cc.insert(
            "Stop".into(),
            vec![CCHookGroup {
                matcher: None,
                hooks: vec![CCHookSpec {
                    kind: "command".into(),
                    command: "echo".into(),
                    timeout: None,
                }],
            }],
        );
        assert!(cc_hooks_to_atomcode(&cc, std::path::Path::new("/")).is_empty());
    }

    #[test]
    fn cc_hooks_to_atomcode_default_timeout_when_omitted() {
        use crate::plugin::manifest::{CCHookGroup, CCHookSpec};
        let mut cc: crate::plugin::manifest::CCHooksMap = std::collections::BTreeMap::new();
        cc.insert(
            "PreToolUse".into(),
            vec![CCHookGroup {
                matcher: Some("bash".into()),
                hooks: vec![CCHookSpec {
                    kind: "command".into(),
                    command: "echo".into(),
                    timeout: None,
                }],
            }],
        );
        let out = cc_hooks_to_atomcode(&cc, std::path::Path::new("/"));
        assert_eq!(out[0].1.timeout_ms, 10_000);
        assert_eq!(out[0].1.matcher.as_deref(), Some("bash"));
    }

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

    #[test]
    fn pascal_case_event_names_are_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        let json = r#"{
            "hooks": {
                "h1": { "event": "PreToolUse", "command": "a" },
                "h2": { "event": "UserPromptSubmit", "command": "b" }
            }
        }"#;
        std::fs::write(&path, json).unwrap();
        let hooks = load_hooks_file(&path).unwrap();
        let map: BTreeMap<String, HookConfig> = hooks.into_iter().collect();
        assert_eq!(map["h1"].event, HookEvent::PreToolUse);
        assert_eq!(map["h2"].event, HookEvent::UserPromptSubmit);
    }

    #[test]
    fn invalid_event_name_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        let json = r#"{
            "hooks": {
                "typo": { "event": "pre_tool", "command": "should-not-run" },
                "valid": { "event": "post_tool_use", "command": "echo ok" }
            }
        }"#;
        std::fs::write(&path, json).unwrap();
        let hooks = load_hooks_file(&path).unwrap();
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].0, "valid");
        assert_eq!(hooks[0].1.event, HookEvent::PostToolUse);
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

    #[test]
    #[serial_test::serial]
    fn plugin_hooks_are_loaded_with_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", tmp.path());

        let plugin_dir = tmp.path().join("plugins/marketplaces/p");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("hooks.json"),
            r#"{"hooks":{"on_pre":{"event":"PreToolUse","command":"echo hi"}}}"#,
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("plugins/installed_plugins.json"),
            r#"{"version":1,"plugins":{"p@p":{"marketplace":"p","plugin":"p","plugin_dir":"marketplaces/p","installed_at":"x"}}}"#,
        )
        .unwrap();

        let working = tempfile::tempdir().unwrap();
        let hooks = load_hooks_config(working.path());
        assert!(hooks.iter().any(|h| h.command == "echo hi"));

        std::env::remove_var("ATOMCODE_HOME");
    }
}
