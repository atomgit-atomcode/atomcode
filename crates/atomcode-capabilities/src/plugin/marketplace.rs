use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::manifest::{load_marketplace_manifest, MarketplaceManifest, PluginSource};
use super::paths;
use super::state::{load_marketplaces_file, save_marketplaces_file, MarketplaceEntry};
use super::url::{infer_marketplace_name_from_url, validate_git_url};

/// Locate the `git` executable on the system.
///
/// Tries the default PATH resolution first, then falls back to a set of
/// well-known installation paths that may not appear in the PATH of a
/// GUI-launched process (macOS apps launched from Dock/Launchpad do not
/// inherit the user's shell profile, so Homebrew/Xcode git is often
/// invisible to them).
///
/// Returns the resolved path on success, or an error with a friendly
/// message explaining that git is required.
pub fn find_git() -> Result<PathBuf> {
    // Test hook: set ATOMCODE_GIT_PATH to a non-existent path to simulate
    // git being unavailable (useful for QA without needing to uninstall git).
    //   ATOMCODE_GIT_PATH=/dev/null atomcode
    if let Ok(custom) = std::env::var("ATOMCODE_GIT_PATH") {
        let p = PathBuf::from(&custom);
        if p.exists() {
            return Ok(p);
        }
        bail!(
            "git is not installed or not on PATH. \
             AtomCode requires git to manage plugin marketplaces. \
             Please install git (e.g. `xcode-select --install` on macOS, \
             `sudo apt install git` on Ubuntu) and restart AtomCode."
        );
    }

    // 1. Default PATH resolution — works on most Linux systems and in
    //    terminal-launched sessions where git is already on PATH.
    if let Ok(path) = which_git("git") {
        return Ok(path);
    }

    // 2. Fallback: probe common installation directories that are
    //    frequently missing from the PATH of GUI-launched processes.
    //
    //    MUST verify the candidate actually RUNS, not merely that the file
    //    exists: on macOS WITHOUT the Xcode Command Line Tools installed,
    //    `/usr/bin/git` is present as a `xcode-select` STUB that fails on
    //    every invocation ("No developer tools were found, requesting
    //    install"). An `exists()`-only check treated that stub as a working
    //    git, so the early GitNotFound guard was skipped and the later
    //    `git clone` dumped a red, crash-looking error on fresh installs.
    //    Probing `--version` rejects the stub → callers fall through to the
    //    friendly "install git (`xcode-select --install`)" hint.
    let candidates: &[&str] = &[
        "/usr/bin/git",          // macOS system (Xcode CLI tools)
        "/usr/local/bin/git",    // macOS Intel Homebrew
        "/opt/homebrew/bin/git", // macOS Apple Silicon Homebrew
        "/opt/local/bin/git",    // MacPorts
        "/snap/bin/git",         // Ubuntu snap
        "/app/bin/git",          // NixOS / some containers
    ];
    for candidate in candidates {
        let p = PathBuf::from(candidate);
        if p.exists() && git_runs(&p) {
            return Ok(p);
        }
    }

    bail!(
        "git is not installed or not on PATH. \
         AtomCode requires git to manage plugin marketplaces. \
         Please install git (e.g. `xcode-select --install` on macOS, \
         `sudo apt install git` on Ubuntu) and restart AtomCode."
    )
}

/// Simple `which`-like resolution for an executable name.
fn which_git(name: &str) -> Result<PathBuf> {
    let out = Command::new(name)
        .arg("--version")
        .output()
        .context("spawn git")?;
    if !out.status.success() {
        bail!("`{} --version` exited with error", name);
    }
    // The command ran, so the executable is reachable.  On Unix we can
    // ask `which` for the absolute path; on failure we fall back to the
    // bare name (which still works for Command::new).
    #[cfg(unix)]
    {
        let which_out = Command::new("which").arg(name).output().ok();
        if let Some(w) = which_out {
            if w.status.success() {
                let path = String::from_utf8_lossy(&w.stdout).trim().to_string();
                if !path.is_empty() {
                    return Ok(PathBuf::from(path));
                }
            }
        }
    }
    Ok(PathBuf::from(name))
}

/// Does the git binary at `path` actually run? Runs `<path> --version` and
/// checks it exits successfully. Distinguishes a working git from a present-
/// but-nonfunctional stub (notably macOS's `/usr/bin/git` xcode-select shim
/// before the Command Line Tools are installed, which exists on disk but exits
/// non-zero on every invocation). Any spawn/exec error counts as "does not run".
fn git_runs(path: &Path) -> bool {
    Command::new(path)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Sanitize a name into a path-safe segment (CC convention).
pub fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
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
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
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
        bail!(
            "marketplace name `{}` sanitized to empty string",
            raw_mp_name
        );
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
        return Err(anyhow!(
            "rename {} -> {}: {}",
            tmp_dir.display(),
            target.display(),
            e
        ));
    }

    let plugins_list = plugins
        .iter()
        .map(|p| sanitize_name(&p.name))
        .collect::<Vec<_>>();

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
                version: None,
            }],
        ),
    }
}

/// Build a `git` Command hardened for headless / TUI use: never prompt on the
/// controlling terminal.
///
/// AtomCode runs marketplace `git clone`/`pull` from inside the TUI, which
/// holds the terminal in raw mode. For a private HTTPS remote, git would
/// otherwise open `/dev/tty` directly and block on `Username for 'https://…':`
/// — the same tty the TUI is reading, so keystrokes never reach git and the
/// whole UI deadlocks (even Ctrl-C barely works). `GIT_TERMINAL_PROMPT=0` makes
/// git fail fast with a clear error instead; the GCM / SSH-BatchMode guards do
/// the same for those transports, and stdin is closed as defense in depth.
/// A real credential helper or stored PAT still authenticates non-interactively
/// — only the blocking interactive fallback is disabled.
pub(super) fn git_command(git: &Path) -> Command {
    let mut cmd = Command::new(git);
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .env("GIT_SSH_COMMAND", "ssh -oBatchMode=yes")
        .stdin(Stdio::null());
    cmd
}

/// Build the `Authorization: Basic` header value from credentials. Pure
/// (creds passed in) so it's unit-testable without touching auth.toml.
fn basic_auth_header(username: &str, token: &str) -> String {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", username, token));
    format!("Authorization: Basic {}", b64)
}

/// `-c` value scoping the auth header to this repo's host only, so git won't
/// send it on a cross-host redirect. git matches `http.<base-url>.*` by URL
/// prefix; we scope to `scheme://host` (no path). Falls back to the full URL
/// if it can't be parsed (caller only reaches here for trusted hosts).
fn extra_header_config(url: &str, header: &str) -> String {
    let base = super::url::scheme_host_prefix(url).unwrap_or_else(|| url.to_string());
    format!("http.{}.extraHeader={}", base, header)
}

/// Fetch (username, fresh access token) from the stored login, refreshing the
/// token if expired. None when not logged in or refresh fails.
fn live_credentials() -> Option<(String, String)> {
    let auth = atomcode_auth::get_stored_auth()?;
    let token = atomcode_auth::get_valid_token().ok()?;
    if token.is_empty() {
        return None;
    }
    Some((auth.user.username, token))
}

/// If `url` is a trusted-host repo and we're logged in, return the
/// `["-c", "http.<host>.extraHeader=Authorization: Basic ..."]` args to inject
/// on an authenticated clone/pull retry. Centralizes host-gate + cred fetch +
/// per-URL scoping. None → caller must NOT inject the token.
pub(super) fn auth_retry_args(url: &str) -> Option<[String; 2]> {
    if !super::url::host_is_trusted(url) {
        return None;
    }
    let (username, token) = live_credentials()?;
    let header = basic_auth_header(&username, &token);
    Some(["-c".to_string(), extra_header_config(url, &header)])
}

/// Run `git clone …` built by `add_args`, anonymously first. If it fails with
/// an auth error on a trusted-host repo while logged in, wipe the partial
/// `target` and retry once with the per-URL auth header injected. `add_args`
/// must append the full `clone … <url> <target>` arguments and is called fresh
/// per attempt. `target` is passed so the failed-attempt dir can be removed
/// before the authenticated retry (git refuses to clone into a non-empty dir).
/// User-facing error when an auth failure could NOT be resolved automatically
/// (no credential was injected). Tailors the hint to the cause so a logged-in
/// user with a dead token isn't told "just log in" as if they hadn't:
/// - untrusted host → SSH / configure git creds (the platform token is never
///   sent to non-allowlisted hosts, so /login wouldn't help);
/// - trusted host + a stored login present (token expired AND refresh failed) →
///   re-login;
/// - trusted host + not logged in → /login (auto-creds) or SSH.
/// `verb` is 克隆 / 更新. Shared by clone + pull so the wording can't drift.
fn auth_required_message(verb: &str, url: &str, stderr: &str) -> String {
    let stderr = stderr.trim();
    if !super::url::host_is_trusted(url) {
        return format!(
            "{verb}失败：该仓库需要认证（私有仓库）。请改用 SSH 地址（git@…）\
             或先用 git 配置好凭证后重试。\n原始错误：{stderr}"
        );
    }
    if atomcode_auth::get_stored_auth().is_some() {
        format!(
            "{verb}失败：登录已过期或凭证无效，请运行 /login 重新登录后重试。\n原始错误：{stderr}"
        )
    } else {
        format!(
            "{verb}失败：该私有仓库需要认证。请先 /login 登录\
             （gitcode.com / atomgit.com 登录后可自动使用凭证），或改用 SSH 地址（git@…）。\
             \n原始错误：{stderr}"
        )
    }
}

pub(super) fn clone_with_optional_auth(
    git: &Path,
    url: &str,
    target: &Path,
    add_args: impl Fn(&mut Command),
) -> Result<()> {
    let run = |extra: Option<&[String]>| -> Result<std::process::Output> {
        let mut cmd = git_command(git);
        if let Some(e) = extra {
            cmd.args(e);
        }
        add_args(&mut cmd);
        cmd.output().context("spawn git clone")
    };

    let out = run(None)?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);

    if is_git_auth_failure(&stderr) {
        if let Some(cargs) = auth_retry_args(url) {
            if target.exists() {
                std::fs::remove_dir_all(target).ok();
            }
            let out2 = run(Some(&cargs))?;
            if out2.status.success() {
                return Ok(());
            }
            bail!(
                "克隆失败：使用已登录凭证仍无法访问该私有仓库（可能无权限或登录已过期，\
                 可 /login 重新登录后重试）。\n原始错误：{}",
                String::from_utf8_lossy(&out2.stderr).trim()
            );
        }
        bail!("{}", auth_required_message("克隆", url, &stderr));
    }
    bail!("git clone failed: {}", stderr);
}

/// `git pull --ff-only` in `repo`, anonymously first; on auth failure for a
/// trusted-host `source_url` while logged in, retry once with the injected
/// header. Symmetric with `clone_with_optional_auth` for the update path.
pub(super) fn git_pull_ff(repo: &Path, source_url: &str) -> Result<()> {
    let git = find_git()?;
    let run = |extra: Option<&[String]>| -> Result<std::process::Output> {
        let mut cmd = git_command(&git);
        if let Some(e) = extra {
            cmd.args(e);
        }
        cmd.args(["pull", "--ff-only"]).current_dir(repo);
        cmd.output().context("spawn git pull")
    };

    let out = run(None)?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if is_git_auth_failure(&stderr) {
        if let Some(cargs) = auth_retry_args(source_url) {
            let out2 = run(Some(&cargs))?;
            if out2.status.success() {
                return Ok(());
            }
            bail!(
                "更新失败：使用已登录凭证仍无法访问（无权限或登录已过期，可 /login 重新登录）。\
                 \n原始错误：{}",
                String::from_utf8_lossy(&out2.stderr).trim()
            );
        }
        bail!("{}", auth_required_message("更新", source_url, &stderr));
    }
    bail!("git pull failed: {}", stderr);
}

/// True when `git`'s stderr indicates it failed because it needed interactive
/// credentials we deliberately suppressed (private repo, no helper/PAT).
fn is_git_auth_failure(stderr: &str) -> bool {
    stderr.contains("terminal prompts disabled")
        || stderr.contains("could not read Username")
        || stderr.contains("could not read Password")
        || stderr.contains("Authentication failed")
}

pub(super) fn git_clone(url: &str, target: &Path) -> Result<()> {
    let git = find_git()?;
    clone_with_optional_auth(&git, url, target, |cmd| {
        cmd.args(["clone", "--depth", "1", url]).arg(target);
    })
}

fn git_rev_parse(repo: &Path) -> Result<String> {
    let git = find_git()?;
    let out = Command::new(&git)
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
        .context("spawn git rev-parse")?;
    if !out.status.success() {
        bail!(
            "git rev-parse failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
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
    let installed =
        super::state::load_installed_plugins_file(&paths::installed_plugins_file().unwrap())?;
    if installed.plugins.values().any(|p| p.marketplace == name) {
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

    // If the clone directory is missing (e.g. deleted manually or a prior
    // clone failed), re-clone from the registered source URL instead of
    // failing with "No such file or directory" on `current_dir`.
    if !target.exists() {
        git_clone(&entry.source, &target)
            .with_context(|| format!("re-clone marketplace `{}`", name))?;
    } else {
        git_pull_ff(&target, &entry.source)?;
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

    #[cfg(unix)]
    #[test]
    fn git_runs_rejects_present_but_failing_stub() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();

        // Present-but-nonfunctional stub (mirrors macOS `/usr/bin/git` with no
        // Xcode CLT: the file exists but every invocation exits non-zero). It
        // must NOT be treated as a working git — otherwise the GitNotFound hint
        // is skipped and a later `git clone` dumps a red xcode-select error.
        let stub = dir.path().join("git-stub");
        write!(
            std::fs::File::create(&stub).unwrap(),
            "#!/bin/sh\necho 'No developer tools were found' >&2\nexit 1\n"
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            !git_runs(&stub),
            "a stub that exits non-zero must not count as working git"
        );

        // A binary that runs `--version` successfully IS accepted.
        let ok = dir.path().join("git-ok");
        write!(
            std::fs::File::create(&ok).unwrap(),
            "#!/bin/sh\necho 'git version 2.40.0'\nexit 0\n"
        )
        .unwrap();
        std::fs::set_permissions(&ok, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            git_runs(&ok),
            "a binary whose --version succeeds must count as git"
        );

        // A path that does not exist does not run.
        assert!(!git_runs(&dir.path().join("does-not-exist")));
    }

    fn make_bare_repo_with_manifest(name: &str, manifest: Option<&str>) -> PathBuf {
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
    fn git_command_runs_noninteractively() {
        // The whole point of git_command: git must never open the tty for a
        // credential prompt — that deadlocks the raw-mode TUI (the private-repo
        // `/plugin marketplace add` freeze). Verify the guard env is present on
        // every git invocation we build.
        let cmd = git_command(Path::new("git"));
        let prompt = cmd
            .get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new("GIT_TERMINAL_PROMPT"))
            .map(|(_, v)| v);
        assert_eq!(
            prompt,
            Some(Some(std::ffi::OsStr::new("0"))),
            "GIT_TERMINAL_PROMPT must be 0 so a private remote fails fast \
             instead of blocking on a tty prompt"
        );
    }

    #[test]
    fn git_auth_failure_is_detected() {
        // The non-interactive error git emits when it needs credentials.
        assert!(is_git_auth_failure(
            "fatal: could not read Username for 'https://gitcode.com': terminal prompts disabled"
        ));
        assert!(is_git_auth_failure(
            "remote: HTTP Basic: Access denied\nfatal: Authentication failed for 'https://x/y'"
        ));
        // Plain not-found / network errors are NOT auth failures — they must
        // keep their original message, not the credentials hint.
        assert!(!is_git_auth_failure(
            "fatal: repository 'https://x/y' not found"
        ));
        assert!(!is_git_auth_failure(
            "fatal: unable to access 'https://x/y': Could not resolve host"
        ));
    }

    #[test]
    fn basic_auth_header_encodes_user_colon_token() {
        // base64("alice:tok123") = "YWxpY2U6dG9rMTIz"
        assert_eq!(
            basic_auth_header("alice", "tok123"),
            "Authorization: Basic YWxpY2U6dG9rMTIz"
        );
    }

    #[test]
    fn extra_header_config_is_scoped_to_host() {
        let cfg = extra_header_config(
            "https://gitcode.com/owner/repo.git",
            "Authorization: Basic XYZ",
        );
        assert_eq!(
            cfg,
            "http.https://gitcode.com.extraHeader=Authorization: Basic XYZ"
        );
    }

    #[test]
    #[serial_test::serial]
    fn auth_retry_args_none_when_untrusted_host() {
        // github.com 非白名单 → 永不注入 token，即使已登录
        assert!(auth_retry_args("https://github.com/owner/repo").is_none());
    }

    #[test]
    #[serial_test::serial]
    fn auth_retry_args_none_when_not_logged_in() {
        let _home = isolated_home(); // 无 auth.toml
        assert!(auth_retry_args("https://gitcode.com/owner/repo").is_none());
    }

    #[test]
    #[serial_test::serial]
    fn auth_required_message_untrusted_host_suggests_ssh_not_login() {
        // Platform token is never sent to non-allowlisted hosts, so /login
        // wouldn't help — guide to SSH/creds instead.
        let m = auth_required_message("克隆", "https://github.com/o/r", "fatal: auth");
        assert!(m.contains("SSH"), "got: {m}");
        assert!(
            !m.contains("/login"),
            "must not suggest /login for untrusted host: {m}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn auth_required_message_trusted_not_logged_in_suggests_login() {
        let _home = isolated_home(); // no auth.toml under the temp ATOMCODE_HOME
        let Some(domain) = atomcode_config::endpoints::trusted_domains().first() else {
            return; // no trusted domain configured — this branch is unreachable
        };
        let url = format!("https://{domain}/o/r");
        let m = auth_required_message("克隆", &url, "fatal: auth");
        assert!(
            m.contains("/login"),
            "trusted host + not logged in should guide to /login: {m}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn auth_required_message_trusted_logged_in_says_session_expired() {
        // Logged in (auth.toml present) but no usable token (expired + refresh
        // failed) → must say the session expired, not "just log in" as if the
        // user never had. ATOMCODE_HOME is isolated, so this writes to a
        // tempdir, never the real ~/.atomcode/auth.toml.
        let _home = isolated_home();
        let dir = atomcode_config::config::Config::config_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("auth.toml"),
            "access_token = \"x\"\ntoken_type = \"Bearer\"\n[user]\nid = \"1\"\nusername = \"alice\"\n",
        )
        .unwrap();
        let Some(domain) = atomcode_config::endpoints::trusted_domains().first() else {
            return; // no trusted domain configured — this branch is unreachable
        };
        let url = format!("https://{domain}/o/r");
        let m = auth_required_message("更新", &url, "fatal: auth");
        assert!(
            m.contains("登录已过期") || m.contains("重新登录"),
            "logged-in-but-dead-token should indicate re-login: {m}"
        );
    }

    #[test]
    fn injected_auth_header_is_not_persisted_to_git_config() {
        // Security invariant lock: we inject the credential via a TOP-LEVEL
        // `git -c <cfg> clone …` (one-shot, NOT written to the new repo), never
        // the clone-option form `git clone -c <cfg> …` (which git PERSISTS into
        // the cloned `.git/config` — a plaintext-token-on-disk leak). Verified
        // empirically 2026-06-17 that the two forms differ; this test fails if
        // the arg order ever regresses to the persisting form.
        let Ok(git) = find_git() else {
            return; // no git on this machine — skip
        };
        let src = make_bare_repo_with_manifest("persist-src", None);
        let dst = tempfile::tempdir().unwrap();
        let clone_dir = dst.path().join("clone");
        let header = basic_auth_header("alice", "tok-SECRET-123");
        let cfg = extra_header_config("https://gitcode.com/o/r", &header);
        // EXACT arg order produced by clone_with_optional_auth's run(Some(..)):
        // git_command base, then `-c <cfg>`, then the `clone …` args.
        let status = git_command(&git)
            .args(["-c", &cfg, "clone", "--depth", "1"])
            .arg(&src)
            .arg(&clone_dir)
            .status()
            .expect("spawn git clone");
        assert!(status.success(), "local clone should succeed");
        let config = std::fs::read_to_string(clone_dir.join(".git/config")).unwrap();
        let lc = config.to_lowercase();
        assert!(
            !lc.contains("extraheader") && !config.contains("tok-SECRET-123"),
            "auth header / token must NOT be persisted to .git/config; got:\n{}",
            config
        );
    }

    #[test]
    #[serial_test::serial]
    fn add_marketplace_with_manifest() {
        let _home = isolated_home();
        let repo = make_bare_repo_with_manifest(
            "ascend-model-agent-plugin",
            Some(
                r#"{"name":"ascend-model-agent-plugin","plugins":[{"name":"ascend-model-agent-plugin","source":"./"}]}"#,
            ),
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
            Some(
                r#"{"name":"canonical-name","plugins":[{"name":"canonical-name","source":"./"}]}"#,
            ),
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
