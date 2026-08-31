//! cuw-daemon: polls each account and serves usage over localhost. The only
//! process that touches tokens (plan §5).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;

use anyhow::Context;
use cuw_core::model::AccountState;
use cuw_core::refresh::OAuthTokenClient;
use cuw_creds::{CredentialStore, KeyringStore};
use cuw_daemon::http::{self, AppState, SharedSource};
use cuw_daemon::registry::Registry;
use cuw_daemon::session::Nonces;
use cuw_daemon::state::{self, Row};
use cuw_daemon::{auth, config, startup};
use rand::Rng;
use time::OffsetDateTime;
use tokio::sync::{broadcast, Mutex, Notify, RwLock};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Default to a useful level so connect/poll problems are visible in the log
    // the overlay captures; `RUST_LOG` still overrides.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,cuw_daemon=debug"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let cfg = config::Config::load().unwrap_or_default();
    let data_dir = startup::data_dir()?;

    // The single-instance guard, and the reason it comes first: a second daemon
    // on the same data dir shares the registry and the keyring with the one
    // already running, and two writers there cost accounts (STATUS). Nothing
    // shared is read or written until this bind succeeds.
    let addr = ("127.0.0.1", cfg.port);
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(e) if startup::port_taken(&e) => {
            tracing::error!(
                port = cfg.port,
                "127.0.0.1:{} is already in use — another cuw-daemon owns this data dir; exiting",
                cfg.port
            );
            std::process::exit(2);
        }
        Err(e) => return Err(e).with_context(|| format!("bind 127.0.0.1:{}", cfg.port)),
    };

    let bearer = auth::load_or_create(&data_dir).context("bearer token")?;
    let registry_path = data_dir.join("registry.toml");

    let store: Arc<dyn CredentialStore> = Arc::new(KeyringStore);
    let (events, _) = broadcast::channel(256);

    let app = AppState {
        rows: Arc::new(RwLock::new(HashMap::new())),
        source: SharedSource::live(),
        store,
        bearer: Arc::new(bearer),
        events,
        tasks: Arc::new(Mutex::new(HashMap::new())),
        registry_path: Arc::new(registry_path.clone()),
        scratch_root: Arc::new(data_dir.join("scratch")),
        registry_lock: Arc::new(Mutex::new(())),
        connect_input: Arc::new(Mutex::new(None)),
        refresher: Arc::new(OAuthTokenClient::default()),
        shutdown: Arc::new(Notify::new()),
        connect_task: Arc::new(Mutex::new(None)),
        sessions: Arc::new(Mutex::new(Nonces::default())),
        launcher: Arc::from(cuw_launch::for_this_platform(&data_dir)),
        // Filled in once the listener binds; a launch before then is refused
        // rather than pointed at a port nothing is listening on.
        port: Arc::new(AtomicU16::new(0)),
        default_cwd: Arc::new(default_cwd()),
    };

    // The overlay reads the pid as a last-resort kill if `POST /shutdown` fails.
    let pid_path = data_dir.join("pid");
    if let Err(e) = std::fs::write(&pid_path, std::process::id().to_string()) {
        tracing::warn!(error = %e, "write pid file");
    }

    sweep_scratch(&app.scratch_root);
    seed_from_registry(&app, &registry_path).await;

    // Publish the actually-bound port so the overlay reads it instead of assuming
    // 8787 — the single source of truth for the localhost address (plan §5).
    let bound_port = listener.local_addr().map(|a| a.port()).unwrap_or(cfg.port);
    let _ = std::fs::write(data_dir.join("port"), bound_port.to_string());
    // The shim is told this port on the command line, so it must be the bound
    // one, not the configured one (SWITCHER §4).
    app.port.store(bound_port, Ordering::Relaxed);
    tracing::info!(port = bound_port, "cuw-daemon serving");

    // The graceful path: ctrl-c or `POST /shutdown` → abort the connect task
    // (its drop scrubs the scratch dir and kills the CLI) and every poll task,
    // let axum drain, and cap the whole exit at 3 s so a stuck connection can
    // never wedge the process.
    let (fired_tx, fired_rx) = tokio::sync::oneshot::channel::<()>();
    let graceful = {
        let app = app.clone();
        async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = app.shutdown.notified() => {},
            }
            tracing::info!("shutdown requested");
            if let Some(handle) = app.connect_task.lock().await.take() {
                handle.abort();
            }
            for (_, handle) in app.tasks.lock().await.drain() {
                handle.abort();
            }
            let _ = fired_tx.send(());
        }
    };

    let mut server = tokio::spawn(async move {
        axum::serve(listener, http::router(app))
            .with_graceful_shutdown(graceful)
            .await
    });

    tokio::select! {
        res = &mut server => {
            res.context("serve task")?.context("serve")?;
        }
        _ = fired_rx => {
            match tokio::time::timeout(std::time::Duration::from_secs(3), server).await {
                Ok(res) => {
                    res.context("serve task")?.context("serve")?;
                }
                Err(_) => {
                    tracing::warn!("shutdown timed out; exiting");
                    let _ = std::fs::remove_file(&pid_path);
                    std::process::exit(0);
                }
            }
        }
    }

    let _ = std::fs::remove_file(&pid_path);
    Ok(())
}

/// Where a switched session starts when the overlay names no directory: the
/// user's home, falling back to wherever the daemon itself was started.
fn default_cwd() -> std::path::PathBuf {
    directories::UserDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// Remove every leftover connect scratch dir: a crash mid-connect can leave a
/// plaintext credential behind (plan §4). Only the count is logged.
fn sweep_scratch(root: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut count = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        // A stray file under the root is not a scratch dir; count only what went.
        let removed = if path.is_dir() {
            std::fs::remove_dir_all(&path).is_ok()
        } else {
            std::fs::remove_file(&path).is_ok()
        };
        if removed {
            count += 1;
        }
    }
    if count > 0 {
        tracing::info!(count, "swept scratch dirs");
    }
}

/// Load the daemon-owned registry and start a poll task per account. An account
/// whose credential is gone shows `reconnect needed` instead of polling.
async fn seed_from_registry(app: &AppState, registry_path: &std::path::Path) {
    let reg = Registry::load(registry_path);
    for (i, acc) in reg.accounts.into_iter().enumerate() {
        let connected_at =
            state::parse_rfc3339(&acc.connected_at).unwrap_or_else(OffsetDateTime::now_utc);

        // Absent is ordinary: the account predates M7.2, or its `setup-token`
        // step did not complete. The row shows `switch unavailable` until a
        // reconnect captures one (SWITCHER §3).
        let can_switch = app.store.get_cli(&acc.id).is_ok();

        match app.store.get(&acc.id) {
            Ok(cred) => {
                let mut row = Row::new(acc.label, AccountState::Unavailable, connected_at);
                row.can_switch = can_switch;
                app.rows.write().await.insert(acc.id.clone(), row);
                // Stagger + jitter so a restart with several near-expiry
                // credentials cannot burst the token endpoint (plan §4).
                let delay = http::seed_delay(i)
                    + std::time::Duration::from_millis(rand::thread_rng().gen_range(0..=3000));
                http::spawn_poll_task(app, acc.id.clone(), cred, delay).await;
                tracing::info!(id = %acc.id, "polling account");
            }
            // `%e` only: a CredError must never be formatted with `?e` (plan §5).
            Err(e) => {
                tracing::warn!(id = %acc.id, error = %e, "no usable credential; needs reconnect");
                // Switching does not depend on the widget's own credential —
                // the two grants are independent (SWITCHER §3) — so a row that
                // cannot poll can still open a session.
                let mut row = Row::new(acc.label, AccountState::ReconnectNeeded, connected_at);
                row.can_switch = can_switch;
                app.rows.write().await.insert(acc.id.clone(), row);
            }
        }
    }
}
