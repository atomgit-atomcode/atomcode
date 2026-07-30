# 退役 `core::conversation`：TUI 会话模型端口至 kernel `Message`

> 状态：设计已确认，待写实施计划。
> 目标：删除 `crates/atomcode-core/src/conversation/`，从而拔掉 `atomcode-core` 最大的一根外部锚（前端对 core 的 ~104 处引用）。
> 策略：**按职责自底向上，每切片保持 workspace 绿且可发**（brainstorming 选定的方案 C）。

## 1. 背景与前提修正

此前把这块当成"session 双模型收敛"项目。实测（见调查报告）**前提已大半成立**：

- **core `<id>.json` 的 live 双写早已删除。** 运行时持久化 100% 走原生（`.snapshot`/`.meta`/`.jsonl` + presentation）。core JSON 现在**运行时只读**——仅被 legacy importer 读取（未迁移的旧会话），且仅在 `#[cfg(test)]` 里被写。
- **所有接入层语义已有原生归属**：命名/时间戳/turn_stats → `SessionMeta`；display messages → `PresentationFile`；cold_summaries → 带前缀标记的**合成 kernel 消息**（native 已这么写；kernel `SessionSnapshot` 无专门 cold-summary 字段）。

**真正的阻塞是 TUI。** `atomcode-tuix` 把 `core::conversation::{Message, MessageContent, ConversationSnapshot, DisplayMessage}` 当作**自己的内存工作类型**，贯穿 `session.rs` 与 `event_loop/*`，每次 hydrate/resume/turn 都经 `snapshot_to_core`/`snapshot_to_kernel` 往返。daemon `/chat` 侧只在边界用 core 类型（轻量）。

因此"删 `core::conversation`" = **把 TUI 会话模型/渲染/undo 从 core 类型端口到 kernel `Message` + capabilities `PresentationFile`/`SessionMeta`**，并删掉所有 `snapshot_to_core`/`snapshot_to_kernel` 调用。

## 2. 目标状态

- `atomcode-tuix` 会话模型/渲染/undo 全部基于 `atomcode_kernel::message::Message` + capabilities `PresentationFile`/`SessionMeta`。
- `legacy_convert.rs` 中 `snapshot_to_core` / `snapshot_to_kernel` / `usage_to_core` 全部删除。
- `crates/atomcode-core/src/conversation/` 删除；`crates/atomcode-core/src/lib.rs` 去掉 `pub mod conversation`。
- legacy `<id>.json` 导入仍工作——靠 legacy_convert **自带的冻结 DTO**（不再依赖 core 类型）。
- 全工作区 `atomcode_core::conversation` 引用归零。

## 3. 类型映射（渲染迁移的核心）

core 的内容是枚举，kernel 是扁平字段：

```
core   Message { role, content: MessageContent, synthetic, internal_origin }
       MessageContent = Text
                      | AssistantWithToolCalls { text, tool_calls, reasoning_content, thinking_blocks }
                      | ToolResult
                      | ToolResultRef
                      | MultiPart { text, images }

kernel Message { role, text, tool_calls: Vec<ToolCall>, tool_call_id,
                 reasoning, thinking_blocks, images, meta, synthetic, internal_origin }
```

渲染层现在 `match MessageContent`，改为读 kernel 扁平字段。转换语义以现有 `message_to_kernel` / `message_to_core`（legacy_convert.rs:216/273）为权威参照——端口后这两者删除，映射逻辑内化进渲染与导入器。

**高风险映射点**（切片 2 必须逐一 parity 验证）：`ToolResultRef`、`MultiPart` 的图片、`thinking_blocks`（Anthropic extended-thinking 的 signature 往返 token）。

## 4. 五个切片（每片绿+可发，按此序）

### 切片 1｜legacy importer 解耦（前置，零行为变化）
`legacy_convert.rs` 的 `LegacySession`/`LegacyDisplayMessage`/`CoreMessage`（现用 `core::conversation::Message`）改为在 legacy_convert 内定义**自包含冻结 legacy-format DTO**（`#[derive(Deserialize)]`，字段与旧磁盘 JSON 逐一对应）。`convert_legacy_session*`（legacy_convert.rs:415/421）直接产出 kernel `SessionSnapshot` + `SessionMeta` + `PresentationFile`，不经 core 类型。
- **验证**：真实旧 `<id>.json` fixture → 导入结果逐字段断言（cold_summaries 往返、display_messages、turn_stats、时间戳秒→毫秒）。

### 切片 2｜TUI 渲染/格式化迁到 kernel Message
消费 `core::Message`/`MessageContent` 的渲染器（scrollback、tool 行、thinking、图片、todo 块）改读 kernel 扁平字段。此时 `TuiSession` 仍持 `Vec<core::Message>`（切片 3 才迁），故渲染调用点**临时**用 core→kernel（`message_to_kernel`/`snapshot_to_kernel`）转换喂入，每步保持绿；切片 3 迁完模型后这层临时转换即消除。
- 抽共享 helper `cold_summaries_from_messages(&[kernel Message]) -> Vec<String>`（从带前缀标记的合成消息提取，参照 legacy_convert.rs:1621-1630）。
- **验证**：同一会话迁移前后 scrollback 输出字符级一致。

### 切片 3｜TuiSession 模型迁移
`TuiSession.messages: Vec<Message>` 与 `DisplayMessage.message` 改成 kernel `Message`（session.rs:79-91、8-11）。`from_catalog_view`（session.rs:121）不再 `snapshot_to_core`，直接持 `view.snapshot`（kernel）+ `view.presentation`。cold_summaries 用切片 2 的 helper 派生。
- **验证**：会话 hydrate/resume 后 messages/display/turn_stats 与旧一致。

### 切片 4｜undo 迁移
`to_conversation_snapshot`/`update_from_conversation_snapshot`（session.rs:166-176，现基于 core `ConversationSnapshot`）改用 kernel `SessionSnapshot`（cold-summary 合成消息已含其中，无需单独字段）。`retain_turn_stats_after_undo`（session.rs:178）逻辑不变（按 message_count 裁剪）。
- **验证**：撤销后 messages + cold_summaries + turn_stats 与旧行为一致（复用 session.rs:217 的既有 undo 测试，改断言类型）。

### 切片 5｜删除
- 删 `snapshot_to_core`/`snapshot_to_kernel`/`usage_to_core`。
- cli `main.rs:1827`（`snapshot_to_kernel` 起 runtime）改为直接持 kernel。
- daemon `/chat` 边界 `snapshot_to_core`（lib.rs:3575、live_api.rs:291/496）改为直接从 kernel 投射响应。
- 全工作区 `atomcode_core::conversation` 归零 → 删 `crates/atomcode-core/src/conversation/` + `pub mod conversation` + 相关孤儿测试（教训：删模块必删其孤儿测试，用 `cargo test --workspace --no-run` 核验）。

## 5. 关键设计决策

- **cold-summaries 表示**：不加新字段，继续用"带前缀标记的合成 kernel 消息"编码（native 已这么写）；所有 `Vec<String>` 消费者经共享 helper 提取。
- **DisplayMessage**：保留 TUI 本地类型，但 `message` 字段换成 kernel `Message`（渲染需要完整消息，不只 presentation 的纯文本）。
- **turn_stats**：TUI 的 `TurnStat`（session.rs:16-25）继续本地持有，来源 `SessionMeta.turn_stats`（已有原生字段，含 turn_id/position_valid/round_count）。
- **legacy importer 单向**：切片 1 后 `convert_legacy_session*` 是唯一的 core-JSON→native 单向、幂等、可失败导入器；不恢复任何双向转换。

## 6. 测试 / fixture

- **旧会话导入 fixture**（切片 1）：真实旧 `<id>.json` → 导入后 kernel snapshot/meta/presentation 逐字段断言。
- **渲染 parity**（切片 2）：迁移前后 scrollback 字符级一致。
- **undo parity**（切片 4）：撤销后状态一致。
- 每片 `cargo test --workspace --no-run`（编译含测试目标）+ tuix 测试套件绿。

## 7. 风险 / 回滚

- **最高风险=渲染 parity**（切片 2）：MessageContent 枚举 → 扁平字段的语义映射，尤其 `ToolResultRef`、`MultiPart` 图片、`thinking_blocks`。独立切片，可单独回滚。
- **第二风险=undo**（切片 4）：独立切片。
- 每片独立 commit；坏了只回该片，不影响已落地的前序切片。
- 全程真机验证 TUI（渲染/续聊/undo/旧会话打开）作为切片 3/4/5 的验收前提——测试绿≠真机绿（本项目教训）。

## 8. 非目标（YAGNI）

- 不动运行时持久化（已全原生）。
- 不改 cold-summary 的磁盘编码（沿用合成消息）。
- 不做 daemon 侧的更深 provider/stream 端口（另立项）。
- 不为老格式加迁移写回（importer 保持只读）。
