use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::redact::redact;

pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
/// The CLI's own public client id (plan §4); used for refresh_token grants only.
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
/// The only 4xx error codes that mean the refresh token itself is dead (plan §4).
pub const REJECT_CODES: [&str; 3] = ["invalid_grant", "invalid_token", "unauthorized_client"];

const TIMEOUT: Duration = Duration::from_secs(15);
const MIN_EXPIRES_IN: f64 = 60.0;
const MAX_EXPIRES_IN: f64 = 2_592_000.0;

#[derive(Debug, thiserror::Error)]
pub enum RefreshError {
    /// 4xx with a whitelisted OAuth error code → `reconnect needed`, never retried.
    #[error("refresh rejected ({0})")]
    Rejected(u16),
    /// Any other 4xx: a WAF or encoding mismatch must not become a reconnect loop.
    #[error("token endpoint contract changed ({0})")]
    Contract(u16),
    #[error("rate limited")]
    RateLimited,
    #[error("server error: {0}")]
    Server(u16),
    #[error("transport: {0}")]
    Transport(String),
    /// 200 but unparseable.
    #[error("unexpected token response shape")]
    BadShape,
}

/// A parsed token response. `expires_in` is clamped to 60 s ..= 30 d.
#[derive(Clone)]
pub struct Refreshed {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: Duration,
    pub scopes: Option<Vec<String>>,
}

impl fmt::Debug for Refreshed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Refreshed")
            .field("access_token", &redact(&self.access_token))
            .field("refresh_token", &self.refresh_token.as_deref().map(redact))
            .field("expires_in", &self.expires_in)
            .field("scopes", &self.scopes)
            .finish()
    }
}

/// Exchanges a refresh token for a fresh access token. Behind a trait for the
/// same reason as `UsageSource`: the endpoint is undocumented (plan §9).
#[async_trait]
pub trait TokenRefresher: Send + Sync {
    async fn refresh(&self, refresh_token: &str) -> Result<Refreshed, RefreshError>;
}

pub struct OAuthTokenClient {
    http: reqwest::Client,
}

impl OAuthTokenClient {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }
}

impl Default for OAuthTokenClient {
    fn default() -> Self {
        // build() only fails on a bad TLS backend, which is a setup invariant.
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(concat!("cuw/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();
        Self { http }
    }
}

#[async_trait]
impl TokenRefresher for OAuthTokenClient {
    /// POST the refresh grant as JSON with no Authorization header (encoding
    /// inferred — plan §8 Q8). The request carries the refresh token, so a 4xx
    /// body could echo it: it is reduced to a whitelisted error code and
    /// dropped, never formatted or stored.
    async fn refresh(&self, refresh_token: &str) -> Result<Refreshed, RefreshError> {
        let resp = self
            .http
            .post(TOKEN_URL)
            .timeout(TIMEOUT)
            .json(&serde_json::json!({
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
                "client_id": CLIENT_ID,
            }))
            .send()
            .await
            .map_err(|e| RefreshError::Transport(e.to_string()))?;

        let code = resp.status().as_u16();
        match code {
            200 => {
                let body = resp
                    .bytes()
                    .await
                    .map_err(|e| RefreshError::Transport(e.to_string()))?;
                let raw: Value =
                    serde_json::from_slice(&body).map_err(|_| RefreshError::BadShape)?;
                parse_refresh(&raw).ok_or(RefreshError::BadShape)
            }
            400 | 401 | 403 => {
                let body = resp.bytes().await.unwrap_or_default();
                match error_code(&body) {
                    Some(_) => Err(RefreshError::Rejected(code)),
                    None => Err(RefreshError::Contract(code)),
                }
            }
            429 => Err(RefreshError::RateLimited),
            500..=599 => Err(RefreshError::Server(code)),
            other => Err(RefreshError::Transport(format!(
                "unexpected status {other}"
            ))),
        }
    }
}

/// Turn the untyped token response into `Refreshed`. Any miss on a required
/// field yields `None` → `BadShape`; a token with unknown lifetime cannot be
/// scheduled, so a missing `expires_in` is a miss too.
pub fn parse_refresh(raw: &Value) -> Option<Refreshed> {
    let access_token = non_empty(raw.get("access_token"))?;
    let secs = raw.get("expires_in").and_then(Value::as_f64)?;
    if secs.is_nan() || secs < 0.0 {
        return None;
    }
    // try_from_secs_f64: never the panicking from_secs_f64 on a remote payload.
    let expires_in =
        Duration::try_from_secs_f64(secs.clamp(MIN_EXPIRES_IN, MAX_EXPIRES_IN)).ok()?;
    let refresh_token = non_empty(raw.get("refresh_token"));
    let scopes = match raw.get("scope").and_then(Value::as_str) {
        Some(s) => Some(s.split_whitespace().map(str::to_owned).collect()),
        None => raw.get("scopes").and_then(Value::as_array).map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        }),
    };
    Some(Refreshed {
        access_token,
        refresh_token,
        expires_in,
        scopes,
    })
}

fn non_empty(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// A whitelisted OAuth `error` code from a 4xx body, or None. Never returns
/// anything else from the body.
pub fn error_code(bytes: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(bytes)
        .ok()?
        .get("error")?
        .as_str()
        .filter(|c| REJECT_CODES.contains(c))
        .map(str::to_owned)
}
