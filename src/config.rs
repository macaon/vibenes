//! Runtime configuration.
//!
//! Today this is a plain `Default`-backed struct, constructed in one
//! place (`App::new`) and threaded through the Nes build path. No disk
//! load, no TOML, no env-var parsing — deliberately minimal.
//!
//! **Future work** (when a settings UI lands): load from
//! `~/.config/vibenes/config.toml` via the XDG Base Directory
//! standard. The Rust-app convention is:
//!   - `dirs` or `directories` crate for `config_dir()`.
//!   - `serde` + `toml` for (de)serialization; field-level `#[serde(default)]`
//!     lets a partial user TOML fall through to these defaults.
//!   - `VIBENES_CONFIG_PATH` env var for override (test harness / CI).
//!   - Write a commented template on first run so users can discover
//!     what's tunable by opening the file (`starship` / `bat` pattern).
//!
//! Compile-time defaults live as `Default` impls below, so adding a
//! disk-loaded `Config::from_path` in the future doesn't refactor
//! call sites.

use std::path::PathBuf;

/// How battery-backed cartridge saves are named and located.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveStyle {
    /// `rompath.sav` next to the `.nes` file. The FCEUX / Nestopia /
    /// Mesen default — self-describing, travels with the ROM when
    /// copied between machines. Fallback is used when the ROM dir
    /// isn't writable (read-only mount, archive, etc.).
    NextToRom,
    /// `<save_dir>/<prg_chr_crc32>.sav`, keyed by CRC so renaming the
    /// ROM doesn't lose progress. Not the default but available as an
    /// alternative when the settings UI lands.
    ByCrc,
}

impl Default for SaveStyle {
    fn default() -> Self {
        Self::NextToRom
    }
}

/// Whole-app runtime configuration. Add fields as features need them;
/// `Default` values are what ships compiled in.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub save: SaveConfig,
}

/// Save-file-related settings. Factored out so a future settings UI
/// can hand users a single "Saves" tab without scraping fields out of
/// the top-level `Config`.
#[derive(Debug, Clone)]
pub struct SaveConfig {
    pub style: SaveStyle,
    /// Explicit directory for the `ByCrc` style, or the fallback when
    /// `NextToRom` can't write to the ROM's folder. `None` means use
    /// the platform default — XDG data dir on Linux
    /// (`~/.local/share/vibenes/saves/`), Application Support on macOS,
    /// `%APPDATA%\vibenes\saves\` on Windows. Not resolved until a save
    /// actually needs the fallback path, so the dirs crate isn't a
    /// build-time dependency yet.
    pub dir_override: Option<PathBuf>,
    /// How often to flush dirty battery RAM to disk during normal
    /// emulation, measured in emulator frames. 60 ≈ once per wall-clock
    /// second on NTSC. Writes are skipped when the mapper's dirty flag
    /// is clear, so the common case is a cheap no-op.
    pub autosave_every_n_frames: u32,
}

impl Default for SaveConfig {
    fn default() -> Self {
        Self {
            style: SaveStyle::default(),
            dir_override: None,
            autosave_every_n_frames: 60,
        }
    }
}
