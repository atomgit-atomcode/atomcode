//! Client-injected ACP `mcpServers` → coding MCP config conversion.
//!
//! ACP clients may supply `mcpServers` in session lifecycle requests
//! (`session/new`, `session/resume`, `session/load`). This module owns the
//! conversion boundary: which transports are connected into the session's tool
//! catalog, which are surfaced as ignored instead of silently dropped, and how
//! the ignored list is logged.

use agent_client_protocol::schema::v1::McpServer;
use atomcode_capabilities::mcp::config::McpConfigSource;
use atomcode_capabilities::mcp::{McpServerConfig, McpTransportConfig};

/// Extract the human-readable names of client-requested MCP servers.
pub fn mcp_server_names(mcp_servers: &[McpServer]) -> Vec<String> {
    mcp_servers
        .iter()
        .map(|server| match server {
            McpServer::Http(h) => h.name.clone(),
            McpServer::Sse(s) => s.name.clone(),
            McpServer::Stdio(s) => s.name.clone(),
            // Non-exhaustive + future variants (e.g. unstable `Acp`): degrade
            // the name gracefully.
            _ => String::new(),
        })
        .collect()
}

/// Convert client-injected ACP `mcpServers` into coding MCP configs.
///
/// Stdio servers — the protocol-baseline transport every agent MUST support —
/// and HTTP servers (advertised via `mcp_capabilities.http`) are connected into
/// the session's tool catalog. Transports this agent does not advertise (`sse`,
/// and any future variant) are returned in the ignored list so the caller can
/// surface them instead of silently dropping them. Malformed stdio entries
/// (empty name/command, relative command path) are also reported as ignored:
/// MCP connection is best-effort (SHOULD), so a bad entry must not fail session
/// setup.
pub fn acp_mcp_server_configs(mcp_servers: &[McpServer]) -> (Vec<McpServerConfig>, Vec<String>) {
    let mut configs = Vec::new();
    let mut ignored = Vec::new();
    for server in mcp_servers {
        match server {
            McpServer::Stdio(s) => {
                let relative = !s.command.is_absolute();
                if s.name.is_empty() || s.command.as_os_str().is_empty() || relative {
                    ignored.push(s.name.clone());
                    if relative && !s.name.is_empty() {
                        eprintln!(
                            "acp: mcpServer `{}` command `{}` is not an absolute path; not connected",
                            s.name,
                            s.command.display()
                        );
                    }
                    continue;
                }
                configs.push(McpServerConfig {
                    name: s.name.clone(),
                    disabled: false,
                    config: McpTransportConfig::Stdio {
                        command: s.command.to_string_lossy().into_owned(),
                        args: s.args.clone(),
                        env: s
                            .env
                            .iter()
                            .map(|e| (e.name.clone(), e.value.clone()))
                            .collect(),
                        timeout_ms: None,
                    },
                    // The ACP client explicitly requested this server: it is the
                    // trust boundary. Tool calls still flow through the kernel
                    // approval round-trip (trust=false) unless the client opts in.
                    source: McpConfigSource::Driver,
                    trust: false,
                    auto_approve: Vec::new(),
                });
            }
            McpServer::Http(h) => {
                // Advertised via `mcp_capabilities.http`. Map to the coding MCP
                // HTTP transport; the client supplies the URL + headers and is
                // the trust boundary (source=Driver below). The ACP
                // `McpServer::Http` shape carries no auth metadata, so `auth`
                // stays `None` (unauthenticated HTTP endpoint).
                configs.push(McpServerConfig {
                    name: h.name.clone(),
                    disabled: false,
                    config: McpTransportConfig::Http {
                        url: h.url.clone(),
                        headers: h
                            .headers
                            .iter()
                            .map(|e| (e.name.clone(), e.value.clone()))
                            .collect(),
                        auth: None,
                        timeout_ms: None,
                    },
                    source: McpConfigSource::Driver,
                    trust: false,
                    auto_approve: Vec::new(),
                });
            }
            McpServer::Sse(s) => ignored.push(s.name.clone()),
            _ => {}
        }
    }
    (configs, ignored)
}

/// Log the subset of client-requested MCP servers this agent does NOT connect —
/// transports it does not advertise (`sse`, unknown/future variants) or
/// malformed stdio entries. Advertised transports (stdio, http) ARE connected
/// into the session's tool catalog via [`acp_mcp_server_configs`]; this logs
/// only the ignored remainder so an operator can see what was dropped.
pub fn log_ignored_mcp_server_names(names: &[String]) {
    if names.is_empty() {
        return;
    }
    eprintln!(
        "acp: {} client-injected mcpServers were NOT connected ({}): \
         transport not advertised or entry malformed",
        names.len(),
        names.join(", "),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        HttpHeader, McpServerHttp, McpServerSse, McpServerStdio,
    };

    #[test]
    fn mcp_server_names_extracts_all_transport_kinds() {
        let servers = vec![
            McpServer::Stdio(McpServerStdio::new("fs-stdio", "/usr/bin/fs")),
            McpServer::Http(McpServerHttp::new(
                "api-http",
                "https://api.example.com/mcp",
            )),
            McpServer::Sse(McpServerSse::new(
                "events-sse",
                "https://events.example.com/mcp",
            )),
        ];
        let names = mcp_server_names(&servers);
        assert_eq!(names, vec!["fs-stdio", "api-http", "events-sse"]);
        assert!(mcp_server_names(&[]).is_empty());
    }

    #[test]
    fn acp_mcp_server_configs_connects_stdio_and_http_ignores_sse() {
        let servers = vec![
            McpServer::Stdio(McpServerStdio::new("fs", "/usr/bin/fs")),
            McpServer::Http(
                McpServerHttp::new("api", "https://api.example.com/mcp")
                    .headers(vec![HttpHeader::new("Authorization", "Bearer t")]),
            ),
            McpServer::Sse(McpServerSse::new(
                "events",
                "https://events.example.com/mcp",
            )),
        ];
        let (configs, ignored) = acp_mcp_server_configs(&servers);
        assert_eq!(
            configs.len(),
            2,
            "stdio (baseline) + advertised http are connected"
        );
        assert_eq!(configs[0].name, "fs");
        assert!(matches!(
            &configs[0].config,
            McpTransportConfig::Stdio { command, .. } if command == "/usr/bin/fs"
        ));
        assert_eq!(configs[0].source, McpConfigSource::Driver);
        assert!(!configs[0].trust);
        // HTTP server maps url + headers, no auth, Driver source (trust boundary).
        assert_eq!(configs[1].name, "api");
        match &configs[1].config {
            McpTransportConfig::Http {
                url, headers, auth, ..
            } => {
                assert_eq!(url, "https://api.example.com/mcp");
                assert_eq!(
                    headers.get("Authorization").map(String::as_str),
                    Some("Bearer t")
                );
                assert!(
                    auth.is_none(),
                    "ACP McpServer::Http carries no auth metadata"
                );
            }
            other => panic!("expected Http transport, got {other:?}"),
        }
        assert_eq!(configs[1].source, McpConfigSource::Driver);
        // SSE is NOT connected (no SSE transport in the capabilities MCP layer).
        assert_eq!(ignored, vec!["events"]);
    }

    #[test]
    fn acp_mcp_server_configs_skips_malformed_stdio_entries() {
        let servers = vec![
            // Relative command path — the protocol requires an absolute path.
            McpServer::Stdio(McpServerStdio::new("rel", "bin/server")),
            // Empty name cannot key the tool namespace.
            McpServer::Stdio(McpServerStdio::new("", "/usr/bin/server")),
            McpServer::Stdio(McpServerStdio::new("ok", "/usr/bin/server")),
        ];
        let (configs, ignored) = acp_mcp_server_configs(&servers);
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].name, "ok");
        assert_eq!(ignored, vec!["rel", ""]);
    }
}
