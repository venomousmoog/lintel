# Lintel — Design Document (v2)

*Target: macOS 26.5 (build 25F80, "Liquid Glass"), Command Line Tools SDK 26.5, Rust (rustc 1.96.1, edition 2024), objc2 crate family. Verified against the local CLT SDK on 2026-07-06.*

> **v2** supersedes [v1](2026-07-06-lintel-design.md). It applies the seven must-fixes and the strongest suggestions from the adversarial design review (see [research appendix](../research/2026-07-06-feasibility-findings.md) for the underlying feasibility evidence). **Read the "Changes from v1" section first.**

---

## 0. Changes from v1

| # | Fix | Where |
|---|---|---|
| 1 | **Dynamic-menu leaf handles die on menu close.** For `menuWillOpen:`-built and non-native (Electron/Qt/Java/JUCE) menus, submenu-child `AXUIElement`s exist only during an open tracking session; caching and later pressing one returns `kAXErrorInvalidUIElement`. v2 never caches dynamic leaf handles for triggering — it re-resolves by *path* against the live tree and presses while the session is open. | §4 |
| 2 | **Cache self-invalidation loop.** Lintel's own `AXShowMenu`/`AXPress` fire `AXMenuOpened`/`AXMenuClosed`, which §4.6(v1) treated as invalidation → discarded the model just built. v2 gates the observer around self-induced AX actions and narrows invalidation to a *targeted subtree re-read*. | §4.6, §5.2 |
| 3 | **Threading contradiction.** `AXUIElement`/`AXObserver` are `!Send + !Sync`; the "run `AXPress` off the UI thread" clause both contradicted §3.2 and would not compile. v2 is **main-thread-only**, bounded by a messaging timeout; `dispatch2` dropped. | §3.2, §4.5 |
| 4 | **Level-101 persistent bar overlaps system status region / ties with system dropdowns.** v2 puts the *persistent* bar at the lowest level that clears the static menu bar and **clamps it away from the right-side status/notch region**; only Lintel's own *transient* dropdown sits at level 101. | §6.3, §6.5, §8 |
| 5 | **AXObserver refcon lifetime/teardown undefined + per-activation leak.** v2 commits to `Box<ObserverCtx>` refcon + `CFRetained<AXObserver>`, an exact teardown order, and an eviction point. | §5.2, §10 |
| 6 | **Wrong trampoline ABI.** The callback type is `unsafe extern "C-unwind" fn`, not `extern "C"`. | §10 |
| 7 | **MenuModel/panel ownership + rebuild-while-open transition undefined.** v2 makes the Coordinator the sole owner of a *versioned snapshot*; the panel is a pure render; a rebuild transition is defined; the leaf is re-resolved and `AXEnabled` re-read immediately before every press. | §3, §4, §7-state |
| + | **Submenu UI simplified** from a tree of nested nonactivating panels to a **single reusable dropdown surface** with one tracking state machine (safe-triangle, open/close delays, dismiss). | §6.6 |
| + | Enumerated Coordinator state machine; modal-sheet handling; permission-revocation handling; testable seams; shortcut-decode scope cut for v1. | §3.3, §5 |

---

## 1. Overview & Goals

Lintel is a background macOS utility that renders a **local menu bar**: when a window gains focus, Lintel reads the focused application's menu bar over the Accessibility (AX) API and draws a **copy of it as a floating, acrylic/frosted bar pinned just above the focused window**. On a super-ultrawide display the real menu bar (top-left of the primary screen) can be far from the working window; Lintel brings the menu to the window.

**Hard requirements:**

1. **Fully interactive** — clicking a mirrored item triggers the *real* menu action in the target app, including submenus. (For the dynamic-menu class this is achieved by re-opening + re-resolving + pressing while the tracking session is live; see §4.)
2. **Non-activating** — interacting with the bar must NOT steal focus from the target app. (If focus moved to Lintel, the target's frontmost status would change and the very menu we mirror could change.)
3. **Hides in native fullscreen _and when the window is maximized_** (zoomed to fill the display) — see §5.4.
4. **Over-the-menu-bar collision handling** — when a *non-maximized* focused window sits at the very top, draw the acrylic bar **over** the static system menu bar, but never over the right-side status/notch region (§6.5). Shifting the system menu bar aside is **infeasible** and dropped (§7.4).
5. **Acrylic / frosted-glass styling.**

**Prior art that de-risks the core:** Menuwhere (Many Tricks) reads the frontmost app's full menu tree over AX and executes items incl. submenus and background apps; the MIT `right-click-menubar` reproduces the exact `AXUIElementCreateApplication → kAXMenuBarAttribute → recursive kAXChildren → AXUIElementPerformAction(kAXPressAction)` path. **What is novel** (no single prior art): a *persistent, non-activating, interactive, acrylic* bar that mirrors live and can be drawn over the system menu bar. The review confirmed the read+leaf-press core as sound for static/native apps; the novel risk is concentrated in the dynamic-menu class and the styling/layering spikes.

---

## 2. Non-Goals / YAGNI

- **Do NOT shift/reposition the real system menu bar.** No public or known-private API; cover the region instead (§6.5).
- **Do NOT support the Mac App Store / App Sandbox.** No sandbox entitlement grants the general accessibility-client role; ship non-sandboxed, direct download.
- **CGEvent key-equivalent synthesis is not a general trigger path.** It is unreliable for non-focused apps and only helps items that *have* a shortcut. v2 keeps it as a **narrow, spike-gated optimization for dynamic items that carry a key-equivalent** (to avoid the visible flash) and otherwise logs `AXPress` failures. Cut entirely from v1 if S3 shows it isn't needed.
- **Do NOT eagerly pre-walk entire deep menu trees.** Read top-level + first-level statically; populate dynamic submenus on demand (§4).
- **No notarization for local/personal use.**
- **No multi-app aggregated menu** (Menuwhere's "all apps" mode).
- **No keyboard-driven navigation** of the mirrored bar in v1 (mouse only).
- **v1 shortcut rendering is `AXMenuItemCmdChar`-only.** Glyph/virtual-key mapping (arrows, return, delete, F-keys) is deferred — it is display polish sitting in the hot read path.

---

## 3. System Architecture

### 3.1 Components

```
                         ┌─────────────────────────────────────────┐
                         │  main thread (MainThreadMarker) ONLY     │
                         │  NSApplication run loop (.Accessory)     │
                         └─────────────────────────────────────────┘
   ┌──────────────┐   focus/geom events  ┌──────────────────────────┐
   │ FocusTracker │─────────────────────▶│  Coordinator             │
   │ NSWorkspace  │                       │  • SOLE owner of the     │
   │ + AXObservers│◀── read requests ─────│    current MenuModel     │
   └──────────────┘                       │    (versioned snapshot)  │
          │                               │  • state machine (§3.3)  │
          │ pid / window elem             │  • AX-event suppression  │
          ▼                               │    gate (§5.2)           │
   ┌──────────────┐   MenuModel snapshot  └──────────────────────────┘
   │ MenuReader   │────────────┐               │ snapshot (v=N)
   │ (all AX FFI) │            │               ▼
   │ read/resolve │◀── press ──┼──────  ┌──────────────┐   ┌──────────────┐
   │ /press/cancel│  (leaf)    └───────▶│ Geometry     │   │ OverlayPanel │
   └──────────────┘                     │ (AX→Cocoa)   │──▶│ bar + ONE    │
          ▲                             │ placement    │   │ dropdown     │
          └───── click: item → press ───┴─────────────────│ (pure render)│
                                                          └──────────────┘
```

- **Coordinator** — the only owner of application state and of the current `MenuModel`. Pure Rust logic (behind injected provider traits, §3.4). Decides show/hide/move/rebuild; runs the AX-event suppression gate; hands the OverlayPanel an **immutable, versioned snapshot**.
- **FocusTracker** — `NSWorkspace` app-activation + per-app `AXObserver` for focused-window/geometry/lifecycle notifications; a low-rate reconciliation timer (§5.3).
- **MenuReader** — **all** raw AX FFI: read the tree, *resolve a leaf by path*, `AXPress` a leaf, `AXShowMenu`/`AXCancel` a menu, wire `AXObserver`s. Exposes a safe wrapper; no objc2 UI types cross into AX calls.
- **Geometry** — AX→Cocoa conversion, owning-screen selection, panel-frame computation incl. collision/maximized/status-region clamps.
- **OverlayPanel** — the persistent bar `NSPanel` + **one** reusable dropdown `NSPanel` (§6.6). A **pure render of the Coordinator's snapshot**; it never mutates the model and clicks route straight to `MenuReader.press` (Geometry is not in the click path).

### 3.2 Threading — main-thread only

**Everything runs on the main thread.** `AXUIElement`/`AXObserver`/`AXValue` are `!Send + !Sync` in `objc2-application-services` 0.3.2, so `AxElem` (`CFRetained<AXUIElement>`), the `MenuModel`, and the per-pid cache **cannot** move or be shared to a worker without `unsafe`. AXObserver run-loop sources are added to the **main** `CFRunLoop`, so callbacks arrive on the main thread — no marshaling, no data races. Stalls from a busy target are bounded by a short **`AXUIElementSetMessagingTimeout`** (1–2 s), not by threading. `dispatch2` is not a dependency. **Tradeoff:** every dynamic populate (§4.4 poll) and dynamic trigger (§4.5) therefore blocks the main run loop up to the timeout worst case; this is bounded and accepted for v1 (the dynamic class is a minority), and is why the timeout is kept short.

*(If off-main AX ever proves necessary — not planned — the only sound pattern is: a newtype with a documented `unsafe impl Send`, a **fresh** `AXUIElementCreateApplication(pid)` created on the worker (only the `pid` crosses), and all calls for one element serialized on one dedicated thread. Treat as unverified on 26.5.)*

### 3.3 Coordinator state machine

Explicit states (the v1 doc named a "state machine" but never enumerated it):

```
Hidden           — no eligible window (untrusted, fullscreen, maximized, no menu bar, minimized)
Shown            — bar visible, pinned above the focused window; no dropdown open
DropdownOpen     — Shown + the single dropdown surface is tracking a submenu path
Moving           — window in rapid motion; bar detached/hidden until settle (§5.3)
Rebuilding       — model changed; a new snapshot is being read (transient — exits to Shown, or back to DropdownOpen, once the snapshot at the new version is published)
```

Transitions are driven by: focus change, `AXWindowMoved/Resized/Miniaturized/Destroyed`, reconciliation tick, model-change notifications, permission change, and user hover/click. **Rebuild-while-open rule:** if a model change arrives while `DropdownOpen`, either (a) rebuild the snapshot and re-render the open dropdown from it, or (b) defer invalidation until the dropdown closes. v1 default: **defer while a dropdown is open**, apply on close (simpler, avoids pointer churn under the cursor); the persistent bar itself always re-renders from the newest snapshot.

### 3.4 Testable seams

The Coordinator's "pure Rust, testable" claim is backed by injected traits so logic runs without AppKit/AX:
`ScreenProvider` (screens, frames, visibleFrame, scale), `FocusProvider` (focused app/window, frame, fullscreen/maximized flags), `MenuProvider` (read tree, resolve leaf, press, cancel). Real impls wrap AppKit/MenuReader; test impls are in-memory. Placement math (§8), the maximized/collision decisions (§5.4/§6.5), and the state machine (§3.3) are unit-tested against fakes.

---

## 4. Menu Model

### 4.1 Data model

```rust
struct MenuModel { version: u64, top: Vec<MenuBarItem> }   // owned solely by Coordinator
struct MenuBarItem { title: String, path: MenuPath, kind: Populated, items: Vec<MenuEntry> }
enum   Populated { Static, Dynamic }                        // Dynamic = AXMenu reports 0 children but parent is submenu-capable
enum   MenuEntry {
    Item { title, enabled: bool, mark: Option<char>, shortcut: Option<Shortcut>,
           path: MenuPath, kind: Populated, children: Vec<MenuEntry> },
    Separator,
}
struct MenuPath(Vec<PathStep>);                             // e.g. ["File", "Open Recent", "Document.txt"]
enum   PathStep { Title(String), Index(usize) }             // Title preferred; Index for blank/duplicate titles
struct Shortcut { key_char: char, mods: Modifiers }        // v1: CmdChar only
```

- **`MenuPath` is the durable identity of an item**, not a live `AXUIElement`. Handles are re-resolved from the path at trigger time (§4.5). This is the core v2 change that survives dynamic teardown.
- `AxElem` (`CFRetained<AXUIElement>`) is cached **only for static/native menu items** whose elements persist, as a fast path; dynamic items carry no cached handle.
- The `MenuModel` is immutable once published; a change produces a new `version`.

### 4.2 Reading (traversal)

Path (constants verified in the local CLT `HIServices` headers):

```
AXUIElementCreateApplication(pid)
  → "AXMenuBar" → "AXChildren" → [AXMenuBarItem]           (role kAXMenuBarItemRole)
     → each "AXMenu" child → "AXChildren" → [AXMenuItem]   (role kAXMenuItemRole)
        → an item with its own "AXMenu" child = submenu
```

Batch-read per item with **`AXUIElementCopyMultipleAttributeValues`** (one IPC round-trip): `AXTitle`, `AXEnabled`, `AXRole`, `AXMenuItemMarkChar`, `AXMenuItemCmdChar`, `AXMenuItemCmdModifiers`, `AXChildren`. **Wrapper contract (review):** this call returns a **+1 (Create Rule)** `CFArray`; a per-attribute failure comes back as an `AXValue` of type `kAXValueAXErrorType`, *not* the requested type — the boundary must detect and map that per element.

- **Separators:** empty `AXTitle`, no action, disabled (heuristic; validate in Accessibility Inspector).
- **Shortcut decode (v1):** `AXMenuItemCmdChar` only. Modifier bitmask is **inverted vs NSEvent:** `Shift=1, Option=2, Control=4, NoCommand=8`; **Command is implied** (0 ⇒ Cmd-only); bit 3 set ⇒ NO Command.
- **`AXUIElementSetMessagingTimeout`** (≈1–2 s) so a hung target can't block the walk.

### 4.3 Eager (static) vs. dynamic classification

- **Read eagerly (Static):** all top-level `AXMenuBarItem`s and their first-level `AXMenu` children. **Window, Font, and Open Recent ARE readable cold** (AppKit-managed persistent `NSMenuItem`s). A passive `AXChildren` read also drives `-[NSMenu update]`/`menuNeedsUpdate:` for most native menus **without any visible display**, so most deep static submenus enumerate cold. *(Whether the invisible-update behavior holds on 26.5 is spike S3.)*
- **Classify DYNAMIC only when** an `AXMenuItem` is submenu-capable but its `AXMenu` reports **zero** children. True dynamic cases: (1) menus built in `NSMenuDelegate menuWillOpen:`, (2) **non-native toolkits** (Electron/Chromium, Qt, Java/JUCE, custom popups) with no backing `NSMenu`. Their children do not exist in AX until a tracking session begins **and are destroyed when it ends**.
- **Feature-detect element survival (S3):** on first dynamic open, test whether the child elements remain valid (`AXRole` read succeeds) *after* close. Apps where they survive get the fast press-in-place path; apps where they don't use re-resolve-on-click (§4.5). Cache the per-app verdict.

### 4.4 Populating a dynamic submenu (for display)

On hover of a mirrored dynamic parent, to show its contents:

1. **Suppress the AX-menu-event gate** for this target (§5.2), then `AXUIElementPerformAction(parent, "AXShowMenu")`. This visibly opens the real submenu — the unavoidable flash for this class.
2. Poll the parent's `AXMenu` `AXChildren` until non-empty or timeout (bounded by the messaging timeout).
3. Read the now-populated children into a snapshot subtree (paths, titles, enabled, shortcuts). Do **not** cache child handles for later triggering (they die on close).
4. **Deterministically dismiss** via `AXUIElementPerformAction(openMenu, "AXCancel")` (`kAXCancelAction`) on the opened `AXMenu` element — **not** a synthesized Escape (unreliable). If `AXCancel` is unavailable/fails, fall back to pressing `Escape` via CGEvent and, if that also fails, leave the dropdown driving off the still-open real menu and record `dismiss_failed` (S3 measures this).
5. **Restore the gate.** Because the target app stays frontmost throughout, opening/closing the real submenu never changes *which* app we mirror.

> **Flash cost is per-open for the dynamic class**, not a one-time populate — quantify per app in S3. For fully-dynamic *deep* trees where repeated flashing is objectionable, degrade to a **"click to open the real menu"** affordance (single AXShowMenu, no mirroring of that subtree) or key-equivalent dispatch where a shortcut exists.

### 4.5 Triggering (click → real action)

Resolve-then-press, always against the **live** tree:

1. From the clicked mirror item, take its `MenuPath`.
2. **Re-resolve** the leaf `AXUIElement` by walking the live tree from the stable `app → AXMenuBar` root along the path (prefer title match; fall back to index). Validate the resolved element with an `AXRole` read; on `kAXErrorInvalidUIElement` re-resolve from the root once.
3. For a **static/native** leaf, the cached `AxElem` may be used directly (still validate with a cheap `AXRole`/`AXEnabled` read first).
4. For a **dynamic** leaf, ensure the tracking session is live: `AXShowMenu` the parent (gate suppressed), resolve the leaf among the now-live children, then press **before** dismissing.
5. **Press:** `AXUIElementPerformAction(leaf, "AXPress")`. Re-read `AXEnabled` immediately before pressing; skip (and give feedback) if disabled.
6. **NEVER `AXPress` a top-level title or submenu parent as navigation** — that opens the real menu. Always resolve to and press the leaf.

- Firing `AXPress` on a leaf executes the item's target/action over IPC **without opening/animating the menu and without activating the target** for static/native items (confirmed by prior art; not formally Apple-documented → S3 across native/Catalyst/Electron/Qt/Java).
- Caveat: an app's own handler may `activate`/`orderFront` itself (app-specific, not Lintel's doing).
- **Fallback (narrow, spike-gated):** a dynamic item *with a key-equivalent* may be dispatched via `CGEventCreateKeyboardEvent` + `CGEventPostToPid` to avoid the flash — only if S3 shows reliable delivery for that app class.

### 4.6 Caching & refresh (targeted, gated)

- Cache the `MenuModel` keyed by pid (+ bundle id). Evict on app termination/deactivation (tied to observer eviction, §5.2).
- **Invalidation is targeted, not whole-model.** `AXMenuOpened`/`AXMenuClosed`/`AXTitleChanged` from the **app** (i.e. *not* suppressed as self-induced) trigger a re-read of the **just-changed subtree** only. Because AX menu notifications don't identify which subtree changed, key the re-read off the notification's element (the `AXMenu`/`AXMenuItem` carried in the callback) and re-resolve its path; if the element is unusable, re-read the owning top-level menu.
- **Self-induced events are ignored** via the suppression gate (§5.2) so §4.4/§4.5 never invalidate the model they are populating.

---

## 5. Focus / Window Tracking & Event Loop

### 5.1 App focus

- Register on **`NSWorkspace.shared.notificationCenter`** (the dedicated center) for `NSWorkspaceDidActivateApplicationNotification` (payload `NSRunningApplication`), `...DidDeactivate...`, and `NSWorkspaceActiveSpaceDidChangeNotification`.
- Get `pid`; seed `AXUIElementCreateApplication(pid)` + an `AXObserver`.

### 5.2 Window focus, geometry, lifecycle — observers, gate, teardown

- Per activated app, create an `AXObserver` (`AXObserverCreate`) and `AXObserverAddNotification` for: `AXFocusedWindowChanged`, `AXMainWindowChanged`, `AXWindowMoved`, `AXWindowResized`, `AXWindowMiniaturized`, `AXWindowDeminiaturized`, `AXUIElementDestroyed`, `AXFocusedUIElementChanged`, plus `AXMenuOpened`/`AXMenuClosed` (for §4.6). Add `AXObserverGetRunLoopSource(obs)` to the **main** `CFRunLoop` (`kCFRunLoopDefaultMode`) — **mandatory**.
- **Refcon & ownership (review):** store the context as **`Box<ObserverCtx>`** via `Box::into_raw` for the `refcon`; hold the observer as **`CFRetained<AXObserver>`** for deterministic drop. (Do not mix `Box` and `Retained` for the same pointer.)
- **Suppression gate:** `ObserverCtx` holds a small set of *expected self-induced menu events*, each keyed to the **opened `AXMenu` element identity** (compared with `CFEqual`), a direction (opened/closed), and an **expiry deadline** — not a bare counter. Before Lintel calls `AXShowMenu`/`AXPress`/`AXCancel`, record the expected entry; in the callback, if an `AXMenuOpened/Closed` matches a live (unexpired) entry by element identity + direction, consume it and do nothing; otherwise treat as app-driven (§4.6). A periodic sweep drops expired entries so a coalesced or never-delivered event cannot leave the gate stuck swallowing a later genuine app-driven event on the same element (the counter-desync failure mode).
- **Teardown order (exact):** (1) `CFRunLoopRemoveSource`, (2) `AXObserverRemoveNotification` per notification, (3) release the `CFRetained<AXObserver>`, (4) `Box::from_raw` the refcon. **Eviction point:** on app deactivate/terminate so observers don't accumulate under rapid Cmd-Tab.
- **Trampoline:** the callback must be `unsafe extern "C-unwind" fn(NonNull<AXObserver>, NonNull<AXUIElement>, NonNull<CFString>, *mut c_void)` (§10); it must **not unwind into AX** (wrap body in `catch_unwind`/abort) and must **`CFRetain` any element/name it stores** (arguments are +0 borrowed).
- Read the focused window via `AXFocusedWindow` (fall back `AXMainWindow`); frame via `AXPosition`/`AXSize` (`AXValueGetValue` + `kAXValueCGPointType`/`kAXValueCGSizeType`).
- **Modal sheets (review):** if the focused UI resolves to an `AXSheet`, pin above the sheet's **parent window** (or suppress the bar during modal tracking) — a sheet sits at the parent's content-top and would place the bar mid-window.

### 5.3 Reconciliation timer (drag / programmatic-move safety net)

AX geometry notifications are **not guaranteed**, are throttled/coalesced (native apps lag during fast drags), and are frequently **absent for window-server-driven moves** (Stage Manager, Spaces, tiling). Therefore:

- Primary: reposition on each `AXWindowMoved`/`AXWindowResized` (debounced).
- Safety net: a **low-rate timer** (100–250 ms; tune) re-reads `AXPosition`/`AXSize`, re-derives the owning screen, and **re-evaluates fullscreen/maximized** (§5.4).
- **Permission-revocation:** there is no AX notification for revocation; the timer treats `kAXErrorAPIDisabled` from any read as "hide the bar + re-check `AXIsProcessTrusted()`," and re-acquires the focused-window element after `AXUIElementDestroyed`.
- Enter **Moving** state during rapid motion (hide/detach), re-pin on settle.

### 5.4 Fullscreen & maximized hide rule

Hide in **two** cases; normal/partial windows always show (a non-maximized window at the top draws over the menu bar, §6.5).

- **Fullscreen — primary probe:** the **private** `AXFullScreen` attribute (absent from SDK headers = private; used by yabai). Isolate; best-effort.
- **Fullscreen — public fallback:** converted window frame ≈ `NSScreen.frame` on its own Space; combine with `NSWorkspaceActiveSpaceDidChange`. (Split-view tiles may not each report `AXFullScreen` — rely on the Space + frame signal; test.)
- **Maximized:** window covers essentially the whole **usable** area without a fullscreen Space. Compare the converted frame to the owning screen's **`visibleFrame`** within a small tolerance; also treat frame ≈ `NSScreen.frame` as maximized. There is no public "isZoomed" for a foreign window. **Compute `visibleFrame` at evaluation time** (it grows when the Dock auto-hides — a window zoomed with the Dock shown must still read maximized later; derive from the live `visibleFrame`, and add explicit test cases for Dock-auto-hide and macOS 26 tiling).
- On either, `orderOut` the panel; re-show on return to a partial frame. The reconciliation tick re-evaluates on every move/resize.

---

## 6. Overlay Panel

### 6.1 Non-activating panel

- **`NSPanel`** with style mask **`NSWindowStyleMask::NonactivatingPanel`** (128) ORed with `Borderless`. **The style mask — not the activation policy — prevents click-activation.** Set at creation on a real `NSPanel`.
- `setFloatingPanel(true)`, `setBecomesKeyOnlyIfNeeded(true)`, `setWorksWhenModal(true)`.
- **Override `acceptsFirstMouse:` → `true`** on interactive item views (`define_class!`) so the first click fires the item instead of merely keying the panel.
- **Override `canBecomeMainWindow` → `false`.** Override `canBecomeKeyWindow` → `true` only if a later search field needs keyboard.
- **Show with `makeKeyAndOrderFront`/`orderFront`** (safe on a nonactivating panel). **NEVER call `NSApplication.activate(ignoringOtherApps:)`.**
- **Responder caveat (review):** when the panel becomes key the target window resigns key — usually cosmetic, but `NSMenuValidation`/responder routing (Copy/Paste/Undo) depend on the key window's first responder, so `AXEnabled` can read stale and a press can misroute. S3 verifies **plain item clicks never make the panel key** (incl. the custom item `NSView`s).

### 6.2 Activation policy

`NSApplication.setActivationPolicy(.Accessory)` once, early in `main()` (**or** `LSUIElement=1` in Info.plist — pick ONE). Accessory removes the Dock icon/app menu; it does **not** by itself prevent frontmost (that's the panel style mask).

### 6.3 Window levels (revised)

- Static system menu bar draws at **`NSMainMenuWindowLevel` (24)**; z-order is level-then-order and the WindowServer does **not** clip app windows out of the menu-bar rect.
- **Persistent bar: use the lowest level that clears the static menu bar — `NSStatusWindowLevel` (25).** This draws over the *static* bar in the collision case (§6.5) without contending with system dropdowns.
- **Lintel's own transient dropdown: `NSPopUpMenuWindowLevel` (101)**, ordered front on open, so it sits above the bar and app content.
- Real system dropdowns/Control Center render at 101; Lintel does **not** try to out-rank them for the persistent bar. Within-level ties are nondeterministic — the persistent bar is at 25 precisely to avoid that tie. (A Lintel dropdown and a real system dropdown both open at 101 is a rare within-level tie; acceptable since each requires a distinct user action.) **S2** validates on 25F80 that (a) the level-25 bar composites over the Liquid Glass static menu bar and (b) a real system dropdown at 101 correctly overtakes the bar.

### 6.4 Spaces / collection behavior

`setCollectionBehavior([MoveToActiveSpace, Stationary, IgnoresCycle, Transient])` so the bar follows the active Space and stays out of Cmd-` / Exposé. (Default hides in fullscreen, so `FullScreenAuxiliary` is not needed.)

### 6.5 The "window touches top" collision case

Applies only to **non-maximized** windows (maximized → hidden, §5.4). When placement (§8) would overlap the static menu-bar strip, **do not clamp below it** — let the level-25 bar draw over the static menu bar. **But never cover the right-side status region or the notch:** if the bar's rect would intersect the menu-bar-extra/status area (clock/Control Center/Notification Center) or the notch auxiliary areas, **clamp the bar left**, and if it still cannot avoid them, **hide** rather than block system UI. On non-notched external ultrawides (the primary target) there is no notch; on built-in displays use `safeAreaInsets.top`/`auxiliaryTopLeftArea`/`auxiliaryTopRightArea`.

### 6.6 Submenu UI — one reusable dropdown surface (revised)

Instead of a tree of nested nonactivating panels (which would hand-reimplement all of `NSMenu`'s tracking across N panels), Lintel uses **a single reusable dropdown `NSPanel`** (level 101) driven by **one tracking state machine**:

- Opening a top-level menu shows the dropdown below that bar item; entering a submenu **push-navigates within the same surface** (miller/breadcrumb or slide), so there is exactly one panel and one owner of hover state.
- The tracking state machine defines: **open delay** and **close delay**, **safe-triangle** diagonal tolerance (pointer traveling toward the submenu doesn't dismiss), **dismiss-on-click-outside**, **dismiss-on-Escape**, and **model-change handling per §3.3** (default: defer re-render of an *open* dropdown until it closes; the persistent bar always re-renders from the newest snapshot).
- Dynamic parents populate on demand via §4.4 when navigated into.
- This is a **first-class component**, not a Phase-checkbox — it is the biggest unproven UI risk after the styling spike (S1).

---

## 7. Styling (Acrylic / Liquid Glass)

Host either backing view inside the borderless panel with `isOpaque=false` and `backgroundColor = NSColor.clear`.

### 7.1 Primary: `NSGlassEffectView` (macOS 26+, public, bound in objc2-app-kit 0.3.2)

- Put item content in `contentView`; set `cornerRadius`, optional `tintColor`, `style` (`NSGlassEffectViewStyle` — pin exact case identifiers, Regular=0/Clear=1, before writing S1).
- If each item is its own glass pill, wrap the row in **`NSGlassEffectContainerView`** (`spacing`) so adjacent glass views don't sample each other.
- Guard behind a runtime class check (objc2 doesn't enforce OS availability). `effectIsInteractive` is beta; the classes are not.

### 7.2 Fallback: `NSVisualEffectView` (10.10+, proven behind-window)

- `setMaterial(...)` — pick empirically among `.hudWindow`, `.menu`, `.popover`, `.headerView`, `.underWindowBackground`.
- `setBlendingMode(.behindWindow)`.
- **`setState(.active)`** — REQUIRED (the non-activating panel never becomes key, so `.followsWindowActiveState` would dim it).
- Rounded corners via `maskImage`.

### 7.3 Unverified: behind-window sampling over a FOREIGN live window — SPIKE S1

Not documented whether either view's behind-window blur updates live over a *different* app's window under a non-activating overlay. **Prototype first.** If it misbehaves → `NSVisualEffectView` fallback.

### 7.4 Menu-bar shifting — DROPPED (infeasible; no public/known-private API).

---

## 8. Geometry & Placement

### 8.1 Coordinate spaces

- **AX** `AXPosition`/`AXSize`: global CG display space, **top-left origin of the primary display, y-down, in points**; secondary displays may have negative origins.
- **Cocoa/NSScreen:** same global space, **bottom-left origin, y-up, points**.

### 8.2 The conversion (single rect flip against the ORIGIN screen)

Use the **origin** screen height (the screen whose `frame.origin == (0,0)`, conventionally `NSScreen.screens[0]`), **NOT `mainScreen`**:

```
let primary_h = screens[0].frame.size.height;
let panel_origin = NSPoint {
    x: ax_x,
    y: primary_h - ax_y - panel_h,        // panel bottom sits on window top
};
```

Omitting `panel_h` mislocates by ~one bar height (the most likely placement bug). `NSRect`/`CGRect` share layout, so `AXValueGetValue` fills a rect directly usable by AppKit.

### 8.3 Which-screen detection

Pick the screen with **maximum frame-intersection area** (beats center containment for straddling windows). **Ultrawide needs nothing special** — a far window has a large positive x; the bar anchors there (clamped).

### 8.4 Placement algorithm

```
on (focus change | AXWindowMoved/Resized | reconciliation tick | model change):
  if !trusted():              hide; return          // §5.3 revocation
  win = focusedWindow(app)
  if win resolves to AXSheet: win = parentWindow(win)  // §5.2
  if none:                    hide; return
  if isFullscreen(win):       hide; return          // §5.4
  rect   = axRect(win) → cocoaRect (§8.2)
  screen = ownerByMaxIntersection(rect) (§8.3)
  if isMaximized(rect, screen): hide; return        // §5.4 (visibleFrame at eval time)
  barW   = min(desiredMenuWidth, screen.visibleFrame.width)
  x      = clamp(rect.minX, screen.frame.minX, screen.frame.maxX - barW)
  yTop   = rect.maxY_cocoa
  bar    = (x, yTop, barW, barH)
  if bar overlaps statusRegion(screen) or notchAux(screen):   // §6.5
      shift bar left to clear; if impossible: hide; return
  # if bar overlaps the static menu-bar strip, level 25 draws OVER it (§6.3/§6.5)
  panel.setFrame(bar); panel.orderFront                       // level 25
```

---

## 9. Permissions / Packaging / Distribution

### 9.1 Accessibility (TCC)

- Gate all AX use behind **`AXIsProcessTrustedWithOptions(kAXTrustedCheckOptionPrompt=true)`** at startup; poll `AXIsProcessTrusted()`; degrade (hide) when untrusted (also handled by §5.3 on `kAXErrorAPIDisabled`).
- **Non-sandboxed.**

### 9.2 Stable signing for TCC persistence (critical dev constraint)

An ad-hoc/unsigned build's cdhash changes each rebuild → stored `csreq` no longer matches → grant silently resets. From day one:

- **Stable `CFBundleIdentifier`** + **reuse one stable signing identity** every build. A **persistent self-signed cert suffices** for local TCC persistence (Developer ID + notarization only for distribution).
- Automate `codesign --sign <id> --options runtime` after bundling.
- `tccutil reset Accessibility <bundle-id>` clears stale entries during dev.

### 9.3 Packaging

- Ship a real **.app bundle** (TCC `client_type=0`; **macOS 26 System Settings won't render path-based entries**).
- `cargo-bundle` maps `identifier`→`CFBundleIdentifier`, `version`, injects `LSUIElement=1` + `LSMinimumSystemVersion`. It does **not** sign — bundle + sign are two steps. Verify the emitted Info.plist.
- **No notarization** for local use.

---

## 10. Rust Binding Approach

- **One binding family:** madsmtm/objc2 — core **0.6.4**; framework crates **0.3.2**. UI via `objc2-app-kit`/`objc2-foundation`; AX C API via **`objc2-application-services` 0.3.2** (full AX surface since issue #624). Single `objc2-core-foundation` CF stack; **avoid** `accessibility-sys`/`axuielement` as deps (reference only for constant values).
- **Feature gating:** on `objc2-application-services` enable `AXUIElement`, `HIServices`, `AXError`, `AXValue`, `libc`; on `objc2-app-kit` enable the per-class features (see recommended-crates in the appendix).
- **AX name constants are NOT re-exported** (`CFSTR` macros). Hand-declare a **single verified constants module** of `CFString`s (`"AXMenuBar"`, `"AXMenuBarItem"`, `"AXMenu"`, `"AXMenuItem"`, `"AXChildren"`, `"AXRole"`, `"AXTitle"`, `"AXEnabled"`, `"AXMenuItemMarkChar"`, `"AXMenuItemCmdChar"`, `"AXMenuItemCmdModifiers"`, `"AXPosition"`, `"AXSize"`, `"AXFocusedWindow"`, `"AXMainWindow"`, `"AXFocusedWindowChanged"`, `"AXWindowMoved"`, `"AXWindowResized"`, `"AXWindowMiniaturized"`, `"AXUIElementDestroyed"`, `"AXPress"`, `"AXShowMenu"`, `"AXCancel"`, `"AXMenuOpened"`, `"AXMenuClosed"`, `"AXFullScreen"` (private), `"AXSheet"`). **Unit-test each name against the correct namespace** — a wrong value fails silently (`kAXErrorAttributeUnsupported` / no callbacks). Assert **attributes** (`AXMenuBar`, `AXTitle`, `AXChildren`, `AXPosition`, …) appear in `AXUIElementCopyAttributeNames` on a live element; assert **actions** (`AXPress`, `AXShowMenu`, `AXCancel`) appear in `AXUIElementCopyActionNames` on a menu item; verify **notifications** (`AXMenuOpened/Closed`, `AXWindowMoved`, …) by a successful `AXObserverAddNotification` for each (a bad name returns `kAXErrorNotificationUnsupported`). Do NOT assert action/notification names via `CopyAttributeNames` — they are not attributes and the test would false-fail.
- **Subclassing/overrides** via `objc2::define_class!` with `#[thread_kind = MainThreadOnly]`. **Overrides are validated at runtime** — spell out exact ObjC selectors + Rust signatures: `acceptsFirstMouse:` (`Option<&NSEvent> -> bool`), `canBecomeKeyWindow`/`canBecomeMainWindow` (`-> bool`), `mouseDown:`, `mouseEntered:`/`mouseExited:`, `mouseMoved:`. A wrong selector panics at class registration on first launch.
- **AXObserver callback:** `unsafe extern "C-unwind" fn(NonNull<AXObserver>, NonNull<AXUIElement>, NonNull<CFString>, *mut c_void)`; `catch_unwind`/abort; `CFRetain` stored args; `Box<ObserverCtx>` refcon via `into_raw`/`from_raw` (§5.2 teardown order).
- **AX FFI ergonomics:** wrap CFType retain/release with `CFRetained`; map `AXError`; handle the multi-attribute-read `kAXValueAXErrorType` per-element failure (§4.2). Prefer the idiomatic `AXUIElement`/`AXObserver` methods over the deprecated free C functions. **Isolate all raw AX FFI in MenuReader.**

---

## 11. Key Risks & De-risking Spikes

| # | Question | Spike |
|---|---|---|
| S1 | Does behind-window blur (`NSGlassEffectView` / `NSVisualEffectView.behindWindow`) sample a **foreign app's live window** under a non-activating panel, and update live? | Minimal panel over a moving foreign window. Bad → `NSVisualEffectView` fallback. |
| S2 | On 25F80, does a **level-25** bar composite over the Liquid Glass static menu bar, and do real **level-101** dropdowns / Control Center / Notification Center / app-modal alerts correctly overtake it? Does a bar near the top-right block system status UI (click occlusion, not just visuals)? | Place bar at 25; open real system menu + Control Center + an alert; test click pass-through; screen-record. |
| S3 | For each app class (native / Catalyst / Electron / Qt / Java): does leaf `AXPress` fire without flash/focus-steal? Do dynamic child elements **survive menu close** (press-in-place) or need re-resolve? Does `AXCancel` reliably dismiss? Does reading `AXChildren` trigger `menuNeedsUpdate:` **invisibly** on 26.5? Do plain item clicks ever make the panel key? | Instrument reads/presses/cancels across representative apps; log flash, staleness, key-window changes, dismiss failures. |
| S4 | `AXWindowMoved` cadence during drags on 26.5; do Stage Manager/Spaces/tiling moves emit anything? | Measure; tune reconciliation timer; confirm hide-on-motion. |
| S5 | Does a **reused self-signed** identity keep the TCC grant across rebuilds on 26.5? | Grant → rebuild+re-sign → confirm no re-prompt. |
| S6 | End-to-end AX FFI: read `AXMenuBar`, resolve+`AXPress` a leaf, wire an `AXObserver` with a `Box` refcon + `C-unwind` trampoline + correct teardown. | Phase-0 walking-skeleton compile spike. |

---

## 12. Phased Implementation Roadmap

**Phase 0 — Walking skeleton (compile spike).**
- Cargo project; objc2 deps + features; `NSApplication` (`.Accessory`); `AXIsProcessTrustedWithOptions` prompt.
- .app bundle + stable-identity `codesign` in the build (S5); verify the TCC toggle appears (bundle, not bare binary).
- Verified AX-constants module + `AXUIElementCopyAttributeNames` test.
- MenuReader: read `AXMenuBar` → top-level titles; resolve+`AXPress` a static leaf; wire one `AXObserver` with the `C-unwind` trampoline + `Box` refcon + full teardown (S6). Confirm the press fires in the target without activating Lintel.

**Phase 1 — Static bar, single display.**
- FocusTracker: `NSWorkspace` activation + per-app `AXObserver` (focused window, move/resize) on the main run loop, with eviction.
- Coordinator owns a versioned snapshot; Geometry AX→Cocoa flip (§8.2); place a plain `NonactivatingPanel` at level 25 above the focused window.
- Render eager static top-level + first-level items as clickable views (`acceptsFirstMouse` override); click → resolve+`AXPress` leaf (re-read `AXEnabled` first).
- **Styling spike S1.**

**Phase 2 — Interactivity depth + dynamic menus.**
- **Single reusable dropdown surface** + tracking state machine (§6.6): open/close delays, safe-triangle, dismiss.
- Dynamic classification (§4.3) + feature-detect element survival (S3); populate via `AXShowMenu` + poll + **`AXCancel`** dismiss (§4.4); **observer suppression gate** (§5.2); targeted subtree refresh (§4.6).
- Separators, disabled/mark states, CmdChar shortcut rendering.

**Phase 3 — Multi-display / ultrawide / collisions / fullscreen.**
- Which-screen by max intersection; clamp; ultrawide validation on the real rig.
- Over-the-menu-bar case at level 25 + **status-region/notch clamp** (S2); modal-sheet parent placement.
- Fullscreen + **maximized** hide (visibleFrame-at-eval-time; Dock-auto-hide & tiling tests) (§5.4).
- Reconciliation timer + hide-on-motion + **permission-revocation** handling (S4).

**Phase 4 — Robustness & polish.**
- Non-native/Catalyst feature-detection & graceful degrade (bail cleanly with no usable `AXMenuBar`); fully-dynamic-deep-tree degrade to "open real menu"/key-equivalent.
- Liquid Glass container grouping; tint/corner tuning; performance (batched reads, timeouts, debounced observers).
- Deferred shortcut glyph/virtual-key mapping; optional keyboard navigation.
