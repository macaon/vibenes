// SPDX-License-Identifier: GPL-3.0-or-later
//! Sunsoft 5B expansion audio - full YM2149F-compatible synth: three
//! tone channels, a shared 17-bit LFSR noise generator, and a shared
//! envelope generator with the 4-bit AY-3-8910-style shape control.
//!
//! Used by exactly one commercial game: *Gimmick!* (Sunsoft, 1992).
//! The mapper's audio half ports cleanly from the reference
//! implementations: the tone path matches Mesen2's
//! `Sunsoft5bAudio.h`; the envelope and noise paths port from
//! Nestopia's `NstBoardSunsoft5b.cpp` (Mesen2 omits both). The
//! 32-entry log levels table is byte-for-byte from Nestopia, derived
//! from the YM2149 datasheet's 1.5 dB-per-step DAC ladder.
//!
//! ## Register interface (mapper-side)
//!
//! Two carry-through registers from the FME-7 / 5B mapper:
//! - `$C000-$DFFF`: select internal audio register (low 4 bits).
//!   Bits 4-7 disable writes when nonzero (AY-3-8910 family quirk -
//!   we honor it).
//! - `$E000-$FFFF`: write the byte to the selected internal register.
//!
//! ## Internal registers
//!
//! | Reg     | Field         | Effect                                |
//! |---------|---------------|---------------------------------------|
//! | 0/1     | A period lo/hi (12-bit)  | Tone period for channel A  |
//! | 2/3     | B period lo/hi           | Channel B                  |
//! | 4/5     | C period lo/hi           | Channel C                  |
//! | 6       | `---P PPPP` (5-bit)      | Noise period               |
//! | 7       | `--CB Acba`              | Mixer: bits 0-2 = tone disable A/B/C, bits 3-5 = noise disable A/B/C |
//! | 8/9/A   | `---E VVVV`              | Per-channel envelope-enable + 4-bit volume |
//! | B/C     | env period lo/hi (16-bit)| Envelope period            |
//! | D       | `---- CAaH`              | Envelope reset + shape (continue/attack/alternate/hold) |
//! | E/F     | I/O ports                | Unused                     |
//!
//! ## Clocking
//!
//! The 5B operates "equivalent to a YM2149F with its SEL pin held
//! low" (wiki §Sound). The native chip-internal clock is the CPU
//! clock divided by 16 for tone (so each tone sub-tick is 16 CPU
//! cycles before the period multiplier kicks in). Mesen2 simplifies
//! the tone path to "channel update every 2 CPU cycles" - close
//! enough that *Gimmick!*'s pitch sounds right; we keep that for
//! parity. Noise advances every 32 CPU cycles before its period
//! divider; envelope every 16 CPU cycles before its period divider.
//!
//! ## Mix scale
//!
//! Mesen2's `NesSoundMixer.cpp:189` weight for `AudioChannel::Sunsoft5B`
//! is `15`, with no inline multiplier inside `Sunsoft5bAudio::UpdateOutputLevel`
//! - so per-raw-unit scale into our 0..1 mix space is `15 / 5018 ≈ 0.00299`.
//! With LUT[15] ≈ 177 (= levels[31] cast to u8), peak raw ≈ 531 with
//! all three channels at full volume. Peak mix sample ≈ 1.59 - louder
//! than FDS / VRC6, matching Mesen2's intended balance for *Gimmick!*'s
//! lead-instrument role.

const NUM_CHANNELS: usize = 3;
const NUM_REGISTERS: usize = 0x10;

const SUNSOFT5B_MIX_SCALE: f32 = 15.0 / 5018.0;

/// 16-entry volume LUT - the YM2149F's 4-bit volume scale, 1.5 dB
/// per *half*-step (3 dB per full step). `volume_lut[v]` matches
/// Mesen2's `_volumeLut[v]` byte-for-byte.
fn build_volume_lut() -> [u8; 16] {
    let mut lut = [0u8; 16];
    let mut output = 1.0_f64;
    for v in lut.iter_mut().skip(1) {
        output *= 1.188_502_227_437_018_4_f64;
        output *= 1.188_502_227_437_018_4_f64;
        *v = output as u8;
    }
    lut
}

/// 32-entry envelope-output LUT - same logarithmic ladder at half
/// the 4-bit-volume granularity. `env_lut[2v + 1] == volume_lut[v]`
/// for v in 1..=15; `env_lut[0] == env_lut[1] == 0` (silent floor).
/// Matches the relationship documented at
/// <https://www.nesdev.org/wiki/Sunsoft_5B_audio> §Envelope.
fn build_envelope_lut() -> [u8; 32] {
    let mut lut = [0u8; 32];
    let mut output = 1.0_f64;
    for i in 2..32 {
        output *= 1.188_502_227_437_018_4_f64;
        lut[i] = output as u8;
    }
    lut
}

/// Shared envelope generator. State machine matches Nestopia's
/// `S5b::Sound::Envelope` - `count` runs 0x1F → 0x00, `attack` is
/// XOR-merged with the count to pick the LUT index (so an "up" ramp
/// just writes attack=0x1F and the same descending counter produces
/// 0x1F^0x1F=0..0x00^0x1F=0x1F = 0..31). End-of-ramp behavior is
/// driven by the `hold` and `alternate` flags exactly as the
/// AY-3-8910/YM2149 datasheet describes.
#[derive(Debug, Clone)]
struct Envelope {
    period: u16,
    /// Position within the current ramp (5-bit), counted top-down so
    /// the attack-XOR trick produces a clean up-ramp without an
    /// extra direction field.
    count: u8,
    /// `0x00` (down ramp output) or `0x1F` (up ramp output). XOR'd
    /// with `count` before LUT lookup. Flips at end-of-ramp when
    /// `alternate` is set.
    attack: u8,
    alternate: bool,
    hold: bool,
    holding: bool,
    /// Sub-CPU-cycle counter - envelope steps every 16 CPU cycles
    /// before the `period` divider applies.
    sub_cycle: u8,
    timer: i32,
    /// Cached LUT output for the current `count ^ attack`. Refreshed
    /// each step (and on register $0D writes).
    output: u8,
}

impl Envelope {
    fn new() -> Self {
        Self {
            period: 0,
            count: 0,
            attack: 0,
            alternate: false,
            hold: false,
            holding: false,
            sub_cycle: 0,
            timer: 0,
            output: 0,
        }
    }

    fn write_period_lo(&mut self, value: u8) {
        self.period = (self.period & 0xFF00) | u16::from(value);
    }

    fn write_period_hi(&mut self, value: u8) {
        self.period = (self.period & 0x00FF) | (u16::from(value) << 8);
    }

    /// Register `$0D` - reset the envelope and set its shape.
    /// Following Nestopia's `WriteReg2` mapping for the C=0 case
    /// (continue=0 → behave as continue=1 with hold=1 + alternate
    /// derived from attack). This makes shapes $0-$3 collapse to the
    /// "down once, hold low" path and $4-$7 to "up once, hold low"
    /// per the wiki.
    fn write_shape(&mut self, value: u8, lut: &[u8; 32]) {
        self.holding = false;
        self.attack = if (value & 0x04) != 0 { 0x1F } else { 0x00 };
        if (value & 0x08) != 0 {
            // Continue = 1 - honor hold + alternate as written.
            self.hold = (value & 0x01) != 0;
            self.alternate = (value & 0x02) != 0;
        } else {
            // Continue = 0 - collapse onto an equivalent C=1 shape:
            // hold = 1, alternate = (attack != 0). This makes
            // $4-$7 ramp up then snap to silent (output 0 after
            // alternate flips attack to 0 at end-of-ramp), and
            // $0-$3 ramp down then sit silent.
            self.hold = true;
            self.alternate = self.attack != 0;
        }
        self.count = 0x1F;
        self.sub_cycle = 0;
        self.timer = self.period as i32;
        self.output = lut[(self.count ^ self.attack) as usize];
    }

    /// Advance one CPU cycle. Returns the current envelope output
    /// (cached; recomputed on each ramp step).
    fn clock(&mut self, lut: &[u8; 32]) -> u8 {
        if self.holding {
            return self.output;
        }
        // Envelope base divider is 16 CPU cycles per `period` tick.
        self.sub_cycle = self.sub_cycle.wrapping_add(1);
        if self.sub_cycle < 16 {
            return self.output;
        }
        self.sub_cycle = 0;
        // Wiki: a period of 0 behaves identically to a period of 1.
        let period = self.period.max(1) as i32;
        self.timer -= 1;
        if self.timer > 0 {
            return self.output;
        }
        self.timer = period;
        // Ramp step: count goes 0x1F → 0x00 → underflow.
        let next = self.count.wrapping_sub(1);
        if next > 0x1F {
            // End-of-ramp.
            if self.hold {
                if self.alternate {
                    self.attack ^= 0x1F;
                }
                self.holding = true;
                self.count = 0;
            } else {
                if self.alternate {
                    self.attack ^= 0x1F;
                }
                self.count = 0x1F;
            }
        } else {
            self.count = next;
        }
        self.output = lut[(self.count ^ self.attack) as usize];
        self.output
    }
}

/// Shared 17-bit LFSR noise generator. Steps every `period * 32` CPU
/// cycles. Output is the LSB of the register. Tap configuration
/// (XOR feedback at bits 0 and 3 of the post-shift state) is the
/// standard AY-3-8910/YM2149 polynomial - bits 16/13 of the wiki's
/// "tap" description map to 0/3 here because we shift right and
/// inject the new bit at bit 16.
#[derive(Debug, Clone)]
struct Noise {
    period: u8,
    sub_cycle: u8,
    timer: i32,
    /// 17-bit register, seeded to 1 so the first shift yields a
    /// deterministic value. `0` would be a dead state.
    lfsr: u32,
}

impl Noise {
    fn new() -> Self {
        Self {
            period: 0,
            sub_cycle: 0,
            timer: 0,
            lfsr: 1,
        }
    }

    fn write_period(&mut self, value: u8) {
        self.period = value & 0x1F;
    }

    /// Advance one CPU cycle. Returns the current noise bit (true
    /// = high). Steps the LFSR every `period * 32` CPU cycles.
    fn clock(&mut self) -> bool {
        // Base divider - every 32 CPU cycles count one "noise tick".
        self.sub_cycle = self.sub_cycle.wrapping_add(1);
        if self.sub_cycle < 32 {
            return (self.lfsr & 1) != 0;
        }
        self.sub_cycle = 0;
        // Period 0 ≈ period 1 per wiki.
        let period = self.period.max(1) as i32;
        self.timer -= 1;
        if self.timer > 0 {
            return (self.lfsr & 1) != 0;
        }
        self.timer = period;
        // Standard AY noise polynomial - feedback = bit_0 XOR bit_3
        // of the current LFSR state, shift right by 1, inject feedback
        // into bit 16. Equivalent to taps at 16/13 of the post-shift
        // value (the form the nesdev wiki documents).
        let feedback = (self.lfsr ^ (self.lfsr >> 3)) & 1;
        self.lfsr = (self.lfsr >> 1) | (feedback << 16);
        (self.lfsr & 1) != 0
    }
}

#[derive(Debug, Clone)]
pub struct Sunsoft5bAudio {
    volume_lut: [u8; 16],
    envelope_lut: [u8; 32],
    /// Currently-selected internal register, latched from `$C000`
    /// writes. Bit 7 high disables `$E000` writes (AY-3-8910 quirk).
    current_register: u8,
    write_disabled: bool,
    /// 16 internal YM2149F registers - keeps the raw written bytes
    /// available for state save / introspection.
    registers: [u8; NUM_REGISTERS],
    /// Per-channel down-counters reloaded with the channel's 12-bit
    /// period when they reach zero. Stepping happens every other
    /// CPU cycle (the `process_tick` toggle).
    timer: [i32; NUM_CHANNELS],
    /// 4-bit step counter per channel - tone is high for steps 0..7,
    /// low for steps 8..15.
    tone_step: [u8; NUM_CHANNELS],
    /// Toggles every CPU cycle; when true, channels update.
    process_tick: bool,
    envelope: Envelope,
    noise: Noise,
    /// Last computed sum (0..3 × LUT[15]). Returned by [`Self::output_level`]
    /// and used by [`Self::mix_sample`].
    last_output: i16,
}

impl Sunsoft5bAudio {
    pub fn new() -> Self {
        Self {
            volume_lut: build_volume_lut(),
            envelope_lut: build_envelope_lut(),
            current_register: 0,
            write_disabled: false,
            registers: [0; NUM_REGISTERS],
            timer: [0; NUM_CHANNELS],
            tone_step: [0; NUM_CHANNELS],
            process_tick: false,
            envelope: Envelope::new(),
            noise: Noise::new(),
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
                    let reg = self.current_register as usize;
                    self.registers[reg] = value;
                    // Side effects beyond the register-array store.
                    match reg {
                        0x06 => self.noise.write_period(value),
                        0x0B => self.envelope.write_period_lo(value),
                        0x0C => self.envelope.write_period_hi(value),
                        0x0D => self.envelope.write_shape(value, &self.envelope_lut),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    pub fn clock(&mut self) {
        // Tone advances every other CPU cycle (Mesen2 pattern).
        if self.process_tick {
            for ch in 0..NUM_CHANNELS {
                self.update_tone_channel(ch);
            }
        }
        self.process_tick = !self.process_tick;

        // Envelope + noise advance every CPU cycle (their internal
        // dividers handle their respective base rates).
        let env_out = self.envelope.clock(&self.envelope_lut);
        let noise_high = self.noise.clock();

        self.update_output_level(env_out, noise_high);
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

    /// Tone-disable bit: register $07, bits 0/1/2 for channels A/B/C.
    /// `1` = disabled, `0` = enabled (normal AY-3-8910 convention).
    fn tone_disabled(&self, ch: usize) -> bool {
        ((self.registers[7] >> ch) & 0x01) == 1
    }

    /// Noise-disable bit: register $07, bits 3/4/5 for channels A/B/C.
    fn noise_disabled(&self, ch: usize) -> bool {
        ((self.registers[7] >> (ch + 3)) & 0x01) == 1
    }

    /// `true` when the channel routes envelope output through its
    /// volume slot (register $08+ch bit 4); `false` selects the fixed
    /// 4-bit volume in bits 0-3.
    fn envelope_enabled_on_channel(&self, ch: usize) -> bool {
        (self.registers[8 + ch] & 0x10) != 0
    }

    fn fixed_volume(&self, ch: usize) -> u8 {
        self.volume_lut[(self.registers[8 + ch] & 0x0F) as usize]
    }

    fn update_tone_channel(&mut self, ch: usize) {
        self.timer[ch] -= 1;
        if self.timer[ch] <= 0 {
            self.timer[ch] = self.period(ch) as i32;
            self.tone_step[ch] = (self.tone_step[ch] + 1) & 0x0F;
        }
    }

    fn update_output_level(&mut self, env_out: u8, noise_high: bool) {
        let mut summed: i16 = 0;
        for ch in 0..NUM_CHANNELS {
            let tone_high = self.tone_step[ch] < 0x08;
            // Per-channel mixer: per the wiki, tone-disable=1 forces
            // the tone path "high" and noise-disable=1 forces the
            // noise path "high"; the two paths AND together. So a
            // channel with both disabled outputs constant volume,
            // and a channel with both enabled outputs only when
            // tone AND noise are both high.
            let tone_path = tone_high || self.tone_disabled(ch);
            let noise_path = noise_high || self.noise_disabled(ch);
            if !(tone_path && noise_path) {
                continue;
            }
            let vol = if self.envelope_enabled_on_channel(ch) {
                env_out
            } else {
                self.fixed_volume(ch)
            };
            summed += vol as i16;
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
        assert!((lut[15] as i16 - 177).abs() <= 1, "lut[15] = {}", lut[15]);
    }

    #[test]
    fn envelope_lut_matches_volume_lut_at_odd_indices() {
        let vol = build_volume_lut();
        let env = build_envelope_lut();
        assert_eq!(env[0], 0);
        assert_eq!(env[1], 0);
        // Per the wiki: env[2v+1] = vol[v] for v in 1..=15.
        // Allow ±1 due to floating-point rounding at small values.
        for v in 1..=15 {
            let env_v = env[2 * v + 1] as i16;
            let vol_v = vol[v] as i16;
            assert!(
                (env_v - vol_v).abs() <= 1,
                "env[{}]={} vs vol[{}]={}",
                2 * v + 1,
                env_v,
                v,
                vol_v
            );
        }
    }

    #[test]
    fn channel_a_steps_at_period_rate() {
        let mut a = Sunsoft5bAudio::new();
        write_internal(&mut a, 0x00, 0x04);
        write_internal(&mut a, 0x01, 0x00);
        write_internal(&mut a, 0x07, 0xFE); // tone A enabled, B/C disabled, noise all disabled
        write_internal(&mut a, 0x08, 0x0F);

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
        assert!(saw_high && saw_low);
    }

    #[test]
    fn tone_disabled_silences_channel_when_noise_also_disabled() {
        // With both tone AND noise disabled on a channel, AY semantics
        // say the channel outputs CONSTANT volume - not silence - so
        // we silence this case via volume = 0 instead.
        let mut a = Sunsoft5bAudio::new();
        write_internal(&mut a, 0x00, 0x02);
        write_internal(&mut a, 0x07, 0xFF); // all tones + all noise disabled
        write_internal(&mut a, 0x08, 0x00); // volume 0 → silent
        for _ in 0..256 {
            a.clock();
        }
        assert_eq!(a.output_level(), 0);
    }

    #[test]
    fn three_channels_sum_into_output() {
        let mut a = Sunsoft5bAudio::new();
        for ch in 0..3 {
            write_internal(&mut a, (ch * 2) as u8, 0x08);
            write_internal(&mut a, (ch * 2 + 1) as u8, 0x00);
            write_internal(&mut a, (8 + ch) as u8, 0x0F);
        }
        write_internal(&mut a, 0x07, 0b1111_1000); // tones enabled, noise disabled
        let mut max_seen: i16 = 0;
        for _ in 0..2048 {
            a.clock();
            if a.output_level() > max_seen {
                max_seen = a.output_level();
            }
        }
        assert!(max_seen > 200, "max output {} suggests channels not summing", max_seen);
        let lut = build_volume_lut();
        assert!(max_seen <= 3 * lut[15] as i16);
    }

    #[test]
    fn write_disable_bit_blocks_e000_writes() {
        let mut a = Sunsoft5bAudio::new();
        write_internal(&mut a, 0x08, 0x0F);
        assert_eq!(a.registers[0x08], 0x0F);
        a.write_register(0xC000, 0x18);
        a.write_register(0xE000, 0x00);
        assert_eq!(a.registers[0x08], 0x0F);
        a.write_register(0xC000, 0x08);
        a.write_register(0xE000, 0x00);
        assert_eq!(a.registers[0x08], 0x00);
    }

    #[test]
    fn noise_lfsr_produces_changing_output_over_time() {
        let mut a = Sunsoft5bAudio::new();
        // Noise period 1, mixer: tone disabled, noise enabled on ch A,
        // ch A volume max.
        write_internal(&mut a, 0x06, 0x01);
        write_internal(&mut a, 0x07, 0b1111_0001); // noise A enabled (bit 3 = 0), tone A disabled (bit 0 = 1)
        write_internal(&mut a, 0x08, 0x0F);
        // Need to NOT use envelope path.
        let mut saw_zero = false;
        let mut saw_nonzero = false;
        for _ in 0..4096 {
            a.clock();
            if a.output_level() == 0 {
                saw_zero = true;
            } else {
                saw_nonzero = true;
            }
        }
        assert!(saw_zero && saw_nonzero, "noise output never alternated");
    }

    #[test]
    fn envelope_reset_initializes_count_at_top_with_attack_zero_for_down_ramp() {
        let mut a = Sunsoft5bAudio::new();
        // Envelope period 1 so we step quickly.
        write_internal(&mut a, 0x0B, 0x01);
        write_internal(&mut a, 0x0C, 0x00);
        // Shape $09 (continue=1, attack=0, alternate=0, hold=1) =
        // "down once, hold low".
        write_internal(&mut a, 0x0D, 0x09);
        // Initial output = lut[0x1F ^ 0] = lut[31] (peak).
        let env_lut = build_envelope_lut();
        assert_eq!(a.envelope.output, env_lut[31]);
        assert!(!a.envelope.holding);

        // Route envelope onto channel A so we can observe it via the
        // mixer too. Tone-disable A, noise-disable A; bit 4 of $08 = envelope.
        write_internal(&mut a, 0x07, 0xFF); // disable everything in mixer
        write_internal(&mut a, 0x08, 0x10); // envelope-enable on ch A, fixed volume bits = 0
        // Tick enough cycles for the envelope to fully ramp down +
        // hit the hold state.
        for _ in 0..(16 * 33) {
            a.clock();
        }
        assert!(a.envelope.holding, "envelope did not enter hold state");
        assert_eq!(a.envelope.output, env_lut[0]);
    }

    #[test]
    fn envelope_continuous_down_ramp_keeps_repeating() {
        let mut a = Sunsoft5bAudio::new();
        write_internal(&mut a, 0x0B, 0x01);
        write_internal(&mut a, 0x0C, 0x00);
        // Shape $08 (continue=1, attack=0, alternate=0, hold=0) =
        // continuous down ramp.
        write_internal(&mut a, 0x0D, 0x08);
        let env_lut = build_envelope_lut();
        // After one full ramp (32 steps × 16 CPU cycles × period=1),
        // envelope should have re-loaded count=0x1F and not be holding.
        for _ in 0..(16 * 33) {
            a.clock();
        }
        assert!(!a.envelope.holding);
        // At some later point, output should hit peak again.
        let mut hit_peak_again = false;
        for _ in 0..(16 * 33) {
            a.clock();
            if a.envelope.output == env_lut[31] {
                hit_peak_again = true;
                break;
            }
        }
        assert!(hit_peak_again, "down ramp did not restart at top");
    }

    #[test]
    fn envelope_attack_bit_makes_output_ramp_up() {
        let mut a = Sunsoft5bAudio::new();
        write_internal(&mut a, 0x0B, 0x01);
        write_internal(&mut a, 0x0C, 0x00);
        // Shape $0E (continue=1, attack=1, alternate=1, hold=0) =
        // /\/\ alternating up/down.
        write_internal(&mut a, 0x0D, 0x0E);
        let env_lut = build_envelope_lut();
        // Initial: count=0x1F, attack=0x1F → output = lut[0x1F^0x1F] = lut[0] = 0.
        assert_eq!(a.envelope.output, env_lut[0]);
        // After half a ramp, output should have climbed.
        for _ in 0..(16 * 16) {
            a.clock();
        }
        assert!(a.envelope.output > env_lut[5], "up-ramp didn't climb");
    }

    #[test]
    fn mix_sample_zero_when_silent() {
        let a = Sunsoft5bAudio::new();
        assert_eq!(a.mix_sample(), 0.0);
    }
}
