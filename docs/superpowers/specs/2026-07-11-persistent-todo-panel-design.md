# 常驻多行 Todo 面板（Persistent Todo Panel）设计

- 日期：2026-07-11
- 状态：已批准（待写实现计划）
- 范围：`crates/atomcode-tuix`（渲染/状态）、`crates/atomcode-capabilities`（todo 数据类型，只读复用）

## 背景 / 动机

当前 `todowrite` 的 todo 列表以**内联块**渲染进 append-only 的 scrollback：每次 `todowrite` 调用都在转录里**再打印一整张表**。随着计划演进，同一张表被反复打印，夹在正文/工具输出中间，看着乱且没有"当前状态"的单一视图。footer 里虽有一行 `☑ 当前任务 · N/M` 计数，但每个 turn 末就被清掉，turn 之间看不到。

对照调研结论（codex `codex-rs/tui/src/history_cell/plans.rs`）：**codex 并不更强**。codex 的 `PlanUpdateCell` 同样是 append-only history cell，每次 `update_plan` 也往 `transcript_cells` 追加一整块（`history_ui.rs:17`），照样多次打印；它唯一的"常驻"物是可选状态行 `Tasks X/Y`（`status_line_setup.rs:143`），等价于 atomcode 早有的 footer 计数行。

因此方向不是"抄 codex"，而是**超过 codex**：把 footer 那一行计数扩成一个**常驻、原地更新、不刷屏**的多行 todo 面板。

## 目标

- footer 上方钉一个多行 todo 面板，**原地更新**整张表（retained diff，不追加进 scrollback）。
- scrollback 里**不再反复内联打印** todo 块。
- 面板**跨 turn 常驻**；**全部完成 / 清空 / 新会话**时消失。
- 面板**跨 `/resume` 回填**（复用已有的 `derive_current_todos`）。
- 长清单**折叠封顶**，绝不吃穿输入框/正文高度。
- 主题安全、非 unicode 终端有 ASCII 回退。

## 非目标

- 不做交互式展开/折叠命令、不做鼠标点击、不做侧栏。
- 不改 `todowrite` 工具协议、不改内核。
- 不落盘持久化 `active_todos`（内存态；resume 靠转录派生，零额外存储）。
- 不引入新配色 Role；沿用现有主题安全样式（`Brand`/`Muted`/默认 + `bold`/`faint`）。

## 已定决策（本轮 brainstorm 拍板）

1. **形态**：常驻多行面板 + 不再内联刷屏（scrollback 无重复 todo 块）。
2. **生命周期**：跨 turn 常驻；`total==0` / 被清空 / 全部 completed / 新会话 → 隐藏。
3. **Resume**：跨 resume 回填（`active_todos = derive_current_todos(messages)`）。
4. **折叠**：已完成折成一行计数；进行中必显；待办按序填满剩余额度，溢出 `+K 更多…`；面板封顶约 6 行（含表头）。
5. **外观**：无框、表头 marker 行（与现有 goal/loop/status 行一致）。
6. **开关**：沿用现有 `ATOMCODE_TODO` env gate。

## 架构

### 数据模型

- 复用 capabilities 既有类型：`atomcode_capabilities::tools::todo::{TodoItem, TodoStatus, todo_glyph, todo_counts, parse_todos, derive_current_todos}`（`crates/atomcode-capabilities/src/tools/todo.rs`）。**capabilities 不改**。
- 扩展 tuix 侧的进度类型 `crate::render::TodoProgress`（`render/mod.rs:517`）以承载全量条目：
  - 现有：`current: Option<String>`, `completed: usize`, `total: usize`
  - 新增：`items: Vec<(TodoStatus, String)>`（状态 + 内容，按模型给出的原始顺序）
  - `completed`/`total` 仍可由 `items` 推出（`todo_counts`），保留字段以最小化调用点改动。
- 新增内存态 `UiState.active_todos: Option<TodoProgress>`（`state.rs`），作为面板的**缓存单一真源**。不落盘。
  - 说明：现有 `live_turn_todo`（`state.rs:341`）是 live-only、turn 末清空的采集字段。本设计用一个**不在 turn 末清空**的字段承载常驻语义。实现时二选一：
    - (a) 新增 `active_todos`，`live_turn_todo` 保留原语义或废弃；或
    - (b) 直接改 `live_turn_todo` 的清空时机 + 扩字段。
  - 计划阶段定稿，倾向 (a)：语义清晰（`active_todos` = 常驻缓存），避免与"live 采集"耦合。

### 状态生命周期

- **写入（live）**：观察到 `todowrite` 工具调用时，用 `parse_todos(args)` 得到全量 items，整表替换 `active_todos`。采集点复用现有 `todo_progress_from_args`（`event_loop/mod.rs:11966`）的调用位置，改为构造带 `items` 的 `TodoProgress`。
- **跨 turn 常驻**：**不再**在 `on_turn_complete` / `on_turn_cancelled` / `on_error`（`state.rs:724/742/753`）清空 `active_todos`。
- **隐藏条件**（渲染时判定，`active_todos` 可留值但面板不显）：
  - `total == 0`（无 todo）
  - 全部 `Completed`（`completed == total && total > 0`）
- **清空**：新会话 / `/clear` / 会话切换 → `active_todos = None`。
- **Resume 回填**：会话加载 / `replay_session` 完成后，`active_todos = derive_current_todos(&messages)` 派生一次（`session_picker.rs` 现有 replay 路径已在扫 `todowrite`，可就近接入）。零额外存储。

### 渲染（footer，`render/retained.rs`）

现有 footer 垂直堆叠：`top_rule / middle(input) / bot_rule / attachments / menu / goal|loop / todo / status`（`paint_footer` ~1733–2003；镜像高度计算 `current_footer_rows` ~2007）。todo 槽当前固定 `0/1` 行（`todo_rows`，`retained.rs:1816`）。

改动：

1. `todo_rows` 由折叠算法算出的 **N**（封顶 `MAX_TODO_PANEL_ROWS`）替代 `0/1`。
2. `build_todo_row`（`retained.rs:1707`）→ `build_todo_rows(&self, todo, rule_width) -> Vec<Vec<Cell>>`，输出 N 行 cell。
3. `paint_footer` 在 `todo_top` 起循环 `draw_row` 铺 N 行（照 menu 行的循环写法 `retained.rs:1952`），`status` 画在 `todo_top + todo_rows`。
4. `current_footer_rows()` 同步按 N 计入总高度（保持与 `paint_footer` 一致，否则 body/footer 布局错位）。
5. `max_input_rows(h, attachments, menu, status + goal + todo_rows)`（`retained.rs:1824`）已把 `todo_rows` 计入输入框预算 —— **面板天然让位输入框**，长清单不会 overflow，输入框保底高度不被吃穿。

### 折叠算法（纯函数，可单测）

输入：`items: &[(TodoStatus, String)]`、`(completed, total)`、`max_rows`（含表头）、`unicode`。
输出：`Vec<PanelLine>`（表头 + 若干条目行），供 `build_todo_rows` 转 cell。

规则（额度 = `max_rows`）：

1. 表头恒占 1 行：`☑ Todos · N/M`。剩余 body 额度 = `max_rows - 1`。
2. 若 `completed > 0`：占 1 行折叠计数 `✔ K 已完成`。
3. 进行中项（协议保证 ≤1）：**必显**，占 1 行。
4. 待办（Pending）按原始顺序填满剩余额度。
5. 若待办 + 上述行数超额：最后一行退回 `☐ +K 更多…`（K = 被省略的待办数）。

不变式：进行中项永远可见；表头恒显；总行数 ≤ `max_rows`。默认 `MAX_TODO_PANEL_ROWS = 6`（表头 + 5 body）。

### 样式 / 主题

面板走 **Cell + `Role` 主题样式**（不是当前内联块的裸 SGR 字符串），天然主题安全，绕开"硬编码颜色浅色看不清"类历史坑。

- 表头 marker：`Role::Brand`（沿用 `build_marker_row` 的 marker 处理）；`Todos` 默认色；` · N/M` 用 `Role::Muted`。
- 已完成折叠行：`faint`（`Role::Muted`）。
- 进行中项：`bold` + marker 用 `Brand` accent。
- 待办项：默认/`Muted`。
- 完成项若单独显示（未折叠场景）：`faint`；**删除线仅在 `CellStyle` 支持时叠加，否则退回 `faint`**（不硬赌能力）。

### 字形 / ASCII 降级

- 每项状态标记复用 `todo_glyph(status, unicode)`：unicode `[•]/[✓]/[ ]` ↔ ASCII `[~]/[x]/[ ]`（`todo.rs:40`，已有回退）。
- 表头 marker 复用 `todo_marker(unicode)`：`☑` ↔ `+`（`retained.rs:207`）。
- 无框设计不引入新的 box-drawing 字符，降级面更小。

### 内联块移除

- 移除 live 路径对 `todowrite` 的内联块渲染：`event_loop/mod.rs:8830–8854` 区间里 `todo_block_styled_lines` → `UiLine::CommandOutput` 的推送。
- 移除 replay 路径同款渲染：`modals/session_picker.rs:579–590`。
- 保留对 `todowrite` **工具结果行的抑制**（`call_rendered=true` / `todowrite_call_ids` 逻辑）——现在面板是唯一视图，工具结果仍不该内联显示。
- `todo_block_styled_lines` / `todo_block_lines`（`event_loop/mod.rs:11905–11941`）若无其他调用点则一并删除（计划阶段核实引用）。

### 开关

- 沿用现有 `ATOMCODE_TODO` env gate：gate 关闭时不注册工具、不显面板，行为与今天一致。

## 受影响文件（锚点）

| 文件 | 改动 |
|---|---|
| `crates/atomcode-tuix/src/render/mod.rs:517` | `TodoProgress` 加 `items` 字段 |
| `crates/atomcode-tuix/src/state.rs:341,724,742,753` | 新增 `active_todos`；移除 turn 末清空 |
| `crates/atomcode-tuix/src/event_loop/mod.rs:8830-8854,11966` | 采集全量 items；移除内联块推送 |
| `crates/atomcode-tuix/src/event_loop/mod.rs:11905-11941` | 视引用删除 `todo_block_*`（若无其他调用点） |
| `crates/atomcode-tuix/src/render/retained.rs:1707,1816,1878,1964,2007` | `build_todo_rows`、`todo_rows=N`、堆叠循环、高度镜像 |
| `crates/atomcode-tuix/src/modals/session_picker.rs:579-590` | 移除 replay 内联块；resume 回填 `active_todos` |
| `crates/atomcode-capabilities/src/tools/todo.rs` | **只读复用**，不改 |

## 边界情形

- **窗口极窄/极矮**：`max_input_rows` 已保底输入框；面板额度不足时进一步折叠（表头 + 进行中 + `+K`），最差只剩表头。
- **全 Pending（无进行中）**：不显"已完成"行，待办按序填充。
- **全 Completed**：面板隐藏（即使 `active_todos` 有值）。
- **`todowrite` 传空表 `[]`**：`total==0` → 隐藏。
- **非法 `todowrite`**：`parse_todos` 失败时不覆盖 `active_todos`（沿用 `derive_current_todos` 跳过非法末次调用的语义）。
- **goal/loop 行同时存在**：堆叠顺序不变 `menu / goal|loop / todo / status`，面板在 goal/loop 下、status 上。
- **CJK/宽字符条目**：走现有 `crate::width` 宽度计算做截断，与 goal/todo 行一致。

## 测试

- 折叠算法纯函数：各状态数量组合、封顶、`+K 更多` 计数、进行中必显、全完成隐藏、空表隐藏。
- `todo_rows` 高度计算 = `build_todo_rows` 行数（`paint_footer` 与 `current_footer_rows` 一致性）。
- 主题样式：浅/深模式下表头/进行中/完成的 Role 快照。
- ASCII 回退：`unicode=false` 下 marker 与条目字形。
- 生命周期：turn 末不清、全完成隐藏、resume 回填（`derive_current_todos`）。
- 回归：`ATOMCODE_TODO` 关闭时零面板、零内联、行为不变。

## 备选方案（已否决）

- **只美化内联块（codex 风格标题+树形）**：最小改动，但没解决"反复刷屏 / 无常驻视图"的根本痛点。否决。
- **常驻面板 + 内联留历史痕迹**：scrollback 在转折点留一条记录。用户选择完全不内联（更干净），故不采纳；如日后想要历史痕迹可再加。
- **每帧 `derive_current_todos` 派生驱动面板**：省一个缓存字段，但长历史无 todowrite 时每帧全量反扫，渲染开销大。改为缓存 `active_todos`（live 写入 + resume 派生一次）。

## 未决问题

无（本轮全部拍板）。计划阶段需核实：`todo_block_*` 的全部调用点、`active_todos` vs `live_turn_todo` 取 (a)/(b)、`CellStyle` 是否支持删除线。
