use super::*;

impl AgentLoop {
    /// Graph-driven preread: identify call chain files from user message,
    /// read their key functions, and inject into system prompt.
    ///
    /// Build a per-turn reminder string injected into the conversation.
    /// Currently returns empty — reserved for future per-turn context injection.
    pub(crate) fn build_turn_reminder(&self) -> String {
        let mut parts = Vec::new();

        // Previous turn's edited files — helps model avoid re-exploring
        if !self.prev_turn_edited_files.is_empty() {
            let files = self.prev_turn_edited_files.join(", ");
            parts.push(format!(
                "[Previous turn: you edited {}. If the user reports the same issue, start from these files.]",
                files
            ));
        }

        // Current task at the very end (recency bias)
        if !self.current_task.is_empty() {
            let task_short = if self.current_task.chars().count() > 300 {
                format!(
                    "{}...",
                    self.current_task.chars().take(297).collect::<String>()
                )
            } else {
                self.current_task.clone()
            };
            parts.push(format!(
                "=== CURRENT TASK ===\n{}\nAct on this task directly. Do NOT search for files you already know about.",
                task_short
            ));
        }

        if parts.is_empty() {
            String::new()
        } else {
            parts.join("\n\n")
        }
    }

    pub(crate) fn build_system_prompt(&mut self) -> String {
        // Dynamic rules: select prompt sections based on task type.
        // If user has a custom system_prompt in config, use that instead (override).
        let rules = if let Some(custom) = self
            .config
            .providers
            .get(&self.config.default_provider)
            .and_then(|p| p.system_prompt.as_deref())
        {
            custom.to_string()
        } else {
            crate::config::prompt_sections::build_rules().to_string()
        };

        let wd: PathBuf = self
            .turn_runner
            .context
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

        // Stable environment metadata (no date — changes every day, breaks cache)
        let shell = if cfg!(target_os = "windows") {
            std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())
        } else {
            std::env::var("SHELL").unwrap_or_else(|_| "bash".into())
        };
        let env_info = format!("Platform: {} | Shell: {}", std::env::consts::OS, shell,);

        // Model identity — needed for language discipline.
        let model_id = self
            .config
            .providers
            .get(&self.config.default_provider)
            .map(|p| p.model.to_lowercase())
            .unwrap_or_default();

        // Identity: inject model name so the model correctly identifies itself.
        let model_display = self
            .config
            .providers
            .get(&self.config.default_provider)
            .map(|p| p.model.as_str())
            .unwrap_or("unknown");

        // Assemble prompt: identity + env → rules LAST (recency effect).
        let mut prompt = format!(
            "You are AtomCode. When asked who you are, say you are AtomCode (an AI coding agent by AtomGit) running the {} model. Never claim to be another product.\n\
             Working directory: {wd}\nAll file paths in tool calls must be absolute, resolved under {wd}. Verify file existence before editing.\n{env_info}\n",
            model_display, wd = wd.display(), env_info = env_info,
        );

        // Project instructions (if any)
        if !project_instructions.is_empty() {
            prompt.push_str(&format!(
                "\n=== PROJECT INSTRUCTIONS (.atomcode.md) ===\n{}\n",
                project_instructions
            ));
        }

        // Plan mode: inject planning-only instructions before rules.
        if self.plan_mode {
            prompt.push_str(
                "\n=== PLAN MODE (ACTIVE) ===\n\
                 You are in PLAN MODE. You can explore, read files, run commands, and analyze the codebase.\n\
                 You MUST NOT edit, create, or delete any files. Only read-only tools are available.\n\
                 Your job is to:\n\
                 1. Analyze the codebase and understand the current state\n\
                 2. Create a detailed implementation plan with specific files, functions, and changes\n\
                 3. Present the plan clearly so the user can review before executing\n\
                 Do NOT attempt to make any changes. Focus on analysis and planning only.\n\n"
            );
        }

        // RULES GO LAST — recency effect ensures the model remembers these
        // when it starts generating tool calls.
        prompt.push_str(&format!(
            "\n=== RULES (follow these strictly) ===\n{rules}\n"
        ));

        // Language discipline: some models (MiniMax, Qwen, DeepSeek) default to
        // English chain-of-thought even when the user speaks Chinese.
        let needs_cn_lock = model_id.contains("minimax")
            || model_id.contains("qwen")
            || model_id.contains("deepseek")
            || model_id.contains("kimi");
        if needs_cn_lock {
            prompt.push_str("\n用户可见的输出请用中文。工具调用和代码保持原样。\n");
        }

        // Platform-specific rules — only injected on the target OS.
        let platform = crate::config::platform_rules();
        if !platform.is_empty() {
            prompt.push_str(platform);
            prompt.push('\n');
        }

        // MiniMax thinking discipline — inline as a rule, not a <system-reminder>
        if model_id.contains("minimax") {
            prompt.push_str(
                "\n## THINKING 纪律:\n\
                 内部思考（<think> 块）必须极简，只写必要的决策线索，\
                 不要复述工具结果、不要分点展开、不要自问自答。目标 ≤ 3 句话。\n",
            );
        }

        prompt
    }
}
