# Scripts

## e2e-live.ps1

Drives the whole backend over localhost and prints a PASS/FAIL/SKIP table.
Exit code is non-zero on any FAIL.

```powershell
powershell -NoProfile -File scripts\e2e-live.ps1 -SkipLive   # no browser opens
powershell -NoProfile -File scripts\e2e-live.ps1             # one live connect
powershell -NoProfile -File scripts\e2e-live.ps1 -Reconnect  # + one reconnect
```

What it does: preflight (fmt/clippy/test/overlay check), daemon start (port +
pid file + clean scratch), auth (401 without bearer, 200 with, wire shape, no
`expires_at`), SSE first frame, optional live connect (POST while streaming
`/events`; records the phase names and whether `awaiting_code` appeared —
plan §8 Q7 — then deletes the e2e row), refresh field observation (never
forces a refresh), log redaction scan, graceful shutdown, and a self-check
that its own output contains no secret.

Only `cuw-daemon` is ever killed — never a `claude` process. It never touches
`%USERPROFILE%\.claude`, never reads a keyring value, and never prints the
bearer or any token.

The live connect opens a real browser sign-in and should be run sparingly (it
mints a credential each time). `%TEMP%\cuw-e2e\` holds the cargo/daemon logs.

## build-release.ps1

Builds the daemon in release mode first (the bundle maps it as a resource),
then `cargo tauri build` for the overlay, and prints the artefact paths.

## Manual overlay matrix

Run `cargo tauri dev` (or the release exe) and walk this by hand; none of it
is scriptable.

Undocked:

- Drag the widget by its body; Esc closes any open modal.
- Gear → change opacity/thresholds/colours → Save → restart: values persist
  (`settings.json`) and apply live.
- Tray: show/hide toggles; Alt+F4 hides (does not quit); Quit stops the
  daemon — also while a connect modal is open, leaving no `cuw-daemon` and no
  spawned `claude` behind.
- Click-through on via settings or tray: clicks pass through the widget; the
  connect modal still takes clicks and keys; click-through off via the tray.

Docked (Windows, docking picked from the tray or settings):

- Pick from the tray menu: the window that was active when the menu closed is
  NOT auto-picked; clicking Windows Terminal attaches and the badge shows
  `docked · CASCADIA_HOSTING_WINDOW_CLASS`.
- Move, resize, minimise, restore WT — the overlay follows; Win+D and back.
- Move WT to virtual desktop 2 and switch there: record whether the overlay
  is visible and docked (plan §8 Q13).
- Close WT → badge `detached — searching`; reopen WT → re-attached without a
  click. Start the overlay with WT closed, then open WT → attached.
- Alt-Tab: the overlay never appears — including after a tray hide/show.
- Type in WT while docked: focus is never stolen. Open the connect modal:
  typing works; closing it hands focus back to WT.
- Undock restores normal dragging and focusability.

Multi-monitor / mixed DPI: needs an external display — not verifiable on the
dev machine (plan §8 Q10).
