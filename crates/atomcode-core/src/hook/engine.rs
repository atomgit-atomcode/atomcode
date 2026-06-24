// ============================================================================
// HookEngine — 统一的 Hook 引擎
//
// 替代 HookRegistry + HookExecutor 的角色。
// - 集中管理所有注册的 Hook（ScriptHook / ShellCommandHook / WebhookHook / BuiltInHook）
// - 按注册槽位分组，顺序触发
// - ShellCommandHook 实现从 JSON 配置适配老 CC 兼容 hook 到新 trait 系统
// ============================================================================

use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

use super::config::matches_tool;
use super::json_config::load_hooks_config;
use super::{
    ErrorContext, Hook, HookConfig, HookContext, HookCtx, HookEvent, HookResult,
    PreHookResult, PreToolExecutionHook, PostToolExecutionHook, PostTurnHook,
    SystemPromptHook, OnSessionStartHook, OnSessionEndHook, OnErrorHook,
    OnUserPromptSubmitHook, OnToolCallStartHook, OnModelResponseHook,
    ToolCallStartContext, ToolResultContext, TurnCompleteContext, TurnStartContext,
    UserPromptHookResult, UserPromptSubmitPayload, UserPromptSubmitOutput,
    UserPromptSubmitResult,
};

// ============================================================================
// HookEngine — 统一注册/触发引擎
// ============================================================================

pub struct HookEngine {
    // TODO(#913): 当前 HookEngine 承担注册表+执行器+加载器+查询 4 种职责。
    // 未来体量超过 1000 行后可考虑拆分为 HookRegistry（注册/查询）+
    // HookExecutor（执行）+ HookLoader（加载）三个独立 struct。
    pre_tool_hooks: Vec<Arc<dyn PreToolExecutionHook>>,
    post_tool_hooks: Vec<Arc<dyn PostToolExecutionHook>>,
    post_turn_hooks: Vec<Arc<dyn PostTurnHook>>,
    system_prompt_hooks: Vec<Arc<dyn SystemPromptHook>>,
    on_session_start_hooks: Vec<Arc<dyn OnSessionStartHook>>,
    on_session_end_hooks: Vec<Arc<dyn OnSessionEndHook>>,
    on_error_hooks: Vec<Arc<dyn OnErrorHook>>,
    on_user_prompt_submit_hooks: Vec<Arc<dyn OnUserPromptSubmitHook>>,
    // 以下槽位保留给 BuiltInHook 使用
    on_turn_start_hooks: Vec<Arc<dyn super::OnTurnStartHook>>,
    on_turn_complete_hooks: Vec<Arc<dyn super::OnTurnCompleteHook>>,
    on_tool_call_start_hooks: Vec<Arc<dyn OnToolCallStartHook>>,
    on_model_response_hooks: Vec<Arc<dyn OnModelResponseHook>>,
}

impl HookEngine {
    pub fn new() -> Self {
        Self {
            pre_tool_hooks: Vec::new(),
            post_tool_hooks: Vec::new(),
            post_turn_hooks: Vec::new(),
            system_prompt_hooks: Vec::new(),
            on_session_start_hooks: Vec::new(),
            on_session_end_hooks: Vec::new(),
            on_error_hooks: Vec::new(),
            on_user_prompt_submit_hooks: Vec::new(),
            on_turn_start_hooks: Vec::new(),
            on_turn_complete_hooks: Vec::new(),
            on_tool_call_start_hooks: Vec::new(),
            on_model_response_hooks: Vec::new(),
        }
    }

    // ── 注册方法 (12 个) ──────────────────────────────────────────

    pub fn register_pre_tool_hook(&mut self, hook: Arc<dyn PreToolExecutionHook>) {
        if hook.is_enabled() {
            self.pre_tool_hooks.push(hook);
            self.pre_tool_hooks.sort_by_key(|h| h.priority());
        }
    }

    pub fn register_post_tool_hook(&mut self, hook: Arc<dyn PostToolExecutionHook>) {
        if hook.is_enabled() {
            self.post_tool_hooks.push(hook);
            self.post_tool_hooks.sort_by_key(|h| h.priority());
        }
    }

    pub fn register_post_turn_hook(&mut self, hook: Arc<dyn PostTurnHook>) {
        if hook.is_enabled() {
            self.post_turn_hooks.push(hook);
            self.post_turn_hooks.sort_by_key(|h| h.priority());
        }
    }

    pub fn register_system_prompt_hook(&mut self, hook: Arc<dyn SystemPromptHook>) {
        if hook.is_enabled() {
            self.system_prompt_hooks.push(hook);
        }
    }

    pub fn register_on_session_start_hook(&mut self, hook: Arc<dyn OnSessionStartHook>) {
        if hook.is_enabled() {
            self.on_session_start_hooks.push(hook);
        }
    }

    pub fn register_on_session_end_hook(&mut self, hook: Arc<dyn OnSessionEndHook>) {
        if hook.is_enabled() {
            self.on_session_end_hooks.push(hook);
        }
    }

    pub fn register_on_error_hook(&mut self, hook: Arc<dyn OnErrorHook>) {
        if hook.is_enabled() {
            self.on_error_hooks.push(hook);
        }
    }

    pub fn register_on_user_prompt_submit_hook(&mut self, hook: Arc<dyn OnUserPromptSubmitHook>) {
        if hook.is_enabled() {
            self.on_user_prompt_submit_hooks.push(hook);
        }
    }

    pub fn register_on_tool_call_start_hook(&mut self, hook: Arc<dyn OnToolCallStartHook>) {
        if hook.is_enabled() {
            self.on_tool_call_start_hooks.push(hook);
        }
    }

    pub fn register_on_model_response_hook(&mut self, hook: Arc<dyn OnModelResponseHook>) {
        if hook.is_enabled() {
            self.on_model_response_hooks.push(hook);
        }
    }

    pub fn register_on_turn_start_hook(&mut self, hook: Arc<dyn super::OnTurnStartHook>) {
        if hook.is_enabled() {
            self.on_turn_start_hooks.push(hook);
        }
    }

    pub fn register_on_turn_complete_hook(&mut self, hook: Arc<dyn super::OnTurnCompleteHook>) {
        if hook.is_enabled() {
            self.on_turn_complete_hooks.push(hook);
        }
    }

    // ── 触发方法 (8 个有调用点的) ──────────────────────────────────

    /// 工具执行前触发所有 PreToolExecutionHook。
    /// 返回:
    /// - Ok((Some(json), _))   — 参数被修改
    /// - Ok((None, true))      — Hook 明确允许，可跳过审批
    /// - Ok((None, false))     — 继续原样
    /// - Err(reason)           — 被阻止
    pub async fn trigger_pre_tool_use(&self, ctx: &HookCtx) -> Result<(Option<String>, bool), String> {
        let mut modified_args: Option<String> = None;
        let mut explicitly_allowed = false;
        for hook in &self.pre_tool_hooks {
            match hook.on_pre_execute(ctx).await {
                HookResult::Ok => {}
                HookResult::ExplicitAllow => {
                    explicitly_allowed = true;
                }
                HookResult::Warning(msg) => {
                    tracing::warn!("[Hook Warning] {}: {}", hook.name(), msg);
                }
                HookResult::Denied(reason) => {
                    return Err(format!("{}: {}", hook.name(), reason));
                }
                HookResult::Modified(new_args) => {
                    tracing::info!("[Hook Modified] {} modified arguments", hook.name());
                    modified_args = Some(new_args);
                }
            }
        }
        Ok((modified_args, explicitly_allowed))
    }

    /// 工具执行后并发触发所有 PostToolExecutionHook (fire-and-forget)。
    /// PostToolUse hook 之间无顺序依赖，并发执行避免单个超时阻塞后续 hook。
    pub async fn trigger_post_tool_use(&self, ctx: &HookCtx, result_ctx: &ToolResultContext) {
        let futures: Vec<_> = self
            .post_tool_hooks
            .iter()
            .map(|hook| async move {
                let result = hook.on_post_execute(ctx, result_ctx).await;
                match result {
                    HookResult::Warning(msg) => {
                        tracing::warn!("[Hook Warning] {}: {}", hook.name(), msg)
                    }
                    HookResult::Denied(reason) => {
                        tracing::warn!("[Hook Denied] {}: {}", hook.name(), reason)
                    }
                    _ => {}
                }
            })
            .collect();
        futures::future::join_all(futures).await;
    }

    /// Turn 完成后触发所有 PostTurnHook (concurrent fire-and-forget)。
    pub async fn trigger_post_turn(&self, ctx: &HookCtx, turn_result: &str) {
        let futures: Vec<_> = self
            .post_turn_hooks
            .iter()
            .map(|hook| async move {
                match hook.on_post_turn(ctx, turn_result).await {
                    HookResult::Warning(msg) => {
                        tracing::warn!("[Hook Warning] {}: {}", hook.name(), msg)
                    }
                    HookResult::Denied(reason) => {
                        tracing::warn!("[Hook Denied] {}: {}", hook.name(), reason)
                    }
                    _ => {}
                }
            })
            .collect();
        futures::future::join_all(futures).await;
    }

    /// 会话开始时触发所有 OnSessionStartHook。
    /// `ctx` 应由调用方 (AgentLoop) 传入真实的 session_id / working_dir / model 等。
    pub async fn trigger_session_start(&self, ctx: &super::SessionContext) -> Vec<String> {
        let mut messages = Vec::new();
        for hook in &self.on_session_start_hooks {
            match hook.on_session_start(ctx).await {
                HookResult::Modified(msg) => messages.push(msg),
                _ => {}
            }
        }
        messages
    }

    /// 会话结束时所有 OnSessionEndHook (fire-and-forget)。
    pub async fn trigger_session_end(&self, ctx: &super::SessionContext) {
        for hook in &self.on_session_end_hooks {
            let _ = hook.on_session_end(ctx).await;
        }
    }

    /// 用户提交 prompt 时触发所有 OnUserPromptSubmitHook。
    pub async fn trigger_user_prompt_submit(
        &self, content: &str, session_id: &str, cwd: &str,
    ) -> UserPromptHookResult {
        let payload = UserPromptSubmitPayload {
            session_id: session_id.to_string(),
            hook_event_name: "UserPromptSubmit".to_string(),
            prompt: content.to_string(),
            cwd: cwd.to_string(),
        };

        let mut injected = String::new();
        let mut warnings = Vec::new();
        for hook in &self.on_user_prompt_submit_hooks {
            match hook.on_user_prompt_submit(&payload).await {
                UserPromptSubmitResult::Continue => {}
                UserPromptSubmitResult::Inject(s) => {
                    if !injected.is_empty() {
                        injected.push_str("\n\n");
                    }
                    injected.push_str(&s);
                }
                UserPromptSubmitResult::Block(reason) => {
                    return UserPromptHookResult::Block(reason);
                }
                UserPromptSubmitResult::Warning(msg) => {
                    warnings.push(msg);
                }
            }
        }

        if !warnings.is_empty() {
            return UserPromptHookResult::Warning(warnings.join("; "));
        }
        if injected.is_empty() {
            UserPromptHookResult::Continue
        } else {
            UserPromptHookResult::Inject(injected)
        }
    }

    /// 收集所有 SystemPromptHook 的扩展内容。
    pub async fn collect_system_prompt_extensions(&self) -> Vec<String> {
        let mut parts = Vec::new();
        for hook in &self.system_prompt_hooks {
            if let Some(ext) = hook.extend_system_prompt().await {
                parts.push(ext);
            }
        }
        parts
    }

    /// Turn 开始时触发所有 OnTurnStartHook (fire-and-forget)。
    pub async fn trigger_on_turn_start(&self, ctx: &TurnStartContext) {
        for hook in &self.on_turn_start_hooks {
            match hook.on_turn_start(ctx).await {
                HookResult::Warning(msg) => {
                    tracing::warn!("[Hook Warning] {}: {}", hook.name(), msg)
                }
                HookResult::Denied(reason) => {
                    tracing::warn!("[Hook Denied] {}: {}", hook.name(), reason)
                }
                _ => {}
            }
        }
    }

    /// 工具调用开始时触发所有 OnToolCallStartHook (fire-and-forget)。
    pub async fn trigger_on_tool_call_start(&self, ctx: &ToolCallStartContext) {
        for hook in &self.on_tool_call_start_hooks {
            match hook.on_tool_call_start(ctx).await {
                HookResult::Warning(msg) => {
                    tracing::warn!("[Hook Warning] {}: {}", hook.name(), msg)
                }
                HookResult::Denied(reason) => {
                    tracing::warn!("[Hook Denied] {}: {}", hook.name(), reason)
                }
                _ => {}
            }
        }
    }

    /// Turn 完成时触发所有 OnTurnCompleteHook (fire-and-forget)。
    pub async fn trigger_on_turn_complete(&self, ctx: &TurnCompleteContext) {
        for hook in &self.on_turn_complete_hooks {
            match hook.on_turn_complete(ctx).await {
                HookResult::Warning(msg) => {
                    tracing::warn!("[Hook Warning] {}: {}", hook.name(), msg)
                }
                HookResult::Denied(reason) => {
                    tracing::warn!("[Hook Denied] {}: {}", hook.name(), reason)
                }
                HookResult::Modified(msg) => {
                    tracing::info!("[Hook Modified] {}: {}", hook.name(), msg)
                }
                _ => {}
            }
        }
    }

    /// 模型响应后触发所有 OnModelResponseHook (fire-and-forget)。
    pub async fn trigger_on_model_response(
        &self, response: &str, turn_ctx: &TurnStartContext,
    ) {
        for hook in &self.on_model_response_hooks {
            match hook.on_model_response(response, turn_ctx).await {
                HookResult::Warning(msg) => {
                    tracing::warn!("[Hook Warning] {}: {}", hook.name(), msg)
                }
                HookResult::Denied(reason) => {
                    tracing::warn!("[Hook Denied] {}: {}", hook.name(), reason)
                }
                _ => {}
            }
        }
    }

    /// 错误发生时触发所有 OnErrorHook (fire-and-forget)。
    pub async fn trigger_on_error(&self, ctx: &ErrorContext) {
        for hook in &self.on_error_hooks {
            match hook.on_error(ctx).await {
                HookResult::Warning(msg) => {
                    tracing::warn!("[Hook Warning] {}: {}", hook.name(), msg)
                }
                HookResult::Denied(reason) => {
                    tracing::warn!("[Hook Denied] {}: {}", hook.name(), reason)
                }
                _ => {}
            }
        }
    }

    /// 是否有任何注册的 hook。
    pub fn has_any(&self) -> bool {
        !self.pre_tool_hooks.is_empty()
            || !self.post_tool_hooks.is_empty()
            || !self.post_turn_hooks.is_empty()
            || !self.system_prompt_hooks.is_empty()
            || !self.on_session_start_hooks.is_empty()
            || !self.on_session_end_hooks.is_empty()
            || !self.on_error_hooks.is_empty()
            || !self.on_user_prompt_submit_hooks.is_empty()
            || !self.on_turn_start_hooks.is_empty()
            || !self.on_turn_complete_hooks.is_empty()
            || !self.on_tool_call_start_hooks.is_empty()
            || !self.on_model_response_hooks.is_empty()
    }

    /// 获取各 slot 的 hook 数量统计。
    pub fn stats(&self) -> super::HookStats {
        super::HookStats {
            pre_tool_hooks: self.pre_tool_hooks.len(),
            post_tool_hooks: self.post_tool_hooks.len(),
            post_turn_hooks: self.post_turn_hooks.len(),
            system_prompt_hooks: self.system_prompt_hooks.len(),
            on_session_start_hooks: self.on_session_start_hooks.len(),
            on_session_end_hooks: self.on_session_end_hooks.len(),
            on_error_hooks: self.on_error_hooks.len(),
            on_user_prompt_submit_hooks: self.on_user_prompt_submit_hooks.len(),
            on_tool_call_start_hooks: self.on_tool_call_start_hooks.len(),
            on_model_response_hooks: self.on_model_response_hooks.len(),
            on_turn_start_hooks: self.on_turn_start_hooks.len(),
            on_turn_complete_hooks: self.on_turn_complete_hooks.len(),
        }
    }

    // ── 加载 / 注册内置 Hook ──────────────────────────────────────

    /// 统一加载：JSON + TOML + 内置 + Webhook
    ///
    /// 注意：当前 `load_all` 每次从磁盘重新加载全部 hook，不维护跨 reload 状态。
    /// 如果未来 hook 需要维护持久状态（rate limiter、缓存），应改为增量 `reload`。
    pub fn load_all(&mut self, working_dir: &Path) {
        // 1. JSON 配置 (老 CC 兼容系统)
        self.load_json_hooks(working_dir);

        // 2. TOML 配置 (新系统)
        self.load_toml_hooks(working_dir);

        // 3. 内置 hook
        self.register_builtins();

        // 4. Webhook
        self.load_webhook_hooks(working_dir);
    }

    /// 从 JSON 配置加载 + 注册为 ShellCommandHook
    fn load_json_hooks(&mut self, working_dir: &Path) {
        let configs = load_hooks_config(working_dir);
        for config in configs {
            match config.event {
                HookEvent::Notification => {
                    // Notification 事件无对应 trait, CC 生态中也被静默跳过
                    tracing::warn!(
                        "[Hook] Notification hooks not supported, skipping: {}",
                        config.command
                    );
                    continue;
                }
                _ => {}
            }

            let shell_hook = Arc::new(ShellCommandHook::from_hook_config(config));

            match shell_hook.event {
                HookEvent::PreToolUse => {
                    // 只注册为 PreToolExecutionHook（可阻断/修改）。
                    // OnToolCallStartHook 是内置 hook 的专用 slot（如 ToolAuditLogHook），
                    // 用户 shell hook 不应双重触发。
                    self.register_pre_tool_hook(shell_hook);
                }
                HookEvent::PostToolUse => {
                    self.register_post_tool_hook(shell_hook);
                }
                HookEvent::SessionStart => {
                    self.register_on_session_start_hook(shell_hook);
                }
                HookEvent::SessionEnd => {
                    self.register_on_session_end_hook(shell_hook);
                }
                HookEvent::UserPromptSubmit => {
                    self.register_on_user_prompt_submit_hook(shell_hook);
                }
                HookEvent::Notification => unreachable!(),
            }
        }
    }

    /// 从 TOML 配置加载 ScriptHook + WebhookHook
    fn load_toml_hooks(&mut self, working_dir: &Path) {
        // 全局 hooks
        if let Some(home) = dirs::home_dir() {
            let global_dir = home.join(".atomcode").join("hooks");
            if global_dir.exists() {
                if let Ok(config) = super::config_loader::HooksConfig::from_dir(&global_dir) {
                    config.register_hooks_to_engine(self, &global_dir);
                }
            }
        }

        // 项目级 hooks
        let project_dir = working_dir.join(".atomcode").join("hooks");
        if project_dir.exists() {
            if let Ok(config) = super::config_loader::HooksConfig::from_dir(&project_dir) {
                config.register_hooks_to_engine(self, &project_dir);
            }
        }
    }

    /// 加载 Webhook hook
    fn load_webhook_hooks(&mut self, working_dir: &Path) {
        // 全局
        if let Some(home) = dirs::home_dir() {
            let global_dir = home.join(".atomcode").join("hooks");
            if global_dir.exists() {
                if let Ok(config) = super::config_loader::HooksConfig::from_dir(&global_dir) {
                    config.register_webhooks_to_engine(self);
                }
            }
        }
        // 项目
        let project_dir = working_dir.join(".atomcode").join("hooks");
        if project_dir.exists() {
            if let Ok(config) = super::config_loader::HooksConfig::from_dir(&project_dir) {
                config.register_webhooks_to_engine(self);
            }
        }
    }

    /// 注册所有内置 hook。
    ///
    /// 当前所有内置 hook 暂停使用，待后续通过配置文件驱动启用。
    fn register_builtins(&mut self) {}
}

// ============================================================================
// ShellCommandHook — 包装老 JSON 配置的 shell 命令 Hook
//
// 实现 5 个 trait:
// - PreToolExecutionHook  (匹配 + 执行 + stdout 解析)
// - PostToolExecutionHook (fire-and-forget)
// - OnSessionStartHook    (fire-and-forget)
// - OnSessionEndHook      (fire-and-forget)
// - OnUserPromptSubmitHook (stdin JSON 协议 + last-line-first 解析)
// ============================================================================

pub struct ShellCommandHook {
    name: String,
    command: String,
    event: HookEvent,
    matcher: Option<String>,
    timeout_ms: u64,
    plugin_root: Option<PathBuf>,
}

impl ShellCommandHook {
    pub fn from_hook_config(config: HookConfig) -> Self {
        Self {
            name: config.command.clone(), // 用 command 作为 name
            command: config.command,
            event: config.event,
            matcher: config.matcher,
            timeout_ms: config.timeout_ms,
            plugin_root: config.plugin_root,
        }
    }

    /// 检查此 hook 是否匹配给定的工具名。
    fn tool_matches(&self, tool_name: &str) -> bool {
        matches_tool(&self.matcher, tool_name)
    }

    /// 执行 shell 命令，返回 stdout（环境变量协议）。
    async fn execute_hook(&self, ctx: &HookContext) -> anyhow::Result<String> {
        let ctx_json = serde_json::to_string(ctx).unwrap_or_else(|e| {
            tracing::warn!("[Hook Warning] Failed to serialize HookContext to JSON: {}", e);
            "{}".to_string()
        });

        let mut cmd = crate::process_utils::shell_command(&self.command);
        cmd.env("ATOMCODE_HOOK_EVENT", &ctx.event)
            .env("ATOMCODE_HOOK_CONTEXT", &ctx_json)
            .kill_on_drop(true);

        if let Some(ref name) = ctx.tool_name {
            cmd.env("ATOMCODE_TOOL_NAME", name);
        }
        if let Some(ref root) = self.plugin_root {
            let s = root.as_os_str();
            cmd.env("CLAUDE_PLUGIN_ROOT", s);
            cmd.env("ATOMCODE_PLUGIN_ROOT", s);
        }

        crate::process_utils::suppress_console_window(&mut cmd);

        let timeout = Duration::from_millis(self.timeout_ms);
        let output = tokio::time::timeout(timeout, cmd.output()).await??;

        if !output.status.success() {
            anyhow::bail!(
                "hook command exited with status {}",
                output.status.code().unwrap_or(-1)
            );
        }

        Ok(crate::process_utils::decode_subprocess_output(&output.stdout))
    }

    /// 执行 shell 命令并 pipe stdin，返回 (exit_ok, stdout, stderr)。
    async fn execute_hook_with_stdin(
        &self,
        payload_json: &str,
    ) -> anyhow::Result<(bool, String, String)> {
        let mut cmd = crate::process_utils::shell_command(&self.command);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        if let Some(ref root) = self.plugin_root {
            let s = root.as_os_str();
            cmd.env("CLAUDE_PLUGIN_ROOT", s);
            cmd.env("ATOMCODE_PLUGIN_ROOT", s);
        }
        crate::process_utils::suppress_console_window(&mut cmd);

        let timeout = Duration::from_millis(self.timeout_ms);

        let fut = async {
            let mut child = cmd.spawn()?;
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(payload_json.as_bytes()).await?;
                stdin.shutdown().await.ok();
                drop(stdin);
            }
            let output = child.wait_with_output().await?;
            anyhow::Ok((
                output.status.success(),
                crate::process_utils::decode_subprocess_output(&output.stdout),
                crate::process_utils::decode_subprocess_output(&output.stderr),
            ))
        };

        Ok(tokio::time::timeout(timeout, fut).await??)
    }

}

impl Hook for ShellCommandHook {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_enabled(&self) -> bool {
        true
    }
}

// ── PreToolExecutionHook ────────────────────────────────────────

#[async_trait]
impl PreToolExecutionHook for ShellCommandHook {
    async fn on_pre_execute(&self, ctx: &HookCtx) -> HookResult {
        if !self.tool_matches(&ctx.tool_name) {
            return HookResult::Ok;
        }

        let exec_ctx = HookContext {
            event: "pre_tool_use".to_string(),
            tool_name: Some(ctx.tool_name.clone()),
            tool_args: Some(serde_json::Value::String(ctx.tool_args.clone())),
            tool_result: None,
            tool_success: None,
            session_id: ctx.session_id.clone().unwrap_or_default(),
            working_dir: ctx.working_dir.clone(),
        };

        match self.execute_hook(&exec_ctx).await {
            Ok(stdout) => {
                match serde_json::from_str::<PreHookResult>(&stdout) {
                    Ok(PreHookResult::Block { reason }) => HookResult::Denied(reason),
                    Ok(PreHookResult::Modify { args }) => HookResult::Modified(
                        serde_json::to_string(&args).unwrap_or_default(),
                    ),
                    Ok(PreHookResult::Allow) => HookResult::ExplicitAllow,
                    Err(_) => HookResult::Ok, // non-JSON → Allow
                }
            }
            Err(_) => HookResult::Ok, // timeout/crash → Allow (fail-open)
        }
    }
}

// ── PostToolExecutionHook ───────────────────────────────────────

#[async_trait]
impl PostToolExecutionHook for ShellCommandHook {
    async fn on_post_execute(&self, ctx: &HookCtx, result_ctx: &ToolResultContext) -> HookResult {
        if !self.tool_matches(&ctx.tool_name) {
            return HookResult::Ok;
        }

        let exec_ctx = HookContext {
            event: "post_tool_use".to_string(),
            tool_name: Some(ctx.tool_name.clone()),
            tool_args: Some(serde_json::Value::String(ctx.tool_args.clone())),
            tool_result: Some(result_ctx.result.clone()),
            tool_success: Some(result_ctx.success),
            session_id: ctx.session_id.clone().unwrap_or_default(),
            working_dir: ctx.working_dir.clone(),
        };

        // fire-and-forget
        let _ = self.execute_hook(&exec_ctx).await;
        HookResult::Ok
    }
}

// ── OnSessionStartHook ──────────────────────────────────────────

#[async_trait]
impl OnSessionStartHook for ShellCommandHook {
    async fn on_session_start(&self, ctx: &super::SessionContext) -> HookResult {
        let exec_ctx = HookContext {
            event: "session_start".to_string(),
            tool_name: None,
            tool_args: None,
            tool_result: None,
            tool_success: None,
            session_id: ctx.session_id.clone(),
            working_dir: ctx.working_dir.clone(),
        };

        let _ = self.execute_hook(&exec_ctx).await;
        HookResult::Ok
    }
}

// ── OnSessionEndHook ────────────────────────────────────────────

#[async_trait]
impl OnSessionEndHook for ShellCommandHook {
    async fn on_session_end(&self, ctx: &super::SessionContext) -> HookResult {
        let exec_ctx = HookContext {
            event: "session_end".to_string(),
            tool_name: None,
            tool_args: None,
            tool_result: None,
            tool_success: None,
            session_id: ctx.session_id.clone(),
            working_dir: ctx.working_dir.clone(),
        };

        let _ = self.execute_hook(&exec_ctx).await;
        HookResult::Ok
    }
}

// ── OnUserPromptSubmitHook ──────────────────────────────────────

#[async_trait]
impl OnUserPromptSubmitHook for ShellCommandHook {
    async fn on_user_prompt_submit(
        &self,
        payload: &UserPromptSubmitPayload,
    ) -> UserPromptSubmitResult {
        let payload_json =
            serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());

        match self.execute_hook_with_stdin(&payload_json).await {
            Ok((exit_ok, stdout, stderr)) => {
                if !exit_ok {
                    // Non-zero exit: check if the hook explicitly blocked
                    // via structured JSON before treating it as an
                    // environment failure. This lets intentional blocks
                    // still work while degrading "command not found" etc.
                    // to a non-blocking warning.
                    let last_line = stdout.lines().rev().find(|l| !l.trim().is_empty());
                    let json_action = last_line.and_then(|l| {
                        serde_json::from_str::<UserPromptSubmitOutput>(l.trim()).ok()
                    });
                    if let Some(parsed) = json_action {
                        if matches!(parsed.decision.as_deref(), Some("block")) {
                            let reason = parsed
                                .reason
                                .unwrap_or_else(|| "user prompt blocked by hook".into());
                            return UserPromptSubmitResult::Block(reason);
                        }
                    }
                    // No structured block decision — this is an
                    // environment failure (missing dep, crash, etc.),
                    // not an intentional rejection. Warn but continue.
                    let reason = if !stderr.trim().is_empty() {
                        stderr.trim().to_string()
                    } else if !stdout.trim().is_empty() {
                        stdout.trim().to_string()
                    } else {
                        "hook exited with error".into()
                    };
                    return UserPromptSubmitResult::Warning(reason);
                }

                // last-line-first JSON 解析 (CC parity)
                let last_line = stdout.lines().rev().find(|l| !l.trim().is_empty());
                let json_action = last_line.and_then(|l| {
                    serde_json::from_str::<UserPromptSubmitOutput>(l.trim()).ok()
                });

                if let Some(parsed) = json_action {
                    if matches!(parsed.decision.as_deref(), Some("block")) {
                        let reason = parsed
                            .reason
                            .unwrap_or_else(|| "user prompt blocked by hook".into());
                        return UserPromptSubmitResult::Block(reason);
                    }
                    if let Some(ctx) = parsed
                        .hook_specific_output
                        .and_then(|o| o.additional_context)
                    {
                        return UserPromptSubmitResult::Inject(ctx);
                    }
                    // Valid JSON but no actionable fields → silent continue
                    return UserPromptSubmitResult::Continue;
                }

                // No JSON decision → treat whole stdout as plain-text inject
                let trimmed = stdout.trim();
                if !trimmed.is_empty() {
                    UserPromptSubmitResult::Inject(trimmed.to_string())
                } else {
                    UserPromptSubmitResult::Continue
                }
            }
            Err(_) => {
                // timeout / spawn failure → fail-open → Continue
                UserPromptSubmitResult::Continue
            }
        }
    }
}

// ── OnToolCallStartHook ─────────────────────────────────────────

#[async_trait]
impl OnToolCallStartHook for ShellCommandHook {
    async fn on_tool_call_start(
        &self,
        _ctx: &super::ToolCallStartContext,
    ) -> HookResult {
        // ShellCommandHook 在此事件不做事（后续可扩展）
        HookResult::Ok
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook::{HookConfig, HookContext, HookEvent};

    // ── Helpers ────────────────────────────────────────────────

    #[allow(dead_code)]
    fn test_ctx() -> HookContext {
        HookContext {
            event: "pre_tool_use".to_string(),
            tool_name: Some("bash".to_string()),
            tool_args: None,
            tool_result: None,
            tool_success: None,
            session_id: String::new(),
            working_dir: String::new(),
        }
    }

    #[allow(dead_code)]
    fn make_hook(event: HookEvent, matcher: Option<&str>, cmd: &str) -> HookConfig {
        HookConfig {
            event,
            matcher: matcher.map(String::from),
            command: cmd.to_string(),
            timeout_ms: 10_000,
            plugin_root: None,
        }
    }

    // ── PreToolUse tests ────────────────────────────────────────

    #[tokio::test]
    async fn empty_engine_allows() {
        let engine = HookEngine::new();
        let ctx = HookCtx::new("bash".into(), "{}".into(), "/tmp".into());
        let result = engine.trigger_pre_tool_use(&ctx).await;
        assert!(result.is_ok());
        let (args, allowed) = result.unwrap();
        assert!(args.is_none());
        assert!(!allowed); // empty engine → no explicit allow
    }

    #[tokio::test]
    async fn shell_command_hook_returning_allow_json() {
        let config = HookConfig {
            event: HookEvent::PreToolUse,
            matcher: None,
            command: "echo '{\"action\": \"allow\"}'".to_string(),
            timeout_ms: 10_000,
            plugin_root: None,
        };
        let hook = Arc::new(ShellCommandHook::from_hook_config(config));
        let ctx = HookCtx::new("bash".into(), "{}".into(), "/tmp".into());
        let result = hook.on_pre_execute(&ctx).await;
        assert!(matches!(result, HookResult::ExplicitAllow));
    }

    #[tokio::test]
    async fn shell_command_hook_returning_block_json() {
        let config = HookConfig {
            event: HookEvent::PreToolUse,
            matcher: None,
            command: "echo '{\"action\": \"block\", \"reason\": \"not allowed\"}'".to_string(),
            timeout_ms: 10_000,
            plugin_root: None,
        };
        let hook = Arc::new(ShellCommandHook::from_hook_config(config));
        let ctx = HookCtx::new("bash".into(), "{}".into(), "/tmp".into());
        let result = hook.on_pre_execute(&ctx).await;
        assert!(matches!(result, HookResult::Denied(_)));
    }

    #[tokio::test]
    async fn shell_command_hook_returning_modify_json() {
        let config = HookConfig {
            event: HookEvent::PreToolUse,
            matcher: None,
            command: "echo '{\"action\": \"modify\", \"args\": {\"modified\": true}}'".to_string(),
            timeout_ms: 10_000,
            plugin_root: None,
        };
        let hook = Arc::new(ShellCommandHook::from_hook_config(config));
        let ctx = HookCtx::new("bash".into(), "{}".into(), "/tmp".into());
        let result = hook.on_pre_execute(&ctx).await;
        assert!(matches!(result, HookResult::Modified(_)));
    }

    #[tokio::test]
    async fn hook_returning_non_json_allows() {
        let config = HookConfig {
            event: HookEvent::PreToolUse,
            matcher: None,
            command: "echo 'debug output'".to_string(),
            timeout_ms: 10_000,
            plugin_root: None,
        };
        let hook = Arc::new(ShellCommandHook::from_hook_config(config));
        let ctx = HookCtx::new("bash".into(), "{}".into(), "/tmp".into());
        let result = hook.on_pre_execute(&ctx).await;
        assert!(matches!(result, HookResult::Ok));
    }

    #[tokio::test]
    async fn hook_timeout_degrades_to_allow() {
        let config = HookConfig {
            event: HookEvent::PreToolUse,
            matcher: None,
            command: "sleep 10".to_string(),
            timeout_ms: 1,  // 极短超时
            plugin_root: None,
        };
        let hook = Arc::new(ShellCommandHook::from_hook_config(config));
        let ctx = HookCtx::new("bash".into(), "{}".into(), "/tmp".into());
        let result = hook.on_pre_execute(&ctx).await;
        assert!(matches!(result, HookResult::Ok));
    }

    #[tokio::test]
    async fn hook_crash_degrades_to_allow() {
        let config = HookConfig {
            event: HookEvent::PreToolUse,
            matcher: None,
            command: "exit 1".to_string(),
            timeout_ms: 10_000,
            plugin_root: None,
        };
        let hook = Arc::new(ShellCommandHook::from_hook_config(config));
        let ctx = HookCtx::new("bash".into(), "{}".into(), "/tmp".into());
        let result = hook.on_pre_execute(&ctx).await;
        assert!(matches!(result, HookResult::Ok));
    }

    #[tokio::test]
    async fn matcher_filters_correctly() {
        let config = HookConfig {
            event: HookEvent::PreToolUse,
            matcher: Some("edit_*".to_string()),
            command: "echo '{\"action\": \"block\", \"reason\": \"no\"}'".to_string(),
            timeout_ms: 10_000,
            plugin_root: None,
        };
        let hook = Arc::new(ShellCommandHook::from_hook_config(config));

        // 匹配: edit_file
        let ctx = HookCtx::new("edit_file".into(), "{}".into(), "/tmp".into());
        let result = hook.on_pre_execute(&ctx).await;
        assert!(matches!(result, HookResult::Denied(_)));

        // 不匹配: bash
        let ctx = HookCtx::new("bash".into(), "{}".into(), "/tmp".into());
        let result = hook.on_pre_execute(&ctx).await;
        assert!(matches!(result, HookResult::Ok));
    }

    // ── PostToolUse tests ───────────────────────────────────────

    #[tokio::test]
    async fn post_tool_use_fire_and_forget() {
        let config = HookConfig {
            event: HookEvent::PostToolUse,
            matcher: None,
            command: "echo 'ok'".to_string(),
            timeout_ms: 10_000,
            plugin_root: None,
        };
        let hook = Arc::new(ShellCommandHook::from_hook_config(config));
        let ctx = HookCtx::new("bash".into(), "{}".into(), "/tmp".into());
        let result_ctx = ToolResultContext {
            tool_name: "bash".to_string(),
            tool_args: "{}".to_string(),
            result: "output".to_string(),
            success: true,
            duration_ms: 100,
        };
        let result = hook.on_post_execute(&ctx, &result_ctx).await;
        assert!(matches!(result, HookResult::Ok));
    }

    // ── UserPromptSubmit tests ──────────────────────────────────

    #[tokio::test]
    async fn user_prompt_no_hooks_returns_continue() {
        let engine = HookEngine::new();
        let result = engine
            .trigger_user_prompt_submit("hello", "s1", "/tmp")
            .await;
        assert!(matches!(result, UserPromptHookResult::Continue));
    }

    #[tokio::test]
    async fn user_prompt_decision_block_blocks() {
        let config = HookConfig {
            event: HookEvent::UserPromptSubmit,
            matcher: None,
            command: "echo '{\"decision\": \"block\", \"reason\": \"no\"}'".to_string(),
            timeout_ms: 10_000,
            plugin_root: None,
        };
        let hook = Arc::new(ShellCommandHook::from_hook_config(config));
        let payload = UserPromptSubmitPayload {
            session_id: "s1".into(),
            hook_event_name: "UserPromptSubmit".into(),
            prompt: "hello".into(),
            cwd: "/tmp".into(),
        };
        let result = hook.on_user_prompt_submit(&payload).await;
        assert!(matches!(result, UserPromptSubmitResult::Block(_)));
    }

    #[tokio::test]
    async fn user_prompt_hook_specific_output_injects() {
        let config = HookConfig {
            event: HookEvent::UserPromptSubmit,
            matcher: None,
            command: "echo '{\"hookSpecificOutput\": {\"additionalContext\": \"helpful context\"}}'".to_string(),
            timeout_ms: 10_000,
            plugin_root: None,
        };
        let hook = Arc::new(ShellCommandHook::from_hook_config(config));
        let payload = UserPromptSubmitPayload {
            session_id: "s1".into(),
            hook_event_name: "UserPromptSubmit".into(),
            prompt: "hello".into(),
            cwd: "/tmp".into(),
        };
        let result = hook.on_user_prompt_submit(&payload).await;
        assert!(matches!(result, UserPromptSubmitResult::Inject(s) if s == "helpful context"));
    }

    #[tokio::test]
    async fn user_prompt_plain_stdout_injects_context() {
        let config = HookConfig {
            event: HookEvent::UserPromptSubmit,
            matcher: None,
            command: "echo 'some plain text'".to_string(),
            timeout_ms: 10_000,
            plugin_root: None,
        };
        let hook = Arc::new(ShellCommandHook::from_hook_config(config));
        let payload = UserPromptSubmitPayload {
            session_id: "s1".into(),
            hook_event_name: "UserPromptSubmit".into(),
            prompt: "hello".into(),
            cwd: "/tmp".into(),
        };
        let result = hook.on_user_prompt_submit(&payload).await;
        assert!(matches!(result, UserPromptSubmitResult::Inject(ref s) if s == "some plain text"));
    }

    #[tokio::test]
    async fn user_prompt_nonzero_exit_blocks_with_stderr() {
        let config = HookConfig {
            event: HookEvent::UserPromptSubmit,
            matcher: None,
            command: "echo 'block message' >&2; exit 1".to_string(),
            timeout_ms: 10_000,
            plugin_root: None,
        };
        let hook = Arc::new(ShellCommandHook::from_hook_config(config));
        let payload = UserPromptSubmitPayload {
            session_id: "s1".into(),
            hook_event_name: "UserPromptSubmit".into(),
            prompt: "hello".into(),
            cwd: "/tmp".into(),
        };
        let result = hook.on_user_prompt_submit(&payload).await;
        assert!(matches!(result, UserPromptSubmitResult::Warning(ref s) if s == "block message"));
    }

    #[tokio::test]
    async fn user_prompt_nonzero_exit_with_json_block_still_blocks() {
        let config = HookConfig {
            event: HookEvent::UserPromptSubmit,
            matcher: None,
            command: r#"echo '{"decision":"block","reason":"intentional"}'; exit 1"#.to_string(),
            timeout_ms: 10_000,
            plugin_root: None,
        };
        let hook = Arc::new(ShellCommandHook::from_hook_config(config));
        let payload = UserPromptSubmitPayload {
            session_id: "s1".into(),
            hook_event_name: "UserPromptSubmit".into(),
            prompt: "hello".into(),
            cwd: "/tmp".into(),
        };
        let result = hook.on_user_prompt_submit(&payload).await;
        assert!(matches!(result, UserPromptSubmitResult::Block(ref s) if s == "intentional"));
    }

    #[tokio::test]
    async fn user_prompt_timeout_degrades_to_continue() {
        let config = HookConfig {
            event: HookEvent::UserPromptSubmit,
            matcher: None,
            command: "sleep 10".to_string(),
            timeout_ms: 1,
            plugin_root: None,
        };
        let hook = Arc::new(ShellCommandHook::from_hook_config(config));
        let payload = UserPromptSubmitPayload {
            session_id: "s1".into(),
            hook_event_name: "UserPromptSubmit".into(),
            prompt: "hello".into(),
            cwd: "/tmp".into(),
        };
        let result = hook.on_user_prompt_submit(&payload).await;
        assert!(matches!(result, UserPromptSubmitResult::Continue));
    }

    #[tokio::test]
    async fn user_prompt_block_after_debug_noise_still_blocks() {
        // CC parity: debug lines before JSON decision, last-line wins
        let config = HookConfig {
            event: HookEvent::UserPromptSubmit,
            matcher: None,
            command: "echo 'debug line 1'; echo 'debug line 2'; echo '{\"decision\": \"block\", \"reason\": \"stuff\"}'".to_string(),
            timeout_ms: 10_000,
            plugin_root: None,
        };
        let hook = Arc::new(ShellCommandHook::from_hook_config(config));
        let payload = UserPromptSubmitPayload {
            session_id: "s1".into(),
            hook_event_name: "UserPromptSubmit".into(),
            prompt: "hello".into(),
            cwd: "/tmp".into(),
        };
        let result = hook.on_user_prompt_submit(&payload).await;
        assert!(matches!(result, UserPromptSubmitResult::Block(_)));
    }

    #[tokio::test]
    async fn user_prompt_json_empty_object_continues() {
        let config = HookConfig {
            event: HookEvent::UserPromptSubmit,
            matcher: None,
            command: "echo '{}'".to_string(),
            timeout_ms: 10_000,
            plugin_root: None,
        };
        let hook = Arc::new(ShellCommandHook::from_hook_config(config));
        let payload = UserPromptSubmitPayload {
            session_id: "s1".into(),
            hook_event_name: "UserPromptSubmit".into(),
            prompt: "hello".into(),
            cwd: "/tmp".into(),
        };
        let result = hook.on_user_prompt_submit(&payload).await;
        assert!(matches!(result, UserPromptSubmitResult::Continue));
    }

    #[tokio::test]
    async fn user_prompt_whitespace_only_continues() {
        let config = HookConfig {
            event: HookEvent::UserPromptSubmit,
            matcher: None,
            command: "echo '   '".to_string(),
            timeout_ms: 10_000,
            plugin_root: None,
        };
        let hook = Arc::new(ShellCommandHook::from_hook_config(config));
        let payload = UserPromptSubmitPayload {
            session_id: "s1".into(),
            hook_event_name: "UserPromptSubmit".into(),
            prompt: "hello".into(),
            cwd: "/tmp".into(),
        };
        let result = hook.on_user_prompt_submit(&payload).await;
        assert!(matches!(result, UserPromptSubmitResult::Continue));
    }

    // ── Session event tests ─────────────────────────────────────

    #[tokio::test]
    async fn session_start_runs_matching_hooks() {
        let config = HookConfig {
            event: HookEvent::SessionStart,
            matcher: None,
            command: "echo 'started'".to_string(),
            timeout_ms: 10_000,
            plugin_root: None,
        };
        let mut engine = HookEngine::new();
        let hook = Arc::new(ShellCommandHook::from_hook_config(config));
        engine.register_on_session_start_hook(hook);
        let ctx = crate::hook::SessionContext {
            session_id: "test-s1".into(),
            working_dir: "/tmp".into(),
            model_name: "test-model".into(),
            provider_name: "mock".into(),
        };
        // Should not panic
        engine.trigger_session_start(&ctx).await;
    }

    #[tokio::test]
    async fn session_end_runs_matching_hooks() {
        let config = HookConfig {
            event: HookEvent::SessionEnd,
            matcher: None,
            command: "echo 'ended'".to_string(),
            timeout_ms: 10_000,
            plugin_root: None,
        };
        let mut engine = HookEngine::new();
        let hook = Arc::new(ShellCommandHook::from_hook_config(config));
        engine.register_on_session_end_hook(hook);
        let ctx = crate::hook::SessionContext {
            session_id: "test-s2".into(),
            working_dir: "/tmp".into(),
            model_name: "test-model".into(),
            provider_name: "mock".into(),
        };
        // Should not panic
        engine.trigger_session_end(&ctx).await;
    }

    #[tokio::test]
    async fn session_event_no_matching_hooks_noop() {
        let engine = HookEngine::new();
        let ctx = crate::hook::SessionContext {
            session_id: "test-s3".into(),
            working_dir: "/tmp".into(),
            model_name: "test-model".into(),
            provider_name: "mock".into(),
        };
        // Should not panic with empty engine
        engine.trigger_session_start(&ctx).await;
        engine.trigger_session_end(&ctx).await;
    }

    // ── load_all tests ──────────────────────────────────────────

    #[tokio::test]
    async fn load_all_does_not_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = HookEngine::new();
        engine.load_all(tmp.path());
        // Should not panic even if no config files exist
    }

    #[tokio::test]
    async fn has_any_returns_false_for_empty_engine() {
        let engine = HookEngine::new();
        assert!(!engine.has_any());
    }

    #[tokio::test]
    async fn has_any_returns_true_with_pre_tool_hook() {
        let mut engine = HookEngine::new();
        let config = HookConfig {
            event: HookEvent::PreToolUse,
            matcher: None,
            command: "echo ok".to_string(),
            timeout_ms: 10_000,
            plugin_root: None,
        };
        let hook = Arc::new(ShellCommandHook::from_hook_config(config));
        engine.register_pre_tool_hook(hook);
        assert!(engine.has_any());
    }
}
