//! MCP configuration loading.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};

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
    #[serde(default)]
    servers: BTreeMap<String, McpServerEntry>,
}

#[derive(Debug, Deserialize)]
struct McpServerEntry {
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
/// (`~/.atomcode/mcp.json`) for servers with the same name.
pub fn load_mcp_config(project_dir: &Path) -> Result<Vec<McpServerConfig>> {
    let user_config = load_config_file(
        &dirs::home_dir()
            .map(|h| h.join(".atomcode/mcp.json"))
            .unwrap_or_default(),
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

    for (name, entry) in raw.servers {
        let config = server_entry_to_config(&name, entry)?;
        configs.push(config);
    }

    Ok(configs)
}

fn server_entry_to_config(name: &str, entry: McpServerEntry) -> Result<McpServerConfig> {
    let transport = if let Some(command) = entry.command {
        McpTransportConfig::Stdio {
            command: expand_env_vars(&command),
            args: entry
                .args
                .unwrap_or_default()
                .into_iter()
                .map(|a| expand_env_vars(&a))
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
            url: expand_env_vars(&url),
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
