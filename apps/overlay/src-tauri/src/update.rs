//! Self-update: the updater plugin reads `latest.json` off the newest GitHub
//! release, downloads the signed installer, swaps the app and relaunches it.
//!
//! Every failure of the *check* — offline, no release published yet, a manifest
//! we don't recognise — is the same display state: no update shown (plan §9).
//! An *install* that fails is reported, and the daemon is allowed back.

use std::sync::Mutex;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_updater::{Update, UpdaterExt};

const OWNER_REPO: &str = "alexnistor0103/claude-usage-widget";

/// The update found by the last check, so a click installs what was offered
/// without a second round trip.
#[derive(Default)]
pub struct Pending(Mutex<Option<Update>>);

#[derive(Debug, Default, PartialEq, Serialize)]
pub struct UpdateInfo {
    available: bool,
    latest: Option<String>,
    url: Option<String>,
}

/// What the UI renders while an install runs. `downloaded`/`total` are bytes;
/// `total` is `None` when the server sent no length.
#[derive(Clone, Serialize)]
#[serde(tag = "phase", rename_all = "lowercase")]
enum Progress {
    Download { downloaded: u64, total: Option<u64> },
    Install,
    Restart,
    Error { message: String },
}

const PROGRESS_EVENT: &str = "update-progress";

#[tauri::command]
pub async fn check_update(app: AppHandle) -> UpdateInfo {
    let Some(update) = check(&app).await else {
        return UpdateInfo::default();
    };
    let info = UpdateInfo {
        available: true,
        latest: Some(update.version.clone()),
        url: Some(release_page()),
    };
    if let Ok(mut slot) = app.state::<Pending>().0.lock() {
        *slot = Some(update);
    }
    info
}

/// Download, install and relaunch. Progress goes out as `update-progress`
/// events; the command itself only returns on failure, since a success ends
/// the process (Windows exits into the installer, macOS restarts).
#[tauri::command]
pub async fn install_update(app: AppHandle) -> Result<(), String> {
    let result = install(&app).await;
    if let Err(e) = &result {
        crate::set_updating(false);
        emit(&app, Progress::Error { message: e.clone() });
    }
    result
}

/// Open the releases page: the fallback when an install fails.
#[tauri::command]
pub fn open_release(app: AppHandle) -> Result<(), String> {
    app.opener()
        .open_url(release_page(), None::<&str>)
        .map_err(|e| e.to_string())
}

async fn check(app: &AppHandle) -> Option<Update> {
    let updater = match app.updater() {
        Ok(u) => u,
        Err(e) => {
            eprintln!("updater: {e}");
            return None;
        }
    };
    match updater.check().await {
        Ok(found) => found,
        Err(e) => {
            // The plugin's Display never carries the response body.
            eprintln!("update check: {e}");
            None
        }
    }
}

async fn install(app: &AppHandle) -> Result<(), String> {
    let pending = app
        .state::<Pending>()
        .0
        .lock()
        .ok()
        .and_then(|mut s| s.take());
    let update = match pending {
        Some(u) => u,
        None => check(app).await.ok_or("no update available")?,
    };

    emit(
        app,
        Progress::Download {
            downloaded: 0,
            total: None,
        },
    );
    let mut downloaded = 0u64;
    let handle = app.clone();
    let bytes = update
        .download(
            move |chunk, total| {
                downloaded += chunk as u64;
                emit(&handle, Progress::Download { downloaded, total });
            },
            || {},
        )
        .await
        .map_err(|e| e.to_string())?;

    emit(app, Progress::Install);
    // The installer overwrites the daemon binary next to the app, which a
    // running daemon locks on Windows; and a daemon left running would be the
    // old build when the new overlay finds its port already answering.
    crate::set_updating(true);
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        crate::stop_daemon_blocking(&handle.state::<crate::DaemonChild>())
    })
    .await
    .map_err(|e| e.to_string())??;

    // Windows never returns from here: the process exits into the installer,
    // which relaunches the app once done.
    update.install(bytes).map_err(|e| e.to_string())?;

    emit(app, Progress::Restart);
    app.restart();
}

fn emit(app: &AppHandle, p: Progress) {
    let _ = app.emit(PROGRESS_EVENT, p);
}

fn release_page() -> String {
    format!("https://github.com/{OWNER_REPO}/releases/latest")
}
