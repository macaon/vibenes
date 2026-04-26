// SPDX-License-Identifier: GPL-3.0-or-later
//! SNES system bus - the 24-bit address space the 5A22 sees. Phase 2d
//! ships a minimal LoROM-only implementation: 128 KiB WRAM, ROM
//! mapped per LoROM, all MMIO stubbed to swallow writes and return
//! the open-bus latch. Real PPU / APU / DMA / IRQ surfaces land in
//! Phases 3-5.
//!
//! Per-region access speeds match the real bus (6 / 8 / 12 master
//! cycles per access) so cycle assertions against the test ROMs
//! don't drift. MEMSEL ($420D bit 0) flips banks $80-$FF to FastROM
//! when set; reset value is 0 (SlowROM) per the wiki.

use crate::snes::cpu::bus::SnesBus;
use crate::snes::rom::{Cartridge, MapMode};

/// Total master-clock ticks each access region charges per byte.
/// "Slow" is the cart default; "fast" is FastROM-eligible cart half
/// when MEMSEL is set; "io" is the legacy serial-joypad strip.
const SLOW: u64 = 8;
const FAST: u64 = 6;
const XSLOW: u64 = 12;

pub struct LoRomBus {
    /// Copy of the post-copier-header ROM payload.
    pub rom: Vec<u8>,
    /// 128 KiB WRAM at $7E:0000-$7F:FFFF (with low-8K mirror at
    /// $00-$3F:$0000-$1FFF and $80-$BF:$0000-$1FFF).
    pub wram: Vec<u8>,
    master: u64,
    memsel_fast: bool,
    /// Memory data register / open-bus latch. Updated on every
    /// access; reads from unmapped regions return its current value.
    open_bus: u8,
    /// Diagnostic counters - a write to $4200/$420B/$420C/$420D
    /// bumps the matching tally so the headless test runner can
    /// see how far the boot sequence got even before we model the
    /// PPU.
    pub mmio_writes: MmioCounters,
}

/// Per-register write counters. Stubbed MMIO regions always swallow
/// the write; this lets the runner observe boot-sequence progress
/// without a real PPU/DMA/IRQ implementation.
#[derive(Debug, Default, Clone)]
pub struct MmioCounters {
    pub ppu_b_bus: u64,        // $2100-$21FF
    pub apu_ports: u64,        // $2140-$2143 (mirrored to $217F)
    pub cpu_ctrl: u64,         // $4200-$420D
    pub cpu_status: u64,       // $4210-$421F (read-only, but counted)
    pub dma_regs: u64,         // $4300-$437F
    pub joypad_io: u64,        // $4016-$4017 (XSlow region)
    pub stz_to_unmapped: u64,  // unrecognised writes
}

impl LoRomBus {
    pub fn from_cartridge(cart: &Cartridge) -> Self {
        assert!(
            cart.header.map_mode == MapMode::LoRom,
            "LoRomBus: cart is {:?}, only LoRom supported in Phase 2d",
            cart.header.map_mode
        );
        Self::from_rom(cart.rom.clone())
    }

    pub fn from_rom(rom: Vec<u8>) -> Self {
        Self {
            rom,
            wram: vec![0; 128 * 1024],
            master: 0,
            memsel_fast: false,
            open_bus: 0,
            mmio_writes: MmioCounters::default(),
        }
    }

    /// Master-cycle cost of one access at `addr`. Same shape every
    /// SNES emulator uses: WRAM and most cart space cost 8, B-bus
    /// MMIO and FastROM cost 6, the legacy joypad strip costs 12.
    pub fn region_speed(&self, addr: u32) -> u64 {
        let bank = (addr >> 16) as u8;
        let off = (addr & 0xFFFF) as u16;
        match bank {
            0x00..=0x3F | 0x80..=0xBF => match off {
                0x0000..=0x1FFF => SLOW,
                0x2000..=0x3FFF => FAST,
                0x4000..=0x41FF => XSLOW,
                0x4200..=0x5FFF => FAST,
                0x6000..=0x7FFF => SLOW,
                0x8000..=0xFFFF => {
                    if bank >= 0x80 && self.memsel_fast {
                        FAST
                    } else {
                        SLOW
                    }
                }
            },
            0x40..=0x7D => SLOW,
            0x7E..=0x7F => SLOW,
            0xC0..=0xFF => {
                if self.memsel_fast {
                    FAST
                } else {
                    SLOW
                }
            }
        }
    }

    /// Translate a 24-bit CPU address to a flat ROM offset under
    /// LoROM rules. Returns `None` for addresses that don't map to
    /// ROM (e.g., the WRAM bank, MMIO ranges, the $0000-$7FFF half
    /// of cart banks, or addresses past the cart's actual size).
    fn lorom_offset(&self, addr: u32) -> Option<usize> {
        let bank = (addr >> 16) as u8;
        let off = (addr & 0xFFFF) as u16;
        // Strip the FastROM mirror bit so $80-$FD aliases $00-$7D.
        let logical_bank = bank & 0x7F;
        let bank_off = match (logical_bank, off) {
            (0x00..=0x3F, 0x8000..=0xFFFF) => {
                (logical_bank as usize) * 0x8000 + (off as usize - 0x8000)
            }
            (0x40..=0x7D, _) => {
                // LoROM banks $40-$7D: $0000-$7FFF mirrors $8000-$FFFF
                // of the corresponding ROM bank. Treat both halves
                // as the same 32 KiB.
                let logical_off = if off < 0x8000 {
                    off as usize
                } else {
                    off as usize - 0x8000
                };
                ((logical_bank as usize - 0x40) + 0x40) * 0x8000 + logical_off
            }
            _ => return None,
        };
        if bank_off < self.rom.len() {
            Some(bank_off)
        } else {
            None
        }
    }

    fn wram_index(addr: u32) -> Option<usize> {
        let bank = (addr >> 16) as u8;
        let off = (addr & 0xFFFF) as u16;
        match bank {
            0x7E => Some(off as usize),
            0x7F => Some(0x10000 + off as usize),
            0x00..=0x3F | 0x80..=0xBF if off < 0x2000 => Some(off as usize),
            _ => None,
        }
    }

    fn read_internal(&mut self, addr: u32) -> u8 {
        let bank = (addr >> 16) as u8;
        let off = (addr & 0xFFFF) as u16;
        if let Some(i) = Self::wram_index(addr) {
            let v = self.wram[i];
            self.open_bus = v;
            return v;
        }
        match (bank, off) {
            // CPU control / status (read side returns open-bus for
            // the registers we don't model yet; the test ROMs we
            // care about don't read these during the boot prelude).
            (0x00..=0x3F | 0x80..=0xBF, 0x4210..=0x421F) => {
                self.mmio_writes.cpu_status += 1;
                self.open_bus
            }
            (0x00..=0x3F | 0x80..=0xBF, 0x2100..=0x213F) => self.open_bus,
            (0x00..=0x3F | 0x80..=0xBF, 0x2140..=0x217F) => self.open_bus,
            (0x00..=0x3F | 0x80..=0xBF, 0x2180..=0x21FF) => self.open_bus,
            (0x00..=0x3F | 0x80..=0xBF, 0x4200..=0x420F) => self.open_bus,
            (0x00..=0x3F | 0x80..=0xBF, 0x4300..=0x437F) => self.open_bus,
            (0x00..=0x3F | 0x80..=0xBF, 0x4016..=0x4017) => self.open_bus,
            _ => match self.lorom_offset(addr) {
                Some(o) => {
                    let v = self.rom[o];
                    self.open_bus = v;
                    v
                }
                None => self.open_bus,
            },
        }
    }

    fn write_internal(&mut self, addr: u32, value: u8) {
        let bank = (addr >> 16) as u8;
        let off = (addr & 0xFFFF) as u16;
        self.open_bus = value;
        if let Some(i) = Self::wram_index(addr) {
            self.wram[i] = value;
            return;
        }
        match (bank, off) {
            (0x00..=0x3F | 0x80..=0xBF, 0x2100..=0x213F) => {
                self.mmio_writes.ppu_b_bus += 1;
            }
            (0x00..=0x3F | 0x80..=0xBF, 0x2140..=0x217F) => {
                self.mmio_writes.apu_ports += 1;
            }
            (0x00..=0x3F | 0x80..=0xBF, 0x2180..=0x21FF) => {
                self.mmio_writes.ppu_b_bus += 1;
            }
            (0x00..=0x3F | 0x80..=0xBF, 0x4200..=0x420C) => {
                self.mmio_writes.cpu_ctrl += 1;
            }
            (0x00..=0x3F | 0x80..=0xBF, 0x420D) => {
                self.mmio_writes.cpu_ctrl += 1;
                self.memsel_fast = value & 1 != 0;
            }
            (0x00..=0x3F | 0x80..=0xBF, 0x4300..=0x437F) => {
                self.mmio_writes.dma_regs += 1;
            }
            (0x00..=0x3F | 0x80..=0xBF, 0x4016..=0x4017) => {
                self.mmio_writes.joypad_io += 1;
            }
            _ => {
                self.mmio_writes.stz_to_unmapped += 1;
            }
        }
    }

    pub fn memsel_fast(&self) -> bool {
        self.memsel_fast
    }

    pub fn open_bus(&self) -> u8 {
        self.open_bus
    }
}

impl SnesBus for LoRomBus {
    fn read(&mut self, addr: u32) -> u8 {
        let speed = self.region_speed(addr);
        self.master = self.master.wrapping_add(speed);
        self.read_internal(addr)
    }

    fn write(&mut self, addr: u32, value: u8) {
        let speed = self.region_speed(addr);
        self.master = self.master.wrapping_add(speed);
        self.write_internal(addr, value);
    }

    fn idle(&mut self) {
        self.master = self.master.wrapping_add(FAST);
    }

    fn master_cycles(&self) -> u64 {
        self.master
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill_rom() -> Vec<u8> {
        let mut rom = vec![0; 0x8000];
        // Reset vector at $7FFC -> $8000, fetched at $00:FFFC-D
        rom[0x7FFC] = 0x00;
        rom[0x7FFD] = 0x80;
        rom[0x0000] = 0xEA; // NOP at the reset target
        rom
    }

    #[test]
    fn lorom_reset_vector_visible_at_00ffc_in_emulation() {
        let mut bus = LoRomBus::from_rom(fill_rom());
        assert_eq!(bus.read(0x00FFFC), 0x00);
        assert_eq!(bus.read(0x00FFFD), 0x80);
        assert_eq!(bus.read(0x008000), 0xEA);
    }

    #[test]
    fn wram_low_mirror_round_trips() {
        let mut bus = LoRomBus::from_rom(fill_rom());
        bus.write(0x000400, 0xAB);
        assert_eq!(bus.read(0x7E0400), 0xAB);
        // $80-$BF half mirrors the same low-WRAM window
        bus.write(0x800500, 0xCD);
        assert_eq!(bus.read(0x7E0500), 0xCD);
    }

    #[test]
    fn full_wram_visible_in_7e_7f() {
        let mut bus = LoRomBus::from_rom(fill_rom());
        bus.write(0x7F1234, 0x77);
        assert_eq!(bus.read(0x7F1234), 0x77);
    }

    #[test]
    fn region_speed_picks_fast_xslow_slow() {
        let bus = LoRomBus::from_rom(fill_rom());
        assert_eq!(bus.region_speed(0x000000), SLOW);
        assert_eq!(bus.region_speed(0x002100), FAST);
        assert_eq!(bus.region_speed(0x004016), XSLOW);
        assert_eq!(bus.region_speed(0x004200), FAST);
        assert_eq!(bus.region_speed(0x008000), SLOW);
        assert_eq!(bus.region_speed(0x808000), SLOW); // memsel still 0
    }

    #[test]
    fn memsel_flips_high_bank_to_fast() {
        let mut bus = LoRomBus::from_rom(fill_rom());
        bus.write(0x00420D, 0x01); // MEMSEL = FastROM
        assert!(bus.memsel_fast());
        assert_eq!(bus.region_speed(0x808000), FAST);
        assert_eq!(bus.region_speed(0xC08000), FAST);
        // Banks $00-$7D are unaffected.
        assert_eq!(bus.region_speed(0x008000), SLOW);
    }

    #[test]
    fn mmio_swallows_writes_and_counts() {
        let mut bus = LoRomBus::from_rom(fill_rom());
        bus.write(0x002100, 0x80); // INIDISP force-blank
        bus.write(0x004200, 0x81); // NMITIMEN
        bus.write(0x004310, 0x09); // DMA channel 1 control
        assert_eq!(bus.mmio_writes.ppu_b_bus, 1);
        assert_eq!(bus.mmio_writes.cpu_ctrl, 1);
        assert_eq!(bus.mmio_writes.dma_regs, 1);
    }
}
