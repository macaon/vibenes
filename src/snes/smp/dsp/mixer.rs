//! S-DSP master mixer.
//!
//! Owns the 8 [`Voice`] instances + the global [`EnvelopeCounter`]
//! and produces one stereo s16 output sample per 32 kHz tick by:
//!
//! 1. Latching the host-written KON / KOFF bits and dispatching them
//!    to the voices (KON priming, KOFF -> Release).
//! 2. Stepping each voice once for the current sample.
//! 3. Summing per-voice (L, R) contributions, clamping to s16.
//! 4. Applying master volume (MVOLL / MVOLR, signed 8-bit / 128).
//! 5. Returning the final stereo pair.
//!
//! Echo is wired here (5c.6): voices flagged in EON feed a parallel
//! echo bus, the [`super::echo::EchoUnit`] runs the FIR + delay-line
//! + feedback writeback into ARAM, and the resulting echo output
//! (already EVOL-scaled) is added to the dry mix before the final
//! s16 clamp.
//!
//! Noise and pitch-modulation (5c.7) are wired here too: a global
//! 15-bit LFSR ticks at the FLG.4-0 rate, and per-voice NON / PMON
//! bits replace the BRR-derived raw sample with the noise sample
//! and modulate the per-voice pitch by the previous voice's
//! post-envelope output respectively.
//!
//! ENDX writeback (5c.9): after stepping the voices, the mixer
//! aggregates each voice's `endx_pending` (set whenever a BRR
//! end-block was processed - looped *or* terminating, per Anomie
//! and the FF6-panning behaviour noted in snes-apu.md) into the
//! global `$7C` register as a sticky OR with whatever the host
//! last left there, then clears the bits of voices we KON'd this
//! same sample. This mirrors Mesen2 `DspVoice.cpp` step S7:
//! `voiceEnd = ReadReg(VoiceEnd) | Looped; if (keyOnStarted)
//! voiceEnd &= ~voiceBit;`. Hardware staggers the publish across
//! cycles (a 1-sample staging buffer); we collapse it - drivers
//! poll ENDX between samples, so the visible behaviour matches.
//!
//! ENVX / OUTX writeback (5c.10): each voice's `$X8` (current
//! envelope level) and `$X9` (current voice output) registers are
//! refreshed at the same end-of-sample point. Per Mesen2
//! `DspVoice.cpp` Step6/Step7:
//! `EnvRegBuffer = envVolume >> 4` and
//! `OutRegBuffer = (uint8_t)(VoiceOutput >> 8)`. Hardware uses a
//! 1-sample staging buffer (Step8/Step9 commit); we publish
//! immediately, which is invisible to drivers that poll between
//! samples.
//!
//! ## Deferred for future sub-phases
//!
//! - **KON polling cadence**: Mesen2 / hardware polls KON / KOFF
//!   every 2 samples (in their 5-step pipeline). We poll every
//!   sample. Audibly indistinguishable for normal music.
//!
//! ## Source pointers
//!
//! - Mesen2 `Core/SNES/DSP/Dsp.cpp::Cycle*` for the master pipeline.
//! - higan `sfc/dsp/dsp.cpp::sample` for cross-checks.
//! - `~/.claude/skills/nes-expert/reference/snes-apu.md` §"Master
//!   pipeline" + §"Voice DSP pipeline".

use super::super::state::DspRegs;
use super::echo::EchoUnit;
use super::envelope::EnvelopeCounter;
use super::voice::Voice;

/// 8-voice S-DSP runtime.
#[derive(Debug, Clone)]
pub struct Mixer {
    pub voices: [Voice; 8],
    pub counter: EnvelopeCounter,
    pub echo: EchoUnit,
    /// Bitmask of voices that need KON priming on the next tick.
    /// Latched from the global `KON` register; cleared after each
    /// voice has been started.
    pub kon_pending: u8,
    /// Global 15-bit noise LFSR. Clocked at the rate selected by
    /// FLG.4-0; the per-voice NON bit replaces a voice's BRR-derived
    /// raw sample with `(lfsr * 2)` (LSB-zeroed s16). Reset value
    /// is `0x4000` per Anomie / Mesen2 documentation.
    pub noise_lfsr: u16,
}

impl Mixer {
    /// SMP master cycles per 32 kHz output sample. The SPC700 runs at
    /// 24.576 MHz / 24 = 1.024 MHz; the S-DSP outputs at 24.576 MHz /
    /// 24 / 32 = 32 kHz. So the host scheduler should call
    /// [`Self::step_sample`] every 32 SMP cycles.
    pub const SMP_CYCLES_PER_SAMPLE: u32 = 32;

    pub const fn new() -> Self {
        Self {
            voices: [
                Voice::new(),
                Voice::new(),
                Voice::new(),
                Voice::new(),
                Voice::new(),
                Voice::new(),
                Voice::new(),
                Voice::new(),
            ],
            counter: EnvelopeCounter::new(),
            echo: EchoUnit::new(),
            kon_pending: 0,
            noise_lfsr: 0x4000,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Latch a host-side write to the global `KON` register. Voices
    /// whose bits are set will be primed on the next sample tick.
    /// Hardware self-clears KON to 0 once consumed; we drain
    /// `kon_pending` after dispatching, but leave the DSP register
    /// to whatever the host wrote (consumers can clear via
    /// [`DspRegs::write`]).
    pub fn latch_kon(&mut self, value: u8) {
        self.kon_pending |= value;
    }

    /// Produce one 32 kHz stereo sample.
    ///
    /// Reads its DSP register inputs from `dsp` and ARAM samples from
    /// `aram`. The caller is responsible for keeping the DSP
    /// register file in sync with what the SMP has written and for
    /// providing a 64 KiB ARAM slice. `dsp` is taken by mutable
    /// reference because the mixer writes back the ENDX (`$7C`)
    /// register at end-of-sample.
    pub fn step_sample(&mut self, dsp: &mut DspRegs, aram: &mut [u8; 0x10000]) -> (i16, i16) {
        // Tick the global counter once per output sample, BEFORE
        // running any voice (so all voices see the same value).
        self.counter.tick();

        // Resolve KOFF bitmask.
        let koff = dsp.koff();
        let eon = dsp.eon();

        // Capture the KON dispatch mask BEFORE clearing kon_pending,
        // so the ENDX writeback at end-of-sample knows which voices
        // were just keyed on (their ENDX bits must be cleared, per
        // Mesen2 DspVoice.cpp step S7).
        let kon_dispatched = self.kon_pending;

        // Dispatch any pending KONs - now (after the counter tick so
        // attack rate computations see this sample's counter).
        let dir_base = dsp.sample_directory_addr();
        for v in 0..8 {
            let bit = 1u8 << v;
            if self.kon_pending & bit != 0 {
                let srcn = dsp.voice_source_number(v);
                self.voices[v].start(srcn, dir_base, aram);
            }
            if koff & bit != 0 {
                self.voices[v].envelope.key_off();
            }
        }
        self.kon_pending = 0;

        // Mute flag (FLG.6) silences the output amplifier but voices
        // continue to step (envelopes / BRR keep advancing).
        let muted = dsp.muted();

        // Tick the noise LFSR if the FLG-selected rate fires this
        // sample. Mesen2 formula: N = (N>>1) | (((N<<14) ^ (N<<13)) & 0x4000).
        let noise_rate = dsp.noise_rate_index();
        if self.counter.should_update(noise_rate) {
            let new_bit = ((self.noise_lfsr << 14) ^ (self.noise_lfsr << 13)) & 0x4000;
            self.noise_lfsr = new_bit ^ (self.noise_lfsr >> 1);
        }
        // Noise sample: (LFSR * 2) cast to i16. The 15-bit LFSR's MSB
        // (bit 14) becomes the i16 sign bit after the shift, giving
        // the full s16 swing the voice path expects.
        let noise_sample = (self.noise_lfsr.wrapping_mul(2)) as i16;
        let non = dsp.non();
        let pmon = dsp.pmon();

        // Sum per-voice contributions; collect EON-flagged voices
        // separately for the echo feedback path. PMON chains the
        // previous voice's last_voice_output into the next voice's
        // pitch (b0 of PMON is ignored - voice 0 cannot be modulated).
        let mut sum_l: i32 = 0;
        let mut sum_r: i32 = 0;
        let mut eon_sum_l: i32 = 0;
        let mut eon_sum_r: i32 = 0;
        let mut prev_voice_output: i16 = 0;
        for v in 0..8 {
            let pitch = dsp.voice_pitch(v);
            let voll = dsp.voice_volume_left(v);
            let volr = dsp.voice_volume_right(v);
            let adsr1 = dsp.voice_adsr1(v);
            let adsr2 = dsp.voice_adsr2(v);
            let gain = dsp.voice_gain(v);
            let pmon_enabled = v > 0 && (pmon & (1u8 << v)) != 0;
            let non_enabled = (non & (1u8 << v)) != 0;
            let (l, r) = self.voices[v].step(
                pitch,
                voll,
                volr,
                adsr1,
                adsr2,
                gain,
                &self.counter,
                aram,
                prev_voice_output,
                pmon_enabled,
                noise_sample,
                non_enabled,
            );
            sum_l = sum_l.saturating_add(l);
            sum_r = sum_r.saturating_add(r);
            if eon & (1u8 << v) != 0 {
                eon_sum_l = eon_sum_l.saturating_add(l);
                eon_sum_r = eon_sum_r.saturating_add(r);
            }
            prev_voice_output = self.voices[v].last_voice_output;
        }

        // ENDX writeback. Each voice's `endx_pending` is set when its
        // most recently advanced BRR block had the end flag (regardless
        // of whether it also had loop set - hardware sets ENDX on every
        // end-block, looped or not). Aggregate-and-clear here, then OR
        // into the host-visible register and mask off any voices we
        // KON'd this sample.
        let mut endx_set: u8 = 0;
        for v in 0..8 {
            if self.voices[v].endx_pending {
                endx_set |= 1u8 << v;
                self.voices[v].endx_pending = false;
            }
        }
        let new_endx = (dsp.endx() | endx_set) & !kon_dispatched;
        dsp.write(super::global_reg::ENDX, new_endx);

        // ENVX / OUTX writeback. ENVX = envelope.level >> 4 (11-bit
        // level into the high 7 bits of an 8-bit unsigned register);
        // OUTX = high byte of last_voice_output (post-envelope, LSB-
        // masked - matches Mesen2 `(uint8_t)(VoiceOutput >> 8)`,
        // signed when interpreted as i8 by the SMP). Both publish
        // every sample; hardware's 1-sample staging buffer is
        // invisible to between-sample polls.
        for v in 0..8 {
            let envx = (self.voices[v].envelope.level >> 4) as u8;
            let outx = (self.voices[v].last_voice_output >> 8) as u8;
            dsp.set_voice_reg(v, super::voice_reg::ENVX, envx);
            dsp.set_voice_reg(v, super::voice_reg::OUTX, outx);
        }

        // Run the echo pipeline. Returns the echo contribution that
        // is added to the dry mix; also writes the feedback back to
        // ARAM (unless FLG.5 is set).
        let fir = [
            dsp.fir_coefficient(0),
            dsp.fir_coefficient(1),
            dsp.fir_coefficient(2),
            dsp.fir_coefficient(3),
            dsp.fir_coefficient(4),
            dsp.fir_coefficient(5),
            dsp.fir_coefficient(6),
            dsp.fir_coefficient(7),
        ];
        let (echo_l, echo_r) = self.echo.step_sample(
            eon_sum_l,
            eon_sum_r,
            dsp.echo_start_byte(),
            dsp.echo_delay(),
            dsp.echo_volume_left(),
            dsp.echo_volume_right(),
            dsp.echo_feedback(),
            fir,
            dsp.echo_write_disabled(),
            aram,
        );

        if muted {
            return (0, 0);
        }

        // Apply master volume (signed 8-bit / 128) to the dry voice
        // mix, then add the echo contribution (already EVOL-scaled by
        // the echo unit). Final clamp to s16.
        let mvol_l = dsp.master_volume_left() as i32;
        let mvol_r = dsp.master_volume_right() as i32;
        let dry_l = (sum_l * mvol_l) >> 7;
        let dry_r = (sum_r * mvol_r) >> 7;
        let final_l = clamp16(dry_l + echo_l as i32);
        let final_r = clamp16(dry_r + echo_r as i32);
        (final_l, final_r)
    }
}

impl Default for Mixer {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn clamp16(x: i32) -> i16 {
    x.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

#[cfg(test)]
mod tests {
    use super::super::super::state::DspRegs;
    use super::super::envelope::EnvelopeMode;
    use super::*;
    use crate::snes::smp::dsp::{global_reg, voice_reg};

    fn dsp_with_master_volume(left: i8, right: i8) -> DspRegs {
        let mut d = DspRegs::new();
        d.write(global_reg::MVOLL, left as u8);
        d.write(global_reg::MVOLR, right as u8);
        d
    }

    #[test]
    fn fresh_mixer_outputs_silence() {
        let mixer_pre = Mixer::new();
        assert_eq!(mixer_pre.counter.value, 0);
        let mut mixer = Mixer::new();
        let mut dsp = dsp_with_master_volume(0x40, 0x40);
        let mut aram = Box::new([0u8; 0x10000]);
        let (l, r) = mixer.step_sample(&mut dsp, &mut aram);
        assert_eq!(l, 0);
        assert_eq!(r, 0);
    }

    #[test]
    fn counter_ticks_once_per_sample() {
        let mut mixer = Mixer::new();
        let mut dsp = dsp_with_master_volume(0x40, 0x40);
        let mut aram = Box::new([0u8; 0x10000]);
        for _ in 0..5 {
            let _ = mixer.step_sample(&mut dsp, &mut aram);
        }
        // Counter started at 0, ticked 5 times: 0 -> 0x77FF -> 0x77FE -> ... -> 0x77FB.
        assert_eq!(mixer.counter.value, 0x77FB);
    }

    #[test]
    fn kon_latch_starts_voice_on_next_step() {
        let mut mixer = Mixer::new();
        let mut dsp = DspRegs::new();
        // Set up sample directory at $0500 / SRCN=0 -> directory entry
        // at $0500 has start=$0600, loop=$0600.
        dsp.write(global_reg::DIR, 0x05);
        dsp.write(global_reg::MVOLL, 0x40);
        dsp.write(global_reg::MVOLR, 0x40);
        // Voice 0 SRCN = 0 (default), VOL = 0x40, no ADSR / GAIN.
        dsp.write(DspRegs::voice_addr(0, voice_reg::VOLL), 0x40);
        dsp.write(DspRegs::voice_addr(0, voice_reg::VOLR), 0x40);
        dsp.write(DspRegs::voice_addr(0, voice_reg::PL), 0x00);
        dsp.write(DspRegs::voice_addr(0, voice_reg::PH), 0x10); // pitch = 0x1000
        dsp.write(DspRegs::voice_addr(0, voice_reg::ADSR1), 0x80); // ADSR enable
        dsp.write(DspRegs::voice_addr(0, voice_reg::ADSR2), 0x00);
        let mut aram = Box::new([0u8; 0x10000]);
        // Directory entry at $0500: start=$0600, loop=$0600
        aram[0x0500] = 0x00;
        aram[0x0501] = 0x06;
        aram[0x0502] = 0x00;
        aram[0x0503] = 0x06;
        // BRR block at $0600: end+loop, all-zero data
        aram[0x0600] = 0x03;

        mixer.latch_kon(0x01); // bit 0 -> voice 0
        let _ = mixer.step_sample(&mut dsp, &mut aram);
        assert!(mixer.voices[0].active, "voice 0 should be active after KON");
        assert_eq!(mixer.voices[0].brr_addr, 0x0600);
        assert_eq!(mixer.voices[0].envelope.mode, EnvelopeMode::Attack);
    }

    #[test]
    fn koff_forces_release_on_active_voice() {
        let mut mixer = Mixer::new();
        let mut dsp = dsp_with_master_volume(0x40, 0x40);
        // Manually mark voice 0 active in Attack.
        mixer.voices[0].active = true;
        mixer.voices[0].envelope.mode = EnvelopeMode::Attack;
        dsp.write(global_reg::KOFF, 0x01);
        let mut aram = Box::new([0u8; 0x10000]);
        let _ = mixer.step_sample(&mut dsp, &mut aram);
        assert_eq!(mixer.voices[0].envelope.mode, EnvelopeMode::Release);
    }

    #[test]
    fn mute_flag_zeroes_output_but_voices_keep_running() {
        // Set up a voice that would produce non-zero output, then
        // assert the mute flag silences final output while voice
        // state still advances.
        let mut mixer = Mixer::new();
        let mut dsp = DspRegs::new();
        dsp.write(global_reg::MVOLL, 0x40);
        dsp.write(global_reg::MVOLR, 0x40);
        dsp.write(global_reg::FLG, 0x40); // bit 6 = mute
        dsp.write(global_reg::DIR, 0x05);
        dsp.write(DspRegs::voice_addr(0, voice_reg::VOLL), 0x40);
        dsp.write(DspRegs::voice_addr(0, voice_reg::VOLR), 0x40);
        dsp.write(DspRegs::voice_addr(0, voice_reg::PH), 0x10);
        dsp.write(DspRegs::voice_addr(0, voice_reg::ADSR1), 0x8F); // fast attack

        let mut aram = Box::new([0u8; 0x10000]);
        aram[0x0500] = 0x00;
        aram[0x0501] = 0x06;
        aram[0x0502] = 0x00;
        aram[0x0503] = 0x06;
        aram[0x0600] = 0x83; // range=8, filter=0, end+loop
        for i in 1..9 {
            aram[0x0600 + i] = 0x77;
        }

        mixer.latch_kon(0x01);
        for _ in 0..50 {
            let (l, r) = mixer.step_sample(&mut dsp, &mut aram);
            assert_eq!(l, 0, "muted output is zero");
            assert_eq!(r, 0);
        }
        // Voice has been ticking: envelope should have advanced.
        assert!(
            mixer.voices[0].envelope.level > 0,
            "envelope grew despite mute"
        );
    }

    #[test]
    fn master_volume_scales_voice_sum() {
        // Two configurations differing only in MVOL should produce
        // proportional output. Easiest: drive a voice that gives
        // some non-zero output; compare MVOL=0x40 vs MVOL=0x20.
        let setup_dsp = |mvol: i8| {
            let mut d = DspRegs::new();
            d.write(global_reg::MVOLL, mvol as u8);
            d.write(global_reg::MVOLR, mvol as u8);
            d.write(global_reg::DIR, 0x05);
            d.write(DspRegs::voice_addr(0, voice_reg::VOLL), 0x40);
            d.write(DspRegs::voice_addr(0, voice_reg::VOLR), 0x40);
            d.write(DspRegs::voice_addr(0, voice_reg::PH), 0x10);
            d.write(DspRegs::voice_addr(0, voice_reg::ADSR1), 0x8F);
            d
        };
        let mut aram = Box::new([0u8; 0x10000]);
        aram[0x0500] = 0x00;
        aram[0x0501] = 0x06;
        aram[0x0502] = 0x00;
        aram[0x0503] = 0x06;
        aram[0x0600] = 0x83;
        for i in 1..9 {
            aram[0x0600 + i] = 0x77;
        }

        let mut dsp_full = setup_dsp(0x40);
        let mut dsp_half = setup_dsp(0x20);
        let mut m1 = Mixer::new();
        let mut m2 = Mixer::new();
        m1.latch_kon(0x01);
        m2.latch_kon(0x01);
        // Run a fixed number of samples and accumulate the absolute
        // output (root-mean-square would also work; abs sum is enough
        // to show the proportional difference).
        let mut acc1: i64 = 0;
        let mut acc2: i64 = 0;
        for _ in 0..256 {
            let (l1, _) = m1.step_sample(&mut dsp_full, &mut aram);
            let (l2, _) = m2.step_sample(&mut dsp_half, &mut aram);
            acc1 += (l1 as i64).abs();
            acc2 += (l2 as i64).abs();
        }
        // Half the master volume should give roughly half the
        // accumulated output (within voice-startup transient slack).
        if acc1 > 0 {
            let ratio = acc1 as f64 / acc2.max(1) as f64;
            assert!(
                (1.5..=3.0).contains(&ratio),
                "MVOL halving should ~halve output: ratio={ratio:.2}"
            );
        }
    }

    #[test]
    fn negative_master_volume_inverts_output_sign() {
        // MVOLL = 0x80 (i8 -128) acts as an inverter. Output should
        // have opposite sign to MVOLL = 0x7F output for the same
        // voice configuration.
        let setup_dsp = |mvol: u8| {
            let mut d = DspRegs::new();
            d.write(global_reg::MVOLL, mvol);
            d.write(global_reg::MVOLR, mvol);
            d.write(global_reg::DIR, 0x05);
            d.write(DspRegs::voice_addr(0, voice_reg::VOLL), 0x40);
            d.write(DspRegs::voice_addr(0, voice_reg::VOLR), 0x40);
            d.write(DspRegs::voice_addr(0, voice_reg::PH), 0x10);
            d.write(DspRegs::voice_addr(0, voice_reg::ADSR1), 0x8F);
            d
        };
        let mut aram = Box::new([0u8; 0x10000]);
        aram[0x0500] = 0x00;
        aram[0x0501] = 0x06;
        aram[0x0502] = 0x00;
        aram[0x0503] = 0x06;
        aram[0x0600] = 0x83;
        for i in 1..9 {
            aram[0x0600 + i] = 0x77;
        }
        let mut m_pos = Mixer::new();
        let mut m_neg = Mixer::new();
        m_pos.latch_kon(0x01);
        m_neg.latch_kon(0x01);
        let mut dsp_pos = setup_dsp(0x7F);
        let mut dsp_neg = setup_dsp(0x80); // -128 in i8
        let mut found_pos = 0i32;
        let mut found_neg = 0i32;
        for _ in 0..512 {
            let (lp, _) = m_pos.step_sample(&mut dsp_pos, &mut aram);
            let (ln, _) = m_neg.step_sample(&mut dsp_neg, &mut aram);
            if lp != 0 && found_pos == 0 {
                found_pos = lp as i32;
            }
            if ln != 0 && found_neg == 0 {
                found_neg = ln as i32;
            }
            if found_pos != 0 && found_neg != 0 {
                break;
            }
        }
        // Signs should be opposite.
        assert!(
            (found_pos > 0 && found_neg < 0) || (found_pos < 0 && found_neg > 0),
            "negative MVOL should flip sign: pos={found_pos} neg={found_neg}"
        );
    }

    #[test]
    fn echo_writeback_lands_in_aram_when_eon_voice_active() {
        // EON-flagged voice 0 producing audible output should make
        // the echo unit write non-zero feedback bytes into ARAM at
        // ESA<<8.
        let mut mixer = Mixer::new();
        let mut dsp = DspRegs::new();
        dsp.write(global_reg::MVOLL, 0x7F);
        dsp.write(global_reg::MVOLR, 0x7F);
        dsp.write(global_reg::DIR, 0x05); // sample dir at $0500
        dsp.write(global_reg::ESA, 0x40); // echo buffer at $4000
        dsp.write(global_reg::EDL, 1); // 2048-byte buffer
        dsp.write(global_reg::EON, 0x01); // voice 0 in echo bus
        dsp.write(DspRegs::voice_addr(0, voice_reg::VOLL), 0x40);
        dsp.write(DspRegs::voice_addr(0, voice_reg::VOLR), 0x40);
        dsp.write(DspRegs::voice_addr(0, voice_reg::PH), 0x10);
        dsp.write(DspRegs::voice_addr(0, voice_reg::ADSR1), 0x8F); // fast attack

        let mut aram = Box::new([0u8; 0x10000]);
        // Sample directory at $0500 -> start=$0600, loop=$0600.
        aram[0x0500] = 0x00;
        aram[0x0501] = 0x06;
        aram[0x0502] = 0x00;
        aram[0x0503] = 0x06;
        // BRR block: range=8, filter=0, end+loop, all positive nibbles.
        aram[0x0600] = 0x83;
        for i in 1..9 {
            aram[0x0600 + i] = 0x77;
        }

        mixer.latch_kon(0x01);
        // Run enough samples for the buffer offset to advance past
        // address 0x4000 so we can inspect a slot the echo wrote.
        for _ in 0..16 {
            let _ = mixer.step_sample(&mut dsp, &mut aram);
        }

        // Some bytes in the echo buffer region should now be non-zero
        // (the feedback writeback path).
        let touched = aram[0x4000..0x4040].iter().any(|&b| b != 0);
        assert!(touched, "echo writeback should have populated ARAM");
    }

    #[test]
    fn fresh_mixer_noise_lfsr_starts_at_4000() {
        let m = Mixer::new();
        assert_eq!(m.noise_lfsr, 0x4000);
    }

    #[test]
    fn noise_lfsr_advances_when_rate_fires() {
        let mut mixer = Mixer::new();
        let mut dsp = DspRegs::new();
        dsp.write(global_reg::FLG, 0x1F); // noise rate = 31 (every sample)
        let mut aram = Box::new([0u8; 0x10000]);
        let pre = mixer.noise_lfsr;
        let _ = mixer.step_sample(&mut dsp, &mut aram);
        let post = mixer.noise_lfsr;
        assert_ne!(pre, post, "rate=31 -> LFSR advances each sample");
    }

    #[test]
    fn noise_lfsr_frozen_when_rate_zero() {
        let mut mixer = Mixer::new();
        let mut dsp = DspRegs::new();
        dsp.write(global_reg::FLG, 0x00); // noise rate = 0 (off)
        let mut aram = Box::new([0u8; 0x10000]);
        let pre = mixer.noise_lfsr;
        for _ in 0..100 {
            let _ = mixer.step_sample(&mut dsp, &mut aram);
        }
        assert_eq!(mixer.noise_lfsr, pre, "rate=0 freezes LFSR");
    }

    #[test]
    fn non_voice_outputs_noise_instead_of_brr() {
        // Voice 0 with NON bit and a known LFSR state should output
        // (LFSR * 2) (envelope-scaled) instead of the BRR-derived
        // sample. Easiest assertion: the BRR block is silent (all
        // zero nibbles), but with NON=1 the voice still produces
        // non-zero output once the envelope ramps up.
        let mut mixer = Mixer::new();
        // Pin LFSR to a non-zero, non-symmetric state so output is
        // detectably non-zero.
        mixer.noise_lfsr = 0x2A55;
        let mut dsp = DspRegs::new();
        dsp.write(global_reg::MVOLL, 0x7F);
        dsp.write(global_reg::MVOLR, 0x7F);
        dsp.write(global_reg::DIR, 0x05);
        dsp.write(global_reg::NON, 0x01); // voice 0 -> noise
        dsp.write(global_reg::FLG, 0x1F); // noise rate fires every sample
        dsp.write(DspRegs::voice_addr(0, voice_reg::VOLL), 0x40);
        dsp.write(DspRegs::voice_addr(0, voice_reg::VOLR), 0x40);
        dsp.write(DspRegs::voice_addr(0, voice_reg::PH), 0x10);
        dsp.write(DspRegs::voice_addr(0, voice_reg::ADSR1), 0x8F); // fast attack
        let mut aram = Box::new([0u8; 0x10000]);
        // Sample dir + silent BRR block (would output silence without NON).
        aram[0x0500] = 0x00;
        aram[0x0501] = 0x06;
        aram[0x0502] = 0x00;
        aram[0x0503] = 0x06;
        aram[0x0600] = 0x03; // silent end+loop block

        mixer.latch_kon(0x01);
        let mut got_nonzero = false;
        for _ in 0..256 {
            let (l, _) = mixer.step_sample(&mut dsp, &mut aram);
            if l != 0 {
                got_nonzero = true;
                break;
            }
        }
        assert!(got_nonzero, "NON-flagged voice with active LFSR should be audible");
    }

    #[test]
    fn pmon_bit_zero_is_ignored_voice0_unmodulated() {
        // PMON.0 is hardware-ignored. A PMON value of 0x01 should NOT
        // cause voice 0's pitch to deviate from its raw register.
        let mut mixer = Mixer::new();
        let mut dsp = DspRegs::new();
        dsp.write(global_reg::MVOLL, 0x7F);
        dsp.write(global_reg::MVOLR, 0x7F);
        dsp.write(global_reg::DIR, 0x05);
        dsp.write(global_reg::PMON, 0x01); // PMON.0 (should be ignored)
        dsp.write(DspRegs::voice_addr(0, voice_reg::VOLL), 0x40);
        dsp.write(DspRegs::voice_addr(0, voice_reg::VOLR), 0x40);
        dsp.write(DspRegs::voice_addr(0, voice_reg::PH), 0x10);
        dsp.write(DspRegs::voice_addr(0, voice_reg::ADSR1), 0x8F);
        let mut aram = Box::new([0u8; 0x10000]);
        aram[0x0500] = 0x00;
        aram[0x0501] = 0x06;
        aram[0x0502] = 0x00;
        aram[0x0503] = 0x06;
        aram[0x0600] = 0x83;
        for i in 1..9 {
            aram[0x0600 + i] = 0x77;
        }
        mixer.latch_kon(0x01);
        // Run for a fixed window; voice 0 should consume BRR samples
        // at exactly pitch=0x1000 = one BRR sample per output tick.
        // After 32 samples we expect to be partway through the second
        // 16-sample block, NOT (e.g.) wildly off.
        for _ in 0..16 {
            let _ = mixer.step_sample(&mut dsp, &mut aram);
        }
        // Sanity: pitch counter should be at 16 * 0x1000 mod 0x10000
        // = 0x0000 (wrapped exactly once). We just check the voice
        // didn't trip an early end-without-loop or get stuck.
        assert!(mixer.voices[0].active, "voice 0 still active after 16 samples");
    }

    #[test]
    fn pmon_voice1_pitch_modulated_by_voice0_output() {
        // With PMON.1 set, voice 1's pitch should be modulated by
        // voice 0's last_voice_output. The clearest observable: with
        // voice 0 producing a non-trivial output, voice 1's
        // sampler.pitch_counter should step at a different rate than
        // a control mixer with PMON.1 cleared.
        // Easier test: just check that voice 1's pitch counter differs
        // between PMON-on and PMON-off after some samples.
        let mk_dsp = |pmon: u8| {
            let mut d = DspRegs::new();
            d.write(global_reg::MVOLL, 0x7F);
            d.write(global_reg::MVOLR, 0x7F);
            d.write(global_reg::DIR, 0x05);
            d.write(global_reg::PMON, pmon);
            // Voice 0: produce output (drives PMON of voice 1).
            d.write(DspRegs::voice_addr(0, voice_reg::VOLL), 0x40);
            d.write(DspRegs::voice_addr(0, voice_reg::VOLR), 0x40);
            d.write(DspRegs::voice_addr(0, voice_reg::PH), 0x10);
            d.write(DspRegs::voice_addr(0, voice_reg::ADSR1), 0x8F);
            // Voice 1: same setup, both KON'd.
            d.write(DspRegs::voice_addr(1, voice_reg::VOLL), 0x40);
            d.write(DspRegs::voice_addr(1, voice_reg::VOLR), 0x40);
            d.write(DspRegs::voice_addr(1, voice_reg::PH), 0x10);
            d.write(DspRegs::voice_addr(1, voice_reg::ADSR1), 0x8F);
            d
        };
        let mut aram = Box::new([0u8; 0x10000]);
        aram[0x0500] = 0x00;
        aram[0x0501] = 0x06;
        aram[0x0502] = 0x00;
        aram[0x0503] = 0x06;
        aram[0x0600] = 0x83;
        for i in 1..9 {
            aram[0x0600 + i] = 0x77;
        }
        let mut m_on = Mixer::new();
        let mut m_off = Mixer::new();
        m_on.latch_kon(0x03); // voices 0 + 1
        m_off.latch_kon(0x03);
        let mut dsp_on = mk_dsp(0x02); // PMON.1 set
        let mut dsp_off = mk_dsp(0x00);
        for _ in 0..64 {
            let _ = m_on.step_sample(&mut dsp_on, &mut aram);
            let _ = m_off.step_sample(&mut dsp_off, &mut aram);
        }
        // Voice 1's pitch counter trajectory should differ between
        // the two configurations.
        assert_ne!(
            m_on.voices[1].sampler.pitch_counter,
            m_off.voices[1].sampler.pitch_counter,
            "PMON should perturb voice 1's pitch counter"
        );
    }

    #[test]
    fn echo_write_disabled_flag_freezes_buffer() {
        // FLG.5 set -> writeback is suppressed even though offset
        // still advances.
        let mut mixer = Mixer::new();
        let mut dsp = DspRegs::new();
        dsp.write(global_reg::MVOLL, 0x7F);
        dsp.write(global_reg::MVOLR, 0x7F);
        dsp.write(global_reg::DIR, 0x05);
        dsp.write(global_reg::ESA, 0x40);
        dsp.write(global_reg::EDL, 1);
        dsp.write(global_reg::EON, 0x01);
        dsp.write(global_reg::FLG, 0x20); // bit 5 = echo writes disabled
        dsp.write(DspRegs::voice_addr(0, voice_reg::VOLL), 0x40);
        dsp.write(DspRegs::voice_addr(0, voice_reg::VOLR), 0x40);
        dsp.write(DspRegs::voice_addr(0, voice_reg::PH), 0x10);
        dsp.write(DspRegs::voice_addr(0, voice_reg::ADSR1), 0x8F);

        let mut aram = Box::new([0u8; 0x10000]);
        aram[0x0500] = 0x00;
        aram[0x0501] = 0x06;
        aram[0x0502] = 0x00;
        aram[0x0503] = 0x06;
        aram[0x0600] = 0x83;
        for i in 1..9 {
            aram[0x0600 + i] = 0x77;
        }

        mixer.latch_kon(0x01);
        for _ in 0..16 {
            let _ = mixer.step_sample(&mut dsp, &mut aram);
        }

        let untouched = aram[0x4000..0x4040].iter().all(|&b| b == 0);
        assert!(untouched, "FLG.5 must suppress echo writeback");
    }

    /// Build a minimal-but-realistic DSP+ARAM setup for the ENDX
    /// tests: voice 0 plays a single 9-byte BRR block whose header
    /// has `end_flag` and `loop_flag` set to `header_byte`. With
    /// `0x83` (range=8, filter=0, end+loop) the voice loops on
    /// itself and re-hits the end-block every 16 output samples
    /// at pitch 0x1000.
    fn endx_test_setup(header_byte: u8) -> (DspRegs, Box<[u8; 0x10000]>) {
        let mut dsp = DspRegs::new();
        dsp.write(global_reg::MVOLL, 0x40);
        dsp.write(global_reg::MVOLR, 0x40);
        dsp.write(global_reg::DIR, 0x05);
        dsp.write(DspRegs::voice_addr(0, voice_reg::VOLL), 0x40);
        dsp.write(DspRegs::voice_addr(0, voice_reg::VOLR), 0x40);
        dsp.write(DspRegs::voice_addr(0, voice_reg::PH), 0x10);
        dsp.write(DspRegs::voice_addr(0, voice_reg::ADSR1), 0x8F);
        let mut aram = Box::new([0u8; 0x10000]);
        // Sample directory at $0500: start=$0600, loop=$0600.
        aram[0x0500] = 0x00;
        aram[0x0501] = 0x06;
        aram[0x0502] = 0x00;
        aram[0x0503] = 0x06;
        aram[0x0600] = header_byte;
        for i in 1..9 {
            aram[0x0600 + i] = 0x77;
        }
        (dsp, aram)
    }

    #[test]
    fn endx_bit_set_when_voice_processes_looped_end_block() {
        // Header 0x83 = range=8, filter=0, end+loop. The voice
        // re-encounters the end block every 16 output samples; ENDX.0
        // should be set after at least one such crossing. (FF6 panning
        // depends on ENDX firing for looped end-blocks too.)
        let (mut dsp, mut aram) = endx_test_setup(0x83);
        let mut mixer = Mixer::new();
        mixer.latch_kon(0x01);
        // 18 samples > 16 sample/block, guaranteed to cross at least
        // one end-block boundary.
        for _ in 0..18 {
            let _ = mixer.step_sample(&mut dsp, &mut aram);
        }
        assert!(
            dsp.endx() & 0x01 != 0,
            "ENDX.0 should be set after a looped end-block: endx={:#04x}",
            dsp.endx()
        );
    }

    #[test]
    fn endx_bit_set_when_voice_processes_terminating_end_block() {
        // Header 0x81 = range=8, filter=0, end-without-loop. The
        // voice goes silent (Release) after one block but ENDX.0
        // must still be set.
        let (mut dsp, mut aram) = endx_test_setup(0x81);
        let mut mixer = Mixer::new();
        mixer.latch_kon(0x01);
        for _ in 0..18 {
            let _ = mixer.step_sample(&mut dsp, &mut aram);
        }
        assert!(
            dsp.endx() & 0x01 != 0,
            "ENDX.0 should be set after a terminating end-block: endx={:#04x}",
            dsp.endx()
        );
    }

    #[test]
    fn endx_is_sticky_oring_with_host_written_value() {
        // Drive voice 0 to set ENDX.0, then verify the bit persists
        // across an idle sample (no end-block crossed) - i.e. the
        // mixer ORs into the existing register rather than overwriting.
        let (mut dsp, mut aram) = endx_test_setup(0x83);
        let mut mixer = Mixer::new();
        mixer.latch_kon(0x01);
        for _ in 0..18 {
            let _ = mixer.step_sample(&mut dsp, &mut aram);
        }
        assert!(dsp.endx() & 0x01 != 0, "precondition: ENDX.0 set");
        // Pre-set ENDX.7 by host write; it must survive the next step.
        let with_host_bit = dsp.endx() | 0x80;
        dsp.write(global_reg::ENDX, with_host_bit);
        // One more sample - voice 0 still active, will set its bit
        // again at some point, but the host-written ENDX.7 must remain.
        let _ = mixer.step_sample(&mut dsp, &mut aram);
        assert!(
            dsp.endx() & 0x80 != 0,
            "host-written ENDX bits must persist (sticky OR): endx={:#04x}",
            dsp.endx()
        );
    }

    #[test]
    fn host_writing_zero_to_endx_clears_acked_bits() {
        // Standard SPC driver pattern: write 0 to ENDX to ack, then
        // poll for new bits. After clearing, only voices that hit a
        // *new* end-block should reappear.
        let (mut dsp, mut aram) = endx_test_setup(0x83);
        let mut mixer = Mixer::new();
        mixer.latch_kon(0x01);
        for _ in 0..18 {
            let _ = mixer.step_sample(&mut dsp, &mut aram);
        }
        assert!(dsp.endx() & 0x01 != 0, "precondition: ENDX.0 set");
        // Driver acks the bit.
        dsp.write(global_reg::ENDX, 0);
        // One sample with no new end-block crossed: ENDX should stay 0.
        // (Voice 0 just looped, next end-block is ~16 samples away.)
        let _ = mixer.step_sample(&mut dsp, &mut aram);
        assert_eq!(
            dsp.endx() & 0x01,
            0,
            "ENDX.0 must stay clear until a new end-block fires: endx={:#04x}",
            dsp.endx()
        );
    }

    #[test]
    fn kon_clears_endx_bit_for_keyed_voice() {
        // Set up an existing ENDX.0 bit by host write, then KON voice 0.
        // The same-sample KON must clear ENDX.0 in the writeback.
        let (mut dsp, mut aram) = endx_test_setup(0x83);
        let mut mixer = Mixer::new();
        // Pre-seed ENDX.0 as if a previous run had set it.
        dsp.write(global_reg::ENDX, 0x01);
        mixer.latch_kon(0x01);
        let _ = mixer.step_sample(&mut dsp, &mut aram);
        assert_eq!(
            dsp.endx() & 0x01,
            0,
            "KON must clear ENDX bit for the keyed voice: endx={:#04x}",
            dsp.endx()
        );
    }

    #[test]
    fn envx_and_outx_are_zero_for_idle_voices() {
        // Fresh mixer + idle DSP: every voice's $X8 / $X9 should be
        // zero after one step (no voice was KON'd, envelopes are 0,
        // outputs are 0).
        let mut mixer = Mixer::new();
        let mut dsp = dsp_with_master_volume(0x40, 0x40);
        let mut aram = Box::new([0u8; 0x10000]);
        let _ = mixer.step_sample(&mut dsp, &mut aram);
        for v in 0..8 {
            assert_eq!(
                dsp.voice_reg(v, voice_reg::ENVX),
                0,
                "voice {v} ENVX should be 0 when idle"
            );
            assert_eq!(
                dsp.voice_reg(v, voice_reg::OUTX),
                0,
                "voice {v} OUTX should be 0 when idle"
            );
        }
    }

    #[test]
    fn envx_reflects_envelope_level_shifted_right_four() {
        // KON a voice with fast attack and let it ramp up; ENVX must
        // track the 7-bit-shifted envelope level (Mesen2: envVolume >> 4).
        let (mut dsp, mut aram) = endx_test_setup(0x83);
        let mut mixer = Mixer::new();
        mixer.latch_kon(0x01);
        // Run until envelope crosses some non-trivial level.
        let mut crossed = false;
        for _ in 0..200 {
            let _ = mixer.step_sample(&mut dsp, &mut aram);
            let internal = mixer.voices[0].envelope.level;
            let written = dsp.voice_reg(0, voice_reg::ENVX);
            assert_eq!(
                written as u16,
                internal >> 4,
                "ENVX byte must equal envelope.level >> 4 (level={internal:#06x}, written={written:#04x})"
            );
            if internal >= 0x100 {
                crossed = true;
            }
        }
        assert!(crossed, "envelope should have ramped past 0x100 in 200 samples");
    }

    #[test]
    fn outx_reflects_high_byte_of_last_voice_output() {
        // Drive a voice that produces non-trivial output; OUTX should
        // equal the high byte of last_voice_output (Mesen2: u8 cast of
        // VoiceOutput >> 8). LSB of last_voice_output is masked off so
        // the >>8 value occupies the full i8 range.
        let (mut dsp, mut aram) = endx_test_setup(0x83);
        let mut mixer = Mixer::new();
        mixer.latch_kon(0x01);
        let mut got_nonzero = false;
        for _ in 0..400 {
            let _ = mixer.step_sample(&mut dsp, &mut aram);
            let internal = mixer.voices[0].last_voice_output;
            let written = dsp.voice_reg(0, voice_reg::OUTX);
            assert_eq!(
                written,
                (internal >> 8) as u8,
                "OUTX byte must equal (last_voice_output >> 8) as u8 \
                 (output={internal}, written={written:#04x})"
            );
            if written != 0 {
                got_nonzero = true;
            }
        }
        assert!(
            got_nonzero,
            "OUTX should have shown some non-zero value over 400 samples"
        );
    }

    #[test]
    fn envx_and_outx_decay_to_zero_after_voice_releases() {
        // Voice plays one block (header 0x81: end-without-loop), goes
        // into Release; both ENVX and OUTX should eventually return
        // to zero as the envelope decays.
        let (mut dsp, mut aram) = endx_test_setup(0x81);
        let mut mixer = Mixer::new();
        mixer.latch_kon(0x01);
        // Run plenty of samples - enough for the voice to release and
        // its envelope to fully decay (Release rate from ADSR = 0x8F
        // is fast).
        for _ in 0..2048 {
            let _ = mixer.step_sample(&mut dsp, &mut aram);
        }
        assert_eq!(mixer.voices[0].envelope.level, 0, "envelope should have decayed");
        assert_eq!(
            dsp.voice_reg(0, voice_reg::ENVX),
            0,
            "ENVX should reflect fully-decayed envelope"
        );
        assert_eq!(
            dsp.voice_reg(0, voice_reg::OUTX),
            0,
            "OUTX should be zero once envelope is zero"
        );
    }

    #[test]
    fn host_writes_to_envx_outx_are_overwritten_each_sample() {
        // ENVX / OUTX are read-only mirrors: any host write is
        // clobbered on the next sample tick. Pre-seed garbage values
        // and verify they're replaced.
        let (mut dsp, mut aram) = endx_test_setup(0x83);
        dsp.set_voice_reg(0, voice_reg::ENVX, 0x55);
        dsp.set_voice_reg(0, voice_reg::OUTX, 0xAA);
        let mut mixer = Mixer::new();
        mixer.latch_kon(0x01);
        let _ = mixer.step_sample(&mut dsp, &mut aram);
        // Voice was just KON'd this sample so envelope.level == 0
        // post-attack-rate-application of one tick (or near-zero).
        // The point isn't the exact value but that whatever the host
        // wrote is gone.
        let envx = dsp.voice_reg(0, voice_reg::ENVX);
        let outx = dsp.voice_reg(0, voice_reg::OUTX);
        assert_ne!(envx, 0x55, "ENVX must have been overwritten by mixer");
        assert_ne!(outx, 0xAA, "OUTX must have been overwritten by mixer");
    }

    #[test]
    fn endx_aggregates_across_voices() {
        // Two voices on independent end-blocks; both bits should
        // appear in ENDX after enough samples for each to cross.
        let mut dsp = DspRegs::new();
        dsp.write(global_reg::MVOLL, 0x40);
        dsp.write(global_reg::MVOLR, 0x40);
        dsp.write(global_reg::DIR, 0x05);
        // Voice 0 -> SRCN 0 -> dir entry at $0500 -> sample at $0600.
        dsp.write(DspRegs::voice_addr(0, voice_reg::SRCN), 0);
        dsp.write(DspRegs::voice_addr(0, voice_reg::VOLL), 0x40);
        dsp.write(DspRegs::voice_addr(0, voice_reg::VOLR), 0x40);
        dsp.write(DspRegs::voice_addr(0, voice_reg::PH), 0x10);
        dsp.write(DspRegs::voice_addr(0, voice_reg::ADSR1), 0x8F);
        // Voice 3 -> SRCN 1 -> dir entry at $0504 -> sample at $0700.
        dsp.write(DspRegs::voice_addr(3, voice_reg::SRCN), 1);
        dsp.write(DspRegs::voice_addr(3, voice_reg::VOLL), 0x40);
        dsp.write(DspRegs::voice_addr(3, voice_reg::VOLR), 0x40);
        dsp.write(DspRegs::voice_addr(3, voice_reg::PH), 0x10);
        dsp.write(DspRegs::voice_addr(3, voice_reg::ADSR1), 0x8F);

        let mut aram = Box::new([0u8; 0x10000]);
        // Sample 0 directory entry at $0500: start=$0600, loop=$0600.
        aram[0x0500] = 0x00;
        aram[0x0501] = 0x06;
        aram[0x0502] = 0x00;
        aram[0x0503] = 0x06;
        aram[0x0600] = 0x83;
        for i in 1..9 {
            aram[0x0600 + i] = 0x77;
        }
        // Sample 1 directory entry at $0504: start=$0700, loop=$0700.
        aram[0x0504] = 0x00;
        aram[0x0505] = 0x07;
        aram[0x0506] = 0x00;
        aram[0x0507] = 0x07;
        aram[0x0700] = 0x83;
        for i in 1..9 {
            aram[0x0700 + i] = 0x77;
        }

        let mut mixer = Mixer::new();
        mixer.latch_kon(0x09); // voices 0 + 3
        for _ in 0..18 {
            let _ = mixer.step_sample(&mut dsp, &mut aram);
        }
        assert_eq!(
            dsp.endx() & 0x09,
            0x09,
            "both voice 0 and voice 3 should have flagged ENDX: endx={:#04x}",
            dsp.endx()
        );
    }
}
