//! The `LspServerConfig` config type (`[lsp.servers.<ext>]`). Moved here from
//! `atomcode_core::lsp::registry` so `Config.lsp.servers` needs no core dependency;
//! the LSP *runtime* (`LspServerRegistry`, client, manager) stays in core and
//! re-exports this type.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub root_markers: Vec<String>,
}
