//! Dev server detection and pre-command orchestration.
//!
//! Each language module registers its server patterns, default ports, and
//! optional pre-commands (e.g., `mvn compile` before `mvn spring-boot:run`).
//! The bash tool calls `detect()` to check if a command is a dev server,
//! then `pre_command()` to get any required pre-step.

pub mod java;
pub mod javascript;
pub mod python;
pub mod rust;

use std::path::Path;

/// A detected dev server command with metadata.
#[derive(Debug, Clone)]
pub struct DetectedServer {
    /// Human-readable label, e.g. "Spring Boot", "Vite"
    pub label: &'static str,
    /// Default port if not specified in command
    pub default_port: u16,
    /// Pre-command to run before starting (e.g. "mvn compile"). None if not needed.
    pub pre_command: Option<&'static str>,
}

/// Result of running a pre-command.
pub struct PreCommandResult {
    pub success: bool,
    pub output: String,
}

/// Detect if a command is a dev server. Returns metadata if matched.
pub fn detect(cmd: &str) -> Option<DetectedServer> {
    // Try each language module in order. First match wins.
    java::detect(cmd)
        .or_else(|| javascript::detect(cmd))
        .or_else(|| python::detect(cmd))
        .or_else(|| rust::detect(cmd))
}

/// Check if a command is a background/server command (should not be killed on timeout).
pub fn is_server_command(cmd: &str) -> bool {
    let trimmed = cmd.trim();
    // Explicit background markers
    if trimmed.ends_with('&') || trimmed.contains("nohup ") || trimmed.contains("setsid ") {
        return true;
    }
    detect(trimmed).is_some()
}

/// Extract port number from a command string.
/// Checks explicit flags first (--port, -p, -Dserver.port=), then falls back
/// to the detected server's default port.
pub fn extract_port(cmd: &str) -> Option<u16> {
    // Explicit port flags (tech-stack agnostic)
    let port_patterns = ["-p ", "--port ", "--port=", "-Dserver.port=", "PORT="];
    for pat in &port_patterns {
        if let Some(pos) = cmd.find(pat) {
            let after = &cmd[pos + pat.len()..];
            let port_str: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(p) = port_str.parse::<u16>() {
                return Some(p);
            }
        }
    }

    // Fall back to detected server's default port
    detect(cmd).map(|s| s.default_port)
}

/// Run a pre-command (e.g. `mvn compile`) synchronously before starting the server.
/// Returns None if no pre-command is needed.
pub async fn run_pre_command(cmd: &str, working_dir: &Path) -> Option<PreCommandResult> {
    let server = detect(cmd)?;
    let pre_cmd = server.pre_command?;

    let output = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(pre_cmd)
        .current_dir(working_dir)
        .output()
        .await;

    match output {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let stderr = String::from_utf8_lossy(&o.stderr);
            let combined = if stderr.trim().is_empty() {
                stdout.trim().to_string()
            } else if stdout.trim().is_empty() {
                stderr.trim().to_string()
            } else {
                format!("{}\n{}", stdout.trim(), stderr.trim())
            };
            Some(PreCommandResult {
                success: o.status.success(),
                output: combined,
            })
        }
        Err(e) => Some(PreCommandResult {
            success: false,
            output: format!("Failed to run `{}`: {}", pre_cmd, e),
        }),
    }
}
