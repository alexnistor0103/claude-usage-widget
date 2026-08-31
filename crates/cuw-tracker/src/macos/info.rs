//! Everything the macOS tracker decides, over plain data. The CoreGraphics
//! layer only fetches and hands over, so the parts that can be wrong are unit
//! tested on every platform — including the ones without a Mac (M5).

use crate::geometry::Candidate;
use crate::{Bounds, Rect};

/// A rectangle in CoreGraphics' global space: points, origin at the top-left of
/// the primary display. `kCGWindowBounds` and `CGDisplayBounds` both live here.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RectF {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// One `CGWindowListCopyWindowInfo` entry, already out of CoreFoundation.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowInfo {
    /// `kCGWindowNumber`.
    pub id: u32,
    /// `kCGWindowOwnerPID`.
    pub pid: i32,
    /// `kCGWindowLayer`: 0 is an ordinary application window. The Dock, the
    /// menu bar and every other shell surface sit on their own layers.
    pub layer: i32,
    /// `kCGWindowName`, empty without Screen Recording — which is never
    /// required, so nothing here may depend on it.
    pub title: String,
    /// `kCGWindowBounds`.
    pub bounds: RectF,
    /// `kCGWindowIsOnscreen`; false for a minimized or hidden window.
    pub on_screen: bool,
}

/// The owning application, which the window list itself does not carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppInfo {
    /// The `TargetId` is built from this, so a window without one names nothing.
    pub bundle_id: String,
    /// Executable basename, else the localized application name; lowercase.
    pub exe: Option<String>,
}

/// A display as CoreGraphics reports it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Display {
    pub bounds: RectF,
    /// Backing pixels per point.
    pub scale: f64,
}

/// The docked window as one poll tick found it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Visible,
    Minimized,
    Gone,
}

/// What a `prev -> now` move owes the consumer. macOS delivers no minimize or
/// destroy event to a poll, so the transition is derived rather than observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    None,
    Minimized,
    Restored,
    Lost,
}

/// `None` for anything that is not an ordinary application window.
pub fn window_from_info(w: &WindowInfo, app: Option<&AppInfo>) -> Option<Candidate> {
    let app = app?;
    if w.layer != 0 || app.bundle_id.is_empty() || w.bounds.w <= 0.0 || w.bounds.h <= 0.0 {
        return None;
    }
    Some(Candidate {
        hwnd: w.id as isize,
        class: app.bundle_id.clone(),
        exe: app.exe.clone(),
        title: w.title.clone(),
    })
}

/// The frontmost window of `pid`, given a front-to-back on-screen list. Used
/// both for the foreground window and to resolve an interactive pick, which is
/// why the shell layers are skipped here rather than by the caller.
pub fn front_window_of(pid: i32, on_screen: &[WindowInfo]) -> Option<u32> {
    on_screen
        .iter()
        .find(|w| w.pid == pid && w.layer == 0 && w.bounds.w > 0.0 && w.bounds.h > 0.0)
        .map(|w| w.id)
}

/// The display a rect sits on: the one holding its centre, else the one it
/// overlaps most, else the first — which `CGGetActiveDisplayList` reports as
/// the primary. `None` only when there are no displays at all.
pub fn display_for(displays: &[Display], r: RectF) -> Option<Display> {
    let (cx, cy) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
    if let Some(d) = displays.iter().find(|d| contains(&d.bounds, cx, cy)) {
        return Some(*d);
    }
    let best = displays
        .iter()
        .map(|d| (overlap(&d.bounds, &r), d))
        .filter(|(area, _)| *area > 0.0)
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, d)| *d);
    best.or_else(|| displays.first().copied())
}

/// Physical pixels for the overlay: points times the scale of the display the
/// rect sits on, origin unchanged.
///
/// This is tao's own convention — `MonitorHandle::position` scales each
/// display's `CGDisplayBounds` origin by *that display's* factor (tao 0.35.3
/// `platform_impl/macos/monitor.rs`), and `util::window_position` treats the
/// top-left of the primary display as the origin. It is not a single linear
/// space across displays of different scales, but it is the space
/// `PhysicalPosition` means, and matching it is what makes `set_position` land.
pub fn to_bounds(r: RectF, scale: f64, approximate: bool) -> Bounds {
    let Rect { x, y, w, h } = to_rect(r, scale);
    Bounds {
        x,
        y,
        w,
        h,
        scale,
        approximate,
    }
}

/// [`to_bounds`] without the DPI, for work areas and the virtual screen.
pub fn to_rect(r: RectF, scale: f64) -> Rect {
    Rect {
        x: px(r.x, scale),
        y: px(r.y, scale),
        w: px(r.w, scale),
        h: px(r.h, scale),
    }
}

/// `approximate` when no display claimed the window: the scale is then a guess
/// of 1.0, and the consumer should know the placement may be off.
pub fn bounds_from_info(w: &WindowInfo, displays: &[Display]) -> Bounds {
    match display_for(displays, w.bounds) {
        Some(d) => to_bounds(w.bounds, d.scale, false),
        None => to_bounds(w.bounds, 1.0, true),
    }
}

/// Bounding box of every display, in the same physical space as [`to_bounds`].
pub fn virtual_screen_of(displays: &[Display]) -> Rect {
    let mut out: Option<Rect> = None;
    for d in displays {
        let r = to_rect(d.bounds, d.scale);
        out = Some(match out {
            None => r,
            Some(acc) => union(acc, r),
        });
    }
    out.unwrap_or(Rect {
        x: 0,
        y: 0,
        w: 0,
        h: 0,
    })
}

/// `Gone` wins from any phase; everything else is the obvious transition.
pub fn change(prev: Phase, now: Phase) -> Change {
    match (prev, now) {
        (Phase::Gone, _) | (Phase::Visible, Phase::Visible) => Change::None,
        (_, Phase::Gone) => Change::Lost,
        (Phase::Minimized, Phase::Minimized) => Change::None,
        (_, Phase::Minimized) => Change::Minimized,
        (_, Phase::Visible) => Change::Restored,
    }
}

fn contains(area: &RectF, x: f64, y: f64) -> bool {
    x >= area.x && x < area.x + area.w && y >= area.y && y < area.y + area.h
}

fn overlap(a: &RectF, b: &RectF) -> f64 {
    let w = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
    let h = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
    if w <= 0.0 || h <= 0.0 {
        0.0
    } else {
        w * h
    }
}

fn union(a: Rect, b: Rect) -> Rect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    Rect {
        x,
        y,
        w: (a.x + a.w).max(b.x + b.w) - x,
        h: (a.y + a.h).max(b.y + b.h) - y,
    }
}

/// A NaN or an absurd coordinate must land on a number, never on a panic or a
/// wrapped `as` cast (plan §9).
fn px(v: f64, scale: f64) -> i32 {
    let scaled = v * scale;
    if scaled.is_nan() {
        return 0;
    }
    scaled
        .round()
        .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> RectF {
        RectF { x, y, w, h }
    }

    fn display(x: f64, y: f64, w: f64, h: f64, scale: f64) -> Display {
        Display {
            bounds: rect(x, y, w, h),
            scale,
        }
    }

    fn window(id: u32, pid: i32, b: RectF) -> WindowInfo {
        WindowInfo {
            id,
            pid,
            layer: 0,
            title: String::new(),
            bounds: b,
            on_screen: true,
        }
    }

    fn app(bundle: &str) -> AppInfo {
        AppInfo {
            bundle_id: bundle.to_string(),
            exe: Some("terminal".to_string()),
        }
    }

    #[test]
    fn a_window_becomes_a_candidate_keyed_by_bundle_id() {
        let w = window(41, 700, rect(0.0, 0.0, 800.0, 600.0));
        let c = window_from_info(&w, Some(&app("com.apple.Terminal"))).expect("candidate");
        assert_eq!(c.hwnd, 41);
        assert_eq!(c.class, "com.apple.Terminal");
        assert_eq!(c.exe.as_deref(), Some("terminal"));
    }

    #[test]
    fn an_unreadable_title_is_empty_and_never_a_failure() {
        let w = window(41, 700, rect(0.0, 0.0, 800.0, 600.0));
        let c = window_from_info(&w, Some(&app("com.apple.Terminal"))).expect("candidate");
        assert_eq!(c.title, "", "a titleless window still docks");
    }

    #[test]
    fn shell_layers_zero_sizes_and_unknown_apps_are_not_candidates() {
        let base = window(41, 700, rect(0.0, 0.0, 800.0, 600.0));
        let terminal = app("com.apple.Terminal");

        let mut shell = base.clone();
        shell.layer = 25;
        assert_eq!(window_from_info(&shell, Some(&terminal)), None);

        let mut empty = base.clone();
        empty.bounds.w = 0.0;
        assert_eq!(window_from_info(&empty, Some(&terminal)), None);

        assert_eq!(window_from_info(&base, None), None);
        assert_eq!(window_from_info(&base, Some(&app(""))), None);
    }

    #[test]
    fn the_front_window_of_an_app_skips_shell_layers_and_other_apps() {
        let mut menu = window(1, 700, rect(0.0, 0.0, 1440.0, 24.0));
        menu.layer = 24;
        let front = window(2, 700, rect(0.0, 30.0, 800.0, 600.0));
        let behind = window(3, 700, rect(10.0, 40.0, 800.0, 600.0));
        let other = window(4, 800, rect(0.0, 0.0, 400.0, 400.0));
        let list = [menu, other, front, behind];

        assert_eq!(front_window_of(700, &list), Some(2));
        assert_eq!(front_window_of(800, &list), Some(4));
        assert_eq!(front_window_of(999, &list), None);
        assert_eq!(front_window_of(700, &[]), None);
    }

    const RETINA: Display = Display {
        bounds: RectF {
            x: 0.0,
            y: 0.0,
            w: 1440.0,
            h: 900.0,
        },
        scale: 2.0,
    };

    #[test]
    fn a_window_takes_the_scale_of_the_display_holding_its_centre() {
        let right = display(1440.0, 0.0, 1920.0, 1080.0, 1.0);
        let displays = [RETINA, right];

        let on_retina = window(1, 700, rect(100.0, 50.0, 800.0, 600.0));
        let b = bounds_from_info(&on_retina, &displays);
        assert_eq!((b.x, b.y, b.w, b.h), (200, 100, 1600, 1200));
        assert_eq!(b.scale, 2.0);
        assert!(!b.approximate);

        let on_right = window(2, 700, rect(1500.0, 40.0, 800.0, 600.0));
        let b = bounds_from_info(&on_right, &displays);
        assert_eq!((b.x, b.y, b.w, b.h), (1500, 40, 800, 600));
        assert_eq!(b.scale, 1.0);
    }

    /// A monitor left of or above the primary gives negative points, exactly as
    /// a Windows virtual screen does.
    #[test]
    fn a_display_left_of_the_primary_gives_negative_physical_coordinates() {
        let left = display(-1920.0, 0.0, 1920.0, 1080.0, 1.0);
        let displays = [RETINA, left];
        let w = window(1, 700, rect(-1900.0, 20.0, 600.0, 400.0));
        let b = bounds_from_info(&w, &displays);
        assert_eq!((b.x, b.y), (-1900, 20));
        assert_eq!(b.scale, 1.0);
    }

    #[test]
    fn a_window_off_every_display_falls_back_to_scale_one_and_is_approximate() {
        let w = window(1, 700, rect(50.0, 60.0, 800.0, 600.0));
        let b = bounds_from_info(&w, &[]);
        assert_eq!((b.x, b.y, b.w, b.h), (50, 60, 800, 600));
        assert_eq!(b.scale, 1.0);
        assert!(b.approximate, "a guessed scale must be flagged");
    }

    /// Straddling two displays: the centre decides, and when it lands on
    /// neither the larger overlap wins.
    #[test]
    fn display_selection_prefers_the_centre_then_the_larger_overlap() {
        let right = display(1440.0, 0.0, 1920.0, 1080.0, 1.0);
        let displays = [RETINA, right];

        let mostly_right = rect(1340.0, 100.0, 400.0, 300.0);
        assert_eq!(display_for(&displays, mostly_right), Some(right));

        // Centre y is below both displays, so neither contains it; the primary
        // still has the larger overlap.
        let below = rect(1200.0, 800.0, 260.0, 400.0);
        assert_eq!(display_for(&displays, below), Some(RETINA));

        let nowhere = rect(9000.0, 9000.0, 10.0, 10.0);
        assert_eq!(display_for(&displays, nowhere), Some(RETINA));
        assert_eq!(display_for(&[], nowhere), None);
    }

    #[test]
    fn the_virtual_screen_is_the_union_of_every_display() {
        let left = display(-1920.0, 0.0, 1920.0, 1080.0, 1.0);
        assert_eq!(
            virtual_screen_of(&[RETINA, left]),
            Rect {
                x: -1920,
                y: 0,
                w: 4800,
                h: 1800
            }
        );
        assert_eq!(
            virtual_screen_of(&[RETINA]),
            Rect {
                x: 0,
                y: 0,
                w: 2880,
                h: 1800
            }
        );
        assert_eq!(
            virtual_screen_of(&[]),
            Rect {
                x: 0,
                y: 0,
                w: 0,
                h: 0
            }
        );
    }

    #[test]
    fn phase_changes_map_to_the_events_the_overlay_expects() {
        use Phase::*;
        assert_eq!(change(Visible, Visible), Change::None);
        assert_eq!(change(Visible, Minimized), Change::Minimized);
        assert_eq!(change(Minimized, Visible), Change::Restored);
        assert_eq!(change(Minimized, Minimized), Change::None);
        assert_eq!(change(Visible, Gone), Change::Lost);
        assert_eq!(change(Minimized, Gone), Change::Lost);
        // A window that already reported Lost never reports it twice.
        assert_eq!(change(Gone, Gone), Change::None);
        assert_eq!(change(Gone, Visible), Change::None);
    }

    #[test]
    fn absurd_geometry_lands_on_a_number_rather_than_a_panic() {
        let huge = window(1, 700, rect(f64::MAX, f64::NAN, 1e300, -1e300));
        let b = bounds_from_info(&huge, &[]);
        assert_eq!(b.x, i32::MAX);
        assert_eq!(b.y, 0);
        assert_eq!(b.w, i32::MAX);
        assert_eq!(b.h, i32::MIN);
    }
}
