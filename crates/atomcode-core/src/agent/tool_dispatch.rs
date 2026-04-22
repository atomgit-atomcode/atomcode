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
            TurnEvent::ToolCallStreaming { name, hint } => {
                let _ = self.event_tx.send(AgentEvent::ToolCallStreaming { name, hint });
            }
            TurnEvent::ToolCallStarted { ref id, ref name, ref arguments } => {
                self.datalog.log_tool_call(name, arguments);

                self.current_tool_name = name.clone();
                self.phase = AgentPhase::CallingTool(name.clone());
                let _ = self.event_tx.send(AgentEvent::PhaseChange(self.phase.clone()));

                // Track bash command for failure categorization
                if name == "bash" {
                    if let Ok(args) = serde_json::from_str::<serde_json::Value>(arguments) {
                        self.discipline_state.last_bash_cmd = args.get("command")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                    }
                }

                // Track files for Working Set
                if matches!(name.as_str(), "read_file" | "edit_file" | "create_file" | "search_replace" | "glob" | "grep") {
                    if let Ok(args) = serde_json::from_str::<serde_json::Value>(arguments) {
                        let fp = args.get("file_path").and_then(|v| v.as_str())
                            .or_else(|| args.get("path").and_then(|v| v.as_str()));
                        if let Some(fp) = fp {
                            let short = std::path::Path::new(fp)
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_else(|| fp.to_string());
                            self.session_files.insert(short, std::path::PathBuf::from(fp));
                        }
                    }
                }

                let _ = self.event_tx.send(AgentEvent::ToolCallStarted { id: id.clone(), name: name.clone(), arguments: arguments.clone() });
            }
            TurnEvent::ToolCallResult { call_id, name, output, success, duration } => {
                // Track files for discipline
                if let Some(pos) = output.find("Edited ") {
                    // Extract full path from "Edited /path/to/file ..." or "Edited /path/to/file\n..."
                    let rest = &output[pos + 7..];
                    let full_path_end = rest.find(|c: char| c == ' ' || c == '\n' || c == '(').unwrap_or(rest.len());
                    let full_path_str = rest[..full_path_end].trim();
                    if !full_path_str.is_empty() {
                        self.active_file = Some(PathBuf::from(full_path_str));
                    }
                    if let Some(end) = rest.find(|c: char| c == '\n' || c == '.') {
                        let file = short_path(&rest[..end]);
                        if !self.files_edited_this_turn.contains(&file) {
                            self.files_edited_this_turn.push(file);
                        }
                    }
                }
                if let Some(pos) = output.find("Wrote ").or_else(|| output.find("Overwrote ")) {
                    let keyword_len = if output[pos..].starts_with("Overwrote ") { 10 } else { 6 };
                    let rest = &output[pos + keyword_len..];
                    let full_path_end = rest.find(|c: char| c == ' ' || c == '\n' || c == '(').unwrap_or(rest.len());
                    let full_path_str = rest[..full_path_end].trim();
                    if !full_path_str.is_empty() {
                        self.active_file = Some(PathBuf::from(full_path_str));
                    }
                    if let Some(end) = rest.find(|c: char| c == '\n' || c == ' ') {
                        let file = short_path(&rest[..end]);
                        if !self.files_edited_this_turn.contains(&file) {
                            self.files_edited_this_turn.push(file);
                        }
                    }
                }
                if matches!(name.as_str(), "read_file" | "list_directory" | "glob" | "grep") {
                    self.discipline_state.consecutive_reads += 1;
                } else if matches!(name.as_str(), "edit_file" | "create_file") {
                    self.discipline_state.consecutive_reads = 0;
                }

                // Track scouting commands for datalog metrics (no injection).
                if name == "bash" {
                    let cmd = self.discipline_state.last_bash_cmd.to_lowercase();
                    if cmd.contains("curl") || cmd.contains("lsof")
                        || cmd.contains("ps aux") || cmd.contains("tail") {
                        self.discipline_state.scouting_count += 1;
                    }
                } else if matches!(name.as_str(), "read_file" | "edit_file" | "create_file") {
                    self.discipline_state.scouting_count = 0;
                }

                self.datalog.log_tool_result(&output, success);
                let _ = self.event_tx.send(AgentEvent::ToolCallResult {
                    call_id, name, output, success, duration,
                });
            }
            TurnEvent::TokenUsage { prompt_tokens, completion_tokens, total_tokens: _, cached_tokens } => {
                if cached_tokens > 0 {
                    self.datalog.log_cache_hit(prompt_tokens, cached_tokens);
                }
                let _ = self.event_tx.send(AgentEvent::TokenUsage(
                    crate::stream::TokenUsage {
                        prompt_tokens,
                        completion_tokens,
                        cached_tokens,
                    }
                ));
            }
            TurnEvent::ContextStats { system_tokens, sent_tokens, dropped_tokens, working_set_tokens, total_messages } => {
                self.datalog.log_context_stats(system_tokens, sent_tokens, dropped_tokens, working_set_tokens, total_messages);
                // Narrow stats — rich breakdown comes from handle_send_message.
                let _ = self.event_tx.send(AgentEvent::ContextStats {
                    system_tokens, sent_tokens, dropped_tokens, working_set_tokens, total_messages,
                    tool_defs_tokens: 0,
                    cold_zone_tokens: 0,
                    ctx_window: 0,
                    ctx_name: String::new(),
                    system_prompt: String::new(),
                });
            }
            TurnEvent::Error(e) => {
                let _ = self.event_tx.send(AgentEvent::Error(e));
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
        }
    }

    /// Post-process tool results added by TurnRunner: truncate large outputs,
    /// then extract file paths from error output and pre-read them.
    pub(crate) fn post_process_tool_results(&mut self, tool_count: usize) {
        // Use ctx's effective window (may be clamped below config's raw
        // value for small-window strategies). Tool truncation must honor
        // the same budget build_messages uses, not the raw config.
        let context_window = self.ctx.ctx_window();
        crate::ctx::truncate::post_process_tool_results(
            &mut self.conversation.messages,
            tool_count,
            &self.current_tool_name,
            context_window,
        );

        // Error file pre-injection: when a bash command fails, extract file paths
        // from the output and inject their content. This saves the model from
        // manually reading files mentioned in error messages (e.g., rustc errors
        // like "src/api.rs:42:5: error").
        // Language-agnostic: just find paths that exist on disk.
        if self.current_tool_name == "bash" {
            let wd = self.turn_runner.context.working_dir
                .try_read().map(|g| g.clone()).unwrap_or_default();

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
                            let short = path.strip_prefix(&wd)
                                .map(|p| p.to_string_lossy().to_string())
                                .unwrap_or_else(|_| path.to_string_lossy().to_string());
                            if self.files_read_this_turn.iter().any(|f| f.contains(&short) || short.contains(f)) {
                                continue;
                            }
                            if let Ok(content) = std::fs::read_to_string(&path) {
                                let lines: Vec<&str> = content.lines().collect();
                                let preview = if lines.len() > 50 {
                                    format!("{}\n[... {} more lines]",
                                        lines[..50].join("\n"), lines.len() - 50)
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
                let injection = files_to_inject.iter()
                    .map(|(path, content)| format!("[Auto-read from error: {}]\n{}", path, content))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                self.conversation.add_user_message(&injection);
            }
        }
    }

    #[allow(dead_code)]
    pub(crate) fn resolve_args(&self, call: &ToolCall) -> String {
        let wd: PathBuf = self
            .turn_runner.context
            .working_dir
            .try_read()
            .map(|g| g.clone())
            .unwrap_or_default();

        // Try parsing JSON directly, then repair, then specialized extractors
        let args_str = &call.arguments;
        let parsed = serde_json::from_str::<serde_json::Value>(args_str)
            .or_else(|_| serde_json::from_str::<serde_json::Value>(&crate::turn::json_repair::repair_json(args_str)))
            .or_else(|_| {
                // For edit_file: specialized parser that handles unescaped source code
                if call.name == "edit_file" {
                    if let Some(v) = crate::turn::json_repair::extract_edit_file_args(args_str) {
                        return Ok(v);
                    }
                }
                Ok::<serde_json::Value, serde_json::Error>(crate::turn::json_repair::extract_json_fields(args_str))
            });

        if let Ok(mut args) = parsed {
            // Resolve relative paths for ALL path-like fields
            for field in &["file_path", "path"] {
                if let Some(fp) = args.get(*field).and_then(|v| v.as_str()) {
                    let p = std::path::Path::new(fp);
                    if !fp.is_empty() && !p.is_absolute() && fp != "." {
                        let resolved = wd.join(p);
                        args[*field] = serde_json::json!(resolved.to_string_lossy().to_string());
                    }
                }
            }

            // Glob pattern resolution is handled inside the glob tool itself.
            // Do NOT resolve here — it breaks patterns containing `**/`.

            // read_file: first read of a file → force full read (ignore offset/limit).
            // Prevents the model from doing 4-7 partial reads of a file it hasn't seen yet.
            // Subsequent re-reads are allowed to use offset/limit.
            if call.name == "read_file" {
                if let Some(fp) = args.get("file_path").and_then(|v| v.as_str()) {
                    let short = short_path(fp);
                    let read_count = self.discipline_state.file_read_counts.get(&short).copied().unwrap_or(0);
                    if read_count == 0 && (args.get("offset").is_some() || args.get("limit").is_some()) {
                        // First read — remove offset/limit to get the full file.
                        if let Some(obj) = args.as_object_mut() {
                            obj.remove("offset");
                            obj.remove("limit");
                        }
                    }
                }
            }

            return serde_json::to_string(&args).unwrap_or(call.arguments.clone());
        }
        call.arguments.clone()
    }

    /// Intercept tool calls that are provably redundant — returns a short message
    /// instead of executing the tool. Historically this also intercepted
    /// `list_directory(".")` because the system prompt used to carry the full
    /// working-directory tree; that tree injection was removed in commit
    /// 1e9936f8 ("refactor: 移除 system prompt 中的 PROJECT STRUCTURE 段") so
    /// the `list_directory` interception below is now a no-op — the model needs
    /// to actually be able to see the top-level layout when it asks.
    /// Does NOT intercept duplicate reads (the model may re-read with different params).
    #[allow(dead_code)]
    pub(crate) fn intercept_redundant_call(&mut self, tool_name: &str, args: &str) -> Option<String> {
        // Pre-read cache hit: if a file was already pre-read, return its content
        // directly from disk (zero overhead — the file is in OS cache).
        // This is better than saying "SKIPPED" because the model needs the actual
        // content to construct edit_file calls, and it can't access the system prompt
        // content mid-conversation with limited context windows.
        // We still avoid counting this as a "real" read for budget purposes.

        // Hard block: 5+ reads of the same file → stop executing, force the model to act.
        // Prevents infinite read loops when context is near-full and model can't generate edits.
        if tool_name == "read_file" {
            if let Ok(args_val) = serde_json::from_str::<serde_json::Value>(args) {
                if let Some(fp) = args_val.get("file_path").and_then(|v| v.as_str()) {
                    let short = std::path::Path::new(fp)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| fp.to_string());
                    let count = self.discipline_state.file_read_counts.get(&short).copied().unwrap_or(0);
                    if count >= 5 {
                        return Some(format!(
                            "BLOCKED: You have read {} {} times. You already have the content. \
                             Use edit_file to make changes NOW, or explain to the user what's wrong.",
                            short, count
                        ));
                    }
                }
            }
        }

        // Loop detection: if the same (tool, args) appears 3+ times in recent calls, block it.
        // EXCEPTION: if there was an edit_file/write_file between repeats, the model is
        // retrying after a fix — that's legitimate, not a loop. Reset the counter on edits.
        // For read_file: hash only file_path (ignore offset/limit) to catch
        // re-reading the same file with different pagination params.
        let args_hash = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            if tool_name == "read_file" {
                // Only hash file_path — different offset/limit is still the same file
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(args) {
                    if let Some(fp) = v.get("file_path").and_then(|v| v.as_str()) {
                        fp.hash(&mut h);
                    } else {
                        args.hash(&mut h);
                    }
                } else {
                    args.hash(&mut h);
                }
            } else {
                args.hash(&mut h);
            }
            h.finish()
        };
        let sig = (tool_name.to_string(), args_hash);
        self.discipline_state.recent_calls.push(sig.clone());
        if self.discipline_state.recent_calls.len() > 20 {
            self.discipline_state.recent_calls.remove(0);
        }

        // Count consecutive repeats of this exact call.
        // For read_file: only reset if the SAME file was edited between reads.
        // Editing app.js should not reset the read count for app.py.
        let mut repeat_count = 0usize;
        for entry in self.discipline_state.recent_calls.iter().rev() {
            if tool_name == "read_file" {
                // For read_file: only an edit on the same file resets the counter
                if matches!(entry.0.as_str(), "edit_file" | "write_file" | "create_file")
                    && entry.1 == args_hash
                {
                    repeat_count += 1;
                    break;
                }
            } else {
                // For other tools: any edit resets the counter (original behavior)
                if matches!(entry.0.as_str(), "edit_file" | "write_file" | "create_file") {
                    repeat_count += 1;
                    break;
                }
            }
            if *entry == sig {
                repeat_count += 1;
            }
        }

        if repeat_count >= 3 {
            return Some(format!(
                "[BLOCKED: You have called {} with the same arguments {} times without making changes. \
                 This is a loop. STOP. If the command failed, the error message tells you why — \
                 fix the underlying issue instead of retrying. \
                 If you cannot fix it, summarize what you completed and what failed for the user.]",
                tool_name, repeat_count
            ));
        }

        // ── Same-file multi-edit detection ──
        // Track consecutive edits to the same file. On the 3rd+ edit without
        // reading the file in between, block and force a re-read.
        if tool_name == "edit_file" || tool_name == "create_file" {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(args) {
                if let Some(fp) = parsed.get("file_path").and_then(|v| v.as_str()) {
                    let short = short_path(fp);
                    if self.discipline_state.consecutive_edits_file.as_deref() == Some(&short) {
                        self.discipline_state.consecutive_edits_count += 1;
                    } else {
                        self.discipline_state.consecutive_edits_file = Some(short.clone());
                        self.discipline_state.consecutive_edits_count = 1;
                    }
                    // Auto-refresh and edit blocking: REMOVED.
                    // Auto-refresh is redundant with view replacement.
                    // View replacement already updates stale reads in context.
                }
            }
        } else if tool_name == "read_file" {
            // Only reading a DIFFERENT file resets the counter.
            // Reading the SAME file you just edited (to check your work) should NOT
            // reset — otherwise edit→read_same→edit→read_same bypasses the limit.
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(args) {
                let read_file = parsed.get("file_path")
                    .and_then(|v| v.as_str())
                    .map(|fp| {
                        fp.rsplit('/').next().unwrap_or(fp).to_string()
                    });
                let is_same_file = match (&read_file, &self.discipline_state.consecutive_edits_file) {
                    (Some(rf), Some(ef)) => rf == ef,
                    _ => false,
                };
                if !is_same_file {
                    self.discipline_state.consecutive_edits_file = None;
                    self.discipline_state.consecutive_edits_count = 0;
                }
            }
        }

        // `list_directory` used to be intercepted here (see doc comment above).
        // After 1e9936f8 removed the prompt-level tree injection, blocking the
        // model's `list_directory(".")` call is actively harmful — it reports
        // "tree already in your system prompt" (false) and forces the model to
        // guess sibling paths. Let every tool through.
        let _ = (tool_name, args);
        None
    }

}

/// Normalize a bash command for repeated-execution detection.
/// Strips: env var prefixes (FOO=bar), stderr redirects (2>&1, 2>/dev/null),
/// sleep prefixes (sleep N &&), and leading/trailing whitespace.
/// Returns a stable key so that semantically identical commands match.
#[allow(dead_code)]
pub(crate) fn normalize_bash_cmd(cmd: &str) -> String {
    let mut s = cmd.trim().to_string();

    // Strip leading "sleep N && " or "sleep N; " — these are just wait wrappers.
    while let Some(rest) = s.strip_prefix("sleep ") {
        // Find the next "&&" or ";"
        if let Some(pos) = rest.find("&&") {
            s = rest[pos + 2..].trim().to_string();
        } else if let Some(pos) = rest.find(';') {
            s = rest[pos + 1..].trim().to_string();
        } else {
            break; // bare "sleep N" — keep as-is
        }
    }

    // Strip leading env var assignments (KEY=VALUE ...)
    let words: Vec<&str> = s.split_whitespace().collect();
    let start = words.iter().position(|w| !w.contains('=')).unwrap_or(0);
    s = words[start..].join(" ");

    // Strip stderr redirects
    s = s.replace("2>&1", "").replace("2>/dev/null", "");

    // Collapse whitespace
    s.split_whitespace().collect::<Vec<&str>>().join(" ")
}

/// Extract file paths from error output. Language-agnostic: finds tokens
/// that look like file paths (contain `/` or `\`, have a code extension)
/// and checks if they exist on disk.
fn extract_file_paths(output: &str, working_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let code_extensions = [
        "rs", "py", "js", "ts", "tsx", "jsx", "java", "go", "c", "cpp", "h",
        "vue", "svelte", "html", "css", "scss", "toml", "yaml", "yml", "json",
    ];

    for line in output.lines() {
        // Split on common delimiters to find path-like tokens
        for token in line.split(&[' ', ':', '"', '\'', '(', ')', ',', '='][..]) {
            let token = token.trim();
            if token.len() < 4 || token.len() > 300 { continue; }
            if !token.contains('/') && !token.contains('\\') { continue; }

            // Strip trailing :line:col (e.g., "src/main.rs:42:5" → "src/main.rs")
            let path_str = token.split(':').next().unwrap_or(token);

            // Check extension
            let has_code_ext = code_extensions.iter()
                .any(|ext| path_str.ends_with(&format!(".{}", ext)));
            if !has_code_ext { continue; }

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
