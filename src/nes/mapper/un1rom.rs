// SPDX-License-Identifier: GPL-3.0-or-later
//! HVC-UN1ROM (iNES mapper 94).
//!
//! UNROM cousin used by exactly one licensed cart: *Senjou no
//! Ookami* (Commando JP, Capcom 1986). The PCB is wired
//! identically to UNROM apart from the bank-select bit positions:
//! the cart routes CPU `D2..D4` into the PRG `A14..A16` pins,
//! shifting the bank index into the upper half of the latch byte.
//!
//! ## Register surface
//!
//! ```text
//! $8000-$FFFF   ...BBB.. -> 16 KiB PRG bank at $8000-$BFFF
//! ```
//!
//! `$C000-$FFFF` is hardwired to the last 16 KiB bank. 8 KiB
//! CHR-RAM, hardwired mirroring, bus conflicts on the latch
//! write. Three bits of bank select cap the cart at 128 KiB PRG
//! (8 banks of 16 KiB), which exactly fits Senjou no Ookami.
//!
//! References:
//! - <https://www.nesdev.org/wiki/INES_Mapper_094>
//! - `~/Git/Mesen2/Core/NES/Mappers/Nintendo/UnRom_94.h`
//! - `~/Git/punes/src/core/mappers/mapper_094.c`

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_16K: usize = 16 * 1024;
const CHR_BANK_8K: usize = 8 * 1024;

pub struct Un1rom {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,

    /// Latched write byte (post-bus-conflict). Bank index is
    /// `(reg >> 2) & 0x07` evaluated at PRG-read time.
    reg: u8,

    mirroring: Mirroring,
    prg_bank_count_16k: usize,
}

impl Un1rom {
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
            reg: 0,
            mirroring: cart.mirroring,
            prg_bank_count_16k,
        }
    }

    fn switch_bank_base(&self) -> usize {
        let bank = ((self.reg >> 2) & 0x07) as usize;
        (bank % self.prg_bank_count_16k) * PRG_BANK_16K
    }

    fn fixed_bank_base(&self) -> usize {
        (self.prg_bank_count_16k - 1) * PRG_BANK_16K
    }

    fn prg_byte(&self, addr: u16) -> u8 {
        match addr {
            0x8000..=0xBFFF => {
                let i = self.switch_bank_base() + (addr - 0x8000) as usize;
                *self.prg_rom.get(i).unwrap_or(&0)
            }
            0xC000..=0xFFFF => {
                let i = self.fixed_bank_base() + (addr - 0xC000) as usize;
                *self.prg_rom.get(i).unwrap_or(&0)
            }
            _ => 0,
        }
    }
}

impl Mapper for Un1rom {
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
            // Bus conflict: the cart's PRG output and the CPU's
            // value are wired-AND together onto the bank-select
            // pins.
            self.reg = data & self.prg_byte(addr);
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
        use crate::save_state::mapper::Un1romSnap;
        Some(crate::save_state::MapperState::Un1rom(Un1romSnap {
            chr_ram_data: if self.chr_ram { self.chr.clone() } else { Vec::new() },
            reg: self.reg,
        }))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::Un1rom(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        if self.chr_ram && snap.chr_ram_data.len() == self.chr.len() {
            self.chr.copy_from_slice(&snap.chr_ram_data);
        }
        self.reg = snap.reg;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, Mirroring, TvSystem};

    /// 128 KiB PRG (8 banks of 16 KiB), CHR-RAM. Each PRG bank's
    /// first byte is its own index; everything else is `$FF` so
    /// bus-conflict ANDs leave the latched value alone.
    fn cart() -> Cartridge {
        let mut prg = vec![0xFFu8; 8 * PRG_BANK_16K];
        for bank in 0..8 {
            prg[bank * PRG_BANK_16K] = bank as u8;
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: Vec::new(),
            chr_ram: true,
            mapper_id: 94,
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
    fn boot_state_first_at_8000_last_at_c000() {
        let m = Un1rom::new(cart());
        assert_eq!(m.cpu_peek(0x8000), 0);
        assert_eq!(m.cpu_peek(0xC000), 7);
    }

    #[test]
    fn bank_select_uses_bits_4_3_2() {
        let mut m = Un1rom::new(cart());
        // value 0b0001_0100 -> (>>2) & 7 = 5.
        // Write at $8001 to avoid bus-conflict mask (PRG byte $FF).
        m.cpu_write(0x8001, 0b0001_0100);
        assert_eq!(m.cpu_peek(0x8000), 5);
        // Bits 0-1 of the value have no effect.
        m.cpu_write(0x8001, 0b0001_0111);
        assert_eq!(m.cpu_peek(0x8000), 5);
        // Bits above bit 4 are also ignored.
        m.cpu_write(0x8001, 0b1110_0100);
        assert_eq!(m.cpu_peek(0x8000), 1);
        // Fixed window stays on the last bank.
        assert_eq!(m.cpu_peek(0xC000), 7);
    }

    #[test]
    fn bus_conflict_masks_value() {
        let mut m = Un1rom::new(cart());
        // First select bank 5 so PRG byte at $8000 is 0x05.
        m.cpu_write(0x8001, 0b0001_0100);
        assert_eq!(m.cpu_peek(0x8000), 5);
        // Now write at $8000 (PRG byte 0x05 = 0b0000_0101).
        // Value 0b0001_0100 & 0b0000_0101 = 0b0000_0100
        // -> (>>2) & 7 = 1.
        m.cpu_write(0x8000, 0b0001_0100);
        assert_eq!(m.cpu_peek(0x8000), 1);
    }

    #[test]
    fn chr_ram_round_trip() {
        let mut m = Un1rom::new(cart());
        m.ppu_write(0x0123, 0xAB);
        assert_eq!(m.ppu_read(0x0123), 0xAB);
        m.ppu_write(0x1FFF, 0x55);
        assert_eq!(m.ppu_read(0x1FFF), 0x55);
    }

    #[test]
    fn save_state_round_trip() {
        let mut m = Un1rom::new(cart());
        m.cpu_write(0x8001, 0b0000_1100); // bank 3
        m.ppu_write(0x0010, 0xEE);
        let snap = m.save_state_capture().unwrap();
        let mut fresh = Un1rom::new(cart());
        fresh.save_state_apply(&snap).unwrap();
        assert_eq!(fresh.cpu_peek(0x8000), 3);
        assert_eq!(fresh.ppu_read(0x0010), 0xEE);
    }
}
