//! LSP diagnostics: a minimal async JSON-RPC client that spawns external language
//! servers and surfaces `publishDiagnostics`. Behind the opt-in `lsp` feature.
//!
//! - [`jsonrpc`] — Content-Length framing.
//! - [`types`] — `Diagnostic` / `DiagnosticSeverity`.
//! - [`registry`] — extension → server command (rust-analyzer / pylsp / gopls / …).
//! - [`client`] — transport-agnostic protocol client (testable over `duplex`).
//! - [`manager`] — pooled, lazily-started [`LspManager`] the diagnostics tool holds.
//!
//! Servers must be installed on PATH; their absence degrades gracefully (the tool
//! reports "LSP not available" rather than erroring).

pub mod client;
pub mod jsonrpc;
pub mod manager;
pub mod registry;
pub mod types;

pub use client::LspClient;
pub use manager::LspManager;
pub use registry::{extension_to_language_id, LspServerConfig, LspServerRegistry};
pub use types::{Diagnostic, DiagnosticSeverity};
