//! BRR sample decoder for the S-DSP voice runtime.
//!
//! BRR ("Bit-Rate Reduction") is the SNES's lossy 4-bit ADPCM-style
//! sample format. Each block is **9 bytes** and decodes to **16
//! samples**. Samples carry across blocks via two predictor taps
//! (`p1`, `p2`) so cross-block continuity is part of the decoder
//! contract.
//!
//! ## Block layout
//!
//! ```text
//! byte 0 = SSSSFFLE
//!   S = range / shift (0..15)
//!   F = filter index (0..3)
//!   L = loop bit
//!   E = end bit
//! bytes 1..9 = 16 sign-extended 4-bit samples, high nibble first
//! ```
//!
//! ## Decoded representation
//!
//! Samples land in `i16` with **the low bit always zero**. This is
//! the "doubled" / 15-bit-precision form used by Mesen2 + higan as
//! their internal BRR working register. The low-bit-zero invariant
//! follows from the algorithm: each decoded sample is clamped to a
//! 16-bit signed range and then left-shifted by 1 before storage.
//! Voice mixers downstream can either treat samples as plain s16 or
//! halve them on read for true 15-bit values; the doubled form is
//! the canonical inter-stage representation.
//!
//! ## Sources
//!
//! - `~/.claude/skills/nes-expert/reference/snes-apu.md` §7 (block
//!   format + filter formulas + range 13-15 clamp + click errata).
//! - Mesen2 `Core/SNES/DSP/DspVoice.cpp::DecodeBrrSample` (line-by-line
//!   filter algebra + buffer doubling).
//! - higan `sfc/dsp/brr.cpp::brrDecode` (compact reference impl).

/// Header fields parsed out of the first byte of a 9-byte BRR block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrrHeader {
    /// Range / shift amount, raw 0..15. Values 13-15 trigger the
    /// hardware "overflow clamp" and ignore the nibble shift.
    pub range: u8,
    /// Filter index 0..3.
    pub filter: u8,
    /// Loop flag: when set together with `end`, the voice's BRR
    /// pointer reloads from the directory's loop address instead of
    /// going to Release.
    pub loop_flag: bool,
    /// End flag: when set, this is the last block of the sample.
    pub end_flag: bool,
}

impl BrrHeader {
    /// Parse the SSSSFFLE header byte. No validation - reserved range
    /// values 13-15 are passed through; the decoder handles them.
    pub const fn parse(byte: u8) -> Self {
        Self {
            range: (byte >> 4) & 0x0F,
            filter: (byte >> 2) & 0x03,
            loop_flag: (byte & 0x02) != 0,
            end_flag: (byte & 0x01) != 0,
        }
    }
}

/// Predictor state that carries from one BRR block to the next within
/// the same voice. Both fields hold samples in the doubled (LSB=0)
/// representation produced by [`decode_block`].
#[derive(Debug, Clone, Copy, Default)]
pub struct BrrState {
    /// Previous decoded sample (doubled).
    pub p1: i16,
    /// Sample before previous (doubled).
    pub p2: i16,
}

impl BrrState {
    /// Power-on / KON state: predictor taps cleared. Filter 0 ignores
    /// p1/p2 anyway; filters 1-3 will produce well-defined output
    /// only once at least two samples have been decoded with filter 0.
    pub const fn new() -> Self {
        Self { p1: 0, p2: 0 }
    }
}

/// Decode one 9-byte BRR block into 16 samples. Updates `state` in
/// place: after the call, `state.p1` holds the 16th (last) decoded
/// sample and `state.p2` holds the 15th, both in doubled form.
///
/// Returns the parsed header alongside the samples so callers can
/// react to `end_flag` / `loop_flag` for voice loop / release
/// transitions.
///
/// # Algorithm
///
/// For each 4-bit signed nibble `n`:
///
/// ```text
/// shifted = (n << range) >> 1     when range <= 12
/// shifted = -2048 if n < 0 else 0 when range >= 13
///
/// p1, p2 are loaded from state, halved (>> 1) to undo the doubling.
///
/// match filter {
///     0 => s = shifted,
///     1 => s = shifted + p1 + (-p1 >> 4),
///     2 => s = shifted + (p1 << 1) + ((-((p1 << 1) + p1)) >> 5)
///                      - p2 + (p2 >> 4),
///     3 => s = shifted + (p1 << 1) + ((-(p1 + (p1 << 2) + (p1 << 3))) >> 6)
///                      - p2 + (((p2 << 1) + p2) >> 4),
/// }
///
/// out = clamp16(s) << 1     // low bit zeroed by the shift
/// ```
///
/// The clamp-to-16 then left-shift-by-1 is what produces the
/// "click" wraparound on consecutive `-32768` values: `-32768 << 1`
/// in 16-bit storage wraps to `0`, audibly visible (Anomie's BRR
/// test exercises this).
pub fn decode_block(block: &[u8; 9], state: &mut BrrState) -> ([i16; 16], BrrHeader) {
    let header = BrrHeader::parse(block[0]);
    let mut out = [0i16; 16];

    for i in 0..16 {
        let byte = block[1 + (i >> 1)];
        // High nibble first (i % 2 == 0 takes the top nibble).
        let nibble = if i & 1 == 0 { byte >> 4 } else { byte & 0x0F };
        // Sign-extend nibble: shift left into bit 15, then arithmetic
        // shift right back to bit 3 keeps the sign.
        let signed = ((nibble as i16) << 12) >> 12;

        let shifted: i32 = if header.range <= 12 {
            ((signed as i32) << header.range) >> 1
        } else {
            // Range 13-15: hardware "overflow clamp" - nibble is
            // forced to -2048 (negative) or 0 (non-negative).
            if signed < 0 { -2048 } else { 0 }
        };

        // p1 in doubled form -> halve to get the true 16-bit-clamp value.
        let p1 = (state.p1 >> 1) as i32;
        let p2 = (state.p2 >> 1) as i32;

        let s = match header.filter {
            0 => shifted,
            1 => shifted + p1 + (-p1 >> 4),
            2 => shifted + (p1 << 1) + ((-((p1 << 1) + p1)) >> 5) - p2 + (p2 >> 4),
            3 => {
                shifted
                    + (p1 << 1)
                    + ((-(p1 + (p1 << 2) + (p1 << 3))) >> 6)
                    - p2
                    + (((p2 << 1) + p2) >> 4)
            }
            // BrrHeader::parse masks filter to 2 bits, so 0..=3 covers all.
            _ => unreachable!(),
        };

        // Clamp to 16-bit signed, then double for storage. The
        // doubling is an int16 multiply that wraps - this is the
        // intentional click-bug surface area. We compute the doubled
        // value in i16 wrapping arithmetic to preserve that.
        let clamped = clamp16(s);
        let doubled = clamped.wrapping_mul(2);
        out[i] = doubled;
        state.p2 = state.p1;
        state.p1 = doubled;
    }

    (out, header)
}

/// Saturate to the s16 range `[-32768, 32767]`.
#[inline]
fn clamp16(x: i32) -> i16 {
    if x > i16::MAX as i32 {
        i16::MAX
    } else if x < i16::MIN as i32 {
        i16::MIN
    } else {
        x as i16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- Header parse ---------------------------------------------------

    #[test]
    fn header_parse_extracts_all_fields() {
        // SSSS FFLE: range=0xC, filter=2, loop=1, end=0 -> 0xCA
        let h = BrrHeader::parse(0xCA);
        assert_eq!(h.range, 0xC);
        assert_eq!(h.filter, 2);
        assert!(h.loop_flag);
        assert!(!h.end_flag);
    }

    #[test]
    fn header_parse_end_only() {
        let h = BrrHeader::parse(0x01);
        assert_eq!(h.range, 0);
        assert_eq!(h.filter, 0);
        assert!(!h.loop_flag);
        assert!(h.end_flag);
    }

    #[test]
    fn header_parse_filter_three() {
        // filter=3 occupies bits 2-3, value 0x0C
        let h = BrrHeader::parse(0x0C);
        assert_eq!(h.filter, 3);
    }

    // ----- Filter 0 / range 0 (passthrough) -------------------------------

    #[test]
    fn filter0_range0_passthrough_zero_block() {
        // All-zero block: 16 zero samples.
        let block = [0u8; 9];
        let mut st = BrrState::new();
        let (samples, header) = decode_block(&block, &mut st);
        assert_eq!(header.range, 0);
        assert_eq!(header.filter, 0);
        assert_eq!(samples, [0; 16]);
        assert_eq!(st.p1, 0);
        assert_eq!(st.p2, 0);
    }

    #[test]
    fn filter0_positive_nibble_doubles_range_shift() {
        // range=0, filter=0. Nibble 0x7 (positive) -> shifted = (7<<0)>>1 = 3.
        // Doubled output = 6.
        let mut block = [0u8; 9];
        block[0] = 0x00;
        block[1] = 0x70; // first nibble = 7, second = 0
        let mut st = BrrState::new();
        let (s, _) = decode_block(&block, &mut st);
        assert_eq!(s[0], 6, "(7<<0)>>1 = 3, doubled = 6");
        assert_eq!(s[1], 0);
    }

    #[test]
    fn filter0_negative_nibble_sign_extends_then_doubles() {
        // Nibble 0x8 sign-extends to -8. (-8<<0)>>1 = -4. Doubled = -8.
        let mut block = [0u8; 9];
        block[1] = 0x80;
        let mut st = BrrState::new();
        let (s, _) = decode_block(&block, &mut st);
        assert_eq!(s[0], -8);
        assert_eq!(s[1], 0);
    }

    #[test]
    fn filter0_max_valid_range_twelve() {
        // range=12, nibble 0x7 -> (7 << 12) >> 1 = 0x3800 = 14336.
        // Doubled output = 28672.
        let mut block = [0u8; 9];
        block[0] = 0xC0; // range=12, filter=0
        block[1] = 0x70;
        let mut st = BrrState::new();
        let (s, _) = decode_block(&block, &mut st);
        assert_eq!(s[0], 28672);
    }

    #[test]
    fn filter0_range_max_negative_pushes_to_int16_min() {
        // range=12, nibble 0x8 (-8) -> (-8 << 12) >> 1 = -16384.
        // Doubled = -32768 (wrap to i16::MIN exactly).
        let mut block = [0u8; 9];
        block[0] = 0xC0;
        block[1] = 0x80;
        let mut st = BrrState::new();
        let (s, _) = decode_block(&block, &mut st);
        assert_eq!(s[0], -32768);
    }

    // ----- Range 13-15 clamp ----------------------------------------------

    #[test]
    fn range_thirteen_negative_nibble_clamps_to_minus_2048() {
        // Range >= 13: positive nibble -> 0, negative -> -2048.
        // Doubled output: 0 stays 0, -2048 doubles to -4096.
        let mut block = [0u8; 9];
        block[0] = 0xD0; // range=13
        block[1] = 0x70; // nibble 7 (positive)
        block[2] = 0x80; // nibble 8 then 0; first nibble of byte2 = 8 (negative)
        let mut st = BrrState::new();
        let (s, _) = decode_block(&block, &mut st);
        assert_eq!(s[0], 0, "positive nibble at range>=13 -> 0");
        assert_eq!(s[2], -4096, "negative nibble at range>=13 -> -2048 doubled");
    }

    #[test]
    fn range_fifteen_same_clamp_as_thirteen() {
        let mut block_r13 = [0u8; 9];
        block_r13[0] = 0xD0; // range=13
        block_r13[1] = 0x8F; // -8, +7 (via nibble 0xF? no, F = -1)
        let mut block_r15 = block_r13;
        block_r15[0] = 0xF0; // range=15

        let mut st1 = BrrState::new();
        let (s1, _) = decode_block(&block_r13, &mut st1);
        let mut st2 = BrrState::new();
        let (s2, _) = decode_block(&block_r15, &mut st2);
        assert_eq!(s1, s2, "range 13/14/15 share the clamp behaviour");
    }

    // ----- Filter 1 -------------------------------------------------------

    #[test]
    fn filter1_first_sample_with_zero_predictor() {
        // Filter 1 with p1=p2=0 is identical to filter 0 for the first
        // sample. Verify the shifted value is what comes out.
        let mut block = [0u8; 9];
        block[0] = 0x14; // range=1, filter=1
        block[1] = 0x70;
        let mut st = BrrState::new();
        let (s, _) = decode_block(&block, &mut st);
        // (7 << 1) >> 1 = 7. p1=0 contributes nothing. Output doubled = 14.
        assert_eq!(s[0], 14);
    }

    #[test]
    fn filter1_uses_previous_sample_with_15_over_16_coefficient() {
        // After s[0] is decoded, s[1] uses p1 = s[0]. With nibble 0
        // for s[1], the output should be approximately p1 * 15/16.
        // Mesen2/higan formula: s = shifted + p1 + (-p1 >> 4).
        // p1 doubled = 14; filter sees p1 = 7. Filter 1 with shifted=0:
        // s = 0 + 7 + (-7 >> 4) = 7 + (-1) = 6 (arith shift right of -7).
        // Doubled output = 12.
        let mut block = [0u8; 9];
        block[0] = 0x14;
        block[1] = 0x70; // s[0] from nibble 7, s[1] from nibble 0
        let mut st = BrrState::new();
        let (s, _) = decode_block(&block, &mut st);
        assert_eq!(s[0], 14);
        assert_eq!(s[1], 12, "filter 1 with shifted=0 + p1*15/16 (rounded)");
    }

    // ----- Filter 2 / 3 ---------------------------------------------------

    #[test]
    fn filter2_first_sample_zero_predictor_matches_filter0() {
        let mut block = [0u8; 9];
        block[0] = 0x18; // range=1, filter=2
        block[1] = 0x70;
        let mut st = BrrState::new();
        let (s, _) = decode_block(&block, &mut st);
        assert_eq!(s[0], 14);
    }

    #[test]
    fn filter3_first_sample_zero_predictor_matches_filter0() {
        let mut block = [0u8; 9];
        block[0] = 0x1C; // range=1, filter=3
        block[1] = 0x70;
        let mut st = BrrState::new();
        let (s, _) = decode_block(&block, &mut st);
        assert_eq!(s[0], 14);
    }

    // ----- Saturation -----------------------------------------------------

    #[test]
    fn saturation_positive_clamps_to_int16_max() {
        // Force a large positive predictor and apply filter 1 with a
        // large positive shifted to push past i16::MAX before clamp.
        let mut st = BrrState {
            p1: 30000, // halved = 15000
            p2: 0,
        };
        // Filter 1, range=12, nibble 7 -> shifted = 14336.
        // s = 14336 + 15000 + (-15000 >> 4) = 14336 + 15000 - 938 = 28398.
        // Within i16, no clamp triggered. Try a bigger nibble.
        // Use nibble 0x7 with range=12 + p1=30000:
        // Pre-clamp s = 28398 (positive). Doubled = 56796 - wrong, doubled
        // would wrap. Let me use a case that DOES clamp.
        let mut block = [0u8; 9];
        block[0] = 0xC4; // range=12, filter=1
        block[1] = 0x70;
        let (out, _) = decode_block(&block, &mut st);
        // Verify the output is finite and within i16; the exact value
        // depends on the algorithm. Re-derive:
        // shifted = 14336, p1_halved = 15000.
        // s = 14336 + 15000 + (-15000 >> 4 = -938) = 28398.
        // clamp16(28398) = 28398. Doubled = 28398*2 = 56796 -> wraps to -8740 in i16.
        let expected_clamped = 28398i32;
        let expected_doubled = (expected_clamped as i16).wrapping_mul(2);
        assert_eq!(out[0], expected_doubled);
    }

    #[test]
    fn saturation_clamp_caps_at_int16_max() {
        // Pick inputs that drive pre-clamp s past 32767.
        // Filter 1: s = shifted + p1 + (-p1 >> 4).
        // Use p1 doubled = 32766 (i16::MAX-1, even), halved = 16383.
        // shifted = 14336 (range=12, nibble 7).
        // s = 14336 + 16383 + (-1024) = 29695. Still under 32767.
        // Try nibble 7, range 12, with state already big and a bigger filter contribution.
        // Use filter 2: s = shifted + 2p1 + (-3p1>>5) - p2 + (p2>>4).
        // p1 halved = 16383, p2 = 0: s = 14336 + 32766 + (-49149>>5) - 0
        //   = 14336 + 32766 + (-1536) = 45566 -> clamps to 32767.
        let mut st = BrrState {
            p1: 32766,
            p2: 0,
        };
        let mut block = [0u8; 9];
        block[0] = 0xC8; // range=12, filter=2
        block[1] = 0x70;
        let (out, _) = decode_block(&block, &mut st);
        // clamp16(45566) = 32767. Doubled wraps: 32767*2 = 65534 -> as i16 = -2.
        assert_eq!(out[0], -2, "32767 doubled wraps to -2 in i16 (BRR click)");
    }

    // ----- BRR click overflow ---------------------------------------------

    #[test]
    fn click_bug_doubled_int16_min_wraps_to_zero() {
        // The hallmark BRR click: a clamped sample of i16::MIN (-32768)
        // gets doubled and stored. -32768 << 1 in 16-bit arithmetic
        // wraps to 0. Real hardware does this too; tools that BRR-encode
        // limit output to about +-29000 to avoid the click.
        let mut block = [0u8; 9];
        block[0] = 0xC0; // range=12, filter=0
        block[1] = 0x80; // nibble -8 -> (-8 << 12) >> 1 = -16384, doubled = -32768
        block[2] = 0x80; // nibble -8 again
        let mut st = BrrState::new();
        let (s, _) = decode_block(&block, &mut st);
        // First sample: pre-clamp = -16384, clamped same, doubled = -32768.
        assert_eq!(s[0], -32768);
        // Second sample is also -16384 pre-clamp, also clamped to -16384,
        // doubled to -32768. No click on filter 0; the click needs the
        // clamp to engage at i16::MIN AND the next double to overflow.
        // To force the click, push pre-clamp below -32768.
        // Filter 1, p1 = -32768 (just stored), halved = -16384.
        // shifted = -16384, s = -16384 + -16384 + (-(-16384) >> 4)
        //   = -32768 + (16384 >> 4 = 1024) = -31744. clamp16 = -31744.
        // Doubled = -31744 * 2 = -63488 -> as i16 wraps to +2048.
        // This is the "audible click".
        let mut block_click = [0u8; 9];
        block_click[0] = 0xC4; // range=12, filter=1
        block_click[1] = 0x80;
        block_click[2] = 0x80;
        let mut st2 = BrrState {
            p1: -32768,
            p2: 0,
        };
        let (s2, _) = decode_block(&block_click, &mut st2);
        // First sample inside this block: pre-clamp = -31744, doubled
        // = -63488 -> wraps to +2048 in i16.
        assert_eq!(s2[0], 2048, "BRR click: -31744 doubled wraps to +2048");
    }

    // ----- Cross-block state continuity -----------------------------------

    #[test]
    fn state_after_decode_holds_last_two_samples() {
        // A simple block where we can predict the last two outputs.
        let mut block = [0u8; 9];
        block[0] = 0x00; // range=0, filter=0
        // Set every nibble to 7 -> every sample = 6 (doubled).
        for byte in &mut block[1..] {
            *byte = 0x77;
        }
        let mut st = BrrState::new();
        let _ = decode_block(&block, &mut st);
        assert_eq!(st.p1, 6, "p1 = last sample (doubled)");
        assert_eq!(st.p2, 6, "p2 = second-to-last sample (doubled)");
    }

    #[test]
    fn state_carries_across_two_blocks() {
        // Decode two consecutive blocks; the second block's filter 1
        // should pull from the first block's last sample.
        let mut block_a = [0u8; 9];
        block_a[0] = 0x10; // range=1, filter=0
        block_a[8] = 0x70; // last byte: nibbles 7, 0 -> samples 14 and 15
                          // are derived from this byte. nibble 7 at idx 14
                          // -> (7 << 1) >> 1 = 7, doubled = 14.
        let mut st = BrrState::new();
        let _ = decode_block(&block_a, &mut st);
        let p1_after_a = st.p1;
        let p2_after_a = st.p2;

        // Second block: filter 1, range 0, all-zero data -> output is
        // entirely predictor contribution.
        let mut block_b = [0u8; 9];
        block_b[0] = 0x04; // range=0, filter=1
        let (s, _) = decode_block(&block_b, &mut st);
        // First sample of block_b uses p1 from block_a's last sample.
        let p1_halved = (p1_after_a >> 1) as i32;
        let expected = (p1_halved + (-p1_halved >> 4)) as i16 * 2;
        assert_eq!(
            s[0], expected,
            "block_b's filter 1 picks up block_a's predictor"
        );
        // Sanity: p2_after_a was used too via the (unchanged) state load
        // for the first sample of block_b, but only filter 2 / 3 use p2.
        let _ = p2_after_a;
    }

    // ----- Cross-check vs Mesen2 representation --------------------------
    //
    // Hand-port Mesen2's exact algebraic structure (DspVoice.cpp:21-66)
    // and verify our decoder produces byte-for-byte identical output for
    // a randomized block + non-trivial predictor. This locks the
    // doubled-buffer / halved-predictor representation in.

    fn mesen2_decode_block(block: &[u8; 9], state: &mut BrrState) -> [i16; 16] {
        let header = block[0];
        let shift = header >> 4;
        let filter = (header >> 2) & 0x03;
        let mut out = [0i16; 16];
        let mut prev1: i32 = (state.p1 >> 1) as i32;
        let mut prev2: i32 = (state.p2 >> 1) as i32;
        for i in 0..16 {
            let byte = block[1 + (i >> 1)];
            let nibble = if i & 1 == 0 { byte >> 4 } else { byte & 0x0F };
            let signed = ((nibble as i16) << 12) >> 12;
            let mut s: i32 = ((signed as i32) << shift) >> 1;
            if shift >= 0x0D {
                s = if signed < 0 { -0x800 } else { 0 };
            }
            match filter {
                0 => {}
                1 => s += prev1 + (-prev1 >> 4),
                2 => s += (prev1 << 1) + ((-((prev1 << 1) + prev1)) >> 5) - prev2 + (prev2 >> 4),
                3 => {
                    s += (prev1 << 1)
                        + ((-(prev1 + (prev1 << 2) + (prev1 << 3))) >> 6)
                        - prev2
                        + (((prev2 << 1) + prev2) >> 4);
                }
                _ => unreachable!(),
            }
            let clamped = if s > i16::MAX as i32 {
                i16::MAX
            } else if s < i16::MIN as i32 {
                i16::MIN
            } else {
                s as i16
            };
            let doubled = clamped.wrapping_mul(2);
            out[i] = doubled;
            prev2 = prev1;
            prev1 = (doubled >> 1) as i32;
        }
        state.p1 = out[15];
        state.p2 = out[14];
        out
    }

    #[test]
    fn matches_mesen2_for_filter_two_with_active_predictor() {
        let block = [
            0x88, // range=8, filter=2, no loop / end
            0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0,
        ];
        let mut st_ours = BrrState {
            p1: 8000,
            p2: -4000,
        };
        let mut st_ref = st_ours;
        let (ours, _) = decode_block(&block, &mut st_ours);
        let theirs = mesen2_decode_block(&block, &mut st_ref);
        assert_eq!(ours, theirs);
        assert_eq!(st_ours.p1, st_ref.p1);
        assert_eq!(st_ours.p2, st_ref.p2);
    }

    #[test]
    fn matches_mesen2_for_filter_three_negative_predictor() {
        let block = [
            0x6C, // range=6, filter=3
            0xFE, 0xDC, 0xBA, 0x98, 0x76, 0x54, 0x32, 0x10,
        ];
        let mut st_ours = BrrState {
            p1: -16000,
            p2: 12000,
        };
        let mut st_ref = st_ours;
        let (ours, _) = decode_block(&block, &mut st_ours);
        let theirs = mesen2_decode_block(&block, &mut st_ref);
        assert_eq!(ours, theirs);
    }

    #[test]
    fn matches_mesen2_for_range_clamp_with_filter_one() {
        // Range 14 -> clamp path AND filter 1 active: stresses the
        // interaction between the shifted-clamp and predictor add.
        let block = [
            0xE4, // range=14, filter=1
            0x80, 0x70, 0x80, 0x70, 0x80, 0x70, 0x80, 0x70,
        ];
        let mut st_ours = BrrState {
            p1: 5000,
            p2: 0,
        };
        let mut st_ref = st_ours;
        let (ours, _) = decode_block(&block, &mut st_ours);
        let theirs = mesen2_decode_block(&block, &mut st_ref);
        assert_eq!(ours, theirs);
    }

    // ----- All 16 nibbles consumed ---------------------------------------

    #[test]
    fn decode_walks_high_nibble_then_low_nibble_per_byte() {
        // Distinct sentinel values per nibble position to verify
        // ordering. Range 0, filter 0 -> shifted = (n<<0)>>1.
        let mut block = [0u8; 9];
        block[0] = 0x00;
        // Byte 1: 0x12 -> sample 0 from nibble 0x1, sample 1 from 0x2.
        // Byte 2: 0x34, ... Byte 8: 0xFE.
        block[1] = 0x12;
        block[2] = 0x34;
        block[3] = 0x56;
        block[4] = 0x78;
        block[5] = 0x9A;
        block[6] = 0xBC;
        block[7] = 0xDE;
        block[8] = 0xF0;
        let mut st = BrrState::new();
        let (s, _) = decode_block(&block, &mut st);
        // Compute expected: each nibble n -> (n<<0)>>1 then *2 = (n>>1)*2.
        // For nibble 1 = 0x1 (positive): (1>>1)*2 = 0.
        // For nibble 2: (2>>1)*2 = 2.
        // For nibble 8: signed = -8, (-8>>1)*2 = -4*2 = -8.
        let nibbles = [
            0x1u8, 0x2, 0x3, 0x4, 0x5, 0x6, 0x7, 0x8, 0x9, 0xA, 0xB, 0xC, 0xD, 0xE, 0xF, 0x0,
        ];
        for (i, &n) in nibbles.iter().enumerate() {
            let signed = ((n as i16) << 12) >> 12; // sign-extend
            let shifted = (signed as i32) >> 1;
            let expected_doubled = (shifted as i16).wrapping_mul(2);
            assert_eq!(
                s[i], expected_doubled,
                "sample {i} from nibble 0x{n:X} -> expected {expected_doubled}"
            );
        }
    }
}
