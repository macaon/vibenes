// SPDX-License-Identifier: GPL-3.0-or-later
//! Nintendo "Super Mario Bros. + Tetris + Nintendo World Cup" multicart
//! - iNES mapper 37.
//!
//! A single PCB that glues three MMC3-compatible games together with a
//! 3-bit outer-bank latch at `$6000-$7FFF`. The outer latch slices the
//! 256 KiB PRG image into three contiguous "jail cells" and selects
//! which 128 KiB CHR half is visible:
//!
//! | Latch (Q\[2:0\]) | PRG window         | CHR window         |
//! |------------------|--------------------|--------------------|
//! | 0,1,2            | `$00000-$0FFFF` (64 KiB) | `$00000-$1FFFF` |
//! | 3                | `$10000-$1FFFF` (64 KiB) | `$00000-$1FFFF` |
//! | 4,5,6            | `$20000-$3FFFF` (128 KiB)| `$20000-$3FFFF` |
//! | 7                | `$30000-$3FFFF` (64 KiB) | `$20000-$3FFFF` |
//!
//! The latch is captured *only* when MMC3's PRG-RAM-write gate would
//! have permitted the byte through - the spec phrases it as "the MMC3
//! thinks this register is RAM, so you need to enable writes to PRG-
//! RAM to update it." There is no actual PRG-RAM on the cart; reads
//! from `$6000-$7FFF` return open bus (we surface 0).
//!
//! ## Implementation
//!
//! Wraps an inner [`Mmc3`] and intercepts the bus paths that need
//! re-routing:
//!
//! - `$6000-$7FFF` writes capture the latch (gated by
//!   [`Mmc3::cpu_can_write_wram`]).
//! - `$8000-$FFFF` reads ask MMC3 which 8 KiB bank it would have
//!   selected, then remap that bank index through the outer latch
//!   per the wiki's per-cell mask/base table (matching puNES'
//!   `mapper_037.c` and Nestopia's `NstBoardZz.cpp`).
//! - PPU reads from `$0000-$1FFF` apply the same kind of remap to the
//!   1 KiB CHR bank, ORing with `Q2 << 7` so the upper half of the
//!   CHR image lights up when the latch is in the 4..7 range.
//! - Everything else (writes to `$8000-$FFFF`, IRQ counter, mirroring,
//!   PPU A12 monitoring, save plumbing) just forwards to MMC3.
//!
//! Reference: <https://www.nesdev.org/wiki/INES_Mapper_037>.

use crate::nes::mapper::mmc3::Mmc3;
use crate::nes::mapper::{Mapper, NametableSource, NametableWriteTarget, PpuFetchKind};
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_8K: usize = 8 * 1024;
const CHR_BANK_1K: usize = 1024;

pub struct Mapper037 {
    inner: Mmc3,
    /// Latched outer block. Three bits, cleared by hard reset (modeled
    /// as power-on init). Per the wiki this register is also tied to
    /// the CIC reset line; we don't model CIC reset, so soft-reset
    /// preserves the latch - matches the "without a working lockout
    /// chip, only a full power cycle gets you back to the menu"
    /// observation in the spec.
    block: u8,
}

impl Mapper037 {
    pub fn new(cart: Cartridge) -> Self {
        Self {
            inner: Mmc3::new(cart),
            block: 0,
        }
    }

    /// Translate the inner MMC3's 8 KiB PRG bank index through the
    /// outer-block latch. The formula matches the wiki table cell-by-
    /// cell; the bit-fiddling form is the one puNES and Nestopia both
    /// use.
    fn remap_prg(&self, mmc3_bank: usize) -> usize {
        let reg = self.block as usize;
        let base = ((reg << 2) & 0x10)
            | (if (reg & 0x03) == 0x03 { 0x08 } else { 0x00 });
        let mask = (reg << 1) | 0x07;
        base | (mmc3_bank & mask)
    }

    /// CHR remap: low 7 bits come from MMC3 (so the inner mapper still
    /// addresses 128 KiB), and `Q2` selects which 128 KiB half of the
    /// 256 KiB CHR image is visible.
    fn remap_chr(&self, mmc3_bank: usize) -> usize {
        let reg = self.block as usize;
        ((reg << 5) & 0x80) | (mmc3_bank & 0x7F)
    }

    fn read_prg_byte(&self, addr: u16) -> u8 {
        let mmc3_bank = self.inner.prg_bank_for(addr);
        let bank = self.remap_prg(mmc3_bank);
        let rom = self.inner.prg_rom();
        let total_banks = (rom.len() / PRG_BANK_8K).max(1);
        let bank = bank % total_banks;
        let offset = (addr as usize) & (PRG_BANK_8K - 1);
        *rom.get(bank * PRG_BANK_8K + offset).unwrap_or(&0)
    }

    fn read_chr_byte(&self, addr: u16) -> u8 {
        let mmc3_bank = self.inner.chr_bank_for(addr);
        let bank = self.remap_chr(mmc3_bank);
        let chr = self.inner.chr();
        let total_banks = (chr.len() / CHR_BANK_1K).max(1);
        let bank = bank % total_banks;
        let offset = (addr as usize) & (CHR_BANK_1K - 1);
        *chr.get(bank * CHR_BANK_1K + offset).unwrap_or(&0)
    }
}

impl Mapper for Mapper037 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => 0, // open bus - no actual PRG-RAM
            0x8000..=0xFFFF => self.read_prg_byte(addr),
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        match addr {
            0x6000..=0x7FFF => {
                if self.inner.cpu_can_write_wram() {
                    self.block = data & 0x07;
                }
            }
            // Everything else is plain MMC3 - bank regs, mirroring,
            // PRG-RAM enable/protect, IRQ latch/reload/enable/ack.
            // We forward unchanged so the outer latch's enable gate
            // stays in sync with the inner mapper.
            _ => self.inner.cpu_write(addr, data),
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr < 0x2000 {
            // Notify MMC3 of the access so the IRQ counter sees the
            // A12 transition, then resolve the byte through the
            // remapped bank.
            let _ = self.inner.ppu_read(addr); // keeps any internal-side-effect parity
            self.read_chr_byte(addr)
        } else {
            0
        }
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if !self.inner.chr_is_ram() || addr >= 0x2000 {
            return;
        }
        // CHR-RAM path. The official multicart ships only CHR-ROM, so
        // this branch is dead in practice - but a homebrew that
        // (re-)labels itself as mapper 37 with CHR-RAM should still
        // see writes land in the same bank the read side resolves.
        let mmc3_bank = self.inner.chr_bank_for(addr);
        let bank = self.remap_chr(mmc3_bank);
        let chr = self.inner.chr_mut();
        let total_banks = (chr.len() / CHR_BANK_1K).max(1);
        let bank = bank % total_banks;
        let offset = (addr as usize) & (CHR_BANK_1K - 1);
        if let Some(b) = chr.get_mut(bank * CHR_BANK_1K + offset) {
            *b = data;
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.inner.mirroring()
    }

    fn on_cpu_cycle(&mut self) {
        self.inner.on_cpu_cycle();
    }

    fn on_ppu_addr(&mut self, addr: u16, ppu_cycle: u64, kind: PpuFetchKind) {
        self.inner.on_ppu_addr(addr, ppu_cycle, kind);
    }

    fn ppu_nametable_read(&mut self, slot: u8, offset: u16) -> NametableSource {
        self.inner.ppu_nametable_read(slot, offset)
    }

    fn ppu_nametable_write(
        &mut self,
        slot: u8,
        offset: u16,
        data: u8,
    ) -> NametableWriteTarget {
        self.inner.ppu_nametable_write(slot, offset, data)
    }

    fn irq_line(&self) -> bool {
        self.inner.irq_line()
    }

    fn audio_output(&self) -> Option<f32> {
        self.inner.audio_output()
    }

    fn save_data(&self) -> Option<&[u8]> {
        // No PRG-RAM on this cart per wiki, so always None - the inner
        // MMC3 has a RAM buffer but it's never observable to software,
        // and persisting it would just clutter the save dir.
        None
    }

    fn load_save_data(&mut self, _data: &[u8]) {}

    fn save_dirty(&self) -> bool {
        false
    }

    fn mark_saved(&mut self) {}

    fn save_state_capture(&self) -> Option<crate::save_state::MapperState> {
        // Capture the inner MMC3 (which produces a MapperState::Mmc3
        // variant) and unwrap to the Mmc3Snap. Fail-safe: if the
        // inner returns Unsupported (won't happen post-Phase 3a), we
        // bubble that up.
        let inner_state = self.inner.save_state_capture()?;
        let crate::save_state::MapperState::Mmc3(inner_snap) = inner_state else {
            return None;
        };
        Some(crate::save_state::MapperState::Mapper037(Box::new(
            crate::save_state::mapper::Mapper037Snap {
                inner: inner_snap,
                block: self.block,
            },
        )))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::Mapper037(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        // Need to repackage the inner Mmc3Snap (which is owned by `snap`)
        // as a borrowed MapperState::Mmc3 so the inner's apply can
        // pattern-match on it. Cheapest path: clone the inner snap.
        let inner_state = crate::save_state::MapperState::Mmc3(crate::save_state::mapper::Mmc3Snap {
            prg_ram: snap.inner.prg_ram.clone(),
            chr_ram_data: snap.inner.chr_ram_data.clone(),
            bank_select: snap.inner.bank_select,
            bank_regs: snap.inner.bank_regs,
            mirroring: snap.inner.mirroring,
            prg_ram_enabled: snap.inner.prg_ram_enabled,
            prg_ram_write_protected: snap.inner.prg_ram_write_protected,
            irq_latch: snap.inner.irq_latch,
            irq_counter: snap.inner.irq_counter,
            irq_reload: snap.inner.irq_reload,
            irq_enabled: snap.inner.irq_enabled,
            irq_line: snap.inner.irq_line,
            a12_low_since: snap.inner.a12_low_since,
            reg_a001: snap.inner.reg_a001,
            save_dirty: snap.inner.save_dirty,
        });
        self.inner.save_state_apply(&inner_state)?;
        self.block = snap.block;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, Mirroring, TvSystem};

    /// 256 KiB PRG (32 banks of 8 KiB), 256 KiB CHR (256 banks of
    /// 1 KiB). Each PRG / CHR bank tagged with its own index so a
    /// `cpu_peek` / `ppu_read` reveals which physical bank is mapped.
    fn cart() -> Cartridge {
        let mut prg = vec![0u8; 32 * PRG_BANK_8K];
        for bank in 0..32 {
            let base = bank * PRG_BANK_8K;
            prg[base..base + PRG_BANK_8K].fill(bank as u8);
        }
        let mut chr = vec![0u8; 256 * CHR_BANK_1K];
        for bank in 0..256 {
            let base = bank * CHR_BANK_1K;
            chr[base..base + CHR_BANK_1K].fill(bank as u8);
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: chr,
            chr_ram: false,
            mapper_id: 37,
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

    /// Convenience: select MMC3 PRG R6 = `r6`, R7 = `r7`, mode 0
    /// (so $8000 = R6, $A000 = R7).
    fn select_mmc3_prg(m: &mut Mapper037, r6: u8, r7: u8) {
        m.cpu_write(0x8000, 0x06); // bank-select index = 6, mode 0, no inversion
        m.cpu_write(0x8001, r6);
        m.cpu_write(0x8000, 0x07);
        m.cpu_write(0x8001, r7);
    }

    fn set_block(m: &mut Mapper037, b: u8) {
        m.cpu_write(0x6000, b);
    }

    #[test]
    fn block_0_through_2_lock_first_64k_window() {
        for b in 0..=2 {
            let mut m = Mapper037::new(cart());
            // R6 = 0 → $8000 reads bank index resolved through outer.
            select_mmc3_prg(&mut m, 0, 1);
            set_block(&mut m, b);
            // PRG bank for $8000 with R6=0, mode=0 → MMC3 bank 0.
            // Remap: base = 0, mask = (b<<1)|7 = 7 (b≤2). Result bank 0.
            assert_eq!(m.cpu_peek(0x8000), 0, "block {b}");
            // R7 = 1, $A000 → MMC3 bank 1 → remap → 1.
            assert_eq!(m.cpu_peek(0xA000), 1, "block {b}");
            // $C000 in mode 0 = MMC3 second-last (= bank 30 of 32),
            // remapped: base=0 mask=7 → 30 & 7 = 6.
            assert_eq!(m.cpu_peek(0xC000), 6, "block {b}");
            // $E000 = last (31) → 31 & 7 = 7.
            assert_eq!(m.cpu_peek(0xE000), 7, "block {b}");
        }
    }

    #[test]
    fn block_3_locks_second_64k_window() {
        let mut m = Mapper037::new(cart());
        select_mmc3_prg(&mut m, 0, 1);
        set_block(&mut m, 3);
        // base = 0x08, mask = 7. So bank = 8 | (mmc3_bank & 7).
        assert_eq!(m.cpu_peek(0x8000), 8); // R6=0 → 8
        assert_eq!(m.cpu_peek(0xA000), 9); // R7=1 → 9
        assert_eq!(m.cpu_peek(0xC000), 14); // 30 & 7 = 6 → 8|6 = 14
        assert_eq!(m.cpu_peek(0xE000), 15); // 31 & 7 = 7 → 8|7 = 15
    }

    #[test]
    fn block_4_through_6_open_full_128k_window() {
        for b in 4..=6 {
            let mut m = Mapper037::new(cart());
            select_mmc3_prg(&mut m, 0, 9);
            set_block(&mut m, b);
            // base = 0x10, mask = 0x0F.
            assert_eq!(m.cpu_peek(0x8000), 16, "block {b}"); // 16 | 0
            assert_eq!(m.cpu_peek(0xA000), 25, "block {b}"); // 16 | 9
            // C000/E000 untouched: second_last=30, last=31. With mask
            // 0x0F: (30 & 15) = 14, (31 & 15) = 15. base=0x10 → 30, 31.
            assert_eq!(m.cpu_peek(0xC000), 30, "block {b}");
            assert_eq!(m.cpu_peek(0xE000), 31, "block {b}");
        }
    }

    #[test]
    fn block_7_locks_last_64k_of_third_jail_cell() {
        let mut m = Mapper037::new(cart());
        select_mmc3_prg(&mut m, 0, 9);
        set_block(&mut m, 7);
        // base = 0x10 | 0x08 = 0x18 (24), mask = 0x0F.
        // R6=0 → 24 | 0 = 24
        // R7=9 → 24 | (9 & 15) = 24 | 9 = 25 (bit 4 already set in 24)
        // Wait - 24 = 0b11000. 9 = 0b01001. OR = 0b11001 = 25.
        assert_eq!(m.cpu_peek(0x8000), 24);
        assert_eq!(m.cpu_peek(0xA000), 25);
        // last = 31 = 0b11111. 31 & 15 = 15. 24 | 15 = 31.
        assert_eq!(m.cpu_peek(0xE000), 31);
    }

    #[test]
    fn chr_q2_bit_selects_upper_half() {
        // Stage R0 = 0 (2 KiB CHR @ $0000) and R2 = 0 (1 KiB @ $1000).
        let mut m = Mapper037::new(cart());
        m.cpu_write(0x8000, 0x00); // bank-select = R0
        m.cpu_write(0x8001, 0x00);
        m.cpu_write(0x8000, 0x02); // bank-select = R2
        m.cpu_write(0x8001, 0x00);

        // Block 0: CHR bank 0 → low half tag 0.
        set_block(&mut m, 0);
        assert_eq!(m.ppu_read(0x0000), 0);
        assert_eq!(m.ppu_read(0x1000), 0);

        // Block 4 (Q2=1): CHR bank 0 → 0x80, tag 128.
        set_block(&mut m, 4);
        assert_eq!(m.ppu_read(0x0000), 128);
        assert_eq!(m.ppu_read(0x1000), 128);
    }

    #[test]
    fn latch_capture_requires_prg_ram_writes_enabled() {
        let mut m = Mapper037::new(cart());
        // Default state: prg_ram_enabled = true, write-protect = false.
        set_block(&mut m, 3);
        select_mmc3_prg(&mut m, 0, 0);
        assert_eq!(m.cpu_peek(0x8000), 8, "should be in block 3");

        // Write-protect via $A001 bit 6.
        m.cpu_write(0xA001, 0x40);
        // Try to flip the latch - should be ignored.
        m.cpu_write(0x6000, 0x07);
        assert_eq!(m.cpu_peek(0x8000), 8, "latch should still report block 3");

        // Disable the chip entirely via $A001 bit 7 = 0 (already 0
        // after the bit-6-only write above, but explicit for clarity).
        m.cpu_write(0xA001, 0x00);
        m.cpu_write(0x6000, 0x07);
        assert_eq!(m.cpu_peek(0x8000), 8, "latch should still report block 3");

        // Re-enable + clear write-protect, then change the latch.
        m.cpu_write(0xA001, 0x80);
        m.cpu_write(0x6000, 0x07);
        // Now block = 7 → bank 24 at $8000.
        assert_eq!(m.cpu_peek(0x8000), 24);
    }

    #[test]
    fn mmc3_irq_pipeline_still_drives_irq_line() {
        // Quick smoke test that forwarding to inner MMC3 keeps the
        // IRQ counter alive. We don't simulate PPU A12 here - just
        // confirm `irq_line()` is reachable and starts low.
        let m = Mapper037::new(cart());
        assert!(!m.irq_line());
    }

    #[test]
    fn six_thousand_window_returns_open_bus() {
        let mut m = Mapper037::new(cart());
        m.cpu_write(0x6000, 0x05); // latch
        // Latched register is not readable per spec.
        assert_eq!(m.cpu_peek(0x6000), 0);
        assert_eq!(m.cpu_peek(0x7FFF), 0);
    }
}
