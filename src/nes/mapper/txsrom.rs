// SPDX-License-Identifier: GPL-3.0-or-later
//! TxSROM / TLSROM / TKSROM - iNES mapper 118. A Nintendo
//! MMC3 derivative that swaps the standard `$A000` mirroring
//! register for **per-CHR-bank dynamic mirroring**: bit 7 of
//! each `$8001` bank-data write retargets a specific nametable
//! slot to CIRAM A or B, latched at write time.
//!
//! ## Mirroring trick
//!
//! On every `$8001` write, the value's bit 7 picks CIRAM bank A
//! (0) or B (1) for one or two nametable slots, depending on
//! the CHR window inversion bit (`$8000.b7`):
//!
//! | CHR mode (`$8000.b7`) | Bank reg written | Nametable slots updated |
//! |-----------------------|------------------|-------------------------|
//! | 0 (R0/R1 are 2 KiB)   | R0               | slots 0 and 1 (paired)  |
//! | 0                     | R1               | slots 2 and 3 (paired)  |
//! | 1 (R2-R5 are 1 KiB)   | R2               | slot 0                  |
//! | 1                     | R3               | slot 1                  |
//! | 1                     | R4               | slot 2                  |
//! | 1                     | R5               | slot 3                  |
//!
//! Writes to other registers (R6, R7, IRQ regs, etc.) and the
//! standard `$A000` mirroring register have no effect on the
//! per-slot routing - the prior assignments persist until the
//! game writes a new value to one of the relevant CHR registers.
//! Switching the CHR mode bit alone (without a new bank-data
//! write) does NOT recompute the assignments, so we cache them
//! in `nt_cache` rather than recomputing on every PPU
//! nametable fetch.
//!
//! ## What's preserved from MMC3
//!
//! Everything else is a verbatim MMC3: PRG layout (R6 + R7 +
//! two fixed banks at the top), CHR resolution, the IRQ
//! counter (Rev B semantics including the A12 ≥ 10-PPU-cycle
//! filter), PRG-RAM enable / write-protect at `$A001`. The
//! `$A000` standard mirroring register's writes are simply
//! ignored by the cart hardware.
//!
//! Carts: *Armadillo* (JP), *Goal! Two*, *Ys III: Wanderers
//! from Ys*. Mesen2 also lists *NES Open Tournament Golf*
//! (TLSROM) and a couple of Hudson titles.
//!
//! Implementation: thin wrapper around our existing [`Mmc3`]
//! (mirrors the [`crate::nes::mapper::mapper037::Mapper037`]
//! pattern). All bus access, IRQ ticking, and save-state PRG /
//! CHR routing flow through the inner mapper untouched. We
//! only intercept `$8001` writes (to update `nt_cache`) and
//! the PPU nametable hooks (to surface the cached routing).
//!
//! References:
//! - <https://www.nesdev.org/wiki/INES_Mapper_118>
//! - `~/Git/Mesen2/Core/NES/Mappers/Nintendo/TxSRom.h`
//! - `~/Git/punes/src/core/mappers/mapper_118.c`

use crate::nes::mapper::mmc3::Mmc3;
use crate::nes::mapper::{Mapper, NametableSource, NametableWriteTarget, PpuFetchKind};
use crate::nes::rom::{Cartridge, Mirroring};

pub struct Txsrom {
    inner: Mmc3,
    /// Per-NT-slot CIRAM bank assignment (0 = A, 1 = B). Latched
    /// on each `$8001` write that targets a relevant CHR bank
    /// register (R0/R1 in CHR mode 0, R2-R5 in CHR mode 1). Cold
    /// power-on value is 0 across the board, matching Mesen2's
    /// `BaseMapper::SetNametables(0,0,0,0)` default.
    nt_cache: [u8; 4],
}

impl Txsrom {
    pub fn new(cart: Cartridge) -> Self {
        Self {
            inner: Mmc3::new(cart),
            nt_cache: [0; 4],
        }
    }

    /// Apply the TxSROM mirroring trick after a `$8001` write.
    /// The inner [`Mmc3`] has already absorbed the write (so its
    /// `bank_select` index and `chr_inverted` flag reflect the
    /// register that was just updated); we read those back to
    /// decide which `nt_cache` slots to retarget.
    fn apply_8001_mirror(&mut self, value: u8) {
        let nt = (value >> 7) & 0x01;
        let chr_mode = self.inner.chr_inverted();
        let reg = self.inner.current_register_index();
        if !chr_mode {
            // CHR mode 0: R0 and R1 are the 2 KiB banks. Each
            // controls a paired NT slot range; R2-R5 writes
            // don't update mirroring in this mode.
            match reg {
                0 => {
                    self.nt_cache[0] = nt;
                    self.nt_cache[1] = nt;
                }
                1 => {
                    self.nt_cache[2] = nt;
                    self.nt_cache[3] = nt;
                }
                _ => {}
            }
        } else {
            // CHR mode 1: R2-R5 are the 1 KiB banks. Each
            // controls one NT slot 1:1; R0/R1 writes don't
            // update mirroring in this mode.
            match reg {
                2 => self.nt_cache[0] = nt,
                3 => self.nt_cache[1] = nt,
                4 => self.nt_cache[2] = nt,
                5 => self.nt_cache[3] = nt,
                _ => {}
            }
        }
    }

    fn slot_source(&self, slot: u8) -> NametableSource {
        if (slot as usize) >= self.nt_cache.len() {
            return NametableSource::Default;
        }
        if self.nt_cache[slot as usize] == 0 {
            NametableSource::CiramA
        } else {
            NametableSource::CiramB
        }
    }

    fn slot_target(&self, slot: u8) -> NametableWriteTarget {
        if (slot as usize) >= self.nt_cache.len() {
            return NametableWriteTarget::Default;
        }
        if self.nt_cache[slot as usize] == 0 {
            NametableWriteTarget::CiramA
        } else {
            NametableWriteTarget::CiramB
        }
    }
}

impl Mapper for Txsrom {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.inner.cpu_read(addr)
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        self.inner.cpu_peek(addr)
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        self.inner.cpu_write(addr, data);
        // `$8001` is the bank-data port. Mesen2's TxSRom override
        // handles the mirroring update *before* delegating to
        // MMC3 to keep its nametable cache fresh, but the Rust
        // ordering doesn't matter here - we read the current
        // register index after the inner write so the post-state
        // is identical.
        if (addr & 0xE001) == 0x8001 {
            self.apply_8001_mirror(data);
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        self.inner.ppu_read(addr)
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        self.inner.ppu_write(addr, data);
    }

    fn mirroring(&self) -> Mirroring {
        // Placeholder: never consulted by the PPU because
        // `ppu_nametable_read` below always returns an explicit
        // CIRAM A / B routing for every slot. We pick Horizontal
        // because it's the safest default if some future code
        // path leaks past the override.
        Mirroring::Horizontal
    }

    fn on_cpu_cycle(&mut self) {
        self.inner.on_cpu_cycle();
    }

    fn on_ppu_addr(&mut self, addr: u16, ppu_cycle: u64, kind: PpuFetchKind) {
        self.inner.on_ppu_addr(addr, ppu_cycle, kind);
    }

    fn ppu_nametable_read(&mut self, slot: u8, _offset: u16) -> NametableSource {
        self.slot_source(slot)
    }

    fn ppu_nametable_write(
        &mut self,
        slot: u8,
        _offset: u16,
        _data: u8,
    ) -> NametableWriteTarget {
        self.slot_target(slot)
    }

    fn irq_line(&self) -> bool {
        self.inner.irq_line()
    }

    fn audio_output(&self) -> Option<f32> {
        self.inner.audio_output()
    }

    fn save_data(&self) -> Option<&[u8]> {
        self.inner.save_data()
    }

    fn load_save_data(&mut self, data: &[u8]) {
        self.inner.load_save_data(data)
    }

    fn save_dirty(&self) -> bool {
        self.inner.save_dirty()
    }

    fn mark_saved(&mut self) {
        self.inner.mark_saved()
    }

    fn save_state_capture(&self) -> Option<crate::save_state::MapperState> {
        // Capture the inner MMC3 first, then promote to a
        // TxSROM-tagged variant. The inner returns
        // `MapperState::Mmc3(..)`; we unwrap to the bare
        // `Mmc3Snap` and rewrap so a Mmc3 (mapper-4) save can't
        // be applied to a TxSROM (mapper-118) cart even though
        // their banking state is structurally identical.
        let inner_state = self.inner.save_state_capture()?;
        let crate::save_state::MapperState::Mmc3(inner_snap) = inner_state else {
            return None;
        };
        Some(crate::save_state::MapperState::Txsrom(Box::new(
            crate::save_state::mapper::TxsromSnap {
                inner: inner_snap,
                nt_cache: self.nt_cache,
            },
        )))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::Txsrom(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        // Repackage the borrowed inner snap as an owned
        // `MapperState::Mmc3` so the inner mapper's apply path
        // (which pattern-matches on `MapperState::Mmc3`) accepts
        // it. Cloning is cheap relative to the apply work itself.
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
        self.nt_cache = snap.nt_cache;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, TvSystem};

    fn cart() -> Cartridge {
        Cartridge {
            prg_rom: vec![0u8; 0x40000], // 256 KiB PRG -> 32x 8 KiB banks
            chr_rom: vec![0u8; 0x20000], // 128 KiB CHR -> 128x 1 KiB banks
            chr_ram: false,
            mapper_id: 118,
            submapper: 0,
            mirroring: Mirroring::Vertical,
            battery_backed: false,
            prg_ram_size: 0x2000,
            prg_nvram_size: 0,
            tv_system: TvSystem::Ntsc,
            is_nes2: false,
            prg_chr_crc32: 0,
            db_matched: false,
            fds_data: None,
        }
    }

    /// Drive a `$8000` then `$8001` pair the way a game does.
    fn write_bank(m: &mut Txsrom, select: u8, data: u8) {
        m.cpu_write(0x8000, select);
        m.cpu_write(0x8001, data);
    }

    #[test]
    fn power_on_routes_all_slots_to_ciram_a() {
        let m = Txsrom::new(cart());
        for slot in 0..4 {
            assert_eq!(m.slot_source(slot), NametableSource::CiramA);
        }
    }

    #[test]
    fn chr_mode_0_r0_pairs_slots_0_and_1() {
        let mut m = Txsrom::new(cart());
        // bank-select index = 0 (R0), CHR mode bit 7 = 0.
        write_bank(&mut m, 0x00, 0x80); // bit 7 set → CIRAM B
        assert_eq!(m.slot_source(0), NametableSource::CiramB);
        assert_eq!(m.slot_source(1), NametableSource::CiramB);
        // Slots 2/3 unaffected by an R0 write in this mode.
        assert_eq!(m.slot_source(2), NametableSource::CiramA);
        assert_eq!(m.slot_source(3), NametableSource::CiramA);
    }

    #[test]
    fn chr_mode_0_r1_pairs_slots_2_and_3() {
        let mut m = Txsrom::new(cart());
        write_bank(&mut m, 0x01, 0x80); // R1, bit 7 set
        assert_eq!(m.slot_source(0), NametableSource::CiramA);
        assert_eq!(m.slot_source(1), NametableSource::CiramA);
        assert_eq!(m.slot_source(2), NametableSource::CiramB);
        assert_eq!(m.slot_source(3), NametableSource::CiramB);
    }

    #[test]
    fn chr_mode_1_r2_through_r5_route_one_slot_each() {
        let mut m = Txsrom::new(cart());
        // CHR mode bit 7 = 1 in $8000.
        write_bank(&mut m, 0x82, 0x80); // R2, CIRAM B → slot 0
        write_bank(&mut m, 0x83, 0x00); // R3, CIRAM A → slot 1
        write_bank(&mut m, 0x84, 0x80); // R4, CIRAM B → slot 2
        write_bank(&mut m, 0x85, 0x00); // R5, CIRAM A → slot 3
        assert_eq!(m.slot_source(0), NametableSource::CiramB);
        assert_eq!(m.slot_source(1), NametableSource::CiramA);
        assert_eq!(m.slot_source(2), NametableSource::CiramB);
        assert_eq!(m.slot_source(3), NametableSource::CiramA);
    }

    #[test]
    fn chr_mode_0_r2_through_r5_writes_dont_change_routing() {
        let mut m = Txsrom::new(cart());
        // R0/R1 set both NT halves.
        write_bank(&mut m, 0x00, 0x80);
        write_bank(&mut m, 0x01, 0x80);
        // R2-R5 writes in mode 0 must NOT update nt_cache, even
        // though their bit 7 differs.
        write_bank(&mut m, 0x02, 0x00);
        write_bank(&mut m, 0x03, 0x00);
        write_bank(&mut m, 0x04, 0x00);
        write_bank(&mut m, 0x05, 0x00);
        for slot in 0..4 {
            assert_eq!(
                m.slot_source(slot),
                NametableSource::CiramB,
                "slot {slot} must stay CIRAM B; R2-R5 don't update routing in mode 0",
            );
        }
    }

    #[test]
    fn chr_mode_switch_alone_does_not_recompute_routing() {
        let mut m = Txsrom::new(cart());
        // Set up mode-0 routing.
        write_bank(&mut m, 0x00, 0x80); // slots 0/1 → B
        write_bank(&mut m, 0x01, 0x00); // slots 2/3 → A
        // Switch to mode 1 by writing $8000 with bit 7 set,
        // selecting any register. No $8001 follow-up.
        m.cpu_write(0x8000, 0x82);
        // Routing must persist - Mesen2's TxSRom only updates the
        // nametable on $8001 writes, never on $8000-only mode
        // changes. Without the latch behavior, a game that toggles
        // CHR mode without re-issuing CHR bank writes would see its
        // mirroring snap to a different state.
        assert_eq!(m.slot_source(0), NametableSource::CiramB);
        assert_eq!(m.slot_source(1), NametableSource::CiramB);
        assert_eq!(m.slot_source(2), NametableSource::CiramA);
        assert_eq!(m.slot_source(3), NametableSource::CiramA);
    }

    #[test]
    fn a000_writes_dont_set_mirroring() {
        let mut m = Txsrom::new(cart());
        // Set routing via TxSROM mechanism.
        write_bank(&mut m, 0x00, 0x80); // slots 0/1 → B
        // MMC3-style $A000 mirroring write would normally flip
        // hardwired mirroring; on TxSROM it should have no
        // effect on per-slot routing. Inner Mmc3 absorbs it but
        // our mirroring() override and ppu_nametable_read
        // bypass any change.
        m.cpu_write(0xA000, 0x01);
        assert_eq!(m.slot_source(0), NametableSource::CiramB);
        assert_eq!(m.slot_source(1), NametableSource::CiramB);
    }

    #[test]
    fn r6_r7_writes_dont_change_routing() {
        let mut m = Txsrom::new(cart());
        write_bank(&mut m, 0x00, 0x80); // slots 0/1 → B
        // PRG bank writes (R6, R7) MUST NOT touch mirroring,
        // even though bit 7 of the data is set.
        write_bank(&mut m, 0x06, 0xFF);
        write_bank(&mut m, 0x07, 0xFF);
        assert_eq!(m.slot_source(0), NametableSource::CiramB);
        assert_eq!(m.slot_source(2), NametableSource::CiramA);
    }

    #[test]
    fn nametable_write_target_mirrors_read_routing() {
        let mut m = Txsrom::new(cart());
        write_bank(&mut m, 0x00, 0x80); // slots 0/1 → B
        write_bank(&mut m, 0x01, 0x00); // slots 2/3 → A
        assert_eq!(m.slot_target(0), NametableWriteTarget::CiramB);
        assert_eq!(m.slot_target(1), NametableWriteTarget::CiramB);
        assert_eq!(m.slot_target(2), NametableWriteTarget::CiramA);
        assert_eq!(m.slot_target(3), NametableWriteTarget::CiramA);
    }
}
