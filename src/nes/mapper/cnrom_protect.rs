// SPDX-License-Identifier: GPL-3.0-or-later
//! CNROM with diode-array security (iNES mapper 185).
//!
//! Plain CNROM hardware (32 KiB fixed PRG at `$8000-$FFFF`, 8 KiB
//! fixed CHR-ROM, hardwired mirroring) gated by a CIC-style
//! diode-array on the cart. Writes to `$8000-$FFFF` latch a byte
//! (with bus conflicts ANDing the value against the visible PRG
//! byte). The diodes decode that latch into CHR-/OE: only specific
//! values keep CHR enabled. When CHR is disabled, PPU reads of
//! `$0000-$1FFF` return `$FF` (the pull-up resistor on D0 is what
//! lets the original *Mighty Bomb Jack* boot - it polls bit 0).
//!
//! ## Submapper-driven unlock condition
//!
//! - Submapper 0 (legacy heuristic): enable when
//!   `(value & 0x0F) != 0 && value != 0x13`. Covers all the
//!   commercial dumps that don't have an explicit submapper.
//! - Submapper 4: enable when `value & 0x03 == 0`.
//! - Submapper 5: enable when `value & 0x03 == 1`.
//! - Submapper 6: enable when `value & 0x03 == 2`.
//! - Submapper 7: enable when `value & 0x03 == 3`.
//!
//! Games: *B-Wings*, *Mighty Bomb Jack*, *Spelunker*, *Seicross*,
//! *Sansuu 1/2/3 Nen* (educational).
//!
//! References:
//! - <https://www.nesdev.org/wiki/INES_Mapper_185>
//! - `~/Git/Mesen2/Core/NES/Mappers/Nintendo/CnromProtect.h`
//! - `~/Git/punes/src/core/mappers/mapper_185.c`

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const CHR_BANK_8K: usize = 8 * 1024;

pub struct CnromProtect {
    prg_rom: Vec<u8>,
    chr_rom: Vec<u8>,
    /// 0 = legacy heuristic; 4-7 = deterministic value match on
    /// `latch & 0x03`. Other submapper IDs collapse to 0.
    submapper: u8,
    /// Latched write value. Boot value chosen so a fresh power-up
    /// has CHR disabled until the program writes a magic key,
    /// matching the real diode-array startup state.
    latch: u8,
    mirroring: Mirroring,
}

impl CnromProtect {
    pub fn new(cart: Cartridge) -> Self {
        let chr_rom = if cart.chr_rom.is_empty() {
            vec![0u8; CHR_BANK_8K]
        } else {
            cart.chr_rom
        };
        Self {
            prg_rom: cart.prg_rom,
            chr_rom,
            submapper: cart.submapper,
            latch: 0,
            mirroring: cart.mirroring,
        }
    }

    fn chr_enabled(&self) -> bool {
        match self.submapper {
            4 => self.latch & 0x03 == 0,
            5 => self.latch & 0x03 == 1,
            6 => self.latch & 0x03 == 2,
            7 => self.latch & 0x03 == 3,
            // Legacy heuristic for iNES-1.0 dumps.
            _ => self.latch & 0x0F != 0 && self.latch != 0x13,
        }
    }

    fn prg_byte(&self, addr: u16) -> u8 {
        let off = (addr - 0x8000) as usize;
        let len = self.prg_rom.len();
        if len == 0 {
            return 0;
        }
        // 32 KiB CNROM. NROM-128-style 16 KiB carts mirror; treat
        // anything else by modulo to be safe.
        *self.prg_rom.get(off % len).unwrap_or(&0)
    }
}

impl Mapper for CnromProtect {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        if addr >= 0x8000 {
            self.prg_byte(addr)
        } else {
            0
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        if addr >= 0x8000 {
            // Bus conflict: the diode array sees the AND of the CPU
            // value and the visible PRG byte.
            self.latch = data & self.prg_byte(addr);
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr < 0x2000 {
            if self.chr_enabled() {
                *self.chr_rom.get(addr as usize).unwrap_or(&0xFF)
            } else {
                // Open bus with D0 pull-up. Mighty Bomb Jack boot
                // depends on bit 0 reading high while locked.
                0xFF
            }
        } else {
            0
        }
    }

    fn ppu_write(&mut self, _addr: u16, _data: u8) {
        // CHR-ROM only.
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }

    fn save_state_capture(&self) -> Option<crate::save_state::MapperState> {
        use crate::save_state::mapper::CnromProtectSnap;
        Some(crate::save_state::MapperState::CnromProtect(
            CnromProtectSnap {
                latch: self.latch,
                submapper: self.submapper,
            },
        ))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::CnromProtect(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        if snap.submapper != self.submapper {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        }
        self.latch = snap.latch;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, Mirroring, TvSystem};

    /// 32 KiB PRG (filled with `$FF` so bus-conflict ANDs are
    /// no-ops by default; first byte tagged for prg_byte sanity).
    /// 8 KiB CHR-ROM tagged with `$AA` so we can tell ROM reads
    /// from the locked-state `$FF` placeholder.
    fn cart(submapper: u8) -> Cartridge {
        let mut prg = vec![0xFFu8; 32 * 1024];
        prg[0] = 0xCC;
        let chr = vec![0xAAu8; 8 * 1024];
        Cartridge {
            prg_rom: prg,
            chr_rom: chr,
            chr_ram: false,
            mapper_id: 185,
            submapper,
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
    fn boot_state_locks_chr() {
        let mut m = CnromProtect::new(cart(0));
        assert_eq!(m.ppu_read(0x0000), 0xFF);
        assert_eq!(m.ppu_read(0x1FFF), 0xFF);
    }

    #[test]
    fn submapper_0_heuristic_unlocks_on_nonzero_low_nibble() {
        // Write at $8001 where PRG byte is $FF so bus-conflict
        // AND is a no-op.
        let mut m = CnromProtect::new(cart(0));
        m.cpu_write(0x8001, 0x01);
        assert_eq!(m.ppu_read(0x0000), 0xAA);
        // $13 is the documented "stays locked" exception.
        m.cpu_write(0x8001, 0x13);
        assert_eq!(m.ppu_read(0x0000), 0xFF);
        // Pure-zero low nibble locks.
        m.cpu_write(0x8001, 0xF0);
        assert_eq!(m.ppu_read(0x0000), 0xFF);
        // Any other nonzero low nibble unlocks.
        m.cpu_write(0x8001, 0x21);
        assert_eq!(m.ppu_read(0x0000), 0xAA);
    }

    #[test]
    fn submapper_4_unlocks_on_low_two_bits_zero() {
        let mut m = CnromProtect::new(cart(4));
        m.cpu_write(0x8001, 0x01);
        assert_eq!(m.ppu_read(0x0000), 0xFF);
        m.cpu_write(0x8001, 0x04); // & 3 == 0
        assert_eq!(m.ppu_read(0x0000), 0xAA);
    }

    #[test]
    fn submappers_5_6_7_match_their_keys() {
        for (sub, key) in [(5u8, 1u8), (6, 2), (7, 3)] {
            let mut m = CnromProtect::new(cart(sub));
            m.cpu_write(0x8001, key);
            assert_eq!(m.ppu_read(0x0000), 0xAA, "sub {sub} key {key}");
            m.cpu_write(0x8001, (key + 1) & 0x03);
            assert_eq!(m.ppu_read(0x0000), 0xFF, "sub {sub} non-key");
        }
    }

    #[test]
    fn bus_conflict_ands_value_with_prg_byte() {
        // Every PRG byte except offset 0 is `$FF`, so AND is a
        // no-op away from $8000. At $8000 the PRG byte is `$CC`,
        // so the latch is masked.
        let mut m = CnromProtect::new(cart(4));
        // Without bus conflict, write 0x01 would lock submapper 4.
        // With bus conflict at $8000 (PRG byte 0xCC), 0x01 & 0xCC
        // == 0x00 -> sub 4 wants `& 3 == 0` -> unlocks.
        m.cpu_write(0x8000, 0x01);
        assert_eq!(m.ppu_read(0x0000), 0xAA);
    }

    #[test]
    fn ppu_writes_are_ignored() {
        let mut m = CnromProtect::new(cart(0));
        m.cpu_write(0x8001, 0x01); // unlock first
        let before = m.ppu_read(0x0000);
        m.ppu_write(0x0000, 0x55);
        assert_eq!(m.ppu_read(0x0000), before);
    }

    #[test]
    fn save_state_round_trip() {
        let mut m = CnromProtect::new(cart(5));
        m.cpu_write(0x8001, 0x01); // unlock sub 5
        let snap = m.save_state_capture().unwrap();
        let mut fresh = CnromProtect::new(cart(5));
        fresh.save_state_apply(&snap).unwrap();
        assert_eq!(fresh.ppu_read(0x0000), 0xAA);
    }

    #[test]
    fn save_state_rejects_cross_submapper() {
        let m = CnromProtect::new(cart(4));
        let snap = m.save_state_capture().unwrap();
        let mut other = CnromProtect::new(cart(5));
        match other.save_state_apply(&snap) {
            Err(crate::save_state::SaveStateError::UnsupportedMapper(_)) => {}
            other => panic!("expected UnsupportedMapper, got {other:?}"),
        }
    }
}
