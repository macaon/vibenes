// SPDX-License-Identifier: GPL-3.0-or-later
//! Irem G-101 - iNES mapper 32.
//!
//! 52-pin DIP ASIC used by *Image Fight*, *Major League*, *Kaiketsu
//! Yanchamaru 2*, and a handful of other Irem titles. Two 8 KiB
//! switchable PRG banks plus eight 1 KiB CHR banks; mirroring is
//! software-controlled (vertical / horizontal). PRG can be wired in
//! one of two modes that swap which slot the `{-2}` fixed bank lives
//! in.
//!
//! ## Register map (`addr & 0xF007`)
//!
//! | Range          | Effect                                    |
//! |----------------|-------------------------------------------|
//! | `$8000-$8007`  | PRG reg 0 (low 5 bits)                    |
//! | `$9000-$9007`  | bit 0 = mirroring (0=Vert, 1=Horz),       |
//! |                | bit 1 = PRG mode (swap `$8000` / `$C000`) |
//! | `$A000-$A007`  | PRG reg 1 (low 5 bits)                    |
//! | `$B000-$B007`  | CHR reg N where N = `addr & 7`            |
//!
//! ## PRG layout
//!
//! ```text
//!                $8000   $A000   $C000   $E000
//! Mode 0:        |reg0  | reg1  | {-2}  | {-1} |
//! Mode 1:        |{-2}  | reg1  | reg0  | {-1} |
//! ```
//!
//! `{-2}` and `{-1}` are the second-to-last and last 8 KiB banks of
//! PRG-ROM, respectively.
//!
//! ## Submapper 1 - Major League
//!
//! `Major League` ties CIRAM A10 to +5V on the cartridge, hardwiring
//! single-screen (upper-page) mirroring. The `$9000` register is also
//! disabled on this board, so the game can only request PRG mode 0.
//! Activated via NES 2.0 submapper 1; iNES-1.0 dumps need a game-DB
//! hint or per-cart override (we don't currently ship one - the
//! Mesen2-conventional submapper field on a NES 2.0 dump is enough).
//!
//! Reference: <https://www.nesdev.org/wiki/INES_Mapper_032>.

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_8K: usize = 8 * 1024;
const CHR_BANK_1K: usize = 1024;
const PRG_RAM_SIZE: usize = 8 * 1024;

pub struct IremG101 {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    prg_ram: Vec<u8>,
    chr_ram: bool,
    /// True for submapper 1 (Major League): one-screen mirroring is
    /// hardwired and `$9000` writes are ignored.
    major_league: bool,
    mirroring: Mirroring,
    /// PRG mode bit (`$9000` bit 1). 0 = `$8000` is `reg0`, `$C000`
    /// is `{-2}`. 1 = `$8000` is `{-2}`, `$C000` is `reg0`. Always 0
    /// on submapper 1.
    prg_mode: u8,
    prg_reg0: u8,
    prg_reg1: u8,
    chr_regs: [u8; 8],
    /// `prg_bank_count - 1` (in 8 KiB units). Power-of-two on every
    /// known cart, so we mask instead of modding the bank index.
    prg_bank_mask: usize,
    chr_bank_mask: usize,
    battery: bool,
    save_dirty: bool,
}

impl IremG101 {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count = (cart.prg_rom.len() / PRG_BANK_8K).max(1);
        debug_assert!(prg_bank_count.is_power_of_two());
        let prg_bank_mask = prg_bank_count - 1;

        let is_chr_ram = cart.chr_ram || cart.chr_rom.is_empty();
        let chr = if is_chr_ram {
            vec![0u8; 8 * CHR_BANK_1K]
        } else {
            cart.chr_rom
        };
        let chr_bank_count = (chr.len() / CHR_BANK_1K).max(1);
        debug_assert!(chr_bank_count.is_power_of_two());
        let chr_bank_mask = chr_bank_count - 1;

        let prg_ram_total =
            (cart.prg_ram_size + cart.prg_nvram_size).max(PRG_RAM_SIZE);

        let major_league = cart.submapper == 1;
        let mirroring = if major_league {
            // CIRAM A10 tied high → always upper page, single screen.
            Mirroring::SingleScreenUpper
        } else {
            cart.mirroring
        };

        Self {
            prg_rom: cart.prg_rom,
            chr,
            prg_ram: vec![0u8; prg_ram_total],
            chr_ram: is_chr_ram,
            major_league,
            mirroring,
            prg_mode: 0,
            prg_reg0: 0,
            prg_reg1: 0,
            chr_regs: [0; 8],
            prg_bank_mask,
            chr_bank_mask,
            battery: cart.battery_backed,
            save_dirty: false,
        }
    }

    fn prg_bank_base(&self, bank: usize) -> usize {
        (bank & self.prg_bank_mask) * PRG_BANK_8K
    }

    fn last_bank(&self) -> usize {
        self.prg_bank_mask
    }

    fn second_last_bank(&self) -> usize {
        self.prg_bank_mask.saturating_sub(1)
    }

    fn prg_slot_base(&self, slot: u8) -> usize {
        // slot: 0 = $8000, 1 = $A000, 2 = $C000, 3 = $E000
        let bank = match (self.prg_mode, slot) {
            (0, 0) => self.prg_reg0 as usize,
            (1, 0) => self.second_last_bank(),
            (_, 1) => self.prg_reg1 as usize,
            (0, 2) => self.second_last_bank(),
            (1, 2) => self.prg_reg0 as usize,
            (_, 3) => self.last_bank(),
            _ => unreachable!(),
        };
        self.prg_bank_base(bank)
    }

    fn chr_slot_base(&self, slot: usize) -> usize {
        (self.chr_regs[slot] as usize & self.chr_bank_mask) * CHR_BANK_1K
    }
}

impl Mapper for IremG101 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => {
                let i = (addr - 0x6000) as usize;
                *self.prg_ram.get(i).unwrap_or(&0)
            }
            0x8000..=0xFFFF => {
                let slot = ((addr - 0x8000) >> 13) as u8; // 0..=3
                let off = (addr & 0x1FFF) as usize;
                let base = self.prg_slot_base(slot);
                *self.prg_rom.get(base + off).unwrap_or(&0)
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
            0x8000..=0xFFFF => {
                // Registers are mirrored across the whole top half;
                // the chip decodes only `addr & 0xF007`.
                let reg = addr & 0xF007;
                match reg & 0xF000 {
                    0x8000 => self.prg_reg0 = data & 0x1F,
                    0x9000 => {
                        if !self.major_league {
                            self.mirroring = if data & 0x01 == 0 {
                                Mirroring::Vertical
                            } else {
                                Mirroring::Horizontal
                            };
                            self.prg_mode = (data >> 1) & 0x01;
                        }
                    }
                    0xA000 => self.prg_reg1 = data & 0x1F,
                    0xB000 => {
                        let i = (reg & 0x0007) as usize;
                        self.chr_regs[i] = data;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr < 0x2000 {
            let slot = (addr >> 10) as usize & 0x07;
            let off = (addr & 0x03FF) as usize;
            let base = self.chr_slot_base(slot);
            *self.chr.get(base + off).unwrap_or(&0)
        } else {
            0
        }
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if self.chr_ram && addr < 0x2000 {
            let slot = (addr >> 10) as usize & 0x07;
            let off = (addr & 0x03FF) as usize;
            let base = self.chr_slot_base(slot);
            if let Some(b) = self.chr.get_mut(base + off) {
                *b = data;
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
    use crate::nes::rom::{Cartridge, Mirroring, TvSystem};

    /// 256 KiB PRG (32 banks of 8 KiB), CHR-RAM. Each PRG bank is
    /// filled with a unique tag (`bank as u8`) so tests can identify
    /// the mapped bank by reading any byte in its window.
    fn cart(submapper: u8) -> Cartridge {
        let mut prg = vec![0u8; 32 * PRG_BANK_8K];
        for bank in 0..32 {
            let base = bank * PRG_BANK_8K;
            prg[base..base + PRG_BANK_8K].fill(bank as u8);
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: vec![],
            chr_ram: true,
            mapper_id: 32,
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
    fn power_on_layout_is_mode0_with_fixed_last_two_banks() {
        let m = IremG101::new(cart(0));
        // reg0=0, reg1=0, mode=0 → $8000=0, $A000=0, $C000=30, $E000=31.
        assert_eq!(m.cpu_peek(0x8000), 0);
        assert_eq!(m.cpu_peek(0xA000), 0);
        assert_eq!(m.cpu_peek(0xC000), 30);
        assert_eq!(m.cpu_peek(0xE000), 31);
        assert_eq!(m.cpu_peek(0xFFFF), 31);
    }

    #[test]
    fn prg_reg0_and_reg1_swap_their_windows() {
        let mut m = IremG101::new(cart(0));
        m.cpu_write(0x8000, 0x05);
        m.cpu_write(0xA000, 0x09);
        assert_eq!(m.cpu_peek(0x8000), 5);
        assert_eq!(m.cpu_peek(0xA000), 9);
        // Fixed slots untouched.
        assert_eq!(m.cpu_peek(0xC000), 30);
        assert_eq!(m.cpu_peek(0xE000), 31);
    }

    #[test]
    fn prg_mode_1_swaps_first_and_third_slot() {
        let mut m = IremG101::new(cart(0));
        m.cpu_write(0x8000, 0x07); // reg0 = 7
        m.cpu_write(0x9000, 0b10); // mode=1, mirroring=Vertical
        // Mode 1: $8000 = {-2} = 30, $C000 = reg0 = 7
        assert_eq!(m.cpu_peek(0x8000), 30);
        assert_eq!(m.cpu_peek(0xC000), 7);
        assert_eq!(m.cpu_peek(0xE000), 31);
    }

    #[test]
    fn mirroring_bit_toggles_v_and_h() {
        let mut m = IremG101::new(cart(0));
        m.cpu_write(0x9000, 0x00);
        assert_eq!(m.mirroring(), Mirroring::Vertical);
        m.cpu_write(0x9000, 0x01);
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
    }

    #[test]
    fn register_decoded_by_a14_a13_a12_only() {
        let mut m = IremG101::new(cart(0));
        // $8007 should also program reg0 (low 3 addr bits ignored).
        m.cpu_write(0x8007, 0x0A);
        assert_eq!(m.cpu_peek(0x8000), 10);
        // $9FFF mirrors $9000.
        m.cpu_write(0x9FFF, 0x01);
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
    }

    #[test]
    fn chr_regs_select_each_1k_slot_independently() {
        // Switch to CHR-ROM cart for a clearer mapping test.
        let mut chr_rom = vec![0u8; 32 * CHR_BANK_1K];
        for bank in 0..32 {
            let base = bank * CHR_BANK_1K;
            chr_rom[base..base + CHR_BANK_1K].fill(bank as u8);
        }
        let mut c = cart(0);
        c.chr_rom = chr_rom;
        c.chr_ram = false;
        let mut m = IremG101::new(c);

        for i in 0..8u8 {
            m.cpu_write(0xB000 | u16::from(i), 0x10 | i); // bank 0x10..0x17
        }
        for slot in 0..8 {
            let addr = (slot as u16) * 0x0400;
            assert_eq!(m.ppu_read(addr), 0x10 + slot as u8);
        }
    }

    #[test]
    fn major_league_submapper_locks_one_screen_and_ignores_9000() {
        let mut m = IremG101::new(cart(1));
        assert_eq!(m.mirroring(), Mirroring::SingleScreenUpper);
        // Try to flip to mode 1 + horizontal - should be ignored.
        m.cpu_write(0x9000, 0xFF);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenUpper);
        m.cpu_write(0x8000, 0x05);
        // PRG mode stayed at 0: reg0 still maps to $8000.
        assert_eq!(m.cpu_peek(0x8000), 5);
        assert_eq!(m.cpu_peek(0xC000), 30);
    }

    #[test]
    fn chr_ram_round_trips_via_selected_bank() {
        let mut m = IremG101::new(cart(0));
        m.cpu_write(0xB000, 3); // slot 0 → CHR bank 3
        m.ppu_write(0x0010, 0x99);
        assert_eq!(m.ppu_read(0x0010), 0x99);
        // Switching the slot changes what we see at the same PPU addr.
        m.cpu_write(0xB000, 4);
        assert_eq!(m.ppu_read(0x0010), 0x00);
        m.cpu_write(0xB000, 3);
        assert_eq!(m.ppu_read(0x0010), 0x99);
    }

    #[test]
    fn prg_ram_round_trip_at_6000_window() {
        let mut m = IremG101::new(cart(0));
        m.cpu_write(0x6000, 0x42);
        m.cpu_write(0x7FFF, 0x55);
        assert_eq!(m.cpu_peek(0x6000), 0x42);
        assert_eq!(m.cpu_peek(0x7FFF), 0x55);
    }
}
