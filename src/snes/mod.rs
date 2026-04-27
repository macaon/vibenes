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

/// SMP master clock divider relative to the SNES master clock. The
/// SPC700 runs at ~1.024 MHz vs. the SNES master at ~21.477 MHz, so
/// one SMP cycle takes ~20.97 master cycles. We use 21 as the integer
/// approximation - this is what bsnes does too, and it's close enough
/// that SPC test ROMs and commercial games sequence correctly. The
/// remaining drift is below the threshold where any known game shows
/// observable timing artefacts.
pub const SMP_MASTER_DIVIDER: u64 = 21;

/// SPC700 + ARAM + IPL + I/O register file. Owned at the [`Snes`]
/// level so the integrated SMP bus can borrow individual fields
/// disjointly from the CPU bus's mailbox latches when stepping.
///
/// The SMP clock advances independently of the CPU - the orchestrator
/// in [`Snes::step_instruction`] tracks how many master cycles the
/// CPU has consumed and runs SMP instructions until the SMP catches
/// up (within one instruction's worth of cycles).
pub struct ApuSubsystem {
    pub smp: smp::Smp,
    pub aram: Vec<u8>,
    pub ipl: smp::ipl::Ipl,
    pub control: smp::state::SmpControl,
    pub timers: smp::state::SmpTimers,
    pub dsp: smp::state::DspRegs,
    /// Total SMP cycles consumed since reset.
    pub cycles: u64,
}

impl ApuSubsystem {
    pub fn new(ipl: smp::ipl::Ipl) -> Self {
        let mut smp = smp::Smp::new();
        // Reset the SMP against a temporary flat bus seeded with the
        // IPL bytes at $FFC0-$FFFF so the reset vector at $FFFE/$FFFF
        // resolves to $FFC0 (the IPL entry point). The real running
        // bus is constructed transiently per SMP step from disjoint
        // fields of [`Snes`], so this seeding step is the only place
        // we materialise a standalone bus.
        let mut reset_bus = smp::bus::FlatSmpBus::new();
        reset_bus.poke_slice(0xFFC0, &ipl.bytes);
        smp.reset(&mut reset_bus);
        Self {
            smp,
            aram: vec![0; 0x10000],
            ipl,
            control: smp::state::SmpControl::RESET,
            timers: smp::state::SmpTimers::default(),
            dsp: smp::state::DspRegs::new(),
            cycles: 0,
        }
    }
}

/// Minimal SNES emulator. Phase 2d wires the 65C816 to a stub
/// LoROM bus so reset + boot prelude actually execute. Phase 5b lands
/// the SMP/SPC700 alongside, with the integrated bus, ARAM, IPL
/// shadow, mailbox latches, and timer/DSP register stubs all wired
/// into [`Snes::step_instruction`]. PPU/DMA still pending.
pub struct Snes {
    cart: rom::Cartridge,
    pub cpu: cpu::Cpu,
    pub bus: bus::LoRomBus,
    pub apu: ApuSubsystem,
    /// Total master clock cycles the CPU has consumed since reset.
    /// Drives the SMP catch-up scheduler in [`Snes::step_instruction`].
    pub master_cycles: u64,
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
        let apu = ApuSubsystem::new(smp::ipl::Ipl::embedded());
        Self {
            cart,
            cpu,
            bus,
            apu,
            master_cycles: 0,
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

    /// Execute one 65C816 instruction, then run the SMP forward
    /// until its cycle counter catches up to the new master-clock
    /// position. NMI/IRQ levels are forwarded from the bus into
    /// the CPU before the step so interrupts dispatch at the next
    /// instruction boundary.
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
        let cycles = self.cpu.step(&mut self.bus);
        // The CPU's per-instruction cycle count is in 5A22 cycles
        // (master / 6 fast or master / 8 slow). For the orchestrator
        // we treat it as master cycles - close enough that the SMP
        // tracks within ~6x of real time, which is plenty for SPC
        // ISA tests and commercial-game mailbox handshakes. Real
        // master-cycle accounting per access lands with the PPU.
        self.master_cycles = self.master_cycles.wrapping_add(cycles as u64);
        self.run_smp_to_master_cycles();
        cycles
    }

    /// Run SMP instructions until [`ApuSubsystem::cycles`] catches up
    /// with `master_cycles / SMP_MASTER_DIVIDER`. SMP instructions
    /// run in their own cycle granularity (2-12 SMP cycles each), so
    /// the SMP may overshoot the target slightly - those cycles are
    /// pre-paid for the next round, which is correct.
    pub fn run_smp_to_master_cycles(&mut self) {
        let target = self.master_cycles / SMP_MASTER_DIVIDER;
        // Cap the per-call work so a runaway CPU loop can't wedge us
        // in here forever. The cap is generous - far more than any
        // sensible per-instruction SMP debt.
        let mut budget = 4096usize;
        while self.apu.cycles < target && budget > 0 {
            self.step_smp_one_instruction();
            budget -= 1;
        }
    }

    /// Run exactly one SMP instruction against the integrated bus.
    /// Borrows disjoint mutable fields of `self` to construct the
    /// transient bus.
    pub fn step_smp_one_instruction(&mut self) {
        let mut spc_bus = smp::bus::IntegratedSmpBus {
            aram: &mut self.apu.aram,
            ipl: &self.apu.ipl.bytes,
            control: &mut self.apu.control,
            timers: &mut self.apu.timers,
            dsp: &mut self.apu.dsp,
            ports: &mut self.bus.apu_ports,
            cycles: &mut self.apu.cycles,
        };
        self.apu.smp.step(&mut spc_bus);
        // Commit any CPU-side mailbox writes that landed in the
        // pending shadow during this instruction. The just-finished
        // instruction read the pre-write value; the next one will
        // see the new byte.
        self.bus.apu_ports.commit_pending();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apu_subsystem_resets_to_ipl_entry_point() {
        let apu = ApuSubsystem::new(smp::ipl::Ipl::embedded());
        assert_eq!(apu.smp.pc, 0xFFC0, "reset vector points at IPL entry");
        assert_eq!(apu.smp.sp, 0xFF);
        assert_eq!(apu.cycles, 0);
    }

    #[test]
    fn apu_subsystem_executes_first_ipl_instruction() {
        // The first IPL byte is `MOV X, #$EF`. Stepping the SMP once
        // through a transient integrated bus must land $EF in X. This
        // is the same end-to-end path Snes::step_smp_one_instruction
        // takes - we just construct the bus inline so the test
        // doesn't need a Cartridge fixture.
        let mut apu = ApuSubsystem::new(smp::ipl::Ipl::embedded());
        let mut ports = smp::state::ApuPorts::RESET;
        let mut bus = smp::bus::IntegratedSmpBus {
            aram: &mut apu.aram,
            ipl: &apu.ipl.bytes,
            control: &mut apu.control,
            timers: &mut apu.timers,
            dsp: &mut apu.dsp,
            ports: &mut ports,
            cycles: &mut apu.cycles,
        };
        apu.smp.step(&mut bus);
        assert_eq!(apu.smp.x, 0xEF);
    }

    #[test]
    fn ipl_rom_writes_boot_signature_to_mailbox_unaided() {
        // End-to-end integration: with the mailbox cleared at the
        // start, run the SMP through the IPL boot sequence and verify
        // that it independently produces $AA at $F4 and $BB at $F5
        // (the two-byte handshake commercial games' WaitForAPUReady
        // loops spin on). This exercises:
        //   - IPL shadow read at $FFC0+ (CONTROL.7 = 1 at reset)
        //   - ARAM write/read (the IPL's clear loop touches $00-$EF)
        //   - branches (BNE in the clear loop)
        //   - DEC X, CMP, MOV-to-IO
        //   - the integrated bus's $F4-$F7 -> ApuPorts smp_write path
        // No host code, no Snes wrapper - just the SMP + integrated
        // bus eating its way through the IPL until the signature
        // bytes appear at the CPU-facing latches.
        let mut apu = ApuSubsystem::new(smp::ipl::Ipl::embedded());
        // Clear the boot signature out of the latches so we can
        // observe the SMP rewriting them.
        let mut ports = smp::state::ApuPorts {
            cpu_to_smp: [0; 4],
            smp_to_cpu: [0; 4],
            pending_cpu_to_smp: [0; 4],
            pending_dirty: 0,
        };
        let mut steps = 0u32;
        let max_steps = 50_000u32;
        let signature_emitted = loop {
            {
                let mut bus = smp::bus::IntegratedSmpBus {
                    aram: &mut apu.aram,
                    ipl: &apu.ipl.bytes,
                    control: &mut apu.control,
                    timers: &mut apu.timers,
                    dsp: &mut apu.dsp,
                    ports: &mut ports,
                    cycles: &mut apu.cycles,
                };
                apu.smp.step(&mut bus);
            }
            steps += 1;
            if ports.smp_to_cpu[0] == 0xAA && ports.smp_to_cpu[1] == 0xBB {
                break true;
            }
            if steps >= max_steps {
                break false;
            }
        };
        assert!(
            signature_emitted,
            "IPL did not produce $AA $BB at $F4/$F5 within {} SMP \
             instructions (took {} so far; PC=${:04X})",
            max_steps, steps, apu.smp.pc
        );
    }

    #[test]
    fn cpu_to_smp_mailbox_handoff_through_shared_apu_ports() {
        // CPU writes to the shared mailbox; after the per-instruction
        // commit fires, the SMP bus reads the new byte. Locks in that
        // LoRomBus and IntegratedSmpBus both route through the same
        // ApuPorts struct, plus the dual-latch delay model.
        let mut ports = smp::state::ApuPorts::RESET;
        // Simulate CPU side write to $2140 - lands in pending shadow.
        ports.cpu_write(0, 0x42);
        // Until commit, SMP still sees the old value.
        assert_eq!(ports.smp_read(0), 0x00, "pending hidden until commit");
        ports.commit_pending();
        assert_eq!(ports.smp_read(0), 0x42);
        // SMP writes to $F4 - CPU sees it on $2140 immediately.
        ports.smp_write(0, 0x99);
        assert_eq!(ports.cpu_read(0), 0x99);
        // The two halves are disjoint: writing the SMP side did not
        // touch what the SMP itself reads (still the CPU's $42).
        assert_eq!(ports.smp_read(0), 0x42);
    }
}
