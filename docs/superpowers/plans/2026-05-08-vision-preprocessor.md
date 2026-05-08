# Vision Preprocessor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When the active LLM provider does not accept images and the user pastes an image, route the image through a configurable vision-language model first, splice its description into the user message, and forward as plain text to the main provider.

**Architecture:** New module `atomcode-core::vision_preprocessor` with one async entry point `maybe_preprocess`. One call site in `agent::handle_send_message`. One new optional `Config` field. Failure surfaced via existing `AgentEvent::Warning`. No changes to `LlmProvider` trait, `Conversation`, `coding_plan/setup.rs`, or `MessageContent`.

**Tech Stack:** Rust, `tokio`, `async-trait`, `wiremock` (test-only), existing `OpenAiProvider` for VL calls.

---

## Reference: Spec

Full design at `docs/superpowers/specs/2026-05-08-vision-preprocessor-design.md`. Key decisions encoded here:

- Trigger = `!model_name_suggests_vision(provider.model_name())` AND `!images.is_empty()` AND config field is `Some(non_empty)`.
- VL receives **only** the current-turn caption + images; no main-conversation history.
- VL output appended to user text wrapped as `"\n\n[图片内容（由 Qwen3-VL 识别）]\n{text}"`. Original images dropped.
- On failure: append `"\n\n[图片识别失败]"` to user text, drop images, emit `AgentEvent::Warning(...)`, continue the turn.
- Config field: `vision_preprocessor_provider: Option<String>` at top level of `Config`; `None`/empty string → feature off.

---

## File Structure

| File | Action | Responsibility |
|---|---|---|
| `crates/atomcode-core/src/vision_preprocessor.rs` | **Create** | `PreprocessOutcome` enum + `maybe_preprocess` async function + unit tests |
| `crates/atomcode-core/src/lib.rs` | **Modify** | Add `pub mod vision_preprocessor;` |
| `crates/atomcode-core/src/config/mod.rs` | **Modify** | Add `vision_preprocessor_provider: Option<String>` field to `Config` |
| `crates/atomcode-core/src/agent/mod.rs` | **Modify** | Call `maybe_preprocess` in `handle_send_message` before the existing `if images.is_empty()` branch |

No TUIX changes. No new `AgentEvent` variants. No changes to provider trait or factory.

---

## Task 1: Add `vision_preprocessor_provider` field to `Config`

**Files:**
- Modify: `crates/atomcode-core/src/config/mod.rs:82-125` (the `Config` struct)
- Test: `crates/atomcode-core/src/config/mod.rs` (in existing `#[cfg(test)] mod tests` block, or add one if absent)

- [ ] **Step 1: Locate the existing `Config` test module**

Run: `grep -n "#\[cfg(test)\]\|fn parse_minimal\|mod tests" crates/atomcode-core/src/config/mod.rs | head -20`

Identify whether `mod.rs` already has a test module. If yes, add the new test there. If no, the `provider.rs` next door has one; mirror its style with a new `#[cfg(test)] mod tests { use super::*; ... }` block at file end.

- [ ] **Step 2: Write the failing test**

Add to the test module of `config/mod.rs`:

```rust
#[test]
fn vision_preprocessor_provider_defaults_to_none() {
    // Existing config.toml files (pre-feature) must parse cleanly with
    // `vision_preprocessor_provider` defaulting to None — feature is opt-in
    // and absence must not break load.
    let toml_str = r#"
        default_provider = "claude"
        [providers.claude]
        type = "claude"
        model = "claude-sonnet-4-5"
        api_key = "sk-test"
    "#;
    let cfg: Config = toml::from_str(toml_str).expect("parse minimal config");
    assert_eq!(cfg.vision_preprocessor_provider, None);
}

#[test]
fn vision_preprocessor_provider_round_trips_through_toml() {
    let toml_str = r#"
        default_provider = "claude"
        vision_preprocessor_provider = "AtomGit-Qwen-Qwen3-VL-32B-Instruct"
        [providers.claude]
        type = "claude"
        model = "claude-sonnet-4-5"
        api_key = "sk-test"
    "#;
    let cfg: Config = toml::from_str(toml_str).expect("parse");
    assert_eq!(
        cfg.vision_preprocessor_provider.as_deref(),
        Some("AtomGit-Qwen-Qwen3-VL-32B-Instruct"),
    );
}
```

- [ ] **Step 3: Run tests to verify failure**

Run: `cargo test -p atomcode-core --lib config::mod -- vision_preprocessor`

Expected: compile error — `Config` has no field `vision_preprocessor_provider`.

- [ ] **Step 4: Add the field to `Config`**

Edit `crates/atomcode-core/src/config/mod.rs`. Inside `pub struct Config { ... }` (around line 82–125), append before the closing brace:

```rust
    /// Provider key (matches a key in `Config.providers`) of a vision-language
    /// model used to preprocess images before forwarding to a non-vision main
    /// provider. When `None` or empty, image preprocessing is disabled — pasted
    /// images either go directly to a vision-capable main provider, or get
    /// degraded to `"[image attached]"` placeholder by the existing path.
    ///
    /// Example value: `"AtomGit-Qwen-Qwen3-VL-32B-Instruct"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision_preprocessor_provider: Option<String>,
```

- [ ] **Step 5: Update any `Config { ... }` literals in tests / blank constructors**

Run: `grep -rn "Config {$\|Config {[^}]" crates/atomcode-core/ | grep -v target | grep -v 'Config::' | head -20`

For every blank `Config { ... }` literal that constructs the whole struct without `..Default::default()`, add `vision_preprocessor_provider: None,`. Known locations from `coding_plan/setup.rs::tests::blank_config()` (line ~575). Update each one accordingly.

If there's no `Default` impl on `Config`, this is the entire blast radius. If there IS a `Default` impl, also update it to set the new field to `None`.

- [ ] **Step 6: Run tests to verify pass**

Run: `cargo test -p atomcode-core --lib`

Expected: ALL tests pass (the two new tests + every previous one). If any fail with a missing-field error, revisit step 5.

- [ ] **Step 7: Commit**

```bash
git add crates/atomcode-core/src/config/mod.rs crates/atomcode-core/src/coding_plan/setup.rs
git commit -m "feat(config): add vision_preprocessor_provider field

Optional top-level Config knob naming a provider key used to OCR images
before forwarding to a non-vision main provider. None/empty = feature off
(safe default for existing config.toml files). Wired in subsequent commits.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Create `vision_preprocessor` module skeleton with short-circuit logic

This task lays down the public API and three of the four short-circuit branches (no images / vision-capable main provider / config not set). The fourth branch (provider key not in config) and the actual VL call come in later tasks.

**Files:**
- Create: `crates/atomcode-core/src/vision_preprocessor.rs`
- Modify: `crates/atomcode-core/src/lib.rs:1-30` (add `pub mod vision_preprocessor;`)

- [ ] **Step 1: Add module declaration**

Edit `crates/atomcode-core/src/lib.rs`. Add `pub mod vision_preprocessor;` in alphabetical position (after `pub mod turn;` line 27, before `pub mod uninstall;` line 28). The result around line 27:

```rust
pub mod turn;
pub mod uninstall;
pub mod version_check;
pub mod vision_preprocessor;
```

(Final placement: between `version_check` and end of list. Adjust to keep alphabetical order.)

- [ ] **Step 2: Create module file with public surface + short-circuits + skipped tests**

Create `crates/atomcode-core/src/vision_preprocessor.rs`:

```rust
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
    /// shape: `format!("{caption}\n\n[图片内容（由 Qwen3-VL 识别）]\n{text}")`
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
    use crate::provider::unavailable_provider;
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

    /// Vision-capable main provider via name heuristic (`claude-sonnet-4-5`).
    /// Real provider construction is irrelevant — `unavailable_provider` carries
    /// a model_name of `""` which passes the not-vision branch, so we use a
    /// trivial inline impl that returns the desired model name.
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
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p atomcode-core --lib vision_preprocessor`

Expected: 6 tests pass (`skipped_when_no_images`, `skipped_when_main_provider_accepts_images`, `skipped_when_config_field_unset`, `skipped_when_config_field_empty_string`, `failed_when_configured_key_missing_from_providers`, `key_present_currently_hits_unimplemented_placeholder`).

- [ ] **Step 4: Commit**

```bash
git add crates/atomcode-core/src/vision_preprocessor.rs crates/atomcode-core/src/lib.rs
git commit -m "feat(vision_preprocessor): module skeleton with short-circuit logic

Public API: PreprocessOutcome enum + maybe_preprocess async fn. Implements
the four early-return branches (no images / main provider accepts images /
config field unset or empty / configured key missing → Failed). VL HTTP
call lands in next commit.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Implement the VL HTTP call (happy path)

**Files:**
- Modify: `crates/atomcode-core/src/vision_preprocessor.rs`

- [ ] **Step 1: Add wiremock dependency check**

Run: `grep -n "wiremock" crates/atomcode-core/Cargo.toml`

Expected: existing `wiremock = "0.6"` line under `[dev-dependencies]`. If missing, add it. If wiremock is already a normal dep (it is, per the grep at design time), skip.

- [ ] **Step 2: Replace the placeholder with real VL invocation**

In `crates/atomcode-core/src/vision_preprocessor.rs`, replace the placeholder block. Find:

```rust
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
```

and replace the entire post-`vl_key` portion (from the `if !config.providers.contains_key` line to the closing `}` of `maybe_preprocess`) with:

```rust
    let vl_cfg = match config.providers.get(vl_key) {
        Some(c) => c.clone(),
        None => {
            return PreprocessOutcome::Failed {
                reason: format!("VL provider '{vl_key}' not found in config.providers"),
            };
        }
    };

    use crate::conversation::message::{Message, MessageContent, Role};
    use crate::provider::create_provider;
    use futures::StreamExt;

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
    }];

    let timeout = std::time::Duration::from_secs(30);
    let call = async {
        let mut stream = vl_provider.chat_stream(&messages, None)?;
        let mut buf = String::new();
        while let Some(event) = stream.next().await {
            match event? {
                crate::stream::StreamEvent::Delta(s) => buf.push_str(&s),
                crate::stream::StreamEvent::Reasoning(_) => {} // ignore — VL OCR rarely streams reasoning
                crate::stream::StreamEvent::Done { .. } => break,
                crate::stream::StreamEvent::Error(e) => anyhow::bail!("{e}"),
                _ => {}
            }
        }
        Ok::<_, anyhow::Error>(buf)
    };

    match tokio::time::timeout(timeout, call).await {
        Err(_) => PreprocessOutcome::Failed {
            reason: format!("VL call timed out after {}s", timeout.as_secs()),
        },
        Ok(Err(e)) => PreprocessOutcome::Failed {
            reason: format!("VL call error: {e:#}"),
        },
        Ok(Ok(text)) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                PreprocessOutcome::Failed {
                    reason: "VL returned empty response".into(),
                }
            } else {
                PreprocessOutcome::Replaced {
                    text: trimmed.to_string(),
                }
            }
        }
    }
```

Also delete the placeholder-branch test `key_present_currently_hits_unimplemented_placeholder` from the test module (it served as a trip-wire and is no longer accurate).

The unused-import safety lines `let _ = ReasoningPolicy::Exclude;` etc. inserted in Task 2 should now be deleted — happy-path tests below will exercise the imports.

- [ ] **Step 3: Add a wiremock test for the happy path**

In the same file's `#[cfg(test)] mod tests`, add:

```rust
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Minimal SSE chunk fixture for an OpenAI-compatible /chat/completions
    /// endpoint that returns one `delta.content` token then `[DONE]`.
    fn sse_one_token(text: &str) -> String {
        // Each chunk: `data: {json}\n\n`. Final terminator: `data: [DONE]\n\n`.
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
        format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            chunk, done,
        )
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
            vl_provider_cfg(&format!("{}/", server.uri())),
        );
        cfg.vision_preprocessor_provider = Some("vl".into());

        let provider = StubProvider { model: "deepseek-v4-flash" };
        let result =
            maybe_preprocess(&cfg, &provider, "explain this", &[sample_image()]).await;

        match result {
            PreprocessOutcome::Replaced { text } => {
                assert_eq!(
                    text,
                    "Python stack trace showing ZeroDivisionError on line 42"
                );
            }
            other => panic!("expected Replaced, got {other:?}"),
        }
    }
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p atomcode-core --lib vision_preprocessor`

Expected: 6 tests pass (5 from Task 2 minus the deleted trip-wire test, plus the new `replaced_when_vl_returns_text`).

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-core/src/vision_preprocessor.rs
git commit -m "feat(vision_preprocessor): implement VL HTTP call + happy-path test

Reuses existing OpenAiProvider via create_provider(). VL conversation is a
locally-constructed Vec<Message> with exactly one user turn — structural
guarantee that main-conversation history never reaches the VL endpoint.
Wraps the call in a 30s tokio timeout. Caption-aware prompt template.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Failure tests — HTTP error, timeout, empty response, caption variants

**Files:**
- Modify: `crates/atomcode-core/src/vision_preprocessor.rs` (test module only)

- [ ] **Step 1: Add HTTP-error test**

In the existing `#[cfg(test)] mod tests`, add:

```rust
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
```

- [ ] **Step 2: Add empty-response test**

```rust
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
```

- [ ] **Step 3: Add caption-prompt assertion test**

This test verifies the prompt sent to VL contains the user's caption, by capturing the request body via wiremock's body inspection.

```rust
    #[tokio::test]
    async fn caption_is_included_in_vl_prompt() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("用户的当前请求：解释这段代码"))
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
            .and(wiremock::matchers::body_string_contains("请详细描述这张图片的内容"))
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
```

(No timeout test — `tokio::time::timeout` against `tokio::time::pause` is fragile across versions, and the 30s wall-time isn't worth a real-time test. The placeholder timeout test is a follow-up if real users hit timeouts in practice.)

- [ ] **Step 4: Run tests**

Run: `cargo test -p atomcode-core --lib vision_preprocessor`

Expected: 9 tests pass total (5 short-circuit + 1 happy + 4 failure-mode/caption variants).

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-core/src/vision_preprocessor.rs
git commit -m "test(vision_preprocessor): HTTP error, empty response, caption variants

Adds wiremock-based tests covering: 500 → Failed, empty SSE token →
Failed('empty'), prompt body contains user caption when present, prompt
body uses pure-describe template when caption is whitespace.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: Wire `maybe_preprocess` into `Agent::handle_send_message`

**Files:**
- Modify: `crates/atomcode-core/src/agent/mod.rs` (around line 1266, the existing `if images.is_empty()` site)

- [ ] **Step 1: Locate the call site**

Run: `grep -n "if images.is_empty()" crates/atomcode-core/src/agent/mod.rs`

Confirm the line is in `handle_send_message` (around line 1266 per current code).

- [ ] **Step 2: Insert the preprocessing call before that branch**

In `crates/atomcode-core/src/agent/mod.rs`, at the existing site that currently reads:

```rust
        if images.is_empty() {
            self.conversation.add_user_message(&clean);
        } else {
            use crate::conversation::message::{Message, MessageContent, Role};
            let msg = Message {
                role: Role::User,
                content: MessageContent::MultiPart {
                    text: if clean.is_empty() { None } else { Some(clean.clone()) },
                    images,
                },
            };
            ...
        }
```

Replace with:

```rust
        // Vision preprocessing: when the active provider can't accept images
        // and the user pasted some, run them through the configured VL model
        // first and turn the result into plain text. See
        // `vision_preprocessor` module doc for the data-flow contract.
        let (clean, images) = if !images.is_empty() {
            use crate::vision_preprocessor::{maybe_preprocess, PreprocessOutcome};
            match maybe_preprocess(
                &self.config,
                self.turn_runner.provider.as_ref(),
                &clean,
                &images,
            )
            .await
            {
                PreprocessOutcome::Skipped => (clean, images),
                PreprocessOutcome::Replaced { text } => {
                    let merged = if clean.is_empty() {
                        format!("[图片内容（由 Qwen3-VL 识别）]\n{text}")
                    } else {
                        format!("{clean}\n\n[图片内容（由 Qwen3-VL 识别）]\n{text}")
                    };
                    (merged, Vec::new())
                }
                PreprocessOutcome::Failed { reason } => {
                    let _ = self
                        .event_tx
                        .send(AgentEvent::Warning(format!("VL 预处理失败：{reason}")));
                    let merged = if clean.is_empty() {
                        "[图片识别失败]".to_string()
                    } else {
                        format!("{clean}\n\n[图片识别失败]")
                    };
                    (merged, Vec::new())
                }
            }
        } else {
            (clean, images)
        };

        if images.is_empty() {
            self.conversation.add_user_message(&clean);
        } else {
            use crate::conversation::message::{Message, MessageContent, Role};
            let msg = Message {
                role: Role::User,
                content: MessageContent::MultiPart {
                    text: if clean.is_empty() { None } else { Some(clean.clone()) },
                    images,
                },
            };
            let idx = self.conversation.messages.len();
            self.conversation.messages.push(msg);
            self.conversation.turn_tracker.on_user_message(idx);
        }
```

- [ ] **Step 3: Build to confirm it compiles**

Run: `cargo build -p atomcode-core`

Expected: success. If borrow-checker complains about `&self.config` while `self.event_tx.send` is called inside the same scope, refactor by storing the warning string in a local and emitting after the match: 

```rust
let mut warning: Option<String> = None;
let (clean, images) = if !images.is_empty() {
    match maybe_preprocess(&self.config, self.turn_runner.provider.as_ref(), &clean, &images).await {
        // ...
        PreprocessOutcome::Failed { reason } => {
            warning = Some(format!("VL 预处理失败：{reason}"));
            // ...
        }
        // ...
    }
} else { (clean, images) };
if let Some(w) = warning {
    let _ = self.event_tx.send(AgentEvent::Warning(w));
}
```

- [ ] **Step 4: Run the existing agent tests to make sure nothing regressed**

Run: `cargo test -p atomcode-core --lib agent`

Expected: all existing tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-core/src/agent/mod.rs
git commit -m "feat(agent): route images through vision_preprocessor before send

In handle_send_message, when the user submitted images, call
maybe_preprocess() with the active provider + caption + images. On
Replaced, splice VL text into the user message and drop images so the
turn proceeds as plain text. On Failed, append [图片识别失败] and emit
AgentEvent::Warning so the user understands why the placeholder appeared.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: Smoke-test build + clippy across all crates

**Files:**
- (None — verification only.)

- [ ] **Step 1: Full workspace build**

Run: `cargo build --workspace --all-targets`

Expected: success.

- [ ] **Step 2: Full workspace test**

Run: `cargo test --workspace --all-targets`

Expected: success. If a `Config { ... }` literal somewhere in TUIX or CLI fixtures was missed in Task 1 step 5, this is where it surfaces. Fix in place by adding `vision_preprocessor_provider: None`.

- [ ] **Step 3: Clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`

Expected: no warnings. Common issues to fix inline:
- Unused `use` import in `vision_preprocessor.rs` (clean up).
- `clippy::needless_borrow` on the `&clean` argument (drop the `&` if clippy demands).

- [ ] **Step 4: If anything required fixing in steps 1-3, commit those fixups**

```bash
git add -A
git commit -m "fix(vision_preprocessor): clippy + build cleanups

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

(Skip the commit if no fixups were needed.)

---

## Manual Integration Verification

(Not a checklist task — runs once after the plan is fully merged. The PR description's Test Plan must include these steps.)

1. `cargo run -p atomcode-cli --release` to enter TUI.
2. `/codingplan` to install AtomGit providers.
3. Manually add a `[providers."AtomGit-Qwen-Qwen3-VL-32B-Instruct"]` block in `~/.atomcode/config.toml` pointing at the AtomGit gateway with model `Qwen/Qwen3-VL-32B-Instruct`. (Or rename to a non-`AtomGit-` prefix to survive `/codingplan` re-runs — e.g. `vl-qwen3vl`.)
4. Add a top-level `vision_preprocessor_provider = "AtomGit-Qwen-Qwen3-VL-32B-Instruct"` (or whatever key you used).
5. `/model AtomGit-DeepSeek-V4-flash` (or any non-vision provider).
6. Ctrl+V paste a code-screenshot, append caption "解释这段代码", press Enter.
7. **Expected:** scrollback shows the user message containing both `解释这段代码` and a `[图片内容（由 Qwen3-VL 识别）]\n...` block; `/datalog tail` shows the request to DeepSeek is plain text only (no `image_url` block); main model replies coherently about the code.
8. Comment out `vision_preprocessor_provider` in config and re-run step 6. **Expected:** DeepSeek receives `[image attached]` placeholder (existing fallback path); main model has no image context.
9. Set `vision_preprocessor_provider = "AtomGit-NoSuchModel"` (typo). Re-run step 6. **Expected:** yellow `Warning` line: `VL 预处理失败：VL provider 'AtomGit-NoSuchModel' not found in config.providers`; user message ends with `[图片识别失败]`; main model still replies (asking for clarification, presumably).
10. With `/model claude-sonnet-4-5` (vision-capable) and `vision_preprocessor_provider` set, re-run step 6. **Expected:** preprocessing skipped (no Notice, no `[图片内容...]` wrapper); image goes natively to Claude.

---

## Self-Review Checklist (run before handoff)

This was performed during plan writing — leaving the checklist documented for re-verification.

**1. Spec coverage:**
- Goal/trigger conditions → Tasks 2 short-circuits + Task 5 wire-up. ✓
- Caption included in VL prompt → Task 3 prompt template + Task 4 caption test. ✓
- VL only sees current image, not history → Task 3 local `Vec<Message>` (structural guarantee). ✓
- VL output appended, image dropped → Task 5 `Replaced` arm. ✓
- Failure → Warning + placeholder → Task 5 `Failed` arm. ✓
- Config field at top level, defaults None, opt-in → Task 1. ✓
- 30s timeout → Task 3 `tokio::time::timeout`. ✓
- No `[image attached]` change for vision-capable case → Task 5 `Skipped` arm preserves existing path; Task 1's serde `skip_serializing_if = Option::is_none` keeps existing config files clean. ✓
- Non-target: no `LlmProvider` trait change. ✓
- Non-target: no `coding_plan/setup.rs` change. ✓

**2. Placeholder scan:** No "TBD"/"TODO"/"add error handling" in any task. ✓

**3. Type consistency:**
- `LlmProvider` (trait), not `Provider`. Used consistently in Task 2 + 5. ✓
- `model_name_suggests_vision` (free function, not method). ✓
- `StreamEvent::Delta(String)` not `TextDelta`. ✓ (TextDelta is the `AgentEvent` variant; provider-side stream uses `Delta`.)
- `create_provider` returns `Result<Box<dyn LlmProvider>>`. Task 3 uses it correctly. ✓
- `AgentEvent::Warning(String)` (tuple variant), not struct. Task 5 uses `AgentEvent::Warning(format!(...))`. ✓
