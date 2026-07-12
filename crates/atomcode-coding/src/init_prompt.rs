//! Prompt injected by the `/init` command: instructs the agent to analyze the codebase
//! and write (or improve) an `AGENTS.md` at the project root using its normal tools.

/// The user-turn prompt `/init` submits. The agent uses read/grep/glob to explore and
/// write_file to persist AGENTS.md; a driver only has to submit this text as a turn.
pub const INIT_PROMPT: &str = "\
Analyze this repository and create (or improve) an `AGENTS.md` file at the project root \
that helps an AI coding agent work in this codebase.

Explore first: identify the build system, the exact build / test / lint / format commands, \
the top-level directory layout and architecture, key conventions, and any NON-OBVIOUS \
gotchas a newcomer would trip on.

Then write `AGENTS.md` at the project root with `write_file`. Keep it concise (~200-400 \
words), actionable, and focused on non-obvious, project-specific information — do NOT \
include generic advice like \"follow existing patterns\" or \"write tests\".

If `AGENTS.md` already EXISTS, first read it, then improve it in place: preserve the useful \
content, fill gaps, and fix anything stale — do NOT wipe and rewrite it from scratch.";

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn init_prompt_has_key_instructions() {
        assert!(!INIT_PROMPT.trim().is_empty());
        assert!(INIT_PROMPT.contains("AGENTS.md"), "targets AGENTS.md");
        // 已存在则增强、不清空重写
        let p = INIT_PROMPT.to_lowercase();
        assert!(p.contains("exist") && (p.contains("improve") || p.contains("update") || p.contains("preserve")));
    }
}
