//! All raw Accessibility (AX) FFI is isolated in this module (design v2 §3.1 / §10).
//!
//! The `objc2-application-services` crate binds the AX *functions* but does NOT re-export
//! the `kAX*` attribute/role/action name strings (they are `CFSTR` macros), so we hand-declare
//! the names we use here. See `docs/plans/2026-07-06-lintel-design-v2.md` §10.

use core::ffi::c_void;
use core::ptr::{self, NonNull};

use objc2_application_services::{
    AXError, AXIsProcessTrusted, AXIsProcessTrustedWithOptions, AXUIElement,
    kAXTrustedCheckOptionPrompt,
};
use objc2_core_foundation::{
    CFArray, CFDictionary, CFRetained, CFString, CFType, kCFBooleanTrue,
    kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks,
};

/// Verified AX name constants (subset used in Phase 0).
pub mod names {
    pub const AX_MENU_BAR: &str = "AXMenuBar";
    pub const AX_CHILDREN: &str = "AXChildren";
    pub const AX_TITLE: &str = "AXTitle";
    pub const AX_PRESS: &str = "AXPress";
    pub const AX_FOCUSED_WINDOW_CHANGED: &str = "AXFocusedWindowChanged";
    pub const AX_WINDOW_MOVED: &str = "AXWindowMoved";
    pub const AX_WINDOW_RESIZED: &str = "AXWindowResized";
}

/// Build an owned `CFString` from a Rust `&str`.
#[inline]
pub fn cfstr(s: &str) -> CFRetained<CFString> {
    CFString::from_str(s)
}

/// Is this process trusted for the Accessibility API?
pub fn is_trusted() -> bool {
    unsafe { AXIsProcessTrusted() }
}

/// Prompt for Accessibility permission (opens System Settings). Returns current trust state.
pub fn prompt_trust() -> bool {
    unsafe {
        let key = kAXTrustedCheckOptionPrompt; // &'static CFString
        let val = kCFBooleanTrue.expect("kCFBooleanTrue static");
        let mut keys: [*const c_void; 1] = [(key as *const CFString) as *const c_void];
        let mut vals: [*const c_void; 1] = [(val as *const _) as *const c_void];
        let dict = CFDictionary::new(
            None,
            keys.as_mut_ptr(),
            vals.as_mut_ptr(),
            1,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        );
        AXIsProcessTrustedWithOptions(dict.as_deref())
    }
}

/// Root AX element for a process id.
pub fn app_element(pid: i32) -> CFRetained<AXUIElement> {
    unsafe { AXUIElement::new_application(pid) }
}

/// Bound the per-message timeout so an unresponsive target can't hang us (design §3.2/§4.2).
pub fn set_timeout(el: &AXUIElement, seconds: f32) {
    unsafe {
        let _ = el.set_messaging_timeout(seconds);
    }
}

/// Copy an attribute as a raw `CFType` (Copy Rule -> owned +1).
fn copy_attr(el: &AXUIElement, attr: &str) -> Option<CFRetained<CFType>> {
    let name = cfstr(attr);
    let mut raw: *const CFType = ptr::null();
    let err = unsafe {
        el.copy_attribute_value(&name, NonNull::new(&mut raw as *mut *const CFType).unwrap())
    };
    if err != AXError::Success || raw.is_null() {
        return None;
    }
    Some(unsafe { CFRetained::from_raw(NonNull::new(raw as *mut CFType).unwrap()) })
}

/// Read a string-valued attribute.
pub fn attr_string(el: &AXUIElement, attr: &str) -> Option<String> {
    let v = copy_attr(el, attr)?;
    v.downcast_ref::<CFString>().map(|s| s.to_string())
}

/// Read an element-valued attribute (e.g. `AXMenuBar`).
pub fn attr_element(el: &AXUIElement, attr: &str) -> Option<CFRetained<AXUIElement>> {
    let v = copy_attr(el, attr)?;
    // The value is an AXUIElement; move the +1 ownership across the type cast.
    let raw = CFRetained::into_raw(v);
    Some(unsafe { CFRetained::from_raw(raw.cast::<AXUIElement>()) })
}

/// Read the `AXChildren` of an element as owned AXUIElements.
pub fn children(el: &AXUIElement) -> Vec<CFRetained<AXUIElement>> {
    let Some(v) = copy_attr(el, names::AX_CHILDREN) else {
        return Vec::new();
    };
    let Some(arr) = v.downcast_ref::<CFArray>() else {
        return Vec::new();
    };
    let n = arr.count();
    let mut out = Vec::with_capacity(n.max(0) as usize);
    for i in 0..n {
        let p = unsafe { arr.value_at_index(i) };
        if let Some(nn) = NonNull::new(p as *mut AXUIElement) {
            // CFArray elements are +0 borrowed; retain to own.
            out.push(unsafe { CFRetained::retain(nn) });
        }
    }
    out
}

/// Perform `AXPress` on a (leaf) element.
pub fn press(el: &AXUIElement) -> AXError {
    unsafe { el.perform_action(&cfstr(names::AX_PRESS)) }
}
