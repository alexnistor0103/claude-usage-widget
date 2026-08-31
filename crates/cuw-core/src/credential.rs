use std::fmt;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::redact::redact;
use crate::refresh::Refreshed;

fn one() -> u8 {
    1
}

/// The per-account blob kept in the OS credential store (plan §5). `v` exists
/// so a future shape change is detected instead of misparsed.
#[derive(Clone, Serialize, Deserialize)]
pub struct Credential {
    #[serde(default = "one")]
    pub v: u8,
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds, UTC.
    pub expires_at: i64,
    #[serde(default)]
    pub scopes: Vec<String>,
}

impl Credential {
    /// The scope the usage endpoint demands (S1: 403 without it).
    pub const REQUIRED_SCOPE: &'static str = "user:profile";

    pub fn has_usage_scope(&self) -> bool {
        self.scopes.iter().any(|s| s == Self::REQUIRED_SCOPE)
    }

    pub fn expires_at_utc(&self) -> Option<OffsetDateTime> {
        OffsetDateTime::from_unix_timestamp(self.expires_at).ok()
    }

    /// Apply a token response; a missing refresh_token/scopes keeps the old one.
    pub fn rotated(&self, r: &Refreshed, now: OffsetDateTime) -> Credential {
        // An overflowing sum only happens with an absurd clock; expiring "now"
        // is the safe reading (the next cycle refreshes again).
        let now_secs = now.unix_timestamp();
        let expires_at = i64::try_from(r.expires_in.as_secs())
            .ok()
            .and_then(|secs| now_secs.checked_add(secs))
            .unwrap_or(now_secs);
        Credential {
            v: self.v,
            access_token: r.access_token.clone(),
            refresh_token: r
                .refresh_token
                .clone()
                .unwrap_or_else(|| self.refresh_token.clone()),
            expires_at,
            scopes: r.scopes.clone().unwrap_or_else(|| self.scopes.clone()),
        }
    }
}

/// Hand-written so `{:?}` on any path can never print a token (plan §5).
impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credential")
            .field("v", &self.v)
            .field("access_token", &redact(&self.access_token))
            .field("refresh_token", &redact(&self.refresh_token))
            .field("expires_at", &self.expires_at)
            .field("scopes", &self.scopes)
            .finish()
    }
}

/// The CLI's own credential: the long-lived token `claude setup-token` mints,
/// stored beside the login credential under `<id>#cli` (SWITCHER §3). It is a
/// **separate grant** with a narrower scope (`user:inference`), so it lives in
/// its own type and never mixes into [`Credential`] — rotating one cannot
/// revoke the other, which is the property the whole switcher rests on.
#[derive(Clone, Serialize, Deserialize)]
pub struct CliToken {
    #[serde(default = "one")]
    pub v: u8,
    pub token: String,
    /// Unix seconds, UTC. `setup-token` prints "valid for 1 year" and hands
    /// back no expiry (SWITCHER Q3), so this is the only age the UI can show.
    pub captured_at: i64,
}

impl CliToken {
    pub fn new(token: impl Into<String>, now: OffsetDateTime) -> CliToken {
        CliToken {
            v: 1,
            token: token.into(),
            captured_at: now.unix_timestamp(),
        }
    }

    pub fn captured_at_utc(&self) -> Option<OffsetDateTime> {
        OffsetDateTime::from_unix_timestamp(self.captured_at).ok()
    }
}

/// Hand-written for the same reason as [`Credential`]'s (plan §5).
impl fmt::Debug for CliToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CliToken")
            .field("v", &self.v)
            .field("token", &redact(&self.token))
            .field("captured_at", &self.captured_at)
            .finish()
    }
}
