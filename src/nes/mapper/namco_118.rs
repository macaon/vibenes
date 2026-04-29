// SPDX-License-Identifier: GPL-3.0-or-later
//! Namco 118 family - iNES mappers 88, 95, 154, and 206 (a.k.a.
//! Namcot 108 / Mimic-1). The base chip is an MMC3-shaped banking
//! ASIC **without** the IRQ counter or the `$A000` mirroring
//! register, used by *DigDug II*, *Mappy-Land*, *Galaxian*, and
//! the rest of the Namcot licensed JP set. Three Namco-licensed
//! variants add quirks for specific carts:
//!
//! - **88** (*Devil Man*, *Mendel Palace*): an extra CHR address
//!   line that rigidly partitions the 64 KiB CHR-ROM - the 2 KiB
//!   `R0`/`R1` banks address the low 32 KiB, the 1 KiB `R2`-`R5`
//!   banks address the high 32 KiB.
//! - **95** (*Dragon Buster*): per-2 KiB-CHR-slot single-screen
//!   mirroring controlled by bit 5 of `R0` / `R1`.
//! - **154** (*Devil World JP*, *Wagyan Land*): dynamic
//!   single-screen mirroring driven by bit 6 of every `$8000`-
//!   `$9FFF` write, plus mapper-88's CHR partitioning.
//!
//! ## Register surface
//!
//! Writes are accepted only at `$8000`-`$9FFF` (the Namco 118's
//! mirroring / IRQ registers don't exist - those addresses are
//! ignored). Within the window:
//!
//! | Address parity | Effect                                                    |
//! |----------------|-----------------------------------------------------------|
//! | even (`$8000`) | Bank-select latch: bits 0-2 pick which R0..R7 to write    |
//! | odd  (`$8001`) | Bank-data: writes the selected register's value           |
//!
//! ## PRG / CHR layout
//!
//! Always:
//! - PRG: `R6` at `$8000-$9FFF`, `R7` at `$A000-$BFFF`, last 16 KiB
//!   of PRG hardwired to `$C000-$FFFF`.
//! - CHR: `R0` (2 KiB) at `$0000`, `R1` (2 KiB) at `$0800`,
//!   `R2`-`R5` (1 KiB each) at `$1000`-`$1FFF`.
//!
//! Internal indexing uses 1 KiB units across the board so the
//! 2 KiB R0/R1 banks resolve as `(R & 0xFE)` paired with the next
//! 1 KiB - a uniform shape borrowed from Mesen2's `BaseMapper`.
//!
//! References: NESdev wiki INES_Mapper_206 / 088 / 095 / 154,
//! Mesen2 `Core/NES/Mappers/Namco/Namco108*.h`, puNES
//! `src/core/mappers/N118.c` + `mapper_0{88,95,154,206}.c`.

use crate::nes::mapper::{Mapper, NametableSource, NametableWriteTarget};
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_8K: usize = 8 * 1024;
const CHR_BANK_1K: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// iNES 206 - the base Namco 118 / Mimic-1 chip. Mirroring
    /// hardwired by the cart, no special CHR / mirroring quirks.
    Mapper206,
    /// iNES 88 - the Namcot Type C "extra CHR address line" variant.
    /// 2 KiB banks land in the low half of CHR, 1 KiB banks in the
    /// high half.
    Mapper88,
    /// iNES 95 - Dragon Buster's per-CHR-slot single-screen
    /// mirroring trick. Bit 5 of R0 / R1 selects CIRAM A vs B per
    /// 2 KiB CHR window.
    Mapper95,
    /// iNES 154 - Devil World JP's dynamic single-screen
    /// mirroring (bit 6 of any `$8000`-`$9FFF` write) + the
    /// mapper-88 CHR partitioning trick.
    Mapper154,
}

pub struct Namco118 {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,
    prg_ram: Vec<u8>,

    variant: Variant,

    /// 8 bank registers. Indices 0-1 are 2 KiB CHR (paired in 1
    /// KiB units), 2-5 are 1 KiB CHR, 6-7 are 8 KiB PRG.
    bank_regs: [u8; 8],
    /// Latched R index from the most recent even-address write.
    /// Low 3 bits select which `bank_regs[i]` the next odd-address
    /// write targets.
    bank_select: u8,

    /// Effective mirroring as observed by the PPU. For mappers
    /// 206 / 88 it stays at the cart's hardwired value; for 154
    /// it tracks the most recent `$8000`-`$9FFF` write bit 6; for
    /// 95 it's a placeholder (the per-slot override in
    /// [`Self::ppu_nametable_read`] supersedes the flat field).
    mirroring: Mirroring,

    prg_bank_count_8k: usize,
    chr_bank_count_1k: usize,

    battery: bool,
    save_dirty: bool,
}

impl Namco118 {
    pub fn new(cart: Cartridge, variant: Variant) -> Self {
        let prg_bank_count_8k = (cart.prg_rom.len() / PRG_BANK_8K).max(1);
        let chr_ram = cart.chr_ram || cart.chr_rom.is_empty();
        let chr = if chr_ram {
            // No commercial Namco 118 cart ships CHR-RAM, but a
            // mis-tagged dump or homebrew might. Allocate one
            // 8 KiB bank.
            vec![0u8; 8 * 1024]
        } else {
            cart.chr_rom
        };
        let chr_bank_count_1k = (chr.len() / CHR_BANK_1K).max(1);
        let prg_ram_total = (cart.prg_ram_size + cart.prg_nvram_size).max(0x2000);
        Self {
            prg_rom: cart.prg_rom,
            chr,
            chr_ram,
            prg_ram: vec![0u8; prg_ram_total],
            variant,
            bank_regs: [0; 8],
            bank_select: 0,
            mirroring: cart.mirroring,
            prg_bank_count_8k,
            chr_bank_count_1k,
            battery: cart.battery_backed,
            save_dirty: false,
        }
    }

    pub fn new_206(cart: Cartridge) -> Self {
        Self::new(cart, Variant::Mapper206)
    }
    pub fn new_88(cart: Cartridge) -> Self {
        Self::new(cart, Variant::Mapper88)
    }
    pub fn new_95(cart: Cartridge) -> Self {
        Self::new(cart, Variant::Mapper95)
    }
    pub fn new_154(cart: Cartridge) -> Self {
        Self::new(cart, Variant::Mapper154)
    }

    /// Effective CHR bank value for register `idx`, factoring in
    /// the mapper-88 / 154 extra-CHR-line wiring. Indices 0-1 are
    /// the 2 KiB pair (low half on 88/154), 2-5 are 1 KiB (high
    /// half on 88/154).
    fn chr_reg(&self, idx: usize) -> u8 {
        let raw = self.bank_regs[idx];
        match self.variant {
            Variant::Mapper88 | Variant::Mapper154 => {
                if idx < 2 {
                    raw & 0x3F
                } else {
                    raw | 0x40
                }
            }
            _ => raw,
        }
    }

    fn prg_index(&self, addr: u16) -> usize {
        let bank = match addr {
            0x8000..=0x9FFF => self.bank_regs[6] as usize,
            0xA000..=0xBFFF => self.bank_regs[7] as usize,
            0xC000..=0xDFFF => self.prg_bank_count_8k.saturating_sub(2),
            0xE000..=0xFFFF => self.prg_bank_count_8k.saturating_sub(1),
            _ => 0,
        };
        let bank = bank % self.prg_bank_count_8k;
        bank * PRG_BANK_8K + (addr as usize & (PRG_BANK_8K - 1))
    }

    fn chr_index(&self, addr: u16) -> usize {
        // 8 1-KiB slots covering $0000-$1FFF. The 2 KiB R0/R1
        // banks cover slot pairs (0,1) and (2,3) by zeroing the
        // low bit of the bank index then ORing slot-parity.
        let slot = ((addr >> 10) & 0x07) as usize;
        let bank = match slot {
            0 => (self.chr_reg(0) as usize) & 0xFE,
            1 => ((self.chr_reg(0) as usize) & 0xFE) | 0x01,
            2 => (self.chr_reg(1) as usize) & 0xFE,
            3 => ((self.chr_reg(1) as usize) & 0xFE) | 0x01,
            4 => self.chr_reg(2) as usize,
            5 => self.chr_reg(3) as usize,
            6 => self.chr_reg(4) as usize,
            7 => self.chr_reg(5) as usize,
            _ => unreachable!(),
        };
        let bank = bank % self.chr_bank_count_1k;
        bank * CHR_BANK_1K + (addr as usize & (CHR_BANK_1K - 1))
    }
}

impl Mapper for Namco118 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => {
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
            0x8000..=0x9FFF => {
                // Mapper 154 captures bit 6 on every write in this
                // window (both even and odd addresses) for its
                // dynamic single-screen mirroring trick. Mesen2's
                // Namco108_154 fires this before delegating to the
                // bank-select / bank-data dispatch.
                if self.variant == Variant::Mapper154 {
                    self.mirroring = if (data & 0x40) != 0 {
                        Mirroring::SingleScreenUpper
                    } else {
                        Mirroring::SingleScreenLower
                    };
                }
                if (addr & 1) == 0 {
                    self.bank_select = data & 0x07;
                } else {
                    let idx = self.bank_select as usize;
                    self.bank_regs[idx] = data;
                }
            }
            // $A000-$FFFF writes are ignored: Namco 118 has no
            // mirroring register, no IRQ counter, and no PRG-RAM
            // protection latch.
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
        // Mapper 95 (Dragon Buster) routes each NT slot to CIRAM A
        // or B based on bit 5 of the corresponding 2 KiB CHR
        // register: slots 0/1 follow R0.b5, slots 2/3 follow R1.b5.
        // Every other variant defers to the standard mirroring
        // path via NametableSource::Default.
        if self.variant != Variant::Mapper95 {
            return NametableSource::Default;
        }
        let r = if slot < 2 {
            self.bank_regs[0]
        } else {
            self.bank_regs[1]
        };
        if (r & 0x20) == 0 {
            NametableSource::CiramA
        } else {
            NametableSource::CiramB
        }
    }

    fn ppu_nametable_write(
        &mut self,
        slot: u8,
        _offset: u16,
        _data: u8,
    ) -> NametableWriteTarget {
        if self.variant != Variant::Mapper95 {
            return NametableWriteTarget::Default;
        }
        let r = if slot < 2 {
            self.bank_regs[0]
        } else {
            self.bank_regs[1]
        };
        if (r & 0x20) == 0 {
            NametableWriteTarget::CiramA
        } else {
            NametableWriteTarget::CiramB
        }
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
        use crate::save_state::mapper::{MirroringSnap, Namco118Snap, Namco118VariantSnap};
        let v = match self.variant {
            Variant::Mapper206 => Namco118VariantSnap::Mapper206,
            Variant::Mapper88 => Namco118VariantSnap::Mapper88,
            Variant::Mapper95 => Namco118VariantSnap::Mapper95,
            Variant::Mapper154 => Namco118VariantSnap::Mapper154,
        };
        Some(crate::save_state::MapperState::Namco118(Namco118Snap {
            variant: v,
            prg_ram: self.prg_ram.clone(),
            chr_ram_data: if self.chr_ram { self.chr.clone() } else { Vec::new() },
            bank_regs: self.bank_regs,
            bank_select: self.bank_select,
            mirroring: MirroringSnap::from_live(self.mirroring),
            save_dirty: self.save_dirty,
        }))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        use crate::save_state::mapper::Namco118VariantSnap;
        let crate::save_state::MapperState::Namco118(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        // Cross-variant apply is rejected even though the file-
        // header check would have caught a wrong-mapper-id load
        // earlier - this is belt-and-suspenders. A mapper-95 snap
        // applied to a mapper-154 cart would silently produce
        // wrong mirroring, which is exactly the failure mode the
        // path-level CRC tagging plus this check are meant to
        // close.
        let want = match snap.variant {
            Namco118VariantSnap::Mapper206 => Variant::Mapper206,
            Namco118VariantSnap::Mapper88 => Variant::Mapper88,
            Namco118VariantSnap::Mapper95 => Variant::Mapper95,
            Namco118VariantSnap::Mapper154 => Variant::Mapper154,
        };
        if want != self.variant {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        }
        if snap.prg_ram.len() == self.prg_ram.len() {
            self.prg_ram.copy_from_slice(&snap.prg_ram);
        }
        if self.chr_ram && snap.chr_ram_data.len() == self.chr.len() {
            self.chr.copy_from_slice(&snap.chr_ram_data);
        }
        self.bank_regs = snap.bank_regs;
        self.bank_select = snap.bank_select;
        self.mirroring = snap.mirroring.to_live();
        self.save_dirty = snap.save_dirty;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, TvSystem};

    fn cart_with(prg_kib: usize, chr_kib: usize, mirror: Mirroring) -> Cartridge {
        Cartridge {
            prg_rom: vec![0u8; prg_kib * 1024],
            chr_rom: vec![0u8; chr_kib * 1024],
            chr_ram: false,
            mapper_id: 206,
            submapper: 0,
            mirroring: mirror,
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
    fn power_on_state_uses_cart_mirroring() {
        let m = Namco118::new_206(cart_with(64, 32, Mirroring::Vertical));
        assert_eq!(m.mirroring, Mirroring::Vertical);
        assert_eq!(m.bank_regs, [0; 8]);
        assert_eq!(m.bank_select, 0);
    }

    #[test]
    fn bank_select_then_data_writes_target_register() {
        let mut m = Namco118::new_206(cart_with(128, 64, Mirroring::Vertical));
        // Select R6 (PRG bank at $8000), then write 0x05.
        m.cpu_write(0x8000, 0x06);
        m.cpu_write(0x8001, 0x05);
        assert_eq!(m.bank_regs[6], 0x05);
        // Even-address writes also re-latch the select index.
        m.cpu_write(0x8000, 0x07);
        m.cpu_write(0x8001, 0x09);
        assert_eq!(m.bank_regs[7], 0x09);
    }

    #[test]
    fn writes_above_9fff_are_ignored() {
        let mut m = Namco118::new_206(cart_with(64, 32, Mirroring::Vertical));
        m.cpu_write(0x8000, 0x06);
        m.cpu_write(0xA001, 0x55); // ignored
        m.cpu_write(0xC000, 0xAA); // ignored
        assert_eq!(m.bank_regs[6], 0); // unchanged - $8001 was never hit
    }

    #[test]
    fn prg_c000_and_e000_fixed_to_last_two_banks() {
        // 64 KiB PRG → 8 banks of 8 KiB. $C000 = bank 6, $E000 = bank 7.
        let mut prg = vec![0u8; 64 * 1024];
        prg[6 * PRG_BANK_8K] = 0xCD; // sentinel at start of bank 6
        prg[7 * PRG_BANK_8K] = 0xEF; // sentinel at start of bank 7
        let cart = Cartridge {
            prg_rom: prg,
            chr_rom: vec![0u8; 32 * 1024],
            chr_ram: false,
            mapper_id: 206,
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
        };
        let mut m = Namco118::new_206(cart);
        assert_eq!(m.cpu_read(0xC000), 0xCD);
        assert_eq!(m.cpu_read(0xE000), 0xEF);
    }

    #[test]
    fn mapper88_chr_widening_partitions_low_high_halves() {
        let mut m = Namco118::new_88(cart_with(32, 64, Mirroring::Vertical));
        // R0 = 0xFF: 2 KiB bank should index into low half
        // (& 0x3F = 0x3F, paired = 0x3E + slot parity).
        m.cpu_write(0x8000, 0x00); // select R0
        m.cpu_write(0x8001, 0xFF); // write 0xFF
        assert_eq!(m.chr_reg(0), 0x3F);
        // R2 = 0x00: 1 KiB bank should index into high half
        // (| 0x40 = 0x40).
        m.cpu_write(0x8000, 0x02); // select R2
        m.cpu_write(0x8001, 0x00);
        assert_eq!(m.chr_reg(2), 0x40);
    }

    #[test]
    fn mapper95_dragon_buster_per_slot_mirroring_via_r0_r1_bit5() {
        let mut m = Namco118::new_95(cart_with(64, 16, Mirroring::Horizontal));
        // R0 bit 5 = 0, R1 bit 5 = 1 → slots 0/1 → CiramA, slots 2/3 → CiramB.
        m.cpu_write(0x8000, 0x00);
        m.cpu_write(0x8001, 0x00);
        m.cpu_write(0x8000, 0x01);
        m.cpu_write(0x8001, 0x20);
        assert_eq!(m.ppu_nametable_read(0, 0), NametableSource::CiramA);
        assert_eq!(m.ppu_nametable_read(1, 0), NametableSource::CiramA);
        assert_eq!(m.ppu_nametable_read(2, 0), NametableSource::CiramB);
        assert_eq!(m.ppu_nametable_read(3, 0), NametableSource::CiramB);
        // Flip both bits: routing inverts.
        m.cpu_write(0x8000, 0x00);
        m.cpu_write(0x8001, 0x20);
        m.cpu_write(0x8000, 0x01);
        m.cpu_write(0x8001, 0x00);
        assert_eq!(m.ppu_nametable_read(0, 0), NametableSource::CiramB);
        assert_eq!(m.ppu_nametable_read(2, 0), NametableSource::CiramA);
    }

    #[test]
    fn mapper154_devil_world_mirroring_toggles_on_8000_writes() {
        let mut m = Namco118::new_154(cart_with(128, 64, Mirroring::Horizontal));
        // Bit 6 set on bank-select write → SingleScreenUpper.
        m.cpu_write(0x8000, 0x40);
        assert_eq!(m.mirroring, Mirroring::SingleScreenUpper);
        // Bit 6 clear on bank-data write also flips it back.
        m.cpu_write(0x8001, 0x00);
        assert_eq!(m.mirroring, Mirroring::SingleScreenLower);
        // Any address in $8000-$9FFF participates.
        m.cpu_write(0x9123, 0x40);
        assert_eq!(m.mirroring, Mirroring::SingleScreenUpper);
    }

    #[test]
    fn mapper154_inherits_mapper88_chr_widening() {
        let mut m = Namco118::new_154(cart_with(128, 64, Mirroring::Horizontal));
        m.cpu_write(0x8000, 0x00);
        m.cpu_write(0x8001, 0xFF);
        assert_eq!(m.chr_reg(0), 0x3F);
        m.cpu_write(0x8000, 0x05);
        m.cpu_write(0x8001, 0x00);
        assert_eq!(m.chr_reg(5), 0x40);
    }

    #[test]
    fn mapper206_no_mirroring_or_chr_quirks() {
        let mut m = Namco118::new_206(cart_with(64, 32, Mirroring::Vertical));
        // Bit 6 in $8000 doesn't move mirroring on 206.
        m.cpu_write(0x8000, 0x40);
        assert_eq!(m.mirroring, Mirroring::Vertical);
        // CHR values pass through untouched.
        m.cpu_write(0x8000, 0x00);
        m.cpu_write(0x8001, 0xFF);
        assert_eq!(m.chr_reg(0), 0xFF);
    }
}
