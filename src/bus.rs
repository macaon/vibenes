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

    open_bus: u8,
    /// True while we're servicing a DMC DMA fetch. Prevents re-entering
    /// the DMA service from `tick_cycle` inside the stall cycles.
    dmc_dma_active: bool,
    /// Number of PPU dots left to run in the current CPU cycle's
    /// post-access phase. `tick_pre_access` advances the clock and
    /// runs all but the last dot; the remainder lives here until
    /// `tick_post_access` drains it. Carried as state (not a local)
    /// because the bus access between the two halves can make
    /// arbitrary register reads / writes.
    pending_ppu_ticks: u64,

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
            open_bus: 0,
            dmc_dma_active: false,
            pending_ppu_ticks: 0,
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
        self.tick_pre_access();
        let value = self.ppu.cpu_read_dummy(addr, &mut *self.mapper);
        self.open_bus = value;
        self.tick_post_access();
        value
    }

    pub fn read(&mut self, addr: u16) -> u8 {
        self.tick_pre_access();
        self.service_pending_dmc_dma(addr);
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
        // $2002 race: if the CPU read $2002 exactly during the VBlank-
        // start dot window, the PPU arms a suppression hint. Clear the
        // NMI that was latched in `tick_pre_access` before the CPU
        // sees it — matches real hardware where reading $2002 at the
        // race cycle cancels the frame's NMI.
        if self.ppu.take_nmi_suppress_hint() {
            self.nmi_pending = false;
        }
        self.tick_post_access();
        value
    }

    /// One CPU bus write. Every write costs one CPU cycle.
    pub fn write(&mut self, addr: u16, data: u8) {
        self.open_bus = data;
        self.tick_pre_access();
        match addr {
            0x0000..=0x1FFF => self.ram[(addr & 0x07FF) as usize] = data,
            0x2000..=0x3FFF => self.ppu.cpu_write(addr, data, &mut *self.mapper),
            0x4000..=0x4013 | 0x4015 | 0x4017 => self.apu.write_reg(addr, data),
            0x4014 => {
                // STA $4014's final write cycle.
                self.tick_post_access();
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
        self.tick_post_access();
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

    /// First half of a CPU cycle — runs before the bus access.
    ///
    /// Advances the master clock and ticks the PPU so register accesses
    /// see PPU state as-of the middle of the cycle (matching real 6502
    /// ↔ 2C02 interactions). Also captures the penultimate-cycle
    /// interrupt snapshot the CPU polls at end-of-instruction. Critical
    /// for blargg CPU-interrupt tests (`2-nmi_and_brk`, `3-nmi_and_irq`)
    /// that time NMI recognition to specific PPU dots.
    fn tick_pre_access(&mut self) {
        self.prev_irq_line = self.irq_line;
        self.prev_nmi_pending = self.nmi_pending;

        // Let the PPU clear any per-cycle race markers (e.g.
        // `vbl_just_set`) before we tick it for this CPU cycle. The
        // markers re-arm if the corresponding dot is ticked in this
        // cycle's PPU advance.
        self.ppu.begin_cpu_cycle();

        // NTSC: 3 PPU dots per CPU cycle, split 2 pre-access + 1
        // post-access to match Mesen2's phase alignment (see
        // `NesCpu.cpp:73-75,296,319` for the master-clock math that
        // yields a 2/1 split in steady state). Required by
        // `cpu_interrupts_v2/3-nmi_and_irq`: when dot 1 of scanline 241
        // is the *third* PPU dot of a CPU cycle, our old 3/0 split
        // made VBL visible to a `bit $2002` read on that same cycle,
        // so `sync_vbl` exited one cycle too early and every
        // downstream timing shifted by one iteration.
        //
        // PAL: 3 or 4 dots per cycle (ratio 1:3.2). Keep the same
        // rule — "all but the last dot" runs pre-access — so the PAL
        // variant of the same alignment behaves consistently.
        let ppu_ticks = self.clock.advance_cpu_cycle();
        self.pending_ppu_ticks = ppu_ticks;
        let pre_ticks = ppu_ticks.saturating_sub(1);
        for _ in 0..pre_ticks {
            self.ppu.tick(&mut *self.mapper);
        }
        self.pending_ppu_ticks -= pre_ticks;

        // APU ticks BEFORE the bus access so that:
        //   * a `$4015` read on the cycle the frame counter asserts
        //     `frame_irq` sees the flag set and clears it in one go
        //     (matches real hardware; `blargg_apu_2005.07.30/08.irq_timing`
        //     requires this to avoid dispatching IRQ one cycle early);
        //   * `apu.cycle` during the bus write equals the current CPU
        //     cycle (was lagging by one under the post-access model).
        // Mapper tick stays co-located with the APU tick so the IRQ-line
        // refresh sees both subsystems' latest state before the CPU's
        // penultimate-cycle poll snapshot is taken. NMI poll and audio
        // sampling run in `tick_post_access` — see that function's
        // comment for why the NMI poll is deferred.
        self.apu.tick_cpu_cycle();
        self.mapper.on_cpu_cycle();
        self.irq_line = self.apu.irq_line() | self.mapper.irq_line();
    }

    /// Second half of a CPU cycle — runs after the bus access.
    ///
    /// Runs the final PPU dot (the 3rd dot on NTSC / 4th on PAL when
    /// the ratio produces it) so register reads during the bus
    /// access see the PPU state as of "2 dots into this cycle". NMI
    /// poll lives here too so a `(241,1)` dot that lands as the
    /// final dot sets `nmi_pending` *after* this cycle's access —
    /// i.e. visible to the CPU no earlier than the next cycle's
    /// penultimate poll, matching Mesen2's
    /// `StartCpuCycle`/`EndCpuCycle` split.
    fn tick_post_access(&mut self) {
        for _ in 0..self.pending_ppu_ticks {
            self.ppu.tick(&mut *self.mapper);
        }
        self.pending_ppu_ticks = 0;
        if self.ppu.poll_nmi() {
            self.nmi_pending = true;
        }
        if let Some(sink) = self.audio_sink.as_mut() {
            sink.on_cpu_cycle(self.apu.output_sample());
        }
    }

    /// Old combined tick entry — kept for stall cycles inside OAM/DMC
    /// DMA that have no CPU-side access. These idle cycles still must
    /// advance clock/PPU/APU and refresh interrupt lines.
    fn tick_cycle(&mut self) {
        self.tick_pre_access();
        self.tick_post_access();
    }

    fn run_oam_dma(&mut self, page: u8, extra_idle: bool) {
        // 513 or 514 cycles beyond STA $4014's own 4 cycles:
        //   - 1 alignment idle (always)
        //   - 1 extra idle if the DMA began on an odd CPU cycle
        //   - 256 read/write pairs = 512 cycles
        self.tick_cycle();
        if extra_idle {
            self.tick_cycle();
        }
        let base = (page as u16) << 8;
        for i in 0..=0xFFu16 {
            let byte = self.read(base | i);
            self.write(0x2004, byte);
        }
    }

    /// Consume a pending DMC DMA request, if any, and insert the stall
    /// cycles required to fetch the sample byte. Called at the top of
    /// [`Bus::read`] with the CPU's pending read address so the halt
    /// cycle can replay the read on the bus — real hardware behavior
    /// that causes one extra `$4016` / `$4017` shift when DMC DMA
    /// fires during a controller read (`dmc_dma_during_read4/
    /// dma_4016_read`).
    ///
    /// Nesdev "DMC DMA" cycle model (`DMC_NORMAL` case, 4 cycles):
    /// 1. **Halt** — RDY drops; CPU still drives the bus and reads
    ///    its pending address one more time. This is the cycle that
    ///    produces the controller double-read bug.
    /// 2. **Dummy** — DMA controller has the bus; idle cycle with
    ///    no externally-visible side effects.
    /// 3. **Align** — optional extra idle to align to the DMC read.
    /// 4. **DMC read** — the sample byte fetch at `DMC.current_addr`
    ///    (always in `$8000..=$FFFF`, so a plain mapper read).
    ///
    /// We use the fixed 4-cycle worst case. puNES's more detailed
    /// 4-way taxonomy (`DMC_NORMAL` / `DMC_CPU_WRITE` / `DMC_R4014` /
    /// `DMC_NNL_DMA`) is a later-phase refinement; for the current
    /// test suite, replaying on the halt cycle only is sufficient.
    fn service_pending_dmc_dma(&mut self, pending_addr: u16) {
        if self.dmc_dma_active {
            return;
        }
        let Some(req) = self.apu.take_dmc_dma_request() else {
            return;
        };
        self.dmc_dma_active = true;
        // Mesen2's `skipDummyReads` distinction (`NesCpu.cpp:349-352,
        // 423-425`): on NES (not Famicom), `$4016`/`$4017` only
        // register the FIRST DMA dummy read as a controller shift;
        // subsequent DMA dummy reads don't pulse `/OE` through to the
        // controller latch. Every other address (including `$2007`)
        // registers each dummy read as a real bus transaction. We
        // model this by always replaying the halt cycle, then doing
        // ONE additional dummy-cycle replay only when the target is
        // not a controller register. `dmc_dma_during_read4/
        // dma_2007_read` wants two buffer advances (`33 44` or
        // `44 55`); `dma_4016_read` wants one bit deletion.
        let is_controller = matches!(pending_addr & 0xFFFE, 0x4016);
        // Cycle 1 — halt: always replays pending_addr. Full bus read
        // with side effects (open-bus, controller shift, $4015
        // frame-IRQ clear, PPU buffer advance).
        let _ = self.read(pending_addr);
        // Cycle 2 — dummy: replay at pending_addr again for
        // non-controller targets (the PPU $2007 buffer advances
        // every DMA read); idle for $4016/$4017.
        if is_controller {
            self.tick_cycle();
        } else {
            let _ = self.read(pending_addr);
        }
        // Cycle 3 — align: for non-controller targets, another bus
        // read at pending_addr (Nestopia `NstApu.cpp:2321-2327` does
        // two extra `Peek(readAddress)` calls plus the halt Peek =
        // 3 total buffer advances for $2007; puNES `apu.h:172` same
        // via `DMC_NORMAL` with 4 wall-clock cycles). For $4016/
        // $4017, `/OE` stays held through dummy cycles so the
        // controller only shifts on the halt cycle — align is idle.
        if is_controller {
            self.tick_cycle();
        } else {
            let _ = self.read(pending_addr);
        }
        // Cycle 4 — DMC read at the sample byte. Match Mesen2's
        // `StartCpuCycle → ProcessDmaRead → SetDmcReadBuffer →
        // EndCpuCycle` ordering (`NesCpu.cpp:396-411`): tick_pre
        // first (this cycle's APU tick happens before the fetch),
        // then mapper fetch, then commit (which asserts `dmc_irq`
        // on the last byte of a non-looping sample), then tick_post.
        // Shifts DMC-IRQ visibility by one cycle vs the prior
        // "commit-before-tick" order — required to align
        // `sync_dmc`'s BIT/$4015 poll with Mesen.
        self.tick_pre_access();
        let byte = self.mapper.cpu_read(req.addr);
        self.apu.dmc_dma_complete(byte);
        self.irq_line = self.apu.irq_line() | self.mapper.irq_line();
        self.tick_post_access();
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
