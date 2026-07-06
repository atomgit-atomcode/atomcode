# 双击 Esc 撤销加冷却（防误撤连撤多轮）

**日期：** 2026-07-06
**分支：** release/v4.26.0

## 动机

用户反馈：输入框为空时快速连按 Esc 会**连撤多轮**（每两下 Esc 撤一轮），且 atomcode 没有 redo，撤掉的内容找不回来。诉求 = 让"手快 mash Esc"不再意外撤销多轮，**不做完整 redo**（redo 因现有 `file_history::restore` 恢复前不快照当前态、前进态被直接丢弃而不可行，属独立大功能，本次不做）。

## 现状机制

- 输入框为空时按 bare Esc 走 `intercept_empty_bare_esc`（`event_loop/mod.rs:3509`）：
  - 第一次 Esc → 武装（记 `pending = Some(now)`），显示 "Esc again to undo"（`Msg::EscAgainToUndo`）。
  - 2s 窗口（`DOUBLE_ESC_UNDO_WINDOW`）内第二次 Esc → 返回 `TriggerUndo`、清 `pending`，执行 `/undo`（回滚文件编辑 + 截断对话到上一轮）。
- **问题**：`TriggerUndo` 后 `pending` 清空，下一次 Esc **立刻重新武装** → 连续 mash `Esc,Esc,Esc,Esc` = 2 次撤销。已有的两步确认（武装提示）被 mash 直接冲过去。
- 作用域：仅"空闲 + 输入框为空"的 bare Esc。流式中 Esc 取消 turn 是另一条路径，不受影响。

## 设计

给撤销加一个**撤后冷却**：撤一轮后的短时间内，bare Esc 不再武装 undo（静默消费），使连续 mash 最多撤 1 轮。

### 组件

1. **常量** `DOUBLE_ESC_UNDO_COOLDOWN: Duration = Duration::from_millis(1500)`（`event_loop/mod.rs`，紧邻 `DOUBLE_ESC_UNDO_WINDOW`）。选 1500ms：足够长到一次连续 mash（几百 ms 内）撤不了第二轮；足够短到刻意停顿后能再撤。

2. **纯函数 `intercept_empty_bare_esc` 增参** `last_undo_at: Option<Instant>`：
   ```
   fn intercept_empty_bare_esc(
       pending: &mut Option<Instant>,
       last_undo_at: Option<Instant>,
       now: Instant,
   ) -> EmptyEscIntercept
   ```
   - 冷却中（`last_undo_at` 存在且 `now - last_undo_at <= COOLDOWN`）→ 直接返回 `Consumed`，**不武装、不触发**（`pending` 保持不变，也不设新武装）。
   - 否则走原逻辑：`second_esc_triggers_undo(*pending, now)` → 清 `pending` 返回 `TriggerUndo`；否则 `*pending = Some(now)` 返回 `Consumed`。

3. **状态**：在存 `pending`（`double_esc_pending: Option<Instant>` 之类）的同一处加 `last_undo_at: Option<Instant>`，初始 `None`，自然过期、无需额外重置。

4. **调用点**（`event_loop/mod.rs` 处理 `EmptyEscIntercept::TriggerUndo` 的 arm，约 :6147）：真正执行 undo 前后**记录 `last_undo_at = Some(now)`**。

### 数据流

```
空闲 + 输入框空 + bare Esc
  → intercept_empty_bare_esc(&mut pending, last_undo_at, now)
      ├─ 冷却中           → Consumed（静默，什么都不做）
      ├─ 有pending且在2s窗口 → TriggerUndo → 执行 /undo + 记 last_undo_at=now
      └─ 否则             → Consumed + 武装 pending=now + 显示 "Esc again to undo"
```

### UX

- 冷却期内按 Esc：**静默无反应**（不显示武装提示、不撤销）——正是"mash 无效"的预期。
- 冷却过后：武装提示恢复，正常双击可再撤。
- **不加新 UI**（YAGNI）。若之后有"以为坏了"的反馈，再补一行 muted 冷却提示。

### 行为对照

| 场景 | 现在 | 改后 |
|---|---|---|
| mash `Esc×4`（500ms 内） | 撤 2 轮 | **撤 1 轮** ✅ |
| 单次双击 Esc | 撤 1 轮 | 撤 1 轮（不变）✅ |
| 撤 1 轮 → 停 ~1.5s → 再双击 | 撤第 2 轮 | 撤第 2 轮 ✅ |

## 测试（TDD，纯函数为主）

`intercept_empty_bare_esc` / `second_esc_triggers_undo` 的单测：
1. **冷却内不触发**：`last_undo_at = now-500ms`、`pending = now-100ms`（本会在窗口内触发）、再按 → 返回 `Consumed`，`pending` 不变（不武装、不撤）。
2. **冷却内不武装**：`last_undo_at = now-500ms`、`pending = None`、按 Esc → `Consumed` 且 `pending` 仍为 `None`。
3. **冷却过后可再撤**：`last_undo_at = now-2s`（>COOLDOWN）、`pending = now-100ms`、按 → `TriggerUndo`。
4. **无前置撤销首次双击照常**：`last_undo_at = None`、`pending = now-100ms` → `TriggerUndo`；`pending=None` → 武装。
5. 现有 4 个 `second_esc_*` 测试仍绿（签名变了要补 `last_undo_at` 实参）。

## 明确不做（YAGNI）

- 完整 redo（前进态不可恢复，独立大功能）。
- 确认卡 / min-gap 第二 Esc / 冷却提示行。
- 触碰流式中 Esc 取消 turn 的路径。
