//! Docking glue (M4.4–M4.7, M5): drives the `cuw-tracker` pump and moves the
//! overlay on its events. Two rules hold everywhere (plan §6): the `DockCtl`
//! lock is never held across a Tauri window call, and every raw native-handle
//! call runs on the main thread — `HWND` work is a synchronous `SendMessage` to
//! the owning thread, and `NSWindow` is main-thread-only. Docking is off by
//! default; with `dock.enabled=false` the tracker thread is never even started.
//!
//! One body serves both platforms; the platform tracker arrives under fixed
//! names from [`crate::platform`].

use serde::Serialize;
use tauri::AppHandle;

/// Emitted as `dock-state` on every transition; `Detached` means the tracker
/// is searching for a target it once had (plan §6).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum DockState {
    Undocked,
    Picking,
    Docked { target: String },
    Detached { target: String },
}

#[cfg(not(any(windows, target_os = "macos")))]
const UNAVAILABLE: &str = "docking is not available on this platform";

/// Dock to `target` (a `TargetId` string), or to the best allowed/remembered
/// candidate when `None`.
#[tauri::command]
pub async fn dock_start(app: AppHandle, target: Option<String>) -> Result<(), String> {
    #[cfg(any(windows, target_os = "macos"))]
    return imp::start(&app, target);
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (app, target);
        Err(UNAVAILABLE.into())
    }
}

/// Arm the interactive picker: the next window the user focuses becomes the target.
#[tauri::command]
pub async fn dock_pick(app: AppHandle) -> Result<(), String> {
    #[cfg(any(windows, target_os = "macos"))]
    return imp::pick(&app);
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = app;
        Err(UNAVAILABLE.into())
    }
}

#[tauri::command]
pub async fn dock_stop(app: AppHandle) -> Result<(), String> {
    #[cfg(any(windows, target_os = "macos"))]
    return imp::stop(&app);
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = app;
        Err(UNAVAILABLE.into())
    }
}

#[tauri::command]
pub async fn dock_state(app: AppHandle) -> DockState {
    #[cfg(any(windows, target_os = "macos"))]
    return imp::state(&app);
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = app;
        DockState::Undocked
    }
}

/// What the UI may honestly say about the Accessibility grant (plan §6).
/// macOS docking works without it and only gets smoother with it, so this is a
/// hint and never a gate. `applicable` is false where there is no such
/// permission to ask for, which is every platform but macOS.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Accessibility {
    pub applicable: bool,
    pub trusted: bool,
}

#[tauri::command]
pub async fn dock_accessibility() -> Accessibility {
    #[cfg(target_os = "macos")]
    return Accessibility {
        applicable: true,
        trusted: cuw_tracker::macos::accessibility_trusted(),
    };
    #[cfg(not(target_os = "macos"))]
    Accessibility {
        applicable: false,
        trusted: false,
    }
}

/// Raise the system's Accessibility grant dialog. A user action only — the
/// tracker thread must never raise it (plan §6) — and it goes through AppKit
/// rather than a System Settings deep link because `open_url`'s allowlist is
/// https-only and must stay that way.
#[tauri::command]
pub async fn dock_grant_accessibility(app: AppHandle) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    return app
        .run_on_main_thread(cuw_tracker::macos::request_accessibility_prompt)
        .map_err(|e| e.to_string());
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        Err("Accessibility is a macOS permission".into())
    }
}

#[cfg(any(windows, target_os = "macos"))]
pub use imp::{
    docked_and_focused, ensure_started, focus_target, is_docked, replace_last, SharedDock,
};

#[cfg(not(any(windows, target_os = "macos")))]
pub fn is_docked(_app: &AppHandle) -> bool {
    false
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn docked_and_focused(_app: &AppHandle) -> bool {
    false
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn focus_target(_app: &AppHandle) {}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn replace_last(_app: &AppHandle) {}

#[cfg(any(windows, target_os = "macos"))]
mod imp {
    use std::sync::mpsc::{self, Receiver};
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::thread::JoinHandle;
    use std::time::Duration;

    use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, Position};

    use cuw_tracker::geometry::{
        clamp_to_work_area, intersects, overlay_origin, parse_target_id, rank_candidates, Anchor,
        Corner,
    };
    use cuw_tracker::{
        Bounds, TargetId, TargetSpec, TrackerConfig, TrackerEvent, TrackerHandle, WindowTracker,
    };

    use super::DockState;
    use crate::platform::{bounds, find, style, Handle, PlatformTracker};
    use crate::{settings, tray};

    pub struct DockCtl {
        handle: Option<Handle>,
        state: DockState,
        last: Option<Bounds>,
        target_hwnd: Option<isize>,
        /// True while the docked target holds the foreground. Drives the
        /// topmost band: a docked widget floats above the target only while the
        /// target is up front, and drops to the normal band otherwise so the
        /// next application the user brings forward covers it.
        target_focused: bool,
        /// Cached once on the main thread; the consumer never calls `window.hwnd()`.
        own_hwnd: Option<isize>,
        #[allow(dead_code)]
        consumer: Option<JoinHandle<()>>,
    }

    impl Default for DockCtl {
        fn default() -> Self {
            Self {
                handle: None,
                state: DockState::Undocked,
                last: None,
                target_hwnd: None,
                target_focused: false,
                own_hwnd: None,
                consumer: None,
            }
        }
    }

    pub type SharedDock = Arc<Mutex<DockCtl>>;

    fn ctl(app: &AppHandle) -> Option<SharedDock> {
        app.try_state::<SharedDock>().map(|s| s.inner().clone())
    }

    fn lock(m: &Mutex<DockCtl>) -> MutexGuard<'_, DockCtl> {
        m.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn dock_settings(app: &AppHandle) -> settings::Dock {
        app.state::<Mutex<settings::Settings>>()
            .lock()
            .map(|s| s.dock.clone())
            .unwrap_or_default()
    }

    struct Started {
        fresh: bool,
        remembered: bool,
    }

    /// Start the tracker thread and the event consumer if they are not running.
    /// Idempotent; also flips the window into its docked style (non-focusable,
    /// topmost) — callers only reach here because docking is starting.
    pub fn ensure_started(app: &AppHandle) -> Result<(), String> {
        start_tracker(app).map(|_| ())
    }

    fn start_tracker(app: &AppHandle) -> Result<Started, String> {
        let dock = ctl(app).ok_or("dock state not managed")?;
        if lock(&dock).handle.is_some() {
            return Ok(Started {
                fresh: false,
                remembered: false,
            });
        }
        let d = dock_settings(app);
        let remembered = d.remembered.is_some();
        let cfg = TrackerConfig {
            allow: d
                .allow
                .iter()
                .map(|a| TargetSpec {
                    class: a.class.clone(),
                    exe: a.exe.clone(),
                })
                .collect(),
            remembered: d.remembered.map(TargetId),
            follow_focus: d.follow_focus,
        };
        let (handle, rx) = PlatformTracker::start(cfg).map_err(|e| format!("{e:#}"))?;
        let own = cached_own_hwnd(app);
        let consumer = std::thread::Builder::new()
            .name("cuw-dock".to_string())
            .spawn({
                let app = app.clone();
                move || consume(app, rx)
            })
            .map_err(|e| e.to_string())?;
        {
            let mut d = lock(&dock);
            if d.handle.is_some() {
                // A concurrent start won; dropping our handle's channel also
                // ends the consumer we just spawned.
                handle.stop();
                return Ok(Started {
                    fresh: false,
                    remembered: false,
                });
            }
            d.handle = Some(handle);
            d.own_hwnd = own;
            d.consumer = Some(consumer);
        }
        let app2 = app.clone();
        let _ = app.run_on_main_thread(move || {
            if let Some(w) = app2.get_webview_window("main") {
                let _ = w.set_focusable(false);
            }
            crate::restyle(&app2);
        });
        Ok(Started {
            fresh: true,
            remembered,
        })
    }

    /// `window.hwnd()` blocks when called off the main thread, so it is read
    /// there once and cached.
    fn cached_own_hwnd(app: &AppHandle) -> Option<isize> {
        let (tx, rx) = mpsc::channel();
        let app2 = app.clone();
        let _ = app.run_on_main_thread(move || {
            let _ = tx.send(crate::overlay_hwnd(&app2));
        });
        rx.recv_timeout(Duration::from_secs(2)).ok().flatten()
    }

    pub fn start(app: &AppHandle, target: Option<String>) -> Result<(), String> {
        let started = start_tracker(app)?;
        // A fresh start with a remembered target attaches by itself; a second
        // attach would emit two Attached events and two settings writes.
        if target.is_none() && started.fresh && started.remembered {
            return Ok(());
        }
        let dock = ctl(app).ok_or("dock state not managed")?;
        let d = lock(&dock);
        let handle = d.handle.as_ref().ok_or("tracker is not running")?;
        // attach() only queues a command; no window call happens under the lock.
        handle
            .attach(target.map(TargetId))
            .map_err(|e| format!("{e:#}"))
    }

    pub fn pick(app: &AppHandle) -> Result<(), String> {
        start_tracker(app)?;
        set_state(app, DockState::Picking);
        let dock = ctl(app).ok_or("dock state not managed")?;
        let d = lock(&dock);
        let handle = d.handle.as_ref().ok_or("tracker is not running")?;
        handle.pick_interactively().map_err(|e| format!("{e:#}"))
    }

    pub fn stop(app: &AppHandle) -> Result<(), String> {
        let dock = ctl(app).ok_or("dock state not managed")?;
        {
            let mut d = lock(&dock);
            if let Some(h) = d.handle.as_ref() {
                h.detach().map_err(|e| format!("{e:#}"))?;
            }
            d.target_hwnd = None;
            d.target_focused = false;
            d.last = None;
        }
        set_state(app, DockState::Undocked);
        settings::update(app, |s| s.dock.enabled = false)?;
        // Restores focusability; the tracker thread stays, idle.
        crate::apply_style_from_settings(app);
        Ok(())
    }

    pub fn state(app: &AppHandle) -> DockState {
        ctl(app)
            .map(|d| lock(&d).state.clone())
            .unwrap_or(DockState::Undocked)
    }

    /// Docked or detached: both mean the window is placed by the tracker.
    pub fn is_docked(app: &AppHandle) -> bool {
        matches!(
            state(app),
            DockState::Docked { .. } | DockState::Detached { .. }
        )
    }

    /// Hand focus back to the target after a modal closes (plan §6). Windows
    /// allows the `SetForegroundWindow` because we are the foreground process
    /// at that moment; macOS has no such rule — it activates *applications*,
    /// and a modern system may refuse a cross-application activation outright,
    /// so the call is best-effort and its failure is cosmetic.
    pub fn focus_target(app: &AppHandle) {
        let Some(dock) = ctl(app) else { return };
        let hwnd = {
            let d = lock(&dock);
            matches!(d.state, DockState::Docked { .. })
                .then_some(d.target_hwnd)
                .flatten()
        };
        if let Some(h) = hwnd {
            let _ = app.run_on_main_thread(move || style::bring_to_foreground(h));
        }
    }

    /// `true` while a docked widget should float in the topmost band — i.e. its
    /// target holds the foreground. `restyle` ORs this with the always-on-top
    /// setting; a docked widget is only ever shown while the target is up front
    /// (see the `Focused` handler), so this genuinely enters the topmost band
    /// instead of a plain `HWND_TOP`, which a background thread cannot lift
    /// above the foreground window.
    pub fn docked_and_focused(app: &AppHandle) -> bool {
        ctl(app)
            .map(|d| {
                let g = lock(&d);
                matches!(
                    g.state,
                    DockState::Docked { .. } | DockState::Detached { .. }
                ) && g.target_focused
            })
            .unwrap_or(false)
    }

    /// Re-run placement with the last bounds — after a settings change or a
    /// window resize. Only while docked and visible; hide/minimise fire
    /// `Resized(0×0)` and must not move anything.
    pub fn replace_last(app: &AppHandle) {
        let Some(dock) = ctl(app) else { return };
        let last = {
            let d = lock(&dock);
            matches!(d.state, DockState::Docked { .. })
                .then_some(d.last)
                .flatten()
        };
        let Some(b) = last else { return };
        let visible = app
            .get_webview_window("main")
            .and_then(|w| w.is_visible().ok())
            .unwrap_or(false);
        if visible {
            place(app, b);
        }
    }

    /// Write under the lock, release, then emit — never the other way round.
    fn set_state(app: &AppHandle, new: DockState) {
        let Some(dock) = ctl(app) else { return };
        let changed = {
            let mut d = lock(&dock);
            if d.state == new {
                false
            } else {
                d.state = new.clone();
                true
            }
        };
        if !changed {
            return;
        }
        let docked = matches!(new, DockState::Docked { .. } | DockState::Detached { .. });
        let _ = app.emit("dock-state", &new);
        tray::set_dock_items(app, docked, true);
    }

    // -----------------------------------------------------------------------
    // Consumer thread
    // -----------------------------------------------------------------------

    fn consume(app: AppHandle, rx: Receiver<TrackerEvent>) {
        while let Ok(first) = rx.recv() {
            let mut batch = vec![first];
            while let Ok(ev) = rx.try_recv() {
                batch.push(ev);
            }
            // A drag delivers one Bounds per mouse move; only the newest one
            // matters. Non-Bounds events keep their order.
            let last_bounds = batch
                .iter()
                .rposition(|e| matches!(e, TrackerEvent::Bounds(_)));
            for (i, ev) in batch.into_iter().enumerate() {
                if matches!(ev, TrackerEvent::Bounds(_)) && Some(i) != last_bounds {
                    continue;
                }
                on_event(&app, ev);
            }
        }
    }

    fn on_event(app: &AppHandle, ev: TrackerEvent) {
        let Some(dock) = ctl(app) else { return };
        match ev {
            TrackerEvent::Attached(id) => {
                let target = id.0;
                let hwnd = resolve_target_hwnd(&target);
                {
                    let mut d = lock(&dock);
                    d.target_hwnd = hwnd;
                    // Fresh dock: place the widget above the target straight away.
                    d.target_focused = true;
                }
                set_state(
                    app,
                    DockState::Docked {
                        target: target.clone(),
                    },
                );
                let t = target;
                if let Err(e) = settings::update(app, move |s| {
                    s.dock.enabled = true;
                    s.dock.remembered = Some(t);
                }) {
                    eprintln!("dock settings write failed: {e}");
                }
                // show_overlay's restyle enters the topmost band (docked +
                // focused), so it must run after target_focused is set above.
                show_overlay(app);
            }
            TrackerEvent::Bounds(b) => place(app, b),
            TrackerEvent::Minimized => hide_overlay(app),
            TrackerEvent::Restored => {
                // A restored target is up front; float above it again.
                lock(&dock).target_focused = true;
                show_overlay(app);
            }
            TrackerEvent::Focused(focused) => {
                // A docked widget shows only while its target holds the
                // foreground; any other window taking it hides the widget.
                // Set target_focused first so show_overlay's restyle enters the
                // topmost band.
                lock(&dock).target_focused = focused;
                if focused {
                    show_overlay(app);
                } else {
                    hide_overlay(app);
                }
            }
            TrackerEvent::Lost => {
                // Float the detached badge above the band so the user notices
                // the widget lost its target rather than losing the widget.
                lock(&dock).target_focused = true;
                // Show first: a follow-focus hide would hide the detached badge.
                show_overlay(app);
                let target = {
                    let mut d = lock(&dock);
                    d.target_hwnd = None;
                    match &d.state {
                        DockState::Docked { target } | DockState::Detached { target } => {
                            target.clone()
                        }
                        _ => String::new(),
                    }
                };
                let target = if target.is_empty() {
                    dock_settings(app).remembered.unwrap_or_default()
                } else {
                    target
                };
                set_state(app, DockState::Detached { target });
            }
            TrackerEvent::NotFound => {
                let was_picking = matches!(lock(&dock).state, DockState::Picking);
                let d = dock_settings(app);
                if was_picking {
                    // Pick timeout: docking goes back off; the tracker may
                    // still hold a previous target, so detach it too.
                    if let Some(h) = lock(&dock).handle.as_ref() {
                        let _ = h.detach();
                    }
                    set_state(app, DockState::Undocked);
                    if let Err(e) = settings::update(app, |s| s.dock.enabled = false) {
                        eprintln!("dock settings write failed: {e}");
                    }
                    crate::apply_style_from_settings(app);
                } else if d.enabled && d.remembered.is_some() {
                    // A remembered terminal that is not running yet: the
                    // tracker keeps searching (plan §6).
                    set_state(
                        app,
                        DockState::Detached {
                            target: d.remembered.unwrap_or_default(),
                        },
                    );
                } else {
                    set_state(app, DockState::Undocked);
                    crate::apply_style_from_settings(app);
                }
            }
        }
    }

    /// The tracker reports a `TargetId`, never an hwnd, so the hwnd used for
    /// focus hand-back and the work-area fallback is re-derived with the same
    /// ranking the tracker itself used.
    fn resolve_target_hwnd(target: &str) -> Option<isize> {
        let id = TargetId(target.to_string());
        let spec = parse_target_id(&id)?;
        let cands = find::candidates();
        rank_candidates(
            &cands,
            std::slice::from_ref(&spec),
            find::foreground(),
            Some(&id),
        )
        .map(|i| cands[i].hwnd)
    }

    fn place(app: &AppHandle, b: Bounds) {
        let Some(dock) = ctl(app) else { return };
        let target_hwnd = lock(&dock).target_hwnd;
        let Some(window) = app.get_webview_window("main") else {
            return;
        };
        let Ok(size) = window.outer_size() else {
            return;
        };
        if size.width == 0 || size.height == 0 {
            return;
        }
        let overlay = (size.width as i32, size.height as i32);
        let d = dock_settings(app);
        let anchor = Anchor {
            corner: corner(d.corner),
            // Offsets are logical px; bounds arrive physical.
            dx: (f64::from(d.offset.x) * b.scale).round() as i32,
            dy: (f64::from(d.offset.y) * b.scale).round() as i32,
            inside: d.inside,
        };
        let origin = overlay_origin(&b, overlay, &anchor);
        let vs = bounds::virtual_screen();
        let mut origin = clamp_to_work_area(origin, overlay, &vs);
        // Cannot miss after the clamp; kept as the off-every-monitor fallback.
        if !intersects(origin, overlay, &vs) {
            if let Some(wa) = target_hwnd.and_then(bounds::work_area_for) {
                origin = clamp_to_work_area(origin, overlay, &wa);
            }
        }
        let _ = window.set_position(Position::Physical(PhysicalPosition {
            x: origin.0,
            y: origin.1,
        }));
        lock(&dock).last = Some(b);
    }

    fn corner(c: settings::Corner) -> Corner {
        match c {
            settings::Corner::TopLeft => Corner::TopLeft,
            settings::Corner::TopRight => Corner::TopRight,
            settings::Corner::BottomLeft => Corner::BottomLeft,
            settings::Corner::BottomRight => Corner::BottomRight,
        }
    }

    fn show_overlay(app: &AppHandle) {
        let app2 = app.clone();
        let _ = app.run_on_main_thread(move || {
            if let Some(w) = app2.get_webview_window("main") {
                let _ = w.show();
            }
            crate::restyle(&app2);
            tray::set_toggle_label(&app2, true);
        });
    }

    fn hide_overlay(app: &AppHandle) {
        let app2 = app.clone();
        let _ = app.run_on_main_thread(move || {
            if let Some(w) = app2.get_webview_window("main") {
                let _ = w.hide();
            }
            crate::restyle(&app2);
            tray::set_toggle_label(&app2, false);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{Accessibility, DockState};

    #[test]
    fn accessibility_serializes_the_two_flags_the_panel_reads() {
        let json = serde_json::to_string(&Accessibility {
            applicable: true,
            trusted: false,
        })
        .unwrap();
        assert_eq!(json, r#"{"applicable":true,"trusted":false}"#);
    }

    #[test]
    fn dock_state_serializes_tagged_snake_case() {
        let json = |s: &DockState| serde_json::to_string(s).unwrap();
        assert_eq!(json(&DockState::Undocked), r#"{"state":"undocked"}"#);
        assert_eq!(json(&DockState::Picking), r#"{"state":"picking"}"#);
        assert_eq!(
            json(&DockState::Docked {
                target: "win32:X|y.exe".into()
            }),
            r#"{"state":"docked","target":"win32:X|y.exe"}"#
        );
        assert_eq!(
            json(&DockState::Detached {
                target: "win32:X|".into()
            }),
            r#"{"state":"detached","target":"win32:X|"}"#
        );
    }
}
