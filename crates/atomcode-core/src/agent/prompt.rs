use super::*;

impl AgentLoop {
    /// DO NOT pre-read source files. Claude Code doesn't do this either.
    /// Pre-reading stuffs the system prompt with 50K+ tokens of irrelevant code,
    /// diluting model attention and making it WORSE at following rules.
    ///
    /// Instead: give the model a good file tree (project_context) and let it
    /// read_file what it needs. Each read is 1 step — trivial cost.
    /// A compact system prompt → model follows rules → fewer mistakes → fewer steps.
    pub(crate) async fn build_preread_context(&self, _content: &str) -> String {
        String::new()
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

        // Active services detected via lsof + extracted from tool outputs.
        if !self.active_services.is_empty() {
            prompt.push_str("Running services (live):\n");
            let mut has_node = false;
            for (label, url) in &self.active_services {
                prompt.push_str(&format!("  {} — {}\n", url, label));
                if label.contains("node") {
                    has_node = true;
                }
            }
            if has_node {
                prompt.push_str("(Node dev server detected — auto-reloads on save, no build needed.)\n");
            }
        }

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

        // Available skills (descriptions only — full content loaded lazily via use_skill tool)
        if let Ok(reg) = self.skill_registry.read() {
            let mut skill_lines: Vec<String> = reg.invocable_by_llm()
                .map(|s| format!("  - {}: {}", s.name, s.description))
                .collect();
            if !skill_lines.is_empty() {
                skill_lines.sort();
                prompt.push_str("\n=== AVAILABLE SKILLS ===\n");
                prompt.push_str("Use the `use_skill` tool to load a skill's full instructions when the task matches.\n");
                prompt.push_str(&skill_lines.join("\n"));
                prompt.push('\n');
            }
        }

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
