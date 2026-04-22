use crate::apu::Apu;
use crate::audio::AudioSink;
use crate::clock::{MasterClock, Region};
use crate::mapper::Mapper;
use crate::ppu::Ppu;

/// Central bus. Every CPU bus access passes through here and advances the
/// master clock; the clock then tells us how many PPU dots to run. This is
/// the mechanism that keeps the CPU/PPU/APU in lock-step.
pub struct Bus {
    pub clock: MasterClock,
    pub ram: [u8; 0x800],
    pub ppu: Ppu,
    pub apu: Apu,
    pub mapper: Box<dyn Mapper>,

    pub controllers: [Controller; 2],

    pub nmi_pending: bool,
    pub irq_line: bool,

    /// `irq_line` as of the end of the *previous* CPU cycle. The CPU
    /// polls this at the end of every instruction so the interrupt is
    /// recognised based on state at end of the penultimate cycle, as
    /// on real 6502 hardware. Without this snapshot the polling would
    /// use state at the end of the *last* cycle, which breaks CLI/SEI/
    /// PLP delayed-interrupt semantics and branch-IRQ timing.
    pub prev_irq_line: bool,
    /// `nmi_pending` as of the end of the previous CPU cycle. Same
    /// rationale as `prev_irq_line`.
    pub prev_nmi_pending: bool,
    /// PPU `nmi_flag` level at end of the previous CPU cycle. Feeds
    /// the rising-edge detector in `tick_post_access`: a false→true
    /// transition across cycle boundaries latches `nmi_pending`.
    /// Mirrors Mesen2's `_prevNmiFlag` (NesCpu.cpp:306-309).
    prev_nmi_flag: bool,

    open_bus: u8,
    /// True while we're servicing a DMC DMA fetch. Prevents re-entering
    /// the DMA service from `tick_cycle` inside the stall cycles.
    dmc_dma_active: bool,
    /// True while OAM DMA is running (between the halt cycle at the
    /// start of `run_oam_dma` and the last sprite write at the end).
    /// When a DMC DMA request arms mid-OAM-DMA, the halt + dummy
    /// stall cycles are absorbed into the sprite-DMA cycles that
    /// were already going to run — the total DMC insertion drops
    /// from 4 to 2 cycles (Nestopia's `DMC_R4014` case,
    /// `NstApu.cpp:2297-2311`). Required by `sprdma_and_dmc_dma`
    /// for a stable cycle count across iterations.
    oam_dma_active: bool,
    /// Current OAM DMA byte index (0..256), valid only while
    /// `oam_dma_active` is true. Updated by `run_oam_dma` before
    /// each sprite read. Used by `service_pending_dmc_dma` to pick
    /// the exact stall-cycle count from Nestopia's end-of-OAM
    /// taxonomy (`cpu_inline.h:1381-1392`): index 253 → 1 cycle
    /// (`DMC_NNL_DMA`), 254 → 2 (`DMC_R4014`), 255 → 3
    /// (`DMC_CPU_WRITE`), everything else → 2.
    oam_dma_idx: u16,
    /// Host audio output. `None` in test ROMs / headless runs where
    /// opening a cpal stream would be pointless or unavailable.
    /// Attached from [`crate::nes::Nes::attach_audio`] after the bus is
    /// constructed.
    pub audio_sink: Option<AudioSink>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Controller {
    pub buttons: u8,
    strobe: bool,
    shifter: u8,
}

impl Controller {
    pub fn write_strobe(&mut self, data: u8) {
        self.strobe = (data & 1) != 0;
        if self.strobe {
            self.shifter = self.buttons;
        }
    }

    pub fn read(&mut self) -> u8 {
        let bit = self.shifter & 1;
        if !self.strobe {
            self.shifter >>= 1;
            self.shifter |= 0x80;
        }
        bit
    }
}

impl Bus {
    pub fn new(mapper: Box<dyn Mapper>, region: Region) -> Self {
        Self {
            clock: MasterClock::new(region),
            ram: [0; 0x800],
            ppu: Ppu::new(region),
            apu: Apu::new(region),
            mapper,
            controllers: [Controller::default(); 2],
            nmi_pending: false,
            irq_line: false,
            prev_irq_line: false,
            prev_nmi_pending: false,
            prev_nmi_flag: false,
            open_bus: 0,
            dmc_dma_active: false,
            oam_dma_active: false,
            oam_dma_idx: 0,
            audio_sink: None,
        }
    }

    pub fn region(&self) -> Region {
        self.clock.region()
    }

    /// One CPU bus read. Every read costs one CPU cycle.
    ///
    /// DMC DMA (if pending) is serviced **before** the read, matching real
    /// hardware: the CPU is halted via RDY, several stall cycles are
    /// inserted, the DMC fetches its sample byte, and only then does the
    /// CPU's read complete. DMA does not start during writes — a request
    /// raised during a write cycle waits for the next read.
    /// CPU bus read variant for the page-cross dummy read emitted by
    /// `abs,X` / `abs,Y` / `(zp),Y` indexed loads. Routes `$2007` to
    /// the PPU's `cpu_read_dummy` so the aborted read doesn't advance
    /// the internal buffer twice (see `blargg_apu_2005/...
    /// double_2007_read` and `Ppu::cpu_read_dummy`). Every other
    /// address behaves exactly like a normal `read`.
    pub fn dummy_read(&mut self, addr: u16) -> u8 {
        if !matches!(addr, 0x2000..=0x3FFF) {
            return self.read(addr);
        }
        self.service_pending_dmc_dma(addr);
        self.tick_pre_access(true);
        let value = self.ppu.cpu_read_dummy(addr, &mut *self.mapper);
        self.open_bus = value;
        self.tick_post_access(true);
        value
    }

    pub fn read(&mut self, addr: u16) -> u8 {
        // Service DMC DMA BEFORE this cycle's `tick_pre_access`, per
        // Mesen2's `MemoryRead` order
        // (`NesCpu.cpp:261-265`: `ProcessPendingDma → StartCpuCycle
        // → Read`). Halt cycle BECOMES the cycle the CPU would
        // have read; the post-service `tick_pre_access` is the
        // original read. Matches Mesen's cycle-count positioning
        // of the CPU's bus operation after DMA service.
        self.service_pending_dmc_dma(addr);
        self.tick_pre_access(true);
        let value = match addr {
            0x0000..=0x1FFF => self.ram[(addr & 0x07FF) as usize],
            0x2000..=0x3FFF => self.ppu.cpu_read(addr, &mut *self.mapper),
            0x4015 => self.apu.read_status(),
            0x4016 => 0x40 | (self.controllers[0].read() & 1),
            0x4017 => 0x40 | (self.controllers[1].read() & 1),
            0x4018..=0x401F => self.open_bus,
            // $4020-$5FFF is cartridge-claimable expansion space. Most
            // mappers (NROM / MMC1 / UxROM / CNROM / AxROM / MMC3)
            // don't decode it and leave `cpu_read_ex` defaulted to
            // `None`, so reads return open bus — the last value the
            // CPU put on the data lines. Required by
            // `cpu_exec_space/test_cpu_exec_space_apu` which fetches
            // opcodes from this region and expects the just-written
            // test-scaffold byte to come back. MMC5 (and eventually
            // FDS) override `cpu_read_ex` to supply real register
            // data for the `$5000-$5FFF` window.
            0x4020..=0x5FFF => self.mapper.cpu_read_ex(addr).unwrap_or(self.open_bus),
            0x6000..=0xFFFF => self.mapper.cpu_read(addr),
            _ => self.open_bus,
        };
        self.open_bus = value;
        self.tick_post_access(true);
        value
    }

    /// One CPU bus write. Every write costs one CPU cycle.
    pub fn write(&mut self, addr: u16, data: u8) {
        // Writes do NOT service DMC DMA. Mesen2's `MemoryWrite`
        // (`NesCpu.cpp:241-251`) deliberately omits any
        // `ProcessPendingDma` call — the DMC DMA always waits for
        // the next CPU bus *read* before halting. Our previous
        // `service_pending_dmc_dma_on_write` (3-cycle "halt absorbed
        // by write" branch, ostensibly from Nestopia `NstApu.cpp:
        // 2295`) was a misread of that code path: Nestopia's 3 vs 4
        // cost is *parity-driven*, not write-vs-read. With it gone,
        // the standalone DMA case in `service_pending_dmc_dma` does
        // the right thing on the next read.
        self.open_bus = data;
        self.tick_pre_access(false);
        match addr {
            0x0000..=0x1FFF => self.ram[(addr & 0x07FF) as usize] = data,
            0x2000..=0x3FFF => self.ppu.cpu_write(addr, data, &mut *self.mapper),
            0x4000..=0x4013 | 0x4015 | 0x4017 => self.apu.write_reg(addr, data),
            0x4014 => {
                // STA $4014's final write cycle.
                self.tick_post_access(false);
                // Snapshot the CPU's penultimate-cycle interrupt samples
                // so they survive OAM DMA's 513/514 stall cycles. Each
                // DMA cycle's `tick_pre_access` overwrites
                // `prev_irq_line`/`prev_nmi_pending`; without a
                // save/restore, STA's end-of-step poll would see
                // end-of-DMA state and mis-attribute any IRQ/NMI that
                // asserted during the DMA window to STA itself. The
                // next instruction's cycle-1 tick re-captures
                // `bus.irq_line` (which stays live across DMA) so a
                // DMA-window assertion still fires on the cycle after
                // DMA releases — matching puNES's explicit `irq.delay`
                // guard and Mesen2's lazy-DMA-inside-MemoryRead model.
                // Required by `cpu_interrupts_v2/4-irq_and_dma.nes`.
                let saved_prev_irq = self.prev_irq_line;
                let saved_prev_nmi = self.prev_nmi_pending;
                // Nesdev "OAM DMA": 513 CPU cycles total, plus one more
                // (514) if the DMA's halt cycle lands on a "put" (odd)
                // cycle and needs to be aligned to the following "get".
                // Our `cpu_cycles` counter is sampled AFTER STA's
                // `tick_post_access`, so it reflects the cycle just
                // completed; the halt cycle that follows runs at
                // `cpu_cycles + 1`. The extra alignment cycle is
                // therefore needed when `cpu_cycles` itself is EVEN
                // (halt lands on odd). Confirmed against
                // `cpu_interrupts_v2/4-irq_and_dma`: with the inverted
                // condition, the column 8→9 boundary lands on dly=527
                // as blargg's reference table requires.
                let extra_idle = (self.clock.cpu_cycles() & 1) == 0;
                self.run_oam_dma(data, extra_idle);
                self.prev_irq_line = saved_prev_irq;
                self.prev_nmi_pending = saved_prev_nmi;
                return;
            }
            0x4016 => {
                let strobe = data;
                self.controllers[0].write_strobe(strobe);
                self.controllers[1].write_strobe(strobe);
            }
            0x4018..=0x401F => {}
            0x4020..=0xFFFF => self.mapper.cpu_write(addr, data),
        }
        self.tick_post_access(false);
    }

    /// Peek without ticking — for debuggers/tracers only. Does not have
    /// bus side effects.
    pub fn peek(&self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x1FFF => self.ram[(addr & 0x07FF) as usize],
            0x6000..=0xFFFF => self.mapper.cpu_peek(addr),
            _ => 0,
        }
    }

    /// Side-effect peek — performs the same register state mutations a
    /// normal read would (controller shift, PPU buffer advance, frame
    /// IRQ clear on `$4015`, PPU status read-clear, etc.) WITHOUT
    /// advancing the master clock. Used by DMC DMA service to model
    /// Nestopia's `Peek(readAddress)` behavior when DMA collides with a
    /// CPU read in the same cycle (`NstApu.cpp:2313-2328`): the extra
    /// buffer advances / controller shifts happen instantaneously
    /// before the DMA's stall cycles run.
    fn peek_with_side_effects(&mut self, addr: u16) {
        match addr {
            0x2000..=0x3FFF => {
                let _ = self.ppu.cpu_read(addr, &mut *self.mapper);
            }
            0x4015 => {
                let _ = self.apu.read_status();
            }
            0x4016 => {
                let _ = self.controllers[0].read();
            }
            0x4017 => {
                let _ = self.controllers[1].read();
            }
            _ => {
                // Addresses without PPU/APU/controller side effects
                // (RAM, PRG-ROM, WRAM, unmapped). The DMA's presence
                // on the bus still broadcasts the address, but the
                // devices on those ranges don't latch it as a new
                // access.
            }
        }
    }

    /// Start-of-cycle phase — runs before the CPU's bus access.
    ///
    /// Drives the PPU via the master clock (`clock.start_cpu_cycle`)
    /// rather than a fixed per-cycle dot count. The number of PPU
    /// dots ticked here (0 through 2 on NTSC) depends on master-clock
    /// phase + `is_read`: in Mesen2's model
    /// (`NesCpu.cpp:73-75,317-322`) reads advance the master by 5
    /// then end by 7, writes by 7 then 5. The split is asymmetric so
    /// the PPU's dot positions within a CPU cycle shift based on
    /// master-clock phase — reproducing the dynamic 2/1 / 1/2 split
    /// that our old fixed 2/1 model couldn't. Required to move
    /// `dmc_dma_during_read4`'s iter alignment off the off-by-one
    /// position it was stuck at.
    ///
    /// APU + mapper tick here too (matching Mesen2's
    /// `ProcessCpuClock` call inside `StartCpuCycle`). The CPU
    /// interrupt-polling snapshot (`prev_irq_line`,
    /// `prev_nmi_pending`) is captured at the top — these reflect
    /// state at end of the **previous** cycle, which is what the
    /// 6502's penultimate-cycle polling expects.
    fn tick_pre_access(&mut self, is_read: bool) {
        self.prev_irq_line = self.irq_line;
        self.prev_nmi_pending = self.nmi_pending;

        let pre_ticks = self.clock.start_cpu_cycle(is_read);
        for _ in 0..pre_ticks {
            self.ppu.tick(&mut *self.mapper);
        }

        self.apu.tick_cpu_cycle();
        self.mapper.on_cpu_cycle();
        self.irq_line = self.apu.irq_line() | self.mapper.irq_line();
    }

    /// End-of-cycle phase — runs after the CPU's bus access.
    ///
    /// Ticks the remaining PPU dots for this cycle (1 or 2 on NTSC)
    /// via `clock.end_cpu_cycle`, then performs the rising-edge
    /// detection on the PPU's live `nmi_flag` to latch
    /// `nmi_pending`. Matches Mesen2 `NesCpu.cpp:294-315`.
    fn tick_post_access(&mut self, is_read: bool) {
        let post_ticks = self.clock.end_cpu_cycle(is_read);
        for _ in 0..post_ticks {
            self.ppu.tick(&mut *self.mapper);
        }
        let nmi_flag = self.ppu.nmi_flag();
        if !self.prev_nmi_flag && nmi_flag {
            self.nmi_pending = true;
        }
        self.prev_nmi_flag = nmi_flag;
        if let Some(sink) = self.audio_sink.as_mut() {
            sink.on_cpu_cycle(self.apu.output_sample());
        }
    }

    /// Combined tick entry — used for stall cycles inside OAM/DMC
    /// DMA that have no CPU-side access. Treated as a read cycle
    /// (matching Mesen2's `StartCpuCycle(true)` call inside its DMA
    /// stall path, `NesCpu.cpp:396-446`).
    fn tick_cycle(&mut self) {
        self.tick_pre_access(true);
        self.tick_post_access(true);
    }

    fn run_oam_dma(&mut self, page: u8, extra_idle: bool) {
        // 513 or 514 cycles beyond STA $4014's own 4 cycles:
        //   - 1 alignment idle (always)
        //   - 1 extra idle if the DMA began on an odd CPU cycle
        //   - 256 read/write pairs = 512 cycles
        //
        // `oam_dma_active` is set across the whole window so
        // `service_pending_dmc_dma` can cut its stall cycles from 4
        // to 2 when DMC DMA fires mid-OAM — the halt + dummy cycles
        // overlap with sprite-DMA's own bus-busy cycles (Nestopia
        // `DMC_R4014`, `NstApu.cpp:2297-2311`).
        self.oam_dma_active = true;
        self.oam_dma_idx = 0;
        self.tick_cycle();
        if extra_idle {
            self.tick_cycle();
        }
        let base = (page as u16) << 8;
        for i in 0..=0xFFu16 {
            self.oam_dma_idx = i;
            let byte = self.read(base | i);
            self.write(0x2004, byte);
        }
        self.oam_dma_active = false;
    }

    /// Consume a pending DMC DMA request, if any, and insert the stall
    /// cycles required to fetch the sample byte. Called at the top of
    /// [`Bus::read`] with the CPU's pending read address so the halt
    /// cycle can replay the read on the bus — real hardware behavior
    /// that causes one extra `$4016` / `$4017` shift when DMC DMA
    /// fires during a controller read (`dmc_dma_during_read4/
    /// dma_4016_read`).
    ///
    /// Cycle-count model — port of Mesen2's `ProcessPendingDma`
    /// (`NesCpu.cpp:325-448`). The DMA inserts:
    /// 1. **Halt** (1 cycle): dummy read of `pending_addr` (the
    ///    "controller double-read"). Skipped for `$4016`/`$4017` on
    ///    the post-halt loop iters but still happens here.
    /// 2. **Dummy** (1 cycle): always runs, regardless of parity.
    /// 3. **Align** (0 or 1 cycle): runs only if cycle parity after
    ///    the dummy is odd (put cycle) — DMC must read on a get/even
    ///    cycle.
    /// 4. **DMC read** (1 cycle): fetches the sample byte at
    ///    `DMC.current_addr`.
    ///
    /// Total: **3 cycles when entry-cycle is even, 4 when odd**.
    /// (Mesen2 derivation: at entry CycleCount=N, halt → N+1, dummy
    /// → N+2; the next get cycle is N+2 if even, else N+3 → DMC
    /// read.) This is what makes the standalone DMC DMA shorter than
    /// our previous hardcoded 4 — and matches the trace bisection
    /// against Mesen on `dma_4016_read.nes`.
    ///
    /// During OAM DMA the cycle counts shrink further because the
    /// halt/dummy cycles are absorbed into sprite-DMA's own bus-busy
    /// cycles. Until the OAM DMA loop is rewritten as an explicit
    /// get/put loop (next phase), we keep the Nestopia end-of-OAM
    /// taxonomy here as a stopgap.
    fn service_pending_dmc_dma(&mut self, pending_addr: u16) {
        if self.dmc_dma_active {
            return;
        }
        let Some(req) = self.apu.take_dmc_dma_request() else {
            return;
        };
        self.dmc_dma_active = true;

        let entry_cycle = self.clock.cpu_cycles();
        let is_4xxx = (pending_addr & 0xF000) == 0x4000;

        // Stall cycle count: Mesen2's parity-driven formula. Even
        // entry → 3 cycles; odd entry → 4 cycles. During OAM DMA we
        // fall back to the Nestopia end-of-OAM taxonomy until the
        // get/put loop rewrite lands.
        let stall_cycles = if self.oam_dma_active {
            match self.oam_dma_idx {
                253 => 1,
                255 => 3,
                _ => 2,
            }
        } else if (entry_cycle & 1) == 0 {
            3
        } else {
            4
        };

        // Pending-address side-effect peeks. In Mesen each non-DMC
        // stall cycle does an actual bus read of `pending_addr`,
        // which advances PPU buffer / shifts controller / clears
        // frame IRQ. We collapse those into instantaneous peeks
        // before the stall (Nestopia's `Peek+StealCycles` shape).
        //
        // Peek count = stall_cycles - 1 for non-`$4xxx` (the DMC
        // read cycle itself doesn't read `pending_addr`); 1 peek for
        // `$4016`/`$4017` (Mesen `skipDummyReads = true` for the
        // controllers, so only the halt shifts the latch).
        let peek_count = if is_4xxx {
            1
        } else {
            stall_cycles - 1
        };
        for _ in 0..peek_count {
            self.peek_with_side_effects(pending_addr);
        }

        for _ in 0..stall_cycles {
            self.tick_cycle();
        }

        let byte = self.mapper.cpu_read(req.addr);
        self.apu.dmc_dma_complete(byte);
        self.dmc_dma_active = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapper::nrom::Nrom;
    use crate::rom::{Cartridge, Mirroring, TvSystem};

    fn build_bus() -> Bus {
        let cart = Cartridge {
            prg_rom: vec![0u8; 0x8000],
            chr_rom: vec![0u8; 0x2000],
            chr_ram: false,
            mapper_id: 0,
            submapper: 0,
            mirroring: Mirroring::Vertical,
            battery_backed: false,
            prg_ram_size: 0x2000,
            tv_system: TvSystem::Ntsc,
            is_nes2: false,
            prg_chr_crc32: 0,
            db_matched: false,
        };
        Bus::new(Box::new(Nrom::new(cart)), Region::Ntsc)
    }

    // The OAM DMA cycle-count convention is tuned by
    // `cpu_interrupts_v2/4-irq_and_dma.nes`: we add the extra alignment
    // idle when `cpu_cycles` at end-of-STA is EVEN (i.e. the halt cycle
    // that follows lands on odd and needs to be aligned). Nesdev phrases
    // this as "total DMA = 513 cycles, +1 if starting on a put cycle";
    // the unit tests below pin the two branches so any future
    // `extra_idle` tweak must deliberately update this contract.

    #[test]
    fn oam_dma_halt_on_get_runs_513_beyond_sta() {
        let mut bus = build_bus();
        // cpu_cycles starts at 0. STA's tick_pre_access brings it to 1
        // (odd) for the match-arm parity check → extra_idle=false →
        // DMA = 513 cycles beyond STA's own cycle. Total ticks in
        // `write()` = 1 + 513 = 514.
        let before = bus.clock.cpu_cycles();
        bus.write(0x4014, 0x00);
        let dma_cycles = bus.clock.cpu_cycles() - before;
        assert_eq!(
            dma_cycles, 514,
            "STA + 1 halt + 512 pairs = 514 ticks (513-cycle DMA branch)"
        );
    }

    #[test]
    fn oam_dma_halt_on_put_runs_514_beyond_sta() {
        let mut bus = build_bus();
        // Tick once so cpu_cycles=1 before STA. STA's tick_pre_access
        // brings it to 2 (even) → extra_idle=true → DMA = 514 cycles
        // beyond STA. Total ticks in `write()` = 1 + 514 = 515.
        bus.tick_cycle();
        let before = bus.clock.cpu_cycles();
        bus.write(0x4014, 0x00);
        let dma_cycles = bus.clock.cpu_cycles() - before;
        assert_eq!(
            dma_cycles, 515,
            "STA + 1 halt + 1 align + 512 pairs = 515 ticks (514-cycle DMA branch)"
        );
    }
}
