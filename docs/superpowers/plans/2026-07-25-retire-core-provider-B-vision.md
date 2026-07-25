# 退役 core::provider 子项目B — vision 预处理迁 kernel-native 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 vision VL 预处理从 `core::vision_preprocessor::maybe_preprocess`（core provider + core Message）重写为 kernel-native，落 `atomcode-coding`，把 cli + daemon 两处消费者脱离 `core::provider` / `core::vision_preprocessor` / `core::conversation::ImagePart`。

**Architecture:** parity-critical 的 VL 流式+prompt+outcome 收敛到 coding 一个 `run_vl_caption`（kernel `chat_stream`），provider 构造留各消费者（cli `derive_tier_config`、daemon `chat_runtime_config`——两者组 `CodingAgentConfig` 路径本就不同）。daemon DTO 保留 `ImagePart`，只在调 `run_vl_caption` 前本地 map 到 `ImageContent`，不动 webui DTO。

**Tech Stack:** Rust（workspace edition 2021）、tokio、async-trait。crate：`atomcode-coding`、`atomcode-cli`、`atomcode-daemon`。

## Global Constraints

- **每任务后 workspace 绿**：`cargo build --workspace` 且 `cargo test --workspace --no-run`（编译含测试目标——本项目教训：勿只 `cargo build`）；touched crate 测试套件绿。
- **每任务一提交**；`docs/superpowers` 用 `git add -f`。
- **marker 文案逐字保留**：`[图片内容（由 {vl_model} 识别）]` / `[图片识别失败]` / VL prompt 中文文案不得改一字（cli `apply_outcome` + tuix `split_live_inputs` 按它配对图片）。
- **30s 空闲（非 wall-clock）超时**语义不变。
- **provider 构造网络耦合**：`factory.build` 可能阻塞认证 I/O → `spawn_blocking` 包裹（同旧 `create_provider` 处理）。此部分不做单测，靠编译 + 真机。
- **不删** core::vision_preprocessor 本体（Task 4 确认零消费者后再删；core 内部不引用它）。
- **在 worktree `retire-core-conversation` 内做，push 到 release/v5.0.3**。

---

### Task 1: coding 新增 kernel-native `vision` 模块（`run_vl_caption` + `should_skip` + `PreprocessOutcome`）

**Files:**
- Create: `crates/atomcode-coding/src/vision.rs`
- Modify: `crates/atomcode-coding/src/lib.rs`（`mod vision;` + re-export）

**Interfaces:**
- Consumes（既有）：`atomcode_kernel::provider::LlmProvider`（`async fn chat_stream(&self, &[Message], &[ToolDef], &ChatOptions) -> Result<BoxStream<'static, StreamEvent>, ProviderError>`；`fn model_name(&self)->&str`；`context_window`/`bind_session_id` 有默认）；`atomcode_kernel::message::{Message, ImageContent}`（`Message::user_with_images(text, Vec<ImageContent>)`）；`atomcode_kernel::stream::StreamEvent::{TextDelta(String), Done{..}, Error(ProviderError), Reasoning, Usage, ToolCall, ToolCallDelta, ReasoningSignature}`；`atomcode_kernel::provider::ChatOptions::default()`；`atomcode_capabilities::provider::model_suggests_vision(&str)->bool`；`futures::StreamExt`。
- Produces：`atomcode_coding::vision::{PreprocessOutcome, should_skip, run_vl_caption}`（也从 crate root re-export 供 cli 短写）。

- [ ] **Step 1: 写失败测试（should_skip + run_vl_caption 用测试替身 provider）**

`crates/atomcode-coding/src/vision.rs` 末尾：
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::provider::{ChatOptions, LlmProvider, ProviderError};
    use atomcode_kernel::message::{Message, ToolDef};
    use atomcode_kernel::stream::StreamEvent;
    use futures::stream;
    use std::sync::Arc;

    // 脚本化的测试替身：chat_stream 回放预置的 StreamEvent 序列。
    struct ScriptedProvider {
        events: Vec<StreamEvent>,
        init_err: bool,
    }
    #[async_trait::async_trait]
    impl LlmProvider for ScriptedProvider {
        fn model_name(&self) -> &str { "fake-vl" }
        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolDef],
            _options: &ChatOptions,
        ) -> Result<futures::stream::BoxStream<'static, StreamEvent>, ProviderError> {
            if self.init_err {
                return Err(ProviderError::new("init boom"));
            }
            let evs = self.events.clone();
            Ok(Box::pin(stream::iter(evs)))
        }
    }

    fn img() -> ImageContent {
        ImageContent { media_type: "image/png".into(), data: "AAAA".into() }
    }

    #[test]
    fn should_skip_when_no_images_or_vision_model() {
        assert!(should_skip("glm-4-flash", false), "no images → skip");
        assert!(should_skip("qwen-vl-max", true), "vision model → skip");
        assert!(!should_skip("glm-4-flash", true), "text model + images → run");
    }

    #[tokio::test]
    async fn run_vl_caption_accumulates_deltas_into_replaced() {
        let p = Arc::new(ScriptedProvider {
            events: vec![
                StreamEvent::TextDelta("你好".into()),
                StreamEvent::TextDelta("世界".into()),
                StreamEvent::Done { stop_reason: None, usage: None },
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
            events: vec![StreamEvent::Error(ProviderError::new("mid boom"))],
            init_err: false,
        });
        let out = run_vl_caption(p, "qwen-vl".into(), "看图", &[img()]).await;
        assert!(matches!(out, PreprocessOutcome::Failed { .. }), "mid-stream error → Failed");
    }

    #[tokio::test]
    async fn run_vl_caption_empty_is_failed() {
        let p = Arc::new(ScriptedProvider {
            events: vec![StreamEvent::Done { stop_reason: None, usage: None }],
            init_err: false,
        });
        let out = run_vl_caption(p, "qwen-vl".into(), "看图", &[img()]).await;
        assert!(matches!(out, PreprocessOutcome::Failed { .. }), "empty response → Failed");
    }
}
```
> 注：`StreamEvent::Done { .. }` 与 `ProviderError::new` 的**确切字段/构造**以 `crates/atomcode-kernel/src/stream.rs` 定义为准——实现时先看该文件（`Done` 可能字段名不同；若 `ProviderError::new` 签名不同，用其真实构造）。测试替身只实现 trait 的两个必需方法，其余走默认。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p atomcode-coding vision:: 2>&1 | tail -20`
Expected: 编译失败（`vision` 模块/符号未定义）。

- [ ] **Step 3: 实现 vision.rs**

`crates/atomcode-coding/src/vision.rs` 顶部（tests 之前）：
```rust
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
                reason: format!("VL '{vl_model}' stream init failed: {e:#}"),
            };
        }
    };

    let mut buf = String::new();
    loop {
        let next = match tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await {
            Ok(n) => n,
            Err(_) => {
                return PreprocessOutcome::Failed {
                    reason: format!("VL '{vl_model}' no progress for {}s", IDLE_TIMEOUT.as_secs()),
                };
            }
        };
        match next {
            None => break,
            Some(StreamEvent::TextDelta(s)) => buf.push_str(&s),
            Some(StreamEvent::Done { .. }) => break,
            Some(StreamEvent::Error(e)) => {
                return PreprocessOutcome::Failed {
                    reason: format!("VL '{vl_model}' call error: {e}"),
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
```
> 注：`StreamEvent::Error(e)` 的 `e` 若非直接 `Display`，用其真实字段（以 stream.rs 为准）；`ProviderError` 的 `Display`/`{:#}` 用现有实现。`ChatOptions::default()` 已确认存在（provider.rs 测试 `chat_options_default_is_neutral`）。

- [ ] **Step 4: 注册模块 + re-export**

`crates/atomcode-coding/src/lib.rs`：加 `pub mod vision;`，并在既有 re-export 处加 `pub use vision::{run_vl_caption, should_skip, PreprocessOutcome};`（放到 `ImageContent`/`UserInput` 等 re-export 附近）。

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo test -p atomcode-coding vision:: 2>&1 | tail -20`
Expected: PASS（4 测试）。若 `Done`/`ProviderError` 字段名不符，按编译器修测试与实现一致。

- [ ] **Step 6: 全绿**

Run: `cargo build --workspace && cargo test --workspace --no-run`
Expected: PASS。

- [ ] **Step 7: 提交**

```bash
git add crates/atomcode-coding/src/vision.rs crates/atomcode-coding/src/lib.rs
git commit -m "feat(coding): kernel-native vision run_vl_caption/should_skip（退役 core vision 预处理·基座）"
```

---

### Task 2: cli `VlImagePreprocessor` 迁 kernel-native

**Files:**
- Modify: `crates/atomcode-cli/src/vision.rs`（struct 携带 factory+base；`preprocess` 重写；删 core imports；`apply_outcome` 改用 coding 的 `PreprocessOutcome`；三测试不动）
- Modify: `crates/atomcode-cli/src/main.rs:1832` 与 `:2149`（构造 `VlImagePreprocessor` 时注入 factory + base agent config）

**Interfaces:**
- Consumes：`atomcode_coding::vision::{run_vl_caption, should_skip, PreprocessOutcome}`；`atomcode_coding::{ImageContent, ImagePreprocessor, UserInput, VisionNotice, CodingAgentConfig, CodingProviderFactory}`；`atomcode_coding::provider_factory::derive_tier_config(&CodingAgentConfig, &atomcode_config::config::provider::ProviderConfig) -> CodingAgentConfig`（确认 pub 可达；否则从 coding re-export）；`atomcode_daemon::coding_provider_factory() -> Arc<dyn CodingProviderFactory>`；`atomcode_config::config::Config`。
- Produces：`VlImagePreprocessor{ factory, base }`（wiring 侧构造）。

- [ ] **Step 1: 重写 vision.rs（struct + preprocess），复用 apply_outcome**

把 `crates/atomcode-cli/src/vision.rs` 顶部 imports 与 struct/impl 换为（`apply_outcome` 及其下 `#[cfg(test)]` 三测试**保持不动**，仅把它匹配的 `PreprocessOutcome` 来源从 core 换成 coding——因两者变体同名同形，测试 `use super::*;` 自动跟随）：
```rust
// crates/atomcode-cli/src/vision.rs
//
// Bridges the coding runtime's `ImagePreprocessor` seam to the kernel-native
// `atomcode_coding::vision::run_vl_caption`. The CLI owns building the VL
// provider (via `derive_tier_config` + the coding provider factory) from the
// configured `vision_preprocessor_provider`; the streaming/outcome lives in
// coding. No `atomcode-core` dependency.

use atomcode_coding::provider_factory::derive_tier_config;
use atomcode_coding::vision::{run_vl_caption, should_skip, PreprocessOutcome};
use atomcode_coding::{
    CodingAgentConfig, CodingProviderFactory, ImageContent, ImagePreprocessor, UserInput,
    VisionNotice,
};
use atomcode_config::config::Config;
use std::sync::Arc;

/// VL preprocessing for the local TUI runtime. Carries the provider factory and
/// a base agent config (the runtime's main config) so it can build a one-off VL
/// provider from `config.vision_preprocessor_provider`. Any failure to load
/// config / resolve / build degrades to sending the original `(text, images)`.
pub struct VlImagePreprocessor {
    factory: Arc<dyn CodingProviderFactory>,
    base: CodingAgentConfig,
}

impl VlImagePreprocessor {
    pub fn new(factory: Arc<dyn CodingProviderFactory>, base: CodingAgentConfig) -> Self {
        Self { factory, base }
    }
}

#[async_trait::async_trait]
impl ImagePreprocessor for VlImagePreprocessor {
    async fn preprocess(
        &self,
        text: String,
        images: Vec<ImageContent>,
        active_model: String,
        session_id: Option<String>,
    ) -> (UserInput, Option<VisionNotice>) {
        // Short-circuit: no images or the main model already accepts images.
        if should_skip(&active_model, !images.is_empty()) {
            return (UserInput { text, images }, None);
        }
        let config = match Config::load(&Config::default_path()) {
            Ok(c) => c,
            Err(_) => return (UserInput { text, images }, None),
        };
        // Resolve the configured VL provider entry. Missing ⇒ pass through
        // (Skipped: nothing configured to caption with).
        let Some(vl_name) = config.vision_preprocessor_provider.clone() else {
            return (UserInput { text, images }, None);
        };
        let Some(vl_pc) = config.providers.get(&vl_name).cloned() else {
            return (UserInput { text, images }, None);
        };
        let vl_model = vl_pc.model.clone();
        // Assemble the VL agent config from the base + the VL provider entry
        // (same primitive subagent tier-routing uses), then build the provider.
        // `build` may block on auth I/O → run it off the async owner task.
        let vl_cfg = derive_tier_config(&self.base, &vl_pc);
        let factory = self.factory.clone();
        let sid = session_id.clone();
        let built = tokio::task::spawn_blocking(move || {
            factory.build(&vl_cfg, sid.as_deref())
        })
        .await;
        let provider = match built {
            Ok(Ok(p)) => p,
            _ => {
                // Build failed → Failed marker (non-vision model can't take the raw image).
                return apply_outcome(
                    text,
                    images,
                    PreprocessOutcome::Failed {
                        reason: format!("VL provider '{vl_name}' build failed"),
                    },
                );
            }
        };
        let outcome = run_vl_caption(provider, vl_model, &text, &images).await;
        apply_outcome(text, images, outcome)
    }
}
```
`apply_outcome(text, images, outcome)` 及其下的三个 `#[cfg(test)]` 测试**整段保留不改**（它们已断言 marker 文案与 char_count parity；`PreprocessOutcome` 现来自 coding，变体同形）。

- [ ] **Step 2: 更新两处 wiring 注入 factory+base**

`crates/atomcode-cli/src/main.rs:2149` 处（`CodingRuntimeStart { agent: <cfg>, provider_factory: atomcode_daemon::coding_provider_factory(), image_preprocessor: Some(Arc::new(VlImagePreprocessor)), .. }`）：把 `Arc::new(crate::vision::VlImagePreprocessor)` 换为
```rust
Some(std::sync::Arc::new(crate::vision::VlImagePreprocessor::new(
    atomcode_daemon::coding_provider_factory(),
    <该 start 的 agent CodingAgentConfig>.clone(),
)))
```
`main.rs:1832` 同样处理（该函数在 `1855` 附近有 `_coding_cfg: CodingAgentConfig` 可作 base——若形参名带下划线表示未用，去掉下划线并 `.clone()` 传入）。
> 注：以两处实际可见的 `CodingAgentConfig` 绑定为准（`agent:` 字段的值 / `_coding_cfg`）。`CodingAgentConfig` 是 `#[derive(Clone)]`，可 `.clone()`。

- [ ] **Step 3: 编译 cli + 清孤儿 import**

Run: `cargo build -p atomcode-cli 2>&1 | grep -E "error|warning: unused"`
Expected: 无 error；确认 `grep -nE "atomcode_core" crates/atomcode-cli/src/vision.rs` 为空。

- [ ] **Step 4: cli 测试绿**

Run: `cargo test -p atomcode-cli 2>&1 | grep -E "test result|error\[" | tail`
Expected: PASS（含 vision.rs 的 3 个 apply_outcome 测试）。

- [ ] **Step 5: 全绿**

Run: `cargo build --workspace && cargo test --workspace --no-run`
Expected: PASS。

- [ ] **Step 6: 提交**

```bash
git add crates/atomcode-cli/src/vision.rs crates/atomcode-cli/src/main.rs
git commit -m "refactor(cli): VlImagePreprocessor 迁 kernel-native（derive_tier_config+run_vl_caption，脱 core::provider/vision）"
```

---

### Task 3: daemon vision 两处迁 kernel-native

**Files:**
- Modify: `crates/atomcode-daemon/src/live_api.rs`（`preprocess_image_caption` ~1119：`active: &dyn core::LlmProvider` → `active_model: &str`；`preprocess_live_caption` ~1159：VL provider 改经 factory；两者 `ImagePart` → 本地 map `ImageContent` 调 `run_vl_caption`）

**Interfaces:**
- Consumes：`atomcode_coding::vision::{run_vl_caption, should_skip, PreprocessOutcome}`；`crate::live_api::chat_runtime_config(&Config, &str, &Path, Arc<Telemetry>) -> CodingRuntimeConfig`；`crate::kernel_runtime::coding_config_from_runtime(&CodingRuntimeConfig) -> CodingAgentConfig`；`crate::runtime_host::coding_provider_factory()`；`crate::live_api::resolve_provider_name`；`atomcode_kernel::message::ImageContent`。
- Produces：无对外新接口（内部重写，返回类型 `String` 不变）。

- [ ] **Step 1: 读现状 + 找调用面**

Run:
```bash
sed -n '1119,1200p' crates/atomcode-daemon/src/live_api.rs
grep -n "preprocess_image_caption\|preprocess_live_caption" crates/atomcode-daemon/src/*.rs
```
确认：`preprocess_live_caption(message, images: &[ImagePart], provider_name, session_id)` 是真入口；`preprocess_image_caption(config, active: &dyn core provider, message, images)` 的 `active` 仅用于 vision-capability 短路（`active.model_name()`）与 session。据此定形参改动。

- [ ] **Step 2: 重写 `preprocess_live_caption` 用 factory + run_vl_caption**

将其内部 `let active = ... provider::create_provider ...` + `preprocess_image_caption(... active ...)` 段，改为：解析 VL provider 名（沿用其对 `resolve_provider_name` / `config.vision_preprocessor_provider` 的现有取值逻辑）；`should_skip(active_model, !images.is_empty())` 为真 → 直接返回原 `message`；否则用
```rust
let config = atomcode_config::config::Config::load(&atomcode_config::config::Config::default_path())?; // 若函数已有 config 复用之
let wd = std::env::current_dir().unwrap_or_default();
let coding_cfg = crate::kernel_runtime::coding_config_from_runtime(
    &crate::live_api::chat_runtime_config(&config, &vl_name, &wd, telemetry.clone()),
);
let factory = crate::runtime_host::coding_provider_factory();
let sid = session_id.map(|s| s.to_string());
let provider = match tokio::task::spawn_blocking(move || factory.build(&coding_cfg, sid.as_deref())).await {
    Ok(Ok(p)) => p,
    _ => return message.to_string(), // build 失败：退回原文（同旧 degrade 行为）
};
let kimgs: Vec<atomcode_kernel::message::ImageContent> = images
    .iter()
    .map(|i| atomcode_kernel::message::ImageContent { media_type: i.media_type.clone(), data: i.data.clone() })
    .collect();
let outcome = atomcode_coding::vision::run_vl_caption(provider, vl_name.clone(), message, &kimgs).await;
```
再把 `outcome` map 成返回 String（与旧 `preprocess_image_caption` 相同的合并规则——`Replaced`→`format!("{message}\n\n[图片内容（由 {vl_model} 识别）]\n{text}")`（message 为空则去掉前缀，同 cli `apply_outcome`）；`Failed`→折 `[图片识别失败]`；`Skipped`→原文）。**marker 文案逐字与 cli `apply_outcome` 一致。**
> 若 `telemetry` 在该函数作用域不可得：`preprocess_live_caption` 需加 `telemetry: Arc<Telemetry>` 形参并从调用点（`live_api.rs:1272` 附近的 `live_message`）线程进来（该处有 AppState/telemetry）。以实际作用域为准。

- [ ] **Step 3: `preprocess_image_caption` 形参去 core provider**

若 Step 2 后 `preprocess_image_caption` 已无调用者，删除它；若仍被别处调用，把 `active: &dyn atomcode_core::provider::LlmProvider` 改为 `active_model: &str`，body 内 `active.model_name()` → `active_model`，并同 Step 2 走 factory + `run_vl_caption`。（以 Step 1 的调用面为准二选一。）

- [ ] **Step 4: 编译 daemon + 清孤儿 import**

Run: `cargo build -p atomcode-daemon 2>&1 | grep -E "error|warning: unused"`
Expected: 无 error；删掉 live_api.rs 里孤儿的 `atomcode_core::vision_preprocessor` / `provider::create_provider` / `ImagePart`（若 ImagePart 仍被 DTO 用则保留）import。

- [ ] **Step 5: daemon 测试绿**

Run: `cargo test -p atomcode-daemon 2>&1 | grep -E "test result|error\[" | tail`
Expected: PASS（`preprocess_live_caption_is_passthrough_without_images` 等既有 vision 测试须绿；webui embedded-asset 两测试为**既有环境性失败**，与本改动无关）。

- [ ] **Step 6: 全绿 + 提交**

Run: `cargo build --workspace && cargo test --workspace --no-run`
```bash
git add crates/atomcode-daemon/src/live_api.rs
git commit -m "refactor(daemon): vision 预处理迁 kernel-native factory+run_vl_caption（脱 core::provider/vision）"
```

---

### Task 4: 收口——确认 vision 路径零 core 消费 + 视需要删 core::vision_preprocessor

**Files:**
- Possibly delete: `crates/atomcode-core/src/vision_preprocessor.rs` + `crates/atomcode-core/src/lib.rs` 的 `pub mod vision_preprocessor;`（仅当零消费者）

- [ ] **Step 1: 确认外部零消费**

Run:
```bash
grep -rn "vision_preprocessor\|maybe_preprocess" crates/ --include=*.rs | grep -v "crates/atomcode-core/src/vision_preprocessor.rs" | grep -v "docs/"
```
Expected: 无 cli/daemon 命中（仅 core 内部/测试）。若仍有命中，回到对应 Task 修完。

- [ ] **Step 2: 若 core 内部也不引用，删除模块**

Run: `grep -rn "vision_preprocessor" crates/atomcode-core/src/ | grep -v "vision_preprocessor.rs"`
若仅 `lib.rs` 的 `pub mod` 声明命中（无其它 core 模块引用），删 `crates/atomcode-core/src/vision_preprocessor.rs` 与 lib.rs 声明 + 其 orphan 测试（若有独立 tests 文件引用它一并删）。否则**跳过删除**（留 C），本任务仅确认解耦。

- [ ] **Step 3: 全绿**

Run: `cargo build --workspace && cargo test --workspace --no-run`
Expected: PASS。

- [ ] **Step 4: 提交**

```bash
git add -A
git commit -m "chore(core): vision 路径零 core 消费确认$( )（视情删 core::vision_preprocessor）"
```

- [ ] **Step 5: 真机验证（验收，仅用户可做）**

配 `vision_preprocessor_provider` 为一个视觉模型、主模型为文字模型，TUI 贴图跑一轮：确认生成 `[图片内容（由 … 识别）]` caption、图片不发给文字模型、`✓ VL recognised` toast。daemon/webui 同法贴图确认 caption。

---

## Self-Review 记录

- **Spec 覆盖**：spec §2 = Task1（coding run_vl_caption/should_skip/PreprocessOutcome）+ Task2（cli 重写+wiring）+ Task3（daemon 两处）；spec §7 非目标（不删 conversation/ctx、不改 tuix、复用 apply_outcome、preflight 留 C）在各任务边界明确；Task4 = spec §6 的"确认零消费者后视情删本体"。
- **Placeholder 扫描**：无 TBD；测试替身/流式循环/wiring 均给完整代码。`Done`/`ProviderError` 字段"以 stream.rs 为准"是 defensive note（字段名可能不同），非空话——附了确切定位。
- **类型一致**：`PreprocessOutcome{Skipped,Replaced{text,vl_model},Failed{reason}}` 三处一致（coding 定义、cli apply_outcome 匹配、daemon map）；`run_vl_caption(Arc<dyn LlmProvider>, String, &str, &[ImageContent])` 在 Task1 定义、Task2/3 调用签名一致；`derive_tier_config(&CodingAgentConfig,&ProviderConfig)->CodingAgentConfig`、`factory.build(&CodingAgentConfig, Option<&str>)` 全链已核实。
- **风险顺序**：Task1（新模块+单测，零消费者，纯加法）→ Task2（cli，apply_outcome 测试护 parity）→ Task3（daemon，既有 vision 测试护）→ Task4（收口/删除，可保守跳过）。每步独立可回滚。
