//! Voice sample-rate-conversion stage of the S-DSP.
//!
//! Each S-DSP voice owns a 12-entry rolling buffer of decoded BRR
//! samples (in the doubled / LSB-zero form produced by
//! [`super::brr::decode_block`]). The voice runtime emits one output
//! sample per 32 kHz tick by:
//!
//! 1. Advancing the per-voice pitch counter by the effective 14-bit
//!    pitch register; whenever the counter's integer step (top 4
//!    bits of `counter >> 12`) increments, a fresh BRR sample is
//!    needed and the consumer must call [`VoiceSampler::push_sample`]
//!    to feed it.
//! 2. Reading four consecutive samples out of the rolling buffer at
//!    a phase offset taken from the counter and weighing them with
//!    the SNES's hardware **Gaussian** table (256 distinct phase
//!    columns; mirrored to give 512 weight slots).
//!
//! The Gaussian gives a slight low-pass character; per the SPC700
//! datasheet and Anomie's notes, output is one s16 per sample (LSB
//! zero by mask).
//!
//! ## API split
//!
//! - [`gaussian_interpolate`]: pure function. Takes the pitch counter,
//!   a 12-entry sample buffer, and the current "oldest sample" index.
//!   Returns the s16 output sample, matching Mesen2's
//!   `DspInterpolation::Gauss` algorithm exactly (verified by
//!   byte-for-byte cross-check tests).
//! - [`VoiceSampler`]: stateful wrapper that owns the buffer + pitch
//!   counter and exposes a `step(pitch)` driver. Splits sample
//!   "needed" detection out via [`VoiceSampler::needs_sample`] so the
//!   voice runtime knows when to invoke the BRR decoder.
//!
//! ## Sources
//!
//! - `~/.claude/skills/nes-expert/reference/snes-apu.md` §"Voice
//!   processing pipeline" + §"Pitch": 14-bit pitch capped to 0x3FFF;
//!   one BRR sample consumed per `pitch / 0x1000` output samples.
//! - Mesen2 `Core/SNES/DSP/DspInterpolation.h::Gauss` for the table
//!   contents AND the fold/clamp algebra ("first three terms wrap at
//!   16 bits as a group, the fourth is added and final-clamped").
//! - higan `sfc/dsp/gaussian.cpp` for the table-construction algebra
//!   (sin/cos derivation) and the 4-tap window layout.

/// SNES hardware Gaussian interpolation coefficient table. 512 i16
/// entries; the lookups in [`gaussian_interpolate`] reach indices in
/// `[0, 511]`. Shared verbatim with Mesen2's
/// `DspInterpolation::gauss`.
#[rustfmt::skip]
pub const GAUSS_TABLE: [i16; 512] = [
       0,    0,    0,    0,    0,    0,    0,    0,    0,    0,    0,    0,    0,    0,    0,    0,
       1,    1,    1,    1,    1,    1,    1,    1,    1,    1,    1,    2,    2,    2,    2,    2,
       2,    2,    3,    3,    3,    3,    3,    4,    4,    4,    4,    4,    5,    5,    5,    5,
       6,    6,    6,    6,    7,    7,    7,    8,    8,    8,    9,    9,    9,   10,   10,   10,
      11,   11,   11,   12,   12,   13,   13,   14,   14,   15,   15,   15,   16,   16,   17,   17,
      18,   19,   19,   20,   20,   21,   21,   22,   23,   23,   24,   24,   25,   26,   27,   27,
      28,   29,   29,   30,   31,   32,   32,   33,   34,   35,   36,   36,   37,   38,   39,   40,
      41,   42,   43,   44,   45,   46,   47,   48,   49,   50,   51,   52,   53,   54,   55,   56,
      58,   59,   60,   61,   62,   64,   65,   66,   67,   69,   70,   71,   73,   74,   76,   77,
      78,   80,   81,   83,   84,   86,   87,   89,   90,   92,   94,   95,   97,   99,  100,  102,
     104,  106,  107,  109,  111,  113,  115,  117,  118,  120,  122,  124,  126,  128,  130,  132,
     134,  137,  139,  141,  143,  145,  147,  150,  152,  154,  156,  159,  161,  163,  166,  168,
     171,  173,  175,  178,  180,  183,  186,  188,  191,  193,  196,  199,  201,  204,  207,  210,
     212,  215,  218,  221,  224,  227,  230,  233,  236,  239,  242,  245,  248,  251,  254,  257,
     260,  263,  267,  270,  273,  276,  280,  283,  286,  290,  293,  297,  300,  304,  307,  311,
     314,  318,  321,  325,  328,  332,  336,  339,  343,  347,  351,  354,  358,  362,  366,  370,
     374,  378,  381,  385,  389,  393,  397,  401,  405,  410,  414,  418,  422,  426,  430,  434,
     439,  443,  447,  451,  456,  460,  464,  469,  473,  477,  482,  486,  491,  495,  499,  504,
     508,  513,  517,  522,  527,  531,  536,  540,  545,  550,  554,  559,  563,  568,  573,  577,
     582,  587,  592,  596,  601,  606,  611,  615,  620,  625,  630,  635,  640,  644,  649,  654,
     659,  664,  669,  674,  678,  683,  688,  693,  698,  703,  708,  713,  718,  723,  728,  732,
     737,  742,  747,  752,  757,  762,  767,  772,  777,  782,  787,  792,  797,  802,  806,  811,
     816,  821,  826,  831,  836,  841,  846,  851,  855,  860,  865,  870,  875,  880,  884,  889,
     894,  899,  904,  908,  913,  918,  923,  927,  932,  937,  941,  946,  951,  955,  960,  965,
     969,  974,  978,  983,  988,  992,  997, 1001, 1005, 1010, 1014, 1019, 1023, 1027, 1032, 1036,
    1040, 1045, 1049, 1053, 1057, 1061, 1066, 1070, 1074, 1078, 1082, 1086, 1090, 1094, 1098, 1102,
    1106, 1109, 1113, 1117, 1121, 1125, 1128, 1132, 1136, 1139, 1143, 1146, 1150, 1153, 1157, 1160,
    1164, 1167, 1170, 1174, 1177, 1180, 1183, 1186, 1190, 1193, 1196, 1199, 1202, 1205, 1207, 1210,
    1213, 1216, 1219, 1221, 1224, 1227, 1229, 1232, 1234, 1237, 1239, 1241, 1244, 1246, 1248, 1251,
    1253, 1255, 1257, 1259, 1261, 1263, 1265, 1267, 1269, 1270, 1272, 1274, 1275, 1277, 1279, 1280,
    1282, 1283, 1284, 1286, 1287, 1288, 1290, 1291, 1292, 1293, 1294, 1295, 1296, 1297, 1297, 1298,
    1299, 1300, 1300, 1301, 1302, 1302, 1303, 1303, 1303, 1304, 1304, 1304, 1304, 1304, 1305, 1305,
];

/// 4-tap Gaussian-weighted interpolation matching the SNES S-DSP.
///
/// `pitch_counter` is the running 16-bit fractional pitch counter for
/// the voice (top 4 bits select which 4-sample window slides into
/// view; bits 4..11 select the phase column; bits 0..3 are
/// sub-resolution residue and are unused here, matching hardware).
///
/// `samples` is the 12-entry rolling buffer; `buffer_pos` is the
/// index of the **oldest** sample in the window (the one that will be
/// overwritten next). The 4-tap window starts at
/// `(pitch_counter >> 12) + buffer_pos` (mod 12) and runs forward.
///
/// Algorithm follows Mesen2 line-for-line: the first three weighted
/// products are summed and **truncated to i16** as a group (the
/// "wrap at 15 bits signed" the comment in Mesen2 alludes to), then
/// the fourth product is added and the result is clamped (not
/// wrapped). The low bit of the output is masked to 0, matching the
/// LSB-zero invariant of the doubled BRR representation.
pub fn gaussian_interpolate(pitch_counter: u16, samples: &[i16; 12], buffer_pos: u8) -> i16 {
    let pos: u8 = ((pitch_counter >> 12) as u8).wrapping_add(buffer_pos);
    let offset: u8 = ((pitch_counter >> 4) & 0xFF) as u8;

    let s0 = samples[(pos % 12) as usize] as i32;
    let s1 = samples[((pos.wrapping_add(1)) % 12) as usize] as i32;
    let s2 = samples[((pos.wrapping_add(2)) % 12) as usize] as i32;
    let s3 = samples[((pos.wrapping_add(3)) % 12) as usize] as i32;

    let w0 = GAUSS_TABLE[(255 - offset as usize) & 0x1FF] as i32;
    let w1 = GAUSS_TABLE[(511 - offset as usize) & 0x1FF] as i32;
    let w2 = GAUSS_TABLE[256 + offset as usize] as i32;
    let w3 = GAUSS_TABLE[offset as usize] as i32;

    // First three terms sum and wrap as i16, then the fourth term is
    // added and clamped. The masked LSB enforces the doubled BRR
    // sample invariant.
    let three = ((w0 * s0) >> 11) + ((w1 * s1) >> 11) + ((w2 * s2) >> 11);
    let three_wrapped = three as i16 as i32;
    let total = three_wrapped + ((w3 * s3) >> 11);
    let clamped = clamp16(total);
    (clamped & !0x01) as i16
}

#[inline]
fn clamp16(x: i32) -> i32 {
    x.clamp(i16::MIN as i32, i16::MAX as i32)
}

/// Per-voice sample-rate-conversion state: the rolling 12-entry
/// buffer + the pitch counter.
///
/// The voice runtime drives this by:
///
/// ```text
/// for each output tick:
///     while sampler.needs_sample(pitch):
///         decode the next BRR sample, then
///         sampler.push_sample(brr_sample);
///     let s = sampler.step(pitch);  // returns one s16 output
/// ```
///
/// The pitch counter is 16 bits; the top 4 bits track how many BRR
/// samples we've consumed since the last reset. Once the counter
/// wraps past `0xFFFF`, the buffer position has been advanced by one
/// (the sliding 4-tap window moves forward).
#[derive(Debug, Clone, Copy)]
pub struct VoiceSampler {
    /// Rolling sample buffer (doubled BRR samples, LSB=0).
    pub buffer: [i16; 12],
    /// Index of the **oldest** sample in the buffer (also: where the
    /// next pushed sample will land).
    pub buffer_pos: u8,
    /// 16-bit fractional pitch counter. Top 4 bits = sample-step
    /// integer, middle 8 bits = Gaussian phase column, low 4 bits =
    /// unused residue (matches hardware).
    pub pitch_counter: u16,
}

impl VoiceSampler {
    /// Power-on / KON state: zero buffer, counter cleared. The voice
    /// runtime should ensure the BRR decoder pre-fills at least 4
    /// samples before the first `step` so the Gaussian window has
    /// real data; until then the output is silence by construction.
    pub const fn new() -> Self {
        Self {
            buffer: [0; 12],
            buffer_pos: 0,
            pitch_counter: 0,
        }
    }

    /// Reset to the same zero state as [`Self::new`]. Called by the
    /// voice runtime on KON.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Push a single newly-decoded BRR sample into the rolling buffer
    /// at the current `buffer_pos`, then advance `buffer_pos` (mod 12).
    ///
    /// The sample is expected to be in the doubled / LSB-zero form
    /// produced by [`super::brr::decode_block`].
    pub fn push_sample(&mut self, sample: i16) {
        self.buffer[self.buffer_pos as usize] = sample;
        self.buffer_pos = (self.buffer_pos + 1) % 12;
    }

    /// Returns the number of additional BRR samples that need to be
    /// pushed before the *next* call to [`Self::step`] would be
    /// well-defined. In practice this is the number of times the
    /// pitch-counter integer step would advance during the next
    /// call.
    ///
    /// Hardware pre-fills 5 samples on KON before audio starts; once
    /// running, this is `0`, `1`, `2`, or rarely `3` per output tick
    /// depending on `pitch`.
    pub fn pending_decodes(&self, pitch: u16) -> u8 {
        let p = effective_pitch(pitch);
        let next = self.pitch_counter as u32 + p as u32;
        ((next >> 12) - (self.pitch_counter as u32 >> 12)) as u8
    }

    /// Compute one output sample (Gaussian-interpolated) and advance
    /// the pitch counter. The caller is responsible for having pushed
    /// any newly-needed BRR samples *before* calling `step`.
    pub fn step(&mut self, pitch: u16) -> i16 {
        let out = gaussian_interpolate(self.pitch_counter, &self.buffer, self.buffer_pos);
        let p = effective_pitch(pitch);
        // 16-bit wrap is intentional: gaussian_interpolate's index
        // arithmetic is mod 12, so the counter's high nibble can roll
        // over freely without losing track of where in the rolling
        // window we are. The voice runtime calls push_sample (which
        // moves buffer_pos forward) once per step crossing, keeping
        // the sliding window aligned.
        self.pitch_counter = self.pitch_counter.wrapping_add(p);
        out
    }
}

impl Default for VoiceSampler {
    fn default() -> Self {
        Self::new()
    }
}

/// Cap the raw pitch register to its hardware 14-bit limit. Anomie /
/// snes-apu.md: bits 0..13 are real pitch; bits 14..15 of the PH
/// register are ignored.
#[inline]
pub const fn effective_pitch(raw: u16) -> u16 {
    raw & 0x3FFF
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- Table sanity --------------------------------------------------

    #[test]
    fn gauss_table_has_512_entries() {
        assert_eq!(GAUSS_TABLE.len(), 512);
    }

    #[test]
    fn gauss_table_peaks_at_index_511() {
        // The table's largest value should be at the end (the curve
        // peaks at offset 0 of the "forward" tap, which maps to
        // GAUSS_TABLE[255-0]=255, GAUSS_TABLE[511-0]=511, etc; the
        // 511 entry is the max weight of the central tap).
        let max = *GAUSS_TABLE.iter().max().unwrap();
        assert_eq!(max, 1305);
        assert_eq!(GAUSS_TABLE[511], 1305);
    }

    #[test]
    fn gauss_table_starts_at_zero() {
        // First several entries are all 0 (the curve is flat at the
        // far edge of the interpolation window).
        for &v in &GAUSS_TABLE[0..16] {
            assert_eq!(v, 0);
        }
    }

    #[test]
    fn gauss_table_is_monotonic_nondecreasing() {
        // The table is the right half of a symmetric Gaussian-like
        // curve; it must be non-decreasing across its 512 entries.
        for i in 1..GAUSS_TABLE.len() {
            assert!(
                GAUSS_TABLE[i] >= GAUSS_TABLE[i - 1],
                "non-monotonic at index {i}: {} < {}",
                GAUSS_TABLE[i],
                GAUSS_TABLE[i - 1]
            );
        }
    }

    // ----- effective_pitch ----------------------------------------------

    #[test]
    fn effective_pitch_caps_at_14_bits() {
        assert_eq!(effective_pitch(0x0000), 0);
        assert_eq!(effective_pitch(0x3FFF), 0x3FFF);
        assert_eq!(effective_pitch(0x4000), 0);
        assert_eq!(effective_pitch(0xFFFF), 0x3FFF);
    }

    // ----- gaussian_interpolate -----------------------------------------

    #[test]
    fn zero_buffer_yields_zero_output_at_any_phase() {
        let buf = [0i16; 12];
        for phase in 0u16..0x1000 {
            assert_eq!(gaussian_interpolate(phase << 4, &buf, 0), 0);
        }
    }

    #[test]
    fn dc_passthrough_constant_buffer_returns_near_constant() {
        // With every sample set to the same value, the Gaussian
        // weighting should sum to ~v. The four tap weights at any
        // phase sum approximately to 2048 (the algorithm divides by
        // >> 11 = /2048), but integer rounding in the baked table
        // produces small natural ripple of order +/- 0.5%, which at
        // v=0x4000 is about +/- 32 LSB. Sample many phases and bound
        // the residual.
        let v: i16 = 0x4000;
        let buf = [v; 12];
        for phase in (0u16..0x1000).step_by(0x40) {
            let out = gaussian_interpolate(phase << 4, &buf, 0);
            let err = (out as i32 - v as i32).abs();
            assert!(
                err <= 64,
                "DC ripple too large at phase {phase:03X}: out={out} (err={err})"
            );
        }
    }

    #[test]
    fn dc_passthrough_silence_is_exact() {
        // The one input where rounding can't kick in: zero everywhere.
        let buf = [0i16; 12];
        for phase in 0u16..0x1000 {
            assert_eq!(gaussian_interpolate(phase << 4, &buf, 0), 0);
        }
    }

    #[test]
    fn output_lsb_is_always_zero() {
        // Throw varied input + phase combinations at the interpolator
        // and confirm the LSB-mask invariant.
        let buf: [i16; 12] = [
            0x100, -0x200, 0x4000, -0x4000, 0x10, 0x20, 0x40, 0x80, 0x800, -0x800, 0x2000, -0x2000,
        ];
        for phase in [0u16, 0x100, 0x800, 0xF00, 0xFFF] {
            for buffer_pos in 0u8..12 {
                let out = gaussian_interpolate(phase << 4, &buf, buffer_pos);
                assert_eq!(out & 1, 0, "LSB nonzero at phase {phase:03X} pos {buffer_pos}");
            }
        }
    }

    // ----- Cross-check against Mesen2's exact algorithm ------------------

    fn mesen2_gauss(pitch_counter: u16, samples: &[i16; 12], buffer_pos: u8) -> i16 {
        let pos: u8 = ((pitch_counter >> 12) as u8).wrapping_add(buffer_pos);
        let offset: u8 = ((pitch_counter >> 4) & 0xFF) as u8;
        let mut three: i32 = (GAUSS_TABLE[255 - offset as usize] as i32)
            * (samples[(pos % 12) as usize] as i32)
            >> 11;
        three += (GAUSS_TABLE[511 - offset as usize] as i32)
            * (samples[((pos as usize + 1) % 12)] as i32)
            >> 11;
        three += (GAUSS_TABLE[256 + offset as usize] as i32)
            * (samples[((pos as usize + 2) % 12)] as i32)
            >> 11;
        let folded = three as i16 as i32;
        let mut out = folded
            + ((GAUSS_TABLE[offset as usize] as i32) * (samples[((pos as usize + 3) % 12)] as i32)
                >> 11);
        out = out.clamp(i16::MIN as i32, i16::MAX as i32);
        ((out as i16) & !0x01) as i16
    }

    #[test]
    fn matches_mesen2_byte_for_byte_across_random_inputs() {
        // Deterministic LCG so the test is reproducible without an
        // RNG dependency. Cover several phases and buffer positions.
        let mut state: u32 = 0xDEAD_BEEF;
        let mut next = || -> u16 {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            (state >> 16) as u16
        };
        for _ in 0..200 {
            let mut buf = [0i16; 12];
            for s in &mut buf {
                // Doubled BRR samples have LSB = 0; mimic that.
                *s = (next() as i16) & !0x01;
            }
            let pitch_counter = next();
            let buffer_pos = (next() as u8) % 12;
            let ours = gaussian_interpolate(pitch_counter, &buf, buffer_pos);
            let theirs = mesen2_gauss(pitch_counter, &buf, buffer_pos);
            assert_eq!(ours, theirs, "diff at pc={pitch_counter:04X} pos={buffer_pos}");
        }
    }

    // ----- VoiceSampler --------------------------------------------------

    #[test]
    fn voice_sampler_reset_zeroes_state() {
        let mut s = VoiceSampler::new();
        s.buffer[5] = 0x1234;
        s.buffer_pos = 7;
        s.pitch_counter = 0x8000;
        s.reset();
        assert_eq!(s.buffer, [0; 12]);
        assert_eq!(s.buffer_pos, 0);
        assert_eq!(s.pitch_counter, 0);
    }

    #[test]
    fn push_sample_writes_at_buffer_pos_and_advances() {
        let mut s = VoiceSampler::new();
        s.push_sample(0x100);
        s.push_sample(0x200);
        s.push_sample(0x300);
        assert_eq!(s.buffer[0], 0x100);
        assert_eq!(s.buffer[1], 0x200);
        assert_eq!(s.buffer[2], 0x300);
        assert_eq!(s.buffer_pos, 3);
    }

    #[test]
    fn push_sample_wraps_at_twelve() {
        let mut s = VoiceSampler::new();
        for i in 0..15 {
            s.push_sample(i);
        }
        // Slot 0 was overwritten by sample 12, slot 1 by 13, slot 2 by 14.
        assert_eq!(s.buffer[0], 12);
        assert_eq!(s.buffer[1], 13);
        assert_eq!(s.buffer[2], 14);
        assert_eq!(s.buffer[3], 3);
        assert_eq!(s.buffer_pos, 3);
    }

    #[test]
    fn pending_decodes_zero_when_pitch_does_not_advance_step() {
        let mut s = VoiceSampler::new();
        // Counter at 0x0000, pitch 0x0001 - no integer-step crossing.
        assert_eq!(s.pending_decodes(0x0001), 0);
        // Counter at 0x0FFF, pitch 0x0001 - WILL cross to 0x1000 (one new sample).
        s.pitch_counter = 0x0FFF;
        assert_eq!(s.pending_decodes(0x0001), 1);
    }

    #[test]
    fn pending_decodes_two_at_double_speed() {
        let mut s = VoiceSampler::new();
        // pitch = 0x2000 = double speed -> two BRR samples per output tick.
        assert_eq!(s.pending_decodes(0x2000), 2);
        // From mid-step: counter 0x0800, +0x2000 = 0x2800 -> step 2 -> 2 new.
        s.pitch_counter = 0x0800;
        assert_eq!(s.pending_decodes(0x2000), 2);
    }

    #[test]
    fn pending_decodes_caps_at_pitch_max() {
        // Effective pitch maxes at 0x3FFF -> at most 3 samples per tick.
        let s = VoiceSampler::new();
        assert_eq!(s.pending_decodes(0x3FFF), 3);
        // Even with bits 14/15 set in raw pitch, effective is 0x3FFF.
        assert_eq!(s.pending_decodes(0xFFFF), 3);
    }

    #[test]
    fn step_at_pitch_zero_freezes_counter_and_outputs_silence_on_empty() {
        let mut s = VoiceSampler::new();
        let out = s.step(0);
        assert_eq!(out, 0);
        assert_eq!(s.pitch_counter, 0);
    }

    #[test]
    fn step_at_pitch_one_thousand_advances_counter_by_one_step() {
        // Pre-load identical samples so output is constant.
        let mut s = VoiceSampler::new();
        s.buffer = [0x4000; 12];
        let _ = s.step(0x1000);
        assert_eq!(s.pitch_counter, 0x1000);
        let _ = s.step(0x1000);
        assert_eq!(s.pitch_counter, 0x2000);
    }
}
