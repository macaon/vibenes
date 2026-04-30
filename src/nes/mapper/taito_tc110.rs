// SPDX-License-Identifier: GPL-3.0-or-later
//! Taito TC-110 (iNES mapper 189).
//!
//! Glue chip Taito built around an MMC3 die for *Thundercade*
//! (also known as *Twin Formation*, Bandai/Tradewest 1989) and
//! *Master Fighter II / III* (Yoko Soft, ports of *Master Karate*).
//! Identical to MMC3 apart from one extra register at
//! `$4120-$7FFF` that overrides the entire `$8000-$FFFF` window
//! with a fixed 32 KiB PRG bank.
//!
//! ## Register surface
//!
//! ```text
//! $4120-$7FFF   AAAA BBBB -> 32 KiB PRG bank = (A | B) & 7
//! $8000-$FFFF   plain MMC3 (bank-select, IRQ, mirroring, etc.)
//! ```
//!
//! `A` and `B` are wired-OR onto the same physical bank lines, so
//! writing `0x10` and writing `0x01` produce identical mappings.
//! Three usable bits cap the cart at 256 KiB PRG (8 banks of 32
//! KiB) which is exactly what *Thundercade* and *Master Fighter II*
//! ship.
//!
//! `$6000-$7FFF` reads return open bus - no PRG-RAM on the cart.
//!
//! CHR banking, mirroring, A12-driven IRQ, and the `$A001` chip-
//! enable / write-protect bits are all forwarded to the inner
//! MMC3 unchanged.
//!
//! References:
//! - <https://www.nesdev.org/wiki/INES_Mapper_189>
//! - `~/Git/Mesen2/Core/NES/Mappers/Txc/MMC3_189.h`
//! - `~/Git/punes/src/core/mappers/mapper_189.c`

use crate::nes::mapper::mmc3::Mmc3;
use crate::nes::mapper::{Mapper, NametableSource, NametableWriteTarget, PpuFetchKind};
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_8K: usize = 8 * 1024;

pub struct TaitoTc110 {
    inner: Mmc3,
    /// Latched value at `$4120-$7FFF`. Bank index is recomputed
    /// at PRG-read time as `((reg | reg >> 4) & 7) * 4 + slot`.
    prg_reg: u8,
}

impl TaitoTc110 {
    pub fn new(cart: Cartridge) -> Self {
        Self {
            inner: Mmc3::new(cart),
            prg_reg: 0,
        }
    }

    fn read_prg_byte(&self, addr: u16) -> u8 {
        let slot = ((addr - 0x8000) >> 13) as usize; // 0..=3
        let base32k = ((self.prg_reg | (self.prg_reg >> 4)) & 0x07) as usize;
        let bank = base32k * 4 + slot;
        let rom = self.inner.prg_rom();
        let total_banks = (rom.len() / PRG_BANK_8K).max(1);
        let bank = bank % total_banks;
        let off = (addr as usize) & (PRG_BANK_8K - 1);
        *rom.get(bank * PRG_BANK_8K + off).unwrap_or(&0)
    }
}

impl Mapper for TaitoTc110 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => 0, // no PRG-RAM on the cart
            0x8000..=0xFFFF => self.read_prg_byte(addr),
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        match addr {
            // The TC-110's prg-bank latch sits across $4120-$7FFF.
            // Anything below $4120 is part of the 2A03 / APU window
            // and never reaches the cart's decoders.
            0x4120..=0x7FFF => {
                self.prg_reg = data;
            }
            0x8000..=0xFFFF => self.inner.cpu_write(addr, data),
            _ => {}
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        self.inner.ppu_read(addr)
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        self.inner.ppu_write(addr, data);
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

    // No PRG-RAM on the cart - skip the battery save plumbing.
    fn save_data(&self) -> Option<&[u8]> {
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
        Some(crate::save_state::MapperState::TaitoTc110(Box::new(
            crate::save_state::mapper::TaitoTc110Snap {
                inner: inner_snap,
                prg_reg: self.prg_reg,
            },
        )))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::TaitoTc110(snap) = state else {
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
        self.prg_reg = snap.prg_reg;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, Mirroring, TvSystem};

    /// 256 KiB PRG (32 banks of 8 KiB), 128 KiB CHR (128 banks of
    /// 1 KiB). Tag every PRG bank with its index in its first byte;
    /// fill rest with `0xFF`.
    fn cart() -> Cartridge {
        let mut prg = vec![0xFFu8; 32 * PRG_BANK_8K];
        for bank in 0..32 {
            prg[bank * PRG_BANK_8K] = bank as u8;
        }
        let chr = vec![0u8; 128 * 1024];
        Cartridge {
            prg_rom: prg,
            chr_rom: chr,
            chr_ram: false,
            mapper_id: 189,
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
    fn boot_state_locks_first_32k_window() {
        let m = TaitoTc110::new(cart());
        // prg_reg = 0 -> base32k = 0 -> banks 0,1,2,3.
        assert_eq!(m.cpu_peek(0x8000), 0);
        assert_eq!(m.cpu_peek(0xA000), 1);
        assert_eq!(m.cpu_peek(0xC000), 2);
        assert_eq!(m.cpu_peek(0xE000), 3);
    }

    #[test]
    fn write_to_4120_through_7fff_swaps_32k_bank() {
        let mut m = TaitoTc110::new(cart());
        // Base bank 5 -> banks 20,21,22,23.
        m.cpu_write(0x4120, 0x05);
        assert_eq!(m.cpu_peek(0x8000), 20);
        assert_eq!(m.cpu_peek(0xA000), 21);
        assert_eq!(m.cpu_peek(0xC000), 22);
        assert_eq!(m.cpu_peek(0xE000), 23);
        // Same chip register addressable through $7000.
        m.cpu_write(0x7000, 0x02);
        assert_eq!(m.cpu_peek(0x8000), 8);
        // ...and $5000.
        m.cpu_write(0x5000, 0x03);
        assert_eq!(m.cpu_peek(0x8000), 12);
    }

    #[test]
    fn high_and_low_nibble_are_or_ed() {
        let mut m = TaitoTc110::new(cart());
        // High nibble 0x10 -> base = 0x10 | 0x01 = 1.
        m.cpu_write(0x4120, 0x10);
        assert_eq!(m.cpu_peek(0x8000), 4);
        // Both nibbles set, distinct values: high = 6, low = 1.
        // OR -> 7. base32k = 7 -> banks 28-31.
        m.cpu_write(0x4120, 0x61);
        assert_eq!(m.cpu_peek(0x8000), 28);
        assert_eq!(m.cpu_peek(0xE000), 31);
    }

    #[test]
    fn mmc3_prg_writes_have_no_effect_on_8000_to_ffff() {
        // Mapper 189 hardwires $8000-$FFFF to a single 32K bank;
        // even after writing to MMC3's PRG bank-select registers,
        // the visible PRG should be the prg_reg-derived 32K block.
        let mut m = TaitoTc110::new(cart());
        m.cpu_write(0x4120, 0x02); // base = 2 -> banks 8..11
        m.cpu_write(0x8000, 0x06); // bank-select index 6
        m.cpu_write(0x8001, 0xFF); // try to set R6 = 31
        assert_eq!(m.cpu_peek(0x8000), 8);
        assert_eq!(m.cpu_peek(0xE000), 11);
    }

    #[test]
    fn six_thousand_window_returns_open_bus() {
        let mut m = TaitoTc110::new(cart());
        m.cpu_write(0x6000, 0x05);
        assert_eq!(m.cpu_peek(0x6000), 0);
        assert_eq!(m.cpu_peek(0x7FFF), 0);
    }

    #[test]
    fn save_state_round_trip_preserves_prg_reg_and_mmc3() {
        let mut m = TaitoTc110::new(cart());
        m.cpu_write(0x4120, 0x32);
        // Touch an MMC3 register too so the inner snap carries
        // something distinguishable.
        m.cpu_write(0x8000, 0x06);
        m.cpu_write(0x8001, 0x10);
        let snap = m.save_state_capture().unwrap();
        let mut fresh = TaitoTc110::new(cart());
        fresh.save_state_apply(&snap).unwrap();
        // OR(3, 2) = 3 -> base = 3 -> banks 12..15.
        assert_eq!(fresh.cpu_peek(0x8000), 12);
        assert_eq!(fresh.cpu_peek(0xE000), 15);
    }

    #[test]
    fn save_state_rejects_other_variants() {
        // Pretend someone hands us an Nrom snap. Should reject.
        let bad = crate::save_state::MapperState::Nrom(
            crate::save_state::mapper::NromSnap::default(),
        );
        let mut m = TaitoTc110::new(cart());
        match m.save_state_apply(&bad) {
            Err(crate::save_state::SaveStateError::UnsupportedMapper(_)) => {}
            other => panic!("expected UnsupportedMapper, got {other:?}"),
        }
    }
}
