//! VL-model image preprocessor.
//!
//! When the active main provider does not accept images and the user submits
//! an image, this module routes the image (plus the current-turn caption only)
//! through a configurable vision-language provider, returning a textual
//! description that callers splice into the user message before forwarding to
//! the main provider as plain text.
//!
//! Key invariant: the VL call NEVER sees the main conversation history. The
//! `Vec<Message>` passed to the VL provider is constructed locally from
//! `caption + images` and contains exactly one user turn.

use crate::config::Config;
use crate::conversation::message::ImagePart;
use crate::provider::{model_name_suggests_vision, LlmProvider};

/// Outcome of a preprocessing attempt.
#[derive(Debug, Clone)]
pub enum PreprocessOutcome {
    /// Preprocessing did not run — feature disabled, main provider already
    /// accepts images, or no images attached. Caller must use the original
    /// `(caption, images)` tuple unchanged.
    Skipped,
    /// VL call succeeded. `text` is the raw VL output (no wrapping). Caller
    /// is responsible for splicing it into the user message — recommended
    /// shape: `format!("{caption}\n\n[图片内容（由 VL 模型识别）]\n{text}")`
    /// — and clearing the images vec.
    Replaced { text: String },
    /// VL call failed (provider missing, network error, timeout, empty
    /// response). `reason` is intended for `AgentEvent::Warning`. Caller
    /// should append `"\n\n[图片识别失败]"` to the user message and clear
    /// images so the turn proceeds with a useful placeholder.
    Failed { reason: String },
}

/// Decide whether and how to preprocess images before a main-provider turn.
///
/// Short-circuit order (each → `Skipped`, except the last):
/// 1. `images` is empty.
/// 2. The active provider's model name passes the `model_name_suggests_vision`
///    heuristic (it can handle the image natively).
/// 3. `config.vision_preprocessor_provider` is `None` or `Some("")`.
/// 4. The configured key is missing from `config.providers` → `Failed` (this
///    is a configuration mistake worth surfacing, not a silent skip).
pub async fn maybe_preprocess(
    config: &Config,
    active_provider: &dyn LlmProvider,
    caption: &str,
    images: &[ImagePart],
) -> PreprocessOutcome {
    let _ = caption; // used in Task 3 prompt template
    if images.is_empty() {
        return PreprocessOutcome::Skipped;
    }
    if model_name_suggests_vision(active_provider.model_name()) {
        return PreprocessOutcome::Skipped;
    }
    let vl_key = match config.vision_preprocessor_provider.as_deref() {
        Some(k) if !k.is_empty() => k,
        _ => return PreprocessOutcome::Skipped,
    };
    if !config.providers.contains_key(vl_key) {
        return PreprocessOutcome::Failed {
            reason: format!("VL provider '{vl_key}' not found in config.providers"),
        };
    }
    // VL HTTP call lands in Task 3 — for now, signal that we got past all
    // short-circuits but haven't yet implemented the call.
    PreprocessOutcome::Failed {
        reason: "VL call not yet implemented".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::provider::ProviderConfig;
    use std::collections::HashMap;

    fn blank_config() -> Config {
        // Mirrors `coding_plan::setup::tests::blank_config` but kept local
        // so this test module does not reach into another module's private test
        // helpers. If new mandatory fields are added to Config, update both.
        Config {
            default_provider: String::new(),
            default_workdir: None,
            providers: HashMap::new(),
            datalog: Default::default(),
            auto_update: true,
            notifications: Default::default(),
            telemetry: Default::default(),
            lsp: Default::default(),
            auto_commit: false,
            subagent: Default::default(),
            vision_preprocessor_provider: None,
        }
    }

    fn sample_image() -> ImagePart {
        ImagePart {
            media_type: "image/png".into(),
            data: "iVBORw0KGgoAAAANSUhEUg==".into(),
        }
    }

    /// Stub `LlmProvider` that only carries a model name — chat_stream is
    /// never called in short-circuit tests, but the trait requires the impl.
    struct StubProvider {
        model: &'static str,
    }
    use crate::stream::StreamEvent;
    use crate::tool::ToolDef;
    use anyhow::Result;
    use async_trait::async_trait;
    use futures::Stream;
    use std::pin::Pin;
    #[async_trait]
    impl LlmProvider for StubProvider {
        fn chat_stream(
            &self,
            _messages: &[crate::conversation::message::Message],
            _tools: Option<&[ToolDef]>,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
            anyhow::bail!("stub never streams");
        }
        fn model_name(&self) -> &str {
            self.model
        }
    }

    #[tokio::test]
    async fn skipped_when_no_images() {
        let cfg = blank_config();
        let provider = StubProvider { model: "deepseek-v4-flash" };
        let result = maybe_preprocess(&cfg, &provider, "any caption", &[]).await;
        assert!(matches!(result, PreprocessOutcome::Skipped));
    }

    #[tokio::test]
    async fn skipped_when_main_provider_accepts_images() {
        let cfg = blank_config();
        let provider = StubProvider { model: "claude-sonnet-4-5" };
        let result =
            maybe_preprocess(&cfg, &provider, "describe", &[sample_image()]).await;
        assert!(matches!(result, PreprocessOutcome::Skipped));
    }

    #[tokio::test]
    async fn skipped_when_config_field_unset() {
        let cfg = blank_config();
        let provider = StubProvider { model: "deepseek-v4-flash" };
        let result =
            maybe_preprocess(&cfg, &provider, "describe", &[sample_image()]).await;
        assert!(matches!(result, PreprocessOutcome::Skipped));
    }

    #[tokio::test]
    async fn skipped_when_config_field_empty_string() {
        let mut cfg = blank_config();
        cfg.vision_preprocessor_provider = Some(String::new());
        let provider = StubProvider { model: "deepseek-v4-flash" };
        let result =
            maybe_preprocess(&cfg, &provider, "describe", &[sample_image()]).await;
        assert!(matches!(result, PreprocessOutcome::Skipped));
    }

    #[tokio::test]
    async fn failed_when_configured_key_missing_from_providers() {
        let mut cfg = blank_config();
        cfg.vision_preprocessor_provider = Some("AtomGit-NoSuchModel".into());
        let provider = StubProvider { model: "deepseek-v4-flash" };
        let result =
            maybe_preprocess(&cfg, &provider, "describe", &[sample_image()]).await;
        match result {
            PreprocessOutcome::Failed { reason } => {
                assert!(
                    reason.contains("AtomGit-NoSuchModel") && reason.contains("not found"),
                    "expected 'not found' for missing key, got: {reason}",
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// Regression marker for Task 3: this test currently passes the "VL call
    /// not yet implemented" placeholder branch. After Task 3 lands, it must
    /// be replaced/removed since the placeholder branch goes away.
    #[tokio::test]
    async fn key_present_currently_hits_unimplemented_placeholder() {
        let mut cfg = blank_config();
        cfg.providers.insert(
            "vl-stub".into(),
            ProviderConfig {
                provider_type: "openai".into(),
                api_key: Some("sk-test".into()),
                model: "Qwen/Qwen3-VL-32B-Instruct".into(),
                base_url: Some("http://127.0.0.1:1/".into()),
                system_prompt: None,
                user_agent: None,
                context_window: 8000,
                max_tokens: None,
                thinking_type: None,
                thinking_keep: None,
                reasoning_history: None,
                thinking_enabled: None,
                thinking_budget: None,
                skip_tls_verify: false,
                ephemeral: false,
            },
        );
        cfg.vision_preprocessor_provider = Some("vl-stub".into());
        let provider = StubProvider { model: "deepseek-v4-flash" };
        let result =
            maybe_preprocess(&cfg, &provider, "describe", &[sample_image()]).await;
        assert!(matches!(result, PreprocessOutcome::Failed { .. }));
    }
}
