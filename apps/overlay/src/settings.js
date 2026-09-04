// The settings window (M7): a window of its own with one tab per area, so the
// controls are never squeezed into the widget or the popover.
//
// Every control saves on change, and the patch names only that field, so a
// Rust-side write while the window is open (a dock attach, a tray toggle) is
// never reverted by a stale neighbour. `settings-changed` then re-syncs every
// control that is not being edited.

let settings = defaultSettings();
let accounts = [];
let dockState = { state: "undocked", target: null };
let bearer = null;
let daemonPort = 8787;

// Each control registers a function that re-reads `settings` into itself.
const syncers = [];

function sync() {
  for (const f of syncers) f();
}

function menuBarMode() {
  return settings.mode !== "widget";
}

// --- saving -----------------------------------------------------------------

const statusEl = document.getElementById("status");
let statusTimer = 0;

function showStatus(msg, isError) {
  clearTimeout(statusTimer);
  statusEl.textContent = msg;
  statusEl.classList.toggle("error", isError === true);
  statusEl.classList.remove("fade");
  if (!isError) {
    statusTimer = setTimeout(() => statusEl.classList.add("fade"), 1500);
  }
}

// One field per patch. On failure the controls are re-synced from the stored
// settings, so the window never shows a value that was not saved.
async function patch(p) {
  try {
    const s = await invoke("set_settings", { patch: p });
    if (s && typeof s === "object") settings = s;
    showStatus("Saved");
  } catch (e) {
    showStatus(String(e), true);
  }
  sync();
}

// --- element helpers --------------------------------------------------------

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}

function isEditing(input) {
  return document.activeElement === input;
}

// A setting row: label and hint on the left, the control on the right.
// `enabledWhen` greys the row and disables its controls while false.
function row(label, hint, control, opts = {}) {
  const r = el("div", `row${opts.stack ? " stack" : ""}`);
  const text = el("div", "text");
  text.appendChild(el("span", "label", label));
  if (hint) text.appendChild(el("span", "hint", hint));
  const c = el("div", "control");
  if (control) c.append(...(Array.isArray(control) ? control : [control]));
  r.append(text, c);
  if (opts.enabledWhen) {
    syncers.push(() => {
      const on = opts.enabledWhen() === true;
      r.classList.toggle("disabled", !on);
      for (const i of r.querySelectorAll("input, select, textarea, button")) i.disabled = !on;
    });
  }
  return r;
}

function group(title, ...children) {
  const g = el("div", "group");
  if (title) g.appendChild(el("div", "group-title", title));
  g.append(...children);
  return g;
}

function switchControl(get, set) {
  const label = el("label", "switch");
  const input = document.createElement("input");
  input.type = "checkbox";
  label.append(input, el("span", "knob"));
  input.addEventListener("change", () => set(input.checked));
  syncers.push(() => {
    input.checked = get() === true;
  });
  return label;
}

function switchRow(label, hint, get, set, opts) {
  return row(label, hint, switchControl(get, set), opts);
}

// Saves on `change` (blur or Enter), not per keystroke: a half-typed path is
// not a setting yet.
function textInput(get, set, opts = {}) {
  const input = document.createElement("input");
  input.type = opts.type || "text";
  input.className = `field${opts.mono ? " mono" : ""}`;
  if (opts.placeholder) input.placeholder = opts.placeholder;
  if (opts.min !== undefined) input.min = String(opts.min);
  if (opts.max !== undefined) input.max = String(opts.max);
  input.spellcheck = false;
  input.addEventListener("change", () => set(input.value));
  syncers.push(() => {
    if (!isEditing(input)) input.value = get();
  });
  return input;
}

function textarea(get, set, placeholder) {
  const t = document.createElement("textarea");
  t.className = "field";
  t.spellcheck = false;
  if (placeholder) t.placeholder = placeholder;
  t.addEventListener("change", () => set(t.value));
  syncers.push(() => {
    if (!isEditing(t)) t.value = get();
  });
  return t;
}

function button(text, kind, onClick) {
  const b = el("button", `btn${kind ? ` ${kind}` : ""}`, text);
  b.type = "button";
  b.addEventListener("click", onClick);
  return b;
}

function intOr(value, fallback) {
  const n = Math.trunc(Number(value));
  return Number.isFinite(n) ? n : fallback;
}

// --- General ----------------------------------------------------------------

function modeChoice(mode, glyph, title, desc) {
  const c = el("button", "choice");
  c.type = "button";
  const t = el("div", "title");
  t.append(el("span", "glyph", glyph), document.createTextNode(title));
  c.append(t, el("div", "desc", desc));
  c.addEventListener("click", () => {
    if (settings.mode !== mode) patch({ mode });
  });
  syncers.push(() => c.classList.toggle("selected", settings.mode === mode));
  return c;
}

function buildGeneral(pane) {
  const choices = el("div", "choices");
  choices.append(
    modeChoice(
      "menu_bar",
      "▣",
      isMac() ? "Menu bar" : "Tray icon",
      isMac()
        ? "No floating window. Click the menu bar icon to see usage; it closes on its own."
        : "No floating window. Click the tray icon to see usage; it closes on its own.",
    ),
    modeChoice(
      "widget",
      "▤",
      "Floating widget",
      "A small always-there window you can move, dock to a terminal, or make click-through.",
    ),
  );
  pane.append(
    group("Show usage as", choices),
    group(
      "System",
      switchRow(
        "Start at login",
        "A dev build stores the flag without registering.",
        () => settings.autostart,
        (v) => patch({ autostart: v }),
      ),
    ),
    group(
      "Floating widget",
      switchRow(
        "Click-through",
        "Clicks and drags pass through the widget. Turn it off again from the tray icon.",
        () => settings.click_through,
        (v) => patch({ click_through: v }),
        { enabledWhen: () => !menuBarMode() },
      ),
      switchRow(
        "Always on top",
        "A docked widget floats over its target anyway; docking turns this off.",
        () => settings.always_on_top,
        (v) => patch({ always_on_top: v }),
        { enabledWhen: () => !menuBarMode() },
      ),
    ),
  );
}

// --- Appearance -------------------------------------------------------------

function opacityRow() {
  const slider = document.createElement("input");
  slider.type = "range";
  slider.min = "0.2";
  slider.max = "1";
  slider.step = "0.01";
  const value = el("span", "value");
  const show = (v) => {
    value.textContent = `${Math.round(Number(v) * 100)}%`;
  };
  // Live preview in the widget while dragging; the value is saved on release.
  slider.addEventListener("input", () => {
    show(slider.value);
    emitTo("main", "preview-opacity", Number(slider.value));
  });
  slider.addEventListener("change", () => patch({ opacity: Number(slider.value) }));
  syncers.push(() => {
    if (isEditing(slider)) return;
    const o = Number.isFinite(settings.opacity) ? settings.opacity : 0.85;
    slider.value = String(o);
    show(o);
  });
  return row("Opacity", "Background of the widget and the popover.", [slider, value]);
}

function thresholdRows() {
  const err = el("span", "error");
  err.hidden = true;
  const warn = textInput(() => String(settings.thresholds.warn), save, {
    type: "number",
    min: 1,
    max: 100,
  });
  const crit = textInput(() => String(settings.thresholds.crit), save, {
    type: "number",
    min: 1,
    max: 100,
  });
  function save() {
    const w = Number(warn.value);
    const c = Number(crit.value);
    const ok = Number.isFinite(w) && Number.isFinite(c) && w >= 1 && c <= 100 && w < c;
    warn.classList.toggle("invalid", !ok);
    crit.classList.toggle("invalid", !ok);
    err.hidden = ok;
    if (ok) patch({ thresholds: { warn: w, crit: c } });
    else err.textContent = "Warn must be below critical, both 1–100.";
  }
  syncers.push(() => {
    if (isEditing(warn) || isEditing(crit)) return;
    warn.classList.remove("invalid");
    crit.classList.remove("invalid");
    err.hidden = true;
  });
  const critRow = row("Critical at", "Bars turn red from this percentage.", [
    crit,
    el("span", "unit", "%"),
  ]);
  critRow.querySelector(".text").appendChild(err);
  return [
    row("Warn at", "Bars turn amber from this percentage.", [warn, el("span", "unit", "%")]),
    critRow,
  ];
}

const colorsGroup = el("div", "group");

// The swatch list is rebuilt when the accounts arrive, so its inputs cannot
// register in `syncers` once; one entry there calls whatever list exists now.
let syncColors = () => {};
syncers.push(() => syncColors());

function renderColors() {
  colorsGroup.replaceChildren(el("div", "group-title", "Account colours"));
  const listed = accounts.filter((a) => a && typeof a.id === "string");
  const local = [];
  syncColors = () => {
    for (const f of local) f();
  };
  if (!listed.length) {
    colorsGroup.appendChild(row("No accounts yet", "Connect one from the usage view first."));
    return;
  }
  listed.forEach((a, i) => {
    const input = document.createElement("input");
    input.type = "color";
    input.addEventListener("change", () => {
      patch({ colors: { ...(settings.colors || {}), [a.id]: input.value } });
    });
    const reset = button("Reset", "small", () => {
      const map = { ...(settings.colors || {}) };
      delete map[a.id];
      patch({ colors: map });
    });
    const r = row(typeof a.label === "string" ? a.label : a.id, null, [reset, input]);
    r.classList.add("swatch-row");
    const syncer = () => {
      input.value = paletteColor(settings, a.id, i);
      reset.hidden = !(settings.colors && typeof settings.colors[a.id] === "string");
    };
    syncer();
    local.push(syncer);
    colorsGroup.appendChild(r);
  });
}

function buildAppearance(pane) {
  pane.append(
    group(
      "Widget",
      opacityRow(),
      switchRow(
        "Compact",
        "Account names and percentages only, no bars.",
        () => settings.compact,
        (v) => patch({ compact: v }),
      ),
      switchRow(
        "Per-model limits",
        "Show the weekly limit of each model under its account.",
        () => settings.show_scoped,
        (v) => patch({ show_scoped: v }),
      ),
    ),
    group("Thresholds", ...thresholdRows()),
    colorsGroup,
  );
}

// --- Docking ----------------------------------------------------------------

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

// "win32:<class>|<exe>" keeps the class; "macos:<bundle id>" is already the
// whole name (geometry.rs).
function shortTarget(t) {
  if (typeof t !== "string") return "";
  if (t.startsWith("macos:")) return t.slice(6);
  const rest = t.startsWith("win32:") ? t.slice(6) : t;
  const bar = rest.indexOf("|");
  return bar >= 0 ? rest.slice(0, bar) : rest;
}

function dockStateStrip() {
  const strip = el("div", "dockstate");
  const dot = el("span", "dot");
  const text = el("span", "text");
  const pick = button("Pick window…", "", () => {
    // The guards against picking our own window live in Rust (plan §6). This
    // window goes first so it is never the one under the click.
    invoke("dock_pick").catch(() => {});
    invoke("close_settings").catch(() => {});
  });
  const undock = button("Undock", "", () => invoke("dock_stop").catch(() => {}));
  strip.append(dot, text, pick, undock);
  syncers.push(() => {
    const st = dockState.state;
    strip.className = `dockstate ${st}`;
    const name = shortTarget(dockState.target);
    text.textContent =
      st === "docked"
        ? `Docked to ${name}`
        : st === "detached"
          ? `Detached — looking for ${name}`
          : st === "picking"
            ? "Picking — click the window to dock to"
            : "Not docked";
    pick.disabled = menuBarMode() || st === "picking";
    undock.disabled = !(st === "docked" || st === "detached");
  });
  return strip;
}

// macOS docking runs on a permission-free poll and only gets smoother when
// Accessibility is granted (plan §6): one honest line the user can dismiss
// for good, never a blocker, and never shown where there is no such
// permission (Rust answers `applicable: false` there). The grant lands while
// this window is open, so the check repeats while the callout is up and the
// line goes away on its own.
const ACCESS_RECHECK_MS = 2000;

function accessibilityCallout() {
  const box = el("div", "callout");
  box.hidden = true;
  const text = el("div", "text");
  const hint = el("span", "hint", "The system asks once; the widget never raises the dialog on its own.");
  text.append(
    el("span", "label", "Docking works now. Grant Accessibility for smoother tracking."),
    hint,
  );
  // The system dialog, not a System Settings link: open_url's allowlist is
  // https-only and stays that way.
  const grant = button("Grant…", "primary", () => {
    invoke("dock_grant_accessibility").catch(() => {});
  });
  let dismissed = false;
  const dismiss = button("Dismiss", "", () => {
    dismissed = true;
    box.hidden = true;
    patch({ dock: { show_accessibility_hint: false } });
  });
  box.append(text, grant, dismiss);

  let timer = 0;
  async function check() {
    clearTimeout(timer);
    if (dismissed) return;
    const wanted = settings.dock && settings.dock.show_accessibility_hint !== false;
    let a = null;
    try {
      a = await invoke("dock_accessibility");
    } catch {
      return;
    }
    const show = wanted && a && a.applicable === true && a.trusted !== true;
    box.hidden = !show;
    if (!show) return;
    // A rebuilt debug binary is a new identity to TCC: the switch in System
    // Settings still reads on, but this process is not trusted until it is
    // removed from the list and granted again.
    hint.textContent =
      a.dev === true
        ? "Dev build: every rebuild loses the grant. In System Settings › Privacy & Security › Accessibility remove cuw-overlay with − and grant it again."
        : "The system asks once; the widget never raises the dialog on its own.";
    if (!box.closest(".pane").hidden) timer = setTimeout(check, ACCESS_RECHECK_MS);
  }
  check();
  // The tab may have been hidden while the timer was off; a switch back or a
  // return from System Settings picks the check up again.
  window.addEventListener("focus", check);
  box.recheck = check;
  return box;
}

function cornerPicker() {
  const grid = el("div", "corners");
  const corners = ["top_left", "top_right", "bottom_left", "bottom_right"];
  const buttons = corners.map((c) => {
    const b = el("button");
    b.type = "button";
    b.title = c.replace("_", " ");
    b.addEventListener("click", () => patch({ dock: { corner: c } }));
    grid.appendChild(b);
    return b;
  });
  syncers.push(() => {
    const cur = (settings.dock && settings.dock.corner) || "top_right";
    buttons.forEach((b, i) => b.classList.toggle("selected", corners[i] === cur));
  });
  return grid;
}

function buildDocking(pane) {
  const locked = el("div", "callout");
  const lockedText = el("div", "text");
  lockedText.append(
    el("span", "label", "Docking needs the floating widget."),
    el("span", "hint", "The popover has nothing to attach to."),
  );
  locked.append(
    lockedText,
    button("Use the floating widget", "primary", () => patch({ mode: "widget" })),
  );
  syncers.push(() => {
    locked.hidden = !menuBarMode();
  });
  const widget = { enabledWhen: () => !menuBarMode() };

  const off = () => (settings.dock && settings.dock.offset) || { x: 1, y: 42 };
  const offX = textInput(
    () => String(off().x),
    (v) => patch({ dock: { offset: { x: intOr(v, 0), y: off().y } } }),
    { type: "number", min: -500, max: 500 },
  );
  const offY = textInput(
    () => String(off().y),
    (v) => patch({ dock: { offset: { x: off().x, y: intOr(v, 0) } } }),
    { type: "number", min: -500, max: 500 },
  );

  const remembered = el("span", "note mono");
  syncers.push(() => {
    remembered.textContent = (settings.dock && settings.dock.remembered) || "(none)";
  });

  const allow = textarea(
    () => allowText(settings.dock && settings.dock.allow),
    (v) => patch({ dock: { allow: parseAllow(v) } }),
    isMac() ? "com.apple.Terminal" : "ConsoleWindowClass\nChrome_WidgetWin_1|Code.exe",
  );

  pane.append(
    locked,
    group(null, dockStateStrip()),
    accessibilityCallout(),
    group(
      "Placement",
      switchRow(
        "Dock at startup",
        "Re-attach to the remembered window when the widget starts.",
        () => settings.dock && settings.dock.enabled,
        (v) => patch({ dock: { enabled: v } }),
        widget,
      ),
      row("Corner", "Which corner of the target the widget sits in.", cornerPicker(), widget),
      row(
        "Offset",
        "Pixels from that corner, x then y.",
        [offX, el("span", "unit", "×"), offY],
        widget,
      ),
      switchRow(
        "Inside the window",
        "Overlap the target instead of sitting just outside it.",
        () => settings.dock && settings.dock.inside,
        (v) => patch({ dock: { inside: v } }),
        widget,
      ),
      switchRow(
        "Follow focus",
        "A docked widget always hides when its target is unfocused; this also re-docks it to another allowed window that takes focus.",
        () => settings.dock && settings.dock.follow_focus,
        (v) => patch({ dock: { follow_focus: v } }),
        widget,
      ),
    ),
    group(
      "Targets",
      row("Remembered window", "Written when the widget attaches; not editable.", remembered),
      row(
        isMac() ? "Allowed apps" : "Allowed windows",
        isMac()
          ? "One bundle id per line."
          : "One window class per line, optionally followed by |exe.",
        allow,
        { stack: true, ...widget },
      ),
    ),
  );
}

// --- Sessions ---------------------------------------------------------------

// A terminal override is argv, one argument per line, never re-split (SWITCHER
// §5) — so a path with spaces survives the round trip through the box.
function argvText(argv) {
  return (Array.isArray(argv) ? argv : []).filter((a) => typeof a === "string").join("\n");
}

function parseArgv(text) {
  return String(text)
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean);
}

function buildSessions(pane) {
  const sess = () => settings.session || {};
  const cwd = textInput(
    () => (typeof sess().cwd === "string" ? sess().cwd : ""),
    (v) => patch({ session: { cwd: v.trim() } }),
    { placeholder: "(your home directory)", mono: true },
  );
  const term = textarea(
    () => argvText(sess().terminal),
    (v) => patch({ session: { terminal: parseArgv(v) } }),
    isMac() ? "open\n-a\niTerm" : "wt.exe\n-d\n{cwd}",
  );
  pane.append(
    el("div", "lead", "How a switched session starts when you press ▸ on an account."),
    group(
      "Terminal",
      row("Start directory", "Where the new session opens.", cwd, { stack: true }),
      row(
        "Terminal command",
        // The fold rule diverges by platform on purpose (cuw-launch plan.rs):
        // on Windows an override is a prefix unless it names {shim}; on macOS
        // the default is `open -a Terminal <wrapper>`, so prefixing it is
        // nonsense and a plain override is a launcher the wrapper is
        // appended to instead.
        isMac()
          ? "One argument per line. Blank uses Terminal.app. {wrapper} {shim} {nonce} {port} {cwd} substitute; a command naming any of them is used as written, anything else is a launcher the wrapper is appended to (open -a iTerm)."
          : "One argument per line. Blank uses the default terminal. {shim} {nonce} {port} {cwd} substitute; a command containing {shim} replaces the default, anything else prefixes it.",
        term,
        { stack: true },
      ),
    ),
  );
}

// --- About ------------------------------------------------------------------

function buildAbout(pane) {
  const box = el("div", "about");
  const ver = el("div", "ver", "version unknown");
  const t = window.__TAURI__;
  if (t && t.app && typeof t.app.getVersion === "function") {
    t.app
      .getVersion()
      .then((v) => {
        ver.textContent = `Version ${v}`;
      })
      .catch(() => {});
  }
  const result = el("span", "note");
  const open = button("Open release page", "small", () => {});
  open.hidden = true;
  const check = button("Check for updates", "", async () => {
    check.disabled = true;
    result.textContent = "Checking…";
    open.hidden = true;
    try {
      const info = await invoke("check_update");
      const ok = info && info.available === true && typeof info.latest === "string";
      result.textContent = ok ? `Version ${info.latest} is available.` : "You are up to date.";
      if (ok && typeof info.url === "string") {
        open.hidden = false;
        open.onclick = () => invoke("open_release", { url: info.url }).catch(() => {});
      }
    } catch {
      result.textContent = "Could not check right now.";
    } finally {
      check.disabled = false;
    }
  });
  const update = el("div", "update");
  update.append(check, result, open);
  box.append(el("div", "app", "Claude Usage Widget"), ver, update);

  const keys = el("div", "text");
  keys.append(
    el("span", "label", "Keyboard"),
    el("span", "hint", "Esc closes this window. In the popover, Esc closes the popover."),
  );
  const keysRow = el("div", "row");
  keysRow.appendChild(keys);
  pane.append(
    group(null, box),
    group(
      "Tips",
      keysRow,
      row(
        "Usage view",
        "Connect, disconnect and switch accounts from the usage view itself, not from here.",
      ),
    ),
  );
}

// --- Tabs -------------------------------------------------------------------

const TABS = [
  { id: "general", title: "General", glyph: "⚙", build: buildGeneral },
  { id: "appearance", title: "Appearance", glyph: "◐", build: buildAppearance },
  { id: "docking", title: "Docking", glyph: "⌖", build: buildDocking, when: dockingSupported },
  { id: "sessions", title: "Sessions", glyph: "❯", build: buildSessions },
  { id: "about", title: "About", glyph: "ⓘ", build: buildAbout },
];

const nav = document.getElementById("nav");
const panes = document.getElementById("panes");
const tabButtons = new Map();
const tabPanes = new Map();

function showTab(id) {
  for (const [k, b] of tabButtons) b.classList.toggle("active", k === id);
  for (const [k, p] of tabPanes) p.hidden = k !== id;
  panes.scrollTop = 0;
  for (const c of panes.querySelectorAll(".callout")) {
    if (typeof c.recheck === "function" && !c.closest(".pane").hidden) c.recheck();
  }
  try {
    localStorage.setItem("settings-tab", id);
  } catch {
    /* storage may be unavailable; the tab just is not remembered */
  }
}

function buildTabs() {
  for (const tab of TABS) {
    if (tab.when && !tab.when()) continue;
    const b = el("button", "tab");
    b.type = "button";
    b.append(el("span", "glyph", tab.glyph), document.createTextNode(tab.title));
    b.addEventListener("click", () => showTab(tab.id));
    nav.appendChild(b);
    tabButtons.set(tab.id, b);

    const pane = el("div", "pane");
    pane.hidden = true;
    pane.appendChild(el("h1", null, tab.title));
    tab.build(pane);
    panes.appendChild(pane);
    tabPanes.set(tab.id, pane);
  }
  let initial = "general";
  try {
    const saved = localStorage.getItem("settings-tab");
    if (saved && tabPanes.has(saved)) initial = saved;
  } catch {
    /* see above */
  }
  showTab(initial);
}

// --- Data -------------------------------------------------------------------

// The account list, for the colour swatches. One authenticated GET: this
// window is short-lived, so it does not hold a stream of its own.
async function loadAccounts() {
  try {
    if (!bearer) bearer = await invoke("bearer_token");
    const p = await invoke("daemon_port");
    if (Number.isInteger(p) && p > 0) daemonPort = p;
    const res = await fetch(`http://127.0.0.1:${daemonPort}/accounts`, {
      headers: { Authorization: `Bearer ${bearer}` },
    });
    const list = res.ok ? await res.json() : [];
    accounts = Array.isArray(list) ? list : [];
  } catch {
    accounts = [];
  }
  renderColors();
}

function close() {
  invoke("close_settings").catch(() => window.close());
}

// --- Wiring -----------------------------------------------------------------

document.getElementById("done").addEventListener("click", close);

document.addEventListener("keydown", (e) => {
  // No menu bar under the accessory activation policy, so Cmd+W is ours.
  if (e.key === "Escape" || (e.key === "w" && (e.metaKey || e.ctrlKey))) {
    e.preventDefault();
    close();
  }
});

// Native text editing only inside fields; the rest of the window is chrome.
document.addEventListener("contextmenu", (e) => {
  if (!e.target.closest("input, textarea")) e.preventDefault();
});

(async function main() {
  try {
    const s = await invoke("get_settings");
    if (s && typeof s === "object") settings = s;
  } catch {
    /* plain browser — defaults stay */
  }
  try {
    const d = await invoke("dock_state");
    if (d && typeof d.state === "string") dockState = d;
  } catch {
    /* no tracker here */
  }
  buildTabs();
  sync();
  loadAccounts();

  listen("settings-changed", (e) => {
    if (!e || !e.payload || typeof e.payload !== "object") return;
    settings = e.payload;
    sync();
  });
  listen("dock-state", (e) => {
    const d = e && e.payload;
    if (d && typeof d.state === "string") {
      dockState = d;
      sync();
    }
  });
})();
