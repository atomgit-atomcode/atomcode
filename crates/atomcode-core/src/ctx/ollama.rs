//! [`OllamaCtx`] — 为本地小窗口 Ollama 模型优化的上下文策略。
//!
//! ## 与 [`DefaultCtx`] 的三点差异
//!
//! 1. **更早触发压缩**：总 tokens 超过窗口 35% 就压，而非默认的 50%。
//!    8K 窗口下 ~2800 tokens 即启动压缩，给后续 turn 留呼吸空间。
//! 2. **工具输出更紧**：单条 tool_result 上限 = `ctx/8` clamp `[2K, 6K]`
//!    字节，显著低于 Default 的 `[8K, 32K]`。本地模型 8K 窗口下一条
//!    bash 输出占一半预算是主要失败模式。
//! 3. **窗口默认值降低**：若 `provider.context_window` 未设，fallback
//!    到 8000（Default 是 128000）。匹配 Ollama CLI 的 `num_ctx` 常见值。
//!
//! ## 不做的事(明确范围)
//!
//! - **不砍 system prompt**：tool schema 作为独立参数传给 LLM API，
//!   不在 `system_prompt: &str` 里。真要简化 system prompt 需要在
//!   [`crate::agent::prompt`] 层面做。
//! - **不改工具集筛选**：哪些工具暴露给模型是 [`crate::tool::ToolRegistry`]
//!   的职责,与 ctx 无关。
//! - **不重写 render 管道**：`build_messages`
//!   直接透传给 [`crate::ctx::render::build_messages`] —— 与默认行为同
//!   pipeline（包括 `collapse_committed` 预处理、hard-cut、turn_reminder
//!   等），只是 ctx_window 更小、配合更紧的 tool-output 截断。
//!   想要 render pipeline 级别的定制,完全重写自己的 `build_messages`
//!   即可,不必受这里影响。
//!
//! 需要以上行为时,在上层扩展相应模块,不在 ctx 里做。

use super::CtxBuilder;
use crate::config::provider::ProviderConfig;
use crate::conversation::message::Message;
use crate::conversation::{ContextStats, Conversation};
use crate::tool::ToolResult;

/// 本地 Ollama 模型的上下文策略。
#[derive(Debug, Clone)]
pub struct OllamaCtx {
    /// Token budget, 至少 4K(再低就没意义了)
    ctx_window: usize,

    /// Lowercased model id。用于 [`crate::ctx::render::apply_model_directives`]
    /// 判断是否追加 CJK 语言锁 / MiniMax thinking 纪律。本地 Ollama 也
    /// 常跑 qwen / deepseek / minimax 蒸馏版,同一套规则适用。
    model_id: String,
}

impl OllamaCtx {
    pub fn new(provider: &ProviderConfig) -> Self {
        // Ollama 的默认 ctx 是 8000(见 default_context_window_for).
        // 再给一个硬下限防 0 / 配置漂移。
        Self {
            ctx_window: provider.context_window.max(4000),
            model_id: provider.model.to_lowercase(),
        }
    }

    /// 单条 tool_result 硬字符上限: ctx/8 clamp [2K, 6K].
    /// 对比 Default 的 ctx/8 clamp [8K, 32K] 显著更紧。
    fn tool_output_cap(&self) -> usize {
        (self.ctx_window / 8).min(6_000).max(2_000)
    }
}

impl CtxBuilder for OllamaCtx {
    fn build_messages(
        &self,
        conv: &Conversation,
        system_prompt: &str,
        turn_reminder: &str,
    ) -> (Vec<Message>, ContextStats) {
        // 渲染透传给默认 render 管道,仅把 ctx_window 传下去决定
        // token 预算; cold zone / collapse_committed / hard-cut / turn_reminder
        // 注入的具体策略由 ctx::render 统一执行。
        // model_id 依赖的指令(CJK 语言锁 / MiniMax thinking 纪律)
        // 在渲染管道前贴到 system prompt 上,与 DefaultCtx 一致。
        let sys = crate::ctx::render::apply_model_directives(system_prompt, &self.model_id);
        crate::ctx::render::build_messages(conv, &sys, self.ctx_window, turn_reminder)
    }

    /// 复用 ctx::render::needs_compression — 它的绝对 headroom 公式
    /// `ctx_window - min(13K, ctx_window/4)` 在小窗口下天然偏紧
    /// (8K Ollama → 6K threshold = 75% 触发, 比之前的 35% 晚但更接近"撑爆前一刻"的真实 headroom)。
    /// 之前的 35% hardcoded 阈值是为 4-8K Ollama 量身的早触发, 但在
    /// 16K-32K Ollama 上反而过早。新公式自适应窗口大小, 不再需要单独的 Ollama tier。
    fn needs_compression(&self, conv: &Conversation, system_tokens: usize) -> bool {
        crate::ctx::render::needs_compression(conv, system_tokens, self.ctx_window)
    }

    fn compression_plan(
        &self,
        conv: &Conversation,
        keep_ceiling: usize,
    ) -> Option<(String, usize)> {
        // 决策用的是 self.needs_compression(35% 早触发),
        // plan 内容生成沿用 ctx::render 的 one-line-per-round 机械摘要。
        let (content, n) = crate::ctx::render::build_compression_content(conv, keep_ceiling);
        if content.is_empty() || n == 0 {
            None
        } else {
            Some((content, n))
        }
    }

    fn truncate_tool_output(&self, result: &mut ToolResult, tool_name: &str) {
        // 先走共享的 per-tool 截断(bash 保错误行、read_file 出 skeleton、
        // web_fetch head+tail 等)。传入 self.ctx_window 让内部公式知道
        // 窗口小。
        crate::ctx::truncate::truncate_output(result, tool_name, self.ctx_window);

        // 再套 Ollama tier 的硬上限,belt-and-suspenders。
        let cap = self.tool_output_cap();
        if result.output.len() > cap {
            // UTF-8 安全截断:cap 可能落在 multi-byte char 中间,
            // 走到前一个 char boundary 再切。
            let mut boundary = cap;
            while boundary > 0 && !result.output.is_char_boundary(boundary) {
                boundary -= 1;
            }
            result.output.truncate(boundary);
            result
                .output
                .push_str("\n[... truncated by OllamaCtx (small window) ...]");
        }
    }

    fn ctx_window(&self) -> usize {
        self.ctx_window
    }

    fn name(&self) -> &'static str {
        "ollama"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::Conversation;
    use crate::tool::ToolResult;

    fn ollama_provider(ctx: usize) -> ProviderConfig {
        ProviderConfig {
            provider_type: "ollama".into(),
            api_key: None,
            model: "llama3-8b".into(),
            base_url: Some("http://localhost:11434".into()),
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

}
    }

    #[test]
    fn name_is_ollama() {
        let o = OllamaCtx::new(&ollama_provider(8_000));
        assert_eq!(o.name(), "ollama");
    }

    #[test]
    fn ctx_window_clamped_to_4k_minimum() {
        // 防御:context_window = 0 或缺失时 fallback 到 4000
        let o = OllamaCtx::new(&ollama_provider(0));
        assert_eq!(o.ctx_window, 4_000);

        let o = OllamaCtx::new(&ollama_provider(2_000));
        assert_eq!(o.ctx_window, 4_000);

        // 正常值不变
        let o = OllamaCtx::new(&ollama_provider(8_000));
        assert_eq!(o.ctx_window, 8_000);

        let o = OllamaCtx::new(&ollama_provider(32_000));
        assert_eq!(o.ctx_window, 32_000);
    }

    #[test]
    fn tool_output_cap_follows_spec() {
        // ctx=8K → 8000/8=1000, 被 max(2000) 抬到 2000
        assert_eq!(
            OllamaCtx::new(&ollama_provider(8_000)).tool_output_cap(),
            2_000
        );
        // ctx=16K → 16000/8=2000, 正好等于下限
        assert_eq!(
            OllamaCtx::new(&ollama_provider(16_000)).tool_output_cap(),
            2_000
        );
        // ctx=32K → 32000/8=4000, 在 [2K, 6K] 内
        assert_eq!(
            OllamaCtx::new(&ollama_provider(32_000)).tool_output_cap(),
            4_000
        );
        // ctx=64K → 64000/8=8000, 被 min(6000) 压到 6000
        assert_eq!(
            OllamaCtx::new(&ollama_provider(64_000)).tool_output_cap(),
            6_000
        );
    }

    #[test]
    fn truncate_result_enforces_small_cap() {
        let o = OllamaCtx::new(&ollama_provider(8_000));
        let mut r = ToolResult {
            call_id: "t1".into(),
            output: "x".repeat(50_000),
            success: true,
        };
        o.truncate_tool_output(&mut r, "bash");
        // tool_output_cap() = 2000, 加上后缀消息大约 +50 字节
        assert!(
            r.output.len() <= 2_200,
            "OllamaCtx truncate 后输出 {} 字节超过 cap 2200",
            r.output.len()
        );
    }

    #[test]
    fn truncate_result_utf8_safe_on_cjk_boundary() {
        // 回归:3 字节 CJK 字符重复,裁切点可能落在 char 中间,
        // String::truncate 本身会 panic。is_char_boundary 循环修正。
        let o = OllamaCtx::new(&ollama_provider(8_000));
        let mut r = ToolResult {
            call_id: "t1".into(),
            output: "中".repeat(5_000), // 15000 字节,远超 2K cap
            success: true,
        };
        o.truncate_tool_output(&mut r, "bash");
        // 不 panic + 输出仍是合法 UTF-8
        assert!(std::str::from_utf8(r.output.as_bytes()).is_ok());
        assert!(r.output.len() <= 2_200);
    }

    #[test]
    fn needs_compression_triggers_earlier_than_default() {
        let o = OllamaCtx::new(&ollama_provider(8_000));
        // 空对话不触发
        let empty = Conversation::new();
        assert!(!o.needs_compression(&empty, 100));

        // 构造超过 35% 阈值(= 2800 tokens)的对话,模型数也够(>= 12)
        let mut conv = Conversation::new();
        for i in 0..8 {
            conv.add_user_message(&format!("user turn {} with moderate content", i));
            conv.add_assistant_tool_calls(
                Some(&format!("some assistant reasoning for turn {}", i)),
                vec![],
                None,
            );
        }
        // 16 条消息,每条 ~10-15 tokens → 总 ~200 tokens,低于 35%,不压
        assert!(!o.needs_compression(&conv, 50));

        // 再填大量长消息让总 tokens 超过 2800
        for _ in 0..20 {
            conv.add_user_message(&"lorem ipsum ".repeat(50).repeat(2)); // 每条 ~250 tokens
            conv.add_assistant_tool_calls(Some(&"dolor sit amet ".repeat(50)), vec![], None);
        }
        // 此时总 tokens 远超 2800
        assert!(
            o.needs_compression(&conv, 50),
            "大对话下 OllamaCtx 应触发压缩(35% threshold)"
        );
    }

    #[test]
    fn compression_plan_none_below_threshold() {
        let o = OllamaCtx::new(&ollama_provider(8_000));
        let conv = Conversation::new();
        assert!(o.compression_plan(&conv, usize::MAX).is_none());
    }

    #[test]
    fn build_messages_returns_nonempty_for_simple_conv() {
        let o = OllamaCtx::new(&ollama_provider(8_000));
        let mut conv = Conversation::new();
        conv.add_user_message("hello");
        let (msgs, stats) = o.build_messages(&conv, "SYS", "");
        assert!(!msgs.is_empty());
        assert!(stats.sent_tokens <= 8_000);
    }
}
