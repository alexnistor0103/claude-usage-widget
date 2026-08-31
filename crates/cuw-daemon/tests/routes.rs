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
use cuw_core::{CliToken, Credential};
use cuw_creds::{CredError, CredentialStore};
use cuw_daemon::http::{
    after_failed_reconnect, router, seed_delay, spawn_poll_task, AppState, ReconnectFallback,
    SharedSource,
};
use cuw_daemon::session::Nonces;
use cuw_daemon::state::Row;
use cuw_launch::{LaunchError, LaunchRequest, SessionLauncher};
use std::sync::atomic::AtomicU16;
use time::OffsetDateTime;
use tokio::sync::{broadcast, mpsc, Mutex, Notify, RwLock};
use tower::ServiceExt;

const BEARER: &str = "test-bearer-token";
const SECRET_TOKEN: &str = "sk-ant-oat01-FAKEFAKEFAKEFAKEFAKEFAKE0005";
const SECRET_REFRESH: &str = "sk-ant-ort01-FAKEFAKEFAKEFAKEFAKEFAKE0006";
const ROTATED_TOKEN: &str = "sk-ant-oat01-FAKEROTATED0000000000000007";
const SECRET_CLI_TOKEN: &str = "sk-ant-oat01-FAKECLIFAKECLIFAKECLIFAKECLI0009";

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
    cli: std::sync::Mutex<HashMap<String, CliToken>>,
}

impl MemStore {
    fn seed_corrupt(&self, id: &str) {
        self.creds.lock().unwrap().insert(id.into(), Err(()));
    }
    fn seed_cli(&self, id: &str) {
        self.put_cli(id, &CliToken::new(SECRET_CLI_TOKEN, now()))
            .expect("put_cli");
    }
    fn has_cli(&self, id: &str) -> bool {
        self.cli.lock().unwrap().contains_key(id)
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
    fn put_cli(&self, id: &str, tok: &CliToken) -> Result<(), CredError> {
        self.cli.lock().unwrap().insert(id.into(), tok.clone());
        Ok(())
    }
    fn get_cli(&self, id: &str) -> Result<CliToken, CredError> {
        match self.cli.lock().unwrap().get(id) {
            Some(tok) => Ok(tok.clone()),
            None => Err(CredError::NotFound(id.into())),
        }
    }
    fn delete_cli(&self, id: &str) -> Result<(), CredError> {
        self.cli.lock().unwrap().remove(id);
        Ok(())
    }
}

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_756_600_000).expect("timestamp")
}

/// Records what it was asked to launch instead of opening a console. Scripted
/// to fail so the nonce-burn path is covered too.
#[derive(Default)]
struct FakeLauncher {
    calls: std::sync::Mutex<Vec<LaunchRequest>>,
    fail: std::sync::atomic::AtomicBool,
}

impl FakeLauncher {
    fn last(&self) -> Option<LaunchRequest> {
        self.calls.lock().unwrap().last().cloned()
    }
    fn count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
    fn script_failure(&self) {
        self.fail.store(true, Ordering::SeqCst);
    }
}

impl SessionLauncher for FakeLauncher {
    fn launch(&self, req: LaunchRequest) -> Result<(), LaunchError> {
        self.calls.lock().unwrap().push(req);
        if self.fail.load(Ordering::SeqCst) {
            return Err(LaunchError::Spawn("no terminal in tests".into()));
        }
        Ok(())
    }
}

fn fake_credential() -> Credential {
    Credential {
        v: 1,
        access_token: SECRET_TOKEN.into(),
        refresh_token: SECRET_REFRESH.into(),
        expires_at: 1_756_600_000,
        scopes: vec!["user:inference".into(), "user:profile".into()],
    }
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
    launcher: Arc<FakeLauncher>,
}

fn harness_with_rows(rows: HashMap<String, Row>) -> Harness {
    let source = Arc::new(FakeSource::default());
    let refresher = Arc::new(FakeRefresher::default());
    let store = Arc::new(MemStore::default());
    let launcher = Arc::new(FakeLauncher::default());

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
        sessions: Arc::new(Mutex::new(Nonces::default())),
        launcher: launcher.clone(),
        port: Arc::new(AtomicU16::new(8787)),
        // Every launch validates it, so it has to be a directory that exists.
        default_cwd: Arc::new(std::env::temp_dir()),
    };
    Harness {
        app,
        source,
        refresher,
        store,
        launcher,
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
// Session switching (SWITCHER §4). `POST /accounts/:id/session` mints a code and
// launches; `GET /session/:nonce` is the one route that returns a token.
// ---------------------------------------------------------------------------

fn get(uri: &str, bearer: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().uri(uri);
    if let Some(t) = bearer {
        b = b.header(AUTHORIZATION, format!("Bearer {t}"));
    }
    b.body(Body::empty()).unwrap()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

/// Mint a code the way the route does, without going through the launcher.
async fn mint(h: &Harness, id: &str) -> String {
    h.app
        .sessions
        .lock()
        .await
        .mint(id, std::time::Instant::now())
}

#[tokio::test]
async fn a_session_launch_mints_a_code_and_never_returns_a_token() {
    let h = harness();
    h.store.seed_cli("work-abc12345");

    let resp = router(h.app.clone())
        .oneshot(post(
            "/accounts/work-abc12345/session",
            Some(BEARER),
            Some(serde_json::json!({})),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        !text.contains("sk-ant"),
        "a token reached the overlay: {text}"
    );
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["ok"], true);

    // The launcher was handed the bound port and a nonce, never a token.
    let req = h.launcher.last().expect("launched");
    assert_eq!(req.port, 8787);
    assert_eq!(req.cwd, std::env::temp_dir());
    assert!(req.nonce.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(req.terminal.is_none());
}

#[tokio::test]
async fn a_session_launch_forwards_the_terminal_override_and_cwd() {
    let h = harness();
    h.store.seed_cli("work-abc12345");
    let cwd = std::env::temp_dir();

    let resp = router(h.app.clone())
        .oneshot(post(
            "/accounts/work-abc12345/session",
            Some(BEARER),
            Some(serde_json::json!({
                "cwd": cwd.to_string_lossy(),
                "terminal": ["wt.exe", "-w", "0", "nt"],
            })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = h.launcher.last().expect("launched");
    assert_eq!(req.cwd, cwd);
    assert_eq!(
        req.terminal.as_deref(),
        Some(["wt.exe", "-w", "0", "nt"].map(String::from).as_slice())
    );
}

#[tokio::test]
async fn an_account_with_no_cli_token_cannot_switch() {
    let h = harness();
    let resp = router(h.app.clone())
        .oneshot(post(
            "/accounts/work-abc12345/session",
            Some(BEARER),
            Some(serde_json::json!({})),
        ))
        .await
        .unwrap();
    // A display state, not a server error: the row says `switch unavailable`.
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(h.launcher.count(), 0, "nothing was launched");
    assert_eq!(
        h.app
            .sessions
            .lock()
            .await
            .mint("x", std::time::Instant::now())
            .len(),
        64,
        "the store is still usable"
    );
}

#[tokio::test]
async fn a_session_for_an_unknown_account_is_404() {
    let h = harness();
    let resp = router(h.app.clone())
        .oneshot(post("/accounts/nope-00000000/session", Some(BEARER), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(h.launcher.count(), 0);
}

#[tokio::test]
async fn a_failed_launch_burns_the_code() {
    let h = harness();
    h.store.seed_cli("work-abc12345");
    h.launcher.script_failure();

    let resp = router(h.app.clone())
        .oneshot(post("/accounts/work-abc12345/session", Some(BEARER), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

    // The code the launcher was given is no longer redeemable.
    let nonce = h.launcher.last().expect("attempted").nonce;
    let refused = router(h.app.clone())
        .oneshot(get(&format!("/session/{nonce}"), Some(BEARER)))
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn redeeming_returns_the_cli_token_once_and_never_the_login_credential() {
    let h = harness();
    h.store.seed_cli("work-abc12345");
    let nonce = mint(&h, "work-abc12345").await;

    let resp = router(h.app.clone())
        .oneshot(get(&format!("/session/{nonce}"), Some(BEARER)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("cache-control")
            .map(|v| v.to_str().unwrap()),
        Some("no-store"),
        "the one token-bearing response must not be cached"
    );

    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["token"], SECRET_CLI_TOKEN);
    // The `user:profile` credential has a different key and no route (plan §5).
    assert!(
        !text.contains(SECRET_TOKEN),
        "the login access token leaked"
    );
    assert!(!text.contains(SECRET_REFRESH), "the refresh token leaked");

    // Single-use: the second read gets nothing.
    let again = router(h.app.clone())
        .oneshot(get(&format!("/session/{nonce}"), Some(BEARER)))
        .await
        .unwrap();
    assert_eq!(again.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn redeeming_requires_the_bearer() {
    let h = harness();
    h.store.seed_cli("work-abc12345");
    let nonce = mint(&h, "work-abc12345").await;

    let resp = router(h.app.clone())
        .oneshot(get(&format!("/session/{nonce}"), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Refused before the nonce was touched: the code is still good.
    let ok = router(h.app.clone())
        .oneshot(get(&format!("/session/{nonce}"), Some(BEARER)))
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_code_whose_token_vanished_is_refused() {
    let h = harness();
    h.store.seed_cli("work-abc12345");
    let nonce = mint(&h, "work-abc12345").await;
    h.store.delete_cli("work-abc12345").expect("delete_cli");

    let resp = router(h.app.clone())
        .oneshot(get(&format!("/session/{nonce}"), Some(BEARER)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(body_json(resp).await.get("token").is_none());
}

#[tokio::test]
async fn deleting_an_account_drops_both_grants() {
    let h = harness();
    h.store.seed_cli("work-abc12345");

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
    assert!(
        !h.store.has_cli("work-abc12345"),
        "the CLI token outlived its account"
    );
}

#[tokio::test]
async fn a_row_with_a_cli_token_advertises_the_switch() {
    let mut rows = HashMap::new();
    let mut row = Row::new(
        "Work".into(),
        available(31.0, 14.0),
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
    );
    row.can_switch = true;
    rows.insert("work-abc12345".to_string(), row);

    let h = harness_with_rows(rows);
    let resp = router(h.app)
        .oneshot(get_accounts(Some(BEARER)))
        .await
        .unwrap();
    let v = body_json(resp).await;
    assert_eq!(v[0]["can_switch"], true);
}
