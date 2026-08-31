//! Localhost HTTP + SSE (M2.3), bearer auth (M2.4), and the per-account poll
//! task (M2.2/M2.6). Bind `127.0.0.1` only; require the bearer on every route.
//!
//!   GET    /accounts               → the wire array
//!   POST   /accounts               → run the connect flow, then poll without a restart
//!   POST   /accounts/:id/reconnect → re-run the login for an existing account
//!   DELETE /accounts/:id           → stop polling, forget the account, drop its credential
//!   POST   /accounts/:id/session   → mint a launch code and open a terminal as that account
//!   GET    /session/:nonce         → the shim redeems the code for the CLI token
//!   POST   /shutdown               → ask the daemon to exit gracefully
//!   GET    /events                 → SSE: a frame on every state change and connect step
//!
//! `GET /session/:nonce` is the one route that returns a token, and the one
//! bounded exception to plan §5 — a *different* credential (`<id>#cli`, scope
//! `user:inference`), single-use, 30 s TTL, never logged (SWITCHER §6).

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Path, Request, State};
use axum::http::{header::AUTHORIZATION, header::CACHE_CONTROL, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use constant_time_eq::constant_time_eq;
use cuw_connect::{ConnectEvent, ConnectRequest, Connected};
use cuw_core::client::{FetchError, OAuthUsageClient, RawResponse, UsageSource};
use cuw_core::model::AccountState;
use cuw_core::refresh::TokenRefresher;
use cuw_core::Credential;
use cuw_creds::{CredError, CredentialStore};
use cuw_launch::{LaunchRequest, SessionLauncher};
use serde::Deserialize;
use serde_json::json;
use time::OffsetDateTime;
use tokio::sync::{broadcast, mpsc, Mutex, Notify};
use tokio::task::{AbortHandle, JoinHandle};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};
use tower_http::cors::CorsLayer;

use crate::poll::{
    apply_fetch, apply_refresh, fingerprint, needs_refresh, refresh_backoff, sleep_crosses_stale,
    PollState, RefreshStatus, RefreshStep, Step,
};
use crate::registry::{RegAccount, Registry};
use crate::session::Nonces;
use crate::state::{self, Row, SharedRows, SseMsg};

/// A cloneable `UsageSource` handle. Needed because `connect()` and the poll
/// loop take a sized `S: UsageSource`, while the daemon holds one behind a trait
/// object so an official API is a one-impl swap (plan §9).
#[derive(Clone)]
pub struct SharedSource(pub Arc<dyn UsageSource>);

#[async_trait::async_trait]
impl UsageSource for SharedSource {
    async fn fetch(&self, token: &str) -> Result<RawResponse, FetchError> {
        self.0.fetch(token).await
    }
}

impl SharedSource {
    /// The real undocumented-endpoint client.
    pub fn live() -> Self {
        SharedSource(Arc::new(OAuthUsageClient::default()))
    }
}

/// Everything the routes and poll tasks share. All `Arc`, so `Clone` is cheap.
#[derive(Clone)]
pub struct AppState {
    pub rows: SharedRows,
    pub source: SharedSource,
    pub store: Arc<dyn CredentialStore>,
    pub bearer: Arc<String>,
    pub events: broadcast::Sender<SseMsg>,
    pub tasks: Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
    pub registry_path: Arc<PathBuf>,
    /// The connect flow's scratch root, `data/scratch`. Daemon-owned so a
    /// leftover dir is swept at startup, and never `%TEMP%` (plan §4).
    pub scratch_root: Arc<PathBuf>,
    /// Serializes registry.toml read-modify-write so a concurrent connect and
    /// delete can't clobber each other and drop an account.
    pub registry_lock: Arc<Mutex<()>>,
    /// Sender into the in-progress connect flow's PTY, present only while a
    /// connect is running. `POST /accounts/connect/code` pushes the pasted
    /// authorization code through it. `Some` also means a connect is in flight,
    /// so a second `POST /accounts` is refused (single-flight).
    pub connect_input: Arc<Mutex<Option<mpsc::UnboundedSender<String>>>>,
    /// The token endpoint, behind a trait like the usage source (plan §9).
    pub refresher: Arc<dyn TokenRefresher>,
    /// `POST /shutdown` notifies; main awaits and exits gracefully.
    pub shutdown: Arc<Notify>,
    /// Abort handle for the in-flight connect task, so shutdown can kill the
    /// CLI and scrub the scratch dir instead of orphaning them.
    pub connect_task: Arc<Mutex<Option<AbortHandle>>>,
    /// Outstanding session-launch codes (SWITCHER §4).
    pub sessions: Arc<Mutex<Nonces>>,
    /// Opens the terminal a launch code is spent in. Behind a trait like the
    /// usage source, so macOS is a one-impl swap (M7.4).
    pub launcher: Arc<dyn SessionLauncher>,
    /// The port the listener actually bound, published here after the bind so
    /// the shim is told where to redeem rather than assuming a default. `0`
    /// until then, which refuses a launch instead of pointing it nowhere.
    pub port: Arc<AtomicU16>,
    /// Where a launched session starts when the caller names no directory.
    pub default_cwd: Arc<PathBuf>,
}

/// Build the router: every route behind the bearer gate, permissive CORS on the
/// outside so the overlay's webview (a different origin) can reach localhost.
pub fn router(app: AppState) -> Router {
    Router::new()
        .route("/accounts", get(list_accounts).post(add_account))
        .route("/accounts/connect/code", axum::routing::post(submit_code))
        .route("/accounts/:id", axum::routing::delete(remove_account))
        .route(
            "/accounts/:id/reconnect",
            axum::routing::post(reconnect_account),
        )
        .route("/accounts/:id/session", axum::routing::post(start_session))
        .route("/session/:nonce", get(redeem_session))
        .route("/shutdown", axum::routing::post(shutdown))
        .route("/events", get(events))
        .layer(middleware::from_fn_with_state(app.clone(), require_bearer))
        .layer(CorsLayer::very_permissive())
        .with_state(app)
}

/// Constant-time bearer check on every route; missing/invalid → 401.
async fn require_bearer(State(app): State<AppState>, req: Request, next: Next) -> Response {
    let provided = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));
    let ok = provided
        .map(|t| constant_time_eq(t.as_bytes(), app.bearer.as_bytes()))
        .unwrap_or(false);
    if ok {
        next.run(req).await
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

async fn list_accounts(State(app): State<AppState>) -> Json<Vec<state::WireAccount>> {
    let rows = app.rows.read().await;
    Json(state::snapshot(&rows))
}

#[derive(Deserialize)]
struct NewAccount {
    label: String,
}

/// Run the M1 connect flow, streaming its steps over `/events`. The flow is
/// interactive — it emits the sign-in URL and an `awaiting_code` phase, and the
/// pasted code arrives via `POST /accounts/connect/code`. On success, store the
/// credential, persist the account, and start polling without a restart.
async fn add_account(State(app): State<AppState>, Json(body): Json<NewAccount>) -> Response {
    if body.label.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "label must not be empty").into_response();
    }
    if body.label.chars().count() > 64 {
        return (StatusCode::BAD_REQUEST, "label too long (max 64)").into_response();
    }

    // Single-flight: registering the input sender both wires up code delivery and
    // rejects a second concurrent connect (the login is one interactive flow).
    let (code_tx, code_rx) = mpsc::unbounded_channel::<String>();
    {
        let mut slot = app.connect_input.lock().await;
        if slot.is_some() {
            return (StatusCode::CONFLICT, "a connect is already in progress").into_response();
        }
        *slot = Some(code_tx);
    }

    let req = ConnectRequest {
        label: body.label,
        existing_id: None,
        scratch_root: (*app.scratch_root).clone(),
    };
    let result = run_connect(&app, req, code_rx).await;
    // Clear the slot whatever the outcome, so the next connect can start.
    *app.connect_input.lock().await = None;

    let connected = match result {
        Ok(c) => c,
        Err(resp) => return *resp,
    };
    tracing::info!(id = %connected.id, "connect validated; storing credential");

    if let Err(e) = app.store.put(&connected.id, &connected.credential) {
        // CredError carries no token; safe to log and return.
        tracing::error!(id = %connected.id, error = %e, "store credential");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not store credential",
        )
            .into_response();
    }
    let can_switch = store_cli_token(&app, &connected.id, connected.cli_token.as_ref());

    let connected_at = OffsetDateTime::now_utc();
    persist_account(
        &app,
        RegAccount {
            id: connected.id.clone(),
            label: connected.label.clone(),
            connected_at: connected_at
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
        },
    )
    .await;

    let mut row = Row::new(
        connected.label.clone(),
        AccountState::Unavailable,
        connected_at,
    );
    row.can_switch = can_switch;
    app.rows.write().await.insert(connected.id.clone(), row);

    spawn_poll_task(
        &app,
        connected.id.clone(),
        connected.credential.clone(),
        std::time::Duration::ZERO,
    )
    .await;
    broadcast_state(&app).await;
    tracing::info!(id = %connected.id, "account persisted and polling started");

    Json(json!({ "id": connected.id, "label": connected.label })).into_response()
}

/// Run the connect flow as a tracked task, so shutdown can abort it mid-flight
/// (dropping the flow scrubs the scratch dir and kills the CLI). The caller
/// still owns the single-flight slot.
async fn run_connect(
    app: &AppState,
    req: ConnectRequest,
    code_rx: mpsc::UnboundedReceiver<String>,
) -> Result<Connected, Box<Response>> {
    let events = app.events.clone();
    let emit = move |ev: ConnectEvent| {
        let _ = events.send(connect_msg(&ev));
    };
    let source = app.source.clone();
    let handle =
        tokio::spawn(async move { cuw_connect::connect(&source, req, emit, code_rx).await });
    *app.connect_task.lock().await = Some(handle.abort_handle());
    let result = handle.await;
    *app.connect_task.lock().await = None;
    match result {
        Ok(Ok(c)) => Ok(c),
        Ok(Err(e)) => {
            tracing::error!(error = %e, "connect flow failed");
            Err(Box::new(
                (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
            ))
        }
        // A JoinError here means the shutdown path aborted us.
        Err(_) => Err(Box::new(
            (StatusCode::BAD_GATEWAY, "connect cancelled").into_response(),
        )),
    }
}

/// After a failed reconnect: a healthy row whose stored credential still reads
/// back keeps polling as if nothing happened; otherwise it needs a reconnect.
pub fn after_failed_reconnect(
    prev: &AccountState,
    stored: Result<Credential, CredError>,
) -> ReconnectFallback {
    match stored {
        Ok(cred) if !matches!(prev, AccountState::ReconnectNeeded) => {
            ReconnectFallback::Respawn(cred)
        }
        _ => ReconnectFallback::MarkReconnect,
    }
}

/// See [`after_failed_reconnect`].
pub enum ReconnectFallback {
    Respawn(Credential),
    MarkReconnect,
}

/// Re-run the login for an already-registered account. The old poll task is
/// stopped first; on success the row starts over with a fresh credential, and
/// on failure a previously healthy row resumes polling untouched — a cancelled
/// reconnect must not kill it.
async fn reconnect_account(State(app): State<AppState>, Path(id): Path<String>) -> Response {
    let (label, prev_state) = {
        let rows = app.rows.read().await;
        match rows.get(&id) {
            Some(r) => (r.label.clone(), r.state.clone()),
            None => return StatusCode::NOT_FOUND.into_response(),
        }
    };

    let (code_tx, code_rx) = mpsc::unbounded_channel::<String>();
    {
        let mut slot = app.connect_input.lock().await;
        if slot.is_some() {
            return (StatusCode::CONFLICT, "a connect is already in progress").into_response();
        }
        *slot = Some(code_tx);
    }

    // Stop the old task before the login: two pollers on one id would race the
    // credential store, and its token may be the reason we are reconnecting.
    if let Some(handle) = app.tasks.lock().await.remove(&id) {
        handle.abort();
    }

    let req = ConnectRequest {
        label: label.clone(),
        existing_id: Some(id.clone()),
        scratch_root: (*app.scratch_root).clone(),
    };
    let result = run_connect(&app, req, code_rx).await;
    *app.connect_input.lock().await = None;

    let connected = match result {
        Ok(c) => c,
        Err(resp) => {
            match after_failed_reconnect(&prev_state, app.store.get(&id)) {
                ReconnectFallback::Respawn(cred) => {
                    spawn_poll_task(&app, id, cred, std::time::Duration::ZERO).await;
                }
                ReconnectFallback::MarkReconnect => {
                    let mut rows = app.rows.write().await;
                    if let Some(row) = rows.get_mut(&id) {
                        row.state = AccountState::ReconnectNeeded;
                        row.refresh = RefreshStatus::Rejected;
                    }
                }
            }
            broadcast_state(&app).await;
            return *resp;
        }
    };

    if let Err(e) = app.store.put(&id, &connected.credential) {
        tracing::error!(id = %id, error = %e, "store credential");
        {
            let mut rows = app.rows.write().await;
            if let Some(row) = rows.get_mut(&id) {
                row.state = AccountState::ReconnectNeeded;
                row.refresh = RefreshStatus::Rejected;
            }
        }
        broadcast_state(&app).await;
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not store credential",
        )
            .into_response();
    }

    let can_switch = store_cli_token(&app, &id, connected.cli_token.as_ref());

    let connected_at = OffsetDateTime::now_utc();
    persist_account(
        &app,
        RegAccount {
            id: id.clone(),
            label: label.clone(),
            connected_at: connected_at
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
        },
    )
    .await;

    let mut row = Row::new(label.clone(), AccountState::Unavailable, connected_at);
    row.can_switch = can_switch;
    app.rows.write().await.insert(id.clone(), row);
    spawn_poll_task(
        &app,
        id.clone(),
        connected.credential.clone(),
        std::time::Duration::ZERO,
    )
    .await;
    broadcast_state(&app).await;
    tracing::info!(id = %id, "account reconnected and polling restarted");

    Json(json!({ "id": id, "label": label })).into_response()
}

/// Persist the CLI token a connect captured. Returns whether the account can
/// offer the switch button: a store failure is a display state, not a connect
/// failure (SWITCHER §6), so it downgrades the row rather than the whole flow.
fn store_cli_token(app: &AppState, id: &str, tok: Option<&cuw_core::CliToken>) -> bool {
    let Some(tok) = tok else {
        // The capture step already explained itself in the connect log; clear
        // any stale token so the row does not offer a switch it cannot honour.
        let _ = app.store.delete_cli(id);
        return false;
    };
    match app.store.put_cli(id, tok) {
        Ok(()) => true,
        Err(e) => {
            tracing::error!(id = %id, error = %e, "store cli token");
            false
        }
    }
}

/// What the overlay may name when it asks for a session. Both optional: the
/// terminal override is `settings.session.terminal`, which the overlay owns
/// (SWITCHER §5), and it is argv — never a shell string.
#[derive(Deserialize, Default)]
struct SessionRequest {
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    terminal: Option<Vec<String>>,
}

/// Mint a single-use launch code and open a terminal on the shim that redeems
/// it (SWITCHER §4). The response carries no token and no nonce — the overlay
/// POSTs and gets `ok`, which is what keeps the plan §5 invariant true for the
/// overlay even though the switcher exists.
async fn start_session(
    State(app): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<SessionRequest>>,
) -> Response {
    if !app.rows.read().await.contains_key(&id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    // Checked before a code is minted: an account that never captured a
    // `setup-token` grant shows `switch unavailable` and needs a reconnect.
    if let Err(e) = app.store.get_cli(&id) {
        tracing::info!(id = %id, error = %e, "no cli token; switch unavailable");
        return (
            StatusCode::CONFLICT,
            "no session token for this account — reconnect it to enable switching",
        )
            .into_response();
    }

    let port = app.port.load(Ordering::Relaxed);
    if port == 0 {
        return (StatusCode::SERVICE_UNAVAILABLE, "no bound port yet").into_response();
    }

    let Json(body) = body.unwrap_or_default();
    let cwd = body
        .cwd
        .map(PathBuf::from)
        .unwrap_or_else(|| (*app.default_cwd).clone());

    let nonce = app.sessions.lock().await.mint(&id, Instant::now());
    let req = LaunchRequest::new(nonce.clone(), port, cwd).with_terminal(body.terminal);

    // The launcher writes files and calls `CreateProcessW`; keep it off the
    // async worker.
    let launcher = app.launcher.clone();
    let launched = tokio::task::spawn_blocking(move || launcher.launch(req)).await;

    match launched {
        Ok(Ok(())) => {
            tracing::info!(id = %id, "session terminal launched");
            Json(json!({ "ok": true })).into_response()
        }
        Ok(Err(e)) => {
            // No terminal will ever spend it; burn it now rather than leave a
            // live code for its whole TTL. `LaunchError` carries no nonce.
            app.sessions.lock().await.burn(&nonce);
            tracing::warn!(id = %id, error = %e, "session launch failed");
            (StatusCode::BAD_GATEWAY, e.to_string()).into_response()
        }
        Err(_) => {
            app.sessions.lock().await.burn(&nonce);
            (StatusCode::INTERNAL_SERVER_ERROR, "launch task failed").into_response()
        }
    }
}

/// Redeem a launch code for the account's CLI token — the one route that
/// returns a token, and the bounded exception documented in plan §5.
///
/// The nonce is burned on read, whatever happens next. Unknown, spent and
/// expired are one answer, so a caller learns nothing from the difference. Only
/// the `<id>#cli` grant is ever served; the `user:profile` credential has a
/// different key and no route at all.
async fn redeem_session(State(app): State<AppState>, Path(nonce): Path<String>) -> Response {
    let Some(id) = app.sessions.lock().await.redeem(&nonce, Instant::now()) else {
        return (StatusCode::NOT_FOUND, "no such session code").into_response();
    };

    // Read after redemption: the token spends the least time in memory that way,
    // and a deleted account cannot be launched with a stale code.
    let token = match app.store.get_cli(&id) {
        Ok(tok) => tok.token,
        Err(e) => {
            tracing::warn!(id = %id, error = %e, "redeem: no cli token");
            return (StatusCode::NOT_FOUND, "no session token").into_response();
        }
    };

    tracing::info!(id = %id, "session code redeemed");
    let mut resp = Json(json!({ "token": token })).into_response();
    // The one token-bearing response in the app: keep it out of every cache.
    resp.headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

/// Ask the daemon to exit. Any local process holding the bearer file can stop
/// it — acceptable for a single-user localhost service (plan §5).
async fn shutdown(State(app): State<AppState>) -> StatusCode {
    app.shutdown.notify_one();
    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
struct CodeBody {
    code: String,
}

/// Forward the pasted authorization code into the in-progress connect flow's
/// PTY. 409 if no connect is waiting for one.
async fn submit_code(State(app): State<AppState>, Json(body): Json<CodeBody>) -> Response {
    let slot = app.connect_input.lock().await;
    match slot.as_ref() {
        Some(tx) if tx.send(body.code).is_ok() => StatusCode::NO_CONTENT.into_response(),
        _ => (StatusCode::CONFLICT, "no connect awaiting a code").into_response(),
    }
}

/// Stop the account's poll task, forget its row, remove it from the registry,
/// and delete its credential from the keyring.
async fn remove_account(State(app): State<AppState>, Path(id): Path<String>) -> Response {
    if let Some(handle) = app.tasks.lock().await.remove(&id) {
        handle.abort();
    }
    let existed = app.rows.write().await.remove(&id).is_some();

    {
        let _guard = app.registry_lock.lock().await;
        let mut reg = Registry::load(&app.registry_path);
        let before = reg.accounts.len();
        reg.accounts.retain(|a| a.id != id);
        if reg.accounts.len() != before {
            if let Err(e) = reg.save(&app.registry_path) {
                tracing::error!(error = %e, "persist registry after delete");
            }
        }
    }

    // A missing or unreadable blob is already the desired end state.
    match app.store.delete(&id) {
        Ok(()) | Err(CredError::NotFound(_) | CredError::Corrupt(_)) => {}
        Err(e) => tracing::error!(id = %id, error = %e, "delete credential"),
    }
    // Both grants go, or a forgotten account would leave a live CLI token in
    // the store with nothing left to name it (SWITCHER §3).
    match app.store.delete_cli(&id) {
        Ok(()) | Err(CredError::NotFound(_) | CredError::Corrupt(_)) => {}
        Err(e) => tracing::error!(id = %id, error = %e, "delete cli token"),
    }
    // Any code minted for this account is now unspendable; drop it early
    // rather than let it sit until its TTL.
    app.sessions.lock().await.sweep(Instant::now());

    broadcast_state(&app).await;
    if existed {
        StatusCode::NO_CONTENT.into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

/// SSE stream: an immediate snapshot, then every broadcast frame. Keep-alive
/// comments hold the connection open between updates.
async fn events(
    State(app): State<AppState>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let rx = app.events.subscribe();
    let initial = {
        let rows = app.rows.read().await;
        SseMsg {
            event: "accounts",
            data: serde_json::to_string(&state::snapshot(&rows)).unwrap_or_else(|_| "[]".into()),
        }
    };
    let stream = tokio_stream::once(initial)
        .chain(BroadcastStream::new(rx).filter_map(Result::ok))
        .map(|msg| Ok(SseEvent::default().event(msg.event).data(msg.data)));
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Append (or replace) an account in the registry and persist it. Holds
/// `registry_lock` across the read-modify-write so a concurrent delete can't
/// overwrite it with a stale copy.
async fn persist_account(app: &AppState, acc: RegAccount) {
    let _guard = app.registry_lock.lock().await;
    let mut reg = Registry::load(&app.registry_path);
    reg.accounts.retain(|a| a.id != acc.id);
    reg.accounts.push(acc);
    if let Err(e) = reg.save(&app.registry_path) {
        tracing::error!(error = %e, "persist registry");
    }
}

/// Serialize the current rows and push them to every `/events` subscriber.
pub async fn broadcast_state(app: &AppState) {
    let data = {
        let rows = app.rows.read().await;
        serde_json::to_string(&state::snapshot(&rows)).unwrap_or_else(|_| "[]".into())
    };
    let _ = app.events.send(SseMsg {
        event: "accounts",
        data,
    });
}

/// Spawn the poll loop for one account and register its handle for later abort.
/// A finished JoinHandle left behind in `tasks` is harmless — spawn and
/// reconnect abort-and-replace it.
pub async fn spawn_poll_task(
    app: &AppState,
    id: String,
    cred: Credential,
    start_delay: std::time::Duration,
) {
    let handle = tokio::spawn(poll_loop(app.clone(), id.clone(), cred, start_delay));
    // Ids are unique per connect, but abort any prior handle defensively so a
    // reused id can never leak a detached duplicate poll task.
    if let Some(old) = app.tasks.lock().await.insert(id, handle) {
        old.abort();
    }
}

/// Deterministic seed stagger: the i-th account's poll task starts 2 s after
/// the previous one, so a restart with many near-expiry credentials cannot
/// burst the token endpoint. The caller adds jitter on top.
pub fn seed_delay(i: usize) -> std::time::Duration {
    std::time::Duration::from_secs(i as u64 * 2)
}

/// Retry-forever keyring write: a rotated credential the OS store refuses stays
/// pending and is retried each cycle, logged once per failure streak (plan §4).
fn persist(app: &AppState, id: &str, cred: &Credential, poll: &mut PollState) {
    match app.store.put(id, cred) {
        Ok(()) => {
            poll.persist_pending = false;
            poll.persist_logged = false;
        }
        Err(e) => {
            poll.persist_pending = true;
            if !poll.persist_logged {
                tracing::error!(id = %id, error = %e, "persist rotated credential");
                poll.persist_logged = true;
            } else {
                tracing::debug!(id = %id, "persist still failing");
            }
        }
    }
}

type Fingerprint = (u8, i64, i64, u8, bool, bool, u64);

/// The row's stored state and its displayed fingerprint, or `None` if deleted.
async fn read_current(app: &AppState, id: &str) -> Option<(AccountState, Fingerprint)> {
    let rows = app.rows.read().await;
    rows.get(id).map(|row| {
        (
            row.state.clone(),
            fingerprint(&row.state, row.refresh, row.stale, row.persist_pending),
        )
    })
}

/// Mirror the poll bookkeeping and credential expiry onto the shared row.
/// Returns false when the row is gone (account deleted) so the loop can end.
async fn sync_row(
    app: &AppState,
    id: &str,
    state: AccountState,
    cred: &Credential,
    poll: &PollState,
) -> bool {
    let mut rows = app.rows.write().await;
    match rows.get_mut(id) {
        Some(row) => {
            row.state = state;
            row.stale = poll.stale;
            row.refresh = poll.refresh_status;
            row.persist_pending = poll.persist_pending;
            row.access_expires_at = cred.expires_at_utc();
            row.refreshed_at = poll.refreshed_at;
            row.last_ok_at = poll.last_success;
            true
        }
        None => false,
    }
}

/// One account's poll loop: refresh if due → fetch → apply → update → broadcast
/// → sleep (M1b.5). At most one token refresh per iteration; a rejected refresh
/// or a post-refresh 401 ends the task — `reconnect needed` rows have no
/// running task (plan §4). Never holds the rows lock across an await. Exits if
/// the row is gone (deleted).
async fn poll_loop(
    app: AppState,
    id: String,
    mut cred: Credential,
    start_delay: std::time::Duration,
) {
    let mut poll = PollState::default();
    tokio::time::sleep(start_delay).await;
    loop {
        let now = OffsetDateTime::now_utc();
        if needs_refresh(&cred, &poll, now) {
            tracing::debug!(id = %id, "refreshing access token");
            let result = app.refresher.refresh(&cred.refresh_token).await;
            match apply_refresh(result, &cred, &mut poll, now) {
                RefreshStep::Rotated(next) => {
                    cred = next;
                    persist(&app, &id, &cred, &mut poll);
                    tracing::info!(id = %id, expires_at = cred.expires_at, "access token refreshed");
                }
                RefreshStep::Reconnect => {
                    if sync_row(&app, &id, AccountState::ReconnectNeeded, &cred, &poll).await {
                        broadcast_state(&app).await;
                    }
                    return;
                }
                RefreshStep::Backoff => {
                    let sleep = refresh_backoff(poll.refresh_attempt);
                    let Some((current, before_fp)) = read_current(&app, &id).await else {
                        return;
                    };
                    // Downgrade *before* a sleep that would carry the kept
                    // numbers past staleness — never render aged data as fresh.
                    let next = if sleep_crosses_stale(&poll, now, sleep)
                        && matches!(current, AccountState::Available(_))
                    {
                        poll.stale = false;
                        AccountState::Unavailable
                    } else {
                        current
                    };
                    let after_fp =
                        fingerprint(&next, poll.refresh_status, poll.stale, poll.persist_pending);
                    if !sync_row(&app, &id, next, &cred, &poll).await {
                        return;
                    }
                    if before_fp != after_fp {
                        broadcast_state(&app).await;
                    }
                    tokio::time::sleep(sleep).await;
                    continue;
                }
            }
        } else if poll.persist_pending {
            persist(&app, &id, &cred, &mut poll);
        }

        let result = app.source.fetch(&cred.access_token).await;
        let now = OffsetDateTime::now_utc();
        match &result {
            Ok(_) => tracing::debug!(id = %id, "poll ok"),
            Err(e) => tracing::warn!(id = %id, error = %e, "poll failed"),
        }

        let Some((current, before_fp)) = read_current(&app, &id).await else {
            return; // account deleted while we were fetching
        };
        let (next_state, step) = apply_fetch(result, &current, &mut poll, now);
        let sleep = match step {
            Step::Normal => cuw_core::poller::next_interval(),
            Step::Backoff => cuw_core::poller::backoff(poll.attempt),
            Step::Idle => std::time::Duration::ZERO,
        };
        let next_state = if step == Step::Backoff
            && sleep_crosses_stale(&poll, now, sleep)
            && matches!(next_state, AccountState::Available(_))
        {
            poll.stale = false;
            AccountState::Unavailable
        } else {
            next_state
        };
        let after_fp = fingerprint(
            &next_state,
            poll.refresh_status,
            poll.stale,
            poll.persist_pending,
        );
        if !sync_row(&app, &id, next_state, &cred, &poll).await {
            return;
        }
        if before_fp != after_fp {
            broadcast_state(&app).await;
        }
        if step == Step::Idle {
            // A dead or wrongly scoped token never self-heals; the task ends
            // and only a reconnect replaces it (plan §4).
            return;
        }
        tokio::time::sleep(sleep).await;
    }
}

/// Map a connect-flow event to an SSE frame. No variant carries a token; the
/// `Output` line is already redacted upstream (plan §5).
fn connect_msg(ev: &ConnectEvent) -> SseMsg {
    let data = match ev {
        ConnectEvent::Started => json!({ "phase": "started" }),
        ConnectEvent::Output(line) => json!({ "phase": "output", "line": line }),
        ConnectEvent::SignInUrl(url) => json!({ "phase": "url", "url": url }),
        ConnectEvent::AwaitingCode => json!({ "phase": "awaiting_code" }),
        ConnectEvent::TokenCaptured => json!({ "phase": "token_captured" }),
        ConnectEvent::SetupTokenStarted => json!({ "phase": "setup_token" }),
        ConnectEvent::CliTokenCaptured => json!({ "phase": "cli_token_captured" }),
        ConnectEvent::Validated { id, label } => {
            json!({ "phase": "validated", "id": id, "label": label })
        }
        ConnectEvent::Failed(message) => json!({ "phase": "failed", "message": message }),
    };
    SseMsg {
        event: "connect",
        data: data.to_string(),
    }
}
