//! Credential storage. The daemon owns its own tokens and stores one JSON
//! `Credential` blob per account, apart from Claude Code's own store — that
//! one is `cuw-switch`'s, touched only to switch accounts (plan §5).
//! `keyring` covers both platforms: `windows-native` is Credential Manager, and
//! `apple-native` calls Security.framework in-process from our own binary —
//! which is what plan §5 asks for, so there is no macOS native impl to drop to.
//!
//! Log a `CredError` with `%e`, never `?e`: `keyring::Error::BadEncoding`
//! carries the raw blob bytes, so it is mapped to `Corrupt` and dropped here
//! rather than wrapped.

#[cfg(target_os = "windows")]
pub mod windows;

use std::sync::OnceLock;

use cuw_core::Credential;

const SERVICE: &str = "com.local.cuw";

/// Env override for the service name, so a test daemon with its own
/// `CUW_DATA_DIR` writes into its own keyring namespace instead of the real
/// accounts. Unset or blank keeps [`SERVICE`].
const SERVICE_ENV: &str = "CUW_KEYRING_SERVICE";

/// The store's blob cap in UTF-16 bytes, where the backend has one. Windows
/// Credential Manager stops at 2560; the Keychain has no comparable limit, so
/// the smaller cap is not imposed on macOS.
const MAX_BLOB_UTF16_BYTES: Option<usize> = if cfg!(windows) { Some(2560) } else { None };

/// The only blob shape this build understands; anything else is `Corrupt`.
const BLOB_VERSION: u8 = 1;

/// Suffix an earlier build stored a second, `setup-token` grant under. Nothing
/// writes one any more; the key survives only so leftovers can be removed.
const CLI_SUFFIX: &str = "#cli";

/// The store key of an account's leftover CLI token. `#` never appears in an
/// id (`make_id` emits `[a-z0-9-]` only), so the namespaces cannot collide.
pub fn cli_key(id: &str) -> String {
    format!("{id}{CLI_SUFFIX}")
}

/// The keyring service every entry is written under. Read from the environment
/// once: the value decides which credentials a whole process can see, so it must
/// not change under a running daemon.
pub fn service() -> &'static str {
    static RESOLVED: OnceLock<String> = OnceLock::new();
    RESOLVED.get_or_init(|| resolve_service(std::env::var(SERVICE_ENV).ok().as_deref()))
}

/// Split out of [`service`] so the fallback is testable without touching a
/// shared process environment. A blank override is treated as unset — an empty
/// `CUW_KEYRING_SERVICE=` must not produce a nameless namespace.
fn resolve_service(raw: Option<&str>) -> String {
    match raw.map(str::trim) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => SERVICE.to_string(),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CredError {
    #[error("no credential for {0}")]
    NotFound(String),
    #[error("credential for {0} is unreadable")]
    Corrupt(String),
    #[error("credential for {0} exceeds the store limit")]
    TooLarge(String),
    #[error(transparent)]
    Backend(#[from] keyring::Error),
}

/// Store keyed by account id. The id registry lives in registry.toml, not here
/// — this holds only secrets.
pub trait CredentialStore: Send + Sync {
    fn put(&self, id: &str, cred: &Credential) -> Result<(), CredError>;
    fn get(&self, id: &str) -> Result<Credential, CredError>;
    fn delete(&self, id: &str) -> Result<(), CredError>;

    /// Remove a leftover `setup-token` grant under [`cli_key`]. `NotFound` is
    /// the ordinary answer.
    fn delete_cli(&self, id: &str) -> Result<(), CredError>;
}

/// Reject an oversized blob before it reaches the backend, so the failure names
/// the account instead of surfacing as an opaque keyring error.
pub(crate) fn check_size(id: &str, s: &str) -> Result<(), CredError> {
    match MAX_BLOB_UTF16_BYTES {
        Some(cap) if s.encode_utf16().count() * 2 > cap => Err(CredError::TooLarge(id.into())),
        _ => Ok(()),
    }
}

/// Map a backend read to a `Credential`. `BadEncoding` carries the raw blob, so
/// it becomes `Corrupt` and the bytes are dropped — never `Backend`.
pub(crate) fn decode(id: &str, r: Result<String, keyring::Error>) -> Result<Credential, CredError> {
    let s = read_blob(id, r)?;
    let cred: Credential = serde_json::from_str(&s).map_err(|_| CredError::Corrupt(id.into()))?;
    if cred.v != BLOB_VERSION {
        return Err(CredError::Corrupt(id.into()));
    }
    Ok(cred)
}

/// The backend read, with the two error shapes that must never carry bytes
/// mapped away first.
fn read_blob(id: &str, r: Result<String, keyring::Error>) -> Result<String, CredError> {
    match r {
        Ok(s) => Ok(s),
        Err(keyring::Error::NoEntry) => Err(CredError::NotFound(id.into())),
        Err(keyring::Error::BadEncoding(_)) => Err(CredError::Corrupt(id.into())),
        Err(e) => Err(CredError::Backend(e)),
    }
}

/// keyring-backed store: DPAPI-equivalent on Windows, Keychain on macOS.
pub struct KeyringStore;

impl KeyringStore {
    fn entry(id: &str) -> Result<keyring::Entry, CredError> {
        Ok(keyring::Entry::new(service(), id)?)
    }
}

impl CredentialStore for KeyringStore {
    fn put(&self, id: &str, cred: &Credential) -> Result<(), CredError> {
        let s = serde_json::to_string(cred).map_err(|_| CredError::Corrupt(id.into()))?;
        check_size(id, &s)?;
        Self::entry(id)?.set_password(&s)?;
        Ok(())
    }

    fn get(&self, id: &str) -> Result<Credential, CredError> {
        decode(id, Self::entry(id)?.get_password())
    }

    fn delete(&self, id: &str) -> Result<(), CredError> {
        deleted(id, Self::entry(id)?.delete_credential())
    }

    fn delete_cli(&self, id: &str) -> Result<(), CredError> {
        let key = cli_key(id);
        deleted(&key, Self::entry(&key)?.delete_credential())
    }
}

/// A delete of an absent entry is `NotFound`, the ordinary answer the callers
/// match on, not a backend error.
fn deleted(id: &str, r: Result<(), keyring::Error>) -> Result<(), CredError> {
    match r {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Err(CredError::NotFound(id.into())),
        Err(e) => Err(CredError::Backend(e)),
    }
}

pub fn default_store() -> impl CredentialStore {
    KeyringStore
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake(len: usize) -> Credential {
        let pad = |prefix: &str| {
            let mut s = String::from(prefix);
            while s.len() < len {
                s.push('0');
            }
            s
        };
        let mut c = Credential::new(
            pad("sk-ant-oat01-FAKE"),
            pad("sk-ant-ort01-FAKE"),
            1_756_600_000,
            vec!["user:inference".into(), "user:profile".into()],
        );
        c.subscription_type = Some("max".into());
        c.rate_limit_tier = Some("default_claude_max_20x".into());
        c.refresh_token_expires_at = Some(1_759_000_000);
        c
    }

    /// Round-trips a throwaway entry through the real OS keyring. Ignored so CI
    /// without a keyring backend still passes; run on the target OS with
    /// `cargo test -p cuw-creds -- --ignored` (M1.1).
    #[test]
    #[ignore]
    fn keyring_put_get_delete_round_trip() {
        let store = KeyringStore;
        let id = format!("cuw-roundtrip-{}", std::process::id());
        let cred = fake(40);

        store.put(&id, &cred).expect("put");
        let got = store.get(&id).expect("get");
        assert_eq!(got.v, cred.v);
        assert_eq!(got.access_token, cred.access_token);
        assert_eq!(got.refresh_token, cred.refresh_token);
        assert_eq!(got.expires_at, cred.expires_at);
        assert_eq!(got.scopes, cred.scopes);
        assert_eq!(got.subscription_type, cred.subscription_type);
        assert_eq!(got.refresh_token_expires_at, cred.refresh_token_expires_at);

        store.delete(&id).expect("delete");
        assert!(matches!(store.get(&id), Err(CredError::NotFound(_))));
    }

    #[test]
    fn cli_key_is_a_separate_namespace() {
        assert_eq!(cli_key("work-abc12345"), "work-abc12345#cli");
        // Ids come from `make_id`, which emits `[a-z0-9-]` only, so no id can
        // ever collide with another id's CLI key.
        assert!(!"work-abc12345".contains('#'));
    }

    /// Two tokens far longer than the real ones still fit Windows' blob cap, so
    /// a normal credential can never hit `TooLarge` on either platform.
    #[test]
    fn blob_for_two_long_tokens_fits_windows_limit() {
        let s = serde_json::to_string(&fake(120)).expect("serialize");
        assert!(s.len() < 1280, "blob is {} chars", s.len());
        assert!(s.encode_utf16().count() * 2 <= 2560);
        assert!(check_size("work-abc12345", &s).is_ok());
    }

    /// The 2560-byte cap is Credential Manager's, not a universal one: on macOS
    /// an oversized blob is the backend's business, not a pre-emptive refusal.
    #[test]
    fn the_blob_cap_applies_on_windows_only() {
        let huge = "x".repeat(1300);
        let checked = check_size("work-abc12345", &huge);
        if cfg!(windows) {
            assert!(matches!(checked, Err(CredError::TooLarge(id)) if id == "work-abc12345"));
        } else {
            assert!(checked.is_ok());
        }
    }

    #[test]
    fn the_service_name_falls_back_when_the_override_is_absent_or_blank() {
        assert_eq!(resolve_service(None), SERVICE);
        assert_eq!(resolve_service(Some("")), SERVICE);
        assert_eq!(resolve_service(Some("   ")), SERVICE);
    }

    /// A test daemon points `CUW_KEYRING_SERVICE` somewhere else so a live run
    /// cannot read or overwrite the real accounts.
    #[test]
    fn the_service_name_honours_the_override() {
        assert_eq!(
            resolve_service(Some("com.local.cuw-test")),
            "com.local.cuw-test"
        );
        assert_ne!(resolve_service(Some("com.local.cuw-test")), SERVICE);
    }

    #[test]
    fn bad_encoding_is_corrupt_and_debug_has_no_blob() {
        let err = decode(
            "x",
            Err(keyring::Error::BadEncoding(b"sk-ant-oat01-FAKE".to_vec())),
        )
        .expect_err("bad encoding");
        assert!(matches!(err, CredError::Corrupt(_)));
        assert!(!format!("{err:?}").contains("sk-ant"));
        assert!(!format!("{err}").contains("sk-ant"));
    }

    #[test]
    fn unknown_version_is_corrupt() {
        let blob = r#"{"v":2,"access_token":"sk-ant-oat01-FAKE","refresh_token":"sk-ant-ort01-FAKE","expires_at":1756600000,"scopes":[]}"#;
        assert!(matches!(
            decode("x", Ok(blob.into())),
            Err(CredError::Corrupt(_))
        ));
    }

    #[test]
    fn garbage_is_corrupt() {
        assert!(matches!(
            decode("x", Ok("not json at all".into())),
            Err(CredError::Corrupt(_))
        ));
        // Valid JSON, wrong shape: the required token fields are missing.
        assert!(matches!(
            decode("x", Ok(r#"{"v":1}"#.into())),
            Err(CredError::Corrupt(_))
        ));
    }

    #[test]
    fn deleting_an_absent_entry_is_not_found() {
        assert!(matches!(
            deleted("x", Err(keyring::Error::NoEntry)),
            Err(CredError::NotFound(id)) if id == "x"
        ));
        assert!(deleted("x", Ok(())).is_ok());
    }

    #[test]
    fn no_entry_is_not_found() {
        assert!(matches!(
            decode("x", Err(keyring::Error::NoEntry)),
            Err(CredError::NotFound(id)) if id == "x"
        ));
    }
}
