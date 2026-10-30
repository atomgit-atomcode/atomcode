//! The `LspServerConfig` config type (`[lsp.servers.<ext>]`). The LSP runtime is
//! owned by `atomcode-capabilities::codeintel::lsp`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub root_markers: Vec<String>,
    #[serde(flatten)]
    pub options: LspOptions,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LspMode {
    On,
    #[default]
    Auto,
    Off,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LspTrace {
    #[default]
    Off,
    Messages,
    Verbose,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LspFeatures {
    pub definition: Option<LspMode>,
    pub references: Option<LspMode>,
    pub hover: Option<LspMode>,
    pub diagnostics: Option<LspMode>,
    pub document_symbols: Option<LspMode>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LspOptions {
    pub disabled: bool,
    /// Total semantic query attempts, including the first (1..=5; default 1).
    pub retry_attempts: Option<u32>,
    /// Delay between successful empty responses (0..=10000; default 5000 ms).
    pub retry_interval_ms: Option<u64>,
    pub init_options_mode: LspMode,
    pub init_options_file: Option<std::path::PathBuf>,
    pub init_options: Option<serde_json::Value>,
    pub settings_file: Option<std::path::PathBuf>,
    pub settings: Option<serde_json::Value>,
    pub features: LspFeatures,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn server_options_parse_without_changing_existing_config_shape() {
        let config: crate::config::LspConfig = toml::from_str(
            r#"
            enabled = true
            trace = "messages"
            [features]
            hover = "off"
            [servers.cj]
            command = "LSPServer.exe"
            retry_attempts = 3
            retry_interval_ms = 5000
            root_markers = ["cjpm.toml"]
            init_options_file = "LSP_config/cangjie.json"
            init_options_mode = "auto"
            [servers.cj.features]
            hover = "on"
        "#,
        )
        .unwrap();
        assert_eq!(config.trace, LspTrace::Messages);
        let server = &config.servers["cj"];
        assert_eq!(server.options.retry_attempts, Some(3));
        assert_eq!(server.options.retry_interval_ms, Some(5000));
        assert_eq!(
            server.options.init_options_file.as_deref(),
            Some(std::path::Path::new("LSP_config/cangjie.json"))
        );
        assert_eq!(server.options.features.hover, Some(LspMode::On));
        assert_eq!(config.features.hover, Some(LspMode::Off));
        assert!(
            toml::from_str::<LspServerConfig>("command = 'x'\ninit_options_mode = 'typo'").is_err()
        );
    }
}
