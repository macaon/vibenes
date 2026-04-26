// SPDX-License-Identifier: GPL-3.0-or-later
use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

pub struct Nrom {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    prg_ram: Vec<u8>,
    mirroring: Mirroring,
    chr_ram: bool,
    prg_mask: usize,
    battery: bool,
    save_dirty: bool,
}

impl Nrom {
    pub fn new(cart: Cartridge) -> Self {
        let prg_mask = cart.prg_rom.len().saturating_sub(1);
        // NROM's work-RAM window at $6000-$7FFF is 8 KiB; we always
        // allocate at least that so carts that forgot to declare RAM
        // still get a valid window for scratch. If the header says
        // more (e.g. Family Basic), honor it.
        let total_ram = cart.prg_ram_size + cart.prg_nvram_size;
        let prg_ram = vec![0; total_ram.max(0x2000)];
        Self {
            prg_rom: cart.prg_rom,
            chr: cart.chr_rom,
            prg_ram,
            mirroring: cart.mirroring,
            chr_ram: cart.chr_ram,
            prg_mask,
            battery: cart.battery_backed,
            save_dirty: false,
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
                if *slot != data {
                    *slot = data;
                    if self.battery {
                        self.save_dirty = true;
                    }
                }
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
    use crate::nes::rom::{Cartridge, TvSystem};

    fn cart(battery: bool) -> Cartridge {
        Cartridge {
            prg_rom: vec![0u8; 0x8000],
            chr_rom: vec![0u8; 0x2000],
            chr_ram: false,
            mapper_id: 0,
            submapper: 0,
            mirroring: Mirroring::Vertical,
            battery_backed: battery,
            prg_ram_size: if battery { 0 } else { 0x2000 },
            prg_nvram_size: if battery { 0x2000 } else { 0 },
            tv_system: TvSystem::Ntsc,
            is_nes2: false,
            prg_chr_crc32: 0,
            db_matched: false,
            fds_data: None,
        }
    }

    #[test]
    fn non_battery_cart_has_no_save_data() {
        let m = Nrom::new(cart(false));
        assert!(m.save_data().is_none());
        assert!(!m.save_dirty());
    }

    #[test]
    fn write_sets_dirty_and_save_roundtrip_restores_ram() {
        let mut m = Nrom::new(cart(true));
        assert!(m.save_data().is_some());
        assert!(!m.save_dirty());

        // Write a distinctive pattern across the RAM window.
        for i in 0..0x10 {
            m.cpu_write(0x6000 + i, 0xA0 ^ i as u8);
        }
        assert!(m.save_dirty());

        // Snapshot + mark clean, then zero the RAM via fresh load,
        // then restore and verify the pattern reads back.
        let snapshot = m.save_data().unwrap().to_vec();
        m.mark_saved();
        assert!(!m.save_dirty());

        let mut fresh = Nrom::new(cart(true));
        fresh.load_save_data(&snapshot);
        for i in 0..0x10 {
            assert_eq!(fresh.cpu_read(0x6000 + i), 0xA0 ^ i as u8);
        }
        // Loading shouldn't mark the new mapper dirty (no bus writes).
        assert!(!fresh.save_dirty());
    }

    #[test]
    fn load_with_wrong_size_is_ignored() {
        let mut m = Nrom::new(cart(true));
        m.cpu_write(0x6000, 0x42);
        m.mark_saved();
        m.load_save_data(&[0xFF; 16]); // size mismatch
        assert_eq!(m.cpu_read(0x6000), 0x42, "short slice must not overwrite");
    }

    #[test]
    fn redundant_write_does_not_set_dirty() {
        // Games often write the same byte repeatedly (menu polling,
        // fixed-data stores). Don't wake the save pipeline for those.
        let mut m = Nrom::new(cart(true));
        m.cpu_write(0x6000, 0x77);
        m.mark_saved();
        assert!(!m.save_dirty());
        m.cpu_write(0x6000, 0x77);
        assert!(!m.save_dirty(), "same-value write must not re-dirty");
    }
}
