// SPDX-License-Identifier: GPL-3.0-or-later
//! Commands produced by the UI layer and consumed by the host app.
//!
//! egui widgets must not mutate emulator state directly - they push
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
    /// Debug: flip the scanline-ruler overlay (gridlines + numbered
    /// labels painted into the framebuffer for pixel-coord readout).
    ToggleScanlineRuler,
    /// Debug: dump the next N frames of OAM to stderr. Useful when
    /// chasing sprite issues; the burst covers 30 Hz sprite-flicker
    /// rotation so a single probe doesn't miss the "on" frame.
    DumpOamBurst(u8),
    /// Toggle the top menu bar's visibility. Persists to
    /// settings.kv. Window resizes to compensate so the NES
    /// viewport stays exactly `scale * 240` regardless.
    ToggleMenuBar,
    /// Toggle the application's fullscreen state. Menu bar is
    /// suppressed in fullscreen regardless of the user's
    /// `menu_bar_visible` preference; exiting fullscreen restores
    /// the prior visibility.
    ToggleFullscreen,
    /// Open the (forthcoming) preferences window. Stub for the
    /// moment - currently surfaces an info toast saying "settings
    /// UI coming soon" so the menu item is wired end-to-end.
    OpenPreferences,
    /// Open the project's GitHub URL in the user's default
    /// browser via xdg-open / open / start.
    OpenGithub,
    /// Show an info modal with the build version and a one-line
    /// project blurb. Drawn as a transient toast for now.
    ShowAbout,
    /// Load a RetroArch shader preset from disk and make it the
    /// active post-process chain. Replaces any currently-loaded
    /// shader. On parse / init failure the host shows an error
    /// toast and the previous shader (or passthrough) stays
    /// active.
    LoadShader(PathBuf),
    /// Drop the active shader and revert to the built-in
    /// passthrough blit.
    ClearShader,
    /// Re-walk the bundled and user shader directories. Picks up
    /// presets the user dropped into `$XDG_DATA_HOME/vibenes/shaders/`
    /// while the app was running.
    RescanShaders,
}
