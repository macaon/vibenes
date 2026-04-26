// SPDX-License-Identifier: GPL-3.0-or-later
//! Sweep unit - pulse channels only.
//!
//! Pulse 1 uses ones'-complement negate (subtract shifted + 1), pulse 2
//! uses two's-complement negate (subtract shifted). Mute is evaluated
//! continuously: the channel silences when the current period is below 8
//! or the target period would exceed $7FF.

#[derive(Debug)]
pub struct Sweep {
    enabled: bool,
    period: u8,
    divider: u8,
    negate: bool,
    shift: u8,
    reload: bool,
    ones_complement: bool,
    target_period: u16,
}

impl Sweep {
    pub fn new(ones_complement: bool) -> Self {
        Self {
            enabled: false,
            period: 0,
            divider: 0,
            negate: false,
            shift: 0,
            reload: false,
            ones_complement,
            target_period: 0,
        }
    }

    pub fn write(&mut self, data: u8) {
        self.enabled = (data & 0x80) != 0;
        self.period = (data >> 4) & 0x07;
        self.negate = (data & 0x08) != 0;
        self.shift = data & 0x07;
        self.reload = true;
    }

    pub fn update_target(&mut self, current_period: u16) {
        let delta = current_period >> self.shift;
        let signed = if self.negate {
            let mut v = current_period as i32 - delta as i32;
            if self.ones_complement {
                v -= 1;
            }
            v
        } else {
            current_period as i32 + delta as i32
        };
        self.target_period = if signed < 0 { 0 } else { signed as u16 };
    }

    /// Mute predicate, evaluated continuously in the channel output path.
    pub fn muted(&self, current_period: u16) -> bool {
        current_period < 8 || self.target_period > 0x7FF
    }

    /// Advance the sweep divider. Returns `Some(new_period)` when the
    /// channel's period should be replaced this clock; otherwise `None`.
    pub fn clock_half_frame(&mut self, current_period: u16) -> Option<u16> {
        let mut new_period = None;
        if self.divider == 0
            && self.enabled
            && self.shift != 0
            && !self.muted(current_period)
        {
            new_period = Some(self.target_period);
        }
        if self.divider == 0 || self.reload {
            self.divider = self.period;
            self.reload = false;
        } else {
            self.divider -= 1;
        }
        new_period
    }
}
