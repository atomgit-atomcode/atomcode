//! Console-record input and pointer encoding belong to the terminal surface.
//! No input characters are interpreted or filtered here.

use std::io::{self, Write};

/// Apply native capture first, then explicitly select the terminal encoding.
/// Disabling always attempts both halves, even if native restoration fails.
pub(crate) fn pointer(
    writer: &mut impl Write,
    mode: crate::ansi::Pointer,
    capture: impl FnMut(bool) -> io::Result<()>,
) -> io::Result<()> {
    pointer_with_alternate_scroll(
        writer,
        mode,
        capture,
        crate::ansi::alternate_scroll_enabled(),
    )
}

fn pointer_with_alternate_scroll(
    writer: &mut impl Write,
    mode: crate::ansi::Pointer,
    mut capture: impl FnMut(bool) -> io::Result<()>,
    native_alternate_scroll: bool,
) -> io::Result<()> {
    use crate::ansi::{Pointer, MOUSE_ON};
    let enabled = mode != Pointer::Terminal;
    let native = capture(enabled);
    if enabled {
        native?;
        // Hover alone does not select SGR. This must follow the native API:
        // Windows capture can otherwise leave legacy reports arriving as keys.
        let result = (|| {
            writer.write_all(MOUSE_ON.as_bytes())?;
            if mode != Pointer::Buttons {
                writer.write_all(mode.escape().as_bytes())?;
            }
            writer.flush()
        })();
        if result.is_err() {
            let _ = capture(false);
            let _ = writer.write_all(crate::ansi::MOUSE_OFF.as_bytes());
            let _ = writer.flush();
        }
        result
    } else {
        let encoded = (|| {
            writer.write_all(mode.escape().as_bytes())?;
            // HiShell's native wheel needs 1007. Restore its existing policy
            // when handing back after capture disabled arrow translation.
            if native_alternate_scroll {
                writer.write_all(crate::ansi::ALT_SCROLL_ON.as_bytes())?;
            }
            writer.flush()
        })();
        native.and(encoded)
    }
}

#[cfg(any(windows, test))]
const VT_INPUT: u32 = 0x0200;

#[cfg(any(windows, test))]
fn mouse_mode(current: u32, original: u32, on: bool) -> u32 {
    const MOUSE: u32 = 0x0010;
    const WINDOW: u32 = 0x0008;
    const QUICK_EDIT: u32 = 0x0040;
    const EXTENDED: u32 = 0x0080;
    let capture_flags = MOUSE | WINDOW | QUICK_EDIT | EXTENDED;
    if on {
        (current | MOUSE | WINDOW | EXTENDED) & !(QUICK_EDIT | VT_INPUT)
    } else {
        ((current & !capture_flags) | (original & capture_flags)) & !VT_INPUT
    }
}

#[cfg(any(windows, test))]
trait Console {
    fn mode(&self) -> io::Result<u32>;
    fn set_mode(&self, mode: u32) -> io::Result<()>;
}

#[cfg(any(windows, test))]
fn ensure(console: &impl Console) -> io::Result<()> {
    let mode = console.mode()?;
    let wanted = mode & !VT_INPUT;
    if wanted != mode {
        console.set_mode(wanted)?;
    }
    Ok(())
}

#[cfg(any(windows, test))]
struct InputMode<C: Console> {
    console: C,
    original: u32,
}

#[cfg(any(windows, test))]
impl<C: Console> InputMode<C> {
    fn take(console: C) -> io::Result<Self> {
        let original = console.mode()?;
        // Preserve all unrelated flags. Raw mode and mouse capture are separate
        // operations, and must not overwrite the snapshot taken before either.
        if original & VT_INPUT != 0 {
            console.set_mode(original & !VT_INPUT)?;
        }
        Ok(Self { console, original })
    }
    fn restore(&self) -> io::Result<()> {
        self.console.set_mode(self.original)
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    type Handle = *mut core::ffi::c_void;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(which: u32) -> Handle;
        fn GetConsoleMode(handle: Handle, mode: *mut u32) -> i32;
        fn SetConsoleMode(handle: Handle, mode: u32) -> i32;
    }
    // Borrowed process console handle, stored as an integer so the saved mode
    // can be restored by the panic hook on another thread. Never closed here.
    struct InputConsole(usize);
    impl InputConsole {
        fn current() -> io::Result<Self> {
            // SAFETY: STD_INPUT_HANDLE; the returned handle remains borrowed.
            let handle = unsafe { GetStdHandle(-10i32 as u32) };
            if handle.is_null() || handle as isize == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(handle as usize))
        }
    }
    impl Console for InputConsole {
        fn mode(&self) -> io::Result<u32> {
            let mut mode = 0;
            // SAFETY: borrowed console handle and a valid local output pointer.
            if unsafe { GetConsoleMode(self.0 as Handle, &mut mode) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(mode)
        }
        fn set_mode(&self, mode: u32) -> io::Result<()> {
            // SAFETY: borrowed handle; SetConsoleMode keeps no pointer.
            if unsafe { SetConsoleMode(self.0 as Handle, mode) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }
    static SAVED: std::sync::Mutex<Option<InputMode<InputConsole>>> = std::sync::Mutex::new(None);

    pub(super) fn take() -> io::Result<()> {
        let mut saved = SAVED.lock().unwrap_or_else(|e| e.into_inner());
        if saved.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "terminal input mode is already owned",
            ));
        }
        *saved = Some(InputMode::take(InputConsole::current()?)?);
        Ok(())
    }
    pub(super) fn check() -> io::Result<()> {
        // The input reader can outlive a screen handoff by one poll. Do not
        // change the shell's mode after the surface has returned ownership.
        let saved = SAVED.lock().unwrap_or_else(|e| e.into_inner());
        if saved.is_some() {
            ensure(&InputConsole::current()?)?;
        }
        Ok(())
    }
    pub(super) fn capture(on: bool) -> io::Result<()> {
        let saved = SAVED.lock().unwrap_or_else(|e| e.into_inner());
        let Some(saved) = saved.as_ref() else {
            return Ok(());
        };
        let current = saved.console.mode()?;
        let wanted = mouse_mode(current, saved.original, on);
        if wanted != current {
            saved.console.set_mode(wanted)?;
        }
        Ok(())
    }
    pub(super) fn restore() -> io::Result<()> {
        let mut saved = SAVED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mode) = saved.as_ref() {
            mode.restore()?;
        }
        *saved = None;
        Ok(())
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        use std::fs::OpenOptions;
        use std::os::windows::{io::AsRawHandle, process::CommandExt};
        const CHILD: &str = "ATOMCODE_WINDOWS_INPUT_MODE_TEST_CHILD";
        #[link(name = "kernel32")]
        extern "system" {
            fn SetStdHandle(which: u32, handle: Handle) -> i32;
        }

        #[test]
        fn an_isolated_windows_console_restores_input_and_mouse_modes() {
            if std::env::var_os(CHILD).is_none() {
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "terminal_modes::windows::tests::an_isolated_windows_console_restores_input_and_mouse_modes", "--nocapture"])
                    .env(CHILD, "1").creation_flags(0x00000010).output().unwrap();
                assert!(
                    output.status.success()
                        && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                    "stdout={} stderr={}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                return;
            }
            let input = OpenOptions::new()
                .read(true)
                .write(true)
                .open("CONIN$")
                .unwrap();
            // SAFETY: an owned console handle kept alive for this entire child test.
            assert_ne!(
                unsafe { SetStdHandle(-10i32 as u32, input.as_raw_handle()) },
                0
            );
            let console = InputConsole::current().unwrap();
            let original = console.mode().unwrap() | VT_INPUT;
            console.set_mode(original).unwrap();
            crate::terminal_modes::take_input().unwrap();
            assert_eq!(console.mode().unwrap() & VT_INPUT, 0);
            console.set_mode(console.mode().unwrap() & !7).unwrap(); // raw input
            let mut out = Vec::new();
            crate::terminal_modes::pointer(&mut out, crate::ansi::Pointer::ButtonsAndHover, |on| {
                capture(on)?;
                if on {
                    assert_ne!(console.mode()? & 0x10, 0);
                }
                Ok(())
            })
            .unwrap();
            assert!(String::from_utf8(out).unwrap().contains("\x1b[?1006h"));
            assert_eq!(console.mode().unwrap() & (7 | VT_INPUT), 0);
            crate::terminal_modes::pointer(
                &mut Vec::new(),
                crate::ansi::Pointer::Terminal,
                capture,
            )
            .unwrap();
            assert_eq!(console.mode().unwrap(), original & !(7 | VT_INPUT));
            crate::terminal_modes::restore_input().unwrap();
            crate::terminal_modes::ensure_input().unwrap(); // no ownership after restoration
            assert_eq!(console.mode().unwrap(), original);
        }
    }
}

pub(crate) fn take_input() -> io::Result<()> {
    #[cfg(windows)]
    windows::take()?;
    Ok(())
}
pub(crate) fn ensure_input() -> io::Result<()> {
    #[cfg(windows)]
    windows::check()?;
    Ok(())
}
pub(crate) fn restore_input() -> io::Result<()> {
    #[cfg(windows)]
    windows::restore()?;
    Ok(())
}

pub(crate) fn capture_mouse(on: bool) -> io::Result<()> {
    #[cfg(windows)]
    windows::capture(on)?;
    #[cfg(not(windows))]
    let _ = on;
    Ok(())
}

/// Crossterm consumes Win32 records, even when its reader wakes after Pending.
/// Only a real console-mode error ends the stream; text events are untouched.
pub(crate) fn input_records<S, F>(
    mut events: S,
    mut check: F,
) -> impl futures::Stream<Item = io::Result<crossterm::event::Event>>
where
    S: futures::Stream<Item = io::Result<crossterm::event::Event>> + Unpin,
    F: FnMut() -> io::Result<()>,
{
    let mut ended = false;
    futures::stream::poll_fn(move |cx| {
        use std::{pin::Pin, task::Poll};
        if ended {
            return Poll::Ready(None);
        }
        if let Err(error) = check() {
            ended = true;
            return Poll::Ready(Some(Err(error)));
        }
        let result = Pin::new(&mut events).poll_next(cx);
        if result.is_pending() {
            if let Err(error) = check() {
                ended = true;
                return Poll::Ready(Some(Err(error)));
            }
        }
        if matches!(result, Poll::Ready(None | Some(Err(_)))) {
            ended = true;
        }
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    #[derive(Clone)]
    struct FakeConsole {
        mode: Rc<Cell<u32>>,
        fail: Rc<Cell<bool>>,
    }
    impl Console for FakeConsole {
        fn mode(&self) -> io::Result<u32> {
            Ok(self.mode.get())
        }
        fn set_mode(&self, mode: u32) -> io::Result<()> {
            if self.fail.get() {
                return Err(io::Error::other("mode refused"));
            }
            self.mode.set(mode);
            Ok(())
        }
    }
    fn console(mode: u32) -> FakeConsole {
        FakeConsole {
            mode: Rc::new(Cell::new(mode)),
            fail: Rc::new(Cell::new(false)),
        }
    }

    #[test]
    fn input_records_preserve_flags_and_restore_the_exact_original_mode() {
        for original in [0, 0x01f7, 0x03f7, 0x0200] {
            let console = console(original);
            let saved = InputMode::take(console.clone()).unwrap();
            assert_eq!(console.mode.get(), original & !VT_INPUT);
            // Model raw input and a child that subsequently turned VT input on.
            console.mode.set((console.mode.get() & !7) | VT_INPUT);
            ensure(&console).unwrap();
            assert_eq!(console.mode.get(), original & !(VT_INPUT | 7));
            saved.restore().unwrap();
            assert_eq!(console.mode.get(), original);
        }
    }

    #[test]
    fn input_mode_failure_is_not_a_successful_capture_or_restore() {
        let console = console(0x03f7);
        console.fail.set(true);
        assert!(InputMode::take(console.clone()).is_err());
        assert_eq!(console.mode.get(), 0x03f7);
        console.fail.set(false);
        let saved = InputMode::take(console.clone()).unwrap();
        console.fail.set(true);
        assert!(saved.restore().is_err());
        console.fail.set(false);
        saved.restore().unwrap();
        assert_eq!(console.mode.get(), 0x03f7);
    }

    #[test]
    fn native_mouse_changes_do_not_restore_cooked_or_vt_input_mid_screen() {
        let original = 0x03f7;
        let raw = original & !(7 | VT_INPUT);
        let on = mouse_mode(raw, original, true);
        assert_ne!(on & 0x10, 0);
        assert_eq!(on & (0x40 | 7 | VT_INPUT), 0);
        let off = mouse_mode(on, original, false);
        assert_eq!(off, raw);
        assert_eq!(mouse_mode(off, original, true), on);
    }

    struct Writer {
        log: Rc<RefCell<Vec<String>>>,
        fail: bool,
        bytes: Vec<u8>,
    }
    impl Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.log
                .borrow_mut()
                .push(String::from_utf8_lossy(bytes).into_owned());
            if self.fail {
                self.fail = false;
                return Err(io::Error::other("write refused"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.log.borrow_mut().push("flush".into());
            Ok(())
        }
    }
    #[test]
    fn native_handoff_restores_only_the_platforms_alternate_scroll_policy() {
        for native_scroll in [false, true] {
            let mut bytes = Vec::new();
            pointer_with_alternate_scroll(
                &mut bytes,
                crate::ansi::Pointer::Terminal,
                |_| Ok(()),
                native_scroll,
            )
            .unwrap();
            let text = String::from_utf8(bytes).unwrap();
            assert!(text.starts_with(crate::ansi::MOUSE_OFF));
            assert_eq!(text.contains(crate::ansi::ALT_SCROLL_ON), native_scroll);
            let mut captured = Vec::new();
            pointer_with_alternate_scroll(
                &mut captured,
                crate::ansi::Pointer::Buttons,
                |_| Ok(()),
                native_scroll,
            )
            .unwrap();
            let text = String::from_utf8(captured).unwrap();
            assert!(text.starts_with(crate::ansi::ALT_SCROLL_OFF));
            assert!(!text.contains(crate::ansi::ALT_SCROLL_ON));
        }
    }

    #[test]
    fn native_capture_precedes_sgr_and_hover_always_gets_sgr() {
        for mode in [
            crate::ansi::Pointer::Buttons,
            crate::ansi::Pointer::ButtonsAndHover,
        ] {
            let log = Rc::new(RefCell::new(Vec::new()));
            let mut out = Writer {
                log: log.clone(),
                fail: false,
                bytes: vec![],
            };
            pointer(&mut out, mode, |on| {
                log.borrow_mut().push(format!("native={on}"));
                Ok(())
            })
            .unwrap();
            assert_eq!(log.borrow()[0], "native=true");
            let bytes = String::from_utf8(out.bytes).unwrap();
            assert!(bytes.contains("\x1b[?1006h"));
            if mode == crate::ansi::Pointer::ButtonsAndHover {
                assert!(bytes.ends_with(crate::ansi::MOUSE_MOTION_ON));
            }
            assert_eq!(log.borrow().last().unwrap(), "flush");
        }
    }
    #[test]
    fn failed_sgr_setup_releases_native_capture_and_resets_the_encoding() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut out = Writer {
            log: log.clone(),
            fail: true,
            bytes: vec![],
        };
        assert!(pointer(&mut out, crate::ansi::Pointer::Buttons, |on| {
            log.borrow_mut().push(format!("native={on}"));
            Ok(())
        })
        .is_err());
        assert!(log.borrow().iter().any(|entry| entry == "native=false"));
        assert_eq!(out.bytes, crate::ansi::MOUSE_OFF.as_bytes());
    }
    #[test]
    fn native_cleanup_failure_does_not_skip_the_terminal_reset() {
        let mut out = Vec::new();
        assert!(pointer(&mut out, crate::ansi::Pointer::Terminal, |_| Err(
            io::Error::other("restore refused")
        ))
        .is_err());
        assert_eq!(out, crate::ansi::MOUSE_OFF.as_bytes());
        let mut out = Vec::new();
        assert!(pointer(&mut out, crate::ansi::Pointer::Buttons, |_| Err(
            io::Error::other("capture refused")
        ))
        .is_err());
        assert!(
            out.is_empty(),
            "do not enable reports after native setup failed"
        );
    }

    #[tokio::test]
    async fn record_input_does_not_filter_mouse_like_plain_text_or_pastes() {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        use futures::StreamExt;
        let text = "!#C!#A!#?!#>";
        let mut original: Vec<Event> = text
            .chars()
            .map(|c| Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)))
            .collect();
        original.push(Event::Paste(text.into()));
        let events = futures::stream::iter(original.iter().cloned().map(Ok).collect::<Vec<_>>());
        let results: Vec<_> = input_records(events, || Ok(())).collect().await;
        assert_eq!(
            results.into_iter().map(Result::unwrap).collect::<Vec<_>>(),
            original
        );
    }
    #[test]
    fn a_pending_reader_reasserts_input_mode_and_stops_on_failure() {
        use futures::Stream;
        use std::{
            pin::Pin,
            task::{Context, Poll},
        };
        let checks = Cell::new(0);
        let mut events = input_records(futures::stream::pending(), || {
            checks.set(checks.get() + 1);
            if checks.get() == 2 {
                Err(io::Error::other("mode refused after Pending"))
            } else {
                Ok(())
            }
        });
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert!(matches!(
            Pin::new(&mut events).poll_next(&mut cx),
            Poll::Ready(Some(Err(_)))
        ));
        assert_eq!(checks.get(), 2);
        assert!(matches!(
            Pin::new(&mut events).poll_next(&mut cx),
            Poll::Ready(None)
        ));
    }
}
