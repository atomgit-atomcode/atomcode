# Whip (鞭子) Feature — Design Spec

**Date:** 2026-04-22
**Base branch:** `main` (targets v4.19.x series)
**Status:** Draft, pending user review

## 1. Motivation

Users sometimes feel an atomcode turn is running too slowly — LLM latency,
long tool chains, provider hiccups. Today the only mid-turn interactions
available are:

- `Ctrl+C` — hard cancel, destroys the in-flight turn
- Typing a message — gets queued in `App::message_queue` (type-ahead)
  and only fires *after* the current turn finishes

Neither lets the user say "keep going, but hurry up" to the running agent.
OpenWhip (a sibling Electron project) addresses this with a visual whip
overlay + OS-level keyboard injection that sends `Ctrl+C` and types
"FASTER" into the active terminal. That approach is fragile across
platforms (osascript / xdotool / Win32 FFI), doesn't work over SSH, and
can't discriminate which terminal is the active CLI.

This spec ports the **semantic core of OpenWhip — "nudge the agent with a
random encouragement phrase" — into atomcode proper**, with an ASCII
animation inspired by (but not replicating) OpenWhip's visual feel.

## 2. Scope

**In scope**

- New `Ctrl+G` keybinding: triggers whip in both `Idle` and `Streaming` phases.
- New `/whip` slash command: works in `Streaming` (the first slash command
  whitelisted to bypass the existing streaming-mode gating).
- When invoked during `Streaming`: send `AgentCommand::AppendInput(phrase)`
  so the agent prepends the phrase as `[Additional context from user]: …`
  on the next LLM call. **No cancel, no new turn.**
- 5-row, ~500ms ASCII whip-sweep animation rendered via a new `WhipOverlay`
  Modal; crack flash + phrase fly-in on the final frames.
- One `🐎 whip: <phrase>` scrollback marker per invocation (persistent visual
  trace independent of the animation).
- Config `[whip]` section: `enabled`, `cooldown_ms`, `phrases` override.
- 1-second cooldown between successive fires.

**Out of scope (YAGNI)**

- Mouse-driven physics (Verlet simulation, cursor tracking) — the W3 option
  from brainstorming was rejected as 4-5 days of work for a novelty feature.
- Audio / terminal bell (`\x07` is widely disabled, UX is poor).
- Electron OpenWhip integration — keep atomcode self-contained; in-process
  `AppendInput` works over SSH, doesn't need platform keyboard APIs.
- Automatic "slow turn" detection. The user decides when it's slow. A
  future iteration could add thresholds on `UiState::turn_elapsed()`, but
  v1 ships purely user-initiated.
- Persistent session-log trace beyond what `AppendInput` already gives
  (whip phrases ride the LLM prefix; they are not stored as standalone
  user messages).

## 3. Architecture

### 3.1 Module layout

```
crates/atomcode-tuix/src/
├── whip/
│   ├── mod.rs          — public API: fire_whip, Cooldown, WhipSession
│   ├── phrases.rs      — DEFAULT_PHRASES + pick_phrase
│   └── anim.rs         — frame(i) -> 5-row buffer + phrase row
└── modals/
    └── whip_overlay.rs — Modal impl; tick-driven frame advance
```

### 3.2 Invocation paths

Both paths converge on a single `whip::fire_whip(ctx, active_modal, state)`:

**Path A — `Ctrl+G`**
```
ReaderThread → KeyEvent(Ctrl+G)
  → input/key_action.rs::classify → Action::Whip
  → event_loop/mod.rs::handle_{idle,streaming}_key matches Action::Whip
  → whip::fire_whip(...)
```

**Path B — `/whip` slash command**
```
User types "/whip" + Enter during Streaming
  → parse_slash_line → ("whip", "")
  → streaming gating in event_loop/mod.rs (currently ~line 1399):
      is_known_slash && phase == Streaming → queue as message
    CHANGE: if cmd == "whip", skip the queue and directly call
            execute_slash_command
  → commands.rs::"whip" arm → whip::fire_whip(...)
```

`fire_whip` does:

1. Check `ctx.config.whip.enabled`; if false → no-op.
2. Check `state.phase != UiPhase::Approval` (agent is waiting on tool
   approval, not slow); if Awaiting → no-op with scrollback notice
   `  🐎 whip disabled during approval`.
3. Check cooldown via `ctx.last_whip_at`; if within `cooldown_ms` → no-op
   (silent; no animation, no AppendInput).
4. Check `active_modal.is_some()` (already in picker); if so → no-op.
5. `phrase = phrases::pick_phrase(&ctx.config.whip.phrases)`.
6. Push `UiLine::CommandOutput("  🐎 whip: {phrase}\n")` to scrollback.
   Append `  (no turn running)` suffix when `state.phase != Streaming`.
7. If `state.phase == UiPhase::Streaming`: send
   `AgentCommand::AppendInput(phrase.clone())`.
8. Install `WhipOverlay::open(phrase, Instant::now())` into `active_modal`
   (unless `!caps.tty` or `cols < 30` — those skip the overlay).
9. Update `ctx.last_whip_at = Some(Instant::now())`.

### 3.3 LoopCtx additions

```rust
pub struct LoopCtx {
    …existing fields…
    /// Timestamp of the last whip fire (any path). None = never fired.
    /// Used for the 1-second cooldown check. Initialised to None in
    /// `LoopCtx` construction in `lib.rs::run`.
    pub last_whip_at: Option<Instant>,
}
```

### 3.4 Frame advance

Event loop gains one more `tokio::time::interval` at 33ms. On each tick:

- If `active_modal` is `WhipOverlay`, call `overlay.advance(now)`.
- Overlay compares `now - started_at` against the frame schedule and:
  - Decides current frame index.
  - Repaints via `renderer.render(UiLine::WhipFrame { rows, phrase })`.
  - If `frame_index >= 15` or elapsed > total duration → returns a
    "done" signal; event loop sets `active_modal = None`.

## 4. Components

### 4.1 `whip::phrases`

```rust
const DEFAULT_PHRASES: &[&str] = &[
    "FASTER", "GO FASTER", "Speed it up",
    "Work harder clanker", "Move it",
    "快点", "别磨蹭", "加速", "动起来", "赶紧的",
];

pub fn pick_phrase(user_override: &[String]) -> String {
    pick_phrase_with_rng(user_override, &mut rand::thread_rng())
}

/// Testable seam: callers can pass a seeded `ChaCha8Rng` to get
/// deterministic selection in tests.
pub fn pick_phrase_with_rng<R: rand::Rng>(
    user_override: &[String],
    rng: &mut R,
) -> String {
    if user_override.is_empty() {
        DEFAULT_PHRASES[rng.gen_range(0..DEFAULT_PHRASES.len())].to_string()
    } else {
        user_override[rng.gen_range(0..user_override.len())].clone()
    }
}
```

User-config `phrases` **fully replaces** the defaults (no merge). Empty
array is treated as unset.

### 4.2 `whip::anim`

```rust
pub struct FrameBuf {
    /// 5 rendered rows, each row is a display string (already width-aware).
    pub rows: [String; 5],
    /// The crack phrase to display in row 4. None = not yet shown.
    pub phrase: Option<String>,
    /// When true the renderer should draw this frame with reverse video
    /// on non-empty cells (crack flash).
    pub flash: bool,
}

pub fn frame(
    frame_idx: u16,
    width: u16,
    phrase: &str,
    palette: &Palette,
) -> FrameBuf;
```

Curve math (per-frame):
```
progress  = frame_idx / 11
amplitude = 1.5 * sin(π * progress)
reach     = width * progress
for x in 0..reach:
    t = x / width
    y = amplitude * sin(2π*t - π*progress)
    row = 2 + round(y)  // baseline row 2, ±1 row from there
    cell[row][x + margin] = palette.pick(|dy/dx|)
```

Frame schedule (15 frames, 33ms each = ~495ms total):

| frame | role |
|------:|------|
| 0 | coil, handle only |
| 1–2 | coil expands |
| 3–6 | curve grows, amplitude climbs |
| 7–9 | curve sweeps right, tail at `~≈` |
| 10 | tip reaches far edge, `⟶` |
| 11 | **crack**: `flash = true`, `💥` at tip, phrase begins in row 4 |
| 12–14 | curve decays to `~`, phrase holds |
| 15 | all rows cleared → overlay closes |

Character palette (all width-1 except `💥` which is width-2 and already
handled by atomcode's `width.rs`):

```
handle  ╫
coil    ╭ ╮ ╯ ╰
body    ─ ╴ ╶ ╱ ╲
fast    ~ ≈ ⋍
tip     ⟶ » ⇢
crack   💥 ✦ ※
```

Colors (ANSI 256-color preferred, graceful degrade):

- Body: grayscale gradient 236 → 250 (dark to light along length = speed cue)
- Crack frame: reverse video on all non-empty cells; `💥` = bright yellow;
  phrase row = bright red + bold
- `NO_COLOR=1` or `caps.colors < 256`: drop colors, keep bold on crack frame

### 4.3 `modals::whip_overlay::WhipOverlay`

Implements `Modal` but is display-only:

- `handle_key`: returns `ModalAction::Close` only for `Esc` (user may
  dismiss early); any other key → `Continue`, key is not re-dispatched
  (animation swallows input for its ~500ms lifetime).
- `draw`: render the current frame via `UiLine::WhipFrame`.
- New method `advance(now) -> ModalAction`: compares elapsed to schedule;
  returns `Close` when frame >= 15.

### 4.4 `render::UiLine::WhipFrame`

New variant:
```rust
WhipFrame { rows: [String; 5], phrase: Option<String>, flash: bool }
```

`retained.rs` paints it into the 5 rows immediately above the status line
(same vertical slot as `SessionPicker`'s menu area). Does **not** enter
scrollback; overwritten each frame; cleared on modal close.

### 4.5 Input handling

`crates/atomcode-tuix/src/input/key_action.rs`:
```rust
(KeyCode::Char('g'), true) => Action::Whip,
```

`Action::Whip` handled identically in `handle_idle_key` and
`handle_streaming_key` — both call `whip::fire_whip`. Not added to
`handle_approval_key` (the Awaiting guard in `fire_whip` would reject it
anyway, but not wiring it there avoids the wasted dispatch).

### 4.6 Slash command registration

`crates/atomcode-tuix/src/commands.rs`:
```rust
Command { name: "whip", desc: "Urge the agent (also: Ctrl+G)" },
```

`crates/atomcode-tuix/src/event_loop/commands.rs`:
```rust
"whip" => whip::fire_whip(ctx, active_modal, state)?,
```

`crates/atomcode-tuix/src/event_loop/mod.rs` streaming-gating change:
```rust
// Around line 1399-ish:
if is_known_slash && matches!(state.phase, UiPhase::Streaming) {
    if cmd == "whip" {
        // The ONE command that's valid mid-stream.
        execute_slash_command(cmd, arg, state, ctx, renderer, active_modal)?;
    } else {
        // existing behaviour: queue as message + hint
    }
}
```

## 5. Configuration

```toml
[whip]
enabled = true          # default; false disables both Ctrl+G and /whip
cooldown_ms = 1000      # default
phrases = []            # default; empty/unset uses built-in pool
```

`atomcode-core/src/config/mod.rs` gets a matching `WhipConfig` struct
with `#[serde(default)]` so existing `config.toml` files keep working
untouched.

## 6. Edge Cases

| Case | Behaviour |
|------|-----------|
| `!caps.tty` (pipe, CI, dumb terminal) | skip overlay; scrollback line + `AppendInput` (if streaming) still happen |
| `cols < 30` | same as above — overlay needs at least 30 columns to look right |
| `caps.colors < 256` or `NO_COLOR=1` | monochrome overlay, crack frame still reverses |
| another modal already open | `fire_whip` returns early silently |
| `state.phase == UiPhase::Approval` (tool approval) | no-op + "whip disabled during approval" scrollback line |
| `state.phase == UiPhase::Suspended` (shell handoff / oauth) | no-op; input is going to child process anyway |
| cooldown not elapsed | silent no-op |
| `enabled = false` | Ctrl+G falls through as inert key; `/whip` emits "whip disabled in config" |
| session resume | whip phrases never enter `session.messages`, so nothing to replay |

## 7. Testing

Unit tests (each in the file that owns the type):

- `phrases::pick_phrase` with seeded RNG is deterministic
- `phrases::pick_phrase` honours `user_override` exclusively when non-empty
- `anim::frame(i)` produces the expected non-empty cell count at
  progress boundaries (0, 5, 10, 11, 14)
- `Cooldown::try_fire(now)` boundary: fires at T+0, blocks at
  T+999ms, fires at T+1000ms
- `fire_whip` early-returns on `caps.tty == false` and `cols < 30`

Integration tests (`crates/atomcode-tuix/tests/whip_integration.rs`):

- Ctrl+G during `Streaming` → `AgentCommand::AppendInput` appears on the
  mock agent's cmd channel
- Ctrl+G during `Idle` → no `AppendInput` sent; scrollback has the marker
- Ctrl+G during `Awaiting` → no `AppendInput`, no overlay
- `/whip` during `Streaming` → same outcome as Ctrl+G (bypasses gating)
- Two Ctrl+G within 500ms → second is silent no-op
- `enabled = false` → neither path has any effect

Snapshot tests for `anim::frame(0)`, `frame(5)`, `frame(10)`, `frame(11)`
render a fixed-width canvas to a `String` and compare against a committed
fixture. Not a visual regression test — purely char-grid equality.

Explicitly **not** tested: visual aesthetics; end-to-end TUI recording.

## 8. Open Questions

- Should `last_whip_at` reset when `/session` clears the conversation?
  Leaning **no** — cooldown is a rate-limit on the user, not on the
  session. Resets naturally feel surprising.
- Do we want a `ATOMCODE_WHIP_SEED` env var for deterministic phrase
  selection in demos / recordings? Skip for v1; trivial to add later.
- Should the scrollback marker include elapsed turn time
  (`🐎 whip: FASTER (after 8.2s)`)? Adds minor info density but fits
  the feature thesis. **Decision: include it when streaming**; omit for
  idle fires where there's no meaningful elapsed.

## 9. Implementation Order

1. `atomcode-core`: add `WhipConfig` to `config::Config` with serde defaults.
2. `atomcode-tuix::whip` module skeleton (phrases + Cooldown, no anim yet).
3. `Action::Whip` + idle/streaming handler wiring; scrollback line only
   (no overlay) — proves end-to-end `AppendInput` path first.
4. `UiLine::WhipFrame` variant + retained.rs paint path.
5. `WhipOverlay` modal + anim module + 33ms tick in event loop.
6. Slash command `/whip` + streaming whitelist change.
7. Tests in parallel with each step.

Steps 1-3 are shippable alone (whip without animation) if we want to land
behaviour early and layer visuals on top.
