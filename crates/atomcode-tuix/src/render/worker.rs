// crates/atomcode-tuix/src/render/worker.rs
//
// Render worker — moves terminal I/O off the main event loop.
//
// ## Why
//
// Mac Terminal.app takes 30-60ms to process a full footer ANSI payload.
// When the event loop calls `renderer.render()` directly, that 30-60ms
// blocks the select! loop, which means:
//   - the spinner tick task can't deliver (drops),
//   - the next keystroke can't be read,
//   - agent events queue up behind the render.
//
// `InputThrottle` (see throttle.rs) mitigates the storm by coalescing
// InputPrompt/StreamingBox paints. This worker eliminates the blocking
// at the architectural level: the event loop sends `UiLine`s and
// lifecycle commands into a channel, a dedicated OS thread owns the
// inner renderer and drains the channel. Slow terminal ≠ stalled event
// loop.
//
// ## Sync vs. async lifecycle
//
// Most render calls are fire-and-forget: `render(UiLine)` just enqueues.
// Lifecycle methods that must complete before the caller proceeds —
// `reset`, `clear_screen`, `suspend_for_external`, `resume_from_external`,
// `shutdown` — send a command with an ACK oneshot channel and block
// until the worker reports done. The `/login` OAuth flow for example
// can't tolerate "renderer hasn't flipped raw mode yet" when the child
// process opens the browser.
//
// `flush` and `flush_deferred` are fire-and-forget (no ACK) — order is
// preserved because all commands travel the same channel.
//
// ## Shutdown
//
// `Drop` sends `Shutdown` and joins the thread, guaranteeing the final
// terminal-reset bytes land before `run()` returns. Dropping the sender
// alone would also let the worker exit on the next recv error, but an
// explicit Shutdown gives clean "process the last queued line + flush"
// semantics rather than "drop whatever is still in flight".

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use super::{Renderer, UiLine};

/// Commands sent to the render worker thread.
enum RenderCmd {
    Line(UiLine),
    Flush,
    FlushDeferred,
    /// Terminal resize — fire-and-forget, the worker updates its
    /// internal DECSTBM region and repaints the footer.
    Resize(u16, u16),
    /// Scroll the body viewport by `delta` rows. Negative = up,
    /// positive = down. RetainedRenderer's append-only path leaves
    /// scrollback to the host terminal, so this is effectively a
    /// trait no-op everywhere today.
    ScrollBody(i32),
    /// Jump body viewport to absolute top / bottom of scrollback.
    ScrollBodyToTop,
    ScrollBodyToBottom,
    /// Jump body viewport to the prev/next message boundary.
    /// Fire-and-forget — no ACK needed.
    ScrollToPrevMessage,
    ScrollToNextMessage,
    ScrollToPrevUserMessage,
    ScrollToNextUserMessage,
    /// Open / close the `/resume` replay's single DECSET 2026 envelope.
    /// Fire-and-forget: FIFO ordering on this channel keeps `BeginSync`
    /// before the subsequent `Reset` Ack and `EndSync` after the trailing
    /// `Flush`, exactly as `replay_session` issues them.
    BeginSync,
    EndSync,
    BeginInitialHistoryReplay,
    EndInitialHistoryReplay,
    SetHistoryReplayMaxRows(Option<usize>),
    /// Suppress / restore automatic clipboard copy during history replay
    /// (issue #699 P1-1). Fire-and-forget.
    SetSuppressAutoCopy(bool),
    /// Set the terminal window/tab title. Fire-and-forget — routed through
    /// the worker so the OSC bytes serialize with every other stdout write
    /// (the worker owns stdout; writing from the event-loop thread would
    /// risk interleaving mid-escape-sequence).
    SetTitle(String),
    /// Lifecycle operation requiring an ACK — the worker performs the
    /// op then sends `()` back so the caller can proceed.
    Ack {
        op: AckOp,
        ack: mpsc::Sender<()>,
    },
}

#[derive(Debug, Clone, Copy)]
enum AckOp {
    Reset,
    ClearScreen,
    SuspendForExternal,
    ResumeFromExternal,
    Shutdown,
}

/// Renderer facade that forwards every call to a background OS thread.
/// Implements the `Renderer` trait so the event loop can use it as a
/// drop-in replacement for `AnsiRenderer` / `PlainRenderer` — the wire
/// protocol is the same `UiLine` enum.
pub struct TaskRenderer {
    cmd_tx: mpsc::Sender<RenderCmd>,
    /// Coalesces the 5ms `FlushDeferred` heartbeat: `true` means one is already
    /// queued and undrained, so we skip enqueuing another. Without this, when the
    /// worker's terminal write blocks — classically the Windows console pausing
    /// output in QuickEdit/mark-selection mode — the ~200/sec heartbeat piles
    /// unbounded `FlushDeferred`s into the channel until allocation fails and the
    /// `panic = "abort"` build fast-fails (Windows reports it as a "stack-based
    /// buffer overrun"). A flush is idempotent, so collapsing redundant ones is
    /// visually lossless — the single queued flush paints the latest state.
    flush_pending: Arc<AtomicBool>,
    /// Join handle for the worker thread; `Some` until `Drop` takes it
    /// to `join()`.
    worker: Option<thread::JoinHandle<()>>,
}

impl TaskRenderer {
    /// Spawn the worker thread, handing it ownership of the inner
    /// renderer. After this returns the caller interacts with the inner
    /// renderer only via the returned facade.
    pub fn new(inner: Box<dyn Renderer>) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<RenderCmd>();
        let flush_pending = Arc::new(AtomicBool::new(false));
        let worker_flag = Arc::clone(&flush_pending);
        let worker = thread::Builder::new()
            .name("tuix-render".to_string())
            .spawn(move || run_worker(inner, cmd_rx, worker_flag))
            .expect("spawn render worker thread");
        Self {
            cmd_tx,
            flush_pending,
            worker: Some(worker),
        }
    }

    /// Send an ACK op and block until the worker reports done. 10s
    /// bound keeps us from hanging forever if the worker ever wedges,
    /// while giving slow CI machines / thermal-throttled laptops /
    /// debug builds enough headroom that routine lifecycle ops don't
    /// spuriously timeout.
    ///
    /// 2s was the original budget — a worker processing `Shutdown`
    /// normally takes < 1ms, so 2s felt like plenty. But on a loaded
    /// CI runner mid-cargo-test, a few tests would sporadically fail
    /// on the timeout line because the OS hadn't scheduled the worker
    /// thread fast enough. CC-style TUI harnesses use ~10s for the
    /// same reason.
    fn ack(&self, op: AckOp) {
        let (ack_tx, ack_rx) = mpsc::channel();
        if self
            .cmd_tx
            .send(RenderCmd::Ack { op, ack: ack_tx })
            .is_err()
        {
            // Worker is gone (already shut down) — nothing to do.
            return;
        }
        let _ = ack_rx.recv_timeout(Duration::from_secs(10));
    }
}

impl Renderer for TaskRenderer {
    fn render(&mut self, line: UiLine) {
        let _ = self.cmd_tx.send(RenderCmd::Line(line));
    }

    fn flush(&mut self) {
        let _ = self.cmd_tx.send(RenderCmd::Flush);
    }

    fn shutdown(&mut self) {
        self.ack(AckOp::Shutdown);
    }

    fn reset(&mut self) {
        self.ack(AckOp::Reset);
    }

    fn clear_screen(&mut self) {
        self.ack(AckOp::ClearScreen);
    }

    fn begin_sync(&mut self) {
        let _ = self.cmd_tx.send(RenderCmd::BeginSync);
    }

    fn end_sync(&mut self) {
        let _ = self.cmd_tx.send(RenderCmd::EndSync);
    }

    fn begin_initial_history_replay(&mut self) {
        let _ = self.cmd_tx.send(RenderCmd::BeginInitialHistoryReplay);
    }

    fn end_initial_history_replay(&mut self) {
        let _ = self.cmd_tx.send(RenderCmd::EndInitialHistoryReplay);
    }

    fn set_history_replay_max_rows(&mut self, max_rows: Option<usize>) {
        let _ = self
            .cmd_tx
            .send(RenderCmd::SetHistoryReplayMaxRows(max_rows));
    }

    fn set_suppress_auto_copy(&mut self, suppress: bool) {
        let _ = self.cmd_tx.send(RenderCmd::SetSuppressAutoCopy(suppress));
    }

    fn set_title(&mut self, title: String) {
        let _ = self.cmd_tx.send(RenderCmd::SetTitle(title));
    }

    fn suspend_for_external(&mut self) {
        self.ack(AckOp::SuspendForExternal);
    }

    fn resume_from_external(&mut self) {
        self.ack(AckOp::ResumeFromExternal);
    }

    fn flush_deferred(&mut self) {
        // Only enqueue if none is already pending (coalesce the 5ms heartbeat).
        // See `flush_pending` — prevents unbounded channel growth when the
        // worker's write is stalled (Windows console pause).
        if !self.flush_pending.swap(true, Ordering::AcqRel) {
            let _ = self.cmd_tx.send(RenderCmd::FlushDeferred);
        }
    }

    fn on_resize(&mut self, cols: u16, rows: u16) {
        let _ = self.cmd_tx.send(RenderCmd::Resize(cols, rows));
    }

    fn scroll_body(&mut self, delta: i32) {
        let _ = self.cmd_tx.send(RenderCmd::ScrollBody(delta));
    }

    fn scroll_body_to_top(&mut self) {
        let _ = self.cmd_tx.send(RenderCmd::ScrollBodyToTop);
    }

    fn scroll_body_to_bottom(&mut self) {
        let _ = self.cmd_tx.send(RenderCmd::ScrollBodyToBottom);
    }

    fn scroll_to_prev_message(&mut self) {
        let _ = self.cmd_tx.send(RenderCmd::ScrollToPrevMessage);
    }

    fn scroll_to_next_message(&mut self) {
        let _ = self.cmd_tx.send(RenderCmd::ScrollToNextMessage);
    }

    fn scroll_to_prev_user_message(&mut self) {
        let _ = self.cmd_tx.send(RenderCmd::ScrollToPrevUserMessage);
    }

    fn scroll_to_next_user_message(&mut self) {
        let _ = self.cmd_tx.send(RenderCmd::ScrollToNextUserMessage);
    }
}

impl Drop for TaskRenderer {
    fn drop(&mut self) {
        // Idempotent shutdown — `Renderer::shutdown` may have already
        // run, in which case the worker is already gone and this call
        // is a no-op (ack() swallows the send error).
        self.ack(AckOp::Shutdown);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

fn run_worker(
    mut inner: Box<dyn Renderer>,
    cmd_rx: mpsc::Receiver<RenderCmd>,
    flush_pending: Arc<AtomicBool>,
) {
    use std::time::Instant;
    const RESIZE_REFLOW_DEBOUNCE: Duration = Duration::from_millis(75);
    let mut pending_cmds = VecDeque::new();
    loop {
        let cmd = match pending_cmds.pop_front() {
            Some(cmd) => cmd,
            None => match cmd_rx.recv() {
                Ok(cmd) => cmd,
                Err(_) => break,
            },
        };
        // Measure the wall-clock time each terminal I/O takes so the log
        // shows where Mac Terminal.app / iTerm2 / etc. actually spend time.
        // Big `flush` durations = kernel pipe backpressure from a slow
        // terminal emulator; big `render` durations = our own bytes taking
        // forever to serialize or intermediate `write_all` blocking.
        match cmd {
            RenderCmd::Line(line) => {
                let tag = ui_line_tag(&line);
                let t0 = Instant::now();
                inner.render(line);
                crate::tuix_trace!("REN", "Line {} render={}µs", tag, t0.elapsed().as_micros());
            }
            RenderCmd::Flush => {
                let t0 = Instant::now();
                inner.flush();
                crate::tuix_trace!("REN", "Flush flush={}µs", t0.elapsed().as_micros());
            }
            RenderCmd::FlushDeferred => {
                // Clear the coalescing flag BEFORE flushing so any render that
                // arrives during this (possibly slow / blocked) flush can queue a
                // fresh FlushDeferred and repaint the latest state afterward.
                flush_pending.store(false, Ordering::Release);
                // Skip logging when it's a true no-op (no pending payload
                // and window not elapsed). throttle.rs already logs when
                // this path actually paints.
                let t0 = Instant::now();
                inner.flush_deferred();
                let d = t0.elapsed();
                if d.as_micros() > 100 {
                    crate::tuix_trace!("REN", "FlushDeferred deferred={}µs", d.as_micros());
                }
            }
            RenderCmd::Resize(mut cols, mut rows) => {
                // Rebuild only after the terminal has stopped reporting
                // intermediate geometries. Besides avoiding redundant work,
                // this keeps large ANSI reflow writes out of conhost's
                // mid-resize buffer update window.
                let mut deadline = Instant::now() + RESIZE_REFLOW_DEBOUNCE;
                loop {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    match cmd_rx.recv_timeout(remaining) {
                        Ok(RenderCmd::Resize(next_cols, next_rows)) => {
                            cols = next_cols;
                            rows = next_rows;
                            deadline = Instant::now() + RESIZE_REFLOW_DEBOUNCE;
                        }
                        Ok(cmd @ RenderCmd::Ack { .. }) => {
                            // Lifecycle ACKs are FIFO barriers. Never inspect
                            // or coalesce commands queued after one.
                            pending_cmds.push_back(cmd);
                            break;
                        }
                        Ok(other) => {
                            // Preserve FIFO ordering for normal render/lifecycle
                            // work, but keep listening for a later resize until
                            // the trailing quiet period expires.
                            pending_cmds.push_back(other);
                        }
                        Err(mpsc::RecvTimeoutError::Timeout)
                        | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                let t0 = Instant::now();
                inner.on_resize(cols, rows);
                crate::tuix_trace!(
                    "REN",
                    "Resize {}x{} dur={}µs",
                    cols,
                    rows,
                    t0.elapsed().as_micros()
                );
            }
            RenderCmd::ScrollBody(delta) => {
                inner.scroll_body(delta);
            }
            RenderCmd::ScrollBodyToTop => {
                inner.scroll_body_to_top();
            }
            RenderCmd::ScrollBodyToBottom => {
                inner.scroll_body_to_bottom();
            }
            RenderCmd::ScrollToPrevMessage => {
                inner.scroll_to_prev_message();
            }
            RenderCmd::ScrollToNextMessage => {
                inner.scroll_to_next_message();
            }
            RenderCmd::ScrollToPrevUserMessage => {
                inner.scroll_to_prev_user_message();
            }
            RenderCmd::ScrollToNextUserMessage => {
                inner.scroll_to_next_user_message();
            }
            RenderCmd::BeginSync => {
                inner.begin_sync();
            }
            RenderCmd::EndSync => {
                inner.end_sync();
            }
            RenderCmd::BeginInitialHistoryReplay => {
                inner.begin_initial_history_replay();
            }
            RenderCmd::EndInitialHistoryReplay => {
                inner.end_initial_history_replay();
            }
            RenderCmd::SetHistoryReplayMaxRows(max_rows) => {
                inner.set_history_replay_max_rows(max_rows);
            }
            RenderCmd::SetSuppressAutoCopy(suppress) => {
                inner.set_suppress_auto_copy(suppress);
            }
            RenderCmd::SetTitle(title) => {
                inner.set_title(title);
            }
            RenderCmd::Ack { op, ack } => {
                let t0 = Instant::now();
                match op {
                    AckOp::Reset => inner.reset(),
                    AckOp::ClearScreen => inner.clear_screen(),
                    AckOp::SuspendForExternal => inner.suspend_for_external(),
                    AckOp::ResumeFromExternal => inner.resume_from_external(),
                    AckOp::Shutdown => {
                        inner.shutdown();
                        crate::tuix_trace!(
                            "REN",
                            "Ack Shutdown dur={}µs",
                            t0.elapsed().as_micros()
                        );
                        let _ = ack.send(());
                        // Exit the loop — drop `inner` + `cmd_rx`.
                        // Any queued commands after this point are
                        // discarded (the sender's next send errors,
                        // which callers treat as "worker gone").
                        return;
                    }
                }
                crate::tuix_trace!("REN", "Ack {:?} dur={}µs", op, t0.elapsed().as_micros());
                let _ = ack.send(());
            }
        }

        // Trailing-edge footer repair: if the command just processed scrolled
        // the whole viewport (an overflow LF lifts the footer up one row),
        // repaint the footer NOW rather than waiting for the event loop's next
        // ~5ms FlushDeferred — which lags, and starves under streaming load,
        // and is exposed on hosts that don't vsync-coalesce (native Win10
        // conhost / pwsh7). Only fires on a real body scroll: InputPrompt / IME
        // bursts never scroll, so their coalescing on the deferred tick is
        // unaffected. A multi-row render sets the flag once → ONE flush here,
        // not one per row. flush_deferred is a no-op when nothing is dirty.
        if inner.take_pending_scroll_flush() {
            inner.flush_deferred();
        }
    }
    // Sender dropped without explicit Shutdown — still run shutdown so
    // the terminal isn't left in raw mode on abrupt exit paths.
    inner.shutdown();
}

/// Short tag for logging which UiLine variant the worker is processing.
/// Keeps trace lines column-aligned so `grep Line` output is readable.
fn ui_line_tag(l: &UiLine) -> &'static str {
    match l {
        UiLine::Welcome { .. } => "Welcome",
        UiLine::User(_) => "User",
        UiLine::UserWithAttachments { .. } => "UserWithAttachments",
        UiLine::AssistantText(_) => "AssistantText",
        UiLine::ReasoningText(_) => "ReasoningText",
        UiLine::AssistantLineBreak => "AssistantLineBreak",
        UiLine::ToolCall { .. } => "ToolCall",
        UiLine::ToolCallInFlight { .. } => "ToolCallInFlight",
        UiLine::ToolCallCommit { .. } => "ToolCallCommit",
        UiLine::ToolGroupRender { .. } => "ToolGroupRender",
        UiLine::ToolGroupChildUpdate { .. } => "ToolGroupChildUpdate",
        UiLine::ToolGroupSummary { .. } => "ToolGroupSummary",
        UiLine::ToolResult { .. } => "ToolResult",
        UiLine::DiffLine { .. } => "DiffLine",
        UiLine::DiffBlock(_) => "DiffBlock",
        UiLine::EditDiffBlock(_) => "EditDiffBlock",
        UiLine::Error(_) => "Error",
        UiLine::Warning(_) => "Warning",
        UiLine::Muted(_) => "Muted",
        UiLine::CompactionMark(_) => "CompactionMark",
        UiLine::TurnCancelled => "TurnCancelled",
        UiLine::TurnComplete => "TurnComplete",
        UiLine::Spinner { .. } => "Spinner",
        UiLine::StreamingBox { .. } => "StreamingBox",
        UiLine::ClearTransient => "ClearTransient",
        UiLine::InputPrompt { .. } => "InputPrompt",
        UiLine::InputCommit => "InputCommit",
        UiLine::CommandOutput(_) => "CommandOutput",
        UiLine::ImageAttachment(_) => "ImageAttachment",
        UiLine::VisionPreprocessSuccess { .. } => "VisionPreprocessSuccess",
        UiLine::TurnSeparator { .. } => "TurnSeparator",
        UiLine::DiffPanel { .. } => "DiffPanel",
        UiLine::ModalOverlayClear => "ModalOverlayClear",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::Renderer;
    use std::sync::{Arc, Mutex};

    /// Counting test renderer — records every call so tests can assert
    /// the worker forwards correctly.
    #[derive(Default)]
    struct Counts {
        renders: usize,
        flushes: usize,
        shutdowns: usize,
        resets: usize,
        clear_screens: usize,
        suspends: usize,
        resumes: usize,
        deferred: usize,
        begin_syncs: usize,
        end_syncs: usize,
        history_replay_caps: Vec<Option<usize>>,
        resizes: Vec<(u16, u16)>,
        calls: Vec<&'static str>,
    }

    struct TestRenderer {
        counts: Arc<Mutex<Counts>>,
    }

    impl Renderer for TestRenderer {
        fn render(&mut self, _line: UiLine) {
            self.counts.lock().unwrap().renders += 1;
        }
        fn flush(&mut self) {
            self.counts.lock().unwrap().flushes += 1;
        }
        fn shutdown(&mut self) {
            self.counts.lock().unwrap().shutdowns += 1;
        }
        fn reset(&mut self) {
            let mut counts = self.counts.lock().unwrap();
            counts.resets += 1;
            counts.calls.push("reset");
        }
        fn clear_screen(&mut self) {
            self.counts.lock().unwrap().clear_screens += 1;
        }
        fn suspend_for_external(&mut self) {
            self.counts.lock().unwrap().suspends += 1;
        }
        fn resume_from_external(&mut self) {
            self.counts.lock().unwrap().resumes += 1;
        }
        fn flush_deferred(&mut self) {
            self.counts.lock().unwrap().deferred += 1;
        }
        fn begin_sync(&mut self) {
            self.counts.lock().unwrap().begin_syncs += 1;
        }
        fn end_sync(&mut self) {
            self.counts.lock().unwrap().end_syncs += 1;
        }
        fn set_history_replay_max_rows(&mut self, max_rows: Option<usize>) {
            self.counts
                .lock()
                .unwrap()
                .history_replay_caps
                .push(max_rows);
        }
        fn on_resize(&mut self, cols: u16, rows: u16) {
            let mut counts = self.counts.lock().unwrap();
            counts.resizes.push((cols, rows));
            counts.calls.push("resize");
        }
    }

    fn setup() -> (TaskRenderer, Arc<Mutex<Counts>>) {
        let counts = Arc::new(Mutex::new(Counts::default()));
        let inner = Box::new(TestRenderer {
            counts: counts.clone(),
        });
        (TaskRenderer::new(inner), counts)
    }

    #[test]
    fn render_and_flush_forward_to_inner() {
        let (mut r, counts) = setup();
        r.render(UiLine::User("hi".into()));
        r.render(UiLine::User("there".into()));
        r.flush();
        // Force ordering: reset is an ACK op that blocks until the
        // worker has drained earlier commands, so after reset() returns
        // the renders + flush must already be counted.
        r.reset();
        let c = counts.lock().unwrap();
        assert_eq!(c.renders, 2);
        assert_eq!(c.flushes, 1);
        assert_eq!(c.resets, 1);
    }

    #[test]
    fn begin_and_end_sync_forward_to_inner() {
        let (mut r, counts) = setup();
        // The `/resume` replay brackets reset()+renders in begin_sync/end_sync.
        // Both are fire-and-forget; FIFO ordering on the channel keeps
        // begin_sync before, and end_sync after, the work in between.
        r.begin_sync();
        r.render(UiLine::User("replayed".into()));
        r.end_sync();
        // reset() is an ACK op — blocks until the worker has drained the
        // three earlier commands, so the counts are settled when it returns.
        r.reset();
        let c = counts.lock().unwrap();
        assert_eq!(c.begin_syncs, 1, "begin_sync must forward to inner");
        assert_eq!(c.end_syncs, 1, "end_sync must forward to inner");
        assert_eq!(c.renders, 1);
    }

    #[test]
    fn history_replay_cap_updates_forward_to_inner() {
        let (mut r, counts) = setup();
        r.set_history_replay_max_rows(Some(1234));
        r.set_history_replay_max_rows(None);
        r.reset();
        assert_eq!(
            counts.lock().unwrap().history_replay_caps,
            vec![Some(1234), None]
        );
    }

    #[test]
    fn lifecycle_ack_blocks_until_worker_done() {
        let (mut r, counts) = setup();
        // Chain several lifecycle ACKs — each must complete in order
        // before the next returns.
        r.clear_screen();
        assert_eq!(counts.lock().unwrap().clear_screens, 1);
        r.suspend_for_external();
        assert_eq!(counts.lock().unwrap().suspends, 1);
        r.resume_from_external();
        assert_eq!(counts.lock().unwrap().resumes, 1);
    }

    #[test]
    fn shutdown_drops_worker_and_later_sends_are_noops() {
        let (mut r, counts) = setup();
        r.render(UiLine::User("before".into()));
        r.shutdown();
        assert_eq!(counts.lock().unwrap().shutdowns, 1);
        // Worker is gone — these must not panic, even though no one is
        // listening on the channel anymore.
        r.render(UiLine::User("after".into()));
        r.flush();
        // Second shutdown is idempotent.
        r.shutdown();
    }

    #[test]
    fn drop_triggers_shutdown_when_not_called_explicitly() {
        let counts = {
            let counts = Arc::new(Mutex::new(Counts::default()));
            let inner = Box::new(TestRenderer {
                counts: counts.clone(),
            });
            let mut r = TaskRenderer::new(inner);
            r.render(UiLine::User("one".into()));
            counts
            // r dropped here — Drop must shut the worker down + join.
        };
        // By the time Drop returns, the worker has finished, so the
        // render AND one shutdown are accounted for.
        let c = counts.lock().unwrap();
        assert_eq!(c.renders, 1);
        assert_eq!(c.shutdowns, 1);
    }

    #[test]
    fn flush_deferred_fire_and_forget() {
        let (mut r, counts) = setup();
        r.flush_deferred();
        // No ACK on flush_deferred — have to fence with a separate ACK
        // to observe it deterministically.
        r.reset();
        assert_eq!(counts.lock().unwrap().deferred, 1);
    }

    #[test]
    fn resize_burst_is_coalesced_to_latest_geometry() {
        let (mut r, counts) = setup();
        r.on_resize(80, 24);
        r.on_resize(100, 30);
        r.on_resize(120, 40);
        // ACK fences the fire-and-forget resize commands.
        r.reset();
        assert_eq!(counts.lock().unwrap().resizes, vec![(120, 40)]);
    }

    #[test]
    fn lifecycle_ack_is_a_resize_coalescing_barrier() {
        let (mut r, counts) = setup();
        r.on_resize(80, 24);
        r.reset();
        r.on_resize(120, 40);
        r.reset();

        let counts = counts.lock().unwrap();
        assert_eq!(counts.resizes, vec![(80, 24), (120, 40)]);
        assert_eq!(counts.calls, vec!["resize", "reset", "resize", "reset"]);
    }
}
