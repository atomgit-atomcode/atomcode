use anyhow::{anyhow, bail, Context, Result};

/// Error returned by [`install`] when the plugin is already present in
/// `installed_plugins.json`. Carries the canonical plugin id so the
/// caller can render a friendly reinstall hint.
#[derive(Debug)]
pub struct AlreadyInstalledError {
    pub id: String,
}

impl std::fmt::Display for AlreadyInstalledError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "plugin `{}` is already installed.\nPS: To reinstall, first run `/plugin uninstall {}` then `/plugin install {}`",
            self.id, self.id, self.id
        )
    }
}

impl std::error::Error for AlreadyInstalledError {}
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use super::manifest::{ExternalSource, GitPin, PluginEntry, PluginSource};
use super::marketplace::sanitize_name;
use super::paths;
use super::state::{
    load_installed_plugins_file, load_marketplaces_file, plugin_id, save_installed_plugins_file,
    InstallScope, InstalledPluginEntry,
};
use super::url::validate_git_url;

#[derive(Debug, Clone)]
pub struct InstalledPluginInfo {
    pub plugin: String,
    pub marketplace: String,
    pub plugin_dir: String,
    /// Installation scope.
    pub scope: InstallScope,
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
        Ok(format!("{}/{}", mp_root_rel, normalized.trim_end_matches('/')))
    }
}

/// Realize an external plugin source by cloning (url/git/github) or copying
/// (local) into `installed/<marketplace>/<plugin>/`. Returns the relative
/// `plugin_dir` to record in state.
fn install_external(
    plugin_key: &str,
    marketplace: &str,
    ext: &ExternalSource,
) -> Result<String> {
    let plugins_root = paths::plugins_root().ok_or_else(|| anyhow!("no plugin home"))?;
    let target_rel = format!("installed/{}/{}", marketplace, plugin_key);
    let target_abs = plugins_root.join(&target_rel);
    if target_abs.exists() {
        // The directory already exists. This can happen when a previous
        // install was cancelled (Esc) after the clone succeeded but before
        // the state file was updated, or when the install failed partway
        // through. If the plugin is NOT recorded in installed_plugins.json,
        // treat the directory as a stale leftover and remove it so the
        // install can proceed. Otherwise, bail out.
        let id = plugin_id(plugin_key, marketplace);
        let installed_path = paths::installed_plugins_file().unwrap();
        let is_registered = load_installed_plugins_file(&installed_path)
            .map(|f| f.plugins.contains_key(&id))
            .unwrap_or(false);
        if is_registered {
            bail!(
                "plugin install dir already exists and is registered: {}",
                target_abs.display()
            );
        }
        // Stale leftover — remove and continue.
        std::fs::remove_dir_all(&target_abs).with_context(|| {
            format!("failed to remove stale install dir {}", target_abs.display())
        })?;
    }
    if let Some(parent) = target_abs.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let git = super::marketplace::find_git()?;
    match ext {
        ExternalSource::Url { url, pin } | ExternalSource::Git { url, pin } => {
            validate_git_url(url)?;
            git_clone_with_pin(&git, url, &target_abs, pin)
                .with_context(|| format!("clone {}", url))?;
        }
        ExternalSource::Github { repo, pin } => {
            let url = expand_github_repo(repo)?;
            git_clone_with_pin(&git, &url, &target_abs, pin)
                .with_context(|| format!("clone {}", url))?;
        }
        ExternalSource::GitSubdir { url, path, pin } => {
            // The recorded plugin_dir points INTO the subtree, so return early
            // with the subdir-qualified path rather than the clone root.
            return git_subdir_clone(&git, url, path, pin, &target_abs)
                .map(|_| format!("{}/{}", target_rel, normalize_rel_subdir(path)))
                .with_context(|| format!("git-subdir clone {} ({})", url, path));
        }
        ExternalSource::Local { path } => {
            let src = expand_local_path(path)?;
            copy_dir_recursive(&src, &target_abs)
                .with_context(|| format!("copy {}", src.display()))?;
        }
    }
    Ok(target_rel)
}

/// Normalise a subdir path for joining into `plugin_dir`: strip a leading
/// `./` and any trailing slash. (Traversal safety is enforced separately by
/// `validate_plugin_source` before this is called.)
fn normalize_rel_subdir(path: &str) -> String {
    path.trim_start_matches("./").trim_end_matches('/').to_string()
}

/// Realise a `git-subdir` source: sparse + partial clone of just `path` from
/// `url` into `target`. `url` may be an `owner/repo` shorthand (expanded as a
/// GitHub repo) or a full git URL.
fn git_subdir_clone(git: &Path, url: &str, path: &str, pin: &GitPin, target: &Path) -> Result<()> {
    validate_plugin_source(path)?;
    let sub = normalize_rel_subdir(path);
    if sub.is_empty() {
        bail!("git-subdir source has empty path");
    }
    let clone_url = resolve_subdir_url(url)?;

    // Partial clone (no blobs) + no checkout, so we can scope the working tree
    // to just `sub` before materialising any files.
    // Hardened git (no interactive tty prompt) — a private remote must fail
    // fast, not deadlock the TUI. See `marketplace::git_command`.
    // git-subdir pins are branch names in practice (the schema's `ref`); a
    // commit `sha` would need full history, but the catalog carries none.
    let branch = pin.git_ref.as_deref().or(pin.branch.as_deref()).map(String::from);
    let build_partial = |cmd: &mut Command| {
        cmd.args(["clone", "--filter=blob:none", "--no-checkout", "--depth", "1"]);
        if let Some(b) = &branch {
            cmd.args(["--branch", b]);
        }
        cmd.arg(clone_url.as_str()).arg(target);
    };
    // Partial clone (no blobs). Old gits lack --filter; fall back to plain.
    if super::marketplace::clone_with_optional_auth(git, clone_url.as_str(), target, build_partial)
        .is_err()
    {
        if target.exists() {
            std::fs::remove_dir_all(target).ok();
        }
        let build_plain = |cmd: &mut Command| {
            cmd.args(["clone", "--no-checkout", "--depth", "1"]);
            if let Some(b) = &branch {
                cmd.args(["--branch", b]);
            }
            cmd.arg(clone_url.as_str()).arg(target);
        };
        super::marketplace::clone_with_optional_auth(git, clone_url.as_str(), target, build_plain)
            .context("git-subdir clone")?;
    }

    // Scope the working tree to the subdir, then check it out. `--no-cone` +
    // `--` keep an exotic path from being read as a cone pattern or a flag.
    let sparse = Command::new(git)
        .args(["sparse-checkout", "set", "--no-cone", "--", &sub])
        .current_dir(target)
        .output()
        .context("spawn git sparse-checkout")?;
    if !sparse.status.success() {
        bail!(
            "git sparse-checkout failed: {}",
            String::from_utf8_lossy(&sparse.stderr)
        );
    }
    let checkout = Command::new(git)
        .args(["checkout"])
        .current_dir(target)
        .output()
        .context("spawn git checkout (git-subdir)")?;
    if !checkout.status.success() {
        bail!(
            "git checkout failed: {}",
            String::from_utf8_lossy(&checkout.stderr)
        );
    }

    // The subdir must actually exist in the repo, or this plugin is empty.
    let materialised = target.join(&sub);
    if !materialised.is_dir() {
        bail!(
            "git-subdir path `{}` not found in repo {}",
            sub,
            clone_url
        );
    }
    Ok(())
}

/// Resolve a git-subdir `url` field. `owner/repo` shorthand → GitHub https URL
/// (reusing the existing anti-injection guard); anything with a scheme or an
/// ssh host → validated as a full git URL.
fn resolve_subdir_url(url: &str) -> Result<String> {
    let looks_shorthand = !url.contains("://")
        && !url.contains('@')
        && url.matches('/').count() == 1
        && !url.starts_with('/');
    if looks_shorthand {
        expand_github_repo(url)
    } else {
        validate_git_url(url)?;
        Ok(url.to_string())
    }
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

fn git_clone_with_pin(git: &Path, url: &str, target: &Path, pin: &GitPin) -> Result<()> {
    let needs_full_history =
        pin.commit.is_some() || pin.tag.is_some() || pin.git_ref.is_some();
    let branch = pin.branch.clone();
    super::marketplace::clone_with_optional_auth(git, url, target, |cmd| {
        cmd.arg("clone");
        if !needs_full_history {
            cmd.args(["--depth", "1"]);
        }
        if let Some(b) = &branch {
            cmd.args(["--branch", b]);
        }
        cmd.arg(url).arg(target);
    })?;

    // Apply commit/tag/ref pin via post-clone checkout.
    let pin_ref = pin
        .commit
        .as_deref()
        .or(pin.tag.as_deref())
        .or(pin.git_ref.as_deref());
    if let Some(rev) = pin_ref {
        let out = Command::new(git)
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
    u.trim().trim_end_matches('/').trim_end_matches(".git").to_string()
}

/// Decide whether an external source points at the same repo as the
/// marketplace's own clone URL. Returns false whenever a `GitPin` is set,
/// since the marketplace working tree is on its default branch and may not
/// match the requested revision.
fn external_matches_marketplace(ext: &ExternalSource, mp_url: &str) -> bool {
    let (url, pin) = match ext {
        ExternalSource::Url { url, pin } | ExternalSource::Git { url, pin } => {
            (url.clone(), pin)
        }
        ExternalSource::Github { repo, pin } => match expand_github_repo(repo) {
            Ok(u) => (u, pin),
            Err(_) => return false,
        },
        // A git-subdir install pulls only a subtree into its own dir; it must
        // never be deduped against the marketplace's full clone.
        ExternalSource::GitSubdir { .. } => return false,
        ExternalSource::Local { .. } => return false,
    };
    if pin.branch.is_some()
        || pin.tag.is_some()
        || pin.commit.is_some()
        || pin.git_ref.is_some()
    {
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
                    bail!("plugin source path '{}' contains disallowed components", source);
                }
            }
            Component::CurDir => {
                // "./" is fine; skip.
            }
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("plugin source path '{}' contains disallowed components", source);
            }
        }
    }
    Ok(())
}

/// Result of resolving a bare plugin name across all registered marketplaces.
#[derive(Debug, Clone)]
pub struct PluginMarketplaceMatch {
    pub marketplace: String,
    pub plugin: String,
}

/// Find all marketplaces that contain a plugin matching the given name.
/// The name is compared against both the raw plugin name and its sanitized
/// form (e.g. "my plugin" matches "my-plugin" in the marketplace's plugin
/// list). Returns an empty Vec if the plugin is not found in any marketplace.
pub fn resolve_plugin_marketplace(plugin_name: &str) -> Result<Vec<PluginMarketplaceMatch>> {
    let mp_state = load_marketplaces_file(&paths::marketplaces_file().unwrap())?;
    let sanitized = sanitize_name(plugin_name);
    let mut matches: Vec<PluginMarketplaceMatch> = Vec::new();

    for (mp_name, entry) in &mp_state.marketplaces {
        for p in &entry.plugins {
            if p == plugin_name || p == &sanitized {
                matches.push(PluginMarketplaceMatch {
                    marketplace: mp_name.clone(),
                    plugin: p.clone(),
                });
                break; // one match per marketplace is enough
            }
        }
    }

    Ok(matches)
}

/// Install a plugin from a given marketplace with the specified scope.
///
/// For `User` scope, the plugin is installed under the global
/// `~/.atomcode/plugins/` root (the original behaviour). For `Project`
/// and `Local` scopes, the plugin files are copied into the project's
/// `.atomcode/plugins/` or `.atomcode/plugins/local/` directory so
/// they are visible only within that project.
pub fn install(plugin: &str, marketplace: &str, scope: InstallScope) -> Result<InstalledPluginInfo> {
    let mp_state = load_marketplaces_file(&paths::marketplaces_file().unwrap())?;
    let entry = mp_state
        .marketplaces
        .get(marketplace)
        .ok_or_else(|| anyhow!("marketplace `{}` not registered", marketplace))?;
    if !entry.plugins.iter().any(|p| p == plugin) {
        bail!("plugin `{}` not found in marketplace `{}`", plugin, marketplace);
    }

    // Resolve plugin source dir relative to marketplace root.
    let mp_root_rel = format!("marketplaces/{}", marketplace);
    let mp_root_abs = paths::plugins_root().unwrap().join(&mp_root_rel);
    if !mp_root_abs.exists() {
        bail!(
            "marketplace `{}` clone is missing — run `/plugin update {marketplace}` to restore it",
            marketplace
        );
    }
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
        PluginSource::Unknown(raw) => {
            bail!(
                "plugin `{}` uses an unsupported source type and cannot be \
                 installed by this build: {}",
                plugin,
                raw
            );
        }
    };

    // Determine the target directory and state file based on scope.
    match &scope {
        InstallScope::User => {
            // Original global install path.
            let id = plugin_id(&plugin_key, marketplace);
            let installed_path = paths::installed_plugins_file().unwrap();
            let mut installed = load_installed_plugins_file(&installed_path)?;
            if installed.plugins.contains_key(&id) {
                let dir_missing = installed
                    .plugins
                    .get(&id)
                    .map(|e| {
                        let abs = paths::plugins_root().unwrap().join(&e.plugin_dir);
                        !abs.exists()
                    })
                    .unwrap_or(false);
                if !dir_missing {
                    if plugin_dir_rel.starts_with("installed/") {
                        let abs = paths::plugins_root().unwrap().join(&plugin_dir_rel);
                        std::fs::remove_dir_all(&abs).ok();
                    }
                    return Err(AlreadyInstalledError { id: id.clone() }.into());
                }
                installed.plugins.remove(&id);
            }
            installed.plugins.insert(
                id.clone(),
                InstalledPluginEntry {
                    marketplace: marketplace.to_string(),
                    plugin: plugin_key.clone(),
                    plugin_dir: plugin_dir_rel.clone(),
                    installed_at: chrono::Utc::now().to_rfc3339(),
                    scope: InstallScope::User,
                },
            );
            save_installed_plugins_file(&installed_path, &installed)?;

            Ok(InstalledPluginInfo {
                plugin: plugin_key,
                marketplace: marketplace.to_string(),
                plugin_dir: plugin_dir_rel,
                scope: InstallScope::User,
            })
        }
        InstallScope::Project | InstallScope::Local => {
            // Project-level install: copy the plugin files into the project's
            // .atomcode/plugins/ directory and record in the project-level
            // installed_plugins.json.
            let working_dir = std::env::current_dir()
                .context("cannot determine current working directory")?;
            let project_root = paths::project_plugins_root(&working_dir, &scope)
                .ok_or_else(|| anyhow!("no project plugins root for scope {:?}", scope))?;

            // Source: the actual plugin files on disk (resolved from the global
            // plugins root since marketplaces always live there).
            let source_abs = paths::plugins_root().unwrap().join(&plugin_dir_rel);
            if !source_abs.exists() {
                bail!("plugin source directory does not exist: {}", source_abs.display());
            }

            // Destination: project-level plugins dir.
            let dest_rel = format!("installed/{}/{}", marketplace, plugin_key);
            let dest_abs = project_root.join(&dest_rel);
            if dest_abs.exists() {
                // The directory already exists — same residual-detect logic
                // as install_external: if the plugin is NOT recorded in the
                // project-level installed_plugins.json, treat the directory
                // as a stale leftover from a cancelled / failed install and
                // remove it.  Otherwise, bail out.
                let state_path = paths::project_installed_plugins_file(&working_dir, &scope)
                    .ok_or_else(|| anyhow!("no project state file for scope {:?}", scope))?;
                let id = plugin_id(&plugin_key, marketplace);
                let is_registered = load_installed_plugins_file(&state_path)
                    .map(|f| f.plugins.contains_key(&id))
                    .unwrap_or(false);
                if is_registered {
                    bail!(
                        "plugin already installed in project at {}",
                        dest_abs.display()
                    );
                }
                // Stale leftover — remove and continue.
                std::fs::remove_dir_all(&dest_abs).with_context(|| {
                    format!(
                        "failed to remove stale project install dir {}",
                        dest_abs.display()
                    )
                })?;
            }
            if let Some(parent) = dest_abs.parent() {
                std::fs::create_dir_all(parent).ok();
            }

            // Copy the plugin files into the project directory.
            copy_dir_recursive(&source_abs, &dest_abs)
                .with_context(|| format!("copy plugin to project dir {}", dest_abs.display()))?;

            // Record in project-level installed_plugins.json.
            let state_path = paths::project_installed_plugins_file(&working_dir, &scope)
                .ok_or_else(|| anyhow!("no project state file for scope {:?}", scope))?;
            let mut state = load_installed_plugins_file(&state_path)?;
            let id = plugin_id(&plugin_key, marketplace);
            if state.plugins.contains_key(&id) {
                // Clean up the copy we just made.
                std::fs::remove_dir_all(&dest_abs).ok();
                bail!("plugin `{}` already installed in project scope {}", id, scope);
            }
            state.plugins.insert(
                id.clone(),
                InstalledPluginEntry {
                    marketplace: marketplace.to_string(),
                    plugin: plugin_key.clone(),
                    plugin_dir: dest_rel.clone(),
                    installed_at: chrono::Utc::now().to_rfc3339(),
                    scope: scope.clone(),
                },
            );
            save_installed_plugins_file(&state_path, &state)?;

            Ok(InstalledPluginInfo {
                plugin: plugin_key,
                marketplace: marketplace.to_string(),
                plugin_dir: dest_rel,
                scope: scope.clone(),
            })
        }
    }
}

/// Ensure a plugin from a marketplace is fully installed (user scope),
/// automatically recovering from missing marketplace registrations or
/// deleted clones. This is the single entry-point for `/guide` auto-install
/// and similar "make this work no matter what" flows.
pub fn ensure_plugin_installed(
    plugin: &str,
    marketplace: &str,
    marketplace_url: &str,
) -> Result<InstalledPluginInfo> {
    // Step 1: Ensure marketplace is registered.
    let mp_file = paths::marketplaces_file().unwrap();
    let mp_state = load_marketplaces_file(&mp_file)?;
    let needs_add = !mp_state.marketplaces.contains_key(marketplace);
    drop(mp_state);

    if needs_add {
        match super::marketplace::add_marketplace(marketplace_url) {
            Ok(_) => {}
            Err(e) => {
                let msg = e.to_string();
                if !msg.contains("already exists") {
                    return Err(e);
                }
            }
        }
    }

    // Step 2: Ensure the marketplace clone exists on disk.
    let mp_root = paths::plugins_root()
        .unwrap()
        .join("marketplaces")
        .join(marketplace);
    if !mp_root.exists() {
        let mp_state = load_marketplaces_file(&mp_file)?;
        let entry = mp_state
            .marketplaces
            .get(marketplace)
            .ok_or_else(|| anyhow!("marketplace `{}` not registered", marketplace))?;
        let url = entry.source.clone();
        drop(mp_state);

        super::marketplace::git_clone(&url, &mp_root)
            .with_context(|| format!("re-clone marketplace `{}`", marketplace))?;
    }

    // Step 3: Install the plugin (user scope by default for auto-install).
    install(plugin, marketplace, InstallScope::User)
}

/// Uninstall a plugin. For User scope, removes from the global plugins
/// root. For Project/Local scope, removes from the project's .atomcode/plugins/
/// directory.
pub fn uninstall(plugin: &str, marketplace: &str, scope: InstallScope) -> Result<()> {
    let plugin_key = sanitize_name(plugin);
    let id = plugin_id(&plugin_key, marketplace);

    match &scope {
        InstallScope::User => {
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
                    let install_root_rel = format!("installed/{}/{}", entry.marketplace, entry.plugin);
                    let abs = root.join(&install_root_rel);
                    if abs.exists() {
                        std::fs::remove_dir_all(&abs).ok();
                    }
                }
            }
        }
        InstallScope::Project | InstallScope::Local => {
            let working_dir = std::env::current_dir()
                .context("cannot determine current working directory")?;
            let state_path = paths::project_installed_plugins_file(&working_dir, &scope)
                .ok_or_else(|| anyhow!("no project state file for scope {:?}", scope))?;
            let mut state = load_installed_plugins_file(&state_path)?;
            let entry = state
                .plugins
                .remove(&id)
                .ok_or_else(|| anyhow!("plugin `{}` not installed in project scope {}", id, scope))?;
            save_installed_plugins_file(&state_path, &state)?;

            // Remove the copied plugin files.
            let project_root = paths::project_plugins_root(&working_dir, &scope)
                .ok_or_else(|| anyhow!("no project plugins root for scope {:?}", scope))?;
            if entry.plugin_dir.starts_with("installed/") {
                let install_root_rel = format!("installed/{}/{}", entry.marketplace, entry.plugin);
                let abs = project_root.join(&install_root_rel);
                if abs.exists() {
                    std::fs::remove_dir_all(&abs).ok();
                }
            }
        }
    }
    Ok(())
}

/// List all installed plugins across all scopes.
///
/// Returns plugins from the global (user) scope plus any project-level
/// plugins found in the current working directory.
pub fn list_installed() -> Result<Vec<InstalledPluginInfo>> {
    let mut result = Vec::new();

    // User scope (global).
    let installed = load_installed_plugins_file(&paths::installed_plugins_file().unwrap())?;
    for e in installed.plugins.into_values() {
        result.push(InstalledPluginInfo {
            plugin: e.plugin,
            marketplace: e.marketplace,
            plugin_dir: e.plugin_dir,
            scope: e.scope,
        });
    }

    // Project and Local scopes.
    let working_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    for scope in [InstallScope::Project, InstallScope::Local] {
        if let Some(state_path) = paths::project_installed_plugins_file(&working_dir, &scope) {
            if state_path.exists() {
                if let Ok(state) = load_installed_plugins_file(&state_path) {
                    for e in state.plugins.into_values() {
                        result.push(InstalledPluginInfo {
                            plugin: e.plugin,
                            marketplace: e.marketplace,
                            plugin_dir: e.plugin_dir,
                            scope: e.scope,
                        });
                    }
                }
            }
        }
    }

    Ok(result)
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
    fn install_single_plugin_fallback() {
        let _home = isolated_home();
        let repo = make_repo("solo", None);
        add_marketplace(&format!("file://{}", repo.display())).unwrap();
        let info = install("solo", "solo", InstallScope::User).unwrap();
        assert_eq!(info.plugin_dir, "marketplaces/solo");
    }

    #[test]
    #[serial_test::serial]
    fn install_rejects_duplicate() {
        let _home = isolated_home();
        let repo = make_repo("dup", None);
        add_marketplace(&format!("file://{}", repo.display())).unwrap();
        install("dup", "dup", InstallScope::User).unwrap();
        assert!(install("dup", "dup", InstallScope::User).is_err());
    }

    #[test]
    #[serial_test::serial]
    fn uninstall_works() {
        let _home = isolated_home();
        let repo = make_repo("u", None);
        add_marketplace(&format!("file://{}", repo.display())).unwrap();
        install("u", "u", InstallScope::User).unwrap();
        uninstall("u", "u", InstallScope::User).unwrap();
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
        Command::new("git").args(["add", "-A"]).current_dir(&repo).status().unwrap();
        Command::new("git").args(["commit", "-q", "-m", "add sub"]).current_dir(&repo).status().unwrap();
        add_marketplace(&format!("file://{}", repo.display())).unwrap();
        let info = install("sub", "mp", InstallScope::User).unwrap();
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
        let err = install("esc", "mp2", InstallScope::User).unwrap_err();
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
        Command::new("git").args(["add", "-A"]).current_dir(&plugin_repo).status().unwrap();
        Command::new("git").args(["commit", "-q", "-m", "marker"]).current_dir(&plugin_repo).status().unwrap();

        // Marketplace repo whose manifest references the plugin repo by URL.
        let plugin_url = format!("file://{}", plugin_repo.display());
        let manifest = format!(
            r#"{{"name":"mp_ext","plugins":[{{"name":"ext","source":{{"source":"url","url":"{}"}}}}]}}"#,
            plugin_url
        );
        let mp_repo = make_repo("mp_ext", Some(&manifest));
        add_marketplace(&format!("file://{}", mp_repo.display())).unwrap();

        let info = install("ext", "mp_ext", InstallScope::User).unwrap();
        assert_eq!(info.plugin_dir, "installed/mp_ext/ext");

        let abs = paths::plugins_root().unwrap().join(&info.plugin_dir);
        assert!(abs.join("PLUGIN_MARKER").exists(), "external clone missing");

        // uninstall must wipe the installed/* dir.
        uninstall("ext", "mp_ext", InstallScope::User).unwrap();
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
        let info = install("loc", "mp_local", InstallScope::User).unwrap();

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
        Command::new("git").args(["init", "-q"]).current_dir(&repo).status().unwrap();
        Command::new("git").args(["config", "user.email", "t@t"]).current_dir(&repo).status().unwrap();
        Command::new("git").args(["config", "user.name", "t"]).current_dir(&repo).status().unwrap();
        std::fs::create_dir_all(repo.join(".atomcode-plugin")).unwrap();
        let url = format!("file://{}", repo.display());
        let manifest = format!(
            r#"{{"name":"self_ref","plugins":[{{"name":"self_ref","source":{{"source":"url","url":"{}"}}}}]}}"#,
            url
        );
        std::fs::write(repo.join(".atomcode-plugin/marketplace.json"), manifest).unwrap();
        std::fs::write(repo.join("README"), "x").unwrap();
        Command::new("git").args(["add", "-A"]).current_dir(&repo).status().unwrap();
        Command::new("git").args(["commit", "-q", "-m", "init"]).current_dir(&repo).status().unwrap();

        add_marketplace(&url).unwrap();
        let info = install("self_ref", "self_ref", InstallScope::User).unwrap();

        // Dedup must land in marketplaces/, not installed/.
        assert_eq!(info.plugin_dir, "marketplaces/self_ref");
        let installed_root = paths::plugins_root().unwrap().join("installed");
        assert!(
            !installed_root.exists() || std::fs::read_dir(&installed_root).unwrap().next().is_none(),
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
        let ext = ExternalSource::Url { url: url.into(), pin };
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
        let ext = ExternalSource::Local { path: "/tmp/x".into() };
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

    // ---- git-subdir source ----

    #[test]
    fn resolve_subdir_url_shorthand_and_full() {
        // owner/repo shorthand → GitHub https
        assert_eq!(
            resolve_subdir_url("openclaw/openclaw").unwrap(),
            "https://github.com/openclaw/openclaw.git"
        );
        // full url passes through
        assert_eq!(
            resolve_subdir_url("https://example.com/r.git").unwrap(),
            "https://example.com/r.git"
        );
        // ssh form is not shorthand (has '@'), validated as a url
        assert!(resolve_subdir_url("git@github.com:o/r.git").is_ok());
        // three segments is neither valid shorthand nor (here) a url scheme
        assert!(resolve_subdir_url("a/b/c").is_err());
    }

    #[test]
    fn normalize_rel_subdir_strips_dot_and_slash() {
        assert_eq!(normalize_rel_subdir("./a/b/"), "a/b");
        assert_eq!(normalize_rel_subdir("a/b"), "a/b");
        assert_eq!(normalize_rel_subdir(".agents/skills/x"), ".agents/skills/x");
    }

    /// Build an "upstream" repo containing a plugin in a subdirectory.
    /// Returns (repo_path, current_branch_name) so the test can pin `ref`
    /// without depending on the machine's git `init.defaultBranch`.
    fn make_subdir_upstream(subdir: &str) -> (PathBuf, String) {
        let work = tempfile::tempdir().unwrap().keep();
        let repo = work.join("upstream");
        std::fs::create_dir_all(&repo).unwrap();
        Command::new("git").args(["init", "-q"]).current_dir(&repo).status().unwrap();
        Command::new("git").args(["config", "user.email", "t@t"]).current_dir(&repo).status().unwrap();
        Command::new("git").args(["config", "user.name", "t"]).current_dir(&repo).status().unwrap();
        let sub = repo.join(subdir);
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("plugin.json"), r#"{"name":"sk"}"#).unwrap();
        std::fs::create_dir_all(sub.join("skills/sk")).unwrap();
        std::fs::write(
            sub.join("skills/sk/SKILL.md"),
            "---\nname: sk\ndescription: d\n---\nbody",
        )
        .unwrap();
        // A file OUTSIDE the subdir, to prove sparse-checkout scopes correctly.
        std::fs::write(repo.join("OUTSIDE_MARKER"), "x").unwrap();
        Command::new("git").args(["add", "-A"]).current_dir(&repo).status().unwrap();
        Command::new("git").args(["commit", "-q", "-m", "init"]).current_dir(&repo).status().unwrap();
        let out = Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(&repo)
            .output()
            .unwrap();
        let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (repo, branch)
    }

    /// End-to-end: a marketplace whose manifest has a `git-subdir` entry
    /// pointing at a separate upstream repo's subdirectory. Install must
    /// sparse-clone only that subtree and record a plugin_dir INTO the subdir.
    #[test]
    #[serial_test::serial]
    fn install_git_subdir_clones_only_subtree() {
        // git-subdir installs use `git sparse-checkout` (git >= 2.25). Skip where the
        // git binary predates it so the suite stays green on old git instead of
        // failing inside the unwrap below.
        let sparse_ok = std::process::Command::new("git")
            .args(["sparse-checkout", "-h"])
            .output()
            .map(|o| !String::from_utf8_lossy(&o.stderr).contains("is not a git command"))
            .unwrap_or(false);
        if !sparse_ok {
            eprintln!(
                "skip install_git_subdir_clones_only_subtree: git lacks sparse-checkout (need git >= 2.25)"
            );
            return;
        }
        let _home = isolated_home();
        let (upstream, branch) = make_subdir_upstream("pkg/tool");
        let upstream_url = format!("file://{}", upstream.display());

        let manifest = format!(
            r#"{{"name":"mp_gs","plugins":[{{"name":"tool","source":{{"source":"git-subdir","url":"{}","path":"pkg/tool","ref":"{}"}}}}]}}"#,
            upstream_url, branch
        );
        let mp_repo = make_repo("mp_gs", Some(&manifest));
        add_marketplace(&format!("file://{}", mp_repo.display())).unwrap();

        let info = install("tool", "mp_gs", InstallScope::User).unwrap();
        // plugin_dir points into the subdir.
        assert_eq!(info.plugin_dir, "installed/mp_gs/tool/pkg/tool");

        let abs = paths::plugins_root().unwrap().join(&info.plugin_dir);
        assert!(abs.join("plugin.json").exists(), "subdir content missing");
        assert!(abs.join("skills/sk/SKILL.md").exists(), "subdir skill missing");

        // sparse-checkout must NOT have materialised files outside the subdir.
        let clone_root = paths::plugins_root().unwrap().join("installed/mp_gs/tool");
        assert!(
            !clone_root.join("OUTSIDE_MARKER").exists(),
            "sparse-checkout leaked files outside the requested subdir"
        );

        // uninstall removes the whole clone.
        uninstall("tool", "mp_gs", InstallScope::User).unwrap();
        assert!(!clone_root.exists(), "uninstall should remove the clone");
    }

    /// A git-subdir entry whose `path` escapes the repo must be rejected.
    #[test]
    #[serial_test::serial]
    fn install_git_subdir_rejects_path_traversal() {
        let _home = isolated_home();
        let manifest = r#"{"name":"mp_bad","plugins":[{"name":"esc","source":{"source":"git-subdir","url":"o/r","path":"../etc","ref":"main"}}]}"#;
        let mp_repo = make_repo("mp_bad", Some(manifest));
        add_marketplace(&format!("file://{}", mp_repo.display())).unwrap();
        let err = install("esc", "mp_bad", InstallScope::User).unwrap_err();
        assert!(
            err.to_string().contains("disallowed components")
                || err.to_string().contains("git-subdir"),
            "expected traversal rejection, got: {err}"
        );
    }

    #[test]
    fn dedup_skipped_for_git_subdir_source() {
        let ext = ExternalSource::GitSubdir {
            url: "o/r".into(),
            path: "sub".into(),
            pin: GitPin::default(),
        };
        assert!(!external_matches_marketplace(&ext, "https://github.com/o/r.git"));
    }
}
