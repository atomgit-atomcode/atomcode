use super::*;

impl AgentLoop {
    /// Graph-driven preread: identify call chain files from user message,
    /// read their key functions, and inject into system prompt.
    ///
    /// Build a per-turn reminder string injected into the conversation.
    /// Currently returns empty — reserved for future per-turn context injection.
    pub(crate) fn build_turn_reminder(&self) -> String {
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

        // Model identity — needed both for identity injection and language discipline.
        let model_id = self.config.providers
            .get(&self.config.default_provider)
            .map(|p| p.model.to_lowercase())
            .unwrap_or_default();

        // Assemble prompt: env → rules LAST (recency effect).
        let mut prompt = format!(
            "Working directory: {wd}\nAll file paths in tool calls must be absolute, resolved under {wd}. Verify file existence before editing.\n{env_info}\n",
            wd = wd.display(), env_info = env_info,
        );

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

        // Language discipline: some models (MiniMax, Qwen, DeepSeek) default to
        // English chain-of-thought even when the user speaks Chinese.
        let needs_cn_lock = model_id.contains("minimax")
            || model_id.contains("qwen")
            || model_id.contains("deepseek")
            || model_id.contains("kimi");
        if needs_cn_lock {
            prompt.push_str(
                "\n用户可见的输出请用中文。工具调用和代码保持原样。\n"
            );
        }

        // Platform-specific rules — only injected on the target OS.
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

        // MiniMax thinking discipline — inline as a rule, not a <system-reminder>
        if model_id.contains("minimax") {
            prompt.push_str(
                "\n## THINKING 纪律:\n\
                 内部思考（<think> 块）必须极简，只写必要的决策线索，\
                 不要复述工具结果、不要分点展开、不要自问自答。目标 ≤ 3 句话。\n"
            );
        }

        // Inject current task at the very end (recency bias).
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
