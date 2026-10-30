//! Neutral LSP declarations; expanded values remain private to live clients.
use serde::{Deserialize, Serialize};

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

impl LspFeatures {
    pub fn mode(&self, operation: &str, defaults: &Self) -> LspMode {
        let value = |features: &Self| match operation {
            "definition" => features.definition,
            "references" => features.references,
            "hover" => features.hover,
            "diagnostics" => features.diagnostics,
            "document_symbols" => features.document_symbols,
            _ => None,
        };
        value(self).or_else(|| value(defaults)).unwrap_or_default()
    }
}
