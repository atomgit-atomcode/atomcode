//! Shared path resolution for every capability that touches the atomcode config
//! tree (`mcp` / `session` / `memory`) — ONE home for the rule, one place to
//! document its single known divergence from production.

use std::path::PathBuf;

/// The atomcode config/data root: `$ATOMCODE_HOME` if set & non-empty, else
/// `~/.atomcode`. Mirrors `atomcode_core::config::Config::config_dir` so everything
/// L1 persists (`sessions/`, `memory.md`, MCP OAuth tokens) lands in the SAME tree
/// as production's.
///
/// KNOWN DIVERGENCE (deliberate L1 simplification): the core helper additionally
/// resolves `$SUDO_USER` via getpwnam, so under `sudo` WITHOUT `$ATOMCODE_HOME` set
/// production resolves the invoking user's home while this resolves root's — the
/// two stacks would then read/write parallel trees. Setting `$ATOMCODE_HOME`
/// (checked first, byte-identical to production) keeps them aligned.
pub(crate) fn config_dir() -> PathBuf {
    if let Ok(p) = std::env::var("ATOMCODE_HOME") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".atomcode")
}

// NO unit test here ON PURPOSE: testing this means mutating the process-global
// `ATOMCODE_HOME`, and libtest runs the lib's unit tests in parallel threads — any
// future unit test touching `config_dir()` (memory.md, session paths, OAuth token
// store) would race it nondeterministically. The `$ATOMCODE_HOME`-wins behavior is
// exercised by the env-isolating INTEGRATION binaries (each its own process):
// capabilities `tests/session.rs` + `tests/mcp.rs`, coding `tests/full_assembly.rs`.
