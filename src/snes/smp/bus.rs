// SPDX-License-Identifier: GPL-3.0-or-later
//! Bus surface seen by the SPC700 core. The SPC has a 16-bit address
//! space (ARAM + IPL shadow + I/O at `$F0-$FF`); the integrated bus
//! lands in Phase 5c as part of the host APU port bridge. For
//! sub-phase 5a we provide [`FlatSmpBus`], a 64 KiB linear memory
//! that charges 1 SMP cycle per access so unit tests can assert
//! cycle counts deterministically.
//!
//! References (paraphrased; clean-room-adjacent porting per project
//! policy): nes-expert SNES APU reference at
//! `~/.claude/skills/nes-expert/reference/snes-apu.md`,
//! Mesen2 `Core/SNES/Spc.h` for the bus surface shape (we land
//! `Read/Write/Idle` mirroring `SnesBus` rather than Mesen2's
//! `read/write` overloads).
//!
//! ## Bus model
//!
//! The SPC700 timing model is simpler than the 65C816's: every
//! instruction has a fixed cycle count documented in the official
//! Sony datasheet. Mesen2 (`Spc::Idle`, `Spc::Read`, `Spc::Write`)
//! charges one SMP cycle per `Read`/`Write` plus extra `Idle`
//! cycles for instructions whose published count exceeds their
//! memory-access count (e.g. CMP, MUL, DIV). We mirror that pattern.

/// Operations the SPC700 needs from whatever owns its 16-bit bus.
/// All methods advance the SMP clock by exactly one SMP cycle; the
/// instruction handlers in [`super::Smp`] sprinkle [`SmpBus::idle`]
/// calls as needed to make their total match the datasheet count.
pub trait SmpBus {
    /// Read one byte from `addr`. Advances the SMP clock by 1.
    fn read(&mut self, addr: u16) -> u8;

    /// Write one byte. Advances the SMP clock by 1.
    fn write(&mut self, addr: u16, value: u8);

    /// Internal cycle (no bus access). Charged for instructions
    /// whose datasheet cycle count exceeds their memory accesses.
    fn idle(&mut self);

    /// Total SMP cycles this bus has accumulated since construction.
    /// Used by tests to assert per-instruction cycle counts.
    fn cycles(&self) -> u64;
}

/// 64 KiB flat-memory bus. Every access charges exactly 1 SMP cycle.
/// Convenient for unit-testing the SPC700 core in isolation; the
/// integrated bus (with IPL shadow + I/O at `$F0-$FF` + DSP) lands
/// in Phases 5b-5c.
pub struct FlatSmpBus {
    pub ram: Vec<u8>,
    cycles: u64,
}

impl FlatSmpBus {
    pub const SIZE: usize = 1 << 16;

    pub fn new() -> Self {
        Self {
            ram: vec![0; Self::SIZE],
            cycles: 0,
        }
    }

    /// Direct memory poke that does NOT advance the clock. Use from
    /// tests to seed program code / data without skewing cycle
    /// counts.
    pub fn poke(&mut self, addr: u16, value: u8) {
        self.ram[addr as usize] = value;
    }

    pub fn poke_slice(&mut self, addr: u16, bytes: &[u8]) {
        let base = addr as usize;
        let end = (base + bytes.len()).min(Self::SIZE);
        self.ram[base..end].copy_from_slice(&bytes[..end - base]);
    }

    pub fn peek(&self, addr: u16) -> u8 {
        self.ram[addr as usize]
    }
}

impl Default for FlatSmpBus {
    fn default() -> Self {
        Self::new()
    }
}

impl SmpBus for FlatSmpBus {
    fn read(&mut self, addr: u16) -> u8 {
        self.cycles = self.cycles.wrapping_add(1);
        self.ram[addr as usize]
    }

    fn write(&mut self, addr: u16, value: u8) {
        self.cycles = self.cycles.wrapping_add(1);
        self.ram[addr as usize] = value;
    }

    fn idle(&mut self) {
        self.cycles = self.cycles.wrapping_add(1);
    }

    fn cycles(&self) -> u64 {
        self.cycles
    }
}

/// Integrated SMP bus: the real bus surface the SPC700 sees inside a
/// running SNES core. Wires up:
///
/// - 64 KiB ARAM (writable, also visible at every address that isn't
///   in the `$00F0-$00FF` I/O region or shadowed by the IPL).
/// - IPL ROM shadow at `$FFC0-$FFFF` while `CONTROL.7 == 1`. Writes to
///   that range still go through to ARAM; only reads see the ROM.
/// - `$00F0-$00FF` I/O register file:
///   - `$F1` CONTROL — IPL/timer enables, mailbox-clear bits.
///   - `$F2` DSP address latch.
///   - `$F3` DSP data port (passthrough stub for Phase 5b).
///   - `$F4-$F7` APU mailbox (reads `cpu_to_smp`, writes `smp_to_cpu`).
///   - `$FA-$FC` timer targets (write-only).
///   - `$FD-$FF` timer counters (read-and-clear, low 4 bits visible).
///   - `$F0` TEST and reserved bits behave as RAM stubs.
///   - `$F8/$F9` AUXIO are general-purpose RAM bytes per anomie /
///     the SPC700 hardware spec (no special register behind them);
///     we route reads and writes through the same ARAM slice that
///     backs $00..$FF, so DSP-written bytes are observable to the
///     SMP and vice versa. Mesen2 / higan model them with separate
///     register fields internally, but the observable behaviour is
///     identical in normal operation.
///
/// Lifetime model: the bus is constructed transiently per SMP step
/// from disjoint mutable fields of [`crate::snes::Snes`]. Each field
/// is borrowed exclusively, so the existence of the SMP bus blocks
/// the CPU bus statically - but only for the duration of one
/// `smp.step()` call, which is what we want.
pub struct IntegratedSmpBus<'a> {
    pub aram: &'a mut [u8],
    pub ipl: &'a [u8; super::ipl::IPL_SIZE],
    pub control: &'a mut super::state::SmpControl,
    pub timers: &'a mut super::state::SmpTimers,
    pub dsp: &'a mut super::state::DspRegs,
    pub ports: &'a mut super::state::ApuPorts,
    pub cycles: &'a mut u64,
}

impl<'a> IntegratedSmpBus<'a> {
    /// Tick the SMP master clock by one cycle. Bus reads/writes/idles
    /// each call this so the timers stay in sync with the cycle count
    /// the integrated SMP core charges itself.
    fn tick(&mut self) {
        *self.cycles = self.cycles.wrapping_add(1);
        self.timers.advance(1);
    }

    /// Resolve a read in the I/O register window `$00F0-$00FF`. Pulled
    /// out of `SmpBus::read` for testability.
    fn read_io(&mut self, addr: u16) -> u8 {
        match addr {
            // $F0 TEST: write-only on hw; reads observed as 0 in
            // Mesen2's stub (it has no privileged behaviour we model).
            0x00F0 => 0,
            // $F1 CONTROL: write-only on hw. Returning the last
            // written byte makes round-trip tests deterministic and
            // costs nothing at runtime - real SPC code doesn't read
            // CONTROL after writing it.
            0x00F1 => self.control.raw,
            0x00F2 => self.dsp.address,
            0x00F3 => self.dsp.read_data(),
            0x00F4..=0x00F7 => self.ports.smp_read((addr - 0x00F4) as usize),
            // $F8/$F9 AUXIO: plain ARAM bytes per the SPC700 spec.
            0x00F8 | 0x00F9 => self.aram[addr as usize],
            // $FA-$FC timer targets: write-only. Reads return open bus;
            // we yield 0 for determinism (matches Mesen2's `SpcTimer`).
            0x00FA..=0x00FC => 0,
            0x00FD => self.timers.read_counter(0),
            0x00FE => self.timers.read_counter(1),
            0x00FF => self.timers.read_counter(2),
            _ => unreachable!("read_io called outside $F0-$FF: {addr:04X}"),
        }
    }

    fn write_io(&mut self, addr: u16, value: u8) {
        match addr {
            0x00F0 => {
                // TEST register. Real hw uses this for factory test
                // signals; treating it as a no-op matches Mesen2's
                // `Spc::WriteRegister` for the same address.
            }
            0x00F1 => {
                self.control.raw = value;
                self.timers.apply_control(*self.control);
                // Bits 4/5 are write-1-to-clear for $2140-$2143 input
                // latches (the SMP-facing side of cpu_to_smp). Both
                // the visible latch AND any pending CPU write that
                // hasn't been committed yet must be zeroed - otherwise
                // a CPU write that arrived just before the clear
                // would re-surface on the next commit tick.
                if value & 0x10 != 0 {
                    self.ports.cpu_to_smp[0] = 0;
                    self.ports.cpu_to_smp[1] = 0;
                    self.ports.pending_cpu_to_smp[0] = 0;
                    self.ports.pending_cpu_to_smp[1] = 0;
                    self.ports.pending_dirty &= !0b0011;
                }
                if value & 0x20 != 0 {
                    self.ports.cpu_to_smp[2] = 0;
                    self.ports.cpu_to_smp[3] = 0;
                    self.ports.pending_cpu_to_smp[2] = 0;
                    self.ports.pending_cpu_to_smp[3] = 0;
                    self.ports.pending_dirty &= !0b1100;
                }
            }
            0x00F2 => self.dsp.address = value,
            0x00F3 => self.dsp.write_data(value),
            0x00F4..=0x00F7 => self.ports.smp_write((addr - 0x00F4) as usize, value),
            0x00F8 | 0x00F9 => self.aram[addr as usize] = value,
            0x00FA => self.timers.set_target(0, value),
            0x00FB => self.timers.set_target(1, value),
            0x00FC => self.timers.set_target(2, value),
            // Counter writes are ignored on real hardware.
            0x00FD..=0x00FF => {}
            _ => unreachable!("write_io called outside $F0-$FF: {addr:04X}"),
        }
    }
}

impl SmpBus for IntegratedSmpBus<'_> {
    fn read(&mut self, addr: u16) -> u8 {
        self.tick();
        match addr {
            0x00F0..=0x00FF => self.read_io(addr),
            0xFFC0..=0xFFFF if self.control.ipl_enabled() => {
                self.ipl[(addr - 0xFFC0) as usize]
            }
            _ => self.aram[addr as usize],
        }
    }

    fn write(&mut self, addr: u16, value: u8) {
        self.tick();
        match addr {
            0x00F0..=0x00FF => self.write_io(addr, value),
            // The IPL shadow is read-only - writes to $FFC0-$FFFF
            // pass through to ARAM regardless of CONTROL.7.
            _ => self.aram[addr as usize] = value,
        }
    }

    fn idle(&mut self) {
        self.tick();
    }

    fn cycles(&self) -> u64 {
        *self.cycles
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_and_write_advance_cycles_by_one() {
        let mut bus = FlatSmpBus::new();
        bus.poke(0x1234, 0x55);
        assert_eq!(bus.cycles(), 0);
        assert_eq!(bus.read(0x1234), 0x55);
        assert_eq!(bus.cycles(), 1);
        bus.write(0x1234, 0xAA);
        assert_eq!(bus.cycles(), 2);
        assert_eq!(bus.peek(0x1234), 0xAA);
    }

    #[test]
    fn idle_advances_cycles_without_touching_memory() {
        let mut bus = FlatSmpBus::new();
        bus.poke(0x0000, 0xFF);
        bus.idle();
        assert_eq!(bus.cycles(), 1);
        assert_eq!(bus.peek(0x0000), 0xFF);
    }

    // ----- IntegratedSmpBus -------------------------------------------

    use super::super::ipl::IPL_ROM;
    use super::super::state::{ApuPorts, DspRegs, SmpControl, SmpTimers};

    /// Build a fresh integrated bus over owned backing storage. Returns
    /// the storage tuple so callers can assert on it after the bus is
    /// dropped (Rust's borrow checker requires the bus to die before
    /// the storage is read again).
    struct IntegratedHarness {
        aram: Vec<u8>,
        ipl: [u8; super::super::ipl::IPL_SIZE],
        control: SmpControl,
        timers: SmpTimers,
        dsp: DspRegs,
        ports: ApuPorts,
        cycles: u64,
    }

    impl IntegratedHarness {
        fn new() -> Self {
            Self {
                aram: vec![0; 0x10000],
                ipl: IPL_ROM,
                control: SmpControl::RESET,
                timers: SmpTimers::default(),
                dsp: DspRegs::new(),
                ports: ApuPorts::RESET,
                cycles: 0,
            }
        }
        fn bus(&mut self) -> IntegratedSmpBus<'_> {
            IntegratedSmpBus {
                aram: &mut self.aram,
                ipl: &self.ipl,
                control: &mut self.control,
                timers: &mut self.timers,
                dsp: &mut self.dsp,
                ports: &mut self.ports,
                cycles: &mut self.cycles,
            }
        }
    }

    #[test]
    fn ipl_shadow_visible_when_control_bit_seven_set() {
        let mut h = IntegratedHarness::new();
        // Reset value of CONTROL has IPL enabled.
        let mut bus = h.bus();
        // First IPL byte is 0xCD (MOV X, #$EF). Reads at $FFC0 must
        // see that, regardless of what's in ARAM at the same address.
        let v = bus.read(0xFFC0);
        assert_eq!(v, 0xCD);
    }

    #[test]
    fn writes_to_ipl_range_pass_through_to_aram() {
        let mut h = IntegratedHarness::new();
        {
            let mut bus = h.bus();
            bus.write(0xFFC0, 0x42);
            // While IPL is active, the read still returns the ROM byte.
            assert_eq!(bus.read(0xFFC0), 0xCD);
        }
        // The byte is in ARAM; disabling IPL exposes it.
        h.control.raw = 0x00;
        let mut bus = h.bus();
        assert_eq!(bus.read(0xFFC0), 0x42);
    }

    #[test]
    fn aram_ordinary_address_round_trips() {
        let mut h = IntegratedHarness::new();
        let mut bus = h.bus();
        bus.write(0x0234, 0x99);
        assert_eq!(bus.read(0x0234), 0x99);
    }

    #[test]
    fn auxio_f8_f9_round_trip_as_plain_aram() {
        // Per the SPC700 spec ($F8/$F9 AUXIO are general-purpose RAM
        // bytes), an SMP write to $F8/$F9 must be observable to a
        // subsequent SMP read at the same address.
        let mut h = IntegratedHarness::new();
        let mut bus = h.bus();
        bus.write(0x00F8, 0x5A);
        bus.write(0x00F9, 0xA5);
        assert_eq!(bus.read(0x00F8), 0x5A);
        assert_eq!(bus.read(0x00F9), 0xA5);
    }

    #[test]
    fn auxio_f8_f9_alias_underlying_aram_for_dsp_visibility() {
        // The bytes live in the same ARAM slice the DSP sees, so a
        // direct ARAM mutation (modelling DSP DMA, debug poke, etc.)
        // is visible to the next SMP read.
        let mut h = IntegratedHarness::new();
        h.aram[0x00F8] = 0x77;
        h.aram[0x00F9] = 0x88;
        let mut bus = h.bus();
        assert_eq!(bus.read(0x00F8), 0x77);
        assert_eq!(bus.read(0x00F9), 0x88);
    }

    #[test]
    fn smp_reads_f4_to_f7_observe_cpu_to_smp_latches() {
        let mut h = IntegratedHarness::new();
        h.ports.cpu_to_smp = [0x11, 0x22, 0x33, 0x44];
        let mut bus = h.bus();
        assert_eq!(bus.read(0x00F4), 0x11);
        assert_eq!(bus.read(0x00F5), 0x22);
        assert_eq!(bus.read(0x00F6), 0x33);
        assert_eq!(bus.read(0x00F7), 0x44);
    }

    #[test]
    fn smp_writes_f4_to_f7_deposit_into_smp_to_cpu_latches() {
        let mut h = IntegratedHarness::new();
        {
            let mut bus = h.bus();
            bus.write(0x00F4, 0x55);
            bus.write(0x00F7, 0x88);
        }
        // CPU side picks up the writes via cpu_read.
        assert_eq!(h.ports.cpu_read(0), 0x55);
        assert_eq!(h.ports.cpu_read(3), 0x88);
        // The unaltered ports retain the boot signature.
        assert_eq!(h.ports.cpu_read(1), 0xBB);
    }

    #[test]
    fn control_bit_four_clears_first_two_input_latches() {
        let mut h = IntegratedHarness::new();
        h.ports.cpu_to_smp = [0x11, 0x22, 0x33, 0x44];
        let mut bus = h.bus();
        // Write CONTROL with bit 4 = 1, IPL still on.
        bus.write(0x00F1, 0x90);
        assert_eq!(h.ports.cpu_to_smp[0], 0);
        assert_eq!(h.ports.cpu_to_smp[1], 0);
        assert_eq!(h.ports.cpu_to_smp[2], 0x33, "$2142 untouched");
        assert_eq!(h.ports.cpu_to_smp[3], 0x44);
    }

    #[test]
    fn control_bit_five_clears_second_two_input_latches() {
        let mut h = IntegratedHarness::new();
        h.ports.cpu_to_smp = [0x11, 0x22, 0x33, 0x44];
        let mut bus = h.bus();
        bus.write(0x00F1, 0xA0);
        assert_eq!(h.ports.cpu_to_smp[0], 0x11);
        assert_eq!(h.ports.cpu_to_smp[1], 0x22);
        assert_eq!(h.ports.cpu_to_smp[2], 0);
        assert_eq!(h.ports.cpu_to_smp[3], 0);
    }

    #[test]
    fn dsp_address_then_data_round_trip() {
        let mut h = IntegratedHarness::new();
        let mut bus = h.bus();
        bus.write(0x00F2, 0x12);
        bus.write(0x00F3, 0x77);
        assert_eq!(bus.read(0x00F2), 0x12);
        assert_eq!(bus.read(0x00F3), 0x77);
    }

    #[test]
    fn timer_target_write_and_counter_advance() {
        let mut h = IntegratedHarness::new();
        // Enable timer 0 (CONTROL bit 0), keep IPL on.
        h.control.raw = 0x81;
        h.timers.apply_control(h.control);
        let mut bus = h.bus();
        bus.write(0x00FA, 4); // T0 target = 4 prescaler ticks
        // 1 SMP cycle is consumed by the write itself; we need 4*128
        // SMP cycles (= 512) for stage2 to tick, less the cycles
        // we've already spent. Issue 511 more idles.
        for _ in 0..511 {
            bus.idle();
        }
        // Counter visible at $FD; reading clears it.
        assert_eq!(bus.read(0x00FD), 1);
        assert_eq!(bus.read(0x00FD), 0);
    }

    #[test]
    fn ipl_first_instruction_runs_through_integrated_bus() {
        // End-to-end smoke: reset the SMP, point PC at $FFC0, and let
        // the dispatcher fetch + execute the first IPL byte through
        // the integrated bus. After `MOV X, #$EF` the X register
        // must hold $EF (the SPC stack-pointer setup).
        let mut h = IntegratedHarness::new();
        let mut smp = super::super::Smp::new();
        smp.pc = 0xFFC0; // IPL entry point
        let mut bus = h.bus();
        smp.step(&mut bus);
        assert_eq!(smp.x, 0xEF, "IPL byte 0 = MOV X, #$EF");
        assert_eq!(smp.pc, 0xFFC2);
    }

    #[test]
    fn timer2_runs_at_higher_rate_than_timer0() {
        let mut h = IntegratedHarness::new();
        h.control.raw = 0x84; // bit 2 = T2 enable
        h.timers.apply_control(h.control);
        h.timers.t2.target = 1;
        // T2 prescales every 16 SMP cycles; target 1 means one stage2
        // tick per 16 cycles. Run 32 cycles, expect 2 ticks.
        let mut bus = h.bus();
        for _ in 0..32 {
            bus.idle();
        }
        assert_eq!(bus.read(0x00FF), 2);
    }
}
