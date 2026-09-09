//! The "add account" flow. Shells out to `claude auth login --claudeai` in a PTY
//! under a scratch `CLAUDE_CONFIG_DIR` the daemon owns, so the user's real login
//! is untouched, reads the `.credentials.json` the CLI writes there, checks the
//! scopes, validates the access token once, and returns the [`Credential`]
//! (plan §4). Nothing the CLI prints is kept.
//!
//! The scratch dir holds a live access **and** refresh token in plaintext while
//! the flow runs, so its lifecycle is a hard rule: the child is awaited after
//! the kill, the file is overwritten, the dir removed with retries, and a
//! [`ScratchGuard`] repeats that on every exit path including a panic or an
//! aborted task. The daemon sweeps leftovers at startup.
//!
//! `claude auth login` is an interactive TUI: at startup it emits terminal
//! capability queries (e.g. `ESC[6n`, cursor-position report) and *blocks* until
//! the terminal answers, and after the browser sign-in it may ask for an
//! authorization code pasted back. So the PTY driver must both **answer those
//! queries** and **forward a pasted code in** — a read-only PTY hangs before the
//! browser ever opens.
//!
//! The scratch dir's `.claude.json` also ends up holding the `oauthAccount`
//! block the CLI writes after a login — who the account is, by uuid and email.
//! It is read alongside the credential so a later switch can put the same
//! identity into the real config; missing it is a degraded state, never a
//! failure.

use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cuw_core::client::{FetchError, UsageSource};
use cuw_core::redact;
use cuw_core::Credential;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::MissedTickBehavior;

/// The PTY input side, shared between the query auto-responder (on the read
/// thread) and the pasted-code forwarder (on an async task).
type PtyWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// Overall cap on the login. Generous — the user has to sign in and maybe paste
/// a code — but bounded so an abandoned flow can't wedge the connect slot.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(300);

/// PTY width for the login TUI.
const LOGIN_COLS: u16 = 120;

/// After the credential lands, how long the CLI is left running for it to
/// fetch the profile and write `oauthAccount` into the scratch config. It does
/// that right after saving the tokens, so this rarely waits out.
const ACCOUNT_GRACE: Duration = Duration::from_secs(5);

/// How often the scratch dir is checked for the credential file.
const CRED_POLL: Duration = Duration::from_millis(250);

/// How many times the credential file is re-read after the CLI is gone: it is
/// written just before exit, so the file can lag the exit by a moment.
const CRED_RETRIES: usize = 4;

/// Removal of the scratch dir is retried: a dying CLI can still hold handles in
/// it for a moment after it exits.
const SCRUB_ATTEMPTS: usize = 5;
const SCRUB_RETRY: Duration = Duration::from_millis(200);

/// Env vars that would make the CLI skip the login, bind another identity, or
/// put the credential somewhere the scratch dir cannot be read back from. The
/// last one forces the CLI's OS-store backend, which writes the OS credential
/// store instead of `.credentials.json` and would end every connect in
/// [`ConnectError::NoCredential`].
const SCRUBBED_ENV: [&str; 6] = [
    "CLAUDE_CODE_OAUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_FORCE_WINDOWS_CREDMAN",
];

/// The CLI, as spawned when it is simply on `PATH`.
const CLAUDE_BIN: &str = "claude";

/// Cap on asking the login shell where `claude` is. Short: it runs once, inside
/// the connect flow, and a shell that hangs must not hold the flow up.
#[cfg(unix)]
const SHELL_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Streamed to the UI so the sign-in and code-paste steps stay visible and
/// interactive. No variant ever carries a token — the credential leaves only
/// inside [`Connected`] (plan §5).
#[derive(Debug, Clone)]
pub enum ConnectEvent {
    Started,
    /// A cleaned PTY output line, already redacted: any token substring is
    /// replaced with its short form before it reaches this event.
    Output(String),
    /// The sign-in URL, parsed from the CLI output, so the UI can show a
    /// clickable link even if the auto-opened browser fails.
    SignInUrl(String),
    /// The CLI is now waiting for the authorization code to be pasted back.
    AwaitingCode,
    /// The credential file has been read from the scratch dir.
    TokenCaptured,
    Validated {
        id: String,
        label: String,
    },
    Failed(String),
}

/// What [`ConnectError::NoCredential`] says. The CLI writes the credential into
/// `CLAUDE_CONFIG_DIR`, so on Windows an empty scratch dir means the login did
/// not complete.
#[cfg(not(target_os = "macos"))]
const NO_CREDENTIAL: &str = "claude auth login finished without writing a credential";

/// macOS has a second place to look before giving up: the CLI stores the
/// credential in the login Keychain by default, under an item named after the
/// config dir — so the scratch flow's item is distinct from the user's real
/// login and safe to read and delete. This message only shows when that read
/// failed too.
#[cfg(target_os = "macos")]
const NO_CREDENTIAL: &str = "claude auth login finished without writing a credential into the \
     scratch config dir, and no matching `Claude Code-credentials-<8 hex>` item could be read \
     from the login Keychain. Report the CLI version.";

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("{}", NO_CREDENTIAL)]
    NoCredential,
    #[error("connect timed out")]
    TimedOut,
    #[error("token rejected by the usage endpoint")]
    Invalid,
    #[error("signed in, but the login lacks the usage scope user:profile (got: {0})")]
    Forbidden(String),
    #[error("spawn failed: {0}")]
    Spawn(String),
}

/// What the caller supplies. `scratch_root` is the daemon's `data/scratch`;
/// `existing_id` reconnects an account under its old id instead of minting one.
pub struct ConnectRequest {
    pub label: String,
    pub existing_id: Option<String>,
    pub scratch_root: PathBuf,
}

/// A validated account. `credential` is the only place a secret travels; the
/// caller (the daemon) stores it in the OS store and never echoes it (plan
/// §5). `Credential` redacts in `Debug`, so deriving here is safe.
#[derive(Debug, Clone)]
pub struct Connected {
    pub id: String,
    pub label: String,
    pub credential: Credential,
    /// The CLI's `oauthAccount` block for this login, when the CLI wrote one
    /// in time: uuid, email, org. No secret in it. `None` means a switch to
    /// this account cannot update what `/status` shows, and the daemon cannot
    /// tell a token the CLI rotated from another account's.
    pub account: Option<Value>,
}

/// Run the connect flow to completion: `claude auth login --claudeai` under a
/// scratch `CLAUDE_CONFIG_DIR` the daemon owns. Its output is streamed (cleaned
/// and redacted) via `emit`, any code arriving on `code_rx` is forwarded into
/// the PTY, and the credential the CLI writes is read back, scope-gated and
/// validated once via `source.fetch`.
///
/// On success returns [`Connected`]; the secret appears only there, never in an
/// emitted event.
///
/// `label` is user-supplied: the usage payload carries no email/org (S1 Q4),
/// so the row label cannot be derived and must come from the connect modal.
///
/// The scratch dir is scrubbed on every exit path (see [`ScratchGuard`]).
pub async fn connect<S, F>(
    source: &S,
    req: ConnectRequest,
    emit: F,
    mut code_rx: UnboundedReceiver<String>,
) -> Result<Connected, ConnectError>
where
    S: UsageSource,
    F: Fn(ConnectEvent),
{
    emit(ConnectEvent::Started);

    let ConnectRequest {
        label,
        existing_id,
        scratch_root,
    } = req;
    let id = existing_id.unwrap_or_else(|| make_id(&label));

    // Held across the whole flow; scrubbed explicitly below and again on drop.
    let mut scratch = ScratchGuard::create(&scratch_root).map_err(|e| fail(&emit, e))?;
    let config_dir = scratch.path().to_path_buf();

    // One scrub for every outcome: the dir has to outlive the CLI run and the
    // validating fetch, and it dies exactly once, on whichever path we leave by.
    let outcome = signed_in(source, &config_dir, &emit, &mut code_rx).await;
    scratch.scrub_async().await;
    let (credential, account) = outcome?;

    emit(ConnectEvent::Validated {
        id: id.clone(),
        label: label.clone(),
    });
    Ok(Connected {
        id,
        label,
        credential,
        account,
    })
}

/// Everything that needs the scratch dir signed in: the login, then the scope
/// gate and validating fetch. Split out of [`connect`] so the scrub sits on one
/// path. Errors are already announced through `emit` by the time they are
/// returned.
async fn signed_in<S, F>(
    source: &S,
    config_dir: &Path,
    emit: &F,
    code_rx: &mut UnboundedReceiver<String>,
) -> Result<(Credential, Option<Value>), ConnectError>
where
    S: UsageSource,
    F: Fn(ConnectEvent),
{
    let run = run_cli(&["auth", "login", "--claudeai"], config_dir, emit, code_rx).await?;

    let cred = match run.credential {
        Some(c) => c,
        None if run.timed_out => return Err(fail(emit, ConnectError::TimedOut)),
        None => return Err(fail(emit, ConnectError::NoCredential)),
    };
    validate(source, &cred, emit).await?;

    if run.account.is_none() {
        emit(ConnectEvent::Output(
            "The CLI did not record who this account is in time; switching to it \
             will still work, but `/status` may keep showing the previous identity."
                .into(),
        ));
    }
    Ok((cred, run.account))
}

/// What one [`run_cli`] came back with.
#[derive(Default)]
struct Run {
    credential: Option<Credential>,
    /// The `oauthAccount` block, when the CLI wrote it before teardown.
    account: Option<Value>,
    timed_out: bool,
}

/// One `claude` invocation in a PTY under `config_dir`. Answers the TUI's
/// terminal-capability queries so it can proceed, streams cleaned and redacted
/// output through `emit`, forwards pasted codes from `code_rx`, and returns once
/// the credential (and, within a grace period, the account block) has landed,
/// the CLI exits, or [`CONNECT_TIMEOUT`] passes. `auth login` writes the
/// credential and then lingers on a success screen, so the file — not the exit
/// — is the signal.
async fn run_cli<F>(
    args: &[&str],
    config_dir: &Path,
    emit: &F,
    code_rx: &mut UnboundedReceiver<String>,
) -> Result<Run, ConnectError>
where
    F: Fn(ConnectEvent),
{
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 30,
            cols: LOGIN_COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| fail(emit, ConnectError::Spawn(format!("openpty: {e}"))))?;

    let cmd = build_command(config_dir, args);
    let child = pair.slave.spawn_command(cmd).map_err(|e| {
        fail(
            emit,
            ConnectError::Spawn(format!(
                "could not run `claude`: {e} — {}",
                cli_missing_hint()
            )),
        )
    })?;

    // A handle to kill the child from the async side if the run is abandoned;
    // taken before `child` moves into the read thread.
    let mut killer = child.clone_killer();

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| fail(emit, ConnectError::Spawn(format!("pty reader: {e}"))))?;
    let writer: PtyWriter =
        Arc::new(Mutex::new(pair.master.take_writer().map_err(|e| {
            fail(emit, ConnectError::Spawn(format!("pty writer: {e}")))
        })?));

    // Drop the slave now; the master stays in this scope and is dropped at the
    // end to close the PTY. On Windows (ConPTY) that close — not the child's
    // exit — is what makes the reader return EOF, so the read thread must never
    // own the master or nothing could ever unblock it.
    drop(pair.slave);
    let master = pair.master;

    let (otx, mut orx) = tokio::sync::mpsc::unbounded_channel::<String>();

    // Blocking read loop on a plain thread: answer terminal queries inline (they
    // arrive with no trailing newline, so line-buffering would never surface
    // them) and forward raw output for cleaning and emit. It is never joined;
    // it exits once the PTY is closed below.
    let writer_r = writer.clone();
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    respond_to_queries(&buf[..n], &writer_r);
                    if otx
                        .send(String::from_utf8_lossy(&buf[..n]).into_owned())
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });

    // The child's exit is watched separately from its output, since the two are
    // not linked on ConPTY.
    let mut child = child;
    let mut child_exit = tokio::task::spawn_blocking(move || child.wait());

    let paths = ScratchPaths::new(config_dir);
    let mut tick = tokio::time::interval(CRED_POLL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut scan = OutputScan::default();
    let mut run = Run::default();
    let mut child_done = false;
    // Once the modal is gone the channel closes; without this the arm would
    // keep matching `None` and spin the loop.
    let mut code_open = true;
    // Set once the credential is in hand: the CLI gets [`ACCOUNT_GRACE`] more
    // to write the account block before it is torn down.
    let mut grace: Option<tokio::time::Instant> = None;
    let deadline = tokio::time::sleep(CONNECT_TIMEOUT);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            maybe = orx.recv() => match maybe {
                Some(raw) => {
                    scan.feed(&raw, emit);
                    if capture(&paths, &scan, &mut run, emit, &mut grace) {
                        break;
                    }
                }
                // The PTY closed. On ConPTY that cannot happen while the master
                // is alive, but on a platform where the child's exit EOFs the
                // reader this arm wins the race with the tick, so anything the
                // CLI already produced must not be dropped here.
                None => {
                    capture_retrying(&paths, &scan, &mut run, emit).await;
                    break;
                }
            },
            // The pasted authorization code, terminated with a carriage return
            // so the CLI's prompt reads it as one submission.
            maybe = code_rx.recv(), if code_open => match maybe {
                Some(code) => {
                    let mut line = code.trim_end_matches(['\r', '\n']).as_bytes().to_vec();
                    line.push(b'\r');
                    if let Ok(mut w) = writer.lock() {
                        let _ = w.write_all(&line);
                        let _ = w.flush();
                    }
                }
                None => code_open = false,
            },
            _ = tick.tick() => {
                if capture(&paths, &scan, &mut run, emit, &mut grace) {
                    break;
                }
            }
            _ = &mut child_exit => {
                child_done = true;
                // The CLI exited on its own (code-paste flow, or an error). Drain
                // whatever output is still in the pipe, briefly, then give a
                // last write a moment to land.
                let until = tokio::time::Instant::now() + Duration::from_millis(800);
                while let Ok(Some(raw)) = tokio::time::timeout_at(until, orx.recv()).await {
                    scan.feed(&raw, emit);
                }
                capture_retrying(&paths, &scan, &mut run, emit).await;
                break;
            }
            _ = &mut deadline => {
                run.timed_out = true;
                break;
            }
        }
    }

    // Tear the CLI down on every path: kill the child (a no-op if it already
    // exited), close the PTY so the read thread unblocks, then wait for the
    // child to actually go away — until it does it still holds handles in the
    // scratch dir and the removal would fail.
    let _ = killer.kill();
    drop(writer);
    drop(master);
    if !child_done {
        let _ = tokio::time::timeout(Duration::from_secs(2), &mut child_exit).await;
    }
    Ok(run)
}

/// The two files a login leaves in the scratch dir.
struct ScratchPaths {
    credentials: PathBuf,
    config: PathBuf,
}

impl ScratchPaths {
    fn new(config_dir: &Path) -> ScratchPaths {
        ScratchPaths {
            credentials: config_dir.join(".credentials.json"),
            config: config_dir.join(".claude.json"),
        }
    }
}

/// Poll once. `true` when the run has what it came for: the credential, plus
/// the account block or the grace period spent waiting for it. The credential
/// is captured the first time it is seen; only the account is re-polled after.
fn capture<F: Fn(ConnectEvent)>(
    paths: &ScratchPaths,
    scan: &OutputScan,
    run: &mut Run,
    emit: &F,
    grace: &mut Option<tokio::time::Instant>,
) -> bool {
    if run.credential.is_none() {
        let found = read_credentials(&paths.credentials);
        // On macOS the CLI stores the credential in the login Keychain by
        // default and writes no file; once the login has announced success,
        // the scratch dir's own Keychain item is the place to look.
        #[cfg(target_os = "macos")]
        let found = found.or_else(|| {
            let dir = paths.credentials.parent().unwrap_or(&paths.credentials);
            scan.login_seen
                .then(|| read_keychain_credentials(dir))
                .flatten()
        });
        #[cfg(not(target_os = "macos"))]
        let _ = scan;
        match found {
            Some(c) => {
                run.credential = Some(c);
                emit(ConnectEvent::TokenCaptured);
                *grace = Some(tokio::time::Instant::now() + ACCOUNT_GRACE);
            }
            None => return false,
        }
    }
    if run.account.is_none() {
        run.account = read_account(&paths.config);
    }
    run.account.is_some() || grace.is_some_and(|until| tokio::time::Instant::now() >= until)
}

/// [`capture`] on the way out. The credential file is written just before the
/// CLI exits, so that write can land after the exit is observed. There is no
/// grace here: the CLI is gone, so the account block is there or it is not.
async fn capture_retrying<F: Fn(ConnectEvent)>(
    paths: &ScratchPaths,
    scan: &OutputScan,
    run: &mut Run,
    emit: &F,
) {
    let mut spent = Some(tokio::time::Instant::now());
    for attempt in 0..CRED_RETRIES {
        if capture(paths, scan, run, emit, &mut spent) {
            return;
        }
        if attempt + 1 < CRED_RETRIES {
            tokio::time::sleep(CRED_POLL).await;
        }
    }
}

/// Announce a terminal failure before returning it, so a UI driven off the
/// event stream never sees `started` followed by silence. The error text is a
/// `ConnectError` display string and carries no token.
fn fail<F: Fn(ConnectEvent)>(emit: &F, e: ConnectError) -> ConnectError {
    emit(ConnectEvent::Failed(e.to_string()));
    e
}

/// Scope gate first — a login without `user:profile` can never pass the usage
/// endpoint (S1), so it must not even reach the network — then one validating
/// fetch. A 401 means the token is not accepted and a 403 that it never will
/// be. Anything else is not proof the token is bad, so keep it: the poll loop
/// already shows `unavailable` and retries (plan §3, §9).
async fn validate<S, F>(source: &S, cred: &Credential, emit: &F) -> Result<(), ConnectError>
where
    S: UsageSource,
    F: Fn(ConnectEvent),
{
    if !cred.has_usage_scope() {
        return Err(fail(emit, ConnectError::Forbidden(cred.scopes.join(" "))));
    }
    match source.fetch(&cred.access_token).await {
        Ok(_) => Ok(()),
        Err(FetchError::Unauthorized) => Err(fail(emit, ConnectError::Invalid)),
        Err(FetchError::Forbidden(_)) => {
            Err(fail(emit, ConnectError::Forbidden(cred.scopes.join(" "))))
        }
        Err(e) => {
            emit(ConnectEvent::Output(format!(
                "Validation deferred ({e}); the account will show as unavailable until a poll succeeds."
            )));
            Ok(())
        }
    }
}

/// The CLI invocation. `portable-pty` seeds the child env from the daemon's
/// whole environment, so a pre-set token or provider switch is removed first:
/// it would make the CLI skip the login, or bind another identity.
pub(crate) fn build_command(config_dir: &Path, args: &[&str]) -> CommandBuilder {
    let mut cmd = CommandBuilder::new(claude_program());
    for arg in args {
        cmd.arg(arg);
    }
    cmd.env("CLAUDE_CONFIG_DIR", config_dir);
    cmd.env("TERM", "xterm-256color");
    for key in SCRUBBED_ENV {
        cmd.env_remove(key);
    }
    cmd
}

/// Where `claude` is. Resolved once per process: the answer cannot change under
/// a running daemon, and the login-shell probe below is far too expensive to
/// repeat per connect.
#[cfg(windows)]
fn claude_program() -> &'static OsStr {
    OsStr::new(CLAUDE_BIN)
}

/// Unix has to look: a daemon started from a bundled `.app` is launched by
/// LaunchServices with a minimal `PATH` (`/usr/bin:/bin:/usr/sbin:/sbin`), so
/// the CLI the user installed is simply not on it and the whole connect flow
/// would die on spawn (plan §4). Order: `PATH`, then the login shell's own
/// `PATH`, then the documented install locations. Falling through to the bare
/// name keeps the failure a clear spawn error rather than a panic.
#[cfg(unix)]
fn claude_program() -> &'static OsStr {
    use std::ffi::OsString;
    use std::sync::OnceLock;
    static RESOLVED: OnceLock<OsString> = OnceLock::new();
    RESOLVED.get_or_init(|| {
        if on_path(CLAUDE_BIN) {
            return OsString::from(CLAUDE_BIN);
        }
        ask_login_shell()
            .or_else(well_known_claude)
            .map(PathBuf::into_os_string)
            .unwrap_or_else(|| OsString::from(CLAUDE_BIN))
    })
}

/// Whether `name` is an executable in one of `PATH`'s directories.
#[cfg(unix)]
fn on_path(name: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| is_executable(&dir.join(name)))
}

/// Ask the user's login shell where `claude` is: an interactive shell sources
/// the profile that put a version manager or `~/.local/bin` on `PATH`, which is
/// exactly what a LaunchServices-started daemon is missing. The output is only
/// trusted as far as "names a file we can execute", so a shell function or an
/// alias definition is rejected rather than spawned.
#[cfg(unix)]
fn ask_login_shell() -> Option<PathBuf> {
    use std::process::{Command, Stdio};

    let shell = std::env::var_os("SHELL")?;
    let mut child = Command::new(shell)
        .args(["-l", "-c", "command -v claude"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    // Read on a helper thread: a blocking read has no timeout of its own, and a
    // profile that waits on something would otherwise hold the connect flow.
    let mut out = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = out.read_to_string(&mut text);
        let _ = tx.send(text);
    });
    let text = rx.recv_timeout(SHELL_PROBE_TIMEOUT).ok();
    let _ = child.kill();
    let _ = child.wait();

    let path = PathBuf::from(text?.lines().next()?.trim());
    is_executable(&path).then_some(path)
}

/// The install locations the CLI documents, in the order a Mac is likely to
/// have them: the native installer, a user-local bin, then Homebrew.
#[cfg(unix)]
fn well_known_claude() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let candidates = [
        home.as_ref().map(|h| h.join(".claude/local/claude")),
        home.as_ref().map(|h| h.join(".local/bin/claude")),
        Some(PathBuf::from("/opt/homebrew/bin/claude")),
        Some(PathBuf::from("/usr/local/bin/claude")),
    ];
    candidates.into_iter().flatten().find(|p| is_executable(p))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(md) => md.is_file() && md.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

/// Appended to a spawn failure. On unix it names every place that was searched,
/// because "not on PATH" is misleading advice for a daemon whose `PATH` came
/// from LaunchServices rather than from a shell.
#[cfg(windows)]
fn cli_missing_hint() -> &'static str {
    "is the Claude Code CLI installed and on PATH?"
}

#[cfg(unix)]
fn cli_missing_hint() -> &'static str {
    "is the Claude Code CLI installed? PATH, your login shell's PATH, \
     ~/.claude/local/claude, ~/.local/bin/claude, /opt/homebrew/bin/claude and \
     /usr/local/bin/claude were all checked."
}

/// A per-flow scratch `CLAUDE_CONFIG_DIR` under the daemon's data dir. Holds
/// plaintext tokens while the CLI runs, so it is scrubbed — file overwritten,
/// dir removed with retries — both explicitly after teardown and on drop, which
/// covers a panic or an aborted task (plan §4).
pub struct ScratchGuard {
    path: PathBuf,
    scrubbed: bool,
}

impl ScratchGuard {
    pub fn create(root: &Path) -> Result<ScratchGuard, ConnectError> {
        let mk = || -> std::io::Result<PathBuf> {
            std::fs::create_dir_all(root)?;
            let path = root.join(format!("cuw-connect-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir(&path)?;
            Ok(path)
        };
        let path = mk().map_err(|e| ConnectError::Spawn(format!("scratch config dir: {e}")))?;
        Ok(ScratchGuard {
            path,
            scrubbed: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Idempotent. Logs the path only on failure — never contents or an error
    /// text that could quote them.
    pub fn scrub(&mut self) {
        if self.scrubbed {
            return;
        }
        self.overwrite_credential();
        for attempt in 0..SCRUB_ATTEMPTS {
            if self.remove_attempt() {
                return;
            }
            if attempt + 1 < SCRUB_ATTEMPTS {
                std::thread::sleep(SCRUB_RETRY);
            }
        }
        self.give_up();
    }

    /// [`ScratchGuard::scrub`] for the async flow: identical, but it yields
    /// between retries instead of parking a runtime worker for up to a second.
    /// Cancel-safe — an abort mid-retry leaves `scrubbed` false, so `Drop`
    /// finishes the job (plan §4).
    pub async fn scrub_async(&mut self) {
        if self.scrubbed {
            return;
        }
        self.overwrite_credential();
        for attempt in 0..SCRUB_ATTEMPTS {
            if self.remove_attempt() {
                return;
            }
            if attempt + 1 < SCRUB_ATTEMPTS {
                tokio::time::sleep(SCRUB_RETRY).await;
            }
        }
        self.give_up();
    }

    /// Blank the credential file first, so even a dir that cannot be removed
    /// holds no token. On macOS the credential usually lives in the Keychain
    /// instead, under this dir's own item — take that too.
    fn overwrite_credential(&self) {
        let cred = self.path.join(".credentials.json");
        if cred.exists() {
            let _ = std::fs::write(&cred, "{}");
        }
        #[cfg(target_os = "macos")]
        scrub_keychain(&self.path);
    }

    /// One removal try. `true` once the dir is gone — which also marks the
    /// guard done, so a later `scrub` (including `Drop`'s) is a no-op.
    fn remove_attempt(&mut self) -> bool {
        if !self.path.exists() || std::fs::remove_dir_all(&self.path).is_ok() {
            self.scrubbed = true;
            return true;
        }
        false
    }

    /// Out of attempts: warn once (path only) and stop retrying on drop.
    fn give_up(&mut self) {
        self.scrubbed = true;
        tracing::warn!(path = %self.path.display(), "scratch dir not removed");
    }
}

impl Drop for ScratchGuard {
    fn drop(&mut self) {
        self.scrub();
    }
}

/// The credential the CLI wrote, if the file is complete and well-formed. Any
/// failure is `None`: a half-written file is normal mid-flow, and an error text
/// could quote the contents, so nothing is logged with it.
fn read_credentials(path: &Path) -> Option<Credential> {
    let text = std::fs::read_to_string(path).ok()?;
    let cred = cuw_switch::parse_cli_json(&text);
    if cred.is_none() {
        tracing::trace!("credentials file not ready");
    }
    cred
}

/// The `oauthAccount` block of the scratch dir's `.claude.json`, once the CLI
/// has written one that names an account. Nothing in it is secret.
fn read_account(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let account = v.get("oauthAccount")?;
    cuw_switch::account_uuid(account)
        .is_some()
        .then(|| account.clone())
}

/// The credential the CLI stored in the login Keychain for this scratch dir,
/// if any. Same JSON as `.credentials.json`. The item's data is a token, so a
/// failure to read it is simply `None`.
#[cfg(target_os = "macos")]
fn read_keychain_credentials(config_dir: &Path) -> Option<Credential> {
    let service = cuw_switch::keychain::service(Some(config_dir));
    let text = cuw_switch::keychain::read(&service).ok().flatten()?;
    cuw_switch::parse_cli_json(&text)
}

/// Delete the Keychain item the CLI may have created for `config_dir` — the
/// Keychain half of scrubbing a scratch dir. Best effort; the item names only
/// this one flow, never the user's real Claude Code login.
#[cfg(target_os = "macos")]
pub fn scrub_keychain(config_dir: &Path) {
    cuw_switch::keychain::delete(&cuw_switch::keychain::service(Some(config_dir)));
}

/// Answer the terminal-capability queries a TUI blocks on. `claude` sends
/// `ESC[6n` (cursor-position report) at startup and waits for a reply; without
/// one it never opens the browser. Also handles device-status and
/// device-attributes queries defensively.
fn respond_to_queries(chunk: &[u8], writer: &PtyWriter) {
    let resp = query_reply(chunk);
    if !resp.is_empty() {
        if let Ok(mut w) = writer.lock() {
            let _ = w.write_all(&resp);
            let _ = w.flush();
        }
    }
}

/// The bytes to write back for whatever queries appear in `chunk`. Split out so
/// the mapping is unit-tested without a real PTY.
fn query_reply(chunk: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    if contains(chunk, b"\x1b[6n") {
        out.extend_from_slice(b"\x1b[24;1R"); // cursor at row 24, col 1
    }
    if contains(chunk, b"\x1b[5n") {
        out.extend_from_slice(b"\x1b[0n"); // device OK
    }
    if contains(chunk, b"\x1b[c") || contains(chunk, b"\x1b[0c") {
        out.extend_from_slice(b"\x1b[?1;2c"); // primary DA: VT100 with AVO
    }
    if contains(chunk, b"\x1b[>c") {
        out.extend_from_slice(b"\x1b[>0;0;0c"); // secondary DA
    }
    out
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Accumulates the CLI's raw output and turns it into UI signals: the sign-in
/// URL, the awaiting-code prompt, and cleaned output lines. Split out from
/// [`connect`] so the pipeline is testable against a recorded transcript
/// without a live `claude`.
#[derive(Default)]
struct OutputScan {
    /// ANSI-stripped text seen so far, for cross-chunk prompt detection.
    acc: String,
    /// Last emitted line, to drop the TUI's repeated redraws.
    last: String,
    /// The login announced success. Gates the Keychain probe on macOS, so a
    /// `security` process is not spawned on every output chunk.
    login_seen: bool,
    url_sent: bool,
    await_sent: bool,
}

impl OutputScan {
    fn feed<F: Fn(ConnectEvent)>(&mut self, raw: &str, emit: &F) {
        if !self.url_sent {
            if let Some(url) = find_signin_url(raw) {
                emit(ConnectEvent::SignInUrl(url));
                self.url_sent = true;
            }
        }

        let clean = strip_ansi(raw);
        self.acc.push_str(&clean);
        self.cap_acc();

        if !self.await_sent && clean.to_ascii_lowercase().contains("paste code") {
            emit(ConnectEvent::AwaitingCode);
            self.await_sent = true;
        }
        if !self.login_seen && self.acc.to_ascii_lowercase().contains("login successful") {
            self.login_seen = true;
        }

        for seg in clean.split(['\n', '\r']) {
            let s = seg.trim();
            if !s.is_empty() && s != self.last {
                emit(ConnectEvent::Output(scrub(s)));
                self.last = s.to_string();
            }
        }
    }

    /// Keep the accumulator bounded; the prompt is near the end, so trimming
    /// the front is safe. Trim on a char boundary.
    fn cap_acc(&mut self) {
        const CAP: usize = 262_144;
        if self.acc.len() > CAP {
            let mut cut = self.acc.len() - CAP;
            while cut < self.acc.len() && !self.acc.is_char_boundary(cut) {
                cut += 1;
            }
            self.acc.drain(..cut);
        }
    }
}

/// Parse the sign-in URL out of an OSC-8 hyperlink (`ESC ] 8 ; params ; URI ST`).
/// `claude` prints the URL both as a hyperlink and as wrapped visible text; the
/// hyperlink target is the one clean, untruncated copy.
fn find_signin_url(raw: &str) -> Option<String> {
    let mut rest = raw;
    loop {
        let p = rest.find("\x1b]8;")?;
        let after = &rest[p + 4..];
        let semi = after.find(';')?;
        let uri_part = &after[semi + 1..];
        let end = uri_part.find('\x07').or_else(|| uri_part.find("\x1b\\"))?;
        let uri = &uri_part[..end];
        if uri.starts_with("http") && uri.contains("authorize") {
            return Some(uri.to_string());
        }
        rest = &uri_part[end..];
    }
}

/// Remove ANSI/OSC escape sequences and stray control bytes, preserving the
/// visible (UTF-8) text. Spans between escapes are copied whole, so multibyte
/// characters are never split.
fn strip_ansi(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            0x1b => {
                let next = skip_escape(b, i);
                // A cursor move or erase means the next text is painted
                // somewhere else on screen: without a separator, two unrelated
                // paints would glue into one run — which is how a captured
                // token once grew a tail of UI prose. SGR (`m`) is only color.
                if moves_cursor(&b[i..next]) {
                    out.push('\n');
                }
                i = next;
            }
            0x08 => {
                // Backspace moves the cursor too.
                out.push('\n');
                i += 1;
            }
            0x07 => i += 1, // BEL
            _ => {
                let start = i;
                while i < b.len() && !matches!(b[i], 0x1b | 0x07 | 0x08) {
                    i += 1;
                }
                out.push_str(&s[start..i]);
            }
        }
    }
    out
}

/// Whether one stripped escape sequence repositions or erases — anything CSI
/// that is not SGR coloring. OSC/DCS and two-byte escapes leave the cursor
/// where it is, so they stay silent and prose spanning them stays whole.
fn moves_cursor(seq: &[u8]) -> bool {
    seq.get(1) == Some(&b'[') && seq.last().is_some_and(|f| *f != b'm')
}

/// Advance past one escape sequence starting at `b[i] == ESC`; returns the index
/// just after it.
fn skip_escape(b: &[u8], i: usize) -> usize {
    if i + 1 >= b.len() {
        return i + 1;
    }
    match b[i + 1] {
        b'[' => {
            // CSI: parameters/intermediates, then a final byte 0x40..=0x7e.
            let mut j = i + 2;
            while j < b.len() && !(0x40..=0x7e).contains(&b[j]) {
                j += 1;
            }
            if j < b.len() {
                j += 1;
            }
            j
        }
        b']' | b'P' | b'X' | b'^' | b'_' => {
            // OSC/DCS/SOS/PM/APC: run until ST (ESC\) or BEL.
            let mut j = i + 2;
            while j < b.len() {
                if b[j] == 0x07 {
                    return j + 1;
                }
                if b[j] == 0x1b && j + 1 < b.len() && b[j + 1] == b'\\' {
                    return j + 2;
                }
                j += 1;
            }
            j
        }
        // A two-byte escape (e.g. ESC(B, ESC>, ESC=).
        _ => i + 2,
    }
}

/// A marker some CLI versions print a token under, so [`scrub`] stays
/// fail-closed whatever the shape.
const TOKEN_MARKER: &str = "CLAUDE_CODE_OAUTH_TOKEN";

/// The leading run of token-safe (base64url-ish) characters.
fn token_run(s: &str) -> &str {
    let end = s
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .unwrap_or(s.len());
    &s[..end]
}

/// Scrub every plausible token from a line before it is shown. Fail-closed
/// (plan §5): covers the value after [`TOKEN_MARKER`] and any `sk-ant-` run, so
/// an output-format change in the CLI still cannot leak a raw token to the UI.
fn scrub(line: &str) -> String {
    let mut out = line.to_string();

    if let Some(i) = out.find(TOKEN_MARKER) {
        let after = &out[i + TOKEN_MARKER.len()..];
        let value = token_run(after.trim_start_matches(['=', ':', ' ', '\t'])).to_string();
        if value.len() >= 8 {
            out = out.replace(&value, &redact(&value));
        }
    }

    // Redact every `sk-ant-` run, not just the first: a chunk can carry more than
    // one, and a single leak defeats the guarantee.
    let mut from = 0;
    while let Some(rel) = out[from..].find("sk-ant-") {
        let start = from + rel;
        let tok = token_run(&out[start..]).to_string();
        if tok.len() >= 8 {
            let red = redact(&tok);
            out.replace_range(start..start + tok.len(), &red);
            from = start + red.len();
        } else {
            from = start + "sk-ant-".len();
        }
    }
    out
}

/// Longest slug kept in an id; with the 8-hex suffix the id stays well under
/// the Credential Manager's 512-byte username cap, so a long label can never
/// fail the store *after* the browser login (plan §4).
const SLUG_MAX: usize = 32;

/// Stable, human-legible account id: a slug of the label plus a short uuid so
/// two accounts with the same label never collide (M1.4).
fn make_id(label: &str) -> String {
    let slug: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug: String = slug.trim_matches('-').chars().take(SLUG_MAX).collect();
    let slug = slug.trim_end_matches('-');
    let short = &uuid::Uuid::new_v4().simple().to_string()[..8];
    if slug.is_empty() {
        short.to_string()
    } else {
        format!("{slug}-{short}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cuw_core::client::RawResponse;
    use std::cell::RefCell;

    /// Tests in one binary share a process environment, and mutating it while
    /// another thread reads it is a data race. Every test that touches the
    /// environment — including a `temp_dir()` read — takes this first.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    const CREDS_FAKE: &str = r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-FAKEFAKEFAKEFAKEFAKEFAKE0001","refreshToken":"sk-ant-ort01-FAKEFAKEFAKEFAKEFAKEFAKE0002","expiresAt":1756600000000,"scopes":["user:inference","user:profile"],"subscriptionType":"max"}}"#;

    const TOKEN: &str = "sk-ant-oat01-FAKEFAKEFAKEFAKEFAKEFAKE0003";

    fn fake_cred(scopes: &[&str]) -> Credential {
        Credential::new(
            TOKEN,
            "sk-ant-ort01-FAKEFAKEFAKEFAKEFAKEFAKE0004",
            1_756_600_000,
            scopes.iter().map(|s| s.to_string()).collect(),
        )
    }

    fn sample() -> String {
        format!(
            "Opening your browser to sign in…\n\
             If the browser did not open, visit: https://claude.ai/oauth/authorize?code=1\n\
             Paste code here if prompted >\n\
             Logged in as someone@example.com\n\
             CLAUDE_CODE_OAUTH_TOKEN={TOKEN}\n\
             Login successful.\n"
        )
    }

    fn collect(chunks: &[&str]) -> Vec<ConnectEvent> {
        let events: RefCell<Vec<ConnectEvent>> = RefCell::new(Vec::new());
        let mut scan = OutputScan::default();
        for c in chunks {
            scan.feed(c, &|e| events.borrow_mut().push(e));
        }
        events.into_inner()
    }

    fn parse(text: &str) -> Option<Credential> {
        cuw_switch::parse_cli_json(text)
    }

    #[test]
    fn parses_the_cli_credentials_file() {
        let c = parse(CREDS_FAKE).expect("parses");
        assert_eq!(c.expires_at, 1_756_600_000);
        assert_eq!(c.scopes.len(), 2);
        assert!(c.has_usage_scope());
        assert!(c.access_token.starts_with("sk-ant-oat01-"));
        assert!(c.refresh_token.starts_with("sk-ant-ort01-"));
        assert_eq!(c.v, 1);
    }

    #[test]
    fn missing_claudeaioauth_is_none() {
        assert!(parse(r#"{"other":{"accessToken":"sk-ant-oat01-FAKE"}}"#).is_none());
        assert!(parse("{}").is_none());
    }

    #[test]
    fn access_token_not_a_string_is_none() {
        let text = CREDS_FAKE.replace(
            r#""accessToken":"sk-ant-oat01-FAKEFAKEFAKEFAKEFAKEFAKE0001""#,
            r#""accessToken":12345"#,
        );
        assert!(parse(&text).is_none());
    }

    #[test]
    fn empty_refresh_token_is_none() {
        let text = CREDS_FAKE.replace(
            r#""refreshToken":"sk-ant-ort01-FAKEFAKEFAKEFAKEFAKEFAKE0002""#,
            r#""refreshToken":"""#,
        );
        assert!(parse(&text).is_none());
    }

    #[test]
    fn seconds_epoch_is_not_divided() {
        let text = CREDS_FAKE.replace("1756600000000", "1756600000");
        let c = parse(&text).expect("parses");
        assert_eq!(c.expires_at, 1_756_600_000);
    }

    #[test]
    fn scopes_missing_is_empty_vec() {
        let text = CREDS_FAKE.replace(r#""scopes":["user:inference","user:profile"],"#, "");
        let c = parse(&text).expect("parses");
        assert!(c.scopes.is_empty());
        assert!(!c.has_usage_scope());
    }

    #[test]
    fn truncated_json_is_none() {
        // A non-atomic write caught mid-way.
        let cut = &CREDS_FAKE[..CREDS_FAKE.len() * 6 / 10];
        assert!(parse(cut).is_none());
    }

    #[test]
    fn scope_gate_message_lists_scopes() {
        let cred = fake_cred(&["user:inference"]);
        assert!(!cred.has_usage_scope());
        let e = ConnectError::Forbidden(cred.scopes.join(" "));
        let msg = e.to_string();
        assert!(msg.contains("user:inference"), "{msg}");
        assert!(msg.contains("user:profile"), "{msg}");
    }

    /// A source that must never be reached: proves the scope gate runs first.
    struct PanicSource;

    #[async_trait::async_trait]
    impl UsageSource for PanicSource {
        async fn fetch(&self, _token: &str) -> Result<RawResponse, FetchError> {
            panic!("the network was reached without the usage scope");
        }
    }

    #[tokio::test]
    async fn scope_gate_runs_before_any_fetch() {
        let events: RefCell<Vec<ConnectEvent>> = RefCell::new(Vec::new());
        let cred = fake_cred(&["user:inference"]);
        let res = validate(&PanicSource, &cred, &|e| events.borrow_mut().push(e)).await;
        assert!(matches!(res, Err(ConnectError::Forbidden(_))));
        let failed = events.borrow().iter().any(|e| {
            matches!(e, ConnectEvent::Failed(m) if m.contains("user:inference") && !m.contains(TOKEN))
        });
        assert!(failed, "Failed event with the scope list expected");
    }

    #[test]
    fn output_frames_are_scrubbed() {
        let events = collect(&[&sample()]);
        let mut outputs = 0;
        for e in &events {
            if let ConnectEvent::Output(s) = e {
                outputs += 1;
                assert!(!s.contains(TOKEN), "token leaked into an Output frame: {s}");
                assert!(!s.contains("FAKE"), "token body leaked: {s}");
            }
        }
        assert!(outputs > 0);
    }

    #[test]
    fn scan_signals_awaiting_code() {
        let events = collect(&[&sample()]);
        assert!(events
            .iter()
            .any(|e| matches!(e, ConnectEvent::AwaitingCode)));
    }

    #[test]
    fn scan_parses_the_osc8_signin_url() {
        let url = "https://claude.com/cai/oauth/authorize?code=true&state=xyz";
        let raw = format!("\x1b]8;id=u-1;{url}\x1b\\click here\x1b]8;;\x1b\\\n");
        let events = collect(&[&raw]);
        let got = events.iter().find_map(|e| match e {
            ConnectEvent::SignInUrl(u) => Some(u.clone()),
            _ => None,
        });
        assert_eq!(got.as_deref(), Some(url));
    }

    #[test]
    fn scrub_is_fail_closed_without_capture() {
        let labelled = format!("CLAUDE_CODE_OAUTH_TOKEN={TOKEN}");
        assert!(!scrub(&labelled).contains(TOKEN), "labelled token leaked");
        let inline = format!("your token is {TOKEN} — keep it safe");
        assert!(!scrub(&inline).contains(TOKEN), "sk-ant token leaked");
        let two = format!("{TOKEN} and again {TOKEN}");
        assert!(!scrub(&two).contains("FAKE"), "second run leaked");
    }

    #[test]
    fn credential_debug_is_redacted() {
        let c = Connected {
            id: "x-12345678".into(),
            label: "x".into(),
            credential: parse(CREDS_FAKE).expect("parses"),
            account: Some(serde_json::json!({"accountUuid":"u","emailAddress":"a@b.c"})),
        };
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("FAKE"), "{dbg}");
        assert!(dbg.contains("user:profile"), "{dbg}");
    }

    #[test]
    fn build_command_scrubs_auth_env() {
        let _env = env_guard();
        std::env::set_var("CLAUDE_CODE_OAUTH_TOKEN", "fake");
        let cmd = build_command(Path::new("scratch-dir"), &["auth", "login", "--claudeai"]);
        std::env::remove_var("CLAUDE_CODE_OAUTH_TOKEN");
        for key in SCRUBBED_ENV {
            assert!(cmd.get_env(key).is_none(), "{key} still set");
        }
        assert_eq!(
            cmd.get_env("CLAUDE_CONFIG_DIR")
                .map(|v| v.to_string_lossy().into_owned()),
            Some("scratch-dir".to_string())
        );
        assert!(cmd.get_env("TERM").is_some());
    }

    /// Windows resolves `claude` through `CreateProcess` exactly as before; the
    /// search only exists for a unix daemon started with LaunchServices' PATH.
    #[test]
    fn the_resolved_program_is_the_bare_name_on_windows() {
        if cfg!(windows) {
            assert_eq!(claude_program(), OsStr::new("claude"));
        } else {
            assert!(
                Path::new(claude_program()).ends_with("claude"),
                "{:?}",
                claude_program()
            );
        }
    }

    /// A pre-set OS-store switch would send the credential to the Keychain or
    /// Credential Manager instead of the scratch dir we read back.
    #[test]
    fn build_command_scrubs_the_os_store_switch() {
        let _env = env_guard();
        std::env::set_var("CLAUDE_CODE_FORCE_WINDOWS_CREDMAN", "1");
        let cmd = build_command(Path::new("scratch-dir"), &["auth", "login", "--claudeai"]);
        std::env::remove_var("CLAUDE_CODE_FORCE_WINDOWS_CREDMAN");
        assert!(cmd.get_env("CLAUDE_CODE_FORCE_WINDOWS_CREDMAN").is_none());
    }

    /// The message a user acts on when the scratch dir stays empty: on macOS it
    /// has to name the Keychain, which is the other place the CLI can put it.
    #[test]
    fn no_credential_explains_where_to_look() {
        let msg = ConnectError::NoCredential.to_string();
        assert!(msg.contains("without writing a credential"), "{msg}");
        if cfg!(target_os = "macos") {
            assert!(msg.contains("Keychain"), "{msg}");
        }
    }

    #[test]
    fn scratch_guard_empties_dir_on_drop() {
        let root = {
            let _env = env_guard();
            std::env::temp_dir().join(format!("cuw-test-scratch-{}", std::process::id()))
        };
        let path = {
            let guard = ScratchGuard::create(&root).expect("create");
            let path = guard.path().to_path_buf();
            assert!(path.starts_with(&root));
            assert!(path.is_dir());
            std::fs::write(
                path.join(".credentials.json"),
                r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-FAKE"}}"#,
            )
            .expect("write");
            path
        };
        assert!(!path.exists(), "scratch dir survived the guard");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scratch_guard_scrub_is_idempotent() {
        let root = {
            let _env = env_guard();
            std::env::temp_dir().join(format!("cuw-test-scratch2-{}", std::process::id()))
        };
        let mut guard = ScratchGuard::create(&root).expect("create");
        let path = guard.path().to_path_buf();
        guard.scrub();
        assert!(!path.exists());
        guard.scrub();
        drop(guard);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn scrub_async_removes_the_dir_and_is_idempotent() {
        let root = {
            let _env = env_guard();
            std::env::temp_dir().join(format!("cuw-test-scratch3-{}", std::process::id()))
        };
        let mut guard = ScratchGuard::create(&root).expect("create");
        let path = guard.path().to_path_buf();
        std::fs::write(
            path.join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-FAKE"}}"#,
        )
        .expect("write");
        guard.scrub_async().await;
        assert!(!path.exists(), "scratch dir survived scrub_async");
        guard.scrub_async().await;
        drop(guard);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_setup_failure_is_announced_before_it_is_returned() {
        let events: RefCell<Vec<ConnectEvent>> = RefCell::new(Vec::new());
        let e = fail(
            &|e| events.borrow_mut().push(e),
            ConnectError::Spawn("could not run `claude`".into()),
        );
        assert!(matches!(e, ConnectError::Spawn(_)));
        let announced = events
            .borrow()
            .iter()
            .any(|e| matches!(e, ConnectEvent::Failed(m) if m.contains("could not run `claude`")));
        assert!(announced, "no Failed event for a spawn failure");
    }

    #[test]
    fn strip_ansi_removes_csi_and_osc_keeps_text() {
        let dirty = "\x1b[38;2;1;2;3mHello\x1b[m \x1b]8;;http://x\x07world\x1b]8;;\x07 café";
        assert_eq!(strip_ansi(dirty), "Hello world café");
    }

    #[test]
    fn query_reply_answers_the_cursor_position_report() {
        assert_eq!(query_reply(b"before\x1b[6nafter"), b"\x1b[24;1R");
        assert!(query_reply(b"no queries here").is_empty());
    }

    /// Color changes are not cursor moves: prose spanning an SGR stays whole,
    /// so cross-chunk prompt detection keeps working.
    #[test]
    fn sgr_does_not_split_text_but_a_cursor_move_does() {
        assert_eq!(strip_ansi("paste \x1b[1mcode\x1b[m"), "paste code");
        assert_eq!(strip_ansi("one\x1b[5;1Htwo"), "one\ntwo");
        assert_eq!(strip_ansi("one\x08two"), "one\ntwo");
    }

    /// The credential lands first and the account block a moment later: the
    /// capture waits for the block, then gives up after the grace period.
    #[tokio::test(start_paused = true)]
    async fn capture_waits_for_the_account_block_but_not_forever() {
        // Only the `temp_dir()` read needs the lock, and a guard must not
        // live across the `advance` below.
        let dir = {
            let _env = env_guard();
            std::env::temp_dir().join(format!("cuw-capture-{}", std::process::id()))
        };
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let paths = ScratchPaths::new(&dir);
        let scan = OutputScan::default();
        let events: RefCell<Vec<ConnectEvent>> = RefCell::new(Vec::new());
        let emit = |e| events.borrow_mut().push(e);
        let mut run = Run::default();
        let mut grace = None;

        assert!(!capture(&paths, &scan, &mut run, &emit, &mut grace));
        assert!(run.credential.is_none());

        std::fs::write(&paths.credentials, CREDS_FAKE).unwrap();
        assert!(
            !capture(&paths, &scan, &mut run, &emit, &mut grace),
            "still waiting"
        );
        assert!(run.credential.is_some());
        assert!(grace.is_some());
        assert_eq!(
            events
                .borrow()
                .iter()
                .filter(|e| matches!(e, ConnectEvent::TokenCaptured))
                .count(),
            1
        );

        std::fs::write(
            &paths.config,
            r#"{"oauthAccount":{"accountUuid":"u-1","emailAddress":"a@b.c"},"numStartups":1}"#,
        )
        .unwrap();
        assert!(capture(&paths, &scan, &mut run, &emit, &mut grace));
        assert_eq!(run.account.as_ref().unwrap()["accountUuid"], "u-1");

        // Without the block, the grace period is the way out.
        let mut run = Run::default();
        let mut grace = None;
        std::fs::remove_file(&paths.config).unwrap();
        assert!(!capture(&paths, &scan, &mut run, &emit, &mut grace));
        tokio::time::advance(ACCOUNT_GRACE + Duration::from_millis(1)).await;
        assert!(capture(&paths, &scan, &mut run, &emit, &mut grace));
        assert!(run.account.is_none());
        // Only one capture event per credential, however often it is polled.
        assert_eq!(
            events
                .borrow()
                .iter()
                .filter(|e| matches!(e, ConnectEvent::TokenCaptured))
                .count(),
            2
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn id_is_slug_plus_short_uuid() {
        let id = make_id("My Work Account");
        assert!(id.starts_with("my-work-account-"));
        assert_eq!(id.len(), "my-work-account".len() + 1 + 8);
        let bare = make_id("!!!");
        assert_eq!(bare.len(), 8);
    }

    #[test]
    fn make_id_caps_long_labels() {
        let label = "ab ".repeat(170);
        assert!(label.len() >= 500);
        let id = make_id(&label);
        assert!(id.len() <= 41, "{} chars", id.len());
        assert!(!id.contains("--"));
        assert!(!id.starts_with('-'));
        // Truncation never leaves a dangling dash before the suffix.
        let (slug, _) = id.rsplit_once('-').expect("suffix");
        assert!(!slug.ends_with('-'));
    }
}
