//! Vision preprocessing: when the active model can't accept images, run them through a
//! configured VL (vision-language) model first and turn the result into plain text.
//!
//! Kernel-provider native — the v2 replacement for v1's `core::vision_preprocessor`. The
//! CONFIG-dependent short-circuit (is the active model vision-capable? is a VL provider
//! configured?) stays with the driver (it owns the config); this module is the pure VL
//! call + the model-name heuristic, both `atomcode-core`-free.

use std::time::Duration;

use futures::StreamExt;

use atomcode_kernel::message::{ImageContent, Message};
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::stream::StreamEvent;

/// Outcome of a VL preprocessing call. (Unlike v1 there is no `Skipped` — the driver
/// does the config-dependent short-circuit before calling here.)
#[derive(Debug, Clone)]
pub enum PreprocessOutcome {
    /// VL call succeeded. `text` is the raw VL output; `vl_key` labels the provider so
    /// the caller can splice `[图片内容（由 {vl_key} 识别）]`.
    Replaced { text: String, vl_key: String },
    /// VL call failed (stream init / network / timeout / empty). `reason` is intended for
    /// a `Warning` to the user; the caller should fall back to a `[图片识别失败]` placeholder.
    Failed { reason: String },
}

/// Idle (no-progress) timeout, NOT wall-clock — a VL call may take any total duration as
/// long as the stream keeps producing chunks; abort only when nothing arrives for this
/// long (catches a stuck socket without killing a slow-but-healthy gateway).
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Name-based heuristic: does this model accept images natively? Carried over from v1 —
/// the driver uses it to skip preprocessing when the ACTIVE model can see the image itself.
pub fn model_name_suggests_vision(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("vision")
        || n.contains("-vl")
        || n.contains("vl-")
        || n.contains("ocr")
        || n.contains("-4v")
        || n.contains("-4.1v")
        || n.starts_with("gpt-4o")
        // Claude 3 onwards is vision-capable. Anthropic uses two naming forms: the legacy
        // `claude-<gen>-<variant>` (claude-3-5-sonnet) and the newer
        // `claude-<variant>-<gen>-<rev>` (claude-sonnet-4-6).
        || n.starts_with("claude-3")
        || n.starts_with("claude-4")
        || n.starts_with("claude-5")
        || n.starts_with("claude-6")
        || n.starts_with("claude-7")
        || n.starts_with("claude-sonnet")
        || n.starts_with("claude-opus")
        || n.starts_with("claude-haiku")
        || n.starts_with("gemini")
        || n.starts_with("pixtral")
        || n.contains("llava")
        || n.contains("qvq")
}

/// Run a one-shot VL OCR/description call over `images` and return its text. The
/// conversation is LOCAL (image + caption only, never history) — the structural
/// guarantee that VL sees nothing it shouldn't. `vl_key` labels the provider for the
/// caller's splice + error messages.
pub async fn preprocess(
    vl_provider: &dyn LlmProvider,
    vl_key: &str,
    caption: &str,
    images: &[ImageContent],
) -> PreprocessOutcome {
    let prompt = if caption.trim().is_empty() {
        "请详细描述这张图片的内容。如果是代码、报错截图或终端输出，请逐字转录文本。".to_string()
    } else {
        format!(
            "用户的当前请求：{caption}\n\n请详细描述这张图片的内容。如果是代码、\
             报错截图或终端输出，请逐字转录文本。",
        )
    };

    // Local one-shot conversation — image + caption only, NEVER history.
    let messages = vec![Message::user_with_images(prompt, images.to_vec())];

    let mut stream = match vl_provider
        .chat_stream(&messages, &[], &ChatOptions::default())
        .await
    {
        Ok(s) => s,
        Err(e) => {
            return PreprocessOutcome::Failed {
                reason: format!("provider '{vl_key}' stream init failed: {e:?}"),
            }
        }
    };

    let mut buf = String::new();
    loop {
        let next = match tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await {
            Ok(n) => n,
            Err(_) => {
                return PreprocessOutcome::Failed {
                    reason: format!(
                        "provider '{vl_key}' no progress for {}s",
                        IDLE_TIMEOUT.as_secs(),
                    ),
                }
            }
        };
        match next {
            None => break,
            Some(StreamEvent::TextDelta(s)) => buf.push_str(&s),
            Some(StreamEvent::Done { .. }) => break,
            Some(StreamEvent::Error(e)) => {
                return PreprocessOutcome::Failed {
                    reason: format!("provider '{vl_key}' call error: {e:?}"),
                }
            }
            // Reasoning / Usage / ToolCall* — a one-shot OCR call has no use for them.
            Some(_) => {}
        }
    }

    let trimmed = buf.trim();
    if trimmed.is_empty() {
        PreprocessOutcome::Failed {
            reason: format!("provider '{vl_key}' returned empty response"),
        }
    } else {
        PreprocessOutcome::Replaced {
            text: trimmed.to_string(),
            vl_key: vl_key.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::testkit::MockProvider;
    use std::sync::Arc;

    fn img() -> ImageContent {
        ImageContent { media_type: "image/png".into(), data: "ZGF0YQ==".into() }
    }

    // ---- preprocess (kernel-provider) — the rewritten half ----

    #[tokio::test]
    async fn preprocess_returns_replaced_text_from_stream() {
        let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider::new(vec![vec![
            StreamEvent::TextDelta("a screenshot of cargo test output, all green".into()),
            StreamEvent::Done { truncated: false },
        ]]));
        let out = preprocess(provider.as_ref(), "qwen-vl", "what is this?", &[img()]).await;
        match out {
            PreprocessOutcome::Replaced { text, vl_key } => {
                assert_eq!(vl_key, "qwen-vl");
                assert!(text.contains("cargo test"), "got: {text}");
            }
            other => panic!("expected Replaced, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn preprocess_trims_and_concatenates_deltas() {
        let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider::new(vec![vec![
            StreamEvent::TextDelta("  line one\n".into()),
            StreamEvent::TextDelta("line two  ".into()),
            StreamEvent::Done { truncated: false },
        ]]));
        let out = preprocess(provider.as_ref(), "vl", "", &[img()]).await;
        match out {
            PreprocessOutcome::Replaced { text, .. } => assert_eq!(text, "line one\nline two"),
            other => panic!("expected Replaced, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn preprocess_empty_response_is_failed() {
        let provider: Arc<dyn LlmProvider> =
            Arc::new(MockProvider::new(vec![vec![StreamEvent::Done { truncated: false }]]));
        let out = preprocess(provider.as_ref(), "vl", "x", &[img()]).await;
        assert!(matches!(out, PreprocessOutcome::Failed { .. }));
    }

    // ---- model-name heuristic (carried over verbatim from v1) ----

    #[test]
    fn heuristic_recognises_vision_models() {
        for m in [
            "claude-3-5-sonnet",
            "claude-4-opus",
            "claude-sonnet-4-6",
            "gpt-4o",
            "gpt-4o-mini",
            "gpt-4-vision-preview",
            "GLM-4V",
            "glm-4.1v-thinking",
            "Qwen2-VL-7B",
            "deepseek-vl",
            "gemini-2.0-flash",
            "pixtral-12b",
            "llava-1.5",
        ] {
            assert!(model_name_suggests_vision(m), "{m} should be vision-capable");
        }
    }

    #[test]
    fn heuristic_rejects_text_only_models() {
        for m in ["deepseek-chat", "qwen-max", "gpt-3.5-turbo", "o1-mini", "kimi-k2"] {
            assert!(!model_name_suggests_vision(m), "{m} should be text-only");
        }
    }
}
