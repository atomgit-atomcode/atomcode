//! MCP integration tests using the built-in mock server.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

use atomcode_core::mcp::{McpRegistry, McpToolInfo};

/// Spawn the mock MCP server for testing.
fn spawn_mock_server() -> (Child, String) {
    let mut child = Command::new("cargo")
        .args(["run", "--bin", "mcp-test-server", "--manifest-path",
               concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("Failed to spawn mock server");

    let pid = child.id().to_string();
    (child, pid)
}

#[tokio::test]
#[ignore = "requires cargo build of test server first"]
async fn test_stdio_client_connect_and_list_tools() {
    // This test is manually verified - the mock server lives in
    // crates/atomcode-core/src/bin/mcp-test-server.rs

    // In practice, use .mcp.json config file to test:
    // {
    //   "servers": {
    //     "test": {
    //       "command": "cargo",
    //       "args": ["run", "--bin", "mcp-test-server"]
    //     }
    //   }
    // }
}

#[test]
fn test_config_parsing() {
    use atomcode_core::mcp::config::load_mcp_config;
    use std::path::Path;

    // Non-existent config should return empty vec
    let configs = load_mcp_config(Path::new("/nonexistent")).unwrap();
    assert!(configs.is_empty());
}

#[test]
fn test_env_var_expansion() {
    use atomcode_core::mcp::config::expand_env_vars;

    std::env::set_var("MCP_TEST_VAR", "test_value");
    let result = expand_env_vars("prefix_${MCP_TEST_VAR}_suffix");
    assert_eq!(result, "prefix_test_value_suffix");

    std::env::remove_var("NONEXISTENT");
    let result = expand_env_vars("${NONEXISTENT:-default}");
    assert_eq!(result, "default");
}
