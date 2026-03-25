use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use futures::StreamExt;
use tokio::sync::mpsc;

use atomcode_core::stream::TokenUsage;
use atomcode_core::tool::{ToolCall, ToolResult};

#[derive(Debug)]
pub enum AppEvent {
    Key(KeyEvent),
    Paste(String),          // Bracketed paste content
    ScrollUp(u16),
    ScrollDown(u16),
    MouseDown(u16, u16),   // (column, row) in terminal coordinates
    MouseDrag(u16, u16),   // (column, row)
    MouseUp(u16, u16),     // (column, row)
    StreamDelta(String),
    StreamToolCallStart { id: String, name: String },
    StreamToolCallDelta(String),
    StreamToolCallDone(ToolCall),
    StreamUsage(TokenUsage),
    StreamDone,
    StreamError(String),
    ToolFinished(ToolResult),
    Resize(u16, u16),
    Tick,
}

pub struct EventLoop {
    rx: mpsc::UnboundedReceiver<AppEvent>,
    tx: mpsc::UnboundedSender<AppEvent>,
}

impl EventLoop {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self { rx, tx }
    }

    pub fn sender(&self) -> mpsc::UnboundedSender<AppEvent> {
        self.tx.clone()
    }

    pub fn start(&self) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let mut reader = EventStream::new();
            loop {
                match reader.next().await {
                    Some(Ok(evt)) => {
                        let app_event = match evt {
                            Event::Key(key) => {
                                // Windows sends Press + Release for each keystroke.
                                // Skip Release to prevent double input. macOS/Linux
                                // only sends Press, so this check is harmless there
                                // but we gate it to avoid any edge cases with IME.
                                #[cfg(target_os = "windows")]
                                if key.kind == KeyEventKind::Release {
                                    continue;
                                }
                                AppEvent::Key(key)
                            }
                            Event::Paste(text) => AppEvent::Paste(text),
                            Event::Resize(w, h) => AppEvent::Resize(w, h),
                            Event::Mouse(MouseEvent { kind, column, row, .. }) => {
                                match kind {
                                    MouseEventKind::ScrollUp => AppEvent::ScrollUp(3),
                                    MouseEventKind::ScrollDown => AppEvent::ScrollDown(3),
                                    MouseEventKind::Down(MouseButton::Left) => AppEvent::MouseDown(column, row),
                                    MouseEventKind::Drag(MouseButton::Left) => AppEvent::MouseDrag(column, row),
                                    MouseEventKind::Up(MouseButton::Left) => AppEvent::MouseUp(column, row),
                                    _ => continue,
                                }
                            }
                            _ => continue,
                        };
                        if tx.send(app_event).is_err() {
                            break;
                        }
                    }
                    Some(Err(_)) => break,
                    None => break,
                }
            }
        });

        let tx = self.tx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(250));
            loop {
                interval.tick().await;
                if tx.send(AppEvent::Tick).is_err() {
                    break;
                }
            }
        });
    }

    pub async fn next(&mut self) -> Option<AppEvent> {
        self.rx.recv().await
    }

    pub fn try_next(&mut self) -> Option<AppEvent> {
        self.rx.try_recv().ok()
    }
}
