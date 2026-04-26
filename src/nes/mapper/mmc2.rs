// SPDX-License-Identifier: GPL-3.0-or-later
//! MMC2 / PxROM (mapper 9).
//!
//! Shipped on exactly two carts: Mike Tyson's Punch-Out!! and its
//! re-release Punch-Out!! Both use the same programming model:
//!
//! **PRG layout** (fixed):
//! - `$8000-$9FFF` - 8 KB switchable via `$A000` (4-bit, so up to 128 KB PRG)
//! - `$A000-$BFFF` - third-to-last 8 KB bank (fixed)
//! - `$C000-$DFFF` - second-to-last 8 KB bank (fixed)
//! - `$E000-$FFFF` - last 8 KB bank (fixed; reset vector always visible)
//!
//! **CHR layout** - two independent 4 KB windows, each with a pair of
//! bank regs (FD / FE, each 5-bit for up to 128 KB CHR) and a 1-bit
//! latch selecting which one is live:
//! - `$0000-$0FFF` - `$B000` sets FD bank, `$C000` sets FE bank
//! - `$1000-$1FFF` - `$D000` sets FD bank, `$E000` sets FE bank
//!
//! **Mirroring**: `$F000` bit 0 - 0 = vertical, 1 = horizontal.
//!
//! ## The CHR latch (the only interesting thing)
//!
//! Punch-Out!! animates its 48×48 "giant sprite" characters without
//! CPU writes: the mapper snoops every PPU fetch and swaps CHR banks
//! when the PPU reads specific pattern-table addresses. Tile IDs
//! `$FD` and `$FE` at the bottom of each 4 KB pattern window act as
//! the trigger.
//!
//! Trigger addresses (MMC2-specific - note the asymmetry, MMC4 uses
//! the range form on both sides):
//! - PPU reads `$0FD8` exactly → left latch = 0 (FD bank)
//! - PPU reads `$0FE8` exactly → left latch = 1 (FE bank)
//! - PPU reads `$1FD8-$1FDF` → right latch = 0 (FD bank)
//! - PPU reads `$1FE8-$1FEF` → right latch = 1 (FE bank)
//!
//! **Timing.** The triggering fetch itself uses the pre-trigger bank;
//! the latch change takes effect on the NEXT fetch. Our `ppu_read`
//! mirrors this by resolving the bank from the current latch state,
//! returning the byte, and updating the latch as a post-step.
//!
//! Power-on: both latches = 1 (FE side). Per Mesen2's init; the real
//! power-on value is unspecified by hardware but 1 is the convention.
//!
//! No PRG-RAM, no battery, no IRQ. Real boards have bus conflicts on
//! `$A000-$FFFF` writes (value ANDed with ROM byte), but every
//! shipping title writes matching bytes - skipping the AND matches
//! Mesen2 / puNES / Nestopia behavior.
//!
//! Clean-room references (behavioral only, no copied code):
//! - `~/Git/Mesen2/Core/NES/Mappers/Nintendo/MMC2.h`
//! - `~/Git/punes/src/core/mappers/MMC2.c` + `mapper_009.c`
//! - `~/Git/nestopia/source/core/board/NstBoardMmc2.cpp`
//! - nesdev.org/wiki/MMC2, nes-expert `reference/mappers.md §Mapper 9`

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_8K: usize = 8 * 1024;
const CHR_BANK_4K: usize = 4 * 1024;

pub struct Mmc2 {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,
    mirroring: Mirroring,

    prg_bank_count_8k: usize,
    chr_bank_count_4k: usize,

    /// `$A000` - 8 KB PRG bank index for `$8000-$9FFF`. 4-bit.
    prg_bank: u8,
    /// `$B000` - 4 KB CHR bank for `$0000-$0FFF` when left latch = 0.
    left_fd: u8,
    /// `$C000` - 4 KB CHR bank for `$0000-$0FFF` when left latch = 1.
    left_fe: u8,
    /// `$D000` - 4 KB CHR bank for `$1000-$1FFF` when right latch = 0.
    right_fd: u8,
    /// `$E000` - 4 KB CHR bank for `$1000-$1FFF` when right latch = 1.
    right_fe: u8,

    /// Current left-window latch (0 = FD, 1 = FE). Power-on: 1.
    left_latch: u8,
    /// Current right-window latch (0 = FD, 1 = FE). Power-on: 1.
    right_latch: u8,
}

impl Mmc2 {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count_8k = (cart.prg_rom.len() / PRG_BANK_8K).max(1);

        let is_chr_ram = cart.chr_ram || cart.chr_rom.is_empty();
        // MMC2 commercial carts (both of them) use CHR-ROM. CHR-RAM
        // handling is here for completeness / fan-made ROMs.
        let chr = if is_chr_ram {
            vec![0u8; 8 * 1024]
        } else {
            cart.chr_rom
        };
        let chr_bank_count_4k = (chr.len() / CHR_BANK_4K).max(1);

        Self {
            prg_rom: cart.prg_rom,
            chr,
            chr_ram: is_chr_ram,
            mirroring: cart.mirroring,
            prg_bank_count_8k,
            chr_bank_count_4k,
            prg_bank: 0,
            left_fd: 0,
            left_fe: 0,
            right_fd: 0,
            right_fe: 0,
            left_latch: 1,
            right_latch: 1,
        }
    }

    /// Which 4 KB CHR bank is currently mapped to the left window
    /// ($0000-$0FFF). Depends only on current latch state.
    fn left_chr_bank(&self) -> usize {
        let bank = if self.left_latch == 0 {
            self.left_fd
        } else {
            self.left_fe
        };
        (bank as usize) % self.chr_bank_count_4k
    }

    fn right_chr_bank(&self) -> usize {
        let bank = if self.right_latch == 0 {
            self.right_fd
        } else {
            self.right_fe
        };
        (bank as usize) % self.chr_bank_count_4k
    }

    fn map_chr(&self, addr: u16) -> usize {
        let bank = if addr < 0x1000 {
            self.left_chr_bank()
        } else {
            self.right_chr_bank()
        };
        bank * CHR_BANK_4K + (addr as usize & (CHR_BANK_4K - 1))
    }

    /// Resolve `$8000-$FFFF` to a flat PRG-ROM offset.
    fn map_prg(&self, addr: u16) -> usize {
        let bank = match addr {
            0x8000..=0x9FFF => (self.prg_bank as usize) % self.prg_bank_count_8k,
            0xA000..=0xBFFF => self.prg_bank_count_8k.saturating_sub(3),
            0xC000..=0xDFFF => self.prg_bank_count_8k.saturating_sub(2),
            0xE000..=0xFFFF => self.prg_bank_count_8k.saturating_sub(1),
            _ => 0,
        };
        bank * PRG_BANK_8K + (addr as usize & (PRG_BANK_8K - 1))
    }

    /// Post-read latch update. The triggering fetch has already been
    /// served from the pre-trigger bank; this mutation takes effect on
    /// the NEXT fetch. Called from `ppu_read` after the byte is read.
    fn update_latch(&mut self, addr: u16) {
        match addr {
            0x0FD8 => self.left_latch = 0,
            0x0FE8 => self.left_latch = 1,
            0x1FD8..=0x1FDF => self.right_latch = 0,
            0x1FE8..=0x1FEF => self.right_latch = 1,
            _ => {}
        }
    }
}

impl Mapper for Mmc2 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        // Registers decode by the top nibble - any write inside a
        // 4 KB window hits that window's register. MMC2 has no
        // PRG-RAM at $6000-$7FFF, so writes there are dropped.
        if addr < 0x8000 {
            return;
        }
        match addr & 0xF000 {
            0xA000 => self.prg_bank = data & 0x0F,
            0xB000 => self.left_fd = data & 0x1F,
            0xC000 => self.left_fe = data & 0x1F,
            0xD000 => self.right_fd = data & 0x1F,
            0xE000 => self.right_fe = data & 0x1F,
            0xF000 => {
                self.mirroring = if data & 0x01 != 0 {
                    Mirroring::Horizontal
                } else {
                    Mirroring::Vertical
                };
            }
            _ => {}
        }
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

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr >= 0x2000 {
            return 0;
        }
        // Resolve the byte using the CURRENT latches - the triggering
        // fetch itself must see the pre-trigger bank.
        let i = self.map_chr(addr);
        let byte = *self.chr.get(i).unwrap_or(&0);
        // Now latch the change. Any subsequent read uses the new bank.
        self.update_latch(addr);
        byte
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        // Commercial MMC2 carts are CHR-ROM - writes are no-ops. The
        // CHR-RAM branch exists for robustness against fan-made ROMs
        // that repurpose the mapper. Latch semantics still apply: a
        // write to a trigger address updates the latch, matching
        // Mesen2's `NotifyVramAddressChange` (which fires on any bus
        // assertion, not just reads).
        if self.chr_ram && addr < 0x2000 {
            let i = self.map_chr(addr);
            if let Some(slot) = self.chr.get_mut(i) {
                *slot = data;
            }
        }
        if addr < 0x2000 {
            self.update_latch(addr);
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, Mirroring, TvSystem};

    /// 128 KB PRG (16 × 8 KB banks) where every byte equals its bank
    /// index; 128 KB CHR (32 × 4 KB banks) where every byte equals its
    /// 4 KB bank index. Matches the existing `mmc3::tests::tagged_cart`
    /// convention so tests can assert "this address reads bank N"
    /// without arithmetic.
    fn tagged_cart() -> Cartridge {
        let mut prg = vec![0u8; 16 * PRG_BANK_8K];
        for b in 0..16 {
            prg[b * PRG_BANK_8K..(b + 1) * PRG_BANK_8K].fill(b as u8);
        }
        let mut chr = vec![0u8; 32 * CHR_BANK_4K];
        for b in 0..32 {
            chr[b * CHR_BANK_4K..(b + 1) * CHR_BANK_4K].fill(b as u8);
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: chr,
            chr_ram: false,
            mapper_id: 9,
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

    // ---- PRG banking ----

    #[test]
    fn prg_default_layout_fixes_last_three_banks() {
        let m = Mmc2::new(tagged_cart());
        // $A000 default = 0 → switchable window reads bank 0.
        assert_eq!(m.cpu_peek(0x8000), 0);
        assert_eq!(m.cpu_peek(0x9FFF), 0);
        // Fixed windows - third-to-last, second-to-last, last.
        assert_eq!(m.cpu_peek(0xA000), 13);
        assert_eq!(m.cpu_peek(0xC000), 14);
        assert_eq!(m.cpu_peek(0xE000), 15);
    }

    #[test]
    fn prg_a000_switches_low_window_only() {
        let mut m = Mmc2::new(tagged_cart());
        m.cpu_write(0xA000, 7);
        assert_eq!(m.cpu_peek(0x8000), 7);
        assert_eq!(m.cpu_peek(0x9FFF), 7);
        // Fixed windows unaffected.
        assert_eq!(m.cpu_peek(0xA000), 13);
        assert_eq!(m.cpu_peek(0xC000), 14);
        assert_eq!(m.cpu_peek(0xE000), 15);
    }

    #[test]
    fn prg_a000_masks_to_4_bits() {
        let mut m = Mmc2::new(tagged_cart());
        // High nibble must be ignored - F7 & 0x0F = 7.
        m.cpu_write(0xA000, 0xF7);
        assert_eq!(m.cpu_peek(0x8000), 7);
    }

    #[test]
    fn prg_register_decoded_by_top_nibble() {
        // Any address in $A000-$AFFF must select the PRG bank.
        let mut m = Mmc2::new(tagged_cart());
        m.cpu_write(0xA123, 5);
        assert_eq!(m.cpu_peek(0x8000), 5);
        m.cpu_write(0xAFFF, 9);
        assert_eq!(m.cpu_peek(0x8000), 9);
    }

    // ---- CHR banking + latch ----

    #[test]
    fn chr_power_on_latch_selects_fe_side() {
        let mut m = Mmc2::new(tagged_cart());
        // Program distinct FD / FE banks for both windows.
        m.cpu_write(0xB000, 4); // left FD
        m.cpu_write(0xC000, 5); // left FE
        m.cpu_write(0xD000, 6); // right FD
        m.cpu_write(0xE000, 7); // right FE
        // Power-on latches are 1 (FE) - reads outside trigger ranges
        // must return the FE bank from both windows.
        assert_eq!(m.ppu_read(0x0000), 5);
        assert_eq!(m.ppu_read(0x1000), 7);
    }

    #[test]
    fn chr_left_latch_triggers_exactly_at_0fd8_and_0fe8() {
        let mut m = Mmc2::new(tagged_cart());
        m.cpu_write(0xB000, 10); // left FD = 10
        m.cpu_write(0xC000, 20); // left FE = 20
        // Baseline: FE.
        assert_eq!(m.ppu_read(0x0000), 20);
        // Read $0FD8 - the fetch itself uses the pre-trigger bank (FE),
        // but after the read the latch flips to FD.
        assert_eq!(m.ppu_read(0x0FD8), 20);
        // Subsequent read picks up FD.
        assert_eq!(m.ppu_read(0x0000), 10);
        // And $0FE8 flips back to FE.
        assert_eq!(m.ppu_read(0x0FE8), 10); // triggering fetch sees FD
        assert_eq!(m.ppu_read(0x0000), 20); // post-trigger reverts to FE
    }

    #[test]
    fn chr_left_trigger_is_single_address_not_range() {
        // MMC2 (unlike MMC4) triggers the LEFT side only on exactly
        // $0FD8 / $0FE8. $0FD9 etc. must NOT flip the latch.
        let mut m = Mmc2::new(tagged_cart());
        m.cpu_write(0xB000, 10);
        m.cpu_write(0xC000, 20);
        // Prime: flip to FD.
        m.ppu_read(0x0FD8);
        assert_eq!(m.ppu_read(0x0000), 10);
        // Read $0FD9 - NOT a trigger on MMC2. Latch must stay at FD.
        m.ppu_read(0x0FD9);
        assert_eq!(m.ppu_read(0x0000), 10);
        // $0FDF also not a trigger on the left side.
        m.ppu_read(0x0FDF);
        assert_eq!(m.ppu_read(0x0000), 10);
        // And $0FE9-$0FEF don't flip to FE either.
        m.ppu_read(0x0FE9);
        assert_eq!(m.ppu_read(0x0000), 10);
    }

    #[test]
    fn chr_right_latch_triggers_across_fd8_to_fdf_range() {
        // The RIGHT side uses the 8-address range form on MMC2.
        // Every address in $1FD8-$1FDF must flip to FD; every address
        // in $1FE8-$1FEF must flip to FE.
        let mut m = Mmc2::new(tagged_cart());
        m.cpu_write(0xD000, 14); // right FD
        m.cpu_write(0xE000, 21); // right FE
        assert_eq!(m.ppu_read(0x1000), 21); // power-on FE

        for trigger in 0x1FD8..=0x1FDF {
            // Reset to FE.
            m.ppu_read(0x1FE8);
            assert_eq!(m.ppu_read(0x1000), 21);
            // Trigger flips to FD.
            m.ppu_read(trigger);
            assert_eq!(m.ppu_read(0x1000), 14, "addr ${:04X} failed", trigger);
        }
        for trigger in 0x1FE8..=0x1FEF {
            // Reset to FD.
            m.ppu_read(0x1FD8);
            assert_eq!(m.ppu_read(0x1000), 14);
            // Trigger flips to FE.
            m.ppu_read(trigger);
            assert_eq!(m.ppu_read(0x1000), 21, "addr ${:04X} failed", trigger);
        }
    }

    #[test]
    fn chr_right_trigger_ignores_out_of_range_addresses() {
        let mut m = Mmc2::new(tagged_cart());
        m.cpu_write(0xD000, 14);
        m.cpu_write(0xE000, 21);
        // Prime FD.
        m.ppu_read(0x1FD8);
        assert_eq!(m.ppu_read(0x1000), 14);
        // $1FD7 (one below the range) must NOT flip.
        m.ppu_read(0x1FD7);
        assert_eq!(m.ppu_read(0x1000), 14);
        // $1FE0 (between the two ranges) must NOT flip.
        m.ppu_read(0x1FE0);
        assert_eq!(m.ppu_read(0x1000), 14);
        // $1FF0 (above both ranges) must NOT flip.
        m.ppu_read(0x1FF0);
        assert_eq!(m.ppu_read(0x1000), 14);
    }

    #[test]
    fn chr_left_and_right_latches_are_independent() {
        let mut m = Mmc2::new(tagged_cart());
        m.cpu_write(0xB000, 1); // left FD
        m.cpu_write(0xC000, 2); // left FE
        m.cpu_write(0xD000, 3); // right FD
        m.cpu_write(0xE000, 4); // right FE

        // Flip left only.
        m.ppu_read(0x0FD8);
        assert_eq!(m.ppu_read(0x0000), 1); // left on FD
        assert_eq!(m.ppu_read(0x1000), 4); // right still on FE

        // Flip right only.
        m.ppu_read(0x1FD8);
        assert_eq!(m.ppu_read(0x0000), 1); // left still on FD
        assert_eq!(m.ppu_read(0x1000), 3); // right now on FD
    }

    #[test]
    fn chr_bank_regs_mask_to_5_bits() {
        // $B000-$E000 writes must mask to 5 bits - we have 32 banks so
        // a larger value would out-of-range without the mask.
        let mut m = Mmc2::new(tagged_cart());
        // 0xFF & 0x1F = 0x1F = 31 (last bank).
        m.cpu_write(0xC000, 0xFF);
        assert_eq!(m.ppu_read(0x0000), 31);
    }

    #[test]
    fn chr_register_decoded_by_top_nibble() {
        // Any address in $B000-$BFFF must update left-FD. Check a
        // non-base address to prove the full-nibble decode.
        let mut m = Mmc2::new(tagged_cart());
        m.cpu_write(0xBABC, 9);
        m.ppu_read(0x0FD8); // flip to FD
        assert_eq!(m.ppu_read(0x0000), 9);
    }

    // ---- Mirroring ----

    #[test]
    fn f000_toggles_mirroring() {
        let mut m = Mmc2::new(tagged_cart());
        m.cpu_write(0xF000, 0); // bit 0 = 0 → vertical
        assert_eq!(m.mirroring(), Mirroring::Vertical);
        m.cpu_write(0xF000, 1); // bit 0 = 1 → horizontal
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
        m.cpu_write(0xFFFE, 0); // any address in $F000-$FFFF; bit 0 low again
        assert_eq!(m.mirroring(), Mirroring::Vertical);
    }

    // ---- Addressing edges ----

    #[test]
    fn cpu_writes_below_8000_are_dropped() {
        let mut m = Mmc2::new(tagged_cart());
        // No PRG-RAM, no side effects. The bank state must not move.
        m.cpu_write(0x6000, 0xFF);
        m.cpu_write(0x7FFF, 0xFF);
        assert_eq!(m.cpu_peek(0x8000), 0);
        assert_eq!(m.mirroring(), Mirroring::Vertical);
    }

    #[test]
    fn cpu_reads_outside_prg_return_zero() {
        let m = Mmc2::new(tagged_cart());
        assert_eq!(m.cpu_peek(0x0000), 0);
        assert_eq!(m.cpu_peek(0x4020), 0);
        assert_eq!(m.cpu_peek(0x7FFF), 0);
    }
}
