use super::*;

impl AgentLoop {
    /// Apply discipline after a turn with tool calls.
    /// Injects system reminders into conversation and tracks usage.
    pub(crate) fn apply_post_turn_discipline(&mut self) {
        // System reminders: re-inject rules + task every 4 steps.
        if self.tool_call_count > 0 && self.tool_call_count % 4 == 0 {
            let task_hint = if self.current_task.chars().count() > 100 {
                format!("{}...", self.current_task.chars().take(97).collect::<String>())
            } else {
                self.current_task.clone()
            };

            // Build file tracking status
            let read_list = if self.files_read_this_turn.is_empty() {
                "none".to_string()
            } else {
                self.files_read_this_turn.join(", ")
            };
            let edit_list = if self.files_edited_this_turn.is_empty() {
                "none yet — you should be editing!".to_string()
            } else {
                self.files_edited_this_turn.join(", ")
            };

            // GLM-5 needs explicit finish signals — it won't self-terminate like Opus.
            // After edits: tell it to verify then stop. Before edits: let it work.
            let urgency = if !self.files_edited_this_turn.is_empty() && self.tool_call_count >= 8 {
                "You have made edits. Verify with ONE check (build/test/run), \
                 then summarize what you changed and STOP."
            } else if self.files_edited_this_turn.is_empty() && self.tool_call_count >= 30 {
                "30+ steps without edits. Either edit now or explain your findings and STOP."
            } else {
                "Continue working on the task."
            };

            let sibling_hint = String::new();

            let reminder = format!(
                "\n\n<system-reminder>\n\
                 TASK: \"{}\"\n\
                 STEP: {}\n\
                 FILES READ: {}\n\
                 FILES EDITED: {}\n\
                 {}\n\
                 {}\
                 </system-reminder>",
                task_hint, self.tool_call_count,
                read_list, edit_list, urgency, sibling_hint
            );

            // Append reminder to the last tool result in conversation
            if let Some(last_msg) = self.conversation.messages.last_mut() {
                match &mut last_msg.content {
                    crate::conversation::message::MessageContent::ToolResult(ref mut r) => {
                        r.output.push_str(&reminder);
                    }
                    _ => {}
                }
            }
        }

        // Plan adherence check: if model has a plan but is working on files NOT in the plan,
        // inject a strong reminder to return to the plan.
        if let Some(ref plan) = self.plan_text {
            if self.tool_call_count >= 8 && self.subtask_driver.subtasks.iter().any(|t| !t.done) {
                // Check if recent work (files read/edited) overlaps with planned files
                let planned_files: Vec<&str> = self.subtask_driver.subtasks.iter()
                    .filter(|t| !t.done)
                    .map(|t| t.file.as_str())
                    .collect();

                let working_on_plan = self.files_read_this_turn.iter()
                    .chain(self.files_edited_this_turn.iter())
                    .any(|f| planned_files.iter().any(|p| f.contains(p) || p.contains(f.as_str())));

                if !working_on_plan && !planned_files.is_empty() {
                    let remaining = planned_files.join(", ");
                    let plan_preview: String = plan.chars().take(200).collect();
                    let adherence_warning = format!(
                        "\n\n<system-reminder>\n\
                         ⚠ PLAN DRIFT DETECTED — you are NOT working on any planned file.\n\
                         YOUR ORIGINAL PLAN:\n{}\n\n\
                         REMAINING TASKS: {}\n\
                         STOP what you are doing. Return to your plan. \
                         Edit the next planned file NOW.\n\
                         </system-reminder>",
                        plan_preview, remaining
                    );
                    if let Some(last_msg) = self.conversation.messages.last_mut() {
                        if let crate::conversation::message::MessageContent::ToolResult(ref mut r) = last_msg.content {
                            r.output.push_str(&adherence_warning);
                        }
                    }
                }
            }
        }

        // Re-read guard: when the same file is read 4+ times, the model is stuck.
        // Re-inject the original task and force a re-plan.
        let mut blocked_files: Vec<String> = Vec::new();
        for (file, count) in &self.discipline_state.file_read_counts {
            if *count >= 4 {
                blocked_files.push(format!("{} ({}x)", file, count));
            }
        }
        if !blocked_files.is_empty() {
            let task = if self.current_task.is_empty() {
                "the user's request".to_string()
            } else {
                format!("\"{}\"", self.current_task)
            };
            let warning = format!(
                "[You are stuck — read {} repeatedly without making progress.]\n\
                 STOP reading. Re-read the original task: {}\n\
                 Now re-plan from scratch:\n\
                 1. What EXACTLY needs to change?\n\
                 2. Which file, which lines?\n\
                 3. Edit NOW or tell the user you cannot do it.",
                blocked_files.join(", "), task
            );
            self.conversation.add_user_message(&warning);
            // Reset counts so the model gets another chance after re-planning
            self.discipline_state.file_read_counts.clear();
        }

        // NOTE: Silent-round progress prompt disabled — add_user_message injections
        // confuse weak models and waste context. Let the model work silently.

        // Turn-budget phase reminders. Triggered when a max_turns cap is set
        // (typical for non-interactive runs like the SWE-bench eval harness).
        // We inject ONCE when the turn count crosses the 50% and 80% marks
        // of the budget, telling the LLM in command form to stop exploring
        // and start writing code. Weak models (e.g. GLM-5) ignore implicit
        // "you have N turns left" hints; explicit imperatives perform better.
        if let Some(max) = self.max_turns {
            if max >= 6 {
                let half = max / 2;
                let near_end = max.saturating_sub(5).max(half + 1);
                let just_crossed = |threshold: usize| -> bool {
                    self.turn_count == threshold
                };
                let phase_msg: Option<&str> = if just_crossed(half) {
                    Some(
                        "<system-reminder>\n\
                         [BUDGET 50%] You have used half of your turn budget. \
                         STOP opening new files for exploration. Based on what you \
                         already know, decide the root cause and start producing \
                         the fix with edit_file/write_file in the next turn. \
                         If you still have not located the bug, make an educated \
                         guess from the context you have rather than reading more.\n\
                         </system-reminder>"
                    )
                } else if just_crossed(near_end) {
                    Some(
                        "<system-reminder>\n\
                         [FINAL ROUNDS] Only a few turns remain. Do NOT call \
                         read_file or grep again. Verify the patch you have \
                         already produced (re-read only the files you edited if \
                         strictly necessary) and emit your final answer. If your \
                         patch is incomplete, finish it now — partial code is \
                         still gradeable, but no patch is not.\n\
                         </system-reminder>"
                    )
                } else {
                    None
                };
                if let Some(msg) = phase_msg {
                    if let Some(last_msg) = self.conversation.messages.last_mut() {
                        if let crate::conversation::message::MessageContent::ToolResult(ref mut r) = last_msg.content {
                            r.output.push_str("\n\n");
                            r.output.push_str(msg);
                        }
                    }
                }
            }
        }

        // Repeated error detection: if the last 3 tool results all failed with
        // the same error message, inject guidance to try a different approach.
        // This catches loops like: bash timeout="60.0" failing 18 times,
        // or edit_file missing new_string 5 times.
        {
            let mut recent_fail_msg: Option<String> = None;
            for msg in self.conversation.messages.iter().rev().take(6) {
                if let crate::conversation::message::MessageContent::ToolResult(ref r) = msg.content {
                    if !r.success {
                        let err_key: String = r.output.chars().take(60).collect();
                        if !err_key.is_empty() {
                            self.discipline_state.recent_errors.push(err_key);
                        }
                    } else {
                        // Success breaks the streak
                        break;
                    }
                }
            }
            // Check if last 3 errors are the same
            if self.discipline_state.recent_errors.len() >= 3 {
                let last = &self.discipline_state.recent_errors[self.discipline_state.recent_errors.len() - 1];
                let consecutive = self.discipline_state.recent_errors.iter().rev().take(3)
                    .all(|e| e == last);
                if consecutive {
                    recent_fail_msg = Some(last.clone());
                }
            }
            if let Some(err) = recent_fail_msg {
                let warning = format!(
                    "\n\n[REPEATED ERROR: The same error occurred 3+ times: \"{}...\"]\n\
                     STOP retrying the same approach. The error is NOT in the command — \
                     it is in how you are calling the tool. Try a completely different approach.",
                    err
                );
                self.conversation.add_user_message(&warning);
                self.discipline_state.recent_errors.clear();
            }
        }
    }

    /// Check if step limit has been reached.
    /// No hard limit — model decides when to stop. Only a safety cap at 200
    /// to prevent runaway API costs from infinite loops.
    pub(crate) fn check_step_limit(&self) -> bool {
        self.tool_call_count >= 200
    }

    /// Check if the turn budget (AgentLoop.max_turns) has been reached.
    /// Returns false when no cap is set (unbounded — historical behavior).
    /// Mirrored by `check_turn_limit_impl` in turn/tests.rs; keep both in sync.
    pub(crate) fn check_turn_limit(&self) -> bool {
        self.max_turns.map_or(false, |m| self.turn_count >= m)
    }

    /// Find sibling files (same directory, same extension) of edited files
    /// and suggest the model check them for the same bug pattern.
    #[allow(dead_code)]
    pub(crate) fn find_sibling_files_hint(&self) -> String {
        let wd: PathBuf = self.turn_runner.context.working_dir
            .try_read()
            .map(|g| g.clone())
            .unwrap_or_default();

        let mut siblings: Vec<String> = Vec::new();
        let mut seen_dirs: std::collections::HashSet<String> = std::collections::HashSet::new();

        for _edited in &self.files_edited_this_turn {
            // Reconstruct full path from short path
            // edited is like ".../views/SearchView.vue"
            // We need to find the directory and list siblings
            for msg in self.conversation.messages.iter().rev() {
                if let crate::conversation::message::MessageContent::AssistantWithToolCalls { tool_calls, .. } = &msg.content {
                    for tc in tool_calls {
                        if tc.name == "edit_file" || tc.name == "create_file" {
                            if let Ok(args) = serde_json::from_str::<serde_json::Value>(&tc.arguments) {
                                if let Some(fp) = args.get("file_path").and_then(|v| v.as_str()) {
                                    let path = std::path::Path::new(fp);
                                    if let (Some(dir), Some(ext)) = (path.parent(), path.extension()) {
                                        let dir_key = dir.to_string_lossy().to_string();
                                        if seen_dirs.contains(&dir_key) { continue; }
                                        seen_dirs.insert(dir_key);

                                        // List sibling files with same extension
                                        if let Ok(entries) = std::fs::read_dir(dir) {
                                            for entry in entries.flatten() {
                                                let name = entry.file_name().to_string_lossy().to_string();
                                                let entry_path = entry.path();
                                                if entry_path.extension() == Some(ext)
                                                    && entry_path != path
                                                    && !self.files_edited_this_turn.iter().any(|e| name.contains(e) || e.contains(&name))
                                                {
                                                    let rel = entry_path.strip_prefix(&wd)
                                                        .map(|p| p.to_string_lossy().to_string())
                                                        .unwrap_or_else(|_| name.clone());
                                                    siblings.push(rel);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        if siblings.is_empty() {
            return String::new();
        }

        siblings.truncate(5);
        format!(
            "IMPORTANT: You fixed a bug in {}. These sibling files may have the SAME bug: {}. Check them before finishing.\n",
            self.files_edited_this_turn.join(", "),
            siblings.join(", ")
        )
    }
}
