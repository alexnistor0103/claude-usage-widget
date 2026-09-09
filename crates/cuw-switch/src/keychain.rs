//! The macOS half of the CLI store: a generic-password item in the login
//! Keychain, read and written through `security` exactly as the CLI does, so
//! the item's access list stays the one the CLI already trusts. The service
//! name derivation is pure and builds everywhere so it can be tested anywhere.

use std::path::Path;

#[cfg(target_os = "macos")]
use crate::SwitchError;

/// The CLI's Keychain service: `Claude Code-credentials`, plus `-` and the
/// first 8 hex of SHA-256 over `CLAUDE_CONFIG_DIR` when one is set, hashed
/// exactly as the env var carries it.
pub fn service(config_dir: Option<&Path>) -> String {
    use sha2::{Digest, Sha256};
    let Some(dir) = config_dir else {
        return "Claude Code-credentials".to_string();
    };
    let digest = Sha256::digest(dir.as_os_str().as_encoded_bytes());
    let mut suffix = String::with_capacity(8);
    for b in &digest[..4] {
        use std::fmt::Write;
        let _ = write!(suffix, "{b:02x}");
    }
    format!("Claude Code-credentials-{suffix}")
}

/// `security`'s exit status for "no such item".
#[cfg(target_os = "macos")]
const NOT_FOUND: i32 = 44;

/// The item's data, or `None` when there is no item. The data is a token, so
/// neither `security`'s output nor its stderr is ever logged or quoted.
#[cfg(target_os = "macos")]
pub fn read(service: &str) -> Result<Option<String>, SwitchError> {
    let out = std::process::Command::new("security")
        .args(["find-generic-password", "-s", service, "-w"])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| SwitchError::Read(format!("security: {e}")))?;
    match out.status.code() {
        Some(0) => String::from_utf8(out.stdout)
            .map(|s| Some(s.trim_end().to_string()))
            .map_err(|_| SwitchError::Read("security: non-utf8 item".into())),
        Some(NOT_FOUND) => Ok(None),
        code => Err(SwitchError::Read(format!(
            "security find-generic-password exited with {}",
            code.map_or("a signal".to_string(), |c| c.to_string())
        ))),
    }
}

/// Create or update the item in place (`-U` keeps the existing access list).
/// The command goes over `security -i`'s stdin as hex, so the data never
/// appears in a process list; the item is read back to confirm the write.
#[cfg(target_os = "macos")]
pub fn write(service: &str, data: &str) -> Result<(), SwitchError> {
    use std::io::Write;
    let account = username().ok_or_else(|| SwitchError::Write("no account name".into()))?;
    if account.contains('"') || service.contains('"') {
        return Err(SwitchError::Write(
            "unquotable account or service name".into(),
        ));
    }
    let hex: String = data.bytes().map(|b| format!("{b:02x}")).collect();
    let line = format!("add-generic-password -U -a \"{account}\" -s \"{service}\" -X {hex}\n");

    let mut child = std::process::Command::new("security")
        .arg("-i")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| SwitchError::Write(format!("security: {e}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(line.as_bytes())
            .map_err(|e| SwitchError::Write(format!("security stdin: {e}")))?;
    }
    let status = child
        .wait()
        .map_err(|e| SwitchError::Write(format!("security: {e}")))?;
    if !status.success() {
        return Err(SwitchError::Write(format!(
            "security add-generic-password exited with {status}"
        )));
    }
    match read(service)? {
        Some(back) if back == data => Ok(()),
        _ => Err(SwitchError::Write(
            "the item did not read back as written".into(),
        )),
    }
}

/// Remove the item. Best effort: used to scrub a scratch dir's item, where a
/// missing item is already the desired state.
#[cfg(target_os = "macos")]
pub fn delete(service: &str) {
    let _ = std::process::Command::new("security")
        .args(["delete-generic-password", "-s", service])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// The passwd name for the effective uid — what the CLI keys the item's
/// account on. `$USER` is only a fallback: it is unset under launchd.
#[cfg(target_os = "macos")]
fn username() -> Option<String> {
    use std::ffi::CStr;
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0u8; 4096];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer names a buffer this frame owns and sizes; `result`
    // is checked before the name it points into is read.
    let rc = unsafe {
        libc::getpwuid_r(
            libc::geteuid(),
            &mut pwd,
            buf.as_mut_ptr().cast::<libc::c_char>(),
            buf.len(),
            &mut result,
        )
    };
    let from_passwd = (rc == 0 && !result.is_null())
        .then(|| {
            unsafe { CStr::from_ptr(pwd.pw_name) }
                .to_str()
                .ok()
                .map(str::to_string)
        })
        .flatten()
        .filter(|s| !s.is_empty());
    from_passwd.or_else(|| std::env::var("USER").ok().filter(|s| !s.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_service_has_no_suffix_and_an_override_a_stable_one() {
        assert_eq!(service(None), "Claude Code-credentials");
        let a = service(Some(Path::new("/tmp/one")));
        let b = service(Some(Path::new("/tmp/one")));
        let c = service(Some(Path::new("/tmp/two")));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("Claude Code-credentials-"));
        assert!(a["Claude Code-credentials-".len()..]
            .chars()
            .all(|ch| ch.is_ascii_hexdigit()));
    }

    /// Writes, updates, reads back and deletes a throwaway item in the real
    /// login Keychain. Ignored: it needs an unlocked Keychain and a GUI session.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn manual_keychain_round_trip() {
        let svc = format!("cuw-switch-test-{}", std::process::id());
        assert_eq!(read(&svc).unwrap(), None);
        write(&svc, r#"{"probe":"a b \"c\""}"#).unwrap();
        assert_eq!(
            read(&svc).unwrap().as_deref(),
            Some(r#"{"probe":"a b \"c\""}"#)
        );
        write(&svc, r#"{"probe":2}"#).unwrap();
        assert_eq!(read(&svc).unwrap().as_deref(), Some(r#"{"probe":2}"#));
        delete(&svc);
        assert_eq!(read(&svc).unwrap(), None);
    }
}
