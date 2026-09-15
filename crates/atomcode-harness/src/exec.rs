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
            // The call as it stands AFTER the gates, arguments and all.
            let call = exec.call.clone();
            let (exec_turn, exec_round) = (exec.turn, exec.round);
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
                //
                // Progress and questions reach the person through whoever drives
                // this agent, if anyone does.
                let session = crate::agent::scoped(&ctx).service::<crate::seams::SessionSvc>();
                let (progress, requester) =
                    match (ctx.service::<crate::seams::ToolDriverSvc>(), &session) {
                        (Some(driver), Some(log)) => (
                            driver.progress(log.id(), &call.id),
                            driver.requester(log.id()),
                        ),
                        _ => (ProgressSink::noop(), None),
                    };
                let tool_ctx = ToolContext {
                    working_dir,
                    cancel,
                    progress,
                    requester,
                };
                // The one moment "this call is running" becomes true: every
                // waterfall listener delegated, so no gate refused it and no
                // scheduler held it back. Committed here rather than derived
                // from the assistant message, which is a fact about what the
                // model ASKED for and lands long before any of them decided.
                //
                // After the unknown-tool check above, so a tool nobody mounted
                // never announces a start it cannot have — the guard a driver
                // used to keep for itself.
                if let Some(session) = session {
                    crate::session::commit(
                        &ctx,
                        &session,
                        crate::session::SessionEvent::ToolStarted {
                            turn: exec_turn,
                            round: exec_round,
                            call: call.clone(),
                        },
                    );
                }
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
