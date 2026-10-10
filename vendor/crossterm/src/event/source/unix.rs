#[cfg(feature = "use-dev-tty")]
pub(crate) mod tty;

#[cfg(not(feature = "use-dev-tty"))]
pub(crate) mod mio;

#[cfg(feature = "use-dev-tty")]
pub(crate) use self::tty::UnixInternalEventSource;

#[cfg(not(feature = "use-dev-tty"))]
pub(crate) use self::mio::UnixInternalEventSource;

use crate::event::{
    sys::unix::parse::parse_event, Event, InternalEvent, KeyCode, KeyEvent, KeyModifiers,
};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

// One incremental parser shared by both Unix backends. OSC responses must remain
// buffered across reads, including a split ESC-backslash string terminator.
// Numeric OSC strings (including unknown commands) are consumed, never exposed
// as keyboard text; only valid 10/11 and 4 slots 0..15 produce internal events.
// A 1s idle deadline bounds malformed OSC recovery, and a new ESC sequence
// resynchronizes early. Bytes arriving more than 1s apart can be interpreted as
// ordinary input after recovery. Alt+] followed immediately by digits is ambiguous and
// treated as OSC; standalone Alt+] uses the 30ms ambiguity deadline.
#[derive(Debug, Default)]
pub(crate) struct Parser {
    buffer: Vec<u8>,
    internal_events: VecDeque<InternalEvent>,
    deadline: Option<Instant>,
    osc_discard: bool,
    osc_escape: bool,
}

impl Parser {
    const ESC_WAIT: Duration = Duration::from_millis(30);
    const OSC_WAIT: Duration = Duration::from_secs(1);
    const OSC_LIMIT: usize = 256;

    pub(crate) fn wait(&self, timeout: Option<Duration>) -> Option<Duration> {
        match (timeout, self.deadline) {
            (Some(t), Some(d)) => Some(t.min(d.saturating_duration_since(Instant::now()))),
            (None, Some(d)) => Some(d.saturating_duration_since(Instant::now())),
            (t, None) => t,
        }
    }

    fn reset(&mut self) {
        self.buffer.clear();
        self.deadline = None;
        self.osc_discard = false;
        self.osc_escape = false;
    }

    fn expire(&mut self) {
        if self.deadline.map_or(false, |d| Instant::now() >= d) {
            let key = match self.buffer.as_slice() {
                b"\x1b" => Some(KeyCode::Esc.into()),
                b"\x1b]" => Some(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::ALT)),
                _ => None,
            };
            if let Some(key) = key {
                self.internal_events
                    .push_back(InternalEvent::Event(Event::Key(key)));
            }
            self.reset();
        }
    }

    pub(crate) fn advance(&mut self, input: &[u8], more: bool) {
        self.expire();
        for (idx, &byte) in input.iter().enumerate() {
            if self.buffer.starts_with(b"\x1b]") {
                self.deadline = Some(Instant::now() + Self::OSC_WAIT);
                if byte == 7 || (self.osc_escape && byte == b'\\') {
                    if !self.osc_discard {
                        let end = self.buffer.len() - usize::from(self.osc_escape);
                        if let Some(event) = parse_color(&self.buffer[2..end]) {
                            self.internal_events.push_back(event);
                        }
                    }
                    self.reset();
                    continue;
                }
                if self.osc_escape {
                    // A new escape sequence replaces a damaged OSC, rather than
                    // swallowing arrow keys or another response indefinitely.
                    self.reset();
                    self.buffer.push(0x1b);
                } else {
                    self.osc_escape = byte == 0x1b;
                    if self.buffer.len() == 2 {
                        // Alt+] followed by non-OSC text is still ordinary input.
                        if !byte.is_ascii_digit() && byte != 0x1b {
                            self.internal_events
                                .push_back(InternalEvent::Event(Event::Key(KeyEvent::new(
                                    KeyCode::Char(']'),
                                    KeyModifiers::ALT,
                                ))));
                            self.reset();
                        } else {
                            self.deadline = Some(Instant::now() + Self::OSC_WAIT);
                        }
                    }
                    if self.buffer.starts_with(b"\x1b]") {
                        if self.buffer.len() < Self::OSC_LIMIT {
                            self.buffer.push(byte);
                        } else {
                            self.osc_discard = true;
                        }
                        continue;
                    }
                }
            }
            self.buffer.push(byte);
            if self.buffer == b"\x1b" || self.buffer == b"\x1b]" {
                self.deadline = Some(Instant::now() + Self::ESC_WAIT);
                continue;
            }
            self.deadline = None;
            match parse_event(&self.buffer, idx + 1 < input.len() || more) {
                Ok(Some(event)) => {
                    self.internal_events.push_back(event);
                    self.reset();
                }
                Ok(None) => (),
                Err(_) => self.reset(),
            }
        }
    }
}

impl Iterator for Parser {
    type Item = InternalEvent;
    fn next(&mut self) -> Option<Self::Item> {
        self.expire();
        self.internal_events.pop_front()
    }
}

fn parse_color(payload: &[u8]) -> Option<InternalEvent> {
    let text = std::str::from_utf8(payload).ok()?;
    let mut fields = text.split(';');
    let kind: u8 = fields.next()?.parse().ok()?;
    let slot = match kind {
        4 => {
            let slot = fields.next()?.parse::<u8>().ok()?;
            if slot > 15 {
                return None;
            }
            Some(slot)
        }
        10 | 11 => None,
        _ => return None,
    };
    let specification = fields.next()?;
    if fields.next().is_some() {
        return None;
    }
    if let Some(hex) = specification.strip_prefix('#') {
        if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let packed = u32::from_str_radix(hex, 16).ok()?;
        return Some(InternalEvent::TerminalColor(
            kind,
            slot,
            ((packed >> 16) as u8, (packed >> 8) as u8, packed as u8),
        ));
    }
    let mut rgb = specification.strip_prefix("rgb:")?.split('/');
    fn component(s: &str) -> Option<u8> {
        if s.is_empty() || s.len() > 4 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let value = u32::from_str_radix(s, 16).ok()?;
        let max = (1u32 << (s.len() * 4)) - 1;
        Some(((value * 255 + max / 2) / max) as u8)
    }
    let color = (
        component(rgb.next()?)?,
        component(rgb.next()?)?,
        component(rgb.next()?)?,
    );
    if rgb.next().is_some() || fields.next().is_some() {
        return None;
    }
    Some(InternalEvent::TerminalColor(kind, slot, color))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_format_boundaries_and_unknown_osc() {
        assert_eq!(
            parse_color(b"11;#aB12fF"),
            Some(InternalEvent::TerminalColor(11, None, (171, 18, 255)))
        );
        assert_eq!(
            parse_color(b"4;15;#000000"),
            Some(InternalEvent::TerminalColor(4, Some(15), (0, 0, 0)))
        );
        for payload in [
            b"4;16;#000000".as_slice(),
            b"4;255;rgb:f/f/f",
            b"10;#fff",
            b"10;#gg0000",
            b"10;#ffffff;extra",
            b"12;rgb:f/f/f",
        ] {
            assert_eq!(parse_color(payload), None);
        }
        let mut p = Parser::default();
        p.advance(b"\x1b]999;ignored\x07q", false);
        assert_eq!(
            p.next(),
            Some(InternalEvent::Event(Event::Key(KeyCode::Char('q').into())))
        );
        assert_eq!(p.next(), None);
    }

    #[test]
    fn actual_source_preserves_late_responses_across_timeouts() {
        use crate::event::source::EventSource;
        use crate::terminal::sys::file_descriptor::FileDesc;
        #[cfg(feature = "libc")]
        use std::os::unix::io::IntoRawFd;
        use std::{io::Write, os::unix::net::UnixStream};

        let (input, mut terminal) = UnixStream::pair().unwrap();
        input.set_nonblocking(true).unwrap();
        #[cfg(feature = "libc")]
        let fd = FileDesc::new(input.into_raw_fd(), true);
        #[cfg(not(feature = "libc"))]
        let fd = FileDesc::Owned(input.into());
        let mut source = UnixInternalEventSource::from_file_descriptor(fd).unwrap();
        let wait = Some(Duration::from_millis(10));
        assert_eq!(source.try_read(wait).unwrap(), None);
        terminal.write_all(b"\x1b]10;#123456\x07a").unwrap();
        assert_eq!(
            source.try_read(wait).unwrap(),
            Some(InternalEvent::TerminalColor(10, None, (18, 52, 86)))
        );
        assert_eq!(
            source.try_read(wait).unwrap(),
            Some(InternalEvent::Event(Event::Key(KeyCode::Char('a').into())))
        );
        terminal.write_all(b"\x1b]4;0;rgb:f/0/0\x1b").unwrap();
        assert_eq!(source.try_read(wait).unwrap(), None);
        terminal.write_all(b"\\b").unwrap();
        assert_eq!(
            source.try_read(wait).unwrap(),
            Some(InternalEvent::TerminalColor(4, Some(0), (255, 0, 0)))
        );
        assert_eq!(
            source.try_read(wait).unwrap(),
            Some(InternalEvent::Event(Event::Key(KeyCode::Char('b').into())))
        );
        assert_eq!(source.try_read(wait).unwrap(), None);
    }

    #[test]
    fn blocking_source_escape_is_bounded_and_rearms_unread_data() {
        use crate::event::source::EventSource;
        use crate::terminal::sys::file_descriptor::FileDesc;
        #[cfg(feature = "libc")]
        use std::os::unix::io::IntoRawFd;
        use std::{io::Write, os::unix::net::UnixStream};

        let (input, mut terminal) = UnixStream::pair().unwrap();
        // Keep blocking mode, but make regressions fail rather than hang forever.
        input
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        #[cfg(feature = "libc")]
        let fd = FileDesc::new(input.into_raw_fd(), true);
        #[cfg(not(feature = "libc"))]
        let fd = FileDesc::Owned(input.into());
        let mut source = UnixInternalEventSource::from_file_descriptor(fd).unwrap();
        terminal.write_all(b"\x1b").unwrap();
        let start = Instant::now();
        assert_eq!(
            source.try_read(Some(Duration::from_millis(200))).unwrap(),
            Some(InternalEvent::Event(Event::Key(KeyCode::Esc.into())))
        );
        assert!(start.elapsed() < Duration::from_secs(1));

        // More bytes than a source buffer: the second readiness notification
        // must occur even though the first read did not drain the fd.
        terminal.write_all(&[b'x'; 2048]).unwrap();
        for _ in 0..2048 {
            assert_eq!(
                source.try_read(Some(Duration::from_millis(200))).unwrap(),
                Some(InternalEvent::Event(Event::Key(KeyCode::Char('x').into())))
            );
        }
        assert_eq!(source.try_read(Some(Duration::ZERO)).unwrap(), None);
    }

    #[test]
    fn colors_at_every_split() {
        for response in [
            b"\x1b]10;rgb:ffff/8000/0000\x1b\\".as_slice(),
            b"\x1b]10;rgb:ffff/8000/0000\x07".as_slice(),
        ] {
            for split in 0..=response.len() {
                let mut p = Parser::default();
                p.advance(&response[..split], false);
                p.advance(&response[split..], false);
                assert_eq!(
                    p.next(),
                    Some(InternalEvent::TerminalColor(10, None, (255, 128, 0)))
                );
                assert_eq!(p.next(), None);
            }
        }
    }

    #[test]
    fn escape_and_alt_bracket_are_bounded() {
        for (bytes, key) in [
            (b"\x1b".as_slice(), KeyCode::Esc.into()),
            (
                b"\x1b]".as_slice(),
                KeyEvent::new(KeyCode::Char(']'), KeyModifiers::ALT),
            ),
        ] {
            let mut p = Parser::default();
            p.advance(bytes, false);
            assert_eq!(p.next(), None);
            p.deadline = Some(Instant::now());
            assert_eq!(p.next(), Some(InternalEvent::Event(Event::Key(key))));
        }
    }

    #[test]
    fn malformed_and_oversized_osc_recover() {
        let mut p = Parser::default();
        p.advance(b"\x1b]10;rgb:bad/no/no\x07a", false);
        assert_eq!(
            p.next(),
            Some(InternalEvent::Event(Event::Key(KeyCode::Char('a').into())))
        );
        p.advance(b"\x1b]10;", false);
        p.advance(&[b'x'; 4096], false);
        assert!(p.buffer.len() <= Parser::OSC_LIMIT);
        p.advance(b"\x1b", false);
        p.advance(b"\\b", false);
        assert_eq!(
            p.next(),
            Some(InternalEvent::Event(Event::Key(KeyCode::Char('b').into())))
        );
        p.advance(b"\x1b]10;broken\x1b[A", false);
        assert_eq!(
            p.next(),
            Some(InternalEvent::Event(Event::Key(KeyCode::Up.into())))
        );
    }

    #[test]
    fn query_timeout_does_not_reset_partial_response() {
        let mut p = Parser::default();
        p.advance(b"\x1b]4;3;rgb:f/0/8\x1b", false);
        assert_eq!(p.next(), None);
        assert_eq!(p.wait(Some(Duration::ZERO)), Some(Duration::ZERO));
        p.advance(b"\\z", false);
        assert_eq!(
            p.next(),
            Some(InternalEvent::TerminalColor(4, Some(3), (255, 0, 136)))
        );
        assert_eq!(
            p.next(),
            Some(InternalEvent::Event(Event::Key(KeyCode::Char('z').into())))
        );
    }

    #[test]
    fn bytewise_palette_and_ordinary_input() {
        let mut p = Parser::default();
        for byte in b"a\x1b]4;15;rgb:12/345/6789\x1b\\\x1b[A\xc3\xa9" {
            p.advance(&[*byte], false);
        }
        assert_eq!(
            p.next(),
            Some(InternalEvent::Event(Event::Key(KeyCode::Char('a').into())))
        );
        assert_eq!(
            p.next(),
            Some(InternalEvent::TerminalColor(4, Some(15), (18, 52, 103)))
        );
        assert_eq!(
            p.next(),
            Some(InternalEvent::Event(Event::Key(KeyCode::Up.into())))
        );
        assert_eq!(
            p.next(),
            Some(InternalEvent::Event(Event::Key(KeyCode::Char('é').into())))
        );
        assert_eq!(p.next(), None);
    }

    #[test]
    fn incomplete_osc_expires_without_retaining_input() {
        let mut p = Parser::default();
        p.advance(b"\x1b]10;rgb:ffff/", false);
        p.deadline = Some(Instant::now());
        assert_eq!(p.next(), None);
        assert!(p.buffer.is_empty());
        p.advance(b"x", false);
        assert_eq!(
            p.next(),
            Some(InternalEvent::Event(Event::Key(KeyCode::Char('x').into())))
        );
    }

    #[cfg(feature = "bracketed-paste")]
    #[test]
    fn pasted_osc_is_not_a_response() {
        let mut p = Parser::default();
        p.advance(b"\x1b[200~\x1b]10;rgb:f/f/f\x07\x1b[201~", false);
        assert_eq!(
            p.next(),
            Some(InternalEvent::Event(Event::Paste(
                "\x1b]10;rgb:f/f/f\x07".into()
            )))
        );
    }
}
