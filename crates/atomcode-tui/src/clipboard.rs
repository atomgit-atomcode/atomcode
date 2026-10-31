//! Clipboard access, with the platform behind one door.
//!
//! `arboard` covers every desktop this TUI runs on but has no Android backend —
//! its platform module does not even compile there (`platform::Clear` is missing),
//! which is why the dependency is gated to `not(target_os = "android")`.
//!
//! Android has no terminal clipboard for us either: the bundled terminal speaks
//! OSC 52, which the app's WebView does not answer. So the shim reports
//! "unavailable" and every caller already handles that — the copy path falls back
//! to the escape sequence, the paste path reports an empty clipboard.

#[cfg(not(target_os = "android"))]
pub use arboard::Clipboard;

#[cfg(not(target_os = "android"))]
pub use arboard::Error;

/// Android: a clipboard that always says no.
///
/// The surface mirrors the few methods the surface calls, so the call sites need
/// no `cfg` of their own — `Clipboard::new()` failing is a case they already
/// handle (`arboard` itself fails that way on a headless box).
#[cfg(target_os = "android")]
#[derive(Debug)]
pub struct Clipboard;

#[cfg(target_os = "android")]
#[derive(Debug, Clone)]
pub struct Error(&'static str);

#[cfg(target_os = "android")]
impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

#[cfg(target_os = "android")]
impl std::error::Error for Error {}

#[cfg(target_os = "android")]
impl Clipboard {
    pub fn new() -> Result<Self, Error> {
        Err(Error("no clipboard on this platform"))
    }

    pub fn get_text(&mut self) -> Result<String, Error> {
        Err(Error("no clipboard on this platform"))
    }

    pub fn set_text(&mut self, _text: impl Into<String>) -> Result<(), Error> {
        Err(Error("no clipboard on this platform"))
    }
}
