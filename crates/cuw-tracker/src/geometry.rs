//! Pure placement math: no platform code, no I/O, unit tested headless (plan §6).

use crate::{Bounds, Rect, TargetId, TargetSpec};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// `dx`/`dy` are physical px: callers scale logical offsets by `Bounds::scale`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchor {
    pub corner: Corner,
    pub dx: i32,
    pub dy: i32,
    /// Inside the target at the corner, or adjacent outside aligned to that edge.
    pub inside: bool,
}

/// Top-left of an `overlay` (w, h) anchored to `target`.
pub fn overlay_origin(target: &Bounds, overlay: (i32, i32), anchor: &Anchor) -> (i32, i32) {
    let (ow, oh) = overlay;
    let (t, a) = (target, anchor);
    let left_inside = t.x + a.dx;
    let right_inside = t.x + t.w - ow - a.dx;
    let top = t.y + a.dy;
    let bottom = t.y + t.h - oh - a.dy;
    let left_outside = t.x - ow - a.dx;
    let right_outside = t.x + t.w + a.dx;

    match (a.corner, a.inside) {
        (Corner::TopLeft, true) => (left_inside, top),
        (Corner::TopRight, true) => (right_inside, top),
        (Corner::BottomLeft, true) => (left_inside, bottom),
        (Corner::BottomRight, true) => (right_inside, bottom),
        (Corner::TopLeft, false) => (left_outside, top),
        (Corner::TopRight, false) => (right_outside, top),
        (Corner::BottomLeft, false) => (left_outside, bottom),
        (Corner::BottomRight, false) => (right_outside, bottom),
    }
}

/// Shift the rect so it lies within `work`; if it cannot fit, pin its top-left.
pub fn clamp_to_work_area(origin: (i32, i32), size: (i32, i32), work: &Rect) -> (i32, i32) {
    let clamp_axis = |o: i32, s: i32, w0: i32, wl: i32| {
        if s > wl {
            w0
        } else {
            o.max(w0).min(w0 + wl - s)
        }
    };
    (
        clamp_axis(origin.0, size.0, work.x, work.w),
        clamp_axis(origin.1, size.1, work.y, work.h),
    )
}

/// True when the rect overlaps `area` at all.
pub fn intersects(origin: (i32, i32), size: (i32, i32), area: &Rect) -> bool {
    let (x, y) = origin;
    let (w, h) = size;
    x < area.x + area.w && x + w > area.x && y < area.y + area.h && y + h > area.y
}

/// A top-level window as enumerated by the platform layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// An `HWND` on Windows, a `CGWindowID` on macOS.
    pub hwnd: isize,
    /// The window class on Windows, the application's bundle id on macOS.
    pub class: String,
    pub exe: Option<String>,
    /// Empty when unreadable — macOS withholds titles without Screen Recording.
    pub title: String,
}

/// Exact class; exe compared case-insensitively only when the spec names one.
pub fn spec_matches(spec: &TargetSpec, class: &str, exe: Option<&str>) -> bool {
    if spec.class != class {
        return false;
    }
    match (&spec.exe, exe) {
        (None, _) => true,
        (Some(want), Some(have)) => want.eq_ignore_ascii_case(have),
        (Some(_), None) => false,
    }
}

/// Index of the best candidate: foreground match, then remembered, then Z-order top.
pub fn rank_candidates(
    cands: &[Candidate],
    allow: &[TargetSpec],
    foreground: Option<isize>,
    remembered: Option<&TargetId>,
) -> Option<usize> {
    let remembered = remembered.and_then(parse_target_id);
    let matches = |c: &Candidate, s: &TargetSpec| spec_matches(s, &c.class, c.exe.as_deref());
    let is_remembered = |c: &Candidate| remembered.as_ref().is_some_and(|s| matches(c, s));
    let allowed = |c: &Candidate| allow.iter().any(|s| matches(c, s)) || is_remembered(c);

    let allowed_idx: Vec<usize> = cands
        .iter()
        .enumerate()
        .filter(|(_, c)| allowed(c))
        .map(|(i, _)| i)
        .collect();

    allowed_idx
        .iter()
        .find(|&&i| foreground == Some(cands[i].hwnd))
        .or_else(|| allowed_idx.iter().find(|&&i| is_remembered(&cands[i])))
        .or_else(|| allowed_idx.first())
        .copied()
}

const ID_PREFIX: &str = "win32:";
const MACOS_ID_PREFIX: &str = "macos:";

pub fn target_id(class: &str, exe: Option<&str>) -> TargetId {
    TargetId(format!(
        "{ID_PREFIX}{class}|{}",
        exe.map(|e| e.to_ascii_lowercase()).unwrap_or_default()
    ))
}

/// "macos:<bundle id>". The bundle id alone identifies the application, so
/// there is no second field to disambiguate the way a Win32 class needs one.
pub fn macos_target_id(bundle_id: &str) -> TargetId {
    TargetId(format!("{MACOS_ID_PREFIX}{bundle_id}"))
}

/// Inverse of [`target_id`] and [`macos_target_id`]; anything not in one of
/// those shapes is `None`. Both prefixes parse on every platform so a settings
/// file written on one machine cannot panic the other.
pub fn parse_target_id(id: &TargetId) -> Option<TargetSpec> {
    if let Some(rest) = id.0.strip_prefix(ID_PREFIX) {
        let (class, exe) = rest.rsplit_once('|')?;
        if class.is_empty() {
            return None;
        }
        return Some(TargetSpec {
            class: class.to_string(),
            exe: (!exe.is_empty()).then(|| exe.to_string()),
        });
    }
    let bundle = id.0.strip_prefix(MACOS_ID_PREFIX)?;
    (!bundle.is_empty()).then(|| TargetSpec {
        class: bundle.to_string(),
        exe: None,
    })
}

/// Shell roots a pick must never resolve to.
pub const SHELL_CLASSES: [&str; 3] = ["Shell_TrayWnd", "Progman", "WorkerW"];

/// The macOS analogue: bundle ids a pick must never resolve to. The last entry
/// is the widget itself, which owns a window like any other application.
pub const MACOS_SHELL_BUNDLES: [&str; 7] = [
    "com.apple.dock",
    "com.apple.WindowManager",
    "com.apple.controlcenter",
    "com.apple.notificationcenterui",
    "com.apple.systemuiserver",
    "com.apple.Spotlight",
    "com.local.cuw",
];

/// Drops repeated identical bounds so a drag does not flood the consumer.
#[derive(Debug, Default)]
pub struct Coalescer {
    last: Option<Bounds>,
}

impl Coalescer {
    /// True when `b` differs from the last pushed bounds and should be emitted.
    pub fn push(&mut self, b: Bounds) -> bool {
        if self.last == Some(b) {
            return false;
        }
        self.last = Some(b);
        true
    }

    pub fn reset(&mut self) {
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(x: i32, y: i32, scale: f64) -> Bounds {
        Bounds {
            x,
            y,
            w: 800,
            h: 600,
            scale,
            approximate: false,
        }
    }

    fn anchor(corner: Corner, inside: bool, d: i32) -> Anchor {
        Anchor {
            corner,
            dx: d,
            dy: d,
            inside,
        }
    }

    fn spec(class: &str, exe: Option<&str>) -> TargetSpec {
        TargetSpec {
            class: class.to_string(),
            exe: exe.map(str::to_string),
        }
    }

    fn cand(hwnd: isize, class: &str, exe: Option<&str>) -> Candidate {
        Candidate {
            hwnd,
            class: class.to_string(),
            exe: exe.map(str::to_string),
            title: format!("window {hwnd}"),
        }
    }

    const OVERLAY: (i32, i32) = (300, 240);

    #[test]
    fn origin_inside_all_corners() {
        let t = target(100, 100, 1.0);
        let cases = [
            (Corner::TopLeft, (108, 108)),
            (Corner::TopRight, (592, 108)),
            (Corner::BottomLeft, (108, 452)),
            (Corner::BottomRight, (592, 452)),
        ];
        for (corner, want) in cases {
            assert_eq!(
                overlay_origin(&t, OVERLAY, &anchor(corner, true, 8)),
                want,
                "{corner:?}"
            );
        }
    }

    #[test]
    fn origin_outside_all_corners() {
        let t = target(100, 100, 1.0);
        let cases = [
            (Corner::TopLeft, (-208, 108)),
            (Corner::TopRight, (908, 108)),
            (Corner::BottomLeft, (-208, 452)),
            (Corner::BottomRight, (908, 452)),
        ];
        for (corner, want) in cases {
            assert_eq!(
                overlay_origin(&t, OVERLAY, &anchor(corner, false, 8)),
                want,
                "{corner:?}"
            );
        }
    }

    #[test]
    fn origin_with_prescaled_offsets() {
        // 8 logical px at 1.5 and 2.0 → 12 and 16 physical px, chosen by the caller.
        let t15 = target(100, 100, 1.5);
        assert_eq!(
            overlay_origin(&t15, OVERLAY, &anchor(Corner::TopRight, true, 12)),
            (588, 112)
        );
        assert_eq!(
            overlay_origin(&t15, OVERLAY, &anchor(Corner::BottomLeft, false, 12)),
            (-212, 448)
        );
        let t20 = target(100, 100, 2.0);
        assert_eq!(
            overlay_origin(&t20, OVERLAY, &anchor(Corner::BottomRight, true, 16)),
            (584, 444)
        );
        assert_eq!(
            overlay_origin(&t20, OVERLAY, &anchor(Corner::TopLeft, false, 16)),
            (-216, 116)
        );
    }

    #[test]
    fn origin_on_monitor_left_of_primary() {
        let t = target(-1920, 0, 1.0);
        assert_eq!(
            overlay_origin(&t, OVERLAY, &anchor(Corner::TopLeft, true, 8)),
            (-1912, 8)
        );
        assert_eq!(
            overlay_origin(&t, OVERLAY, &anchor(Corner::TopRight, true, 8)),
            (-1428, 8)
        );
        assert_eq!(
            overlay_origin(&t, OVERLAY, &anchor(Corner::TopRight, false, 8)),
            (-1112, 8)
        );
    }

    const WORK: Rect = Rect {
        x: 0,
        y: 0,
        w: 2560,
        h: 1392,
    };

    #[test]
    fn clamp_pulls_back_from_right_edge() {
        assert_eq!(clamp_to_work_area((2400, 100), OVERLAY, &WORK), (2260, 100));
        assert_eq!(clamp_to_work_area((100, 1300), OVERLAY, &WORK), (100, 1152));
        assert_eq!(clamp_to_work_area((-50, -20), OVERLAY, &WORK), (0, 0));
    }

    #[test]
    fn clamp_leaves_fitting_rect_alone() {
        assert_eq!(clamp_to_work_area((100, 100), OVERLAY, &WORK), (100, 100));
    }

    #[test]
    fn clamp_pins_oversized_rect_to_origin() {
        let work = Rect {
            x: 1920,
            y: 40,
            w: 200,
            h: 100,
        };
        assert_eq!(clamp_to_work_area((3000, 500), OVERLAY, &work), (1920, 40));
    }

    #[test]
    fn intersects_cases() {
        assert!(intersects((100, 100), OVERLAY, &WORK));
        assert!(intersects((-200, -200), OVERLAY, &WORK));
        assert!(!intersects((-300, 0), OVERLAY, &WORK));
        assert!(!intersects((2560, 0), OVERLAY, &WORK));
        assert!(!intersects((0, 1392), OVERLAY, &WORK));
    }

    #[test]
    fn spec_matches_rules() {
        let wt = spec("CASCADIA_HOSTING_WINDOW_CLASS", None);
        assert!(spec_matches(&wt, "CASCADIA_HOSTING_WINDOW_CLASS", None));
        assert!(spec_matches(
            &wt,
            "CASCADIA_HOSTING_WINDOW_CLASS",
            Some("anything.exe")
        ));
        assert!(!spec_matches(&wt, "cascadia_hosting_window_class", None));

        let code = spec("Chrome_WidgetWin_1", Some("code.exe"));
        assert!(spec_matches(&code, "Chrome_WidgetWin_1", Some("Code.exe")));
        assert!(!spec_matches(
            &code,
            "Chrome_WidgetWin_1",
            Some("chrome.exe")
        ));
        assert!(!spec_matches(&code, "Chrome_WidgetWin_1", None));
    }

    #[test]
    fn rank_prefers_foreground_then_remembered_then_first() {
        let allow = [spec("A", None), spec("B", None)];
        let cands = [cand(1, "A", None), cand(2, "B", None), cand(3, "A", None)];
        let remembered = target_id("B", None);

        assert_eq!(
            rank_candidates(&cands, &allow, Some(3), Some(&remembered)),
            Some(2)
        );
        assert_eq!(
            rank_candidates(&cands, &allow, None, Some(&remembered)),
            Some(1)
        );
        assert_eq!(rank_candidates(&cands, &allow, None, None), Some(0));
    }

    #[test]
    fn rank_ignores_non_allowed_foreground_and_empty_input() {
        let allow = [spec("A", None)];
        let cands = [cand(1, "Z", None), cand(2, "A", None)];
        assert_eq!(rank_candidates(&cands, &allow, Some(1), None), Some(1));
        assert_eq!(rank_candidates(&cands, &[], None, None), None);
        assert_eq!(rank_candidates(&[], &allow, Some(1), None), None);
    }

    #[test]
    fn rank_treats_remembered_as_allowed() {
        let cands = [cand(1, "Z", None), cand(2, "R", Some("term.exe"))];
        let remembered = target_id("R", Some("Term.exe"));
        assert_eq!(
            rank_candidates(&cands, &[], None, Some(&remembered)),
            Some(1)
        );
    }

    #[test]
    fn target_id_round_trip() {
        let id = target_id("Chrome_WidgetWin_1", Some("Code.exe"));
        assert_eq!(id.0, "win32:Chrome_WidgetWin_1|code.exe");
        assert_eq!(
            parse_target_id(&id),
            Some(spec("Chrome_WidgetWin_1", Some("code.exe")))
        );

        let bare = target_id("CASCADIA_HOSTING_WINDOW_CLASS", None);
        assert_eq!(bare.0, "win32:CASCADIA_HOSTING_WINDOW_CLASS|");
        assert_eq!(
            parse_target_id(&bare),
            Some(spec("CASCADIA_HOSTING_WINDOW_CLASS", None))
        );
    }

    #[test]
    fn parse_target_id_rejects_garbage() {
        for bad in [
            "",
            "win32:",
            "win32:|x.exe",
            "mac:Foo|",
            "macos:",
            "macosFoo",
            "Foo|bar.exe",
            "win32:NoSeparator",
        ] {
            assert_eq!(parse_target_id(&TargetId(bad.to_string())), None, "{bad:?}");
        }
    }

    #[test]
    fn macos_target_id_round_trip() {
        let id = macos_target_id("com.apple.Terminal");
        assert_eq!(id.0, "macos:com.apple.Terminal");
        assert_eq!(
            parse_target_id(&id),
            Some(spec("com.apple.Terminal", None)),
            "a bundle id carries no exe field"
        );
    }

    /// Both shapes parse on both platforms: a settings file copied between
    /// machines must degrade to `NotFound`, never to a panic.
    #[test]
    fn both_platform_prefixes_parse_everywhere() {
        assert_eq!(
            parse_target_id(&TargetId("win32:Chrome_WidgetWin_1|code.exe".to_string())),
            Some(spec("Chrome_WidgetWin_1", Some("code.exe")))
        );
        assert_eq!(
            parse_target_id(&TargetId("macos:com.googlecode.iterm2".to_string())),
            Some(spec("com.googlecode.iterm2", None))
        );
    }

    #[test]
    fn rank_matches_a_macos_candidate_on_its_bundle_id() {
        let cands = [
            cand(41, "com.apple.finder", Some("finder")),
            cand(42, "com.apple.Terminal", Some("terminal")),
        ];
        let remembered = macos_target_id("com.apple.Terminal");
        assert_eq!(
            rank_candidates(&cands, &[], None, Some(&remembered)),
            Some(1)
        );
        // The spec carries no exe, so the candidate's own is not consulted.
        assert_eq!(
            rank_candidates(&cands, &[spec("com.apple.Terminal", None)], None, None),
            Some(1)
        );
    }

    #[test]
    fn the_widget_itself_is_never_a_macos_pick() {
        assert!(MACOS_SHELL_BUNDLES.contains(&"com.local.cuw"));
        assert!(MACOS_SHELL_BUNDLES.contains(&"com.apple.dock"));
        assert!(!MACOS_SHELL_BUNDLES.contains(&"com.apple.Terminal"));
    }

    #[test]
    fn coalescer_emits_on_change_only() {
        let mut c = Coalescer::default();
        let b = target(0, 0, 1.0);
        assert!(c.push(b));
        assert!(!c.push(b));
        assert!(c.push(Bounds { scale: 1.5, ..b }));
        assert!(c.push(Bounds {
            approximate: true,
            scale: 1.5,
            ..b
        }));
        c.reset();
        assert!(c.push(Bounds {
            approximate: true,
            scale: 1.5,
            ..b
        }));
    }
}
