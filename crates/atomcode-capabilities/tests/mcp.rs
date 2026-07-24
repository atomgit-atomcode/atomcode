//! End-to-end tests for the `mcp` capability, using the in-tree `mcp-test-server`
//! stdio fixture (a minimal MCP server: `initialize` + `tools/list` [one `echo`
//! tool] + `tools/call`). Exercises the real ported transport/registry, the kernel
//! `Tool` adapter, and the kernel Tool-contract conformance gate.
#![cfg(feature = "mcp")]

use std::collections::BTreeMap;
use std::sync::Arc;

use atomcode_capabilities::mcp::config::{McpConfigSource, McpServerConfig, McpTransportConfig};
use atomcode_capabilities::mcp::{McpRegistry, McpToolAdapter, CONNECT_TIMEOUT};
use atomcode_kernel::conformance;
use atomcode_kernel::tool::{ProgressSink, RiskLevel, Tool, ToolContext};
use tokio_util::sync::CancellationToken;

// Redirect ATOMCODE_HOME to a throwaway temp dir before any test in this binary runs,
// so a test that resolves the user mcp.json without setting its own ATOMCODE_HOME never
// touches the developer's real home. Tests that set their own ATOMCODE_HOME still win.
#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

/// A stdio server config pointing at the in-tree `mcp-test-server` fixture binary.
fn test_server_config(name: &str) -> McpServerConfig {
    McpServerConfig {
        name: name.to_string(),
        source: McpConfigSource::Project,
        disabled: false,
        config: McpTransportConfig::Stdio {
            command: env!("CARGO_BIN_EXE_mcp-test-server").to_string(),
            args: vec![],
            env: BTreeMap::new(),
            timeout_ms: Some(5_000),
        },
        trust: false,
        auto_approve: vec![],
    }
}

fn ctx() -> ToolContext {
    ToolContext {
        working_dir: std::env::temp_dir(),
        cancel: CancellationToken::new(),
        progress: ProgressSink::noop(),
        requester: None,
    }
}

/// The core happy path: connect a stdio server, discover its tool, wrap it as a
/// kernel `Tool`, and call it — asserting the `mcp__{server}__{tool}` naming, the
/// always-`Risky` classification, and the round-tripped echo output.
#[tokio::test]
async fn registry_connect_discover_and_call_echo() {
    let registry = McpRegistry::new();
    registry
        .add_server(test_server_config("testsrv"))
        .await
        .expect("stdio MCP server should connect");
    let registry = registry.share();

    let infos = registry.list_all_tools().await;
    assert_eq!(infos.len(), 1, "test server exposes exactly one tool");
    assert_eq!(infos[0].tool_name, "echo");

    let adapter = McpToolAdapter::new(registry, infos.into_iter().next().unwrap());
    assert_eq!(adapter.name(), "mcp__testsrv__echo");
    assert_eq!(
        adapter.risk("{}"),
        RiskLevel::Risky,
        "external MCP tools must always be Risky so approval middleware gates them"
    );

    let result = adapter.execute(r#"{"message":"hi"}"#, &ctx()).await;
    assert!(!result.is_error, "echo call should succeed: {result:?}");
    assert_eq!(result.content, "echo:hi");
}

/// A malformed-arguments call must surface as a tool error (`is_error`), never a
/// panic — the kernel PANIC CONTRACT.
#[tokio::test]
async fn adapter_maps_bad_arguments_to_tool_error() {
    let registry = McpRegistry::new();
    registry
        .add_server(test_server_config("testsrv"))
        .await
        .expect("stdio MCP server should connect");
    let registry = registry.share();
    let infos = registry.list_all_tools().await;
    let adapter = McpToolAdapter::new(registry, infos.into_iter().next().unwrap());

    let result = adapter.execute("not json", &ctx()).await;
    assert!(
        result.is_error,
        "invalid JSON args must become a tool error"
    );
    assert!(result.content.contains("invalid MCP tool arguments"));
}

/// Every discovered MCP tool must satisfy the kernel `Tool` contract (stable
/// name/description/schema, deterministic risk, execute that terminates without
/// panicking). This is the gate the spec requires for each surfaced tool.
#[tokio::test]
async fn adapter_passes_kernel_tool_conformance() {
    let registry = McpRegistry::new();
    registry
        .add_server(test_server_config("conf"))
        .await
        .expect("stdio MCP server should connect");
    let registry = registry.share();
    let infos = registry.list_all_tools().await;
    let adapter: Arc<dyn Tool> = Arc::new(McpToolAdapter::new(
        registry,
        infos.into_iter().next().unwrap(),
    ));

    let report = conformance::tool::check(adapter, &[r#"{"message":"x"}"#]).await;
    report.assert_conformant();
}

/// Write a trust store file that marks `project_dir` as trusted.
/// Mirrors the format written by `atomcode_core::mcp::trust::trust_project`.
fn write_trusted_store(store_path: &std::path::Path, project_dir: &std::path::Path) {
    let key = atomcode_capabilities::mcp::registry::project_trust_key(project_dir);
    let store = serde_json::json!({
        "version": 1,
        "projects": {
            key: { "path": project_dir.display().to_string() }
        }
    });
    std::fs::write(store_path, serde_json::to_vec_pretty(&store).unwrap()).unwrap();
}

/// Background config loading discovers a trusted project's tools without imposing
/// session-transition policy on the capability layer.
#[tokio::test]
#[serial_test::serial]
async fn background_registry_reads_project_mcp_json() {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: edition 2021; this is the only test that reads global MCP config, and
    // it only ever points ATOMCODE_HOME at an empty dir (no user mcp.json), so a
    // concurrent `load_mcp_config` still resolves to "no user servers".
    std::env::set_var("ATOMCODE_HOME", home.path());

    let project = tempfile::tempdir().unwrap();

    // Pre-trust the project so the security gate allows its servers through.
    // Point ATOMCODE_MCP_TRUST_STORE at a store inside our isolated home dir.
    let trust_store = home.path().join("mcp_trust.json");
    // SAFETY: test-only env mutation; #[serial] prevents concurrent tests from
    // racing on this variable.
    unsafe {
        std::env::set_var("ATOMCODE_MCP_TRUST_STORE", &trust_store);
    }
    write_trusted_store(&trust_store, project.path());

    let server = env!("CARGO_BIN_EXE_mcp-test-server");
    let mcp_json = serde_json::json!({
        "mcpServers": { "proj": { "command": server, "args": [], "timeout_ms": 5000 } }
    });
    std::fs::write(project.path().join(".mcp.json"), mcp_json.to_string()).unwrap();

    let registry = McpRegistry::from_config_background(project.path()).share();
    registry.wait_for_initial_connections(CONNECT_TIMEOUT).await;
    let adapters: Vec<Arc<dyn Tool>> = registry
        .list_all_tools()
        .await
        .into_iter()
        .map(|info| Arc::new(McpToolAdapter::new(registry.clone(), info)) as Arc<dyn Tool>)
        .collect();

    let names: Vec<String> = adapters.iter().map(|a| a.name().to_string()).collect();
    assert!(
        names.iter().any(|n| n == "mcp__proj__echo"),
        "background discovery should surface the project server's echo tool; got {names:?}"
    );
    let statuses = registry.server_statuses().await;
    assert!(
        statuses.iter().any(|(n, _)| n == "proj"),
        "the connected server should appear in server_statuses"
    );
}
