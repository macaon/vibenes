// SPDX-License-Identifier: GPL-3.0-or-later
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
    /// `$XDG_CONFIG_HOME/vibenes/saves/<rom-stem>.sav` (falls back
    /// to `$HOME/.config/vibenes/saves/<rom-stem>.sav` when
    /// `XDG_CONFIG_HOME` is unset). Default since 2026-04-23 —
    /// matches Mesen2's `~/.config/Mesen2/Saves/<rom-stem>.sav`
    /// convention and keeps ROM directories clean.
    ConfigDir,
    /// `rompath.sav` next to the `.nes` file. The FCEUX default —
    /// self-describing, travels with the ROM when copied between
    /// machines. Available for users who prefer it.
    NextToRom,
    /// `$XDG_CONFIG_HOME/vibenes/saves/<prg_chr_crc32>.sav`, keyed
    /// by CRC so renaming the ROM doesn't lose progress. Available
    /// as an alternative when the settings UI lands.
    ByCrc,
}

impl Default for SaveStyle {
    fn default() -> Self {
        Self::ConfigDir
    }
}

/// Whole-app runtime configuration. Add fields as features need them;
/// `Default` values are what ships compiled in.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub save: SaveConfig,
    pub fds: FdsConfig,
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
    /// Periodic safety-flush interval in emulator frames. The
    /// authoritative save triggers are ROM swap + app quit (same as
    /// Mesen2 / Nestopia); this interval is a belt-and-suspenders
    /// against crashes and SIGKILL. 10800 frames @ 60 Hz ≈ 3
    /// minutes, which mirrors puNES's `machine.fps * (60 * 3)` at
    /// `core/emu.c:642`.
    ///
    /// None of the three reference emulators (Mesen2, puNES,
    /// Nestopia) flush on every write — battery RAM is just normal
    /// SRAM on real hardware and the game has no "save commit"
    /// signal the emulator can latch on to. Emulators buffer writes
    /// and flush at session boundaries; `0` disables the periodic
    /// safety flush entirely and relies solely on quit/swap.
    pub autosave_every_n_frames: u32,
}

impl Default for SaveConfig {
    fn default() -> Self {
        Self {
            style: SaveStyle::default(),
            dir_override: None,
            // 3 minutes, matching puNES. Mesen2 and Nestopia flush
            // only on quit/swap; we go with puNES's safety-flush to
            // narrow the SIGKILL data-loss window without hammering
            // the disk.
            autosave_every_n_frames: 60 * 60 * 3,
        }
    }
}

/// Famicom Disk System configuration. All fields are `None` by
/// default; vibenes falls through to the `VIBENES_FDS_BIOS` env var
/// + XDG + ROM-directory search order defined in
/// [`crate::fds::bios`].
#[derive(Debug, Clone)]
pub struct FdsConfig {
    /// Absolute path to `disksys.rom`. Set here by a future settings
    /// UI; can be overridden per-run by the `--fds-bios` CLI flag or
    /// the `VIBENES_FDS_BIOS` environment variable.
    pub bios_path: Option<PathBuf>,
    /// Whether to auto-insert disk side 0 on power-on / reset.
    /// Default true (matches Mesen2's `FdsAutoLoadDisk`): users load
    /// a `.fds` file expecting the BIOS splash + title screen
    /// without having to manually reach for the disk-swap menu.
    pub auto_insert: bool,
}

impl Default for FdsConfig {
    fn default() -> Self {
        Self {
            bios_path: None,
            auto_insert: true,
        }
    }
}
