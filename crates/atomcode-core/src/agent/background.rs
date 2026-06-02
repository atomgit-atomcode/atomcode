//! Background agent — runs a task in an isolated context.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::conversation::Conversation;
use crate::ctx::CtxBuilder;
use crate::i18n::{t, Msg};
use crate::provider::LlmProvider;
use crate::tool::{ToolContext, ToolRegistry};
use crate::turn::event::TurnResult;
use crate::turn::permission::{AutoPermissionDecider, AutoPermissionMode};
use crate::turn::runner::TurnRunner;

use super::AgentEvent;

/// Maximum turns for a background task.
const MAX_BACKGROUND_TURNS: usize = 5;
/// Overall timeout for the entire background task.
const BACKGROUND_TIMEOUT: Duration = Duration::from_secs(120);

/// Run a background task with an independent conversation and turn runner.
/// Sends progress events through `progress_tx` so the TUI can show updates.
pub async fn run_background_task(
    task: &str,
    provider: Arc<dyn LlmProvider>,
    tools: Arc<ToolRegistry>,
    context: ToolContext,
    config: Config,
    ctx: Arc<dyn CtxBuilder>,
    _progress_tx: mpsc::UnboundedSender<AgentEvent>,
) -> AgentEvent {
    match tokio::time::timeout(
        BACKGROUND_TIMEOUT,
        run_background_inner(task, provider, tools, context, config, ctx),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => AgentEvent::BackgroundComplete {
            summary: t(Msg::BgTaskTimedOut { secs: BACKGROUND_TIMEOUT.as_secs() }).into_owned(),
            files_edited: vec![],
            turns: 0,
            success: false,
        },
    }
}

async fn run_background_inner(
    task: &str,
    provider: Arc<dyn LlmProvider>,
    tools: Arc<ToolRegistry>,
    context: ToolContext,
    config: Config,
    ctx: Arc<dyn CtxBuilder>,
) -> AgentEvent {
    let bg_context = context.isolate().await;
    let permission = Box::new(AutoPermissionDecider::new(AutoPermissionMode::AcceptEdits));

    // Build a minimal tool registry for background tasks — only file I/O tools.
    // Excludes MCP tools and bash to keep the prompt small and responses fast.
    // The full Config is passed (not stripped) because the provider needs
    // api_key, base_url, model, and context_window from it. Only the tool
    // set is restricted; config fields are read-only and pose no risk.
    let bg_tools = crate::tool::ToolRegistry::new();
    let essential = ["read_file", "write_file", "edit_file", "glob", "grep", "list_directory", "search_replace"];
    for name in &essential {
        if let Some(tool) = tools.get(name).await {
            bg_tools.register_arc(name.to_string(), tool).await;
        }
    }

    let bg_working_dir = bg_context.working_dir
        .try_read()
        .map(|g| g.clone())
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    let mut hook_engine = crate::hook::HookEngine::new();
    hook_engine.load_all(&bg_working_dir);

    let mut runner = TurnRunner {
        provider,
        tools: Arc::new(bg_tools),
        context: bg_context,
        config: config.clone(),
        ctx,
        permission,
        recently_edited_files: Vec::new(),
        hook_engine: std::sync::Arc::new(hook_engine),
        loop_guard: Default::default(),
        current_turn_number: 0,
    };

    let mut conversation = Conversation::new();
    conversation.add_user_message(task);

    let system_prompt = "You are a background agent. Complete the task autonomously.\n\
         You can read and edit files. You CANNOT run bash commands.\n\
         Be concise. Report what you changed when done.";

    let cancel = CancellationToken::new();
    // Internal event channel — drain only, tool events not forwarded to TUI.
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let mut turns = 0usize;
    let mut last_text = String::new();

    for _ in 0..MAX_BACKGROUND_TURNS {
        let result = runner
            .run(&mut conversation, system_prompt, &event_tx, cancel.clone())
            .await;

        turns += 1;

        // Drain internal events to prevent channel backpressure
        while event_rx.try_recv().is_ok() {}

        match result {
            TurnResult::Responded { text, .. } => {
                last_text = text;
                break;
            }
            TurnResult::UsedTools { text, .. } => {
                if let Some(t) = text {
                    last_text = t;
                }
            }
            TurnResult::Failed(e) => {
                return AgentEvent::BackgroundComplete {
                    summary: t(Msg::BgTaskError { error: &e.to_string() }).into_owned(),
                    files_edited: std::mem::take(&mut runner.recently_edited_files),
                    turns,
                    success: false,
                };
            }
            TurnResult::Cancelled => {
                return AgentEvent::BackgroundComplete {
                    summary: t(Msg::BgTaskCancelled).into_owned(),
                    files_edited: std::mem::take(&mut runner.recently_edited_files),
                    turns,
                    success: false,
                };
            }
        }
    }

    let summary = if last_text.len() > 500 {
        let mut boundary = 497;
        while boundary > 0 && !last_text.is_char_boundary(boundary) {
            boundary -= 1;
        }
        format!("{}...", &last_text[..boundary])
    } else if last_text.is_empty() {
        t(Msg::BgTaskNoSummary).into_owned()
    } else {
        last_text
    };

    AgentEvent::BackgroundComplete {
        summary,
        files_edited: runner.recently_edited_files,
        turns,
        success: true,
    }
}
