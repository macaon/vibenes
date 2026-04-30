// SPDX-License-Identifier: GPL-3.0-or-later
//! Jaleco JF-11 / JF-14 (iNES mapper 140).
//!
//! Tiny PCB: 32 KiB-switchable PRG at `$8000-$FFFF` plus 8 KiB
//! CHR-ROM banking, hardwired mirroring, no audio. The same chip
//! family as the JF-13 (mapper 86) minus the speech engine, so
//! the bank-select layout is also a strict simplification of
//! mapper 86.
//!
//! ## Register surface (single latch at `$6000-$7FFF`)
//!
//! ```text
//! $6000-$7FFF   PPPP CCCC -> CHR bank in low nibble,
//!                            PRG bank in bits 5-4 (high nibble masked).
//! ```
//!
//! Two PRG bits = 4 banks of 32 KiB (128 KiB max), four CHR bits
//! = 16 banks of 8 KiB (128 KiB CHR-ROM max).
//!
//! Games (licensed Jaleco): *Bio Senshi Dan: Increaser tono Tatakai*,
//! *Mindseeker*, *Penguin Kun Wars 2*, *Doraemon* (Famicom),
//! *Youkai Club*, *Captain Saver* JP and a handful of other
//! single-cart Jaleco titles.
//!
//! References:
//! - <https://www.nesdev.org/wiki/INES_Mapper_140>
//! - `~/Git/Mesen2/Core/NES/Mappers/Jaleco/JalecoJf11_14.h`
//! - `~/Git/punes/src/core/mappers/mapper_140.c`

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_32K: usize = 32 * 1024;
const CHR_BANK_8K: usize = 8 * 1024;

pub struct JalecoJf11_14 {
    prg_rom: Vec<u8>,
    chr_rom: Vec<u8>,
    chr_ram: bool,

    /// Latched value at `$6000-$7FFF`.
    reg: u8,

    mirroring: Mirroring,
    prg_bank_count_32k: usize,
    chr_bank_count_8k: usize,
}

impl JalecoJf11_14 {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count_32k = (cart.prg_rom.len() / PRG_BANK_32K).max(1);
        let is_chr_ram = cart.chr_ram || cart.chr_rom.is_empty();
        let chr = if is_chr_ram {
            vec![0u8; CHR_BANK_8K]
        } else {
            cart.chr_rom
        };
        let chr_bank_count_8k = (chr.len() / CHR_BANK_8K).max(1);
        Self {
            prg_rom: cart.prg_rom,
            chr_rom: chr,
            chr_ram: is_chr_ram,
            reg: 0,
            mirroring: cart.mirroring,
            prg_bank_count_32k,
            chr_bank_count_8k,
        }
    }

    fn prg_bank(&self) -> usize {
        let bank = ((self.reg >> 4) & 0x03) as usize;
        bank % self.prg_bank_count_32k
    }

    fn chr_bank(&self) -> usize {
        let bank = (self.reg & 0x0F) as usize;
        bank % self.chr_bank_count_8k
    }
}

impl Mapper for JalecoJf11_14 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        if addr >= 0x8000 {
            let off = (addr - 0x8000) as usize;
            let base = self.prg_bank() * PRG_BANK_32K;
            *self.prg_rom.get(base + off).unwrap_or(&0)
        } else {
            0
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        if (0x6000..=0x7FFF).contains(&addr) {
            self.reg = data;
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr < 0x2000 {
            let base = self.chr_bank() * CHR_BANK_8K;
            *self.chr_rom.get(base + addr as usize).unwrap_or(&0)
        } else {
            0
        }
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if self.chr_ram && addr < 0x2000 {
            let base = self.chr_bank() * CHR_BANK_8K;
            if let Some(slot) = self.chr_rom.get_mut(base + addr as usize) {
                *slot = data;
            }
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }

    fn save_state_capture(&self) -> Option<crate::save_state::MapperState> {
        use crate::save_state::mapper::JalecoJf11_14Snap;
        Some(crate::save_state::MapperState::JalecoJf11_14(
            JalecoJf11_14Snap {
                chr_ram_data: if self.chr_ram { self.chr_rom.clone() } else { Vec::new() },
                reg: self.reg,
            },
        ))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::JalecoJf11_14(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        if self.chr_ram && snap.chr_ram_data.len() == self.chr_rom.len() {
            self.chr_rom.copy_from_slice(&snap.chr_ram_data);
        }
        self.reg = snap.reg;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, Mirroring, TvSystem};

    /// 128 KiB PRG (4 banks of 32 KiB) + 128 KiB CHR (16 banks
    /// of 8 KiB). Tag every bank's first byte with its index.
    fn cart() -> Cartridge {
        let mut prg = vec![0xFFu8; 4 * PRG_BANK_32K];
        for bank in 0..4 {
            prg[bank * PRG_BANK_32K] = bank as u8;
        }
        let mut chr = vec![0xFFu8; 16 * CHR_BANK_8K];
        for bank in 0..16 {
            chr[bank * CHR_BANK_8K] = (0x20 + bank) as u8;
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: chr,
            chr_ram: false,
            mapper_id: 140,
            submapper: 0,
            mirroring: Mirroring::Vertical,
            battery_backed: false,
            prg_ram_size: 0,
            prg_nvram_size: 0,
            tv_system: TvSystem::Ntsc,
            is_nes2: true,
            prg_chr_crc32: 0,
            db_matched: false,
            fds_data: None,
        }
    }

    #[test]
    fn boot_state_zeroes_both_banks() {
        let mut m = JalecoJf11_14::new(cart());
        assert_eq!(m.cpu_peek(0x8000), 0);
        assert_eq!(m.ppu_read(0x0000), 0x20);
    }

    #[test]
    fn prg_bank_lives_in_high_nibble() {
        let mut m = JalecoJf11_14::new(cart());
        m.cpu_write(0x6000, 0b0010_0000); // bits 5-4 = 10 -> bank 2
        assert_eq!(m.cpu_peek(0x8000), 2);
        // Bits 7-6 should be ignored.
        m.cpu_write(0x6000, 0b1101_0000); // bits 5-4 = 01 -> bank 1
        assert_eq!(m.cpu_peek(0x8000), 1);
    }

    #[test]
    fn chr_bank_lives_in_low_nibble() {
        let mut m = JalecoJf11_14::new(cart());
        m.cpu_write(0x6000, 0x0F); // CHR 15
        assert_eq!(m.ppu_read(0x0000), 0x2F);
        m.cpu_write(0x6000, 0x05); // CHR 5
        assert_eq!(m.ppu_read(0x0000), 0x25);
    }

    #[test]
    fn full_byte_carries_both_banks() {
        let mut m = JalecoJf11_14::new(cart());
        // bits 5-4 = 11 -> PRG 3; low nibble = 0xA -> CHR 10.
        m.cpu_write(0x6000, 0b0011_1010);
        assert_eq!(m.cpu_peek(0x8000), 3);
        assert_eq!(m.ppu_read(0x0000), 0x2A);
    }

    #[test]
    fn writes_outside_register_window_are_ignored() {
        let mut m = JalecoJf11_14::new(cart());
        m.cpu_write(0x5FFF, 0x33);
        m.cpu_write(0x8000, 0x33);
        assert_eq!(m.cpu_peek(0x8000), 0);
    }

    #[test]
    fn save_state_round_trip() {
        let mut m = JalecoJf11_14::new(cart());
        m.cpu_write(0x7FFF, 0b0010_0111);
        let snap = m.save_state_capture().unwrap();
        let mut fresh = JalecoJf11_14::new(cart());
        fresh.save_state_apply(&snap).unwrap();
        assert_eq!(fresh.cpu_peek(0x8000), 2);
        assert_eq!(fresh.ppu_read(0x0000), 0x27);
    }
}
