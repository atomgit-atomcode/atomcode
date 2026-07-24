use crate::conversation::message::{Message, Role};
use crate::conversation::Conversation;
use crate::tool::ToolRegistry;

#[derive(Debug, Clone)]
pub struct RichContextStats {
    pub system_tokens: usize,
    pub sent_tokens: usize,
    pub total_messages: usize,
    pub tool_defs_tokens: usize,
    pub cold_zone_tokens: usize,
    pub ctx_window: usize,
    pub ctx_name: String,
}

pub async fn compute_rich_context_stats(
    conv: &Conversation,
    msgs: &[Message],
    tools: &ToolRegistry,
    ctx: &dyn super::CtxBuilder,
) -> RichContextStats {
    let tool_defs = tools.get_definitions().await;
    let tool_defs_tokens = tool_defs
        .iter()
        .map(|definition| {
            let params = serde_json::to_string(&definition.parameters).unwrap_or_default();
            (definition.name.len() + definition.description.len() + params.len()) / 4
        })
        .sum();
    let cold_zone_tokens = conv
        .cold_summaries
        .iter()
        .map(|summary| summary.len() / 4 + 4)
        .sum();
    let system_tokens = msgs
        .iter()
        .find(|message| matches!(message.role, Role::System))
        .map(Message::estimate_tokens)
        .unwrap_or(0);
    let sent_tokens = msgs
        .iter()
        .map(Message::estimate_tokens)
        .sum::<usize>()
        .saturating_sub(system_tokens);
    RichContextStats {
        system_tokens,
        sent_tokens,
        total_messages: msgs.len(),
        tool_defs_tokens,
        cold_zone_tokens,
        ctx_window: ctx.ctx_window(),
        ctx_name: ctx.name().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn counts_system_and_sent_tokens() {
        let conv = Conversation::new();
        let msgs = vec![
            Message::new(Role::System, "system prompt here"),
            Message::new(Role::User, "hello world"),
        ];
        let tools = ToolRegistry::new();
        let ctx = super::super::for_provider(&atomcode_config::config::provider::ProviderConfig {
            provider_type: String::new(),
            api_key: None,
            model: String::new(),
            base_url: None,
            system_prompt: None,
            user_agent: None,
            context_window: 128_000,
            max_tokens: None,
            thinking_type: None,
            thinking_keep: None,
            reasoning_history: None,
            reasoning_effort: None,
            thinking_enabled: None,
            thinking_budget: None,
            skip_tls_verify: false,
            ephemeral: true,
            capable_model: None,
        });
        let stats = compute_rich_context_stats(&conv, &msgs, &tools, &*ctx).await;
        assert_eq!(stats.total_messages, 2);
        assert!(stats.system_tokens > 0);
        assert!(stats.sent_tokens > 0);
        assert_eq!(stats.ctx_window, 128_000);
        assert_eq!(stats.tool_defs_tokens, 0);
    }
}
