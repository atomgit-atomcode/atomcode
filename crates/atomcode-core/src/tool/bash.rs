use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
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

#[async_trait]
impl Tool for BashTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "bash",
            description: "Execute a bash command and return its stdout and stderr. Commands run in the project working directory.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The bash command to execute" },
                    "timeout": { "type": "integer", "description": "Timeout in seconds. Default: 120" }
                },
                "required": ["command"]
            }),
        }
    }

    fn approval(&self, args: &str) -> ApprovalRequirement {
        let parsed: std::result::Result<BashArgs, _> = serde_json::from_str(args);
        let desc = match parsed {
            Ok(a) => format!("Run: {}", a.command),
            Err(_) => "Run bash command (could not parse arguments)".to_string(),
        };
        ApprovalRequirement::RequireApproval(desc)
    }

    async fn execute(&self, args: &str) -> Result<ToolResult> {
        let parsed: BashArgs = serde_json::from_str(args)?;
        let timeout_secs = parsed.timeout.unwrap_or(120);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            Command::new("bash")
                .arg("-c")
                .arg(&parsed.command)
                .current_dir(&self.working_dir)
                .output(),
        ).await;
        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let combined = if stderr.is_empty() { stdout }
                    else if stdout.is_empty() { format!("STDERR:\n{}", stderr) }
                    else { format!("{}\nSTDERR:\n{}", stdout, stderr) };
                Ok(ToolResult { call_id: String::new(), output: combined, success: output.status.success() })
            }
            Ok(Err(e)) => Ok(ToolResult { call_id: String::new(), output: format!("Failed to execute: {}", e), success: false }),
            Err(_) => Ok(ToolResult { call_id: String::new(), output: format!("Timed out after {}s", timeout_secs), success: false }),
        }
    }
}
