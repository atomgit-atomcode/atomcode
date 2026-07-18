//! `schedule_wakeup` — the kernel-side tool that drives the v2 `/loop` self-pace.
//!
//! METHOD B (bridge-side kernel tool): the kernel turn runner cannot see v1's core
//! `ScheduleWakeupTool` (that one implements `atomcode_core::tool::Tool` and routes
//! through a v1 `TurnEvent`). So the bridge mounts THIS tool — implementing the kernel
//! [`Tool`](atomcode_kernel::tool::Tool) trait — into the kernel registry. On `execute`
//! it clamps the delay and sends a [`WakeupRequest`] back to the bridge over an injected
//! `tokio::mpsc` channel; the bridge's turn-end Snapshot hook then schedules a
//! cancel-aware delayed continuation (the v2 analogue of v1's wakeup scheduling).
//!
//! Reuses v1's [`WakeupRequest`] type (the same way the v2 goal path reuses
//! `GoalState`), so the loop's data shape stays identical across engines. The
//! description is copied verbatim from v1's `schedule_wakeup` so the model sees the
//! same guidance on both engines.

use async_trait::async_trait;
use atomcode_core::agent::loop_state::WakeupRequest;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Deserialize)]
struct Args {
    delay_seconds: u32,
    reason: String,
    prompt: String,
}

/// Kernel tool that, when called, hands a [`WakeupRequest`] to the bridge so it can
/// schedule the next `/loop` continuation. Holds the bridge's injected sender; cheap
/// to construct (one per prepared engine, re-mounted on respawn).
pub struct ScheduleWakeupTool {
    wakeup_tx: UnboundedSender<WakeupRequest>,
}

impl ScheduleWakeupTool {
    pub fn new(wakeup_tx: UnboundedSender<WakeupRequest>) -> Self {
        Self { wakeup_tx }
    }
}

#[async_trait]
impl Tool for ScheduleWakeupTool {
    fn name(&self) -> &str {
        "schedule_wakeup"
    }

    fn description(&self) -> &str {
        // Copied verbatim from v1 (atomcode-core schedule_wakeup) so the model gets the
        // identical delay-picking guidance on both engines.
        "Schedule when to resume work in a self-paced /loop. ONLY call inside a /loop.\n\n\
After this turn's work, if the task still needs another pass, call this to set the next wakeup; if the \
task is done or no longer needs to run, do NOT call it — the loop ends.\n\n\
## Picking delay_seconds (prompt cache TTL is ~5 minutes)\n\
- 60–270s: cache stays warm. For actively polling external state the harness can't notify you about — a \
CI run, a deploy, a remote queue.\n\
- 300–3600s: pay the cache miss. When there's no point checking sooner, or as a long fallback heartbeat.\n\
- Don't pick 300s — worst of both. Want ~5 min? use 270s (warm) or commit to 1200s+. Think in cache \
windows, not round minutes.\n\
- Idle tick with no specific signal: default 1200–1800s.\n\
- Do NOT short-poll background work you started — harness-tracked work re-invokes you when done, so \
polling wastes wakeups. Use a long 1200s+ fallback there instead.\n\n\
The runtime clamps delay_seconds to [60, 3600]."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "delay_seconds": {"type": "integer", "description": "60–3600. Seconds until wakeup."},
                "reason": {"type": "string", "description": "One short sentence explaining the chosen delay. Be specific. Shown to the user and recorded to telemetry."},
                "prompt": {"type": "string", "description": "The /loop input to fire on wakeup. Pass the same input verbatim each turn so the next firing re-enters the loop and continues."}
            },
            "required": ["delay_seconds", "reason", "prompt"]
        })
    }

    // Scheduling only (no side effect on the user's machine) → Safe, so it never prompts.
    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => {
                return ToolResult {
                    call_id: String::new(),
                    content: format!("schedule_wakeup: invalid arguments: {e}."),
                    is_error: true,
                    images: vec![],
                }
            }
        };
        let delay = a.delay_seconds.clamp(60, 3600);
        // Send the request back to the bridge. A closed channel means the loop was torn
        // down (ClearLoop/Shutdown) — report it so the model stops trying to reschedule.
        match self.wakeup_tx.send(WakeupRequest {
            delay_seconds: delay,
            prompt: a.prompt,
            reason: a.reason.clone(),
        }) {
            Ok(()) => ToolResult {
                call_id: String::new(),
                content: format!("Will resume in {delay}s: {}", a.reason),
                is_error: false,
                images: vec![],
            },
            Err(_) => ToolResult {
                call_id: String::new(),
                content: "schedule_wakeup is only available inside an active /loop turn".into(),
                is_error: true,
                images: vec![],
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn ctx() -> ToolContext {
        ToolContext {
            working_dir: std::path::PathBuf::from("."),
            cancel: CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        }
    }

    #[tokio::test]
    async fn clamps_and_emits_when_channel_open() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tool = ScheduleWakeupTool::new(tx);
        let out = tool
            .execute(
                r#"{"delay_seconds":5,"reason":"wait CI","prompt":"keep going"}"#,
                &ctx(),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        let req = rx.try_recv().unwrap();
        assert_eq!(req.delay_seconds, 60); // clamped up from 5
        assert_eq!(req.prompt, "keep going");
    }

    #[tokio::test]
    async fn clamps_high_delay_down() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tool = ScheduleWakeupTool::new(tx);
        tool.execute(
            r#"{"delay_seconds":99999,"reason":"r","prompt":"p"}"#,
            &ctx(),
        )
        .await;
        assert_eq!(rx.try_recv().unwrap().delay_seconds, 3600);
    }

    #[tokio::test]
    async fn errors_when_channel_closed() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx); // loop torn down
        let tool = ScheduleWakeupTool::new(tx);
        let out = tool
            .execute(r#"{"delay_seconds":120,"reason":"r","prompt":"p"}"#, &ctx())
            .await;
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn invalid_args_error() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tool = ScheduleWakeupTool::new(tx);
        let out = tool.execute(r#"{"delay_seconds":"oops"}"#, &ctx()).await;
        assert!(out.is_error);
    }
}
