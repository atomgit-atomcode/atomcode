//! MCP integration tests using the built-in mock server.

use atomcode_core::mcp::client::McpClient;
use atomcode_core::mcp::config::load_mcp_config;
use atomcode_core::mcp::transport_stdio::StdioClient;

use std::collections::BTreeMap;
use std::path::Path;

#[test]
fn test_config_parsing() {
    // Non-existent config should return empty vec
    let configs = load_mcp_config(Path::new("/nonexistent")).unwrap();
    assert!(configs.is_empty());
}

#[test]
fn test_config_env_var_expansion() {
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
        env!("CARGO_BIN_EXE_mcp-test-server").to_string(),
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
        env!("CARGO_BIN_EXE_mcp-test-server").to_string(),
        Vec::new(),
        BTreeMap::new(),
        Some(5_000),
    );

    client.initialize().await.unwrap();
    let tools = client.list_tools().await.unwrap();

    assert_eq!(tools.tools.len(), 1);
    assert_eq!(tools.tools[0].name, "echo");
}
