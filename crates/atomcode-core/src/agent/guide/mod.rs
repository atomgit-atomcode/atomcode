//! The atomcode-guide subagent.
//!
//! This subagent answers user questions about AtomCode features, commands,
//! configuration, MCP, Skills, and other usage topics. It uses a local
//! knowledge base (`KnowledgeBase`) for retrieval-augmented generation.
//!
//! # Wiring
//!
//! The `register` function should be called during application startup to
//! register the subagent with the global `SubAgentRegistry`.

pub mod kb;

use crate::agent::sub_agent::registry::SubAgentRegistry;
use crate::agent::sub_agent::types::{SubAgentDefinition, SubAgentKind, SubAgentToolPolicy};
//use std::sync::Arc;
//use kb::KnowledgeBase;  // wired in Task 10

/// Register the atomcode-guide subagent with the given registry.
///
/// # Errors
///
/// Returns an error if a subagent with name `"atomcode-guide"` is already
/// registered.
pub fn register(registry: &SubAgentRegistry) -> Result<(), String> {
    let def = SubAgentDefinition {
        name: "atomcode-guide".to_string(),
        description: "解答 AtomCode 使用问题 (功能、命令、配置、MCP、Skill 等)".to_string(),
        kind: SubAgentKind::QnA,
        system_prompt: GUIDE_SYSTEM_PROMPT.to_string(),
        model: None,
        tools: SubAgentToolPolicy::ReadOnlyWithWeb,
        // KnowledgeBase is wired in Task 10 (AgentLoop changes).
        // The KnowledgeBase is fully functional as a standalone module;
        // integration with the sub-agent runner will pass it as context.
        knowledge: None,
        ..Default::default()
    };
    registry.register(def)
}

/// System prompt for the atomcode-guide subagent.
///
/// The agent is instructed to prefer knowledge base answers, fall back to
/// web search, and be concise and accurate.
const GUIDE_SYSTEM_PROMPT: &str = r#"你是 AtomCode 使用指南。你的职责是回答关于 AtomCode 功能、命令、配置和使用方法的问题。

规则:
1. 优先使用知识库中的信息回答问题
2. 如果知识库不包含所需信息，使用 web_search 或 web_fetch 查找
3. 如果知识库消息在对话中被压缩或截断，你可以通过 grep 搜索 knowledge/ 目录下的 .md 文件来按需获取更多上下文
4. 回答应简洁、准确，必要时提供示例
5. 如果问题超出你的知识范围，如实告知并建议查阅官方文档
"#;
