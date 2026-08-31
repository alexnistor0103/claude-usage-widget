//! Release check: asks GitHub whether a newer tag than this build exists.
//!
//! Every failure — offline, rate-limited, no release published yet, a shape we
//! don't recognise — is the same display state: no update shown (plan §9). The
//! command therefore returns a value, never an error the UI has to render.

use std::time::Duration;

use serde::Serialize;
use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;

const OWNER_REPO: &str = "alexnistor0103/claude-usage-widget";

/// GitHub rejects an unidentified client, so the UA is not optional.
const CLIENT_UA: &str = concat!("cuw-overlay/", env!("CARGO_PKG_VERSION"));

/// Short: this runs on startup, and a slow answer is worth less than a fast
/// "nothing to show".
const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Default, PartialEq, Serialize)]
pub struct UpdateInfo {
    available: bool,
    latest: Option<String>,
    url: Option<String>,
}

#[tauri::command]
pub async fn check_update() -> UpdateInfo {
    let Some((tag, url)) = latest_release().await else {
        return UpdateInfo::default();
    };
    compare(env!("CARGO_PKG_VERSION"), &tag, &url)
}

/// Open the release page. Its own allowlist rather than `open_url`'s: that one
/// gates sign-in hosts, and this is the only place a github.com URL is allowed.
#[tauri::command]
pub fn open_release(app: AppHandle, url: String) -> Result<(), String> {
    if !release_url_allowed(&url) {
        return Err("refusing to open that url".into());
    }
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())
}

/// `(tag, page url)` from the latest release, or `None` for anything else —
/// a 404 while no release has ever been published included.
async fn latest_release() -> Option<(String, String)> {
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .user_agent(CLIENT_UA)
        .build()
        .ok()?;
    let res = client
        .get(format!(
            "https://api.github.com/repos/{OWNER_REPO}/releases/latest"
        ))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .ok()?;
    if !res.status().is_success() {
        return None;
    }
    let body: serde_json::Value = res.json().await.ok()?;
    let tag = body.get("tag_name")?.as_str()?.to_string();
    let url = body
        .get("html_url")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| format!("https://github.com/{OWNER_REPO}/releases/latest"));
    Some((tag, url))
}

/// Split out from the request so the version rule is testable offline.
fn compare(current: &str, tag: &str, url: &str) -> UpdateInfo {
    let latest = strip_v(tag);
    let (Some(have), Some(there)) = (triple(current), triple(latest)) else {
        return UpdateInfo::default();
    };
    if there <= have {
        return UpdateInfo::default();
    }
    UpdateInfo {
        available: true,
        latest: Some(latest.to_string()),
        url: Some(url.to_string()),
    }
}

fn strip_v(tag: &str) -> &str {
    let t = tag.trim();
    t.strip_prefix('v')
        .or_else(|| t.strip_prefix('V'))
        .unwrap_or(t)
}

/// A dotted numeric triple and nothing else: a pre-release or build suffix
/// parses to `None`, which reads as "no update" rather than a guess.
fn triple(v: &str) -> Option<(u64, u64, u64)> {
    let mut parts = v.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

fn release_url_allowed(url: &str) -> bool {
    let prefix = format!("https://github.com/{OWNER_REPO}/releases");
    let Some(rest) = url.strip_prefix(&prefix) else {
        return false;
    };
    // Only the page itself or a path under it — never `…/releasesfoo`.
    rest.is_empty() || rest.starts_with('/')
}

#[cfg(test)]
mod tests {
    use super::{compare, release_url_allowed, triple, UpdateInfo};

    const URL: &str = "https://github.com/alexnistor0103/claude-usage-widget/releases/tag/v9.0.0";

    #[test]
    fn newer_tag_is_offered_without_its_v() {
        assert_eq!(
            compare("0.1.0", "v0.2.0", URL),
            UpdateInfo {
                available: true,
                latest: Some("0.2.0".into()),
                url: Some(URL.into()),
            }
        );
    }

    #[test]
    fn same_or_older_tag_is_not_an_update() {
        for tag in ["v0.1.0", "0.1.0", "v0.0.9", "v0.1.0  "] {
            assert_eq!(compare("0.1.0", tag, URL), UpdateInfo::default(), "{tag}");
        }
    }

    #[test]
    fn versions_compare_numerically_not_lexically() {
        assert!(compare("0.9.0", "v0.10.0", URL).available);
        assert!(!compare("0.10.0", "v0.9.0", URL).available);
        assert!(compare("1.2.3", "v2.0.0", URL).available);
    }

    #[test]
    fn an_unparseable_version_means_no_update() {
        for tag in [
            "",
            "v",
            "latest",
            "v1.2",
            "v1.2.3.4",
            "v1.2.3-rc1",
            "v1.x.0",
        ] {
            assert_eq!(compare("0.1.0", tag, URL), UpdateInfo::default(), "{tag}");
        }
        assert_eq!(compare("nightly", "v9.9.9", URL), UpdateInfo::default());
    }

    #[test]
    fn triple_parses_plain_numbers_only() {
        assert_eq!(triple("1.2.3"), Some((1, 2, 3)));
        assert_eq!(triple("0.0.0"), Some((0, 0, 0)));
        assert_eq!(triple(" 1.2.3"), None);
        assert_eq!(triple("-1.2.3"), None);
    }

    #[test]
    fn only_this_repos_release_pages_open() {
        for url in [
            "https://github.com/alexnistor0103/claude-usage-widget/releases",
            "https://github.com/alexnistor0103/claude-usage-widget/releases/latest",
            "https://github.com/alexnistor0103/claude-usage-widget/releases/tag/v1.0.0",
        ] {
            assert!(release_url_allowed(url), "{url}");
        }
        for url in [
            "http://github.com/alexnistor0103/claude-usage-widget/releases",
            "https://github.com/alexnistor0103/claude-usage-widget/releasesfoo",
            "https://github.com/alexnistor0103/claude-usage-widget/issues",
            "https://github.com/someone/else/releases",
            "https://evil.example/github.com/alexnistor0103/claude-usage-widget/releases",
            "javascript:alert(1)",
            "",
        ] {
            assert!(!release_url_allowed(url), "{url}");
        }
    }
}
