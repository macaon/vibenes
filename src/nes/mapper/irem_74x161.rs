// SPDX-License-Identifier: GPL-3.0-or-later
//! Irem 74*161 / Jaleco JF-16 - iNES mapper 78.
//!
//! Discrete-TTL board shared between Irem's *Holy Diver* (Famicom)
//! and Jaleco/Irem's *Cosmo Carrier*. Same chip with two
//! different mirroring wirings - the iNES 1.0 mapper number can't
//! distinguish them, so NES 2.0 submappers carry the disambiguator:
//!
//! - **Submapper 1** (default for non-NES-2.0 dumps): Cosmo Carrier
//!   wiring; bit 3 picks single-screen page A or B.
//! - **Submapper 3**: Holy Diver wiring; bit 3 picks horizontal or
//!   vertical mirroring.
//!
//! Bus conflict applies (open-collector AND of CPU value with the
//! visible ROM byte). All three reference emulators model it.
//!
//! ## Register surface
//!
//! Single 8-bit latch decoded across `$8000-$FFFF`:
//!
//! ```text
//! 7  bit  0
//! ---- ----
//! CCCC MPPP
//! |||| ||||
//! |||| |+++- 16 KiB PRG bank at $8000-$BFFF (last 16 KiB fixed at $C000)
//! |||| +---- Mirroring (submapper-dependent; see above)
//! ++++------ 8 KiB CHR bank at PPU $0000-$1FFF
//! ```
//!
//! References:
//! - <https://www.nesdev.org/wiki/INES_Mapper_078>
//! - `~/Git/Mesen2/Core/NES/Mappers/Jaleco/JalecoJf16.h` (Mesen2
//!   files mapper 78 under Jaleco; submapper 3 path is Holy Diver)
//! - `~/Git/punes/src/core/mappers/mapper_078.c` (bus conflict)
//! - `~/Git/nestopia/source/core/board/NstBoardIremHolyDiver.cpp`

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_16K: usize = 16 * 1024;
const CHR_BANK_8K: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MirrorMode {
    /// Submapper 1 (Cosmo Carrier): single-screen A/B.
    SingleScreen,
    /// Submapper 3 (Holy Diver): horizontal / vertical.
    HorizontalVertical,
}

pub struct Irem74x161 {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,

    reg: u8,

    mirror_mode: MirrorMode,
    mirroring: Mirroring,

    prg_bank_count_16k: usize,
    chr_bank_count_8k: usize,
}

impl Irem74x161 {
    pub fn new(cart: Cartridge) -> Self {
        // Submapper 3 = Holy Diver (H/V); everything else (incl.
        // submapper 0 / 1 from non-NES-2.0 dumps) defaults to the
        // Cosmo Carrier single-screen wiring, matching Mesen2 and
        // puNES.
        let mirror_mode = if cart.submapper == 3 {
            MirrorMode::HorizontalVertical
        } else {
            MirrorMode::SingleScreen
        };

        let prg_bank_count_16k = (cart.prg_rom.len() / PRG_BANK_16K).max(1);

        let is_chr_ram = cart.chr_ram || cart.chr_rom.is_empty();
        let chr = if is_chr_ram {
            vec![0u8; CHR_BANK_8K]
        } else {
            cart.chr_rom
        };
        let chr_bank_count_8k = (chr.len() / CHR_BANK_8K).max(1);

        // Boot mirroring: register is 0, bit 3 clear. For SingleScreen
        // wiring that is page A (lower); for H/V it is horizontal.
        let mirroring = match mirror_mode {
            MirrorMode::SingleScreen => Mirroring::SingleScreenLower,
            MirrorMode::HorizontalVertical => Mirroring::Horizontal,
        };

        Self {
            prg_rom: cart.prg_rom,
            chr,
            chr_ram: is_chr_ram,
            reg: 0,
            mirror_mode,
            mirroring,
            prg_bank_count_16k,
            chr_bank_count_8k,
        }
    }

    fn last_prg_bank(&self) -> usize {
        self.prg_bank_count_16k - 1
    }

    fn prg_bank_index(&self) -> usize {
        (self.reg & 0x07) as usize % self.prg_bank_count_16k
    }

    fn chr_bank_index(&self) -> usize {
        ((self.reg >> 4) & 0x0F) as usize % self.chr_bank_count_8k
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

impl Mapper for Irem74x161 {
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
        // Bus conflict: AND with ROM byte at write address.
        let rom_byte = self.cpu_prg_byte(addr);
        let effective = data & rom_byte;
        self.reg = effective;

        let bit3 = effective & 0x08 != 0;
        self.mirroring = match (self.mirror_mode, bit3) {
            (MirrorMode::SingleScreen, false) => Mirroring::SingleScreenLower,
            (MirrorMode::SingleScreen, true) => Mirroring::SingleScreenUpper,
            (MirrorMode::HorizontalVertical, false) => Mirroring::Horizontal,
            (MirrorMode::HorizontalVertical, true) => Mirroring::Vertical,
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
        use crate::save_state::mapper::{Irem74x161Snap, MirroringSnap};
        Some(crate::save_state::MapperState::Irem74x161(Irem74x161Snap {
            chr_ram_data: if self.chr_ram { self.chr.clone() } else { Vec::new() },
            reg: self.reg,
            mirroring: MirroringSnap::from_live(self.mirroring),
            holy_diver_mode: self.mirror_mode == MirrorMode::HorizontalVertical,
        }))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::Irem74x161(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        let live_holy_diver = self.mirror_mode == MirrorMode::HorizontalVertical;
        if snap.holy_diver_mode != live_holy_diver {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        }
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

    /// 128 KiB PRG (8 banks of 16 KiB), 128 KiB CHR (16 banks of 8
    /// KiB). First byte of each bank tagged with the bank index;
    /// remaining bytes = $FF.
    fn cart(submapper: u8) -> Cartridge {
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
            mapper_id: 78,
            submapper,
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
    fn power_on_layout_is_bank0_then_last_for_both_submappers() {
        let m = Irem74x161::new(cart(1));
        assert_eq!(m.cpu_peek(0x8000), 0);
        assert_eq!(m.cpu_peek(0xC000), 7);
        let m = Irem74x161::new(cart(3));
        assert_eq!(m.cpu_peek(0x8000), 0);
        assert_eq!(m.cpu_peek(0xC000), 7);
    }

    #[test]
    fn cosmo_carrier_submapper_uses_single_screen_mirroring() {
        let mut m = Irem74x161::new(cart(1));
        assert_eq!(m.mirroring(), Mirroring::SingleScreenLower);
        m.cpu_write(0xFFFF, 0x08);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenUpper);
        m.cpu_write(0xFFFF, 0x00);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenLower);
    }

    #[test]
    fn holy_diver_submapper_uses_horizontal_vertical_mirroring() {
        let mut m = Irem74x161::new(cart(3));
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
        m.cpu_write(0xFFFF, 0x08);
        assert_eq!(m.mirroring(), Mirroring::Vertical);
        m.cpu_write(0xFFFF, 0x00);
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
    }

    #[test]
    fn prg_uses_low_3_bits_chr_uses_high_4_bits() {
        let mut m = Irem74x161::new(cart(3));
        // PRG bank = 5; CHR bank = 0xB (11). $FFFF byte is $FF
        // → bus conflict no-op.
        m.cpu_write(0xFFFF, 0xB5);
        assert_eq!(m.cpu_peek(0x8000), 5);
        assert_eq!(m.ppu_read(0x0000), 11);
        // Last slot stays fixed at the last bank.
        assert_eq!(m.cpu_peek(0xC000), 7);
    }

    #[test]
    fn bus_conflict_ands_value_with_rom_byte() {
        let mut m = Irem74x161::new(cart(3));
        // First swap to bank 5 via $FFFF (no AND).
        m.cpu_write(0xFFFF, 0x05);
        assert_eq!(m.cpu_peek(0x8000), 5);
        // Now write at $8000 where ROM byte = $05. CPU value $77
        // ANDs with $05 → $05 (PRG bank 5, CHR bank 0).
        m.cpu_write(0x8000, 0x77);
        assert_eq!(m.cpu_peek(0x8000), 5);
        assert_eq!(m.ppu_read(0x0000), 0);
    }

    #[test]
    fn writes_below_8000_are_noop() {
        let mut m = Irem74x161::new(cart(1));
        m.cpu_write(0x4020, 0xFF);
        m.cpu_write(0x6000, 0xFF);
        m.cpu_write(0x7FFF, 0xFF);
        assert_eq!(m.cpu_peek(0x8000), 0);
    }

    #[test]
    fn save_state_rejects_cross_submapper_restore() {
        let cosmo = Irem74x161::new(cart(1));
        let snap = cosmo.save_state_capture().unwrap();
        let mut holy = Irem74x161::new(cart(3));
        match holy.save_state_apply(&snap) {
            Err(crate::save_state::SaveStateError::UnsupportedMapper(_)) => {}
            other => panic!("expected UnsupportedMapper, got {other:?}"),
        }
    }
}
