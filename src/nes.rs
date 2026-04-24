// SPDX-License-Identifier: GPL-3.0-or-later
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::audio::AudioSink;
use crate::bus::Bus;
use crate::clock::Region;
use crate::config::SaveConfig;
use crate::cpu::Cpu;
use crate::mapper;
use crate::rom::Cartridge;
use crate::save;

pub struct Nes {
    pub cpu: Cpu,
    pub bus: Bus,
    /// Metadata needed to route save I/O. Populated by the app's ROM
    /// loader via [`Nes::attach_save_metadata`]; `None` keeps
    /// `save_battery` / `load_battery` no-ops which is the correct
    /// behavior for test harnesses that build a cart from raw bytes.
    save_meta: Option<SaveMeta>,
}

/// Where and how to persist the currently-loaded cart's battery RAM.
/// Captured at load time so we have a stable resolution even after a
/// ROM swap moves the `Cartridge` into the mapper (which consumed it).
#[derive(Debug, Clone)]
struct SaveMeta {
    rom_path: PathBuf,
    prg_chr_crc32: u32,
}

impl Nes {
    pub fn from_cartridge(cart: Cartridge) -> Result<Self> {
        let region = Region::from_tv_system(cart.tv_system);
        let mapper = mapper::build(cart)?;
        let bus = Bus::new(mapper, region);
        let mut nes = Self {
            cpu: Cpu::new(),
            bus,
            save_meta: None,
        };
        nes.cpu.reset(&mut nes.bus);
        Ok(nes)
    }

    /// Attach the metadata used by [`Nes::save_battery`] and
    /// [`Nes::load_battery`] to resolve a save path. Call this from
    /// the app's ROM loader right after `from_cartridge` /
    /// `swap_cartridge`. Test harnesses that build a `Cartridge` in-
    /// memory and never intend to persist it skip this — the save
    /// methods become no-ops.
    pub fn attach_save_metadata(&mut self, rom_path: impl Into<PathBuf>, prg_chr_crc32: u32) {
        self.save_meta = Some(SaveMeta {
            rom_path: rom_path.into(),
            prg_chr_crc32,
        });
    }

    /// Drop the save metadata attached by
    /// [`Nes::attach_save_metadata`]. Used during ROM swap so a
    /// failed flush for the outgoing cart can't route the new cart's
    /// RAM to the old path.
    pub fn clear_save_metadata(&mut self) {
        self.save_meta = None;
    }

    /// Read the save file (if any) for the currently-attached cart
    /// and hand it to the mapper. No-op when no metadata is attached
    /// or the mapper doesn't want save data (`save_data() == None`).
    /// Returns `Ok(true)` if bytes were loaded, `Ok(false)` if there
    /// was nothing to load, `Err` on I/O error.
    pub fn load_battery(&mut self, cfg: &SaveConfig) -> Result<bool> {
        let Some(meta) = self.save_meta.as_ref() else {
            return Ok(false);
        };
        if self.bus.mapper.save_data().is_none() {
            return Ok(false);
        }
        let Some(path) = save::save_path_for(&meta.rom_path, meta.prg_chr_crc32, cfg) else {
            return Ok(false);
        };
        match save::load(&path)? {
            None => Ok(false),
            Some(bytes) => {
                self.bus.mapper.load_save_data(&bytes);
                self.bus.mapper.mark_saved();
                Ok(true)
            }
        }
    }

    /// Write battery RAM to disk if the mapper has flagged it dirty.
    /// No-op on non-battery carts, when no metadata is attached, and
    /// when `save_dirty()` is false. Returns `Ok(true)` on a
    /// successful write, `Ok(false)` when there was nothing to do,
    /// `Err` on I/O error — the caller decides whether to surface it
    /// (usually just log and continue).
    pub fn save_battery(&mut self, cfg: &SaveConfig) -> Result<bool> {
        let Some(meta) = self.save_meta.as_ref() else {
            return Ok(false);
        };
        if !self.bus.mapper.save_dirty() {
            return Ok(false);
        }
        let Some(data) = self.bus.mapper.save_data() else {
            return Ok(false);
        };
        let Some(path) = save::save_path_for(&meta.rom_path, meta.prg_chr_crc32, cfg) else {
            return Ok(false);
        };
        // Copy the slice before touching the mapper mutably.
        let data = data.to_vec();
        save::write(&path, &data)?;
        self.bus.mapper.mark_saved();
        Ok(true)
    }

    /// Return the save-path that would be used for the current cart,
    /// or `None` if no metadata is attached. Useful for logging in
    /// the app layer.
    pub fn save_path(&self, cfg: &SaveConfig) -> Option<PathBuf> {
        let meta = self.save_meta.as_ref()?;
        save::save_path_for(&meta.rom_path, meta.prg_chr_crc32, cfg)
    }

    /// Resolve the FDS disk-save (`.ips`) path for the current cart,
    /// or `None` when no metadata is attached. Same routing rules as
    /// [`Nes::save_path`], just a different extension — a single cart
    /// can carry both a `.sav` (battery) and `.ips` (disk) file side
    /// by side.
    pub fn disk_save_path(&self, cfg: &SaveConfig) -> Option<PathBuf> {
        let meta = self.save_meta.as_ref()?;
        save::save_path_for_with_ext(
            &meta.rom_path,
            meta.prg_chr_crc32,
            cfg,
            save::DISK_SAVE_EXT,
        )
    }

    /// Load the FDS disk-save sidecar (`<stem>.ips`) and hand it to
    /// the mapper. No-op on non-FDS carts. Returns `Ok(true)` when a
    /// patch was applied, `Ok(false)` on "nothing to do," `Err` only
    /// for real I/O errors.
    pub fn load_disk(&mut self, cfg: &SaveConfig) -> Result<bool> {
        let Some(meta) = self.save_meta.as_ref() else {
            return Ok(false);
        };
        // Mapper declines (returns None) on non-FDS carts; skip I/O.
        if self.bus.mapper.disk_save_data().is_none() {
            return Ok(false);
        }
        let Some(path) = save::save_path_for_with_ext(
            &meta.rom_path,
            meta.prg_chr_crc32,
            cfg,
            save::DISK_SAVE_EXT,
        ) else {
            return Ok(false);
        };
        match save::load(&path)? {
            None => Ok(false),
            Some(bytes) => {
                self.bus.mapper.load_disk_save(&bytes);
                self.bus.mapper.mark_disk_saved();
                Ok(true)
            }
        }
    }

    /// Persist the FDS disk-save sidecar when the mapper's disk is
    /// dirty. No-op on non-FDS carts and when nothing has changed.
    /// Returns `Ok(true)` on a successful write, `Ok(false)` when
    /// there was nothing to do, `Err` on I/O error.
    pub fn save_disk(&mut self, cfg: &SaveConfig) -> Result<bool> {
        let Some(meta) = self.save_meta.as_ref() else {
            return Ok(false);
        };
        if !self.bus.mapper.disk_save_dirty() {
            return Ok(false);
        }
        let Some(bytes) = self.bus.mapper.disk_save_data() else {
            return Ok(false);
        };
        let Some(path) = save::save_path_for_with_ext(
            &meta.rom_path,
            meta.prg_chr_crc32,
            cfg,
            save::DISK_SAVE_EXT,
        ) else {
            return Ok(false);
        };
        save::write(&path, &bytes)?;
        self.bus.mapper.mark_disk_saved();
        Ok(true)
    }

    /// Currently-attached ROM path, for convenience in the app layer
    /// (e.g. to rebuild metadata after a cart swap).
    pub fn current_rom_path(&self) -> Option<&Path> {
        self.save_meta.as_ref().map(|m| m.rom_path.as_path())
    }

    pub fn region(&self) -> Region {
        self.bus.region()
    }

    /// Warm reset — the user pressing the Reset button on the console.
    /// RAM, PRG-RAM and cartridge state are preserved; the CPU reloads
    /// PC from the reset vector, the APU silences channels and keeps the
    /// DMC output level (so long samples don't pop), and the PPU resets
    /// its rendering state. This is the hook blargg's reset-protocol
    /// ($81 at $6000) relies on.
    pub fn reset(&mut self) {
        self.bus.apu.reset();
        self.bus.ppu.reset();
        self.bus.nmi_pending = false;
        self.bus.irq_line = false;
        self.cpu.reset(&mut self.bus);
    }

    pub fn step(&mut self) -> Result<(), String> {
        self.cpu.step(&mut self.bus)
    }

    pub fn run_cycles(&mut self, cycles: u64) -> Result<(), String> {
        let end = self.bus.clock.cpu_cycles() + cycles;
        while self.bus.clock.cpu_cycles() < end {
            self.step()?;
            if self.cpu.halted {
                break;
            }
        }
        Ok(())
    }

    /// Step until the PPU finishes a frame (or the CPU halts). Used by
    /// the GUI event loop to pace execution to the display: run one
    /// frame, upload to the GPU, present, repeat.
    pub fn step_until_frame(&mut self) -> Result<(), String> {
        let start_frame = self.bus.ppu.frame();
        while self.bus.ppu.frame() == start_frame {
            if self.cpu.halted {
                break;
            }
            self.step()?;
        }
        Ok(())
    }

    /// Attach a host audio sink. From this point on every CPU cycle's
    /// APU output is fed into the sink's band-limited resampler.
    pub fn attach_audio(&mut self, sink: AudioSink) {
        self.bus.audio_sink = Some(sink);
    }

    /// Replace the loaded cartridge with a new one and cold-reset the
    /// CPU. The already-attached `AudioSink` is preserved across the
    /// swap so the cpal stream keeps running without a reconnect; if
    /// the new ROM's TV system differs from the old one, the sink is
    /// re-tuned to the new CPU clock so pitch stays correct.
    pub fn swap_cartridge(&mut self, cart: Cartridge) -> Result<()> {
        let mut sink = self.bus.audio_sink.take();
        let region = Region::from_tv_system(cart.tv_system);
        if let Some(sink) = sink.as_mut() {
            sink.set_cpu_clock(region.cpu_clock_hz());
        }
        let mapper = mapper::build(cart)?;
        self.bus = Bus::new(mapper, region);
        self.bus.audio_sink = sink;
        self.cpu = Cpu::new();
        self.cpu.reset(&mut self.bus);
        // Caller re-attaches save metadata for the new cart. Clear
        // here so a caller that forgets gets a no-op rather than
        // routing the new cart's RAM to the old cart's path.
        self.save_meta = None;
        Ok(())
    }

    /// Flush pending resampler output into the ring. Call once per
    /// emulator frame from the GUI loop to bound audio latency;
    /// otherwise the sink flushes on its internal cycle threshold
    /// (~20 ms), which is fine but adds up to a frame of extra lag.
    pub fn end_audio_frame(&mut self) {
        if let Some(sink) = self.bus.audio_sink.as_mut() {
            sink.end_frame();
        }
    }

    /// FDS disk status for the overlay menu. `None` on non-FDS carts.
    /// Returns `(side_count, current_side_or_none)`.
    pub fn fds_info(&self) -> Option<FdsInfo> {
        let control = self.bus.mapper.as_fds()?;
        Some(FdsInfo {
            side_count: control.side_count(),
            current_side: control.current_side(),
        })
    }

    /// Eject the current FDS disk side. No-op on non-FDS carts.
    pub fn fds_eject(&mut self) {
        if let Some(fds) = self.bus.mapper.as_fds_mut() {
            fds.eject();
        }
    }

    /// Insert the given FDS disk side (0-indexed). No-op on non-FDS
    /// carts or when `side` is out of range.
    pub fn fds_insert(&mut self, side: u8) {
        if let Some(fds) = self.bus.mapper.as_fds_mut() {
            fds.insert(side);
        }
    }
}

/// Snapshot of an FDS drive's current state for the UI. Deliberately
/// a plain copy (not a borrow) so the overlay-menu render pass can
/// build items without holding a live reference into the mapper.
#[derive(Debug, Clone, Copy)]
pub struct FdsInfo {
    pub side_count: u8,
    /// Currently-inserted side (0-indexed). `None` when ejected.
    pub current_side: Option<u8>,
}
