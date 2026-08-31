//! Window enumeration and identity. Every fn takes and returns `isize` hwnds
//! and wraps its Win32 calls; nothing here panics on a bad handle (plan §6).

use std::ffi::c_void;

use windows::core::{BOOL, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetAncestor, GetClassNameW, GetForegroundWindow, GetWindowLongPtrW,
    GetWindowTextW, GetWindowThreadProcessId, IsWindow, IsWindowVisible, GA_ROOT, GWL_EXSTYLE,
    WS_EX_TOOLWINDOW,
};

use crate::geometry::Candidate;

pub(crate) fn hwnd(h: isize) -> HWND {
    HWND(h as *mut c_void)
}

/// Visible, un-cloaked root windows without `WS_EX_TOOLWINDOW`, top of Z order first.
pub fn candidates() -> Vec<Candidate> {
    top_level_windows()
        .into_iter()
        .filter(|&h| is_visible(h) && !is_cloaked(h) && root_of(h) == h && !is_tool_window(h))
        .filter_map(describe)
        .collect()
}

/// `None` when the window is gone or has no class (an empty class would make
/// `target_id` unparseable).
pub fn describe(hwnd: isize) -> Option<Candidate> {
    if !is_alive(hwnd) {
        return None;
    }
    let class = class_of(hwnd)?;
    let (pid, _) = pid_tid(hwnd);
    Some(Candidate {
        hwnd,
        class,
        exe: exe_basename(pid),
        title: title_of(hwnd),
    })
}

/// Lowercased image basename, `None` when the process cannot be opened
/// (elevated or protected processes).
pub fn exe_basename(pid: u32) -> Option<String> {
    // SAFETY: plain handle-returning call; the handle is closed below on every path.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` outlives the call and `len` carries its capacity in chars.
    let queried = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    };
    // SAFETY: `handle` came from OpenProcess above and is closed exactly once.
    let _ = unsafe { CloseHandle(handle) };
    queried.ok()?;
    let path = String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]);
    let base = path.rsplit(['\\', '/']).next().unwrap_or("");
    if base.is_empty() {
        None
    } else {
        Some(base.to_ascii_lowercase())
    }
}

pub fn foreground() -> Option<isize> {
    // SAFETY: no arguments; a null return means no foreground window.
    let h = unsafe { GetForegroundWindow() };
    (!h.is_invalid()).then_some(h.0 as isize)
}

/// Top-level ancestor; the input itself when it is already a root or invalid.
pub fn root_of(h: isize) -> isize {
    // SAFETY: GetAncestor tolerates invalid handles and returns null for them.
    let root = unsafe { GetAncestor(hwnd(h), GA_ROOT) };
    if root.is_invalid() {
        h
    } else {
        root.0 as isize
    }
}

/// `(pid, tid)`; both zero for a dead window.
pub fn pid_tid(h: isize) -> (u32, u32) {
    let mut pid = 0u32;
    // SAFETY: `pid` outlives the call; the fn returns 0 rather than failing.
    let tid = unsafe { GetWindowThreadProcessId(hwnd(h), Some(&mut pid)) };
    (pid, tid)
}

pub fn is_alive(h: isize) -> bool {
    // SAFETY: IsWindow is defined for any handle value.
    unsafe { IsWindow(Some(hwnd(h))) }.as_bool()
}

/// `DWMWA_CLOAKED != 0` — hidden UWP hosts and other-desktop windows enumerate
/// as visible. A DWM error reads as "not cloaked" so a target is never dropped
/// on a transient failure.
pub fn is_cloaked(h: isize) -> bool {
    let mut cloaked = 0u32;
    // SAFETY: the out pointer and its size describe the local `u32`.
    let res = unsafe {
        DwmGetWindowAttribute(
            hwnd(h),
            DWMWA_CLOAKED,
            (&mut cloaked as *mut u32).cast::<c_void>(),
            std::mem::size_of::<u32>() as u32,
        )
    };
    res.is_ok() && cloaked != 0
}

/// Class name only; cheap enough for the pick guard.
pub fn class_of(h: isize) -> Option<String> {
    let mut buf = [0u16; 256];
    // SAFETY: the slice carries its own length; the return is the chars written.
    let n = unsafe { GetClassNameW(hwnd(h), &mut buf) };
    if n <= 0 {
        return None;
    }
    Some(String::from_utf16_lossy(
        &buf[..(n as usize).min(buf.len())],
    ))
}

pub fn own_pid() -> u32 {
    // SAFETY: no arguments, cannot fail.
    unsafe { GetCurrentProcessId() }
}

fn title_of(h: isize) -> String {
    let mut buf = [0u16; 512];
    // SAFETY: the slice carries its own length; the return is the chars written.
    let n = unsafe { GetWindowTextW(hwnd(h), &mut buf) };
    if n <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..(n as usize).min(buf.len())])
}

fn is_visible(h: isize) -> bool {
    // SAFETY: defined for any handle value.
    unsafe { IsWindowVisible(hwnd(h)) }.as_bool()
}

fn is_tool_window(h: isize) -> bool {
    // SAFETY: returns 0 for an invalid window, which reads as "no style bits".
    let ex = unsafe { GetWindowLongPtrW(hwnd(h), GWL_EXSTYLE) };
    ex as u32 & WS_EX_TOOLWINDOW.0 != 0
}

fn top_level_windows() -> Vec<isize> {
    let mut out: Vec<isize> = Vec::new();
    // SAFETY: `out` outlives the synchronous enumeration that receives it via LPARAM.
    let _ = unsafe {
        EnumWindows(
            Some(push_hwnd),
            LPARAM(&mut out as *mut Vec<isize> as isize),
        )
    };
    out
}

/// Runs inside EnumWindows on this thread; must never unwind across the FFI edge.
unsafe extern "system" fn push_hwnd(h: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: `lparam` is the `&mut Vec<isize>` `top_level_windows` is still borrowing.
    let out = unsafe { &mut *(lparam.0 as *mut Vec<isize>) };
    if out.len() < 4096 {
        out.push(h.0 as isize);
    }
    true.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exe_basename_of_own_process_is_the_test_binary() {
        let exe = exe_basename(own_pid()).expect("own process is always openable");
        let expected = std::env::current_exe()
            .ok()
            .and_then(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().to_ascii_lowercase())
            })
            .expect("test binary has a file name");
        assert_eq!(exe, expected);
    }

    #[test]
    fn exe_basename_of_nonexistent_pid_is_none() {
        assert_eq!(exe_basename(u32::MAX), None);
    }

    #[test]
    fn foreground_does_not_panic() {
        let _ = foreground();
    }

    #[test]
    fn bogus_hwnd_is_dead_and_its_own_root() {
        let bogus = 0x7fff_0001isize;
        assert!(!is_alive(bogus));
        assert_eq!(root_of(bogus), bogus);
        assert_eq!(describe(bogus), None);
        assert_eq!(pid_tid(bogus), (0, 0));
        assert!(!is_cloaked(bogus));
        assert_eq!(class_of(bogus), None);
    }

    #[test]
    fn candidates_never_have_an_empty_class() {
        for c in candidates() {
            assert!(!c.class.is_empty(), "hwnd {} has no class", c.hwnd);
        }
    }

    /// Spawns a console of our own so the user's terminal is never touched.
    /// The window class depends on the default terminal app (conhost vs
    /// Windows Terminal), so a WT window that appears after the spawn counts too.
    #[test]
    #[ignore = "opens a console window"]
    fn candidates_include_a_console_we_spawn() {
        use std::os::windows::process::CommandExt;
        use std::time::{Duration, Instant};

        const CREATE_NEW_CONSOLE: u32 = 0x10;
        let before: Vec<isize> = candidates().into_iter().map(|c| c.hwnd).collect();
        // Give the child its own stdio: inheriting a captured/piped parent
        // handle alongside CREATE_NEW_CONSOLE can suppress the new console
        // window entirely on some hosts.
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/c", "pause"])
            .creation_flags(CREATE_NEW_CONSOLE)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn cmd.exe");

        // Other consoles may already exist (a killed cmd leaves none, but the
        // test runner's own may be around), so only a window new since the
        // snapshot counts. Class varies with the machine's default console
        // host: classic conhost is "ConsoleWindowClass", Windows Terminal is
        // "CASCADIA_HOSTING_WINDOW_CLASS", and newer conhost builds put the
        // top-level window on the child process itself as "PseudoConsoleWindow"
        // (observed live on this machine) — so also accept any new window
        // whose owning process is our spawned cmd.exe.
        let is_ours = |c: &Candidate| {
            !before.contains(&c.hwnd)
                && (matches!(
                    c.class.as_str(),
                    "ConsoleWindowClass" | "CASCADIA_HOSTING_WINDOW_CLASS" | "PseudoConsoleWindow"
                ) || c.exe.as_deref() == Some("cmd.exe"))
        };
        // Windows Terminal (when it is the default host) shows a placeholder
        // window first, and its cold start can take close to 2 s on some
        // machines, so give this a wide budget before declaring failure.
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut outcome = Err("no console window appeared within 8 s".to_string());
        while outcome.is_err() && Instant::now() < deadline {
            if let Some(c) = candidates().into_iter().find(is_ours) {
                outcome = match super::super::bounds::read(c.hwnd) {
                    super::super::bounds::Read::Bounds(b)
                        if b.w > 0 && b.h > 0 && b.scale >= 1.0 =>
                    {
                        Ok(c.hwnd)
                    }
                    other => Err(format!("{c:?} read as {other:?}")),
                };
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let _ = child.kill();
        let _ = child.wait();
        let hwnd = outcome.unwrap_or_else(|e| panic!("{e}"));

        // The host may swap windows while shutting down, so check for any
        // fresh console window, not only the one we read.
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut leftover = Some(hwnd);
        while leftover.is_some() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
            leftover = is_alive(hwnd)
                .then_some(hwnd)
                .or_else(|| candidates().iter().find(|c| is_ours(c)).map(|c| c.hwnd));
        }
        assert_eq!(leftover, None, "a console window survived the child");
    }
}
