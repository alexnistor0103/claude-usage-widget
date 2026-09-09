use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use time::OffsetDateTime;

use crate::redact::redact;
use crate::refresh::Refreshed;

fn one() -> u8 {
    1
}

/// The per-account blob kept in the OS credential store (plan §5). `v` exists
/// so a future shape change is detected instead of misparsed.
///
/// The three optional fields are what the CLI keeps beside the tokens in its
/// own store. They ride along so a switch writes back exactly what a login
/// would have; an older blob without them still loads.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credential {
    #[serde(default = "one")]
    pub v: u8,
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds, UTC.
    pub expires_at: i64,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_tier: Option<String>,
    /// Unix seconds, UTC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token_expires_at: Option<i64>,
}

impl Credential {
    /// The scope the usage endpoint demands (S1: 403 without it).
    pub const REQUIRED_SCOPE: &'static str = "user:profile";

    pub fn new(
        access_token: impl Into<String>,
        refresh_token: impl Into<String>,
        expires_at: i64,
        scopes: Vec<String>,
    ) -> Credential {
        Credential {
            v: 1,
            access_token: access_token.into(),
            refresh_token: refresh_token.into(),
            expires_at,
            scopes,
            subscription_type: None,
            rate_limit_tier: None,
            refresh_token_expires_at: None,
        }
    }

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
            subscription_type: self.subscription_type.clone(),
            rate_limit_tier: self.rate_limit_tier.clone(),
            // A rotated refresh token has an expiry of its own we were not
            // told; the old one would be a lie about the new token.
            refresh_token_expires_at: if r.refresh_token.is_some() {
                None
            } else {
                self.refresh_token_expires_at
            },
        }
    }

    /// Whether two credentials descend from one login: a refresh rotates the
    /// access token and may rotate the refresh token, but never both at once
    /// relative to the copy it started from.
    pub fn same_lineage(&self, other: &Credential) -> bool {
        self.access_token == other.access_token || self.refresh_token == other.refresh_token
    }

    /// Field-by-field parse of the CLI's own store — `.credentials.json`, or
    /// the same JSON out of the Keychain. The shape is undocumented, so nothing
    /// is trusted (plan §4): a missing token is `None`, an odd extra is dropped.
    /// `expiresAt` is a millisecond epoch today; seconds are accepted too.
    pub fn from_cli_json(v: &Value) -> Option<Credential> {
        let o = v.get("claudeAiOauth")?;
        let access = non_empty(o.get("accessToken"))?;
        let refresh = non_empty(o.get("refreshToken"))?;
        let expires_at = o.get("expiresAt").and_then(Value::as_f64).map(epoch_secs)?;
        let scopes = o
            .get("scopes")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        Some(Credential {
            v: 1,
            access_token: access.into(),
            refresh_token: refresh.into(),
            expires_at,
            scopes,
            subscription_type: non_empty(o.get("subscriptionType")).map(str::to_string),
            rate_limit_tier: non_empty(o.get("rateLimitTier")).map(str::to_string),
            refresh_token_expires_at: o
                .get("refreshTokenExpiresAt")
                .and_then(Value::as_f64)
                .map(epoch_secs),
        })
    }

    /// The inverse of [`Credential::from_cli_json`]: what a switch writes into
    /// the CLI's store, in the shape the CLI's own login leaves there.
    pub fn to_cli_json(&self) -> Value {
        let mut o = serde_json::Map::new();
        o.insert("accessToken".into(), json!(self.access_token));
        o.insert("refreshToken".into(), json!(self.refresh_token));
        o.insert(
            "expiresAt".into(),
            json!(self.expires_at.saturating_mul(1000)),
        );
        o.insert("scopes".into(), json!(self.scopes));
        if let Some(t) = &self.subscription_type {
            o.insert("subscriptionType".into(), json!(t));
        }
        if let Some(t) = &self.rate_limit_tier {
            o.insert("rateLimitTier".into(), json!(t));
        }
        if let Some(t) = self.refresh_token_expires_at {
            o.insert(
                "refreshTokenExpiresAt".into(),
                json!(t.saturating_mul(1000)),
            );
        }
        json!({ "claudeAiOauth": Value::Object(o) })
    }
}

fn non_empty(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Milliseconds or seconds, whichever the value plainly is.
fn epoch_secs(raw: f64) -> i64 {
    if raw > 1e11 {
        (raw / 1000.0) as i64
    } else {
        raw as i64
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
            .field("subscription_type", &self.subscription_type)
            .field("rate_limit_tier", &self.rate_limit_tier)
            .field("refresh_token_expires_at", &self.refresh_token_expires_at)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLI_JSON: &str = r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-FAKEFAKEFAKEFAKEFAKEFAKE0001","refreshToken":"sk-ant-ort01-FAKEFAKEFAKEFAKEFAKEFAKE0002","expiresAt":1756600000000,"refreshTokenExpiresAt":1759000000000,"scopes":["user:inference","user:profile"],"subscriptionType":"max","rateLimitTier":"default_claude_max_5x"}}"#;

    fn parse(s: &str) -> Option<Credential> {
        Credential::from_cli_json(&serde_json::from_str(s).expect("json"))
    }

    #[test]
    fn cli_json_round_trips_including_the_extras() {
        let c = parse(CLI_JSON).expect("parses");
        assert_eq!(c.expires_at, 1_756_600_000);
        assert_eq!(c.refresh_token_expires_at, Some(1_759_000_000));
        assert_eq!(c.subscription_type.as_deref(), Some("max"));
        assert_eq!(c.rate_limit_tier.as_deref(), Some("default_claude_max_5x"));
        assert!(c.has_usage_scope());

        let back = c.to_cli_json();
        let expected: Value = serde_json::from_str(CLI_JSON).unwrap();
        assert_eq!(back, expected);
    }

    #[test]
    fn a_seconds_expiry_and_missing_extras_still_parse() {
        let c = parse(
            r#"{"claudeAiOauth":{"accessToken":"a","refreshToken":"r","expiresAt":1756600000}}"#,
        )
        .expect("parses");
        assert_eq!(c.expires_at, 1_756_600_000);
        assert!(c.scopes.is_empty());
        assert_eq!(c.subscription_type, None);
        let o = &c.to_cli_json()["claudeAiOauth"];
        assert!(o.get("subscriptionType").is_none(), "no invented extras");
        assert!(o.get("refreshTokenExpiresAt").is_none());
    }

    #[test]
    fn a_missing_or_empty_token_is_not_a_credential() {
        assert!(parse(r#"{"claudeAiOauth":{"accessToken":"a","expiresAt":1}}"#).is_none());
        assert!(
            parse(r#"{"claudeAiOauth":{"accessToken":"","refreshToken":"r","expiresAt":1}}"#)
                .is_none()
        );
        assert!(parse(r#"{"claudeAiOauth":{"accessToken":"a","refreshToken":"r"}}"#).is_none());
        assert!(parse(r#"{}"#).is_none());
    }

    #[test]
    fn an_old_blob_without_the_extras_loads_and_rotates_them_forward() {
        let old: Credential = serde_json::from_str(
            r#"{"v":1,"access_token":"a","refresh_token":"r","expires_at":1,"scopes":[]}"#,
        )
        .unwrap();
        assert_eq!(old.subscription_type, None);
        let mut c = parse(CLI_JSON).unwrap();
        c.subscription_type = Some("max".into());
        let r = Refreshed {
            access_token: "sk-ant-oat01-NEW".into(),
            refresh_token: None,
            expires_in: std::time::Duration::from_secs(3600),
            scopes: None,
        };
        let next = c.rotated(&r, OffsetDateTime::from_unix_timestamp(1_000).unwrap());
        assert_eq!(next.subscription_type.as_deref(), Some("max"));
        assert_eq!(next.refresh_token, c.refresh_token);
        assert_eq!(next.refresh_token_expires_at, c.refresh_token_expires_at);
        assert_eq!(next.expires_at, 4_600);
        assert!(next.same_lineage(&c));

        let both = Refreshed {
            refresh_token: Some("sk-ant-ort01-NEW".into()),
            ..r
        };
        let next = c.rotated(&both, OffsetDateTime::from_unix_timestamp(1_000).unwrap());
        assert_eq!(next.refresh_token_expires_at, None);
    }

    #[test]
    fn lineage_survives_one_rotation_but_not_a_new_login() {
        let a = Credential::new("acc-1", "ref-1", 1, vec![]);
        let rotated_access = Credential::new("acc-2", "ref-1", 1, vec![]);
        let rotated_both = Credential::new("acc-2", "ref-2", 1, vec![]);
        assert!(a.same_lineage(&rotated_access));
        assert!(!a.same_lineage(&rotated_both));
    }

    #[test]
    fn debug_never_prints_a_token() {
        let c = parse(CLI_JSON).unwrap();
        let s = format!("{c:?}");
        assert!(!s.contains("FAKE"), "{s}");
        assert!(s.contains("max"));
    }
}
