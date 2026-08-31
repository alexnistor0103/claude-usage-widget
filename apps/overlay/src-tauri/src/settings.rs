//! Rust-owned `settings.json` in the app config dir (M6.1). The panel sends
//! patches, never whole objects, so a docking event that rewrote the file while
//! the panel was open is not clobbered on Save.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

const FILE_NAME: &str = "settings.json";

/// Bumped when the meaning of a stored field changes, so `validate` can migrate
/// an older file once. v2: `dock.follow_focus` no longer hides the widget (that
/// is automatic while docked now) — it only re-docks to another allowed window
/// on focus, so a v1 `true` (which meant "hide") must not carry over as that.
const SCHEMA_VERSION: u32 = 2;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    pub opacity: f32,
    pub compact: bool,
    pub thresholds: Thresholds,
    pub autostart: bool,
    pub click_through: bool,
    pub always_on_top: bool,
    pub show_scoped: bool,
    pub colors: BTreeMap<String, String>,
    pub dock: Dock,
    pub session: Session,
}

/// How a switched session is started (SWITCHER §5). `terminal` is argv, never a
/// shell string, so a path with spaces cannot come apart; empty means the
/// platform default. Empty `cwd` leaves the directory to the daemon.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(default)]
pub struct Session {
    pub terminal: Vec<String>,
    pub cwd: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct Thresholds {
    pub warn: u8,
    pub crit: u8,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct Dock {
    pub enabled: bool,
    /// Last docked target, written by the tracker side only.
    pub remembered: Option<String>,
    pub corner: Corner,
    pub offset: Offset,
    pub inside: bool,
    pub follow_focus: bool,
    pub allow: Vec<AllowSpec>,
    /// Show the macOS Accessibility line in the panel. Dismissible, and it
    /// never gates docking — the poll path needs no grant (plan §6). Inert
    /// everywhere else.
    pub show_accessibility_hint: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Corner {
    TopLeft,
    #[default]
    TopRight,
    BottomLeft,
    BottomRight,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct Offset {
    pub x: i32,
    pub y: i32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AllowSpec {
    pub class: String,
    #[serde(default)]
    pub exe: Option<String>,
}

/// What the panel may write: every field optional; `dock.remembered` is
/// Rust-owned and never accepted.
#[derive(Deserialize, Default, Debug)]
#[serde(default)]
pub struct SettingsPatch {
    pub opacity: Option<f32>,
    pub compact: Option<bool>,
    pub thresholds: Option<Thresholds>,
    pub autostart: Option<bool>,
    pub click_through: Option<bool>,
    pub always_on_top: Option<bool>,
    pub show_scoped: Option<bool>,
    pub colors: Option<BTreeMap<String, String>>,
    pub dock: Option<DockPatch>,
    pub session: Option<SessionPatch>,
}

#[derive(Deserialize, Default, Debug)]
#[serde(default)]
pub struct DockPatch {
    pub enabled: Option<bool>,
    pub corner: Option<Corner>,
    pub offset: Option<Offset>,
    pub inside: Option<bool>,
    pub follow_focus: Option<bool>,
    pub allow: Option<Vec<AllowSpec>>,
    pub show_accessibility_hint: Option<bool>,
}

#[derive(Deserialize, Default, Debug)]
#[serde(default)]
pub struct SessionPatch {
    pub terminal: Option<Vec<String>>,
    pub cwd: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            opacity: 0.85,
            compact: false,
            thresholds: Thresholds::default(),
            autostart: false,
            click_through: false,
            always_on_top: false,
            show_scoped: true,
            colors: BTreeMap::new(),
            dock: Dock::default(),
            session: Session::default(),
        }
    }
}

impl Default for Thresholds {
    fn default() -> Self {
        Self { warn: 75, crit: 90 }
    }
}

impl Default for Offset {
    fn default() -> Self {
        Self { x: 1, y: 42 }
    }
}

impl Default for Dock {
    fn default() -> Self {
        Self {
            enabled: false,
            remembered: None,
            corner: Corner::TopRight,
            offset: Offset::default(),
            inside: false,
            follow_focus: false,
            allow: default_allow(),
            show_accessibility_hint: true,
        }
    }
}

/// Windows matches a window class, plus an exe basename where the class is
/// generic. macOS matches the owning application's bundle id and has no second
/// field (`geometry::parse_target_id`), so the two vocabularies never overlap:
/// a `settings.json` carried between platforms keeps entries that simply never
/// match, which is inert rather than fatal.
#[cfg(not(target_os = "macos"))]
fn default_allow() -> Vec<AllowSpec> {
    let spec = |class: &str, exe: Option<&str>| AllowSpec {
        class: class.to_string(),
        exe: exe.map(str::to_string),
    };
    vec![
        spec("CASCADIA_HOSTING_WINDOW_CLASS", None),
        spec("ConsoleWindowClass", None),
        spec("org.wezfurlong.wezterm", None),
        spec("Alacritty", None),
        spec("Chrome_WidgetWin_1", Some("Code.exe")),
        spec("Chrome_WidgetWin_1", Some("Hyper.exe")),
    ]
}

#[cfg(target_os = "macos")]
fn default_allow() -> Vec<AllowSpec> {
    let spec = |class: &str| AllowSpec {
        class: class.to_string(),
        exe: None,
    };
    vec![
        spec("com.apple.Terminal"),
        spec("com.googlecode.iterm2"),
        spec("com.microsoft.VSCode"),
        spec("com.anthropic.claudefordesktop"),
    ]
}

pub fn path(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_config_dir().ok().map(|d| d.join(FILE_NAME))
}

/// Missing or corrupt file → defaults; the file is rewritten on the next save.
pub fn load(app: &AppHandle) -> Settings {
    let Some(path) = path(app) else {
        return Settings::default();
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(_) => return Settings::default(),
    };
    match serde_json::from_str::<Settings>(&raw) {
        Ok(s) => validate(s),
        Err(e) => {
            if cfg!(debug_assertions) {
                eprintln!("settings.json unreadable, using defaults: {e}");
            }
            Settings::default()
        }
    }
}

/// Atomic write: tmp file in the same dir, then rename over the target.
pub fn save(app: &AppHandle, s: &Settings) -> Result<(), String> {
    let path = path(app).ok_or_else(|| "no config directory".to_string())?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let body = serde_json::to_vec_pretty(s).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

pub fn validate(mut s: Settings) -> Settings {
    // A v1 file's follow_focus=true meant "hide when unfocused", which is now
    // automatic while docked; the flag only re-docks between allowed windows.
    // Clear it so the changed meaning does not surprise, then stamp the version.
    if s.version < SCHEMA_VERSION {
        if s.version < 2 {
            s.dock.follow_focus = false;
        }
        s.version = SCHEMA_VERSION;
    }
    // A docked widget is placed over its target and floats above it anyway,
    // so docking turns the global always-on-top off rather than fighting it.
    if s.dock.enabled {
        s.always_on_top = false;
    }
    s.opacity = if s.opacity.is_finite() {
        s.opacity.clamp(0.2, 1.0)
    } else {
        Settings::default().opacity
    };
    let t = &mut s.thresholds;
    t.warn = t.warn.clamp(1, 100);
    t.crit = t.crit.clamp(1, 100);
    if t.warn >= t.crit {
        *t = Thresholds::default();
    }
    s.colors.retain(|_, v| is_hex_color(v));
    // A blank argv entry would be passed to the terminal as an empty argument;
    // the textarea produces one for every stray line.
    s.session.terminal.retain(|a| !a.trim().is_empty());
    s.session.cwd = s.session.cwd.trim().to_string();
    s
}

fn is_hex_color(v: &str) -> bool {
    let Some(hex) = v.strip_prefix('#') else {
        return false;
    };
    hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Apply every `Some` in the patch. `dock.remembered` is never touched.
pub fn merge(base: &mut Settings, patch: SettingsPatch) {
    let SettingsPatch {
        opacity,
        compact,
        thresholds,
        autostart,
        click_through,
        always_on_top,
        show_scoped,
        colors,
        dock,
        session,
    } = patch;
    if let Some(v) = opacity {
        base.opacity = v;
    }
    if let Some(v) = compact {
        base.compact = v;
    }
    if let Some(v) = thresholds {
        base.thresholds = v;
    }
    if let Some(v) = autostart {
        base.autostart = v;
    }
    if let Some(v) = click_through {
        base.click_through = v;
    }
    if let Some(v) = always_on_top {
        base.always_on_top = v;
    }
    if let Some(v) = show_scoped {
        base.show_scoped = v;
    }
    if let Some(v) = colors {
        base.colors = v;
    }
    if let Some(d) = dock {
        if let Some(v) = d.enabled {
            base.dock.enabled = v;
        }
        if let Some(v) = d.corner {
            base.dock.corner = v;
        }
        if let Some(v) = d.offset {
            base.dock.offset = v;
        }
        if let Some(v) = d.inside {
            base.dock.inside = v;
        }
        if let Some(v) = d.follow_focus {
            base.dock.follow_focus = v;
        }
        if let Some(v) = d.allow {
            base.dock.allow = v;
        }
        if let Some(v) = d.show_accessibility_hint {
            base.dock.show_accessibility_hint = v;
        }
    }
    if let Some(s) = session {
        if let Some(v) = s.terminal {
            base.session.terminal = v;
        }
        if let Some(v) = s.cwd {
            base.session.cwd = v;
        }
    }
}

/// Mutate, validate, persist, then notify. Safe from any thread: no window
/// work happens here, only under `on_changed` (which must stay thread-safe too).
pub fn update(app: &AppHandle, f: impl FnOnce(&mut Settings)) -> Result<Settings, String> {
    let state: State<Mutex<Settings>> = app.state();
    let (old, new) = {
        let mut guard = state
            .lock()
            .map_err(|_| "settings lock poisoned".to_string())?;
        let old = guard.clone();
        let mut next = old.clone();
        f(&mut next);
        let next = validate(next);
        save(app, &next)?;
        *guard = next.clone();
        (old, next)
    };
    on_changed(app, &old, &new);
    let _ = app.emit("settings-changed", &new);
    Ok(new)
}

/// Which side effects a settings change needs (M6.3).
#[derive(Debug, PartialEq, Eq, Default)]
struct Effects {
    style: bool,
    autostart: bool,
    placement: bool,
}

fn diff(old: &Settings, new: &Settings) -> Effects {
    Effects {
        style: old.click_through != new.click_through || old.always_on_top != new.always_on_top,
        autostart: old.autostart != new.autostart,
        placement: old.dock.corner != new.dock.corner
            || old.dock.offset != new.dock.offset
            || old.dock.inside != new.dock.inside,
    }
}

/// Side-effect hook for fields whose change must reach the window or the OS.
/// Applies only what changed. Runs on whichever thread called `update`, so no
/// window work happens inline: the callees hop to the main thread themselves.
pub fn on_changed(app: &AppHandle, old: &Settings, new: &Settings) {
    let e = diff(old, new);
    if e.style {
        crate::apply_style_from_settings(app);
        crate::tray::sync_click_through(app, new.click_through);
    }
    if e.autostart {
        crate::apply_autostart(app, new.autostart);
    }
    if e.placement {
        crate::dock::replace_last(app);
    }
}

#[tauri::command]
pub fn get_settings(state: State<Mutex<Settings>>) -> Settings {
    state.lock().map(|s| s.clone()).unwrap_or_default()
}

#[tauri::command]
pub async fn set_settings(app: AppHandle, patch: SettingsPatch) -> Result<Settings, String> {
    update(&app, |s| merge(s, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_roundtrips_json() {
        let s = Settings::default();
        let json = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
        assert!(!s.dock.allow.is_empty());
    }

    #[test]
    fn the_allow_default_speaks_this_platforms_vocabulary() {
        let allow = default_allow();
        #[cfg(target_os = "macos")]
        {
            // Bundle ids, never a class/exe pair.
            assert!(allow
                .iter()
                .all(|a| a.exe.is_none() && a.class.contains('.')));
            assert!(allow.iter().any(|a| a.class == "com.apple.Terminal"));
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert_eq!(allow.len(), 6);
            assert!(allow.iter().any(|a| a.exe.as_deref() == Some("Code.exe")));
        }
    }

    /// A file carried between machines must load and stay inert, never fail.
    #[test]
    fn a_settings_file_from_the_other_platform_loads_without_matching_anything() {
        let raw = r#"{"version":1,"dock":{"enabled":true,
            "remembered":"macos:com.apple.Terminal",
            "allow":[{"class":"com.apple.Terminal"},
                     {"class":"CASCADIA_HOSTING_WINDOW_CLASS","exe":null}]}}"#;
        let s = validate(serde_json::from_str::<Settings>(raw).unwrap());
        assert_eq!(s.dock.allow.len(), 2);
        assert_eq!(
            s.dock.remembered.as_deref(),
            Some("macos:com.apple.Terminal")
        );
        // A file predating the hint still shows it.
        assert!(s.dock.show_accessibility_hint);
    }

    #[test]
    fn older_file_without_dock_loads_defaults() {
        let s: Settings = serde_json::from_str(r#"{"version":1,"opacity":0.5}"#).unwrap();
        assert_eq!(s.opacity, 0.5);
        assert_eq!(s.dock, Dock::default());
        assert_eq!(s.thresholds, Thresholds::default());
        assert!(s.show_scoped);
    }

    #[test]
    fn validate_clamps_opacity_and_thresholds() {
        let v = validate(Settings {
            opacity: 3.0,
            thresholds: Thresholds { warn: 0, crit: 200 },
            ..Settings::default()
        });
        assert_eq!(v.opacity, 1.0);
        assert_eq!(v.thresholds, Thresholds { warn: 1, crit: 100 });

        let low = validate(Settings {
            opacity: 0.0,
            ..Settings::default()
        });
        assert_eq!(low.opacity, 0.2);

        let inverted = Settings {
            thresholds: Thresholds { warn: 95, crit: 50 },
            ..Settings::default()
        };
        assert_eq!(validate(inverted).thresholds, Thresholds::default());
    }

    #[test]
    fn session_defaults_to_the_platform_terminal_and_home() {
        let s = Settings::default();
        assert!(s.session.terminal.is_empty());
        assert!(s.session.cwd.is_empty());
        // An older file predating the switcher still loads.
        let old: Settings = serde_json::from_str(r#"{"version":1,"compact":true}"#).unwrap();
        assert_eq!(old.session, Session::default());
    }

    #[test]
    fn validate_drops_blank_terminal_args() {
        let v = validate(Settings {
            session: Session {
                terminal: vec!["wt.exe".into(), "  ".into(), String::new(), "-w".into()],
                cwd: "  D:/src  ".into(),
            },
            ..Settings::default()
        });
        assert_eq!(v.session.terminal, vec!["wt.exe", "-w"]);
        assert_eq!(v.session.cwd, "D:/src");
    }

    #[test]
    fn merge_applies_session_fields_independently() {
        let mut base = Settings::default();
        base.session.cwd = "D:/keep".into();
        merge(
            &mut base,
            SettingsPatch {
                session: Some(SessionPatch {
                    terminal: Some(vec!["wt.exe".into()]),
                    ..SessionPatch::default()
                }),
                ..SettingsPatch::default()
            },
        );
        assert_eq!(base.session.terminal, vec!["wt.exe"]);
        assert_eq!(base.session.cwd, "D:/keep");
    }

    #[test]
    fn enabling_docking_turns_always_on_top_off() {
        let mut s = Settings::default();
        s.always_on_top = true;
        s.dock.enabled = true;
        assert!(!validate(s).always_on_top);

        let mut undocked = Settings::default();
        undocked.always_on_top = true;
        assert!(validate(undocked).always_on_top);
    }

    #[test]
    fn v1_follow_focus_is_cleared_and_the_version_stamped() {
        // v1 true meant "hide when unfocused"; the meaning changed, so migrate.
        let raw = r#"{"version":1,"dock":{"enabled":true,"follow_focus":true}}"#;
        let s = validate(serde_json::from_str::<Settings>(raw).unwrap());
        assert!(!s.dock.follow_focus);
        assert_eq!(s.version, SCHEMA_VERSION);

        // A current file keeps an explicit follow_focus untouched.
        let raw2 = r#"{"version":2,"dock":{"enabled":true,"follow_focus":true}}"#;
        let s2 = validate(serde_json::from_str::<Settings>(raw2).unwrap());
        assert!(s2.dock.follow_focus);
    }

    #[test]
    fn validate_drops_bad_colors() {
        let mut s = Settings::default();
        s.colors.insert("ok".into(), "#6ea8fe".into());
        s.colors.insert("upper".into(), "#ABCDEF".into());
        s.colors.insert("short".into(), "#fff".into());
        s.colors.insert("nohash".into(), "6ea8fe".into());
        s.colors.insert("nonhex".into(), "#gggggg".into());
        let v = validate(s);
        assert_eq!(
            v.colors.keys().cloned().collect::<Vec<_>>(),
            vec!["ok", "upper"]
        );
    }

    #[test]
    fn corner_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&Corner::TopRight).unwrap(),
            "\"top_right\""
        );
        let c: Corner = serde_json::from_str("\"bottom_left\"").unwrap();
        assert_eq!(c, Corner::BottomLeft);
    }

    #[test]
    fn merge_applies_only_some_fields() {
        let mut base = Settings::default();
        base.colors.insert("a".into(), "#112233".into());
        let expected = Settings {
            opacity: 0.4,
            ..base.clone()
        };
        merge(
            &mut base,
            SettingsPatch {
                opacity: Some(0.4),
                ..SettingsPatch::default()
            },
        );
        assert_eq!(base, expected);
    }

    #[test]
    fn merge_never_touches_remembered() {
        let mut base = Settings::default();
        base.dock.remembered = Some("hwnd:1234".into());
        merge(
            &mut base,
            SettingsPatch {
                dock: Some(DockPatch {
                    enabled: Some(true),
                    ..DockPatch::default()
                }),
                ..SettingsPatch::default()
            },
        );
        assert!(base.dock.enabled);
        assert_eq!(base.dock.remembered.as_deref(), Some("hwnd:1234"));
    }

    #[test]
    fn diff_flags_only_changed_fields() {
        let base = Settings::default();
        assert_eq!(diff(&base, &base), Effects::default());

        let mut ct = base.clone();
        ct.click_through = true;
        assert_eq!(
            diff(&base, &ct),
            Effects {
                style: true,
                ..Effects::default()
            }
        );

        let mut top = base.clone();
        top.always_on_top = true;
        assert_eq!(
            diff(&base, &top),
            Effects {
                style: true,
                ..Effects::default()
            }
        );

        let mut auto = base.clone();
        auto.autostart = true;
        assert_eq!(
            diff(&base, &auto),
            Effects {
                autostart: true,
                ..Effects::default()
            }
        );

        for placed in [
            Settings {
                dock: Dock {
                    offset: Offset { x: 1, y: 2 },
                    ..base.dock.clone()
                },
                ..base.clone()
            },
            Settings {
                dock: Dock {
                    corner: Corner::BottomLeft,
                    ..base.dock.clone()
                },
                ..base.clone()
            },
            Settings {
                dock: Dock {
                    inside: true,
                    ..base.dock.clone()
                },
                ..base.clone()
            },
        ] {
            assert_eq!(
                diff(&base, &placed),
                Effects {
                    placement: true,
                    ..Effects::default()
                }
            );
        }

        // Fields with no OS side effect (opacity, enabled, remembered) are silent.
        let mut quiet = base.clone();
        quiet.opacity = 0.5;
        quiet.dock.enabled = true;
        quiet.dock.remembered = Some("win32:x|y".into());
        assert_eq!(diff(&base, &quiet), Effects::default());
    }

    #[test]
    fn patch_ignores_unknown_remembered_key() {
        let patch: SettingsPatch =
            serde_json::from_str(r#"{"dock":{"remembered":"hwnd:999","corner":"bottom_right"}}"#)
                .unwrap();
        let mut base = Settings::default();
        base.dock.remembered = Some("hwnd:1".into());
        merge(&mut base, patch);
        assert_eq!(base.dock.corner, Corner::BottomRight);
        assert_eq!(base.dock.remembered.as_deref(), Some("hwnd:1"));
    }
}
