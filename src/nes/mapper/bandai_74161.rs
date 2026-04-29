// SPDX-License-Identifier: GPL-3.0-or-later
//! Bandai 74*161/161/32 - iNES mappers 70 (variant A) and 152
//! (variant B).
//!
//! A discrete-TTL board built from two 74*161 latches, one 74*32
//! (quad OR), and an external CHR-ROM. The chip has no on-die data
//! drivers, so writes are subject to a real-hardware **bus
//! conflict**: the value latched is `cpu_byte AND rom_byte_at_addr`.
//! Most retail games avoid the issue by writing values that match
//! the ROM byte under the latch (or write to an `$FF` byte where the
//! AND is a no-op), but a handful rely on it.
//!
//! ## Carts
//!
//! - **Mapper 70** (no mirroring control; cart-fixed): *Family
//!   Trainer 1-7*, *Famicom Jump*, *Kamen no Ninja Hanamaru*,
//!   *Kamen Rider Club: Gekitotsu Shocker Land*, *Family School
//!   Aerobic Studio* etc. (Bandai-licensed Famicom titles 1986-89.)
//! - **Mapper 152** (single-screen mirroring controlled by bit 7):
//!   *Saint Seiya: Ougon Densetsu Kanketsu Hen*, *Pocket Zaurus:
//!   Juu Ouken no Nazo*, *Famicom Tigers no Kessho-ban*, *Arkanoid
//!   II*.
//!
//! ## Register surface
//!
//! Single 8-bit latch decoded across `$8000-$FFFF`:
//!
//! ```text
//! 7  bit  0
//! ---- ----
//! MPPP CCCC
//! |||| ||||
//! |||| ++++- 8 KiB CHR bank at PPU $0000-$1FFF
//! |+++------ 16 KiB PRG bank at CPU $8000-$BFFF (high half fixed last)
//! +--------- Mirroring (mapper 152 only): 0 = screen A, 1 = screen B
//! ```
//!
//! Mapper 70 ignores bit 7 by default (header determines mirroring).
//! Mesen2's clean trick: if a mapper-70 game *ever* writes bit 7
//! high we auto-promote to single-screen control - some bad iNES
//! dumps (Kamen Rider Club) need this to play correctly without a
//! game-DB override.
//!
//! References:
//! - <https://www.nesdev.org/wiki/INES_Mapper_070>
//! - <https://www.nesdev.org/wiki/INES_Mapper_152>
//! - `~/Git/Mesen2/Core/NES/Mappers/Bandai/Bandai74161_7432.h`
//! - `~/Git/punes/src/core/mappers/mapper_070.c` (bus-conflict model)
//! - `~/Git/nestopia/source/core/board/NstBoardDiscrete.cpp`
//!   (`Ic74x161x161x32`)

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_16K: usize = 16 * 1024;
const CHR_BANK_8K: usize = 8 * 1024;

pub struct Bandai74161 {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,

    /// Latched register value (post bus-conflict AND).
    reg: u8,

    /// True for mapper 152 from boot, OR for a mapper-70 cart that
    /// has ever written bit 7 = 1. Once enabled it stays enabled.
    mirroring_control: bool,

    /// Effective mirroring; static when `mirroring_control` is
    /// false (cart-fixed), single-screen-A/B otherwise.
    mirroring: Mirroring,

    prg_bank_count_16k: usize,
    chr_bank_count_8k: usize,
}

impl Bandai74161 {
    pub fn new_70(cart: Cartridge) -> Self {
        Self::new(cart, false)
    }

    pub fn new_152(cart: Cartridge) -> Self {
        Self::new(cart, true)
    }

    fn new(cart: Cartridge, mirroring_control: bool) -> Self {
        let prg_bank_count_16k = (cart.prg_rom.len() / PRG_BANK_16K).max(1);

        let is_chr_ram = cart.chr_ram || cart.chr_rom.is_empty();
        let chr = if is_chr_ram {
            vec![0u8; CHR_BANK_8K]
        } else {
            cart.chr_rom
        };
        let chr_bank_count_8k = (chr.len() / CHR_BANK_8K).max(1);

        // Mapper 152 boots in single-screen-A; mapper 70 keeps the
        // cart-fixed mirroring from the iNES header.
        let mirroring = if mirroring_control {
            Mirroring::SingleScreenLower
        } else {
            cart.mirroring
        };

        Self {
            prg_rom: cart.prg_rom,
            chr,
            chr_ram: is_chr_ram,
            reg: 0,
            mirroring_control,
            mirroring,
            prg_bank_count_16k,
            chr_bank_count_8k,
        }
    }

    fn prg_bank_index(&self) -> usize {
        ((self.reg >> 4) as usize) % self.prg_bank_count_16k
    }

    fn chr_bank_index(&self) -> usize {
        (self.reg as usize & 0x0F) % self.chr_bank_count_8k
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

impl Mapper for Bandai74161 {
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
        // Bus conflict: CPU and ROM both drive the bus, the latch
        // sees the open-collector AND of the two values. Mesen2
        // skips this; puNES and Nestopia model it - we follow the
        // hardware-true path.
        let rom_byte = self.cpu_prg_byte(addr);
        let effective = data & rom_byte;

        if !self.mirroring_control && (data & 0x80) != 0 {
            // Mesen2 heuristic: a mapper-70 game touching bit 7
            // probably wants mirroring control. Latch the upgrade
            // so we don't keep flipping back to header-mirroring.
            self.mirroring_control = true;
        }

        self.reg = effective;

        if self.mirroring_control {
            self.mirroring = if effective & 0x80 != 0 {
                Mirroring::SingleScreenUpper
            } else {
                Mirroring::SingleScreenLower
            };
        }
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
        use crate::save_state::mapper::{Bandai74161Snap, MirroringSnap};
        Some(crate::save_state::MapperState::Bandai74161(Bandai74161Snap {
            chr_ram_data: if self.chr_ram { self.chr.clone() } else { Vec::new() },
            reg: self.reg,
            mirroring_control: self.mirroring_control,
            mirroring: MirroringSnap::from_live(self.mirroring),
        }))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::Bandai74161(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        if self.chr_ram && snap.chr_ram_data.len() == self.chr.len() {
            self.chr.copy_from_slice(&snap.chr_ram_data);
        }
        self.reg = snap.reg;
        self.mirroring_control = snap.mirroring_control;
        self.mirroring = snap.mirroring.to_live();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, Mirroring, TvSystem};

    /// 128 KiB PRG (8 banks of 16 KiB), 128 KiB CHR (16 banks of 8
    /// KiB). Each bank's first byte = its bank number; everything
    /// else = $FF so bus-conflict tests have a clean "no AND-down"
    /// path.
    fn cart(prg_banks: usize, chr_banks: usize, mirroring: Mirroring) -> Cartridge {
        assert!(prg_banks.is_power_of_two());
        let mut prg = vec![0xFFu8; prg_banks * PRG_BANK_16K];
        for bank in 0..prg_banks {
            prg[bank * PRG_BANK_16K] = bank as u8;
        }
        let mut chr = vec![0xFFu8; chr_banks * CHR_BANK_8K];
        for bank in 0..chr_banks {
            chr[bank * CHR_BANK_8K] = bank as u8;
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: chr,
            chr_ram: false,
            mapper_id: 70,
            submapper: 0,
            mirroring,
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
    fn power_on_layout_has_bank0_at_8000_and_last_at_c000() {
        let m = Bandai74161::new_70(cart(8, 16, Mirroring::Vertical));
        assert_eq!(m.cpu_peek(0x8000), 0); // bank 0
        assert_eq!(m.cpu_peek(0xC000), 7); // last (bank 7)
    }

    #[test]
    fn write_swaps_prg_and_chr_banks() {
        let mut m = Bandai74161::new_70(cart(8, 16, Mirroring::Vertical));
        // bits 4-6 select PRG bank 5, bits 0-3 select CHR bank 9.
        // Use $FFFF as the write target so bus conflict ANDs with
        // $FF (a no-op).
        m.cpu_write(0xFFFF, 0x59);
        assert_eq!(m.cpu_peek(0x8000), 5);
        assert_eq!(m.ppu_read(0x0000), 9);
        // Last bank stays fixed.
        assert_eq!(m.cpu_peek(0xC000), 7);
    }

    #[test]
    fn bus_conflict_ands_value_with_rom_byte() {
        let mut m = Bandai74161::new_70(cart(8, 16, Mirroring::Vertical));
        // First put bank 0 at $8000. The byte at $8000 is bank 0's
        // tag = $00. Now write $59 at $8000: ROM byte is $00, so
        // effective = $59 AND $00 = $00. The latch should hold
        // $00 (no-op switch back to bank 0).
        m.cpu_write(0xFFFF, 0x59); // first switch: ROM byte at $FFFF = $FF, no AND
        assert_eq!(m.cpu_peek(0x8000), 5);
        // The byte at $8000 in bank 5 = bank tag $05.
        m.cpu_write(0x8000, 0x77); // ROM byte = 5; 0x77 & 0x05 = 0x05
        // Latch is now 0x05: PRG bank = 0, CHR bank = 5.
        assert_eq!(m.cpu_peek(0x8000), 0);
        assert_eq!(m.ppu_read(0x0000), 5);
    }

    #[test]
    fn mapper_70_keeps_header_mirroring_until_bit7_seen() {
        let mut m = Bandai74161::new_70(cart(8, 16, Mirroring::Horizontal));
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
        // Write without bit 7 - mirroring unchanged.
        m.cpu_write(0xFFFF, 0x10);
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
        // Bit-7 write auto-promotes to single-screen control.
        m.cpu_write(0xFFFF, 0x80);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenUpper);
        // From here on bit 7 picks the page and we never go back to
        // header-static mirroring.
        m.cpu_write(0xFFFF, 0x00);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenLower);
        m.cpu_write(0xFFFF, 0x80);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenUpper);
    }

    #[test]
    fn mapper_152_drives_mirroring_from_bit7_immediately() {
        let mut m = Bandai74161::new_152(cart(8, 16, Mirroring::Horizontal));
        assert_eq!(m.mirroring(), Mirroring::SingleScreenLower);
        m.cpu_write(0xFFFF, 0x80);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenUpper);
        m.cpu_write(0xFFFF, 0x00);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenLower);
    }

    #[test]
    fn write_outside_8000_ffff_is_a_noop() {
        let mut m = Bandai74161::new_70(cart(8, 16, Mirroring::Vertical));
        m.cpu_write(0x6000, 0x59);
        m.cpu_write(0x4020, 0x59);
        assert_eq!(m.cpu_peek(0x8000), 0); // still bank 0
    }

    #[test]
    fn chr_ram_round_trips_on_carts_without_chr_rom() {
        let mut c = cart(8, 1, Mirroring::Vertical);
        c.chr_rom = Vec::new();
        c.chr_ram = true;
        let mut m = Bandai74161::new_70(c);
        m.ppu_write(0x0010, 0x99);
        assert_eq!(m.ppu_read(0x0010), 0x99);
    }
}
