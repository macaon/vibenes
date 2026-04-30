// SPDX-License-Identifier: GPL-3.0-or-later
//! UNROM-flip / Crazy Climber wiring (iNES mapper 180).
//!
//! UNROM hardware with the bank-switching window relocated from
//! `$8000-$BFFF` to `$C000-$FFFF`. The first PRG bank is hardwired
//! at the low window and the high window swaps - the inverse of
//! mapper 2. Driven by *Crazy Climber* (Nichibutsu, 1986) and
//! *Hayauchi Super Igo* (Nichibutsu).
//!
//! ## Register surface
//!
//! ```text
//! $8000-$FFFF   .....BBB -> 16 KiB PRG bank at $C000-$FFFF
//! ```
//!
//! `$8000-$BFFF` is fixed to bank 0. 8 KiB CHR-RAM, hardwired
//! mirroring. No bus conflicts on the canonical (submapper 0)
//! variant.
//!
//! References:
//! - <https://www.nesdev.org/wiki/INES_Mapper_180>
//! - `~/Git/Mesen2/Core/NES/Mappers/Nintendo/UnRom_180.h`
//! - `~/Git/punes/src/core/mappers/mapper_180.c`

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_16K: usize = 16 * 1024;
const CHR_BANK_8K: usize = 8 * 1024;

pub struct Un1rom180 {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,

    /// Selected 16 KiB bank for the `$C000-$FFFF` window.
    bank: u8,

    mirroring: Mirroring,
    prg_bank_count_16k: usize,
}

impl Un1rom180 {
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
            bank: 0,
            mirroring: cart.mirroring,
            prg_bank_count_16k,
        }
    }

    fn switch_bank_base(&self) -> usize {
        ((self.bank as usize) % self.prg_bank_count_16k) * PRG_BANK_16K
    }
}

impl Mapper for Un1rom180 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
            0x8000..=0xBFFF => {
                // Fixed first bank.
                let i = (addr - 0x8000) as usize;
                *self.prg_rom.get(i).unwrap_or(&0)
            }
            0xC000..=0xFFFF => {
                let i = self.switch_bank_base() + (addr - 0xC000) as usize;
                *self.prg_rom.get(i).unwrap_or(&0)
            }
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        if addr >= 0x8000 {
            self.bank = data;
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
        use crate::save_state::mapper::Un1rom180Snap;
        Some(crate::save_state::MapperState::Un1rom180(Un1rom180Snap {
            chr_ram_data: if self.chr_ram { self.chr.clone() } else { Vec::new() },
            bank: self.bank,
        }))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::Un1rom180(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        if self.chr_ram && snap.chr_ram_data.len() == self.chr.len() {
            self.chr.copy_from_slice(&snap.chr_ram_data);
        }
        self.bank = snap.bank;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, Mirroring, TvSystem};

    fn cart() -> Cartridge {
        // 128 KiB PRG (8 banks of 16 KiB), CHR-RAM. First byte of
        // each bank tagged with bank index for read-back checks.
        let mut prg = vec![0xFFu8; 8 * PRG_BANK_16K];
        for bank in 0..8 {
            prg[bank * PRG_BANK_16K] = bank as u8;
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: Vec::new(),
            chr_ram: true,
            mapper_id: 180,
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
    fn boot_state_bank0_at_both_windows() {
        let m = Un1rom180::new(cart());
        assert_eq!(m.cpu_peek(0x8000), 0);
        // After power-up the bank latch is 0, so $C000 also sees
        // bank 0.
        assert_eq!(m.cpu_peek(0xC000), 0);
    }

    #[test]
    fn write_swaps_high_window_low_window_stays_fixed() {
        let mut m = Un1rom180::new(cart());
        m.cpu_write(0x8000, 5);
        assert_eq!(m.cpu_peek(0xC000), 5);
        // Low window does not move.
        assert_eq!(m.cpu_peek(0x8000), 0);
        // Any address in $8000-$FFFF triggers the latch.
        m.cpu_write(0xFFFF, 3);
        assert_eq!(m.cpu_peek(0xC000), 3);
        assert_eq!(m.cpu_peek(0x8000), 0);
    }

    #[test]
    fn high_bits_wrap_via_modulo() {
        let mut m = Un1rom180::new(cart());
        // 8 banks, write 9 -> bank 1.
        m.cpu_write(0x8000, 9);
        assert_eq!(m.cpu_peek(0xC000), 1);
    }

    #[test]
    fn chr_ram_round_trip() {
        let mut m = Un1rom180::new(cart());
        m.ppu_write(0x0042, 0xAB);
        assert_eq!(m.ppu_read(0x0042), 0xAB);
    }

    #[test]
    fn save_state_round_trip() {
        let mut m = Un1rom180::new(cart());
        m.cpu_write(0x8000, 4);
        m.ppu_write(0x0123, 0x55);
        let snap = m.save_state_capture().unwrap();
        let mut fresh = Un1rom180::new(cart());
        fresh.save_state_apply(&snap).unwrap();
        assert_eq!(fresh.cpu_peek(0xC000), 4);
        assert_eq!(fresh.ppu_read(0x0123), 0x55);
    }
}
