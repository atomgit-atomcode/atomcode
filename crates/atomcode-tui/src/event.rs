use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event, KeyEvent, MouseEvent, poll};
use crossterm::execute;
use crossterm::event::EnableMouseCapture;
use std::io::{self};
use tokio::sync::mpsc;

#[derive(Debug)]
pub enum AppEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Paste(String),          // Bracketed paste content
    Resize(u16, u16),
    Tick,
    IssueCreated { success: bool, message: String },  // Issue creation result
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

        // Enable mouse capture at startup (stays enabled forever)
        let _ = execute!(io::stdout(), EnableMouseCapture);

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
