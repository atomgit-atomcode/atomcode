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
use crate::i18n::current_locale;
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
        description: crate::i18n::t(crate::i18n::Msg::GuideDescription).into_owned(),
        kind: SubAgentKind::QnA,
        system_prompt: get_guide_system_prompt(),
        model: None,
        tools: SubAgentToolPolicy::ReadOnlyWithWeb,
        knowledge: Some(Arc::new(KnowledgeBase::embedded())),
        ..Default::default()
    };
    registry.register(def)
}

/// Get the system prompt for the atomcode-guide subagent based on current locale.
///
/// The agent is instructed to prefer knowledge base answers, fall back to
/// web search, and be concise and accurate.
pub fn get_guide_system_prompt() -> String {
    let locale = current_locale();
    match locale {
        crate::locale::Locale::En => GUIDE_SYSTEM_PROMPT_EN.to_string(),
        crate::locale::Locale::ZhCn => GUIDE_SYSTEM_PROMPT_ZH.to_string(),
    }
}

/// System prompt for the atomcode-guide subagent (Chinese).
const GUIDE_SYSTEM_PROMPT_ZH: &str = r#"你是 AtomCode 使用指南，回答关于 AtomCode 功能、命令、配置和使用方法的问题。

## 回答策略

1. **知识库已覆盖** → 直接基于知识库内容回答
2. **知识库未覆盖** → 参考知识库中的「文档页索引」，按用户问题匹配对应文档页，用 web_fetch 拉取后回答
   - 中文文档 URL：https://atomcode.atomgit.com/docs/zh/
   - 英文文档 URL：https://atomcode.atomgit.com/docs/en/

## 规则
- 知识库未覆盖时，最多拉取 1 个文档页
- 文档页拉取失败 → 告知用户直接访问文档站
- 回答控制在 500-800 字，基于文档原文总结，不要编造
- 使用与用户提问相同的语言回答
"#;

/// System prompt for the atomcode-guide subagent (English).
const GUIDE_SYSTEM_PROMPT_EN: &str = r#"You are the AtomCode User Guide, answering questions about AtomCode features, commands, configuration, and usage.

## Response Strategy

1. **Knowledge base covered** → Answer directly based on knowledge base content
2. **Knowledge base not covered** → Refer to the "Documentation Index" in the knowledge base, match the corresponding documentation page based on the user's question, and use web_fetch to retrieve it
   - Chinese doc URL: https://atomcode.atomgit.com/docs/zh/
   - English doc URL: https://atomcode.atomgit.com/docs/en/

## Rules
- When the knowledge base doesn't cover it, fetch at most 1 documentation page
- If documentation page fetch fails → tell the user to visit the documentation site directly
- Keep answers within 500-800 words, summarize based on documentation, don't make things up
- Answer in the same language as the user's question
"#;
