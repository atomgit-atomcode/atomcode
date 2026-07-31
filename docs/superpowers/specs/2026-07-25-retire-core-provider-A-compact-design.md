# 退役 core::provider — 子项目 A：/compact 摘要 provider 迁到 kernel-native factory

> 状态：设计已确认，待写实施计划。
> 属 Option 1（删除 core::conversation + provider + ctx + vision）的第一个子项目 A。B=vision、C=conversation+删除，各自独立 spec。
> 目标：把 daemon `/compact` 的摘要 provider 从 `core::provider::create_provider` + `KernelSummaryProvider` adapter 切到已存在的 kernel-native `CodingProviderFactory`，删掉该 adapter。

## 1. 背景

`atomcode-core` 正逐模块退役。`core::conversation` 删不掉，因为被 core 内部的 `provider/`、`ctx/`、`vision_preprocessor` 使用，而这些仍被 daemon/cli 消费。Option 1 = 迁走这三块。cluster 调查（见 `docs/superpowers/specs/` 同期 map，或本 spec §2）确认 daemon 对 `core::provider` 的消费有三个**独立**的活：

1. **`/compact` 摘要 provider**（本子项目 A）——最小、最净：纯 swap 到现有 factory，删一个 adapter。
2. **vision 预处理**（子项目 B）——真实重写。
3. **`/chat` preflight 校验 provider**（与 vision 纠缠，B 处理）。

`core::provider` 与 kernel `LlmProvider` 是**两个不同的 trait**（core 同步返回 stream/无 options；kernel async/带 ChatOptions/带 context_window）。`KernelSummaryProvider`（commands.rs:14-74）正是把 core provider 适配成 kernel trait 的转换器。v2 目标 `CodingProviderFactory::build` 直接产出 kernel `LlmProvider`（原生 provider + gateway 签名 + 正确 context_window），**无需 adapter**。

## 2. 目标状态

- `exec_native_compact`（commands.rs）用 daemon 既有的原生 provider 构造链建 provider：
  `chat_runtime_config(&config, &resolved_provider, working_dir, telemetry)` → `kernel_runtime::coding_config_from_runtime(&cfg)` → `runtime_host::coding_provider_factory().build(&coding_cfg, None)` → `Arc<dyn atomcode_kernel::provider::LlmProvider>`，直接喂给已是 kernel-native 的 `atomcode_coding::runtime::compact_snapshot`。
  这与 native `/chat`（native_live.rs:378 用 chat_runtime_config）是**同一条构造链**。
- **删除 `KernelSummaryProvider` struct + impl（commands.rs:14-74）** 及其对 `legacy_convert::message_to_core` 的桥接使用（原用于 core→kernel 消息转换，native provider 不需要）。
- `lib.rs:2509` 的 `atomcode_core::provider::openai::OpenAiProvider::reason_effort_applicable(&p.model)` → `atomcode_capabilities::provider::reason_effort_applicable(&p.model)`（Option 2 已把该 fn 放开为 pub 并 re-export）。
- **不碰** vision（live_api.rs 的 `preprocess_image_caption`/`preprocess_live_caption`）、`/chat` preflight（lib.rs:3638）——属 B。**不删** core::provider/ctx/conversation 任何模块——属 C。

## 3. 关键改动点

- `exec_native_compact` 签名需要 `working_dir: &Path` 与 `telemetry: Arc<Telemetry>`（`chat_runtime_config` 需要）。这是**重新加回**之前 /compact 解耦时移除的参数，但这次是为 factory 路径。调用者 `exec_compact` 已有 `working_dir`；`telemetry` 从 `AppState` 取——需把 `state`（或 `state.telemetry` + working_dir）沿 `run_command` → `exec_compact` → `exec_native_compact` 线程进来。
- `context_window`：`chat_runtime_config` 从 provider 配置写入（config.rs:271 `apply_provider_config` 同款语义），factory 建出的 provider `context_window()` 返回正确值，`compact_snapshot` 用它算压缩预算——与旧 `KernelSummaryProvider.context_window` 等价，故 adapter 的 override 冗余可删。
- provider 解析：沿用现有 `resolve_provider_name`（Option 1 前已加的纯函数）解析 `provider_name`。

## 4. 行为 parity 契约

- `/compact` 的**压缩结果不变**：`compact_snapshot(messages, provider, focus)` 输入的 messages/focus 不变，provider 换成 kernel-native 但语义等价（同 model、同 context_window、同 gateway 认证）。
- OAuth/gateway 认证：旧路径 `create_provider` 内做 `load_auth_token`；新路径 factory 内 `AtomGitProviderAuthenticator` 做——两者都解析 AtomGit 网关签名，等价。
- `reason_effort_applicable`：capabilities 版与 core 版逐字相同（Option 2 已验证 parity）。

## 5. 测试

- **单测**：`exec_native_compact` 用一个可解析的 provider 配置构造出 provider（provider 解析 + factory build 不 panic）。provider 构造涉及网络/认证的部分用可测的 seam 或只测解析/config 组装（避免真网络）。
- **构建/编译**：`cargo build --workspace` + `cargo test --workspace --no-run`（编译所有测试目标——本项目教训：勿只 `cargo build`）。daemon 测试套件绿。
- **真机**：webui 或 daemon `/command` 触发 `/compact` 一次，确认压缩生效、token 合理、无 panic（尤其非默认 provider）。

## 6. 风险 / 回滚

- 风险低：`compact_snapshot` 已是 kernel-native，factory 是 `/chat` 已用的成熟构造链；本子项目只是让 /compact 复用它并删 adapter。
- 签名线程（working_dir/telemetry 穿 run_command→exec_compact→exec_native_compact）是唯一"面"，编译器驱动、可增量绿。
- 独立 commit，坏了单独回滚，不影响其它。

## 7. 非目标（YAGNI）

- 不迁 vision、不删 `/chat` preflight（B）。
- 不删 core::provider/ctx/conversation（C）。
- 不改 `compact_snapshot` 本身。
- 不引入新的 provider 抽象——复用现有 `CodingProviderFactory` + `chat_runtime_config`。
