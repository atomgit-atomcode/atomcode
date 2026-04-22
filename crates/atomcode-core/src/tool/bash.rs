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
        // Capture workspace state before exec. If the command later turns out
        // to have modified files, we surface the list to the agent so it can
        // tell when bash went around edit_file. `.gitignore` drives what
        // "counts" — tech-stack neutral, no pattern list of tool names.
        let pre_wd = ctx.working_dir.read().await.clone();
        let workspace_before = snapshot_workspace_changes(&pre_wd).await;

        let mut result = bash_execute(args, ctx).await?;

        // Detect `cd` commands and update the shared working directory so the
        // status bar and subsequent bash calls reflect the change.  Without
        // this, `cd /path` in a child process only affects that process — the
        // TUI keeps showing the old directory.
        if result.success {
            if let Ok(parsed) = serde_json::from_str::<BashArgs>(args) {
                if let Some(new_dir) = detect_cd_target(&parsed.command) {
                    let current = ctx.working_dir.read().await.clone();
                    let resolved = if new_dir.starts_with('/') {
                        std::path::PathBuf::from(&new_dir)
                    } else if new_dir.starts_with('~') {
                        dirs::home_dir()
                            .map(|h| h.join(new_dir.strip_prefix("~/").unwrap_or(&new_dir[1..])))
                            .unwrap_or_else(|| std::path::PathBuf::from(&new_dir))
                    } else {
                        current.join(&new_dir)
                    };
                    let resolved = std::fs::canonicalize(&resolved).unwrap_or(resolved);
                    if resolved.is_dir() {
                        let mut wd = ctx.working_dir.write().await;
                        *wd = resolved;
                    }
                }
            }
        }

        // Workspace-change detection: if a bash command modified files that
        // weren't already modified before (new untracked / newly modified
        // tracked), surface them. Purely effect-based — catches `sed -i`,
        // `perl -pi`, `echo > file`, `python edit_script.py`, and any other
        // path to "bash wrote to source files". Silent no-op outside git repos.
        //
        // Bounded: max 5 files listed to keep the nudge compact. The goal is
        // to nudge the agent toward edit_file (which has diff review + undo),
        // not to block the bash call.
        if let Some(before) = workspace_before {
            let post_wd = ctx.working_dir.read().await.clone();
            if let Some(after) = snapshot_workspace_changes(&post_wd).await {
                let added: Vec<&String> = after.difference(&before).collect();
                if !added.is_empty() {
                    let shown = added.iter().take(5).map(|s| s.as_str()).collect::<Vec<_>>().join(", ");
                    let more = if added.len() > 5 {
                        format!(", +{} more", added.len() - 5)
                    } else {
                        String::new()
                    };
                    result.output.push_str(&format!(
                        "\n[workspace modified via bash: {}{}. If you meant to edit source, \
                         use edit_file next time — it tracks diffs and supports /undo.]",
                        shown, more
                    ));
                }
            }
        }

        // Auto-STOP nudge on resolved error (P0 #5, multi-sig revision
        // 2026-04-22 evening): on the first bash failure in a session,
        // record top-5 longest substantive lines as a "fingerprint" of the
        // original failure. On a subsequent bash SUCCESS where ≥3 of those
        // 5 lines are now absent, append a hint nudging the model to
        // summarize and stop instead of drifting into unrelated refactors.
        //
        // Why ≥3/5 majority, not any single line: tools like `cargo` emit
        // ambient status ("Blocking waiting for file lock", "Checking
        // v0.1.0") on both failure and success. A single-line signature
        // locks onto a status line and never fires the nudge. Majority
        // rule is robust to that noise without per-tool pattern lists.
        //
        // Informational only — no hard STOP. The model decides.
        {
            let mut sigs_lock = ctx.first_error_signatures.write().await;
            if !result.success {
                if sigs_lock.is_empty() {
                    let sigs = super::extract_error_signatures(&result.output);
                    if !sigs.is_empty() {
                        *sigs_lock = sigs;
                    }
                }
            } else if !sigs_lock.is_empty() {
                let absent_count = sigs_lock
                    .iter()
                    .filter(|s| !result.output.contains(s.as_str()))
                    .count();
                // Fire when ≥50% of recorded sigs are now absent.
                //
                // Wording history: the first version said "summarize and
                // stop instead of continuing with unrelated changes". The
                // hermes 2026-04-22 21-06 session exposed that weak models
                // take "stop" as a direct command and skip remaining
                // user-requested steps — e.g. user asked for 3 cargo
                // checks, 2nd passed, nudge fired, model stopped after 2.
                // Also: quoting a specific sig ("Compiling hermes-tauri
                // v0.1.0 …") was misleading because length-sort picked a
                // cargo STATUS line rather than an error line.
                //
                // Now purely informational: no stop directive, no quoted
                // line. Tells the model what changed without overriding
                // the user's multi-step request.
                if absent_count > 0 && absent_count * 2 >= sigs_lock.len() {
                    result.output.push_str(
                        "\n[Note: the workspace no longer shows the key diagnostic lines \
                         from the earlier failure. The fix looks landed. Continue with \
                         any remaining steps the user asked for; only summarize if the \
                         full original request is done.]"
                    );
                }
            }
        }

        // Append cwd to every bash result so model always knows where it is.
        let wd = ctx.working_dir.read().await;
        result.output.push_str(&format!("\n[cwd: {}]", wd.display()));
        Ok(result)
    }
}

/// Snapshot the set of files currently showing as changed / untracked per
/// `git status --porcelain -uall`. Returns `None` when the directory isn't
/// inside a git repo or git is unavailable — detection silently skips so
/// non-git workflows see no behavior change.
///
/// Effect-based detection is the project's replacement for hand-maintained
/// lists of "dangerous" shell tools (sed -i / perl -pi / awk -i / ed / …).
/// `.gitignore` naturally excludes build artifacts (`target/`, `node_modules/`,
/// `dist/`, `__pycache__/`), so a `cargo build` that writes into `target/`
/// doesn't spuriously trigger the nudge — the user controls the boundary,
/// not a pattern list the framework maintains.
async fn snapshot_workspace_changes(
    wd: &std::path::Path,
) -> Option<std::collections::HashSet<String>> {
    let out = tokio::process::Command::new("git")
        .args(["status", "--porcelain", "-uall"])
        .current_dir(wd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut set = std::collections::HashSet::new();
    for line in text.lines() {
        // `git status --porcelain` format: `XY <path>` (2-char status + space
        // + path). We only care about identity ("was this file touched?"),
        // not the status code, so strip the 3-char prefix.
        if line.len() > 3 {
            set.insert(line[3..].to_string());
        }
    }
    Some(set)
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
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;

        #[cfg(not(target_os = "windows"))]
        let mut child = {
            let mut cmd = Command::new("bash");
            cmd.arg("-c")
                .arg(&parsed.command)
                .current_dir(&wd)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            // Detach child from the controlling terminal so neither it nor any
            // grandchild (ssh, git credential helpers, server-side hook output
            // rendered by git) can write directly to /dev/tty.  Without this,
            // programs that open /dev/tty bypass our piped stdout/stderr and
            // scribble ANSI escape sequences onto the TUI — producing artifacts
            // like the [PASSED] box from AtomGit push hooks.
            unsafe {
                cmd.pre_exec(|| {
                    extern "C" {
                        fn setsid() -> i32;
                        fn open(path: *const u8, oflag: i32) -> i32;
                        fn close(fd: i32) -> i32;
                        fn ioctl(fd: i32, request: u64, ...) -> i32;
                    }
                    // Create a new session — detaches from the controlling
                    // terminal so /dev/tty opens fail.
                    setsid();
                    // Belt-and-suspenders: also try to explicitly detach using
                    // TIOCNOTTY, which works even when setsid alone doesn't
                    // fully sever the connection on some macOS versions.
                    const O_RDWR: i32 = 2;
                    #[cfg(target_os = "macos")]
                    const TIOCNOTTY: u64 = 0x20007471;
                    #[cfg(not(target_os = "macos"))]
                    const TIOCNOTTY: u64 = 0x5422;
                    let tty_fd = open(b"/dev/tty\0".as_ptr(), O_RDWR);
                    if tty_fd >= 0 {
                        ioctl(tty_fd, TIOCNOTTY);
                        close(tty_fd);
                    }
                    Ok(())
                });
            }
            cmd.spawn()?
        };

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

                // Capture both the success flag AND the numeric exit code.
                // Previously only `.success()` was read, which meant a failed
                // command with empty stdout/stderr came back as bare
                // "[elapsed: 0.0s]" — agent had zero signal on whether the
                // command ran, was denied by the shell, or exited for a
                // specific reason (e.g. grep's exit 1 = no match, exit 2 =
                // real error; agent cannot tell these apart without the code).
                //
                // Two-stage wait to close a kernel-level race: for fast
                // commands (true, echo, grep with no match) stdout/stderr hit
                // EOF before SIGCHLD is observed, so a bare try_wait() sees
                // `None` and the result gets misclassified as "idle kill".
                // After the pipes close, we know the child is essentially
                // done — give the reaper a tiny window to catch up before
                // declaring it stuck. 100ms is well under human-perceptible
                // latency and sufficient for any real reap on modern kernels.
                match child.try_wait() {
                    Ok(Some(status)) => Some((status.success(), status.code())),
                    _ => match tokio::time::timeout(
                        Duration::from_millis(100),
                        child.wait(),
                    )
                    .await
                    {
                        Ok(Ok(status)) => Some((status.success(), status.code())),
                        _ => None,
                    },
                }
            }
        ).await;

        let stdout_str = String::from_utf8_lossy(&stdout_buf).to_string();
        let stderr_str = String::from_utf8_lossy(&stderr_buf).to_string();

        // Commands with & (backgrounded processes) may return non-zero even on success.
        // pkill returns 1 when no process matched. These shouldn't be marked as failures.
        let has_background = has_background_ampersand(&parsed.command);
        let has_pkill = parsed.command.contains("pkill");

        // Total elapsed wall-clock — appended to every result so the agent can
        // judge "slow but succeeded" vs "stalled/hung" without any per-tool
        // pattern matching. Purely numeric, tech-neutral.
        let elapsed_secs = start_instant.elapsed().as_secs_f64();

        match result {
            Ok(Some((success, code))) => {
                let mut combined = format_output(&stdout_str, &stderr_str);
                // For background/pkill commands: non-empty output = success
                let effective_success = success || has_background || (has_pkill && !combined.is_empty());

                if !effective_success {
                    // Even when stdout+stderr are empty, the agent needs to know
                    // the command actually failed and with what code. The old
                    // behavior dropped both pieces of info here, leaving the
                    // agent to retry the same command blindly. Now every failure
                    // carries exit code AND an explicit "nothing to read" note.
                    let suffix = if combined.is_empty() {
                        "[no stdout or stderr — use the exit code above to diagnose; \
                         common causes: missing file/path, permission denied, wrong shell, \
                         command not found]"
                    } else {
                        "\n\n[IMPORTANT: Command failed. Read the error above and fix the root cause. \
                         Do NOT retry the same command.]"
                    };
                    combined.push_str(suffix);
                }
                let elapsed_marker = format_exit_marker(elapsed_secs, code);
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
                let elapsed_marker = format!("[elapsed: {:.1}s, killed: idle]", elapsed_secs);
                let output = if combined.is_empty() {
                    format!(
                        "{} [no output for {}s — treat as stuck, don't retry the same command]",
                        elapsed_marker, SILENT_KILL_SECS
                    )
                } else {
                    format!(
                        "{}\n{}\n\n[no new output for {}s — output above is partial]",
                        elapsed_marker, combined, SILENT_KILL_SECS
                    )
                };
                Ok(ToolResult { call_id: String::new(), output, success: false })
            }
            Err(_) => {
                // Hard timeout — kill it
                let _ = child.kill().await;
                let combined = format_output(&stdout_str, &stderr_str);
                let elapsed_marker = format!("[elapsed: {:.1}s, killed: timeout]", elapsed_secs);
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

/// Format the header line that every bash result starts with. Carries two
/// tech-neutral numbers the agent needs to decide whether to retry, diagnose,
/// or move on: wall-clock elapsed, and process exit code. `code == None`
/// means the process was terminated by a signal (Unix) — surfaces as
/// `exit: signal` so the agent can tell this apart from a normal exit.
fn format_exit_marker(elapsed_secs: f64, code: Option<i32>) -> String {
    match code {
        Some(c) => format!("[elapsed: {:.1}s, exit: {}]", elapsed_secs, c),
        None => format!("[elapsed: {:.1}s, exit: signal]", elapsed_secs),
    }
}

/// Detect a "backgrounded command" by looking for a single `&` that isn't
/// part of the `&&` chain operator. Previously the check was
/// `command.contains(" &")`, which matched `&&` as a prefix because `" &&"`
/// contains the substring `" &"` — this caused every chained command
/// (`cd foo && cargo check`) to be treated as backgrounded, force-setting
/// `effective_success = true` regardless of the real exit code and breaking
/// downstream error detection (see hermes 2026-04-22_20-28-37 session where
/// cargo check exit 101 came back with `success=true`).
///
/// Bash treats `&` as async only when:
/// - followed by whitespace / end of input / `;` / `|` (but not `|&`)
/// - NOT when the next char is also `&` (that's logical AND)
fn has_background_ampersand(cmd: &str) -> bool {
    let bytes = cmd.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&' {
            let next = bytes.get(i + 1).copied();
            // `&&` is logical AND — skip both bytes, not a background marker.
            if next == Some(b'&') {
                i += 2;
                continue;
            }
            // Accept `&` followed by whitespace, end-of-string, `;`, or `|`
            // (but reject `&|` which isn't a valid shell token anyway).
            let prev_ok = i == 0 || matches!(bytes[i - 1], b' ' | b'\t' | b')' | b'\'' | b'"');
            let next_ok = matches!(
                next,
                None | Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b';') | Some(b'|')
            );
            if prev_ok && next_ok {
                return true;
            }
        }
        i += 1;
    }
    false
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

    // Previously this function also pattern-matched `sed -i` / `perl -pi` /
    // `awk -i inplace` as "in-place edit bypass" and required approval.
    // Removed 2026-04-22 in favor of effect-based detection (see
    // `snapshot_workspace_changes` + the post-exec diff in `BashTool::execute`):
    // pattern lists miss `sed --in-place`, `ed`, `ex`, custom Python edit
    // scripts, shell redirects `cmd > file`, etc.; snapshot-based detection
    // catches ANY workspace modification via bash regardless of how it was
    // spelled, using the user's own `.gitignore` as the neutrality boundary.

    None
}


/// Detect if a bash command is (or starts with) a `cd` and extract the target
/// directory.  Handles: `cd /path`, `cd ~/path`, `cd dir && ...`, `cd dir; ...`.
/// Returns None for non-cd commands or bare `cd` (go home).
fn detect_cd_target(cmd: &str) -> Option<String> {
    let trimmed = cmd.trim();
    if !trimmed.starts_with("cd ") && trimmed != "cd" {
        return None;
    }
    if trimmed == "cd" {
        // bare `cd` goes to $HOME
        return dirs::home_dir().map(|h| h.to_string_lossy().to_string());
    }
    // Extract the path after `cd `, stopping at `&&`, `;`, `||`, `|`, or end.
    let after_cd = trimmed[3..].trim_start();
    let end = after_cd.find(|c: char| c == '&' || c == ';' || c == '|')
        .unwrap_or(after_cd.len());
    let path = after_cd[..end].trim().trim_matches('"').trim_matches('\'');
    if path.is_empty() {
        return dirs::home_dir().map(|h| h.to_string_lossy().to_string());
    }
    Some(path.to_string())
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
mod exit_code_tests {
    use super::*;
    use crate::tool::ToolContext;
    use tempfile::TempDir;

    fn ctx() -> (TempDir, ToolContext) {
        let dir = TempDir::new().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf());
        (dir, ctx)
    }

    #[tokio::test]
    async fn success_marker_includes_exit_zero() {
        let (_d, ctx) = ctx();
        let r = BashTool.execute(r#"{"command":"true"}"#, &ctx).await.unwrap();
        assert!(r.success);
        assert!(r.output.contains("exit: 0"), "output was: {}", r.output);
    }

    #[tokio::test]
    async fn failure_marker_includes_specific_exit_code() {
        let (_d, ctx) = ctx();
        let r = BashTool.execute(r#"{"command":"exit 7"}"#, &ctx).await.unwrap();
        assert!(!r.success);
        assert!(r.output.contains("exit: 7"),
            "failure with code 7 must be visible, got: {}", r.output);
    }

    /// The core bug we're fixing: previously a failed command with no
    /// stdout/stderr left the agent staring at `[elapsed: 0.0s]` with no
    /// clue about what went wrong. Now every failure surfaces the exit
    /// code AND a recovery hint, even when the process wrote nothing.
    #[tokio::test]
    async fn empty_output_failure_has_diagnostic_hint() {
        let (_d, ctx) = ctx();
        let r = BashTool.execute(r#"{"command":"exit 3"}"#, &ctx).await.unwrap();
        assert!(!r.success);
        assert!(r.output.contains("exit: 3"), "exit code missing: {}", r.output);
        assert!(r.output.contains("no stdout or stderr"),
            "empty-output hint missing: {}", r.output);
    }

    #[tokio::test]
    async fn stderr_survives_with_exit_code() {
        let (_d, ctx) = ctx();
        let r = BashTool
            .execute(r#"{"command":"echo boom >&2; exit 2"}"#, &ctx)
            .await
            .unwrap();
        assert!(!r.success);
        assert!(r.output.contains("boom"), "stderr dropped: {}", r.output);
        assert!(r.output.contains("exit: 2"), "exit code missing: {}", r.output);
        assert!(r.output.contains("IMPORTANT"), "failure nudge missing: {}", r.output);
    }

    // --- Effect-based workspace-change detection (2026-04-22, P0 #2 option C) ---
    //
    // Replaced pattern-list hardcode (`sed -i` / `perl -pi` / `awk -i inplace`)
    // with effect-based detection using `git status --porcelain` snapshots.
    // Catches ANY bypass of edit_file (shell redirects, custom scripts, new
    // tools) without maintaining a list of names; uses the project's own
    // .gitignore as the neutrality boundary.

    async fn git_ctx() -> (TempDir, ToolContext) {
        let dir = TempDir::new().unwrap();
        // Initialize a real git repo so `git status` works inside the test.
        let status = tokio::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(dir.path())
            .status()
            .await
            .expect("git init");
        assert!(status.success(), "git init failed");
        let ctx = ToolContext::new(dir.path().to_path_buf());
        (dir, ctx)
    }

    #[tokio::test]
    async fn bash_shell_redirect_triggers_workspace_note() {
        // `echo ... > file` is a pure shell redirect — no tool name to match.
        // Old pattern list wouldn't catch it; effect-based detection does.
        let (_d, ctx) = git_ctx().await;
        let r = BashTool
            .execute(r#"{"command":"echo hello > src_new.rs"}"#, &ctx)
            .await
            .unwrap();
        assert!(
            r.output.contains("workspace modified via bash"),
            "missing workspace note: {}",
            r.output
        );
        assert!(r.output.contains("src_new.rs"), "filename must be listed: {}", r.output);
        assert!(r.output.contains("edit_file"), "nudge must point at edit_file: {}", r.output);
    }

    #[tokio::test]
    async fn bash_readonly_command_no_workspace_note() {
        // `ls` doesn't modify anything — no nudge.
        let (dir, ctx) = git_ctx().await;
        std::fs::write(dir.path().join("existing.txt"), "hi").unwrap();
        let r = BashTool.execute(r#"{"command":"ls"}"#, &ctx).await.unwrap();
        assert!(
            !r.output.contains("workspace modified via bash"),
            "read-only command must not trigger nudge: {}",
            r.output
        );
    }

    #[tokio::test]
    async fn bash_sed_in_place_detected_via_effect() {
        // The sed -i case old pattern-hardcode targeted — still caught, but
        // now via effect, not via parsing the command for the literal "sed -i".
        let (dir, ctx) = git_ctx().await;
        let path = dir.path().join("app.vue");
        std::fs::write(&path, "class=\"active\"\n").unwrap();
        // Commit so the file is tracked; then sed modifies it.
        tokio::process::Command::new("git")
            .args(["-c", "user.email=t@t", "-c", "user.name=t", "add", "."])
            .current_dir(dir.path())
            .status()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args([
                "-c", "user.email=t@t", "-c", "user.name=t",
                "commit", "--quiet", "-m", "init",
            ])
            .current_dir(dir.path())
            .status()
            .await
            .unwrap();
        let cmd = format!(
            r#"{{"command":"sed -i '' 's/active/is-active/' {}"}}"#,
            path.display()
        );
        let r = BashTool.execute(&cmd, &ctx).await.unwrap();
        assert!(
            r.output.contains("workspace modified via bash"),
            "sed -i effect must be flagged: {}",
            r.output
        );
    }

    #[tokio::test]
    async fn bash_non_git_directory_silently_skips() {
        // Outside a git repo, `git status` errors — detection must not spam
        // errors or attach spurious notes. Silent no-op is the contract.
        let dir = TempDir::new().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let r = BashTool
            .execute(r#"{"command":"echo hello > marker.txt"}"#, &ctx)
            .await
            .unwrap();
        assert!(
            !r.output.contains("workspace modified via bash"),
            "non-git dir must skip detection: {}",
            r.output
        );
    }

    #[tokio::test]
    async fn bash_gitignored_write_is_ignored() {
        // Writes into paths ignored by the repo's own .gitignore (build
        // artifacts, caches) must NOT trigger the nudge — it's the user's
        // gitignore, not a framework list, that defines "workspace".
        let (dir, ctx) = git_ctx().await;
        std::fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        tokio::process::Command::new("git")
            .args(["-c", "user.email=t@t", "-c", "user.name=t", "add", "."])
            .current_dir(dir.path())
            .status()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args([
                "-c", "user.email=t@t", "-c", "user.name=t",
                "commit", "--quiet", "-m", "ignore",
            ])
            .current_dir(dir.path())
            .status()
            .await
            .unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        let r = BashTool
            .execute(r#"{"command":"echo built > target/out.o"}"#, &ctx)
            .await
            .unwrap();
        assert!(
            !r.output.contains("workspace modified via bash"),
            "gitignored path must not trigger nudge: {}",
            r.output
        );
    }

    // --- Auto-STOP on resolved error (P0 #5, 2026-04-22) ---
    //
    // Session-scoped signature tracking: first failed bash records a
    // "signature" (first substantive output line). Subsequent successes that
    // don't contain the signature get a nudge suggesting to summarize + stop.
    // Tech-neutral — no keyword matching on "error/failed/panic", just "what
    // line of output was the first thing the model saw go wrong".

    #[tokio::test]
    async fn resolved_error_nudge_fires_after_fix() {
        let (_d, ctx) = ctx();
        // Turn 1: bash fails with a distinctive line.
        let r1 = BashTool
            .execute(r#"{"command":"echo distinctive_compile_error_xyz >&2; exit 1"}"#, &ctx)
            .await
            .unwrap();
        assert!(!r1.success);
        assert!(r1.output.contains("distinctive_compile_error_xyz"));
        // No "earlier error" hint on the FAILURE itself — it's the current
        // error, not a resolved one.
        assert!(!r1.output.contains("key diagnostic lines"), "own failure must not self-nudge: {}", r1.output);

        // Turn 2: bash succeeds with unrelated output — signature not present
        // → nudge should fire. New wording after 21-06 hermes session is
        // informational only (no "stop" directive, no quoted line) so the
        // weak-model doesn't skip remaining user-requested steps.
        let r2 = BashTool.execute(r#"{"command":"echo all good"}"#, &ctx).await.unwrap();
        assert!(r2.success);
        assert!(
            r2.output.contains("key diagnostic lines"),
            "resolved nudge must fire when sig no longer appears: {}",
            r2.output
        );
        // Nudge must no longer command "stop" directly — that caused the
        // model to skip user-requested follow-up steps.
        assert!(!r2.output.contains("summarize and stop"), "nudge must not command stop: {}", r2.output);
        assert!(r2.output.contains("remaining steps"));
    }

    #[tokio::test]
    async fn resolved_nudge_suppressed_when_sig_still_present() {
        let (_d, ctx) = ctx();
        let _ = BashTool
            .execute(r#"{"command":"echo compile_error_KEEP_ME >&2; exit 1"}"#, &ctx)
            .await
            .unwrap();

        // Later success that STILL echoes the error string (e.g. build ran
        // but same error recurred from a different path). Must NOT nudge.
        let r = BashTool
            .execute(r#"{"command":"echo 'still seeing: compile_error_KEEP_ME'"}"#, &ctx)
            .await
            .unwrap();
        assert!(r.success, "command succeeded: {}", r.output);
        assert!(
            !r.output.contains("key diagnostic lines"),
            "nudge must not fire while sig still appears: {}",
            r.output
        );
    }

    #[tokio::test]
    async fn no_nudge_without_prior_failure() {
        let (_d, ctx) = ctx();
        // Clean session — nudge must never fire when nothing failed yet.
        let r = BashTool.execute(r#"{"command":"echo hello"}"#, &ctx).await.unwrap();
        assert!(r.success);
        assert!(!r.output.contains("key diagnostic lines"));
    }

    #[tokio::test]
    async fn signature_ignores_framework_markers() {
        // extract_error_signatures must skip `[elapsed:…]` / `[cwd:…]` lines
        // so signatures are actual diagnostic content, not our own markers.
        let fake = "[elapsed: 1.2s, exit: 1]\n[cwd: /tmp]\nfatal: something very specific went wrong here and this is a very long diagnostic line";
        let sigs = super::super::extract_error_signatures(fake);
        assert!(!sigs.is_empty());
        assert!(sigs[0].contains("fatal"), "longest must be picked: {:?}", sigs);
        assert!(!sigs.iter().any(|s| s.contains("elapsed")));
        assert!(!sigs.iter().any(|s| s.contains("cwd")));
    }

    #[tokio::test]
    async fn signature_ranks_by_length_not_order() {
        // Cargo-style output where ambient status appears first but the
        // longer real diagnostic comes later. Length-sort must push the
        // long line to position 0 so at least one distinctive sig survives
        // the top-5 cutoff.
        let cargo_like = "\
[elapsed: 1.7s, exit: 101]
Blocking waiting for file lock on build directory
    Checking hermes-tauri v0.1.0 (/workspace/hermes-tauri/src-tauri)
error[E0425]: cannot find function `undefined_marker_abc123` in this scope and it spans here
error: could not compile `hermes-tauri` (bin \"hermes-tauri\") due to 1 previous error";
        let sigs = super::super::extract_error_signatures(cargo_like);
        assert!(sigs.len() >= 3);
        // Top signature must be a real diagnostic line, not the ambient
        // 50-char "Blocking waiting" status.
        assert!(
            sigs[0].len() > 60,
            "longest sig should be ≥60 chars, got len={}: {}",
            sigs[0].len(),
            sigs[0]
        );
        assert!(
            sigs.iter().any(|s| s.contains("undefined_marker_abc123")),
            "the specific error marker must be captured: {:?}",
            sigs,
        );
    }

    // --- has_background_ampersand (2026-04-22) ---
    //
    // Pre-fix `has_background = command.contains(" &")` matched `" &&"` as
    // a substring, so every chained command (`cd X && cargo check`) got
    // marked as backgrounded, which in turn forced `effective_success =
    // true` even when the child process exited non-zero. Downstream: all
    // failed chained cargo / npm / pytest commands reported success=true
    // to the agent, the Auto-STOP sig-capture never ran, and loop
    // detection missed real failures. Rebuilt as a bytewise parser to
    // distinguish single `&` (real background) from `&&` (shell AND).

    #[test]
    fn ampersand_and_is_not_background() {
        assert!(!has_background_ampersand("cd foo && cargo check"));
        assert!(!has_background_ampersand("a && b && c"));
    }

    #[test]
    fn bare_trailing_ampersand_is_background() {
        assert!(has_background_ampersand("sleep 10 &"));
        assert!(has_background_ampersand("npm run dev &"));
    }

    #[test]
    fn ampersand_before_chain_operator_is_background() {
        // `cmd & ; other` is rare but bash-legal.
        assert!(has_background_ampersand("job & ; wait"));
        assert!(has_background_ampersand("job & | tee log"));
    }

    #[test]
    fn no_ampersand_is_not_background() {
        assert!(!has_background_ampersand("echo hi"));
        assert!(!has_background_ampersand("grep pattern file"));
    }

    /// Regression: chained command with failing tail must surface the real
    /// failure (`success=false`) so Auto-STOP sig capture fires. Before
    /// the fix, `&&` was mistaken for background → success=true → sig
    /// never captured → nudge never fires downstream.
    #[tokio::test]
    async fn chained_command_failure_reports_failure_not_background() {
        let (_d, ctx) = ctx();
        let r = BashTool
            .execute(r#"{"command":"true && exit 42"}"#, &ctx)
            .await
            .unwrap();
        assert!(!r.success, "chained tail exit 42 must report failure, got: {}", r.output);
        assert!(r.output.contains("exit: 42"));
    }

    /// Regression for the hermes 2026-04-22_20-12-22 miss: single-line sig
    /// locked onto "Blocking waiting for file lock" which appears in BOTH
    /// fail and success. New multi-sig + majority-absent rule must fire on
    /// this exact case.
    #[tokio::test]
    async fn resolved_nudge_fires_on_real_cargo_failure_then_success() {
        let (_d, ctx) = ctx();
        let failing = r#"{"command":"echo 'Blocking waiting for file lock on build directory'; echo '    Checking demo v0.1.0 (/path/foo)'; echo 'error[E0425]: cannot find function `xyz_specific` in this scope'; echo 'error: could not compile `demo` (bin \"demo\") due to 1 previous error' >&2; exit 101"}"#;
        let r1 = BashTool.execute(failing, &ctx).await.unwrap();
        assert!(!r1.success, "test setup: first run must fail");

        // Success rerun with only ambient status — the distinctive error
        // lines are gone.
        let passing = r#"{"command":"echo 'Blocking waiting for file lock on build directory'; echo '    Checking demo v0.1.0 (/path/foo)'; echo '    Finished `dev` profile in 0.5s'"}"#;
        let r2 = BashTool.execute(passing, &ctx).await.unwrap();
        assert!(r2.success);
        assert!(
            r2.output.contains("key diagnostic lines"),
            "majority-absent rule must fire: {}",
            r2.output
        );
    }

    #[tokio::test]
    async fn grep_no_match_is_visible_exit_1() {
        // The canonical "silent failure" that tripped 426-atom's Turn 8:
        // grep exits 1 when no line matches, no stdout, no stderr. Before
        // the fix this looked identical to a hard failure — now exit:1
        // tells the agent "no match" vs exit:2 "bad regex / missing file".
        let (_d, ctx) = ctx();
        let r = BashTool
            .execute(r#"{"command":"echo hello | grep xyz"}"#, &ctx)
            .await
            .unwrap();
        assert!(!r.success);
        assert!(r.output.contains("exit: 1"), "grep no-match must show exit:1, got: {}", r.output);
    }
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
