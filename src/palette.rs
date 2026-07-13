//! Command palette — a global-hotkey menu type-ahead search (design:
//! docs/plans/2026-07-12-command-palette-design.md).
//!
//! The focused app's menu tree is walked on a background thread into a flat, streamable index of
//! leaf commands (path + shortcut, `Send` data only — no `AXUIElement` handles cross threads). An
//! activating acrylic panel fuzzy-filters them as you type; Return fires the selection by
//! re-resolving its path against the live tree and `AXPress`ing it.

use std::cell::{Cell, RefCell};
use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, Sel};
use objc2::{
    define_class, msg_send, sel, AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly,
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationOptions, NSBackingStoreType, NSButton, NSColor, NSControl,
    NSFocusRingType, NSFont, NSRunningApplication,
    NSFontAttributeName, NSForegroundColorAttributeName, NSLayoutAttribute, NSLineBreakMode,
    NSMutableParagraphStyle, NSPanel, NSParagraphStyleAttributeName, NSProgressIndicator, NSScreen,
    NSStackView, NSTextAlignment, NSTextField, NSTextView, NSUserInterfaceLayoutOrientation, NSView,
    NSVisualEffectBlendingMode, NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView,
    NSWindowStyleMask,
};
use objc2_application_services::{AXError, AXUIElement};
use objc2_core_foundation::CFRetained;
use objc2_foundation::{
    NSAttributedString, NSDictionary, NSMutableAttributedString, NSNotification, NSPoint, NSRange,
    NSRect, NSSize, NSString, NSTimer,
};

use crate::ax::{self, names};

const PANEL_W: f64 = 620.0;
const MAX_RESULTS: usize = 12;
const FIELD_X: f64 = 18.0; // search field left/right inset (aligned with the indented row text)
const LIST_X: f64 = 10.0; // results left/right inset — margin so the highlight doesn't touch the edges
const TEXT_INSET: f64 = 8.0; // left padding of row text INSIDE the highlight
const TOP: f64 = 12.0; // margin above the field
const FIELD_H: f64 = 28.0;
const SEP_GAP: f64 = 8.0; // gap between field and the results list
const ROW_H: f64 = 28.0; // single-line row height
const ROW_H_HELP: f64 = 44.0; // two-line row (leaf + help text)
const ROW_SPACING: f64 = 1.0;
const BOTTOM: f64 = 10.0; // margin below the last row

// ---- index (built off the main thread) ----------------------------------------------------

/// A single leaf menu command.
#[derive(Clone)]
pub struct Command {
    pub path: Vec<String>,        // e.g. ["Format", "Font", "Bold"]
    pub enabled: bool,
    pub shortcut: Option<String>, // display string, e.g. "⌘⇧B"
    pub help: Option<String>,     // AXHelp tooltip, if the app set one (usually not)
}

impl Command {
    fn row_height(&self) -> f64 {
        if self.help.is_some() {
            ROW_H_HELP
        } else {
            ROW_H
        }
    }
}

#[derive(Default)]
struct IndexState {
    commands: Vec<Command>,
    done: bool,
}

/// Walk `pid`'s whole menu tree into `state` (runs on a background thread).
fn build_index(pid: i32, state: Arc<Mutex<IndexState>>) {
    let app = ax::app_element(pid);
    ax::set_timeout(&app, 1.0);
    if let Some(menubar) = ax::attr_element(&app, names::AX_MENU_BAR) {
        for top in ax::children(&menubar) {
            let Some(title) = ax::attr_string(&top, names::AX_TITLE) else {
                continue;
            };
            if title.is_empty() || title == "Apple" {
                continue;
            }
            walk_menu(&top, vec![title], &state);
        }
    }
    state.lock().unwrap().done = true;
}

/// `item` is a menu-bar item or a submenu item; its single AXMenu child holds the entries.
fn walk_menu(item: &AXUIElement, path: Vec<String>, state: &Arc<Mutex<IndexState>>) {
    let Some(menu) = ax::children(item).into_iter().next() else {
        return;
    };
    for entry in ax::children(&menu) {
        let Some(title) = ax::attr_string(&entry, names::AX_TITLE) else {
            continue;
        };
        if title.is_empty() {
            continue; // separator
        }
        let mut child_path = path.clone();
        child_path.push(title);
        if ax::children(&entry).is_empty() {
            // leaf command
            let cmd = Command {
                enabled: ax::attr_bool(&entry, names::AX_ENABLED).unwrap_or(true),
                shortcut: shortcut_display(&entry),
                help: ax::attr_string(&entry, names::AX_HELP).filter(|h| !h.is_empty()),
                path: child_path,
            };
            state.lock().unwrap().commands.push(cmd);
        } else {
            walk_menu(&entry, child_path, state); // submenu
        }
    }
}

/// A shortcut display string (e.g. "⌘⇧B") from the item's AX cmd-char + modifier mask, or None.
fn shortcut_display(entry: &AXUIElement) -> Option<String> {
    let ch = ax::attr_string(entry, names::AX_MENU_ITEM_CMD_CHAR).filter(|c| !c.is_empty())?;
    let m = ax::attr_i64(entry, names::AX_MENU_ITEM_CMD_MODIFIERS).unwrap_or(0);
    let mut s = String::new();
    if m & 4 != 0 {
        s.push('⌃');
    }
    if m & 2 != 0 {
        s.push('⌥');
    }
    if m & 1 != 0 {
        s.push('⇧');
    }
    if m & 8 == 0 {
        s.push('⌘'); // Command implied unless the NoCommand bit is set
    }
    s.push_str(&ch.to_uppercase());
    Some(s)
}

// ---- fuzzy matching (pure) ----------------------------------------------------------------

/// Subsequence fuzzy score: `Some(score)` if all query chars appear in `text` in order (gaps
/// allowed), higher = better; `None` if no match. Rewards contiguous runs and word-boundary hits.
fn fuzzy_score(query: &str, text: &str) -> Option<i64> {
    let q: Vec<char> = query.to_lowercase().chars().collect();
    if q.is_empty() {
        return Some(0);
    }
    let t: Vec<char> = text.chars().collect();
    let tl: Vec<char> = text.to_lowercase().chars().collect();
    let mut qi = 0;
    let mut score = 0i64;
    let mut prev: Option<usize> = None;
    for i in 0..tl.len() {
        if qi < q.len() && tl[i] == q[qi] {
            let mut pts = 1i64;
            if prev == Some(i.wrapping_sub(1)) {
                pts += 5; // contiguous
            }
            if i == 0 || !t[i - 1].is_alphanumeric() {
                pts += 8; // start of a word
            } else if t[i].is_uppercase() {
                pts += 4; // CamelCase boundary
            }
            score += pts;
            prev = Some(i);
            qi += 1;
        }
    }
    (qi == q.len()).then(|| score - (t.len() as i64) / 10) // slight preference for shorter text
}

/// Score a command against the query: prefer a leaf-title match, fall back to the full path.
fn match_command(query: &str, cmd: &Command) -> Option<i64> {
    let leaf = cmd.path.last().map(String::as_str).unwrap_or("");
    if let Some(s) = fuzzy_score(query, leaf) {
        return Some(s + 100);
    }
    fuzzy_score(query, &cmd.path.join(" ")).map(|s| s - 20)
}

/// The best-ranked commands for `query` (capped at `MAX_RESULTS`).
fn ranked(query: &str, commands: &[Command]) -> Vec<Command> {
    let mut scored: Vec<(i64, &Command)> = commands
        .iter()
        .filter_map(|c| match_command(query, c).map(|s| (s, c)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.into_iter().take(MAX_RESULTS).map(|(_, c)| c.clone()).collect()
}

// ---- firing (main thread; re-resolve the path against the live tree) ----------------------

/// Fire the command at `path` in app `pid` by re-resolving each path component live and `AXPress`.
///
/// Re-resolution matters: menu-item `AXUIElement` handles cached from an earlier tree walk go
/// stale when the app rebuilds or lazily repopulates its menus (Electron, etc.), and `AXPress`
/// on a stale handle silently no-ops. Resolving fresh by title against the live tree at fire
/// time is what actually triggers the action. Returns `true` if a matching leaf was pressed OK.
pub(crate) fn fire(pid: i32, path: &[String]) -> bool {
    if path.is_empty() {
        return false;
    }
    let app = ax::app_element(pid);
    ax::set_timeout(&app, 2.0);
    let Some(menubar) = ax::attr_element(&app, names::AX_MENU_BAR) else {
        return false;
    };
    let mut current: Option<CFRetained<AXUIElement>> = ax::children(&menubar)
        .into_iter()
        .find(|top| ax::attr_string(top, names::AX_TITLE).as_deref() == Some(path[0].as_str()));
    for comp in &path[1..] {
        let Some(node) = current else {
            return false;
        };
        let Some(menu) = ax::children(&node).into_iter().next() else {
            return false;
        };
        current = ax::children(&menu)
            .into_iter()
            .find(|it| ax::attr_string(it, names::AX_TITLE).as_deref() == Some(comp.as_str()));
    }
    if let Some(leaf) = current {
        let err = ax::press(&leaf);
        tracing::debug!("fire {path:?} -> {err:?}");
        return err == AXError::Success;
    }
    false
}

// ---- UI -----------------------------------------------------------------------------------

thread_local! {
    static PANEL: RefCell<Option<Retained<NSPanel>>> = const { RefCell::new(None) };
    static CONTROLLER: RefCell<Option<Retained<PaletteController>>> = const { RefCell::new(None) };
}

define_class!(
    // A borderless panel that can still become key, so the search field receives typing.
    #[unsafe(super(NSPanel))]
    #[thread_kind = MainThreadOnly]
    #[name = "LintelPalettePanel"]
    struct PalettePanel;

    impl PalettePanel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key(&self) -> bool {
            true
        }
    }
);

struct PaletteIvars {
    pid: i32,
    index: Arc<Mutex<IndexState>>,
    panel: Retained<NSPanel>,
    effect: Retained<NSVisualEffectView>,
    tint: Retained<NSView>,
    field: Retained<NSTextField>,
    results: Retained<NSStackView>,
    spinner: Retained<NSProgressIndicator>,
    center_x: Cell<f64>, // fixed Cocoa point the panel is centered on (x, y)
    center_y: Cell<f64>,
    anchor_top: Cell<f64>, // locked Cocoa y of the panel's top edge while a query is active
    matches: RefCell<Vec<Command>>,
    rows: RefCell<Vec<Retained<NSButton>>>,
    selected: Cell<usize>,
    last_len: Cell<usize>,
    last_done: Cell<bool>,
    timer: RefCell<Option<Retained<NSTimer>>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "LintelPaletteController"]
    #[ivars = PaletteIvars]
    struct PaletteController;

    unsafe impl NSObjectProtocol for PaletteController {}

    impl PaletteController {
        // NSControl/NSTextField delegate: query changed.
        #[unsafe(method(controlTextDidChange:))]
        fn text_changed(&self, _note: &NSNotification) {
            self.refilter();
        }

        // Intercept navigation keys while the field keeps focus.
        #[unsafe(method(control:textView:doCommandBySelector:))]
        fn do_command(&self, _c: &NSControl, _tv: &NSTextView, selector: Sel) -> bool {
            if selector == sel!(moveUp:) {
                self.move_selection(-1);
                true
            } else if selector == sel!(moveDown:) {
                self.move_selection(1);
                true
            } else if selector == sel!(insertNewline:) {
                self.fire_selected();
                true
            } else if selector == sel!(cancelOperation:) {
                self.close();
                true
            } else {
                false
            }
        }

        // Window delegate: dismiss when the palette loses key (click-away / app switch).
        #[unsafe(method(windowDidResignKey:))]
        fn resign_key(&self, _note: &NSNotification) {
            self.close();
        }

        // Poll the streaming index; re-run the match as commands arrive.
        #[unsafe(method(poll:))]
        fn poll(&self, _t: &NSTimer) {
            let (len, done) = {
                let g = self.ivars().index.lock().unwrap();
                (g.commands.len(), g.done)
            };
            if len != self.ivars().last_len.get() || done != self.ivars().last_done.get() {
                self.refilter();
            }
        }

        #[unsafe(method(rowClicked:))]
        fn row_clicked(&self, sender: &NSButton) {
            self.ivars().selected.set(sender.tag().max(0) as usize);
            self.fire_selected();
        }
    }
);

impl PaletteController {
    #[allow(clippy::too_many_arguments)]
    fn new(
        mtm: MainThreadMarker,
        pid: i32,
        index: Arc<Mutex<IndexState>>,
        panel: Retained<NSPanel>,
        effect: Retained<NSVisualEffectView>,
        tint: Retained<NSView>,
        field: Retained<NSTextField>,
        results: Retained<NSStackView>,
        spinner: Retained<NSProgressIndicator>,
        center_x: f64,
        center_y: f64,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(PaletteIvars {
            pid,
            index,
            panel,
            effect,
            tint,
            field,
            results,
            spinner,
            center_x: Cell::new(center_x),
            center_y: Cell::new(center_y),
            anchor_top: Cell::new(center_y), // set for real on the first (empty-query) layout
            matches: RefCell::new(Vec::new()),
            rows: RefCell::new(Vec::new()),
            selected: Cell::new(0),
            last_len: Cell::new(usize::MAX),
            last_done: Cell::new(false),
            timer: RefCell::new(None),
        });
        unsafe { msg_send![super(this), init] }
    }

    /// Recompute matches for the current query + streamed index, then rebuild the rows.
    fn refilter(&self) {
        let query = self.ivars().field.stringValue().to_string();
        let (commands, done) = {
            let g = self.ivars().index.lock().unwrap();
            (g.commands.clone(), g.done)
        };
        self.ivars().last_len.set(commands.len());
        self.ivars().last_done.set(done);
        let matches = ranked(&query, &commands);
        // Spinner: still walking and nothing to show yet.
        unsafe {
            if done || !matches.is_empty() {
                self.ivars().spinner.stopAnimation(None);
            } else {
                self.ivars().spinner.startAnimation(None);
            }
        }
        *self.ivars().matches.borrow_mut() = matches;
        self.ivars().selected.set(0);
        self.rebuild_rows();
    }

    fn rebuild_rows(&self) {
        let mtm = self.mtm();
        // Clear existing rows.
        for row in self.ivars().rows.borrow().iter() {
            row.removeFromSuperview();
        }
        let row_w = PANEL_W - 2.0 * LIST_X;
        let matches = self.ivars().matches.borrow();
        let target: &AnyObject = self;
        let mut rows = Vec::with_capacity(matches.len());
        let mut rows_h = 0.0_f64;
        for (i, cmd) in matches.iter().enumerate() {
            let h = cmd.row_height();
            let btn = make_row(mtm, cmd, target, i as isize);
            btn.widthAnchor().constraintEqualToConstant(row_w).setActive(true);
            btn.heightAnchor().constraintEqualToConstant(h).setActive(true);
            self.ivars().results.addArrangedSubview(&btn);
            rows.push(btn);
            rows_h += h;
        }
        let n = matches.len();
        drop(matches);
        *self.ivars().rows.borrow_mut() = rows;

        // Size the panel to exactly fit the header + rows (no dead space at the bottom).
        let content_h = if n == 0 {
            TOP + FIELD_H + BOTTOM
        } else {
            TOP + FIELD_H + SEP_GAP + rows_h + ROW_SPACING * (n as f64 - 1.0) + BOTTOM
        };
        self.relayout(content_h);
        self.restyle_selection();
    }

    /// Resize the panel to `height` and re-place the sub-views. The panel's TOP edge is locked
    /// relative to the parent window: while the query is empty (the full, unfiltered list — the
    /// initial look) it stays centered and we record that top edge; once a query narrows the
    /// results the top stays put, so the panel grows/shrinks downward instead of jumping around
    /// the center as the match count changes.
    fn relayout(&self, height: f64) {
        let panel = &self.ivars().panel;
        let top = if self.ivars().field.stringValue().to_string().is_empty() {
            let centered_top = self.ivars().center_y.get() + height / 2.0;
            self.ivars().anchor_top.set(centered_top);
            centered_top
        } else {
            self.ivars().anchor_top.get()
        };
        let origin = NSPoint::new(self.ivars().center_x.get() - PANEL_W / 2.0, top - height);
        panel.setFrame_display(NSRect::new(origin, NSSize::new(PANEL_W, height)), true);

        let full = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(PANEL_W, height));
        self.ivars().effect.setFrame(full);
        self.ivars().tint.setFrame(full);
        self.ivars().field.setFrame(NSRect::new(
            NSPoint::new(FIELD_X, height - TOP - FIELD_H),
            NSSize::new(PANEL_W - 2.0 * FIELD_X - 24.0, FIELD_H),
        ));
        self.ivars().spinner.setFrame(NSRect::new(
            NSPoint::new(PANEL_W - FIELD_X - 18.0, height - TOP - FIELD_H + 4.0),
            NSSize::new(18.0, 18.0),
        ));
        let list_top = height - TOP - FIELD_H - SEP_GAP;
        self.ivars().results.setFrame(NSRect::new(
            NSPoint::new(LIST_X, BOTTOM),
            NSSize::new(PANEL_W - 2.0 * LIST_X, (list_top - BOTTOM).max(0.0)),
        ));
    }

    fn restyle_selection(&self) {
        let sel = self.ivars().selected.get();
        let matches = self.ivars().matches.borrow();
        for (i, row) in self.ivars().rows.borrow().iter().enumerate() {
            let selected = i == sel;
            // Native list selection: solid emphasized accent + white text on the selected row.
            if let Some(cmd) = matches.get(i) {
                row.setAttributedTitle(&row_title(cmd, selected));
            }
            if let Some(layer) = row.layer() {
                let bg = if selected {
                    NSColor::selectedContentBackgroundColor().CGColor()
                } else {
                    NSColor::clearColor().CGColor()
                };
                layer.setBackgroundColor(Some(&bg));
            }
        }
    }

    fn move_selection(&self, delta: i64) {
        let n = self.ivars().rows.borrow().len();
        if n == 0 {
            return;
        }
        let cur = self.ivars().selected.get() as i64;
        let next = (cur + delta).rem_euclid(n as i64) as usize;
        self.ivars().selected.set(next);
        self.restyle_selection();
    }

    fn fire_selected(&self) {
        let path = {
            let m = self.ivars().matches.borrow();
            m.get(self.ivars().selected.get()).map(|c| c.path.clone())
        };
        let pid = self.ivars().pid;
        self.close();
        if let Some(path) = path {
            fire(pid, &path);
        }
    }

    fn close(&self) {
        if let Some(t) = self.ivars().timer.borrow_mut().take() {
            t.invalidate();
        }
        self.ivars().panel.orderOut(None);
        // We stole activation to take key focus; hand it back to the app that was frontmost when the
        // palette opened, so keyboard focus returns to the window the user was in.
        if let Some(app) =
            NSRunningApplication::runningApplicationWithProcessIdentifier(self.ivars().pid)
        {
            #[allow(deprecated)]
            app.activateWithOptions(NSApplicationActivationOptions::empty());
        }
        PANEL.with(|p| *p.borrow_mut() = None);
        CONTROLLER.with(|c| *c.borrow_mut() = None);
    }

    fn mtm(&self) -> MainThreadMarker {
        MainThreadMarker::from(self)
    }
}

/// An attributed run in `font`/`color`.
fn attr_run(text: &str, font: &NSFont, color: &NSColor) -> Retained<NSAttributedString> {
    let attrs = NSDictionary::from_slices(
        &[unsafe { NSForegroundColorAttributeName }, unsafe {
            NSFontAttributeName
        }],
        &[color as &AnyObject, font as &AnyObject],
    );
    unsafe {
        NSAttributedString::initWithString_attributes(
            NSAttributedString::alloc(),
            &NSString::from_str(text),
            Some(&attrs),
        )
    }
}

/// The attributed title for a row: `path ▸ leaf   shortcut` (+ a dimmed AXHelp second line). When
/// `selected`, text is the selection text color (white) to read on the solid selection background.
fn row_title(cmd: &Command, selected: bool) -> Retained<NSMutableAttributedString> {
    let mut line1 = cmd.path.join(" ▸ ");
    if let Some(sc) = &cmd.shortcut {
        line1.push_str(&format!("    {sc}"));
    }
    // Solid selection bg is the emphasized accent, so the selected row's text is white (matching
    // native list selection). alternateSelectedControlTextColor doesn't resolve to white as an
    // attributed-title color here, so use white directly.
    let primary = if selected {
        NSColor::whiteColor()
    } else if cmd.enabled {
        NSColor::labelColor()
    } else {
        NSColor::secondaryLabelColor()
    };
    let secondary = if selected {
        NSColor::whiteColor().colorWithAlphaComponent(0.75)
    } else {
        NSColor::secondaryLabelColor()
    };
    // In-menu font (what dropdown items use), NOT the bolder menu-bar title font.
    let title = NSMutableAttributedString::new();
    title.appendAttributedString(&attr_run(&line1, &NSFont::menuFontOfSize(0.0), &primary));
    if let Some(help) = &cmd.help {
        title.appendAttributedString(&attr_run(
            &format!("\n{help}"),
            &NSFont::menuFontOfSize(11.0),
            &secondary,
        ));
    }
    // Left padding inside the highlight so the text isn't flush against its rounded edge.
    let para = NSMutableParagraphStyle::new();
    para.setFirstLineHeadIndent(TEXT_INSET);
    para.setHeadIndent(TEXT_INSET);
    let range = NSRange::new(0, title.length());
    unsafe {
        title.addAttribute_value_range(NSParagraphStyleAttributeName, &para, range);
    }
    title
}

/// Build one result row (a borderless, left-aligned button). `restyle_selection` sets its title
/// color + background per selection; here we set the unselected title.
fn make_row(
    mtm: MainThreadMarker,
    cmd: &Command,
    target: &AnyObject,
    tag: isize,
) -> Retained<NSButton> {
    let btn = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str(""),
            Some(target),
            Some(sel!(rowClicked:)),
            mtm,
        )
    };
    btn.setBordered(false);
    btn.setAttributedTitle(&row_title(cmd, false));
    btn.setAlignment(NSTextAlignment::Left);
    btn.setTag(tag);
    btn.setWantsLayer(true);
    if let Some(layer) = btn.layer() {
        layer.setCornerRadius(6.0);
    }
    if cmd.help.is_some() {
        if let Some(cell) = btn.cell() {
            cell.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
        }
    }
    btn
}

/// Open the command palette for app `pid` (captured at hotkey time). Main thread only.
pub fn open(mtm: MainThreadMarker, pid: i32) {
    // One at a time; re-pressing the hotkey while open just re-focuses.
    if PANEL.with(|p| p.borrow().is_some()) {
        return;
    }
    tracing::debug!("palette open pid={pid}");

    // Placeholder size; relayout() sizes the panel to its content on the first refilter.
    let init = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(PANEL_W, 120.0));
    let panel: Retained<NSPanel> = {
        let p: Retained<PalettePanel> = unsafe {
            msg_send![
                PalettePanel::alloc(mtm),
                initWithContentRect: init,
                styleMask: NSWindowStyleMask::Borderless,
                backing: NSBackingStoreType::Buffered,
                defer: false,
            ]
        };
        Retained::into_super(p)
    };
    panel.setLevel(101); // ~ pop-up-menu level: above the floating bar
    unsafe { panel.setReleasedWhenClosed(false) };
    panel.setOpaque(false);
    panel.setBackgroundColor(Some(&NSColor::clearColor()));

    // Acrylic rounded background.
    let effect = NSVisualEffectView::new(mtm);
    effect.setMaterial(NSVisualEffectMaterial::Popover);
    effect.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
    effect.setState(NSVisualEffectState::Active);
    effect.setWantsLayer(true);
    if let Some(layer) = effect.layer() {
        layer.setCornerRadius(12.0);
        layer.setMasksToBounds(true);
    }

    // Tint the blur toward the window background color so it reads more opaque — light grey in
    // light mode, dark grey in dark mode (windowBackgroundColor is appearance-adaptive; resolved
    // at open time, which is fine for a transient panel). Sits behind the field/results.
    let tint = NSView::initWithFrame(NSView::alloc(mtm), init);
    tint.setWantsLayer(true);
    if let Some(layer) = tint.layer() {
        let bg = NSColor::windowBackgroundColor().colorWithAlphaComponent(0.9).CGColor();
        layer.setBackgroundColor(Some(&bg));
    }
    effect.addSubview(&tint);

    // Search field.
    let field = NSTextField::new(mtm);
    field.setEditable(true);
    field.setBezeled(false);
    field.setDrawsBackground(false);
    field.setFocusRingType(NSFocusRingType::None); // no boxy focus rectangle
    field.setFont(Some(&NSFont::menuFontOfSize(18.0)));
    field.setPlaceholderString(Some(&NSString::from_str("Search menus…")));

    // Results list (vertical stack, top-aligned).
    let results = NSStackView::new(mtm);
    results.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
    results.setAlignment(NSLayoutAttribute::Leading);
    results.setSpacing(ROW_SPACING);

    // Spinner.
    let spinner = NSProgressIndicator::new(mtm);
    spinner.setStyle(objc2_app_kit::NSProgressIndicatorStyle::Spinning);
    spinner.setDisplayedWhenStopped(false);
    spinner.setControlSize(objc2_app_kit::NSControlSize::Small);

    effect.addSubview(&field);
    effect.addSubview(&spinner);
    effect.addSubview(&results);
    panel.setContentView(Some(&effect));

    // Shared index + background walk.
    let index = Arc::new(Mutex::new(IndexState::default()));
    {
        let idx = index.clone();
        std::thread::spawn(move || build_index(pid, idx));
    }

    // Center the palette on the focused window.
    let (cx, cy) = target_center(mtm, pid);

    let controller = PaletteController::new(
        mtm,
        pid,
        index,
        panel.clone(),
        effect.clone(),
        tint.clone(),
        field.clone(),
        results.clone(),
        spinner.clone(),
        cx,
        cy,
    );
    // Wire delegates (field editing + window key changes) via raw msg_send (avoids protocol casts).
    let ctrl_obj: &AnyObject = &controller;
    unsafe {
        let _: () = msg_send![&field, setDelegate: ctrl_obj];
        let _: () = msg_send![&panel, setDelegate: ctrl_obj];
    }

    // Streaming poll timer.
    let timer = unsafe {
        NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
            0.04,
            &controller,
            sel!(poll:),
            None,
            true,
        )
    };
    *controller.ivars().timer.borrow_mut() = Some(timer);

    controller.refilter(); // sizes + positions the panel before it's shown
    #[allow(deprecated)]
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
    panel.makeKeyAndOrderFront(None);
    panel.makeFirstResponder(Some(&field));

    PANEL.with(|p| *p.borrow_mut() = Some(panel));
    CONTROLLER.with(|c| *c.borrow_mut() = Some(controller));
}

/// Cocoa (center-x, center-y) of `pid`'s focused window; falls back to the main screen center.
fn target_center(mtm: MainThreadMarker, pid: i32) -> (f64, f64) {
    let primary_h = NSScreen::screens(mtm)
        .iter()
        .next()
        .map(|s| s.frame().size.height)
        .unwrap_or(1080.0);
    let app = ax::app_element(pid);
    ax::set_timeout(&app, 0.5);
    if let Some(win) = ax::focused_window(&app) {
        if let (Some(p), Some(s)) = (
            ax::attr_point(&win, names::AX_POSITION),
            ax::attr_size(&win, names::AX_SIZE),
        ) {
            // AX is top-left/y-down; flip the window's vertical center to Cocoa y-up.
            return (p.x + s.width / 2.0, primary_h - (p.y + s.height / 2.0));
        }
    }
    if let Some(screen) = NSScreen::mainScreen(mtm) {
        let f = screen.frame();
        return (f.origin.x + f.size.width / 2.0, f.origin.y + f.size.height / 2.0);
    }
    (700.0, primary_h / 2.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(path: &[&str]) -> Command {
        Command {
            path: path.iter().map(|s| s.to_string()).collect(),
            enabled: true,
            shortcut: None,
            help: None,
        }
    }

    #[test]
    fn subsequence_matches_and_ranks() {
        // "nw" matches "New" (n..ew) and "New Window".
        assert!(fuzzy_score("nw", "New Window").is_some());
        assert!(fuzzy_score("new", "New").is_some());
        assert!(fuzzy_score("xyz", "New").is_none());
    }

    #[test]
    fn word_start_beats_midword() {
        // "b" at the start of "Bold" should outscore "b" inside "Table".
        let a = fuzzy_score("b", "Bold").unwrap();
        let b = fuzzy_score("b", "Table").unwrap();
        assert!(a > b, "word-start {a} should beat mid-word {b}");
    }

    #[test]
    fn ranked_prefers_leaf_and_caps() {
        let cmds = vec![
            cmd(&["Format", "Font", "Bold"]),
            cmd(&["Edit", "Boldface toggle wrapper thing"]),
            cmd(&["View", "Sidebar"]),
        ];
        let r = ranked("bold", &cmds);
        assert_eq!(r[0].path.last().unwrap(), "Bold"); // leaf match wins
        assert!(ranked("", &cmds).len() <= MAX_RESULTS);
    }
}
