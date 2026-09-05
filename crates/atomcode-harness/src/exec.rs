//! Running one tool call, as a free function.
//!
//! It lives outside the loop so a scheduling plugin can reach it. The loop's own
//! terminal calls it in a line; `tool-exec-parallel` calls it from inside a
//! `FuturesOrdered`; a remote executor would call it on the other side of a
//! socket. All three go through the same `tools/execute` waterfall, so approval,
//! permission rules, plan mode and the loop guard apply identically no matter
//! who scheduled the call.

use std::path::PathBuf;

use atomcode_kernel::tool::{ProgressSink, ToolContext, ToolResult};
use atomcode_plexus::Context;
use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use crate::events::{ToolExec, ToolsExecute};
use crate::seams::ToolsSvc;

/// Run one call through the `tools/execute` waterfall.
///
/// A denial is a *result*, not an error: the model has to see why its call did
/// not run, and the history has to stay pairable.
pub async fn execute_one(
    agent_ctx: &Context,
    cancel: CancellationToken,
    working_dir: PathBuf,
    mut exec: ToolExec,
) -> ToolResult {
    let ctx = agent_ctx.clone();
    agent_ctx
        .waterfall::<ToolsExecute, _>(&mut exec, move |exec| {
            let ctx = ctx.clone();
            let cancel = cancel.clone();
            let working_dir = working_dir.clone();
            let call = exec.call.clone();
            Box::pin(async move {
                let Some(toolbox) = ctx.service::<ToolsSvc>() else {
                    return error_result(&call.id, "no tool catalog is mounted");
                };
                let Some(tool) = toolbox.get(&call.name) else {
                    return error_result(
                        &call.id,
                        &format!(
                            "unknown tool `{}`; mounted: {}",
                            call.name,
                            toolbox.names().join(", ")
                        ),
                    );
                };
                // The agent's own token, not a fresh one: a long-running tool
                // that polls `ctx.cancel` has to actually see a stop, or a
                // cancel only takes effect after it finishes.
                let tool_ctx = ToolContext {
                    working_dir,
                    cancel,
                    progress: ProgressSink::noop(),
                    requester: None,
                };
                let mut result = tool.execute(&call.arguments, &tool_ctx).await;
                // Tools mint their own id-less results; pairing is the caller's.
                result.call_id = call.id.clone();
                result
            }) as BoxFuture<'_, ToolResult>
        })
        .await
}

pub fn error_result(call_id: &str, message: &str) -> ToolResult {
    ToolResult {
        call_id: call_id.to_string(),
        content: message.to_string(),
        is_error: true,
        images: vec![],
    }
}
