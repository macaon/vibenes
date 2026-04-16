//! Triangle channel. 32-step sequencer clocked at CPU rate. Advances
//! only when both the linear counter and length counter are nonzero.
//!
//! Ultrasonic handling (timer < 2): mute to the center of the triangle
//! wave (the duty[8] slot = 0x0F) rather than silencing — matches puNES
//! and what blargg expects.

use super::length::LengthCounter;

const TRIANGLE_SEQUENCE: [u8; 32] = [
    15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0, //
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
];

#[derive(Debug, Default)]
pub struct Triangle {
    length: LengthCounter,

    linear_reload_flag: bool,
    linear_reload_value: u8,
    linear_counter: u8,
    control_flag: bool,

    timer: u16,
    period: u16,
    sequencer_pos: u8,
}

impl Triangle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.length.set_enabled(enabled);
    }

    pub fn length_nonzero(&self) -> bool {
        self.length.is_nonzero()
    }

    /// $4008: CRRRRRRR — control flag + linear counter reload value.
    pub fn write_linear(&mut self, data: u8) {
        self.control_flag = (data & 0x80) != 0;
        self.linear_reload_value = data & 0x7F;
        // Control flag doubles as length halt.
        self.length.set_halt(self.control_flag);
    }

    pub fn write_timer_lo(&mut self, data: u8) {
        self.period = (self.period & 0xFF00) | data as u16;
    }

    pub fn write_timer_hi(&mut self, data: u8, length_clocked: bool) {
        self.period = (self.period & 0x00FF) | (((data & 0x07) as u16) << 8);
        self.length.load(data >> 3, length_clocked);
        self.linear_reload_flag = true;
    }

    pub fn clock_quarter_frame(&mut self) {
        if self.linear_reload_flag {
            self.linear_counter = self.linear_reload_value;
        } else if self.linear_counter > 0 {
            self.linear_counter -= 1;
        }
        if !self.control_flag {
            self.linear_reload_flag = false;
        }
    }

    pub fn clock_half_frame(&mut self) {
        self.length.clock_half_frame();
    }

    /// Tick the CPU-rate timer; advance sequencer on underflow only if
    /// both counters are nonzero.
    pub fn tick_cpu(&mut self) {
        if self.timer == 0 {
            self.timer = self.period;
            if self.linear_counter > 0 && self.length.is_nonzero() {
                self.sequencer_pos = (self.sequencer_pos + 1) & 0x1F;
            }
        } else {
            self.timer -= 1;
        }
    }

    pub fn output(&self) -> u8 {
        if self.period < 2 {
            // Ultrasonic — mute to midpoint rather than 0 (puNES).
            return 0x0F / 2 + 1; // value at sequence center
        }
        TRIANGLE_SEQUENCE[self.sequencer_pos as usize]
    }
}
