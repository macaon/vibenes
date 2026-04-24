// SPDX-License-Identifier: GPL-3.0-or-later
//! Commands produced by the UI layer and consumed by the host app.
//!
//! egui widgets must not mutate emulator state directly — they push
//! variants of this enum into a scratch `Vec` that the host drains
//! after the UI pass. This keeps the UI thread free of borrows on
//! `Nes` / `AudioStream` and gives the host a single dispatch seam
//! for every menu action.
//!
//! Extending the UI is a two-step process: add a variant here and a
//! matching arm in the host's dispatch. No other files need to change.

use std::path::PathBuf;

use crate::video::ParMode;

#[derive(Debug, Clone)]
pub enum UiCommand {
    /// Open the native file picker and, if the user selects a `.nes`
    /// file, swap the cartridge.
    OpenRomDialog,
    /// Swap the cartridge to this specific path. Pushed by the Recent
    /// ROMs submenu where the path is already known.
    OpenRom(PathBuf),
    /// Quit the application.
    Quit,
    /// Set the integer scale (1×–6×). Host clamps and resizes the
    /// window to the new content size.
    SetScale(u8),
    /// Set the pixel-aspect-ratio mode (Auto follows ROM region;
    /// Fixed pins to a specific PAR).
    SetAspectRatio(ParMode),
    /// Warm reset (the console's Reset button).
    Reset,
    /// FDS only: eject the currently-inserted disk side. No-op on
    /// non-FDS carts.
    FdsEject,
    /// FDS only: insert the specified 0-indexed disk side. If a disk
    /// is already loaded the mapper handles the eject+pause+reinsert
    /// dance automatically so games detect the swap edge.
    FdsInsert(u8),
}
