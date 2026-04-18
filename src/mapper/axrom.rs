//! AxROM (mapper 7).
//!
//! One-register mapper with two coupled controls in the same byte:
//! - PRG: a 32KB window at `$8000-$FFFF` selects one of up to 8 banks
//!   (128KB, sufficient for the common AxROM titles; some later boards
//!   extend to 16 banks = 256KB).
//! - Mirroring: the cart hardwires single-screen, and the *bit 4* of the
//!   bank-select write picks which nametable the PPU sees — clear = lower
//!   ($2000), set = upper ($2400). This is what's known as "one-screen
//!   mirroring" and distinguishes AxROM from most other mappers.
//!
//! CHR is 8KB of CHR-RAM (no CHR-ROM variants).
//!
//! Common AxROM games: Battletoads, Marble Madness, RC Pro-Am,
//! Solstice, Ultimate Air Combat.

use crate::mapper::Mapper;
use crate::rom::{Cartridge, Mirroring};

const PRG_BANK_32K: usize = 32 * 1024;
const CHR_BANK_8K: usize = 8 * 1024;

pub struct Axrom {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,
    bank: u8,
    bank_mask: u8,
    mirroring: Mirroring,
}

impl Axrom {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count = (cart.prg_rom.len() / PRG_BANK_32K).max(1);
        debug_assert!(prg_bank_count.is_power_of_two());
        let bank_mask = (prg_bank_count as u8).wrapping_sub(1);

        let is_chr_ram = cart.chr_ram || cart.chr_rom.is_empty();
        let chr = if is_chr_ram {
            vec![0u8; CHR_BANK_8K]
        } else {
            cart.chr_rom
        };

        // Power-on mirroring: nesdev says "the bit is indeterminate" but
        // most emulators default to single-screen lower. We do too.
        // Games always initialize this register early, so the startup
        // choice rarely matters.
        Self {
            prg_rom: cart.prg_rom,
            chr,
            chr_ram: is_chr_ram,
            bank: 0,
            bank_mask,
            mirroring: Mirroring::SingleScreenLower,
        }
    }

    fn bank_base(&self) -> usize {
        ((self.bank & self.bank_mask) as usize) * PRG_BANK_32K
    }
}

impl Mapper for Axrom {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        if (0x8000..=0xFFFF).contains(&addr) {
            // Low bits select the PRG bank; bit 4 picks the nametable
            // (single-screen lower vs upper).
            self.bank = data & self.bank_mask;
            self.mirroring = if data & 0x10 != 0 {
                Mirroring::SingleScreenUpper
            } else {
                Mirroring::SingleScreenLower
            };
        }
        // No PRG-RAM on AxROM — writes to $6000-$7FFF are open bus.
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
            0x8000..=0xFFFF => {
                let i = self.bank_base() + (addr - 0x8000) as usize;
                *self.prg_rom.get(i).unwrap_or(&0)
            }
            _ => 0,
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr < 0x2000 {
            *self.chr.get(addr as usize).unwrap_or(&0)
        } else {
            0
        }
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if self.chr_ram && addr < 0x2000 {
            if let Some(slot) = self.chr.get_mut(addr as usize) {
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

    /// 128KB PRG (4 banks of 32KB), CHR-RAM. Each 32KB bank filled with
    /// a unique tag byte: bank N -> 0x20 + N.
    fn cart_128k_prg() -> Cartridge {
        let mut prg = vec![0u8; 4 * PRG_BANK_32K];
        for bank in 0..4 {
            let base = bank * PRG_BANK_32K;
            prg[base..base + PRG_BANK_32K].fill(0x20 + bank as u8);
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: vec![],
            chr_ram: true,
            mapper_id: 7,
            submapper: 0,
            // Header mirroring is ignored by AxROM at runtime, but we
            // still set Vertical here to verify we override it.
            mirroring: Mirroring::Vertical,
            battery_backed: false,
            prg_ram_size: 0,
            tv_system: TvSystem::Ntsc,
            is_nes2: false,
            prg_chr_crc32: 0,
            db_matched: false,
        }
    }

    #[test]
    fn default_is_bank_zero_single_screen_lower() {
        let m = Axrom::new(cart_128k_prg());
        assert_eq!(m.cpu_peek(0x8000), 0x20);
        assert_eq!(m.cpu_peek(0xFFFF), 0x20);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenLower);
    }

    #[test]
    fn bank_write_selects_prg_bank() {
        let mut m = Axrom::new(cart_128k_prg());
        m.cpu_write(0x8000, 0x02);
        assert_eq!(m.cpu_peek(0x8000), 0x22);
        assert_eq!(m.cpu_peek(0xFFFF), 0x22);
    }

    #[test]
    fn bit4_selects_mirroring() {
        let mut m = Axrom::new(cart_128k_prg());
        m.cpu_write(0x8000, 0x10);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenUpper);
        m.cpu_write(0xBEEF, 0x00);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenLower);
    }

    #[test]
    fn bank_and_mirror_decode_independently() {
        let mut m = Axrom::new(cart_128k_prg());
        // bank=3, mirror=upper
        m.cpu_write(0x8000, 0x13);
        assert_eq!(m.cpu_peek(0x8000), 0x23);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenUpper);
    }

    #[test]
    fn chr_ram_roundtrip() {
        let mut m = Axrom::new(cart_128k_prg());
        m.ppu_write(0x0555, 0xAA);
        assert_eq!(m.ppu_read(0x0555), 0xAA);
        m.ppu_write(0x1234, 0x77);
        assert_eq!(m.ppu_read(0x1234), 0x77);
    }
}
