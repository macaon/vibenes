// SPDX-License-Identifier: GPL-3.0-or-later
//! NES-CPROM (iNES mapper 13).
//!
//! Nintendo first-party board used by exactly one licensed retail
//! cart - *Videomation* (Sculptured Software / Nintendo, 1991).
//! Unique among the early Nintendo boards in that it banks
//! **CHR-RAM** rather than CHR-ROM: the cart ships 16 KiB of
//! CHR-RAM, exposes the first 4 KiB as a fixed window at
//! PPU `$0000-$0FFF`, and lets the CPU swap one of four 4 KiB
//! banks into PPU `$1000-$1FFF`.
//!
//! ## Register surface
//!
//! ```text
//! $8000-$FFFF   ......BB -> 4 KiB CHR-RAM bank at PPU $1000-$1FFF
//! ```
//!
//! 32 KiB PRG fixed at `$8000-$FFFF` (no banking). Mirroring is
//! hardwired to vertical regardless of the iNES header (some dumps
//! incorrectly flag the cart as 4-screen). No PRG-RAM, no IRQ, no
//! bus conflicts on the commercial cart.
//!
//! References:
//! - <https://www.nesdev.org/wiki/INES_Mapper_013>
//! - `~/Git/Mesen2/Core/NES/Mappers/Nintendo/CpRom.h`
//! - `~/Git/punes/src/core/mappers/mapper_013.c`

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const CHR_BANK_4K: usize = 4 * 1024;
const CHR_RAM_SIZE: usize = 16 * 1024;

pub struct Cprom {
    prg_rom: Vec<u8>,
    chr_ram: Vec<u8>,

    /// Selected CHR-RAM bank for the upper PPU window
    /// (`$1000-$1FFF`). Two-bit field, masked at write time.
    upper_bank: u8,

    prg_len: usize,
}

impl Cprom {
    pub fn new(cart: Cartridge) -> Self {
        // Per spec the cart always ships 16 KiB CHR-RAM regardless
        // of what the iNES header claims. Drop any CHR-ROM bytes
        // (unexpected on a CPROM dump) and allocate fresh RAM.
        Self {
            prg_len: cart.prg_rom.len(),
            prg_rom: cart.prg_rom,
            chr_ram: vec![0u8; CHR_RAM_SIZE],
            upper_bank: 0,
        }
    }

    fn prg_byte(&self, addr: u16) -> u8 {
        if self.prg_len == 0 {
            return 0;
        }
        let off = (addr - 0x8000) as usize;
        // 32 KiB fixed; 16 KiB carts mirror.
        *self.prg_rom.get(off % self.prg_len).unwrap_or(&0)
    }

    fn chr_index(&self, addr: u16) -> usize {
        if addr < 0x1000 {
            // Fixed 4 KiB bank 0.
            addr as usize
        } else {
            let off = (addr - 0x1000) as usize;
            (self.upper_bank as usize) * CHR_BANK_4K + off
        }
    }
}

impl Mapper for Cprom {
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
            self.upper_bank = data & 0x03;
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr < 0x2000 {
            *self.chr_ram.get(self.chr_index(addr)).unwrap_or(&0)
        } else {
            0
        }
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if addr < 0x2000 {
            let i = self.chr_index(addr);
            if let Some(slot) = self.chr_ram.get_mut(i) {
                *slot = data;
            }
        }
    }

    fn mirroring(&self) -> Mirroring {
        // Hardwired vertical per spec. Some iNES dumps mis-flag
        // the cart as 4-screen; we override here so those still
        // play correctly.
        Mirroring::Vertical
    }

    fn save_state_capture(&self) -> Option<crate::save_state::MapperState> {
        use crate::save_state::mapper::CpromSnap;
        Some(crate::save_state::MapperState::Cprom(CpromSnap {
            chr_ram_data: self.chr_ram.clone(),
            upper_bank: self.upper_bank,
        }))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::Cprom(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        if snap.chr_ram_data.len() == self.chr_ram.len() {
            self.chr_ram.copy_from_slice(&snap.chr_ram_data);
        }
        self.upper_bank = snap.upper_bank;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, Mirroring, TvSystem};

    /// 32 KiB PRG (one bank). The test does not need PRG bank
    /// switching since CPROM has none; tag the very first byte
    /// for sanity.
    fn cart() -> Cartridge {
        let mut prg = vec![0xAAu8; 32 * 1024];
        prg[0] = 0x55;
        prg[0x4000] = 0x33;
        // Override mirroring to Horizontal in the cart header to
        // verify we ignore it.
        Cartridge {
            prg_rom: prg,
            chr_rom: Vec::new(),
            chr_ram: true,
            mapper_id: 13,
            submapper: 0,
            mirroring: Mirroring::Horizontal,
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
    fn prg_is_fixed_at_8000() {
        let m = Cprom::new(cart());
        assert_eq!(m.cpu_peek(0x8000), 0x55);
        assert_eq!(m.cpu_peek(0xC000), 0x33);
    }

    #[test]
    fn lower_chr_window_is_fixed_to_bank_0() {
        let mut m = Cprom::new(cart());
        // Stage: write through PPU into bank 0.
        m.ppu_write(0x0010, 0x11);
        m.ppu_write(0x0FFF, 0x22);
        // Switch upper bank - should not move the lower window.
        m.cpu_write(0x8000, 0x03);
        assert_eq!(m.ppu_read(0x0010), 0x11);
        assert_eq!(m.ppu_read(0x0FFF), 0x22);
    }

    #[test]
    fn upper_chr_window_swaps_among_4_banks() {
        let mut m = Cprom::new(cart());
        // Stage distinct contents in each of the 4 upper banks.
        for bank in 0..=3 {
            m.cpu_write(0x8000, bank);
            // Write a tag at $1000 of the currently-selected bank.
            m.ppu_write(0x1000, 0x40 | bank);
            m.ppu_write(0x1FFF, 0x80 | bank);
        }
        // Read back each bank and confirm.
        for bank in 0..=3 {
            m.cpu_write(0x8000, bank);
            assert_eq!(m.ppu_read(0x1000), 0x40 | bank);
            assert_eq!(m.ppu_read(0x1FFF), 0x80 | bank);
        }
    }

    #[test]
    fn high_bits_of_register_are_masked() {
        let mut m = Cprom::new(cart());
        m.ppu_write(0x1000, 0xAA); // bank 0 upper
        m.cpu_write(0x8000, 0xFC); // bits 0-1 = 0
        assert_eq!(m.ppu_read(0x1000), 0xAA);
        m.cpu_write(0x8000, 0xFE); // bits 0-1 = 10 (bank 2)
        // Bank 2 untouched -> reads zero.
        assert_eq!(m.ppu_read(0x1000), 0);
    }

    #[test]
    fn mirroring_is_hardwired_vertical_even_when_header_says_otherwise() {
        let m = Cprom::new(cart());
        assert_eq!(m.mirroring(), Mirroring::Vertical);
    }

    #[test]
    fn save_state_round_trip_preserves_chr_ram_and_bank() {
        let mut m = Cprom::new(cart());
        m.cpu_write(0x8000, 0x02);
        m.ppu_write(0x1500, 0xCD);
        m.ppu_write(0x0500, 0xEF);
        let snap = m.save_state_capture().unwrap();
        let mut fresh = Cprom::new(cart());
        fresh.save_state_apply(&snap).unwrap();
        assert_eq!(fresh.upper_bank, 2);
        assert_eq!(fresh.ppu_read(0x1500), 0xCD);
        assert_eq!(fresh.ppu_read(0x0500), 0xEF);
    }
}
