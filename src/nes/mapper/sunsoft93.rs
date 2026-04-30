// SPDX-License-Identifier: GPL-3.0-or-later
//! Sunsoft-3R / Sunsoft-2 IC variant (iNES mapper 93).
//!
//! Discrete-logic mapper Sunsoft used on a small handful of
//! Famicom carts. The PCB is wired similarly to the Sunsoft-2
//! board (mapper 89) but the data routing is simpler: one
//! register at `$8000-$FFFF` carries a 3-bit PRG bank and a
//! single CHR-OE bit. No mirroring control, no audio. Bus
//! conflicts are present (the cart's PRG output and the CPU
//! value are wired-AND together onto the chip's input pins).
//!
//! Games (licensed Sunsoft Famicom):
//! - *Fantasy Zone* (Sunsoft, 1987)
//! - *Shanghai* (Sunsoft, 1988)
//!
//! ## Register surface (single latch at `$8000-$FFFF`)
//!
//! ```text
//! PPPx xxxE
//!   |    |
//!   |    +-- E: 1 = CHR enabled (bank 0); 0 = CHR-OE disabled
//!   +------- PPP: 16 KiB PRG bank at $8000-$BFFF (bits 6-4)
//! ```
//!
//! `$C000-$FFFF` is hardwired to the last 16 KiB bank. When the
//! CHR-OE bit is clear, PPU reads of `$0000-$1FFF` return open
//! bus (`$FF` per the floating-data-line convention - same as
//! mapper 185's disabled-CHR path).
//!
//! Three bits of PRG = 8 banks of 16 KiB = 128 KiB max, which is
//! exactly what the two known retail games ship.
//!
//! References:
//! - <https://www.nesdev.org/wiki/INES_Mapper_093>
//! - `~/Git/Mesen2/Core/NES/Mappers/Sunsoft/Sunsoft93.h`
//! - `~/Git/punes/src/core/mappers/mapper_093.c`

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_16K: usize = 16 * 1024;
const CHR_BANK_8K: usize = 8 * 1024;

pub struct Sunsoft93 {
    prg_rom: Vec<u8>,
    chr_rom: Vec<u8>,

    /// Latched value (post-bus-conflict AND).
    reg: u8,

    mirroring: Mirroring,
    prg_bank_count_16k: usize,
}

impl Sunsoft93 {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count_16k = (cart.prg_rom.len() / PRG_BANK_16K).max(1);
        let chr_rom = if cart.chr_rom.is_empty() {
            vec![0u8; CHR_BANK_8K]
        } else {
            cart.chr_rom
        };
        Self {
            prg_rom: cart.prg_rom,
            chr_rom,
            reg: 0,
            mirroring: cart.mirroring,
            prg_bank_count_16k,
        }
    }

    fn switch_bank_base(&self) -> usize {
        let bank = ((self.reg >> 4) & 0x07) as usize;
        (bank % self.prg_bank_count_16k) * PRG_BANK_16K
    }

    fn fixed_bank_base(&self) -> usize {
        (self.prg_bank_count_16k - 1) * PRG_BANK_16K
    }

    fn chr_enabled(&self) -> bool {
        self.reg & 0x01 != 0
    }

    fn prg_byte(&self, addr: u16) -> u8 {
        match addr {
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
}

impl Mapper for Sunsoft93 {
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
            // Bus conflict: visible PRG byte ANDs the CPU value
            // before reaching the latch.
            self.reg = data & self.prg_byte(addr);
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr < 0x2000 {
            if self.chr_enabled() {
                *self.chr_rom.get(addr as usize).unwrap_or(&0xFF)
            } else {
                // CHR-OE high - PPU bus floats. Open-bus
                // convention on a disconnected data line.
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
        use crate::save_state::mapper::Sunsoft93Snap;
        Some(crate::save_state::MapperState::Sunsoft93(Sunsoft93Snap {
            reg: self.reg,
        }))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::Sunsoft93(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        self.reg = snap.reg;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, Mirroring, TvSystem};

    /// 128 KiB PRG (8 banks of 16 KiB), 8 KiB CHR-ROM. Each PRG
    /// bank has its index in the first byte; CHR is tagged
    /// `$AA` so we can spot the locked-state `$FF` placeholder.
    /// PRG fill is `$FF` so bus-conflict ANDs are no-ops away
    /// from offset 0.
    fn cart() -> Cartridge {
        let mut prg = vec![0xFFu8; 8 * PRG_BANK_16K];
        for bank in 0..8 {
            prg[bank * PRG_BANK_16K] = bank as u8;
        }
        let chr = vec![0xAAu8; CHR_BANK_8K];
        Cartridge {
            prg_rom: prg,
            chr_rom: chr,
            chr_ram: false,
            mapper_id: 93,
            submapper: 0,
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
    fn boot_state_chr_disabled_first_at_8000_last_at_c000() {
        let mut m = Sunsoft93::new(cart());
        // reg = 0 -> bit 0 = 0 -> CHR disabled -> open bus.
        assert_eq!(m.ppu_read(0x0000), 0xFF);
        // PRG bank 0 at low window, last (7) at high window.
        assert_eq!(m.cpu_peek(0x8000), 0);
        assert_eq!(m.cpu_peek(0xC000), 7);
    }

    #[test]
    fn bit0_of_register_gates_chr_access() {
        let mut m = Sunsoft93::new(cart());
        // Write at $8001 (PRG byte $FF) so bus-conflict AND is a
        // no-op.
        m.cpu_write(0x8001, 0x01);
        assert_eq!(m.ppu_read(0x0000), 0xAA);
        m.cpu_write(0x8001, 0x00);
        assert_eq!(m.ppu_read(0x0000), 0xFF);
    }

    #[test]
    fn bank_select_uses_bits_6_5_4() {
        let mut m = Sunsoft93::new(cart());
        // 0b0011_0001 -> PRG bits 6-4 = 011 = 3. CHR enabled.
        m.cpu_write(0x8001, 0b0011_0001);
        assert_eq!(m.cpu_peek(0x8000), 3);
        // High bit (bit 7) ignored.
        m.cpu_write(0x8001, 0b1110_0001); // bits 6-4 = 110 = 6
        assert_eq!(m.cpu_peek(0x8000), 6);
        // Bits 1-3 ignored.
        m.cpu_write(0x8001, 0b0010_1111); // bits 6-4 = 010 = 2
        assert_eq!(m.cpu_peek(0x8000), 2);
        // Fixed window stays on bank 7.
        assert_eq!(m.cpu_peek(0xC000), 7);
    }

    #[test]
    fn bus_conflict_masks_value() {
        let mut m = Sunsoft93::new(cart());
        // First load bank 5 so the PRG byte at $8000 is `$05`.
        m.cpu_write(0x8001, 0b0101_0001);
        assert_eq!(m.cpu_peek(0x8000), 5);
        // Now write at $8000 (PRG byte 0x05 = 0b0000_0101).
        // Value 0b0011_0001 ANDed with 0x05 = 0b0000_0001.
        // -> bits 6-4 = 0, bit 0 = 1 -> bank 0, CHR on.
        m.cpu_write(0x8000, 0b0011_0001);
        assert_eq!(m.cpu_peek(0x8000), 0);
        assert_eq!(m.ppu_read(0x0000), 0xAA);
    }

    #[test]
    fn ppu_writes_have_no_effect() {
        let mut m = Sunsoft93::new(cart());
        m.cpu_write(0x8001, 0x01); // enable CHR
        m.ppu_write(0x0000, 0x55);
        assert_eq!(m.ppu_read(0x0000), 0xAA);
    }

    #[test]
    fn save_state_round_trip() {
        let mut m = Sunsoft93::new(cart());
        m.cpu_write(0x8001, 0b0100_0001); // bank 4, CHR on
        let snap = m.save_state_capture().unwrap();
        let mut fresh = Sunsoft93::new(cart());
        fresh.save_state_apply(&snap).unwrap();
        assert_eq!(fresh.cpu_peek(0x8000), 4);
        assert_eq!(fresh.ppu_read(0x0000), 0xAA);
    }
}
