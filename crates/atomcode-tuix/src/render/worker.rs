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

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use super::{Renderer, UiLine};

/// Commands sent to the render worker thread.
enum RenderCmd {
    Line(UiLine),
    Flush,
    FlushDeferred,
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
        let worker = thread::Builder::new()
            .name("tuix-render".to_string())
            .spawn(move || run_worker(inner, cmd_rx))
            .expect("spawn render worker thread");
        Self {
            cmd_tx,
            worker: Some(worker),
        }
    }

    /// Send an ACK op and block until the worker reports done. 2s bound
    /// keeps us from hanging forever if the worker ever wedges (it
    /// shouldn't — but a bounded wait beats an infinite one for UX).
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
        let _ = ack_rx.recv_timeout(Duration::from_secs(2));
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

    fn suspend_for_external(&mut self) {
        self.ack(AckOp::SuspendForExternal);
    }

    fn resume_from_external(&mut self) {
        self.ack(AckOp::ResumeFromExternal);
    }

    fn flush_deferred(&mut self) {
        let _ = self.cmd_tx.send(RenderCmd::FlushDeferred);
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

fn run_worker(mut inner: Box<dyn Renderer>, cmd_rx: mpsc::Receiver<RenderCmd>) {
    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            RenderCmd::Line(line) => {
                crate::tuix_trace!("REN", "Line {}", ui_line_tag(&line));
                inner.render(line);
            }
            RenderCmd::Flush => {
                crate::tuix_trace!("REN", "Flush");
                inner.flush();
            }
            RenderCmd::FlushDeferred => {
                // Skip logging this to avoid flooding — the 20ms tick
                // fires 50 times/sec even when nothing is pending, and
                // throttle.rs already logs when it decides to paint.
                inner.flush_deferred();
            }
            RenderCmd::Ack { op, ack } => {
                crate::tuix_trace!("REN", "Ack {:?}", op);
                match op {
                    AckOp::Reset => inner.reset(),
                    AckOp::ClearScreen => inner.clear_screen(),
                    AckOp::SuspendForExternal => inner.suspend_for_external(),
                    AckOp::ResumeFromExternal => inner.resume_from_external(),
                    AckOp::Shutdown => {
                        inner.shutdown();
                        let _ = ack.send(());
                        // Exit the loop — drop `inner` + `cmd_rx`.
                        // Any queued commands after this point are
                        // discarded (the sender's next send errors,
                        // which callers treat as "worker gone").
                        return;
                    }
                }
                let _ = ack.send(());
            }
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
        UiLine::AssistantText(_) => "AssistantText",
        UiLine::AssistantLineBreak => "AssistantLineBreak",
        UiLine::ToolCall { .. } => "ToolCall",
        UiLine::ToolResult { .. } => "ToolResult",
        UiLine::DiffLine { .. } => "DiffLine",
        UiLine::DiffBlock(_) => "DiffBlock",
        UiLine::ApprovalPrompt { .. } => "ApprovalPrompt",
        UiLine::Error(_) => "Error",
        UiLine::TurnCancelled => "TurnCancelled",
        UiLine::TurnComplete => "TurnComplete",
        UiLine::Spinner { .. } => "Spinner",
        UiLine::StreamingBox { .. } => "StreamingBox",
        UiLine::ClearTransient => "ClearTransient",
        UiLine::InputPrompt { .. } => "InputPrompt",
        UiLine::InputCommit => "InputCommit",
        UiLine::CommandOutput(_) => "CommandOutput",
        UiLine::TurnSeparator { .. } => "TurnSeparator",
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
            self.counts.lock().unwrap().resets += 1;
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
}
