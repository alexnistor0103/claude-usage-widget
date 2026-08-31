//! The Accessibility upgrade path (M5.4). Hand-declared bindings rather than
//! another crate: the whole surface is six functions and two string constants,
//! and nothing in the overlay's lock covers AX.
//!
//! AX is used purely as a *trigger*. An observer on the target application's
//! move and resize notifications wakes the tracker's run loop; the geometry
//! still comes from the window list, so there is no `CGWindowID`-to-
//! `AXUIElement` mapping to get wrong (that needs a private API) and no second
//! coordinate convention to reconcile. Without the grant the poll simply runs
//! at its normal rate, which is why this is an upgrade and never a requirement.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

use objc2_core_foundation::{CFBoolean, CFDictionary, CFRunLoop, CFRunLoopSource, CFString};

/// `AXError`; anything else is a failure we degrade from.
const AX_SUCCESS: i32 = 0;

type AXObserverCallback = unsafe extern "C-unwind" fn(
    observer: *mut c_void,
    element: *mut c_void,
    notification: *const CFString,
    refcon: *mut c_void,
);

#[link(name = "ApplicationServices", kind = "framework")]
extern "C-unwind" {
    fn AXIsProcessTrusted() -> u8;
    fn AXIsProcessTrustedWithOptions(options: *const CFDictionary) -> u8;
    fn AXUIElementCreateApplication(pid: i32) -> *mut c_void;
    fn AXObserverCreate(
        application: i32,
        callback: AXObserverCallback,
        out: *mut *mut c_void,
    ) -> i32;
    fn AXObserverAddNotification(
        observer: *mut c_void,
        element: *mut c_void,
        notification: &CFString,
        refcon: *mut c_void,
    ) -> i32;
    fn AXObserverGetRunLoopSource(observer: *mut c_void) -> *mut CFRunLoopSource;
    static kAXTrustedCheckOptionPrompt: &'static CFString;
}

extern "C-unwind" {
    fn CFRelease(cf: *mut c_void);
}

/// `#define`s in `AXNotificationConstants.h`, so there is no static to link.
const MOVED: &str = "AXWindowMoved";
const RESIZED: &str = "AXWindowResized";

pub(super) fn trusted() -> bool {
    // SAFETY: no arguments, no out pointers.
    unsafe { AXIsProcessTrusted() != 0 }
}

/// Shows the system grant dialog. Separate from [`trusted`] because the
/// prompting form must only ever run on a user action (plan §6).
pub(super) fn request_prompt() {
    // SAFETY: the key static is live for the life of the process.
    let key = unsafe { kAXTrustedCheckOptionPrompt };
    let options = CFDictionary::from_slices(&[key], &[CFBoolean::new(true)]);
    // SAFETY: `options` outlives the call; the pointer is never retained.
    unsafe {
        AXIsProcessTrustedWithOptions(options.as_opaque());
    }
}

/// An AX observer on one application, live on the run loop that created it.
/// Dropping it takes the source back out and releases both AX objects.
pub(super) struct Observer {
    observer: *mut c_void,
    element: *mut c_void,
    /// The callback's only job. Boxed so its address survives moving the
    /// `Observer`, which is what makes it usable as the AX refcon.
    fired: Box<AtomicBool>,
}

impl Observer {
    /// True once a real notification has arrived. The tracker slows its poll
    /// only after that, so an observer that registers and then says nothing can
    /// never make tracking worse than the plain poll.
    pub(super) fn has_fired(&self) -> bool {
        self.fired.load(Ordering::Relaxed)
    }
}

/// `None` whenever anything at all refuses — no grant, a process that just
/// exited, an application with no AX server. The caller keeps polling.
pub(super) fn observe(pid: i32) -> Option<Observer> {
    if !trusted() {
        return None;
    }
    let mut raw: *mut c_void = std::ptr::null_mut();
    // SAFETY: `raw` is a live local; the callee writes an owned AXObserverRef.
    let err = unsafe { AXObserverCreate(pid, on_ax_notification, &mut raw) };
    if err != AX_SUCCESS || raw.is_null() {
        return None;
    }
    // SAFETY: the pid is the one AXObserverCreate just accepted.
    let element = unsafe { AXUIElementCreateApplication(pid) };
    if element.is_null() {
        // SAFETY: `raw` is the observer we own and never handed out.
        unsafe { CFRelease(raw) };
        return None;
    }
    let observer = Observer {
        observer: raw,
        element,
        fired: Box::new(AtomicBool::new(false)),
    };
    // The box owns the flag for exactly as long as the registration lives, and
    // its address does not move when the `Observer` is returned.
    let refcon: *mut c_void = std::ptr::from_ref(&*observer.fired)
        .cast_mut()
        .cast::<c_void>();

    let mut added = 0;
    for name in [MOVED, RESIZED] {
        let name = CFString::from_static_str(name);
        // SAFETY: both AX objects are ours and live, and the refcon outlives
        // the registration — `Drop` takes the source out before the box goes.
        let err = unsafe {
            AXObserverAddNotification(observer.observer, observer.element, &name, refcon)
        };
        if err == AX_SUCCESS {
            added += 1;
        }
    }
    // Registered on the *application* element, so the notifications arrive for
    // whichever of its windows moved — no per-window element to resolve.
    if added == 0 {
        return None;
    }

    // SAFETY: the observer is live, so its source is too.
    let source = unsafe { AXObserverGetRunLoopSource(observer.observer) };
    let run_loop = CFRunLoop::current()?;
    // SAFETY: the source belongs to the observer this fn owns and is removed
    // again in `Drop`, on this same thread.
    unsafe {
        run_loop.add_source(source.as_ref(), default_mode());
    }
    Some(observer)
}

impl Drop for Observer {
    fn drop(&mut self) {
        // SAFETY: the source is the one added above, on this same thread; a
        // run loop that has already gone leaves nothing to remove.
        unsafe {
            let source = AXObserverGetRunLoopSource(self.observer);
            if let Some(run_loop) = CFRunLoop::current() {
                run_loop.remove_source(source.as_ref(), default_mode());
            }
            CFRelease(self.element);
            CFRelease(self.observer);
        }
    }
}

/// Runs on the tracker's run loop. Handling the source is what returns control
/// to the tick, which then reads the geometry, so all this owes anyone is the
/// flag. The rule the Windows hook callback follows applies here too — no
/// blocking work, and never an unwind across the FFI edge.
unsafe extern "C-unwind" fn on_ax_notification(
    _observer: *mut c_void,
    _element: *mut c_void,
    _notification: *const CFString,
    refcon: *mut c_void,
) {
    // SAFETY: the refcon is the `Observer`'s boxed flag, alive until the
    // registration it belongs to is taken off this same thread's run loop.
    if let Some(fired) = unsafe { refcon.cast::<AtomicBool>().as_ref() } {
        fired.store(true, Ordering::Relaxed);
    }
}

fn default_mode() -> Option<&'static objc2_core_foundation::CFRunLoopMode> {
    // SAFETY: a CoreFoundation string constant, live for the life of the process.
    unsafe { objc2_core_foundation::kCFRunLoopDefaultMode }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_trust_check_answers_without_prompting() {
        // Either answer is correct; the point is that a background thread may
        // ask, and that asking never shows a dialog.
        let _ = trusted();
    }

    #[test]
    fn observing_a_dead_process_degrades_to_none() {
        assert!(
            observe(-1).is_none(),
            "a bogus pid must not yield an observer"
        );
    }

    /// Grant Accessibility to the test binary first, then watch: moving a
    /// window of the observed application should wake the run loop, so this
    /// returns an observer rather than `None`.
    #[test]
    #[ignore = "needs an Accessibility grant"]
    fn a_granted_process_can_observe_its_own_application() {
        assert!(trusted(), "grant Accessibility to the test binary first");
        let observer = observe(std::process::id() as i32);
        assert!(observer.is_some(), "a granted process must observe itself");
    }
}
