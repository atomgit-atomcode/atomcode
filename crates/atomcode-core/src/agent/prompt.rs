use super::*;

impl AgentLoop {
    /// Graph-driven preread: identify call chain files from user message,
    /// read their key functions, and inject into system prompt.
    ///
    /// Unlike the old "preread everything" approach (disabled for bloating context),
    /// this only reads files ON THE CALL CHAIN — typically 3-5 files, ~15K tok max.
    /// Budget: total preread ≤ 25% of remaining context.
    pub(crate) async fn build_preread_context(&mut self, content: &str) -> String {
        let graph = self.turn_runner.context.graph.read().await;
        if !graph.is_ready() {
            return String::new();
        }

        // Collect files on the call chain using the same strategies as auto_inject_graph_context
        let mut chain_files: Vec<PathBuf> = Vec::new();
        let mut seen_files = std::collections::HashSet::new();

        // Strategy 1: file names in user message
        let file_re = regex::Regex::new(r"\b(\w+\.(?:rs|py|js|ts|tsx|java|go|vue|c|cpp))\b")
            .unwrap_or_else(|_| regex::Regex::new(".^").unwrap());
        for cap in file_re.captures_iter(content) {
            let filename = &cap[1];
            // Find full path in graph
            for (path, _) in &graph.file_symbols {
                if path.file_name().map(|f| f.to_string_lossy() == filename).unwrap_or(false) {
                    if seen_files.insert(path.clone()) {
                        chain_files.push(path.clone());
                        // Also add files it calls
                        let deps = graph.file_dependents(path, 1);
                        for dep in deps {
                            if seen_files.insert(dep.clone()) {
                                chain_files.push(dep);
                            }
                        }
                    }
                    break;
                }
            }
        }

        // Strategy 2: function names → trace callees → collect files
        let fn_re = regex::Regex::new(r"\b([a-z_][a-z0-9_]{3,})\b")
            .unwrap_or_else(|_| regex::Regex::new(".^").unwrap());
        for cap in fn_re.captures_iter(content) {
            let name = &cap[1];
            let symbols = graph.find_by_name(name);
            for sym in symbols.iter()
                .filter(|s| matches!(s.kind, crate::graph::SymbolKind::Function | crate::graph::SymbolKind::Method))
                .take(1)
            {
                if seen_files.insert(sym.file.clone()) {
                    chain_files.push(sym.file.clone());
                }
                // Add callee files
                let callees = graph.trace_callees(sym.id, 2);
                for (callee_id, _) in &callees {
                    if let Some(node) = graph.node(*callee_id) {
                        if seen_files.insert(node.file.clone()) {
                            chain_files.push(node.file.clone());
                        }
                    }
                }
            }
        }

        // Strategy 3: previous turn edited files → include them and their callees
        for prev_file in &self.prev_turn_edited_files {
            for (path, _) in &graph.file_symbols {
                if path.file_name().map(|f| f.to_string_lossy().contains(prev_file)).unwrap_or(false) {
                    if seen_files.insert(path.clone()) {
                        chain_files.push(path.clone());
                    }
                    break;
                }
            }
        }

        drop(graph);

        if chain_files.is_empty() {
            return String::new();
        }

        // Budget: preread ≤ 25% of context window
        let ctx_window = self.config.providers
            .get(&self.config.default_provider)
            .map(|p| p.context_window)
            .unwrap_or(16000);
        let max_preread_tokens = ctx_window / 4;

        // Read files, use skeleton for large ones, full for small ones
        let mut preread = String::from("=== RELEVANT CODE (auto-detected from call chain) ===\n");
        let mut total_tokens = 0usize;
        let mut preread_paths: Vec<PathBuf> = Vec::new();

        // Sort: smaller files first (more likely to fit)
        let mut files_with_size: Vec<(PathBuf, usize)> = chain_files.iter()
            .filter_map(|p| {
                std::fs::read_to_string(p).ok().map(|c| (p.clone(), c.lines().count()))
            })
            .collect();
        files_with_size.sort_by_key(|(_, lines)| *lines);

        let per_file_budget = max_preread_tokens / files_with_size.len().max(1);

        for (path, line_count) in &files_with_size {
            let file_tokens = line_count * 12;
            if total_tokens + file_tokens.min(per_file_budget) > max_preread_tokens {
                break;
            }

            let fname = path.file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_default();

            if let Ok(content) = std::fs::read_to_string(path) {
                if file_tokens <= per_file_budget {
                    // Full content
                    preread.push_str(&format!("\n--- {} ({} lines) ---\n", fname, line_count));
                    for (i, line) in content.lines().enumerate() {
                        preread.push_str(&format!("{:>4}| {}\n", i + 1, line));
                    }
                    total_tokens += file_tokens;
                } else {
                    // Skeleton with key function expansion
                    let mut searcher = self.turn_runner.context.semantic.lock().await;
                    if let Some(symbols) = searcher.list_symbols(path) {
                        preread.push_str(&format!("\n--- {} ({} lines, skeleton) ---\n", fname, line_count));
                        let lines: Vec<&str> = content.lines().collect();

                        // Score and expand top 2 functions
                        let interest = ["handle", "process", "route", "search", "query",
                            "fetch", "execute", "dispatch", "run", "parse"];
                        let mut scored: Vec<_> = symbols.iter()
                            .map(|s| {
                                let kw = if interest.iter().any(|k| s.name.to_lowercase().contains(k)) { 100 } else { 0 };
                                (kw + (s.end_line - s.start_line), s)
                            })
                            .collect();
                        scored.sort_by(|a, b| b.0.cmp(&a.0));
                        let expand: Vec<_> = scored.iter().take(2)
                            .filter(|(_, s)| (s.end_line - s.start_line) <= 50)
                            .map(|(_, s)| (s.start_line, s.end_line))
                            .collect();

                        for s in &symbols {
                            let should_expand = expand.iter().any(|(sl, _)| *sl == s.start_line);
                            let sig = lines.get(s.start_line.saturating_sub(1))
                                .map(|l| l.trim()).unwrap_or(&s.name);
                            if should_expand {
                                preread.push_str(&format!("{:>4}| {}  [expanded]\n", s.start_line, sig));
                                let start = s.start_line.saturating_sub(1);
                                let end = s.end_line.min(lines.len());
                                for i in (start + 1)..end {
                                    if let Some(line) = lines.get(i) {
                                        preread.push_str(&format!("{:>4}| {}\n", i + 1, line));
                                    }
                                }
                            } else {
                                preread.push_str(&format!("{:>4}| {}  (L{}-{})\n", s.start_line, sig, s.start_line, s.end_line));
                            }
                        }
                        total_tokens += per_file_budget; // approximate
                    }
                    drop(searcher);
                }
                preread_paths.push(path.clone());
            }
        }

        // Register preread files so read_file won't re-read them
        for path in &preread_paths {
            if let Ok(canonical) = std::fs::canonicalize(path) {
                self.context_included_files.insert(canonical);
            }
        }

        if preread_paths.is_empty() {
            String::new()
        } else {
            preread
        }
    }

    pub(crate) fn build_system_prompt(&mut self) -> String {
        // Dynamic rules: select prompt sections based on task type.
        // If user has a custom system_prompt in config, use that instead (override).
        let rules = if let Some(custom) = self.config.providers
            .get(&self.config.default_provider)
            .and_then(|p| p.system_prompt.as_deref())
        {
            custom.to_string()
        } else {
            crate::config::prompt_sections::build_rules().to_string()
        };

        let wd: PathBuf = self
            .turn_runner.context
            .working_dir
            .try_read()
            .map(|g| g.clone())
            .unwrap_or_default();

        // Use cached project context if working dir hasn't changed
        let project_ctx = match &self.project_context_cache {
            Some((cached_wd, cached_ctx)) if cached_wd == &wd => cached_ctx.clone(),
            _ => {
                let pc = crate::project_context::build_project_context(&wd);
                self.project_context_cache = Some((wd.clone(), pc.text.clone()));
                self.context_included_files = pc.included_files;
                pc.text
            }
        };

        // No file suggestions — let the model decide which files to read
        // based on the project structure and conversation context (like Claude Code).

        // Load project-level instructions (.atomcode.md or ATOMCODE.md)
        let project_instructions = [".atomcode.md", "ATOMCODE.md"]
            .iter()
            .find_map(|name| {
                let path = wd.join(name);
                std::fs::read_to_string(&path).ok()
            })
            .unwrap_or_default();

        // Inject environment metadata
        let shell = if cfg!(target_os = "windows") {
            std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())
        } else {
            std::env::var("SHELL").unwrap_or_else(|_| "bash".into())
        };
        let date_str = if cfg!(target_os = "windows") {
            // Windows: use PowerShell for date
            std::process::Command::new("cmd.exe")
                .args(&["/C", "echo %date%"])
                .output()
                .ok().and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| "unknown".into())
        } else {
            std::process::Command::new("date").arg("+%Y-%m-%d").output()
                .ok().and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| "unknown".into())
        };
        let env_info = format!(
            "Platform: {} | Shell: {} | Date: {}",
            std::env::consts::OS, shell, date_str,
        );

        // Git context (branch + status summary)
        let git_info = std::process::Command::new("git")
            .args(&["status", "--short", "--branch"])
            .current_dir(&wd)
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| {
                let lines: Vec<&str> = s.lines().take(10).collect();
                lines.join("\n")
            })
            .unwrap_or_default();

        // Assemble prompt: env + project context + pre-read files (bulk) → rules LAST.
        // Models attend most to the START and END of context (primacy + recency).
        // Pre-read files go in the middle (bulk reference material).
        // Rules go LAST so the model remembers them when generating tool calls.
        let mut prompt = format!(
            "Working directory: {wd}\nALL file paths MUST start with {wd}. NEVER use paths from previous sessions.\n{env_info}\n",
            wd = wd.display(), env_info = env_info,
        );

        if !git_info.is_empty() {
            prompt.push_str(&format!("Git: {}\n", git_info));
        }

        // Recent activity: extract edited file names from the most recent datalog.
        // Only file names (not content/user messages) — safe, small, factual.
        let recent_activity = super::extract_recent_activity_from_datalog(&wd);
        if !recent_activity.is_empty() {
            prompt.push_str(&format!("Recent activity: {}\n", recent_activity));
        }

        // Active services: disabled. Server commands are BLOCKED in Phase 3.5,
        // so detecting running services is unnecessary noise in the system prompt.

        prompt.push_str(&format!(
            "\n=== PROJECT STRUCTURE ===\n{project_ctx}\n"
        ));

        // Pre-read files (bulk content — middle of prompt)
        if !self.preread_context.is_empty() {
            prompt.push_str(&format!("\n\n{}", self.preread_context));
        }

        // NOTE: Active file full-content injection disabled — it consumes too much
        // context window on weak models (32K), degrading decision quality.
        // The working-set skeleton mechanism is sufficient.

        // Project instructions (if any)
        if !project_instructions.is_empty() {
            prompt.push_str(&format!(
                "\n=== PROJECT INSTRUCTIONS (.atomcode.md) ===\n{}\n",
                project_instructions
            ));
        }

        // Cross-session knowledge: db credentials, ports, startup commands, etc.
        let project_knowledge = knowledge::load_knowledge(&wd);
        if !project_knowledge.is_empty() {
            prompt.push_str(&format!("\n{}\n", project_knowledge));
        }

        // Previous session context: inject the last few completed turns' outcomes
        // so the model knows what was done before (prevents re-doing the same work).
        let prev_context = self.build_previous_session_context();
        if !prev_context.is_empty() {
            prompt.push_str(&format!(
                "\n=== PREVIOUS SESSION ===\n{}\n",
                prev_context
            ));
        }

        // Skills section: disabled until skill system is implemented.
        // Listing unavailable skills wastes context tokens.

        // RULES GO LAST — recency effect ensures the model remembers these
        // when it starts generating tool calls.
        prompt.push_str(&format!("\n=== RULES (follow these strictly) ===\n{rules}\n"));

        // Platform-specific rules — only injected on the target OS.
        // macOS/Linux get nothing extra; Windows gets cmd.exe syntax rules.
        let platform = crate::config::platform_rules();
        if !platform.is_empty() {
            prompt.push_str(platform);
            prompt.push('\n');
        }

        // Inject previous turn's edited files — helps model avoid re-exploring
        if !self.prev_turn_edited_files.is_empty() {
            let files = self.prev_turn_edited_files.join(", ");
            prompt.push_str(&format!(
                "\n[Previous turn: you edited {}. If the user reports the same issue, start from these files.]\n",
                files
            ));
        }

        // Inject current task at the very end (recency bias).
        // The model attends most to the last ~200 tokens of system prompt.
        // Putting the task here ensures it's the first thing the model "thinks about"
        // when generating Turn 1 — no more blind glob/grep before reading the task.
        if !self.current_task.is_empty() {
            let task_short = if self.current_task.chars().count() > 300 {
                format!("{}...", self.current_task.chars().take(297).collect::<String>())
            } else {
                self.current_task.clone()
            };
            prompt.push_str(&format!(
                "\n=== CURRENT TASK ===\n{}\n\
                 Act on this task directly. Do NOT search for files you already know about.\n",
                task_short
            ));
        }

        prompt
    }

    /// Build a summary of the previous session's completed turns.
    /// This gives the model context about what was already done, preventing
    /// it from re-doing work (e.g., re-fixing Java version compatibility).
    /// Only includes turns that are Completed (not the current Active turn).
    /// Capped at the last 5 turns and 1500 chars total.
    pub(crate) fn build_previous_session_context(&self) -> String {
        let turns = &self.conversation.turn_tracker.turns;
        if turns.is_empty() {
            return String::new();
        }

        // Only include Completed turns (not Active).
        let completed: Vec<_> = turns.iter()
            .filter(|t| t.status == crate::conversation::turn::TurnStatus::Completed)
            .collect();

        if completed.is_empty() {
            return String::new();
        }

        // Take the last 5 completed turns.
        let recent = &completed[completed.len().saturating_sub(5)..];
        let mut ctx = String::new();

        for turn in recent {
            let msgs = &self.conversation.messages[turn.start_idx..turn.end_idx()];

            // Extract user question.
            let user_q = msgs.first()
                .and_then(|m| m.text())
                .unwrap_or("(unknown)");
            let user_short = if user_q.chars().count() > 80 {
                format!("{}...", user_q.chars().take(77).collect::<String>())
            } else {
                user_q.to_string()
            };

            // Extract assistant outcome (last text message in turn).
            let mut outcome = String::new();
            for msg in msgs.iter().rev() {
                if let Some(text) = msg.text() {
                    if matches!(msg.role, crate::conversation::message::Role::Assistant) && !text.trim().is_empty() {
                        outcome = if text.chars().count() > 200 {
                            format!("{}...", text.chars().take(197).collect::<String>())
                        } else {
                            text.to_string()
                        };
                        break;
                    }
                }
            }

            if outcome.is_empty() {
                // Synthesize from tool results
                outcome = self.conversation.synthesize_turn_outcome(msgs);
            }

            if !outcome.is_empty() {
                ctx.push_str(&format!("- User: \"{}\"\n  Result: {}\n", user_short, outcome));
            }

            if ctx.len() > 1500 {
                // Truncate at a char boundary to avoid panic on multi-byte UTF-8.
                let mut end = 1500;
                while end > 0 && !ctx.is_char_boundary(end) {
                    end -= 1;
                }
                ctx.truncate(end);
                ctx.push_str("\n...(truncated)");
                break;
            }
        }

        ctx
    }
}
