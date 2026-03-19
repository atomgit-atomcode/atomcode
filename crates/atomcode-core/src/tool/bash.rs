use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use super::{ApprovalRequirement, Tool, ToolDef, ToolResult};

pub struct BashTool {
    working_dir: PathBuf,
}

impl BashTool {
    pub fn new(working_dir: PathBuf) -> Self {
        Self { working_dir }
    }
}

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
            description: "Run a bash command in the working directory. Returns stdout+stderr. \
                For long-running processes, the tool returns early with partial output if the process \
                is still running after 10s. Use & for explicit background.",
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

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::AutoApprove
    }

    async fn execute(&self, args: &str) -> Result<ToolResult> {
        let parsed: BashArgs = serde_json::from_str(args)?;
        let timeout_secs = parsed.timeout.unwrap_or(30);

        let mut child = Command::new("bash")
            .arg("-c")
            .arg(&parsed.command)
            .current_dir(&self.working_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;

        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();

        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();

        // Try to wait for process to finish within timeout
        let result = tokio::time::timeout(
            Duration::from_secs(timeout_secs),
            async {
                // Read stdout and stderr concurrently
                let (_, _) = tokio::join!(
                    async {
                        let mut buf = vec![0u8; 65536];
                        loop {
                            match tokio::time::timeout(Duration::from_secs(INITIAL_WAIT_SECS), stdout.read(&mut buf)).await {
                                Ok(Ok(0)) => break, // EOF
                                Ok(Ok(n)) => stdout_buf.extend_from_slice(&buf[..n]),
                                Ok(Err(_)) => break,
                                Err(_) => break, // Read timeout — process still producing or hung
                            }
                        }
                    },
                    async {
                        let mut buf = vec![0u8; 65536];
                        loop {
                            match tokio::time::timeout(Duration::from_secs(INITIAL_WAIT_SECS), stderr.read(&mut buf)).await {
                                Ok(Ok(0)) => break,
                                Ok(Ok(n)) => stderr_buf.extend_from_slice(&buf[..n]),
                                Ok(Err(_)) => break,
                                Err(_) => break,
                            }
                        }
                    }
                );

                // Check if process has exited
                match child.try_wait() {
                    Ok(Some(status)) => Some(status.success()),
                    _ => None, // Still running
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
