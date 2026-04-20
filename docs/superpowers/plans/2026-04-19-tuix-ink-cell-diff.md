# atomcode-tuix Ink 化 — Cell-level Diff 实施计划

**目标**：对齐 CC 视觉（两条铺满 UTF-8 rule + 菜单扩展 footer）+ CC 流畅度。通过 cell-level diff 让 footer 高度变化的字节量从 ~1900 B 压到 ~600 B 以下。

**Scope**：**只对 footer + menu 区域做 cell-diff**。body（scroll region 内的 streaming / tool output / history）保持 pure-append。这是最小 Ink 化 — 不重写整个 render，只把 footer path 的 row-level diff 升级到 cell-level。

**不动的**：DECSTBM 固定 footer 架构、body append 路径、streaming flush_assistant_lines、lifecycle（shutdown/suspend/resume）。

---

## 核心原理

当前 footer render：
```
build_X_row -> Vec<u8>  (pre-encoded ANSI bytes)
emit_footer_absolute: 按 row index diff; 任何 row 变 → 整行 emit
```

Ink 化后：
```
build_X_row -> Vec<Cell>  (字符 + 样式 per cell)
emit_footer_cell_diff: 按 (row, col) cell diff; 只 emit 变化的 cell
```

footer 高度变化（菜单开合）时：
- 原来：5 行 footer 占屏幕 [H-4, H]，新 9 行占 [H-8, H] → 每行都是"新位置"，全量 emit 1900 B
- cell-diff：status 行绝对行号不变（都是 H）且内容不变 → **0 bytes**；rule 中间的 `─` 很多 cell 相同 → skip；只 emit 真正变化的 cell → **~600 B**

---

## 设计决策

### Cell 结构

```rust
pub struct Cell {
    pub ch: char,
    pub fg: Option<Color>,    // crossterm::style::Color，None = default fg
    pub bold: bool,
    pub reverse: bool,
}
```

只覆盖 footer 用到的 SGR：fg color、bold、reverse video。不支持 bg color、underline、italic 等（footer 不用）。如未来要扩展，加字段即可。

### Cell equality

`#[derive(PartialEq, Eq)]`。crossterm::style::Color 已 derive PartialEq。Cell 比较 = 所有字段相同 = 字节级相同。

### Frame buffer

`last_footer_cells: Vec<Vec<Cell>>` 替代 `last_footer_rows: Vec<Vec<u8>>`。按 footer row 相对索引存储（0 = footer top row, N-1 = footer bottom）。

**绝对行号 diff**：diff 时把旧 cells 按绝对屏幕行号映射（`prev_footer_top + i`），新 cells 按新绝对行号（`new_footer_top + i`）。两者在同一屏幕行号时比较。

### SGR 状态机 emit

serialize(patches) 时维护 `current_style`：
- 只在 cell.style 和 current_style 不同时 emit SGR 序列
- 合并相邻 cell 同一样式 → 一次 SGR 覆盖多 cell
- 光标连续 cell 时 auto-advance，不重发 `\x1b[row;col H`

预期 SGR 开销降低 30-50%。

### Menu 仍属于 footer

不改 C 方案那种菜单剥离。菜单 4 行仍是 footer 的一部分（footer_rows = 5 或 9）。DECSTBM 继续管理 scroll region。这保持 CC 同款行为。

### 降级路径

非 DECSTBM 路径（dumb terminal、测试）保留现 row-level 逻辑。cell-diff 只在 `caps.scroll_region = true` 时启用。

---

## 实施步骤

### Task 17: Cell struct + CellStyle

**Files**:
- 新 `crates/atomcode-tuix/src/render/cell.rs`

```rust
use crossterm::style::Color;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub style: CellStyle,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CellStyle {
    pub fg: Option<Color>,
    pub bold: bool,
    pub reverse: bool,
}

/// Helper: push N cells with the same style into a row.
pub fn push_str_cells(row: &mut Vec<Cell>, s: &str, style: CellStyle) {
    for ch in s.chars() {
        row.push(Cell { ch, style: style.clone() });
    }
}
```

**Tests**: `Cell::default()` == 全默认、相同 cell eq、不同 cell neq。

### Task 18: 改 build_X_row 返回 Vec<Cell>

**Files**:
- `crates/atomcode-tuix/src/render/ansi.rs`

5 个方法改造：
- `build_spinner_row` → `Vec<Cell>`
- `build_rule_row` → `Vec<Cell>`
- `build_middle_row` → `Vec<Cell>`
- `build_menu_row` → `Vec<Cell>`
- `build_status_row` → `Vec<Cell>`

每个方法：
- 原来用 `push_sgr_fg` / `push_sgr_bold_on` 等往 String 里塞 ANSI → 改成维护 `CellStyle` + `push_str_cells(&mut row, s, style)`
- 右填充用 `Cell { ch: ' ', style: default }` 填到 rule_width
- PAD_COL 用 default style 空格

**回退兼容**: 加一个 `cell_row_to_bytes(cells: &[Cell]) -> Vec<u8>` 把 cells 序列化成 ANSI 字节（for legacy 非 DECSTBM 路径）。

### Task 19: cell-diff + serialize

**Files**:
- `crates/atomcode-tuix/src/render/cell.rs` (加 diff 函数)

```rust
pub struct Patch {
    pub row: u16,  // absolute terminal row, 1-indexed
    pub col: u16,  // absolute terminal col, 1-indexed
    pub cell: Cell,
}

/// Diff new_rows vs prev_rows cell by cell.
/// Rows indexed by their ABSOLUTE terminal row number.
pub fn diff_cells(
    prev: &HashMap<u16, Vec<Cell>>,
    next: &HashMap<u16, Vec<Cell>>,
) -> Vec<Patch>;

/// Serialize patches into ANSI bytes with SGR state machine.
/// Emits cursor moves only when non-adjacent, SGR only on style change.
pub fn serialize_patches(patches: &[Patch]) -> Vec<u8>;
```

### Task 20: emit_footer_cell_diff

**Files**:
- `crates/atomcode-tuix/src/render/ansi.rs`

新 `emit_footer_cell_diff(&mut self, new_cells: Vec<Vec<Cell>>)`:
1. 计算 new_footer_top = `H - new_total_rows + 1`
2. 构造 `next: HashMap<u16, Vec<Cell>>` 把 cells 按绝对行号映射
3. 调 `diff_cells(&self.last_footer_cells_by_row, &next)` 得 patches
4. 调 `serialize_patches(&patches)` 得 ANSI bytes
5. `self.out.write_all(bytes)`
6. 更新 `self.last_footer_cells_by_row = next`

在 `draw_footer_here_with_prev_cursor` 里 DECSTBM 路径改走这个方法。

### Task 21: 验证

新 byte test `footer_menu_toggle_byte_cost`：
- 先 render 基础 footer (5 rows, no menu) 稳态
- open menu（5→9 rows）测字节
- close menu（9→5 rows）测字节
- 目标：**单次 toggle < 600 B**（原 ~1900 B）

158 lib test 保持全绿。

手测 checklist：
- streaming 流畅（确认 43 B/delta 没回归）
- 打字稳态（56 B/keystroke 没回归）
- 菜单开合不卡顿
- login resume 幽灵线无
- 视觉和 CC 同款（两条铺满 rule）

---

## 工期

| Task | 规模 | 风险 |
|---|---|---|
| 17 Cell struct | ~80 行 | Low |
| 18 build_X_row 改造 | ~200 行改 | Medium（要重写 SGR emission） |
| 19 diff + serialize | ~150 行 | Medium（SGR 状态机 edge case） |
| 20 集成到 footer 路径 | ~100 行 | Medium（确保 cache 正确） |
| 21 验证 | ~80 行 | Low |
| **合计** | **~610 行** | 约 6-8 小时 |

---

## 不做的（out of scope）

- body 区 cell-diff（streaming body 仍走 append）
- 菜单剥离到 scroll region（C 方案已证伪）
- alt-screen
- ratatui 迁移
- 响应式布局调整（rule 居中 / 缩短等视觉改动）
