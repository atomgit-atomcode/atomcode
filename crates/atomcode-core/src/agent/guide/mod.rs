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
const GUIDE_SYSTEM_PROMPT: &str = r#"你是 AtomCode 使用指南，回答关于 AtomCode 功能、命令、配置和使用方法的问题。

## 回答策略

1. **知识库已覆盖** → 直接基于知识库内容回答
2. **知识库未覆盖** → 参考知识库中的「文档页索引」，按用户问题匹配对应文档页，用 web_fetch 拉取后回答

## 规则
- 知识库未覆盖时，最多拉取 1 个文档页
- 文档页拉取失败 → 告知用户直接访问 https://atomcode.atomgit.com/docs/zh/
- 回答控制在 500-800 字，基于文档原文总结，不要编造
"#;
