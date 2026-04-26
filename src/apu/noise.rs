// SPDX-License-Identifier: GPL-3.0-or-later
//! Noise channel - 15-bit LFSR driven at APU rate by a region-specific
//! period table. Two feedback modes: long (XOR bit 0 with bit 1) and
//! short (XOR bit 0 with bit 6), selected by $400E bit 7.

use super::envelope::Envelope;
use super::length::LengthCounter;
use crate::clock::Region;

const NOISE_PERIODS_NTSC: [u16; 16] = [
    4, 8, 16, 32, 64, 96, 128, 160, 202, 254, 380, 508, 762, 1016, 2034, 4068,
];

const NOISE_PERIODS_PAL: [u16; 16] = [
    4, 8, 14, 30, 60, 88, 118, 148, 188, 236, 354, 472, 708, 944, 1890, 3778,
];

#[derive(Debug)]
pub struct Noise {
    region: Region,
    envelope: Envelope,
    length: LengthCounter,
    lfsr: u16,
    mode_short: bool,
    timer: u16,
    period: u16,
}

impl Noise {
    pub fn new(region: Region) -> Self {
        Self {
            region,
            envelope: Envelope::default(),
            length: LengthCounter::default(),
            lfsr: 1,
            mode_short: false,
            timer: 0,
            period: NOISE_PERIODS_NTSC[0],
        }
    }

    fn period_table(&self) -> &'static [u16; 16] {
        match self.region {
            Region::Ntsc => &NOISE_PERIODS_NTSC,
            Region::Pal => &NOISE_PERIODS_PAL,
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.length.set_enabled(enabled);
    }

    /// Warm-reset handler: drop the `$4015` enable latch but keep the
    /// length counter value (see `LengthCounter::clear_enable_latch_only`).
    pub fn on_warm_reset(&mut self) {
        self.length.clear_enable_latch_only();
    }

    pub fn length_nonzero(&self) -> bool {
        self.length.is_nonzero()
    }

    pub fn write_ctrl(&mut self, data: u8) {
        // Halt is staged - same rule as pulse. See length.rs.
        self.length.stage_halt((data & 0x20) != 0);
        self.envelope.write_ctrl(data);
    }

    pub fn write_period(&mut self, data: u8) {
        self.mode_short = (data & 0x80) != 0;
        let idx = (data & 0x0F) as usize;
        self.period = self.period_table()[idx];
    }

    pub fn write_length(&mut self, data: u8) {
        self.length.stage_reload(data >> 3);
        self.envelope.restart();
    }

    pub fn commit_length_pending(&mut self) {
        self.length.commit_pending();
    }

    pub fn clock_quarter_frame(&mut self) {
        self.envelope.clock_quarter_frame();
    }

    pub fn clock_half_frame(&mut self) {
        self.length.clock_half_frame();
    }

    pub fn tick_apu(&mut self) {
        if self.timer == 0 {
            self.timer = self.period;
            let tap = if self.mode_short { 6 } else { 1 };
            let bit0 = self.lfsr & 1;
            let bitn = (self.lfsr >> tap) & 1;
            let feedback = bit0 ^ bitn;
            self.lfsr = (self.lfsr >> 1) | (feedback << 14);
        } else {
            self.timer -= 1;
        }
    }

    pub fn output(&self) -> u8 {
        if (self.lfsr & 1) != 0 {
            return 0;
        }
        if !self.length.is_nonzero() {
            return 0;
        }
        self.envelope.volume()
    }
}
