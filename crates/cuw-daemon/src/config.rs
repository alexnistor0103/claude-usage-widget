use serde::Deserialize;

/// Daemon config from `accounts.toml`. Holds the account registry and port —
/// never tokens.
#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub accounts: Vec<Account>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)] // read at M2
pub struct Account {
    pub id: String,
    pub label: String,
}

fn default_port() -> u16 {
    8787
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: default_port(),
            accounts: Vec::new(),
        }
    }
}

impl Config {
    /// Load from `$CUW_CONFIG` or `./accounts.toml`. A missing file is fine —
    /// no accounts yet.
    pub fn load() -> anyhow::Result<Self> {
        let path = std::env::var("CUW_CONFIG").unwrap_or_else(|_| "accounts.toml".into());
        match std::fs::read_to_string(&path) {
            Ok(s) => Ok(toml::from_str(&s)?),
            Err(_) => Ok(Self::default()),
        }
    }
}
