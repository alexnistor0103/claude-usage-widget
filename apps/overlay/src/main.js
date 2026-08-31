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

function isWindows() {
  return navigator.userAgent.includes("Windows");
}

function isMac() {
  return navigator.userAgent.includes("Mac OS X");
}

// Docking exists on Windows and macOS only (plan §6); everywhere else the dock
// commands answer with an error and the UI must not offer them.
function dockingSupported() {
  return isWindows() || isMac();
}

// --- Tauri bridge (safe no-ops outside the shell) ---------------------------

function invoke(cmd, args) {
  const t = window.__TAURI__;
  if (t && t.core && typeof t.core.invoke === "function") {
    return t.core.invoke(cmd, args);
  }
  return Promise.reject(new Error("tauri unavailable"));
}

function listen(name, cb) {
  const t = window.__TAURI__;
  if (t && t.event && typeof t.event.listen === "function") {
    t.event.listen(name, cb).catch(() => {});
  }
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

// Mirror of the Rust defaults (settings.rs); used until get_settings answers
// and forever in a plain browser.
function defaultSettings() {
  return {
    version: 1,
    opacity: 0.85,
    compact: false,
    thresholds: { warn: 75, crit: 90 },
    autostart: false,
    click_through: false,
    always_on_top: true,
    show_scoped: true,
    colors: {},
    dock: {
      enabled: false,
      remembered: null,
      corner: "top_right",
      offset: { x: 8, y: 8 },
      inside: false,
      follow_focus: false,
      // Mirrors this platform's Rust default (settings.rs): window classes on
      // Windows, application bundle ids on macOS.
      allow: isMac()
        ? [
            { class: "com.apple.Terminal", exe: null },
            { class: "com.googlecode.iterm2", exe: null },
            { class: "com.microsoft.VSCode", exe: null },
            { class: "com.anthropic.claudefordesktop", exe: null },
          ]
        : [
            { class: "CASCADIA_HOSTING_WINDOW_CLASS", exe: null },
            { class: "ConsoleWindowClass", exe: null },
            { class: "org.wezfurlong.wezterm", exe: null },
            { class: "Alacritty", exe: null },
            { class: "Chrome_WidgetWin_1", exe: "Code.exe" },
            { class: "Chrome_WidgetWin_1", exe: "Hyper.exe" },
          ],
      show_accessibility_hint: true,
    },
    session: { terminal: [], cwd: "" },
  };
}

let settings = defaultSettings();

function applySettings() {
  const o = Number.isFinite(settings.opacity) ? settings.opacity : 0.85;
  document.documentElement.style.setProperty("--bg-alpha", String(o));
  document.body.classList.toggle("compact", settings.compact === true);
  clickThrough = settings.click_through === true;
  const dockBtn = document.getElementById("dock");
  if (dockBtn) dockBtn.hidden = !dockingSupported();
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

const palette = ["#6ea8fe", "#7ee0a0", "#f5c96b", "#c79bff", "#ff9b85"];

function colorFor(id, index) {
  const c = settings.colors && settings.colors[id];
  return typeof c === "string" ? c : palette[index % palette.length];
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

// The release page for the version last offered; null when nothing is offered.
let updateUrl = null;

// Rust answers with a value for every outcome — offline, rate-limited, no
// release published — so "no update" is the only failure the UI has.
async function checkUpdate() {
  const btn = document.getElementById("update");
  if (!btn) return;
  let info = null;
  try {
    info = await invoke("check_update");
  } catch {
    return; // plain browser — leave the pill hidden
  }
  const ok = info && info.available === true && typeof info.latest === "string";
  updateUrl = ok && typeof info.url === "string" ? info.url : null;
  btn.hidden = !ok;
  btn.textContent = ok ? `Update v${info.latest}` : "";
  btn.title = ok ? "Open the release page" : "";
}

// --- Modals -----------------------------------------------------------------

// The open modal's cancel path, for Escape. Null when nothing is open, and
// while a modal must not be dismissed (a connect flow in progress).
let modalCancel = null;

// Settings-panel liveness: a new modal displaces the panel, so buildModal
// resets these.
let panelOpen = false;
let panelRefs = null;

function closeModal() {
  modalRoot.replaceChildren();
  modalRoot.classList.remove("open");
  modalCancel = null;
  panelOpen = false;
  panelRefs = null;
  // Hand the window style back to settings/docking (M6.3).
  invoke("modal_interactive", { on: false }).catch(() => {});
}

function buildModal(title) {
  modalCancel = null;
  panelOpen = false;
  panelRefs = null;
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

// --- Settings panel ---------------------------------------------------------

function fieldRow(labelText, control, noteText) {
  const f = document.createElement("div");
  f.className = "field";
  const l = document.createElement("span");
  l.className = "flabel";
  l.textContent = labelText;
  f.append(l, control);
  if (noteText) {
    const n = document.createElement("span");
    n.className = "note";
    n.textContent = noteText;
    f.appendChild(n);
  }
  return f;
}

function checkboxInput(checked) {
  const c = document.createElement("input");
  c.type = "checkbox";
  c.checked = checked === true;
  return c;
}

function numberInput(value, min, max) {
  const n = document.createElement("input");
  n.type = "number";
  n.min = String(min);
  n.max = String(max);
  n.value = Number.isFinite(value) ? String(value) : "";
  return n;
}

function allowText(allow) {
  return (Array.isArray(allow) ? allow : [])
    .filter((a) => a && typeof a.class === "string")
    .map((a) => (a.exe ? `${a.class}|${a.exe}` : a.class))
    .join("\n");
}

function parseAllow(text) {
  const out = [];
  for (const raw of String(text).split("\n")) {
    const line = raw.trim();
    if (!line) continue;
    const bar = line.indexOf("|");
    const cls = (bar >= 0 ? line.slice(0, bar) : line).trim();
    const exe = bar >= 0 ? line.slice(bar + 1).trim() : "";
    if (cls) out.push({ class: cls, exe: exe || null });
  }
  return out;
}

// A terminal override is argv, one argument per line, never re-split (SWITCHER
// §5) — so a path with spaces survives the round trip through the box.
function argvText(argv) {
  return (Array.isArray(argv) ? argv : []).filter((a) => typeof a === "string").join("\n");
}

// macOS docking runs on a permission-free poll and only gets smoother when
// Accessibility is granted (plan §6), so this is one honest line the user can
// dismiss for good — never a blocker, and never shown where there is no such
// permission (Rust answers `applicable: false` there). The container is
// returned empty and filled once the command answers.
function accessibilityHint() {
  const box = document.createElement("div");
  if (settings.dock && settings.dock.show_accessibility_hint === false) return box;
  invoke("dock_accessibility")
    .then((a) => {
      if (!a || a.applicable !== true || a.trusted === true) return;
      const line = document.createElement("div");
      line.className = "note block";
      line.textContent = "Docking works now. Grant Accessibility for smoother tracking.";
      const grant = makeButton("Grant…", "");
      grant.onclick = () => {
        // The system dialog, not a System Settings link: open_url's allowlist
        // is https-only and stays that way.
        invoke("dock_grant_accessibility").catch(() => {});
      };
      const dismiss = makeButton("Dismiss", "");
      dismiss.onclick = () => {
        box.replaceChildren();
        // Persisted on the spot rather than on Save: Save only sends the
        // fields the user edited, and this is not one of them.
        invoke("set_settings", {
          patch: { dock: { show_accessibility_hint: false } },
        }).catch(() => {});
      };
      const row = document.createElement("div");
      row.className = "field";
      row.append(grant, dismiss);
      box.append(line, row);
    })
    .catch(() => {});
  return box;
}

function parseArgv(text) {
  return String(text)
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean);
}

function openSettings() {
  if (panelOpen) return;
  const card = buildModal("Settings");
  card.classList.add("settings");
  panelOpen = true;

  // Paths the user actually edited; Save sends only these, so a Rust-side
  // change while the panel is open (an Attached event, a tray toggle) is never
  // reverted by an untouched field (M6.1).
  const touched = new Set();
  const refs = { touched, colorInputs: new Map() };
  const track = (el, path) => {
    const mark = () => touched.add(path);
    el.addEventListener("input", mark);
    el.addEventListener("change", mark);
  };
  const section = (title) => {
    const h = document.createElement("div");
    h.className = "section-title";
    h.textContent = title;
    card.appendChild(h);
  };

  // -- Docking (Windows and macOS; the tracker exists nowhere else) --
  if (dockingSupported()) {
    section("Docking");
    card.appendChild(accessibilityHint());
    refs.dockEnabled = checkboxInput(settings.dock && settings.dock.enabled);
    track(refs.dockEnabled, "dock.enabled");
    card.appendChild(fieldRow("Enabled", refs.dockEnabled));

    refs.corner = document.createElement("select");
    for (const [v, t] of [
      ["top_left", "Top left"],
      ["top_right", "Top right"],
      ["bottom_left", "Bottom left"],
      ["bottom_right", "Bottom right"],
    ]) {
      const o = document.createElement("option");
      o.value = v;
      o.textContent = t;
      refs.corner.appendChild(o);
    }
    refs.corner.value = (settings.dock && settings.dock.corner) || "top_right";
    track(refs.corner, "dock.corner");
    card.appendChild(fieldRow("Corner", refs.corner));

    const off = (settings.dock && settings.dock.offset) || { x: 8, y: 8 };
    refs.offx = numberInput(off.x, -500, 500);
    refs.offy = numberInput(off.y, -500, 500);
    track(refs.offx, "dock.offset");
    track(refs.offy, "dock.offset");
    const offWrap = document.createElement("span");
    offWrap.append(refs.offx, refs.offy);
    offWrap.style.display = "flex";
    offWrap.style.gap = "4px";
    card.appendChild(fieldRow("Offset x/y", offWrap));

    refs.inside = checkboxInput(settings.dock && settings.dock.inside);
    track(refs.inside, "dock.inside");
    card.appendChild(fieldRow("Inside the window", refs.inside));

    refs.follow = checkboxInput(settings.dock && settings.dock.follow_focus);
    track(refs.follow, "dock.follow_focus");
    card.appendChild(fieldRow("Hide when target unfocused", refs.follow));

    const pick = makeButton("Pick window…", "");
    pick.onclick = () => {
      // The guards against picking our own window live in Rust (plan §6).
      closeModal();
      invoke("dock_pick").catch(() => {});
    };
    card.appendChild(fieldRow("Target", pick));

    refs.remembered = document.createElement("span");
    refs.remembered.className = "readonly";
    refs.remembered.textContent = (settings.dock && settings.dock.remembered) || "(none)";
    card.appendChild(fieldRow("Remembered", refs.remembered));

    refs.allow = document.createElement("textarea");
    refs.allow.value = allowText(settings.dock && settings.dock.allow);
    refs.allow.spellcheck = false;
    track(refs.allow, "dock.allow");
    const allowField = document.createElement("div");
    allowField.className = "field";
    const allowLabel = document.createElement("span");
    allowLabel.className = "flabel";
    allowLabel.textContent = isMac()
      ? "Allowed apps (bundle id per line)"
      : "Allowed windows (class|exe per line)";
    allowField.append(allowLabel);
    card.append(allowField, refs.allow);
  }

  // -- Appearance --
  section("Appearance");
  refs.opacity = document.createElement("input");
  refs.opacity.type = "range";
  refs.opacity.min = "0.2";
  refs.opacity.max = "1";
  refs.opacity.step = "0.01";
  refs.opacity.value = String(Number.isFinite(settings.opacity) ? settings.opacity : 0.85);
  track(refs.opacity, "opacity");
  refs.opacity.addEventListener("input", () => {
    // Live preview only; Cancel reverts via applySettings().
    document.documentElement.style.setProperty("--bg-alpha", refs.opacity.value);
  });
  card.appendChild(fieldRow("Opacity", refs.opacity));

  refs.compact = checkboxInput(settings.compact);
  track(refs.compact, "compact");
  card.appendChild(fieldRow("Compact (labels only)", refs.compact));

  refs.showScoped = checkboxInput(settings.show_scoped);
  track(refs.showScoped, "show_scoped");
  card.appendChild(fieldRow("Show per-model limits", refs.showScoped));

  const th = settings.thresholds || { warn: 75, crit: 90 };
  refs.warn = numberInput(th.warn, 1, 100);
  refs.crit = numberInput(th.crit, 1, 100);
  track(refs.warn, "thresholds");
  track(refs.crit, "thresholds");
  const thWrap = document.createElement("span");
  thWrap.append(refs.warn, refs.crit);
  thWrap.style.display = "flex";
  thWrap.style.gap = "4px";
  card.appendChild(fieldRow("Warn / crit %", thWrap));

  lastAccounts.forEach((a, i) => {
    if (!a || typeof a.id !== "string") return;
    const c = document.createElement("input");
    c.type = "color";
    c.value = colorFor(a.id, i);
    track(c, "colors");
    refs.colorInputs.set(a.id, c);
    card.appendChild(fieldRow(typeof a.label === "string" ? a.label : a.id, c));
  });

  // -- Session switching --
  section("Session switching");
  const sess = settings.session || {};
  refs.sessionCwd = document.createElement("input");
  refs.sessionCwd.type = "text";
  refs.sessionCwd.spellcheck = false;
  refs.sessionCwd.placeholder = "(your home directory)";
  refs.sessionCwd.value = typeof sess.cwd === "string" ? sess.cwd : "";
  track(refs.sessionCwd, "session.cwd");
  card.appendChild(fieldRow("Start directory", refs.sessionCwd));

  refs.sessionTerminal = document.createElement("textarea");
  refs.sessionTerminal.value = argvText(sess.terminal);
  refs.sessionTerminal.spellcheck = false;
  track(refs.sessionTerminal, "session.terminal");
  const termField = document.createElement("div");
  termField.className = "field";
  const termLabel = document.createElement("span");
  termLabel.className = "flabel";
  termLabel.textContent = "Terminal command (one argument per line)";
  termField.append(termLabel);
  const termNote = document.createElement("div");
  termNote.className = "note block";
  // The fold rule diverges by platform on purpose (cuw-launch plan.rs): on
  // Windows an override is a prefix unless it names {shim}; on macOS the
  // default is `open -a Terminal <wrapper>`, so prefixing it is nonsense and a
  // plain override is a launcher the wrapper path is appended to instead.
  termNote.textContent = isMac()
    ? "Blank uses Terminal.app. {wrapper} {shim} {nonce} {port} {cwd} substitute; a command naming any of them is used as written, anything else is a launcher the wrapper is appended to (open -a iTerm)."
    : "Blank uses the default terminal. {shim} {nonce} {port} {cwd} substitute; a command containing {shim} replaces the default, anything else prefixes it.";
  card.append(termField, refs.sessionTerminal, termNote);

  // -- System --
  section("System");
  refs.autostart = checkboxInput(settings.autostart);
  track(refs.autostart, "autostart");
  card.appendChild(
    fieldRow("Start at login", refs.autostart, "A dev build stores the flag without registering."),
  );

  refs.clickThrough = checkboxInput(settings.click_through);
  track(refs.clickThrough, "click_through");
  card.appendChild(
    fieldRow("Click-through", refs.clickThrough, "Turn it off again from the tray icon."),
  );

  refs.alwaysOnTop = checkboxInput(settings.always_on_top);
  track(refs.alwaysOnTop, "always_on_top");
  card.appendChild(
    fieldRow("Always on top", refs.alwaysOnTop, "Off: other windows can cover the widget. Docking stays on top regardless."),
  );

  const err = document.createElement("div");
  err.className = "error";
  err.hidden = true;
  const actions = document.createElement("div");
  actions.className = "actions";
  const done = makeButton("Done", "primary");
  actions.append(done);
  card.append(err, actions);

  // Settings save automatically: Done, Escape and a click outside the panel
  // all run the same save; an invalid field keeps the panel open instead.
  let saving = false;
  const saveAndClose = async () => {
    err.hidden = true;
    const patch = {};
    if (touched.has("opacity")) patch.opacity = Number(refs.opacity.value);
    if (touched.has("compact")) patch.compact = refs.compact.checked;
    if (touched.has("show_scoped")) patch.show_scoped = refs.showScoped.checked;
    if (touched.has("thresholds")) {
      const w = Number(refs.warn.value);
      const c = Number(refs.crit.value);
      if (!Number.isFinite(w) || !Number.isFinite(c) || w < 1 || c > 100 || w >= c) {
        err.textContent = "Thresholds: warn must be below crit (1–100).";
        err.hidden = false;
        return;
      }
      patch.thresholds = { warn: w, crit: c };
    }
    if (touched.has("autostart")) patch.autostart = refs.autostart.checked;
    if (touched.has("click_through")) patch.click_through = refs.clickThrough.checked;
    if (touched.has("always_on_top")) patch.always_on_top = refs.alwaysOnTop.checked;
    if (touched.has("colors")) {
      const map = { ...(settings.colors || {}) };
      for (const [id, inp] of refs.colorInputs) map[id] = inp.value;
      patch.colors = map;
    }
    const dockPatch = {};
    if (touched.has("dock.enabled")) dockPatch.enabled = refs.dockEnabled.checked;
    if (touched.has("dock.corner")) dockPatch.corner = refs.corner.value;
    if (touched.has("dock.offset")) {
      dockPatch.offset = {
        x: Math.trunc(Number(refs.offx.value)) || 0,
        y: Math.trunc(Number(refs.offy.value)) || 0,
      };
    }
    if (touched.has("dock.inside")) dockPatch.inside = refs.inside.checked;
    if (touched.has("dock.follow_focus")) dockPatch.follow_focus = refs.follow.checked;
    if (touched.has("dock.allow")) dockPatch.allow = parseAllow(refs.allow.value);
    if (Object.keys(dockPatch).length) patch.dock = dockPatch;

    const sessionPatch = {};
    if (touched.has("session.cwd")) sessionPatch.cwd = refs.sessionCwd.value.trim();
    if (touched.has("session.terminal")) {
      sessionPatch.terminal = parseArgv(refs.sessionTerminal.value);
    }
    if (Object.keys(sessionPatch).length) patch.session = sessionPatch;

    if (!Object.keys(patch).length) {
      closeModal();
      return;
    }
    if (saving) return;
    saving = true;
    try {
      await invoke("set_settings", { patch });
      closeModal(); // settings-changed re-renders everything
    } catch (e) {
      err.textContent = String(e);
      err.hidden = false;
    } finally {
      saving = false;
    }
  };

  done.onclick = saveAndClose;
  modalCancel = saveAndClose;
  card.parentElement.addEventListener("mousedown", (e) => {
    if (e.target === card.parentElement) saveAndClose();
  });

  panelRefs = refs;
}

// A Rust-side settings change while the panel is open (Attached rewriting
// dock.enabled/remembered, a tray click-through toggle) updates the panel's
// untouched fields in place; touched fields keep the user's edits.
function refreshPanelFromSettings() {
  if (!panelOpen || !panelRefs) return;
  const r = panelRefs;
  const untouched = (path) => !r.touched.has(path);
  if (untouched("opacity") && r.opacity) {
    r.opacity.value = String(Number.isFinite(settings.opacity) ? settings.opacity : 0.85);
  }
  if (untouched("compact") && r.compact) r.compact.checked = settings.compact === true;
  if (untouched("show_scoped") && r.showScoped) {
    r.showScoped.checked = settings.show_scoped === true;
  }
  if (untouched("thresholds") && r.warn && r.crit) {
    const th = settings.thresholds || {};
    if (Number.isFinite(th.warn)) r.warn.value = String(th.warn);
    if (Number.isFinite(th.crit)) r.crit.value = String(th.crit);
  }
  if (untouched("autostart") && r.autostart) r.autostart.checked = settings.autostart === true;
  if (untouched("click_through") && r.clickThrough) {
    r.clickThrough.checked = settings.click_through === true;
  }
  if (untouched("always_on_top") && r.alwaysOnTop) {
    r.alwaysOnTop.checked = settings.always_on_top === true;
  }
  const d = settings.dock || {};
  if (untouched("dock.enabled") && r.dockEnabled) r.dockEnabled.checked = d.enabled === true;
  if (untouched("dock.corner") && r.corner && typeof d.corner === "string") {
    r.corner.value = d.corner;
  }
  if (untouched("dock.offset") && r.offx && r.offy && d.offset) {
    if (Number.isFinite(d.offset.x)) r.offx.value = String(d.offset.x);
    if (Number.isFinite(d.offset.y)) r.offy.value = String(d.offset.y);
  }
  if (untouched("dock.inside") && r.inside) r.inside.checked = d.inside === true;
  if (untouched("dock.follow_focus") && r.follow) r.follow.checked = d.follow_focus === true;
  if (untouched("dock.allow") && r.allow) r.allow.value = allowText(d.allow);
  const se = settings.session || {};
  if (untouched("session.cwd") && r.sessionCwd) {
    r.sessionCwd.value = typeof se.cwd === "string" ? se.cwd : "";
  }
  if (untouched("session.terminal") && r.sessionTerminal) {
    r.sessionTerminal.value = argvText(se.terminal);
  }
  // Read-only and never sent, so always current.
  if (r.remembered) r.remembered.textContent = d.remembered || "(none)";
}

// --- Wiring -----------------------------------------------------------------

// Drag the window by the widget body. WebView2 ignores -webkit-app-region, so
// this goes through the window API; a docked or click-through window is not
// ours to move (M6.4).
document.getElementById("widget").addEventListener("mousedown", (e) => {
  if (e.button !== 0) return;
  if (e.target.closest("button, input, a, .modal, textarea, select")) return;
  if (dockState === "docked" || clickThrough) return;
  const w = window.__TAURI__ && window.__TAURI__.window;
  if (w && w.getCurrentWindow) {
    w.getCurrentWindow()
      .startDragging()
      .catch(() => {});
  }
});

// One Escape handler for every modal: each sets its own cancel path, and a
// modal that must not be dismissed leaves it null.
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && modalRoot.classList.contains("open") && modalCancel) {
    modalCancel();
  }
});

document.getElementById("connect").addEventListener("click", async () => {
  const label = await promptLabel();
  if (label) await runConnect(label);
});

document.getElementById("gear").addEventListener("click", openSettings);

document.getElementById("update").addEventListener("click", () => {
  if (updateUrl) invoke("open_release", { url: updateUrl }).catch(() => {});
});

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
    refreshPanelFromSettings();
  });
  listen("dock-state", (e) => setDock(e && e.payload));
  listen("open-settings", () => openSettings());
  invoke("dock_state")
    .then(setDock)
    .catch(() => {});

  checkUpdate();
  setInterval(checkUpdate, UPDATE_EVERY_MS);

  streamLoop();
})();
