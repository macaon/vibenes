//! ADSR / GAIN envelope generator for the S-DSP.
//!
//! Each voice owns an [`EnvelopeState`] that is advanced once per
//! 32 kHz output sample. The envelope is an 11-bit unsigned magnitude
//! (0..0x7FF) that the voice mixer multiplies into the
//! Gaussian-interpolated raw sample to set the per-sample loudness.
//! The visible `ENVX` register exposes bits 10-4 (top 7) of the
//! internal envelope.
//!
//! ## Two envelope dialects
//!
//! - **ADSR** (when `ADSR1.7 = 1`): four-stage state machine with
//!   Attack (linear up by +32 per fire, or +1024 if the Attack rate
//!   is 15), Decay (exponential down), Sustain (exponential down at
//!   `ADSR2.4-0` rate), and Release (linear down by 8 every sample).
//!   The state machine transitions Attack→Decay automatically on
//!   saturation (any value > 0x7FF or < 0 in the per-sample math).
//!   The Decay→Sustain transition fires when `env >> 8 == SL` where
//!   `SL = ADSR2.7-5`.
//!
//! - **GAIN** (when `ADSR1.7 = 0`): four sub-modes selected by
//!   `GAIN.7-5`. Direct (`GAIN.7 = 0`) writes a fixed envelope value;
//!   the four rate-controlled modes (linear/exp decrease, linear/bent
//!   increase) advance every sample whose rate fires.
//!
//! - **Release** (entered by KOFF or by the voice runtime explicitly):
//!   overrides everything, decrements by 8 every sample, no rate
//!   gating, clamped at 0.
//!
//! ## Rate-fire counter
//!
//! 32 distinct rates 0..31. Rate 0 is "off" (envelope frozen). Rate
//! 31 fires every sample. The other rates fire on a periodic schedule
//! determined by [`EnvelopeCounter`], a global counter shared across
//! all 8 voices that decrements once per 32 kHz tick. A given rate
//! fires when `(counter + offset[rate]) % period[rate] == 0`. Period
//! 30720 (= 0x77FF + 1) is the LCM of all non-trivial periods so the
//! counter cycles cleanly.
//!
//! ## Sources
//!
//! - `~/.claude/skills/nes-expert/reference/snes-apu.md` §6
//!   (canonical formulae + period table). Note: that doc has a sign
//!   error in the Decay/Sustain formula (`((env-1)>>8) - 1`); the
//!   actual hardware does `((env-1)>>8) + 1`. Mesen2 + higan agree.
//! - Mesen2 `Core/SNES/DSP/DspVoice.cpp::ProcessEnvelope` for the
//!   complete state machine.
//! - Mesen2 `Core/SNES/DSP/Dsp.cpp::CheckCounter` for the rate +
//!   offset tables (32 entries each).
//! - higan `sfc/dsp/envelope.cpp::envelopeRun` as cross-check.

/// ADSR state machine modes. Voices not in ADSR (i.e. driven by GAIN
/// only) effectively stay in [`EnvelopeMode::Sustain`] for accounting
/// purposes - GAIN math runs unconditionally each sample regardless
/// of the recorded mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvelopeMode {
    Attack,
    Decay,
    Sustain,
    Release,
}

/// Per-voice envelope state.
#[derive(Debug, Clone, Copy)]
pub struct EnvelopeState {
    /// Internal 11-bit envelope value (0..0x7FF).
    pub level: u16,
    pub mode: EnvelopeMode,
    /// Pre-clamp computed envelope from the previous tick. Only the
    /// "bent line" GAIN sub-mode actually reads this; we store it
    /// for all paths to keep the state model simple. Stored as i32
    /// to capture pre-clamp negative values that flag Attack→Decay
    /// transitions.
    pub prev_calc: i32,
}

impl EnvelopeState {
    /// Idle state: silent, parked in Sustain (GAIN-only voices live
    /// here; ADSR voices need [`Self::key_on`] to enter Attack).
    pub const fn new() -> Self {
        Self {
            level: 0,
            mode: EnvelopeMode::Sustain,
            prev_calc: 0,
        }
    }

    /// KON-equivalent reset: jump to Attack with envelope cleared.
    /// The voice runtime calls this when the host writes a 1 bit to
    /// the global KON register for this voice.
    pub fn key_on(&mut self) {
        self.level = 0;
        self.mode = EnvelopeMode::Attack;
        self.prev_calc = 0;
    }

    /// Force into Release. The voice runtime calls this when KOFF is
    /// asserted for the voice or when a BRR end-without-loop block
    /// completes.
    pub fn key_off(&mut self) {
        self.mode = EnvelopeMode::Release;
    }
}

impl Default for EnvelopeState {
    fn default() -> Self {
        Self::new()
    }
}

/// Global envelope rate counter shared by all 8 voices. Decrements
/// once per output sample; wraps from 0 back to 0x77FF.
#[derive(Debug, Clone, Copy)]
pub struct EnvelopeCounter {
    pub value: u16,
}

impl EnvelopeCounter {
    /// Power-on / reset: start at 0 (matches Mesen2's
    /// `Dsp::Reset` which sets `_state.Counter = 0`).
    pub const fn new() -> Self {
        Self { value: 0 }
    }

    /// Advance the counter by one output sample. Call once per
    /// 32 kHz tick BEFORE running per-voice envelope updates so all
    /// voices see the same counter value within a sample.
    pub fn tick(&mut self) {
        self.value = if self.value == 0 { 0x77FF } else { self.value - 1 };
    }

    /// Returns whether the given rate (0..31) fires on the current
    /// counter value.
    ///
    /// Rate 0 returns `false` always ("envelope frozen"); rate 31
    /// returns `true` every sample. Other rates fire on a
    /// rate-specific phase within the counter's 30720-sample cycle.
    pub fn should_update(&self, rate: u8) -> bool {
        let r = rate as usize & 0x1F;
        if r == 0 {
            return false;
        }
        let period = RATE_PERIOD[r];
        let offset = RATE_OFFSET[r];
        ((self.value as u32 + offset as u32) % period as u32) == 0
    }
}

impl Default for EnvelopeCounter {
    fn default() -> Self {
        Self::new()
    }
}

/// Period (in samples) between successive fires for each rate.
/// Rate 0 is encoded as `u16::MAX` ("never fires"); rate 31 is 1
/// (every sample). Source: Mesen2 `CheckCounter` (Dsp.cpp:59-74).
pub const RATE_PERIOD: [u16; 32] = [
    u16::MAX, 2048, 1536, 1280, 1024, 768, 640, 512, 384, 320, 256, 192, 160, 128, 96, 80, 64, 48,
    40, 32, 24, 20, 16, 12, 10, 8, 6, 5, 4, 3, 2, 1,
];

/// Phase offset within the period for each rate. Source: Mesen2
/// `CheckCounter` (Dsp.cpp:76-90).
pub const RATE_OFFSET: [u16; 32] = [
    0, 0, 1040, 536, 0, 1040, 536, 0, 1040, 536, 0, 1040, 536, 0, 1040, 536, 0, 1040, 536, 0, 1040,
    536, 0, 1040, 536, 0, 1040, 536, 0, 1040, 0, 0,
];

/// Advance the envelope by one 32 kHz output sample.
///
/// Inputs:
/// - `state`: the per-voice [`EnvelopeState`], updated in place.
/// - `adsr1`, `adsr2`, `gain`: raw register values for the voice.
/// - `counter`: the global envelope rate counter (already
///   `tick()`ed for the current sample by the caller).
///
/// Algorithm summary (Mesen2 `ProcessEnvelope` line-for-line, with
/// canonical typo correction noted in module docs):
///
/// 1. Release mode: `level -= 8`, clamp at 0, return.
/// 2. ADSR mode (`ADSR1.7 = 1`): pick rate + compute new env per
///    current state machine stage.
/// 3. GAIN mode (`ADSR1.7 = 0`): direct gain (rate ignored) or one
///    of four rate-controlled curves.
/// 4. Decay→Sustain transition fires whenever the new `env >> 8`
///    matches `sustain_byte >> 5`.
/// 5. Save pre-clamp env to `prev_calc` (used by next sample's bent-
///    line GAIN mode).
/// 6. Clamp env to [0, 0x7FF]; if Attack saturates either way,
///    transition to Decay.
/// 7. Commit `env` into `state.level` only when the rate fires.
pub fn step_envelope(
    state: &mut EnvelopeState,
    adsr1: u8,
    adsr2: u8,
    gain: u8,
    counter: &EnvelopeCounter,
) {
    if state.mode == EnvelopeMode::Release {
        let new_level = (state.level as i32 - 8).max(0);
        state.level = new_level as u16;
        state.prev_calc = new_level;
        return;
    }

    let mut env = state.level as i32;
    let rate: u8;
    let sustain_byte: u8;

    if adsr1 & 0x80 != 0 {
        // ADSR-driven envelope.
        sustain_byte = adsr2;
        match state.mode {
            EnvelopeMode::Attack => {
                let attack = adsr1 & 0x0F;
                if attack == 0x0F {
                    // Fast attack (rate 15): fires every sample with +1024.
                    rate = 31;
                    env += 1024;
                } else {
                    rate = (attack << 1) | 1;
                    env += 32;
                }
            }
            EnvelopeMode::Decay => {
                env -= ((env - 1) >> 8) + 1;
                rate = ((adsr1 >> 3) & 0x0E) | 0x10;
            }
            EnvelopeMode::Sustain => {
                env -= ((env - 1) >> 8) + 1;
                rate = adsr2 & 0x1F;
            }
            EnvelopeMode::Release => unreachable!("Release handled above"),
        }
    } else {
        // GAIN-driven envelope.
        sustain_byte = gain;
        if gain & 0x80 != 0 {
            rate = gain & 0x1F;
            match gain & 0x60 {
                0x00 => env -= 32,
                0x20 => env -= ((env - 1) >> 8) + 1,
                0x40 => env += 32,
                0x60 => {
                    let prev_unsigned = state.prev_calc as u32 & 0xFFFF;
                    env += if prev_unsigned < 0x600 { 32 } else { 8 };
                }
                _ => unreachable!(),
            }
        } else {
            // Direct gain - the level snaps to gain * 16, no rate gating
            // (rate is forced to 31 so should_update always returns true).
            env = (gain as i32) << 4;
            rate = 31;
        }
    }

    // Decay→Sustain transition - happens BEFORE clamp so a Decay path
    // crossing the SL threshold transitions even if env later saturates.
    if state.mode == EnvelopeMode::Decay && (env >> 8) == ((sustain_byte >> 5) as i32) {
        state.mode = EnvelopeMode::Sustain;
    }

    // Save pre-clamp value for next sample's bent-line GAIN mode.
    state.prev_calc = env;

    // Clamp to 11-bit unsigned. Attack mode flips to Decay on EITHER
    // direction of saturation (per Anomie's "negative values also
    // trigger this" critical note quoted in Mesen2).
    if !(0..=0x7FF).contains(&env) {
        env = env.clamp(0, 0x7FF);
        if state.mode == EnvelopeMode::Attack {
            state.mode = EnvelopeMode::Decay;
        }
    }

    if counter.should_update(rate) {
        state.level = env as u16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- EnvelopeCounter -----------------------------------------------

    #[test]
    fn counter_starts_at_zero() {
        assert_eq!(EnvelopeCounter::new().value, 0);
    }

    #[test]
    fn counter_tick_wraps_zero_to_77ff() {
        let mut c = EnvelopeCounter::new();
        c.tick();
        assert_eq!(c.value, 0x77FF);
    }

    #[test]
    fn counter_tick_decrements() {
        let mut c = EnvelopeCounter { value: 100 };
        c.tick();
        assert_eq!(c.value, 99);
    }

    #[test]
    fn rate_zero_never_fires() {
        for v in 0..=0x77FF {
            let c = EnvelopeCounter { value: v };
            assert!(!c.should_update(0));
        }
    }

    #[test]
    fn rate_thirty_one_fires_every_sample() {
        for v in 0..=0x77FF {
            let c = EnvelopeCounter { value: v };
            assert!(c.should_update(31));
        }
    }

    #[test]
    fn rate_thirty_fires_every_other_sample() {
        // Period 2, offset 0: fires when value is even.
        let c = EnvelopeCounter { value: 0 };
        assert!(c.should_update(30));
        let c = EnvelopeCounter { value: 1 };
        assert!(!c.should_update(30));
        let c = EnvelopeCounter { value: 2 };
        assert!(c.should_update(30));
    }

    #[test]
    fn rate_one_fires_once_in_period() {
        // Period 2048, offset 0. Fires when value % 2048 == 0.
        let mut count = 0;
        let mut c = EnvelopeCounter::new();
        for _ in 0..0x7800 {
            if c.should_update(1) {
                count += 1;
            }
            c.tick();
        }
        // 0x7800 / 2048 = 15.
        assert_eq!(count, 15);
    }

    // ----- Direct GAIN ---------------------------------------------------

    #[test]
    fn direct_gain_snaps_envelope_to_gain_times_sixteen() {
        let mut state = EnvelopeState::new();
        let counter = EnvelopeCounter::new();
        // ADSR disabled (b7=0), GAIN.7=0 -> direct gain mode.
        step_envelope(&mut state, 0x00, 0x00, 0x40, &counter);
        assert_eq!(state.level, 0x40 << 4);
    }

    #[test]
    fn direct_gain_max_value() {
        let mut state = EnvelopeState::new();
        let counter = EnvelopeCounter::new();
        // GAIN = 0x7F (b7=0 direct, max value) -> env = 0x7F0 (clamped).
        step_envelope(&mut state, 0x00, 0x00, 0x7F, &counter);
        assert_eq!(state.level, 0x7F0);
    }

    // ----- Rate-controlled GAIN -----------------------------------------

    #[test]
    fn gain_linear_decrease_subtracts_thirty_two() {
        // GAIN = 0x9F (b7=1 rate-mode, b6-5=00 lin-dec, rate=31).
        let mut state = EnvelopeState {
            level: 0x100,
            mode: EnvelopeMode::Sustain,
            prev_calc: 0x100,
        };
        let counter = EnvelopeCounter::new();
        step_envelope(&mut state, 0x00, 0x00, 0x9F, &counter);
        assert_eq!(state.level, 0x100 - 32);
    }

    #[test]
    fn gain_linear_decrease_clamps_at_zero() {
        let mut state = EnvelopeState {
            level: 16,
            mode: EnvelopeMode::Sustain,
            prev_calc: 16,
        };
        let counter = EnvelopeCounter::new();
        step_envelope(&mut state, 0x00, 0x00, 0x9F, &counter);
        assert_eq!(state.level, 0);
    }

    #[test]
    fn gain_linear_increase_adds_thirty_two() {
        // GAIN = 0xDF (b7=1, b6-5=10 lin-inc, rate=31).
        let mut state = EnvelopeState {
            level: 0x100,
            mode: EnvelopeMode::Sustain,
            prev_calc: 0x100,
        };
        let counter = EnvelopeCounter::new();
        step_envelope(&mut state, 0x00, 0x00, 0xDF, &counter);
        assert_eq!(state.level, 0x100 + 32);
    }

    #[test]
    fn gain_bent_increase_below_sixhundred_adds_thirty_two() {
        // GAIN = 0xFF (b7=1, b6-5=11 bent, rate=31).
        let mut state = EnvelopeState {
            level: 0x500,
            mode: EnvelopeMode::Sustain,
            prev_calc: 0x500,
        };
        let counter = EnvelopeCounter::new();
        step_envelope(&mut state, 0x00, 0x00, 0xFF, &counter);
        assert_eq!(state.level, 0x500 + 32);
    }

    #[test]
    fn gain_bent_increase_at_or_above_sixhundred_adds_eight() {
        let mut state = EnvelopeState {
            level: 0x650,
            mode: EnvelopeMode::Sustain,
            prev_calc: 0x650,
        };
        let counter = EnvelopeCounter::new();
        step_envelope(&mut state, 0x00, 0x00, 0xFF, &counter);
        assert_eq!(state.level, 0x650 + 8);
    }

    #[test]
    fn gain_exponential_decrease_drops_proportionally() {
        // GAIN = 0xBF (b7=1, b6-5=01 exp-dec, rate=31).
        // env=0x500 -> env -= ((0x4FF) >> 8) + 1 = 4 + 1 = 5.
        let mut state = EnvelopeState {
            level: 0x500,
            mode: EnvelopeMode::Sustain,
            prev_calc: 0x500,
        };
        let counter = EnvelopeCounter::new();
        step_envelope(&mut state, 0x00, 0x00, 0xBF, &counter);
        assert_eq!(state.level, 0x500 - 5);
    }

    // ----- ADSR Attack ---------------------------------------------------

    #[test]
    fn attack_with_rate_below_fifteen_adds_thirty_two() {
        // ADSR1.7=1, A=0 -> rate index = 1 (period 2048, offset 0).
        // Counter starts at 0, so (0 + 0) % 2048 == 0 -> fires.
        let mut state = EnvelopeState {
            level: 0,
            mode: EnvelopeMode::Attack,
            prev_calc: 0,
        };
        let counter = EnvelopeCounter { value: 0 };
        step_envelope(&mut state, 0x80, 0x00, 0x00, &counter);
        assert_eq!(state.level, 32);
    }

    #[test]
    fn attack_rate_fifteen_jumps_by_1024() {
        // ADSR1.b3-0 = 0xF -> rate 31, env += 1024.
        let mut state = EnvelopeState {
            level: 0,
            mode: EnvelopeMode::Attack,
            prev_calc: 0,
        };
        let counter = EnvelopeCounter::new();
        step_envelope(&mut state, 0x8F, 0x00, 0x00, &counter);
        assert_eq!(state.level, 1024);
    }

    #[test]
    fn attack_saturates_to_decay() {
        // Attack pushes env over 0x7FF -> mode becomes Decay, level
        // clamps to 0x7FF.
        let mut state = EnvelopeState {
            level: 0x7E0,
            mode: EnvelopeMode::Attack,
            prev_calc: 0x7E0,
        };
        let counter = EnvelopeCounter::new();
        step_envelope(&mut state, 0x8F, 0x00, 0x00, &counter);
        assert_eq!(state.level, 0x7FF);
        assert_eq!(state.mode, EnvelopeMode::Decay);
    }

    // ----- ADSR Decay ----------------------------------------------------

    #[test]
    fn decay_step_subtracts_proportionally() {
        // env=0x600, decay step: env -= ((0x5FF) >> 8) + 1 = 5 + 1 = 6.
        // ADSR1: b7=1, b6-4 D=4 -> rate index = (D<<1)|0x10 = 8|0x10 = 24.
        // Use ADSR2 with SL=7 so we don't transition to Sustain prematurely.
        let mut state = EnvelopeState {
            level: 0x600,
            mode: EnvelopeMode::Decay,
            prev_calc: 0x600,
        };
        // Find a counter value where rate 24 fires.
        let mut c = EnvelopeCounter::new();
        while !c.should_update(24) {
            c.tick();
        }
        step_envelope(&mut state, 0x84_u8.wrapping_shl(0) | 0x40, 0xE0, 0x00, &c);
        // ADSR1 byte: 0xC0 (b7=1 enable, b6-4 D=4)? Let me reconstruct:
        // ADSR1 = enable(0x80) | D<<4 (0x40 for D=4) | A (0..15).
        // We don't care about A here since mode=Decay. So 0xC0.
        // Re-run with 0xC0 properly.
        state.level = 0x600;
        state.mode = EnvelopeMode::Decay;
        state.prev_calc = 0x600;
        step_envelope(&mut state, 0xC0, 0xE0, 0x00, &c);
        assert_eq!(state.level, 0x600 - 6);
    }

    #[test]
    fn decay_to_sustain_transition_when_post_decay_high_byte_matches_sustain_level() {
        // SL = 5 -> ADSR2.7-5 = 0b101 = 0xA0.
        // Starting level 0x600 -> Decay step yields 0x600 - ((0x5FF)>>8) - 1
        //   = 0x600 - 5 - 1 = 0x5FA. High byte = 5 == SL -> transition.
        let mut state = EnvelopeState {
            level: 0x600,
            mode: EnvelopeMode::Decay,
            prev_calc: 0x600,
        };
        let counter = EnvelopeCounter::new();
        step_envelope(&mut state, 0xC0, 0xA0, 0x00, &counter);
        assert_eq!(state.mode, EnvelopeMode::Sustain);
    }

    // ----- ADSR Sustain --------------------------------------------------

    #[test]
    fn sustain_uses_sustain_rate_field() {
        // SR field is ADSR2.4-0. With SR=31, rate fires every sample.
        let mut state = EnvelopeState {
            level: 0x400,
            mode: EnvelopeMode::Sustain,
            prev_calc: 0x400,
        };
        let counter = EnvelopeCounter::new();
        step_envelope(&mut state, 0x80, 0x1F, 0x00, &counter);
        // env=0x400 -> step = ((0x3FF)>>8)+1 = 3+1 = 4. Result = 0x3FC.
        assert_eq!(state.level, 0x400 - 4);
    }

    #[test]
    fn sustain_with_sr_zero_does_not_change() {
        // SR=0 -> rate 0 -> never fires.
        let mut state = EnvelopeState {
            level: 0x400,
            mode: EnvelopeMode::Sustain,
            prev_calc: 0x400,
        };
        let counter = EnvelopeCounter::new();
        for _ in 0..1000 {
            step_envelope(&mut state, 0x80, 0x00, 0x00, &counter);
        }
        assert_eq!(state.level, 0x400);
    }

    // ----- Release -------------------------------------------------------

    #[test]
    fn release_decrements_by_eight_every_sample() {
        let mut state = EnvelopeState {
            level: 100,
            mode: EnvelopeMode::Release,
            prev_calc: 100,
        };
        // Release ignores the counter - fires every sample regardless.
        let counter = EnvelopeCounter { value: 0x1234 };
        step_envelope(&mut state, 0xFF, 0xFF, 0xFF, &counter);
        assert_eq!(state.level, 92);
    }

    #[test]
    fn release_clamps_at_zero() {
        let mut state = EnvelopeState {
            level: 5,
            mode: EnvelopeMode::Release,
            prev_calc: 5,
        };
        let counter = EnvelopeCounter::new();
        step_envelope(&mut state, 0x00, 0x00, 0x00, &counter);
        assert_eq!(state.level, 0);
    }

    #[test]
    fn release_stays_at_zero() {
        let mut state = EnvelopeState {
            level: 0,
            mode: EnvelopeMode::Release,
            prev_calc: 0,
        };
        let counter = EnvelopeCounter::new();
        for _ in 0..5 {
            step_envelope(&mut state, 0x00, 0x00, 0x00, &counter);
        }
        assert_eq!(state.level, 0);
    }

    // ----- key_on / key_off ----------------------------------------------

    #[test]
    fn key_on_resets_to_attack_with_zero_level() {
        let mut state = EnvelopeState {
            level: 0x500,
            mode: EnvelopeMode::Sustain,
            prev_calc: 0x500,
        };
        state.key_on();
        assert_eq!(state.level, 0);
        assert_eq!(state.mode, EnvelopeMode::Attack);
    }

    #[test]
    fn key_off_jumps_to_release_preserving_level() {
        let mut state = EnvelopeState {
            level: 0x500,
            mode: EnvelopeMode::Decay,
            prev_calc: 0x500,
        };
        state.key_off();
        assert_eq!(state.level, 0x500, "level kept across key_off");
        assert_eq!(state.mode, EnvelopeMode::Release);
    }

    // ----- Rate-zero gating during ADSR --------------------------------

    #[test]
    fn rate_zero_freezes_envelope_during_decay() {
        // ADSR1: b6-4 D=0 -> rate index = (D<<1)|0x10 = 0x10. Period 64.
        // We pick a counter where this rate doesn't fire to check freeze.
        let mut state = EnvelopeState {
            level: 0x400,
            mode: EnvelopeMode::Decay,
            prev_calc: 0x400,
        };
        // Counter value where rate 16 (period 64, offset 0) does NOT fire.
        let counter = EnvelopeCounter { value: 1 };
        step_envelope(&mut state, 0x80, 0xFF, 0x00, &counter);
        // Mode might still transition to Sustain, but level shouldn't change.
        assert_eq!(state.level, 0x400);
    }
}
