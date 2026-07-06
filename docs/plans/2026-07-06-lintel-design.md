# Lintel — Design Document

*Target: macOS 26.5 (build 25F80, "Liquid Glass"), Command Line Tools SDK 26.5, Rust (rustc 1.96.1, edition 2024), objc2 crate family. Verified against the local CLT SDK on 2026-07-06.*

---

## 1. Overview & Goals

Lintel is a background macOS utility that renders a **local menu bar**: when a window gains focus, Lintel reads the focused application's menu bar over the Accessibility (AX) API and draws a **copy of it as a floating, acrylic/frosted bar pinned just above the focused window**. On a super-ultrawide display the real menu bar (top-left of the primary screen) can be far from the working window; Lintel brings the menu to the window.

**Hard requirements:**

1. **Fully interactive** — clicking a mirrored item triggers the *real* menu action in the target app, including submenus.
2. **Non-activating** — interacting with the bar must NOT steal focus from the target app. (If focus moved to Lintel, the target's frontmost status would change and the very menu we mirror could change.)
3. **Hides in native fullscreen _and when the window is maximized_** (zoomed to fill the display) — see §5.4 for the maximized definition.
4. **Over-the-menu-bar collision handling** — when the focused window sits at the very top and the bar would collide with the real system menu bar, draw the acrylic bar **over** it. (Shifting the system menu bar aside is a stretch goal — see §7.4; it is **infeasible** and dropped.)
5. **Acrylic / frosted-glass styling.**

**Prior art that de-risks the core:** Menuwhere (Many Tricks, shipping since ~2016/2021) reads the frontmost app's full menu tree over AX and executes items incl. submenus and background apps; the MIT `right-click-menubar` reproduces the exact `AXUIElementCreateApplication → kAXMenuBarAttribute → recursive kAXChildren → AXUIElementPerformAction(kAXPressAction)` path. **What is novel** (no single prior art): a *persistent, non-activating, interactive, acrylic* bar that mirrors live and can be drawn over the system menu bar.

---

## 2. Non-Goals / YAGNI

- **Do NOT shift/reposition the real system menu bar.** No public or known-private API repositions the Apple/app menus; drop it. Cover the region instead.
- **Do NOT support the Mac App Store / App Sandbox.** There is no sandbox entitlement for a general accessibility client; Lintel ships non-sandboxed, direct-download.
- **Do NOT synthesize keystrokes as the primary trigger.** `CGEventPostToPid` to a non-focused app is unreliable and most items have no key-equivalent; keep it only as a narrow per-item fallback (§4.5).
- **Do NOT eagerly pre-walk entire deep menu trees** of every app on every focus change. Read top-level + first-level statically; lazily populate dynamic submenus on hover.
- **No notarization for local/personal use** (a locally built .app has no quarantine bit). Notarization is only for distributing to other machines.
- **No multi-app aggregated menu** (Menuwhere's "all apps" mode). Lintel mirrors only the focused app.
- **No keyboard-driven navigation of the mirrored bar** in v1 (mouse only). Adding a search field later would require overriding `canBecomeKeyWindow` (§6.1).

---

## 3. System Architecture

### 3.1 Components

```
                         ┌─────────────────────────────────────────┐
                         │  main thread (MainThreadMarker)          │
                         │  NSApplication run loop (.Accessory)     │
                         └─────────────────────────────────────────┘
   ┌──────────────┐   focus events    ┌──────────────────┐
   │ FocusTracker │──────────────────▶│  Coordinator     │
   │ NSWorkspace  │                    │  (state machine) │
   │ + AXObservers│◀── geometry ──────│                  │
   └──────────────┘                    └──────────────────┘
          │                                   │   │
          │ pid / window elem                 │   │ menu model
          ▼                                   ▼   ▼
   ┌──────────────┐                    ┌──────────────┐   ┌──────────────┐
   │ MenuReader   │  AXMenu* tree      │ Geometry     │   │ OverlayPanel │
   │ (AX FFI)     │───────────────────▶│ (AX→Cocoa)   │──▶│ NSPanel      │
   │ read+cache   │                    │ placement    │   │ + glass view │
   │ +AXPress     │◀── click routes ───┼────────────────  │ + item views │
   └──────────────┘                    └──────────────┘   └──────────────┘
```

- **Coordinator** — owns app state; decides when to show/hide/move/rebuild the bar. Pure Rust logic, testable.
- **FocusTracker** — `NSWorkspace.shared.notificationCenter` for app activation; per-app `AXObserver` for focused-window and geometry/lifecycle notifications; a low-rate reconciliation timer.
- **MenuReader** — the AX FFI module: reads the menu tree into a Rust `MenuModel`, caches per app, refreshes from AX menu notifications, and executes `AXUIElementPerformAction(kAXPressAction)` on click. **All raw AX FFI is isolated here** behind a small safe wrapper.
- **Geometry** — converts AX window frames (global top-left points) to Cocoa (bottom-left points), picks the owning screen, computes the panel frame incl. the top-collision and fullscreen cases.
- **OverlayPanel** — the single `NSPanel` (NonactivatingPanel), its glass backing view, and the row of interactive item views.

### 3.2 Threading

Everything runs **on the main thread**. AXObserver run-loop sources are added to the **main** `CFRunLoop` (`CFRunLoopGetCurrent()` on the main thread), so AX callbacks arrive on the main thread — no cross-thread marshaling and no data races with AppKit. If a specific menu read proves slow enough to stall UI, move *only that read* to a worker and hop back via `dispatch2` main queue (verify its main-queue helper first); default is main-thread with a short `AXUIElementSetMessagingTimeout`.

---

## 4. Menu Model

### 4.1 Data model

```rust
struct MenuModel { top: Vec<MenuBarItem> }              // AXMenuBar → items
struct MenuBarItem { title: String, ax: AxElem, menu: Option<Submenu> }
struct Submenu { items: Vec<MenuEntry>, populated: Populated }
enum   Populated { Static, Dynamic { opened_once: bool } } // lazy-load state
enum   MenuEntry {
    Item { title, enabled: bool, mark: Option<char>,      // AXMenuItemMarkChar
           shortcut: Option<Shortcut>, submenu: Option<Submenu>, ax: AxElem },
    Separator,                                            // empty-title heuristic
}
struct Shortcut { key: Key, mods: Modifiers }            // Key = char | virtualkey | glyph
```

`AxElem` wraps a `CFRetained<AXUIElement>`; it is the *live handle* used to fire the action, held for the lifetime of the cache entry.

### 4.2 Reading (traversal)

Path (all constants verified present in the local CLT SDK `HIServices` headers):

```
AXUIElementCreateApplication(pid)
  → CopyAttributeValue("AXMenuBar")                     // kAXMenuBarAttribute
    → "AXChildren"  → [AXMenuBarItem]                   // role kAXMenuBarItemRole
       → each has one "AXMenu" child                    // kAXMenuRole
          → "AXChildren" → [AXMenuItem]                 // kAXMenuItemRole
             → an item with its own "AXMenu" child = submenu
```

Per item, batch-read with **`AXUIElementCopyMultipleAttributeValues`** (one IPC round-trip per element): `AXTitle`, `AXEnabled`, `AXRole`, `AXMenuItemMarkChar`, `AXMenuItemCmdChar`, `AXMenuItemCmdVirtualKey`, `AXMenuItemCmdGlyph`, `AXMenuItemCmdModifiers`, `AXChildren`.

- **Separators** are heuristic: empty `AXTitle`, no action, disabled. Validate against Accessibility Inspector across sample apps.
- **Shortcut decode:** prefer `AXMenuItemCmdChar`; else `AXMenuItemCmdVirtualKey` (keycode) or `AXMenuItemCmdGlyph` (glyph id for arrows/return/delete). **Modifier bitmask is inverted vs NSEvent:** `Shift=1, Option=2, Control=4, NoCommand=8`; **Command is implied** (0 ⇒ Cmd-only); bit 3 set ⇒ NO Command.
- Set **`AXUIElementSetMessagingTimeout`** (≈1–2 s) so a hung target can't block the walk.

### 4.3 Eager vs. lazy (the load-bearing caveat, corrected)

The verifiers **narrowed** the original lazy-load claim. Read strategy:

- **Read eagerly (static):** all top-level `AXMenuBarItem`s and their first-level `AXMenu` children. Crucially, **Window (NSApplication addWindowsItem:), Font (NSFontManager), and Open Recent (NSDocumentController) ARE readable without opening** — they are AppKit-managed persistent `NSMenuItem`s, not rebuilt-on-open. A passive `AXChildren` read also triggers `-[NSMenu update]`/`menuNeedsUpdate:` for most native menus **without any visible display**, so most deep static submenus enumerate cold too.
- **Treat as DYNAMIC (lazy) only when:** an `AXMenuItem` is submenu-capable but its `AXMenu` reports **zero** children. The true dynamic cases are (1) menus built at tracking-start in `NSMenuDelegate menuWillOpen:`, and (2) **non-native toolkits** (Electron/Chromium, Qt, Java/JUCE, custom popup controls) with no backing `NSMenu`. For these, children do not exist in AX until a tracking session begins.

### 4.4 Lazy-load handling (dynamic submenus)

On hover/open of a mirrored dynamic parent:

1. `AXUIElementPerformAction(parent, "AXShowMenu")` (or `AXPress` the parent). **This visibly opens the real submenu** — the unavoidable flash for this minority.
2. Poll the parent's `AXMenu`'s `AXChildren` until non-empty or timeout (bounded by messaging timeout).
3. Read the now-populated children into the model; cache with `opened_once = true`.
4. Dismiss the real menu (Escape via the fallback path, or re-press) and render the mirror.
5. Subscribe to `AXMenuOpened`/`AXMenuClosed` on the app element to keep the cache fresh and to catch content changes cheaply instead of re-walking.

Because the target app stays frontmost throughout (Lintel never activates), opening the real submenu does not change *which* app's menu we mirror. Enabled state for dynamic items may be **stale until validated once** (`validateMenuItem:`/NSMenuValidation runs at display time); read `AXEnabled` right before pressing and, if unreliable, prime once.

### 4.5 Triggering (click → real action)

- **Primary:** resolve the mirrored control to the **leaf `AXMenuItem`** and call `AXUIElementPerformAction(leaf, "AXPress")`. This fires the item's target/action over IPC **without opening/animating the menu and without activating the target** — confirmed by prior art and AX semantics (though not formally Apple-documented, so verify per app). Run off the UI thread or with a short timeout to survive a busy target.
- **NEVER `AXPress` a top-level title or a submenu parent as a navigation step** — that *opens* the real menu. Always resolve to and press the leaf directly; for a nested item whose branch is dynamic, populate via §4.4 first, then press the leaf.
- Caveat: an app's own action handler may `activate`/`orderFront` itself as a side effect (app-specific, not Lintel's doing).
- **Fallback (narrow):** for an item that has a key-equivalent AND fails `AXPress`, synthesize it via `CGEventCreateKeyboardEvent` + `CGEventPostToPid`. Unreliable for non-focused apps and cannot cover items without shortcuts — last resort only.

### 4.6 Caching & refresh

Cache the `MenuModel` keyed by pid (+ bundle id). Invalidate on app switch, on `AXMenuOpened`/`AXMenuClosed`/`AXTitleChanged`, and on app termination. Re-read only the changed subtree where possible.

---

## 5. Focus / Window Tracking & Event Loop

### 5.1 App focus

- Register on **`NSWorkspace.shared.notificationCenter`** (the dedicated center — the default `NSNotificationCenter` receives nothing) for `NSWorkspaceDidActivateApplicationNotification` (payload `NSWorkspaceApplicationKey` = `NSRunningApplication`), `...DidDeactivateApplication...`, and `NSWorkspaceActiveSpaceDidChangeNotification` (re-evaluate/hide on Space switch).
- Get `pid` from `NSRunningApplication.processIdentifier`; seed `AXUIElementCreateApplication(pid)` and an `AXObserver`.

### 5.2 Window focus, geometry, lifecycle

- Per activated app, create an `AXObserver` (`AXObserverCreate`) and `AXObserverAddNotification` for: `AXFocusedWindowChanged`, `AXMainWindowChanged`, `AXWindowMoved`, `AXWindowResized`, `AXWindowMiniaturized`, `AXWindowDeminiaturized`, `AXUIElementDestroyed`, `AXFocusedUIElementChanged`. Add `AXObserverGetRunLoopSource(obs)` to the **main** `CFRunLoop` (`CFRunLoopDefaultMode`) — **mandatory**, no callbacks fire otherwise. Store a heap `Box`/`Retained` context pointer as the observer `refcon`; remove sources and observers on teardown to avoid leaks.
- Read the focused window via `AXFocusedWindow` (fall back to `AXMainWindow`); read frame with `AXPosition`/`AXSize` (AXValue boxes unwrapped by `AXValueGetValue` with `kAXValueCGPointType`/`kAXValueCGSizeType`).
- Optionally use the system-wide element (`AXUIElementCreateSystemWide` + `AXFocusedUIElementChanged`) to reduce per-app observer churn — but a per-pid observer is still needed for window move/resize granularity.

### 5.3 Reconciliation timer (drag/programmatic-move safety net)

AX geometry notifications are **not guaranteed**, are throttled/coalesced (native apps *lag* during fast drags), and are frequently **absent for window-server-driven moves** (Stage Manager, Spaces, tiling). Therefore:

- Primary: reposition on each `AXWindowMoved`/`AXWindowResized` callback (debounced).
- Safety net: a **low-rate timer** (e.g. 100–250 ms; tune empirically) re-reads `AXPosition`/`AXSize` and re-derives the owning screen to catch missed moves. Optionally raise cadence only while a move is in progress.
- During rapid motion, **hide/detach** the bar and re-pin on settle to avoid visible drift.
- Note: Electron/Java top-level windows ARE native `NSWindow`s that post geometry notifications; only their *content* AX tree is lazy, which placement does not need.

### 5.4 Fullscreen & maximized hide rule

Lintel hides the bar in **two** cases: native fullscreen, and "maximized" (the window fills the display but is not in a fullscreen Space). Normal/partial windows always show a bar — including a non-maximized window dragged against the top, which draws *over* the menu bar (§6.5).

- **Fullscreen — primary probe:** the **private** `AXFullScreen` attribute on the focused window (confirmed absent from the SDK headers = private; used by yabai). Isolate it; treat as best-effort.
- **Fullscreen — public fallback:** compare the (converted) window frame to `NSScreen.frame` — a native-fullscreen window occupies the entire display on its own Space; combine with `NSWorkspaceActiveSpaceDidChange`.
- **Maximized:** the window covers essentially the whole *usable* area of its screen without being in a fullscreen Space. Detect by comparing the converted window frame to the owning screen's **`visibleFrame`** (menu-bar/Dock-excluded) within a small tolerance (≈ a few points per edge); also treat a frame ≈ `NSScreen.frame` as maximized. There is no public "isZoomed" for a *foreign* window (the `AXZoomButton`/`AXFullScreenButton` elements report presence, not zoom *state*), so the frame-vs-`visibleFrame` comparison is the reliable signal. Tune the tolerance on the real rig.
- When either is detected, hide the panel (`orderOut`); re-show when the window returns to a normal (partial) frame. Because "maximized" is a frame comparison, the reconciliation tick (§5.3) re-evaluates it on every move/resize with no extra machinery.

---

## 6. Overlay Panel

### 6.1 Non-activating panel

- **`NSPanel`** created with style mask **`NSWindowStyleMask::NonactivatingPanel`** (value 128) ORed with `Borderless`. **The style mask — not the activation policy — is what prevents click-activation** (Apple: "a panel … that does not activate the owning app"). Set at creation on an actual `NSPanel` (toggling on a bare `NSWindow` is unreliable).
- `setFloatingPanel(true)`, `setBecomesKeyOnlyIfNeeded(true)`, `setWorksWhenModal(true)` (receive events even during menu tracking).
- **Override `acceptsFirstMouse(for:) -> true`** on interactive item views (via `define_class!`) — REQUIRED so the first click on a custom-drawn view fires the item instead of merely making the panel key (default is false; only NSButton/NSSlider-style controls accept click-through).
- **Override `canBecomeMainWindow -> false`** (reinforce non-activation). Override **`canBecomeKeyWindow -> true` only** if a later search field needs keyboard focus — even then the NonactivatingPanel mask keeps the app non-frontmost.
- **Show with `makeKeyAndOrderFront`/`orderFront`** (safe on a nonactivating panel — does NOT activate). **NEVER call `NSApplication.activate(ignoringOtherApps:)`.** When the panel becomes key the target window resigns key status (cosmetic focus-ring/cursor change) but **remains frontmost**, preserving its menu.

### 6.2 Activation policy

`NSApplication.setActivationPolicy(.Accessory)` once, early in `main()` before launch finishes (or `LSUIElement=1` in Info.plist — pick ONE, not both, per winit #261). Accessory removes the Dock icon and app menu and runs a normal event loop. It does **not** by itself prevent frontmost — non-activation comes from the panel style mask.

### 6.3 Window level (over the menu bar)

- The system menu bar draws at **`NSMainMenuWindowLevel` (24)**; z-order is by level then order, and the WindowServer does **not** clip app windows out of the menu-bar rectangle. Any window at a higher level positioned over `NSScreen.frame`'s top strip draws over it — **including from an accessory app** (level ordering is activation-independent).
- **Use `NSPopUpMenuWindowLevel` (101)** (or `CGShieldingWindowLevel()`) for the bar **and its own dropdowns**, NOT merely `NSStatusWindowLevel` (25): real system menu dropdowns and Control Center render at 101, so a bar at 25 would be overdrawn. Use the *lowest* level that achieves dominance to avoid covering system alerts.
- Note: a *normal-level* window is pushed out from under the menu bar by AppKit's `constrainFrameRect`; raising the level avoids this.
- **Validate on macOS 26.5 (build 25F80)** that level 101 composites above the transparent Liquid Glass menu bar (Liquid Glass changed menu-bar rendering and broke menu-bar managers — layering is empirical, not contractually guaranteed).

### 6.4 Spaces / collection behavior

`setCollectionBehavior([CanJoinAllSpaces or MoveToActiveSpace, Stationary, IgnoresCycle, Transient])` so the bar follows the active Space, stays out of Cmd-` cycling/Exposé. Consider `FullScreenAuxiliary` only if ever showing over fullscreen aux windows (default: hide in fullscreen per §5.4).

### 6.5 The "window touches top" collision case

This case applies only to **non-maximized** windows dragged against the top — a maximized window is hidden entirely (§5.4), so there is no bar to collide. When the placement (§8) would put a (non-maximized) window's bar rectangle overlapping the system menu-bar strip (`cocoa top y > visibleFrame.maxY` on the menu-bar-hosting screen), **do not clamp below it** — instead let the bar draw over the menu bar at level ≥101 (§6.3). This is the intended over-the-menu-bar behavior. On non-notched external ultrawides (the user's primary target) there is no notch to route around; on built-in notched displays use `safeAreaInsets.top`/`auxiliaryTopLeftArea`/`auxiliaryTopRightArea` to lay out around the cutout.

---

## 7. Styling (Acrylic / Liquid Glass)

Both paths are public and bound in `objc2-app-kit` 0.3.2; host either inside the borderless panel with `isOpaque=false` and `backgroundColor = NSColor.clear` (required for behind-window blending).

### 7.1 Primary: NSGlassEffectView (macOS 26.0+, public)

- Put item content in `contentView` (only the contentView is guaranteed inside the glass; don't rely on sibling z-order). Set `cornerRadius`, optional `tintColor`, `style` (`NSGlassEffectViewStyle` — verify exact case names in generated source; Regular=0/Clear=1). Use the clear variant sparingly.
- If each mirrored item is its own glass pill, wrap the row in **`NSGlassEffectContainerView`** (`spacing`) — two glass views placed too close otherwise sample each other and render incorrectly.
- Guard behind a runtime class check even though the target is 26-only (objc2 does not enforce OS availability at compile time). `effectIsInteractive` is still beta; the classes themselves are not.

### 7.2 Fallback: NSVisualEffectView (10.10+, proven behind-window)

- `setMaterial(...)` — pick empirically among `.hudWindow`, `.menu`, `.popover`, `.headerView`, `.underWindowBackground` (avoid deprecated `.light/.dark/.appearanceBased`).
- `setBlendingMode(.behindWindow)` — blurs the content behind the panel (the true frosted look over another app's window).
- **`setState(.active)`** — REQUIRED: the non-activating panel never becomes key, so the default `.followsWindowActiveState` would render dimmed.
- Rounded corners via `maskImage` (masks material, not subviews); or clip subviews separately.

### 7.3 Unverified: behind-window sampling over a FOREIGN live window

It is **not documented** whether either view's behind-window blur updates live over a *different* app's window under a non-activating overlay. **Spike this in Phase 1.** If glass misbehaves, fall back to `NSVisualEffectView` (a decade of proven behind-window behavior).

### 7.4 Menu-bar shifting — DROPPED

No public or known-private API repositions the Apple/app menus or status items. Existing menu-bar managers only hide/rearrange right-side status items. Cover the region (§6.3); do not attempt to move the bar.

---

## 8. Geometry & Placement

### 8.1 Coordinate spaces (Apple-documented)

- **AX** `kAXPositionAttribute`/`kAXSizeAttribute` are in the **global CG display space**: origin at the **top-left of the primary display, y-down, in POINTS** (not pixels, not per-display-local). Secondary displays may have negative origins.
- **Cocoa/NSScreen** is the same global space but **bottom-left origin, y-up, in points**, anchored to the same primary display.

### 8.2 The conversion (single flip against the ORIGIN screen — corrected)

Use the **primary/origin** screen height, i.e. the screen whose `frame.origin == (0,0)` (conventionally `NSScreen.screens[0]`), **NOT `NSScreen.mainScreen`** (which follows focus). This one flip is exact on any display incl. negative-origin and mixed-DPI, because the global space is unified in points.

**It is a rect flip, not a point flip — subtract the height term:**

```
let primary_h = screens[0].frame.size.height;          // origin screen
// window (AX top-left) → Cocoa bottom-left origin:
let win_cocoa_y = primary_h - (ax_y + ax_h);
// place panel just ABOVE the window's top edge:
let panel_origin = NSPoint {
    x: ax_x,                                            // + optional centering
    y: primary_h - ax_y - panel_h,                     // bottom edge sits on window top
};
```
Omitting the `panel_h` (or `ax_h`) term mislocates the bar by ~one bar height — the most likely placement bug, independent of DPI. `NSRect`/`CGRect` share layout, so `AXValueGetValue` fills a rect directly usable by AppKit. Optional cross-check: map an AX point through `CGDisplayBounds` to validate on a mixed-DPI rig.

### 8.3 Which-screen detection

Compute the window's Cocoa rect and pick the screen with **maximum frame-intersection area** (matches how macOS assigns window ownership; beats center-point containment for straddling windows). Clamp the bar's x/width to that screen. **Ultrawide needs nothing special** — a far window simply has a large positive x; the bar anchors there (clamped), which is the whole product goal.

### 8.4 Placement algorithm

```
on (focus change | AXWindowMoved/Resized | reconciliation tick):
  if !trusted(): hide; return
  win = focusedWindow(app); if none: hide; return
  if isFullscreen(win): hide; return                   // §5.4
  rect = axRect(win) → cocoaRect (§8.2)
  screen = ownerByMaxIntersection(rect) (§8.3)
  if isMaximized(rect, screen): hide; return           // §5.4 (frame ≈ visibleFrame/frame)
  barW = min(desiredMenuWidth, screen.visibleFrame.width)
  x = clamp(rect.minX, screen.frame.minX, screen.frame.maxX - barW)
  yTop = rect.maxY_cocoa                                // window top edge in Cocoa
  panelOrigin = (x, yTop)                               // bar bottom on window top
  if panelOrigin.y + barH > screen.visibleFrame.maxY:  // collides with menu-bar strip
      # keep position; level ≥101 draws OVER the menu bar (§6.3, §6.5)
  panel.setFrameOrigin(panelOrigin); panel.orderFront
```

---

## 9. Permissions / Packaging / Distribution

### 9.1 Accessibility (TCC)

- Gate all AX use behind **`AXIsProcessTrustedWithOptions(kAXTrustedCheckOptionPrompt=true)`** at startup (prompts to System Settings ▸ Privacy & Security ▸ Accessibility); poll **`AXIsProcessTrusted()`** to detect the grant; degrade gracefully (hide bar) when untrusted. Without it, AX calls return `kAXErrorAPIDisabled` and observers never fire.
- **Non-sandboxed** — no `com.apple.security.app-sandbox` (no sandbox entitlement grants the accessibility-client role).

### 9.2 The critical build constraint — stable signing for TCC persistence

An ad-hoc/unsigned build's **cdhash changes every rebuild**, so its stored `csreq` no longer matches and the grant silently resets (the yabai/skhd pain). Wire this in **from day one**:

- **Stable `CFBundleIdentifier`** + **reuse one stable code-signing identity** on every build. A **persistent self-signed cert is sufficient** for TCC persistence — a Developer ID cert + notarization is only for *distribution* to other Macs, not local use.
- Automate `codesign --sign <id> --deep --options runtime` after bundling (`--options runtime` only matters if you later notarize).
- During dev, `tccutil reset Accessibility <bundle-id>` clears stale entries.

### 9.3 Packaging

- Build a real **.app bundle** (client_type=0 in TCC) — **macOS 26 System Settings does not render path-based (bare-binary) TCC entries**, so a bundle is required for a usable toggle.
- `cargo-bundle` (`[package.metadata.bundle]`) maps `identifier`→`CFBundleIdentifier`, `version`, and injects `LSUIElement=1` + `LSMinimumSystemVersion` via `osx_info_plist_exts`. It does **not** code-sign — treat bundle + sign as two steps. Verify the resulting Info.plist actually contains `LSUIElement`, `CFBundleIdentifier`, `LSMinimumSystemVersion`.
- **No notarization** for local personal use (locally built .app has no quarantine bit; Gatekeeper won't block it).

---

## 10. Rust Binding Approach

- **One binding family:** madsmtm/objc2 (core 0.6.4; framework crates 0.3.2). UI via `objc2-app-kit`/`objc2-foundation`; AX C API via **`objc2-application-services` 0.3.2** (the change since issue #624 — the whole AX surface is now bound). This keeps a single `objc2-core-foundation` CF stack; **avoid** `accessibility-sys`/`axuielement` as deps (they use core-foundation-rs and would mix two CF stacks) — use them only as a reference for constant values.
- **Feature gating:** enable `AXUIElement`, `HIServices`, `AXError`, `AXValue`, `libc` on `objc2-application-services`; enable per-class features on `objc2-app-kit` (§ recommended_crates).
- **AX name constants are NOT re-exported** (they are `CFSTR` macros). Hand-declare a **single verified constants module** of `CFString`s. Exact values confirmed against the local CLT SDK headers: `"AXMenuBar"`, `"AXMenuBarItem"`, `"AXMenu"`, `"AXMenuItem"`, `"AXChildren"`, `"AXRole"`, `"AXTitle"`, `"AXEnabled"`, `"AXMenuItemMarkChar"`, `"AXMenuItemCmdChar"`, `"AXMenuItemCmdVirtualKey"`, `"AXMenuItemCmdGlyph"`, `"AXMenuItemCmdModifiers"`, `"AXPosition"`, `"AXSize"`, `"AXFocusedWindow"`, `"AXFocusedWindowChanged"`, `"AXPress"`, `"AXShowMenu"`, `"AXMenuOpened"`, `"AXMenuClosed"`, `"AXFullScreen"` (private). Unit-test that `AXUIElementCopyAttributeNames` on a live element contains them.
- **Subclassing/overrides** via `objc2::define_class!` with `#[thread_kind = MainThreadOnly]`: the app delegate, the `NSPanel` subclass (`canBecomeMainWindow`/`canBecomeKeyWindow`), and the item `NSView` subclass (`acceptsFirstMouse`, `mouseDown:`, target-action).
- **AX FFI ergonomics** are hand-rolled: wrap CFType retain/release with `CFRetained`, map `AXError`, and build the `AXObserverCallback` extern-"C" trampoline with a stable `Box`/`Retained` refcon. The generated free C functions are marked deprecated in favor of idiomatic `AXUIElement`/`AXObserver` methods — prefer the methods.
- **Isolate all raw AX FFI in one module** and convert `CFTypeRef` at the boundary; never pass objc2 types directly into AX calls.

---

## 11. Key Risks & Open Questions → De-risking Spikes

| # | Question (from verifiers) | Spike |
|---|---|---|
| S1 | Does behind-window blur (`NSGlassEffectView` / `NSVisualEffectView.behindWindow`) sample a **foreign app's live window** under a non-activating panel, and update live? | Minimal panel over a moving foreign window; observe blur update. If bad → `NSVisualEffectView` fallback. |
| S2 | On 26.5 (25F80), does a level-101 panel actually composite **over the Liquid Glass menu bar**, and do real dropdowns still overtake it? | Place bar at 25 vs 101 vs `CGShieldingWindowLevel()`; open a real system menu; screen-record. |
| S3 | Does `AXPress` on a **leaf** reliably fire without flash/focus-steal across native + Catalyst + Electron/Qt/Java? Does accessing `AXChildren` trigger `menuNeedsUpdate:` without a visible open on 26.5? | Instrument reads/presses across representative apps; log flash + enabled-state staleness. |
| S4 | Drag/programmatic-move fidelity: how chatty are `AXWindowMoved` during drags on 26.5; do Stage Manager/Spaces/tiling moves emit anything? | Measure event cadence; tune reconciliation timer; confirm hide-on-motion. |
| S5 | Does a **reused self-signed** identity keep the TCC grant across rebuilds on 26.5? | Grant → rebuild+re-sign same cert → confirm no re-prompt. |
| S6 | Full AX FFI ergonomics: read `AXMenuBar` + `AXPress` a leaf + wire an `AXObserver` callback with a Rust refcon end-to-end. | The Phase-0 walking-skeleton compile spike. |
| S7 | `dispatch2` main-queue helper existence. | Check docs.rs; else keep AX on main run loop. |

---

## 12. Phased Implementation Roadmap

**Phase 0 — Walking skeleton (compile spike, no UI polish).**
- Cargo project; `objc2` family deps + features; `NSApplication` (`.Accessory`) run loop; `AXIsProcessTrustedWithOptions` prompt.
- .app bundle + stable-identity `codesign` in the build (S5). Verify TCC toggle appears (bundle, not bare binary).
- MenuReader: read `AXMenuBar` → top-level titles; print them. `AXPress` a known leaf (e.g. a static Edit item) and confirm it fires in the target without activating Lintel (S6).
- Verified AX-constants module + `AXUIElementCopyAttributeNames` test.

**Phase 1 — Static bar, single display.**
- FocusTracker: `NSWorkspace` activation + per-app `AXObserver` (focused window, move/resize) on the main run loop.
- Geometry: AX→Cocoa flip (§8.2) with rect-height term; place a plain `NSPanel` (NonactivatingPanel) above the focused window on the main screen.
- Render eager static top-level + first-level items as clickable views; `acceptsFirstMouse` override; click → `AXPress` leaf.
- Styling spike S1 (glass vs visual-effect).

**Phase 2 — Interactivity depth + dynamic menus.**
- Submenu dropdowns as their own nonactivating panels at level 101; nested navigation.
- Lazy dynamic-submenu populate on hover via `AXShowMenu` + poll + dismiss (§4.4); `AXMenuOpened/Closed` refresh; per-app cache.
- Shortcut rendering (modifier-bitmask decode); separators; disabled/mark states.

**Phase 3 — Multi-display / ultrawide / collisions / fullscreen.**
- Which-screen by max intersection; clamp; ultrawide validation on the real rig.
- Over-the-menu-bar case at level ≥101 (S2); notch-safe layout for built-in displays.
- Fullscreen hide via `AXFullScreen` + `NSScreen.frame` fallback + Space-change (§5.4).
- Reconciliation timer + hide-on-motion (S4).

**Phase 4 — Robustness & polish.**
- Non-native/Catalyst feature-detection and graceful degrade (S3).
- Liquid Glass container grouping; tint/corner tuning; performance (batched multi-attribute reads, messaging timeouts, debounced observers).
- Permission revocation handling; teardown (remove run-loop sources/observers, no leaks).
