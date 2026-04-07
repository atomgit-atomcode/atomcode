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
/// Also handles compound commands (e.g., `kill PID; sleep 2; mvn spring-boot:run`)
/// by splitting on `;`, `&&`, `||` and checking each segment.
pub fn detect(cmd: &str) -> Option<DetectedServer> {
    detect_single(cmd).or_else(|| {
        // Split compound commands and check each segment
        split_compound(cmd).iter()
            .filter_map(|seg| detect_single(seg))
            .last() // last match = the actual server command
    })
}

/// Detect a single (non-compound) command.
fn detect_single(cmd: &str) -> Option<DetectedServer> {
    java::detect(cmd)
        .or_else(|| javascript::detect(cmd))
        .or_else(|| python::detect(cmd))
        .or_else(|| rust::detect(cmd))
}

/// Split a compound command by `;`, `&&`, `||` into segments.
fn split_compound(cmd: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut rest = cmd;
    while !rest.is_empty() {
        // Find earliest delimiter
        let delims = ["&&", "||", ";"];
        let mut earliest: Option<(usize, usize)> = None; // (pos, delim_len)
        for d in &delims {
            if let Some(pos) = rest.find(d) {
                if earliest.is_none() || pos < earliest.unwrap().0 {
                    earliest = Some((pos, d.len()));
                }
            }
        }
        match earliest {
            Some((pos, len)) => {
                let seg = rest[..pos].trim();
                if !seg.is_empty() { parts.push(seg); }
                rest = rest[pos + len..].trim_start();
            }
            None => {
                let seg = rest.trim();
                if !seg.is_empty() { parts.push(seg); }
                break;
            }
        }
    }
    parts
}

/// Check if a command is a background/server command (should not be killed on timeout).
pub fn is_server_command(cmd: &str) -> bool {
    let trimmed = cmd.trim();
    // Explicit background markers
    if trimmed.ends_with('&') || trimmed.contains("nohup ") || trimmed.contains("setsid ") {
        return true;
    }

    // Extract the primary command (first token before pipes/&&).
    // "npm install @vitejs/plugin-vue" → primary is "npm"
    // "ps aux | grep uvicorn" → primary is "ps"
    // "uvicorn app:main" → primary is "uvicorn"
    let primary = trimmed
        .split(|c: char| c == '|' || c == '&' || c == ';')
        .next()
        .unwrap_or(trimmed)
        .trim();

    // Non-server commands: never block these regardless of arguments
    let safe_prefixes = [
        "npm install", "npm i ", "npm ci", "npm add",
        "pip install", "pip3 install", "pip ", "pip3 ",
        "cargo add", "cargo install",
        "yarn add", "pnpm add", "pnpm install", "bun add", "bun install",
        "apt ", "brew ", "winget ", "choco ",
        "ps ", "grep ", "kill ", "pkill ", "pgrep ",
        "cat ", "head ", "tail ", "less ", "more ",
        "ls ", "find ", "which ", "where ", "type ",
        "curl ", "wget ",
        "mkdir ", "rm ", "mv ", "cp ",
        "git ", "docker ",
    ];
    let primary_lower = primary.to_lowercase();
    if safe_prefixes.iter().any(|p| primary_lower.starts_with(p)) {
        return false;
    }

    detect(primary).is_some()
}

/// Extract port number from a command string + project config files.
/// Priority: explicit flags > project config > detected server default.
pub fn extract_port(cmd: &str) -> Option<u16> {
    extract_port_with_dir(cmd, None)
}

/// Extract port with optional working directory for config file lookup.
pub fn extract_port_with_dir(cmd: &str, working_dir: Option<&Path>) -> Option<u16> {
    // 1. Explicit port flags in command (highest priority)
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

    // 2. Read from project config files (application.properties / .env / vite.config)
    if let Some(wd) = working_dir {
        if let Some(p) = read_port_from_config(wd) {
            return Some(p);
        }
    }

    // 3. Fall back to detected server's default port
    detect(cmd).map(|s| s.default_port)
}

/// Try to read server port from common config files.
fn read_port_from_config(working_dir: &Path) -> Option<u16> {
    // Spring Boot: application.properties / application.yml
    let spring_configs = [
        "src/main/resources/application.properties",
        "src/main/resources/application.yml",
        "src/main/resources/application.yaml",
        "application.properties",
    ];
    for cfg in &spring_configs {
        let path = working_dir.join(cfg);
        if let Ok(content) = std::fs::read_to_string(&path) {
            // server.port=8081 or server.port: 8081
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("server.port") {
                    let port_str: String = trimmed.chars()
                        .filter(|c| c.is_ascii_digit())
                        .collect();
                    if let Ok(p) = port_str.parse::<u16>() {
                        return Some(p);
                    }
                }
            }
        }
    }

    // Node.js: .env (PORT=3001)
    let env_path = working_dir.join(".env");
    if let Ok(content) = std::fs::read_to_string(&env_path) {
        for line in content.lines() {
            if line.trim().starts_with("PORT=") {
                let port_str: String = line.trim()[5..].chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                if let Ok(p) = port_str.parse::<u16>() {
                    return Some(p);
                }
            }
        }
    }

    None
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
