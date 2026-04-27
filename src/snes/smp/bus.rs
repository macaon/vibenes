// SPDX-License-Identifier: GPL-3.0-or-later
//! Bus surface seen by the SPC700 core. The SPC has a 16-bit address
//! space (ARAM + IPL shadow + I/O at `$F0-$FF`); the integrated bus
//! lands in Phase 5c as part of the host APU port bridge. For
//! sub-phase 5a we provide [`FlatSmpBus`], a 64 KiB linear memory
//! that charges 1 SMP cycle per access so unit tests can assert
//! cycle counts deterministically.
//!
//! References (paraphrased; clean-room-adjacent porting per project
//! policy): nes-expert SNES APU reference at
//! `~/.claude/skills/nes-expert/reference/snes-apu.md`,
//! Mesen2 `Core/SNES/Spc.h` for the bus surface shape (we land
//! `Read/Write/Idle` mirroring `SnesBus` rather than Mesen2's
//! `read/write` overloads).
//!
//! ## Bus model
//!
//! The SPC700 timing model is simpler than the 65C816's: every
//! instruction has a fixed cycle count documented in the official
//! Sony datasheet. Mesen2 (`Spc::Idle`, `Spc::Read`, `Spc::Write`)
//! charges one SMP cycle per `Read`/`Write` plus extra `Idle`
//! cycles for instructions whose published count exceeds their
//! memory-access count (e.g. CMP, MUL, DIV). We mirror that pattern.

/// Operations the SPC700 needs from whatever owns its 16-bit bus.
/// All methods advance the SMP clock by exactly one SMP cycle; the
/// instruction handlers in [`super::Smp`] sprinkle [`SmpBus::idle`]
/// calls as needed to make their total match the datasheet count.
pub trait SmpBus {
    /// Read one byte from `addr`. Advances the SMP clock by 1.
    fn read(&mut self, addr: u16) -> u8;

    /// Write one byte. Advances the SMP clock by 1.
    fn write(&mut self, addr: u16, value: u8);

    /// Internal cycle (no bus access). Charged for instructions
    /// whose datasheet cycle count exceeds their memory accesses.
    fn idle(&mut self);

    /// Total SMP cycles this bus has accumulated since construction.
    /// Used by tests to assert per-instruction cycle counts.
    fn cycles(&self) -> u64;
}

/// 64 KiB flat-memory bus. Every access charges exactly 1 SMP cycle.
/// Convenient for unit-testing the SPC700 core in isolation; the
/// integrated bus (with IPL shadow + I/O at `$F0-$FF` + DSP) lands
/// in Phases 5b-5c.
pub struct FlatSmpBus {
    pub ram: Vec<u8>,
    cycles: u64,
}

impl FlatSmpBus {
    pub const SIZE: usize = 1 << 16;

    pub fn new() -> Self {
        Self {
            ram: vec![0; Self::SIZE],
            cycles: 0,
        }
    }

    /// Direct memory poke that does NOT advance the clock. Use from
    /// tests to seed program code / data without skewing cycle
    /// counts.
    pub fn poke(&mut self, addr: u16, value: u8) {
        self.ram[addr as usize] = value;
    }

    pub fn poke_slice(&mut self, addr: u16, bytes: &[u8]) {
        let base = addr as usize;
        let end = (base + bytes.len()).min(Self::SIZE);
        self.ram[base..end].copy_from_slice(&bytes[..end - base]);
    }

    pub fn peek(&self, addr: u16) -> u8 {
        self.ram[addr as usize]
    }
}

impl Default for FlatSmpBus {
    fn default() -> Self {
        Self::new()
    }
}

impl SmpBus for FlatSmpBus {
    fn read(&mut self, addr: u16) -> u8 {
        self.cycles = self.cycles.wrapping_add(1);
        self.ram[addr as usize]
    }

    fn write(&mut self, addr: u16, value: u8) {
        self.cycles = self.cycles.wrapping_add(1);
        self.ram[addr as usize] = value;
    }

    fn idle(&mut self) {
        self.cycles = self.cycles.wrapping_add(1);
    }

    fn cycles(&self) -> u64 {
        self.cycles
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_and_write_advance_cycles_by_one() {
        let mut bus = FlatSmpBus::new();
        bus.poke(0x1234, 0x55);
        assert_eq!(bus.cycles(), 0);
        assert_eq!(bus.read(0x1234), 0x55);
        assert_eq!(bus.cycles(), 1);
        bus.write(0x1234, 0xAA);
        assert_eq!(bus.cycles(), 2);
        assert_eq!(bus.peek(0x1234), 0xAA);
    }

    #[test]
    fn idle_advances_cycles_without_touching_memory() {
        let mut bus = FlatSmpBus::new();
        bus.poke(0x0000, 0xFF);
        bus.idle();
        assert_eq!(bus.cycles(), 1);
        assert_eq!(bus.peek(0x0000), 0xFF);
    }
}
