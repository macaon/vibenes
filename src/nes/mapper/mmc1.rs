// SPDX-License-Identifier: GPL-3.0-or-later
//! MMC1 / SxROM (mapper 1).
//!
//! Writes to $8000-$FFFF feed a 5-bit serial shift register. After the
//! fifth write the shift is committed to one of four internal registers
//! selected by address bits 14-13. Writing with bit 7 set resets the
//! shifter and OR's the control register with $0C, i.e. forces PRG mode 3
//! (fixed last bank at $C000).
//!
//! Consecutive-write filter: if the CPU writes on two adjacent cycles
//! (typical for RMW's dummy-write-then-real-write pair), only the first
//! write is honored. We track the last CPU cycle we accepted a write on
//! and drop writes that arrive exactly one cycle later.

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_16K: usize = 16 * 1024;
const CHR_BANK_4K: usize = 4 * 1024;

pub struct Mmc1 {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    prg_ram: Vec<u8>,
    chr_ram: bool,

    // Shift register + write count (5 serial bits per commit).
    shift: u8,
    shift_count: u8,

    // Committed registers.
    control: u8,
    chr_bank_0: u8,
    chr_bank_1: u8,
    prg_bank: u8,

    // Derived / cached.
    mirroring: Mirroring,

    // Consecutive-write filter.
    cycle_counter: u64,
    last_write_cycle: Option<u64>,

    prg_bank_count_16k: usize,

    battery: bool,
    save_dirty: bool,
}

impl Mmc1 {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count_16k = (cart.prg_rom.len() / PRG_BANK_16K).max(1);
        let prg_ram = vec![0; (cart.prg_ram_size + cart.prg_nvram_size).max(0x2000)];
        let chr = if cart.chr_ram {
            vec![0; 8 * 1024]
        } else {
            cart.chr_rom
        };
        let mut m = Self {
            prg_rom: cart.prg_rom,
            chr,
            prg_ram,
            chr_ram: cart.chr_ram,
            shift: 0x10,
            shift_count: 0,
            control: 0x0C, // PRG mode 3 on power-up
            chr_bank_0: 0,
            chr_bank_1: 0,
            prg_bank: 0,
            mirroring: cart.mirroring,
            cycle_counter: 0,
            last_write_cycle: None,
            prg_bank_count_16k,
            battery: cart.battery_backed,
            save_dirty: false,
        };
        m.refresh_mirroring();
        m
    }

    fn refresh_mirroring(&mut self) {
        self.mirroring = match self.control & 0x03 {
            0 => Mirroring::SingleScreenLower,
            1 => Mirroring::SingleScreenUpper,
            2 => Mirroring::Vertical,
            3 => Mirroring::Horizontal,
            _ => unreachable!(),
        };
    }

    fn prg_mode(&self) -> u8 {
        (self.control >> 2) & 0x03
    }

    fn chr_mode_4k(&self) -> bool {
        (self.control & 0x10) != 0
    }

    fn last_prg_bank(&self) -> usize {
        self.prg_bank_count_16k.saturating_sub(1)
    }

    fn map_prg(&self, addr: u16) -> usize {
        let bank_sel = (self.prg_bank & 0x0F) as usize;
        let bank = match self.prg_mode() {
            0 | 1 => {
                // 32KB switch; bit 0 ignored.
                let base = bank_sel & !1;
                match addr {
                    0x8000..=0xBFFF => base,
                    0xC000..=0xFFFF => base + 1,
                    _ => 0,
                }
            }
            2 => {
                // Fix first bank at $8000, switch $C000.
                match addr {
                    0x8000..=0xBFFF => 0,
                    0xC000..=0xFFFF => bank_sel,
                    _ => 0,
                }
            }
            3 => {
                // Switch $8000, fix last at $C000.
                match addr {
                    0x8000..=0xBFFF => bank_sel,
                    0xC000..=0xFFFF => self.last_prg_bank(),
                    _ => 0,
                }
            }
            _ => 0,
        };
        let bank = bank % self.prg_bank_count_16k.max(1);
        let offset = (addr as usize) & (PRG_BANK_16K - 1);
        bank * PRG_BANK_16K + offset
    }

    fn map_chr(&self, addr: u16) -> usize {
        let len = self.chr.len().max(1);
        let banks_4k = len / CHR_BANK_4K;
        if banks_4k == 0 {
            return addr as usize % len;
        }
        let mask_4k = banks_4k - 1;
        if self.chr_mode_4k() {
            let bank = match addr {
                0x0000..=0x0FFF => (self.chr_bank_0 as usize) & mask_4k,
                0x1000..=0x1FFF => (self.chr_bank_1 as usize) & mask_4k,
                _ => 0,
            };
            let offset = (addr as usize) & (CHR_BANK_4K - 1);
            (bank * CHR_BANK_4K + offset) % len
        } else {
            // 8KB mode: chr_bank_0 bit 0 ignored.
            let base = ((self.chr_bank_0 as usize) & !1) & mask_4k;
            let offset = addr as usize;
            (base * CHR_BANK_4K + offset) % len
        }
    }

    fn commit(&mut self, addr: u16, value: u8) {
        match addr {
            0x8000..=0x9FFF => {
                self.control = value & 0x1F;
                self.refresh_mirroring();
            }
            0xA000..=0xBFFF => {
                self.chr_bank_0 = value & 0x1F;
            }
            0xC000..=0xDFFF => {
                self.chr_bank_1 = value & 0x1F;
            }
            0xE000..=0xFFFF => {
                self.prg_bank = value & 0x1F;
            }
            _ => {}
        }
    }

    fn feed_shift(&mut self, addr: u16, data: u8) {
        if (data & 0x80) != 0 {
            self.shift = 0x10;
            self.shift_count = 0;
            self.control |= 0x0C;
            self.refresh_mirroring();
            return;
        }
        self.shift = (self.shift >> 1) | ((data & 1) << 4);
        self.shift_count += 1;
        if self.shift_count == 5 {
            let value = self.shift & 0x1F;
            self.commit(addr, value);
            self.shift = 0x10;
            self.shift_count = 0;
        }
    }
}

impl Mapper for Mmc1 {
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
                if let Some(prev) = self.last_write_cycle {
                    if self.cycle_counter == prev.wrapping_add(1) {
                        // Consecutive-write bug: drop the second write.
                        return;
                    }
                }
                self.last_write_cycle = Some(self.cycle_counter);
                self.feed_shift(addr, data);
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
            0x8000..=0xFFFF => {
                let i = self.map_prg(addr);
                *self.prg_rom.get(i).unwrap_or(&0)
            }
            _ => 0,
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr < 0x2000 {
            let i = self.map_chr(addr);
            *self.chr.get(i).unwrap_or(&0)
        } else {
            0
        }
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
        self.cycle_counter = self.cycle_counter.wrapping_add(1);
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
        use crate::save_state::mapper::{Mmc1Snap, MirroringSnap};
        Some(crate::save_state::MapperState::Mmc1(Mmc1Snap {
            prg_ram: self.prg_ram.clone(),
            chr_ram_data: if self.chr_ram { self.chr.clone() } else { Vec::new() },
            shift: self.shift,
            shift_count: self.shift_count,
            control: self.control,
            chr_bank_0: self.chr_bank_0,
            chr_bank_1: self.chr_bank_1,
            prg_bank: self.prg_bank,
            mirroring: MirroringSnap::from_live(self.mirroring),
            cycle_counter: self.cycle_counter,
            last_write_cycle: self.last_write_cycle,
            save_dirty: self.save_dirty,
        }))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::Mmc1(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        if snap.prg_ram.len() == self.prg_ram.len() {
            self.prg_ram.copy_from_slice(&snap.prg_ram);
        }
        if self.chr_ram && snap.chr_ram_data.len() == self.chr.len() {
            self.chr.copy_from_slice(&snap.chr_ram_data);
        }
        self.shift = snap.shift;
        self.shift_count = snap.shift_count;
        self.control = snap.control;
        self.chr_bank_0 = snap.chr_bank_0;
        self.chr_bank_1 = snap.chr_bank_1;
        self.prg_bank = snap.prg_bank;
        self.mirroring = snap.mirroring.to_live();
        self.cycle_counter = snap.cycle_counter;
        self.last_write_cycle = snap.last_write_cycle;
        self.save_dirty = snap.save_dirty;
        Ok(())
    }
}
