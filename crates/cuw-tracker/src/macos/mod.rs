//! macOS tracker (M5). Names and signatures mirror the `windows` module so the
//! overlay runs one dock body on both platforms.
//!
//! Position comes from a ~10 Hz `CGWindowListCopyWindowInfo` poll, which needs
//! no permission and is the default path; when Accessibility is granted an
//! `AXObserver` on the target application's move/resize notifications wakes the
//! run loop instead and the poll drops to a heartbeat (plan §6). Focus comes
//! from `NSWorkspace.frontmostApplication` on the same tick — the `pump` module
//! header says why the notification was not used. Docking is degraded by
//! default here by design; the UI must say so rather than block.
//!
//! **Handle kinds.** Every `isize` in this module is a `CGWindowID` belonging
//! to some other application, *except* the one handed to
//! `style::set_tool_window` and `style::assert_topmost`, which is the overlay's
//! own `NSWindow` pointer — what Tauri's `ns_window()` returns. On Windows both
//! happen to be `HWND`s, which is why one shared dock body can pass either.
//!
//! **Coordinates.** [`Bounds`](crate::Bounds) and every [`Rect`](crate::Rect)
//! here are physical pixels with the origin at the top-left of the primary
//! display — what Tauri's `PhysicalPosition` means on macOS — so the overlay's
//! placement math needs no per-platform branch. See [`info::to_bounds`] for the
//! tao convention this matches.
//!
//! **Not here.** Keeping the widget out of Cmd-Tab is an application-level
//! property (`NSApplication`'s activation policy), not a window one, so it
//! belongs to the overlay; `style::set_tool_window` is only the window half.

// Pure, and deliberately not gated: its tests run on every platform, including
// the ones that cannot build the rest of this module.
pub mod info;

#[cfg(target_os = "macos")]
mod ax;
#[cfg(target_os = "macos")]
pub mod bounds;
#[cfg(target_os = "macos")]
pub mod find;
#[cfg(target_os = "macos")]
mod pump;
#[cfg(target_os = "macos")]
pub mod style;

// `Handle` too, so the overlay can name what `start` hands back.
#[cfg(target_os = "macos")]
pub use pump::{Handle, MacosTracker};

/// Whether this process may use the Accessibility API. False is the normal
/// first-run state, not an error: the poll path works without it.
#[cfg(target_os = "macos")]
pub fn accessibility_trusted() -> bool {
    ax::trusted()
}

/// Ask macOS to show the Accessibility grant dialog. Call it from a user
/// action on the UI thread and never from the tracker thread — an unprompted
/// dialog out of a background poll is exactly the sharp edge plan §6 warns
/// about.
#[cfg(target_os = "macos")]
pub fn request_accessibility_prompt() {
    ax::request_prompt();
}
