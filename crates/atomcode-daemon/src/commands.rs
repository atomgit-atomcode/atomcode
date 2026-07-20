//! `POST /command`: 无状态斜杠命令执行器（对已持久化会话/记忆施加一次性变更）。
use axum::{extract::State, response::IntoResponse, Json};
use futures::StreamExt;
use std::path::Path;
use std::sync::Arc;

use crate::AppState;
use atomcode_config::config::memory::MemoryStore;
use atomcode_core::conversation::{Conversation, ConversationSnapshot};
use atomcode_core::session::{Session, SessionId, SessionManager};

struct KernelSummaryProvider {
    inner: Arc<dyn atomcode_core::provider::LlmProvider>,
    context_window: u32,
}

#[async_trait::async_trait]
impl atomcode_kernel::provider::LlmProvider for KernelSummaryProvider {
    fn model_name(&self) -> &str {
        self.inner.model_name()
    }

    fn context_window(&self) -> u32 {
        self.context_window
    }

    async fn chat_stream(
        &self,
        messages: &[atomcode_kernel::message::Message],
        _tools: &[atomcode_kernel::tool::ToolDef],
        _options: &atomcode_kernel::provider::ChatOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, atomcode_kernel::stream::StreamEvent>,
        atomcode_kernel::stream::ProviderError,
    > {
        let messages: Vec<_> = messages.iter().map(message_to_core).collect();
        let stream = self.inner.chat_stream(&messages, None).map_err(|error| {
            atomcode_kernel::stream::ProviderError {
                message: error.to_string(),
                ..Default::default()
            }
        })?;
        Ok(stream
            .filter_map(|event| async move {
                use atomcode_core::stream::StreamEvent as Core;
                use atomcode_kernel::stream::{ProviderError, StreamEvent as Kernel, TokenUsage};
                match event {
                    Ok(Core::Delta(text)) => Some(Kernel::TextDelta(text)),
                    Ok(Core::Reasoning(text)) => Some(Kernel::Reasoning(text)),
                    Ok(Core::Usage(usage)) => Some(Kernel::Usage(TokenUsage {
                        prompt: usage.prompt_tokens as u32,
                        completion: usage.completion_tokens as u32,
                        cached: usage.cached_tokens as u32,
                    })),
                    Ok(Core::Done { truncated }) => Some(Kernel::Done { truncated }),
                    Ok(Core::Error(message)) => Some(Kernel::Error(ProviderError {
                        message: message.to_string(),
                        ..Default::default()
                    })),
                    Err(error) => Some(Kernel::Error(ProviderError {
                        message: error.to_string(),
                        ..Default::default()
                    })),
                    _ => None,
                }
            })
            .boxed())
    }
}

fn message_to_kernel(
    message: &atomcode_core::conversation::message::Message,
) -> atomcode_kernel::message::Message {
    use atomcode_core::conversation::message::{MessageContent, Role};
    use atomcode_kernel::message::{ImageContent, Message, Role as KernelRole};
    let mut converted = match &message.content {
        MessageContent::Text(text) => {
            let mut output = Message::user(text.clone());
            output.role = match message.role {
                Role::System => KernelRole::System,
                Role::User => KernelRole::User,
                Role::Assistant => KernelRole::Assistant,
                Role::Tool => KernelRole::Tool,
            };
            output
        }
        MessageContent::AssistantWithToolCalls {
            text,
            tool_calls,
            reasoning_content,
            thinking_blocks,
        } => {
            let calls = tool_calls.iter().map(|call| atomcode_kernel::tool::ToolCall {
                id: call.id.clone(), name: call.name.clone(), arguments: call.arguments.clone(),
            }).collect();
            let mut output = Message::assistant(text.clone().unwrap_or_default(), calls);
            output.reasoning = reasoning_content.clone();
            output.reasoning_blocks = thinking_blocks
                .iter()
                .map(|block| atomcode_kernel::message::ReasoningBlock {
                    text: block.text.clone(),
                    opaque: Some(block.signature.clone()),
                    provider: Some("anthropic".into()),
                })
                .collect();
            output
        }
        MessageContent::ToolResult(result) =>
            Message::tool_result(result.call_id.clone(), result.output.clone(), !result.success),
        MessageContent::ToolResultRef(result) =>
            Message::tool_result(result.call_id.clone(), result.summary.clone(), !result.success),
        MessageContent::MultiPart { text, images } => Message::user_with_images(
            text.clone().unwrap_or_default(),
            images.iter().map(|image| ImageContent {
                media_type: image.media_type.clone(), data: image.data.clone(),
            }).collect(),
        ),
    };
    converted.synthetic = message.synthetic;
    converted.internal_origin = message.internal_origin.clone();
    converted
}

pub(crate) fn message_to_core(
    message: &atomcode_kernel::message::Message,
) -> atomcode_core::conversation::message::Message {
    use atomcode_core::conversation::message::{ImagePart, Message, MessageContent, Role};
    use atomcode_kernel::message::Role as KernelRole;
    let role = match message.role {
        KernelRole::System => Role::System,
        KernelRole::User => Role::User,
        KernelRole::Assistant => Role::Assistant,
        KernelRole::Tool => Role::Tool,
    };
    let content = if message.role == KernelRole::Tool {
        MessageContent::ToolResult(atomcode_core::tool::ToolResult {
            call_id: message.tool_call_id.clone().unwrap_or_default(),
            output: message.text.clone(), success: !message.is_error,
        })
    } else if !message.tool_calls.is_empty() {
        MessageContent::AssistantWithToolCalls {
            text: (!message.text.is_empty()).then(|| message.text.clone()),
            tool_calls: message.tool_calls.iter().map(|call| atomcode_core::tool::ToolCall {
                id: call.id.clone(), name: call.name.clone(), arguments: call.arguments.clone(),
            }).collect(),
            reasoning_content: message.reasoning.clone(),
            thinking_blocks: message
                .reasoning_blocks
                .iter()
                .map(|block| atomcode_core::conversation::message::ThinkingBlock {
                    text: block.text.clone(),
                    signature: block.opaque.clone().unwrap_or_default(),
                })
                .collect(),
        }
    } else if !message.images.is_empty() {
        MessageContent::MultiPart {
            text: (!message.text.is_empty()).then(|| message.text.clone()),
            images: message.images.iter().map(|image| ImagePart {
                media_type: image.media_type.clone(), data: image.data.clone(),
            }).collect(),
        }
    } else {
        MessageContent::Text(message.text.clone())
    };
    Message { role, content, synthetic: message.synthetic, internal_origin: message.internal_origin.clone() }
}

fn legacy_cold_summary_message(
    summary: &str,
) -> atomcode_core::conversation::message::Message {
    use atomcode_core::conversation::{
        message::{Message, Role},
        LEGACY_COLD_SUMMARY_ORIGIN, LEGACY_COLD_SUMMARY_PREFIX,
    };

    let mut message = Message::new(
        Role::User,
        format!("{LEGACY_COLD_SUMMARY_PREFIX}{summary}"),
    );
    message.synthetic = true;
    message.internal_origin = Some(LEGACY_COLD_SUMMARY_ORIGIN.into());
    message
}

fn split_legacy_cold_summary_messages(
    messages: Vec<atomcode_core::conversation::message::Message>,
) -> ConversationSnapshot {
    use atomcode_core::conversation::{
        LEGACY_COLD_SUMMARY_ORIGIN, LEGACY_COLD_SUMMARY_PREFIX,
    };

    let mut recent = Vec::with_capacity(messages.len());
    let mut cold_summaries = Vec::new();
    for message in messages {
        if message.internal_origin.as_deref() == Some(LEGACY_COLD_SUMMARY_ORIGIN) {
            if let Some(summary) = message
                .text()
                .and_then(|text| text.strip_prefix(LEGACY_COLD_SUMMARY_PREFIX))
            {
                cold_summaries.push(summary.to_string());
                continue;
            }
        }
        recent.push(message);
    }
    ConversationSnapshot {
        messages: recent,
        cold_summaries,
    }
}

fn is_legacy_cold_summary_message(message: &atomcode_kernel::message::Message) -> bool {
    use atomcode_core::conversation::{
        LEGACY_COLD_SUMMARY_ORIGIN, LEGACY_COLD_SUMMARY_PREFIX,
    };

    message.internal_origin.as_deref() == Some(LEGACY_COLD_SUMMARY_ORIGIN)
        && message.text.starts_with(LEGACY_COLD_SUMMARY_PREFIX)
}

fn adjust_compaction_mutation_for_cold_summaries(
    mutation: atomcode_coding::runtime::SnapshotCompactionMutation,
    before: &[atomcode_kernel::message::Message],
    after: &[atomcode_kernel::message::Message],
) -> atomcode_coding::runtime::SnapshotCompactionMutation {
    use atomcode_coding::runtime::SnapshotCompactionMutation;

    let SnapshotCompactionMutation::Replace {
        old_start,
        old_end,
        new_end,
    } = mutation
    else {
        return mutation;
    };
    let visible_before = |end: usize| {
        before
            .iter()
            .take(end)
            .filter(|message| !is_legacy_cold_summary_message(message))
            .count()
    };
    let visible_after = after
        .iter()
        .take(new_end)
        .filter(|message| !is_legacy_cold_summary_message(message))
        .count();
    SnapshotCompactionMutation::Replace {
        old_start: visible_before(old_start),
        old_end: visible_before(old_end),
        new_end: visible_after,
    }
}

fn update_core_message_text(
    message: &mut atomcode_core::conversation::message::Message,
    compacted: &atomcode_kernel::message::Message,
) {
    use atomcode_core::conversation::message::MessageContent;
    match &mut message.content {
        MessageContent::Text(text) => *text = compacted.text.clone(),
        MessageContent::AssistantWithToolCalls { text, reasoning_content, .. } => {
            *text = (!compacted.text.is_empty()).then(|| compacted.text.clone());
            *reasoning_content = compacted.reasoning.clone();
        }
        MessageContent::ToolResult(result) => result.output = compacted.text.clone(),
        MessageContent::ToolResultRef(result) => result.summary = compacted.text.clone(),
        MessageContent::MultiPart { text, .. } => {
            *text = (!compacted.text.is_empty()).then(|| compacted.text.clone());
        }
    }
    message.synthetic = compacted.synthetic;
    message.internal_origin = compacted.internal_origin.clone();
}

fn merge_compacted_messages(
    original: Vec<atomcode_core::conversation::message::Message>,
    before: &[atomcode_kernel::message::Message],
    after: &[atomcode_kernel::message::Message],
    mutation: atomcode_coding::runtime::SnapshotCompactionMutation,
) -> Vec<atomcode_core::conversation::message::Message> {
    use atomcode_coding::runtime::SnapshotCompactionMutation;

    match mutation {
        SnapshotCompactionMutation::Noop => original,
        SnapshotCompactionMutation::RewriteOnly => {
            let common = original.len().min(after.len());
            let mut merged = Vec::with_capacity(after.len());
            for (index, mut message) in original.into_iter().take(common).enumerate() {
                if before.get(index) != after.get(index) {
                    update_core_message_text(&mut message, &after[index]);
                }
                merged.push(message);
            }
            merged.extend(after[common..].iter().map(message_to_core));
            merged
        }
        SnapshotCompactionMutation::Replace { old_start, old_end, new_end } => {
            let mut original = original;
            let suffix = original.split_off(old_end.min(original.len()));
            original.truncate(old_start.min(original.len()));
            original.extend(
                after[old_start.min(after.len())..new_end.min(after.len())]
                    .iter()
                    .map(message_to_core),
            );
            original.extend(suffix);
            original
        }
    }
}

/// 撤销会话最后若干轮（arg 空 = 最后一轮；否则回退到第 arg 个用户提示之前——对齐 TUI /undo）。
/// 就地修改 session.messages / cold_summaries / display_messages / turn_stats，
/// 返回被移除的提示数。纯内存，无磁盘/env 依赖。
pub(crate) fn apply_undo(session: &mut Session, arg: &str) -> usize {
    let snapshot = ConversationSnapshot {
        messages: std::mem::take(&mut session.messages),
        cold_summaries: session.cold_summaries.clone(),
    };
    let mut conv = Conversation::from_snapshot(snapshot);
    let available = conv.prompt_count();
    if available == 0 {
        let s = conv.snapshot();
        session.messages = s.messages;
        return 0;
    }
    let target = arg.trim().parse::<usize>().ok().unwrap_or(available);
    let before = conv.prompt_count();
    conv.undo_to_prompt(target);
    let after = conv.prompt_count();
    let s = conv.snapshot();
    session.messages = s.messages;
    session.cold_summaries = s.cold_summaries;
    let undone = before.saturating_sub(after);
    if undone > 0 {
        // Prune display_messages and turn_stats that reference removed turns.
        prune_orphaned_display(session);
    }
    undone
}

/// 会话 messages 变短后，裁掉锚点越界的 UI 附加消息与轮次统计，避免被撤销/压缩掉的
/// 回合的通知重现、上下文表尺读到过期 turn_stat。
fn prune_orphaned_display(session: &mut Session) {
    let n = session.messages.len();
    session.display_messages.retain(|d| d.after_message <= n);
    session.turn_stats.retain(|t| t.after_message <= n);
}

/// Re-index UI anchors after kernel compaction replaces one contiguous old span
/// `[old_start, old_end)` with `[old_start, new_end)` (normally one summary).
fn reindex_after_compaction(
    session: &mut Session,
    old_start: usize,
    old_end: usize,
    new_end: usize,
) {
    session.display_messages.retain_mut(|d| {
        if d.after_message <= old_start {
            true
        } else if d.after_message < old_end {
            false
        } else {
            d.after_message = new_end + d.after_message.saturating_sub(old_end);
            true
        }
    });
    session.turn_stats.retain_mut(|t| {
        if t.after_message > old_start && t.after_message < old_end {
            false
        } else {
            if t.after_message >= old_end {
                t.after_message = new_end + t.after_message.saturating_sub(old_end);
            }
            true
        }
    });
}

fn reindex_after_snapshot_compaction(
    session: &mut Session,
    mutation: atomcode_coding::runtime::SnapshotCompactionMutation,
) {
    if let atomcode_coding::runtime::SnapshotCompactionMutation::Replace {
        old_start,
        old_end,
        new_end,
    } = mutation
    {
        reindex_after_compaction(session, old_start, old_end, new_end);
    }
}

/// 保存到与加载时相同的桶：若有 project_hash 则写 project_hash 桶，否则按 working_dir 桶。
/// 与 load_command_session 严格对称，防止 undo/compact 写入不同桶产生幽灵副本。
fn save_command_session(session: &mut Session, project_hash: Option<&str>) -> anyhow::Result<()> {
    session.touch();
    match project_hash {
        Some(hash) => crate::save_session_to_hash(hash, session)?,
        None => SessionManager::new(&session.working_dir).save(session)?,
    }
    Ok(())
}

#[derive(serde::Deserialize)]
pub(crate) struct CommandReq {
    pub command: String,
    #[serde(default)]
    pub arg: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub project_hash: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
}

#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum CommandResult {
    Undo {
        undone: usize,
    },
    Remember {
        scope: String,
    },
    Forget {
        removed: Vec<String>,
    },
    Memory {
        global: Vec<String>,
        project: Vec<String>,
    },
    Context {
        system_tokens: usize,
        sent_tokens: usize,
        total_messages: usize,
        tool_defs_tokens: usize,
        cold_zone_tokens: usize,
        ctx_window: usize,
        ctx_name: String,
    },
    Compact {
        applied: bool,
        removed_messages: usize,
        before_tokens: usize,
        after_tokens: usize,
    },
    Whoami {
        logged_in: bool,
        username: Option<String>,
        name: Option<String>,
        email: Option<String>,
    },
    Status {
        logged_in: bool,
        username: Option<String>,
        provider: String,
        model: String,
        working_dir: String,
        config_path: String,
        text: String,
    },
    Config {
        path: String,
        provider: String,
    },
    Diff {
        stat: String,
    },
    Cost {
        total_tokens: usize,
        turn_count: usize,
    },
    Todo {
        items: Vec<TodoItemJson>,
    },
    Error {
        message: String,
    },
}

#[derive(serde::Serialize)]
pub(crate) struct TodoItemJson {
    pub status: String,
    pub content: String,
}

/// 按会话真实桶加载：优先 project_hash（跨 /cd 稳定），否则回退到 working_dir。
fn load_command_session(
    working_dir: &std::path::Path,
    project_hash: Option<&str>,
    session_id: &SessionId,
) -> anyhow::Result<Session> {
    if let Some(hash) = project_hash {
        Ok(crate::load_session(hash, session_id.as_str())?)
    } else {
        Ok(SessionManager::new(working_dir).load(session_id)?)
    }
}

fn exec_undo(
    working_dir: &Path,
    session_id: Option<&str>,
    arg: &str,
    project_hash: Option<&str>,
) -> anyhow::Result<CommandResult> {
    let sid = session_id.ok_or_else(|| anyhow::anyhow!("session_id required for undo"))?;
    let session_id = SessionId::from_string(sid.to_string());
    let mut session = load_command_session(working_dir, project_hash, &session_id)?;
    let undone = apply_undo(&mut session, arg);
    if undone > 0 {
        save_command_session(&mut session, project_hash)?;
    }
    Ok(CommandResult::Undo { undone })
}

async fn exec_context(
    state: &AppState,
    working_dir: &std::path::Path,
    project_hash: Option<&str>,
    session_id: Option<&str>,
    provider: Option<&str>,
) -> anyhow::Result<CommandResult> {
    let sid = session_id.ok_or_else(|| anyhow::anyhow!("session_id required for context"))?;
    let session_id = SessionId::from_string(sid.to_string());
    let session = load_command_session(working_dir, project_hash, &session_id)?;
    let parts = crate::live_api::build_turn_parts(
        working_dir,
        provider,
        &state.mcp_cache,
        state.telemetry.clone(),
    )
    .await?;
    let conv = Conversation::from_snapshot(ConversationSnapshot {
        messages: session.messages.clone(),
        cold_summaries: session.cold_summaries.clone(),
    });
    let (msgs, _) = parts.ctx.build_messages(&conv, &parts.system_prompt, "");
    let s = atomcode_core::ctx::compute_rich_context_stats(
        &conv,
        &msgs,
        &parts.tools,
        &*parts.ctx,
    )
    .await;
    Ok(CommandResult::Context {
        system_tokens: s.system_tokens,
        sent_tokens: s.sent_tokens,
        total_messages: s.total_messages,
        tool_defs_tokens: s.tool_defs_tokens,
        cold_zone_tokens: s.cold_zone_tokens,
        ctx_window: s.ctx_window,
        ctx_name: s.ctx_name,
    })
}

/// 解析 `/remember` 参数：可选前缀 `--global`。返回 (是否全局, 去掉前缀并 trim 后的内容)。
pub(crate) fn parse_remember_arg(arg: &str) -> (bool, &str) {
    let arg = arg.trim();
    if let Some(rest) = arg.strip_prefix("--global") {
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            return (true, rest.trim());
        }
    }
    (false, arg)
}

fn exec_remember(working_dir: &Path, arg: &str) -> anyhow::Result<CommandResult> {
    let (global, content) = parse_remember_arg(arg);
    if content.is_empty() {
        anyhow::bail!("remember needs content");
    }
    let store = if global {
        MemoryStore::global()
    } else {
        MemoryStore::project(working_dir)
    };
    store.append(content)?;
    Ok(CommandResult::Remember {
        scope: if global { "global" } else { "project" }.to_string(),
    })
}

fn exec_forget(working_dir: &Path, arg: &str) -> anyhow::Result<CommandResult> {
    let keyword = arg.trim();
    if keyword.is_empty() {
        anyhow::bail!("forget needs a keyword");
    }
    let mut removed = MemoryStore::global().remove_matching(keyword)?;
    removed.extend(MemoryStore::project(working_dir).remove_matching(keyword)?);
    Ok(CommandResult::Forget { removed })
}

fn exec_memory(working_dir: &Path) -> anyhow::Result<CommandResult> {
    Ok(CommandResult::Memory {
        global: MemoryStore::global().load(),
        project: MemoryStore::project(working_dir).load(),
    })
}

async fn exec_compact(
    state: &AppState,
    working_dir: &std::path::Path,
    project_hash: Option<&str>,
    session_id: Option<&str>,
    provider: Option<&str>,
    arg: &str,
) -> anyhow::Result<CommandResult> {
    let sid = session_id.ok_or_else(|| anyhow::anyhow!("session_id required for compact"))?;
    let session_id = SessionId::from_string(sid.to_string());
    let mut session = load_command_session(working_dir, project_hash, &session_id)?;
    let config = atomcode_config::config::Config::load(
        &atomcode_config::config::Config::default_path(),
    )?;
    let provider_name = provider.unwrap_or(&config.default_provider);
    let context_window = config
        .providers
        .get(provider_name)
        .ok_or_else(|| anyhow::anyhow!("Provider '{}' not found", provider_name))?
        .context_window as u32;
    let parts = crate::live_api::build_turn_parts(
        working_dir,
        provider,
        &state.mcp_cache,
        state.telemetry.clone(),
    )
    .await?;

    let provider = Arc::new(KernelSummaryProvider {
        inner: parts.provider,
        context_window,
    });
    let mut original_messages: Vec<_> = std::mem::take(&mut session.cold_summaries)
        .into_iter()
        .map(|summary| legacy_cold_summary_message(&summary))
        .collect();
    original_messages.append(&mut session.messages);
    let messages: Vec<_> = original_messages
        .iter()
        .map(message_to_kernel)
        .collect();
    let before_messages = messages.clone();
    let compacted = atomcode_coding::runtime::compact_snapshot(
        messages,
        provider,
        (!arg.trim().is_empty()).then(|| arg.trim().to_string()),
    )
    .await;
    let adjusted_mutation = adjust_compaction_mutation_for_cold_summaries(
        compacted.mutation,
        &before_messages,
        &compacted.messages,
    );
    let merged = merge_compacted_messages(
        original_messages,
        &before_messages,
        &compacted.messages,
        compacted.mutation,
    );
    let snapshot = split_legacy_cold_summary_messages(merged);
    session.update_from_conversation_snapshot(snapshot);
    if compacted.outcome.committed {
        reindex_after_snapshot_compaction(&mut session, adjusted_mutation);
        save_command_session(&mut session, project_hash)?;
    }

    Ok(CommandResult::Compact {
        applied: compacted.outcome.committed,
        removed_messages: compacted.outcome.removed_messages,
        before_tokens: compacted.outcome.estimated_tokens_before,
        after_tokens: compacted.outcome.estimated_tokens_after,
    })
}

fn exec_whoami() -> anyhow::Result<CommandResult> {
    match atomcode_core::auth::get_stored_auth() {
        Some(auth) => Ok(CommandResult::Whoami {
            logged_in: true,
            username: Some(auth.user.username),
            name: auth.user.name,
            email: auth.user.email,
        }),
        None => Ok(CommandResult::Whoami {
            logged_in: false,
            username: None,
            name: None,
            email: None,
        }),
    }
}

fn exec_config() -> anyhow::Result<CommandResult> {
    let path = atomcode_config::config::Config::default_path();
    let provider = atomcode_config::config::Config::load(&path)
        .map(|c| c.default_provider)
        .unwrap_or_default();
    Ok(CommandResult::Config {
        path: path.display().to_string(),
        provider,
    })
}

fn exec_diff(working_dir: &std::path::Path) -> anyhow::Result<CommandResult> {
    let out = std::process::Command::new("git")
        .args(["diff", "--stat"])
        .current_dir(working_dir)
        .output()?;
    let stat = if out.status.success() {
        String::from_utf8_lossy(&out.stdout).trim_end().to_string()
    } else {
        String::from_utf8_lossy(&out.stderr).trim_end().to_string()
    };
    Ok(CommandResult::Diff { stat })
}

fn render_instruction_status_block(working_dir: &std::path::Path) -> String {
    use atomcode_config::config::instructions::LayeredInstructions;
    use atomcode_config::i18n::{t, Msg};
    let instructions = LayeredInstructions::load(working_dir);
    let mut out = t(Msg::StatusInstructionFilesHeader).into_owned();
    for (level, path) in instructions.status_lines() {
        match path {
            Some(p) => out.push_str(&t(Msg::StatusInstructionPresent {
                path: &p.display().to_string(),
                label: level.label(),
            })),
            None => out.push_str(&t(Msg::StatusInstructionMissing {
                label: level.label(),
            })),
        }
    }
    out
}

fn render_login_line(user: Option<&str>) -> String {
    use atomcode_config::i18n::{t, Msg};
    match user {
        Some(u) => t(Msg::StatusLoginLoggedIn { user: u }).into_owned(),
        None => t(Msg::StatusLoginNotSignedIn).into_owned(),
    }
}

fn format_login_identity(name: Option<&str>, username: &str) -> String {
    match name.map(str::trim).filter(|n| !n.is_empty() && *n != username) {
        Some(n) => format!("{n}({username})"),
        None => username.to_string(),
    }
}

fn render_login_line_from_stored_auth() -> String {
    match atomcode_core::auth::get_stored_auth() {
        Some(a) => {
            let identity = format_login_identity(a.user.name.as_deref(), &a.user.username);
            render_login_line(Some(&identity))
        }
        None => render_login_line(None),
    }
}

fn render_cp_auth_error(e: &anyhow::Error, fallback: impl FnOnce() -> String) -> String {
    use atomcode_codingplan::is_auth_expired;
    use atomcode_config::i18n::{t, Msg};
    if is_auth_expired(e) {
        t(Msg::StatusCpAuthExpired).into_owned()
    } else {
        fallback()
    }
}

fn render_codingplan_status_for_status_cmd() -> String {
    tokio::task::block_in_place(|| {
        use atomcode_codingplan::setup::format_duration_secs;
        use atomcode_codingplan::Client;
        use atomcode_config::i18n::{t, Msg};

        let client = match Client::from_stored_auth() {
            Ok(c) => c,
            Err(e) => return render_cp_auth_error(&e, || t(Msg::StatusCpNotSignedIn).into_owned()),
        };
        let status = match client.status_v2() {
            Ok(s) => s,
            Err(e) => {
                return render_cp_auth_error(&e, || {
                    t(Msg::StatusCpFetchFailed {
                        error: &format!("{:#}", e),
                    })
                    .into_owned()
                })
            }
        };
        let plan = match &status.codingplan_free {
            Some(p) => p,
            None => {
                return t(Msg::StatusCpNoActive).into_owned();
            }
        };

        let mut out = t(Msg::StatusCpLine {
            plan: &plan.plan_name,
            expires_at: &plan.expires_at,
            remaining_days: plan.remaining_days,
            total_days: plan.total_days,
        })
        .into_owned();
        if !status.rate_limit_windows.is_empty() {
            for w in status
                .rate_limit_windows
                .iter()
                .filter(|w| w.show_enable == 1)
            {
                out.push_str(&t(Msg::StatusCpUsage {
                    usage: &w.usage_status_desc,
                    reset_at: &w.reset_at_display,
                    duration: &format_duration_secs(w.seconds_until_reset),
                }));
            }
        } else if status.window_quota_exhausted {
            if let Some(hint) = &status.window_quota_hint {
                out.push_str(&t(Msg::StatusCpWindowHint { hint }));
            } else {
                out.push_str(&t(Msg::StatusCpWindowExhausted));
            }
        } else if let Some(u) = &status.current_usage {
            out.push_str(&t(Msg::StatusCpUsage {
                usage: &u.display_desc(),
                reset_at: &u.reset_at_display,
                duration: &format_duration_secs(u.seconds_until_reset),
            }));
        }
        out
    })
}

fn assemble_status(
    login: &str,
    body: &str,
    codingplan: &str,
    proxy: &str,
    instructions: &str,
) -> String {
    let mut txt = String::with_capacity(
        login.len() + body.len() + codingplan.len() + proxy.len() + instructions.len() + 16,
    );
    txt.push_str(login);
    txt.push_str(body);
    txt.push_str(codingplan);
    txt.push_str(proxy);
    txt.push('\n');
    txt.push_str(instructions);
    txt
}

fn exec_status(
    working_dir: &std::path::Path,
    provider: Option<&str>,
) -> anyhow::Result<CommandResult> {
    use atomcode_config::i18n::{t, Msg};
    let config_path = atomcode_config::config::Config::default_path();
    let config = atomcode_config::config::Config::load(&config_path).ok();
    let provider_name = provider
        .map(|s| s.to_string())
        .or_else(|| config.as_ref().map(|c| c.default_provider.clone()))
        .unwrap_or_default();
    let model = config
        .as_ref()
        .and_then(|c| c.providers.get(&provider_name))
        .map(|p| p.model.clone())
        .unwrap_or_default();
    let auth = atomcode_core::auth::get_stored_auth();

    let body = t(Msg::StatusBody {
        model: &model,
        dir: &working_dir.display().to_string(),
        config: &config_path.display().to_string(),
    })
    .into_owned();
    let proxy_summary = config
        .as_ref()
        .map(|c| c.network.proxy.summary())
        .unwrap_or_else(|| "follow_system".to_string());
    let proxy_line = format!("  Proxy:  {}\n", proxy_summary);

    let text = assemble_status(
        &render_login_line_from_stored_auth(),
        &body,
        &render_codingplan_status_for_status_cmd(),
        &proxy_line,
        &render_instruction_status_block(working_dir),
    );

    Ok(CommandResult::Status {
        logged_in: auth.is_some(),
        username: auth.map(|a| a.user.username),
        provider: provider_name,
        model,
        working_dir: working_dir.display().to_string(),
        config_path: config_path.display().to_string(),
        text,
    })
}

fn exec_cost(
    working_dir: &std::path::Path,
    project_hash: Option<&str>,
    session_id: Option<&str>,
) -> anyhow::Result<CommandResult> {
    let sid = session_id.ok_or_else(|| anyhow::anyhow!("session_id required for cost"))?;
    let session = load_command_session(
        working_dir,
        project_hash,
        &SessionId::from_string(sid.to_string()),
    )?;
    // TurnStat.total_tokens stores the per-turn token count (reset to 0 at turn start,
    // accumulated during the turn, saved at TurnComplete). Summing gives session total.
    let total_tokens: usize = session.turn_stats.iter().map(|t| t.total_tokens).sum();
    let turn_count = session.turn_stats.len();
    Ok(CommandResult::Cost {
        total_tokens,
        turn_count,
    })
}

fn exec_todo(
    working_dir: &std::path::Path,
    project_hash: Option<&str>,
    session_id: Option<&str>,
) -> anyhow::Result<CommandResult> {
    let sid = session_id.ok_or_else(|| anyhow::anyhow!("session_id required for todo"))?;
    let session = load_command_session(
        working_dir,
        project_hash,
        &SessionId::from_string(sid.to_string()),
    )?;

    // `derive_current_todos` takes kernel messages, but `reduce_todos` folds a message-agnostic
    // `(tool_name, args)` stream — so we map core messages to that and fold via the CANONICAL
    // reducer (baseline = last full-list plan, then apply every `{action}` update after it).
    // This shows CURRENT statuses in `/todo`, matching the merged `todowrite` tool + the TUI.
    use atomcode_capabilities::tools::todo::{reduce_todos, TodoStatus};
    use atomcode_core::conversation::message::MessageContent;

    let calls: Vec<(&str, &str)> = session
        .messages
        .iter()
        .filter_map(|m| match &m.content {
            MessageContent::AssistantWithToolCalls { tool_calls, .. } => Some(tool_calls),
            _ => None,
        })
        .flat_map(|tcs| tcs.iter().map(|c| (c.name.as_str(), c.arguments.as_str())))
        .collect();
    let todos = reduce_todos(calls);

    let items = todos
        .into_iter()
        .map(|t| TodoItemJson {
            status: match t.status {
                TodoStatus::Pending => "pending",
                TodoStatus::InProgress => "in_progress",
                TodoStatus::Completed => "completed",
            }
            .to_string(),
            content: t.content,
        })
        .collect();

    Ok(CommandResult::Todo { items })
}

pub(crate) async fn run_command(
    State(state): State<AppState>,
    Json(req): Json<CommandReq>,
) -> impl IntoResponse {
    let working_dir = match req.working_dir.as_deref() {
        Some(d) if !d.is_empty() => std::path::PathBuf::from(d),
        _ => {
            return Json(CommandResult::Error {
                message: "working_dir required".into(),
            })
        }
    };
    let result = match req.command.as_str() {
        "undo" => exec_undo(
            &working_dir,
            req.session_id.as_deref(),
            &req.arg,
            req.project_hash.as_deref(),
        ),
        "remember" => exec_remember(&working_dir, &req.arg),
        "forget" => exec_forget(&working_dir, &req.arg),
        "memory" => exec_memory(&working_dir),
        "context" => {
            exec_context(
                &state,
                &working_dir,
                req.project_hash.as_deref(),
                req.session_id.as_deref(),
                req.provider.as_deref(),
            )
            .await
        }
        "compact" => {
            exec_compact(
                &state,
                &working_dir,
                req.project_hash.as_deref(),
                req.session_id.as_deref(),
                req.provider.as_deref(),
                &req.arg,
            )
            .await
        }
        "whoami" => exec_whoami(),
        "config" => exec_config(),
        "diff" => exec_diff(&working_dir),
        "status" => exec_status(&working_dir, req.provider.as_deref()),
        "cost" => exec_cost(
            &working_dir,
            req.project_hash.as_deref(),
            req.session_id.as_deref(),
        ),
        "todo" => exec_todo(
            &working_dir,
            req.project_hash.as_deref(),
            req.session_id.as_deref(),
        ),
        other => Err(anyhow::anyhow!("unknown command: {other}")),
    };
    match result {
        Ok(r) => Json(r),
        Err(e) => Json(CommandResult::Error {
            message: e.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_config::config::memory::MemoryStore;
    use atomcode_core::conversation::message::{Message, Role};
    use atomcode_core::session::{DisplayMessage, TurnStat};

    fn session_with_turns(n: usize) -> Session {
        let mut s = Session::new(std::path::PathBuf::from("/tmp/plan2-test"));
        for i in 0..n {
            s.messages.push(Message::new(Role::User, &format!("q{i}")));
            s.messages
                .push(Message::new(Role::Assistant, &format!("a{i}")));
        }
        s
    }

    #[test]
    fn compact_persistence_conversion_preserves_tool_pair() {
        let assistant = Message {
            role: Role::Assistant,
            content: atomcode_core::conversation::message::MessageContent::AssistantWithToolCalls {
                text: Some("checking".into()),
                tool_calls: vec![atomcode_core::tool::ToolCall {
                    id: "call-1".into(),
                    name: "read_file".into(),
                    arguments: "{\"path\":\"a.rs\"}".into(),
                }],
                reasoning_content: Some("reason".into()),
                thinking_blocks: vec![
                    atomcode_core::conversation::message::ThinkingBlock {
                        text: "thinking".into(),
                        signature: "signature".into(),
                    },
                ],
            },
            synthetic: false,
            internal_origin: None,
        };
        let result = Message {
            role: Role::Tool,
            content: atomcode_core::conversation::message::MessageContent::ToolResult(
                atomcode_core::tool::ToolResult {
                    call_id: "call-1".into(),
                    output: "file body".into(),
                    success: true,
                },
            ),
            synthetic: false,
            internal_origin: None,
        };

        let assistant_roundtrip = message_to_core(&message_to_kernel(&assistant));
        let result_roundtrip = message_to_core(&message_to_kernel(&result));

        match assistant_roundtrip.content {
            atomcode_core::conversation::message::MessageContent::AssistantWithToolCalls {
                tool_calls,
                reasoning_content,
                thinking_blocks,
                ..
            } => {
                assert_eq!(tool_calls[0].id, "call-1");
                assert_eq!(tool_calls[0].name, "read_file");
                assert_eq!(reasoning_content.as_deref(), Some("reason"));
                assert_eq!(thinking_blocks[0].signature, "signature");
            }
            _ => panic!("assistant tool call shape was not preserved"),
        }
        match result_roundtrip.content {
            atomcode_core::conversation::message::MessageContent::ToolResult(result) => {
                assert_eq!(result.call_id, "call-1");
                assert_eq!(result.output, "file body");
                assert!(result.success);
            }
            _ => panic!("tool result shape was not preserved"),
        }
    }

    #[test]
    fn cold_summaries_survive_conversion_and_do_not_shift_ui_anchor_indexes() {
        use atomcode_coding::runtime::SnapshotCompactionMutation;

        let mut core_before = vec![
            legacy_cold_summary_message("cold one"),
            legacy_cold_summary_message("cold two"),
        ];
        core_before.extend([
            Message::new(Role::User, "u1"),
            Message::new(Role::Assistant, "a1"),
            Message::new(Role::User, "u2"),
            Message::new(Role::Assistant, "a2"),
        ]);
        let before: Vec<_> = core_before.iter().map(message_to_kernel).collect();
        let mut after = vec![atomcode_kernel::message::Message::user("summary")];
        after.extend(before[4..].iter().cloned());

        let adjusted = adjust_compaction_mutation_for_cold_summaries(
            SnapshotCompactionMutation::Replace {
                old_start: 0,
                old_end: 4,
                new_end: 1,
            },
            &before,
            &after,
        );
        assert_eq!(
            adjusted,
            SnapshotCompactionMutation::Replace {
                old_start: 0,
                old_end: 2,
                new_end: 1,
            }
        );

        let split = split_legacy_cold_summary_messages(core_before);
        assert_eq!(split.cold_summaries, vec!["cold one", "cold two"]);
        assert_eq!(split.messages.len(), 4);
    }

    #[test]
    fn compact_rewrite_preserves_core_only_message_fields() {
        use atomcode_core::conversation::message::{MessageContent, ThinkingBlock};
        use atomcode_core::tool::result_store::ToolResultRef;

        let assistant = Message {
            role: Role::Assistant,
            content: MessageContent::AssistantWithToolCalls {
                text: Some("checking".into()),
                tool_calls: vec![atomcode_core::tool::ToolCall {
                    id: "call-1".into(),
                    name: "bash".into(),
                    arguments: "{}".into(),
                }],
                reasoning_content: Some("reason".into()),
                thinking_blocks: vec![ThinkingBlock {
                    text: "private reasoning".into(),
                    signature: "opaque-signature".into(),
                }],
            },
            synthetic: false,
            internal_origin: None,
        };
        let tool_ref = Message {
            role: Role::Tool,
            content: MessageContent::ToolResultRef(ToolResultRef {
                call_id: "call-1".into(),
                hash: "content-hash".into(),
                summary: "large output".into(),
                byte_size: 42_000,
                success: true,
            }),
            synthetic: false,
            internal_origin: None,
        };
        let original = vec![assistant, tool_ref];
        let before: Vec<_> = original.iter().map(message_to_kernel).collect();
        let mut after = before.clone();
        after[0].text = "updated assistant text".into();
        after[1].text = "[bash output compacted]".into();

        let merged = merge_compacted_messages(
            original,
            &before,
            &after,
            atomcode_coding::runtime::SnapshotCompactionMutation::RewriteOnly,
        );

        match &merged[0].content {
            MessageContent::AssistantWithToolCalls { text, thinking_blocks, .. } => {
                assert_eq!(text.as_deref(), Some("updated assistant text"));
                assert_eq!(thinking_blocks[0].signature, "opaque-signature");
            }
            _ => panic!("assistant shape changed"),
        }
        match &merged[1].content {
            MessageContent::ToolResultRef(result) => {
                assert_eq!(result.summary, "[bash output compacted]");
                assert_eq!(result.hash, "content-hash");
                assert_eq!(result.byte_size, 42_000);
            }
            _ => panic!("tool result reference was downgraded"),
        }
    }

    #[test]
    fn undo_no_arg_removes_last_turn() {
        let mut s = session_with_turns(3);
        let removed = apply_undo(&mut s, "");
        assert_eq!(removed, 1);
        // 3 用户提示 → 剩 2；每轮 user+assistant，剩 2 轮 = 4 条消息。
        let users = s
            .messages
            .iter()
            .filter(|m| matches!(m.role, Role::User))
            .count();
        assert_eq!(users, 2);
    }

    #[test]
    fn undo_to_prompt_1_removes_all() {
        let mut s = session_with_turns(3);
        let removed = apply_undo(&mut s, "1");
        assert_eq!(removed, 3);
        assert!(s.messages.is_empty());
    }

    #[test]
    fn undo_on_empty_session_is_noop() {
        let mut s = session_with_turns(0);
        assert_eq!(apply_undo(&mut s, ""), 0);
        assert!(s.messages.is_empty());
    }

    #[test]
    fn parse_remember_arg_detects_global() {
        assert_eq!(parse_remember_arg("--global 记住这个"), (true, "记住这个"));
        assert_eq!(parse_remember_arg("普通事实"), (false, "普通事实"));
        assert_eq!(
            parse_remember_arg("  --global   trimmed  "),
            (true, "trimmed")
        );
        assert_eq!(parse_remember_arg("--globalfoo"), (false, "--globalfoo"));
    }

    #[test]
    fn remember_then_memory_roundtrip_project_scope() {
        // hermetic：project 作用域写到 working_dir/.atomcode/memory.md
        let dir = tempfile::tempdir().unwrap();
        let wd = dir.path();
        exec_remember(wd, "阿童木用 Rust 写").unwrap();
        let store = MemoryStore::project(wd);
        assert!(store.load().iter().any(|e| e.contains("阿童木用 Rust 写")));
    }

    #[test]
    fn forget_removes_project_entry() {
        let dir = tempfile::tempdir().unwrap();
        let wd = dir.path();
        exec_remember(wd, "delete-me fact").unwrap();
        exec_remember(wd, "keep-me fact").unwrap();
        // exec_forget 也会扫全局，但全局此刻应无匹配；断言项目侧被删。
        let _ = exec_forget(wd, "delete-me");
        let remaining = MemoryStore::project(wd).load();
        assert!(!remaining.iter().any(|e| e.contains("delete-me")));
        assert!(remaining.iter().any(|e| e.contains("keep-me")));
    }

    #[test]
    fn reindex_after_compaction_preserves_prefix_and_shifts_suffix() {
        let mut s = session_with_turns(0);
        // after_message=0: "before the first message" — always kept.
        s.display_messages.push(DisplayMessage {
            after_message: 0,
            message: Message::new(Role::Assistant, "preamble"),
        });
        // after_message=2: within the drained range (<=3) — should be dropped.
        s.display_messages.push(DisplayMessage {
            after_message: 2,
            message: Message::new(Role::Assistant, "drained"),
        });
        // after_message=5: survivor — shifts to 5-3=2.
        s.display_messages.push(DisplayMessage {
            after_message: 5,
            message: Message::new(Role::Assistant, "keep"),
        });
        // turn_stat at 2: drained (<=3) — dropped.
        s.turn_stats.push(TurnStat {
            after_message: 2,
            turn_count: 1,
            tool_call_count: 0,
            duration_ms: 50,
            total_tokens: 5,
            errored: false,
            used_tokens: 0,
            ctx_window: 0,
        });
        // turn_stat at 6: survivor — shifts to 6-3=3.
        s.turn_stats.push(TurnStat {
            after_message: 6,
            turn_count: 1,
            tool_call_count: 0,
            duration_ms: 50,
            total_tokens: 5,
            errored: false,
            used_tokens: 0,
            ctx_window: 0,
        });
        // Replace old messages [1, 4) with one summary at [1, 2).
        reindex_after_compaction(&mut s, 1, 4, 2);
        assert_eq!(s.display_messages.len(), 2);
        assert_eq!(s.display_messages[0].after_message, 0);
        assert_eq!(s.display_messages[1].after_message, 3);
        assert_eq!(s.turn_stats.len(), 1);
        assert_eq!(s.turn_stats[0].after_message, 4);
    }

    #[test]
    fn rewrite_only_compaction_preserves_all_ui_anchors() {
        let mut s = session_with_turns(3);
        s.display_messages.push(DisplayMessage {
            after_message: 2,
            message: Message::new(Role::Assistant, "first rewrite boundary"),
        });
        s.display_messages.push(DisplayMessage {
            after_message: 5,
            message: Message::new(Role::Assistant, "between rewrites"),
        });
        s.turn_stats.push(TurnStat {
            after_message: 4,
            turn_count: 2,
            tool_call_count: 1,
            duration_ms: 50,
            total_tokens: 5,
            errored: false,
            used_tokens: 0,
            ctx_window: 0,
        });

        reindex_after_snapshot_compaction(
            &mut s,
            atomcode_coding::runtime::SnapshotCompactionMutation::RewriteOnly,
        );

        assert_eq!(s.display_messages.len(), 2);
        assert_eq!(s.display_messages[0].after_message, 2);
        assert_eq!(s.display_messages[1].after_message, 5);
        assert_eq!(s.turn_stats[0].after_message, 4);
    }

    #[test]
    fn cost_sums_turn_stats_tokens() {
        let mut s = Session::new(std::path::PathBuf::from("/tmp/cost-test"));
        s.turn_stats.push(TurnStat {
            after_message: 2,
            turn_count: 1,
            tool_call_count: 0,
            duration_ms: 100,
            total_tokens: 100,
            errored: false,
            used_tokens: 0,
            ctx_window: 0,
        });
        s.turn_stats.push(TurnStat {
            after_message: 4,
            turn_count: 1,
            tool_call_count: 0,
            duration_ms: 120,
            total_tokens: 250,
            errored: false,
            used_tokens: 0,
            ctx_window: 0,
        });
        let total: usize = s.turn_stats.iter().map(|t| t.total_tokens).sum();
        assert_eq!(total, 350);
        assert_eq!(s.turn_stats.len(), 2);
    }

    #[test]
    fn todo_derives_from_last_todowrite_call() {
        use atomcode_core::conversation::message::{MessageContent, Role};
        use atomcode_core::tool::ToolCall;

        let args = r#"{"todos":[{"content":"写测试","status":"in_progress"},{"content":"提交","status":"pending"}]}"#;
        let mut s = Session::new(std::path::PathBuf::from("/tmp/todo-test"));
        s.messages.push(Message {
            role: Role::Assistant,
            content: MessageContent::AssistantWithToolCalls {
                text: None,
                tool_calls: vec![ToolCall {
                    id: "call1".into(),
                    name: "todowrite".into(),
                    arguments: args.into(),
                }],
                reasoning_content: None,
                thinking_blocks: vec![],
            },
            synthetic: false,
            internal_origin: None,
        });

        // Inline the same derivation logic as exec_todo (core Message ≠ kernel Message).
        use atomcode_capabilities::tools::todo::parse_todos;
        let todos = s
            .messages
            .iter()
            .rev()
            .find_map(|m| {
                if let MessageContent::AssistantWithToolCalls { tool_calls, .. } = &m.content {
                    tool_calls
                        .iter()
                        .rev()
                        .filter(|c| c.name == "todowrite")
                        .find_map(|c| parse_todos(&c.arguments).ok())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        assert_eq!(todos.len(), 2);
        assert_eq!(todos[0].content, "写测试");
        assert_eq!(todos[1].content, "提交");
    }

    #[test]
    fn todo_empty_session_returns_empty() {
        use atomcode_capabilities::tools::todo::parse_todos;
        use atomcode_core::conversation::message::MessageContent;
        let s = Session::new(std::path::PathBuf::from("/tmp/todo-empty-test"));
        let todos: Vec<_> = s
            .messages
            .iter()
            .rev()
            .find_map(|m| {
                if let MessageContent::AssistantWithToolCalls { tool_calls, .. } = &m.content {
                    tool_calls
                        .iter()
                        .rev()
                        .filter(|c| c.name == "todowrite")
                        .find_map(|c| parse_todos(&c.arguments).ok())
                } else {
                    None
                }
            })
            .unwrap_or_default();
        assert!(todos.is_empty());
    }

    #[test]
    fn exec_diff_returns_stat_in_a_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        // init a repo with one committed file + a working-tree change
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap()
        };
        run(&["init"]);
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        run(&["add", "a.txt"]);
        run(&["commit", "-m", "init"]);
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        let res = exec_diff(dir.path()).unwrap();
        match res {
            CommandResult::Diff { stat } => assert!(stat.contains("a.txt"), "stat was: {stat}"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn undo_prunes_display_messages_and_turn_stats() {
        // 3 turns = 6 messages. After undo 1 turn → 4 messages remain.
        // display_messages/turn_stats anchored at <=4 survive; >4 are pruned.
        let mut s = session_with_turns(3);
        // Anchored at message 2 (inside surviving turns) — should survive.
        s.display_messages.push(DisplayMessage {
            after_message: 2,
            message: Message::new(Role::Assistant, "keep"),
        });
        // Anchored at message 6 (inside the removed turn) — should be pruned.
        s.display_messages.push(DisplayMessage {
            after_message: 6,
            message: Message::new(Role::Assistant, "drop"),
        });
        s.turn_stats.push(TurnStat {
            after_message: 4,
            turn_count: 1,
            tool_call_count: 0,
            duration_ms: 100,
            total_tokens: 10,
            errored: false,
            used_tokens: 0,
            ctx_window: 0,
        });
        s.turn_stats.push(TurnStat {
            after_message: 6,
            turn_count: 1,
            tool_call_count: 0,
            duration_ms: 100,
            total_tokens: 10,
            errored: false,
            used_tokens: 0,
            ctx_window: 0,
        });
        let removed = apply_undo(&mut s, ""); // undo last 1 turn → 4 messages remain
        assert_eq!(removed, 1);
        assert_eq!(s.messages.len(), 4);
        // display_messages: after_message=2 survives, after_message=6 is pruned.
        assert_eq!(s.display_messages.len(), 1);
        assert_eq!(s.display_messages[0].after_message, 2);
        // turn_stats: after_message=4 survives, after_message=6 is pruned.
        assert_eq!(s.turn_stats.len(), 1);
        assert_eq!(s.turn_stats[0].after_message, 4);
    }

    /// Verify save_session_to_hash / load_session bucket symmetry:
    /// writing to a project-hash bucket and reading it back returns the same session.
    #[test]
    fn save_session_to_hash_roundtrip() {
        // Shared process-global env lock so ATOMCODE_HOME mutations don't race
        // the other daemon test modules in the same test binary.
        let _guard = crate::atomcode_home_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var("ATOMCODE_HOME").ok();
        std::env::set_var("ATOMCODE_HOME", dir.path());

        let result = std::panic::catch_unwind(|| {
            let session = session_with_turns(2);
            let hash = "deadbeef";

            crate::save_session_to_hash(hash, &session).expect("save_session_to_hash");

            let loaded = crate::load_session(hash, session.id.as_str()).expect("load_session");
            assert_eq!(loaded.id.as_str(), session.id.as_str());
            assert_eq!(loaded.messages.len(), session.messages.len());
        });

        match &prev {
            Some(v) => std::env::set_var("ATOMCODE_HOME", v),
            None => std::env::remove_var("ATOMCODE_HOME"),
        }

        result.expect("round-trip test panicked");
    }
}
