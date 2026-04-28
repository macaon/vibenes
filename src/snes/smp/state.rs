// SPDX-License-Identifier: GPL-3.0-or-later
//! SPC700 I/O register file. Bundles the SMP-side state that the
//! integrated bus (`super::bus::IntegratedSmpBus`) routes the
//! `$00F0-$00FF` accesses through:
//!
//! - [`SmpControl`] — `$F1` CONTROL byte (timer enables, IPL shadow
//!   enable, mailbox-clear bits).
//! - [`SmpTimers`] — three timer slots with a 4-bit visible counter;
//!   timer 0/1 prescale at 128 SMP cycles, timer 2 at 16.
//! - [`DspRegs`] — `$F2` address latch + `$F3` data port over a
//!   128-byte register file. Phase 5b ships this as a passthrough
//!   array; the actual DSP arithmetic lands in 5c.
//! - [`ApuPorts`] — bidirectional `$2140-$2143` ↔ `$F4-$F7` latches.
//!   On reset the SMP-side latches hold `$AA $BB $00 $00`, the IPL
//!   boot signature commercial games' `WaitForAPUReady` loop spins on.
//!
//! These types are owned at the `Snes` level (per the Phase 5b plan)
//! so both the CPU bus and the integrated SMP bus can borrow them as
//! disjoint mutable fields - the orchestrator interleaves CPU and SMP
//! steps but never runs them concurrently, so a single owner with
//! field-borrows is enough; no `Rc<RefCell<>>` needed.
//!
//! References (clean-room-adjacent, see `vendor/snes-ipl/README.md`
//! for the project's stance on how this differs from the IPL itself):
//! Anomie's SNES APU notes, Mesen2 `Core/SNES/Spc.cpp` for the
//! register layout / timing convention, bsnes-plus
//! `bsnes/snes/smp/iplrom.cpp` for the boot signature.

/// `$F1` CONTROL register. Write-only on real hardware; we stash the
/// last-written byte so reads (which are technically open-bus) at
/// least produce something deterministic for tests.
///
/// Bits, MSB to LSB:
///
/// | bit | meaning                                              |
/// |-----|------------------------------------------------------|
/// | 7   | IPL ROM shadow enable (`$FFC0-$FFFF`). 1 at reset.   |
/// | 6   | reserved                                             |
/// | 5   | Write-1-to-clear `$2142/$2143` input latch.          |
/// | 4   | Write-1-to-clear `$2140/$2141` input latch.          |
/// | 3   | reserved                                             |
/// | 2   | Timer 2 enable.                                      |
/// | 1   | Timer 1 enable.                                      |
/// | 0   | Timer 0 enable.                                      |
#[derive(Debug, Clone, Copy)]
pub struct SmpControl {
    pub raw: u8,
}

impl SmpControl {
    /// Reset value: IPL shadow on, timers off, no input clears.
    pub const RESET: Self = Self { raw: 0x80 };

    pub fn ipl_enabled(self) -> bool {
        self.raw & 0x80 != 0
    }

    pub fn timer_enabled(self, idx: usize) -> bool {
        debug_assert!(idx < 3);
        self.raw & (1 << idx) != 0
    }
}

impl Default for SmpControl {
    fn default() -> Self {
        Self::RESET
    }
}

/// One of the three SPC timers. The visible counter is 4 bits
/// (`stage2`); the prescaler stages live below.
///
/// Hardware layout, paraphrased from Anomie / Mesen2:
///
/// 1. A free-running prescaler ticks every N SMP cycles, where
///    N = 128 for T0/T1 and 16 for T2.
/// 2. Each prescaler tick increments `stage1`. When `stage1` reaches
///    the programmed `target`, it resets to 0 AND `stage2` increments
///    by 1 (modulo 16, since only the low nibble is visible).
/// 3. Reading the counter (`$FD-$FF`) returns `stage2 & 0x0F` and
///    clears `stage2` to 0.
/// 4. `target == 0` is interpreted as 256 (a full byte's worth of
///    prescaler ticks), per the SPC700 datasheet.
#[derive(Debug, Default, Clone, Copy)]
pub struct SmpTimer {
    pub target: u8,
    pub stage1: u16,
    pub stage2: u8,
    pub enabled: bool,
    /// Prescaler accumulator: counts SMP cycles since last stage1
    /// tick. Subtracts the per-timer divider when it crosses.
    pub cycle_accum: u32,
}

impl SmpTimer {
    pub fn target_period(self) -> u16 {
        if self.target == 0 {
            256
        } else {
            self.target as u16
        }
    }

    /// Advance this timer by `smp_cycles` SMP cycles. `divider` is
    /// the per-timer prescaler period (128 for T0/T1, 16 for T2).
    pub fn advance(&mut self, smp_cycles: u32, divider: u32) {
        if !self.enabled {
            return;
        }
        self.cycle_accum += smp_cycles;
        while self.cycle_accum >= divider {
            self.cycle_accum -= divider;
            self.stage1 += 1;
            if self.stage1 >= self.target_period() {
                self.stage1 = 0;
                self.stage2 = (self.stage2 + 1) & 0x0F;
            }
        }
    }

    /// Read-and-clear the visible counter. Reads always return only
    /// the low nibble; the act of reading clears `stage2` to 0.
    pub fn read_counter(&mut self) -> u8 {
        let v = self.stage2 & 0x0F;
        self.stage2 = 0;
        v
    }
}

/// Bundle of the three timers plus their fixed prescaler dividers.
#[derive(Debug, Default, Clone, Copy)]
pub struct SmpTimers {
    pub t0: SmpTimer,
    pub t1: SmpTimer,
    pub t2: SmpTimer,
}

impl SmpTimers {
    /// SMP cycles per stage1 tick for T0/T1. The SPC700 master clock
    /// runs at 1.024 MHz; the 8 kHz timers see one prescaler tick
    /// every 1.024e6 / 8e3 = 128 SMP cycles.
    pub const T01_DIVIDER: u32 = 128;
    /// SMP cycles per stage1 tick for T2 (64 kHz: 1.024e6 / 64e3 = 16).
    pub const T2_DIVIDER: u32 = 16;

    pub fn advance(&mut self, smp_cycles: u32) {
        self.t0.advance(smp_cycles, Self::T01_DIVIDER);
        self.t1.advance(smp_cycles, Self::T01_DIVIDER);
        self.t2.advance(smp_cycles, Self::T2_DIVIDER);
    }

    /// Apply a CONTROL byte: update each timer's `enabled` flag and
    /// reset its prescaler/counter on a 0->1 transition (per Anomie:
    /// "writing 1 to a timer enable bit clears its stages").
    pub fn apply_control(&mut self, control: SmpControl) {
        for (i, t) in [&mut self.t0, &mut self.t1, &mut self.t2]
            .into_iter()
            .enumerate()
        {
            let enable = control.timer_enabled(i);
            if enable && !t.enabled {
                t.stage1 = 0;
                t.stage2 = 0;
                t.cycle_accum = 0;
            }
            t.enabled = enable;
        }
    }

    pub fn set_target(&mut self, idx: usize, value: u8) {
        match idx {
            0 => self.t0.target = value,
            1 => self.t1.target = value,
            2 => self.t2.target = value,
            _ => debug_assert!(false, "timer idx out of range: {idx}"),
        }
    }

    pub fn read_counter(&mut self, idx: usize) -> u8 {
        match idx {
            0 => self.t0.read_counter(),
            1 => self.t1.read_counter(),
            2 => self.t2.read_counter(),
            _ => 0,
        }
    }
}

// `DspRegs` lives in [`super::dsp`] now - moved out of `state` because
// the DSP grew its own substantial submodules (BRR, voice runtime,
// envelope). Re-exported for callers that imported it from here so
// existing call sites keep working.
pub use super::dsp::DspRegs;

/// Bidirectional 4-byte latches between the host 5A22 (CPU) and the
/// SPC700 (SMP). Same bytes show up at `$2140-$2143` on the CPU side
/// and `$F4-$F7` on the SMP side, but each direction has its own
/// register so a CPU read and an SMP read don't see the same value
/// unless they're explicitly synchronised through a write on the
/// other side first.
///
/// Reset state: the SMP-side latch holds the IPL boot signature
/// `$AA $BB $00 $00`. Commercial games spin on `$2140` waiting for
/// `$AA` and `$2141` for `$BB` as their "APU is alive" handshake;
/// without those bytes the boot loop hangs. The CPU-side latch
/// resets to all zeros - the SMP cannot observe anything before the
/// host has written.
#[derive(Debug, Clone, Copy)]
pub struct ApuPorts {
    /// What the CPU last wrote; the SMP reads this on `$F4-$F7`.
    pub cpu_to_smp: [u8; 4],
    /// What the SMP last wrote; the CPU reads this on `$2140-$2143`.
    pub smp_to_cpu: [u8; 4],
    /// Shadow latch for fresh CPU writes. Hides the new byte from the
    /// SMP until the next [`Self::commit_pending`] tick, modelling the
    /// sub-cycle latency that real hardware exhibits between a 5A22
    /// store to `$2140-$2143` and the SMP observing it on `$F4-$F7`.
    /// Without this delay, Kishin Douji Zenki and Kawasaki Superbike
    /// Challenge can race their boot handshake and freeze.
    pub pending_cpu_to_smp: [u8; 4],
    /// Bitmask of `pending_cpu_to_smp` slots that have been written
    /// since the last commit. Bit `n` set means slot `n` needs to be
    /// copied into `cpu_to_smp` on the next commit.
    pub pending_dirty: u8,
}

impl ApuPorts {
    /// Reset state: SMP side carries the IPL handshake; CPU side is
    /// zero (the host hasn't written anything yet).
    pub const RESET: Self = Self {
        cpu_to_smp: [0; 4],
        smp_to_cpu: [0xAA, 0xBB, 0x00, 0x00],
        pending_cpu_to_smp: [0; 4],
        pending_dirty: 0,
    };

    /// CPU writes `value` to `$2140 + idx`. Visible to the SMP on
    /// its next read of `$F4 + idx`.
    ///
    /// **Earlier model (5b.x)** routed writes through a one-SMP-
    /// instruction-delayed `pending_cpu_to_smp` shadow on the theory
    /// that hiding fresh writes from the in-flight SMP read would
    /// fix a boot race in Kishin Douji Zenki / Kawasaki Superbike.
    /// In practice it broke the IPL block-upload protocol for SMW
    /// (and presumably every other game that uses the standard
    /// upload path): a 1-instruction lag desynchronises the
    /// counter↔echo handshake just enough that the SMP misorders
    /// bytes in the uploaded driver, eventually following a
    /// corrupted `JMP [$0000+X]` out of IPL ROM into ARAM garbage.
    /// Real hardware has no such delay - the mailbox is a single
    /// register the SMP reads at the access cycle, not a queue.
    /// The Zenki/Kawasaki race needs to be re-investigated against
    /// proper master-cycle scheduling, not papered over here.
    pub fn cpu_write(&mut self, idx: usize, value: u8) {
        debug_assert!(idx < 4);
        self.cpu_to_smp[idx] = value;
        // `pending_*` fields are kept on the struct for ABI stability
        // with any external state-snapshot code, but no longer drive
        // visibility. They stay zero so `pending_dirty` still works as
        // a "no-op writes pending" indicator.
        self.pending_cpu_to_smp[idx] = value;
        self.pending_dirty = 0;
    }

    /// Retained for callers that explicitly synchronise mailbox
    /// state at instruction boundaries; with the dual-latch removed
    /// this is a no-op.
    pub fn commit_pending(&mut self) {
        // No-op: writes are now immediately visible. See `cpu_write`
        // doc for the rationale.
    }

    /// CPU reads `$2140 + idx`. Returns whatever the SMP last wrote.
    pub fn cpu_read(&self, idx: usize) -> u8 {
        debug_assert!(idx < 4);
        self.smp_to_cpu[idx]
    }

    /// SMP writes `value` to `$F4 + idx`. Updates the latch the CPU
    /// will see on its next read.
    pub fn smp_write(&mut self, idx: usize, value: u8) {
        debug_assert!(idx < 4);
        self.smp_to_cpu[idx] = value;
    }

    /// SMP reads `$F4 + idx`. Returns whatever the CPU last wrote
    /// (committed value; pending writes are not yet visible).
    pub fn smp_read(&self, idx: usize) -> u8 {
        debug_assert!(idx < 4);
        self.cpu_to_smp[idx]
    }
}

impl Default for ApuPorts {
    fn default() -> Self {
        Self::RESET
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_reset_has_ipl_enabled_timers_disabled() {
        let c = SmpControl::RESET;
        assert!(c.ipl_enabled());
        assert!(!c.timer_enabled(0));
        assert!(!c.timer_enabled(1));
        assert!(!c.timer_enabled(2));
    }

    #[test]
    fn timer_zero_target_means_period_256() {
        let t = SmpTimer {
            target: 0,
            ..Default::default()
        };
        assert_eq!(t.target_period(), 256);
    }

    #[test]
    fn timer_advance_disabled_does_nothing() {
        let mut t = SmpTimer {
            target: 4,
            enabled: false,
            ..Default::default()
        };
        t.advance(10_000, 128);
        assert_eq!(t.stage1, 0);
        assert_eq!(t.stage2, 0);
    }

    #[test]
    fn timer_advance_increments_stage2_when_target_hit() {
        let mut t = SmpTimer {
            target: 4,
            enabled: true,
            ..Default::default()
        };
        // Divider 128, target 4 -> stage2 ticks every 4*128 = 512
        // SMP cycles. Run 512 cycles, expect stage2 = 1.
        t.advance(512, 128);
        assert_eq!(t.stage2, 1);
        assert_eq!(t.stage1, 0);
    }

    #[test]
    fn timer_stage2_wraps_at_16() {
        let mut t = SmpTimer {
            target: 1,
            enabled: true,
            stage2: 0x0F,
            ..Default::default()
        };
        // Divider 128, target 1 -> stage2 ticks every 128 cycles.
        t.advance(128, 128);
        assert_eq!(t.stage2, 0x00, "low nibble wraps");
    }

    #[test]
    fn timer_read_counter_clears_stage2() {
        let mut t = SmpTimer {
            stage2: 0x0A,
            ..Default::default()
        };
        assert_eq!(t.read_counter(), 0x0A);
        assert_eq!(t.read_counter(), 0x00, "second read sees the clear");
    }

    #[test]
    fn timers_apply_control_resets_on_rising_enable_edge() {
        let mut ts = SmpTimers::default();
        ts.t0.stage1 = 5;
        ts.t0.stage2 = 0x0A;
        ts.t0.cycle_accum = 50;
        // Enable T0
        ts.apply_control(SmpControl { raw: 0x81 });
        assert!(ts.t0.enabled);
        assert_eq!(ts.t0.stage1, 0, "stage1 cleared on enable edge");
        assert_eq!(ts.t0.stage2, 0);
        assert_eq!(ts.t0.cycle_accum, 0);
    }

    #[test]
    fn timers_apply_control_does_not_reset_already_enabled_timer() {
        let mut ts = SmpTimers::default();
        ts.apply_control(SmpControl { raw: 0x81 }); // T0 enable
        ts.t0.stage2 = 0x0A;
        ts.apply_control(SmpControl { raw: 0x81 }); // still enabled
        assert_eq!(ts.t0.stage2, 0x0A, "no reset on no-op");
    }

    // DspRegs tests moved to [`super::super::dsp`] alongside the
    // expanded register layout - re-export keeps `state::DspRegs`
    // valid as a path but the canonical home is `dsp.rs`.

    #[test]
    fn apu_ports_reset_holds_boot_signature() {
        let p = ApuPorts::RESET;
        assert_eq!(p.smp_to_cpu, [0xAA, 0xBB, 0x00, 0x00]);
        assert_eq!(p.cpu_to_smp, [0; 4]);
    }

    #[test]
    fn apu_ports_round_trip_each_direction_independently() {
        let mut p = ApuPorts::RESET;
        // CPU -> SMP: writes are immediately visible to the SMP, no
        // commit step. Real hardware exhibits no buffering on this
        // path - the mailbox is a single shared register.
        p.cpu_write(0, 0x11);
        p.cpu_write(3, 0x44);
        assert_eq!(p.smp_read(0), 0x11);
        assert_eq!(p.smp_read(3), 0x44);
        // SMP -> CPU (overrides reset boot signature)
        p.smp_write(0, 0x99);
        assert_eq!(p.cpu_read(0), 0x99);
        // The two directions are independent: CPU's read of $2140
        // sees what SMP wrote, NOT what CPU wrote to $2140.
        assert_ne!(p.cpu_read(0), p.smp_read(0));
    }

    #[test]
    fn apu_ports_cpu_write_immediately_visible_to_smp() {
        // The previous build hid CPU writes behind a one-instruction
        // pending-shadow latch. That broke the standard SPC IPL
        // block-upload protocol (SMW etc.) by desynchronising the
        // counter↔echo handshake. Real hardware exposes the new byte
        // on the very next SMP read.
        let mut p = ApuPorts::RESET;
        p.cpu_write(1, 0x55);
        assert_eq!(p.smp_read(1), 0x55, "byte must be visible immediately");
    }

    #[test]
    fn apu_ports_commit_pending_is_now_a_noop() {
        // Retained as a no-op so any external state-snapshot code
        // doesn't break, but it should make zero observable change
        // when called.
        let mut p = ApuPorts::RESET;
        p.cpu_write(2, 0xFF);
        let before = p.cpu_to_smp;
        p.commit_pending();
        assert_eq!(p.cpu_to_smp, before, "commit_pending no longer promotes anything");
    }

    #[test]
    fn apu_ports_repeated_cpu_writes_keep_only_the_latest() {
        // Multiple CPU writes between any two SMP reads leave only
        // the most recent value visible.
        let mut p = ApuPorts::RESET;
        p.cpu_write(0, 0xAA);
        p.cpu_write(0, 0xBB);
        p.cpu_write(0, 0xCC);
        assert_eq!(p.smp_read(0), 0xCC);
    }
}
