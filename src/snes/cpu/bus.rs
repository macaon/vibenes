// SPDX-License-Identifier: GPL-3.0-or-later
//! Bus surface seen by the 65C816 core. Phase 2 only needs the
//! cycle-counted read/write/idle calls; the real
//! [`crate::snes::Snes`] bus (with PPU/MMIO/DMA) lands in Phase 3.
//! For unit tests we provide [`FlatBus`], a 16 MiB linear memory
//! that charges 8 master cycles per access (the SlowROM rate) so
//! cycle-count assertions stay deterministic.

/// Operations the 65C816 needs from whatever owns the 24-bit bus.
/// All methods advance the master clock; the implementation decides
/// by how much (typically 6 / 8 / 12 master cycles per access based
/// on the target region + `MEMSEL`).
pub trait SnesBus {
    /// Read one byte from the 24-bit address `addr`. The
    /// implementation advances the master clock by the appropriate
    /// per-region access cost.
    fn read(&mut self, addr: u32) -> u8;

    /// Write one byte. Same clocking rules as [`SnesBus::read`].
    fn write(&mut self, addr: u32, value: u8);

    /// Internal CPU cycle (no bus access). Advances the master clock
    /// by the "fast" rate (6 master cycles) - the 65C816 datasheet
    /// calls these "internal operation" cycles. Used for things like
    /// the dummy cycle on direct-page operations when `D & $FF != 0`.
    fn idle(&mut self);

    /// Total master-clock ticks this bus has accumulated. Used by
    /// tests to assert cycle counts.
    fn master_cycles(&self) -> u64;
}

/// 16 MiB flat-memory bus. Every access charges 8 master cycles
/// regardless of address. Convenient for unit-testing the CPU core
/// without a memory-mapper in place; the real bus lands in Phase 3.
pub struct FlatBus {
    pub ram: Vec<u8>,
    master: u64,
}

impl FlatBus {
    pub const SIZE: usize = 1 << 24;

    pub fn new() -> Self {
        Self {
            ram: vec![0; Self::SIZE],
            master: 0,
        }
    }

    /// Direct memory poke that does NOT advance the clock. Use from
    /// tests to seed program code / data without skewing cycle
    /// counts.
    pub fn poke(&mut self, addr: u32, value: u8) {
        self.ram[(addr as usize) & (Self::SIZE - 1)] = value;
    }

    pub fn poke_slice(&mut self, addr: u32, bytes: &[u8]) {
        let base = (addr as usize) & (Self::SIZE - 1);
        let end = (base + bytes.len()).min(Self::SIZE);
        self.ram[base..end].copy_from_slice(&bytes[..end - base]);
    }

    pub fn peek(&self, addr: u32) -> u8 {
        self.ram[(addr as usize) & (Self::SIZE - 1)]
    }
}

impl Default for FlatBus {
    fn default() -> Self {
        Self::new()
    }
}

impl SnesBus for FlatBus {
    fn read(&mut self, addr: u32) -> u8 {
        self.master = self.master.wrapping_add(8);
        self.ram[(addr as usize) & (Self::SIZE - 1)]
    }

    fn write(&mut self, addr: u32, value: u8) {
        self.master = self.master.wrapping_add(8);
        self.ram[(addr as usize) & (Self::SIZE - 1)] = value;
    }

    fn idle(&mut self) {
        self.master = self.master.wrapping_add(6);
    }

    fn master_cycles(&self) -> u64 {
        self.master
    }
}
