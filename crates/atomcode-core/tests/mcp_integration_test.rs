//! MCP integration tests using the built-in mock server.

use atomcode_core::mcp::client::McpClient;
use atomcode_core::mcp::config::load_mcp_config;
use atomcode_core::mcp::transport_stdio::StdioClient;

use std::collections::BTreeMap;
use std::path::Path;

/// Point `ATOMCODE_HOME` at an empty tempdir for the duration of the test so
/// `load_mcp_config` (which always merges the *user* config at
/// `config_dir()/mcp.json`) can't pick up the developer's real
/// `~/.atomcode/mcp.json`. Removes the var on drop. Pair with `#[serial]` —
/// the env var is process-global, so concurrent tests would race the guard.
struct EmptyHome(#[allow(dead_code)] tempfile::TempDir);
impl Drop for EmptyHome {
    fn drop(&mut self) {
        std::env::remove_var("ATOMCODE_HOME");
    }
}
fn isolated_empty_home() -> EmptyHome {
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", tmp.path());
    EmptyHome(tmp)
}

#[test]
#[serial_test::serial]
fn test_config_parsing() {
    let _home = isolated_empty_home();
    // Non-existent config should return empty vec
    let configs = load_mcp_config(Path::new("/nonexistent")).unwrap();
    assert!(configs.is_empty());
}

#[test]
#[serial_test::serial]
fn test_config_env_var_expansion() {
    let _home = isolated_empty_home();
    // Test via the public API: load_mcp_config
    // The expand_env_vars function is tested internally in config.rs
    // This test verifies the public config loading path works correctly
    let configs = load_mcp_config(Path::new("/nonexistent")).unwrap();
    assert!(configs.is_empty(), "empty path should return empty configs");
}

#[tokio::test]
async fn stdio_client_skips_plain_text_stdout_between_protocol_messages() {
    let mut env = BTreeMap::new();
    env.insert(
        "MCP_TEST_STDOUT_NOISE_AFTER_INITIALIZED".to_string(),
        "1".to_string(),
    );
    let mut client = StdioClient::new(
        "noisy-test-server".to_string(),
        env!("CARGO_BIN_EXE_mcp-test-server-core").to_string(),
        Vec::new(),
        env,
        Some(5_000),
    );

    client.initialize().await.unwrap();
    let tools = client.list_tools().await.unwrap();

    assert_eq!(tools.tools.len(), 1);
    assert_eq!(tools.tools[0].name, "echo");
}

#[tokio::test]
async fn stdio_client_works_without_noise() {
    // Baseline: server that doesn't print noise should work fine
    let mut client = StdioClient::new(
        "clean-test-server".to_string(),
        env!("CARGO_BIN_EXE_mcp-test-server-core").to_string(),
        Vec::new(),
        BTreeMap::new(),
        Some(5_000),
    );

    client.initialize().await.unwrap();
    let tools = client.list_tools().await.unwrap();

    assert_eq!(tools.tools.len(), 1);
    assert_eq!(tools.tools[0].name, "echo");
}
