//! Tray icon and menu (M6.2). The tray is the way back from a hidden or
//! click-through overlay, so click-through is never persisted without it.

use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, Wry};

use crate::{restyle, set_interactive, settings, stop_daemon_blocking, DaemonChild};

const ID_TOGGLE: &str = "toggle";
const ID_DOCK: &str = "dock";
const ID_UNDOCK: &str = "undock";
const ID_CLICK_THROUGH: &str = "click_through";
const ID_SETTINGS: &str = "settings";
const ID_QUIT: &str = "quit";

/// Handles to the items that get relabelled or toggled later. Managed in the
/// app state; the setters hop to the main thread themselves.
pub struct TrayItems {
    toggle: MenuItem<Wry>,
    dock: MenuItem<Wry>,
    undock: MenuItem<Wry>,
    click_through: CheckMenuItem<Wry>,
}

pub fn build(app: &AppHandle) -> tauri::Result<()> {
    let click_through = app
        .state::<std::sync::Mutex<settings::Settings>>()
        .lock()
        .map(|s| s.click_through)
        .unwrap_or(false);
    let visible = main_window_visible(app);

    let toggle = MenuItem::with_id(app, ID_TOGGLE, toggle_label(visible), true, None::<&str>)?;
    // The tracker exists on Windows and macOS only; `set_dock_items` keeps the
    // pair in step with the dock state from then on.
    let tracked = cfg!(any(windows, target_os = "macos"));
    let dock = MenuItem::with_id(app, ID_DOCK, "Dock to window…", tracked, None::<&str>)?;
    let undock = MenuItem::with_id(app, ID_UNDOCK, "Undock", false, None::<&str>)?;
    let check = CheckMenuItem::with_id(
        app,
        ID_CLICK_THROUGH,
        "Click-through",
        true,
        click_through,
        None::<&str>,
    )?;
    let settings_item = MenuItem::with_id(app, ID_SETTINGS, "Settings…", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, ID_QUIT, "Quit", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(
        app,
        &[&toggle, &dock, &undock, &check, &settings_item, &sep, &quit],
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
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle_visible(tray.app_handle());
            }
        })
        .build(app)?;

    // Managed only once the icon really exists: `exists` is what close-to-hide
    // and the label setters key off, and a hidden window with no tray is a
    // dead end.
    app.manage(TrayItems {
        toggle,
        dock,
        undock,
        click_through: check,
    });
    Ok(())
}

/// True once `build` has put a tray icon in the shell's notification area.
pub fn exists(app: &AppHandle) -> bool {
    app.try_state::<TrayItems>().is_some()
}

fn on_menu(app: &AppHandle, id: &str) {
    match id {
        ID_TOGGLE => toggle_visible(app),
        ID_CLICK_THROUGH => {
            // `on_changed` applies the style and re-syncs the check item.
            if let Err(e) = settings::update(app, |s| s.click_through = !s.click_through) {
                eprintln!("click-through toggle failed: {e}");
            }
        }
        ID_SETTINGS => {
            // Interactive first: a click-through or non-focusable window cannot
            // take the panel's clicks or keys.
            set_interactive(app, true);
            show(app);
            // Focus again from the main-thread queue: a foreground request
            // against a still-hidden window is a no-op, so the panel would
            // come up unfocused and need a click before it took keys.
            focus(app);
            let _ = app.emit("open-settings", ());
        }
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

fn show(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
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

fn hide(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.hide();
    }
    restyle(app);
    set_toggle_label(app, false);
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

fn toggle_label(visible: bool) -> &'static str {
    if visible {
        "Hide overlay"
    } else {
        "Show overlay"
    }
}

/// Keep the toggle honest after a hide/show from anywhere (close button,
/// docking events), not only from the tray itself.
pub fn set_toggle_label(app: &AppHandle, visible: bool) {
    if let Some(items) = app.try_state::<TrayItems>() {
        let _ = items.toggle.set_text(toggle_label(visible));
    }
}

/// `available` = the tracker exists on this platform and is wired (M4).
pub fn set_dock_items(app: &AppHandle, docked: bool, available: bool) {
    if let Some(items) = app.try_state::<TrayItems>() {
        let _ = items.dock.set_enabled(available && !docked);
        let _ = items.undock.set_enabled(available && docked);
    }
}

pub fn sync_click_through(app: &AppHandle, on: bool) {
    if let Some(items) = app.try_state::<TrayItems>() {
        let _ = items.click_through.set_checked(on);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_label_follows_visibility() {
        assert_eq!(toggle_label(true), "Hide overlay");
        assert_eq!(toggle_label(false), "Show overlay");
    }
}
