// SPDX-License-Identifier: GPL-3.0-or-later
//! SNES (Super Famicom) emulator core. Phase 1 lands the cartridge
//! loader, header detection, and a stub [`Snes`] that satisfies the
//! [`crate::core::Core`] trait so the host can dispatch to it - but
//! does not yet execute any 65C816 instructions. The stub presents
//! a black framebuffer at the canonical 256x224 resolution; later
//! phases will plug in CPU, PPU, SMP/DSP, and DMA.

pub mod bus;
pub mod cpu;
pub mod rom;
pub mod smp;

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::audio::AudioSink;
use crate::config::SaveConfig;
use crate::core::{Core, Region};
use crate::save;

/// Standard SNES output dimensions (NTSC, no overscan, no hi-res).
/// `Snes::framebuffer_dims` returns this so the host's render
/// pipeline can size its texture correctly. Hi-res/interlace land
/// later as a runtime-switchable dimension.
pub const FRAME_WIDTH: u32 = 256;
pub const FRAME_HEIGHT: u32 = 224;

/// Attached battery-RAM routing, populated at ROM-load time and
/// consulted by [`Snes::save_battery`] / [`Snes::load_battery`].
/// Same shape as the NES `SaveMeta` so the host treats both cores
/// uniformly.
#[derive(Debug, Clone)]
struct SaveMeta {
    rom_path: PathBuf,
    rom_crc32: u32,
}

/// Minimal SNES emulator. Phase 2d wires the 65C816 to a stub
/// LoROM bus so reset + boot prelude actually execute. PPU/APU/DMA
/// lands in Phases 3-5; until then [`Core::step_until_frame`]
/// only services CPU steps and the framebuffer stays black.
pub struct Snes {
    cart: rom::Cartridge,
    pub cpu: cpu::Cpu,
    pub bus: bus::LoRomBus,
    framebuffer: Vec<u8>,
    save_meta: Option<SaveMeta>,
    audio_sink: Option<AudioSink>,
}

impl Snes {
    pub fn from_cartridge(cart: rom::Cartridge) -> Self {
        let framebuffer = vec![0; (FRAME_WIDTH * FRAME_HEIGHT * 4) as usize];
        let mut bus = bus::LoRomBus::from_cartridge(&cart);
        let mut cpu = cpu::Cpu::new();
        cpu.reset(&mut bus);
        Self {
            cart,
            cpu,
            bus,
            framebuffer,
            save_meta: None,
            audio_sink: None,
        }
    }

    pub fn cartridge(&self) -> &rom::Cartridge {
        &self.cart
    }

    pub fn region(&self) -> Region {
        self.cart.region
    }

    /// Borrow the rendered 256x224 RGBA framebuffer (refreshed on
    /// every [`Core::step_until_frame`]).
    pub fn framebuffer_for_host(&self) -> &[u8] {
        &self.framebuffer
    }

    /// Execute one 65C816 instruction. After each step we forward
    /// the latched NMI/IRQ levels from the bus into the CPU; the
    /// CPU then dispatches the interrupt at the next instruction
    /// boundary.
    pub fn step_instruction(&mut self) -> u8 {
        if self.bus.take_nmi() {
            self.cpu.nmi_pending = true;
        }
        if self.bus.take_irq() {
            self.cpu.irq_pending = true;
        } else {
            // IRQ is level-triggered; the bus owns the line state.
            // Until a real timer source is wired in 3b, leave the
            // CPU's irq_pending where the bus put it so a clear
            // signal cancels a pending entry.
            self.cpu.irq_pending = false;
        }
        self.cpu.step(&mut self.bus)
    }

    pub fn attach_save_metadata(&mut self, rom_path: PathBuf, rom_crc32: u32) {
        self.save_meta = Some(SaveMeta { rom_path, rom_crc32 });
    }

    pub fn clear_save_metadata(&mut self) {
        self.save_meta = None;
    }
}

impl Core for Snes {
    fn step_until_frame(&mut self) -> Result<(), String> {
        // Run until the bus's frame counter advances by one. The
        // bus increments frame_count on every vblank entry, which
        // happens once per ~357k master cycles on NTSC. After the
        // frame completes, render the current PPU state into the
        // framebuffer so the host can present it.
        let start = self.bus.frame_count();
        let mut steps = 0u64;
        while self.bus.frame_count() == start {
            self.step_instruction();
            steps += 1;
            if steps > 5_000_000 {
                // Safety net: if a misbehaving ROM never enables
                // NMI / never reaches vblank, surface that to the
                // host instead of looping forever.
                return Err(format!(
                    "step_until_frame: 5M instructions without a frame edge (PC={:02X}:{:04X})",
                    self.cpu.pbr, self.cpu.pc
                ));
            }
        }
        self.bus.render_frame(&mut self.framebuffer);
        Ok(())
    }

    fn run_cycles(&mut self, _cycles: u64) -> Result<(), String> {
        Ok(())
    }

    fn reset(&mut self) {
        // Wipe the framebuffer back to black; nothing else to reset
        // until we have CPU/PPU/APU state.
        for b in self.framebuffer.iter_mut() {
            *b = 0;
        }
    }

    fn region(&self) -> Region {
        Snes::region(self)
    }

    fn framebuffer(&self) -> &[u8] {
        &self.framebuffer
    }

    fn framebuffer_dims(&self) -> (u32, u32) {
        (FRAME_WIDTH, FRAME_HEIGHT)
    }

    fn attach_audio(&mut self, sink: AudioSink) {
        self.audio_sink = Some(sink);
    }

    fn end_audio_frame(&mut self) {
        if let Some(sink) = self.audio_sink.as_mut() {
            sink.end_frame();
        }
    }

    fn attach_save_metadata(&mut self, rom_path: PathBuf, content_crc32: u32) {
        Snes::attach_save_metadata(self, rom_path, content_crc32);
    }

    fn clear_save_metadata(&mut self) {
        Snes::clear_save_metadata(self);
    }

    fn load_battery(&mut self, _cfg: &SaveConfig) -> Result<bool> {
        // Nothing to load until the cart's SRAM is wired into a bus.
        Ok(false)
    }

    fn save_battery(&mut self, _cfg: &SaveConfig) -> Result<bool> {
        Ok(false)
    }

    fn save_path(&self, cfg: &SaveConfig) -> Option<PathBuf> {
        let meta = self.save_meta.as_ref()?;
        save::save_path_for(&meta.rom_path, meta.rom_crc32, cfg)
    }

    fn current_rom_path(&self) -> Option<&Path> {
        self.save_meta.as_ref().map(|m| m.rom_path.as_path())
    }
}
