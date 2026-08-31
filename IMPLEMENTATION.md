# Implementation Plan

Execution roadmap that takes the current scaffold to a finished app. The
*design* lives in [plan.md](plan.md); this file is the *how* and *in what
order*. Conventions and commands are in [CLAUDE.md](CLAUDE.md).

Milestone IDs (S1, M0–M6, M1b) match plan.md §7. Sub-tasks are numbered (e.g.
`M2.3`) so they can be referenced in commits and issues. Check boxes as you
land them.

---

## How to use this doc

- Work one milestone at a time, top to bottom. Each is independently useful and
  ends in a demonstrable state.
- **M3 is the ship point.** Everything through M3 is the product; M4–M6 are
  additive. Do not start M4 until M3 has been in daily use for a week (plan §7).
- A task is done when its box is checked *and* its milestone's Definition of
  Done holds *and* `cargo fmt`, `cargo clippy --all-targets`, `cargo test` are
  clean.
- Every unknown resolves to a display state, never a panic. If a task can't meet
  that, it isn't done.

---

## Current state snapshot

| Crate | Real | Missing / changing |
|---|---|---|
| `cuw-core` | client, defensive parser + golden test, poller, `redact`, `Credential`, `TokenRefresher`, `Usage.scoped` | — |
| `cuw-creds` | JSON credential blob, `Corrupt`/`TooLarge`, keyring-backed | macOS store untested live |
| `cuw-connect` | `auth login` flow: PTY driver, credential file read, scope gate, scrub, single-flight | live login never completed end to end (plan §8 Q7) |
| `cuw-daemon` | poll + refresh loop, routes incl. reconnect/shutdown, SSE, bearer, registry, route + paused-clock tests | live refresh unobserved (plan §8 Q8/Q12) |
| `cuw-tracker` | Windows hooks/pump/pick/re-acquire, geometry, live tests (`--ignored --test-threads=1`) | macOS impl (M5) |
| `apps/overlay` | full UI (rows, wire fields, settings panel, dock badge), docking glue, tray, bundle/NSIS | M4 manual matrix + multi-monitor sign-off; docking stays default-off until M3+M1b has a week of daily use |

`AccountState` has `Available`/`Unavailable`/`ReconnectNeeded`. `Detached` is an
overlay/tracker display state (a `dock-state` event), not a per-account state.

---

## Cross-cutting rules (apply to every milestone)

- **Redaction:** tokens never appear in logs, errors, HTTP responses, or SSE
  frames. Add a `redact()` helper in `cuw-core` early (M0) and route every
  token-adjacent log through it. A grep test in CI asserts no bearer string
  leaks (see Testing).
- **Defensive parsing:** never `serde` into required fields on the wire. Non-200
  / unexpected shape / parse failure → `Unavailable`, never a wrong number.
- **Rate discipline:** ≤ 1 request/account/minute, jittered, with hard backoff
  on 429/5xx. `poller.rs` already encodes this — use it, don't hand-roll delays.
- **Small changes:** one milestone ≈ one reviewable branch; keep clippy clean as
  you go, not at the end.

---

## S1 — Spike: does a `setup-token` token hit `/api/oauth/usage`? (blocking)

**Objective:** answer plan §4's blocking question before writing anything that
depends on it.

> **CORRECTION (2026-08-30): S1 is NOT confirmed — the `setup-token` path
> fails live.** The first end-to-end connect through the daemon captured a
> `setup-token` token (Claude Code 2.1.251) and the usage endpoint answered
> **403** `permission_error: "OAuth token does not meet scope requirement
> user:profile"` (`required_scopes: ["user:profile"]`). The sign-in URL
> `setup-token` opens requests `scope=user:inference` only. The 2026-08-29
> result below must have been produced with the interactive-login token. The
> plan §4 fallback (a per-account `CLAUDE_CONFIG_DIR` login, `claude auth
> login`, and Claude Code's own credential file) is back in play — see the
> open question at the end of this section.

**RESULT (2026-08-29, superseded above): fully confirmed — proceed on the primary `setup-token`
path, no fallback needed.** Both the interactive-login token *and* a real
`claude setup-token` token return **200** from the endpoint (checked live).
Payload captured as `crates/cuw-core/tests/fixtures/usage_ok.json`. Concretely:

- **Request:** `GET https://api.anthropic.com/api/oauth/usage`, header
  `Authorization: Bearer <token>` **only** — no `anthropic-beta`, no
  `anthropic-version` needed (bare bearer → 200). No auth at all → **429**
  (not 401), so never map 429 to an auth failure.
- **Status mapping for M0.1:** 200 → parse; 401 → `Unauthorized`
  (`ReconnectNeeded`); 429/5xx → backoff. Confirmed 200 and 429 live.
- **Payload:** primary windows are `five_hour` and `seven_day`, each
  `{ utilization: f64 (0–100), resets_at: RFC3339, limit_dollars, used_dollars,
  remaining_dollars, locked_reason }`. A structured `limits[]` array mirrors
  this: `kind:"session"` = 5h, `kind:"weekly_all"` = 7d, `kind:"weekly_scoped"`
  carries `scope.model.display_name`. Many nullable codename windows
  (`seven_day_opus`, `seven_day_sonnet`, `nimbus_quill`, …) and `extra_usage` /
  `spend` blocks for pay-as-you-go. See the fixture for the full shape.

**Resolved open questions (plan §8):**
- **Q2 (shape):** captured — see fixture. Note field is `utilization`, not
  `used_pct`; `resets_at` is RFC3339. `model.rs` maps cleanly.
- **Q3 (per-model weekly quotas?):** **yes** — `weekly_scoped` limits with
  `scope.model.display_name`, plus `seven_day_opus`/`seven_day_sonnet` fields.
  M0 parses the two primary windows; showing an active scoped window is an
  M3/M6 display choice, not a blocker.
- **Q4 (identity for row label?):** **no email/org in the payload.** → the
  connect modal **must** prompt the user for a label (M1.4 / M3.4). Settled.

**`setup-token` scopes — confirmed.** A token minted by `claude setup-token`
returns 200, so the plan §4 credential-store fallback is **not** needed; M1 uses
`setup-token` as the sole connect path. `setup-token` also reports the token is
**valid ~1 year** (settles plan §8 Q6) and prints it **once** to stdout (as the
value for `CLAUDE_CODE_OAUTH_TOKEN`, "won't be able to see it again") — so M1
must capture it from the PTY stream on the spot (M1.3), and M3.8 drives an
expiry countdown off the connect date (≈365 days).

- [x] **S1 reopened (2026-08-30) and decided.** Endpoint, headers and payload
  hold, but `setup-token` acceptance does **not**: its token lacks
  `user:profile` → 403. Everything in M1 that is not the token *source* (PTY
  driver, query replies, URL/code UI, scrub, single-flight, persistence) is
  done and works live; only the credential the flow ends with changes.

  **Decision: option 1** — `claude auth login` under a scratch
  `CLAUDE_CONFIG_DIR`, read the `.credentials.json` it writes, refresh via the
  OAuth token endpoint behind a `TokenRefresher` trait, using the CLI's public
  client id for `refresh_token` grants only (plan §4 records why the earlier
  preference was overridden). Option 2 (our own PKCE flow) was rejected: it
  would mint tokens from scratch under another client's id. Option 3 (wait)
  is not a plan. The work is **M1b** below.

---

## M0 — `cuw-core`: real fetch + defensive parse, driven by a hardcoded token

**Objective:** `cargo run` prints live 5h/7d percentages and reset times.
**Depends on:** S1 (need the real payload and header set).

- [x] **M0.1** Implement `OAuthUsageClient::fetch` (`client.rs`): GET `USAGE_URL`
  with just `Authorization: Bearer <token>` (S1: no extra headers needed). Map
  status → `FetchError`: 401→`Unauthorized`, 429→`RateLimited`, 5xx→`Server`,
  transport → `Transport`. Set a short timeout; never `unwrap` the response.
- [x] **M0.2** Add `redact(token) -> String` to `cuw-core` (e.g. first 4 chars +
  `…`). Use it anywhere a token could be logged.
- [x] **M0.3** Implement `parse_usage` (`parse.rs`) against the golden file:
  read `five_hour.utilization` / `seven_day.utilization` (field is
  `utilization`, an f64 percent) and `.resets_at` (RFC3339) out of
  `serde_json::Value` field by field with `.get(...).and_then(...)`; any miss →
  `None`. Clamp percent to `0..=100`. A bad/missing timestamp → `resets_at:
  None`, not a failed parse. (The `limits[]` array — `kind:"session"` /
  `"weekly_all"` — is an equivalent, arguably more semantic source; pick one and
  cover it with the golden test.)
- [x] **M0.4** Per-model weekly windows exist but are nullable/optional (S1 Q3:
  `weekly_scoped`, `seven_day_opus`, …). Keep `Usage` as the two primary windows
  for M0; leave scoped-model display to M3/M6. No `model.rs` change needed now.
- [x] **M0.5** Add `examples/print_usage.rs` (or a `--once` path) that reads a
  token from `$CUW_TOKEN`, fetches, parses, and prints percentages + resets or
  the display state. This is the M0 demo.

**New deps:** none beyond workspace (`reqwest`, `serde_json`, `time`).

**Tests:**
- [x] Golden-file test: `parse_usage(usage_ok.json)` → expected `Usage`.
- [x] Malformed-input tests: empty object, wrong types, missing fields, extra
  fields → all `None` / defaults, never panic.
- [x] `next_interval()` stays in `[60, 120]s`; `backoff` is monotonic and capped
  at 15 min.

**DoD:** `CUW_TOKEN=… cargo run --example print_usage` prints correct live
numbers; feeding it garbage prints `unavailable`; no token appears in output.

**Risks:** payload shape differs from community reports → the golden file is the
source of truth, not any blog. Shape may change over time → the golden test
fails loudly in CI (plan §8 Q2), which is the intended early-warning.

---

## M1 — `cuw-creds` + `cuw-connect`: connect flow end to end, headless

**Objective:** connect two accounts via `claude setup-token`, tokens land in the
OS store, both fetch successfully.
**Depends on:** M0 (`fetch` for validation).

> **Retired as a token source (2026-08-30).** The `setup-token` path is kept
> here as history: its PTY driver is the one M1b reuses verbatim. The
> ConPTY facts below still hold and are not obvious — killing the child does
> **not** EOF the PTY reader, only dropping the master does; keep the master
> in the async scope, never in the read thread; the read thread is never
> joined; `child.wait()` runs in `spawn_blocking` inside the `select`; the CLI
> blocks until `ESC[6n` is answered with a cursor-position report.

- [x] **M1.1** Confirm `KeyringStore` round-trips on the target OS
  (put/get/delete). Add a `#[cfg]`-gated ignored integration test that writes and
  deletes a throwaway entry.
- [x] **M1.2** Implement `connect()` (`cuw-connect/lib.rs`): spawn `claude
  setup-token` via `portable-pty` under a scratch `CLAUDE_CONFIG_DIR`
  (tempdir). Stream PTY output through `emit(ConnectEvent::Output(...))` so a
  code-paste step is visible.
- [x] **M1.3** Capture the emitted token from the PTY stream: `setup-token`
  prints it **once** (the value for `CLAUDE_CODE_OAUTH_TOKEN`) and cannot show it
  again, so grab it on the spot. Redact it in every `Output` event — never echo
  the raw token to the UI.
- [x] **M1.4** Validate once via `source.fetch`. The usage payload carries **no**
  email/org (S1 Q4), so the label is always user-supplied: generate a stable
  `id` (uuid/slug) and take the `label` from the connect modal. Emit
  `Validated { id, label }`. (`connect()` returns `Connected { id, label, token }`;
  the token travels only there, never in an event.)
- [x] **M1.5** Persist: caller stores the token under `id` in `KeyringStore` and
  appends `{id, label}` to the account registry. **Decision:** have the daemon
  own a registry file it writes (e.g. `registry.toml` in its data dir), separate
  from the hand-authored `accounts.toml`, so writes never clobber user comments.
  Record the choice here.
- [x] **M1.6** Always delete the scratch config dir (success or failure). Map
  spawn/exit failures to `ConnectError`; a missing `claude` binary gets a clear
  message.
- [x] **M1.7** Guard: connecting an already-registered id must warn (re-running
  `setup-token` invalidates the prior token, plan §4). Surface as a caller-side
  precondition + an `emit` warning. (Documented on `connect()` as a caller
  precondition; registry tracking is M2.)

**New deps:** `portable-pty`, `tempfile`, `tokio` (process/io) in `cuw-connect`.

**Tests:**
- [x] Token-capture parser: unit-test the PTY-line → token extraction against
  recorded sample output (no live `claude` needed).
- [x] Redaction: assert no `ConnectEvent::Output` frame contains the full token.

**DoD:** a headless test binary runs the flow twice for two accounts; `get`
returns both tokens; both validate via `fetch`. Scratch dirs are gone.

**Risks:** `setup-token` output format could change → keep the extraction
tolerant and behind one function with a test. PTY behaviour differs Win/macOS →
test on both before M2 leans on it.

---

## M1b — Token source: `claude auth login` + credential file + refresh

**Objective:** the connect flow ends with a credential the usage endpoint
accepts, and the daemon keeps it alive without user action.
**Depends on:** M1 (PTY driver), M2 (poll loop, routes). **Blocks shipping.**

Rules specific to this milestone, on top of the cross-cutting ones:

- Never log a token-endpoint error body — the request carries the refresh
  token and a body could echo it. Status code plus a whitelisted OAuth
  `error` code only.
- A *rejected* refresh (`invalid_grant` / `invalid_token` /
  `unauthorized_client` on 400/401/403) is terminal (`reconnect needed`) and
  ends the poll task; never retry it. Any other 4xx is
  `RefreshError::Contract` and backs off like a 5xx with a loud log.
  429/5xx/transport/bad-shape/contract back off with `poller::backoff`
  floored at 60 s, on a refresh-only attempt counter.
- At most one refresh per account per poll cycle; a 401 does not trigger an
  immediate refresh+refetch inside the same minute. `reconnect needed` rows
  have no running task: nothing polls or refreshes them until a reconnect.
- Every branch that keeps last-good numbers without a fresh 200 sets
  `stale = true`, and no sleep may carry numbers past `STALE_AFTER`:
  downgrade to `unavailable` before sleeping if the sleep would cross it.
- Never run `claude auth logout` for cleanup — it may revoke the token
  server-side. Deleting the scratch dir is the cleanup, and the scratch dir
  lives under the daemon's data dir, is scrubbed on every exit path, and is
  swept at startup (plan §4).
- Log `CredError` and `RefreshError` with `%e`, never `?e`.
- Exactly one live `claude auth login` and one live refresh are allowed while
  building this; never probe the usage endpoint with a bogus token. Never
  `curl -v`/`--trace`/`-i` with the bearer; PowerShell helpers build the
  header inside a function and `Remove-Variable` it on exit.

### M1b.1 `cuw-core::Credential` (new `crates/cuw-core/src/credential.rs`)

- [x] Type, serde, redacted `Debug`, `rotated()`.

```rust
#[derive(Clone, Serialize, Deserialize)]
pub struct Credential {
    #[serde(default = "one")] pub v: u8,        // blob version, currently 1
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,                        // Unix seconds, UTC
    #[serde(default)] pub scopes: Vec<String>,
}
impl Credential {
    pub const REQUIRED_SCOPE: &'static str = "user:profile";
    pub fn has_usage_scope(&self) -> bool;
    pub fn expires_at_utc(&self) -> Option<OffsetDateTime>;   // from_unix_timestamp(..).ok()
    /// Apply a token response. A missing refresh_token/scopes keeps the old one.
    pub fn rotated(&self, r: &Refreshed, now: OffsetDateTime) -> Credential;
}
impl fmt::Debug for Credential { /* access=redact(..) refresh=redact(..) expires_at=.. scopes=[..] */ }
```

`cuw-core/Cargo.toml` gains `serde.workspace = true`. Export from `lib.rs`.

### M1b.2 `cuw-core::refresh` (new `crates/cuw-core/src/refresh.rs`) and scoped windows

- [x] `TokenRefresher` trait, `OAuthTokenClient`, `parse_refresh`.

```rust
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e"; // the CLI's own public id (plan §4)

#[derive(Debug, thiserror::Error)]
pub enum RefreshError {
    #[error("refresh rejected ({0})")] Rejected(u16),   // 4xx + whitelisted error code → reconnect needed
    #[error("token endpoint contract changed ({0})")] Contract(u16), // other 4xx → backoff + loud log
    #[error("rate limited")] RateLimited,               // 429
    #[error("server error: {0}")] Server(u16),          // 5xx
    #[error("transport: {0}")] Transport(String),       // reqwest message only
    #[error("unexpected token response shape")] BadShape, // 200 but unparseable
}
/// The only 4xx error codes that mean "this refresh token is dead".
pub const REJECT_CODES: [&str; 3] = ["invalid_grant", "invalid_token", "unauthorized_client"];
#[derive(Clone)]
pub struct Refreshed {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: Duration,          // clamped 60 s ..= 30 d
    pub scopes: Option<Vec<String>>,
}
impl fmt::Debug for Refreshed { /* redacted */ }

#[async_trait]
pub trait TokenRefresher: Send + Sync {
    async fn refresh(&self, refresh_token: &str) -> Result<Refreshed, RefreshError>;
}
pub struct OAuthTokenClient { http: reqwest::Client }   // Default like OAuthUsageClient, 15 s timeout
pub fn parse_refresh(raw: &serde_json::Value) -> Option<Refreshed>;
```

Request: `POST TOKEN_URL`, JSON body
`{"grant_type":"refresh_token","refresh_token":<rt>,"client_id":CLIENT_ID}`,
no `Authorization` header (encoding inferred — plan §8 Q8; if the live
refresh answers `Contract`, switch to `.form(..)` and record it). Status
map: 200 → bytes → `Value` → `parse_refresh(..).ok_or(BadShape)`;
400|401|403 → read the body bytes into memory, `pub fn error_code(bytes:
&[u8]) -> Option<String>` = `serde_json::from_slice::<Value>` →
`get("error").as_str()` → keep only if it is in `REJECT_CODES`, drop the
bytes → `Some(_)` → `Rejected(code)`, `None` → `Contract(code)`; 429 →
`RateLimited`; 500..=599 → `Server(code)`; other → `Transport("unexpected
status N")`. The body is never formatted, logged or stored anywhere else;
the extracted string is bounded to the whitelist.

`parse_refresh`: `access_token` non-empty string (else `None`); `expires_in`
as `as_f64` (u64 or f64), missing → `None` (a token with unknown lifetime
cannot be scheduled), clamped to `60..=2_592_000` s and converted with
`Duration::try_from_secs_f64(..).ok()?` (never the panicking `from_secs_f64`
on a remote-payload path); `refresh_token` optional non-empty string; scopes
from `scope` (space-separated string) or `scopes` (array of strings) else
`None`; unknown fields ignored.

- [x] Scoped windows: `model.rs` gains
  `pub struct ScopedWindow { pub name: String, pub used_pct: f32, pub resets_at: Option<OffsetDateTime>, pub is_active: bool }`
  and `Usage` gains `pub scoped: Vec<ScopedWindow>`. `parse.rs` reads
  `limits[]` entries where `kind == "weekly_scoped"`; name from
  `scope.model.display_name` (string), pct from `percent` (`as_f64`, clamp
  0..=100), `resets_at` via the existing RFC3339 path, `is_active` via
  `as_bool().unwrap_or(false)`. Any miss skips that entry; a missing
  `limits` array is an empty vec, never a failed parse. The fixture yields
  one entry: `Fable`, 0 %, no reset, `is_active: false` (in the fixture
  `is_active` marks the currently binding limit, not existence — both
  `weekly_all` and `weekly_scoped` are `false` there).

**Tests** (`crates/cuw-core/tests/refresh.rs`, fixture
`tests/fixtures/token_ok.json` with obviously fake values, e.g.
`sk-ant-oat01-FAKE…0001`, `sk-ant-ort01-FAKE…0002`, `expires_in: 28800`,
`scope: "user:inference user:profile"`): golden parses (28800 s, two scopes);
empty object → `None`; missing `access_token` → `None`; `expires_in` as string
→ `None`; missing `refresh_token` → `Some` with `None`; `scopes` array form;
extra fields ignored; out-of-range `expires_in` clamped, negative → `None`,
huge → clamped; `Refreshed`/`Credential` `Debug` never contains the tokens;
`rotated` keeps the old refresh token when the response has none and sets
`expires_at = now + expires_in`; `error_code` maps `{"error":"invalid_grant"}`
→ `Some`, `{"error":"invalid_request"}` / `{}` / non-JSON → `None`, and a
body that echoes a `FAKE` refresh token never reaches the error's
`Display`/`Debug`. `tests/parse.rs` gains `golden_scoped_window_parses` and
`limits_missing_is_empty_scoped`.

### M1b.3 `cuw-creds`: JSON blob, `Corrupt` / `TooLarge`

- [x] Trait now moves `Credential`s:

```rust
pub trait CredentialStore: Send + Sync {
    fn put(&self, id: &str, cred: &Credential) -> Result<(), CredError>;
    fn get(&self, id: &str) -> Result<Credential, CredError>;
    fn delete(&self, id: &str) -> Result<(), CredError>;
}
pub enum CredError {
    NotFound(String),
    Corrupt(String),      // JSON parse failed → reconnect needed
    TooLarge(String),     // > 2560 UTF-16 bytes, checked before the backend call
    Backend(#[from] keyring::Error),
}
```

`put` serializes with `serde_json::to_string`, rejects when
`s.encode_utf16().count() * 2 > 2560`, then `set_password`. `get` →
`get_password` → a free `fn decode(id, Result<String, keyring::Error>) ->
Result<Credential, CredError>`: `NoEntry` → `NotFound`; **`BadEncoding(_)` →
`Corrupt`** (keyring 3.6.3 `windows.rs:419,426` puts the raw blob bytes in
that variant, and `CredError` derives `Debug`); other backend errors →
`Backend`; `Ok(s)` → `from_str::<Credential>` → `Err(_) => Corrupt(id)`, then
`cred.v != 1 => Corrupt(id)` (the version field exists to be checked).
`CredError` never carries the blob. `cuw-creds/Cargo.toml` adds `cuw-core`,
`serde_json`.

**Tests:** the ignored keyring round-trip uses a fake `Credential`; a new
non-ignored test asserts two 120-char fake tokens plus two scopes serialize to
fewer than 1280 chars; `too_large_is_rejected_before_backend`;
`bad_encoding_is_corrupt_and_debug_has_no_blob` (feed `decode` an
`Err(keyring::Error::BadEncoding(b"sk-ant-FAKE".to_vec()))`, assert `Corrupt`
and `format!("{:?}")` lacks `sk-ant`); `unknown_version_is_corrupt`; a
`Corrupt` seed test is done in the daemon's route tests (`MemStore` seeded
with a `Corrupt` result → row `reconnect needed`, `refresh: "rejected"`).

### M1b.4 `cuw-connect`: credential file instead of printed token

- [x] Command: `claude auth login --claudeai` (verified present in 2.1.251;
  `--claudeai` is the default, passed explicitly). Same PTY driver, same
  query replies, same input forwarder, same 300 s cap. Built by `fn
  build_command(config_dir: &Path) -> CommandBuilder`, which `env_remove`s
  `CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`,
  `CLAUDE_CODE_USE_BEDROCK`, `CLAUDE_CODE_USE_VERTEX` (portable-pty 0.9.0
  `cmdbuilder.rs:75` seeds the child env from `std::env::vars_os()`, and a
  pre-set token makes the CLI skip the login or bind the wrong identity).
- [x] Scratch dir: `ConnectRequest.scratch_root: PathBuf` (the daemon passes
  `data_dir/scratch`); the flow creates `scratch_root/cuw-connect-<uuid>` and
  wraps it in a `ScratchGuard` whose `scrub()` (also run from `Drop`) does:
  overwrite `.credentials.json` with `{}` if present (best effort), then
  `remove_dir_all` with 5 × 200 ms retries, and on final failure
  `tracing::warn!(path = %p, "scratch dir not removed")` — path only. No
  `tempfile`, no `%TEMP%`. After `killer.kill()` the flow awaits the
  `child_exit` handle with a 2 s `tokio::time::timeout` **before** scrubbing,
  so the dying CLI no longer holds handles in the dir (the M1 driver's
  `TempDir::drop` failed silently for exactly that reason and left four
  scratch dirs in `%TEMP%`). `main.rs` sweeps every entry under
  `data_dir/scratch` before seeding, logging only the count.
- [x] Inside the existing `select!`: a 250 ms `tokio::time::interval`
  (`MissedTickBehavior::Delay`) calls `read_credentials(&config_dir.join(".credentials.json"))`;
  the first `Some` sets `found`, emits `TokenCaptured` (wire phase stays
  `token_captured`) and breaks. On child exit: drain output 800 ms as today,
  then up to 4 × 250 ms `read_credentials` retries (a last write may still be
  landing), then break. Read errors are never logged with their text (they can
  quote the file); `trace!("credentials file not ready")` only.
- [x] `pub(crate) fn parse_credentials_file(v: &Value) -> Option<Credential>`:
  `claudeAiOauth.{accessToken, refreshToken}` non-empty strings, `expiresAt`
  as `as_f64` → if `> 1e11` treat as milliseconds (`/ 1000`) else seconds,
  `scopes` array of strings (missing → empty). Any miss → `None`.
- [x] After the loop: teardown as today; `TimedOut` if nothing found and the
  deadline hit; else `found.ok_or(NoCredential)`; then **scope gate before any
  network call**: `!cred.has_usage_scope()` → `Failed(..)` +
  `Err(Forbidden(scopes.join(" ")))` (scopes are not secret; the message lists
  them); then validate once with `source.fetch(&cred.access_token)` using the
  existing 401 → `Invalid` / 403 → `Forbidden` / other → deferred mapping.
- [x] API: `pub struct ConnectRequest { pub label: String, pub existing_id:
  Option<String>, pub scratch_root: PathBuf }`, `connect(source, req, emit,
  code_rx)`; `make_id` only when `existing_id` is `None`, and its slug is
  truncated to 32 chars (the Credential Manager username cap is 512 bytes;
  keyring 3.6.3 `windows.rs:183` fails `set_password` beyond it — a long
  label must never fail *after* the browser login). `Connected { id, label,
  credential: Credential }`. `ConnectError::NoToken` → `NoCredential("claude
  auth login finished without writing a credential")`; `Forbidden(String)`
  now carries the scope list.
- [x] Remove `extract_token`, `OutputScan::{captured, token_in, finalize}`,
  the capture block in `feed`, and `scrub`'s `captured` argument. Keep
  `TOKEN_MARKER` and both scrub passes: redaction stays fail-closed if the CLI
  ever prints a token. Keep `AwaitingCode` on `"paste code"`; confirm on the
  one live run whether `auth login` prints it (plan §8 Q7).

**Tests** (fixture as a `const` in the test module, all values fake):
parses the CLI file (`expires_at == 1756600000` from `expiresAt:
1756600000000`, two scopes); missing `claudeAiOauth` → `None`; `accessToken`
not a string → `None`; empty `refreshToken` → `None`; seconds epoch is not
divided; missing `scopes` → empty vec; fixture cut at 60 % (non-atomic write)
→ `None`; `has_usage_scope` false for `["user:inference"]` and the `Forbidden`
message contains it; every `Output` frame from a transcript containing an
`sk-ant-` run is scrubbed; `build_command` leaves `get_env` `None` for the
five scrubbed keys; `ScratchGuard` empties a dir holding a fake
`.credentials.json` on drop; `make_id` of a 500-char label is ≤ 41 chars.
Delete the five printed-token tests.

### M1b.5 `cuw-daemon` poll loop: refresh phase

- [x] `poll.rs` additions (pure, tested):

```rust
pub const REFRESH_LEAD: Duration = Duration::minutes(5);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RefreshStatus { #[default] Ok, Backoff, Rejected }
pub const REFRESH_BACKOFF_FLOOR: std::time::Duration = std::time::Duration::from_secs(60);
pub struct PollState {
    pub attempt: u32,           // usage-endpoint backoff counter (429/5xx/transport)
    pub refresh_attempt: u32,   // token-endpoint backoff counter; separate so neither wipes the other
    pub last_success: Option<OffsetDateTime>,
    pub stale: bool,            // last-good numbers held without a fresh 200
    pub force_refresh: bool,    // set by a 401 on a token that was not just refreshed
    pub just_refreshed: bool,   // between a refresh and the next applied fetch: a 401 now is final
    pub refresh_status: RefreshStatus,
    pub refreshed_at: Option<OffsetDateTime>,
    pub persist_pending: bool,  // keyring write failed; retry next cycle
    pub persist_logged: bool,   // error logged once per failure streak
    pub forced_for_429: bool,   // one forced refresh per 429 outage (plan §8 Q12)
}
pub fn needs_refresh(cred: &Credential, poll: &PollState, now: OffsetDateTime) -> bool;
  // poll.force_refresh || cred.expires_at_utc().map_or(true, |t| t - now < REFRESH_LEAD)
pub fn refresh_backoff(attempt: u32) -> std::time::Duration;  // max(poller::backoff(attempt), REFRESH_BACKOFF_FLOOR)
pub enum RefreshStep { Rotated(Credential), Reconnect, Backoff }
pub fn apply_refresh(result: Result<Refreshed, RefreshError>, cred: &Credential,
                     poll: &mut PollState, now: OffsetDateTime) -> RefreshStep;
  // Ok → Rotated(cred.rotated(..)); force_refresh=false; just_refreshed=true; refresh_attempt=0; status=Ok; refreshed_at=now  (attempt untouched)
  // Rejected(_) → status=Rejected → Reconnect
  // Contract|RateLimited|Server|Transport|BadShape → refresh_attempt+=1; status=Backoff; stale=true → Backoff
  //   (BadShape and Contract also tracing::error!(status, "token endpoint contract changed"))
/// True when sleeping `sleep` from `now` would carry last-good numbers past STALE_AFTER.
pub fn sleep_crosses_stale(poll: &PollState, now: OffsetDateTime, sleep: std::time::Duration) -> bool;
```

`apply_fetch` changes: `Unauthorized` → if `poll.just_refreshed` then
`(ReconnectNeeded, Idle)` else `{ poll.force_refresh = true; poll.stale =
true; keep current unless stale per STALE_AFTER (do not bump attempt);
Step::Normal }` — the normal 60–120 s sleep keeps the one-request-per-minute
rule. `Forbidden` → `(ReconnectNeeded, Idle)` unchanged. Success sets
`poll.stale = false; poll.forced_for_429 = false`. The transient branch sets
`poll.stale = true` when it keeps numbers (`false` once downgraded to
`Unavailable`), and on `RateLimited` with `poll.attempt >= 2` and
`!poll.forced_for_429` and no refresh since `last_success` it sets
`force_refresh = true; forced_for_429 = true` (one bounded token POST per
outage — plan §8 Q12). Every branch clears `just_refreshed` at the end.
`fingerprint(state, refresh, stale, persist_pending)` returns `(u8, i64,
i64, u8, bool, bool, u64 /*scoped hash*/)`. **The loop compares the
fingerprint of what the `Row` currently holds (its stored `state`,
`refresh`, `stale`, `persist_pending`) against the post-apply values** — both
sides from the same post-apply `poll` would hide a stale-only or
refresh-only flip and the SSE-driven overlay would keep rendering aged
numbers as fresh.

- [x] `http.rs` `poll_loop(app, id, mut cred: Credential, start_delay:
  Duration)`: sleep `start_delay` first. Each iteration: `if needs_refresh {
  refresh → apply_refresh → Rotated: cred = next; persist (store.put; on Err
  log `error = %e` once per streak, set persist_pending) | Reconnect:
  sync_row(ReconnectNeeded), broadcast, **return** | Backoff: `sleep =
  refresh_backoff(refresh_attempt)`; if `sleep_crosses_stale` write
  `Unavailable` else keep with `stale = true`; sync, broadcast if the
  fingerprint changed, sleep, continue }`; if `persist_pending` retry the
  put; then the existing fetch/apply; `Step::Idle` → sync, broadcast,
  **return** (no 15-minute idle loop: a dead token is never fetched again);
  `Step::Backoff` also applies `sleep_crosses_stale`. `fn sync_row(row: &mut
  Row, cred: &Credential, poll: &PollState)` copies `access_expires_at`,
  `refreshed_at`, `refresh`, `stale`, `last_ok_at`, `persist_pending` in one
  place. Single-flight by construction: one task per id; a finished
  `JoinHandle` left in `app.tasks` is harmless (reconnect and spawn abort
  and replace it).
- [x] `AppState.refresher: Arc<dyn TokenRefresher>`; `main.rs` passes
  `Arc::new(OAuthTokenClient::default())`; `seed_from_registry` handles
  `Ok(cred)` → spawn with `start_delay = i × 2 s + rand(0..=3 s)` (no N
  simultaneous token POSTs after a restart), `Err(_)` → `ReconnectNeeded`
  with `row.refresh = Rejected` (a `reconnect needed` row must carry a
  reason). `add_account`/`reconnect` spawn with zero delay. The first
  iteration refreshes an expired stored token before the first fetch.

**Tests** (`poll.rs`): `first_401_forces_refresh_and_sleeps_normal`,
`401_keeps_numbers_but_marks_stale`, `401_after_refresh_is_reconnect`,
`refresh_rejected_is_reconnect`, `refresh_contract_is_backoff_not_reconnect`,
`refresh_transient_is_backoff_and_bumps_refresh_attempt_only`,
`refresh_backoff_keeps_numbers_but_marks_stale`, `refresh_backoff_floor_is_60s`,
`refresh_ok_does_not_reset_usage_attempt`,
`rotation_keeps_refresh_token_when_absent`, `needs_refresh_within_lead`,
`second_429_forces_one_refresh_per_outage`,
`sleep_crosses_stale_at_boundary`, `stale_flag_set_and_cleared`,
`fingerprint_changes_on_refresh_status_stale_persist_and_scoped`. Loop-level
(`tests/routes.rs`, driving `spawn_poll_task` with fakes and
`tokio::time::pause`): `rejected_refresh_ends_the_task` (a counting
`FakeRefresher` returning `Rejected` once: refresh count stays 1 and the
usage-source count stays flat over a simulated 30 min, row is `reconnect
needed`/`rejected`), `stale_flip_pushes_a_frame` (source 200 then 429 with a
fresh `last_success`: an `accounts` broadcast is observed on the flip),
`seeded_tasks_are_staggered` (five seeded fakes: no two first refreshes
within 1 s).

### M1b.6 Wire shape

- [x] `Row` gains `access_expires_at: Option<OffsetDateTime>`,
  `refreshed_at: Option<OffsetDateTime>`, `refresh: RefreshStatus`,
  `stale: bool`, `last_ok_at: Option<OffsetDateTime>`, `persist_pending:
  bool`, plus `Row::new(label, state, connected_at)` defaulting them. Delete
  `TOKEN_TTL` and `expires_at` (the overlay's `expiryHint` treats a missing
  field as "no hint", so nothing breaks before I1 lands).
- [x] `WireAccount` — every field below the line is always present:

```json
{"id":"work-abc12345","label":"Work","state":"available",
 "five_hour":31,"seven_day":14,
 "resets_at":"2026-08-30T12:00:00Z","seven_day_resets_at":"2026-09-04T15:00:00Z",
 "stale":false,"fetched_at":"2026-08-30T10:12:03Z",
 "scoped":[{"name":"Fable","pct":12,"resets_at":null,"is_active":false}],
 "access_expires_at":"2026-08-30T18:10:03Z","refreshed_at":"2026-08-30T10:10:03Z","refresh":"ok",
 "persist_pending":false}
{"id":"home-def67890","label":"Home","state":"reconnect needed",
 "stale":false,"fetched_at":null,"scoped":[],
 "access_expires_at":null,"refreshed_at":null,"refresh":"rejected",
 "persist_pending":false}
```

`persist_pending: true` means the rotated credential could not be written to
the OS store; the row keeps working from memory but will need a reconnect
after a daemon restart (I1 shows "not saved — reconnect after restart").

`five_hour`/`seven_day`/`resets_at`/`seven_day_resets_at` keep the
present-only-when-available rule (`Option<Option<String>>` skip pattern).
`stale` is `true` only with `state: "available"`. SSE event names are
unchanged: `accounts` and `connect`.

### M1b.7 Routes

- [x] `POST /accounts/:id/reconnect` (no body): 404 if unknown id; 409 if the
  connect slot is busy; abort and remove the id's poll task **first**; run
  `connect(.., ConnectRequest { label: row.label, existing_id: Some(id),
  scratch_root }, ..)` as a tracked task (below); success → `store.put`
  (overwrite), `persist_account` with a fresh `connected_at`, row →
  `Row::new(label, Unavailable, now)`, `spawn_poll_task`, broadcast,
  `200 {"id","label"}`. Failure → if `store.get(&id)` still yields a
  credential and the row was not already `ReconnectNeeded`, respawn the poll
  task with it and leave the state to the loop (a cancelled reconnect on a
  healthy row must not kill it); otherwise row → `ReconnectNeeded` with
  `refresh = Rejected`; broadcast; `502 <message>`.
- [x] Connect flows run as a **tracked task**: `AppState.connect_task:
  Arc<Mutex<Option<JoinHandle<..>>>>` (or a `CancellationToken`) is set for
  the duration of `add_account`/`reconnect_account`, so shutdown can abort it
  and the flow's teardown (`killer.kill()`, master drop, scratch scrub) runs
  instead of the graceful-shutdown waiting up to 300 s for the handler.
- [x] `POST /shutdown` (bearer-gated, 204): signals `AppState.shutdown:
  Arc<tokio::sync::Notify>`; `main.rs` selects `ctrl_c | notify.notified()`
  in `with_graceful_shutdown`, then aborts the connect task and every poll
  task, and wraps the remaining `serve` future in `tokio::time::timeout(3
  s)` → `std::process::exit(0)` on expiry. `main.rs` writes `data_dir/pid`
  unconditionally at startup so the overlay can kill a daemon it did not
  spawn.
- [x] `POST /accounts` rejects labels longer than 64 chars with 400 before
  anything spawns; stores `connected.credential`; `DELETE` unchanged.

**Tests** (`tests/routes.rs`): `MemStore` holds `Credential`s; `FakeRefresher`
returns fake values; the shape test asserts the new always-present fields
(`refresh` is one of the three strings, `stale` and `persist_pending` are
bools, `scoped` is an array) and keeps `!text.contains("sk-ant")`;
`corrupt_blob_is_reconnect_needed_with_refresh_rejected` (seed a `Corrupt`
result); `reconnect_unknown_id_is_404`;
`reconnect_while_connect_in_flight_is_409`;
`failed_reconnect_on_healthy_row_keeps_polling` (a running task remains in
`tasks`); `long_label_is_400`; `shutdown_requires_bearer_and_notifies`.

**DoD (M1b):** two accounts connected from the widget, both `available`; a
forced 401 (fake source) refreshes and recovers without a reconnect; a
rejected refresh shows `reconnect needed` + `refresh: "rejected"`; `grep -r
sk-ant` over `daemon.log`, `/accounts` and an `/events` capture finds nothing;
a daemon restart after the access token expired refreshes before its first
fetch; exactly one live login and one live refresh were run to confirm plan
§8 Q7/Q8, and their findings are written back here.

**Live findings (2026-08-31, `scripts/e2e-live.ps1 -SkipLive`):** preflight,
startup (port + pid file + clean scratch), auth-wire (401 without bearer, 200
with, no `expires_at` on the wire), SSE first frame, redaction and graceful
shutdown all PASS against the A6 daemon. The live connect was **not** run
(needs an interactive browser sign-in), so plan §8 Q7 (paste-code prompt), Q8
(token-endpoint encoding/rotation) and Q12 (429 on a dead token) remain
unobserved; run the script without `-SkipLive` once to close them.

---

## M2 — `cuw-daemon`: poll loop + localhost HTTP/SSE, backoff, 401 handling

**Objective:** `curl localhost:PORT/accounts` returns both accounts; survives
token expiry gracefully.
**Depends on:** M0, M1.

- [x] **M2.1** Shared state: `Arc<RwLock<HashMap<AccountId, AccountState>>>` (or
  a small `State` struct) written by the poll loop, read by HTTP. Define the
  serde wire shape once (matches what `apps/overlay/src/main.js` already expects:
  `{ id, label, state, five_hour?, seven_day?, resets_at? }`).
- [x] **M2.2** Poll loop: one task per account. Loop = `fetch` → `parse` →
  update state → sleep `next_interval()`. On `RateLimited`/`Server`, sleep
  `backoff(attempt)` and increment attempt; reset attempt on success. On
  `Unauthorized`, set `ReconnectNeeded` and stop hammering (long sleep / wait for
  a reconnect signal). On parse `None`, set `Unavailable` but keep polling.
- [x] **M2.3** HTTP server (**axum** — tokio-native, first-class SSE; note the
  choice) bound to `127.0.0.1:cfg.port` only:
  - `GET /accounts` → current states.
  - `GET /events` → SSE; push a frame on every state change and on connect-flow
    progress.
  - `POST /accounts` → start the M1 connect flow; stream `ConnectEvent`s over
    `/events` (or the POST response stream).
  - `DELETE /accounts/:id` → remove from registry + `KeyringStore`.
- [x] **M2.4** Auth: generate a random bearer token at first run, write it to a
  `0600` file in the data dir; require it on every route (constant-time compare).
  Localhost bind + bearer (plan §5).
- [x] **M2.5** Wire `main.rs`: load config, build `KeyringStore`, spawn poll
  tasks for each registered account, serve the router. Structured `tracing`
  throughout, all token-adjacent logs via `redact`.
- [x] **M2.6** Graceful behaviour: a newly connected account starts polling
  without a restart; a deleted one stops. Clean shutdown on Ctrl-C.

**New deps:** `axum`, `tower`/`tower-http` (as needed), `tokio` full features,
`cuw-connect`, plus `rand` for the bearer token.

**Tests:**
- [x] Route tests against an in-memory `UsageSource` fake: `/accounts` shape;
  401 → `ReconnectNeeded`; 429 → state stays last-good-or-unavailable, never a
  wrong number; missing/invalid bearer → 401.
- [x] Redaction grep test over captured log output.

**DoD:** with two connected accounts, `curl -H "Authorization: Bearer …"
localhost:8787/accounts` returns both with live states; expiring a token flips
that row to `reconnect needed` without affecting the other.

**Risks:** SSE + connect-flow streaming is the fiddly part → get `/accounts`
polling solid first, layer `/events` on after.

---

## M3 — Overlay v1: always-on-top, **undocked**, connect/disconnect UI  ← SHIP

**Objective:** usable daily on both platforms. This is the real ship point.
**Depends on:** M2.

- [x] **M3.1** Window style: frameless, transparent, `always_on_top`, resizable,
  small default size, skip-taskbar. Configure in `tauri.conf.json` +
  `apps/overlay/src-tauri/src/lib.rs`. (Focus-stealing/click-through flags are
  M4/M6 — keep M3 a normal always-on-top window.)
- [x] **M3.2** Bearer plumbing: a Tauri command reads the daemon's `0600` bearer
  file and hands it to the web layer; `main.js` sends `Authorization` on every
  `fetch`. (This is the localhost auth token, not a Claude token — safe to hold
  in the webview.)
- [x] **M3.3** Replace the 5s poll in `main.js` with an SSE subscription to
  `/events`, falling back to polling if the stream drops. Render all
  `AccountState` variants (available / unavailable / reconnect needed) — the row
  markup already branches on `state`.
- [x] **M3.4** Connect flow UI: wire the `#connect` button to `POST /accounts`,
  open a small modal, stream `ConnectEvent`s ("Complete sign-in in your
  browser", live PTY output, success/fail). On `ReconnectNeeded`, show a
  one-click re-run.
- [x] **M3.5** Disconnect UI: per-row action → `DELETE /accounts/:id` with a
  confirm.
- [x] **M3.6** Daemon lifecycle: overlay launches the daemon if it isn't already
  up. **Decision:** Tauri sidecar (bundle `cuw-daemon` as a sidecar binary) vs a
  detached spawn — pick one, record it. Show "daemon offline" honestly when it's
  down (already stubbed in `main.js`).
- [x] **M3.7** Persist window position/size across restarts
  (`tauri-plugin-window-state` or manual).
- [x] **M3.8** Refresh-status surfacing (plan §8 Q6, revised by M1b): there is
  no one-year cliff any more. Show `refresh: backoff` as "refreshing…" and
  `refresh: rejected` as the reason next to `reconnect needed`; drop the
  365-day countdown (the wire no longer carries `expires_at`). Landed as
  integration item I1 after M1b.

**New deps:** Tauri CLI (`cargo install tauri-cli`), sidecar config, optional
`tauri-plugin-window-state`.

**Tests / verification:** mostly manual (it's UI). Verify: fresh machine →
launch overlay → daemon starts → connect one account → row shows live numbers →
connect a second → both visible → disconnect one → expire a token → row shows
`reconnect needed` → re-run fixes it. Confirm the overlay never receives a token
in any response (inspect `/accounts` and `/events` payloads).

**DoD:** you use it daily. **Gate: keep using it for a week before M4.**

---

## M4 — `cuw-tracker` Windows: hooks, DWM bounds, focus-follow

**Objective:** overlay sticks to Windows Terminal across move, resize,
minimise, virtual-desktop switch and monitor change. **Depends on:** M3 (the
window to move). May be built alongside M1b (separate crate and files) but
ships **off by default** and stays off until M3+M1b has had a week of daily
use (plan §7). Docking status is overlay-level (`dock-state` event), never a
per-account state.

Verified on this machine (2026-08-30): Windows Terminal's class is
`CASCADIA_HOSTING_WINDOW_CLASS`; `GetWindowRect` is exactly 7 px wider than
`DWMWA_EXTENDED_FRAME_BOUNDS` on left/right/bottom; a pid-scoped
`SetWinEventHook` sees zero events while WT idles and exactly one
`EVENT_OBJECT_LOCATIONCHANGE` (`idObject=0, idChild=0`) per frame change;
`DWMWA_CLOAKED` is non-zero for the invisible UWP hosts that `EnumWindows`
otherwise returns; the overlay's lock already has `windows 0.61.3` via
`tao 0.35.3`, which sets `PER_MONITOR_AWARE_V2` at `EventLoop::new`; Tauri's
`skipTaskbar` uses `ITaskbarList::DeleteTab`, not `WS_EX_TOOLWINDOW`;
`set_focusable(false)` sets `WS_EX_NOACTIVATE`; `set_position` already passes
`SWP_NOACTIVATE | SWP_NOZORDER`. Not verifiable here: multi-monitor DPI (one
96-DPI display), minimized-window rect values, WezTerm/Alacritty/conhost class
names (inferred), `LOCATIONCHANGE` rate during a drag (inferred ~60–120 Hz).

### M4.1 Types and pure geometry (`crates/cuw-tracker/src/{lib.rs,geometry.rs}`)

- [x] Replace the stub trait with the plan §6 shape: `Bounds { x, y, w, h,
  scale: f64, approximate: bool }` (physical px, virtual-screen origin),
  `Rect { x, y, w, h }` (kept, for work areas), `TargetId(String)` as
  `"win32:<class>|<exe basename, lowercase, may be empty>"`, `TargetSpec {
  class, exe: Option<String> }`, `TrackerConfig { allow, remembered,
  follow_focus }`, `TrackerEvent { Attached(TargetId), Bounds(Bounds),
  Minimized, Restored, Focused(bool), Lost, NotFound }`, `TrackerHandle`
  (`attach(Option<TargetId>)`, `pick_interactively()`, `detach()`, `stop(self)`)
  and `WindowTracker { type Handle; fn start(cfg) -> Result<(Handle,
  Receiver<TrackerEvent>)> }`. `pub mod geometry;` unconditionally.
- [x] `geometry.rs`, no platform code:

```rust
pub enum Corner { TopLeft, TopRight, BottomLeft, BottomRight }
pub struct Anchor { pub corner: Corner, pub dx: i32, pub dy: i32, pub inside: bool } // dx/dy physical px
pub fn overlay_origin(target: &Bounds, overlay: (i32, i32), anchor: &Anchor) -> (i32, i32);
pub fn clamp_to_work_area(origin: (i32, i32), size: (i32, i32), work: &Rect) -> (i32, i32);
pub struct Candidate { pub hwnd: isize, pub class: String, pub exe: Option<String>, pub title: String }
pub fn spec_matches(spec: &TargetSpec, class: &str, exe: Option<&str>) -> bool; // exact class, case-insensitive exe
pub fn rank_candidates(cands: &[Candidate], allow: &[TargetSpec], foreground: Option<isize>,
                       remembered: Option<&TargetId>) -> Option<usize>;
pub fn target_id(class: &str, exe: Option<&str>) -> TargetId;
pub fn parse_target_id(id: &TargetId) -> Option<TargetSpec>;
pub struct Coalescer { last: Option<Bounds> }  impl Coalescer { pub fn push(&mut self, b: Bounds) -> bool } // true = emit
```

`overlay_origin`: `inside=true` puts the overlay inside the target at the
corner, offset toward the interior (TopRight: `x = t.x + t.w - ow - dx, y =
t.y + dy`); `inside=false` puts it adjacent outside, aligned to that edge
(TopRight: `x = t.x + t.w + dx, y = t.y + dy`; TopLeft: `x = t.x - ow - dx`;
Bottom*: `y = t.y + t.h - oh - dy`). Callers scale logical offsets by
`target.scale` before passing them. `clamp_to_work_area` keeps the whole
overlay inside `work` when it fits, else pins the top-left. `rank_candidates`
considers only candidates matching some spec in `allow` (`remembered`, if
given, is also an allowed spec) and prefers: foreground match, then remembered
match, then index 0 (Z order top).

**Tests** (headless, every platform): all four corners × inside/outside;
scale 1.0/1.5/2.0 with scaled offsets; a target at negative virtual-screen
coordinates (monitor left of primary); clamp when the overlay would leave the
work area; `spec_matches` (exact class, case-insensitive exe, generic class
needs an exe); `rank_candidates` (foreground wins, then remembered, then
first; non-allowed foreground ignored); `target_id`/`parse_target_id`
round-trip incl. empty exe; `Coalescer` drops an identical `Bounds` and emits
on any field change.

### M4.2 Win32 helpers (`crates/cuw-tracker/src/windows/{find.rs,bounds.rs,style.rs}`)

- [x] `Cargo.toml`: `[target.'cfg(windows)'.dependencies] windows = { version
  = "0.61", features = ["Win32_Foundation", "Win32_UI_WindowsAndMessaging",
  "Win32_UI_Accessibility", "Win32_Graphics_Dwm", "Win32_Graphics_Gdi",
  "Win32_UI_HiDpi", "Win32_System_Threading"] }`. `src/windows.rs` becomes
  `src/windows/mod.rs`. `HWND` crosses threads as `isize` only.
- [x] `find.rs`: `pub fn candidates() -> Vec<Candidate>` via `EnumWindows`,
  keeping `IsWindowVisible`, `DWMWA_CLOAKED == 0`, `GetAncestor(GA_ROOT) ==
  hwnd`, `GWL_EXSTYLE` lacks `WS_EX_TOOLWINDOW`, with class from
  `GetClassNameW`, exe basename from `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)`
  + `QueryFullProcessImageNameW` (lowercased, `None` on failure), title from
  `GetWindowTextW`. `pub fn foreground() -> Option<isize>`,
  `pub fn root_of(hwnd: isize) -> isize`, `pub fn describe(hwnd) -> Option<Candidate>`,
  `pub fn pid_tid(hwnd) -> (u32, u32)`, `pub fn is_alive(hwnd) -> bool` (`IsWindow`).
- [x] `bounds.rs`: `pub fn read(hwnd: isize) -> Option<Bounds>`: `None` if
  `!IsWindow`; `IsIconic` → the caller emits `Minimized` (return
  `Err(Iconic)` via a small enum); else `DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS)`
  → `approximate: false`, on DWM error `GetWindowRect` → `approximate: true`;
  `scale = GetDpiForWindow(hwnd) as f64 / 96.0` (0 → 1.0). `pub fn
  work_area_for(hwnd) -> Option<Rect>` via `MonitorFromWindow` +
  `GetMonitorInfoW(rcWork)`.
- [x] `style.rs`: `pub fn set_tool_window(hwnd: isize)` ORs
  `WS_EX_TOOLWINDOW` into `GWL_EXSTYLE` then `SetWindowPos(SWP_FRAMECHANGED |
  NOMOVE | NOSIZE | NOZORDER | NOACTIVATE)`; `pub fn assert_topmost(hwnd)` →
  `SetWindowPos(HWND_TOPMOST, .. NOMOVE|NOSIZE|NOACTIVATE)`. Both take `isize`
  and are called by the overlay on its own window from the main thread.

### M4.3 Hook thread (`crates/cuw-tracker/src/windows/hook.rs`, `mod.rs`)

- [x] `pub struct WindowsTracker; impl WindowTracker for WindowsTracker`.
  `start` spawns `std::thread::Builder::new().name("cuw-tracker")`, which
  first forces its message queue into existence with `PeekMessageW(&mut m,
  None, WM_USER, WM_USER, PM_NOREMOVE)` (a thread has no queue until its
  first User32 queue call, and `PostThreadMessageW` to a queue-less thread
  fails with `ERROR_INVALID_THREAD_ID` — an `attach` right after `start`
  would intermittently report the thread dead), then records
  `GetCurrentThreadId`, sends it back over a oneshot, installs the global
  `EVENT_SYSTEM_FOREGROUND` hook (`WINEVENT_OUTOFCONTEXT |
  WINEVENT_SKIPOWNPROCESS`), applies `cfg.remembered` as an initial
  `attach(Some(id))` if set, then runs `while GetMessageW(&mut msg, None, 0,
  0).as_bool() { .. }` dispatching `WM_APP+1` (drain commands), `WM_APP+2`
  (deferred re-acquire/pick resolution, `wParam` = hwnd), `WM_TIMER` on
  `wParam == search_timer | pick_timer`, else `TranslateMessage;
  DispatchMessageW`.
- [x] Handle: `Arc<Mutex<VecDeque<Cmd>>>` + `PostThreadMessageW(tid, WM_APP +
  1, 0, 0)` to wake; `Cmd = Attach(Option<TargetId>) | Pick | Detach`.
  `stop(self)` posts `WM_QUIT`, takes the `JoinHandle`, joins with a 2 s cap
  and sets `stopped`; `Drop` posts `WM_QUIT` only when the thread is still
  joinable (never a second post that could land on a recycled thread id).
  Handle methods do no Win32 work.
- [x] Timers: `SetTimer(None, ..)` ignores the id passed and returns a fresh
  one; `HookState` keeps `search_timer: Option<usize>` and `pick_timer:
  Option<usize>` from the return values, dispatches `WM_TIMER` on
  `msg.wParam.0 == id`, and `KillTimer(None, id)` with the stored id.
- [x] Callback state in a `thread_local! RefCell<Option<HookState>>` on the
  pump thread: target hwnd, `Sender<TrackerEvent>`, `Coalescer`, target hook
  handles, `picking_until`, `searching`, remembered spec, allow list. The
  `WINEVENTPROC` body: filter `idObject == OBJID_WINDOW.0 && idChild ==
  CHILDID_SELF as i32`, then by event id; no panics, no locks other than the
  `RefCell`, no Tauri calls.
- [x] Attach (`Cmd::Attach` / re-acquire / pick result): resolve the target
  (`rank_candidates` over `find::candidates()`, or the picked root), unhook
  any previous target hooks, install two target-scoped hooks (pid+tid from
  `pid_tid`): `EVENT_SYSTEM_MOVESIZESTART..=EVENT_SYSTEM_MINIMIZEEND` and
  `EVENT_OBJECT_DESTROY..=EVENT_OBJECT_LOCATIONCHANGE`, plus
  `EVENT_OBJECT_CLOAKED..=EVENT_OBJECT_UNCLOAKED`; emit `Attached(id)`, then
  `Minimized` or `Bounds`; store the spec as remembered. No candidate → if
  the attach had a spec (explicit id, or a remembered one), `searching =
  true`, start the 2 s timer, emit `NotFound` (the consumer shows
  `detached`); with no spec at all emit `NotFound` and stay idle.
- [x] Event mapping: `LOCATIONCHANGE | MOVESIZEEND | MINIMIZEEND | UNCLOAKED |
  SHOW` → `bounds::read` → `Bounds` (through the `Coalescer`) or `Minimized`;
  `MINIMIZESTART | CLOAKED | HIDE` → `Minimized`; `MINIMIZEEND | UNCLOAKED` →
  `Restored` before the `Bounds`; `DESTROY` on the target → `Lost`, unhook
  target hooks, `searching = true`, `SetTimer(2 s)`; `FOREGROUND` (global) →
  `Focused(root == target)` when attached; when `searching` or `picking`,
  the callback only stores the hwnd and `PostThreadMessageW(own_tid,
  WM_APP+2, hwnd, 0)` — re-acquire and pick resolution (`EnumWindows`,
  `OpenProcess`, `SetWinEventHook`) run from the pump, never inside the
  `WINEVENTPROC`. `WM_TIMER` while searching → try re-acquire from the
  remembered spec. Every use of the target first checks `is_alive`, else
  treats it as `DESTROY`.
- [x] Pick: `Cmd::Pick` records `pick_ignore = find::foreground()` and
  `pick_started = Instant::now()`, sets `picking_until = now + 10 s`; a
  `FOREGROUND` hwnd is accepted only if `now - pick_started > 300 ms`,
  `root_of(hwnd) != pick_ignore`, its pid ≠ ours, it is not cloaked, and its
  class is not `Shell_TrayWnd` / `Progman` / `WorkerW` (closing the tray
  menu re-activates the previous window and fires `FOREGROUND` before the
  user clicks anything); the accepted root becomes the target via `describe`
  (allow list not consulted); on timeout → `NotFound`.
- [x] `#[ignore]` live test (Windows only): spawn `cmd.exe /c pause` with
  `CREATE_NEW_CONSOLE`, wait for a `ConsoleWindowClass` window owned by that
  pid, `start` with `allow = [ConsoleWindowClass]`, `attach(None)`, expect
  `Attached` + `Bounds` within 1 s; `SetWindowPos` the console (we own it)
  and expect a new `Bounds` within 500 ms matching the DWM rect; kill it and
  expect `Lost` within 500 ms; `stop()` returns. Never touches the user's
  terminal.

### M4.4–M4.7 Overlay glue (`apps/overlay/src-tauri/src/dock.rs`, `lib.rs`)

- [x] `dock.rs`: `DockState` (`Undocked | Picking | Docked(TargetId) |
  Detached(TargetId)`) in `SharedDock = Arc<Mutex<DockCtl>>` with the
  handle, the last `Bounds`, the target hwnd, the overlay's own hwnd (cached
  once on the main thread in `ensure_started`, never re-read from the
  consumer), and the consumer thread's join. Commands (all `async
  #[tauri::command]`, so they never run inline on the main thread while a
  lock is held): `dock_start(target: Option<String>)` (Some = a `TargetId`,
  None = best allowed / remembered), `dock_pick()`, `dock_stop()`,
  `dock_state()`. Emits `dock-state` `{ "state": "undocked" | "picking" |
  "docked" | "detached", "target": string | null }` on every transition.
- [x] **Lock discipline (deadlock by construction otherwise):** the
  `DockCtl` lock is never held across any Tauri window call — copy
  `target_hwnd`, `last`, the own hwnd and a settings snapshot out under the
  lock, release, then call `outer_size`/`set_position`/`show`/`hide`. Every
  raw-HWND call (`set_tool_window`, `assert_topmost`,
  `SetForegroundWindow`) goes through `app.run_on_main_thread(move || ..)`
  with the hwnd captured as `isize` (cross-thread `SetWindowPos(SWP_FRAMECHANGED)`
  is a synchronous `SendMessage` to the owning thread). Non-async commands
  run inline on the main thread (tauri-macros only spawns `async`
  commands), and `outer_size`/`hwnd`/`scale_factor` from another thread
  block on the main thread — a sync command holding the dock lock plus a
  consumer holding it while calling a getter is a hang.
- [x] `pub fn restyle(app: &AppHandle)` = `run_on_main_thread(|| set_tool_window(hwnd))`,
  called after **every** `show`/`hide`/`set_focusable`/`set_ignore_cursor_events`/
  `set_always_on_top` in `tray.rs`, `lib.rs` and `dock.rs` — tao 0.35.3
  `apply_diff` rewrites `GWL_EXSTYLE` from its own flags on any flag diff,
  `VISIBLE` included, so the bit is gone after the first hide/show (plan §8
  Q7). Ordering is safe: `run_on_main_thread` and the window calls travel the
  same event-loop proxy FIFO.
- [x] Consumer thread: `rx.recv()`, drain `try_recv()` to the last `Bounds`
  (non-`Bounds` events are processed in order), compute `overlay_origin` with
  the settings' corner/offset (offsets × `bounds.scale`) and
  `window.outer_size()`, clamp to the virtual screen
  (`bounds::virtual_screen()` via `GetSystemMetrics(SM_XVIRTUALSCREEN..)`)
  and only to `work_area_for(target)` when the result would be entirely off
  every monitor, `window.set_position(Physical(..))`. `Minimized` →
  `hide()`; `Restored` → `show()` + `assert_topmost`; `Focused(false)` with
  `follow_focus` → `hide()`, `Focused(true)` → `show()`; `Attached` → state
  `Docked`, `settings::update` for `dock.enabled`/`dock.remembered` (the
  update must not run window-style work — see M6.3); `Lost` → **`show()`
  first** (a follow-focus hide would otherwise hide the detached badge) then
  `Detached` (window stays where it is); `NotFound` → `Detached{remembered}`
  when `dock.enabled` and a remembered target exists (the tracker is
  searching), `Undocked` only after a pick timeout or when nothing was
  remembered. Every show/hide is followed by `restyle`. Re-run placement on
  the window's `Resized` event **only when the window is visible and neither
  side of `outer_size` is zero** (hide/minimise fire `Resized(0×0)`).
- [x] Setup order in `run()`: window exists → `set_focusable(false)` only
  when docking starts (not at boot: modals need keyboard focus while
  undocked) → `set_tool_window(hwnd)` at boot and `restyle` after every
  style-changing call → if `settings.dock.enabled` and `remembered` is set,
  **`ensure_started` only** — the tracker's own `cfg.remembered` performs
  the attach; a second `attach(None)` from `dock_start` would emit two
  `Attached` events and two settings writes.
- [x] Undocked mode is untouched: the tracker thread is not even started
  until docking starts.

**DoD:** the plan §7 M4 row. Manual matrix: move WT, resize WT,
minimise/restore, Win+D, move WT to virtual desktop 2 and switch there
(overlay visible and docked — plan §8 Q13 if not), drag WT to a second
monitor with a different scale (needs an external display), close WT →
`detached`, reopen WT → re-attached, start the overlay with WT closed then
open WT → attached without a click, Alt-Tab (overlay absent), tray hide then
show then Alt-Tab (still absent), type in WT while docked (never loses
focus), open the connect modal while docked (typing works, focus returns to
WT on close via `SetForegroundWindow`), pick from the tray menu (the
previously active window is not auto-picked).

**Risks:** anything blocking or panicking in the callback wedges the hooks
(re-acquire/pick work is posted to the pump, never run in the callback);
`WS_EX_TOOLWINDOW` **is** cleared by every tao flag diff (re-apply via
`restyle`); other `HWND_TOPMOST` windows (NVIDIA overlay, games) can fight
for z-order — re-assert only on `Attached`/`Restored`, never per `Bounds`;
a lock held across a Tauri window getter from the consumer thread deadlocks
against a sync command on the main thread.

---

## M5 — `cuw-tracker` macOS: CGWindowList fallback, then AX upgrade

**Objective:** widget sticks to iTerm2/Terminal.app; degrades cleanly without
Accessibility. **Depends on:** M4 (shared trait shape proven).

- [ ] **M5.1** Overlay window native config via `NSWindow` on the Tauri handle:
  borderless, `level=.floating`, `collectionBehavior=[.canJoinAllSpaces,
  .fullScreenAuxiliary]`.
- [ ] **M5.2** Focus via `NSWorkspace.didActivateApplicationNotification` (no
  permission).
- [ ] **M5.3** Position fallback: poll `CGWindowListCopyWindowInfo` at ~10Hz for
  the target bounds. No Accessibility grant needed — this is the default path.
- [ ] **M5.4** Position upgrade: when Accessibility is granted, use
  `AXObserver` on `kAXWindowMoved/ResizedNotification` for event-driven
  tracking. One-line in-UI prompt to grant; never a hard blocker.
- [ ] **M5.5** Re-acquire by bundle id after restart; `Gone` → `detached`.

**Deps:** `objc2` / `core-graphics` / `accessibility` crates (or equivalents).

**DoD:** tracks Terminal.app/iTerm2 via CGWindowList without any grant; upgrades
to smooth AX tracking when permission is granted; undocked mode unaffected.

**Risks:** macOS docking is degraded-by-default by design (plan §6) — the UI must
say so honestly. AX permission prompts are the classic sharp edge.

---

## M6 — Polish

**Objective:** the niceties, plus the M3 correctness gaps found in review.
None are load-bearing; all live in `apps/overlay` and the daemon wire fields
from M1b.6.

Verified on this machine (2026-08-30): Tauri 2.11.5 with the `tray-icon`
feature builds offline (`tray-icon 0.24.2`, `muda 0.19.3` already in the
lock); `tauri-plugin-autostart 2.5.1`, `-opener 2.5.4`, `-single-instance
2.4.3` are now in the registry cache; `-webkit-app-region` is not honoured by
WebView2/wry (Tauri's drag script keys on `data-tauri-drag-region` and calls
`plugin:window|start_dragging`, which `core:window:default` does **not**
allow); `core:event:default` allows `listen`; `tauri-plugin-window-state`'s
default flags persist `VISIBLE`, so a hidden overlay would come back hidden;
`%APPDATA%\com.local.cuw` is the app config dir (the plugin's dir too).

### M6.1 Settings (`apps/overlay/src-tauri/src/settings.rs`)

- [x] Rust-owned `settings.json` in `app.path().app_config_dir()`, written
  atomically (tmp + rename), missing/corrupt → defaults + `warn!`. Held in
  `tauri::State<Mutex<Settings>>`. Commands `get_settings() -> Settings`,
  `set_settings(SettingsPatch) -> Result<Settings, String>`: a **patch**, not
  a whole object — every field `Option`, `dock` a nested `DockPatch`, merged
  under the lock, then clamp/validate, persist, apply side effects, emit
  `settings-changed` with the stored value. `dock.remembered` is Rust-owned
  and ignored if present in a patch. The panel sends only the fields the
  user actually touched. Otherwise an `Attached` event that rewrote
  `dock.enabled`/`remembered` on disk while the panel was open would be
  reverted by the panel's stale full copy on Save (last-writer-wins
  clobbering).

```json
{"version":1,"opacity":0.85,"compact":false,"thresholds":{"warn":75,"crit":90},
 "autostart":false,"click_through":false,"show_scoped":true,
 "colors":{"work-abc12345":"#6ea8fe"},
 "dock":{"enabled":false,"remembered":null,"corner":"top_right","offset":{"x":8,"y":8},
         "inside":false,"follow_focus":false,
         "allow":[{"class":"CASCADIA_HOSTING_WINDOW_CLASS","exe":null},
                  {"class":"ConsoleWindowClass","exe":null},
                  {"class":"org.wezfurlong.wezterm","exe":null},
                  {"class":"Alacritty","exe":null},
                  {"class":"Chrome_WidgetWin_1","exe":"Code.exe"},
                  {"class":"Chrome_WidgetWin_1","exe":"Hyper.exe"}]}}
```

Validation: `opacity` clamped `0.2..=1.0`; `warn < crit`, both `1..=100`;
colours must match `^#[0-9a-fA-F]{6}$` or are dropped; `corner` is one of
`top_left | top_right | bottom_left | bottom_right`. Every field
`#[serde(default)]` so an older file loads.

### M6.2 Tray, close-to-hide, daemon stop (`tray.rs`, `lib.rs`)

- [x] `tauri = { features = ["macos-private-api", "tray-icon", "image-png"] }`;
  icon from `include_bytes!("../icons/32x32.png")`. Menu: `Show/Hide overlay`
  (relabelled), `Dock to window…` / `Undock` (calls `dock_pick` /
  `dock_stop`; disabled label until M4.3 lands), `Click-through`
  (`CheckMenuItem` bound to settings), `Settings…` (emits `open-settings`,
  shows the window), separator, `Quit`. Left click toggles visibility.
- [x] `on_window_event`: `CloseRequested` → `prevent_close` + `hide()` while
  the tray exists. `Quit` → `stop_daemon()` → `app.exit(0)`.
- [x] `stop_daemon`: `POST /shutdown` with the bearer over a hand-written
  HTTP/1.1 request on `std::net::TcpStream` (no new dep), wait ≤ 2 s for the
  port to close, else kill by **pid file** (`data_dir/pid`, written by the
  daemon unconditionally): `taskkill /PID <pid> /F /T` — regardless of who
  spawned the daemon; then `Child::kill()` + `wait()` if we hold a child.
  Keep the `Child` from `spawn_daemon` in `State<Mutex<Option<Child>>>`
  instead of dropping it, and in the dev fallback spawn
  `target/debug/cuw-daemon.exe` directly when it exists (with `cargo run`
  the retained child is `cargo`, and killing cargo on Windows leaves
  `cuw-daemon.exe` running). Never log the bearer. Quit during an open
  connect modal must leave no `cuw-daemon` and no `claude` we spawned (the
  daemon aborts the tracked connect task on shutdown, M1b.7).
- [x] `tauri_plugin_window_state::Builder::default().with_state_flags(StateFlags::SIZE | StateFlags::POSITION)`.

### M6.3 Click-through and autostart (`lib.rs`)

- [x] `set_click_through(on)`: `set_ignore_cursor_events(on)` +
  `set_focusable(!on)` + `restyle`; applied from settings at startup and
  from the tray item; the tray is the way back, so never persist
  `click_through: true` without the tray.
- [x] `modal_interactive(on: bool)` (async command, replaces the earlier
  dock-only `dock_interactive`): `on = true` → `set_ignore_cursor_events(false)`,
  `set_focusable(true)`, `set_focus()`, `restyle` — **regardless of dock or
  click-through state**, because a click-through window swallows clicks on
  the connect/settings/confirm modals and a non-focusable one cannot take
  keys; `on = false` → re-derive from settings (click-through → ignore
  cursor + non-focusable; docked → non-focusable), `restyle`, and when
  docked hand focus back with `run_on_main_thread(|| SetForegroundWindow(target))`
  (allowed: we are the foreground process at that moment). The tray's
  `Settings…` calls `modal_interactive(true)` **before** emitting
  `open-settings`.
- [x] `settings::on_changed(app, old: &Settings, new: &Settings)` diffs and
  applies only what changed: click-through via `run_on_main_thread`,
  autostart only when `autostart` changed (registry I/O), and
  `dock::replace_last(app)` when any `dock.{corner,offset,inside}` changed
  so the overlay moves without waiting for the target to move. It is called
  from `settings::update` on **any** thread (the dock consumer calls it on
  `Attached`), so it must never do window-style work inline.
- [x] `tauri-plugin-autostart` (`.app_name("cuw-overlay").args(["--autostart"])`);
  `set_settings` enables/disables; startup reconciles `is_enabled()` with the
  flag. Enable only when `!cfg!(debug_assertions)`; a debug build stores the
  flag, logs a warning and the settings panel says "dev build: not
  registered".

### M6.4 Web UI correctness fixes (`src/main.js`, `styles.css`, `capabilities/default.json`)

- [x] Drag: remove `-webkit-app-region`; `mousedown` on `#widget` (ignoring
  `button, input, a, .modal` targets) calls
  `window.__TAURI__.window.getCurrentWindow().startDragging()`; skipped when
  docked or click-through. Capability adds `core:window:allow-start-dragging`.
- [x] Esc: one `document` `keydown` handler closes the open modal through its
  cancel path (the connect modal only when the flow is not active).
- [x] SSE reconnect: exponential backoff 1 s → 30 s with ±20 % jitter, reset
  on a good frame; the `start_daemon` probe stays per pass.
- [x] Modal input: `buildModal` invokes `modal_interactive(true)`,
  `closeModal` invokes `modal_interactive(false)` (M6.3) — needed for
  click-through as much as for docking.
- [x] Sign-in link: `tauri-plugin-opener` (cached) or a Rust `open_url`
  command; `target=_blank` is inert in the webview.

### M6.5 Settings panel and display polish (`src/index.html`, `main.js`, `styles.css`)

- [x] In-window panel on the existing modal system (no second window): dock
  enabled/corner/offset/inside/follow-focus + a `Pick window…` button
  (`dock_pick`) and the remembered target; opacity slider (live via
  `--bg-alpha`); compact toggle (hides bars and secondary lines); warn/crit
  numbers; autostart; click-through (with "turn off from the tray"); show
  scoped; per-account colour swatches for the current rows. Save →
  `set_settings`; `listen('settings-changed')` re-renders; a gear button in a
  small header opens it; `open-settings` from the tray opens it.
- [x] `level()` reads `settings.thresholds`; rows set `--bar` from
  `settings.colors[id]` or a default palette by index; `body.compact` and
  `--bg: rgba(20,20,22,var(--bg-alpha))`.
- [x] Dock badge from `dock-state`: `docked · <target>`, `detached —
  searching`, `picking — click a window`; a `Dock`/`Undock` button next to
  `+ Connect account` (hidden until M4.3 lands).

### M6.6 Wire-driven display (integration item I1, after M1b)

- [x] Two-line rows: `5h 31% · resets in 2h 14m` / `7d 14% · resets in 3d
  4h` from `resets_at` / `seven_day_resets_at`; a 30 s interval updates only
  the countdown text nodes. `stale: true` → `.stale` (dimmed) + `last update
  4m ago` from `fetched_at`; `refresh: "backoff"` → `refreshing…`; `refresh:
  "rejected"` → reason next to `reconnect needed`; `access_expires_at` only
  as a tooltip. `scoped[]` → a collapsed `▸ Fable 7d 12%` line when
  `show_scoped`, `is_active` highlighted. Remove `expiryHint`.

### M6.7 Reconnect UX (integration item I2, after M1b)

- [x] `Reconnect` opens a confirm (`ok` label `Reconnect`, text "This re-runs
  the sign-in and replaces the stored credential for this account.") then
  `POST /accounts/:id/reconnect` (row id stays stable; no `DELETE` of the old
  id). Connect log first line becomes `Launching claude auth login…`;
  `token_captured` reads `Credential captured.`

### M6.8 Icons and bundle (docs-and-build item D3)

- [x] `bundle.active: true`, `bundle.icon` list (icons already generated),
  Windows target `nsis`, and the daemon as a bundle resource in the **map
  form** `"resources": { "../../../target/release/cuw-daemon.exe": "cuw-daemon.exe" }`
  — the list form maps each `..` to `_up_` (tauri-utils 2.9.3
  `resources.rs:24`), so the daemon would land at
  `<install>/_up_/_up_/_up_/target/release/cuw-daemon.exe` and the sibling
  lookup in `lib.rs` would never find it. `scripts/build-release.ps1`
  builds the daemon **before** `cargo tauri build` (tauri-build fails on a
  missing resource) and also copies it next to the un-bundled
  `target/release/cuw-overlay.exe`. Ad-hoc only (plan §1).

**DoD:** the plan §7 M6 row.

---

## M7 — Session switcher

Design and milestone breakdown live in [SWITCHER.md](SWITCHER.md); it is not
duplicated here. Independent of M1b and M4 — a new crate plus one hook in the
connect flow.

- [x] **M7.1** — `cuw-launch`: `SessionLauncher` trait, the generated shims
      (`session-shim.ps1` / `.sh`), pure command construction in `plan.rs`, and
      the Windows `CreateProcessW` spawn. 25 tests + one `#[ignore]` that opens
      a real console. Verified against a stub `/session/:nonce` and a stand-in
      `claude`.
- [x] **M7.2** — `setup-token` capture in the connect flow, keyring `<id>#cli`,
      `POST /accounts/:id/session` and `GET /session/:nonce`.
      - `cuw-connect`: one reusable PTY driver (`run_cli`) behind both steps,
        picked by a `Watch` (credential file vs. token line). The code channel
        is **borrowed**, not consumed, so the modal feeds both. New phases
        `setup_token` / `cli_token_captured` — the flow has **two** browser
        steps and the modal has to say so (M7.3). The scope gate and validating
        fetch run *before* the second consent screen. A failed capture is a
        display state: the account connects with `cli_token: None`.
      - Token parse: keyed on the `sk-ant-oat` prefix (version digits not
        pinned), only a run terminated inside the buffer counts, 240-column PTY
        so it cannot wrap. Still redacted on every emitted frame.
      - `cuw-creds`: `put_cli`/`get_cli`/`delete_cli` under `cli_key(id)` =
        `<id>#cli`, versioned blob that cannot cross-decode with a `Credential`.
      - `cuw-daemon`: `session.rs` holds the launch codes (32 random bytes hex,
        30 s TTL, single-use, bounded, clock-injected so the rules are tested
        without sleeping). `GET /session/:nonce` answers `no-store` and is the
        only route that returns a token; unknown/spent/expired are one answer.
        `DELETE /accounts/:id` drops both grants. Wire gained `can_switch`.
      - 191 tests green; live pass against an isolated daemon (real keyring
        round trip, real launch, real redemption, no token in the log).
- [x] **M7.3** — overlay button, `switch unavailable` state, confirmation.
      - `main.js`: a `▸` button on every row the wire marks `can_switch`, a
        confirmation naming the account and the start directory, then
        `POST /accounts/:id/session` with `{cwd, terminal}` — the answer is
        `ok`, so the overlay still never sees a token. A launch in flight is
        tracked in a `Set`, not on the button: the row is rebuilt on every
        accounts frame, so a disabled button would not survive one.
      - `switch unavailable` is a row state with an *Enable* action into the
        reconnect flow (SWITCHER §6), suppressed on a row that already asks for
        a reconnect. A 409 from the route says the same thing in a modal, for
        the race where the grant went away between frames.
      - `settings.session` (Rust-owned like the rest): `cwd` blank = the
        daemon's default, `terminal` argv one-per-line, never re-split, blank
        entries dropped in `validate`. Panel section, patch, and the
        `refreshPanelFromSettings` arms that keep untouched fields current.
      - The connect modal announces **two** browser steps and logs the
        `setup_token` / `cli_token_captured` phases M7.2 added.
- [ ] **M7.4** — macOS launcher and verification (needs a Mac). `open` does not
      forward arguments to a document, so the nonce/port/cwd need a per-launch
      wrapper; `macos.rs` carries the note.

**DoD:** clicking a row's switch button opens a terminal already signed in as
that account, in the chosen directory, with no token in any process argument,
any log, or the overlay.

---

## Testing strategy (whole app)

- **Unit:** parse (golden + malformed), poller timing, credential-file and
  token-response parsing, refresh state machine, redaction, docking geometry.
  These are the correctness core — keep them fast and CI-gated.
- **Golden files:** `usage_ok.json` (S1) and `token_ok.json` (M1b, fake
  values) are the contracts with the undocumented endpoints; a shape change
  fails CI loudly (plan §8 Q2), which is the point.
- **Integration:** daemon routes against a fake `UsageSource` and a fake
  `TokenRefresher`; creds round-trip and the self-spawned-console tracker test
  (both ignored by default, run on the target OS).
- **Redaction CI gate:** grep captured logs/test output for the bearer prefix and
  any known token markers; fail if present.
- **Manual matrix (M3+):** the connect/disconnect/expire walkthrough in M3, run
  on Windows and macOS before calling a milestone done.

---

## Sequencing & gates

```
S1(✗) ──▶ M0 ──▶ M1 ──▶ M2 ──▶ M3 ──▶ M1b ═══ SHIP ═══▶ (1 week daily use) ──▶ enable M4 ──▶ M5
                                        │
                                        ├─ Track A: M1b (core → creds → connect → daemon)
                                        └─ Track B: M4 + M6 built in parallel, default-off
                                                  └─ integration: overlay renders M1b wire, reconnect UX, live script
```

- **S1 was a hard gate and it failed** — M1b replaces the credential source.
  Nothing ships until M1b's DoD holds: it is the blocker.
- **M3 + M1b is the ship point.** M4/M6 code may be built alongside (separate
  crates and files), but docking stays default-off until a week of daily use
  (plan §7). Enabling it is a settings flip, not a build.
- Track A and Track B never edit the same file; the overlay's `main.js`,
  `styles.css`, `index.html` and `lib.rs` belong to Track B and integration
  items make the final cross-cutting edits after both tracks land.
- M4 and M5 share the tracker trait; do Windows first, then macOS.

---

## Definition of done (the whole app)

- Multiple accounts connect from inside the widget and show live server-side
  5h/7d usage simultaneously.
- Every failure is a display state (`unavailable` / `reconnect needed` /
  `detached`), never a panic and never a wrong number.
- Tokens live only in the OS store and the daemon; they never reach the overlay,
  logs, or the wire. CI proves it.
- Undocked mode is fully usable on Windows and macOS (M3). Docking works on both
  and degrades honestly (M4/M5).
- If a public `claude usage` / Admin endpoint ships, the undocumented client is
  a single `UsageSource` impl swap (plan §9).

---

## Risk register

| Risk | Where | Mitigation |
|---|---|---|
| `setup-token` scopes rejected — **confirmed live 2026-08-30** (403, needs `user:profile`) | S1 | Client maps 403 → `Forbidden` → `reconnect needed`, no retry hammering. Token source moved to `auth login` + refresh (M1b). |
| Token endpoint shape/behaviour differs from assumptions (units, rotation, encoding, error bodies) | M1b | Defensive `parse_refresh`; 200-with-bad-shape → backoff + loud log; 4xx is `reconnect needed` only with a whitelisted OAuth error code, otherwise `Contract` → backoff + loud log (a WAF or encoding mismatch must not become a reconnect loop); error bodies never logged (they could echo the refresh token). |
| Refresh rotates the token but the keyring write fails | M1b | Keep the in-memory credential, retry the write each cycle, never re-refresh because of a failed write; `persist_pending` on the wire so the coming reconnect is not a surprise. |
| Scratch config dir left behind with tokens in plaintext | M1b | Scratch under the daemon's data dir; await child exit before scrubbing; overwrite `.credentials.json` then remove with retries; `Drop` guard; startup sweep. |
| A dead account keeps hitting the token/usage endpoints | M1b | `reconnect needed` ends the poll task; refresh backoff floored at 60 s; seeded tasks staggered. |
| Hook callback wedges the tracker thread | M4 | Callback = filter + DWM read + channel send only; re-acquire/pick posted to the pump; no Tauri calls, no locks, no panics. |
| Consumer thread deadlocks against the main thread | M4 | Dock lock never held across a Tauri window call; raw-HWND calls via `run_on_main_thread`; commands `async`. |
| Tray Quit leaves the daemon or a spawned `claude` alive | M6 | Connect runs as an abortable task; serve shutdown capped at 3 s; pid file + `taskkill` fallback; dev spawns the daemon exe directly. |
| Docking to the wrong window (generic classes, cloaked windows) | M4 | Specs are `{class, exe}`; cloaked/tool windows filtered; picker resolves to root. |
| Click-through leaves the widget unreachable | M6 | Tray toggle always available; click-through never persisted without a tray. |
| Endpoint shape changes silently | M0/ongoing | Golden test fails CI; state → `unavailable` |
| Token leak to overlay/logs | all | `redact` everywhere + CI grep gate |
| Polling too aggressively → 429/ban | M2 | `poller.rs` jitter + hard backoff; ≤1/min/account |
| Docking eats the schedule | M4/M5 | Hard gate after M3 + a week of use |
| `setup-token` re-run invalidates prior token | M1/M3 | Warn before reconnect; detect 401 → reconnect UI |
| macOS multi-account (if S1 fails) | M1 | Owned credential avoids Keychain collision; else §5 fallback |
