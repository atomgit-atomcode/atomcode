# Persistent Todo Panel Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the reprinted inline todowrite blocks with a persistent, in-place-updating multi-line todo panel pinned in the footer above the input.

**Architecture:** Extend the existing footer "todo row" (a single `TodoProgress` line) into a variable-height panel. The panel is driven by a persistent, in-memory `UiState.active_todos` cache (written live from `todowrite` calls, seeded from the transcript via `derive_current_todos` on resume/switch, reset on `/clear`/`/new`). A pure collapse function caps the panel height; the retained-mode cell/diff renderer updates it in place. Inline todowrite blocks are removed from both live and replay paths.

**Tech Stack:** Rust, `atomcode-tuix` (retained-mode TUI), `atomcode-capabilities::tools::todo` (todo data types, unchanged), `atomcode-core` i18n.

## Global Constraints

- Never hardcode natural-language strings in the TUI — use `atomcode-core` i18n `Msg` (add variants to `messages.rs` + `en.rs` + `zh_cn.rs`). Verbatim from spec §样式/§边界.
- Never hardcode colors — compose on `self.style_for(Role)` (which resolves theme-aware fg); only add `bold`/`faint` cell attributes. `CellStyle` supports `fg`/`bold`/`reverse`/`faint` ONLY (no strikethrough) — completed items use `faint`.
- All glyphs must have an ASCII fallback gated on `self.caps.unicode_symbols` (reuse `todo_glyph` / `todo_marker`).
- Panel never overflows the screen: it is folded into the input-box height reservation (`max_input_rows(..., status_rows + goal_rows + todo_rows)`); `todo_rows` = panel row count.
- `active_todos` is in-memory only — never written to disk. Resume rehydration is derived from the transcript.
- Feature stays behind the existing `ATOMCODE_TODO` env gate (no change needed — the tool is only registered when gated on; the panel is only fed by `todowrite` calls).
- After editing anything in `atomcode-core` (i18n), when running `atomcode-tuix` tests, `touch crates/atomcode-core/src/lib.rs` first to avoid stale build artifacts (per repo lore).

**Panel visual (unicode):**
```
☑ Todos · 2/5          ← header: ☑ marker (Brand), "Todos", " · N/M" (Muted)
  [✓] 2 completed      ← completed fold (faint), one line
  [•] wire openai_compat   ← in-progress (bold + Brand)
  [ ] update docs      ← pending (Muted)
  [ ] +1 more…         ← overflow (Muted)
```

---

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `crates/atomcode-tuix/src/render/mod.rs` | `TodoProgress` type | Add `items` field |
| `crates/atomcode-core/src/i18n/messages.rs` + `en.rs` + `zh_cn.rs` | i18n | 3 new `Msg` variants |
| `crates/atomcode-tuix/src/render/retained.rs` | Footer rendering | Pure collapse fn + cell builder + footer wiring + height |
| `crates/atomcode-tuix/src/state.rs` | UI state | Rename `live_turn_todo`→`active_todos`, drop turn-end clears |
| `crates/atomcode-tuix/src/event_loop/mod.rs` | Live capture / helpers | Capture-only (no inline block), hide-all-done filter, `todo_progress_from_messages`, delete dead block fns |
| `crates/atomcode-tuix/src/modals/session_picker.rs` | Replay | Remove inline block, seed `active_todos` |
| `crates/atomcode-tuix/src/event_loop/commands.rs` | `/clear`/`/new` reset | Reset `active_todos` |

---

## Task 1: Extend `TodoProgress` with full item list

**Files:**
- Modify: `crates/atomcode-tuix/src/render/mod.rs:516-525`
- Modify: `crates/atomcode-tuix/src/event_loop/mod.rs:11821-11835` (`todo_progress_from_items`)
- Test: `crates/atomcode-tuix/src/event_loop/mod.rs` (existing `todo_block_tests` mod near 11848)

**Interfaces:**
- Produces: `TodoProgress.items: Vec<(atomcode_capabilities::tools::todo::TodoStatus, String)>` — the full ordered list, populated by `todo_progress_from_items`.

- [ ] **Step 1: Write the failing test** — append to the `todo_block_tests` module in `event_loop/mod.rs`:

```rust
    #[test]
    fn todo_progress_carries_full_items_in_order() {
        let p = todo_progress_from_args(
            r#"{"todos":[
                {"content":"a","status":"completed"},
                {"content":"b","status":"in_progress"},
                {"content":"c","status":"pending"}
            ]}"#,
        )
        .unwrap();
        use atomcode_capabilities::tools::todo::TodoStatus;
        assert_eq!(p.items.len(), 3);
        assert_eq!(p.items[0], (TodoStatus::Completed, "a".to_string()));
        assert_eq!(p.items[1], (TodoStatus::InProgress, "b".to_string()));
        assert_eq!(p.items[2], (TodoStatus::Pending, "c".to_string()));
        assert_eq!((p.completed, p.total), (1, 3));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p atomcode-tuix todo_progress_carries_full_items_in_order`
Expected: FAIL — `no field 'items' on type TodoProgress`.

- [ ] **Step 3: Add the field.** In `render/mod.rs`, replace the `TodoProgress` struct (lines 516-525) with:

```rust
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TodoProgress {
    /// The description of the task currently `in_progress` (todowrite enforces
    /// at most one). `None` when no task is in progress (all pending / all done).
    pub current: Option<String>,
    /// Number of tasks marked `completed`.
    pub completed: usize,
    /// Total number of tasks in the list.
    pub total: usize,
    /// The full ordered list (status + content) — drives the multi-line footer
    /// todo panel. `current`/`completed`/`total` are retained as pre-computed
    /// conveniences for the header + hide-when-all-done filter.
    pub items: Vec<(atomcode_capabilities::tools::todo::TodoStatus, String)>,
}
```

- [ ] **Step 4: Populate it.** In `event_loop/mod.rs`, replace the body of `todo_progress_from_items` (lines 11821-11835) with:

```rust
pub(crate) fn todo_progress_from_items(
    todos: &[atomcode_capabilities::tools::todo::TodoItem],
) -> crate::render::TodoProgress {
    use atomcode_capabilities::tools::todo::{todo_counts, TodoStatus};
    let (completed, total) = todo_counts(todos);
    let current = todos
        .iter()
        .find(|t| t.status == TodoStatus::InProgress)
        .map(|t| t.content.clone());
    let items = todos
        .iter()
        .map(|t| (t.status, t.content.clone()))
        .collect();
    crate::render::TodoProgress {
        current,
        completed,
        total,
        items,
    }
}
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p atomcode-tuix todo_progress_carries_full_items_in_order`
Expected: PASS. Also run `cargo build -p atomcode-tuix` — the `retained.rs` test fixtures that build `TodoProgress { current, completed, total, .. }` or `TodoProgress::default()` still compile (new field defaults to empty vec via `..Default` / literal). If any literal `TodoProgress { current, completed, total }` fails to compile, add `items: vec![],` to it.

- [ ] **Step 6: Commit**

```bash
git add crates/atomcode-tuix/src/render/mod.rs crates/atomcode-tuix/src/event_loop/mod.rs
git commit -m "feat(tuix): TodoProgress carries the full ordered item list"
```

---

## Task 2: i18n `Msg` variants for the panel labels

**Files:**
- Modify: `crates/atomcode-core/src/i18n/messages.rs` (enum, near line 228)
- Modify: `crates/atomcode-core/src/i18n/en.rs` (arm, near line 310)
- Modify: `crates/atomcode-core/src/i18n/zh_cn.rs` (arm, near line 300)
- Test: `crates/atomcode-core/src/i18n/mod.rs` or the nearest existing i18n test (add a small render assertion)

**Interfaces:**
- Produces: `Msg::TodoPanelTitle`, `Msg::TodoPanelCompleted { n: usize }`, `Msg::TodoPanelMore { n: usize }` — rendered via `crate::i18n::t(...)` returning `Cow<'static, str>`.

- [ ] **Step 1: Write the failing test** — add to the test module in `atomcode-core/src/i18n/mod.rs` (create a `#[cfg(test)] mod tests` block if none exists; if one exists, append):

```rust
#[cfg(test)]
mod todo_panel_i18n_tests {
    use super::*;
    #[test]
    fn todo_panel_labels_render() {
        // Non-empty in the default locale; exact copy is locale-dependent.
        assert!(!t(Msg::TodoPanelTitle).is_empty());
        assert!(t(Msg::TodoPanelCompleted { n: 3 }).contains('3'));
        assert!(t(Msg::TodoPanelMore { n: 2 }).contains('2'));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p atomcode-core todo_panel_labels_render`
Expected: FAIL — `no variant named TodoPanelTitle`.

- [ ] **Step 3: Add the enum variants.** In `messages.rs`, after `SessionResumedLabel { name: &'a str },` (line 228) add:

```rust
    // ── Todo panel ──
    TodoPanelTitle,
    TodoPanelCompleted { n: usize },
    TodoPanelMore { n: usize },
```

- [ ] **Step 4: Add the English arms.** In `en.rs`, after the `Msg::SessionResumedLabel` arm (line 309-310) add:

```rust
        // ── Todo panel ──
        Msg::TodoPanelTitle => "Todos".into(),
        Msg::TodoPanelCompleted { n } => format!("{n} completed").into(),
        Msg::TodoPanelMore { n } => format!("+{n} more…").into(),
```

- [ ] **Step 5: Add the Chinese arms.** In `zh_cn.rs`, after the `Msg::SessionResumedLabel` arm (line 299-300) add:

```rust
        // ── 待办面板 ──
        Msg::TodoPanelTitle => "待办".into(),
        Msg::TodoPanelCompleted { n } => format!("{n} 已完成").into(),
        Msg::TodoPanelMore { n } => format!("+{n} 更多…").into(),
```

- [ ] **Step 6: Run test to verify it passes**

Run: `cargo test -p atomcode-core todo_panel_labels_render`
Expected: PASS. Also `cargo build -p atomcode-core` — the `t()` match must be exhaustive across all locales; a missing arm is a compile error (that is the intended safety net).

- [ ] **Step 7: Commit**

```bash
git add crates/atomcode-core/src/i18n/messages.rs crates/atomcode-core/src/i18n/en.rs crates/atomcode-core/src/i18n/zh_cn.rs crates/atomcode-core/src/i18n/mod.rs
git commit -m "i18n: add todo panel labels (title, completed fold, more)"
```

---

## Task 3: Pure collapse function `todo_panel_rows`

**Files:**
- Modify: `crates/atomcode-tuix/src/render/retained.rs` — add const + enum + fn near the other footer-row helpers (after `todo_row_parts`, ~line 252)
- Test: same file (there is a `#[cfg(test)] mod` with footer fixtures — add a nested test module)

**Interfaces:**
- Produces:
  - `const MAX_TODO_PANEL_ROWS: usize = 6;`
  - `enum TodoPanelRow { Header { completed, total }, CompletedFold { count }, Item { status, content }, More { hidden } }`
  - `fn todo_panel_rows(items: &[(TodoStatus, String)], completed: usize, total: usize, max_rows: usize) -> Vec<TodoPanelRow>` — total rows ≤ `max_rows`; in-progress always shown when present; display order Header, CompletedFold?, InProgress?, Pending…, More?.

- [ ] **Step 1: Write the failing tests** — add near the retained-mode test fixtures:

```rust
#[cfg(test)]
mod todo_panel_rows_tests {
    use super::*;
    use atomcode_capabilities::tools::todo::TodoStatus;

    fn items(spec: &[(TodoStatus, &str)]) -> Vec<(TodoStatus, String)> {
        spec.iter().map(|(s, c)| (*s, c.to_string())).collect()
    }

    #[test]
    fn header_plus_all_when_fits() {
        let it = items(&[
            (TodoStatus::Completed, "a"),
            (TodoStatus::InProgress, "b"),
            (TodoStatus::Pending, "c"),
        ]);
        let rows = todo_panel_rows(&it, 1, 3, MAX_TODO_PANEL_ROWS);
        // Header, CompletedFold, InProgress, Pending
        assert_eq!(rows.len(), 4);
        assert!(matches!(rows[0], TodoPanelRow::Header { completed: 1, total: 3 }));
        assert!(matches!(rows[1], TodoPanelRow::CompletedFold { count: 1 }));
        assert!(matches!(&rows[2], TodoPanelRow::Item { status: TodoStatus::InProgress, content } if content == "b"));
        assert!(matches!(&rows[3], TodoPanelRow::Item { status: TodoStatus::Pending, content } if content == "c"));
    }

    #[test]
    fn no_fold_when_none_completed() {
        let it = items(&[(TodoStatus::InProgress, "b"), (TodoStatus::Pending, "c")]);
        let rows = todo_panel_rows(&it, 0, 2, MAX_TODO_PANEL_ROWS);
        assert!(!rows.iter().any(|r| matches!(r, TodoPanelRow::CompletedFold { .. })));
    }

    #[test]
    fn pending_overflow_becomes_more() {
        let it = items(&[
            (TodoStatus::InProgress, "ip"),
            (TodoStatus::Pending, "p1"),
            (TodoStatus::Pending, "p2"),
            (TodoStatus::Pending, "p3"),
            (TodoStatus::Pending, "p4"),
        ]);
        // max_rows=4 → header + ip + (2 pending, but reserve 1 for More) → 1 pending + More{3}
        let rows = todo_panel_rows(&it, 0, 5, 4);
        assert_eq!(rows.len(), 4);
        assert!(matches!(rows.last().unwrap(), TodoPanelRow::More { hidden: 3 }));
    }

    #[test]
    fn in_progress_survives_tight_budget() {
        // body budget = 1: in-progress wins the single slot over the completed fold.
        let it = items(&[(TodoStatus::Completed, "done"), (TodoStatus::InProgress, "ip")]);
        let rows = todo_panel_rows(&it, 1, 2, 2);
        assert_eq!(rows.len(), 2);
        assert!(matches!(&rows[1], TodoPanelRow::Item { status: TodoStatus::InProgress, .. }));
    }

    #[test]
    fn never_exceeds_max_rows() {
        let mut it = items(&[(TodoStatus::InProgress, "ip"), (TodoStatus::Completed, "c")]);
        for i in 0..20 { it.push((TodoStatus::Pending, format!("p{i}"))); }
        let rows = todo_panel_rows(&it, 1, 22, MAX_TODO_PANEL_ROWS);
        assert!(rows.len() <= MAX_TODO_PANEL_ROWS);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p atomcode-tuix todo_panel_rows_tests`
Expected: FAIL — `cannot find function todo_panel_rows`.

- [ ] **Step 3: Implement the const, enum, and function** (place after `todo_row_parts`, ~line 252):

```rust
/// Max rows the footer todo panel may occupy, INCLUDING the header. The panel
/// is additionally clamped against screen height by the caller.
const MAX_TODO_PANEL_ROWS: usize = 6;

/// One logical row of the collapsed todo panel. Pure structure — glyphs,
/// i18n words, styling and width-fitting are applied in `build_todo_rows`.
#[derive(Debug, Clone, PartialEq)]
enum TodoPanelRow {
    Header { completed: usize, total: usize },
    CompletedFold { count: usize },
    Item {
        status: atomcode_capabilities::tools::todo::TodoStatus,
        content: String,
    },
    More { hidden: usize },
}

/// Collapse a todo list into at most `max_rows` panel rows (incl. header).
///
/// Selection priority under a tight budget: the in-progress task always shows
/// when present, then the completed fold, then pending items. Pending overflow
/// collapses into a single `More` row (which itself costs a row). Display order
/// is: Header, CompletedFold?, InProgress?, Pending…, More?.
fn todo_panel_rows(
    items: &[(atomcode_capabilities::tools::todo::TodoStatus, String)],
    completed: usize,
    total: usize,
    max_rows: usize,
) -> Vec<TodoPanelRow> {
    use atomcode_capabilities::tools::todo::TodoStatus;
    let mut rows = vec![TodoPanelRow::Header { completed, total }];
    let body_budget = max_rows.saturating_sub(1);
    if body_budget == 0 {
        return rows;
    }

    let in_progress: Option<&String> = items
        .iter()
        .find(|(s, _)| *s == TodoStatus::InProgress)
        .map(|(_, c)| c);
    let pendings: Vec<&String> = items
        .iter()
        .filter(|(s, _)| *s == TodoStatus::Pending)
        .map(|(_, c)| c)
        .collect();

    // Reserve high-priority slots first (in-progress, then completed fold).
    let mut used = 0usize;
    let show_ip = in_progress.is_some() && used < body_budget;
    if show_ip {
        used += 1;
    }
    let show_fold = completed > 0 && used < body_budget;
    if show_fold {
        used += 1;
    }

    // Remaining budget for pending rows (+ possible More row).
    let pend_budget = body_budget.saturating_sub(used);
    let (shown_pending, hidden) = if pend_budget == 0 {
        (0usize, 0usize) // header N/M still reflects them
    } else if pendings.len() <= pend_budget {
        (pendings.len(), 0)
    } else {
        let shown = pend_budget - 1; // reserve 1 row for the More marker
        (shown, pendings.len() - shown)
    };

    // Emit in display order.
    if show_fold {
        rows.push(TodoPanelRow::CompletedFold { count: completed });
    }
    if show_ip {
        if let Some(c) = in_progress {
            rows.push(TodoPanelRow::Item {
                status: TodoStatus::InProgress,
                content: c.clone(),
            });
        }
    }
    for c in pendings.iter().take(shown_pending) {
        rows.push(TodoPanelRow::Item {
            status: TodoStatus::Pending,
            content: (*c).clone(),
        });
    }
    if hidden > 0 {
        rows.push(TodoPanelRow::More { hidden });
    }
    rows
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p atomcode-tuix todo_panel_rows_tests`
Expected: PASS (all 5).

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-tuix/src/render/retained.rs
git commit -m "feat(tuix): pure todo-panel collapse (todo_panel_rows)"
```

---

## Task 4: Render the panel into cells + wire footer height

**Files:**
- Modify: `crates/atomcode-tuix/src/render/retained.rs`
  - Add `build_todo_rows` + `todo_panel_row_count` (near `build_todo_row`, ~1707)
  - `paint_footer`: `todo_rows` calc (1816), `todo_cells` build (1878-1881), draw loop (1964-1974)
  - `current_footer_rows`: `todo_rows` calc (~2034)
  - Remove the now-unused single-line `build_todo_row` (1707-1717)
- Test: same file

**Interfaces:**
- Consumes: `todo_panel_rows`, `MAX_TODO_PANEL_ROWS`, `TodoPanelRow` (Task 3); `TodoProgress.items` (Task 1); `Msg::TodoPanel*` (Task 2); `todo_marker`, `todo_glyph`, `build_marker_row`, `push_str_cells`, `style_for`, `CellStyle`, `scrub_controls`, `crate::width`.
- Produces: `fn build_todo_rows(&self, todo: &TodoProgress, rule_width: usize) -> Vec<Vec<Cell>>`; `fn todo_panel_row_count(&self, todo: &TodoProgress) -> usize`.

- [ ] **Step 1: Write the failing test** — add to the retained test module:

```rust
    #[test]
    fn build_todo_rows_header_and_inprogress() {
        use atomcode_capabilities::tools::todo::TodoStatus;
        let r = renderer_80x24_unicode(); // existing test helper that builds a Renderer
        let todo = crate::render::TodoProgress {
            current: Some("wire it".into()),
            completed: 1,
            total: 3,
            items: vec![
                (TodoStatus::Completed, "done a".into()),
                (TodoStatus::InProgress, "wire it".into()),
                (TodoStatus::Pending, "later".into()),
            ],
        };
        let rows = r.build_todo_rows(&todo, 40);
        let text = |cells: &Vec<Cell>| cells.iter().map(|c| c.ch).collect::<String>();
        assert!(text(&rows[0]).contains("Todos") && text(&rows[0]).contains("1/3"));
        // in-progress row is bold
        let ip = rows.iter().find(|row| text(row).contains("wire it")).unwrap();
        assert!(ip.iter().any(|c| c.style.bold));
    }
```

Note: if `renderer_80x24_unicode()` / an equivalent constructor is not the exact helper name in the test module, use whatever helper the surrounding tests already use to build a `Renderer` (grep the test module for `fn renderer` / `Renderer::new`). The assertion logic is unchanged.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p atomcode-tuix build_todo_rows_header_and_inprogress`
Expected: FAIL — `no method named build_todo_rows`.

- [ ] **Step 3: Replace `build_todo_row` (1707-1717) with the multi-row builder + count helper:**

```rust
    /// Effective panel height cap: `MAX_TODO_PANEL_ROWS`, clamped so the panel
    /// never claims more than the screen can spare (rules + one input + status).
    fn todo_panel_cap(&self) -> usize {
        let h = self.screen.height() as usize;
        MAX_TODO_PANEL_ROWS.min(h.saturating_sub(4)).max(1)
    }

    /// Number of rows the todo panel will occupy — mirrors `build_todo_rows`'
    /// row count without building cells (used by the footer height math).
    fn todo_panel_row_count(&self, todo: &crate::render::TodoProgress) -> usize {
        todo_panel_rows(&todo.items, todo.completed, todo.total, self.todo_panel_cap()).len()
    }

    /// Build the multi-line todo panel: a header marker row (`☑ Todos · N/M`)
    /// followed by collapsed item rows. Sits directly above the status line (and
    /// above the goal/loop row when present). Theme-safe: Brand marker, bold
    /// in-progress, faint completed/fold, Muted pending/more. ASCII fallback via
    /// `todo_marker`/`todo_glyph`.
    fn build_todo_rows(
        &self,
        todo: &crate::render::TodoProgress,
        rule_width: usize,
    ) -> Vec<Vec<Cell>> {
        use atomcode_capabilities::tools::todo::{todo_glyph, TodoStatus};
        let unicode = self.caps.unicode_symbols;
        let rows = todo_panel_rows(&todo.items, todo.completed, todo.total, self.todo_panel_cap());

        // width budget for an indented item line: `  <glyph> <content>`
        let item_line = |glyph: &str, body: &str, style: &CellStyle| -> Vec<Cell> {
            let mut row = Vec::new();
            let gw = crate::width::display_width(glyph);
            let budget = rule_width.saturating_sub(2 + gw + 1); // indent + glyph + space
            let fitted = crate::width::truncate_with_ellipsis(&scrub_controls(body), budget);
            push_str_cells(&mut row, &format!("  {glyph} {fitted}"), style);
            row
        };

        rows.into_iter()
            .map(|r| match r {
                TodoPanelRow::Header { completed, total } => self.build_marker_row(
                    todo_marker(unicode),
                    &crate::i18n::t(crate::i18n::Msg::TodoPanelTitle).into_owned(),
                    &format!(" \u{b7} {completed}/{total}"),
                ),
                TodoPanelRow::CompletedFold { count } => {
                    let style = CellStyle { faint: true, ..self.style_for(Role::Muted) };
                    let label = crate::i18n::t(crate::i18n::Msg::TodoPanelCompleted { n: count })
                        .into_owned();
                    item_line(todo_glyph(TodoStatus::Completed, unicode), &label, &style)
                }
                TodoPanelRow::Item { status, content } => {
                    let style = match status {
                        TodoStatus::InProgress => {
                            CellStyle { bold: true, ..self.style_for(Role::Brand) }
                        }
                        TodoStatus::Completed => {
                            CellStyle { faint: true, ..self.style_for(Role::Muted) }
                        }
                        TodoStatus::Pending => self.style_for(Role::Muted),
                    };
                    item_line(todo_glyph(status, unicode), &content, &style)
                }
                TodoPanelRow::More { hidden } => {
                    let style = self.style_for(Role::Muted);
                    let label =
                        crate::i18n::t(crate::i18n::Msg::TodoPanelMore { n: hidden }).into_owned();
                    item_line(todo_glyph(TodoStatus::Pending, unicode), &label, &style)
                }
            })
            .collect()
    }
```

- [ ] **Step 4: Update `paint_footer` height + draw.**

(4a) Replace the `todo_rows` line (1816):

```rust
        let todo_rows = self
            .status
            .todo
            .as_ref()
            .map(|t| self.todo_panel_row_count(t))
            .unwrap_or(0);
```

(4b) Replace the `todo_cells` pre-build (1878-1881):

```rust
        let todo_cells: Vec<Vec<Cell>> = status_clone
            .todo
            .as_ref()
            .map(|t| self.build_todo_rows(t, rule_width))
            .unwrap_or_default();
```

(4c) Replace the todo draw + status draw block (1964-1974):

```rust
        let todo_top = goal_top + goal_rows;
        for (i, tr) in todo_cells.into_iter().enumerate() {
            let mut padded = tr;
            Self::pad_row_to_width(&mut padded, w);
            self.screen.draw_row(todo_top + i, 0, &padded);
        }
        if let Some(st) = status_cells {
            let mut padded = st;
            Self::pad_row_to_width(&mut padded, w);
            self.screen.draw_row(todo_top + todo_rows, 0, &padded);
        }
```

- [ ] **Step 5: Update `current_footer_rows`** — replace the `todo_rows` line (~2034):

```rust
        let todo_rows = self
            .status
            .todo
            .as_ref()
            .map(|t| self.todo_panel_row_count(t))
            .unwrap_or(0);
```

- [ ] **Step 6: Run tests**

Run: `cargo test -p atomcode-tuix build_todo_rows_header_and_inprogress` then `cargo test -p atomcode-tuix --lib`
Expected: PASS. The 4 pre-existing retained byte-budget red tests are known-unrelated (per repo lore) — confirm no NEW failures.

- [ ] **Step 7: Commit**

```bash
git add crates/atomcode-tuix/src/render/retained.rs
git commit -m "feat(tuix): render multi-line todo panel in footer"
```

---

## Task 5: Persistent state + live capture (no inline block) + hide-all-done + reset

**Files:**
- Modify: `crates/atomcode-tuix/src/state.rs` (341 field, 475 init, 724/742/753 clears)
- Modify: `crates/atomcode-tuix/src/event_loop/mod.rs` (8754-8774 live arm, 10767 read filter)
- Modify: `crates/atomcode-tuix/src/event_loop/commands.rs` (reset_to_new_session, ~4295)
- Test: `crates/atomcode-tuix/src/state.rs`

**Interfaces:**
- Produces: `UiState.active_todos: Option<TodoProgress>` — persistent (NOT cleared at turn end); read by the footer with a `total > 0 && completed < total` filter.

- [ ] **Step 1: Write the failing test** — add to the state.rs test module (grep for `mod tests` in state.rs; if absent, add one):

```rust
    #[test]
    fn active_todos_persists_across_turn_end() {
        let mut s = UiState::default(); // or the existing test constructor
        s.active_todos = Some(crate::render::TodoProgress {
            current: Some("x".into()),
            completed: 0,
            total: 2,
            items: vec![],
        });
        s.on_turn_complete();
        assert!(s.active_todos.is_some(), "panel must survive turn end");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p atomcode-tuix active_todos_persists_across_turn_end`
Expected: FAIL — `no field active_todos` (field still named `live_turn_todo`).

- [ ] **Step 3: Rename + change semantics in `state.rs`.**

(3a) Replace the field (335-341) doc + decl:

```rust
    /// Active todo list for the persistent footer todo PANEL. Written from the
    /// turn's `todowrite` calls, seeded from the transcript on resume/switch
    /// (`replay_session`), reset on `/clear`/`/new` (`reset_to_new_session`).
    /// Unlike the old live-only row, this PERSISTS across turn boundaries — the
    /// panel is a standing view, hidden only when the list is empty or all done.
    pub active_todos: Option<crate::render::TodoProgress>,
```

(3b) Init (475): `live_turn_todo: None,` → `active_todos: None,`

(3c) Remove the three turn-end clears — delete `self.live_turn_todo = None;` at lines 724, 742, 753. (Leave the surrounding `subagent_activity = None;` etc. intact. Also remove/adjust the now-stale comment block at 720-723 that explains the live-only hand-back.)

- [ ] **Step 4: Update the live capture arm** in `event_loop/mod.rs` (8754-8774) — replace the whole `if name == "todowrite" { … }` block with capture-only (no inline block, still suppress the tool result):

```rust
            // todowrite: the persistent footer PANEL is the sole view. Capture the
            // full list into `active_todos` (the transcript won't carry it until
            // turn end), and suppress the tool CALL + RESULT rows. On a parse
            // failure fall through to the normal tool row so the error surfaces.
            if name == "todowrite" {
                if let Some(progress) = todo_progress_from_args(&arguments) {
                    state.active_todos = Some(progress);
                    // call_rendered=true ⇒ ToolCallResult suppresses the result row.
                    pending_tools.insert(id, (display.clone(), detail, true));
                    state.on_tool_call_started(&display);
                    return;
                }
            }
```

(Note: `todo_block_styled_lines`, `UiLine::AssistantLineBreak` / `CommandOutput` for the block, and `renderer.flush()` in the old block are gone — the panel replaces them.)

- [ ] **Step 5: Update the footer read filter** (10761-10767) — replace the comment + `let todo = …` with:

```rust
    // Todo panel source: the persistent `active_todos` cache. Hidden when the
    // list is empty (`total == 0`) or fully done (`completed == total`) so a
    // finished panel disappears — otherwise it stands across turns and resume.
    let todo = state
        .active_todos
        .clone()
        .filter(|p| p.total > 0 && p.completed < p.total);
```

- [ ] **Step 6: Reset on new session** — in `commands.rs` `reset_to_new_session`, after `state.on_turn_complete();` (line 4295) add:

```rust
    state.active_todos = None;
```

- [ ] **Step 7: Run tests + build**

Run: `cargo build -p atomcode-tuix && cargo test -p atomcode-tuix active_todos_persists_across_turn_end`
Expected: build clean (all `live_turn_todo` references updated — the compiler enforces this), test PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/atomcode-tuix/src/state.rs crates/atomcode-tuix/src/event_loop/mod.rs crates/atomcode-tuix/src/event_loop/commands.rs
git commit -m "feat(tuix): persistent active_todos, capture-only todowrite, hide when done"
```

---

## Task 6: Replay — remove inline block, seed the panel from the transcript

**Files:**
- Modify: `crates/atomcode-tuix/src/modals/session_picker.rs` (576-593 replay arm; end of `replay_session` ~ after the message loop)
- Modify: `crates/atomcode-tuix/src/event_loop/mod.rs` — add `todo_progress_from_messages`
- Test: `crates/atomcode-tuix/src/modals/session_picker.rs` (existing replay tests at ~998/1083)

**Interfaces:**
- Consumes: `derive_current_todos` (capabilities), `todo_progress_from_items` (Task 1).
- Produces: `fn todo_progress_from_messages(messages: &[Message]) -> Option<TodoProgress>`.

- [ ] **Step 1: Write the failing test** — add to the session_picker test module:

```rust
    #[test]
    fn replay_seeds_active_todos_from_transcript() {
        use atomcode_core::conversation::message::Message;
        use atomcode_kernel::tool::ToolCall;
        let mut rec = /* the existing recording-renderer used by neighbouring tests */;
        let mut state = /* the existing UiState test constructor used nearby */;
        let mut session = atomcode_core::session::Session::default_session(".".into());
        session.messages = vec![Message::assistant(
            "",
            vec![ToolCall {
                id: "1".into(),
                name: "todowrite".into(),
                arguments: r#"{"todos":[{"content":"a","status":"in_progress"},{"content":"b","status":"pending"}]}"#.into(),
            }],
        )];
        replay_session(&mut rec, &mut state, &session, false);
        let p = state.active_todos.expect("panel seeded from transcript");
        assert_eq!(p.total, 2);
        assert_eq!(p.current.as_deref(), Some("a"));
    }
```

(Match `rec`/`state` construction to the two existing `replay_session(&mut rec, &mut state, &session, false)` tests at lines ~998/1083 — copy their setup verbatim.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p atomcode-tuix replay_seeds_active_todos_from_transcript`
Expected: FAIL — `active_todos` is `None` after replay.

- [ ] **Step 3: Add `todo_progress_from_messages`** in `event_loop/mod.rs` (next to `todo_progress_from_args`, ~11842):

```rust
/// Todo panel state derived from a full transcript — the last VALID `todowrite`
/// call wins (see `derive_current_todos`). `None` when the session never used
/// todowrite. Used to seed the panel on `/resume` / session switch with zero
/// extra storage.
pub(crate) fn todo_progress_from_messages(
    messages: &[atomcode_kernel::message::Message],
) -> Option<crate::render::TodoProgress> {
    let todos = atomcode_capabilities::tools::todo::derive_current_todos(messages);
    if todos.is_empty() {
        None
    } else {
        Some(todo_progress_from_items(&todos))
    }
}
```

(If `atomcode_kernel::message::Message` is not the type held by `Session.messages`, use the same type the replay loop iterates — grep `session.messages` element type; `derive_current_todos` takes `&[atomcode_kernel::message::Message]`, so convert/borrow accordingly. `session_picker.rs` already imports the message types it needs.)

- [ ] **Step 4: Strip the inline block from replay** — replace the todowrite arm (576-593) with suppress-only:

```rust
                for tc in tool_calls {
                    // todowrite → no inline block; the persistent panel is the
                    // sole view. Suppress the (successful) tool RESULT below by
                    // remembering the call id. Mirror the live path: only a
                    // PARSEABLE call is suppressed — a bad one falls through to a
                    // normal tool row so its error still shows.
                    if tc.name == "todowrite"
                        && atomcode_capabilities::tools::todo::parse_todos(&tc.arguments).is_ok()
                    {
                        if !tc.id.is_empty() {
                            todowrite_call_ids.insert(tc.id.clone());
                        }
                        continue;
                    }
                    renderer.render(UiLine::ToolCall {
                        name: crate::event_loop::display_tool_name(&tc.name),
                        detail: format_tool_detail(&tc.name, &tc.arguments),
                    });
                }
```

- [ ] **Step 5: Seed the panel** — at the end of `replay_session`, after the `for (i, m) in session.messages.iter().enumerate()` loop closes and before the final `renderer.end_sync()` / return, add:

```rust
    // Seed the persistent todo panel from the transcript (zero extra storage).
    // This both RESETS the previous session's panel and rehydrates the loaded
    // one, so session switch / resume land on the correct list.
    state.active_todos = crate::event_loop::todo_progress_from_messages(&session.messages);
```

(Locate the exact insertion point by reading the tail of `replay_session`; place it just before the function's final renderer flush/`end_sync`.)

- [ ] **Step 6: Run tests**

Run: `touch crates/atomcode-core/src/lib.rs && cargo test -p atomcode-tuix replay_seeds_active_todos_from_transcript`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/atomcode-tuix/src/modals/session_picker.rs crates/atomcode-tuix/src/event_loop/mod.rs
git commit -m "feat(tuix): seed todo panel on resume, drop inline replay block"
```

---

## Task 7: Delete the now-dead inline-block helpers

**Files:**
- Modify: `crates/atomcode-tuix/src/event_loop/mod.rs` — remove `todo_block_lines` (11781-11798), `todo_block_styled_lines` (11800-11817), and their `todo_block_tests` cases that reference them (the `..._weights_by_status` test and the raw-SGR assertions).

**Interfaces:** none (pure removal). Verify no remaining callers before deleting.

- [ ] **Step 1: Verify no callers remain**

Run: `grep -rn "todo_block_styled_lines\|todo_block_lines" crates/`
Expected: only the definitions + their own tests (both call sites removed in Tasks 5 and 6). If any non-test caller remains, STOP and fix it first.

- [ ] **Step 2: Delete the two functions** (`todo_block_lines`, `todo_block_styled_lines`) and the tests that call them. Keep the `todo_progress_*` tests (still valid). If `todo_block_tests` becomes empty, remove the empty module.

- [ ] **Step 3: Run build + tests**

Run: `cargo build -p atomcode-tuix && cargo test -p atomcode-tuix --lib`
Expected: clean build (no `unused function` warnings for the deleted fns), tests green apart from the 4 known-unrelated retained byte-budget reds.

- [ ] **Step 4: Commit**

```bash
git add crates/atomcode-tuix/src/event_loop/mod.rs
git commit -m "refactor(tuix): remove dead inline todo-block renderers"
```

---

## Task 8: Full verification

- [ ] **Step 1: Whole-workspace build + test**

Run: `touch crates/atomcode-core/src/lib.rs && cargo build && cargo test -p atomcode-tuix -p atomcode-capabilities -p atomcode-core`
Expected: build clean; tuix green except the 4 pre-existing retained byte-budget red tests (confirm they are the SAME 4 as on a clean checkout — `git stash` is FORBIDDEN per repo lore; instead compare against a fresh `cargo test` on the parent commit in a separate worktree if unsure).

- [ ] **Step 2: Manual smoke (documented, not automated)** — record in the commit/PR body that the following need a real terminal (cannot be unit-tested):
  1. Trigger a multi-step `todowrite` (with `ATOMCODE_TODO` enabled); confirm the panel appears above the input and UPDATES IN PLACE across turns (no repeated inline blocks in scrollback).
  2. Long list (>5 items) → completed collapses to one line, in-progress shown, pending capped with `+K more…`.
  3. Mark all complete → panel disappears.
  4. `/resume` a session that used todowrite → panel rehydrates.
  5. `/clear` → panel gone.
  6. Non-unicode terminal (`TERM` without unicode / caps off) → `+`/`[~]`/`[x]`/`[ ]` ASCII fallbacks.
  7. Narrow + short terminal → input box still usable (panel yields), no overflow.

- [ ] **Step 3: Request code review**

Use the `superpowers:requesting-code-review` skill (or `/code-review`) on the branch diff before merge.

---

## Self-Review (completed during authoring)

- **Spec coverage:** §数据模型→T1; §生命周期(跨turn/隐藏/清空)→T5; §Resume→T6; §渲染→T4; §折叠算法→T3; §样式/主题→T4; §字形/降级→T4 (ASCII via todo_glyph/todo_marker); §内联块移除→T5(live)+T6(replay)+T7(delete); §开关→unchanged (Global Constraints); §测试→per-task + T8. i18n discipline → T2.
- **Placeholder scan:** none — every code step shows full code. Two flagged lookups (`renderer_80x24_unicode` helper name in T4-S1, replay test `rec`/`state` setup in T6-S1) are explicitly "copy the neighbouring test's setup", not TODOs.
- **Type consistency:** `active_todos: Option<TodoProgress>` (T5) used identically in read filter (T5), replay seed (T6), reset (T5). `TodoProgress.items: Vec<(TodoStatus, String)>` (T1) consumed by `todo_panel_rows` (T3) and `build_todo_rows` (T4). `todo_panel_rows`/`TodoPanelRow`/`MAX_TODO_PANEL_ROWS` names consistent across T3/T4. `Msg::TodoPanelTitle|Completed{n}|More{n}` consistent T2/T4.
