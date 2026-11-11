//! Host-local setup of an externally installed browser MCP. No browser is launched here.
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use atomcode_capabilities::mcp::{
    merge_stdio_mcp_server_into_text, set_mcp_server_disabled_in_text,
};
use atomcode_capabilities::setup::lock::SetupLock;
use atomcode_capabilities::ProductDirs;
use clap::Subcommand;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;

use crate::computer_cmd::{read_optional, write_checked};

const SERVER: &str = "browser-use";
const OWNER: &str = "atomcode-browser-use-v1";
const VERSION: &str = "0.0.85";
const SKILL: &str = include_str!("../assets/browser-use/SKILL.md");
const ALLOWED_TOOLS: &[&str] = &[
    "browser_close",
    "browser_resize",
    "browser_handle_dialog",
    "browser_fill_form",
    "browser_press_key",
    "browser_type",
    "browser_navigate",
    "browser_navigate_back",
    "browser_take_screenshot",
    "browser_snapshot",
    "browser_click",
    "browser_drag",
    "browser_hover",
    "browser_select_option",
    "browser_tabs",
    "browser_wait_for",
];
const OPTIONS: &[&str] = &[
    "--isolated",
    "--no-webmcp",
    "--block-service-workers",
    "--image-responses",
    "allow",
];

// Clear backend attachment/import overrides inherited by the MCP subprocess.
// CLI flags alone do not override every backend environment option.
const BLOCKED_ENV: &[&str] = &[
    "PLAYWRIGHT_MCP_CONFIG",
    "PLAYWRIGHT_MCP_CDP_ENDPOINT",
    "PLAYWRIGHT_MCP_CDP_HEADERS",
    "PLAYWRIGHT_MCP_STORAGE_STATE",
    "PLAYWRIGHT_MCP_USER_DATA_DIR",
    "PLAYWRIGHT_MCP_SECRETS_FILE",
    "PLAYWRIGHT_MCP_INIT_PAGE",
    "PLAYWRIGHT_MCP_INIT_SCRIPT",
    "PLAYWRIGHT_MCP_EXTENSION_TOKEN",
    "PLAYWRIGHT_MCP_PORT",
    "PLAYWRIGHT_MCP_HOST",
    "PLAYWRIGHT_MCP_CAPS",
    "PLAYWRIGHT_MCP_GRANT_PERMISSIONS",
    "PLAYWRIGHT_MCP_EXECUTABLE_PATH",
];

fn managed_env(dirs: &ProductDirs) -> Result<Value> {
    let mut env = serde_json::Map::new();
    for name in BLOCKED_ENV {
        env.insert((*name).into(), json!(""));
    }
    for name in [
        "PLAYWRIGHT_MCP_EXTENSION",
        "PLAYWRIGHT_MCP_ALLOW_UNRESTRICTED_FILE_ACCESS",
        "PLAYWRIGHT_MCP_IGNORE_ROOTS",
        "PLAYWRIGHT_MCP_IGNORE_HTTPS_ERRORS",
    ] {
        env.insert(name.into(), json!("false"));
    }
    env.insert(
        "PLAYWRIGHT_MCP_OUTPUT_DIR".into(),
        json!(std::path::absolute(dirs.user().join("integrations/browser-use/artifacts"))?
            .to_str()
            .context("output path must be UTF-8")?),
    );
    env.insert("PLAYWRIGHT_MCP_OUTPUT_MAX_SIZE".into(), json!("16777216"));
    Ok(Value::Object(env))
}

#[derive(Clone, Subcommand)]
pub enum BrowserCli {
    /// Configure an already installed @playwright/mcp 0.0.85. Does not install or launch a browser.
    Enable {
        /// Absolute path to the installed @playwright/mcp/cli.js (required on first setup).
        #[arg(long)]
        server: Option<PathBuf>,
        /// Node executable; defaults to saved configuration or node on PATH.
        #[arg(long)]
        node: Option<PathBuf>,
    },
    /// Read saved configuration, not the live connection.
    Status,
    /// Check Node and backend versions without opening a browser or installing software.
    Doctor {
        #[arg(long)]
        server: Option<PathBuf>,
        #[arg(long)]
        node: Option<PathBuf>,
    },
    /// Disable the managed configuration for new/reloaded sessions.
    Disable,
}

pub async fn handle(command: BrowserCli) -> Result<()> {
    println!(
        "{}",
        execute(&atomcode_coding::config::product_dirs_from_env(), command).await?
    );
    Ok(())
}

fn config_path(dirs: &ProductDirs) -> PathBuf {
    dirs.user().join("mcp.json")
}
fn skill_path(dirs: &ProductDirs) -> PathBuf {
    dirs.user().join("skills/browser-use/SKILL.md")
}

fn prepare_artifacts(dirs: &ProductDirs) -> Result<()> {
    let path = dirs.user().join("integrations/browser-use/artifacts");
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if !meta.file_type().is_dir() => {
            bail!("refusing non-directory browser artifacts path")
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(&path)?
        }
        Err(error) => return Err(error.into()),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn owned_entry(text: Option<&str>) -> Result<Option<Value>> {
    let root: Value = match text {
        Some(text) => serde_json::from_str(&atomcode_capabilities::jsonc::strip_comments(text))?,
        None => json!({}),
    };
    if !root.is_object() {
        bail!("MCP config must be an object");
    }
    let mut found = None;
    for key in ["servers", "mcpServers"] {
        if let Some(map) = root.get(key) {
            let map = map
                .as_object()
                .context("MCP server map must be an object")?;
            if let Some(entry) = map.get(SERVER) {
                if entry.get("managedBy").and_then(Value::as_str) != Some(OWNER) {
                    bail!("MCP server 'browser-use' is not managed by this integration; inspect /mcp instead of overwriting it");
                }
                found = Some(entry.clone());
            }
        }
    }
    Ok(found)
}

pub async fn execute(dirs: &ProductDirs, command: BrowserCli) -> Result<String> {
    let persist = matches!(&command, BrowserCli::Enable { .. });
    match command {
        BrowserCli::Status => {
            let entry = owned_entry(read_optional(&config_path(dirs))?.as_deref())?;
            let state = match entry {
                None => "not configured",
                Some(ref v) if v["disabled"] == true => "disabled in configuration",
                Some(_) => "enabled in configuration (live connection not queried)",
            };
            Ok(format!("Browser Use: {state}\nBackend: external @playwright/mcp {VERSION}; Node 18+.\nFirst setup: atomcode browser enable --server <absolute-path-to-cli.js>\nUse /mcp to inspect live connection and project overrides. Browsers are not bundled or installed automatically."))
        }
        BrowserCli::Disable => {
            disable(dirs)?;
            Ok("Browser Use disabled in configuration. Reload with /browser reload. Disabling cannot interrupt or undo dispatched browser operations.".into())
        }
        BrowserCli::Enable { server, node } | BrowserCli::Doctor { server, node } => {
            let (node, server) = backend(dirs, server.as_deref(), node.as_deref())?;
            check(&node, &server).await?;
            if persist {
                enable(dirs, &node, &server)?;
                Ok("Browser Use configured with native MCP approvals and an isolated profile. Restart or /browser reload, then use $browser-use with your task. No browser was launched or installed by setup. Read-only hints and Always grants are not site-scoped permissions.".into())
            } else {
                Ok(format!("Node: {}\nBackend: {}\n@playwright/mcp {VERSION}: version check passed\nBrowser launch, installed browser binaries and live connection were not checked; no browser was opened.", node.display(), server.display()))
            }
        }
    }
}

fn backend(
    dirs: &ProductDirs,
    server: Option<&Path>,
    node: Option<&Path>,
) -> Result<(PathBuf, PathBuf)> {
    let entry = owned_entry(read_optional(&config_path(dirs))?.as_deref())?;
    let saved_node = entry.as_ref().and_then(|v| v["command"].as_str());
    let saved_server = entry
        .as_ref()
        .and_then(|v| v["args"].get(0))
        .and_then(Value::as_str);
    let node = std::fs::canonicalize(
        which::which(node.unwrap_or(Path::new(saved_node.unwrap_or("node"))))
            .context("Node is not installed; install Node 18+ explicitly")?,
    )?;
    let server = server.or_else(|| saved_server.map(Path::new)).context("backend not configured; install @playwright/mcp@0.0.85 explicitly, then use --server <absolute-path-to-cli.js>")?;
    if !server.is_absolute() {
        bail!("--server must be an absolute path to @playwright/mcp/cli.js");
    }
    let server = std::fs::canonicalize(server).context("backend server file not found")?;
    if !server.is_file() {
        bail!("backend server must be a regular JavaScript file");
    }
    Ok((node, server))
}

async fn version(node: &Path, args: &[&std::ffi::OsStr]) -> Result<String> {
    let mut child = tokio::process::Command::new(node)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("could not start version diagnostic")?;
    let mut stdout = child.stdout.take().context("missing diagnostic stdout")?;
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let mut bytes = Vec::new();
        (&mut stdout).take(4097).read_to_end(&mut bytes).await?;
        if bytes.len() > 4096 {
            bail!("version diagnostic exceeded output limit");
        }
        let status = child.wait().await?;
        if !status.success() {
            bail!("version diagnostic failed; configuration unchanged");
        }
        Ok::<_, anyhow::Error>(String::from_utf8(bytes)?.trim().to_string())
    })
    .await;
    match result {
        Ok(result) => result,
        Err(_) => {
            let _ = child.kill().await;
            bail!("version diagnostic timed out; configuration unchanged");
        }
    }
}

async fn check(node: &Path, server: &Path) -> Result<()> {
    let n = version(node, &[std::ffi::OsStr::new("--version")]).await?;
    if n.strip_prefix('v')
        .and_then(|v| v.split('.').next())
        .and_then(|v| v.parse::<u32>().ok())
        .is_none_or(|v| v < 18)
    {
        bail!("Node 18 or later is required");
    }
    let v = version(
        node,
        &[server.as_os_str(), std::ffi::OsStr::new("--version")],
    )
    .await?;
    if v != format!("Version {VERSION}") {
        bail!("unsupported browser backend version; expected @playwright/mcp {VERSION}");
    }
    Ok(())
}

fn enable(dirs: &ProductDirs, node: &Path, server: &Path) -> Result<()> {
    let _lock = SetupLock::acquire(&dirs.user().join("integrations/browser-use"), false)?;
    let path = config_path(dirs);
    let before = read_optional(&path)?;
    owned_entry(before.as_deref())?;
    let skill = skill_path(dirs);
    let prior_skill = read_optional(&skill)?;
    if prior_skill.as_deref().is_some_and(|s| s != SKILL) {
        bail!("browser-use skill has local changes; preserve or rename it before enabling");
    }
    let args: Vec<String> = std::iter::once(
        server
            .to_str()
            .context("backend path must be UTF-8")?
            .to_string(),
    )
    .chain(OPTIONS.iter().map(|s| s.to_string()))
    .collect();
    let text = merge_stdio_mcp_server_into_text(
        before.as_deref(),
        SERVER,
        node.to_str().context("Node path must be UTF-8")?,
        &args,
    )?;
    let mut root: Value = serde_json::from_str(&text)?;
    let entry = &mut root["mcpServers"][SERVER];
    entry["managedBy"] = json!(OWNER);
    entry["disabled"] = json!(false);
    entry["trust"] = json!(false);
    entry["autoApprove"] = json!([]);
    entry["allowedTools"] = json!(ALLOWED_TOOLS);
    entry["timeout_ms"] = json!(30_000);
    // Do not retain environment overrides that could attach a personal profile or CDP endpoint.
    entry["env"] = managed_env(dirs)?;
    prepare_artifacts(dirs)?;
    write_checked(&skill, &prior_skill, SKILL)?;
    write_checked(
        &path,
        &before,
        &format!("{}\n", serde_json::to_string_pretty(&root)?),
    )
}

fn disable(dirs: &ProductDirs) -> Result<()> {
    let path = config_path(dirs);
    if owned_entry(read_optional(&path)?.as_deref())?.is_none() {
        return Ok(());
    }
    let _lock = SetupLock::acquire(&dirs.user().join("integrations/browser-use"), false)?;
    let before = read_optional(&path)?;
    owned_entry(before.as_deref())?.context("managed browser configuration disappeared")?;
    let after = set_mcp_server_disabled_in_text(
        before.as_deref().context("missing configuration")?,
        SERVER,
        true,
    )?;
    write_checked(&path, &before, &after)?;
    let skill = skill_path(dirs);
    if read_optional(&skill)?.as_deref() == Some(SKILL) {
        std::fs::remove_file(skill)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tree() -> (tempfile::TempDir, ProductDirs) {
        let temp = tempfile::tempdir().unwrap();
        let dirs = ProductDirs::new(temp.path().join("user"), ".test-product");
        (temp, dirs)
    }
    fn put(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    fn setup(dirs: &ProductDirs) {
        enable(dirs, Path::new("/test/node"), Path::new("/test/server.js")).unwrap();
    }

    #[tokio::test]
    async fn queries_and_missing_setup_do_not_write() {
        let (_t, dirs) = tree();
        assert!(execute(&dirs, BrowserCli::Status)
            .await
            .unwrap()
            .contains("not configured"));
        execute(&dirs, BrowserCli::Disable).await.unwrap();
        assert!(execute(
            &dirs,
            BrowserCli::Enable {
                server: None,
                node: None
            }
        )
        .await
        .is_err());
        assert!(!dirs.user().exists());
    }

    #[test]
    fn setup_is_isolated_and_preserves_other_servers() {
        let (_t, dirs) = tree();
        put(
            &config_path(&dirs),
            r#"{"other":42,"servers":{"keep":{"command":"untouched"}}}"#,
        );
        setup(&dirs);
        let first = read_optional(&config_path(&dirs)).unwrap().unwrap();
        let root: Value = serde_json::from_str(&first).unwrap();
        assert_eq!(root["other"], 42);
        assert_eq!(root["mcpServers"]["keep"]["command"], "untouched");
        let entry = &root["mcpServers"][SERVER];
        assert_eq!(entry["trust"], false);
        assert_eq!(entry["autoApprove"], json!([]));
        assert_eq!(entry["allowedTools"], json!(ALLOWED_TOOLS));
        for name in [
            "browser_evaluate",
            "browser_run_code_unsafe",
            "browser_file_upload",
            "browser_install",
        ] {
            assert!(!ALLOWED_TOOLS.contains(&name));
        }
        assert_eq!(entry["args"][0], "/test/server.js");
        assert!(entry["args"]
            .as_array()
            .unwrap()
            .contains(&json!("--isolated")));
        assert_eq!(entry["env"]["PLAYWRIGHT_MCP_STORAGE_STATE"], "");
        assert_eq!(entry["env"]["PLAYWRIGHT_MCP_EXTENSION"], "false");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(dirs.user().join("integrations/browser-use/artifacts"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        setup(&dirs);
        assert_eq!(read_optional(&config_path(&dirs)).unwrap().unwrap(), first);
        disable(&dirs).unwrap();
        assert!(!skill_path(&dirs).exists());
        let root: Value =
            serde_json::from_str(&read_optional(&config_path(&dirs)).unwrap().unwrap()).unwrap();
        assert_eq!(root["mcpServers"][SERVER]["disabled"], true);
        assert_eq!(root["mcpServers"]["keep"]["command"], "untouched");
    }

    #[test]
    fn custom_entries_and_edited_skills_are_never_overwritten() {
        for key in ["servers", "mcpServers"] {
            let (_t, dirs) = tree();
            let initial = json!({key: {SERVER: {"command":"custom"}}}).to_string();
            put(&config_path(&dirs), &initial);
            assert!(enable(&dirs, Path::new("node"), Path::new("server")).is_err());
            assert!(disable(&dirs).is_err());
            assert_eq!(
                read_optional(&config_path(&dirs)).unwrap().unwrap(),
                initial
            );
            assert!(!skill_path(&dirs).exists());
        }
        let (_t, dirs) = tree();
        setup(&dirs);
        let before = read_optional(&config_path(&dirs)).unwrap();
        put(&skill_path(&dirs), "custom skill\nauto-load: off");
        assert!(enable(&dirs, Path::new("replacement"), Path::new("server")).is_err());
        assert_eq!(read_optional(&config_path(&dirs)).unwrap(), before);
        disable(&dirs).unwrap();
        assert_eq!(
            read_optional(&skill_path(&dirs)).unwrap().unwrap(),
            "custom skill\nauto-load: off"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_config_and_skill_are_refused() {
        let (temp, dirs) = tree();
        let target = temp.path().join("target");
        put(&target, "{}");
        std::fs::create_dir_all(dirs.user()).unwrap();
        std::os::unix::fs::symlink(&target, config_path(&dirs)).unwrap();
        assert!(enable(&dirs, Path::new("node"), Path::new("server")).is_err());
        std::fs::remove_file(config_path(&dirs)).unwrap();
        std::fs::create_dir_all(skill_path(&dirs).parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, skill_path(&dirs)).unwrap();
        assert!(enable(&dirs, Path::new("node"), Path::new("server")).is_err());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "{}");
    }
}
