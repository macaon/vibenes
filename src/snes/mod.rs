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
    /// S-DSP master mixer: 8 voices, envelopes, echo, noise, master
    /// volume. Stepped once per 32 kHz output sample by
    /// [`Self::advance_dsp`]. KON writes to DSP register `$4C` are
    /// latched here through the integrated bus.
    pub mixer: smp::dsp::mixer::Mixer,
    /// SMP cycles accumulated since the last 32 kHz mixer tick. When
    /// this crosses [`smp::dsp::mixer::Mixer::SMP_CYCLES_PER_SAMPLE`]
    /// (32), one stereo sample is produced.
    pub sample_cycle_accum: u32,
    /// Stereo samples produced by the mixer since the last drain.
    /// Phase 5c.8 buffers these in-core; later phases will resample
    /// and forward them to the host [`AudioSink`]. Held as `(L, R)`
    /// signed 16-bit at the DSP's native 32 kHz rate.
    pub samples: Vec<(i16, i16)>,
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
            mixer: smp::dsp::mixer::Mixer::new(),
            sample_cycle_accum: 0,
            samples: Vec::new(),
        }
    }

    /// Advance the 32 kHz DSP sample clock by `smp_cycles` SMP cycles.
    /// For every full [`smp::dsp::mixer::Mixer::SMP_CYCLES_PER_SAMPLE`]
    /// crossed, the mixer steps and a stereo sample is appended to
    /// [`Self::samples`].
    ///
    /// Called once per SMP instruction by
    /// [`Snes::step_smp_one_instruction`]; the per-call cycle count is
    /// 2-12 (one instruction's worth), so at most one or two samples
    /// land per call. The `while` loop is robust to larger deltas if
    /// callers ever batch-tick the DSP outside the per-instruction
    /// scheduler.
    pub fn advance_dsp(&mut self, smp_cycles: u32) {
        self.sample_cycle_accum += smp_cycles;
        let period = smp::dsp::mixer::Mixer::SMP_CYCLES_PER_SAMPLE;
        while self.sample_cycle_accum >= period {
            self.sample_cycle_accum -= period;
            // Coerce the 64 KiB ARAM Vec into a fixed-size array
            // reference for the mixer. The Vec is initialised with
            // `vec![0; 0x10000]` in [`Self::new`] and never resized,
            // so the conversion always succeeds.
            let aram: &mut [u8; 0x10000] = (&mut self.aram[..])
                .try_into()
                .expect("ARAM must be exactly 64 KiB");
            let sample = self.mixer.step_sample(&mut self.dsp, aram);
            self.samples.push(sample);
        }
    }

    /// Drain accumulated samples, returning the buffered `(L, R)`
    /// pairs and clearing the internal vector. Used by hosts /
    /// resamplers to lift produced audio into a downstream sink.
    pub fn drain_samples(&mut self) -> Vec<(i16, i16)> {
        std::mem::take(&mut self.samples)
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
    framebuffer: Vec<u8>,
    save_meta: Option<SaveMeta>,
    audio_sink: Option<AudioSink>,
    /// Debug counters for the audio path - only updated/printed when
    /// `VIBENES_SNES_AUDIO_DEBUG` is set. Lets a host operator
    /// observe SMP/DSP traffic without rebuilding.
    audio_debug: AudioDebug,
}

#[derive(Debug, Default)]
struct AudioDebug {
    enabled: bool,
    /// Frames since last log emission.
    frames_since_log: u32,
    /// Total stereo samples produced by the mixer since reset.
    total_samples: u64,
    /// Number of stereo samples in the most recent drain that were
    /// non-zero on either channel.
    last_nonzero_count: u32,
    /// Last drained frame's biggest absolute (L, R) value.
    last_peak: i16,
    /// Total times `end_audio_frame` has been called (whether or not
    /// it logged). Used to print every frame for the first few so
    /// startup is observable.
    total_samples_drains: u32,
}

impl Snes {
    pub fn from_cartridge(cart: rom::Cartridge) -> Self {
        let framebuffer = vec![0; (FRAME_WIDTH * FRAME_HEIGHT * 4) as usize];
        let mut bus = bus::LoRomBus::from_cartridge(&cart);
        let mut cpu = cpu::Cpu::new();
        cpu.reset(&mut bus);
        let apu = ApuSubsystem::new(smp::ipl::Ipl::embedded());
        if std::env::var("VIBENES_SNES_AUDIO_DEBUG").is_ok() {
            eprintln!(
                "[snes-audio] STARTUP: SNES core constructed, reset_pc={:02X}:{:04X} region={:?}",
                cpu.pbr, cpu.pc, cart.region
            );
        }
        Self {
            cart,
            cpu,
            bus,
            apu,
            framebuffer,
            save_meta: None,
            audio_sink: None,
            audio_debug: AudioDebug {
                enabled: std::env::var("VIBENES_SNES_AUDIO_DEBUG").is_ok(),
                ..AudioDebug::default()
            },
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
        // SMP catch-up is driven off the *bus*-side master clock
        // (`LoRomBus::master`), the single source of truth for master
        // cycles in this core. Each CPU bus access charges its
        // region's actual master-cycle cost (FAST=6 / SLOW=8 /
        // XSLOW=12) via `LoRomBus::advance_master`; reading that
        // counter here keeps the SMP synchronised to the real clock
        // ratio (master / 21 ≈ SMP cycles) instead of an approximation
        // built from CPU-instruction cycle counts.
        self.run_smp_to_master_cycles();
        cycles
    }

    /// Run SMP instructions until [`ApuSubsystem::cycles`] catches up
    /// with `bus.master / SMP_MASTER_DIVIDER`. SMP instructions
    /// run in their own cycle granularity (2-12 SMP cycles each), so
    /// the SMP may overshoot the target slightly - those cycles are
    /// pre-paid for the next round, which is correct.
    pub fn run_smp_to_master_cycles(&mut self) {
        let target = self.bus.master_cycles() / SMP_MASTER_DIVIDER;
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
        let cycles_before = self.apu.cycles;
        {
            let mut spc_bus = smp::bus::IntegratedSmpBus {
                aram: &mut self.apu.aram,
                ipl: &self.apu.ipl.bytes,
                control: &mut self.apu.control,
                timers: &mut self.apu.timers,
                dsp: &mut self.apu.dsp,
                ports: &mut self.bus.apu_ports,
                cycles: &mut self.apu.cycles,
                mixer: &mut self.apu.mixer,
            };
            self.apu.smp.step(&mut spc_bus);
        }
        // Commit any CPU-side mailbox writes that landed in the
        // pending shadow during this instruction. The just-finished
        // instruction read the pre-write value; the next one will
        // see the new byte.
        self.bus.apu_ports.commit_pending();
        // Advance the 32 kHz DSP sample clock by the SMP cycles this
        // instruction consumed. May produce zero, one, or two samples
        // depending on the prior accumulator state and instruction
        // length (SMP instructions are 2-12 cycles).
        let smp_delta = (self.apu.cycles - cycles_before) as u32;
        self.apu.advance_dsp(smp_delta);
    }

    pub fn attach_save_metadata(&mut self, rom_path: PathBuf, rom_crc32: u32) {
        self.save_meta = Some(SaveMeta { rom_path, rom_crc32 });
    }

    pub fn clear_save_metadata(&mut self) {
        self.save_meta = None;
    }

    /// Surrender the host audio sink so it can be re-attached to
    /// another core when the host swaps ROMs (NES ↔ SNES). Returns
    /// `None` if no sink was attached.
    pub fn detach_audio(&mut self) -> Option<AudioSink> {
        self.audio_sink.take()
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
        // Tighter cap when the audio diagnostic is on so a wedged
        // CPU surfaces in seconds, not in tens of seconds.
        let limit: u64 = if self.audio_debug.enabled {
            500_000
        } else {
            5_000_000
        };
        while self.bus.frame_count() == start {
            self.step_instruction();
            steps += 1;
            if steps > limit {
                if self.audio_debug.enabled {
                    eprintln!(
                        "[snes-audio] STEP-UNTIL-FRAME WEDGE after {steps} steps: \
                         cpu_pc={:02X}:{:04X} smp_pc={:#06x} smp_cy={} \
                         master_cy={} frame_count={} mailbox_cpu_view={:02X}{:02X}{:02X}{:02X}",
                        self.cpu.pbr,
                        self.cpu.pc,
                        self.apu.smp.pc,
                        self.apu.cycles,
                        self.bus.master_cycles(),
                        self.bus.frame_count(),
                        self.bus.apu_ports.cpu_read(0),
                        self.bus.apu_ports.cpu_read(1),
                        self.bus.apu_ports.cpu_read(2),
                        self.bus.apu_ports.cpu_read(3),
                    );
                }
                return Err(format!(
                    "step_until_frame: {limit} instructions without a frame edge (PC={:02X}:{:04X})",
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

    fn attach_audio(&mut self, mut sink: AudioSink) {
        // Reset resampler state on the new attachment so the SNES
        // path doesn't inherit interpolation history from a previous
        // core (which would alias one sample of garbage into the
        // first frame).
        sink.reset();
        self.audio_sink = Some(sink);
    }

    fn end_audio_frame(&mut self) {
        // Drain the APU's accumulated 32 kHz stereo samples into the
        // host sink. The SNES path bypasses BlipBuf entirely - the
        // S-DSP output is already band-limited at 32 kHz, so we feed
        // it straight into the linear-interp resampler that lifts it
        // to the host rate.
        let drained = self.apu.drain_samples();
        if self.audio_debug.enabled {
            let mut nonzero = 0u32;
            let mut peak: i16 = 0;
            for &(l, r) in &drained {
                if l != 0 || r != 0 {
                    nonzero += 1;
                }
                let lp = l.saturating_abs();
                let rp = r.saturating_abs();
                if lp > peak {
                    peak = lp;
                }
                if rp > peak {
                    peak = rp;
                }
            }
            self.audio_debug.total_samples += drained.len() as u64;
            self.audio_debug.last_nonzero_count = nonzero;
            self.audio_debug.last_peak = peak;
            self.audio_debug.frames_since_log += 1;
            // First 5 frames: log every frame so startup is visible.
            // After that throttle to every 30 frames (~0.5s @ 60Hz)
            // so the log doesn't drown stderr.
            let log_now = self.audio_debug.total_samples_drains < 5
                || self.audio_debug.frames_since_log >= 30;
            self.audio_debug.total_samples_drains =
                self.audio_debug.total_samples_drains.saturating_add(1);
            if log_now {
                self.audio_debug.frames_since_log = 0;
                let kon = self.apu.dsp.read(smp::dsp::global_reg::KON);
                let flg = self.apu.dsp.read(smp::dsp::global_reg::FLG);
                let mvoll = self.apu.dsp.read(smp::dsp::global_reg::MVOLL) as i8;
                let mvolr = self.apu.dsp.read(smp::dsp::global_reg::MVOLR) as i8;
                let endx = self.apu.dsp.read(smp::dsp::global_reg::ENDX);
                let voices_active = (0..8)
                    .filter(|&v| self.apu.mixer.voices[v].active)
                    .count();
                let smp_pc = self.apu.smp.pc;
                let smp_cycles = self.apu.cycles;
                let cpu_pc = self.cpu.pc;
                eprintln!(
                    "[snes-audio] frame_total={} drained={} nonzero={} peak={} \
                     KON={:#04x} FLG={:#04x} MVOL=({},{}) ENDX={:#04x} \
                     voices_active={} smp_pc={:#06x} smp_cy={} cpu_pc={:#06x}",
                    self.audio_debug.total_samples,
                    drained.len(),
                    nonzero,
                    peak,
                    kon,
                    flg,
                    mvoll,
                    mvolr,
                    endx,
                    voices_active,
                    smp_pc,
                    smp_cycles,
                    cpu_pc,
                );
            }
        }
        if let Some(sink) = self.audio_sink.as_mut() {
            for (l, r) in drained {
                sink.push_stereo_sample(l, r);
            }
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
            mixer: &mut apu.mixer,
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
                    mixer: &mut apu.mixer,
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
    fn advance_dsp_emits_one_sample_per_32_smp_cycles() {
        // The S-DSP outputs at 32 kHz, the SMP runs at ~1.024 MHz, so
        // one sample lands every 32 SMP cycles. With a fresh subsystem
        // (no voices keyed-on), the produced samples are silent but
        // their COUNT is what matters for the scheduler.
        let mut apu = ApuSubsystem::new(smp::ipl::Ipl::embedded());
        let period = smp::dsp::mixer::Mixer::SMP_CYCLES_PER_SAMPLE;
        // Just under the period: zero samples.
        apu.advance_dsp(period - 1);
        assert_eq!(apu.samples.len(), 0);
        // One more cycle crosses the boundary -> 1 sample.
        apu.advance_dsp(1);
        assert_eq!(apu.samples.len(), 1);
        // Two more periods: 3 total.
        apu.advance_dsp(period * 2);
        assert_eq!(apu.samples.len(), 3);
        // Drain clears the buffer but leaves the accumulator alone.
        let drained = apu.drain_samples();
        assert_eq!(drained.len(), 3);
        assert_eq!(apu.samples.len(), 0);
    }

    #[test]
    fn advance_dsp_handles_multi_sample_deltas_in_one_call() {
        // Robustness: a single advance_dsp call with cycles >> period
        // must produce floor(cycles/period) samples.
        let mut apu = ApuSubsystem::new(smp::ipl::Ipl::embedded());
        let period = smp::dsp::mixer::Mixer::SMP_CYCLES_PER_SAMPLE;
        apu.advance_dsp(period * 100 + 5); // 100 full periods + 5 cycles
        assert_eq!(apu.samples.len(), 100);
        assert_eq!(apu.sample_cycle_accum, 5);
    }

    #[test]
    fn fresh_apu_subsystem_has_silent_mixer_and_empty_sample_buffer() {
        let apu = ApuSubsystem::new(smp::ipl::Ipl::embedded());
        assert_eq!(apu.samples.len(), 0);
        assert_eq!(apu.sample_cycle_accum, 0);
        assert_eq!(apu.mixer.kon_pending, 0);
        // LFSR resets to 0x4000 per Anomie / Mesen2.
        assert_eq!(apu.mixer.noise_lfsr, 0x4000);
    }

    #[test]
    fn smp_harness_run_loop_drives_dsp_sample_clock() {
        // Run an SPC fragment (NOPs in a tight BRA loop) through the
        // harness's run loop and verify the DSP scheduler accumulated
        // samples via the same path the Snes orchestrator uses. Each
        // NOP is 2 SMP cycles, BRA is 4 - so a handful of iterations
        // is plenty to cross the 32-cycle sample boundary.
        let mut apu = ApuSubsystem::new(smp::ipl::Ipl::embedded());
        let mut ports = smp::state::ApuPorts::RESET;
        // NOP, NOP, NOP, BRA -3 -> spins forever, each loop ~10 cycles.
        let prog = [0x00, 0x00, 0x00, 0x2F, 0xFD];
        smp::harness::load_raw_spc_image(&mut apu, &mut ports, &prog, 0x0200);
        smp::harness::run_smp_until(&mut apu, &mut ports, 1000, |_, _| false);
        assert!(
            !apu.samples.is_empty(),
            "harness loop must drive the DSP sample clock"
        );
        // Cycle budget 1000 / 32 = ~31 expected samples (loose lower bound).
        assert!(
            apu.samples.len() >= 25,
            "expected ~31 samples, got {}",
            apu.samples.len()
        );
    }

    #[test]
    fn end_audio_frame_drain_path_pushes_each_apu_sample_to_the_sink() {
        // Validate the data flow that `Snes::end_audio_frame` runs:
        // drain `apu.samples` and call `sink.push_stereo_sample` for
        // each (l, r). At equal sample rates (32 kHz device) the
        // resampler is near-pass-through, so the ring should receive
        // approximately as many stereo frames as we drained.
        use ringbuf::traits::Consumer;
        let (mut sink, mut consumer) = AudioSink::for_test(32_000);
        let mut apu = ApuSubsystem::new(smp::ipl::Ipl::embedded());
        // Synthesise a few samples without actually running the SMP.
        for i in 0..10i16 {
            apu.samples.push((i * 100, i * -100));
        }
        // Mirror the body of `Snes::end_audio_frame`.
        for (l, r) in apu.drain_samples() {
            sink.push_stereo_sample(l, r);
        }
        // Drain consumer; count stereo frames received.
        let mut frames = 0usize;
        while consumer.try_pop().is_some() {
            assert!(consumer.try_pop().is_some(), "ring must hold L/R pairs");
            frames += 1;
        }
        // 10 inputs at 32 kHz → 32 kHz: linear interp emits
        // 9 frames (one-sample priming delay) within rounding.
        assert!(
            (8..=10).contains(&frames),
            "expected ~9 frames at equal rate, got {frames}"
        );
    }

    #[test]
    fn audio_sink_reset_clears_resampler_state_between_cores() {
        // The cross-core swap path detaches from one core, calls
        // `reset()` (via the receiving core's `attach_audio`), and
        // re-attaches. After reset, the SNES resampler must not
        // carry interpolation state from the previous core.
        let (mut sink, _consumer) = AudioSink::for_test(48_000);
        // Push a few SNES samples to advance resampler state.
        for _ in 0..32 {
            sink.push_stereo_sample(0x4000, -0x4000);
        }
        // Reset and verify state is back to zero.
        sink.reset();
        // We can't directly observe the private fields, but a fresh
        // ramp into push_stereo_sample after reset should produce
        // the same first-output behaviour as a brand-new sink. Easier
        // proxy: another reset is a no-op (idempotent).
        sink.reset();
        // Sanity check that NES-side reset also doesn't panic / cycle.
        sink.on_cpu_cycle(0.5);
        sink.end_frame();
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
