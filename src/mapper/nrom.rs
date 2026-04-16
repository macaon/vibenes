use crate::mapper::Mapper;
use crate::rom::{Cartridge, Mirroring};

pub struct Nrom {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    prg_ram: Vec<u8>,
    mirroring: Mirroring,
    chr_ram: bool,
    prg_mask: usize,
}

impl Nrom {
    pub fn new(cart: Cartridge) -> Self {
        let prg_mask = cart.prg_rom.len().saturating_sub(1);
        let prg_ram = vec![0; cart.prg_ram_size.max(0x2000)];
        Self {
            prg_rom: cart.prg_rom,
            chr: cart.chr_rom,
            prg_ram,
            mirroring: cart.mirroring,
            chr_ram: cart.chr_ram,
            prg_mask,
        }
    }
}

impl Mapper for Nrom {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => {
                let i = (addr - 0x6000) as usize;
                *self.prg_ram.get(i).unwrap_or(&0)
            }
            0x8000..=0xFFFF => {
                let i = (addr - 0x8000) as usize & self.prg_mask;
                self.prg_rom[i]
            }
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        if let 0x6000..=0x7FFF = addr {
            let i = (addr - 0x6000) as usize;
            if let Some(slot) = self.prg_ram.get_mut(i) {
                *slot = data;
            }
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

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => {
                let i = (addr - 0x6000) as usize;
                *self.prg_ram.get(i).unwrap_or(&0)
            }
            0x8000..=0xFFFF => {
                let i = (addr - 0x8000) as usize & self.prg_mask;
                self.prg_rom[i]
            }
            _ => 0,
        }
    }
}
