//! Settings window (mirrors the pattern in ~/src/canopy's `settings.rs`) but with a real
//! `NSTabView`: a **General** tab (launch at login) and an **Advanced** tab (the timing
//! knobs — fade, settle delay, poll rate).
//!
//! Architecture, straight from canopy: a `SettingsController` (`define_class!`,
//! main-thread-only) holds a working `Config` plus a `write` closure. Every control's
//! action mutates the working config in place and calls `emit()`, which hands the config
//! back through `write` — the app layer persists it (`config::save`) and live-applies it.
//! The UI never touches disk itself. `read`/`write` decouple this module from the running
//! `overlay::Controller` that owns the live config.

use std::cell::RefCell;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, Sel};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSAutoresizingMaskOptions, NSBackingStoreType, NSButton, NSControlStateValueOff,
    NSControlStateValueOn, NSLayoutAttribute, NSSlider, NSStackView, NSTabView, NSTabViewItem,
    NSTextField, NSUserInterfaceLayoutOrientation, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

use crate::config::Config;

/// Register (or clear) Lintel as a login item via `SMAppService` (macOS 13+). Only takes
/// effect from a signed `.app` bundle; from `make dev` it logs a harmless error.
pub fn set_login_item(enabled: bool) {
    use objc2_service_management::SMAppService;
    let service = unsafe { SMAppService::mainAppService() };
    let res = if enabled {
        unsafe { service.registerAndReturnError() }
    } else {
        unsafe { service.unregisterAndReturnError() }
    };
    if let Err(e) = res {
        eprintln!(
            "[login-item] {} failed: {e:?}",
            if enabled { "register" } else { "unregister" }
        );
    }
}

type WriteFn = Rc<dyn Fn(Config)>;
type LabelSlot = RefCell<Option<Retained<NSTextField>>>;

thread_local! {
    /// Keep the window + controller retained while open (a released window / dropped
    /// controller would vanish, taking the write closure with it). One at a time.
    static WINDOW: RefCell<Option<Retained<NSWindow>>> = const { RefCell::new(None) };
    static CONTROLLER: RefCell<Option<Retained<SettingsController>>> = const { RefCell::new(None) };
}

struct ControllerIvars {
    config: RefCell<Config>,
    write: WriteFn,
    fade_label: LabelSlot,
    settle_label: LabelSlot,
    poll_label: LabelSlot,
}

define_class!(
    // Target for every settings control. Each action mutates the working `Config` and
    // calls `emit()` -> the app persists + live-applies.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "LintelSettingsController"]
    #[ivars = ControllerIvars]
    struct SettingsController;

    impl SettingsController {
        #[unsafe(method(fadeChanged:))]
        fn fade_changed(&self, sender: &NSSlider) {
            let v = sender.doubleValue().round() as u32;
            self.ivars().config.borrow_mut().fade_ms = v;
            set_label(&self.ivars().fade_label, &format!("{v} ms"));
            self.emit();
        }

        #[unsafe(method(settleChanged:))]
        fn settle_changed(&self, sender: &NSSlider) {
            let v = sender.doubleValue().round() as u32;
            self.ivars().config.borrow_mut().settle_ms = v;
            set_label(&self.ivars().settle_label, &format!("{v} ms"));
            self.emit();
        }

        #[unsafe(method(pollChanged:))]
        fn poll_changed(&self, sender: &NSSlider) {
            let v = sender.doubleValue().round() as u32;
            self.ivars().config.borrow_mut().poll_hz = v;
            set_label(&self.ivars().poll_label, &format!("{v} Hz"));
            self.emit();
        }

        #[unsafe(method(launchToggled:))]
        fn launch_toggled(&self, sender: &NSButton) {
            self.ivars().config.borrow_mut().launch_at_login = is_on(sender);
            self.emit();
        }
    }
);

impl SettingsController {
    fn new(mtm: MainThreadMarker, cfg: Config, write: WriteFn) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ControllerIvars {
            config: RefCell::new(cfg),
            write,
            fade_label: RefCell::new(None),
            settle_label: RefCell::new(None),
            poll_label: RefCell::new(None),
        });
        unsafe { msg_send![super(this), init] }
    }

    /// Push the current working config back to the app (persist + live-apply).
    fn emit(&self) {
        let cfg = self.ivars().config.borrow().clone();
        (self.ivars().write)(cfg);
    }
}

/// Open (or replace) the settings window. `read` snapshots the current config; `write`
/// receives the updated config on every control change. Main thread only.
pub fn open(mtm: MainThreadMarker, read: Rc<dyn Fn() -> Config>, write: WriteFn) {
    let cfg = read();

    let controller = SettingsController::new(mtm, cfg.clone(), write);
    CONTROLLER.with(|c| *c.borrow_mut() = Some(controller.clone()));
    let target: &AnyObject = &controller;

    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(460.0, 300.0));
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable;
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            mtm.alloc::<NSWindow>(),
            frame,
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    window.setTitle(&NSString::from_str("Lintel Settings"));
    unsafe { window.setReleasedWhenClosed(false) };

    let tabs = NSTabView::new(mtm);

    // --- General ---
    let general = vstack(mtm);
    add(
        &general,
        &checkbox(
            mtm,
            target,
            "Launch Lintel at login",
            cfg.launch_at_login,
            sel!(launchToggled:),
        ),
    );
    tabs.addTabViewItem(&tab_item(mtm, "General", &general));

    // --- Advanced ---
    let advanced = vstack(mtm);

    let fade_lbl = make_label(mtm, &format!("{} ms", cfg.fade_ms));
    *controller.ivars().fade_label.borrow_mut() = Some(fade_lbl.clone());
    add(
        &advanced,
        &slider_row(
            mtm, "Fade", &fade_lbl, cfg.fade_ms as f64, 0.0, 1000.0, target, sel!(fadeChanged:),
        ),
    );

    let settle_lbl = make_label(mtm, &format!("{} ms", cfg.settle_ms));
    *controller.ivars().settle_label.borrow_mut() = Some(settle_lbl.clone());
    add(
        &advanced,
        &slider_row(
            mtm, "Settle delay", &settle_lbl, cfg.settle_ms as f64, 0.0, 1000.0, target,
            sel!(settleChanged:),
        ),
    );

    let poll_lbl = make_label(mtm, &format!("{} Hz", cfg.poll_hz));
    *controller.ivars().poll_label.borrow_mut() = Some(poll_lbl.clone());
    add(
        &advanced,
        &slider_row(
            mtm, "Poll rate", &poll_lbl, cfg.poll_hz as f64, 15.0, 120.0, target,
            sel!(pollChanged:),
        ),
    );

    tabs.addTabViewItem(&tab_item(mtm, "Advanced", &advanced));

    if let Some(content) = window.contentView() {
        content.addSubview(&tabs);
        let b = content.bounds();
        let inset = 12.0;
        tabs.setFrame(NSRect::new(
            NSPoint::new(inset, inset),
            NSSize::new(b.size.width - 2.0 * inset, b.size.height - 2.0 * inset),
        ));
    }

    window.center();
    // Accessory apps must activate to bring a regular window frontmost.
    #[allow(deprecated)]
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
    window.makeKeyAndOrderFront(None);

    WINDOW.with(|w| *w.borrow_mut() = Some(window));
}

// ---- view helpers -------------------------------------------------------------------------

fn vstack(mtm: MainThreadMarker) -> Retained<NSStackView> {
    let s = NSStackView::new(mtm);
    s.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
    s.setAlignment(NSLayoutAttribute::Leading);
    s.setSpacing(14.0);
    s
}

/// Wrap a populated stack as a tab's content view: a container the `NSTabView` resizes to
/// its content rect, with the stack filling it (inset frame + autoresizing).
fn tab_item(mtm: MainThreadMarker, label: &str, stack: &NSStackView) -> Retained<NSTabViewItem> {
    let size = NSSize::new(420.0, 240.0);
    let container =
        NSView::initWithFrame(mtm.alloc::<NSView>(), NSRect::new(NSPoint::new(0.0, 0.0), size));
    let inset = 16.0;
    stack.setFrame(NSRect::new(
        NSPoint::new(inset, inset),
        NSSize::new(size.width - 2.0 * inset, size.height - 2.0 * inset),
    ));
    stack.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    container.addSubview(stack);

    let item = NSTabViewItem::new();
    item.setLabel(&NSString::from_str(label));
    item.setView(Some(&container));
    item
}

fn add(stack: &NSStackView, view: &NSView) {
    stack.addArrangedSubview(view);
}

/// A `[label] [slider] [value]` row.
#[allow(clippy::too_many_arguments)]
fn slider_row(
    mtm: MainThreadMarker,
    label: &str,
    value_label: &NSTextField,
    value: f64,
    min: f64,
    max: f64,
    target: &AnyObject,
    action: Sel,
) -> Retained<NSStackView> {
    let slider = unsafe {
        NSSlider::sliderWithValue_minValue_maxValue_target_action(
            value,
            min,
            max,
            Some(target),
            Some(action),
            mtm,
        )
    };
    // Fire on release, not per drag-pixel, so we don't hammer config.toml on every frame.
    slider.setContinuous(false);
    slider
        .widthAnchor()
        .constraintEqualToConstant(200.0)
        .setActive(true);
    let h = NSStackView::new(mtm);
    h.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
    h.setSpacing(8.0);
    add(&h, &make_label(mtm, label));
    add(&h, &slider);
    add(&h, value_label);
    h
}

fn make_label(mtm: MainThreadMarker, text: &str) -> Retained<NSTextField> {
    NSTextField::labelWithString(&NSString::from_str(text), mtm)
}

fn checkbox(
    mtm: MainThreadMarker,
    target: &AnyObject,
    title: &str,
    on: bool,
    action: Sel,
) -> Retained<NSButton> {
    let btn = unsafe {
        NSButton::checkboxWithTitle_target_action(
            &NSString::from_str(title),
            Some(target),
            Some(action),
            mtm,
        )
    };
    btn.setState(if on {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    btn
}

fn is_on(button: &NSButton) -> bool {
    button.state() == NSControlStateValueOn
}

fn set_label(slot: &LabelSlot, text: &str) {
    if let Some(lbl) = slot.borrow().as_ref() {
        lbl.setStringValue(&NSString::from_str(text));
    }
}
