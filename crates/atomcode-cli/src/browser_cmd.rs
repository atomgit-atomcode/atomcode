//! Host-local setup of an externally installed browser MCP. No browser is launched here.
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use atomcode_capabilities::mcp::{
    merge_stdio_mcp_server_into_text, set_mcp_server_disabled_in_text,
};
use atomcode_capabilities::ProductDirs;
use clap::Subcommand;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use crate::computer_cmd::{lock_mcp_setup, read_optional, write_checked};

mod origin;
pub use origin::OriginArgs;

const OLD_SKILL_SHA256: &str = "5057170a3cb358850ae77794f1596bf3e995e39dd0c6eb0b78adef864316196d";

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
        json!(
            std::path::absolute(dirs.user().join("integrations/browser-use/artifacts"))?
                .to_str()
                .context("output path must be UTF-8")?
        ),
    );
    env.insert("PLAYWRIGHT_MCP_OUTPUT_MAX_SIZE".into(), json!("16777216"));
    // This transport reads server frames during RPCs, not while the person is
    // considering an approval. The backend's optional 5s client heartbeat
    // would close an otherwise healthy idle connection. AtomCode retains its
    // own bounded RPC deadline and subprocess ownership.
    env.insert("PLAYWRIGHT_MCP_PING_TIMEOUT_MS".into(), json!("0"));
    Ok(Value::Object(env))
}

#[derive(Clone, Subcommand)]
pub enum BrowserCli {
    /// Configure an already installed @playwright/mcp 0.0.85. Does not install or launch a browser.
    Enable {
        /// Stock @playwright/mcp/cli.js path (required on first stock setup).
        #[arg(long)]
        server: Option<PathBuf>,
        /// Node executable; defaults to saved configuration or node on PATH.
        #[arg(long)]
        node: Option<PathBuf>,
        #[command(flatten)]
        scope: OriginArgs,
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
                if found.is_some() {
                    bail!(
                        "duplicate managed browser-use entries; keep a single entry before setup"
                    );
                }
                found = Some(entry.clone());
            }
        }
    }
    Ok(found)
}

pub async fn execute(dirs: &ProductDirs, command: BrowserCli) -> Result<String> {
    match command {
        BrowserCli::Status => {
            let entry = owned_entry(read_optional(&config_path(dirs))?.as_deref())?;
            let state = match entry.as_ref() {
                None => "not configured",
                Some(v) if v["disabled"] == true => "disabled in configuration",
                Some(_) => "enabled in configuration (live connection not queried)",
            };
            let scope = origin::describe(entry.as_ref(), dirs)?;
            Ok(format!("Browser Use: {state}\n{scope}\nBackend: external @playwright/mcp {VERSION}; Node 20+.\nFirst setup: atomcode browser enable --server <absolute-path-to-cli.js>\nUse /mcp to inspect live connection and project overrides. Browsers are not bundled or installed automatically."))
        }
        BrowserCli::Disable => {
            disable(dirs)?;
            Ok("Browser Use disabled in configuration. Reload with /browser reload. Disabling cannot interrupt or undo dispatched browser operations.".into())
        }
        BrowserCli::Enable {
            server,
            node,
            scope,
        } => {
            let before = read_optional(&config_path(dirs))?;
            let entry = owned_entry(before.as_deref())?;
            let plan = origin::prepare(
                dirs,
                entry.as_ref(),
                server.as_deref(),
                node.as_deref(),
                &scope,
            )?;
            plan.check(dirs).await?;
            enable_prepared(dirs, &plan, &before)?;
            Ok(format!("{}\nBrowser Use configured with native MCP approvals and an isolated profile. Restart or /browser reload, then use $browser-use with your task. No browser was launched or installed by setup. Configuration is not live activation; read-only hints and Always grants do not replace backend origin policy.", plan.summary()))
        }
        BrowserCli::Doctor { server, node } => {
            let before = read_optional(&config_path(dirs))?;
            let entry = owned_entry(before.as_deref())?;
            let plan = origin::prepare(
                dirs,
                entry.as_ref(),
                server.as_deref(),
                node.as_deref(),
                &OriginArgs::default(),
            )?;
            plan.check(dirs).await?;
            Ok(format!("Node: {}\n{}\nBackend contract check passed; no browser was opened. Browser binaries and live connection were not checked.", plan.node.to_string_lossy().escape_debug(), plan.summary()))
        }
    }
}

async fn diagnostic(
    node: &Path,
    args: &[String],
    env: &std::collections::BTreeMap<String, String>,
) -> Result<String> {
    diagnostic_with_deadline(node, args, env, Duration::from_secs(10)).await
}

async fn diagnostic_with_deadline(
    node: &Path,
    args: &[String],
    env: &std::collections::BTreeMap<String, String>,
    deadline: Duration,
) -> Result<String> {
    let mut command = tokio::process::Command::new(node);
    command
        .args(args)
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .context("could not start browser diagnostic")?;
    #[cfg(unix)]
    let mut group =
        crate::computer_cmd::ProbeGroup(child.id().and_then(|id| i32::try_from(id).ok()));
    let mut stdout = child.stdout.take().context("missing diagnostic stdout")?;
    let result = tokio::time::timeout(deadline, async {
        let mut bytes = Vec::new();
        (&mut stdout).take(16385).read_to_end(&mut bytes).await?;
        if bytes.len() > 16384 {
            bail!("browser diagnostic exceeded output limit");
        }
        if !child.wait().await?.success() {
            bail!("browser diagnostic failed; configuration unchanged");
        }
        Ok::<_, anyhow::Error>(String::from_utf8(bytes)?.trim().to_string())
    })
    .await;
    match result {
        Ok(Ok(output)) => {
            #[cfg(unix)]
            {
                group.0 = None;
            }
            Ok(output)
        }
        other => {
            #[cfg(unix)]
            drop(group);
            let _ = child.kill().await;
            match other {
                Ok(Err(error)) => Err(error),
                _ => bail!("browser diagnostic timed out; configuration unchanged"),
            }
        }
    }
}

async fn check_node(node: &Path) -> Result<()> {
    let value = diagnostic(node, &["--version".into()], &Default::default()).await?;
    if value
        .strip_prefix('v')
        .and_then(|value| value.split('.').next())
        .and_then(|v| v.parse::<u32>().ok())
        .is_none_or(|v| v < 20)
    {
        bail!("Node 20 or later is required by the pinned Playwright runtime");
    }
    Ok(())
}
async fn check_stock(node: &Path, server: &Path) -> Result<()> {
    let value = diagnostic(
        node,
        &[
            server
                .to_str()
                .context("backend path must be UTF-8")?
                .into(),
            "--version".into(),
        ],
        &Default::default(),
    )
    .await?;
    if value != format!("Version {VERSION}") {
        bail!("unsupported browser backend version; expected @playwright/mcp {VERSION}");
    }
    Ok(())
}

fn bundled_skill(text: &str) -> bool {
    text == SKILL || format!("{:x}", Sha256::digest(text.as_bytes())) == OLD_SKILL_SHA256
}

#[cfg(test)]
fn enable(dirs: &ProductDirs, node: &Path, server: &Path) -> Result<()> {
    let before = read_optional(&config_path(dirs))?;
    enable_prepared(
        dirs,
        &origin::Prepared::stock(node.into(), server.into()),
        &before,
    )
}

fn enable_prepared(
    dirs: &ProductDirs,
    plan: &origin::Prepared,
    expected: &Option<String>,
) -> Result<()> {
    let _lock = lock_mcp_setup(dirs)?;
    let path = config_path(dirs);
    let before = read_optional(&path)?;
    if &before != expected {
        bail!("browser MCP configuration changed during diagnosis; retry without overwriting it");
    }
    let prior = owned_entry(before.as_deref())?;
    if !plan.scoped() && !plan.remove_scope {
        origin::describe(prior.as_ref(), dirs)?;
        if prior
            .as_ref()
            .is_some_and(|entry| entry.get("originScope").is_some())
        {
            bail!("explicit --unrestricted-origins is required to remove saved scope");
        }
    }
    let skill = skill_path(dirs);
    let prior_skill = read_optional(&skill)?;
    if prior_skill.as_deref().is_some_and(|s| !bundled_skill(s)) {
        bail!("browser-use skill has local changes; preserve or rename it before enabling");
    }
    let args = plan.args(dirs)?;
    let text = merge_stdio_mcp_server_into_text(
        before.as_deref(),
        SERVER,
        plan.node.to_str().context("Node path must be UTF-8")?,
        &args,
    )?;
    let mut root: Value = serde_json::from_str(&text)?;
    let entry = &mut root["mcpServers"][SERVER];
    if let Some(prior) = prior.as_ref() {
        *entry = prior.clone();
    }
    entry["command"] = json!(plan.node.to_str().context("Node path must be UTF-8")?);
    entry["args"] = json!(args);
    entry["managedBy"] = json!(OWNER);
    entry["disabled"] = json!(false);
    entry["trust"] = json!(false);
    entry["autoApprove"] = json!([]);
    if entry.get("allowedTools").is_some() && entry.get("allowed_tools").is_some() {
        bail!("duplicate browser tool restrictions");
    }
    let tools: Vec<String> = match entry
        .get("allowedTools")
        .or_else(|| entry.get("allowed_tools"))
    {
        Some(value) if !value.is_null() => serde_json::from_value::<Vec<String>>(value.clone())
            .context("invalid saved browser tool restrictions")?
            .into_iter()
            .filter(|name| ALLOWED_TOOLS.contains(&name.as_str()))
            .collect(),
        _ => ALLOWED_TOOLS.iter().map(|name| (*name).into()).collect(),
    };
    entry["allowedTools"] = json!(tools);
    entry
        .as_object_mut()
        .context("browser entry must be an object")?
        .remove("allowed_tools");
    entry
        .as_object_mut()
        .context("browser entry must be an object")?
        .remove("auto_approve");
    let mut required = origin::requirements(prior.as_ref())?;
    if plan.scoped() {
        if required
            .get(origin::CAPABILITY)
            .is_some_and(|version| *version != 1)
        {
            bail!("saved browser capability conflicts with supported scope version 1");
        }
        required.insert(origin::CAPABILITY.into(), 1);
        entry["originScope"] = json!(1);
    } else if plan.remove_scope {
        required.remove(origin::CAPABILITY);
        entry
            .as_object_mut()
            .context("browser entry must be an object")?
            .remove("originScope");
    }
    entry
        .as_object_mut()
        .context("browser entry must be an object")?
        .remove("required_experimental_capabilities");
    if required.is_empty() {
        entry
            .as_object_mut()
            .context("browser entry must be an object")?
            .remove("requiredExperimentalCapabilities");
    } else {
        entry["requiredExperimentalCapabilities"] = json!(required);
    }
    entry["timeout_ms"] = json!(30_000);
    // Do not retain environment overrides that could attach a personal profile or CDP endpoint.
    entry["env"] = plan.env(dirs)?;
    atomcode_capabilities::mcp::parse_mcp_servers(
        &json!({"mcpServers":{SERVER:entry.clone()}}).to_string(),
        atomcode_capabilities::mcp::McpConfigSource::User,
    )?;
    if !plan.scoped() {
        prepare_artifacts(dirs)?;
    }
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
    let _lock = lock_mcp_setup(dirs)?;
    let before = read_optional(&path)?;
    owned_entry(before.as_deref())?.context("managed browser configuration disappeared")?;
    let after = set_mcp_server_disabled_in_text(
        before.as_deref().context("missing configuration")?,
        SERVER,
        true,
    )?;
    write_checked(&path, &before, &after)?;
    let skill = skill_path(dirs);
    if read_optional(&skill)?.as_deref().is_some_and(bundled_skill) {
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

    #[test]
    fn origins_are_exact_canonical_bounded_and_do_not_accept_encoded_policy_bypasses() {
        assert_eq!(
            origin::normalize(vec![
                "https://EXAMPLE.test:443/".into(),
                "https://example.test".into()
            ])
            .unwrap(),
            vec!["https://example.test"]
        );
        for bad in [
            "*",
            "https://*.test",
            "file:///tmp",
            "http://a.test/path",
            "http://a.test/?",
            "http://a.test/#",
            "http://@a.test",
            "http:////a.test",
            "http://user:pass@a.test",
            " http://a.test",
            "http://a.test\n",
            "http:\\a.test",
        ] {
            assert!(origin::normalize(vec![bad.into()]).is_err(), "{bad}");
        }
        assert!(origin::normalize(vec!["https://example.test".into(); 257]).is_err());
        assert!(origin::normalize(Vec::new()).unwrap().is_empty());
    }

    #[test]
    fn old_bundled_browser_guidance_upgrades_but_local_edits_do_not() {
        let old = include_str!("../tests/fixtures/browser-use-v1.md");
        assert!(bundled_skill(old));
        assert_ne!(old, SKILL);
        let (_temp, dirs) = tree();
        setup(&dirs);
        put(&skill_path(&dirs), old);
        setup(&dirs);
        assert_eq!(
            read_optional(&skill_path(&dirs)).unwrap().as_deref(),
            Some(SKILL)
        );
        put(&skill_path(&dirs), &format!("{old}\nLocal change"));
        assert!(enable(&dirs, Path::new("/node"), Path::new("/server")).is_err());
        put(&skill_path(&dirs), old);
        disable(&dirs).unwrap();
        assert!(!skill_path(&dirs).exists());
    }

    #[test]
    fn changed_snapshot_and_ambiguous_entries_are_refused() {
        let (_temp, dirs) = tree();
        setup(&dirs);
        let before = read_optional(&config_path(&dirs)).unwrap();
        let changed = before.as_ref().unwrap().replace("30000", "40000");
        put(&config_path(&dirs), &changed);
        let plan = origin::Prepared::stock("/test/node".into(), "/test/server.js".into());
        assert!(enable_prepared(&dirs, &plan, &before).is_err());
        assert_eq!(
            read_optional(&config_path(&dirs)).unwrap().unwrap(),
            changed
        );
        let duplicate = json!({"servers":{SERVER:{"managedBy":OWNER}},"mcpServers":{SERVER:{"managedBy":OWNER}}});
        assert!(owned_entry(Some(&duplicate.to_string())).is_err());
    }

    #[cfg(unix)]
    fn fake_node(temp: &Path, report: &Value, major: u32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let node = temp.join("node fixture");
        std::fs::write(&node,format!("#!/bin/sh\nif [ \"$#\" = 1 ] && [ \"$1\" = --version ]; then echo v{major}.0.0; exit 0; fi\nfor arg do if [ \"$arg\" = --doctor ]; then printf '%s' '{report}'; exit 0; fi; done\necho 'Version 0.0.85'\n")).unwrap();
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o700)).unwrap();
        node
    }
    #[cfg(unix)]
    fn scope_report(count: usize) -> Value {
        json!({"version":1,"kind":"doctor","browser_launched":false,"capability":origin::CAPABILITY,
            "mcp":VERSION,"playwright":"1.66.0-alpha-2026-11-09","allowed_origin_count":count,
            "downloads":"deny-artifacts","uploads":"deny","full_cdp_access":"deny"})
    }
    #[cfg(unix)]
    fn scope_paths(temp: &Path) -> (PathBuf, PathBuf) {
        let script = temp.join("origin backend.cjs");
        std::fs::write(&script, "fixture").unwrap();
        let root = temp.join("installed package");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("cli.js"), "fixture").unwrap();
        (script, root)
    }
    #[cfg(unix)]
    fn scoped_args(script: &Path, root: &Path) -> OriginArgs {
        OriginArgs {
            origin_backend: Some(script.into()),
            backend_root: Some(root.into()),
            allowed_origins: vec!["https://EXAMPLE.test:443/".into()],
            ..Default::default()
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn scoped_enable_preserves_policy_other_contracts_and_narrowed_tools_until_explicit_unrestriction(
    ) {
        tokio::time::resume(); // Real subprocess diagnostics.
        let (temp, dirs) = tree();
        let (script, root) = scope_paths(temp.path());
        let node = fake_node(temp.path(), &scope_report(1), 22);
        execute(
            &dirs,
            BrowserCli::Enable {
                server: None,
                node: Some(node.clone()),
                scope: scoped_args(&script, &root),
            },
        )
        .await
        .unwrap();
        let read = || -> Value {
            serde_json::from_str(&read_optional(&config_path(&dirs)).unwrap().unwrap()).unwrap()
        };
        let mut saved = read();
        let entry = &mut saved["mcpServers"][SERVER];
        assert_eq!(entry["originScope"], 1);
        assert_eq!(
            entry["requiredExperimentalCapabilities"][origin::CAPABILITY],
            1
        );
        let policy: Value =
            serde_json::from_str(entry["env"][origin::ENV].as_str().unwrap()).unwrap();
        assert_eq!(
            policy,
            json!({"version":1,"origins":["https://example.test"]})
        );
        entry["requiredExperimentalCapabilities"]["vendor/another"] = json!(2);
        entry["allowedTools"] = json!(["browser_snapshot"]);
        entry["env"]["PLAYWRIGHT_MCP_CDP_ENDPOINT"] = json!("http://must-not-attach");
        put(&config_path(&dirs), &saved.to_string());
        execute(
            &dirs,
            BrowserCli::Enable {
                server: None,
                node: None,
                scope: Default::default(),
            },
        )
        .await
        .unwrap();
        let configured = read();
        let entry = &configured["mcpServers"][SERVER];
        assert_eq!(
            entry["requiredExperimentalCapabilities"],
            json!({origin::CAPABILITY:1,"vendor/another":2})
        );
        assert_eq!(entry["allowedTools"], json!(["browser_snapshot"]));
        assert_eq!(entry["env"]["PLAYWRIGHT_MCP_CDP_ENDPOINT"], "");
        assert_eq!(entry["env"][origin::ENV], json!(policy.to_string()));
        let before = read_optional(&config_path(&dirs)).unwrap();
        execute(
            &dirs,
            BrowserCli::Doctor {
                server: None,
                node: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(read_optional(&config_path(&dirs)).unwrap(), before);
        assert!(execute(&dirs, BrowserCli::Status)
            .await
            .unwrap()
            .contains("https://example.test"));
        disable(&dirs).unwrap();
        execute(
            &dirs,
            BrowserCli::Enable {
                server: None,
                node: None,
                scope: Default::default(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            read()["mcpServers"][SERVER]["env"][origin::ENV],
            json!(policy.to_string())
        );
        fake_node(temp.path(), &scope_report(0), 22);
        execute(
            &dirs,
            BrowserCli::Enable {
                server: None,
                node: None,
                scope: OriginArgs {
                    deny_all_origins: true,
                    ..Default::default()
                },
            },
        )
        .await
        .unwrap();
        assert!(execute(&dirs, BrowserCli::Status)
            .await
            .unwrap()
            .contains("deny all origins"));
        execute(
            &dirs,
            BrowserCli::Enable {
                server: None,
                node: None,
                scope: OriginArgs {
                    unrestricted_origins: true,
                    ..Default::default()
                },
            },
        )
        .await
        .unwrap();
        let saved = read();
        let entry = &saved["mcpServers"][SERVER];
        assert!(entry.get("originScope").is_none());
        assert!(entry["env"].get(origin::ENV).is_none());
        assert_eq!(
            entry["requiredExperimentalCapabilities"],
            json!({"vendor/another":2})
        );
        assert_eq!(entry["allowedTools"], json!(["browser_snapshot"]));
        assert_eq!(
            entry["args"][0],
            root.join("cli.js")
                .canonicalize()
                .unwrap()
                .to_str()
                .unwrap()
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn unsupported_doctor_and_old_node_never_create_product_configuration() {
        tokio::time::resume();
        let (temp, dirs) = tree();
        let (script, root) = scope_paths(temp.path());
        for field in [
            "version",
            "kind",
            "browser_launched",
            "capability",
            "mcp",
            "playwright",
            "allowed_origin_count",
            "downloads",
            "uploads",
            "full_cdp_access",
        ] {
            let mut report = scope_report(1);
            report.as_object_mut().unwrap().remove(field);
            let node = fake_node(temp.path(), &report, 22);
            assert!(
                execute(
                    &dirs,
                    BrowserCli::Enable {
                        server: None,
                        node: Some(node),
                        scope: scoped_args(&script, &root)
                    }
                )
                .await
                .is_err(),
                "{field}"
            );
            assert!(!dirs.user().exists());
        }
        let node = fake_node(temp.path(), &scope_report(1), 18);
        assert!(execute(
            &dirs,
            BrowserCli::Enable {
                server: None,
                node: Some(node),
                scope: scoped_args(&script, &root)
            }
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("Node 20"));
        assert!(!dirs.user().exists());
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn malformed_saved_scope_and_conflicting_versions_do_not_fall_back_to_stock() {
        tokio::time::resume();
        let (temp, dirs) = tree();
        let (script, root) = scope_paths(temp.path());
        let node = fake_node(temp.path(), &scope_report(1), 22);
        execute(
            &dirs,
            BrowserCli::Enable {
                server: None,
                node: Some(node),
                scope: scoped_args(&script, &root),
            },
        )
        .await
        .unwrap();
        let initial = read_optional(&config_path(&dirs)).unwrap().unwrap();
        for corrupt in ["policy", "contract", "marker", "args"] {
            let mut value: Value = serde_json::from_str(&initial).unwrap();
            let entry = &mut value["mcpServers"][SERVER];
            match corrupt {
                "policy" => entry["env"][origin::ENV] = json!("{invalid"),
                "contract" => {
                    entry["requiredExperimentalCapabilities"][origin::CAPABILITY] = json!(2)
                }
                "marker" => {
                    entry.as_object_mut().unwrap().remove("originScope");
                }
                _ => entry["args"] = json!(["/different/backend"]),
            }
            let before = value.to_string();
            put(&config_path(&dirs), &before);
            assert!(
                execute(
                    &dirs,
                    BrowserCli::Enable {
                        server: None,
                        node: None,
                        scope: Default::default()
                    }
                )
                .await
                .is_err(),
                "{corrupt}"
            );
            assert_eq!(read_optional(&config_path(&dirs)).unwrap().unwrap(), before);
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn diagnostic_timeout_kills_owned_descendants_without_late_effects() {
        use std::os::unix::fs::PermissionsExt;
        tokio::time::resume();
        let (temp, dirs) = tree();
        let executable = temp.path().join("late diagnostic");
        std::fs::write(
            &executable,
            "#!/bin/sh\nsleep 1; printf bad > \"$0.effect\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(diagnostic_with_deadline(
            &executable,
            &[],
            &Default::default(),
            Duration::from_millis(100)
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("timed out"));
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(!temp.path().join("late diagnostic.effect").exists());
        assert!(!dirs.user().exists());
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn cancelling_diagnosis_kills_its_owned_process_group_without_config_writes() {
        use std::os::unix::fs::PermissionsExt;
        tokio::time::resume();
        let (temp, dirs) = tree();
        let executable = temp.path().join("cancelled diagnostic");
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf started > \"$0.started\"\nsleep 1; printf bad > \"$0.effect\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let task =
            tokio::spawn(async move { diagnostic(&executable, &[], &Default::default()).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !temp.path().join("cancelled diagnostic.started").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(!temp.path().join("cancelled diagnostic.effect").exists());
        assert!(!dirs.user().exists());
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
                node: None,
                scope: Default::default(),
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
        assert_eq!(entry["env"]["PLAYWRIGHT_MCP_PING_TIMEOUT_MS"], "0");
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

    #[tokio::test]
    async fn desktop_and_browser_setup_share_the_same_document_lock() {
        let (_t, dirs) = tree();
        setup(&dirs);
        let mut root: Value =
            serde_json::from_str(&read_optional(&config_path(&dirs)).unwrap().unwrap()).unwrap();
        root["mcpServers"]["computer-use"] =
            json!({"command":"unused", "managedBy":"atomcode-computer-use-v1"});
        put(&config_path(&dirs), &root.to_string());
        let before = read_optional(&config_path(&dirs)).unwrap();
        let lock = lock_mcp_setup(&dirs).unwrap();
        assert!(enable(&dirs, Path::new("node"), Path::new("server")).is_err());
        assert!(disable(&dirs).is_err());
        assert!(
            crate::computer_cmd::execute(&dirs, crate::computer_cmd::ComputerCli::Disable)
                .await
                .is_err()
        );
        assert_eq!(read_optional(&config_path(&dirs)).unwrap(), before);
        drop(lock);
        disable(&dirs).unwrap();
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
