// SPDX-License-Identifier: GPL-3.0-or-later
//! Irem H3001 - iNES mapper 65.
//!
//! Used by *Spartan X 2* (Famicom-only sequel to *Kung-Fu Master*),
//! *Daiku no Gen-san 2: Akage no Dan no Gyakushuu*, and *Kaiketsu
//! Yanchamaru 3: Taiketsu! Zouringen*. PRG is split into three
//! switchable 8 KiB banks plus a hardwired last bank; CHR is 8 × 1
//! KiB. The chip's signature feature is a 16-bit CPU-cycle IRQ
//! down-counter, distinct from MMC3's PPU-A12 model.
//!
//! ## Register map
//!
//! | Address | Effect                                        |
//! |---------|-----------------------------------------------|
//! | `$8000` | PRG slot 0 bank (8 KiB at `$8000-$9FFF`)      |
//! | `$9001` | bit 7: mirroring (0 = vertical, 1 = horiz.)   |
//! | `$9003` | bit 7: IRQ enable; write also acks pending    |
//! | `$9004` | reload counter from latch; write also acks    |
//! | `$9005` | IRQ latch high byte                           |
//! | `$9006` | IRQ latch low byte                            |
//! | `$A000` | PRG slot 1 bank (`$A000-$BFFF`)               |
//! | `$B000` | CHR bank 0 (1 KiB at PPU `$0000-$03FF`)       |
//! | `$B001` | CHR bank 1 (`$0400-$07FF`)                    |
//! | `$B002` | CHR bank 2 (`$0800-$0BFF`)                    |
//! | `$B003` | CHR bank 3 (`$0C00-$0FFF`)                    |
//! | `$B004` | CHR bank 4 (`$1000-$13FF`)                    |
//! | `$B005` | CHR bank 5 (`$1400-$17FF`)                    |
//! | `$B006` | CHR bank 6 (`$1800-$1BFF`)                    |
//! | `$B007` | CHR bank 7 (`$1C00-$1FFF`)                    |
//! | `$C000` | PRG slot 2 bank (`$C000-$DFFF`)               |
//!
//! Slot 3 (`$E000-$FFFF`) is hardwired to the last 8 KiB bank.
//!
//! ## IRQ
//!
//! 16-bit down-counter ticking every CPU cycle while enabled. On
//! transitioning from 1 to 0 it raises /IRQ and self-disables (so
//! `$9003` re-arming is required for a follow-up trigger). The line
//! stays asserted until a write to `$9003` or `$9004` acknowledges
//! it.
//!
//! References:
//! - <https://www.nesdev.org/wiki/INES_Mapper_065>
//! - `~/Git/Mesen2/Core/NES/Mappers/Irem/IremH3001.h`
//! - `~/Git/nestopia/source/core/board/NstBoardIremH3001.cpp`
//! - `~/Git/punes/src/core/mappers/mapper_065.c`

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_8K: usize = 8 * 1024;
const CHR_BANK_1K: usize = 1024;
const PRG_RAM_SIZE: usize = 8 * 1024;

pub struct IremH3001 {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,
    prg_ram: Vec<u8>,

    /// PRG bank registers for slots 0/1/2. Slot 3 is fixed to the
    /// last bank.
    prg_regs: [u8; 3],
    /// 1 KiB CHR bank for each of the eight PPU windows.
    chr_regs: [u8; 8],

    mirroring: Mirroring,

    irq_enabled: bool,
    irq_counter: u16,
    irq_latch: u16,
    irq_line: bool,

    prg_bank_mask: usize,
    chr_bank_mask: usize,

    battery: bool,
    save_dirty: bool,
}

impl IremH3001 {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count = (cart.prg_rom.len() / PRG_BANK_8K).max(1);
        debug_assert!(prg_bank_count.is_power_of_two());
        let prg_bank_mask = prg_bank_count - 1;

        let is_chr_ram = cart.chr_ram || cart.chr_rom.is_empty();
        let chr = if is_chr_ram {
            vec![0u8; 8 * CHR_BANK_1K]
        } else {
            cart.chr_rom
        };
        let chr_bank_count = (chr.len() / CHR_BANK_1K).max(1);
        debug_assert!(chr_bank_count.is_power_of_two());
        let chr_bank_mask = chr_bank_count - 1;

        let prg_ram_total =
            (cart.prg_ram_size + cart.prg_nvram_size).max(PRG_RAM_SIZE);

        Self {
            prg_rom: cart.prg_rom,
            chr,
            chr_ram: is_chr_ram,
            prg_ram: vec![0u8; prg_ram_total],
            prg_regs: [0, 1, 0],
            chr_regs: [0; 8],
            mirroring: cart.mirroring,
            irq_enabled: false,
            irq_counter: 0,
            irq_latch: 0,
            irq_line: false,
            prg_bank_mask,
            chr_bank_mask,
            battery: cart.battery_backed,
            save_dirty: false,
        }
    }

    fn prg_slot_base(&self, slot: u8) -> usize {
        let bank = match slot {
            0 => self.prg_regs[0] as usize,
            1 => self.prg_regs[1] as usize,
            2 => self.prg_regs[2] as usize,
            3 => self.prg_bank_mask,
            _ => unreachable!(),
        };
        (bank & self.prg_bank_mask) * PRG_BANK_8K
    }

    fn chr_slot_base(&self, slot: usize) -> usize {
        (self.chr_regs[slot] as usize & self.chr_bank_mask) * CHR_BANK_1K
    }
}

impl Mapper for IremH3001 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => {
                let i = (addr - 0x6000) as usize;
                *self.prg_ram.get(i).unwrap_or(&0)
            }
            0x8000..=0xFFFF => {
                let slot = ((addr - 0x8000) >> 13) as u8;
                let off = (addr & 0x1FFF) as usize;
                let base = self.prg_slot_base(slot);
                *self.prg_rom.get(base + off).unwrap_or(&0)
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
            0x8000..=0xFFFF => match addr {
                0x8000 => self.prg_regs[0] = data,
                0xA000 => self.prg_regs[1] = data,
                0xC000 => self.prg_regs[2] = data,
                0x9001 => {
                    self.mirroring = if data & 0x80 != 0 {
                        Mirroring::Horizontal
                    } else {
                        Mirroring::Vertical
                    };
                }
                0x9003 => {
                    self.irq_enabled = data & 0x80 != 0;
                    self.irq_line = false;
                }
                0x9004 => {
                    self.irq_counter = self.irq_latch;
                    self.irq_line = false;
                }
                0x9005 => {
                    self.irq_latch =
                        (self.irq_latch & 0x00FF) | ((data as u16) << 8);
                }
                0x9006 => {
                    self.irq_latch = (self.irq_latch & 0xFF00) | (data as u16);
                }
                0xB000..=0xB007 => {
                    let i = (addr & 0x0007) as usize;
                    self.chr_regs[i] = data;
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr < 0x2000 {
            let slot = (addr >> 10) as usize & 0x07;
            let off = (addr & 0x03FF) as usize;
            let base = self.chr_slot_base(slot);
            *self.chr.get(base + off).unwrap_or(&0)
        } else {
            0
        }
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if self.chr_ram && addr < 0x2000 {
            let slot = (addr >> 10) as usize & 0x07;
            let off = (addr & 0x03FF) as usize;
            let base = self.chr_slot_base(slot);
            if let Some(b) = self.chr.get_mut(base + off) {
                *b = data;
            }
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }

    fn on_cpu_cycle(&mut self) {
        if self.irq_enabled && self.irq_counter != 0 {
            self.irq_counter -= 1;
            if self.irq_counter == 0 {
                self.irq_enabled = false;
                self.irq_line = true;
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

    fn save_state_capture(&self) -> Option<crate::save_state::MapperState> {
        use crate::save_state::mapper::{IremH3001Snap, MirroringSnap};
        Some(crate::save_state::MapperState::IremH3001(Box::new(IremH3001Snap {
            prg_ram: self.prg_ram.clone(),
            chr_ram_data: if self.chr_ram { self.chr.clone() } else { Vec::new() },
            prg_regs: self.prg_regs,
            chr_regs: self.chr_regs,
            mirroring: MirroringSnap::from_live(self.mirroring),
            irq_enabled: self.irq_enabled,
            irq_counter: self.irq_counter,
            irq_latch: self.irq_latch,
            irq_line: self.irq_line,
            save_dirty: self.save_dirty,
        })))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::IremH3001(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        if snap.prg_ram.len() == self.prg_ram.len() {
            self.prg_ram.copy_from_slice(&snap.prg_ram);
        }
        if self.chr_ram && snap.chr_ram_data.len() == self.chr.len() {
            self.chr.copy_from_slice(&snap.chr_ram_data);
        }
        self.prg_regs = snap.prg_regs;
        self.chr_regs = snap.chr_regs;
        self.mirroring = snap.mirroring.to_live();
        self.irq_enabled = snap.irq_enabled;
        self.irq_counter = snap.irq_counter;
        self.irq_latch = snap.irq_latch;
        self.irq_line = snap.irq_line;
        self.save_dirty = snap.save_dirty;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, Mirroring, TvSystem};

    /// 256 KiB PRG (32 banks of 8 KiB), 256 KiB CHR (256 banks of 1
    /// KiB). Each bank filled with its own tag so tests can identify
    /// which bank is mapped by reading any byte in its window.
    fn cart() -> Cartridge {
        let mut prg = vec![0u8; 32 * PRG_BANK_8K];
        for bank in 0..32 {
            let base = bank * PRG_BANK_8K;
            prg[base..base + PRG_BANK_8K].fill(bank as u8);
        }
        let mut chr = vec![0u8; 256 * CHR_BANK_1K];
        for bank in 0..256 {
            let base = bank * CHR_BANK_1K;
            chr[base..base + CHR_BANK_1K].fill(bank as u8);
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: chr,
            chr_ram: false,
            mapper_id: 65,
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
    fn power_on_layout_has_first_two_banks_then_zero_then_last() {
        let m = IremH3001::new(cart());
        // prg_regs init = [0, 1, 0]; slot 3 = last (31).
        assert_eq!(m.cpu_peek(0x8000), 0);
        assert_eq!(m.cpu_peek(0xA000), 1);
        assert_eq!(m.cpu_peek(0xC000), 0);
        assert_eq!(m.cpu_peek(0xE000), 31);
        assert_eq!(m.cpu_peek(0xFFFF), 31);
    }

    #[test]
    fn prg_slot_writes_take_effect() {
        let mut m = IremH3001::new(cart());
        m.cpu_write(0x8000, 0x05);
        m.cpu_write(0xA000, 0x09);
        m.cpu_write(0xC000, 0x10);
        assert_eq!(m.cpu_peek(0x8000), 5);
        assert_eq!(m.cpu_peek(0xA000), 9);
        assert_eq!(m.cpu_peek(0xC000), 16);
        // Slot 3 still locked to last.
        assert_eq!(m.cpu_peek(0xE000), 31);
    }

    #[test]
    fn chr_regs_select_each_1k_slot_independently() {
        let mut m = IremH3001::new(cart());
        for i in 0..8u8 {
            m.cpu_write(0xB000 | u16::from(i), 0x10 | i);
        }
        for slot in 0..8 {
            let addr = (slot as u16) * 0x0400;
            assert_eq!(m.ppu_read(addr), 0x10 + slot as u8);
        }
    }

    #[test]
    fn mirroring_bit_7_toggles_v_and_h() {
        let mut m = IremH3001::new(cart());
        m.cpu_write(0x9001, 0x00);
        assert_eq!(m.mirroring(), Mirroring::Vertical);
        m.cpu_write(0x9001, 0x80);
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
        m.cpu_write(0x9001, 0x00);
        assert_eq!(m.mirroring(), Mirroring::Vertical);
    }

    #[test]
    fn irq_fires_after_latch_count_cycles() {
        let mut m = IremH3001::new(cart());
        // Latch = 100, reload, enable.
        m.cpu_write(0x9005, 0x00); // high
        m.cpu_write(0x9006, 100); // low
        m.cpu_write(0x9004, 0); // reload from latch
        m.cpu_write(0x9003, 0x80); // enable
        for _ in 0..99 {
            m.on_cpu_cycle();
            assert!(!m.irq_line());
        }
        m.on_cpu_cycle();
        assert!(m.irq_line(), "IRQ should fire on the 100th cycle");
    }

    #[test]
    fn writing_9003_or_9004_acknowledges_irq() {
        let mut m = IremH3001::new(cart());
        m.cpu_write(0x9006, 1);
        m.cpu_write(0x9004, 0);
        m.cpu_write(0x9003, 0x80);
        m.on_cpu_cycle();
        assert!(m.irq_line());
        m.cpu_write(0x9003, 0x00); // disable + ack
        assert!(!m.irq_line());
        // Re-arm and ack via $9004.
        m.cpu_write(0x9006, 1);
        m.cpu_write(0x9004, 0);
        m.cpu_write(0x9003, 0x80);
        m.on_cpu_cycle();
        assert!(m.irq_line());
        m.cpu_write(0x9004, 0); // reload + ack
        assert!(!m.irq_line());
    }

    #[test]
    fn irq_self_disables_after_firing() {
        let mut m = IremH3001::new(cart());
        m.cpu_write(0x9006, 1);
        m.cpu_write(0x9004, 0);
        m.cpu_write(0x9003, 0x80);
        m.on_cpu_cycle();
        assert!(m.irq_line());
        // Counter shouldn't keep ticking - run many cycles, no double-fire.
        for _ in 0..1000 {
            m.on_cpu_cycle();
        }
        // Ack the line; without a re-arm + reload, no new IRQ.
        m.cpu_write(0x9003, 0x80); // ack but stays enabled
        assert!(!m.irq_line());
        for _ in 0..1000 {
            m.on_cpu_cycle();
        }
        assert!(!m.irq_line());
    }

    #[test]
    fn irq_latch_split_high_low_writes() {
        let mut m = IremH3001::new(cart());
        m.cpu_write(0x9005, 0x12); // high
        m.cpu_write(0x9006, 0x34); // low → latch = $1234
        m.cpu_write(0x9004, 0); // reload
        m.cpu_write(0x9003, 0x80); // enable
        // Tick $1233 cycles - no fire yet.
        for _ in 0..0x1233 {
            m.on_cpu_cycle();
        }
        assert!(!m.irq_line());
        m.on_cpu_cycle();
        assert!(m.irq_line());
    }

    #[test]
    fn disabled_counter_does_not_tick() {
        let mut m = IremH3001::new(cart());
        m.cpu_write(0x9006, 5);
        m.cpu_write(0x9004, 0);
        // Don't enable - line should never assert.
        for _ in 0..100 {
            m.on_cpu_cycle();
        }
        assert!(!m.irq_line());
    }

    #[test]
    fn cpu_write_register_decode_is_strict() {
        // H3001 decodes specific addresses, NOT mirrors. $8001 is a
        // no-op (PRG slot 0 only fires on $8000 itself).
        let mut m = IremH3001::new(cart());
        m.cpu_write(0x8001, 0x05);
        // Slot 0 should still be at bank 0 (initial value).
        assert_eq!(m.cpu_peek(0x8000), 0);
        // The "real" $8000 write does work.
        m.cpu_write(0x8000, 0x05);
        assert_eq!(m.cpu_peek(0x8000), 5);
    }
}
