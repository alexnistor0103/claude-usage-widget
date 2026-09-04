// Shared by the widget and the settings window: the Tauri bridge (safe no-ops
// outside the shell), platform sniffing, and the settings defaults.

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

// Window-to-window event, e.g. a live preview the settings window sends the
// widget before the value is saved.
function emitTo(label, name, payload) {
  const t = window.__TAURI__;
  if (t && t.event && typeof t.event.emitTo === "function") {
    t.event.emitTo(label, name, payload).catch(() => {});
  }
}

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

// Mirror of the Rust defaults (settings.rs); used until get_settings answers
// and forever in a plain browser.
function defaultSettings() {
  return {
    version: 3,
    mode: "menu_bar",
    opacity: 0.85,
    compact: false,
    thresholds: { warn: 75, crit: 90 },
    autostart: false,
    click_through: false,
    always_on_top: false,
    show_scoped: true,
    colors: {},
    dock: {
      enabled: false,
      remembered: null,
      corner: "top_right",
      offset: { x: 1, y: 42 },
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

// Row colours when the user has not picked one; the index is the row's.
const PALETTE = ["#6ea8fe", "#7ee0a0", "#f5c96b", "#c79bff", "#ff9b85"];

function paletteColor(settings, id, index) {
  const c = settings.colors && settings.colors[id];
  return typeof c === "string" ? c : PALETTE[index % PALETTE.length];
}
