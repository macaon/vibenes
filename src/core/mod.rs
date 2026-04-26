// SPDX-License-Identifier: GPL-3.0-or-later
//! Cross-core abstractions. Defines the [`Core`] trait every console
//! emulator implements so the host (window/audio/input) doesn't need
//! to know whether it's driving the NES or the SNES.
//!
//! Phase 0 surface: just enough to switch `app.rs` to `Box<dyn Core>`
//! once the second core (SNES) lands. Until then, the trait is
//! implemented by [`crate::nes::Nes`] but the binaries still hold a
//! concrete `Nes` so the NES test sweep stays untouched.

pub mod system;

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::audio::AudioSink;
use crate::config::SaveConfig;

/// Display region. Universal across consoles - NTSC ≈ 60 Hz, PAL ≈
/// 50 Hz - so it lives at the cross-core layer. Per-console master
/// clock ratios stay inside each core's own timing module
/// (`nes::clock`, eventually `snes::clock`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    Ntsc,
    Pal,
}

impl Region {
    /// Nominal frame period in nanoseconds, used by the host loop to
    /// pace presentation. NTSC = master/12 ÷ 29780.5 cyc/frame ≈
    /// 16.639 ms; PAL = master/16 ÷ 33247.5 cyc/frame ≈ 19.997 ms.
    /// These values match the NES today and are within a few μs of
    /// the SNES (the host can re-tune per core if needed).
    pub const fn frame_period_ns(self) -> u64 {
        match self {
            Region::Ntsc => 16_639_267,
            Region::Pal => 19_997_194,
        }
    }
}

/// Common surface every console emulator presents to the host. The
/// trait deliberately stays narrow: stepping, framebuffer/audio I/O,
/// and battery-save plumbing. Console-specific operations (FDS disk
/// swap, SNES coprocessor state, etc.) stay on the concrete type.
pub trait Core {
    /// Run until the next visible frame completes (or the CPU halts).
    fn step_until_frame(&mut self) -> Result<(), String>;

    /// Run for at least `cycles` host CPU cycles. "Cycle" here means
    /// the console's CPU clock - 1.789 MHz for the NES, ~3.58 MHz at
    /// FastROM speed for the SNES. The host uses this for warm-up
    /// stepping during ROM swap, not for frame pacing.
    fn run_cycles(&mut self, cycles: u64) -> Result<(), String>;

    /// Warm reset (Reset button). RAM, save-RAM, and cartridge state
    /// preserved; CPU/PPU/APU re-init, vectors re-fetched.
    fn reset(&mut self);

    /// Display region (NTSC/PAL) of the currently-loaded ROM.
    fn region(&self) -> Region;

    /// Current framebuffer contents (RGBA8) and its dimensions.
    /// Returned as a slice borrowed from the core; the host must
    /// either upload it synchronously or copy it - the next
    /// `step_until_frame` call may overwrite the buffer.
    fn framebuffer(&self) -> &[u8];
    fn framebuffer_dims(&self) -> (u32, u32);

    /// Attach a host audio sink. The core feeds it samples until the
    /// sink is replaced or the core is dropped.
    fn attach_audio(&mut self, sink: AudioSink);

    /// Flush queued audio samples at the end of an emulator frame.
    /// The host calls this once per `step_until_frame` to bound
    /// audio latency.
    fn end_audio_frame(&mut self);

    /// Tell the core where its battery RAM lives on disk and what
    /// fingerprint (CRC32 over the loaded ROM payload) to file it
    /// under. Without this, [`Core::save_battery`] / [`Core::load_battery`]
    /// no-op - which is the right behavior for test harnesses that
    /// build a cart from raw bytes.
    fn attach_save_metadata(&mut self, rom_path: PathBuf, content_crc32: u32);
    fn clear_save_metadata(&mut self);

    fn load_battery(&mut self, cfg: &SaveConfig) -> Result<bool>;
    fn save_battery(&mut self, cfg: &SaveConfig) -> Result<bool>;
    fn save_path(&self, cfg: &SaveConfig) -> Option<PathBuf>;

    /// Currently-loaded ROM path, if save metadata is attached.
    fn current_rom_path(&self) -> Option<&Path>;
}
