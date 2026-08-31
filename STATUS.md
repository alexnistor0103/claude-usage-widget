# Status — token-source rebuild + docking + polish

> **2026-08-31, macOS session — read this first. It supersedes every block below.**
>
> **M5 (macOS tracker) and M7.4 (macOS launcher) are implemented, and the rest of
> the app was made to work on macOS.** 226 workspace tests (was 191) and 20
> overlay tests (was 16), zero failures; `cargo clippy --all-targets -- -D
> warnings` clean on both; the whole app — workspace *and* the detached overlay —
> type-checks for **both** `aarch64-apple-darwin` and `x86_64-apple-darwin`.
>
> **Nothing here has been compiled by an Apple toolchain, and nothing has run on
> a Mac.** "Type-checks" is a compiler pass with no linking; it is not evidence
> of behaviour. Treat every macOS runtime claim in this repo as unverified.
>
> **The review pass never ran.** This session was stopped after integration, so
> the three review agents and the documentation agent were cancelled. The code
> has had an integration pass (which found and fixed a real blocker, below) but
> **no adversarial review**.
>
> **plan.md, SWITCHER.md and IMPLEMENTATION.md are now STALE** — the docs agent
> was the cancelled one. Specifically wrong today: SWITCHER §7 still lists M7.4
> as "0.5 d (needs a Mac)" and says "Only M7.4 is left"; SWITCHER §8 Q4 (the
> macOS `PATH` question) is answered by the work but still marked open; plan §7's
> M5 row and §6's macOS subsection describe intentions, not what was built;
> IMPLEMENTATION's M5.1–M5.5 and M7.4 checkboxes are all still unticked. Fixing
> those documents is the first task for the next session.
>
> **How to verify macOS from a Windows box** (this is new and it is the whole
> reason the above was possible): `scripts/check-macos.ps1` — `cargo check` never
> links, so an Apple target needs only `rustup target add`. The one dependency
> that does not cross is `objc2-exception-helper`, whose build script compiles a
> `.m` file; `scripts/macos-check/objc2-exception-helper` is a never-linked stub
> patched in on the command line, leaving Cargo.toml and Cargo.lock untouched.
> Switches: `-Package`, `-OverlayOnly`, `-Clippy`, `-Target`, `-TargetDir`. The
> integration pass proved it really compiles the macOS code by planting
> deliberate type errors in all nine macOS-only files plus the macOS arms of
> `lib.rs`, `settings.rs` and `dock.rs`, and confirming each one failed at the
> planted line.
>
> **The data-loss hole from the M7.2 session is closed.** `CUW_DATA_DIR` replaces
> the `ProjectDirs` data dir and `CUW_KEYRING_SERVICE` replaces the
> `com.local.cuw` keyring service, so a test daemon touches nothing real; the
> listener now binds **before** any registry, keyring, scratch or poll work, so a
> second daemon exits instead of sharing state; `registry.toml` is written
> atomically; and `e2e-live.ps1` now stops the *overlay* too, which is what let it
> respawn a daemon the script could never own.
>
> **One design change worth knowing about.** `bundle.resources` is no longer
> declared in any checked-in Tauri config. `tauri_build` validates resource paths
> inside the overlay's *build script*, so the daemon entry made a plain `cargo
> check` of the overlay depend on a release artefact of the other workspace — it
> failed on a fresh clone, and only passed here because a stray zero-byte
> `target/release/cuw-daemon` was on disk. The daemon is now declared at bundle
> time by `scripts/build-release.{ps1,sh}`. **Cost:** a hand-run `cargo tauri
> build` produces a bundle with no daemon inside it and fails quietly at runtime.
> Use the release scripts.
>
> **Name and icons.** The app installs as **Claude Usage Widget** (`productName`;
> `mainBinaryName` stays `cuw-overlay` so the executable name, and the e2e
> script's overlay stop, are unaffected). `identifier` deliberately stays
> `com.local.cuw` — it keys both the keyring service and `ProjectDirs`, so
> changing it would orphan stored credentials and settings. Icons are a real set
> now (`icon.ico` 16–256, `icon.icns` ic07–ic14, the `Square*` set, and a
> black-on-alpha macOS menu-bar template the tray uses via `icon_as_template`);
> `assets/logo.svg` is the readable source and `assets/icons.py` regenerates them.
>
> **Next steps, needs a Mac (ranked):**
> 1. Does it build at all? Push the branch — `.github/workflows/ci.yml` already
>    has a `macos-14` job. This is free and settles far more than a borrowed
>    laptop would. Expect the first run to be informative rather than green: the
>    tracker's non-ignored macOS unit tests (four pump, two find, two ax, one
>    bounds, two style) have never executed anywhere.
> 2. `CGWindowListCopyWindowInfo(kCGWindowListOptionIncludingWindow, wid)`
>    returning exactly that window, and carrying `kCGWindowIsOnscreen` only while
>    visible, is load-bearing: `is_alive`, `describe`, `bounds::read` and the
>    Minimized-vs-Lost split all rest on it. There is a full-scan fallback, so
>    the worst case is slow rather than wrong — but check this first.
> 3. Terminal.app's handling of the launcher's `.command` wrapper: that `open -a
>    Terminal <file>.command` runs it in a login shell, and that Terminal tolerates
>    a file that deletes itself before `exec`.
> 4. `scripts/build-release.sh` has never run. Whether Tauri preserves the
>    daemon's executable bit copying it into `Contents/Resources` is read off the
>    source, not observed; if it does not, the app finds the daemon and fails to
>    spawn it.
> 5. The coordinate convention — tracker `Bounds` are physical pixels, top-left
>    origin of the primary display, matched to tao's `set_outer_position`. A sign
>    error puts the widget off-screen and no test catches it.
>
> **Next steps, this machine:**
> - Bring plan.md, SWITCHER.md and IMPLEMENTATION.md up to date (see above).
> - Run the review pass that was cancelled.
> - **One-click install is not done.** A default `cargo tauri build` targets the
>   host arch only, and GitHub's `macos-14` runners are Apple Silicon — so the dmg
>   CI would produce is arm64-only and will not open on an Intel Mac. Wanted:
>   `--target universal-apple-darwin` with the daemon `lipo`'d, a release job that
>   uploads the dmg as an artefact, and signing/notarisation that engages only
>   when the Apple secrets are present (unsigned otherwise). Without a Developer
>   ID the first launch costs one trip through System Settings → Privacy &
>   Security → Open Anyway; a stable signing identity is also what stops macOS
>   re-prompting for Keychain access after every rebuild (plan §5).
> - **First commit:** everything except README.md is still untracked (one commit
>   in the repo). After `git add`, run
>   `git update-index --chmod=+x scripts/e2e-live.sh scripts/build-release.sh scripts/check-macos.sh`
>   — `core.filemode` is false here, so the mode set on disk is not recorded.
>   Until then invoke them as `sh scripts/<name>.sh`.
>
> Q5 (the 28-day `refreshTokenExpiresAt`) is still untouched and is still the
> highest-value non-macOS item.

> **2026-08-31, M7.3 session — read this first.**
>
> **M7.3 is done**: the switch button, the `switch unavailable` row state, the
> confirmation, the `settings.session` section (start directory + terminal
> argv), and the connect modal finally saying there are two browser steps.
> Details in SWITCHER §7 and IMPLEMENTATION M7.3. 191 workspace + 16 overlay
> tests green; fmt and clippy clean.
>
> **It has not been exercised against a live daemon.** The accounts lost in the
> M7.2 session are still gone, so nothing here holds a `#cli` grant and every
> row would render `switch unavailable`. **Next step is a reconnect** — that
> one flow proves the two-consent connect copy, the `can_switch` wire field and
> the button in a single pass. Stop the *overlay* first, per the M7.2 note below.
>
> Q5 (the 28-day refresh-token expiry) was deliberately left alone this session.

> **2026-08-31, M7.2 session — read this first.**
>
> **M7.2 is done** (`setup-token` capture, keyring `<id>#cli`, both session
> routes). Details in SWITCHER §7 and IMPLEMENTATION M7.2. 191 tests green;
> fmt/clippy clean; a live pass ran against an isolated daemon on port 8799.
>
> **Both connected accounts were lost during this session and must be
> reconnected.** `registry.toml` is `accounts = []` and neither
> `personal-5cb2ab9c` nor `work-f9ca7144` remains in Windows Credential Manager.
> The credentials are gone from the OS store, so there is nothing to restore —
> each account needs a fresh browser login through the connect modal. (Doing so
> now also captures the new `<id>#cli` grant, so the switch button will work.)
>
> **What caused it is not established.** The only code path that removes both a
> registry entry and its keyring blob is `DELETE /accounts/:id`, and no DELETE
> was issued deliberately. What *is* certain from `daemon.log`: at 09:56:14 UTC
> both accounts seeded and polled fine, at 09:56:57 only `personal-5cb2ab9c`
> seeded, and `registry.toml` was rewritten empty at 09:57:01. In that window
> several daemons were alive at once — the installed release daemon under
> `%LOCALAPPDATA%\cuw-overlay` (respawned by the running overlay), and
> `scripts/e2e-live.ps1` starting its own via `cargo run`, which then failed to
> bind 8787. **Two daemons sharing one data dir and one keyring is the prime
> suspect and is worth fixing before the next live run** — the e2e script's
> `Stop-Daemon` kills `cuw-daemon` but not the overlay that immediately restarts
> it, so the script can never actually own the port while the widget is running.
>
> **Lesson for the next session:** stop the *overlay* (not just the daemon)
> before `scripts/e2e-live.ps1`, and note that `directories::ProjectDirs`
> resolves `%APPDATA%` through `SHGetKnownFolderPath`, so **setting the
> `APPDATA` environment variable does not isolate the data dir** — a "sandboxed"
> daemon started that way still reads and writes the real `registry.toml`.
> A `CUW_DATA_DIR` override would make live testing safe and is the obvious
> follow-up.

> **2026-08-31 (end of follow-up session): all coding items are done** — A1–A6, B1–B8, I1–I3,
> D1–D3. What remains needs a human or a Mac: ~~(1) one live login~~ **done, see below**,
> (2) the M4 manual docking matrix (`scripts/README.md`), (3) a week of daily use before
> flipping docking on, (4) M5 macOS.
>
> **Update, later the same day — the live unknowns are largely closed.** Two accounts are
> connected and polling; the credential-file shape, the `user:profile` scope gate and the
> **JSON** refresh encoding are all confirmed against live data (plan §8 resolved 8; method in
> SWITCHER §9). `refresh.rs` needs no change. M7.1 (`cuw-launch`) has landed and SWITCHER Q1 is
> answered: the `setup-token` grant is genuinely independent of the login's.
>
> **The one thing that got worse:** a login credential's `refreshTokenExpiresAt` is only
> **28 days** out and nothing in the code reads it (plan §8 Q14). If refreshing does not extend
> it, every account needs a manual reconnect monthly. Test this before building more on M7.

Written 2026-08-31 after stopping a multi-agent workflow mid-run (user request). Everything
below was verified directly against the code on disk (`cargo build --all-targets`, `cargo fmt
--check`, `cargo test`, `cargo clippy --all-targets` all clean at the moment this was written,
and each claim below was confirmed by reading the actual file, not by trusting an agent's report).

**Do not trust the workflow journal or agent self-reports for "done" status** — this run hit a
caching issue (see "Process notes" at the bottom) that caused items to be redundantly re-verified,
and some agent reports disagree with what the file on disk actually contains. Trust the code.

## Why this rebuild happened

The original connect flow shelled out to `claude setup-token`. That token has scope
`user:inference` only; the usage endpoint requires `user:profile` and returns 403. So the whole
token source was replaced with `claude auth login --claudeai` in a scratch `CLAUDE_CONFIG_DIR`,
which reads back a full OAuth credential (access + refresh token) from the CLI's own
`.credentials.json`, refreshed via the OAuth token endpoint. Full design is in `plan.md` §4–§8 and
`IMPLEMENTATION.md` M1b/M4/M6 — both already updated to reflect this design; read those before
touching architecture, per CLAUDE.md.

## Done and verified (code + tests, read in full — not just "review approved")

- **D1** — README.md and CLAUDE.md rewritten for the current architecture.
- **A1** (`cuw-core`) — `Credential` type, `TokenRefresher` trait, `OAuthTokenClient`,
  `parse_refresh`, error mapping. `crates/cuw-core/tests/refresh.rs` (23 tests, green).
- **A2** (`cuw-core`) — `ScopedWindow` / per-model weekly window parsing into `Usage.scoped`.
  `crates/cuw-core/tests/parse.rs` (13 tests, green).
- **A3** (`cuw-creds`) — `CredentialStore` now moves the whole `Credential` (access + refresh +
  expiry + scopes) as one JSON blob, not a bare access token. `Corrupt`/`TooLarge` handled.
- **A4** (`cuw-connect`) — connect flow now runs `claude auth login --claudeai` in a
  daemon-owned scratch dir (`data/scratch`, swept at startup), reads the credential file, gates on
  `user:profile` scope, validates once. 23 tests green in `cuw-connect`.
- **A5** (`cuw-daemon/poll.rs`, `state.rs`) — the refresh **state machine** is fully built and
  heavily tested: `apply_refresh`, `needs_refresh`, `refresh_backoff` (60 s floor),
  `sleep_crosses_stale`, `RefreshStatus`, `PollState.{force_refresh,just_refreshed,
  forced_for_429,persist_pending,...}`, `Row`/`WireAccount` carry `refresh`, `stale`,
  `access_expires_at`, `refreshed_at`, `persist_pending`. This is real, tested logic — but see A6
  below, **it is not wired into the running poll loop yet**.
- **B1** (`cuw-tracker`) — trait/event types + pure geometry module, headless-tested.
- **B2** (`cuw-tracker/windows`) — Win32 find/bounds/style helpers on the `windows` crate.
- **B3** (`cuw-tracker/windows/hook.rs`) — the SetWinEventHook message-pump thread, timers,
  pick/re-acquire logic. 28 tests (7 marked `ignored` because they open real windows — run with
  `cargo test -p cuw-tracker -- --ignored` to exercise them live).
- **B4** (`apps/overlay/src-tauri/settings.rs`) — `Settings`/`Thresholds`/`Dock` model,
  `settings.json` persistence, `get_settings`/`set_settings` Tauri commands (registered).
- **B5** (`apps/overlay/src-tauri/tray.rs` + `lib.rs`) — tray icon/menu, close-to-hide,
  click-through, `restyle()` (re-applies `WS_EX_TOOLWINDOW`), `modal_interactive`, autostart,
  `open_url` with a host allowlist.
- **B7** (`apps/overlay/src/main.js`) — Esc-closes-modal, SSE reconnect/backoff, `startDragging`,
  reconnect-row action, token-expiry countdown text. These look complete and correct as JS, but
  they render/react to dock state that nothing produces yet (see B6).

## NOT done — real gaps, verified by reading the code

- **A6 — DONE (2026-08-31, follow-up session).** `poll_loop` now runs the full refresh phase
  (`needs_refresh`/`apply_refresh` before each fetch, one refresh max per iteration, 60 s-floored
  backoff, stale downgrade before long sleeps, rotated-credential persist with retry), a rejected
  refresh or post-refresh 401 ends the task, `POST /shutdown` (Notify → graceful exit capped at
  3 s, pid file written/removed) and `POST /accounts/:id/reconnect` exist, connect runs as a
  tracked abortable task, labels are validated (empty/>64 chars → 400), and seeded poll tasks are
  staggered 2 s apart + jitter. 15 route/loop tests in `crates/cuw-daemon/tests/routes.rs`
  (paused-clock loop tests included); fmt/clippy/test clean. The live refresh check (spec step 7)
  was skipped at the time; it has since been done by hand — see "Live-verified" below.
- **B6 — DONE (2026-08-31, follow-up session).** `apps/overlay/src-tauri/src/dock.rs` now wires
  the tracker: `SharedDock`/`DockCtl` (lock never held across a Tauri window call; every raw-HWND
  call via `run_on_main_thread`; own hwnd cached once on the main thread), async
  `dock_start`/`dock_pick`/`dock_stop`/`dock_state` commands registered, consumer thread
  (drain-to-last-Bounds, Attached→settings write, Lost→show-then-Detached, NotFound→
  Detached-while-searching vs Undocked-on-pick-timeout), `place()` with scaled offsets + virtual-
  screen clamp, `replace_last` on `Resized`/placement changes, tray Dock/Undock items live, boot
  `ensure_started` when `dock.enabled` + remembered. Non-Windows gets an Err stub. check/clippy/
  fmt/test clean. **The manual matrix (spec step 3: pick WT, move/resize/minimise, Alt-Tab,
  virtual desktop, modal focus return) has NOT been run — needs a human at the machine.**
- **B8 — DONE (2026-08-31, follow-up session).** Header bar (dock badge + dock button + gear),
  full settings panel in `main.js` (`openSettings`): docking (enabled/corner/offset/inside/
  follow-focus/pick/remembered-readonly/allow-list), appearance (opacity live-preview, compact,
  show-scoped, warn/crit with inline validation, per-account colours), system (autostart,
  click-through). Save sends a patch of **touched paths only**; `refreshPanelFromSettings`
  live-updates untouched fields on `settings-changed`; `dock.remembered` is never sent. Dock
  badge/button driven by the `dock-state` event + `dock_state` on boot.
- **I1 — DONE (same session).** `expiryHint` removed; rows render 5h/7d countdowns (30 s ticker
  rewrites text nodes only), `stale` dims + "last update Xm ago", `refresh: backoff` → suffix
  "refreshing…", `rejected` → "reconnect needed · refresh rejected", `persist_pending` warning
  line, scoped weekly windows behind a collapsed toggle line, access-token tooltip, status-line
  freshness ("last update …" when all stale / >3 min old). All field reads defensive.
- **I2 — DONE (same session).** Reconnect is confirm-first (`POST /accounts/:id/reconnect`,
  stable id — the old DELETE-after-connect branch is gone), 409/404/other each get a readable
  log line, labels validated inline (empty / >64), connect log says "claude auth login…" /
  "Credential captured.", disconnect wording updated. Shared `openConnectModal` factors the
  connect/reconnect modal.
  **Verification:** `node --check` + a 32-assertion headless DOM smoke run (render states, dock
  badge, patch-building, panel refresh, label validation) — all green. **Not looked at in a real
  webview yet** — `cargo tauri dev` visual pass still pending, along with the B6 manual matrix.
- **I3 — DONE (same session).** `scripts/e2e-live.ps1` (PS 5.1, bearer never printed, output
  self-checked for leaks) + `scripts/README.md` with the manual matrix. Run with `-SkipLive`:
  **all PASS** (preflight, startup, auth/wire shape, SSE, redaction, shutdown ≤3 s with pid
  cleanup) — connect/refresh SKIP (needs the interactive browser login). Also ran the tracker's
  live hook tests: **all 7 pass with `--test-threads=1`** (parallel runs fight over foreground
  focus — CLAUDE.md command updated); a parallel run fails 3 of them, that's test interference,
  not a product bug.
- **D3 — DONE (same session).** `bundle.active=true`, NSIS target, map-form daemon resource,
  PerMonitorV2 manifest in `build.rs`; `scripts/build-release.ps1` ran clean: release exe +
  `cuw-overlay_0.1.0_x64-setup.exe` produced, and the release overlay was launched live — it
  spawned the **sibling release** `cuw-daemon.exe` (path verified via `Get-Process`), pid file
  written, then both stopped.
- **D2 — DONE (same session).** IMPLEMENTATION.md checkbox pass (76 boxes ticked; M5/macOS left
  open), snapshot table refreshed, M1b "Live findings" note added (Q8 has since been resolved
  live; see below), plan.md status line updated. Root `cargo test` + overlay
  `cargo check` green at the time of the pass (e2e preflight proves both).

## Live-verified 2026-08-31 (was: "open, unverified-live risk")

Superseded. Against CLI **2.1.251**, with the daemon and overlay stopped and a second account
held as a control (full method and fingerprint table in SWITCHER §9):

- **The credential-file shape is confirmed**, not assumed. `claudeAiOauth.{accessToken,
  refreshToken, expiresAt, scopes}` parse as written; `expiresAt` is milliseconds; a real login
  carries `user:profile` among five scopes, so the §4 gate passes.
- **The refresh endpoint accepts the JSON body.** Three live refreshes, all 200.
  `refresh.rs` needs no change — the `.form(...)` fallback can be forgotten.
- **The refresh token rotates on every call**, not sometimes. The persist-after-refresh path is
  therefore load-bearing: a dropped write loses the account, which is exactly what
  `persist_pending` guards.
- **Two accounts connect and poll correctly** through the daemon, so the connect flow works end
  to end. What is *still* unverified is only the `AwaitingCode` marker text for `auth login`
  (that run was interactive, so its output was not captured) — plan §8 open 7.

Still worth doing: watch one account cross its 8 h access-token expiry and confirm the row keeps
working instead of going to `reconnect needed`. The refresh machinery is now proven in isolation,
but not yet observed firing on its own schedule in the running daemon.

**New risk, higher than anything above:** `refreshTokenExpiresAt` is 28 days out and unmodelled
(plan §8 Q14). Nothing surfaces it, so the failure mode is a widget that works for a month and
then quietly asks every account to reconnect.

## Everything needed to resume

- `plan.md` and `IMPLEMENTATION.md` are current — read them before writing code.
- Full self-contained specs for every remaining item (A6, B6, B8, I1, I2, I3, D1–D3, and every
  already-done item for reference) live at:
  `C:\Users\alexn\AppData\Local\Temp\claude\C--Users-alexn-Others-Work-Personal-Projects-claude-usage-widget\a769d4b7-5f6a-4927-bcf1-80177042678f\scratchpad\items\*.md`
  — this path is under a session-specific temp dir and **will not survive** past this machine
  session in general, but is intact right now. If it's gone, the specs are also fully captured in
  `IMPLEMENTATION.md`'s M1b/M4/M6 sections, which were written from the same source.
- The stopped workflow's run id was `wf_c15a3766-bb4` — **cannot be resumed from a new chat**
  (`resumeFromRunId` is same-session only). A fresh chat needs a new workflow run; reuse the item
  specs above rather than re-deriving them.
- No stray processes: `cuw-daemon` is not running, no extra `claude` processes. Repo tree builds,
  formats, and tests clean as of this write-up.

## Process notes (read before launching another large workflow here)

This run hit **both a session usage limit and a weekly usage limit** and needed two manual
resumes. On resume, caching was far less effective than expected: many already-approved items
(D1, A1, A2, B1, B2, B4, B5...) were **redundantly re-implemented and re-reviewed multiple times**
across the three run attempts — this consumed the bulk of ~3.5M subagent tokens across the whole
effort and is almost certainly what caused the limits to be hit. The likely cause: each item's
prompt embeds its upstream dependencies' `notes_for_dependents` as literal text, and once one
edited item's phrasing changed on a retry, every downstream item's prompt changed too, cascading
cache misses through the whole dependency chain on every resume. A next attempt should keep
per-item work small and independent enough that a resume doesn't re-trigger this cascade, or
avoid rebuilding the whole plan as one giant resumable run.
