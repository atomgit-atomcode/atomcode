use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

pub mod async_batcher;
pub mod built_in;
pub mod config_loader;
pub mod script_runner;
pub mod webhook;

/// Hook 执行结果
#[derive(Debug, Clone)]
pub enum HookResult {
    /// Hook 成功执行
    Ok,
    /// Hook 失败（非致命，仅记录警告）
    Warning(String),
    /// Hook 拒绝继续操作（致命）
    Denied(String),
    /// Hook 修改了参数（返回新的参数）
    Modified(String),
}

/// 用户消息上下文
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMessageContext {
    /// 用户消息内容
    pub content: String,
    /// Session ID
    pub session_id: Option<String>,
    /// 附加的文件路径
    pub attached_files: Vec<String>,
    /// 时间戳
    pub timestamp: String,
}

/// Turn 开始上下文
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnStartContext {
    /// Turn 编号
    pub turn_number: u32,
    /// Session ID
    pub session_id: Option<String>,
    /// 工作目录
    pub working_dir: String,
    /// 当前阶段（planning/diagnosis/execution）
    pub phase: String,
    /// 是否有文件上下文
    pub has_file_context: bool,
}

/// 工具调用开始上下文
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallStartContext {
    /// 工具名称
    pub tool_name: String,
    /// 工具参数
    pub tool_args: String,
    /// 调用 ID
    pub call_id: String,
    /// Turn 编号
    pub turn_number: u32,
}

/// Turn 完成上下文
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnCompleteContext {
    /// Turn 编号
    pub turn_number: u32,
    /// 结果类型（Responded/UsedTools/Failed/Cancelled）
    pub result_type: String,
    /// 消耗的 token 数
    pub tokens_used: usize,
    /// 工具调用次数
    pub tool_calls: usize,
    /// 执行时长（毫秒）
    pub duration_ms: u64,
    /// 是否被截断
    pub truncated: bool,
    /// 编辑的文件列表
    pub edited_files: Vec<String>,
}

/// 会话上下文
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionContext {
    /// Session ID
    pub session_id: String,
    /// 工作目录
    pub working_dir: String,
    /// 模型名称
    pub model_name: String,
    /// Provider 名称
    pub provider_name: String,
}

/// 错误上下文
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorContext {
    /// 错误类型
    pub error_type: String,
    /// 错误信息
    pub error_message: String,
    /// 发生错误的阶段
    pub phase: String,
    /// Turn 编号（如果适用）
    pub turn_number: Option<u32>,
}

/// Hook 上下文 - 传递给钩子的数据（向后兼容）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookContext {
    /// 当前工具名称
    pub tool_name: String,
    /// 工具参数（JSON 字符串）
    pub tool_args: String,
    /// 工作目录
    pub working_dir: String,
    /// 当前 session ID
    pub session_id: Option<String>,
    /// 当前 turn 编号
    pub turn_number: u32,
    /// 额外元数据
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, String>>,
}

impl HookContext {
    pub fn new(tool_name: String, tool_args: String, working_dir: String) -> Self {
        Self {
            tool_name,
            tool_args,
            working_dir,
            session_id: None,
            turn_number: 0,
            metadata: None,
        }
    }

    pub fn with_session(mut self, session_id: String) -> Self {
        self.session_id = Some(session_id);
        self
    }

    pub fn with_turn(mut self, turn_number: u32) -> Self {
        self.turn_number = turn_number;
        self
    }

    pub fn with_metadata(mut self, key: String, value: String) -> Self {
        let metadata = self.metadata.get_or_insert_with(HashMap::new);
        metadata.insert(key, value);
        self
    }
}

/// 工具执行结果上下文（用于 PostExecutionHook）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultContext {
    pub tool_name: String,
    pub tool_args: String,
    pub result: String,
    pub success: bool,
    pub duration_ms: u64,
}

/// Hook trait - 所有钩子必须实现的基础接口
#[async_trait]
pub trait Hook: Send + Sync {
    /// Hook 名称（用于日志和调试）
    fn name(&self) -> &str;

    /// Hook 描述（用于文档）
    fn description(&self) -> &str {
        ""
    }

    /// 是否应该启用此 hook
    fn is_enabled(&self) -> bool {
        true
    }

    /// Hook 优先级（数字越小越先执行）
    fn priority(&self) -> i32 {
        0
    }
}

/// 工具执行前钩子 - 在工具实际执行前调用
/// 可用于：参数验证/修改、额外检查、日志记录、阻止执行
#[async_trait]
pub trait PreToolExecutionHook: Hook {
    /// 返回 HookResult 决定是否继续执行
    /// - Ok: 继续执行
    /// - Modified(new_args): 使用新参数继续执行
    /// - Denied(reason): 阻止执行
    /// - Warning(msg): 记录警告但继续执行
    async fn on_pre_execute(&self, ctx: &HookContext) -> HookResult;
}

/// 工具执行后钩子 - 在工具执行完成后调用
/// 可用于：结果处理、日志记录、触发后续操作、结果修改
#[async_trait]
pub trait PostToolExecutionHook: Hook {
    /// 接收工具执行结果
    async fn on_post_execute(&self, ctx: &HookContext, result_ctx: &ToolResultContext) -> HookResult;
}

/// Turn 完成后钩子 - 在一轮对话结束后调用
/// 可用于：自动提交、代码审查、统计收集
#[async_trait]
pub trait PostTurnHook: Hook {
    /// Turn 完成后的回调
    /// turn_result 包含本轮的最终状态（Responded/ToolUsed/Failed）
    async fn on_post_turn(&self, ctx: &HookContext, turn_result: &str) -> HookResult;
}

/// 系统 Prompt 扩展钩子 - 在构建系统提示时调用
/// 可用于：注入额外规则、添加自定义指令
#[async_trait]
pub trait SystemPromptHook: Hook {
    /// 返回要添加到系统 prompt 的内容
    async fn extend_system_prompt(&self) -> Option<String>;
}

// ============================================================================
// 新增的工程化 Hook 时机
// ============================================================================

/// 用户消息接收钩子 - 在收到用户消息时调用
/// 可用于：消息过滤、审计、自动回复、上下文增强
#[async_trait]
pub trait OnMessageReceivedHook: Hook {
    /// 用户消息接收时的回调
    /// 返回 Modified 可以修改用户消息内容
    async fn on_message_received(&self, ctx: &UserMessageContext) -> HookResult;
}

/// Turn 开始钩子 - 在 Turn 开始前调用
/// 可用于：注入自定义上下文、设置环境变量、记录日志
#[async_trait]
pub trait OnTurnStartHook: Hook {
    /// Turn 开始时的回调
    async fn on_turn_start(&self, ctx: &TurnStartContext) -> HookResult;
}

/// 工具调用开始钩子 - 在工具调用开始时调用（在权限检查前）
/// 可用于：审计、限流、拦截
#[async_trait]
pub trait OnToolCallStartHook: Hook {
    /// 工具调用开始时的回调
    /// 返回 Denied 可以阻止工具调用
    async fn on_tool_call_start(&self, ctx: &ToolCallStartContext) -> HookResult;
}

/// Turn 完成钩子 - 在 Turn 完成后调用（包含详细信息）
/// 可用于：统计分析、自动操作、报告生成
#[async_trait]
pub trait OnTurnCompleteHook: Hook {
    /// Turn 完成后的回调
    async fn on_turn_complete(&self, ctx: &TurnCompleteContext) -> HookResult;
}

/// 会话开始钩子 - 在会话启动时调用
/// 可用于：初始化、加载自定义上下文、环境检查
#[async_trait]
pub trait OnSessionStartHook: Hook {
    /// 会话开始时的回调
    async fn on_session_start(&self, ctx: &SessionContext) -> HookResult;
}

/// 会话结束钩子 - 在会话结束时调用
/// 可用于：清理、生成报告、保存状态
#[async_trait]
pub trait OnSessionEndHook: Hook {
    /// 会话结束时的回调
    async fn on_session_end(&self, ctx: &SessionContext) -> HookResult;
}

/// 错误发生钩子 - 在错误发生时调用
/// 可用于：错误报告、自动恢复、通知
#[async_trait]
pub trait OnErrorHook: Hook {
    /// 错误发生时的回调
    async fn on_error(&self, ctx: &ErrorContext) -> HookResult;
}

/// 模型响应钩子 - 在模型响应完成后调用
/// 可用于：响应验证、自动修正、日志记录
#[async_trait]
pub trait OnModelResponseHook: Hook {
    /// 模型响应完成后的回调
    async fn on_model_response(&self, response: &str, turn_ctx: &TurnStartContext) -> HookResult;
}

/// Hook 注册表 - 管理和触发所有钩子
pub struct HookRegistry {
    pre_tool_hooks: Vec<Arc<dyn PreToolExecutionHook>>,
    post_tool_hooks: Vec<Arc<dyn PostToolExecutionHook>>,
    post_turn_hooks: Vec<Arc<dyn PostTurnHook>>,
    system_prompt_hooks: Vec<Arc<dyn SystemPromptHook>>,
    // 新增的工程化 hook
    on_message_received_hooks: Vec<Arc<dyn OnMessageReceivedHook>>,
    on_turn_start_hooks: Vec<Arc<dyn OnTurnStartHook>>,
    on_tool_call_start_hooks: Vec<Arc<dyn OnToolCallStartHook>>,
    on_turn_complete_hooks: Vec<Arc<dyn OnTurnCompleteHook>>,
    on_session_start_hooks: Vec<Arc<dyn OnSessionStartHook>>,
    on_session_end_hooks: Vec<Arc<dyn OnSessionEndHook>>,
    on_error_hooks: Vec<Arc<dyn OnErrorHook>>,
    on_model_response_hooks: Vec<Arc<dyn OnModelResponseHook>>,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self {
            pre_tool_hooks: Vec::new(),
            post_tool_hooks: Vec::new(),
            post_turn_hooks: Vec::new(),
            system_prompt_hooks: Vec::new(),
            on_message_received_hooks: Vec::new(),
            on_turn_start_hooks: Vec::new(),
            on_tool_call_start_hooks: Vec::new(),
            on_turn_complete_hooks: Vec::new(),
            on_session_start_hooks: Vec::new(),
            on_session_end_hooks: Vec::new(),
            on_error_hooks: Vec::new(),
            on_model_response_hooks: Vec::new(),
        }
    }

    /// 注册工具执行前钩子
    pub fn register_pre_tool_hook(&mut self, hook: Arc<dyn PreToolExecutionHook>) {
        if hook.is_enabled() {
            self.pre_tool_hooks.push(hook);
            self.pre_tool_hooks.sort_by_key(|h| h.priority());
        }
    }

    /// 注册工具执行后钩子
    pub fn register_post_tool_hook(&mut self, hook: Arc<dyn PostToolExecutionHook>) {
        if hook.is_enabled() {
            self.post_tool_hooks.push(hook);
            self.post_tool_hooks.sort_by_key(|h| h.priority());
        }
    }

    /// 注册 Turn 完成后钩子
    pub fn register_post_turn_hook(&mut self, hook: Arc<dyn PostTurnHook>) {
        if hook.is_enabled() {
            self.post_turn_hooks.push(hook);
            self.post_turn_hooks.sort_by_key(|h| h.priority());
        }
    }

    /// 注册系统 Prompt 扩展钩子
    pub fn register_system_prompt_hook(&mut self, hook: Arc<dyn SystemPromptHook>) {
        if hook.is_enabled() {
            self.system_prompt_hooks.push(hook);
        }
    }

    // 新增 hook 注册方法

    pub fn register_on_message_received_hook(&mut self, hook: Arc<dyn OnMessageReceivedHook>) {
        if hook.is_enabled() {
            self.on_message_received_hooks.push(hook);
            self.on_message_received_hooks.sort_by_key(|h| h.priority());
        }
    }

    pub fn register_on_turn_start_hook(&mut self, hook: Arc<dyn OnTurnStartHook>) {
        if hook.is_enabled() {
            self.on_turn_start_hooks.push(hook);
            self.on_turn_start_hooks.sort_by_key(|h| h.priority());
        }
    }

    pub fn register_on_tool_call_start_hook(&mut self, hook: Arc<dyn OnToolCallStartHook>) {
        if hook.is_enabled() {
            self.on_tool_call_start_hooks.push(hook);
            self.on_tool_call_start_hooks.sort_by_key(|h| h.priority());
        }
    }

    pub fn register_on_turn_complete_hook(&mut self, hook: Arc<dyn OnTurnCompleteHook>) {
        if hook.is_enabled() {
            self.on_turn_complete_hooks.push(hook);
            self.on_turn_complete_hooks.sort_by_key(|h| h.priority());
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

    pub fn register_on_model_response_hook(&mut self, hook: Arc<dyn OnModelResponseHook>) {
        if hook.is_enabled() {
            self.on_model_response_hooks.push(hook);
        }
    }

    /// 触发所有 pre-tool hooks
    /// 返回 Ok(None) 表示正常继续
    /// 返回 Ok(Some(new_args)) 表示参数被修改
    /// 返回 Err(reason) 表示被拒绝
    pub async fn trigger_pre_tool_hooks(
        &self,
        ctx: &HookContext,
    ) -> Result<Option<String>, String> {
        let mut modified_args: Option<String> = None;

        for hook in &self.pre_tool_hooks {
            match hook.on_pre_execute(ctx).await {
                HookResult::Ok => {}
                HookResult::Warning(msg) => {
                    eprintln!("[Hook Warning] {}: {}", hook.name(), msg);
                }
                HookResult::Denied(reason) => {
                    return Err(format!("{}: {}", hook.name(), reason));
                }
                HookResult::Modified(new_args) => {
                    eprintln!("[Hook Modified] {} modified arguments", hook.name());
                    modified_args = Some(new_args);
                }
            }
        }

        Ok(modified_args)
    }

    /// 触发所有 post-tool hooks
    pub async fn trigger_post_tool_hooks(
        &self,
        ctx: &HookContext,
        result_ctx: &ToolResultContext,
    ) {
        for hook in &self.post_tool_hooks {
            match hook.on_post_execute(ctx, result_ctx).await {
                HookResult::Ok | HookResult::Warning(_) | HookResult::Modified(_) => {}
                HookResult::Denied(reason) => {
                    eprintln!("[Hook Denied] {}: {}", hook.name(), reason);
                }
            }
        }
    }

    /// 触发所有 post-turn hooks
    pub async fn trigger_post_turn_hooks(
        &self,
        ctx: &HookContext,
        turn_result: &str,
    ) {
        for hook in &self.post_turn_hooks {
            match hook.on_post_turn(ctx, turn_result).await {
                HookResult::Ok | HookResult::Warning(_) | HookResult::Denied(_) | HookResult::Modified(_) => {}
            }
        }
    }

    /// 收集所有 system prompt 扩展
    pub async fn collect_system_prompt_extensions(&self) -> Vec<String> {
        let mut extensions = Vec::new();
        for hook in &self.system_prompt_hooks {
            if let Some(content) = hook.extend_system_prompt().await {
                extensions.push(content);
            }
        }
        extensions
    }

    /// 获取已注册的 hook 数量
    pub fn stats(&self) -> HookStats {
        HookStats {
            pre_tool_hooks: self.pre_tool_hooks.len(),
            post_tool_hooks: self.post_tool_hooks.len(),
            post_turn_hooks: self.post_turn_hooks.len(),
            system_prompt_hooks: self.system_prompt_hooks.len(),
            on_message_received_hooks: self.on_message_received_hooks.len(),
            on_turn_start_hooks: self.on_turn_start_hooks.len(),
            on_tool_call_start_hooks: self.on_tool_call_start_hooks.len(),
            on_turn_complete_hooks: self.on_turn_complete_hooks.len(),
            on_session_start_hooks: self.on_session_start_hooks.len(),
            on_session_end_hooks: self.on_session_end_hooks.len(),
            on_error_hooks: self.on_error_hooks.len(),
            on_model_response_hooks: self.on_model_response_hooks.len(),
        }
    }
}

#[derive(Debug)]
pub struct HookStats {
    pub pre_tool_hooks: usize,
    pub post_tool_hooks: usize,
    pub post_turn_hooks: usize,
    pub system_prompt_hooks: usize,
    pub on_message_received_hooks: usize,
    pub on_turn_start_hooks: usize,
    pub on_tool_call_start_hooks: usize,
    pub on_turn_complete_hooks: usize,
    pub on_session_start_hooks: usize,
    pub on_session_end_hooks: usize,
    pub on_error_hooks: usize,
    pub on_model_response_hooks: usize,
}

impl Default for HookRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// 新增 hook 触发方法
// ============================================================================

impl HookRegistry {
    /// 触发 on_message_received hooks
    pub async fn trigger_on_message_received(&self, ctx: &UserMessageContext) -> Option<String> {
        let mut modified_content: Option<String> = None;

        for hook in &self.on_message_received_hooks {
            match hook.on_message_received(ctx).await {
                HookResult::Ok => {}
                HookResult::Warning(msg) => {
                    eprintln!("[Hook Warning] {}: {}", hook.name(), msg);
                }
                HookResult::Denied(reason) => {
                    eprintln!("[Hook Denied] {}: {}", hook.name(), reason);
                    return None; // 拒绝处理该消息
                }
                HookResult::Modified(new_content) => {
                    eprintln!("[Hook Modified] {} modified message content", hook.name());
                    modified_content = Some(new_content);
                }
            }
        }

        modified_content
    }

    /// 触发 on_turn_start hooks
    pub async fn trigger_on_turn_start(&self, ctx: &TurnStartContext) {
        for hook in &self.on_turn_start_hooks {
            match hook.on_turn_start(ctx).await {
                HookResult::Ok | HookResult::Warning(_) | HookResult::Modified(_) => {}
                HookResult::Denied(reason) => {
                    eprintln!("[Hook Denied] {}: {}", hook.name(), reason);
                }
            }
        }
    }

    /// 触发 on_tool_call_start hooks
    /// 返回 Err 表示应该阻止该工具调用
    pub async fn trigger_on_tool_call_start(&self, ctx: &ToolCallStartContext) -> Result<(), String> {
        for hook in &self.on_tool_call_start_hooks {
            match hook.on_tool_call_start(ctx).await {
                HookResult::Ok | HookResult::Warning(_) | HookResult::Modified(_) => {}
                HookResult::Denied(reason) => {
                    return Err(format!("{}: {}", hook.name(), reason));
                }
            }
        }
        Ok(())
    }

    /// 触发 on_turn_complete hooks
    pub async fn trigger_on_turn_complete(&self, ctx: &TurnCompleteContext) {
        for hook in &self.on_turn_complete_hooks {
            match hook.on_turn_complete(ctx).await {
                HookResult::Ok | HookResult::Warning(_) | HookResult::Denied(_) | HookResult::Modified(_) => {}
            }
        }
    }

    /// 触发 on_session_start hooks
    pub async fn trigger_on_session_start(&self, ctx: &SessionContext) {
        for hook in &self.on_session_start_hooks {
            match hook.on_session_start(ctx).await {
                HookResult::Ok | HookResult::Warning(_) | HookResult::Denied(_) | HookResult::Modified(_) => {}
            }
        }
    }

    /// 触发 on_session_end hooks
    pub async fn trigger_on_session_end(&self, ctx: &SessionContext) {
        for hook in &self.on_session_end_hooks {
            match hook.on_session_end(ctx).await {
                HookResult::Ok | HookResult::Warning(_) | HookResult::Denied(_) | HookResult::Modified(_) => {}
            }
        }
    }

    /// 触发 on_error hooks
    pub async fn trigger_on_error(&self, ctx: &ErrorContext) {
        for hook in &self.on_error_hooks {
            match hook.on_error(ctx).await {
                HookResult::Ok | HookResult::Warning(_) | HookResult::Denied(_) | HookResult::Modified(_) => {}
            }
        }
    }

    /// 触发 on_model_response hooks
    pub async fn trigger_on_model_response(&self, response: &str, turn_ctx: &TurnStartContext) {
        for hook in &self.on_model_response_hooks {
            match hook.on_model_response(response, turn_ctx).await {
                HookResult::Ok | HookResult::Warning(_) | HookResult::Denied(_) | HookResult::Modified(_) => {}
            }
        }
    }
}
