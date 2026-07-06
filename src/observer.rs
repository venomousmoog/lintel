//! An `AXObserver` wired onto the main run loop, with the exact teardown order from design v2 §5.2.
//! Phase 0 proves the `C-unwind` trampoline + `Box` refcon + run-loop-source wiring end-to-end (spike S6).

use core::ffi::c_void;
use core::ptr::{self, NonNull};

use objc2_application_services::{AXError, AXObserver, AXUIElement};
use objc2_core_foundation::{CFRetained, CFRunLoop, CFRunLoopSource, CFString, kCFRunLoopDefaultMode};

use crate::ax::{cfstr, names};

/// Heap context handed to the callback via the observer `refcon`.
pub struct ObserverCtx {
    pub label: String,
}

/// The AX callback. MUST be `extern "C-unwind"` (matches the bound `AXObserverCallback` type),
/// and must not unwind into AX — so the body is wrapped in `catch_unwind` (design §5.2).
unsafe extern "C-unwind" fn trampoline(
    _observer: NonNull<AXObserver>,
    _element: NonNull<AXUIElement>,
    notification: NonNull<CFString>,
    refcon: *mut c_void,
) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let name = unsafe { notification.as_ref() }.to_string();
        let label = if refcon.is_null() {
            "?".to_string()
        } else {
            unsafe { &*(refcon as *const ObserverCtx) }.label.clone()
        };
        println!("[ax-event] {label}: {name}");
    }));
}

/// A live observer plus everything needed to tear it down cleanly.
pub struct AxObserver {
    observer: CFRetained<AXObserver>,
    source: CFRetained<CFRunLoopSource>,
    element: CFRetained<AXUIElement>,
    notifications: Vec<CFRetained<CFString>>,
    refcon: *mut ObserverCtx,
}

impl AxObserver {
    /// Create an observer for `pid`, subscribe to focus/geometry notifications on `element`,
    /// and install its run-loop source on the current (main) run loop.
    pub fn new(pid: i32, element: CFRetained<AXUIElement>, ctx: ObserverCtx) -> Option<AxObserver> {
        let refcon = Box::into_raw(Box::new(ctx));

        let mut raw: *mut AXObserver = ptr::null_mut();
        let err = unsafe { AXObserver::create(pid, Some(trampoline), NonNull::new(&mut raw).unwrap()) };
        if err != AXError::Success || raw.is_null() {
            unsafe { drop(Box::from_raw(refcon)) };
            eprintln!("AXObserverCreate failed: {err:?}");
            return None;
        }
        let observer = unsafe { CFRetained::from_raw(NonNull::new(raw).unwrap()) };

        let mut notifications = Vec::new();
        for n in [
            names::AX_FOCUSED_WINDOW_CHANGED,
            names::AX_WINDOW_MOVED,
            names::AX_WINDOW_RESIZED,
        ] {
            let name = cfstr(n);
            let err = unsafe { observer.add_notification(&element, &name, refcon as *mut c_void) };
            if err == AXError::Success {
                notifications.push(name);
            } else {
                eprintln!("add_notification {n} -> {err:?}");
            }
        }

        let source = unsafe { observer.run_loop_source() };
        let rl = CFRunLoop::current().expect("current run loop");
        rl.add_source(Some(&source), unsafe { kCFRunLoopDefaultMode });

        Some(AxObserver {
            observer,
            source,
            element,
            notifications,
            refcon,
        })
    }
}

impl Drop for AxObserver {
    fn drop(&mut self) {
        // Teardown order (design v2 §5.2): remove source -> remove notifications -> drop
        // observer (CFRetained) -> free the refcon box.
        if let Some(rl) = CFRunLoop::current() {
            rl.remove_source(Some(&self.source), unsafe { kCFRunLoopDefaultMode });
        }
        for n in &self.notifications {
            unsafe {
                let _ = self.observer.remove_notification(&self.element, n);
            }
        }
        unsafe { drop(Box::from_raw(self.refcon)) };
    }
}
