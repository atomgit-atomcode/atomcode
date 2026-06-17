//! Windows conhost mouse capture helpers, used by RetainedRenderer to
//! set/clear `ENABLE_MOUSE_INPUT` while preserving the pre-enter console
//! mode.

#![cfg(windows)]

/// Clear `ENABLE_QUICK_EDIT_MODE` on STD_INPUT_HANDLE (setting the required
/// `ENABLE_EXTENDED_FLAGS` in the same call so the change is honored) and
/// write the result back. This is the fix for the legacy-conhost
/// click-to-freeze: with QuickEdit on (the conhost default) a single click
/// puts the console window into "Select"/Mark mode, which *pauses the
/// application's stdout writes* until the selection is dismissed. atomcode's
/// render worker then blocks mid-flush on its next `WriteFile`, freezing the
/// whole UI; pressing Enter copies + dismisses the selection and unblocks it.
///
/// It deliberately does NOT set `ENABLE_MOUSE_INPUT` / `ENABLE_WINDOW_INPUT`:
/// atomcode defers wheel-scroll / selection / copy to the terminal's native
/// handling (see `RetainedRenderer::with_writer`). Native scrollback wheel
/// scrolling is a host-window feature independent of the QuickEdit bit, so it
/// keeps working; enabling mouse input would instead route the wheel into the
/// app and steal it back from the terminal — the regression we avoid.
///
/// Returns the original mode on success so the caller can restore it
/// byte-for-byte via [`restore_conhost_console_in_mode`]; returns `None` if
/// stdin isn't a console (redirected / piped) or Get/SetConsoleMode fails.
/// Under Windows Terminal / VSCode ConPTY there is no host-window Select mode
/// to pause output, so the syscall succeeds but the clear is an inert no-op —
/// making this one code path safe on both conhost and ConPTY with no runtime
/// detection.
///
/// All results are mirrored to `tuix_trace!` (gated on `ATOMCODE_TUIX_LOG`)
/// so a freeze report shows exactly which syscall returned what mask.
pub fn disable_conhost_quick_edit() -> Option<u32> {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_EXTENDED_FLAGS,
        ENABLE_QUICK_EDIT_MODE, STD_INPUT_HANDLE,
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
        // ENABLE_EXTENDED_FLAGS MUST be set in the same SetConsoleMode call
        // or the QUICK_EDIT clear is silently ignored by the console.
        let new_mode = (original | ENABLE_EXTENDED_FLAGS) & !ENABLE_QUICK_EDIT_MODE;
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
