//! MCP configuration loading.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

/// MCP server transport configuration.
#[derive(Debug, Clone)]
pub enum McpTransportConfig {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        timeout_ms: Option<u64>,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
        timeout_ms: Option<u64>,
    },
}

/// MCP server configuration.
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub name: String,
    pub disabled: bool,
    pub config: McpTransportConfig,
}

/// Configuration source for a server.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum McpConfigSource {
    Project,
    User,
}

/// Raw MCP config file format (for deserialization).
#[derive(Debug, Deserialize)]
struct McpConfigFile {
    /// JSON key `mcpServers`（与 Cursor 等工具一致）；`servers` 仍可作为别名读取旧配置。
    #[serde(default, rename = "mcpServers", alias = "servers")]
    mcp_servers: BTreeMap<String, McpServerEntry>,
}

#[derive(Debug, Deserialize)]
struct McpServerEntry {
    /// Ignored for transport selection (stdio vs HTTP is inferred from `command` vs `url`).
    /// Accepted so configs copied from Claude / Cursor validate.
    #[serde(default, rename = "type")]
    _transport_hint: Option<String>,
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Option<Vec<String>>,
    #[serde(default)]
    env: Option<BTreeMap<String, String>>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    headers: Option<BTreeMap<String, String>>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

use serde::Deserialize;

/// Load and merge MCP configurations from project and user levels.
///
/// Project config (`.mcp.json` in project root) overrides user config
/// (`ATOMCODE_HOME/mcp.json`) for servers with the same name.
pub fn load_mcp_config(project_dir: &Path) -> Result<Vec<McpServerConfig>> {
    let user_config = load_config_file(
        &crate::config::Config::config_dir().join("mcp.json"),
        McpConfigSource::User,
    )
    .unwrap_or_default();

    let project_config = load_config_file(
        &project_dir.join(".mcp.json"),
        McpConfigSource::Project,
    )
    .unwrap_or_default();

    // Merge: project overrides user
    let mut merged: BTreeMap<String, McpServerConfig> = BTreeMap::new();

    for config in user_config {
        merged.insert(config.name.clone(), config);
    }

    for config in project_config {
        merged.insert(config.name.clone(), config);
    }

    Ok(merged.into_values().filter(|c| !c.disabled).collect())
}

fn load_config_file(path: &Path, _source: McpConfigSource) -> Result<Vec<McpServerConfig>> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read MCP config from {}", path.display()))?;

    let raw: McpConfigFile = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse MCP config from {}", path.display()))?;

    let mut configs = Vec::new();

    for (name, entry) in raw.mcp_servers {
        let config = server_entry_to_config(&name, entry)?;
        configs.push(config);
    }

    Ok(configs)
}

fn server_entry_to_config(name: &str, entry: McpServerEntry) -> Result<McpServerConfig> {
    let transport = if let Some(command) = entry.command {
        McpTransportConfig::Stdio {
            command: expand_tilde(&expand_env_vars(&command)),
            args: entry
                .args
                .unwrap_or_default()
                .into_iter()
                .map(|a| expand_tilde(&expand_env_vars(&a)))
                .collect(),
            env: entry
                .env
                .unwrap_or_default()
                .into_iter()
                .map(|(k, v)| (k, expand_env_vars(&v)))
                .collect(),
            timeout_ms: entry.timeout_ms,
        }
    } else if let Some(url) = entry.url {
        McpTransportConfig::Http {
            url: expand_tilde(&expand_env_vars(&url)),
            headers: entry
                .headers
                .unwrap_or_default()
                .into_iter()
                .map(|(k, v)| (k, expand_env_vars(&v)))
                .collect(),
            timeout_ms: entry.timeout_ms,
        }
    } else {
        bail!(
            "MCP server '{}' must have either 'command' (stdio) or 'url' (http)",
            name
        );
    };

    Ok(McpServerConfig {
        name: name.to_string(),
        disabled: entry.disabled,
        config: transport,
    })
}

fn collect_merged_mcp_server_maps(root: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(Value::Object(m)) = root.get("servers") {
        for (k, v) in m {
            out.insert(k.clone(), v.clone());
        }
    }
    if let Some(Value::Object(m)) = root.get("mcpServers") {
        for (k, v) in m {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

/// Add or replace a **stdio** MCP server entry in a JSON config file (`.mcp.json` or `~/.atomcode/mcp.json`).
///
/// Merges existing `servers` and `mcpServers` maps, then writes a single `mcpServers` object (drops the legacy
/// `servers` key). Other top-level JSON keys are preserved.
pub fn merge_stdio_mcp_server_into_json_file(
    path: &Path,
    server_key: &str,
    program: &str,
    args: &[String],
) -> Result<()> {
    if server_key.is_empty() {
        bail!("MCP server name must not be empty");
    }
    if program.is_empty() {
        bail!("command must not be empty");
    }

    let mut root: Value = if path.exists() {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read MCP config from {}", path.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("Failed to parse MCP config JSON from {}", path.display()))?
    } else {
        json!({})
    };

    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("MCP config root must be a JSON object"))?;

    let mut servers = collect_merged_mcp_server_maps(root_obj);
    let entry = json!({
        "command": program,
        "args": args,
    });
    servers.insert(server_key.to_string(), entry);
    root_obj.insert("mcpServers".to_string(), Value::Object(servers));
    root_obj.remove("servers");

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "Failed to create parent directory for {}",
                    path.display()
                )
            })?;
        }
    }

    let text = serde_json::to_string_pretty(&root).context("Failed to serialize MCP config")?;
    std::fs::write(path, format!("{text}\n"))
        .with_context(|| format!("Failed to write MCP config to {}", path.display()))?;

    Ok(())
}

/// Expand environment variables in a string.
///
/// Supports `${VAR}` and `${VAR:-default}` syntax.
fn expand_env_vars(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            i += 2; // skip ${

            let mut var_name = String::new();
            let mut default = String::new();
            let mut has_default = false;

            while i < bytes.len() && bytes[i] != b'}' {
                if bytes[i] == b':' && !has_default && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
                    i += 2; // skip :-
                    has_default = true;
                    continue;
                }
                if has_default {
                    default.push(bytes[i] as char);
                } else {
                    var_name.push(bytes[i] as char);
                }
                i += 1;
            }
            if i < bytes.len() {
                i += 1; // skip }
            }

            let value = std::env::var(&var_name).unwrap_or_else(|_| {
                if has_default {
                    default
                } else {
                    String::new()
                }
            });
            result.push_str(&value);
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }

    result
}

/// Expand a leading `~` (home) in a string.
///
/// - `~/path` → `$HOME/path`
/// - `~` → `$HOME`
/// - Other forms (e.g. `~user/...`) are left unchanged.
fn expand_tilde(s: &str) -> String {
    if s == "~" {
        return dirs::home_dir()
            .map(|h| h.to_string_lossy().to_string())
            .unwrap_or_else(|| s.to_string());
    }
    let Some(rest) = s.strip_prefix("~/") else {
        return s.to_string();
    };
    let Some(home) = dirs::home_dir() else {
        return s.to_string();
    };
    home.join(rest).to_string_lossy().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn test_expand_env_vars_simple() {
        std::env::set_var("TEST_VAR", "test_value");
        let result = expand_env_vars("${TEST_VAR}");
        assert_eq!(result, "test_value");
    }

    #[test]
    fn test_expand_env_vars_with_default() {
        std::env::remove_var("NONEXISTENT_VAR");
        let result = expand_env_vars("${NONEXISTENT_VAR:-default_value}");
        assert_eq!(result, "default_value");
    }

    #[test]
    fn test_expand_env_vars_existing_with_default() {
        std::env::set_var("EXISTING_VAR", "actual");
        let result = expand_env_vars("${EXISTING_VAR:-unused}");
        assert_eq!(result, "actual");
    }

    #[test]
    fn test_expand_env_vars_no_var() {
        std::env::remove_var("MISSING_VAR");
        let result = expand_env_vars("${MISSING_VAR}");
        assert_eq!(result, "");
    }

    #[test]
    fn test_expand_env_vars_mixed() {
        std::env::set_var("VAR1", "a");
        std::env::set_var("VAR2", "b");
        let result = expand_env_vars("prefix_${VAR1}_middle_${VAR2}_suffix");
        assert_eq!(result, "prefix_a_middle_b_suffix");
    }

    #[test]
    fn test_expand_tilde_home_only() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        assert_eq!(expand_tilde("~"), home.to_string_lossy());
    }

    #[test]
    fn test_expand_tilde_home_prefix() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        assert_eq!(
            expand_tilde("~/x/y"),
            home.join("x/y").to_string_lossy().to_string()
        );
    }

    #[test]
    fn test_expand_tilde_does_not_expand_other_forms() {
        assert_eq!(expand_tilde("~someone/x"), "~someone/x");
        assert_eq!(expand_tilde("/abs/path"), "/abs/path");
    }

    #[test]
    fn mcp_config_file_accepts_mcp_servers_key() {
        let raw: McpConfigFile = serde_json::from_str(
            r#"{"mcpServers":{"a":{"command":"echo","args":[]}}}"#,
        )
        .unwrap();
        assert!(raw.mcp_servers.contains_key("a"));
    }

    #[test]
    fn mcp_config_file_accepts_servers_alias() {
        let raw: McpConfigFile =
            serde_json::from_str(r#"{"servers":{"b":{"command":"echo","args":[]}}}"#).unwrap();
        assert!(raw.mcp_servers.contains_key("b"));
    }

    #[test]
    fn merge_stdio_creates_mcp_servers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        merge_stdio_mcp_server_into_json_file(&path, "p", "npx", &["@x/y".to_string()])
            .unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let p = v["mcpServers"]["p"].as_object().unwrap();
        assert_eq!(p["command"].as_str(), Some("npx"));
        assert_eq!(
            p["args"].as_array().unwrap()[0].as_str(),
            Some("@x/y")
        );
    }

    #[test]
    fn merge_stdio_preserves_other_top_level_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{"note":"keep","mcpServers":{"old":{"command":"true","args":[]}}}"#,
        )
        .unwrap();
        merge_stdio_mcp_server_into_json_file(&path, "new", "uv", &[]).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v.get("note").and_then(|x| x.as_str()), Some("keep"));
        let m = v.get("mcpServers").unwrap().as_object().unwrap();
        assert!(m.contains_key("old"));
        assert!(m.contains_key("new"));
    }
}
