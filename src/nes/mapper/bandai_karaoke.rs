// SPDX-License-Identifier: GPL-3.0-or-later
//! Bandai Karaoke Studio (iNES mapper 188).
//!
//! Custom Bandai Famicom board for *Karaoke Studio* (1987). The
//! cart hosts the 128 KiB game ROM ("internal"), and the bundled
//! microphone unit carries a second 128 KiB ROM ("expansion") with
//! the song library. The mapper presents either half at
//! `$8000-$BFFF` selectable per-write; `$C000-$FFFF` is hardwired
//! to the last 16 KiB of the internal ROM (which holds the boot
//! / shell code that has to stay resident regardless of the
//! current bank). 8 KiB CHR-RAM, dynamic H/V mirroring.
//!
//! Three add-on song carts (*Senjou no Carmen*, *Sangokushi*,
//! *Kenshirou*) plug into the microphone slot and replace the
//! 128 KiB expansion image - the iNES dump just concatenates
//! both halves into a 256 KiB PRG image.
//!
//! ## Register surface (single latch at `$8000-$FFFF`)
//!
//! ```text
//! ..MR PBBB
//!   |  |  |
//!   |  |  +-- BBB: 16 KiB bank index within the selected ROM half
//!   |  +----- P  : (bit 4) 1 = internal ROM half, 0 = expansion
//!   +-------- M  : (bit 5) 1 = horizontal mirroring, 0 = vertical
//! ```
//!
//! ## Microphone reads (`$6000-$7FFF`)
//!
//! On real hardware the cart routes the mic input through a
//! comparator into D0/D1, with D2-D7 floating (open bus). The
//! microphone is **not modeled** here, so we report "silent
//! mic": low bits zero, high bits filled with the standard
//! open-bus pattern Mesen2 uses (`0xF8`). Karaoke Studio still
//! boots fully and the game plays - only the mic-driven scoring
//! is inert.
//!
//! ## 128-KiB-only dumps
//!
//! Some library dumps include only the internal half (no song
//! ROM attached). Per Mesen2, when the cart is < 256 KiB and the
//! game writes bit 4 = 0 (expansion), the `$8000-$BFFF` window
//! goes open bus rather than wrapping back into the internal
//! image. We replicate that behavior to match real hardware
//! hot-plug semantics.
//!
//! References:
//! - <https://www.nesdev.org/wiki/INES_Mapper_188>
//! - `~/Git/Mesen2/Core/NES/Mappers/Bandai/BandaiKaraoke.h`
//! - `~/Git/punes/src/core/mappers/mapper_188.c`

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_16K: usize = 16 * 1024;
const CHR_BANK_8K: usize = 8 * 1024;
const INTERNAL_ROM_BANKS: usize = 8; // 128 KiB / 16 KiB
const FIXED_HIGH_BANK: usize = 7; // last 16 KiB of the internal half

pub struct BandaiKaraoke {
    prg_rom: Vec<u8>,
    chr_ram: Vec<u8>,

    /// Latched register byte (post bus-conflict AND).
    reg: u8,
    /// Live mirroring derived from the latch's bit 5.
    mirroring: Mirroring,

    prg_bank_count_16k: usize,
    has_expansion: bool,
}

impl BandaiKaraoke {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count_16k = (cart.prg_rom.len() / PRG_BANK_16K).max(1);
        let has_expansion = prg_bank_count_16k > INTERNAL_ROM_BANKS;
        Self {
            prg_rom: cart.prg_rom,
            chr_ram: vec![0u8; CHR_BANK_8K],
            // Power-on: bit 4 = 0 selects expansion. Real Karaoke
            // Studio bootstraps from the fixed `$C000-$FFFF`
            // window (always internal bank 7), so the initial
            // state of `$8000` doesn't matter for boot.
            reg: 0,
            mirroring: cart.mirroring,
            prg_bank_count_16k,
            has_expansion,
        }
    }

    /// Bank selector for the `$8000-$BFFF` window. Returns `None`
    /// when the latch points at expansion ROM but the cart is
    /// internal-only (the read should go open bus).
    fn switch_bank(&self) -> Option<usize> {
        let idx = (self.reg & 0x07) as usize;
        if self.reg & 0x10 != 0 {
            // Internal half - banks 0..=7.
            Some(idx % self.prg_bank_count_16k)
        } else if self.has_expansion {
            // Expansion half - banks 8..=15.
            Some((idx | 0x08) % self.prg_bank_count_16k)
        } else {
            None
        }
    }

    fn fixed_bank(&self) -> usize {
        // Always the last bank of the internal half.
        FIXED_HIGH_BANK.min(self.prg_bank_count_16k.saturating_sub(1))
    }

    fn prg_byte(&self, bank: usize, off: usize) -> u8 {
        let base = bank * PRG_BANK_16K;
        *self.prg_rom.get(base + off).unwrap_or(&0)
    }

    fn rom_visible_at(&self, addr: u16) -> u8 {
        // Used by the bus-conflict path. Returns whatever the cart
        // currently presents at this address, or `$FF` for an
        // unmapped (open-bus) low window.
        match addr {
            0x8000..=0xBFFF => match self.switch_bank() {
                Some(b) => self.prg_byte(b, (addr - 0x8000) as usize),
                None => 0xFF,
            },
            0xC000..=0xFFFF => self.prg_byte(self.fixed_bank(), (addr - 0xC000) as usize),
            _ => 0xFF,
        }
    }
}

impl Mapper for BandaiKaraoke {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => {
                // No microphone modeled - low bits silent (0),
                // high bits open bus (Mesen pattern).
                0xF8
            }
            0x8000..=0xBFFF => match self.switch_bank() {
                Some(b) => self.prg_byte(b, (addr - 0x8000) as usize),
                None => 0,
            },
            0xC000..=0xFFFF => self.prg_byte(self.fixed_bank(), (addr - 0xC000) as usize),
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        if addr >= 0x8000 {
            // Bus conflict: visible ROM byte ANDs the CPU value
            // before reaching the latch.
            self.reg = data & self.rom_visible_at(addr);
            self.mirroring = if self.reg & 0x20 != 0 {
                Mirroring::Horizontal
            } else {
                Mirroring::Vertical
            };
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr < 0x2000 {
            *self.chr_ram.get(addr as usize).unwrap_or(&0)
        } else {
            0
        }
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if addr < 0x2000 {
            if let Some(slot) = self.chr_ram.get_mut(addr as usize) {
                *slot = data;
            }
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }

    fn save_state_capture(&self) -> Option<crate::save_state::MapperState> {
        use crate::save_state::mapper::{BandaiKaraokeSnap, MirroringSnap};
        Some(crate::save_state::MapperState::BandaiKaraoke(
            BandaiKaraokeSnap {
                chr_ram_data: self.chr_ram.clone(),
                reg: self.reg,
                mirroring: MirroringSnap::from_live(self.mirroring),
            },
        ))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::BandaiKaraoke(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        if snap.chr_ram_data.len() == self.chr_ram.len() {
            self.chr_ram.copy_from_slice(&snap.chr_ram_data);
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

    /// 256 KiB PRG (16 banks of 16 KiB - 8 internal + 8 expansion),
    /// CHR-RAM. Each bank is filled with `$FF` apart from the first
    /// byte, which holds the bank index (so bus-conflict ANDs are
    /// no-ops away from offset 0).
    fn cart(banks: usize) -> Cartridge {
        let mut prg = vec![0xFFu8; banks * PRG_BANK_16K];
        for bank in 0..banks {
            prg[bank * PRG_BANK_16K] = bank as u8;
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: Vec::new(),
            chr_ram: true,
            mapper_id: 188,
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
    fn boot_state_sees_internal_last_bank_at_c000() {
        let m = BandaiKaraoke::new(cart(16));
        // Fixed window is always internal bank 7.
        assert_eq!(m.cpu_peek(0xC000), 7);
        // Default reg 0 -> bit 4 = 0 -> expansion ROM, bank 0
        // of the upper half = physical bank 8.
        assert_eq!(m.cpu_peek(0x8000), 8);
    }

    #[test]
    fn bit4_picks_internal_vs_expansion_half() {
        let mut m = BandaiKaraoke::new(cart(16));
        // Internal half (bit 4 set), bank index 3.
        // Write at $8001 (PRG byte $FF) so bus-conflict AND is a
        // no-op.
        m.cpu_write(0x8001, 0b0001_0011);
        assert_eq!(m.cpu_peek(0x8000), 3);
        // Expansion half (bit 4 clear), bank index 5 -> bank 13.
        m.cpu_write(0x8001, 0b0000_0101);
        assert_eq!(m.cpu_peek(0x8000), 13);
    }

    #[test]
    fn bit5_drives_mirroring_dynamically() {
        let mut m = BandaiKaraoke::new(cart(16));
        m.cpu_write(0x8001, 0b0010_0000); // bit 5 set
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
        m.cpu_write(0x8001, 0b0000_0000); // bit 5 clear
        assert_eq!(m.mirroring(), Mirroring::Vertical);
    }

    #[test]
    fn fixed_window_stays_on_internal_bank_7_through_swaps() {
        let mut m = BandaiKaraoke::new(cart(16));
        for v in [0x00u8, 0x10, 0x20, 0x37] {
            m.cpu_write(0x8001, v);
            assert_eq!(m.cpu_peek(0xC000), 7, "value {v:#x}");
        }
    }

    #[test]
    fn cart_without_expansion_returns_open_bus_when_expansion_selected() {
        // 128 KiB cart - no expansion half present.
        let mut m = BandaiKaraoke::new(cart(8));
        m.cpu_write(0x8001, 0b0000_0000); // bit 4 clear -> expansion
        // Open bus visible as 0 in our peek path.
        assert_eq!(m.cpu_peek(0x8000), 0);
        // Internal selection still works.
        m.cpu_write(0x8001, 0b0001_0001);
        assert_eq!(m.cpu_peek(0x8000), 1);
    }

    #[test]
    fn microphone_reads_return_silent_open_bus_pattern() {
        let m = BandaiKaraoke::new(cart(16));
        assert_eq!(m.cpu_peek(0x6000), 0xF8);
        assert_eq!(m.cpu_peek(0x7FFF), 0xF8);
    }

    #[test]
    fn bus_conflict_masks_value() {
        let mut m = BandaiKaraoke::new(cart(16));
        // Currently-visible bank at $8000 with reg=0: bank 8,
        // PRG byte at $8000 = 8 (= 0b0000_1000).
        // Writing 0b0001_0011 ANDed with 0b0000_1000 = 0b0000_0000.
        // -> bit 4 stays clear (expansion), bank index 0 -> bank 8.
        m.cpu_write(0x8000, 0b0001_0011);
        assert_eq!(m.cpu_peek(0x8000), 8);
    }

    #[test]
    fn save_state_round_trip_preserves_reg_and_chr() {
        let mut m = BandaiKaraoke::new(cart(16));
        m.cpu_write(0x8001, 0b0011_0010); // mirror H, internal, bank 2
        m.ppu_write(0x0042, 0xCC);
        let snap = m.save_state_capture().unwrap();
        let mut fresh = BandaiKaraoke::new(cart(16));
        fresh.save_state_apply(&snap).unwrap();
        assert_eq!(fresh.cpu_peek(0x8000), 2);
        assert_eq!(fresh.mirroring(), Mirroring::Horizontal);
        assert_eq!(fresh.ppu_read(0x0042), 0xCC);
    }
}
