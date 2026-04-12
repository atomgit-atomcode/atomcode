use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

pub struct BashTool;

/// Deserialize a u64 that may arrive as a JSON string (weak models often quote integers).
fn deserialize_lenient_u64<'de, D>(deserializer: D) -> std::result::Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct LenientU64;

    impl<'de> de::Visitor<'de> for LenientU64 {
        type Value = Option<u64>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a u64 or a string containing a u64")
        }
        fn visit_none<E: de::Error>(self) -> std::result::Result<Self::Value, E> { Ok(None) }
        fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> { Ok(None) }
        fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> { Ok(Some(v)) }
        fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
            if v >= 0 { Ok(Some(v as u64)) } else { Err(de::Error::custom("negative timeout")) }
        }
        fn visit_f64<E: de::Error>(self, v: f64) -> std::result::Result<Self::Value, E> { Ok(Some(v as u64)) }
        fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Self::Value, E> {
            let s = v.trim();
            // Try u64 first, then f64 (models often send "60.0" instead of 60)
            s.parse::<u64>().map(Some)
                .or_else(|_| s.parse::<f64>().map(|f| Some(f as u64)))
                .map_err(de::Error::custom)
        }
    }

    deserializer.deserialize_any(LenientU64)
}

#[derive(Deserialize)]
struct BashArgs {
    command: String,
    #[serde(default, deserialize_with = "deserialize_lenient_u64")]
    timeout: Option<u64>,
}

#[async_trait]
impl Tool for BashTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "bash",
            description: "Execute a shell command. Use for: build, test, git, install deps.\n\
                Do NOT use for: reading files (use read_file), searching (use grep), editing (use edit_file).\n\
                Do NOT start servers or long-running processes — the user manages those.\n\
                Default timeout: 30s. Destructive commands require user confirmation.".to_string(),
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
        let parsed = match serde_json::from_str::<BashArgs>(args) {
            Ok(p) => p,
            Err(_) => {
                // Fail-closed: unparseable args require approval.
                return ApprovalRequirement::RequireApproval(
                    "Could not parse bash arguments for safety check.".to_string(),
                );
            }
        };
        if let Some(reason) = check_destructive_command(&parsed.command) {
            return ApprovalRequirement::RequireApproval(reason);
        }
        ApprovalRequirement::AutoApprove
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let mut result = bash_execute(args, ctx).await?;
        // Append cwd to every bash result so model always knows where it is.
        // This prevents "forgot to cd" loops (e.g., 5x "no POM in this directory").
        let wd = ctx.working_dir.read().await;
        result.output.push_str(&format!("\n[cwd: {}]", wd.display()));
        Ok(result)
    }
}

async fn bash_execute(args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let mut parsed: BashArgs = serde_json::from_str(args)?;
        // Strip model-added tail/head pipes — framework's truncation handles output length.
        parsed.command = strip_output_pipes(&parsed.command);
        // Cap timeout: model may request absurdly large values. Max 5 min.
        let timeout_secs = parsed.timeout.unwrap_or(30).min(300);

        let wd = ctx.working_dir.read().await.clone();

        // Platform-aware shell: cmd.exe on Windows, bash on Unix
        #[cfg(target_os = "windows")]
        let mut child = Command::new("cmd.exe")
            .args(&["/C", &parsed.command])
            .current_dir(&wd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;

        #[cfg(not(target_os = "windows"))]
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
        // Idle detection: if output stops for 30s after having produced some output,
        // the command is likely stuck. Kill it early instead of waiting for the full timeout.
        let idle_timeout = Duration::from_secs(30);
        let has_any_output = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let has_out_1 = has_any_output.clone();
        let has_out_2 = has_any_output.clone();
        let result = tokio::time::timeout(
            Duration::from_secs(timeout_secs),
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
                // Process still running but output stopped for 30s = stuck. Kill it.
                let _ = child.kill().await;
                let combined = format_output(&stdout_str, &stderr_str);
                let hint = stall_hint(&parsed.command);
                if combined.is_empty() {
                    Ok(ToolResult { call_id: String::new(), output: format!(
                        "Command produced no output and was killed after 30s idle.\n{}", hint
                    ), success: false })
                } else {
                    Ok(ToolResult { call_id: String::new(), output: format!(
                        "{}\n\n[Command stalled — no output for 30s. Killed.]\n{}", combined, hint
                    ), success: false })
                }
            }
            Err(_) => {
                // Hard timeout — kill it
                let _ = child.kill().await;
                let combined = format_output(&stdout_str, &stderr_str);
                let hint = stall_hint(&parsed.command);
                let output = if combined.is_empty() {
                    format!("Timed out after {}s with no output.\n{}", timeout_secs, hint)
                } else {
                    format!("{}\n\n[Timed out after {}s, process killed]\n{}", combined, timeout_secs, hint)
                };
                Ok(ToolResult { call_id: String::new(), output, success: false })
            }
        }
    }

/// Provide actionable guidance when a bash command stalls or times out.
/// Instead of vague "try a different approach", tell the model what likely went wrong.
fn stall_hint(command: &str) -> String {
    let cmd = command.to_lowercase();
    if cmd.contains("cargo run") || cmd.contains("python") && cmd.contains("run")
        || cmd.contains("npm start") || cmd.contains("flask run") || cmd.contains("uvicorn")
    {
        "[HINT: This looks like a server/long-running process. Do NOT run servers — \
         use other tools to verify (cargo test, read_file, grep).]".to_string()
    } else if cmd.contains("cargo build") || cmd.contains("cargo check") || cmd.contains("mvn") || cmd.contains("gradle") {
        "[HINT: Build timed out. Check for compilation errors in the output above. \
         If the build is too slow, try building a smaller scope (single package).]".to_string()
    } else if cmd.contains("curl") || cmd.contains("wget") {
        "[HINT: Network request timed out. The service may not be running or the port is wrong. \
         Check if the server is up, or read the code to verify the correct port/endpoint.]".to_string()
    } else if cmd.contains("pip install") || cmd.contains("npm install") || cmd.contains("cargo install") {
        "[HINT: Package installation timed out. Try installing with --no-deps or check network.]".to_string()
    } else {
        "[HINT: Command stalled. Analyze the partial output (if any) and try a different approach.]".to_string()
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


/// Strip model-added `| tail -N` / `| head -N` from the end of bash commands.
/// The framework's truncation system manages output length — model shouldn't self-truncate.
/// Preserves `tail -f` (streaming), `| grep` (filtering), `| sort` (semantics).
fn strip_output_pipes(cmd: &str) -> String {
    let trimmed = cmd.trim_end();
    // Find last pipe
    if let Some(pipe_pos) = trimmed.rfind('|') {
        let after_pipe = trimmed[pipe_pos + 1..].trim();
        // Check if it's `tail -N`, `tail -n N`, `head -N`, `head -n N`
        let is_tail_head = (after_pipe.starts_with("tail ") || after_pipe.starts_with("head "))
            && !after_pipe.contains("-f")  // preserve tail -f (streaming)
            && after_pipe.chars().any(|c| c.is_ascii_digit()); // must have a number
        if is_tail_head {
            return trimmed[..pipe_pos].trim_end().to_string();
        }
    }
    cmd.to_string()
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
