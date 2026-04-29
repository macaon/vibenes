// SPDX-License-Identifier: GPL-3.0-or-later
//! Sunsoft-2 (with single-screen mirror control) - iNES mapper 89.
//!
//! Sunsoft's discrete `R7F` chip family. Mapper 89 is the variant
//! with on-cart single-screen mirroring control; the related
//! mapper 93 wires the same register set without mirroring control
//! (and CHR-RAM-only) and is intentionally not handled here. The
//! one licensed retail cart on mapper 89 is *Tenka no Goikenban:
//! Mito Koumon* (Sunsoft, 1987 Famicom).
//!
//! Like the Bandai 74*161, the chip ships without internal data
//! drivers, so writes to `$8000-$FFFF` are subject to a real-
//! hardware **bus conflict**: the latched value is the
//! open-collector `data & rom_byte_at_addr`. puNES and Nestopia
//! model this; Mesen2 omits it. We follow puNES + Nestopia.
//!
//! ## Register surface
//!
//! ```text
//! 7  bit  0
//! ---- ----
//! CPPP MCCC
//! |||| ||||
//! |||| |+++- low 3 bits of 8 KiB CHR bank
//! |||| +---- mirroring (0 = single-screen lower, 1 = upper)
//! |+++------ 16 KiB PRG bank at CPU $8000-$BFFF (high fixed last)
//! +--------- bit 3 of 8 KiB CHR bank
//! ```
//!
//! So the CHR bank index is `((reg & 0x80) >> 4) | (reg & 0x07)`,
//! giving a 4-bit value covering 16 banks (128 KiB) of CHR-ROM.
//! `$C000-$FFFF` is hardwired to the last 16 KiB PRG bank.
//!
//! References:
//! - <https://www.nesdev.org/wiki/INES_Mapper_089>
//! - `~/Git/Mesen2/Core/NES/Mappers/Sunsoft/Sunsoft89.h`
//! - `~/Git/punes/src/core/mappers/mapper_089.c` (bus conflict)
//! - `~/Git/nestopia/source/core/board/NstBoardSunsoft2.cpp`
//!   (`S2b::Poke_8000`)

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_16K: usize = 16 * 1024;
const CHR_BANK_8K: usize = 8 * 1024;

pub struct Sunsoft2 {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,

    /// Latched register value (post bus-conflict AND).
    reg: u8,

    mirroring: Mirroring,

    prg_bank_count_16k: usize,
    chr_bank_count_8k: usize,
}

impl Sunsoft2 {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count_16k = (cart.prg_rom.len() / PRG_BANK_16K).max(1);

        let is_chr_ram = cart.chr_ram || cart.chr_rom.is_empty();
        let chr = if is_chr_ram {
            vec![0u8; CHR_BANK_8K]
        } else {
            cart.chr_rom
        };
        let chr_bank_count_8k = (chr.len() / CHR_BANK_8K).max(1);

        Self {
            prg_rom: cart.prg_rom,
            chr,
            chr_ram: is_chr_ram,
            reg: 0,
            // Boot state is single-screen lower (`reg = 0` → bit 3
            // clear). Header mirroring is ignored on this mapper -
            // the chip itself drives the CIRAM A10 line.
            mirroring: Mirroring::SingleScreenLower,
            prg_bank_count_16k,
            chr_bank_count_8k,
        }
    }

    fn prg_bank_index(&self) -> usize {
        ((self.reg >> 4) as usize & 0x07) % self.prg_bank_count_16k
    }

    fn chr_bank_index(&self) -> usize {
        let bank = (((self.reg & 0x80) >> 4) | (self.reg & 0x07)) as usize;
        bank % self.chr_bank_count_8k
    }

    fn last_prg_bank(&self) -> usize {
        self.prg_bank_count_16k - 1
    }

    fn cpu_prg_byte(&self, addr: u16) -> u8 {
        match addr {
            0x8000..=0xBFFF => {
                let off = (addr - 0x8000) as usize;
                let base = self.prg_bank_index() * PRG_BANK_16K;
                *self.prg_rom.get(base + off).unwrap_or(&0)
            }
            0xC000..=0xFFFF => {
                let off = (addr - 0xC000) as usize;
                let base = self.last_prg_bank() * PRG_BANK_16K;
                *self.prg_rom.get(base + off).unwrap_or(&0)
            }
            _ => 0,
        }
    }
}

impl Mapper for Sunsoft2 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        self.cpu_prg_byte(addr)
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        if !(0x8000..=0xFFFF).contains(&addr) {
            return;
        }
        // Bus conflict: open-collector AND of the CPU value with
        // the ROM byte at the same address. The ROM bank visible
        // at `addr` depends on the *current* register, not the
        // post-write one - this matters when the write itself
        // would change the bank.
        let rom_byte = self.cpu_prg_byte(addr);
        let effective = data & rom_byte;
        self.reg = effective;
        self.mirroring = if effective & 0x08 != 0 {
            Mirroring::SingleScreenUpper
        } else {
            Mirroring::SingleScreenLower
        };
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr < 0x2000 {
            let off = (addr & 0x1FFF) as usize;
            let base = self.chr_bank_index() * CHR_BANK_8K;
            *self.chr.get(base + off).unwrap_or(&0)
        } else {
            0
        }
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if self.chr_ram && addr < 0x2000 {
            let off = (addr & 0x1FFF) as usize;
            let base = self.chr_bank_index() * CHR_BANK_8K;
            if let Some(b) = self.chr.get_mut(base + off) {
                *b = data;
            }
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }

    fn save_state_capture(&self) -> Option<crate::save_state::MapperState> {
        use crate::save_state::mapper::{MirroringSnap, Sunsoft2Snap};
        Some(crate::save_state::MapperState::Sunsoft2(Sunsoft2Snap {
            chr_ram_data: if self.chr_ram { self.chr.clone() } else { Vec::new() },
            reg: self.reg,
            mirroring: MirroringSnap::from_live(self.mirroring),
        }))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::Sunsoft2(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        if self.chr_ram && snap.chr_ram_data.len() == self.chr.len() {
            self.chr.copy_from_slice(&snap.chr_ram_data);
        }
        self.reg = snap.reg;
        self.mirroring = snap.mirroring.to_live();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, Mirroring, TvSystem};

    /// 128 KiB PRG (8 banks of 16 KiB) tagged in their first byte;
    /// 128 KiB CHR (16 banks of 8 KiB) tagged the same way. Every
    /// other byte = `0xFF` so bus-conflict tests at `$FFFF` pass
    /// the full CPU value through.
    fn cart() -> Cartridge {
        let mut prg = vec![0xFFu8; 8 * PRG_BANK_16K];
        for bank in 0..8 {
            prg[bank * PRG_BANK_16K] = bank as u8;
        }
        let mut chr = vec![0xFFu8; 16 * CHR_BANK_8K];
        for bank in 0..16 {
            chr[bank * CHR_BANK_8K] = bank as u8;
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: chr,
            chr_ram: false,
            mapper_id: 89,
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
    fn power_on_layout_is_bank0_at_8000_and_last_at_c000() {
        let m = Sunsoft2::new(cart());
        assert_eq!(m.cpu_peek(0x8000), 0); // bank 0
        assert_eq!(m.cpu_peek(0xC000), 7); // last bank (bank 7)
        assert_eq!(m.mirroring(), Mirroring::SingleScreenLower);
    }

    #[test]
    fn prg_bank_uses_bits_4_through_6() {
        let mut m = Sunsoft2::new(cart());
        // bits 4-6 = 0b101 → bank 5; bits 0-2 = 0 → CHR bank 0; bit
        // 3 clear → still single-screen-lower. Write at $FFFF where
        // ROM byte = $FF (bus conflict no-op).
        m.cpu_write(0xFFFF, 0b0101_0000);
        assert_eq!(m.cpu_peek(0x8000), 5);
        assert_eq!(m.cpu_peek(0xC000), 7); // fixed last bank unchanged
        assert_eq!(m.mirroring(), Mirroring::SingleScreenLower);
    }

    #[test]
    fn chr_bank_combines_bit_7_with_low_3_bits() {
        let mut m = Sunsoft2::new(cart());
        // Encoded CHR bank = ((bit7) << 3) | bits0-2.
        // 0b0000_0011 → CHR bank 3.
        m.cpu_write(0xFFFF, 0b0000_0011);
        assert_eq!(m.ppu_read(0x0000), 3);
        // 0b1000_0011 → CHR bank 11 ((1 << 3) | 3).
        m.cpu_write(0xFFFF, 0b1000_0011);
        assert_eq!(m.ppu_read(0x0000), 11);
        // 0b1000_0111 → CHR bank 15.
        m.cpu_write(0xFFFF, 0b1000_0111);
        assert_eq!(m.ppu_read(0x0000), 15);
    }

    #[test]
    fn mirroring_bit_3_picks_single_screen_a_or_b() {
        let mut m = Sunsoft2::new(cart());
        m.cpu_write(0xFFFF, 0x00);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenLower);
        m.cpu_write(0xFFFF, 0x08);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenUpper);
        m.cpu_write(0xFFFF, 0x00);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenLower);
    }

    #[test]
    fn bus_conflict_ands_value_with_rom_byte() {
        let mut m = Sunsoft2::new(cart());
        // First swap to bank 5: write at $FFFF where rom_byte = $FF
        // so the CPU value passes through.
        m.cpu_write(0xFFFF, 0b0101_0000);
        assert_eq!(m.cpu_peek(0x8000), 5);
        // The byte at $8000 in bank 5 is the bank tag = $05. Now
        // write $77 at $8000 → effective = $77 & $05 = $05.
        m.cpu_write(0x8000, 0x77);
        // Latch is now 0x05: PRG bank index = 0, CHR bank = 5,
        // mirroring stays lower (bit 3 of 0x05 is clear).
        assert_eq!(m.cpu_peek(0x8000), 0);
        assert_eq!(m.ppu_read(0x0000), 5);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenLower);
    }

    #[test]
    fn writes_below_8000_are_noop() {
        let mut m = Sunsoft2::new(cart());
        m.cpu_write(0x4020, 0xFF);
        m.cpu_write(0x6000, 0xFF);
        m.cpu_write(0x7FFF, 0xFF);
        assert_eq!(m.cpu_peek(0x8000), 0); // still bank 0
    }

    #[test]
    fn chr_ram_round_trips_when_no_chr_rom() {
        let mut c = cart();
        c.chr_rom = Vec::new();
        c.chr_ram = true;
        let mut m = Sunsoft2::new(c);
        m.ppu_write(0x0010, 0x99);
        assert_eq!(m.ppu_read(0x0010), 0x99);
    }
}
