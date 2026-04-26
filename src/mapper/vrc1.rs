// SPDX-License-Identifier: GPL-3.0-or-later
//! Konami VRC1 - iNES mapper 75.
//!
//! The smallest member of the VRC family - no IRQ, no expansion
//! audio, no PRG-RAM. Used in *Tetsuwan Atom*, *Ganbare Goemon!*,
//! *King Kong 2: Ikari no Megaton Punch*, and *Exciting Boxing*
//! (whose Famicom-Power-Pad peripheral is unrelated to the chip).
//!
//! The interesting wrinkle is the **CHR bank's high bit**. VRC1
//! exposes 4 KiB CHR banks via `$E000` (low slot) and `$F000` (high
//! slot), but each register only carries the bottom 4 bits. The 5th
//! bit (the A4 line of the 5-bit CHR bank index) lives in `$9000`,
//! interleaved with the mirroring bit. So a full CHR bank update
//! requires two writes: one to `$E000`/`$F000` for the low nibble
//! and one to `$9000` for the new top bit. Most games either keep
//! the high bit constant (CHR ROM <= 128 KiB, where bit 4 is
//! always 0) or set it once at boot.
//!
//! ## Register surface (`addr & 0xF000`)
//!
//! | Address  | Effect                                                    |
//! |----------|-----------------------------------------------------------|
//! | `$8000`  | PRG bank 0 (`$8000-$9FFF`)                                |
//! | `$9000`  | bit 0 = mirroring (0=V/1=H); bit 1 = CHR slot 0 high bit; bit 2 = CHR slot 1 high bit |
//! | `$A000`  | PRG bank 1 (`$A000-$BFFF`)                                |
//! | `$C000`  | PRG bank 2 (`$C000-$DFFF`)                                |
//! | `$E000`  | CHR slot 0 low 4 bits (4 KiB at `$0000-$0FFF`)            |
//! | `$F000`  | CHR slot 1 low 4 bits (4 KiB at `$1000-$1FFF`)            |
//!
//! `$E000` is hardwired to the last 8 KiB PRG bank.
//!
//! ## Four-screen quirk
//!
//! The Vs. System variant of VRC1 was wired for 4-screen VRAM. When
//! the cart header declares `Mirroring::FourScreen` we ignore the
//! `$9000.b0` mirroring bit (Mesen2's "the mirroring bit is ignored
//! if the cartridge is wired for 4-screen VRAM" comment). All known
//! retail Famicom VRC1 carts ship V/H-switchable, so this branch is
//! exercised only on Vs. System dumps.
//!
//! ## References
//!
//! Wiki: <https://www.nesdev.org/wiki/VRC1>. Ported from
//! `~/Git/Mesen2/Core/NES/Mappers/Konami/VRC1.h`, cross-checked
//! against `~/Git/punes/src/core/mappers/mapper_075.c` and
//! `~/Git/nestopia/source/core/board/NstBoardKonamiVrc1.cpp`. All
//! three converge on the same banking model; this implementation
//! adds an `allow_oversized_prg = true` default since the only
//! commercial cart that exceeds 128 KiB PRG (none, in practice) is
//! still better served by treating the PRG-bank value as a full
//! byte rather than masking to 4 bits.

use crate::mapper::Mapper;
use crate::rom::{Cartridge, Mirroring};

const PRG_BANK_8K: usize = 8 * 1024;
const CHR_BANK_4K: usize = 4 * 1024;

pub struct Vrc1 {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,

    /// Three switchable 8 KiB PRG banks for `$8000`/`$A000`/`$C000`.
    /// `$E000` is hardwired to the last bank.
    prg_banks: [u8; 3],
    /// Two CHR banks. Each is a 5-bit value built from the low 4
    /// bits stored in `$E000`/`$F000` plus the top bit from `$9000`.
    chr_banks: [u8; 2],

    prg_bank_count_8k: usize,
    chr_bank_count_4k: usize,

    /// Active mirroring. Honors the cart's hardwired 4-screen
    /// declaration; otherwise toggles between V/H based on `$9000.b0`.
    mirroring: Mirroring,
    /// Set when the cart was wired for 4-screen VRAM (Vs. System).
    /// While true, `$9000.b0` writes don't touch [`Self::mirroring`].
    four_screen_locked: bool,
}

impl Vrc1 {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count_8k = (cart.prg_rom.len() / PRG_BANK_8K).max(1);

        let chr_ram = cart.chr_ram || cart.chr_rom.is_empty();
        let chr = if chr_ram {
            // VRC1 doesn't ship CHR-RAM on commercial carts, but
            // honor the header for the homebrew that does.
            vec![0u8; 8 * 1024]
        } else {
            cart.chr_rom
        };
        let chr_bank_count_4k = (chr.len() / CHR_BANK_4K).max(1);

        let four_screen_locked = matches!(cart.mirroring, Mirroring::FourScreen);

        Self {
            prg_rom: cart.prg_rom,
            chr,
            chr_ram,
            prg_banks: [0, 0, 0],
            chr_banks: [0, 0],
            prg_bank_count_8k,
            chr_bank_count_4k,
            mirroring: cart.mirroring,
            four_screen_locked,
        }
    }

    fn prg_offset(&self, slot: usize, addr_in_slot: u16) -> usize {
        let bank = if slot == 3 {
            self.prg_bank_count_8k.saturating_sub(1)
        } else {
            (self.prg_banks[slot] as usize) % self.prg_bank_count_8k
        };
        bank * PRG_BANK_8K + addr_in_slot as usize
    }

    fn chr_offset(&self, slot: usize, addr_in_slot: u16) -> usize {
        let bank = (self.chr_banks[slot] as usize) % self.chr_bank_count_4k;
        bank * CHR_BANK_4K + addr_in_slot as usize
    }
}

impl Mapper for Vrc1 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
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
        if addr < 0x8000 {
            return;
        }
        match addr & 0xF000 {
            0x8000 => self.prg_banks[0] = data,
            0xA000 => self.prg_banks[1] = data,
            0xC000 => self.prg_banks[2] = data,

            0x9000 => {
                if !self.four_screen_locked {
                    self.mirroring = if (data & 0x01) != 0 {
                        Mirroring::Horizontal
                    } else {
                        Mirroring::Vertical
                    };
                }
                // Bits 1/2 carry the CHR bank A4 line for slots 0/1.
                // Preserve the low 4 bits already stored from prior
                // `$E000`/`$F000` writes.
                self.chr_banks[0] = (self.chr_banks[0] & 0x0F) | ((data & 0x02) << 3);
                self.chr_banks[1] = (self.chr_banks[1] & 0x0F) | ((data & 0x04) << 2);
            }

            0xE000 => {
                self.chr_banks[0] = (self.chr_banks[0] & 0x10) | (data & 0x0F);
            }
            0xF000 => {
                self.chr_banks[1] = (self.chr_banks[1] & 0x10) | (data & 0x0F);
            }
            _ => {}
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr >= 0x2000 {
            return 0;
        }
        let slot = if addr < 0x1000 { 0 } else { 1 };
        let in_slot = addr & 0x0FFF;
        let off = self.chr_offset(slot, in_slot);
        *self.chr.get(off).unwrap_or(&0)
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if !self.chr_ram || addr >= 0x2000 {
            return;
        }
        let slot = if addr < 0x1000 { 0 } else { 1 };
        let in_slot = addr & 0x0FFF;
        let off = self.chr_offset(slot, in_slot);
        if let Some(b) = self.chr.get_mut(off) {
            *b = data;
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rom::{Cartridge, Mirroring, TvSystem};

    /// 128 KiB PRG (16 banks of 8 KiB), 128 KiB CHR (32 banks of 4 KiB).
    /// PRG bank N tagged with byte N at every offset; CHR bank N
    /// tagged with byte N. So `cpu_peek` / `ppu_read` reveal the
    /// resolved bank index directly.
    fn cart() -> Cartridge {
        let prg_banks = 16;
        let mut prg = vec![0u8; prg_banks * PRG_BANK_8K];
        for bank in 0..prg_banks {
            let base = bank * PRG_BANK_8K;
            prg[base..base + PRG_BANK_8K].fill(bank as u8);
        }
        let chr_banks = 32;
        let mut chr = vec![0u8; chr_banks * CHR_BANK_4K];
        for bank in 0..chr_banks {
            let base = bank * CHR_BANK_4K;
            chr[base..base + CHR_BANK_4K].fill(bank as u8);
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: chr,
            chr_ram: false,
            mapper_id: 75,
            submapper: 0,
            mirroring: Mirroring::Vertical,
            battery_backed: false,
            prg_ram_size: 0,
            prg_nvram_size: 0,
            tv_system: TvSystem::Ntsc,
            is_nes2: false,
            prg_chr_crc32: 0,
            db_matched: false,
            fds_data: None,
        }
    }

    #[test]
    fn power_on_layout_pins_last_8k_at_e000() {
        let m = Vrc1::new(cart());
        assert_eq!(m.cpu_peek(0x8000), 0); // PRG slot 0 default = bank 0
        assert_eq!(m.cpu_peek(0xA000), 0);
        assert_eq!(m.cpu_peek(0xC000), 0);
        assert_eq!(m.cpu_peek(0xE000), 15); // last bank
        assert_eq!(m.cpu_peek(0xFFFF), 15);
    }

    #[test]
    fn three_prg_banks_independently_switchable() {
        let mut m = Vrc1::new(cart());
        m.cpu_write(0x8000, 5);
        m.cpu_write(0xA000, 10);
        m.cpu_write(0xC000, 14);
        assert_eq!(m.cpu_peek(0x8000), 5);
        assert_eq!(m.cpu_peek(0xA000), 10);
        assert_eq!(m.cpu_peek(0xC000), 14);
        assert_eq!(m.cpu_peek(0xE000), 15);
    }

    #[test]
    fn chr_4k_banks_select_via_e000_and_f000() {
        let mut m = Vrc1::new(cart());
        m.cpu_write(0xE000, 0x05);
        m.cpu_write(0xF000, 0x09);
        assert_eq!(m.ppu_read(0x0000), 0x05);
        assert_eq!(m.ppu_read(0x0FFF), 0x05);
        assert_eq!(m.ppu_read(0x1000), 0x09);
        assert_eq!(m.ppu_read(0x1FFF), 0x09);
    }

    #[test]
    fn nine_thousand_bit_one_supplies_chr_slot0_high_bit() {
        let mut m = Vrc1::new(cart());
        // Slot 0 low nibble 0x05; combined with $9000.b1 → 0x15.
        m.cpu_write(0xE000, 0x05);
        m.cpu_write(0x9000, 0x02);
        assert_eq!(m.ppu_read(0x0000), 0x15);
        // Slot 1 untouched (still bank 0).
        assert_eq!(m.ppu_read(0x1000), 0x00);
    }

    #[test]
    fn nine_thousand_bit_two_supplies_chr_slot1_high_bit() {
        let mut m = Vrc1::new(cart());
        m.cpu_write(0xF000, 0x07);
        m.cpu_write(0x9000, 0x04);
        assert_eq!(m.ppu_read(0x1000), 0x17);
        assert_eq!(m.ppu_read(0x0000), 0x00);
    }

    #[test]
    fn nine_thousand_high_bit_persists_across_low_nibble_writes() {
        let mut m = Vrc1::new(cart());
        // Set slot 0 bit 4 once.
        m.cpu_write(0x9000, 0x02);
        // Then update only the low nibble.
        m.cpu_write(0xE000, 0x03);
        assert_eq!(m.ppu_read(0x0000), 0x13);
        m.cpu_write(0xE000, 0x0A);
        assert_eq!(m.ppu_read(0x0000), 0x1A);
    }

    #[test]
    fn nine_thousand_bit_zero_decodes_mirroring() {
        let mut m = Vrc1::new(cart());
        m.cpu_write(0x9000, 0x00);
        assert_eq!(m.mirroring(), Mirroring::Vertical);
        m.cpu_write(0x9000, 0x01);
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
        // Setting CHR high bits doesn't touch mirroring.
        m.cpu_write(0x9000, 0x06); // bits 1+2, bit 0 = 0
        assert_eq!(m.mirroring(), Mirroring::Vertical);
    }

    #[test]
    fn four_screen_locks_out_mirroring_writes() {
        let mut c = cart();
        c.mirroring = Mirroring::FourScreen;
        let mut m = Vrc1::new(c);
        // Even toggling $9000.b0 must leave mirroring at FourScreen.
        m.cpu_write(0x9000, 0x01);
        assert_eq!(m.mirroring(), Mirroring::FourScreen);
        // CHR high bits still apply normally.
        m.cpu_write(0xE000, 0x05);
        m.cpu_write(0x9000, 0x02);
        assert_eq!(m.ppu_read(0x0000), 0x15);
    }

    #[test]
    fn writes_below_8000_are_dropped() {
        let mut m = Vrc1::new(cart());
        m.cpu_write(0x6000, 0xFF); // no PRG-RAM
        m.cpu_write(0x4020, 0xFF);
        // Initial state intact.
        assert_eq!(m.cpu_peek(0x8000), 0);
        assert_eq!(m.ppu_read(0x0000), 0);
    }
}
