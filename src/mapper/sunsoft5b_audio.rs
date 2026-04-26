// SPDX-License-Identifier: GPL-3.0-or-later
//! Sunsoft 5B expansion audio — three YM2149F-compatible tone
//! channels.
//!
//! Used by exactly one commercial game: *Gimmick!* (Sunsoft, 1992).
//! The full YM2149F adds a 17-bit LFSR noise generator and a shared
//! envelope generator on top of the three tone channels; *Gimmick!*
//! never touches either, and Mesen2's reference implementation
//! likewise omits both. We follow Mesen2 — the reverse-engineered
//! noise / envelope behavior is documented at
//! <https://www.nesdev.org/wiki/Sunsoft_5B_audio> and is the obvious
//! place to extend if/when a homebrew game tickles them.
//!
//! ## Register interface (mapper-side)
//!
//! Two carry-through registers from the FME-7 / 5B mapper:
//! - `$C000-$DFFF`: select internal audio register (low 4 bits).
//!   Bits 4-7 disable writes when nonzero (AY-3-8910 family quirk —
//!   we silently honor it; nothing in the wild relies on it).
//! - `$E000-$FFFF`: write the byte to the selected internal register.
//!
//! ## Internal registers we honor
//!
//! | Reg | Field         | Effect                         |
//! |-----|---------------|--------------------------------|
//! | 0/1 | A period lo/hi (12-bit) | Tone period for channel A |
//! | 2/3 | B period lo/hi          | Channel B                  |
//! | 4/5 | C period lo/hi          | Channel C                  |
//! | 7   | `--CB Acba`             | Tone-disable bits for A/B/C (lower nibble); noise bits ignored (no noise) |
//! | 8/9/A | `---- VVVV` (low nibble)| 4-bit volume per channel (envelope bit ignored — no envelope) |
//!
//! All other registers are accepted (writes land in the buffer) but
//! have no audible effect.
//!
//! ## Clocking
//!
//! Per the wiki, the 5B "operates equivalent to a YM2149F with its
//! SEL pin held low" — which on top of the chip's natural clock
//! divider gives us "one period tick every 16 CPU cycles". Mesen2
//! models this as one channel update every other CPU cycle plus a
//! `period`-driven divider inside the channel. We do the same: a
//! `process_tick` toggle gates the per-channel update.
//!
//! ## Mix scale
//!
//! Mesen2's `NesSoundMixer.cpp:189` weight for `AudioChannel::Sunsoft5B`
//! is `15`, with no inline multiplier inside `Sunsoft5bAudio::UpdateOutputLevel`
//! — so per-raw-unit scale into our 0..1 mix space is `15 / 5018 ≈ 0.00299`.
//! With LUT[15] ≈ 177 and 3 channels, peak raw ≈ 531 → peak mix
//! sample ≈ 1.59. This is louder than FDS / VRC6, matching Mesen2's
//! intended balance for *Gimmick!*'s lead-instrument role.

const NUM_CHANNELS: usize = 3;
const NUM_REGISTERS: usize = 0x10;

const SUNSOFT5B_MIX_SCALE: f32 = 15.0 / 5018.0;

/// 16-entry volume LUT — the YM2149F's 4-bit volume scale is
/// logarithmic at 1.5 dB per *half*-step (so 3 dB per full step).
/// Built once at construction; values match Mesen2's `_volumeLut`
/// initialization byte-for-byte.
fn build_volume_lut() -> [u8; 16] {
    let mut lut = [0u8; 16];
    let mut output = 1.0_f64;
    for v in lut.iter_mut().skip(1) {
        // Two 1.5 dB steps = one 3 dB step.
        output *= 1.188_502_227_437_018_4_f64;
        output *= 1.188_502_227_437_018_4_f64;
        *v = output as u8;
    }
    lut
}

#[derive(Debug, Clone)]
pub struct Sunsoft5bAudio {
    volume_lut: [u8; 16],
    /// Currently-selected internal register, latched from `$C000`
    /// writes. Bit 7 high disables `$E000` writes (AY-3-8910 quirk).
    current_register: u8,
    write_disabled: bool,
    /// 16 internal YM2149F registers. Only entries 0..7, 8..0xA are
    /// audibly meaningful — 6, B, C, D, E, F land here too but go
    /// unused (no noise / envelope / I/O ports modelled).
    registers: [u8; NUM_REGISTERS],
    /// Per-channel down-counters reloaded with the channel's 12-bit
    /// period when they reach zero. Stepping happens every other
    /// CPU cycle (the `process_tick` toggle).
    timer: [i32; NUM_CHANNELS],
    /// 4-bit step counter per channel — tone is high for steps 0..7,
    /// low for steps 8..15.
    tone_step: [u8; NUM_CHANNELS],
    /// Toggles every CPU cycle; when true, channels update.
    process_tick: bool,
    /// Last computed sum (0..3 × LUT[15]). Returned by [`Self::output_level`]
    /// and used by [`Self::mix_sample`].
    last_output: i16,
}

impl Sunsoft5bAudio {
    pub fn new() -> Self {
        Self {
            volume_lut: build_volume_lut(),
            current_register: 0,
            write_disabled: false,
            registers: [0; NUM_REGISTERS],
            timer: [0; NUM_CHANNELS],
            tone_step: [0; NUM_CHANNELS],
            process_tick: false,
            last_output: 0,
        }
    }

    pub fn write_register(&mut self, addr: u16, value: u8) {
        match addr & 0xE000 {
            0xC000 => {
                self.current_register = value & 0x0F;
                self.write_disabled = (value & 0xF0) != 0;
            }
            0xE000 => {
                if !self.write_disabled {
                    self.registers[self.current_register as usize] = value;
                }
            }
            _ => {}
        }
    }

    pub fn clock(&mut self) {
        if self.process_tick {
            for ch in 0..NUM_CHANNELS {
                self.update_channel(ch);
            }
            self.update_output_level();
        }
        self.process_tick = !self.process_tick;
    }

    pub fn output_level(&self) -> i16 {
        self.last_output
    }

    pub fn mix_sample(&self) -> f32 {
        self.last_output as f32 * SUNSOFT5B_MIX_SCALE
    }

    fn period(&self, ch: usize) -> u16 {
        let lo = self.registers[ch * 2] as u16;
        let hi = (self.registers[ch * 2 + 1] & 0x0F) as u16;
        (hi << 8) | lo
    }

    /// Tone-enable bit for channel `ch`. Register $07 layout (low
    /// nibble): bit 0 = tone disable A, bit 1 = B, bit 2 = C. A
    /// `0` enables; a `1` disables.
    fn tone_enabled(&self, ch: usize) -> bool {
        ((self.registers[7] >> ch) & 0x01) == 0
    }

    /// 4-bit volume from registers $08-$0A. Bit 4 (envelope-enable)
    /// is intentionally ignored — no envelope generator modeled.
    fn volume(&self, ch: usize) -> u8 {
        self.volume_lut[(self.registers[8 + ch] & 0x0F) as usize]
    }

    fn update_channel(&mut self, ch: usize) {
        self.timer[ch] -= 1;
        if self.timer[ch] <= 0 {
            self.timer[ch] = self.period(ch) as i32;
            self.tone_step[ch] = (self.tone_step[ch] + 1) & 0x0F;
        }
    }

    fn update_output_level(&mut self) {
        let mut summed: i16 = 0;
        for ch in 0..NUM_CHANNELS {
            if self.tone_enabled(ch) && self.tone_step[ch] < 0x08 {
                summed += self.volume(ch) as i16;
            }
        }
        self.last_output = summed;
    }
}

impl Default for Sunsoft5bAudio {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_internal(a: &mut Sunsoft5bAudio, reg: u8, value: u8) {
        a.write_register(0xC000, reg);
        a.write_register(0xE000, value);
    }

    #[test]
    fn volume_lut_is_monotonic_and_matches_known_endpoints() {
        let lut = build_volume_lut();
        assert_eq!(lut[0], 0);
        for w in lut.windows(2).skip(1) {
            assert!(w[0] <= w[1], "non-monotonic at {:?}", w);
        }
        // LUT[15] should be ~177 (Mesen2 byte-for-byte). Allow ±1 to
        // tolerate any FP rounding.
        assert!((lut[15] as i16 - 177).abs() <= 1, "lut[15] = {}", lut[15]);
    }

    #[test]
    fn channel_a_steps_at_period_rate() {
        let mut a = Sunsoft5bAudio::new();
        // Period 4 (small), full volume on channel A, all others muted.
        write_internal(&mut a, 0x00, 0x04); // A period lo
        write_internal(&mut a, 0x01, 0x00); // A period hi
        write_internal(&mut a, 0x07, 0xFE); // bit 0 = tone-A enabled, others disabled (1)
        write_internal(&mut a, 0x08, 0x0F); // A volume = 15

        // process_tick alternates every CPU cycle: half the cycles
        // do nothing, the other half do channel-update + output.
        // After ~64 CPU cycles, the tone step should have advanced
        // from 0 toward something nonzero.
        let mut saw_high = false;
        let mut saw_low = false;
        for _ in 0..256 {
            a.clock();
            if a.tone_step[0] < 0x08 {
                saw_high = true;
            } else {
                saw_low = true;
            }
        }
        assert!(saw_high && saw_low, "channel A never toggled high/low");
    }

    #[test]
    fn tone_disabled_silences_channel() {
        let mut a = Sunsoft5bAudio::new();
        write_internal(&mut a, 0x00, 0x02);
        write_internal(&mut a, 0x07, 0xFF); // all tones disabled
        write_internal(&mut a, 0x08, 0x0F);
        for _ in 0..256 {
            a.clock();
        }
        assert_eq!(a.output_level(), 0);
    }

    #[test]
    fn three_channels_sum_into_output() {
        let mut a = Sunsoft5bAudio::new();
        // All three channels enabled with non-zero period and full
        // volume; clock for a long-enough run and verify that the
        // summed output exceeds any single-channel max.
        for ch in 0..3 {
            write_internal(&mut a, (ch * 2) as u8, 0x08);
            write_internal(&mut a, (ch * 2 + 1) as u8, 0x00);
            write_internal(&mut a, (8 + ch) as u8, 0x0F);
        }
        write_internal(&mut a, 0x07, 0b1111_1000); // tones A/B/C enabled
        let mut max_seen: i16 = 0;
        for _ in 0..2048 {
            a.clock();
            if a.output_level() > max_seen {
                max_seen = a.output_level();
            }
        }
        // Single-channel max ≈ 177; sum of three should exceed that.
        assert!(max_seen > 200, "max output {} suggests channels not summing", max_seen);
        // And it should not exceed 3 × LUT[15].
        let lut = build_volume_lut();
        assert!(max_seen <= 3 * lut[15] as i16);
    }

    #[test]
    fn write_disable_bit_blocks_e000_writes() {
        let mut a = Sunsoft5bAudio::new();
        // Set channel A volume to 0x0F via normal path.
        write_internal(&mut a, 0x08, 0x0F);
        assert_eq!(a.registers[0x08], 0x0F);
        // Re-select reg 8 with the disable bit set; subsequent
        // $E000 writes should be ignored.
        a.write_register(0xC000, 0x18); // bit 4 set = disable
        a.write_register(0xE000, 0x00);
        assert_eq!(a.registers[0x08], 0x0F);
        // Selecting again with disable bit clear re-enables.
        a.write_register(0xC000, 0x08);
        a.write_register(0xE000, 0x00);
        assert_eq!(a.registers[0x08], 0x00);
    }

    #[test]
    fn mix_sample_zero_when_silent() {
        let a = Sunsoft5bAudio::new();
        assert_eq!(a.mix_sample(), 0.0);
    }
}
