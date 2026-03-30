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
            if is_background_command(cmd_trimmed)
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
        let is_background = is_background_command(&parsed.command);

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
        // Tech-stack-agnostic idle detection: if the process is still running but
        // output has stopped for 3s, it's likely a long-running process (dev server)
        // that started successfully. Return early without killing it.
        let wait_secs = parsed.timeout.unwrap_or(INITIAL_WAIT_SECS).min(300);
        let idle_timeout = Duration::from_secs(3);
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
                // This is the typical pattern for dev servers: they print startup info then go quiet.
                let pid = child.id().map(|p| p.to_string()).unwrap_or_else(|| "?".into());
                let combined = format_output(&stdout_str, &stderr_str);
                let output = if combined.is_empty() {
                    format!("Process still running (PID: {}). No output captured yet.", pid)
                } else {
                    format!("{}\n\n[Process running in background, PID: {}. Output stopped — likely started successfully. Do NOT wait for it to exit.]", combined, pid)
                };
                Ok(ToolResult { call_id: String::new(), output, success: true })
            }
            Err(_) => {
                if is_background {
                    // Background/server command — don't kill, let it run
                    let pid = child.id().map(|p| p.to_string()).unwrap_or_else(|| "?".into());
                    let combined = format_output(&stdout_str, &stderr_str);

                    // Auto-detect port from command and poll until ready
                    let port = extract_port(&parsed.command);
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
        ("killall", "Kill all matching processes"),
        ("git push --force", "Force push"),
        ("git push -f", "Force push"),
        ("git reset --hard", "Hard reset (destroys uncommitted changes)"),
        ("git clean -f", "Force clean untracked files"),
    ];

    for (pattern, reason) in patterns {
        if cmd.contains(pattern) {
            // Don't flag pkill/pgrep — they're standard process management commands
            if pattern.contains("kill") && (cmd.contains("pkill") || cmd.contains("pgrep")) {
                continue;
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

/// Detect commands intended to run in the background / as long-lived servers.
/// These should not be killed on timeout and should get their own process group.
fn is_background_command(cmd: &str) -> bool {
    let trimmed = cmd.trim();
    // Explicit background: trailing &, nohup, setsid
    if trimmed.ends_with('&') || trimmed.contains("nohup ") || trimmed.contains("setsid ") {
        return true;
    }
    // Common dev server commands
    let server_patterns = [
        "npm run dev", "npm start", "npx ", "yarn dev", "pnpm dev",
        "python -m http", "python manage.py runserver", "uvicorn ", "gunicorn ",
        "cargo run", "go run", "node server", "flask run", "rails s",
        "mvn spring-boot:run", "mvn spring-boot:", "gradle bootRun",
        "java -jar", "java -cp",
    ];
    server_patterns.iter().any(|p| trimmed.contains(p))
}

/// Extract port number from a command string.
/// Detects patterns like `:8080`, `--port 3000`, `-p 8080`, `PORT=3000`.
fn extract_port(cmd: &str) -> Option<u16> {
    // Common default ports by tool
    let defaults: &[(&str, u16)] = &[
        ("spring-boot:run", 8080),
        ("npm run dev", 3000), ("npm start", 3000),
        ("vite", 5173), ("next", 3000),
        ("flask run", 5000), ("uvicorn", 8000),
        ("rails s", 3000), ("cargo run", 8080),
    ];

    // Check for explicit port in command
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

    // Fall back to defaults
    for (pattern, port) in defaults {
        if cmd.contains(pattern) {
            return Some(*port);
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
