// SPDX-License-Identifier: GPL-3.0-or-later
//! Nintendo NES-EVENT (iNES mapper 105).
//!
//! Single-cart mapper used by the Nintendo World Championships 1990
//! prize ROM - around 110 carts ever produced. The board is an MMC1
//! variant where the CHR-bank-0 register is repurposed:
//!
//! - **bit 4 ("I bit")** drives a CPU-cycle countdown timer. Cleared =
//!   timer enabled (counts up); set = timer reset and IRQ acked.
//! - **bit 3** picks PRG mode: 0 = direct 32 KiB block in the first
//!   128 KiB ("menu / Block A"), 1 = MMC1's normal PRG mapping
//!   constrained to the second 128 KiB ("game / Block B").
//! - **bits 1-2** in Block A mode select one of four 32 KiB banks
//!   (the four games on the menu).
//!
//! The cart adds two pieces of state on top of MMC1:
//!
//! 1. **Init state machine** (Mesen2's three-state guard): power-on
//!    sets I = 1 and locks PRG to the first 32 KiB. The cart escapes
//!    only after observing `I=1 → I=0 → I=1` (Mesen2 calls these
//!    states `0 → 1 → 2`); from state 2 onward, PRG layout follows
//!    `chr0` / `prg` per the rules above. This matches the silicon's
//!    "double-arm" sequence the menu code performs before letting
//!    a chosen game's PRG be mapped.
//! 2. **IRQ countdown**: a 32-bit CPU-cycle counter that fires when
//!    `counter >= (dip | 0x10) << 25`. The dip-switch field (4 bits)
//!    selects the time limit between roughly 5:00 and 9:42. We
//!    default `dip = 0` (shortest time, ~5 minutes); a future UI hook
//!    can drive [`Mapper105::set_dip`].
//!
//! ## Bank formulas (state 2)
//!
//! Let `chr0` = bank-CHR-0 latched register, `prg` = PRG bank
//! latched register, `mode` = control bits 2-3.
//!
//! | `chr0 & 0x08` | `mode` | `$8000-$BFFF` | `$C000-$FFFF` |
//! |---------------|--------|---------------|----------------|
//! | 0 (Block A)   | any    | `chr0 & 0x06` | `(chr0 & 0x06) | 1` |
//! | 1 (Block B)   | 0/1    | `(prg & 0x06) | 0x08` | `(prg & 0x06) | 0x09` |
//! | 1 (Block B)   | 2      | `0x08`        | `(prg & 0x07) | 0x08` |
//! | 1 (Block B)   | 3      | `(prg & 0x07) | 0x08` | `0x0F`     |
//!
//! Banks are 16 KiB; `0x08`-`0x0F` are the second-half 128 KiB
//! window.
//!
//! ## CHR / mirroring / WRAM
//!
//! 8 KiB CHR-RAM (no banking). Mirroring follows MMC1's control bits
//! 0-1. WRAM at `$6000-$7FFF` is always enabled (the prize cart had
//! no save chip; reads still pass through, writes round-trip in RAM).
//!
//! Clean-room references (behavioral only):
//! - `~/Git/Mesen2/Core/NES/Mappers/Nintendo/MMC1_105.h`
//! - `~/Git/punes/src/core/mappers/mapper_105.c`
//! - `~/Git/nestopia/source/core/board/NstBoardEvent.cpp` (`Event`)
//! - nesdev.org/wiki/INES_Mapper_105

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_16K: usize = 16 * 1024;
const CHR_RAM_SIZE: usize = 8 * 1024;
const WRAM_SIZE: usize = 8 * 1024;

pub struct Mapper105 {
    prg_rom: Vec<u8>,
    chr_ram: Vec<u8>,
    prg_ram: Vec<u8>,
    mirroring: Mirroring,

    prg_bank_count_16k: usize,

    // MMC1 serial shifter
    shift: u8,
    shift_count: u8,

    // MMC1 committed registers
    control: u8,
    chr0: u8,
    chr1: u8,
    prg: u8,

    // Mapper-105 additions
    /// Init state: 0 = power-on (I bit was 1 at boot, never cleared),
    /// 1 = saw `I=0` once, 2 = saw `I=0 → I=1` and is now in game mode.
    init_state: u8,
    /// CPU-cycle counter for the timer. Counts only when I = 0.
    irq_counter: u32,
    /// Whether `irq_counter` is currently advancing.
    irq_enabled: bool,
    /// /IRQ output line. Cleared whenever I bit goes back to 1.
    irq_line: bool,
    /// 4-bit dip switch (0..=15) controlling the timer cap.
    dip: u8,

    // MMC1 consecutive-write filter
    cycle_counter: u64,
    last_write_cycle: Option<u64>,
}

impl Mapper105 {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count_16k = (cart.prg_rom.len() / PRG_BANK_16K).max(1);
        let prg_ram = vec![0u8; (cart.prg_ram_size + cart.prg_nvram_size).max(WRAM_SIZE)];
        let mut m = Self {
            prg_rom: cart.prg_rom,
            chr_ram: vec![0u8; CHR_RAM_SIZE],
            prg_ram,
            mirroring: cart.mirroring,
            prg_bank_count_16k,
            shift: 0x10,
            shift_count: 0,
            control: 0x0C,    // PRG mode 3 on power-up (MMC1 default)
            chr0: 0x10,        // I bit set on power-up - matches Mesen2
            chr1: 0,
            prg: 0,
            init_state: 0,
            irq_counter: 0,
            irq_enabled: false,
            irq_line: false,
            dip: 0,
            cycle_counter: 0,
            last_write_cycle: None,
        };
        m.refresh_mirroring();
        m.update_state();
        m
    }

    /// Set the cart's 4-bit dip switch (0..=15). Called by the UI to
    /// match the Nestopia / Mesen2 dialog. Default 0 = shortest time.
    pub fn set_dip(&mut self, dip: u8) {
        self.dip = dip & 0x0F;
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

    /// MMC1 prg_mode (control bits 2-3): 0/1 = 32 KiB switch,
    /// 2 = fix first / switch second, 3 = switch first / fix last.
    fn prg_mode(&self) -> u8 {
        (self.control >> 2) & 0x03
    }

    /// Mesen2-style transition tracker. Called whenever a register
    /// commit happens: advances the init state machine and gates
    /// the IRQ counter on `chr0 & 0x10`.
    fn update_state(&mut self) {
        let i_bit_set = (self.chr0 & 0x10) != 0;
        match self.init_state {
            0 if !i_bit_set => self.init_state = 1,
            1 if i_bit_set => self.init_state = 2,
            _ => {}
        }
        if i_bit_set {
            self.irq_enabled = false;
            self.irq_counter = 0;
            self.irq_line = false;
        } else {
            self.irq_enabled = true;
        }
    }

    /// Resolve a CPU address inside `$8000-$FFFF` to a 16 KiB bank
    /// index, applying the init-state lock and Block A / Block B
    /// rules.
    fn prg_bank_for(&self, addr: u16) -> usize {
        // States 0 and 1 lock to the first 32 KiB unconditionally -
        // the menu / arming sequence has not yet completed.
        if self.init_state < 2 {
            return match addr {
                0x8000..=0xBFFF => 0,
                0xC000..=0xFFFF => 1,
                _ => 0,
            };
        }

        if (self.chr0 & 0x08) == 0 {
            // Block A direct: 32 KiB switch from chr0 bits 1-2.
            let pair = (self.chr0 & 0x06) as usize;
            return match addr {
                0x8000..=0xBFFF => pair,
                0xC000..=0xFFFF => pair | 0x01,
                _ => 0,
            };
        }

        // Block B (second 128 KiB) under MMC1 PRG-mode rules.
        let prg_eff = ((self.prg & 0x07) | 0x08) as usize;
        match self.prg_mode() {
            0 | 1 => {
                // 32 KiB switch within block B - low bit of prg_eff ignored.
                let base = prg_eff & 0xFE;
                match addr {
                    0x8000..=0xBFFF => base,
                    0xC000..=0xFFFF => base | 0x01,
                    _ => 0,
                }
            }
            2 => {
                // Fix first (0x08), switch second.
                match addr {
                    0x8000..=0xBFFF => 0x08,
                    0xC000..=0xFFFF => prg_eff,
                    _ => 0,
                }
            }
            3 => {
                // Switch first, fix last of block B (0x0F).
                match addr {
                    0x8000..=0xBFFF => prg_eff,
                    0xC000..=0xFFFF => 0x0F,
                    _ => 0,
                }
            }
            _ => 0,
        }
    }

    fn map_prg(&self, addr: u16) -> usize {
        let bank = self.prg_bank_for(addr) % self.prg_bank_count_16k.max(1);
        bank * PRG_BANK_16K + ((addr as usize) & (PRG_BANK_16K - 1))
    }

    fn commit(&mut self, addr: u16, value: u8) {
        match addr {
            0x8000..=0x9FFF => {
                self.control = value & 0x1F;
                self.refresh_mirroring();
            }
            0xA000..=0xBFFF => {
                self.chr0 = value & 0x1F;
            }
            0xC000..=0xDFFF => {
                self.chr1 = value & 0x1F;
            }
            0xE000..=0xFFFF => {
                self.prg = value & 0x1F;
            }
            _ => {}
        }
        self.update_state();
    }

    fn feed_shift(&mut self, addr: u16, data: u8) {
        if (data & 0x80) != 0 {
            self.shift = 0x10;
            self.shift_count = 0;
            self.control |= 0x0C;
            self.refresh_mirroring();
            // Reset writes do *not* trigger update_state on Mesen2 -
            // only direct register commits do. We mirror that: only
            // refresh mirroring here.
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

    /// Timer cap = `(dip | 0x10) << 25`. Matches Mesen2 + puNES.
    fn irq_cap(&self) -> u32 {
        (self.dip as u32 | 0x10) << 25
    }
}

impl Mapper for Mapper105 {
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
                let i = self.map_prg(addr);
                *self.prg_rom.get(i).unwrap_or(&0)
            }
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        match addr {
            0x6000..=0x7FFF => {
                let i = (addr - 0x6000) as usize;
                if let Some(slot) = self.prg_ram.get_mut(i) {
                    *slot = data;
                }
            }
            0x8000..=0xFFFF => {
                if let Some(prev) = self.last_write_cycle {
                    if self.cycle_counter == prev.wrapping_add(1) {
                        return; // MMC1 consecutive-write bug
                    }
                }
                self.last_write_cycle = Some(self.cycle_counter);
                self.feed_shift(addr, data);
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
        self.cycle_counter = self.cycle_counter.wrapping_add(1);
        if self.irq_enabled {
            self.irq_counter = self.irq_counter.wrapping_add(1);
            if self.irq_counter >= self.irq_cap() {
                self.irq_line = true;
                self.irq_enabled = false;
            }
        }
    }

    fn irq_line(&self) -> bool {
        self.irq_line
    }

    fn save_state_capture(&self) -> Option<crate::save_state::MapperState> {
        use crate::save_state::mapper::{MirroringSnap, NesEventSnap};
        Some(crate::save_state::MapperState::NesEvent(Box::new(NesEventSnap {
            chr_ram: self.chr_ram.clone(),
            prg_ram: self.prg_ram.clone(),
            mirroring: MirroringSnap::from_live(self.mirroring),
            shift: self.shift,
            shift_count: self.shift_count,
            control: self.control,
            chr0: self.chr0,
            chr1: self.chr1,
            prg: self.prg,
            init_state: self.init_state,
            irq_counter: self.irq_counter,
            irq_enabled: self.irq_enabled,
            irq_line: self.irq_line,
            dip: self.dip,
            cycle_counter: self.cycle_counter,
            last_write_cycle: self.last_write_cycle,
        })))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::NesEvent(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        if snap.chr_ram.len() == self.chr_ram.len() {
            self.chr_ram.copy_from_slice(&snap.chr_ram);
        }
        if snap.prg_ram.len() == self.prg_ram.len() {
            self.prg_ram.copy_from_slice(&snap.prg_ram);
        }
        self.mirroring = snap.mirroring.to_live();
        self.shift = snap.shift;
        self.shift_count = snap.shift_count;
        self.control = snap.control;
        self.chr0 = snap.chr0;
        self.chr1 = snap.chr1;
        self.prg = snap.prg;
        self.init_state = snap.init_state;
        self.irq_counter = snap.irq_counter;
        self.irq_enabled = snap.irq_enabled;
        self.irq_line = snap.irq_line;
        self.dip = snap.dip;
        self.cycle_counter = snap.cycle_counter;
        self.last_write_cycle = snap.last_write_cycle;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, TvSystem};

    /// 256 KiB PRG (16 banks * 16 KiB) + 8 KiB CHR-RAM. Bank N has
    /// its first byte tagged with `0x10 + N` so we can tell which
    /// physical bank is mapped at any address.
    fn cart() -> Cartridge {
        let mut prg = vec![0u8; 16 * PRG_BANK_16K];
        for b in 0..16 {
            prg[b * PRG_BANK_16K] = 0x10 + b as u8;
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: vec![],
            chr_ram: true,
            mapper_id: 105,
            submapper: 0,
            mirroring: Mirroring::Horizontal,
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

    fn m() -> Mapper105 {
        Mapper105::new(cart())
    }

    /// Walk a 5-bit value into the serial shifter. Pushes a cycle
    /// gap between writes so the consecutive-write filter doesn't
    /// drop the next bit.
    fn shift_in(m: &mut Mapper105, addr: u16, value: u8) {
        for i in 0..5 {
            let bit = (value >> i) & 1;
            m.cpu_write(addr, bit);
            m.on_cpu_cycle();
            m.on_cpu_cycle();
        }
    }

    #[test]
    fn power_on_locks_prg_to_first_32k_with_i_bit_set() {
        let m = m();
        // I bit is set at boot - init_state == 0, PRG locked to banks 0/1.
        assert_eq!(m.init_state, 0);
        assert_eq!(m.cpu_peek(0x8000), 0x10); // bank 0
        assert_eq!(m.cpu_peek(0xC000), 0x11); // bank 1
        assert!(!m.irq_enabled);
    }

    #[test]
    fn first_i_clear_advances_to_state_1_but_keeps_prg_locked() {
        let mut m = m();
        // Write CHR0 = 0 (clears I bit). state should advance 0 -> 1
        // but PRG must still report banks 0/1.
        shift_in(&mut m, 0xA000, 0x00);
        assert_eq!(m.init_state, 1);
        assert!(m.irq_enabled, "IRQ counter should be running in state 1");
        assert_eq!(m.cpu_peek(0x8000), 0x10);
        assert_eq!(m.cpu_peek(0xC000), 0x11);
    }

    #[test]
    fn second_i_set_advances_to_state_2_and_unlocks_prg() {
        let mut m = m();
        shift_in(&mut m, 0xA000, 0x00); // state 0 -> 1
        shift_in(&mut m, 0xA000, 0x10); // state 1 -> 2 (I bit re-set)
        assert_eq!(m.init_state, 2);
        // I bit is set - IRQ disabled, counter cleared.
        assert!(!m.irq_enabled);
        assert_eq!(m.irq_counter, 0);
    }

    #[test]
    fn block_a_direct_picks_one_of_four_32k_banks_via_chr0_bits_1_and_2() {
        // Get into state 2 first.
        let mut m = m();
        shift_in(&mut m, 0xA000, 0x00);
        shift_in(&mut m, 0xA000, 0x10);
        // chr0 = 0x10 puts us in Block A direct (bit 3 = 0).
        // chr0 & 0x06 = 0 -> banks 0/1.
        assert_eq!(m.cpu_peek(0x8000), 0x10);
        assert_eq!(m.cpu_peek(0xC000), 0x11);

        // chr0 = 0x12 -> bit 3 still 0, chr0 & 0x06 = 2 -> banks 2/3.
        // (Need to keep I bit set so we stay in state 2 with IRQ off.)
        shift_in(&mut m, 0xA000, 0x12);
        assert_eq!(m.cpu_peek(0x8000), 0x12);
        assert_eq!(m.cpu_peek(0xC000), 0x13);

        // chr0 = 0x16 -> chr0 & 0x06 = 6 -> banks 6/7.
        shift_in(&mut m, 0xA000, 0x16);
        assert_eq!(m.cpu_peek(0x8000), 0x16);
        assert_eq!(m.cpu_peek(0xC000), 0x17);
    }

    #[test]
    fn block_b_mode0_32k_switch_within_second_128k() {
        let mut m = m();
        shift_in(&mut m, 0xA000, 0x00);
        shift_in(&mut m, 0xA000, 0x18); // I=1 + bit 3 = 1 (Block B), bits 1-2 don't matter
        // control bits 2-3 = 0 -> 32 KiB switch (mode 0/1). Need to
        // commit a control with prg_mode = 0.
        shift_in(&mut m, 0x8000, 0x00); // control = 0 -> mirroring single-A, prg_mode 0
        // prg = 0x05 -> prg_eff = (5 & 7) | 8 = 0x0D, base = 0x0C.
        shift_in(&mut m, 0xE000, 0x05);
        assert_eq!(m.cpu_peek(0x8000), 0x10 + 0x0C);
        assert_eq!(m.cpu_peek(0xC000), 0x10 + 0x0D);
    }

    #[test]
    fn block_b_mode2_fixes_first_then_switches_c000() {
        let mut m = m();
        shift_in(&mut m, 0xA000, 0x00);
        shift_in(&mut m, 0xA000, 0x18); // Block B, I = 1
        // control = 0x08 -> prg_mode 2 (control bit 3 = 1, bit 2 = 0).
        shift_in(&mut m, 0x8000, 0x08);
        shift_in(&mut m, 0xE000, 0x05); // prg = 5 -> prg_eff = 0x0D
        assert_eq!(m.cpu_peek(0x8000), 0x10 + 0x08); // fixed first of block B
        assert_eq!(m.cpu_peek(0xC000), 0x10 + 0x0D); // switchable
    }

    #[test]
    fn block_b_mode3_switches_8000_fixes_last_at_0f() {
        let mut m = m();
        shift_in(&mut m, 0xA000, 0x00);
        shift_in(&mut m, 0xA000, 0x18);
        // control = 0x0C -> prg_mode 3 (control bits 2-3 = 11).
        shift_in(&mut m, 0x8000, 0x0C);
        shift_in(&mut m, 0xE000, 0x05); // prg_eff = 0x0D
        assert_eq!(m.cpu_peek(0x8000), 0x10 + 0x0D);
        assert_eq!(m.cpu_peek(0xC000), 0x10 + 0x0F); // last of block B
    }

    #[test]
    fn irq_counter_fires_at_dip_zero_cap() {
        let mut m = m();
        // Set dip = 0 (default). Cap = 0x20000000.
        // Get into state 1 with I clear so the counter runs.
        shift_in(&mut m, 0xA000, 0x00);
        assert!(m.irq_enabled);
        // Tick to one cycle before the cap, then one more.
        let cap = m.irq_cap();
        // Subtract the cycles consumed by shift_in (5 bits * 2 cycles
        // per bit = 10 cycles already accounted for by `cycle_counter`,
        // but irq_counter ticks only when irq_enabled was set, which
        // toggles on each commit). To keep the test focused, we just
        // simulate the remaining gap as a raw advance.
        let needed = cap.saturating_sub(m.irq_counter);
        for _ in 0..needed.saturating_sub(1) {
            m.on_cpu_cycle();
        }
        assert!(!m.irq_line, "must not fire before cap");
        m.on_cpu_cycle();
        assert!(m.irq_line, "must fire at cap");
        // Once fired, the counter is disabled (one-shot).
        assert!(!m.irq_enabled);
    }

    #[test]
    fn writing_i_bit_set_resets_counter_and_clears_irq() {
        let mut m = m();
        shift_in(&mut m, 0xA000, 0x00); // I = 0, counter starts
        for _ in 0..1_000 {
            m.on_cpu_cycle();
        }
        assert!(m.irq_counter > 0);

        // Re-set I bit -> counter clears, IRQ off.
        shift_in(&mut m, 0xA000, 0x10);
        assert_eq!(m.irq_counter, 0);
        assert!(!m.irq_enabled);
        assert!(!m.irq_line);
    }

    #[test]
    fn dip_switch_scales_the_cap() {
        // dip = 0 -> cap = 0x20000000; dip = 15 -> cap = 0x3E000000.
        let mut m = m();
        m.set_dip(0);
        assert_eq!(m.irq_cap(), 0x2000_0000);
        m.set_dip(15);
        assert_eq!(m.irq_cap(), 0x3E00_0000);
    }

    #[test]
    fn save_state_round_trips_init_state_irq_and_registers() {
        let mut a = m();
        shift_in(&mut a, 0xA000, 0x00);
        shift_in(&mut a, 0xA000, 0x18); // state 2, Block B
        shift_in(&mut a, 0x8000, 0x0C); // mode 3
        shift_in(&mut a, 0xE000, 0x05); // prg = 5
        a.set_dip(7);
        for _ in 0..50 {
            a.on_cpu_cycle();
        }
        let snap = a.save_state_capture().unwrap();

        let mut b = m();
        b.save_state_apply(&snap).unwrap();
        assert_eq!(b.init_state, 2);
        assert_eq!(b.dip, 7);
        // Same PRG layout as A.
        assert_eq!(b.cpu_peek(0x8000), a.cpu_peek(0x8000));
        assert_eq!(b.cpu_peek(0xC000), a.cpu_peek(0xC000));
    }
}
