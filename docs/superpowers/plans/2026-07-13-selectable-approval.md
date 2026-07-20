# Selectable Approval Panel Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the type-`y/a/n` tool approval with a footer-pinned, arrow-key-selectable vertical option list (Allow once / Always allow / Deny).

**Architecture:** A new footer-region panel (like the todo panel) shows during `UiPhase::Approval`, driven by `UiState.approval_panel`. `↑/↓` move the selection, `Enter` confirms, `Esc` denies, `y/a/n` are accelerators. The decision maps to the existing `AgentCommand` → `deliver_approval` path (nothing below tuix changes). Moving approval out of the body lets us delete the fragile `pop_approval_prompt` body-erase.

**Tech Stack:** Rust, `atomcode-tuix` (state/render/input), `atomcode-core` i18n (option labels).

## Global Constraints

- Keyboard-only (v1, no mouse). `↑/↓` select (wrap), `Enter` confirm, `Esc` = Deny, `y/a/n` accelerators. Default selection = index 0 (Allow once).
- Options are exactly: Allow once / Always allow / Deny — mapped to `AgentCommand::ApproveTool` / `ApproveToolAlways` / `DenyTool`. No deny-with-feedback.
- "Always allow" label uses the TOOL NAME (`Always allow bash`) — the `AgentEvent::ApprovalNeeded` event carries no scope, so the exact scope pattern is NOT shown.
- Foreground color + `reverse` only (the cell model has no background). Selected row = `▸ ` prefix + reverse-video (mirrors `build_menu_row`). No hardcoded colors — use `style_for(Role)`.
- All glyphs (`▌` left bar, `▸`, `⚠`) need an ASCII fallback gated on `self.caps.unicode_symbols`.
- Never hardcode natural-language strings — option labels + header + hint via `atomcode-core` i18n `Msg`.
- The panel is footer-region; on resolve it just stops rendering (no body erase). Keep the permanent `▸ Tool(detail)` body row.
- COMMIT DISCIPLINE: `git add <exact path>` only; never `-A`/`.`/`-u`. Ignore the unrelated `crates/atomcode-codingplan-crypto/*` files — never stage them.
- Known: ~4 pre-existing tuix "byte budget" retained tests fail — unrelated; confirm the count doesn't increase. After editing core i18n, `touch crates/atomcode-core/src/lib.rs` before running tuix tests.

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `crates/atomcode-tuix/src/state.rs` | UI state | `ApprovalKind`/`ApprovalOption`/`ApprovalPanel` types; `UiState.approval_panel`; clear in `on_approval_resolved` + turn-end/reset paths |
| `crates/atomcode-core/src/i18n/{messages,en,zh_cn}.rs` | i18n | 4 `Msg` variants (allow-once / always-allow / deny / hint) |
| `crates/atomcode-tuix/src/event_loop/mod.rs` | wiring + input | `build_approval_options`; `ApprovalNeeded` handler sets panel; `handle_approval_key` arrows/enter/esc/y-a-n + drop pop call |
| `crates/atomcode-tuix/src/event_loop/commands.rs` | `/bg` resume | set panel instead of emit `ApprovalPrompt` |
| `crates/atomcode-tuix/src/render/retained.rs` | footer render | `build_approval_rows` + `approval_panel_row_count` + `paint_footer` slot + height; **remove** `ApprovalPrompt` arm + `pop_approval_prompt` + `approval_block_rows` |
| `crates/atomcode-tuix/src/render/plain.rs` | pipe render | non-interactive approval text (keep); remove `ApprovalPrompt` arm in cleanup |
| `crates/atomcode-tuix/src/render/mod.rs` + `worker.rs` | cleanup | remove `UiLine::ApprovalPrompt` variant + `PopApprovalPrompt` cmd + trait method |

---

## Task 1: State types + i18n labels + options builder

**Files:**
- Modify: `crates/atomcode-tuix/src/state.rs` (add types + `approval_panel` field + clear in `on_approval_resolved`)
- Modify: `crates/atomcode-core/src/i18n/messages.rs` + `en.rs` + `zh_cn.rs`
- Modify: `crates/atomcode-tuix/src/event_loop/mod.rs` (`build_approval_options` + test)

**Interfaces produced:**
- `pub enum ApprovalKind { AllowOnce, AlwaysAllow, Deny }`
- `pub struct ApprovalOption { pub label: String, pub kind: ApprovalKind, pub accel: char }`
- `pub struct ApprovalPanel { pub tool: String, pub detail: String, pub options: Vec<ApprovalOption>, pub selected: usize }` with methods `move_up(&mut self)`, `move_down(&mut self)`, `accel_index(&self, c: char) -> Option<usize>`
- `UiState.approval_panel: Option<ApprovalPanel>`
- `pub(crate) fn build_approval_options(tool: &str) -> Vec<ApprovalOption>` (event_loop)
- `Msg::ApprovalAllowOnce`, `Msg::ApprovalAlwaysAllow { tool: &'a str }`, `Msg::ApprovalDeny`, `Msg::ApprovalHint`

- [ ] **Step 1: Add the i18n variants.** In `crates/atomcode-core/src/i18n/messages.rs`, after the todo-panel variants (`TodoPanelMore { n: usize }`), add:
```rust
    // ── Approval panel ──
    ApprovalAllowOnce,
    ApprovalAlwaysAllow { tool: &'a str },
    ApprovalDeny,
    ApprovalHint,
```
In `en.rs`, after the todo-panel arms, add:
```rust
        // ── Approval panel ──
        Msg::ApprovalAllowOnce => "Allow once".into(),
        Msg::ApprovalAlwaysAllow { tool } => format!("Always allow {tool} (this session)").into(),
        Msg::ApprovalDeny => "Deny".into(),
        Msg::ApprovalHint => "↑↓ select · enter confirm · esc deny".into(),
```
In `zh_cn.rs`, after the todo-panel arms, add:
```rust
        // ── 审批面板 ──
        Msg::ApprovalAllowOnce => "允许一次".into(),
        Msg::ApprovalAlwaysAllow { tool } => format!("本会话总是允许 {tool}").into(),
        Msg::ApprovalDeny => "拒绝".into(),
        Msg::ApprovalHint => "↑↓ 选 · enter 确认 · esc 拒绝".into(),
```

- [ ] **Step 2: Write the failing test** — append to the state.rs test module (`mod tests`, uses `UiState::new()`):
```rust
    #[test]
    fn approval_panel_selection_wraps_and_accel_maps() {
        use crate::state::{ApprovalKind, ApprovalOption, ApprovalPanel};
        let mut p = ApprovalPanel {
            tool: "bash".into(),
            detail: "rm -rf build/".into(),
            options: vec![
                ApprovalOption { label: "Allow once".into(), kind: ApprovalKind::AllowOnce, accel: 'y' },
                ApprovalOption { label: "Always allow bash".into(), kind: ApprovalKind::AlwaysAllow, accel: 'a' },
                ApprovalOption { label: "Deny".into(), kind: ApprovalKind::Deny, accel: 'n' },
            ],
            selected: 0,
        };
        p.move_up();
        assert_eq!(p.selected, 2, "up from 0 wraps to last");
        p.move_down();
        assert_eq!(p.selected, 0, "down from last wraps to 0");
        p.move_down();
        assert_eq!(p.selected, 1);
        assert_eq!(p.accel_index('A'), Some(1), "accel is case-insensitive");
        assert_eq!(p.accel_index('n'), Some(2));
        assert_eq!(p.accel_index('z'), None);
    }
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p atomcode-tuix approval_panel_selection_wraps_and_accel_maps`
Expected: FAIL — `ApprovalPanel` not found.

- [ ] **Step 4: Add the types** — in `state.rs`, near the other UI-state structs (top level of the module), add:
```rust
/// A tool-approval decision offered in the footer approval panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalKind {
    AllowOnce,
    AlwaysAllow,
    Deny,
}

/// One selectable row of the approval panel.
#[derive(Debug, Clone)]
pub struct ApprovalOption {
    pub label: String,
    pub kind: ApprovalKind,
    /// Single-key accelerator (lower-case): 'y' / 'a' / 'n'.
    pub accel: char,
}

/// The active tool-approval prompt, shown as a footer panel while
/// `UiPhase::Approval`. `None` when no approval is pending.
#[derive(Debug, Clone)]
pub struct ApprovalPanel {
    pub tool: String,
    pub detail: String,
    pub options: Vec<ApprovalOption>,
    pub selected: usize,
}

impl ApprovalPanel {
    pub fn move_up(&mut self) {
        if self.options.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.options.len() - 1
        } else {
            self.selected - 1
        };
    }
    pub fn move_down(&mut self) {
        if self.options.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.options.len();
    }
    /// Index of the option whose accelerator matches `c` (case-insensitive).
    pub fn accel_index(&self, c: char) -> Option<usize> {
        let c = c.to_ascii_lowercase();
        self.options.iter().position(|o| o.accel == c)
    }
}
```

- [ ] **Step 5: Add the field + clear-on-resolve.** In the `UiState` struct add (near `phase`/`prior_spinner_label`):
```rust
    /// The active footer approval panel (arrow-key selectable). `None` when no
    /// tool approval is pending. Set in the `ApprovalNeeded` handler, cleared on
    /// resolve / turn-end / session reset.
    pub approval_panel: Option<ApprovalPanel>,
```
In `UiState::new()` add the initializer `approval_panel: None,`.
In `on_approval_resolved` (the fn that sets `self.phase = UiPhase::Streaming`), add as the FIRST line of the body:
```rust
        self.approval_panel = None;
```

- [ ] **Step 6: Add `build_approval_options`** — in `event_loop/mod.rs` (near `approval_command_to_decision`), add:
```rust
/// The three approval options for `tool`, in display order (Allow once is the
/// default selection). The "Always allow" label carries the tool name because
/// `AgentEvent::ApprovalNeeded` does not carry the grant scope.
pub(crate) fn build_approval_options(tool: &str) -> Vec<crate::state::ApprovalOption> {
    use crate::state::{ApprovalKind, ApprovalOption};
    vec![
        ApprovalOption {
            label: crate::i18n::t(crate::i18n::Msg::ApprovalAllowOnce).into_owned(),
            kind: ApprovalKind::AllowOnce,
            accel: 'y',
        },
        ApprovalOption {
            label: crate::i18n::t(crate::i18n::Msg::ApprovalAlwaysAllow { tool }).into_owned(),
            kind: ApprovalKind::AlwaysAllow,
            accel: 'a',
        },
        ApprovalOption {
            label: crate::i18n::t(crate::i18n::Msg::ApprovalDeny).into_owned(),
            kind: ApprovalKind::Deny,
            accel: 'n',
        },
    ]
}
```
And append its test to the `bypass_approval_tests` module (or a new module) in `event_loop/mod.rs`:
```rust
    #[test]
    fn build_approval_options_shape() {
        use crate::state::ApprovalKind;
        let opts = super::build_approval_options("bash");
        assert_eq!(opts.len(), 3);
        assert_eq!((opts[0].kind, opts[0].accel), (ApprovalKind::AllowOnce, 'y'));
        assert_eq!((opts[1].kind, opts[1].accel), (ApprovalKind::AlwaysAllow, 'a'));
        assert!(opts[1].label.contains("bash"), "always-allow label names the tool: {}", opts[1].label);
        assert_eq!((opts[2].kind, opts[2].accel), (ApprovalKind::Deny, 'n'));
    }
```

- [ ] **Step 7: Build + test**

Run: `cargo build -p atomcode-core && cargo build -p atomcode-tuix`
Expected: clean (i18n match exhaustiveness across en/zh_cn is the compiler's safety net).
Run: `cargo test -p atomcode-tuix approval_panel_selection_wraps_and_accel_maps build_approval_options_shape`
Expected: PASS.

- [ ] **Step 8: Commit**
```bash
git add crates/atomcode-core/src/i18n/messages.rs crates/atomcode-core/src/i18n/en.rs crates/atomcode-core/src/i18n/zh_cn.rs crates/atomcode-tuix/src/state.rs crates/atomcode-tuix/src/event_loop/mod.rs
git commit -m "feat(tuix): approval-panel state types + options builder + i18n labels"
```

---

## Task 2: Footer render of the approval panel

**Files:**
- Modify: `crates/atomcode-tuix/src/render/retained.rs` — add `approval_panel_row_count` + `build_approval_rows`, slot into `paint_footer` + height math (mirror the existing todo-panel region)
- Test: `crates/atomcode-tuix/src/render/retained.rs` (vterm test)

**Interfaces:**
- Consumes: `UiState.approval_panel` (Task 1). The renderer reads it via `self.status` — SEE STEP 1: the panel must reach the renderer. `StatusLine` (in `render/mod.rs`) already carries footer data (`todo`, `goal`, …); add `pub approval: Option<crate::render::ApprovalPanelView>` OR pass the `state.approval_panel` through the same channel the todo panel uses. Follow the todo-panel wiring exactly: `build_status` (event_loop) copies `state.approval_panel` into the `StatusLine`, and `paint_footer` renders from `self.status.approval`.
- Produces: `fn build_approval_rows(&self, panel, rule_width) -> Vec<Vec<Cell>>`, `fn approval_panel_row_count(&self, panel) -> usize`.

- [ ] **Step 1: Thread the panel to the renderer.** The renderer reads footer data from `self.status: StatusLine`. Mirror the todo panel:
  - In `render/mod.rs`, define a view type the renderer consumes (avoid depending on `state` in render): 
    ```rust
    /// Renderer-facing snapshot of the approval panel (mirrors how `TodoProgress`
    /// feeds the todo panel). Header + option rows + selected index.
    #[derive(Debug, Clone)]
    pub struct ApprovalPanelView {
        pub tool: String,
        pub detail: String,
        /// (label, is_selected) per option, in display order.
        pub options: Vec<String>,
        pub selected: usize,
    }
    ```
    and add `pub approval: Option<ApprovalPanelView>` to `StatusLine`. Set all existing `StatusLine { … }` literals in tests to include `approval: None` (grep `todo: None` in retained.rs tests — add `approval: None` beside each).
  - In `event_loop/mod.rs` `build_status` (the fn that assembles `StatusLine`, where `let todo = state.active_todos…`), add:
    ```rust
    let approval = state.approval_panel.as_ref().map(|p| crate::render::ApprovalPanelView {
        tool: p.tool.clone(),
        detail: p.detail.clone(),
        options: p.options.iter().map(|o| o.label.clone()).collect(),
        selected: p.selected,
    });
    ```
    and add `approval,` to the `StatusLine { … }` it returns.

- [ ] **Step 2: Write the failing vterm test** — add to the retained test module (uses `new_capturing`/`drain_into_vterm`/`status_basic`):
```rust
    #[test]
    fn approval_panel_renders_selectable_options() {
        let (mut r, buf) = new_capturing(80, 24);
        r.caps.colors = true;
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        let mut status = status_basic();
        status.approval = Some(crate::render::ApprovalPanelView {
            tool: "Bash".into(),
            detail: "rm -rf build/".into(),
            options: vec!["Allow once".into(), "Always allow Bash".into(), "Deny".into()],
            selected: 0,
        });
        r.render(UiLine::InputPrompt {
            buf: String::new(), cursor_byte: 0, menu: None, status, attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        let dump = vterm.dump();
        // Header names the tool + shows the detail.
        assert!(vterm.any_row(|row| row.contains("Bash") && row.contains("rm -rf build/")) 
            || (vterm.any_row(|r| r.contains("Bash")) && vterm.any_row(|r| r.contains("rm -rf build/"))),
            "header + detail present\n{dump}");
        // All three options render.
        assert!(vterm.any_row(|r| r.contains("Allow once")), "allow once row\n{dump}");
        assert!(vterm.any_row(|r| r.contains("Always allow Bash")), "always row\n{dump}");
        assert!(vterm.any_row(|r| r.contains("Deny")), "deny row\n{dump}");
        // Selected row (index 0 = Allow once) carries the ▸ marker.
        assert!(vterm.any_row(|r| r.contains("▸") && r.contains("Allow once")), "selected marker on option 0\n{dump}");
        // Panel renders ABOVE the input box.
        let h = vterm.height() as usize;
        let row_of = |n: &str| (0..h).find(|&i| vterm.row_text(i).contains(n));
        assert!(row_of("Allow once") < row_of("❯").or(Some(h)), "panel above input\n{dump}");
    }
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p atomcode-tuix approval_panel_renders_selectable_options`
Expected: FAIL — no approval rendering yet (and `status.approval` field / view type wired in Step 1 must compile; if not, finish Step 1).

- [ ] **Step 4: Add the render helpers** — in `retained.rs`, near `build_todo_rows`/`todo_panel_row_count`, add:
```rust
    /// Rows the approval panel occupies (header + detail + one per option + hint),
    /// for the footer height math.
    fn approval_panel_row_count(&self, panel: &crate::render::ApprovalPanelView) -> usize {
        // header + detail + N options + hint
        2 + panel.options.len() + 1
    }

    /// Build the footer approval panel: a warning `⚠ <tool> …` header, the
    /// command detail, the selectable options (selected row = `▸ ` + reverse),
    /// and a hint line. Left `▌` accent bar per row (ASCII `|`). Colorless
    /// besides the warning header + reverse selection.
    fn build_approval_rows(
        &self,
        panel: &crate::render::ApprovalPanelView,
        rule_width: usize,
    ) -> Vec<Vec<Cell>> {
        let unicode = self.caps.unicode_symbols;
        let bar = if unicode { "\u{258c} " } else { "| " }; // ▌
        let bar_style = self.style_for(Role::Warning);
        let warn = if unicode { "\u{26a0} " } else { "! " }; // ⚠
        let mut out: Vec<Vec<Cell>> = Vec::new();

        // header row: `▌ ⚠ <tool> …`
        {
            let mut row = Vec::new();
            push_str_cells(&mut row, bar, &bar_style);
            push_str_cells(&mut row, warn, &self.style_for(Role::Warning));
            let head = crate::width::truncate_with_ellipsis(
                &scrub_controls(&panel.tool),
                rule_width.saturating_sub(6),
            );
            push_str_cells(&mut row, &head, &self.style_bold(Role::ToolName));
            out.push(row);
        }
        // detail row: `▌   <detail>`
        {
            let mut row = Vec::new();
            push_str_cells(&mut row, bar, &bar_style);
            let budget = rule_width.saturating_sub(4);
            let det = crate::width::truncate_with_ellipsis(&scrub_controls(&panel.detail), budget);
            push_str_cells(&mut row, &format!("  {det}"), &self.style_for(Role::Secondary));
            out.push(row);
        }
        // option rows: `▌  ▸ <label>` (selected: ▸ + reverse) / `▌    <label>`
        for (i, label) in panel.options.iter().enumerate() {
            let mut row = Vec::new();
            push_str_cells(&mut row, bar, &bar_style);
            let selected = i == panel.selected;
            let marker = if selected {
                if unicode { "  \u{25b8} " } else { "  > " } // ▸
            } else {
                "    "
            };
            let style = if selected {
                CellStyle { reverse: true, ..CellStyle::default() }
            } else {
                self.style_for(Role::Secondary)
            };
            let budget = rule_width.saturating_sub(6);
            let lbl = crate::width::truncate_with_ellipsis(&scrub_controls(label), budget);
            push_str_cells(&mut row, marker, &style);
            push_str_cells(&mut row, &lbl, &style);
            out.push(row);
        }
        // hint row: `▌  ↑↓ select · enter confirm · esc deny`
        {
            let mut row = Vec::new();
            push_str_cells(&mut row, bar, &bar_style);
            let hint = crate::i18n::t(crate::i18n::Msg::ApprovalHint).into_owned();
            let budget = rule_width.saturating_sub(4);
            let fitted = crate::width::truncate_with_ellipsis(&hint, budget);
            push_str_cells(&mut row, &format!("  {fitted}"), &self.style_for(Role::Muted));
            out.push(row);
        }
        out
    }
```

- [ ] **Step 5: Slot into `paint_footer`.** In `paint_footer`, mirror the todo panel region EXACTLY (the todo panel is drawn at the top of the footer with `todo_rows` reserved). Add an `approval_rows` reservation and draw the approval panel directly BELOW the todo panel and ABOVE the top rule:
  - Where `todo_rows` is computed, add:
    ```rust
    let approval_rows = self
        .status
        .approval
        .as_ref()
        .map(|p| self.approval_panel_row_count(p))
        .unwrap_or(0);
    ```
  - Include `approval_rows` in `total_rows` and in the `max_input_rows(... status_rows + goal_rows + todo_rows + approval_rows)` reservation (add `+ approval_rows`).
  - In the draw section: after the todo panel is drawn at `todo_top` (`= footer_top`), draw the approval panel starting at `footer_top + todo_rows`, then shift `rules_top` down by `approval_rows` too:
    ```rust
    let approval_top = footer_top + todo_rows;
    if let Some(p) = self.status.approval.clone() {
        for (i, ar) in self.build_approval_rows(&p, rule_width).into_iter().enumerate() {
            let mut padded = ar;
            Self::pad_row_to_width(&mut padded, w);
            self.screen.draw_row(approval_top + i, 0, &padded);
        }
    }
    let rules_top = footer_top + todo_rows + approval_rows;
    ```
    (Replace the existing `let rules_top = footer_top + todo_rows;` with the `+ approval_rows` version, and use `approval_top` for the panel draw. The cursor math already keys off `rules_top`.)

- [ ] **Step 6: Run test + build**

Run: `cargo test -p atomcode-tuix approval_panel_renders_selectable_options`
Expected: PASS.
Run: `cargo test -p atomcode-tuix --lib`
Expected: PASS except the ~4 pre-existing byte-budget reds (unchanged count). Fix any `StatusLine { … }` literal that fails to compile by adding `approval: None`.

- [ ] **Step 7: Commit**
```bash
git add crates/atomcode-tuix/src/render/mod.rs crates/atomcode-tuix/src/render/retained.rs crates/atomcode-tuix/src/event_loop/mod.rs
git commit -m "feat(tuix): render the footer approval panel (selectable options)"
```

---

## Task 3: Wire the request + input, drop the pop call

**Files:**
- Modify: `crates/atomcode-tuix/src/event_loop/mod.rs` (`ApprovalNeeded` handler + `handle_approval_key`)
- Modify: `crates/atomcode-tuix/src/event_loop/commands.rs` (`/bg` resume path ~2228)
- Modify: `crates/atomcode-tuix/src/state.rs` (clear `approval_panel` in turn-end + reset paths)

**Interfaces:**
- Consumes: `build_approval_options` (Task 1), `ApprovalPanel` (Task 1), `deliver_approval`/`AgentCommand` (existing).

- [ ] **Step 1: Set panel state in the `ApprovalNeeded` handler.** In `event_loop/mod.rs`, replace the `renderer.render(UiLine::ApprovalPrompt { tool: display.clone(), detail: detail.clone() });` line (in the `AgentEvent::ApprovalNeeded` arm, right before `renderer.flush();`) with:
```rust
            state.approval_panel = Some(crate::state::ApprovalPanel {
                tool: display.clone(),
                detail: detail.clone(),
                options: build_approval_options(&display),
                selected: 0,
            });
```
(Keep the `▸ Tool(detail)` body-row emit above it, the `renderer.flush()`, the `notify`, `state.on_approval_needed(&display)`, and the `redraw_idle_plain(...)` — the redraw now paints the footer approval panel.)

- [ ] **Step 2: Set panel state on `/bg` resume.** In `commands.rs`, replace the `renderer.render(UiLine::ApprovalPrompt { tool: tool_name, detail });` block (in the `find_pending_approval` branch ~2228) with:
```rust
                    if let Some((tool_name, detail)) = pending_approval {
                        state.approval_panel = Some(crate::state::ApprovalPanel {
                            options: crate::event_loop::build_approval_options(&tool_name),
                            selected: 0,
                            tool: tool_name,
                            detail,
                        });
                        state.on_approval_needed("");
                    }
```
(If `build_approval_options` is not `pub(crate)` reachable from `commands.rs`, make it `pub(crate)` — it already is per Task 1.)

- [ ] **Step 3: Write the failing input test** — add to the `bypass_approval_tests` (or a new `approval_key_tests`) module in `event_loop/mod.rs`. Since `handle_approval_key` needs a full `App`/`LoopCtx`, test the PURE selection→command mapping instead via a small helper. Add this helper next to `build_approval_options`:
```rust
/// The `AgentCommand` for a chosen approval option kind. Pure seam so the
/// key handler's decision mapping is unit-testable.
pub(crate) fn approval_kind_to_command(kind: crate::state::ApprovalKind) -> AgentCommand {
    use crate::state::ApprovalKind;
    match kind {
        ApprovalKind::AllowOnce => AgentCommand::ApproveTool,
        ApprovalKind::AlwaysAllow => AgentCommand::ApproveToolAlways,
        ApprovalKind::Deny => AgentCommand::DenyTool,
    }
}
```
Test:
```rust
    #[test]
    fn approval_kind_command_mapping() {
        use crate::state::ApprovalKind;
        assert!(matches!(super::approval_kind_to_command(ApprovalKind::AllowOnce), AgentCommand::ApproveTool));
        assert!(matches!(super::approval_kind_to_command(ApprovalKind::AlwaysAllow), AgentCommand::ApproveToolAlways));
        assert!(matches!(super::approval_kind_to_command(ApprovalKind::Deny), AgentCommand::DenyTool));
    }
```

- [ ] **Step 4: Run test to verify it fails**

Run: `cargo test -p atomcode-tuix approval_kind_command_mapping`
Expected: FAIL — `approval_kind_to_command` not found.

- [ ] **Step 5: Rework `handle_approval_key`.** Replace the body of `handle_approval_key` AFTER the Ctrl+C block (from `// Any other key resets…` through the final `Ok(())`) with:
```rust
    // Any other key resets the exit confirmation
    app.exit_pending = None;

    // Navigation: move the selection and repaint the footer, no decision yet.
    match code {
        KeyCode::Up => {
            if let Some(p) = app.state.approval_panel.as_mut() {
                p.move_up();
            }
            redraw_idle_plain(&app.buf, &mut app.state, ctx, renderer);
            return Ok(());
        }
        KeyCode::Down => {
            if let Some(p) = app.state.approval_panel.as_mut() {
                p.move_down();
            }
            redraw_idle_plain(&app.buf, &mut app.state, ctx, renderer);
            return Ok(());
        }
        _ => {}
    }

    // Resolve to a decision: Enter = the selected option; y/a/n = accelerators;
    // Esc = Deny (safe default). Any other key is ignored.
    let kind = match code {
        KeyCode::Enter => app
            .state
            .approval_panel
            .as_ref()
            .and_then(|p| p.options.get(p.selected).map(|o| o.kind)),
        KeyCode::Esc => Some(crate::state::ApprovalKind::Deny),
        KeyCode::Char(c) => app
            .state
            .approval_panel
            .as_ref()
            .and_then(|p| p.accel_index(c).and_then(|i| p.options.get(i).map(|o| o.kind))),
        _ => None,
    };
    let Some(kind) = kind else {
        return Ok(());
    };
    let cmd = approval_kind_to_command(kind);
    deliver_approval(ctx, cmd);
    app.state.on_approval_resolved(); // clears approval_panel + phase → Streaming
    Ok(())
```
Also in the SAME function's Ctrl+C block, DELETE the line `renderer.pop_approval_prompt();` (the panel is now cleared by `on_approval_resolved()` which the Ctrl+C branch already calls). The `renderer` param may become unused in the non-Ctrl+C path — it is still used for the Up/Down redraw and the Ctrl+C `CommandOutput`, so keep it.

- [ ] **Step 6: Clear `approval_panel` on turn-end + reset.** In `state.rs`, add `self.approval_panel = None;` to `on_turn_cancelled` and `on_error` (next to the existing `self.active_todos`… no — approval_panel is separate; add it near where those fns clear other transient state). Do NOT clear it in `on_turn_complete` (a completed turn shouldn't happen mid-approval, but clearing there is harmless — add it too for safety). In `event_loop/commands.rs::reset_to_new_session`, add `state.approval_panel = None;` next to `state.active_todos = None;`. In the `SessionSwitched` handler (`event_loop/mod.rs`, where `state.active_todos = None;` was added), add `state.approval_panel = None;` beside it.

- [ ] **Step 7: Run tests + build**

Run: `cargo test -p atomcode-tuix approval_kind_command_mapping`
Expected: PASS.
Run: `cargo build -p atomcode-tuix`
Expected: clean. (There may be an unused-warning for `pop_approval_prompt` now that its callers in `handle_approval_key` are gone — that is addressed in Task 4. If the build is warning-as-error, note it and continue; Task 4 removes the fn.)
Run: `cargo test -p atomcode-tuix --lib`
Expected: PASS except the ~4 pre-existing byte-budget reds.

- [ ] **Step 8: Commit**
```bash
git add crates/atomcode-tuix/src/event_loop/mod.rs crates/atomcode-tuix/src/event_loop/commands.rs crates/atomcode-tuix/src/state.rs
git commit -m "feat(tuix): drive approval from the footer panel (arrows/enter/esc/yan)"
```

---

## Task 4: Remove the dead ApprovalPrompt / pop machinery

**Files:**
- Modify: `crates/atomcode-tuix/src/render/retained.rs` (remove `UiLine::ApprovalPrompt` arm, `pop_approval_prompt`, `approval_block_rows` field + its resize logic)
- Modify: `crates/atomcode-tuix/src/render/plain.rs` (remove `UiLine::ApprovalPrompt` arm)
- Modify: `crates/atomcode-tuix/src/render/mod.rs` (remove `UiLine::ApprovalPrompt` variant + the `pop_approval_prompt` trait method)
- Modify: `crates/atomcode-tuix/src/render/worker.rs` (remove `RenderCmd::PopApprovalPrompt` + its handler + the `pop_approval_prompt` impl + the name-map arm)

**Interfaces:** none produced (pure removal). After Task 3 nothing EMITS `UiLine::ApprovalPrompt` and nothing CALLS `pop_approval_prompt` — confirm before deleting.

- [ ] **Step 1: Confirm no live emitters / callers remain**

Run: `grep -rn "UiLine::ApprovalPrompt\|pop_approval_prompt\|PopApprovalPrompt\|approval_block_rows" crates/atomcode-tuix/src`
Expected: matches are ONLY definitions/render-arms/worker-plumbing (no `renderer.render(UiLine::ApprovalPrompt` emit outside a match arm, no `renderer.pop_approval_prompt()` call). If a live emit/call remains, STOP — Task 3 is incomplete.

- [ ] **Step 2: Remove the variant + trait method** (`render/mod.rs`): delete the `ApprovalPrompt { tool, detail }` variant from the `UiLine` enum and the `fn pop_approval_prompt(&mut self)` from the `Renderer` trait (and its doc). 

- [ ] **Step 3: Remove the retained implementation** (`retained.rs`): delete the `UiLine::ApprovalPrompt { tool, detail } => { … }` render arm; delete `fn pop_approval_prompt`; delete the `approval_block_rows` field, its initializer, and the resize logic that consumes it (the `Some(0)` consumed-noop in `reflow_body_to_current_width` + any `approval_block_rows` set/read). Remove `UiLine::ApprovalPrompt` from the transient-variant match (~3661) and the `impl Renderer` `pop_approval_prompt` method.

- [ ] **Step 4: Remove the plain + worker plumbing** (`plain.rs` + `worker.rs`): delete the `UiLine::ApprovalPrompt` arm in `plain.rs`; in `worker.rs` delete `RenderCmd::PopApprovalPrompt`, its send/handler, the `pop_approval_prompt` forwarding impl, and the `UiLine::ApprovalPrompt { .. } => "ApprovalPrompt"` name-map arm.

- [ ] **Step 5: Delete the now-dead tests** — any test that asserted the old body `ApprovalPrompt` render or `pop_approval_prompt` behavior (grep in Step 1 will have surfaced them; e.g. the approval-pop / blank-gap / resize-count tests). Delete or rewrite them; the new behavior is covered by Task 2/3 tests. Keep the Ctrl+C-deny behavior test if it doesn't depend on `pop_approval_prompt`.

- [ ] **Step 6: Build + test**

Run: `cargo build -p atomcode-tuix`
Expected: clean, no `unused` warnings for the removed items.
Run: `grep -rn "ApprovalPrompt\|pop_approval_prompt\|PopApprovalPrompt\|approval_block_rows" crates/atomcode-tuix/src`
Expected: only the `highlight/theme.rs:51` comment (a passing mention) may remain — update or leave it; no code references.
Run: `cargo test -p atomcode-tuix --lib`
Expected: PASS except the ~4 pre-existing byte-budget reds.

- [ ] **Step 7: Commit**
```bash
git add crates/atomcode-tuix/src/render/mod.rs crates/atomcode-tuix/src/render/retained.rs crates/atomcode-tuix/src/render/plain.rs crates/atomcode-tuix/src/render/worker.rs
git commit -m "refactor(tuix): remove the dead body-ApprovalPrompt + pop machinery"
```

---

## Task 5: Verification

- [ ] **Step 1: Whole-workspace build + touched-crate tests**

Run: `touch crates/atomcode-core/src/lib.rs && cargo build`
Expected: clean.
Run: `cargo test -p atomcode-core -p atomcode-tuix`
Expected: green except the ~4 pre-existing tuix byte-budget reds (same count as a clean checkout).

- [ ] **Step 2: Manual smoke (documented, real terminal only)** — record that these need a real terminal:
  1. Trigger a tool that needs approval (e.g. a bash command) → a footer panel appears above the input: `⚠ Bash` / the command / `▸ Allow once` / `Always allow bash` / `Deny` / hint.
  2. `↑/↓` move the `▸` + reverse highlight; `Enter` on "Allow once" runs the tool; `Enter` on "Deny" denies.
  3. `y` / `a` / `n` still work as direct accelerators.
  4. `Esc` denies. `Ctrl+C` denies + arms exit.
  5. After a decision the panel vanishes; the `▸ Bash(…)` row stays in scrollback; the input box is usable.
  6. `/bg` a session waiting on approval, resume it → the panel reappears.
  7. Non-unicode terminal → `|` bar, `>` marker, `!` warning; no `▸`/`▌`/`⚠`.

- [ ] **Step 3: Request review** — `/code-review` on the branch diff before merge.

---

## Self-Review (completed during authoring)

- **Spec coverage:** vertical selectable list → Task 2 render + Task 3 input. footer-pinned → Task 2 paint_footer slot. keyboard-only (↑↓/enter/esc/y-a-n, default 0) → Task 3. 3 options + tool-name Always label → Task 1 `build_approval_options`. reverse-selected + `▌`/`▸`/`⚠` + ASCII fallback → Task 2 `build_approval_rows`. i18n labels → Task 1. no-bg / no-hardcoded-color → Task 2 (Role + reverse). decision→AgentCommand unchanged plumbing → Task 3. drop pop_approval_prompt → Task 4. clear on resolve/turn-end/reset → Task 1 + Task 3. plain fallback → kept (Task 4 removes only the body-ApprovalPrompt arm; the panel is retained-only, plain shows the `▸ Tool` row + result as before — NOTE: v1 plain has no interactive approval; the daemon/pipe path uses sync approval, unaffected).
- **Placeholder scan:** none — full code for every NEW piece. The paint_footer slot (Task 2 Step 5) references the in-repo todo-panel region as the pattern to mirror (a real, readable pattern, not a placeholder) and gives the exact `approval_rows`/`approval_top`/`rules_top` edits.
- **Type consistency:** `ApprovalKind`/`ApprovalOption`/`ApprovalPanel` (Task 1) used in Task 2 (via `ApprovalPanelView`) and Task 3 (`approval_kind_to_command`, `accel_index`, `move_up/down`). `ApprovalPanelView` (Task 2, render/mod.rs) consumed only by the renderer. `build_approval_options`/`approval_kind_to_command` signatures consistent between Task 1/3 definition and call sites.
- **Plan-phase items resolved:** V1 (no scope in event) → tool-name Always label. V2 (ApprovalPrompt entangled) → dedicated Task 4 removal with a pre-check. V3 (footer slot) → Task 2 Step 5 (below todo panel, above top rule).
