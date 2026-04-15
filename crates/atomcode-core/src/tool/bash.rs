use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

pub struct BashTool;

/// Default overall timeout for bash commands. Bumped from 30→60 so common long
/// commands (cargo build cold, mvn download, npm install, git clone) don't need
/// the model to remember to pass `timeout`. Still capped at 300s in execute().
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// How long a process can be silent (no new stdout/stderr) AFTER having emitted
/// something, before we kill it. Bumped from 30→90 to tolerate legitimate silent
/// phases (file lock waits, dependency downloads, linker blocking, large file
/// reads). This is NOT tool- or language-specific — any process with these
/// patterns benefits. Tradeoff: genuine deadlocks wait 60s longer than before.
const SILENT_KILL_SECS: u64 = 90;

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
                Default timeout: 60s. Destructive commands require user confirmation.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The bash command to execute" },
                    "timeout": { "type": "integer", "description": "Max wait seconds (default 60, max 300)" }
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
        let timeout_secs = parsed.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS).min(300);
        let start_instant = Instant::now();

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
        // Idle detection: if output stops for SILENT_KILL_SECS after having produced
        // some output, assume the command is truly stuck. This threshold needs to
        // tolerate legitimate silent phases common across many tools/languages
        // (build cache scan, dep lock waits, dep downloads, large file I/O, linking,
        // compiler type-check pass, etc.) — none of which emit progress to stdout.
        let idle_timeout = Duration::from_secs(SILENT_KILL_SECS);
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

        // Total elapsed wall-clock — appended to every result so the agent can
        // judge "slow but succeeded" vs "stalled/hung" without any per-tool
        // pattern matching. Purely numeric, tech-neutral.
        let elapsed_secs = start_instant.elapsed().as_secs_f64();
        let elapsed_marker = format!("[elapsed: {:.1}s]", elapsed_secs);

        match result {
            Ok(Some(success)) => {
                let mut combined = format_output(&stdout_str, &stderr_str);
                // For background/pkill commands: non-empty output = success
                let effective_success = success || has_background || (has_pkill && !combined.is_empty());

                if !effective_success && !combined.is_empty() {
                    combined.push_str("\n\n[IMPORTANT: Command failed. Read the error above and fix the root cause. Do NOT retry the same command.]");
                }
                // Prepend elapsed so it's visible even when output is truncated later
                let output = if combined.is_empty() {
                    elapsed_marker
                } else {
                    format!("{}\n{}", elapsed_marker, combined)
                };
                Ok(ToolResult { call_id: String::new(), output, success: effective_success })
            }
            Ok(None) => {
                // Process still running but output stopped for SILENT_KILL_SECS = likely stuck.
                // Kill it. Include elapsed time so agent can tell slow-work vs deadlock.
                let _ = child.kill().await;
                let combined = format_output(&stdout_str, &stderr_str);
                let output = if combined.is_empty() {
                    format!(
                        "{} [killed: no output for {}s — treat as stuck, don't retry the same command]",
                        elapsed_marker, SILENT_KILL_SECS
                    )
                } else {
                    format!(
                        "{}\n{}\n\n[killed: no new output for {}s — output above is partial]",
                        elapsed_marker, combined, SILENT_KILL_SECS
                    )
                };
                Ok(ToolResult { call_id: String::new(), output, success: false })
            }
            Err(_) => {
                // Hard timeout — kill it
                let _ = child.kill().await;
                let combined = format_output(&stdout_str, &stderr_str);
                let output = if combined.is_empty() {
                    format!("{} [timed out after {}s with no output]", elapsed_marker, timeout_secs)
                } else {
                    format!(
                        "{}\n{}\n\n[timed out after {}s — consider passing a larger `timeout` if this command legitimately takes longer]",
                        elapsed_marker, combined, timeout_secs
                    )
                };
                Ok(ToolResult { call_id: String::new(), output, success: false })
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
    let stdout = sanitize_terminal_output(stdout);
    let stderr = sanitize_terminal_output(stderr);
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

/// Strip ANSI escape sequences and resolve `\r` progress-line rewrites so bash
/// output is safe to splice into ratatui cells. Without this, git hooks / cargo /
/// docker / progress bars emit CSI sequences and `\r` cursor-returns; ratatui
/// stores them verbatim in buffer cells, and when crossterm flushes, the host
/// terminal executes them — shifting the cursor mid-frame, stranding `[PASSED]`
/// fragments at the right edge of the screen, and leaking content outside the
/// tool block that captured it.
fn sanitize_terminal_output(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    // Strip ANSI escape sequences: CSI (`ESC [ … final`), OSC (`ESC ] … BEL|ST`),
    // and solo two-byte escapes (`ESC X`). Done in a single pass over bytes so
    // we don't need the `regex` crate here.
    let bytes = s.as_bytes();
    let mut stripped: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b && i + 1 < bytes.len() {
            let next = bytes[i + 1];
            match next {
                b'[' => {
                    // CSI: ESC [ (params: 0x30-0x3f) (intermediates: 0x20-0x2f) (final: 0x40-0x7e)
                    let mut j = i + 2;
                    while j < bytes.len() && (0x30..=0x3f).contains(&bytes[j]) { j += 1; }
                    while j < bytes.len() && (0x20..=0x2f).contains(&bytes[j]) { j += 1; }
                    if j < bytes.len() { j += 1; } // consume final byte
                    i = j;
                    continue;
                }
                b']' => {
                    // OSC: ESC ] ... (BEL | ESC \)
                    let mut j = i + 2;
                    while j < bytes.len() {
                        if bytes[j] == 0x07 { j += 1; break; }
                        if bytes[j] == 0x1b && j + 1 < bytes.len() && bytes[j + 1] == b'\\' {
                            j += 2; break;
                        }
                        j += 1;
                    }
                    i = j;
                    continue;
                }
                _ => {
                    // Two-byte escape (e.g. ESC =, ESC >, ESC M, …) — drop both.
                    i += 2;
                    continue;
                }
            }
        }
        stripped.push(b);
        i += 1;
    }
    // Lossy decode: the strip phase removes whole escape sequences, but a
    // pathological ESC followed by a UTF-8 continuation byte could still
    // produce invalid UTF-8 — lossy keeps us safe without another allocation
    // in the common case.
    let cleaned = String::from_utf8_lossy(&stripped).into_owned();

    // Resolve `\r` progress rewrites. For each logical line, when `\r` appears
    // mid-line the terminal would repaint from column 0, so only the suffix
    // after the final `\r` is actually visible to the user. We keep just that.
    let mut out = String::with_capacity(cleaned.len());
    for (idx, line) in cleaned.split('\n').enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        let line = line.trim_end_matches('\r');
        if let Some(pos) = line.rfind('\r') {
            out.push_str(&line[pos + 1..]);
        } else {
            out.push_str(line);
        }
    }

    // Drop any remaining C0 control characters except tab — they render as
    // glyph garbage or misbehave in ratatui cells.
    out.chars()
        .filter(|c| *c == '\n' || *c == '\t' || !c.is_control())
        .collect()
}

#[cfg(test)]
mod sanitize_tests {
    use super::sanitize_terminal_output;

    #[test]
    fn strips_csi_color_sequences() {
        let input = "\x1b[32m[PASSED]\x1b[0m done";
        assert_eq!(sanitize_terminal_output(input), "[PASSED] done");
    }

    #[test]
    fn collapses_progress_rewrites() {
        let input = "Downloading 10%\rDownloading 50%\rDownloading 100%";
        assert_eq!(sanitize_terminal_output(input), "Downloading 100%");
    }

    #[test]
    fn preserves_multiline_progress() {
        let input = "step1: ok\nDownloading 10%\rDownloading 100%\nstep3: ok";
        assert_eq!(
            sanitize_terminal_output(input),
            "step1: ok\nDownloading 100%\nstep3: ok"
        );
    }

    #[test]
    fn strips_cursor_movement() {
        let input = "remote: Checking\x1b[K\r\x1b[A[PASSED]";
        let out = sanitize_terminal_output(input);
        assert!(!out.contains('\x1b'));
        assert!(!out.contains('\r'));
    }

    #[test]
    fn normalizes_crlf() {
        let input = "a\r\nb\r\nc";
        assert_eq!(sanitize_terminal_output(input), "a\nb\nc");
    }

    #[test]
    fn keeps_utf8() {
        let input = "中文 \x1b[1m粗体\x1b[0m 结束";
        assert_eq!(sanitize_terminal_output(input), "中文 粗体 结束");
    }

    #[test]
    fn drops_bel_and_other_c0() {
        let input = "hello\x07world\x08";
        assert_eq!(sanitize_terminal_output(input), "helloworld");
    }
}
