use super::*;
use crate::conversation::message::MessageContent;

impl AgentLoop {
    /// Forward a TurnEvent to the TUI as an AgentEvent.
    /// Also writes to the datalog for persistent turn logging.
    pub(crate) fn forward_turn_event(&mut self, event: TurnEvent) {
        match event {
            TurnEvent::TextDelta(text) => {
                self.discipline_state.model_produced_text = true;
                let _ = self.event_tx.send(AgentEvent::TextDelta(text));
            }
            TurnEvent::ReasoningDelta(text) => {
                let _ = self.event_tx.send(AgentEvent::ReasoningDelta(text));
            }
            TurnEvent::ToolCallStreaming { name, hint } => {
                let _ = self
                    .event_tx
                    .send(AgentEvent::ToolCallStreaming { name, hint });
            }
            TurnEvent::ToolBatchStarted { batch_id, calls } => {
                let _ = self
                    .event_tx
                    .send(AgentEvent::ToolBatchStarted { batch_id, calls });
            }
            TurnEvent::ToolBatchCompleted {
                batch_id,
                ok,
                total,
                elapsed_ms,
            } => {
                let _ = self.event_tx.send(AgentEvent::ToolBatchCompleted {
                    batch_id,
                    ok,
                    total,
                    elapsed_ms,
                });
            }
            TurnEvent::ToolOutputChunk { call_id, chunk } => {
                let _ = self
                    .event_tx
                    .send(AgentEvent::ToolOutputChunk { call_id, chunk });
            }
            TurnEvent::ToolCallStarted {
                ref id,
                ref name,
                ref arguments,
            } => {
                // Dedupe across retries — see the matching guard in
                // `agent/mod.rs` inline forward path. Same id arriving
                // twice means the previous attempt's stream got cut off
                // (429 / timeout) and was retried; we've already painted
                // a row for it.
                if !self.emitted_tool_ids.insert(id.clone()) {
                    return;
                }
                self.datalog.log_tool_call(name, arguments);

                self.current_tool_name = name.clone();
                self.phase = AgentPhase::CallingTool(name.clone());
                let _ = self
                    .event_tx
                    .send(AgentEvent::PhaseChange(self.phase.clone()));

                // Track bash command for failure categorization
                if name == "bash" {
                    if let Ok(args) = serde_json::from_str::<serde_json::Value>(arguments) {
                        self.discipline_state.last_bash_cmd = args
                            .get("command")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                    }
                }

                // Track files for Working Set
                if matches!(
                    name.as_str(),
                    "read_file" | "edit_file" | "create_file" | "search_replace" | "glob" | "grep"
                ) {
                    if let Ok(args) = serde_json::from_str::<serde_json::Value>(arguments) {
                        let fp = args
                            .get("file_path")
                            .and_then(|v| v.as_str())
                            .or_else(|| args.get("path").and_then(|v| v.as_str()));
                        if let Some(fp) = fp {
                            let short = std::path::Path::new(fp)
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_else(|| fp.to_string());
                            self.session_files
                                .insert(short, std::path::PathBuf::from(fp));
                        }
                    }
                }

                let _ = self.event_tx.send(AgentEvent::ToolCallStarted {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                });
            }
            TurnEvent::ToolCallResult {
                call_id,
                name,
                output,
                success,
                duration,
            } => {
                // Track files for discipline
                if let Some(pos) = output.find("Edited ") {
                    // Extract full path from "Edited /path/to/file ..." or "Edited /path/to/file\n..."
                    let rest = &output[pos + 7..];
                    let full_path_end = rest
                        .find(|c: char| c == ' ' || c == '\n' || c == '(')
                        .unwrap_or(rest.len());
                    let full_path_str = rest[..full_path_end].trim();
                    if !full_path_str.is_empty() {
                        self.active_file = Some(PathBuf::from(full_path_str));
                    }
                    if !full_path_str.is_empty() {
                        let file = full_path_str.to_string();
                        if !self.files_edited_this_turn.contains(&file) {
                            self.files_edited_this_turn.push(file);
                        }
                    }
                }
                if let Some(pos) = output.find("Wrote ").or_else(|| output.find("Overwrote ")).or_else(|| output.find("Created new file ")) {
                    let keyword_len = if output[pos..].starts_with("Overwrote ") {
                        10
                    } else if output[pos..].starts_with("Created new file ") {
                        17
                    } else {
                        6
                    };
                    let rest = &output[pos + keyword_len..];
                    let full_path_end = rest
                        .find(|c: char| c == ' ' || c == '\n' || c == '(')
                        .unwrap_or(rest.len());
                    let full_path_str = rest[..full_path_end].trim();
                    if !full_path_str.is_empty() {
                        self.active_file = Some(PathBuf::from(full_path_str));
                    }
                    if !full_path_str.is_empty() {
                        let file = full_path_str.to_string();
                        if !self.files_edited_this_turn.contains(&file) {
                            self.files_edited_this_turn.push(file);
                        }
                    }
                }
                if success {
                    track_tool_modified_files(
                        &name,
                        &self.discipline_state.last_bash_cmd,
                        &output,
                        &mut self.files_edited_this_turn,
                    );
                }
                if matches!(
                    name.as_str(),
                    "read_file" | "list_directory" | "glob" | "grep"
                ) {
                    self.discipline_state.consecutive_reads += 1;
                } else if matches!(name.as_str(), "edit_file" | "create_file") {
                    self.discipline_state.consecutive_reads = 0;
                }

                // Track scouting commands for datalog metrics (no injection).
                if name == "bash" {
                    let cmd = self.discipline_state.last_bash_cmd.to_lowercase();
                    if cmd.contains("curl")
                        || cmd.contains("lsof")
                        || cmd.contains("ps aux")
                        || cmd.contains("tail")
                    {
                        self.discipline_state.scouting_count += 1;
                    }
                } else if matches!(name.as_str(), "read_file" | "edit_file" | "create_file") {
                    self.discipline_state.scouting_count = 0;
                }

                self.datalog.log_tool_result(&output, success);
                let _ = self.event_tx.send(AgentEvent::ToolCallResult {
                    call_id,
                    name,
                    output,
                    success,
                    duration,
                });
            }
            TurnEvent::TokenUsage {
                prompt_tokens,
                completion_tokens,
                total_tokens: _,
                cached_tokens,
            } => {
                if cached_tokens > 0 {
                    self.datalog.log_cache_hit(prompt_tokens, cached_tokens);
                }
                let _ = self
                    .event_tx
                    .send(AgentEvent::TokenUsage(crate::stream::TokenUsage {
                        prompt_tokens,
                        completion_tokens,
                        cached_tokens,
                    }));
            }
            TurnEvent::ContextStats {
                system_tokens,
                sent_tokens,
                dropped_tokens,
                working_set_tokens,
                total_messages,
            } => {
                self.datalog.log_context_stats(
                    system_tokens,
                    sent_tokens,
                    dropped_tokens,
                    working_set_tokens,
                    total_messages,
                );
                // Narrow stats — rich breakdown comes from handle_send_message.
                let _ = self.event_tx.send(AgentEvent::ContextStats {
                    system_tokens,
                    sent_tokens,
                    dropped_tokens,
                    working_set_tokens,
                    total_messages,
                    tool_defs_tokens: 0,
                    cold_zone_tokens: 0,
                    ctx_window: 0,
                    ctx_name: String::new(),
                    system_prompt: String::new(),
                });
            }
            TurnEvent::Error(e) => {
                let _ = self.event_tx.send(AgentEvent::Error {
                    error: e,
                    snapshot: self.conversation.snapshot(),
                });
            }
            TurnEvent::Warning(w) => {
                self.datalog.log_warning(&w);
                let _ = self.event_tx.send(AgentEvent::Warning(w));
            }
            TurnEvent::WorkingDirChanged(new_dir) => {
                // The tool itself (change_dir / bash cd) already mutated
                // the shared `ctx.working_dir`. Just surface the new path
                // so the TUI footer can redraw. Deliberately NOT doing the
                // heavier work that `services.rs::change_dir` performs for
                // `/cd` (clearing the conversation, reloading the code
                // graph, spawning a new indexer) — those are destructive
                // mid-turn; the LLM expects its context to survive a cd.
                let _ = self.event_tx.send(AgentEvent::WorkingDirChanged(new_dir));
            }
            TurnEvent::ApprovalRequested { .. } => {
                // ApprovalRequested is handled inline in the `select!`
                // loop inside `run_turn_loop`, not through this dispatch
                // method.  The event carries conversation.messages for
                // mid-turn persistence; the approval flow itself is
                // managed by the `approval_req_rx` channel.
            }
            TurnEvent::RateLimited { reset_at_display, reset_label, secs_until_reset, auto_resuming } => {
                let _ = self.event_tx.send(AgentEvent::RateLimited {
                    reset_at_display,
                    reset_label,
                    secs_until_reset,
                    auto_resuming,
                });
            }
        }
    }

    /// Post-process tool results added by TurnRunner: CLI-specific
    /// semantic enrichment (pre-read files mentioned in failed-bash
    /// errors). The generic byte-level truncation (head/tail caps,
    /// 300-line universal cap, 32K char cap, per-turn budget) moved
    /// into `TurnRunner::run_with_filter` so daemon + any other caller
    /// gets it for free — previously each caller had to remember.
    pub(crate) fn post_process_tool_results(&mut self, tool_count: usize) {
        // Error file pre-injection: when a bash command fails, extract file paths
        // from the output and inject their content. This saves the model from
        // manually reading files mentioned in error messages (e.g., rustc errors
        // like "src/api.rs:42:5: error").
        // Language-agnostic: just find paths that exist on disk.
        if self.current_tool_name == "bash" {
            let wd = self
                .turn_runner
                .context
                .working_dir
                .try_read()
                .map(|g| g.clone())
                .unwrap_or_default();

            // Only for failed bash results
            let len = self.conversation.messages.len();
            let start = len.saturating_sub(tool_count);
            let mut files_to_inject: Vec<(String, String)> = Vec::new();

            for i in start..len {
                if let MessageContent::ToolResult(ref r) = self.conversation.messages[i].content {
                    if !r.success {
                        // Extract file paths from error output
                        let paths = extract_file_paths(&r.output, &wd);
                        for path in paths.into_iter().take(3) {
                            // Skip files already read this turn
                            let short = path
                                .strip_prefix(&wd)
                                .map(|p| p.to_string_lossy().to_string())
                                .unwrap_or_else(|_| path.to_string_lossy().to_string());
                            if self
                                .files_read_this_turn
                                .iter()
                                .any(|f| f.contains(&short) || short.contains(f))
                            {
                                continue;
                            }
                            if let Ok(content) = std::fs::read_to_string(&path) {
                                let lines: Vec<&str> = content.lines().collect();
                                let preview = if lines.len() > 50 {
                                    format!(
                                        "{}\n[... {} more lines]",
                                        lines[..50].join("\n"),
                                        lines.len() - 50
                                    )
                                } else {
                                    content.clone()
                                };
                                files_to_inject.push((short, preview));
                            }
                        }
                    }
                }
            }

            if !files_to_inject.is_empty() {
                let injection = files_to_inject
                    .iter()
                    .map(|(path, content)| format!("[Auto-read from error: {}]\n{}", path, content))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                self.conversation.add_user_message(&injection);
            }
        }
    }
}

/// Extract file paths from error output. Language-agnostic: finds tokens
/// that look like file paths (contain `/` or `\`, have a code extension)
/// and checks if they exist on disk.
fn extract_file_paths(output: &str, working_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let code_extensions = [
        "rs", "py", "js", "ts", "tsx", "jsx", "java", "go", "c", "cpp", "h", "vue", "svelte",
        "html", "css", "scss", "toml", "yaml", "yml", "json",
    ];

    for line in output.lines() {
        // Split on common delimiters to find path-like tokens
        for token in line.split(&[' ', ':', '"', '\'', '(', ')', ',', '='][..]) {
            let token = token.trim();
            if token.len() < 4 || token.len() > 300 {
                continue;
            }
            if !token.contains('/') && !token.contains('\\') {
                continue;
            }

            // Strip trailing :line:col (e.g., "src/main.rs:42:5" → "src/main.rs")
            let path_str = token.split(':').next().unwrap_or(token);

            // Check extension
            let has_code_ext = code_extensions
                .iter()
                .any(|ext| path_str.ends_with(&format!(".{}", ext)));
            if !has_code_ext {
                continue;
            }

            // Try as absolute path first, then relative to working dir
            let path = std::path::Path::new(path_str);
            let full_path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                working_dir.join(path_str)
            };

            if full_path.is_file() && !seen.contains(&full_path) {
                seen.insert(full_path.clone());
                paths.push(full_path);
            }
        }
    }
    paths
}
