use crate::rom::TvSystem;

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
}

/// Master clock model.
///
/// The master clock is the single source of truth; CPU and PPU tick
/// counts are derived by dividing accumulated master ticks. This avoids
/// drift when the CPU:PPU ratio isn't integer (PAL is 1:3.2).
#[derive(Debug)]
pub struct MasterClock {
    region: Region,
    master_cycles: u64,
    cpu_cycles: u64,
    ppu_cycles: u64,
}

impl MasterClock {
    pub fn new(region: Region) -> Self {
        Self {
            region,
            master_cycles: 0,
            cpu_cycles: 0,
            ppu_cycles: 0,
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

    /// Charge one CPU bus access against the master clock. Returns the number
    /// of PPU dots that have elapsed (3 on NTSC, 3 or 4 on PAL depending on
    /// phase) so the bus can step the PPU the right amount.
    #[inline]
    pub fn advance_cpu_cycle(&mut self) -> u64 {
        self.master_cycles = self.master_cycles.wrapping_add(self.region.master_per_cpu());
        self.cpu_cycles = self.cpu_cycles.wrapping_add(1);
        let target_ppu = self.master_cycles / self.region.master_per_ppu();
        let delta = target_ppu - self.ppu_cycles;
        self.ppu_cycles = target_ppu;
        delta
    }
}
