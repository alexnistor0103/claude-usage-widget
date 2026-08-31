# Session Switcher — design (M7)

A per-account button in the widget that starts a **new** Claude Code session
signed in as that account. Companion to [plan.md](plan.md); read plan §4 (connect
flow) and §5 (credential storage) first — this reuses both.

Not landed in `plan.md` yet: §1 lists *"No account switching or automation.
Display only"* as a non-goal. Adopting this design means amending that line.

---

## 1. What it is and is not

- **Is**: "open a terminal running `claude` as account X, in this directory".
- **Is not**: `/login`. A running session holds its credential in memory; no
  outside process can move it. Existing sessions stay on their account until
  restarted. This limit applies to every possible design, not just this one.

---

## 2. Why this mechanism

Three candidates were weighed:

| | Cross-platform | Risk |
|---|---|---|
| A. Overwrite the CLI's credential store | No — Windows file vs macOS Keychain (plan §5) | Shared refresh-token rotation revokes the widget's credential, or logs the CLI out mid-session |
| B. A `CLAUDE_CONFIG_DIR` profile per account | No — the macOS Keychain service name looks fixed, so profiles collide on one item (plan §5) | Splits settings, MCP config and history per profile |
| **C. Inject `CLAUDE_CODE_OAUTH_TOKEN` at launch** | **Yes — identical on both** | Bounded; see §6 |

C never answers the question "where does the CLI keep credentials", so the whole
macOS asymmetry that killed A and B does not arise. It also leaves the user's own
login, settings and history untouched — nothing is overwritten, so nothing needs
a backup or a restore path.

`CLAUDE_CODE_OAUTH_TOKEN` is already known to bind another identity: it is on the
`SCRUBBED_ENV` list the connect flow clears before spawning the CLI
(`crates/cuw-connect/src/lib.rs`).

---

## 3. The second credential

Each account gets a **second, independent credential**: a long-lived token from
`claude setup-token` (scope `user:inference` — all the CLI needs), stored beside
the first under keyring key `<id>#cli`.

Independence is the property that makes this safe. The widget's credential
(`user:profile`, refreshed by the daemon) and the CLI's token come from separate
grants, so neither rotation can revoke the other. Every design that *copies* one
credential into two refreshing owners has that failure mode; this one does not.

**Captured during the existing connect flow, at close to no extra cost.**
While the scratch `CLAUDE_CONFIG_DIR` is still signed in as the account, run
`claude setup-token` in the same dir, capture the token, then scrub as today
(plan §4).

Not quite "one sign-in", though: `setup-token` opens **its own browser consent
screen** (§9). The connect flow therefore has *two* interactive browser steps,
and the connect modal must say so or the second one reads as a bug. Both land on
whichever account the browser profile is signed into, so a second account is
added from another profile or a private window.

An already-connected account reaches a `<id>#cli` capture through the same
reconnect path, not a migration: `POST /accounts/:id/reconnect` runs the whole
two-step flow again. An account with no `<id>#cli` entry is simply
`can_switch: false` on the wire.

`setup-token` prints the token to the terminal, so the PTY capture must redact it
before any `ConnectEvent` — the existing redactor covers this, but the parse is
"read from output", not "read a file". As built (M7.2): the token is keyed on
its own `sk-ant-oat` prefix rather than on surrounding prose, only a run
*terminated* inside the buffer counts (it arrives across PTY chunks), and the
`setup-token` PTY is opened at 240 columns so the line cannot wrap and be
captured truncated.

---

## 4. Wiring

New crate `cuw-launch`, platform behind a trait, matching `WindowTracker` and
`CredentialStore`:

```rust
trait SessionLauncher {
    fn launch(&self, req: LaunchRequest) -> Result<(), LaunchError>;
}
struct LaunchRequest { nonce: String, port: u16, cwd: PathBuf }
```

The token never appears in process arguments — a process list is readable by any
process in the session. Instead, a **shim**:

- `POST /accounts/:id/session` — the daemon mints a single-use nonce (TTL 30 s)
  and spawns a terminal on the shim (`session-shim.ps1` / `.sh`, generated in the
  data dir, holding no secret).
- `GET /session/:nonce` — the shim redeems it once, puts the token in its own
  environment, and `exec`s `claude`. The nonce is burned on read.

The shim is also what makes the launcher terminal-agnostic: nothing depends on a
terminal emulator inheriting the daemon's environment (§5).

---

## 5. Platforms

| | Windows | macOS |
|---|---|---|
| Spawn | `powershell.exe -NoExit -File session-shim.ps1`, `CREATE_NEW_CONSOLE` | `open -a Terminal session-shim.sh` |
| Why not the nicer terminal | `wt.exe` hands off to an existing `WindowsTerminal.exe`; the environment does not reliably reach the new pane (Q2) | `open` goes through LaunchServices, which does not inherit the caller's environment |
| Override | `settings.session.terminal` — any command, because the shim fetches its own token | same |

An override is argv, never a shell string: one containing `{shim}` replaces the
default command outright (`{nonce}`, `{port}`, `{cwd}` also substitute), anything
else is a prefix and the default is appended. Nothing is ever re-split, so a path
with spaces cannot come apart.

The overlay owns both `settings.session.terminal` and `settings.session.cwd` and
sends them with each request (M7.3). Neither is required: an empty terminal is
the platform default, and an empty directory leaves the choice to the daemon,
which starts the session in the user's home.

**The spawn cannot go through `std::process::Command`** (M7.1 finding).
`Command` always sets `STARTF_USESTDHANDLES` and hands the child the daemon's own
stdio — and the daemon's stdout is the widget's log file, its stdin `NUL`
(`apps/overlay/src-tauri/src/lib.rs`). Inherited, the session's output lands in
the log instead of the console and `-NoExit` dies on the closed stdin, so the
window vanishes. `CreateProcessW` with `bInheritHandles = FALSE` and no
`STARTF_USESTDHANDLES` gives the child the handles of the console
`CREATE_NEW_CONSOLE` just created, which is the entire point of the window. The
same reasoning will apply to whatever macOS spawns in M7.4.

---

## 6. Hard rules

- The CLI token leaves the daemon **only** through `/session/:nonce`: bearer-gated,
  single-use, expiring, never logged. This is a deliberate, bounded exception to
  plan §5 *"tokens never leave the daemon"* and must be documented there.
- `/session/:nonce` never returns the `user:profile` credential. Different key,
  different scope, different route.
- The overlay sends a `POST` and receives `ok`. It never sees a token — the plan §5
  invariant for the overlay is unchanged.
- An account with no CLI token is a **display state**, not an error: the row shows
  `switch unavailable` with a reconnect action (plan §9).
- Launching never touches `~/.claude`, the Keychain, or the user's own login.

---

## 7. Milestones

M7 is independent of M1b and M4 — new crate, new files, plus one hook in the
connect flow.

| | | |
|---|---|---|
| M7.1 | `cuw-launch`: trait, shims, Windows impl | **done** |
| M7.2 | `setup-token` capture in connect, keyring `<id>#cli`, both routes | **done** |
| M7.3 | Overlay button, states, confirmation | **done** |
| M7.4 | macOS impl and verification | 0.5 d (needs a Mac) |

Only M7.4 is left, and it needs a Mac.

M7.3 landed as designed. A row grows a `▸` button whenever the wire says
`can_switch`; clicking it confirms — naming the account and the directory, and
saying plainly what switching cannot do (§1) — then POSTs
`/accounts/:id/session`, which answers `ok` and never a token. An account with
no `<id>#cli` grant shows `switch unavailable` with an *Enable* action into the
existing reconnect flow, except on a row that already asks for a reconnect,
where it would only say the same thing twice. Settings gained a **Session
switching** section owning both halves of the request body: `session.cwd` and
`session.terminal`, the latter edited one argument per line so a path with
spaces survives the round trip and is never re-split (§5). The connect modal
now says up front that there are two browser steps and narrates the
`setup_token` / `cli_token_captured` phases, which M7.2 left silent — the gap
that made the second consent screen read as a bug.

Verified: 191 workspace tests and 16 overlay tests (3 new, over
`settings.session`) green, `cargo fmt` and `cargo clippy` clean on both. The
route already had a test for the exact body the overlay now sends
(`a_session_launch_forwards_the_terminal_override_and_cwd`). The overlay JS ran
in a throwaway DOM harness — 25 checks over the row states, the confirmation
copy, the POST body (terminal argv, cwd, and no token or nonce in it), the
settings round trip and the new connect lines. **Not yet run against a live
daemon:** both accounts were lost before this session (STATUS.md), so no account
on this machine holds a `#cli` grant to switch to. The first reconnect will be
the real test.

M7.2 landed as designed. `cuw-connect` now drives **two** PTY runs off one
reusable driver (`run_cli`, borrowing the code channel so both steps can be fed
from the modal), announcing the second with a `setup_token` phase; the login is
scope-gated and validated *before* the second consent screen, so a wrongly
scoped login never costs the user a browser round trip. `cuw-creds` grew
`put_cli`/`get_cli`/`delete_cli` under `cli_key(id)`, with a versioned blob that
cannot cross-decode with a `Credential` in either direction. The daemon holds
the launch codes in `session.rs` (clock-injected, so the TTL and single-use rules
are tested without sleeping) and serves both routes; the wire gained
`can_switch`, and `DELETE /accounts/:id` now drops both grants.

Verified: 191 unit/route tests green, plus a live pass against a real daemon on
an isolated port — the `#cli` round trip through the real Windows Credential
Manager, the row seeding `can_switch` from the CLI grant alone, a real launch
whose minted code was then redeemed once (`no-store`, second read 404), and no
token prefix in the daemon log.

M7.1 is verified end to end against a stub `/session/:nonce` and a stand-in
`claude`: the shim reads the gate from `bearer.token`, sends it, parses the
token, clears a leftover `ANTHROPIC_API_KEY`, starts in the requested directory,
and a refused code prints a fixed line with no response body. `cargo test -p
cuw-launch -- --ignored` opens the real console (it must **stay** at a prompt —
that is the stdio check above).

---

## 8. Open questions

- **Q1 — ANSWERED: yes, the grants are independent.** §3 stands; M7.2 may proceed
  as designed. Measured 2026-08-31 against CLI 2.1.251, one real account, all
  four checks agreeing (§9).
- **Q2 — moot.** The shim removed the dependency on environment inheritance, so
  what `wt.exe` does with the environment no longer decides anything. `wt.exe`
  remains a `settings.session.terminal` prefix, not the default, for the
  hand-off reason in §5.
- **Q3 — partly answered.** `setup-token` says "long-lived (1-year) auth token"
  and prints "valid for 1 year", so ~1 y is confirmed. What the row shows 30 days
  out is still a UI decision.
- **Q4** — On macOS, does `claude` launched from the shim see the same `PATH` as
  an interactive shell?
- **Q5 — new, and it outranks the rest.** The login credential carries
  `refreshTokenExpiresAt` **28 days** out (`expiresAt` is only 8 h). If a refresh
  does not extend it, every account needs a manual reconnect monthly and the
  widget silently rots. `parse_refresh` does not read this field and
  `Credential` does not model it. **Test before M7.2:** refresh a disposable
  grant and check whether the response carries a new refresh-token expiry.

---

## 9. Q1: what was measured

Method: a throwaway `claude auth login --claudeai` in a scratch
`CLAUDE_CONFIG_DIR`, then `setup-token` in the same dir, comparing truncated
SHA-256 fingerprints (never tokens) via `cargo run -p cuw-daemon --example
q1_probe`. The daemon and overlay were stopped so no concurrent refresh could
confound the result; a second account was held as an untouched control.

| Check | Result |
|---|---|
| Does a new login displace an existing grant on the same account? | **No.** The daemon's credential stayed alive and unchanged — several OAuth grants coexist per account. |
| Does `setup-token` ask for its own consent? | **Yes** — its own browser `/oauth/authorize` and Authorize screen. Not derived from the login session. |
| Does `setup-token` alter the login credential? | **No.** `accessToken`, `refreshToken`, `expiresAt`, `refreshTokenExpiresAt` all byte-identical across the call. |
| Does rotating the login grant revoke the CLI token? | **No.** After three refreshes, `CLAUDE_CODE_OAUTH_TOKEN` alone in an empty config dir still ran `claude -p`. |
| Does using the CLI token break the login grant? | **No.** Both the rotated login credential and the control account stayed alive. |

Two incidental findings, both load-bearing elsewhere:

- The token endpoint **rotates the refresh token on every call** (three refreshes,
  three new tokens). The daemon's persist-after-refresh path is therefore not
  optional — a dropped write loses the account. This is what `persist_pending`
  already guards.
- Injecting `CLAUDE_CODE_OAUTH_TOKEN` into an otherwise empty `CLAUDE_CONFIG_DIR`
  really does bind the session to that account, which is the whole premise of
  approach C — now observed rather than assumed.
