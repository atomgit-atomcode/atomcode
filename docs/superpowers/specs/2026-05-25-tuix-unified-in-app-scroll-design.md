# TUI 统一 in-app 滚动设计

**Date**: 2026-05-25
**Status**: Design

## 背景

用户反馈 retained 模式下"使用一段时间后偶尔无法滚动查看输出上下文，焦点像被锁在输入框，只能滚到输入历史"。排查发现根因是 retained 把 body 滚动外包给宿主终端 scrollback，而宿主终端的 DECSTBM 区是否把顶行复制到 scrollback、是否被 resize / control 序列搅乱，跨终端、跨会话表现不一致，atomcode 这边查不到也修不动。

参考 opencode 的做法（基于 `@opentui/core` 的 `<scrollbox>` 组件 + `stickyScroll` + 显式滚动键 + 可视滚动条），把 in-app 滚动做成两个 renderer 的一等公民，不再依赖宿主终端 scrollback 行为。

## 目标

- retained 和 alt-screen 两个 renderer 滚动行为**完全一致**
- 滚动用 atomcode 自己的 `body_lines` 缓冲，不依赖宿主终端 scrollback
- 输入框始终可见可编辑，翻看期间不被打断
- streaming 静默累积，不抢视口（sticky-bottom 语义）
- 加可视滚动条（默认关，`/scrollbar` 切换）
- 加跳消息 / 跳自己消息的键位

## 非目标

- 不抽共享 `ScrollViewport` 模块，retained 和 alt-screen 各自实现 viewport 状态（约 150 行重复）
- 不动 retained 的 DECSTBM 主路径（只在 view_mode 时绕开）
- 不加滚轮加速度（保持当前 3 行/tick）
- 不实现"用户滚走时浮窗提示 N 行未看"

## 用户决策回放（brainstorm 期间）

| 编号 | 问题 | 决策 |
|---|---|---|
| Q1 | 翻看时输入框处理 | 保持可见且可编辑 |
| Q2 | streaming 期间行为 | 静默累积，不抢视口（sticky_bottom） |
| Q3 | retained body 缓冲深度 | 对齐 alt-screen，5000 行 |
| Q4 | retained 是否接管鼠标滚轮 | 是（彻底消除终端依赖） |
| Q5 | 可视滚动条默认显示 | 默认隐藏，`/scrollbar` 切换 |
| Q6 | 退出翻看触发 | 手动滚到底 / End，不动 Esc |
| Q7 | 新增滚动键 | Alt+↑/↓（消息边界）+ Ctrl+↑/↓（用户消息） |
| Q8 | 滚轮步长 | 固定 3 行 |
| Q9 | 滚动条切换入口 | `/scrollbar` slash command |
| Q10 | retained 鼠标拖选 | A+D：复用 alt-screen 实现 + Shift+drag 自动 bypass |

## 架构

### 两个 renderer 收敛后的差异

| | retained | alt-screen |
|---|---|---|
| 切 alt buffer | 否 | 是（`\x1b[?1049h`） |
| 接管鼠标 | **是**（新增） | 是 |
| body emit 路径（sticky 跟底） | DECSTBM `\n` 流式 | buffer paint（CUP+EL） |
| body emit 路径（view_mode） | buffer paint（CUP+EL） | buffer paint（CUP+EL） |
| body 缓冲深度 | 5000 行（**新增，原 height\*4**） | 5000 行（不变） |
| viewport 状态 | view_mode + viewport_top + sticky_bottom（**新增**） | viewport_top + sticky_bottom（已有） |

### Retained 状态机

```
sticky 跟底 (view_mode=false, sticky_bottom=true)
    │
    │ scroll_body(delta) 后 viewport_top < max_top
    ▼
翻看中 (view_mode=true, sticky_bottom=false)
    │
    │ scroll_body 后 viewport_top >= max_top，或 scroll_body_to_bottom() / End
    ▼
重回 sticky 跟底
```

### Retained 新增字段

```rust
view_mode: bool,            // false = sticky 跟底（DECSTBM 流式）；true = 翻看（buffer 重绘）
viewport_top: usize,        // view_mode 时使用，body_lines 索引
sticky_bottom: bool,        // 与 alt-screen 语义一致
max_scrollback_rows: usize, // 5000；body_lines push 时 drain front
message_marks: Vec<MessageMark>, // 与 body_lines 并行的消息边界标记
show_scrollbar: bool,       // 是否画右侧滚动条
```

### `scroll_body(delta)` 实现（两个 renderer 一致）

1. 算 `body_height = height - footer_rows()`，`max_top = body_lines.len() - body_height`
2. 当前 `current_top = if sticky_bottom { max_top } else { viewport_top }`
3. `new_top = clamp(current_top + delta, 0, max_top)`
4. `sticky_bottom = new_top >= max_top`
5. `view_mode = !sticky_bottom`（仅 retained 需要这步）
6. `body_dirty = true; footer_dirty = true; paint_frame()`

### Retained `paint_body` 分叉

- `view_mode = false`：维持现有 DECSTBM 流式路径。若刚从 view_mode 退回（边界翻转），先 CUP+EL+content 重画 body 区从 `body_lines` 尾部一屏，**不发 `\n`**（避免内容重入 scrollback 形成重复）
- `view_mode = true`：跳过 DECSTBM，按绝对位置 CUP+EL+content 重画 `body_lines[viewport_top .. viewport_top + body_height]`，每行带 EL 防止旧字符残留

### Retained `emit_body_line_inner` 分叉

- `view_mode = false`：现有 DECSTBM `\n` scroll 路径**不动**
- `view_mode = true`：**只 push 到 `body_lines`，跳过终端 write**。新内容静默累积；下次 paint 才显示，且因 `sticky_bottom = false`，viewport 不动

### 鼠标接管（retained）

复用 alt-screen 已有的 mode toggle 序列：

| 时机 | 序列 | Windows conhost |
|---|---|---|
| `with_writer` 启动 | `\x1b[3J\x1b[?1002h\x1b[?1006h` | `enable_conhost_mouse_capture()` |
| `suspend_for_external` | `\x1b[?1006l\x1b[?1002l` | `restore_conhost_console_in_mode()` |
| `resume_from_external` | `\x1b[?1002h\x1b[?1006h` | `enable_conhost_mouse_capture()` |
| `shutdown` / `Drop` | `\x1b[?1006l\x1b[?1002l` | `restore_conhost_console_in_mode()` |

### Selection 共享模块

把 alt-screen 当前的 selection / OSC 52 复制实现（`alt_screen.rs:2353-2440` 的 `begin_selection`/`update_selection`/`end_selection`/`copy_selection`，加 `alt_screen.rs:3884-4186` 的 `selection_col_range_for_line`/`render_line_with_selection`/`extract_line_selection_text` 等辅助函数）抽到新模块 `render/selection.rs`：

```rust
// render/selection.rs
pub struct SelectionState {
    pub selection: Option<Selection>,
    pub selection_active: bool,
}

pub struct Selection {
    pub anchor: (usize, u16),
    pub head: (usize, u16),
}

impl SelectionState {
    pub fn begin(&mut self, col: u16, row: u16, body_lines: &[String]) { ... }
    pub fn update(&mut self, col: u16, row: u16, body_lines: &[String]) { ... }
    pub fn end(&mut self, body_lines: &[String]) -> Option<String> { ... }  // 返回选中文本供 OSC 52 写入
    pub fn copy(&mut self, body_lines: &[String]) -> bool { ... }            // 走 arboard
}

// 辅助函数（移动）
pub fn selection_col_range_for_line(...) -> Option<(u16, u16)>;
pub fn render_line_with_selection(...) -> String;
pub fn extract_line_selection_text(...) -> String;
pub fn line_display_width_sgr_aware(...) -> u16;
```

两个 renderer 各自持有 `SelectionState`，paint_body 时调用 `render_line_with_selection`。

**两个 renderer 的 body_lines 类型不同**（alt-screen 是 `Vec<String>`，retained 是 `Vec<Vec<Cell>>`），共享模块用 trait 抽象：

```rust
// render/selection.rs
pub trait BodyLineView {
    fn len(&self) -> usize;
    fn line_text(&self, idx: usize) -> Cow<'_, str>;  // 返回该行的可见文本
}

// alt-screen 现成：impl BodyLineView for Vec<String>
// retained 新增：impl BodyLineView for Vec<Vec<Cell>>，line_text 按需 serialize Cell row 为 String
```

selection / 复制接口都以 `&dyn BodyLineView` 为参数。Cell → String 转换走现有 `serialize_row` 的简化版（只 emit 可见字符，不带 SGR），首次实现可以每次调用即转换，后续如发现热路径耗时再加缓存。

### 可视滚动条

**布局**：占 body 区域最右 1 列，body 内容 paint 时右边 padding 1 列。

**计算**：

```rust
let total       = body_lines.len();
let visible     = body_height as usize;
if total <= visible || !show_scrollbar { return; /* 不画 */ }

let max_top     = total - visible;
let thumb_h     = (visible * visible / total).max(1);
let track_avail = visible - thumb_h;
let thumb_top   = if max_top == 0 { 0 }
                  else { viewport_top * track_avail / max_top };
```

**字符**：track 用 `│` (U+2502, Role::Muted)，thumb 用 `█` (U+2588, Role::Default)。

**特殊情形**：
- `total <= visible`：内容没溢出，不画 track 不画 thumb，body 用满 1 列宽度
- `show_scrollbar = false`：完全不画

**`/scrollbar` 命令**：toggle `show_scrollbar`，回显 `Scrollbar: ON` / `Scrollbar: OFF`，状态持久化到 `$ATOMCODE_HOME/ui-state.toml`（新文件，单一 `[ui]` 表，初版仅 `show_scrollbar: bool` 一个键）。文件不存在 / 读失败时默认 `false`（隐藏）。

### 键位（两个 renderer 一致）

| 键 | 作用 | 拦截位置 |
|---|---|---|
| PageUp / PageDown | 翻 10 行 | `handle_scroll_key` 现有 |
| Shift+↑ / Shift+↓ | 翻 1 行 | `handle_scroll_key` 现有 |
| Home / End | 跳顶 / 跳底 | `handle_scroll_key` 现有 |
| **Alt+↑ / Alt+↓** | 上/下一条消息 | `handle_scroll_key` 新增 |
| **Ctrl+↑ / Ctrl+↓** | 上/下一条用户消息 | `handle_scroll_key` 新增 |
| 鼠标滚轮 | 3 行/tick | `MouseScroll` event 已有 |

### MessageMark 数据结构

```rust
enum MarkKind {
    User,
    Assistant,
    ToolCall,
    ToolResult,
}

struct MessageMark {
    line_idx: usize,    // body_lines 中的行号
    kind: MarkKind,
}
```

**入口**：在 renderer 处理 `UiLine::User` / `UiLine::Assistant` / `UiLine::ToolCall` / `UiLine::ToolResult` 等的分支处，把当前 `body_lines.len()` 记为新 mark 的 `line_idx`。具体哪些分支需要 mark 由 plan 阶段对照 `UiLine` 枚举枚举一遍确定。

**Drain 同步**：`body_lines.drain(0..n)` / `body_lines.remove(0)` 时同步 `message_marks`：
- 删 `line_idx < n` 的 mark
- 剩下的 `line_idx -= n`

**跳转算法**：
- `scroll_to_prev_message()`：在 `message_marks` 里**线性反向遍历**找 `line_idx < viewport_top` 的最大值（数量不大，线性即可），`scroll_body_to(target.line_idx)`
- `scroll_to_next_message()`：找 `line_idx > viewport_top` 的最小值
- `_user_msg()` 变种：加 `kind == User` 过滤

`scroll_body_to(target)`：直接 `viewport_top = target.min(max_top); sticky_bottom = viewport_top >= max_top; view_mode = !sticky_bottom;`

### `/keys` 文档新增章节

`crates/atomcode-core/src/i18n/zh_cn.rs` 和 `en.rs` 的 `Msg::KeybindingsHelp` 加：

```
  ── 翻看输出 ──
    PageUp / PageDown                上下翻一页（10 行）
    Shift+↑ / Shift+↓                上下翻一行
    Alt+↑ / Alt+↓                    跳到上/下一条消息 ***
    Ctrl+↑ / Ctrl+↓                  跳到上/下一条自己发的消息
    Home / End                       跳到最顶 / 跳回最新
    鼠标滚轮                          上下滚（atomcode 接管）
    Shift+拖鼠标                      用宿主终端选择文本（绕过 atomcode）

  ── 显示 ──
    /scrollbar                       切换右侧滚动条显示

  *** Alt+↑/↓ macOS Apple Terminal 需在
      Settings → Profiles → Keyboard 启用 "Use Option as Meta key"
      才会发送修饰键。其他终端默认即可。
```

## 数据流

### 启动

1. 选 renderer（retained / alt-screen / plain，逻辑不变）
2. retained：`with_writer` 发 `\x1b[3J\x1b[?1002h\x1b[?1006h`，Windows 同步 `enable_conhost_mouse_capture()`
3. 从 `$ATOMCODE_HOME/ui-state.toml` 读 `show_scrollbar` 状态（不存在则 false）
4. 初始化 `view_mode=false, sticky_bottom=true, viewport_top=0`

### Streaming（sticky 跟底状态）

1. `agent` 发 `UiLine::AssistantText(...)` 等到 worker
2. retained：`push_body_row` → `emit_body_line_inner` → DECSTBM `\n` scroll；同步 `body_lines.push` + 必要时 `mark_message`
3. alt-screen：`push_body_row` → `body_lines.push` + `body_dirty=true`，下次 paint 重绘
4. body_lines 超 5000 时 drain front + 同步 message_marks

### 用户滚动（PageUp / Shift+↑ / 滚轮）

1. crossterm 报事件 → `reader.rs` → `InputEvent::Key` / `InputEvent::MouseScroll`
2. `event_loop/mod.rs:handle_input` → `handle_scroll_key` → `renderer.scroll_body(delta)`
3. renderer 算 `new_top`，更新 `view_mode` / `sticky_bottom` / `viewport_top`
4. retained 进入 view_mode：之后 streaming 只 push 到 buffer，paint_body 从 buffer 重绘
5. retained 退出 view_mode：CUP+EL 重画 body 区一屏，**不发 `\n`**

### `/scrollbar` 切换

1. 用户输入 `/scrollbar` 回车
2. `event_loop/commands.rs` 找到 `scrollbar` 分支
3. `renderer.toggle_scrollbar()` flip `show_scrollbar`
4. 写 `$ATOMCODE_HOME/ui-state.toml`（写失败不阻塞，记 trace 日志）
5. `body_dirty = true; paint_frame()`
6. 回显 `Scrollbar: ON` / `OFF` 到 body

## Error handling / 边界条件

| 场景 | 行为 |
|---|---|
| view_mode 中 modal 弹出（如 `/provider`） | modal 走自己的全屏 paint，view_mode 不丢；modal 关后回到 view 状态继续 |
| view_mode 中 approval prompt 出现 | 强制退出 view_mode（设回 sticky_bottom=true），避免审批被忽略 |
| view_mode 中 `/clear` / `reset()` | 强制退出 view_mode，body_lines 清空 |
| view_mode 中 `on_resize` | 强制退出 view_mode，避免 viewport_top 在 reflow 后指向无效位置 |
| body_lines.len() < body_height（早期会话） | `max_top=0`，scroll_body 早返回；view_mode 不进入 |
| 终端高度 <= footer_rows() | body_height=1（现有保护），view_mode 仍可进入但只能看 1 行 |
| 用户按 PageDown 滚到 max_top | 自动 sticky_bottom=true, view_mode=false |
| 用户在 view_mode 中切到 Windows 控制台 alt-tab 走开 | 状态保留；回来后 viewport_top 还在；新 streaming 已累积在 buffer |
| 用户在 view_mode 中收到 EOF | 不影响；EOF 走的是退出路径 |
| 鼠标 mode 接管失败（终端不支持 `?1002h`） | 鼠标滚轮事件不到达 atomcode；键盘滚动仍可用；不报错 |
| selection 时进入 view_mode | selection 状态保留但视觉上无关（选中范围基于 body_lines，view_mode 切换不改变 body_lines） |
| `/scrollbar` toggle 时 body 已有内容 | body 有效宽度变化（width vs width-1），触发 reflow（alt-screen 调 `reflow_body_lines`，retained 重画 body 区一屏） |
| `show_scrollbar=true` 但内容未溢出 | 不画 thumb 也不画 track，body 用满整宽（避免空 track 占空间）；下次溢出时自动出现 |

## 测试

### 单测（每个 renderer 各自一套）

**`render/retained.rs` 新增**：
- `retained_enters_view_mode_on_scroll_up`
- `retained_view_mode_suppresses_terminal_writes`
- `retained_exits_view_on_scroll_to_bottom`
- `retained_view_exit_repaints_without_newline_scroll`
- `retained_mouse_capture_sequence_emitted_at_init`
- `retained_suspend_resume_toggles_mouse_capture`
- `retained_reset_clears_view_mode`
- `retained_resize_clears_view_mode`
- `retained_approval_prompt_forces_view_exit`
- `retained_body_cap_drains_oldest`
- `retained_message_marks_track_drain`
- `retained_selection_via_shared_module`

**`render/alt_screen.rs` 新增**：
- `alt_scroll_to_prev_message_finds_nearest_above`
- `alt_scroll_to_prev_user_msg_filters_kind`
- `alt_scrollbar_thumb_height_when_overflow`
- `alt_scrollbar_hidden_when_no_overflow`
- `alt_scrollbar_toggles_via_show_flag`

**`render/selection.rs`（新模块）单测**：
- 搬 alt_screen.rs:3884-4186 的现有 selection 测试

### 集成 QA checklist（手测）

1. macOS Terminal.app retained + 滚轮上滚 → 进入 view，新内容静默累积，End 跳回
2. iTerm2 alt-screen + Alt+↑/↓ → 在 user/assistant/tool 消息间跳转
3. retained 上 Shift+拖鼠标 → 终端原生选择高亮
4. retained 上普通拖鼠标 → atomcode 反色高亮，松手 OSC 52 写剪贴板
5. `/scrollbar` 切换可视滚动条，重启后状态保留
6. streaming 进行时 PageUp → viewport 不动，spinner 继续转
7. 翻看中 `/clear` → 立即回 sticky 跟底
8. 翻看中 approval 弹出 → 强制回 sticky，approval 在底部正常审批
9. retained 启动 → 确认 `\x1b[?1002h` 已发，crossterm 收到 mouse 事件
10. retained + bash tool 走 suspend_for_external → child 期间鼠标恢复终端控制，resume 后 atomcode 重新接管

### 回归保护

所有现有 retained / alt-screen 测试（约 70 个）必须全绿。

## 实现顺序建议

1. **Phase 1**：抽 `render/selection.rs` 共享模块（保留 alt-screen 现有行为，确保 alt-screen 测试全绿）
2. **Phase 2**：retained 加 `body_lines` cap 扩到 5000，加 `MessageMark` 数据结构 + drain 同步，在 push 入口加 `mark_message` 调用
3. **Phase 3**：retained 加 view_mode 状态 + `scroll_body` / `scroll_body_to_top` / `scroll_body_to_bottom` 实现，`paint_body` 分叉，`emit_body_line_inner` 分叉
4. **Phase 4**：retained 加鼠标接管（with_writer / suspend / resume / shutdown 四处），跑通滚轮
5. **Phase 5**：retained 接 selection 共享模块，跑通拖选
6. **Phase 6**：两个 renderer 加 scrollbar 可视化 + `/scrollbar` 命令
7. **Phase 7**：两个 renderer 加 Alt+↑/↓、Ctrl+↑/↓ 键位 + 跳转算法
8. **Phase 8**：i18n 加 `/keys` 文档章节
9. **Phase 9**：edge cases + 集成 QA

每 phase 完成 + 测试全绿才进下一 phase。
