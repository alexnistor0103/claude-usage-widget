//! Windows launcher: PowerShell on the generated shim, in a console of its own.
//!
//! `wt.exe` is deliberately not the default — it hands off to an existing
//! `WindowsTerminal.exe`, so the spawned process is not the one the user ends up
//! typing in (SWITCHER §5, Q2). Anyone who wants it can set it as a
//! `settings.session.terminal` prefix; the shim fetches its own token, so a
//! terminal that drops the environment still works.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Threading::{
    CreateProcessW, CREATE_NEW_CONSOLE, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION,
    STARTUPINFOW,
};

use crate::{plan, shim, LaunchError, LaunchRequest, SessionLauncher};

pub struct WindowsLauncher {
    data_dir: PathBuf,
}

impl WindowsLauncher {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
        }
    }
}

impl SessionLauncher for WindowsLauncher {
    fn launch(&self, req: LaunchRequest) -> Result<(), LaunchError> {
        // Before the shim is touched, so a malformed request costs no disk work
        // and no nonce.
        plan::validate(&req)?;
        let shims = shim::ensure(&self.data_dir)?;
        let argv = plan::windows_argv(&req, &shims.powershell)?;
        spawn(&argv, &req.cwd)
    }
}

/// Start the terminal and let go of it. The session is the user's, not the
/// daemon's: it outlives a widget restart and is never waited on.
///
/// `CreateProcessW` rather than `std::process::Command`, because `Command`
/// always sets `STARTF_USESTDHANDLES` and hands the child the daemon's own
/// stdio. The daemon's stdout is its log file and its stdin is `NUL`
/// (`apps/overlay/src-tauri/src/lib.rs`), so an inherited spawn would put the
/// session's output in the widget's log and leave the new console dead to type
/// in. With no inherited handles the child gets the handles of the console
/// `CREATE_NEW_CONSOLE` just gave it — which is the whole point of the window.
fn spawn(argv: &[String], cwd: &Path) -> Result<(), LaunchError> {
    let program = argv
        .first()
        .ok_or_else(|| LaunchError::Spawn("empty terminal command".into()))?
        .clone();

    // Mutable: CreateProcessW may write into the command line buffer.
    let mut cmdline = wide(&plan::quote_argv(argv));
    let cwd = wide(&cwd.to_string_lossy());
    let mut env = env_block();

    let si = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut pi = PROCESS_INFORMATION::default();

    // SAFETY: every pointer is to a live local that outlives the call, and the
    // command line and environment are NUL-terminated wide buffers.
    let started = unsafe {
        CreateProcessW(
            PCWSTR::null(),
            Some(PWSTR(cmdline.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NEW_CONSOLE | CREATE_UNICODE_ENVIRONMENT,
            Some(env.as_mut_ptr() as *const c_void),
            PCWSTR(cwd.as_ptr()),
            &si,
            &mut pi,
        )
    };
    started.map_err(|e| LaunchError::Spawn(format!("{program}: {e}")))?;

    // The session is detached; only the handles are ours to release.
    // SAFETY: both handles come from the successful call above and are unused.
    unsafe {
        let _ = CloseHandle(pi.hThread);
        let _ = CloseHandle(pi.hProcess);
    }
    // The nonce is in the argv; only the pid is ever logged.
    tracing::info!(pid = pi.dwProcessId, "session terminal started");
    Ok(())
}

/// NUL-terminated UTF-16, as every `W` entry point wants it.
fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// `name=value\0…\0\0`, sorted, with the identity-binding variables removed.
fn env_block() -> Vec<u16> {
    let mut block: Vec<u16> = Vec::new();
    for (k, v) in plan::child_env(std::env::vars_os()) {
        // A `=C:=…` drive-current-directory entry starts with `=` and must be
        // passed through untouched; everything else joins normally.
        block.extend(k.encode_wide());
        block.push(u16::from(b'='));
        block.extend(v.encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_command_is_a_spawn_error_not_a_panic() {
        let e = spawn(&[], Path::new(".")).expect_err("empty");
        assert!(matches!(e, LaunchError::Spawn(_)));
    }

    #[test]
    fn a_malformed_request_never_writes_a_shim() {
        let root = std::env::temp_dir().join(format!("cuw-launch-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let launcher = WindowsLauncher::new(&root);
        let e = launcher
            .launch(LaunchRequest::new("short", 8787, std::env::temp_dir()))
            .expect_err("bad nonce");
        assert!(matches!(e, LaunchError::BadNonce));
        assert!(!root.exists(), "the shim dir was created for a bad request");
    }

    #[test]
    fn the_environment_block_is_double_nul_terminated() {
        let block = env_block();
        assert_eq!(&block[block.len() - 2..], &[0, 0]);
        let text = String::from_utf16_lossy(&block);
        for key in crate::SCRUBBED_ENV {
            assert!(
                !text.contains(&format!("{key}=")),
                "{key} reached the child"
            );
        }
    }

    /// Opens a real console window, so it never runs in a normal `cargo test`.
    /// The window should report a refused session code and **stay open** at a
    /// usable prompt — that last part is what proves the console, not the
    /// daemon's log, owns the child's stdio.
    #[test]
    #[ignore]
    fn manual_launch_shows_the_refusal_in_a_new_console() {
        let root = std::env::temp_dir().join("cuw-launch-manual");
        std::fs::create_dir_all(&root).expect("root");
        // No daemon on this port: the redemption must fail, not hang.
        let req = LaunchRequest::new("nonce-manual-0001", 59_999, std::env::temp_dir());
        WindowsLauncher::new(&root).launch(req).expect("launch");
    }
}
