//! Global hotkey registration via Carbon's `RegisterEventHotKey` (adapted from ~/src/canopy's
//! `canopy-mac/src/hotkey.rs`).
//!
//! `RegisterEventHotKey` lives in the deprecated-but-supported Carbon `HIToolbox` framework — still
//! the accepted way to register a process-global hotkey from a non-sandboxed app. The chord fires a
//! callback on the main run loop. On failure we return an error; the caller logs and continues.

use std::fmt;

#[derive(Debug)]
pub enum HotkeyError {
    /// `RegisterEventHotKey` (or handler install) returned a non-zero `OSStatus`.
    Register(i32),
}

impl fmt::Display for HotkeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HotkeyError::Register(code) => write!(f, "RegisterEventHotKey failed: OSStatus {code}"),
        }
    }
}

impl std::error::Error for HotkeyError {}

/// RAII guard: unregisters the hotkey and tears down the handler on drop.
pub struct HotkeyRegistration {
    _inner: Inner,
}

impl HotkeyRegistration {
    /// Register `mods` (Carbon modifier mask) + `keycode` globally. `cb` fires on the main run loop
    /// each time the chord is pressed.
    pub fn install(
        mods: u32,
        keycode: u32,
        cb: impl Fn() + 'static,
    ) -> Result<Self, HotkeyError> {
        Ok(HotkeyRegistration {
            _inner: Inner::install(mods, keycode, cb)?,
        })
    }
}

use std::ffi::c_void;

// --- Carbon FFI ---

#[repr(C)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct EventHotKeyID {
    signature: u32,
    id: u32,
}

type EventRef = *mut c_void;
type EventHandlerRef = *mut c_void;
type EventHandlerCallRef = *mut c_void;
type EventTargetRef = *mut c_void;
type EventHotKeyRef = *mut c_void;
type OSStatus = i32;

type EventHandlerProc =
    extern "C-unwind" fn(next: EventHandlerCallRef, event: EventRef, user_data: *mut c_void)
        -> OSStatus;

// `kEventClassKeyboard` = 'keyb', `kEventHotKeyPressed` = 6.
const K_EVENT_CLASS_KEYBOARD: u32 = u32::from_be_bytes(*b"keyb");
const K_EVENT_HOT_KEY_PRESSED: u32 = 6;
const SIGNATURE: u32 = u32::from_be_bytes(*b"LNTL");

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C-unwind" {
    fn GetApplicationEventTarget() -> EventTargetRef;

    fn InstallEventHandler(
        target: EventTargetRef,
        handler: EventHandlerProc,
        num_types: usize,
        list: *const EventTypeSpec,
        user_data: *mut c_void,
        out_ref: *mut EventHandlerRef,
    ) -> OSStatus;

    fn RemoveEventHandler(handler: EventHandlerRef) -> OSStatus;

    fn RegisterEventHotKey(
        key_code: u32,
        modifiers: u32,
        hot_key_id: EventHotKeyID,
        target: EventTargetRef,
        options: u32,
        out_ref: *mut EventHotKeyRef,
    ) -> OSStatus;

    fn UnregisterEventHotKey(hot_key: EventHotKeyRef) -> OSStatus;
}

/// Boxed user callback, kept alive behind a raw pointer for the C handler.
struct HandlerState {
    cb: Box<dyn Fn()>,
}

extern "C-unwind" fn hot_key_handler(
    _next: EventHandlerCallRef,
    _event: EventRef,
    user_data: *mut c_void,
) -> OSStatus {
    if !user_data.is_null() {
        // SAFETY: `user_data` is the `*mut HandlerState` we installed and keep alive.
        let state = unsafe { &*(user_data as *const HandlerState) };
        (state.cb)();
    }
    0 // noErr
}

struct Inner {
    handler_ref: EventHandlerRef,
    hotkey_ref: EventHotKeyRef,
    state: *mut HandlerState,
}

impl Inner {
    fn install(modifiers: u32, keycode: u32, cb: impl Fn() + 'static) -> Result<Self, HotkeyError> {
        let state = Box::into_raw(Box::new(HandlerState { cb: Box::new(cb) }));

        let spec = EventTypeSpec {
            event_class: K_EVENT_CLASS_KEYBOARD,
            event_kind: K_EVENT_HOT_KEY_PRESSED,
        };

        let mut handler_ref: EventHandlerRef = std::ptr::null_mut();
        let status = unsafe {
            InstallEventHandler(
                GetApplicationEventTarget(),
                hot_key_handler,
                1,
                &spec,
                state as *mut c_void,
                &mut handler_ref,
            )
        };
        if status != 0 {
            unsafe { drop(Box::from_raw(state)) };
            return Err(HotkeyError::Register(status));
        }

        let hk_id = EventHotKeyID {
            signature: SIGNATURE,
            id: 1,
        };
        let mut hotkey_ref: EventHotKeyRef = std::ptr::null_mut();
        let status = unsafe {
            RegisterEventHotKey(
                keycode,
                modifiers,
                hk_id,
                GetApplicationEventTarget(),
                0,
                &mut hotkey_ref,
            )
        };
        if status != 0 {
            unsafe { RemoveEventHandler(handler_ref) };
            unsafe { drop(Box::from_raw(state)) };
            return Err(HotkeyError::Register(status));
        }

        Ok(Inner {
            handler_ref,
            hotkey_ref,
            state,
        })
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        unsafe {
            if !self.hotkey_ref.is_null() {
                UnregisterEventHotKey(self.hotkey_ref);
            }
            if !self.handler_ref.is_null() {
                RemoveEventHandler(self.handler_ref);
            }
            if !self.state.is_null() {
                drop(Box::from_raw(self.state));
            }
        }
    }
}
