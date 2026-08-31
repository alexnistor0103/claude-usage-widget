# CLAUDE.md

Guidance for working in this repo: conventions, commands, and the rules that
the architecture depends on.

## What this is

A personal, read-only widget showing live Claude usage for several accounts at
once. Two processes: `cuw-daemon` (owns tokens, polls, refreshes, serves
localhost) and the Tauri `overlay` (renders percentages, never sees a token).

## Layout

- `crates/cuw-core` — usage model, endpoint client, token refresher, poll
  helpers. No I/O side effects beyond the clients, which sit behind the
  `UsageSource` and `TokenRefresher` traits.
- `crates/cuw-creds` — `CredentialStore` trait, keyring-backed by default.
- `crates/cuw-connect` — the connect flow (`claude auth login` in a PTY, reads
  the scratch credential file).
- `crates/cuw-daemon` — poll loop + localhost HTTP/SSE. The only token holder.
- `crates/cuw-tracker` — `WindowTracker` trait, per-platform docking impls;
  runs on a thread inside the overlay.
- `crates/cuw-launch` — `SessionLauncher` trait, the launch shims, per-platform
  spawn. Starts a terminal on a shim that redeems a nonce for a CLI token; the
  crate itself never holds one.
- `apps/overlay` — Tauri v2 shell. Detached from the workspace (own build).

## Commands

```sh
cargo check                       # the five backend crates
cargo test                        # unit + golden tests
cargo test -p cuw-tracker -- --ignored --test-threads=1   # live hook tests; spawn console
                                  # windows and fight over foreground focus — never in parallel
cargo test -p cuw-launch -- --ignored   # opens one real console; it must stay at a prompt
cargo clippy --all-targets
cargo fmt
cargo run -p cuw-daemon           # stop a running daemon first (it locks the exe)

cd apps/overlay/src-tauri
cargo check                       # the overlay, outside the workspace
cargo tauri dev                   # needs the Tauri CLI: cargo install tauri-cli
cd ../../..

# from the repo root
powershell -NoProfile -File scripts/e2e-live.ps1 -SkipLive      # daemon/routes/redaction, no browser
powershell -NoProfile -File scripts/build-release.ps1           # daemon first, then cargo tauri build
```

The overlay is not a workspace member, so `cargo check` at the root skips Tauri.
Stop a running daemon before building it:
`powershell -NoProfile -Command "Get-Process cuw-daemon -EA SilentlyContinue | Stop-Process -Force"`.

## Code comments

- Short. One line where it fits. Delete a comment before padding it.
- Say *why*, not *what* — the code already says what.
- No comment that restates the signature or an obvious line.
- No attribution, changelog, or author tags in comments.
- Never reference AI, models, assistants, or how the code was produced —
  not in comments, commit messages, or identifiers.

## Rust conventions

- Errors: `thiserror` in libraries, `anyhow` in binaries. No `unwrap`/`expect`
  on runtime paths; reserve them for provably-infallible setup.
- Keep the endpoint client behind `UsageSource` and the token endpoint behind
  `TokenRefresher` so an official API is a one-impl swap.
- Match the surrounding style. Run `cargo fmt` and keep clippy clean.

## Hard rules

- **Every unknown is a display state, not a panic.** `unavailable`,
  `reconnect needed`, `detached` are first-class from M0.
- **Never render stale data as fresh.** Non-200 / unexpected shape / parse
  failure → `unavailable`, never a wrong number; numbers held without a fresh
  200 carry `stale: true`.
- **Tokens never leave the daemon** — not to the overlay, not to logs. Redact
  on every path, including errors.
- **Do not poll faster than one request per account per minute.** Jitter, and
  back off hard on 429/5xx.
- Parse the usage response defensively — never `serde` into required fields.
- **Refresh discipline.** At most one token refresh per account per poll
  cycle; a rejected refresh (whitelisted OAuth error code) is terminal —
  `reconnect needed` and the poll task ends; any other 4xx is `Contract` and
  backs off; never log a token-endpoint response body.
- **Scratch dirs are daemon-owned.** The connect scratch dir lives under the
  data dir, is scrubbed on every exit path and swept at startup; never use
  `%TEMP%` for it. Never run `claude auth logout`.
- **Log errors with `%e`, never `?e`.** `keyring::Error::BadEncoding` carries
  the raw blob; `CredError`/`RefreshError` Debug output is not for logs.
- **Docking never steals focus and ships default-off.** The tracker callback
  does filter + read + send only; the dock lock is never held across a Tauri
  window call; raw-HWND work runs on the main thread; re-apply
  `WS_EX_TOOLWINDOW` after every show/hide/style call.

## Ordering

M1b (token source + refresh) is the blocker and goes first. M4 (docking) and
M6 (polish) may be built alongside it — separate crates and files — but
docking stays off by default until M3 + M1b has had a week of daily use.
Enabling it is a settings flip, not a build.
