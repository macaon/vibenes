// SPDX-License-Identifier: GPL-3.0-or-later
//! Codemasters / Camerica BF9096 (iNES mapper 232).
//!
//! The Quattro multicart chip - bundles four games on a single 256
//! KiB cart by stacking an outer "block" select on top of an inner
//! 16 KiB bank select. Games:
//!
//! - **Submapper 0** (default): *Quattro Adventure*, *Quattro
//!   Arcade*, *Quattro Sports* (Camerica/Codemasters, 1991-92).
//! - **Submapper 1** (Aladdin Deck Enhancer): bit-swapped outer
//!   block select. The Aladdin pass-through cart re-routes the
//!   block bits because of how the daughterboard wires the host
//!   cart slot.
//!
//! ## Register surface
//!
//! ```text
//! $8000-$BFFF   outer 64 KiB block select (mode-dependent bits)
//! $C000-$FFFF   inner 16 KiB page select (low 2 bits)
//! ```
//!
//! After every write the chip recomputes both PRG slots:
//!
//! - `$8000-$BFFF` -> `(block << 2) | page`
//! - `$C000-$FFFF` -> `(block << 2) | 3`  (last 16 KiB of block)
//!
//! 8 KiB CHR-RAM, hardwired mirroring.
//!
//! ## Outer-block bit layout
//!
//! - Submapper 0: `block = (value >> 3) & 0x03` (bits 4 and 3 of the
//!   write end up as bits 1 and 0 of the block select).
//! - Submapper 1: bit-swapped - `block = ((value >> 4) & 0x01) |
//!   ((value >> 2) & 0x02)` (bit 4 is block bit 0, bit 3 is block
//!   bit 1).
//!
//! References:
//! - <https://www.nesdev.org/wiki/INES_Mapper_232>
//! - `~/Git/Mesen2/Core/NES/Mappers/Codemasters/BF9096.h`
//! - `~/Git/punes/src/core/mappers/mapper_232.c`

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_16K: usize = 16 * 1024;
const CHR_BANK_8K: usize = 8 * 1024;

pub struct CodemastersBf9096 {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,

    /// Outer 64 KiB block index (0..=3).
    prg_block: u8,
    /// Inner 16 KiB page index within the current block (0..=3).
    prg_page: u8,
    /// True for submapper 1 (Aladdin Deck Enhancer bit-swap).
    aladdin_mode: bool,

    mirroring: Mirroring,
    prg_bank_count_16k: usize,
}

impl CodemastersBf9096 {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count_16k = (cart.prg_rom.len() / PRG_BANK_16K).max(1);
        let is_chr_ram = cart.chr_ram || cart.chr_rom.is_empty();
        let chr = if is_chr_ram {
            vec![0u8; CHR_BANK_8K]
        } else {
            cart.chr_rom
        };
        Self {
            prg_rom: cart.prg_rom,
            chr,
            chr_ram: is_chr_ram,
            prg_block: 0,
            prg_page: 0,
            aladdin_mode: cart.submapper == 1,
            mirroring: cart.mirroring,
            prg_bank_count_16k,
        }
    }

    fn switch_bank_index(&self) -> usize {
        ((self.prg_block as usize) << 2) | (self.prg_page as usize)
    }

    fn fixed_bank_index(&self) -> usize {
        ((self.prg_block as usize) << 2) | 0x03
    }

    fn prg_byte(&self, bank: usize, off: usize) -> u8 {
        let base = (bank % self.prg_bank_count_16k) * PRG_BANK_16K;
        *self.prg_rom.get(base + off).unwrap_or(&0)
    }
}

impl Mapper for CodemastersBf9096 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
            0x8000..=0xBFFF => self.prg_byte(self.switch_bank_index(), (addr - 0x8000) as usize),
            0xC000..=0xFFFF => self.prg_byte(self.fixed_bank_index(), (addr - 0xC000) as usize),
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        match addr {
            0x8000..=0xBFFF => {
                self.prg_block = if self.aladdin_mode {
                    ((data >> 4) & 0x01) | ((data >> 2) & 0x02)
                } else {
                    (data >> 3) & 0x03
                };
            }
            0xC000..=0xFFFF => {
                self.prg_page = data & 0x03;
            }
            _ => {}
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

    fn save_state_capture(&self) -> Option<crate::save_state::MapperState> {
        use crate::save_state::mapper::CodemastersBf9096Snap;
        Some(crate::save_state::MapperState::CodemastersBf9096(
            CodemastersBf9096Snap {
                chr_ram_data: if self.chr_ram { self.chr.clone() } else { Vec::new() },
                prg_block: self.prg_block,
                prg_page: self.prg_page,
                aladdin_mode: self.aladdin_mode,
            },
        ))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::CodemastersBf9096(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        if snap.aladdin_mode != self.aladdin_mode {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        }
        if self.chr_ram && snap.chr_ram_data.len() == self.chr.len() {
            self.chr.copy_from_slice(&snap.chr_ram_data);
        }
        self.prg_block = snap.prg_block;
        self.prg_page = snap.prg_page;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, Mirroring, TvSystem};

    /// 256 KiB PRG (16 banks of 16 KiB), CHR-RAM. Tag every PRG
    /// bank's first byte with its own index so we can read off
    /// which bank is mapped where.
    fn cart(submapper: u8) -> Cartridge {
        let mut prg = vec![0xFFu8; 16 * PRG_BANK_16K];
        for bank in 0..16 {
            prg[bank * PRG_BANK_16K] = bank as u8;
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: Vec::new(),
            chr_ram: true,
            mapper_id: 232,
            submapper,
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
    fn boot_state_block0_page0() {
        let m = CodemastersBf9096::new(cart(0));
        assert_eq!(m.cpu_peek(0x8000), 0); // block 0, page 0
        assert_eq!(m.cpu_peek(0xC000), 3); // block 0, last page
    }

    #[test]
    fn page_select_at_c000() {
        let mut m = CodemastersBf9096::new(cart(0));
        // page = 2, block stays 0 -> bank 2 at $8000.
        m.cpu_write(0xC000, 0x02);
        assert_eq!(m.cpu_peek(0x8000), 2);
        // High slot still locked to last bank of current block.
        assert_eq!(m.cpu_peek(0xC000), 3);
        // Only low 2 bits of the page write count.
        m.cpu_write(0xFFFF, 0xFF);
        assert_eq!(m.cpu_peek(0x8000), 3);
    }

    #[test]
    fn quattro_block_select_at_8000() {
        let mut m = CodemastersBf9096::new(cart(0));
        // bits 4-3 = 0b10 -> block 2 -> banks 8..=11 visible.
        m.cpu_write(0x8000, 0b0001_0000); // bit 4 set
        assert_eq!(m.cpu_peek(0x8000), 8); // block 2 page 0
        assert_eq!(m.cpu_peek(0xC000), 11); // block 2 last
        // Pick page within the block.
        m.cpu_write(0xC000, 0x01);
        assert_eq!(m.cpu_peek(0x8000), 9); // block 2 page 1
        // Block 3 (bits 4 and 3 both set).
        m.cpu_write(0xBFFF, 0b0001_1000);
        m.cpu_write(0xC000, 0x02);
        assert_eq!(m.cpu_peek(0x8000), 14); // block 3 page 2
        assert_eq!(m.cpu_peek(0xC000), 15); // block 3 last
    }

    #[test]
    fn aladdin_block_select_swaps_bits() {
        let mut m = CodemastersBf9096::new(cart(1));
        // Submapper 1: bit 4 = block bit 0, bit 3 = block bit 1.
        // Value 0b0000_1000 -> block bit 1 set -> block = 2.
        m.cpu_write(0x8000, 0b0000_1000);
        assert_eq!(m.cpu_peek(0x8000), 8);
        // Value 0b0001_0000 -> block bit 0 set -> block = 1.
        m.cpu_write(0x8000, 0b0001_0000);
        assert_eq!(m.cpu_peek(0x8000), 4);
        // Value 0b0001_1000 -> both -> block 3.
        m.cpu_write(0x8000, 0b0001_1000);
        assert_eq!(m.cpu_peek(0x8000), 12);
    }

    #[test]
    fn chr_ram_round_trip() {
        let mut m = CodemastersBf9096::new(cart(0));
        m.ppu_write(0x0042, 0xCD);
        assert_eq!(m.ppu_read(0x0042), 0xCD);
    }

    #[test]
    fn save_state_rejects_cross_submapper_apply() {
        let m = CodemastersBf9096::new(cart(0));
        let snap = m.save_state_capture().unwrap();
        let mut alad = CodemastersBf9096::new(cart(1));
        match alad.save_state_apply(&snap) {
            Err(crate::save_state::SaveStateError::UnsupportedMapper(_)) => {}
            other => panic!("expected UnsupportedMapper, got {other:?}"),
        }
    }

    #[test]
    fn save_state_round_trip_preserves_block_and_page() {
        let mut m = CodemastersBf9096::new(cart(0));
        m.cpu_write(0x8000, 0b0001_1000); // block 3
        m.cpu_write(0xC000, 0x02); // page 2
        let snap = m.save_state_capture().unwrap();
        let mut fresh = CodemastersBf9096::new(cart(0));
        fresh.save_state_apply(&snap).unwrap();
        assert_eq!(fresh.cpu_peek(0x8000), 14);
        assert_eq!(fresh.cpu_peek(0xC000), 15);
    }
}
