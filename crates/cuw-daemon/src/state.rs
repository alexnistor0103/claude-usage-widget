//! Shared display state and its serde wire shape (M2.1, M1b.6). The poll loop
//! writes `Row`s; the HTTP layer reads them. The wire shape is defined once here
//! and matches what `apps/overlay/src/main.js` renders.

use std::collections::HashMap;
use std::sync::Arc;

use cuw_core::model::AccountState;
use serde::Serialize;
use serde_json::Value;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use tokio::sync::{broadcast, RwLock};

use crate::poll::RefreshStatus;

/// One account's display row. Never holds a token — the token lives only in the
/// keyring and the poll task's stack (plan §5).
#[derive(Clone)]
pub struct Row {
    pub label: String,
    pub state: AccountState,
    pub connected_at: OffsetDateTime,
    /// When the current access token expires, mirrored from the credential the
    /// poll task holds.
    pub access_expires_at: Option<OffsetDateTime>,
    pub refreshed_at: Option<OffsetDateTime>,
    pub refresh: RefreshStatus,
    /// Last-good numbers held without a fresh 200 (plan §3).
    pub stale: bool,
    pub last_ok_at: Option<OffsetDateTime>,
    /// A rotated credential could not be written to the OS store; the row keeps
    /// working from memory but needs a reconnect after a restart.
    pub persist_pending: bool,
    /// The CLI's `oauthAccount` block for this login, when the connect caught
    /// it: uuid, email, org — identity, never a secret. A switch writes it
    /// into the CLI's config, and the poll loop keys on its uuid to tell a
    /// token the CLI rotated from another account's.
    pub account: Option<Value>,
}

impl Row {
    /// A fresh row: nothing fetched, nothing refreshed, nothing pending.
    pub fn new(label: String, state: AccountState, connected_at: OffsetDateTime) -> Row {
        Row {
            label,
            state,
            connected_at,
            access_expires_at: None,
            refreshed_at: None,
            refresh: RefreshStatus::default(),
            stale: false,
            last_ok_at: None,
            persist_pending: false,
            account: None,
        }
    }

    /// The account uuid, if the identity block was captured.
    pub fn account_uuid(&self) -> Option<&str> {
        self.account.as_ref().and_then(cuw_switch::account_uuid)
    }

    /// The account email, for the overlay's tooltip.
    pub fn email(&self) -> Option<&str> {
        self.account
            .as_ref()
            .and_then(|a| a.get("emailAddress"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    }
}

/// Map read by HTTP, written by the poll tasks. Held briefly, never across a
/// long await.
pub type SharedRows = Arc<RwLock<HashMap<String, Row>>>;

/// A single server-sent event: a named frame carrying a JSON string. Broadcast
/// fans it out to every `/events` subscriber.
#[derive(Clone)]
pub struct SseMsg {
    pub event: &'static str,
    pub data: String,
}

pub type Events = broadcast::Sender<SseMsg>;

/// One per-model weekly window (plan §8 Q3). `is_active` marks the currently
/// binding limit, not whether the window exists.
#[derive(Serialize)]
pub struct WireScoped {
    pub name: String,
    pub pct: i64,
    pub resets_at: Option<String>,
    pub is_active: bool,
}

/// The serde wire shape for one account (M1b.6). The four usage fields appear
/// only when the state is `available`; everything below them is always present
/// so the overlay can explain *why* a row is not showing numbers.
#[derive(Serialize)]
pub struct WireAccount {
    pub id: String,
    pub label: String,
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub five_hour: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seven_day: Option<i64>,
    // Present-but-null when available with no reset time; absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seven_day_resets_at: Option<Option<String>>,
    pub stale: bool,
    pub fetched_at: Option<String>,
    pub scoped: Vec<WireScoped>,
    pub access_expires_at: Option<String>,
    pub refreshed_at: Option<String>,
    pub refresh: &'static str,
    pub persist_pending: bool,
    /// The CLI's store currently holds this account: every `claude` is
    /// signed in as it.
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

fn fmt(dt: OffsetDateTime) -> Option<String> {
    dt.format(&Rfc3339).ok()
}

/// Parse an RFC3339 timestamp; a bad value is not fatal (caller substitutes).
pub fn parse_rfc3339(s: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(s, &Rfc3339).ok()
}

/// Project a `Row` into its wire form. Percentages are rounded to integers to
/// match the overlay's `${a.five_hour}%` rendering.
pub fn to_wire(id: &str, row: &Row, active: bool) -> WireAccount {
    let (state, five_hour, seven_day, resets_at, seven_day_resets_at, scoped) = match &row.state {
        AccountState::Available(u) => (
            "available",
            Some(u.five_hour.used_pct.round() as i64),
            Some(u.seven_day.used_pct.round() as i64),
            Some(u.five_hour.resets_at.and_then(fmt)),
            Some(u.seven_day.resets_at.and_then(fmt)),
            u.scoped
                .iter()
                .map(|s| WireScoped {
                    name: s.name.clone(),
                    pct: s.used_pct.round() as i64,
                    resets_at: s.resets_at.and_then(fmt),
                    is_active: s.is_active,
                })
                .collect(),
        ),
        AccountState::Unavailable => ("unavailable", None, None, None, None, Vec::new()),
        AccountState::ReconnectNeeded => ("reconnect needed", None, None, None, None, Vec::new()),
    };
    WireAccount {
        id: id.to_string(),
        label: row.label.clone(),
        state,
        five_hour,
        seven_day,
        resets_at,
        seven_day_resets_at,
        // There are no kept numbers to be stale about unless we show numbers.
        stale: row.stale && matches!(row.state, AccountState::Available(_)),
        fetched_at: row.last_ok_at.and_then(fmt),
        scoped,
        access_expires_at: row.access_expires_at.and_then(fmt),
        refreshed_at: row.refreshed_at.and_then(fmt),
        refresh: row.refresh.as_wire(),
        persist_pending: row.persist_pending,
        active,
        email: row.email().map(str::to_string),
    }
}

/// Every account, oldest connection first, as the wire array `GET /accounts`
/// returns and `/events` pushes. `active` is the id the CLI's store holds.
pub fn snapshot(rows: &HashMap<String, Row>, active: Option<&str>) -> Vec<WireAccount> {
    let mut out: Vec<(OffsetDateTime, WireAccount)> = rows
        .iter()
        .map(|(id, row)| {
            (
                row.connected_at,
                to_wire(id, row, active == Some(id.as_str())),
            )
        })
        .collect();
    out.sort_by_key(|(t, _)| *t);
    out.into_iter().map(|(_, w)| w).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_carries_refresh_fields_and_no_secret() {
        let connected_at = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let row = Row::new("Work".into(), AccountState::Unavailable, connected_at);
        let text = serde_json::to_string(&to_wire("work-abc12345", &row, false)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();

        for key in [
            "stale",
            "fetched_at",
            "scoped",
            "access_expires_at",
            "refreshed_at",
            "refresh",
            "persist_pending",
            "active",
        ] {
            assert!(v.get(key).is_some(), "{key} must always be present");
        }
        assert_eq!(v["refresh"], "ok");
        assert_eq!(v["active"], false);
        assert!(v.get("email").is_none(), "no identity block, no email key");
        assert_eq!(v["stale"], false);
        assert_eq!(v["persist_pending"], false);
        assert!(v["scoped"].is_array());
        assert!(v.get("expires_at").is_none(), "the 365-day expiry is gone");
        assert!(!text.contains("sk-ant"), "no token may reach the wire");
    }

    #[test]
    fn stale_is_never_true_without_numbers() {
        let connected_at = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let mut row = Row::new("Work".into(), AccountState::Unavailable, connected_at);
        row.stale = true;
        assert!(!to_wire("id", &row, false).stale);
    }

    #[test]
    fn the_identity_block_yields_uuid_and_email_and_marks_the_active_row() {
        let connected_at = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let mut row = Row::new("Work".into(), AccountState::Unavailable, connected_at);
        row.account = Some(serde_json::json!({
            "accountUuid": "u-1", "emailAddress": "a@example.com", "organizationRole": "admin"
        }));
        assert_eq!(row.account_uuid(), Some("u-1"));
        assert_eq!(row.email(), Some("a@example.com"));

        let mut rows = HashMap::new();
        rows.insert("work-abc12345".to_string(), row);
        let wire = snapshot(&rows, Some("work-abc12345"));
        assert!(wire[0].active);
        assert_eq!(wire[0].email.as_deref(), Some("a@example.com"));
        let wire = snapshot(&rows, Some("someone-else"));
        assert!(!wire[0].active);
    }
}
