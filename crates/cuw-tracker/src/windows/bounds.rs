//! Target geometry. DWM extended frame bounds are exact; `GetWindowRect` is
//! 7 px wide on three sides, so it is a flagged fallback only (plan §6).

use std::ffi::c_void;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, GetWindowRect, IsIconic, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use super::find::{hwnd, is_alive};
use crate::{Bounds, Rect};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Read {
    Bounds(Bounds),
    /// Minimized: geometry reports -32000, so the caller emits `Minimized` instead.
    Iconic,
    Gone,
}

pub fn read(h: isize) -> Read {
    if !is_alive(h) {
        return Read::Gone;
    }
    // SAFETY: defined for any handle value.
    if unsafe { IsIconic(hwnd(h)) }.as_bool() {
        return Read::Iconic;
    }
    let scale = scale_of(h);
    if let Some(r) = dwm_frame(h) {
        return Read::Bounds(to_bounds(r, scale, false));
    }
    let mut r = RECT::default();
    // SAFETY: the out pointer is a live local RECT.
    match unsafe { GetWindowRect(hwnd(h), &mut r) } {
        Ok(()) => Read::Bounds(to_bounds(r, scale, true)),
        Err(_) => Read::Gone,
    }
}

/// Work area of the monitor nearest the window (excludes the taskbar).
pub fn work_area_for(h: isize) -> Option<Rect> {
    // SAFETY: DEFAULTTONEAREST always yields a monitor for a valid window.
    let mon = unsafe { MonitorFromWindow(hwnd(h), MONITOR_DEFAULTTONEAREST) };
    if mon.is_invalid() {
        return None;
    }
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is a live local with cbSize set as the API requires.
    if !unsafe { GetMonitorInfoW(mon, &mut info) }.as_bool() {
        return None;
    }
    Some(to_rect(info.rcWork))
}

/// Bounding box of all monitors; the origin is negative when a monitor sits
/// left of or above the primary.
pub fn virtual_screen() -> Rect {
    // SAFETY: metric queries take no pointers and cannot fail.
    let (x, y, w, h) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    Rect { x, y, w, h }
}

fn dwm_frame(h: isize) -> Option<RECT> {
    let mut r = RECT::default();
    // SAFETY: the out pointer and size describe the local RECT.
    unsafe {
        DwmGetWindowAttribute(
            hwnd(h),
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&mut r as *mut RECT).cast::<c_void>(),
            std::mem::size_of::<RECT>() as u32,
        )
    }
    .ok()
    .map(|()| r)
}

fn scale_of(h: isize) -> f64 {
    // SAFETY: returns 0 for an invalid window, mapped to scale 1.0 below.
    let dpi = unsafe { GetDpiForWindow(hwnd(h)) };
    if dpi == 0 {
        1.0
    } else {
        f64::from(dpi) / 96.0
    }
}

fn to_rect(r: RECT) -> Rect {
    Rect {
        x: r.left,
        y: r.top,
        w: r.right - r.left,
        h: r.bottom - r.top,
    }
}

fn to_bounds(r: RECT, scale: f64, approximate: bool) -> Bounds {
    let Rect { x, y, w, h } = to_rect(r);
    Bounds {
        x,
        y,
        w,
        h,
        scale,
        approximate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_screen_has_area() {
        let vs = virtual_screen();
        assert!(vs.w > 0 && vs.h > 0, "{vs:?}");
    }

    #[test]
    fn bogus_hwnd_reads_gone() {
        let bogus = 0x7fff_0001isize;
        assert_eq!(read(bogus), Read::Gone);
        assert_eq!(work_area_for(bogus), None);
    }

    #[test]
    fn rect_conversion_keeps_width_and_height() {
        let r = RECT {
            left: -10,
            top: 5,
            right: 90,
            bottom: 65,
        };
        assert_eq!(
            to_rect(r),
            Rect {
                x: -10,
                y: 5,
                w: 100,
                h: 60
            }
        );
        let b = to_bounds(r, 1.5, true);
        assert_eq!((b.x, b.y, b.w, b.h), (-10, 5, 100, 60));
        assert_eq!(b.scale, 1.5);
        assert!(b.approximate);
    }
}
