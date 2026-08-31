//! Window enumeration and identity over `CGWindowListCopyWindowInfo`. Every fn
//! takes and returns `isize` `CGWindowID`s and wraps its CoreFoundation calls;
//! nothing here panics on a stale id, and every field is read defensively —
//! the window list is a bag of key/value pairs, not a struct.
//!
//! The fetching stops here: what the fields *mean* lives in
//! [`info`](super::info), which is tested on every platform.

use objc2_app_kit::{NSRunningApplication, NSWorkspace};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType, CGRect,
};
use objc2_core_graphics::{
    kCGNullWindowID, kCGWindowBounds, kCGWindowIsOnscreen, kCGWindowLayer, kCGWindowName,
    kCGWindowNumber, kCGWindowOwnerPID, CGRectMakeWithDictionaryRepresentation,
    CGWindowListCopyWindowInfo, CGWindowListOption,
};

use super::info::{self, AppInfo, RectF, WindowInfo};
use crate::geometry::Candidate;

/// Desktop icons and wallpaper hosts are never dock targets, and excluding them
/// keeps the list short enough to walk at 10 Hz (plan §6).
const ON_SCREEN: CGWindowListOption = CGWindowListOption(
    CGWindowListOption::OptionOnScreenOnly.0 | CGWindowListOption::ExcludeDesktopElements.0,
);

/// Ordinary application windows, front of the Z order first.
pub fn candidates() -> Vec<Candidate> {
    let windows = on_screen();
    let mut apps = AppCache::default();
    windows
        .iter()
        .filter_map(|w| info::window_from_info(w, apps.get(w.pid)))
        .collect()
}

/// `None` when the window is gone or its application has no bundle id (an
/// empty one would make `macos_target_id` unparseable).
pub fn describe(id: isize) -> Option<Candidate> {
    let w = window(id)?;
    info::window_from_info(&w, app_of(w.pid).as_ref())
}

/// The frontmost window of the frontmost application. macOS activates
/// applications, not windows, so this is the honest analogue of
/// `GetForegroundWindow`.
pub fn foreground() -> Option<isize> {
    let pid = frontmost_pid()?;
    info::front_window_of(pid, &on_screen()).map(|id| id as isize)
}

/// Still in the window list — on screen or minimized. A window whose
/// application quit leaves the list entirely.
pub fn is_alive(id: isize) -> bool {
    window(id).is_some()
}

pub fn own_pid() -> u32 {
    std::process::id()
}

/// Every on-screen window, front to back, as CoreGraphics reports the order.
pub(super) fn on_screen() -> Vec<WindowInfo> {
    list(ON_SCREEN, kCGNullWindowID)
}

/// One window by id, whether or not it is on screen. The single-window query is
/// the cheap path and a full scan is the fallback: a query that declines to
/// answer must never read as "this window died", which is `Lost`.
pub(super) fn window(id: isize) -> Option<WindowInfo> {
    let Ok(id) = u32::try_from(id) else {
        return None;
    };
    let found = |option, relative| list(option, relative).into_iter().find(|w| w.id == id);
    found(CGWindowListOption::OptionIncludingWindow, id)
        .or_else(|| found(CGWindowListOption::OptionAll, kCGNullWindowID))
}

pub(super) fn pid_of(id: isize) -> Option<i32> {
    window(id).map(|w| w.pid)
}

/// The bundle id and executable of a running application; `None` for a process
/// LaunchServices does not track or one that has exited.
pub(super) fn app_of(pid: i32) -> Option<AppInfo> {
    let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?;
    let bundle_id = app.bundleIdentifier()?.to_string();
    if bundle_id.is_empty() {
        return None;
    }
    let exe = app
        .executableURL()
        .and_then(|url| url.lastPathComponent())
        .or_else(|| app.localizedName())
        .map(|s| s.to_string().to_lowercase())
        .filter(|s| !s.is_empty());
    Some(AppInfo { bundle_id, exe })
}

/// The pid of the application that owns the menu bar. Polled on the tracker
/// tick rather than observed: `NSWorkspace`'s activation notification is
/// documented to reach an observer's run loop, not a chosen thread's, and a
/// wrong threading assumption here would hang the tracker (plan §6).
pub(super) fn frontmost_pid() -> Option<i32> {
    NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .map(|app| app.processIdentifier())
}

/// One `NSRunningApplication` lookup per process per enumeration; a terminal
/// with twenty windows would otherwise pay for twenty.
#[derive(Default)]
struct AppCache(Vec<(i32, Option<AppInfo>)>);

impl AppCache {
    fn get(&mut self, pid: i32) -> Option<&AppInfo> {
        if let Some(i) = self.0.iter().position(|(p, _)| *p == pid) {
            return self.0[i].1.as_ref();
        }
        self.0.push((pid, app_of(pid)));
        self.0.last().and_then(|(_, a)| a.as_ref())
    }
}

fn list(option: CGWindowListOption, relative_to: u32) -> Vec<WindowInfo> {
    let Some(array) = CGWindowListCopyWindowInfo(option, relative_to) else {
        return Vec::new();
    };
    // SAFETY: the window list is an array of CFDictionary descriptions.
    let array: &CFArray<CFType> = unsafe { array.cast_unchecked() };
    let mut out = Vec::with_capacity(array.len());
    for item in array.iter() {
        if let Some(dict) = item.downcast_ref::<CFDictionary>() {
            if let Some(w) = window_info(dict) {
                out.push(w);
            }
        }
    }
    out
}

/// Field by field, never a `serde`-style all-or-nothing decode: an entry
/// missing a key we do not need must still describe a window.
fn window_info(dict: &CFDictionary) -> Option<WindowInfo> {
    // SAFETY: every key in a window description is a CFString.
    let dict: &CFDictionary<CFString, CFType> = unsafe { dict.cast_unchecked() };
    // SAFETY: the CoreGraphics key statics are live for the life of the process.
    let (number, pid_key, layer_key, bounds_key, name_key, onscreen_key) = unsafe {
        (
            kCGWindowNumber,
            kCGWindowOwnerPID,
            kCGWindowLayer,
            kCGWindowBounds,
            kCGWindowName,
            kCGWindowIsOnscreen,
        )
    };
    Some(WindowInfo {
        id: u32::try_from(number_of(dict, number)?.max(0.0).round() as i64).ok()?,
        pid: number_of(dict, pid_key)? as i32,
        // A missing layer reads as the ordinary one; a shell surface always
        // carries its own.
        layer: number_of(dict, layer_key).unwrap_or(0.0) as i32,
        title: string_of(dict, name_key).unwrap_or_default(),
        bounds: bounds_of(dict, bounds_key)?,
        // The key is present only while the window is on screen.
        on_screen: bool_of(dict, onscreen_key).unwrap_or(false),
    })
}

fn number_of(dict: &CFDictionary<CFString, CFType>, key: &CFString) -> Option<f64> {
    let value = dict.get(key)?;
    value.downcast_ref::<CFNumber>()?.as_f64()
}

fn string_of(dict: &CFDictionary<CFString, CFType>, key: &CFString) -> Option<String> {
    let value = dict.get(key)?;
    value.downcast_ref::<CFString>().map(CFString::to_string)
}

fn bool_of(dict: &CFDictionary<CFString, CFType>, key: &CFString) -> Option<bool> {
    let value = dict.get(key)?;
    value.downcast_ref::<CFBoolean>().map(CFBoolean::as_bool)
}

fn bounds_of(dict: &CFDictionary<CFString, CFType>, key: &CFString) -> Option<RectF> {
    let value = dict.get(key)?;
    let rect: CFRetained<CFDictionary> = value.downcast().ok()?;
    let mut out = CGRect::default();
    // SAFETY: the value is CoreGraphics' own rect dictionary and `out` is a
    // live local; a shape it does not recognise returns false.
    let ok = unsafe { CGRectMakeWithDictionaryRepresentation(Some(&rect), &mut out) };
    ok.then_some(RectF {
        x: out.origin.x,
        y: out.origin.y,
        w: out.size.width,
        h: out.size.height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stale_window_id_is_dead_and_describes_as_nothing() {
        let bogus = 0x7fff_0001isize;
        assert!(!is_alive(bogus));
        assert_eq!(describe(bogus), None);
        assert_eq!(pid_of(bogus), None);
        // Negative ids cannot be CGWindowIDs and must not wrap on the cast.
        assert!(!is_alive(-1));
    }

    #[test]
    fn foreground_and_candidates_do_not_panic() {
        let _ = foreground();
        for c in candidates() {
            assert!(!c.class.is_empty(), "window {} has no bundle id", c.hwnd);
        }
    }

    #[test]
    fn our_own_process_has_a_pid_but_no_bundle_of_its_own() {
        // A `cargo test` binary is not an application bundle, so this is the
        // "no bundle id" branch on a process that definitely exists.
        let _ = app_of(own_pid() as i32);
    }

    /// Needs a windowed session: run it from a logged-in desktop, with at
    /// least one ordinary application window open. Every candidate must carry
    /// a bundle id, and the frontmost application must own a candidate.
    #[test]
    #[ignore = "needs a windowed session"]
    fn the_frontmost_application_owns_an_on_screen_window() {
        let pid = frontmost_pid().expect("a frontmost application");
        let id = info::front_window_of(pid, &on_screen()).expect("a frontmost window");
        let cand = describe(id as isize).expect("the frontmost window describes");
        assert!(!cand.class.is_empty());
        assert!(is_alive(id as isize));
    }
}
