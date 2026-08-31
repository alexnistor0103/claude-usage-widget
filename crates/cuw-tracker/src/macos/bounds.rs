//! Target geometry and screen layout, all through CoreGraphics.
//!
//! Deliberately no `NSScreen`: it is main-thread-only, and the overlay reads
//! [`work_area_for`] and [`virtual_screen`] from its tracker-consumer thread.
//! `CGDisplayBounds` reports the same top-left-origin point space the window
//! list uses, so the conversion to physical pixels is one multiply.

use objc2_core_foundation::CGRect;
use objc2_core_graphics::{
    CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayMode, CGGetActiveDisplayList,
};

use super::find;
use super::info::{self, Display, RectF};
use crate::{Bounds, Rect};

/// More than this many displays and we simply take the first ones; nobody
/// docks a widget to the sixteenth monitor.
const MAX_DISPLAYS: u32 = 16;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Read {
    Bounds(Bounds),
    /// Minimized or hidden: the window is still in the list but off screen, so
    /// the caller emits `Minimized` rather than a stale rectangle.
    Iconic,
    Gone,
}

pub fn read(id: isize) -> Read {
    let Some(w) = find::window(id) else {
        return Read::Gone;
    };
    if !w.on_screen {
        return Read::Iconic;
    }
    Read::Bounds(info::bounds_from_info(&w, &displays()))
}

/// The display the window sits on, in physical pixels.
///
/// Not the `visibleFrame`: the menu bar and Dock insets are an `NSScreen`
/// property and `NSScreen` is main-thread-only, so this is the whole display.
/// The overlay only reaches for it when a placement landed off every monitor,
/// where a slightly generous rectangle is the right kind of wrong.
pub fn work_area_for(id: isize) -> Option<Rect> {
    let w = find::window(id)?;
    let all = displays();
    let d = info::display_for(&all, w.bounds)?;
    Some(info::to_rect(d.bounds, d.scale))
}

/// Bounding box of all active displays; the origin is negative when a display
/// sits left of or above the primary.
pub fn virtual_screen() -> Rect {
    info::virtual_screen_of(&displays())
}

/// Active displays, primary first — the order `CGGetActiveDisplayList` promises.
pub(super) fn displays() -> Vec<Display> {
    let mut ids = [0u32; MAX_DISPLAYS as usize];
    let mut count = 0u32;
    // SAFETY: both out pointers are live locals and the buffer holds MAX_DISPLAYS.
    let err = unsafe { CGGetActiveDisplayList(MAX_DISPLAYS, ids.as_mut_ptr(), &mut count) };
    if err.0 != 0 {
        return Vec::new();
    }
    ids.iter()
        .take((count as usize).min(ids.len()))
        .map(|&id| Display {
            bounds: rect_of(CGDisplayBounds(id)),
            scale: scale_of(id),
        })
        .collect()
}

/// Backing pixels per point for a display. Framebuffer pixels over mode points
/// is the same number `NSScreen::backingScaleFactor` reports — 2.0 on every
/// Retina mode, scaled ones included — and unlike it, needs no main thread.
fn scale_of(id: u32) -> f64 {
    let Some(mode) = CGDisplayCopyDisplayMode(id) else {
        return 1.0;
    };
    let points = CGDisplayMode::width(Some(&mode));
    let pixels = CGDisplayMode::pixel_width(Some(&mode));
    if points == 0 || pixels == 0 {
        return 1.0;
    }
    pixels as f64 / points as f64
}

fn rect_of(r: CGRect) -> RectF {
    RectF {
        x: r.origin.x,
        y: r.origin.y,
        w: r.size.width,
        h: r.size.height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stale_window_id_reads_gone_and_has_no_work_area() {
        let bogus = 0x7fff_0001isize;
        assert_eq!(read(bogus), Read::Gone);
        assert_eq!(work_area_for(bogus), None);
    }

    /// Needs a windowed session: a headless build agent has no active display.
    #[test]
    #[ignore = "needs a windowed session"]
    fn the_primary_display_has_area_and_a_sane_scale() {
        let all = displays();
        assert!(!all.is_empty(), "no active display");
        for d in &all {
            assert!(d.bounds.w > 0.0 && d.bounds.h > 0.0, "{d:?}");
            assert!((1.0..=4.0).contains(&d.scale), "{d:?}");
        }
        let vs = virtual_screen();
        assert!(vs.w > 0 && vs.h > 0, "{vs:?}");
    }
}
