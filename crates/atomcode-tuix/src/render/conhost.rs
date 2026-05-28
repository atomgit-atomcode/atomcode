//! Windows conhost mouse capture helpers, used by RetainedRenderer to
//! set/clear `ENABLE_MOUSE_INPUT` while preserving the pre-enter console
//! mode.

#![cfg(windows)]

/// Enable `ENABLE_MOUSE_INPUT | ENABLE_EXTENDED_FLAGS | ENABLE_WINDOW_INPUT`
/// on STD_INPUT_HANDLE, clear `ENABLE_QUICK_EDIT_MODE` so the user can't
/// accidentally drag-select and freeze the terminal, and write the result
/// back. Returns the original mode on success so the caller can restore it
/// byte-for-byte; returns `None` if either GetConsoleMode or SetConsoleMode
/// fails (typically: stdin was redirected and isn't a console handle, e.g.
/// running under a pipe).
///
/// All results are mirrored to `tuix_trace!` (gated on
/// `ATOMCODE_TUIX_LOG`) so a "wheel still doesn't work" report shows
/// exactly which syscall returned what mask.
pub fn enable_conhost_mouse_capture() -> Option<u32> {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_EXTENDED_FLAGS, ENABLE_MOUSE_INPUT,
        ENABLE_QUICK_EDIT_MODE, ENABLE_WINDOW_INPUT, STD_INPUT_HANDLE,
    };
    unsafe {
        let h = GetStdHandle(STD_INPUT_HANDLE);
        // GetStdHandle returns INVALID_HANDLE_VALUE (`!0 as HANDLE`) on
        // failure; on Windows that's `-1isize as *mut c_void`. Treat
        // null and "all bits set" as failure shapes.
        if h.is_null() || h as isize == -1 {
            crate::tuix_trace!("REN", "conhost-mouse: GetStdHandle returned invalid");
            return None;
        }
        let mut original: u32 = 0;
        if GetConsoleMode(h, &mut original) == 0 {
            let err = std::io::Error::last_os_error();
            crate::tuix_trace!("REN", "conhost-mouse: GetConsoleMode failed: {}", err);
            return None;
        }
        let new_mode =
            (original | ENABLE_MOUSE_INPUT | ENABLE_EXTENDED_FLAGS | ENABLE_WINDOW_INPUT)
                & !ENABLE_QUICK_EDIT_MODE;
        if SetConsoleMode(h, new_mode) == 0 {
            let err = std::io::Error::last_os_error();
            crate::tuix_trace!(
                "REN",
                "conhost-mouse: SetConsoleMode(0x{:08x}) failed: {}",
                new_mode,
                err
            );
            return None;
        }
        crate::tuix_trace!(
            "REN",
            "conhost-mouse: ok prev=0x{:08x} new=0x{:08x}",
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
