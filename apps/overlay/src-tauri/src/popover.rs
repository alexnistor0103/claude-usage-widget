//! Menu bar mode: the main window doubles as a popover that lives only while
//! it holds focus. Nothing here touches a window directly — `tray::hide` does,
//! and the focus event that calls it arrives on the main thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager};

use crate::settings::{self, Mode};

/// A tray click that lands this soon after a focus-loss hide is the click that
/// caused the hide: the user meant "close", so it must not reopen.
const REOPEN_GUARD: Duration = Duration::from_millis(400);

#[derive(Default)]
pub struct Popover {
    last_auto_hide: Mutex<Option<Instant>>,
    /// A modal (connect, disconnect, settings) hands focus to a browser or a
    /// dialog; the popover must survive that.
    modal_open: AtomicBool,
}

pub fn active(app: &AppHandle) -> bool {
    app.try_state::<Mutex<settings::Settings>>()
        .and_then(|s| s.lock().ok().map(|s| s.mode == Mode::MenuBar))
        .unwrap_or(false)
}

pub fn set_modal_open(app: &AppHandle, on: bool) {
    if let Some(p) = app.try_state::<Popover>() {
        p.modal_open.store(on, Ordering::Relaxed);
    }
}

/// `WindowEvent::Focused(false)` on the main window.
pub fn on_focus_lost(app: &AppHandle) {
    {
        let Some(p) = app.try_state::<Popover>() else {
            return;
        };
        if !active(app) || p.modal_open.load(Ordering::Relaxed) {
            return;
        }
        let _ = p
            .last_auto_hide
            .lock()
            .map(|mut t| *t = Some(Instant::now()));
    }
    crate::tray::hide(app);
}

pub fn just_auto_hidden(app: &AppHandle) -> bool {
    app.try_state::<Popover>()
        .and_then(|p| p.last_auto_hide.lock().ok().and_then(|t| *t))
        .is_some_and(|t| t.elapsed() < REOPEN_GUARD)
}
