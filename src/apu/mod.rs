//! 2A03 APU — five channels plus a frame counter driving per-channel
//! timing events. Ticked once per CPU cycle by the bus.
//!
//! References: NES dev wiki APU pages, Mesen2 `Core/NES/APU/*`, puNES
//! `src/core/apu.*`. See also `reference/apu.md` and `reference/*-notes.md`
//! in the nes-expert skill.

use crate::clock::Region;

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
    /// Set inside length_counter half-frame clock. Cleared at the start
    /// of each CPU tick. Used by channel length-load writes to detect the
    /// "write on the same cycle as the clock" race.
    length_clocked: bool,
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
            length_clocked: false,
        }
    }

    /// Warm reset — matches what the 2A03 does on /RES low: silence all
    /// channels, clear frame IRQ, restart the frame-counter divider, keep
    /// the DMC output level so it doesn't pop.
    pub fn reset(&mut self) {
        self.write_reg(0x4015, 0);
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

    /// Advance one CPU cycle. Must be called after every CPU bus access.
    pub fn tick_cpu_cycle(&mut self) {
        self.length_clocked = false;

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
        self.length_clocked = true;
        self.pulse1.clock_half_frame();
        self.pulse2.clock_half_frame();
        self.triangle.clock_half_frame();
        self.noise.clock_half_frame();
    }

    /// `$4015` read: returns channel-status bits, clears frame IRQ.
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
            0x4003 => self.pulse1.write_timer_hi(data, self.length_clocked),

            0x4004 => self.pulse2.write_ctrl(data),
            0x4005 => self.pulse2.write_sweep(data),
            0x4006 => self.pulse2.write_timer_lo(data),
            0x4007 => self.pulse2.write_timer_hi(data, self.length_clocked),

            0x4008 => self.triangle.write_linear(data),
            0x400A => self.triangle.write_timer_lo(data),
            0x400B => self.triangle.write_timer_hi(data, self.length_clocked),

            0x400C => self.noise.write_ctrl(data),
            0x400E => self.noise.write_period(data),
            0x400F => self.noise.write_length(data, self.length_clocked),

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
                self.dmc.set_enabled((data & 0x10) != 0);
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

    /// Sampled analog output in 0.0..=1.0 using the nonlinear APU mixer.
    pub fn output_sample(&self) -> f32 {
        let p1 = self.pulse1.output() as u32;
        let p2 = self.pulse2.output() as u32;
        let tr = self.triangle.output() as u32;
        let ns = self.noise.output() as u32;
        let dmc = self.dmc.output() as u32;
        pulse_out(p1 + p2) + tnd_out(3 * tr + 2 * ns + dmc)
    }
}

fn pulse_out(n: u32) -> f32 {
    if n == 0 {
        0.0
    } else {
        95.88 / (8128.0 / n as f32 + 100.0)
    }
}

fn tnd_out(n: u32) -> f32 {
    if n == 0 {
        0.0
    } else {
        159.79 / (1.0 / (n as f32 / 100.0) + 100.0)
    }
}
