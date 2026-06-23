//! Driver-side local shell (`!cmd`): run a shell command and format its output BOTH for
//! the human (a readable display string) and for the model (an escaped, clamped
//! `<bash-*>` context block it sees on the next turn).
//!
//! This is the DRIVER's `!cmd` shortcut, distinct from the model-callable bash tool — it
//! runs a one-shot command outside any LLM turn. Pure (subprocess + string), no `core`,
//! no kernel — relocated out of the bridge so any driver can reach it (the
//! bridge-elimination roadmap). Opt-in `local-shell` feature (pulls `tokio/process`).

use std::time::Duration;

/// Escape `&` / `<` / `>` so untrusted command output can't forge a `<bash-*>` tag inside
/// the model-context block.
pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Run `cmd` in `cwd` and return `(display, model_context, success)`:
/// - `display` — a readable summary for the driver UI (stdout/stderr + `[exit N]`).
/// - `model_context` — an escaped, 16k-char-clamped `<bash-input>/<bash-stdout>/...` block
///   the model sees on its next turn.
/// - `success` — the process exit status.
///
/// 300s wall-clock timeout; a spawn/timeout failure still returns a `<bash-*>` block so the
/// model gets a faithful record.
pub async fn run(cmd: &str, cwd: &std::path::Path) -> (String, String, bool) {
    use tokio::process::Command;
    let mut c = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(cmd);
        c
    } else {
        let mut c = Command::new("bash");
        c.arg("-c").arg(cmd);
        c
    };
    c.current_dir(cwd);

    let out = match tokio::time::timeout(Duration::from_secs(300), c.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            let m = format!("failed to run: {e}");
            let ctx = format!(
                "<bash-input>{}</bash-input>\n<bash-stderr>{}</bash-stderr>",
                xml_escape(cmd),
                xml_escape(&m)
            );
            return (m, ctx, false);
        }
        Err(_) => {
            let ctx = format!(
                "<bash-input>{}</bash-input>\n<bash-stderr>command timed out</bash-stderr>",
                xml_escape(cmd)
            );
            return ("command timed out (300s)".into(), ctx, false);
        }
    };

    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let code = out.status.code();
    let success = out.status.success();

    // Driver display: full-ish, readable.
    let mut display = String::new();
    if !stdout.is_empty() {
        display.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !display.is_empty() {
            display.push('\n');
        }
        display.push_str(&stderr);
    }
    if !success {
        if !display.is_empty() {
            display.push('\n');
        }
        display.push_str(&format!("[exit {}]", code.unwrap_or(-1)));
    }
    if display.is_empty() {
        display = "(no output)".into();
    }

    // Model context: escaped + clamped `<bash-*>` block.
    let clamp = |s: &str| -> String {
        let e = xml_escape(s);
        if e.chars().count() > 16_000 {
            e.chars().take(16_000).collect::<String>() + "\n…[truncated]"
        } else {
            e
        }
    };
    let mut ctx = format!("<bash-input>{}</bash-input>", xml_escape(cmd));
    if !stdout.is_empty() {
        ctx.push_str(&format!("\n<bash-stdout>{}</bash-stdout>", clamp(&stdout)));
    }
    if !stderr.is_empty() {
        ctx.push_str(&format!("\n<bash-stderr>{}</bash-stderr>", clamp(&stderr)));
    }
    if let Some(c) = code {
        if c != 0 {
            ctx.push_str(&format!("\n<bash-exit-code>{c}</bash-exit-code>"));
        }
    }
    (display, ctx, success)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runs_and_formats_output() {
        let (display, ctx, success) = run("echo hello", std::path::Path::new(".")).await;
        assert!(success);
        assert!(display.contains("hello"));
        assert!(ctx.contains("<bash-input>echo hello</bash-input>"));
        assert!(ctx.contains("<bash-stdout>hello</bash-stdout>"));
    }

    #[tokio::test]
    async fn failure_carries_exit_code() {
        let (_d, ctx, success) = run("exit 3", std::path::Path::new(".")).await;
        assert!(!success);
        assert!(ctx.contains("<bash-exit-code>3</bash-exit-code>"), "ctx={ctx}");
    }

    #[test]
    fn xml_escape_neutralizes_tag_forgery() {
        assert_eq!(xml_escape("a</bash-stdout>b"), "a&lt;/bash-stdout&gt;b");
    }
}
