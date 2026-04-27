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
//! ## Deferred for future sub-phases
//!
//! - **5c.7** noise + pitch modulation: per-voice noise replacement
//!   + per-voice PMON of the previous voice's last_raw_sample.
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
}

impl Mixer {
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
    /// providing a 64 KiB ARAM slice.
    pub fn step_sample(&mut self, dsp: &DspRegs, aram: &mut [u8; 0x10000]) -> (i16, i16) {
        // Tick the global counter once per output sample, BEFORE
        // running any voice (so all voices see the same value).
        self.counter.tick();

        // Resolve KOFF bitmask.
        let koff = dsp.koff();
        let eon = dsp.eon();

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

        // Sum per-voice contributions; collect EON-flagged voices
        // separately for the echo feedback path.
        let mut sum_l: i32 = 0;
        let mut sum_r: i32 = 0;
        let mut eon_sum_l: i32 = 0;
        let mut eon_sum_r: i32 = 0;
        for v in 0..8 {
            let pitch = dsp.voice_pitch(v);
            let voll = dsp.voice_volume_left(v);
            let volr = dsp.voice_volume_right(v);
            let adsr1 = dsp.voice_adsr1(v);
            let adsr2 = dsp.voice_adsr2(v);
            let gain = dsp.voice_gain(v);
            let (l, r) = self.voices[v].step(
                pitch,
                voll,
                volr,
                adsr1,
                adsr2,
                gain,
                &self.counter,
                aram,
            );
            sum_l = sum_l.saturating_add(l);
            sum_r = sum_r.saturating_add(r);
            if eon & (1u8 << v) != 0 {
                eon_sum_l = eon_sum_l.saturating_add(l);
                eon_sum_r = eon_sum_r.saturating_add(r);
            }
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
        let dsp = dsp_with_master_volume(0x40, 0x40);
        let mut aram = Box::new([0u8; 0x10000]);
        let (l, r) = mixer.step_sample(&dsp, &mut aram);
        assert_eq!(l, 0);
        assert_eq!(r, 0);
    }

    #[test]
    fn counter_ticks_once_per_sample() {
        let mut mixer = Mixer::new();
        let dsp = dsp_with_master_volume(0x40, 0x40);
        let mut aram = Box::new([0u8; 0x10000]);
        for _ in 0..5 {
            let _ = mixer.step_sample(&dsp, &mut aram);
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
        let _ = mixer.step_sample(&dsp, &mut aram);
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
        let _ = mixer.step_sample(&dsp, &mut aram);
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
            let (l, r) = mixer.step_sample(&dsp, &mut aram);
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

        let dsp_full = setup_dsp(0x40);
        let dsp_half = setup_dsp(0x20);
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
            let (l1, _) = m1.step_sample(&dsp_full, &mut aram);
            let (l2, _) = m2.step_sample(&dsp_half, &mut aram);
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
        let dsp_pos = setup_dsp(0x7F);
        let dsp_neg = setup_dsp(0x80); // -128 in i8
        let mut found_pos = 0i32;
        let mut found_neg = 0i32;
        for _ in 0..512 {
            let (lp, _) = m_pos.step_sample(&dsp_pos, &mut aram);
            let (ln, _) = m_neg.step_sample(&dsp_neg, &mut aram);
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
            let _ = mixer.step_sample(&dsp, &mut aram);
        }

        // Some bytes in the echo buffer region should now be non-zero
        // (the feedback writeback path).
        let touched = aram[0x4000..0x4040].iter().any(|&b| b != 0);
        assert!(touched, "echo writeback should have populated ARAM");
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
            let _ = mixer.step_sample(&dsp, &mut aram);
        }

        let untouched = aram[0x4000..0x4040].iter().all(|&b| b == 0);
        assert!(untouched, "FLG.5 must suppress echo writeback");
    }
}
