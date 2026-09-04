// Renders one row per account from the daemon. Read-only: the daemon holds
// Claude tokens; this only draws percentages and carries the localhost bearer.
//
// Transport: an authenticated SSE stream to /events (EventSource can't set an
// Authorization header, so we read the response body as a stream). If the stream
// drops we poll /accounts as a fallback and try to re-establish it (M3.3).

// The daemon's localhost base URL. The port is read from the daemon (via a Tauri
// command) once it has published it; 8787 is the fallback until then (M3.6).
let daemonPort = 8787;
function daemonBase() {
  return `http://127.0.0.1:${daemonPort}`;
}

const rows = document.getElementById("rows");
const status = document.getElementById("status");
const modalRoot = document.getElementById("modal-root");

// The localhost bearer (M3.2). Null until fetched, and when running outside the
// Tauri shell (a plain browser). Never a Claude token.
let bearer = null;

// Connect-flow modal state: while a flow is open we route "connect" SSE frames
// into its log.
let connectActive = false;
let connectLog = null;
// The code-paste section of the connect modal, revealed once the daemon signals
// the CLI is awaiting the authorization code.
let connectCodeBox = null;
let connectCodeInput = null;

let lastStart = 0;

// Window state that suppresses body dragging: a docked window is placed by the
// tracker, and a click-through one gets no mouse events to drag with. The
// settings panel and the `dock-state` event keep these current (M6.5).
let dockState = "undocked";
let dockTarget = null;
let clickThrough = false;

// The last accounts frame, kept so a settings change can re-render without
// waiting for the next SSE frame.
let lastAccounts = [];

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ±20 % so many clients (or many restarts) don't retry in lockstep.
const jitter = (ms) => ms * (0.8 + Math.random() * 0.4);

// Menu bar mode: no floating widget, the tray icon opens this view as a
// popover. There is nothing to dock or drag.
function menuBarMode() {
  return settings.mode !== "widget";
}

async function initBearer() {
  try {
    bearer = await invoke("bearer_token");
  } catch {
    bearer = null; // stay usable; requests will surface auth errors honestly
  }
}

async function initPort() {
  try {
    const p = await invoke("daemon_port");
    if (Number.isInteger(p) && p > 0) daemonPort = p;
  } catch {
    /* not in the shell — keep the fallback port */
  }
}

async function tryStartDaemon() {
  const now = Date.now();
  if (now - lastStart < 5000) return; // don't spam the spawn
  lastStart = now;
  try {
    await invoke("start_daemon");
  } catch {
    /* not in the shell, or spawn failed — the status line already tells the user */
  }
}

// --- Settings ---------------------------------------------------------------

let settings = defaultSettings();

function applySettings() {
  const o = Number.isFinite(settings.opacity) ? settings.opacity : 0.85;
  document.documentElement.style.setProperty("--bg-alpha", String(o));
  document.body.classList.toggle("compact", settings.compact === true);
  clickThrough = settings.click_through === true;
  document.body.classList.toggle("popover", menuBarMode());
  const dockBtn = document.getElementById("dock");
  if (dockBtn) dockBtn.hidden = !dockingSupported() || menuBarMode();
}

// --- HTTP -------------------------------------------------------------------

function authFetch(path, opts = {}) {
  const headers = new Headers(opts.headers || {});
  if (bearer) headers.set("Authorization", `Bearer ${bearer}`);
  return fetch(`${daemonBase()}${path}`, { ...opts, headers });
}

async function pollOnce() {
  try {
    const res = await authFetch("/accounts");
    if (res.status === 401) {
      bearer = null; // re-read on the next loop; the daemon may have just written it
      showStatus("authenticating…");
      return;
    }
    if (!res.ok) throw new Error(String(res.status));
    render(await res.json());
  } catch {
    showStatus("daemon offline — starting…");
  }
}

// --- SSE stream -------------------------------------------------------------

// `onFirstFrame` fires once the stream has actually delivered something, which
// is what clears the reconnect backoff (a 200 alone is not proof of life).
async function runStream(onFirstFrame) {
  const res = await authFetch("/events", {
    headers: { Accept: "text/event-stream" },
  });
  if (res.status === 401) {
    bearer = null; // re-read on the next loop; the daemon may have just written it
    showStatus("authenticating…");
    throw new Error("401");
  }
  if (!res.ok || !res.body) throw new Error(`stream ${res.status}`);

  clearStatus();
  const reader = res.body.getReader();
  const dec = new TextDecoder();
  let buf = "";
  let first = true;
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    buf += dec.decode(value, { stream: true });
    let idx;
    while ((idx = buf.indexOf("\n\n")) >= 0) {
      handleFrame(buf.slice(0, idx));
      buf = buf.slice(idx + 2);
      if (first) {
        first = false;
        if (onFirstFrame) onFirstFrame();
      }
    }
  }
}

function handleFrame(raw) {
  let event = "message";
  const dataLines = [];
  for (const line of raw.split(/\r?\n/)) {
    if (!line || line.startsWith(":")) continue; // blank or keep-alive comment
    if (line.startsWith("event:")) event = line.slice(6).trim();
    else if (line.startsWith("data:")) dataLines.push(line.slice(5).replace(/^ /, ""));
  }
  if (dataLines.length === 0) return;
  let obj;
  try {
    obj = JSON.parse(dataLines.join("\n"));
  } catch {
    return; // never render a half-parsed frame
  }
  if (event === "accounts") render(obj);
  else if (event === "connect") handleConnectPhase(obj);
}

async function streamLoop() {
  let delay = 1000; // 1 s → 30 s while the daemon stays down (M6.4)
  for (;;) {
    try {
      // The daemon writes its bearer/port on startup, so re-read them each pass
      // until we have them — a first run that auto-starts the daemon self-heals
      // instead of wedging on an auth error (M3.6).
      if (!bearer) await initBearer();
      await initPort();
      // Resolves when the stream ends cleanly.
      await runStream(() => {
        delay = 1000;
      });
    } catch {
      await tryStartDaemon(); // likely offline — bring it up if we can
    }
    await pollOnce(); // fallback refresh + honest offline status
    await sleep(jitter(delay)); // then re-establish the stream
    delay = Math.min(delay * 2, 30000);
  }
}

// --- Rendering --------------------------------------------------------------

function colorFor(id, index) {
  return paletteColor(settings, id, index);
}

function level(pct) {
  const t = settings.thresholds || {};
  const warn = Number.isFinite(t.warn) ? t.warn : 75;
  const crit = Number.isFinite(t.crit) ? t.crit : 90;
  if (pct >= crit) return "crit";
  if (pct >= warn) return "warn";
  return "";
}

function fmtPct(v) {
  return v === null ? "—" : `${v}%`;
}

function barNode(pct, title) {
  const wrap = document.createElement("div");
  wrap.className = `bar ${level(pct)}`.trim();
  if (title) wrap.title = title;
  const span = document.createElement("span");
  span.style.width = `${Math.max(0, Math.min(100, pct))}%`;
  wrap.appendChild(span);
  return wrap;
}

function resetTitle(a) {
  const r = a && a.resets_at;
  if (typeof r === "string") {
    const t = Date.parse(r);
    if (!Number.isNaN(t)) return `5h window resets ${new Date(t).toLocaleString()}`;
  }
  return null;
}

// "resets in 2h 14m" from an epoch-ms deadline.
function fmtCountdownMs(t) {
  const left = t - Date.now();
  if (left <= 0) return "resetting…";
  if (left < 3600000) return `${Math.floor(left / 60000)}m`;
  const m = Math.floor(left / 60000) % 60;
  if (left < 86400000) return `${Math.floor(left / 3600000)}h ${m}m`;
  const h = Math.floor(left / 3600000) % 24;
  return `${Math.floor(left / 86400000)}d ${h}h`;
}

function agoMs(t) {
  const m = Math.floor((Date.now() - t) / 60000);
  if (m < 1) return "just now";
  if (m < 60) return `${m}m ago`;
  return `${Math.floor(m / 60)}h ago`;
}

function ago(iso) {
  if (typeof iso !== "string") return "";
  const t = Date.parse(iso);
  return Number.isNaN(t) ? "" : agoMs(t);
}

// A countdown span the 30 s ticker rewrites in place — text only, no render().
function countdownSpan(iso, prefix) {
  const s = document.createElement("span");
  s.className = "cd";
  if (typeof iso === "string") {
    const t = Date.parse(iso);
    if (!Number.isNaN(t)) {
      s.dataset.until = String(t);
      s.dataset.prefix = prefix;
      s.textContent = `${prefix}${fmtCountdownMs(t)}`;
    }
  }
  return s;
}

function secondaryLine(cls, child) {
  const el = document.createElement("div");
  el.className = `secondary${cls ? ` ${cls}` : ""}`;
  if (typeof child === "string") el.textContent = child;
  else if (child) el.appendChild(child);
  return el;
}

function actionButton(text, action, id, label, cls) {
  const b = document.createElement("button");
  b.textContent = text;
  b.className = cls;
  b.dataset.action = action;
  b.dataset.id = id;
  b.dataset.label = label;
  return b;
}

// Per-model weekly windows (plan §8 Q3): one toggle line, entries collapsed
// until it is clicked. Malformed data renders nothing.
function appendScoped(row, scoped) {
  const entries = [];
  for (const s of Array.isArray(scoped) ? scoped : []) {
    if (!s || typeof s.name !== "string" || !Number.isFinite(s.pct)) continue;
    const line = document.createElement("div");
    line.className = `scoped entry${s.is_active === true ? " active" : ""}`;
    line.append(document.createTextNode(`${s.name} 7d ${s.pct}%`));
    const cd = countdownSpan(s.resets_at, " · resets in ");
    if (cd.dataset.until) line.appendChild(cd);
    entries.push(line);
  }
  if (!entries.length) return;
  const toggle = document.createElement("div");
  toggle.className = "scoped toggle";
  const arrow = () => (row.classList.contains("scoped-open") ? "▾" : "▸");
  toggle.textContent = `▸ ${entries.length} model limit${entries.length > 1 ? "s" : ""}`;
  toggle.addEventListener("click", () => {
    row.classList.toggle("scoped-open");
    toggle.textContent = `${arrow()} ${entries.length} model limit${entries.length > 1 ? "s" : ""}`;
  });
  row.appendChild(toggle);
  for (const e of entries) row.appendChild(e);
}

function rowNode(a, index) {
  const id = a && typeof a.id === "string" ? a.id : "";
  const label = a && typeof a.label === "string" ? a.label : "(unnamed)";
  const state = a && typeof a.state === "string" ? a.state : "unavailable";
  const refresh = a && typeof a.refresh === "string" ? a.refresh : "ok";

  const row = document.createElement("div");
  row.className = "row";
  row.style.setProperty("--bar", colorFor(id, index));
  if (a && a.stale === true) row.classList.add("stale");

  const head = document.createElement("div");
  head.className = "label";
  const name = document.createElement("span");
  name.className = "name";
  name.textContent = label;
  if (a && typeof a.access_expires_at === "string") {
    const t = Date.parse(a.access_expires_at);
    if (!Number.isNaN(t)) name.title = `access token until ${new Date(t).toLocaleString()}`;
  }
  const right = document.createElement("span");
  right.className = "right";
  const disc = actionButton("×", "disconnect", id, label, "icon");
  disc.title = "Disconnect";
  head.append(name, right);
  // Only an account holding a `<id>#cli` grant can be switched to (SWITCHER §6).
  if (a && a.can_switch === true) {
    const sw = actionButton("▸", "switch", id, label, "icon switch");
    sw.title = `Start a new session as ${label}`;
    head.appendChild(sw);
  }
  head.appendChild(disc);
  row.appendChild(head);

  if (state === "available") {
    const five = a && Number.isFinite(a.five_hour) ? a.five_hour : null;
    const seven = a && Number.isFinite(a.seven_day) ? a.seven_day : null;
    right.textContent =
      `5h ${fmtPct(five)} · 7d ${fmtPct(seven)}` + (refresh === "backoff" ? " · refreshing…" : "");
    if (five !== null) row.appendChild(barNode(five, resetTitle(a)));
    const fiveCd = countdownSpan(a && a.resets_at, "resets in ");
    if (fiveCd.dataset.until) row.appendChild(secondaryLine("", fiveCd));
    if (seven !== null) row.appendChild(barNode(seven, null));
    const sevenCd = countdownSpan(a && a.seven_day_resets_at, "resets in ");
    if (sevenCd.dataset.until) row.appendChild(secondaryLine("", sevenCd));
  } else {
    right.textContent =
      state === "reconnect needed" && refresh === "rejected"
        ? "reconnect needed · refresh rejected"
        : state;
    right.classList.add(state === "reconnect needed" ? "crit" : "muted");
    if (state === "reconnect needed") {
      row.appendChild(actionButton("Reconnect", "reconnect", id, label, "btn small"));
    }
  }

  if (a && a.stale === true) {
    const when = ago(a.fetched_at);
    row.appendChild(secondaryLine("stale-note", when ? `last update ${when}` : "last update unknown"));
  }
  if (a && a.persist_pending === true) {
    row.appendChild(secondaryLine("warn", "not saved — reconnect after restart"));
  }
  // No CLI grant is a display state, not an error (SWITCHER §6). A row already
  // asking for a reconnect says it once; this line would only repeat it.
  if (a && a.can_switch !== true && state !== "reconnect needed") {
    const note = secondaryLine("switch-note", "switch unavailable · ");
    note.appendChild(actionButton("Enable", "reconnect", id, label, "btn small"));
    row.appendChild(note);
  }
  if (settings.show_scoped === true) appendScoped(row, a && a.scoped);
  return row;
}

function render(accounts) {
  lastAccounts = Array.isArray(accounts) ? accounts : [];
  clearStatus();
  rows.replaceChildren();
  if (lastAccounts.length === 0) {
    const empty = document.createElement("div");
    empty.className = "empty";
    empty.textContent = "No accounts yet.";
    rows.appendChild(empty);
    return;
  }
  lastAccounts.forEach((a, i) => rows.appendChild(rowNode(a, i)));
  updateFreshness();
}

// Everything stale, or nothing fresh for 3 min while the stream is quiet: say
// so in the status line rather than looking alive (plan §3).
function updateFreshness() {
  if (!lastAccounts.length) return;
  // Only claim the line when it is empty or already ours; never clobber an
  // offline/auth message from the transport.
  const cur = status.textContent;
  if (cur !== "" && !cur.startsWith("last update")) return;
  let newest = NaN;
  let allStale = true;
  for (const a of lastAccounts) {
    if (!a || a.stale !== true) allStale = false;
    const t = a && typeof a.fetched_at === "string" ? Date.parse(a.fetched_at) : NaN;
    if (!Number.isNaN(t) && !(t <= newest)) newest = t;
  }
  const old = !Number.isNaN(newest) && Date.now() - newest > 180000;
  if (allStale || old) {
    showStatus(`last update ${Number.isNaN(newest) ? "unknown" : agoMs(newest)}`);
  } else if (cur.startsWith("last update")) {
    clearStatus();
  }
}

// One ticker rewrites countdown text nodes in place; render() never runs for it.
setInterval(() => {
  for (const s of document.querySelectorAll(".cd[data-until]")) {
    const t = Number(s.dataset.until);
    if (Number.isFinite(t)) s.textContent = `${s.dataset.prefix || ""}${fmtCountdownMs(t)}`;
  }
  updateFreshness();
}, 30000);

// --- Dock badge & button ----------------------------------------------------

// "win32:<class>|<exe>" keeps the class; "macos:<bundle id>" is already the
// whole name (geometry.rs).
function shortTarget(t) {
  if (typeof t !== "string") return "";
  if (t.startsWith("macos:")) return t.slice(6);
  const rest = t.startsWith("win32:") ? t.slice(6) : t;
  const bar = rest.indexOf("|");
  return bar >= 0 ? rest.slice(0, bar) : rest;
}

function setDock(payload) {
  dockState = payload && typeof payload.state === "string" ? payload.state : "undocked";
  dockTarget = payload && typeof payload.target === "string" ? payload.target : null;
  const badge = document.getElementById("dock-badge");
  const btn = document.getElementById("dock");
  if (badge) {
    badge.classList.toggle("detached", dockState === "detached");
    if (dockState === "docked") {
      badge.hidden = false;
      badge.textContent = `docked · ${shortTarget(dockTarget)}`;
    } else if (dockState === "detached") {
      badge.hidden = false;
      badge.textContent = "detached — searching";
    } else if (dockState === "picking") {
      badge.hidden = false;
      badge.textContent = "picking — click a window";
    } else {
      badge.hidden = true;
      badge.textContent = "";
    }
  }
  if (btn) {
    const docked = dockState === "docked" || dockState === "detached";
    btn.textContent = docked ? "⏏" : "⌖";
    btn.title = docked ? "Undock" : "Dock to a window";
  }
}

// --- Update check -----------------------------------------------------------

// Six hours: the widget stays open for days, and the check is one
// unauthenticated GitHub request that must not be worth rate-limiting.
const UPDATE_EVERY_MS = 6 * 60 * 60 * 1000;

// "idle" shows the offer, "busy" the progress bar, "failed" the fallback link.
let updateState = "idle";
let updateLatest = null;

// Rust answers with a value for every outcome — offline, rate-limited, no
// release published — so "no update" is the only failure the UI has.
async function checkUpdate() {
  if (updateState !== "idle") return;
  const btn = document.getElementById("update");
  if (!btn) return;
  let info = null;
  try {
    info = await invoke("check_update");
  } catch {
    return; // plain browser — leave the pill hidden
  }
  const ok = info && info.available === true && typeof info.latest === "string";
  updateLatest = ok ? info.latest : null;
  btn.hidden = !ok;
  btn.classList.remove("busy", "failed");
  btn.style.removeProperty("--p");
  btn.textContent = ok ? `Update v${info.latest}` : "";
  btn.title = ok ? "Download and install, then relaunch" : "";
}

// One click does the whole thing: download with a bar, install, relaunch.
// The process ends on success, so only the failure path ever comes back.
async function installUpdate() {
  const btn = document.getElementById("update");
  if (!btn || updateState === "busy") return;
  if (updateState === "failed") {
    invoke("open_release").catch(() => {});
    return;
  }
  updateState = "busy";
  btn.classList.add("busy");
  btn.classList.remove("failed");
  btn.title = "";
  renderUpdateProgress(btn, { phase: "download", downloaded: 0, total: null });
  try {
    await invoke("install_update");
  } catch (e) {
    updateState = "failed";
    btn.classList.remove("busy");
    btn.classList.add("failed");
    btn.style.removeProperty("--p");
    btn.textContent = "Update failed — open release page";
    btn.title = String(e && e.message ? e.message : e);
  }
}

function renderUpdateProgress(btn, p) {
  if (!p || typeof p.phase !== "string") return;
  if (p.phase === "download") {
    const total = Number(p.total);
    const done = Number(p.downloaded) || 0;
    if (Number.isFinite(total) && total > 0) {
      const pct = Math.max(0, Math.min(100, Math.round((done / total) * 100)));
      btn.style.setProperty("--p", `${pct}%`);
      btn.textContent = `Downloading ${pct}%`;
    } else {
      btn.style.setProperty("--p", "0%");
      btn.textContent = done > 0 ? `Downloading ${(done / 1048576).toFixed(1)} MB` : "Downloading…";
    }
  } else if (p.phase === "install") {
    btn.style.setProperty("--p", "100%");
    btn.textContent = `Installing v${updateLatest || ""}…`;
  } else if (p.phase === "restart") {
    btn.textContent = "Restarting…";
  }
}

// --- Modals -----------------------------------------------------------------

// The open modal's cancel path, for Escape. Null when nothing is open, and
// while a modal must not be dismissed (a connect flow in progress).
let modalCancel = null;

function closeModal() {
  modalRoot.replaceChildren();
  modalRoot.classList.remove("open");
  modalCancel = null;
  // Hand the window style back to settings/docking (M6.3).
  invoke("modal_interactive", { on: false }).catch(() => {});
}

function buildModal(title) {
  modalCancel = null;
  // A click-through or non-focusable window can't be clicked or typed into;
  // modals need both for as long as they are open (M6.3).
  invoke("modal_interactive", { on: true }).catch(() => {});
  modalRoot.replaceChildren();
  const overlay = document.createElement("div");
  overlay.className = "modal";
  const card = document.createElement("div");
  card.className = "card";
  const h = document.createElement("div");
  h.className = "card-title";
  h.textContent = title;
  card.appendChild(h);
  overlay.appendChild(card);
  modalRoot.appendChild(overlay);
  modalRoot.classList.add("open");
  return card;
}

function makeButton(text, kind) {
  const b = document.createElement("button");
  b.textContent = text;
  b.className = `btn${kind ? ` ${kind}` : ""}`;
  return b;
}

// The payload carries no identity (S1 Q4), so the label is always user-supplied.
// Validated here so a too-long label fails before the browser ever opens
// (plan §4: the route caps at 64).
function promptLabel(def = "") {
  return new Promise((resolve) => {
    const card = buildModal("Name this account");
    const input = document.createElement("input");
    input.type = "text";
    input.className = "input";
    input.value = def;
    input.placeholder = "e.g. Personal, Work";
    const err = document.createElement("div");
    err.className = "error";
    err.hidden = true;
    const actions = document.createElement("div");
    actions.className = "actions";
    const cancel = makeButton("Cancel", "");
    const ok = makeButton("Connect", "primary");
    actions.append(cancel, ok);
    card.append(input, err, actions);
    input.focus();

    ok.onclick = () => {
      const v = input.value.trim();
      if (!v) {
        err.textContent = "Enter a name.";
        err.hidden = false;
        return;
      }
      if (v.length > 64) {
        err.textContent = "64 characters max";
        err.hidden = false;
        return;
      }
      resolve(v); // leave the modal open; runConnect repurposes it
    };
    cancel.onclick = () => {
      closeModal();
      resolve(null);
    };
    modalCancel = () => cancel.click();
    input.onkeydown = (e) => {
      if (e.key === "Enter") ok.click();
    };
  });
}

function confirmModal(title, message, okLabel = "Disconnect", okKind = "danger") {
  return new Promise((resolve) => {
    const card = buildModal(title);
    const p = document.createElement("div");
    p.className = "msg";
    p.textContent = message;
    const actions = document.createElement("div");
    actions.className = "actions";
    const cancel = makeButton("Cancel", "");
    const ok = makeButton(okLabel, okKind);
    actions.append(cancel, ok);
    card.append(p, actions);
    ok.onclick = () => {
      closeModal();
      resolve(true);
    };
    cancel.onclick = () => {
      closeModal();
      resolve(false);
    };
    modalCancel = () => cancel.click();
  });
}

// One-button result modal. The status line is wiped by the next accounts frame,
// which can be a second away, so an outcome the user asked for is shown here.
function noticeModal(title, message) {
  return new Promise((resolve) => {
    const card = buildModal(title);
    const p = document.createElement("div");
    p.className = "msg";
    p.textContent = message;
    const actions = document.createElement("div");
    actions.className = "actions";
    const ok = makeButton("Close", "primary");
    actions.append(ok);
    card.append(p, actions);
    ok.onclick = () => {
      closeModal();
      resolve();
    };
    modalCancel = () => ok.click();
  });
}

function appendLog(line) {
  if (!connectLog) return;
  const el = document.createElement("div");
  el.className = "log-line";
  el.textContent = line;
  connectLog.appendChild(el);
  connectLog.scrollTop = connectLog.scrollHeight;
}

// Show the sign-in URL as a selectable link, in case the auto-opened browser
// didn't appear. The daemon opens the browser itself; this is the fallback.
function appendUrl(url) {
  if (!connectLog || typeof url !== "string") return;
  const wrap = document.createElement("div");
  wrap.className = "log-line";
  const a = document.createElement("a");
  // The href stays so the URL can be copied; navigation happens through the
  // shell, since the webview has nowhere to open a new window (M6.4).
  a.href = url;
  a.rel = "noopener";
  a.className = "signin-url";
  a.textContent = url;
  a.addEventListener("click", (e) => {
    e.preventDefault();
    invoke("open_url", { url }).catch(() => {});
  });
  wrap.append(document.createTextNode("Sign in: "), a);
  connectLog.appendChild(wrap);
  connectLog.scrollTop = connectLog.scrollHeight;
}

// Reveal the code-paste box and focus it once the CLI is waiting for the code.
function revealCodeBox() {
  if (!connectCodeBox) return;
  connectCodeBox.hidden = false;
  if (connectCodeInput) connectCodeInput.focus();
}

// Hide the code-paste box: no code is needed once the credential is captured,
// and it should never linger after the flow ends.
function hideCodeBox() {
  if (connectCodeBox) connectCodeBox.hidden = true;
}

// POST the pasted authorization code into the running connect flow.
async function postCode(code) {
  const trimmed = (code || "").trim();
  if (!trimmed) return;
  try {
    const res = await authFetch("/accounts/connect/code", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ code: trimmed }),
    });
    appendLog(res.ok ? "Code submitted." : `Could not submit code (${res.status}).`);
  } catch {
    appendLog("Could not submit code: daemon unreachable.");
  }
}

// Map a connect SSE frame to a log line. No frame carries a token; Output is
// already redacted by the daemon (plan §5).
function handleConnectPhase(obj) {
  if (!connectActive || !obj) return;
  switch (obj.phase) {
    case "started":
      appendLog("Launching claude auth login…");
      break;
    case "output":
      if (typeof obj.line === "string" && obj.line.length) appendLog(obj.line);
      break;
    case "url":
      appendUrl(obj.url);
      break;
    case "awaiting_code":
      appendLog("If the browser shows a code, paste it below; otherwise this finishes on its own.");
      revealCodeBox();
      break;
    case "token_captured":
      appendLog("Credential captured.");
      hideCodeBox(); // no code needed once we have the credential
      break;
    case "setup_token":
      appendLog("Step 2 of 2: authorizing a session token. Approve the new browser page too.");
      break;
    case "cli_token_captured":
      appendLog("Session token captured — switching is enabled for this account.");
      break;
    case "validated":
      appendLog(`Validated${obj.label ? ` ${obj.label}` : ""}.`);
      hideCodeBox();
      break;
    case "failed":
      appendLog(`Error: ${obj.message || "connect failed"}`);
      hideCodeBox();
      break;
    default:
      break;
  }
}

// The shared connect/reconnect modal: log, code-paste box, close button.
function openConnectModal(title) {
  const card = buildModal(title);
  const log = document.createElement("div");
  log.className = "log";
  connectLog = log;
  connectActive = true;

  // Code-paste box: hidden until the daemon signals `awaiting_code`.
  const codeBox = document.createElement("div");
  codeBox.className = "code-box";
  codeBox.hidden = true;
  const codeInput = document.createElement("input");
  codeInput.type = "text";
  codeInput.className = "input";
  codeInput.placeholder = "Paste the authorization code";
  const codeSubmit = makeButton("Submit code", "primary");
  const submit = () => {
    postCode(codeInput.value);
    codeInput.value = "";
  };
  codeSubmit.onclick = submit;
  codeInput.onkeydown = (e) => {
    if (e.key === "Enter") submit();
  };
  codeBox.append(codeInput, codeSubmit);
  connectCodeBox = codeBox;
  connectCodeInput = codeInput;

  const actions = document.createElement("div");
  actions.className = "actions";
  const close = makeButton("Close", "");
  actions.append(close);
  card.append(log, codeBox, actions);
  close.onclick = () => {
    connectActive = false;
    connectLog = null;
    connectCodeBox = null;
    connectCodeInput = null;
    closeModal();
  };
  // Escape must not tear the modal down mid-flow: the log is the only view of
  // a sign-in that is still running.
  modalCancel = () => {
    if (!connectActive) close.click();
  };

  // Two consent screens, not one (SWITCHER §3): the second authorizes the
  // token session switching uses, and reads as a bug unless it is announced.
  appendLog("This signs in twice: once for usage, once for session switching. Approve both.");
  return { close };
}

// POST /accounts and stream progress into the modal log.
async function runConnect(label) {
  const { close } = openConnectModal(`Connect ${label}`);
  try {
    const res = await authFetch("/accounts", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ label }),
    });
    if (!res.ok) {
      const detail = await res.text().catch(() => "");
      appendLog(`Failed: ${res.status}${detail ? ` ${detail}` : ""}`);
    } else {
      appendLog("Connected.");
      hideCodeBox();
      close.textContent = "Done";
    }
  } catch {
    appendLog("Failed: could not reach the daemon.");
  } finally {
    connectActive = false;
  }
}

// Re-run the sign-in for an existing row. The id is stable (M1b.7): the daemon
// replaces the credential in place, so nothing is deleted here.
async function runReconnect(label, id) {
  const { close } = openConnectModal(`Reconnect ${label}`);
  try {
    const res = await authFetch(`/accounts/${encodeURIComponent(id)}/reconnect`, {
      method: "POST",
    });
    if (res.ok) {
      appendLog("Reconnected.");
      hideCodeBox();
      close.textContent = "Done";
    } else if (res.status === 409) {
      appendLog("Another connect is already running.");
    } else if (res.status === 404) {
      appendLog("Account no longer exists.");
    } else {
      const detail = await res.text().catch(() => "");
      appendLog(`Failed: ${res.status}${detail ? ` ${detail}` : ""}`);
    }
  } catch {
    appendLog("Failed: could not reach the daemon.");
  } finally {
    connectActive = false;
  }
}

// --- Session switching ------------------------------------------------------

// One launch in flight per account. The row is rebuilt on every accounts frame,
// so a disabled button would not survive; the guard lives here instead.
const switching = new Set();

function sessionCwd() {
  const c = settings.session && settings.session.cwd;
  return typeof c === "string" && c.trim() ? c.trim() : "";
}

// The daemon defaults to the user's home when the overlay names no directory.
function sessionWhere() {
  const c = sessionCwd();
  return c ? `It starts in ${c}.` : "It starts in your home directory.";
}

// POST /accounts/:id/session. The answer is `ok` and never a token: the shim the
// daemon spawns fetches its own, so the overlay still sees none (SWITCHER §4).
async function startSession(id, label) {
  if (switching.has(id)) return;
  const ok = await confirmModal(
    "Start a session",
    `Open a terminal running claude as "${label}"? ${sessionWhere()}` +
      " Sessions already running keep the account they signed in with.",
    "Start",
    "primary",
  );
  if (!ok) return;
  switching.add(id);
  showStatus(`starting a session as ${label}…`);
  try {
    const body = {};
    const cwd = sessionCwd();
    if (cwd) body.cwd = cwd;
    const term = settings.session && settings.session.terminal;
    if (Array.isArray(term) && term.length) body.terminal = term;
    const res = await authFetch(`/accounts/${encodeURIComponent(id)}/session`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
    if (res.ok) {
      showStatus(`session started as ${label}`);
    } else if (res.status === 401) {
      bearer = null; // re-read on the next stream pass, as the poller does
      await noticeModal("Could not start a session", "Not authenticated yet — try again.");
    } else if (res.status === 409) {
      // The row already says `switch unavailable`; say why, and where the fix is.
      await noticeModal(
        "Switch unavailable",
        `"${label}" has no session token. Reconnect it to enable switching.`,
      );
    } else {
      const detail = await res.text().catch(() => "");
      await noticeModal(
        "Could not start a session",
        detail || `The daemon answered ${res.status}.`,
      );
    }
  } catch {
    await noticeModal("Could not start a session", "The daemon is unreachable.");
  } finally {
    switching.delete(id);
  }
}

// --- Wiring -----------------------------------------------------------------

// Drag the window by the widget body. WebView2 ignores -webkit-app-region, so
// this goes through the window API; a docked or click-through window is not
// ours to move (M6.4).
document.getElementById("widget").addEventListener("mousedown", (e) => {
  if (e.button !== 0) return;
  if (e.target.closest("button, input, a, .modal, textarea, select")) return;
  if (dockState === "docked" || clickThrough || menuBarMode()) return;
  const w = window.__TAURI__ && window.__TAURI__.window;
  if (w && w.getCurrentWindow) {
    w.getCurrentWindow()
      .startDragging()
      .catch(() => {});
  }
});

// One Escape handler for every modal: each sets its own cancel path, and a
// modal that must not be dismissed leaves it null. With no modal up, Escape
// closes the popover (a no-op in widget mode).
document.addEventListener("keydown", (e) => {
  if (e.key !== "Escape") return;
  if (modalRoot.classList.contains("open")) {
    if (modalCancel) modalCancel();
  } else {
    invoke("hide_popover").catch(() => {});
  }
});

document.getElementById("connect").addEventListener("click", async () => {
  const label = await promptLabel();
  if (label) await runConnect(label);
});

// Settings live in a window of their own (settings.js); Rust opens it.
document.getElementById("gear").addEventListener("click", () => {
  invoke("open_settings").catch(() => {});
});

document.getElementById("update").addEventListener("click", installUpdate);

document.getElementById("dock").addEventListener("click", () => {
  const docked = dockState === "docked" || dockState === "detached";
  invoke(docked ? "dock_stop" : "dock_pick").catch(() => {});
});

rows.addEventListener("click", async (e) => {
  const btn = e.target.closest("button[data-action]");
  if (!btn) return;
  const { action, id, label } = btn.dataset;
  if (!id) return;
  if (action === "disconnect") {
    const ok = await confirmModal(
      "Disconnect account",
      `Disconnect "${label}"? Its credential is removed from this machine.`,
    );
    if (ok) {
      try {
        await authFetch(`/accounts/${encodeURIComponent(id)}`, { method: "DELETE" });
      } catch {
        showStatus("could not reach the daemon");
      }
    }
  } else if (action === "reconnect") {
    const ok = await confirmModal(
      "Reconnect account",
      `Reconnect "${label}"? This re-runs the sign-in in your browser and replaces the stored credential for this account.`,
      "Reconnect",
      "primary",
    );
    if (ok) await runReconnect(label || "account", id);
  } else if (action === "switch") {
    await startSession(id, label || "account");
  }
});

// --- Status line ------------------------------------------------------------

function showStatus(msg) {
  status.textContent = msg;
}
function clearStatus() {
  status.textContent = "";
}

// --- Boot -------------------------------------------------------------------

(async function main() {
  await initPort();
  await initBearer();
  try {
    const s = await invoke("get_settings");
    if (s && typeof s === "object") settings = s;
  } catch {
    /* plain browser — defaults stay */
  }
  applySettings();

  listen("settings-changed", (e) => {
    if (!e || !e.payload || typeof e.payload !== "object") return;
    settings = e.payload;
    applySettings();
    render(lastAccounts);
  });
  // The settings window previews the opacity slider here while it is dragged;
  // the saved value arrives as settings-changed and wins.
  listen("preview-opacity", (e) => {
    const o = e && e.payload;
    if (Number.isFinite(o)) document.documentElement.style.setProperty("--bg-alpha", String(o));
  });
  listen("dock-state", (e) => setDock(e && e.payload));
  listen("update-progress", (e) => {
    const btn = document.getElementById("update");
    if (btn && updateState === "busy") renderUpdateProgress(btn, e && e.payload);
  });
  invoke("dock_state")
    .then(setDock)
    .catch(() => {});

  checkUpdate();
  setInterval(checkUpdate, UPDATE_EVERY_MS);

  streamLoop();
})();
