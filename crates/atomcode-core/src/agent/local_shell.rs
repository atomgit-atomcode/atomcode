//! Formatting for user-invoked `!` shell mode: turn a `ShellOutcome` into
//! (a) conversation context the model sees, and (b) display text for the TUI.

use crate::tool::bash::{ShellExit, ShellOutcome};

/// Build the User-message text injected into the conversation. The model
/// reads this on its next turn. XML tags mirror Claude Code's bash mode.
pub(crate) fn format_bash_context(cmd: &str, outcome: &ShellOutcome) -> String {
    let mut s = format!("<bash-input>{}</bash-input>", cmd);
    let stdout = outcome.stdout.trim();
    if !stdout.is_empty() {
        s.push_str(&format!("\n<bash-stdout>{}</bash-stdout>", stdout));
    }
    let stderr = outcome.stderr.trim();
    if !stderr.is_empty() {
        s.push_str(&format!("\n<bash-stderr>{}</bash-stderr>", stderr));
    }
    match outcome.exit {
        ShellExit::Exited { code: Some(c), .. } if c != 0 => {
            s.push_str(&format!("\n<bash-exit-code>{}</bash-exit-code>", c));
        }
        ShellExit::KilledTimeout => {
            s.push_str("\n<bash-stderr>command timed out</bash-stderr>");
        }
        ShellExit::KilledIdle => {
            s.push_str("\n<bash-stderr>command killed (no progress)</bash-stderr>");
        }
        _ => {}
    }
    s
}

/// Build the text shown in the TUI tool-result block (ToolCallResult.output).
pub(crate) fn format_bash_display(outcome: &ShellOutcome) -> String {
    let stdout = outcome.stdout.trim();
    let stderr = outcome.stderr.trim();
    match (stdout.is_empty(), stderr.is_empty()) {
        (false, true) => stdout.to_string(),
        (true, false) => format!("STDERR:\n{}", stderr),
        (false, false) => format!("{}\nSTDERR:\n{}", stdout, stderr),
        (true, true) => match outcome.exit {
            ShellExit::Exited { code: Some(c), .. } => format!("(no output, exit {})", c),
            ShellExit::KilledTimeout => "(timed out, no output)".to_string(),
            ShellExit::KilledIdle => "(killed, no output)".to_string(),
            ShellExit::Exited { code: None, .. } => "(no output)".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::bash::{ShellExit, ShellOutcome};

    fn outcome(stdout: &str, stderr: &str, exit: ShellExit) -> ShellOutcome {
        ShellOutcome {
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            exit,
            elapsed_secs: 0.0,
        }
    }

    #[test]
    fn context_includes_input_and_stdout() {
        let o = outcome("hi\n", "", ShellExit::Exited { success: true, code: Some(0) });
        let s = format_bash_context("echo hi", &o);
        assert!(s.contains("<bash-input>echo hi</bash-input>"));
        assert!(s.contains("<bash-stdout>hi</bash-stdout>"));
        assert!(!s.contains("bash-exit-code"), "clean run must omit exit code: {s}");
    }

    #[test]
    fn context_includes_exit_code_on_failure() {
        let o = outcome("", "boom", ShellExit::Exited { success: false, code: Some(2) });
        let s = format_bash_context("false", &o);
        assert!(s.contains("<bash-stderr>boom</bash-stderr>"));
        assert!(s.contains("<bash-exit-code>2</bash-exit-code>"));
    }

    #[test]
    fn context_notes_timeout() {
        let o = outcome("partial", "", ShellExit::KilledTimeout);
        let s = format_bash_context("sleep 999", &o);
        assert!(s.contains("timed out"), "got: {s}");
    }

    #[test]
    fn display_joins_stdout_and_stderr() {
        let o = outcome("out", "err", ShellExit::Exited { success: false, code: Some(1) });
        let d = format_bash_display(&o);
        assert!(d.contains("out"));
        assert!(d.contains("STDERR:\n"));
        assert!(d.contains("err"));
    }
}
