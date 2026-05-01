// SPDX-License-Identifier: GPL-3.0-or-later
//! AVE NINA-03 / NINA-06 (iNES mapper 79).
//!
//! Discrete-logic board used by American Video Entertainment (AVE),
//! C&E (Computer & Entertainment), and a handful of one-off licensees
//! to ship commercial unlicensed cartridges in the early '90s. The
//! chip is a single 8-bit register exposed through `$4100-$5FFF` -
//! writes only land when address line A8 is set, so the in-software
//! convention is to write `$4100`.
//!
//! ```text
//! 7  bit  0
//! ---- ----
//! xxxx PCCC
//!      ||||
//!      |+++- 8 KiB CHR-ROM bank at PPU $0000-$1FFF (3 bits)
//!      +---- 32 KiB PRG-ROM bank at CPU $8000-$FFFF (1 bit)
//! ```
//!
//! - PRG window is fixed at 32 KiB; D3 selects between two 32 KiB
//!   banks (real AVE carts top out at 64 KiB PRG: F-15 City War,
//!   Tiles of Fate). Single-bank carts (32 KiB PRG) ignore the bit.
//! - CHR window is fixed at 8 KiB; D0-D2 select up to 8 banks of
//!   8 KiB (64 KiB max - matches every retail AVE cart).
//! - **No bus conflict.** The chip only ever sees writes in
//!   `$4100-$5FFF`, which is cartridge expansion space - the PRG-ROM
//!   isn't on the bus at those addresses.
//! - Mirroring is solder-set from the iNES header (no register).
//! - No PRG-RAM, no IRQs, no audio.
//!
//! **Address decode.** A write hits the latch when
//! `(addr & 0xE100) == 0x4100`: A15=0, A14=1, A13=0, A8=1. This
//! covers `$4100-$41FF`, `$4300-$43FF`, ..., and the matching mirrors
//! through `$5F00-$5FFF`. Matches Mesen2's `Nina03_06` and Nestopia's
//! `Nina06::Poke_4100` (mapped at `$4100-$5FFF` step `$0200`). puNES'
//! mapper_079 uses the looser `(addr & 0x100) != 0` test which
//! coincides for every commercial software write.
//!
//! Commercial library (selection):
//! - **AVE**: Deathbots, F-15 City War, Krazy Kreatures, Tiles of
//!   Fate, Puzzle, Mermaids of Atlantis, Solitaire, Blackjack,
//!   Trolls on Treasure Island, Wally Bear and the No! Gang.
//! - **C&E** (Computer & Entertainment): Double Strike, Puzzle.
//! - **Sachen** (export carts using the same chip): Poke Block,
//!   Magical Mathematics.
//!
//! Clean-room references (behavioral only):
//! - `~/Git/Mesen2/Core/NES/Mappers/Unlicensed/Nina03_06.h`
//! - `~/Git/nestopia/source/core/board/NstBoardAveNina.{hpp,cpp}`
//! - `~/Git/punes/src/core/mappers/mapper_079.c`
//! - nesdev.org/wiki/INES_Mapper_079

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_32K: usize = 32 * 1024;
const CHR_BANK_8K: usize = 8 * 1024;

pub struct AveNina {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,
    mirroring: Mirroring,

    prg_bank_count_32k: usize,
    chr_bank_count_8k: usize,

    /// Latch byte. D3 = PRG bank, D0-D2 = CHR bank. Stored raw - the
    /// bank-index getters mask the relevant bits and modulo by the
    /// live bank count.
    reg: u8,
}

impl AveNina {
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
        (((self.reg >> 3) & 0x01) as usize) % self.prg_bank_count_32k
    }

    fn chr_bank(&self) -> usize {
        ((self.reg & 0x07) as usize) % self.chr_bank_count_8k
    }

    fn map_prg(&self, addr: u16) -> usize {
        self.prg_bank() * PRG_BANK_32K + ((addr - 0x8000) as usize)
    }

    fn map_chr(&self, addr: u16) -> usize {
        self.chr_bank() * CHR_BANK_8K + (addr as usize)
    }
}

impl Mapper for AveNina {
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
        // The latch is wired into the cartridge expansion window
        // ($4020-$5FFF). Real chip needs A14=1, A13=0, A8=1, and
        // A15=0 (which is implicit at $4xxx-$5xxx).
        if (addr & 0xE100) == 0x4100 {
            self.reg = data;
        }
    }

    fn cpu_read_ex(&mut self, _addr: u16) -> Option<u8> {
        // The chip only listens for writes; reads in $4020-$5FFF
        // float on the open bus. Returning None lets the bus take
        // over.
        None
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
        use crate::save_state::mapper::{AveNinaSnap, MirroringSnap};
        Some(crate::save_state::MapperState::AveNina(AveNinaSnap {
            chr_ram_data: if self.chr_ram { self.chr.clone() } else { Vec::new() },
            mirroring: MirroringSnap::from_live(self.mirroring),
            reg: self.reg,
        }))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::AveNina(snap) = state else {
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

    /// 64 KiB PRG (2 banks * 32 KiB) tagged at the start of each
    /// bank, plus 64 KiB CHR-ROM (8 banks * 8 KiB) also tagged.
    /// Tag layout:
    /// - PRG bank N: first byte = `0xA0 + N`.
    /// - CHR bank N: first byte = `0xC0 + N`.
    fn cart() -> Cartridge {
        let mut prg = vec![0xFFu8; 2 * PRG_BANK_32K];
        for b in 0..2 {
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
            mapper_id: 79,
            submapper: 0,
            mirroring: Mirroring::Horizontal,
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

    fn m() -> AveNina {
        AveNina::new(cart())
    }

    #[test]
    fn power_on_layout_is_bank_zero_for_both_planes() {
        let mut m = m();
        assert_eq!(m.cpu_peek(0x8000), 0xA0);
        assert_eq!(m.ppu_read(0x0000), 0xC0);
    }

    #[test]
    fn d3_selects_prg_bank() {
        let mut m = m();
        // D3 set -> PRG bank 1.
        m.cpu_write(0x4100, 0x08);
        assert_eq!(m.cpu_peek(0x8000), 0xA1);
        // D3 clear -> back to PRG bank 0.
        m.cpu_write(0x4100, 0x00);
        assert_eq!(m.cpu_peek(0x8000), 0xA0);
    }

    #[test]
    fn low_three_bits_select_chr_bank() {
        let mut m = m();
        for b in 0..8u8 {
            m.cpu_write(0x4100, b);
            assert_eq!(m.ppu_read(0x0000), 0xC0 + b);
        }
    }

    #[test]
    fn prg_and_chr_can_combine_independently() {
        let mut m = m();
        // 0x0F = PRG bank 1, CHR bank 7.
        m.cpu_write(0x4100, 0x0F);
        assert_eq!(m.cpu_peek(0x8000), 0xA1);
        assert_eq!(m.ppu_read(0x0000), 0xC7);
    }

    #[test]
    fn upper_bits_above_d3_are_ignored() {
        let mut m = m();
        // 0xF7 = upper nibble all set, D3 clear, CHR bank 7.
        m.cpu_write(0x4100, 0xF7);
        assert_eq!(m.cpu_peek(0x8000), 0xA0);
        assert_eq!(m.ppu_read(0x0000), 0xC7);
    }

    #[test]
    fn write_decode_requires_a8_high() {
        let mut m = m();
        // $4000 - A8 clear, ignored.
        m.cpu_write(0x4000, 0x07);
        assert_eq!(m.ppu_read(0x0000), 0xC0);
        // $40FF - A8 still clear.
        m.cpu_write(0x40FF, 0x07);
        assert_eq!(m.ppu_read(0x0000), 0xC0);
        // $4100 - A8 set, latch updates.
        m.cpu_write(0x4100, 0x07);
        assert_eq!(m.ppu_read(0x0000), 0xC7);
    }

    #[test]
    fn write_decode_accepts_mirrors_through_5fff() {
        // Real software always writes $4100, but the chip's decode
        // covers all $4xxx-$5xxx with A14=1, A13=0, A8=1. Verify the
        // useful mirrors hit.
        for addr in [0x4100u16, 0x4300, 0x45FF, 0x4F00, 0x5100, 0x5F00] {
            let mut m = m();
            m.cpu_write(addr, 0x05);
            assert_eq!(m.ppu_read(0x0000), 0xC5, "addr={addr:#06X}");
        }
    }

    #[test]
    fn write_decode_rejects_addresses_with_a13_high_at_5_window() {
        // $5200 has A13=1 -> doesn't match decode.
        let mut m = m();
        m.cpu_write(0x5200, 0x07);
        assert_eq!(m.ppu_read(0x0000), 0xC0);
    }

    #[test]
    fn writes_above_6000_are_ignored() {
        let mut m = m();
        m.cpu_write(0x6000, 0x07);
        m.cpu_write(0x8100, 0x07);
        m.cpu_write(0xFFFF, 0x07);
        assert_eq!(m.ppu_read(0x0000), 0xC0);
    }

    #[test]
    fn cpu_read_ex_returns_open_bus() {
        let mut m = m();
        // The mapper does not back any reads in $4020-$5FFF.
        assert_eq!(m.cpu_read_ex(0x4100), None);
        assert_eq!(m.cpu_read_ex(0x5000), None);
    }

    #[test]
    fn chr_ram_round_trips_when_cart_has_no_chr_rom() {
        let mut cart = cart();
        cart.chr_rom = vec![];
        cart.chr_ram = true;
        let mut m = AveNina::new(cart);
        m.ppu_write(0x0123, 0x42);
        assert_eq!(m.ppu_read(0x0123), 0x42);
        // Bank-select writes don't index past the single 8 KiB RAM.
        m.cpu_write(0x4100, 0x07);
        assert_eq!(m.ppu_read(0x0123), 0x42);
    }

    #[test]
    fn save_state_round_trip_preserves_reg_and_chr_ram() {
        let mut cart = cart();
        cart.chr_rom = vec![];
        cart.chr_ram = true;
        let mut a = AveNina::new(cart.clone());
        a.cpu_write(0x4100, 0x0F);
        a.ppu_write(0x0010, 0xAA);
        let snap = a.save_state_capture().unwrap();

        let mut b = AveNina::new(cart);
        b.save_state_apply(&snap).unwrap();
        assert_eq!(b.reg, a.reg);
        assert_eq!(b.ppu_read(0x0010), 0xAA);
    }

    #[test]
    fn cross_variant_apply_rejected() {
        use crate::save_state::mapper::{MapperState, NromSnap};
        let mut m = m();
        let bogus = MapperState::Nrom(NromSnap::default());
        assert!(m.save_state_apply(&bogus).is_err());
    }
}
