// SPDX-License-Identifier: GPL-3.0-or-later
//! Pulse channel (one of two). Combines an envelope, a sweep unit, a
//! length counter, an 11-bit timer, and an 8-step duty sequencer.

use super::envelope::Envelope;
use super::length::LengthCounter;
use super::sweep::Sweep;

const DUTY_TABLE: [[u8; 8]; 4] = [
    [0, 1, 0, 0, 0, 0, 0, 0], // 12.5%
    [0, 1, 1, 0, 0, 0, 0, 0], // 25%
    [0, 1, 1, 1, 1, 0, 0, 0], // 50%
    [1, 0, 0, 1, 1, 1, 1, 1], // 75%
];

#[derive(Debug)]
pub struct Pulse {
    envelope: Envelope,
    sweep: Sweep,
    length: LengthCounter,

    duty: u8,
    sequencer_pos: u8,

    timer: u16,
    period: u16,
}

impl Pulse {
    pub fn new_pulse1() -> Self {
        Self::new(true)
    }

    pub fn new_pulse2() -> Self {
        Self::new(false)
    }

    fn new(ones_complement_sweep: bool) -> Self {
        Self {
            envelope: Envelope::default(),
            sweep: Sweep::new(ones_complement_sweep),
            length: LengthCounter::default(),
            duty: 0,
            sequencer_pos: 0,
            timer: 0,
            period: 0,
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
        self.duty = data >> 6;
        // Length halt + envelope loop are the same bit. Halt is
        // staged (committed at end of cycle, after any same-cycle
        // half-frame clock) to preserve blargg's "halt applies AFTER
        // the length clock" rule.
        self.length.stage_halt((data & 0x20) != 0);
        self.envelope.write_ctrl(data);
    }

    pub fn write_sweep(&mut self, data: u8) {
        self.sweep.write(data);
        self.sweep.update_target(self.period);
    }

    pub fn write_timer_lo(&mut self, data: u8) {
        self.period = (self.period & 0xFF00) | data as u16;
        self.sweep.update_target(self.period);
    }

    pub fn write_timer_hi(&mut self, data: u8) {
        self.period = (self.period & 0x00FF) | (((data & 0x07) as u16) << 8);
        self.sweep.update_target(self.period);
        // Length reload is staged; commit drops it if a half-frame
        // clock fires this cycle AND the counter was non-zero at
        // write time. Sequencer-pos reset and envelope restart
        // happen immediately (nesdev: "duty cycle is NOT reset on
        // $4003 write" is false — sequencer IS reset — but those
        // bits are not part of the same-cycle length race).
        self.length.stage_reload(data >> 3);
        self.sequencer_pos = 0;
        self.envelope.restart();
    }

    /// End-of-cycle commit. Applied by the APU after any same-cycle
    /// half-frame clock has run, giving staged halt/reload writes
    /// their hardware-correct visibility window.
    pub fn commit_length_pending(&mut self) {
        self.length.commit_pending();
    }

    pub fn clock_quarter_frame(&mut self) {
        self.envelope.clock_quarter_frame();
    }

    pub fn clock_half_frame(&mut self) {
        self.length.clock_half_frame();
        if let Some(new_period) = self.sweep.clock_half_frame(self.period) {
            self.period = new_period;
            self.sweep.update_target(self.period);
        }
    }

    /// Tick the 11-bit timer at APU rate (once every 2 CPU cycles).
    pub fn tick_apu(&mut self) {
        if self.timer == 0 {
            self.timer = self.period;
            self.sequencer_pos = (self.sequencer_pos + 1) & 0x07;
        } else {
            self.timer -= 1;
        }
    }

    /// Current 4-bit channel output (0..=15).
    pub fn output(&self) -> u8 {
        if !self.length.is_nonzero() {
            return 0;
        }
        if self.sweep.muted(self.period) {
            return 0;
        }
        let duty_bit = DUTY_TABLE[self.duty as usize][self.sequencer_pos as usize];
        if duty_bit == 0 {
            0
        } else {
            self.envelope.volume()
        }
    }
}
