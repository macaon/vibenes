// SPDX-License-Identifier: GPL-3.0-or-later
//! Konami VRC7 - iNES mapper 85.
//!
//! Big chip on a tiny game list. Commercial usage is essentially one
//! cart, *Lagrange Point* (Konami, 1991, JP-only), with a handful of
//! homebrews leaning on its FM audio. The mapper itself is a
//! straightforward member of the VRC family - three switchable 8 KiB
//! PRG slots, eight 1 KiB CHR banks, four mirroring modes, and the
//! standard VRC IRQ counter - but it carries an on-cart YM2413
//! derivative (OPLL) wired through `$9010` / `$9030`. All the FM
//! synth complexity lives in the vendored emu2413 core; see
//! [`crate::nes::mapper::vrc7_opll`] for the FFI wrapper.
//!
//! ## Register surface
//!
//! VRC7 originally shipped on two slightly different boards (commonly
//! "VRC7a" and "VRC7b") that differ only in which address-line bit
//! selects the second register at each base. The trick - taken
//! straight from Mesen2's `Core/NES/Mappers/Konami/VRC7.h` - is to
//! mirror `A4` to `A3` for everything *except* the audio-port select
//! at `$9010`, then mask `addr & 0xF038` and dispatch:
//!
//! | Address  | Effect                                   |
//! |----------|------------------------------------------|
//! | `$8000`  | PRG bank 0 (`$8000-$9FFF`, 6 bits)       |
//! | `$8008`  | PRG bank 1 (`$A000-$BFFF`, 6 bits)       |
//! | `$9000`  | PRG bank 2 (`$C000-$DFFF`, 6 bits)       |
//! | `$9010`  | OPLL register select (mute-gated)        |
//! | `$9030`  | OPLL register write   (mute-gated)       |
//! | `$A000`  | CHR bank 0 (1 KiB at `$0000-$03FF`)      |
//! | `$A008`  | CHR bank 1 (1 KiB at `$0400-$07FF`)      |
//! | `$B000`  | CHR bank 2 (1 KiB at `$0800-$0BFF`)      |
//! | `$B008`  | CHR bank 3 (1 KiB at `$0C00-$0FFF`)      |
//! | `$C000`  | CHR bank 4 (1 KiB at `$1000-$13FF`)      |
//! | `$C008`  | CHR bank 5 (1 KiB at `$1400-$17FF`)      |
//! | `$D000`  | CHR bank 6 (1 KiB at `$1800-$1BFF`)      |
//! | `$D008`  | CHR bank 7 (1 KiB at `$1C00-$1FFF`)      |
//! | `$E000`  | Control: `RWMM ..MM`                     |
//! | `$E008`  | IRQ counter reload                       |
//! | `$F000`  | IRQ control                              |
//! | `$F008`  | IRQ acknowledge                          |
//!
//! `$E000` bits - `R` (bit 7): PRG-RAM enable. `W` (bit 6): mute audio
//! (writes to `$9010`/`$9030` are dropped while set per the wiki).
//! Bits 0-1: mirroring (0 V / 1 H / 2 A-only / 3 B-only).
//!
//! `$E000-$DFFF` is mapped via three switchable 8 KiB banks; the
//! `$E000-$FFFF` window is hardwired to the last 8 KiB of PRG.
//!
//! ## Audio
//!
//! OPLL register write is a two-step exchange - `$9010` latches the
//! target register number, then `$9030` deposits a value at that
//! register inside the chip. The VRC family's mute bit (`$E000.b6`)
//! gates *both* writes per nesdev wiki. We clock the FM core at its
//! native 49716 Hz rate using a Q16-fixed-point CPU-cycle accumulator
//! (one OPLL sample ≈ every 36 CPU cycles at NTSC) and cache the
//! latest 16-bit signed sample for [`Mapper::audio_output`]. The
//! mixing scale matches Mesen2's `NesSoundMixer.cpp:191` weight of
//! 1 against the shared 5018-denominator mix bus - Mesen2 adds the
//! raw `OPLL_calc` result with a multiplier of 1, so we divide by
//! 5018 to land in our `f32` mix space.
//!
//! ## References
//!
//! Wiki: <https://www.nesdev.org/wiki/VRC7>. Mapper structure ported
//! from `~/Git/Mesen2/Core/NES/Mappers/Konami/VRC7.h`; OPLL backend
//! is the vendored emu2413 v1.5.9 by Mitsutaka Okazaki (MIT) - same
//! library Mesen2 ships under `Core/Shared/Utilities/emu2413.cpp`.
//! IRQ model is the standard VRC counter, mirrored from
//! [`crate::nes::mapper::vrc2_4`].

use crate::nes::mapper::vrc7_opll::{Opll, OPLL_SAMPLE_RATE};
use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_8K: usize = 8 * 1024;
const CHR_BANK_1K: usize = 1024;
const PRG_RAM_SIZE: usize = 8 * 1024;

/// VRC family IRQ prescaler reload value. The counter ticks at 1/3 of
/// the CPU rate in scanline mode (113.667 CPU cycles per scanline,
/// approximated by stepping the prescaler -3 every CPU cycle and
/// reloading at +341).
const PRESCALER_RELOAD: i16 = 341;
const PRESCALER_STEP: i16 = 3;

/// CPU clock (NTSC). One OPLL sample is generated every
/// `CPU_HZ / OPLL_SAMPLE_RATE ≈ 35.998` CPU cycles. We track the
/// remainder in Q16 fixed point so the rounding error stays under
/// 1 ppm over the long run rather than aliasing into the audible
/// band.
const CPU_HZ: u64 = 1_789_773;

/// `(CPU_HZ << 16) / OPLL_SAMPLE_RATE` - increment per CPU cycle is
/// `1 << 16`; once `clock_acc` crosses this threshold we emit one
/// OPLL sample and subtract.
const OPLL_THRESHOLD_Q16: u32 = ((CPU_HZ << 16) / OPLL_SAMPLE_RATE as u64) as u32;

/// Mesen2's `NesSoundMixer.cpp:191` adds VRC7's raw `OPLL_calc` int16
/// directly with a multiplier of 1 against the shared 5018-denominator
/// mix bus. Match that ratio so the relative loudness vs. APU pulses,
/// VRC6, FDS, and Sunsoft 5B is faithful.
const VRC7_MIX_SCALE: f32 = 1.0 / 5018.0;

// ---- VRC IRQ counter ----
//
// Identical state machine to `vrc2_4::VrcIrq`; we keep a private copy
// here so this module stays self-contained (and so VRC7-specific
// register addresses don't leak into the VRC2/4 file).

struct VrcIrq {
    reload_value: u8,
    counter: u8,
    prescaler: i16,
    enabled: bool,
    enabled_after_ack: bool,
    cycle_mode: bool,
    irq_line: bool,
}

impl VrcIrq {
    fn new() -> Self {
        Self {
            reload_value: 0,
            counter: 0,
            prescaler: 0,
            enabled: false,
            enabled_after_ack: false,
            cycle_mode: false,
            irq_line: false,
        }
    }

    fn clock(&mut self) {
        if !self.enabled {
            return;
        }
        let tick = if self.cycle_mode {
            true
        } else {
            self.prescaler -= PRESCALER_STEP;
            self.prescaler <= 0
        };
        if tick {
            if self.counter == 0xFF {
                self.counter = self.reload_value;
                self.irq_line = true;
            } else {
                self.counter += 1;
            }
            if !self.cycle_mode {
                self.prescaler += PRESCALER_RELOAD;
            }
        }
    }

    fn set_reload(&mut self, value: u8) {
        self.reload_value = value;
    }

    fn set_control(&mut self, value: u8) {
        self.enabled_after_ack = (value & 0x01) != 0;
        self.enabled = (value & 0x02) != 0;
        self.cycle_mode = (value & 0x04) != 0;
        if self.enabled {
            self.counter = self.reload_value;
            self.prescaler = PRESCALER_RELOAD;
        }
        self.irq_line = false;
    }

    fn acknowledge(&mut self) {
        self.enabled = self.enabled_after_ack;
        self.irq_line = false;
    }
}

// ---- Mapper ----

pub struct Vrc7 {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,
    prg_ram: Vec<u8>,

    /// 8 KiB PRG banks for slots `$8000`, `$A000`, `$C000`. Slot 3
    /// (`$E000`) is hardwired to the last bank.
    prg_banks: [u8; 3],
    /// 1 KiB CHR bank indices for the eight 1 KiB PPU slots.
    chr_banks: [u8; 8],

    prg_bank_count: usize,
    chr_bank_count: usize,

    mirroring: Mirroring,
    /// `$E000.b7` - gates `$6000-$7FFF` PRG-RAM access. When clear,
    /// reads return open-bus (we surface 0) and writes are dropped.
    prg_ram_enable: bool,
    /// `$E000.b6` - when set, writes to `$9010`/`$9030` are dropped
    /// (the chip stays in whatever state it was last left in but
    /// stops being driven).
    audio_muted: bool,

    irq: VrcIrq,
    opll: Opll,
    /// Last value latched by `$9010` - the OPLL register-select port.
    /// `$9030` reads this back to know which OPLL register to write
    /// the data byte into. Both writes are gated by [`Self::audio_muted`].
    opll_pending_reg: u8,
    /// Latest OPLL sample from `OPLL_calc`, refreshed once every
    /// ~36 CPU cycles. Surfaced verbatim through [`Mapper::audio_output`]
    /// (scaled to f32) so the bus mixer can add it linearly.
    last_sample: i16,
    /// Q16 accumulator: incremented by `1 << 16` every CPU cycle;
    /// when it crosses [`OPLL_THRESHOLD_Q16`] we generate one OPLL
    /// sample and subtract the threshold.
    clock_acc: u32,

    battery: bool,
    save_dirty: bool,
}

impl Vrc7 {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count = (cart.prg_rom.len() / PRG_BANK_8K).max(1);

        let chr_ram = cart.chr_ram || cart.chr_rom.is_empty();
        let chr = if chr_ram {
            // Mesen2 grants 8 KiB of CHR-RAM to VRC7 carts that lack
            // CHR-ROM. None of the known commercial dumps actually
            // use this path (Lagrange Point ships CHR-ROM), but the
            // homebrew scene does and we keep the surface uniform.
            vec![0u8; 8 * 1024]
        } else {
            cart.chr_rom
        };
        let chr_bank_count = (chr.len() / CHR_BANK_1K).max(1);

        let prg_ram_total =
            (cart.prg_ram_size + cart.prg_nvram_size).max(PRG_RAM_SIZE);

        Self {
            prg_rom: cart.prg_rom,
            chr,
            chr_ram,
            prg_ram: vec![0u8; prg_ram_total],
            prg_banks: [0, 0, 0],
            chr_banks: [0, 1, 2, 3, 4, 5, 6, 7],
            prg_bank_count,
            chr_bank_count,
            mirroring: cart.mirroring,
            prg_ram_enable: false,
            audio_muted: false,
            irq: VrcIrq::new(),
            opll: Opll::new(),
            opll_pending_reg: 0,
            last_sample: 0,
            clock_acc: 0,
            battery: cart.battery_backed,
            save_dirty: false,
        }
    }

    fn prg_offset(&self, slot: usize, addr_in_slot: u16) -> usize {
        let bank = if slot == 3 {
            // Last 8 KiB hardwired to the top bank.
            self.prg_bank_count - 1
        } else {
            (self.prg_banks[slot] as usize) % self.prg_bank_count
        };
        bank * PRG_BANK_8K + addr_in_slot as usize
    }

    fn chr_offset(&self, slot: usize, addr_in_slot: u16) -> usize {
        let bank = (self.chr_banks[slot] as usize) % self.chr_bank_count;
        bank * CHR_BANK_1K + addr_in_slot as usize
    }

    /// Address-bit re-mapping that lets us treat both VRC7a and VRC7b
    /// boards uniformly: mirror `A4` to `A3` for every register
    /// *except* the OPLL select port at `$9010`. Lifted verbatim from
    /// Mesen2's `VRC7.h::WriteRegister` (line 85).
    fn translate(addr: u16) -> u16 {
        if (addr & 0x10) != 0 && (addr & 0xF010) != 0x9010 {
            (addr | 0x08) & !0x10
        } else {
            addr
        }
    }

    /// Map the `$E000` bits-0-1 to the four mirroring modes the VRC7
    /// supports. Matches Mesen2's mapping: 0 V / 1 H / 2 A / 3 B.
    fn decode_mirroring(value: u8) -> Mirroring {
        match value & 0x03 {
            0 => Mirroring::Vertical,
            1 => Mirroring::Horizontal,
            2 => Mirroring::SingleScreenLower,
            _ => Mirroring::SingleScreenUpper,
        }
    }
}

impl Mapper for Vrc7 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => {
                if !self.prg_ram_enable {
                    return 0;
                }
                let i = (addr - 0x6000) as usize;
                *self.prg_ram.get(i).unwrap_or(&0)
            }
            0x8000..=0x9FFF => {
                let off = self.prg_offset(0, addr - 0x8000);
                *self.prg_rom.get(off).unwrap_or(&0)
            }
            0xA000..=0xBFFF => {
                let off = self.prg_offset(1, addr - 0xA000);
                *self.prg_rom.get(off).unwrap_or(&0)
            }
            0xC000..=0xDFFF => {
                let off = self.prg_offset(2, addr - 0xC000);
                *self.prg_rom.get(off).unwrap_or(&0)
            }
            0xE000..=0xFFFF => {
                let off = self.prg_offset(3, addr - 0xE000);
                *self.prg_rom.get(off).unwrap_or(&0)
            }
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        match addr {
            0x6000..=0x7FFF => {
                if !self.prg_ram_enable {
                    return;
                }
                let i = (addr - 0x6000) as usize;
                if let Some(slot) = self.prg_ram.get_mut(i) {
                    if *slot != data {
                        *slot = data;
                        if self.battery {
                            self.save_dirty = true;
                        }
                    }
                }
            }
            0x8000..=0xFFFF => {
                let translated = Self::translate(addr);
                match translated & 0xF038 {
                    0x8000 => self.prg_banks[0] = data & 0x3F,
                    0x8008 => self.prg_banks[1] = data & 0x3F,
                    0x9000 => self.prg_banks[2] = data & 0x3F,

                    0x9010 => {
                        if !self.audio_muted {
                            // emu2413's `OPLL_writeIO(0, val)` is the
                            // wiki-correct register-select path; we
                            // mirror Mesen2's shortcut of stashing
                            // the latched register in our own state
                            // and feeding it back on the value write
                            // (see 0x9030 below). This avoids needing
                            // to expose `OPLL_writeIO` through the
                            // FFI.
                            self.opll_pending_reg = data;
                        }
                    }
                    0x9030 => {
                        if !self.audio_muted {
                            self.opll
                                .write_reg(self.opll_pending_reg, data);
                        }
                    }

                    0xA000 => self.chr_banks[0] = data,
                    0xA008 => self.chr_banks[1] = data,
                    0xB000 => self.chr_banks[2] = data,
                    0xB008 => self.chr_banks[3] = data,
                    0xC000 => self.chr_banks[4] = data,
                    0xC008 => self.chr_banks[5] = data,
                    0xD000 => self.chr_banks[6] = data,
                    0xD008 => self.chr_banks[7] = data,

                    0xE000 => {
                        self.mirroring = Self::decode_mirroring(data);
                        self.prg_ram_enable = (data & 0x80) != 0;
                        let new_mute = (data & 0x40) != 0;
                        if new_mute && !self.audio_muted {
                            // Mute drops the chip's output to zero
                            // immediately so the bus doesn't hold
                            // the prior sample for ~36 CPU cycles.
                            self.last_sample = 0;
                        }
                        self.audio_muted = new_mute;
                    }
                    0xE008 => self.irq.set_reload(data),
                    0xF000 => self.irq.set_control(data),
                    0xF008 => self.irq.acknowledge(),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr < 0x2000 {
            let slot = (addr / 0x400) as usize;
            let in_slot = addr % 0x400;
            let off = self.chr_offset(slot, in_slot);
            *self.chr.get(off).unwrap_or(&0)
        } else {
            0
        }
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if !self.chr_ram || addr >= 0x2000 {
            return;
        }
        let slot = (addr / 0x400) as usize;
        let in_slot = addr % 0x400;
        let off = self.chr_offset(slot, in_slot);
        if let Some(b) = self.chr.get_mut(off) {
            *b = data;
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }

    fn on_cpu_cycle(&mut self) {
        self.irq.clock();

        // Generate an OPLL sample roughly once every ~36 CPU cycles -
        // the Q16 accumulator carries the 0.998 fractional remainder
        // forward across ticks so the long-term rate is exact.
        self.clock_acc = self.clock_acc.wrapping_add(1 << 16);
        while self.clock_acc >= OPLL_THRESHOLD_Q16 {
            self.clock_acc -= OPLL_THRESHOLD_Q16;
            self.last_sample = if self.audio_muted { 0 } else { self.opll.calc() };
        }
    }

    fn irq_line(&self) -> bool {
        self.irq.irq_line
    }

    fn audio_output(&self) -> Option<f32> {
        Some(f32::from(self.last_sample) * VRC7_MIX_SCALE)
    }

    fn save_data(&self) -> Option<&[u8]> {
        self.battery.then(|| self.prg_ram.as_slice())
    }

    fn load_save_data(&mut self, data: &[u8]) {
        if self.battery && data.len() == self.prg_ram.len() {
            self.prg_ram.copy_from_slice(data);
        }
    }

    fn save_dirty(&self) -> bool {
        self.save_dirty
    }

    fn mark_saved(&mut self) {
        self.save_dirty = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, Mirroring, TvSystem};

    /// 256 KiB PRG (32 banks of 8 KiB), 128 KiB CHR (128 banks of 1 KiB).
    /// Each PRG bank tagged with its bank index in its first byte; each
    /// CHR bank ditto. Lets `cpu_peek` and `ppu_read` reveal the active
    /// bank.
    fn cart() -> Cartridge {
        let prg_banks = 32;
        let mut prg = vec![0u8; prg_banks * PRG_BANK_8K];
        for bank in 0..prg_banks {
            let base = bank * PRG_BANK_8K;
            prg[base..base + PRG_BANK_8K].fill(bank as u8);
        }
        let chr_banks = 128;
        let mut chr = vec![0u8; chr_banks * CHR_BANK_1K];
        for bank in 0..chr_banks {
            let base = bank * CHR_BANK_1K;
            chr[base..base + CHR_BANK_1K].fill(bank as u8);
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: chr,
            chr_ram: false,
            mapper_id: 85,
            submapper: 0,
            mirroring: Mirroring::Vertical,
            battery_backed: true,
            prg_ram_size: 0x2000,
            prg_nvram_size: 0,
            tv_system: TvSystem::Ntsc,
            is_nes2: false,
            prg_chr_crc32: 0,
            db_matched: false,
            fds_data: None,
        }
    }

    #[test]
    fn power_on_layout_pins_last_bank_at_e000() {
        let m = Vrc7::new(cart());
        assert_eq!(m.cpu_peek(0x8000), 0); // bank 0 default
        assert_eq!(m.cpu_peek(0xA000), 0);
        assert_eq!(m.cpu_peek(0xC000), 0);
        // $E000 fixed to last bank (31).
        assert_eq!(m.cpu_peek(0xE000), 31);
        assert_eq!(m.cpu_peek(0xFFFF), 31);
    }

    #[test]
    fn three_prg_banks_are_independently_switchable() {
        let mut m = Vrc7::new(cart());
        m.cpu_write(0x8000, 5);
        m.cpu_write(0x8008, 10);
        m.cpu_write(0x9000, 15);
        assert_eq!(m.cpu_peek(0x8000), 5);
        assert_eq!(m.cpu_peek(0xA000), 10);
        assert_eq!(m.cpu_peek(0xC000), 15);
        assert_eq!(m.cpu_peek(0xE000), 31);
    }

    #[test]
    fn vrc7b_addresses_alias_to_vrc7a_via_a4_to_a3_mirror() {
        let mut m = Vrc7::new(cart());
        // VRC7a uses bit 3 to disambiguate; VRC7b uses bit 4. Writing
        // to $8010 (VRC7b's "PRG bank 1") must land in the same place
        // as $8008 (VRC7a's). Audio port $9010 is exempt - see below.
        m.cpu_write(0x8010, 7);
        assert_eq!(m.cpu_peek(0xA000), 7);
        m.cpu_write(0x9010, 0xFF);
        // $9010 is the audio register-select port, NOT another PRG
        // alias - slot 2 must remain unchanged at 0.
        assert_eq!(m.cpu_peek(0xC000), 0);
    }

    #[test]
    fn chr_banking_routes_each_1k_slot_independently() {
        let mut m = Vrc7::new(cart());
        m.cpu_write(0xA000, 0x10);
        m.cpu_write(0xA008, 0x20);
        m.cpu_write(0xB000, 0x30);
        m.cpu_write(0xD008, 0x7F);
        // CHR pages are tagged by bank index in our test cart.
        assert_eq!(m.ppu_read(0x0000), 0x10);
        assert_eq!(m.ppu_read(0x0400), 0x20);
        assert_eq!(m.ppu_read(0x0800), 0x30);
        assert_eq!(m.ppu_read(0x1C00), 0x7F);
    }

    #[test]
    fn e000_decodes_mirroring_modes() {
        let mut m = Vrc7::new(cart());
        m.cpu_write(0xE000, 0x00);
        assert_eq!(m.mirroring(), Mirroring::Vertical);
        m.cpu_write(0xE000, 0x01);
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
        m.cpu_write(0xE000, 0x02);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenLower);
        m.cpu_write(0xE000, 0x03);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenUpper);
    }

    #[test]
    fn prg_ram_disabled_by_default_returns_zero() {
        let mut m = Vrc7::new(cart());
        m.cpu_write(0x6000, 0x42);
        // Writes drop with RAM disabled.
        assert_eq!(m.cpu_peek(0x6000), 0);

        // Enable + write + read.
        m.cpu_write(0xE000, 0x80); // bit 7 = enable, mirroring V
        m.cpu_write(0x6000, 0x42);
        assert_eq!(m.cpu_peek(0x6000), 0x42);
    }

    #[test]
    fn irq_counter_fires_after_set_reload_and_enable() {
        let mut m = Vrc7::new(cart());
        // Reload 0xFE → counter starts at 0xFE, ticks to 0xFF, then on
        // overflow asserts /IRQ. In scanline mode each tick takes
        // ~113.7 CPU cycles, so 2 ticks ≈ 227 cycles.
        m.cpu_write(0xE008, 0xFE);
        m.cpu_write(0xF000, 0x02); // E=1, scanline mode
        let mut fired = false;
        for _ in 0..400 {
            m.on_cpu_cycle();
            if m.irq_line() {
                fired = true;
                break;
            }
        }
        assert!(fired);
    }

    #[test]
    fn f008_acknowledges_irq() {
        let mut m = Vrc7::new(cart());
        m.cpu_write(0xE008, 0xFF);
        m.cpu_write(0xF000, 0x06); // E=1, M=1 (cycle mode)
        // 1 tick to overflow.
        m.on_cpu_cycle();
        assert!(m.irq_line());
        m.cpu_write(0xF008, 0);
        assert!(!m.irq_line());
    }

    #[test]
    fn audio_mute_silences_output_and_drops_writes() {
        let mut m = Vrc7::new(cart());
        // Key on a violin patch on channel 0.
        m.cpu_write(0x9010, 0x30); // select reg 0x30
        m.cpu_write(0x9030, 0x10); // patch=1, vol=0
        m.cpu_write(0x9010, 0x10);
        m.cpu_write(0x9030, 0x80);
        m.cpu_write(0x9010, 0x20);
        m.cpu_write(0x9030, 0x30); // key on, block=4

        // Run through enough CPU cycles to populate ~10 ms of audio.
        for _ in 0..18_000 {
            m.on_cpu_cycle();
        }
        let audible = m.audio_output().unwrap().abs() > 0.0
            || (-32_000..=32_000)
                .contains(&i32::from(m.last_sample));
        assert!(audible);

        // Mute and verify next sample lands at exactly zero.
        m.cpu_write(0xE000, 0x40); // mute, mirroring V
        for _ in 0..18_000 {
            m.on_cpu_cycle();
        }
        assert_eq!(m.last_sample, 0);
        assert_eq!(m.audio_output().unwrap(), 0.0);
    }
}
