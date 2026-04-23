use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{poll, Event, KeyEvent, MouseEvent};
use std::io::{self, Write};
use tokio::sync::mpsc;

/// Enable only mouse button reporting (click + wheel) WITHOUT drag-motion tracking.
///
/// This is NOT what `crossterm::EnableMouseCapture` does — that enables 1000+1002+1015+1006
/// which also captures drag, breaking the terminal's native click-drag text selection.
///
/// We enable:
///  - `?1000h` — X11 mouse reporting (button press/release, scroll wheel as button 64/65)
///  - `?1006h` — SGR extended coordinate encoding (supports >223 columns)
///
/// We deliberately OMIT `?1002h` (button-event tracking / drag motion). Most modern
/// terminals then leave click-drag alone for native text selection while still reporting
/// wheel + click events to us. See TUI docs.
const ENABLE_MOUSE_SCROLL_ONLY: &str = "\x1B[?1000h\x1B[?1006h";
const DISABLE_MOUSE_SCROLL_ONLY: &str = "\x1B[?1006l\x1B[?1000l";

fn enable_mouse_scroll_only() {
    let mut stdout = io::stdout();
    let _ = stdout.write_all(ENABLE_MOUSE_SCROLL_ONLY.as_bytes());
    let _ = stdout.flush();
}

#[allow(dead_code)]
pub fn disable_mouse_scroll_only() {
    let mut stdout = io::stdout();
    let _ = stdout.write_all(DISABLE_MOUSE_SCROLL_ONLY.as_bytes());
    let _ = stdout.flush();
}

#[derive(Debug)]
pub enum AppEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Paste(String), // Bracketed paste content
    Resize(u16, u16),
    Tick,
    IssueCreated { success: bool, message: String }, // Issue creation result
}

pub struct EventLoop {
    rx: mpsc::UnboundedReceiver<AppEvent>,
    tx: mpsc::UnboundedSender<AppEvent>,
    /// Flag to signal the input thread to stop
    stop_flag: Arc<AtomicBool>,
    /// Handle to the keyboard/mouse reader thread
    input_thread: Option<std::thread::JoinHandle<()>>,
    /// Handle to the tick task
    tick_task: Option<tokio::task::JoinHandle<()>>,
}

impl EventLoop {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            rx,
            tx,
            stop_flag: Arc::new(AtomicBool::new(false)),
            input_thread: None,
            tick_task: None,
        }
    }

    pub fn sender(&self) -> mpsc::UnboundedSender<AppEvent> {
        self.tx.clone()
    }

    pub fn start(&mut self) {
        // Reset stop flag
        self.stop_flag.store(false, Ordering::SeqCst);

        // Enable scroll-only mouse reporting (mode 1000 + 1006, no 1002 drag tracking).
        // Goal: get wheel scroll events in-app while preserving native click-drag selection.
        enable_mouse_scroll_only();

        // Start keyboard/mouse reader in a dedicated thread (not tokio task)
        // This gives us more control over the input stream
        let tx = self.tx.clone();
        let stop_flag = self.stop_flag.clone();
        let handle = std::thread::spawn(move || {
            while !stop_flag.load(Ordering::SeqCst) {
                // Use poll with timeout to allow checking stop flag periodically
                match poll(Duration::from_millis(50)) {
                    Ok(true) => {
                        // Event available, read it
                        match crossterm::event::read() {
                            Ok(evt) => {
                                let app_event = match evt {
                                    Event::Key(key) => {
                                        #[cfg(target_os = "windows")]
                                        if key.kind == crossterm::event::KeyEventKind::Release {
                                            continue;
                                        }
                                        AppEvent::Key(key)
                                    }
                                    Event::Mouse(mouse) => {
                                        // CRITICAL: Mouse capture must stay ALWAYS enabled.
                                        // We no longer toggle capture for text selection because:
                                        // 1. When capture is disabled, terminals convert scroll wheel to Up/Down keys
                                        // 2. This would incorrectly trigger history navigation in Input Box
                                        // 3. Text selection still works via terminal's native selection (shift+click or drag)

                                        // Just forward all mouse events to the app
                                        AppEvent::Mouse(mouse)
                                    }
                                    Event::Paste(text) => AppEvent::Paste(text),
                                    Event::Resize(w, h) => AppEvent::Resize(w, h),
                                    _ => continue,
                                };
                                if tx.send(app_event).is_err() {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    Ok(false) => {
                        // Timeout, continue to check stop flag
                    }
                    Err(_) => break,
                }
            }
        });
        self.input_thread = Some(handle);

        // Start tick generator (keep as tokio task)
        let tx = self.tx.clone();
        let stop_flag = self.stop_flag.clone();
        let tick_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(250));
            loop {
                interval.tick().await;
                if stop_flag.load(Ordering::SeqCst) {
                    break;
                }
                if tx.send(AppEvent::Tick).is_err() {
                    break;
                }
            }
        });
        self.tick_task = Some(tick_handle);
    }

    /// Stop the event loop tasks (before opening external editor)
    pub fn stop(&mut self) {
        // Signal threads to stop
        self.stop_flag.store(true, Ordering::SeqCst);

        // Wait for input thread to finish (it should exit quickly due to timeout)
        if let Some(handle) = self.input_thread.take() {
            let _ = handle.join();
        }

        // Abort tick task
        if let Some(handle) = self.tick_task.take() {
            handle.abort();
        }

        // Drain any remaining events from the queue
        while self.rx.try_recv().is_ok() {}
    }

    pub async fn next(&mut self) -> Option<AppEvent> {
        self.rx.recv().await
    }

    pub fn try_next(&mut self) -> Option<AppEvent> {
        self.rx.try_recv().ok()
    }
}
