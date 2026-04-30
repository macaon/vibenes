// SPDX-License-Identifier: GPL-3.0-or-later
//! Irem-LROG017 (iNES mapper 77).
//!
//! One-of-a-kind board Lenar / Irem built for *Napoleon Senki*
//! (Famicom, 1988). The chip is otherwise plain 74HC161 glue;
//! the unusual feature is a **split-CHR** layout that combines a
//! single banked 2 KiB CHR-ROM window with three fixed 2 KiB
//! CHR-RAM windows, plus 4-screen nametable mirroring backed by
//! the cart's own VRAM.
//!
//! ## Register surface (single latch at `$8000-$FFFF`)
//!
//! ```text
//! CCCC PPPP
//!      |||
//!      +++- PPPP: 32 KiB PRG bank at $8000-$FFFF (low nibble)
//! +++- CCCC: 2 KiB CHR-ROM bank at PPU $0000-$07FF (high nibble)
//! ```
//!
//! Bus conflicts on the latch write (CPU value AND visible PRG
//! byte). 4-screen mirroring is hardwired - the cart provides
//! 2 KiB of additional NTRAM beyond the console's 2 KiB.
//!
//! ## CHR memory layout
//!
//! | PPU range          | Source                                |
//! |--------------------|---------------------------------------|
//! | `$0000-$07FF`      | CHR-ROM, 2 KiB bank from `reg >> 4`   |
//! | `$0800-$0FFF`      | CHR-RAM bank 0 (fixed)                |
//! | `$1000-$17FF`      | CHR-RAM bank 1 (fixed)                |
//! | `$1800-$1FFF`      | CHR-RAM bank 2 (fixed)                |
//!
//! 6 KiB of CHR-RAM total. PPU writes to the CHR-ROM window are
//! ignored.
//!
//! References:
//! - <https://www.nesdev.org/wiki/INES_Mapper_077>
//! - `~/Git/Mesen2/Core/NES/Mappers/Irem/IremLrog017.h`
//! - `~/Git/punes/src/core/mappers/mapper_077.c`

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_32K: usize = 32 * 1024;
const CHR_ROM_BANK_2K: usize = 2 * 1024;
const CHR_RAM_SIZE: usize = 6 * 1024;

pub struct IremLrog017 {
    prg_rom: Vec<u8>,
    chr_rom: Vec<u8>,
    /// 6 KiB of cart CHR-RAM mapped at PPU `$0800-$1FFF`.
    chr_ram: Vec<u8>,

    /// Latched value (post-bus-conflict AND).
    reg: u8,

    prg_bank_count_32k: usize,
    chr_rom_bank_count_2k: usize,
}

impl IremLrog017 {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count_32k = (cart.prg_rom.len() / PRG_BANK_32K).max(1);
        let chr_rom = if cart.chr_rom.is_empty() {
            vec![0u8; CHR_ROM_BANK_2K]
        } else {
            cart.chr_rom
        };
        let chr_rom_bank_count_2k = (chr_rom.len() / CHR_ROM_BANK_2K).max(1);
        Self {
            prg_rom: cart.prg_rom,
            chr_rom,
            chr_ram: vec![0u8; CHR_RAM_SIZE],
            reg: 0,
            prg_bank_count_32k,
            chr_rom_bank_count_2k,
        }
    }

    fn prg_bank_base(&self) -> usize {
        let bank = (self.reg & 0x0F) as usize;
        (bank % self.prg_bank_count_32k) * PRG_BANK_32K
    }

    fn chr_rom_bank_base(&self) -> usize {
        let bank = ((self.reg >> 4) & 0x0F) as usize;
        (bank % self.chr_rom_bank_count_2k) * CHR_ROM_BANK_2K
    }

    fn prg_byte(&self, addr: u16) -> u8 {
        let off = (addr - 0x8000) as usize;
        *self.prg_rom.get(self.prg_bank_base() + off).unwrap_or(&0)
    }
}

impl Mapper for IremLrog017 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        if addr >= 0x8000 {
            self.prg_byte(addr)
        } else {
            0
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        if addr >= 0x8000 {
            // Bus conflict: visible PRG byte ANDs the CPU value
            // before reaching the latch.
            self.reg = data & self.prg_byte(addr);
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x07FF => {
                let i = self.chr_rom_bank_base() + (addr as usize);
                *self.chr_rom.get(i).unwrap_or(&0)
            }
            0x0800..=0x1FFF => {
                let i = (addr - 0x0800) as usize;
                *self.chr_ram.get(i).unwrap_or(&0)
            }
            _ => 0,
        }
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if (0x0800..=0x1FFF).contains(&addr) {
            let i = (addr - 0x0800) as usize;
            if let Some(slot) = self.chr_ram.get_mut(i) {
                *slot = data;
            }
        }
        // PPU writes to $0000-$07FF (CHR-ROM) are dropped; PPU
        // writes outside $0000-$1FFF aren't handled here.
    }

    fn mirroring(&self) -> Mirroring {
        // 4-screen mirroring is hardwired on this board - the
        // cart provides 2 KiB of NTRAM for the second pair of
        // nametables.
        Mirroring::FourScreen
    }

    fn save_state_capture(&self) -> Option<crate::save_state::MapperState> {
        use crate::save_state::mapper::IremLrog017Snap;
        Some(crate::save_state::MapperState::IremLrog017(IremLrog017Snap {
            chr_ram_data: self.chr_ram.clone(),
            reg: self.reg,
        }))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::IremLrog017(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        if snap.chr_ram_data.len() == self.chr_ram.len() {
            self.chr_ram.copy_from_slice(&snap.chr_ram_data);
        }
        self.reg = snap.reg;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, Mirroring, TvSystem};

    /// 128 KiB PRG (4 banks of 32 KiB) + 8 KiB CHR-ROM (4 banks
    /// of 2 KiB). PRG fill is `$FF` so bus-conflict ANDs are
    /// no-ops away from offset 0; first byte of each PRG bank
    /// is its index. Each CHR-ROM bank tagged with `0x10 + idx`.
    fn cart() -> Cartridge {
        let mut prg = vec![0xFFu8; 4 * PRG_BANK_32K];
        for bank in 0..4 {
            prg[bank * PRG_BANK_32K] = bank as u8;
        }
        let mut chr = vec![0xFFu8; 4 * CHR_ROM_BANK_2K];
        for bank in 0..4 {
            chr[bank * CHR_ROM_BANK_2K] = (0x10 + bank) as u8;
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: chr,
            chr_ram: false,
            mapper_id: 77,
            submapper: 0,
            mirroring: Mirroring::FourScreen,
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
    fn boot_state_bank_0() {
        let mut m = IremLrog017::new(cart());
        assert_eq!(m.cpu_peek(0x8000), 0); // PRG bank 0
        assert_eq!(m.ppu_read(0x0000), 0x10); // CHR-ROM bank 0
        // CHR-RAM windows start zeroed.
        assert_eq!(m.ppu_read(0x0800), 0);
        assert_eq!(m.ppu_read(0x1000), 0);
        assert_eq!(m.ppu_read(0x1800), 0);
    }

    #[test]
    fn low_nibble_selects_prg_high_nibble_selects_chr() {
        let mut m = IremLrog017::new(cart());
        // PRG = 0x03, CHR-ROM = 0x02 -> reg = 0x23.
        // Write at $8001 (PRG byte $FF) so bus-conflict AND
        // is a no-op.
        m.cpu_write(0x8001, 0x23);
        assert_eq!(m.cpu_peek(0x8000), 3);
        assert_eq!(m.ppu_read(0x0000), 0x12);
        // Switch to PRG 1, CHR-ROM 3.
        m.cpu_write(0x8001, 0x31);
        assert_eq!(m.cpu_peek(0x8000), 1);
        assert_eq!(m.ppu_read(0x0000), 0x13);
    }

    #[test]
    fn chr_ram_round_trips_per_window() {
        let mut m = IremLrog017::new(cart());
        m.ppu_write(0x0900, 0xA1);
        m.ppu_write(0x1100, 0xB2);
        m.ppu_write(0x1900, 0xC3);
        assert_eq!(m.ppu_read(0x0900), 0xA1);
        assert_eq!(m.ppu_read(0x1100), 0xB2);
        assert_eq!(m.ppu_read(0x1900), 0xC3);
        // CHR-ROM window does not accept writes.
        m.ppu_write(0x0000, 0x55);
        assert_eq!(m.ppu_read(0x0000), 0x10);
    }

    #[test]
    fn bus_conflict_masks_value() {
        let mut m = IremLrog017::new(cart());
        // First load PRG bank 2 so the byte at $8000 is `$02`.
        m.cpu_write(0x8001, 0x32);
        assert_eq!(m.cpu_peek(0x8000), 2);
        // Now write at $8000 (PRG byte 0x02 = 0b0000_0010).
        // Value 0x33 ANDed with 0x02 = 0x02.
        // -> low nibble = 0x2 -> PRG bank 2 (unchanged); high
        //    nibble = 0x0 -> CHR-ROM bank 0.
        m.cpu_write(0x8000, 0x33);
        assert_eq!(m.cpu_peek(0x8000), 2);
        assert_eq!(m.ppu_read(0x0000), 0x10);
    }

    #[test]
    fn mirroring_is_four_screen() {
        let m = IremLrog017::new(cart());
        assert_eq!(m.mirroring(), Mirroring::FourScreen);
    }

    #[test]
    fn save_state_round_trip() {
        let mut m = IremLrog017::new(cart());
        m.cpu_write(0x8001, 0x32);
        m.ppu_write(0x0900, 0xAB);
        let snap = m.save_state_capture().unwrap();
        let mut fresh = IremLrog017::new(cart());
        fresh.save_state_apply(&snap).unwrap();
        assert_eq!(fresh.cpu_peek(0x8000), 2);
        assert_eq!(fresh.ppu_read(0x0000), 0x13);
        assert_eq!(fresh.ppu_read(0x0900), 0xAB);
    }
}
