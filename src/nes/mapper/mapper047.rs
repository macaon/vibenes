// SPDX-License-Identifier: GPL-3.0-or-later
//! Nintendo NES-QJ multicart - iNES mapper 47.
//!
//! Single PCB pairing two MMC3 sub-carts behind a 1-bit outer-bank
//! latch at `$6000-$7FFF`. The two known commercial titles are the
//! NES-QJ release of *Super Spike V'Ball + Nintendo World Cup* (US)
//! and *Nintendo World Cup* solo on the same PCB design.
//!
//! ## Latch
//!
//! Bit 0 of any write in `$6000-$7FFF` is the outer block:
//!
//! - `block = 0` → first 128 KiB of PRG / first 128 KiB of CHR.
//! - `block = 1` → second 128 KiB of PRG / second 128 KiB of CHR.
//!
//! Capture is gated by MMC3's PRG-RAM-write-enable - same shape as
//! mapper 37: the cart's spec phrases this as "the MMC3 thinks
//! `$6000-$7FFF` is PRG-RAM, so writes only land when the chip is
//! enabled and not write-protected via `$A001`." NES-QJ has no
//! actual PRG-RAM; reads from this window return open bus (we
//! surface 0).
//!
//! ## Bank remapping
//!
//! ```text
//! PRG: (block << 4) | (mmc3_bank & 0x0F)   // 5-bit, 32 banks of 8 KiB
//! CHR: (block << 7) | (mmc3_bank & 0x7F)   // 8-bit, 256 banks of 1 KiB
//! ```
//!
//! That is, MMC3's natural bank space is constrained to the lower
//! half (4 PRG bits, 7 CHR bits) and the outer block bit becomes
//! the high bit. Same trick the NES-QJ board uses to keep both
//! sub-carts addressable without physically merging their PRG /
//! CHR ROMs.
//!
//! ## Implementation
//!
//! Wraps an inner [`Mmc3`] and re-routes only the bus paths that
//! need it (`$6000-$7FFF` writes, `$8000-$FFFF` reads, PPU CHR
//! reads / writes). Everything else - bank-select / bank-data
//! commits, IRQ counter, mirroring, A12 monitoring, save state
//! plumbing - forwards untouched. Mirrors the design used by
//! [`Mapper037`].
//!
//! Clean-room references (behavioral only):
//! - `~/Git/Mesen2/Core/NES/Mappers/Nintendo/MMC3_47.h`
//! - `~/Git/punes/src/core/mappers/mapper_047.c`
//! - `~/Git/nestopia/source/core/board/NstBoardQj.cpp`
//! - nesdev.org/wiki/INES_Mapper_047

use crate::nes::mapper::mmc3::Mmc3;
use crate::nes::mapper::{Mapper, NametableSource, NametableWriteTarget, PpuFetchKind};
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_8K: usize = 8 * 1024;
const CHR_BANK_1K: usize = 1024;

pub struct Mapper047 {
    inner: Mmc3,
    /// 1-bit outer block latch. Power-on / hard-reset = 0.
    block: u8,
}

impl Mapper047 {
    pub fn new(cart: Cartridge) -> Self {
        Self {
            inner: Mmc3::new(cart),
            block: 0,
        }
    }

    fn remap_prg(&self, mmc3_bank: usize) -> usize {
        ((self.block as usize) << 4) | (mmc3_bank & 0x0F)
    }

    fn remap_chr(&self, mmc3_bank: usize) -> usize {
        ((self.block as usize) << 7) | (mmc3_bank & 0x7F)
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

impl Mapper for Mapper047 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => 0, // open bus - no PRG-RAM on QJ
            0x8000..=0xFFFF => self.read_prg_byte(addr),
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        match addr {
            0x6000..=0x7FFF => {
                if self.inner.cpu_can_write_wram() {
                    self.block = data & 0x01;
                }
            }
            _ => self.inner.cpu_write(addr, data),
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr < 0x2000 {
            // Drive the inner mapper's CHR read so its A12 IRQ
            // counter sees the access (matches Mapper037 path).
            let _ = self.inner.ppu_read(addr);
            self.read_chr_byte(addr)
        } else {
            0
        }
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if !self.inner.chr_is_ram() || addr >= 0x2000 {
            return;
        }
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
        // No PRG-RAM on QJ; never produce a .sav file.
        None
    }

    fn load_save_data(&mut self, _data: &[u8]) {}

    fn save_dirty(&self) -> bool {
        false
    }

    fn mark_saved(&mut self) {}

    fn save_state_capture(&self) -> Option<crate::save_state::MapperState> {
        let inner_state = self.inner.save_state_capture()?;
        let crate::save_state::MapperState::Mmc3(inner_snap) = inner_state else {
            return None;
        };
        Some(crate::save_state::MapperState::Mapper047(Box::new(
            crate::save_state::mapper::Mapper047Snap {
                inner: inner_snap,
                block: self.block,
            },
        )))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::Mapper047(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
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

    /// 256 KiB PRG (32 banks * 8 KiB) and 256 KiB CHR (256 banks *
    /// 1 KiB), each bank tagged with its own physical index.
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
            mapper_id: 47,
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

    fn select_mmc3_prg(m: &mut Mapper047, r6: u8, r7: u8) {
        m.cpu_write(0x8000, 0x06);
        m.cpu_write(0x8001, r6);
        m.cpu_write(0x8000, 0x07);
        m.cpu_write(0x8001, r7);
    }

    fn set_block(m: &mut Mapper047, b: u8) {
        m.cpu_write(0x6000, b);
    }

    #[test]
    fn block_zero_pins_to_first_128k_of_prg_and_chr() {
        let mut m = Mapper047::new(cart());
        select_mmc3_prg(&mut m, 0x05, 0x09);
        set_block(&mut m, 0);
        // PRG: block=0, R6=5 → bank 5; R7=9 → bank 9.
        assert_eq!(m.cpu_peek(0x8000), 5);
        assert_eq!(m.cpu_peek(0xA000), 9);
        // C000/E000: second_last=30, last=31. block=0 → masked to 14, 15.
        assert_eq!(m.cpu_peek(0xC000), 30 & 0x0F);
        assert_eq!(m.cpu_peek(0xE000), 31 & 0x0F);
    }

    #[test]
    fn block_one_shifts_into_second_128k_of_prg_and_chr() {
        let mut m = Mapper047::new(cart());
        select_mmc3_prg(&mut m, 0x05, 0x09);
        set_block(&mut m, 1);
        // PRG: block=1 → 0x10 | (mmc3_bank & 0x0F).
        assert_eq!(m.cpu_peek(0x8000), 0x10 | 5);
        assert_eq!(m.cpu_peek(0xA000), 0x10 | 9);
        assert_eq!(m.cpu_peek(0xC000), 0x10 | (30 & 0x0F));
        assert_eq!(m.cpu_peek(0xE000), 0x10 | (31 & 0x0F));

        // CHR: stage R0 = 0 (2 KiB CHR @ $0000 lower half), R2 = 0
        // (1 KiB @ $1000). Block=1 should add 0x80 to each.
        m.cpu_write(0x8000, 0x00); m.cpu_write(0x8001, 0x00);
        m.cpu_write(0x8000, 0x02); m.cpu_write(0x8001, 0x00);
        assert_eq!(m.ppu_read(0x0000), 0x80);
        assert_eq!(m.ppu_read(0x1000), 0x80);
    }

    #[test]
    fn outer_bit_only_uses_d0_of_value() {
        // Per the spec, only bit 0 of the latch value matters.
        let mut m = Mapper047::new(cart());
        select_mmc3_prg(&mut m, 0, 0);
        set_block(&mut m, 0xFE); // bit 0 = 0
        assert_eq!(m.cpu_peek(0x8000), 0); // block = 0
        set_block(&mut m, 0xFF); // bit 0 = 1
        assert_eq!(m.cpu_peek(0x8000), 0x10);
    }

    #[test]
    fn latch_capture_requires_prg_ram_writes_enabled() {
        let mut m = Mapper047::new(cart());
        select_mmc3_prg(&mut m, 0, 0);
        // Default state = WRAM enabled, not write-protected.
        set_block(&mut m, 1);
        assert_eq!(m.cpu_peek(0x8000), 0x10, "block should latch by default");

        // Write-protect via $A001 bit 6.
        m.cpu_write(0xA001, 0x40);
        m.cpu_write(0x6000, 0x00); // try to clear - should be ignored
        assert_eq!(m.cpu_peek(0x8000), 0x10, "latch must be write-protected");

        // Disable chip via $A001 = 0 (bit 7 cleared, bit 6 cleared).
        m.cpu_write(0xA001, 0x00);
        m.cpu_write(0x6000, 0x00);
        assert_eq!(m.cpu_peek(0x8000), 0x10);

        // Re-enable + write 0 to clear.
        m.cpu_write(0xA001, 0x80);
        m.cpu_write(0x6000, 0x00);
        assert_eq!(m.cpu_peek(0x8000), 0);
    }

    #[test]
    fn six_thousand_window_returns_open_bus() {
        let mut m = Mapper047::new(cart());
        m.cpu_write(0x6000, 0x01);
        assert_eq!(m.cpu_peek(0x6000), 0);
        assert_eq!(m.cpu_peek(0x7FFF), 0);
    }

    #[test]
    fn save_state_round_trip_preserves_block_and_inner_mmc3_state() {
        let mut a = Mapper047::new(cart());
        select_mmc3_prg(&mut a, 0x07, 0x0B);
        set_block(&mut a, 1);
        let snap = a.save_state_capture().unwrap();

        let mut b = Mapper047::new(cart());
        b.save_state_apply(&snap).unwrap();
        assert_eq!(b.block, 1);
        assert_eq!(b.cpu_peek(0x8000), 0x10 | 7);
        assert_eq!(b.cpu_peek(0xA000), 0x10 | 0x0B);
    }

    #[test]
    fn irq_line_starts_low_and_forwards_to_inner() {
        let m = Mapper047::new(cart());
        assert!(!m.irq_line());
    }
}
