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
            if let (Some(success), Some(output)) = (msg.tool_result_success(), msg.tool_result_output()) {
                if last_tool_name.is_empty() {
                    last_result_success = success;
                    // Also check output for build failure keywords
                    let out = output.to_lowercase();
                    if out.contains("build failed") || out.contains("error") || out.contains("failed") {
                        last_result_success = false;
                    }
                }
            }
            if let crate::conversation::message::MessageContent::AssistantWithToolCalls { tool_calls, .. } = &msg.content {
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

    /// Inject a verification prompt into the conversation as a user message,
    /// forcing the model to check its work before declaring success.
    #[allow(dead_code)]
    pub(crate) fn inject_verify_prompt(&mut self) {
        let files = self.files_edited_this_turn.join(", ");
        let verify_msg = format!(
            "[SYSTEM: You edited {}. Before finishing, verify your changes work. \
             Run a quick check: look for syntax errors, check if the dev server shows errors, \
             or re-read a key edited file to confirm it's correct. \
             If you find errors, fix them now.]",
            files
        );
        // Inject as assistant thought + will trigger another LLM call
        self.conversation.push_delta(&verify_msg);
        self.conversation.finalize_stream();
    }

    /// ATLAS-style auto-compile verification after edits.
    /// Detects the project's compile command and runs it.
    /// Injects result into conversation so model sees errors immediately.
    /// Only runs once per "batch" of edits (tracked by last_compile_at_step).
    pub(crate) async fn auto_compile_verify(&mut self) {
        // Only auto-compile for compiled languages (Java/Rust/Go/TS).
        // Skip if we already compiled at this step count.
        static COMPILE_COMMANDS: &[(&str, &str)] = &[
            ("pom.xml", "mvn compile -q 2>&1 | tail -20"),
            ("build.gradle", "gradle compileJava -q 2>&1 | tail -20"),
            ("Cargo.toml", "cargo check 2>&1 | tail -20"),
            ("tsconfig.json", "npx tsc --noEmit 2>&1 | tail -20"),
            ("vite.config.ts", "npx vite build 2>&1 | tail -20"),
            ("vite.config.js", "npx vite build 2>&1 | tail -20"),
            ("go.mod", "go build ./... 2>&1 | tail -20"),
        ];

        let wd = self.turn_runner.context.working_dir.try_read()
            .map(|g| g.clone()).unwrap_or_default();

        // Find compile command by checking for build files (project root or subdirs)
        let mut compile_cmd: Option<String> = None;
        let mut compile_dir = wd.clone();

        for &(marker, cmd) in COMPILE_COMMANDS {
            // Check project root
            if wd.join(marker).exists() {
                compile_cmd = Some(cmd.to_string());
                compile_dir = wd.clone();
                break;
            }
            // Check all immediate subdirectories for marker files.
            // No hardcoded directory names — any subdir with a build marker gets checked.
            if let Ok(entries) = std::fs::read_dir(&wd) {
                for entry in entries.flatten() {
                    if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) { continue; }
                    let sub = entry.path();
                    if sub.join(marker).exists() {
                        compile_cmd = Some(format!("cd {} && {}", sub.display(), cmd));
                        compile_dir = sub;
                        break;
                    }
                }
            }
            if compile_cmd.is_some() { break; }
        }

        let compile_cmd = match compile_cmd {
            Some(c) => c,
            None => return, // No compiled language detected
        };

        // Auto-compile after every edit. The 5-10s compile cost is worth it —
        // catching errors immediately saves 5-10 steps of broken edit accumulation.

        // Run compile
        let output = tokio::process::Command::new("bash")
            .arg("-c")
            .arg(&compile_cmd)
            .current_dir(&compile_dir)
            .output()
            .await;

        match output {
            Ok(o) => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                let stderr = String::from_utf8_lossy(&o.stderr);
                let combined = format!("{}{}", stdout, stderr);

                if o.status.success() {
                    // Compile passed — inject short confirmation
                    self.build_fail_count = 0;
                    // Advance subtask driver on compile pass
                    if self.subtask_driver.active {
                        self.subtask_driver.advance();
                        if let Some(instr) = self.subtask_driver.current_instruction() {
                            self.conversation.add_user_message(&format!(
                                "[Auto-compile: PASSED]\n{}", instr
                            ));
                        } else {
                            self.conversation.add_user_message(
                                "[Auto-compile: PASSED. All subtasks done. Verify and summarize.]"
                            );
                        }
                    } else {
                        self.conversation.add_user_message("[Auto-compile: PASSED. Continue.]");
                    }
                } else {
                    // Compile failed — inject error with source diagnosis
                    self.build_fail_count += 1;
                    let enhanced = crate::tool::devserver::java::enhance_compile_error(
                        &combined, &compile_dir,
                    );
                    // Trim to keep context small
                    let error_lines: String = enhanced.lines()
                        .filter(|l| l.contains("[ERROR]") || l.contains(">>>") || l.contains("---") || l.contains("[AUTO"))
                        .take(20)
                        .collect::<Vec<_>>()
                        .join("\n");
                    let msg = if error_lines.is_empty() {
                        format!("[Auto-compile: FAILED]\n{}", combined.lines().take(15).collect::<Vec<_>>().join("\n"))
                    } else {
                        format!("[Auto-compile: FAILED]\n{}", error_lines)
                    };
                    self.conversation.add_user_message(&msg);
                }
            }
            Err(_) => {} // Compile command not available, skip
        }
    }

    /// Tree-sitter syntax check on recently edited files.
    /// Language-agnostic: works on any file tree-sitter can parse.
    /// Catches bracket mismatches, missing closings, duplicate declarations
    /// that build tools may miss (e.g., Vite doesn't catch Vue SFC syntax errors).
    pub(crate) async fn syntax_check_edited_files(&mut self) {
        let wd = self.turn_runner.context.working_dir.try_read()
            .map(|g| g.clone()).unwrap_or_default();

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
                    let lines_str = lines.iter()
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
    pub(crate) fn snapshot_devserver_log_sizes(&self) -> std::collections::HashMap<String, u64> {
        let wd = self.turn_runner.context.working_dir.try_read()
            .map(|g| g.clone()).unwrap_or_default();
        let candidates = [
            "frontend.log", "backend.log", "server.log",
            "frontend/frontend.log", "backend/backend.log",
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
    pub(crate) async fn check_devserver_logs(&mut self, pre_sizes: &std::collections::HashMap<String, u64>) {
        let wd = self.turn_runner.context.working_dir.try_read()
            .map(|g| g.clone()).unwrap_or_default();

        // Small delay to let HMR process the file change
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        for (log_name, &old_size) in pre_sizes {
            let log_path = wd.join(log_name);
            let new_size = match tokio::fs::metadata(&log_path).await {
                Ok(m) => m.len(),
                Err(_) => continue,
            };
            // No new content since edit → skip
            if new_size <= old_size { continue; }

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
                if skip < content.len() { &content[skip..] } else { continue; }
            };

            // Look for error patterns in the new content only
            let error_lines: Vec<&str> = new_content.lines()
                .filter(|l| {
                    let lower = l.to_lowercase();
                    (lower.contains("error") || lower.contains("failed") || lower.contains("syntaxerror"))
                        && !lower.contains("0 error")
                        && !lower.contains("error overlay")
                })
                .collect();

            if !error_lines.is_empty() {
                let errors: String = error_lines.iter()
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

    /// Detect partial Vue/React SFC edits: if a single edit only touched <script>
    /// but the file also has <template>, remind the model to use multi-edit.
    pub(crate) async fn check_vue_partial_edit(&mut self) {
        let wd = self.turn_runner.context.working_dir.try_read()
            .map(|g| g.clone()).unwrap_or_default();

        for file in &self.files_edited_this_turn {
            let is_sfc = file.ends_with(".vue") || file.ends_with(".svelte");
            if !is_sfc { continue; }

            let path = if std::path::Path::new(file).is_absolute() {
                std::path::PathBuf::from(file)
            } else {
                wd.join(file)
            };
            let content = match tokio::fs::read_to_string(&path).await {
                Ok(c) => c,
                Err(_) => continue,
            };

            let has_script = content.contains("<script");
            let has_template = content.contains("<template");
            if !has_script || !has_template { continue; }

            // Check if the last edit result for this file was a single edit (not multi-edit)
            let last_edit_was_single = self.conversation.messages.iter().rev()
                .filter_map(|m| match &m.content {
                    crate::conversation::message::MessageContent::ToolResult(r) if r.success => Some(&r.output),
                    _ => None,
                })
                .take(3)
                .any(|o| o.contains(file) && !o.contains("Multi-edit:"));

            if last_edit_was_single {
                // Append to last tool result instead of adding user message
                if let Some(last_msg) = self.conversation.messages.last_mut() {
                    if let crate::conversation::message::MessageContent::ToolResult(ref mut r) = last_msg.content {
                        r.output.push_str(&format!(
                            "\n\n[HINT: {} is a Vue SFC with both <script> and <template>. \
                             If you need to change both sections, use edit_file with edits array \
                             to change ALL regions in ONE call. Do NOT make separate edits.]",
                            file
                        ));
                    }
                }
            }
        }
    }
}
