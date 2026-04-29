//! Phase 4 EXECUTE mode — focused edit execution with minimal context.
//!
//! When the model outputs a plan with edit instructions, the agent loop
//! switches to EXECUTE mode: each instruction is executed in isolation
//! with only the target file + instruction in context. This prevents
//! context noise from degrading edit precision.

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::AgentEvent;
use crate::turn::event::{TurnEvent, TurnResult};

/// A parsed edit instruction from REASON mode output.
#[derive(Debug, Clone)]
pub struct EditInstruction {
    /// Target file path (absolute or relative).
    pub file: String,
    /// What to do — the full instruction text for this file.
    pub instruction: String,
}

/// Parse EDIT INSTRUCTIONS from a REASON mode response.
/// Looks for patterns like:
///   ### File: TopBar.vue
///   Edit the avatar section...
///
///   ### File: App.vue
///   Import FootBar and add to template...
///
/// Also handles numbered list format:
///   1. TopBar.vue: change the avatar to use local image
///   2. App.vue: import and register FootBar component
pub fn parse_edit_instructions(text: &str) -> Vec<EditInstruction> {
    let mut instructions = Vec::new();

    // Strategy 1: "### File: X" sections
    let mut current_file: Option<String> = None;
    let mut current_lines: Vec<String> = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim();

        // Detect file header: "### File: X.vue" or "**File: X.vue**" or "## X.vue"
        let file_from_header = extract_file_from_header(trimmed);
        if let Some(file) = file_from_header {
            // Save previous instruction
            if let Some(prev_file) = current_file.take() {
                let instr = current_lines.join("\n").trim().to_string();
                if !instr.is_empty() {
                    instructions.push(EditInstruction {
                        file: prev_file,
                        instruction: instr,
                    });
                }
            }
            current_file = Some(file);
            current_lines.clear();
            continue;
        }

        if current_file.is_some() {
            current_lines.push(line.to_string());
        }
    }
    // Save last instruction
    if let Some(file) = current_file {
        let instr = current_lines.join("\n").trim().to_string();
        if !instr.is_empty() {
            instructions.push(EditInstruction {
                file,
                instruction: instr,
            });
        }
    }

    // Strategy 2: if no "### File:" headers found, try numbered list
    // "1. modify TopBar.vue — change avatar"
    // "2. update App.vue — add FootBar import"
    if instructions.is_empty() {
        for line in text.lines() {
            let trimmed = line.trim();
            // Match "N. filename.ext ..." or "- filename.ext ..."
            let rest = if trimmed.starts_with(|c: char| c.is_ascii_digit()) {
                trimmed.split_once('.').map(|(_, r)| r.trim())
            } else if trimmed.starts_with("- ") {
                Some(&trimmed[2..])
            } else {
                None
            };

            if let Some(rest) = rest {
                // Extract file name from beginning of rest
                let words: Vec<&str> = rest
                    .splitn(2, |c: char| {
                        c == ':' || c == '\u{2014}' || c == '-' || c == ' '
                    })
                    .collect();
                if let Some(first_word) = words.first() {
                    let first_word = first_word.trim().trim_matches('`');
                    if is_source_file(first_word) {
                        let file = first_word.to_string();
                        let instr = if words.len() > 1 {
                            words[1..].join(" ").trim().to_string()
                        } else {
                            String::new()
                        };
                        if !instr.is_empty() {
                            instructions.push(EditInstruction {
                                file,
                                instruction: instr,
                            });
                        }
                    }
                }
            }
        }
    }

    instructions
}

/// Extract file name from a header line.
fn extract_file_from_header(line: &str) -> Option<String> {
    // "### File: TopBar.vue" or "## File: TopBar.vue"
    if let Some(rest) = line.strip_prefix("###").or_else(|| line.strip_prefix("##")) {
        let rest = rest.trim();
        if let Some(file_part) = rest
            .strip_prefix("File:")
            .or_else(|| rest.strip_prefix("file:"))
        {
            let file = file_part.trim().trim_matches('`');
            if is_source_file(file) {
                return Some(file.to_string());
            }
        }
        // Just "### TopBar.vue"
        let bare = rest.trim_matches('*').trim();
        if is_source_file(bare) {
            return Some(bare.to_string());
        }
    }

    // "**TopBar.vue**:" or "**File: TopBar.vue**"
    if line.starts_with("**") && line.contains("**") {
        let inner = line
            .trim_start_matches("**")
            .split("**")
            .next()
            .unwrap_or("");
        let inner = inner
            .strip_prefix("File:")
            .or_else(|| inner.strip_prefix("file:"))
            .unwrap_or(inner)
            .trim();
        if is_source_file(inner) {
            return Some(inner.to_string());
        }
    }

    None
}

fn is_source_file(s: &str) -> bool {
    let s = s.trim_matches(|c: char| c == '`' || c == '*' || c == ':' || c == ' ');
    s.ends_with(".vue")
        || s.ends_with(".ts")
        || s.ends_with(".tsx")
        || s.ends_with(".js")
        || s.ends_with(".jsx")
        || s.ends_with(".java")
        || s.ends_with(".py")
        || s.ends_with(".rs")
        || s.ends_with(".go")
        || s.ends_with(".svelte")
        || s.ends_with(".html")
        || s.ends_with(".css")
}

/// Execute a list of edit instructions in EXECUTE mode.
/// Each instruction runs in isolation: fresh file read + minimal context.
/// Returns a summary of results.
pub async fn execute_instructions(
    instructions: Vec<EditInstruction>,
    runner: &mut crate::turn::runner::TurnRunner,
    event_tx: &tokio::sync::mpsc::UnboundedSender<AgentEvent>,
    working_dir: &std::path::Path,
) -> (Vec<String>, bool) {
    let mut summaries = Vec::new();
    let mut all_success = true;

    for (i, instr) in instructions.iter().enumerate() {
        // Resolve file path
        let file_path = if std::path::Path::new(&instr.file).is_absolute() {
            instr.file.clone()
        } else {
            // Search for the file in the working directory
            match find_file(working_dir, &instr.file) {
                Some(p) => p.to_string_lossy().to_string(),
                None => {
                    summaries.push(format!(
                        "EXECUTE {}/{}: {} — file not found",
                        i + 1,
                        instructions.len(),
                        instr.file
                    ));
                    all_success = false;
                    continue;
                }
            }
        };

        let _ = event_tx.send(AgentEvent::TextDelta(format!(
            "\n[EXECUTE {}/{}] Editing {} ...\n",
            i + 1,
            instructions.len(),
            instr.file
        )));

        // Create a turn event channel for this execute call
        let (turn_tx, mut turn_rx) = mpsc::unbounded_channel::<TurnEvent>();
        let cancel = CancellationToken::new();

        let result = runner
            .run_execute(&file_path, &instr.instruction, &turn_tx, cancel)
            .await;

        // Forward turn events to agent event channel
        while let Ok(event) = turn_rx.try_recv() {
            match event {
                TurnEvent::ToolCallResult {
                    ref call_id,
                    ref name,
                    ref output,
                    success,
                    ..
                } => {
                    let _ = event_tx.send(AgentEvent::ToolCallResult {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        output: output.clone(),
                        success,
                        duration: std::time::Duration::ZERO,
                    });
                }
                TurnEvent::TextDelta(text) => {
                    let _ = event_tx.send(AgentEvent::TextDelta(text));
                }
                TurnEvent::ReasoningDelta(text) => {
                    let _ = event_tx.send(AgentEvent::ReasoningDelta(text));
                }
                _ => {}
            }
        }

        match result {
            TurnResult::UsedTools { .. } => {
                summaries.push(format!(
                    "EXECUTE {}/{}: {} — edited",
                    i + 1,
                    instructions.len(),
                    instr.file
                ));
            }
            TurnResult::Responded { ref text, .. } => {
                // Model responded with text instead of calling edit_file — likely an error
                summaries.push(format!(
                    "EXECUTE {}/{}: {} — no edit (model said: {})",
                    i + 1,
                    instructions.len(),
                    instr.file,
                    text.chars().take(100).collect::<String>()
                ));
                all_success = false;
            }
            TurnResult::Failed(ref e) => {
                summaries.push(format!(
                    "EXECUTE {}/{}: {} — failed: {}",
                    i + 1,
                    instructions.len(),
                    instr.file,
                    e
                ));
                all_success = false;
            }
            TurnResult::Cancelled => {
                summaries.push(format!(
                    "EXECUTE {}/{}: {} — cancelled",
                    i + 1,
                    instructions.len(),
                    instr.file
                ));
                all_success = false;
                break;
            }
        }
    }

    (summaries, all_success)
}

/// Find a file by name in the working directory tree.
fn find_file(dir: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    // Check direct path first
    let direct = dir.join(name);
    if direct.exists() {
        return Some(direct);
    }

    // Walk the tree
    let walker = ignore::WalkBuilder::new(dir)
        .hidden(true)
        .git_ignore(true)
        .max_depth(Some(8))
        .build();

    for entry in walker {
        if let Ok(e) = entry {
            if e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                if let Some(fname) = e.path().file_name() {
                    if fname.to_string_lossy() == name {
                        return Some(e.into_path());
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_file_header_sections() {
        let text = "\
I'll modify two files:

### File: TopBar.vue
Replace the avatar with a local image.
Keep the tab navigation intact.

### File: App.vue
Import FootBar component and add it to the template.";

        let instrs = parse_edit_instructions(text);
        assert_eq!(instrs.len(), 2);
        assert_eq!(instrs[0].file, "TopBar.vue");
        assert!(instrs[0].instruction.contains("avatar"));
        assert_eq!(instrs[1].file, "App.vue");
        assert!(instrs[1].instruction.contains("FootBar"));
    }

    #[test]
    fn parse_numbered_list() {
        let text = "\
Plan:
1. TopBar.vue: change avatar to local image
2. App.vue: import and add FootBar
3. FootBar.vue: redesign with navigation";

        let instrs = parse_edit_instructions(text);
        assert_eq!(instrs.len(), 3);
        assert_eq!(instrs[0].file, "TopBar.vue");
        assert_eq!(instrs[2].file, "FootBar.vue");
    }

    #[test]
    fn parse_bold_headers() {
        let text = "\
**TopBar.vue**:
Fix the avatar section

**App.vue**:
Add FootBar import";

        let instrs = parse_edit_instructions(text);
        assert_eq!(instrs.len(), 2);
    }

    #[test]
    fn parse_no_instructions() {
        let text = "I looked at the code and everything seems fine.";
        let instrs = parse_edit_instructions(text);
        assert_eq!(instrs.len(), 0);
    }

    #[test]
    fn parse_single_file_instruction() {
        let text = "\
### File: IdeaCenter.vue
Change the formatDate function to output MM-DD HH:mm format.";

        let instrs = parse_edit_instructions(text);
        assert_eq!(instrs.len(), 1);
        assert_eq!(instrs[0].file, "IdeaCenter.vue");
        assert!(instrs[0].instruction.contains("formatDate"));
    }
}
