// SPDX-License-Identifier: GPL-3.0-or-later
//! Sunsoft-3 - iNES mapper 67. Used by *Fantasy Zone II*, *Mito
//! Koumon* (JP), and a small handful of other Sunsoft-licensed JP
//! titles. The chip ships in cartridge form as the "Sunsoft 5347"
//! ASIC (NES-1 era, before the FME-7 / 5A / 5B family).
//!
//! ## Register surface (`addr & 0xF800`)
//!
//! | Address  | Effect                                             |
//! |----------|----------------------------------------------------|
//! | `$8800`  | CHR bank 0 (2 KiB at PPU `$0000-$07FF`)            |
//! | `$9800`  | CHR bank 1 (`$0800-$0FFF`)                         |
//! | `$A800`  | CHR bank 2 (`$1000-$17FF`)                         |
//! | `$B800`  | CHR bank 3 (`$1800-$1FFF`)                         |
//! | `$C800`  | IRQ counter write (alternating high then low byte) |
//! | `$D800`  | IRQ control: bit 4 enables; ack pending IRQ        |
//! | `$E800`  | Mirroring: 0=V, 1=H, 2=single-lower, 3=single-upper|
//! | `$F800`  | PRG bank: 16 KiB at `$8000`; `$C000` fixed to last |
//!
//! ## IRQ semantics
//!
//! 16-bit down-counter that ticks every CPU cycle when enabled.
//! IRQ asserts on the `$0000 -> $FFFF` underflow transition. On the
//! IRQ firing the `irq_enabled` bit is cleared (so the counter
//! stops ticking until re-enabled) but the line stays high until
//! acknowledged via a write to `$D800`. The `$C800` write uses a
//! two-step toggle: the first write sets the high byte, the second
//! sets the low byte (or vice versa - the toggle starts cleared).
//! Writing `$D800` resets the toggle so a fresh `$C800` pair always
//! starts on the high byte.
//!
//! References:
//! - <https://www.nesdev.org/wiki/INES_Mapper_067>
//! - `~/Git/Mesen2/Core/NES/Mappers/Sunsoft/Sunsoft3.h`
//! - `~/Git/punes/src/core/mappers/mapper_067.c`

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_16K: usize = 16 * 1024;
const CHR_BANK_2K: usize = 2 * 1024;

pub struct Sunsoft3 {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,
    prg_ram: Vec<u8>,

    /// Switchable 16 KiB PRG bank at `$8000-$BFFF`. `$C000-$FFFF`
    /// is hardwired to the last bank.
    prg_bank: u8,
    /// Four 2 KiB CHR banks for the four PPU windows.
    chr_banks: [u8; 4],

    prg_bank_count_16k: usize,
    chr_bank_count_2k: usize,

    mirroring: Mirroring,

    /// IRQ counter byte-write toggle. Cleared by writes to `$D800`;
    /// each write to `$C800` flips it.
    irq_toggle: bool,
    /// Live 16-bit IRQ counter.
    irq_counter: u16,
    /// Counter-ticks-and-fires gate. Set/cleared by `$D800` bit 4.
    /// Mesen2's clean-room behavior: cleared automatically on
    /// underflow, so the IRQ fires once per re-arm.
    irq_enabled: bool,
    /// Latched output line. Stays asserted from underflow until a
    /// `$D800` write acknowledges it.
    irq_line: bool,

    battery: bool,
    save_dirty: bool,
}

impl Sunsoft3 {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count_16k = (cart.prg_rom.len() / PRG_BANK_16K).max(1);
        let chr_ram = cart.chr_ram || cart.chr_rom.is_empty();
        let chr = if chr_ram {
            // Commercial Sunsoft-3 carts ship CHR-ROM, but homebrew
            // / mis-tagged dumps may declare CHR-RAM. Allocate a
            // single 8 KiB bank in that case.
            vec![0u8; 8 * 1024]
        } else {
            cart.chr_rom
        };
        let chr_bank_count_2k = (chr.len() / CHR_BANK_2K).max(1);
        let prg_ram_total = (cart.prg_ram_size + cart.prg_nvram_size).max(0x2000);
        Self {
            prg_rom: cart.prg_rom,
            chr,
            chr_ram,
            prg_ram: vec![0u8; prg_ram_total],
            prg_bank: 0,
            chr_banks: [0; 4],
            prg_bank_count_16k,
            chr_bank_count_2k,
            mirroring: cart.mirroring,
            irq_toggle: false,
            irq_counter: 0,
            irq_enabled: false,
            irq_line: false,
            battery: cart.battery_backed,
            save_dirty: false,
        }
    }

    fn prg_index(&self, addr: u16) -> usize {
        // $8000-$BFFF = switchable; $C000-$FFFF = last bank.
        let bank = if addr < 0xC000 {
            (self.prg_bank as usize) % self.prg_bank_count_16k
        } else {
            self.prg_bank_count_16k - 1
        };
        let off = (addr as usize) & (PRG_BANK_16K - 1);
        bank * PRG_BANK_16K + off
    }

    fn chr_index(&self, addr: u16) -> usize {
        let slot = ((addr >> 11) & 0x03) as usize; // 4× 2 KiB windows
        let bank = (self.chr_banks[slot] as usize) % self.chr_bank_count_2k;
        let off = (addr as usize) & (CHR_BANK_2K - 1);
        bank * CHR_BANK_2K + off
    }
}

impl Mapper for Sunsoft3 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => {
                let i = (addr - 0x6000) as usize;
                self.prg_ram.get(i).copied().unwrap_or(0)
            }
            0x8000..=0xFFFF => {
                let idx = self.prg_index(addr);
                self.prg_rom.get(idx).copied().unwrap_or(0)
            }
            _ => 0,
        }
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => {
                let i = (addr - 0x6000) as usize;
                self.prg_ram.get(i).copied().unwrap_or(0)
            }
            0x8000..=0xFFFF => {
                let idx = self.prg_index(addr);
                self.prg_rom.get(idx).copied().unwrap_or(0)
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
                match addr & 0xF800 {
                    0x8800 => self.chr_banks[0] = data,
                    0x9800 => self.chr_banks[1] = data,
                    0xA800 => self.chr_banks[2] = data,
                    0xB800 => self.chr_banks[3] = data,
                    0xC800 => {
                        // Two-write toggle: high byte then low byte.
                        // Mesen2 and puNES both initialize the toggle
                        // at false, with the first write setting the
                        // high byte. Without the toggle, runaway
                        // single-byte writes (BG flicker / status
                        // updates) would scramble the counter.
                        if self.irq_toggle {
                            self.irq_counter = (self.irq_counter & 0xFF00) | (data as u16);
                        } else {
                            self.irq_counter =
                                (self.irq_counter & 0x00FF) | ((data as u16) << 8);
                        }
                        self.irq_toggle = !self.irq_toggle;
                    }
                    0xD800 => {
                        self.irq_enabled = (data & 0x10) != 0;
                        self.irq_toggle = false;
                        self.irq_line = false;
                    }
                    0xE800 => {
                        self.mirroring = match data & 0x03 {
                            0 => Mirroring::Vertical,
                            1 => Mirroring::Horizontal,
                            2 => Mirroring::SingleScreenLower,
                            _ => Mirroring::SingleScreenUpper,
                        };
                    }
                    0xF800 => {
                        self.prg_bank = data;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr < 0x2000 {
            let idx = self.chr_index(addr);
            self.chr.get(idx).copied().unwrap_or(0)
        } else {
            0
        }
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if !self.chr_ram || addr >= 0x2000 {
            return;
        }
        let idx = self.chr_index(addr);
        if let Some(slot) = self.chr.get_mut(idx) {
            *slot = data;
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }

    fn on_cpu_cycle(&mut self) {
        if !self.irq_enabled {
            return;
        }
        // Tick. Underflow ($0000 -> $FFFF) fires the IRQ and
        // disables the counter so it doesn't re-fire on every wrap.
        let (next, underflow) = self.irq_counter.overflowing_sub(1);
        self.irq_counter = next;
        if underflow {
            self.irq_enabled = false;
            self.irq_line = true;
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

    fn save_state_capture(&self) -> Option<crate::save_state::MapperState> {
        use crate::save_state::mapper::{MirroringSnap, Sunsoft3Snap};
        Some(crate::save_state::MapperState::Sunsoft3(Sunsoft3Snap {
            prg_ram: self.prg_ram.clone(),
            chr_ram_data: if self.chr_ram { self.chr.clone() } else { Vec::new() },
            prg_bank: self.prg_bank,
            chr_banks: self.chr_banks,
            mirroring: MirroringSnap::from_live(self.mirroring),
            irq_toggle: self.irq_toggle,
            irq_counter: self.irq_counter,
            irq_enabled: self.irq_enabled,
            irq_line: self.irq_line,
            save_dirty: self.save_dirty,
        }))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::Sunsoft3(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        if snap.prg_ram.len() == self.prg_ram.len() {
            self.prg_ram.copy_from_slice(&snap.prg_ram);
        }
        if self.chr_ram && snap.chr_ram_data.len() == self.chr.len() {
            self.chr.copy_from_slice(&snap.chr_ram_data);
        }
        self.prg_bank = snap.prg_bank;
        self.chr_banks = snap.chr_banks;
        self.mirroring = snap.mirroring.to_live();
        self.irq_toggle = snap.irq_toggle;
        self.irq_counter = snap.irq_counter;
        self.irq_enabled = snap.irq_enabled;
        self.irq_line = snap.irq_line;
        self.save_dirty = snap.save_dirty;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, TvSystem};

    fn cart() -> Cartridge {
        Cartridge {
            prg_rom: vec![0u8; 0x20000], // 128 KiB PRG: 8 banks
            chr_rom: vec![0u8; 0x10000], // 64 KiB CHR: 32 × 2 KiB
            chr_ram: false,
            mapper_id: 67,
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
    fn power_on_state_matches_nrom() {
        let m = Sunsoft3::new(cart());
        assert_eq!(m.prg_bank, 0);
        assert_eq!(m.chr_banks, [0; 4]);
        assert!(!m.irq_enabled);
        assert!(!m.irq_line);
    }

    #[test]
    fn c800_two_writes_high_then_low() {
        let mut m = Sunsoft3::new(cart());
        m.cpu_write(0xC800, 0x12);
        m.cpu_write(0xC800, 0x34);
        assert_eq!(m.irq_counter, 0x1234);
    }

    #[test]
    fn d800_resets_toggle_and_line() {
        let mut m = Sunsoft3::new(cart());
        // Half-write to flip the toggle.
        m.cpu_write(0xC800, 0xAA);
        assert!(m.irq_toggle);
        m.irq_line = true;
        m.cpu_write(0xD800, 0x10);
        assert!(!m.irq_toggle);
        assert!(!m.irq_line);
        assert!(m.irq_enabled);
    }

    #[test]
    fn underflow_fires_irq_and_disables_counter() {
        let mut m = Sunsoft3::new(cart());
        m.cpu_write(0xC800, 0x00); // high
        m.cpu_write(0xC800, 0x01); // low → counter = 0x0001
        m.cpu_write(0xD800, 0x10); // enable
        m.on_cpu_cycle(); // 0x0001 -> 0x0000
        assert!(!m.irq_line);
        assert!(m.irq_enabled);
        m.on_cpu_cycle(); // 0x0000 -> 0xFFFF underflow
        assert!(m.irq_line);
        assert!(!m.irq_enabled, "counter must disarm itself on underflow");
    }

    #[test]
    fn mirroring_register_decodes_all_four_modes() {
        let mut m = Sunsoft3::new(cart());
        m.cpu_write(0xE800, 0);
        assert_eq!(m.mirroring, Mirroring::Vertical);
        m.cpu_write(0xE800, 1);
        assert_eq!(m.mirroring, Mirroring::Horizontal);
        m.cpu_write(0xE800, 2);
        assert_eq!(m.mirroring, Mirroring::SingleScreenLower);
        m.cpu_write(0xE800, 3);
        assert_eq!(m.mirroring, Mirroring::SingleScreenUpper);
    }

    #[test]
    fn prg_c000_fixed_to_last_bank() {
        let mut prg = vec![0u8; 0x20000];
        prg[0x1FFFF] = 0xAB; // top of last 16 KiB bank
        let cart = Cartridge {
            prg_rom: prg,
            chr_rom: vec![0u8; 0x10000],
            chr_ram: false,
            mapper_id: 67,
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
        };
        let mut m = Sunsoft3::new(cart);
        // Write 0 to PRG bank reg; $C000-$FFFF still uses last bank.
        m.cpu_write(0xF800, 0);
        assert_eq!(m.cpu_read(0xFFFF), 0xAB);
    }
}
