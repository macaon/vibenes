// SPDX-License-Identifier: GPL-3.0-or-later
//! CNROM (mapper 3).
//!
//! PRG-ROM is fixed (16KB or 32KB mapped into $8000-$FFFF, 16KB mirrors
//! at $C000-$FFFF). Writes to $8000-$FFFF select an 8KB CHR-ROM bank —
//! this is the entire programming model. A handful of CNROM boards only
//! honor the low 2 bits, others decode all 8; most games don't notice.

use crate::mapper::Mapper;
use crate::rom::{Cartridge, Mirroring};

const PRG_BANK_16K: usize = 16 * 1024;
const CHR_BANK_8K: usize = 8 * 1024;

pub struct Cnrom {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    prg_ram: Vec<u8>,
    mirroring: Mirroring,
    chr_ram: bool,
    chr_bank: u8,
    chr_bank_count: usize,
    prg_mirror_mask: usize,
    battery: bool,
    save_dirty: bool,
}

impl Cnrom {
    pub fn new(cart: Cartridge) -> Self {
        let chr_bank_count = (cart.chr_rom.len() / CHR_BANK_8K).max(1);
        let prg_mirror_mask = if cart.prg_rom.len() <= PRG_BANK_16K {
            PRG_BANK_16K - 1
        } else {
            cart.prg_rom.len() - 1
        };
        let prg_ram = vec![0; (cart.prg_ram_size + cart.prg_nvram_size).max(0x2000)];
        Self {
            prg_rom: cart.prg_rom,
            chr: cart.chr_rom,
            prg_ram,
            mirroring: cart.mirroring,
            chr_ram: cart.chr_ram,
            chr_bank: 0,
            chr_bank_count,
            prg_mirror_mask,
            battery: cart.battery_backed,
            save_dirty: false,
        }
    }

    fn chr_index(&self, addr: u16) -> usize {
        let bank = (self.chr_bank as usize) % self.chr_bank_count;
        bank * CHR_BANK_8K + addr as usize
    }
}

impl Mapper for Cnrom {
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
                self.chr_bank = data & 0x03;
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
                let i = (addr as usize - 0x8000) & self.prg_mirror_mask;
                *self.prg_rom.get(i).unwrap_or(&0)
            }
            _ => 0,
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr < 0x2000 {
            let i = self.chr_index(addr) % self.chr.len().max(1);
            *self.chr.get(i).unwrap_or(&0)
        } else {
            0
        }
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if self.chr_ram && addr < 0x2000 {
            let i = self.chr_index(addr) % self.chr.len().max(1);
            if let Some(slot) = self.chr.get_mut(i) {
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
