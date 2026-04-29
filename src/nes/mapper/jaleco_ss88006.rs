// SPDX-License-Identifier: GPL-3.0-or-later
//! Jaleco SS88006 (iNES mapper 18).
//!
//! Jaleco's workhorse 1990-1991 ASIC. Shipped on JF-23 through JF-40
//! boards covering roughly 15 games - The Lord of King, Magic John,
//! Pizza Pop, Shin Satomi Hakkenden, Plasma Ball, Mouryou Senki
//! Madara, Perman Part 2, etc. Some boards pair the SS88006 with a
//! µPD7755C / µPD7756C ADPCM sound IC; the expansion audio is out of
//! scope here (same as Mesen2 / puNES parked it for now).
//!
//! ## Programming model
//!
//! **PRG layout** - four 8 KB windows, last one fixed:
//! - `$8000-$9FFF` - 8 KB switchable (PRG bank 0)
//! - `$A000-$BFFF` - 8 KB switchable (PRG bank 1)
//! - `$C000-$DFFF` - 8 KB switchable (PRG bank 2)
//! - `$E000-$FFFF` - last 8 KB bank (fixed)
//!
//! **CHR layout** - eight independent 1 KB windows.
//!
//! ## The register scheme: nibbles written in pairs
//!
//! PRG and CHR bank selectors are 8 bits wide, but the mapper only
//! decodes the low 4 bits of the data bus. Each 8-bit bank register
//! therefore takes TWO writes: one to the even address (sets the low
//! nibble) and one to the odd address (sets the high nibble). The
//! register is decoded by `addr & 0xF003`:
//!
//! | address | effect |
//! |---|---|
//! | `$8000` / `$8001` | PRG bank 0 low / high nibble |
//! | `$8002` / `$8003` | PRG bank 1 low / high nibble |
//! | `$9000` / `$9001` | PRG bank 2 low / high nibble |
//! | `$9002` | PRG-RAM chip enable / write protect (ignored here - no shipping game tests it) |
//! | `$A000`-`$D003` | CHR banks 0-7 low / high nibble (same pattern) |
//! | `$E000`-`$E003` | 4 nibbles of 16-bit IRQ reload value (LSN-first) |
//! | `$F000` | Acknowledge IRQ + reload counter from the reload value |
//! | `$F001` | Acknowledge IRQ + set enable bit + counter size |
//! | `$F002` | Mirroring: 0=H, 1=V, 2=single-A, 3=single-B |
//! | `$F003` | Expansion audio (ADPCM) - unsupported |
//!
//! Note `$F002` mirroring has `0=Horizontal, 1=Vertical` - swapped
//! from MMC3 / MMC1 where `0=Vertical, 1=Horizontal`.
//!
//! ## IRQ
//!
//! 16-bit down counter, ticks every CPU cycle while enabled. `$F001`
//! also selects a counter SIZE: 16 / 12 / 8 / 4 bits. The low-N bits
//! of the stored 16-bit counter are what's clocked and compared to
//! zero; the high bits are preserved but stationary. Fires when the
//! masked portion hits zero after a pre-decrement (not before -
//! opposite of mapper 16's quirk). Load value N, enable → fires on
//! cycle N. On underflow the masked bits wrap back to `mask` and
//! keep counting.
//!
//! `$F000` reloads from the 4-nibble reload value; `$F001` changes
//! size / enable but does NOT reload the counter.
//!
//! ## PRG-RAM
//!
//! 8 KB at `$6000-$7FFF` when the cart declares it. Battery-backed
//! on some carts (JF-27 / JF-40 variants). `$9002` nominally gates
//! chip-enable + write-protect, but no shipping game is known to
//! test it - we leave it unwired, matching Mesen2 / puNES.
//!
//! Clean-room references (behavioral only, no copied code):
//! - `~/Git/Mesen2/Core/NES/Mappers/Jaleco/JalecoSs88006.h`
//! - `~/Git/punes/src/core/mappers/mapper_018.c`
//! - `~/Git/nestopia/source/core/board/NstBoardJalecoSs88006.cpp`
//! - nesdev.org/wiki/INES_Mapper_018

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_8K: usize = 8 * 1024;
const CHR_BANK_1K: usize = 1024;
const PRG_RAM_SIZE: usize = 8 * 1024;

/// Mask tables indexed by `irq_counter_size`. Index 0 = 16-bit, index
/// 3 = 4-bit. Matches Mesen2's `_irqMask` in `JalecoSs88006.h`.
const IRQ_MASKS: [u16; 4] = [0xFFFF, 0x0FFF, 0x00FF, 0x000F];

pub struct JalecoSs88006 {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,
    prg_ram: Vec<u8>,
    mirroring: Mirroring,

    prg_bank_count_8k: usize,
    chr_bank_count_1k: usize,

    /// 8-bit PRG bank selectors for the three switchable windows.
    /// Assembled from two 4-bit register writes.
    prg_banks: [u8; 3],
    /// 8-bit CHR bank selectors for the eight 1 KB windows.
    chr_banks: [u8; 8],

    /// 4 nibbles of the 16-bit IRQ reload value, LSN at index 0.
    irq_reload: [u8; 4],
    /// Live 16-bit IRQ counter. Only the low N bits (per
    /// `irq_counter_size`) are clocked; the high bits sit preserved.
    irq_counter: u16,
    /// 0 = 16-bit mode, 1 = 12-bit, 2 = 8-bit, 3 = 4-bit.
    irq_counter_size: u8,
    irq_enabled: bool,
    irq_line: bool,

    battery: bool,
    save_dirty: bool,
}

impl JalecoSs88006 {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count_8k = (cart.prg_rom.len() / PRG_BANK_8K).max(1);

        let is_chr_ram = cart.chr_ram || cart.chr_rom.is_empty();
        let chr = if is_chr_ram {
            vec![0u8; 8 * 1024]
        } else {
            cart.chr_rom
        };
        let chr_bank_count_1k = (chr.len() / CHR_BANK_1K).max(1);

        let prg_ram = vec![0u8; (cart.prg_ram_size + cart.prg_nvram_size).max(PRG_RAM_SIZE)];

        Self {
            prg_rom: cart.prg_rom,
            chr,
            chr_ram: is_chr_ram,
            prg_ram,
            mirroring: cart.mirroring,
            prg_bank_count_8k,
            chr_bank_count_1k,
            prg_banks: [0; 3],
            chr_banks: [0; 8],
            irq_reload: [0; 4],
            irq_counter: 0,
            irq_counter_size: 0,
            irq_enabled: false,
            irq_line: false,
            battery: cart.battery_backed,
            save_dirty: false,
        }
    }

    /// Merge a 4-bit write into the specified PRG bank register's
    /// low or high nibble. Leaves the other nibble intact.
    fn update_prg_nibble(&mut self, bank: usize, nibble: u8, high: bool) {
        self.prg_banks[bank] = if high {
            (self.prg_banks[bank] & 0x0F) | (nibble << 4)
        } else {
            (self.prg_banks[bank] & 0xF0) | nibble
        };
    }

    fn update_chr_nibble(&mut self, bank: usize, nibble: u8, high: bool) {
        self.chr_banks[bank] = if high {
            (self.chr_banks[bank] & 0x0F) | (nibble << 4)
        } else {
            (self.chr_banks[bank] & 0xF0) | nibble
        };
    }

    /// Reassemble the 16-bit reload value from its four 4-bit pieces.
    fn assembled_reload(&self) -> u16 {
        (self.irq_reload[0] as u16)
            | ((self.irq_reload[1] as u16) << 4)
            | ((self.irq_reload[2] as u16) << 8)
            | ((self.irq_reload[3] as u16) << 12)
    }

    fn map_prg(&self, addr: u16) -> usize {
        let bank = match addr {
            0x8000..=0x9FFF => (self.prg_banks[0] as usize) % self.prg_bank_count_8k,
            0xA000..=0xBFFF => (self.prg_banks[1] as usize) % self.prg_bank_count_8k,
            0xC000..=0xDFFF => (self.prg_banks[2] as usize) % self.prg_bank_count_8k,
            0xE000..=0xFFFF => self.prg_bank_count_8k.saturating_sub(1),
            _ => 0,
        };
        bank * PRG_BANK_8K + (addr as usize & (PRG_BANK_8K - 1))
    }

    fn map_chr(&self, addr: u16) -> usize {
        let reg = ((addr >> 10) & 0x07) as usize;
        let bank = (self.chr_banks[reg] as usize) % self.chr_bank_count_1k;
        bank * CHR_BANK_1K + (addr as usize & (CHR_BANK_1K - 1))
    }

    fn write_register(&mut self, addr: u16, data: u8) {
        // Every register takes only 4 bits; the top nibble of the
        // data bus is wired through but masked at this ASIC. Same
        // semantic Mesen2 uses (`value &= 0x0F`).
        let nibble = data & 0x0F;
        let high = (addr & 0x01) != 0;

        match addr & 0xF003 {
            0x8000 | 0x8001 => self.update_prg_nibble(0, nibble, high),
            0x8002 | 0x8003 => self.update_prg_nibble(1, nibble, high),
            0x9000 | 0x9001 => self.update_prg_nibble(2, nibble, high),
            // $9002 is PRG-RAM chip-enable / write-protect on paper;
            // no shipping game tests it, so we leave PRG-RAM always
            // enabled. $9003 is unused. Silently drop both.
            0x9002 | 0x9003 => {}

            0xA000 | 0xA001 => self.update_chr_nibble(0, nibble, high),
            0xA002 | 0xA003 => self.update_chr_nibble(1, nibble, high),
            0xB000 | 0xB001 => self.update_chr_nibble(2, nibble, high),
            0xB002 | 0xB003 => self.update_chr_nibble(3, nibble, high),
            0xC000 | 0xC001 => self.update_chr_nibble(4, nibble, high),
            0xC002 | 0xC003 => self.update_chr_nibble(5, nibble, high),
            0xD000 | 0xD001 => self.update_chr_nibble(6, nibble, high),
            0xD002 | 0xD003 => self.update_chr_nibble(7, nibble, high),

            0xE000 | 0xE001 | 0xE002 | 0xE003 => {
                self.irq_reload[(addr & 0x03) as usize] = nibble;
            }
            0xF000 => {
                self.irq_line = false;
                self.irq_counter = self.assembled_reload();
            }
            0xF001 => {
                self.irq_line = false;
                self.irq_enabled = (nibble & 0x01) != 0;
                // Size priority: smallest counter wins, matching
                // Mesen2's if/else chain (bit 3 > bit 2 > bit 1 > else).
                self.irq_counter_size = if nibble & 0x08 != 0 {
                    3 // 4-bit
                } else if nibble & 0x04 != 0 {
                    2 // 8-bit
                } else if nibble & 0x02 != 0 {
                    1 // 12-bit
                } else {
                    0 // 16-bit
                };
            }
            0xF002 => {
                // Mirroring: 0=H, 1=V (SWAPPED from MMC3 / MMC1).
                self.mirroring = match nibble & 0x03 {
                    0 => Mirroring::Horizontal,
                    1 => Mirroring::Vertical,
                    2 => Mirroring::SingleScreenLower,
                    _ => Mirroring::SingleScreenUpper,
                };
            }
            0xF003 => {
                // Expansion audio (µPD7755C / µPD7756C ADPCM). Out of
                // scope - same deferral as Mesen2 / puNES.
            }
            _ => {}
        }
    }
}

impl Mapper for JalecoSs88006 {
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
            0x8000..=0xFFFF => self.write_register(addr, data),
            _ => {}
        }
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => {
                let i = (addr - 0x6000) as usize;
                *self.prg_ram.get(i).unwrap_or(&0)
            }
            0x8000..=0xFFFF => {
                let i = self.map_prg(addr);
                *self.prg_rom.get(i).unwrap_or(&0)
            }
            _ => 0,
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr >= 0x2000 {
            return 0;
        }
        let i = self.map_chr(addr);
        *self.chr.get(i).unwrap_or(&0)
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if self.chr_ram && addr < 0x2000 {
            let i = self.map_chr(addr);
            if let Some(slot) = self.chr.get_mut(i) {
                *slot = data;
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
        // Decrement the masked portion; preserve the high bits.
        // Pre-decrement + compare-to-zero (opposite of mapper 16's
        // "check zero BEFORE decrement" quirk): load N → fire on
        // cycle N. Underflow wraps within the mask size.
        let mask = IRQ_MASKS[self.irq_counter_size as usize];
        let live = (self.irq_counter & mask).wrapping_sub(1) & mask;
        if live == 0 {
            self.irq_line = true;
        }
        self.irq_counter = (self.irq_counter & !mask) | live;
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

    fn save_state_capture(&self) -> Option<crate::save_state::MapperState> {
        use crate::save_state::mapper::{JalecoSnap, MirroringSnap};
        Some(crate::save_state::MapperState::Jaleco(Box::new(JalecoSnap {
            prg_ram: self.prg_ram.clone(),
            chr_ram_data: if self.chr_ram { self.chr.clone() } else { Vec::new() },
            mirroring: MirroringSnap::from_live(self.mirroring),
            prg_banks: self.prg_banks,
            chr_banks: self.chr_banks,
            irq_reload: self.irq_reload,
            irq_counter: self.irq_counter,
            irq_counter_size: self.irq_counter_size,
            irq_enabled: self.irq_enabled,
            irq_line: self.irq_line,
            save_dirty: self.save_dirty,
        })))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::Jaleco(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        if snap.prg_ram.len() == self.prg_ram.len() {
            self.prg_ram.copy_from_slice(&snap.prg_ram);
        }
        if self.chr_ram && snap.chr_ram_data.len() == self.chr.len() {
            self.chr.copy_from_slice(&snap.chr_ram_data);
        }
        self.mirroring = snap.mirroring.to_live();
        self.prg_banks = snap.prg_banks;
        self.chr_banks = snap.chr_banks;
        self.irq_reload = snap.irq_reload;
        self.irq_counter = snap.irq_counter;
        self.irq_counter_size = snap.irq_counter_size;
        self.irq_enabled = snap.irq_enabled;
        self.irq_line = snap.irq_line;
        self.save_dirty = snap.save_dirty;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, Mirroring, TvSystem};

    /// 256 KB PRG (32 × 8 KB banks) where each byte == its 8 KB bank
    /// index; 256 KB CHR (256 × 1 KB banks) where each byte == its
    /// 1 KB bank index. Lets tests read back "which bank is here"
    /// directly.
    fn tagged_cart() -> Cartridge {
        let mut prg = vec![0u8; 32 * PRG_BANK_8K];
        for b in 0..32 {
            prg[b * PRG_BANK_8K..(b + 1) * PRG_BANK_8K].fill(b as u8);
        }
        let mut chr = vec![0u8; 256 * CHR_BANK_1K];
        for b in 0..256 {
            chr[b * CHR_BANK_1K..(b + 1) * CHR_BANK_1K].fill(b as u8);
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: chr,
            chr_ram: false,
            mapper_id: 18,
            submapper: 0,
            mirroring: Mirroring::Horizontal,
            battery_backed: false,
            prg_ram_size: 0,
            prg_nvram_size: 0,
            tv_system: TvSystem::Ntsc,
            is_nes2: false,
            prg_chr_crc32: 0,
            db_matched: false,
            fds_data: None,
        }
    }

    fn battery_cart() -> Cartridge {
        let mut cart = tagged_cart();
        cart.battery_backed = true;
        cart
    }

    // ---- PRG nibble assembly ----

    #[test]
    fn prg_default_layout_fixes_last_bank() {
        let m = JalecoSs88006::new(tagged_cart());
        // All three switchable banks default to 0.
        assert_eq!(m.cpu_peek(0x8000), 0);
        assert_eq!(m.cpu_peek(0xA000), 0);
        assert_eq!(m.cpu_peek(0xC000), 0);
        // $E000 fixed to last bank (31).
        assert_eq!(m.cpu_peek(0xE000), 31);
        assert_eq!(m.cpu_peek(0xFFFF), 31);
    }

    #[test]
    fn prg_bank_0_assembled_from_two_nibble_writes() {
        let mut m = JalecoSs88006::new(tagged_cart());
        m.cpu_write(0x8000, 0x0A); // low nibble = 0xA
        m.cpu_write(0x8001, 0x01); // high nibble = 0x1 → bank 0x1A = 26
        assert_eq!(m.cpu_peek(0x8000), 26);
        // Other banks unchanged.
        assert_eq!(m.cpu_peek(0xA000), 0);
        assert_eq!(m.cpu_peek(0xC000), 0);
    }

    #[test]
    fn prg_bank_1_and_2_independent() {
        let mut m = JalecoSs88006::new(tagged_cart());
        m.cpu_write(0x8002, 0x05); // PRG 1 low
        m.cpu_write(0x8003, 0x00); // PRG 1 high → bank 5
        m.cpu_write(0x9000, 0x0C); // PRG 2 low
        m.cpu_write(0x9001, 0x00); // PRG 2 high → bank 12
        assert_eq!(m.cpu_peek(0x8000), 0);
        assert_eq!(m.cpu_peek(0xA000), 5);
        assert_eq!(m.cpu_peek(0xC000), 12);
        assert_eq!(m.cpu_peek(0xE000), 31);
    }

    #[test]
    fn prg_nibble_writes_only_use_low_4_bits_of_data() {
        let mut m = JalecoSs88006::new(tagged_cart());
        // High nibble of data bus must be ignored.
        m.cpu_write(0x8000, 0xF5); // low nibble = 5
        m.cpu_write(0x8001, 0xFA); // high nibble = 10 → bank 0xA5 → 0xA5 % 32 = 5
        assert_eq!(m.cpu_peek(0x8000), 5);
    }

    #[test]
    fn prg_register_decoded_by_f003_mask() {
        // $8000 ≡ $8100 ≡ $8F00 ≡ $8FF0 ≡ $8FFC (all mask to $8000).
        let mut m = JalecoSs88006::new(tagged_cart());
        m.cpu_write(0x8F00, 0x07); // PRG 0 low
        m.cpu_write(0x8FF1, 0x00); // PRG 0 high
        assert_eq!(m.cpu_peek(0x8000), 7);
    }

    // ---- CHR nibble assembly ----

    #[test]
    fn chr_banks_assembled_independently() {
        let mut m = JalecoSs88006::new(tagged_cart());
        let bank_reg_pairs = [
            (0xA000u16, 0xA001u16, 0),
            (0xA002, 0xA003, 1),
            (0xB000, 0xB001, 2),
            (0xB002, 0xB003, 3),
            (0xC000, 0xC001, 4),
            (0xC002, 0xC003, 5),
            (0xD000, 0xD001, 6),
            (0xD002, 0xD003, 7),
        ];
        for (i, &(lo, hi, bank_idx)) in bank_reg_pairs.iter().enumerate() {
            let value = 10 + i as u8;
            m.cpu_write(lo, value & 0x0F);
            m.cpu_write(hi, (value >> 4) & 0x0F);
            let addr = (bank_idx as u16) * CHR_BANK_1K as u16;
            assert_eq!(
                m.ppu_read(addr),
                value,
                "CHR bank {bank_idx} (regs ${:04X}/${:04X}) should read {value}",
                lo,
                hi
            );
        }
    }

    #[test]
    fn chr_bank_selector_is_full_8_bits() {
        let mut m = JalecoSs88006::new(tagged_cart());
        // Assemble 0xFF (255 - last bank in our 256-bank fixture).
        m.cpu_write(0xA000, 0x0F);
        m.cpu_write(0xA001, 0x0F);
        assert_eq!(m.ppu_read(0x0000), 255);
    }

    // ---- Mirroring ($F002 - note swapped 0=H, 1=V) ----

    #[test]
    fn f002_mirroring_all_four_modes() {
        let mut m = JalecoSs88006::new(tagged_cart());
        m.cpu_write(0xF002, 0);
        assert_eq!(m.mirroring(), Mirroring::Horizontal); // swapped
        m.cpu_write(0xF002, 1);
        assert_eq!(m.mirroring(), Mirroring::Vertical); // swapped
        m.cpu_write(0xF002, 2);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenLower);
        m.cpu_write(0xF002, 3);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenUpper);
    }

    // ---- IRQ: reload-value nibble assembly ----

    #[test]
    fn irq_reload_value_assembled_from_four_nibbles() {
        let mut m = JalecoSs88006::new(tagged_cart());
        m.cpu_write(0xE000, 0x0D); // LSN
        m.cpu_write(0xE001, 0x0E);
        m.cpu_write(0xE002, 0x0A);
        m.cpu_write(0xE003, 0x0B); // MSN → reload = 0xBAED
        // $F000 copies reload → counter.
        m.cpu_write(0xF000, 0);
        assert_eq!(m.irq_counter, 0xBAED);
    }

    #[test]
    fn f001_does_not_reload_counter() {
        // Writing $F001 sets enable/size but MUST NOT touch the live
        // counter. The typical game sequence is: write $F000 to
        // load, THEN $F001 to enable.
        let mut m = JalecoSs88006::new(tagged_cart());
        m.cpu_write(0xE000, 3);
        m.cpu_write(0xF000, 0); // reload → counter = 3
        assert_eq!(m.irq_counter, 3);
        m.cpu_write(0xF001, 0x01); // enable, 16-bit
        assert_eq!(m.irq_counter, 3, "$F001 must not reload");
    }

    // ---- IRQ: pre-decrement + zero-check ----

    #[test]
    fn irq_fires_exactly_n_cycles_after_enable() {
        // Load N, enable → fires on cycle N. Pre-decrement semantic.
        let mut m = JalecoSs88006::new(tagged_cart());
        m.cpu_write(0xE000, 3); // reload nibble 0
        m.cpu_write(0xF000, 0); // reload counter
        m.cpu_write(0xF001, 0x01); // enable, 16-bit mode
        for i in 1..=2 {
            m.on_cpu_cycle();
            assert!(!m.irq_line(), "fired early at cycle {i}");
        }
        m.on_cpu_cycle();
        assert!(m.irq_line(), "must fire on cycle N");
    }

    #[test]
    fn irq_disabled_does_not_tick_or_fire() {
        let mut m = JalecoSs88006::new(tagged_cart());
        m.cpu_write(0xE000, 1);
        m.cpu_write(0xF000, 0);
        for _ in 0..100 {
            m.on_cpu_cycle();
        }
        assert_eq!(m.irq_counter, 1);
        assert!(!m.irq_line());
    }

    // ---- IRQ: counter sizes ----

    #[test]
    fn irq_8_bit_counter_wraps_within_low_byte() {
        // 8-bit mode with high byte of counter set - the high byte
        // stays frozen; only the low byte clocks.
        let mut m = JalecoSs88006::new(tagged_cart());
        // Reload value = 0xAB05 - high byte 0xAB sticks, low byte 0x05
        // counts down.
        m.cpu_write(0xE000, 0x05);
        m.cpu_write(0xE001, 0x00);
        m.cpu_write(0xE002, 0x0B);
        m.cpu_write(0xE003, 0x0A);
        m.cpu_write(0xF000, 0); // reload
        // Enable + 8-bit mode ($F001 bit 2 → 8-bit).
        m.cpu_write(0xF001, 0x01 | 0x04);

        for _ in 0..4 {
            m.on_cpu_cycle();
        }
        // 5 → 4 → 3 → 2 → 1 after 4 ticks; high byte still 0xAB.
        assert_eq!(m.irq_counter & 0x00FF, 1);
        assert_eq!(m.irq_counter & 0xFF00, 0xAB00);
        assert!(!m.irq_line());
        m.on_cpu_cycle();
        // Cycle 5: low → 0, fires. High preserved.
        assert!(m.irq_line());
        assert_eq!(m.irq_counter & 0xFF00, 0xAB00);
    }

    #[test]
    fn irq_4_bit_counter_mode() {
        let mut m = JalecoSs88006::new(tagged_cart());
        m.cpu_write(0xE000, 0x02); // reload low nibble = 2
        m.cpu_write(0xF000, 0);
        // Enable + 4-bit mode (bit 3).
        m.cpu_write(0xF001, 0x01 | 0x08);
        m.on_cpu_cycle(); // 2 → 1
        assert!(!m.irq_line());
        m.on_cpu_cycle(); // 1 → 0 → fire
        assert!(m.irq_line());
    }

    #[test]
    fn irq_12_bit_counter_mode() {
        let mut m = JalecoSs88006::new(tagged_cart());
        m.cpu_write(0xE000, 0x03);
        m.cpu_write(0xF000, 0);
        // Enable + 12-bit mode (bit 1).
        m.cpu_write(0xF001, 0x01 | 0x02);
        for _ in 0..2 {
            m.on_cpu_cycle();
        }
        assert!(!m.irq_line());
        m.on_cpu_cycle();
        assert!(m.irq_line());
    }

    #[test]
    fn irq_smallest_counter_wins_when_multiple_size_bits_set() {
        // Bits 1+2+3 set → 4-bit mode (smallest wins, matches Mesen2
        // if/else priority).
        let mut m = JalecoSs88006::new(tagged_cart());
        m.cpu_write(0xE000, 0x01);
        m.cpu_write(0xF000, 0);
        m.cpu_write(0xF001, 0x01 | 0x02 | 0x04 | 0x08);
        m.on_cpu_cycle(); // 1 → 0 → fire in 4-bit mode
        assert!(m.irq_line());
    }

    // ---- IRQ: acknowledge ----

    #[test]
    fn f000_write_acknowledges_pending_irq() {
        let mut m = JalecoSs88006::new(tagged_cart());
        m.cpu_write(0xE000, 1);
        m.cpu_write(0xF000, 0);
        m.cpu_write(0xF001, 0x01);
        m.on_cpu_cycle();
        assert!(m.irq_line());
        m.cpu_write(0xF000, 0); // also reloads
        assert!(!m.irq_line());
    }

    #[test]
    fn f001_write_acknowledges_pending_irq() {
        let mut m = JalecoSs88006::new(tagged_cart());
        m.cpu_write(0xE000, 1);
        m.cpu_write(0xF000, 0);
        m.cpu_write(0xF001, 0x01);
        m.on_cpu_cycle();
        assert!(m.irq_line());
        m.cpu_write(0xF001, 0x00); // disable + ack
        assert!(!m.irq_line());
    }

    #[test]
    fn irq_wraps_and_keeps_counting_at_zero() {
        // With a masked underflow, the counter wraps to `mask` and
        // keeps firing periodically.
        let mut m = JalecoSs88006::new(tagged_cart());
        // 4-bit mode, reload = 1.
        m.cpu_write(0xE000, 1);
        m.cpu_write(0xF000, 0);
        m.cpu_write(0xF001, 0x01 | 0x08);
        m.on_cpu_cycle(); // fire #1 (counter 1 → 0)
        assert!(m.irq_line());
        // Ack.
        m.cpu_write(0xF001, 0x01 | 0x08);
        assert!(!m.irq_line());
        // 16 more cycles: 0 → 0xF → 0xE → ... → 0 → fire again.
        for _ in 0..15 {
            m.on_cpu_cycle();
        }
        assert!(!m.irq_line(), "mid-wrap should not fire");
        m.on_cpu_cycle();
        assert!(m.irq_line(), "fires again after full wrap");
    }

    // ---- PRG-RAM + battery ----

    #[test]
    fn prg_ram_roundtrip_at_6000_7fff() {
        let mut m = JalecoSs88006::new(tagged_cart());
        m.cpu_write(0x6000, 0xAB);
        m.cpu_write(0x7FFF, 0xCD);
        assert_eq!(m.cpu_peek(0x6000), 0xAB);
        assert_eq!(m.cpu_peek(0x7FFF), 0xCD);
    }

    #[test]
    fn non_battery_cart_exposes_no_save_data() {
        let m = JalecoSs88006::new(tagged_cart());
        assert!(m.save_data().is_none());
        assert!(!m.save_dirty());
    }

    #[test]
    fn battery_cart_exposes_8kib_save_data() {
        let mut m = JalecoSs88006::new(battery_cart());
        assert_eq!(m.save_data().map(|s| s.len()), Some(PRG_RAM_SIZE));
        assert!(!m.save_dirty());
        m.cpu_write(0x6000, 0x42);
        assert!(m.save_dirty());
        m.mark_saved();
        assert!(!m.save_dirty());
    }

    #[test]
    fn battery_load_save_data_roundtrip() {
        let mut m = JalecoSs88006::new(battery_cart());
        let mut snapshot = vec![0u8; PRG_RAM_SIZE];
        snapshot[0] = 0x11;
        snapshot[PRG_RAM_SIZE - 1] = 0x22;
        m.load_save_data(&snapshot);
        assert_eq!(m.cpu_peek(0x6000), 0x11);
        assert_eq!(m.cpu_peek(0x7FFF), 0x22);
    }

    #[test]
    fn non_battery_write_does_not_dirty_save() {
        let mut m = JalecoSs88006::new(tagged_cart());
        m.cpu_write(0x6000, 0xAA);
        // Byte lands in RAM but save state stays clean.
        assert_eq!(m.cpu_peek(0x6000), 0xAA);
        assert!(!m.save_dirty());
    }

    // ---- Edges ----

    #[test]
    fn cpu_writes_below_6000_are_dropped() {
        let mut m = JalecoSs88006::new(tagged_cart());
        m.cpu_write(0x4020, 0xFF);
        m.cpu_write(0x5FFF, 0xFF);
        assert_eq!(m.cpu_peek(0x8000), 0);
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
    }

    #[test]
    fn f003_expansion_audio_is_silently_dropped() {
        // Mesen2 parks this pending expansion audio support. For us
        // it should be a no-op - no side effects on other state.
        let mut m = JalecoSs88006::new(tagged_cart());
        m.cpu_write(0xE000, 5); // stage some reload state
        m.cpu_write(0xF003, 0xFF);
        // IRQ state untouched.
        assert_eq!(m.irq_reload[0], 5);
        assert!(!m.irq_enabled);
    }
}
