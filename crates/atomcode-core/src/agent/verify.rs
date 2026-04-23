// ── INVARIANT (2026-04-16): DO NOT DELETE THIS FILE ──
// verify.rs was deleted once (01afc5b, "all dead_code") and had to be
// restored (4f704cb) after causing 22-49 turn sessions. The functions
// are called conditionally from mod.rs — #[allow(dead_code)] does NOT
// mean unused. If you think this file is dead, grep for `should_verify`
// and `inject_verify_prompt` in mod.rs before touching it.
// ──────────────────────────────────────────────────────────────────

use super::*;

impl AgentLoop {
    /// Check if the model should verify its changes before finishing.
    /// Returns true if: edits were made AND no bash/build command was run AFTER the last edit.
    #[allow(dead_code)]
    pub(crate) fn should_verify(&self) -> bool {
        if self.files_edited_this_turn.is_empty() {
            return false; // No edits, nothing to verify
        }
        if self.tool_call_count >= 20 {
            return false; // Near step limit, don't waste steps
        }

        // Check the LAST tool call and its result.
        // If it's a SUCCESSFUL bash → already verified. No need for another.
        // If it's a FAILED bash (build error) → need to verify/fix.
        // If it's edit/write/read → hasn't verified yet.
        let mut last_tool_name = String::new();
        let mut last_result_success = true;
        for msg in self.conversation.messages.iter().rev() {
            if let (Some(success), Some(output)) =
                (msg.tool_result_success(), msg.tool_result_output())
            {
                if last_tool_name.is_empty() {
                    last_result_success = success;
                    // Also check output for build failure keywords
                    let out = output.to_lowercase();
                    if out.contains("build failed")
                        || out.contains("error")
                        || out.contains("failed")
                    {
                        last_result_success = false;
                    }
                }
            }
            if let crate::conversation::message::MessageContent::AssistantWithToolCalls {
                tool_calls,
                ..
            } = &msg.content
            {
                if let Some(last_tc) = tool_calls.last() {
                    if last_tool_name.is_empty() {
                        last_tool_name = last_tc.name.clone();
                    }
                    // If last tool was bash AND it succeeded → no verify needed
                    // If last tool was bash AND it failed → verify/fix needed
                    return last_tool_name != "bash" || !last_result_success;
                }
            }
            if matches!(msg.role, crate::conversation::message::Role::User) {
                break;
            }
        }
        false
    }

    // ── INVARIANT (2026-04-16): verify prompt must NOT mention dev server ──
    // "check if the dev server shows errors" caused models to run `npm run dev`
    // for verification → 140-168s blocking waits. Always guide toward build/check
    // commands that exit immediately. Tech-stack neutral: no npm/cargo/mvn.
    // History: "dev server" wording survived 16 commits unnoticed. Fixed today.
    // ────────────────────────────────────────────────────────────────────────
    /// Inject a verification prompt into the conversation as a user message,
    /// forcing the model to check its work before declaring success.
    #[allow(dead_code)]
    pub(crate) fn inject_verify_prompt(&mut self) {
        let files = self.files_edited_this_turn.join(", ");
        let verify_msg = format!(
            "[SYSTEM: You edited {}. Before finishing, verify your changes work. \
             Run the project's build/check/compile command to catch errors. \
             Do NOT start any long-running process that does not exit on its own. \
             If you find errors, fix them now.]",
            files
        );
        // Inject as assistant thought + will trigger another LLM call
        self.conversation.push_delta(&verify_msg);
        self.conversation.finalize_stream();
    }

    /// Legacy auto-compile verification — DISABLED since Phase 4.2.
    /// Replaced by language-agnostic approach: discipline.rs now prompts
    /// the model to verify its own changes (build/test/lint/run).
    /// The model decides the appropriate verification for the project.
    #[allow(dead_code)]
    pub(crate) async fn auto_compile_verify(&mut self) {
        // No-op. Verification is now model-driven via discipline prompt.
        // See discipline.rs apply_post_turn_discipline() for the verification nudge.
    }

    /// Tree-sitter syntax check on recently edited files.
    /// Language-agnostic: works on any file tree-sitter can parse.
    /// Catches bracket mismatches, missing closings, duplicate declarations
    /// that build tools may miss (e.g., Vite doesn't catch Vue SFC syntax errors).
    /// Called for non-compiled projects as an auto-compile equivalent.
    #[allow(dead_code)]
    pub(crate) async fn syntax_check_edited_files(&mut self) {
        let wd = self
            .turn_runner
            .context
            .working_dir
            .try_read()
            .map(|g| g.clone())
            .unwrap_or_default();

        let mut warnings: Vec<String> = Vec::new();
        let mut searcher = self.turn_runner.context.semantic.lock().await;

        for file in &self.files_edited_this_turn {
            // Resolve to full path
            let path = if std::path::Path::new(file).is_absolute() {
                std::path::PathBuf::from(file)
            } else {
                wd.join(file)
            };
            if let Ok(content) = std::fs::read_to_string(&path) {
                let (errors, lines) = searcher.count_syntax_errors(&content, &path);
                if errors > 0 {
                    let lines_str = lines
                        .iter()
                        .map(|l| format!("L{}", l))
                        .collect::<Vec<_>>()
                        .join(", ");
                    warnings.push(format!(
                        "{}: {} syntax error(s) at {}",
                        file, errors, lines_str
                    ));
                }
            }
        }
        drop(searcher);

        if !warnings.is_empty() {
            let msg = format!(
                "[SYNTAX CHECK: {}. Fix these before continuing — the file structure may be broken.]",
                warnings.join("; ")
            );
            self.conversation.add_user_message(&msg);
        }
    }

    /// Snapshot dev server log sizes before an edit, so we can diff after.
    #[allow(dead_code)]
    pub(crate) fn snapshot_devserver_log_sizes(&self) -> std::collections::HashMap<String, u64> {
        let wd = self
            .turn_runner
            .context
            .working_dir
            .try_read()
            .map(|g| g.clone())
            .unwrap_or_default();
        let candidates = [
            "frontend.log",
            "backend.log",
            "server.log",
            "frontend/frontend.log",
            "backend/backend.log",
        ];
        let mut sizes = std::collections::HashMap::new();
        for name in &candidates {
            let path = wd.join(name);
            if let Ok(meta) = std::fs::metadata(&path) {
                sizes.insert(name.to_string(), meta.len());
            }
        }
        sizes
    }

    /// Check dev server logs for NEW errors after editing frontend/backend files.
    /// Only reads lines appended AFTER `pre_sizes` snapshot, ignoring stale errors.
    #[allow(dead_code)]
    pub(crate) async fn check_devserver_logs(
        &mut self,
        pre_sizes: &std::collections::HashMap<String, u64>,
    ) {
        let wd = self
            .turn_runner
            .context
            .working_dir
            .try_read()
            .map(|g| g.clone())
            .unwrap_or_default();

        // Small delay to let HMR process the file change
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        for (log_name, &old_size) in pre_sizes {
            let log_path = wd.join(log_name);
            let new_size = match tokio::fs::metadata(&log_path).await {
                Ok(m) => m.len(),
                Err(_) => continue,
            };
            // No new content since edit → skip
            if new_size <= old_size {
                continue;
            }

            // Read only the NEW bytes
            let content = match tokio::fs::read_to_string(&log_path).await {
                Ok(c) => c,
                Err(_) => continue,
            };
            let new_content = if old_size == 0 {
                &content
            } else {
                // Approximate: skip old_size bytes (may split a UTF-8 char, but log lines are mostly ASCII)
                let skip = old_size as usize;
                if skip < content.len() {
                    &content[skip..]
                } else {
                    continue;
                }
            };

            // Look for error patterns in the new content only
            let error_lines: Vec<&str> = new_content
                .lines()
                .filter(|l| {
                    let lower = l.to_lowercase();
                    (lower.contains("error")
                        || lower.contains("failed")
                        || lower.contains("syntaxerror"))
                        && !lower.contains("0 error")
                        && !lower.contains("error overlay")
                })
                .collect();

            if !error_lines.is_empty() {
                let errors: String = error_lines
                    .iter()
                    .take(5)
                    .map(|l| l.to_string())
                    .collect::<Vec<_>>()
                    .join("\n");
                let msg = format!(
                    "[DEV SERVER ERROR in {}:]\n{}\n\nFix these errors before continuing.",
                    log_name, errors
                );
                self.conversation.add_user_message(&msg);
                break; // One log file's errors is enough
            }
        }
    }

    /// No-op: Vue partial edit detection removed. Multi-edit is disabled;
    /// serial edit_file calls with old_string/new_string are the standard approach.
    #[allow(dead_code)]
    pub(crate) async fn check_vue_partial_edit(&mut self) {
        // Intentionally empty. Kept as stub to avoid changing call sites.
    }
}
