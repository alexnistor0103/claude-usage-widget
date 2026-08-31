//! Q1 probe (SWITCHER §8): is the `claude setup-token` grant independent of the
//! login's OAuth grant?
//!
//! Everything here prints **fingerprints only** — a truncated SHA-256 of a
//! token, never the token (plan §5). A fingerprint is enough to answer the
//! question: whether two credentials share a token, and whether a token changed
//! after some other operation, are both equality tests.
//!
//! ```text
//! cargo run -p cuw-daemon --example q1_probe -- fp-keyring <account-id>
//! cargo run -p cuw-daemon --example q1_probe -- fp-file <.credentials.json>
//! cargo run -p cuw-daemon --example q1_probe -- refresh-file <path> [--times N] [--write]
//! ```

use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use cuw_core::client::FetchError;
use cuw_core::refresh::{OAuthTokenClient, TokenRefresher};
use cuw_core::Credential;
use cuw_core::{parse_usage, OAuthUsageClient, UsageSource};
use cuw_creds::{CredentialStore, KeyringStore};
use serde_json::Value;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");

    match cmd {
        "fp-keyring" => {
            let id = args.get(1).context("usage: fp-keyring <account-id>")?;
            let cred = KeyringStore
                .get(id)
                // `%e`: a CredError is never formatted with `?e` (plan §5).
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            report(&format!("keyring:{id}"), &cred);
        }
        "fp-file" => {
            let path = args.get(1).context("usage: fp-file <path>")?;
            let cred = read_cred_file(Path::new(path))?;
            report(&format!("file:{path}"), &cred);
        }
        "check-keyring" => {
            let id = args.get(1).context("usage: check-keyring <account-id>")?;
            let cred = KeyringStore.get(id).map_err(|e| anyhow::anyhow!("{e}"))?;
            check(&format!("keyring:{id}"), &cred).await;
        }
        "check-file" => {
            let path = args.get(1).context("usage: check-file <path>")?;
            let cred = read_cred_file(Path::new(path))?;
            check(&format!("file:{path}"), &cred).await;
        }
        "refresh-file" => {
            let path = PathBuf::from(args.get(1).context("usage: refresh-file <path>")?);
            let times: usize = flag_value(&args, "--times").unwrap_or_else(|| "1".into()).parse()?;
            let write = args.iter().any(|a| a == "--write");
            refresh_file(&path, times, write).await?;
        }
        _ => bail!(
            "commands: fp-keyring <id> | fp-file <path> | check-keyring <id> | \n             check-file <path> | refresh-file <path> [--times N] [--write]"
        ),
    }
    Ok(())
}

/// Rotate the grant behind `path` `times` times, printing what changed each
/// round. Nothing is written back unless `--write` is given, so a dry run
/// leaves the file usable for exactly one more refresh.
async fn refresh_file(path: &Path, times: usize, write: bool) -> anyhow::Result<()> {
    let mut cred = read_cred_file(path)?;
    report("before", &cred);

    let client = OAuthTokenClient::default();
    for round in 1..=times {
        let refreshed = client
            .refresh(&cred.refresh_token)
            .await
            .map_err(|e| anyhow::anyhow!("round {round}: {e}"))?;
        let rotated = cred.rotated(&refreshed, OffsetDateTime::now_utc());
        println!(
            "  round {round}: refresh_token {} → {}  ({})",
            fp(&cred.refresh_token),
            fp(&rotated.refresh_token),
            if rotated.refresh_token == cred.refresh_token {
                "NOT rotated"
            } else {
                "rotated"
            }
        );
        cred = rotated;
    }

    report("after", &cred);
    if write {
        write_cred_file(path, &cred)?;
        println!("  wrote the rotated credential back to {}", path.display());
    } else {
        println!(
            "  (dry run: {} still holds the pre-refresh token)",
            path.display()
        );
    }
    Ok(())
}

/// Is this access token still live? A usage fetch, which reads and rotates
/// nothing — so a credential can be checked without being spent.
async fn check(label: &str, cred: &Credential) {
    let client = OAuthUsageClient::default();
    let verdict = match client.fetch(&cred.access_token).await {
        Ok(raw) => match parse_usage(&raw) {
            Some(u) => format!(
                "ALIVE (5h {:.0}%, 7d {:.0}%)",
                u.five_hour.used_pct, u.seven_day.used_pct
            ),
            None => "ALIVE but the payload did not parse".to_string(),
        },
        Err(FetchError::Unauthorized) => "DEAD (401 unauthorized)".to_string(),
        Err(FetchError::RateLimited) => "inconclusive (rate limited)".to_string(),
        Err(e) => format!("inconclusive ({e})"),
    };
    println!("{label}\n  access {} → {verdict}", fp(&cred.access_token));
}

fn report(label: &str, cred: &Credential) {
    let expiry = cred
        .expires_at_utc()
        .map(|t| t.to_string())
        .unwrap_or_else(|| "unparseable".into());
    println!("{label}");
    println!("  access  {}", fp(&cred.access_token));
    println!("  refresh {}", fp(&cred.refresh_token));
    println!("  expires {expiry}  ({})", relative(cred.expires_at));
    println!("  scopes  {:?}", cred.scopes);
}

/// A stable, non-reversible handle on a token: enough to compare two of them.
fn fp(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let hex: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
    // The prefix is not a secret and tells the token kind apart at a glance.
    let kind: String = token.chars().take(13).collect();
    format!("{kind}… #{hex}")
}

fn relative(expires_at: i64) -> String {
    let secs = expires_at - OffsetDateTime::now_utc().unix_timestamp();
    if secs < 0 {
        return format!("expired {}h ago", -secs / 3600);
    }
    let days = secs / 86_400;
    if days > 0 {
        format!("in {days}d")
    } else {
        format!("in {}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// The CLI's `.credentials.json`, parsed the way the connect flow parses it.
fn read_cred_file(path: &Path) -> anyhow::Result<Credential> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let v: Value = serde_json::from_str(&text).context("parse json")?;
    let o = v
        .get("claudeAiOauth")
        .context("no claudeAiOauth key — is this a CLI credentials file?")?;
    let s = |k: &str| {
        o.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let expires_at = o.get("expiresAt").and_then(Value::as_i64).unwrap_or(0);
    Ok(Credential {
        v: 1,
        access_token: s("accessToken"),
        refresh_token: s("refreshToken"),
        // The CLI writes milliseconds; the daemon's model is seconds.
        expires_at: if expires_at > 100_000_000_000 {
            expires_at / 1000
        } else {
            expires_at
        },
        scopes: o
            .get("scopes")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// Write back in the CLI's own shape, preserving any other keys in the file.
fn write_cred_file(path: &Path, cred: &Credential) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(path)?;
    let mut v: Value = serde_json::from_str(&text)?;
    let o = v.get_mut("claudeAiOauth").context("no claudeAiOauth key")?;
    o["accessToken"] = Value::from(cred.access_token.clone());
    o["refreshToken"] = Value::from(cred.refresh_token.clone());
    o["expiresAt"] = Value::from(cred.expires_at * 1000);
    o["scopes"] = Value::from(cred.scopes.clone());
    std::fs::write(path, serde_json::to_string(&v)?)?;
    Ok(())
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    let i = args.iter().position(|a| a == flag)?;
    args.get(i + 1).cloned()
}
