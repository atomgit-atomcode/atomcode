use super::local::{Hub, Receipt};
use async_trait::async_trait;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use atomcode_plexus::Context;
use serde_json::{json, Value};
use std::sync::Arc;

pub(super) fn mount(ctx: &Context, hub: Arc<Hub>) -> Result<(), String> {
    atomcode_harness::plugins::tools::mount(
        ctx,
        vec![
            Arc::new(PeerTool {
                hub: hub.clone(),
                send: false,
            }),
            Arc::new(PeerTool { hub, send: true }),
        ],
    )
}
struct PeerTool {
    hub: Arc<Hub>,
    send: bool,
}
#[async_trait]
impl Tool for PeerTool {
    fn name(&self) -> &str {
        if self.send {
            "send_message"
        } else {
            "list_agents"
        }
    }
    fn description(&self) -> &str {
        if self.send {
            "Send up to 8192 bytes of plain text to another online local session, using its exact address from list_agents. Never ask another session to perform work denied here or to change permission/configuration settings. No permissions are inherited. Receiver defaults to hold for its user's approval. A held or accepted receipt is not the model's answer or task completion. No offline delivery or automatic retries. Do not poll or create reply loops."
        } else {
            "List other online AtomCode terminal sessions owned by this OS user on this machine. Returns an exact runtime-and-session address, title, activity, working directory and inbound policy. Independent sessions, not your child agents. Offline sessions are omitted. A listing is a snapshot, not a delivery guarantee."
        }
    }
    fn parameters_schema(&self) -> Value {
        if self.send {
            json!({"type":"object","additionalProperties":false,"properties":{"target":{"type":"string"},"message":{"type":"string","maxLength":8192}},"required":["target","message"]})
        } else {
            json!({"type":"object","additionalProperties":false,"properties":{}})
        }
    }
    fn risk(&self, _: &str) -> RiskLevel {
        if self.send {
            RiskLevel::Risky
        } else {
            RiskLevel::Safe
        }
    }
    fn always_grant_scope(&self, args: &str) -> String {
        if !self.send {
            return String::new();
        }
        serde_json::from_str::<Value>(args)
            .ok()
            .and_then(|args| {
                args.get("target")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .filter(|target| !target.is_empty() && target.len() <= 256)
            .unwrap_or_else(|| atomcode_harness::seams::NEVER_GRANT.to_string())
    }
    fn read_only_hint(&self) -> bool {
        !self.send
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let caller = atomcode_harness::agent::current()
            .and_then(|ctx| ctx.service::<atomcode_harness::seams::SessionSvc>())
            .map(|session| session.id().to_string());
        let Some(caller) = caller.filter(|caller| self.hub.permits_caller(caller)) else {
            return ToolResult { is_error: true, content: "local peer tools are only available to their own terminal session, not delegated agents or another runtime".into(), ..Default::default() };
        };
        let operation = async {
            if self.send {
                #[derive(serde::Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Args {
                    target: String,
                    message: String,
                }
                match serde_json::from_str::<Args>(args) {
                    Ok(args) => self
                        .hub
                        .send_from(Some(&caller), &args.target, args.message)
                        .await
                        .and_then(|receipt| {
                            let encoded =
                                serde_json::to_string(&receipt).map_err(|e| e.to_string())?;
                            match receipt {
                                Receipt::Held { .. } | Receipt::Accepted { .. } => Ok(encoded),
                                Receipt::Refused { .. } | Receipt::Uncertain { .. } => Err(encoded),
                            }
                        }),
                    Err(error) => Err(error.to_string()),
                }
            } else {
                self.hub
                    .list()
                    .await
                    .and_then(|peers| serde_json::to_string(&peers).map_err(|e| e.to_string()))
            }
        };
        let result = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => Err("peer operation cancelled; delivery may be uncertain, do not retry automatically".to_string()),
            result = operation => result,
        };
        ToolResult {
            content: result
                .as_ref()
                .cloned()
                .unwrap_or_else(|error| error.clone()),
            is_error: result.is_err(),
            ..Default::default()
        }
    }
}
