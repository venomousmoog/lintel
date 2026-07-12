# Command Palette (menu type-ahead) — design

*2026-07-12*

A VSCode-⌘⇧P-style command palette for Lintel: a global hotkey opens a search field that
fuzzy-filters the **focused app's** menu commands and fires the selected one via `AXPress`.
New feature, lands as its own diff. Deferred/rejected (YAGNI): command history/frecency,
"recently used", actions beyond firing a menu item.

## Flow

1. Controller registers a global hotkey on `start()` (from `Config`, default **⌘⇧M**).
2. On press (main run loop): capture the **frontmost app's pid** *before* the palette
   activates Lintel, then open the palette for that pid.
3. The palette shows a search field; typing fuzzy-filters an index of that app's menu
   commands; ↑/↓ selects, Return fires (`AXPress`), Esc closes.

## Indexing — off the main thread, streaming

- The menu index is a recursive `AXMenuBar` walk producing a flat list of leaf commands,
  each: `path: ["Format","Font","Bold"]`, `enabled: bool`, `shortcut: Option<String>`
  (a display string like `⌘⇧B`). **Only `Send` data** — no `AXUIElement` handles cross the
  thread boundary (sidesteps ownership/`Send` questions and dynamic-menu staleness).
- The walk runs on a **background thread** (created from the pid) so the huge-menu case
  (Xcode) never blocks the UI. Bounded by a per-element AX messaging timeout.
- Shared `Arc<Mutex<IndexState { commands: Vec<Command>, done: bool }>>`. The walker pushes
  commands and sets `done`.
- While the palette is open, a short main-thread poll (~40 ms) re-reads the buffer and
  **re-runs the match as more commands arrive**, so results fill in live. Polling stops
  once `done`.
- **Spinner** (`NSProgressIndicator`): shown while `!done` **and** the current query has no
  matches yet — so an early keystroke isn't a false "no results". Clears when matches appear
  or the walk finishes.
- **Cache:** the completed index is cached per-app (keyed by pid, invalidated when the
  menu-title change we already detect fires), so re-opening is instant; a cold/incomplete
  cache just streams + spins.

## UI

- An **activating** `NSPanel` (key window, unlike the non-activating bar) so the text field
  receives typing — same activate-then-`makeKeyAndOrderFront` an accessory app uses (the
  settings window already does this). Acrylic `NSVisualEffectView`, ~600 pt wide.
- **Position:** x-centered on the focused window, anchored near its **top** edge, so the
  results list has the window height below to grow into.
- **Contents:** search `NSTextField` on top; results list below; a spinner.
- **Keyboard:** type → live re-filter; ↑/↓ move selection; Return fires; Esc closes.
  Arrows/Return/Esc are intercepted from the field via
  `control(_:textView:doCommandBySelector:)` (`moveUp:`/`moveDown:`/`insertNewline:`/
  `cancelOperation:`) so the field keeps focus while the list navigates. Clicking a row fires.
- Each row: menu path (dimmed parents, bold leaf) with the shortcut trailing; disabled items
  greyed and skipped by selection.

## Matching & firing

- **Fuzzy match:** VSCode-style **subsequence** (`k·e·y … k·e·y`, i.e. `key*key*key`) — typed
  chars matched in order with gaps allowed, over the leaf title (path as a secondary field),
  scored: contiguous runs, word-boundary starts, leaf-over-parent weight, shorter-path
  tiebreak. Sort desc, cap ~50. **Pure function → unit-tested.**
- **Firing:** no element handle crossed threads, so on Return we **re-resolve the leaf by its
  `path`** against the live menu tree (on the main thread) and `AXPress` it — which is also the
  robustness path for dynamic menus whose cached handles die on close. Submenus that only
  populate when opened aren't indexed cold (documented limitation of the chosen full-tree scope).

## Code

- `src/hotkey.rs` — Carbon `RegisterEventHotKey` RAII guard (adapted from canopy-mac), fires a
  main-run-loop callback. Self-contained; `(mods, keycode)` in, no `HotkeyChord` dependency.
- `src/palette.rs` — the panel + streaming index + fuzzy matcher + firing.
- `Config.palette_hotkey: HotkeyChord { mods, keycode }` (default ⌘⇧M); Advanced settings tab
  shows it (v1: label + toml-editable; live "record shortcut" capture is a follow-up).
- Controller: register the hotkey on `start()`; on press capture the frontmost pid and open
  the palette. Re-resolve-by-path helper reuses the existing AX walk.

## Risks

- Global hotkey shadows ⌘⇧M app-wide while Lintel runs — expected; rebindable.
- AX off the main thread: calls are made on a thread that creates its own `AXUIElement` from
  the pid; only owned `Send` data returns. Fire-time re-resolution happens back on main.
- Big menus may index partially under the AX timeout — acceptable (streamed + spinner).
