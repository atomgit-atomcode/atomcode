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
use crate::conversation::message::{ImagePart, Message, MessageContent, Role};
use crate::provider::{create_provider, model_name_suggests_vision, LlmProvider};
use futures::StreamExt;

/// Outcome of a preprocessing attempt.
#[derive(Debug, Clone)]
pub enum PreprocessOutcome {
    /// Preprocessing did not run — feature disabled, main provider already
    /// accepts images, or no images attached. Caller must use the original
    /// `(caption, images)` tuple unchanged.
    Skipped,
    /// VL call succeeded. `text` is the raw VL output (no wrapping);
    /// `vl_key` is the provider key used (so the caller can show "by
    /// {model}" in the splice wrapper). Caller is responsible for
    /// splicing both into the user message — recommended shape:
    /// `format!("{caption}\n\n[图片内容（由 {vl_key} 识别）]\n{text}")`
    /// — and clearing the images vec.
    Replaced { text: String, vl_key: String },
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
    let vl_cfg = match config.providers.get(vl_key) {
        Some(c) => c.clone(),
        None => {
            return PreprocessOutcome::Failed {
                reason: format!("VL provider '{vl_key}' not found in config.providers"),
            };
        }
    };

    // Build a one-off VL provider. `create_provider` handles auth-token
    // loading (api_key=None) for the AtomGit gateway case.
    let vl_provider = match create_provider(&vl_cfg) {
        Ok(p) => p,
        Err(e) => {
            return PreprocessOutcome::Failed {
                reason: format!("VL provider build failed: {e:#}"),
            };
        }
    };

    let prompt = if caption.trim().is_empty() {
        "请详细描述这张图片的内容。如果是代码、报错截图或终端输出，请逐字转录文本。"
            .to_string()
    } else {
        format!(
            "用户的当前请求：{caption}\n\n请详细描述这张图片的内容。如果是代码、\
             报错截图或终端输出，请逐字转录文本。",
        )
    };

    // Local one-shot conversation — explicitly NOT linked to the main
    // `agent.conversation.messages`. This is the structural guarantee that
    // VL only sees the current image + caption, never history.
    let messages = vec![Message {
        role: Role::User,
        content: MessageContent::MultiPart {
            text: Some(prompt),
            images: images.to_vec(),
        },
        synthetic: false,
    }];

    // Idle (no-progress) timeout, NOT wall-clock. A VL call can take any
    // total duration as long as the stream keeps producing chunks — we only
    // abort when no event has arrived for `IDLE_TIMEOUT`. The previous 30s
    // wall-clock killed perfectly healthy slow gateways: a Qwen3-VL cold
    // start can spend 10-15s on TTFT, then another 10-20s OCR-ing a dense
    // screenshot, easily clearing 30s end-to-end while streaming the whole
    // way through. Idle-timeout still catches genuinely stuck sockets
    // (gateway accepted the request, holds the connection, never produces
    // tokens) — that's the failure mode worth aborting on.
    const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

    let mut stream = match vl_provider.chat_stream(&messages, None) {
        Ok(s) => s,
        Err(e) => {
            return PreprocessOutcome::Failed {
                reason: format!("provider '{vl_key}' stream init failed: {e:#}"),
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
                        "provider '{vl_key}' no progress for {}s",
                        IDLE_TIMEOUT.as_secs(),
                    ),
                };
            }
        };
        let event = match next {
            None => break,
            Some(Ok(ev)) => ev,
            Some(Err(e)) => {
                return PreprocessOutcome::Failed {
                    reason: format!("provider '{vl_key}' call error: {e:#}"),
                };
            }
        };
        match event {
            crate::stream::StreamEvent::Delta(s) => buf.push_str(&s),
            crate::stream::StreamEvent::Reasoning(_) => {}
            crate::stream::StreamEvent::Done { .. } => break,
            crate::stream::StreamEvent::Error(e) => {
                return PreprocessOutcome::Failed {
                    reason: format!("provider '{vl_key}' call error: {e}"),
                };
            }
            // VL is a one-shot OCR call — Warnings (e.g., proxy truncation
            // heuristics) and Usage stats are not actionable for the user
            // here; tool-call variants don't apply because we pass `None`
            // for tools. Drop them.
            crate::stream::StreamEvent::Warning(_)
            | crate::stream::StreamEvent::Usage(_)
            | crate::stream::StreamEvent::ThinkingBlock { .. }
            | crate::stream::StreamEvent::ToolCallStart { .. }
            | crate::stream::StreamEvent::ToolCallDelta(_)
            | crate::stream::StreamEvent::ToolCallDone(_) => {}
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
            language: None,
            ui: Default::default(),
            plugin: Default::default(),
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

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Minimal SSE chunk fixture for an OpenAI-compatible /chat/completions
    /// endpoint that returns one `delta.content` token then a stop chunk
    /// then `[DONE]`. Mirrors the wire shape `OpenAiProvider` consumes.
    fn sse_one_token(text: &str) -> String {
        let chunk = serde_json::json!({
            "choices": [{
                "delta": { "content": text },
                "finish_reason": null,
            }],
        });
        let done = serde_json::json!({
            "choices": [{
                "delta": {},
                "finish_reason": "stop",
            }],
        });
        format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n", chunk, done)
    }

    fn vl_provider_cfg(base_url: &str) -> ProviderConfig {
        ProviderConfig {
            provider_type: "openai".into(),
            api_key: Some("sk-test".into()),
            model: "Qwen/Qwen3-VL-32B-Instruct".into(),
            base_url: Some(base_url.to_string()),
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
        }
    }

    #[tokio::test]
    async fn replaced_when_vl_returns_text() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_one_token(
                        "Python stack trace showing ZeroDivisionError on line 42",
                    )),
            )
            .expect(1)
            .mount(&server)
            .await;

        let mut cfg = blank_config();
        cfg.providers.insert(
            "vl".into(),
            vl_provider_cfg(&server.uri()),
        );
        cfg.vision_preprocessor_provider = Some("vl".into());

        let provider = StubProvider { model: "deepseek-v4-flash" };
        let result =
            maybe_preprocess(&cfg, &provider, "explain this", &[sample_image()]).await;

        match result {
            PreprocessOutcome::Replaced { text, vl_key } => {
                assert_eq!(
                    text,
                    "Python stack trace showing ZeroDivisionError on line 42"
                );
                assert_eq!(vl_key, "vl", "Replaced must carry the configured key");
            }
            other => panic!("expected Replaced, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn failed_when_vl_returns_500() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(500).set_body_string("upstream error"))
            // Existing OpenAI provider may retry per its retry::RetryPolicy.
            // Don't pin .expect(N); just assert the eventual outcome.
            .mount(&server)
            .await;

        let mut cfg = blank_config();
        cfg.providers.insert(
            "vl".into(),
            vl_provider_cfg(&format!("{}/", server.uri())),
        );
        cfg.vision_preprocessor_provider = Some("vl".into());

        let provider = StubProvider { model: "deepseek-v4-flash" };
        let result =
            maybe_preprocess(&cfg, &provider, "x", &[sample_image()]).await;

        match result {
            PreprocessOutcome::Failed { reason } => {
                assert!(
                    reason.contains("VL call error") || reason.contains("500"),
                    "expected error reason mentioning failure, got: {reason}",
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn failed_when_vl_returns_empty_string() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_one_token("")), // empty token then [DONE]
            )
            .mount(&server)
            .await;

        let mut cfg = blank_config();
        cfg.providers.insert(
            "vl".into(),
            vl_provider_cfg(&format!("{}/", server.uri())),
        );
        cfg.vision_preprocessor_provider = Some("vl".into());

        let provider = StubProvider { model: "deepseek-v4-flash" };
        let result =
            maybe_preprocess(&cfg, &provider, "x", &[sample_image()]).await;

        match result {
            PreprocessOutcome::Failed { reason } => {
                assert!(
                    reason.contains("empty"),
                    "expected 'empty' in reason, got: {reason}",
                );
            }
            other => panic!("expected Failed for empty response, got {other:?}"),
        }
    }

    /// Custom matcher for request body containing a substring.
    use wiremock::Match;
    struct BodyContains(String);
    impl Match for BodyContains {
        fn matches(&self, req: &wiremock::Request) -> bool {
            String::from_utf8_lossy(&req.body).contains(&self.0)
        }
    }

    /// Inverse of `BodyContains` — matches when the request body does NOT
    /// include the substring. Pairs with `BodyContains` to assert that one
    /// prompt template was selected and the other was not.
    struct BodyNotContains(String);
    impl Match for BodyNotContains {
        fn matches(&self, request: &wiremock::Request) -> bool {
            !String::from_utf8_lossy(&request.body).contains(&self.0)
        }
    }

    #[tokio::test]
    async fn caption_is_included_in_vl_prompt() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(BodyContains("用户的当前请求：解释这段代码".into()))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_one_token("ok")),
            )
            .expect(1)
            .mount(&server)
            .await;

        let mut cfg = blank_config();
        cfg.providers.insert(
            "vl".into(),
            vl_provider_cfg(&format!("{}/", server.uri())),
        );
        cfg.vision_preprocessor_provider = Some("vl".into());

        let provider = StubProvider { model: "deepseek-v4-flash" };
        let result = maybe_preprocess(
            &cfg,
            &provider,
            "解释这段代码",
            &[sample_image()],
        )
        .await;

        // Replaced confirms the body matched the caption pattern (otherwise
        // wiremock would reject the request and the call would fail).
        assert!(matches!(result, PreprocessOutcome::Replaced { .. }));
    }

    #[tokio::test]
    async fn empty_caption_uses_pure_describe_prompt() {
        let server = MockServer::start().await;
        // Pure describe prompt — must NOT contain the "用户的当前请求：" prefix.
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(BodyContains("请详细描述这张图片的内容".into()))
            .and(BodyNotContains("用户的当前请求：".into()))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_one_token("ok")),
            )
            .expect(1)
            .mount(&server)
            .await;

        let mut cfg = blank_config();
        cfg.providers.insert(
            "vl".into(),
            vl_provider_cfg(&format!("{}/", server.uri())),
        );
        cfg.vision_preprocessor_provider = Some("vl".into());

        let provider = StubProvider { model: "deepseek-v4-flash" };
        let result = maybe_preprocess(&cfg, &provider, "  ", &[sample_image()]).await;

        assert!(matches!(result, PreprocessOutcome::Replaced { .. }));
    }
}
