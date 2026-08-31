//! Session switching: start a **new** Claude Code session signed in as a chosen
//! account (SWITCHER §4). A running session holds its credential in memory, so
//! this can never move an existing session to another account — it only opens a
//! terminal.
//!
//! The CLI token is never passed as a process argument: a process list is
//! readable by every process in the session. Instead the daemon mints a
//! single-use, short-lived nonce and spawns a terminal on a **shim** ([`shim`]),
//! which redeems the nonce over localhost, puts the token in its own
//! environment and runs `claude`. The shim on disk holds no secret, which is
//! also what makes the launcher terminal-agnostic: nothing depends on a
//! terminal emulator inheriting the daemon's environment (SWITCHER §5).
//!
//! Command construction lives in [`plan`] and is pure, so the argv is tested
//! without spawning anything; only the spawn itself is per-platform.

pub mod plan;
pub mod shim;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

use std::path::PathBuf;

/// Placeholders a `settings.session.terminal` override may use. On Windows a
/// command containing [`P_SHIM`] replaces the default command outright and any
/// other override is a prefix the default is appended to; macOS deliberately
/// diverges (see [`plan::macos_argv`]).
pub const P_SHIM: &str = "{shim}";
pub const P_NONCE: &str = "{nonce}";
pub const P_PORT: &str = "{port}";
pub const P_CWD: &str = "{cwd}";
/// The per-launch `.command` wrapper. macOS only — nothing wraps the shim on
/// Windows, so this stays literal there.
pub const P_WRAPPER: &str = "{wrapper}";

/// Env that would bind another identity and silently win over the injected
/// token. Cleared at spawn and again inside the shim, which may be started by a
/// terminal of the user's choosing (SWITCHER §5).
pub const SCRUBBED_ENV: [&str; 5] = [
    "CLAUDE_CODE_OAUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
];

/// One launch. Carries no token — only the nonce that buys one (SWITCHER §4).
#[derive(Debug, Clone)]
pub struct LaunchRequest {
    /// Single-use redemption code minted by the daemon.
    pub nonce: String,
    /// The daemon's bound localhost port; the shim reads it from argv rather
    /// than assuming a default.
    pub port: u16,
    /// Directory the new session starts in.
    pub cwd: PathBuf,
    /// `settings.session.terminal`, already split into argv. `None` or empty
    /// means the platform default.
    pub terminal: Option<Vec<String>>,
}

impl LaunchRequest {
    pub fn new(nonce: impl Into<String>, port: u16, cwd: impl Into<PathBuf>) -> Self {
        Self {
            nonce: nonce.into(),
            port,
            cwd: cwd.into(),
            terminal: None,
        }
    }

    pub fn with_terminal(mut self, terminal: Option<Vec<String>>) -> Self {
        self.terminal = terminal.filter(|t| !t.is_empty());
        self
    }
}

/// Never carries the nonce or a token: a `LaunchError` is displayed and logged,
/// and the redemption code is as good as a credential until it is burned.
#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("the session code is malformed")]
    BadNonce,
    #[error("the daemon has no bound port")]
    BadPort,
    #[error("no such working directory: {0}")]
    BadCwd(PathBuf),
    #[error("could not write the session shim: {0}")]
    Shim(String),
    #[error("could not start a terminal: {0}")]
    Spawn(String),
    #[error("session switching is not supported on this platform yet")]
    Unsupported,
}

/// Per-platform launcher entry point, matching `WindowTracker` and
/// `CredentialStore`. `Send + Sync` so the daemon can hold one as
/// `Arc<dyn SessionLauncher>`.
pub trait SessionLauncher: Send + Sync {
    fn launch(&self, req: LaunchRequest) -> Result<(), LaunchError>;
}

/// The launcher for this build. On an unsupported platform every launch is a
/// display state (`switch unavailable`), never a panic (plan §9).
pub fn for_this_platform(data_dir: impl Into<PathBuf>) -> Box<dyn SessionLauncher> {
    let data_dir = data_dir.into();
    #[cfg(target_os = "windows")]
    {
        Box::new(windows::WindowsLauncher::new(data_dir))
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::MacosLauncher::new(data_dir))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = data_dir;
        Box::new(Unsupported)
    }
}

/// Stands in wherever no platform impl exists (everything but Windows and
/// macOS).
pub struct Unsupported;

impl SessionLauncher for Unsupported {
    fn launch(&self, _req: LaunchRequest) -> Result<(), LaunchError> {
        Err(LaunchError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_terminal_override_is_none() {
        let req = LaunchRequest::new("abcd1234", 8787, ".").with_terminal(Some(Vec::new()));
        assert!(req.terminal.is_none());
    }

    #[test]
    fn errors_never_carry_the_nonce() {
        // Every variant is a fixed string or a path; nothing formats a nonce.
        let msgs = [
            LaunchError::BadNonce.to_string(),
            LaunchError::BadPort.to_string(),
            LaunchError::Shim("io".into()).to_string(),
            LaunchError::Spawn("io".into()).to_string(),
            LaunchError::Unsupported.to_string(),
        ];
        for m in msgs {
            assert!(!m.contains("nonce-"), "{m}");
        }
    }

    #[test]
    fn unsupported_launcher_reports_a_display_state() {
        let e = Unsupported
            .launch(LaunchRequest::new("abcd1234", 8787, "."))
            .expect_err("unsupported");
        assert!(matches!(e, LaunchError::Unsupported));
    }
}
