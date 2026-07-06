//! Phase 1 MVP overlay: a floating acrylic bar pinned above the focused window that mirrors the
//! top-level menus; clicking a menu drops down its first-level items and fires the real action via
//! `AXPress`. Timer-driven (the reconciliation loop of design v2 §5.3 as the MVP's primary driver).
//!
//! Simplifications vs the full design (tracked as TODO for later phases):
//!   * one reconciliation timer instead of AXObservers (§5.2)
//!   * dropdown left-aligned under the bar, not under the clicked item; single level deep
//!   * presses the cached leaf element (no re-resolve-by-path yet — fine for static/native menus)

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSButton, NSColor, NSPanel, NSScreen, NSStackView,
    NSUserInterfaceLayoutOrientation, NSVisualEffectBlendingMode, NSVisualEffectMaterial,
    NSVisualEffectState, NSVisualEffectView, NSWindowCollectionBehavior, NSWindowStyleMask,
    NSWorkspace,
};
use objc2_application_services::AXUIElement;
use objc2_core_foundation::{CFRetained, CGPoint, CGSize};
use objc2_foundation::{NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSTimer};

use crate::ax::{self, names};

const BAR_H: f64 = 26.0;
const NS_STATUS_LEVEL: isize = 25; // draws over the static system menu bar (design v2 §6.3)
const NS_POPUP_LEVEL: isize = 101; // Lintel's own dropdown

// ---- menu model (elements cached for the current app) -------------------------------------

struct ItemEntry {
    title: String,
    el: CFRetained<AXUIElement>,
    is_sep: bool,
}
struct TopMenu {
    title: String,
    items: Vec<ItemEntry>,
}

struct Inner {
    bar: Retained<NSPanel>,
    dropdown: Retained<NSPanel>,
    model: Vec<TopMenu>,
    current_pid: i32,
    bar_size: CGSize,
    open_top: Option<usize>,
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
                self.on_top_clicked(b.tag() as usize);
            }
        }

        #[unsafe(method(itemClicked:))]
        fn item_clicked_(&self, sender: Option<&AnyObject>) {
            if let Some(b) = sender.and_then(|s| s.downcast_ref::<NSButton>()) {
                self.on_item_clicked(b.tag() as usize);
            }
        }
    }
);

impl Controller {
    pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let bar = make_panel(mtm, NS_STATUS_LEVEL);
        let dropdown = make_panel(mtm, NS_POPUP_LEVEL);
        let inner = Inner {
            bar,
            dropdown,
            model: Vec::new(),
            current_pid: 0,
            bar_size: CGSize::new(0.0, BAR_H),
            open_top: None,
        };
        let this = Self::alloc(mtm).set_ivars(Ivars {
            inner: RefCell::new(inner),
        });
        unsafe { msg_send![super(this), init] }
    }

    /// Start the ~10 Hz reconciliation timer.
    pub fn start(&self) {
        unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                0.1,
                self,
                sel!(tick:),
                None,
                true,
            );
        }
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
        let (Some(pos), Some(size)) = (
            ax::attr_point(&win, names::AX_POSITION),
            ax::attr_size(&win, names::AX_SIZE),
        ) else {
            self.hide_all();
            return;
        };

        if should_hide(self.mtm(), size) {
            self.hide_all();
            return;
        }

        let (x, y_top) = self.place(pos);
        let inner = self.ivars().inner.borrow();
        if std::env::var_os("LINTEL_DEBUG").is_some() {
            eprintln!(
                "[dbg] pid={pid} winpos=({:.0},{:.0}) winsize=({:.0},{:.0}) baro=({:.0},{:.0}) barsz=({:.0},{:.0})",
                pos.x, pos.y, size.width, size.height, x, y_top, inner.bar_size.width, inner.bar_size.height
            );
        }
        // Bar's bottom edge sits on the window's top edge (design v2 §8.2): origin.y = y_top.
        inner.bar.setFrameOrigin(NSPoint::new(x, y_top));
        inner.bar.orderFront(None);
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
                if title.is_empty() {
                    continue;
                }
                let items = ax::children(&top)
                    .into_iter()
                    .next() // the single AXMenu child
                    .map(|menu| {
                        ax::children(&menu)
                            .into_iter()
                            .map(|mi| {
                                let t = ax::attr_string(&mi, names::AX_TITLE).unwrap_or_default();
                                ItemEntry {
                                    is_sep: t.is_empty(),
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

        // Build a fresh acrylic content view with one button per top-level menu.
        let (effect, stack) = make_content(mtm, NSUserInterfaceLayoutOrientation::Horizontal);
        let target: &AnyObject = self;
        for (i, top) in model.iter().enumerate() {
            let btn = make_button(mtm, &top.title, target, sel!(topClicked:), i as isize);
            stack.addArrangedSubview(&btn);
        }
        let size = stack.fittingSize();

        let inner_ref = self.ivars();
        let mut inner = inner_ref.inner.borrow_mut();
        inner.bar.setContentView(Some(&effect));
        inner.bar.setContentSize(size);
        stack.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), size));
        inner.bar_size = size;
        inner.model = model;
        inner.current_pid = pid;
        inner.open_top = None;
        inner.dropdown.orderOut(None);
    }

    fn on_top_clicked(&self, idx: usize) {
        let mtm = self.mtm();
        let (effect, stack) = make_content(mtm, NSUserInterfaceLayoutOrientation::Vertical);
        let target: &AnyObject = self;
        {
            let inner = self.ivars().inner.borrow();
            let Some(top) = inner.model.get(idx) else {
                return;
            };
            for (j, it) in top.items.iter().enumerate() {
                if it.is_sep {
                    continue;
                }
                let btn = make_button(mtm, &it.title, target, sel!(itemClicked:), j as isize);
                stack.addArrangedSubview(&btn);
            }
        }
        let size = stack.fittingSize();

        let inner = self.ivars().inner.borrow_mut();
        inner.dropdown.setContentView(Some(&effect));
        inner.dropdown.setContentSize(size);
        stack.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), size));

        // Left-align the dropdown under the bar (MVP; not yet under the clicked item).
        let bar_frame = inner.bar.frame();
        let origin = NSPoint::new(bar_frame.origin.x, bar_frame.origin.y - size.height);
        inner.dropdown.setFrameOrigin(origin);
        inner.dropdown.orderFront(None);
        drop(inner);
        self.ivars().inner.borrow_mut().open_top = Some(idx);
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
        let inner = self.ivars().inner.borrow();
        inner.dropdown.orderOut(None);
    }

    /// Convert an AX window position (global top-left, y-down) to the Cocoa y of the window's top
    /// edge (design v2 §8.2), returning the bar's desired top-left in Cocoa coords.
    fn place(&self, pos: CGPoint) -> (f64, f64) {
        let primary_h = origin_screen_height(self.mtm());
        (pos.x, primary_h - pos.y)
    }

    fn hide_all(&self) {
        let inner = self.ivars().inner.borrow();
        inner.bar.orderOut(None);
        inner.dropdown.orderOut(None);
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
    panel.setHasShadow(true);
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::MoveToActiveSpace
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    panel
}

fn make_content(
    mtm: MainThreadMarker,
    orient: NSUserInterfaceLayoutOrientation,
) -> (Retained<NSVisualEffectView>, Retained<NSStackView>) {
    let effect = NSVisualEffectView::new(mtm);
    effect.setMaterial(NSVisualEffectMaterial::HUDWindow);
    effect.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
    effect.setState(NSVisualEffectState::Active);
    effect.setWantsLayer(true);

    let stack = NSStackView::new(mtm);
    stack.setOrientation(orient);
    stack.setSpacing(2.0);
    effect.addSubview(&stack);
    (effect, stack)
}

fn make_button(
    mtm: MainThreadMarker,
    title: &str,
    target: &AnyObject,
    action: objc2::runtime::Sel,
    tag: isize,
) -> Retained<NSButton> {
    let ns = NSString::from_str(title);
    let btn =
        unsafe { NSButton::buttonWithTitle_target_action(&ns, Some(target), Some(action), mtm) };
    btn.setBordered(false);
    btn.setTag(tag);
    btn
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
