//! Event-driven tracker: `SetWinEventHook` with `WINEVENT_OUTOFCONTEXT` (no DLL
//! injection). Target-scoped hooks for move/size/minimize, one global hook for
//! `EVENT_SYSTEM_FOREGROUND`. Geometry from `DWMWA_EXTENDED_FRAME_BOUNDS`, not
//! `GetWindowRect` (plan §6). `HWND` crosses every boundary here as `isize`.

pub mod bounds;
pub mod find;
mod hook;
pub mod style;

// `Handle` too, so the overlay can name what `start` hands back.
pub use hook::{Handle, WindowsTracker};
