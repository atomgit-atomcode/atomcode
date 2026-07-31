# 退役 core::conversation — TUI 端口至 kernel Message 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 `atomcode-tuix` 的会话模型/渲染/undo 从 `core::conversation` 类型端口到 `atomcode_kernel::message::Message` + capabilities `PresentationFile`/`SessionMeta`，删掉 `snapshot_to_core`/`snapshot_to_kernel`，最终删除 `crates/atomcode-core/src/conversation/`。

**Architecture:** 按职责自底向上、每切片保持 workspace 绿且可发（brainstorming 方案 C）。五切片：legacy importer 解耦 → 渲染迁移 → TuiSession 模型 → undo → 删除。类型映射以现有 `message_to_kernel`/`message_to_core`（legacy_convert.rs:216/273）为权威参照。

**Tech Stack:** Rust（edition 2021 workspace）、cargo、serde、tokio。相关 crate：`atomcode-tuix`、`atomcode-daemon`、`atomcode-cli`、`atomcode-kernel`、`atomcode-capabilities`、`atomcode-core`。

## Global Constraints

- **每个任务后 workspace 绿**：`cargo build --workspace` 且 **`cargo test --workspace --no-run`**（编译含测试目标——本项目教训：`cargo build` 不编译测试，会漏孤儿测试）。触及 crate 的测试套件须跑过。
- **每任务一提交**（commit per task）。
- **行为 parity 是契约**：渲染/undo/旧会话打开的可观察行为不得变（除非该任务明确声明）。
- **删模块必删其孤儿测试**（`cargo test --workspace --no-run` 核验，勿只 `cargo build`）。
- **cold-summaries 沿用磁盘编码**：带前缀标记的合成 kernel 消息（`LEGACY_COLD_SUMMARY_ORIGIN`/`LEGACY_COLD_SUMMARY_PREFIX`）；不加新磁盘字段。
- **不提交非本人改动**；`docs/superpowers` 被 .gitignore 广泛忽略，spec/plan 用 `git add -f`（仓库既有 20+ 份如此跟踪）。
- **每切片真机验证 TUI**（渲染/续聊/undo/打开旧会话）作为验收前提——测试绿≠真机绿。

---

### Task 1: legacy importer 解耦（自包含冻结 DTO）

把 `legacy_convert.rs` 的 legacy-JSON reader 从 `core::conversation::Message`（别名 `CoreMessage`）解耦为**自包含冻结 DTO**，使删 core 后旧 `<id>.json` 仍能导入。零行为变化。

**Files:**
- Modify: `crates/atomcode-daemon/src/legacy_convert.rs`（`LegacySession`/`LegacyDisplayMessage` 的 `messages`/`message` 字段类型；`to_conversation_snapshot`；`convert_legacy_session*`；`message_to_kernel` 的 legacy 入参）
- Test: `crates/atomcode-daemon/src/legacy_convert.rs`（`#[cfg(test)]`，用真实旧 JSON fixture）

**Interfaces:**
- Produces: 冻结 DTO `LegacyMessage { role: String, content: LegacyContent, #[serde(default)] synthetic: bool, #[serde(default)] internal_origin: Option<String> }`，其中 `LegacyContent` 逐一镜像 core `MessageContent` 的 serde 形态（`Text` / `AssistantWithToolCalls{text,tool_calls,reasoning_content,thinking_blocks}` / `ToolResult` / `ToolResultRef` / `MultiPart{text,images}`，见 `crates/atomcode-core/src/conversation/message.rs:43-78`，含各字段的 `#[serde(default, skip_serializing_if=...)]` 属性，逐字复制）。
- Produces: `fn legacy_message_to_kernel(m: &LegacyMessage) -> atomcode_kernel::message::Message`（把冻结 DTO 直接转 kernel，取代经 core 的 `message_to_kernel`）。
- Consumes（不变）：`atomcode_kernel::message::{Message, SessionSnapshot}`、capabilities `SessionMeta`/`PresentationFile`。

- [ ] **Step 1: 准备真实旧会话 fixture**

从生产旧格式取一份（或构造）`<id>.json` 字符串常量，覆盖：一条 `AssistantWithToolCalls`（带 tool_calls + reasoning_content + thinking_blocks）、一条 `ToolResult`、一条 `MultiPart`（带 image）、`cold_summaries: ["s1","s2"]`、`display_messages`、`turn_stats`、`user_renamed:true`、秒级 `created_at/updated_at`。存为测试内 `const LEGACY_JSON: &str`。

- [ ] **Step 2: 写失败测试（断言当前导入结果，锁定 parity 基线）**

```rust
#[test]
fn legacy_import_is_stable_across_dto_decoupling() {
    let session: LegacySession = serde_json::from_str(LEGACY_JSON).unwrap();
    let out = convert_legacy_session(&session); // 现有签名
    // kernel snapshot: 消息条数 + 首条 assistant 的 tool_calls 名称 + cold-summary 合成消息
    assert_eq!(out.snapshot.messages.len(), /* 期望值 */);
    assert!(out.snapshot.messages.iter().any(|m|
        m.internal_origin.as_deref() == Some(atomcode_core::conversation::LEGACY_COLD_SUMMARY_ORIGIN)));
    // meta: 命名/时间戳（秒→毫秒）
    assert_eq!(out.meta.user_renamed, true);
    assert_eq!(out.meta.created_at, session.created_at as i64 * 1000);
    // presentation: display_messages 条数
    assert_eq!(out.presentation.entries.len(), /* 期望值 */);
}
```
（`convert_legacy_session` 的真实返回结构以 legacy_convert.rs:415-509 为准；断言字段照抄其产出。）

- [ ] **Step 3: 运行测试确认通过（这是基线，先绿）**

Run: `cargo test -p atomcode-daemon legacy_import_is_stable_across_dto_decoupling`
Expected: PASS（当前仍走 core 类型；此测试锁定基线，后续解耦不得改变它）。

- [ ] **Step 4: 定义冻结 DTO，替换 CoreMessage**

在 legacy_convert.rs 顶部（DTO 区）新增 `LegacyMessage`/`LegacyContent`（+ 需要的 `LegacyImage`/`LegacyToolCall`/`LegacyThinkingBlock` 冻结子 DTO，逐字镜像 core message.rs 的 serde 形态）。把 `LegacySession.messages: Vec<CoreMessage>` → `Vec<LegacyMessage>`、`LegacyDisplayMessage.message: CoreMessage` → `LegacyMessage`。删掉 `use ... Message as CoreMessage`。

- [ ] **Step 5: 改 to_conversation_snapshot / convert_legacy_session 走冻结 DTO**

`to_conversation_snapshot` 删除（它产出 core `ConversationSnapshot`，已无 core 消费者——Task 5 前若仍被引用则改为内部辅助）。`convert_legacy_session*` 内原经 `message_to_kernel(&core_msg)` 的路径改为 `legacy_message_to_kernel(&legacy_msg)`（新函数，逻辑照抄 message_to_kernel 的 core→kernel 映射，但入参是冻结 DTO）。

- [ ] **Step 6: 运行基线测试 + daemon 测试套件**

Run: `cargo test -p atomcode-daemon legacy_import_is_stable_across_dto_decoupling && cargo test -p atomcode-daemon`
Expected: PASS（导入产出逐字不变——解耦是纯类型替换）。

- [ ] **Step 7: 确认 legacy_convert 不再引用 core::conversation（除 Task 5 待删的 snapshot_to_core/kernel）**

Run: `grep -n "core::conversation" crates/atomcode-daemon/src/legacy_convert.rs`
Expected: 仅剩 `snapshot_to_core`/`snapshot_to_kernel`/`usage_to_core` 及 `LEGACY_COLD_SUMMARY_*` 常量引用（这些 Task 2-5 处理）；`LegacySession`/importer 已无 core 类型。

- [ ] **Step 8: 提交**

```bash
git add crates/atomcode-daemon/src/legacy_convert.rs
git commit -m "refactor(daemon): legacy importer 解耦为自包含冻结 DTO（脱离 core::conversation）"
```

---

### Task 2: TUI 渲染/格式化迁到 kernel Message

把消费 `core::Message`/`MessageContent` 的渲染器改读 kernel 扁平字段。此时 `TuiSession` 仍持 `Vec<core::Message>`（Task 3 才迁），故渲染调用点临时用 core→kernel（`snapshot_to_kernel`/`message_to_kernel`）转换喂入。

**Files:**
- Create: `crates/atomcode-tuix/src/session_summary.rs`（或就近模块）放共享 helper
- Modify: TUI 渲染层（`event_loop/mod.rs`、`render/*`、scrollback/tool-row/thinking/image/todo 格式化函数——以 `grep -rl "MessageContent" crates/atomcode-tuix/src` 为准）
- Test: 新 helper 的单测 + 一处渲染 parity 快照测试

**Interfaces:**
- Produces: `pub fn cold_summaries_from_messages(messages: &[atomcode_kernel::message::Message]) -> Vec<String>`（从 `internal_origin == LEGACY_COLD_SUMMARY_ORIGIN` 的合成消息剥 `LEGACY_COLD_SUMMARY_PREFIX` 前缀，逻辑照抄 legacy_convert.rs:1621-1630 的 `snapshot_to_core` 内提取段）。
- Consumes: kernel `Message` 扁平字段（`role`/`text`/`tool_calls`/`tool_call_id`/`reasoning`/`thinking_blocks`/`images`，见 kernel message.rs:109-140）。

- [ ] **Step 1: 写 cold_summaries_from_messages 失败测试**

```rust
#[test]
fn cold_summaries_extracted_from_synthetic_messages() {
    use atomcode_kernel::message::{Message, Role};
    let mut m = Message::user(&format!("{}old summary", atomcode_core::conversation::LEGACY_COLD_SUMMARY_PREFIX));
    m.internal_origin = Some(atomcode_core::conversation::LEGACY_COLD_SUMMARY_ORIGIN.to_string());
    let msgs = vec![Message::user("hi"), m];
    assert_eq!(cold_summaries_from_messages(&msgs), vec!["old summary".to_string()]);
}
```
（注：`LEGACY_COLD_SUMMARY_*` 常量在 Task 5 前仍在 core；Task 5 时随 core 删除需把常量搬到 kernel 或 capabilities——见 Task 5 Step 2。）

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p atomcode-tuix cold_summaries_extracted_from_synthetic_messages`
Expected: FAIL（函数不存在）。

- [ ] **Step 3: 实现 helper**

按上面 Interfaces 描述实现（遍历、按 origin 过滤、剥前缀）。

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p atomcode-tuix cold_summaries_extracted_from_synthetic_messages`
Expected: PASS。

- [ ] **Step 5: 逐个渲染器改读 kernel 字段（编译器驱动）**

把渲染/格式化函数签名从 `&core::Message`/`&MessageContent` 改为 `&kernel::Message`。`match MessageContent { Text|AssistantWithToolCalls|ToolResult|ToolResultRef|MultiPart }` 改为读扁平字段：
- `Text` → `msg.text`
- `AssistantWithToolCalls{text,tool_calls,reasoning,thinking}` → `msg.text`+`msg.tool_calls`+`msg.reasoning`+`msg.thinking_blocks`
- `ToolResult`/`ToolResultRef` → `msg.role==Tool` + `msg.tool_call_id` + `msg.text`
- `MultiPart{text,images}` → `msg.text`+`msg.images`
调用点临时用 `snapshot_to_kernel(&tui_session_snapshot)` 或 `message_to_kernel` 喂入。**每改一个文件跑一次 `cargo build -p atomcode-tuix` 保持增量绿。**

- [ ] **Step 6: 渲染 parity 快照测试**

对一个覆盖全变体的会话，断言迁移前后 scrollback 输出字符级一致（用现有渲染测试 harness；若无，构造一个：喂固定会话→收集渲染行→`assert_eq!` 已知期望）。重点覆盖 `ToolResultRef`、`MultiPart` 图片占位、`thinking_blocks`。

- [ ] **Step 7: 全绿 + 提交**

Run: `cargo build --workspace && cargo test --workspace --no-run && cargo test -p atomcode-tuix`
Expected: PASS。
```bash
git add -A crates/atomcode-tuix
git commit -m "refactor(tuix): 渲染层迁到 kernel Message（临时经 snapshot_to_kernel 喂入）"
```

---

### Task 3: TuiSession 模型迁移

把 `TuiSession`/`DisplayMessage` 的消息字段从 core `Message` 换成 kernel `Message`，消除渲染入口的临时转换。

**Files:**
- Modify: `crates/atomcode-tuix/src/session.rs`（`TuiSession.messages`、`DisplayMessage.message`、`from_catalog_view`、`update_from_conversation_snapshot`、`to_conversation_snapshot`）
- Modify: `crates/atomcode-tuix/src/event_loop/{mod.rs,bg_runtime.rs,commands.rs}`（`snapshot_to_core` 调用点）
- Test: `crates/atomcode-tuix/src/session.rs` 既有 undo 测试（改断言类型）+ hydrate parity 测试

**Interfaces:**
- Produces: `TuiSession.messages: Vec<atomcode_kernel::message::Message>`、`DisplayMessage.message: atomcode_kernel::message::Message`。
- Consumes: `CatalogSessionView { snapshot: kernel SessionSnapshot, meta: SessionMeta, presentation: PresentationFile }`（已是 kernel 类型，见 legacy_convert.rs `CatalogSessionView`）。

- [ ] **Step 1: 改 session.rs 字段类型 + from_catalog_view**

`use atomcode_kernel::message::{Message, Role}`（替换 core import）。`TuiSession.messages`/`DisplayMessage.message` → kernel `Message`。`from_catalog_view`（session.rs:121）删掉 `snapshot_to_core(view.snapshot)`，直接 `messages: view.snapshot.messages.clone()`；`display_messages` 从 `view.presentation.entries` 构造（`after_message`=entry.anchor，`message`= 由 presentation entry 合成的 kernel Message，或保留 display 专用轻量结构——见 Step 2）；`cold_summaries` = `cold_summaries_from_messages(&view.snapshot.messages)`（Task 2 helper）。

- [ ] **Step 2: 决定 DisplayMessage.message 来源**

presentation entry 是 `{anchor, role, text}`（纯文本，无 tool_calls/images）。若渲染 display_messages 只需 text/role，则 `DisplayMessage.message` 用 `Message::new(role, text)` 合成即可；若渲染需完整消息，则 display 消息取 `view.snapshot.messages[anchor]`。按 Task 2 渲染器实际读取的字段决定（读了 tool_calls/images 就取 snapshot 消息，否则合成）。在本步用一句注释固化该决定。

- [ ] **Step 3: 消除 event_loop 的 snapshot_to_core 调用（编译器驱动）**

`bg_runtime.rs:786/805/843`、`event_loop/mod.rs` 的 `snapshot_to_core` 调用点：现在 session 已是 kernel，直接用 `view.snapshot`/kernel 消息，删掉转换。`apply_session_snapshot` 直接吃 kernel `SessionSnapshot`。每改一处 `cargo build -p atomcode-tuix`。

- [ ] **Step 4: hydrate parity 测试**

构造一个 `CatalogSessionView`（kernel snapshot + meta + presentation）→ `TuiSession::from_catalog_view` → 断言 `messages`/`cold_summaries`/`turn_stats`/`user_renamed` 与期望一致（尤其 cold_summaries 经 helper 派生正确）。

- [ ] **Step 5: 全绿 + 提交**

Run: `cargo build --workspace && cargo test --workspace --no-run && cargo test -p atomcode-tuix`
Expected: PASS。
```bash
git add -A crates/atomcode-tuix
git commit -m "refactor(tuix): TuiSession 模型迁到 kernel Message，去掉 hydrate 的 snapshot_to_core"
```

---

### Task 4: undo 迁移

把 undo 快照/恢复从 core `ConversationSnapshot` 换成 kernel `SessionSnapshot`。

**Files:**
- Modify: `crates/atomcode-tuix/src/session.rs`（`to_conversation_snapshot`/`update_from_conversation_snapshot`→ kernel；`retain_turn_stats_after_undo` 逻辑不变）
- Modify: undo 的调用点（`grep -rn "to_conversation_snapshot\|update_from_conversation_snapshot" crates/atomcode-tuix/src`）
- Test: session.rs:217 既有 undo 测试改断言类型

**Interfaces:**
- Produces: `TuiSession::to_snapshot(&self) -> atomcode_kernel::message::SessionSnapshot`、`TuiSession::restore_from_snapshot(&mut self, snap: SessionSnapshot)`（重命名以脱离 core 语义；或保留原名改类型）。
- 说明：cold-summaries 已作为合成消息含在 `snapshot.messages` 内，无需单独字段——undo 快照/恢复整份 messages 即自动带上 cold summaries。

- [ ] **Step 1: 改 undo 快照/恢复类型**

`to_conversation_snapshot`（session.rs:166）→ 返回 `SessionSnapshot { messages: self.messages.clone(), ..SessionSnapshot::new(...) }`（或 `SessionSnapshot::new(self.messages.clone())`）。`update_from_conversation_snapshot`（session.rs:173）→ 吃 `SessionSnapshot`，`self.messages = snap.messages; self.cold_summaries = cold_summaries_from_messages(&self.messages);`。

- [ ] **Step 2: 改 undo 调用点（编译器驱动）**

undo 栈现存 core `ConversationSnapshot` 的地方改存 kernel `SessionSnapshot`。`cargo build -p atomcode-tuix` 逐处修绿。

- [ ] **Step 3: 改既有 undo 测试断言（session.rs:217 附近）**

`undo_stat_pruning_preserves_accounting_only_history` 等：把构造/断言从 core 类型换 kernel，验证撤销后 `messages`+`cold_summaries`+`turn_stats` 与旧行为一致。

- [ ] **Step 4: 全绿 + 提交**

Run: `cargo build --workspace && cargo test --workspace --no-run && cargo test -p atomcode-tuix`
Expected: PASS。
```bash
git add -A crates/atomcode-tuix
git commit -m "refactor(tuix): undo 迁到 kernel SessionSnapshot"
```

---

### Task 5: 删除 snapshot_to_core/kernel 与 core::conversation

消费者归零后，删掉转换函数与整个 core::conversation 模块。

**Files:**
- Modify: `crates/atomcode-cli/src/main.rs:1827`（`snapshot_to_kernel` 调用 → 直接持 kernel）
- Modify: `crates/atomcode-daemon/src/lib.rs:3575`、`live_api.rs:291/496`（`/chat` 边界 `snapshot_to_core` → 直接从 kernel 投射响应）
- Modify: `crates/atomcode-daemon/src/legacy_convert.rs`（删 `snapshot_to_core`/`snapshot_to_kernel`/`usage_to_core`）
- Move: `LEGACY_COLD_SUMMARY_ORIGIN`/`LEGACY_COLD_SUMMARY_PREFIX` 常量 core→ 一个存活 crate（kernel `message.rs` 或 capabilities `session`）
- Delete: `crates/atomcode-core/src/conversation/`、`pub mod conversation`、任何 `core/tests/*conversation*` 孤儿测试
- Modify: `crates/atomcode-core/src/lib.rs`

**Interfaces:**
- Consumes: 前序任务已使所有前端持 kernel 类型。

- [ ] **Step 1: 迁 cold-summary 常量到存活 crate**

把 `LEGACY_COLD_SUMMARY_ORIGIN`/`_PREFIX`（core/src/conversation/mod.rs:19-22）移到 `atomcode_kernel::message`（或 capabilities session）。更新 Task 1/2 引用点（legacy_convert、cold_summaries_from_messages 测试）指向新位置。`cargo build -p atomcode-tuix -p atomcode-daemon`。

- [ ] **Step 2: 切 cli/daemon 边界的最后转换（编译器驱动）**

cli `main.rs:1827`：起 runtime 的输入现已是 kernel snapshot，删 `snapshot_to_kernel`。daemon `/chat` 的 `snapshot_to_core`（lib.rs:3575、live_api.rs:291/496）：响应投射直接从 kernel 消息构造。`cargo build --workspace` 修绿。

- [ ] **Step 3: 删转换函数 + 确认零引用**

删 `snapshot_to_core`/`snapshot_to_kernel`/`usage_to_core`（legacy_convert.rs）。
Run: `grep -rn "atomcode_core::conversation\|snapshot_to_core\|snapshot_to_kernel" crates --include='*.rs' | grep -v crates/atomcode-core/`
Expected: 空（零外部引用）。

- [ ] **Step 4: 删 core::conversation + 声明 + 孤儿测试**

```bash
rm -rf crates/atomcode-core/src/conversation
# 删 core/src/lib.rs 的 `pub mod conversation;`
grep -rln "conversation" crates/atomcode-core/tests/ 2>/dev/null   # 找孤儿测试并删
```

- [ ] **Step 5: 全量核验（含测试目标）**

Run: `cargo build --workspace && cargo test --workspace --no-run && cargo test -p atomcode-core -p atomcode-tuix -p atomcode-daemon -p atomcode`
Expected: PASS，零警告。确认 `grep -rn "atomcode_core::conversation" crates` 为空。

- [ ] **Step 6: 提交**

```bash
git add -A
git commit -m "refactor(core): 删除 core::conversation，前端全面基于 kernel Message"
```

- [ ] **Step 7: 真机验证（验收）**

真机跑 TUI：打开一个旧 `<id>.json` 会话（验导入）、续聊一轮（验渲染）、撤销一轮（验 undo）、开一个带图/带 tool_calls 的会话（验 MultiPart/ToolResultRef 渲染）。

---

## Self-Review 记录

- **Spec 覆盖**：spec 五切片 ↔ Task 1-5 一一对应；cold-summary/DisplayMessage/turn_stats 三决策分别落在 Task 2(helper)/Task 3(Step 2)/Task 3-4；legacy importer 解耦=Task 1；测试/fixture=各 Task 的 parity 测试。
- **占位符**：编译器驱动的 call-site 清扫（Task 2 Step 5、Task 3 Step 3、Task 4 Step 2、Task 5 Step 2）是类型迁移的固有形态，已给出触发它的**确切类型改动**+**增量绿门**+**parity 测试**，非"handle edge cases"式空话。真实期望值（`/* 期望值 */`）需实现者对着 fixture 填——已标明来源（fixture + convert_legacy_session 产出）。
- **类型一致**：`cold_summaries_from_messages`、`from_catalog_view`、`to_snapshot`/`restore_from_snapshot` 命名跨 Task 一致；kernel `Message` 扁平字段名以 kernel message.rs 为准。
- **风险顺序**：渲染(Task 2)、undo(Task 4)各自独立可回滚，符合 spec 风险分层。
