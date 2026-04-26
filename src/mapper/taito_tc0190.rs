// SPDX-License-Identifier: GPL-3.0-or-later
//! Taito TC0190 / TC0350 - iNES mapper 33.
//!
//! Two 8 KiB switchable PRG slots plus the standard `{-2}` / `{-1}`
//! fixed pair, six CHR slots (two 2 KiB followed by four 1 KiB), and
//! software-controlled vertical/horizontal mirroring. No IRQs - the
//! TC0350 variant on *Don Doko Don* never used the interrupt that
//! the closely-related mapper 48 chip exposes.
//!
//! ## Register map (`addr & 0xA003`)
//!
//! | Address | Bits           | Effect                                  |
//! |---------|----------------|-----------------------------------------|
//! | `$8000` | `[.MPP PPPP]`  | M = mirroring (0=V, 1=H), P = PRG reg 0 |
//! | `$8001` | `[..PP PPPP]`  | PRG reg 1 (8 KiB @ `$A000`)             |
//! | `$8002` | `[CCCC CCCC]`  | CHR reg 0 (2 KiB @ `$0000`)             |
//! | `$8003` | `[CCCC CCCC]`  | CHR reg 1 (2 KiB @ `$0800`)             |
//! | `$A000` | `[CCCC CCCC]`  | CHR reg 2 (1 KiB @ `$1000`)             |
//! | `$A001` | `[CCCC CCCC]`  | CHR reg 3 (1 KiB @ `$1400`)             |
//! | `$A002` | `[CCCC CCCC]`  | CHR reg 4 (1 KiB @ `$1800`)             |
//! | `$A003` | `[CCCC CCCC]`  | CHR reg 5 (1 KiB @ `$1C00`)             |
//!
//! ## CHR addressing quirk
//!
//! Unlike MMC3, the two 2 KiB CHR registers do **not** drop the LSB -
//! the byte written is a multiple of 2 KiB into the CHR image, so a
//! full 512 KiB CHR can be addressed by those slots. The four 1 KiB
//! CHR slots are limited to the first 256 KiB. We honor both ranges
//! by masking against the appropriate `chr_bank_count - 1`.
//!
//! Reference: <https://www.nesdev.org/wiki/INES_Mapper_033>.

use crate::mapper::Mapper;
use crate::rom::{Cartridge, Mirroring};

const PRG_BANK_8K: usize = 8 * 1024;
const CHR_BANK_1K: usize = 1024;
const CHR_BANK_2K: usize = 2 * 1024;
const PRG_RAM_SIZE: usize = 8 * 1024;

pub struct TaitoTc0190 {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    prg_ram: Vec<u8>,
    chr_ram: bool,
    mirroring: Mirroring,
    /// PRG-ROM size in 8 KiB units, minus one. Power-of-two on every
    /// known TC0190 dump.
    prg_bank_mask: usize,
    /// CHR-ROM size in 1 KiB units, minus one. Used for the four
    /// 1 KiB slots; we right-shift by 1 to derive the 2 KiB mask.
    chr_bank_mask_1k: usize,
    chr_bank_mask_2k: usize,
    prg_reg0: u8,
    prg_reg1: u8,
    chr_2k: [u8; 2],
    chr_1k: [u8; 4],
    battery: bool,
    save_dirty: bool,
}

impl TaitoTc0190 {
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
        let chr_bank_count_1k = (chr.len() / CHR_BANK_1K).max(1);
        let chr_bank_count_2k = (chr.len() / CHR_BANK_2K).max(1);
        debug_assert!(chr_bank_count_1k.is_power_of_two());
        let chr_bank_mask_1k = chr_bank_count_1k - 1;
        let chr_bank_mask_2k = chr_bank_count_2k - 1;

        let prg_ram_total =
            (cart.prg_ram_size + cart.prg_nvram_size).max(PRG_RAM_SIZE);

        // Mirroring is software-controlled; cart-header value is just
        // a power-on hint until the game writes `$8000`.
        let mirroring = match cart.mirroring {
            Mirroring::Horizontal => Mirroring::Horizontal,
            _ => Mirroring::Vertical,
        };

        Self {
            prg_rom: cart.prg_rom,
            chr,
            prg_ram: vec![0u8; prg_ram_total],
            chr_ram: is_chr_ram,
            mirroring,
            prg_bank_mask,
            chr_bank_mask_1k,
            chr_bank_mask_2k,
            prg_reg0: 0,
            prg_reg1: 0,
            chr_2k: [0; 2],
            chr_1k: [0; 4],
            battery: cart.battery_backed,
            save_dirty: false,
        }
    }

    fn prg_slot_base(&self, slot: u8) -> usize {
        let bank = match slot {
            0 => self.prg_reg0 as usize,
            1 => self.prg_reg1 as usize,
            2 => self.prg_bank_mask.saturating_sub(1), // {-2}
            _ => self.prg_bank_mask,                   // {-1}
        };
        (bank & self.prg_bank_mask) * PRG_BANK_8K
    }
}

impl Mapper for TaitoTc0190 {
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
                // Only A0/A1/A13 are decoded; mirror across the top half.
                let reg = addr & 0xA003;
                match reg {
                    0x8000 => {
                        self.mirroring = if data & 0x40 == 0 {
                            Mirroring::Vertical
                        } else {
                            Mirroring::Horizontal
                        };
                        self.prg_reg0 = data & 0x3F;
                    }
                    0x8001 => self.prg_reg1 = data & 0x3F,
                    0x8002 => self.chr_2k[0] = data,
                    0x8003 => self.chr_2k[1] = data,
                    0xA000 => self.chr_1k[0] = data,
                    0xA001 => self.chr_1k[1] = data,
                    0xA002 => self.chr_1k[2] = data,
                    0xA003 => self.chr_1k[3] = data,
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr >= 0x2000 {
            return 0;
        }
        let off = self.chr_offset(addr);
        *self.chr.get(off).unwrap_or(&0)
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if !self.chr_ram || addr >= 0x2000 {
            return;
        }
        let off = self.chr_offset(addr);
        if let Some(b) = self.chr.get_mut(off) {
            *b = data;
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

impl TaitoTc0190 {
    fn chr_offset(&self, addr: u16) -> usize {
        match addr {
            0x0000..=0x07FF => {
                let bank = self.chr_2k[0] as usize & self.chr_bank_mask_2k;
                bank * CHR_BANK_2K + (addr & 0x07FF) as usize
            }
            0x0800..=0x0FFF => {
                let bank = self.chr_2k[1] as usize & self.chr_bank_mask_2k;
                bank * CHR_BANK_2K + (addr & 0x07FF) as usize
            }
            0x1000..=0x1FFF => {
                let slot = ((addr - 0x1000) >> 10) as usize; // 0..=3
                let bank = self.chr_1k[slot] as usize & self.chr_bank_mask_1k;
                bank * CHR_BANK_1K + (addr & 0x03FF) as usize
            }
            _ => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rom::{Cartridge, Mirroring, TvSystem};

    /// 256 KiB PRG (32 banks of 8 KiB). Each bank tagged with its
    /// own index so `cpu_peek` reveals which bank is mapped.
    fn cart_prg256k_chr256k() -> Cartridge {
        let mut prg = vec![0u8; 32 * PRG_BANK_8K];
        for bank in 0..32 {
            let base = bank * PRG_BANK_8K;
            prg[base..base + PRG_BANK_8K].fill(bank as u8);
        }
        let mut chr = vec![0u8; 256 * CHR_BANK_1K];
        for bank in 0..256 {
            let base = bank * CHR_BANK_1K;
            chr[base..base + CHR_BANK_1K].fill(bank as u8);
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: chr,
            chr_ram: false,
            mapper_id: 33,
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
    fn power_on_layout_fixes_last_two_8k_banks() {
        let m = TaitoTc0190::new(cart_prg256k_chr256k());
        assert_eq!(m.cpu_peek(0x8000), 0);
        assert_eq!(m.cpu_peek(0xA000), 0);
        assert_eq!(m.cpu_peek(0xC000), 30);
        assert_eq!(m.cpu_peek(0xE000), 31);
        assert_eq!(m.cpu_peek(0xFFFF), 31);
    }

    #[test]
    fn prg_regs_route_to_8000_and_a000_only() {
        let mut m = TaitoTc0190::new(cart_prg256k_chr256k());
        // $8000: bit6 = mirroring, low 6 = PRG reg 0
        m.cpu_write(0x8000, 0x05);
        m.cpu_write(0x8001, 0x09);
        assert_eq!(m.cpu_peek(0x8000), 5);
        assert_eq!(m.cpu_peek(0xA000), 9);
        assert_eq!(m.cpu_peek(0xC000), 30);
        assert_eq!(m.cpu_peek(0xE000), 31);
    }

    #[test]
    fn mirroring_bit_in_8000_toggles_v_and_h() {
        let mut m = TaitoTc0190::new(cart_prg256k_chr256k());
        m.cpu_write(0x8000, 0x00);
        assert_eq!(m.mirroring(), Mirroring::Vertical);
        m.cpu_write(0x8000, 0x40);
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
    }

    #[test]
    fn chr_2k_slots_dont_drop_lsb() {
        let mut m = TaitoTc0190::new(cart_prg256k_chr256k());
        // Writing 1 to a 2 KiB CHR reg points to CHR offset 2 KiB.
        // The 2 KiB window covers 1 KiB tag banks 2 and 3, so the
        // first half reads tag 2 and the second half reads tag 3 -
        // proving the LSB was NOT dropped (an MMC3-style mapper
        // would have aligned the bank to 2 KiB and shown 2/2).
        m.cpu_write(0x8002, 1);
        assert_eq!(m.ppu_read(0x0000), 2);
        assert_eq!(m.ppu_read(0x03FF), 2);
        assert_eq!(m.ppu_read(0x0400), 3);
        assert_eq!(m.ppu_read(0x07FF), 3);
        // Slot at $0800 follows a separate register.
        m.cpu_write(0x8003, 4);
        assert_eq!(m.ppu_read(0x0800), 8);
        assert_eq!(m.ppu_read(0x0FFF), 9);
    }

    #[test]
    fn chr_1k_slots_address_individually() {
        let mut m = TaitoTc0190::new(cart_prg256k_chr256k());
        m.cpu_write(0xA000, 0x10);
        m.cpu_write(0xA001, 0x11);
        m.cpu_write(0xA002, 0x12);
        m.cpu_write(0xA003, 0x13);
        assert_eq!(m.ppu_read(0x1000), 0x10);
        assert_eq!(m.ppu_read(0x1400), 0x11);
        assert_eq!(m.ppu_read(0x1800), 0x12);
        assert_eq!(m.ppu_read(0x1C00), 0x13);
    }

    #[test]
    fn registers_mirror_across_top_half() {
        // The chip decodes only A0, A1, A13, A15 - `addr & 0xA003`.
        // So any address in `$8000-$9FFF` with A1=1, A0=0 (e.g.
        // `$9FFE`) hits the same register as `$8002`, and any address
        // in `$A000-$BFFF` hits the `$A___` register group.
        let mut m = TaitoTc0190::new(cart_prg256k_chr256k());
        m.cpu_write(0x9FFE, 1); // ($9FFE & 0xA003) = 0x8002 → CHR reg 0
        assert_eq!(m.ppu_read(0x0000), 2);
        m.cpu_write(0xBFFE, 5); // ($BFFE & 0xA003) = 0xA002 → CHR reg 4
        assert_eq!(m.ppu_read(0x1800), 5);
    }

    #[test]
    fn prg_ram_round_trip() {
        let mut m = TaitoTc0190::new(cart_prg256k_chr256k());
        m.cpu_write(0x6000, 0x42);
        m.cpu_write(0x7FFF, 0x55);
        assert_eq!(m.cpu_peek(0x6000), 0x42);
        assert_eq!(m.cpu_peek(0x7FFF), 0x55);
    }
}
