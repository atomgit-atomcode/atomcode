use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use super::devserver;
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
            description: "Execute a shell command and return its output (stdout + stderr).\n\
                When to use:\n\
                - Build/compile: npm run build, cargo build, make, etc.\n\
                - Run tests: npm test, pytest, cargo test, etc.\n\
                - Git commands: git status, git diff, git log, git add, git commit, etc.\n\
                - Install packages: npm install, pip install, cargo add, etc.\n\
                - Start/restart servers: npm run dev, python manage.py runserver, etc.\n\
                - System commands: ls, mkdir, which, curl (for API testing), etc.\n\
                When NOT to use (use dedicated tools instead):\n\
                - Reading files: use read_file, NOT cat/head/tail\n\
                - Searching content: use grep tool, NOT bash grep/rg/awk\n\
                - Finding files: use glob tool, NOT bash find\n\
                - Editing files: use edit_file, NOT sed/awk\n\
                Behavior:\n\
                - Default timeout: 30 seconds. Use 'timeout' parameter for longer commands.\n\
                - Long-running server processes: returns after 10s with partial output (server keeps running).\n\
                - Destructive commands (rm -rf, drop, etc.) require user confirmation.".to_string(),
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
        let mut parsed: BashArgs = serde_json::from_str(args)?;
        // Cap timeout: model may request absurdly large values. Max 5 min for
        // normal commands, 3 min for background/server commands.
        let timeout_secs = parsed.timeout.unwrap_or(30).min(300);

        let wd = ctx.working_dir.read().await.clone();

        // For commands that background a server process, wrap with nohup and
        // redirect stdout/stderr to prevent SIGPIPE when the bash tool exits.
        // Without this, the server dies as soon as our stdout reader drops the pipe.
        #[cfg(not(target_os = "windows"))]
        {
            let cmd_trimmed = parsed.command.trim();
            // Detect pattern: "some_command &" or "some_command & other_command"
            // where the backgrounded part is a server/dev command
            if devserver::is_server_command(cmd_trimmed)
                && !cmd_trimmed.contains("nohup")
                && !cmd_trimmed.contains(">/dev/null")
                && !cmd_trimmed.contains("&>/dev/null")
            {
                // Find the backgrounded command and add nohup + redirect
                if let Some(amp_pos) = cmd_trimmed.find(" &") {
                    let bg_cmd = cmd_trimmed[..amp_pos].trim();
                    let rest = cmd_trimmed[amp_pos + 2..].trim();
                    if rest.is_empty() {
                        parsed.command = format!("nohup {} >/dev/null 2>&1 &", bg_cmd);
                    } else {
                        parsed.command = format!("nohup {} >/dev/null 2>&1 & {}", bg_cmd, rest);
                    }
                }
            }
        }

        // NOTE: Pre-compile interception disabled — blocking the model's command
        // confuses weak models into retrying with different flags instead of fixing
        // the compile error. The system prompt rule "ALWAYS compile/build BEFORE
        // starting a server" is sufficient guidance.

        // Platform-aware shell: cmd.exe on Windows, bash on Unix
        #[cfg(target_os = "windows")]
        let mut child = Command::new("cmd.exe")
            .args(&["/C", &parsed.command])
            .current_dir(&wd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;

        // Detect long-running / background commands — these get their own process group
        // so they survive atomcode exit and won't be killed on timeout.
        let is_background = devserver::is_server_command(&parsed.command);

        #[cfg(not(target_os = "windows"))]
        let mut child = {
            let mut cmd = Command::new("bash");
            cmd.arg("-c")
                .arg(&parsed.command)
                .current_dir(&wd)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            if is_background {
                // Own process group so the server survives atomcode exit
                cmd.process_group(0);
            }
            cmd.spawn()?
        };

        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();

        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();

        // Wait for process to finish or timeout. Read stdout/stderr concurrently.
        // Idle detection (3s no output → early return) ONLY for background/server commands.
        // Non-background commands (curl, mvn, etc.) wait the full timeout.
        let wait_secs = if is_background {
            parsed.timeout.unwrap_or(INITIAL_WAIT_SECS).min(300)
        } else {
            parsed.timeout.unwrap_or(30).min(300)
        };
        // For background commands: 3s idle = server started. For others: no idle detection.
        let idle_timeout = if is_background {
            Duration::from_secs(3)
        } else {
            Duration::from_secs(wait_secs + 1) // effectively disabled
        };
        let has_any_output = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let has_out_1 = has_any_output.clone();
        let has_out_2 = has_any_output.clone();
        let result = tokio::time::timeout(
            Duration::from_secs(wait_secs),
            async {
                let (_, _) = tokio::join!(
                    async {
                        let mut buf = vec![0u8; 65536];
                        loop {
                            match tokio::time::timeout(idle_timeout, stdout.read(&mut buf)).await {
                                Ok(Ok(0)) => break,
                                Ok(Ok(n)) => {
                                    stdout_buf.extend_from_slice(&buf[..n]);
                                    has_out_1.store(true, std::sync::atomic::Ordering::Relaxed);
                                }
                                Ok(Err(_)) => break,
                                Err(_) => {
                                    // No new stdout for 3s — if we have ANY output, break
                                    if has_out_1.load(std::sync::atomic::Ordering::Relaxed) {
                                        break;
                                    }
                                }
                            }
                        }
                    },
                    async {
                        let mut buf = vec![0u8; 65536];
                        loop {
                            match tokio::time::timeout(idle_timeout, stderr.read(&mut buf)).await {
                                Ok(Ok(0)) => break,
                                Ok(Ok(n)) => {
                                    stderr_buf.extend_from_slice(&buf[..n]);
                                    has_out_2.store(true, std::sync::atomic::Ordering::Relaxed);
                                }
                                Ok(Err(_)) => break,
                                Err(_) => {
                                    if has_out_2.load(std::sync::atomic::Ordering::Relaxed) {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                );

                match child.try_wait() {
                    Ok(Some(status)) => Some(status.success()),
                    _ => None,
                }
            }
        ).await;

        let stdout_str = String::from_utf8_lossy(&stdout_buf).to_string();
        let stderr_str = String::from_utf8_lossy(&stderr_buf).to_string();

        // Commands with & (backgrounded processes) may return non-zero even on success.
        // pkill returns 1 when no process matched. These shouldn't be marked as failures.
        let has_background = parsed.command.contains(" &");
        let has_pkill = parsed.command.contains("pkill");

        match result {
            Ok(Some(success)) => {
                let mut combined = format_output(&stdout_str, &stderr_str);
                // For background/pkill commands: non-empty output = success
                let effective_success = success || has_background || (has_pkill && !combined.is_empty());
                if !effective_success && !combined.is_empty() {
                    combined.push_str("\n\n[IMPORTANT: Command failed. Read the error above and fix the root cause. Do NOT retry the same command.]");
                }
                Ok(ToolResult { call_id: String::new(), output: combined, success: effective_success })
            }
            Ok(None) => {
                // Process still running but output stopped (idle timeout).
                if is_background {
                    // Dev server: idle = likely started successfully. Don't kill.
                    let pid = child.id().map(|p| p.to_string()).unwrap_or_else(|| "?".into());
                    let combined = format_output(&stdout_str, &stderr_str);
                    let output = if combined.is_empty() {
                        format!("Process still running (PID: {}). No output captured yet.", pid)
                    } else {
                        format!("{}\n\n[Process running in background, PID: {}. Output stopped — likely started successfully. Do NOT wait for it to exit.]", combined, pid)
                    };
                    Ok(ToolResult { call_id: String::new(), output, success: true })
                } else {
                    // Non-background command (curl, etc.): shouldn't reach here normally,
                    // but if it does, wait for the process to finish.
                    let _ = child.wait().await;
                    let combined = format_output(&stdout_str, &stderr_str);
                    if combined.is_empty() {
                        Ok(ToolResult { call_id: String::new(), output: "(no output)".to_string(), success: true })
                    } else {
                        Ok(ToolResult { call_id: String::new(), output: combined, success: true })
                    }
                }
            }
            Err(_) => {
                if is_background {
                    // Background/server command — don't kill, let it run
                    let pid = child.id().map(|p| p.to_string()).unwrap_or_else(|| "?".into());
                    let combined = format_output(&stdout_str, &stderr_str);

                    // Auto-detect port from command and poll until ready
                    let port = devserver::extract_port_with_dir(&parsed.command, Some(&wd));
                    let port_status = if let Some(p) = port {
                        // Poll port every 2s for up to 30s
                        let mut ready = false;
                        for _ in 0..15 {
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                            if std::net::TcpStream::connect(format!("127.0.0.1:{}", p)).is_ok() {
                                ready = true;
                                break;
                            }
                        }
                        if ready {
                            format!(" Port {} is ready.", p)
                        } else {
                            format!(" Port {} not responding after 30s — check logs.", p)
                        }
                    } else {
                        String::new()
                    };

                    let output = if combined.is_empty() {
                        format!("Process running in background (PID: {}).{}", pid, port_status)
                    } else {
                        format!("{}\n\n[Process running in background, PID: {}]", combined, pid)
                    };
                    Ok(ToolResult { call_id: String::new(), output, success: true })
                } else {
                    // Non-background command — hard timeout, kill it
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
        ("killall ", "Kill all matching processes"),
        ("git push --force", "Force push"),
        ("git push -f", "Force push"),
        ("git reset --hard", "Hard reset (destroys uncommitted changes)"),
        ("git clean -f", "Force clean untracked files"),
    ];

    for (pattern, reason) in patterns {
        if cmd.contains(pattern) {
            // Don't flag pkill/pgrep — standard process management
            if pattern.contains("kill") && (cmd.contains("pkill") || cmd.contains("pgrep")) {
                continue;
            }
            // Don't flag `kill -9 <PID>` or `kill <PID>` targeting a specific process.
            // Also allow piped kill patterns like `lsof -ti:PORT | xargs kill -9`
            // which are standard dev server restart operations.
            if pattern.contains("kill") {
                let is_targeted_kill = cmd.contains("| xargs kill")
                    || cmd.contains("| kill")
                    || {
                        // `kill -9 12345` — numeric PID follows
                        let after_kill = if let Some(pos) = cmd.find("kill -9") {
                            cmd[pos + 7..].trim_start()
                        } else if let Some(pos) = cmd.find("kill ") {
                            cmd[pos + 5..].trim_start()
                        } else { "" };
                        after_kill.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false)
                    };
                if is_targeted_kill {
                    continue;
                }
            }
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
