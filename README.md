# Lintel

> A *lintel* is the horizontal beam that sits directly above a window opening. This app is exactly that — a bar resting above your window.

Lintel is a background macOS utility that gives every window a **local menu bar**. When a
window gains focus, Lintel reads that app's menu bar over the Accessibility API and draws a
copy of it as a **floating, acrylic bar pinned just above the focused window** — so on a
super-ultrawide you don't have to travel to the top-left of the screen to reach a menu.

- **Fully interactive** — clicking a mirrored item fires the *real* action in the target app
  (via `AXUIElementPerformAction`), including submenus.
- **Non-activating** — the bar never steals focus, so the app it mirrors stays frontmost.
- **Draws over the system menu bar** when a window sits at the very top.
- **Hides** in native fullscreen.
- **Acrylic / Liquid-Glass** styling.

## Status

**Phase 1 MVP is implemented** — `lintel run` shows a floating acrylic bar pinned above the
focused window that mirrors its top-level menus, tracks the window as it moves (incl. across an
ultrawide), hides when the window is fullscreen/maximized, and — clicking a menu drops down its
first-level items and fires the real action via `AXPress`, without stealing focus. Verified
visually against Chrome (bar renders + tracks; follows a window to x=5760 on a 7680px display).
The click→drop-down→fire path is wired on the proven Phase 0 `AXPress` engine.

**Phase 0 (walking skeleton)** — reads a foreign app's menu bar over Accessibility, fires a real
action with `AXPress`, and wires an `AXObserver` on the main run loop. Verified against Chrome
(menus read *cold*, incl. dynamic History/Bookmarks) and TextEdit (`File ▸ New` fired). See §12.

The feasibility design is complete and has passed an adversarial review (7 must-fixes applied in v2):

- **Design (canonical):** [`docs/plans/2026-07-06-lintel-design-v2.md`](docs/plans/2026-07-06-lintel-design-v2.md)
  — start with its "Changes from v1" section. ([v1](docs/plans/2026-07-06-lintel-design.md) kept for history.)
- **Research appendix** (per-dimension findings, adversarial verdicts, sources):
  [`docs/research/2026-07-06-feasibility-findings.md`](docs/research/2026-07-06-feasibility-findings.md)

**Verdict:** feasible for the fully-interactive version. The read+trigger core is proven by
shipping prior art (Many Tricks *Menuwhere*; the MIT `right-click-menubar`). Every required
API has a public `objc2` binding — no private API except an optional fullscreen probe (which
has a public fallback). Two areas need early prototype spikes: behind-window blur sampling a
*foreign* app's window, and layering over the macOS 26 "Liquid Glass" menu bar.

## Building & running

```sh
cargo build
cargo run -- run                      # THE MVP: floating menu bar above the focused window
cargo run -- read                     # (debug) print the frontmost app's menu bar
cargo run -- press "File" "New"       # (debug) fire a first-level item in the frontmost app
cargo run -- watch                    # (debug) log focus/move/resize AX events

LINTEL_DEBUG=1 cargo run -- run       # run with placement debug logging on stderr
scripts/bundle.sh                     # build a signed Lintel.app (see the script for TCC/signing notes)
```

With `run`, focus a normal (non-maximized) window and its menus appear in a bar just above it;
click a menu to drop down its items and click one to trigger it. Ctrl-C to quit.

First use needs Accessibility permission: **System Settings ▸ Privacy & Security ▸ Accessibility**
(the binary prompts if untrusted). For the grant to survive rebuilds, sign with a stable identity —
see `scripts/bundle.sh`.

## Target

macOS 26.5+ · Rust (edition 2024) · the `objc2` crate family (AppKit + Accessibility) ·
ships as a non-sandboxed, code-signed, `LSUIElement` accessory `.app`.
