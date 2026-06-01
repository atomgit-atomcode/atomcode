use anyhow::{anyhow, bail, Context, Result};
use std::path::Path;
use std::process::Command;

use super::manifest::{load_marketplace_manifest, MarketplaceManifest, PluginSource};
use super::paths;
use super::state::{load_marketplaces_file, save_marketplaces_file, MarketplaceEntry};
use super::url::{infer_marketplace_name_from_url, validate_git_url};

/// Sanitize a name into a path-safe segment (CC convention).
pub fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect()
}

#[derive(Debug, Clone)]
pub struct MarketplaceInfo {
    pub name: String,
    pub source: String,
    pub git_commit: String,
    pub plugins: Vec<String>,
}

/// Clone a marketplace, parse its manifest, and persist registration.
/// Caller is responsible for showing UX (spinner). This call blocks on git.
pub fn add_marketplace(url: &str) -> Result<MarketplaceInfo> {
    validate_git_url(url)?;
    let url_tail = infer_marketplace_name_from_url(url)?;

    let mp_root = paths::marketplaces_root().ok_or_else(|| anyhow!("no plugin home"))?;
    std::fs::create_dir_all(&mp_root).ok();

    // Clone into a temp directory inside marketplaces/ so we can read the
    // manifest, then determine the canonical (sanitized) name and rename to
    // its final location atomically. This avoids the prior key/dir mismatch
    // when manifest.name differs from the URL tail.
    let tmp_suffix: u128 = {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
    };
    let tmp_dir = mp_root.join(format!(".tmp-{}-{}", std::process::id(), tmp_suffix));
    if tmp_dir.exists() {
        std::fs::remove_dir_all(&tmp_dir).ok();
    }

    let cleanup = |p: &Path| {
        if p.exists() {
            std::fs::remove_dir_all(p).ok();
        }
    };

    if let Err(e) = git_clone(url, &tmp_dir) {
        cleanup(&tmp_dir);
        return Err(e).with_context(|| format!("clone {}", url));
    }

    let commit = match git_rev_parse(&tmp_dir) {
        Ok(c) => c,
        Err(e) => {
            cleanup(&tmp_dir);
            return Err(e);
        }
    };

    let manifest = match load_marketplace_manifest(&tmp_dir) {
        Ok(m) => m,
        Err(e) => {
            cleanup(&tmp_dir);
            return Err(e);
        }
    };

    let (raw_mp_name, plugins) = resolve_marketplace_identity(&manifest, &url_tail);
    // Sanitize the chosen name so a hostile manifest cannot escape the
    // marketplaces/ directory via "..", "/" or absolute paths.
    let mp_name = sanitize_name(&raw_mp_name);
    if mp_name.is_empty() {
        cleanup(&tmp_dir);
        bail!("marketplace name `{}` sanitized to empty string", raw_mp_name);
    }

    let target = mp_root.join(&mp_name);

    let mp_file = paths::marketplaces_file().unwrap();
    let mut state = load_marketplaces_file(&mp_file)?;
    if state.marketplaces.contains_key(&mp_name) {
        cleanup(&tmp_dir);
        bail!("marketplace `{}` already exists; remove first", mp_name);
    }
    if target.exists() {
        cleanup(&tmp_dir);
        bail!(
            "directory {} already exists but is not registered; remove it manually",
            target.display()
        );
    }

    if let Err(e) = std::fs::rename(&tmp_dir, &target) {
        cleanup(&tmp_dir);
        return Err(anyhow!("rename {} -> {}: {}", tmp_dir.display(), target.display(), e));
    }

    let plugins_list = plugins.iter().map(|p| sanitize_name(&p.name)).collect::<Vec<_>>();

    state.marketplaces.insert(
        mp_name.clone(),
        MarketplaceEntry {
            source: url.to_string(),
            added_at: now_rfc3339(),
            git_commit: commit.clone(),
            plugins: plugins_list.clone(),
        },
    );
    save_marketplaces_file(&mp_file, &state)?;

    Ok(MarketplaceInfo {
        name: mp_name,
        source: url.to_string(),
        git_commit: commit,
        plugins: plugins_list,
    })
}

/// Decide the marketplace name + plugin list. When manifest is absent, fall
/// back to single-plugin mode where mp_name = plugin_name = directory name.
pub(super) fn resolve_marketplace_identity(
    manifest: &Option<MarketplaceManifest>,
    dir_name: &str,
) -> (String, Vec<super::manifest::PluginEntry>) {
    use super::manifest::PluginEntry;
    match manifest {
        Some(m) => (m.name.clone(), m.plugins.clone()),
        None => (
            dir_name.to_string(),
            vec![PluginEntry {
                name: dir_name.to_string(),
                source: PluginSource::Inline("./".into()),
                description: None,
            }],
        ),
    }
}

pub(super) fn git_clone(url: &str, target: &Path) -> Result<()> {
    let out = Command::new("git")
        .args(["clone", "--depth", "1", url])
        .arg(target)
        .output()
        .context("spawn git")?;
    if !out.status.success() {
        bail!("git clone failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}

fn git_rev_parse(repo: &Path) -> Result<String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
        .context("spawn git rev-parse")?;
    if !out.status.success() {
        bail!("git rev-parse failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

pub fn remove_marketplace(name: &str) -> Result<()> {
    let mp_file = paths::marketplaces_file().ok_or_else(|| anyhow!("no plugin home"))?;
    let mut state = load_marketplaces_file(&mp_file)?;
    if !state.marketplaces.contains_key(name) {
        bail!("marketplace `{}` not found", name);
    }
    // Refuse if any installed plugin still references this marketplace.
    let installed = super::state::load_installed_plugins_file(
        &paths::installed_plugins_file().unwrap(),
    )?;
    if installed
        .plugins
        .values()
        .any(|p| p.marketplace == name)
    {
        bail!(
            "marketplace `{}` has installed plugins; uninstall them first",
            name
        );
    }
    let target = paths::marketplaces_root().unwrap().join(name);
    if target.exists() {
        std::fs::remove_dir_all(&target).ok();
    }
    state.marketplaces.remove(name);
    save_marketplaces_file(&mp_file, &state)?;
    Ok(())
}

pub fn update_marketplace(name: &str) -> Result<MarketplaceInfo> {
    let mp_file = paths::marketplaces_file().ok_or_else(|| anyhow!("no plugin home"))?;
    let mut state = load_marketplaces_file(&mp_file)?;
    let entry = state
        .marketplaces
        .get(name)
        .ok_or_else(|| anyhow!("marketplace `{}` not found", name))?
        .clone();
    let target = paths::marketplaces_root().unwrap().join(name);
    let out = Command::new("git")
        .args(["pull", "--ff-only"])
        .current_dir(&target)
        .output()
        .context("spawn git pull")?;
    if !out.status.success() {
        bail!("git pull failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    let commit = git_rev_parse(&target)?;
    let manifest = load_marketplace_manifest(&target)?;
    let (_mp_name, plugins) = resolve_marketplace_identity(&manifest, name);
    let plugins_list: Vec<String> = plugins.iter().map(|p| sanitize_name(&p.name)).collect();
    state.marketplaces.insert(
        name.to_string(),
        MarketplaceEntry {
            source: entry.source.clone(),
            added_at: entry.added_at.clone(),
            git_commit: commit.clone(),
            plugins: plugins_list.clone(),
        },
    );
    save_marketplaces_file(&mp_file, &state)?;
    Ok(MarketplaceInfo {
        name: name.to_string(),
        source: entry.source,
        git_commit: commit,
        plugins: plugins_list,
    })
}

pub fn list_marketplaces() -> Result<Vec<MarketplaceInfo>> {
    let mp_file = paths::marketplaces_file().ok_or_else(|| anyhow!("no plugin home"))?;
    let state = load_marketplaces_file(&mp_file)?;
    Ok(state
        .marketplaces
        .into_iter()
        .map(|(name, e)| MarketplaceInfo {
            name,
            source: e.source,
            git_commit: e.git_commit,
            plugins: e.plugins,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::test_support::isolated_home;
    use std::path::PathBuf;

    fn make_bare_repo_with_manifest(name: &str, manifest: Option<&str>) -> PathBuf {
        let work = tempfile::tempdir().unwrap().keep();
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
    fn add_marketplace_with_manifest() {
        let _home = isolated_home();
        let repo = make_bare_repo_with_manifest(
            "ascend-model-agent-plugin",
            Some(r#"{"name":"ascend-model-agent-plugin","plugins":[{"name":"ascend-model-agent-plugin","source":"./"}]}"#),
        );
        let url = format!("file://{}", repo.display());
        let info = add_marketplace(&url).unwrap();
        assert_eq!(info.name, "ascend-model-agent-plugin");
        assert_eq!(info.plugins, vec!["ascend-model-agent-plugin"]);
        assert!(!info.git_commit.is_empty());
    }

    #[test]
    #[serial_test::serial]
    fn add_marketplace_single_plugin_fallback() {
        let _home = isolated_home();
        let repo = make_bare_repo_with_manifest("solo-plugin", None);
        let url = format!("file://{}", repo.display());
        let info = add_marketplace(&url).unwrap();
        assert_eq!(info.name, "solo-plugin");
        assert_eq!(info.plugins, vec!["solo-plugin"]);
    }

    #[test]
    #[serial_test::serial]
    fn add_marketplace_rejects_duplicate() {
        let _home = isolated_home();
        let repo = make_bare_repo_with_manifest("dup", None);
        let url = format!("file://{}", repo.display());
        add_marketplace(&url).unwrap();
        let err = add_marketplace(&url).unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    #[serial_test::serial]
    fn remove_marketplace_works() {
        let _home = isolated_home();
        let repo = make_bare_repo_with_manifest("rm-mp", None);
        let url = format!("file://{}", repo.display());
        add_marketplace(&url).unwrap();
        remove_marketplace("rm-mp").unwrap();
        assert!(list_marketplaces().unwrap().is_empty());
    }

    #[test]
    #[serial_test::serial]
    fn list_marketplaces_returns_added() {
        let _home = isolated_home();
        let repo = make_bare_repo_with_manifest("list-mp", None);
        let url = format!("file://{}", repo.display());
        add_marketplace(&url).unwrap();
        let list = list_marketplaces().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "list-mp");
    }

    /// B1 regression: when manifest.name differs from the URL tail, the
    /// directory must be renamed so the registered key matches the on-disk
    /// path. Otherwise update_marketplace cannot find the working tree.
    #[test]
    #[serial_test::serial]
    fn add_marketplace_canonical_name_differs_from_url_tail() {
        let _home = isolated_home();
        // Repo on disk is "url-tail-name", but manifest declares
        // "canonical-name".
        let repo = make_bare_repo_with_manifest(
            "url-tail-name",
            Some(r#"{"name":"canonical-name","plugins":[{"name":"canonical-name","source":"./"}]}"#),
        );
        let url = format!("file://{}", repo.display());
        let info = add_marketplace(&url).unwrap();
        assert_eq!(info.name, "canonical-name");

        // Directory must be at marketplaces/canonical-name, not
        // marketplaces/url-tail-name. update_marketplace exercises this:
        // it computes the working directory from the registered key.
        let updated = update_marketplace("canonical-name").unwrap();
        assert_eq!(updated.name, "canonical-name");

        let mp_root = paths::marketplaces_root().unwrap();
        assert!(mp_root.join("canonical-name").exists());
        assert!(!mp_root.join("url-tail-name").exists());
    }

    /// B2 regression: a manifest whose `name` contains traversal or
    /// separators must be sanitized; the marketplace must land inside
    /// `marketplaces/`, not somewhere else on disk.
    #[test]
    #[serial_test::serial]
    fn add_marketplace_sanitizes_traversal_in_manifest_name() {
        let _home = isolated_home();
        let repo = make_bare_repo_with_manifest(
            "evil-source",
            Some(r#"{"name":"../evil","plugins":[{"name":"p","source":"./"}]}"#),
        );
        let url = format!("file://{}", repo.display());
        let info = add_marketplace(&url).unwrap();
        // "../evil" -> "---evil" after sanitize_name (3 specials become 3 dashes).
        assert_eq!(info.name, "---evil");

        let mp_root = paths::marketplaces_root().unwrap();
        assert!(mp_root.join("---evil").exists());
        // Crucially: nothing landed in the parent of mp_root.
        assert!(!mp_root.parent().unwrap().join("evil").exists());
    }
}
