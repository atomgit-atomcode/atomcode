//! [`for_provider`] — 按 provider 配置选 [`CtxBuilder`] 实现的唯一入口。
//!
//! 添加新模型的 ctx 管理策略 **只需两步**：
//!
//! 1. 新建文件 `crates/atomcode-core/src/ctx/<name>.rs`，定义
//!    `pub struct XxxCtx` 并 `impl CtxBuilder for XxxCtx`
//! 2. 在 [`for_provider`] 里插一条 `if` 分支，匹配
//!    `provider.provider_type` 或 `provider.model` 前缀。
//!
//! 未命中任何规则 → [`DefaultCtx`]，保留 atomcode 当前的上下文行为
//! 不变。
//!
//! 规则表是 **按顺序匹配**：靠上的规则优先生效。当一个 provider
//! 同时匹配多个规则（例如 `provider_type == "ollama"` 且
//! `model.starts_with("claude-")`，虽然现实中极少见），靠前的赢。

use std::sync::Arc;

use super::{CtxBuilder, DefaultCtx};
use atomcode_config::config::provider::ProviderConfig;

/// 给定 provider config 返回对应的 [`CtxBuilder`]。
///
/// 由 coding runtime 在会话准备和 provider 重建时调用。
///
/// 返回 `Arc` 而非 `Box`，让消息构建与 kernel agent 共享同一个 ctx 实例，
/// 避免上下文渲染与实际发送路径漂移。
pub fn for_provider(provider: &ProviderConfig) -> Arc<dyn CtxBuilder> {
    // ── 规则表（按注册顺序匹配）─────────────────────────

    // Ollama: 本地小窗口模型
    if provider.provider_type == "ollama" {
        return Arc::new(super::ollama::OllamaCtx::new(provider));
    }

    // （未来其它模型在此插入 —— Claude / GPT-4 / 自定义微调等）
    //
    // 示例:
    //   if provider.model.starts_with("claude-") {
    //       return Arc::new(super::claude::ClaudeCtx::new(provider));
    //   }

    // ── Fallback ────────────────────────────────────────
    Arc::new(DefaultCtx::new(provider))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(ptype: &str, model: &str, ctx: usize) -> ProviderConfig {
        ProviderConfig {
            provider_type: ptype.into(),
            api_key: None,
            model: model.into(),
            base_url: None,
            system_prompt: None,
            user_agent: None,
            context_window: ctx,
            max_tokens: None,
            thinking_type: None,
            thinking_keep: None,
            reasoning_history: None,
            reasoning_effort: None,
            thinking_enabled: None,
            thinking_budget: None,
            skip_tls_verify: false,
            ephemeral: false,
            capable_model: None,
        }
    }

    #[test]
    fn resolves_ollama_by_provider_type() {
        let p = provider("ollama", "llama3", 8_000);
        assert_eq!(for_provider(&p).name(), "ollama");
    }

    #[test]
    fn resolves_default_for_unknown_provider() {
        let p = provider("openai", "gpt-4o", 128_000);
        assert_eq!(for_provider(&p).name(), "default");

        let p = provider("anthropic", "claude-3-5-sonnet", 200_000);
        assert_eq!(for_provider(&p).name(), "default");

        let p = provider("", "", 0);
        assert_eq!(for_provider(&p).name(), "default");
    }

    #[test]
    fn ollama_rule_matches_any_model_under_ollama_provider() {
        // 不管 model 字段是什么,provider_type == "ollama" 就走 Ollama
        for model in ["llama3-8b", "qwen2.5-coder", "deepseek-coder-v2", ""] {
            let p = provider("ollama", model, 8_000);
            assert_eq!(for_provider(&p).name(), "ollama", "model={}", model);
        }
    }
}
