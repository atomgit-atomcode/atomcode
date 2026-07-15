# GitHub-style Diff (line numbers + color) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Render edit/write diffs in a GitHub-ish style — a real line diff with a right-aligned line-number gutter, `+`/`-`/context signs, and foreground green/red/muted coloring.

**Architecture:** Replace the naive `build_compact_diff` (first-4-old / first-4-new, no real matching) in `atomcode-capabilities` with a real unified diff computed by the `similar` crate over the WHOLE old vs new file (context radius 3, capped) — this yields correct file line numbers + hunks for free, exactly as codex does. The diff still travels to the TUI as the tool-result string (no new event plumbing); the TUI re-parses the unified diff into line-numbered, color-coded rows. Foreground-only color (the cell model has no background), no syntax highlighting.

**Tech Stack:** Rust, `similar` crate (line diff, same crate codex uses), `atomcode-capabilities` (diff compute), `atomcode-tuix` (parse + render).

## Global Constraints

- Foreground color ONLY. The `CellStyle` has no background (`fg`/`bold`/`reverse`/`faint`); do NOT add background shading. Color via existing `Role::DiffAdd` (green) / `Role::DiffRemove` (red) / `Role::Muted` (context), which are theme-aware.
- NO syntax highlighting. The repo deliberately removed syntect from the TUI (macOS Terminal selection-overlay bug); coloring is line-level only.
- `similar` is added as an OPTIONAL dependency, gated under the existing `tools` feature of `atomcode-capabilities` (the same feature that gates `edit_file`).
- Scope is the `atomcode-capabilities` diff (feeds the default v2 engine → TUI). The parallel `atomcode-core/src/tool/edit.rs::build_compact_diff` (v1/legacy, being retired on this branch) is OUT of scope.
- COMMIT DISCIPLINE: stage ONLY the files each task changes with `git add <path>`; never `-A`/`.`/`-u`.
- Known: ~4 pre-existing "byte budget" retained tests fail in atomcode-tuix — unrelated; confirm the count does not increase. After editing a lower crate, `touch crates/atomcode-core/src/lib.rs` before running tuix tests if you hit stale artifacts.

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `Cargo.toml` (workspace) | dep versions | add `similar = "2"` to `[workspace.dependencies]` |
| `crates/atomcode-capabilities/Cargo.toml` | crate deps | add optional `similar`, put in `tools` feature |
| `crates/atomcode-capabilities/src/tools/edit.rs` | diff compute | `build_compact_diff` → unified diff over whole files; 2 call sites; tests |
| `crates/atomcode-tuix/src/render/mod.rs` | diff data type | `DiffEntry` → `{ kind, old_lineno, new_lineno, text }` + `DiffKind` enum |
| `crates/atomcode-tuix/src/render/diff.rs` (NEW) | pure diff logic | `parse_unified_diff`, `diff_gutter_width`, `diff_row_text` (+ tests) |
| `crates/atomcode-tuix/src/render/mod.rs` | module wiring | `pub(crate) mod diff;` |
| `crates/atomcode-tuix/src/event_loop/mod.rs` | wire parser | replace the `strip_prefix` parser with `parse_unified_diff` |
| `crates/atomcode-tuix/src/render/retained.rs` | interactive render | draw the gutter + sign, color by kind |
| `crates/atomcode-tuix/src/render/plain.rs` | pipe render | same, with SGR |

---

## Task 1: Real unified diff in capabilities

**Files:**
- Modify: `Cargo.toml` (workspace `[workspace.dependencies]`, ~line 38)
- Modify: `crates/atomcode-capabilities/Cargo.toml` (`[dependencies]` + `tools` feature)
- Modify: `crates/atomcode-capabilities/src/tools/edit.rs:177-204` (`build_compact_diff`), call sites `:123` and `:167`, tests `:356-357` and `:362-371`

**Interfaces:**
- Produces: `fn build_compact_diff(old_file: &str, new_file: &str) -> String` — a git unified diff (`@@ -a,b +c,d @@` hunks, 3 lines context, capped to 60 lines). Callers pass the WHOLE pre-edit and post-edit file contents.

- [ ] **Step 1: Add the `similar` dependency**

In the workspace `Cargo.toml`, under `[workspace.dependencies]` (alphabetical, after `anyhow = "1"`), add:
```toml
similar = "2"
```
In `crates/atomcode-capabilities/Cargo.toml`, in `[dependencies]` add (near the other optional tools deps like `regex`):
```toml
# Real line diff for edit_file's compact diff (git-style unified hunks + line numbers).
similar = { workspace = true, optional = true }
```
and add `"dep:similar"` to the `tools` feature list:
```toml
tools = ["dep:ignore", "dep:regex", "dep:grep", "dep:globset", "dep:encoding_rs", "dep:tokio-util", "dep:similar", "tokio/fs", "tokio/process", "tokio/io-util"]
```

- [ ] **Step 2: Write the failing test**

In `edit.rs`, REPLACE the existing `compact_diff_truncates_each_side` test (lines 362-371) with:
```rust
    #[test]
    fn compact_diff_is_unified_with_line_numbers() {
        // Whole-file old vs new; a real diff must produce a `@@` hunk header whose
        // new-side start reflects the changed line's position in the file.
        let old = "fn main() {\n    let x = 1;\n}\n";
        let new = "fn main() {\n    let x = 2;\n}\n";
        let diff = build_compact_diff(old, new);
        assert!(diff.contains("@@"), "must be a unified diff with a hunk header: {diff}");
        assert!(diff.contains("-    let x = 1;"), "removed line present: {diff}");
        assert!(diff.contains("+    let x = 2;"), "added line present: {diff}");
        // The change is on file line 2, so the hunk header covers line 2 on both sides.
        assert!(diff.contains("-2") && diff.contains("+2"), "hunk covers line 2: {diff}");
    }

    #[test]
    fn compact_diff_caps_huge_diffs() {
        let old = String::new();
        let new: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let diff = build_compact_diff(&old, &new);
        assert!(diff.lines().count() <= 61, "capped: {} lines", diff.lines().count());
        assert!(diff.contains("more diff lines"), "shows a truncation note: {diff}");
    }
```
Also UPDATE the assertions in the existing `unique_replace_succeeds` test (lines 356-357) from:
```rust
        assert!(r.content.contains("- let x = 1;"), "{}", r.content);
        assert!(r.content.contains("+ let x = 2;"), "{}", r.content);
```
to (the new format has no space after the sign, and the surrounding `fn main` lines are unchanged file context):
```rust
        assert!(r.content.contains("-    let x = 1;"), "{}", r.content);
        assert!(r.content.contains("+    let x = 2;"), "{}", r.content);
```
(Note: `unique_replace_succeeds` writes the file `"fn main() {\n    let x = 1;\n}\n"` and edits `let x = 1;`→`let x = 2;`, so the diff is over the whole file and the changed line keeps its 4-space indent.)

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p atomcode-capabilities compact_diff_is_unified_with_line_numbers`
Expected: FAIL — the old `build_compact_diff` produces `- 1`-style output with no `@@`.

- [ ] **Step 4: Rewrite `build_compact_diff`**

Replace `build_compact_diff` (lines 177-204) with:
```rust
/// A compact GIT UNIFIED DIFF (`@@` hunks, 3 lines of context) between the OLD
/// and NEW whole-file contents, capped so a large edit can't flood the model
/// context / transcript. The TUI re-parses this into a line-numbered, color-
/// coded diff block; the model reads it as a normal unified diff.
fn build_compact_diff(old_file: &str, new_file: &str) -> String {
    const MAX_DIFF_LINES: usize = 60;
    let full = similar::TextDiff::from_lines(old_file, new_file)
        .unified_diff()
        .context_radius(3)
        .to_string();
    let full = full.trim_end();
    let lines: Vec<&str> = full.lines().collect();
    if lines.len() <= MAX_DIFF_LINES {
        return full.to_string();
    }
    let mut out = lines[..MAX_DIFF_LINES].join("\n");
    out.push_str(&format!(
        "\n… ({} more diff lines)",
        lines.len() - MAX_DIFF_LINES
    ));
    out
}
```

- [ ] **Step 5: Update the two call sites to pass whole files**

At `edit.rs:123` (fuzzy path — `content` is the old file, `fuzzy_result` the new), change:
```rust
                let diff = build_compact_diff(&a.old_string, &a.new_string);
```
to:
```rust
                let diff = build_compact_diff(&content, &fuzzy_result);
```
At `edit.rs:167` (normal path — `content` old, `updated` new), change:
```rust
        let diff = build_compact_diff(&a.old_string, &a.new_string);
```
to:
```rust
        let diff = build_compact_diff(&content, &updated);
```

- [ ] **Step 6: Run tests + build**

Run: `cargo test -p atomcode-capabilities compact_diff unique_replace_succeeds`
Expected: PASS (both new diff tests + the updated edit test).
Run: `cargo build -p atomcode-capabilities`
Expected: clean.

- [ ] **Step 7: Commit**
```bash
git add Cargo.toml crates/atomcode-capabilities/Cargo.toml crates/atomcode-capabilities/src/tools/edit.rs Cargo.lock
git commit -m "feat(capabilities): compute edit diffs as real unified diffs (similar)"
```

---

## Task 2: TUI diff types + pure parse/format logic

**Files:**
- Modify: `crates/atomcode-tuix/src/render/mod.rs:560-565` (`DiffEntry`) + add `pub(crate) mod diff;`
- Create: `crates/atomcode-tuix/src/render/diff.rs`

**Interfaces:**
- Produces:
  - `pub enum DiffKind { Add, Del, Context }`
  - `pub struct DiffEntry { pub kind: DiffKind, pub old_lineno: Option<usize>, pub new_lineno: Option<usize>, pub text: String }`
  - `pub(crate) fn parse_unified_diff(diff: &str, max_lines: usize) -> Vec<DiffEntry>`
  - `pub(crate) fn diff_gutter_width(entries: &[DiffEntry]) -> usize`
  - `pub(crate) fn diff_row_text(entry: &DiffEntry, gutter: usize) -> String` — `"  {num:>gutter} {sign} {text}"`

- [ ] **Step 1: Replace the `DiffEntry` type**

In `render/mod.rs`, replace (lines 560-565):
```rust
/// One line in a diff batch. `added = true` renders as `+`, false as `-`.
#[derive(Debug, Clone)]
pub struct DiffEntry {
    pub added: bool,
    pub text: String,
}
```
with:
```rust
/// The role of a diff line: an addition (`+`), a deletion (`-`), or unchanged
/// context (` `). Drives the sign + color in the renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    Add,
    Del,
    Context,
}

/// One line of a rendered diff, with the file line number for its side.
/// `old_lineno` is set for Del + Context, `new_lineno` for Add + Context.
#[derive(Debug, Clone)]
pub struct DiffEntry {
    pub kind: DiffKind,
    pub old_lineno: Option<usize>,
    pub new_lineno: Option<usize>,
    pub text: String,
}
```
Then add the module declaration near the other `mod` lines at the top of `render/mod.rs` (search for `mod retained;` / `mod plain;` and add alongside):
```rust
pub(crate) mod diff;
```

- [ ] **Step 2: Write the failing tests** — create `crates/atomcode-tuix/src/render/diff.rs` with ONLY the test module first (so it fails to compile against missing fns):
```rust
//! Pure diff-parsing and row-formatting logic for `UiLine::DiffBlock`.
//! Rendering (cells/SGR) lives in retained.rs / plain.rs; this module only
//! turns a unified-diff string into line-numbered entries and formats a row.

use crate::render::{DiffEntry, DiffKind};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hunk_line_numbers_and_kinds() {
        let diff = "\
@@ -1,3 +1,3 @@
 fn main() {
-    let x = 1;
+    let x = 2;
 }";
        let e = parse_unified_diff(diff, 100);
        assert_eq!(e.len(), 4);
        assert_eq!(e[0].kind, DiffKind::Context);
        assert_eq!((e[0].old_lineno, e[0].new_lineno), (Some(1), Some(1)));
        assert_eq!(e[1].kind, DiffKind::Del);
        assert_eq!((e[1].old_lineno, e[1].new_lineno), (Some(2), None));
        assert_eq!(e[1].text, "    let x = 1;");
        assert_eq!(e[2].kind, DiffKind::Add);
        assert_eq!((e[2].old_lineno, e[2].new_lineno), (None, Some(2)));
        assert_eq!(e[3].kind, DiffKind::Context);
        assert_eq!((e[3].old_lineno, e[3].new_lineno), (Some(3), Some(3)));
    }

    #[test]
    fn ignores_preamble_and_file_headers() {
        let diff = "\
Edited a.rs (1 replacement)
--- a/a.rs
+++ b/a.rs
@@ -2,1 +2,1 @@
-old
+new";
        let e = parse_unified_diff(diff, 100);
        // The `Edited …`, `--- a/a.rs`, `+++ b/a.rs` lines must NOT become entries.
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].kind, DiffKind::Del);
        assert_eq!(e[0].text, "old");
        assert_eq!(e[1].kind, DiffKind::Add);
        assert_eq!(e[1].text, "new");
    }

    #[test]
    fn respects_max_lines() {
        let mut diff = String::from("@@ -1,0 +1,50 @@\n");
        for i in 0..50 {
            diff.push_str(&format!("+line {i}\n"));
        }
        let e = parse_unified_diff(&diff, 10);
        assert_eq!(e.len(), 10);
    }

    #[test]
    fn gutter_width_and_row_format() {
        let entries = vec![
            DiffEntry { kind: DiffKind::Context, old_lineno: Some(9), new_lineno: Some(9), text: "ctx".into() },
            DiffEntry { kind: DiffKind::Add, old_lineno: None, new_lineno: Some(10), text: "added".into() },
            DiffEntry { kind: DiffKind::Del, old_lineno: Some(10), new_lineno: None, text: "removed".into() },
        ];
        let w = diff_gutter_width(&entries);
        assert_eq!(w, 2); // largest line number is 10 → width 2
        assert_eq!(diff_row_text(&entries[0], w), "   9   ctx");
        assert_eq!(diff_row_text(&entries[1], w), "  10 + added");
        assert_eq!(diff_row_text(&entries[2], w), "  10 - removed");
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p atomcode-tuix --lib render::diff`
Expected: FAIL to compile — `parse_unified_diff` / `diff_gutter_width` / `diff_row_text` not found.

- [ ] **Step 4: Implement the pure functions** — add ABOVE the `#[cfg(test)] mod tests` in `diff.rs`:
```rust
/// Parse a git unified diff (`@@ -a,b +c,d @@` hunks + ` `/`+`/`-` lines) into
/// line-numbered entries. Lines before the first `@@`, and `---`/`+++` file
/// headers, are ignored. Stops after `max_lines` entries.
pub(crate) fn parse_unified_diff(diff: &str, max_lines: usize) -> Vec<DiffEntry> {
    let mut out: Vec<DiffEntry> = Vec::new();
    let mut old_ln = 0usize;
    let mut new_ln = 0usize;
    for line in diff.lines() {
        if out.len() >= max_lines {
            break;
        }
        if let Some(rest) = line.strip_prefix("@@") {
            if let Some((o, n)) = parse_hunk_header(rest) {
                old_ln = o;
                new_ln = n;
            }
            continue;
        }
        if line.starts_with("---") || line.starts_with("+++") {
            continue; // unified-diff file headers
        }
        if old_ln == 0 && new_ln == 0 {
            continue; // preamble before the first hunk
        }
        match line.as_bytes().first() {
            Some(b'+') => {
                out.push(DiffEntry {
                    kind: DiffKind::Add,
                    old_lineno: None,
                    new_lineno: Some(new_ln),
                    text: line[1..].to_string(),
                });
                new_ln += 1;
            }
            Some(b'-') => {
                out.push(DiffEntry {
                    kind: DiffKind::Del,
                    old_lineno: Some(old_ln),
                    new_lineno: None,
                    text: line[1..].to_string(),
                });
                old_ln += 1;
            }
            Some(b' ') => {
                out.push(DiffEntry {
                    kind: DiffKind::Context,
                    old_lineno: Some(old_ln),
                    new_lineno: Some(new_ln),
                    text: line[1..].to_string(),
                });
                old_ln += 1;
                new_ln += 1;
            }
            _ => {} // `\ No newline at end of file`, blank lines, etc.
        }
    }
    out
}

/// Parse the two 1-based start line numbers from a hunk header body
/// (`rest` = the text after `@@`, e.g. ` -12,3 +14,4 @@ …`). Returns
/// `(old_start, new_start)`.
fn parse_hunk_header(rest: &str) -> Option<(usize, usize)> {
    let mut old_start = None;
    let mut new_start = None;
    for tok in rest.split_whitespace() {
        if let Some(o) = tok.strip_prefix('-') {
            old_start = o.split(',').next().and_then(|s| s.parse::<usize>().ok());
        } else if let Some(n) = tok.strip_prefix('+') {
            new_start = n.split(',').next().and_then(|s| s.parse::<usize>().ok());
        }
    }
    Some((old_start?, new_start?))
}

/// Width of the line-number gutter: the digit count of the largest line number
/// shown across `entries` (Del shows old, others show new), minimum 1.
pub(crate) fn diff_gutter_width(entries: &[DiffEntry]) -> usize {
    entries
        .iter()
        .filter_map(|e| match e.kind {
            DiffKind::Del => e.old_lineno,
            _ => e.new_lineno,
        })
        .max()
        .unwrap_or(1)
        .to_string()
        .len()
        .max(1)
}

/// Format one diff row as `"  {num:>gutter} {sign} {text}"` — the display line
/// (WITHOUT color; the caller applies the theme role). `text` is control-scrubbed.
pub(crate) fn diff_row_text(entry: &DiffEntry, gutter: usize) -> String {
    let num = match entry.kind {
        DiffKind::Del => entry.old_lineno,
        _ => entry.new_lineno,
    };
    let numstr = num.map(|n| n.to_string()).unwrap_or_default();
    let sign = match entry.kind {
        DiffKind::Add => '+',
        DiffKind::Del => '-',
        DiffKind::Context => ' ',
    };
    format!(
        "  {numstr:>gutter$} {sign} {}",
        crate::render::scrub_controls_pub(&entry.text)
    )
}
```
Note on `scrub_controls`: if `scrub_controls` is a private helper not reachable from `render/diff.rs`, either (a) make a small `pub(crate) fn scrub_controls_pub` re-export in the module where `scrub_controls` lives, or (b) inline the existing `scrub_controls` call at the two RENDER sites (retained/plain) instead of inside `diff_row_text` and drop the scrub here. Prefer (b): remove the `crate::render::scrub_controls_pub(&entry.text)` wrap here (use `&entry.text` raw) and keep the existing `scrub_controls(&…)` in retained.rs/plain.rs. Update the Step-2 test's expected strings accordingly (they use plain ASCII, so no change needed). Confirm which by grepping `fn scrub_controls` before implementing.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p atomcode-tuix --lib render::diff`
Expected: PASS (all 4).

- [ ] **Step 6: Commit**
```bash
git add crates/atomcode-tuix/src/render/mod.rs crates/atomcode-tuix/src/render/diff.rs
git commit -m "feat(tuix): line-numbered DiffEntry + pure unified-diff parser/formatter"
```

---

## Task 3: Wire the parser + render the gutter

**Files:**
- Modify: `crates/atomcode-tuix/src/event_loop/mod.rs:9017-9040` (the diff parser call)
- Modify: `crates/atomcode-tuix/src/render/retained.rs:4448-4459` (`UiLine::DiffBlock` arm)
- Modify: `crates/atomcode-tuix/src/render/plain.rs:287-306` (`UiLine::DiffBlock` arm)

**Interfaces:**
- Consumes: `parse_unified_diff`, `diff_gutter_width`, `diff_row_text`, `DiffKind` (Task 2).

- [ ] **Step 1: Write the failing test** — add to the retained test module (uses `new_capturing`/`drain_into_vterm`, same module as the other vterm tests):
```rust
    #[test]
    fn diff_block_renders_line_number_gutter() {
        use crate::render::{DiffEntry, DiffKind};
        let (mut r, buf) = new_capturing(80, 24);
        r.caps.colors = true;
        let mut vterm = crate::test_term::VirtualTerminal::new(80, 24);
        r.render(UiLine::DiffBlock(vec![
            DiffEntry { kind: DiffKind::Context, old_lineno: Some(9), new_lineno: Some(9), text: "keep".into() },
            DiffEntry { kind: DiffKind::Del, old_lineno: Some(10), new_lineno: None, text: "old line".into() },
            DiffEntry { kind: DiffKind::Add, old_lineno: None, new_lineno: Some(10), text: "new line".into() },
        ]));
        r.render(UiLine::InputPrompt {
            buf: String::new(), cursor_byte: 0, menu: None,
            status: status_basic(), attachments: Vec::new(),
        });
        r.flush_deferred();
        drain_into_vterm(&buf, &mut vterm);
        // Gutter shows the line number, then the sign, then content.
        assert!(vterm.any_row(|row| row.contains("10 - old line")), "removed row w/ gutter\n{}", vterm.dump());
        assert!(vterm.any_row(|row| row.contains("10 + new line")), "added row w/ gutter\n{}", vterm.dump());
        assert!(vterm.any_row(|row| row.contains("9   keep")), "context row w/ gutter\n{}", vterm.dump());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p atomcode-tuix diff_block_renders_line_number_gutter`
Expected: FAIL — the current renderer emits `       - old line` (7 spaces, no gutter) and won't compile against the new `DiffEntry` fields anyway.

- [ ] **Step 3: Rewrite the event_loop parser**

Replace `event_loop/mod.rs:9017-9040` (the `if emits_diff { … }` block that builds `diff_entries` via `strip_prefix`) with:
```rust
            if emits_diff {
                let diff_entries = crate::render::diff::parse_unified_diff(&output, 120);
                if !diff_entries.is_empty() {
                    renderer.render(UiLine::DiffBlock(diff_entries));
                }
            }
```

- [ ] **Step 4: Rewrite the retained renderer**

Replace `retained.rs:4448-4459` (`UiLine::DiffBlock(entries) => { … }`) with:
```rust
            UiLine::DiffBlock(entries) => {
                let gutter = crate::render::diff::diff_gutter_width(&entries);
                for entry in &entries {
                    let role = match entry.kind {
                        crate::render::DiffKind::Add => Role::DiffAdd,
                        crate::render::DiffKind::Del => Role::DiffRemove,
                        crate::render::DiffKind::Context => Role::Muted,
                    };
                    let style = self.style_for(role);
                    let body = crate::render::diff::diff_row_text(entry, gutter);
                    self.push_body_text(&scrub_controls(&body), &style);
                }
            }
```
(`diff_row_text` returns the raw text per Task 2's note (b); `scrub_controls` is applied here at the render site, matching the old behavior.)

- [ ] **Step 5: Rewrite the plain renderer**

Replace `plain.rs:287-306` (`UiLine::DiffBlock(entries) => { … }`) with:
```rust
            UiLine::DiffBlock(entries) => {
                self.drop_transient();
                let gutter = crate::render::diff::diff_gutter_width(&entries);
                for entry in &entries {
                    let color = if self.caps.colors {
                        match entry.kind {
                            crate::render::DiffKind::Add => SGR_GREEN,
                            crate::render::DiffKind::Del => SGR_RED,
                            crate::render::DiffKind::Context => "",
                        }
                    } else {
                        ""
                    };
                    let reset = if self.caps.colors && !color.is_empty() { SGR_RESET } else { "" };
                    let body = crate::render::diff::diff_row_text(entry, gutter);
                    let _ = writeln!(self.out, "{}{}{}", color, scrub_controls(&body), reset);
                }
            }
```

- [ ] **Step 6: Run tests + build**

Run: `cargo build -p atomcode-tuix` — clean (the `DiffEntry.added` field is gone; the compiler confirms all users updated). If `UiLine::DiffLine { added, text }` (a separate single-line variant at `render/mod.rs:116`) still compiles — it uses its OWN `added`/`text`, not `DiffEntry`, so it is unaffected; leave it.
Run: `cargo test -p atomcode-tuix diff_block_renders_line_number_gutter`
Expected: PASS.
Run: `cargo test -p atomcode-tuix --lib`
Expected: PASS except the ~4 pre-existing byte-budget reds (unchanged count).

- [ ] **Step 7: Commit**
```bash
git add crates/atomcode-tuix/src/event_loop/mod.rs crates/atomcode-tuix/src/render/retained.rs crates/atomcode-tuix/src/render/plain.rs
git commit -m "feat(tuix): render diffs with a line-number gutter + kind coloring"
```

---

## Task 4: Verification

- [ ] **Step 1: Whole-workspace build + touched-crate tests**

Run: `touch crates/atomcode-core/src/lib.rs && cargo build`
Expected: clean.
Run: `cargo test -p atomcode-capabilities -p atomcode-tuix`
Expected: green except the ~4 pre-existing tuix byte-budget reds (same count as a clean checkout).

- [ ] **Step 2: Manual smoke (documented, real terminal only)**

Record in the PR/commit that these need a real terminal:
1. `edit_file` a real source file → the diff shows a right-aligned line-number gutter, `+`/`-`/context signs, green/red/muted lines with CORRECT file line numbers.
2. A multi-hunk edit (`replace_all`) → multiple hunks, each with its own line numbers.
3. A huge rewrite → capped diff with `… (N more diff lines)`.
4. Non-color terminal (`caps.colors=false`) → gutter + signs still present, no color.
5. `/resume` a session that had an edit → the diff re-renders from the stored tool result (same parser path).

- [ ] **Step 3: Request review** — `/code-review` on the branch diff before merge.

---

## Self-Review (completed during authoring)

- **Coverage:** real diff + line numbers → Task 1 (similar unified diff) + Task 2 (parser assigns file line numbers from hunk headers). Color distinction → Task 3 (DiffAdd/DiffRemove/Muted, fg-only). Gutter render → Task 3. Cap → Task 1 + parser `max_lines`. fg-only / no-syntect constraints honored (no bg, no highlighter). `similar` gating → Task 1 Step 1.
- **Placeholders:** none — every code step is complete. The one flagged lookup (`scrub_controls` visibility, Task 2 Step 4) resolves to option (b): scrub at the render site (retained/plain), not inside `diff_row_text` — the render steps (Task 3) already wrap with `scrub_controls`, and `diff_row_text` returns raw text. Ensure `diff_row_text` uses `&entry.text` (not a scrub wrapper) when implementing.
- **Type consistency:** `DiffEntry { kind, old_lineno, new_lineno, text }` + `DiffKind { Add, Del, Context }` used identically across Task 2 (definition/parser/formatter) and Task 3 (event_loop/retained/plain). `parse_unified_diff(&str, usize)`, `diff_gutter_width(&[DiffEntry])`, `diff_row_text(&DiffEntry, usize)` signatures consistent between definition (Task 2) and call sites (Task 3).
- **Open item for the implementer:** in Task 2 Step 4, before implementing, `grep -n "fn scrub_controls" crates/atomcode-tuix/src` and adopt option (b): `diff_row_text` returns `format!("  {numstr:>gutter$} {sign} {}", entry.text)` (raw), scrubbing stays at the two render sites.
