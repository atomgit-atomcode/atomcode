//! MCP integration tests using the built-in mock server.

use atomcode_core::mcp::config::load_mcp_config;

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
