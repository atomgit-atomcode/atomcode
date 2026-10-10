//! OSC 22 link feedback, restricted to terminals with known shape semantics.
use std::io::{self, Write};
use std::sync::atomic::{AtomicU8, Ordering};

use super::{Click, Input};
use crate::frame::Frame;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TerminalKind {
    Unsupported = 0,
    Kitty = 1,
    Ghostty = 2,
}

pub(super) fn supported(program: &str, term: &str, kitty: bool, multiplexed: bool) -> TerminalKind {
    if multiplexed || term.starts_with("tmux") || term.starts_with("screen") {
        TerminalKind::Unsupported
    } else if program.eq_ignore_ascii_case("ghostty") || term == "xterm-ghostty" {
        TerminalKind::Ghostty
    } else if program.eq_ignore_ascii_case("kitty") || term == "xterm-kitty" || kitty {
        TerminalKind::Kitty
    } else {
        TerminalKind::Unsupported
    }
}

pub(super) fn detect() -> TerminalKind {
    supported(
        &std::env::var("TERM_PROGRAM").unwrap_or_default(),
        &std::env::var("TERM").unwrap_or_default(),
        std::env::var_os("KITTY_WINDOW_ID").is_some(),
        ["TMUX", "STY", "ZELLIJ"]
            .iter()
            .any(|key| std::env::var_os(key).is_some()),
    )
}

// Shared with panic and signal cleanup, which must not lock the hover state.
pub(super) static POINTER: LinkPointer = LinkPointer::new();

pub(super) struct LinkPointer {
    terminal: AtomicU8,
    // 0: untouched/restored, 1: default arrow, 2: hand, 3: uncertain write.
    state: AtomicU8,
}

impl LinkPointer {
    const fn new() -> Self {
        Self {
            terminal: AtomicU8::new(0),
            state: AtomicU8::new(0),
        }
    }
    pub(super) fn configure(&self, terminal: TerminalKind) {
        self.terminal.store(terminal as u8, Ordering::Relaxed);
    }
    pub(super) fn enabled(&self) -> bool {
        self.terminal.load(Ordering::Relaxed) != 0
    }
    pub(super) fn update(&self, writer: &mut impl Write, over_link: bool) -> io::Result<()> {
        if !self.enabled() {
            return Ok(());
        }
        let previous = self.state.load(Ordering::Relaxed);
        let next = if over_link { 2 } else { 1 };
        if previous == next || (previous == 0 && !over_link) {
            return Ok(());
        }
        // Arm cleanup before writing; a failed flush must leave it armed.
        self.state.store(3, Ordering::Relaxed);
        writer.write_all(if over_link {
            b"\x1b]22;pointer\x1b\\"
        } else {
            b"\x1b]22;default\x1b\\"
        })?;
        writer.flush()?;
        self.state.store(next, Ordering::Relaxed);
        Ok(())
    }
    pub(super) fn restore_bytes(&self) -> &'static [u8] {
        if self.state.load(Ordering::Relaxed) == 0 {
            return b"";
        }
        match self.terminal.load(Ordering::Relaxed) {
            1 => b"\x1b]22;\x1b\\",
            2 => b"\x1b]22;text\x1b\\",
            _ => b"",
        }
    }
    pub(super) fn restore(&self, writer: &mut impl Write) -> io::Result<()> {
        let bytes = self.restore_bytes();
        if bytes.is_empty() {
            return Ok(());
        }
        writer.write_all(bytes)?;
        writer.flush()?;
        self.state.store(0, Ordering::Relaxed);
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct LinkHover {
    position: Option<(u16, u16)>,
    frame: Option<Frame>,
}

impl LinkHover {
    pub(super) fn clear(&mut self) {
        self.position = None;
    }
    pub(super) fn observe(&mut self, input: &Input) {
        match input {
            Input::Mouse(
                Click::Hover | Click::Release | Click::WheelUp | Click::WheelDown,
                x,
                y,
            ) => self.position = Some((*x, *y)),
            Input::Resize(..) => {
                self.clear();
                self.frame = None;
            }
            Input::Mouse(..) | Input::Focus(false) => self.clear(),
            _ => {}
        }
    }
    pub(super) fn paint(&mut self, frame: &Frame) {
        self.frame = Some(frame.clone());
    }
    pub(super) fn over_link(&self) -> bool {
        self.position
            .and_then(|(x, y)| self.frame.as_ref()?.link_at(x, y))
            .is_some_and(|url| !url.bytes().any(|b| b < 0x20 || b == 0x7f))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{Line, Rect, Span, Style};

    fn frame(linked: bool) -> Frame {
        let mut f = Frame::new(30, 3);
        f.place(
            "link",
            Rect::new(0, 0, 30, 1),
            vec![Line::from_spans(vec![if linked {
                Span::linked("link", Style::new(), "https://example.com")
            } else {
                Span::raw("text")
            }])],
        );
        f
    }

    #[test]
    fn only_known_direct_terminals_are_enabled() {
        assert_eq!(
            supported("ghostty", "xterm-256color", false, false),
            TerminalKind::Ghostty
        );
        assert_eq!(
            supported("", "xterm-kitty", false, false),
            TerminalKind::Kitty
        );
        assert_eq!(
            supported("iTerm.app", "xterm-256color", false, false),
            TerminalKind::Unsupported
        );
        for term in ["tmux-256color", "screen-256color"] {
            assert_eq!(
                supported("ghostty", term, true, false),
                TerminalKind::Unsupported
            );
        }
        assert_eq!(
            supported("ghostty", "xterm-256color", false, true),
            TerminalKind::Unsupported
        );
    }
    #[test]
    fn shapes_change_once_and_cleanup_releases_each_terminal() {
        for (kind, restore) in [
            (TerminalKind::Kitty, "\x1b]22;\x1b\\"),
            (TerminalKind::Ghostty, "\x1b]22;text\x1b\\"),
        ] {
            let p = LinkPointer::new();
            p.configure(kind);
            let mut out = Vec::new();
            p.update(&mut out, false).unwrap();
            assert!(out.is_empty());
            p.update(&mut out, true).unwrap();
            p.update(&mut out, true).unwrap();
            p.update(&mut out, false).unwrap();
            p.restore(&mut out).unwrap();
            p.restore(&mut out).unwrap();
            assert_eq!(
                String::from_utf8(out).unwrap(),
                format!("\x1b]22;pointer\x1b\\\x1b]22;default\x1b\\{restore}")
            );
        }
    }
    #[test]
    fn hover_tracks_the_painted_link_and_invalidates_on_handoffs() {
        let mut hover = LinkHover::default();
        hover.paint(&frame(true));
        hover.observe(&Input::Mouse(Click::Hover, 1, 0));
        assert!(hover.over_link());
        hover.paint(&frame(false));
        assert!(!hover.over_link());
        for event in [
            Input::Focus(false),
            Input::Resize(10, 10),
            Input::Mouse(Click::Drag, 1, 0),
            Input::Mouse(Click::Press, 1, 0),
        ] {
            hover.paint(&frame(true));
            hover.observe(&Input::Mouse(Click::Hover, 1, 0));
            hover.observe(&event);
            assert!(!hover.over_link());
        }
    }
    #[test]
    fn unsupported_terminals_emit_nothing() {
        let p = LinkPointer::new();
        let mut out = Vec::new();
        p.update(&mut out, true).unwrap();
        p.restore(&mut out).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn a_link_covered_by_a_panel_or_removed_by_resize_is_not_pointed_at() {
        let mut hover = LinkHover::default();
        let mut covered = frame(true);
        covered.place("panel", Rect::new(0, 0, 30, 1), vec![Line::raw("menu")]);
        hover.observe(&Input::Mouse(Click::Hover, 1, 0));
        hover.paint(&covered);
        assert!(!hover.over_link());
        hover.paint(&frame(true));
        assert!(hover.over_link());
        hover.observe(&Input::Resize(10, 10));
        hover.observe(&Input::Mouse(Click::Hover, 1, 0));
        assert!(!hover.over_link());
        hover.paint(&frame(true));
        assert!(hover.over_link());
    }

    #[test]
    fn a_failed_flush_is_retried_and_cleanup_stays_armed() {
        struct NoFlush(Vec<u8>);
        impl Write for NoFlush {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::other("flush"))
            }
        }
        let p = LinkPointer::new();
        p.configure(TerminalKind::Ghostty);
        let mut writer = NoFlush(Vec::new());
        assert!(p.update(&mut writer, true).is_err());
        assert!(p.update(&mut writer, true).is_err());
        assert_eq!(writer.0, b"\x1b]22;pointer\x1b\\\x1b]22;pointer\x1b\\");
        let mut out = Vec::new();
        p.restore(&mut out).unwrap();
        assert_eq!(out, b"\x1b]22;text\x1b\\");
    }

    #[test]
    fn failed_writes_keep_cleanup_armed() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("broken"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::other("broken"))
            }
        }
        let p = LinkPointer::new();
        p.configure(TerminalKind::Kitty);
        assert!(p.update(&mut Broken, true).is_err());
        assert!(p.restore(&mut Broken).is_err());
        let mut out = Vec::new();
        p.restore(&mut out).unwrap();
        assert_eq!(out, b"\x1b]22;\x1b\\");
        assert!(p.restore_bytes().is_empty());
    }
}
