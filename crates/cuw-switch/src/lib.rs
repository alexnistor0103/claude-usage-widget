//! Claude Code's own credential store: the one `claude auth login` and `/login`
//! write and every running `claude` reads. Switching the active account means
//! writing an account's credential there — exactly what `/login` does. The CLI
//! reads its store back instead of pinning a token in memory, so sessions
//! already open follow the switch as well.
//!
//! macOS keeps it in the login Keychain (`Claude Code-credentials`, suffixed
//! with a hash of `CLAUDE_CONFIG_DIR` when that is set); everywhere else it is
//! `<config dir>/.credentials.json`. Beside it, `.claude.json` carries the
//! `oauthAccount` block the CLI shows in `/status`; a switch writes that too
//! when the connect captured one.
//!
//! This is the one place the daemon deliberately hands a token to another
//! program. Nothing here logs one: a store that fails to parse reads as
//! `None`, and every error text carries a path or a status, never content.

pub mod keychain;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use cuw_core::Credential;
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum SwitchError {
    #[error("no home directory")]
    NoHome,
    #[error("could not read the CLI credential store: {0}")]
    Read(String),
    #[error("could not write the CLI credential store: {0}")]
    Write(String),
    #[error("could not update the CLI config: {0}")]
    Config(String),
}

/// The CLI's store, behind a trait so the daemon's tests drive an in-memory
/// one and never touch a real login.
pub trait CliStore: Send + Sync {
    /// The credential the CLI holds now. A store that is absent or fails to
    /// parse is `None`; only an I/O failure is an error.
    fn read(&self) -> Result<Option<Credential>, SwitchError>;
    /// Replace the CLI's credential, as a login would.
    fn write(&self, cred: &Credential) -> Result<(), SwitchError>;
    /// The `oauthAccount` block of the CLI's config, if it names an account.
    fn read_account(&self) -> Option<Value>;
    /// Replace that block — or remove it, for `None` — leaving the rest of the
    /// config as it was. A stale block is worse than none: the uuid in it is
    /// what identifies the account the CLI holds.
    fn write_account(&self, account: Option<&Value>) -> Result<(), SwitchError>;
}

/// The `accountUuid` an `oauthAccount` block carries — the one field a switch
/// keys identity on, so a block without it is not an account.
pub fn account_uuid(account: &Value) -> Option<&str> {
    account
        .get("accountUuid")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// The CLI's JSON, either store, as a credential — or `None`, silently.
pub fn parse_cli_json(text: &str) -> Option<Credential> {
    let v: Value = serde_json::from_str(text.trim()).ok()?;
    Credential::from_cli_json(&v)
}

/// The store of the CLI installed for this user.
pub struct Installed {
    /// `CLAUDE_CONFIG_DIR`, or `~/.claude`.
    config_dir: PathBuf,
    /// `.claude.json`: in the home dir by default, inside an overridden
    /// config dir otherwise — the CLI's own rule.
    config_file: PathBuf,
    /// The Keychain service the credential is keyed under on macOS.
    service: String,
}

impl Installed {
    pub fn detect() -> Result<Installed, SwitchError> {
        let home = directories::UserDirs::new()
            .map(|d| d.home_dir().to_path_buf())
            .ok_or(SwitchError::NoHome)?;
        Ok(Self::resolve(
            std::env::var_os("CLAUDE_CONFIG_DIR").as_deref(),
            &home,
        ))
    }

    /// Split out of [`Installed::detect`] so the path rule is testable without
    /// touching the process environment. A blank override is unset.
    pub fn resolve(config_dir: Option<&OsStr>, home: &Path) -> Installed {
        match config_dir.filter(|s| !s.is_empty()) {
            Some(dir) => Installed {
                config_dir: PathBuf::from(dir),
                config_file: Path::new(dir).join(".claude.json"),
                service: keychain::service(Some(Path::new(dir))),
            },
            None => Installed {
                config_dir: home.join(".claude"),
                config_file: home.join(".claude.json"),
                service: keychain::service(None),
            },
        }
    }

    pub fn credentials_file(&self) -> PathBuf {
        self.config_dir.join(".credentials.json")
    }

    pub fn config_file(&self) -> &Path {
        &self.config_file
    }

    pub fn service(&self) -> &str {
        &self.service
    }
}

impl CliStore for Installed {
    fn read(&self) -> Result<Option<Credential>, SwitchError> {
        #[cfg(target_os = "macos")]
        if let Some(text) = keychain::read(&self.service)? {
            return Ok(parse_cli_json(&text));
        }
        // The CLI falls back to the file when the Keychain has nothing, and the
        // file is the only store elsewhere.
        read_file(&self.credentials_file())
    }

    fn write(&self, cred: &Credential) -> Result<(), SwitchError> {
        let text = cred.to_cli_json().to_string();
        #[cfg(target_os = "macos")]
        {
            keychain::write(&self.service, &text)
        }
        #[cfg(not(target_os = "macos"))]
        {
            write_file(&self.credentials_file(), &text)
        }
    }

    fn read_account(&self) -> Option<Value> {
        let text = std::fs::read_to_string(&self.config_file).ok()?;
        let v: Value = serde_json::from_str(&text).ok()?;
        let account = v.get("oauthAccount")?;
        account_uuid(account).is_some().then(|| account.clone())
    }

    fn write_account(&self, account: Option<&Value>) -> Result<(), SwitchError> {
        let path = &self.config_file;
        // The CLI rewrites this file often and whole; the window in which a
        // read-modify-rename can lose one of its writes is small, and not
        // opened at all when the block already says what it should.
        if self.read_account().as_ref() == account {
            return Ok(());
        }
        let mut config = match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str::<Value>(&text)
                .ok()
                .filter(Value::is_object)
                // A config we cannot parse is not ours to replace: the CLI
                // keeps far more than the account in it.
                .ok_or_else(|| {
                    SwitchError::Config(format!("{} is not a JSON object", path.display()))
                })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Object(Default::default()),
            Err(e) => return Err(SwitchError::Config(format!("{}: {e}", path.display()))),
        };
        match (config.as_object_mut(), account) {
            (Some(o), Some(a)) => {
                o.insert("oauthAccount".into(), a.clone());
            }
            (Some(o), None) => {
                o.remove("oauthAccount");
            }
            (None, _) => unreachable!("filtered to an object above"),
        }
        let text = serde_json::to_string_pretty(&config)
            .map_err(|e| SwitchError::Config(e.to_string()))?;
        replace_file(path, &text, false).map_err(SwitchError::Config)
    }
}

/// Stands in when the CLI's store cannot even be located (no home directory):
/// nothing is ever active, and a switch is refused rather than misdirected.
pub struct NoStore;

impl CliStore for NoStore {
    fn read(&self) -> Result<Option<Credential>, SwitchError> {
        Ok(None)
    }
    fn write(&self, _cred: &Credential) -> Result<(), SwitchError> {
        Err(SwitchError::NoHome)
    }
    fn read_account(&self) -> Option<Value> {
        None
    }
    fn write_account(&self, _account: Option<&Value>) -> Result<(), SwitchError> {
        Err(SwitchError::NoHome)
    }
}

fn read_file(path: &Path) -> Result<Option<Credential>, SwitchError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(parse_cli_json(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(SwitchError::Read(format!("{}: {e}", path.display()))),
    }
}

#[cfg(not(target_os = "macos"))]
fn write_file(path: &Path, text: &str) -> Result<(), SwitchError> {
    replace_file(path, text, true).map_err(SwitchError::Write)
}

/// Write through a sibling temp file and rename over the target, so a reader
/// never sees a half-written store. `private` keeps the file to the owner on
/// unix, as the CLI does for its credential file; otherwise an existing
/// target's mode is kept.
fn replace_file(path: &Path, text: &str, private: bool) -> Result<(), String> {
    let display = path.display();
    let parent = path
        .parent()
        .ok_or_else(|| format!("{display}: no parent"))?;
    std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let tmp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(OsStr::to_str).unwrap_or("store"),
        std::process::id()
    ));
    let written = write_private(&tmp, text, private).and_then(|()| {
        #[cfg(unix)]
        if !private {
            if let Ok(meta) = std::fs::metadata(path) {
                std::fs::set_permissions(&tmp, meta.permissions())?;
            }
        }
        Ok(())
    });
    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{display}: {e}"));
    }
    Ok(())
}

fn write_private(path: &Path, text: &str, private: bool) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = private;
    let mut f = opts.open(path)?;
    f.write_all(text.as_bytes())?;
    f.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLI_JSON: &str = r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-FAKEFAKEFAKEFAKEFAKEFAKE0001","refreshToken":"sk-ant-ort01-FAKEFAKEFAKEFAKEFAKEFAKE0002","expiresAt":1756600000000,"scopes":["user:inference","user:profile"],"subscriptionType":"max"}}"#;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cuw-switch-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp root");
        dir
    }

    #[test]
    fn the_default_store_hangs_off_home_and_an_override_off_itself() {
        let home = Path::new("/Users/someone");
        let d = Installed::resolve(None, home);
        assert_eq!(d.credentials_file(), home.join(".claude/.credentials.json"));
        assert_eq!(d.config_file(), home.join(".claude.json"));
        assert_eq!(d.service(), "Claude Code-credentials");

        let blank = Installed::resolve(Some(OsStr::new("")), home);
        assert_eq!(blank.config_file(), home.join(".claude.json"));

        let o = Installed::resolve(Some(OsStr::new("/tmp/cfg")), home);
        assert_eq!(
            o.credentials_file(),
            Path::new("/tmp/cfg/.credentials.json")
        );
        assert_eq!(o.config_file(), Path::new("/tmp/cfg/.claude.json"));
        assert!(o.service().starts_with("Claude Code-credentials-"));
        assert_eq!(o.service().len(), "Claude Code-credentials-".len() + 8);
    }

    #[test]
    fn a_missing_or_malformed_file_reads_as_none_and_never_errors() {
        let root = temp_root("read");
        let path = root.join(".credentials.json");
        assert!(read_file(&path).unwrap().is_none());
        std::fs::write(&path, "{not json").unwrap();
        assert!(read_file(&path).unwrap().is_none());
        std::fs::write(&path, r#"{"claudeAiOauth":{"accessToken":"a"}}"#).unwrap();
        assert!(read_file(&path).unwrap().is_none());
        std::fs::write(&path, CLI_JSON).unwrap();
        let c = read_file(&path).unwrap().expect("parses");
        assert_eq!(c.subscription_type.as_deref(), Some("max"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_replaced_file_is_whole_private_and_leaves_no_temp_behind() {
        let root = temp_root("replace");
        let path = root.join("nested").join(".credentials.json");
        replace_file(&path, CLI_JSON, true).expect("first write");
        replace_file(&path, "{}", true).expect("overwrite");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_account_block_is_swapped_and_the_rest_of_the_config_kept() {
        let root = temp_root("config");
        let store = Installed::resolve(Some(root.as_os_str()), &root);
        assert!(store.read_account().is_none(), "no config yet");

        std::fs::write(
            store.config_file(),
            r#"{"numStartups":7,"projects":{"/x":{"allowedTools":[]}},"oauthAccount":{"accountUuid":"old","emailAddress":"old@example.com"}}"#,
        )
        .unwrap();
        let got = store.read_account().expect("account");
        assert_eq!(account_uuid(&got), Some("old"));

        let next = serde_json::json!({"accountUuid":"new","emailAddress":"new@example.com"});
        store.write_account(Some(&next)).expect("write");
        let back: Value =
            serde_json::from_str(&std::fs::read_to_string(store.config_file()).unwrap()).unwrap();
        assert_eq!(back["numStartups"], 7);
        assert_eq!(
            back["projects"]["/x"]["allowedTools"],
            serde_json::json!([])
        );
        assert_eq!(back["oauthAccount"], next);

        // No identity for the account being switched to: the old block goes,
        // the rest stays.
        store.write_account(None).expect("clear");
        let back: Value =
            serde_json::from_str(&std::fs::read_to_string(store.config_file()).unwrap()).unwrap();
        assert!(back.get("oauthAccount").is_none());
        assert_eq!(back["numStartups"], 7);
        assert!(store.read_account().is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_config_is_created_and_a_corrupt_one_is_left_alone() {
        let root = temp_root("config-edge");
        let store = Installed::resolve(Some(root.as_os_str()), &root);
        let acct = serde_json::json!({"accountUuid":"u"});
        store.write_account(Some(&acct)).expect("create");
        assert_eq!(store.read_account(), Some(acct.clone()));

        std::fs::write(store.config_file(), "{broken").unwrap();
        let other = serde_json::json!({"accountUuid":"v"});
        let e = store.write_account(Some(&other)).expect_err("refused");
        assert!(matches!(e, SwitchError::Config(_)));
        assert_eq!(
            std::fs::read_to_string(store.config_file()).unwrap(),
            "{broken"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_rewritten_config_keeps_its_mode() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root("mode");
        let store = Installed::resolve(Some(root.as_os_str()), &root);
        std::fs::write(store.config_file(), "{}").unwrap();
        std::fs::set_permissions(store.config_file(), std::fs::Permissions::from_mode(0o600))
            .unwrap();
        store
            .write_account(Some(&serde_json::json!({"accountUuid":"u"})))
            .unwrap();
        let mode = std::fs::metadata(store.config_file())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_block_without_a_uuid_is_not_an_account() {
        assert_eq!(account_uuid(&serde_json::json!({"emailAddress":"x"})), None);
        assert_eq!(account_uuid(&serde_json::json!({"accountUuid":""})), None);
        assert_eq!(
            account_uuid(&serde_json::json!({"accountUuid":"u"})),
            Some("u")
        );
    }

    #[test]
    fn errors_name_paths_never_content() {
        let root = temp_root("errors");
        // A directory where the file should be: an I/O error, not a parse one.
        let dir = root.join(".credentials.json");
        std::fs::create_dir_all(&dir).unwrap();
        let e = read_file(&dir).expect_err("a directory is unreadable");
        let text = e.to_string();
        assert!(text.contains(".credentials.json"), "{text}");
        assert!(!text.contains("sk-ant"), "{text}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
