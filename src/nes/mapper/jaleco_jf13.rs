// SPDX-License-Identifier: GPL-3.0-or-later
//! Jaleco JF-13 (iNES mapper 86).
//!
//! 32 KiB-switchable PRG at `$8000-$FFFF`, 8 KiB-switchable
//! CHR-ROM, hardwired mirroring. The chip also wires a uPD7756C
//! ADPCM speech-sample chip at `$7000` for the *Moero!! Pro
//! Yakyuu* announcer voice. Like mapper 72 (JF-17), the speech
//! channel is **not modeled** here - games still boot and play,
//! but the announcer is silent.
//!
//! ## Register surface (single latch at `$6000-$6FFF`)
//!
//! ```text
//! $6000-$6FFF   .CPP CCC -> PRG bank in bits 5-4, CHR bank in
//!                bits 6 and 1-0 (CHR = bit6<<2 | bits 1-0)
//! $7000-$7FFF   speech control (uPD7756 - not implemented)
//! ```
//!
//! Three bits of CHR (8 banks of 8 KiB = 64 KiB max), two bits
//! of PRG (4 banks of 32 KiB = 128 KiB max). Both fit the JF-13
//! retail catalog.
//!
//! Games (licensed Jaleco): *Moero!! Pro Yakyuu* (Bases Loaded
//! JP), *Moero!! Juuou-Densetsu Pro Yakyuu*, *Hanjuusou Densetsu*,
//! *Choujin Sentai Jetman*.
//!
//! References:
//! - <https://www.nesdev.org/wiki/INES_Mapper_086>
//! - `~/Git/Mesen2/Core/NES/Mappers/Jaleco/JalecoJf13.h`
//! - `~/Git/punes/src/core/mappers/mapper_086.c`

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_32K: usize = 32 * 1024;
const CHR_BANK_8K: usize = 8 * 1024;

pub struct JalecoJf13 {
    prg_rom: Vec<u8>,
    chr_rom: Vec<u8>,
    chr_ram: bool,

    /// Latched value at `$6000-$6FFF`.
    reg: u8,

    mirroring: Mirroring,
    prg_bank_count_32k: usize,
    chr_bank_count_8k: usize,
}

impl JalecoJf13 {
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
        // Bits 6 and 1-0 form the CHR index: bit 6 -> bit 2.
        let bank = (((self.reg >> 4) & 0x04) | (self.reg & 0x03)) as usize;
        bank % self.chr_bank_count_8k
    }
}

impl Mapper for JalecoJf13 {
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
        // The PCB decodes only the `$6000` slot; `$7000` is the
        // speech-sample chip's window (not modeled).
        if (0x6000..=0x6FFF).contains(&addr) {
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
        use crate::save_state::mapper::JalecoJf13Snap;
        Some(crate::save_state::MapperState::JalecoJf13(JalecoJf13Snap {
            chr_ram_data: if self.chr_ram { self.chr_rom.clone() } else { Vec::new() },
            reg: self.reg,
        }))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::JalecoJf13(snap) = state else {
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

    /// 128 KiB PRG (4 banks of 32 KiB) + 64 KiB CHR (8 banks of
    /// 8 KiB), each bank tagged with its index.
    fn cart() -> Cartridge {
        let mut prg = vec![0xFFu8; 4 * PRG_BANK_32K];
        for bank in 0..4 {
            prg[bank * PRG_BANK_32K] = bank as u8;
        }
        let mut chr = vec![0xFFu8; 8 * CHR_BANK_8K];
        for bank in 0..8 {
            chr[bank * CHR_BANK_8K] = (0x10 + bank) as u8;
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: chr,
            chr_ram: false,
            mapper_id: 86,
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
        let m = JalecoJf13::new(cart());
        assert_eq!(m.cpu_peek(0x8000), 0);
        // CHR bank 0 tagged 0x10.
        let mut m = m;
        assert_eq!(m.ppu_read(0x0000), 0x10);
    }

    #[test]
    fn prg_bank_uses_bits_5_4() {
        let mut m = JalecoJf13::new(cart());
        m.cpu_write(0x6000, 0b0001_0000); // bits 5-4 = 01
        assert_eq!(m.cpu_peek(0x8000), 1);
        m.cpu_write(0x6000, 0b0011_0000); // bits 5-4 = 11
        assert_eq!(m.cpu_peek(0x8000), 3);
    }

    #[test]
    fn chr_bank_combines_bit6_and_bits_1_0() {
        let mut m = JalecoJf13::new(cart());
        // bits 1-0 = 0b11, bit 6 = 1 -> bank 0b111 = 7.
        m.cpu_write(0x6000, 0b0100_0011);
        assert_eq!(m.ppu_read(0x0000), 0x17);
        // bits 1-0 = 0b10, bit 6 = 0 -> bank 0b010 = 2.
        m.cpu_write(0x6000, 0b0000_0010);
        assert_eq!(m.ppu_read(0x0000), 0x12);
    }

    #[test]
    fn writes_in_7000_window_do_not_change_banking() {
        let mut m = JalecoJf13::new(cart());
        m.cpu_write(0x6000, 0b0011_0000); // bank 3
        assert_eq!(m.cpu_peek(0x8000), 3);
        // Speech-sample register write should not move PRG.
        m.cpu_write(0x7000, 0xFF);
        assert_eq!(m.cpu_peek(0x8000), 3);
    }

    #[test]
    fn save_state_round_trip() {
        let mut m = JalecoJf13::new(cart());
        m.cpu_write(0x6000, 0b0011_0011);
        let snap = m.save_state_capture().unwrap();
        let mut fresh = JalecoJf13::new(cart());
        fresh.save_state_apply(&snap).unwrap();
        assert_eq!(fresh.cpu_peek(0x8000), 3);
        assert_eq!(fresh.ppu_read(0x0000), 0x13);
    }
}
