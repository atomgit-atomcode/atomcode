//! JSON permission scopes resolved at the runtime's mount boundary.
//!
//! Do not fold these into the retained agent config: its TOML rules are the
//! baseline for every project, and a `/cd` must discard the previous project's
//! JSON grants. No process cwd or product-home environment lookup lives here.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use atomcode_capabilities::tools::PermissionRules;
use atomcode_capabilities::ProductDirs;
use serde::Deserialize;

#[derive(Clone, Debug, Default)]
pub struct PermissionSettings {
    pub rules: Arc<PermissionRules>,
    /// Directories actually searched, including scopes with no file yet.
    pub directories: Vec<PathBuf>,
    /// Existing settings files, for the enforcing row's self-description.
    pub sources: Vec<PathBuf>,
}

// JSON scopes import rules only. A TOML startup mode (or Claude's
// defaultMode) must not change the live mode on a capability reload or /cd.
#[derive(Default, Deserialize)]
#[serde(default)]
struct RuleSettings {
    allow: Vec<String>,
    deny: Vec<String>,
}

#[derive(Deserialize)]
struct Settings {
    #[serde(default)]
    permissions: RuleSettings,
}

pub(crate) fn load(
    baseline: &PermissionRules,
    dirs: &ProductDirs,
    working_dir: &Path,
) -> Result<PermissionSettings, String> {
    let mut rules = baseline.clone();
    let mut sources = Vec::new();
    let directories = vec![
        dirs.user().to_path_buf(),
        working_dir.join(".claude"),
        dirs.project(working_dir),
    ];
    for directory in &directories {
        for name in ["settings.json", "settings.local.json"] {
            let path = directory.join(name);
            if sources.contains(&path) {
                continue;
            }
            let metadata = match std::fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(error)
                    if error.kind() == io::ErrorKind::NotFound
                        && std::fs::symlink_metadata(&path)
                            .is_err_and(|error| error.kind() == io::ErrorKind::NotFound) =>
                {
                    continue;
                }
                Err(error) => {
                    return Err(format!(
                        "cannot read permissions from {}: {error}",
                        path.display()
                    ))
                }
            };
            // A present but broken link is an unreadable policy, not an absent
            // one. Reject non-files before reading so a FIFO cannot park start.
            if !metadata.is_file() {
                return Err(format!(
                    "invalid permission settings in {}: expected a regular file",
                    path.display()
                ));
            }
            let bytes = std::fs::read(&path).map_err(|error| {
                format!("cannot read permissions from {}: {error}", path.display())
            })?;
            let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
                format!("invalid permission settings in {}: {error}", path.display())
            })?;
            if !value.is_object() {
                return Err(format!(
                    "invalid permission settings in {}: expected a JSON object",
                    path.display()
                ));
            }
            if value
                .get("permissions")
                .is_some_and(|permissions| !permissions.is_object())
            {
                return Err(format!(
                    "invalid permission settings in {}: permissions must be an object",
                    path.display()
                ));
            }
            let settings: Settings = serde_json::from_value(value).map_err(|error| {
                format!("invalid permission settings in {}: {error}", path.display())
            })?;
            let (parsed, invalid) =
                PermissionRules::parse(&settings.permissions.allow, &settings.permissions.deny);
            if !invalid.is_empty() {
                // Include the source, not arbitrary settings content, in the error.
                return Err(format!(
                    "invalid permission rule in {} (expected Tool or Tool(pattern))",
                    path.display()
                ));
            }
            rules.extend(parsed);
            sources.push(path);
        }
    }
    Ok(PermissionSettings {
        rules: Arc::new(rules),
        directories,
        sources,
    })
}

impl PermissionSettings {
    pub(crate) fn describe(&self) -> String {
        let directories = self
            .directories
            .iter()
            .map(|path| format!("`{}`", path.display()))
            .collect::<Vec<_>>()
            .join(", ");
        let sources = self
            .sources
            .iter()
            .map(|path| format!("`{}`", path.display()))
            .collect::<Vec<_>>()
            .join(", ");
        let sources = if sources.is_empty() { "none" } else { &sources };
        format!(
            "PERMISSION SETTINGS. At mount this runtime reads `settings.json` and \
             `settings.local.json` in its product user directory, and in `.claude` and \
             its product directory under the working directory (no ancestor search). \
             Directories searched: {directories}. \
             Existing files read: {sources}. Only `permissions.allow` and `permissions.deny` \
             are consumed; other fields do not configure hooks or other settings. These \
             lists merge with config.toml permissions; deny wins across all files. Bare \
             tool denies hide the tool and block calls; command/path denies block only \
             matching calls. Allow skips ordinary approval, never a hard safety boundary. \
             Settings are read again on a full tool-tree rebuild, including a directory \
             change or a new session. A model-only provider patch or a skills-only reload \
             preserves this tree's rules; edits do not change the current tree. Plugin hook \
             subprocesses use plugin trust, not Bash tool permission rules."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_capabilities::tools::{RuleDecision, SensitivePaths};

    fn write(path: &Path, value: serde_json::Value) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    }

    #[test]
    fn all_scopes_merge_with_toml_and_any_deny_wins() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let dirs = ProductDirs::new(root.path().join("user"), ".ours");
        let files = [
            dirs.user().join("settings.json"),
            dirs.user().join("settings.local.json"),
            project.join(".claude/settings.json"),
            project.join(".claude/settings.local.json"),
            dirs.project(&project).join("settings.json"),
            dirs.project(&project).join("settings.local.json"),
        ];
        for (i, file) in files.iter().enumerate() {
            write(
                file,
                serde_json::json!({"permissions": {"allow": [format!("Bash(command{i} *)")], "deny": [format!("tool{i}")]}, "hooks": {"ignored": true}}),
            );
        }
        let baseline =
            PermissionRules::parse(&["WebSearch".into()], &["Bash(command5 forbidden)".into()]).0;
        let settings = load(&baseline, &dirs, &project).unwrap();
        assert_eq!(settings.sources, files);
        let sensitive = SensitivePaths::of(&dirs);
        for i in 0..6 {
            assert!(settings.rules.denies_tool(&format!("tool{i}")));
            assert_eq!(
                settings.rules.decide(
                    &sensitive,
                    "bash",
                    &serde_json::json!({"command": format!("command{i} ok")}).to_string(),
                    &project
                ),
                RuleDecision::Allow
            );
        }
        assert_eq!(
            settings.rules.decide(
                &sensitive,
                "bash",
                r#"{"command":"command5 forbidden"}"#,
                &project
            ),
            RuleDecision::Deny
        );
        assert_eq!(
            settings
                .rules
                .decide(&sensitive, "web_search", "{}", &project),
            RuleDecision::Allow
        );
        write(
            &files[0],
            serde_json::json!({"permissions": {"deny": ["WebSearch"]}}),
        );
        let settings = load(&baseline, &dirs, &project).unwrap();
        assert!(settings.rules.denies_tool("web_search"));
        assert!(settings
            .describe()
            .contains(&files[0].display().to_string()));
        assert!(!settings.describe().contains("command5 forbidden"));
    }

    #[test]
    fn a_new_project_does_not_inherit_the_old_projects_rules_or_ancestor_files() {
        let root = tempfile::tempdir().unwrap();
        let dirs = ProductDirs::new(root.path().join("user"), ".ours");
        let old = root.path().join("old");
        let new = old.join("nested");
        write(
            &dirs.project(&old).join("settings.local.json"),
            serde_json::json!({"permissions": {"allow": ["Bash(python3 *)"]}}),
        );
        assert!(!load(&PermissionRules::default(), &dirs, &old)
            .unwrap()
            .rules
            .is_empty());
        assert!(load(&PermissionRules::default(), &dirs, &new)
            .unwrap()
            .rules
            .is_empty());
    }

    #[test]
    fn unrelated_settings_are_ignored_but_present_permissions_must_be_valid() {
        let root = tempfile::tempdir().unwrap();
        let dirs = ProductDirs::new(root.path().join("user"), ".ours");
        let path = dirs.user().join("settings.local.json");
        for value in [
            serde_json::json!({}),
            serde_json::json!({"hooks": {}, "env": {"KEY": "not-for-the-model"}}),
            serde_json::json!({"permissions": {"defaultMode": "bypassPermissions", "default_mode": "not-a-toml-mode"}}),
        ] {
            write(&path, value);
            let settings = load(&PermissionRules::default(), &dirs, root.path()).unwrap();
            assert!(settings.rules.is_empty());
            assert!(!settings.describe().contains("not-for-the-model"));
        }
        for value in [
            serde_json::json!([]),
            serde_json::json!(null),
            serde_json::json!({"permissions": null}),
            serde_json::json!({"permissions": []}),
            serde_json::json!({"permissions": {"deny": "WebSearch"}}),
            serde_json::json!({"permissions": {"allow": [1]}}),
            serde_json::json!({"permissions": {"deny": ["Bash("]}}),
            serde_json::json!({"permissions": {"deny": ["Bash)"]}}),
        ] {
            write(&path, value);
            assert!(load(&PermissionRules::default(), &dirs, root.path())
                .unwrap_err()
                .contains(&path.display().to_string()));
        }
        std::fs::write(&path, b"{").unwrap();
        assert!(load(&PermissionRules::default(), &dirs, root.path()).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(load(&PermissionRules::default(), &dirs, root.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_settings_link_fails_closed_and_a_regular_file_link_is_supported() {
        let root = tempfile::tempdir().unwrap();
        let dirs = ProductDirs::new(root.path().join("user"), ".ours");
        std::fs::create_dir_all(dirs.user()).unwrap();
        let target = root.path().join("shared-settings.json");
        let link = dirs.user().join("settings.local.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let error = load(&PermissionRules::default(), &dirs, root.path()).unwrap_err();
        assert!(error.contains(&link.display().to_string()), "{error}");
        write(
            &target,
            serde_json::json!({"permissions": {"deny": ["WebFetch"]}}),
        );
        let settings = load(&PermissionRules::default(), &dirs, root.path()).unwrap();
        assert!(settings.rules.denies_tool("web_fetch"));
        assert_eq!(settings.sources, vec![link]);
    }

    #[cfg(unix)]
    #[test]
    fn a_non_file_settings_source_is_rejected_before_reading() {
        let root = tempfile::tempdir().unwrap();
        let dirs = ProductDirs::new(root.path().join("user"), ".ours");
        std::fs::create_dir_all(dirs.user()).unwrap();
        let path = dirs.user().join("settings.json");
        let _socket = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let error = load(&PermissionRules::default(), &dirs, root.path()).unwrap_err();
        assert!(error.contains("expected a regular file"), "{error}");
    }
}
