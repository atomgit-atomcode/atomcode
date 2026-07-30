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
}
