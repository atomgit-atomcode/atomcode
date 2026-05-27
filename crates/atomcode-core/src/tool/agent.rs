use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};
use crate::agent::sub_agent::registry::SubAgentRegistry;
use crate::agent::sub_agent::runner::SubAgentRunner;
use crate::agent::AgentEvent;
use crate::config::Config;
use crate::provider::LlmProvider;

#[derive(Debug, Deserialize)]
struct InvokeSubAgentInput {
    subagent_name: String,
    task: String,
}

pub struct AgentTool {
    pub provider: Arc<dyn LlmProvider>,
    pub config: Config,
    pub event_tx: mpsc::UnboundedSender<AgentEvent>,
    pub subagent_registry: Arc<std::sync::RwLock<SubAgentRegistry>>,
}

impl AgentTool {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        config: Config,
        event_tx: mpsc::UnboundedSender<AgentEvent>,
        subagent_registry: Arc<std::sync::RwLock<SubAgentRegistry>>,
    ) -> Self {
        Self {
            provider,
            config,
            event_tx,
            subagent_registry,
        }
    }
}

#[async_trait]
impl Tool for AgentTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "invoke_subagent",
            description: "调用子代理执行特定任务。可用的子代理: atomcode-guide (解答 AtomCode 使用问题)".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "subagent_name": {
                        "type": "string",
                        "description": "子代理名称"
                    },
                    "task": {
                        "type": "string",
                        "description": "要执行的任务描述"
                    }
                },
                "required": ["subagent_name", "task"]
            }),
        }
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::AutoApprove
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let input: InvokeSubAgentInput = serde_json::from_str(args)
            .map_err(|e| anyhow::anyhow!("invalid args: {}", e))?;

        // 1. Lookup definition + clone + drop lock
        let def = {
            let registry = self.subagent_registry.read()
                .map_err(|e| anyhow::anyhow!("lock error: {}", e))?;
            registry.find(&input.subagent_name)
                .ok_or_else(|| {
                    let available: Vec<String> = registry.list()
                        .into_iter().map(|d| d.name).collect();
                    anyhow::anyhow!("未找到子代理: {}，可用: {}", input.subagent_name, available.join(", "))
                })?
        };

        // 2. Construct runner with a fresh cancellation token
        let cancel_token = CancellationToken::new();
        let runner = SubAgentRunner::new(
            self.provider.clone(),
            Arc::new(self.config.clone()),
            ctx.tool_registry.clone().unwrap_or_else(|| Arc::new(crate::tool::ToolRegistry::new())),
            ctx.clone(),
            self.event_tx.clone(),
            cancel_token,
        );

        // 3. Execute
        let result = runner.run(def, input.task).await;

        match result {
            Ok(output) => {
                let mut text = output.text;
                if output.truncated {
                    text.push_str("\n\n(回答已截断)");
                }
                Ok(ToolResult {
                    call_id: String::new(),
                    output: text,
                    success: true,
                })
            }
            Err(e) => {
                Ok(ToolResult {
                    call_id: String::new(),
                    output: format!("子代理错误: {}", e.message),
                    success: false,
                })
            }
        }
    }
}
