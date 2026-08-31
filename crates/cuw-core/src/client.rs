use async_trait::async_trait;

/// Raw endpoint response, kept untyped until the defensive parser runs.
pub type RawResponse = serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// 401 → the row shows `reconnect needed`.
    #[error("unauthorized")]
    Unauthorized,
    /// 403 → the token is valid but cannot read usage (wrong OAuth scope).
    /// Retrying cannot fix it, so it is handled like `Unauthorized`.
    #[error("forbidden: {0}")]
    Forbidden(String),
    /// 429 → back off.
    #[error("rate limited")]
    RateLimited,
    #[error("server error: {0}")]
    Server(u16),
    #[error("transport: {0}")]
    Transport(String),
}

/// Source of raw usage data. `OAuthUsageClient` hits the undocumented endpoint;
/// swap the impl wholesale if an official API ships (plan §9).
#[async_trait]
pub trait UsageSource: Send + Sync {
    async fn fetch(&self, token: &str) -> Result<RawResponse, FetchError>;
}

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub struct OAuthUsageClient {
    http: reqwest::Client,
}

impl OAuthUsageClient {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }
}

impl Default for OAuthUsageClient {
    /// A client with the short usage-poll timeout baked in.
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
impl UsageSource for OAuthUsageClient {
    /// GET the usage endpoint with a bare bearer header (S1: no other headers
    /// needed). The token never reaches an error string (plan §5): transport
    /// errors carry only reqwest's own message, which omits request headers.
    async fn fetch(&self, token: &str) -> Result<RawResponse, FetchError> {
        let resp = self
            .http
            .get(USAGE_URL)
            .timeout(TIMEOUT)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| FetchError::Transport(e.to_string()))?;

        let status = resp.status();
        let code = status.as_u16();
        match code {
            200 => {
                let body = resp
                    .bytes()
                    .await
                    .map_err(|e| FetchError::Transport(e.to_string()))?;
                serde_json::from_slice(&body)
                    .map_err(|_| FetchError::Transport("non-JSON body".into()))
            }
            401 => Err(FetchError::Unauthorized),
            403 => {
                let body = resp.bytes().await.unwrap_or_default();
                Err(FetchError::Forbidden(body_snippet(&body)))
            }
            429 => Err(FetchError::RateLimited),
            500..=599 => Err(FetchError::Server(code)),
            other => {
                let body = resp.bytes().await.unwrap_or_default();
                Err(FetchError::Transport(format!(
                    "unexpected status {other}: {}",
                    body_snippet(&body)
                )))
            }
        }
    }
}

/// A short, log-safe view of an error body: the endpoint never echoes the
/// bearer, but cap and flatten it anyway so a surprise cannot spill a secret
/// or a page of HTML into the log.
fn body_snippet(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let flat: String = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(200)
        .collect();
    if flat.is_empty() {
        "<empty body>".into()
    } else {
        flat
    }
}
