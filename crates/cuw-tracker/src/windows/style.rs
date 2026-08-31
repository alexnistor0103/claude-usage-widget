//! Style helpers for the overlay's OWN window. Every fn here must run on the
//! main thread (`app.run_on_main_thread`): `SetWindowPos(SWP_FRAMECHANGED)` on
//! a window another thread owns is a synchronous `SendMessage` to that thread
//! (plan §6). Failures are ignored — there is nothing useful to do about them.

use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE,
    HWND_NOTOPMOST, HWND_TOP, HWND_TOPMOST, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, WS_EX_TOOLWINDOW,
};

use super::find::hwnd;

/// ORs `WS_EX_TOOLWINDOW` into the ex-style (hides the overlay from Alt-Tab)
/// and asserts the z-order `topmost` asks for (the always-on-top setting).
/// tao rewrites the ex-style on every flag diff, so call again after any
/// show/hide/set_focusable. Main thread only.
pub fn set_tool_window(h: isize, topmost: bool) {
    // SAFETY: plain style read/write on a handle we own; SetWindowPos only
    // re-applies the frame and the z-order band (no move, size or activation).
    unsafe {
        let ex = GetWindowLongPtrW(hwnd(h), GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd(h), GWL_EXSTYLE, ex | WS_EX_TOOLWINDOW.0 as isize);
        let _ = SetWindowPos(
            hwnd(h),
            Some(if topmost {
                HWND_TOPMOST
            } else {
                HWND_NOTOPMOST
            }),
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// Re-asserts `HWND_TOPMOST` without activating. Call on Attached/Restored
/// only, never per Bounds. Main thread only.
pub fn assert_topmost(h: isize) {
    // SAFETY: z-order change only; no move, size or activation.
    let _ = unsafe {
        SetWindowPos(
            hwnd(h),
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )
    };
}

/// Raises the overlay to the top of the non-topmost band without activating —
/// how a non-topmost docked widget gets above its target when that target
/// comes forward. Main thread only.
pub fn raise_to_top(h: isize) {
    // SAFETY: z-order change only; no move, size or activation.
    let _ = unsafe {
        SetWindowPos(
            hwnd(h),
            Some(HWND_TOP),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )
    };
}

/// Hands focus to `h` (the dock target after a modal closes). Windows only
/// honours this while our process is the foreground process. Main thread only.
pub fn bring_to_foreground(h: isize) {
    // SAFETY: defined for any handle value; a refused request just returns FALSE.
    let _ = unsafe { SetForegroundWindow(hwnd(h)) };
}
