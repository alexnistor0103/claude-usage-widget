//! Run loop thread and `MacosTracker` (M5.3–M5.5).
//!
//! One dedicated thread owns every CoreGraphics and AppKit call here, exactly
//! as the Windows pump owns every Win32 one. Handle methods queue a command and
//! signal a `CFRunLoopSource` — the analogue of `PostThreadMessageW` — and
//! every result comes back as a `TrackerEvent` (plan §6).
//!
//! The loop is `CFRunLoopRunInMode(.., return_after_source_handled: true)` in a
//! `while`, not `CFRunLoopRun`: the timeout *is* the poll interval, and any
//! source — a queued command, or an AX move notification — cuts it short, so
//! the AX upgrade needs no second mechanism. The AX callback sets one flag and
//! returns; the read and the send happen on the tick, never inside a C callback
//! (the rule the Windows hook callback follows).
//!
//! Focus is polled, not observed. `NSWorkspace`'s activation notification is
//! documented to reach an observer, with no promise about which thread's run
//! loop delivers it to a background registrant; a poll on the tick we already
//! run is honest, and a wrong threading assumption would wedge the tracker.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use objc2_core_foundation::{
    kCFRunLoopDefaultMode, CFRetained, CFRunLoop, CFRunLoopMode, CFRunLoopSource,
    CFRunLoopSourceContext, CFTimeInterval,
};

use super::info::{self, Change, Phase};
use super::{ax, bounds, find};
use crate::geometry::{
    macos_target_id, parse_target_id, rank_candidates, spec_matches, Candidate, Coalescer,
    MACOS_SHELL_BUNDLES,
};
use crate::{TargetId, TargetSpec, TrackerConfig, TrackerEvent, TrackerHandle, WindowTracker};

/// The permission-free rate plan §6 settles on: laggy on a drag, acceptable.
const POLL: CFTimeInterval = 0.1;
/// Once an AX observer has actually delivered, the run loop returns on every
/// move and resize, so the tick is only catching focus changes and a window
/// that vanished. An observer that never fires never earns this.
const HEARTBEAT: CFTimeInterval = 0.25;

const SEARCH_INTERVAL: Duration = Duration::from_secs(2);
const PICK_TIMEOUT: Duration = Duration::from_secs(10);
/// A menu closing re-activates the previously active application before the
/// user clicks anything.
const PICK_SETTLE: Duration = Duration::from_millis(300);
const START_TIMEOUT: Duration = Duration::from_secs(2);
const JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// The macOS implementation of [`WindowTracker`].
pub struct MacosTracker;

enum Cmd {
    Attach(Option<TargetId>),
    Pick,
    Detach,
    Stop,
}

/// Owns the run loop thread; every method is a queue push plus a wake-up.
pub struct Handle {
    queue: Arc<Mutex<VecDeque<Cmd>>>,
    waker: Arc<Waker>,
    join: Option<JoinHandle<()>>,
    stopped: bool,
}

impl WindowTracker for MacosTracker {
    type Handle = Handle;

    fn start(cfg: TrackerConfig) -> anyhow::Result<(Handle, Receiver<TrackerEvent>)> {
        let (tx, rx) = mpsc::channel();
        let queue: Arc<Mutex<VecDeque<Cmd>>> = Arc::new(Mutex::new(VecDeque::new()));
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread_queue = Arc::clone(&queue);
        let join = std::thread::Builder::new()
            .name("cuw-tracker".to_string())
            .spawn(move || run_main(cfg, tx, thread_queue, ready_tx))
            .context("spawning the tracker thread")?;
        // The waker arrives only once the run loop holds the command source, so
        // the first command can never be signalled into nothing.
        let waker = ready_rx
            .recv_timeout(START_TIMEOUT)
            .map_err(|_| anyhow!("tracker thread did not report its run loop"))?;
        Ok((
            Handle {
                queue,
                waker: Arc::new(waker),
                join: Some(join),
                stopped: false,
            },
            rx,
        ))
    }
}

impl Handle {
    fn post(&self, cmd: Cmd) -> anyhow::Result<()> {
        if self.join.as_ref().is_some_and(JoinHandle::is_finished) {
            return Err(anyhow!("tracker thread is not accepting commands"));
        }
        lock(&self.queue).push_back(cmd);
        self.waker.wake();
        Ok(())
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
        let _ = self.post(Cmd::Stop);
        if let Some(join) = self.join.take() {
            join_capped(join, JOIN_TIMEOUT);
        }
        self.stopped = true;
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.stopped && self.join.is_some() {
            let _ = self.post(Cmd::Stop);
        }
    }
}

/// Joins without ever blocking the caller on a wedged run loop.
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

fn default_mode() -> Option<&'static CFRunLoopMode> {
    // SAFETY: a CoreFoundation string constant, live for the life of the process.
    unsafe { kCFRunLoopDefaultMode }
}

// ---------------------------------------------------------------------------
// Waking the run loop from another thread
// ---------------------------------------------------------------------------

/// The only thing a foreign thread may touch. Signalling a source and waking a
/// run loop are the two CFRunLoop calls documented as thread-safe; everything
/// else in this file runs on the tracker thread.
struct Waker {
    run_loop: CFRetained<CFRunLoop>,
    source: CFRetained<CFRunLoopSource>,
}

// SAFETY: `wake` is the whole API, and both of its calls are the documented
// cross-thread entry points into another thread's run loop.
unsafe impl Send for Waker {}
// SAFETY: as above; `wake` takes `&self` and mutates nothing on this side.
unsafe impl Sync for Waker {}

impl Waker {
    fn wake(&self) {
        self.source.signal();
        self.run_loop.wake_up();
    }
}

/// A signalled source is what lets a command cut a `run_in_mode` short. The
/// perform callback is empty on purpose: draining the queue belongs to the loop
/// body, not to a C callback that must never unwind or block.
fn command_source() -> Option<CFRetained<CFRunLoopSource>> {
    let mut context = CFRunLoopSourceContext {
        version: 0,
        info: std::ptr::null_mut(),
        retain: None,
        release: None,
        copyDescription: None,
        equal: None,
        hash: None,
        schedule: None,
        cancel: None,
        perform: Some(perform_nothing),
    };
    // SAFETY: `context` is a live local, which CoreFoundation copies.
    unsafe { CFRunLoopSource::new(None, 0, &mut context) }
}

unsafe extern "C-unwind" fn perform_nothing(_info: *mut c_void) {}

// ---------------------------------------------------------------------------
// Tracker thread
// ---------------------------------------------------------------------------

struct State {
    tx: Sender<TrackerEvent>,
    /// `CGWindowID` of the docked window.
    target: Option<isize>,
    target_pid: Option<i32>,
    coalescer: Coalescer,
    allow: Vec<TargetSpec>,
    remembered: Option<TargetSpec>,
    follow_focus: bool,
    /// Derived rather than observed: a poll gets no minimize or destroy event.
    phase: Phase,
    focused: Option<bool>,
    front_pid: Option<i32>,
    searching: bool,
    last_search: Instant,
    picking_until: Option<Instant>,
    pick_started: Option<Instant>,
    pick_ignore: Option<i32>,
    /// The AX upgrade, re-tried on every attach so a grant given mid-session
    /// takes effect without a restart.
    observer: Option<ax::Observer>,
    dead: bool,
    stop: bool,
}

impl State {
    /// A dropped receiver is the only send failure; it means "stop".
    fn send(&mut self, ev: TrackerEvent) {
        if self.dead {
            return;
        }
        if self.tx.send(ev).is_err() {
            self.dead = true;
            self.stop = true;
        }
    }

    fn remembered_id(&self) -> Option<TargetId> {
        self.remembered.as_ref().map(|s| macos_target_id(&s.class))
    }

    fn tick_budget(&self) -> CFTimeInterval {
        let observing = self.observer.as_ref().is_some_and(ax::Observer::has_fired);
        if observing && self.picking_until.is_none() && !self.searching {
            HEARTBEAT
        } else {
            POLL
        }
    }
}

fn run_main(
    cfg: TrackerConfig,
    tx: Sender<TrackerEvent>,
    queue: Arc<Mutex<VecDeque<Cmd>>>,
    ready: Sender<Waker>,
) {
    let (Some(run_loop), Some(source)) = (CFRunLoop::current(), command_source()) else {
        return;
    };
    run_loop.add_source(Some(&source), default_mode());
    let waker = Waker {
        run_loop: run_loop.clone(),
        source: source.clone(),
    };
    if ready.send(waker).is_err() {
        return;
    }

    let remembered_id = cfg.remembered.clone();
    let mut st = State {
        tx,
        target: None,
        target_pid: None,
        coalescer: Coalescer::default(),
        allow: cfg.allow,
        remembered: cfg.remembered.as_ref().and_then(parse_target_id),
        follow_focus: cfg.follow_focus,
        phase: Phase::Gone,
        focused: None,
        front_pid: find::frontmost_pid(),
        searching: false,
        last_search: Instant::now(),
        picking_until: None,
        pick_started: None,
        pick_ignore: None,
        observer: None,
        dead: false,
        stop: false,
    };
    if remembered_id.is_some() {
        do_attach(&mut st, remembered_id);
    }

    while !st.stop {
        // Returns as soon as a command or an AX notification is handled, else
        // after the tick budget.
        CFRunLoop::run_in_mode(default_mode(), st.tick_budget(), true);
        drain_queue(&queue, &mut st);
        if st.stop {
            break;
        }
        tick(&mut st);
    }

    // Before the run loop goes: the observer takes its source back out.
    st.observer = None;
    run_loop.remove_source(Some(&source), default_mode());
}

fn drain_queue(queue: &Mutex<VecDeque<Cmd>>, st: &mut State) {
    while let Some(cmd) = lock(queue).pop_front() {
        match cmd {
            Cmd::Attach(id) => do_attach(st, id),
            Cmd::Pick => start_pick(st),
            Cmd::Detach => do_detach(st),
            Cmd::Stop => st.stop = true,
        }
    }
}

fn tick(st: &mut State) {
    if st.target.is_some() && !emit_geometry(st) {
        on_lost(st);
    }
    let front = find::frontmost_pid();
    if front != st.front_pid {
        st.front_pid = front;
        on_front_change(st, front);
    }
    if st.picking_until.is_some_and(|until| Instant::now() > until) {
        end_pick_timeout(st);
    }
    if st.searching && st.last_search.elapsed() >= SEARCH_INTERVAL {
        st.last_search = Instant::now();
        try_reacquire(st);
    }
}

// ---------------------------------------------------------------------------
// Attach / detach
// ---------------------------------------------------------------------------

fn do_attach(st: &mut State, id: Option<TargetId>) {
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
    // before the attach is attempted: otherwise the first search tick would
    // find no spec and cancel the search this NotFound just promised.
    if let Some(spec) = explicit {
        st.remembered = Some(spec);
    }
    match rank_candidates(&cands, &allow, fg, prefer.as_ref()) {
        Some(i) => {
            attach_to(st, &cands[i]);
        }
        None => {
            st.send(TrackerEvent::NotFound);
            if had_spec {
                start_search(st);
            }
        }
    }
}

/// `true` when the dock is the given candidate afterwards. A caller that
/// reports focus must gate it on this: the window can die between enumeration
/// and the attach.
fn attach_to(st: &mut State, cand: &Candidate) -> bool {
    let Some(pid) = find::pid_of(cand.hwnd) else {
        // Died between enumeration and here; the existing dock stays untouched.
        if !st.searching {
            st.send(TrackerEvent::NotFound);
            if st.remembered.is_some() {
                start_search(st);
            }
        }
        return false;
    };
    st.observer = None;
    st.target = Some(cand.hwnd);
    st.target_pid = Some(pid);
    // Bundle id only, exactly what `macos_target_id` round-trips: the exe is
    // display data here, and an app that renames its binary in an update must
    // still be the same dock target.
    st.remembered = Some(TargetSpec {
        class: cand.class.clone(),
        exe: None,
    });
    st.phase = Phase::Visible;
    st.focused = None;
    stop_search(st);
    st.coalescer.reset();
    st.send(TrackerEvent::Attached(macos_target_id(&cand.class)));
    st.observer = ax::observe(pid);
    if !emit_geometry(st) {
        on_lost(st);
        return false;
    }
    true
}

fn do_detach(st: &mut State) {
    st.observer = None;
    st.target = None;
    st.target_pid = None;
    st.phase = Phase::Gone;
    st.focused = None;
    stop_search(st);
    clear_pick(st);
}

/// `false` when the target is gone: the caller decides what follows, because
/// the attach path and the tick want different follow-ups.
fn emit_geometry(st: &mut State) -> bool {
    let Some(target) = st.target else {
        return true;
    };
    let read = bounds::read(target);
    let now = match read {
        bounds::Read::Bounds(_) => Phase::Visible,
        bounds::Read::Iconic => Phase::Minimized,
        bounds::Read::Gone => Phase::Gone,
    };
    match info::change(st.phase, now) {
        Change::None => {}
        Change::Minimized => st.send(TrackerEvent::Minimized),
        Change::Restored => {
            st.coalescer.reset();
            st.send(TrackerEvent::Restored);
        }
        Change::Lost => {
            st.phase = now;
            return false;
        }
    }
    st.phase = now;
    if let bounds::Read::Bounds(b) = read {
        if st.coalescer.push(b) {
            st.send(TrackerEvent::Bounds(b));
        }
    }
    true
}

fn on_lost(st: &mut State) {
    st.observer = None;
    st.target = None;
    st.target_pid = None;
    st.phase = Phase::Gone;
    st.focused = None;
    start_search(st);
    st.send(TrackerEvent::Lost);
}

// ---------------------------------------------------------------------------
// Searching and focus
// ---------------------------------------------------------------------------

fn start_search(st: &mut State) {
    st.searching = true;
    st.last_search = Instant::now();
}

fn stop_search(st: &mut State) {
    st.searching = false;
}

/// Re-acquire is restricted to the remembered bundle id: a lost target comes
/// back as itself, never as some other allowed application (M5.5). `true` when
/// it came back.
fn try_reacquire(st: &mut State) -> bool {
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
        Some(i) => attach_to(st, &cands[i]),
        None => false,
    }
}

/// The frontmost application changed. Everything a foreground change implies
/// runs here, on the tick — the AX callback never does any of it.
fn on_front_change(st: &mut State, front: Option<i32>) {
    if st.picking_until.is_some() {
        end_pick(st, front);
        return;
    }
    if !st.searching {
        if let Some(pid) = st.target_pid {
            send_focused(st, front == Some(pid));
        }
    }
    // A re-acquire wins over follow-focus, but a search that found nothing must
    // not suppress it: a search for a window that never comes back would
    // otherwise disable following for good.
    let reacquired = st.searching && try_reacquire(st);
    if !reacquired && st.follow_focus {
        follow(st, front);
    }
}

/// `follow_focus`: an allowed application that comes to the front takes over.
fn follow(st: &mut State, front: Option<i32>) {
    let Some(pid) = front else {
        return;
    };
    if st.target_pid == Some(pid) {
        return;
    }
    let Some(id) = info::front_window_of(pid, &find::on_screen()) else {
        return;
    };
    let Some(cand) = find::describe(id as isize) else {
        return;
    };
    if st
        .allow
        .iter()
        .any(|s| spec_matches(s, &cand.class, cand.exe.as_deref()))
        // Only once the dock really is this window: focus must never be
        // reported for a window the tracker failed to attach to.
        && attach_to(st, &cand)
    {
        // The tick already reported Focused(false) for the old target; without
        // this the consumer would hide the overlay it just moved.
        send_focused(st, true);
    }
}

fn send_focused(st: &mut State, focused: bool) {
    if st.focused == Some(focused) {
        return;
    }
    st.focused = Some(focused);
    st.send(TrackerEvent::Focused(focused));
}

// ---------------------------------------------------------------------------
// Interactive pick
// ---------------------------------------------------------------------------

fn start_pick(st: &mut State) {
    clear_pick(st);
    let now = Instant::now();
    st.pick_ignore = find::frontmost_pid();
    st.pick_started = Some(now);
    st.picking_until = Some(now + PICK_TIMEOUT);
}

/// The frontmost application changed while picking. Anything that fails a guard
/// leaves the pick armed — the user simply has not clicked what they mean yet.
fn end_pick(st: &mut State, front: Option<i32>) {
    if st.picking_until.is_some_and(|until| Instant::now() > until) {
        end_pick_timeout(st);
        return;
    }
    let Some(pid) = front else {
        return;
    };
    let settled = st.pick_started.is_some_and(|t| t.elapsed() >= PICK_SETTLE);
    if !settled || Some(pid) == st.pick_ignore || Some(pid) == own_pid() {
        return;
    }
    let Some(id) = info::front_window_of(pid, &find::on_screen()) else {
        return;
    };
    let Some(cand) = find::describe(id as isize) else {
        return;
    };
    if MACOS_SHELL_BUNDLES.contains(&cand.class.as_str()) {
        return;
    }
    clear_pick(st);
    // The user chose it, so the allow list is not consulted.
    attach_to(st, &cand);
}

fn end_pick_timeout(st: &mut State) {
    clear_pick(st);
    st.send(TrackerEvent::NotFound);
}

fn clear_pick(st: &mut State) {
    st.picking_until = None;
    st.pick_started = None;
    st.pick_ignore = None;
}

fn own_pid() -> Option<i32> {
    i32::try_from(find::own_pid()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(allow: Vec<TargetSpec>, remembered: Option<TargetId>) -> TrackerConfig {
        TrackerConfig {
            allow,
            remembered,
            follow_focus: false,
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

    #[test]
    fn start_and_stop_without_target() {
        let (handle, rx) = MacosTracker::start(cfg(Vec::new(), None)).expect("tracker starts");
        // Right after start: proves the run loop took its command source before
        // the handle got its waker.
        handle.attach(None).expect("attach reaches the run loop");
        assert_eq!(
            wait_for(&rx, Duration::from_secs(2), |_| true),
            Some(TrackerEvent::NotFound)
        );
        let started = Instant::now();
        handle.stop();
        assert!(started.elapsed() < Duration::from_secs(3), "stop hung");
    }

    #[test]
    fn attach_with_missing_remembered_leaves_tracker_searching() {
        let remembered = TargetId("macos:com.local.no-such-app".to_string());
        let (handle, rx) = MacosTracker::start(cfg(Vec::new(), Some(remembered)))
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
            class: "com.apple.Terminal".to_string(),
            exe: None,
        }];
        let (handle, rx) = MacosTracker::start(cfg(allow, None)).expect("tracker starts");
        // A malformed id names nothing; it must never widen to the allow list.
        handle
            .attach(Some(TargetId("not-a-target-id".to_string())))
            .expect("attach reaches the run loop");
        assert_eq!(
            wait_for(&rx, Duration::from_secs(1), |_| true),
            Some(TrackerEvent::NotFound)
        );
        handle.stop();
    }

    #[test]
    fn detach_and_pick_reach_the_run_loop() {
        let (handle, rx) = MacosTracker::start(cfg(Vec::new(), None)).expect("tracker starts");
        handle.detach().expect("detach reaches the run loop");
        handle
            .pick_interactively()
            .expect("pick reaches the run loop");
        // Nothing can be picked inside the 300 ms settle window, so no event is
        // expected; the point is that neither command wedges the run loop.
        assert_eq!(wait_for(&rx, Duration::from_millis(200), |_| true), None);
        handle.stop();
    }

    /// Attaches to whatever is frontmost, so run it from a logged-in desktop
    /// with a terminal open: you should see `Attached` then `Bounds`, and
    /// moving that window should produce further `Bounds` that follow it.
    #[test]
    #[ignore = "needs a windowed session"]
    fn attaching_to_the_frontmost_application_reports_its_geometry() {
        let front = find::foreground().expect("a foreground window");
        let cand = find::describe(front).expect("the foreground window describes");
        let allow = vec![TargetSpec {
            class: cand.class.clone(),
            exe: None,
        }];
        let (handle, rx) = MacosTracker::start(cfg(allow, None)).expect("tracker starts");
        handle.attach(None).expect("attach reaches the run loop");

        assert_eq!(
            wait_for(&rx, Duration::from_secs(2), |e| matches!(
                e,
                TrackerEvent::Attached(_)
            )),
            Some(TrackerEvent::Attached(macos_target_id(&cand.class))),
            "no Attached for {cand:?}"
        );
        let bounds = wait_for(&rx, Duration::from_secs(2), |e| {
            matches!(e, TrackerEvent::Bounds(_))
        });
        match bounds {
            Some(TrackerEvent::Bounds(b)) => {
                assert!(b.w > 0 && b.h > 0, "{b:?}");
                assert!(b.scale >= 1.0, "{b:?}");
            }
            other => panic!("expected Bounds, got {other:?}"),
        }
        handle.stop();
    }
}
