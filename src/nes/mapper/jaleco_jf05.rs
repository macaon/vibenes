// SPDX-License-Identifier: GPL-3.0-or-later
//! Jaleco JF-05/06/07/08/09/10/11 (iNES mapper 87).
//!
//! Tiny PCB family from Jaleco's early Famicom catalog. Plain
//! 32 KiB PRG fixed at `$8000-$FFFF`, hardwired mirroring, and a
//! single CHR-banking register that lives in cart-RAM space:
//!
//! ```text
//! $6000-$7FFF   ......BA -> CHR bank = AB (bits swapped)
//! ```
//!
//! The bit swap is the only non-trivial bit on the cart - the PCB
//! routes the CPU data lines straight into the CHR `A13`/`A14`
//! pins crossed-over, so writing `$01` selects bank 2 and writing
//! `$02` selects bank 1.
//!
//! Games (licensed Jaleco / Asmik):
//! *Argus*, *Argos no Senshi* (Rygar JP),
//! *City Connection*, *Ninja Jajamaru-kun*, *Spy vs Spy*,
//! *Mahjong Companion*, *Mezase Pachi Pro: Pachio-kun 2*,
//! *Moero! TwinBee*, *Jaleco Mahjong* and several other early
//! Famicom titles.
//!
//! References:
//! - <https://www.nesdev.org/wiki/INES_Mapper_087>
//! - `~/Git/Mesen2/Core/NES/Mappers/Jaleco/JalecoJfxx.h`
//! - `~/Git/punes/src/core/mappers/mapper_087.c`

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const CHR_BANK_8K: usize = 8 * 1024;

pub struct JalecoJf05 {
    prg_rom: Vec<u8>,
    chr_rom: Vec<u8>,
    /// Raw latch as written. The CHR bank index is the low-2-bit
    /// swap of this value, recomputed on read.
    reg: u8,
    mirroring: Mirroring,
    chr_bank_count_8k: usize,
    prg_len: usize,
}

impl JalecoJf05 {
    pub fn new(cart: Cartridge) -> Self {
        let chr_rom = if cart.chr_rom.is_empty() {
            vec![0u8; CHR_BANK_8K]
        } else {
            cart.chr_rom
        };
        let chr_bank_count_8k = (chr_rom.len() / CHR_BANK_8K).max(1);
        Self {
            prg_len: cart.prg_rom.len(),
            prg_rom: cart.prg_rom,
            chr_rom,
            reg: 0,
            mirroring: cart.mirroring,
            chr_bank_count_8k,
        }
    }

    fn chr_bank(&self) -> usize {
        let swapped = ((self.reg & 0x01) << 1) | ((self.reg & 0x02) >> 1);
        (swapped as usize) % self.chr_bank_count_8k
    }
}

impl Mapper for JalecoJf05 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        if addr >= 0x8000 && self.prg_len > 0 {
            let off = (addr - 0x8000) as usize;
            *self.prg_rom.get(off % self.prg_len).unwrap_or(&0)
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

    fn ppu_write(&mut self, _addr: u16, _data: u8) {
        // CHR-ROM only.
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }

    fn save_state_capture(&self) -> Option<crate::save_state::MapperState> {
        use crate::save_state::mapper::JalecoJf05Snap;
        Some(crate::save_state::MapperState::JalecoJf05(JalecoJf05Snap {
            reg: self.reg,
        }))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::JalecoJf05(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        self.reg = snap.reg;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, Mirroring, TvSystem};

    /// 32 KiB PRG (one bank) + 32 KiB CHR (4 x 8 KiB), each CHR
    /// bank tagged with its own index for read-back checks.
    fn cart() -> Cartridge {
        let prg = vec![0xAAu8; 32 * 1024];
        let mut chr = vec![0xFFu8; 4 * CHR_BANK_8K];
        for bank in 0..4 {
            chr[bank * CHR_BANK_8K] = bank as u8;
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: chr,
            chr_ram: false,
            mapper_id: 87,
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
    fn boot_state_chr_bank_zero() {
        let mut m = JalecoJf05::new(cart());
        assert_eq!(m.ppu_read(0x0000), 0);
    }

    #[test]
    fn write_to_6000_swaps_low_two_bits_into_chr_bank() {
        let mut m = JalecoJf05::new(cart());
        // value 0x01 -> bank 2 (bit 0 -> bit 1).
        m.cpu_write(0x6000, 0x01);
        assert_eq!(m.ppu_read(0x0000), 2);
        // value 0x02 -> bank 1 (bit 1 -> bit 0).
        m.cpu_write(0x6000, 0x02);
        assert_eq!(m.ppu_read(0x0000), 1);
        // value 0x03 -> bank 3 (both bits set).
        m.cpu_write(0x6000, 0x03);
        assert_eq!(m.ppu_read(0x0000), 3);
        // High bits ignored after the swap mask.
        m.cpu_write(0x6000, 0xFC);
        assert_eq!(m.ppu_read(0x0000), 0);
    }

    #[test]
    fn writes_outside_register_window_are_ignored() {
        let mut m = JalecoJf05::new(cart());
        m.cpu_write(0x5FFF, 0x03);
        assert_eq!(m.ppu_read(0x0000), 0);
        m.cpu_write(0x8000, 0x03);
        assert_eq!(m.ppu_read(0x0000), 0);
    }

    #[test]
    fn ppu_writes_have_no_effect() {
        let mut m = JalecoJf05::new(cart());
        m.ppu_write(0x0000, 0x55);
        assert_eq!(m.ppu_read(0x0000), 0);
    }

    #[test]
    fn save_state_round_trip() {
        let mut m = JalecoJf05::new(cart());
        m.cpu_write(0x7000, 0x03);
        let snap = m.save_state_capture().unwrap();
        let mut fresh = JalecoJf05::new(cart());
        fresh.save_state_apply(&snap).unwrap();
        assert_eq!(fresh.ppu_read(0x0000), 3);
    }
}
