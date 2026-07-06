//! Lintel — Phase 0 walking skeleton (see docs/plans/2026-07-06-lintel-design-v2.md §12).
//!
//! Proves the riskiest FFI end-to-end before any UI:
//!   * Accessibility trust check + prompt
//!   * read the frontmost app's menu bar (top-level + first-level titles) over AX
//!   * resolve a menu path and fire the real action with AXPress
//!   * wire an AXObserver (C-unwind trampoline + Box refcon + correct teardown) on the main run loop
//!
//! Usage:
//!   lintel [read]                     print the frontmost app's menus (default)
//!   lintel press "<TopMenu>" "<Item>" fire a first-level menu item in the frontmost app
//!   lintel watch                      run as an accessory app and log focus/geometry events

mod ax;
mod observer;

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSWorkspace};
use objc2_application_services::AXError;

use ax::{
    app_element, attr_element, attr_string, children, is_trusted, names, press, prompt_trust,
    set_timeout,
};
use observer::{AxObserver, ObserverCtx};

/// (pid, localized name) of the frontmost application, if any.
fn frontmost() -> Option<(i32, String)> {
    let ws = NSWorkspace::sharedWorkspace();
    let app = ws.frontmostApplication()?;
    let name = app.localizedName().map(|s| s.to_string()).unwrap_or_default();
    Some((app.processIdentifier(), name))
}

/// Ensure Accessibility is granted; prompt and return false if not.
fn ensure_trust() -> bool {
    if is_trusted() {
        return true;
    }
    eprintln!("Lintel needs Accessibility permission.");
    let _ = prompt_trust();
    eprintln!("Grant it in System Settings > Privacy & Security > Accessibility, then re-run.");
    false
}

fn cmd_read() {
    if !ensure_trust() {
        std::process::exit(1);
    }
    let Some((pid, name)) = frontmost() else {
        eprintln!("No frontmost application.");
        return;
    };
    println!("Frontmost: {name} (pid {pid})\n");

    let app = app_element(pid);
    set_timeout(&app, 2.0);

    let Some(menubar) = attr_element(&app, names::AX_MENU_BAR) else {
        eprintln!("No AXMenuBar exposed by {name}.");
        return;
    };

    for item in children(&menubar) {
        let Some(title) = attr_string(&item, names::AX_TITLE) else {
            continue;
        };
        if title.is_empty() {
            continue;
        }
        // First-level items live under the menu-bar item's single AXMenu child.
        let subs: Vec<String> = children(&item)
            .into_iter()
            .next()
            .map(|menu| {
                children(&menu)
                    .into_iter()
                    .filter_map(|mi| attr_string(&mi, names::AX_TITLE))
                    .filter(|t| !t.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        println!("* {title}  ({} items)", subs.len());
        for s in subs.iter().take(6) {
            println!("    - {s}");
        }
        if subs.len() > 6 {
            println!("    ...");
        }
    }
}

fn cmd_press(top: &str, item: &str) {
    if !ensure_trust() {
        std::process::exit(1);
    }
    let Some((pid, name)) = frontmost() else {
        eprintln!("No frontmost application.");
        return;
    };
    println!("Pressing {top} > {item} in {name} (pid {pid})");

    let app = app_element(pid);
    set_timeout(&app, 2.0);
    let Some(menubar) = attr_element(&app, names::AX_MENU_BAR) else {
        eprintln!("No AXMenuBar.");
        return;
    };

    for it in children(&menubar) {
        if attr_string(&it, names::AX_TITLE).as_deref() != Some(top) {
            continue;
        }
        let Some(menu) = children(&it).into_iter().next() else {
            continue;
        };
        for mi in children(&menu) {
            if attr_string(&mi, names::AX_TITLE).as_deref() == Some(item) {
                match press(&mi) {
                    AXError::Success => println!("OK (AXPress fired)"),
                    err => eprintln!("AXPress -> {err:?}"),
                }
                return;
            }
        }
    }
    eprintln!("Item '{top} > {item}' not found.");
}

fn cmd_watch() {
    if !ensure_trust() {
        std::process::exit(1);
    }
    let mtm = MainThreadMarker::new().expect("must run on the main thread");
    let Some((pid, name)) = frontmost() else {
        eprintln!("No frontmost application.");
        return;
    };
    println!("Watching {name} (pid {pid}); focus/move/resize events below. Ctrl-C to quit.");

    let app = app_element(pid);
    set_timeout(&app, 2.0);
    // Kept alive for the lifetime of the run loop; Drop performs the §5.2 teardown.
    let _obs = AxObserver::new(pid, app, ObserverCtx { label: name });

    let ns = NSApplication::sharedApplication(mtm);
    ns.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    ns.run();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("watch") => cmd_watch(),
        Some("press") => match (args.get(2), args.get(3)) {
            (Some(top), Some(item)) => cmd_press(top, item),
            _ => eprintln!("usage: lintel press \"<TopMenu>\" \"<Item>\""),
        },
        Some("read") | None => cmd_read(),
        Some(other) => eprintln!("unknown command '{other}' (try: read | press | watch)"),
    }
}
