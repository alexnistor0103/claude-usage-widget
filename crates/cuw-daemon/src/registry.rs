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
        }
    }

    #[test]
    fn a_saved_registry_round_trips_and_leaves_no_temp_file() {
        let dir = std::env::temp_dir().join(format!("cuw-registry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("registry.toml");

        let reg = Registry {
            accounts: vec![account("personal-5cb2ab9c"), account("work-f9ca7144")],
        };
        reg.save(&path).expect("save");

        let back = Registry::load(&path);
        assert_eq!(back.accounts.len(), 2);
        assert_eq!(back.accounts[1].id, "work-f9ca7144");
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
}
