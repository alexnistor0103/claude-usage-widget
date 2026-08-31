//! Window docking. The overlay follows a chosen top-level window. Tabs and
//! panes are not separate windows — do not build tab detection (plan §6).

pub mod geometry;
// Not gated: its `info` submodule is pure and its tests run on every platform.
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

use std::sync::mpsc::Receiver;

/// Work areas and other plain rectangles; no DPI attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Physical pixels, virtual-screen origin; `scale` = target DPI / 96.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub scale: f64,
    /// Set when the geometry came from a fallback (e.g. `GetWindowRect`).
    pub approximate: bool,
}

/// "win32:<class>|<exe basename lowercase>" or "macos:<bundle id>" — never a
/// live window handle, which is not stable across a restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetId(pub String);

/// What a candidate window must look like; `exe` is required for generic Win32
/// classes and unused on macOS, where the bundle id names the application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetSpec {
    pub class: String,
    pub exe: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TrackerConfig {
    pub allow: Vec<TargetSpec>,
    pub remembered: Option<TargetId>,
    /// Re-attach to whichever allowed window becomes foreground.
    pub follow_focus: bool,
}

/// NotFound after an attach WITH a spec means the tracker is searching (plan §6);
/// after a pick timeout or a spec-less attach it is idle.
#[derive(Debug, Clone, PartialEq)]
pub enum TrackerEvent {
    Attached(TargetId),
    Bounds(Bounds),
    Minimized,
    Restored,
    Focused(bool),
    Lost,
    NotFound,
}

/// Commands only queue work for the tracker thread; results arrive as events.
pub trait TrackerHandle: Send {
    /// `None` = best allowed candidate.
    fn attach(&self, id: Option<TargetId>) -> anyhow::Result<()>;
    fn pick_interactively(&self) -> anyhow::Result<()>;
    fn detach(&self) -> anyhow::Result<()>;
    fn stop(self);
}

/// Per-platform tracker entry point (plan §6).
pub trait WindowTracker: Sized {
    type Handle: TrackerHandle;
    fn start(cfg: TrackerConfig) -> anyhow::Result<(Self::Handle, Receiver<TrackerEvent>)>;
}
