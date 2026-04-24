//! GxROM / MHROM (mapper 66).
//!
//! One-register discrete logic board. A single 8-bit write to any
//! address in `$8000-$FFFF` latches two bank selects packed into the
//! same byte:
//!
//! ```text
//! 7  bit  0
//! ---- ----
//! xxPP xxCC
//!   ||   ||
//!   ||   ++- 8 KB CHR ROM bank  at PPU $0000-$1FFF
//!   ++------ 32 KB PRG ROM bank at CPU $8000-$FFFF
//! ```
//!
//! * PRG window is fixed at 32 KB — up to 4 banks (128 KB).
//! * CHR window is fixed at 8 KB  — up to 4 banks (32 KB).
//! * Mirroring is solder-set from the iNES header (H or V).
//! * No PRG-RAM, no battery, no IRQ, no bus-access side effects.
//!
//! Real boards have a bus conflict on the write (value ANDed with the
//! ROM byte at the target address), but every shipping game writes a
//! value that already matches the ROM byte, so Mesen2 and puNES both
//! skip the AND — we do too, matching their behavior.
//!
//! Common GxROM titles: Super Mario Bros. + Duck Hunt multicart,
//! Dragon Power, Thunder & Lightning, Arkanoid (Japan).
//!
//! References: nesdev.org/wiki/GxROM, Mesen2
//! `Core/NES/Mappers/Nintendo/GxRom.h`, puNES
//! `src/core/mappers/mapper_066.c`, Nestopia
//! `source/core/board/NstBoardGxRom.cpp`.

use crate::mapper::Mapper;
use crate::rom::{Cartridge, Mirroring};

const PRG_BANK_32K: usize = 32 * 1024;
const CHR_BANK_8K: usize = 8 * 1024;

pub struct Gxrom {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,
    mirroring: Mirroring,
    prg_bank: u8,
    chr_bank: u8,
    prg_bank_count: usize,
    chr_bank_count: usize,
}

impl Gxrom {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count = (cart.prg_rom.len() / PRG_BANK_32K).max(1);

        let is_chr_ram = cart.chr_ram || cart.chr_rom.is_empty();
        let (chr, chr_bank_count) = if is_chr_ram {
            (vec![0u8; CHR_BANK_8K], 1)
        } else {
            let count = (cart.chr_rom.len() / CHR_BANK_8K).max(1);
            (cart.chr_rom, count)
        };

        Self {
            prg_rom: cart.prg_rom,
            chr,
            chr_ram: is_chr_ram,
            mirroring: cart.mirroring,
            prg_bank: 0,
            chr_bank: 0,
            prg_bank_count,
            chr_bank_count,
        }
    }

    fn prg_index(&self, addr: u16) -> usize {
        let bank = (self.prg_bank as usize) % self.prg_bank_count;
        bank * PRG_BANK_32K + (addr - 0x8000) as usize
    }

    fn chr_index(&self, addr: u16) -> usize {
        let bank = (self.chr_bank as usize) % self.chr_bank_count;
        bank * CHR_BANK_8K + addr as usize
    }
}

impl Mapper for Gxrom {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        if (0x8000..=0xFFFF).contains(&addr) {
            // Mesen2: PRG = (value >> 4) & 0x03, CHR = value & 0x03.
            // We keep the raw 2-bit fields so debuggers can read them back.
            self.prg_bank = (data >> 4) & 0x03;
            self.chr_bank = data & 0x03;
        }
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
            0x8000..=0xFFFF => {
                let i = self.prg_index(addr);
                *self.prg_rom.get(i).unwrap_or(&0)
            }
            _ => 0,
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr < 0x2000 {
            let i = self.chr_index(addr);
            *self.chr.get(i).unwrap_or(&0)
        } else {
            0
        }
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if self.chr_ram && addr < 0x2000 {
            let i = self.chr_index(addr);
            if let Some(slot) = self.chr.get_mut(i) {
                *slot = data;
            }
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

    /// 128 KB PRG (4 × 32 KB banks) + 32 KB CHR-ROM (4 × 8 KB banks).
    /// Each PRG bank N is filled with byte `0x80 + N`; each CHR bank N
    /// is filled with byte `0x10 + N`. Makes bank-switch failures easy
    /// to read in assertion output.
    fn cart_full() -> Cartridge {
        let mut prg = vec![0u8; 4 * PRG_BANK_32K];
        for bank in 0..4 {
            let base = bank * PRG_BANK_32K;
            prg[base..base + PRG_BANK_32K].fill(0x80 + bank as u8);
        }
        let mut chr = vec![0u8; 4 * CHR_BANK_8K];
        for bank in 0..4 {
            let base = bank * CHR_BANK_8K;
            chr[base..base + CHR_BANK_8K].fill(0x10 + bank as u8);
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: chr,
            chr_ram: false,
            mapper_id: 66,
            submapper: 0,
            mirroring: Mirroring::Horizontal,
            battery_backed: false,
            prg_ram_size: 0,
            prg_nvram_size: 0,
            tv_system: TvSystem::Ntsc,
            is_nes2: false,
            prg_chr_crc32: 0,
            db_matched: false,
        }
    }

    #[test]
    fn power_on_selects_bank_zero_for_prg_and_chr() {
        let m = Gxrom::new(cart_full());
        assert_eq!(m.cpu_peek(0x8000), 0x80);
        assert_eq!(m.cpu_peek(0xFFFF), 0x80);
        // PPU bank 0 tag
        let mut m = m;
        assert_eq!(m.ppu_read(0x0000), 0x10);
        assert_eq!(m.ppu_read(0x1FFF), 0x10);
    }

    #[test]
    fn prg_bank_comes_from_bits_4_5() {
        let mut m = Gxrom::new(cart_full());
        // PRG = 2, CHR = 0
        m.cpu_write(0x8000, 0b0010_0000);
        assert_eq!(m.cpu_peek(0x8000), 0x82);
        assert_eq!(m.cpu_peek(0xFFFF), 0x82);
        // CHR untouched
        assert_eq!(m.ppu_read(0x0000), 0x10);
    }

    #[test]
    fn chr_bank_comes_from_bits_0_1() {
        let mut m = Gxrom::new(cart_full());
        // PRG = 0, CHR = 3
        m.cpu_write(0xBEEF, 0b0000_0011);
        assert_eq!(m.ppu_read(0x0000), 0x13);
        assert_eq!(m.ppu_read(0x1FFF), 0x13);
        // PRG untouched
        assert_eq!(m.cpu_peek(0x8000), 0x80);
    }

    #[test]
    fn prg_and_chr_banks_decode_independently() {
        let mut m = Gxrom::new(cart_full());
        // PRG = 3, CHR = 1, stray bits 2/3/6/7 set → must be ignored
        m.cpu_write(0x8000, 0b1111_1101);
        assert_eq!(m.cpu_peek(0x8000), 0x83);
        assert_eq!(m.ppu_read(0x0000), 0x11);
    }

    #[test]
    fn write_below_8000_is_ignored() {
        let mut m = Gxrom::new(cart_full());
        m.cpu_write(0x6000, 0b0011_0011);
        m.cpu_write(0x7FFF, 0b0011_0011);
        assert_eq!(m.cpu_peek(0x8000), 0x80);
        assert_eq!(m.ppu_read(0x0000), 0x10);
    }

    #[test]
    fn chr_ram_variant_is_writable() {
        let mut cart = cart_full();
        cart.chr_rom = vec![];
        cart.chr_ram = true;
        let mut m = Gxrom::new(cart);
        m.ppu_write(0x0555, 0xAA);
        m.ppu_write(0x1234, 0x77);
        assert_eq!(m.ppu_read(0x0555), 0xAA);
        assert_eq!(m.ppu_read(0x1234), 0x77);
    }

    #[test]
    fn prg_bank_selector_wraps_on_undersized_rom() {
        // 32 KB PRG = only 1 bank. Writing any PRG bank select must
        // wrap back to bank 0 instead of reading out of bounds.
        let mut cart = cart_full();
        cart.prg_rom = vec![0xAB; PRG_BANK_32K];
        let mut m = Gxrom::new(cart);
        m.cpu_write(0x8000, 0b0011_0000);
        assert_eq!(m.cpu_peek(0x8000), 0xAB);
        assert_eq!(m.cpu_peek(0xFFFF), 0xAB);
    }

    #[test]
    fn mirroring_is_taken_from_header_unchanged() {
        let mut cart = cart_full();
        cart.mirroring = Mirroring::Vertical;
        let mut m = Gxrom::new(cart);
        assert_eq!(m.mirroring(), Mirroring::Vertical);
        // Register writes must not affect mirroring.
        m.cpu_write(0x8000, 0xFF);
        assert_eq!(m.mirroring(), Mirroring::Vertical);
    }
}
