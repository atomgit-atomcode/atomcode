//! Opt-in bundled Huawei Cloud MCP. Installation is local; live ownership stays in coding.
use anyhow::{bail, Context, Result};
use atomcode_capabilities::mcp::merge_stdio_mcp_server_into_text;
use atomcode_capabilities::ProductDirs;
use clap::Subcommand;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

include!("../assets/huaweicloud/files.rs");
const SERVER: &str = "huaweicloud";
const OWNER: &str = "atomcode-huaweicloud-v1";
const SKILL: &str = include_str!("../assets/huaweicloud/skill/SKILL.md");

#[derive(Subcommand)]
pub enum CloudCli {
    /// Enable the bundled Huawei Cloud sandbox MCP (requires Node >=22.19 and npm).
    Enable,
    /// Disable the integration for new sessions; does not delete cloud resources.
    Disable,
    /// Show local integration state without accessing cloud credentials.
    Status,
    /// Check Node, npm and local registration. Does not contact Huawei Cloud.
    Doctor,
}

pub fn handle(command: CloudCli) -> Result<()> {
    let dirs = atomcode_coding::config::product_dirs_from_env();
    match command {
        CloudCli::Enable => {
            check_node()?;
            enable(&dirs, install_dependencies)?;
            println!("Huawei Cloud sandbox integration enabled. Restart the session; discover tools with mcp_search (server: huaweicloud).\nCredentials stay outside chat: configure HW_ACCESS_KEY/HW_SECRET_KEY in your launcher environment or use the upstream Devkit's local auth init. Cloud actions retain native MCP approval.");
        }
        CloudCli::Disable => {
            disable(&dirs)?;
            println!("Huawei Cloud disabled for new sessions. Restart or disable the live server in /mcp. No cloud resources were removed.");
        }
        CloudCli::Status => status(&dirs)?,
        CloudCli::Doctor => {
            check_node()?;
            let npm = npm_command()
                .arg("--version")
                .output()
                .context("npm is missing; install Node.js >=22.19 with npm")?;
            if !npm.status.success() {
                bail!("npm is unavailable; repair your Node.js installation");
            }
            println!("Node >=22.19: OK\nnpm: OK\nCredentials: not inspected (configure locally, never in chat)");
            status(&dirs)?;
            if root(&dirs).exists() {
                let checked = Command::new("node").args(["--input-type=module", "--eval",
                    "await import('./adapter.mjs'); await import('./vendor/src/tools.mjs'); await import('undici');"])
                    .current_dir(root(&dirs)).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status()?;
                if !checked.success() {
                    bail!("Huawei Cloud runtime dependencies are incomplete; remove the integration cache and run atomcode cloud enable again");
                }
                println!("Bundled adapter and dependencies: OK");
            }
        }
    }
    Ok(())
}

fn check_node() -> Result<()> {
    let out = Command::new("node")
        .arg("--version")
        .output()
        .context("Node.js is missing; install Node.js >=22.19 with npm")?;
    let version = String::from_utf8_lossy(&out.stdout);
    let mut parts = version.trim().trim_start_matches('v').split('.');
    let major = parts
        .next()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    let minor = parts
        .next()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    if !out.status.success() || !(major > 22 || (major == 22 && minor >= 19)) {
        bail!("Huawei Cloud requires Node.js >=22.19; update your local Node.js installation");
    }
    Ok(())
}

fn npm_command() -> Command {
    Command::new(if cfg!(windows) { "npm.cmd" } else { "npm" })
}

fn install_dependencies(root: &Path) -> Result<()> {
    let status = npm_command()
        .args([
            "ci",
            "--omit=dev",
            "--ignore-scripts",
            "--no-audit",
            "--no-fund",
            "--fetch-timeout=30000",
            "--fetch-retries=1",
        ])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("could not run npm; install Node.js >=22.19 with npm")?;
    if !status.success() {
        bail!("Huawei Cloud dependency installation failed; check npm registry/network access and retry. Integration was not enabled.");
    }
    Ok(())
}

fn revision() -> String {
    let mut hash = Sha256::new();
    for (path, bytes) in FILES {
        hash.update(path.as_bytes());
        hash.update([0]);
        hash.update(bytes);
    }
    format!("{:x}", hash.finalize())[..16].to_string()
}
fn root(dirs: &ProductDirs) -> PathBuf {
    dirs.user()
        .join("integrations")
        .join(format!("huaweicloud-{}", revision()))
}
fn skill_path(dirs: &ProductDirs) -> PathBuf {
    dirs.user().join("skills/huaweicloud-preview/SKILL.md")
}
fn read_optional(path: &Path) -> Result<Option<String>> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if !meta.file_type().is_file() => {
            bail!("refusing non-regular file: {}", path.display())
        }
        Ok(_) => Ok(Some(
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}
fn owned_entry(text: Option<&str>) -> Result<Option<Value>> {
    let value: Value = match text {
        Some(text) => serde_json::from_str(text)
            .context("MCP config is invalid JSON; repair it before enabling Huawei Cloud")?,
        None => json!({}),
    };
    let entry = value
        .get("mcpServers")
        .and_then(|s| s.get(SERVER))
        .or_else(|| value.get("servers").and_then(|s| s.get(SERVER)));
    if let Some(entry) = entry {
        if entry.get("managedBy").and_then(Value::as_str) != Some(OWNER) {
            bail!("MCP server 'huaweicloud' already belongs to your configuration; rename or remove it explicitly before enabling the bundled integration");
        }
    }
    Ok(entry.cloned())
}
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("missing parent directory")?;
    std::fs::create_dir_all(parent)?;
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if !meta.file_type().is_file() {
            bail!("refusing non-regular file: {}", path.display());
        }
    }
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

fn enable(dirs: &ProductDirs, install: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    let config = dirs.user().join("mcp.json");
    let before = read_optional(&config)?;
    owned_entry(before.as_deref())?;
    let skill = skill_path(dirs);
    let prior_skill = read_optional(&skill)?;
    if prior_skill.as_deref().is_some_and(|text| text != SKILL) {
        bail!("huaweicloud-preview skill already exists with local changes; preserve or rename it before enabling");
    }
    let dest = root(dirs);
    let parent = dest.parent().unwrap();
    std::fs::create_dir_all(parent)?;
    if !dest.exists() {
        let stage = tempfile::Builder::new()
            .prefix(".huaweicloud-")
            .tempdir_in(parent)?;
        for (relative, bytes) in FILES {
            let file = stage.path().join(relative);
            std::fs::create_dir_all(file.parent().unwrap())?;
            std::fs::write(file, bytes)?;
        }
        install(stage.path())?;
        if !stage
            .path()
            .join("node_modules/undici/package.json")
            .is_file()
        {
            bail!("Huawei Cloud dependencies were not installed; integration was not enabled");
        }
        // Publish only a complete installation; a failure leaves existing config unchanged.
        std::fs::rename(stage.path(), &dest)?;
    } else {
        if !std::fs::symlink_metadata(&dest)?.file_type().is_dir() {
            bail!("integration cache is not a regular directory");
        }
        for (relative, bytes) in FILES {
            let file = dest.join(relative);
            if !std::fs::symlink_metadata(&file)?.file_type().is_file()
                || std::fs::read(file)? != *bytes
            {
                bail!("Huawei Cloud integration cache was modified; preserve it and remove the cache directory before retrying");
            }
        }
    }
    if !dest.join("node_modules/undici/package.json").is_file() {
        bail!("Huawei Cloud cache dependencies are missing; remove the integration cache and enable again");
    }
    let adapter = dest
        .join("adapter.mjs")
        .to_str()
        .context("integration path is not UTF-8")?
        .to_string();
    let merged = merge_stdio_mcp_server_into_text(before.as_deref(), SERVER, "node", &[adapter])?;
    let mut value: Value = serde_json::from_str(&merged)?;
    let entry = &mut value["mcpServers"][SERVER];
    entry["managedBy"] = json!(OWNER);
    entry["disabled"] = json!(false);
    entry["trust"] = json!(false);
    entry["autoApprove"] = json!([]);
    entry["timeout_ms"] = json!(300000);
    // Installation can take a while: do not overwrite edits made in the meantime.
    if read_optional(&config)? != before {
        bail!("MCP config changed during installation; retry enabling");
    }
    if read_optional(&skill)? != prior_skill {
        bail!(
            "Huawei Cloud routing skill changed during installation; preserve it before retrying"
        );
    }
    atomic_write(&skill, SKILL.as_bytes())?;
    if let Err(error) = atomic_write(
        &config,
        format!("{}\n", serde_json::to_string_pretty(&value)?).as_bytes(),
    ) {
        if prior_skill.is_none() && read_optional(&skill).ok().flatten().as_deref() == Some(SKILL) {
            let _ = std::fs::remove_file(skill);
        }
        return Err(error);
    }
    Ok(())
}

fn disable(dirs: &ProductDirs) -> Result<()> {
    let path = dirs.user().join("mcp.json");
    let before = read_optional(&path)?;
    if owned_entry(before.as_deref())?.is_none() {
        println!("Huawei Cloud is not enabled.");
        return Ok(());
    }
    let text = atomcode_capabilities::mcp::set_mcp_server_disabled_in_text(
        before.as_deref().unwrap(),
        SERVER,
        true,
    )?;
    atomic_write(&path, text.as_bytes())?;
    let skill = skill_path(dirs);
    if read_optional(&skill)?.as_deref() == Some(SKILL) {
        std::fs::remove_file(skill)?;
    }
    Ok(())
}
fn status(dirs: &ProductDirs) -> Result<()> {
    let text = read_optional(&dirs.user().join("mcp.json"))?;
    let entry = owned_entry(text.as_deref())?;
    let state = match entry.as_ref() {
        None => "not enabled",
        Some(entry) if entry["disabled"] == true => "disabled",
        Some(_) => "enabled",
    };
    let grants = entry.as_ref().is_some_and(|entry| {
        entry["trust"] == true
            || entry["autoApprove"]
                .as_array()
                .is_some_and(|items| !items.is_empty())
    });
    let approval = if grants {
        "user-configured trust or automatic approval grants are present"
    } else {
        "no server trust or automatic approval grants"
    };
    println!("Huawei Cloud: {state}\nScope: user; sandbox preview + diagnostics\nDiscovery: existing MCP search policy (default on)\nApproval: {approval}\nBundled upstream: 3ab9162868c17534544183c667abbd37113eb32e");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fake_install(root: &Path) -> Result<()> {
        let dependency = root.join("node_modules/undici");
        std::fs::create_dir_all(&dependency)?;
        std::fs::write(dependency.join("package.json"), "{}")?;
        Ok(())
    }
    fn dirs(root: &Path) -> ProductDirs {
        ProductDirs::new(root, ".test-product")
    }
    #[test]
    fn enable_disable_preserve_other_servers_and_never_grant_trust() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = dirs(temp.path());
        let path = dirs.user().join("mcp.json");
        std::fs::write(
            &path,
            r#"{"mcpServers":{"other":{"command":"other","env":{"TOKEN":"kept"}}},"custom":true}"#,
        )
        .unwrap();
        enable(&dirs, fake_install).unwrap();
        let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["mcpServers"]["other"]["env"]["TOKEN"], "kept");
        assert_eq!(value["mcpServers"][SERVER]["trust"], false);
        assert_eq!(value["mcpServers"][SERVER]["autoApprove"], json!([]));
        let parsed = atomcode_capabilities::mcp::parse_mcp_servers(
            &serde_json::to_string(&value).unwrap(),
            atomcode_capabilities::mcp::McpConfigSource::User,
        )
        .unwrap();
        let server = parsed.iter().find(|server| server.name == SERVER).unwrap();
        assert!(!server.trust);
        assert!(matches!(
            server.config,
            atomcode_capabilities::mcp::McpTransportConfig::Stdio {
                timeout_ms: Some(300000),
                ..
            }
        ));
        assert!(value["mcpServers"][SERVER].get("env").is_none());
        assert_eq!(std::fs::read_to_string(skill_path(&dirs)).unwrap(), SKILL);
        disable(&dirs).unwrap();
        assert!(!skill_path(&dirs).exists());
        let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["mcpServers"][SERVER]["disabled"], true);
        enable(&dirs, |_| panic!("cached installation needs no network")).unwrap();
    }
    #[test]
    fn failed_install_and_user_owned_server_leave_config_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = dirs(temp.path());
        let path = dirs.user().join("mcp.json");
        assert!(enable(&dirs, |_| anyhow::bail!("offline")).is_err());
        assert!(!path.exists());
        assert!(!skill_path(&dirs).exists());
        assert!(!root(&dirs).exists());
        let original = r#"{"mcpServers":{"huaweicloud":{"command":"mine"}}}"#;
        std::fs::write(&path, original).unwrap();
        assert!(enable(&dirs, |_| panic!("must not install")).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert!(disable(&dirs).is_err());
    }
    #[test]
    fn concurrent_config_edits_are_not_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = dirs(temp.path());
        let path = dirs.user().join("mcp.json");
        let updated = r#"{"mcpServers":{"new":{"command":"new"}}}"#;
        assert!(enable(&dirs, |root| {
            fake_install(root)?;
            std::fs::write(&path, updated)?;
            Ok(())
        })
        .is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), updated);
        assert!(!skill_path(&dirs).exists());
    }
    #[test]
    fn a_skill_created_during_installation_is_not_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = dirs(temp.path());
        let skill = skill_path(&dirs);
        assert!(enable(&dirs, |root| {
            fake_install(root)?;
            std::fs::create_dir_all(skill.parent().unwrap())?;
            std::fs::write(&skill, "user-created guidance")?;
            Ok(())
        })
        .is_err());
        assert_eq!(
            std::fs::read_to_string(&skill).unwrap(),
            "user-created guidance"
        );
        assert!(!dirs.user().join("mcp.json").exists());
    }
}
