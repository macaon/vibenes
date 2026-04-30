// SPDX-License-Identifier: GPL-3.0-or-later
//! Bandai LZ93D50 + 8 KiB battery-backed SRAM (iNES mapper 153).
//!
//! Same chip as the LZ93D50 we handle in [`bandai_fcg.rs`][bandai_fcg]
//! for mappers 16 / 159 - but this variant swaps the on-chip 24C0X
//! serial EEPROM out for an 8 KiB external SRAM at `$6000-$7FFF`,
//! and re-purposes a couple of the chip's pins to extend the PRG
//! bank by one bit (so the cart can address 512 KiB).
//!
//! Used by exactly one licensed cart: *Famicom Jump II: Saikyou
//! no Shichinin* (Bandai 1991), 512 KiB PRG + 8 KiB CHR-RAM +
//! 8 KiB battery SRAM.
//!
//! ## What's different from mappers 16 / 159
//!
//! 1. **8 KiB battery-backed PRG-RAM** sits at `$6000-$7FFF` -
//!    the FCG register file no longer mirrors there (writes/reads
//!    in that window are plain RAM). Access is gated by `$x00D`
//!    bit 5 (RAM enable). Mesen2 unifies this with the EEPROM-
//!    enable bit on mapper 16; here we treat the gate as a pure
//!    read/write enable.
//! 2. **Outer PRG bank bit.** Each of the 8 CHR-bank registers
//!    ($x000-$x007) carries bit 0 = PRG bank bit 4. Bit 0 of
//!    every CHR-bank register is OR'd together onto the same
//!    physical line on the chip - so writing any non-zero bit 0
//!    to any CHR register sets the PRG outer to 1; clearing all
//!    of them sets it to 0. The cart uses this to switch between
//!    the upper and lower 256 KiB halves.
//! 3. **CHR-RAM only.** Mapper 16's CHR registers normally drive
//!    a CHR-ROM bus; here CHR is plain RAM and the registers
//!    only contribute their bit 0 to the PRG outer.
//!
//! Everything else (mirroring control at `$x009`, IRQ counter
//! latch at `$x00B`/`$x00C`, IRQ enable + reload at `$x00A` bit 0)
//! follows the LZ93D50 model unchanged.
//!
//! ## Register surface (mirrored across `$8000-$FFFF`)
//!
//! ```text
//! $x000-$x007   ......E.  E -> bit 0 contributes to PRG outer (bit 4)
//! $x008         .....PPP  P -> 16 KiB PRG bank for $8000-$BFFF (bits 3-0)
//! $x009         ......MM  MM -> mirroring (00 V, 01 H, 10 A, 11 B)
//! $x00A         .......E  E -> IRQ enable + counter reload from latch
//! $x00B         LLLL LLLL latch low byte (also reloads counter on $x00A)
//! $x00C         LLLL LLLL latch high byte
//! $x00D         ..R.....  R -> SRAM enable (1 = $6000-$7FFF readable/writable)
//! ```
//!
//! `$C000-$FFFF` is hardwired to **the last bank of the currently
//! selected 256 KiB half** (i.e. `0x0F | (outer << 4)`).
//!
//! References:
//! - <https://www.nesdev.org/wiki/INES_Mapper_153>
//! - `~/Git/Mesen2/Core/NES/Mappers/Bandai/BandaiFcg.h` (the
//!   mapper-153-specific branches)
//! - `~/Git/punes/src/core/mappers/mapper_153.c`
//!
//! [bandai_fcg]: super::bandai_fcg

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_16K: usize = 16 * 1024;
const CHR_RAM_SIZE: usize = 8 * 1024;
const PRG_RAM_SIZE: usize = 8 * 1024;

pub struct BandaiLz93d50Sram {
    prg_rom: Vec<u8>,
    chr_ram: Vec<u8>,
    /// 8 KiB battery-backed SRAM at `$6000-$7FFF`. `prg_ram_enabled`
    /// gates both reads and writes from the CPU.
    prg_ram: Vec<u8>,

    /// Each CHR-bank register's last-written byte. Only bit 0 of
    /// each is read out - the rest is captured in the snapshot
    /// because real software writes a full byte and snapshots
    /// must round-trip it.
    chr_regs: [u8; 8],
    /// Inner PRG bank - low 4 bits, written via `$x008`.
    prg_page: u8,
    /// Outer PRG bank bit (0 or 1). Recomputed from
    /// `OR(chr_regs[i] & 1)` on every CHR-register write.
    prg_outer: u8,
    /// Live mirroring derived from `$x009`.
    mirroring: Mirroring,

    /// IRQ down-counter (clocks once per CPU cycle while enabled).
    irq_counter: u16,
    /// Reload latch loaded by `$x00B` / `$x00C`. Copied to the
    /// counter on `$x00A` write.
    irq_reload: u16,
    irq_enabled: bool,
    irq_line: bool,

    /// `$x00D` bit 5 - SRAM read/write enable.
    prg_ram_enabled: bool,

    prg_bank_count_16k: usize,
    save_dirty: bool,
}

impl BandaiLz93d50Sram {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count_16k = (cart.prg_rom.len() / PRG_BANK_16K).max(1);
        // Battery SRAM size: prefer the cart-declared NVRAM size,
        // fall back to RAM, fall back to 8 KiB minimum (the spec
        // size for this mapper).
        let prg_ram_total = (cart.prg_ram_size + cart.prg_nvram_size).max(PRG_RAM_SIZE);
        Self {
            prg_rom: cart.prg_rom,
            chr_ram: vec![0u8; CHR_RAM_SIZE],
            prg_ram: vec![0u8; prg_ram_total],
            chr_regs: [0; 8],
            prg_page: 0,
            prg_outer: 0,
            mirroring: cart.mirroring,
            irq_counter: 0,
            irq_reload: 0,
            irq_enabled: false,
            irq_line: false,
            // SRAM defaults to enabled. Famicom Jump II writes
            // `$x00D` bit 5 = 1 early in boot, but defaulting to
            // enabled lets carts that never write it (or test
            // ROMs) still see the RAM.
            prg_ram_enabled: true,
            prg_bank_count_16k,
            save_dirty: false,
        }
    }

    fn recompute_outer(&mut self) {
        // Real chip wires bit 0 of all 8 CHR-bank pins together.
        // OR'ing them gives a single bit; shift to bit 4 of the
        // resulting PRG bank.
        let mut bit = 0u8;
        for r in self.chr_regs {
            bit |= r & 0x01;
        }
        self.prg_outer = bit;
    }

    fn switch_bank_index(&self) -> usize {
        let bank = (self.prg_page as usize) | ((self.prg_outer as usize) << 4);
        bank % self.prg_bank_count_16k
    }

    fn fixed_bank_index(&self) -> usize {
        // Last bank of the currently-selected 256 KiB half.
        let bank = 0x0F | ((self.prg_outer as usize) << 4);
        bank % self.prg_bank_count_16k
    }
}

impl Mapper for BandaiLz93d50Sram {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => {
                if self.prg_ram_enabled {
                    let i = (addr - 0x6000) as usize;
                    *self.prg_ram.get(i).unwrap_or(&0)
                } else {
                    // Open bus when SRAM is gated off.
                    0
                }
            }
            0x8000..=0xBFFF => {
                let i = self.switch_bank_index() * PRG_BANK_16K + (addr - 0x8000) as usize;
                *self.prg_rom.get(i).unwrap_or(&0)
            }
            0xC000..=0xFFFF => {
                let i = self.fixed_bank_index() * PRG_BANK_16K + (addr - 0xC000) as usize;
                *self.prg_rom.get(i).unwrap_or(&0)
            }
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        match addr {
            0x6000..=0x7FFF => {
                if self.prg_ram_enabled {
                    let i = (addr - 0x6000) as usize;
                    if let Some(slot) = self.prg_ram.get_mut(i) {
                        if *slot != data {
                            *slot = data;
                            self.save_dirty = true;
                        }
                    }
                }
            }
            0x8000..=0xFFFF => {
                // Registers mirror across $8000-$FFFF. Address
                // bits 0-3 select the register.
                match addr & 0x000F {
                    0x0..=0x7 => {
                        let i = (addr & 0x07) as usize;
                        self.chr_regs[i] = data;
                        self.recompute_outer();
                    }
                    0x8 => {
                        self.prg_page = data & 0x0F;
                    }
                    0x9 => {
                        self.mirroring = match data & 0x03 {
                            0 => Mirroring::Vertical,
                            1 => Mirroring::Horizontal,
                            2 => Mirroring::SingleScreenLower,
                            _ => Mirroring::SingleScreenUpper,
                        };
                    }
                    0xA => {
                        // Enable + reload from latch (LZ93D50 model).
                        self.irq_enabled = data & 0x01 != 0;
                        self.irq_counter = self.irq_reload;
                        self.irq_line = false;
                    }
                    0xB => {
                        self.irq_reload = (self.irq_reload & 0xFF00) | data as u16;
                    }
                    0xC => {
                        self.irq_reload = (self.irq_reload & 0x00FF) | ((data as u16) << 8);
                    }
                    0xD => {
                        // Bit 5: SRAM enable.
                        self.prg_ram_enabled = data & 0x20 != 0;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr < 0x2000 {
            *self.chr_ram.get(addr as usize).unwrap_or(&0)
        } else {
            0
        }
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if addr < 0x2000 {
            if let Some(slot) = self.chr_ram.get_mut(addr as usize) {
                *slot = data;
            }
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }

    fn on_cpu_cycle(&mut self) {
        if self.irq_enabled {
            // Mesen2's documented quirk: check counter == 0 BEFORE
            // decrementing - this is the only model that gets both
            // *Famicom Jump II* and *Magical Taruruuto-kun 2* right
            // simultaneously.
            if self.irq_counter == 0 {
                self.irq_line = true;
            }
            self.irq_counter = self.irq_counter.wrapping_sub(1);
        }
    }

    fn irq_line(&self) -> bool {
        self.irq_line
    }

    fn save_data(&self) -> Option<&[u8]> {
        Some(self.prg_ram.as_slice())
    }

    fn load_save_data(&mut self, data: &[u8]) {
        if data.len() == self.prg_ram.len() {
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
        use crate::save_state::mapper::{BandaiLz93d50SramSnap, MirroringSnap};
        Some(crate::save_state::MapperState::BandaiLz93d50Sram(Box::new(
            BandaiLz93d50SramSnap {
                prg_ram: self.prg_ram.clone(),
                chr_ram_data: self.chr_ram.clone(),
                chr_regs: self.chr_regs,
                prg_page: self.prg_page,
                prg_outer: self.prg_outer,
                mirroring: MirroringSnap::from_live(self.mirroring),
                irq_counter: self.irq_counter,
                irq_reload: self.irq_reload,
                irq_enabled: self.irq_enabled,
                irq_line: self.irq_line,
                prg_ram_enabled: self.prg_ram_enabled,
                save_dirty: self.save_dirty,
            },
        )))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::BandaiLz93d50Sram(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        if snap.prg_ram.len() == self.prg_ram.len() {
            self.prg_ram.copy_from_slice(&snap.prg_ram);
        }
        if snap.chr_ram_data.len() == self.chr_ram.len() {
            self.chr_ram.copy_from_slice(&snap.chr_ram_data);
        }
        self.chr_regs = snap.chr_regs;
        self.prg_page = snap.prg_page;
        self.prg_outer = snap.prg_outer;
        self.mirroring = snap.mirroring.to_live();
        self.irq_counter = snap.irq_counter;
        self.irq_reload = snap.irq_reload;
        self.irq_enabled = snap.irq_enabled;
        self.irq_line = snap.irq_line;
        self.prg_ram_enabled = snap.prg_ram_enabled;
        self.save_dirty = snap.save_dirty;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, Mirroring, TvSystem};

    /// 512 KiB PRG (32 banks of 16 KiB) + CHR-RAM. Each bank's
    /// first byte tagged with its index for read-back.
    fn cart() -> Cartridge {
        let mut prg = vec![0xFFu8; 32 * PRG_BANK_16K];
        for bank in 0..32 {
            prg[bank * PRG_BANK_16K] = bank as u8;
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: Vec::new(),
            chr_ram: true,
            mapper_id: 153,
            submapper: 0,
            mirroring: Mirroring::Vertical,
            battery_backed: true,
            prg_ram_size: 0,
            prg_nvram_size: 0x2000,
            tv_system: TvSystem::Ntsc,
            is_nes2: true,
            prg_chr_crc32: 0,
            db_matched: false,
            fds_data: None,
        }
    }

    #[test]
    fn boot_state_lower_half_first_bank_at_8000() {
        let m = BandaiLz93d50Sram::new(cart());
        assert_eq!(m.cpu_peek(0x8000), 0); // outer 0, page 0
        assert_eq!(m.cpu_peek(0xC000), 15); // last of lower half
    }

    #[test]
    fn x008_writes_set_inner_prg_page() {
        let mut m = BandaiLz93d50Sram::new(cart());
        m.cpu_write(0x8008, 0x05); // PRG page 5
        assert_eq!(m.cpu_peek(0x8000), 5);
        m.cpu_write(0xE008, 0x0F); // mirrored register; page 15
        assert_eq!(m.cpu_peek(0x8000), 15);
        // Bits 7-4 of the value are ignored.
        m.cpu_write(0x8008, 0xFC); // page 12
        assert_eq!(m.cpu_peek(0x8000), 12);
    }

    #[test]
    fn chr_register_bit0_drives_prg_outer() {
        let mut m = BandaiLz93d50Sram::new(cart());
        m.cpu_write(0x8008, 0x03); // page 3
        // No outer set yet -> bank 3.
        assert_eq!(m.cpu_peek(0x8000), 3);
        // Write bit 0 to any CHR reg -> outer 1 -> bank 0x10|3 = 19.
        m.cpu_write(0x8000, 0x01);
        assert_eq!(m.cpu_peek(0x8000), 19);
        // Last bank for upper half = 0x10 | 0x0F = 31.
        assert_eq!(m.cpu_peek(0xC000), 31);
        // Clearing bit 0 of *every* CHR reg drops the outer.
        for r in 0..8 {
            m.cpu_write(0x8000 + r, 0x00);
        }
        assert_eq!(m.cpu_peek(0x8000), 3);
        assert_eq!(m.cpu_peek(0xC000), 15);
        // Outer remains set if any CHR reg still has bit 0.
        m.cpu_write(0x8003, 0x01);
        m.cpu_write(0x8005, 0x01);
        assert_eq!(m.cpu_peek(0x8000), 19);
        m.cpu_write(0x8003, 0x00);
        // $8005 still has bit 0 -> outer still 1.
        assert_eq!(m.cpu_peek(0x8000), 19);
        m.cpu_write(0x8005, 0x00);
        // Now outer drops to 0.
        assert_eq!(m.cpu_peek(0x8000), 3);
    }

    #[test]
    fn x009_drives_mirroring() {
        let mut m = BandaiLz93d50Sram::new(cart());
        m.cpu_write(0x8009, 0x00);
        assert_eq!(m.mirroring(), Mirroring::Vertical);
        m.cpu_write(0x8009, 0x01);
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
        m.cpu_write(0x8009, 0x02);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenLower);
        m.cpu_write(0x8009, 0x03);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenUpper);
    }

    #[test]
    fn irq_counter_reload_and_fire() {
        let mut m = BandaiLz93d50Sram::new(cart());
        // Latch low + high.
        m.cpu_write(0x800B, 0x05);
        m.cpu_write(0x800C, 0x00);
        // Enable + reload.
        m.cpu_write(0x800A, 0x01);
        assert_eq!(m.irq_counter, 5);
        // Tick down. Counter pre-check fires when the cycle starts
        // with counter == 0, then decrements. With reload=5, the
        // sequence is: cycle 1 sees 5, ..., cycle 6 sees 0 -> fire.
        for _ in 0..5 {
            assert!(!m.irq_line());
            m.on_cpu_cycle();
        }
        // Counter is 0 going into the next cycle.
        assert!(!m.irq_line());
        m.on_cpu_cycle();
        assert!(m.irq_line());
        // Disable + counter clears line on next $x00A write.
        m.cpu_write(0x800A, 0x00);
        assert!(!m.irq_line());
    }

    #[test]
    fn x00d_gates_sram_access() {
        let mut m = BandaiLz93d50Sram::new(cart());
        // Default enabled: write/read.
        m.cpu_write(0x6100, 0xAB);
        assert_eq!(m.cpu_peek(0x6100), 0xAB);
        // Disable.
        m.cpu_write(0x800D, 0x00);
        assert_eq!(m.cpu_peek(0x6100), 0); // open bus
        m.cpu_write(0x6100, 0xCD); // dropped
        m.cpu_write(0x800D, 0x20); // re-enable
        assert_eq!(m.cpu_peek(0x6100), 0xAB); // original byte still there
    }

    #[test]
    fn save_data_round_trips_battery_sram() {
        let mut m = BandaiLz93d50Sram::new(cart());
        m.cpu_write(0x6000, 0x42);
        m.cpu_write(0x7FFF, 0x99);
        assert!(m.save_dirty());
        let saved = m.save_data().unwrap().to_vec();
        let mut fresh = BandaiLz93d50Sram::new(cart());
        fresh.load_save_data(&saved);
        assert_eq!(fresh.cpu_peek(0x6000), 0x42);
        assert_eq!(fresh.cpu_peek(0x7FFF), 0x99);
    }

    #[test]
    fn save_state_round_trip() {
        let mut m = BandaiLz93d50Sram::new(cart());
        m.cpu_write(0x8008, 0x07); // page 7
        m.cpu_write(0x8003, 0x01); // outer 1
        m.cpu_write(0x8009, 0x01); // horizontal
        m.cpu_write(0x800B, 0x10);
        m.cpu_write(0x800C, 0x20);
        m.cpu_write(0x800A, 0x01); // enable IRQ, reload to 0x2010
        m.cpu_write(0x6000, 0xAA);
        m.ppu_write(0x0042, 0xBB);
        let snap = m.save_state_capture().unwrap();
        let mut fresh = BandaiLz93d50Sram::new(cart());
        fresh.save_state_apply(&snap).unwrap();
        assert_eq!(fresh.cpu_peek(0x8000), 23); // 0x10 | 0x07
        assert_eq!(fresh.mirroring(), Mirroring::Horizontal);
        assert_eq!(fresh.irq_counter, 0x2010);
        assert!(fresh.irq_enabled);
        assert_eq!(fresh.cpu_peek(0x6000), 0xAA);
        assert_eq!(fresh.ppu_read(0x0042), 0xBB);
    }
}
