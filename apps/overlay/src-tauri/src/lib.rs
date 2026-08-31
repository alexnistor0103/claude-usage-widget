//! Overlay shell: always-on-top widget, one row per account. Talks to the
//! daemon over localhost and renders percentages — it never sees a Claude token.
//! The only secret it holds is the localhost bearer, which gates the daemon's
//! HTTP surface (plan §5).

mod dock;
#[cfg(any(windows, target_os = "macos"))]
mod platform;
mod settings;
mod tray;
mod update;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, Once, OnceLock};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager, State, WindowEvent};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_window_state::StateFlags;

/// The daemon's default port. The real port is read from the data-dir `port`
/// file the daemon writes after binding; this is only the fallback before that
/// file exists (plan §5).
const DAEMON_PORT: u16 = 8787;

/// The daemon process we spawned, kept so `stop_daemon` can kill it as a last
/// resort. `None` when the daemon was already running or start failed.
pub struct DaemonChild(pub Mutex<Option<Child>>);

/// Read the daemon's localhost bearer from its `0600` file. Same path the daemon
/// derives (`directories` with the `com.local.cuw` triple, then `bearer.token`).
/// This is the localhost gate, not a Claude token — safe in the webview (M3.2).
#[tauri::command]
fn bearer_token() -> Result<String, String> {
    let path = bearer_path().ok_or_else(|| "no data directory".to_string())?;
    let raw = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let token = raw.trim();
    if token.is_empty() {
        return Err("bearer file is empty".into());
    }
    Ok(token.to_string())
}

/// Start the daemon if it is not already listening. Idempotent: a probe of the
/// localhost port short-circuits when the daemon is already up, so the overlay
/// can call this freely on every reconnect attempt (M3.6).
#[tauri::command]
fn start_daemon(state: State<DaemonChild>) -> Result<(), String> {
    if daemon_is_up() {
        return Ok(());
    }
    let child = spawn_daemon().map_err(|e| e.to_string())?;
    if let Ok(mut slot) = state.0.lock() {
        // A previous child that already exited is just dropped; a live one is
        // superseded (the port probe said nothing is listening).
        *slot = Some(child);
    }
    Ok(())
}

/// Stop the daemon: graceful `POST /shutdown`, then the pid file, then our own
/// child handle (M6.2). Runs off the main thread; the bearer is never logged.
#[tauri::command]
async fn stop_daemon(app: AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let child: State<DaemonChild> = app.state();
        stop_daemon_blocking(&child)
    })
    .await
    .map_err(|e| e.to_string())?
}

pub fn stop_daemon_blocking(child: &DaemonChild) -> Result<(), String> {
    let was_up = daemon_is_up();
    if was_up {
        // Errors here are not fatal: the fallbacks below still run.
        let _ = post_shutdown();
        let deadline = Instant::now() + Duration::from_secs(2);
        while daemon_is_up() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    // A hung daemon may hold the pid file without answering the port, so the
    // pid step also runs when the probe said it was down. It is skipped only
    // after a graceful stop, where the pid could already belong to someone else.
    if !was_up || daemon_is_up() {
        if let Some(pid) = read_pid() {
            kill_pid(pid);
        }
    }
    if let Ok(mut slot) = child.0.lock() {
        if let Some(mut c) = slot.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
    Ok(())
}

/// Hand-written HTTP/1.1 over a raw socket: no HTTP client dep for one call.
fn post_shutdown() -> Result<(), String> {
    let bearer = bearer_token()?;
    let addr = SocketAddr::from(([127, 0, 0, 1], daemon_port_value()));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(500))
        .map_err(|e| format!("connect: {e}"))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let req = format!(
        "POST /shutdown HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {bearer}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    let mut sink = Vec::new();
    // Read until EOF so the daemon sees the request fully before we drop.
    let _ = stream.read_to_end(&mut sink);
    Ok(())
}

fn read_pid() -> Option<u32> {
    let path = data_dir()?.join("pid");
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(windows)]
fn kill_pid(pid: u32) {
    let mut cmd = Command::new("taskkill");
    cmd.args(["/PID", &pid.to_string(), "/F", "/T"]);
    detach(&mut cmd);
    run_quiet(&mut cmd);
}

#[cfg(not(windows))]
fn kill_pid(pid: u32) {
    let mut cmd = Command::new("kill");
    cmd.args(["-TERM", &pid.to_string()]);
    run_quiet(&mut cmd);
}

fn run_quiet(cmd: &mut Command) {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let _ = cmd.status();
}

/// Override for the data dir, and it must mean exactly what it means to the
/// daemon (`cuw-daemon` `startup.rs`) — the two disagreeing would point the
/// widget at a different daemon's bearer, port and pid.
const DATA_DIR_ENV: &str = "CUW_DATA_DIR";

/// Resolved once: the env is read at startup, like the daemon's, and the dir is
/// created so the daemon log can be opened before a daemon has ever run.
fn data_dir() -> Option<PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = match data_dir_override(std::env::var(DATA_DIR_ENV).ok().as_deref()) {
            Some(dir) => dir,
            None => directories::ProjectDirs::from("com", "local", "cuw")?
                .data_dir()
                .to_path_buf(),
        };
        let _ = std::fs::create_dir_all(&dir);
        restrict(&dir);
        Some(dir)
    })
    .clone()
}

/// The overlay can reach the data dir before any daemon has run, so it applies
/// the same `0700` the daemon does (`cuw-daemon` `startup.rs`) rather than
/// leaving the dir at the default umask until the first daemon start.
/// Best-effort: `bearer.token` sets its own `0600` regardless.
#[cfg(unix)]
fn restrict(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn restrict(_dir: &std::path::Path) {}

/// Split out so the rule is testable without a shared process environment. A
/// blank value is unset, not "the current directory" — the daemon's rule.
fn data_dir_override(raw: Option<&str>) -> Option<PathBuf> {
    match raw.map(str::trim) {
        Some(s) if !s.is_empty() => Some(PathBuf::from(s)),
        _ => None,
    }
}

fn bearer_path() -> Option<PathBuf> {
    data_dir().map(|d| d.join("bearer.token"))
}

fn port_path() -> Option<PathBuf> {
    data_dir().map(|d| d.join("port"))
}

/// The port the daemon actually bound, published to its data dir. Falls back to
/// the default before the daemon has written it, so the overlay never disagrees
/// with a configured daemon port (M3.6).
fn daemon_port_value() -> u16 {
    port_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(DAEMON_PORT)
}

/// Expose the daemon port to the web layer so it builds the right base URL.
#[tauri::command]
fn daemon_port() -> u16 {
    daemon_port_value()
}

fn daemon_is_up() -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], daemon_port_value()));
    TcpStream::connect_timeout(&addr, Duration::from_millis(250)).is_ok()
}

/// Launch `cuw-daemon` detached from the overlay. Decision (M3.6): a plain
/// detached spawn, not a Tauri sidecar bundle — simpler for a personal dev build
/// and it sidesteps target-triple sidecar naming. Prefer the daemon binary
/// sitting next to the overlay exe (an NSIS install), then the macOS bundle's
/// resource dir, then a built `target/debug/cuw-daemon` in the repo (dev: the
/// Child is then the daemon itself, not cargo), and only last
/// `cargo run -p cuw-daemon`, which exists on no user's machine.
fn spawn_daemon() -> std::io::Result<Child> {
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(daemon_bin_name())));
    // Repo root is three levels above this crate.
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let built = root.join("target").join("debug").join(daemon_bin_name());

    let installed = [sibling, bundled_daemon()]
        .into_iter()
        .flatten()
        .find(|p| p.exists());
    let mut cmd = match installed {
        Some(path) => Command::new(path),
        None if built.exists() => Command::new(built),
        None => {
            let mut c = Command::new("cargo");
            c.args(["run", "-p", "cuw-daemon"]).current_dir(root);
            c
        }
    };

    // Send the daemon's output to a log file rather than null, so a failed poll
    // or connect leaves a trace to read instead of vanishing.
    cmd.stdin(Stdio::null());
    match daemon_log().and_then(|out| out.try_clone().ok().map(|err| (out, err))) {
        Some((out, err)) => {
            cmd.stdout(Stdio::from(out)).stderr(Stdio::from(err));
        }
        None => {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
    detach(&mut cmd);
    cmd.spawn()
}

/// Append-mode handle to the daemon's log in the data dir, if it can be opened.
fn daemon_log() -> Option<std::fs::File> {
    let dir = data_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("daemon.log"))
        .ok()
}

/// Inside a `.app` the executable is in `Contents/MacOS` while Tauri's bundled
/// resources land in `Contents/Resources`, so the sibling lookup finds nothing
/// there and would fall through to the `cargo run` that no user's Mac has (M5).
#[cfg(target_os = "macos")]
fn bundled_daemon() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let contents = exe.parent()?.parent()?;
    Some(contents.join("Resources").join(daemon_bin_name()))
}

#[cfg(not(target_os = "macos"))]
fn bundled_daemon() -> Option<PathBuf> {
    None
}

fn daemon_bin_name() -> &'static str {
    if cfg!(windows) {
        "cuw-daemon.exe"
    } else {
        "cuw-daemon"
    }
}

#[cfg(windows)]
fn detach(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP: no shared console, survives the
    // overlay closing.
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(windows))]
fn detach(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // New session so the daemon is not torn down with the overlay's group.
    unsafe {
        cmd.pre_exec(|| {
            libc_setsid();
            Ok(())
        });
    }
}

#[cfg(not(windows))]
fn libc_setsid() {
    extern "C" {
        fn setsid() -> i32;
    }
    unsafe {
        setsid();
    }
}

/// Our own native window as a plain integer: an `HWND` on Windows, the
/// `NSWindow` pointer on macOS. Main thread only — both getters block from
/// anywhere else, and the `NSWindow` is only sound to touch there.
#[cfg(windows)]
pub fn overlay_hwnd(app: &AppHandle) -> Option<isize> {
    app.get_webview_window("main")
        .and_then(|w| w.hwnd().ok())
        .map(|h| h.0 as isize)
}

#[cfg(target_os = "macos")]
pub fn overlay_hwnd(app: &AppHandle) -> Option<isize> {
    app.get_webview_window("main")
        .and_then(|w| w.ns_window().ok())
        .map(|p| p as isize)
}

/// Re-assert the overlay's own window style, after every
/// show/hide/set_focusable/set_ignore_cursor_events/set_always_on_top.
///
/// Windows *needs* it: tao rewrites the whole ex-style on every flag diff, so a
/// hand-applied `WS_EX_TOOLWINDOW` is gone after the first hide/show (plan §6).
/// macOS has nothing that rewrites the level or the collection behaviour, but
/// re-asserting them is free and keeps one call site for both. Staying out of
/// Cmd-Tab is *not* here: on macOS that is the application's activation policy,
/// set once in `run`.
///
/// Safe from any thread: on the main thread the task runs inline after the
/// inline setters; elsewhere both queue FIFO on the same proxy.
#[cfg(any(windows, target_os = "macos"))]
pub fn restyle(app: &AppHandle) {
    // With always-on-top off a docked widget rides its target's z-order: it
    // enters the topmost band only while the target holds the foreground (a
    // background thread cannot lift a non-topmost window above the foreground
    // window), and drops back to normal otherwise so other applications cover
    // it. The always-on-top setting keeps it topmost unconditionally.
    let topmost = always_on_top_setting(app) || dock::docked_and_focused(app);
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(h) = overlay_hwnd(&app2) {
            platform::style::set_tool_window(h, topmost);
        }
    });
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn restyle(_app: &AppHandle) {}

fn click_through_setting(app: &AppHandle) -> bool {
    app.try_state::<Mutex<settings::Settings>>()
        .and_then(|s| s.lock().ok().map(|s| s.click_through))
        .unwrap_or(false)
}

fn always_on_top_setting(app: &AppHandle) -> bool {
    app.try_state::<Mutex<settings::Settings>>()
        .and_then(|s| s.lock().ok().map(|s| s.always_on_top))
        .unwrap_or(true)
}

/// Derive the window style from settings and dock state. Any thread: the
/// window work is handed to the main thread (M6.3).
pub fn apply_style_from_settings(app: &AppHandle) {
    let click_through = click_through_setting(app);
    let always_on_top = always_on_top_setting(app);
    let docked = dock::is_docked(app);
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(w) = app2.get_webview_window("main") {
            let _ = w.set_ignore_cursor_events(click_through);
            let _ = w.set_focusable(!(click_through || docked));
            let _ = w.set_always_on_top(always_on_top);
        }
        restyle(&app2);
    });
}

/// `on`: the window takes clicks and keys whatever settings or docking say —
/// a click-through window swallows clicks on modals, a non-focusable one
/// cannot type. `!on`: back to the derived style, focus returned to the dock
/// target when docked.
pub fn set_interactive(app: &AppHandle, on: bool) {
    if on {
        let app2 = app.clone();
        let _ = app.run_on_main_thread(move || {
            if let Some(w) = app2.get_webview_window("main") {
                let _ = w.set_ignore_cursor_events(false);
                let _ = w.set_focusable(true);
                let _ = w.set_focus();
            }
            restyle(&app2);
        });
    } else {
        apply_style_from_settings(app);
        if dock::is_docked(app) {
            dock::focus_target(app);
        }
    }
}

#[tauri::command]
async fn modal_interactive(app: AppHandle, on: bool) -> Result<(), String> {
    set_interactive(&app, on);
    Ok(())
}

/// Register/unregister the overlay at login. A debug build only stores the
/// flag: registering `target/debug/cuw-overlay.exe` would be wrong.
pub fn apply_autostart(app: &AppHandle, on: bool) {
    if cfg!(debug_assertions) {
        static WARNED: Once = Once::new();
        WARNED.call_once(|| eprintln!("dev build: autostart flag stored but not registered"));
        return;
    }
    let al = app.autolaunch();
    let result = if on { al.enable() } else { al.disable() };
    if let Err(e) = result {
        eprintln!("autostart {}: {e}", if on { "enable" } else { "disable" });
    }
}

/// Startup: bring the OS registration in line with the stored flag.
fn reconcile_autostart(app: &AppHandle, on: bool) {
    if cfg!(debug_assertions) {
        return;
    }
    match app.autolaunch().is_enabled() {
        Ok(enabled) if enabled != on => apply_autostart(app, on),
        Ok(_) => {}
        Err(e) => eprintln!("autostart query: {e}"),
    }
}

/// Hosts the webview may open in the browser: sign-in and account pages only.
fn url_allowed(url: &str) -> bool {
    // Scheme and host are both case-insensitive; lowercase once, up front.
    let lower = url.to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix("https://") else {
        return false;
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = &rest[..end];
    let plain = !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    if !plain {
        return false;
    }
    matches!(
        host,
        "claude.ai"
            | "claude.com"
            | "www.claude.com"
            | "console.anthropic.com"
            | "platform.claude.com"
    ) || host.ends_with(".claude.ai")
        || host.ends_with(".claude.com")
}

#[tauri::command]
fn open_url(app: AppHandle, url: String) -> Result<(), String> {
    if !url_allowed(&url) {
        return Err("refusing to open that url".into());
    }
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())
}

pub fn run() {
    tauri::Builder::default()
        // Restore/persist window position and size across restarts (M3.7).
        // Not VISIBLE: a tray-hidden overlay must not come back hidden.
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(StateFlags::SIZE | StateFlags::POSITION)
                .build(),
        )
        .plugin(
            tauri_plugin_autostart::Builder::new()
                .app_name("cuw-overlay")
                .args(["--autostart"])
                .build(),
        )
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let handle = app.handle().clone();
            let s = settings::load(&handle);
            let autostart = s.autostart;
            #[cfg(any(windows, target_os = "macos"))]
            let dock_boot = s.dock.enabled && s.dock.remembered.is_some();
            app.manage(Mutex::new(s));
            app.manage(DaemonChild(Mutex::new(None)));
            // Out of the Dock and out of Cmd-Tab is an *application* property on
            // macOS, not a per-window flag the way `WS_EX_TOOLWINDOW` is — so it
            // is set once here and never per window (plan §6). The bundle's
            // `LSUIElement` says the same thing before the first frame; this is
            // what makes it hold in `tauri dev` too.
            #[cfg(target_os = "macos")]
            if let Err(e) = handle.set_activation_policy(tauri::ActivationPolicy::Accessory) {
                eprintln!("activation policy: {e}");
            }
            restyle(&handle);
            if let Err(e) = tray::build(&handle) {
                // Without the tray there is no way back from click-through.
                eprintln!("tray unavailable, click-through forced off: {e}");
                if let Err(e) = settings::update(&handle, |s| s.click_through = false) {
                    eprintln!("could not clear click-through: {e}");
                }
            }
            apply_style_from_settings(&handle);
            reconcile_autostart(&handle, autostart);
            #[cfg(any(windows, target_os = "macos"))]
            {
                app.manage(dock::SharedDock::default());
                // ensure_started only: the tracker's own remembered target
                // performs the attach, a second attach would double the
                // Attached event and the settings write (plan §6).
                if dock_boot {
                    if let Err(e) = dock::ensure_started(&handle) {
                        eprintln!("dock start: {e}");
                    }
                }
            }
            Ok(())
        })
        // Alt+F4 / close hides *while the tray exists* — it is the only way back
        // from a hidden window. With no tray the close proceeds and takes the
        // daemon with it, rather than stranding an invisible overlay (M6.2).
        .on_window_event(|w, e| match e {
            WindowEvent::CloseRequested { api, .. } => {
                let app = w.app_handle();
                if !tray::exists(app) {
                    let _ = stop_daemon_blocking(&app.state::<DaemonChild>());
                    return;
                }
                api.prevent_close();
                let _ = w.hide();
                restyle(app);
                tray::set_toggle_label(app, false);
            }
            // Hide/minimise fire Resized(0×0); replace_last also gates on the
            // window being visible and docked.
            WindowEvent::Resized(s) if s.width > 0 && s.height > 0 => {
                dock::replace_last(w.app_handle());
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            bearer_token,
            start_daemon,
            stop_daemon,
            daemon_port,
            modal_interactive,
            open_url,
            update::check_update,
            update::open_release,
            settings::get_settings,
            settings::set_settings,
            dock::dock_start,
            dock::dock_pick,
            dock::dock_stop,
            dock::dock_state,
            dock::dock_accessibility,
            dock::dock_grant_accessibility
        ])
        .run(tauri::generate_context!())
        .expect("failed to start overlay");
}

#[cfg(test)]
mod tests {
    use super::{data_dir_override, url_allowed};

    #[test]
    fn data_dir_override_matches_the_daemons_rule() {
        assert_eq!(data_dir_override(None), None);
        assert_eq!(data_dir_override(Some("")), None);
        assert_eq!(data_dir_override(Some("   ")), None);
        assert_eq!(
            data_dir_override(Some("  D:/cuw-test  ")),
            Some(std::path::PathBuf::from("D:/cuw-test"))
        );
    }

    #[test]
    fn open_url_accepts_known_hosts() {
        for url in [
            "https://claude.ai/",
            "https://claude.ai",
            "https://CLAUDE.AI/login?x=1#y",
            "HTTPS://claude.ai/login",
            "https://claude.com/path",
            "https://www.claude.com",
            "https://console.anthropic.com/settings",
            "https://platform.claude.com/oauth/authorize?code=true",
            "https://auth.claude.ai/x",
            "https://a.b.claude.com/",
        ] {
            assert!(url_allowed(url), "{url}");
        }
    }

    #[test]
    fn open_url_rejects_foreign_or_insecure() {
        for url in [
            "http://claude.ai/",
            "https://anthropic.com/",
            "https://www.anthropic.com/",
            "https://evilclaude.ai/",
            "https://claude.ai.evil.example/",
            "https://claude.ai@evil.example/",
            "https://claude.ai:8443/",
            "https://claude.ai\\evil.example/",
            "https://",
            "https:///claude.ai",
            "file:///C:/x",
            "javascript:alert(1)",
            "",
        ] {
            assert!(!url_allowed(url), "{url}");
        }
    }
}
