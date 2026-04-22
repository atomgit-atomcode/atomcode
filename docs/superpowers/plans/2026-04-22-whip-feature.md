# Whip Feature Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `Ctrl+G` / `/whip` feature to atomcode-tuix that lets users urge the running agent with a random encouragement phrase, accompanied by a 5-row ASCII whip-sweep animation.

**Architecture:** New `whip` module in `atomcode-tuix` (phrases pool + curve animation). New `WhipOverlay` modal renders the animation. Triggered by `Ctrl+G` keybinding and a streaming-whitelisted `/whip` slash command. Both paths funnel into `whip::fire_whip`, which sends `AgentCommand::AppendInput(phrase)` when a turn is running and installs the overlay modal regardless of phase. Backed by a `WhipConfig` section in `atomcode-core::config::Config` (enabled / cooldown_ms / phrases override). Spec: `docs/superpowers/specs/2026-04-22-whip-design.md`.

**Tech Stack:** Rust, crossterm (TTY), tokio (33ms tick interval), rand 0.9 (phrase selection), serde (config), existing `atomcode-tuix` retained renderer + Modal trait.

---

## File Structure

**New files:**
- `crates/atomcode-tuix/src/whip/mod.rs` — public API (`fire_whip`, `Cooldown`, `WhipSession` types)
- `crates/atomcode-tuix/src/whip/phrases.rs` — default phrases + `pick_phrase_with_rng`
- `crates/atomcode-tuix/src/whip/anim.rs` — `FrameBuf`, `frame(idx, width, phrase) -> FrameBuf`
- `crates/atomcode-tuix/src/modals/whip_overlay.rs` — `WhipOverlay` Modal impl
- `crates/atomcode-tuix/tests/whip_integration.rs` — end-to-end integration tests

**Modified files:**
- `crates/atomcode-core/src/config/mod.rs` — add `WhipConfig` + field on `Config`
- `crates/atomcode-tuix/Cargo.toml` — add `rand = "0.9"`
- `crates/atomcode-tuix/src/lib.rs` — `pub mod whip;`, `LoopCtx.last_whip_at = None` in `run`
- `crates/atomcode-tuix/src/input/key_action.rs` — new `Action::Whip` + `Ctrl+G` mapping
- `crates/atomcode-tuix/src/event_loop/mod.rs` — new `last_whip_at` field on LoopCtx, `whip_tick` interval, `Action::Whip` handler in idle + streaming, `/whip` whitelist in streaming gate
- `crates/atomcode-tuix/src/event_loop/commands.rs` — `/whip` arm
- `crates/atomcode-tuix/src/commands.rs` — register `whip` in BUILTIN_COMMANDS
- `crates/atomcode-tuix/src/modals/mod.rs` — `pub mod whip_overlay; pub use whip_overlay::WhipOverlay;`
- `crates/atomcode-tuix/src/render/mod.rs` — new `UiLine::WhipFrame` variant
- `crates/atomcode-tuix/src/render/retained.rs` — paint path for `UiLine::WhipFrame`
- `crates/atomcode-tuix/src/render/plain.rs` — plain-text fallback

---

## Task 1: WhipConfig in core

**Files:**
- Modify: `crates/atomcode-core/src/config/mod.rs`

- [ ] **Step 1: Add `WhipConfig` struct + field**

Insert after the `DatalogConfig` block (just after `impl Default for DatalogConfig`):

```rust
/// Controls the Ctrl+G / `/whip` "urge the agent" feature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhipConfig {
    /// When false, Ctrl+G falls through as a no-op and `/whip` errors out.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Minimum gap between two successive fires, in milliseconds.
    #[serde(default = "default_whip_cooldown_ms")]
    pub cooldown_ms: u64,
    /// When non-empty, REPLACES the built-in phrase pool (not merged).
    #[serde(default)]
    pub phrases: Vec<String>,
}

fn default_whip_cooldown_ms() -> u64 { 1000 }

impl Default for WhipConfig {
    fn default() -> Self {
        Self { enabled: true, cooldown_ms: 1000, phrases: Vec::new() }
    }
}
```

Then add the field to `Config` right after the `auto_update` field (before the closing `}`):

```rust
    /// `[whip]` — the Ctrl+G "urge the agent" feature. See `WhipConfig`.
    /// Absent from older configs → defaults to enabled + built-in phrases.
    #[serde(default)]
    pub whip: WhipConfig,
```

- [ ] **Step 2: Add unit test for default + round-trip**

Append inside the existing `#[cfg(test)] mod tests { ... }` block in the same file (find existing Config tests; if none, create the module):

```rust
    #[test]
    fn whip_config_defaults_when_missing() {
        let toml = r#"
default_provider = "test"
[providers.test]
type = "openai"
api_key = "sk"
model = "m"
context_window = 8000
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(cfg.whip.enabled);
        assert_eq!(cfg.whip.cooldown_ms, 1000);
        assert!(cfg.whip.phrases.is_empty());
    }

    #[test]
    fn whip_config_respects_overrides() {
        let toml = r#"
default_provider = "test"
[providers.test]
type = "openai"
api_key = "sk"
model = "m"
context_window = 8000
[whip]
enabled = false
cooldown_ms = 500
phrases = ["a", "b"]
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(!cfg.whip.enabled);
        assert_eq!(cfg.whip.cooldown_ms, 500);
        assert_eq!(cfg.whip.phrases, vec!["a".to_string(), "b".to_string()]);
    }
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p atomcode-core --lib whip_config`
Expected: 2 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/atomcode-core/src/config/mod.rs
git commit -m "feat(core): add WhipConfig for the rope/whip urge feature"
```

---

## Task 2: Add rand dependency and whip module skeleton

**Files:**
- Modify: `crates/atomcode-tuix/Cargo.toml`
- Create: `crates/atomcode-tuix/src/whip/mod.rs`
- Create: `crates/atomcode-tuix/src/whip/phrases.rs`
- Modify: `crates/atomcode-tuix/src/lib.rs`

- [ ] **Step 1: Add rand to Cargo.toml**

In `crates/atomcode-tuix/Cargo.toml`, in `[dependencies]`, append:

```toml
rand = "0.9"
```

- [ ] **Step 2: Create `whip/phrases.rs`**

Write `crates/atomcode-tuix/src/whip/phrases.rs`:

```rust
// crates/atomcode-tuix/src/whip/phrases.rs
//
// Catalogue of "urge" phrases used by Ctrl+G / `/whip`. The built-in
// pool is intentionally short and bilingual — when users want their
// own vocabulary, `WhipConfig.phrases` REPLACES the pool entirely (no
// merging), so the default list stays deliberately tiny.

use rand::Rng;

pub const DEFAULT_PHRASES: &[&str] = &[
    "FASTER",
    "GO FASTER",
    "Speed it up",
    "Work harder clanker",
    "Move it",
    "快点",
    "别磨蹭",
    "加速",
    "动起来",
    "赶紧的",
];

/// Select one phrase for this whip. When `user_override` is empty, draws
/// from `DEFAULT_PHRASES`; otherwise draws exclusively from the override.
pub fn pick_phrase(user_override: &[String]) -> String {
    pick_phrase_with_rng(user_override, &mut rand::rng())
}

/// Testable seam — callers in tests pass a seeded RNG for determinism.
pub fn pick_phrase_with_rng<R: Rng>(user_override: &[String], rng: &mut R) -> String {
    if user_override.is_empty() {
        DEFAULT_PHRASES[rng.random_range(0..DEFAULT_PHRASES.len())].to_string()
    } else {
        user_override[rng.random_range(0..user_override.len())].clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    #[test]
    fn default_pool_is_nonempty_and_bilingual() {
        assert!(DEFAULT_PHRASES.len() >= 6);
        assert!(DEFAULT_PHRASES.iter().any(|p| p.is_ascii()));
        assert!(DEFAULT_PHRASES.iter().any(|p| !p.is_ascii()));
    }

    #[test]
    fn seeded_rng_is_deterministic() {
        let mut r1 = StdRng::seed_from_u64(42);
        let mut r2 = StdRng::seed_from_u64(42);
        assert_eq!(pick_phrase_with_rng(&[], &mut r1), pick_phrase_with_rng(&[], &mut r2));
    }

    #[test]
    fn override_fully_replaces_defaults() {
        let user = vec!["zulu".to_string()];
        let mut rng = StdRng::seed_from_u64(7);
        // Every draw from a singleton must yield "zulu".
        for _ in 0..20 {
            assert_eq!(pick_phrase_with_rng(&user, &mut rng), "zulu");
        }
    }

    #[test]
    fn empty_override_falls_back_to_defaults() {
        let mut rng = StdRng::seed_from_u64(11);
        let picked = pick_phrase_with_rng(&[], &mut rng);
        assert!(DEFAULT_PHRASES.iter().any(|p| *p == picked));
    }
}
```

- [ ] **Step 3: Create `whip/mod.rs` with Cooldown only (no fire_whip yet)**

Write `crates/atomcode-tuix/src/whip/mod.rs`:

```rust
// crates/atomcode-tuix/src/whip/mod.rs
//
// The "whip" feature — Ctrl+G / `/whip` nudge. Split out into its own
// module so the tuix event loop doesn't grow another ad-hoc feature dir:
//   - phrases.rs : phrase pool + RNG-backed picker
//   - anim.rs    : frame generator (added in Task 4)
//   - mod.rs     : Cooldown + fire_whip orchestration (fire_whip in Task 5)

pub mod phrases;

use std::time::{Duration, Instant};

/// Monotonic rate-limit gate shared by both Ctrl+G and `/whip`.
/// `last` is stored on `LoopCtx` (not inside this struct) so a single
/// source of truth lives with the event loop; this struct is a stateless
/// helper.
pub struct Cooldown;

impl Cooldown {
    /// Returns true if a whip may fire right now (the stored `last` is
    /// either None or older than `window`). Callers update `last` on
    /// their own after a successful fire.
    pub fn try_fire(last: Option<Instant>, now: Instant, window: Duration) -> bool {
        match last {
            None => true,
            Some(t) => now.duration_since(t) >= window,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_fire_is_always_allowed() {
        let now = Instant::now();
        assert!(Cooldown::try_fire(None, now, Duration::from_millis(1000)));
    }

    #[test]
    fn fire_within_window_is_blocked() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(500);
        assert!(!Cooldown::try_fire(Some(t0), t1, Duration::from_millis(1000)));
    }

    #[test]
    fn fire_exactly_at_window_is_allowed() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(1000);
        assert!(Cooldown::try_fire(Some(t0), t1, Duration::from_millis(1000)));
    }

    #[test]
    fn fire_after_window_is_allowed() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(1001);
        assert!(Cooldown::try_fire(Some(t0), t1, Duration::from_millis(1000)));
    }
}
```

- [ ] **Step 4: Register module in lib.rs**

Modify `crates/atomcode-tuix/src/lib.rs` — in the `pub mod` block at the top (around line 7 next to `pub mod modals;`), add:

```rust
pub mod whip;
```

Alphabetical placement: after `pub mod think;` / before `pub mod trace;` or wherever it fits the existing pattern — consult current file.

- [ ] **Step 5: Build + run tests**

Run: `cargo build -p atomcode-tuix`
Expected: compiles clean.

Run: `cargo test -p atomcode-tuix --lib whip::`
Expected: 7 tests pass (4 in phrases, 4 in mod, minus one if I miscounted — it's `phrases(4) + mod(4) = 8`; if any fail, fix).

- [ ] **Step 6: Commit**

```bash
git add crates/atomcode-tuix/Cargo.toml crates/atomcode-tuix/src/whip/ crates/atomcode-tuix/src/lib.rs
git commit -m "feat(tuix): add whip module skeleton (phrases + Cooldown)"
```

---

## Task 3: Action::Whip + LoopCtx field

**Files:**
- Modify: `crates/atomcode-tuix/src/input/key_action.rs`
- Modify: `crates/atomcode-tuix/src/event_loop/mod.rs`
- Modify: `crates/atomcode-tuix/src/lib.rs`

- [ ] **Step 1: Add `Action::Whip` and Ctrl+G binding — write failing test first**

Append to the `tests` module in `crates/atomcode-tuix/src/input/key_action.rs`:

```rust
    #[test]
    fn ctrl_g_whips() {
        assert_eq!(k(KeyCode::Char('g'), KeyModifiers::CONTROL), Action::Whip);
    }
```

- [ ] **Step 2: Run test — expect FAIL**

Run: `cargo test -p atomcode-tuix --lib key_action::tests::ctrl_g_whips`
Expected: FAIL (`Action::Whip` doesn't exist yet).

- [ ] **Step 3: Add the variant + classify arm**

In `crates/atomcode-tuix/src/input/key_action.rs`:

- Add `Whip,` to the `Action` enum (after `DeleteToEnd,` around line 11).
- Add the classify arm right after the existing `Ctrl+k` line (around line 36):

```rust
        (KeyCode::Char('g'), true) => Action::Whip,
```

- [ ] **Step 4: Run test — expect PASS**

Run: `cargo test -p atomcode-tuix --lib key_action::tests::ctrl_g_whips`
Expected: PASS. Also run the full `key_action::tests` module to ensure nothing else broke.

- [ ] **Step 5: Add `last_whip_at` field to LoopCtx**

In `crates/atomcode-tuix/src/event_loop/mod.rs`, in `pub struct LoopCtx { ... }` (starts at line 39), insert right after `pub previous_dir: Option<PathBuf>,`:

```rust
    /// Timestamp of the last whip fire (Ctrl+G or `/whip`). None = never
    /// fired. Used by `whip::Cooldown::try_fire` to rate-limit.
    pub last_whip_at: Option<std::time::Instant>,
```

- [ ] **Step 6: Initialize it where LoopCtx is constructed**

In `crates/atomcode-tuix/src/lib.rs`, find the `let ctx = LoopCtx { ... }` block (around line 224). Add:

```rust
        last_whip_at: None,
```

in the struct literal (anywhere; alphabetical-ish placement is fine — match existing style).

- [ ] **Step 7: Build**

Run: `cargo build -p atomcode-tuix`
Expected: compiles. If "pattern non-exhaustive" errors appear for `Action::Whip` in match blocks, those will be fixed in Task 5 — add `Action::Whip => {}` stub arms in `handle_idle_key` and `handle_streaming_key` to keep compilation green. These stubs will be replaced in Task 5.

- [ ] **Step 8: Commit**

```bash
git add crates/atomcode-tuix/src/input/key_action.rs crates/atomcode-tuix/src/event_loop/mod.rs crates/atomcode-tuix/src/lib.rs
git commit -m "feat(tuix): wire Action::Whip (Ctrl+G) + LoopCtx.last_whip_at"
```

---

## Task 4: Animation module — frame generator

**Files:**
- Create: `crates/atomcode-tuix/src/whip/anim.rs`
- Modify: `crates/atomcode-tuix/src/whip/mod.rs`

- [ ] **Step 1: Write the failing test (one frame per boundary)**

Create `crates/atomcode-tuix/src/whip/anim.rs` with JUST the tests at first so they fail on missing symbols:

```rust
// crates/atomcode-tuix/src/whip/anim.rs
//
// Procedural frame generator for the whip sweep. Produces 15 frames over
// ~500ms. Not a physics simulation — a sinusoidal curve whose amplitude
// follows a bell and whose horizontal reach grows linearly with frame
// index. Frame 11 is the "crack" (flash = true, 💥 at tip, phrase shows).

// (implementation below)

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_0_has_coil_on_left() {
        let f = frame(0, 40, "FASTER");
        assert!(!f.flash);
        assert_eq!(f.phrase, None);
        // Some visible content lives in the left 6 cells of the middle row.
        let mid = &f.rows[2];
        assert!(!mid.trim().is_empty(), "row 2 should show the coil");
    }

    #[test]
    fn frame_10_reaches_right_edge() {
        let f = frame(10, 40, "FASTER");
        let mid = &f.rows[2];
        // The last non-blank cell should be near the right edge.
        let last_nonblank = mid.char_indices()
            .filter(|(_, c)| !c.is_whitespace())
            .last()
            .map(|(i, _)| i)
            .unwrap_or(0);
        assert!(last_nonblank >= 30, "tip should be past col 30 at frame 10, was {}", last_nonblank);
    }

    #[test]
    fn frame_11_is_the_crack() {
        let f = frame(11, 40, "FASTER");
        assert!(f.flash, "frame 11 must flash");
        assert_eq!(f.phrase.as_deref(), Some("FASTER"));
    }

    #[test]
    fn frame_15_is_empty() {
        let f = frame(15, 40, "FASTER");
        assert!(f.rows.iter().all(|r| r.trim().is_empty()));
        assert_eq!(f.phrase, None);
        assert!(!f.flash);
    }

    #[test]
    fn width_below_30_still_produces_5_rows() {
        let f = frame(5, 20, "快点");
        assert_eq!(f.rows.len(), 5);
    }
}
```

- [ ] **Step 2: Run — expect FAIL**

Run: `cargo test -p atomcode-tuix --lib whip::anim`
Expected: compile error ("cannot find function `frame`"). That's the failing state.

- [ ] **Step 3: Implement `FrameBuf` + `frame`**

Replace the placeholder comment in `anim.rs` with:

```rust
use std::f32::consts::PI;

#[derive(Debug, Clone)]
pub struct FrameBuf {
    /// 5 rendered display strings, one per row (top to bottom).
    pub rows: [String; 5],
    /// The crack phrase to display under the whip. `None` until frame 11.
    pub phrase: Option<String>,
    /// When true the renderer should invert non-blank cells (crack flash).
    pub flash: bool,
}

/// Whip width cap — on terminals wider than this the animation is drawn
/// centred within this many columns.
const MAX_WIDTH: usize = 60;
const TOTAL_FRAMES: u16 = 15;
/// Frame index at which the crack happens (flash + 💥 + phrase).
const CRACK_FRAME: u16 = 11;
/// Frames 0..=10 progress the sweep; we divide by 11 for progress∈[0,1].
const SWEEP_END: u16 = 11;

pub fn frame(idx: u16, terminal_width: u16, phrase: &str) -> FrameBuf {
    if idx >= TOTAL_FRAMES {
        return FrameBuf { rows: empty_rows(), phrase: None, flash: false };
    }
    let width = (terminal_width as usize).min(MAX_WIDTH).max(10);
    let mut rows = empty_grid(width);

    if idx <= SWEEP_END - 1 {
        draw_sweep(&mut rows, idx, width);
    } else if idx == CRACK_FRAME {
        draw_full_reach(&mut rows, width);
        place_tip(&mut rows, width - 1, "💥");
    } else {
        // 12..=14 decay
        let decay_progress = (idx - CRACK_FRAME) as f32 / (TOTAL_FRAMES - 1 - CRACK_FRAME) as f32;
        draw_decay(&mut rows, decay_progress, width);
    }

    let rows_strings: [String; 5] = [
        rows[0].iter().collect(),
        rows[1].iter().collect(),
        rows[2].iter().collect(),
        rows[3].iter().collect(),
        rows[4].iter().collect(),
    ];

    let phrase_out = if idx >= CRACK_FRAME && idx < TOTAL_FRAMES - 0 {
        Some(phrase.to_string())
    } else {
        None
    };
    let flash = idx == CRACK_FRAME;

    FrameBuf { rows: rows_strings, phrase: phrase_out, flash }
}

fn empty_rows() -> [String; 5] {
    [String::new(), String::new(), String::new(), String::new(), String::new()]
}

fn empty_grid(width: usize) -> Vec<Vec<char>> {
    vec![vec![' '; width]; 5]
}

/// Frames 0..=10: bell-shaped amplitude, linear reach.
fn draw_sweep(rows: &mut [Vec<char>], idx: u16, width: usize) {
    // Always show the handle at col 0 on row 2.
    rows[2][0] = '╫';
    let progress = idx as f32 / SWEEP_END as f32; // 0..=1
    let amplitude = 1.5 * (PI * progress).sin();
    let reach = ((width as f32) * progress).round() as usize;
    for x in 1..reach.min(width) {
        let t = x as f32 / width as f32;
        let y_off = amplitude * (2.0 * PI * t - PI * progress).sin();
        let row = ((2.0 + y_off).round() as i32).clamp(0, 4) as usize;
        let ch = pick_body_char(y_off.abs(), idx);
        if rows[row][x] == ' ' {
            rows[row][x] = ch;
        }
    }
    // Tip hint on the last visible cell.
    if reach > 0 && reach <= width {
        let tip_col = (reach - 1).min(width - 1);
        rows[2][tip_col] = '»';
    }
}

/// Frame 11: whip fully extended at peak reach.
fn draw_full_reach(rows: &mut [Vec<char>], width: usize) {
    rows[2][0] = '╫';
    for x in 1..width.saturating_sub(2) {
        if rows[2][x] == ' ' {
            rows[2][x] = '─';
        }
    }
}

/// Frames 12..=14: amplitude collapses to zero along a straight line.
fn draw_decay(rows: &mut [Vec<char>], progress: f32, width: usize) {
    rows[2][0] = '╫';
    let dots_only = progress > 0.5;
    for x in 1..width.saturating_sub(1) {
        rows[2][x] = if dots_only { ' ' } else { '~' };
    }
}

fn place_tip(rows: &mut [Vec<char>], col: usize, s: &str) {
    // `💥` is width-2; write it into a single cell slot and let width.rs
    // deduplicate later on render — we keep one char per grid cell, so
    // render-time handles the actual 2-cell draw.
    if let Some(c) = s.chars().next() {
        if col < rows[2].len() {
            rows[2][col] = c;
        }
    }
}

fn pick_body_char(abs_y: f32, idx: u16) -> char {
    // Dense characters for slow early frames, thin/fast glyphs for
    // later high-velocity frames. Very approximate — tuned by eye.
    if idx < 4 {
        if abs_y > 1.0 { '╱' } else { '─' }
    } else if idx < 8 {
        if abs_y > 1.0 { '╲' } else { '~' }
    } else {
        '≈'
    }
}
```

- [ ] **Step 4: Register in `whip/mod.rs`**

Add to `whip/mod.rs` right after `pub mod phrases;`:

```rust
pub mod anim;
```

- [ ] **Step 5: Run tests — expect PASS**

Run: `cargo test -p atomcode-tuix --lib whip::anim`
Expected: 5 tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/atomcode-tuix/src/whip/anim.rs crates/atomcode-tuix/src/whip/mod.rs
git commit -m "feat(tuix): whip animation frame generator (15-frame sweep)"
```

---

## Task 5: UiLine::WhipFrame + renderer paint paths

**Files:**
- Modify: `crates/atomcode-tuix/src/render/mod.rs`
- Modify: `crates/atomcode-tuix/src/render/plain.rs`
- Modify: `crates/atomcode-tuix/src/render/retained.rs`

- [ ] **Step 1: Add variant to UiLine enum**

In `crates/atomcode-tuix/src/render/mod.rs`, the `pub enum UiLine { ... }` block (near line 15). Add a variant near the end of the enum (before the trailing `}`):

```rust
    /// Whip overlay frame — 5 rows painted immediately above the status
    /// line. Ephemeral: each render replaces the previous frame. Closed
    /// when the `WhipOverlay` modal finishes.
    WhipFrame {
        rows: [String; 5],
        /// Crack phrase, shown in a bold accent row when present.
        phrase: Option<String>,
        /// When true the renderer should invert all non-blank cells.
        flash: bool,
    },
```

- [ ] **Step 2: Handle variant in PlainRenderer**

In `crates/atomcode-tuix/src/render/plain.rs`, find the `match` block that dispatches on `UiLine` variants. Add:

```rust
            UiLine::WhipFrame { phrase, .. } => {
                // Pipe / CI renderer: no animation. Print the phrase once
                // when it first appears (frame 11) and nothing for the
                // other frames.
                if let Some(p) = phrase {
                    let _ = writeln!(self.out, "  🐎 {}", p);
                }
            }
```

- [ ] **Step 3: Handle variant in RetainedRenderer**

In `crates/atomcode-tuix/src/render/retained.rs`, search for the match arm that handles existing UiLine variants (likely in a `fn render(&mut self, line: UiLine)` or similar). Add:

```rust
            UiLine::WhipFrame { rows, phrase, flash } => {
                self.paint_whip_overlay(&rows, phrase.as_deref(), flash);
            }
```

Then add the method to the renderer `impl` block:

```rust
    /// Paint the 5-row whip animation overlay immediately above the status
    /// line. Reuses the same "above-status" band that SessionPicker draws
    /// into — the overlay is ephemeral, so the existing footer invalidation
    /// on modal close will restore idle prompt rendering.
    fn paint_whip_overlay(&mut self, rows: &[String; 5], phrase: Option<&str>, flash: bool) {
        // The retained renderer already has a footer-anchored paint path
        // used by SessionPicker / ModelPicker / ProviderWizard. Emit the
        // 5 rows into that path. Use reverse-video on non-blank cells
        // when `flash` is true; use `phrase` (bold bright red) in row 4
        // when Some.
        //
        // Implementation detail: this follows the same pattern as
        // `paint_menu` — grep for it in this file for reference.
        let _ = (rows, phrase, flash); // scaffolding — see Task 7 for visuals
        // Placeholder: initial implementation delegates to whatever the
        // existing picker overlay uses. Visual polish lands in Task 8.
        self.mark_dirty();
    }
```

Note: we leave the body nearly empty intentionally — Task 8 polishes the colors/flash. At this point the plumbing must compile and the modal lifecycle must work.

- [ ] **Step 4: Build**

Run: `cargo build -p atomcode-tuix`
Expected: compiles. If the `paint_menu` reference or `mark_dirty` don't exist by those exact names in retained.rs, stub `paint_whip_overlay` to the simplest no-op that compiles (`let _ = (rows, phrase, flash);`). Task 8 revisits visual quality.

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-tuix/src/render/
git commit -m "feat(tuix): add UiLine::WhipFrame variant + renderer scaffolding"
```

---

## Task 6: WhipOverlay modal

**Files:**
- Create: `crates/atomcode-tuix/src/modals/whip_overlay.rs`
- Modify: `crates/atomcode-tuix/src/modals/mod.rs`

- [ ] **Step 1: Write WhipOverlay with tick-driven advance**

Create `crates/atomcode-tuix/src/modals/whip_overlay.rs`:

```rust
// crates/atomcode-tuix/src/modals/whip_overlay.rs
//
// Display-only modal that plays the 15-frame whip sweep, then closes.
// Does NOT consume keys (Esc is the one exception — early dismiss).
// Relies on `WhipOverlay::advance(now)` being called on each 33ms tick
// from the event loop.

use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{KeyCode, KeyModifiers};

use super::{Modal, ModalAction};
use crate::event_loop::{Buffer, LoopCtx};
use crate::render::{Renderer, UiLine};
use crate::state::UiState;
use crate::whip::anim;

/// Time between frames. 15 frames × 33ms ≈ 495ms total.
pub const FRAME_MS: u64 = 33;
const TOTAL_FRAMES: u16 = 15;

pub struct WhipOverlay {
    phrase: String,
    started_at: Instant,
    last_frame_drawn: Option<u16>,
    done: bool,
}

impl WhipOverlay {
    pub fn open(phrase: String) -> Self {
        Self {
            phrase,
            started_at: Instant::now(),
            last_frame_drawn: None,
            done: false,
        }
    }

    /// Frame index the timeline is in AT `now`. Clamped to TOTAL_FRAMES.
    pub fn current_frame(&self, now: Instant) -> u16 {
        let ms = now.duration_since(self.started_at).as_millis() as u64;
        let f = (ms / FRAME_MS) as u16;
        f.min(TOTAL_FRAMES)
    }

    /// Called by the event loop's 33ms tick. If a new frame is due,
    /// re-renders; when the animation is exhausted the caller clears
    /// `active_modal`. Returns `true` when the caller should drop the
    /// modal this tick.
    pub fn advance(
        &mut self,
        buf: &Buffer,
        state: &UiState,
        ctx: &LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> bool {
        if self.done {
            return true;
        }
        let now = Instant::now();
        let frame = self.current_frame(now);
        if self.last_frame_drawn != Some(frame) {
            self.last_frame_drawn = Some(frame);
            self.paint(frame, buf, state, ctx, renderer);
        }
        if frame >= TOTAL_FRAMES {
            self.done = true;
            return true;
        }
        false
    }

    fn paint(
        &self,
        frame_idx: u16,
        _buf: &Buffer,
        _state: &UiState,
        _ctx: &LoopCtx,
        renderer: &mut dyn Renderer,
    ) {
        let width = 60; // terminal-aware sizing is Task 8; 60 works for now
        let f = anim::frame(frame_idx, width, &self.phrase);
        renderer.render(UiLine::WhipFrame {
            rows: f.rows,
            phrase: f.phrase,
            flash: f.flash,
        });
        renderer.flush();
    }
}

impl Modal for WhipOverlay {
    fn handle_key(
        &mut self,
        code: KeyCode,
        _mods: KeyModifiers,
        _buf: &mut Buffer,
        _state: &mut UiState,
        _ctx: &mut LoopCtx,
        _renderer: &mut dyn Renderer,
    ) -> Result<ModalAction> {
        // Only Esc dismisses early. All other keys are swallowed for
        // the ~500ms animation lifetime.
        if code == KeyCode::Esc {
            self.done = true;
            return Ok(ModalAction::Close);
        }
        Ok(ModalAction::Continue)
    }

    fn draw(
        &self,
        buf: &Buffer,
        state: &UiState,
        ctx: &LoopCtx,
        renderer: &mut dyn Renderer,
    ) {
        // Initial paint — draws frame 0 so the user sees something
        // before the first tick arrives.
        self.paint(0, buf, state, ctx, renderer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_frame_at_t0_is_0() {
        let o = WhipOverlay::open("x".into());
        assert_eq!(o.current_frame(o.started_at), 0);
    }

    #[test]
    fn current_frame_advances_with_elapsed() {
        let o = WhipOverlay::open("x".into());
        let five = o.started_at + Duration::from_millis(5 * FRAME_MS + 5);
        assert_eq!(o.current_frame(five), 5);
    }

    #[test]
    fn current_frame_clamps_after_total() {
        let o = WhipOverlay::open("x".into());
        let way_later = o.started_at + Duration::from_secs(10);
        assert_eq!(o.current_frame(way_later), TOTAL_FRAMES);
    }
}
```

- [ ] **Step 2: Register modal**

In `crates/atomcode-tuix/src/modals/mod.rs`, add alongside existing modals:

```rust
pub mod whip_overlay;
pub use whip_overlay::WhipOverlay;
```

Alphabetical placement: after `pub mod session_picker;` works.

- [ ] **Step 3: Build + test**

Run: `cargo test -p atomcode-tuix --lib modals::whip_overlay`
Expected: 3 tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/atomcode-tuix/src/modals/
git commit -m "feat(tuix): add WhipOverlay modal (tick-driven animation player)"
```

---

## Task 7: fire_whip orchestrator + Ctrl+G handler wiring

**Files:**
- Modify: `crates/atomcode-tuix/src/whip/mod.rs`
- Modify: `crates/atomcode-tuix/src/event_loop/mod.rs`

- [ ] **Step 1: Add `fire_whip` to `whip/mod.rs`**

Append to `crates/atomcode-tuix/src/whip/mod.rs`:

```rust
use anyhow::Result;
use atomcode_core::agent::AgentCommand;

use crate::event_loop::LoopCtx;
use crate::modals::{Modal, WhipOverlay};
use crate::render::{Renderer, UiLine};
use crate::state::{UiPhase, UiState};

/// Fire a whip: play the animation and (if a turn is running) queue the
/// encouragement phrase via `AgentCommand::AppendInput`. Idempotent when
/// gated (cooldown, disabled, modal conflict) — just returns silently.
///
/// Must be called from both the Ctrl+G keyboard handler and the `/whip`
/// slash command so their semantics stay identical.
pub fn fire_whip(
    ctx: &mut LoopCtx,
    active_modal: &mut Option<Box<dyn Modal>>,
    state: &UiState,
    renderer: &mut dyn Renderer,
) -> Result<()> {
    if !ctx.config.whip.enabled {
        return Ok(());
    }
    // No whip during tool approval (the agent is waiting on you, not slow)
    // or while suspended (stdin is handed off to a child process).
    if matches!(state.phase, UiPhase::Approval | UiPhase::Suspended) {
        return Ok(());
    }
    if active_modal.is_some() {
        return Ok(());
    }
    let now = std::time::Instant::now();
    let window = std::time::Duration::from_millis(ctx.config.whip.cooldown_ms);
    if !Cooldown::try_fire(ctx.last_whip_at, now, window) {
        return Ok(());
    }

    let phrase = phrases::pick_phrase(&ctx.config.whip.phrases);
    ctx.last_whip_at = Some(now);

    // Scrollback trace — always printed so the user sees what happened
    // even without the animation (pipe mode, narrow terminal).
    let trace = if matches!(state.phase, UiPhase::Streaming) {
        let suffix = state
            .turn_elapsed()
            .map(|d| format!(" (after {:.1}s)", d.as_secs_f32()))
            .unwrap_or_default();
        format!("  🐎 whip: {}{}\n", phrase, suffix)
    } else {
        format!("  🐎 whip: {}  (no turn running)\n", phrase)
    };
    renderer.render(UiLine::CommandOutput(trace));
    renderer.flush();

    // Only inject into the LLM context when a turn is actually running.
    if matches!(state.phase, UiPhase::Streaming) {
        ctx.agent
            .cmd_tx
            .send(AgentCommand::AppendInput(phrase.clone()))
            .ok();
    }

    // Install the animation overlay (all phases). The event loop's 33ms
    // tick advances it via `WhipOverlay::advance`.
    *active_modal = Some(Box::new(WhipOverlay::open(phrase)));

    Ok(())
}
```

- [ ] **Step 2: Wire `Action::Whip` in handle_idle_key**

In `crates/atomcode-tuix/src/event_loop/mod.rs`, find the `handle_idle_key` function. Inside the `match action` block, add an arm:

```rust
        Action::Whip => {
            crate::whip::fire_whip(ctx, &mut app.active_modal, &app.state, renderer)?;
        }
```

- [ ] **Step 3: Wire `Action::Whip` in handle_streaming_key**

Similarly in `handle_streaming_key`, in the `match action` dispatch (the path that lands in `app.buf.apply`), short-circuit before reaching `apply`:

```rust
    let action = classify(code, modifiers);
    if action == Action::Whip {
        crate::whip::fire_whip(ctx, &mut app.active_modal, &app.state, renderer)?;
        return Ok(());
    }
    match app.buf.apply(action, ctx.history.entries(), &ctx.commands) { /* ...existing... */ }
```

(Replace the `Action::Whip => {}` stub left in Task 3 with the above.)

- [ ] **Step 4: Add whip tick interval to the event loop select!**

Still in `mod.rs` — find the main `tokio::select!` inside `run_loop` (the one that has `spinner_interval.tick()`). Add above that block:

```rust
    let mut whip_tick = tokio::time::interval(
        std::time::Duration::from_millis(crate::modals::whip_overlay::FRAME_MS),
    );
    whip_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
```

Inside the `select!`, add a new branch (before the spinner tick branch):

```rust
            _ = whip_tick.tick() => {
                if let Some(modal) = app.active_modal.as_mut() {
                    if let Some(overlay) = (modal as &mut dyn std::any::Any).downcast_mut::<crate::modals::WhipOverlay>() {
                        if overlay.advance(&app.buf, &app.state, ctx, renderer) {
                            app.active_modal = None;
                            // Force a redraw of idle / streaming UI so the
                            // overlay's 5-row band doesn't linger as ghost text.
                            redraw_after_slash(&app.buf, &app.state, ctx, &app.active_modal, renderer);
                        }
                    }
                }
            }
```

⚠ The `downcast_mut` path requires `Modal: std::any::Any` — if the `Modal` trait doesn't already extend `Any`, an alternative approach is:

- Add `fn as_any_mut(&mut self) -> &mut dyn std::any::Any { self }` as a provided method on the `Modal` trait.
- Each modal impl inherits the default; no per-modal overrides needed.

Add that default method to `crates/atomcode-tuix/src/modals/mod.rs` in the `Modal` trait definition:

```rust
    /// Downcast helper. Default impl works for any `Self: 'static`.
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any
    where
        Self: Sized + 'static,
    {
        self
    }
```

And use `modal.as_any_mut().downcast_mut::<WhipOverlay>()` in the select! branch instead of casting `&mut dyn Modal` directly.

- [ ] **Step 5: Build**

Run: `cargo build -p atomcode-tuix`
Expected: compiles clean. If `Modal: 'static` is missing, add the bound to the trait.

- [ ] **Step 6: Commit**

```bash
git add crates/atomcode-tuix/src/whip/mod.rs crates/atomcode-tuix/src/event_loop/mod.rs crates/atomcode-tuix/src/modals/mod.rs
git commit -m "feat(tuix): wire fire_whip + 33ms overlay tick"
```

---

## Task 8: `/whip` slash command + streaming whitelist

**Files:**
- Modify: `crates/atomcode-tuix/src/commands.rs`
- Modify: `crates/atomcode-tuix/src/event_loop/commands.rs`
- Modify: `crates/atomcode-tuix/src/event_loop/mod.rs`

- [ ] **Step 1: Register `/whip` in BUILTIN_COMMANDS**

In `crates/atomcode-tuix/src/commands.rs`, in `BUILTIN_COMMANDS`, add a line anywhere in the list:

```rust
    Command { name: "whip",    desc: "Urge the agent (also: Ctrl+G)" },
```

- [ ] **Step 2: Add the handler arm**

In `crates/atomcode-tuix/src/event_loop/commands.rs`, inside `execute_slash_command`'s `match cmd { ... }`, add an arm (somewhere before the catch-all `other` arm):

```rust
        "whip" => {
            crate::whip::fire_whip(ctx, active_modal, state, renderer)?;
        }
```

- [ ] **Step 3: Whitelist `/whip` in the streaming gate**

In `crates/atomcode-tuix/src/event_loop/mod.rs`, find the streaming slash-command gate (line ~1399, the `is_known_slash` block in `handle_streaming_key`'s commit arm). Replace:

```rust
            if is_known_slash {
                renderer.render(UiLine::CommandOutput(
                    "  (slash commands are disabled while a turn is running)\n".into(),
                ));
                renderer.flush();
                app.buf.text.clear();
                app.buf.cursor = 0;
                app.menu.selected = 0;
                draw_spinner_now(/*...*/);
                return Ok(());
            }
```

with:

```rust
            if is_known_slash {
                let (cmd_name, arg) = parse_slash_line(&line)
                    .expect("already known to parse (is_known_slash is true)");
                if cmd_name.eq_ignore_ascii_case("whip") {
                    // The ONE command that's valid mid-stream — it's the
                    // whole reason the feature exists.
                    app.buf.text.clear();
                    app.buf.cursor = 0;
                    app.menu.selected = 0;
                    execute_slash_command(
                        cmd_name, arg, &mut app.state, ctx, renderer, &mut app.active_modal,
                    )?;
                    return Ok(());
                }
                renderer.render(UiLine::CommandOutput(
                    "  (slash commands are disabled while a turn is running — except /whip)\n".into(),
                ));
                renderer.flush();
                app.buf.text.clear();
                app.buf.cursor = 0;
                app.menu.selected = 0;
                draw_spinner_now(&mut app.state, &app.buf, ctx, renderer, app.message_queue.len(), app.menu.selected);
                return Ok(());
            }
```

- [ ] **Step 4: Build**

Run: `cargo build -p atomcode-tuix`
Expected: compiles clean.

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-tuix/src/commands.rs crates/atomcode-tuix/src/event_loop/
git commit -m "feat(tuix): /whip slash command + streaming-mode whitelist"
```

---

## Task 9: Integration tests

**Files:**
- Create: `crates/atomcode-tuix/tests/whip_integration.rs`

- [ ] **Step 1: Write the tests**

Integration tests live outside `src/`, so they can observe the tuix crate only through its public API. Given tuix's public surface is narrow (`run()` drives a full TTY), we use the lower-level `whip::fire_whip` + a mock `LoopCtx` + the in-memory `PlainRenderer`.

Write `crates/atomcode-tuix/tests/whip_integration.rs`:

```rust
// crates/atomcode-tuix/tests/whip_integration.rs
//
// Black-box tests over the `whip::fire_whip` orchestrator. Constructs a
// mock LoopCtx + AgentHandle and checks the AgentCommand channel.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use atomcode_core::agent::{AgentCommand, AgentHandle};
use atomcode_core::config::{Config, WhipConfig};
use atomcode_tuix::render::plain::PlainRenderer;
use atomcode_tuix::state::{UiPhase, UiState};

fn make_ctx(cfg_whip: WhipConfig) -> (atomcode_tuix::event_loop::LoopCtx, tokio::sync::mpsc::UnboundedReceiver<AgentCommand>) {
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let (_evt_tx, evt_rx) = tokio::sync::mpsc::unbounded_channel();
    let agent = AgentHandle {
        cmd_tx,
        evt_rx: Arc::new(Mutex::new(evt_rx)),
    };
    let mut config = Config::default_for_tests();
    config.whip = cfg_whip;
    let ctx = atomcode_tuix::event_loop::LoopCtx {
        // Fill only the fields we touch. Other fields use dummy values
        // — if the compiler complains, look at how `event_loop::run_loop`
        // is initialised in `lib.rs::run` and copy those defaults.
        //
        // NOTE: this test harness is intentionally minimal; if LoopCtx
        // grows fields you cannot default here, move these tests into
        // `#[cfg(test)] mod tests` inside `whip/mod.rs` and pass a
        // hand-rolled `LoopCtxView` trait instead.
        config,
        agent,
        last_whip_at: None,
        working_dir: std::path::PathBuf::from("/tmp"),
        previous_dir: None,
        model_name: "t".into(),
        ..: panic!("LoopCtx has no Default; complete this harness before running")
    };
    (ctx, cmd_rx)
}

// NOTE: The harness above is a sketch. If the full LoopCtx construction
// is too expensive for a black-box test, the pragmatic path is to add a
// small public helper in `lib.rs` like `LoopCtx::for_tests()` that
// returns a ctx with dummy channels for every field the tests don't
// use. See "Alternative" at the bottom of this file.
```

Given `LoopCtx` has 12+ fields wired through `run()`, building a real one in a test file is heavy. **Instead**, put these tests inside the crate (they become unit tests with `#[cfg(test)]` access to private helpers):

Move the file to `crates/atomcode-tuix/src/whip/tests.rs` and add `#[cfg(test)] mod tests;` to `whip/mod.rs`. The tests then operate on a trimmed view.

**Practical minimum** — add this inside `crates/atomcode-tuix/src/whip/mod.rs` at the bottom of the existing `#[cfg(test)] mod tests { ... }` block:

```rust
    use atomcode_core::agent::AgentCommand;

    /// Stub "LoopCtx shard" exposing just what fire_whip reads/writes.
    /// Not all of LoopCtx is needed here — only (config.whip, agent.cmd_tx,
    /// last_whip_at). Factoring `fire_whip` to take these three via a
    /// trait would make this clean; for now we test through the real
    /// function with a minimal synthetic LoopCtx reached by a helper.
    fn drain_commands(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<AgentCommand>,
    ) -> Vec<AgentCommand> {
        let mut out = Vec::new();
        while let Ok(c) = rx.try_recv() {
            out.push(c);
        }
        out
    }

    // The remaining integration-style assertions live in whip_integration.rs
    // once `LoopCtx::for_tests()` (helper TBD in Task 10) lands.
```

Because the integration test surface needs a `LoopCtx::for_tests()` helper, **Task 9 is the place to add that helper**. Do it now:

- [ ] **Step 2: Add `LoopCtx::for_tests()` helper**

In `crates/atomcode-tuix/src/event_loop/mod.rs`, at the bottom of `impl LoopCtx` (add the `impl` block if one doesn't exist):

```rust
#[cfg(any(test, feature = "test-helpers"))]
impl LoopCtx {
    /// Build a minimal `LoopCtx` for tests. Agent channels are open but
    /// dangling (tests drain `cmd_rx` directly); all file-system
    /// interactions (session dir, history) use temp paths.
    pub fn for_tests(
        config: atomcode_core::config::Config,
    ) -> (Self, tokio::sync::mpsc::UnboundedReceiver<atomcode_core::agent::AgentCommand>) {
        use tokio::sync::mpsc;
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (_evt_tx, evt_rx) = mpsc::unbounded_channel();
        let (_upgrade_tx, upgrade_rx) = mpsc::unbounded_channel();
        let (_wake_tx, wake_rx) = mpsc::channel(1);
        let (_input_tx, input_rx) = mpsc::unbounded_channel();
        let agent = atomcode_core::agent::AgentHandle {
            cmd_tx,
            evt_rx: std::sync::Arc::new(std::sync::Mutex::new(evt_rx)),
        };
        let tmp = std::env::temp_dir().join("atomcode-tuix-tests");
        let _ = std::fs::create_dir_all(&tmp);
        let session_manager = atomcode_core::session::SessionManager::new(&tmp);
        let ctx = Self {
            config,
            model_name: "test-model".into(),
            agent,
            working_dir: tmp.clone(),
            previous_dir: None,
            last_whip_at: None,
            history: crate::input::history::History::load(tmp.join("hist.txt")),
            input_rx,
            commands: crate::commands::CommandRegistry::builtin(),
            session_manager,
            update_hint: std::sync::Arc::new(std::sync::Mutex::new(None)),
            wake_rx,
            reader: None,
            upgrade_tx: _upgrade_tx.clone(),
            upgrade_rx,
        };
        (ctx, cmd_rx)
    }
}
```

If any field name differs, match the real struct definition — consult `event_loop/mod.rs:39`.

- [ ] **Step 3: Write integration tests using the helper**

Replace the sketch in `crates/atomcode-tuix/tests/whip_integration.rs` with the real tests:

```rust
// crates/atomcode-tuix/tests/whip_integration.rs
use atomcode_core::agent::AgentCommand;
use atomcode_core::config::Config;
use atomcode_tuix::event_loop::LoopCtx;
use atomcode_tuix::render::plain::PlainRenderer;
use atomcode_tuix::state::{UiPhase, UiState};
use atomcode_tuix::whip;

fn mk_config() -> Config {
    // Use whatever constructor Config exposes for tests; fall back to
    // reading the example config in-tree if Config::default() is absent.
    // This comment serves as a checkpoint — the exact call depends on the
    // Config API at time of implementation.
    Config::default()
}

#[test]
fn fire_whip_during_streaming_sends_append_input() {
    let cfg = mk_config();
    let (mut ctx, mut cmd_rx) = LoopCtx::for_tests(cfg);
    let mut state = UiState::new();
    state.on_submit(); // phase = Streaming
    let mut modal: Option<Box<dyn atomcode_tuix::modals::Modal>> = None;
    let mut r = PlainRenderer::new();

    whip::fire_whip(&mut ctx, &mut modal, &state, &mut r).unwrap();

    // Exactly one AppendInput should have been sent.
    let mut found = false;
    while let Ok(c) = cmd_rx.try_recv() {
        if matches!(c, AgentCommand::AppendInput(_)) { found = true; }
    }
    assert!(found, "AppendInput must be sent while streaming");
    assert!(modal.is_some(), "modal must be installed");
}

#[test]
fn fire_whip_during_idle_does_not_send_append_input() {
    let cfg = mk_config();
    let (mut ctx, mut cmd_rx) = LoopCtx::for_tests(cfg);
    let state = UiState::new(); // Idle by default
    let mut modal: Option<Box<dyn atomcode_tuix::modals::Modal>> = None;
    let mut r = PlainRenderer::new();

    whip::fire_whip(&mut ctx, &mut modal, &state, &mut r).unwrap();

    while let Ok(c) = cmd_rx.try_recv() {
        assert!(!matches!(c, AgentCommand::AppendInput(_)), "no AppendInput at idle");
    }
    assert!(modal.is_some(), "overlay still shown at idle");
}

#[test]
fn fire_whip_during_approval_is_a_noop() {
    let cfg = mk_config();
    let (mut ctx, mut cmd_rx) = LoopCtx::for_tests(cfg);
    let mut state = UiState::new();
    state.on_submit();
    state.on_approval_needed("bash"); // phase = Approval
    let mut modal: Option<Box<dyn atomcode_tuix::modals::Modal>> = None;
    let mut r = PlainRenderer::new();

    whip::fire_whip(&mut ctx, &mut modal, &state, &mut r).unwrap();
    assert!(modal.is_none(), "no overlay during approval");
    assert!(cmd_rx.try_recv().is_err(), "no commands sent during approval");
}

#[test]
fn cooldown_blocks_second_fire() {
    let mut cfg = mk_config();
    cfg.whip.cooldown_ms = 1000;
    let (mut ctx, _rx) = LoopCtx::for_tests(cfg);
    let mut state = UiState::new();
    state.on_submit();
    let mut modal: Option<Box<dyn atomcode_tuix::modals::Modal>> = None;
    let mut r = PlainRenderer::new();

    whip::fire_whip(&mut ctx, &mut modal, &state, &mut r).unwrap();
    modal = None; // simulate overlay closing
    // Immediately fire again — cooldown should block (no new modal install).
    whip::fire_whip(&mut ctx, &mut modal, &state, &mut r).unwrap();
    assert!(modal.is_none(), "second fire within cooldown must be silent");
}

#[test]
fn disabled_config_suppresses_everything() {
    let mut cfg = mk_config();
    cfg.whip.enabled = false;
    let (mut ctx, mut cmd_rx) = LoopCtx::for_tests(cfg);
    let mut state = UiState::new();
    state.on_submit();
    let mut modal: Option<Box<dyn atomcode_tuix::modals::Modal>> = None;
    let mut r = PlainRenderer::new();

    whip::fire_whip(&mut ctx, &mut modal, &state, &mut r).unwrap();
    assert!(modal.is_none());
    assert!(cmd_rx.try_recv().is_err());
}
```

If `Config::default()` is not implemented, add a `Config::default_for_tests()` method in `atomcode-core::config::mod.rs` that returns a minimum-viable Config, and call that instead.

- [ ] **Step 4: Run**

Run: `cargo test -p atomcode-tuix --test whip_integration`
Expected: 5 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-tuix/src/event_loop/mod.rs crates/atomcode-tuix/tests/whip_integration.rs crates/atomcode-core/src/config/mod.rs
git commit -m "test(tuix): integration tests for whip orchestrator"
```

---

## Task 10: Visual polish — terminal-width awareness, fallback, colors

**Files:**
- Modify: `crates/atomcode-tuix/src/modals/whip_overlay.rs`
- Modify: `crates/atomcode-tuix/src/render/retained.rs`

- [ ] **Step 1: Pass real terminal width into the overlay**

In `WhipOverlay::paint`, replace the hard-coded `60`:

```rust
        let width = {
            // Caller uses crossterm; `renderer` knows its width via caps.
            let (cols, _) = crossterm::terminal::size().unwrap_or((60, 24));
            cols
        };
        let f = anim::frame(frame_idx, width, &self.phrase);
```

- [ ] **Step 2: Narrow-terminal fallback**

At the top of `WhipOverlay::paint`, before the frame call:

```rust
        let (cols, _) = crossterm::terminal::size().unwrap_or((60, 24));
        if cols < 30 {
            // Too narrow for a 5-row animation; skip frames, just hold the
            // phrase on frame 11+. Scrollback line already printed in
            // fire_whip so the user has a trace regardless.
            if frame_idx < 11 {
                return;
            }
            // no-op: leave previous paint (or blank) as-is
            return;
        }
```

- [ ] **Step 3: Color + flash in retained renderer**

In `crates/atomcode-tuix/src/render/retained.rs`, flesh out `paint_whip_overlay`:

```rust
    fn paint_whip_overlay(&mut self, rows: &[String; 5], phrase: Option<&str>, flash: bool) {
        // Find the overlay band (5 rows immediately above the status row).
        // Reuse the same coordinate helpers the SessionPicker overlay uses
        // — search this file for `overlay_origin` or the SessionPicker's
        // paint method and mirror its positioning.
        let origin_row = self.status_row().saturating_sub(5);
        for (dy, row) in rows.iter().enumerate() {
            let target_row = origin_row + dy as u16;
            self.clear_row(target_row);
            if flash {
                // SGR: reverse video on non-blank cells. The cell-diff
                // engine will re-paint only the delta.
                self.write_styled_row(target_row, row, Style::reverse());
            } else {
                self.write_styled_row(target_row, row, Style::dim());
            }
        }
        if let Some(p) = phrase {
            let target_row = origin_row + 4;
            self.clear_row(target_row);
            self.write_styled_row(target_row, &format!("  💥 {}", p), Style::bold_red());
        }
        self.mark_dirty();
    }
```

If `status_row()` / `clear_row` / `write_styled_row` / `Style::*` don't exist, adapt to the actual primitives in `retained.rs`. The method signatures above are illustrative — the point is: (a) write 5 rows into the status-adjacent band, (b) invert cells when `flash`, (c) bold-red the phrase row.

- [ ] **Step 4: Build + run the full test suite**

Run: `cargo test -p atomcode-tuix`
Expected: all prior tests plus whip tests pass.

- [ ] **Step 5: Manual smoke**

Run: `cargo run -p atomcode -- --tui` (or the default TUI) in a real terminal. Ask the agent any question, then hit Ctrl+G during streaming. Verify:

1. `🐎 whip: <phrase>` appears in scrollback.
2. 5-row animation plays briefly.
3. Turn continues (did NOT cancel).
4. Second press within 1 s is silent.
5. Press again >1s later → whip plays, another AppendInput delivered.
6. Ctrl+G at idle → animation plays, no extra turn fires.
7. `/whip` while streaming → same as Ctrl+G.
8. `/whip` at idle → same as Ctrl+G.

- [ ] **Step 6: Commit**

```bash
git add crates/atomcode-tuix/src/modals/whip_overlay.rs crates/atomcode-tuix/src/render/retained.rs
git commit -m "feat(tuix): terminal-width-aware whip animation + flash styling"
```

---

## Task 11: Docs

**Files:**
- Modify: `crates/atomcode-tuix/src/commands.rs` (description already set in Task 8 — no-op if updated there)
- Create: `docs/keybindings.md` entry OR update existing keybindings doc
- Modify: `docs/config.example.toml`

- [ ] **Step 1: Append to `docs/config.example.toml`**

Add at the bottom of `docs/config.example.toml`:

```toml
# ---------------------------------------------------------------------------
# [whip] — the Ctrl+G / `/whip` "urge the agent" feature. Purely cosmetic +
# an [Additional context from user] prefix injected into the next LLM call
# during streaming. No effect on models, temperature, etc.
# ---------------------------------------------------------------------------
[whip]
enabled = true
cooldown_ms = 1000
# When unset/empty, uses the built-in bilingual pool (FASTER / 快点 / ...).
# phrases = ["FASTER", "赶紧的"]
```

- [ ] **Step 2: Keybindings doc**

Find the existing keybindings doc (check `docs/` or the web `site/docs/keybindings.html`). Add a row:

| `Ctrl+G` | Whip — urge the agent (also: `/whip`). |

- [ ] **Step 3: Commit**

```bash
git add docs/
git commit -m "docs: whip feature config + keybinding"
```

---

## Self-Review

**Spec coverage check** (each spec section → task that implements it):

- § 3.1 module layout → Tasks 2, 4, 6
- § 3.2 invocation paths (Ctrl+G + /whip whitelist) → Tasks 3, 7, 8
- § 3.3 LoopCtx additions → Task 3 (`last_whip_at`)
- § 3.4 frame advance via tick → Task 7 (whip_tick interval + `advance` call)
- § 4.1 phrases module → Task 2
- § 4.2 anim module → Task 4
- § 4.3 WhipOverlay modal → Task 6
- § 4.4 UiLine::WhipFrame → Task 5
- § 4.5 Action::Whip + key handling → Tasks 3, 7
- § 4.6 slash registration + streaming whitelist → Task 8
- § 5 WhipConfig → Task 1
- § 6 edge cases: non-TTY, narrow, colors, modal-busy, Approval/Suspended, cooldown, disabled → Tasks 7 (fire_whip guards) + 10 (fallback) + 1 (config) + 5 (PlainRenderer)
- § 7 tests → Tasks 2, 3, 4, 6, 9
- § 9 implementation order → Tasks 1-11 mirror it

**Placeholder scan:** Task 5 Step 3 and Task 10 Step 3 reference renderer primitives by name (`status_row`, `clear_row`, `write_styled_row`, `Style::reverse`) whose exact API the implementing engineer must confirm against `retained.rs`. This is called out explicitly in each step — it's a fitting-to-existing-code instruction, not a TODO.

**Type consistency:** `fire_whip` signature is `(&mut LoopCtx, &mut Option<Box<dyn Modal>>, &UiState, &mut dyn Renderer) -> Result<()>` in Tasks 7, 8, 9, 10. Matches. `WhipOverlay::open` takes `String` in Tasks 6, 7. Matches. `FrameBuf { rows: [String; 5], phrase: Option<String>, flash: bool }` in Tasks 4, 5, 6. Matches. Good.

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-04-22-whip-feature.md`. Two execution options:

**1. Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration.

**2. Inline Execution** — Execute tasks in this session using executing-plans, batch execution with checkpoints.

Which approach?
