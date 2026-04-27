//! S-DSP echo unit: 8-tap FIR + circular delay buffer in ARAM.
//!
//! Each sample the echo stage:
//!
//! 1. Reads the (delayed) stereo sample at `ESA<<8 + offset` from
//!    ARAM into an 8-entry-per-channel history ring (samples are
//!    halved on read to match the doubled-BRR convention).
//! 2. Runs an 8-tap FIR over the history using the per-DSP
//!    coefficients `FIR0..FIR7` (signed 8-bit each). The sum is the
//!    "echo input" - the post-FIR delayed signal.
//! 3. Computes the echo output the master mixer adds in:
//!    `echo_out = echo_input * EVOL / 128`.
//! 4. Computes the feedback writeback value:
//!    `fb = (sum of EON voice outputs) + (echo_input * EFB / 128)`,
//!    clamped to s16 and LSB-zeroed.
//! 5. Writes `fb` back into ARAM at the SAME offset (unless FLG.5
//!    "echo writes disabled" is set), then advances `offset += 4`
//!    and wraps at the cached buffer length.
//!
//! The cached `bank` (ESA) updates 1 sample late and the cached
//! `length` (EDL × 2048) only updates when `offset == 0`, matching
//! the documented hardware quirks (snes-apu.md §"Echo buffer
//! caveats").
//!
//! ## FIR intermediate-wrap quirk
//!
//! The FIR sum is NOT a straight i32 accumulator. Hardware does the
//! sum in 16-bit wrapping arithmetic for the first 7 taps, then
//! clamps when adding the 8th. higan reproduces this in
//! `echo25()`:
//!
//! ```text
//! l = sum(FIR0..FIR6) + FIR7_partial
//! l = int16(l)                   // wraps to i16
//! l += int16(FIR7_remainder)
//! echo_input = clamp16(l) & ~1
//! ```
//!
//! We follow the same pattern: accumulate taps 0..7 with intermediate
//! i16 wrap, then clamp + LSB-mask.
//!
//! ## Sources
//!
//! - higan `sfc/dsp/echo.cpp::calculateFIR` + `echo22..echo30` for
//!   the per-cycle pipeline (we collapse to one entry per output
//!   sample).
//! - Mesen2 `Core/SNES/DSP/Dsp.cpp` for the bank / length latching
//!   semantics.
//! - `~/.claude/skills/nes-expert/reference/snes-apu.md` §"Master
//!   pipeline" + §"Echo buffer caveats".

#[derive(Debug, Clone)]
pub struct EchoUnit {
    /// Per-channel 8-entry history ring (halved-on-read samples).
    pub history_l: [i16; 8],
    pub history_r: [i16; 8],
    /// Index where the most-recently-read echo-buffer sample lands.
    /// Wraps mod 8.
    pub history_offset: u8,
    /// Byte offset within the echo buffer (multiples of 4).
    pub offset: u32,
    /// Cached buffer length in bytes (`EDL * 2048`). Updates only
    /// when [`offset`] returns to 0, matching hardware.
    pub length: u32,
    /// Cached high byte of the echo buffer base address. Updates one
    /// sample after the host writes ESA.
    pub bank: u8,
}

impl EchoUnit {
    pub const fn new() -> Self {
        Self {
            history_l: [0; 8],
            history_r: [0; 8],
            history_offset: 0,
            offset: 0,
            length: 0,
            bank: 0,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// One 32 kHz sample of the echo pipeline.
    ///
    /// Inputs:
    /// - `eon_sum_l` / `eon_sum_r`: sum of voice outputs for voices
    ///   that have their EON bit set this sample (s16 - the master
    ///   mixer is responsible for collecting them).
    /// - `esa`: ECHO_START_ADDR register (high byte of buffer base).
    /// - `edl`: ECHO_DELAY register (0..15; length = edl * 2048).
    /// - `evol_l` / `evol_r`: signed 8-bit echo volume per channel.
    /// - `efb`: signed 8-bit echo feedback coefficient.
    /// - `fir`: 8 signed-8 FIR coefficients (`FIR0..FIR7`).
    /// - `readonly`: FLG.5 - if true, the writeback step is skipped
    ///   so the echo buffer cannot be overwritten.
    /// - `aram`: 64 KiB audio RAM, mutated for the writeback.
    ///
    /// Returns the (left, right) echo contribution that the master
    /// mixer adds to the dry voice mix.
    #[allow(clippy::too_many_arguments)]
    pub fn step_sample(
        &mut self,
        eon_sum_l: i32,
        eon_sum_r: i32,
        esa: u8,
        edl: u8,
        evol_l: i8,
        evol_r: i8,
        efb: i8,
        fir: [i8; 8],
        readonly: bool,
        aram: &mut [u8; 0x10000],
    ) -> (i16, i16) {
        // Resolve current address from the cached bank.
        let base = (self.bank as u32) << 8;
        let addr = (base.wrapping_add(self.offset)) as u16;

        // Read the delayed sample (4 bytes: L lo, L hi, R lo, R hi).
        // Samples are halved on read to match BRR's doubled
        // representation.
        let l_lo = aram[addr as usize] as u16;
        let l_hi = aram[addr.wrapping_add(1) as usize] as u16;
        let l_word = ((l_hi << 8) | l_lo) as i16;
        let r_lo = aram[addr.wrapping_add(2) as usize] as u16;
        let r_hi = aram[addr.wrapping_add(3) as usize] as u16;
        let r_word = ((r_hi << 8) | r_lo) as i16;

        // Advance history ring and store new samples (halved).
        self.history_offset = (self.history_offset + 1) & 7;
        self.history_l[self.history_offset as usize] = l_word >> 1;
        self.history_r[self.history_offset as usize] = r_word >> 1;

        // 8-tap FIR with intermediate i16 wrap on taps 0..7, then
        // clamp + LSB-mask. Tap i picks history at offset
        // `(history_offset + i + 1) & 7`.
        let echo_input_l = fir_apply(&self.history_l, self.history_offset, &fir);
        let echo_input_r = fir_apply(&self.history_r, self.history_offset, &fir);

        // Echo output for the master mixer to add to the dry mix.
        let out_l = ((echo_input_l as i32 * evol_l as i32) >> 7) as i16;
        let out_r = ((echo_input_r as i32 * evol_r as i32) >> 7) as i16;

        // Feedback: voice contribution + echo_input * EFB / 128.
        let fb_l = clamp16_i32(eon_sum_l + ((echo_input_l as i32 * efb as i32) >> 7)) & !1;
        let fb_r = clamp16_i32(eon_sum_r + ((echo_input_r as i32 * efb as i32) >> 7)) & !1;

        // Write feedback back to the same address - it will be read
        // back exactly `length / 4` samples from now (the delay).
        if !readonly {
            aram[addr as usize] = fb_l as u8;
            aram[addr.wrapping_add(1) as usize] = (fb_l >> 8) as u8;
            aram[addr.wrapping_add(2) as usize] = fb_r as u8;
            aram[addr.wrapping_add(3) as usize] = (fb_r >> 8) as u8;
        }

        // Cache ESA for next sample (1-sample-delayed effect).
        self.bank = esa;

        // Advance offset; latch length when wrapping to 0.
        if self.offset == 0 {
            self.length = (edl as u32) << 11;
        }
        self.offset = self.offset.wrapping_add(4);
        if self.length == 0 || self.offset >= self.length {
            // EDL=0 keeps offset stuck at 0 (hardware quirk -
            // every sample stomps the same 4 bytes at ESA).
            self.offset = 0;
        }

        (out_l, out_r)
    }
}

impl Default for EchoUnit {
    fn default() -> Self {
        Self::new()
    }
}

/// Apply the 8-tap FIR with hardware-faithful intermediate wrapping.
///
/// Tap `i` reads `history[(history_offset + i + 1) & 7] * fir[i] >> 6`.
/// Taps 0..7 sum with i16 wrap (the SNES's 16-bit accumulator), then
/// the result is clamped to s16 and the LSB is masked off.
fn fir_apply(history: &[i16; 8], history_offset: u8, fir: &[i8; 8]) -> i16 {
    // Taps 0..6 in i16 wrapping arithmetic, then cast result to i16.
    // The 7th tap is added in i32 with final clamp.
    let tap = |i: usize| -> i32 {
        let idx = (history_offset.wrapping_add(i as u8 + 1) & 7) as usize;
        (history[idx] as i32 * fir[i] as i32) >> 6
    };
    let partial: i16 = (tap(0)
        .wrapping_add(tap(1))
        .wrapping_add(tap(2))
        .wrapping_add(tap(3))
        .wrapping_add(tap(4))
        .wrapping_add(tap(5))
        .wrapping_add(tap(6))) as i16;
    let total = (partial as i32).wrapping_add(tap(7));
    (clamp16_i32(total) & !1) as i16
}

#[inline]
fn clamp16_i32(x: i32) -> i32 {
    x.clamp(i16::MIN as i32, i16::MAX as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_echo_outputs_silence_with_zero_buffer() {
        let mut echo = EchoUnit::new();
        let mut aram = Box::new([0u8; 0x10000]);
        let fir = [0i8; 8];
        let (l, r) = echo.step_sample(0, 0, 0x00, 0, 0, 0, 0, fir, false, &mut aram);
        assert_eq!((l, r), (0, 0));
    }

    #[test]
    fn edl_zero_keeps_offset_at_zero_and_writes_same_slot() {
        // Hardware quirk: EDL=0 still writes 4 bytes per sample at
        // ESA<<8, stomping the same slot every sample. Our buffer
        // offset must stay at 0.
        let mut echo = EchoUnit::new();
        let mut aram = Box::new([0u8; 0x10000]);
        let fir = [0i8; 8];
        for _ in 0..10 {
            let _ = echo.step_sample(
                0x100, 0x200, 0x40, 0, 0x40, 0x40, 0, fir, false, &mut aram,
            );
        }
        assert_eq!(echo.offset, 0, "offset stays at 0 with EDL=0");
    }

    #[test]
    fn writes_disabled_when_readonly_set() {
        // FLG.5 (readonly) must prevent ARAM mutation.
        let mut echo = EchoUnit::new();
        let mut aram = Box::new([0u8; 0x10000]);
        let fir = [0i8; 8];
        let _ = echo.step_sample(
            0x100, 0x200, 0x40, 1, 0x40, 0x40, 0, fir, true, &mut aram,
        );
        // Address $4000 (esa=0x40, offset=0). Should still be 0.
        assert_eq!(aram[0x4000], 0);
        assert_eq!(aram[0x4001], 0);
        assert_eq!(aram[0x4002], 0);
        assert_eq!(aram[0x4003], 0);
    }

    #[test]
    fn writeback_lands_at_esa_offset_when_not_readonly() {
        let mut echo = EchoUnit::new();
        // Pre-set bank to match esa so the write goes to $4000.
        echo.bank = 0x40;
        let mut aram = Box::new([0u8; 0x10000]);
        let fir = [0i8; 8];
        // EON sum drives the writeback; with EFB=0 and zero history,
        // feedback = eon_sum & !1.
        let _ = echo.step_sample(
            0x1234, -0x5678, 0x40, 4, 0, 0, 0, fir, false, &mut aram,
        );
        let l_word = (aram[0x4000] as u16) | ((aram[0x4001] as u16) << 8);
        let r_word = (aram[0x4002] as u16) | ((aram[0x4003] as u16) << 8);
        assert_eq!(l_word as i16, 0x1234 & !1);
        assert_eq!(r_word as i16, (-0x5678i32 & !1) as i16);
    }

    #[test]
    fn buffer_advances_in_4_byte_strides_and_wraps_at_length() {
        let mut echo = EchoUnit::new();
        let mut aram = Box::new([0u8; 0x10000]);
        let fir = [0i8; 8];
        // EDL=1 -> length = 2048 bytes = 512 samples. Each step
        // increments offset by 4. After 512 samples, offset wraps.
        for i in 0..512 {
            assert_eq!(echo.offset, (i * 4) as u32);
            let _ = echo.step_sample(0, 0, 0x40, 1, 0, 0, 0, fir, false, &mut aram);
        }
        assert_eq!(echo.offset, 0);
    }

    #[test]
    fn esa_change_takes_effect_one_sample_late() {
        // Write ESA=0x40 first sample, ESA=0x50 second sample. The
        // second sample's read+write should still go to $40xx; the
        // third sample's read+write goes to $50xx. Use modest
        // values that do not saturate the s16 clamp on writeback.
        let mut echo = EchoUnit::new();
        echo.bank = 0x40; // pre-cache so first step uses 0x40
        let mut aram = Box::new([0u8; 0x10000]);
        let fir = [0i8; 8];
        // Sample 1: cached bank = 0x40 -> writes at 0x4000.
        let _ = echo.step_sample(0x1000, 0, 0x40, 1, 0, 0, 0, fir, false, &mut aram);
        assert_eq!(aram[0x4001], 0x10, "hi of 0x1000 = 0x10");
        // Sample 2: pass esa=0x50 - cached bank is still 0x40 so
        // write goes to 0x4004. Bank latches to 0x50 after.
        let _ = echo.step_sample(0x2000, 0, 0x50, 1, 0, 0, 0, fir, false, &mut aram);
        assert_eq!(aram[0x4005], 0x20, "still old bank 0x40 - hi = 0x20");
        // Sample 3: cached bank is now 0x50 -> writes at 0x5008.
        let _ = echo.step_sample(0x3000, 0, 0x50, 1, 0, 0, 0, fir, false, &mut aram);
        assert_eq!(aram[0x5009], 0x30, "new bank in effect - hi = 0x30");
    }

    #[test]
    fn fir_zero_coefficients_yield_zero_echo_input() {
        // History fills with whatever the buffer has; with all FIR
        // coefficients 0, echo_input = 0, so echo_out = 0.
        let mut echo = EchoUnit::new();
        let mut aram = Box::new([0u8; 0x10000]);
        // Pre-load some non-zero data in the echo buffer.
        for byte in &mut aram[0x4000..0x5000] {
            *byte = 0x55;
        }
        let fir = [0i8; 8];
        let (l, r) = echo.step_sample(0, 0, 0x40, 1, 0x40, 0x40, 0, fir, true, &mut aram);
        assert_eq!((l, r), (0, 0));
    }

    #[test]
    fn fir_passthrough_at_tap_seven_returns_recent_sample() {
        // FIR[7] = 64 (= 1.0 in /64 fixed-point), all others 0.
        // After 8 reads, history fills; tap 7 picks the *current*
        // sample (history[(off + 8) & 7] = history[off]). Output
        // before EVOL scaling is the read-back sample (halved),
        // clamped + LSB-masked.
        let mut echo = EchoUnit::new();
        echo.bank = 0x40; // pre-cache - first step reads from $4000, not $0000
        let mut aram = Box::new([0u8; 0x10000]);
        // Echo buffer starts at 0x4000. Put a known sample at
        // offset 0.
        aram[0x4000] = 0x00;
        aram[0x4001] = 0x40; // L = 0x4000 = 16384
        aram[0x4002] = 0x00;
        aram[0x4003] = 0xC0; // R = 0xC000 = -16384
        let mut fir = [0i8; 8];
        fir[7] = 64;
        // EVOL = 0x40 -> output = echo_input * 64 / 128 = echo_input/2.
        let (l, _r) = echo.step_sample(
            0, 0, 0x40, 1, 0x40, 0x40, 0, fir, true, &mut aram,
        );
        // Read sample = 16384, halved to 8192. FIR pass = 8192 *
        // 64 / 64 = 8192. Clamp16 + !1 = 8192. EVOL: 8192*64/128 = 4096.
        assert_eq!(l, 4096);
    }

    #[test]
    fn feedback_writes_eon_sum_when_efb_and_history_zero() {
        let mut echo = EchoUnit::new();
        echo.bank = 0x40;
        let mut aram = Box::new([0u8; 0x10000]);
        let fir = [0i8; 8];
        // Drive EON sum, EFB=0, FIR=0 -> feedback = eon_sum & !1.
        let _ = echo.step_sample(
            1234, -567, 0x40, 1, 0, 0, 0, fir, false, &mut aram,
        );
        let l_word = (aram[0x4000] as u16) | ((aram[0x4001] as u16) << 8);
        let r_word = (aram[0x4002] as u16) | ((aram[0x4003] as u16) << 8);
        assert_eq!(l_word as i16, 1234 & !1);
        assert_eq!(r_word as i16, -567i16 & !1);
    }
}
