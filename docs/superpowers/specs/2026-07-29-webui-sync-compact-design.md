# webui sync 模式支持 /compact 设计

## 问题

webui 在 **sync 模式**下执行 `/compact`，会显示「同步模式下暂不支持该命令」并直接拒绝
（`webui/src/components/Chat.tsx:1431`，`SESSION_MUTATING = {undo, compact}` 在 `sync` 时 bail）。

拒绝的原因是正确的：sync 模式下**内存中的实时对话**（由绑定的 live 运行时持有——TUI 附着或
headless）是权威来源。原有的非 sync `/compact` 走 `POST /command` → `exec_native_compact`，
它从**磁盘**加载会话快照压缩后回写；若在 sync 模式下这么做，会与内存中的实时对话分叉，且下一
回合会把完整对话重新落盘，静默撤销这次压缩。

## 目标

让 webui 在 sync 模式下的 `/compact` 针对**共享的实时运行时**执行压缩，使 webui 与（若附着的）
TUI 视图保持一致。

## 关键发现：机制已存在

- `DriverCommand::Compact(Option<String>)`（`atomcode-coding/src/runtime.rs:395`）是 TUI 的
  `/compact` 所派发的原生压缩命令；`Option<String>` 是可选的 focus，手动压缩传 `None`。
- live hub 已有派发通道：`native_live::dispatch(command: DriverCommand)`
  （`crates/atomcode-daemon/src/native_live.rs:192`）→ `hub().dispatch(command)`，TUI 附着与
  headless 两种情况都可用（headless 由 `ensure_headless_runtime` 绑定运行时）。
- sync 实时流**已经会渲染压缩结果**：`NativeLiveWireProjector`
  （`crates/atomcode-daemon/src/live_api.rs:889`）把 `CompactionFinished{Completed, committed}`
  映射成一条 `Warning` 线路事件，携带 `format_compaction_mark(removed, before, after)`；失败映射为
  `Error`。因此**回合中的自动压缩早已在 webui sync 显示**。

所以唯一缺口是：让 webui 能**主动触发**一次针对共享运行时的手动压缩。不需要新增任何事件管道。

## 设计

### 后端

1. **新增 `POST /live/compact` 端点**（`live_api.rs`，紧挨 `live_cancel`）：
   ```rust
   pub(crate) async fn live_compact(State(_state): State<AppState>) -> impl IntoResponse {
       let accepted = crate::native_live::dispatch(
           atomcode_coding::DriverCommand::Compact(None),
       ).is_ok();
       Json(serde_json::json!({ "accepted": accepted }))
   }
   ```
   - `accepted:false` 表示当前没有绑定的 live 运行时（无可压缩对象）。
   - 与 `/live/cancel`、`/live/mode` 的形状一致。

2. **注册路由**（`crates/atomcode-daemon/src/lib.rs`，`/live/cancel` 一行旁）：
   ```rust
   .route("/live/compact", post(live_api::live_compact))
   ```

压缩在共享对话上执行；`CompactionFinished` 通过既有 projector 回流为 `Warning` 到 webui（及 TUI）。

### 前端（webui）

3. **`api.ts`**：新增 `postLiveCompact(): Promise<{ accepted: boolean }>`，POST `/live/compact`。

4. **`Chat.tsx` 的 `execServerCommand`**（约 `1428`）：当 `command === 'compact'` **且** `sync` 为真时，
   不再走 `syncUnsupported` bail，改为：
   - 保留现有 `busy` 守卫：`busyRef.current` 为真时显示 `cmd.session.busy`（回合进行中不做手动压缩）。
   - 显示 `cmd.compact.pending` 提示。
   - 调用 `postLiveCompact()`。
   - `accepted === false` → 显示一条简短提示（新 i18n key，见下），表示无实时运行时/无可压缩内容。
   - **不**再额外推送 `cmd.compact.done`：压缩结果由 SSE 的 `Warning` 事件渲染（其文本即
     `format_compaction_mark`，与非 sync 下 `cmd.compact.done` 内容一致），避免重复提示。

   `undo` 仍保留在 sync-unsupported 分支（超出本次范围）。

5. **i18n**：新增 `accepted:false` 的提示 key（中/英），例如
   `cmd.compact.syncNoRuntime`：`当前没有可压缩的实时会话` / `No live session to compact`。

## 数据流

```
webui /compact (sync)
  └─ POST /live/compact
       └─ native_live::dispatch(DriverCommand::Compact(None))
            └─ 共享 live 运行时执行压缩
                 └─ CompactionFinished 事件
                      ├─ NativeLiveWireProjector → LiveWireEvent::Warning → webui SSE 渲染标记
                      └─ (若 TUI 附着) TUI 同步渲染
```

## 错误处理

- 无绑定运行时：`dispatch` 返回 `Err` → `accepted:false` → webui 显示 `cmd.compact.syncNoRuntime`。
- 压缩失败：既有 `CompactionFinished{Failed}` → `LiveWireEvent::Error` → webui 显示 `compact failed: …`。
- 无需压缩（noop）：`Completed` 但 `!committed` → 当前 projector 不发 `Warning`（无回流）。webui 已显示
  `cmd.compact.pending`，此时无「done」回流即可视为无操作；可接受（与 sync 自动压缩一致）。

## 范围外（明确不做）

- **`undo` 的 sync 支持**：截图仅涉及 `/compact`，`undo` 维持拒绝。
- **hub 存储快照压缩后陈旧**：`native_live.rs:449` 的 headless 转发仅在 `SessionChanged` 时提交快照；
  压缩不发 `SessionChanged`，故 hub 内存快照可能陈旧，导致刷新/重连的 `/live` snapshot 显示压缩前消息。
  这是**既有**自动压缩行为，非本次引入，本次不修。
- **webui 视图消息列表就地替换为压缩后消息**：webui 是 append-only 视图，保留旧消息 + 一条压缩标记
  Warning，与 sync 自动压缩、与 TUI scrollback 行为一致。

## 测试

- **后端单测**（`live_api.rs` tests 或 `native_live.rs`）：未绑定运行时时 `live_compact` 返回
  `accepted:false`；绑定后派发 `DriverCommand::Compact`。
- **前端**：sync 模式真机手动验证（与本项目惯例一致，webui 无自动化真机测试）：
  1. sync 开启，多轮对话后 `/compact` → 不再显示「暂不支持」，出现压缩标记 Warning。
  2. 回合进行中 `/compact` → 显示 busy 提示。
  3. 空/新会话 `/compact` → 无运行时时显示 `cmd.compact.syncNoRuntime`。
