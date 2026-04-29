// SPDX-License-Identifier: GPL-3.0-or-later
//! Taito X1-017 - iNES mapper 82. Used by *SD Keiji: Blader*
//! and *Kyonshiizu 2*. Battery-backed cart with three
//! independently-gated WRAM regions and an MMC3-style CHR
//! window swap.
//!
//! ## Memory map
//!
//! | Range          | Function                                                |
//! |----------------|---------------------------------------------------------|
//! | `$6000-$63FF`  | WRAM bank 0 (1 KiB), gated by permission latch 0 = `$CA` |
//! | `$6400-$67FF`  | WRAM bank 1 (1 KiB), same gate as bank 0                 |
//! | `$6800-$6BFF`  | WRAM bank 2 (1 KiB), gated by permission latch 1 = `$69` |
//! | `$6C00-$6FFF`  | WRAM bank 3 (1 KiB), same gate as bank 2                 |
//! | `$7000-$73FF`  | WRAM bank 4 (1 KiB), gated by permission latch 2 = `$84` |
//! | `$7EF0-$7EFC`  | Register window (no decoding past the listed addresses) |
//! | `$8000-$FFFF`  | PRG-ROM (4x 8 KiB slots; last slot fixed to last bank)  |
//!
//! Total WRAM: 5 KiB. The two 4 KiB pairs share a single
//! permission latch each because the cart hardware decodes
//! the gate at A12 + A11 granularity, not A10. Battery-backed
//! per Mesen2 (uses `PrgMemoryType::SaveRam` for all five
//! banks), so SD Keiji's record / Kyonshiizu 2's progress
//! survives a save / load cycle.
//!
//! ## Register surface (`addr` exact match)
//!
//! | Address       | Effect                                                |
//! |---------------|-------------------------------------------------------|
//! | `$7EF0-$7EF5` | CHR bank registers `R0-R5` (1 KiB units)              |
//! | `$7EF6`       | bit 0: mirroring (1 = vertical, 0 = horizontal); bit 1: CHR mode swap (R0/R1 at low or high half) |
//! | `$7EF7`       | WRAM permission latch 0 (unlocks banks 0/1 with `$CA`) |
//! | `$7EF8`       | WRAM permission latch 1 (unlocks banks 2/3 with `$69`) |
//! | `$7EF9`       | WRAM permission latch 2 (unlocks bank 4 with `$84`)    |
//! | `$7EFA-$7EFC` | PRG bank slots 0/1/2 (8 KiB; value `>> 2`)             |
//!
//! `$7EFD-$7EFF` and any address outside the listed ranges
//! are silently ignored (no IRQ counter on this chip - the
//! initial agent-research summary said there was, but
//! Mesen2's TaitoX1017.h has no IRQ logic at all and neither
//! known commercial cart needs it).
//!
//! ## CHR mode swap
//!
//! `$7EF6.b1` flips the CHR window layout MMC3-style:
//!
//! - mode 0: 2 KiB R0/R1 at PPU `$0000`-`$0FFF`,
//!   1 KiB R2-R5 at `$1000`-`$1FFF`.
//! - mode 1: 1 KiB R2-R5 at `$0000`-`$0FFF`,
//!   2 KiB R0/R1 at `$1000`-`$1FFF`.
//!
//! R0 / R1 always ignore bit 0 of their stored value (they're
//! 2 KiB banks paired with the next 1 KiB half).
//!
//! ## PRG bank decoding
//!
//! `$7EFA-$7EFC` writes go through a `value >> 2` shift before
//! reaching the bank index. Mapper 82 specifically (per
//! Mesen2 `MapperID == 82` branch) - the related unlicensed
//! mapper 552 uses a bit-scrambled decode we don't model.
//!
//! References:
//! - <https://www.nesdev.org/wiki/INES_Mapper_082>
//! - `~/Git/Mesen2/Core/NES/Mappers/Taito/TaitoX1017.h`
//! - `~/Git/punes/src/core/mappers/mapper_082.c`

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_8K: usize = 8 * 1024;
const CHR_BANK_1K: usize = 1024;
const WRAM_SIZE: usize = 5 * 1024;
/// Per-region unlock magic: `$CA` for banks 0+1, `$69` for
/// banks 2+3, `$84` for bank 4. Borrowed from Taito's
/// licensing-chip-era WRAM gates.
const RAM_UNLOCK: [u8; 3] = [0xCA, 0x69, 0x84];

pub struct TaitoX1017 {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,
    /// 5 KiB on-cart WRAM laid out as five contiguous 1 KiB
    /// banks. Battery-backed unconditionally on this chip.
    wram: [u8; WRAM_SIZE],

    /// `$7EF0`-`$7EF5` raw values. R0/R1 mask the LSB before
    /// indexing; R2-R5 use the value directly.
    chr_regs: [u8; 6],
    /// `$7EF6.b1`. 0 = R0/R1 at low CHR half (default mode),
    /// 1 = R0/R1 at high half (swapped).
    chr_mode: u8,
    /// `$7EF7` / `$7EF8` / `$7EF9` permission latches.
    ram_permission: [u8; 3],
    /// `$7EFA` / `$7EFB` / `$7EFC` PRG bank values, already
    /// `>> 2`-decoded at write time.
    prg_regs: [u8; 3],

    mirroring: Mirroring,

    prg_bank_count_8k: usize,
    chr_bank_count_1k: usize,

    save_dirty: bool,
}

impl TaitoX1017 {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count_8k = (cart.prg_rom.len() / PRG_BANK_8K).max(1);
        let chr_ram = cart.chr_ram || cart.chr_rom.is_empty();
        let chr = if chr_ram {
            vec![0u8; 8 * 1024]
        } else {
            cart.chr_rom
        };
        let chr_bank_count_1k = (chr.len() / CHR_BANK_1K).max(1);
        Self {
            prg_rom: cart.prg_rom,
            chr,
            chr_ram,
            wram: [0; WRAM_SIZE],
            chr_regs: [0; 6],
            chr_mode: 0,
            ram_permission: [0; 3],
            prg_regs: [0; 3],
            mirroring: cart.mirroring,
            prg_bank_count_8k,
            chr_bank_count_1k,
            save_dirty: false,
        }
    }

    /// Map a CPU address in `$6000-$73FF` to (permission-latch
    /// index, byte offset into `wram`). Returns `None` for
    /// addresses outside the WRAM window.
    fn wram_route(addr: u16) -> Option<(usize, usize)> {
        match addr {
            0x6000..=0x63FF => Some((0, (addr - 0x6000) as usize)),
            0x6400..=0x67FF => Some((0, (addr - 0x6400) as usize + 0x400)),
            0x6800..=0x6BFF => Some((1, (addr - 0x6800) as usize + 0x800)),
            0x6C00..=0x6FFF => Some((1, (addr - 0x6C00) as usize + 0xC00)),
            0x7000..=0x73FF => Some((2, (addr - 0x7000) as usize + 0x1000)),
            _ => None,
        }
    }

    fn perm_unlocked(&self, perm_idx: usize) -> bool {
        self.ram_permission
            .get(perm_idx)
            .copied()
            .map(|v| v == RAM_UNLOCK[perm_idx])
            .unwrap_or(false)
    }

    fn prg_index(&self, addr: u16) -> usize {
        let slot = (((addr - 0x8000) >> 13) & 0x03) as usize;
        let bank = if slot < 3 {
            self.prg_regs[slot] as usize
        } else {
            self.prg_bank_count_8k.saturating_sub(1)
        };
        let bank = bank % self.prg_bank_count_8k;
        bank * PRG_BANK_8K + (addr as usize & (PRG_BANK_8K - 1))
    }

    fn chr_index(&self, addr: u16) -> usize {
        let slot = ((addr >> 10) & 0x07) as usize;
        let bank = if self.chr_mode == 0 {
            // Mode 0: 2 KiB R0/R1 at low half, 1 KiB R2-R5 at high.
            match slot {
                0 => (self.chr_regs[0] & 0xFE) as usize,
                1 => (self.chr_regs[0] & 0xFE) as usize + 1,
                2 => (self.chr_regs[1] & 0xFE) as usize,
                3 => (self.chr_regs[1] & 0xFE) as usize + 1,
                4 => self.chr_regs[2] as usize,
                5 => self.chr_regs[3] as usize,
                6 => self.chr_regs[4] as usize,
                7 => self.chr_regs[5] as usize,
                _ => 0,
            }
        } else {
            // Mode 1: layout flipped.
            match slot {
                0 => self.chr_regs[2] as usize,
                1 => self.chr_regs[3] as usize,
                2 => self.chr_regs[4] as usize,
                3 => self.chr_regs[5] as usize,
                4 => (self.chr_regs[0] & 0xFE) as usize,
                5 => (self.chr_regs[0] & 0xFE) as usize + 1,
                6 => (self.chr_regs[1] & 0xFE) as usize,
                7 => (self.chr_regs[1] & 0xFE) as usize + 1,
                _ => 0,
            }
        };
        let bank = bank % self.chr_bank_count_1k;
        bank * CHR_BANK_1K + (addr as usize & (CHR_BANK_1K - 1))
    }
}

impl Mapper for TaitoX1017 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        if let Some((perm_idx, off)) = Self::wram_route(addr) {
            if !self.perm_unlocked(perm_idx) {
                return 0;
            }
            return self.wram[off];
        }
        match addr {
            0x8000..=0xFFFF => {
                let idx = self.prg_index(addr);
                self.prg_rom.get(idx).copied().unwrap_or(0)
            }
            _ => 0,
        }
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        if let Some((perm_idx, off)) = Self::wram_route(addr) {
            if !self.perm_unlocked(perm_idx) {
                return 0;
            }
            return self.wram[off];
        }
        match addr {
            0x8000..=0xFFFF => {
                let idx = self.prg_index(addr);
                self.prg_rom.get(idx).copied().unwrap_or(0)
            }
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        if let Some((perm_idx, off)) = Self::wram_route(addr) {
            if !self.perm_unlocked(perm_idx) {
                return;
            }
            if self.wram[off] != data {
                self.wram[off] = data;
                self.save_dirty = true;
            }
            return;
        }
        match addr {
            0x7EF0..=0x7EF5 => {
                self.chr_regs[(addr - 0x7EF0) as usize] = data;
            }
            0x7EF6 => {
                self.mirroring = if (data & 0x01) != 0 {
                    Mirroring::Vertical
                } else {
                    Mirroring::Horizontal
                };
                self.chr_mode = (data >> 1) & 0x01;
            }
            0x7EF7 => self.ram_permission[0] = data,
            0x7EF8 => self.ram_permission[1] = data,
            0x7EF9 => self.ram_permission[2] = data,
            // Mapper 82 takes the bank as `value >> 2`; the
            // related mapper 552 uses a bit-scrambled decode
            // that we don't model.
            0x7EFA => self.prg_regs[0] = data >> 2,
            0x7EFB => self.prg_regs[1] = data >> 2,
            0x7EFC => self.prg_regs[2] = data >> 2,
            _ => {}
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr < 0x2000 {
            let idx = self.chr_index(addr);
            self.chr.get(idx).copied().unwrap_or(0)
        } else {
            0
        }
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if !self.chr_ram || addr >= 0x2000 {
            return;
        }
        let idx = self.chr_index(addr);
        if let Some(slot) = self.chr.get_mut(idx) {
            *slot = data;
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }

    fn save_data(&self) -> Option<&[u8]> {
        // Always exposed - X1-017 carts are always
        // battery-backed per Mesen2's PrgMemoryType::SaveRam
        // mapping.
        Some(&self.wram)
    }

    fn load_save_data(&mut self, data: &[u8]) {
        if data.len() == self.wram.len() {
            self.wram.copy_from_slice(data);
        }
    }

    fn save_dirty(&self) -> bool {
        self.save_dirty
    }

    fn mark_saved(&mut self) {
        self.save_dirty = false;
    }

    fn save_state_capture(&self) -> Option<crate::save_state::MapperState> {
        use crate::save_state::mapper::{MirroringSnap, TaitoX1017Snap};
        Some(crate::save_state::MapperState::TaitoX1017(Box::new(
            TaitoX1017Snap {
                wram: self.wram,
                chr_ram_data: if self.chr_ram { self.chr.clone() } else { Vec::new() },
                chr_regs: self.chr_regs,
                chr_mode: self.chr_mode,
                ram_permission: self.ram_permission,
                prg_regs: self.prg_regs,
                mirroring: MirroringSnap::from_live(self.mirroring),
                save_dirty: self.save_dirty,
            },
        )))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::TaitoX1017(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        self.wram = snap.wram;
        if self.chr_ram && snap.chr_ram_data.len() == self.chr.len() {
            self.chr.copy_from_slice(&snap.chr_ram_data);
        }
        self.chr_regs = snap.chr_regs;
        self.chr_mode = snap.chr_mode;
        self.ram_permission = snap.ram_permission;
        self.prg_regs = snap.prg_regs;
        self.mirroring = snap.mirroring.to_live();
        self.save_dirty = snap.save_dirty;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, TvSystem};

    fn cart() -> Cartridge {
        Cartridge {
            prg_rom: vec![0u8; 0x40000], // 256 KiB → 32x 8 KiB
            chr_rom: vec![0u8; 0x20000], // 128 KiB → 128x 1 KiB
            chr_ram: false,
            mapper_id: 82,
            submapper: 0,
            mirroring: Mirroring::Horizontal,
            battery_backed: true,
            prg_ram_size: 0,
            prg_nvram_size: 0x1400,
            tv_system: TvSystem::Ntsc,
            is_nes2: false,
            prg_chr_crc32: 0,
            db_matched: false,
            fds_data: None,
        }
    }

    #[test]
    fn power_on_locks_all_three_wram_regions() {
        let mut m = TaitoX1017::new(cart());
        m.cpu_write(0x6000, 0x42);
        m.cpu_write(0x6800, 0x42);
        m.cpu_write(0x7000, 0x42);
        assert_eq!(m.cpu_read(0x6000), 0);
        assert_eq!(m.cpu_read(0x6800), 0);
        assert_eq!(m.cpu_read(0x7000), 0);
    }

    #[test]
    fn unlock_perm0_opens_first_two_banks_only() {
        let mut m = TaitoX1017::new(cart());
        m.cpu_write(0x7EF7, 0xCA); // perm[0]
        m.cpu_write(0x6000, 0xAA); // bank 0
        m.cpu_write(0x6400, 0xBB); // bank 1
        m.cpu_write(0x6800, 0xCC); // bank 2 - still locked
        assert_eq!(m.cpu_read(0x6000), 0xAA);
        assert_eq!(m.cpu_read(0x6400), 0xBB);
        assert_eq!(m.cpu_read(0x6800), 0); // perm[1] not unlocked
    }

    #[test]
    fn each_perm_uses_its_own_magic() {
        let mut m = TaitoX1017::new(cart());
        // Unlock all three with the right magics.
        m.cpu_write(0x7EF7, 0xCA);
        m.cpu_write(0x7EF8, 0x69);
        m.cpu_write(0x7EF9, 0x84);
        m.cpu_write(0x6800, 0xCC);
        m.cpu_write(0x7000, 0xDD);
        assert_eq!(m.cpu_read(0x6800), 0xCC);
        assert_eq!(m.cpu_read(0x7000), 0xDD);
        // Wrong magic at perm[2] re-locks bank 4.
        m.cpu_write(0x7EF9, 0x85);
        assert_eq!(m.cpu_read(0x7000), 0);
        // Banks under perm[0] / perm[1] still readable.
        assert_eq!(m.cpu_read(0x6800), 0xCC);
    }

    #[test]
    fn cross_perm_magics_dont_unlock_wrong_region() {
        let mut m = TaitoX1017::new(cart());
        // Write 0xCA (perm[0] magic) to perm[1] - shouldn't unlock bank 2.
        m.cpu_write(0x7EF8, 0xCA);
        m.cpu_write(0x6800, 0xCC);
        assert_eq!(m.cpu_read(0x6800), 0);
    }

    #[test]
    fn mirroring_register_decodes_bit_0() {
        let mut m = TaitoX1017::new(cart());
        m.cpu_write(0x7EF6, 0x01);
        assert_eq!(m.mirroring(), Mirroring::Vertical);
        m.cpu_write(0x7EF6, 0x00);
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
    }

    #[test]
    fn chr_mode_bit_swaps_window_layout() {
        let mut m = TaitoX1017::new(cart());
        // Mode 0: R0 (2K) maps to PPU $0000-$07FF.
        m.cpu_write(0x7EF0, 0x04); // R0 = 4 (1K-aligned)
        // ppu_read at $0000 should fetch from CHR bank 4.
        let mut chr_test = vec![0u8; 0x20000];
        chr_test[4 * CHR_BANK_1K] = 0x40;
        chr_test[5 * CHR_BANK_1K] = 0x50;
        m.chr = chr_test;
        assert_eq!(m.ppu_read(0x0000), 0x40);
        assert_eq!(m.ppu_read(0x0400), 0x50);
        // Switch to mode 1 (R0/R1 at high half).
        m.cpu_write(0x7EF6, 0x02);
        // Now $0000-$03FF resolves through R2 (which is 0).
        assert_eq!(m.ppu_read(0x0000), 0);
        // R0's bytes now appear at $1000-$17FF.
        assert_eq!(m.ppu_read(0x1000), 0x40);
        assert_eq!(m.ppu_read(0x1400), 0x50);
    }

    #[test]
    fn prg_bank_value_uses_shift_right_2() {
        let mut prg = vec![0u8; 0x40000];
        prg[3 * PRG_BANK_8K] = 0x33; // start of bank 3
        let cart = Cartridge {
            prg_rom: prg,
            ..cart()
        };
        let mut m = TaitoX1017::new(cart);
        // Game writes 0x0C → bank index = 0x0C >> 2 = 3.
        m.cpu_write(0x7EFA, 0x0C);
        assert_eq!(m.cpu_read(0x8000), 0x33);
    }

    #[test]
    fn prg_e000_fixed_to_last_bank() {
        let mut prg = vec![0u8; 0x40000];
        prg[0x3FFFF] = 0xAB;
        let cart = Cartridge {
            prg_rom: prg,
            ..cart()
        };
        let mut m = TaitoX1017::new(cart);
        assert_eq!(m.cpu_read(0xFFFF), 0xAB);
    }

    #[test]
    fn save_data_always_exposed_and_dirty_tracked() {
        let mut m = TaitoX1017::new(cart());
        assert_eq!(m.save_data().map(|s| s.len()), Some(WRAM_SIZE));
        m.cpu_write(0x7EF7, 0xCA);
        m.cpu_write(0x6100, 0xEE);
        assert!(m.save_dirty());
        m.mark_saved();
        assert!(!m.save_dirty());
    }
}
