// SPDX-License-Identifier: GPL-3.0-or-later
//! Sony S-DSP register file, voice state, BRR decoder, and the
//! sample-rate mixer. Phase 5c builds this up incrementally:
//!
//! - **5c.1 (this file's first cut)**: register layout constants +
//!   structured accessors. Replaces the passthrough stub from 5b.1
//!   so the rest of the DSP work can address registers by name
//!   instead of magic numbers.
//! - **5c.2**: BRR sample decoder (`brr` submodule).
//! - **5c.3**: voice pitch counter + Gaussian 4-point interpolation.
//! - **5c.4**: ADSR / GAIN envelope generator.
//! - **5c.5**: master mix → 32 kHz stereo samples through `AudioSink`.
//! - **5c.6**: echo unit (FIR + delay buffer in ARAM).
//! - **5c.7**: noise + pitch modulation.
//!
//! References (clean-room-adjacent porting per project policy):
//! Anomie's S-DSP notes, Mesen2 `Core/SNES/Dsp/Dsp.cpp` for the
//! register layout / event model, bsnes-plus `bsnes/snes/dsp/`
//! for the per-cycle scheduling skeleton.

pub mod brr;
pub mod echo;
pub mod envelope;
pub mod mixer;
pub mod voice;
pub mod voice_sampler;

/// Per-voice register offsets within an `$X0`-`$X9` block. Voice `i`'s
/// register `voice_reg` lives at `(i << 4) | voice_reg`.
///
/// | offset | name  | role                                          |
/// |--------|-------|-----------------------------------------------|
/// | `$X0`  | VOLL  | Left volume (signed 8-bit, post-envelope mul) |
/// | `$X1`  | VOLR  | Right volume (signed 8-bit)                   |
/// | `$X2`  | PL    | Pitch low byte (14-bit, 1.0 == sample rate)   |
/// | `$X3`  | PH    | Pitch high byte (low 6 bits used)             |
/// | `$X4`  | SRCN  | Sample number (index into DIR table)          |
/// | `$X5`  | ADSR1 | ADSR enable + attack/decay rates              |
/// | `$X6`  | ADSR2 | Sustain level + sustain rate                  |
/// | `$X7`  | GAIN  | GAIN-mode envelope control                    |
/// | `$X8`  | ENVX  | Current envelope level (read-only mirror)     |
/// | `$X9`  | OUTX  | Current voice output (read-only mirror)       |
pub mod voice_reg {
    pub const VOLL: u8 = 0x0;
    pub const VOLR: u8 = 0x1;
    pub const PL: u8 = 0x2;
    pub const PH: u8 = 0x3;
    pub const SRCN: u8 = 0x4;
    pub const ADSR1: u8 = 0x5;
    pub const ADSR2: u8 = 0x6;
    pub const GAIN: u8 = 0x7;
    pub const ENVX: u8 = 0x8;
    pub const OUTX: u8 = 0x9;
}

/// Global S-DSP register addresses (the ones that don't live in a
/// per-voice block). The `$XF` slots hold the 8-tap echo FIR
/// coefficients C0..C7, indexed by voice number; the rest are
/// scattered through the high-nibble pages.
///
/// | addr  | name  | role                                          |
/// |-------|-------|-----------------------------------------------|
/// | `$0C` | MVOLL | Master volume left (signed 8-bit)             |
/// | `$1C` | MVOLR | Master volume right                           |
/// | `$2C` | EVOLL | Echo volume left                              |
/// | `$3C` | EVOLR | Echo volume right                             |
/// | `$4C` | KON   | Key-on (bit per voice, write triggers attack) |
/// | `$5C` | KOFF  | Key-off (bit per voice, write triggers release)|
/// | `$6C` | FLG   | Flags: reset/mute/echo-write/noise-rate       |
/// | `$7C` | ENDX  | End-of-sample mirror (bit per voice)          |
/// | `$0D` | EFB   | Echo feedback (signed 8-bit)                  |
/// | `$2D` | PMON  | Pitch modulation enable mask (bit per voice)  |
/// | `$3D` | NON   | Noise enable mask (bit per voice)             |
/// | `$4D` | EON   | Echo enable mask (bit per voice)              |
/// | `$5D` | DIR   | Sample directory base page (addr = DIR << 8)  |
/// | `$6D` | ESA   | Echo start address (page; ARAM offset = ESA<<8)|
/// | `$7D` | EDL   | Echo delay (low 4 bits, units of 2 KiB)       |
/// | `$xF` | FIR  | Echo FIR coefficient (one per voice slot 0-7) |
pub mod global_reg {
    pub const MVOLL: u8 = 0x0C;
    pub const MVOLR: u8 = 0x1C;
    pub const EVOLL: u8 = 0x2C;
    pub const EVOLR: u8 = 0x3C;
    pub const KON: u8 = 0x4C;
    pub const KOFF: u8 = 0x5C;
    pub const FLG: u8 = 0x6C;
    pub const ENDX: u8 = 0x7C;
    pub const EFB: u8 = 0x0D;
    pub const PMON: u8 = 0x2D;
    pub const NON: u8 = 0x3D;
    pub const EON: u8 = 0x4D;
    pub const DIR: u8 = 0x5D;
    pub const ESA: u8 = 0x6D;
    pub const EDL: u8 = 0x7D;
    /// FIR coefficient for voice `i` lives at `(i << 4) | 0x0F`.
    pub fn fir(i: usize) -> u8 {
        debug_assert!(i < 8);
        ((i as u8) << 4) | 0x0F
    }
}

/// FLG register bit assignments.
pub mod flg_bit {
    /// Soft-reset: when set, all voices are muted, KON is ignored, and
    /// the DSP holds in idle until the bit is cleared.
    pub const SOFT_RESET: u8 = 0x80;
    /// Mute: when set, output is silenced (echo still runs).
    pub const MUTE: u8 = 0x40;
    /// Echo-write disable: when set, the echo unit does not write
    /// back to ARAM (echo reads still happen, mix output unaffected).
    pub const ECHO_WRITE_DISABLE: u8 = 0x20;
    /// Noise rate selector lives in bits 4..=0 (5 bits → 32 entries).
    pub const NOISE_RATE_MASK: u8 = 0x1F;
}

/// Sony S-DSP register file. Accesses are address-routed through the
/// `$F2`/`$F3` ports on the SMP side. The flat `regs[128]` array
/// preserves Mesen2's straightforward reg-as-byte model; structured
/// accessors below just compose offsets so the rest of the DSP code
/// can read voices by name.
///
/// All registers reset to zero - including FLG, which means `MUTE = 0`
/// and `SOFT_RESET = 0` at power-on. Real hardware leaves the FLG
/// register undefined on cold boot; commercial games write FLG as
/// part of their reset sequence so the indeterminate window doesn't
/// matter.
#[derive(Debug, Clone)]
pub struct DspRegs {
    /// `$F2` address latch. Bit 7 selects the read mirror; writes to
    /// addresses with bit 7 set are silently dropped on real hardware
    /// and we mirror that behaviour.
    pub address: u8,
    /// 128-byte register file. Writes to `$80-$FF` (high-bit set on
    /// the address latch) bounce off without storing.
    pub regs: [u8; 128],
}

impl DspRegs {
    pub const fn new() -> Self {
        Self {
            address: 0,
            regs: [0; 128],
        }
    }

    /// Read the currently-selected register. Bit 7 of the address is
    /// the read-mirror select on real hardware; we mask it to 7 bits
    /// so reading from `$80+offset` yields whatever is at `$00+offset`.
    /// That matches Mesen2 `Dsp::ReadRam` for the cases SPC code uses.
    pub fn read_data(&self) -> u8 {
        self.regs[(self.address & 0x7F) as usize]
    }

    /// Write the data port. Real hardware ignores writes to `$80-$FF`
    /// (the high bit is the read-mirror select, not a writable
    /// register), so we silently drop those.
    pub fn write_data(&mut self, value: u8) {
        if self.address & 0x80 == 0 {
            self.regs[(self.address & 0x7F) as usize] = value;
        }
    }

    /// Direct register read by absolute address. Used by the host /
    /// other internal subsystems (e.g. the upcoming mixer reading
    /// voice pitch / volume per sample). Bit 7 of `addr` is masked
    /// per the same rule as `read_data`.
    pub fn read(&self, addr: u8) -> u8 {
        self.regs[(addr & 0x7F) as usize]
    }

    /// Direct register write by absolute address. Bypasses the address
    /// latch but applies the same `$80+` silence rule.
    pub fn write(&mut self, addr: u8, value: u8) {
        if addr & 0x80 == 0 {
            self.regs[(addr & 0x7F) as usize] = value;
        }
    }

    /// Lookup `(voice << 4) | offset` for one of the per-voice
    /// register slots. `voice` must be 0-7; `offset` is one of the
    /// `voice_reg::*` constants.
    pub fn voice_addr(voice: usize, offset: u8) -> u8 {
        debug_assert!(voice < 8);
        debug_assert!(offset < 0x10);
        ((voice as u8) << 4) | offset
    }

    pub fn voice_reg(&self, voice: usize, offset: u8) -> u8 {
        self.read(Self::voice_addr(voice, offset))
    }

    pub fn set_voice_reg(&mut self, voice: usize, offset: u8, value: u8) {
        self.write(Self::voice_addr(voice, offset), value);
    }

    /// Signed 8-bit voice volume L/R. Voice volumes scale the
    /// post-envelope sample before mixing; SNES treats them as 2's-
    /// complement, so a value of `0x80` means "negative full scale"
    /// (an inverter, used for stereo width).
    pub fn voice_volume_left(&self, voice: usize) -> i8 {
        self.voice_reg(voice, voice_reg::VOLL) as i8
    }
    pub fn voice_volume_right(&self, voice: usize) -> i8 {
        self.voice_reg(voice, voice_reg::VOLR) as i8
    }

    /// 14-bit pitch register. The high byte's top 2 bits are reserved
    /// (mask `$3F`); the low byte is a fractional sample-rate
    /// multiplier. `$1000` (4096) means "one output sample per BRR
    /// sample" - i.e. play at the source sample rate.
    pub fn voice_pitch(&self, voice: usize) -> u16 {
        let lo = self.voice_reg(voice, voice_reg::PL) as u16;
        let hi = (self.voice_reg(voice, voice_reg::PH) & 0x3F) as u16;
        (hi << 8) | lo
    }

    pub fn voice_source_number(&self, voice: usize) -> u8 {
        self.voice_reg(voice, voice_reg::SRCN)
    }

    pub fn voice_adsr1(&self, voice: usize) -> u8 {
        self.voice_reg(voice, voice_reg::ADSR1)
    }
    pub fn voice_adsr2(&self, voice: usize) -> u8 {
        self.voice_reg(voice, voice_reg::ADSR2)
    }
    pub fn voice_gain(&self, voice: usize) -> u8 {
        self.voice_reg(voice, voice_reg::GAIN)
    }

    /// ADSR-mode flag. When set in ADSR1 bit 7, the voice runs the
    /// 4-stage envelope; otherwise it's GAIN-mode.
    pub fn voice_adsr_enabled(&self, voice: usize) -> bool {
        self.voice_adsr1(voice) & 0x80 != 0
    }

    pub fn master_volume_left(&self) -> i8 {
        self.read(global_reg::MVOLL) as i8
    }
    pub fn master_volume_right(&self) -> i8 {
        self.read(global_reg::MVOLR) as i8
    }
    pub fn echo_volume_left(&self) -> i8 {
        self.read(global_reg::EVOLL) as i8
    }
    pub fn echo_volume_right(&self) -> i8 {
        self.read(global_reg::EVOLR) as i8
    }
    pub fn echo_feedback(&self) -> i8 {
        self.read(global_reg::EFB) as i8
    }

    pub fn kon(&self) -> u8 {
        self.read(global_reg::KON)
    }
    pub fn koff(&self) -> u8 {
        self.read(global_reg::KOFF)
    }
    pub fn flg(&self) -> u8 {
        self.read(global_reg::FLG)
    }
    pub fn endx(&self) -> u8 {
        self.read(global_reg::ENDX)
    }
    pub fn pmon(&self) -> u8 {
        self.read(global_reg::PMON)
    }
    pub fn non(&self) -> u8 {
        self.read(global_reg::NON)
    }
    pub fn eon(&self) -> u8 {
        self.read(global_reg::EON)
    }

    pub fn soft_reset(&self) -> bool {
        self.flg() & flg_bit::SOFT_RESET != 0
    }
    pub fn muted(&self) -> bool {
        self.flg() & flg_bit::MUTE != 0
    }
    pub fn echo_write_disabled(&self) -> bool {
        self.flg() & flg_bit::ECHO_WRITE_DISABLE != 0
    }
    pub fn noise_rate_index(&self) -> u8 {
        self.flg() & flg_bit::NOISE_RATE_MASK
    }

    /// Sample directory base ARAM address. SRCN indexes a 4-byte
    /// entry at this base + (SRCN * 4). Writes to DIR change the
    /// base for *future* key-ons; in-flight voices keep their
    /// already-resolved start/loop pointers.
    pub fn sample_directory_addr(&self) -> u16 {
        (self.read(global_reg::DIR) as u16) << 8
    }

    /// Echo buffer start address in ARAM (page-aligned).
    pub fn echo_start_addr(&self) -> u16 {
        (self.read(global_reg::ESA) as u16) << 8
    }

    /// Raw ESA register byte (high byte of the echo buffer base
    /// address). Convenience for the echo unit which keeps its own
    /// 1-sample-delayed cache of this value.
    pub fn echo_start_byte(&self) -> u8 {
        self.read(global_reg::ESA)
    }

    /// Echo delay length in 2 KiB units. Multiply by 2048 to get
    /// the buffer size in bytes; multiply by 16 to get its length
    /// in 32 kHz samples.
    pub fn echo_delay(&self) -> u8 {
        self.read(global_reg::EDL) & 0x0F
    }

    /// FIR coefficient for echo tap `i` (0..=7). Signed 8-bit.
    pub fn fir_coefficient(&self, i: usize) -> i8 {
        self.read(global_reg::fir(i)) as i8
    }
}

impl Default for DspRegs {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_addr_layout_matches_per_voice_block() {
        // Voice 0's VOLL is $00, voice 7's OUTX is $79.
        assert_eq!(DspRegs::voice_addr(0, voice_reg::VOLL), 0x00);
        assert_eq!(DspRegs::voice_addr(0, voice_reg::OUTX), 0x09);
        assert_eq!(DspRegs::voice_addr(7, voice_reg::VOLL), 0x70);
        assert_eq!(DspRegs::voice_addr(7, voice_reg::OUTX), 0x79);
    }

    #[test]
    fn voice_volume_round_trips_signed() {
        let mut d = DspRegs::new();
        d.set_voice_reg(3, voice_reg::VOLL, 0x7F);
        d.set_voice_reg(3, voice_reg::VOLR, 0x80);
        assert_eq!(d.voice_volume_left(3), 127);
        assert_eq!(d.voice_volume_right(3), -128);
    }

    #[test]
    fn voice_pitch_combines_14_bits_masking_high_two() {
        let mut d = DspRegs::new();
        d.set_voice_reg(0, voice_reg::PL, 0x34);
        d.set_voice_reg(0, voice_reg::PH, 0xFF); // high two bits ignored
        assert_eq!(d.voice_pitch(0), 0x3F34);
    }

    #[test]
    fn voice_adsr_enabled_reflects_adsr1_bit_seven() {
        let mut d = DspRegs::new();
        d.set_voice_reg(2, voice_reg::ADSR1, 0x80);
        assert!(d.voice_adsr_enabled(2));
        d.set_voice_reg(2, voice_reg::ADSR1, 0x7F);
        assert!(!d.voice_adsr_enabled(2));
    }

    #[test]
    fn flg_helpers_decode_bits_correctly() {
        let mut d = DspRegs::new();
        d.write(global_reg::FLG, flg_bit::SOFT_RESET | 0x07);
        assert!(d.soft_reset());
        assert!(!d.muted());
        assert!(!d.echo_write_disabled());
        assert_eq!(d.noise_rate_index(), 0x07);

        d.write(global_reg::FLG, flg_bit::MUTE | flg_bit::ECHO_WRITE_DISABLE);
        assert!(d.muted());
        assert!(d.echo_write_disabled());
        assert!(!d.soft_reset());
        assert_eq!(d.noise_rate_index(), 0);
    }

    #[test]
    fn sample_directory_address_is_page_shifted() {
        let mut d = DspRegs::new();
        d.write(global_reg::DIR, 0x12);
        assert_eq!(d.sample_directory_addr(), 0x1200);
    }

    #[test]
    fn echo_start_and_delay_decode() {
        let mut d = DspRegs::new();
        d.write(global_reg::ESA, 0x40);
        d.write(global_reg::EDL, 0x0A);
        assert_eq!(d.echo_start_addr(), 0x4000);
        assert_eq!(d.echo_delay(), 0x0A);
    }

    #[test]
    fn fir_coefficient_is_per_voice_slot_signed() {
        let mut d = DspRegs::new();
        d.write(global_reg::fir(3), 0xFF);
        assert_eq!(d.fir_coefficient(3), -1);
    }

    #[test]
    fn write_to_high_address_is_silenced() {
        // The DSP "address" port's high bit is the read-mirror select;
        // writes to $80-$FF are no-ops on real hardware.
        let mut d = DspRegs::new();
        d.address = 0x82;
        d.write_data(0x77);
        // Reading from the same address (high bit masked off) finds
        // whatever was at $02 - which we never wrote, so 0.
        assert_eq!(d.regs[0x02], 0);
    }

    #[test]
    fn read_high_address_mirrors_low_register() {
        // $80-$FF mirror $00-$7F on read: writing $02 then reading
        // through $82 returns the same byte.
        let mut d = DspRegs::new();
        d.address = 0x02;
        d.write_data(0xAB);
        d.address = 0x82;
        assert_eq!(d.read_data(), 0xAB);
    }

    #[test]
    fn write_to_high_address_does_not_corrupt_mirror_target() {
        // A blocked write to $82 must not stomp the $02 register.
        let mut d = DspRegs::new();
        d.address = 0x02;
        d.write_data(0x55);
        d.address = 0x82;
        d.write_data(0xFF); // dropped
        d.address = 0x02;
        assert_eq!(d.read_data(), 0x55, "blocked write must not reach mirror");
    }

    #[test]
    fn write_data_guard_threshold_at_address_0x80() {
        // Boundary: $7F writes through, $80 is blocked.
        let mut d = DspRegs::new();
        d.address = 0x7F;
        d.write_data(0x11);
        assert_eq!(d.regs[0x7F], 0x11);

        d.address = 0x80;
        d.write_data(0x22);
        assert_eq!(d.regs[0x00], 0x00, "$80 must not write to $00 mirror target");
    }

    #[test]
    fn kon_koff_round_trip() {
        let mut d = DspRegs::new();
        d.write(global_reg::KON, 0xF0);
        d.write(global_reg::KOFF, 0x0F);
        assert_eq!(d.kon(), 0xF0);
        assert_eq!(d.koff(), 0x0F);
    }

    #[test]
    fn voice_source_number_round_trips() {
        let mut d = DspRegs::new();
        d.set_voice_reg(5, voice_reg::SRCN, 0x42);
        assert_eq!(d.voice_source_number(5), 0x42);
    }
}
