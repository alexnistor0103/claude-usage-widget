//! Credential storage. The daemon owns its own tokens and stores one JSON
//! `Credential` blob per account; it never reads Claude Code's store (plan §5).
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

use cuw_core::{CliToken, Credential};

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

/// Suffix for the second, independent credential: the CLI's `setup-token`
/// grant (SWITCHER §3). A different key means a `get` can never hand the login
/// credential to the session route by mistake.
const CLI_SUFFIX: &str = "#cli";

/// The store key for an account's CLI token. `#` never appears in an id
/// (`make_id` emits `[a-z0-9-]` only), so the two namespaces cannot collide.
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

    /// The CLI token, under [`cli_key`]. Absent is ordinary — an account that
    /// never captured one shows `switch unavailable` (SWITCHER §6), so this
    /// returns `NotFound` rather than being an error state.
    fn put_cli(&self, id: &str, tok: &CliToken) -> Result<(), CredError>;
    fn get_cli(&self, id: &str) -> Result<CliToken, CredError>;
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

/// [`decode`] for the CLI token. A blob that parses as a `Credential` would not
/// parse here — the fields differ — so a key mix-up surfaces as `Corrupt`
/// rather than as the wrong secret.
pub(crate) fn decode_cli(
    id: &str,
    r: Result<String, keyring::Error>,
) -> Result<CliToken, CredError> {
    let s = read_blob(id, r)?;
    let tok: CliToken = serde_json::from_str(&s).map_err(|_| CredError::Corrupt(id.into()))?;
    if tok.v != BLOB_VERSION || tok.token.is_empty() {
        return Err(CredError::Corrupt(id.into()));
    }
    Ok(tok)
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
        Self::entry(id)?.delete_credential()?;
        Ok(())
    }

    fn put_cli(&self, id: &str, tok: &CliToken) -> Result<(), CredError> {
        let key = cli_key(id);
        let s = serde_json::to_string(tok).map_err(|_| CredError::Corrupt(key.clone()))?;
        check_size(&key, &s)?;
        Self::entry(&key)?.set_password(&s)?;
        Ok(())
    }

    fn get_cli(&self, id: &str) -> Result<CliToken, CredError> {
        let key = cli_key(id);
        decode_cli(&key, Self::entry(&key)?.get_password())
    }

    fn delete_cli(&self, id: &str) -> Result<(), CredError> {
        Self::entry(&cli_key(id))?.delete_credential()?;
        Ok(())
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
        Credential {
            v: 1,
            access_token: pad("sk-ant-oat01-FAKE"),
            refresh_token: pad("sk-ant-ort01-FAKE"),
            expires_at: 1_756_600_000,
            scopes: vec!["user:inference".into(), "user:profile".into()],
        }
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

        store.delete(&id).expect("delete");
        assert!(matches!(store.get(&id), Err(CredError::NotFound(_))));
    }

    /// The CLI token round-trips under its own key and is deleted independently
    /// of the login credential (SWITCHER §3).
    #[test]
    #[ignore]
    fn keyring_cli_token_round_trip_is_independent() {
        let store = KeyringStore;
        let id = format!("cuw-cli-roundtrip-{}", std::process::id());
        let cred = fake(40);
        let tok = CliToken::new("sk-ant-oat01-FAKECLI0000000000000000", now());

        store.put(&id, &cred).expect("put");
        store.put_cli(&id, &tok).expect("put_cli");

        let got = store.get_cli(&id).expect("get_cli");
        assert_eq!(got.token, tok.token);
        assert_eq!(got.captured_at, tok.captured_at);

        // Dropping the CLI token leaves the login credential alone.
        store.delete_cli(&id).expect("delete_cli");
        assert!(matches!(store.get_cli(&id), Err(CredError::NotFound(_))));
        assert!(store.get(&id).is_ok());

        store.delete(&id).expect("delete");
    }

    fn now() -> time::OffsetDateTime {
        time::OffsetDateTime::from_unix_timestamp(1_756_600_000).unwrap()
    }

    #[test]
    fn cli_key_is_a_separate_namespace() {
        assert_eq!(cli_key("work-abc12345"), "work-abc12345#cli");
        // Ids come from `make_id`, which emits `[a-z0-9-]` only, so no id can
        // ever collide with another id's CLI key.
        assert!(!"work-abc12345".contains('#'));
    }

    #[test]
    fn a_login_blob_never_decodes_as_a_cli_token() {
        let blob = serde_json::to_string(&fake(40)).expect("serialize");
        assert!(matches!(
            decode_cli("work-abc12345#cli", Ok(blob)),
            Err(CredError::Corrupt(_))
        ));
    }

    #[test]
    fn a_cli_blob_never_decodes_as_a_login_credential() {
        let blob = serde_json::to_string(&CliToken::new("sk-ant-oat01-FAKE", now())).unwrap();
        assert!(matches!(decode("x", Ok(blob)), Err(CredError::Corrupt(_))));
    }

    #[test]
    fn an_empty_or_wrong_version_cli_token_is_corrupt() {
        for blob in [
            r#"{"v":1,"token":"","captured_at":1756600000}"#,
            r#"{"v":2,"token":"sk-ant-oat01-FAKE","captured_at":1756600000}"#,
            "not json at all",
        ] {
            assert!(
                matches!(decode_cli("x", Ok(blob.into())), Err(CredError::Corrupt(_))),
                "{blob}"
            );
        }
        assert!(matches!(
            decode_cli("x", Err(keyring::Error::NoEntry)),
            Err(CredError::NotFound(_))
        ));
    }

    #[test]
    fn cli_token_debug_is_redacted() {
        let tok = CliToken::new("sk-ant-oat01-FAKECLI0000000000000000", now());
        let text = format!("{tok:?}");
        assert!(!text.contains("FAKECLI"), "{text}");
        assert!(text.contains("sk-a…"), "{text}");
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
    fn no_entry_is_not_found() {
        assert!(matches!(
            decode("x", Err(keyring::Error::NoEntry)),
            Err(CredError::NotFound(id)) if id == "x"
        ));
    }
}
