//! Pump thread, WinEvent hooks and `WindowsTracker` (M4.3).
//!
//! One dedicated thread owns every Win32 call here: `WINEVENT_OUTOFCONTEXT`
//! callbacks reach only the thread that installed the hook, and only while it
//! pumps messages. Handle methods queue a command and wake that thread; every
//! result comes back as a `TrackerEvent` (plan §6).

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, KillTimer, PeekMessageW, PostThreadMessageW, SetTimer,
    TranslateMessage, CHILDID_SELF, EVENT_OBJECT_CLOAKED, EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE,
    EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_SHOW, EVENT_OBJECT_UNCLOAKED,
    EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART,
    EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZESTART, MSG, OBJID_WINDOW, PM_NOREMOVE,
    WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_APP, WM_QUIT, WM_TIMER, WM_USER,
};

use super::{bounds, find};
use crate::geometry::{
    parse_target_id, rank_candidates, spec_matches, target_id, Candidate, Coalescer, SHELL_CLASSES,
};
use crate::{TargetId, TargetSpec, TrackerConfig, TrackerEvent, TrackerHandle, WindowTracker};

/// Drain the command queue.
const WM_CMD: u32 = WM_APP + 1;
/// A foreground change the callback deferred; `wParam` is the hwnd.
const WM_DEFERRED_FG: u32 = WM_APP + 2;

const SEARCH_INTERVAL_MS: u32 = 2_000;
const PICK_TIMEOUT_MS: u32 = 10_000;
/// A tray menu closing re-activates the previous window before the user clicks.
const PICK_SETTLE: Duration = Duration::from_millis(300);
const START_TIMEOUT: Duration = Duration::from_secs(2);
const JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// The Windows implementation of [`WindowTracker`].
pub struct WindowsTracker;

enum Cmd {
    Attach(Option<TargetId>),
    Pick,
    Detach,
}

/// Owns the pump thread; every method is a queue push plus a wake-up post.
pub struct Handle {
    tid: u32,
    queue: Arc<Mutex<VecDeque<Cmd>>>,
    join: Option<JoinHandle<()>>,
    stopped: bool,
}

impl WindowTracker for WindowsTracker {
    type Handle = Handle;

    fn start(cfg: TrackerConfig) -> anyhow::Result<(Handle, Receiver<TrackerEvent>)> {
        let (tx, rx) = mpsc::channel();
        let queue: Arc<Mutex<VecDeque<Cmd>>> = Arc::new(Mutex::new(VecDeque::new()));
        let (tid_tx, tid_rx) = mpsc::channel();
        let thread_queue = Arc::clone(&queue);
        let join = std::thread::Builder::new()
            .name("cuw-tracker".to_string())
            .spawn(move || pump_main(cfg, tx, thread_queue, tid_tx))
            .context("spawning the tracker thread")?;
        // The id arrives only after the thread owns a message queue, so the
        // first command can never hit ERROR_INVALID_THREAD_ID.
        let tid = tid_rx
            .recv_timeout(START_TIMEOUT)
            .map_err(|_| anyhow!("tracker thread did not report its id"))?;
        Ok((
            Handle {
                tid,
                queue,
                join: Some(join),
                stopped: false,
            },
            rx,
        ))
    }
}

impl Handle {
    fn post(&self, cmd: Cmd) -> anyhow::Result<()> {
        lock(&self.queue).push_back(cmd);
        post_thread(self.tid, WM_CMD, 0).context("tracker thread is not accepting commands")
    }
}

impl TrackerHandle for Handle {
    fn attach(&self, id: Option<TargetId>) -> anyhow::Result<()> {
        self.post(Cmd::Attach(id))
    }

    fn pick_interactively(&self) -> anyhow::Result<()> {
        self.post(Cmd::Pick)
    }

    fn detach(&self) -> anyhow::Result<()> {
        self.post(Cmd::Detach)
    }

    fn stop(mut self) {
        let _ = post_thread(self.tid, WM_QUIT, 0);
        if let Some(join) = self.join.take() {
            join_capped(join, JOIN_TIMEOUT);
        }
        self.stopped = true;
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // Never a second WM_QUIT: after stop() the id may already be recycled.
        if !self.stopped && self.join.is_some() {
            let _ = post_thread(self.tid, WM_QUIT, 0);
        }
    }
}

/// Joins without ever blocking the caller on a wedged pump.
fn join_capped(join: JoinHandle<()>, cap: Duration) {
    let (done_tx, done_rx) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("cuw-tracker-join".to_string())
        .spawn(move || {
            let _ = join.join();
            let _ = done_tx.send(());
        });
    if spawned.is_ok() {
        let _ = done_rx.recv_timeout(cap);
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn post_thread(tid: u32, msg: u32, wparam: usize) -> windows::core::Result<()> {
    // SAFETY: a thread-message post; the payload is a plain integer.
    unsafe { PostThreadMessageW(tid, msg, WPARAM(wparam), LPARAM(0)) }
}

// ---------------------------------------------------------------------------
// Pump thread
// ---------------------------------------------------------------------------

struct HookState {
    own_tid: u32,
    own_pid: u32,
    target: Option<isize>,
    target_hooks: Vec<isize>,
    fg_hook: isize,
    tx: Sender<TrackerEvent>,
    coalescer: Coalescer,
    allow: Vec<TargetSpec>,
    remembered: Option<TargetSpec>,
    follow_focus: bool,
    searching: bool,
    search_timer: Option<usize>,
    picking_until: Option<Instant>,
    pick_started: Option<Instant>,
    pick_ignore: Option<isize>,
    pick_timer: Option<usize>,
    pending_fg: Option<isize>,
    /// The callback saw the target go; the pump does the unhooking. `IsWindow`
    /// is still true while `EVENT_OBJECT_DESTROY` runs, so this cannot be
    /// re-derived once the message reaches the pump.
    lost_pending: bool,
    dead: bool,
}

impl HookState {
    /// A dropped receiver is the only send failure; it means "stop".
    fn send(&mut self, ev: TrackerEvent) {
        if self.dead {
            return;
        }
        if self.tx.send(ev).is_err() {
            self.dead = true;
            let _ = post_thread(self.own_tid, WM_QUIT, 0);
        }
    }

    fn remembered_id(&self) -> Option<TargetId> {
        self.remembered
            .as_ref()
            .map(|s| target_id(&s.class, s.exe.as_deref()))
    }
}

thread_local! {
    static STATE: RefCell<Option<HookState>> = const { RefCell::new(None) };
}

/// Runs `f` on the pump thread's state; a re-entrant borrow drops the event.
fn with_state(f: impl FnOnce(&mut HookState)) {
    let _ = STATE.try_with(|cell| {
        if let Ok(mut slot) = cell.try_borrow_mut() {
            if let Some(st) = slot.as_mut() {
                f(st);
            }
        }
    });
}

fn pump_main(
    cfg: TrackerConfig,
    tx: Sender<TrackerEvent>,
    queue: Arc<Mutex<VecDeque<Cmd>>>,
    tid_tx: Sender<u32>,
) {
    // A thread has no message queue until its first User32 queue call, and
    // PostThreadMessageW to a queue-less thread fails with
    // ERROR_INVALID_THREAD_ID — so this precedes the tid handoff (plan §6).
    let mut probe = MSG::default();
    // SAFETY: `probe` is a live local; PM_NOREMOVE leaves the queue untouched.
    unsafe {
        let _ = PeekMessageW(&mut probe, None, WM_USER, WM_USER, PM_NOREMOVE);
    }
    // SAFETY: no arguments, cannot fail.
    let own_tid = unsafe { GetCurrentThreadId() };
    if tid_tx.send(own_tid).is_err() {
        return;
    }

    // SAFETY: the callback is a plain fn pointer; OUTOFCONTEXT needs no module.
    let fg_hook = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(win_event_cb),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        )
    };

    let remembered_id = cfg.remembered.clone();
    STATE.with(|cell| {
        *cell.borrow_mut() = Some(HookState {
            own_tid,
            own_pid: find::own_pid(),
            target: None,
            target_hooks: Vec::new(),
            fg_hook: fg_hook.0 as isize,
            tx,
            coalescer: Coalescer::default(),
            allow: cfg.allow,
            remembered: cfg.remembered.as_ref().and_then(parse_target_id),
            follow_focus: cfg.follow_focus,
            searching: false,
            search_timer: None,
            picking_until: None,
            pick_started: None,
            pick_ignore: None,
            pick_timer: None,
            pending_fg: None,
            lost_pending: false,
            dead: false,
        });
    });

    if remembered_id.is_some() {
        with_state(|st| do_attach(st, remembered_id));
    }

    loop {
        let mut msg = MSG::default();
        // SAFETY: `msg` is a live local; hwnd None takes thread messages too.
        let got = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        // 0 is WM_QUIT, -1 an error we must not spin on.
        if got.0 <= 0 {
            break;
        }
        match msg.message {
            WM_CMD => drain_queue(&queue),
            WM_DEFERRED_FG => on_deferred_foreground(msg.wParam.0 as isize),
            // Only the ids this pump created; anything else is dispatched
            // rather than silently swallowed.
            WM_TIMER if on_timer(msg.wParam.0) => {}
            _ => {
                // SAFETY: `msg` came from GetMessageW and outlives both calls.
                unsafe {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }
    }

    STATE.with(|cell| {
        if let Some(mut st) = cell.borrow_mut().take() {
            unhook_target(&mut st);
            stop_search(&mut st);
            clear_pick(&mut st);
            unhook(st.fg_hook);
        }
    });
}

fn drain_queue(queue: &Mutex<VecDeque<Cmd>>) {
    while let Some(cmd) = lock(queue).pop_front() {
        match cmd {
            Cmd::Attach(id) => with_state(|st| do_attach(st, id)),
            Cmd::Pick => with_state(start_pick),
            Cmd::Detach => with_state(do_detach),
        }
    }
}

/// `true` when the id is one of ours.
fn on_timer(id: usize) -> bool {
    let mut ours = false;
    with_state(|st| {
        if st.search_timer == Some(id) {
            try_reacquire(st);
            ours = true;
        } else if st.pick_timer == Some(id) {
            end_pick_timeout(st);
            ours = true;
        }
    });
    ours
}

// ---------------------------------------------------------------------------
// Attach / detach
// ---------------------------------------------------------------------------

fn do_attach(st: &mut HookState, id: Option<TargetId>) {
    let had_spec = id.is_some() || st.remembered.is_some();
    // An explicit id is exclusive: the remembered spec must not widen it. An
    // id that does not parse names nothing, so it can only be NotFound.
    let explicit = match &id {
        Some(id) => match parse_target_id(id) {
            Some(spec) => Some(spec),
            None => {
                st.send(TrackerEvent::NotFound);
                return;
            }
        },
        None => None,
    };
    let (allow, prefer) = match &explicit {
        Some(spec) => (vec![spec.clone()], None),
        None => (
            st.allow
                .iter()
                .cloned()
                .chain(st.remembered.clone())
                .collect(),
            st.remembered_id(),
        ),
    };
    let cands = find::candidates();
    let fg = find::foreground();
    // The search runs off `remembered` alone, so an explicit id becomes it
    // before the attach is even attempted: otherwise `try_reacquire` would
    // cancel the search NotFound just promised, and `hook_target`'s
    // died-in-the-race guard would search for the previous id (plan §6).
    if let Some(spec) = explicit {
        st.remembered = Some(spec);
    }
    match rank_candidates(&cands, &allow, fg, prefer.as_ref()) {
        Some(i) => {
            hook_target(st, &cands[i]);
        }
        None => {
            st.send(TrackerEvent::NotFound);
            // A failed re-target keeps the target it already has, so this can
            // leave the tracker attached and searching: the search is for the
            // newly remembered spec, and the old dock keeps reporting until it
            // is found.
            if had_spec {
                start_search(st);
            }
        }
    }
}

/// `true` when the dock is the given candidate afterwards. A caller that
/// reports focus must gate it on this: the window can die between enumeration
/// and the hook install.
fn hook_target(st: &mut HookState, cand: &Candidate) -> bool {
    let (pid, tid) = find::pid_tid(cand.hwnd);
    // Died between enumeration and here. pid/tid 0 would install the hooks
    // GLOBALLY — LOCATIONCHANGE especially — so this is no candidate at all;
    // the existing dock and its hooks stay untouched.
    if pid == 0 {
        if !st.searching {
            st.send(TrackerEvent::NotFound);
            if st.remembered.is_some() {
                start_search(st);
            }
        }
        return false;
    }
    unhook_target(st);
    // Scoped to the owning pid+tid: LOCATIONCHANGE is far too noisy globally.
    for (lo, hi) in [
        (EVENT_SYSTEM_MOVESIZESTART, EVENT_SYSTEM_MINIMIZEEND),
        (EVENT_OBJECT_DESTROY, EVENT_OBJECT_LOCATIONCHANGE),
        (EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED),
    ] {
        // SAFETY: the callback is a plain fn pointer; OUTOFCONTEXT needs no module.
        let h = unsafe {
            SetWinEventHook(
                lo,
                hi,
                None,
                Some(win_event_cb),
                pid,
                tid,
                WINEVENT_OUTOFCONTEXT,
            )
        };
        if !h.is_invalid() {
            st.target_hooks.push(h.0 as isize);
        }
    }
    st.target = Some(cand.hwnd);
    st.remembered = Some(TargetSpec {
        class: cand.class.clone(),
        exe: cand.exe.clone(),
    });
    stop_search(st);
    st.coalescer.reset();
    st.send(TrackerEvent::Attached(target_id(
        &cand.class,
        cand.exe.as_deref(),
    )));
    if !emit_geometry(st) {
        on_lost(st);
        return false;
    }
    true
}

fn do_detach(st: &mut HookState) {
    unhook_target(st);
    st.target = None;
    stop_search(st);
    clear_pick(st);
}

/// `false` when the target is gone: the caller decides, because the callback
/// may not unhook or set the search timer itself.
fn emit_geometry(st: &mut HookState) -> bool {
    let Some(target) = st.target else {
        return true;
    };
    match bounds::read(target) {
        bounds::Read::Bounds(b) => {
            if st.coalescer.push(b) {
                st.send(TrackerEvent::Bounds(b));
            }
            true
        }
        bounds::Read::Iconic => {
            st.send(TrackerEvent::Minimized);
            true
        }
        bounds::Read::Gone => false,
    }
}

fn on_lost(st: &mut HookState) {
    unhook_target(st);
    st.target = None;
    start_search(st);
    st.send(TrackerEvent::Lost);
}

fn unhook_target(st: &mut HookState) {
    for h in st.target_hooks.drain(..) {
        unhook(h);
    }
}

fn unhook(h: isize) {
    if h == 0 {
        return;
    }
    // SAFETY: `h` came from SetWinEventHook on this thread and is dropped after.
    let _ = unsafe { UnhookWinEvent(HWINEVENTHOOK(h as *mut c_void)) };
}

// ---------------------------------------------------------------------------
// Searching
// ---------------------------------------------------------------------------

fn start_search(st: &mut HookState) {
    st.searching = true;
    if st.search_timer.is_some() {
        return;
    }
    // SAFETY: no window, no TIMERPROC — WM_TIMER lands in this thread's queue.
    // SetTimer(None, ..) ignores the id passed in and returns a fresh one.
    let id = unsafe { SetTimer(None, 0, SEARCH_INTERVAL_MS, None) };
    if id != 0 {
        st.search_timer = Some(id);
    }
}

fn stop_search(st: &mut HookState) {
    st.searching = false;
    if let Some(id) = st.search_timer.take() {
        // SAFETY: the id is the one SetTimer returned for this thread.
        let _ = unsafe { KillTimer(None, id) };
    }
}

/// Re-acquire is restricted to the remembered spec: a lost target comes back as
/// itself, never as some other allowed window. `true` when it came back.
fn try_reacquire(st: &mut HookState) -> bool {
    let Some(spec) = st.remembered.clone() else {
        stop_search(st);
        return false;
    };
    let cands = find::candidates();
    let id = st.remembered_id();
    match rank_candidates(
        &cands,
        std::slice::from_ref(&spec),
        find::foreground(),
        id.as_ref(),
    ) {
        Some(i) => hook_target(st, &cands[i]),
        None => false,
    }
}

/// `follow_focus`: an allowed window that takes the foreground takes over.
fn follow(st: &mut HookState, hwnd: isize) {
    // Only while docked or searching for a lost target. After an explicit
    // detach the tracker is idle, and clicking an allowed window must not
    // re-dock on its own.
    if st.target.is_none() && !st.searching {
        return;
    }
    let root = find::root_of(hwnd);
    if st.target == Some(root) {
        return;
    }
    let Some(cand) = find::describe(root) else {
        return;
    };
    if st
        .allow
        .iter()
        .any(|s| spec_matches(s, &cand.class, cand.exe.as_deref()))
        // Only once the dock really is this window: focus must never be
        // reported for a window the tracker failed to attach to.
        && hook_target(st, &cand)
    {
        // The callback already reported Focused(false) for the old target;
        // without this the consumer would hide the overlay it just moved.
        st.send(TrackerEvent::Focused(true));
    }
}

// ---------------------------------------------------------------------------
// Interactive pick
// ---------------------------------------------------------------------------

fn start_pick(st: &mut HookState) {
    clear_pick(st);
    let now = Instant::now();
    st.pick_ignore = find::foreground();
    st.pick_started = Some(now);
    st.picking_until = Some(now + Duration::from_millis(u64::from(PICK_TIMEOUT_MS)));
    // SAFETY: no window, no TIMERPROC — WM_TIMER lands in this thread's queue.
    // SetTimer(None, ..) ignores the id passed in and returns a fresh one.
    let id = unsafe { SetTimer(None, 0, PICK_TIMEOUT_MS, None) };
    if id != 0 {
        st.pick_timer = Some(id);
    }
}

/// A foreground change while picking. Anything that fails a guard leaves the
/// pick armed — the user simply has not clicked the window they mean yet.
fn end_pick(st: &mut HookState, hwnd: isize) {
    if st.picking_until.is_some_and(|until| Instant::now() > until) {
        end_pick_timeout(st);
        return;
    }
    let root = find::root_of(hwnd);
    let settled = st.pick_started.is_some_and(|t| t.elapsed() >= PICK_SETTLE);
    let shell = find::class_of(root).is_none_or(|c| SHELL_CLASSES.contains(&c.as_str()));
    if !settled
        || Some(root) == st.pick_ignore
        || find::pid_tid(root).0 == st.own_pid
        || find::is_cloaked(root)
        || shell
    {
        return;
    }
    let Some(cand) = find::describe(root) else {
        return;
    };
    clear_pick(st);
    // The user chose it, so the allow list is not consulted.
    hook_target(st, &cand);
}

fn end_pick_timeout(st: &mut HookState) {
    clear_pick(st);
    st.send(TrackerEvent::NotFound);
}

fn clear_pick(st: &mut HookState) {
    if let Some(id) = st.pick_timer.take() {
        // SAFETY: the id is the one SetTimer returned for this thread.
        let _ = unsafe { KillTimer(None, id) };
    }
    st.picking_until = None;
    st.pick_started = None;
    st.pick_ignore = None;
}

// ---------------------------------------------------------------------------
// Hook callback
// ---------------------------------------------------------------------------

/// Runs on the pump thread inside message dispatch; must never unwind across
/// the FFI edge, block, or call back into the overlay.
unsafe extern "system" fn win_event_cb(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _event_tid: u32,
    _time: u32,
) {
    // An unwind out of an `extern "system"` fn aborts the whole overlay, so the
    // guarantee is enforced here rather than only documented.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        on_win_event(event, hwnd.0 as isize, id_object, id_child);
    }));
}

fn on_win_event(event: u32, hwnd: isize, id_object: i32, id_child: i32) {
    if id_object != OBJID_WINDOW.0 || id_child != CHILDID_SELF as i32 {
        return;
    }
    with_state(|st| {
        if event == EVENT_SYSTEM_FOREGROUND {
            // Everything a foreground change implies (pick resolution,
            // re-acquire, liveness, follow) runs on the pump, never here.
            st.pending_fg = Some(hwnd);
            defer(st, hwnd);
            if st.picking_until.is_none() && !st.searching {
                if let Some(target) = st.target {
                    st.send(TrackerEvent::Focused(find::root_of(hwnd) == target));
                }
            }
            return;
        }
        if st.target != Some(hwnd) {
            return;
        }
        // A loss is never handled here: unhooking and the search timer belong
        // to the pump, which sees the dead target on the deferred message.
        match event {
            EVENT_SYSTEM_MOVESIZEEND | EVENT_OBJECT_LOCATIONCHANGE | EVENT_OBJECT_SHOW => {
                if !emit_geometry(st) {
                    mark_lost(st);
                }
            }
            EVENT_SYSTEM_MINIMIZESTART | EVENT_OBJECT_HIDE | EVENT_OBJECT_CLOAKED => {
                st.send(TrackerEvent::Minimized)
            }
            EVENT_SYSTEM_MINIMIZEEND | EVENT_OBJECT_UNCLOAKED => {
                st.send(TrackerEvent::Restored);
                if !emit_geometry(st) {
                    mark_lost(st);
                }
            }
            EVENT_OBJECT_DESTROY => mark_lost(st),
            // MOVESIZESTART and the other ids inside the hooked ranges.
            _ => {}
        }
    });
}

/// Wakes the pump for work the callback must not do itself.
fn defer(st: &HookState, hwnd: isize) {
    let _ = post_thread(st.own_tid, WM_DEFERRED_FG, hwnd as usize);
}

/// A loss carries no foreground window: hwnd 0 keeps the dead target out of the
/// pick and follow guards, which read the posted hwnd.
fn mark_lost(st: &mut HookState) {
    st.lost_pending = true;
    defer(st, 0);
}

fn on_deferred_foreground(posted: isize) {
    with_state(|st| {
        // The message carries the hwnd its own callback saw, so two queued
        // deferrals resolve against the right window each; `pending_fg` is only
        // the fallback for a post whose payload did not survive (hwnd 0).
        let hwnd = if posted != 0 {
            st.pending_fg = None;
            posted
        } else {
            st.pending_fg.take().unwrap_or(0)
        };
        // Either the callback saw the target go, or its process died without a
        // DESTROY event ever reaching us.
        let gone = st.lost_pending || st.target.is_some_and(|t| !find::is_alive(t));
        st.lost_pending = false;
        if gone && st.target.is_some() {
            on_lost(st);
        }
        if st.picking_until.is_some() {
            end_pick(st, hwnd);
        } else {
            // A re-acquire wins over follow-focus, but a search that found
            // nothing must not suppress it: a search for a window that never
            // comes back would otherwise disable following for good.
            let reacquired = st.searching && try_reacquire(st);
            if !reacquired && st.follow_focus {
                follow(st, hwnd);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Bounds;

    fn cfg(allow: Vec<TargetSpec>, remembered: Option<TargetId>) -> TrackerConfig {
        TrackerConfig {
            allow,
            remembered,
            follow_focus: false,
        }
    }

    fn cfg_following(allow: Vec<TargetSpec>) -> TrackerConfig {
        TrackerConfig {
            allow,
            remembered: None,
            follow_focus: true,
        }
    }

    fn spec_of(cand: &Candidate) -> TargetSpec {
        TargetSpec {
            class: cand.class.clone(),
            exe: cand.exe.clone(),
        }
    }

    /// Blocks until an event satisfying `want` arrives or the budget runs out.
    fn wait_for(
        rx: &Receiver<TrackerEvent>,
        budget: Duration,
        want: impl Fn(&TrackerEvent) -> bool,
    ) -> Option<TrackerEvent> {
        let deadline = Instant::now() + budget;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(ev) if want(&ev) => return Some(ev),
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
        None
    }

    /// Everything the tracker sent inside `budget`, in order.
    fn collect_for(rx: &Receiver<TrackerEvent>, budget: Duration) -> Vec<TrackerEvent> {
        let deadline = Instant::now() + budget;
        let mut out = Vec::new();
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(ev) => out.push(ev),
                Err(_) => break,
            }
        }
        out
    }

    /// Index of the first event at or after `from` that matches — the ordered
    /// subsequence check the overlay's contract actually needs.
    fn next_at(
        stream: &[TrackerEvent],
        from: usize,
        want: impl Fn(&TrackerEvent) -> bool,
    ) -> Option<usize> {
        stream.get(from..)?.iter().position(want).map(|i| from + i)
    }

    #[test]
    fn start_and_stop_without_target() {
        let (handle, rx) = WindowsTracker::start(cfg(Vec::new(), None)).expect("tracker starts");
        // Right after start: proves the pump created its queue before the
        // handle got the thread id.
        handle.attach(None).expect("attach reaches the pump");
        assert_eq!(
            wait_for(&rx, Duration::from_secs(2), |_| true),
            Some(TrackerEvent::NotFound)
        );
        handle.stop();
    }

    #[test]
    fn attach_with_missing_remembered_leaves_tracker_searching() {
        let remembered = TargetId("win32:NoSuchClass_cuw|".to_string());
        let (handle, rx) = WindowsTracker::start(cfg(Vec::new(), Some(remembered)))
            .expect("tracker starts with a remembered target");
        // NotFound with a spec means "searching", not an error; the tracker
        // keeps its 2 s timer and must still stop promptly.
        assert_eq!(
            wait_for(&rx, Duration::from_secs(1), |_| true),
            Some(TrackerEvent::NotFound)
        );
        let started = Instant::now();
        handle.stop();
        assert!(started.elapsed() < Duration::from_secs(2), "stop hung");
    }

    #[test]
    fn attach_with_an_unparseable_id_reports_not_found() {
        let allow = vec![TargetSpec {
            class: "ConsoleWindowClass".to_string(),
            exe: None,
        }];
        let (handle, rx) = WindowsTracker::start(cfg(allow, None)).expect("tracker starts");
        // A malformed id names nothing; it must never widen to the allow list.
        handle
            .attach(Some(TargetId("not-a-target-id".to_string())))
            .expect("attach reaches the pump");
        assert_eq!(
            wait_for(&rx, Duration::from_secs(1), |_| true),
            Some(TrackerEvent::NotFound)
        );
        handle.stop();
    }

    #[test]
    fn detach_and_pick_reach_the_pump() {
        let (handle, rx) = WindowsTracker::start(cfg(Vec::new(), None)).expect("tracker starts");
        handle.detach().expect("detach reaches the pump");
        handle.pick_interactively().expect("pick reaches the pump");
        // Nothing can be picked inside the 300 ms settle window, so no event
        // is expected; the point is that neither command wedges the pump.
        assert_eq!(wait_for(&rx, Duration::from_millis(200), |_| true), None);
        handle.stop();
    }

    /// Drives the `follow_focus` branch of `on_deferred_foreground` with the
    /// message the callback posts. Nothing is allowed, so the branch must run
    /// to its own rejection and leave the pump answering commands.
    #[test]
    fn follow_focus_does_not_wedge_the_pump() {
        let (handle, rx) =
            WindowsTracker::start(cfg_following(Vec::new())).expect("tracker starts");
        for hwnd in [find::foreground().unwrap_or(0), 0x7fff_0001] {
            post_thread(handle.tid, WM_DEFERRED_FG, hwnd as usize).expect("the pump takes a post");
        }
        assert_eq!(wait_for(&rx, Duration::from_millis(200), |_| true), None);
        handle.attach(None).expect("attach reaches the pump");
        assert_eq!(
            wait_for(&rx, Duration::from_secs(2), |_| true),
            Some(TrackerEvent::NotFound)
        );
        handle.stop();
    }

    const CREATE_NEW_CONSOLE: u32 = 0x10;

    /// Kills the child on every exit path, panics included.
    struct Console(std::process::Child);

    impl Drop for Console {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A console app that does not read stdin: `cmd /c pause` returns the
    /// instant stdin is not a console, taking the window with it, and a child
    /// of `cmd` would survive killing `cmd` and hold the console open.
    fn spawn_console() -> Console {
        use std::os::windows::process::CommandExt;

        Console(
            std::process::Command::new("ping.exe")
                .args(["-n", "60", "127.0.0.1"])
                .creation_flags(CREATE_NEW_CONSOLE)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn a console process"),
        )
    }

    /// The class and the owning process both depend on the machine's default
    /// console host, so take whichever window is new since the spawn. Windows
    /// Terminal shows a placeholder window first and replaces it, so a window
    /// only counts once it has survived a poll with the same geometry.
    fn fresh_console(before: &[isize], budget: Duration) -> Option<Candidate> {
        let deadline = Instant::now() + budget;
        let mut settling: Option<(Candidate, Bounds)> = None;
        while Instant::now() < deadline {
            let found = find::candidates()
                .into_iter()
                .find(|c| !before.contains(&c.hwnd))
                .and_then(|c| match bounds::read(c.hwnd) {
                    bounds::Read::Bounds(b) if b.w > 0 && b.h > 0 => Some((c, b)),
                    _ => None,
                });
            match (settling.take(), found) {
                (Some((prev, b)), Some((c, now))) if prev.hwnd == c.hwnd && b == now => {
                    return Some(c)
                }
                (_, seen) => settling = seen,
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        None
    }

    /// When the default host puts the console in a process that already owns a
    /// window (Windows Terminal), the user's own window matches the same spec.
    /// Waiting for ours to take focus makes the ranking pick it. A console that
    /// died a moment earlier can leave the foreground bouncing, so callers that
    /// need the identity assert on the result.
    fn wait_for_foreground(hwnd: isize, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        while find::foreground() != Some(hwnd) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        find::foreground() == Some(hwnd)
    }

    /// Spawns its own console so the user's terminal is never touched: attach,
    /// move, destroy, re-acquire, stop.
    #[test]
    #[ignore = "opens console windows"]
    fn tracks_a_console_we_spawn() {
        use windows::Win32::Foundation::RECT;
        use windows::Win32::UI::WindowsAndMessaging::{
            GetWindowRect, SetWindowPos, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
        };

        let before: Vec<isize> = find::candidates().into_iter().map(|c| c.hwnd).collect();
        let first = spawn_console();
        let cand = fresh_console(&before, Duration::from_secs(8)).expect("a console window");
        // This test measures a move against one specific window, so the
        // ranking must land on ours and nothing else of the same class.
        assert!(
            wait_for_foreground(cand.hwnd, Duration::from_secs(5)),
            "the console we spawned never took focus: {cand:?}"
        );

        let (handle, rx) =
            WindowsTracker::start(cfg(vec![spec_of(&cand)], None)).expect("tracker starts");
        handle.attach(None).expect("attach reaches the pump");

        let attached = wait_for(&rx, Duration::from_secs(1), |e| {
            matches!(e, TrackerEvent::Attached(_))
        });
        assert_eq!(
            attached,
            Some(TrackerEvent::Attached(target_id(
                &cand.class,
                cand.exe.as_deref()
            ))),
            "no Attached for {cand:?}"
        );
        let first_bounds = match wait_for(&rx, Duration::from_secs(1), |e| {
            matches!(e, TrackerEvent::Bounds(_))
        }) {
            Some(TrackerEvent::Bounds(b)) => b,
            other => panic!("expected Bounds, got {other:?}"),
        };
        // Another window of the same class would report someone else's
        // geometry; fail here rather than measure the move against it.
        assert_eq!(
            bounds::read(cand.hwnd),
            bounds::Read::Bounds(first_bounds),
            "the tracker attached to a different window of the same class"
        );

        // Move the window we spawned (never the user's) by exactly 50 px.
        let mut rect = RECT::default();
        // SAFETY: the out pointer is a live local RECT.
        unsafe { GetWindowRect(find::hwnd(cand.hwnd), &mut rect) }.expect("console rect");
        // SAFETY: a move on a window our own child owns; no activation.
        unsafe {
            SetWindowPos(
                find::hwnd(cand.hwnd),
                None,
                rect.left + 50,
                rect.top + 50,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            )
        }
        .expect("move the console");

        let moved = wait_for(&rx, Duration::from_secs(1), |e| match e {
            TrackerEvent::Bounds(b) => b.x != first_bounds.x || b.y != first_bounds.y,
            _ => false,
        });
        match moved {
            Some(TrackerEvent::Bounds(b)) => {
                assert!(
                    (b.x - first_bounds.x - 50).abs() <= 2
                        && (b.y - first_bounds.y - 50).abs() <= 2,
                    "{b:?} did not follow {first_bounds:?} by 50 px"
                );
            }
            other => panic!("expected a moved Bounds, got {other:?}"),
        }

        drop(first);
        assert_eq!(
            wait_for(&rx, Duration::from_secs(2), |e| matches!(
                e,
                TrackerEvent::Lost
            )),
            Some(TrackerEvent::Lost),
            "no Lost after the console died"
        );

        // A second console re-acquires the remembered spec with no further
        // attach() call. A host that puts every console in one process (the
        // default Windows Terminal) leaves other windows of the same spec on
        // the machine, so which one is re-acquired is not fixed here;
        // `reacquires_a_window_of_its_own_class` pins that down exactly.
        let second = spawn_console();
        assert_eq!(
            wait_for(&rx, Duration::from_secs(6), |e| matches!(
                e,
                TrackerEvent::Attached(_)
            )),
            Some(TrackerEvent::Attached(target_id(
                &cand.class,
                cand.exe.as_deref()
            ))),
            "no re-acquire after a new console appeared"
        );
        drop(second);

        let started = Instant::now();
        handle.stop();
        assert!(started.elapsed() < Duration::from_secs(3), "stop hung");
    }

    /// `follow_focus` end to end, across processes: `WINEVENT_SKIPOWNPROCESS`
    /// hides our own foreground changes from the global hook, so the second
    /// console taking focus is what the hook actually sees. The order asserted
    /// here is the one the overlay codes against — the leading `Focused(false)`
    /// hides it, so `Focused(true)` must follow the re-attach or the overlay
    /// stays hidden over the window it just moved to.
    #[test]
    #[ignore = "opens console windows"]
    fn follow_focus_moves_to_a_second_console() {
        let before: Vec<isize> = find::candidates().into_iter().map(|c| c.hwnd).collect();
        let first = spawn_console();
        let cand = fresh_console(&before, Duration::from_secs(8)).expect("a console window");
        // Any window of this spec is a fine starting dock — the assertions
        // below are about the handover to the second console, not identity.
        wait_for_foreground(cand.hwnd, Duration::from_secs(5));

        let (handle, rx) =
            WindowsTracker::start(cfg_following(vec![spec_of(&cand)])).expect("tracker starts");
        handle.attach(None).expect("attach reaches the pump");
        let want = TrackerEvent::Attached(target_id(&cand.class, cand.exe.as_deref()));
        assert_eq!(
            wait_for(&rx, Duration::from_secs(2), |e| matches!(
                e,
                TrackerEvent::Attached(_)
            )),
            Some(want.clone()),
            "no Attached for the first console {cand:?}"
        );
        assert!(
            wait_for(&rx, Duration::from_secs(1), |e| matches!(
                e,
                TrackerEvent::Bounds(_)
            ))
            .is_some(),
            "no Bounds for the first console"
        );

        // A second console of the same spec takes the foreground on its own.
        let second = spawn_console();
        let stream = collect_for(&rx, Duration::from_secs(6));
        drop(second);
        drop(first);

        let bad = |what: &str| format!("{what} after the second console took focus: {stream:?}");
        let unfocused = next_at(&stream, 0, |e| *e == TrackerEvent::Focused(false))
            .unwrap_or_else(|| panic!("{}", bad("no Focused(false)")));
        let attached = next_at(&stream, unfocused + 1, |e| {
            matches!(e, TrackerEvent::Attached(_))
        })
        .unwrap_or_else(|| panic!("{}", bad("no re-attach")));
        assert_eq!(stream[attached], want, "{}", bad("wrong Attached"));
        let bounds = next_at(&stream, attached + 1, |e| {
            matches!(e, TrackerEvent::Bounds(_))
        })
        .unwrap_or_else(|| panic!("{}", bad("no Bounds")));
        next_at(&stream, bounds + 1, |e| *e == TrackerEvent::Focused(true))
            .unwrap_or_else(|| panic!("{}", bad("no Focused(true)")));

        let started = Instant::now();
        handle.stop();
        assert!(started.elapsed() < Duration::from_secs(3), "stop hung");
    }

    /// A plain top-level window of our own, on a thread that pumps so a
    /// cross-thread `SetWindowPos` or `WM_CLOSE` never blocks.
    struct TestWindow {
        hwnd: isize,
        tid: u32,
        join: Option<std::thread::JoinHandle<()>>,
    }

    impl TestWindow {
        fn open(class: &str) -> Option<TestWindow> {
            use windows::core::PCWSTR;
            use windows::Win32::UI::WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, RegisterClassW, WINDOW_EX_STYLE, WNDCLASSW,
                WS_OVERLAPPEDWINDOW, WS_VISIBLE,
            };

            /// `DefWindowProcW` is a wrapper, not a `WNDPROC`, so the class
            /// needs a real one; the default handling is all a probe needs.
            unsafe extern "system" fn probe_proc(
                h: HWND,
                msg: u32,
                w: WPARAM,
                l: LPARAM,
            ) -> windows::Win32::Foundation::LRESULT {
                // SAFETY: forwarding the arguments the window manager passed in.
                unsafe { DefWindowProcW(h, msg, w, l) }
            }

            let name: Vec<u16> = class.encode_utf16().chain([0]).collect();
            let (tx, rx) = mpsc::channel();
            let join = std::thread::spawn(move || {
                // `name` lives on this thread's stack until the pump ends, so
                // the class name outlives both the class and the window.
                let cls = WNDCLASSW {
                    lpfnWndProc: Some(probe_proc),
                    lpszClassName: PCWSTR(name.as_ptr()),
                    ..Default::default()
                };
                // SAFETY: `cls` and its name buffer are live locals; a second
                // registration of the same name simply returns 0.
                unsafe { RegisterClassW(&cls) };
                // SAFETY: null instance/parent/menu; the class was just registered.
                let created = unsafe {
                    CreateWindowExW(
                        WINDOW_EX_STYLE(0),
                        PCWSTR(name.as_ptr()),
                        PCWSTR(name.as_ptr()),
                        WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                        140,
                        140,
                        320,
                        200,
                        None,
                        None,
                        None,
                        None,
                    )
                };
                // SAFETY: no arguments, cannot fail.
                let tid = unsafe { GetCurrentThreadId() };
                let hwnd = created.ok().map(|h| h.0 as isize);
                if tx.send(hwnd.map(|h| (h, tid))).is_err() || hwnd.is_none() {
                    return;
                }
                let mut msg = MSG::default();
                // SAFETY: `msg` is a live local; the loop ends on WM_QUIT.
                while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
                    // SAFETY: `msg` came from GetMessageW and outlives both calls.
                    unsafe {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            });
            let (hwnd, tid) = rx.recv_timeout(Duration::from_secs(2)).ok().flatten()?;
            Some(TestWindow {
                hwnd,
                tid,
                join: Some(join),
            })
        }

        /// Destroys the window (`DefWindowProcW` turns `WM_CLOSE` into
        /// `DestroyWindow` on the owning thread) and ends its pump.
        fn close(&mut self) {
            use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};

            // SAFETY: a posted message; the payload is a plain integer.
            let _ = unsafe {
                PostMessageW(Some(find::hwnd(self.hwnd)), WM_CLOSE, WPARAM(0), LPARAM(0))
            };
            let _ = post_thread(self.tid, WM_QUIT, 0);
            if let Some(join) = self.join.take() {
                join_capped(join, Duration::from_secs(2));
            }
        }
    }

    impl Drop for TestWindow {
        fn drop(&mut self) {
            if self.join.is_some() {
                self.close();
            }
        }
    }

    /// Re-acquire, exactly: the class is ours alone, so the only window the
    /// tracker can come back with is the second one we open. Our own
    /// foreground changes are invisible to the global hook
    /// (`WINEVENT_SKIPOWNPROCESS`), so this is the 2 s search timer's path.
    #[test]
    #[ignore = "opens windows"]
    fn reacquires_a_window_of_its_own_class() {
        const CLASS: &str = "cuw_tracker_probe_window";

        let mut first = TestWindow::open(CLASS).expect("a window of our own");
        let cand = find::describe(first.hwnd).expect("the window enumerates");
        assert_eq!(cand.class, CLASS);
        assert_eq!(
            find::candidates()
                .iter()
                .filter(|c| c.class == CLASS)
                .count(),
            1,
            "the probe class must belong to exactly one window"
        );

        let allow = vec![TargetSpec {
            class: cand.class.clone(),
            exe: cand.exe.clone(),
        }];
        let (handle, rx) = WindowsTracker::start(cfg(allow, None)).expect("tracker starts");
        handle.attach(None).expect("attach reaches the pump");
        assert_eq!(
            wait_for(&rx, Duration::from_secs(2), |e| matches!(
                e,
                TrackerEvent::Attached(_)
            )),
            Some(TrackerEvent::Attached(target_id(
                &cand.class,
                cand.exe.as_deref()
            ))),
            "no Attached for {cand:?}"
        );

        first.close();
        assert_eq!(
            wait_for(&rx, Duration::from_secs(2), |e| matches!(
                e,
                TrackerEvent::Lost
            )),
            Some(TrackerEvent::Lost),
            "no Lost after the window was destroyed"
        );

        let mut second = TestWindow::open(CLASS).expect("a second window of our own");
        assert_eq!(
            wait_for(&rx, Duration::from_secs(6), |e| matches!(
                e,
                TrackerEvent::Attached(_)
            )),
            Some(TrackerEvent::Attached(target_id(
                &cand.class,
                cand.exe.as_deref()
            ))),
            "the search timer did not re-acquire the remembered spec"
        );
        second.close();

        let started = Instant::now();
        handle.stop();
        assert!(started.elapsed() < Duration::from_secs(3), "stop hung");
    }

    /// An explicit `attach(Some(id))` for a window that is not open must keep
    /// searching until it appears. Empty allow and nothing remembered is what
    /// makes this a regression test: the search runs off `remembered` alone, so
    /// without `do_attach` promoting the explicit spec the first 2 s tick finds
    /// no spec, cancels the search, and the `Attached` below never arrives.
    #[test]
    #[ignore = "opens windows"]
    fn an_explicit_attach_searches_until_the_window_appears() {
        const CLASS: &str = "cuw_tracker_explicit_window";

        let mut probe = TestWindow::open(CLASS).expect("a window of our own");
        let cand = find::describe(probe.hwnd).expect("the window enumerates");
        assert_eq!(cand.class, CLASS);
        let id = target_id(&cand.class, cand.exe.as_deref());
        probe.close();

        let (handle, rx) = WindowsTracker::start(cfg(Vec::new(), None)).expect("tracker starts");
        handle
            .attach(Some(id.clone()))
            .expect("attach reaches the pump");
        assert_eq!(
            wait_for(&rx, Duration::from_secs(1), |_| true),
            Some(TrackerEvent::NotFound),
            "an explicit attach to a window that is not open must report NotFound"
        );

        let mut second = TestWindow::open(CLASS).expect("a second window of our own");
        assert_eq!(
            wait_for(&rx, Duration::from_secs(6), |e| matches!(
                e,
                TrackerEvent::Attached(_)
            )),
            Some(TrackerEvent::Attached(id)),
            "the search did not survive its first tick"
        );
        second.close();

        let started = Instant::now();
        handle.stop();
        assert!(started.elapsed() < Duration::from_secs(3), "stop hung");
    }

    /// The pick guards, driven rather than read: nothing resolves inside the
    /// 300 ms settle window, a window of our own process never resolves at all,
    /// and the window that does resolve is not in the (empty) allow list —
    /// a pick is the user's choice, so the allow list is not consulted.
    #[test]
    #[ignore = "opens console windows"]
    fn a_pick_rejects_our_own_window_and_takes_a_foreign_one() {
        const CLASS: &str = "cuw_tracker_pick_window";

        let first = TestWindow::open(CLASS).expect("a window of our own");
        let second = TestWindow::open(CLASS).expect("a second window of our own");
        let (handle, rx) = WindowsTracker::start(cfg(Vec::new(), None)).expect("tracker starts");
        // Whichever of the two is not the foreground the pick is about to
        // remember as `pick_ignore`: the own-process guard is then the only one
        // that can reject it, so the assertion below is about that guard.
        let ours = if find::foreground() == Some(second.hwnd) {
            first.hwnd
        } else {
            second.hwnd
        };
        handle.pick_interactively().expect("pick reaches the pump");

        // Inside the settle window: a closing tray menu re-activates the
        // previous window before the user can click anything.
        post_thread(handle.tid, WM_DEFERRED_FG, ours as usize).expect("the pump takes a post");
        assert_eq!(
            wait_for(&rx, PICK_SETTLE, |_| true),
            None,
            "picked too early"
        );
        // Settled, but still ours: the overlay's own process is never a dock.
        post_thread(handle.tid, WM_DEFERRED_FG, ours as usize).expect("the pump takes a post");
        assert_eq!(
            wait_for(&rx, Duration::from_millis(400), |_| true),
            None,
            "picked a window of our own process"
        );

        let before: Vec<isize> = find::candidates().into_iter().map(|c| c.hwnd).collect();
        let console = spawn_console();
        let cand = fresh_console(&before, Duration::from_secs(5)).expect("a console window");
        post_thread(handle.tid, WM_DEFERRED_FG, cand.hwnd as usize).expect("the pump takes a post");
        assert_eq!(
            wait_for(&rx, Duration::from_secs(2), |e| matches!(
                e,
                TrackerEvent::Attached(_)
            )),
            Some(TrackerEvent::Attached(target_id(
                &cand.class,
                cand.exe.as_deref()
            ))),
            "the pick did not resolve to the console {cand:?}"
        );
        drop(console);
        drop(second);
        drop(first);

        let started = Instant::now();
        handle.stop();
        assert!(started.elapsed() < Duration::from_secs(3), "stop hung");
    }

    /// The same `follow_focus` contract as the console test, but deterministic:
    /// the pump is handed exactly the message its callback would post, so the
    /// three events are the first three and their order is exact.
    #[test]
    #[ignore = "opens windows"]
    fn follow_focus_reattaches_and_refocuses() {
        const CLASS: &str = "cuw_tracker_follow_window";

        let window = TestWindow::open(CLASS).expect("a window of our own");
        let cand = find::describe(window.hwnd).expect("the window enumerates");
        assert_eq!(cand.class, CLASS);

        let (handle, rx) =
            WindowsTracker::start(cfg_following(vec![spec_of(&cand)])).expect("tracker starts");
        // Nothing is attached, so this is purely the follow branch.
        post_thread(handle.tid, WM_DEFERRED_FG, window.hwnd as usize)
            .expect("the pump takes a post");

        let budget = Duration::from_secs(2);
        assert_eq!(
            rx.recv_timeout(budget),
            Ok(TrackerEvent::Attached(target_id(
                &cand.class,
                cand.exe.as_deref()
            ))),
            "follow did not attach to {cand:?}"
        );
        assert!(
            matches!(rx.recv_timeout(budget), Ok(TrackerEvent::Bounds(_))),
            "no Bounds after the follow re-attach"
        );
        assert_eq!(
            rx.recv_timeout(budget),
            Ok(TrackerEvent::Focused(true)),
            "the followed window was never reported focused"
        );

        drop(window);
        let started = Instant::now();
        handle.stop();
        assert!(started.elapsed() < Duration::from_secs(3), "stop hung");
    }
}
