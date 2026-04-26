// SPDX-License-Identifier: GPL-3.0-or-later
//! 2A03 APU - five channels plus a frame counter driving per-channel
//! timing events. Ticked once per CPU cycle by the bus.
//!
//! References: NES dev wiki APU pages, Mesen2 `Core/NES/APU/*`, puNES
//! `src/core/apu.*`. See also `reference/apu.md` and `reference/*-notes.md`
//! in the nes-expert skill.

use crate::nes::clock::Region;

mod dmc;
mod envelope;
mod frame_counter;
mod length;
mod noise;
mod pulse;
mod sweep;
mod triangle;

use self::dmc::Dmc;
use self::frame_counter::FrameCounter;
use self::noise::Noise;
use self::pulse::Pulse;
use self::triangle::Triangle;

/// Signals raised by the APU that the rest of the system must observe.
#[derive(Debug, Default, Clone, Copy)]
pub struct ApuOutputs {
    pub irq_line: bool,
}

/// A pending DMC DMA fetch. The bus consumes these one at a time,
/// inserting the appropriate CPU stall before handing back the byte via
/// [`Apu::dmc_dma_complete`].
#[derive(Debug, Clone, Copy)]
pub struct DmcDmaRequest {
    pub addr: u16,
}

#[derive(Debug)]
pub struct Apu {
    region: Region,
    /// Absolute CPU cycle count since power-on. Used for `$4017` write
    /// delay parity decisions and as the reference timebase.
    cycle: u64,
    frame_counter: FrameCounter,
    pulse1: Pulse,
    pulse2: Pulse,
    triangle: Triangle,
    noise: Noise,
    dmc: Dmc,
    frame_irq: bool,
    dmc_irq: bool,
}

impl Apu {
    pub fn new(region: Region) -> Self {
        Self {
            region,
            cycle: 0,
            frame_counter: FrameCounter::new(region),
            pulse1: Pulse::new_pulse1(),
            pulse2: Pulse::new_pulse2(),
            triangle: Triangle::new(),
            noise: Noise::new(region),
            dmc: Dmc::new(region),
            frame_irq: false,
            dmc_irq: false,
        }
    }

    /// Warm reset - user pressing the Reset button (/RES low).
    ///
    /// Per nesdev "APU Power up and reset":
    /// - `$4015` enable latches clear - but unlike an actual `$4015=0`
    ///   write, the length counters are NOT forced to 0. Their values
    ///   persist (blargg `apu_reset/len_ctrs_enabled` relies on this).
    /// - DMC bytes_remaining = 0 and any pending DMA is dropped, but
    ///   the DMC output level is PRESERVED (next `$4011` write may pop).
    /// - Frame IRQ + DMC IRQ are cleared.
    /// - `$4017` mode / IRQ-inhibit bits are retained; the frame-counter
    ///   divider restarts as if `$4017` were rewritten with those bits
    ///   (3/4-cycle parity delay applies just like a real write).
    /// - All other APU registers retain their values.
    pub fn reset(&mut self) {
        self.pulse1.on_warm_reset();
        self.pulse2.on_warm_reset();
        self.triangle.on_warm_reset();
        self.noise.on_warm_reset();
        self.dmc.on_warm_reset();
        self.frame_irq = false;
        self.dmc_irq = false;
        self.frame_counter.reset_on_cpu_reset(self.cycle);
    }

    pub fn region(&self) -> Region {
        self.region
    }

    pub fn irq_line(&self) -> bool {
        self.frame_irq || self.dmc_irq
    }

    /// Test hook - force the frame IRQ flag so a unit test can drive
    /// the interrupt line without arranging ~29830 cycles of frame
    /// counter timing. Not exposed outside cfg(test).
    #[cfg(test)]
    pub(crate) fn set_frame_irq_for_test(&mut self, v: bool) {
        self.frame_irq = v;
    }

    /// Advance one CPU cycle. Must be called after every CPU bus access.
    pub fn tick_cpu_cycle(&mut self) {
        let event = self.frame_counter.tick(self.cycle);
        if event.set_frame_irq {
            self.frame_irq = true;
        }
        if event.quarter {
            self.clock_quarter();
        }
        if event.half {
            self.clock_half();
        }

        // Commit any staged halt/length-reload writes now that the
        // frame counter has had its chance to fire a half-frame
        // clock for this cycle. Ordering is load-bearing: commits
        // must run strictly after `clock_half` (blargg tests 10 and
        // 11) and strictly before channel timer ticks (so a staged
        // reload that survives is audible immediately).
        self.pulse1.commit_length_pending();
        self.pulse2.commit_length_pending();
        self.triangle.commit_length_pending();
        self.noise.commit_length_pending();

        // Triangle timer + sequencer tick every CPU cycle.
        self.triangle.tick_cpu();
        // DMC timer ticks every CPU cycle.
        let dmc_out = self.dmc.tick_cpu();
        if dmc_out.raised_irq {
            self.dmc_irq = true;
        }

        // Pulse + noise run at APU (=CPU/2) rate.
        if self.cycle & 1 == 1 {
            self.pulse1.tick_apu();
            self.pulse2.tick_apu();
            self.noise.tick_apu();
        }

        self.cycle = self.cycle.wrapping_add(1);
    }

    fn clock_quarter(&mut self) {
        self.pulse1.clock_quarter_frame();
        self.pulse2.clock_quarter_frame();
        self.triangle.clock_quarter_frame();
        self.noise.clock_quarter_frame();
    }

    fn clock_half(&mut self) {
        self.pulse1.clock_half_frame();
        self.pulse2.clock_half_frame();
        self.triangle.clock_half_frame();
        self.noise.clock_half_frame();
    }

    /// `$4015` read: returns channel-status bits, clears frame IRQ.
    ///
    /// Same-cycle race (nesdev "Frame IRQ flag"): if the frame counter
    /// sets `frame_irq` on the same CPU cycle as a `$4015` read,
    /// hardware must return the bit set and clear it after. `Bus::
    /// tick_pre_access` advances the APU *before* the CPU's bus
    /// access, so by the time `read_status` runs, any frame IRQ
    /// event for the current cycle has already set `frame_irq` and
    /// is observable here. `blargg_apu_2005.07.30/08.irq_timing`
    /// relies on this ordering - the older post-access model
    /// dispatched IRQ one cycle early because the CPU's
    /// `prev_irq_line` snapshot saw the flag a cycle before the
    /// `$4015` read could see it.
    pub fn read_status(&mut self) -> u8 {
        let mut status = 0u8;
        if self.pulse1.length_nonzero() {
            status |= 0x01;
        }
        if self.pulse2.length_nonzero() {
            status |= 0x02;
        }
        if self.triangle.length_nonzero() {
            status |= 0x04;
        }
        if self.noise.length_nonzero() {
            status |= 0x08;
        }
        if self.dmc.bytes_remaining() > 0 {
            status |= 0x10;
        }
        if self.frame_irq {
            status |= 0x40;
        }
        if self.dmc_irq {
            status |= 0x80;
        }
        // Reading $4015 clears the frame IRQ (not the DMC IRQ).
        self.frame_irq = false;
        status
    }

    /// `$4000-$4017` register write.
    pub fn write_reg(&mut self, addr: u16, data: u8) {
        match addr {
            0x4000 => self.pulse1.write_ctrl(data),
            0x4001 => self.pulse1.write_sweep(data),
            0x4002 => self.pulse1.write_timer_lo(data),
            0x4003 => self.pulse1.write_timer_hi(data),

            0x4004 => self.pulse2.write_ctrl(data),
            0x4005 => self.pulse2.write_sweep(data),
            0x4006 => self.pulse2.write_timer_lo(data),
            0x4007 => self.pulse2.write_timer_hi(data),

            0x4008 => self.triangle.write_linear(data),
            0x400A => self.triangle.write_timer_lo(data),
            0x400B => self.triangle.write_timer_hi(data),

            0x400C => self.noise.write_ctrl(data),
            0x400E => self.noise.write_period(data),
            0x400F => self.noise.write_length(data),

            0x4010 => {
                self.dmc.write_ctrl(data);
                if !self.dmc.irq_enabled() {
                    self.dmc_irq = false;
                }
            }
            0x4011 => self.dmc.write_output(data),
            0x4012 => self.dmc.write_sample_addr(data),
            0x4013 => self.dmc.write_sample_len(data),

            0x4015 => {
                self.pulse1.set_enabled((data & 0x01) != 0);
                self.pulse2.set_enabled((data & 0x02) != 0);
                self.triangle.set_enabled((data & 0x04) != 0);
                self.noise.set_enabled((data & 0x08) != 0);
                // Mesen2-style transfer-start delay needs the CPU-cycle
                // parity at the moment `$4015` was written. `apu.cycle`
                // equals the current CPU cycle (phase-6 moved APU tick
                // into pre-access); even → 2-cycle delay, odd → 3.
                self.dmc.set_enabled((data & 0x10) != 0, (self.cycle & 1) == 1);
                // Any $4015 write clears DMC IRQ (not frame IRQ).
                self.dmc_irq = false;
            }
            0x4017 => {
                let parity_odd = (self.cycle & 1) == 1;
                self.frame_counter
                    .write_4017(data, self.cycle, parity_odd);
                if (data & 0x40) != 0 {
                    self.frame_irq = false;
                }
            }
            _ => {}
        }
    }

    /// Poll for a pending DMC DMA fetch. The bus should insert the stall
    /// cycles before actually performing the read and then pass the byte
    /// to [`Apu::dmc_dma_complete`].
    pub fn take_dmc_dma_request(&mut self) -> Option<DmcDmaRequest> {
        self.dmc.take_dma_request()
    }

    pub fn dmc_dma_complete(&mut self, byte: u8) {
        if self.dmc.dma_complete(byte) {
            self.dmc_irq = true;
        }
    }

    /// Sampled analog output in 0.0..≈0.98 using the 2A03 non-linear
    /// mixer. Computed via Blargg's precomputed lookup tables (nesdev
    /// "APU Mixer", "Lookup tables" form) - ~100× faster than the
    /// per-sample division-based formula and tiny-fraction accurate
    /// across the full input domain. Called from the bus every CPU
    /// cycle (~1.79 MHz NTSC), so the speed matters.
    /// Trace helper - exposes the DMC channel's internal state for the
    /// instruction-level tracer. Zero-cost unless the tracer is on.
    pub fn dmc_trace(&self) -> crate::nes::apu::dmc::DmcTraceSnapshot {
        self.dmc.trace_snapshot()
    }

    pub fn output_sample(&self) -> f32 {
        let p1 = self.pulse1.output() as usize;
        let p2 = self.pulse2.output() as usize;
        let tr = self.triangle.output() as usize;
        let ns = self.noise.output() as usize;
        let dmc = self.dmc.output() as usize;
        PULSE_TABLE[p1 + p2] + TND_TABLE[3 * tr + 2 * ns + dmc]
    }
}

/// `pulse_table[n] = 95.52 / (8128/n + 100)` for n in 1..=30, with
/// index 0 = 0.0. Covers the full domain `pulse1 + pulse2` where each
/// channel outputs 0..15.
static PULSE_TABLE: [f32; 31] = {
    let mut t = [0.0f32; 31];
    let mut n = 1usize;
    while n < 31 {
        t[n] = 95.52 / (8128.0 / n as f32 + 100.0);
        n += 1;
    }
    t
};

/// `tnd_table[n] = 163.67 / (24329/n + 100)` for n in 1..=202, with
/// index 0 = 0.0. Covers the full domain `3*triangle + 2*noise + dmc`
/// where triangle and noise output 0..15 and dmc outputs 0..127.
static TND_TABLE: [f32; 203] = {
    let mut t = [0.0f32; 203];
    let mut n = 1usize;
    while n < 203 {
        t[n] = 163.67 / (24329.0 / n as f32 + 100.0);
        n += 1;
    }
    t
};

#[cfg(test)]
mod tests {
    use super::*;

    fn ntsc() -> Apu {
        Apu::new(Region::Ntsc)
    }

    #[test]
    fn read_status_clears_frame_irq_not_dmc_irq() {
        let mut apu = ntsc();
        apu.frame_irq = true;
        apu.dmc_irq = true;

        let s = apu.read_status();

        assert_eq!(s & 0x40, 0x40, "frame IRQ bit must be set in returned value");
        assert_eq!(s & 0x80, 0x80, "DMC IRQ bit must be set in returned value");
        assert!(!apu.frame_irq, "frame IRQ cleared after read");
        assert!(apu.dmc_irq, "DMC IRQ preserved across $4015 read");
    }

    #[test]
    fn write_4015_clears_dmc_irq_not_frame_irq() {
        let mut apu = ntsc();
        apu.frame_irq = true;
        apu.dmc_irq = true;

        apu.write_reg(0x4015, 0x00);

        assert!(apu.frame_irq, "frame IRQ survives $4015 write");
        assert!(!apu.dmc_irq, "DMC IRQ cleared by any $4015 write");
    }

    #[test]
    fn write_4010_with_irq_disabled_clears_dmc_irq() {
        let mut apu = ntsc();
        apu.dmc_irq = true;

        // Bit 7 = 0 → IRQ disabled; must also clear any latched DMC IRQ.
        apu.write_reg(0x4010, 0x00);

        assert!(!apu.dmc_irq, "$4010 with I=0 must clear DMC IRQ");
    }

    #[test]
    fn write_4010_with_irq_enabled_preserves_dmc_irq() {
        let mut apu = ntsc();
        apu.dmc_irq = true;

        // Bit 7 = 1 → IRQ stays enabled; DMC IRQ latch must not be cleared.
        apu.write_reg(0x4010, 0x80);

        assert!(apu.dmc_irq, "$4010 with I=1 leaves latched DMC IRQ intact");
    }

    #[test]
    fn dmc_enable_via_4015_arms_dma_request_after_transfer_start_delay() {
        let mut apu = ntsc();
        // $4013: sample length = 1 byte; $4012: start addr $C000.
        apu.write_reg(0x4013, 0x00);
        apu.write_reg(0x4012, 0x00);
        apu.write_reg(0x4015, 0x10); // enable DMC

        // Mesen2's `_transferStartDelay` defers the DMA arming by 2 or
        // 3 CPU cycles (per write-cycle parity). Tick the APU until the
        // delay clears; the request must then be visible.
        assert!(apu.take_dmc_dma_request().is_none());
        for _ in 0..3 {
            apu.tick_cpu_cycle();
        }
        assert!(
            apu.take_dmc_dma_request().is_some(),
            "enabling DMC with bytes_remaining==0 must arm a DMA fetch \
             once the transfer-start delay elapses"
        );
    }

    #[test]
    fn dmc_disable_via_4015_mid_sample_discards_pending_dma() {
        let mut apu = ntsc();
        apu.write_reg(0x4013, 0x01); // 17 bytes
        apu.write_reg(0x4015, 0x10); // enable - arms DMA

        apu.write_reg(0x4015, 0x00); // disable before bus services the DMA

        assert!(
            apu.take_dmc_dma_request().is_none(),
            "pending DMA must be dropped when DMC is disabled"
        );
    }
}
