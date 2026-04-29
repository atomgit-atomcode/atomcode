use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::manifest::{load_marketplace_manifest, MarketplaceManifest};
use super::paths;
use super::state::{
    load_marketplaces_file, save_marketplaces_file, MarketplaceEntry, MarketplacesFile,
};
use super::url::{infer_marketplace_name_from_url, validate_git_url};

/// Sanitize a name into a path-safe segment (CC convention).
fn sanitize_name(name: &str) -> String {
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
    let raw_name = infer_marketplace_name_from_url(url)?;
    let name = sanitize_name(&raw_name);

    let mp_root = paths::marketplaces_root().ok_or_else(|| anyhow!("no plugin home"))?;
    let target = mp_root.join(&name);

    // Idempotency: refuse to overwrite existing marketplace.
    let mp_file = paths::marketplaces_file().unwrap();
    let mut state = load_marketplaces_file(&mp_file)?;
    if state.marketplaces.contains_key(&name) {
        bail!("marketplace `{}` already exists; remove first", name);
    }
    if target.exists() {
        bail!(
            "directory {} already exists but is not registered; remove it manually",
            target.display()
        );
    }

    std::fs::create_dir_all(&mp_root).ok();
    git_clone(url, &target).with_context(|| format!("clone {}", url))?;
    let commit = git_rev_parse(&target)?;

    let manifest = load_marketplace_manifest(&target)?;
    let (mp_name, plugins) = resolve_marketplace_identity(&manifest, &name);
    let plugins_list = plugins.iter().map(|p| p.name.clone()).collect::<Vec<_>>();

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
                source: "./".into(),
                description: None,
            }],
        ),
    }
}

fn git_clone(url: &str, target: &Path) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Set up an isolated fake home, return its path. Caller must keep the
    /// `tempfile::TempDir` alive for the duration of the test.
    fn isolated_home() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", tmp.path());
        tmp
    }

    fn make_bare_repo_with_manifest(name: &str, manifest: Option<&str>) -> PathBuf {
        let work = tempfile::tempdir().unwrap().into_path();
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
}
