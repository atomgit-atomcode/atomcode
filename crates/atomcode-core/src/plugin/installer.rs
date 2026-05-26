use anyhow::{anyhow, bail, Context, Result};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use super::manifest::{ExternalSource, GitPin, PluginEntry, PluginSource};
use super::marketplace::sanitize_name;
use super::paths;
use super::state::{
    load_installed_plugins_file, load_marketplaces_file, plugin_id, save_installed_plugins_file,
    InstalledPluginEntry,
};
use super::url::validate_git_url;

#[derive(Debug, Clone)]
pub struct InstalledPluginInfo {
    pub plugin: String,
    pub marketplace: String,
    pub plugin_dir: String,
}

/// Resolve an inline (relative-to-marketplace-root) source string into the
/// canonical `plugin_dir` recorded in `installed_plugins.json`. Rejects path
/// traversal up front.
fn resolve_inline_dir(source: &str, mp_root_rel: &str) -> Result<String> {
    validate_plugin_source(source)?;
    let normalized = source.trim_start_matches("./");
    if normalized.is_empty() {
        Ok(mp_root_rel.to_string())
    } else {
        Ok(format!(
            "{}/{}",
            mp_root_rel,
            normalized.trim_end_matches('/')
        ))
    }
}

/// Realize an external plugin source by cloning (url/git/github) or copying
/// (local) into `installed/<marketplace>/<plugin>/`. Returns the relative
/// `plugin_dir` to record in state.
fn install_external(plugin_key: &str, marketplace: &str, ext: &ExternalSource) -> Result<String> {
    let plugins_root = paths::plugins_root().ok_or_else(|| anyhow!("no plugin home"))?;
    let target_rel = format!("installed/{}/{}", marketplace, plugin_key);
    let target_abs = plugins_root.join(&target_rel);
    if target_abs.exists() {
        bail!(
            "plugin install dir already exists: {}",
            target_abs.display()
        );
    }
    if let Some(parent) = target_abs.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    match ext {
        ExternalSource::Url { url, pin } | ExternalSource::Git { url, pin } => {
            validate_git_url(url)?;
            git_clone_with_pin(url, &target_abs, pin).with_context(|| format!("clone {}", url))?;
        }
        ExternalSource::Github { repo, pin } => {
            let url = expand_github_repo(repo)?;
            git_clone_with_pin(&url, &target_abs, pin).with_context(|| format!("clone {}", url))?;
        }
        ExternalSource::Local { path } => {
            let src = expand_local_path(path)?;
            copy_dir_recursive(&src, &target_abs)
                .with_context(|| format!("copy {}", src.display()))?;
        }
    }
    Ok(target_rel)
}

/// Expand a `github` shorthand (`owner/name`) into the canonical clone URL.
/// Reject anything that doesn't look like a single `owner/name` segment to
/// avoid command injection or path traversal in the resulting URL.
fn expand_github_repo(repo: &str) -> Result<String> {
    let trimmed = repo.trim().trim_end_matches(".git").trim_matches('/');
    let parts: Vec<&str> = trimmed.split('/').collect();
    if parts.len() != 2 || parts.iter().any(|s| s.is_empty()) {
        bail!("github repo must be in `owner/name` form, got `{}`", repo);
    }
    for seg in &parts {
        if !seg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            || seg.contains("..")
        {
            bail!("github repo `{}` contains disallowed characters", repo);
        }
        // Reject leading `-`: `git clone https://github.com/-x/foo.git`
        // (or any URL whose path component begins with `-`) lets git
        // interpret the segment as a flag — CVE-2017-1000117 family.
        if seg.starts_with('-') {
            bail!("github repo `{}` segment must not start with '-'", repo);
        }
    }
    Ok(format!("https://github.com/{}/{}.git", parts[0], parts[1]))
}

/// Expand a local filesystem path. `~` is expanded relative to the user's
/// home dir; relative paths are interpreted from the current working dir.
fn expand_local_path(path: &str) -> Result<PathBuf> {
    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        crate::tool::real_home_dir()
            .ok_or_else(|| anyhow!("no home dir to expand `~`"))?
            .join(rest)
    } else if path == "~" {
        crate::tool::real_home_dir().ok_or_else(|| anyhow!("no home dir to expand `~`"))?
    } else {
        PathBuf::from(path)
    };
    if !expanded.exists() {
        bail!("local plugin source does not exist: {}", expanded.display());
    }
    Ok(expanded)
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if ty.is_symlink() {
            // Resolve symlinks by copying the target file. Avoids leaving
            // dangling links inside the install dir.
            let resolved = std::fs::read_link(&from)?;
            let abs = if resolved.is_absolute() {
                resolved
            } else {
                from.parent().unwrap_or(Path::new(".")).join(resolved)
            };
            if abs.is_dir() {
                copy_dir_recursive(&abs, &to)?;
            } else {
                std::fs::copy(&abs, &to)?;
            }
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

fn git_clone_with_pin(url: &str, target: &Path, pin: &GitPin) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.arg("clone");
    let needs_full_history = pin.commit.is_some() || pin.tag.is_some() || pin.git_ref.is_some();
    if !needs_full_history {
        cmd.args(["--depth", "1"]);
    }
    if let Some(branch) = &pin.branch {
        cmd.args(["--branch", branch]);
    }
    cmd.arg(url).arg(target);
    let out = cmd.output().context("spawn git clone")?;
    if !out.status.success() {
        bail!("git clone failed: {}", String::from_utf8_lossy(&out.stderr));
    }

    // Apply commit/tag/ref pin via post-clone checkout.
    let pin_ref = pin
        .commit
        .as_deref()
        .or(pin.tag.as_deref())
        .or(pin.git_ref.as_deref());
    if let Some(rev) = pin_ref {
        let out = Command::new("git")
            .args(["checkout", "--detach", rev])
            .current_dir(target)
            .output()
            .context("spawn git checkout")?;
        if !out.status.success() {
            bail!(
                "git checkout {} failed: {}",
                rev,
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
    Ok(())
}

/// Strip surface-level differences (trailing slash, `.git` suffix, whitespace)
/// so two URLs that point at the same repo compare equal. Case is preserved
/// because path components are case-sensitive on most git hosts.
fn normalize_git_url(u: &str) -> String {
    u.trim()
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_string()
}

/// Decide whether an external source points at the same repo as the
/// marketplace's own clone URL. Returns false whenever a `GitPin` is set,
/// since the marketplace working tree is on its default branch and may not
/// match the requested revision.
fn external_matches_marketplace(ext: &ExternalSource, mp_url: &str) -> bool {
    let (url, pin) = match ext {
        ExternalSource::Url { url, pin } | ExternalSource::Git { url, pin } => (url.clone(), pin),
        ExternalSource::Github { repo, pin } => match expand_github_repo(repo) {
            Ok(u) => (u, pin),
            Err(_) => return false,
        },
        ExternalSource::Local { .. } => return false,
    };
    if pin.branch.is_some() || pin.tag.is_some() || pin.commit.is_some() || pin.git_ref.is_some() {
        return false;
    }
    normalize_git_url(&url) == normalize_git_url(mp_url)
}

/// Validate that an inline plugin source path (declared in marketplace.json)
/// only contains plain forward components. Reject `..`, absolute paths, and
/// any other non-`Normal` component to prevent escaping the marketplace root.
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
                    bail!(
                        "plugin source path '{}' contains disallowed components",
                        source
                    );
                }
            }
            Component::CurDir => {
                // "./" is fine; skip.
            }
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!(
                    "plugin source path '{}' contains disallowed components",
                    source
                );
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
        bail!(
            "plugin `{}` not found in marketplace `{}`",
            plugin,
            marketplace
        );
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
            source: PluginSource::Inline("./".into()),
            description: None,
        },
    };

    // Sanitize the plugin name component of the canonical id; the marketplace
    // is already a sanitized key (enforced in add_marketplace).
    let plugin_key = sanitize_name(plugin);
    if plugin_key.is_empty() {
        bail!("plugin name `{}` sanitized to empty string", plugin);
    }

    let plugin_dir_rel = match &plugin_entry.source {
        PluginSource::Inline(s) => resolve_inline_dir(s, &mp_root_rel)?,
        PluginSource::External(ext) => {
            // Dedup: if the external source resolves to the same git URL as
            // the marketplace itself (and no pin overrides the working tree),
            // reuse the marketplace clone instead of cloning twice.
            if external_matches_marketplace(ext, &entry.source) {
                mp_root_rel.clone()
            } else {
                install_external(&plugin_key, marketplace, ext)?
            }
        }
    };

    let id = plugin_id(&plugin_key, marketplace);
    let installed_path = paths::installed_plugins_file().unwrap();
    let mut installed = load_installed_plugins_file(&installed_path)?;
    if installed.plugins.contains_key(&id) {
        // Roll back any external clone that we just created so retries work.
        if plugin_dir_rel.starts_with("installed/") {
            let abs = paths::plugins_root().unwrap().join(&plugin_dir_rel);
            std::fs::remove_dir_all(&abs).ok();
        }
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
    let entry = installed
        .plugins
        .remove(&id)
        .ok_or_else(|| anyhow!("plugin `{}` not installed", id))?;
    save_installed_plugins_file(&installed_path, &installed)?;

    // Garbage-collect external clones. `marketplaces/*` belongs to the
    // marketplace itself and must be left intact for any sibling plugins.
    if entry.plugin_dir.starts_with("installed/") {
        if let Some(root) = paths::plugins_root() {
            let abs = root.join(&entry.plugin_dir);
            if abs.exists() {
                std::fs::remove_dir_all(&abs).ok();
            }
        }
    }
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
        let work = tempfile::tempdir().unwrap().keep();
        let repo = work.join(name);
        std::fs::create_dir_all(&repo).unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "t@t"])
            .current_dir(&repo)
            .status()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "t"])
            .current_dir(&repo)
            .status()
            .unwrap();
        if let Some(m) = manifest {
            std::fs::create_dir_all(repo.join(".atomcode-plugin")).unwrap();
            std::fs::write(repo.join(".atomcode-plugin/marketplace.json"), m).unwrap();
        }
        std::fs::write(repo.join("README"), "x").unwrap();
        Command::new("git")
            .args(["add", "-A"])
            .current_dir(&repo)
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "-q", "-m", "init"])
            .current_dir(&repo)
            .status()
            .unwrap();
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
        Command::new("git")
            .args(["add", "-A"])
            .current_dir(&repo)
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "-q", "-m", "add sub"])
            .current_dir(&repo)
            .status()
            .unwrap();
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

    /// External `url` source: marketplace declares one URL but the plugin
    /// lives in a separate repo. Installer must clone that repo into
    /// `installed/<mp>/<plugin>/`.
    #[test]
    #[serial_test::serial]
    fn install_external_url_clones_separate_repo() {
        let _home = isolated_home();
        // The plugin's own repo (cloned by install_external).
        let plugin_repo = make_repo("upstream", None);
        // Pre-create a marker file so we can verify the clone landed.
        std::fs::write(plugin_repo.join("PLUGIN_MARKER"), "yes").unwrap();
        Command::new("git")
            .args(["add", "-A"])
            .current_dir(&plugin_repo)
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "-q", "-m", "marker"])
            .current_dir(&plugin_repo)
            .status()
            .unwrap();

        // Marketplace repo whose manifest references the plugin repo by URL.
        let plugin_url = format!("file://{}", plugin_repo.display());
        let manifest = format!(
            r#"{{"name":"mp_ext","plugins":[{{"name":"ext","source":{{"source":"url","url":"{}"}}}}]}}"#,
            plugin_url
        );
        let mp_repo = make_repo("mp_ext", Some(&manifest));
        add_marketplace(&format!("file://{}", mp_repo.display())).unwrap();

        let info = install("ext", "mp_ext").unwrap();
        assert_eq!(info.plugin_dir, "installed/mp_ext/ext");

        let abs = paths::plugins_root().unwrap().join(&info.plugin_dir);
        assert!(abs.join("PLUGIN_MARKER").exists(), "external clone missing");

        // uninstall must wipe the installed/* dir.
        uninstall("ext", "mp_ext").unwrap();
        assert!(!abs.exists(), "uninstall should remove installed/* clone");
    }

    /// External `local` source: copy a directory tree into the install dir.
    #[test]
    #[serial_test::serial]
    fn install_external_local_copies_tree() {
        let _home = isolated_home();
        let local_src = tempfile::tempdir().unwrap().keep();
        std::fs::create_dir_all(local_src.join("skills/x")).unwrap();
        std::fs::write(local_src.join("skills/x/SKILL.md"), "body").unwrap();

        let manifest = format!(
            r#"{{"name":"mp_local","plugins":[{{"name":"loc","source":{{"source":"local","path":"{}"}}}}]}}"#,
            local_src.display()
        );
        let mp_repo = make_repo("mp_local", Some(&manifest));
        add_marketplace(&format!("file://{}", mp_repo.display())).unwrap();
        let info = install("loc", "mp_local").unwrap();

        let abs = paths::plugins_root().unwrap().join(&info.plugin_dir);
        assert!(abs.join("skills/x/SKILL.md").exists(), "local copy missing");
    }

    /// Real-world ascend pattern: the marketplace.json's plugin source URL
    /// is the same repo as the marketplace itself. Installer must reuse the
    /// marketplace clone instead of cloning a second copy.
    #[test]
    #[serial_test::serial]
    fn install_external_url_dedups_with_marketplace() {
        let _home = isolated_home();
        // Single repo whose manifest references its own clone URL.
        let work = tempfile::tempdir().unwrap().keep();
        let repo = work.join("self_ref");
        std::fs::create_dir_all(&repo).unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "t@t"])
            .current_dir(&repo)
            .status()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "t"])
            .current_dir(&repo)
            .status()
            .unwrap();
        std::fs::create_dir_all(repo.join(".atomcode-plugin")).unwrap();
        let url = format!("file://{}", repo.display());
        let manifest = format!(
            r#"{{"name":"self_ref","plugins":[{{"name":"self_ref","source":{{"source":"url","url":"{}"}}}}]}}"#,
            url
        );
        std::fs::write(repo.join(".atomcode-plugin/marketplace.json"), manifest).unwrap();
        std::fs::write(repo.join("README"), "x").unwrap();
        Command::new("git")
            .args(["add", "-A"])
            .current_dir(&repo)
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "-q", "-m", "init"])
            .current_dir(&repo)
            .status()
            .unwrap();

        add_marketplace(&url).unwrap();
        let info = install("self_ref", "self_ref").unwrap();

        // Dedup must land in marketplaces/, not installed/.
        assert_eq!(info.plugin_dir, "marketplaces/self_ref");
        let installed_root = paths::plugins_root().unwrap().join("installed");
        assert!(
            !installed_root.exists()
                || std::fs::read_dir(&installed_root).unwrap().next().is_none(),
            "dedup should skip the installed/ tree entirely"
        );
    }

    /// Same external URL but with a branch pin must NOT dedup — the
    /// marketplace clone is on the default branch, which may differ.
    #[test]
    fn dedup_skipped_when_pin_set() {
        let url = "https://example.com/r.git";
        let mut pin = GitPin::default();
        pin.branch = Some("dev".into());
        let ext = ExternalSource::Url {
            url: url.into(),
            pin,
        };
        assert!(!external_matches_marketplace(&ext, url));
    }

    #[test]
    fn normalize_git_url_strips_suffix_and_slash() {
        assert_eq!(normalize_git_url("https://x/r.git"), "https://x/r");
        assert_eq!(normalize_git_url("https://x/r/"), "https://x/r");
        assert_eq!(normalize_git_url("https://x/r.git/"), "https://x/r");
        assert_eq!(normalize_git_url("https://x/r"), "https://x/r");
    }

    #[test]
    fn expand_github_repo_basic() {
        assert_eq!(
            expand_github_repo("anthropic/claude").unwrap(),
            "https://github.com/anthropic/claude.git"
        );
        assert_eq!(
            expand_github_repo("anthropic/claude.git").unwrap(),
            "https://github.com/anthropic/claude.git"
        );
        assert!(expand_github_repo("just-name").is_err());
        assert!(expand_github_repo("a/b/c").is_err());
        assert!(expand_github_repo("../etc/passwd").is_err());
        assert!(expand_github_repo("a/..").is_err());
        assert!(expand_github_repo("$(rm -rf)/x").is_err());
        // CVE-2017-1000117 family: `-x` would be treated as a git flag.
        assert!(expand_github_repo("-x/repo").is_err());
        assert!(expand_github_repo("repo/-x").is_err());
    }

    /// `Local` source must never dedup against the marketplace clone — a
    /// local path could point anywhere on disk, so reusing the marketplace
    /// dir would silently swap the user's intended files for the
    /// marketplace's.
    #[test]
    fn dedup_skipped_for_local_source() {
        let ext = ExternalSource::Local {
            path: "/tmp/x".into(),
        };
        assert!(!external_matches_marketplace(&ext, "/tmp/x"));
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
