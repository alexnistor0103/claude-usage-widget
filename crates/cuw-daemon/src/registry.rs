//! The daemon-owned account registry (M2.5 / M1.5 decision). `registry.toml`
//! lives in the daemon's data dir and is written by the daemon, kept **separate**
//! from the hand-authored `accounts.toml` so daemon writes never clobber the
//! user's comments. Holds identity only — never tokens.

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub accounts: Vec<RegAccount>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegAccount {
    pub id: String,
    pub label: String,
    /// RFC3339. Kept as a string so a hand-edit with an odd value degrades to a
    /// substituted timestamp rather than failing the whole load.
    pub connected_at: String,
    /// The CLI's `oauthAccount` block for this login, as JSON text: uuid,
    /// email, org. Text rather than a table so the CLI's nulls survive TOML.
    /// Absent for an account connected before it was captured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_account: Option<String>,
}

impl RegAccount {
    /// The identity block, if stored and still well-formed.
    pub fn account(&self) -> Option<serde_json::Value> {
        let v: serde_json::Value = serde_json::from_str(self.oauth_account.as_deref()?).ok()?;
        cuw_switch::account_uuid(&v).is_some().then_some(v)
    }

    /// Store an identity block as text; a block without a uuid is not kept.
    pub fn set_account(&mut self, account: Option<&serde_json::Value>) {
        self.oauth_account = account
            .filter(|a| cuw_switch::account_uuid(a).is_some())
            .map(serde_json::Value::to_string);
    }
}

impl Registry {
    /// A missing or unparseable registry loads as empty rather than panicking —
    /// every unknown is a state, not a crash.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Write through a sibling temp file and rename over the original: this is
    /// the file that names every account, and a half-written or truncated one
    /// reads back as `accounts = []`, which is indistinguishable from "the user
    /// disconnected everything". `rename` replaces on both platforms.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let body = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("toml.new");
        std::fs::write(&tmp, body)?;
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: &str) -> RegAccount {
        RegAccount {
            id: id.into(),
            label: id.into(),
            connected_at: "2026-08-31T09:56:14Z".into(),
            oauth_account: None,
        }
    }

    #[test]
    fn a_saved_registry_round_trips_and_leaves_no_temp_file() {
        let dir = std::env::temp_dir().join(format!("cuw-registry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("registry.toml");

        let mut work = account("work-f9ca7144");
        work.set_account(Some(&serde_json::json!({
            "accountUuid": "u-1", "emailAddress": "w@example.com", "seatTier": null
        })));
        let reg = Registry {
            accounts: vec![account("personal-5cb2ab9c"), work],
        };
        reg.save(&path).expect("save");

        let back = Registry::load(&path);
        assert_eq!(back.accounts.len(), 2);
        assert_eq!(back.accounts[1].id, "work-f9ca7144");
        assert!(back.accounts[0].account().is_none());
        let acct = back.accounts[1].account().expect("identity kept");
        assert_eq!(acct["emailAddress"], "w@example.com");
        assert!(acct["seatTier"].is_null(), "the CLI's nulls survive");
        assert!(!path.with_extension("toml.new").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_or_unparseable_registry_loads_as_empty() {
        let dir = std::env::temp_dir().join(format!("cuw-registry-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("registry.toml");
        assert!(Registry::load(&path).accounts.is_empty());

        std::fs::write(&path, "this is not toml = [").expect("write");
        assert!(Registry::load(&path).accounts.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_block_without_a_uuid_or_with_bad_text_is_not_an_identity() {
        let mut a = account("x-00000000");
        a.set_account(Some(
            &serde_json::json!({"emailAddress": "no-uuid@example.com"}),
        ));
        assert_eq!(a.oauth_account, None);
        a.oauth_account = Some("{not json".into());
        assert!(a.account().is_none());
    }
}
