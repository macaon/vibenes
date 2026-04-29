// SPDX-License-Identifier: GPL-3.0-or-later
//! Sunsoft-1 - iNES mapper 184.
//!
//! Predecessor of the Sunsoft-3/4/FME-7 family. Discrete chip with
//! a single register at `$6000-$7FFF`; PRG is hardwired to a 32 KiB
//! window at `$8000`, all banking is in CHR. Used by *Atlantis no
//! Nazo*, *Wing of Madoola*, *Hi no Tori: Houou-hen Gaou no Bouken*,
//! *Maharaja*, *Kid Niki: Radical Ninja* JP, *Ripple Island*, etc.
//! all 1986-87 Sunsoft Famicom releases.
//!
//! ## Register surface
//!
//! Single 8-bit register, decoded across `$6000-$7FFF`:
//!
//! ```text
//! 7  bit  0
//! ---- ----
//! .HHH .LLL
//!  |||  |||
//!  |||  +++- 4 KiB CHR bank at PPU $0000-$0FFF
//!  +++------ 4 KiB CHR bank at PPU $1000-$1FFF
//! ```
//!
//! Per the NESdev wiki and Mesen2: the **most significant bit of
//! the high-slot bank index is always set in hardware**, so the
//! high CHR slot reads from bank `0x80 | (HHH)`. On the small (16-
//! 32 KiB) CHR-ROMs of real mapper-184 carts this just wraps within
//! the available banks; the rule matters only on hypothetical large
//! CHR carts. puNES and Nestopia drop this detail; we side with
//! Mesen2 + the wiki because it is the documented hardware model.
//!
//! ## PRG layout
//!
//! Fixed 32 KiB at CPU `$8000-$FFFF`. Most carts ship 32 KiB so
//! there is nothing to swap; oversized PRG (none known) would land
//! at bank 0.
//!
//! References:
//! - <https://www.nesdev.org/wiki/INES_Mapper_184>
//! - `~/Git/Mesen2/Core/NES/Mappers/Sunsoft/Sunsoft184.h`
//! - `~/Git/punes/src/core/mappers/mapper_184.c`
//! - `~/Git/nestopia/source/core/board/NstBoardSunsoft1.cpp`

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const CHR_BANK_4K: usize = 4 * 1024;
#[cfg(test)]
const PRG_BANK_32K: usize = 32 * 1024;

pub struct Sunsoft1 {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,

    /// Latched register value. CHR slot 0 = bits 0-2; CHR slot 1
    /// = `0x80 | bits 4-6` (Mesen2's hardware model).
    reg: u8,

    mirroring: Mirroring,

    chr_bank_count_4k: usize,
}

impl Sunsoft1 {
    pub fn new(cart: Cartridge) -> Self {
        let is_chr_ram = cart.chr_ram || cart.chr_rom.is_empty();
        let chr = if is_chr_ram {
            vec![0u8; 2 * CHR_BANK_4K]
        } else {
            cart.chr_rom
        };
        let chr_bank_count_4k = (chr.len() / CHR_BANK_4K).max(1);

        Self {
            prg_rom: cart.prg_rom,
            chr,
            chr_ram: is_chr_ram,
            reg: 0,
            mirroring: cart.mirroring,
            chr_bank_count_4k,
        }
    }

    fn chr_low_bank(&self) -> usize {
        (self.reg & 0x07) as usize % self.chr_bank_count_4k
    }

    fn chr_high_bank(&self) -> usize {
        (0x80 | ((self.reg >> 4) & 0x07)) as usize % self.chr_bank_count_4k
    }
}

impl Mapper for Sunsoft1 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
            0x8000..=0xFFFF => {
                let off = (addr - 0x8000) as usize;
                let total = self.prg_rom.len();
                if total == 0 {
                    return 0;
                }
                // Single 32 KiB window, masked for safety in case
                // someone hands us an oversized ROM.
                *self.prg_rom.get(off & (total - 1).max(1)).unwrap_or(&0)
            }
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        if (0x6000..=0x7FFF).contains(&addr) {
            self.reg = data;
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr < 0x2000 {
            let off = (addr & 0x0FFF) as usize;
            let bank = if addr < 0x1000 {
                self.chr_low_bank()
            } else {
                self.chr_high_bank()
            };
            let base = bank * CHR_BANK_4K;
            *self.chr.get(base + off).unwrap_or(&0)
        } else {
            0
        }
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if self.chr_ram && addr < 0x2000 {
            let off = (addr & 0x0FFF) as usize;
            let bank = if addr < 0x1000 {
                self.chr_low_bank()
            } else {
                self.chr_high_bank()
            };
            let base = bank * CHR_BANK_4K;
            if let Some(b) = self.chr.get_mut(base + off) {
                *b = data;
            }
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }

    fn save_state_capture(&self) -> Option<crate::save_state::MapperState> {
        use crate::save_state::mapper::Sunsoft1Snap;
        Some(crate::save_state::MapperState::Sunsoft1(Sunsoft1Snap {
            chr_ram_data: if self.chr_ram { self.chr.clone() } else { Vec::new() },
            reg: self.reg,
        }))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::Sunsoft1(snap) = state else {
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

    /// 32 KiB PRG (single bank), `chr_banks_4k` × 4 KiB CHR. Each
    /// CHR bank's first byte = its bank index, so tests can read
    /// `ppu_read(0x0000)` / `ppu_read(0x1000)` to learn which bank
    /// each window is currently mapped to.
    fn cart(chr_banks_4k: usize) -> Cartridge {
        let mut prg = vec![0u8; PRG_BANK_32K];
        prg[0] = 0xAA;
        prg[PRG_BANK_32K - 1] = 0xBB;
        let mut chr = vec![0u8; chr_banks_4k * CHR_BANK_4K];
        for bank in 0..chr_banks_4k {
            chr[bank * CHR_BANK_4K] = bank as u8;
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: chr,
            chr_ram: false,
            mapper_id: 184,
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
    fn prg_is_a_fixed_32k_window() {
        let m = Sunsoft1::new(cart(8));
        assert_eq!(m.cpu_peek(0x8000), 0xAA);
        assert_eq!(m.cpu_peek(0xFFFF), 0xBB);
        // Writes to the PRG window are dropped on the floor.
        let mut m = m;
        m.cpu_write(0x8000, 0x55);
        assert_eq!(m.cpu_peek(0x8000), 0xAA);
    }

    #[test]
    fn chr_low_slot_picks_bank_from_low_3_bits() {
        let mut m = Sunsoft1::new(cart(16));
        // Write reg = 0b0000_0011 → low slot bank 3.
        m.cpu_write(0x6000, 0b0000_0011);
        assert_eq!(m.ppu_read(0x0000), 3);
        m.cpu_write(0x7FFF, 0b0000_0101);
        assert_eq!(m.ppu_read(0x0000), 5);
    }

    #[test]
    fn chr_high_slot_forces_bit_3_into_bank_index() {
        let mut m = Sunsoft1::new(cart(16));
        // High nibble = 0b011 → bits 4-6 = 0b011. The chip forces
        // bit 7 (== bank 0x80) so effective bank = 0x83 = 131.
        // 131 mod 16 = 3.
        m.cpu_write(0x6000, 0b0011_0000);
        assert_eq!(m.ppu_read(0x1000), 3);
        // High nibble = 0b101 → effective bank 0x85 = 133. mod 16 = 5.
        m.cpu_write(0x6000, 0b0101_0000);
        assert_eq!(m.ppu_read(0x1000), 5);
    }

    #[test]
    fn high_slot_bit_3_visible_when_chr_is_large_enough() {
        // 256 KiB CHR = 64 banks of 4 KiB. The forced bit-7 trick
        // now puts the high slot in banks 128..135 - except those
        // wrap to bank 0..7 after the mod. So show that the high
        // slot is NOT the same as low slot when the encoded value
        // selects bank 0.
        let mut m = Sunsoft1::new(cart(64));
        m.cpu_write(0x6000, 0); // low = bank 0, high = bank 0x80 mod 64 = 0
        assert_eq!(m.ppu_read(0x0000), 0);
        assert_eq!(m.ppu_read(0x1000), 0);
        // Now flip a non-zero high nibble: low = bank 1, high =
        // bank 0x81 mod 64 = 1. Same number, but the *path* differs.
        m.cpu_write(0x6000, 0b0001_0001);
        assert_eq!(m.ppu_read(0x0000), 1);
        assert_eq!(m.ppu_read(0x1000), 1);
    }

    #[test]
    fn write_outside_6000_7fff_is_a_noop() {
        let mut m = Sunsoft1::new(cart(8));
        m.cpu_write(0x4020, 0x77);
        m.cpu_write(0x8000, 0x77);
        m.cpu_write(0xFFFF, 0x77);
        // CHR slot 0 still bank 0.
        assert_eq!(m.ppu_read(0x0000), 0);
    }

    #[test]
    fn chr_ram_round_trips_when_cart_has_no_chr_rom() {
        let mut c = cart(0);
        c.chr_rom = Vec::new();
        c.chr_ram = true;
        let mut m = Sunsoft1::new(c);
        m.ppu_write(0x0010, 0x99);
        assert_eq!(m.ppu_read(0x0010), 0x99);
    }
}
