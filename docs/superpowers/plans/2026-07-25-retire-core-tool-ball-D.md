# 退役 core 子项目D — 删除 tool/conversation/provider/ctx 纠缠球 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development 或 executing-plans。Steps 用 checkbox。

**Goal:** 解开并删除 `core::{tool, conversation, provider, ctx}` 整个球，`core::conversation` 物理删除（最初目标）。

**Architecture:** 存活 core 模块只通过 `tool::real_home_dir`（skill/graph/plugin）+ `tool::ToolCall`（stream）碰球，daemon 只通过 `tool::PermissionDecision`+`parse_permission_decision` 碰球。D1 把前两者搬进存活 core 模块，D2 把后者归 capabilities，D3 删球。

**Tech Stack:** Rust workspace。crate：`atomcode-core`、`atomcode-capabilities`、`atomcode-daemon`。

## Global Constraints
- 每任务后 `cargo build --workspace` + `cargo test --workspace --no-run`；touched crate 测试绿；daemon 的 2 个 webui embedded-asset 失败是既有环境性，无关。
- 每任务一提交；`docs/superpowers` 用 `git add -f`。
- parity：`real_home_dir`/`ToolCall` 逐字搬；permission wire 映射不变（Allow→AllowOnce 纯改名）。
- worktree `retire-core-conversation`，push release/v5.0.3。

---

### Task 1: D1 — 搬 `real_home_dir` → core::process_utils，`ToolCall` → core::stream

**Files:** Modify `crates/atomcode-core/src/{process_utils.rs, stream/mod.rs, tool/mod.rs, skill.rs, graph/indexer.rs, plugin/installer.rs}`。

- [ ] **Step 1: 搬 `real_home_dir` 到 process_utils**
  - 从 `tool/mod.rs`（~360）剪切 `pub fn real_home_dir() -> Option<PathBuf>` 全体（连同其私有 helper，若有）到 `crates/atomcode-core/src/process_utils.rs`（加必要的 `use std::path::PathBuf;` 等）。若 tool/mod.rs 有 `real_home_dir` 的单测，一并搬。
  - 更新 3 个 core 内部调用点：`skill.rs:501`、`graph/indexer.rs:489`、`plugin/installer.rs:267` 与 `:271`，把 `crate::tool::real_home_dir()` → `crate::process_utils::real_home_dir()`。
- [ ] **Step 2: 搬 `ToolCall` 到 core::stream**
  - 从 `tool/mod.rs`（~824）剪切 `pub struct ToolCall { pub id: String, pub name: String, pub arguments: String }`（连其 derive 属性）到 `crates/atomcode-core/src/stream/mod.rs`。⚠️不要搬 `ToolCallBuffer`（那是 tool 内部，随球删）。
  - `stream/mod.rs:1` 的 `use crate::tool::ToolCall;` 删掉（现在同模块内定义）。
  - 若 tool/mod.rs 内其它地方（将被删的球代码）还用 `ToolCall`，它们随球删，不用管；但若 `provider`/`conversation`/`ctx`（也随球删）用了 `ToolCall`，同样不用管。⚠️只需保证**存活模块**（stream 及其消费者）能编译。
- [ ] **Step 3: 编译 core**
  Run: `cargo build -p atomcode-core 2>&1 | grep -E "error|warning: unused"`
  Expected: 无 error（此时球还在，只是符号搬走了；球内对 `crate::tool::real_home_dir`/`crate::tool::ToolCall` 的引用可能报错——若报，把球内引用也改到新路径，或因球即将删可暂留但必须编译过。稳妥做法：球内引用也一并改到 `crate::process_utils::real_home_dir` / `crate::stream::ToolCall`，D3 再删球）。
- [ ] **Step 4: 全绿 + 提交**
  Run: `cargo build --workspace && cargo test --workspace --no-run && cargo test -p atomcode-core`
  ```bash
  git add crates/atomcode-core/
  git commit -m "refactor(core): real_home_dir→process_utils, ToolCall→stream（解开 tool 对存活模块的钩子·D1）"
  ```

---

### Task 2: D2 — PermissionDecision 归 capabilities，repoint daemon

**Files:** Modify `crates/atomcode-capabilities/src/tools/approval.rs` + `tools/mod.rs`（re-export）；`crates/atomcode-daemon/src/{permission_bridge.rs, lib.rs, live_api.rs}`。

**Interfaces:**
- capabilities 既有 `PermissionDecision { AllowOnce, AllowAlways, Deny }`（approval.rs:77）+ `from_value(&Value)`。
- 新增 `pub fn parse_permission_decision(s: &str) -> PermissionDecision`。

- [ ] **Step 1: 写失败测试（capabilities parse_permission_decision）**
  在 `crates/atomcode-capabilities/src/tools/approval.rs` 的 `#[cfg(test)]`：
  ```rust
  #[test]
  fn parse_permission_decision_maps_daemon_wire() {
      assert_eq!(parse_permission_decision("allow"), PermissionDecision::AllowOnce);
      assert_eq!(parse_permission_decision("always_allow"), PermissionDecision::AllowAlways);
      assert_eq!(parse_permission_decision("deny"), PermissionDecision::Deny);
      assert_eq!(parse_permission_decision("garbage"), PermissionDecision::Deny);
  }
  ```
  （`PermissionDecision` 需 `PartialEq`——若没有，给 enum 加 `#[derive(PartialEq, Eq)]`，或测试用 `matches!`。）
- [ ] **Step 2: 跑测试确认失败** → `cargo test -p atomcode-capabilities parse_permission_decision`（未定义）。
- [ ] **Step 3: 实现**
  approval.rs 加：
  ```rust
  /// Parse the wire string used by the daemon's permission endpoints
  /// (`/chat/permission`, `/live/permission`) into a decision. Mirrors the
  /// retired `core::tool::parse_permission_decision` wire mapping.
  pub fn parse_permission_decision(s: &str) -> PermissionDecision {
      match s {
          "allow" => PermissionDecision::AllowOnce,
          "always_allow" => PermissionDecision::AllowAlways,
          _ => PermissionDecision::Deny,
      }
  }
  ```
  `crates/atomcode-capabilities/src/tools/mod.rs` 的 re-export 加 `parse_permission_decision`（与 `PermissionDecision` 同处）。
- [ ] **Step 4: repoint daemon 三文件**
  - `permission_bridge.rs:7` + `:46`、`live_api.rs:18` + `:1673`、`lib.rs:3607`（`::<atomcode_core::tool::PermissionDecision>()`）+ `:3756`：把 `atomcode_core::tool::{PermissionDecision, parse_permission_decision}` → `atomcode_capabilities::tools::{PermissionDecision, parse_permission_decision}`。
  - 全 daemon 把 `PermissionDecision::Allow` → `PermissionDecision::AllowOnce`（~10 处；`grep -rn "PermissionDecision::Allow\b" crates/atomcode-daemon/src/` 定位，注意 `AllowAlways` 不要误改）。
- [ ] **Step 5: 编译 + 全绿 + 提交**
  Run: `cargo build --workspace && cargo test --workspace --no-run && cargo test -p atomcode-daemon && cargo test -p atomcode-capabilities`
  Expected: daemon 仅 2 个既有 webui 失败。确认 `grep -rn "atomcode_core::tool" crates/atomcode-daemon/` 为空。
  ```bash
  git add crates/atomcode-capabilities/ crates/atomcode-daemon/
  git commit -m "refactor(daemon): PermissionDecision 归 capabilities（+wire parser），脱最后一个 core::tool 消费者·D2"
  ```

---

### Task 3: D3 — 删掉整个球

**Files:** Delete `crates/atomcode-core/src/{tool/, conversation/, provider/, ctx/}`；Modify `crates/atomcode-core/src/lib.rs`；Delete orphan tests。

- [ ] **Step 1: 确认球零外部/存活引用**
  Run:
  ```bash
  grep -rnE "crate::(tool|conversation|provider|ctx)::|use crate::(tool|conversation|provider|ctx)\b" crates/atomcode-core/src/ | grep -viE "/(tool|conversation|provider|ctx)/"
  grep -rnE "atomcode_core::(tool|conversation|provider|ctx)\b" crates/ --include='*.rs' | grep -v "crates/atomcode-core/" | grep -vE "^\s*//|///"
  ```
  Expected: 两条都应为空（D1/D2 后）。非空则回到 D1/D2 补。
- [ ] **Step 2: 删模块**
  ```bash
  git rm -r crates/atomcode-core/src/tool crates/atomcode-core/src/conversation crates/atomcode-core/src/provider crates/atomcode-core/src/ctx
  ```
  `lib.rs` 删 `pub mod tool;`（35）、`pub mod conversation;`（18）、`pub mod ctx;`（19）、`pub mod provider;`（29）。若 lib.rs 有 `pub use conversation::...` / `pub use tool::...` 等 re-export，一并删。
- [ ] **Step 3: 清 orphan 测试 + 编译**
  Run: `cargo build -p atomcode-core 2>&1 | grep -E "error"`；再 `cargo test -p atomcode-core --no-run 2>&1 | grep -E "error"`。
  按报错删/改 `crates/atomcode-core/tests/*.rs` 里引用被删模块的文件（如 `set_messages_resume_test.rs` 用 core::ctx → 删该测试文件）。core `bin/` 若有引用一并处理。
- [ ] **Step 4: 全绿**
  Run: `cargo build --workspace && cargo test --workspace --no-run && cargo test -p atomcode-core && cargo test -p atomcode-daemon`
  Expected: 全绿（daemon 2 webui 既有失败）。
- [ ] **Step 5: 确认 conversation 已物理删除**
  Run: `ls crates/atomcode-core/src/conversation 2>&1`（应 No such file）；`grep -rn "core::conversation" crates/ --include='*.rs' | grep -v "^.*//"`（应空或仅历史注释）。
- [ ] **Step 6: 提交**
  ```bash
  git add -A crates/atomcode-core/
  git commit -m "chore(core): 删除 tool/conversation/provider/ctx 纠缠球（core::conversation 物理删除·D3）"
  ```

---

## Self-Review 记录
- **Spec 覆盖**：spec §2 D1/D2/D3 = Task1/2/3。spec §6 orphan 测试风险落在 Task3 Step3。
- **Placeholder**：无。搬迁/parse/删除均给确切位置与代码；「球内引用是否需改」在 Task1 Step3 明确（稳妥改到新路径）。
- **类型一致**：`real_home_dir()->Option<PathBuf>`、`ToolCall{id,name,arguments}`、`PermissionDecision{AllowOnce,AllowAlways,Deny}`、`parse_permission_decision(&str)->PermissionDecision` 三处一致。
- **风险顺序**：D1（搬符号，存活模块解钩）→ D2（daemon 脱 core::tool）→ D3（纯删除）。每刀独立编译绿可回滚。
