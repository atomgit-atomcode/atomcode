//! Windows conhost mouse capture helpers, used by RetainedRenderer to
//! set/clear `ENABLE_MOUSE_INPUT` while preserving the pre-enter console
//! mode.

#![cfg(windows)]

/// `ENABLE_MOUSE_INPUT` (0x0010): when set, the console queues mouse events
/// (incl. wheel) into the input buffer for the app to read; when CLEAR, the
/// conhost window handles the wheel itself and scrolls its scrollback buffer.
/// Mirrored here (rather than only imported from `windows_sys`) so the pure
/// mask transform below — and its test — stay simple; a unit test asserts this
/// equals the `windows_sys` constant so the mirror cannot drift.
const ENABLE_MOUSE_INPUT_BIT: u32 = 0x0010;
/// `ENABLE_QUICK_EDIT_MODE` (0x0040): conhost text selection. ON is the default
/// and is what lets a click enter "Select" mode (which pauses our stdout writes
/// and freezes the render worker).
const ENABLE_QUICK_EDIT_MODE_BIT: u32 = 0x0040;
/// `ENABLE_EXTENDED_FLAGS` (0x0080): must be set in the same `SetConsoleMode`
/// call or a QuickEdit-mode change is silently ignored by the console.
const ENABLE_EXTENDED_FLAGS_BIT: u32 = 0x0080;

/// Compute the STD_INPUT console mode atomcode wants while it owns the console,
/// from the mode read at startup. We clear BOTH:
///   * `ENABLE_QUICK_EDIT_MODE` — kills the legacy-conhost click-to-freeze, and
///   * `ENABLE_MOUSE_INPUT` — so the wheel is NOT routed to the app (whose
///     `scroll_body` is a deliberate no-op, since the body lives in native
///     scrollback). With mouse input off, the conhost window scrolls its
///     scrollback on the wheel itself — restoring native wheel scrolling.
///
/// Clearing QuickEdit WITHOUT also clearing mouse input was the v4.25.2
/// regression: conhost stopped handling the wheel (QuickEdit off) yet kept
/// delivering wheel events to the app (mouse input still on, the conhost
/// default), which dropped them — so the wheel did nothing at all.
///
/// `ENABLE_EXTENDED_FLAGS` is set so the QuickEdit clear is honored. Every
/// other bit is preserved, so [`restore_conhost_console_in_mode`] puts the
/// original mode back byte-for-byte on exit.
fn managed_console_in_mode(original: u32) -> u32 {
    (original | ENABLE_EXTENDED_FLAGS_BIT) & !(ENABLE_QUICK_EDIT_MODE_BIT | ENABLE_MOUSE_INPUT_BIT)
}

/// Clear `ENABLE_QUICK_EDIT_MODE` on STD_INPUT_HANDLE (setting the required
/// `ENABLE_EXTENDED_FLAGS` in the same call so the change is honored) and
/// write the result back. This is the fix for the legacy-conhost
/// click-to-freeze: with QuickEdit on (the conhost default) a single click
/// puts the console window into "Select"/Mark mode, which *pauses the
/// application's stdout writes* until the selection is dismissed. atomcode's
/// render worker then blocks mid-flush on its next `WriteFile`, freezing the
/// whole UI; pressing Enter copies + dismisses the selection and unblocks it.
///
/// It also CLEARS `ENABLE_MOUSE_INPUT` (it never sets it). conhost's default
/// input mode has mouse input ON, and with QuickEdit on, conhost handles the
/// wheel itself (scrolling scrollback). Clearing QuickEdit alone — as v4.25.2
/// did — stops conhost handling the wheel yet leaves mouse input on, so wheel
/// ticks get delivered to the app instead; atomcode's `scroll_body` is a
/// deliberate no-op (the body lives in native scrollback), so the wheel did
/// nothing at all. Clearing mouse input too hands the wheel back to the conhost
/// window, restoring native scrollback scrolling. `ENABLE_WINDOW_INPUT` is left
/// untouched. See [`managed_console_in_mode`] for the exact bit math.
///
/// Returns the original mode on success so the caller can restore it
/// byte-for-byte via [`restore_conhost_console_in_mode`]; returns `None` if
/// stdin isn't a console (redirected / piped) or Get/SetConsoleMode fails.
///
/// CALLERS MUST GATE THIS TO LEGACY CONHOST (`caps.legacy_conhost`). On Windows
/// Terminal / VSCode ConPTY, clearing QuickEdit is NOT an inert no-op (the
/// original assumption, now known wrong): ConPTY reacts by enabling mouse-event
/// forwarding, so the terminal stops doing native click-drag text selection —
/// the user can no longer select/copy and clicks hit atomcode's no-op handler
/// ("mouse dead / can't copy on Windows Terminal"). The click-to-freeze this
/// fixes is conhost-only, so skipping it on ConPTY is correct regardless.
///
/// All results are mirrored to `tuix_trace!` (gated on `ATOMCODE_TUIX_LOG`)
/// so a freeze report shows exactly which syscall returned what mask.
pub fn disable_conhost_quick_edit() -> Option<u32> {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, STD_INPUT_HANDLE,
    };
    unsafe {
        let h = GetStdHandle(STD_INPUT_HANDLE);
        // GetStdHandle returns INVALID_HANDLE_VALUE (`!0 as HANDLE`) on
        // failure; on Windows that's `-1isize as *mut c_void`. Treat
        // null and "all bits set" as failure shapes.
        if h.is_null() || h as isize == -1 {
            crate::tuix_trace!("REN", "conhost-quickedit: GetStdHandle returned invalid");
            return None;
        }
        let mut original: u32 = 0;
        if GetConsoleMode(h, &mut original) == 0 {
            let err = std::io::Error::last_os_error();
            crate::tuix_trace!("REN", "conhost-quickedit: GetConsoleMode failed: {}", err);
            return None;
        }
        // Clear QuickEdit (kills click-to-freeze) AND mouse input (so the
        // wheel scrolls native scrollback instead of being routed to the app's
        // no-op handler — the v4.25.2 regression). ENABLE_EXTENDED_FLAGS is set
        // in the same call or the QuickEdit clear is silently ignored.
        let new_mode = managed_console_in_mode(original);
        if SetConsoleMode(h, new_mode) == 0 {
            let err = std::io::Error::last_os_error();
            crate::tuix_trace!(
                "REN",
                "conhost-quickedit: SetConsoleMode(0x{:08x}) failed: {}",
                new_mode,
                err
            );
            return None;
        }
        crate::tuix_trace!(
            "REN",
            "conhost-quickedit: ok prev=0x{:08x} new=0x{:08x}",
            original,
            new_mode
        );
        Some(original)
    }
}

/// Restore STD_INPUT_HANDLE's console mode to the value `enable_conhost_
/// mouse_capture` returned. Best-effort — failure here just means the
/// shell mode bits drift slightly on exit; better than aborting.
pub fn restore_conhost_console_in_mode(prior: u32) {
    use windows_sys::Win32::System::Console::{GetStdHandle, SetConsoleMode, STD_INPUT_HANDLE};
    unsafe {
        let h = GetStdHandle(STD_INPUT_HANDLE);
        if h.is_null() || h as isize == -1 {
            return;
        }
        let _ = SetConsoleMode(h, prior);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The local mirror constants must match the real Win32 values, or the
    /// pure mask transform would compute the wrong mode at runtime.
    #[test]
    fn mirror_constants_match_windows_sys() {
        use windows_sys::Win32::System::Console::{
            ENABLE_EXTENDED_FLAGS, ENABLE_MOUSE_INPUT, ENABLE_QUICK_EDIT_MODE,
        };
        assert_eq!(ENABLE_MOUSE_INPUT_BIT, ENABLE_MOUSE_INPUT);
        assert_eq!(ENABLE_QUICK_EDIT_MODE_BIT, ENABLE_QUICK_EDIT_MODE);
        assert_eq!(ENABLE_EXTENDED_FLAGS_BIT, ENABLE_EXTENDED_FLAGS);
    }

    /// REGRESSION (v4.25.2 dead mouse wheel): the managed mode must clear BOTH
    /// QuickEdit AND mouse input (so conhost handles the wheel natively), set
    /// ENABLE_EXTENDED_FLAGS, and preserve every other bit for a faithful
    /// restore. Clearing only QuickEdit left mouse input on → wheel routed to
    /// the app's no-op handler → wheel did nothing.
    #[test]
    fn managed_mode_clears_quickedit_and_mouse_sets_extended_preserves_rest() {
        // A representative conhost default: mouse input + quick edit + extended
        // flags on, plus some unrelated bits we must not disturb.
        let processed = 0x0001; // ENABLE_PROCESSED_INPUT
        let window = 0x0008; // ENABLE_WINDOW_INPUT
        let original = processed
            | window
            | ENABLE_MOUSE_INPUT_BIT
            | ENABLE_QUICK_EDIT_MODE_BIT
            | ENABLE_EXTENDED_FLAGS_BIT;

        let managed = managed_console_in_mode(original);

        assert_eq!(managed & ENABLE_QUICK_EDIT_MODE_BIT, 0, "QuickEdit must be cleared");
        assert_eq!(managed & ENABLE_MOUSE_INPUT_BIT, 0, "mouse input must be cleared");
        assert_eq!(
            managed & ENABLE_EXTENDED_FLAGS_BIT,
            ENABLE_EXTENDED_FLAGS_BIT,
            "extended flags must be set or the change is ignored"
        );
        assert_eq!(managed & processed, processed, "unrelated bits preserved");
        assert_eq!(managed & window, window, "unrelated bits preserved");
    }

    /// Extended flags must be SET even when the original mode had it off, so a
    /// console that doesn't report it by default still honors the clear.
    #[test]
    fn managed_mode_sets_extended_flags_when_absent() {
        let original = ENABLE_MOUSE_INPUT_BIT | ENABLE_QUICK_EDIT_MODE_BIT; // no extended flag
        let managed = managed_console_in_mode(original);
        assert_eq!(managed & ENABLE_EXTENDED_FLAGS_BIT, ENABLE_EXTENDED_FLAGS_BIT);
        assert_eq!(managed & ENABLE_QUICK_EDIT_MODE_BIT, 0);
        assert_eq!(managed & ENABLE_MOUSE_INPUT_BIT, 0);
    }
}
