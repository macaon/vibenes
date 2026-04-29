// SPDX-License-Identifier: GPL-3.0-or-later
//! Envelope generator - shared by pulse 1, pulse 2, and noise.

#[derive(Debug, Default)]
pub struct Envelope {
    start: bool,
    loop_flag: bool,
    constant: bool,
    divider_period: u8,
    divider: u8,
    decay: u8,
}

impl Envelope {
    /// Write the channel's DDLCNNNN / --LCNNNN byte.
    pub fn write_ctrl(&mut self, data: u8) {
        self.loop_flag = (data & 0x20) != 0;
        self.constant = (data & 0x10) != 0;
        self.divider_period = data & 0x0F;
    }

    /// Called when the channel's timer-high / length byte is written -
    /// sets the envelope's start flag.
    pub fn restart(&mut self) {
        self.start = true;
    }

    pub fn clock_quarter_frame(&mut self) {
        if self.start {
            self.start = false;
            self.decay = 15;
            self.divider = self.divider_period;
        } else if self.divider == 0 {
            self.divider = self.divider_period;
            if self.decay > 0 {
                self.decay -= 1;
            } else if self.loop_flag {
                self.decay = 15;
            }
        } else {
            self.divider -= 1;
        }
    }

    /// Current envelope output (0..=15).
    pub fn volume(&self) -> u8 {
        if self.constant {
            self.divider_period
        } else {
            self.decay
        }
    }

    pub(crate) fn save_state_capture(&self) -> crate::save_state::apu::EnvelopeSnap {
        crate::save_state::apu::EnvelopeSnap {
            start: self.start,
            loop_flag: self.loop_flag,
            constant: self.constant,
            divider_period: self.divider_period,
            divider: self.divider,
            decay: self.decay,
        }
    }

    pub(crate) fn save_state_apply(&mut self, snap: crate::save_state::apu::EnvelopeSnap) {
        self.start = snap.start;
        self.loop_flag = snap.loop_flag;
        self.constant = snap.constant;
        self.divider_period = snap.divider_period;
        self.divider = snap.divider;
        self.decay = snap.decay;
    }
}
