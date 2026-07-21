//! Persistent user configuration (mirrors the pattern in ~/src/canopy's `config.rs`).
//!
//! `Config` is round-tripped to `~/Library/Application Support/Lintel/config.toml`.
//! Every field carries a serde default, so a missing key — or a whole missing file —
//! falls back to the compiled default; a corrupt file is logged and ignored, never
//! fatal. Saves are atomic (write a temp file, then rename over the target). The
//! settings UI edits a working copy and hands it back through a `write` closure; this
//! module is the only thing that touches disk.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A global-hotkey chord: Carbon modifier mask + virtual keycode. Default ⌘⇧M
/// (cmdKey 0x0100 | shiftKey 0x0200 = 0x0300; kVK_ANSI_M = 0x2E).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotkeyChord {
    pub mods: u32,
    pub keycode: u32,
}

impl Default for HotkeyChord {
    fn default() -> Self {
        HotkeyChord {
            mods: 0x0100 | 0x0200, // ⌘⇧
            keycode: 0x2E,         // M
        }
    }
}

fn default_palette_hotkey() -> HotkeyChord {
    HotkeyChord::default()
}

fn default_true() -> bool {
    true
}

/// App appearance: follow the system, force Dark / Light, or invert the system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Theme {
    System,
    Dark,
    Light,
    /// The inverse of the OS setting — Light while the system is Dark, Dark while it's Light —
    /// tracked live as the system appearance changes.
    OppositeSystem,
}

impl Default for Theme {
    fn default() -> Self {
        Theme::System
    }
}

fn default_fade_ms() -> u32 {
    200
}
fn default_settle_ms() -> u32 {
    180
}
fn default_poll_hz() -> u32 {
    60
}

/// User-configurable settings. Timings are milliseconds unless the name says otherwise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Show/hide fade duration (ms). 0 = instant.
    #[serde(default = "default_fade_ms")]
    pub fade_ms: u32,
    /// How long a window must be still after a move before the bar re-pins (ms).
    #[serde(default = "default_settle_ms")]
    pub settle_ms: u32,
    /// Window-follow reconciliation rate (Hz).
    #[serde(default = "default_poll_hz")]
    pub poll_hz: u32,
    /// Launch Lintel automatically at login (via `SMAppService`).
    #[serde(default)]
    pub launch_at_login: bool,
    /// Whether the command palette (menu type-ahead) is enabled (registers its hotkey).
    #[serde(default = "default_true")]
    pub palette_enabled: bool,
    /// Global hotkey that opens the command palette (menu type-ahead search).
    #[serde(default = "default_palette_hotkey")]
    pub palette_hotkey: HotkeyChord,
    /// App appearance (System / Dark / Light / Opposite System).
    #[serde(default)]
    pub theme: Theme,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            fade_ms: default_fade_ms(),
            settle_ms: default_settle_ms(),
            poll_hz: default_poll_hz(),
            launch_at_login: false,
            palette_enabled: true,
            palette_hotkey: HotkeyChord::default(),
            theme: Theme::System,
        }
    }
}

impl Config {
    /// Clamp fields to sane ranges so a hand-edited or corrupt file can't wedge the app
    /// (e.g. a 0 Hz poll rate that never ticks). The settings sliders use the same bounds.
    pub fn sanitized(mut self) -> Self {
        self.fade_ms = self.fade_ms.min(2000);
        self.settle_ms = self.settle_ms.min(2000);
        self.poll_hz = self.poll_hz.clamp(15, 120);
        self
    }

    /// Fade duration in seconds (as AppKit animations want it).
    pub fn fade_secs(&self) -> f64 {
        self.fade_ms as f64 / 1000.0
    }

    /// Reconciliation tick interval in seconds.
    pub fn tick_interval(&self) -> f64 {
        1.0 / self.poll_hz.max(1) as f64
    }
}

/// `~/Library/Application Support/Lintel/config.toml`.
pub fn default_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join("Library/Application Support/Lintel")
            .join("config.toml"),
    )
}

/// Load config from the default path; missing/corrupt → defaults. Always sanitized.
pub fn load() -> Config {
    match default_path() {
        Some(p) => load_from(&p),
        None => Config::default(),
    }
    .sanitized()
}

/// Load from an explicit path. Missing → default (silent). Present but unparseable →
/// default (warned). Testable without the real HOME.
pub fn load_from(path: &Path) -> Config {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Config::default(),
        Err(e) => {
            tracing::warn!("config read failed ({}); using defaults: {e}", path.display());
            return Config::default();
        }
    };
    match toml::from_str(&text) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::warn!("config parse failed ({}); using defaults: {e}", path.display());
            Config::default()
        }
    }
}

/// Persist `config` to the default path (atomic). Best-effort; errors are logged.
pub fn save(config: &Config) {
    let Some(path) = default_path() else {
        return;
    };
    if let Err(e) = save_to(&path, config) {
        tracing::warn!("config save failed: {e}");
    }
}

/// Persist to an explicit path with an atomic temp-write + rename so a crash mid-write
/// never leaves a truncated file. Testable without the real HOME.
pub fn save_to(path: &Path, config: &Config) -> std::io::Result<()> {
    let body = toml::to_string_pretty(config).map_err(std::io::Error::other)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_preserves_all_fields() {
        let cfg = Config {
            fade_ms: 321,
            settle_ms: 90,
            poll_hz: 45,
            launch_at_login: true,
            palette_enabled: false,
            palette_hotkey: HotkeyChord { mods: 0x0100, keycode: 0x23 },
            theme: Theme::Dark,
        };
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn missing_file_yields_default() {
        let path = std::env::temp_dir().join("lintel_cfg_missing_9f3a.toml");
        let _ = std::fs::remove_file(&path);
        assert_eq!(load_from(&path), Config::default());
    }

    #[test]
    fn corrupt_file_falls_back_to_default() {
        let path = std::env::temp_dir().join("lintel_cfg_corrupt_9f3a.toml");
        std::fs::write(&path, b"not = = valid toml").unwrap();
        assert_eq!(load_from(&path), Config::default());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn partial_toml_fills_defaults() {
        // Only one key present → the rest fall back to compiled defaults (not zeroed).
        let cfg: Config = toml::from_str("fade_ms = 50\n").unwrap();
        assert_eq!(cfg.fade_ms, 50);
        assert_eq!(cfg.settle_ms, Config::default().settle_ms);
        assert_eq!(cfg.poll_hz, Config::default().poll_hz);
    }

    #[test]
    fn sanitize_clamps_out_of_range() {
        let c = Config {
            fade_ms: 99_999,
            settle_ms: 99_999,
            poll_hz: 0,
            launch_at_login: false,
            palette_enabled: true,
            palette_hotkey: HotkeyChord::default(),
            theme: Theme::default(),
        }
        .sanitized();
        assert_eq!(c.fade_ms, 2000);
        assert_eq!(c.settle_ms, 2000);
        assert_eq!(c.poll_hz, 15);
    }
}
