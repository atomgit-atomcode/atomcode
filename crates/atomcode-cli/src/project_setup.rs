//! Project capability inspection and confirmed local setup. No agent or connection owner.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use atomcode_capabilities::mcp::{McpConfigSource, McpStorage, McpTransportConfig};
use atomcode_capabilities::setup::recommendations::{
    recommendations_for, Capability, Recommendation, CONTEXT7_URL,
};
use atomcode_capabilities::setup::{fs_atomic::atomic_write, lock::SetupLock, scan};
use atomcode_capabilities::ProductDirs;
use atomcode_tui::setup::{CapabilityEntry, CapabilityPlan, CapabilityReport};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const NOTICE_FILE: &str = "capability-notices.json";
const NOTICE_VERSION: u32 = 1;

pub(crate) fn words<'a>(en: &'a str, zh: &'a str) -> &'a str {
    if atomcode_i18n::current_locale() == atomcode_i18n::Locale::ZhCn {
        zh
    } else {
        en
    }
}

#[derive(Clone)]
pub struct ProjectSetup {
    pub root: PathBuf,
    pub dirs: ProductDirs,
    pub config_path: PathBuf,
}

enum PreparedAction {
    ConfigureContext7(String),
    InstallAstGrep {
        program: PathBuf,
        args: Vec<&'static str>,
    },
}

#[derive(Default, Serialize, Deserialize)]
struct Notices {
    version: u32,
    #[serde(default)]
    seen: BTreeMap<String, u32>,
}

fn read_notices(project_dir: &Path) -> Result<Notices> {
    match std::fs::read(project_dir.join(NOTICE_FILE)) {
        Ok(bytes) => {
            let state: Notices = serde_json::from_slice(&bytes)?;
            if state.version != NOTICE_VERSION {
                bail!("unsupported capability notice version");
            }
            Ok(state)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Notices {
            version: NOTICE_VERSION,
            ..Default::default()
        }),
        Err(e) => Err(e.into()),
    }
}

/// A notice is acknowledged only by the screen's existing drawn-welcome callback.
#[derive(Debug, Clone)]
pub struct CapabilityHint {
    pub text: String,
    project_dir: PathBuf,
    recommendations: Vec<Recommendation>,
}

impl CapabilityHint {
    pub fn shown(&self) -> Result<()> {
        remember(&self.project_dir, &self.recommendations)
    }
}

fn remember(project_dir: &Path, recommendations: &[Recommendation]) -> Result<()> {
    let _lock = SetupLock::acquire(project_dir, false)?;
    let mut notices = read_notices(project_dir)?;
    for rec in recommendations {
        notices.seen.insert(rec.id.into(), rec.revision);
    }
    atomic_write(
        &project_dir.join(NOTICE_FILE),
        &serde_json::to_vec_pretty(&notices)?,
        0o600,
    )
}

pub fn startup_hint(root: &Path, dirs: &ProductDirs) -> Option<CapabilityHint> {
    let signals = scan::scan(root);
    if signals.stacks.is_empty() {
        return None;
    }
    let project_dir = dirs.project(root);
    // Unreadable/future state is preserved rather than reset on every startup.
    let state = read_notices(&project_dir).ok()?;
    let recommendations: Vec<_> = recommendations_for(&signals.stacks)
        .into_iter()
        .filter(|r| state.seen.get(r.id).copied().unwrap_or(0) < r.revision)
        .collect();
    if recommendations.is_empty() {
        return None;
    }
    Some(CapabilityHint {
        text: words("Code project detected. Run /setup to check language tools and optional documentation access.", "检测到代码项目，可运行 /setup 检查语言工具与可选的库文档能力。").into(),
        project_dir,
        recommendations,
    })
}

impl ProjectSetup {
    fn catalog(&self) -> Vec<Recommendation> {
        recommendations_for(&scan::scan(&self.root).stacks)
    }

    pub fn dismiss(&self) -> Result<String> {
        remember(&self.dirs.project(&self.root), &self.catalog())?;
        Ok(words(
            "These recommendations were acknowledged. /setup can check again at any time.",
            "已记录这些推荐；随时可用 /setup 重新检查。",
        )
        .into())
    }

    pub async fn check(&self, _force: bool) -> Result<CapabilityReport> {
        let this = self.clone();
        let (catalog, lsp, mcp) = tokio::task::spawn_blocking(move || -> Result<_> {
            let config = if this.config_path.exists() {
                atomcode_config::config::Config::load_strict(&this.config_path)
                    .context("Cannot read settings for capability inspection")?
            } else {
                atomcode_config::config::Config::default()
            };
            let mcp = McpStorage::new(this.root.clone(), this.dirs.user().to_path_buf())
                .load_including_disabled()?;
            Ok((this.catalog(), config.lsp, mcp))
        })
        .await??;
        let mut entries = Vec::new();
        for rec in &catalog {
            let mut entry = CapabilityEntry {
                id: rec.id.into(),
                name: rec.name.into(),
                status: String::new(),
                detail: String::new(),
                installable: false,
            };
            match rec.capability {
                Capability::AstGrep => match ast_grep_binary(&self.root).await {
                    Ok(Some(path)) => {
                        entry.status = words("Available", "可用").into();
                        entry.detail = format!("{}\n{}", path.display(), words("Structural search; the built-in ast_grep tool can use this binary.", "结构化搜索；内置 ast_grep 工具可直接使用此二进制。"));
                    }
                    Ok(None) => {
                        entry.status = words("Binary missing", "缺少二进制").into();
                        entry.installable = install_command().is_ok();
                        entry.detail = words("Structural search is built in, but needs the ast-grep binary. Confirm a supported installer, or install manually: https://ast-grep.github.io/guide/quick-start", "已内置结构化搜索工具，需安装 ast-grep 二进制。可确认执行受支持的安装方案，或参照官方说明手动安装：https://ast-grep.github.io/guide/quick-start").into();
                    }
                    Err(error) => {
                        entry.status = words("Binary check failed", "二进制检查失败").into();
                        entry.detail = error.to_string();
                    }
                },
                Capability::Context7 => {
                    if let Some(server) = mcp.iter().find(|s| is_context7(&s.config)) {
                        entry.status = if server.disabled {
                            words("Configured, disabled", "已配置，已禁用")
                        } else if server.source == McpConfigSource::Project
                            && !atomcode_capabilities::mcp::trust::is_project_trusted(
                                &self.root,
                                self.dirs.user(),
                            )
                        {
                            words("Configured, project untrusted", "已配置，项目未信任")
                        } else {
                            words("Configured; connection not checked", "已配置；未检查连接")
                        }
                        .into();
                        entry.detail = words("Inspect connection and enablement in /mcp. Existing configuration is preserved.", "请在 /mcp 查看连接与启用状态；保留现有配置。").into();
                    } else if mcp.iter().any(|s| s.name == "context7") {
                        entry.status = words("Name already used", "名称已被使用").into();
                        entry.detail = words(
                            "A different server is named context7; it will not be replaced.",
                            "已有其他服务使用 context7 名称，不会替换。",
                        )
                        .into();
                    } else {
                        entry.status = words("Optional, not configured", "可选，尚未配置").into();
                        entry.installable = true;
                        entry.detail = words("Version-specific third-party library documentation. Adds a disabled remote MCP entry without a key; queries go to Context7 when you enable and use it. Source: https://github.com/upstash/context7", "检索第三方库的版本相关文档。添加不带 key、默认禁用的远程 MCP 配置；启用并使用后，查询会发送至 Context7。来源：https://github.com/upstash/context7").into();
                    }
                }
                Capability::NativeLsp { extension } => {
                    // Mirror the runtime registry's normalized extension matching.
                    let configured = configured_lsp(&lsp, extension);
                    let defaults =
                        atomcode_capabilities::codeintel::lsp::LspServerRegistry::with_defaults();
                    let command = configured
                        .map(|s| s.command.as_str())
                        .or_else(|| defaults.get(extension).map(|s| s.command.as_str()));
                    let binary = command.and_then(|c| resolve_command(c, &self.root));
                    entry.status =
                        if configured.is_some_and(|s| s.options.disabled) || !lsp.enabled {
                            words("Local LSP disabled", "本地 LSP 未启用")
                        } else if configured.is_none() && !lsp.auto_detect {
                            words("Server not configured", "尚未配置服务端")
                        } else if binary.is_none() {
                            words("Server binary missing", "缺少服务端二进制")
                        } else {
                            words(
                                "Binary found; connection not checked",
                                "已找到二进制；未检查连接",
                            )
                        }
                        .into();
                    entry.detail = format!("{}: {}\n{}", words("Server", "服务端"), command.unwrap_or("—"), words("Use the existing native LSP for semantic navigation. Configure lsp.enabled plus lsp.auto_detect or lsp.servers; validate a source file with `lsp probe`. IDE binding is separate and is not probed here.", "复用原生 LSP 做语义导航。配置 lsp.enabled 以及 lsp.auto_detect 或 lsp.servers，再用 lsp probe 验证源文件；IDE 绑定独立，此处不探测。"));
                }
            }
            entries.push(entry);
        }
        // Explicit inspection acknowledges the current catalog, independent of seed state.
        // Force always recomputes; no stale capability cache is used.
        if let Err(error) = remember(&self.dirs.project(&self.root), &catalog) {
            // A read-only project or newer notice format must not prevent inspection.
            // Preserve the notice document and allow an explicit skip to report failure.
            tracing::warn!(%error, "could not remember capability inspection");
        }
        Ok(CapabilityReport { summary: words("Project capabilities — select a missing tool to review its setup plan. Inspection does not start language servers or connect MCP.", "项目能力检查——选择缺失工具查看安装计划。检查不会启动语言服务器或连接 MCP。").into(), entries })
    }

    pub async fn plan(&self, id: &str) -> Result<CapabilityPlan> {
        Ok(self.prepare(id).await?.0)
    }

    async fn prepare(&self, id: &str) -> Result<(CapabilityPlan, PreparedAction)> {
        if !self.catalog().iter().any(|r| r.id == id) {
            bail!("Unknown project capability: {id}");
        }
        let action;
        let detail = match id {
            "ast-grep" => {
                if ast_grep_binary(&self.root).await?.is_some() {
                    bail!("ast-grep is already available");
                }
                let (program, args) = install_command()?;
                action = PreparedAction::InstallAstGrep {
                    program: program.clone(),
                    args: args.clone(),
                };
                format!("{}\n{:?} {}\n{}", words("Install ast-grep using this package manager:", "使用以下包管理器安装 ast-grep："), program, args.join(" "), words("Installs into the package manager's normal user/system location. No sudo is added. May download packages and run build scripts. The binary is verified afterward. Source: https://ast-grep.github.io/guide/quick-start", "安装至包管理器的常规用户/系统位置，不添加 sudo。可能下载包并执行构建脚本，完成后校验二进制。来源：https://ast-grep.github.io/guide/quick-start"))
            }
            "context7" => {
                action = PreparedAction::ConfigureContext7(self.context7_edit()?);
                format!("{}\n{}\n{}", self.root.join(".mcp.json").display(), CONTEXT7_URL, words("Add Context7 as disabled, without credentials or auto-approval. Existing entries are preserved. Use /mcp to reload and explicitly enable it; project trust is still required. No network connection is made now.", "添加默认禁用、不带凭据和自动审批权限的 Context7，保留现有条目。随后在 /mcp 重载并显式启用，仍须满足项目信任要求；现在不会建立网络连接。"))
            }
            _ => bail!("This capability requires manual configuration"),
        };
        // Include the command, location and exact project config being confirmed.
        let mut hash = Sha256::new();
        hash.update(id.as_bytes());
        hash.update(self.root.to_string_lossy().as_bytes());
        hash.update(detail.as_bytes());
        if let PreparedAction::ConfigureContext7(content) = &action {
            hash.update(content.as_bytes());
        }
        Ok((
            CapabilityPlan {
                id: id.into(),
                detail,
                token: format!("{:x}", hash.finalize()),
            },
            action,
        ))
    }

    fn context7_edit(&self) -> Result<String> {
        let configs = McpStorage::new(self.root.clone(), self.dirs.user().to_path_buf())
            .load_including_disabled()?;
        if configs
            .iter()
            .any(|s| s.name == "context7" || is_context7(&s.config))
        {
            bail!("Context7 is already configured or its name is in use; inspect /mcp");
        }
        let text = match std::fs::read_to_string(self.root.join(".mcp.json")) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        atomcode_capabilities::mcp::config::add_disabled_http_server_into_text(
            text.as_deref(),
            "context7",
            CONTEXT7_URL,
        )
    }

    pub async fn apply(&self, id: &str, token: &str) -> Result<String> {
        let _lock = SetupLock::acquire(&self.dirs.project(&self.root), false)?;
        let (plan, action) = self.prepare(id).await?;
        if plan.token != token {
            bail!("Setup plan changed. Review the new plan before confirming.");
        }
        match action {
            PreparedAction::ConfigureContext7(content) => {
                atomic_write(&self.root.join(".mcp.json"), content.as_bytes(), 0o600)?;
                Ok(words("Context7 added disabled. Open /mcp, reload, then enable it explicitly. Existing project trust and tool approvals were not changed.", "已添加默认禁用的 Context7。请打开 /mcp，重载后显式启用；项目信任和工具审批权限未改变。").into())
            }
            PreparedAction::InstallAstGrep { program, args } => {
                // Avoid executing package-manager config from the project being inspected.
                let staging = tempfile::tempdir()?;
                run_installer(&program, &args, staging.path(), Duration::from_secs(600)).await?;
                if ast_grep_binary(&self.root).await?.is_none() {
                    bail!("Installer exited successfully but ast-grep is not available on this process PATH; check the installation and restart if PATH changed");
                }
                Ok(words("ast-grep installed and verified. The built-in ast_grep tool is ready; no MCP configuration is needed.", "ast-grep 已安装并通过校验，内置 ast_grep 工具可直接使用，无需 MCP 配置。").into())
            }
        }
    }
}

/// Keeps installer descendants with their parent on cancellation and timeout.
struct InstallerTree {
    #[cfg(unix)]
    pgid: Option<i32>,
    #[cfg(windows)]
    job: Option<atomcode_capabilities::process_utils::JobHandle>,
    #[cfg(windows)]
    pid: Option<u32>,
}

impl InstallerTree {
    fn new(child: &tokio::process::Child) -> Self {
        Self {
            #[cfg(unix)]
            pgid: child.id().and_then(|id| i32::try_from(id).ok()),
            #[cfg(windows)]
            job: atomcode_capabilities::process_utils::assign_child_to_kill_on_close_job(child),
            #[cfg(windows)]
            pid: child.id(),
        }
    }
    fn disarm(&mut self) {
        #[cfg(unix)]
        {
            self.pgid = None;
        }
        #[cfg(windows)]
        {
            if let Some(job) = &self.job {
                job.preserve_descendants();
            }
            self.job = None;
            self.pid = None;
        }
    }
}

impl Drop for InstallerTree {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid {
            // This child started a fresh process group; never signal our own group.
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
        #[cfg(windows)]
        atomcode_capabilities::process_utils::kill_windows_tree(&self.job, self.pid);
    }
}

async fn run_installer(
    program: &Path,
    args: &[&str],
    root: &Path,
    timeout: Duration,
) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    // File-backed stderr bounds memory even if a package manager is very noisy.
    let mut errors = tempfile::tempfile()?;
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(errors.try_clone()?)
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    atomcode_capabilities::process_utils::suppress_console_window(&mut command);
    let mut child = command.spawn()?;
    let mut tree = InstallerTree::new(&child);
    let waited = tokio::time::timeout(timeout, child.wait()).await;
    // Reap and disarm without another await, avoiding a later signal to a reused PID.
    if matches!(waited, Ok(Ok(_))) {
        tree.disarm();
    }
    let status =
        waited.context("Installation timed out; inspect the package manager before retrying")??;
    if !status.success() {
        let length = errors.metadata()?.len();
        errors.seek(SeekFrom::Start(length.saturating_sub(8192)))?;
        let mut bytes = Vec::new();
        errors.read_to_end(&mut bytes)?;
        let text = String::from_utf8_lossy(&bytes);
        let tail: String = text
            .chars()
            .rev()
            .take(2000)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        bail!("ast-grep installation failed ({status}): {tail}");
    }
    Ok(())
}

async fn is_ast_grep_binary(path: &Path, root: &Path) -> bool {
    let mut cmd = tokio::process::Command::new(path);
    cmd.arg("--version")
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    atomcode_capabilities::process_utils::suppress_console_window(&mut cmd);
    matches!(tokio::time::timeout(Duration::from_secs(2), cmd.output()).await, Ok(Ok(output)) if output.status.success() && String::from_utf8_lossy(&output.stdout).trim().starts_with("ast-grep "))
}

fn configured_lsp<'a>(
    config: &'a atomcode_config::config::LspConfig,
    extension: &str,
) -> Option<&'a atomcode_config::lsp_registry::LspServerConfig> {
    // The scanner groups JavaScript and TypeScript under Node. Respect an
    // explicit JS/JSX configuration instead of reporting that entire stack missing.
    let extensions = if extension == "ts" {
        vec!["ts", "js", "tsx", "jsx"]
    } else {
        vec![extension]
    };
    extensions.into_iter().find_map(|extension| {
        config
            .servers
            .iter()
            .find(|(ext, _)| ext.trim_start_matches('.').eq_ignore_ascii_case(extension))
            .map(|(_, server)| server)
    })
}

fn resolve_command(command: &str, root: &Path) -> Option<PathBuf> {
    which::which_in(command, std::env::var_os("PATH"), root).ok()
}

async fn ast_grep_binary(root: &Path) -> Result<Option<PathBuf>> {
    for name in ["ast-grep", "sg"] {
        let Some(path) = resolve_command(name, root) else {
            continue;
        };
        if is_ast_grep_binary(&path, root).await {
            return Ok(Some(path));
        }
        // The runtime also prefers ast-grep when present. Never claim its sg
        // fallback is usable while a broken canonical binary shadows it.
        if name == "ast-grep" {
            bail!(
                "The ast-grep executable failed its identity/version check; repair it before setup"
            );
        }
    }
    Ok(None)
}

fn install_command() -> Result<(PathBuf, Vec<&'static str>)> {
    if cfg!(target_os = "macos") {
        if let Ok(program) = which::which("brew") {
            return Ok((program, vec!["install", "ast-grep"]));
        }
    }
    if let Ok(program) = which::which("cargo") {
        return Ok((program, vec!["install", "ast-grep", "--locked"]));
    }
    bail!("No supported installer found (Homebrew on macOS, or Cargo). Install manually: https://ast-grep.github.io/guide/quick-start")
}

fn is_context7(config: &McpTransportConfig) -> bool {
    match config {
        McpTransportConfig::Http { url, .. } => reqwest::Url::parse(url).is_ok_and(|url| {
            url.scheme() == "https"
                && url.host_str() == Some("mcp.context7.com")
                && url.port_or_known_default() == Some(443)
                && url.path().trim_end_matches('/') == "/mcp"
        }),
        McpTransportConfig::Stdio { args, .. } => args
            .iter()
            .any(|a| a == "@upstash/context7-mcp" || a.starts_with("@upstash/context7-mcp@")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(root: &Path, user: &Path) -> ProjectSetup {
        ProjectSetup {
            root: root.into(),
            dirs: ProductDirs::new(user, ".test-product"),
            config_path: user.join("config.toml"),
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn confirmed_context7_configuration_refuses_changed_plans_and_stays_disabled() {
        let root = tempfile::tempdir().unwrap();
        let user = tempfile::tempdir().unwrap();
        let setup = service(root.path(), user.path());
        std::fs::write(root.path().join("Cargo.toml"), "[package]").unwrap();
        let plan = setup.plan("context7").await.unwrap();
        assert!(
            !root.path().join(".mcp.json").exists(),
            "planning is read-only"
        );
        assert!(setup.apply("context7", "unreviewed").await.is_err());
        std::fs::write(root.path().join(".mcp.json"), r#"{"other":42}"#).unwrap();
        assert!(
            setup.apply("context7", &plan.token).await.is_err(),
            "edited plans need new confirmation"
        );
        let reviewed = setup.plan("context7").await.unwrap();
        setup.apply("context7", &reviewed.token).await.unwrap();
        let configs = McpStorage::new(root.path(), user.path());
        assert!(configs.load().unwrap().is_empty());
        assert!(configs.load_including_disabled().unwrap()[0].disabled);
        assert!(!atomcode_capabilities::mcp::trust::is_project_trusted(
            &setup.root,
            setup.dirs.user()
        ));
    }

    // Real subprocesses run outside Tokio's virtual clock; this checks their output.
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn checks_binary_identity_and_reports_installer_failures() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let binary = root.path().join("sg");
        std::fs::write(&binary, "#!/bin/sh\necho 'unrelated group utility'\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(!is_ast_grep_binary(&binary, root.path()).await);
        std::fs::write(&binary, "#!/bin/sh\necho 'ast-grep 0.40.0'\n").unwrap();
        assert!(is_ast_grep_binary(&binary, root.path()).await);
        let result = run_installer(
            Path::new("/bin/sh"),
            &["-c", "echo failed >&2; exit 3"],
            root.path(),
            Duration::from_secs(2),
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("failed"));
        run_installer(
            Path::new("/bin/sh"),
            &["-c", "exit 0"],
            root.path(),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn installation_timeout_is_an_error() {
        let root = tempfile::tempdir().unwrap();
        let error = run_installer(
            Path::new("/bin/sleep"),
            &["60"],
            root.path(),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }

    #[test]
    fn native_lsp_check_respects_normalized_extensions_and_node_js_configuration() {
        let config: atomcode_config::config::LspConfig = toml::from_str(
            r#"
            enabled = true
            [servers.js]
            command = "project-js-server"
            [servers.".RS"]
            command = "project-rust-server"
        "#,
        )
        .unwrap();
        assert_eq!(
            configured_lsp(&config, "ts").unwrap().command,
            "project-js-server"
        );
        assert_eq!(
            configured_lsp(&config, "rs").unwrap().command,
            "project-rust-server"
        );
        assert!(configured_lsp(&config, "py").is_none());
    }
    #[test]
    fn notice_survives_manifest_changes_and_preserves_future_state() {
        let root = tempfile::tempdir().unwrap();
        let user = tempfile::tempdir().unwrap();
        let setup = service(root.path(), user.path());
        std::fs::write(root.path().join("Cargo.toml"), "[package]\nname='a'\n").unwrap();
        let hint = startup_hint(&setup.root, &setup.dirs).unwrap();
        // Merely preparing a hint does not acknowledge it.
        assert!(startup_hint(&setup.root, &setup.dirs).is_some());
        hint.shown().unwrap();
        std::fs::write(root.path().join("Cargo.toml"), "[package]\nname='b'\n").unwrap();
        assert!(startup_hint(&setup.root, &setup.dirs).is_none());
        std::fs::write(root.path().join("package.json"), "{}").unwrap();
        assert!(startup_hint(&setup.root, &setup.dirs).is_some());
        let path = setup.dirs.project(&setup.root).join(NOTICE_FILE);
        std::fs::write(&path, r#"{"version":99,"seen":{}}"#).unwrap();
        assert!(setup.dismiss().is_err());
        assert!(startup_hint(&setup.root, &setup.dirs).is_none());
        assert!(std::fs::read_to_string(path).unwrap().contains("99"));
    }

    #[test]
    fn context7_edits_are_disabled_and_preserve_existing_servers_and_trust() {
        let root = tempfile::tempdir().unwrap();
        let user = tempfile::tempdir().unwrap();
        let setup = service(root.path(), user.path());
        std::fs::write(root.path().join("Cargo.toml"), "[package]").unwrap();
        std::fs::write(
            root.path().join(".mcp.json"),
            r#"{"mcpServers":{"mine":{"command":"server","trust":true}},"extra":7}"#,
        )
        .unwrap();
        atomcode_capabilities::mcp::trust::trust_project(&setup.root, setup.dirs.user()).unwrap();
        let updated = setup.context7_edit().unwrap();
        let value: serde_json::Value = serde_json::from_str(&updated).unwrap();
        assert_eq!(value["extra"], 7);
        assert_eq!(value["mcpServers"]["mine"]["trust"], true);
        let added = &value["mcpServers"]["context7"];
        assert_eq!(
            added,
            &serde_json::json!({"url":CONTEXT7_URL,"disabled":true})
        );
        std::fs::write(root.path().join(".mcp.json"), updated).unwrap();
        assert!(setup.context7_edit().is_err());
        assert!(McpStorage::new(setup.root.clone(), setup.dirs.user())
            .load()
            .unwrap()
            .iter()
            .all(|s| s.name != "context7"));
    }

    #[test]
    fn detects_context7_by_identity_including_disabled_user_aliases() {
        let root = tempfile::tempdir().unwrap();
        let user = tempfile::tempdir().unwrap();
        let setup = service(root.path(), user.path());
        std::fs::write(
            user.path().join("mcp.json"),
            format!(r#"{{"mcpServers":{{"docs":{{"url":"{CONTEXT7_URL}/","disabled":true}}}}}}"#),
        )
        .unwrap();
        assert!(setup.context7_edit().is_err());
        std::fs::write(user.path().join("mcp.json"), "invalid").unwrap();
        assert!(setup.context7_edit().is_err());
        assert!(!root.path().join(".mcp.json").exists());
    }
}
