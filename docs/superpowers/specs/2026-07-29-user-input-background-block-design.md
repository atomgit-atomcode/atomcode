# 用户输入背景色块（区分输入/输出）设计

日期：2026-07-29
状态：已确认，待实现

## 问题

用户反馈：TUI 里用户输入和助手输出没有明确的视觉区分，一屏滚下来分不清哪段是自己问的、哪段是模型答的。参考 codex 的做法——给用户输入加一个整行铺满的浅背景色块。

## 目标

给**用户输入消息**渲染一个整行铺满的背景色块（含上下留白），把它和无背景的助手输出清晰区分开。只动用户输入，不动助手输出，对齐 codex 的取舍。

## 现状（为什么不难）

atomcode-tuix 是自研 retained cell 渲染器（非 ratatui），但背景色所需的基础设施全部现成：

- `render/cell.rs`：`CellStyle` 已有 `bg: Option<Color>` 字段。
- `render/cell.rs`：序列化器 `emit_sgr_transition` 已会发 `\x1b[48;...m` 背景色 SGR，并在行尾 `\x1b[0m` reset。**底层零改动。**
- `render/theme.rs:216`：已存在自适应背景角色 `Role::PanelBg`——深色 `AnsiValue(236)`、浅色 `AnsiValue(254)`，按 `md_theme::is_light_for_render()` 切换。scrollback 里从未用上。

当前 `push_user_message`（`retained.rs:6477`）只给 `❯` 前缀和续行 `▎` 竖条上青色（`Role::Accent`），正文用 `CellStyle::default()`（无背景），上下用空 `Vec::new()` 作分隔。

## 方案

### 颜色：复用 `Role::PanelBg`（自适应，不新造颜色）

| 主题 | 值 | ≈RGB | 说明 |
|------|-----|------|------|
| 深色 | `AnsiValue(236)` | `#303030` | 与 codex（叠 12% 白 ≈`#3a3a3a`）几乎一致 |
| 浅色 | `AnsiValue(254)` | `#e4e4e4` | 比 codex（叠 4% 黑 ≈`#f5f5f5`）略强，正好加强区分 |

用 AnsiValue 而非 truecolor：不依赖探测终端底色，256 色灰阶是标准化 ramp，更稳。若后续觉得浅色偏重，把 254 调 255 即可（真机后微调）。

### 结构：整块 = 上留白行 + 内容行 + 下留白行

对齐 codex 的"有呼吸感的整块"：

```
[整行 PanelBg 背景空行]        ← 上留白
❯ 用户第一行文字 ……(铺满 PanelBg 背景到 w 列)
▎ 续行文字 ……(铺满 PanelBg 背景)
└ [Image #1] ……(附件行，也在块内)
[整行 PanelBg 背景空行]        ← 下留白
```

- `❯` / `▎` / `└` 前缀保持原色（Accent/Muted），叠在 PanelBg 背景之上。
- 正文保持终端默认前景色，只加 `bg`。
- 导航 mark 仍锚在 `❯` 那一行（`mark_message` 位置不变）。

### 铺满宽度：铺到 `w = screen.width() - PAD_COL`

`build_prefixed_rows`（`retained.rs:5818`）已按 `w = screen.width() - PAD_COL` 排版，右边天然留 `PAD_COL` 边距。背景铺到 `w` 列即可：

1. 复用现有 `build_prefixed_rows` 得到内容行；
2. 给每行的**已有 cell**（前缀+正文）的 `style.bg` 设为 PanelBg；
3. 用带 PanelBg 的空格 cell 把每行补齐到 `w` 列；
4. 上下留白行 = 一整行 `w` 个带 PanelBg 的空格 cell（替换原来的空 `Vec::new()`）。

**铺到 `w`（而非终端满宽）顺带规避"最后一列写字符触发自动换行/滚动"的坑**——右边留出的 `PAD_COL` 就是安全边距。

## 落点（改动集中在一个文件）

`crates/atomcode-tuix/src/render/retained.rs`：

1. 新增样式辅助 `style_panel_bg()` → 返回 `CellStyle { bg: role(caps, Role::PanelBg), ..default }`（前景 None，走终端默认）。
2. 新增行辅助 `pad_row_to_bg(row, w, bg)`：给已有 cell 套 bg + 补齐空格到 `w`；以及 `bg_blank_row(w, bg)` 生成整行背景空行。
3. 改 `push_user_message`：
   - 上分隔：原逻辑若需要空行，改 push `bg_blank_row`；若复用已有 tail blank，需把它也刷成 bg（或统一改成 push 一个 bg blank）。
   - 内容：把 `build_prefixed_rows` 产出的每行经 `pad_row_to_bg` 处理后再 `push_body_row`（不再直接走 `push_body_prefixed_cont`）。
   - 附件行 `└ [Image #N]` 同样纳入块内、套 bg。
   - 下分隔：`push_body_row(bg_blank_row)` 取代 `push_body_row(Vec::new())`。

`render/cell.rs`、`render/theme.rs`、`render/screen.rs`：**无改动**。

## 边界与风险

- **原生 scrollback**：行 promote 进原生 scrollback 时按 cell 序列化，行尾已 reset，背景不会溢出到后续行。
- **终端 resize 不重排**：已 promote 的 bg 行宽度是 push 时定死的，resize 后不重排——这是 atomcode 所有 scrollback 内容的既有行为，非本功能引入的回归。
- **复制粘贴**：整行 bg 会带上尾部空格，复制用户输入时多出右侧空白。可接受（codex 同样如此）。
- **NO_COLOR / colors=false**：`role()` 在 `!caps.colors` 时返回 None，`bg` 自然为 None → 无背景，优雅降级。
- **非 unicode 终端**：前缀 glyph 走 `downgrade_glyphs` 已有降级；bg 与 glyph 正交，不受影响。

## 测试

- `retained_user_message_has_bg_block`：断言用户输入的内容行每个 cell（含尾部 pad）`style.bg == PanelBg`，且上下各有一整行 bg 空行。
- `retained_user_message_no_bg_when_colors_disabled`：`caps.colors=false` 时所有 cell `bg == None`。
- 助手输出行断言 `bg == None`（不回归）。
- 沿用 `retained_multiline_user_message_has_full_height_bar` 校验 `❯`/`▎` 前缀色不变。

## 非目标

- 不给助手输出加背景（对齐 codex）。
- 不给工具输出块加背景。
- 不新造颜色、不做终端底色探测/truecolor 混合。
- 不改 webui（本设计仅 TUI）。
