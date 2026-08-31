//! One set of names for the per-platform tracker, so `dock.rs` has a single
//! body on both platforms rather than two that drift (plan §6).
//!
//! Every handle here is an `isize`. On Windows it is always an `HWND`; on macOS
//! it is a `CGWindowID` for a target window but the overlay's own `NSWindow`
//! pointer for [`style::set_tool_window`] and [`style::assert_topmost`] — the
//! tracker's `macos` module header spells out which call takes which.

#[cfg(windows)]
pub use cuw_tracker::windows::{bounds, find, style, Handle, WindowsTracker as PlatformTracker};

#[cfg(target_os = "macos")]
pub use cuw_tracker::macos::{bounds, find, style, Handle, MacosTracker as PlatformTracker};
