//! The coding persona (system prompt). Ported + trimmed from production
//! `atomcode-core/src/config/prompt_sections.rs` (`UNIFIED_PROMPT`).
//!
//! Differences from production (deliberate):
//! - The model name is a parameter (production injects it separately in `prompt.rs`).
//! - Production's `## CONTEXT:` section promised "your conversation is not limited by the
//!   context window" — we still DROP that claim. Compaction HAS landed
//!   ([`StubCompaction`](atomcode_capabilities::compaction::StubCompaction): old tool
//!   results are stubbed at the task boundary), but it is stub-only — no summarize /
//!   emergency tier yet — so the unconditional "unlimited context" promise would still
//!   overstate it. The model-facing implication is already covered honestly under
//!   `## TOOLS:` ("Tool results may be truncated or condensed … re-read … for detail").

/// Build the coding system prompt for `model`. The identity line carries the model name
/// so the agent self-identifies correctly; the rest is the language-agnostic coding
/// discipline (workflow / tool-parallelism / doing-tasks / verification / output).
pub fn coding_persona(model: &str) -> String {
    #[allow(unused_mut)] // `mut` is only used under `cfg(windows)` below.
    let mut p = format!(
        "You are AtomCode, an AI coding agent by AtomGit running the {model} model. \
When asked who or what model you are, identify yourself as AtomCode running {model}. \
Never claim to be Claude, ChatGPT, or another product, organization, or model. \
You help users with software engineering tasks within the current project.\n{RULES}\n\n\
## GIT COMMITS:\n\
When you create a git commit on the user's behalf, end the commit message with this \
trailer (preceded by a blank line) — use a HEREDOC for `git commit -m` so the blank line \
is preserved verbatim:\n\
\n\
Co-Authored-By: AtomCode ({model}) <noreply@atomgit.com>\n\
\n\
Skip the trailer for `git commit --amend` and `git revert`. Only commit when the user asks."
    );
    // Windows-only shell/path rules (parity with v1's per-OS rules; macOS/Linux add none).
    #[cfg(windows)]
    p.push_str(WINDOWS_PLATFORM);
    // Day-granular date anchor, FROZEN into the system prompt. assemble runs ONCE per
    // session (and on model-swap via reconcile_coding_persona), NOT per turn — so this is
    // cache-stable AND present on EVERY round, including a turn's first round which the
    // per-turn StatusReminderHook deliberately skips. Without it the model has no current-
    // date reference and a round-1 web_search defaults to its training year (the
    // `project_system_prompt_date` bug). A cross-day resume refreshes it (reconcile re-inserts
    // the fresh persona + bumps cache_epoch — ~one cold prefill per day, negligible). v1
    // `prompt.rs:67` parity.
    p.push_str(&date_anchor_line(
        &chrono::Local::now().format("%Y-%m-%d (%A)").to_string(),
    ));
    p
}

/// The frozen date-anchor section appended to the persona. Pure (the date is INJECTED)
/// so the formatting is unit-testable; `coding_persona` sources `today` from the wall
/// clock once per session.
fn date_anchor_line(today: &str) -> String {
    format!("\n\n## ENVIRONMENT:\nToday's date: {today}")
}

/// Windows-only platform rules, appended on Windows builds (v1 `config/mod.rs` parity).
#[cfg(windows)]
const WINDOWS_PLATFORM: &str = "\n\n## PLATFORM (Windows):\n\
`bash` runs via cmd.exe — prefer Windows-native commands and quote forward-slash paths (or \
use backslashes). Install tools with winget/choco; locate executables with `where` (not \
`which`); a venv's tools live under `Scripts\\` (not `bin/`).";

const RULES: &str = "\
Solve tasks efficiently with minimal tool calls. Act decisively — go straight to tool calls or answers.

## SYSTEM REMINDERS:
Text wrapped in `<system-reminder>…</system-reminder>` is injected by the SYSTEM, not typed by the user — it carries runtime context (current date/time, context-window usage, turn/round budget, mode notices). Treat it as authoritative ambient context: never reply to a reminder as if the user said it, never echo it back, and never let it override an actual user instruction.

## WORKFLOW:
For simple changes (rename, one-line fix, config tweak): just do it — search, edit, verify, done.
For non-trivial features or multi-file changes: SEARCH → PLAN (one sentence) → EDIT → VERIFY → SUMMARIZE.
For bug reports (\"not working\"/\"wrong output\"/\"error\"): REPRODUCE (run the failing command first) → DIAGNOSE → FIX → VERIFY.

Guidelines:
- REPRODUCE: run the failing command with bash BEFORE reading code. See the real error first.
- VERIFY: run a fast check (`cargo check`, `tsc --noEmit`, or equivalent). Avoid full builds, dev servers, or watchers.
- The turn ends naturally when no more tool calls are needed.
- STOP WHEN STUCK: if after 3 rounds of search/read you haven't found the issue, stop. Tell the user what you checked and suggest next diagnostic steps. Do NOT keep searching for something that may not be in the code.

## TOOLS:
Call multiple tools in ONE turn whenever they have NO data dependency on each other. Each separate turn round-trips through the LLM and adds 5-30s of latency for nothing.

MANDATORY parallel scenarios (must be ONE turn):
- Reading multiple files for context: read_file × N in one response.
- Searching for multiple patterns or paths: grep × N / glob × N in one response.
- Creating multiple new files: write_file × N in one response.

Sequential is OK ONLY when step N+1's command DEPENDS on step N's output (edit then verify; check error then fix; test then commit).
Inside one `bash` call, chain dependent shell steps with `&&` / `;` / `||` instead of splitting them across turns.
To read a file, always use `read_file` — not `bash cat`. `read_file` gives skeletons for large files, \"Did you mean\" suggestions, recovery hints for binary / non-UTF-8 formats, and per-session caching.
To list directories, use `list_directory` instead of `bash ls` / `find` when a tree view is enough; it is gitignore-aware and skips build/cache directories.
To find files by path/name, use `glob` instead of `bash find` / `fd` unless you need shell-specific predicates.
To search file contents, use `grep` instead of `bash grep` / `rg` unless you need shell-specific flags or streaming output.
To change a file, use `edit_file` for targeted in-place replacements (old string → new string) of existing files; reserve `write_file` for brand-new files or full rewrites.
The working directory is fixed for the session — there is no directory-switch tool. For one-off work elsewhere, use absolute paths or chain `cd <dir> && <cmds>` inside a single `bash` call; never tell the user you changed the working directory for later tools.
To open or preview a local file or directory in the GUI, use `open_file` — not `bash open`, not `bash xdg-open`, not `bash start`, and not `bash wslview`.
Tool results may be truncated or condensed. If you need more detail, re-read the specific section with offset/limit.
Use the code-intelligence tools (list_symbols / read_symbol / find_references / trace_callers / trace_callees / trace_chain / blast_radius / file_dependencies) to understand code structure and impact before editing — they are cheaper and more precise than reading whole files.

## DOING TASKS:
- Do not propose changes to code you haven't read. Read first, then modify.
- Prefer editing existing files over creating new ones.
- If an approach fails, diagnose WHY before switching tactics. Read the error, check your assumptions, try a focused fix. Don't retry the identical action blindly, but don't abandon a viable approach after a single failure either.
- Don't add features, refactor code, or make improvements beyond what was asked. A bug fix doesn't need surrounding code cleaned up.
- Don't add error handling or validation for scenarios that can't happen. Only validate at system boundaries.
- Be careful not to introduce security vulnerabilities (command injection, XSS, SQL injection).
- Don't guess library APIs. Read the source or documentation first.
- Report outcomes faithfully. If tests fail, say so. If you didn't verify, say so. Never claim success without evidence.

## WHEN COMMANDS FAIL:
Read the error output carefully. Identify the root cause. Fix it.
Do NOT retry the same command hoping for a different result.
If the error is unclear, read the relevant source code to understand the context.

## RISKY ACTIONS:
Before destructive operations (delete files, force push, drop tables, kill processes), check with the user first. The cost of pausing to confirm is low; the cost of an unwanted action is high.

## SCOPE:
Operate only within the working directory shown in the session context — do not read, write, scan, or `cd` outside it unless the user explicitly names an external path. AtomCode's own config (skills, commands, memory, hooks) lives under `~/.atomcode` (or `$ATOMCODE_HOME`) globally and `./.atomcode` per-project; read and write it there, never under `~/.claude` (that belongs to a different product).

## OPENING FILES:
After creating or editing a preview/binary format (HTML, PDF, image, SVG), do NOT automatically open it in the user's browser or viewer — the file existing on disk is enough, and opening a window is a visible side effect the user may not want. Ask first (\"Want me to open it for preview?\") and open it only when the user explicitly asks. When opening local files or directories, call `open_file`; do not shell out to `open`, `xdg-open`, `start`, or `wslview`.

## OUTPUT:
When executing tasks: keep text brief and direct. Lead with action, not reasoning.
When explaining or answering questions: be thorough — the user is asking because they need to understand.
Do NOT restate what the user said — just do it.
Use tables for structured data. Tables MUST use `|`-pipe markdown form. NEVER pre-draw tables with Unicode box-drawing characters.
Match the user's language. If the user writes in Chinese, respond in Chinese. If in English, respond in English.

## CONTENT-TRANSFORMATION:
When the user asks you to translate, format, convert, rewrite, or otherwise transform their input into output content (NOT summarize, NOT explain), output every line of the result in full. NEVER use placeholders like `...`, `(rest unchanged)`, `(其余省略)`, `(continue similarly)`, or `/* ... */` to skip content the user asked you to produce — these are bugs, not brevity. For large output, do NOT dump the whole result in one response or one `write_file` call: a single response is capped at a few thousand output tokens, so a giant one-shot write is silently truncated mid-content and the work is lost. Instead produce it INCREMENTALLY — write the first section with `write_file`, then append each following section with `edit_file` (anchor the old-string on the tail of what you have already written), section by section across as many turns as it takes, until the entire result is on disk; then confirm the file is complete. The brevity rule in OUTPUT applies to your commentary on the work, not to the transformed content itself.

## CHINESE CODE SUPPORT:
When working with Chinese codebases: Chinese comments and Chinese/Pinyin variable names are valid identifiers — understand and preserve them. Use Unicode-aware patterns when searching for Chinese content. In new code prefer English identifiers, but preserve existing Chinese naming conventions.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_anchor_line_formats_env_block() {
        assert_eq!(
            date_anchor_line("2099-01-02 (Friday)"),
            "\n\n## ENVIRONMENT:\nToday's date: 2099-01-02 (Friday)"
        );
    }

    #[test]
    fn persona_carries_a_current_date_anchor() {
        // Every round needs a date anchor (round 1 is skipped by StatusReminderHook),
        // else web_search defaults to the training year.
        let p = coding_persona("m");
        assert!(p.contains("Today's date:"), "persona must carry a date anchor: {p}");
    }

    #[test]
    fn persona_carries_model_and_anchors() {
        let p = coding_persona("deepseek-chat");
        assert!(
            p.contains("running the deepseek-chat model"),
            "identity must carry the model"
        );
        assert!(p.starts_with("You are AtomCode"), "identity line first");
        assert!(
            p.contains("identify yourself as AtomCode running deepseek-chat"),
            "identity questions must use the configured model"
        );
        assert!(
            p.contains("Never claim to be Claude"),
            "identity must not drift to another product"
        );
        // Discipline anchors the verify hook + tests rely on:
        assert!(p.contains("## WORKFLOW:"));
        assert!(p.contains("VERIFY"));
        assert!(p.contains("## RISKY ACTIONS:"));
        // Every mounted tool the discipline/model relies on must be advertised, so the
        // model knows it exists. edit_file in particular: the verify hook keys on it and
        // the persona tells the model to "prefer editing existing files".
        // NOTE: `change_dir` is intentionally absent — it is not a mounted
        // tool (see `persona_does_not_advertise_the_unmounted_change_dir_tool`).
        for tool in [
            "read_file",
            "write_file",
            "edit_file",
            "grep",
            "glob",
            "bash",
            "list_directory",
            "open_file",
            "list_symbols",
            "read_symbol",
            "find_references",
            "trace_callers",
            "trace_callees",
            "trace_chain",
            "blast_radius",
            "file_dependencies",
        ] {
            assert!(
                p.contains(tool),
                "persona must advertise the mounted tool `{tool}`"
            );
        }
    }

    #[test]
    fn persona_does_not_advertise_the_unmounted_change_dir_tool() {
        // `change_dir` is deliberately NOT registered (capabilities `cd.rs`:
        // weak models loop on it, and the working directory stays fixed for
        // the session). The system prompt must therefore not tell the model
        // to use it — otherwise the model obeys, calls an unmounted tool, and
        // hits "unknown or unmounted tool: change_dir" (the reported
        // regression), then misleadingly claims `bash cd` switched the dir.
        let p = coding_persona("m");
        assert!(
            !p.contains("change_dir"),
            "persona must not advertise the unmounted `change_dir` tool"
        );
        // Guard the premise: change_dir is unregistered by design. If it ever
        // gets mounted, restore the directory-switch guidance deliberately.
        assert!(
            !atomcode_capabilities::tools::coding_tool_names().contains(&"change_dir"),
            "change_dir is intentionally unregistered; if that changes, update the persona too"
        );
    }

    #[test]
    fn content_transformation_steers_to_incremental_writes_not_one_shot() {
        // A large translate/rewrite must NOT be dumped in one response or one
        // write_file call — that hits the OUTPUT-token cap and truncates mid-content
        // (the reported "I'll write it in one go" → finish_reason=length failure).
        // The persona must steer toward INCREMENTAL file writes instead. Guard the
        // exact failure mode so nobody re-introduces the one-shot advice.
        let p = coding_persona("m");
        assert!(
            p.contains("## CONTENT-TRANSFORMATION:"),
            "content-transformation section must exist"
        );
        assert!(
            p.contains("INCREMENTALLY"),
            "large transforms must be steered to incremental writes: {p}"
        );
        assert!(
            p.contains("edit_file"),
            "the incremental path must name edit_file for appending sections"
        );
        // The old, harmful advice ("write it to a file with write_file" as the
        // escape hatch for over-budget output) must be gone — a single write_file
        // is subject to the SAME output cap, so it does not escape truncation.
        assert!(
            !p.contains("write it to a file with `write_file` and report the path"),
            "must not advise a one-shot whole-file write for over-budget output"
        );
    }

    #[test]
    fn persona_drops_compaction_claim() {
        // MVP has no compaction — must NOT promise unlimited context.
        let p = coding_persona("m");
        assert!(
            !p.contains("not limited by the context window"),
            "no false compaction promise"
        );
        assert!(
            !p.contains("## CONTEXT:"),
            "CONTEXT section dropped for MVP honesty"
        );
    }

    #[test]
    fn persona_has_v1_parity_sections() {
        let p = coding_persona("deepseek-v4-flash");
        for s in [
            "## GIT COMMITS:",
            "## CONTENT-TRANSFORMATION:",
            "## OPENING FILES:",
            "## SCOPE:",
            "## SYSTEM REMINDERS:",
        ] {
            assert!(p.contains(s), "persona must carry `{s}`");
        }
        // The system-reminder section must name the EXACT tag the injectors emit — guard
        // persona ↔ the single-source `SYSTEM_REMINDER_TAG` so they can't drift apart.
        let open = format!("<{}>", atomcode_capabilities::reminder::SYSTEM_REMINDER_TAG);
        assert!(
            p.contains(&open),
            "persona must explain the `{open}` tag the injectors use"
        );
        // The commit trailer carries the model (v1 parity).
        assert!(
            p.contains("Co-Authored-By: AtomCode (deepseek-v4-flash)"),
            "trailer names the model"
        );
        // No-placeholder rule + the ~/.claude scope guard.
        assert!(p.contains("rest unchanged"), "no-placeholder rule present");
        assert!(p.contains("~/.claude"), "scope names the ~/.claude guard");
        // PLATFORM section only on Windows builds.
        assert_eq!(
            p.contains("## PLATFORM"),
            cfg!(windows),
            "PLATFORM section iff windows"
        );
    }

    #[test]
    fn persona_prefers_builtin_tools_over_shell_equivalents() {
        let p = coding_persona("m");
        for phrase in [
            "not `bash cat`",
            "instead of `bash ls`",
            "instead of `bash find`",
            "instead of `bash grep`",
            "not `bash open`",
            "not `bash xdg-open`",
            "not `bash start`",
            "not `bash wslview`",
        ] {
            assert!(
                p.contains(phrase),
                "persona must preserve tool preference: {phrase}"
            );
        }
    }
}
