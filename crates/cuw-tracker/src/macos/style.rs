//! Style helpers for the overlay's OWN window, plus the one call that reaches
//! the dock target. Every fn here must run on the main thread
//! (`app.run_on_main_thread`): `NSWindow` is main-thread-only, and
//! `NSRunningApplication` activation is a UI action. Failures are ignored —
//! there is nothing useful to do about them.
//!
//! [`set_tool_window`] and [`assert_topmost`] take the overlay's own
//! `NSWindow` pointer; [`bring_to_foreground`] takes a `CGWindowID`.

use objc2_app_kit::{
    NSApplicationActivationOptions, NSFloatingWindowLevel, NSNormalWindowLevel,
    NSRunningApplication, NSWindow, NSWindowCollectionBehavior,
};

use super::find;

/// The macOS half of `WS_EX_TOOLWINDOW`, at the window level: float above
/// ordinary windows, follow the user across Spaces, and stay visible beside a
/// full-screen app instead of being swept away with its Space.
///
/// Keeping the overlay out of Cmd-Tab is *not* here — that is the application's
/// activation policy (`NSApplication::setActivationPolicy`), which the overlay
/// owns. Unlike Windows, nothing rewrites this behind our back, but the overlay
/// calls it after show/hide anyway so one dock body serves both platforms.
/// `topmost` picks the level: floating above ordinary windows, or a normal
/// window other applications may cover (the always-on-top setting).
pub fn set_tool_window(h: isize, topmost: bool) {
    let Some(window) = ns_window(h) else {
        return;
    };
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary,
    );
    window.setLevel(if topmost {
        NSFloatingWindowLevel
    } else {
        NSNormalWindowLevel
    });
}

/// Re-asserts the floating level. Call on Attached/Restored only, never per
/// Bounds. Main thread only.
pub fn assert_topmost(h: isize) {
    if let Some(window) = ns_window(h) {
        window.setLevel(NSFloatingWindowLevel);
    }
}

/// Hands focus back to the dock target after a modal closes. macOS activates
/// applications rather than windows, so this activates the one that owns the
/// given `CGWindowID`. Main thread only.
pub fn bring_to_foreground(h: isize) {
    let Some(pid) = find::pid_of(h) else {
        return;
    };
    if let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid) {
        app.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows);
    }
}

/// `h` is what Tauri's `ns_window()` returned for our own window.
fn ns_window(h: isize) -> Option<&'static NSWindow> {
    if h == 0 {
        return None;
    }
    // SAFETY: the caller runs on the main thread (see the module header) and
    // `h` is the overlay's own live NSWindow; the reference never escapes.
    Some(unsafe { &*(h as *const NSWindow) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_null_window_pointer_is_ignored_rather_than_dereferenced() {
        set_tool_window(0, true);
        set_tool_window(0, false);
        assert_topmost(0);
    }

    #[test]
    fn bringing_a_stale_window_id_forward_does_nothing() {
        bring_to_foreground(0x7fff_0001);
    }
}
