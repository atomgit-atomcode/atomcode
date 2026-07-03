# 终端标题状态圆点（Terminal Status Glyph）

**日期：** 2026-07-03
**分支起点：** release/v4.25.9

## 动机

用户希望像 codex/claude 那样有个工作状态标记：**不用切换到 atomcode 的终端窗口，就能一眼看出当前任务是空闲、忙碌、还是需要确认。** 通过在终端标签栏/任务栏显示彩色圆点实现。

## 技术现实（决定设计边界）

- 终端标题栏（OSC 0/1/2）只接受**纯文本**，不能画彩色图形。但 emoji 是自带颜色的 Unicode 字符，塞进标题文本前缀里，绝大多数现代 GUI 终端的标签栏 / 操作系统任务栏会用彩色 emoji 字体（Segoe UI Emoji / Apple Color Emoji / Noto Color Emoji）渲染。
- **彩色正常**：Windows Terminal、iTerm2、macOS Terminal.app、GNOME Terminal、WezTerm、kitty、VS Code 内置终端、Windows 任务栏按钮；SSH 远程也可（标题带外传给本地终端）。
- **可能单色 / 豆腐块 □**：tmux/screen、纯 Linux VT 控制台、DevEco/IntelliJ 的 JediTerm、很老的终端。
- **兜底安全性**：圆点只是标题的**前缀字符**，最坏情况是标题前多一个单色小方块，永远不会搞坏显示（不同于正文里的转义序列会串位）。
- 无法从 TTY 内部检测"本终端 emoji 是否彩色"，因此只能提供 config 开关，不能自动降级。

## 决策记录（brainstorm 结论）

| 议题 | 结论 |
|---|---|
| 形态 | **静态状态圆点**（非逐帧动画）。动机是"一眼看状态"，静态已完全满足；动画有闪烁/刷新代价且只在部分终端好看。 |
| 状态集 | **三态**：🟢 空闲 / 🟡 忙 / 🔴 待确认。 |
| 完成态 | **完成 = 空闲**，不单设 ✅。 |
| 思考 vs 回答 | **不拆**（都归 🟡 忙）。 |
| 默认 | **默认开启，可关**（config 开关）。 |
| 任务栏色（OSC 9;4） | **不做**。 |

## 状态 → 圆点映射

现有 `UiPhase`（`crates/atomcode-tuix/src/state.rs:33`）四态：

| UiPhase | 圆点 | 含义 |
|---|---|---|
| `Idle` | 🟢 | 空闲，等你输入 |
| `Streaming` | 🟡 | 忙着（思考 + 回答合并） |
| `Approval` | 🔴 | 需要你确认（审批） |
| `Suspended` | *(无)* | `/shell`、OAuth 等外部交接期，不动圆点、沿用上一个标题 |

## 组件设计

### 1. 圆点映射（纯函数，`title.rs`）

```rust
fn phase_status_glyph(phase: UiPhase) -> Option<&'static str> {
    match phase {
        UiPhase::Idle      => Some("🟢"),
        UiPhase::Streaming => Some("🟡"),
        UiPhase::Approval  => Some("🔴"),
        UiPhase::Suspended => None,
    }
}
```

`Option` 而非 `&str`：`None` 表示"不加圆点/不改标题"，同时用于表达配置关闭。

### 2. 组装标题（纯函数，`title.rs`）

新增 `session_terminal_title_with_status(name, fallback, glyph: Option<&str>) -> String`：

- 先调用现有的 `session_terminal_title(name, fallback)` 得到名字部分（**截断逻辑一字不改**，`MAX_TITLE_CHARS = 40` 只管名字）。
- 若 `glyph = Some(g)`，返回 `format!("{g} {title}")`；否则原样返回。
- 圆点是 1 个 scalar + 1 空格，不占用名字预算，标题总长最坏 +2。
- 占位名（全新窗口）也照加圆点：空闲新标签显示 `🟢 atomcode v4.25.9`，一眼看出活着且空闲。

**依赖：** `title.rs` 需要引用 `UiPhase`（同 crate，`crate::state::UiPhase`）。

### 3. 触发（改 `sync_terminal_title`，`event_loop/mod.rs:6546`）

现签名 `fn sync_terminal_title(ctx, renderer, last)`。改为额外接收 `phase: UiPhase` 与开关 `status_glyph_enabled: bool`：

- `Suspended` → early-return，不改标题（保留外部交接前的显示）。
- 计算 `glyph = if status_glyph_enabled { phase_status_glyph(phase) } else { None }`。
- 组装带圆点的完整标题；`last` 缓存的是**带圆点的完整字符串**，因此 phase 变化（Idle↔Streaming↔Approval）下一次循环迭代就 re-emit。
- 开关关闭时 `glyph = None`，退化成今天的纯名字标题，**零行为变化**。

调用点 `event_loop/mod.rs:3817` 增加 `state.phase` 与开关实参（开关在启动时从 config 读一次并捕获，仿 `auto_copy_enabled` 现有模式）。

### 4. Config 开关（`atomcode-core/src/config/mod.rs`）

`UiConfig`（`config/mod.rs:253`）新增：

```rust
#[serde(default = "default_terminal_status_glyph")]
pub terminal_status_glyph: bool,   // TOML: ui.terminal_status_glyph
```

```rust
fn default_terminal_status_glyph() -> bool { true }
```

`UiConfig::default()` 里补上 `terminal_status_glyph: default_terminal_status_glyph()`。仿 `auto_copy_code_blocks`（`config/mod.rs:272`）那套。

## 数据流

```
每轮事件循环顶部 (mod.rs:3817)
  → sync_terminal_title(ctx, renderer, last, state.phase, enabled)
      → 若 Suspended: return
      → glyph = enabled ? phase_status_glyph(phase) : None
      → title = session_terminal_title_with_status(session.name, fallback, glyph)
      → 若 title != *last: renderer.set_title(title); *last = title
          → RetainedRenderer::set_title → crossterm::SetTitle
              (unix: OSC 序列 / windows: SetConsoleTitleW)
```

phase 转换本就发生在 turn 生命周期里（Idle→Streaming→Approval→Idle），无需新增事件；`sync` 在循环顶部每轮检查即可捕获。

## 错误处理 / 边界

- `set_title` 已有实现是 fire-and-forget（`let _ = execute!`），失败无害。
- 非 TTY / 管道输出：`Renderer::set_title` 默认 no-op，圆点不会泄漏进管道（现状已保证）。
- 名字含注入序列：现有 `session_terminal_title` 已 `scrub_controls`；圆点是硬编码常量，无注入面。
- 圆点渲染成豆腐块：用户 `ui.terminal_status_glyph = false` 关闭。

## 测试（TDD，纯函数为主）

`title.rs` 单测：

1. `phase_status_glyph`：三态各自映射对，`Suspended → None`。
2. `session_terminal_title_with_status`：
   - `glyph = Some("🟡")` → 标题带 `🟡 ` 前缀。
   - `glyph = None` → 与 `session_terminal_title` 完全一致（开关关 = 今天行为）。
   - 占位名 + 圆点 → `🟢 atomcode v9.9.9`。
   - 超长名字：名字截断预算不被圆点挤坏（名字部分仍 ≤ `MAX_TITLE_CHARS`）。

`event_loop` 单测（用假 renderer 记录 `set_title` 调用）：

3. `Suspended` 时 `sync_terminal_title` 不调用 `set_title`。
4. phase 从 Idle 变 Streaming → 触发一次带 🟡 的 `set_title`。

`config` 单测：

5. `terminal_status_glyph` 缺省键 → 默认 `true`。

## 明确不做（YAGNI）

- 逐帧动画（标题栏闪烁 / 刷屏代价）。
- OSC 9;4 任务栏进度色（WT/ConEmu 专属，收益有限）。
- 思考 / 回答拆成两色（额外可见正文判断，价值低）。
- ✅ 完成态（= 空闲）。
- ASCII 降级前缀（标题栏无法上色，emoji 是唯一彩色手段）。
