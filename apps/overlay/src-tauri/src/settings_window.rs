//! The settings window (M7): a normal decorated window of its own, so the
//! controls are not squeezed into the widget or the popover. Created on first
//! use and destroyed on close; the next open starts from the stored settings.

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

pub const LABEL: &str = "settings";

/// Any thread: the builder and the window calls run on the main thread, where
/// macOS requires window creation anyway.
pub fn open(app: &AppHandle) {
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(w) = app2.get_webview_window(LABEL) {
            let _ = w.unminimize();
            let _ = w.show();
            let _ = w.set_focus();
            return;
        }
        let built =
            WebviewWindowBuilder::new(&app2, LABEL, WebviewUrl::App("settings.html".into()))
                .title("Settings")
                .inner_size(680.0, 520.0)
                .min_inner_size(560.0, 420.0)
                .center()
                .theme(Some(tauri::Theme::Dark))
                .build();
        if let Err(e) = built {
            eprintln!("settings window: {e}");
        }
    });
}

pub fn close(app: &AppHandle) {
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(w) = app2.get_webview_window(LABEL) {
            let _ = w.close();
        }
    });
}

#[tauri::command]
pub async fn open_settings(app: AppHandle) -> Result<(), String> {
    open(&app);
    Ok(())
}

#[tauri::command]
pub async fn close_settings(app: AppHandle) -> Result<(), String> {
    close(&app);
    Ok(())
}
