//! Tray icon and menu (M6.2). The tray is the way back from a hidden or
//! click-through overlay, so click-through is never persisted without it. In
//! menu bar mode it is also the front door: a left click opens the usage view
//! as a popover under (or above) the icon.

use std::sync::Mutex;

use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, PhysicalPosition, PhysicalSize, Rect, Wry};

use crate::settings::{self, Mode};
use crate::{popover, restyle, stop_daemon_blocking, DaemonChild};

const ID_TOGGLE: &str = "toggle";
const ID_MENU_BAR: &str = "menu_bar";
const ID_DOCK: &str = "dock";
const ID_UNDOCK: &str = "undock";
const ID_CLICK_THROUGH: &str = "click_through";
const ID_SETTINGS: &str = "settings";
const ID_QUIT: &str = "quit";

/// Gap between the icon and the popover, in physical pixels.
const POPOVER_GAP: i32 = 6;

/// Handles to the items that get relabelled or toggled later. Managed in the
/// app state; the setters hop to the main thread themselves.
pub struct TrayItems {
    toggle: MenuItem<Wry>,
    menu_bar: CheckMenuItem<Wry>,
    dock: MenuItem<Wry>,
    undock: MenuItem<Wry>,
    click_through: CheckMenuItem<Wry>,
    /// Where the icon was at the last click, so a popover opened from the menu
    /// (Settings…) or from the toggle item lands in the same place.
    anchor: Mutex<Option<Rect>>,
}

pub fn build(app: &AppHandle) -> tauri::Result<()> {
    let s = app
        .state::<Mutex<settings::Settings>>()
        .lock()
        .map(|s| s.clone())
        .unwrap_or_default();
    let visible = main_window_visible(app);

    let toggle = MenuItem::with_id(
        app,
        ID_TOGGLE,
        toggle_label(s.mode, visible),
        true,
        None::<&str>,
    )?;
    let menu_bar = CheckMenuItem::with_id(
        app,
        ID_MENU_BAR,
        "Menu bar only",
        true,
        s.mode == Mode::MenuBar,
        None::<&str>,
    )?;
    // The tracker exists on Windows and macOS only; `set_dock_items` keeps the
    // pair in step with the dock state from then on.
    let tracked = cfg!(any(windows, target_os = "macos")) && s.mode == Mode::Widget;
    let dock = MenuItem::with_id(app, ID_DOCK, "Dock to window…", tracked, None::<&str>)?;
    let undock = MenuItem::with_id(app, ID_UNDOCK, "Undock", false, None::<&str>)?;
    let check = CheckMenuItem::with_id(
        app,
        ID_CLICK_THROUGH,
        "Click-through",
        s.mode == Mode::Widget,
        s.click_through,
        None::<&str>,
    )?;
    let settings_item = MenuItem::with_id(app, ID_SETTINGS, "Settings…", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, ID_QUIT, "Quit", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(
        app,
        &[
            &toggle,
            &menu_bar,
            &sep,
            &dock,
            &undock,
            &check,
            &settings_item,
            &sep2,
            &quit,
        ],
    )?;

    // `default_window_icon()` is None on a dev run, so load from bytes. macOS
    // wants a template — black on alpha, which the menu bar tints itself — and
    // a colour icon there looks wrong in both appearances.
    #[cfg(target_os = "macos")]
    let icon = Image::from_bytes(include_bytes!("../icons/tray-mac@2x.png"))?;
    #[cfg(not(target_os = "macos"))]
    let icon = Image::from_bytes(include_bytes!("../icons/32x32.png"))?;
    TrayIconBuilder::with_id("main")
        .icon(icon)
        .icon_as_template(cfg!(target_os = "macos"))
        .tooltip("Claude Usage Widget")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| on_menu(app, event.id.0.as_str()))
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button,
                button_state,
                rect,
                ..
            } = event
            {
                let app = tray.app_handle();
                remember_anchor(app, rect);
                if button == MouseButton::Left && button_state == MouseButtonState::Up {
                    on_left_click(app);
                }
            }
        })
        .build(app)?;

    // Managed only once the icon really exists: `exists` is what close-to-hide
    // and the label setters key off, and a hidden window with no tray is a
    // dead end.
    app.manage(TrayItems {
        toggle,
        menu_bar,
        dock,
        undock,
        click_through: check,
        anchor: Mutex::new(None),
    });
    Ok(())
}

/// True once `build` has put a tray icon in the shell's notification area.
pub fn exists(app: &AppHandle) -> bool {
    app.try_state::<TrayItems>().is_some()
}

fn remember_anchor(app: &AppHandle, rect: Rect) {
    if let Some(items) = app.try_state::<TrayItems>() {
        if let Ok(mut a) = items.anchor.lock() {
            *a = Some(rect);
        }
    }
}

fn anchor(app: &AppHandle) -> Option<Rect> {
    app.try_state::<TrayItems>()
        .and_then(|i| i.anchor.lock().ok().and_then(|a| *a))
}

fn on_left_click(app: &AppHandle) {
    if mode(app) == Mode::MenuBar {
        // Clicking the icon while the popover is up first takes its focus,
        // which already hid it; that click means "close", not "open again".
        if popover::just_auto_hidden(app) {
            return;
        }
        if main_window_visible(app) {
            hide(app);
        } else {
            show(app);
            focus(app);
        }
    } else {
        toggle_visible(app);
    }
}

fn on_menu(app: &AppHandle, id: &str) {
    match id {
        ID_TOGGLE => {
            if mode(app) == Mode::MenuBar {
                on_left_click(app);
            } else {
                toggle_visible(app);
            }
        }
        ID_MENU_BAR => {
            // `on_changed` applies the mode and re-syncs the check items.
            if let Err(e) = settings::update(app, |s| {
                s.mode = match s.mode {
                    Mode::MenuBar => Mode::Widget,
                    Mode::Widget => Mode::MenuBar,
                }
            }) {
                eprintln!("mode toggle failed: {e}");
            }
        }
        ID_CLICK_THROUGH => {
            // `on_changed` applies the style and re-syncs the check item.
            if let Err(e) = settings::update(app, |s| s.click_through = !s.click_through) {
                eprintln!("click-through toggle failed: {e}");
            }
        }
        ID_SETTINGS => crate::settings_window::open(app),
        // Tray handlers run on the main thread: never lock the dock state
        // here, spawn the async command instead (M4.4).
        ID_DOCK => {
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = crate::dock::dock_pick(app).await {
                    eprintln!("dock pick: {e}");
                }
            });
        }
        ID_UNDOCK => {
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = crate::dock::dock_stop(app).await {
                    eprintln!("undock: {e}");
                }
            });
        }
        ID_QUIT => {
            // Off the main thread so the stop's port waits do not freeze the UI.
            let app = app.clone();
            std::thread::spawn(move || {
                let _ = stop_daemon_blocking(&app.state::<DaemonChild>());
                app.exit(0);
            });
        }
        _ => {}
    }
}

fn mode(app: &AppHandle) -> Mode {
    app.try_state::<Mutex<settings::Settings>>()
        .and_then(|s| s.lock().ok().map(|s| s.mode))
        .unwrap_or_default()
}

/// Show the window; a popover is first moved next to the icon it came from.
/// Tray callbacks run on the main thread, so the window getters are safe here.
pub fn show(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        if mode(app) == Mode::MenuBar {
            if let Some(rect) = anchor(app) {
                if let Some(pos) = popover_position(app, &w, rect) {
                    let _ = w.set_position(pos);
                }
            }
        }
        let _ = w.show();
    }
    restyle(app);
    set_toggle_label(app, true);
}

fn focus(app: &AppHandle) {
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(w) = app2.get_webview_window("main") {
            let _ = w.set_focus();
        }
    });
}

pub fn hide(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.hide();
    }
    restyle(app);
    set_toggle_label(app, false);
}

/// Where to put the popover for an icon at `rect`: centred on the icon, below
/// it when the bar is at the top of the screen (macOS), above it otherwise
/// (a Windows taskbar at the bottom), and always inside the work area.
fn popover_position(
    app: &AppHandle,
    w: &tauri::WebviewWindow,
    rect: Rect,
) -> Option<PhysicalPosition<i32>> {
    // The event rect may be logical on macOS; the icon's own monitor scales it.
    let probe = match rect.position {
        tauri::Position::Physical(p) => (f64::from(p.x), f64::from(p.y)),
        tauri::Position::Logical(p) => (p.x, p.y),
    };
    let monitor = app
        .monitor_from_point(probe.0, probe.1)
        .ok()
        .flatten()
        .or_else(|| app.primary_monitor().ok().flatten())?;
    let scale = monitor.scale_factor();
    let icon_pos: PhysicalPosition<i32> = rect.position.to_physical(scale);
    let icon_size: PhysicalSize<u32> = rect.size.to_physical(scale);
    let win = w.outer_size().ok()?;
    let area = monitor.work_area();

    let win_w = i32::try_from(win.width).unwrap_or(i32::MAX);
    let win_h = i32::try_from(win.height).unwrap_or(i32::MAX);
    let icon_w = i32::try_from(icon_size.width).unwrap_or(0);
    let icon_h = i32::try_from(icon_size.height).unwrap_or(0);
    let area_w = i32::try_from(area.size.width).unwrap_or(i32::MAX);
    let area_h = i32::try_from(area.size.height).unwrap_or(i32::MAX);

    let x = icon_pos.x + icon_w / 2 - win_w / 2;
    let icon_centre_y = icon_pos.y + icon_h / 2;
    let bar_on_top = icon_centre_y < area.position.y + area_h / 2;
    let y = if bar_on_top {
        icon_pos.y + icon_h + POPOVER_GAP
    } else {
        icon_pos.y - win_h - POPOVER_GAP
    };
    let max_x = (area.position.x + area_w - win_w).max(area.position.x);
    let max_y = (area.position.y + area_h - win_h).max(area.position.y);
    Some(PhysicalPosition::new(
        x.clamp(area.position.x, max_x),
        y.clamp(area.position.y, max_y),
    ))
}

/// Tray callbacks run on the main thread, so the `is_visible` getter is safe.
fn toggle_visible(app: &AppHandle) {
    if main_window_visible(app) {
        hide(app);
    } else {
        show(app);
    }
}

/// A failed getter reads as hidden: the next toggle then shows (idempotent on a
/// visible window) instead of hiding one that is already gone.
fn main_window_visible(app: &AppHandle) -> bool {
    app.get_webview_window("main")
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false)
}

fn toggle_label(mode: Mode, visible: bool) -> &'static str {
    match (mode, visible) {
        (Mode::MenuBar, true) => "Hide usage",
        (Mode::MenuBar, false) => "Show usage",
        (Mode::Widget, true) => "Hide overlay",
        (Mode::Widget, false) => "Show overlay",
    }
}

/// Keep the toggle honest after a hide/show from anywhere (close button,
/// docking events, a popover losing focus), not only from the tray itself.
pub fn set_toggle_label(app: &AppHandle, visible: bool) {
    if let Some(items) = app.try_state::<TrayItems>() {
        let _ = items.toggle.set_text(toggle_label(mode(app), visible));
    }
}

/// `available` = the tracker exists on this platform and is wired (M4). Only
/// a widget docks; the popover has nothing to attach to.
pub fn set_dock_items(app: &AppHandle, docked: bool, available: bool) {
    if let Some(items) = app.try_state::<TrayItems>() {
        let available = available && mode(app) == Mode::Widget;
        let _ = items.dock.set_enabled(available && !docked);
        let _ = items.undock.set_enabled(available && docked);
    }
}

/// Re-sync every settings-driven item after a change from any side.
pub fn sync_settings(app: &AppHandle, s: &settings::Settings) {
    if let Some(items) = app.try_state::<TrayItems>() {
        let widget = s.mode == Mode::Widget;
        let _ = items.menu_bar.set_checked(!widget);
        let _ = items.click_through.set_checked(s.click_through);
        let _ = items.click_through.set_enabled(widget);
        let _ = items
            .toggle
            .set_text(toggle_label(s.mode, main_window_visible(app)));
    }
    set_dock_items(
        app,
        crate::dock::is_docked(app),
        cfg!(any(windows, target_os = "macos")),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_label_follows_mode_and_visibility() {
        assert_eq!(toggle_label(Mode::Widget, true), "Hide overlay");
        assert_eq!(toggle_label(Mode::Widget, false), "Show overlay");
        assert_eq!(toggle_label(Mode::MenuBar, true), "Hide usage");
        assert_eq!(toggle_label(Mode::MenuBar, false), "Show usage");
    }
}
