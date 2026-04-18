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
}
