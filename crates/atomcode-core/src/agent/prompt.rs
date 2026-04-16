use super::*;

impl AgentLoop {
    /// Graph-driven preread: identify call chain files from user message,
    /// read their key functions, and inject into system prompt.
    ///
    /// Unlike the old "preread everything" approach (disabled for bloating context),
    /// this only reads files ON THE CALL CHAIN — typically 3-5 files, ~15K tok max.
    /// Budget: total preread ≤ 25% of remaining context.
    pub(crate) async fn build_preread_context(&mut self, _content: &str) -> String {
        String::new()
    }

    /// Build the stable system prompt. This should NOT change between turns
    /// within the same session, enabling prompt caching across all providers.
    ///
    /// Dynamic per-turn content (git status, recent activity, current task, etc.)
    /// is built separately by `build_turn_reminder()` and injected into the
    /// last user message as a <system-reminder>.
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
                // Pass graph for cross-file call annotations in file tree
                let graph_ref = self.turn_runner.context.graph.try_read()
                    .ok();
                let pc = if let Some(ref g) = graph_ref {
                    crate::project_context::build_project_context_with_graph(&wd, Some(g))
                } else {
                    crate::project_context::build_project_context(&wd)
                };
                self.project_context_cache = Some((wd.clone(), pc.text.clone()));
                self.context_included_files = pc.included_files;
                pc.text
            }
        };

        // Load project-level instructions (.atomcode.md or ATOMCODE.md)
        let project_instructions = [".atomcode.md", "ATOMCODE.md"]
            .iter()
            .find_map(|name| {
                let path = wd.join(name);
                std::fs::read_to_string(&path).ok()
            })
            .unwrap_or_default();

        // Stable environment metadata (no date — model can run `date` if needed)
        let shell = if cfg!(target_os = "windows") {
            std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())
        } else {
            std::env::var("SHELL").unwrap_or_else(|_| "bash".into())
        };
        let env_info = format!(
            "Platform: {} | Shell: {}",
            std::env::consts::OS, shell,
        );

        // Assemble stable prompt: working dir + env + project context + instructions + rules.
        let mut prompt = format!(
            "Working directory: {wd}\nALL file paths MUST start with {wd}. NEVER use paths from previous sessions.\n{env_info}\n",
            wd = wd.display(), env_info = env_info,
        );

        prompt.push_str(&format!(
            "\n=== PROJECT STRUCTURE ===\n{project_ctx}\n"
        ));

        // Pre-read files (bulk content — middle of prompt)
        if !self.preread_context.is_empty() {
            prompt.push_str(&format!("\n\n{}", self.preread_context));
        }

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

        // RULES GO LAST — recency effect ensures the model remembers these
        // when it starts generating tool calls.
        prompt.push_str(&format!("\n=== RULES (follow these strictly) ===\n{rules}\n"));

        // Language discipline: some models (MiniMax, Qwen, DeepSeek) default to
        // English chain-of-thought even when the user speaks Chinese.
        let model_id = self.config.providers
            .get(&self.config.default_provider)
            .map(|p| p.model.to_lowercase())
            .unwrap_or_default();
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

        // MiniMax thinking discipline
        if model_id.contains("minimax") {
            prompt.push_str(
                "\n<system-reminder>\n\
                 THINKING 简洁纪律：内部思考（<think> 块）必须极简，\
                 只写必要的决策线索，不要复述工具结果、不要分点展开、不要自问自答。\
                 目标 ≤ 3 句话。冗长 thinking 视为严重问题。\n\
                 </system-reminder>\n"
            );
        }

        prompt
    }

    /// Build per-turn dynamic context as a <system-reminder> block.
    /// This is injected into the last user message before sending to the LLM,
    /// keeping the system prompt stable for caching.
    ///
    /// Currently empty — current_task is already in the user message,
    /// and other dynamic content (git status, date, etc.) was removed.
    /// Reserved for future per-turn context injection needs.
    pub(crate) fn build_turn_reminder(&self) -> String {
        String::new()
    }
}
