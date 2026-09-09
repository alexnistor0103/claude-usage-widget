//! Router tests against an in-memory `UsageSource` and a pre-seeded state, driven
//! with `tower::ServiceExt::oneshot` — no real socket, no network (M2).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{header::AUTHORIZATION, Request, StatusCode};
use cuw_core::client::{FetchError, RawResponse, UsageSource};
use cuw_core::model::{AccountState, Usage, Window};
use cuw_core::refresh::{RefreshError, Refreshed, TokenRefresher};
use cuw_core::Credential;
use cuw_creds::{CredError, CredentialStore};
use cuw_daemon::http::{
    after_failed_reconnect, router, seed_delay, spawn_poll_task, AppState, ReconnectFallback,
    SharedSource,
};
use cuw_daemon::state::Row;
use cuw_switch::{CliStore, SwitchError};
use serde_json::Value;
use time::OffsetDateTime;
use tokio::sync::{broadcast, mpsc, Mutex, Notify, RwLock};
use tower::ServiceExt;

const BEARER: &str = "test-bearer-token";
const SECRET_TOKEN: &str = "sk-ant-oat01-FAKEFAKEFAKEFAKEFAKEFAKE0005";
const SECRET_REFRESH: &str = "sk-ant-ort01-FAKEFAKEFAKEFAKEFAKEFAKE0006";
const ROTATED_TOKEN: &str = "sk-ant-oat01-FAKEROTATED0000000000000007";
/// What some other login left in the CLI's store.
const FOREIGN_TOKEN: &str = "sk-ant-oat01-FAKEFOREIGN000000000000000008";
const FOREIGN_REFRESH: &str = "sk-ant-ort01-FAKEFOREIGN000000000000000009";

/// Scripted usage endpoint: replies pop off the queue; an empty queue answers
/// the canonical good body. Counts every call.
#[derive(Default)]
struct FakeSource {
    calls: AtomicUsize,
    replies: std::sync::Mutex<VecDeque<Result<RawResponse, FetchError>>>,
}

impl FakeSource {
    fn script(&self, reply: Result<RawResponse, FetchError>) {
        self.replies.lock().unwrap().push_back(reply);
    }
}

fn good_body() -> RawResponse {
    serde_json::json!({
        "five_hour": { "utilization": 31.0, "resets_at": null },
        "seven_day": { "utilization": 14.0, "resets_at": null }
    })
}

#[async_trait::async_trait]
impl UsageSource for FakeSource {
    async fn fetch(&self, _token: &str) -> Result<RawResponse, FetchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(good_body()))
    }
}

/// Scripted token endpoint: replies pop off the queue; an empty queue rotates
/// to `ROTATED_TOKEN` with an 8 h lifetime. Counts every call.
#[derive(Default)]
struct FakeRefresher {
    calls: AtomicUsize,
    replies: std::sync::Mutex<VecDeque<Result<Refreshed, RefreshError>>>,
}

impl FakeRefresher {
    fn script(&self, reply: Result<Refreshed, RefreshError>) {
        self.replies.lock().unwrap().push_back(reply);
    }
}

fn rotated() -> Refreshed {
    Refreshed {
        access_token: ROTATED_TOKEN.into(),
        refresh_token: None,
        expires_in: std::time::Duration::from_secs(28_800),
        scopes: None,
    }
}

#[async_trait::async_trait]
impl TokenRefresher for FakeRefresher {
    async fn refresh(&self, _refresh_token: &str) -> Result<Refreshed, RefreshError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(rotated()))
    }
}

/// In-memory credential store so tests never touch the real OS keyring. A
/// seeded `Err(())` stands in for an unreadable blob and reads back as
/// `CredError::Corrupt`.
#[derive(Default)]
struct MemStore {
    creds: std::sync::Mutex<HashMap<String, Result<Credential, ()>>>,
}

impl MemStore {
    fn seed_corrupt(&self, id: &str) {
        self.creds.lock().unwrap().insert(id.into(), Err(()));
    }
}

impl CredentialStore for MemStore {
    fn put(&self, id: &str, cred: &Credential) -> Result<(), CredError> {
        self.creds
            .lock()
            .unwrap()
            .insert(id.into(), Ok(cred.clone()));
        Ok(())
    }
    fn get(&self, id: &str) -> Result<Credential, CredError> {
        match self.creds.lock().unwrap().get(id) {
            Some(Ok(cred)) => Ok(cred.clone()),
            Some(Err(())) => Err(CredError::Corrupt(id.into())),
            None => Err(CredError::NotFound(id.into())),
        }
    }
    fn delete(&self, id: &str) -> Result<(), CredError> {
        self.creds.lock().unwrap().remove(id);
        Ok(())
    }
    fn delete_cli(&self, id: &str) -> Result<(), CredError> {
        Err(CredError::NotFound(id.into()))
    }
}

/// The CLI's own store, in memory: what a switch writes and what the poll
/// loop reconciles against. Scripted to fail so the refusal path is covered.
#[derive(Default)]
struct FakeCli {
    cred: std::sync::Mutex<Option<Credential>>,
    account: std::sync::Mutex<Option<Value>>,
    writes: AtomicUsize,
    fail: std::sync::atomic::AtomicBool,
}

impl FakeCli {
    fn seed(&self, cred: Credential, account: Option<Value>) {
        *self.cred.lock().unwrap() = Some(cred);
        *self.account.lock().unwrap() = account;
    }
    fn held(&self) -> Option<Credential> {
        self.cred.lock().unwrap().clone()
    }
    fn held_account(&self) -> Option<Value> {
        self.account.lock().unwrap().clone()
    }
    fn script_failure(&self) {
        self.fail.store(true, Ordering::SeqCst);
    }
}

impl CliStore for FakeCli {
    fn read(&self) -> Result<Option<Credential>, SwitchError> {
        Ok(self.cred.lock().unwrap().clone())
    }
    fn write(&self, cred: &Credential) -> Result<(), SwitchError> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            return Err(SwitchError::Write("no keychain in tests".into()));
        }
        *self.cred.lock().unwrap() = Some(cred.clone());
        Ok(())
    }
    fn read_account(&self) -> Option<Value> {
        self.account.lock().unwrap().clone()
    }
    fn write_account(&self, account: Option<&Value>) -> Result<(), SwitchError> {
        *self.account.lock().unwrap() = account.cloned();
        Ok(())
    }
}

fn identity(uuid: &str, email: &str) -> Value {
    serde_json::json!({ "accountUuid": uuid, "emailAddress": email, "seatTier": null })
}

fn fake_credential() -> Credential {
    Credential::new(
        SECRET_TOKEN,
        SECRET_REFRESH,
        1_756_600_000,
        vec!["user:inference".into(), "user:profile".into()],
    )
}

fn available(five: f32, seven: f32) -> AccountState {
    AccountState::Available(Usage {
        five_hour: Window {
            used_pct: five,
            resets_at: None,
        },
        seven_day: Window {
            used_pct: seven,
            resets_at: None,
        },
        scoped: Vec::new(),
    })
}

/// The app plus handles to its fakes, so tests can script and count.
struct Harness {
    app: AppState,
    source: Arc<FakeSource>,
    refresher: Arc<FakeRefresher>,
    store: Arc<MemStore>,
    cli: Arc<FakeCli>,
}

fn harness_with_rows(rows: HashMap<String, Row>) -> Harness {
    let source = Arc::new(FakeSource::default());
    let refresher = Arc::new(FakeRefresher::default());
    let store = Arc::new(MemStore::default());
    let cli = Arc::new(FakeCli::default());

    let (events, _) = broadcast::channel(16);
    let tmp = std::env::temp_dir().join(format!("cuw-test-registry-{}.toml", std::process::id()));
    let scratch = std::env::temp_dir().join(format!("cuw-test-scratch-{}", std::process::id()));

    let app = AppState {
        rows: Arc::new(RwLock::new(rows)),
        source: SharedSource(source.clone()),
        store: store.clone(),
        bearer: Arc::new(BEARER.into()),
        events,
        tasks: Arc::new(Mutex::new(HashMap::new())),
        registry_path: Arc::new(tmp),
        scratch_root: Arc::new(scratch),
        registry_lock: Arc::new(Mutex::new(())),
        connect_input: Arc::new(Mutex::new(None)),
        refresher: refresher.clone(),
        shutdown: Arc::new(Notify::new()),
        connect_task: Arc::new(Mutex::new(None)),
        cli: cli.clone(),
        active: Arc::new(RwLock::new(None)),
        cli_lock: Arc::new(Mutex::new(())),
    };
    Harness {
        app,
        source,
        refresher,
        store,
        cli,
    }
}

fn harness() -> Harness {
    let mut rows = HashMap::new();
    rows.insert(
        "work-abc12345".to_string(),
        Row::new(
            "Work".into(),
            available(31.0, 14.0),
            OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
        ),
    );
    rows.insert(
        "home-def67890".to_string(),
        Row::new(
            "Home".into(),
            AccountState::ReconnectNeeded,
            OffsetDateTime::from_unix_timestamp(1_700_000_100).unwrap(),
        ),
    );
    let h = harness_with_rows(rows);
    // Seed a credential so the redaction test can prove it never reaches the wire.
    h.store.put("work-abc12345", &fake_credential()).unwrap();
    h
}

fn test_app() -> AppState {
    harness().app
}

fn get_accounts(bearer: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().uri("/accounts");
    if let Some(t) = bearer {
        b = b.header(AUTHORIZATION, format!("Bearer {t}"));
    }
    b.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn accounts_returns_the_documented_shape() {
    let app = router(test_app());
    let resp = app.oneshot(get_accounts(Some(BEARER))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let arr = v.as_array().expect("array");
    assert_eq!(arr.len(), 2);

    // Oldest connection first: Work (available), then Home (reconnect needed).
    let work = &arr[0];
    assert_eq!(work["id"], "work-abc12345");
    assert_eq!(work["label"], "Work");
    assert_eq!(work["state"], "available");
    assert_eq!(work["five_hour"], 31);
    assert_eq!(work["seven_day"], 14);
    assert!(
        work.get("resets_at").is_some(),
        "resets_at present when available"
    );
    // The always-present half of the wire contract (`state.rs`): the overlay
    // needs it to explain a row that is not showing numbers.
    for key in [
        "stale",
        "fetched_at",
        "scoped",
        "access_expires_at",
        "refreshed_at",
        "refresh",
        "persist_pending",
    ] {
        assert!(work.get(key).is_some(), "{key} must always be present");
    }
    assert!(
        work.get("expires_at").is_none(),
        "the 365-day expiry is gone"
    );

    let home = &arr[1];
    assert_eq!(home["state"], "reconnect needed");
    assert!(
        home.get("five_hour").is_none(),
        "no numbers when not available"
    );
}

#[tokio::test]
async fn missing_bearer_is_unauthorized() {
    let app = router(test_app());
    let resp = app.oneshot(get_accounts(None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn wrong_bearer_is_unauthorized() {
    let app = router(test_app());
    let resp = app
        .oneshot(get_accounts(Some("not-the-token")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn token_never_appears_in_the_accounts_body() {
    let app = router(test_app());
    let resp = app.oneshot(get_accounts(Some(BEARER))).await.unwrap();
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();

    assert!(
        !text.contains(SECRET_TOKEN),
        "raw token leaked into /accounts"
    );
    assert!(
        !text.contains(SECRET_REFRESH),
        "refresh token leaked into /accounts"
    );
    assert!(
        !text.contains(ROTATED_TOKEN),
        "a rotated token leaked into /accounts"
    );
    assert!(
        !text.contains("sk-ant-ort01"),
        "a refresh-token prefix leaked into /accounts"
    );
    assert!(
        !text.contains("sk-ant"),
        "a token prefix leaked into /accounts"
    );
}

fn post(uri: &str, bearer: Option<&str>, body: Option<serde_json::Value>) -> Request<Body> {
    let mut b = Request::builder().method("POST").uri(uri);
    if let Some(t) = bearer {
        b = b.header(AUTHORIZATION, format!("Bearer {t}"));
    }
    match body {
        Some(v) => b
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    }
}

#[tokio::test]
async fn reconnect_unknown_id_is_404() {
    let app = router(test_app());
    let resp = app
        .oneshot(post("/accounts/no-such-id/reconnect", Some(BEARER), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn reconnect_while_connect_in_flight_is_409() {
    let h = harness();
    let (tx, _rx) = mpsc::unbounded_channel::<String>();
    *h.app.connect_input.lock().await = Some(tx);
    let resp = router(h.app)
        .oneshot(post(
            "/accounts/work-abc12345/reconnect",
            Some(BEARER),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn long_label_is_400() {
    let app = router(test_app());
    let long = "x".repeat(65);
    let resp = app
        .oneshot(post(
            "/accounts",
            Some(BEARER),
            Some(serde_json::json!({ "label": long })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn empty_label_is_400() {
    let app = router(test_app());
    let resp = app
        .oneshot(post(
            "/accounts",
            Some(BEARER),
            Some(serde_json::json!({ "label": "   " })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn shutdown_requires_bearer() {
    let h = harness();
    let shutdown = h.app.shutdown.clone();

    let resp = router(h.app.clone())
        .oneshot(post("/shutdown", None, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let resp = router(h.app)
        .oneshot(post("/shutdown", Some(BEARER), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // The notify permit persists, so awaiting after the request still observes it.
    tokio::time::timeout(std::time::Duration::from_secs(1), shutdown.notified())
        .await
        .expect("shutdown was not notified");
}

#[test]
fn seeded_tasks_are_staggered() {
    for i in 0..5 {
        let gap = seed_delay(i + 1) - seed_delay(i);
        assert!(
            gap >= std::time::Duration::from_secs(2),
            "seed delays must be at least 2 s apart"
        );
    }
}

#[test]
fn failed_reconnect_decision() {
    let healthy = available(31.0, 14.0);
    // A healthy row whose credential still reads back resumes polling.
    assert!(matches!(
        after_failed_reconnect(&healthy, Ok(fake_credential())),
        ReconnectFallback::Respawn(_)
    ));
    // A row already needing reconnect stays that way even with a credential.
    assert!(matches!(
        after_failed_reconnect(&AccountState::ReconnectNeeded, Ok(fake_credential())),
        ReconnectFallback::MarkReconnect
    ));
    // No credential to poll with → reconnect, whatever the row looked like.
    assert!(matches!(
        after_failed_reconnect(&healthy, Err(CredError::NotFound("id".into()))),
        ReconnectFallback::MarkReconnect
    ));
}

/// An expired seed credential and a refresher answering `Rejected`: the task
/// makes exactly one token POST, never touches the usage endpoint, marks the
/// row `reconnect needed`, and ends (plan §4).
#[tokio::test(start_paused = true)]
async fn rejected_refresh_ends_the_task() {
    let mut rows = HashMap::new();
    rows.insert(
        "work-abc12345".to_string(),
        Row::new(
            "Work".into(),
            AccountState::Unavailable,
            OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
        ),
    );
    let h = harness_with_rows(rows);
    h.refresher.script(Err(RefreshError::Rejected(400)));

    let mut cred = fake_credential();
    cred.expires_at = 0; // long expired → refresh before the first fetch
    spawn_poll_task(
        &h.app,
        "work-abc12345".into(),
        cred,
        std::time::Duration::ZERO,
    )
    .await;

    tokio::time::sleep(std::time::Duration::from_secs(30 * 60)).await;

    assert_eq!(h.refresher.calls.load(Ordering::SeqCst), 1);
    assert_eq!(h.source.calls.load(Ordering::SeqCst), 0);
    let rows = h.app.rows.read().await;
    let row = rows.get("work-abc12345").unwrap();
    assert!(matches!(row.state, AccountState::ReconnectNeeded));
    assert_eq!(row.refresh.as_wire(), "rejected");
    drop(rows);
    assert!(h.app.tasks.lock().await["work-abc12345"].is_finished());
}

/// A refresh succeeds but the very next fetch is a 401: the token minted
/// moments ago is dead, so the task ends instead of looping (plan §4).
#[tokio::test(start_paused = true)]
async fn post_refresh_401_ends_the_task() {
    let mut rows = HashMap::new();
    rows.insert(
        "work-abc12345".to_string(),
        Row::new(
            "Work".into(),
            AccountState::Unavailable,
            OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
        ),
    );
    let h = harness_with_rows(rows);
    h.source.script(Err(FetchError::Unauthorized));

    let mut cred = fake_credential();
    cred.expires_at = 0;
    spawn_poll_task(
        &h.app,
        "work-abc12345".into(),
        cred,
        std::time::Duration::ZERO,
    )
    .await;

    tokio::time::sleep(std::time::Duration::from_secs(30 * 60)).await;

    assert_eq!(h.refresher.calls.load(Ordering::SeqCst), 1);
    assert_eq!(h.source.calls.load(Ordering::SeqCst), 1);
    let rows = h.app.rows.read().await;
    let row = rows.get("work-abc12345").unwrap();
    assert!(matches!(row.state, AccountState::ReconnectNeeded));
    drop(rows);
    assert!(h.app.tasks.lock().await["work-abc12345"].is_finished());
}

/// A success followed by a 429: the kept numbers flip to `stale: true`, and
/// that flip alone must push an SSE frame (plan §3).
#[tokio::test(start_paused = true)]
async fn stale_flip_pushes_a_frame() {
    let mut rows = HashMap::new();
    rows.insert(
        "work-abc12345".to_string(),
        Row::new(
            "Work".into(),
            AccountState::Unavailable,
            OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
        ),
    );
    let h = harness_with_rows(rows);
    h.source.script(Ok(good_body()));
    h.source.script(Err(FetchError::RateLimited));

    let mut rx = h.app.events.subscribe();
    let mut cred = fake_credential();
    // Far-future expiry so no refresh phase interferes.
    cred.expires_at = OffsetDateTime::now_utc().unix_timestamp() + 86_400;
    spawn_poll_task(
        &h.app,
        "work-abc12345".into(),
        cred,
        std::time::Duration::ZERO,
    )
    .await;

    let saw_stale = tokio::time::timeout(std::time::Duration::from_secs(10 * 60), async {
        loop {
            let msg = rx.recv().await.expect("event channel closed");
            if msg.event != "accounts" {
                continue;
            }
            let v: serde_json::Value = serde_json::from_str(&msg.data).unwrap();
            let row = v
                .as_array()
                .and_then(|a| a.iter().find(|r| r["id"] == "work-abc12345"));
            if row.is_some_and(|r| r["stale"] == true && r["state"] == "available") {
                break;
            }
        }
    })
    .await;
    assert!(saw_stale.is_ok(), "no stale frame arrived");
}

/// The corrupt seed A5/A6 build on: an unreadable blob reads back as `Corrupt`,
/// not `NotFound`, so the seed path can tell the two apart.
#[test]
fn mem_store_corrupt_seed_reads_as_corrupt() {
    let store = MemStore::default();
    store.seed_corrupt("home-def67890");
    assert!(matches!(
        store.get("home-def67890"),
        Err(CredError::Corrupt(_))
    ));
    assert!(matches!(
        store.get("missing-id"),
        Err(CredError::NotFound(_))
    ));
}

// ---------------------------------------------------------------------------
// Account switching. `POST /accounts/:id/switch` writes the credential into the
// CLI's store; the poll loop keeps the two stores in step afterwards.
// ---------------------------------------------------------------------------

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

async fn accounts_json(app: &AppState) -> Value {
    let resp = router(app.clone())
        .oneshot(get_accounts(Some(BEARER)))
        .await
        .unwrap();
    body_json(resp).await
}

fn by_id<'a>(v: &'a Value, id: &str) -> &'a Value {
    v.as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == id)
        .unwrap_or_else(|| panic!("no row {id}"))
}

/// The seeded credential with an expiry the real clock will not cross: the
/// poll loop refreshes against `now_utc()`, which paused tokio time does not
/// move.
fn live_credential() -> Credential {
    let mut c = fake_credential();
    c.expires_at = 9_999_999_999;
    c
}

fn row(state: AccountState, at: i64) -> Row {
    Row::new(
        "Work".into(),
        state,
        OffsetDateTime::from_unix_timestamp(at).unwrap(),
    )
}

/// One account with a credential in the store and an identity on its row.
fn switch_harness() -> Harness {
    let mut rows = HashMap::new();
    let mut work = row(available(31.0, 14.0), 1_700_000_000);
    work.account = Some(identity("u-work", "work@example.com"));
    rows.insert("work-abc12345".to_string(), work);
    rows.insert(
        "home-def67890".to_string(),
        row(AccountState::ReconnectNeeded, 1_700_000_100),
    );
    let h = harness_with_rows(rows);
    h.store.put("work-abc12345", &fake_credential()).unwrap();
    h
}

#[tokio::test]
async fn a_switch_writes_the_credential_and_identity_and_marks_the_row_active() {
    let h = switch_harness();
    h.cli.seed(
        Credential::new(FOREIGN_TOKEN, FOREIGN_REFRESH, 1_756_600_000, vec![]),
        Some(identity("u-other", "other@example.com")),
    );

    let resp = router(h.app.clone())
        .oneshot(post("/accounts/work-abc12345/switch", Some(BEARER), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(!text.contains("sk-ant"), "a token reached the wire: {text}");
    assert_eq!(text, r#"{"ok":true}"#);

    let held = h.cli.held().expect("the cli store was written");
    assert_eq!(held.access_token, SECRET_TOKEN);
    assert_eq!(held.refresh_token, SECRET_REFRESH);
    assert_eq!(
        h.cli.held_account().unwrap()["emailAddress"],
        "work@example.com"
    );

    let v = accounts_json(&h.app).await;
    assert_eq!(by_id(&v, "work-abc12345")["active"], true);
    assert_eq!(by_id(&v, "work-abc12345")["email"], "work@example.com");
    assert_eq!(by_id(&v, "home-def67890")["active"], false);
    assert!(by_id(&v, "home-def67890").get("email").is_none());
    assert!(!v.to_string().contains("sk-ant"));
}

/// An account connected before identities were captured: the switch still
/// signs the CLI in, and the previous identity block is removed rather than
/// left naming an account the CLI no longer holds.
#[tokio::test]
async fn a_switch_without_an_identity_clears_the_previous_block() {
    let h = switch_harness();
    h.app
        .rows
        .write()
        .await
        .get_mut("work-abc12345")
        .unwrap()
        .account = None;
    h.cli.seed(
        Credential::new(FOREIGN_TOKEN, FOREIGN_REFRESH, 1_756_600_000, vec![]),
        Some(identity("u-other", "other@example.com")),
    );
    let resp = router(h.app.clone())
        .oneshot(post("/accounts/work-abc12345/switch", Some(BEARER), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(h.cli.held().unwrap().access_token, SECRET_TOKEN);
    assert!(h.cli.held_account().is_none(), "a stale identity survived");
}

#[tokio::test]
async fn a_row_that_needs_a_reconnect_cannot_be_switched_to() {
    let h = switch_harness();
    // A dead token whose blob is still stored: the usual post-401 shape.
    h.store.put("home-def67890", &fake_credential()).unwrap();
    let resp = router(h.app.clone())
        .oneshot(post("/accounts/home-def67890/switch", Some(BEARER), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(h.cli.writes.load(Ordering::SeqCst), 0);
    assert!(h.app.active.read().await.is_none());
}

#[tokio::test]
async fn a_switch_for_an_unknown_account_is_404() {
    let h = switch_harness();
    let resp = router(h.app.clone())
        .oneshot(post("/accounts/nope-00000000/switch", Some(BEARER), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(h.cli.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_account_without_a_stored_credential_cannot_switch() {
    let h = switch_harness();
    let resp = router(h.app.clone())
        .oneshot(post("/accounts/home-def67890/switch", Some(BEARER), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(h.cli.writes.load(Ordering::SeqCst), 0);
    assert!(h.app.active.read().await.is_none());
}

#[tokio::test]
async fn a_switch_needs_the_bearer() {
    let h = switch_harness();
    let resp = router(h.app.clone())
        .oneshot(post("/accounts/work-abc12345/switch", None, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(h.cli.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_failed_store_write_is_reported_and_marks_nothing_active() {
    let h = switch_harness();
    h.cli.script_failure();
    let resp = router(h.app.clone())
        .oneshot(post("/accounts/work-abc12345/switch", Some(BEARER), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("no keychain in tests"), "{text}");
    assert!(!text.contains("sk-ant"), "{text}");
    assert!(h.app.active.read().await.is_none());
    assert!(h.cli.held().is_none());
}

#[tokio::test]
async fn deleting_the_active_account_clears_active_but_leaves_the_cli_store() {
    let h = switch_harness();
    router(h.app.clone())
        .oneshot(post("/accounts/work-abc12345/switch", Some(BEARER), None))
        .await
        .unwrap();
    assert_eq!(h.app.active.read().await.as_deref(), Some("work-abc12345"));

    let resp = router(h.app.clone())
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/accounts/work-abc12345")
                .header(AUTHORIZATION, format!("Bearer {BEARER}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(h.store.get("work-abc12345").is_err());
    assert!(h.app.active.read().await.is_none());
    assert_eq!(
        h.cli.held().map(|c| c.access_token),
        Some(SECRET_TOKEN.to_string()),
        "the cli keeps its login"
    );
}

/// The poll loop sees the CLI holding the same lineage and marks the row
/// active without anyone having pressed the button.
#[tokio::test(start_paused = true)]
async fn the_poll_loop_notices_the_cli_already_holds_this_account() {
    let h = switch_harness();
    h.cli.seed(live_credential(), None);
    spawn_poll_task(
        &h.app,
        "work-abc12345".into(),
        live_credential(),
        std::time::Duration::ZERO,
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    assert_eq!(h.app.active.read().await.as_deref(), Some("work-abc12345"));
    assert_eq!(h.cli.writes.load(Ordering::SeqCst), 0, "nothing to write");
    assert_eq!(h.refresher.calls.load(Ordering::SeqCst), 0);
}

/// The CLI refreshed first (a rotation that changed both tokens, tied to us
/// only by uuid): its copy is adopted, persisted, and used for the fetch — no
/// refresh of our own, which could be refused now.
#[tokio::test(start_paused = true)]
async fn the_poll_loop_adopts_a_token_the_cli_refreshed() {
    let h = switch_harness();
    let mut theirs = Credential::new(FOREIGN_TOKEN, FOREIGN_REFRESH, 9_999_999_999, vec![]);
    theirs.subscription_type = Some("max".into());
    h.cli
        .seed(theirs, Some(identity("u-work", "work@example.com")));

    let mut ours = fake_credential();
    ours.expires_at = 0; // would refresh, if it were not superseded
    spawn_poll_task(
        &h.app,
        "work-abc12345".into(),
        ours,
        std::time::Duration::ZERO,
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    assert_eq!(h.refresher.calls.load(Ordering::SeqCst), 0);
    assert_eq!(h.source.calls.load(Ordering::SeqCst), 1);
    let stored = h.store.get("work-abc12345").unwrap();
    assert_eq!(stored.access_token, FOREIGN_TOKEN);
    assert_eq!(stored.refresh_token, FOREIGN_REFRESH);
    assert_eq!(stored.subscription_type.as_deref(), Some("max"));
    assert_eq!(h.app.active.read().await.as_deref(), Some("work-abc12345"));
}

/// The daemon refreshes while active: the rotated token goes straight back
/// into the CLI's store, so the CLI never refreshes against a stale token.
#[tokio::test(start_paused = true)]
async fn a_rotation_while_active_is_written_back_to_the_cli() {
    let h = switch_harness();
    h.cli.seed(fake_credential(), None);
    let mut ours = fake_credential();
    ours.expires_at = 0;
    spawn_poll_task(
        &h.app,
        "work-abc12345".into(),
        ours,
        std::time::Duration::ZERO,
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    assert_eq!(h.refresher.calls.load(Ordering::SeqCst), 1);
    let held = h.cli.held().unwrap();
    assert_eq!(held.access_token, ROTATED_TOKEN);
    assert_eq!(held.refresh_token, SECRET_REFRESH);
    assert_eq!(
        h.store.get("work-abc12345").unwrap().access_token,
        ROTATED_TOKEN
    );
}

/// Someone ran `/login` as an account we do not know: the row stops being
/// active, its own credential is untouched, and the CLI's login is left alone.
#[tokio::test(start_paused = true)]
async fn a_foreign_login_clears_active_and_is_not_adopted() {
    let h = switch_harness();
    *h.app.active.write().await = Some("work-abc12345".into());
    h.cli.seed(
        Credential::new(FOREIGN_TOKEN, FOREIGN_REFRESH, 1_756_600_000, vec![]),
        Some(identity("u-other", "other@example.com")),
    );
    spawn_poll_task(
        &h.app,
        "work-abc12345".into(),
        live_credential(),
        std::time::Duration::ZERO,
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    assert!(h.app.active.read().await.is_none());
    assert_eq!(
        h.store.get("work-abc12345").unwrap().access_token,
        SECRET_TOKEN
    );
    assert_eq!(h.cli.held().unwrap().access_token, FOREIGN_TOKEN);
    assert_eq!(h.cli.writes.load(Ordering::SeqCst), 0);
}

/// The CLI holds an older token of ours (a write-back that never landed):
/// the current one goes back in.
#[tokio::test(start_paused = true)]
async fn a_cli_that_fell_behind_gets_the_current_token() {
    let h = switch_harness();
    let mut old = fake_credential();
    old.expires_at = 1_000;
    h.cli.seed(old, None);
    let mut ours = fake_credential();
    ours.access_token = ROTATED_TOKEN.into();
    ours.expires_at = 9_999_999_999;
    spawn_poll_task(
        &h.app,
        "work-abc12345".into(),
        ours,
        std::time::Duration::ZERO,
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    assert_eq!(h.cli.held().unwrap().access_token, ROTATED_TOKEN);
    assert_eq!(h.app.active.read().await.as_deref(), Some("work-abc12345"));
    assert_eq!(h.refresher.calls.load(Ordering::SeqCst), 0);
}
