// SPDX-License-Identifier: GPL-3.0-or-later
//! Codemasters / Camerica BF909x (iNES mapper 71).
//!
//! Two PCB variants share the mapper number:
//!
//! - **BF9093 / BF9094** (the common case, submapper 0): UNROM-style
//!   16 KiB PRG bank-switcher with hardwired mirroring. One register
//!   spans `$8000-$FFFF`; any write loads the bank index for the
//!   `$8000-$BFFF` window. `$C000-$FFFF` is fixed to the last bank.
//!   8 KiB CHR-RAM. Used by the bulk of the Camerica/Codemasters
//!   library: *Micro Machines*, *Bee 52*, *Big Nose*, *Quattro
//!   Adventure* (single-cart releases), etc.
//! - **BF9097** (submapper 1, plus the *Fire Hawk* runtime
//!   detection): adds a 1-screen mirroring control. Writes to
//!   `$8000-$BFFF` now set mirroring (bit 4: 0=screen A / lower,
//!   1=screen B / upper) instead of selecting a bank. PRG banking
//!   moves to `$C000-$FFFF` only.
//!
//! ## Auto-detection
//!
//! NES 2.0 dumps carry submapper 1 explicitly. iNES-1.0 dumps of
//! *Fire Hawk* don't, so we follow Mesen2's heuristic: any write
//! anywhere in `$9000-$9FFF` promotes the cart to BF9097 mode for
//! the rest of the run. The mapper starts in BF9093 mode unless the
//! header explicitly says otherwise.
//!
//! References:
//! - <https://www.nesdev.org/wiki/INES_Mapper_071>
//! - `~/Git/Mesen2/Core/NES/Mappers/Codemasters/BF909x.h`
//! - `~/Git/punes/src/core/mappers/mapper_071.c`

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_16K: usize = 16 * 1024;
const CHR_BANK_8K: usize = 8 * 1024;
const PRG_RAM_SIZE: usize = 8 * 1024;

pub struct CodemastersBf909x {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,
    prg_ram: Vec<u8>,

    /// Selected 16 KiB bank for the `$8000-$BFFF` window.
    prg_bank: u8,
    /// `true` once the cart has been promoted to BF9097 (either via
    /// NES 2.0 submapper 1 or by writing anywhere in `$9000-$9FFF`).
    bf9097_mode: bool,

    mirroring: Mirroring,
    prg_bank_count_16k: usize,
}

impl CodemastersBf909x {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count_16k = (cart.prg_rom.len() / PRG_BANK_16K).max(1);
        let is_chr_ram = cart.chr_ram || cart.chr_rom.is_empty();
        let chr = if is_chr_ram {
            vec![0u8; CHR_BANK_8K]
        } else {
            cart.chr_rom
        };
        let prg_ram = vec![0u8; (cart.prg_ram_size + cart.prg_nvram_size).max(PRG_RAM_SIZE)];

        Self {
            prg_rom: cart.prg_rom,
            chr,
            chr_ram: is_chr_ram,
            prg_ram,
            prg_bank: 0,
            bf9097_mode: cart.submapper == 1,
            mirroring: cart.mirroring,
            prg_bank_count_16k,
        }
    }

    fn switch_bank_base(&self) -> usize {
        ((self.prg_bank as usize) % self.prg_bank_count_16k) * PRG_BANK_16K
    }

    fn fixed_bank_base(&self) -> usize {
        (self.prg_bank_count_16k - 1) * PRG_BANK_16K
    }
}

impl Mapper for CodemastersBf909x {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => {
                let i = (addr - 0x6000) as usize;
                *self.prg_ram.get(i).unwrap_or(&0)
            }
            0x8000..=0xBFFF => {
                let i = self.switch_bank_base() + (addr - 0x8000) as usize;
                *self.prg_rom.get(i).unwrap_or(&0)
            }
            0xC000..=0xFFFF => {
                let i = self.fixed_bank_base() + (addr - 0xC000) as usize;
                *self.prg_rom.get(i).unwrap_or(&0)
            }
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        match addr {
            0x6000..=0x7FFF => {
                let i = (addr - 0x6000) as usize;
                if let Some(slot) = self.prg_ram.get_mut(i) {
                    *slot = data;
                }
            }
            0x8000..=0xFFFF => {
                // Fire Hawk's `$9000` writes promote the cart to
                // BF9097 even when the header didn't flag it.
                if (addr & 0xF000) == 0x9000 {
                    self.bf9097_mode = true;
                }

                if addr >= 0xC000 || !self.bf9097_mode {
                    self.prg_bank = data;
                } else {
                    // BF9097: $8000-$BFFF write sets 1-screen mirror.
                    self.mirroring = if data & 0x10 != 0 {
                        Mirroring::SingleScreenUpper
                    } else {
                        Mirroring::SingleScreenLower
                    };
                }
            }
            _ => {}
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr < 0x2000 {
            *self.chr.get(addr as usize).unwrap_or(&0)
        } else {
            0
        }
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if self.chr_ram && addr < 0x2000 {
            if let Some(slot) = self.chr.get_mut(addr as usize) {
                *slot = data;
            }
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }

    fn save_state_capture(&self) -> Option<crate::save_state::MapperState> {
        use crate::save_state::mapper::{CodemastersBf909xSnap, MirroringSnap};
        Some(crate::save_state::MapperState::CodemastersBf909x(
            CodemastersBf909xSnap {
                chr_ram_data: if self.chr_ram { self.chr.clone() } else { Vec::new() },
                prg_bank: self.prg_bank,
                bf9097_mode: self.bf9097_mode,
                mirroring: MirroringSnap::from_live(self.mirroring),
            },
        ))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::CodemastersBf909x(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        if self.chr_ram && snap.chr_ram_data.len() == self.chr.len() {
            self.chr.copy_from_slice(&snap.chr_ram_data);
        }
        self.prg_bank = snap.prg_bank;
        self.bf9097_mode = snap.bf9097_mode;
        self.mirroring = snap.mirroring.to_live();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, Mirroring, TvSystem};

    /// 256 KiB PRG (16 banks of 16 KiB), CHR-RAM. Tag the first
    /// byte of each bank with its index for sanity checks.
    fn cart(submapper: u8) -> Cartridge {
        let mut prg = vec![0xFFu8; 16 * PRG_BANK_16K];
        for bank in 0..16 {
            prg[bank * PRG_BANK_16K] = bank as u8;
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: Vec::new(),
            chr_ram: true,
            mapper_id: 71,
            submapper,
            mirroring: Mirroring::Vertical,
            battery_backed: false,
            prg_ram_size: 0x2000,
            prg_nvram_size: 0,
            tv_system: TvSystem::Ntsc,
            is_nes2: true,
            prg_chr_crc32: 0,
            db_matched: false,
            fds_data: None,
        }
    }

    #[test]
    fn boot_state_first_at_8000_last_at_c000() {
        let m = CodemastersBf909x::new(cart(0));
        assert_eq!(m.cpu_peek(0x8000), 0);
        assert_eq!(m.cpu_peek(0xC000), 15);
    }

    #[test]
    fn write_anywhere_in_8000_ffff_swaps_low_window_in_default_mode() {
        let mut m = CodemastersBf909x::new(cart(0));
        m.cpu_write(0x8000, 5);
        assert_eq!(m.cpu_peek(0x8000), 5);
        // High window is fixed.
        assert_eq!(m.cpu_peek(0xC000), 15);
        // Other addresses in $8000-$FFFF also load the bank in
        // BF9093 mode - no mirroring magic.
        m.cpu_write(0xABCD, 7);
        assert_eq!(m.cpu_peek(0x8000), 7);
        m.cpu_write(0xFFFF, 9);
        assert_eq!(m.cpu_peek(0x8000), 9);
    }

    #[test]
    fn submapper_1_8000_bfff_sets_mirroring() {
        let mut m = CodemastersBf909x::new(cart(1));
        // Bit 4 = 0 -> screen A (lower).
        m.cpu_write(0x9000, 0x00);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenLower);
        // Bit 4 = 1 -> screen B (upper).
        m.cpu_write(0x9000, 0x10);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenUpper);
        // PRG bank untouched by the mirroring write.
        assert_eq!(m.cpu_peek(0x8000), 0);
        // C000+ still selects bank.
        m.cpu_write(0xC000, 4);
        assert_eq!(m.cpu_peek(0x8000), 4);
    }

    #[test]
    fn fire_hawk_auto_promotes_on_9000_write() {
        // Header is iNES 1.0 style (submapper 0) but the game
        // writes to $9000 - we should latch BF9097 mode and treat
        // that very write as a mirroring update.
        let mut m = CodemastersBf909x::new(cart(0));
        assert!(!m.bf9097_mode);
        m.cpu_write(0x9000, 0x10);
        assert!(m.bf9097_mode);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenUpper);
        // After auto-promotion, $8000 also routes to mirroring.
        m.cpu_write(0x8000, 0x00);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenLower);
        // Bank load only happens via $C000+.
        m.cpu_write(0xC000, 6);
        assert_eq!(m.cpu_peek(0x8000), 6);
    }

    #[test]
    fn chr_ram_round_trip() {
        let mut m = CodemastersBf909x::new(cart(0));
        m.ppu_write(0x0123, 0xAB);
        assert_eq!(m.ppu_read(0x0123), 0xAB);
        m.ppu_write(0x1FFF, 0x55);
        assert_eq!(m.ppu_read(0x1FFF), 0x55);
    }

    #[test]
    fn prg_ram_passes_through() {
        let mut m = CodemastersBf909x::new(cart(0));
        m.cpu_write(0x6000, 0x42);
        assert_eq!(m.cpu_peek(0x6000), 0x42);
        m.cpu_write(0x7FFF, 0x99);
        assert_eq!(m.cpu_peek(0x7FFF), 0x99);
    }

    #[test]
    fn save_state_round_trip_preserves_promotion() {
        let mut m = CodemastersBf909x::new(cart(0));
        m.cpu_write(0x9000, 0x10); // promote + screen B
        m.cpu_write(0xC000, 5);
        let snap = m.save_state_capture().unwrap();
        let mut fresh = CodemastersBf909x::new(cart(0));
        fresh.save_state_apply(&snap).unwrap();
        assert!(fresh.bf9097_mode);
        assert_eq!(fresh.mirroring(), Mirroring::SingleScreenUpper);
        assert_eq!(fresh.cpu_peek(0x8000), 5);
    }
}
