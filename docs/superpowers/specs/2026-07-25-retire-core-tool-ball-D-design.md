# 退役 core — 子项目 D：删除 `core::{tool, conversation, provider, ctx}` 纠缠球

> 状态：设计已确认（用户 approve），待写实施计划。
> Option 1 之后的收口子项目。A（/compact provider）、B（vision）、C（daemon 传输脱 core::conversation）已落地：conversation/provider/ctx 外部代码消费者已全零，但物理删不掉——被 core 内部 `tool↔ctx` 纠缠 + `core::tool` 的外部消费者阻塞。D = 解开并删掉整个球，`core::conversation` 终于物理删除（最初目标）。

## 1. 背景（调查已核实）

- **`core::tool` 唯一外部代码消费者** = daemon 的 `PermissionDecision` + `parse_permission_decision`（permission_bridge.rs、lib.rs、live_api.rs）。`ToolCall`/`ToolResult` 已在 C3 后归零；config/util.rs + capabilities/pathnorm.rs 的命中是**注释**。
- **存活 core 模块碰球只通过 2 个符号**：`tool::real_home_dir()`（skill.rs:501、graph/indexer.rs:489、plugin/installer.rs:267/271）与 `tool::ToolCall`（仅 `core::stream/mod.rs:1` 内部用；外部对 core::stream 只用 `TokenUsage`）。**没有任何存活模块引用 `conversation`/`provider`/`ctx`。**
- **capabilities 已有 `PermissionDecision`**（`tools/approval.rs:77`，变体 `AllowOnce/AllowAlways/Deny`），语义等于 core 的 `Allow/AllowAlways/Deny`；daemon 实测只用 `Allow(10)/AllowAlways(3)/Deny(7)`，**从不用 `Ask`**。capabilities 的 `from_value(&Value)` wire 与 daemon 的 `/chat|/live/permission` wire（`allow`/`always_allow`）不同，故需新增一个按 daemon wire 解析的 `parse_permission_decision(&str)`。
- core **不依赖 kernel（仅 dev-dep）**，故 `ToolCall` 只能移到 core 内部模块，不能用 `kernel::tool::ToolCall`。

## 2. 目标状态（三刀）

**D1 — 把 2 个可分离符号移进存活 core 模块**
- `real_home_dir`（tool/mod.rs:360，纯函数）→ `core::process_utils`（存活 util 模块）。更新 3 个 core 内部调用点（skill/graph/plugin）。
- `ToolCall { id, name, arguments }`（tool/mod.rs:824，3 字段 struct）→ `core::stream`（唯一存活使用者）。更新 `stream/mod.rs` 的 `use crate::tool::ToolCall`。
- 完成后：存活 core 模块不再引用 `crate::tool`。

**D2 — PermissionDecision 归 capabilities（daemon 消费者脱 core::tool）**
- `capabilities::tools::approval` 加 `pub fn parse_permission_decision(s: &str) -> PermissionDecision`（`"allow"→AllowOnce`、`"always_allow"→AllowAlways`、`_→Deny`——逐字对齐 core 版 wire 语义，仅变体名 Allow→AllowOnce）。从 `capabilities::tools` re-export。
- daemon 三文件：`atomcode_core::tool::{PermissionDecision, parse_permission_decision}` → `atomcode_capabilities::tools::{...}`；把 `PermissionDecision::Allow` 全部改 `::AllowOnce`（~10 处）。daemon 不用 `Ask`，无缺口。
- 完成后：`core::tool` 外部代码消费者归零（仅余注释）。

**D3 — 删掉整个球**
- 删 `core::{tool, conversation, provider, ctx}` 模块目录 + `lib.rs` 声明（第 18/19/29/35 行）+ `ctx::file_store` + 任何 orphan 测试文件（`cargo test --workspace --no-run` 抓）。
- 验证存活 core（process_utils/graph/lsp/plugin/proxy/semantic/skill/skill_render/stream/trace/turn/fs_atomic）编译绿；workspace 绿。
- daemon `Cargo.toml` 的 `atomcode-core` 依赖**保留**（仍用 `core::stream::TokenUsage` 等），不动。

## 3. 关键决策

- **PermissionDecision 归 capabilities**（审批域权威，cc_hooks/write_approval 已用），不新建 kernel 类型、不建 daemon 本地副本——DRY + 与 L1 审批栈统一。
- **real_home_dir 归 core::process_utils**（不是 capabilities）：调用者是**存活的 core 模块**，core 不能依赖 capabilities（L1），故必须留在 core 内。capabilities 已有独立 port（pathutil.rs）不受影响。
- **ToolCall 归 core::stream**：唯一存活使用者在 stream 内部；就近落地，不新建 core util 模块。
- **顺序 D1→D2→D3**：D1/D2 解开所有对球的外部/存活引用（可各自独立编译绿），D3 才是纯删除。

## 4. 行为 parity 契约

- `real_home_dir` / `ToolCall` 定义逐字搬迁，语义不变。
- daemon permission 流：`parse_permission_decision` 的 wire 映射不变（`allow`/`always_allow`/其它→Deny）；变体 `Allow`→`AllowOnce` 是纯改名，语义（放行一次）相同。capabilities `AllowOnce/AllowAlways/Deny` 覆盖 daemon 全部用法。
- 删除的球本体无外部行为（conversation/provider/ctx/tool 的执行机器早已不被任何存活代码调用）。

## 5. 测试

- **D1**：`real_home_dir`/`ToolCall` 搬迁后 core 单测绿（搬迁若带原测试一并搬）。
- **D2**：新 `parse_permission_decision` 单测（三 wire→三变体）；daemon permission 相关测试绿。
- **每刀**：`cargo build --workspace` + `cargo test --workspace --no-run`（编译所有测试目标——本项目教训）；daemon 套件绿（webui embedded-asset 两测试是既有环境性失败，无关）。
- **真机**：webui 审批流（Build 模式弹审批→Allow/Always/Deny 各一次）确认 permission 端点仍工作——D2 唯一行为面。

## 6. 风险 / 回滚

- **中低风险**：D1/D2 是符号搬迁 + 改名，编译器驱动；D3 是删除，`cargo test --workspace --no-run` 兜 orphan 测试。
- 每刀独立 commit，坏了单独回滚。D3 前 D1/D2 已绿，删除若炸（漏了某处引用）编译器立刻指出。
- ⚠️ **orphan 测试**：删 conversation/tool 可能留下 `crates/atomcode-core/tests/*.rs` 引用被删模块（如 `set_messages_resume_test.rs` 用 core::ctx）——D3 必须一并删/改，靠 `--no-run` 抓。

## 7. 非目标（YAGNI）

- 不动存活 core 模块的功能（stream/proxy/plugin/skill/graph/…）——只搬 2 个符号进来。
- 不删 daemon 的 `atomcode-core` 依赖（仍用 core::stream 等）。
- 不继续退役其它 core 模块（stream/proxy/plugin/skill…→L1）——那是后续 E/F/G，core 删 D 后仍存活为中等大小 crate。
- 不改 capabilities 已有的 PermissionDecision 变体/from_value（只加 wire 解析器）。
