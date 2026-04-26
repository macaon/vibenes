// SPDX-License-Identifier: GPL-3.0-or-later
//! Konami VRC3 - iNES mapper 73.
//!
//! Used (apparently) only by the Famicom port of *Salamander*. Tiny
//! ASIC: one switchable 16 KiB PRG bank at `$8000-$BFFF`, the last
//! 16 KiB fixed at `$C000-$FFFF`, 8 KiB of CHR-RAM, hardwired
//! mirroring, and a 16-bit CPU-cycle IRQ counter assembled nibble-by-
//! nibble from four register writes. No expansion audio.
//!
//! ## Register map (`addr & 0xF000`)
//!
//! | Address  | Effect                                            |
//! |----------|---------------------------------------------------|
//! | `$8000`  | IRQ latch bits 0-3                                |
//! | `$9000`  | IRQ latch bits 4-7                                |
//! | `$A000`  | IRQ latch bits 8-11                               |
//! | `$B000`  | IRQ latch bits 12-15                              |
//! | `$C000`  | IRQ control: bits = `.... .MEA` (see below)       |
//! | `$D000`  | IRQ acknowledge + copy `A` → `E`                  |
//! | `$F000`  | PRG bank @ `$8000` (low 3 bits select 16 KiB bank)|
//!
//! `$C000` bits - `M` (bit 2) selects 8-bit (1) or 16-bit (0) counter
//! mode. `E` (bit 1) is the IRQ enable. `A` (bit 0) is the
//! "enable-on-acknowledge" latch - `$D000` copies it into `E` so a
//! game can re-enable IRQs in the same instruction that acknowledges
//! the previous one. Any write to `$C000` acks the pending IRQ; a
//! `$C000` write with `E` set reloads the full 16-bit counter from
//! the latch (regardless of mode).
//!
//! ## IRQ counter
//!
//! 16-bit counter clocked every CPU cycle when enabled. On overflow
//! from `$FFFF` the counter is reloaded from the 16-bit latch and an
//! `/IRQ` is asserted. In 8-bit mode (`M` bit set) only the low 8
//! bits increment and only the low 8 bits reload on overflow - the
//! high byte is preserved across overflows (until a `$C000` reload
//! rewrites it).
//!
//! Reference: <https://www.nesdev.org/wiki/VRC3>. Cross-checked
//! against `~/Git/Mesen2/Core/NES/Mappers/Konami/VRC3.h`,
//! `~/Git/punes/src/core/mappers/mapper_073.c`, and
//! `~/Git/nestopia/source/core/board/NstBoardKonamiVrc3.cpp`. Mesen2
//! has a stale typo in its 8-bit-mode IRQ branch (`if(_smallCounter ==
//! 0)` checks the bool mode flag rather than the post-increment value)
//! that prevents 8-bit-mode IRQs from firing - never tickled because
//! Salamander uses 16-bit mode. We implement both paths per the wiki.

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_16K: usize = 16 * 1024;
const CHR_BANK_8K: usize = 8 * 1024;
const PRG_RAM_SIZE: usize = 8 * 1024;

pub struct Vrc3 {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    prg_ram: Vec<u8>,
    mirroring: Mirroring,
    /// Selected 16 KiB bank for `$8000-$BFFF`. Low 3 bits of the
    /// `$F000` write - VRC3 carts cap out at 128 KiB (8 banks).
    prg_bank: u8,
    /// `prg_bank_count - 1` in 16 KiB units. Always a power of two
    /// for known carts so we mask instead of mod.
    prg_bank_mask: usize,
    irq_latch: u16,
    irq_counter: u16,
    /// IRQ enable (`E` bit of `$C000`).
    irq_enabled: bool,
    /// IRQ enable to apply on the next `$D000` write (`A` bit of
    /// `$C000`). `$D000` copies this into `irq_enabled`.
    irq_enable_on_ack: bool,
    /// 8-bit-counter mode (`M` bit of `$C000`). When set, only the
    /// low byte of the counter ticks and reloads on overflow.
    small_counter: bool,
    irq_line: bool,
    battery: bool,
    save_dirty: bool,
}

impl Vrc3 {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count = (cart.prg_rom.len() / PRG_BANK_16K).max(1);
        debug_assert!(prg_bank_count.is_power_of_two());
        let prg_bank_mask = prg_bank_count - 1;

        // VRC3 ships CHR-RAM exclusively; the cart header may still
        // claim CHR-ROM if mis-tagged, so we honor the header but
        // allocate 8 KiB of writable storage when in RAM mode.
        let chr = if cart.chr_ram || cart.chr_rom.is_empty() {
            vec![0u8; CHR_BANK_8K]
        } else {
            cart.chr_rom
        };

        let prg_ram_total =
            (cart.prg_ram_size + cart.prg_nvram_size).max(PRG_RAM_SIZE);

        Self {
            prg_rom: cart.prg_rom,
            chr,
            prg_ram: vec![0u8; prg_ram_total],
            mirroring: cart.mirroring,
            prg_bank: 0,
            prg_bank_mask,
            irq_latch: 0,
            irq_counter: 0,
            irq_enabled: false,
            irq_enable_on_ack: false,
            small_counter: false,
            irq_line: false,
            battery: cart.battery_backed,
            save_dirty: false,
        }
    }

    fn last_bank_base(&self) -> usize {
        self.prg_bank_mask * PRG_BANK_16K
    }

    fn switch_bank_base(&self) -> usize {
        ((self.prg_bank as usize) & self.prg_bank_mask) * PRG_BANK_16K
    }
}

impl Mapper for Vrc3 {
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
                let i = self.last_bank_base() + (addr - 0xC000) as usize;
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
                    if *slot != data {
                        *slot = data;
                        if self.battery {
                            self.save_dirty = true;
                        }
                    }
                }
            }
            0x8000..=0xFFFF => {
                let nibble = u16::from(data & 0x0F);
                match addr & 0xF000 {
                    0x8000 => self.irq_latch = (self.irq_latch & 0xFFF0) | nibble,
                    0x9000 => {
                        self.irq_latch = (self.irq_latch & 0xFF0F) | (nibble << 4)
                    }
                    0xA000 => {
                        self.irq_latch = (self.irq_latch & 0xF0FF) | (nibble << 8)
                    }
                    0xB000 => {
                        self.irq_latch = (self.irq_latch & 0x0FFF) | (nibble << 12)
                    }
                    0xC000 => {
                        // Ack pending IRQ on any $C000 write; reload
                        // the full counter when the new E bit is set.
                        self.irq_line = false;
                        self.small_counter = (data & 0x04) != 0;
                        self.irq_enabled = (data & 0x02) != 0;
                        self.irq_enable_on_ack = (data & 0x01) != 0;
                        if self.irq_enabled {
                            self.irq_counter = self.irq_latch;
                        }
                    }
                    0xD000 => {
                        // Ack + roll the A bit into E so games can
                        // re-arm the IRQ in the same routine that
                        // acknowledges it.
                        self.irq_line = false;
                        self.irq_enabled = self.irq_enable_on_ack;
                    }
                    0xF000 => self.prg_bank = data & 0x07,
                    _ => {}
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
        if addr < 0x2000 {
            if let Some(b) = self.chr.get_mut(addr as usize) {
                *b = data;
            }
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }

    fn on_cpu_cycle(&mut self) {
        if !self.irq_enabled {
            return;
        }
        if self.small_counter {
            // Only the low byte ticks and reloads. The wiki is
            // explicit that the high byte is "ignored (and never
            // incremented)" in 8-bit mode.
            let lo = (self.irq_counter as u8).wrapping_add(1);
            if lo == 0 {
                let reload_lo = self.irq_latch as u8;
                self.irq_counter = (self.irq_counter & 0xFF00) | u16::from(reload_lo);
                self.irq_line = true;
            } else {
                self.irq_counter = (self.irq_counter & 0xFF00) | u16::from(lo);
            }
        } else {
            let next = self.irq_counter.wrapping_add(1);
            if next == 0 {
                self.irq_counter = self.irq_latch;
                self.irq_line = true;
            } else {
                self.irq_counter = next;
            }
        }
    }

    fn irq_line(&self) -> bool {
        self.irq_line
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

    /// 128 KiB PRG (8 banks of 16 KiB), 8 KiB CHR-RAM. Each PRG bank
    /// tagged with its bank index so `cpu_peek` reveals the mapped
    /// bank.
    fn cart() -> Cartridge {
        let mut prg = vec![0u8; 8 * PRG_BANK_16K];
        for bank in 0..8 {
            let base = bank * PRG_BANK_16K;
            prg[base..base + PRG_BANK_16K].fill(bank as u8);
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: vec![],
            chr_ram: true,
            mapper_id: 73,
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
    fn power_on_layout_fixes_last_16k_bank() {
        let m = Vrc3::new(cart());
        assert_eq!(m.cpu_peek(0x8000), 0); // bank 0 by default
        assert_eq!(m.cpu_peek(0xBFFF), 0);
        assert_eq!(m.cpu_peek(0xC000), 7); // last bank
        assert_eq!(m.cpu_peek(0xFFFF), 7);
    }

    #[test]
    fn f000_write_selects_prg_bank() {
        let mut m = Vrc3::new(cart());
        m.cpu_write(0xF000, 0x05);
        assert_eq!(m.cpu_peek(0x8000), 5);
        assert_eq!(m.cpu_peek(0xBFFF), 5);
        // High bits ignored - only low 3 bits matter on a 128 KiB cart.
        m.cpu_write(0xFFFF, 0xFA); // 0xFA & 0x07 = 0x02
        assert_eq!(m.cpu_peek(0x8000), 2);
        // Last bank still pinned.
        assert_eq!(m.cpu_peek(0xC000), 7);
    }

    #[test]
    fn prg_ram_round_trip_at_6000_window() {
        let mut m = Vrc3::new(cart());
        m.cpu_write(0x6000, 0x42);
        m.cpu_write(0x7FFF, 0x55);
        assert_eq!(m.cpu_peek(0x6000), 0x42);
        assert_eq!(m.cpu_peek(0x7FFF), 0x55);
    }

    #[test]
    fn chr_ram_round_trip() {
        let mut m = Vrc3::new(cart());
        m.ppu_write(0x0010, 0xAB);
        m.ppu_write(0x1FFF, 0x99);
        assert_eq!(m.ppu_read(0x0010), 0xAB);
        assert_eq!(m.ppu_read(0x1FFF), 0x99);
    }

    #[test]
    fn latch_assembled_from_four_nibble_writes() {
        let mut m = Vrc3::new(cart());
        m.cpu_write(0x8000, 0x0A); // bits 0-3
        m.cpu_write(0x9000, 0x0B); // bits 4-7
        m.cpu_write(0xA000, 0x0C); // bits 8-11
        m.cpu_write(0xB000, 0x0D); // bits 12-15
        // Then $C000 with E set → counter = latch = 0xDCBA.
        m.cpu_write(0xC000, 0x02);
        assert_eq!(m.irq_latch, 0xDCBA);
        assert_eq!(m.irq_counter, 0xDCBA);
        // Upper bits of each write are ignored.
        m.cpu_write(0x8000, 0xF1);
        assert_eq!(m.irq_latch, 0xDCB1);
    }

    #[test]
    fn sixteen_bit_counter_fires_on_overflow_and_reloads_from_latch() {
        let mut m = Vrc3::new(cart());
        // Latch = 0xFFF0 → counter starts there, overflows after 16
        // CPU cycles, IRQ asserts, counter reloads to 0xFFF0.
        m.cpu_write(0x8000, 0x00);
        m.cpu_write(0x9000, 0x0F);
        m.cpu_write(0xA000, 0x0F);
        m.cpu_write(0xB000, 0x0F);
        m.cpu_write(0xC000, 0x02); // E=1, M=0 → 16-bit, reload now
        assert_eq!(m.irq_counter, 0xFFF0);
        for _ in 0..15 {
            m.on_cpu_cycle();
            assert!(!m.irq_line(), "early IRQ at counter {:04X}", m.irq_counter);
        }
        m.on_cpu_cycle(); // 16th cycle: 0xFFFF → 0x0000 → wrap → IRQ
        assert!(m.irq_line());
        assert_eq!(m.irq_counter, 0xFFF0); // reloaded
    }

    #[test]
    fn eight_bit_mode_only_low_byte_ticks_and_reloads() {
        let mut m = Vrc3::new(cart());
        // High latch = 0xAB, low latch = 0xF0. Counter at 0xABF0 in
        // 8-bit mode should overflow after 16 cycles to 0xABF0 again
        // (high byte preserved + low byte reloaded from 0xF0).
        m.cpu_write(0x8000, 0x00);
        m.cpu_write(0x9000, 0x0F);
        m.cpu_write(0xA000, 0x0B);
        m.cpu_write(0xB000, 0x0A);
        m.cpu_write(0xC000, 0x06); // E=1, M=1 → 8-bit, reload all
        assert_eq!(m.irq_counter, 0xABF0);
        for _ in 0..15 {
            m.on_cpu_cycle();
            assert!(!m.irq_line());
        }
        m.on_cpu_cycle();
        assert!(m.irq_line());
        // High byte preserved, low byte reloaded from latch low.
        assert_eq!(m.irq_counter, 0xABF0);
    }

    #[test]
    fn c000_write_with_e_clear_just_acks() {
        let mut m = Vrc3::new(cart());
        m.cpu_write(0x8000, 0x0E);
        m.cpu_write(0xC000, 0x02); // enable + reload (counter = 0x000E)
        for _ in 0..(0x10000 - 0x000E) {
            m.on_cpu_cycle();
        }
        assert!(m.irq_line());
        // Ack via $C000 with E clear - IRQ clears, counter unchanged
        // by reload (E was 0, no force-reload).
        let pre = m.irq_counter;
        m.cpu_write(0xC000, 0x00);
        assert!(!m.irq_line());
        assert_eq!(m.irq_counter, pre);
        assert!(!m.irq_enabled);
    }

    #[test]
    fn d000_acks_and_promotes_a_bit_to_e() {
        let mut m = Vrc3::new(cart());
        // Set A=1 (enable-on-ack), E=1.
        m.cpu_write(0xC000, 0x03);
        assert!(m.irq_enabled);
        // Run until IRQ fires.
        for _ in 0..0x10000 {
            m.on_cpu_cycle();
            if m.irq_line() {
                break;
            }
        }
        assert!(m.irq_line());
        // $D000: ack + copy A → E. Since A was 1, E stays 1.
        m.cpu_write(0xD000, 0x00);
        assert!(!m.irq_line());
        assert!(m.irq_enabled);

        // Now arm with A=0, E=1; $D000 should disable.
        m.cpu_write(0xC000, 0x02); // A=0, E=1, M=0
        assert!(m.irq_enabled);
        m.cpu_write(0xD000, 0x00);
        assert!(!m.irq_enabled);
    }

    #[test]
    fn disabled_counter_does_not_tick() {
        let mut m = Vrc3::new(cart());
        m.cpu_write(0x8000, 0x05);
        m.cpu_write(0xC000, 0x00); // E=0 - counter held
        let pre = m.irq_counter;
        for _ in 0..1000 {
            m.on_cpu_cycle();
        }
        assert_eq!(m.irq_counter, pre);
        assert!(!m.irq_line());
    }
}
