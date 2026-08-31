//! macOS launcher: `open -a Terminal` on a per-launch `.command` wrapper.
//!
//! `open` goes through LaunchServices, which does not inherit the caller's
//! environment — hence the shim, which fetches its own token — and does not
//! forward arguments to a document, so the nonce, port and directory reach
//! `session-shim.sh` through a wrapper written for this one launch
//! (SWITCHER §5).
//!
//! `Terminal.app` is deliberately the default for the same reason `wt.exe` is
//! not one on Windows: anything nicer is a `settings.session.terminal`
//! override, and the shim fetches its own token, so a terminal that drops the
//! environment still works.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use crate::{plan, shim, LaunchError, LaunchRequest, SessionLauncher};

/// Suffix `open` needs for Terminal.app to be the handler.
const WRAPPER_EXT: &str = "command";

/// A wrapper whose terminal never opened is dead weight — the nonce it carries
/// expires in 30 s regardless. Swept generously so a slow LaunchServices
/// hand-off can never race the broom.
const WRAPPER_TTL: Duration = Duration::from_secs(300);

pub struct MacosLauncher {
    data_dir: PathBuf,
}

impl MacosLauncher {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
        }
    }
}

impl SessionLauncher for MacosLauncher {
    fn launch(&self, req: LaunchRequest) -> Result<(), LaunchError> {
        // Before anything touches disk, so a malformed request costs no disk
        // work and no nonce.
        plan::validate(&req)?;
        let shims = shim::ensure(&self.data_dir)?;
        sweep(&shims.dir);
        let wrapper = write_wrapper(&shims.dir, &shims.posix, &req)?;
        let argv = plan::macos_argv(&req, &wrapper, &shims.posix)?;
        // A wrapper nothing will ever open still names a nonce: take it back on
        // the failure path rather than leaving it for the sweep.
        spawn(&argv, &req.cwd).inspect_err(|_| {
            let _ = std::fs::remove_file(&wrapper);
        })
    }
}

/// Write the one-shot wrapper this launch opens.
///
/// Not named after the nonce: Terminal titles its window after the document it
/// was given, and the nonce is a redemption code. `0700` because the file names
/// the nonce inside, and the data dir is not necessarily private.
fn write_wrapper(dir: &Path, shim_sh: &Path, req: &LaunchRequest) -> Result<PathBuf, LaunchError> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let shim_str = shim_sh
        .to_str()
        .ok_or_else(|| LaunchError::Shim(format!("{}: not utf-8", shim_sh.display())))?;
    let cwd = req
        .cwd
        .to_str()
        .ok_or_else(|| LaunchError::BadCwd(req.cwd.clone()))?;
    let body = plan::wrapper_body(shim_str, &req.nonce, req.port, cwd);

    // `create_new` and a mode set at creation, never a chmod after: an existing
    // file keeps its own permissions and would take the nonce at whatever they
    // are. A leftover from a recycled pid just moves the counter on.
    for _ in 0..8 {
        let path = dir.join(format!(
            "claude-{}-{}.{WRAPPER_EXT}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(body.as_bytes())
                    .map_err(|e| wrapper_err(&path, e))?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(wrapper_err(&path, e)),
        }
    }
    Err(LaunchError::Shim(format!(
        "{}: no free wrapper name",
        dir.display()
    )))
}

/// Drop wrappers a terminal never picked up. Best effort: a launch is not worth
/// failing over a sweep, and a wrapper left behind is inert once its nonce has
/// expired.
fn sweep(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(WRAPPER_EXT) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > WRAPPER_TTL);
        if stale {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Start the terminal and let go of it. The session is the user's, not the
/// daemon's: it outlives a widget restart and is never waited on for its exit
/// status.
///
/// `std::process::Command` is fine here, unlike on Windows: `open` hands off to
/// LaunchServices, so the terminal is not our child and inherits none of our
/// handles. The stdio is still nulled and the child still `setsid`s, because an
/// override *can* name a terminal that is our child — and that one would
/// otherwise inherit the daemon's log as its stdout and the daemon's controlling
/// terminal as its own (the M7.1 finding, in its macOS form).
fn spawn(argv: &[String], cwd: &Path) -> Result<(), LaunchError> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| LaunchError::Spawn("empty terminal command".into()))?;

    let mut cmd = std::process::Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_clear()
        .envs(plan::child_env(std::env::vars_os()));

    // SAFETY: `setsid` is async-signal-safe, and the child is fresh from `fork`
    // so it cannot already lead a process group.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| LaunchError::Spawn(format!("{program}: {e}")))?;
    let pid = child.id();
    // `open` exits as soon as LaunchServices has the document; nobody waits on
    // the daemon's behalf, so reap it here or it stays a zombie for the life of
    // the process.
    std::thread::spawn(move || {
        let _ = child.wait();
    });

    // The nonce is in the wrapper the argv names; only the pid is ever logged.
    tracing::info!(pid, "session terminal started");
    Ok(())
}

fn wrapper_err(path: &Path, e: std::io::Error) -> LaunchError {
    LaunchError::Shim(format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cuw-launch-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn an_empty_command_is_a_spawn_error_not_a_panic() {
        let e = spawn(&[], Path::new(".")).expect_err("empty");
        assert!(matches!(e, LaunchError::Spawn(_)));
    }

    #[test]
    fn a_malformed_request_never_writes_a_wrapper() {
        let root = temp_root("guard");
        let e = MacosLauncher::new(&root)
            .launch(LaunchRequest::new("short", 8787, std::env::temp_dir()))
            .expect_err("bad nonce");
        assert!(matches!(e, LaunchError::BadNonce));
        assert!(!root.exists(), "the shim dir was created for a bad request");
    }

    #[test]
    fn each_wrapper_gets_its_own_name_and_is_private() {
        let root = temp_root("wrapper");
        let shims = shim::ensure(&root).expect("ensure");
        let req = LaunchRequest::new("nonce-abcd1234", 8787, std::env::temp_dir());
        let first = write_wrapper(&shims.dir, &shims.posix, &req).expect("first");
        let second = write_wrapper(&shims.dir, &shims.posix, &req).expect("second");
        assert_ne!(first, second);
        for path in [&first, &second] {
            let name = path.file_name().and_then(|n| n.to_str()).expect("name");
            assert!(name.starts_with("claude-"), "{name}");
            assert!(name.ends_with(".command"), "{name}");
            // Terminal titles the window after the document.
            assert!(!name.contains("nonce-abcd1234"), "{name}");
            let mode = std::fs::metadata(path).expect("stat").permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "{name}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_sweep_takes_stale_wrappers_and_leaves_the_shims_alone() {
        let root = temp_root("sweep");
        let shims = shim::ensure(&root).expect("ensure");
        let req = LaunchRequest::new("nonce-abcd1234", 8787, std::env::temp_dir());
        let fresh = write_wrapper(&shims.dir, &shims.posix, &req).expect("wrapper");
        let stale = shims.dir.join("claude-1-0.command");
        std::fs::write(&stale, "#!/bin/sh\n").expect("stale");
        let old = SystemTime::now() - WRAPPER_TTL - Duration::from_secs(60);
        std::fs::File::open(&stale)
            .and_then(|f| f.set_modified(old))
            .expect("backdate");

        sweep(&shims.dir);
        assert!(!stale.exists(), "a stale wrapper survived the sweep");
        assert!(fresh.exists(), "a fresh wrapper was swept");
        assert!(shims.posix.exists(), "the shim itself was swept");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Opens a real Terminal window, so it never runs in a normal `cargo test`.
    /// The window should report a refused session code and **stay open** at a
    /// usable prompt, and the `.command` file must be gone from
    /// `<data dir>/shim` by the time it does.
    #[test]
    #[ignore]
    fn manual_launch_shows_the_refusal_in_a_new_terminal() {
        let root = std::env::temp_dir().join("cuw-launch-manual");
        std::fs::create_dir_all(&root).expect("root");
        // No daemon on this port: the redemption must fail, not hang.
        let req = LaunchRequest::new("nonce-manual-0001", 59_999, std::env::temp_dir());
        MacosLauncher::new(&root).launch(req).expect("launch");
    }
}
