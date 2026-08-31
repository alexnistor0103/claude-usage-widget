//! Where the daemon comes up: the one place the data dir is resolved, and the
//! single-instance check that must pass before anything shared is touched.
//!
//! Two daemons sharing one data dir and one keyring is the suspected cause of
//! the accounts lost on 2026-08-31, so the listener binds first and an
//! address-in-use bind ends the process before a registry read, a keyring
//! access, a scratch sweep or a poll task can run.

use std::path::PathBuf;

use anyhow::Context;

/// Env override for the data dir. `directories::ProjectDirs` resolves through
/// `SHGetKnownFolderPath` on Windows, so setting `%APPDATA%` does **not** move
/// it — this is the only way to give a test daemon its own registry, bearer
/// file and scratch root.
const DATA_DIR_ENV: &str = "CUW_DATA_DIR";

/// The daemon's data dir, created if missing. Everything the daemon owns hangs
/// off this: `registry.toml`, `bearer.token`, `port`, `pid`, `scratch/`, the
/// launch shims (plan §4, §5).
pub fn data_dir() -> anyhow::Result<PathBuf> {
    let dir = match data_dir_override(std::env::var(DATA_DIR_ENV).ok().as_deref()) {
        Some(dir) => dir,
        None => directories::ProjectDirs::from("com", "local", "cuw")
            .context("could not resolve the data directory")?
            .data_dir()
            .to_path_buf(),
    };
    std::fs::create_dir_all(&dir).context("create data dir")?;
    restrict(&dir);
    Ok(dir)
}

/// Split out of [`data_dir`] so the override rule is testable without touching
/// a shared process environment. A blank value is treated as unset rather than
/// as "the current directory".
fn data_dir_override(raw: Option<&str>) -> Option<PathBuf> {
    match raw.map(str::trim) {
        Some(s) if !s.is_empty() => Some(PathBuf::from(s)),
        _ => None,
    }
}

/// The dir holds `bearer.token` and, mid-connect, a scratch credential, so keep
/// it to the owner on unix. Best-effort: a dir we cannot chmod is still usable,
/// and the bearer file sets its own `0600` regardless.
fn restrict(dir: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)) {
            tracing::warn!(error = %e, "could not restrict the data dir");
        }
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Whether a failed bind means another daemon already owns this port — and with
/// it the data dir and the keyring namespace this one was about to use.
pub fn port_taken(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::AddrInUse
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blank_data_dir_override_is_treated_as_unset() {
        assert_eq!(data_dir_override(None), None);
        assert_eq!(data_dir_override(Some("")), None);
        assert_eq!(data_dir_override(Some("  \t ")), None);
    }

    #[test]
    fn a_set_data_dir_override_wins_over_project_dirs() {
        let dir = data_dir_override(Some("C:/tmp/cuw-test")).expect("override");
        assert_eq!(dir, PathBuf::from("C:/tmp/cuw-test"));
    }

    /// The real signal the single-instance guard keys on, taken from a real
    /// second bind rather than a hand-made error.
    #[test]
    fn a_second_bind_on_the_same_port_reads_as_taken() {
        let first = std::net::TcpListener::bind("127.0.0.1:0").expect("first bind");
        let addr = first.local_addr().expect("addr");
        let err = std::net::TcpListener::bind(addr).expect_err("second bind");
        assert!(port_taken(&err), "{err:?}");
    }

    #[test]
    fn an_ordinary_bind_failure_is_not_a_second_instance() {
        let err = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(!port_taken(&err));
    }
}
