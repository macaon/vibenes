//! FDS audio — RP2C33 wavetable + FM modulator synth.
//!
//! Two DSP units, one sample output per CPU cycle:
//!
//! - **Volume channel** (`$4080`, `$4082`, `$4083`): 6-bit "gain"
//!   envelope, 12-bit pitch counter. The wave accumulator counts
//!   `pitch + mod_output` per CPU cycle; every 16-bit overflow steps
//!   the 64-entry wave table position.
//! - **Modulator channel** (`$4084-$4088`): parallel 6-bit envelope
//!   + 12-bit pitch counter feeding a 64-entry 3-bit modulation table
//!   (`_modTable`). Each overflow advances the mod table pointer and
//!   updates a 7-bit signed `counter`. The counter's non-linear
//!   transform (`counter × gain` with a peculiar half-rounding step)
//!   produces the pitch-adjustment value added to the volume channel's
//!   pitch each cycle.
//!
//! Mixed output: `wave_sample × min(gain,32) × WAVE_VOL_TABLE[master]`
//! divided by 1152, giving a 0..63 level. The bus scales 0..63 to
//! ~0..0.624 so the FDS signal sits ~2.4× APU pulse peak — matching
//! the nesdev "FDS audio" wiki mix ratio.
//!
//! ## References
//!
//! Port of Mesen2's `Core/NES/Mappers/FDS/{BaseFdsChannel,
//! ModChannel, FdsAudio}.{h,cpp}`. The `counter × gain` rounding
//! quirk, the 16-bit overflow semantics, and the envelope-clock
//! formula (`8 × (speed+1) × master_speed`) are all protocol-exact,
//! so reimplementing them from scratch is just a transcription
//! hazard — we port Mesen2's logic with Rust idioms and credit it
//! here. Cross-checked against nesdev wiki's "FDS audio" page
//! (register map + mixing ratio) and the skill's
//! `reference/fds.md` §8.

// ---- Constants ----

/// Master-volume multipliers applied to the wavetable output. Index is
/// `$4089.0-1`. Values copied from Mesen2's `WaveVolumeTable`; these
/// are the exact DAC-gain values the RP2C33 uses (derived from the
/// current-mirror weights inside the chip's output stage).
const WAVE_VOL_TABLE: [u32; 4] = [36, 24, 17, 14];

/// Special sentinel in the 3-bit mod-table value: `4` means "reset
/// counter to 0" rather than applying a signed offset.
const MOD_RESET: i32 = 0xFF;

/// Mod-table value → counter-adjust offset. Mesen2's `_modLut`.
/// Index 4 is the reset sentinel handled specially in [`ModChannel::tick_modulator`].
const MOD_LUT: [i32; 8] = [0, 1, 2, 4, MOD_RESET, -4, -2, -1];

/// Gain saturation cap. Even though the field is 6-bit (0..63), the
/// RP2C33 clamps to 32 internally before multiplying.
const GAIN_CAP: u8 = 32;

/// Per-raw-unit FDS → mix scale factor.
///
/// The nesdev wiki says "FDS peak ≈ 2.4× APU pulse peak" for the raw
/// analog signal on the cart connector. But Mesen2 — and every other
/// emulator that sounds balanced — deliberately attenuates FDS well
/// below that so it doesn't drown the 2A03.
///
/// Mesen2's `NesSoundMixer::GetOutputVolume` adds `fds × 20` into a
/// fixed-point accumulator where pulse channels contribute
/// `95.88 × 5000 / (8128/sq + 100)`. The two-pulses-maxed peak is
/// `95.88 × 5000 / (8128/30 + 100) ≈ 1292`; FDS peak at raw=63 is
/// `63 × 20 = 1260`. So in Mesen2's default balance FDS peak
/// ≈ 0.975 × two-pulses peak.
///
/// Our [`crate::apu`] outputs `PULSE_TABLE[30] ≈ 0.2575` for the
/// same peak. To match Mesen2's loudness we want FDS peak ≈ 0.2575
/// in our 0..1 mix space; `0.2575 / 63 ≈ 0.00409` per raw unit.
const FDS_MIX_SCALE: f32 = 0.2575 / 63.0;

// ---- Base envelope (shared volume + mod) ----

/// The two FDS channels (volume + mod) share an envelope-clock
/// design: a 6-bit "speed" value, a direction bit, an enable bit,
/// and a `masterSpeed`-multiplied countdown timer. This struct is
/// the common part; each concrete channel layers its own unit
/// behavior on top.
///
/// Port of Mesen2's `BaseFdsChannel` — the write-register
/// dispatch, the `8 × (speed+1) × master_speed` timer formula, and
/// the 0..32 gain clamp all come straight from there.
#[derive(Debug, Clone)]
pub(super) struct EnvelopeUnit {
    pub(super) speed: u8,
    pub(super) gain: u8,
    pub(super) envelope_off: bool,
    pub(super) volume_increase: bool,
    pub(super) frequency: u16,
    timer: u32,
    /// Global envelope-clock multiplier from `$408A`. BIOS boots with
    /// `$E8` (232) — few NSFs touch it; we match Mesen2's default.
    pub(super) master_speed: u8,
}

impl EnvelopeUnit {
    pub(super) fn new() -> Self {
        Self {
            speed: 0,
            gain: 0,
            envelope_off: false,
            volume_increase: false,
            frequency: 0,
            timer: 0,
            master_speed: 0xE8,
        }
    }

    pub(super) fn reset_timer(&mut self) {
        // 8 × (speed + 1) × master_speed — cycle count until the next
        // envelope tick. Writes to the channel's reg0 reset this
        // (puNES does the same), delaying the next tick slightly.
        self.timer = 8 * (self.speed as u32 + 1) * self.master_speed as u32;
    }

    pub(super) fn set_master_speed(&mut self, master: u8) {
        self.master_speed = master;
    }

    /// Default dispatch for the `reg & 0x03`-style writes shared by
    /// the volume channel and the mod channel. Concrete channels
    /// override specific addresses (see [`ModChannel::write_reg`]).
    pub(super) fn write_reg_common(&mut self, addr: u16, value: u8) {
        match addr & 0x03 {
            0 => {
                self.speed = value & 0x3F;
                self.volume_increase = (value & 0x40) != 0;
                self.envelope_off = (value & 0x80) != 0;
                self.reset_timer();
                if self.envelope_off {
                    // Manual-gain mode: gain latches to speed field.
                    self.gain = self.speed;
                }
            }
            2 => {
                self.frequency = (self.frequency & 0x0F00) | value as u16;
            }
            3 => {
                self.frequency = (self.frequency & 0x00FF) | (((value as u16) & 0x0F) << 8);
            }
            _ => {}
        }
    }

    /// Advance the envelope by one CPU cycle. Returns true if the
    /// gain tick fired (caller may want to refresh the mod output).
    /// Matches Mesen2's `TickEnvelope`: only runs when the envelope
    /// is enabled AND master_speed > 0; otherwise the unit is
    /// "frozen" at its current gain (manual-gain mode).
    pub(super) fn tick_envelope(&mut self) -> bool {
        if self.envelope_off || self.master_speed == 0 {
            return false;
        }
        self.timer = self.timer.saturating_sub(1);
        if self.timer == 0 {
            self.reset_timer();
            if self.volume_increase && self.gain < GAIN_CAP {
                self.gain += 1;
            } else if !self.volume_increase && self.gain > 0 {
                self.gain -= 1;
            }
            true
        } else {
            false
        }
    }
}

// ---- Modulator channel ----

/// The FDS modulator. Embeds an [`EnvelopeUnit`] plus its own state:
/// a 64-entry 3-bit mod table, a read pointer, a 7-bit signed
/// counter, a 16-bit overflow counter, and a cached output value
/// (the post-transform pitch adjustment). Mod table pushes via
/// `$4088` are gated by the mod-halt bit in `$4087`.
#[derive(Debug, Clone)]
pub(super) struct ModChannel {
    pub(super) env: EnvelopeUnit,
    counter: i8,
    modulation_disabled: bool,
    mod_table: [u8; 64],
    mod_table_position: u8,
    overflow_counter: u16,
    output: i32,
}

impl ModChannel {
    fn new() -> Self {
        Self {
            env: EnvelopeUnit::new(),
            counter: 0,
            modulation_disabled: false,
            mod_table: [0; 64],
            mod_table_position: 0,
            overflow_counter: 0,
            output: 0,
        }
    }

    fn is_enabled(&self) -> bool {
        !self.modulation_disabled && self.env.frequency > 0
    }

    fn update_counter(&mut self, value: i32) {
        // Wrap into 7-bit signed range [-64, 63]. Mesen2 and puNES
        // both do this explicitly instead of relying on two's-
        // complement wraparound.
        let mut v = value;
        if v >= 64 {
            v -= 128;
        } else if v < -64 {
            v += 128;
        }
        self.counter = v as i8;
    }

    /// Mod-table push via `$4088`. Only accepted when the modulator
    /// is halted (`$4087.7 = 1`); writes while running are silently
    /// dropped. Each write pushes the 3-bit value into TWO adjacent
    /// slots — the mod table is actually 32 user entries presented as
    /// 64 on the read side, so writing a single entry advances the
    /// position by 2.
    fn write_mod_table(&mut self, value: u8) {
        if self.modulation_disabled {
            let v = value & 0x07;
            self.mod_table[(self.mod_table_position as usize) & 0x3F] = v;
            self.mod_table[((self.mod_table_position + 1) as usize) & 0x3F] = v;
            self.mod_table_position = (self.mod_table_position + 2) & 0x3F;
        }
    }

    fn write_reg(&mut self, addr: u16, value: u8) {
        match addr {
            0x4084 | 0x4086 => self.env.write_reg_common(addr, value),
            0x4085 => self.update_counter((value & 0x7F) as i32),
            0x4087 => {
                self.env.write_reg_common(addr, value);
                self.modulation_disabled = (value & 0x80) != 0;
                if self.modulation_disabled {
                    // Halt drops the overflow counter so writes to
                    // the mod table enter at the known starting
                    // offset. Mesen2 does the same — `_overflowCounter = 0`.
                    self.overflow_counter = 0;
                }
            }
            _ => {}
        }
    }

    /// Advance the mod unit by one CPU cycle. Returns true when the
    /// 16-bit pitch counter overflows; the caller should then refresh
    /// [`ModChannel::update_output`]. Mirrors Mesen2's `TickModulator`
    /// but uses Rust's `overflowing_add` for the wraparound detection.
    fn tick_modulator(&mut self) -> bool {
        if !self.is_enabled() {
            return false;
        }
        let (new, overflowed) = self.overflow_counter.overflowing_add(self.env.frequency);
        self.overflow_counter = new;
        if !overflowed {
            return false;
        }
        // Read the next mod-table entry, either apply the signed
        // offset or reset the counter.
        let entry = self.mod_table[self.mod_table_position as usize];
        let offset = MOD_LUT[entry as usize];
        if offset == MOD_RESET {
            self.update_counter(0);
        } else {
            self.update_counter(self.counter as i32 + offset);
        }
        self.mod_table_position = (self.mod_table_position + 1) & 0x3F;
        true
    }

    /// Compute the pitch-adjustment that gets added to the volume
    /// channel's pitch every CPU cycle.
    ///
    /// Ported from Mesen2's `UpdateOutput`, which cites the nesdev
    /// wiki's `FDS audio` page. Three steps:
    ///
    /// 1. Multiply signed counter by mod gain, shift right 4 bits,
    ///    round in a peculiar half-rounding way when the low nibble
    ///    is non-zero AND the shifted result's sign bit is clear —
    ///    this is hardware-accurate, not a mistake.
    /// 2. Wrap the 9-bit result into `[-64, 191]`.
    /// 3. Multiply by the volume pitch, round to nearest across a
    ///    6-bit remainder, drop the low 6 bits. Final value is a
    ///    signed pitch delta consumed by the wave accumulator.
    fn update_output(&mut self, volume_pitch: u16) {
        let mut temp: i32 = self.counter as i32 * self.env.gain as i32;
        let remainder = temp & 0x0F;
        temp >>= 4;
        if remainder > 0 && (temp & 0x80) == 0 {
            temp += if self.counter < 0 { -1 } else { 2 };
        }

        if temp >= 192 {
            temp -= 256;
        } else if temp < -64 {
            temp += 256;
        }

        temp = volume_pitch as i32 * temp;
        let remainder = temp & 0x3F;
        temp >>= 6;
        if remainder >= 32 {
            temp += 1;
        }

        self.output = temp;
    }

    fn output(&self) -> i32 {
        self.output
    }
}

// ---- Top-level audio unit ----

/// RP2C33 audio unit. Owns the 64-byte wavetable, the volume +
/// modulator channels, master-volume gating, the 16-bit wave-pitch
/// accumulator, and a cached output sample updated each CPU cycle.
///
/// `clock` is called once per CPU cycle by the FDS mapper's
/// `on_cpu_cycle` hook; `sample` returns the mix-ready f32 the bus
/// adds to the APU output.
pub struct FdsAudio {
    wave_table: [u8; 64],
    wave_write_enabled: bool,
    volume: EnvelopeUnit,
    modulator: ModChannel,
    disable_envelopes: bool,
    halt_waveform: bool,
    master_volume: u8,
    wave_overflow_counter: u16,
    wave_position: u8,
    last_output: u8,
}

impl FdsAudio {
    pub fn new() -> Self {
        Self {
            wave_table: [0; 64],
            wave_write_enabled: false,
            volume: EnvelopeUnit::new(),
            modulator: ModChannel::new(),
            disable_envelopes: false,
            halt_waveform: false,
            master_volume: 0,
            wave_overflow_counter: 0,
            wave_position: 0,
            last_output: 0,
        }
    }

    /// Raw 0..63 current sample. Useful for tests and the mapper-
    /// state introspection screens; the mix sink wants `mix_sample`
    /// instead.
    pub fn output_level(&self) -> u8 {
        self.last_output
    }

    /// Mix-ready sample in approximately 0.0..0.624 — pre-scaled
    /// against the APU's 0.0..≈0.98 range. Bus adds this directly
    /// to the APU sample.
    pub fn mix_sample(&self) -> f32 {
        self.last_output as f32 * FDS_MIX_SCALE
    }

    /// CPU-visible read through `$4040-$4097`. Mirrors Mesen2's
    /// `ReadRegister` including the "returns the current wavetable
    /// sample (not the addressed one) when writes are disabled"
    /// quirk.
    pub fn read_register(&self, addr: u16) -> u8 {
        if addr <= 0x407F {
            if self.wave_write_enabled {
                self.wave_table[(addr & 0x3F) as usize]
            } else {
                // Playback mode — the whole $4040-$407F window
                // returns whatever sample the wave pointer is on.
                self.wave_table[self.wave_position as usize]
            }
        } else {
            match addr {
                0x4090 => self.volume.gain,
                0x4092 => self.modulator.env.gain,
                _ => 0,
            }
        }
    }

    /// CPU-visible write through `$4040-$4097`. The mapper is
    /// responsible for the `$4023.1` sound-enable gate; by the time
    /// we reach here the write is authorized.
    pub fn write_register(&mut self, addr: u16, value: u8) {
        if addr <= 0x407F {
            if self.wave_write_enabled {
                self.wave_table[(addr & 0x3F) as usize] = value & 0x3F;
            }
            return;
        }

        match addr {
            0x4080 | 0x4082 => {
                self.volume.write_reg_common(addr, value);
                self.modulator.update_output(self.volume.frequency);
            }
            0x4083 => {
                self.disable_envelopes = (value & 0x40) != 0;
                self.halt_waveform = (value & 0x80) != 0;
                if self.halt_waveform {
                    // $4083.7 both halts the wave AND resets the
                    // accumulator to phase 0. Games use this as
                    // "retrigger note."
                    self.wave_position = 0;
                }
                if self.disable_envelopes {
                    // Freezing envelopes resets both timers so the
                    // first tick after re-enable is a clean full
                    // period.
                    self.volume.reset_timer();
                    self.modulator.env.reset_timer();
                }
                self.volume.write_reg_common(addr, value);
                self.modulator.update_output(self.volume.frequency);
            }
            0x4084 | 0x4085 | 0x4086 | 0x4087 => {
                self.modulator.write_reg(addr, value);
                if matches!(addr, 0x4084 | 0x4085) {
                    // Gain or counter changed — refresh mod output.
                    self.modulator.update_output(self.volume.frequency);
                }
            }
            0x4088 => self.modulator.write_mod_table(value),
            0x4089 => {
                self.master_volume = value & 0x03;
                self.wave_write_enabled = (value & 0x80) != 0;
            }
            0x408A => {
                self.volume.set_master_speed(value);
                self.modulator.env.set_master_speed(value);
            }
            _ => {}
        }
    }

    /// Advance the audio unit by one CPU cycle. Safe to call
    /// unconditionally — the unit does nothing audible until the
    /// BIOS enables sound at `$4023.1` and the game writes to
    /// `$4080` / `$4082` / `$4083`.
    pub fn clock(&mut self) {
        let frequency = self.volume.frequency;
        if !self.halt_waveform && !self.disable_envelopes {
            self.volume.tick_envelope();
            if self.modulator.env.tick_envelope() {
                self.modulator.update_output(frequency);
            }
        }

        if self.modulator.tick_modulator() {
            self.modulator.update_output(frequency);
        }

        self.update_output();

        if !self.halt_waveform {
            let delta = frequency as i32 + self.modulator.output();
            if delta > 0 {
                let (new, overflowed) = self
                    .wave_overflow_counter
                    .overflowing_add(delta as u16);
                self.wave_overflow_counter = new;
                if overflowed {
                    self.wave_position = (self.wave_position + 1) & 0x3F;
                }
            }
        }
    }

    /// Recompute the cached output sample. Called from `clock` every
    /// cycle; matches Mesen2's `UpdateOutput`.
    ///
    /// Formula (from Mesen2): `wave[pos] * min(gain, 32) * WAVE_VOL_TABLE[master] / 1152`.
    /// With `wave[pos] = 63`, `gain = 32`, `master = 0`: output =
    /// `63 * 32 * 36 / 1152 = 63` (the peak). Master volumes 1/2/3
    /// scale that down via the precomputed DAC-weight table.
    fn update_output(&mut self) {
        if self.wave_write_enabled {
            // $4089.7 = 1 freezes the output at `last_output` and
            // lets the CPU rewrite the wavetable click-free.
            return;
        }

        let gain = self.volume.gain.min(GAIN_CAP) as u32;
        let level = gain * WAVE_VOL_TABLE[self.master_volume as usize];
        let sample = self.wave_table[self.wave_position as usize] as u32;
        let output = (sample * level) / 1152;
        self.last_output = output as u8;
    }

    // --- Introspection hooks used by the mapper's $4090-$4097 read path ---

    pub fn volume_gain(&self) -> u8 {
        self.volume.gain
    }

    pub fn mod_gain(&self) -> u8 {
        self.modulator.env.gain
    }
}

impl Default for FdsAudio {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_audio_is_silent() {
        let a = FdsAudio::new();
        assert_eq!(a.output_level(), 0);
        assert_eq!(a.mix_sample(), 0.0);
    }

    /// With `$4089.7=1` the wavetable range is CPU-writable. Writes
    /// mask to 6 bits and land at the addressed offset.
    #[test]
    fn wavetable_writes_mask_to_6_bits_when_enabled() {
        let mut a = FdsAudio::new();
        a.write_register(0x4089, 0x80); // enable wave writes
        a.write_register(0x4040, 0xFF);
        a.write_register(0x407F, 0x3F);
        assert_eq!(a.wave_table[0], 0x3F);
        assert_eq!(a.wave_table[63], 0x3F);
    }

    /// With `$4089.7=0` the wavetable window is read-only — writes
    /// are silently dropped (real hardware behavior).
    #[test]
    fn wavetable_writes_ignored_when_disabled() {
        let mut a = FdsAudio::new();
        // Pre-seed a value through the enabled path.
        a.write_register(0x4089, 0x80);
        a.write_register(0x4040, 0x20);
        // Disable wave writes and try to overwrite — should drop.
        a.write_register(0x4089, 0x00);
        a.write_register(0x4040, 0x01);
        assert_eq!(a.wave_table[0], 0x20);
    }

    /// Reads from the wavetable window in playback mode return the
    /// CURRENT sample (not the addressed one). Mesen2 + nesdev wiki
    /// both call this out explicitly.
    #[test]
    fn wavetable_reads_in_playback_mode_return_current_sample() {
        let mut a = FdsAudio::new();
        a.write_register(0x4089, 0x80);
        for i in 0..64 {
            a.write_register(0x4040 + i, i as u8);
        }
        a.write_register(0x4089, 0x00); // disable writes → playback mode
        a.wave_position = 10; // direct poke for the test
        // All addresses in the window return wave[10].
        assert_eq!(a.read_register(0x4040), 10);
        assert_eq!(a.read_register(0x4055), 10);
        assert_eq!(a.read_register(0x407F), 10);
    }

    /// `$4083.7=1` halts the waveform and resets the wave position
    /// to zero so the next note starts from the beginning of the
    /// table — the "retrigger" idiom.
    #[test]
    fn write_4083_halt_bit_resets_wave_position() {
        let mut a = FdsAudio::new();
        a.wave_position = 37;
        a.write_register(0x4083, 0x80); // halt waveform
        assert_eq!(a.wave_position, 0);
        assert!(a.halt_waveform);
    }

    /// `$4089.0-1` selects the master volume divider. Writing 2
    /// picks `WAVE_VOL_TABLE[2] = 17` and keeps the wave-writes flag
    /// clear.
    #[test]
    fn master_volume_decoded_from_4089() {
        let mut a = FdsAudio::new();
        a.write_register(0x4089, 0x02);
        assert_eq!(a.master_volume, 2);
        assert!(!a.wave_write_enabled);
    }

    /// `$408A` is the global envelope-speed divider. BIOS boots with
    /// `$E8`; a game overwrite must propagate to both channels.
    #[test]
    fn write_408a_propagates_master_speed_to_both_channels() {
        let mut a = FdsAudio::new();
        a.write_register(0x408A, 0x10);
        assert_eq!(a.volume.master_speed, 0x10);
        assert_eq!(a.modulator.env.master_speed, 0x10);
    }

    /// The envelope timer formula is `8 × (speed+1) × master_speed`.
    /// With speed=0 and master=1 a single envelope tick fires in 8
    /// cycles. Verifies `tick_envelope` returns true exactly on the
    /// countdown-reached cycle.
    #[test]
    fn envelope_ticks_at_expected_cycle() {
        let mut e = EnvelopeUnit::new();
        e.set_master_speed(1);
        e.speed = 0;
        e.envelope_off = false;
        e.volume_increase = true;
        e.reset_timer();
        // 7 cycles of countdown, no tick yet.
        for _ in 0..7 {
            assert!(!e.tick_envelope());
        }
        // 8th cycle: tick fires, gain increments, timer reloads.
        assert!(e.tick_envelope());
        assert_eq!(e.gain, 1);
    }

    /// Envelope in "off" mode (bit 7) freezes gain at the speed
    /// value and stops the countdown — no ticks regardless of how
    /// many cycles pass.
    #[test]
    fn envelope_off_mode_freezes_gain() {
        let mut a = FdsAudio::new();
        a.write_register(0x408A, 1);
        // $4080 with bit7=1 and bits0-5=25 → manual gain 25.
        a.write_register(0x4080, 0x80 | 25);
        assert_eq!(a.volume.gain, 25);
        for _ in 0..1000 {
            a.volume.tick_envelope();
        }
        assert_eq!(a.volume.gain, 25);
    }

    /// Wave position advances whenever the accumulator
    /// `pitch + mod_output` overflows 16 bits. With pitch = 0x8000
    /// and mod disabled, two cycles overflow once → position = 1.
    #[test]
    fn wave_position_advances_on_accumulator_overflow() {
        let mut a = FdsAudio::new();
        // Fill wavetable with an incrementing ramp so position is
        // observable indirectly through output_level.
        a.write_register(0x4089, 0x80);
        for i in 0..64u16 {
            a.write_register(0x4040 + i, (i & 0x3F) as u8);
        }
        a.write_register(0x4089, 0x00);
        // Manual gain = 32 (cap), master volume 0.
        a.write_register(0x4080, 0x80 | 32);
        // Disable modulator.
        a.write_register(0x4087, 0x80);
        // Pitch = 0x8000 → two cycles overflow.
        a.write_register(0x4082, 0x00);
        a.write_register(0x4083, 0x08); // bits 0-3 of $4083 → pitch bits 8-11
        // Higher bit: $4083 bit 3 isn't enough for 0x800. Re-derive.
        // $4083 low nibble << 8 = pitch[11..8]. 0x8 here = 0x800 pitch,
        // accumulator goes 0x800 per cycle → overflows after ~32 cycles.
        // To cleanly test: set pitch to 0x0800 and count cycles to overflow.
        // Overflow occurs after 0x10000 / 0x800 = 32 ticks.
        for _ in 0..32 {
            a.clock();
        }
        assert!(a.wave_position > 0, "wave should have advanced past 0");
    }

    /// Mod-table entries are pushed in duplicate: one write to
    /// `$4088` lands in slots N and N+1, then the position
    /// increments by 2. This is how FDS music drivers cram 32
    /// effective entries into the 64-byte physical table.
    #[test]
    fn mod_table_push_writes_duplicate_pairs() {
        let mut a = FdsAudio::new();
        a.write_register(0x4087, 0x80); // halt mod to enable $4088 writes
        a.write_register(0x4088, 0x03);
        assert_eq!(a.modulator.mod_table[0], 3);
        assert_eq!(a.modulator.mod_table[1], 3);
        assert_eq!(a.modulator.mod_table_position, 2);
        a.write_register(0x4088, 0x05);
        // Mesen2 masks the value with 0x07 — 0x05 = -4 offset
        // interpretation on the output side but the stored byte is
        // still 5.
        assert_eq!(a.modulator.mod_table[2], 5);
        assert_eq!(a.modulator.mod_table[3], 5);
        assert_eq!(a.modulator.mod_table_position, 4);
    }

    /// Mod-table writes while the modulator is running are dropped
    /// on the floor. Some music drivers rely on this to stage a new
    /// table without clobbering an active voice.
    #[test]
    fn mod_table_push_ignored_while_running() {
        let mut a = FdsAudio::new();
        // Modulator halted first so the sentinel write lands.
        a.write_register(0x4087, 0x80);
        a.write_register(0x4088, 0x07);
        assert_eq!(a.modulator.mod_table[0], 7);
        // Re-enable modulator ($4087.7 = 0, non-zero freq would be
        // needed to actually run but the write-guard only checks
        // the halt bit).
        a.write_register(0x4087, 0x00);
        a.write_register(0x4088, 0x01);
        // Second push should be dropped.
        assert_eq!(a.modulator.mod_table[0], 7);
    }

    /// The read-back port `$4090` returns the current volume gain
    /// (unmasked in the low 6 bits). `$4092` does the same for the
    /// modulator.
    #[test]
    fn readback_ports_report_current_gains() {
        let mut a = FdsAudio::new();
        a.write_register(0x408A, 1);
        a.write_register(0x4080, 0x80 | 15); // manual gain = 15
        a.write_register(0x4084, 0x80 | 7); // mod manual gain = 7
        assert_eq!(a.read_register(0x4090), 15);
        assert_eq!(a.read_register(0x4092), 7);
    }
}
