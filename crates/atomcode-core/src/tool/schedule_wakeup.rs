use crate::tool::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

pub struct ScheduleWakeupTool;

#[derive(Deserialize)]
struct Args {
    delay_seconds: u32,
    reason: String,
    prompt: String,
}

#[async_trait]
impl Tool for ScheduleWakeupTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "schedule_wakeup",
            description: "Schedule when to resume work in a self-paced /loop. ONLY call inside a /loop.\n\n\
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
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "delay_seconds": {"type": "integer", "description": "60–3600. Seconds until wakeup."},
                    "reason": {"type": "string", "description": "One short sentence explaining the chosen delay. Be specific. Shown to the user and recorded to telemetry."},
                    "prompt": {"type": "string", "description": "The /loop input to fire on wakeup. Pass the same input verbatim each turn so the next firing re-enters the loop and continues."}
                },
                "required": ["delay_seconds", "reason", "prompt"]
            }),
        }
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::AutoApprove
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let a: Args = serde_json::from_str(args)?;
        let delay = a.delay_seconds.clamp(60, 3600);
        let call_id = ctx.current_call_id.clone().unwrap_or_default();
        match &ctx.event_tx {
            Some(tx) => {
                let _ = tx.send(crate::turn::event::TurnEvent::WakeupScheduled {
                    delay_seconds: delay,
                    prompt: a.prompt,
                    reason: a.reason.clone(),
                });
                Ok(ToolResult {
                    call_id,
                    output: format!("Will resume in {delay}s: {}", a.reason),
                    success: true,
                })
            }
            None => Ok(ToolResult {
                call_id,
                output: "schedule_wakeup is only available inside an active /loop turn".into(),
                success: false,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;

    #[tokio::test]
    async fn clamps_and_emits_when_event_tx_present() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut ctx = ToolContext::new(std::path::PathBuf::from("/tmp"));
        ctx.event_tx = Some(std::sync::Arc::new(tx));
        let out = ScheduleWakeupTool
            .execute(r#"{"delay_seconds":5,"reason":"wait CI","prompt":"keep going"}"#, &ctx)
            .await
            .unwrap();
        assert!(out.success);
        match rx.try_recv().unwrap() {
            crate::turn::event::TurnEvent::WakeupScheduled { delay_seconds, .. } => {
                assert_eq!(delay_seconds, 60); // clamped up from 5
            }
            _ => panic!("expected WakeupScheduled"),
        }
    }

    #[tokio::test]
    async fn fails_without_event_tx() {
        let ctx = ToolContext::new(std::path::PathBuf::from("/tmp"));
        let out = ScheduleWakeupTool
            .execute(r#"{"delay_seconds":120,"reason":"r","prompt":"p"}"#, &ctx)
            .await
            .unwrap();
        assert!(!out.success);
    }
}
