//! Rust dev server detection.
//!
//! Covers `cargo run`. Pre-command: `cargo check` to catch compile errors
//! before the potentially long `cargo run` build.

use super::DetectedServer;

pub fn detect(cmd: &str) -> Option<DetectedServer> {
    let trimmed = cmd.trim();

    if trimmed.contains("cargo run") {
        return Some(DetectedServer {
            label: "Cargo",
            default_port: 8080,
            pre_command: Some("cargo check 2>&1 | tail -20"),
        });
    }

    None
}
