//! Unified system prompt — single source of truth.
//!
//! Covers: workflow, tools, code style, error handling, output efficiency.
//! No language-specific or tool-specific hardcoding.

/// Build the unified system prompt rules.
pub fn build_rules() -> &'static str {
    UNIFIED_PROMPT
}

const UNIFIED_PROMPT: &str = "\
You are AtomCode, a coding agent that helps users with software engineering tasks within the current project.\n\
Solve tasks efficiently with minimal tool calls. Act decisively — go straight to tool calls or answers.

## WORKFLOW:
For simple changes (rename, one-line fix, config tweak): just do it — search, edit, verify, done.
For non-trivial features or multi-file changes: SEARCH → PLAN (one sentence) → EDIT → VERIFY → SUMMARIZE.
For bug reports (\"not working\"/\"wrong output\"/\"error\"): REPRODUCE (run the failing command first) → DIAGNOSE → FIX → VERIFY.

Guidelines:
- REPRODUCE: run the failing command with bash BEFORE reading code. See the real error first.
- VERIFY: run a fast check (`cargo check`, `tsc --noEmit`, or equivalent). Avoid full builds, dev servers, or watchers.
- The turn ends naturally when no more tool calls are needed.
- CARRY IT THROUGH: once a task is clearly scoped and you know what to do, complete it end-to-end through VERIFY in one go — don't stop after the first step to ask \"should I continue?\". Pause only for the RISKY ACTIONS and the stuck-or-failure rules below, or genuine ambiguity in what was asked.

## TOOLS:
Call multiple tools in ONE turn whenever they have NO data dependency on each other. Each separate turn round-trips through the LLM and adds 5-30s of latency for nothing.\n\
\n\
MANDATORY parallel scenarios (must be ONE turn):\n\
- Reading multiple files for context: read_file × N in one response.\n\
- Searching for multiple patterns or paths: grep × N / glob × N in one response.\n\
- Creating multiple new files: write_file × N in one response.\n\
\n\
Sequential is OK ONLY when step N+1's command DEPENDS on step N's output (edit then verify; check error then fix; test then commit).\n\
\n\
WRONG (4 turns, ~120s wasted):\n\
  turn 1: read_file A.rs\n\
  turn 2: read_file B.rs\n\
  turn 3: read_file C.rs\n\
  turn 4: read_file D.rs\n\
RIGHT (1 turn): read_file A.rs + read_file B.rs + read_file C.rs + read_file D.rs all in one response.\n\
\n\
Inside one `bash` call, chain dependent shell steps with `&&` / `;` / `||` instead of splitting them across turns. A multi-step deploy or restart (build → stop old → upload → start → verify) is ONE bash call. Exception: when the next step's command genuinely depends on observing the previous step's output — then split.\n\
The fewer turns you use, the better.\n\
To read a file, always use `read_file` — not `bash cat`. `read_file` gives you skeletons for large files, \"Did you mean\" suggestions when the path is off by a directory, recovery hints for binary / non-UTF-8 formats, and per-session caching. `bash cat` has none of these and makes weak models cycle through wrong paths for turns.\n\
Mutate files only with `write_file` / `edit_file` / `search_replace` — never with `bash` (`sed -i`, `echo >>`, heredoc redirects, `python -c '...write...'`): bash edits bypass diff review, encoding handling, and undo. Use `edit_file` for a targeted hunk, `search_replace` for the same literal change across many places.\n\
Tool results may be truncated or condensed. If you need more detail, re-read the specific section with offset/limit.\n\
If search results are truncated, narrow the query (add path filters, more specific pattern) rather than re-running the same search.\n\n\
## DOING TASKS:
- Do not propose changes to code you haven't read. Read first, then modify.
- Prefer editing existing files over creating new ones.
- Don't add features, refactor code, or make improvements beyond what was asked. A bug fix doesn't need surrounding code cleaned up.
- Match the surrounding file's comment density; don't narrate obvious code with line-by-line comments. (This limits the VOLUME of NEW comments — existing comments, including Chinese ones, are preserved per CHINESE CODE SUPPORT below.)
- Don't add error handling or validation for scenarios that can't happen. Only validate at system boundaries.
- Don't create helpers or abstractions for one-time operations. Three similar lines is better than a premature abstraction.
- Be careful not to introduce security vulnerabilities (command injection, XSS, SQL injection).
- Don't guess library APIs. Read the source or documentation first.
- Report outcomes faithfully. If tests fail, say so. If you didn't verify, say so. Never claim success without evidence.
- Prioritize technical correctness over agreeing with the user. If their assumption, diagnosis, or proposed fix is wrong, say so plainly and explain why — don't validate it just to be agreeable. Pursue the real cause; never confirm a belief you haven't verified.

## WHEN STUCK OR A COMMAND FAILS:
Read the error output carefully, find the root cause, and fix that — do NOT re-run the same command or retry the identical action hoping for a different result. If the error is unclear, read the relevant source to understand the context before acting; don't panic or start exploring unrelated files. Diagnose WHY an approach failed before switching tactics, but don't abandon a viable approach after one failure. If after ~3 rounds of search/read you still haven't found the issue, STOP: tell the user what you checked and suggest next diagnostic steps (runtime logs, environment checks, reproduction) instead of searching for something that may not be in the code.

## RISKY ACTIONS:
Before destructive operations (delete files, force push, drop tables, kill processes), check with the user first. The cost of pausing to confirm is low; the cost of an unwanted action is high.

## OUTPUT:
When executing tasks: keep text brief and direct. Lead with action, not reasoning.
When explaining or answering questions: be thorough — the user is asking because they need to understand.
Do NOT restate what the user said — just do it.
Skip filler words, preamble, and transitions.
Focus output on: decisions needing user input, key findings, errors or blockers.
Use tables for structured data.
Tables MUST use `|`-pipe markdown form (`| col1 | col2 |` with `|---|---|` separator). NEVER pre-draw tables with Unicode box-drawing characters (┌ ─ ┐ │ ├ ┼ ┤ └ ┴ ┘) — the renderer relies on the `|` form to detect the table and re-flow it for narrow terminals; pre-drawn box tables overflow on small screens and break alignment.
Match the user's language. If the user writes in Chinese, respond in Chinese. If in English, respond in English.

## CONTENT-TRANSFORMATION TASKS:
When the user asks you to translate, format, convert, rewrite, refactor, or otherwise transform their input into output content (NOT summarize, NOT explain), output every line of the result in full.
NEVER use placeholders like `...`, `(以下省略)`, `(rest unchanged)`, `(此处继续 ...)`, `(continue similarly)`, `(略)`, `(其余类似)`, or `/* ... */` to skip content the user asked you to produce. These are bugs, not brevity. The user wants the artifact, not a sketch of it.
If the full output would exceed your token budget, write it to a file with `write_file` and report the path — do NOT inline-abbreviate. A file with every line is always better than a chat reply with `(...)`.
The brevity rule in OUTPUT above applies to your commentary on the work, not to the transformed content itself.

## CHINESE CODE SUPPORT:
Preserve existing Chinese comments and identifiers (Chinese/pinyin names, 中文 string literals, /** 中文 */ doc comments) and treat them as first-class — understand them, keep them in edits, match them correctly in search/replace.
For NEW code prefer English identifiers, but keep the file's existing naming convention.
When searching Chinese content, use Unicode-aware patterns (the grep tool supports Chinese regex).

## CONTEXT:
The system will automatically compress prior messages as context fills up. Your conversation is not limited by the context window. After compression, do NOT assume prior tool results are still available. Re-read files and re-check state before continuing.";

#[cfg(test)]
mod tests {
    use super::*;

    /// Lock the bash-chunking guidance into the prompt. 5-7 atomgr datalog
    /// (build 942b615) showed weak models burn 5-8 turns on what should be
    /// a single `&&`-chained bash call. If a future refactor accidentally
    /// drops this paragraph, this test catches it.
    #[test]
    fn unified_prompt_includes_bash_chunking_guidance() {
        let p = build_rules();
        assert!(
            p.contains("chain dependent shell steps"),
            "TOOLS section must keep the chunking principle"
        );
        assert!(
            p.contains("&&") && p.contains(";"),
            "must show the chain operators the model should use"
        );
        assert!(
            p.contains("ONE bash call"),
            "must call out the unit of chunking"
        );
    }

    /// Lock the parallel-mandate guidance. 5-7 atomgr datalog (build 2e6621f)
    /// 24 turn / 24 tool calls = 1.0 tool/turn average — model burns turns
    /// on independent reads/greps that should fly in parallel. The
    /// guidance teaches MANDATORY parallel scenarios + a concrete WRONG/
    /// RIGHT contrast so weak models with low directive-uptake can
    /// pattern-match and chunk correctly.
    #[test]
    fn unified_prompt_includes_mandatory_parallel_scenarios() {
        let p = build_rules();
        assert!(
            p.contains("MANDATORY parallel"),
            "TOOLS section must keep the mandatory-parallel header"
        );
        assert!(
            p.contains("read_file × N"),
            "must enumerate the read-many scenario"
        );
        assert!(
            p.contains("grep × N"),
            "must enumerate the search-many scenario"
        );
        assert!(
            p.contains("write_file × N"),
            "must enumerate the create-many scenario"
        );
        assert!(
            p.contains("WRONG") && p.contains("RIGHT"),
            "must include the WRONG/RIGHT contrast example"
        );
        assert!(
            p.contains("DEPENDS on step N's output"),
            "must explain when sequential is actually correct"
        );
    }

    /// Lock the anti-abbreviation guidance into the prompt. Small/fast
    /// models (e.g. deepseek-v4-flash) on long transformation tasks
    /// (translate a full doc, refactor a long function, output a long
    /// diff) skip the middle with placeholders like "(此处继续 ...)" /
    /// "(rest unchanged)" / "...". The OUTPUT section's "keep text brief"
    /// rule is upstream of this and was being mis-applied to the
    /// transformed content itself. This test locks both the section and
    /// a representative subset of the forbidden placeholder list — a
    /// future edit that drops the section or softens it (removes the
    /// `NEVER use placeholders` clause, or the `write_file` escape
    /// hatch) will fail here.
    #[test]
    fn unified_prompt_forbids_placeholder_abbreviation() {
        let p = build_rules();
        assert!(
            p.contains("CONTENT-TRANSFORMATION TASKS"),
            "must keep the anti-abbreviation section header"
        );
        assert!(
            p.contains("output every line of the result in full"),
            "must keep the full-output mandate"
        );
        // Representative subset of the forbidden placeholder list.
        // Covers both Chinese and English variants since the issue
        // first surfaced on a Chinese-language translation task.
        for token in &[
            "(以下省略)",
            "(rest unchanged)",
            "(此处继续 ...)",
            "(continue similarly)",
            "(略)",
        ] {
            assert!(
                p.contains(token),
                "must explicitly forbid placeholder `{}`",
                token
            );
        }
        assert!(
            p.contains("write_file") && p.contains("token budget"),
            "must offer write_file as escape hatch when over token budget"
        );
        assert!(
            p.contains("applies to your commentary"),
            "must distinguish brevity-of-commentary from brevity-of-content"
        );
    }

    /// Tech-stack-neutrality check for the parallel guidance paragraph.
    /// Other prompt sections may mention concrete commands (`cargo check`,
    /// `tsc --noEmit`), but the parallel paragraph stays at the generic
    /// tool-name level (read_file / grep / glob / write_file are
    /// framework-internal tool names, not tech-stack keywords).
    #[test]
    fn parallel_guidance_paragraph_stays_tech_neutral() {
        let p = build_rules();
        let start = p
            .find("MANDATORY parallel")
            .expect("parallel guidance must exist");
        // Inspect ~700 chars after the anchor (covers the WRONG/RIGHT
        // contrast block too).
        let para_end = (start + 700).min(p.len());
        let para = &p[start..para_end];
        for forbidden in &["cargo ", "npm ", "pytest", "go build", "mvn ", "gradle "] {
            assert!(
                !para.contains(forbidden),
                "parallel guidance must stay tech-neutral; found `{}` in:\n{}",
                forbidden,
                para
            );
        }
    }

    /// Tech-stack-neutrality check: the chunking paragraph stays generic.
    /// Other prompt sections still mention concrete commands as
    /// illustrations (e.g. `cargo check`, `tsc --noEmit`), but the
    /// chunking paragraph must not bloat the prompt with tool-specific
    /// examples. Guards against well-meaning future edits that add
    /// rust/node/python-specific deploy chains.
    #[test]
    fn shell_chunking_paragraph_stays_tech_neutral() {
        let p = build_rules();
        let start = p
            .find("chain dependent shell steps")
            .expect("chunking guidance must exist");
        // Inspect the paragraph (~500 chars after the anchor).
        let para_end = (start + 500).min(p.len());
        let para = &p[start..para_end];
        for forbidden in &["cargo ", "npm ", "pytest", "go build", "mvn ", "gradle "] {
            assert!(
                !para.contains(forbidden),
                "chunking paragraph must stay tech-neutral; found `{}`",
                forbidden
            );
        }
    }

    /// Lock the behavior-shaping clauses added for the deferential CN model
    /// family (deepseek/qwen/glm/kimi) and weak-model edit hygiene. A future
    /// edit that drops these silently regresses the exact failure modes they
    /// fix: rubber-stamping a wrong diagnosis, editing files via bash, over-
    /// commenting generated code, and stopping mid-task to ask permission.
    #[test]
    fn unified_prompt_includes_behavior_guards() {
        let p = build_rules();
        assert!(
            p.contains("Prioritize technical correctness over agreeing"),
            "must keep the anti-sycophancy / objectivity clause"
        );
        assert!(
            p.contains("Mutate files only with") && p.contains("search_replace"),
            "must forbid bash file mutation and name the edit tools"
        );
        assert!(
            p.contains("comment density"),
            "must keep the soft comment-density rule"
        );
        assert!(
            p.contains("CARRY IT THROUGH"),
            "must keep the carry-to-completion counterweight"
        );
    }

    /// Lock the consolidated anti-thrash guidance (optimization #2): STOP-WHEN-
    /// STUCK, "diagnose before retry", and command-failure handling were merged
    /// into one block. Assert the load-bearing parts survived and the old
    /// duplicate STOP-WHEN-STUCK bullet is gone (single source now).
    #[test]
    fn unified_prompt_consolidates_stuck_and_failure_guidance() {
        let p = build_rules();
        assert!(
            p.contains("WHEN STUCK OR A COMMAND FAILS"),
            "must keep the consolidated stuck/failure section"
        );
        assert!(
            p.contains("do NOT re-run the same command"),
            "must keep the don't-retry-the-same-command rule"
        );
        assert!(
            p.contains("3 rounds") && p.contains("STOP"),
            "must keep the stop-after-~3-fruitless-rounds rule"
        );
        assert!(
            !p.contains("STOP WHEN STUCK:"),
            "the standalone STOP WHEN STUCK bullet must be folded in (no duplicate)"
        );
    }

    /// Lock the compressed CHINESE CODE SUPPORT section (optimization #1): it
    /// was trimmed from 8 bullets to 3 lines; the load-bearing bits must remain
    /// — preserve existing Chinese comments/identifiers (cross-ref'd by the
    /// comment-density rule in DOING TASKS), prefer-English-for-new, and the
    /// Unicode-aware grep tip.
    #[test]
    fn chinese_support_keeps_load_bearing_bits() {
        let p = build_rules();
        assert!(
            p.contains("CHINESE CODE SUPPORT"),
            "must keep the Chinese-support section"
        );
        assert!(
            p.contains("Preserve existing Chinese comments and identifiers"),
            "must keep the preserve-existing rule (the comment-density rule cross-refs it)"
        );
        assert!(
            p.contains("prefer English identifiers"),
            "must keep the prefer-English-for-new-code rule"
        );
        assert!(
            p.contains("Unicode-aware"),
            "must keep the Unicode-aware grep tip"
        );
    }
}
