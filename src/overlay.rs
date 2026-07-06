//! The overlay: a floating acrylic bar pinned above the focused window that mirrors its top-level
//! menus. Clicking a top menu pops a native `NSMenu` (so the system font, separators, right-aligned
//! shortcut column, and disabled greying come for free); selecting an item fires the real action
//! via `AXPress`.
//!
//! Window-follow hides the bar during a move/resize (AXObserver -> begin_move) and re-pins it once
//! the window settles, driven by the 60 Hz timer (which is also the fallback for window-server
//! moves that post no AX events). Single menu level for now; presses the cached leaf element (no
//! re-resolve-by-path yet — fine for static/native menus).

use std::cell::RefCell;
use std::time::{Duration, Instant};
use core::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSButton, NSColor, NSEventModifierFlags, NSFont, NSImage, NSLayoutAttribute,
    NSMenu, NSMenuItem, NSPanel, NSScreen, NSStackView, NSStatusBar, NSStatusItem,
    NSUserInterfaceLayoutOrientation, NSVariableStatusItemLength, NSVisualEffectBlendingMode,
    NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView, NSWindowCollectionBehavior,
    NSWindowStyleMask, NSWorkspace,
};
use objc2_application_services::{AXError, AXObserver, AXUIElement};
use objc2_core_foundation::{
    kCFRunLoopDefaultMode, CFEqual, CFRetained, CFRunLoop, CFRunLoopSource, CFString, CGPoint,
    CGSize,
};
use objc2_foundation::{
    NSEdgeInsets, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSTimer,
};

use crate::ax::{self, names};

const BAR_H: f64 = 24.0; // fallback; the real bar height tracks the system menu bar (§ menu_bar_height)
const NS_STATUS_LEVEL: isize = 25; // draws over the static system menu bar (design v2 §6.3)
const TICK_INTERVAL: f64 = 1.0 / 60.0; // window-follow poll rate (~60 Hz; was 10 Hz)
const SETTLE: Duration = Duration::from_millis(120); // how long the window must be still before re-showing
const ITEM_SPACING: f64 = 20.0; // gap between top-level menu titles
const BAR_EDGE: f64 = 14.0; // left/right padding inside the bar
const BAR_V_MARGIN: f64 = 6.0; // extra height beyond the system menu bar (vertical breathing room)
const FONT_SIZE: f64 = 15.0; // slightly larger than the default menu-bar font
const CORNER_RADIUS: f64 = 12.0; // matches the macOS window corner radius (rounds the bar's ends)
const WINDOW_GAP: f64 = 2.0; // gap between the window's top edge and the bar

// ---- menu model (elements cached for the current app) -------------------------------------

struct Shortcut {
    key: String,               // the key-equivalent character (lowercased)
    mods: NSEventModifierFlags, // modifier flags to render (⌘⇧⌥⌃)
}
struct ItemEntry {
    title: String,
    el: CFRetained<AXUIElement>,
    is_sep: bool,
    enabled: bool,
    shortcut: Option<Shortcut>,
}
struct TopMenu {
    title: String,
    items: Vec<ItemEntry>,
}

/// Translate the AX menu-item modifier bitmask (Shift=1, Option=2, Control=4, NoCommand=8;
/// Command implied unless the NoCommand bit is set) into `NSEventModifierFlags`.
fn ax_mods_to_ns(axmods: i64) -> NSEventModifierFlags {
    let mut m = NSEventModifierFlags::empty();
    if axmods & 1 != 0 {
        m |= NSEventModifierFlags::Shift;
    }
    if axmods & 2 != 0 {
        m |= NSEventModifierFlags::Option;
    }
    if axmods & 4 != 0 {
        m |= NSEventModifierFlags::Control;
    }
    if axmods & 8 == 0 {
        m |= NSEventModifierFlags::Command; // Command is implied unless NoCommand is set
    }
    m
}

/// An AXObserver watching the focused window's move/resize notifications so the bar follows
/// event-driven (between 60 Hz ticks). Re-armed when the focused app or window changes; its
/// Drop removes the run-loop source and notifications.
struct MoveObserver {
    observer: CFRetained<AXObserver>,
    source: CFRetained<CFRunLoopSource>,
    window: CFRetained<AXUIElement>,
    pid: i32,
}

impl Drop for MoveObserver {
    fn drop(&mut self) {
        if let Some(rl) = CFRunLoop::current() {
            rl.remove_source(Some(&self.source), unsafe { kCFRunLoopDefaultMode });
        }
        for n in [names::AX_WINDOW_MOVED, names::AX_WINDOW_RESIZED] {
            unsafe {
                let _ = self.observer.remove_notification(&self.window, &ax::cfstr(n));
            }
        }
    }
}

/// AXObserver callback (main-thread, via the run-loop source): reposition the bar immediately
/// on a window move/resize. `refcon` is a borrowed `*const Controller` (the Controller outlives
/// every observer it owns). Must not unwind into AX.
unsafe extern "C-unwind" fn ax_move_cb(
    _observer: NonNull<AXObserver>,
    _element: NonNull<AXUIElement>,
    _notification: NonNull<CFString>,
    refcon: *mut core::ffi::c_void,
) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if !refcon.is_null() {
            let controller = unsafe { &*(refcon as *const Controller) };
            controller.begin_move();
        }
    }));
}

fn same_element(a: &AXUIElement, b: &AXUIElement) -> bool {
    CFEqual(Some(&**a), Some(&**b))
}

/// What a reconciliation step decided to do with the bar (executed after the RefCell borrow drops).
enum Action {
    Show(f64, f64, Retained<NSPanel>),
    Hide(Retained<NSPanel>),
    Nothing,
}

struct Inner {
    bar: Retained<NSPanel>,
    model: Vec<TopMenu>,
    current_pid: i32,
    bar_size: CGSize,
    open_top: Option<usize>, // which top-level menu the open NSMenu belongs to
    status_item: Option<Retained<NSStatusItem>>,
    last_frame: Option<(CGPoint, CGSize)>, // last focused-window frame we saw
    move_obs: Option<MoveObserver>,        // event-driven move detection
    moving: bool,                          // window is moving/resizing -> bar hidden
    settle_at: Option<Instant>,            // re-show once the window is still past this instant
    shown: bool,                           // bar is currently on screen
}

pub struct Ivars {
    inner: RefCell<Inner>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "LintelController"]
    #[ivars = Ivars]
    pub struct Controller;

    unsafe impl NSObjectProtocol for Controller {}

    impl Controller {
        #[unsafe(method(tick:))]
        fn tick_(&self, _timer: &NSTimer) {
            self.on_tick();
        }

        #[unsafe(method(topClicked:))]
        fn top_clicked_(&self, sender: Option<&AnyObject>) {
            if let Some(b) = sender.and_then(|s| s.downcast_ref::<NSButton>()) {
                self.on_top_clicked(b);
            }
        }

        #[unsafe(method(itemClicked:))]
        fn item_clicked_(&self, sender: Option<&AnyObject>) {
            if let Some(b) = sender.and_then(|s| s.downcast_ref::<NSButton>()) {
                self.on_item_clicked(b.tag() as usize);
            }
        }

        #[unsafe(method(quitLintel:))]
        fn quit_(&self, _sender: Option<&AnyObject>) {
            std::process::exit(0);
        }
    }
);

impl Controller {
    pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let bar = make_panel(mtm, NS_STATUS_LEVEL);
        let inner = Inner {
            bar,
            model: Vec::new(),
            current_pid: 0,
            bar_size: CGSize::new(0.0, BAR_H),
            open_top: None,
            status_item: None,
            last_frame: None,
            move_obs: None,
            moving: false,
            settle_at: None,
            shown: false,
        };
        let this = Self::alloc(mtm).set_ivars(Ivars {
            inner: RefCell::new(inner),
        });
        unsafe { msg_send![super(this), init] }
    }

    /// Install the menu-bar status item and start the window-follow timer.
    pub fn start(&self) {
        self.setup_status_item();
        unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                TICK_INTERVAL,
                self,
                sel!(tick:),
                None,
                true,
            );
        }
    }

    /// A menu-bar status item (icon + "Quit Lintel"), so Lintel can run in the background
    /// without a controlling terminal.
    fn setup_status_item(&self) {
        let mtm = self.mtm();
        let status_bar = NSStatusBar::systemStatusBar();
        let item = status_bar.statusItemWithLength(NSVariableStatusItemLength);
        if let Some(button) = item.button(mtm) {
            let sym = NSString::from_str("menubar.rectangle");
            let desc = NSString::from_str("Lintel");
            if let Some(img) =
                NSImage::imageWithSystemSymbolName_accessibilityDescription(&sym, Some(&desc))
            {
                img.setTemplate(true);
                button.setImage(Some(&img));
            } else {
                button.setTitle(&NSString::from_str("L")); // fallback if the symbol is unavailable
            }
        }
        let menu = NSMenu::new(mtm);
        let target: &AnyObject = self;
        unsafe {
            let quit = menu.addItemWithTitle_action_keyEquivalent(
                &NSString::from_str("Quit Lintel"),
                Some(sel!(quitLintel:)),
                &NSString::from_str("q"),
            );
            quit.setTarget(Some(target));
        }
        item.setMenu(Some(&menu));
        self.ivars().inner.borrow_mut().status_item = Some(item);
    }

    fn on_tick(&self) {
        if !ax::is_trusted() {
            self.hide_all();
            return;
        }
        let Some(pid) = frontmost_pid() else {
            self.hide_all();
            return;
        };
        if pid == std::process::id() as i32 {
            return; // never mirror ourselves
        }

        if self.ivars().inner.borrow().current_pid != pid {
            self.rebuild_bar_content(pid);
        }

        let app = ax::app_element(pid);
        ax::set_timeout(&app, 1.0);
        let Some(win) = ax::focused_window(&app) else {
            self.hide_all();
            return;
        };
        // Arm/refresh the event-driven move observer on the current window (re-arm only here,
        // never inside the callback), then position (this is also the 60 Hz fallback).
        self.ensure_observer(pid, &win);
        self.reconcile(&win);
    }

    /// AXObserver callback: the focused window just started moving/resizing. Hide the bar at once
    /// (so it never visibly chases) and mark it moving; the timer re-pins it once things settle.
    fn begin_move(&self) {
        let now = Instant::now();
        let bar = {
            let mut inner = self.ivars().inner.borrow_mut();
            inner.moving = true;
            inner.settle_at = Some(now + SETTLE);
            inner.shown.then(|| {
                inner.shown = false;
                inner.bar.clone()
            })
        };
        if let Some(bar) = bar {
            if std::env::var_os("LINTEL_DEBUG").is_some() {
                eprintln!("[dbg] begin_move -> hide");
            }
            bar.orderOut(None);
        }
    }

    /// The 60 Hz reconciliation: show on a fresh focus, hide while the window is moving, and
    /// re-pin once it has been still for `SETTLE`. Also the fallback for window-server moves
    /// (Stage Manager / Spaces / tiling) that post no AX move events.
    fn reconcile(&self, win: &AXUIElement) {
        let (Some(pos), Some(size)) = (
            ax::attr_point(win, names::AX_POSITION),
            ax::attr_size(win, names::AX_SIZE),
        ) else {
            self.hide_all();
            return;
        };
        if should_hide(self.mtm(), size) {
            self.hide_all();
            return;
        }
        let (x, y_top) = self.place(pos);
        let now = Instant::now();
        let action = {
            let mut inner = self.ivars().inner.borrow_mut();
            let frame = (pos, size);
            if inner.last_frame != Some(frame) {
                let fresh = inner.last_frame.is_none();
                inner.last_frame = Some(frame);
                if fresh {
                    // First frame for this window (focus change): show right away.
                    inner.moving = false;
                    inner.shown = true;
                    Action::Show(x, y_top, inner.bar.clone())
                } else {
                    // Window moved/resized: hide and wait for it to settle.
                    inner.moving = true;
                    inner.settle_at = Some(now + SETTLE);
                    if inner.shown {
                        inner.shown = false;
                        Action::Hide(inner.bar.clone())
                    } else {
                        Action::Nothing
                    }
                }
            } else if inner.moving {
                // Frame stable this tick; re-pin once it's been still long enough.
                if inner.settle_at.map_or(true, |d| now >= d) {
                    inner.moving = false;
                    inner.shown = true;
                    Action::Show(x, y_top, inner.bar.clone())
                } else {
                    Action::Nothing
                }
            } else if !inner.shown {
                inner.shown = true;
                Action::Show(x, y_top, inner.bar.clone())
            } else {
                Action::Nothing
            }
        };
        let debug = std::env::var_os("LINTEL_DEBUG").is_some();
        match action {
            Action::Show(x, y_top, bar) => {
                if debug {
                    eprintln!("[dbg] show winpos=({:.0},{:.0}) baro=({x:.0},{y_top:.0})", pos.x, pos.y);
                }
                bar.setFrameOrigin(NSPoint::new(x, y_top + WINDOW_GAP));
                bar.orderFront(None);
            }
            Action::Hide(bar) => {
                if debug {
                    eprintln!("[dbg] hide (moving)");
                }
                bar.orderOut(None);
            }
            Action::Nothing => {}
        }
    }

    /// Ensure the move observer is watching the current focused window; re-arm on app/window change.
    fn ensure_observer(&self, pid: i32, win: &AXUIElement) {
        let need_arm = match &self.ivars().inner.borrow().move_obs {
            Some(m) => m.pid != pid || !same_element(&m.window, win),
            None => true,
        };
        if need_arm {
            self.arm_move_observer(pid, win);
        }
    }

    fn arm_move_observer(&self, pid: i32, win: &AXUIElement) {
        let refcon = self as *const Controller as *mut core::ffi::c_void;
        let mut raw: *mut AXObserver = core::ptr::null_mut();
        let err =
            unsafe { AXObserver::create(pid, Some(ax_move_cb), NonNull::new(&mut raw).unwrap()) };
        if err != AXError::Success || raw.is_null() {
            self.ivars().inner.borrow_mut().move_obs = None;
            return;
        }
        if std::env::var_os("LINTEL_DEBUG").is_some() {
            eprintln!("[dbg] arm_move_observer pid={pid}");
        }
        let observer = unsafe { CFRetained::from_raw(NonNull::new(raw).unwrap()) };
        let mut registered = false;
        for n in [names::AX_WINDOW_MOVED, names::AX_WINDOW_RESIZED] {
            let err = unsafe { observer.add_notification(win, &ax::cfstr(n), refcon) };
            registered |= err == AXError::Success;
        }
        if !registered {
            // Couldn't subscribe (e.g. a momentarily-busy target); leave unarmed so a later
            // tick retries, and fall back to the 60 Hz poll meanwhile.
            self.ivars().inner.borrow_mut().move_obs = None;
            return;
        }
        let source = unsafe { observer.run_loop_source() };
        if let Some(rl) = CFRunLoop::current() {
            rl.add_source(Some(&source), unsafe { kCFRunLoopDefaultMode });
        }
        let window = unsafe { CFRetained::retain(NonNull::from(win)) };
        // Replacing drops the previous observer, whose Drop removes its source + notifications.
        self.ivars().inner.borrow_mut().move_obs = Some(MoveObserver {
            observer,
            source,
            window,
            pid,
        });
    }

    /// Read the app's menu bar (top-level + first-level) and rebuild the bar's buttons.
    fn rebuild_bar_content(&self, pid: i32) {
        let mtm = self.mtm();
        let app = ax::app_element(pid);
        ax::set_timeout(&app, 1.0);

        let mut model = Vec::new();
        if let Some(menubar) = ax::attr_element(&app, names::AX_MENU_BAR) {
            for top in ax::children(&menubar) {
                let Some(title) = ax::attr_string(&top, names::AX_TITLE) else {
                    continue;
                };
                if title.is_empty() || title == "Apple" {
                    continue; // leave the Apple menu on the real system menu bar
                }
                let items = ax::children(&top)
                    .into_iter()
                    .next() // the single AXMenu child
                    .map(|menu| {
                        ax::children(&menu)
                            .into_iter()
                            .map(|mi| {
                                let t = ax::attr_string(&mi, names::AX_TITLE).unwrap_or_default();
                                let enabled = ax::attr_bool(&mi, names::AX_ENABLED).unwrap_or(true);
                                let shortcut = ax::attr_string(&mi, names::AX_MENU_ITEM_CMD_CHAR)
                                    .filter(|c| !c.is_empty())
                                    .map(|c| Shortcut {
                                        key: c.to_lowercase(),
                                        mods: ax_mods_to_ns(
                                            ax::attr_i64(&mi, names::AX_MENU_ITEM_CMD_MODIFIERS)
                                                .unwrap_or(0),
                                        ),
                                    });
                                ItemEntry {
                                    is_sep: t.is_empty(),
                                    enabled,
                                    shortcut,
                                    title: t,
                                    el: mi,
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                model.push(TopMenu { title, items });
            }
        }

        if std::env::var_os("LINTEL_DEBUG").is_some() {
            eprintln!(
                "[dbg] bar titles: {:?}",
                model.iter().map(|t| t.title.as_str()).collect::<Vec<_>>()
            );
        }

        // Build a fresh acrylic content view with one button per top-level menu.
        let (effect, stack) = make_content(mtm);
        let (font, bold) = menu_bar_fonts(mtm);
        let target: &AnyObject = self;
        for (i, top) in model.iter().enumerate() {
            // The app menu (first item, after the Apple menu is dropped) is bold, like macOS.
            let f: &NSFont = if i == 0 { &bold } else { &font };
            let btn = make_button(mtm, &top.title, f, true, target, sel!(topClicked:), i as isize);
            stack.addArrangedSubview(&btn);
        }
        // Width fits the titles; height is the system menu bar plus a little vertical margin.
        let fit = stack.fittingSize();
        let size = CGSize::new(fit.width, menu_bar_height() + BAR_V_MARGIN);

        let bar = {
            let mut inner = self.ivars().inner.borrow_mut();
            inner.bar_size = size;
            inner.model = model;
            inner.current_pid = pid;
            inner.open_top = None;
            inner.last_frame = None; // bar size changed; reposition on the next tick
            inner.bar.clone()
        };
        bar.setContentView(Some(&effect));
        bar.setContentSize(size);
        stack.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), size));
    }

    fn on_top_clicked(&self, button: &NSButton) {
        let idx = button.tag() as usize;
        let mtm = self.mtm();

        // Build a native NSMenu mirroring this top-level menu — the system menu font, separators,
        // right-aligned shortcut column, and disabled greying all come for free.
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false); // honor our explicit per-item enabled state
        let target: &AnyObject = self;
        let bar = {
            let inner = self.ivars().inner.borrow();
            let Some(top) = inner.model.get(idx) else {
                return;
            };
            for (j, it) in top.items.iter().enumerate() {
                if it.is_sep {
                    menu.addItem(&NSMenuItem::separatorItem(mtm));
                    continue;
                }
                let key = it.shortcut.as_ref().map(|s| s.key.as_str()).unwrap_or("");
                let item = unsafe {
                    menu.addItemWithTitle_action_keyEquivalent(
                        &NSString::from_str(&it.title),
                        Some(sel!(itemClicked:)),
                        &NSString::from_str(key),
                    )
                };
                if let Some(s) = &it.shortcut {
                    item.setKeyEquivalentModifierMask(s.mods);
                }
                item.setEnabled(it.enabled);
                item.setTag(j as isize);
                unsafe { item.setTarget(Some(target)) };
            }
            inner.bar.clone()
        };
        self.ivars().inner.borrow_mut().open_top = Some(idx);

        // Pop it up just under the clicked title (screen coords = the button's bottom-left).
        let scr = bar.convertRectToScreen(button.convertRect_toView(button.bounds(), None));
        menu.popUpMenuPositioningItem_atLocation_inView(
            None,
            NSPoint::new(scr.origin.x, scr.origin.y),
            None,
        );
    }

    fn on_item_clicked(&self, j: usize) {
        let el = {
            let inner = self.ivars().inner.borrow();
            inner
                .open_top
                .and_then(|t| inner.model.get(t))
                .and_then(|top| top.items.get(j))
                .map(|it| it.el.clone())
        };
        if let Some(el) = el {
            let err = ax::press(&el);
            println!("[lintel] AXPress -> {err:?}");
        }
    }

    /// Convert an AX window position (global top-left, y-down) to the Cocoa y of the window's top
    /// edge (design v2 §8.2), returning the bar's desired top-left in Cocoa coords.
    fn place(&self, pos: CGPoint) -> (f64, f64) {
        let primary_h = origin_screen_height(self.mtm());
        (pos.x, primary_h - pos.y)
    }

    fn hide_all(&self) {
        let bar = {
            let mut inner = self.ivars().inner.borrow_mut();
            inner.last_frame = None; // force a fresh show when an eligible window returns
            inner.moving = false;
            inner.shown = false;
            inner.bar.clone()
        };
        bar.orderOut(None);
    }
}

// ---- free helpers -------------------------------------------------------------------------

fn make_panel(mtm: MainThreadMarker, level: isize) -> Retained<NSPanel> {
    let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(10.0, BAR_H));
    let style = NSWindowStyleMask::NonactivatingPanel | NSWindowStyleMask::Borderless;
    let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        rect,
        style,
        NSBackingStoreType::Buffered,
        false,
    );
    panel.setLevel(level);
    panel.setOpaque(false);
    panel.setBackgroundColor(Some(&NSColor::clearColor()));
    panel.setHasShadow(false); // no black outline; the rounded acrylic view is the whole visual
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::MoveToActiveSpace
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    panel
}

/// The bar's acrylic content view + a horizontal stack for the top-level menu buttons.
fn make_content(mtm: MainThreadMarker) -> (Retained<NSVisualEffectView>, Retained<NSStackView>) {
    let effect = NSVisualEffectView::new(mtm);
    effect.setMaterial(NSVisualEffectMaterial::HUDWindow);
    effect.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
    effect.setState(NSVisualEffectState::Active);
    effect.setWantsLayer(true);
    // Round the blurred background to the macOS window corner radius (clips the blur).
    if let Some(layer) = effect.layer() {
        layer.setCornerRadius(CORNER_RADIUS);
        layer.setMasksToBounds(true);
    }

    let stack = NSStackView::new(mtm);
    stack.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
    stack.setSpacing(ITEM_SPACING);
    stack.setAlignment(NSLayoutAttribute::CenterY);
    stack.setEdgeInsets(NSEdgeInsets {
        top: 0.0,
        left: BAR_EDGE,
        bottom: 0.0,
        right: BAR_EDGE,
    });
    effect.addSubview(&stack);
    (effect, stack)
}

#[allow(clippy::too_many_arguments)]
fn make_button(
    mtm: MainThreadMarker,
    title: &str,
    font: &NSFont,
    enabled: bool,
    target: &AnyObject,
    action: objc2::runtime::Sel,
    tag: isize,
) -> Retained<NSButton> {
    let ns = NSString::from_str(title);
    let btn =
        unsafe { NSButton::buttonWithTitle_target_action(&ns, Some(target), Some(action), mtm) };
    btn.setBordered(false);
    btn.setFont(Some(font));
    btn.setEnabled(enabled);
    btn.setTag(tag);
    btn
}

/// The standard system menu-bar height.
fn menu_bar_height() -> f64 {
    NSStatusBar::systemStatusBar().thickness()
}

/// The system menu-bar font, in regular and bold (bold is used for the app menu, like macOS).
fn menu_bar_fonts(mtm: MainThreadMarker) -> (Retained<NSFont>, Retained<NSFont>) {
    let _ = mtm;
    let regular = NSFont::menuBarFontOfSize(FONT_SIZE);
    let bold = NSFont::boldSystemFontOfSize(FONT_SIZE);
    (regular, bold)
}

fn frontmost_pid() -> Option<i32> {
    let ws = NSWorkspace::sharedWorkspace();
    ws.frontmostApplication().map(|a| a.processIdentifier())
}

fn origin_screen_height(mtm: MainThreadMarker) -> f64 {
    let screens = NSScreen::screens(mtm);
    if screens.count() > 0 {
        screens.objectAtIndex(0).frame().size.height
    } else {
        0.0
    }
}

/// Hide when the focused window is (near-)fullscreen or maximized (design v2 §5.4, MVP heuristic).
fn should_hide(mtm: MainThreadMarker, size: CGSize) -> bool {
    let Some(screen) = NSScreen::mainScreen(mtm) else {
        return false;
    };
    let frame = screen.frame();
    let visible = screen.visibleFrame();
    // Fullscreen: window ~= whole display. Maximized: window ~= usable (visible) area.
    let fullscreen = size.height >= frame.size.height - 2.0;
    let maximized =
        size.height >= visible.size.height - 2.0 && size.width >= visible.size.width - 2.0;
    fullscreen || maximized
}
