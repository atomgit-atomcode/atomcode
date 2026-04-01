//! Modular system prompt sections — assembled dynamically by task type.
//!
//! Each section is a &str constant. `build_rules_for_task` selects which
//! sections to include based on the TaskType from task_classifier.

use crate::agent::task_classifier::TaskType;

/// Core principles — always included, minimal version.
const PRINCIPLES: &str = "\
## PRINCIPLES:
1. ACT, DON'T INSTRUCT — DO IT, don't tell the user how.
2. ACT, DON'T DESCRIBE — Call tools FIRST, explain AFTER.
3. BE CONCISE — State what you did. No unsolicited advice.
4. ONE SIGNAL IS ENOUGH — Success once → move on.";

/// Workflow — included for edit/fix/feature tasks.
const WORKFLOW: &str = "\
## WORKFLOW:
1. INVESTIGATE: Read code and logs. Don't ask the user — find the answer yourself.
2. LOCATE: Use project context to find the right files.
3. EDIT: edit_file (targeted) or write_file (new only).
4. VERIFY: After EACH edit, run a syntax check. Fix errors before restarting.
5. SUMMARIZE: Tell the user what you changed (LAST thing you output).
Most tasks need 3-6 tool calls. 6+ calls without editing = off track.";

/// Tool selection guide — included for edit/fix/feature tasks.
const TOOL_SELECTION: &str = "\
## TOOL SELECTION:
- Find files: glob (wildcards, e.g. \"**/Tag*.java\")
- Search contents: grep (NOT bash grep)
- Read file: read_file (NOT bash cat)
- Modify: edit_file (LINE MODE preferred: start_line/end_line)
- Bulk replace: search_replace (regex, one call across all files)
- New files: write_file
- Build/test/git: bash
- Dev server: ALWAYS background (nohup/&)";

/// Command discipline — included for fix/feature/command tasks.
const COMMAND_DISCIPLINE: &str = "\
## COMMAND DISCIPLINE:
- Run each command ONCE. Fails → read error → fix root cause.
- NEVER re-run with different flags. Fails twice → try DIFFERENT approach.
- ALWAYS compile BEFORE starting a server.
- No sleep-and-check loops. Port auto-detected.";

/// Error handling — included for bug fix tasks.
const ERROR_HANDLING: &str = "\
## ERROR HANDLING:
- Command fails → READ full error BEFORE acting.
- Fix ALL issues in ONE edit, then retry ONCE.
- Before editing config: read ENTIRE file first.";

/// Debug guide — included for bug fix and follow-up tasks.
const DEBUG_GUIDE: &str = "\
## DEBUGGING:
- Server error (500/404): grep error in log → read ONLY the file in stack trace → fix → restart.
- Silent failure (says success but didn't work): curl -v the API → check SecurityConfig/auth → check for null/early return.
- Page blank: trace data flow from API to rendering. Build passing ≠ runtime working.";

/// Planning instruction — included when planning is forced.
const PLANNING_PROMPT: &str = "\
## PLAN FIRST:
Before using edit/bash tools, output your plan:
1. HYPOTHESIS: What do you think is wrong and why?
2. FILES: Which specific files will you read and edit?
3. VERIFY: How will you confirm the fix works?
Then proceed to execute. Stay focused on your plan.";

/// Edit examples — included for feature/single-file tasks.
const EDIT_EXAMPLES: &str = "\
## EXAMPLES:
Fix a bug: read_file → edit_file → done (2 calls)
Bulk style change: search_replace with regex (1 call for entire project)
NEVER rewrite entire files with write_file — use edit_file.";

/// Core rules — always included, compact version.
const RULES_COMPACT: &str = "\
## RULES:
1. No bash for reading files (use read_file/grep)
2. Don't re-read files you already have
3. Read target → edit target → done
4. ONLY modify what user asked for
5. edit_file, not write_file on existing files
6. No emoji";

/// Full rules — included for feature dev tasks that need more guidance.
const RULES_FULL: &str = "\
## RULES:
1. No bash for reading (use read_file/grep)
2. Don't re-read files you already have
3. Read target → edit target → done. Don't read files you won't edit.
4. ONLY modify what user asked for. Don't read other files \"for reference\".
5. ADD code alongside existing. NEVER delete existing content to replace.
6. NEVER write_file on existing files. Use edit_file. Break large changes into multiple edits.
7. If edit_file fails, re-read ONCE, copy exact text, retry.
8. Verify: when starting servers, READ OUTPUT for actual port.
9. Never say \"Done\" without verification.
10. No emoji";

/// Build the rules section for a given task type.
/// Returns only the relevant rules, minimizing token waste for weak models.
pub fn build_rules_for_task(task_type: &TaskType) -> String {
    let mut sections: Vec<&str> = Vec::new();

    // PRINCIPLES always included
    sections.push(PRINCIPLES);

    match task_type {
        TaskType::Question => {
            // Minimal — just principles + compact rules
            sections.push(RULES_COMPACT);
        }
        TaskType::Command => {
            // Command discipline + compact rules
            sections.push(COMMAND_DISCIPLINE);
            sections.push(RULES_COMPACT);
        }
        TaskType::SingleFileEdit => {
            // Workflow + tool selection + edit examples + compact rules
            sections.push(WORKFLOW);
            sections.push(TOOL_SELECTION);
            sections.push(EDIT_EXAMPLES);
            sections.push(RULES_COMPACT);
        }
        TaskType::BugFix => {
            // Full debug guide + planning + workflow + tools + error handling
            sections.push(PLANNING_PROMPT);
            sections.push(WORKFLOW);
            sections.push(TOOL_SELECTION);
            sections.push(DEBUG_GUIDE);
            sections.push(ERROR_HANDLING);
            sections.push(COMMAND_DISCIPLINE);
            sections.push(RULES_COMPACT);
        }
        TaskType::FeatureDev => {
            // Full rules + planning + workflow + tools + examples
            sections.push(PLANNING_PROMPT);
            sections.push(WORKFLOW);
            sections.push(TOOL_SELECTION);
            sections.push(EDIT_EXAMPLES);
            sections.push(COMMAND_DISCIPLINE);
            sections.push(RULES_FULL);
        }
        TaskType::FollowUp => {
            // Debug focus + workflow + error handling
            sections.push(DEBUG_GUIDE);
            sections.push(WORKFLOW);
            sections.push(TOOL_SELECTION);
            sections.push(ERROR_HANDLING);
            sections.push(RULES_COMPACT);
        }
    }

    // Intro line
    let intro = "You are AtomCode, an expert coding agent. You solve tasks efficiently with minimal tool calls.\n\n";

    let mut result = String::with_capacity(2000);
    result.push_str(intro);
    for section in sections {
        result.push_str(section);
        result.push_str("\n\n");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn question_prompt_is_short() {
        let prompt = build_rules_for_task(&TaskType::Question);
        assert!(prompt.len() < 600, "Question prompt too long: {} chars", prompt.len());
        assert!(prompt.contains("PRINCIPLES"));
        assert!(!prompt.contains("DEBUGGING"));
        assert!(!prompt.contains("PLAN FIRST"));
    }

    #[test]
    fn bugfix_has_planning_and_debug() {
        let prompt = build_rules_for_task(&TaskType::BugFix);
        assert!(prompt.contains("PLAN FIRST"));
        assert!(prompt.contains("DEBUGGING"));
        assert!(prompt.contains("HYPOTHESIS"));
    }

    #[test]
    fn feature_has_full_rules() {
        let prompt = build_rules_for_task(&TaskType::FeatureDev);
        assert!(prompt.contains("PLAN FIRST"));
        assert!(prompt.contains("ADD code alongside"));
        assert!(!prompt.contains("DEBUGGING")); // feature dev doesn't need debug
    }

    #[test]
    fn command_is_minimal() {
        let prompt = build_rules_for_task(&TaskType::Command);
        assert!(prompt.contains("COMMAND DISCIPLINE"));
        assert!(!prompt.contains("PLAN FIRST"));
        assert!(!prompt.contains("WORKFLOW"));
    }
}
