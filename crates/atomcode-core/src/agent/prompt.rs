use super::*;

impl AgentLoop {
    // NOTE: `build_turn_reminder` lived here but moved to
    // `ctx::CtxBuilder::render_turn_reminder` (default impl in
    // `ctx::render::render_turn_reminder`). The agent is the only
    // consumer — having it on the ctx trait lets per-model impls
    // override placement/content without touching agent code.
    // AgentLoop owns the inputs (`prev_turn_edited_files`,
    // `current_task`); the ctx owns the rendering policy.

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

        // Stable environment metadata (no date — changes every day, breaks cache)
        let shell = if cfg!(target_os = "windows") {
            std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())
        } else {
            std::env::var("SHELL").unwrap_or_else(|_| "bash".into())
        };
        let env_info = format!(
            "Platform: {} | Shell: {}",
            std::env::consts::OS, shell,
        );

        // Identity: inject model name so the model correctly identifies itself.
        let model_display = self.config.providers
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
        prompt.push_str(&format!("\n=== RULES (follow these strictly) ===\n{rules}\n"));

        // Platform-specific rules — only injected on the target OS.
        let platform = crate::config::platform_rules();
        if !platform.is_empty() {
            prompt.push_str(platform);
            prompt.push('\n');
        }

        // NOTE: model-specific directives (CJK language lock for MiniMax/
        // Qwen/DeepSeek/Kimi, MiniMax thinking discipline) were here but
        // moved to `ctx::render::apply_model_directives`, invoked by each
        // CtxBuilder impl in `build_messages`. Keeping them out of this
        // function keeps agent::prompt free of `if model_id.contains(...)`
        // branches — per-model customization now lives in ctx.

        prompt
    }

}
