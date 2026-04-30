// SPDX-License-Identifier: GPL-3.0-or-later
//! Color Dreams (iNES mapper 11) - 74'377 single-latch board.
//!
//! Discrete-logic chip used by Color Dreams' own commercial library
//! and Wisdom Tree's Christian-themed shareware line. Single 8-bit
//! latch at `$8000-$FFFF` packs both bank selects:
//!
//! ```text
//! 7  bit  0
//! ---- ----
//! CCCC PPPP
//! ||||  |||
//! ||||  +++- 32 KiB PRG bank at $8000-$FFFF (low 4 bits)
//! ++++------ 8 KiB CHR bank at $0000-$1FFF (high 4 bits)
//! ```
//!
//! - PRG window is fixed at 32 KiB; the low nibble of the latch
//!   selects up to 16 banks (512 KiB max - real Color Dreams chips
//!   only ever shipped up to 4 banks / 128 KiB, but keeping the full
//!   nibble lets oversized homebrew dumps work).
//! - CHR window is fixed at 8 KiB; the high nibble selects up to 16
//!   banks (128 KiB max - Wisdom Tree's largest carts top out at 64
//!   KiB / 8 banks).
//! - Mirroring is solder-set from the iNES header (no register).
//! - **Bus conflict** on the latch write: stored value =
//!   `cpu_data AND prg_rom[addr]`. Mesen2 and Nestopia both apply
//!   this; puNES omits it (a simplification, not a behavioral
//!   difference for any commercial cart since they all write values
//!   that already match the ROM byte).
//!
//! Commercial library (selection):
//! - **Color Dreams**: Menace Beach, Crystal Mines, Captain Comic,
//!   Solitaire, Chiller, Galactic Crusader, Master Chu and the
//!   Drunkard Hu, Operation Secret Storm, Pesterminator, Robodemons,
//!   Sidewinder, Silent Assault, Stakk M, Baby Boomer, Challenge of
//!   the Dragon, P'radikus Conflict, Raid 2020.
//! - **Wisdom Tree** (Christian-themed): Bible Adventures, Bible
//!   Buffet, Exodus, Joshua and the Battle of Jericho, King of
//!   Kings: The Early Years, Spiritual Warfare, Sunday Funday.
//! - **AGCI** (American Game Cartridges, also used the 74'377 board
//!   under a different label): Death Race lives on **mapper 144**
//!   instead - same chip with one extra 74HC08 gate that biases the
//!   bus-conflict bit 0.
//!
//! Clean-room references (behavioral only):
//! - `~/Git/Mesen2/Core/NES/Mappers/Unlicensed/ColorDreams.h`
//! - `~/Git/punes/src/core/mappers/mapper_011.c`
//! - `~/Git/nestopia/source/core/board/NstBoardDiscrete.cpp`
//!   (`Ic74x377`)
//! - nesdev.org/wiki/INES_Mapper_011, /Color_Dreams

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_32K: usize = 32 * 1024;
const CHR_BANK_8K: usize = 8 * 1024;

pub struct ColorDreams {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,
    mirroring: Mirroring,

    prg_bank_count_32k: usize,
    chr_bank_count_8k: usize,

    /// Combined latch byte: low nibble = PRG bank, high nibble = CHR bank.
    /// Stored as the post-bus-conflict value (data AND rom[addr]).
    reg: u8,
}

impl ColorDreams {
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
            chr,
            chr_ram: is_chr_ram,
            mirroring: cart.mirroring,
            prg_bank_count_32k,
            chr_bank_count_8k,
            reg: 0,
        }
    }

    fn prg_bank(&self) -> usize {
        ((self.reg & 0x0F) as usize) % self.prg_bank_count_32k
    }

    fn chr_bank(&self) -> usize {
        (((self.reg >> 4) & 0x0F) as usize) % self.chr_bank_count_8k
    }

    fn map_prg(&self, addr: u16) -> usize {
        self.prg_bank() * PRG_BANK_32K + ((addr - 0x8000) as usize)
    }

    fn map_chr(&self, addr: u16) -> usize {
        self.chr_bank() * CHR_BANK_8K + (addr as usize)
    }
}

impl Mapper for ColorDreams {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
            0x8000..=0xFFFF => {
                let i = self.map_prg(addr);
                *self.prg_rom.get(i).unwrap_or(&0)
            }
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        if !(0x8000..=0xFFFF).contains(&addr) {
            return;
        }
        // Bus-conflict gate: stored value = cpu_data AND rom[addr].
        // The latch index lookup uses the *current* mapping, not the
        // post-write one - matches Mesen2 / Nestopia (`GetBusData`
        // reads from the cart-mapped slot before the write commits).
        let i = self.map_prg(addr);
        let rom_byte = *self.prg_rom.get(i).unwrap_or(&0xFF);
        self.reg = data & rom_byte;
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr >= 0x2000 {
            return 0;
        }
        let i = self.map_chr(addr);
        *self.chr.get(i).unwrap_or(&0)
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if self.chr_ram && addr < 0x2000 {
            let i = self.map_chr(addr);
            if let Some(slot) = self.chr.get_mut(i) {
                *slot = data;
            }
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }

    fn save_state_capture(&self) -> Option<crate::save_state::MapperState> {
        use crate::save_state::mapper::{ColorDreamsSnap, MirroringSnap};
        Some(crate::save_state::MapperState::ColorDreams(ColorDreamsSnap {
            chr_ram_data: if self.chr_ram { self.chr.clone() } else { Vec::new() },
            mirroring: MirroringSnap::from_live(self.mirroring),
            reg: self.reg,
        }))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::ColorDreams(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        if self.chr_ram && snap.chr_ram_data.len() == self.chr.len() {
            self.chr.copy_from_slice(&snap.chr_ram_data);
        }
        self.mirroring = snap.mirroring.to_live();
        self.reg = snap.reg;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, TvSystem};

    /// 128 KiB PRG (4 banks * 32 KiB) all 0xFF except a tag byte at
    /// the start of each bank, plus 64 KiB CHR-ROM (8 banks * 8 KiB)
    /// also tagged. Tag layout:
    /// - PRG bank N: first byte = `0xA0 + N`, rest = 0xFF.
    /// - CHR bank N: first byte = `0xC0 + N`.
    fn cart() -> Cartridge {
        let mut prg = vec![0xFFu8; 4 * PRG_BANK_32K];
        for b in 0..4 {
            prg[b * PRG_BANK_32K] = 0xA0 + b as u8;
        }
        let mut chr = vec![0u8; 8 * CHR_BANK_8K];
        for b in 0..8 {
            chr[b * CHR_BANK_8K] = 0xC0 + b as u8;
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: chr,
            chr_ram: false,
            mapper_id: 11,
            submapper: 0,
            mirroring: Mirroring::Vertical,
            battery_backed: false,
            prg_ram_size: 0,
            prg_nvram_size: 0,
            tv_system: TvSystem::Ntsc,
            is_nes2: false,
            prg_chr_crc32: 0,
            db_matched: false,
            fds_data: None,
        }
    }

    fn m() -> ColorDreams {
        ColorDreams::new(cart())
    }

    #[test]
    fn power_on_layout_is_bank_zero_for_both_planes() {
        let mut m = m();
        assert_eq!(m.cpu_peek(0x8000), 0xA0);
        assert_eq!(m.ppu_read(0x0000), 0xC0);
    }

    #[test]
    fn low_nibble_picks_prg_bank() {
        let mut m = m();
        // Write at $8001 - ROM byte there is 0xFF, so bus conflict
        // is a no-op and the value passes through unchanged.
        m.cpu_write(0x8001, 0x02);
        assert_eq!(m.cpu_peek(0x8000), 0xA2);
        m.cpu_write(0x8001, 0x03);
        assert_eq!(m.cpu_peek(0x8000), 0xA3);
    }

    #[test]
    fn high_nibble_picks_chr_bank() {
        let mut m = m();
        // 0x50 → CHR bank 5, PRG bank 0. ROM[$8001] = 0xFF, no-op AND.
        m.cpu_write(0x8001, 0x50);
        assert_eq!(m.ppu_read(0x0000), 0xC5);
        // 0x70 → CHR bank 7.
        m.cpu_write(0x8001, 0x70);
        assert_eq!(m.ppu_read(0x0000), 0xC7);
    }

    #[test]
    fn nibble_combinations_select_independently() {
        let mut m = m();
        m.cpu_write(0x8001, 0x32);
        assert_eq!(m.cpu_peek(0x8000), 0xA2); // PRG = 2
        assert_eq!(m.ppu_read(0x0000), 0xC3); // CHR = 3
    }

    #[test]
    fn bus_conflict_masks_value_at_tag_byte_address() {
        let mut m = m();
        // Writing to $8000 sees ROM byte 0xA0 = 0b10100000.
        // Try to load PRG bank 0x03: 0x03 AND 0xA0 = 0x00, so bank
        // stays at 0. PRG window keeps reading bank 0.
        m.cpu_write(0x8000, 0x03);
        assert_eq!(m.cpu_peek(0x8000), 0xA0);
    }

    #[test]
    fn writes_below_8000_are_ignored() {
        let mut m = m();
        m.cpu_write(0x4020, 0xFF);
        m.cpu_write(0x6000, 0xFF);
        m.cpu_write(0x7FFF, 0xFF);
        assert_eq!(m.cpu_peek(0x8000), 0xA0);
        assert_eq!(m.ppu_read(0x0000), 0xC0);
    }

    #[test]
    fn chr_ram_round_trips_when_cart_has_no_chr_rom() {
        let mut cart = cart();
        cart.chr_rom = vec![];
        cart.chr_ram = true;
        let mut m = ColorDreams::new(cart);
        m.ppu_write(0x0123, 0x42);
        assert_eq!(m.ppu_read(0x0123), 0x42);
        // Bank-select writes don't index past the single 8 KiB RAM.
        m.cpu_write(0x8001, 0xF0);
        assert_eq!(m.ppu_read(0x0123), 0x42);
    }

    #[test]
    fn save_state_round_trip_preserves_reg_and_chr_ram() {
        let mut cart = cart();
        cart.chr_rom = vec![];
        cart.chr_ram = true;
        let mut a = ColorDreams::new(cart.clone());
        a.cpu_write(0x8001, 0x37);
        a.ppu_write(0x0010, 0xAA);
        let snap = a.save_state_capture().unwrap();

        let mut b = ColorDreams::new(cart);
        b.save_state_apply(&snap).unwrap();
        assert_eq!(b.reg, a.reg);
        assert_eq!(b.ppu_read(0x0010), 0xAA);
    }

    #[test]
    fn cross_variant_apply_rejected() {
        use crate::save_state::mapper::{NromSnap, MapperState};
        let mut m = m();
        let bogus = MapperState::Nrom(NromSnap::default());
        assert!(m.save_state_apply(&bogus).is_err());
    }
}
