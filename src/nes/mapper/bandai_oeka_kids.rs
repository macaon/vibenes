// SPDX-License-Identifier: GPL-3.0-or-later
//! Bandai Oeka Kids (iNES mapper 96).
//!
//! Drawing-tablet board used by *Oeka Kids - Anpanman no Hiragana
//! Daisuki* (1993). The cartridge implements 32 KiB CHR-RAM split
//! into four 4 KiB pages; the 4 KiB window at `$0000-$0FFF` is
//! selected by **the most recent nametable-tile fetch the PPU
//! drives onto its bus**, so the cart effectively follows the
//! "current tile" the PPU is rendering.
//!
//! ## Why this exists
//!
//! The drawing tablet writes per-tile pixel data into CHR-RAM. To
//! avoid a giant CPU-driven blit every frame, the cart's CHR
//! output is split into four pages and the active page is picked
//! by the PPU's nametable address: as the PPU walks the screen,
//! whichever vertical "stripe" of the nametable it's reading
//! determines which 4 KiB CHR page is mapped at `$0000`. The
//! second 4 KiB page (`$1000`) is hard-pinned to bank 3 of the
//! current outer set so the right pattern table holds the
//! cursor / overlay tiles.
//!
//! ## Register surface
//!
//! Single 8-bit register at `$8000-$FFFF` (with 32 KiB-wide bus
//! conflicts: stored value is `cpu_data & prg_rom[addr]`):
//!
//! ```text
//!   bit 2     - outer CHR bank (selects which 8 KiB CHR-RAM half)
//!   bits 1-0  - 32 KiB PRG bank (0..3)
//! ```
//!
//! `$C000-$FFFF` doesn't exist as a separate window - the entire
//! `$8000-$FFFF` range is one 32 KiB switchable bank (NROM-256
//! style).
//!
//! ## CHR-page latch
//!
//! The cart watches the PPU address bus. Whenever an access lands
//! in the **nametable byte** range (`$2000-$2FFF` minus the
//! attribute-table tail at `(addr & 0x3FF) >= 0x3C0`), the inner
//! CHR latch captures **bits 8-9** of the address - giving 0..3.
//! `$0000-$0FFF` then maps to `outer | inner`, while
//! `$1000-$1FFF` always maps to `outer | 3`.
//!
//! Both `update_r2006`-style (CPU `$2006` latch updates) and live
//! rendering nametable fetches drive the latch; we hook
//! [`Mapper::on_ppu_addr`] which fires on every PPU bus drive.
//!
//! Clean-room references (behavioral only):
//! - `~/Git/Mesen2/Core/NES/Mappers/Bandai/OekaKids.h`
//! - `~/Git/punes/src/core/mappers/mapper_096.c`
//! - `~/Git/nestopia/source/core/board/NstBoardBandaiOekaKids.cpp`
//! - nesdev.org/wiki/INES_Mapper_096

use crate::nes::mapper::{Mapper, PpuFetchKind};
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_32K: usize = 32 * 1024;
const CHR_PAGE_4K: usize = 4 * 1024;
const CHR_RAM_TOTAL: usize = 32 * 1024;

pub struct BandaiOekaKids {
    prg_rom: Vec<u8>,
    chr_ram: Vec<u8>,
    mirroring: Mirroring,

    prg_bank_count_32k: usize,

    /// `$8000` write & PRG[$8000 + addr]: bits 0-1 PRG, bit 2 outer CHR.
    reg: u8,
    /// PPU-driven inner CHR latch (0..3). Captured from bits 8-9
    /// of the most recent nametable-tile fetch address.
    inner_chr: u8,
}

impl BandaiOekaKids {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count_32k = (cart.prg_rom.len() / PRG_BANK_32K).max(1);
        Self {
            prg_rom: cart.prg_rom,
            chr_ram: vec![0u8; CHR_RAM_TOTAL],
            mirroring: cart.mirroring,
            prg_bank_count_32k,
            reg: 0,
            inner_chr: 0,
        }
    }

    fn outer_chr(&self) -> usize {
        ((self.reg & 0x04) as usize) >> 2
    }

    fn map_prg(&self, addr: u16) -> usize {
        let bank = (self.reg & 0x03) as usize % self.prg_bank_count_32k;
        bank * PRG_BANK_32K + ((addr - 0x8000) as usize)
    }

    fn map_chr(&self, addr: u16) -> usize {
        // Eight 4 KiB pages = 32 KiB total. The outer bit selects
        // the high or low 4-page set; within that set, $0000 picks
        // the inner-latched page and $1000 pins to page 3.
        let page = match addr {
            0x0000..=0x0FFF => (self.outer_chr() << 2) | (self.inner_chr as usize & 0x03),
            0x1000..=0x1FFF => (self.outer_chr() << 2) | 0x03,
            _ => 0,
        };
        page * CHR_PAGE_4K + (addr as usize & (CHR_PAGE_4K - 1))
    }
}

impl Mapper for BandaiOekaKids {
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
        let i = self.map_prg(addr);
        let rom_byte = *self.prg_rom.get(i).unwrap_or(&0xFF);
        self.reg = data & rom_byte;
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr >= 0x2000 {
            return 0;
        }
        let i = self.map_chr(addr);
        *self.chr_ram.get(i).unwrap_or(&0)
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if addr >= 0x2000 {
            return;
        }
        let i = self.map_chr(addr);
        if let Some(slot) = self.chr_ram.get_mut(i) {
            *slot = data;
        }
    }

    fn on_ppu_addr(&mut self, addr: u16, _ppu_cycle: u64, _kind: PpuFetchKind) {
        // Latch fires on any PPU bus drive that lands inside a
        // nametable's tile-byte region (low 10 bits 0..0x3BF).
        // Attribute-byte fetches (low 10 bits 0x3C0..0x3FF) and
        // pattern fetches ($0000-$1FFF) are excluded - matches the
        // Nestopia / puNES gate exactly. Mesen2 uses a slightly
        // different debounce (last-address transition) but reaches
        // the same steady state for the only commercial cart.
        if (0x2000..0x3000).contains(&addr) && (addr & 0x03FF) < 0x03C0 {
            self.inner_chr = ((addr >> 8) & 0x03) as u8;
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }

    fn save_state_capture(&self) -> Option<crate::save_state::MapperState> {
        use crate::save_state::mapper::{BandaiOekaKidsSnap, MirroringSnap};
        Some(crate::save_state::MapperState::BandaiOekaKids(BandaiOekaKidsSnap {
            chr_ram: self.chr_ram.clone(),
            mirroring: MirroringSnap::from_live(self.mirroring),
            reg: self.reg,
            inner_chr: self.inner_chr,
        }))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::BandaiOekaKids(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        if snap.chr_ram.len() == self.chr_ram.len() {
            self.chr_ram.copy_from_slice(&snap.chr_ram);
        }
        self.mirroring = snap.mirroring.to_live();
        self.reg = snap.reg;
        self.inner_chr = snap.inner_chr;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, Mirroring, TvSystem};

    /// 128 KiB PRG (4 banks * 32 KiB) where each bank is filled
    /// with `0xFF` so bus conflicts pass through the CPU value
    /// unchanged. We tag the first byte of each 32 KiB bank so we
    /// can identify the active bank.
    fn cart() -> Cartridge {
        let mut prg = vec![0xFFu8; 4 * PRG_BANK_32K];
        for b in 0..4 {
            prg[b * PRG_BANK_32K] = 0xA0 + b as u8;
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: vec![],
            chr_ram: true,
            mapper_id: 96,
            submapper: 0,
            mirroring: Mirroring::SingleScreenLower,
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

    fn m() -> BandaiOekaKids {
        BandaiOekaKids::new(cart())
    }

    #[test]
    fn power_on_state_is_bank_zero() {
        let m = m();
        assert_eq!(m.reg, 0);
        assert_eq!(m.inner_chr, 0);
        assert_eq!(m.cpu_peek(0x8000), 0xA0);
    }

    #[test]
    fn prg_write_with_full_rom_byte_passes_value_through() {
        let mut m = m();
        // ROM is 0xFF everywhere except the bank-tag bytes; writing
        // at $8001 sees ROM byte 0xFF, so the AND is a no-op.
        m.cpu_write(0x8001, 0x03);
        assert_eq!(m.reg, 0x03);
        assert_eq!(m.cpu_peek(0x8000), 0xA3);
    }

    #[test]
    fn prg_write_at_tag_byte_masks_through_bus_conflict() {
        let mut m = m();
        // The write address is exactly the bank-0 tag (0xA0); writing
        // 0x07 ANDs with 0xA0 = 0x00. So the PRG bank stays at 0.
        m.cpu_write(0x8000, 0x07);
        assert_eq!(m.reg, 0x00);
    }

    #[test]
    fn outer_chr_bit_picks_high_or_low_chr_ram_half() {
        let mut m = m();
        // Without the outer bit set, outer_chr() = 0 → pages 0..3.
        m.cpu_write(0x8001, 0x00);
        assert_eq!(m.outer_chr(), 0);
        // With bit 2 set, outer_chr() = 1 → pages 4..7.
        m.cpu_write(0x8001, 0x04);
        assert_eq!(m.outer_chr(), 1);
    }

    #[test]
    fn fixed_window_pins_to_page_three_within_outer_set() {
        let mut m = m();
        // Write a sentinel into page 3 ($3000-$3FFF in CHR-RAM).
        for i in 0..16 {
            m.chr_ram[3 * CHR_PAGE_4K + i] = 0xC0 + i as u8;
        }
        // With outer bit clear, $1000-$1FFF reads page 3.
        m.cpu_write(0x8001, 0x00);
        for i in 0..16 {
            assert_eq!(m.ppu_read(0x1000 + i), 0xC0 + i as u8);
        }
    }

    #[test]
    fn nametable_tile_fetch_updates_inner_chr_latch() {
        let mut m = m();
        // $2100 has bits 8-9 = 01 → inner = 1. Low 10 bits = 0x100,
        // which is < 0x3C0, so the gate accepts.
        m.on_ppu_addr(0x2100, 0, PpuFetchKind::BgNametable);
        assert_eq!(m.inner_chr, 1);
        // $2200 → inner = 2.
        m.on_ppu_addr(0x2200, 0, PpuFetchKind::BgNametable);
        assert_eq!(m.inner_chr, 2);
        // $2300 → inner = 3 (3 << 8).
        m.on_ppu_addr(0x2300, 0, PpuFetchKind::BgNametable);
        assert_eq!(m.inner_chr, 3);
    }

    #[test]
    fn attribute_byte_fetch_does_not_update_inner_chr_latch() {
        let mut m = m();
        m.on_ppu_addr(0x2200, 0, PpuFetchKind::BgNametable);
        assert_eq!(m.inner_chr, 2);
        // Attribute table sits at low 10 bits 0x3C0..0x3FF.
        m.on_ppu_addr(0x23C0, 0, PpuFetchKind::BgAttribute);
        assert_eq!(m.inner_chr, 2); // unchanged
        m.on_ppu_addr(0x23FF, 0, PpuFetchKind::BgAttribute);
        assert_eq!(m.inner_chr, 2);
    }

    #[test]
    fn pattern_table_fetch_does_not_update_inner_chr_latch() {
        let mut m = m();
        m.on_ppu_addr(0x2100, 0, PpuFetchKind::BgNametable);
        assert_eq!(m.inner_chr, 1);
        // Pattern fetches live below $2000.
        m.on_ppu_addr(0x0123, 0, PpuFetchKind::BgPattern);
        m.on_ppu_addr(0x1FFF, 0, PpuFetchKind::SpritePattern);
        assert_eq!(m.inner_chr, 1);
    }

    #[test]
    fn switchable_window_follows_inner_chr_latch() {
        let mut m = m();
        // Tag each 4 KiB CHR-RAM page's first byte with a sentinel.
        for p in 0..8 {
            m.chr_ram[p * CHR_PAGE_4K] = 0x80 + p as u8;
        }
        m.cpu_write(0x8001, 0x00); // outer = 0
        // PPU reads NT byte at $2200 → inner_chr = 2 → $0000 maps to page 2.
        m.on_ppu_addr(0x2200, 0, PpuFetchKind::BgNametable);
        assert_eq!(m.ppu_read(0x0000), 0x82);
        // Move to NT byte at $2000 → inner_chr = 0 → $0000 maps to page 0.
        m.on_ppu_addr(0x2000, 0, PpuFetchKind::BgNametable);
        assert_eq!(m.ppu_read(0x0000), 0x80);

        // With outer bit set, pages 4..7 are addressed; inner_chr = 0
        // means $0000 maps to page 4.
        m.cpu_write(0x8001, 0x04);
        m.on_ppu_addr(0x2000, 0, PpuFetchKind::BgNametable);
        assert_eq!(m.ppu_read(0x0000), 0x84);
    }

    #[test]
    fn save_state_round_trip_preserves_chr_ram_and_latches() {
        let mut a = m();
        // Drive into a non-default state.
        a.cpu_write(0x8001, 0x07);
        a.on_ppu_addr(0x2300, 0, PpuFetchKind::BgNametable);
        a.ppu_write(0x0123, 0x42);
        let snap = a.save_state_capture().unwrap();

        let mut b = m();
        b.save_state_apply(&snap).unwrap();
        assert_eq!(b.reg, a.reg);
        assert_eq!(b.inner_chr, a.inner_chr);
        // CHR-RAM round-trips: the byte we wrote should still be live
        // after the same nametable latch is reapplied.
        b.on_ppu_addr(0x2300, 0, PpuFetchKind::BgNametable);
        assert_eq!(b.ppu_read(0x0123), 0x42);
    }
}
