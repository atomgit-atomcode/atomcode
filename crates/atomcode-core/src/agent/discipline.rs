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

            let urgency = if !self.files_edited_this_turn.is_empty() && self.build_fail_count == 0 && self.tool_call_count >= 12 {
                "REMINDER: You already edited files and compile passed. \
                 Verify you are still working on the original request. \
                 If done, summarize your changes."
            } else if self.files_edited_this_turn.is_empty() && self.tool_call_count >= 20 {
                "GUIDANCE: 20+ steps without editing. Consider one of:\n\
                 1. DESIGN question → explain analysis and propose options.\n\
                 2. BUG → state hypothesis and edit the suspected file.\n\
                 3. STUCK → tell the user what is unclear."
            } else if self.tool_call_count >= 15 {
                "REMINDER: 15+ steps used. Prioritize taking action: edit code or explain findings."
            } else if self.files_edited_this_turn.is_empty() && self.tool_call_count >= 10 {
                "REMINDER: 10+ steps without edits. Consider acting: edit_file, bash, or explain to user."
            } else if self.files_edited_this_turn.is_empty() && self.tool_call_count >= 6 {
                "Focus on files you plan to edit."
            } else {
                "Focus on files you plan to edit."
            };

            let sibling_hint = String::new();

            let reminder = format!(
                "\n\n<system-reminder>\n\
                 TASK: \"{}\"\n\
                 STEP: {}/{}\n\
                 FILES READ: {}\n\
                 FILES EDITED: {}\n\
                 {}\n\
                 {}\
                 </system-reminder>",
                task_hint, self.tool_call_count,
                25 + self.files_edited_this_turn.len() * 5,
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

        // Re-read guard: inject warnings for files read too many times.
        // count >= 3: hard block warning — the model is looping on the same file.
        // count >= 2 without offset in last read: warn about full re-reads.
        let mut reread_warnings: Vec<String> = Vec::new();
        for (file, count) in &self.file_read_counts {
            if *count >= 3 {
                reread_warnings.push(format!(
                    "[BLOCKED: You have read {} {} times this turn. \
                     You already have the content. STOP re-reading and use what you have. \
                     If you need to edit, use edit_file now.]",
                    file, count
                ));
            }
        }
        if !reread_warnings.is_empty() {
            let warning = reread_warnings.join("\n");
            if let Some(last_msg) = self.conversation.messages.last_mut() {
                match &mut last_msg.content {
                    crate::conversation::message::MessageContent::ToolResult(ref mut r) => {
                        r.output.push_str(&format!("\n{}", warning));
                    }
                    _ => {}
                }
            }
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
    }

    /// Check if step limit has been reached.
    /// Base limit is 50. Each edited/created file adds 5. Hard cap at 100.
    pub(crate) fn check_step_limit(&self) -> bool {
        let dynamic_limit = 50 + (5 * self.files_edited_this_turn.len());
        let hard_limit = dynamic_limit.min(100);
        self.tool_call_count >= hard_limit
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
