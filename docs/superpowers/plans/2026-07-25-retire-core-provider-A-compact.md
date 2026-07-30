# 退役 core::provider 子项目A — /compact provider 迁 kernel-native factory 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 daemon `/compact` 摘要 provider 从 `core::provider::create_provider` + `KernelSummaryProvider` adapter 切到 daemon 既有的 kernel-native 构造链（`chat_runtime_config → coding_config_from_runtime → coding_provider_factory().build`），删掉 adapter；`reason_effort_applicable` 指向 capabilities。

**Architecture:** `/compact` 复用 native `/chat` 已用的 provider 构造链，直接得到 `Arc<dyn kernel::LlmProvider>` 喂给已是 kernel-native 的 `compact_snapshot`，删掉 core↔kernel 适配器 `KernelSummaryProvider`。不碰 vision/preflight（子项目B），不删任何模块（子项目C）。

**Tech Stack:** Rust（edition 2021 workspace）、cargo、tokio。crate：`atomcode-daemon`、`atomcode-coding`、`atomcode-capabilities`。

## Global Constraints

- **每任务后 workspace 绿**：`cargo build --workspace` 且 **`cargo test --workspace --no-run`**（编译含测试目标；勿只 `cargo build`）；touched crate 测试套件绿。
- **每任务一提交**。
- **行为 parity**：`/compact` 压缩结果、context_window、gateway 认证语义不变。
- **provider 构造是网络/认证耦合**：`factory.build` 可能做阻塞认证 I/O（同旧 `create_provider`），须 `spawn_blocking` 包裹（与旧代码一致，防阻塞 worker）。因网络耦合，本子项目的 provider 构造**不做单测**，靠编译 + 真机 `/compact` 验证。
- **不提交非本人改动**；`docs/superpowers` 用 `git add -f`。
- **在 worktree `retire-core-conversation` 内做，push 到 release/v5.0.3**。

---

### Task 1: /compact 迁到 factory provider，删除 KernelSummaryProvider

**Files:**
- Modify: `crates/atomcode-daemon/src/commands.rs`（`exec_native_compact` 重写；删 `KernelSummaryProvider` struct+impl（约 14-74）；`exec_compact` 与 `run_command` "compact" 分支线程 `working_dir`/`telemetry`）

**Interfaces:**
- Consumes（daemon 既有）：
  - `crate::live_api::chat_runtime_config(config: &Config, provider_name: &str, working_dir: &Path, telemetry: Arc<Telemetry>) -> atomcode_coding::CodingRuntimeConfig`
  - `crate::kernel_runtime::coding_config_from_runtime(&CodingRuntimeConfig) -> atomcode_coding::CodingAgentConfig`
  - `crate::runtime_host::coding_provider_factory() -> Arc<dyn atomcode_coding::CodingProviderFactory>`；`.build(&CodingAgentConfig, session_id: Option<&str>) -> Result<Arc<dyn atomcode_kernel::provider::LlmProvider>, _>`
  - `atomcode_coding::runtime::compact_snapshot(messages: Vec<kernel::Message>, provider: Arc<dyn kernel::LlmProvider>, focus: Option<String>) -> SnapshotCompaction`
  - `crate::live_api::resolve_provider_name(&Config, Option<&str>) -> String`
- Produces：无对外新接口（内部重写）。

- [ ] **Step 1: 线程 telemetry 到 run_command 的 compact 分支**

`run_command`（commands.rs:797 附近）签名 `State(_state): State<AppState>` → 把 `_state` 改回 `state`（它现在要用了）。`"compact" =>` 分支（约 828）改为把 `state.telemetry.clone()` 传入 `exec_compact`：
```rust
        "compact" => {
            exec_compact(
                &working_dir,
                req.project_hash.as_deref(),
                req.session_id.as_deref(),
                req.provider.as_deref(),
                &req.arg,
                state.telemetry.clone(),
            )
            .await
        }
```
（`AppState.telemetry` 字段类型为 `Arc<atomcode_telemetry::Telemetry>`——若字段名/类型不同，以 AppState 定义为准。）

- [ ] **Step 2: exec_compact 接收并透传 working_dir+telemetry**

`exec_compact` 加 `telemetry: Arc<atomcode_telemetry::Telemetry>` 参数，把 `working_dir` 与 `telemetry` 透传给 `exec_native_compact`：
```rust
async fn exec_compact(
    working_dir: &std::path::Path,
    project_hash: Option<&str>,
    session_id: Option<&str>,
    provider: Option<&str>,
    arg: &str,
    telemetry: std::sync::Arc<atomcode_telemetry::Telemetry>,
) -> anyhow::Result<CommandResult> {
    let sid = session_id.ok_or_else(|| anyhow::anyhow!("session_id required for compact"))?;
    let native = load_native_command_session(working_dir, project_hash, sid)?
        .ok_or_else(|| anyhow::anyhow!("session {sid:?} not found"))?;
    exec_native_compact(provider, arg, native, working_dir, telemetry).await
}
```

- [ ] **Step 3: 重写 exec_native_compact 用 factory provider**

把 `exec_native_compact`（commands.rs:336-378）整体替换为：
```rust
async fn exec_native_compact(
    provider_name: Option<&str>,
    arg: &str,
    session: NativeCommandSession,
    working_dir: &std::path::Path,
    telemetry: std::sync::Arc<atomcode_telemetry::Telemetry>,
) -> anyhow::Result<CommandResult> {
    let config =
        atomcode_config::config::Config::load(&atomcode_config::config::Config::default_path())?;
    let resolved = crate::live_api::resolve_provider_name(&config, provider_name);

    // Build the summarizing provider via the SAME native chain `/chat` uses
    // (chat_runtime_config → coding_config_from_runtime → coding_provider_factory().build),
    // yielding a kernel-native `LlmProvider` directly — no core provider, no adapter.
    // `build` may do blocking auth I/O (gateway token), so run it off the async runtime.
    let coding_cfg = crate::kernel_runtime::coding_config_from_runtime(
        &crate::live_api::chat_runtime_config(&config, &resolved, working_dir, telemetry),
    );
    let factory = crate::runtime_host::coding_provider_factory();
    let provider = tokio::task::spawn_blocking(move || factory.build(&coding_cfg, None))
        .await
        .map_err(|e| anyhow::anyhow!("provider build task panicked: {e}"))?
        .map_err(|e| anyhow::anyhow!("provider construction failed: {e}"))?;

    let compacted = atomcode_coding::runtime::compact_snapshot(
        session.loaded.snapshot.messages.clone(),
        provider,
        (!arg.trim().is_empty()).then(|| arg.trim().to_string()),
    )
    .await;
    if compacted.outcome.committed {
        commit_native_compaction(session, compacted.messages, compacted.mutation)?;
    }
    Ok(CommandResult::Compact {
        applied: compacted.outcome.committed,
        removed_messages: compacted.outcome.removed_messages,
        before_tokens: compacted.outcome.estimated_tokens_before,
        after_tokens: compacted.outcome.estimated_tokens_after,
    })
}
```
（注：`factory.build` 返回 `Result<Arc<dyn kernel::LlmProvider>, ProviderBuildError>`；`compact_snapshot` 第二参就是 `Arc<dyn kernel::LlmProvider>`，直接可用。`CodingAgentConfig` 是 `Send + 'static`，可移入 `spawn_blocking`。）

- [ ] **Step 4: 删除 KernelSummaryProvider struct + impl**

删除 commands.rs 顶部的 `struct KernelSummaryProvider { inner: Arc<dyn atomcode_core::provider::LlmProvider>, context_window: u32 }` 及其 `impl atomcode_kernel::provider::LlmProvider for KernelSummaryProvider { ... }`（约 14-74 行整块）。连带删除该块内对 `crate::legacy_convert::message_to_core` 的 `use`/调用（若在 commands.rs 顶部有 `use ...message_to_core`）。**不要删 `legacy_convert::message_to_core` 本体**——它仍被 `snapshot_to_core` 使用（属子项目C）。

- [ ] **Step 5: 编译 + 清理孤儿 import**

Run: `cargo build -p atomcode-daemon 2>&1 | grep -E "error|warning: unused"`
Expected: 无 error；按编译器提示删掉 commands.rs 里现在孤儿的 import（`atomcode_core::provider`、`Arc`（若仅 adapter 用）、`message_to_core` 等）。确认 `grep -n "KernelSummaryProvider\|atomcode_core::provider::create_provider" crates/atomcode-daemon/src/commands.rs` 为空。

- [ ] **Step 6: 全绿（含测试目标编译）**

Run: `cargo build --workspace && cargo test --workspace --no-run && cargo test -p atomcode-daemon`
Expected: PASS，零 error。（provider 构造网络耦合，无新单测；已存在的 daemon 测试须绿。）

- [ ] **Step 7: 提交**

```bash
git add crates/atomcode-daemon/src/commands.rs
git commit -m "refactor(daemon): /compact provider 迁 kernel-native factory，删 KernelSummaryProvider adapter"
```

- [ ] **Step 8: 真机验证（验收）**

用一个非 gateway provider（如 openrouter）跑一次 `/compact`（webui 或 daemon `POST /command` kind=compact），确认返回 `applied` 且 before/after tokens 合理、无 panic。（headless 无 /compact 入口；用 webui 或直接 HTTP。）

---

### Task 2: reason_effort_applicable 指向 capabilities

**Files:**
- Modify: `crates/atomcode-daemon/src/lib.rs:2509`（`OpenAiProvider::reason_effort_applicable` → capabilities）

**Interfaces:**
- Consumes：`atomcode_capabilities::provider::reason_effort_applicable(model: &str) -> bool`（Option 2 已放开为 pub + re-export）。

- [ ] **Step 1: 替换调用点**

lib.rs:2509 把
```rust
atomcode_core::provider::openai::OpenAiProvider::reason_effort_applicable(&p.model)
```
改为
```rust
atomcode_capabilities::provider::reason_effort_applicable(&p.model)
```
（两函数逐字相同——Option 2 已验证 parity。）

- [ ] **Step 2: 编译 + 确认无孤儿 import**

Run: `cargo build -p atomcode-daemon 2>&1 | grep -E "error|warning: unused"`
Expected: 无 error；若 `atomcode_core::provider` 在 lib.rs 已无其它使用，删掉其 `use`（lib.rs:86）。

- [ ] **Step 3: 全绿**

Run: `cargo build --workspace && cargo test --workspace --no-run`
Expected: PASS。

- [ ] **Step 4: 提交**

```bash
git add crates/atomcode-daemon/src/lib.rs
git commit -m "refactor(daemon): reason_effort_applicable 指向 capabilities::provider"
```

---

## Self-Review 记录

- **Spec 覆盖**：spec §2 目标状态 = Task 1（factory 链 + 删 adapter + 线程 working_dir/telemetry）+ Task 2（reason_effort 重指向）。spec §3 关键改动点全落在 Task 1 Step 1-4。spec §7 非目标（不碰 vision/preflight、不删模块、不删 message_to_core 本体）在 Task 1 Step 4 明确。
- **占位符**：Task 1 provider 构造无单测——已在 Global Constraints + Task 1 说明是网络耦合的有意取舍，靠编译 + 真机（Step 8）验证，非"handle edge cases"空话；`AppState.telemetry` 类型标注"以定义为准"是 defensive note，实现者按实际字段名用。
- **类型一致**：`chat_runtime_config`(→CodingRuntimeConfig) / `coding_config_from_runtime`(→CodingAgentConfig) / `coding_provider_factory().build`(→Arc<dyn kernel LlmProvider>) / `compact_snapshot`(第二参 Arc<dyn kernel LlmProvider>) 全链已核实一致（live_api.rs:295/kernel_runtime.rs:10/runtime_host.rs:81/runtime.rs:665）。
- **风险顺序**：Task 1（provider 迁移+删 adapter）独立可回滚；Task 2 trivial 独立。
