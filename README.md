# claude-usage-widget

Always-on-top desktop widget showing live 5-hour and 7-day Claude usage for
several subscription accounts at once. Personal, read-only, Windows first
(macOS docking pending). Two processes: `cuw-daemon` owns the credentials,
polls, and serves localhost; the Tauri `overlay` renders percentages and never
sees a token.

See [plan.md](plan.md) for the design and milestones,
[IMPLEMENTATION.md](IMPLEMENTATION.md) for the build log, and
[CLAUDE.md](CLAUDE.md) for commands and conventions.

## What it does

- Shows every connected account side by side — the point is seeing the
  capacity of the account you are *not* using right now.
- Server-side 5h / 7d numbers (real quota), not local token estimates. Per-model
  weekly windows show as a collapsed line under the row.
- Connect an account from the widget: the daemon runs `claude auth login` in a
  scratch config dir it owns, reads the credential the CLI wrote there, stores
  it in the OS credential store, and scrubs the scratch dir. Your real Claude
  Code login is never touched.
- Refresh is the daemon's job: the access token is rotated before it expires;
  a dead refresh token shows as `reconnect needed` with the reason, never a
  wrong number.
- Optional docking to a chosen window on Windows (terminal, editor). Off by
  default; the undocked widget is the product.
- Tray icon, settings panel (opacity, compact mode, thresholds, per-account
  colours, click-through, autostart), window position remembered.

## Layout

```
crates/
  cuw-core/      usage model, endpoint client, token refresher, poll helpers
  cuw-creds/     credential store (keyring; Windows Credential Manager / Keychain)
  cuw-connect/   add-account flow (claude auth login in a PTY, credential file)
  cuw-daemon/    poll loop + localhost HTTP/SSE; the only token holder
  cuw-tracker/   window docking (Windows hooks / macOS AX), runs inside the overlay
apps/
  overlay/       Tauri v2 widget (src/ web UI, src-tauri/ shell)
```

## Requirements

- Rust stable.
- Claude Code CLI ≥ 2.1.251 on `PATH` (`claude auth login` is the sign-in).
- Tauri CLI: `cargo install tauri-cli` (2.11).
- Windows 10/11. macOS is untested; docking there is pending (plan §6).

## Quick start

```sh
cargo check                                  # backend crates
cargo test
cargo run -p cuw-daemon                      # stop a running daemon first
cd apps/overlay/src-tauri && cargo tauri dev # spawns the daemon if it is not up
```

The overlay is not a workspace member, so root `cargo check` skips Tauri.
Release build (daemon first, then the bundle), from the repo root:
`powershell -NoProfile -File scripts/build-release.ps1`.

## Where things live

| | |
|---|---|
| Daemon data | `%APPDATA%\local\cuw\data\` — `bearer.token`, `port`, `pid`, `registry.toml`, `daemon.log`, `scratch\` |
| Credentials | Windows Credential Manager, service `com.local.cuw`, one entry per account |
| Overlay settings | `%APPDATA%\com.local.cuw\settings.json` |

The daemon binds `127.0.0.1` only and requires the bearer from `bearer.token`
on every route. The log never contains a Claude token. The only plaintext
credential on disk is the CLI's `.credentials.json` inside `scratch\` while a
connect is running; the daemon overwrites and deletes it on every exit path
and sweeps `scratch\` at startup (plan §4).

## Status

M0–M3 built. M1b (token source + refresh) is the blocker for daily use; M4
docking and M6 polish are built and ship default-off. See the milestone table
in plan.md §7.

## Caveat

The usage endpoint is reverse-engineered, not a public interface. This is a
personal, read-only monitor for accounts you own: don't distribute it, don't
build anything load-bearing on it, don't poll it aggressively. If a public
`claude usage` command or Admin endpoint ships, migrate to it and delete the
undocumented path — the client sits behind a trait so that swap is one impl
(plan §9).
