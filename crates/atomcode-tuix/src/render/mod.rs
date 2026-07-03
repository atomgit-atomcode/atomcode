// crates/atomcode-tuix/src/render/mod.rs
pub mod cell;
pub mod plain;
pub mod qr;
pub mod retained;
pub mod screen;
pub mod theme;
pub mod worker;

use std::time::Duration;

/// Boundary marker for an originated message in the body buffer. Drives
/// "jump to prev/next message" navigation keys. Marked at push time;
/// kept in sync when body_lines drains from the front.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkKind {
    User,
    Assistant,
    ToolCall,
    ToolResult,
}

#[derive(Debug, Clone, Copy)]
pub struct MessageMark {
    /// Index into the renderer's visible body buffer (`Vec<Vec<Cell>>`).
    /// Drives "jump to message" — viewport_top is compared against this.
    pub line_idx: usize,
    pub kind: MarkKind,
}

/// Semantic line to render. Renderer implementations translate this to bytes.
///
/// Permanent lines (User, Assistant, ToolCall, ToolResult, Diff, Approval,
/// Error, Blank) all enter scrollback. Spinner and InputPrompt are transient.
#[derive(Debug, Clone)]
pub enum UiLine {
    Welcome {
        model: String,
        working_dir: String,
    },
    User(String),
    /// A user message and its image echoes rendered as one append-only
    /// group. Retained renderers must not commit a temporary spacer between
    /// the text and attachments because that scroll cannot be undone.
    UserWithAttachments {
        text: String,
        attachments: Vec<usize>,
    },
    AssistantText(String),
    /// LLM reasoning/thinking content (displayed in gray/dimmed style)
    ReasoningText(String),
    AssistantLineBreak,
    ToolCall {
        name: String,
        detail: String,
    },
    /// Animated tool-call line. Pushed on `AgentEvent::ToolCallStarted`
    /// instead of the static `ToolCall`, so the user sees the call land
    /// the moment the model commits to it AND its leading icon ticks in
    /// lockstep with the footer spinner via the live-row mechanism (see
    /// `RetainedRenderer::push_or_update_inflight_tool`). Switched to a
    /// static `▸` icon by `ToolCallCommit` once the matching result
    /// lands, freeing the live-row slot for the spinner to resume.
    ToolCallInFlight {
        id: String,
        name: String,
        detail: String,
    },
    /// Freeze the most recent `ToolCallInFlight` row to its final
    /// static `▸` icon. Emitted right before `ToolResult` so the
    /// bottom body row stops animating exactly when the result is
    /// about to be appended below it.
    /// If `call_id` is provided, only commits if the inflight_tool matches.
    ToolCallCommit {
        call_id: Option<String>,
    },
    /// Push a parallel-tool batch as a live multi-row group: one
    /// header line + N child rows (one per tool call), all visible
    /// from the start. Subsequent `ToolGroupChildUpdate` events find
    /// child rows by `call_id` and update them in place (CC-style
    /// ✓ light-up). The group is "live" only as long as it remains
    /// the bottom of body_lines; any other body push freezes it (in
    /// place forever, but no further child updates take effect).
    ToolGroupRender {
        batch_id: String,
        header: String,
        children: Vec<ToolGroupChild>,
    },
    /// Update one child row inside an active live-group. Renderer
    /// finds the row keyed by `call_id` and CUPs to its terminal
    /// position to rewrite. Falls back to no-op if the group has been
    /// frozen (other content was pushed below it).
    ToolGroupChildUpdate {
        batch_id: String,
        call_id: String,
        new_text: String,
    },
    /// One-shot summary line for a completed tool batch — rendered
    /// with bold + brand-color emphasis so it stands out as the
    /// "this is what happened" anchor (mirrors CC's task-completion
    /// summary visual). Used by both ToolBatchCompleted and
    /// SubAgentDispatchEnd.
    ToolGroupSummary {
        text: String,
    },
    ToolResult {
        success: bool,
        summary: String,
    },
    DiffLine {
        added: bool,
        text: String,
    },
    /// A batch of diff lines emitted in a single render call. Use this
    /// instead of N individual `DiffLine` renders when a tool result
    /// carries many changed lines — each `DiffLine` triggers a full
    /// erase_footer + redraw_footer cycle, so 50 diff lines translate
    /// into 50 footer redraws and tens of KB of ANSI, blocking the
    /// event loop long enough to freeze the spinner. `DiffBlock` does
    /// one erase + N writes + one redraw.
    DiffBlock(Vec<DiffEntry>),
    ApprovalPrompt {
        tool: String,
        detail: String,
    },
    Error(String),
    /// Non-fatal advisory line (yellow). Visually distinct from `Error`
    /// so the user can tell "we saw something fishy and want you to
    /// know" apart from "the turn died." Currently used by the OpenAI
    /// provider's truncation detector.
    Warning(String),
    /// Dim, non-bold informational line with no forced prefix. Used for
    /// notable but non-alarming status lines that should not read as
    /// warnings or errors — e.g. a rate-limit pause announcement. Rendered
    /// in DarkGrey (same palette as `CompactionMark`) so it recedes into
    /// the scrollback without grabbing attention.
    Muted(String),
    /// A compaction occurred here — a dim, left-aligned dash rule marking the
    /// scrollback point where history was folded/summarized. Unified across
    /// auto-compaction and manual `/compact`. Payload is the localized label
    /// (e.g. "已压缩 · 摘要 12 条 · ~48.2K→~9.1K"); renderers wrap it in a dash
    /// rule honoring the terminal's unicode caps. Permanent (enters scrollback).
    CompactionMark(String),
    TurnCancelled,
    TurnComplete,
    /// Legacy single-line spinner (kept for tests / PlainRenderer fallback).
    /// During Streaming the event loop emits `StreamingBox` instead so the
    /// spinner sits ABOVE the input box rather than inside it.
    Spinner {
        frame: &'static str,
        label: String,
    },
    /// Clear the current transient line (prepares for a permanent write).
    ClearTransient,
    /// Draw the input prompt "> " + current buffer (transient, idle).
    /// When `menu` is Some, a command palette is drawn above the box.
    /// `cursor_byte` is a byte offset into `buf` — the renderer wraps
    /// `buf` to the available input width and derives the 2D cursor
    /// position (row, col) itself so the input box can grow multi-line
    /// when the user exceeds a single row.
    InputPrompt {
        buf: String,
        cursor_byte: usize,
        menu: Option<MenuPayload>,
        status: StatusLine,
        /// Marker numbers (`N` from `[Image #N]`) that actually have
        /// image bytes ready to ship — either freshly attached this
        /// turn or recalled from cache via arrow-up. Renderers cross-
        /// reference each marker against `buf` and draw a `└ [Image #N]`
        /// preview row for the intersection right under the input box,
        /// so users can tell "real attachment" from "literal text" at
        /// a glance, before submit. Empty means no preview rows. Only
        /// the main idle / streaming compose paths populate this; modal
        /// flows that reuse `InputPrompt` for text entry pass `Vec::new()`.
        attachments: Vec<usize>,
    },
    /// Streaming chrome: spinner line above a (possibly multi-line)
    /// input box. Same `cursor_byte` semantics as `InputPrompt`.
    /// When `menu` is Some (user typed `/` into the type-ahead buffer
    /// mid-stream), the slash-command palette is drawn above the box
    /// in place of the spinner — same rendering path as `InputPrompt`.
    StreamingBox {
        buf: String,
        cursor_byte: usize,
        frame: &'static str,
        label: String,
        status: StatusLine,
        menu: Option<MenuPayload>,
        /// Same semantics as `InputPrompt::attachments` — type-ahead
        /// during streaming can carry pasted attachments too, so the
        /// preview path needs to fire here as well.
        attachments: Vec<usize>,
    },
    /// User pressed Enter: commit the current InputPrompt to scrollback.
    InputCommit,
    /// Slash-command output (arbitrary text, already sanitised by caller).
    CommandOutput(String),
    /// Image-attachment echo (`└ [Image #N]`). Emitted right after the
    /// `UiLine::User` row that contains the matching `[Image #N]`
    /// marker, so each renderer can align the `└` glyph at the same
    /// column as the `[` of the marker in the user message above
    /// (col 2). A dedicated variant rather than `CommandOutput` so
    /// alignment stays consistent across renderers — retained's
    /// `push_body_text` auto-prefixes PAD_COL (2 spaces) but
    /// alt-screen's `push_command_output` does not, so the same
    /// CommandOutput payload would land at col 2 in one and col 4
    /// (or col 0) in the other.
    ImageAttachment(usize),
    /// One-line success notice for vision-preprocessor OCR. Renders as
    /// `{msg}  {model}` where `msg` uses the default text style and
    /// `model` uses the Muted (gray) role — visually distinct from
    /// failure (yellow `! ...`) and from arbitrary command output.
    /// The actual VL description is intentionally NOT shown in the UI;
    /// it still rides into conversation history for the main model.
    VisionPreprocessSuccess {
        msg: String,
        model: String,
    },
    /// A visible separator between turns: `────── {label} ──────`.
    TurnSeparator {
        label: String,
    },
    /// Overlay modal: a floating window drawn on top of body+footer.
    /// RetainedRenderer paints this after the normal frame; PlainRenderer ignores it.
    ModalOverlay {
        title: String,
        lines: Vec<String>,
        scroll: usize,
        total: usize,
        win_width: u16,
        win_height: u16,
    },
    /// Clear the overlay modal and restore the underlying frame.
    ModalOverlayClear,
}

pub trait Renderer: Send {
    /// Emit one UiLine. Implementations may batch internally; call `flush()` to force.
    fn render(&mut self, line: UiLine);
    fn flush(&mut self);
    /// Shutdown: disable bracketed paste, disable raw mode, etc.
    fn shutdown(&mut self);
    /// Forget all cached rendering state (footer rows, last footer snapshot,
    /// assistant-text mid-line buffer, markdown parser) AND clear the
    /// physical terminal screen. Used by callers that hand control back
    /// to a non-TUI process (e.g. the blocking OAuth flow in /login)
    /// and then want a clean slate — without this, the next render
    /// tries to `erase_footer` at a position the terminal cursor is no
    /// longer at, corrupting every subsequent ANSI cursor move.
    fn reset(&mut self);

    /// Wipe the physical terminal with `\x1b[2J\x1b[H` and flush.
    /// **Does not** touch cached footer/stream state — callers that want a
    /// full state wipe should call `reset()` instead. Use this when only
    /// the visible scrollback should be cleared (e.g. the `/clear`
    /// command after which the footer immediately redraws).
    fn clear_screen(&mut self);

    /// Open a single DECSET 2026 synchronized-output envelope spanning the
    /// burst of operations up to the matching `end_sync()`. Used by the
    /// `/resume` replay so the screen wipe + full-transcript re-emit paint
    /// as ONE atomic update on capable hosts instead of visibly blanking
    /// and re-scrolling (the flicker). Between the two calls, per-frame
    /// envelopes are suppressed so they can't end the batch early.
    ///
    /// Default no-op: renderers that don't use synchronized output
    /// (PlainRenderer, pipe mode, tests) just emit their writes as usual.
    fn begin_sync(&mut self) {}

    /// Close the envelope opened by `begin_sync()` (after landing the final
    /// frame inside it). Default no-op. Must be paired with `begin_sync()`.
    fn end_sync(&mut self) {}

    /// Suppress automatic clipboard copy during history replay so that
    /// `/resume`, `/undo` and `atomcode -c` don't overwrite the user's
    /// clipboard or inject stale "Copied" hints into the replay output
    /// (issue #699). Default no-op — only the retained renderer implements
    /// this.
    fn set_suppress_auto_copy(&mut self, _suppress: bool) {}

    /// Enable/disable the code-block auto-copy feature (issue #699). Default
    /// OFF (opt-in via `config.ui.auto_copy_code_blocks` / `ATOMCODE_AUTO_COPY`),
    /// set once at startup. Default no-op — only the retained renderer implements it.
    fn set_auto_copy_enabled(&mut self, _enabled: bool) {}

    /// Set the terminal window/tab title. Default no-op — only the
    /// interactive retained renderer implements this, so title bytes never
    /// leak into piped/plain (non-TTY) output. `title` is already sanitised
    /// (see `crate::title::session_terminal_title`).
    fn set_title(&mut self, _title: String) {}

    /// Hand the terminal off to a non-TUI child process (blocking OAuth
    /// flow, `/shell`, etc.): disable raw mode + bracketed paste, finish
    /// any pending writes. After this returns, the child is free to use
    /// the terminal in cooked mode; `resume_from_external()` must be
    /// called before any further `render()` calls.
    fn suspend_for_external(&mut self);

    /// Take the terminal back after `suspend_for_external()`: re-enable
    /// raw mode + bracketed paste AND call `reset()` to wipe the cached
    /// state (the child wrote to stdout in cooked mode, so our cursor
    /// tracking is now lying).
    fn resume_from_external(&mut self);

    /// Paint any throttled payload that's been sitting in the deferred
    /// queue past its throttle window. Called from the event loop on a
    /// ~50fps timer so the "trailing edge" of a burst of input renders
    /// actually lands — without this tick a lone stale payload would
    /// stay invisible until the next unrelated render arrived.
    ///
    /// Implementations without throttling (e.g. PlainRenderer) can
    /// treat this as a flush.
    fn flush_deferred(&mut self);

    /// Returns (and clears) whether a body overflow scrolled the whole
    /// viewport — footer included — up one row since the last call. The
    /// render worker calls this after each command and, when true, repaints
    /// the footer immediately via `flush_deferred` instead of waiting for the
    /// event loop's next ~5ms deferred tick, so the footer doesn't visibly lag
    /// the scroll on hosts that don't vsync-coalesce (native Win10 conhost /
    /// pwsh7). Default `false`: only the retained renderer scrolls a viewport;
    /// plain/pipe and the cross-thread proxy renderer never do.
    fn take_pending_scroll_flush(&mut self) -> bool {
        false
    }

    /// Remove the most recent `ApprovalPrompt` body row, if the tail
    /// row is one. Called by the event loop after the user responds
    /// Y/A/N so the prompt stops sitting in the body above the footer.
    /// Default: no-op — implementations that stream body lines to
    /// stdout (plain/pipe mode) can't retract them.
    fn pop_approval_prompt(&mut self) {}

    /// Terminal window was resized to `(cols, rows)`. The retained
    /// backend uses this to re-flow body width and reposition the
    /// pinned footer; non-geometry-sensitive backends (Plain, tests)
    /// keep the no-op default.
    fn on_resize(&mut self, _cols: u16, _rows: u16) {}

    /// Scroll the body viewport up (negative `delta`) or down
    /// (positive `delta`) by `delta` rows.
    ///
    /// Default no-op for renderers that delegate scrollback to the
    /// host terminal (RetainedRenderer's append-only path; PlainRenderer
    /// streaming to stdout).
    fn scroll_body(&mut self, _delta: i32) {}

    /// Jump the body viewport to the absolute top / bottom of
    /// scrollback. Used for Home / End key handling.
    fn scroll_body_to_top(&mut self) {}
    fn scroll_body_to_bottom(&mut self) {}

    /// Update the cached welcome banner's model / working_dir fields in
    /// place and trigger a repaint of the banner rows. Used after the
    /// QR-onboarding `/codingplan` claim finishes: the banner was
    /// painted at the top of scrollback with `model=""` (the claim
    /// hadn't picked a default provider yet) — once the claim writes
    /// `ctx.model_name`, this hook splices the resolved model into the
    /// existing banner rows so the user doesn't see a permanently
    /// blank model bullet.
    ///
    /// Default no-op: renderers without a retained body buffer can't
    /// edit already-emitted rows in place.
    fn refresh_welcome_banner(&mut self, _model: &str, _working_dir: &str) {}

    /// Jump body viewport to the prev/next message boundary. No-op when no
    /// such boundary exists in the configured direction.
    fn scroll_to_prev_message(&mut self) {}
    fn scroll_to_next_message(&mut self) {}
    fn scroll_to_prev_user_message(&mut self) {}
    fn scroll_to_next_user_message(&mut self) {}
}

/// Visual style for the menu popup. Drives whether the renderer prefixes
/// each row with `/` (slash-command palette) or `+ ` (file/dir mention),
/// and which marker indicates the selected row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MenuKind {
    /// Default: rows shown as `/<name>`, selected row marked `▸`.
    #[default]
    SlashCommand,
    /// `@`-mention popup: rows shown as `+ <path>`, no slash prefix.
    /// Selected row uses reverse-video only (no extra arrow).
    AtMention,
    /// `$`-trigger skills picker. Rows show the bare skill name + description,
    /// no `/`, `/skills`, or `$` prefix; selection marked with `▸`.
    Skill,
    /// Two-column list: name left-aligned, desc right-aligned,
    /// selected row uses reverse-video (no prefix, no arrow).
    /// Used by session picker.
    /// `row_prefix` is prepended before the name (e.g. `/`).
    /// `selected_marker` is shown before the prefix for the selected row;
    /// unselected rows get `display_width(marker)` spaces.
    TwoColumn {
        row_prefix: &'static str,
        selected_marker: &'static str,
    },
}

impl MenuKind {
    /// Max visible rows for this menu kind. Both `paint_footer` and
    /// `current_footer_rows` use this so the estimate matches actual
    /// rendering.
    pub fn max_visible_rows(&self, screen_height: usize, item_count: usize) -> usize {
        match self {
            MenuKind::SlashCommand | MenuKind::AtMention | MenuKind::Skill => item_count.min(4),
            // Window cap is `max(h/2, 4)`, but never reserve more rows than
            // there are items — `paint_footer` only paints `item_count`
            // rows when there are fewer than the cap, so `.max(4)` MUST
            // apply to the cap, not the final value, or `current_footer_rows`
            // over-estimates the footer height for short lists (< 4 items)
            // and desyncs from the actual paint.
            MenuKind::TwoColumn { .. } => item_count.min((screen_height / 2).max(4)),
        }
    }
}

/// Slash-command palette payload: filtered entries + which one is selected.
#[derive(Debug, Clone, Default)]
pub struct MenuPayload {
    pub items: Vec<(String, String)>, // (name, desc)
    pub selected: usize,
    /// Visual style. Defaults to `SlashCommand`; existing call sites
    /// using `MenuPayload { items, selected }` get the slash style for
    /// free. `@`-mention path explicitly sets `MenuKind::AtMention`.
    pub kind: MenuKind,
}

/// Persistent status line drawn directly below the input box — CC-style
/// Severity classification for the right-aligned status hint.
/// Warning → Role::Error (red, e.g. "no provider", "model retired").
/// Info → Role::Muted (dim, e.g. "new version available", drift notice).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HintSeverity {
    #[default]
    Warning,
    Info,
}

/// "model · cwd · ctx_used / ctx_window" chrome. Visible in both Idle
/// and Streaming phases so the user always sees what provider is active
/// and how much of the context window is currently in use. Cumulative
/// session token totals are NOT shown here — they're per-session and
/// don't tell the user whether the next turn is at risk of overflow.
/// `ctx_used` answers "what does the model see right now"; `ctx_window`
/// is the cap. Together they answer "how close are we to compaction".
#[derive(Debug, Clone, Default)]
pub struct StatusLine {
    pub model: String,
    pub cwd: String, // HOME replaced with "~"
    /// Tokens currently in the model's context (last turn's `sent_tokens`).
    /// Pre-first-turn this is 0; the renderer hides the field then.
    pub ctx_used: usize,
    /// Provider's context window (cap). 0 when not yet known — renderer
    /// falls back to a bare "12.3k tok" display in that case.
    pub ctx_window: usize,
    /// Right-aligned passive hint with severity. `Warning` renders red
    /// (no-provider nudge, CodingPlan model-missing); `Info` renders
    /// muted (upgrade banner, CodingPlan drift notice). None → no hint.
    pub hint: Option<(String, HintSeverity)>,
    /// Left-aligned mode indicator, prepended before `model`. Present
    /// only when the user explicitly switched to a non-default agent
    /// mode (Plan today; conceivably others later). `None` for the
    /// default Build mode so the status row doesn't gain noise for
    /// the common case. Renders in brand color (Role::Brand) to draw
    /// the eye — switching modes changes whether file edits and shell
    /// run, so the user wants this prominent.
    pub mode_indicator: Option<String>,
    /// Right-aligned bypass indicator, appended after `hint` on the
    /// right side of the status row. Shown when
    /// `--dangerously-skip-permissions / -y` is active, rendering a
    /// yellow warning badge so the user is always aware that all tool
    /// calls are auto-approved. Kept separate from `mode_indicator`
    /// (left-aligned PLAN/Build badge) so BYPASS does not displace
    /// the mode indicator.
    pub bypass_indicator: Option<String>,
    /// Current session display name, shown as a right-aligned cyan
    /// pill overlaid on the input box's top rule. `Some` only after
    /// the user has explicitly run `/rename` (Session::user_renamed) —
    /// auto-named / default sessions leave this `None` to keep the
    /// chrome quiet on fresh conversations.
    pub session_name: Option<String>,
    /// Current reasoning_effort for the active provider's model.
    /// None = not set (API uses its own default). Cycled via Ctrl+T.
    pub reasoning_effort: Option<String>,
    /// When an autonomous `/goal` loop is active, this carries its live status
    /// for the DEDICATED footer goal row (its own full-width line above the
    /// status row). `None` ⇒ no goal running, row omitted. Previously this was
    /// a pre-formatted suffix crammed onto the shared status line, where it was
    /// the first thing truncated under a hint / narrow terminal and omitted the
    /// condition text — so users couldn't reliably see the goal while tool
    /// output scrolled. Its own row fixes that.
    pub goal: Option<GoalStatus>,
}

/// Live status of an active autonomous `/goal` loop, rendered on the dedicated
/// footer goal row. The renderer width-truncates `condition` to fit; `round`
/// and the elapsed time always survive (see `format_goal_row`).
#[derive(Debug, Clone)]
pub struct GoalStatus {
    /// The goal condition text (truncated with `…` to fit the row width).
    pub condition: String,
    /// Round number AS DISPLAYED — 1-based (the first attempt reads `round 1`).
    /// The caller adds 1 to the engine's 0-based internal round.
    pub round: u32,
    /// Wall-clock seconds since the goal was set.
    pub elapsed_secs: u64,
}

/// One line in a diff batch. `added = true` renders as `+`, false as `-`.
#[derive(Debug, Clone)]
pub struct DiffEntry {
    pub added: bool,
    pub text: String,
}

/// One child entry inside a `UiLine::ToolGroupRender` payload. `call_id`
/// is the model-supplied tool-call id; `text` is the display string the
/// renderer initially prints (e.g. `↳ Read File foo.rs`). Subsequent
/// `ToolGroupChildUpdate` events with the same call_id rewrite this row
/// in place (e.g. to `↳ ✓ Read File foo.rs`).
#[derive(Debug, Clone)]
pub struct ToolGroupChild {
    pub call_id: String,
    pub text: String,
}

/// Wrap a compaction marker label in a dash rule: `─── {label} ───` (unicode)
/// or `--- {label} ---` (ASCII fallback for fonts lacking box-drawing — the
/// same `unicode_symbols` gate the spinner `◐`→`|/-\` and ellipsis `…`→`...`
/// use). Pure, so the wrapping is unit-tested independent of a renderer.
pub fn compaction_rule(label: &str, unicode: bool) -> String {
    let dash = if unicode { "───" } else { "---" };
    format!("{dash} {label} {dash}")
}

/// Convert a Duration to a short label, scaling the unit with magnitude:
/// `340ms` (< 1s) → `23.1s` (< 1min) → `2m9s` (< 1h) → `1h5m9s` (≥ 1h).
/// Sub-minute keeps one decimal; at minute scale and above the sub-second
/// part is dropped (it's noise next to whole minutes/hours).
pub fn fmt_dur(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        return format!("{}ms", ms);
    }
    let total = d.as_secs();
    if total < 60 {
        return format!("{:.1}s", d.as_secs_f64());
    }
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h == 0 {
        format!("{m}m{s}s")
    } else {
        format!("{h}h{m}m{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_rule_wraps_label_unicode() {
        assert_eq!(compaction_rule("已压缩 · 摘要 2 条", true), "─── 已压缩 · 摘要 2 条 ───");
    }

    #[test]
    fn compaction_rule_wraps_label_ascii_fallback() {
        assert_eq!(compaction_rule("done", false), "--- done ---");
    }

    #[test]
    fn fmt_dur_scales_unit_with_magnitude() {
        assert_eq!(fmt_dur(Duration::from_millis(340)), "340ms");
        assert_eq!(fmt_dur(Duration::from_millis(999)), "999ms");
        // sub-minute keeps one decimal (the screenshot's `23.1s`)
        assert_eq!(fmt_dur(Duration::from_secs_f64(23.1)), "23.1s");
        assert_eq!(fmt_dur(Duration::from_secs_f64(59.4)), "59.4s");
        // minute scale drops the decimal (`129.8s` → `2m9s`)
        assert_eq!(fmt_dur(Duration::from_secs_f64(129.8)), "2m9s");
        assert_eq!(fmt_dur(Duration::from_secs(60)), "1m0s");
        assert_eq!(fmt_dur(Duration::from_secs(3599)), "59m59s");
        // hour scale shows all three components
        assert_eq!(fmt_dur(Duration::from_secs(3600)), "1h0m0s");
        assert_eq!(fmt_dur(Duration::from_secs(3661)), "1h1m1s");
    }

    fn two_column() -> MenuKind {
        MenuKind::TwoColumn {
            row_prefix: "",
            selected_marker: "▸",
        }
    }

    // `current_footer_rows` reserves `max_visible_rows` rows while
    // `paint_footer` only paints `min(item_count, cap)` rows. For the two
    // to agree, `max_visible_rows` must never exceed `item_count`.
    #[test]
    fn two_column_never_reserves_more_than_item_count() {
        let k = two_column();
        // Short lists: must equal item_count, NOT the 4-row floor.
        assert_eq!(k.max_visible_rows(40, 0), 0);
        assert_eq!(k.max_visible_rows(40, 1), 1);
        assert_eq!(k.max_visible_rows(40, 2), 2);
        assert_eq!(k.max_visible_rows(40, 3), 3);
    }

    #[test]
    fn two_column_caps_at_half_screen_for_long_lists() {
        let k = two_column();
        // 50 items, height 40 → window cap = max(20, 4) = 20.
        assert_eq!(k.max_visible_rows(40, 50), 20);
        // At the cap boundary.
        assert_eq!(k.max_visible_rows(40, 20), 20);
        assert_eq!(k.max_visible_rows(40, 19), 19);
    }

    #[test]
    fn two_column_floor_keeps_at_least_four_on_tiny_screens() {
        let k = two_column();
        // Tiny screen (h/2 = 3) with plenty of items → floor lifts cap to 4.
        assert_eq!(k.max_visible_rows(6, 50), 4);
        // ...but still never more than the item count.
        assert_eq!(k.max_visible_rows(6, 2), 2);
    }

    #[test]
    fn fixed_kinds_cap_at_four() {
        for k in [MenuKind::SlashCommand, MenuKind::AtMention, MenuKind::Skill] {
            assert_eq!(k.max_visible_rows(40, 2), 2);
            assert_eq!(k.max_visible_rows(40, 10), 4);
        }
    }
}
