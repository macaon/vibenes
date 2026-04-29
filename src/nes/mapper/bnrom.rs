// SPDX-License-Identifier: GPL-3.0-or-later
//! BNROM / NINA-001 - iNES mapper 34 (two distinct chips share
//! the mapper number).
//!
//! - **BNROM** (submapper 2): bare 32 KiB PRG bank-switcher with
//!   CHR-RAM. Single register decoded across `$8000-$FFFF` with
//!   bus conflicts. Originally a Nintendo discrete board; the
//!   licensed retail copy is *Deadly Towers* (Brøderbund USA, 1986)
//!   and a JP variant (Mashō).
//! - **NINA-001** (submapper 1): later AVE/Bunch board with three
//!   registers in cart-RAM space (`$7FFD`-`$7FFF`): one 32 KiB PRG
//!   bank plus two 4 KiB CHR banks. Used by *Impossible Mission II*,
//!   *Wayne's World*, and a handful of other AVE titles.
//!
//! ## Auto-detection
//!
//! NES 2.0 dumps carry an explicit submapper. iNES-1.0 dumps need
//! a heuristic: a cart with CHR-ROM is NINA-001; a CHR-RAM cart
//! is BNROM. Mesen2 and puNES both follow this rule and we copy
//! it here.
//!
//! ## Register surface
//!
//! ### Submapper 2 (BNROM)
//!
//! ```text
//! $8000-$FFFF  PRG bank (32 KiB), bus-conflict ANDed
//! ```
//!
//! ### Submapper 1 (NINA-001)
//!
//! ```text
//! $7FFD  PRG bank (32 KiB)
//! $7FFE  CHR bank 0 (4 KiB at PPU $0000-$0FFF)
//! $7FFF  CHR bank 1 (4 KiB at PPU $1000-$1FFF)
//! ```
//!
//! NINA-001 reads at `$7FFD-$7FFF` are normal PRG-RAM reads (the
//! cart maps 8 KiB of PRG-RAM at `$6000-$7FFF`); only writes to
//! the magic three addresses are intercepted by the chip.
//!
//! References:
//! - <https://www.nesdev.org/wiki/INES_Mapper_034>
//! - `~/Git/Mesen2/Core/NES/Mappers/Irem/BnRom.h` (BNROM only;
//!   Mesen2 files NINA-001 elsewhere)
//! - `~/Git/punes/src/core/mappers/mapper_034.c` (auto-detect +
//!   bus-conflict handling for both submappers)
//! - `~/Git/nestopia/source/core/board/NstBoardAveNina.cpp`
//!   (`Nina001::SubReset`)

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_32K: usize = 32 * 1024;
const CHR_BANK_4K: usize = 4 * 1024;
const CHR_BANK_8K: usize = 8 * 1024;
const PRG_RAM_SIZE: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Variant {
    /// Submapper 2: PRG-bank-only register at `$8000-$FFFF`,
    /// bus conflicts, CHR-RAM.
    Bnrom,
    /// Submapper 1: three registers at `$7FFD-$7FFF`, CHR-ROM
    /// with two 4 KiB banks.
    Nina001,
}

pub struct Bnrom {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,
    prg_ram: Vec<u8>,

    variant: Variant,

    /// PRG bank index (32 KiB).
    prg_bank: u8,
    /// CHR bank registers. `[0]` = PPU $0000-$0FFF; `[1]` =
    /// $1000-$1FFF. Used only on NINA-001.
    chr_banks: [u8; 2],

    mirroring: Mirroring,

    prg_bank_count_32k: usize,
    chr_bank_count_4k: usize,

    battery: bool,
    save_dirty: bool,
}

impl Bnrom {
    pub fn new(cart: Cartridge) -> Self {
        let variant = match cart.submapper {
            1 => Variant::Nina001,
            2 => Variant::Bnrom,
            // iNES 1.0 heuristic: CHR-ROM → NINA-001; CHR-RAM →
            // BNROM. Matches Mesen2 and puNES.
            _ => {
                if cart.chr_ram || cart.chr_rom.is_empty() {
                    Variant::Bnrom
                } else {
                    Variant::Nina001
                }
            }
        };

        let prg_bank_count_32k = (cart.prg_rom.len() / PRG_BANK_32K).max(1);

        let is_chr_ram = matches!(variant, Variant::Bnrom)
            || cart.chr_ram
            || cart.chr_rom.is_empty();
        let chr = if is_chr_ram {
            vec![0u8; CHR_BANK_8K]
        } else {
            cart.chr_rom
        };
        let chr_bank_count_4k = (chr.len() / CHR_BANK_4K).max(1);

        let prg_ram_total =
            (cart.prg_ram_size + cart.prg_nvram_size).max(PRG_RAM_SIZE);

        Self {
            prg_rom: cart.prg_rom,
            chr,
            chr_ram: is_chr_ram,
            prg_ram: vec![0u8; prg_ram_total],
            variant,
            prg_bank: 0,
            chr_banks: [0, 1],
            mirroring: cart.mirroring,
            prg_bank_count_32k,
            chr_bank_count_4k,
            battery: cart.battery_backed,
            save_dirty: false,
        }
    }

    fn prg_byte(&self, addr: u16) -> u8 {
        let off = (addr - 0x8000) as usize;
        let bank = (self.prg_bank as usize) % self.prg_bank_count_32k;
        let base = bank * PRG_BANK_32K;
        *self.prg_rom.get(base + off).unwrap_or(&0)
    }

    fn chr_slot_base(&self, slot: usize) -> usize {
        let bank = (self.chr_banks[slot] as usize) % self.chr_bank_count_4k;
        bank * CHR_BANK_4K
    }
}

impl Mapper for Bnrom {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => {
                let i = (addr - 0x6000) as usize;
                *self.prg_ram.get(i).unwrap_or(&0)
            }
            0x8000..=0xFFFF => self.prg_byte(addr),
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        match (self.variant, addr) {
            (Variant::Nina001, 0x7FFD) => self.prg_bank = data,
            (Variant::Nina001, 0x7FFE) => self.chr_banks[0] = data,
            (Variant::Nina001, 0x7FFF) => self.chr_banks[1] = data,
            (Variant::Nina001, 0x6000..=0x7FFC) => {
                // Plain PRG-RAM write below the magic registers.
                let i = (addr - 0x6000) as usize;
                if let Some(slot) = self.prg_ram.get_mut(i) {
                    if *slot != data {
                        *slot = data;
                        if self.battery {
                            self.save_dirty = true;
                        }
                    }
                }
            }
            (Variant::Bnrom, 0x6000..=0x7FFF) => {
                let i = (addr - 0x6000) as usize;
                if let Some(slot) = self.prg_ram.get_mut(i) {
                    if *slot != data {
                        *slot = data;
                        if self.battery {
                            self.save_dirty = true;
                        }
                    }
                }
            }
            (Variant::Bnrom, 0x8000..=0xFFFF) => {
                // Bus conflict: AND with the visible ROM byte.
                let rom_byte = self.prg_byte(addr);
                self.prg_bank = data & rom_byte;
            }
            _ => {}
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr < 0x2000 {
            let slot = (addr >> 12) as usize;
            let off = (addr & 0x0FFF) as usize;
            *self.chr.get(self.chr_slot_base(slot) + off).unwrap_or(&0)
        } else {
            0
        }
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if self.chr_ram && addr < 0x2000 {
            let slot = (addr >> 12) as usize;
            let off = (addr & 0x0FFF) as usize;
            let base = self.chr_slot_base(slot);
            if let Some(b) = self.chr.get_mut(base + off) {
                *b = data;
            }
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }

    fn save_data(&self) -> Option<&[u8]> {
        self.battery.then(|| self.prg_ram.as_slice())
    }

    fn load_save_data(&mut self, data: &[u8]) {
        if self.battery && data.len() == self.prg_ram.len() {
            self.prg_ram.copy_from_slice(data);
        }
    }

    fn save_dirty(&self) -> bool {
        self.save_dirty
    }

    fn mark_saved(&mut self) {
        self.save_dirty = false;
    }

    fn save_state_capture(&self) -> Option<crate::save_state::MapperState> {
        use crate::save_state::mapper::BnromSnap;
        Some(crate::save_state::MapperState::Bnrom(BnromSnap {
            prg_ram: self.prg_ram.clone(),
            chr_ram_data: if self.chr_ram { self.chr.clone() } else { Vec::new() },
            prg_bank: self.prg_bank,
            chr_banks: self.chr_banks,
            nina001: self.variant == Variant::Nina001,
            save_dirty: self.save_dirty,
        }))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::Bnrom(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        let live_nina = self.variant == Variant::Nina001;
        if snap.nina001 != live_nina {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        }
        if snap.prg_ram.len() == self.prg_ram.len() {
            self.prg_ram.copy_from_slice(&snap.prg_ram);
        }
        if self.chr_ram && snap.chr_ram_data.len() == self.chr.len() {
            self.chr.copy_from_slice(&snap.chr_ram_data);
        }
        self.prg_bank = snap.prg_bank;
        self.chr_banks = snap.chr_banks;
        self.save_dirty = snap.save_dirty;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, Mirroring, TvSystem};

    /// 256 KiB PRG (8 banks of 32 KiB), submapper-controlled CHR.
    /// Each PRG bank tagged in its first byte; everything else
    /// `$FF` for clean bus-conflict tests.
    fn cart(submapper: u8, with_chr_rom: bool) -> Cartridge {
        let mut prg = vec![0xFFu8; 8 * PRG_BANK_32K];
        for bank in 0..8 {
            prg[bank * PRG_BANK_32K] = bank as u8;
        }
        let (chr_rom, chr_ram) = if with_chr_rom {
            let mut chr = vec![0xFFu8; 16 * CHR_BANK_4K]; // 64 KiB CHR
            for bank in 0..16 {
                chr[bank * CHR_BANK_4K] = bank as u8;
            }
            (chr, false)
        } else {
            (Vec::new(), true)
        };
        Cartridge {
            prg_rom: prg,
            chr_rom,
            chr_ram,
            mapper_id: 34,
            submapper,
            mirroring: Mirroring::Vertical,
            battery_backed: false,
            prg_ram_size: 0x2000,
            prg_nvram_size: 0,
            tv_system: TvSystem::Ntsc,
            is_nes2: true,
            prg_chr_crc32: 0,
            db_matched: false,
            fds_data: None,
        }
    }

    #[test]
    fn auto_detect_bnrom_when_chr_ram() {
        let m = Bnrom::new(cart(0, false));
        assert_eq!(m.variant, Variant::Bnrom);
    }

    #[test]
    fn auto_detect_nina001_when_chr_rom_present() {
        let m = Bnrom::new(cart(0, true));
        assert_eq!(m.variant, Variant::Nina001);
    }

    #[test]
    fn explicit_submapper_overrides_heuristic() {
        // Force NINA-001 even with CHR-RAM cart.
        let m = Bnrom::new(cart(1, false));
        assert_eq!(m.variant, Variant::Nina001);
        // Force BNROM even with CHR-ROM cart.
        let m = Bnrom::new(cart(2, true));
        assert_eq!(m.variant, Variant::Bnrom);
    }

    #[test]
    fn bnrom_writes_swap_32k_with_bus_conflict() {
        let mut m = Bnrom::new(cart(2, false));
        // Boot: bank 0 at $8000.
        assert_eq!(m.cpu_peek(0x8000), 0);
        // Write at $FFFF (ROM byte = $FF) → no AND, latch = 5.
        m.cpu_write(0xFFFF, 5);
        assert_eq!(m.cpu_peek(0x8000), 5);
        // Write at $8000 (ROM byte in bank 5 = $05) → 0x07 & 0x05
        // = 0x05 → latch stays at 5.
        m.cpu_write(0x8000, 0x07);
        assert_eq!(m.cpu_peek(0x8000), 5);
    }

    #[test]
    fn nina001_writes_to_7ffd_swap_prg() {
        let mut m = Bnrom::new(cart(1, true));
        assert_eq!(m.cpu_peek(0x8000), 0);
        m.cpu_write(0x7FFD, 3);
        assert_eq!(m.cpu_peek(0x8000), 3);
    }

    #[test]
    fn nina001_writes_to_7ffe_and_7fff_swap_chr_slots() {
        let mut m = Bnrom::new(cart(1, true));
        // Boot: chr_banks = [0, 1]
        assert_eq!(m.ppu_read(0x0000), 0);
        assert_eq!(m.ppu_read(0x1000), 1);
        m.cpu_write(0x7FFE, 5);
        m.cpu_write(0x7FFF, 9);
        assert_eq!(m.ppu_read(0x0000), 5);
        assert_eq!(m.ppu_read(0x1000), 9);
    }

    #[test]
    fn nina001_writes_to_other_prg_ram_addresses_pass_through() {
        let mut m = Bnrom::new(cart(1, true));
        m.cpu_write(0x6000, 0x42);
        assert_eq!(m.cpu_peek(0x6000), 0x42);
        m.cpu_write(0x7FFC, 0x55);
        assert_eq!(m.cpu_peek(0x7FFC), 0x55);
        // The magic registers don't appear as PRG-RAM reads.
    }

    #[test]
    fn bnrom_writes_to_8000_dont_affect_prg_ram() {
        let mut m = Bnrom::new(cart(2, false));
        m.cpu_write(0x8000, 5);
        assert_eq!(m.cpu_peek(0x6000), 0); // PRG-RAM untouched
    }

    #[test]
    fn save_state_rejects_cross_variant_restore() {
        let bnrom = Bnrom::new(cart(2, false));
        let snap = bnrom.save_state_capture().unwrap();
        let mut nina = Bnrom::new(cart(1, true));
        match nina.save_state_apply(&snap) {
            Err(crate::save_state::SaveStateError::UnsupportedMapper(_)) => {}
            other => panic!("expected UnsupportedMapper, got {other:?}"),
        }
    }
}
