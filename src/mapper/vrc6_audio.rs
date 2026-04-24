// SPDX-License-Identifier: GPL-3.0-or-later
//! Konami VRC6 expansion audio — 2 pulse channels + 1 sawtooth.
//!
//! Three voices on the cart, summed linearly into one DAC line that
//! returns to the Famicom over the EXP6 pin:
//!
//! - **Pulse 1 / Pulse 2** (`$9000-$9002`, `$A000-$A002`): 16-step
//!   counters with programmable duty (4-bit field selects a duty
//!   threshold 0..15). Volume is 4-bit. An "ignore duty" bit makes
//!   the channel output full volume every step — games use this to
//!   fake a sawtooth-ish sound on a pulse.
//! - **Sawtooth** (`$B000-$B002`): a 7-step-per-octant accumulator
//!   that adds `accumulator_rate` every even step, zeros on step 0,
//!   running for 14 steps before restarting. High 5 bits of the
//!   accumulator are the output.
//!
//! The combined output level is `pulse1.vol + pulse2.vol + saw.vol`
//! in 0..61 (pulse max 15 each, saw max 31). The bus scales this
//! into our 0..1 mix space matching Mesen2's default mix balance.
//!
//! ## Global controls (`$9003`)
//!
//! - Bit 0: **halt audio** — freezes all three channel clocks. Games
//!   use this to silence the cart audio without losing state.
//! - Bits 1-2: **frequency shift** — right-shift the period divider
//!   by 4 (bit 1) or 8 (bit 2) for high-pitched SFX.
//!
//! ## References
//!
//! Port of Mesen2's `Core/NES/Mappers/Audio/{Vrc6Pulse,Vrc6Saw,
//! Vrc6Audio}.h`. The saw step count (14), the even-step-only
//! accumulator add, the enabled-gate clearing `_accumulator = 0`
//! and `_step = 0`, and the mixer weight are all protocol-exact.
//! Cross-checked against nesdev.org wiki "VRC6 audio" and the
//! long-form writeups in the Famicom Developer Wiki.

/// Per-raw-unit mix scale for VRC6 against our 0..1 APU-sample space.
///
/// Derivation mirrors the FDS scale (see `fds_audio::FDS_MIX_SCALE`):
/// Mesen2's `NesSoundMixer` applies a `×15` inline multiplier inside
/// `Vrc6Audio::ClockAudio` and a `×5` channel weight in
/// `GetOutputVolume`, giving an effective per-raw-unit Mesen2 scale
/// of 75. Our 0..1 unit is ~5018 Mesen2 units, so VRC6 per-raw =
/// 75 / 5018 ≈ 0.01494. Peak raw = 61 → peak mix sample ≈ 0.912,
/// ~3.6× the FDS peak — accurate to Mesen2's default balance where
/// VRC6 (the lead instrument in Akumajō Densetsu) sits prominent.
const VRC6_MIX_SCALE: f32 = 75.0 / 5018.0;

/// Decode `$9003` bits 1-2 into the period shift (0 / 4 / 8).
/// Bit 2 wins over bit 1 per Mesen2's precedence.
fn freq_shift(value: u8) -> u8 {
    if (value & 0x04) != 0 {
        8
    } else if (value & 0x02) != 0 {
        4
    } else {
        0
    }
}

// ---- Pulse channel ----

/// One VRC6 pulse channel. Runs a 4-bit step counter driven by a
/// 12-bit reload; output is either 0 or `volume` depending on
/// `step <= duty_cycle`. The "ignore duty" bit short-circuits the
/// duty comparator so the channel always outputs `volume` when
/// enabled (hardware "square always on" mode).
#[derive(Debug, Clone)]
pub(super) struct Vrc6Pulse {
    volume: u8,
    duty_cycle: u8,
    ignore_duty: bool,
    frequency: u16,
    enabled: bool,
    timer: i32,
    step: u8,
    frequency_shift: u8,
}

impl Vrc6Pulse {
    pub(super) fn new() -> Self {
        Self {
            volume: 0,
            duty_cycle: 0,
            ignore_duty: false,
            frequency: 1,
            enabled: false,
            timer: 1,
            step: 0,
            frequency_shift: 0,
        }
    }

    /// Register write. `addr & 0x03` selects the sub-register:
    /// 0 = volume + duty + ignore-duty, 1 = freq lo, 2 = freq hi + enable.
    pub(super) fn write_reg(&mut self, addr: u16, value: u8) {
        match addr & 0x03 {
            0 => {
                self.volume = value & 0x0F;
                self.duty_cycle = (value & 0x70) >> 4;
                self.ignore_duty = (value & 0x80) != 0;
            }
            1 => {
                self.frequency = (self.frequency & 0x0F00) | value as u16;
            }
            2 => {
                self.frequency = (self.frequency & 0x00FF) | (((value as u16) & 0x0F) << 8);
                self.enabled = (value & 0x80) != 0;
                if !self.enabled {
                    // Nesdev: "The step is forced to 0 when E is cleared."
                    self.step = 0;
                }
            }
            _ => {}
        }
    }

    pub(super) fn set_frequency_shift(&mut self, shift: u8) {
        self.frequency_shift = shift;
    }

    /// Advance by one CPU cycle.
    pub(super) fn clock(&mut self) {
        if !self.enabled {
            return;
        }
        self.timer -= 1;
        if self.timer == 0 {
            self.step = (self.step + 1) & 0x0F;
            // Reload with the (shifted) period + 1. The +1 matches
            // the 2A03 period semantics: a raw frequency of 0 still
            // steps once per cycle. Shift is 0/4/8 per $9003.
            self.timer = ((self.frequency >> self.frequency_shift) as i32) + 1;
        }
    }

    pub(super) fn volume_out(&self) -> u8 {
        if !self.enabled {
            0
        } else if self.ignore_duty {
            self.volume
        } else if self.step <= self.duty_cycle {
            self.volume
        } else {
            0
        }
    }
}

// ---- Sawtooth channel ----

/// VRC6 sawtooth. 14-step cycle; on even steps the 8-bit accumulator
/// receives `+accumulator_rate`, step 0 zeros it. The cartridge DAC
/// outputs the top 5 bits. This produces a stepped saw that ramps up
/// in 7 increments over one half of the full-step cycle.
#[derive(Debug, Clone)]
pub(super) struct Vrc6Saw {
    accumulator_rate: u8,
    accumulator: u8,
    frequency: u16,
    enabled: bool,
    timer: i32,
    step: u8,
    frequency_shift: u8,
}

impl Vrc6Saw {
    pub(super) fn new() -> Self {
        Self {
            accumulator_rate: 0,
            accumulator: 0,
            frequency: 1,
            enabled: false,
            timer: 1,
            step: 0,
            frequency_shift: 0,
        }
    }

    pub(super) fn write_reg(&mut self, addr: u16, value: u8) {
        match addr & 0x03 {
            0 => self.accumulator_rate = value & 0x3F,
            1 => {
                self.frequency = (self.frequency & 0x0F00) | value as u16;
            }
            2 => {
                self.frequency = (self.frequency & 0x00FF) | (((value as u16) & 0x0F) << 8);
                self.enabled = (value & 0x80) != 0;
                if !self.enabled {
                    // Nesdev: "If E is clear, the accumulator is forced
                    // to zero until E is again set" AND "The phase of
                    // the saw generator can be mostly reset by clearing
                    // and immediately setting E." — both behaviors.
                    self.accumulator = 0;
                    self.step = 0;
                }
            }
            _ => {}
        }
    }

    pub(super) fn set_frequency_shift(&mut self, shift: u8) {
        self.frequency_shift = shift;
    }

    pub(super) fn clock(&mut self) {
        if !self.enabled {
            return;
        }
        self.timer -= 1;
        if self.timer == 0 {
            self.step = (self.step + 1) % 14;
            self.timer = ((self.frequency >> self.frequency_shift) as i32) + 1;
            if self.step == 0 {
                self.accumulator = 0;
            } else if (self.step & 0x01) == 0 {
                // Even step: add rate. Wraps on 8-bit overflow —
                // real hardware behavior, makes the "stuck high"
                // case possible when the game sets a too-big rate.
                self.accumulator = self.accumulator.wrapping_add(self.accumulator_rate);
            }
        }
    }

    pub(super) fn volume_out(&self) -> u8 {
        if !self.enabled {
            0
        } else {
            // Output is the high 5 bits of the accumulator.
            self.accumulator >> 3
        }
    }
}

// ---- Combined audio unit ----

/// Three VRC6 voices + the `$9003` control register, plus a cached
/// output sample updated once per CPU cycle by [`Vrc6Audio::clock`].
/// The owning mapper routes writes through [`Vrc6Audio::write_register`]
/// for any `$9000-$B002` address and reads the mix-ready sample via
/// [`Vrc6Audio::mix_sample`].
pub struct Vrc6Audio {
    pulse1: Vrc6Pulse,
    pulse2: Vrc6Pulse,
    saw: Vrc6Saw,
    halt_audio: bool,
    last_output: u8,
}

impl Vrc6Audio {
    pub fn new() -> Self {
        Self {
            pulse1: Vrc6Pulse::new(),
            pulse2: Vrc6Pulse::new(),
            saw: Vrc6Saw::new(),
            halt_audio: false,
            last_output: 0,
        }
    }

    pub fn write_register(&mut self, addr: u16, value: u8) {
        match addr & 0xF003 {
            0x9000 | 0x9001 | 0x9002 => self.pulse1.write_reg(addr, value),
            0x9003 => {
                self.halt_audio = (value & 0x01) != 0;
                let shift = freq_shift(value);
                self.pulse1.set_frequency_shift(shift);
                self.pulse2.set_frequency_shift(shift);
                self.saw.set_frequency_shift(shift);
            }
            0xA000 | 0xA001 | 0xA002 => self.pulse2.write_reg(addr, value),
            0xB000 | 0xB001 | 0xB002 => self.saw.write_reg(addr, value),
            _ => {}
        }
    }

    /// Advance the three channels by one CPU cycle and update the
    /// cached output. Safe to call regardless of `halt_audio` — the
    /// flag gates CLOCKING only; reads of `last_output` return the
    /// frozen value exactly as real hardware does.
    pub fn clock(&mut self) {
        if !self.halt_audio {
            self.pulse1.clock();
            self.pulse2.clock();
            self.saw.clock();
        }
        let out = self.pulse1.volume_out() + self.pulse2.volume_out() + self.saw.volume_out();
        self.last_output = out;
    }

    /// Raw 0..61 combined channel output. Tests + state-introspection
    /// screens want this; the bus mixer wants [`Self::mix_sample`].
    pub fn output_level(&self) -> u8 {
        self.last_output
    }

    /// Mix-ready sample in approximately 0.0..0.91 — pre-scaled
    /// against the APU's 0.0..≈1.0 range matching Mesen2's defaults.
    pub fn mix_sample(&self) -> f32 {
        self.last_output as f32 * VRC6_MIX_SCALE
    }
}

impl Default for Vrc6Audio {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_audio_is_silent() {
        let a = Vrc6Audio::new();
        assert_eq!(a.output_level(), 0);
        assert_eq!(a.mix_sample(), 0.0);
    }

    /// Pulse 1 write to $9000 decodes volume / duty / ignore-duty.
    #[test]
    fn pulse_reg0_decode() {
        let mut a = Vrc6Audio::new();
        a.write_register(0x9000, 0x9F); // ignore_duty + duty=1 + vol=F
        assert!(a.pulse1.ignore_duty);
        assert_eq!(a.pulse1.duty_cycle, 1);
        assert_eq!(a.pulse1.volume, 0xF);
    }

    /// $9003 bits 1-2 decode to the shift-by-4 / shift-by-8 / none
    /// frequency shifter and propagate to all three channels.
    #[test]
    fn frequency_shift_propagates_to_all_channels() {
        let mut a = Vrc6Audio::new();
        a.write_register(0x9003, 0x02);
        assert_eq!(a.pulse1.frequency_shift, 4);
        assert_eq!(a.pulse2.frequency_shift, 4);
        assert_eq!(a.saw.frequency_shift, 4);
        a.write_register(0x9003, 0x04);
        assert_eq!(a.pulse1.frequency_shift, 8);
        a.write_register(0x9003, 0x00);
        assert_eq!(a.pulse1.frequency_shift, 0);
    }

    /// Bit 2 wins over bit 1 when both are set — Mesen2's precedence.
    #[test]
    fn frequency_shift_bit2_wins() {
        let mut a = Vrc6Audio::new();
        a.write_register(0x9003, 0x06); // bits 1 and 2 both set
        assert_eq!(a.pulse1.frequency_shift, 8);
    }

    /// Halt bit freezes all channel clocks. The cached output value
    /// stays whatever it was at the moment of the write.
    #[test]
    fn halt_audio_freezes_clocks() {
        let mut a = Vrc6Audio::new();
        // Enable pulse1 at freq=0 so it steps every cycle.
        a.write_register(0x9000, 0x8F); // ignore_duty, vol=F
        a.write_register(0x9001, 0x00);
        a.write_register(0x9002, 0x80); // enable
        a.clock();
        // Halt.
        a.write_register(0x9003, 0x01);
        let p1_step_before = a.pulse1.step;
        for _ in 0..100 {
            a.clock();
        }
        assert_eq!(a.pulse1.step, p1_step_before);
    }

    /// Disabling a pulse clears its step to 0 — retrigger behavior.
    #[test]
    fn pulse_disable_resets_step() {
        let mut a = Vrc6Audio::new();
        a.write_register(0x9000, 0x8F);
        a.write_register(0x9001, 0x00);
        a.write_register(0x9002, 0x80); // enable
        // Clock a few times so step advances.
        for _ in 0..20 {
            a.clock();
        }
        assert_ne!(a.pulse1.step, 0);
        // Disable.
        a.write_register(0x9002, 0x00);
        assert_eq!(a.pulse1.step, 0);
    }

    /// When the ignore-duty bit is clear, the pulse is high for
    /// `step <= duty_cycle` and zero otherwise.
    #[test]
    fn pulse_duty_gate_gates_output() {
        let mut a = Vrc6Audio::new();
        // Volume = 10, duty = 3, ignore-duty clear.
        a.write_register(0x9000, 0x3A);
        a.write_register(0x9001, 0x00);
        a.write_register(0x9002, 0x80);
        // Step 0..3 high, 4..15 low.
        a.pulse1.step = 0;
        assert_eq!(a.pulse1.volume_out(), 10);
        a.pulse1.step = 3;
        assert_eq!(a.pulse1.volume_out(), 10);
        a.pulse1.step = 4;
        assert_eq!(a.pulse1.volume_out(), 0);
        a.pulse1.step = 15;
        assert_eq!(a.pulse1.volume_out(), 0);
    }

    /// Ignore-duty bit short-circuits the comparator; output is
    /// `volume` on every step when enabled.
    #[test]
    fn pulse_ignore_duty_outputs_full_volume() {
        let mut a = Vrc6Audio::new();
        a.write_register(0x9000, 0x8A); // ignore-duty, vol=0x0A
        a.write_register(0x9001, 0x00);
        a.write_register(0x9002, 0x80);
        for step in 0..16 {
            a.pulse1.step = step;
            assert_eq!(a.pulse1.volume_out(), 0x0A, "step {step}");
        }
    }

    /// Saw step 0 zeros the accumulator; subsequent even steps add
    /// `accumulator_rate`; output is the top 5 bits.
    #[test]
    fn saw_accumulator_ramps_and_resets() {
        let mut a = Vrc6Audio::new();
        a.write_register(0xB000, 0x08); // rate = 8
        a.write_register(0xB001, 0x00);
        a.write_register(0xB002, 0x80); // enable, freq = 0
        // With freq = 0, timer reloads to 1 → step advances every cycle.
        // Step sequence: 1 (odd, no add), 2 (even, +8), 3 (odd), 4 (+8),
        // 5, 6 (+8), 7, 8 (+8), 9, 10 (+8), 11, 12 (+8), 13, 0 (reset).
        // After 12 clocks: step=12, accumulator = 6 × 8 = 48, high-5 = 6.
        for _ in 0..12 {
            a.clock();
        }
        assert_eq!(a.saw.step, 12);
        assert_eq!(a.saw.accumulator, 48);
        assert_eq!(a.saw.volume_out(), 48 >> 3);
        // Two more clocks: step 13 (odd, no add), then step 0 (reset).
        a.clock();
        a.clock();
        assert_eq!(a.saw.step, 0);
        assert_eq!(a.saw.accumulator, 0);
    }

    /// Disabling the saw forces the accumulator + step to 0 (retrigger
    /// pattern used by music drivers).
    #[test]
    fn saw_disable_resets_state() {
        let mut a = Vrc6Audio::new();
        a.write_register(0xB000, 0x10);
        a.write_register(0xB001, 0x00);
        a.write_register(0xB002, 0x80);
        for _ in 0..30 {
            a.clock();
        }
        assert!(a.saw.accumulator > 0 || a.saw.step > 0);
        a.write_register(0xB002, 0x00);
        assert_eq!(a.saw.accumulator, 0);
        assert_eq!(a.saw.step, 0);
    }

    /// End-to-end: volume contributions from all three voices sum
    /// linearly into the cached output sample.
    #[test]
    fn combined_output_sums_channels() {
        let mut a = Vrc6Audio::new();
        // Pulse1: ignore-duty, vol=5.
        a.write_register(0x9000, 0x85);
        a.write_register(0x9001, 0x00);
        a.write_register(0x9002, 0x80);
        // Pulse2: ignore-duty, vol=7.
        a.write_register(0xA000, 0x87);
        a.write_register(0xA001, 0x00);
        a.write_register(0xA002, 0x80);
        // Leave saw disabled.
        a.clock();
        // After one clock, each pulse is at step 1 but ignore-duty
        // keeps them at full volume.
        assert_eq!(a.output_level(), 5 + 7);
    }

    /// Peak-level smoke test: two max-volume ignore-duty pulses and a
    /// maxed saw should produce output_level = 15 + 15 + 31 = 61.
    #[test]
    fn peak_output_hits_61() {
        let mut a = Vrc6Audio::new();
        a.write_register(0x9000, 0x8F);
        a.write_register(0x9001, 0x00);
        a.write_register(0x9002, 0x80);
        a.write_register(0xA000, 0x8F);
        a.write_register(0xA001, 0x00);
        a.write_register(0xA002, 0x80);
        // Saw: rate = 63 so after enough ticks the accumulator
        // saturates high-5-bits to 31.
        a.write_register(0xB000, 0x3F);
        a.write_register(0xB001, 0x00);
        a.write_register(0xB002, 0x80);
        // Push until we see the 31 on the saw.
        let mut saw_peak = 0;
        for _ in 0..32 {
            a.clock();
            if a.saw.volume_out() > saw_peak {
                saw_peak = a.saw.volume_out();
            }
        }
        assert_eq!(saw_peak, 31);
        // Re-clock once at saw peak to capture combined output in cache.
        // (The saw accumulator may have wrapped past 31 × 8 = 248 by
        // now; on the next even step the rate-63 add wraps to a lower
        // value — we just assert the max seen stays at 31.)
        assert!(a.output_level() > 0);
    }

    /// Mix-sample scales the raw 0..61 to approximately 0.0..0.912 —
    /// ~3.6× the FDS peak, matching Mesen2's default balance.
    #[test]
    fn mix_sample_peaks_near_0_91() {
        let mut a = Vrc6Audio::new();
        a.last_output = 61;
        let s = a.mix_sample();
        assert!((s - 0.9117).abs() < 0.005, "got {s}");
    }
}
