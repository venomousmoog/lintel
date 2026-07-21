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
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly, Message};
use objc2_app_kit::{
    NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication,
    NSAutoresizingMaskOptions, NSBackingStoreType, NSButton, NSControlStateValueOff,
    NSControlStateValueOn, NSEvent, NSEventMask, NSEventModifierFlags, NSLayoutAttribute,
    NSPopUpButton, NSSlider, NSStackView, NSTabView, NSTabViewItem, NSTextField,
    NSUserInterfaceLayoutOrientation, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString, NSUserDefaults};
use block2::RcBlock;
use core::ptr::NonNull;

use crate::config::{Config, HotkeyChord, Theme};

/// Name under which the settings window's frame is autosaved (defaults key "NSWindow Frame <this>").
const AUTOSAVE_NAME: &str = "LintelSettings";

/// Read the raw window frame Cocoa autosaved under `NSWindow Frame <name>`, parsing the leading
/// `x y w h` fields. We read it directly instead of `setFrameUsingName:` to skip that method's
/// `constrainFrameRect:toScreen:`, which remaps negative-origin frames (monitors left of / below
/// the primary) back onto the main screen. `None` if nothing was ever saved or the value is junk.
fn saved_window_frame(autosave_name: &str) -> Option<NSRect> {
    let key = NSString::from_str(&format!("NSWindow Frame {autosave_name}"));
    let value = NSUserDefaults::standardUserDefaults().stringForKey(&key)?;
    parse_frame(&value.to_string())
}

/// Parse a Cocoa autosaved frame string — `"x y w h screenX screenY screenW screenH"` — into the
/// window rect (the leading four fields). The trailing screen descriptor is ignored on purpose.
fn parse_frame(s: &str) -> Option<NSRect> {
    let mut nums = s.split_whitespace();
    let x = nums.next()?.parse::<f64>().ok()?;
    let y = nums.next()?.parse::<f64>().ok()?;
    let w = nums.next()?.parse::<f64>().ok()?;
    let h = nums.next()?.parse::<f64>().ok()?;
    Some(NSRect::new(NSPoint::new(x, y), NSSize::new(w, h)))
}

/// Apply the app appearance for `theme` (System = follow the OS). Affects all Lintel windows.
/// `OppositeSystem` resolves to an explicit Light/Dark that inverts the current OS setting; the
/// controller re-applies it when the system flips (an override posts no app-level appearance event).
pub fn apply_theme(mtm: MainThreadMarker, theme: Theme) {
    let app = NSApplication::sharedApplication(mtm);
    let appearance = match theme {
        Theme::System => None,
        Theme::Dark => NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua }),
        Theme::Light => NSAppearance::appearanceNamed(unsafe { NSAppearanceNameAqua }),
        Theme::OppositeSystem => {
            let sys_dark = system_is_dark();
            tracing::debug!("apply theme OppositeSystem: system dark={sys_dark} -> Lintel {}", if sys_dark { "Light" } else { "Dark" });
            let name = if sys_dark {
                unsafe { NSAppearanceNameAqua } // system Dark -> Lintel Light
            } else {
                unsafe { NSAppearanceNameDarkAqua } // system Light -> Lintel Dark
            };
            NSAppearance::appearanceNamed(name)
        }
    };
    app.setAppearance(appearance.as_deref());
}

/// Whether the OS is in Dark mode, independent of any app-level appearance override we've set.
/// Reads the global `AppleInterfaceStyle` default (`"Dark"` in Dark mode, absent in Light).
pub fn system_is_dark() -> bool {
    let key = NSString::from_str("AppleInterfaceStyle");
    NSUserDefaults::standardUserDefaults()
        .stringForKey(&key)
        .is_some_and(|s| s.to_string().eq_ignore_ascii_case("dark"))
}

/// The theme choices in popup order (index into this array is the selection index).
const THEME_ORDER: [Theme; 4] = [
    Theme::System,
    Theme::Dark,
    Theme::Light,
    Theme::OppositeSystem,
];

fn theme_index(t: Theme) -> usize {
    THEME_ORDER.iter().position(|&x| x == t).unwrap_or(0)
}
fn theme_from_index(i: usize) -> Theme {
    *THEME_ORDER.get(i).unwrap_or(&Theme::System)
}
fn theme_label(t: Theme) -> &'static str {
    match t {
        Theme::System => "System",
        Theme::Dark => "Dark",
        Theme::Light => "Light",
        Theme::OppositeSystem => "Opposite System",
    }
}

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
        tracing::warn!(
            "login item {} failed: {e:?}",
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
    hotkey_button: RefCell<Option<Retained<NSButton>>>, // shows the chord; click to re-record
    monitor: RefCell<Option<Retained<AnyObject>>>,      // active key-capture event monitor
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

        #[unsafe(method(themeSelected:))]
        fn theme_selected(&self, sender: &NSPopUpButton) {
            let idx = sender.indexOfSelectedItem().max(0) as usize;
            self.ivars().config.borrow_mut().theme = theme_from_index(idx);
            self.emit();
        }

        #[unsafe(method(paletteToggled:))]
        fn palette_toggled(&self, sender: &NSButton) {
            self.ivars().config.borrow_mut().palette_enabled = is_on(sender);
            self.emit();
        }

        #[unsafe(method(recordHotkey:))]
        fn record_hotkey(&self, _sender: &NSButton) {
            self.start_recording();
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
            hotkey_button: RefCell::new(None),
            monitor: RefCell::new(None),
        });
        unsafe { msg_send![super(this), init] }
    }

    /// Push the current working config back to the app (persist + live-apply).
    fn emit(&self) {
        let cfg = self.ivars().config.borrow().clone();
        (self.ivars().write)(cfg);
    }

    /// Begin capturing the next chord for the palette hotkey via a local key-down monitor.
    fn start_recording(&self) {
        if self.ivars().monitor.borrow().is_some() {
            return; // already recording
        }
        if let Some(btn) = self.ivars().hotkey_button.borrow().as_ref() {
            btn.setTitle(&NSString::from_str("Type a shortcut… (⎋ to cancel)"));
        }
        let this = self.retain();
        let handler = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
            let event = unsafe { event.as_ref() };
            let keycode = event.keyCode();
            if keycode == 53 {
                this.finish_recording(None); // Esc cancels
                return std::ptr::null_mut();
            }
            let mods = event.modifierFlags();
            // Require a real modifier (⌘/⌃/⌥) — a bare or Shift-only global hotkey is a bad idea.
            let has_mod = mods.intersects(
                NSEventModifierFlags::Command
                    | NSEventModifierFlags::Control
                    | NSEventModifierFlags::Option,
            );
            if has_mod {
                this.finish_recording(Some(HotkeyChord {
                    mods: carbon_mods(mods),
                    keycode: keycode as u32,
                }));
            }
            std::ptr::null_mut() // swallow the key while recording
        });
        let monitor = unsafe {
            NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::KeyDown, &handler)
        };
        *self.ivars().monitor.borrow_mut() = monitor;
    }

    /// Stop capturing; if `chord` is `Some`, adopt it. Restores the button label either way.
    fn finish_recording(&self, chord: Option<HotkeyChord>) {
        if let Some(m) = self.ivars().monitor.borrow_mut().take() {
            unsafe { NSEvent::removeMonitor(&m) };
        }
        if let Some(chord) = chord {
            self.ivars().config.borrow_mut().palette_hotkey = chord;
            self.emit();
        }
        let chord = self.ivars().config.borrow().palette_hotkey;
        if let Some(btn) = self.ivars().hotkey_button.borrow().as_ref() {
            btn.setTitle(&NSString::from_str(&hotkey_display(chord)));
        }
    }
}

/// Translate `NSEventModifierFlags` to a Carbon modifier mask (for `RegisterEventHotKey`).
fn carbon_mods(m: NSEventModifierFlags) -> u32 {
    let mut c = 0;
    if m.contains(NSEventModifierFlags::Command) {
        c |= 0x0100;
    }
    if m.contains(NSEventModifierFlags::Shift) {
        c |= 0x0200;
    }
    if m.contains(NSEventModifierFlags::Option) {
        c |= 0x0800;
    }
    if m.contains(NSEventModifierFlags::Control) {
        c |= 0x1000;
    }
    c
}

/// Open (or replace) the settings window. `read` snapshots the current config; `write`
/// receives the updated config on every control change. Main thread only.
pub fn open(mtm: MainThreadMarker, read: Rc<dyn Fn() -> Config>, write: WriteFn) {
    tracing::debug!("settings::open");
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
    let theme_popup = popup(
        mtm,
        THEME_ORDER.iter().map(|&t| theme_label(t)),
        theme_index(cfg.theme),
        target,
        sel!(themeSelected:),
    );
    add(&general, &row(mtm, "Appearance", &theme_popup));
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
    add(
        &general,
        &checkbox(
            mtm,
            target,
            "Enable command palette",
            cfg.palette_enabled,
            sel!(paletteToggled:),
        ),
    );
    let hotkey_btn = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str(&hotkey_display(cfg.palette_hotkey)),
            Some(target),
            Some(sel!(recordHotkey:)),
            mtm,
        )
    };
    *controller.ivars().hotkey_button.borrow_mut() = Some(hotkey_btn.clone());
    add(&general, &row(mtm, "Palette hotkey", &hotkey_btn));
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

    // Reopen the window where it was last left (handy while iterating with `--settings`, so it
    // returns to your chosen monitor/spot instead of centering on the active screen). We keep
    // `setFrameAutosaveName:` for *saving* — it writes the true frame on every move/resize and
    // persists via `cfprefsd`, so it survives the `killall` on restart (a `windowWillClose:` hook
    // wouldn't, since SIGTERM skips it) — but restore the frame ourselves. `setFrameUsingName:`
    // runs the saved frame through `constrainFrameRect:toScreen:` against the main screen, which
    // remaps a window last placed on a monitor left of / below the primary (negative global
    // coords) back onto the primary (restored_x == saved_x + primaryWidth). `setFrame:display:`
    // does no constraining. Center on first-ever run, or if the saved monitor is now unplugged.
    window.setFrameAutosaveName(&NSString::from_str(AUTOSAVE_NAME));
    match saved_window_frame(AUTOSAVE_NAME) {
        Some(rect) => {
            window.setFrame_display(rect, true);
            if window.screen().is_none() {
                window.center();
            }
        }
        None => window.center(),
    }
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

/// A pop-up button with `titles`, `selected` index, wired to `action` on `target`.
fn popup<'a>(
    mtm: MainThreadMarker,
    titles: impl Iterator<Item = &'a str>,
    selected: usize,
    target: &AnyObject,
    action: objc2::runtime::Sel,
) -> Retained<NSPopUpButton> {
    let p = NSPopUpButton::new(mtm);
    for title in titles {
        p.addItemWithTitle(&NSString::from_str(title));
    }
    p.selectItemAtIndex(selected as isize);
    unsafe {
        p.setTarget(Some(target));
        p.setAction(Some(action));
    }
    p
}

/// A `[label]   [control]` horizontal row.
fn row(mtm: MainThreadMarker, label: &str, control: &NSView) -> Retained<NSStackView> {
    let h = NSStackView::new(mtm);
    h.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
    h.setSpacing(8.0);
    add(&h, &make_label(mtm, label));
    add(&h, control);
    h
}

/// Human-readable chord, e.g. "⌘⇧M" (Carbon modifier mask + virtual keycode).
fn hotkey_display(c: HotkeyChord) -> String {
    let mut s = String::new();
    if c.mods & 0x1000 != 0 {
        s.push('⌃');
    }
    if c.mods & 0x0800 != 0 {
        s.push('⌥');
    }
    if c.mods & 0x0200 != 0 {
        s.push('⇧');
    }
    if c.mods & 0x0100 != 0 {
        s.push('⌘');
    }
    s.push_str(keycode_name(c.keycode));
    s
}

/// Display name for a virtual keycode (letters + a few common keys; `?` otherwise).
fn keycode_name(k: u32) -> &'static str {
    match k {
        0x00 => "A", 0x0B => "B", 0x08 => "C", 0x02 => "D", 0x0E => "E", 0x03 => "F", 0x05 => "G",
        0x04 => "H", 0x22 => "I", 0x26 => "J", 0x28 => "K", 0x25 => "L", 0x2E => "M", 0x2D => "N",
        0x1F => "O", 0x23 => "P", 0x0C => "Q", 0x0F => "R", 0x01 => "S", 0x11 => "T", 0x20 => "U",
        0x09 => "V", 0x0D => "W", 0x07 => "X", 0x10 => "Y", 0x06 => "Z",
        0x31 => "Space", 0x24 => "Return", 0x30 => "Tab", 0x35 => "Esc",
        _ => "?",
    }
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

#[cfg(test)]
mod tests {
    use super::parse_frame;

    #[test]
    fn parses_negative_origin_frame() {
        // A window last placed on a monitor left of the primary: negative x. The trailing screen
        // descriptor is ignored; the negative origin must be preserved verbatim (not remapped).
        let r = parse_frame("-996 200 460 332 -3360 0 2560 1440").unwrap();
        assert_eq!(r.origin.x, -996.0);
        assert_eq!(r.origin.y, 200.0);
        assert_eq!(r.size.width, 460.0);
        assert_eq!(r.size.height, 332.0);
    }

    #[test]
    fn rejects_junk_or_short() {
        assert!(parse_frame("").is_none());
        assert!(parse_frame("1 2 3").is_none()); // fewer than 4 fields
        assert!(parse_frame("a b c d").is_none()); // non-numeric
    }
}
