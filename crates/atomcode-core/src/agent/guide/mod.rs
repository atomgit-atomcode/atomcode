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

use std::sync::Arc;

use crate::agent::sub_agent::registry::SubAgentRegistry;
use crate::agent::sub_agent::types::{SubAgentDefinition, SubAgentKind, SubAgentToolPolicy};
use kb::KnowledgeBase;

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
        knowledge: Some(Arc::new(KnowledgeBase::embedded())),
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
1. 优先使用对话中的知识库内容回答问题。如果知识库已覆盖该问题，直接回答
2. 仅在知识库完全不包含相关信息时，才使用 web_search 或 web_fetch。
   每次搜索使用精准关键词，最多搜索 2 次。2 次搜索仍未找到答案，
   直接告知用户并建议查阅官方文档
3. 必须在第一轮就给出实质性回答，不要反复搜索
4. 回答简洁准确，控制在 300-500 字
"#;
