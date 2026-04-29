// SPDX-License-Identifier: GPL-3.0-or-later
//! Jaleco JF-17 family - iNES mappers 72 (JF-17) and 92 (JF-19).
//!
//! Same discrete-TTL board with two PCB wirings: JF-17 makes
//! `$8000-$BFFF` the switchable PRG slot (last 16 KiB fixed at
//! `$C000`), JF-19 swaps the roles (bank 0 fixed at `$8000`,
//! switchable slot at `$C000`). The chip's signature behavior is
//! a pair of **rising-edge gates** that latch new PRG/CHR banks
//! only when bit 7 / bit 6 transitions from 0 to 1; once high
//! the bank stays put until bit 7 / bit 6 goes low and re-rises.
//!
//! Bus conflict applies (open-collector AND of CPU value with the
//! ROM byte at the write address) - puNES, Nestopia, and Mesen2's
//! `HasBusConflicts()` all set this for the JF family.
//!
//! ## Carts
//!
//! - **Mapper 72** (JF-17): *Pinball Quest*, *Moero!! Juudou
//!   Warriors*, *Wing of Madoola*.
//! - **Mapper 92** (JF-19): *Moero!! Pro Yakyuu '88: Ketteiban*,
//!   *Moero!! Pro Tennis*.
//!
//! ## Register map
//!
//! Single 8-bit latch decoded across `$8000-$FFFF`:
//!
//! ```text
//! 7  bit  0
//! ---- ----
//! PCSS BBBB
//! |||| ||||
//! |||| ++++- Bank value (PRG or CHR, latched by P/C gates below)
//! ||++------ uPD7756C ADPCM sample-playback control (NOT EMULATED)
//! |+-------- 0->1 rising edge: latch CHR bank from BBBB
//! +--------- 0->1 rising edge: latch PRG bank from BBBB
//! ```
//!
//! The audio bits drive an off-chip uPD7756C speech sample-ROM
//! that ships separately from the cart program ROM (`misc.rom` in
//! NES 2.0 dumps). We don't ship ADPCM support; the bits are
//! decoded enough to keep the latch state consistent but no
//! sample is played.
//!
//! References:
//! - <https://www.nesdev.org/wiki/INES_Mapper_072>
//! - <https://www.nesdev.org/wiki/INES_Mapper_092>
//! - `~/Git/Mesen2/Core/NES/Mappers/Jaleco/JalecoJf17_19.h`
//!   (rising-edge gates + bus-conflict declaration)
//! - `~/Git/punes/src/core/mappers/mapper_072.c` (bus conflict +
//!   `(reg ^ value) & value` rising-edge trick)
//! - `~/Git/nestopia/source/core/board/NstBoardJalecoJf17.cpp`

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_16K: usize = 16 * 1024;
const CHR_BANK_8K: usize = 8 * 1024;

/// Which slot is software-switchable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrgWiring {
    /// JF-17 (mapper 72): switchable at `$8000`, fixed last at `$C000`.
    SwitchableLow,
    /// JF-19 (mapper 92): fixed bank 0 at `$8000`, switchable at `$C000`.
    SwitchableHigh,
}

pub struct JalecoJf17 {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,

    wiring: PrgWiring,

    /// Live PRG bank for the switchable slot.
    prg_bank: u8,
    /// Live CHR bank for the 8 KiB CHR window.
    chr_bank: u8,
    /// Last value written's bit-7 state (for rising-edge detection).
    prev_prg_gate: bool,
    /// Last value written's bit-6 state.
    prev_chr_gate: bool,

    mirroring: Mirroring,

    prg_bank_count_16k: usize,
    chr_bank_count_8k: usize,
}

impl JalecoJf17 {
    /// Mapper 72 (JF-17): switchable PRG at `$8000`, last bank fixed at `$C000`.
    pub fn new_72(cart: Cartridge) -> Self {
        Self::new(cart, PrgWiring::SwitchableLow)
    }

    /// Mapper 92 (JF-19): bank 0 fixed at `$8000`, switchable PRG at `$C000`.
    pub fn new_92(cart: Cartridge) -> Self {
        Self::new(cart, PrgWiring::SwitchableHigh)
    }

    fn new(cart: Cartridge, wiring: PrgWiring) -> Self {
        let prg_bank_count_16k = (cart.prg_rom.len() / PRG_BANK_16K).max(1);

        let is_chr_ram = cart.chr_ram || cart.chr_rom.is_empty();
        let chr = if is_chr_ram {
            vec![0u8; CHR_BANK_8K]
        } else {
            cart.chr_rom
        };
        let chr_bank_count_8k = (chr.len() / CHR_BANK_8K).max(1);

        Self {
            prg_rom: cart.prg_rom,
            chr,
            chr_ram: is_chr_ram,
            wiring,
            prg_bank: 0,
            chr_bank: 0,
            prev_prg_gate: false,
            prev_chr_gate: false,
            mirroring: cart.mirroring,
            prg_bank_count_16k,
            chr_bank_count_8k,
        }
    }

    fn last_prg_bank(&self) -> usize {
        self.prg_bank_count_16k - 1
    }

    fn prg_slot_bank(&self, slot: u8) -> usize {
        match (self.wiring, slot) {
            (PrgWiring::SwitchableLow, 0) => self.prg_bank as usize,
            (PrgWiring::SwitchableLow, 1) => self.last_prg_bank(),
            (PrgWiring::SwitchableHigh, 0) => 0,
            (PrgWiring::SwitchableHigh, 1) => self.prg_bank as usize,
            _ => unreachable!(),
        }
    }

    fn cpu_prg_byte(&self, addr: u16) -> u8 {
        let slot = ((addr - 0x8000) >> 14) as u8;
        let off = (addr & 0x3FFF) as usize;
        let bank = self.prg_slot_bank(slot) % self.prg_bank_count_16k;
        let base = bank * PRG_BANK_16K;
        *self.prg_rom.get(base + off).unwrap_or(&0)
    }
}

impl Mapper for JalecoJf17 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        if !(0x8000..=0xFFFF).contains(&addr) {
            return 0;
        }
        self.cpu_prg_byte(addr)
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        if !(0x8000..=0xFFFF).contains(&addr) {
            return;
        }
        // Bus conflict: AND with whatever ROM byte the active bank
        // exposes at this address.
        let rom_byte = self.cpu_prg_byte(addr);
        let value = data & rom_byte;

        let prg_gate = (value & 0x80) != 0;
        let chr_gate = (value & 0x40) != 0;

        // Rising-edge latch: only commit a new bank when the gate
        // bit transitions from 0 to 1 in this write.
        if prg_gate && !self.prev_prg_gate {
            self.prg_bank = value & 0x0F;
        }
        if chr_gate && !self.prev_chr_gate {
            self.chr_bank = value & 0x0F;
        }

        self.prev_prg_gate = prg_gate;
        self.prev_chr_gate = chr_gate;

        // Bits 4-5 are uPD7756C ADPCM sample control. Not modeled.
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr < 0x2000 {
            let bank = (self.chr_bank as usize) % self.chr_bank_count_8k;
            let off = (addr & 0x1FFF) as usize;
            *self.chr.get(bank * CHR_BANK_8K + off).unwrap_or(&0)
        } else {
            0
        }
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if self.chr_ram && addr < 0x2000 {
            let bank = (self.chr_bank as usize) % self.chr_bank_count_8k;
            let off = (addr & 0x1FFF) as usize;
            if let Some(b) = self.chr.get_mut(bank * CHR_BANK_8K + off) {
                *b = data;
            }
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }

    fn save_state_capture(&self) -> Option<crate::save_state::MapperState> {
        use crate::save_state::mapper::JalecoJf17Snap;
        Some(crate::save_state::MapperState::JalecoJf17(JalecoJf17Snap {
            chr_ram_data: if self.chr_ram { self.chr.clone() } else { Vec::new() },
            prg_bank: self.prg_bank,
            chr_bank: self.chr_bank,
            prev_prg_gate: self.prev_prg_gate,
            prev_chr_gate: self.prev_chr_gate,
            switchable_high: self.wiring == PrgWiring::SwitchableHigh,
        }))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::JalecoJf17(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        // Reject a cross-variant restore (mapper 72 ↔ 92): the PRG
        // wiring is structural and a swap would mean the live cart
        // is the wrong mapper for this save.
        let live_high = self.wiring == PrgWiring::SwitchableHigh;
        if snap.switchable_high != live_high {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        }
        if self.chr_ram && snap.chr_ram_data.len() == self.chr.len() {
            self.chr.copy_from_slice(&snap.chr_ram_data);
        }
        self.prg_bank = snap.prg_bank;
        self.chr_bank = snap.chr_bank;
        self.prev_prg_gate = snap.prev_prg_gate;
        self.prev_chr_gate = snap.prev_chr_gate;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, Mirroring, TvSystem};

    /// 256 KiB PRG (16 banks of 16 KiB), 128 KiB CHR (16 banks of 8
    /// KiB). First byte of each bank tagged with the bank index;
    /// remaining bytes = $FF so bus-conflict tests at `$FFFF`
    /// (where the byte is $FF) pass the CPU value through cleanly.
    fn cart() -> Cartridge {
        let mut prg = vec![0xFFu8; 16 * PRG_BANK_16K];
        for bank in 0..16 {
            prg[bank * PRG_BANK_16K] = bank as u8;
        }
        let mut chr = vec![0xFFu8; 16 * CHR_BANK_8K];
        for bank in 0..16 {
            chr[bank * CHR_BANK_8K] = bank as u8;
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: chr,
            chr_ram: false,
            mapper_id: 72,
            submapper: 0,
            mirroring: Mirroring::Horizontal,
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
    fn jf17_power_on_layout_is_bank0_then_last() {
        let m = JalecoJf17::new_72(cart());
        assert_eq!(m.cpu_peek(0x8000), 0); // bank 0
        assert_eq!(m.cpu_peek(0xC000), 15); // last bank
    }

    #[test]
    fn jf19_power_on_layout_is_bank0_at_8000_with_switchable_at_c000() {
        let m = JalecoJf17::new_92(cart());
        assert_eq!(m.cpu_peek(0x8000), 0); // fixed bank 0
        // Switchable slot defaults to bank 0 too (prg_bank = 0).
        assert_eq!(m.cpu_peek(0xC000), 0);
    }

    #[test]
    fn rising_edge_on_bit_7_latches_new_prg_bank() {
        let mut m = JalecoJf17::new_72(cart());
        // First write with bit 7 high latches a new bank.
        // Use $FFFF: ROM byte = $FF; bus conflict no-op.
        m.cpu_write(0xFFFF, 0x85); // bit 7 set, bank value 5
        assert_eq!(m.cpu_peek(0x8000), 5);
        // A second write while bit 7 stays high does NOT switch.
        m.cpu_write(0xFFFF, 0x83);
        assert_eq!(m.cpu_peek(0x8000), 5, "no rising edge -> no swap");
        // Drop bit 7 low.
        m.cpu_write(0xFFFF, 0x03);
        assert_eq!(m.cpu_peek(0x8000), 5, "low gate doesn't change anything");
        // Re-rise: now we latch the new value.
        m.cpu_write(0xFFFF, 0x87);
        assert_eq!(m.cpu_peek(0x8000), 7);
    }

    #[test]
    fn rising_edge_on_bit_6_latches_new_chr_bank() {
        let mut m = JalecoJf17::new_72(cart());
        m.cpu_write(0xFFFF, 0x46); // bit 6 set, bank 6
        assert_eq!(m.ppu_read(0x0000), 6);
        m.cpu_write(0xFFFF, 0x42); // still high; no swap
        assert_eq!(m.ppu_read(0x0000), 6);
        m.cpu_write(0xFFFF, 0x02);
        m.cpu_write(0xFFFF, 0x4B); // re-rise + bank 11
        assert_eq!(m.ppu_read(0x0000), 11);
    }

    #[test]
    fn jf19_writes_change_high_slot_only() {
        let mut m = JalecoJf17::new_92(cart());
        m.cpu_write(0xFFFF, 0x83); // bit 7 set, bank 3
        // $8000 stays at bank 0 (fixed); $C000 becomes bank 3.
        assert_eq!(m.cpu_peek(0x8000), 0);
        assert_eq!(m.cpu_peek(0xC000), 3);
    }

    #[test]
    fn bus_conflict_ands_value_with_rom_byte() {
        let mut m = JalecoJf17::new_72(cart());
        // Latch bank 5 first.
        m.cpu_write(0xFFFF, 0x85);
        assert_eq!(m.cpu_peek(0x8000), 5);
        // Drop the gate so the next rising-edge can latch.
        m.cpu_write(0xFFFF, 0x05);
        // Now write at $8000 (bank 5's tag byte = $05). The CPU
        // value $C7 (bit 7 set, bank 7) ANDs with $05 -> $05. The
        // gate bit becomes clear, so the rising-edge check fails
        // and no swap happens.
        m.cpu_write(0x8000, 0xC7);
        assert_eq!(m.cpu_peek(0x8000), 5, "AND killed bit 7, no rising edge");
    }

    #[test]
    fn jf19_save_state_rejects_cross_variant_restore() {
        let m92 = JalecoJf17::new_92(cart());
        let snap = m92.save_state_capture().unwrap();
        let mut m72 = JalecoJf17::new_72(cart());
        match m72.save_state_apply(&snap) {
            Err(crate::save_state::SaveStateError::UnsupportedMapper(_)) => {}
            other => panic!("expected UnsupportedMapper, got {other:?}"),
        }
    }

    #[test]
    fn writes_below_8000_are_noop() {
        let mut m = JalecoJf17::new_72(cart());
        m.cpu_write(0x4020, 0xFF);
        m.cpu_write(0x6000, 0x85);
        m.cpu_write(0x7FFF, 0x85);
        assert_eq!(m.cpu_peek(0x8000), 0); // still bank 0
    }

    #[test]
    fn chr_ram_round_trips_when_no_chr_rom() {
        let mut c = cart();
        c.chr_rom = Vec::new();
        c.chr_ram = true;
        let mut m = JalecoJf17::new_72(c);
        m.ppu_write(0x0010, 0x99);
        assert_eq!(m.ppu_read(0x0010), 0x99);
    }
}
