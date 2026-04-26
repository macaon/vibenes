// SPDX-License-Identifier: GPL-3.0-or-later
//! Safe Rust wrapper around the vendored emu2413 OPLL FM core
//! ([`vendor/emu2413/`]).
//!
//! emu2413 is the de-facto reference YM2413 / VRC7 implementation -
//! Mesen2, mGBA, and several MSX/SMS emulators all build against it
//! directly. We vendor v1.5.9 (MIT, by Mitsutaka Okazaki) and link it
//! through this single file. The rest of the codebase only sees the
//! safe [`Opll`] type.
//!
//! The wrapper is intentionally minimal: VRC7 only ever needs to
//! construct a chip in VRC7 mode, write a register pair, advance the
//! envelope by one OPLL sample, and read the resulting 16-bit signed
//! value. Everything else (panning, channel masks, multi-chip support)
//! is not exposed.

use std::ptr::NonNull;

/// Native OPLL output sample rate (Hz). Matches Mesen2's
/// `Vrc7Audio::OpllSampleRate`. Equal to `OpllClockRate / 72`, which
/// disables emu2413's internal rate converter (see emu2413.h).
pub const OPLL_SAMPLE_RATE: u32 = 49716;

/// OPLL master clock (Hz). The chip generates one output sample every
/// 72 master ticks, so `OPLL_SAMPLE_RATE * 72` is exactly the right
/// rate for the MAME / VRC7 reference design.
pub const OPLL_CLOCK_RATE: u32 = OPLL_SAMPLE_RATE * 72;

/// Opaque emu2413 chip instance. All the gory state lives behind this
/// pointer in C land; we never inspect it from Rust.
#[repr(C)]
struct OpllRaw {
    _opaque: [u8; 0],
}

extern "C" {
    fn OPLL_new(clk: u32, rate: u32) -> *mut OpllRaw;
    fn OPLL_delete(opll: *mut OpllRaw);
    fn OPLL_reset(opll: *mut OpllRaw);
    fn OPLL_resetPatch(opll: *mut OpllRaw, kind: u8);
    fn OPLL_setChipType(opll: *mut OpllRaw, kind: u8);
    fn OPLL_writeReg(opll: *mut OpllRaw, reg: u32, value: u8);
    fn OPLL_calc(opll: *mut OpllRaw) -> i16;
}

/// VRC7 chip-type constant (`OPLL_VRC7_TONE = 1`). YM2413 itself is 0.
const CHIP_TYPE_VRC7: u8 = 1;

/// Safe owning handle to a single OPLL instance configured for VRC7.
pub struct Opll {
    handle: NonNull<OpllRaw>,
}

// SAFETY: emu2413 stores all chip state inside the `OPLL` struct
// returned by `OPLL_new`; there is no thread-local or global state.
// Callers are responsible for not aliasing the same `Opll` from
// multiple threads concurrently - `&mut self` on every method that
// mutates ensures Rust enforces this. The struct is otherwise
// portable across threads.
unsafe impl Send for Opll {}

impl Opll {
    /// Construct a fresh VRC7-mode chip. Panics only if emu2413 fails
    /// its own internal allocation, which would only happen under
    /// genuine OOM.
    pub fn new() -> Self {
        // SAFETY: `OPLL_new` returns either a valid pointer or NULL.
        // We immediately wrap with `NonNull::new` and panic on the
        // NULL case rather than ever dereferencing a null pointer.
        let raw = unsafe { OPLL_new(OPLL_CLOCK_RATE, OPLL_SAMPLE_RATE) };
        let handle = NonNull::new(raw).expect("emu2413: OPLL_new returned NULL");

        let mut opll = Self { handle };
        // SAFETY: `handle` is freshly constructed, valid, and exclusively
        // owned by `opll`; both calls only mutate state behind the
        // handle. `CHIP_TYPE_VRC7` is the documented VRC7 selector.
        unsafe {
            OPLL_setChipType(opll.handle.as_ptr(), CHIP_TYPE_VRC7);
            OPLL_resetPatch(opll.handle.as_ptr(), CHIP_TYPE_VRC7);
        }
        opll.reset();
        opll
    }

    /// Power-on / reset. Clears all envelope and phase state but
    /// preserves the patch ROM (i.e. the 15 fixed VRC7 instruments
    /// stay loaded - they live in the ROM, not the writable registers).
    pub fn reset(&mut self) {
        // SAFETY: handle is non-null and exclusively borrowed via &mut self.
        unsafe { OPLL_reset(self.handle.as_ptr()) };
    }

    /// Write a value to OPLL register `reg`. VRC7 exposes registers
    /// `$00-$07` (instrument patch RAM), `$0E` (rhythm - VRC7 ignores
    /// it), `$10-$18` (per-channel low F-number / period), `$20-$28`
    /// (per-channel block + key-on + sus), and `$30-$38` (per-channel
    /// instrument # + volume). emu2413 silently ignores out-of-range
    /// addresses, so we don't gate them here.
    pub fn write_reg(&mut self, reg: u8, value: u8) {
        // SAFETY: handle is non-null and exclusively borrowed via &mut self.
        unsafe { OPLL_writeReg(self.handle.as_ptr(), reg as u32, value) };
    }

    /// Advance the chip by one OPLL sample (one tick at the native
    /// 49716 Hz rate) and return the mono mixed output as a 16-bit
    /// signed sample.
    pub fn calc(&mut self) -> i16 {
        // SAFETY: handle is non-null and exclusively borrowed via &mut self.
        unsafe { OPLL_calc(self.handle.as_ptr()) }
    }
}

impl Drop for Opll {
    fn drop(&mut self) {
        // SAFETY: handle was obtained from `OPLL_new` and has not been
        // freed - `Drop` runs exactly once per instance.
        unsafe { OPLL_delete(self.handle.as_ptr()) };
    }
}

impl Default for Opll {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opll_constructs_and_drops_cleanly() {
        let _opll = Opll::new();
    }

    #[test]
    fn silent_chip_outputs_zero() {
        let mut opll = Opll::new();
        // No key-on, no register writes - every channel should be
        // silent and the mixed output should sit at exactly zero for
        // any number of samples.
        for _ in 0..1024 {
            assert_eq!(opll.calc(), 0);
        }
    }

    #[test]
    fn key_on_first_channel_produces_signal() {
        let mut opll = Opll::new();
        // Patch 1 (Violin) on channel 0, max volume.
        opll.write_reg(0x30, 0x10);
        // F-number low byte for ~A4. Exact pitch doesn't matter - we
        // just want a non-silent envelope.
        opll.write_reg(0x10, 0x80);
        // Block=4, F-number high bit = 0, key-on bit set.
        opll.write_reg(0x20, 0x20 | 0x10);

        let mut peak = 0_i32;
        for _ in 0..(OPLL_SAMPLE_RATE / 10) as usize {
            peak = peak.max(opll.calc().unsigned_abs() as i32);
        }
        assert!(
            peak > 100,
            "expected audible signal after key-on, peak amplitude was {peak}"
        );
    }
}
