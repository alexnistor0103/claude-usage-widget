# Claude Usage Widget — Implementation Plan

A small always-on-top desktop widget that shows live 5-hour and 7-day usage for
several Claude subscription accounts at once, optionally docked to a chosen
window (terminal, editor, Claude desktop app). Windows and macOS.

Status: M0–M4 and M6 built and tested on Windows (the M4 manual matrix and
multi-monitor sign-off are pending). M1b landed and is now **live-verified**:
two accounts are connected and polling, and three live refreshes have run
(§8 Q8 resolved 2026-08-31). M5 (macOS) is not started. Docking ships
default-off until M3+M1b has had its week of daily use (§7). Session switching
is designed and started — [SWITCHER.md](SWITCHER.md), M7.1 landed.

---

## 1. Goals and non-goals

### Goals

- Show **all connected accounts simultaneously** — the point is seeing the
  capacity of the account you are *not* currently using.
- **Add an account from inside the widget**: a button that opens the system
  browser, runs a real sign-in, and comes back with a working token.
- Server-side numbers (actual quota), not local token estimates.
- Optional **docking** to a target window so the widget follows it around.
- One codebase, two platforms: Windows 10/11 and macOS 13+.

### Non-goals

- No inference. The widget never calls the Messages API, never spends quota.
- No *silent* account switching or automation. The widget can **start a new
  Claude Code session** as a chosen account on an explicit click
  ([SWITCHER.md](SWITCHER.md), M7) — that opens a terminal, nothing more. It
  never moves a running session to another account (impossible: a live session
  holds its credential in memory), never signs the user in or out, and never
  touches `~/.claude`. Reading usage remains the whole of the widget's own
  behaviour.
- No distribution. Personal build, ad-hoc signed. No Store, no notarisation
  beyond what is needed to run locally.
- No per-tab or per-pane tracking (see §6, it is not achievable).

---

## 2. Architecture

Two processes, deliberately decoupled, so a broken usage endpoint or a broken
docking layer can never take the other down.

```
┌─────────────────────────────────────────────┐
│ cuw-daemon  (Rust, headless, tray-less)     │
│                                             │
│  registry.toml ──┐                          │
│                  ├─ CredentialStore (trait) │
│                  │    ├── windows: CredMan  │
│                  │    └── macos: Keychain   │
│                  │                          │
│  poll loop ──────┼─ GET /api/oauth/usage    │
│                  │  per account, 60–120s    │
│                  │  jittered, backoff       │
│                  └─ TokenRefresher (trait)  │
│                     POST …/oauth/token      │
│                     before expiry / on 401  │
│                                             │
│  HTTP: 127.0.0.1:<port>                     │
│    GET  /accounts     → [{id,label,usage}]  │
│    POST /accounts     → start connect flow  │
│    POST /accounts/:id/reconnect             │
│    DELETE /accounts/:id                     │
│    GET  /events       → SSE, push updates   │
│    POST /shutdown                           │
└─────────────────────────────────────────────┘
                     ▲
                     │ localhost only, bearer token in a 0600 file
                     ▼
┌─────────────────────────────────────────────┐
│ cuw-overlay  (Tauri v2)                     │
│                                             │
│  UI: one row per account                    │
│  tray, settings.json, autostart             │
│  WindowTracker (trait, in-process thread)   │
│    ├── windows: SetWinEventHook + DWM       │
│    └── macos:   AX observer / CGWindowList  │
└─────────────────────────────────────────────┘
```

Tauri v2 for the shell: Rust core, both platforms, small binary, and it gives
direct access to the native window handle when the docking layer needs it.

The daemon is the only thing that touches credentials. The overlay never sees a
token — it talks to localhost and renders percentages.

### Repo layout

```
crates/
  cuw-core/        usage model, poller, endpoint client, no I/O side effects
  cuw-creds/       CredentialStore trait + windows/ + macos/ impls
  cuw-daemon/      binary: poll loop + local HTTP/SSE
  cuw-connect/     the "add account" flow (§4)
  cuw-tracker/     WindowTracker trait + windows/ + macos/ impls
apps/
  overlay/         Tauri v2 app (src-tauri/ + web UI)
```

---

## 3. Data source and its risk

There is no official API for subscription (Pro/Max) usage. The Admin API's
Usage & Cost endpoints cover Console *organization API* usage, which is a
different thing. A `claude usage` command is an open feature request.

What we use instead:

```
GET https://api.anthropic.com/api/oauth/usage
Authorization: Bearer <oauth access token>
```

This is undocumented and unsupported. It is what the community status-line
projects use, and it returns real server-side 5-hour and 7-day numbers. Because
subscription usage is shared across Claude Code, Claude.ai chat and Cowork, one
figure per account covers every surface — no need to track web chat separately.

**Rules for depending on it:**

- Treat the response as untyped. Parse defensively into an internal model;
  never `serde` straight into a struct with required fields.
- Any non-200, unexpected shape, or parse failure → that account's row shows
  `unavailable`, not a wrong number. Never render stale data as fresh.
- Version-gate: record which shape we saw last, log loudly on change.
- 60–120s poll with jitter per account. Back off hard on 429/5xx. This is one
  request per account per minute; do not be tempted to go faster.

**Fallback if the endpoint disappears:** parse `~/.claude/projects/**/*.jsonl`
for a local token/cost estimate (the `ccusage` approach) and label it clearly as
an estimate. It does not reflect server-side limits, so it answers "how much
have I burned" but not "do I have room". Ship it as a degraded mode only.

---

## 4. Connecting an account (the button)

This is the part that most shapes the design, so it gets decided first.

### Chosen approach: `claude auth login` in a scratch config dir, then refresh

The widget does **not** run its own sign-in. It shells out to the CLI, which
owns the OAuth flow and opens the browser itself, and then reads the credential
the CLI wrote into a scratch `CLAUDE_CONFIG_DIR` that only the widget knows
about. From that point on the widget owns the credential: it stores access +
refresh token in the OS store under its own service and refreshes the access
token itself before it expires.

Flow:

1. User clicks **+ Connect account** and types a label (the usage payload
   carries no identity, §8 Q4).
2. Daemon spawns `claude auth login --claudeai` in a PTY (`portable-pty`) with
   `CLAUDE_CONFIG_DIR` pointing at a fresh scratch dir **under the daemon's own
   data dir** (`data/scratch/cuw-connect-<uuid>`, never `%TEMP%`), so the
   user's real Claude Code login is never touched and a leftover is
   recognisable and daemon-owned. The child's environment is scrubbed of
   `CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN` and
   the Bedrock/Vertex switches first — `portable-pty` inherits the daemon's
   whole environment, and a pre-set token would make the CLI skip the login.
   The PTY driver answers the TUI's terminal queries, streams (redacted)
   output, and forwards a pasted code if the CLI asks for one.
3. The CLI opens the system browser. User signs in to whichever account they
   want — personal vs work is chosen there, with normal browser session
   behaviour.
4. The daemon polls the scratch dir for `.credentials.json` (250 ms). The CLI
   writes `{"claudeAiOauth":{accessToken, refreshToken, expiresAt (ms epoch),
   scopes[], …}}`; we parse it field by field, never trusting the shape.
5. The daemon checks the scopes include `user:profile` (else `Forbidden`, with
   the scope list in the message), validates the access token once against the
   usage endpoint, stores the credential, kills the CLI, and deletes the scratch
   dir. Nothing the CLI printed is kept.
6. Row appears and starts polling.

Scratch-dir lifecycle is a hard rule, not a `Drop`: the scratch dir holds a
live access **and** refresh token in plaintext while the flow runs. After the
kill the daemon waits (≤ 2 s) for the child to exit so it no longer holds file
handles, overwrites `.credentials.json` with `{}`, then removes the dir with
retries; a guard object does the same on every exit path including a panic or
an aborted task, and a failure is logged with the path only. On startup the
daemon sweeps `data/scratch/*` before seeding accounts. (The M1 driver leaned
on `tempfile`'s silent `Drop`, and four scratch dirs were found left behind
in `%TEMP%` after the M1 runs — with `auth login` those would have been
tokens on disk.)

Labels are capped at 64 characters at the route and the id slug at 32, so the
id always fits the Credential Manager's 512-byte username limit; a too-long
label fails *before* the browser opens, never after.

Refresh: the access token is short-lived (hours). Before it expires — and on
the first 401 — the daemon `POST`s the OAuth token endpoint
(`https://platform.claude.com/v1/oauth/token`, `grant_type=refresh_token`) and
rotates the stored credential. This lives behind a `TokenRefresher` trait with
the same rules as the usage endpoint (§3): undocumented, parsed defensively,
hard backoff on 429/5xx, and a rejected refresh is terminal → `reconnect
needed`, never retried. At most one refresh per account per poll cycle.

What "rejected" means is deliberately narrow. The request shape (JSON body,
no form encoding) is inferred, not verified, and a WAF or a contract mismatch
can answer 400/403 to a perfectly good refresh token. So on 400/401/403 the
client reads the body **only** to extract a whitelisted OAuth `error` code
(`invalid_grant`, `invalid_token`, `unauthorized_client` → `Rejected`);
anything else — another code, no body, unparseable — is
`RefreshError::Contract(status)`, which backs off like a 5xx and logs
`token endpoint contract changed` loudly. The body is never formatted or
logged; the extracted string is bounded to the whitelist. This keeps a
contract break from turning every account into a reconnect loop the user
cannot fix.

`reconnect needed` is terminal for the poll task: once a refresh is rejected
or a usage 401 follows a fresh refresh, the task writes the row, broadcasts,
and **returns**. Nothing polls or refreshes that account again until a
reconnect replaces the task. Refresh backoff has a 60 s floor and its own
attempt counter, so a transient outage never produces two token POSTs in one
minute and never wipes the usage-endpoint backoff. Seeded accounts start
staggered (`i × 2 s + 0..3 s`) so a restart after downtime is not N
simultaneous token POSTs.

**Honesty note on the client id.** The token endpoint needs a `client_id`, and
the only one that can refresh a token minted by the CLI's login is the CLI's
own public client id (it is visible in the sign-in URL the CLI prints). This
plan originally ruled that out: the preference was `claude setup-token`, whose
long-lived token never rotates and needs no client id. That path was overridden
by a hard fact found in S1: a `setup-token` token carries only `user:inference`
and `/api/oauth/usage` answers 403 `permission_error: OAuth token does not meet
scope requirement user:profile`. Only the interactive login mints
`user:profile`, and its token expires in hours, so a refresh path is not
optional. We use the id **only** for `refresh_token` grants on a credential the
CLI itself created; we never run an authorization-code flow or mint a token
from scratch. If a future CLI grants `user:profile` to `setup-token`, or a
public usage API ships, the refresher is a one-impl swap (§9).

Sharp edges, surfaced in the UI:

- Reconnecting an account re-runs the login; the previous credential for that
  row is replaced. `reconnect needed` rows get a one-click re-run that warns
  first.
- A dead refresh token (revoked, or the CLI's login for that browser session
  ended) shows as `reconnect needed` within one poll cycle, with the reason
  visible (`refresh: rejected`).
- macOS: the CLI may write to the Keychain instead of the config-dir file, in
  which case the file watcher finds nothing and the flow ends in
  `NoCredential`. This is the §5 constraint; macOS multi-account is the open
  problem it always was.

### Retired: `claude setup-token`

The M1 PTY driver was built against `setup-token` and everything in it except
the credential source carries over unchanged (query replies, URL parsing, code
forwarding, redaction, single-flight, teardown). Only the last step changed:
read a file instead of scraping a printed token. The output scrubber stays
fail-closed regardless — if the CLI ever prints a token, it is still redacted.

---

## 5. Credential storage

The daemon stores its own credentials. It never reads Claude Code's store: the
connect flow reads a file the CLI wrote into a scratch dir the daemon created,
then deletes that dir (§4).

| | Windows | macOS |
|---|---|---|
| Store | Windows Credential Manager generic credential (`keyring`, `windows-native`) | Keychain generic password, service `com.local.cuw`, one item per account |
| Crate | `keyring` | `keyring` (`apple-native`) |

`keyring` wraps both behind one API. Drop to native calls only if it gets in
the way.

### The credential blob

One entry per account, keyed `keyring::Entry::new("com.local.cuw", id)`. The
password is a single JSON string:

```json
{"v":1,"access_token":"sk-ant-oat01-…","refresh_token":"sk-ant-ort01-…",
 "expires_at":1756577403,"scopes":["user:inference","user:profile"]}
```

- `expires_at` is Unix seconds UTC; `v` exists so a future shape change is
  detected instead of misparsed. Unknown fields are ignored on read.
- The type is `cuw_core::Credential`; its `Debug` impl is hand-written and
  redacts both tokens, so `{:?}` on a live path cannot leak.
- Windows caps a credential blob at 2560 bytes **of UTF-16**, i.e. 1280 ASCII
  characters. Two tokens plus field names is ~400 characters; the store checks
  the limit before writing and fails with `CredError::TooLarge` rather than a
  backend error.
- An unparseable blob, a `v` other than `1`, or a backend `BadEncoding` (which
  in `keyring` carries the raw blob bytes — it must never reach a `{:?}`) is
  `CredError::Corrupt` → that row shows `reconnect needed` with
  `refresh: rejected`. `CredError` is logged with `%e` only, never `?e`. No
  migration from the earlier plain-string entries: the registry was empty
  when the shape changed.
- Refresh rotates the blob in place. If the write fails, the daemon keeps using
  the in-memory credential and retries the write next cycle; it never
  re-refreshes just because persistence failed (the old refresh token may
  already be revoked server-side). The failure is logged once per streak and
  surfaces on the wire as `persist_pending: true`, so the UI can warn that the
  account will need a reconnect after a restart instead of it being a
  surprise.

On macOS, write the item using the **native Security framework API from our own
signed binary** rather than shelling out to `/usr/bin/security`. This binds
access to our code signature and avoids the repeated-authorisation-prompt
failure mode that has bitten Claude Code itself.

### The one bounded exception to "tokens never leave the daemon"

Session switching (M7) needs a token to reach the terminal it starts, so
`GET /session/:nonce` returns one. The exception is deliberate and fenced
(SWITCHER §6):

- It serves a **different credential** — the `<id>#cli` entry from
  `claude setup-token`, scope `user:inference`. The `user:profile` credential
  this section describes is never returned by any route.
- The two are **independent grants**, verified live: rotating one does not
  revoke the other (SWITCHER §9). So handing out the CLI token cannot endanger
  the credential the widget itself runs on.
- The route is bearer-gated, the nonce is single-use with a 30 s TTL, and the
  token is never logged. The overlay still never sees a token: it POSTs and
  gets `ok`. The recipient is the shim, which puts the token in its own
  environment — never in a process argument.

Everything else in this plan still holds: no other route, log, event or error
path may carry a token.

### Why we avoid reading Claude Code's credentials

On Windows it would be easy: credentials live at
`%USERPROFILE%\.claude\.credentials.json` and follow `CLAUDE_CONFIG_DIR`, so two
profile directories give two cleanly separated logins.

On macOS it is not: credentials go into the Keychain under what appears to be a
**fixed service name** (`Claude Code-credentials`), and the docs do not extend
the `CLAUDE_CONFIG_DIR` clause to the Mac. Two accounts collide on one item.
Some community reports mention a config-dir-hashed service-name variant, which
suggests this has changed across versions — either way it is undocumented and
not a foundation to build on.

There is also a security dimension worth being explicit about: because that
Keychain item is readable by other user-context processes, security researchers
have flagged it as a soft spot. Building a background daemon that routinely
reads it would make the problem worse, not better. Owning our own credential is
both more robust and more defensible.

### Threat model for this app

- Tokens at rest: OS credential store, never plaintext on disk.
- Local HTTP: bind `127.0.0.1` only, require a bearer token read from a 0600
  file, so other local processes can't scrape the endpoint casually.
- Tokens never leave the daemon — not to the overlay, not to logs. Redact in
  all log output, including error paths.

---

## 6. Docking

The tracker runs **inside the overlay process** (it moves the overlay's own
window; the daemon stays headless and never links Win32 UI), as a library the
overlay drives. Docking is default-off and the undocked UI must stay fully
usable whatever the tracker does.

Abstraction (`crates/cuw-tracker`):

```rust
/// Physical pixels, virtual-screen origin. `scale` = target DPI / 96.
pub struct Bounds { x: i32, y: i32, w: i32, h: i32, scale: f64, approximate: bool }
/// "win32:<class>|<exe basename>" — never an HWND, which is not stable.
pub struct TargetId(pub String);
pub struct TargetSpec { class: String, exe: Option<String> }
pub struct TrackerConfig { allow: Vec<TargetSpec>, remembered: Option<TargetId>, follow_focus: bool }

pub enum TrackerEvent {
    Attached(TargetId),   // first attach or re-acquire; a Bounds/Minimized follows
    Bounds(Bounds),
    Minimized,
    Restored,
    Focused(bool),        // target is (not) the foreground window
    Lost,                 // target destroyed → "detached, searching"
    NotFound,             // attach/pick found nothing; tracker idle
}

pub trait TrackerHandle: Send {
    fn attach(&self, id: Option<TargetId>) -> anyhow::Result<()>; // None = best allowed candidate
    fn pick_interactively(&self) -> anyhow::Result<()>;
    fn detach(&self) -> anyhow::Result<()>;
    fn stop(self);
}
pub trait WindowTracker: Sized {
    type Handle: TrackerHandle;
    fn start(cfg: TrackerConfig) -> anyhow::Result<(Self::Handle, Receiver<TrackerEvent>)>;
}
```

Handle calls never do platform work on the caller's thread: they queue a
command and wake the tracker thread; results come back as events. Pure
placement math lives in `geometry.rs` (corner/offset → overlay origin, clamp
to work area, candidate matching and ranking, bounds coalescing) and is unit
tested headless on every platform.

`NotFound` has two meanings the consumer must keep apart: an attach that had
a spec (remembered or explicit) and found nothing leaves the tracker
**searching** (2 s timer + re-acquire on every foreground change) and the
overlay shows `detached`; only a pick timeout or an attach with no spec at
all leaves the tracker idle and the overlay `undocked`. A remembered
terminal that is not running yet at boot is the common case, not an error.

### Windows

Event-driven, no polling. `SetWinEventHook` with `WINEVENT_OUTOFCONTEXT` — no
DLL injection. Callbacks arrive only on the thread that installed the hook and
only while it pumps messages, and Tauri owns the main thread's loop, so the
tracker gets a **dedicated thread** (`cuw-tracker`) with its own
`GetMessageW`/`DispatchMessageW` pump, stopped with `PostThreadMessageW(WM_QUIT)`.
All Win32 calls happen on that thread; `HWND` crosses threads as `isize`.
Three pump details that are easy to get wrong: the thread has no message
queue until its first User32 queue call, so it runs `PeekMessageW(..,
PM_NOREMOVE)` **before** handing its thread id back, or the first
`PostThreadMessageW` from the handle fails with `ERROR_INVALID_THREAD_ID`;
`SetTimer(None, ..)` ignores the id you pass and returns a fresh one, so the
re-acquire and pick timers keep the returned ids and `WM_TIMER` is
dispatched on `wParam`; and the hook callback does **only** filter + bounds
read + channel send — re-acquire and pick resolution (`EnumWindows`,
`OpenProcess`, new hooks) are posted back to the pump as `WM_APP+2` and run
outside the callback.

- Hook **scoped to the target's pid and tid** for the
  `EVENT_SYSTEM_MOVESIZESTART..MINIMIZEEND` range and the
  `EVENT_OBJECT_DESTROY..LOCATIONCHANGE` range (filter by event id; add
  `EVENT_OBJECT_CLOAKED/UNCLOAKED` so a virtual-desktop switch reads as
  minimize/restore). Scoping is essential — `LOCATIONCHANGE` is very noisy
  globally. Filter `idObject == OBJID_WINDOW && idChild == CHILDID_SELF &&
  hwnd == target`; measured live, Windows Terminal then emits zero events while
  idle and exactly one per frame change.
- One **global** hook for `EVENT_SYSTEM_FOREGROUND` with
  `WINEVENT_SKIPOWNPROCESS`, to detect focus changes and to drive re-acquire
  and the interactive picker. A process-scoped hook cannot see this.
- Geometry from `DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS)`, not
  `GetWindowRect` — measured live, the latter is exactly 7 px off on left,
  right and bottom. Skip bounds while `IsIconic` (minimized windows report
  -32000). Fall back to `GetWindowRect` with `approximate: true` rather than
  emitting nothing. Read `GetDpiForWindow` for `scale`. Coalesce: drop a
  `Bounds` identical to the last one emitted; a drag delivers one per mouse
  move and the consumer drains to the latest before moving the overlay.
- Target death: `EVENT_OBJECT_DESTROY` on the target → `Lost`, unhook the
  target-scoped hooks, keep the pump and the foreground hook, and re-acquire
  from the remembered `TargetId` on each foreground change plus a 2 s timer.
- Finding a target: `EnumWindows`, keep visible, un-cloaked
  (`DWMWA_CLOAKED == 0` — hidden UWP hosts and other-desktop windows enumerate
  as visible), root (`GetAncestor(GA_ROOT) == hwnd`), non-`WS_EX_TOOLWINDOW`
  windows whose class matches the allow list. Generic classes
  (`Chrome_WidgetWin_1` is Chrome, WebView2 and every Electron app;
  `Window Class` is anything) need an exe basename too, so a spec is
  `{class, exe?}`. Rank: current foreground if it matches, else the remembered
  id, else first in Z order.
- Interactive pick: no mouse hook. Arm a 10 s window; the next
  `EVENT_SYSTEM_FOREGROUND` from a window outside our process, resolved to its
  root and checked for cloaking, becomes the target whether or not it is in the
  allow list — the user chose it. Two guards, because a pick is usually
  started from a tray menu: when the menu closes Windows re-activates the
  previously active window and fires `FOREGROUND` for it before the user
  clicks anything, so the pick records the foreground window at arm time and
  ignores it, ignores everything in the first 300 ms, and skips the shell's
  own roots (`Shell_TrayWnd`, `Progman`, `WorkerW`).
- Overlay side (Tauri): `set_focusable(false)` → `WS_EX_NOACTIVATE` (never
  steal focus — non-negotiable, it would break typing); `set_position` already
  uses `SWP_NOACTIVATE | SWP_NOZORDER`, so no raw `SetWindowPos` for moves.
  Tauri's `skipTaskbar` does not set `WS_EX_TOOLWINDOW` (the overlay still
  shows in Alt-Tab), so OR it into `GWL_EXSTYLE` through `window.hwnd()`.
  **tao rewrites the whole ex-style on every window-flag diff** — `show`,
  `hide`, `set_focusable`, `set_ignore_cursor_events`, `set_always_on_top`
  all go through `apply_diff`, which does `SetWindowLongW(GWL_EXSTYLE, ..)`
  from its own flags — so the hand-applied bit is gone after the first
  hide/show. One `restyle()` helper re-applies it and is called after every
  such call, on the main thread. Every raw-HWND call (`set_tool_window`,
  `assert_topmost`, `SetForegroundWindow`) runs via
  `app.run_on_main_thread`: `SetWindowPos(SWP_FRAMECHANGED)` on a window
  another thread owns is a synchronous `SendMessage` to that thread, and the
  consumer thread must never hold the dock lock while calling a Tauri window
  getter (`outer_size`, `hwnd`, `scale_factor` block on the main thread — a
  sync command holding the same lock on the main thread is a deadlock).
  `set_ignore_cursor_events(true)` is the click-through
  (`WS_EX_TRANSPARENT | WS_EX_LAYERED`). Hide on `Minimized`, show on
  `Restored`; re-assert topmost only on `Attached`/`Restored`, never per
  `Bounds`. Placement clamps to the virtual screen first and only falls back
  to the target monitor's work area when the overlay would be entirely off
  every monitor, so an outside anchor next to a maximised terminal may land
  on the adjacent monitor rather than over the terminal's edge.
- Modals need input whatever the window style says: the webview calls
  `modal_interactive(true)` when any modal opens — cursor events on,
  focusable, focused, restyled — and `modal_interactive(false)` when it
  closes, which re-derives the style from settings (click-through, docked)
  and, when docked, hands focus back to the target with
  `SetForegroundWindow` (allowed: we are the foreground process at that
  moment). Click-through alone would otherwise make the connect and settings
  modals unusable.
- DPI: Tauri's runtime already sets `PER_MONITOR_AWARE_V2` before any window
  exists, so DWM bounds arrive in physical pixels and no manifest change is
  needed. Multi-monitor mixed-DPI placement is covered by the pure geometry
  tests (scale 1.0/1.5/2.0, negative virtual-screen coordinates) because it
  cannot be checked on a single-monitor machine; sign-off needs a second
  display.

### macOS

- Overlay window: borderless, `level = .floating`, `collectionBehavior =
  [.canJoinAllSpaces, .fullScreenAuxiliary]`, `ignoresMouseEvents` for
  click-through. Configure via `NSWindow` on the Tauri handle.
- Focus changes: `NSWorkspace.didActivateApplicationNotification`. **No special
  permission required.**
- Position tracking, preferred: Accessibility API — `AXUIElementCreateApplication`
  + `AXObserver` on `kAXWindowMovedNotification` / `kAXWindowResizedNotification`.
  Event-driven and accurate, but **requires the user to grant Accessibility
  permission** in System Settings → Privacy & Security.
- Position tracking, fallback: poll `CGWindowListCopyWindowInfo` for the
  target's bounds at ~10Hz. No Accessibility grant needed. Slightly laggy on
  drag; acceptable.

Design consequence: macOS docking is **degraded by default** and upgrades when
permission is granted. The UI must handle this honestly — a one-line prompt
("Grant Accessibility for smoother docking"), never a hard blocker. Undocked
mode must be fully usable on both platforms; docking is a bonus, not the
product.

### What you can attach to

You attach to a top-level window handle, so: Windows Terminal, iTerm2,
Terminal.app, VS Code, the Claude desktop app — all fine, one handle each.

**Tabs and panes are not separate windows.** If personal Claude Code runs in tab
1 and work in tab 2 of the same terminal, no API on either platform will let you
dock a different widget to each. This is exactly why the widget shows all
accounts in one panel — the constraint and the design goal happen to agree.
Do not build tab detection.

Target selection: a "click to attach" picker, never a hardcoded process name.
Resolve to the root/top-level window, then remember it by app bundle ID or
executable + window class so it can be re-acquired after a restart. On `Lost`,
enter a `detached, searching` state rather than crashing.

---

## 7. Milestones

Each milestone is independently useful. Stop at any point and still have
something that works.

| | Milestone | Done when |
|---|---|---|
| **S1** | Spike: does a `setup-token` token work against `/api/oauth/usage`? | Answered: **no** (403, needs `user:profile`). Endpoint, headers and payload confirmed. Decides §4. |
| **M0** | `cuw-core`: endpoint client + defensive parser, driven by a hardcoded token | `cargo run` prints 5h/7d percentages and reset times |
| **M1** | `cuw-creds` + `cuw-connect`: connect flow end to end, headless | PTY driver works live against the CLI; the credential it ends with is replaced by M1b |
| **M1b** | Token source: `claude auth login` + credential file + refresh (`TokenRefresher`) | Two accounts connected from the widget; both rows `available`; a forced 401 refreshes and recovers without a reconnect; a rejected refresh shows `reconnect needed` with `refresh: rejected`; no token in any log, response or SSE frame; the daemon restarted after the access token expired refreshes before its first fetch |
| **M2** | `cuw-daemon`: poll loop, localhost HTTP + SSE, backoff, 401 handling | `curl localhost:PORT/accounts` returns both accounts, survives token expiry gracefully |
| **M3** | Overlay v1: Tauri window, always-on-top, **undocked**, connect/disconnect UI | Usable daily on both platforms. **This is the real ship point.** |
| **M4** | `cuw-tracker` Windows: hooks, DWM bounds, focus-follow | Overlay sticks to Windows Terminal across move, resize, minimise/restore, virtual-desktop switch and monitor change; closing the terminal → `detached`, reopening it → re-attached; a remembered terminal that starts *after* the overlay is attached without user action; overlay absent from Alt-Tab, including after a tray hide/show; typing in the terminal never loses focus and focus returns to it when a modal closes; geometry unit tests pass at scale 1.0/1.5/2.0; docking is off by default and undocked mode is untouched |
| **M5** | `cuw-tracker` macOS: CGWindowList fallback, then AX upgrade path | Widget sticks to iTerm2/Terminal.app; degrades cleanly without Accessibility |
| **M6** | Polish: tray, settings panel, autostart, per-account colours, thresholds, click-through, compact/opacity, stale/scoped display, drag/Esc fixes | Tray shows/hides and quits (stopping the daemon — also mid-connect, leaving no `cuw-daemon` and no `claude` we spawned); settings persist in `settings.json` and apply live, and the panel never clobbers a field Rust changed while it was open; click-through is reversible from the tray and modals stay usable under it; autostart registers only from a release build; stale numbers are visibly stale; reconnect warns before it runs; the widget drags by its body and Esc closes modals |

The original gate — do **not** start M4 before M3 has been in daily use for a
week — was written when M3 could be used. It could not: S1 failed, so no
account has ever stayed connected. Resolution: M1b is the blocker and goes
first; M4 and M6 may be **built** alongside it because they are separate crates
and files, but docking ships **off by default** and stays off until M3+M1b has
had its week of daily use. The docking layer is the fun part and the least
valuable part; it is where this project dies if the ordering slips.

---

## 8. Open questions

Resolved:

1. **S1** — **no.** `setup-token` mints `user:inference` only; the usage
   endpoint needs `user:profile` (403, confirmed live 2026-08-30). Token source
   is `claude auth login` + refresh (§4).
2. Response shape — captured as `crates/cuw-core/tests/fixtures/usage_ok.json`
   with a golden test. Field is `utilization`, `resets_at` is RFC3339.
3. Per-model weekly quotas — **yes**: `limits[]` entries with
   `kind: "weekly_scoped"` carry `scope.model.display_name`, `percent`,
   `resets_at`, `is_active`. Parsed into `Usage.scoped` and shown as a
   collapsed line per row (M6); the two primary bars stay the row.
4. Identity — **none** in the payload. The connect modal asks for a label.
5. macOS Keychain — still open, but it only affects macOS connect (§4 note);
   Windows never reads the Keychain.
6. Token TTL — no longer a one-year cliff. The access token expires in hours
   and is refreshed; the wire carries `access_expires_at`, `refreshed_at` and
   `refresh` (`ok | backoff | rejected`) so the UI can say why a row is
   reconnecting instead of counting down.
7. (was Q11) Tauri's style recompute **does** clear a hand-applied
   `WS_EX_TOOLWINDOW`: tao 0.35.3 `window_state.rs` `apply_diff` (line 315)
   rewrites `GWL_EXSTYLE` from its own flags (line 440) on any flag change,
   `VISIBLE` included. The overlay re-applies the bit after every
   show/hide/focusable/cursor-events/always-on-top call (§6); Alt-Tab after
   a tray hide/show is in the M4 manual matrix.

8. **Token endpoint — resolved live 2026-08-31** (three refreshes of a
   disposable grant, CLI 2.1.251). The **JSON body works**: `refresh.rs` needs
   no change and the form-encoded fallback is not required. The refresh token
   **rotates on every single call**, not sometimes — so the persist-after-
   refresh path is load-bearing, and `persist_pending` is the guard that
   matters. `expires_in` in seconds is consistent with the ~8 h expiry that
   came back. See SWITCHER §9 for the method.
   The same run confirmed the **credential file shape**:
   `claudeAiOauth.{accessToken, refreshToken, expiresAt, scopes}` parse exactly
   as assumed, `expiresAt` in milliseconds, and a real login carries
   `user:profile` among five scopes so the §4 gate passes. Three further fields
   are present and ignored by the parser: `subscriptionType`, `rateLimitTier`
   and `refreshTokenExpiresAt` — the last one is Q14.

Open:

7. Does `claude auth login` print a "paste code" prompt like `setup-token`
   does, or only the OSC-8 link? Still unconfirmed **for `auth login`** — the
   2026-08-31 run was driven interactively, not through the PTY, so its output
   was not captured. (`setup-token`'s output *was* captured and prints both a
   "Paste code here if prompted >" line and an OSC-8 `/oauth/authorize` link,
   which is what the current detector keys on.) Two accounts have since
   connected through the daemon and poll correctly, so the flow works
   end to end; only the marker text is unverified.
9. Does the CLI's login for a scratch `CLAUDE_CONFIG_DIR` get revoked when the
   user later logs out of their real Claude Code? Unknown; if so it surfaces as
   `refresh: rejected` → `reconnect needed`, which is the designed behaviour.
10. Multi-monitor / mixed-DPI docking is untested (single 96-DPI monitor
    here). The geometry is unit-tested at 1.5×/2× and negative coordinates;
    the DoD item needs a second display.
11. Resolved — see 7 above.
12. Does the usage endpoint answer 429 (not 401) to an expired or revoked
    access token, the way it does to no auth at all? If so, a dead token
    would sit in transient backoff forever, so the poll loop forces **one**
    refresh per outage after two consecutive 429s with no refresh since the
    last success. Bounded to one extra token POST; check the assumption when
    the first live refresh is observed.
13. Virtual desktops: the overlay is an ordinary per-desktop window and
    Tauri's `set_visible_on_all_workspaces` is a no-op on Windows; the only
    lever is `WS_EX_TOOLWINDOW` (tool windows show on every desktop —
    inferred from shell behaviour, not verified). If the manual-matrix line
    "move WT to desktop 2, switch there" fails, `IVirtualDesktopManager`
    pinning is the follow-up and docking stays default-off.
14. **`refreshTokenExpiresAt` is 28 days out** while the access token lasts 8 h
    (observed 2026-08-31). Nothing reads this field: `parse_refresh` ignores it
    and `Credential` does not model it. If a refresh does not extend the refresh
    token's own lifetime, every account silently needs a manual reconnect each
    month — the widget would look fine and then rot. **The highest-value unknown
    left.** Test by refreshing a disposable grant and checking whether the
    response carries a refresh-token expiry at all; if it does, model it and
    surface a "reconnect by <date>" hint well before the cliff.

---

## 9. Standing constraints

- The usage endpoint is reverse-engineered, not a public interface. This is a
  personal, read-only monitor for accounts you own. Don't distribute it, don't
  build anything load-bearing on it, don't poll it aggressively.
- If a public `claude usage` command or Admin endpoint ships, migrate to it
  immediately and delete the undocumented path. Keep `cuw-core`'s client behind
  a trait so that swap is a single impl.
- Every unknown is a display state, not a panic. `unavailable`,
  `reconnect needed`, `detached` are first-class UI states from M0 onward.
