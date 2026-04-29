// SPDX-License-Identifier: GPL-3.0-or-later
//! Sunsoft-4 - iNES mapper 68. Used by *After Burner II*, *After
//! Burner* (JP), *Maharaja* (JP), and *Ripple Island* (JP). The
//! signature feature is **CHR-as-nametable replacement**: bit 4 of
//! `$E000` redirects PPU nametable fetches at `$2000-$2FFF` into
//! CHR-ROM at offsets selected by a pair of 1 KiB bank registers
//! (`$C000` / `$D000`), which lets the cart store backgrounds as
//! "tile-of-tiles" CHR data instead of consuming nametable RAM.
//!
//! ## Register surface (`addr & 0xF000`)
//!
//! | Address  | Effect                                            |
//! |----------|---------------------------------------------------|
//! | `$8000`  | CHR bank 0 (2 KiB at PPU `$0000-$07FF`)           |
//! | `$9000`  | CHR bank 1 (`$0800-$0FFF`)                        |
//! | `$A000`  | CHR bank 2 (`$1000-$17FF`)                        |
//! | `$B000`  | CHR bank 3 (`$1800-$1FFF`)                        |
//! | `$C000`  | NTRAM bank register 0 (1 KiB CHR-ROM page)        |
//! | `$D000`  | NTRAM bank register 1                             |
//! | `$E000`  | bits 0-1: mirroring, bit 4: NTRAM enable          |
//! | `$F000`  | bits 0-3: PRG bank (16 KiB), bit 4: PRG-RAM enable|
//!
//! ## NTRAM (CHR-as-nametable) routing
//!
//! When `$E000.b4` is set, each of the four nametable slots draws
//! its bytes from CHR-ROM at offset `nt_regs[reg] * 0x400`,
//! addressable per slot via the following selector (per Mesen2,
//! cross-checked with puNES `mirroring_fix_068`):
//!
//! - Vertical mirroring   → slots `0/2` use `nt_regs[0]`, `1/3` use `nt_regs[1]`
//! - Horizontal mirroring → slots `0/1` use `nt_regs[0]`, `2/3` use `nt_regs[1]`
//! - Single-screen lower  → all slots use `nt_regs[0]`
//! - Single-screen upper  → all slots use `nt_regs[1]`
//!
//! Mesen2 force-sets bit 7 on the stored register value
//! (`value | 0x80`) - the purpose isn't documented but every
//! reference implementation does it, so we match. Likely a
//! protection-chip artifact that doesn't affect the bank index
//! since we mod by `chr_bank_count_1k` before indexing.
//!
//! ## Out of scope
//!
//! Sunsoft-4 submapper 1 ("FME-7-style" / Maeda-protected carts -
//! roughly the *Ripple Island* / *Sugoro Quest* set) carries an
//! external-ROM access window gated by a ~107 K-cycle licensing
//! timer triggered by `$6000-$7FFF` writes. None of the commercial
//! US releases use it; we don't model it.
//!
//! References:
//! - <https://www.nesdev.org/wiki/INES_Mapper_068>
//! - `~/Git/Mesen2/Core/NES/Mappers/Sunsoft/Sunsoft4.h`
//! - `~/Git/punes/src/core/mappers/mapper_068.c`

use crate::nes::mapper::{Mapper, NametableSource};
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_16K: usize = 16 * 1024;
const CHR_BANK_2K: usize = 2 * 1024;
const CHR_BANK_1K: usize = 1024;

pub struct Sunsoft4 {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,
    prg_ram: Vec<u8>,

    /// Switchable 16 KiB PRG bank at `$8000-$BFFF`. `$C000-$FFFF`
    /// is hardwired to the last bank.
    prg_bank: u8,
    /// `$F000.b4`. When clear, `$6000-$7FFF` reads return open bus
    /// (we surface 0) and writes are dropped.
    prg_ram_enabled: bool,

    /// Four 2 KiB CHR-ROM banks for the PPU pattern windows.
    chr_banks: [u8; 4],
    /// Two 1 KiB CHR-ROM bank registers used for NTRAM mode.
    /// Stored with bit 7 forced set per Mesen2 / puNES.
    nt_regs: [u8; 2],
    /// `$E000.b4`. When set, `ppu_nametable_read` substitutes a
    /// CHR byte for each NT fetch instead of falling through to
    /// CIRAM via the normal mirroring path.
    use_chr_for_nametables: bool,

    prg_bank_count_16k: usize,
    chr_bank_count_2k: usize,
    chr_bank_count_1k: usize,

    mirroring: Mirroring,

    battery: bool,
    save_dirty: bool,
}

impl Sunsoft4 {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count_16k = (cart.prg_rom.len() / PRG_BANK_16K).max(1);
        let chr_ram = cart.chr_ram || cart.chr_rom.is_empty();
        let chr = if chr_ram {
            // No commercial Sunsoft-4 cart ships CHR-RAM, but we
            // keep the path defensive so a homebrew / mis-tagged
            // dump doesn't fault on construction.
            vec![0u8; 8 * 1024]
        } else {
            cart.chr_rom
        };
        let chr_bank_count_2k = (chr.len() / CHR_BANK_2K).max(1);
        let chr_bank_count_1k = (chr.len() / CHR_BANK_1K).max(1);
        let prg_ram_total = (cart.prg_ram_size + cart.prg_nvram_size).max(0x2000);
        Self {
            prg_rom: cart.prg_rom,
            chr,
            chr_ram,
            prg_ram: vec![0u8; prg_ram_total],
            prg_bank: 0,
            prg_ram_enabled: false,
            chr_banks: [0; 4],
            // Mesen2 / puNES initial state: NTRAM regs power on
            // with 0 (the bit-7 force happens on write only).
            nt_regs: [0; 2],
            use_chr_for_nametables: false,
            prg_bank_count_16k,
            chr_bank_count_2k,
            chr_bank_count_1k,
            mirroring: cart.mirroring,
            battery: cart.battery_backed,
            save_dirty: false,
        }
    }

    fn prg_index(&self, addr: u16) -> usize {
        let bank = if addr < 0xC000 {
            (self.prg_bank as usize) % self.prg_bank_count_16k
        } else {
            self.prg_bank_count_16k - 1
        };
        let off = (addr as usize) & (PRG_BANK_16K - 1);
        bank * PRG_BANK_16K + off
    }

    fn chr_index(&self, addr: u16) -> usize {
        let slot = ((addr >> 11) & 0x03) as usize;
        let bank = (self.chr_banks[slot] as usize) % self.chr_bank_count_2k;
        let off = (addr as usize) & (CHR_BANK_2K - 1);
        bank * CHR_BANK_2K + off
    }

    /// Pick which `nt_regs[]` entry to consult for a given
    /// nametable slot (0..=3) under the current mirroring mode.
    /// Matches Mesen2's `UpdateNametables` selector and puNES's
    /// `mirroring_fix_068`.
    fn nt_reg_for_slot(&self, slot: u8) -> u8 {
        match self.mirroring {
            Mirroring::Vertical => slot & 0x01,
            Mirroring::Horizontal => (slot >> 1) & 0x01,
            Mirroring::SingleScreenLower => 0,
            Mirroring::SingleScreenUpper => 1,
            // Sunsoft-4 hardware doesn't drive 4-screen; fall back
            // to vertical so a misconfigured cart doesn't crash.
            Mirroring::FourScreen => slot & 0x01,
        }
    }
}

impl Mapper for Sunsoft4 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => {
                if !self.prg_ram_enabled {
                    return 0;
                }
                let i = (addr - 0x6000) as usize;
                self.prg_ram.get(i).copied().unwrap_or(0)
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
            0x6000..=0x7FFF => {
                if !self.prg_ram_enabled {
                    return 0;
                }
                let i = (addr - 0x6000) as usize;
                self.prg_ram.get(i).copied().unwrap_or(0)
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
            0x6000..=0x7FFF => {
                if !self.prg_ram_enabled {
                    return;
                }
                let i = (addr - 0x6000) as usize;
                if let Some(slot) = self.prg_ram.get_mut(i) {
                    if *slot != data {
                        *slot = data;
                        if self.battery {
                            self.save_dirty = true;
                        }
                    }
                }
            }
            0x8000..=0xFFFF => match addr & 0xF000 {
                0x8000 => self.chr_banks[0] = data,
                0x9000 => self.chr_banks[1] = data,
                0xA000 => self.chr_banks[2] = data,
                0xB000 => self.chr_banks[3] = data,
                0xC000 => self.nt_regs[0] = data | 0x80,
                0xD000 => self.nt_regs[1] = data | 0x80,
                0xE000 => {
                    self.mirroring = match data & 0x03 {
                        0 => Mirroring::Vertical,
                        1 => Mirroring::Horizontal,
                        2 => Mirroring::SingleScreenLower,
                        _ => Mirroring::SingleScreenUpper,
                    };
                    self.use_chr_for_nametables = (data & 0x10) != 0;
                }
                0xF000 => {
                    self.prg_bank = data & 0x0F;
                    self.prg_ram_enabled = (data & 0x10) != 0;
                }
                _ => {}
            },
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

    fn ppu_nametable_read(&mut self, slot: u8, offset: u16) -> NametableSource {
        if !self.use_chr_for_nametables {
            return NametableSource::Default;
        }
        let reg_idx = self.nt_reg_for_slot(slot) as usize;
        // Strip the bit-7 forced-set per Mesen2 before using as
        // bank index. The bit doesn't affect bank math (we mod by
        // count anyway), but keeping the strip explicit makes the
        // formula match the referenced source.
        let bank = (self.nt_regs[reg_idx] & 0x7F) as usize;
        let bank = bank % self.chr_bank_count_1k;
        let chr_off = bank * CHR_BANK_1K + (offset as usize & (CHR_BANK_1K - 1));
        let byte = self.chr.get(chr_off).copied().unwrap_or(0);
        NametableSource::Byte(byte)
    }

    fn save_data(&self) -> Option<&[u8]> {
        self.battery.then(|| self.prg_ram.as_slice())
    }

    fn load_save_data(&mut self, data: &[u8]) {
        if self.battery && data.len() == self.prg_ram.len() {
            self.prg_ram.copy_from_slice(data);
        }
    }

    fn save_dirty(&self) -> bool {
        self.save_dirty
    }

    fn mark_saved(&mut self) {
        self.save_dirty = false;
    }

    fn save_state_capture(&self) -> Option<crate::save_state::MapperState> {
        use crate::save_state::mapper::{MirroringSnap, Sunsoft4Snap};
        Some(crate::save_state::MapperState::Sunsoft4(Sunsoft4Snap {
            prg_ram: self.prg_ram.clone(),
            chr_ram_data: if self.chr_ram { self.chr.clone() } else { Vec::new() },
            prg_bank: self.prg_bank,
            prg_ram_enabled: self.prg_ram_enabled,
            chr_banks: self.chr_banks,
            nt_regs: self.nt_regs,
            use_chr_for_nametables: self.use_chr_for_nametables,
            mirroring: MirroringSnap::from_live(self.mirroring),
            save_dirty: self.save_dirty,
        }))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::Sunsoft4(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        if snap.prg_ram.len() == self.prg_ram.len() {
            self.prg_ram.copy_from_slice(&snap.prg_ram);
        }
        if self.chr_ram && snap.chr_ram_data.len() == self.chr.len() {
            self.chr.copy_from_slice(&snap.chr_ram_data);
        }
        self.prg_bank = snap.prg_bank;
        self.prg_ram_enabled = snap.prg_ram_enabled;
        self.chr_banks = snap.chr_banks;
        self.nt_regs = snap.nt_regs;
        self.use_chr_for_nametables = snap.use_chr_for_nametables;
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
            prg_rom: vec![0u8; 0x20000], // 128 KiB → 8× 16 KiB banks
            chr_rom: vec![0u8; 0x20000], // 128 KiB → 64× 2 KiB / 128× 1 KiB
            chr_ram: false,
            mapper_id: 68,
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

    #[test]
    fn power_on_state_is_safe_default() {
        let m = Sunsoft4::new(cart());
        assert_eq!(m.prg_bank, 0);
        assert!(!m.use_chr_for_nametables);
        assert!(!m.prg_ram_enabled);
    }

    #[test]
    fn nt_register_writes_force_bit7() {
        let mut m = Sunsoft4::new(cart());
        m.cpu_write(0xC000, 0x12);
        m.cpu_write(0xD000, 0x05);
        assert_eq!(m.nt_regs[0], 0x92);
        assert_eq!(m.nt_regs[1], 0x85);
    }

    #[test]
    fn e000_decodes_mirror_and_ntram_enable() {
        let mut m = Sunsoft4::new(cart());
        m.cpu_write(0xE000, 0x10); // NTRAM on, mirror = vertical
        assert!(m.use_chr_for_nametables);
        assert_eq!(m.mirroring, Mirroring::Vertical);
        m.cpu_write(0xE000, 0x01); // NTRAM off, mirror = horizontal
        assert!(!m.use_chr_for_nametables);
        assert_eq!(m.mirroring, Mirroring::Horizontal);
    }

    #[test]
    fn f000_gates_prg_ram_and_selects_bank() {
        let mut m = Sunsoft4::new(cart());
        // PRG-RAM gated off → reads return 0, writes drop.
        m.cpu_write(0xF000, 0x00);
        m.cpu_write(0x6000, 0xAB);
        assert_eq!(m.cpu_read(0x6000), 0);
        // Enable, write again, observe persistence.
        m.cpu_write(0xF000, 0x10);
        m.cpu_write(0x6000, 0xAB);
        assert_eq!(m.cpu_read(0x6000), 0xAB);
        // Bank index honors low 4 bits.
        m.cpu_write(0xF000, 0x13);
        assert_eq!(m.prg_bank, 0x03);
    }

    #[test]
    fn nametable_routing_picks_correct_register_per_mirror() {
        let mut m = Sunsoft4::new(cart());
        // After Burner-style: NTRAM on, vertical, distinct regs.
        m.cpu_write(0xC000, 0x00); // nt_regs[0] = 0x80, low bank index = 0
        m.cpu_write(0xD000, 0x01); // nt_regs[1] = 0x81, low bank index = 1
        m.cpu_write(0xE000, 0x10); // NTRAM on, vertical
        // Slot 0 → reg 0 → bank 0.
        // Slot 1 → reg 1 → bank 1.
        // We populate distinguishable sentinel bytes at the start
        // of each 1 KiB bank, then verify the routing.
        m.chr[0] = 0xAA;
        m.chr[CHR_BANK_1K] = 0xBB;
        match m.ppu_nametable_read(0, 0) {
            NametableSource::Byte(b) => assert_eq!(b, 0xAA),
            other => panic!("expected Byte, got {other:?}"),
        }
        match m.ppu_nametable_read(1, 0) {
            NametableSource::Byte(b) => assert_eq!(b, 0xBB),
            other => panic!("expected Byte, got {other:?}"),
        }
    }

    #[test]
    fn nametable_routing_off_returns_default() {
        let mut m = Sunsoft4::new(cart());
        m.cpu_write(0xE000, 0x01); // NTRAM disabled, mirror horizontal
        assert_eq!(
            m.ppu_nametable_read(0, 0),
            NametableSource::Default,
        );
    }
}
