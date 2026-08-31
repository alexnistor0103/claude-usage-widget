//! Usage model, endpoint client, and poll helpers. No I/O side effects beyond
//! the client, which is behind `UsageSource` so the undocumented endpoint can
//! be swapped for an official one later (plan §9).

pub mod client;
pub mod credential;
pub mod model;
pub mod parse;
pub mod poller;
pub mod redact;
pub mod refresh;

pub use client::{OAuthUsageClient, UsageSource};
pub use credential::{CliToken, Credential};
pub use model::{AccountState, ScopedWindow, Usage, Window};
pub use parse::parse_usage;
pub use redact::redact;
pub use refresh::{
    error_code, parse_refresh, OAuthTokenClient, RefreshError, Refreshed, TokenRefresher,
    REJECT_CODES,
};
