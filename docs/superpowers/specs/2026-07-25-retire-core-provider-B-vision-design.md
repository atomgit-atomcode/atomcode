# 退役 core::provider — 子项目 B：vision 预处理迁到 kernel-native

> 状态：设计待确认。属 Option 1（删 core::conversation + provider + ctx + vision）的第二个子项目。A（/compact provider）已完成落地。C = /chat transport conversation 迁移 + 删除。
> 目标：把 `core::vision_preprocessor::maybe_preprocess`（用 core provider + core Message）重写为 kernel-native，落在 `atomcode-coding`，然后把 3 个消费者（cli、daemon 两处）重指向它，使它们不再依赖 `core::provider` / `core::vision_preprocessor`。

## 1. 背景

vision 预处理（VL 图片转文字 caption，喂给非视觉模型）目前在 `core::vision_preprocessor::maybe_preprocess`：
- 入参 `config, active_provider: &dyn core::LlmProvider, message, images: &[core::ImagePart]`；
- 短路：空图 → Skipped；active 模型本身是视觉（`model_name_suggests_vision`）→ Skipped；未配 vl provider → Skipped；缺 key → Failed；
- 用 `core::create_provider` 建一次性 VL provider；构造 core `Message{MultiPart{text,images}}`；跑 core `chat_stream`（同步返回 stream）流式循环 + 30s **空闲**超时，累积 `Delta`；
- 返回 `PreprocessOutcome::{Replaced{text,vl_model}, Failed{reason}, Skipped}`。

消费者（3 处，都要脱离 core）：
- **cli** `crates/atomcode-cli/src/vision.rs`：`VlImagePreprocessor` 实现 coding 的 `ImagePreprocessor` seam，内部 `create_provider` + `maybe_preprocess`（core）。outcome→UserInput 的映射（`apply_outcome`）**已是 kernel-native**，可复用。
- **daemon** `live_api.rs:1119 preprocess_image_caption(config, active: &dyn core::LlmProvider, …)` → `maybe_preprocess`（core）。
- **daemon** `live_api.rs:1159 preprocess_live_caption` → `provider::create_provider` 建 active provider 再喂 `preprocess_image_caption`。

kernel-native 目标已就位：`CodingProviderFactory::build`（子项目 A 已用于 /compact）建 kernel provider；kernel `Message` + `ImageContent`；`capabilities::provider::model_suggests_vision`（core `model_name_suggests_vision` 的孪生，已有 parity 测试）；coding 的 `ImagePreprocessor` trait + `VisionNotice`。

## 2. 目标状态

**拆分：parity-critical 流式共享（coding），provider 构造各消费者自理。** 调查确认无"一参建 kernel provider"的原语——必须先组 `CodingAgentConfig` 再 `factory.build(&cfg, session_id)`（build 时绑 session，无 set_session_id）。cli 与 daemon 组 `CodingAgentConfig` 的路径不同（cli 用 `derive_tier_config(&base, &ProviderConfig)`——subagent tier 路由已在用的辅助 provider 原语；daemon 用 `chat_runtime_config(config, name, wd, telemetry).agent_config()`）。故把**风险最高的 VL 流式+prompt+outcome 收敛到 coding 一处**，provider 构造样板留各消费者（纯 config 管道，低 parity 风险）。

- 新 `atomcode-coding/src/vision.rs`：
  - `pub enum PreprocessOutcome { Skipped, Replaced{text,vl_model}, Failed{reason} }`（从 cli 的 `apply_outcome` 匹配语义搬来，cli `apply_outcome` 直接复用）。
  - `pub fn should_skip(active_model: &str, has_images: bool) -> bool`（纯短路：无图 或 主模型本身视觉 `capabilities::provider::model_suggests_vision`）。
  - `pub async fn run_vl_caption(vl_provider: Arc<dyn atomcode_kernel::provider::LlmProvider>, vl_model: String, caption: &str, images: &[ImageContent]) -> PreprocessOutcome`（只返 Replaced|Failed）：构造 prompt（逐字保留 core 中文文案）+ kernel `Message::user_with_images(prompt, images.to_vec())` + **async** `vl_provider.chat_stream(&msgs, &[], &ChatOptions::default())`，消费 `StreamEvent::TextDelta` 累积、`Done`→break、`Error`/空→Failed；30s **空闲**超时（`tokio::time::timeout` 包 `stream.next()`）。
- `atomcode-coding` 放置：coding 已依赖 kernel/capabilities/config/telemetry，有 `CodingProviderFactory`/`derive_tier_config`/`CodingRuntimeConfig::from_config`，cli+daemon 都依赖它。
- **cli** `VlImagePreprocessor`：改为携带 `factory: Arc<dyn CodingProviderFactory>` + `base: CodingAgentConfig`（wiring 时给运行时主 agent config）。`preprocess`：`should_skip`→原样透传；resolve `config.vision_preprocessor_provider`（None→Skipped）；`derive_tier_config(&base, &vl_pc)`→vl agent；`factory.build(&vl_agent, session_id.as_deref())`（Err→Failed）；`run_vl_caption`→**复用现有 `apply_outcome`**。删 `core::provider::create_provider`/`core::vision_preprocessor`/`core::conversation::message::ImagePart`。cli 视觉路径 100% 脱离 core。
- **daemon** `preprocess_live_caption`（真入口，已取 `provider_name: Option<&str>`）：VL provider 改经 `chat_runtime_config(config, vl_name, wd, telemetry).agent_config()`+`factory.build`（替 `provider::create_provider`），调 `run_vl_caption`；`preprocess_image_caption` 的 `active: &dyn core::LlmProvider` 入参改为 `active_model: &str`（只用于短路）。daemon DTO 仍用 `ImagePart`——在调 `run_vl_caption` 处**本地 map ImagePart→ImageContent**（3 字段：media_type/data；不改 daemon DTO 类型，避免 webui ripple）。返回的合并 caption String 语义不变。
- **顺带**：`/chat` preflight `create_provider`（lib.rs:3638，原双用途=校验+vision）——vision 脱 core 后，preflight 若仅剩校验可改 factory 或移除（以实际调用面为准；不强求本子项目删，记录留 C）。

## 3. 关键设计决策

- **home = coding**：vision 预处理天然是"运行时用 provider 做一次 VL 调用"，与 CodingProviderFactory/ImagePreprocessor 同层；不放 capabilities（那样要把 factory 依赖引入 capabilities）。
- **provider 构造复用 A 的链**：`CodingProviderFactory::build`（子项目 A 已验证可行——headless turn 用同一 factory）。VL provider 的配置来自 `config.providers[vision_preprocessor_provider]`，经 `apply_provider_config` 或 `chat_runtime_config` 组 `CodingAgentConfig`。
- **outcome 类型**：`PreprocessOutcome{Replaced{text,vl_model},Failed{reason},Skipped}` 搬到 coding（cli 现有 `apply_outcome` 按此匹配，保持一致）。
- **async 流式**：kernel provider 是 async chat_stream，重写为 async 循环 + `tokio::time::timeout` 空闲守卫（core 版也是空闲超时，语义一致）。

## 4. 行为 parity 契约

- 短路顺序与结果不变（空图/视觉模型/无 vl provider → Skipped；缺 key → Failed）。
- Replaced 的 caption 文本 = VL 模型对 `[图片内容（由 … VL 识别）]` 的输出（与 core 版同 prompt 构造）；失败标记 `[图片识别失败]` 语义不变（cli/daemon 的 apply_outcome 依赖这些 marker——见 tuix split_live_inputs 的 marker 契约）。
- vl_model 名、30s 空闲超时不变。

## 5. 测试

- **单测（coding vision）**：`should_skip` 纯逻辑（空图→true；视觉模型名→true；非视觉模型+有图→false）。`run_vl_caption` 用一个**测试替身** kernel provider（实现 `atomcode_kernel::provider::LlmProvider`，`chat_stream` 返脚本化 `BoxStream`：几个 `TextDelta`+`Done`→断言 `Replaced{text=拼接}`; 直接 `Error`→`Failed`; 空流→`Failed`）——真正测流式累积/终结/错误映射，无网络。
- **单测（cli）**：`apply_outcome` 三分支（已存在，复用不动——保证 marker 文案 parity）。
- **构建**：`cargo build --workspace` + `cargo test --workspace --no-run`（编译所有测试目标）。
- **真机**：真 provider（openrouter/…）+ 配 `vision_preprocessor_provider` 的视觉模型，对文字模型贴图跑一轮，确认 caption 生成、非视觉模型不收原始图字节（tuix split_live_inputs 的 marker 生效）。

## 6. 风险 / 回滚

- **中等风险**：这是真重写（async VL 流式循环）。marker 契约（`[图片内容…]`/`[图片识别失败]`）必须逐字保留，否则 cli/daemon 的 apply_outcome + tuix split_live_inputs 会误判。
- 独立 commit（新 coding 模块 + 3 消费者重指向）；坏了回滚该子项目，不影响 A 与主线。
- core::vision_preprocessor 本体先不删（等消费者归零后，随 C 一并；或 B 末尾确认零消费者后删该文件——见非目标）。

## 7. 非目标（YAGNI）

- 不删 core::conversation / provider / ctx（C）。
- 不改 tuix（已 100% 脱离 core）。
- 不重构 ImagePreprocessor seam / apply_outcome（复用）。
- `/chat` preflight 的彻底原生化若牵扯过多，留给 C（本子项目只保证 vision 路径脱离 core）。
