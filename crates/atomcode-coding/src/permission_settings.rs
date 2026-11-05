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

struct ProjectRules {
    cwd: Arc<std::sync::RwLock<PathBuf>>,
    root: PathBuf,
    dirs: ProductDirs,
    baseline: Arc<PermissionRules>,
    active: Arc<PermissionRules>,
}

pub(crate) fn project_rule_store(
    cwd: Arc<std::sync::RwLock<PathBuf>>,
    root: PathBuf,
    dirs: ProductDirs,
    baseline: Arc<PermissionRules>,
    active: Arc<PermissionRules>,
) -> Arc<dyn atomcode_capabilities::tools::approval::ProjectRuleStore> {
    Arc::new(ProjectRules {
        cwd,
        root,
        dirs,
        baseline,
        active,
    })
}

#[async_trait::async_trait]
impl atomcode_capabilities::tools::approval::ProjectRuleStore for ProjectRules {
    fn explanation(&self, call: &atomcode_kernel::tool::ToolCall) -> String {
        let Ok(cwd) = self.cwd.read() else {
            return "Working directory is unavailable; approval required.".into();
        };
        if *cwd != self.root {
            return "The project changed; this approval is stale.".into();
        }
        let cause = serde_json::from_str::<serde_json::Value>(&call.arguments)
            .ok()
            .and_then(|v| {
                v.get("command")
                    .and_then(|c| c.as_str())
                    .and_then(atomcode_capabilities::tools::bash::check_destructive_command)
            })
            .unwrap_or_else(|| {
                "This tool requires approval under the ordinary safety policy.".into()
            });
        format!(
            "{cause}\n{}",
            self.active.explanation(
                &atomcode_capabilities::tools::SensitivePaths::of(&self.dirs),
                &call.name,
                &call.arguments,
                &cwd
            )
        )
    }

    async fn propose(
        &self,
        call: &atomcode_kernel::tool::ToolCall,
    ) -> Option<atomcode_capabilities::tools::approval::ProjectRuleProposal> {
        if call.name != "bash" {
            return None;
        }
        let cwd = self.cwd.read().ok()?.clone();
        if cwd != self.root {
            return None;
        }
        let args: serde_json::Value = serde_json::from_str(&call.arguments).ok()?;
        let command = args.get("command")?.as_str()?.trim();
        if command.is_empty() || command.len() > 8192 || command.contains(['*', '?']) {
            return None;
        }
        // The capabilities parser validates eligibility through the normal rule
        // matcher; no second shell parser or project directory lookup is introduced.
        let rule = format!(
            "Bash({})",
            atomcode_capabilities::tools::normalize_command_for_grant(command)
        );
        let (candidate, invalid) = PermissionRules::parse(std::slice::from_ref(&rule), &[]);
        if !invalid.is_empty() {
            return None;
        }
        let sensitive = atomcode_capabilities::tools::SensitivePaths::of(&self.dirs);
        if sensitive
            .references_in(
                &call.arguments,
                &cwd,
                &atomcode_capabilities::world::LocalFs::unfenced(),
            )
            .await
        {
            return None;
        }
        if candidate.decide(&sensitive, &call.name, &call.arguments, &cwd)
            != atomcode_capabilities::tools::permission_rules::RuleDecision::Allow
        {
            return None;
        }
        let settings = load(&self.baseline, &self.dirs, &cwd).ok()?;
        if settings
            .rules
            .decide(&sensitive, &call.name, &call.arguments, &cwd)
            == atomcode_capabilities::tools::permission_rules::RuleDecision::Deny
        {
            return None;
        }
        Some(
            atomcode_capabilities::tools::approval::ProjectRuleProposal {
                rule,
                file: self
                    .dirs
                    .project(&cwd)
                    .join("settings.local.json")
                    .to_str()?
                    .into(),
            },
        )
    }

    async fn save(
        &self,
        call: &atomcode_kernel::tool::ToolCall,
        proposal: &atomcode_capabilities::tools::approval::ProjectRuleProposal,
    ) -> Result<(), String> {
        if self.propose(call).await.as_ref() != Some(proposal) {
            return Err("the project or policy changed; this proposal is stale".into());
        }
        let path = PathBuf::from(&proposal.file);
        let rule = proposal.rule.clone();
        let sensitive = atomcode_capabilities::tools::SensitivePaths::of(&self.dirs);
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || save_exact_rule(&path, &rule, &sensitive, &root))
            .await
            .map_err(|e| e.to_string())?
    }
}

/// Patch only permissions.allow under an advisory writer lock, then atomically
/// replace the file. Broken policy and symlinks are errors, never empty settings.
fn save_exact_rule(
    path: &Path,
    rule: &str,
    sensitive: &atomcode_capabilities::tools::SensitivePaths,
    cwd: &Path,
) -> Result<(), String> {
    use fs2::FileExt;
    use std::io::Write;
    let operation = || -> Result<(), Box<dyn std::error::Error>> {
        let directory = path.parent().ok_or("missing project settings directory")?;
        std::fs::create_dir_all(directory)?;
        if std::fs::symlink_metadata(directory)?
            .file_type()
            .is_symlink()
        {
            return Err("project settings directory is a symlink".into());
        }
        let lock_path = directory.join("permission-rules.lock");
        if std::fs::symlink_metadata(&lock_path)
            .is_ok_and(|m| !m.is_file() || m.file_type().is_symlink())
        {
            return Err("invalid permission writer lock".into());
        }
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        lock.try_lock_exclusive()?;
        let mut value = match std::fs::symlink_metadata(path) {
            Ok(meta)
                if meta.is_file()
                    && !meta.file_type().is_symlink()
                    && meta.len() <= 2 * 1024 * 1024 =>
            {
                serde_json::from_slice::<serde_json::Value>(&std::fs::read(path)?)?
            }
            Ok(_) => {
                return Err("settings must be a regular, non-symlink JSON file under 2 MiB".into())
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => serde_json::json!({}),
            Err(e) => return Err(e.into()),
        };
        let settings: Settings = serde_json::from_value(value.clone())?;
        let (rules, invalid) =
            PermissionRules::parse(&settings.permissions.allow, &settings.permissions.deny);
        if !invalid.is_empty() {
            return Err("settings contain malformed permission rules".into());
        }
        let command = rule
            .strip_prefix("Bash(")
            .and_then(|s| s.strip_suffix(')'))
            .ok_or("expected an exact Bash rule")?;
        if command.contains(['*', '?']) {
            return Err("project save cannot broaden a command into a wildcard".into());
        }
        if rules.decide(
            sensitive,
            "bash",
            &serde_json::json!({"command":command}).to_string(),
            cwd,
        ) == atomcode_capabilities::tools::permission_rules::RuleDecision::Deny
        {
            return Err("a deny rule now blocks this command".into());
        }
        let root = value.as_object_mut().ok_or("settings must be an object")?;
        let permissions = root
            .entry("permissions")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .ok_or("permissions must be an object")?;
        let allows = permissions
            .entry("allow")
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .ok_or("permissions.allow must be an array")?;
        if !allows.iter().all(|v| v.is_string()) {
            return Err("permissions.allow contains a non-string rule".into());
        }
        if allows.iter().any(|v| v.as_str() == Some(rule)) {
            return Ok(());
        }
        allows.push(serde_json::Value::String(rule.into()));
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        temporary.write_all(&serde_json::to_vec_pretty(&value)?)?;
        temporary.write_all(b"\n")?;
        temporary.as_file().sync_all()?;
        temporary.persist(path)?;
        Ok(())
    };
    operation().map_err(|error| error.to_string())
}

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
            rules.extend(parsed.with_source(path.display().to_string()));
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

    #[test]
    fn exact_rule_writer_preserves_data_deduplicates_and_refuses_broken_policy() {
        let root = tempfile::tempdir().unwrap();
        let dirs = ProductDirs::new(root.path().join("user"), ".ours");
        let cwd = root.path().join("project");
        let path = dirs.project(&cwd).join("settings.local.json");
        let sensitive = SensitivePaths::of(&dirs);
        write(
            &path,
            serde_json::json!({"other":{"keep":1}, "permissions":{"allow":[], "deny":["Bash(rm -rf protected)"]}}),
        );
        save_exact_rule(&path, "Bash(rm -rf scratch)", &sensitive, &cwd).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        save_exact_rule(&path, "Bash(rm -rf scratch)", &sensitive, &cwd).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["other"]["keep"], 1);
        assert_eq!(value["permissions"]["allow"].as_array().unwrap().len(), 1);
        assert!(save_exact_rule(&path, "Bash(rm -rf protected)", &sensitive, &cwd).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        for broken in [
            "{bad",
            r#"{"permissions":{"allow":false}}"#,
            r#"{"permissions":{"deny":[42]}}"#,
        ] {
            std::fs::write(&path, broken).unwrap();
            assert!(save_exact_rule(&path, "Bash(rm -rf scratch)", &sensitive, &cwd).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
        }
    }

    #[cfg(unix)]
    #[test]
    fn exact_rule_writer_refuses_symlinks_and_an_active_writer_lock() {
        use fs2::FileExt;
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let dirs = ProductDirs::new(root.path().join("user"), ".ours");
        let cwd = root.path().join("project");
        let directory = dirs.project(&cwd);
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("settings.local.json");
        let outside = root.path().join("outside.json");
        std::fs::write(&outside, "{}").unwrap();
        symlink(&outside, &path).unwrap();
        let sensitive = SensitivePaths::of(&dirs);
        assert!(save_exact_rule(&path, "Bash(rm -rf scratch)", &sensitive, &cwd).is_err());
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "{}");
        std::fs::remove_file(&path).unwrap();
        let lock = std::fs::OpenOptions::new()
            .write(true)
            .open(directory.join("permission-rules.lock"))
            .unwrap();
        lock.lock_exclusive().unwrap();
        assert!(save_exact_rule(&path, "Bash(rm -rf scratch)", &sensitive, &cwd).is_err());
        assert!(!path.exists());
    }

    /// Windows counterpart of the criterion above. None of the writer's guarantees lean on
    /// `symlink(2)`: `LockFileEx` enforces the writer lock here as well, and a directory
    /// junction — which needs no elevation — is what `symlink_metadata` reports as a link on
    /// Windows. Real file symlinks require Developer Mode or elevation, so that branch stays
    /// covered by the unix criterion instead of making this one environment-dependent.
    #[cfg(windows)]
    #[test]
    fn exact_rule_writer_refuses_directory_junctions_and_an_active_writer_lock() {
        use fs2::FileExt;
        use std::os::windows::process::CommandExt;

        let root = tempfile::tempdir().unwrap();
        let dirs = ProductDirs::new(root.path().join("user"), ".ours");
        let cwd = root.path().join("project");
        let directory = dirs.project(&cwd);
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("settings.local.json");
        let sensitive = SensitivePaths::of(&dirs);

        // A junction standing in for the settings directory must be refused rather than
        // written through to whatever it points at.
        let outside = root.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let linked = root.path().join("settings-via-junction");
        let made = std::process::Command::new("cmd")
            .arg("/c")
            .raw_arg(format!(
                "mklink /J \"{}\" \"{}\"",
                linked.display(),
                outside.display()
            ))
            .output()
            .expect("spawn mklink");
        if made.status.success() && linked.exists() {
            let through = linked.join("settings.local.json");
            let error =
                save_exact_rule(&through, "Bash(rm -rf scratch)", &sensitive, &cwd).unwrap_err();
            assert!(error.contains("symlink"), "{error}");
            assert!(!outside.join("settings.local.json").exists());
        } else {
            // No junction support in this environment: say so instead of passing silently.
            eprintln!(
                "skipping the junction half: mklink /J failed: {}",
                String::from_utf8_lossy(&made.stderr).trim()
            );
        }

        // A lock path occupied by a directory is not a writer lock.
        let lock_path = directory.join("permission-rules.lock");
        std::fs::create_dir_all(&lock_path).unwrap();
        let error = save_exact_rule(&path, "Bash(rm -rf scratch)", &sensitive, &cwd).unwrap_err();
        assert!(error.contains("writer lock"), "{error}");
        assert!(!path.exists());
        std::fs::remove_dir(&lock_path).unwrap();

        // An active writer lock fails the save closed: `LockFileEx` rejects the second
        // handle with ERROR_LOCK_VIOLATION, the same outcome unix reaches through flock.
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        lock.try_lock_exclusive().unwrap();
        assert!(save_exact_rule(&path, "Bash(rm -rf scratch)", &sensitive, &cwd).is_err());
        assert!(!path.exists());
        let _ = FileExt::unlock(&lock);
    }

    #[tokio::test]
    async fn project_rule_proposals_are_bound_to_the_original_directory() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("old");
        let next = root.path().join("next");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&next).unwrap();
        let dirs = ProductDirs::new(root.path().join("user"), ".ours");
        let cwd = Arc::new(std::sync::RwLock::new(old.clone()));
        let rules = Arc::new(PermissionRules::default());
        let store =
            project_rule_store(cwd.clone(), old.clone(), dirs.clone(), rules.clone(), rules);
        let call = atomcode_kernel::tool::ToolCall {
            id: "call".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"command":"rm -rf scratch"}).to_string(),
        };
        let proposal = store.propose(&call).await.unwrap();
        *cwd.write().unwrap() = next.clone();
        assert!(store.save(&call, &proposal).await.is_err());
        assert!(!dirs.project(&old).join("settings.local.json").exists());
        assert!(!dirs.project(&next).join("settings.local.json").exists());
    }

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
