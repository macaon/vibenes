// SPDX-License-Identifier: GPL-3.0-or-later
//! Taito X1-005 - iNES mappers 80 and 207. The chip has the
//! widest licensed-cart footprint of any of the older Taito
//! mappers: *Wagyan Land 2*, *Wagyan Land 3*, *Famista 89 / 90 /
//! 91 / Pro*, *Bakushou!! Jinsei Gekijou 1 / 2 / 3*, *Don Doko
//! Don*, *Daikoukai Jidai*, *Yousuke Sega no Mahjong Kyoushitsu*,
//! and several more. Mapper 207 is the same silicon with one
//! pin reconfigured so the cart drives nametable A10 from CHR
//! bank-register bit 7 instead of from a global mirroring flag;
//! it's used by *Fudou Myou-Ou Den* (Demon Sword JP).
//!
//! ## Register window
//!
//! Writes are accepted only at the exact addresses `$7EF0`
//! through `$7EFF` (no decoding). `$8000-$FFFF` is read-only
//! PRG-ROM. The `$7F00-$7FFF` range hosts 128 bytes of WRAM
//! (battery-backed on the few save-bearing carts), mirrored
//! once at `$7F80-$7FFF`. WRAM access is gated on a magic
//! permission byte: writing `$A3` to either `$7EF8` or `$7EF9`
//! enables read/write; any other value disables.
//!
//! | Address       | Effect                                                |
//! |---------------|-------------------------------------------------------|
//! | `$7EF0`       | CHR 2 KiB pair: slot 0 = v, slot 1 = v + 1            |
//! | `$7EF1`       | CHR 2 KiB pair: slot 2 = v, slot 3 = v + 1            |
//! | `$7EF2-$7EF5` | CHR 1 KiB banks for slots 4 / 5 / 6 / 7               |
//! | `$7EF6-$7EF7` | Mirroring (bit 0: 1 = vertical, 0 = horizontal). For mapper 207 these are ignored. |
//! | `$7EF8-$7EF9` | WRAM permission latch (`$A3` enables `$7F00-$7FFF`)   |
//! | `$7EFA-$7EFB` | PRG slot 0 (8 KiB at `$8000`)                         |
//! | `$7EFC-$7EFD` | PRG slot 1 (8 KiB at `$A000`)                         |
//! | `$7EFE-$7EFF` | PRG slot 2 (8 KiB at `$C000`)                         |
//!
//! Slot 3 (`$E000-$FFFF`) is hardwired to the last 8 KiB PRG
//! bank. CHR `value + 1` (not `value | 1`) means a write of an
//! odd value to `$7EF0` / `$7EF1` lands on misaligned 1 KiB
//! banks - real carts always write even values, but we follow
//! the Mesen2 wraparound semantic so a misbehaving game just
//! sees what the hardware would.
//!
//! ## Mapper-207 mirroring trick
//!
//! When constructed via [`TaitoX1005::new_207`], bit 7 of the
//! values written to `$7EF0` / `$7EF1` drives per-NT-slot CIRAM
//! routing instead of the standard `$7EF6` mirroring register:
//! - `$7EF0.b7` selects CIRAM A (0) or B (1) for nametable
//!   slots 0 and 1.
//! - `$7EF1.b7` selects CIRAM for slots 2 and 3.
//!
//! `$7EF6` / `$7EF7` writes are silently ignored on mapper 207.
//! The implementation routes the override through the same
//! `ppu_nametable_read` / `ppu_nametable_write` API used by the
//! Namco 118 / TxSROM dynamic-mirroring chips.
//!
//! References:
//! - <https://www.nesdev.org/wiki/INES_Mapper_080>
//! - <https://www.nesdev.org/wiki/INES_Mapper_207>
//! - `~/Git/Mesen2/Core/NES/Mappers/Taito/TaitoX1005.h`
//! - `~/Git/punes/src/core/mappers/mapper_080.c`

use crate::nes::mapper::{Mapper, NametableSource, NametableWriteTarget};
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_8K: usize = 8 * 1024;
const CHR_BANK_1K: usize = 1024;
const WRAM_SIZE: usize = 128;
/// Magic byte the cart writes to `$7EF8` / `$7EF9` to unlock
/// the on-chip WRAM. Any other value re-locks. Borrowed from
/// the licensing-chip era - even non-battery X1-005 carts have
/// to write the magic before scratch-RAM access works.
const RAM_UNLOCK: u8 = 0xA3;

pub struct TaitoX1005 {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,
    /// 128 bytes of on-cart RAM mirrored at `$7F00-$7F7F` and
    /// `$7F80-$7FFF`. Battery-backed when the iNES header sets
    /// flag6 bit 1. Always present even on non-battery carts -
    /// the few games that need it (Bakushou! Jinsei Gekijou
    /// records) gate the access via the permission latch.
    wram: [u8; WRAM_SIZE],

    /// True for mapper 207 (Fudou Myou-Ou Den). Drives the
    /// per-NT-slot routing trick below.
    alternate_mirroring: bool,

    /// `$7EF0` / `$7EF1` raw values. Slot 0/1 read from the
    /// first; slot 2/3 from the second. Mapper 207 also
    /// consults bit 7 for nametable routing.
    chr_2k_regs: [u8; 2],
    /// `$7EF2`-`$7EF5`: 1 KiB CHR banks for the high half of
    /// the pattern table.
    chr_1k_regs: [u8; 4],
    /// `$7EFA` / `$7EFC` / `$7EFE`: 8 KiB PRG banks for
    /// `$8000` / `$A000` / `$C000`. Slot 3 (`$E000`) is fixed
    /// to the last bank.
    prg_regs: [u8; 3],
    /// Effective mirroring driven by `$7EF6`/`$7EF7` on mapper
    /// 80; placeholder on mapper 207 (the per-slot override
    /// supersedes it for both reads and writes).
    mirroring: Mirroring,
    /// Per-NT-slot CIRAM bank for mapper 207. Indices map to
    /// nametable slots 0..3. Indexed by `>= 2` for the
    /// `$7EF1.b7`-driven half. Always zero on mapper 80.
    nt_cache: [u8; 4],
    /// Latched `$7EF8` / `$7EF9` write value. WRAM access at
    /// `$7F00-$7FFF` is gated on this equaling [`RAM_UNLOCK`].
    ram_permission: u8,

    prg_bank_count_8k: usize,
    chr_bank_count_1k: usize,

    battery: bool,
    save_dirty: bool,
}

impl TaitoX1005 {
    pub fn new(cart: Cartridge) -> Self {
        Self::build(cart, false)
    }

    pub fn new_207(cart: Cartridge) -> Self {
        Self::build(cart, true)
    }

    fn build(cart: Cartridge, alternate_mirroring: bool) -> Self {
        let prg_bank_count_8k = (cart.prg_rom.len() / PRG_BANK_8K).max(1);
        let chr_ram = cart.chr_ram || cart.chr_rom.is_empty();
        let chr = if chr_ram {
            // No commercial X1-005 cart uses CHR-RAM, but a
            // mis-tagged dump or homebrew might.
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
            alternate_mirroring,
            chr_2k_regs: [0; 2],
            chr_1k_regs: [0; 4],
            prg_regs: [0; 3],
            mirroring: cart.mirroring,
            nt_cache: [0; 4],
            ram_permission: 0,
            prg_bank_count_8k,
            chr_bank_count_1k,
            battery: cart.battery_backed,
            save_dirty: false,
        }
    }

    fn ram_unlocked(&self) -> bool {
        self.ram_permission == RAM_UNLOCK
    }

    fn wram_index(addr: u16) -> usize {
        // 128-byte block mirrored once across $7F00-$7FFF: bit
        // 7 of the offset toggles between low and high copy,
        // both of which alias the same bytes.
        (addr as usize) & 0x7F
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
        let bank = match slot {
            0 => self.chr_2k_regs[0],
            1 => self.chr_2k_regs[0].wrapping_add(1),
            2 => self.chr_2k_regs[1],
            3 => self.chr_2k_regs[1].wrapping_add(1),
            4 => self.chr_1k_regs[0],
            5 => self.chr_1k_regs[1],
            6 => self.chr_1k_regs[2],
            7 => self.chr_1k_regs[3],
            _ => 0,
        } as usize;
        let bank = bank % self.chr_bank_count_1k;
        bank * CHR_BANK_1K + (addr as usize & (CHR_BANK_1K - 1))
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

impl Mapper for TaitoX1005 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        match addr {
            0x7F00..=0x7FFF if self.ram_unlocked() => {
                self.wram[Self::wram_index(addr)]
            }
            0x8000..=0xFFFF => {
                let idx = self.prg_index(addr);
                self.prg_rom.get(idx).copied().unwrap_or(0)
            }
            _ => 0,
        }
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
            0x7F00..=0x7FFF if self.ram_unlocked() => {
                self.wram[Self::wram_index(addr)]
            }
            0x8000..=0xFFFF => {
                let idx = self.prg_index(addr);
                self.prg_rom.get(idx).copied().unwrap_or(0)
            }
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        match addr {
            0x7F00..=0x7FFF => {
                if !self.ram_unlocked() {
                    return;
                }
                let i = Self::wram_index(addr);
                if self.wram[i] != data {
                    self.wram[i] = data;
                    if self.battery {
                        self.save_dirty = true;
                    }
                }
            }
            0x7EF0 => {
                self.chr_2k_regs[0] = data;
                if self.alternate_mirroring {
                    let nt = (data >> 7) & 0x01;
                    self.nt_cache[0] = nt;
                    self.nt_cache[1] = nt;
                }
            }
            0x7EF1 => {
                self.chr_2k_regs[1] = data;
                if self.alternate_mirroring {
                    let nt = (data >> 7) & 0x01;
                    self.nt_cache[2] = nt;
                    self.nt_cache[3] = nt;
                }
            }
            0x7EF2 => self.chr_1k_regs[0] = data,
            0x7EF3 => self.chr_1k_regs[1] = data,
            0x7EF4 => self.chr_1k_regs[2] = data,
            0x7EF5 => self.chr_1k_regs[3] = data,
            0x7EF6 | 0x7EF7 => {
                if !self.alternate_mirroring {
                    self.mirroring = if (data & 0x01) != 0 {
                        Mirroring::Vertical
                    } else {
                        Mirroring::Horizontal
                    };
                }
            }
            0x7EF8 | 0x7EF9 => self.ram_permission = data,
            0x7EFA | 0x7EFB => self.prg_regs[0] = data,
            0x7EFC | 0x7EFD => self.prg_regs[1] = data,
            0x7EFE | 0x7EFF => self.prg_regs[2] = data,
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

    fn ppu_nametable_read(&mut self, slot: u8, _offset: u16) -> NametableSource {
        if !self.alternate_mirroring {
            return NametableSource::Default;
        }
        self.slot_source(slot)
    }

    fn ppu_nametable_write(
        &mut self,
        slot: u8,
        _offset: u16,
        _data: u8,
    ) -> NametableWriteTarget {
        if !self.alternate_mirroring {
            return NametableWriteTarget::Default;
        }
        self.slot_target(slot)
    }

    fn save_data(&self) -> Option<&[u8]> {
        self.battery.then(|| self.wram.as_slice())
    }

    fn load_save_data(&mut self, data: &[u8]) {
        if self.battery && data.len() == self.wram.len() {
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
        use crate::save_state::mapper::{MirroringSnap, TaitoX1005Snap};
        Some(crate::save_state::MapperState::TaitoX1005(Box::new(
            TaitoX1005Snap {
                alternate_mirroring: self.alternate_mirroring,
                wram: self.wram,
                chr_ram_data: if self.chr_ram { self.chr.clone() } else { Vec::new() },
                chr_2k_regs: self.chr_2k_regs,
                chr_1k_regs: self.chr_1k_regs,
                prg_regs: self.prg_regs,
                mirroring: MirroringSnap::from_live(self.mirroring),
                nt_cache: self.nt_cache,
                ram_permission: self.ram_permission,
                save_dirty: self.save_dirty,
            },
        )))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::TaitoX1005(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        // The mirroring-variant flag is set at construction
        // and shouldn't change across a round trip. Reject if
        // it differs - that's a cross-mapper apply (80 ↔ 207)
        // that the file-header check should already have
        // caught upstream.
        if snap.alternate_mirroring != self.alternate_mirroring {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        }
        self.wram = snap.wram;
        if self.chr_ram && snap.chr_ram_data.len() == self.chr.len() {
            self.chr.copy_from_slice(&snap.chr_ram_data);
        }
        self.chr_2k_regs = snap.chr_2k_regs;
        self.chr_1k_regs = snap.chr_1k_regs;
        self.prg_regs = snap.prg_regs;
        self.mirroring = snap.mirroring.to_live();
        self.nt_cache = snap.nt_cache;
        self.ram_permission = snap.ram_permission;
        self.save_dirty = snap.save_dirty;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, TvSystem};

    fn cart() -> Cartridge {
        // 256 KiB PRG (32x 8 KiB), 128 KiB CHR (128x 1 KiB).
        Cartridge {
            prg_rom: vec![0u8; 0x40000],
            chr_rom: vec![0u8; 0x20000],
            chr_ram: false,
            mapper_id: 80,
            submapper: 0,
            mirroring: Mirroring::Horizontal,
            battery_backed: true,
            prg_ram_size: 0,
            prg_nvram_size: 0x80,
            tv_system: TvSystem::Ntsc,
            is_nes2: false,
            prg_chr_crc32: 0,
            db_matched: false,
            fds_data: None,
        }
    }

    #[test]
    fn power_on_locks_wram() {
        let mut m = TaitoX1005::new(cart());
        // Without the magic, $7F00-$7FFF reads return 0 and
        // writes drop.
        m.cpu_write(0x7F00, 0x42);
        assert_eq!(m.cpu_read(0x7F00), 0);
    }

    #[test]
    fn ram_unlock_magic_lets_wram_through() {
        let mut m = TaitoX1005::new(cart());
        m.cpu_write(0x7EF8, RAM_UNLOCK);
        m.cpu_write(0x7F00, 0x42);
        assert_eq!(m.cpu_read(0x7F00), 0x42);
        // Mirror at $7F80 sees the same byte.
        assert_eq!(m.cpu_read(0x7F80), 0x42);
        // 7EF9 also unlocks (per Mesen2 - both addresses share
        // the latch).
        m.cpu_write(0x7EF8, 0x00); // re-lock
        m.cpu_write(0x7EF9, RAM_UNLOCK);
        assert_eq!(m.cpu_read(0x7F00), 0x42);
    }

    #[test]
    fn wrong_magic_locks_wram() {
        let mut m = TaitoX1005::new(cart());
        m.cpu_write(0x7EF8, 0xA2); // close, but not 0xA3
        m.cpu_write(0x7F00, 0x42);
        assert_eq!(m.cpu_read(0x7F00), 0);
    }

    #[test]
    fn mirroring_register_toggles_h_v() {
        let mut m = TaitoX1005::new(cart());
        m.cpu_write(0x7EF6, 0x00);
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
        m.cpu_write(0x7EF6, 0x01);
        assert_eq!(m.mirroring(), Mirroring::Vertical);
        // $7EF7 mirrors the same effect.
        m.cpu_write(0x7EF7, 0x00);
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
    }

    #[test]
    fn mapper_207_routes_per_pair_via_chr_bank_bit_7() {
        let mut m = TaitoX1005::new_207(cart());
        // $7EF0.b7 = 1 → slots 0/1 → CIRAM B; $7EF1.b7 = 0 → slots 2/3 → CIRAM A.
        m.cpu_write(0x7EF0, 0x80);
        m.cpu_write(0x7EF1, 0x00);
        assert_eq!(m.ppu_nametable_read(0, 0), NametableSource::CiramB);
        assert_eq!(m.ppu_nametable_read(1, 0), NametableSource::CiramB);
        assert_eq!(m.ppu_nametable_read(2, 0), NametableSource::CiramA);
        assert_eq!(m.ppu_nametable_read(3, 0), NametableSource::CiramA);
    }

    #[test]
    fn mapper_207_ignores_7ef6_writes() {
        let mut m = TaitoX1005::new_207(cart());
        // Set up routing via $7EF0/$7EF1.
        m.cpu_write(0x7EF0, 0x00);
        m.cpu_write(0x7EF1, 0x00);
        let initial = m.ppu_nametable_read(0, 0);
        // Try to flip mirroring via $7EF6 - should not touch
        // the nt_cache routing on mapper 207.
        m.cpu_write(0x7EF6, 0x01);
        assert_eq!(m.ppu_nametable_read(0, 0), initial);
    }

    #[test]
    fn mapper_80_returns_default_nt_source() {
        let mut m = TaitoX1005::new(cart());
        m.cpu_write(0x7EF0, 0x80); // bit 7 set, but mapper 80 ignores
        assert_eq!(m.ppu_nametable_read(0, 0), NametableSource::Default);
    }

    #[test]
    fn prg_e000_fixed_to_last_bank() {
        let mut prg = vec![0u8; 0x40000];
        prg[0x3FFFF] = 0xAB; // top of last 8 KiB bank
        let cart = Cartridge {
            prg_rom: prg,
            ..cart()
        };
        let m = TaitoX1005::new(cart);
        // Read top of $E000-$FFFF without writing any prg
        // register - $E000 should resolve to the last bank.
        let mut m = m;
        assert_eq!(m.cpu_read(0xFFFF), 0xAB);
    }

    #[test]
    fn prg_slot_writes_select_8k_banks() {
        let mut prg = vec![0u8; 0x40000];
        prg[0x2000] = 0x11; // start of bank 1
        prg[0x4000] = 0x22; // start of bank 2
        prg[0x6000] = 0x33; // start of bank 3
        let cart = Cartridge {
            prg_rom: prg,
            ..cart()
        };
        let mut m = TaitoX1005::new(cart);
        m.cpu_write(0x7EFA, 0x01); // slot 0 = bank 1
        m.cpu_write(0x7EFC, 0x02); // slot 1 = bank 2
        m.cpu_write(0x7EFE, 0x03); // slot 2 = bank 3
        assert_eq!(m.cpu_read(0x8000), 0x11);
        assert_eq!(m.cpu_read(0xA000), 0x22);
        assert_eq!(m.cpu_read(0xC000), 0x33);
    }

    #[test]
    fn chr_2k_pair_uses_v_and_v_plus_1() {
        let mut chr = vec![0u8; 0x20000];
        chr[2 * CHR_BANK_1K] = 0x20; // start of bank 2
        chr[3 * CHR_BANK_1K] = 0x30; // start of bank 3
        let cart = Cartridge {
            chr_rom: chr,
            ..cart()
        };
        let mut m = TaitoX1005::new(cart);
        // $7EF0 = 2 → slot 0 = bank 2, slot 1 = bank 3.
        m.cpu_write(0x7EF0, 0x02);
        assert_eq!(m.ppu_read(0x0000), 0x20);
        assert_eq!(m.ppu_read(0x0400), 0x30);
    }

    #[test]
    fn save_data_only_when_battery() {
        let mut m = TaitoX1005::new(cart());
        // Battery is on by default in our test cart.
        assert!(m.save_data().is_some());
        // Write through the unlock + mark_saved cycle.
        m.cpu_write(0x7EF8, RAM_UNLOCK);
        m.cpu_write(0x7F00, 0xCD);
        assert!(m.save_dirty());
        m.mark_saved();
        assert!(!m.save_dirty());
    }
}
