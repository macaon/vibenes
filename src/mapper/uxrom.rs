// SPDX-License-Identifier: GPL-3.0-or-later
//! UxROM (mapper 2).
//!
//! PRG is split into two 16KB windows:
//! - `$8000-$BFFF`: switchable bank, selected by any write to `$8000-$FFFF`.
//!   The register is just a bank index - UNROM only decodes the low 3 bits
//!   (8 banks, 128KB), UOROM decodes a full byte (256 banks, 4MB).
//! - `$C000-$FFFF`: hardwired to the **last** 16KB bank.
//!
//! CHR is 8KB of CHR-RAM (UxROM boards don't ship CHR-ROM). Reads and
//! writes to PPU `$0000-$1FFF` go through the CHR-RAM. A small subset of
//! boards ship actual CHR-ROM; we honor the cart header's `chr_ram` flag.
//!
//! Mirroring is hardwired at the cart header level (no runtime change).
//!
//! Common UxROM games: Mega Man, Castlevania, Contra, DuckTales, 1943.

use crate::mapper::Mapper;
use crate::rom::{Cartridge, Mirroring};

const PRG_BANK_16K: usize = 16 * 1024;
const CHR_BANK_8K: usize = 8 * 1024;

pub struct Uxrom {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    prg_ram: Vec<u8>,
    mirroring: Mirroring,
    chr_ram: bool,
    /// Selected low-bank index (mapped at `$8000-$BFFF`).
    bank: u8,
    /// `prg_bank_count - 1`. Masks off high bits of a bank write so garbage
    /// in the top bits never indexes past the PRG image.
    bank_mask: u8,
    prg_bank_count: usize,
    battery: bool,
    save_dirty: bool,
}

impl Uxrom {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count = (cart.prg_rom.len() / PRG_BANK_16K).max(1);
        // UxROM is a register-of-infinite-width board - all game variants
        // use power-of-two bank counts, so `count - 1` is a clean mask.
        // If a cart ever had a non-power-of-two bank count we'd mod instead.
        debug_assert!(prg_bank_count.is_power_of_two());
        let bank_mask = (prg_bank_count as u8).wrapping_sub(1);

        // Ensure CHR has at least one 8KB bank. Carts with no CHR-ROM ship
        // chr_ram=true + empty chr_rom; we allocate the RAM here.
        let is_chr_ram = cart.chr_ram || cart.chr_rom.is_empty();
        let chr = if is_chr_ram {
            vec![0u8; CHR_BANK_8K]
        } else {
            cart.chr_rom
        };

        let prg_ram = vec![0u8; (cart.prg_ram_size + cart.prg_nvram_size).max(0x2000)];

        Self {
            prg_rom: cart.prg_rom,
            chr,
            prg_ram,
            mirroring: cart.mirroring,
            chr_ram: is_chr_ram,
            bank: 0,
            bank_mask,
            prg_bank_count,
            battery: cart.battery_backed,
            save_dirty: false,
        }
    }

    fn fixed_bank_base(&self) -> usize {
        (self.prg_bank_count - 1) * PRG_BANK_16K
    }

    fn switch_bank_base(&self) -> usize {
        ((self.bank & self.bank_mask) as usize) * PRG_BANK_16K
    }
}

impl Mapper for Uxrom {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
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
            0x8000..=0xFFFF => {
                // Bank select - any address in the register range works.
                // Low bits of `data` select the bank, higher bits ignored.
                self.bank = data & self.bank_mask;
            }
            _ => {}
        }
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rom::{Cartridge, Mirroring, TvSystem};

    /// Build a cart with 128KB PRG (8 banks of 16KB) and CHR-RAM.
    /// Fills each PRG bank with a unique tag byte so tests can tell which
    /// bank is mapped: bank N contents = `0x10 + N`.
    fn cart_128k_prg_chr_ram() -> Cartridge {
        let mut prg = vec![0u8; 8 * PRG_BANK_16K];
        for bank in 0..8 {
            let base = bank * PRG_BANK_16K;
            prg[base..base + PRG_BANK_16K].fill(0x10 + bank as u8);
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: vec![],
            chr_ram: true,
            mapper_id: 2,
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
    fn default_bank_is_zero_and_fixed_is_last() {
        let cart = cart_128k_prg_chr_ram();
        let m = Uxrom::new(cart);
        // $8000-$BFFF -> bank 0 (tag 0x10).
        assert_eq!(m.cpu_peek(0x8000), 0x10);
        assert_eq!(m.cpu_peek(0xBFFF), 0x10);
        // $C000-$FFFF -> bank 7 (tag 0x17).
        assert_eq!(m.cpu_peek(0xC000), 0x17);
        assert_eq!(m.cpu_peek(0xFFFF), 0x17);
    }

    #[test]
    fn bank_write_switches_the_lower_window() {
        let cart = cart_128k_prg_chr_ram();
        let mut m = Uxrom::new(cart);
        m.cpu_write(0x8000, 0x03); // select bank 3
        assert_eq!(m.cpu_peek(0x8000), 0x13);
        assert_eq!(m.cpu_peek(0xBFFF), 0x13);
        // Fixed window unchanged.
        assert_eq!(m.cpu_peek(0xC000), 0x17);

        // Write via a different address in the register range.
        m.cpu_write(0xABCD, 0x05);
        assert_eq!(m.cpu_peek(0x9000), 0x15);

        // High bits of the write value are masked off. With 8 banks the
        // mask is 0x07, so 0x8A → bank 2.
        m.cpu_write(0xFFFF, 0x8A);
        assert_eq!(m.cpu_peek(0x8000), 0x12);
    }

    #[test]
    fn chr_ram_read_and_write_roundtrip() {
        let cart = cart_128k_prg_chr_ram();
        let mut m = Uxrom::new(cart);
        m.ppu_write(0x0123, 0xAB);
        assert_eq!(m.ppu_read(0x0123), 0xAB);
        m.ppu_write(0x1FFF, 0x99);
        assert_eq!(m.ppu_read(0x1FFF), 0x99);
    }

    #[test]
    fn prg_ram_read_and_write_at_6000_window() {
        let cart = cart_128k_prg_chr_ram();
        let mut m = Uxrom::new(cart);
        m.cpu_write(0x6000, 0x42);
        assert_eq!(m.cpu_peek(0x6000), 0x42);
        m.cpu_write(0x7FFF, 0x55);
        assert_eq!(m.cpu_peek(0x7FFF), 0x55);
    }
}
