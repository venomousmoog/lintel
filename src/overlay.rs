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

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{
    define_class, msg_send, sel, AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, Message,
};
use objc2_app_kit::{
    NSAnimatablePropertyContainer, NSAnimationContext, NSBackingStoreType, NSButton, NSColor,
    NSEvent, NSEventModifierFlags, NSFont, NSFontAttributeName, NSForegroundColorAttributeName,
    NSImage, NSLayoutAttribute, NSMenu, NSMenuItem, NSPanel, NSScreen, NSStackView, NSStatusBar,
    NSStatusItem, NSUserInterfaceLayoutOrientation, NSVariableStatusItemLength, NSView,
    NSVisualEffectBlendingMode, NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView,
    NSWindowCollectionBehavior, NSWindowStyleMask, NSWorkspace,
};
use objc2_application_services::{AXError, AXObserver, AXUIElement};
use objc2_core_foundation::{
    kCFRunLoopDefaultMode, CFEqual, CFRetained, CFRunLoop, CFRunLoopSource, CFString, CGPoint,
    CGSize,
};
use objc2_foundation::{
    NSAttributedString, NSDictionary, NSEdgeInsets, NSObject, NSObjectProtocol, NSPoint, NSRect,
    NSRunLoop, NSRunLoopCommonModes, NSSize, NSString, NSTimer,
};
use objc2_quartz_core::{kCAMediaTimingFunctionLinear, CAMediaTimingFunction};

use crate::ax::{self, names};
use crate::config::{self, Config};

const BAR_H: f64 = 24.0; // fallback; the real bar height tracks the system menu bar (§ menu_bar_height)
const NS_STATUS_LEVEL: isize = 25; // draws over the static system menu bar (design v2 §6.3)
// Window-follow poll rate, settle delay, and fade duration are user-configurable — see
// `crate::config::Config` (poll_hz / settle_ms / fade_ms). The running Controller holds a
// live `Config` and reads these at each use-site so the settings pane can change them.
const MENU_RECHECK: Duration = Duration::from_millis(500); // re-read the current app's menus this often
const STANDARD_WINDOW_SUBROLE: &str = "AXStandardWindow"; // only mirror the menu above real windows
const MENU_HIDE_FACTOR: f64 = 1.5; // hide when within this multiple of the menu's length of its start
const OVERFLOW_LABEL: &str = "»"; // the overflow button shown when top menus don't fit the window
const ITEM_SPACING: f64 = 20.0; // gap between top-level menu titles
const BAR_EDGE: f64 = 14.0; // left/right padding inside the bar
const BAR_V_MARGIN: f64 = 6.0; // extra height beyond the system menu bar (vertical breathing room)
const FONT_SIZE: f64 = 13.0; // ~ the system menu-bar font size
const MENU_LEFT_ADJUST: f64 = -13.0; // shift the dropdown left so its item text lines up under the title text
const PILL_MARGIN: f64 = 10.0; // horizontal padding of the active-title highlight pill
const PILL_V_INSET: f64 = 2.0; // top/bottom inset so the pill doesn't touch the bar edges
const CORNER_RADIUS: f64 = 12.0; // matches the macOS window corner radius (rounds the bar's ends)
const WINDOW_GAP: f64 = 2.0; // gap between the window's top edge and the bar
// Nudge the popped menu down so its visual top clears the bar's bottom edge. NSMenu renders its
// top chrome a few points above the requested location, which otherwise overlaps the bar.
const MENU_DROP: f64 = 10.0;

// ---- menu model (elements cached for the current app) -------------------------------------

struct Shortcut {
    key: String,               // the key-equivalent character (lowercased)
    mods: NSEventModifierFlags, // modifier flags to render (⌘⇧⌥⌃)
}
struct ItemEntry {
    title: String,
    is_sep: bool,
    enabled: bool,
    has_submenu: bool, // opens a submenu (expand on hover) rather than firing an action
    shortcut: Option<Shortcut>,
}
struct TopMenu {
    title: String,
    items: Vec<ItemEntry>,
}

/// Field separator used to pack a menu item's full title path into its `representedObject` (leaf
/// items) or a submenu's title (submenu parents). A control char that never occurs in a menu title.
const PATH_SEP: char = '\u{1f}';

fn join_path(path: &[String]) -> String {
    path.join(PATH_SEP.encode_utf8(&mut [0u8; 4]))
}
fn split_path(s: &str) -> Vec<String> {
    if s.is_empty() {
        return Vec::new();
    }
    s.split(PATH_SEP).map(str::to_string).collect()
}

/// Read one level of a menu's items (title, enabled, shortcut, and whether the item opens a
/// submenu). Shared by the eager top-level read and lazy submenu population. `menu` is an AXMenu.
fn read_items(menu: &AXUIElement) -> Vec<ItemEntry> {
    ax::children(menu)
        .into_iter()
        .map(|mi| {
            let t = ax::attr_string(&mi, names::AX_TITLE).unwrap_or_default();
            let enabled = ax::attr_bool(&mi, names::AX_ENABLED).unwrap_or(true);
            let shortcut = ax::attr_string(&mi, names::AX_MENU_ITEM_CMD_CHAR)
                .filter(|c| !c.is_empty())
                .map(|c| Shortcut {
                    key: c.to_lowercase(),
                    mods: ax_mods_to_ns(
                        ax::attr_i64(&mi, names::AX_MENU_ITEM_CMD_MODIFIERS).unwrap_or(0),
                    ),
                });
            // A submenu parent exposes a single AXMenu child; a leaf (or separator) has none.
            let has_submenu = !t.is_empty() && !ax::children(&mi).is_empty();
            ItemEntry {
                is_sep: t.is_empty(),
                enabled,
                has_submenu,
                shortcut,
                title: t,
            }
        })
        .collect()
}

/// Resolve `path` (top-menu title, then submenu titles) against the app's live tree and read the
/// items of the menu it names — the last component is the submenu-parent whose contents we return.
/// Empty if the path no longer resolves (the app rebuilt or the menu is gone).
fn read_menu_at_path(pid: i32, path: &[String]) -> Vec<ItemEntry> {
    let Some((first, rest)) = path.split_first() else {
        return Vec::new();
    };
    let app = ax::app_element(pid);
    ax::set_timeout(&app, 1.0);
    let Some(menubar) = ax::attr_element(&app, names::AX_MENU_BAR) else {
        return Vec::new();
    };
    let mut node = ax::children(&menubar)
        .into_iter()
        .find(|t| ax::attr_string(t, names::AX_TITLE).as_deref() == Some(first.as_str()));
    for comp in rest {
        let Some(n) = node else {
            return Vec::new();
        };
        let Some(menu) = ax::children(&n).into_iter().next() else {
            return Vec::new();
        };
        node = ax::children(&menu)
            .into_iter()
            .find(|it| ax::attr_string(it, names::AX_TITLE).as_deref() == Some(comp.as_str()));
    }
    match node.and_then(|n| ax::children(&n).into_iter().next()) {
        Some(menu) => read_items(&menu),
        None => Vec::new(),
    }
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

/// Cheap read of just the top-level menu titles (no descent into items), for change detection —
/// mirrors the filtering in `rebuild_bar_content` (drop the empty + Apple entries).
fn read_top_titles(app: &AXUIElement) -> Vec<String> {
    let mut titles = Vec::new();
    if let Some(menubar) = ax::attr_element(app, names::AX_MENU_BAR) {
        for top in ax::children(&menubar) {
            let Some(title) = ax::attr_string(&top, names::AX_TITLE) else {
                continue;
            };
            if title.is_empty() || title == "Apple" {
                continue;
            }
            titles.push(title);
        }
    }
    titles
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
    highlight: Option<Retained<NSView>>,   // pill behind the active title while its menu is open
    buttons: Vec<Retained<NSButton>>,      // the VISIBLE top-level title buttons (for hover hit-testing)
    overflow_from: Option<usize>,          // model index where overflowed (»-button) menus begin
    win_width: f64,                        // last focused-window width (drives the overflow layout)
    laid_out_width: f64,                   // window width the current bar layout was fitted to
    open_menu: Option<Retained<NSMenu>>,   // the currently-tracking dropdown (to cancel on switch)
    pending_switch: Option<usize>,         // peer title to open after cancelling the current one
    menu_left: f64,                         // AX x of the real menus' left edge (Apple menu); per-display
    menu_right: f64,                        // AX x of the real menus' right edge (last app menu)
    fade_gen: u64,                          // bumped on every show/hide so stale fade completions no-op
    current_window: Option<CFRetained<AXUIElement>>, // focused window identity (detects window switches)
    config: Config,                         // live user settings (timings); edited via the settings pane
    tick_timer: Option<Retained<NSTimer>>,  // the reconciliation timer (recreated when poll rate changes)
    last_menu_check: Option<Instant>,       // last periodic menu re-read (picks up late-populating menus)
    hotkey: Option<crate::hotkey::HotkeyRegistration>, // global command-palette hotkey (RAII)
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
            // The sender of a menu item's action is the NSMenuItem itself, not a button.
            if let Some(mi) = sender.and_then(|s| s.downcast_ref::<NSMenuItem>()) {
                self.on_item_clicked(mi);
            }
        }

        #[unsafe(method(openOverflow:))]
        fn open_overflow_(&self, sender: Option<&AnyObject>) {
            if let Some(b) = sender.and_then(|s| s.downcast_ref::<NSButton>()) {
                self.on_overflow_clicked(b);
            }
        }

        // NSMenuDelegate: a submenu is about to display -> populate it lazily from the live tree.
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update_(&self, menu: &NSMenu) {
            self.on_menu_needs_update(menu);
        }

        #[unsafe(method(checkSwitch:))]
        fn check_switch_(&self, _timer: &NSTimer) {
            self.check_switch();
        }

        #[unsafe(method(openSettingsDeferred:))]
        fn open_settings_deferred_(&self, _timer: &NSTimer) {
            self.open_settings();
        }

        #[unsafe(method(openSettings:))]
        fn open_settings_(&self, _sender: Option<&AnyObject>) {
            self.open_settings();
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
            highlight: None,
            buttons: Vec::new(),
            overflow_from: None,
            win_width: 0.0,
            laid_out_width: 0.0,
            open_menu: None,
            pending_switch: None,
            menu_left: f64::INFINITY,
            menu_right: f64::NEG_INFINITY,
            fade_gen: 0,
            current_window: None,
            config: config::load(),
            tick_timer: None,
            last_menu_check: None,
            hotkey: None,
        };
        let this = Self::alloc(mtm).set_ivars(Ivars {
            inner: RefCell::new(inner),
        });
        unsafe { msg_send![super(this), init] }
    }

    /// Install the menu-bar status item and start the timers.
    pub fn start(&self) {
        self.setup_status_item();
        // Window-follow reconciliation (default mode; paused while a menu tracks). Stored so a
        // poll-rate change from the settings pane can recreate it at the new interval.
        self.restart_tick_timer();
        unsafe {
            // Hover switch-watcher, in COMMON modes so it also fires during the menu's modal
            // tracking loop — lets us switch menus when the mouse moves to a peer title.
            let watcher = NSTimer::timerWithTimeInterval_target_selector_userInfo_repeats(
                0.02,
                self,
                sel!(checkSwitch:),
                None,
                true,
            );
            NSRunLoop::currentRunLoop().addTimer_forMode(&watcher, NSRunLoopCommonModes);
        }
        self.register_hotkey();
        crate::settings::apply_theme(self.mtm(), self.ivars().inner.borrow().config.theme);
    }

    /// (Re)register the global command-palette hotkey from config. Drops any prior registration
    /// first (RAII unregisters); a no-op beyond that when the palette is disabled.
    fn register_hotkey(&self) {
        self.ivars().inner.borrow_mut().hotkey = None; // unregister the old chord
        let (enabled, chord) = {
            let inner = self.ivars().inner.borrow();
            (inner.config.palette_enabled, inner.config.palette_hotkey)
        };
        if !enabled {
            return;
        }
        let this = self.retain();
        match crate::hotkey::HotkeyRegistration::install(chord.mods, chord.keycode, move || {
            this.on_palette_hotkey()
        }) {
            Ok(reg) => self.ivars().inner.borrow_mut().hotkey = Some(reg),
            Err(e) => tracing::error!("command-palette hotkey registration failed: {e}"),
        }
    }

    /// The command-palette hotkey fired (on the main run loop): open the palette for the app that
    /// is frontmost *now* (before the palette activates Lintel and steals key focus).
    fn on_palette_hotkey(&self) {
        let Some(pid) = frontmost_pid() else {
            return;
        };
        if pid == std::process::id() as i32 {
            return; // never target ourselves
        }
        tracing::debug!("palette hotkey fired, frontmost pid={pid}");
        crate::palette::open(self.mtm(), pid);
    }

    /// (Re)create the reconciliation timer at the configured poll rate, invalidating any prior one.
    fn restart_tick_timer(&self) {
        let interval = self.ivars().inner.borrow().config.tick_interval();
        if let Some(old) = self.ivars().inner.borrow_mut().tick_timer.take() {
            old.invalidate();
        }
        let timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                interval,
                self,
                sel!(tick:),
                None,
                true,
            )
        };
        self.ivars().inner.borrow_mut().tick_timer = Some(timer);
    }

    /// Snapshot the live config (for the settings pane's `read` closure).
    fn current_config(&self) -> Config {
        self.ivars().inner.borrow().config.clone()
    }

    /// Adopt a config edited in the settings pane: store it, live-apply (restart the timer if the
    /// poll rate changed, (un)register the login item if that flipped), and persist to disk.
    fn apply_and_save_config(&self, cfg: Config) {
        let cfg = cfg.sanitized();
        let (poll_changed, login_changed, theme_changed, hotkey_changed) = {
            let inner = self.ivars().inner.borrow();
            (
                inner.config.poll_hz != cfg.poll_hz,
                inner.config.launch_at_login != cfg.launch_at_login,
                inner.config.theme != cfg.theme,
                inner.config.palette_enabled != cfg.palette_enabled
                    || inner.config.palette_hotkey != cfg.palette_hotkey,
            )
        };
        self.ivars().inner.borrow_mut().config = cfg.clone();
        if poll_changed {
            self.restart_tick_timer();
        }
        if login_changed {
            crate::settings::set_login_item(cfg.launch_at_login);
        }
        if theme_changed {
            crate::settings::apply_theme(self.mtm(), cfg.theme);
        }
        if hotkey_changed {
            self.register_hotkey();
        }
        config::save(&cfg);
    }

    /// Open the settings window on the next run-loop pass (used at launch, once the app is up).
    pub fn open_settings_soon(&self) {
        unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                0.1,
                self,
                sel!(openSettingsDeferred:),
                None,
                false,
            );
        }
    }

    /// Open the settings window, wiring its read/write closures back to this controller.
    pub fn open_settings(&self) {
        let r = self.retain();
        let read: std::rc::Rc<dyn Fn() -> Config> = std::rc::Rc::new(move || r.current_config());
        let w = self.retain();
        let write: std::rc::Rc<dyn Fn(Config)> =
            std::rc::Rc::new(move |cfg| w.apply_and_save_config(cfg));
        crate::settings::open(self.mtm(), read, write);
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
            let settings = menu.addItemWithTitle_action_keyEquivalent(
                &NSString::from_str("Settings…"),
                Some(sel!(openSettings:)),
                &NSString::from_str(","),
            );
            settings.setTarget(Some(target));
            menu.addItem(&NSMenuItem::separatorItem(mtm));
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

        // App focus change: kill the old app's bar instantly and build the new app's bar; reconcile
        // then fades the new bar in.
        if self.ivars().inner.borrow().current_pid != pid {
            self.begin_refocus(pid);
        }

        let app = ax::app_element(pid);
        ax::set_timeout(&app, 1.0);
        let Some(win) = ax::focused_window(&app) else {
            self.hide_all();
            return;
        };

        // The focused window is a transient sub-window (popover / sheet / dialog / floating panel)
        // if its subrole isn't AXStandardWindow. We don't reposition the bar onto those — a menu
        // bar doesn't belong above a popover. But if we're already showing THIS app's bar, we keep
        // it in place above its parent window (like macOS keeps the real menu bar active while a
        // popover/sheet is open — e.g. Chrome's tab-search popover keeps the Chrome menu up).
        // Otherwise (a different app's transient, e.g. a TCC prompt owned by UserNotificationCenter,
        // or nothing showing) we hide. Checked before the refocus logic so a transient never drags
        // the bar off its parent window.
        let subrole = ax::attr_string(&win, names::AX_SUBROLE);
        if subrole.as_deref() != Some(STANDARD_WINDOW_SUBROLE) {
            let keep = {
                let inner = self.ivars().inner.borrow();
                inner.shown && inner.current_pid == pid
            };
            tracing::debug!(
                "transient focused window (subrole={subrole:?}) -> {}",
                if keep { "keep bar in place" } else { "hide" }
            );
            if !keep {
                self.hide_all();
            }
            return;
        }

        // Focus moved to a different window of the SAME app (a genuine focus change, not a move of
        // the same window): kill + refocus onto it too. `begin_refocus` clears `current_window`, so
        // this fires once; the block below re-establishes the new window's identity.
        let win_changed = {
            let inner = self.ivars().inner.borrow();
            inner
                .current_window
                .as_deref()
                .is_some_and(|w| !same_element(w, &win))
        };
        if win_changed {
            tracing::debug!("window focus changed -> refocus");
            self.begin_refocus(pid);
        }
        // Remember the focused window (only when unset) so the next tick can detect a switch away
        // from it; once set it stays until a refocus clears it (avoids a retain/release each tick).
        {
            let mut inner = self.ivars().inner.borrow_mut();
            if inner.current_window.is_none() {
                inner.current_window = Some(win.clone());
            }
        }

        // Periodically re-read the menus so a menu bar that populates AFTER focus (Electron / lazy
        // apps that had no menu at focus time) gets picked up without a manual refocus. Cheap: only
        // compares top-level titles; a full rebuild happens only when they actually change.
        self.recheck_menus(&app, pid);

        // Arm/refresh the event-driven move observer on the current window (re-arm only here,
        // never inside the callback), then position (this is also the 60 Hz fallback).
        self.ensure_observer(pid, &win);
        self.reconcile(&win);
    }

    /// Low-frequency menu refresh: every `MENU_RECHECK`, compare the live top-level menu titles to
    /// the ones we're showing and rebuild if they differ. Skipped while a dropdown is open. Never
    /// wipes to empty on a transient read failure (only rebuilds when the new titles are non-empty).
    fn recheck_menus(&self, app: &AXUIElement, pid: i32) {
        let now = Instant::now();
        let due = {
            let inner = self.ivars().inner.borrow();
            inner.open_top.is_none()
                && inner
                    .last_menu_check
                    .map_or(true, |t| now >= t + MENU_RECHECK)
        };
        if !due {
            return;
        }
        self.ivars().inner.borrow_mut().last_menu_check = Some(now);
        let titles = read_top_titles(app);
        if titles.is_empty() {
            return; // treat a menu-less read as transient; don't blank an existing bar
        }
        let changed = {
            let inner = self.ivars().inner.borrow();
            titles.len() != inner.model.len()
                || titles.iter().zip(inner.model.iter()).any(|(t, m)| *t != m.title)
        };
        if changed {
            tracing::debug!("menu titles changed -> rebuild");
            self.rebuild_bar_content(pid);
        }
    }

    /// Handle a focus change: kill the current bar instantly (no fade-out) and build the new app's
    /// bar. `reconcile` then fades the new bar in. `current_window` is cleared so the caller
    /// re-establishes the newly focused window's identity.
    fn begin_refocus(&self, pid: i32) {
        let bar = {
            let mut inner = self.ivars().inner.borrow_mut();
            inner.current_window = None;
            inner.shown.then(|| {
                inner.shown = false;
                inner.bar.clone()
            })
        };
        if let Some(bar) = bar {
            tracing::debug!("begin_refocus -> kill + rebuild (pid {pid})");
            self.hide_fast(bar); // drop the old app's bar instantly
        }
        self.rebuild_bar_content(pid); // build the new app's bar; reconcile fades it in
    }

    /// AXObserver callback: the focused window just started moving/resizing. Hide the bar at once
    /// (so it never visibly chases) and mark it moving; the timer re-pins it once things settle.
    fn begin_move(&self) {
        let now = Instant::now();
        let bar = {
            let mut inner = self.ivars().inner.borrow_mut();
            inner.moving = true;
            inner.settle_at = Some(now + Duration::from_millis(inner.config.settle_ms as u64));
            inner.shown.then(|| {
                inner.shown = false;
                inner.bar.clone()
            })
        };
        if let Some(bar) = bar {
            tracing::debug!("begin_move -> hide");
            self.hide_fast(bar); // pop out fast during a move; re-pin (fade) once settled
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
        if should_hide(self.mtm(), pos, size) {
            self.hide_all();
            return;
        }
        // Nothing to mirror: the app exposes no menus (e.g. an accessory / LSUIElement app like
        // Canopy has no menu bar). Don't show an empty bar. A late-loading menu is picked up by the
        // periodic recheck (empty -> populated), which then shows the bar.
        if self.ivars().inner.borrow().model.is_empty() {
            self.hide_all();
            return;
        }
        // Re-fit the bar to the current window width so the overflow (») button appears/disappears
        // as the window is resized. Cheap (no AX); skipped while a dropdown is open or empty.
        {
            let relayout = {
                let inner = self.ivars().inner.borrow();
                (size.width - inner.laid_out_width).abs() > 1.0
                    && inner.open_top.is_none()
                    && !inner.model.is_empty()
            };
            if relayout {
                self.layout_bar(size.width);
            }
            self.ivars().inner.borrow_mut().win_width = size.width;
        }
        // Hide only when the bar would sit near the actual system MENUS: vertically within the
        // menu-bar strip AND horizontally within MENU_HIDE_FACTOR x the menus' length of where they
        // start. A window off to the side (bar over empty menu-bar space) still shows and overlaps.
        // Menu coords are per-display and go NEGATIVE on non-primary monitors, so we use the real
        // menu span (min..max, never seeded at 0) — a window far to the right of the menus on a
        // left-hand display (negative x) must not be treated as covering them.
        let (bar_size, menu_left, menu_right) = {
            let inner = self.ivars().inner.borrow();
            (inner.bar_size, inner.menu_left, inner.menu_right)
        };
        let bar_h = bar_size.height;
        let in_strip = pos.y < menu_bar_height() + WINDOW_GAP + bar_h;
        let has_menus = menu_right >= menu_left; // false until we've read menu geometry
        let hide_x = menu_left + MENU_HIDE_FACTOR * (menu_right - menu_left);
        let over_menus = has_menus && pos.x < hide_x;
        // Camera-notch: while up in the top strip, hide if the bar's x-span would slide under the
        // notch of the window's display (it would occlude the middle of the menu). x is shared by AX
        // and Cocoa, so we compare directly.
        let over_notch = notch_x_span(self.mtm(), pos.x + size.width / 2.0)
            .is_some_and(|(nl, nr)| pos.x < nr && pos.x + bar_size.width > nl);
        if in_strip && (over_menus || over_notch) {
            tracing::debug!(
                "hide: {} (winx={:.0} barw={:.0} hide_x={:.0} span=[{:.0},{:.0}])",
                if over_notch { "under notch" } else { "near system menus" },
                pos.x, bar_size.width, hide_x, menu_left, menu_right
            );
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
                    inner.settle_at = Some(now + Duration::from_millis(inner.config.settle_ms as u64));
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
        match action {
            Action::Show(x, y_top, bar) => {
                tracing::debug!("show winpos=({:.0},{:.0}) baro=({x:.0},{y_top:.0})", pos.x, pos.y);
                bar.setFrameOrigin(NSPoint::new(x, y_top + WINDOW_GAP));
                self.show_animated(bar); // fresh focus / settle re-pin -> fade in
            }
            Action::Hide(bar) => {
                tracing::debug!("hide (moving)");
                self.hide_fast(bar); // window is moving -> pop out fast
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
        tracing::debug!("arm_move_observer pid={pid}");
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
        let app = ax::app_element(pid);
        ax::set_timeout(&app, 1.0);

        let mut model = Vec::new();
        // Real system menus' horizontal span (include the Apple menu). Seeded to an empty range so
        // negative per-display coordinates aren't clamped to 0 (a left-hand monitor reports menus at
        // negative x); left as empty if no item exposes geometry.
        let mut menu_left = f64::INFINITY;
        let mut menu_right = f64::NEG_INFINITY;
        if let Some(menubar) = ax::attr_element(&app, names::AX_MENU_BAR) {
            for top in ax::children(&menubar) {
                if let (Some(p), Some(s)) = (
                    ax::attr_point(&top, names::AX_POSITION),
                    ax::attr_size(&top, names::AX_SIZE),
                ) {
                    menu_left = menu_left.min(p.x);
                    menu_right = menu_right.max(p.x + s.width);
                }
                let Some(title) = ax::attr_string(&top, names::AX_TITLE) else {
                    continue;
                };
                if title.is_empty() || title == "Apple" {
                    continue; // leave the Apple menu on the real system menu bar
                }
                let items = ax::children(&top)
                    .into_iter()
                    .next() // the single AXMenu child
                    .map(|menu| read_items(&menu))
                    .unwrap_or_default();
                model.push(TopMenu { title, items });
            }
        }

        tracing::debug!(
            "bar titles: {:?}",
            model.iter().map(|t| t.title.as_str()).collect::<Vec<_>>()
        );

        let avail = {
            let mut inner = self.ivars().inner.borrow_mut();
            inner.model = model;
            inner.current_pid = pid;
            inner.open_top = None;
            inner.last_frame = None; // bar size may change; reposition on the next tick
            inner.menu_left = menu_left;
            inner.menu_right = menu_right;
            // Lay out to the known window width; INFINITY (no clamp) until reconcile learns it.
            if inner.win_width > 1.0 { inner.win_width } else { f64::INFINITY }
        };
        self.layout_bar(avail);
    }

    /// Build the bar's content view + top-level buttons from the stored model, fitting into `avail`
    /// px of window width. When the menus don't all fit, show as many as do plus a `»` overflow
    /// button that opens the rest as submenus. Cheap (no AX) — re-run when the window width changes.
    fn layout_bar(&self, avail: f64) {
        let mtm = self.mtm();
        let (effect, stack, highlight) = make_content(mtm);
        let (font, bold) = menu_bar_fonts(mtm);
        let target: &AnyObject = self;

        let titles: Vec<(String, bool)> = {
            let inner = self.ivars().inner.borrow();
            inner
                .model
                .iter()
                .enumerate()
                .map(|(i, t)| (t.title.clone(), i == 0)) // first (app) menu is bold, like macOS
                .collect()
        };
        let n = titles.len();

        // Build + measure each top-level button (tag = model index).
        let mut btns = Vec::with_capacity(n);
        let mut widths = Vec::with_capacity(n);
        for (title, is_first) in &titles {
            let f: &NSFont = if *is_first { &bold } else { &font };
            let btn =
                make_button(mtm, title, f, true, target, sel!(topClicked:), btns.len() as isize);
            widths.push(btn.fittingSize().width);
            btns.push(btn);
        }

        // Natural width of all buttons; if it exceeds the window, fit a prefix + overflow button.
        let natural = 2.0 * BAR_EDGE
            + widths.iter().sum::<f64>()
            + ITEM_SPACING * (n.saturating_sub(1)) as f64;
        let mut visible = n;
        let mut overflow_from = None;
        let mut overflow_btn = None;
        if n > 0 && natural > avail {
            let ov = make_button(mtm, OVERFLOW_LABEL, &font, true, target, sel!(openOverflow:), -1);
            let mut used = 2.0 * BAR_EDGE + ov.fittingSize().width + ITEM_SPACING;
            let mut k = 0usize;
            for w in &widths {
                let add = w + if k > 0 { ITEM_SPACING } else { 0.0 };
                if used + add <= avail {
                    used += add;
                    k += 1;
                } else {
                    break;
                }
            }
            visible = k;
            overflow_from = Some(k);
            overflow_btn = Some(ov);
        }
        tracing::debug!(
            "layout: avail={avail:.0} natural={natural:.0} n={n} visible={visible} overflow={}",
            overflow_from.is_some()
        );

        let mut buttons = Vec::with_capacity(visible);
        for btn in btns.into_iter().take(visible) {
            stack.addArrangedSubview(&btn);
            buttons.push(btn);
        }
        if let Some(ov) = overflow_btn {
            stack.addArrangedSubview(&ov);
        }

        // Height is the system menu bar plus a little vertical margin; width fits the shown buttons.
        let fit = stack.fittingSize();
        let size = CGSize::new(fit.width, menu_bar_height() + BAR_V_MARGIN);
        let bar = {
            let mut inner = self.ivars().inner.borrow_mut();
            inner.bar_size = size;
            inner.highlight = Some(highlight);
            inner.buttons = buttons;
            inner.overflow_from = overflow_from;
            inner.laid_out_width = avail;
            inner.bar.clone()
        };
        bar.setContentView(Some(&effect));
        bar.setContentSize(size);
        stack.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), size));
    }

    fn on_top_clicked(&self, button: &NSButton) {
        self.open_menu_at(button.tag() as usize);
    }

    /// Open the top-level menu at `first`, then keep switching whenever the hover watcher
    /// (`check_switch`) cancels the current menu because the mouse moved to a peer title —
    /// giving system-style menu-bar tracking. `NSMenu` dismisses on click-outside natively.
    fn open_menu_at(&self, first: usize) {
        let mtm = self.mtm();
        let mut idx = first;
        loop {
            // Build the native NSMenu for `idx` and grab its title button (for positioning).
            let menu = NSMenu::new(mtm);
            menu.setAutoenablesItems(false); // honor our explicit per-item enabled state
            let button = {
                let inner = self.ivars().inner.borrow();
                let Some(top) = inner.model.get(idx) else {
                    return;
                };
                self.build_menu_items(&menu, std::slice::from_ref(&top.title), &top.items);
                inner.buttons.get(idx).cloned()
            };
            let Some(button) = button else {
                return;
            };

            let (bar, bar_h, hl) = {
                let mut inner = self.ivars().inner.borrow_mut();
                inner.open_top = Some(idx);
                inner.open_menu = Some(menu.clone());
                inner.pending_switch = None;
                (
                    inner.bar.clone(),
                    inner.bar_size.height,
                    inner.highlight.clone(),
                )
            };

            // Position under the title (below the bar's bottom edge) and show the highlight pill.
            let btn = bar.convertRectToScreen(button.convertRect_toView(button.bounds(), None));
            let bar_frame = bar.frame();
            let loc = NSPoint::new(
                btn.origin.x + MENU_LEFT_ADJUST,
                bar_frame.origin.y - MENU_DROP,
            );
            if let Some(hl) = &hl {
                let bf = button.frame();
                hl.setFrame(NSRect::new(
                    NSPoint::new(bf.origin.x - PILL_MARGIN, PILL_V_INSET),
                    NSSize::new(bf.size.width + 2.0 * PILL_MARGIN, bar_h - 2.0 * PILL_V_INSET),
                ));
                hl.setHidden(false);
            }

            menu.popUpMenuPositioningItem_atLocation_inView(None, loc, None); // blocks (modal)

            if let Some(hl) = &hl {
                hl.setHidden(true);
            }
            self.ivars().inner.borrow_mut().open_menu = None;

            match self.ivars().inner.borrow_mut().pending_switch.take() {
                Some(next) => idx = next, // hover moved to a peer title -> reopen it
                None => break,            // dismissed or an item was selected
            }
        }
        self.ivars().inner.borrow_mut().open_top = None;
    }

    /// Fires in common run-loop modes (so it runs during the menu's modal tracking): if a menu is
    /// open and the mouse is over a *different* top-level title, cancel the current menu so
    /// `open_menu_at` reopens the peer's.
    fn check_switch(&self) {
        let loc = NSEvent::mouseLocation();
        let (menu, target, cur) = {
            let inner = self.ivars().inner.borrow();
            let Some(menu) = inner.open_menu.clone() else {
                return;
            };
            let mut target = None;
            for (i, btn) in inner.buttons.iter().enumerate() {
                let f = inner
                    .bar
                    .convertRectToScreen(btn.convertRect_toView(btn.bounds(), None));
                if loc.x >= f.origin.x
                    && loc.x <= f.origin.x + f.size.width
                    && loc.y >= f.origin.y
                    && loc.y <= f.origin.y + f.size.height
                {
                    target = Some(i);
                    break;
                }
            }
            (menu, target, inner.open_top)
        };
        if let Some(t) = target {
            if Some(t) != cur {
                self.ivars().inner.borrow_mut().pending_switch = Some(t);
                menu.cancelTrackingWithoutAnimation();
            }
        }
    }

    /// Populate `menu` with `items` under `base_path` (the path of `menu` itself: `[top]` for a top
    /// dropdown, `[top, sub, …]` for a submenu). Leaves fire via `itemClicked:` carrying their full
    /// title path in `representedObject`; submenu parents get an empty child menu whose contents this
    /// same routine fills lazily on `menuNeedsUpdate:` (so slow menus like Services aren't read until
    /// hovered, and dynamic ones re-read each open). Firing re-resolves the path — no cached handles.
    fn build_menu_items(&self, menu: &NSMenu, base_path: &[String], items: &[ItemEntry]) {
        let mtm = self.mtm();
        let target: &AnyObject = self;
        for it in items {
            if it.is_sep {
                menu.addItem(&NSMenuItem::separatorItem(mtm));
                continue;
            }
            let mut path = base_path.to_vec();
            path.push(it.title.clone());
            if it.has_submenu {
                let item = unsafe {
                    menu.addItemWithTitle_action_keyEquivalent(
                        &NSString::from_str(&it.title),
                        None, // a submenu parent expands on hover rather than firing
                        &NSString::from_str(""),
                    )
                };
                item.setEnabled(it.enabled);
                let submenu = NSMenu::new(mtm);
                submenu.setAutoenablesItems(false);
                submenu.setTitle(&NSString::from_str(&join_path(&path))); // path key for the delegate
                unsafe {
                    let _: () = msg_send![&*submenu, setDelegate: target];
                }
                item.setSubmenu(Some(&submenu));
            } else {
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
                unsafe {
                    item.setTarget(Some(target));
                    item.setRepresentedObject(Some(&NSString::from_str(&join_path(&path))));
                }
            }
        }
    }

    /// A leaf menu item was chosen: read its full title path from `representedObject` and fire it by
    /// re-resolving that path against the app's live tree (`palette::fire`), which handles any depth
    /// and stays correct when the app rebuilt or lazily repopulated its menus.
    fn on_item_clicked(&self, item: &NSMenuItem) {
        let path = item
            .representedObject()
            .and_then(|o| o.downcast_ref::<NSString>().map(NSString::to_string))
            .map(|s| split_path(&s))
            .unwrap_or_default();
        if path.is_empty() {
            return;
        }
        let pid = self.ivars().inner.borrow().current_pid;
        if !crate::palette::fire(pid, &path) {
            tracing::debug!("menu fire could not resolve {path:?}");
        }
    }

    /// The `»` overflow button was clicked: pop up a menu whose items are the overflowed top-level
    /// menus, each carrying its own items as a submenu (deeper submenus expand lazily as usual).
    fn on_overflow_clicked(&self, button: &NSButton) {
        let mtm = self.mtm();
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        {
            let inner = self.ivars().inner.borrow();
            let Some(start) = inner.overflow_from else {
                return;
            };
            for top in &inner.model[start..] {
                let parent = unsafe {
                    menu.addItemWithTitle_action_keyEquivalent(
                        &NSString::from_str(&top.title),
                        None,
                        &NSString::from_str(""),
                    )
                };
                let submenu = NSMenu::new(mtm);
                submenu.setAutoenablesItems(false);
                self.build_menu_items(&submenu, std::slice::from_ref(&top.title), &top.items);
                parent.setSubmenu(Some(&submenu));
            }
        }
        let bar = self.ivars().inner.borrow().bar.clone();
        let btn = bar.convertRectToScreen(button.convertRect_toView(button.bounds(), None));
        let loc = NSPoint::new(btn.origin.x + MENU_LEFT_ADJUST, bar.frame().origin.y - MENU_DROP);
        menu.popUpMenuPositioningItem_atLocation_inView(None, loc, None);
    }

    /// NSMenuDelegate: a submenu is about to display. Its title carries its path (set when we built
    /// the parent item); re-resolve that path against the live tree and (re)fill the submenu.
    fn on_menu_needs_update(&self, menu: &NSMenu) {
        let path = split_path(&menu.title().to_string());
        if path.is_empty() {
            return;
        }
        let pid = self.ivars().inner.borrow().current_pid;
        let items = read_menu_at_path(pid, &path);
        menu.removeAllItems();
        self.build_menu_items(menu, &path, &items);
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
            let was_shown = inner.shown;
            inner.last_frame = None; // force a fresh show when an eligible window returns
            inner.moving = false;
            inner.shown = false;
            was_shown.then(|| inner.bar.clone())
        };
        if let Some(bar) = bar {
            self.hide_animated(bar); // non-move hide (fullscreen/maximized/over-menus/no-window) -> fade out
        }
    }

    /// Fade the bar in over FADE seconds (used for non-move shows: settle + fresh focus).
    fn show_animated(&self, bar: Retained<NSPanel>) {
        self.ivars().inner.borrow_mut().fade_gen += 1;
        // Only reset to 0 when the bar is off-screen. If it's still visible (a non-move fade-out
        // was interrupted mid-fade), fade up from its current alpha so it doesn't blink to 0 first.
        if !bar.isVisible() {
            bar.setAlphaValue(0.0);
        }
        bar.orderFront(None);
        let fade = self.ivars().inner.borrow().config.fade_secs();
        let b = bar.clone();
        let changes = RcBlock::new(move |ctx: NonNull<NSAnimationContext>| {
            let ctx = unsafe { ctx.as_ref() };
            ctx.setDuration(fade);
            ctx.setTimingFunction(Some(&linear_timing()));
            b.animator().setAlphaValue(1.0);
        });
        NSAnimationContext::runAnimationGroup(&changes);
    }

    /// Hide the bar immediately (used during a window move — pop out fast, no fade, no chase).
    fn hide_fast(&self, bar: Retained<NSPanel>) {
        self.ivars().inner.borrow_mut().fade_gen += 1;
        bar.orderOut(None);
        bar.setAlphaValue(1.0);
    }

    /// Fade the bar out over FADE seconds, then order it out. A generation guard stops a stale
    /// completion from hiding a bar that was shown again during the fade.
    fn hide_animated(&self, bar: Retained<NSPanel>) {
        let my_gen = {
            let mut inner = self.ivars().inner.borrow_mut();
            inner.fade_gen += 1;
            inner.fade_gen
        };
        let fade = self.ivars().inner.borrow().config.fade_secs();
        let b = bar.clone();
        let changes = RcBlock::new(move |ctx: NonNull<NSAnimationContext>| {
            let ctx = unsafe { ctx.as_ref() };
            ctx.setDuration(fade);
            ctx.setTimingFunction(Some(&linear_timing()));
            b.animator().setAlphaValue(0.0);
        });
        let this = self.retain();
        let done = RcBlock::new(move || {
            if this.ivars().inner.borrow().fade_gen == my_gen {
                bar.orderOut(None);
                bar.setAlphaValue(1.0);
            }
        });
        NSAnimationContext::runAnimationGroup_completionHandler(&changes, Some(&done));
    }
}

// ---- free helpers -------------------------------------------------------------------------

/// A linear timing curve so fades progress at a constant rate (the default ease-in-ease-out
/// spends the start/end near-stationary, which reads as a delay on long durations).
fn linear_timing() -> Retained<CAMediaTimingFunction> {
    unsafe { CAMediaTimingFunction::functionWithName(kCAMediaTimingFunctionLinear) }
}

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
    panel.setHasShadow(true); // soft drop shadow around the rounded acrylic bar
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::MoveToActiveSpace
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    panel
}

/// The bar's acrylic content view, a highlight pill (behind), and the horizontal stack of titles.
fn make_content(
    mtm: MainThreadMarker,
) -> (
    Retained<NSVisualEffectView>,
    Retained<NSStackView>,
    Retained<NSView>,
) {
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

    // Highlight pill (added first so it sits behind the titles); positioned + shown on click.
    let highlight = NSView::initWithFrame(NSView::alloc(mtm), NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0)));
    highlight.setWantsLayer(true);
    if let Some(layer) = highlight.layer() {
        // A slight tint of the system accent color.
        let cg = NSColor::controlAccentColor()
            .colorWithAlphaComponent(0.25)
            .CGColor();
        layer.setBackgroundColor(Some(&cg));
        layer.setCornerRadius(CORNER_RADIUS);
    }
    highlight.setHidden(true);
    effect.addSubview(&highlight);

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
    (effect, stack, highlight)
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
    // Opaque black title (an attributed title overrides the vibrancy view's text blending, which
    // otherwise greys the text out).
    btn.setAttributedTitle(&black_title(title, font));
    btn.setEnabled(enabled);
    btn.setTag(tag);
    btn
}

/// An attributed string that renders `text` in opaque black at `font` (used for the bar titles so
/// they don't get vibrancy-blended by the acrylic background).
fn black_title(text: &str, font: &NSFont) -> Retained<NSAttributedString> {
    let color = NSColor::colorWithWhite_alpha(0.0, 1.0); // opaque black
    let color_obj: &AnyObject = &color;
    let font_obj: &AnyObject = font;
    let attrs = NSDictionary::from_slices(
        &[
            unsafe { NSForegroundColorAttributeName },
            unsafe { NSFontAttributeName },
        ],
        &[color_obj, font_obj],
    );
    unsafe {
        NSAttributedString::initWithString_attributes(
            NSAttributedString::alloc(),
            &NSString::from_str(text),
            Some(&attrs),
        )
    }
}

/// The standard system menu-bar height.
fn menu_bar_height() -> f64 {
    NSStatusBar::systemStatusBar().thickness()
}

/// The horizontal span `[left, right]` of the camera notch on the screen containing `probe_x`
/// (AX and Cocoa x-coordinates are equal), or `None` if that screen has no notch. The notch is the
/// gap between the two top auxiliary areas; `safeAreaInsets().top > 0` marks a notched display.
fn notch_x_span(mtm: MainThreadMarker, probe_x: f64) -> Option<(f64, f64)> {
    let screens = NSScreen::screens(mtm);
    for i in 0..screens.count() {
        let s = screens.objectAtIndex(i);
        let fr = s.frame();
        if probe_x >= fr.origin.x && probe_x < fr.origin.x + fr.size.width {
            if s.safeAreaInsets().top <= 0.0 {
                return None; // this screen has no notch
            }
            let al = s.auxiliaryTopLeftArea();
            let ar = s.auxiliaryTopRightArea();
            let left = al.origin.x + al.size.width;
            let right = ar.origin.x;
            return (right > left).then_some((left, right));
        }
    }
    None
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

/// Hide when the focused window is (near-)fullscreen or maximized AND is actually covering the top
/// of the screen (design v2 §5.4, MVP heuristic). A maximized-SIZE window dragged down from the top
/// leaves room above it for the bar, so we only hide while its top edge is up in the menu-bar row.
fn should_hide(mtm: MainThreadMarker, pos: CGPoint, size: CGSize) -> bool {
    let Some(screen) = NSScreen::mainScreen(mtm) else {
        return false;
    };
    let frame = screen.frame();
    let visible = screen.visibleFrame();
    // `pos` is AX top-left (y down). A window filling the visible area sits just below the menu bar;
    // a fullscreen window covers it. Once dragged down, pos.y grows past this and we stop hiding.
    let at_top = pos.y <= menu_bar_height() + 6.0;
    // Fullscreen: window ~= whole display. Maximized: window ~= usable (visible) area.
    let fullscreen = size.height >= frame.size.height - 2.0;
    let maximized =
        size.height >= visible.size.height - 2.0 && size.width >= visible.size.width - 2.0;
    at_top && (fullscreen || maximized)
}
