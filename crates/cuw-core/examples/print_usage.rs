//! M0 demo: fetch and print one account's live usage (plan §7, M0.5).
//!
//! `CUW_TOKEN=<token> cargo run -p cuw-core --example print_usage`

use cuw_core::client::FetchError;
use cuw_core::{parse_usage, redact, OAuthUsageClient, UsageSource};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let token = match std::env::var("CUW_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ => {
            eprintln!("set CUW_TOKEN to a Claude OAuth token");
            std::process::exit(2);
        }
    };

    let client = OAuthUsageClient::default();
    match client.fetch(&token).await {
        Ok(raw) => match parse_usage(&raw) {
            Some(usage) => {
                print_window("5h", &usage.five_hour);
                print_window("7d", &usage.seven_day);
            }
            None => println!("unavailable"),
        },
        Err(FetchError::Unauthorized) => println!("reconnect needed (unauthorized)"),
        Err(FetchError::RateLimited) => println!("rate limited"),
        // Redact so the token never rides along in an error path (plan §5).
        Err(e) => println!("unavailable ({e}) [token {}]", redact(&token)),
    }
}

fn print_window(label: &str, w: &cuw_core::Window) {
    match w.resets_at {
        Some(ts) => println!("{label}: {:.0}%  (resets {ts})", w.used_pct),
        None => println!("{label}: {:.0}%  (resets unknown)", w.used_pct),
    }
}
