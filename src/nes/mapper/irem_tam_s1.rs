// SPDX-License-Identifier: GPL-3.0-or-later
//! Irem TAM-S1 - iNES mapper 97.
//!
//! Single Irem-licensed Famicom cart: *Kaiketsu Yanchamaru* (1986).
//! Custom Irem ASIC with one register decoded across `$8000-$FFFF`.
//! Notable feature is the inverted PRG layout: the fixed (last)
//! 16 KiB bank lives at `$8000-$BFFF`, with the switchable bank at
//! `$C000-$FFFF`.
//!
//! ## Register surface
//!
//! ```text
//! 7  bit  0
//! ---- ----
//! MM.. PPPP
//! ||   ||||
//! ||   ++++- 16 KiB PRG bank at $C000-$FFFF
//! ++-------- Mirroring (submapper-dependent; see below)
//! ```
//!
//! ## Submapper variants
//!
//! NESdev wiki splits the mirroring decode between submappers:
//!
//! - **Submapper 0** (default for non-NES-2.0 dumps): bit 7 only;
//!   0 = horizontal, 1 = vertical. puNES models this.
//! - **Submapper 1**: bits 7-6; 0 = single-screen A, 1 = horizontal,
//!   2 = vertical, 3 = single-screen B. Mesen2 models this for
//!   every dump, but the wiki gates it on submapper 1.
//!
//! We follow the wiki: 2-mode by default, 4-mode only when the
//! NES 2.0 header explicitly says submapper 1. The single known
//! retail cart (Kaiketsu Yanchamaru) uses 2-mode mirroring.
//!
//! References:
//! - <https://www.nesdev.org/wiki/INES_Mapper_097>
//! - `~/Git/Mesen2/Core/NES/Mappers/Irem/IremTamS1.h` (4-mode
//!   path; matches our submapper-1 branch)
//! - `~/Git/punes/src/core/mappers/mapper_097.c` (2-mode path;
//!   matches our submapper-0 branch)

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_16K: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MirrorMode {
    /// Submapper 0: bit 7 only - horizontal / vertical.
    TwoMode,
    /// Submapper 1: bits 7-6 - single-screen A, horizontal,
    /// vertical, single-screen B.
    FourMode,
}

pub struct IremTamS1 {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,

    reg: u8,

    mirror_mode: MirrorMode,
    mirroring: Mirroring,

    prg_bank_count_16k: usize,
}

impl IremTamS1 {
    pub fn new(cart: Cartridge) -> Self {
        let mirror_mode = if cart.submapper == 1 {
            MirrorMode::FourMode
        } else {
            MirrorMode::TwoMode
        };

        let prg_bank_count_16k = (cart.prg_rom.len() / PRG_BANK_16K).max(1);

        let is_chr_ram = cart.chr_ram || cart.chr_rom.is_empty();
        let chr = if is_chr_ram {
            vec![0u8; 8 * 1024]
        } else {
            cart.chr_rom
        };

        // Boot mirroring: register = 0.
        // - 2-mode → bit 7 clear → horizontal.
        // - 4-mode → bits 7-6 clear → single-screen A (per Mesen).
        let mirroring = match mirror_mode {
            MirrorMode::TwoMode => Mirroring::Horizontal,
            MirrorMode::FourMode => Mirroring::SingleScreenLower,
        };

        Self {
            prg_rom: cart.prg_rom,
            chr,
            chr_ram: is_chr_ram,
            reg: 0,
            mirror_mode,
            mirroring,
            prg_bank_count_16k,
        }
    }

    fn last_prg_bank(&self) -> usize {
        self.prg_bank_count_16k - 1
    }

    fn switchable_prg_bank(&self) -> usize {
        (self.reg & 0x0F) as usize % self.prg_bank_count_16k
    }
}

impl Mapper for IremTamS1 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
            0x8000..=0xBFFF => {
                let off = (addr - 0x8000) as usize;
                let base = self.last_prg_bank() * PRG_BANK_16K;
                *self.prg_rom.get(base + off).unwrap_or(&0)
            }
            0xC000..=0xFFFF => {
                let off = (addr - 0xC000) as usize;
                let base = self.switchable_prg_bank() * PRG_BANK_16K;
                *self.prg_rom.get(base + off).unwrap_or(&0)
            }
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        if !(0x8000..=0xFFFF).contains(&addr) {
            return;
        }
        self.reg = data;
        self.mirroring = match self.mirror_mode {
            MirrorMode::TwoMode => {
                if data & 0x80 != 0 {
                    Mirroring::Vertical
                } else {
                    Mirroring::Horizontal
                }
            }
            MirrorMode::FourMode => match data >> 6 {
                0 => Mirroring::SingleScreenLower,
                1 => Mirroring::Horizontal,
                2 => Mirroring::Vertical,
                _ => Mirroring::SingleScreenUpper,
            },
        };
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
            if let Some(b) = self.chr.get_mut(addr as usize) {
                *b = data;
            }
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }

    fn save_state_capture(&self) -> Option<crate::save_state::MapperState> {
        use crate::save_state::mapper::{IremTamS1Snap, MirroringSnap};
        Some(crate::save_state::MapperState::IremTamS1(IremTamS1Snap {
            chr_ram_data: if self.chr_ram { self.chr.clone() } else { Vec::new() },
            reg: self.reg,
            mirroring: MirroringSnap::from_live(self.mirroring),
            four_mode: self.mirror_mode == MirrorMode::FourMode,
        }))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::IremTamS1(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        let live_four = self.mirror_mode == MirrorMode::FourMode;
        if snap.four_mode != live_four {
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

    /// 256 KiB PRG (16 banks of 16 KiB), 8 KiB CHR-RAM. Each PRG
    /// bank tagged in its first byte; remaining bytes = $FF.
    fn cart(submapper: u8) -> Cartridge {
        let mut prg = vec![0xFFu8; 16 * PRG_BANK_16K];
        for bank in 0..16 {
            prg[bank * PRG_BANK_16K] = bank as u8;
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: Vec::new(),
            chr_ram: true,
            mapper_id: 97,
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
    fn power_on_layout_has_last_at_8000_and_bank0_at_c000() {
        let m = IremTamS1::new(cart(0));
        // Inverted layout: $8000-$BFFF = last 16 KiB; $C000-$FFFF
        // = switchable, defaults to bank 0.
        assert_eq!(m.cpu_peek(0x8000), 15);
        assert_eq!(m.cpu_peek(0xC000), 0);
    }

    #[test]
    fn switchable_bank_at_c000_uses_low_4_bits() {
        let mut m = IremTamS1::new(cart(0));
        m.cpu_write(0x8000, 0x07);
        assert_eq!(m.cpu_peek(0xC000), 7);
        assert_eq!(m.cpu_peek(0x8000), 15, "fixed slot unchanged");
        m.cpu_write(0xFFFF, 0x0E);
        assert_eq!(m.cpu_peek(0xC000), 14);
    }

    #[test]
    fn submapper_0_two_mode_mirroring() {
        let mut m = IremTamS1::new(cart(0));
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
        m.cpu_write(0xFFFF, 0x80);
        assert_eq!(m.mirroring(), Mirroring::Vertical);
        m.cpu_write(0xFFFF, 0x00);
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
    }

    #[test]
    fn submapper_1_four_mode_mirroring() {
        let mut m = IremTamS1::new(cart(1));
        // Boot: bits 7-6 = 0 → single-screen lower (A).
        assert_eq!(m.mirroring(), Mirroring::SingleScreenLower);
        m.cpu_write(0xFFFF, 0x40); // bits = 01 → horizontal
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
        m.cpu_write(0xFFFF, 0x80); // bits = 10 → vertical
        assert_eq!(m.mirroring(), Mirroring::Vertical);
        m.cpu_write(0xFFFF, 0xC0); // bits = 11 → single-screen upper (B)
        assert_eq!(m.mirroring(), Mirroring::SingleScreenUpper);
        m.cpu_write(0xFFFF, 0x00);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenLower);
    }

    #[test]
    fn writes_below_8000_are_noop() {
        let mut m = IremTamS1::new(cart(0));
        m.cpu_write(0x4020, 0x07);
        m.cpu_write(0x6000, 0x07);
        m.cpu_write(0x7FFF, 0x07);
        assert_eq!(m.cpu_peek(0xC000), 0);
    }

    #[test]
    fn save_state_rejects_cross_submapper_restore() {
        let two = IremTamS1::new(cart(0));
        let snap = two.save_state_capture().unwrap();
        let mut four = IremTamS1::new(cart(1));
        match four.save_state_apply(&snap) {
            Err(crate::save_state::SaveStateError::UnsupportedMapper(_)) => {}
            other => panic!("expected UnsupportedMapper, got {other:?}"),
        }
    }

    #[test]
    fn chr_ram_round_trips() {
        let mut m = IremTamS1::new(cart(0));
        m.ppu_write(0x0010, 0x99);
        assert_eq!(m.ppu_read(0x0010), 0x99);
    }
}
