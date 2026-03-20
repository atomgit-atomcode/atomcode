use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

pub struct BashTool;

#[derive(Deserialize)]
struct BashArgs {
    command: String,
    timeout: Option<u64>,
}

/// How long to wait for initial output before checking if process is still running.
const INITIAL_WAIT_SECS: u64 = 10;

#[async_trait]
impl Tool for BashTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "bash",
            description: "Run a bash command. ONLY use for: running programs, installing packages, \
                starting servers, git commands, build/test commands. \
                Do NOT use bash to read files — use read_file instead. \
                Do NOT use grep/cat/head/tail/sed/awk through bash — use the grep or read_file tools. \
                For long-running processes, returns early after 10s with partial output.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The bash command to execute" },
                    "timeout": { "type": "integer", "description": "Max wait seconds (default 30)" }
                },
                "required": ["command"]
            }),
        }
    }

    fn approval(&self, args: &str) -> ApprovalRequirement {
        if let Ok(parsed) = serde_json::from_str::<BashArgs>(args) {
            if let Some(reason) = check_destructive_command(&parsed.command) {
                return ApprovalRequirement::RequireApproval(reason);
            }
        }
        ApprovalRequirement::AutoApprove
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: BashArgs = serde_json::from_str(args)?;
        let timeout_secs = parsed.timeout.unwrap_or(30);

        let wd = ctx.working_dir.read().await.clone();

        let mut child = Command::new("bash")
            .arg("-c")
            .arg(&parsed.command)
            .current_dir(&wd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;

        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();

        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();

        // Wait for process to finish or timeout. Read stdout/stderr concurrently.
        // Use INITIAL_WAIT_SECS as the early-return timeout for long-running processes.
        // After that, capture whatever output was produced so far.
        let wait_secs = parsed.timeout.unwrap_or(INITIAL_WAIT_SECS);
        let result = tokio::time::timeout(
            Duration::from_secs(wait_secs),
            async {
                // Read stdout and stderr concurrently until process exits
                let (_, _) = tokio::join!(
                    async {
                        let mut buf = vec![0u8; 65536];
                        loop {
                            match stdout.read(&mut buf).await {
                                Ok(0) => break,
                                Ok(n) => stdout_buf.extend_from_slice(&buf[..n]),
                                Err(_) => break,
                            }
                        }
                    },
                    async {
                        let mut buf = vec![0u8; 65536];
                        loop {
                            match stderr.read(&mut buf).await {
                                Ok(0) => break,
                                Ok(n) => stderr_buf.extend_from_slice(&buf[..n]),
                                Err(_) => break,
                            }
                        }
                    }
                );

                // Process exited (stdout/stderr closed = process done)
                match child.try_wait() {
                    Ok(Some(status)) => Some(status.success()),
                    _ => None,
                }
            }
        ).await;

        let stdout_str = String::from_utf8_lossy(&stdout_buf).to_string();
        let stderr_str = String::from_utf8_lossy(&stderr_buf).to_string();

        match result {
            Ok(Some(success)) => {
                // Process exited within timeout
                let combined = format_output(&stdout_str, &stderr_str);
                Ok(ToolResult { call_id: String::new(), output: combined, success })
            }
            Ok(None) => {
                // Process still running — return what we have + PID
                let pid = child.id().map(|p| p.to_string()).unwrap_or_else(|| "?".into());
                let combined = format_output(&stdout_str, &stderr_str);
                let output = if combined.is_empty() {
                    format!("Process still running (PID: {}). No output captured yet.", pid)
                } else {
                    format!("{}\n\n[Process still running, PID: {}]", combined, pid)
                };
                Ok(ToolResult { call_id: String::new(), output, success: true })
            }
            Err(_) => {
                // Hard timeout
                let _ = child.kill().await;
                let combined = format_output(&stdout_str, &stderr_str);
                let output = if combined.is_empty() {
                    format!("Timed out after {}s with no output.", timeout_secs)
                } else {
                    format!("{}\n\n[Timed out after {}s, process killed]", combined, timeout_secs)
                };
                Ok(ToolResult { call_id: String::new(), output, success: false })
            }
        }
    }
}

/// Check if a shell command contains destructive patterns that require user approval.
fn check_destructive_command(command: &str) -> Option<String> {
    let cmd = command.to_lowercase();

    let patterns: &[(&str, &str)] = &[
        ("rm -rf", "Recursive force delete"),
        ("rm -r ", "Recursive delete"),
        ("rm -fr", "Recursive force delete"),
        ("rmdir", "Directory removal"),
        (" drop ", "SQL DROP statement"),
        ("drop table", "SQL DROP TABLE"),
        ("drop database", "SQL DROP DATABASE"),
        ("format ", "Disk format"),
        ("mkfs", "Filesystem creation"),
        ("dd if=", "Raw disk write"),
        ("> /dev/", "Device write"),
        ("chmod 777", "World-writable permission"),
        ("chmod -r ", "Recursive permission change"),
        ("kill -9", "Force kill process"),
        ("killall", "Kill all matching processes"),
        ("git push --force", "Force push"),
        ("git push -f", "Force push"),
        ("git reset --hard", "Hard reset (destroys uncommitted changes)"),
        ("git clean -f", "Force clean untracked files"),
    ];

    for (pattern, reason) in patterns {
        if cmd.contains(pattern) {
            return Some(format!("Destructive command detected: {}. Command: {}", reason, command));
        }
    }

    // Detect `rm` on files in the working directory (prevents rm+write_file bypass).
    // Tech-stack agnostic: any `rm` that isn't cleaning temp/build artifacts needs approval.
    if cmd.starts_with("rm ") && !cmd.contains("-r") {
        let ignore_dirs = ["node_modules", "dist", "build", ".cache", "target", "__pycache__", ".tmp"];
        let is_artifact = ignore_dirs.iter().any(|d| cmd.contains(d));
        if !is_artifact {
            return Some(format!(
                "Deleting file: {}. Use edit_file to modify files instead of deleting and recreating.",
                command
            ));
        }
    }

    None
}

fn format_output(stdout: &str, stderr: &str) -> String {
    let stdout = stdout.trim();
    let stderr = stderr.trim();
    if stderr.is_empty() {
        stdout.to_string()
    } else if stdout.is_empty() {
        format!("STDERR:\n{}", stderr)
    } else {
        format!("{}\nSTDERR:\n{}", stdout, stderr)
    }
}
