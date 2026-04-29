// SPDX-License-Identifier: GPL-3.0-or-later
use crate::nes::rom::TvSystem;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    Ntsc,
    Pal,
}

impl Region {
    pub fn from_tv_system(tv: TvSystem) -> Self {
        match tv {
            TvSystem::Ntsc => Region::Ntsc,
            TvSystem::Pal => Region::Pal,
        }
    }

    /// Master-clock ticks per CPU cycle. NTSC CPU = master/12; PAL CPU = master/16.
    pub const fn master_per_cpu(self) -> u64 {
        match self {
            Region::Ntsc => 12,
            Region::Pal => 16,
        }
    }

    /// Master-clock ticks per PPU dot. NTSC PPU = master/4; PAL PPU = master/5.
    /// CPU:PPU ratio therefore is 1:3 on NTSC and 1:3.2 on PAL.
    pub const fn master_per_ppu(self) -> u64 {
        match self {
            Region::Ntsc => 4,
            Region::Pal => 5,
        }
    }

    /// Total scanlines in a frame (including pre-render line).
    pub const fn scanlines_per_frame(self) -> i16 {
        match self {
            Region::Ntsc => 262,
            Region::Pal => 312,
        }
    }

    /// Index of the pre-render scanline.
    pub const fn pre_render_scanline(self) -> i16 {
        match self {
            Region::Ntsc => 261,
            Region::Pal => 311,
        }
    }

    /// CPU-side clock rate in Hz - the rate at which the APU emits one
    /// mixer sample per bus tick. Used by the host audio resampler to
    /// convert APU output to the sound device's sample rate.
    /// NTSC master = 21.477272 MHz ÷ 12; PAL master = 26.601712 MHz ÷ 16.
    pub fn cpu_clock_hz(self) -> f64 {
        match self {
            Region::Ntsc => 21_477_272.0 / 12.0,
            Region::Pal => 26_601_712.0 / 16.0,
        }
    }
}

/// Master clock model.
///
/// The master clock is the single source of truth; CPU and PPU tick
/// counts are derived by dividing accumulated master ticks. This avoids
/// drift when the CPU:PPU ratio isn't integer (PAL is 1:3.2).
///
/// Each CPU cycle is split into a **start** phase (before the CPU's
/// bus access) and an **end** phase (after it). The split of master
/// cycles between the two is asymmetric and depends on whether the
/// CPU is reading or writing - mirroring Mesen2's
/// `_startClockCount`/`_endClockCount` model (`NesCpu.cpp:73-75`):
///
/// - **Read**:  start = `_startClockCount - 1` = 5, end = `_endClockCount + 1` = 7.
/// - **Write**: start = `_startClockCount + 1` = 7, end = `_endClockCount - 1` = 5.
///
/// (Both add to 12 on NTSC / 16 on PAL - total per CPU cycle.) The
/// asymmetry means that within a single CPU cycle the PPU runs 1 or 2
/// dots during the start phase and 1 or 2 during the end phase,
/// determined by master-clock parity + read/write - producing the
/// dynamic 2/1 or 1/2 split that our old fixed 2/1 model couldn't
/// reproduce. Required for `dmc_dma_during_read4`-class tests whose
/// iter alignment converges on the exact sub-cycle PPU state.
///
/// A `ppu_offset` shifts the PPU's master-clock "view" - the PPU is
/// run to `master_cycles - ppu_offset` rather than `master_cycles`
/// directly, matching Mesen2's default `_ppuOffset = 1`
/// (`NesCpu.cpp:154`). This is what makes a CPU cycle starting at
/// master = 12 see the 3-dot boundary at dot 2 vs dot 3 relative to
/// the bus access.
#[derive(Debug)]
pub struct MasterClock {
    region: Region,
    master_cycles: u64,
    cpu_cycles: u64,
    ppu_cycles: u64,
    /// PPU phase offset in master-clock units. PPU runs to
    /// `master_cycles - ppu_offset`. Mesen2 defaults to 1; see
    /// `NesCpu.cpp:154`.
    ppu_offset: u64,
}

impl MasterClock {
    pub fn new(region: Region) -> Self {
        // Mesen2 initialises `_masterClock = cpuDivider + cpuOffset`
        // (`NesCpu.cpp:158`) - master starts at one full CPU cycle
        // ahead of zero, so the first CPU cycle's start/end phases
        // land at master = cpuDivider+5 then +12 on a read. We match
        // that priming so phase math lines up with Mesen from the
        // first instruction.
        let master_cycles = region.master_per_cpu();
        Self {
            region,
            master_cycles,
            cpu_cycles: 0,
            ppu_cycles: 0,
            ppu_offset: 1,
        }
    }

    pub fn region(&self) -> Region {
        self.region
    }

    #[inline]
    pub fn master_cycles(&self) -> u64 {
        self.master_cycles
    }

    #[inline]
    pub fn cpu_cycles(&self) -> u64 {
        self.cpu_cycles
    }

    #[inline]
    pub fn ppu_cycles(&self) -> u64 {
        self.ppu_cycles
    }

    /// Advance the master clock by `delta` master-clock units and
    /// return the number of PPU dots that must now be ticked to catch
    /// the PPU up to `master_cycles - ppu_offset`.
    #[inline]
    fn advance_master(&mut self, delta: u64) -> u64 {
        self.master_cycles = self.master_cycles.wrapping_add(delta);
        let target_ppu = self
            .master_cycles
            .saturating_sub(self.ppu_offset)
            / self.region.master_per_ppu();
        let delta_ppu = target_ppu.saturating_sub(self.ppu_cycles);
        self.ppu_cycles = target_ppu;
        delta_ppu
    }

    /// Master-clock units the start phase of a CPU cycle should
    /// advance: `_startClockCount - 1` for a read, `+1` for a write.
    #[inline]
    fn start_clock_count(&self, is_read: bool) -> u64 {
        // NTSC: 6 ± 1. PAL: 8 ± 1.
        let base = self.region.master_per_cpu() / 2;
        if is_read { base - 1 } else { base + 1 }
    }

    /// Master-clock units the end phase of a CPU cycle should
    /// advance: the complement of `start_clock_count` so the two
    /// sum to `master_per_cpu`.
    #[inline]
    fn end_clock_count(&self, is_read: bool) -> u64 {
        self.region.master_per_cpu() - self.start_clock_count(is_read)
    }

    /// Enter the **start phase** of a CPU cycle: advance the master
    /// clock by the read/write-specific start delta and return the
    /// number of PPU dots the caller must tick before the bus access.
    /// Increments `cpu_cycles` here, matching Mesen2's
    /// `StartCpuCycle → _state.CycleCount++` ordering.
    #[inline]
    pub fn start_cpu_cycle(&mut self, is_read: bool) -> u64 {
        self.cpu_cycles = self.cpu_cycles.wrapping_add(1);
        let delta = self.start_clock_count(is_read);
        self.advance_master(delta)
    }

    /// Enter the **end phase** of a CPU cycle: advance by the
    /// complementary delta and return the number of PPU dots to tick
    /// after the bus access completes.
    #[inline]
    pub fn end_cpu_cycle(&mut self, is_read: bool) -> u64 {
        let delta = self.end_clock_count(is_read);
        self.advance_master(delta)
    }

    pub(crate) fn save_state_capture(&self) -> crate::save_state::bus::MasterClockSnap {
        crate::save_state::bus::MasterClockSnap {
            region: crate::save_state::RegionTag::from_region(self.region),
            master_cycles: self.master_cycles,
            cpu_cycles: self.cpu_cycles,
            ppu_cycles: self.ppu_cycles,
            ppu_offset: self.ppu_offset,
        }
    }

    /// Apply a captured master-clock snapshot. The `region` field of
    /// `snap` is informational only - the live region was set at
    /// [`MasterClock::new`] time from the cart's TV system and is
    /// already validated against the file header in
    /// [`crate::save_state::validate_against_nes`].
    pub(crate) fn save_state_apply(&mut self, snap: crate::save_state::bus::MasterClockSnap) {
        self.master_cycles = snap.master_cycles;
        self.cpu_cycles = snap.cpu_cycles;
        self.ppu_cycles = snap.ppu_cycles;
        self.ppu_offset = snap.ppu_offset;
    }
}
