//! Kernel-native VL image preprocessing. Ported from `core::vision_preprocessor`
//! but provider-agnostic: the caller builds the VL `LlmProvider` (via its own
//! factory path) and hands it in; this module owns the parity-critical bits —
//! the prompt, the one-off kernel message, the 30s idle-timeout streaming loop,
//! and the outcome mapping. Both the CLI and the daemon call `run_vl_caption`.

use atomcode_kernel::message::{ImageContent, Message};
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::stream::StreamEvent;
use futures::StreamExt;
use std::sync::Arc;

/// Outcome of a VL preprocessing attempt. Same three variants (and the
/// `apply_outcome` contract) as the retired `core` version.
#[derive(Debug)]
pub enum PreprocessOutcome {
    /// Main model is vision-capable, or no images — pass through untouched.
    Skipped,
    /// VL produced a caption; caller folds it into the text and clears images.
    Replaced { text: String, vl_model: String },
    /// VL failed (build/stream/empty); caller folds a failure marker in.
    Failed { reason: String },
}

/// Pure short-circuit: no images, or the main model already accepts images.
pub fn should_skip(active_model: &str, has_images: bool) -> bool {
    !has_images || atomcode_capabilities::provider::model_suggests_vision(active_model)
}

/// Display name for a VL model: strip a `vendor/` prefix (e.g.
/// `Qwen/Qwen3-VL-8B-Instruct` → `Qwen3-VL-8B-Instruct`) for the recognised
/// marker / toast. Verbatim from the retired `core::vision_preprocessor`.
pub fn vl_model_display(model: &str) -> &str {
    match model.rsplit_once('/') {
        Some((_, tail)) if !tail.is_empty() => tail,
        _ => model,
    }
}

/// Run the one-off VL caption call against an already-built provider. Owns the
/// prompt, the local one-shot kernel message (deliberately NOT linked to the
/// main conversation — VL only ever sees this image + caption), and the 30s
/// idle-timeout streaming loop. Returns `Replaced` or `Failed` (never `Skipped`
/// — callers short-circuit via [`should_skip`] before building the provider).
pub async fn run_vl_caption(
    vl_provider: Arc<dyn LlmProvider>,
    vl_model: String,
    caption: &str,
    images: &[ImageContent],
) -> PreprocessOutcome {
    // Prompt text is byte-for-byte the retired core version (marker contract).
    let prompt = if caption.trim().is_empty() {
        "请详细描述这张图片的内容。如果是代码、报错截图或终端输出，请逐字转录文本。".to_string()
    } else {
        format!(
            "用户的当前请求：{caption}\n\n请详细描述这张图片的内容。如果是代码、\
             报错截图或终端输出，请逐字转录文本。",
        )
    };
    let messages = vec![Message::user_with_images(prompt, images.to_vec())];

    // Idle (no-progress) timeout, NOT wall-clock: a healthy slow gateway keeps
    // producing chunks; only abort when nothing arrives for IDLE_TIMEOUT.
    const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

    let mut stream = match vl_provider
        .chat_stream(&messages, &[], &ChatOptions::default())
        .await
    {
        Ok(s) => s,
        Err(e) => {
            return PreprocessOutcome::Failed {
                reason: format!("VL '{vl_model}' stream init failed: {}", e.message),
            };
        }
    };

    let mut buf = String::new();
    loop {
        let next = match tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await {
            Ok(n) => n,
            Err(_) => {
                return PreprocessOutcome::Failed {
                    reason: format!(
                        "VL '{vl_model}' no progress for {}s",
                        IDLE_TIMEOUT.as_secs()
                    ),
                };
            }
        };
        match next {
            None => break,
            Some(StreamEvent::TextDelta(s)) => buf.push_str(&s),
            Some(StreamEvent::Done { .. }) => break,
            Some(StreamEvent::Error(e)) => {
                return PreprocessOutcome::Failed {
                    reason: format!("VL '{vl_model}' call error: {}", e.message),
                };
            }
            // One-shot OCR call: reasoning / usage / tool-call variants are not
            // actionable here (no tools passed). Drop them.
            Some(_) => {}
        }
    }

    let trimmed = buf.trim();
    if trimmed.is_empty() {
        PreprocessOutcome::Failed {
            reason: format!("VL '{vl_model}' returned empty response"),
        }
    } else {
        PreprocessOutcome::Replaced {
            text: trimmed.to_string(),
            vl_model,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::message::Message;
    use atomcode_kernel::provider::{ChatOptions, LlmProvider};
    use atomcode_kernel::stream::{ProviderError, StreamEvent};
    use atomcode_kernel::tool::ToolDef;
    use futures::stream;
    use std::sync::Arc;

    // 脚本化的测试替身：chat_stream 回放预置的 StreamEvent 序列。
    struct ScriptedProvider {
        events: Vec<StreamEvent>,
        init_err: bool,
    }
    // ProviderError has public fields and NO `new` — construct via struct literal.
    fn perr(msg: &str) -> ProviderError {
        ProviderError {
            retryable: false,
            message: msg.into(),
            http_status: None,
            code: None,
            retry_after_secs: None,
        }
    }
    #[async_trait::async_trait]
    impl LlmProvider for ScriptedProvider {
        fn model_name(&self) -> &str {
            "fake-vl"
        }
        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolDef],
            _options: &ChatOptions,
        ) -> Result<futures::stream::BoxStream<'static, StreamEvent>, ProviderError> {
            if self.init_err {
                return Err(perr("init boom"));
            }
            let evs = self.events.clone();
            Ok(Box::pin(stream::iter(evs)))
        }
    }

    fn img() -> ImageContent {
        ImageContent {
            media_type: "image/png".into(),
            data: "AAAA".into(),
        }
    }

    #[test]
    fn vl_model_display_strips_vendor_prefix() {
        assert_eq!(
            vl_model_display("Qwen/Qwen3-VL-8B-Instruct"),
            "Qwen3-VL-8B-Instruct"
        );
        assert_eq!(vl_model_display("qwen-vl-max"), "qwen-vl-max");
        assert_eq!(vl_model_display("a/b/c"), "c");
        assert_eq!(vl_model_display("trailing/"), "trailing/"); // empty tail → whole
    }

    #[test]
    fn should_skip_when_no_images_or_vision_model() {
        assert!(should_skip("glm-4-flash", false), "no images → skip");
        assert!(should_skip("qwen-vl-max", true), "vision model → skip");
        assert!(
            !should_skip("glm-4-flash", true),
            "text model + images → run"
        );
    }

    #[tokio::test]
    async fn run_vl_caption_accumulates_deltas_into_replaced() {
        let p = Arc::new(ScriptedProvider {
            events: vec![
                StreamEvent::TextDelta("你好".into()),
                StreamEvent::TextDelta("世界".into()),
                StreamEvent::Done { truncated: false },
            ],
            init_err: false,
        });
        let out = run_vl_caption(p, "qwen-vl".into(), "看图", &[img()]).await;
        match out {
            PreprocessOutcome::Replaced { text, vl_model } => {
                assert_eq!(text, "你好世界");
                assert_eq!(vl_model, "qwen-vl");
            }
            other => panic!("expected Replaced, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_vl_caption_stream_error_is_failed() {
        let p = Arc::new(ScriptedProvider {
            events: vec![StreamEvent::Error(perr("mid boom"))],
            init_err: false,
        });
        let out = run_vl_caption(p, "qwen-vl".into(), "看图", &[img()]).await;
        assert!(
            matches!(out, PreprocessOutcome::Failed { .. }),
            "mid-stream error → Failed"
        );
    }

    #[tokio::test]
    async fn run_vl_caption_empty_is_failed() {
        let p = Arc::new(ScriptedProvider {
            events: vec![StreamEvent::Done { truncated: false }],
            init_err: false,
        });
        let out = run_vl_caption(p, "qwen-vl".into(), "看图", &[img()]).await;
        assert!(
            matches!(out, PreprocessOutcome::Failed { .. }),
            "empty response → Failed"
        );
    }
}
