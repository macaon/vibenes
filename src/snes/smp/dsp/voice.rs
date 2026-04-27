//! Per-voice S-DSP runtime.
//!
//! Wires the BRR decoder, the Gaussian voice sampler, and the ADSR /
//! GAIN envelope generator together to produce one stereo sample per
//! 32 kHz tick. Each [`Voice`] owns the per-voice state; the master
//! mixer (see [`super::mixer`]) drives 8 voices in lockstep and sums
//! their outputs.
//!
//! ## Pipeline (per output sample)
//!
//! ```text
//!   if KON pending:
//!       voice.start(srcn, sample_dir, aram)        // load BRR ptrs
//!   if KOFF for this voice:
//!       voice.envelope.key_off()                   // jump to Release
//!   while voice.sampler.pending_decodes(pitch) > 0:
//!       voice.advance_brr(aram)                    // pull next sample
//!   raw = voice.sampler.step(pitch)                // Gaussian
//!   step_envelope(...)                             // 11-bit envelope
//!   shaped = (raw * envelope_level) / 2048
//!   left   = (shaped * voll) / 128
//!   right  = (shaped * volr) / 128
//! ```
//!
//! ## Deferred features
//!
//! - **KON delay**: hardware spends 5 samples after KON priming the
//!   buffer before audible output starts. We start audible output
//!   immediately after the 4-sample warm-up (which is implicit in the
//!   sampler since the buffer fills as samples are pushed). Most
//!   music is unaffected.
//! - **Pitch modulation** (PMON), **noise** (NON), **echo** (EON):
//!   land in Phase 5c.6 / 5c.7. The voice exposes `last_raw_sample`
//!   so a future PMON stage can read voice n-1's output.
//! - **ENDX / OUTX** voice-status registers: writeback is not yet
//!   plumbed; the voice records the latest values in
//!   [`Voice::endx_pending`] / [`Voice::last_raw_sample`] so the
//!   mixer can publish them when the global register policy is
//!   wired up.
//!
//! ## Sources
//!
//! - `~/.claude/skills/nes-expert/reference/snes-apu.md` §"Voice
//!   processing pipeline".
//! - Mesen2 `Core/SNES/DSP/DspVoice.cpp::Run` for the per-sample step
//!   sequence (Step1..Step5 in their five-cycle pipeline; we collapse
//!   to one entry point per output sample).
//! - higan `sfc/dsp/dsp.cpp::voiceOutput` as cross-check.

use super::brr::{decode_block, BrrHeader, BrrState};
use super::envelope::{step_envelope, EnvelopeCounter, EnvelopeMode, EnvelopeState};
use super::voice_sampler::VoiceSampler;

/// Per-voice DSP state.
#[derive(Debug, Clone)]
pub struct Voice {
    pub brr_state: BrrState,
    pub sampler: VoiceSampler,
    pub envelope: EnvelopeState,
    /// Address in ARAM of the **first byte** of the current 9-byte
    /// BRR block. Advances by 9 each block. Loaded from the sample
    /// directory entry at KON, then walked forward.
    pub brr_addr: u16,
    /// Loop-target address for the current sample, loaded once at
    /// KON from the directory. Used when an end-with-loop block is
    /// hit.
    pub loop_addr: u16,
    /// Cached decoded samples for the current block (16 entries).
    /// Filled by [`Voice::decode_current_block`]; consumed one at a
    /// time as the pitch counter advances past 0x1000 boundaries.
    pub block_samples: [i16; 16],
    /// Index into `block_samples` of the next sample that needs to
    /// be pushed into `sampler`. Reaches 16 when the block is fully
    /// consumed; that triggers a new-block fetch.
    pub block_index: u8,
    /// Cached header of the current block - used to know when to
    /// loop / end.
    pub block_header: BrrHeader,
    /// True between [`Voice::start`] and the next end-without-loop
    /// terminator. Voices in inactive state contribute silence.
    pub active: bool,
    /// `true` if the voice's most recently decoded block had its end
    /// flag set (regardless of loop). The mixer publishes the OR
    /// across voices into the global ENDX register when wired up.
    pub endx_pending: bool,
    /// Last per-sample raw output (post-Gaussian, pre-envelope, pre-
    /// volume). Exposed for pitch-modulation in the next voice.
    pub last_raw_sample: i16,
}

impl Voice {
    pub const fn new() -> Self {
        Self {
            brr_state: BrrState::new(),
            sampler: VoiceSampler::new(),
            envelope: EnvelopeState::new(),
            brr_addr: 0,
            loop_addr: 0,
            block_samples: [0; 16],
            block_index: 16, // forces a fetch on first sample
            block_header: BrrHeader {
                range: 0,
                filter: 0,
                loop_flag: false,
                end_flag: false,
            },
            active: false,
            endx_pending: false,
            last_raw_sample: 0,
        }
    }

    /// Reset to power-on state. Called when the voice is re-used.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// KON entry: read the sample directory at `dir_base + srcn*4`
    /// for the start and loop addresses, prime BRR / sampler /
    /// envelope state, and mark the voice active.
    pub fn start(&mut self, srcn: u8, dir_base: u16, aram: &[u8; 0x10000]) {
        let entry = dir_base.wrapping_add((srcn as u16).wrapping_mul(4));
        let start_lo = aram[entry as usize] as u16;
        let start_hi = aram[entry.wrapping_add(1) as usize] as u16;
        let loop_lo = aram[entry.wrapping_add(2) as usize] as u16;
        let loop_hi = aram[entry.wrapping_add(3) as usize] as u16;
        self.brr_addr = (start_hi << 8) | start_lo;
        self.loop_addr = (loop_hi << 8) | loop_lo;
        self.brr_state = BrrState::new();
        self.sampler.reset();
        self.envelope.key_on();
        self.block_index = 16; // request a fetch on next step
        self.block_header = BrrHeader {
            range: 0,
            filter: 0,
            loop_flag: false,
            end_flag: false,
        };
        self.active = true;
        self.endx_pending = false;
        self.last_raw_sample = 0;
    }

    /// Decode the 9-byte block starting at `brr_addr` from ARAM into
    /// `block_samples`, updating `brr_state` and `block_header`. The
    /// block_index is reset to 0 so subsequent steps consume the
    /// freshly-decoded samples.
    fn decode_current_block(&mut self, aram: &[u8; 0x10000]) {
        let mut block = [0u8; 9];
        for (i, slot) in block.iter_mut().enumerate() {
            *slot = aram[(self.brr_addr.wrapping_add(i as u16)) as usize];
        }
        let (samples, header) = decode_block(&block, &mut self.brr_state);
        self.block_samples = samples;
        self.block_header = header;
        self.block_index = 0;
    }

    /// Advance the BRR pointer to the next block, looping or stopping
    /// according to the current block's flags.
    fn advance_block(&mut self) {
        if self.block_header.end_flag {
            self.endx_pending = true;
            if self.block_header.loop_flag {
                self.brr_addr = self.loop_addr;
            } else {
                // End without loop: voice goes silent (Release path
                // via envelope). The runtime is responsible for
                // calling key_off on the envelope to ramp it down.
                self.envelope.mode = EnvelopeMode::Release;
                self.active = false;
                return;
            }
        } else {
            self.brr_addr = self.brr_addr.wrapping_add(9);
        }
    }

    /// Push the next decoded sample into the sampler buffer; fetch /
    /// advance to a new block as needed.
    fn push_one(&mut self, aram: &[u8; 0x10000]) {
        if self.block_index >= 16 {
            self.decode_current_block(aram);
        }
        let s = self.block_samples[self.block_index as usize];
        self.sampler.push_sample(s);
        self.block_index = self.block_index.wrapping_add(1);
        if self.block_index >= 16 {
            self.advance_block();
        }
    }

    /// Per-sample step. Returns `(left, right)` post-volume voice
    /// contributions (signed, pre-master-volume). Caller is
    /// responsible for applying KON/KOFF outside the voice.
    ///
    /// Inputs:
    /// - `pitch`: 14-bit pitch register for this voice (pre-PMON).
    /// - `voll` / `volr`: signed 8-bit per-voice volume.
    /// - `adsr1` / `adsr2` / `gain`: envelope control bytes.
    /// - `counter`: global envelope rate counter.
    /// - `aram`: 64 KiB audio RAM slice for BRR fetch.
    pub fn step(
        &mut self,
        pitch: u16,
        voll: i8,
        volr: i8,
        adsr1: u8,
        adsr2: u8,
        gain: u8,
        counter: &EnvelopeCounter,
        aram: &[u8; 0x10000],
    ) -> (i32, i32) {
        // Inactive voices contribute silence but still tick their
        // envelope so the host's ENVX read stays sensible.
        if !self.active {
            step_envelope(&mut self.envelope, adsr1, adsr2, gain, counter);
            self.last_raw_sample = 0;
            return (0, 0);
        }

        // Pull as many BRR samples as needed to satisfy this tick's
        // pitch step.
        let needed = self.sampler.pending_decodes(pitch);
        for _ in 0..needed {
            self.push_one(aram);
            if !self.active {
                // End-without-loop tripped during the fill - bail.
                step_envelope(&mut self.envelope, adsr1, adsr2, gain, counter);
                self.last_raw_sample = 0;
                return (0, 0);
            }
        }

        // Gaussian-interpolate at the current pitch counter, then
        // advance.
        let raw = self.sampler.step(pitch);
        self.last_raw_sample = raw;

        // Run envelope for this sample.
        step_envelope(&mut self.envelope, adsr1, adsr2, gain, counter);

        // Apply envelope: shaped = (raw * env) / 2048. The envelope
        // is unsigned 11-bit; raw is signed 16-bit; the product fits
        // in i32 with headroom.
        let env = self.envelope.level as i32;
        let shaped = (raw as i32 * env) >> 11;

        // Apply per-voice volume (signed 8-bit, /128 normalisation).
        let left = (shaped * voll as i32) >> 7;
        let right = (shaped * volr as i32) >> 7;
        (left, right)
    }
}

impl Default for Voice {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aram_with_silent_block() -> Box<[u8; 0x10000]> {
        // A directory entry at $1000 pointing at a silent BRR block
        // at $1100. Block: header = 0x03 (range=0, filter=0, loop+end),
        // bytes 1-8 all zero.
        let mut aram = Box::new([0u8; 0x10000]);
        aram[0x1000] = 0x00; // start lo
        aram[0x1001] = 0x11; // start hi -> 0x1100
        aram[0x1002] = 0x00; // loop lo
        aram[0x1003] = 0x11; // loop hi -> 0x1100 (loops to itself)
        aram[0x1100] = 0x03; // header: end + loop
        aram
    }

    #[test]
    fn fresh_voice_is_inactive_and_outputs_silence() {
        let mut v = Voice::new();
        let aram = Box::new([0u8; 0x10000]);
        let counter = EnvelopeCounter::new();
        let (l, r) = v.step(0x1000, 0x40, 0x40, 0x00, 0x00, 0x00, &counter, &aram);
        assert_eq!((l, r), (0, 0));
    }

    #[test]
    fn start_loads_brr_addr_from_directory() {
        let aram = aram_with_silent_block();
        let mut v = Voice::new();
        v.start(0, 0x1000, &aram);
        assert_eq!(v.brr_addr, 0x1100);
        assert_eq!(v.loop_addr, 0x1100);
        assert!(v.active);
        assert_eq!(v.envelope.mode, EnvelopeMode::Attack);
    }

    #[test]
    fn silent_block_yields_silent_output() {
        // Directly drive the sampler past warm-up and confirm output
        // stays at 0. (Envelope is in Attack -> level grows, but the
        // raw is 0 so shaped output is 0.)
        let aram = aram_with_silent_block();
        let mut v = Voice::new();
        v.start(0, 0x1000, &aram);
        let counter = EnvelopeCounter::new();
        for _ in 0..100 {
            let (l, r) = v.step(0x1000, 0x40, 0x40, 0x80, 0x00, 0x00, &counter, &aram);
            assert_eq!(l, 0);
            assert_eq!(r, 0);
        }
    }

    #[test]
    fn end_without_loop_drops_voice_to_release() {
        // Build a single block with end=1, loop=0. After consuming the
        // block the voice should mark itself inactive and the
        // envelope should be in Release.
        let mut aram = Box::new([0u8; 0x10000]);
        aram[0x1000] = 0x00; // start lo
        aram[0x1001] = 0x11; // start hi
        aram[0x1100] = 0x01; // header: end only (no loop)
        let mut v = Voice::new();
        v.start(0, 0x1000, &aram);
        let counter = EnvelopeCounter::new();
        // pitch = 0x1000 -> one BRR sample per tick. After 16 ticks
        // we've consumed the whole block.
        for _ in 0..32 {
            let _ = v.step(0x1000, 0x40, 0x40, 0x80, 0x00, 0x00, &counter, &aram);
        }
        assert!(!v.active, "voice should release after end-without-loop");
        assert_eq!(v.envelope.mode, EnvelopeMode::Release);
    }

    #[test]
    fn loop_block_continues_indefinitely() {
        // A block with end+loop returns to brr_addr each iteration.
        // Voice should stay active.
        let aram = aram_with_silent_block();
        let mut v = Voice::new();
        v.start(0, 0x1000, &aram);
        let counter = EnvelopeCounter::new();
        for _ in 0..1000 {
            let _ = v.step(0x1000, 0x40, 0x40, 0x80, 0x00, 0x00, &counter, &aram);
        }
        assert!(v.active, "looping voice stays active");
        assert!(v.endx_pending, "ENDX latches on each end-block");
    }

    #[test]
    fn nonzero_block_produces_audible_output_after_warmup() {
        // Block with range=8, filter=0, all-positive nibbles -> non-
        // trivial samples flow through. Confirm we get a nonzero
        // output once the pitch counter has stepped past 4 samples
        // (the Gaussian window needs filling).
        let mut aram = Box::new([0u8; 0x10000]);
        aram[0x1000] = 0x00;
        aram[0x1001] = 0x11;
        aram[0x1002] = 0x00;
        aram[0x1003] = 0x11;
        aram[0x1100] = 0x83; // range=8, filter=0, end+loop
        for i in 1..9 {
            aram[0x1100 + i] = 0x77; // every nibble = 7 (positive, mid-amplitude)
        }
        let mut v = Voice::new();
        v.start(0, 0x1000, &aram);
        let counter = EnvelopeCounter::new();
        // Run 32 samples at pitch=0x1000 (1:1) so the buffer fills
        // and the envelope reaches some non-zero level.
        let mut got_nonzero = false;
        for _ in 0..256 {
            let (l, _r) = v.step(0x1000, 0x40, 0x40, 0x80, 0x00, 0x00, &counter, &aram);
            if l != 0 {
                got_nonzero = true;
                break;
            }
        }
        assert!(got_nonzero, "non-silent block should produce audible output");
    }

    #[test]
    fn key_off_through_voice_envelope_releases() {
        let aram = aram_with_silent_block();
        let mut v = Voice::new();
        v.start(0, 0x1000, &aram);
        // Manually trigger KOFF -> envelope key_off.
        v.envelope.key_off();
        assert_eq!(v.envelope.mode, EnvelopeMode::Release);
    }
}
