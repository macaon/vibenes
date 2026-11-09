// SPDX-License-Identifier: GPL-3.0-or-later
use crate::nes::apu::Apu;
use crate::audio::AudioSink;
use crate::nes::clock::{MasterClock, Region};
use crate::nes::mapper::Mapper;
use crate::nes::ppu::Ppu;

/// Central bus. Every CPU bus access passes through here and advances the
/// master clock; the clock then tells us how many PPU dots to run. This is
/// the mechanism that keeps the CPU/PPU/APU in lock-step.
pub struct Bus {
    pub clock: MasterClock,
    pub ram: [u8; 0x800],
    pub ppu: Ppu,
    pub apu: Apu,
    pub mapper: Box<dyn Mapper>,

    /// iNES / NES 2.0 mapper id of the currently-loaded cart. Captured
    /// at [`Bus::new`] so the save-state header (see
    /// [`crate::save_state`]) can validate that a loaded state was
    /// made for the same mapper variant - belt-and-suspenders against
    /// CRC32 collisions or a user pointing a state at the wrong ROM.
    mapper_id: u16,

    /// NES 2.0 submapper nibble. 0 for plain iNES dumps and for
    /// NES 2.0 carts whose submapper field is the default. Used by
    /// the save-sidecar header (see [`crate::save_header`]) to
    /// refuse cross-submapper loads where chip behavior diverges
    /// (e.g. UNROM-512 sub 1 / sub 3).
    submapper: u8,

    pub controllers: [Controller; 2],
    /// CPU-written OUT0 value, sampled by the controller output latch
    /// at the get-to-put boundary. Separate from each pad's strobe pin.
    controller_strobe_pending: bool,

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
    /// Internal CPU data latch, isolated from external data on $4015 reads.
    internal_bus: u8,

    /// DMA state machine - Mesen2 port (`NesCpu.cpp:325-448`). Both
    /// DMC DMA and OAM (sprite) DMA flow through the same parity-gated
    /// get/put loop in [`Bus::process_pending_dma`]. DMC arming inside
    /// `tick_pre_access` (after the APU ticks) sets `need_halt` and
    /// `need_dummy_read`; the next CPU read checks `need_halt` and, if
    /// set, runs the loop until both `dmc_dma_running` and
    /// `sprite_dma_running` are false. This is what lets DMC
    /// mid-OAM-DMA naturally hijack a sprite-DMA get cycle (the
    /// `sprdma_and_dmc_dma_512` 524-cycle pattern).
    ///
    /// `in_dma_loop` guards against nested entry: `tick_pre_access`
    /// inside a DMA cycle may detect a newly-armed DMC DMA, but the
    /// outer loop picks it up via the while-condition rather than
    /// recursively re-entering `process_pending_dma`.
    need_halt: bool,
    need_dummy_read: bool,
    dmc_dma_running: bool,
    dmc_dma_addr: u16,
    /// Mesen2 `NesCpu::_abortDmcDma` (`NesCpu.cpp:545`). Latched when
    /// the APU's `disable_delay` reaches zero while DMC DMA has
    /// already passed its halt cycle - the DMA still costs one halt
    /// cycle (which has already been taken), but the fetch is
    /// suppressed. Drained at the top of each DMA-loop iter; if pre-
    /// halt (`need_halt` still set) the cancellation is full and the
    /// flag never sees the loop.
    dmc_abort: bool,
    sprite_dma_running: bool,
    sprite_dma_page: u8,
    in_dma_loop: bool,
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

    pub(crate) fn save_state_capture(&self) -> crate::save_state::bus::ControllerSnap {
        crate::save_state::bus::ControllerSnap {
            buttons: self.buttons,
            strobe: self.strobe,
            shifter: self.shifter,
        }
    }

    pub(crate) fn save_state_apply(&mut self, snap: crate::save_state::bus::ControllerSnap) {
        self.buttons = snap.buttons;
        self.strobe = snap.strobe;
        self.shifter = snap.shifter;
    }
}

impl Bus {
    pub fn new(
        mapper: Box<dyn Mapper>,
        region: Region,
        mapper_id: u16,
        submapper: u8,
    ) -> Self {
        Self {
            clock: MasterClock::new(region),
            ram: [0; 0x800],
            ppu: Ppu::new(region),
            apu: Apu::new(region),
            mapper_id,
            submapper,
            mapper,
            controllers: [Controller::default(); 2],
            controller_strobe_pending: false,
            nmi_pending: false,
            irq_line: false,
            prev_irq_line: false,
            prev_nmi_pending: false,
            prev_nmi_flag: false,
            open_bus: 0,
            internal_bus: 0,
            need_halt: false,
            need_dummy_read: false,
            dmc_dma_running: false,
            dmc_dma_addr: 0,
            dmc_abort: false,
            sprite_dma_running: false,
            sprite_dma_page: 0,
            in_dma_loop: false,
            audio_sink: None,
        }
    }

    pub fn region(&self) -> Region {
        self.clock.region()
    }

    /// iNES / NES 2.0 mapper id of the cart loaded into this bus.
    /// Used by the save-state header to refuse cross-mapper loads.
    /// Current CPU-side data-bus latch (open bus). Updated on every
    /// CPU read/write. Used by trace tooling to capture what byte an
    /// unmapped-address read would return at any instruction boundary;
    /// critical for diagnosing dance-corruption bugs like the
    /// AccuracyCoin IDR test where wrong open-bus bytes at $4013-$4015
    /// derail the stack-crafted return path.
    pub fn open_bus(&self) -> u8 {
        self.open_bus
    }

    pub fn mapper_id(&self) -> u16 {
        self.mapper_id
    }

    /// NES 2.0 submapper of the cart loaded into this bus. 0 for
    /// plain iNES dumps. Used by the save-sidecar header to
    /// refuse cross-submapper loads.
    pub fn submapper(&self) -> u8 {
        self.submapper
    }

    /// Capture the bus's state into a serde-friendly shadow struct.
    /// Excludes `ppu`, `apu`, `mapper`, and `audio_sink` - those are
    /// siblings in the save-state tree (PPU/APU) or owned outside
    /// this snapshot (mapper, audio sink).
    pub(crate) fn save_state_capture(&self) -> crate::save_state::BusSnap {
        crate::save_state::BusSnap {
            clock: self.clock.save_state_capture(),
            ram: self.ram,
            controllers: [
                self.controllers[0].save_state_capture(),
                self.controllers[1].save_state_capture(),
            ],
            controller_strobe_pending: self.controller_strobe_pending,
            nmi_pending: self.nmi_pending,
            irq_line: self.irq_line,
            prev_irq_line: self.prev_irq_line,
            prev_nmi_pending: self.prev_nmi_pending,
            prev_nmi_flag: self.prev_nmi_flag,
            open_bus: self.open_bus,
            internal_bus: self.internal_bus,
            need_halt: self.need_halt,
            need_dummy_read: self.need_dummy_read,
            dmc_dma_running: self.dmc_dma_running,
            dmc_dma_addr: self.dmc_dma_addr,
            dmc_abort: self.dmc_abort,
            sprite_dma_running: self.sprite_dma_running,
            sprite_dma_page: self.sprite_dma_page,
            in_dma_loop: self.in_dma_loop,
            mapper_id: self.mapper_id,
        }
    }

    /// Restore the bus from a previously-captured snapshot. The
    /// `mapper_id` field is intentionally NOT applied - the live
    /// cart's mapper id is authoritative; the snap copy is purely
    /// diagnostic.
    pub(crate) fn save_state_apply(&mut self, snap: crate::save_state::BusSnap) {
        self.clock.save_state_apply(snap.clock);
        self.ram = snap.ram;
        self.controllers[0].save_state_apply(snap.controllers[0]);
        self.controllers[1].save_state_apply(snap.controllers[1]);
        self.controller_strobe_pending = snap.controller_strobe_pending;
        self.nmi_pending = snap.nmi_pending;
        self.irq_line = snap.irq_line;
        self.prev_irq_line = snap.prev_irq_line;
        self.prev_nmi_pending = snap.prev_nmi_pending;
        self.prev_nmi_flag = snap.prev_nmi_flag;
        self.open_bus = snap.open_bus;
        self.internal_bus = snap.internal_bus;
        self.need_halt = snap.need_halt;
        self.need_dummy_read = snap.need_dummy_read;
        self.dmc_dma_running = snap.dmc_dma_running;
        self.dmc_dma_addr = snap.dmc_dma_addr;
        self.dmc_abort = snap.dmc_abort;
        self.sprite_dma_running = snap.sprite_dma_running;
        self.sprite_dma_page = snap.sprite_dma_page;
        self.in_dma_loop = snap.in_dma_loop;
        // mapper_id intentionally NOT touched.
    }

    /// One CPU bus read. Every read costs one CPU cycle.
    ///
    /// DMC DMA (if pending) is serviced **before** the read, matching real
    /// hardware: the CPU is halted via RDY, several stall cycles are
    /// inserted, the DMC fetches its sample byte, and only then does the
    /// CPU's read complete. DMA does not start during writes - a request
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
        if self.need_halt {
            self.process_pending_dma(addr);
        }
        self.tick_pre_access(true);
        let value = self.ppu.cpu_read_dummy(addr, &mut *self.mapper);
        self.open_bus = value;
        self.internal_bus = value;
        self.finish_read_cycle(addr, value)
    }

    pub fn read(&mut self, addr: u16) -> u8 {
        // Process any pending DMA before the CPU's read, per Mesen2's
        // `MemoryRead` order (`NesCpu.cpp:261-265`: `ProcessPendingDma
        // → StartCpuCycle → Read`). `need_halt` is the flag the APU's
        // DMC or the OAM path raises; when set, `process_pending_dma`
        // runs the get/put loop until all DMA is drained, then returns
        // and the CPU's original read proceeds on the next cycle.
        if self.need_halt {
            self.process_pending_dma(addr);
        }
        self.tick_pre_access(true);
        // $4015 reads do NOT update the floating data bus latch on real
        // hardware - the APU's status read is one of the registers that
        // doesn't actually drive the bus. AccuracyCoin "Open Bus" #7
        // gates on this. Snapshot and restore around the match below.
        let preserved_open_bus = self.open_bus;
        let suppress_open_bus_update = matches!(addr, 0x4015);
        let value = match addr {
            0x0000..=0x1FFF => self.ram[(addr & 0x07FF) as usize],
            0x2000..=0x3FFF => self.ppu.cpu_read(addr, &mut *self.mapper),
            // $4015 bit 5 retains internal bus state; the APU drives
            // bits 0-4 / 6-7 without driving the external data pins.
            0x4015 => (self.apu.read_status() & 0xDF) | (self.internal_bus & 0x20),
            // $4016/$4017: bit 0 = controller, bits 1-4 = 0.
            // Bits 5-7 are undriven and retain the CPU open-bus value.
            // Bit 6 is usually set by the operand fetch, but stack writes
            // during JSR can clear it before the second operand read.
            0x4016 => (self.open_bus & 0xE0) | (self.controllers[0].read() & 1),
            0x4017 => (self.open_bus & 0xE0) | (self.controllers[1].read() & 1),
            0x4018..=0x401F => self.open_bus,
            // $4020-$5FFF is cartridge-claimable expansion space. Most
            // mappers (NROM / MMC1 / UxROM / CNROM / AxROM / MMC3)
            // don't decode it and leave `cpu_read_ex` defaulted to
            // `None`, so reads return open bus - the last value the
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
        self.internal_bus = value;
        if suppress_open_bus_update {
            self.open_bus = preserved_open_bus;
        } else {
            self.open_bus = value;
        }
        self.finish_read_cycle(addr, value)
    }

    /// Sample live PPU status outputs at the end of a read while retaining
    /// the VBlank value and side effects from read assertion. All mirrors
    /// and DMA reads use the same two edges; no extra PPU ticks are added.
    fn finish_read_cycle(&mut self, addr: u16, value: u8) -> u8 {
        self.tick_post_access(true);
        if (0x2000..=0x3FFF).contains(&addr) && addr & 7 == 2 {
            let value = self.ppu.finish_status_read(value);
            self.open_bus = value;
            self.internal_bus = value;
            value
        } else {
            value
        }
    }

    /// One CPU bus write. Every write costs one CPU cycle.
    pub fn write(&mut self, addr: u16, data: u8) {
        // Writes do NOT service DMC DMA. Mesen2's `MemoryWrite`
        // (`NesCpu.cpp:241-251`) deliberately omits any
        // `ProcessPendingDma` call - the DMC DMA always waits for
        // the next CPU bus *read* before halting. Our previous
        // `service_pending_dmc_dma_on_write` (3-cycle "halt absorbed
        // by write" branch, ostensibly from Nestopia `NstApu.cpp:
        // 2295`) was a misread of that code path: Nestopia's 3 vs 4
        // cost is *parity-driven*, not write-vs-read. With it gone,
        // the standalone DMA case in `service_pending_dmc_dma` does
        // the right thing on the next read.
        self.open_bus = data;
        self.internal_bus = data;
        if addr == 0x4010 {
            // The rate selector feeds a reload in this same CPU cycle.
            // Only stage rate bits here: IRQ/loop control retains the
            // normal register-write ordering below.
            self.apu.prepare_dmc_rate_write(data);
        }
        self.tick_pre_access(false);
        match addr {
            0x0000..=0x1FFF => self.ram[(addr & 0x07FF) as usize] = data,
            0x2000..=0x3FFF => self.ppu.cpu_write(addr, data, &mut *self.mapper),
            0x4000..=0x4013 | 0x4015 | 0x4017 => self.apu.write_reg(addr, data),
            0x4014 => {
                // RDY only halts reads. Consecutive writes (including
                // the second write of INC $4014) can replace this page
                // before the next CPU read supplies the frozen address.
                self.sprite_dma_running = true;
                self.sprite_dma_page = data;
                self.need_halt = true;
            }
            0x4016 => {
                self.controller_strobe_pending = data & 1 != 0;
            }
            0x4018..=0x401F => {}
            0x4020..=0xFFFF => self.mapper.cpu_write(addr, data),
        }
        self.tick_post_access(false);
    }

    /// Peek without ticking - for debuggers/tracers only. Does not have
    /// bus side effects.
    pub fn peek(&self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x1FFF => self.ram[(addr & 0x07FF) as usize],
            0x6000..=0xFFFF => self.mapper.cpu_peek(addr),
            _ => 0,
        }
    }

    /// Start-of-cycle phase - runs before the CPU's bus access.
    ///
    /// Drives the PPU via the master clock (`clock.start_cpu_cycle`)
    /// rather than a fixed per-cycle dot count. The number of PPU
    /// dots ticked here (0 through 2 on NTSC) depends on master-clock
    /// phase + `is_read`: in Mesen2's model
    /// (`NesCpu.cpp:73-75,317-322`) reads advance the master by 5
    /// then end by 7, writes by 7 then 5. The split is asymmetric so
    /// the PPU's dot positions within a CPU cycle shift based on
    /// master-clock phase - reproducing the dynamic 2/1 / 1/2 split
    /// that our old fixed 2/1 model couldn't. Required to move
    /// `dmc_dma_during_read4`'s iter alignment off the off-by-one
    /// position it was stuck at.
    ///
    /// APU + mapper tick here too (matching Mesen2's
    /// `ProcessCpuClock` call inside `StartCpuCycle`). The CPU
    /// interrupt-polling snapshot (`prev_irq_line`,
    /// `prev_nmi_pending`) is captured at the top - these reflect
    /// state at end of the **previous** cycle, which is what the
    /// 6502's penultimate-cycle polling expects.
    fn tick_pre_access(&mut self, is_read: bool) {
        // Sample OUT0 at the start of put, before that cycle's CPU
        // write. A high written on the preceding get reaches the pin;
        // a put-only high can be overwritten on get before sampling.
        if self.clock.cpu_cycles() & 1 == 1 {
            let strobe = u8::from(self.controller_strobe_pending);
            self.controllers[0].write_strobe(strobe);
            self.controllers[1].write_strobe(strobe);
        }
        self.prev_irq_line = self.irq_line;
        self.prev_nmi_pending = self.nmi_pending;

        let pre_ticks = self.clock.start_cpu_cycle(is_read);
        for _ in 0..pre_ticks {
            self.ppu.tick(&mut *self.mapper);
        }

        self.apu.tick_cpu_cycle();
        self.mapper.on_cpu_cycle();
        self.irq_line = self.apu.irq_line() | self.mapper.irq_line();

        // If the APU's DMC just disabled (its `disable_delay`
        // decremented to zero this tick), mirror Mesen2's
        // `StopDmcTransfer` (`NesCpu.cpp:534-548`): pre-halt cancels
        // the DMA outright, post-halt latches `dmc_abort` so the DMA
        // loop drops the fetch on its next iteration. Without this,
        // a $4015 bit-4 = 0 fired while the DMA was queued would
        // still pull the sample byte onto the open bus - producing
        // the wrong byte for AccuracyCoin's IDR dance.
        if self.apu.take_dmc_abort_request() {
            if self.need_halt {
                // Pre-halt: full cancel.
                self.dmc_dma_running = false;
                // OAM may be queued behind writes too; cancelling DMC
                // must not withdraw the shared halt request for OAM.
                self.need_halt = self.sprite_dma_running && !self.in_dma_loop;
                self.need_dummy_read = false;
            } else if self.dmc_dma_running {
                // Post-halt: late abort, drained inside the DMA loop.
                self.dmc_abort = true;
            }
        }

        // If the APU's DMC just armed a DMA this cycle, promote the
        // request into bus-side DMA state so the next CPU read (or
        // the running DMA loop) picks it up. Matches Mesen2's
        // `ApuClock → DmcProcessClock → StartDmcTransfer` pathway
        // (`DeltaModulationChannel.cpp:247-277`, `NesCpu.cpp:432-437`).
        if !self.dmc_dma_running {
            if let Some(req) = self.apu.take_dmc_dma_request() {
                self.dmc_dma_running = true;
                self.need_halt = true;
                self.need_dummy_read = true;
                self.dmc_dma_addr = req.addr;
            }
        }
    }

    /// End-of-cycle phase - runs after the CPU's bus access.
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
        // Re-OR the mapper's IRQ line into bus.irq_line. PPU ticks
        // in post-access can raise a mapper IRQ (MMC3 A12 counter
        // clocking on a sprite pattern fetch that lands in the
        // post-access PPU-dot window); without this refresh, the
        // rise wouldn't be visible to next cycle's `prev_irq_line`
        // snapshot, delaying CPU recognition by one full CPU cycle
        // (= 3 PPU cycles) - the `mmc3_test/4-scanline_timing #3`
        // symptom. APU IRQ state doesn't change in post-access
        // (APU ticks only in pre-access) so re-OR'ing it is a
        // no-op for the APU side.
        self.irq_line = self.apu.irq_line() | self.mapper.irq_line();
        let nmi_flag = self.ppu.nmi_flag();
        if !self.prev_nmi_flag && nmi_flag {
            self.nmi_pending = true;
        }
        self.prev_nmi_flag = nmi_flag;
        if let Some(sink) = self.audio_sink.as_mut() {
            // Linear blend: APU 2A03 in 0.0..≈0.98, plus any cart-side
            // expansion audio (FDS, VRC6, MMC5, N163, Sunsoft 5B) that
            // the mapper has pre-scaled against the 2A03's level range.
            // Non-audio mappers default-return `None` so the common
            // path is a branchless `unwrap_or(0.0)`.
            let apu_sample = self.apu.output_sample();
            let exp_sample = self.mapper.audio_output().unwrap_or(0.0);
            sink.on_cpu_cycle(apu_sample + exp_sample);
        }
    }


    /// Unified DMA processor - Mesen2 port (`NesCpu.cpp:325-448`).
    /// Handles DMC DMA, OAM (sprite) DMA, and their interleave in a
    /// single parity-gated get/put loop:
    ///
    /// - **Get cycles** (even `cpu_cycles`): DMC read if ready, else
    ///   sprite read, else dummy read of `pending_addr`.
    /// - **Put cycles** (odd): sprite write to `$2004` when a sprite
    ///   read just completed, else an alignment read of
    ///   `pending_addr`.
    ///
    /// The halt cycle is run outside the loop. `need_halt` /
    /// `need_dummy_read` are cleared one-at-a-time by the loop so the
    /// "halt → dummy → DMC-read" ordering inside the while is driven
    /// by parity, not hardcoded counts. When DMC fires mid-OAM, its
    /// fetch hijacks a get cycle that would have been a sprite read,
    /// which is what produces the per-iter 524 vs 525 cycle variance
    /// in `sprdma_and_dmc_dma_512` (can't be captured by a flat
    /// "standalone - 1" formula).
    ///
    /// Side-effect reads during halt / dummy / align cycles: for most
    /// `pending_addr` ranges the DMA issues a real bus read (PPU
    /// buffer advances on `$2007`, controller shifts on `$4016`/
    /// `$4017`). For the NES-flavour `$4016`/`$4017` case Mesen's
    /// `skipDummyReads` rule limits the shift to the halt cycle only
    /// - required by `dmc_dma_during_read4/dma_4016_read` (golden
    /// `08 08 07 08 08`).
    fn process_pending_dma(&mut self, pending_addr: u16) {
        if self.in_dma_loop {
            return;
        }
        self.in_dma_loop = true;

        // NES behaviour: subsequent controller reads during the DMA
        // are suppressed (only the halt cycle shifts the latch). Non-
        // `$4016`/`$4017` pending addresses get a real read on every
        // halt/dummy/align cycle, producing the multi-buffer-advance
        // behaviour for `$2007` and the single-shift behaviour for
        // the controllers.
        let skip_dummy_reads = pending_addr == 0x4016 || pending_addr == 0x4017;

        // Halt cycle - always runs once. `need_halt` is cleared
        // BEFORE `tick_pre_access` (Mesen2 line 354) so that if the
        // APU tick inside this cycle re-arms DMC (same-cycle buffer
        // drain after the halt is decided), the new `need_halt=true`
        // survives into the loop - the halt-cycle clear was for the
        // entering DMA's flag, not a new one that armed mid-cycle.
        self.need_halt = false;
        self.tick_pre_access(true);
        let value = self.dma_bus_read(pending_addr);
        self.finish_read_cycle(pending_addr, value);

        // Cancellation detected during the halt can still release RDY
        // after this single cycle. Later cancellations are observed by
        // a DMA cycle already selected below, which must finish first.
        if self.dmc_abort {
            self.dmc_abort = false;
            self.dmc_dma_running = false;
            self.need_dummy_read = false;
            self.need_halt = false;
            if !self.sprite_dma_running {
                self.in_dma_loop = false;
                return;
            }
        }

        let mut sprite_counter: u16 = 0;
        let mut sprite_byte: u8 = 0;

        while self.dmc_dma_running || self.sprite_dma_running {
            // A cancellation latched after the halt is observed by the
            // next DMA phase. Without OAM, drain that phase as an idle
            // read rather than removing a clock from the bus. With OAM,
            // its next transfer provides that phase without extra delay.
            if self.dmc_abort {
                self.dmc_abort = false;
                self.dmc_dma_running = false;
                self.need_dummy_read = false;
                self.need_halt = false;
                if !self.sprite_dma_running {
                    self.tick_pre_access(true);
                    if !skip_dummy_reads {
                        let value = self.dma_bus_read(pending_addr);
                        self.finish_read_cycle(pending_addr, value);
                    } else {
                        self.tick_post_access(true);
                    }
                    break;
                }
                continue;
            }

            let get_cycle = (self.clock.cpu_cycles() & 1) == 0;
            // Mesen's `processCycle` lambda - clear exactly one
            // pending flag (halt > dummy priority) per DMA cycle,
            // regardless of which branch actually runs. This is the
            // mechanism that lets need_halt / need_dummy_read burn
            // down while OAM DMA is servicing sprite reads, so the
            // DMC's halt cycle can overlap with a sprite read rather
            // than costing an extra standalone cycle.
            let clear_flag = if self.need_halt {
                self.need_halt = false;
                true
            } else if self.need_dummy_read {
                self.need_dummy_read = false;
                true
            } else {
                false
            };
            if get_cycle {
                if self.dmc_dma_running && !clear_flag {
                    // The cartridge sees the DMC address while register
                    // activation also depends on the halted CPU address.
                    // Keep that decoding in the fetch helper rather than
                    // treating this as an ordinary CPU register read.
                    self.tick_pre_access(true);
                    let byte = self.dmc_bus_read(self.dmc_dma_addr, pending_addr);
                    self.tick_post_access(true);
                    self.apu.dmc_dma_complete(byte);
                    self.dmc_dma_running = false;
                    self.dmc_abort = false;
                } else if self.sprite_dma_running {
                    // Sprite DMA read - sprite_counter indexes bytes
                    // 0..255 (the read slot of each read/write pair).
                    let sprite_addr =
                        ((self.sprite_dma_page as u16) << 8) | (sprite_counter >> 1);
                    self.tick_pre_access(true);
                    let value = self.oam_bus_read(sprite_addr, pending_addr);
                    sprite_byte = self.finish_read_cycle(sprite_addr, value);
                    sprite_counter = sprite_counter.wrapping_add(1);
                } else {
                    // DMC waiting, no sprite DMA - dummy read of
                    // pending_addr (clear_flag above already
                    // consumed one pending flag).
                    self.tick_pre_access(true);
                    if !skip_dummy_reads {
                        let value = self.dma_bus_read(pending_addr);
                        self.finish_read_cycle(pending_addr, value);
                    } else {
                        self.tick_post_access(true);
                    }
                }
            } else {
                // Put cycle.
                if self.sprite_dma_running && (sprite_counter & 1) == 1 {
                    // Sprite write - commit the byte read last cycle
                    // to $2004.
                    self.tick_pre_access(false);
                    self.ppu.cpu_write(0x2004, sprite_byte, &mut *self.mapper);
                    self.open_bus = sprite_byte;
                    self.internal_bus = sprite_byte;
                    self.tick_post_access(false);
                    sprite_counter = sprite_counter.wrapping_add(1);
                    if sprite_counter >= 0x200 {
                        self.sprite_dma_running = false;
                    }
                } else {
                    // Alignment read - happens pre-sprite-DMA to
                    // land on an even get cycle, or between DMC
                    // halt/dummy and DMC read.
                    self.tick_pre_access(true);
                    if !skip_dummy_reads {
                        let value = self.dma_bus_read(pending_addr);
                        self.finish_read_cycle(pending_addr, value);
                    } else {
                        self.tick_post_access(true);
                    }
                }
            }
        }

        self.in_dma_loop = false;
    }

    /// OAM memory read with register selection from the halted CPU.
    fn oam_bus_read(&mut self, addr: u16, halted_addr: u16) -> u8 {
        let previous_internal = self.internal_bus;
        // External memory is selected by the actual DMA address. The
        // write-only/internal register window is otherwise open bus.
        let external = if (0x4000..=0x401F).contains(&addr) {
            self.open_bus
        } else {
            self.dma_bus_read(addr)
        };
        self.open_bus = external;
        let decode_source = self.clock.region() == Region::Ntsc
            || (0x4000..=0x401F).contains(&addr);
        if decode_source && (halted_addr & 0xFFE0) == 0x4000 {
            match addr & 0x1F {
                0x15 => {
                    // Status drives the internal OAM input, not the
                    // external bus. The following OAM put will drive it.
                    let value = (self.apu.read_status() & 0xDF) | (previous_internal & 0x20);
                    self.internal_bus = value;
                    return value;
                }
                0x16 | 0x17 => {
                    let port = (addr & 1) as usize;
                    self.open_bus = (external & 0xE0) | (self.controllers[port].read() & 1);
                }
                _ => {}
            }
        }
        self.internal_bus = self.open_bus;
        self.open_bus
    }

    /// DMC fetch with the 2A03's split register address decoder.
    /// The cartridge sees the DMA address, but register selection keeps
    /// A5-A15 from the halted CPU and takes A0-A4 from the DMA unit.
    fn dmc_bus_read(&mut self, addr: u16, halted_addr: u16) -> u8 {
        let previous_internal = self.internal_bus;
        let mut isolated = false;
        let sample = self.mapper.cpu_read(addr);
        self.open_bus = sample;
        if self.clock.region() == Region::Ntsc && (halted_addr & 0xFFE0) == 0x4000 {
            match addr & 0x1F {
                0x15 => {
                    // Status is internal: its side effect still occurs,
                    // but it must not replace the external sample byte.
                    self.internal_bus = (self.apu.read_status() & 0xDF) | (previous_internal & 0x20);
                    isolated = true;
                }
                0x16 | 0x17 => {
                    let port = (addr & 1) as usize;
                    self.open_bus = (sample & 0xE0) | (self.controllers[port].read() & 1);
                }
                _ => {}
            }
        }
        if !isolated {
            self.internal_bus = self.open_bus;
        }
        // The CPU-visible byte and DMC input are separate observations.
        // Keep the existing sample path here: electrical corruption of
        // the DMC byte during a controller conflict is not modeled yet.
        sample
    }

    /// Bus read as issued by the DMA unit (Mesen2 `ProcessDmaRead`,
    /// `NesCpu.cpp:450-467`). Performs the same side-effect match as
    /// [`Bus::read`] but without the `tick_pre_access` /
    /// `tick_post_access` calls - those are driven explicitly by the
    /// DMA loop so the cycle accounting stays in our hands.
    fn dma_bus_read(&mut self, addr: u16) -> u8 {
        let value = match addr {
            0x0000..=0x1FFF => self.ram[(addr & 0x07FF) as usize],
            0x2000..=0x3FFF => self.ppu.cpu_read(addr, &mut *self.mapper),
            0x4015 => (self.apu.read_status() & 0xDF) | (self.internal_bus & 0x20),
            0x4016 => (self.open_bus & 0xE0) | (self.controllers[0].read() & 1),
            0x4017 => (self.open_bus & 0xE0) | (self.controllers[1].read() & 1),
            0x4018..=0x401F => self.open_bus,
            0x4020..=0x5FFF => self.mapper.cpu_read_ex(addr).unwrap_or(self.open_bus),
            0x6000..=0xFFFF => self.mapper.cpu_read(addr),
            _ => self.open_bus,
        };
        self.internal_bus = value;
        if addr != 0x4015 {
            self.open_bus = value;
        }
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::mapper::nrom::Nrom;
    use crate::nes::rom::{Cartridge, Mirroring, TvSystem};

    fn build_bus() -> Bus {
        build_bus_with_sample(0, Region::Ntsc)
    }

    fn build_bus_with_sample(sample: u8, region: Region) -> Bus {
        let cart = Cartridge {
            prg_rom: vec![sample; 0x8000],
            chr_rom: vec![0u8; 0x2000],
            chr_ram: false,
            mapper_id: 0,
            submapper: 0,
            mirroring: Mirroring::Vertical,
            battery_backed: false,
            prg_ram_size: 0x2000,
            prg_nvram_size: 0,
            tv_system: TvSystem::Ntsc,
            is_nes2: false,
            prg_chr_crc32: 0,
            db_matched: false,
            fds_data: None,
        };
        Bus::new(Box::new(Nrom::new(cart)), region, 0, 0)
    }

    #[test]
    fn pre_render_oam_handoff_copies_all_nine_bytes_from_the_selected_row() {
        for region in [Region::Ntsc, Region::Pal] {
            for source in [0, 5] {
                let mut bus = build_bus_with_sample(0, region);
                let mut ppu = bus.ppu.save_state_capture();
                ppu.scanline = region.pre_render_scanline();
                ppu.dot = 0;
                ppu.mask = 8;
                ppu.rendering_enabled = true;
                ppu.oam_rendering_enabled = true;
                ppu.oam2_addr = 4;
                ppu.oam_addr = source * 8 + 3;
                ppu.oam.fill(0xFF);
                ppu.secondary_oam.fill(0xAA);
                for byte in 0..8 {
                    ppu.oam[source as usize * 8 + byte] = 0x80 + byte as u8;
                }
                ppu.secondary_oam[source as usize] = 0x37;
                let original = ppu.oam;
                bus.ppu.save_state_apply(ppu);
                bus.ppu.tick(&mut *bus.mapper);
                let after = bus.ppu.save_state_capture();
                let mut expected = original;
                if region == Region::Ntsc {
                    expected[32..40].copy_from_slice(
                        &original[source as usize * 8..source as usize * 8 + 8],
                    );
                    assert_eq!(after.secondary_oam[4], 0x37);
                } else {
                    assert_eq!(after.secondary_oam[4], 0xAA);
                }
                assert_eq!(after.oam, expected);
            }
        }
    }

    #[test]
    fn oam_row_selector_freezes_after_disable_and_survives_vblank_restore() {
        let mut bus = build_bus();
        let mut ppu = bus.ppu.save_state_capture();
        ppu.scanline = 0;
        ppu.dot = 7;
        ppu.mask = 8;
        ppu.rendering_enabled = true;
        ppu.oam_rendering_enabled = true;
        ppu.oam.fill(0xFF);
        ppu.oam[0] = 0;
        bus.ppu.save_state_apply(ppu);
        bus.ppu.cpu_write(0x2001, 0, &mut *bus.mapper);
        for _ in 0..20 {
            bus.ppu.tick(&mut *bus.mapper);
        }
        let mut frozen = bus.ppu.save_state_capture();
        assert_eq!(frozen.oam2_addr, 4); // Independent M2 pulse probe.
        assert!(!frozen.oam_rendering_enabled);
        assert_eq!(frozen.oam[32], 0xFF);
        frozen.scanline = 250;
        frozen.dot = 0;
        let encoded = bincode::serde::encode_to_vec(&frozen, bincode::config::standard()).unwrap();
        let (frozen, _): (crate::save_state::PpuSnap, usize) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        let mut restored = build_bus();
        restored.ppu.save_state_apply(frozen);
        restored.ppu.cpu_write(0x2001, 0x10, &mut *restored.mapper);
        for _ in 0..20 {
            restored.ppu.tick(&mut *restored.mapper);
        }
        let mut before = restored.ppu.save_state_capture();
        assert_eq!(before.oam[32], 0xFF);
        assert_eq!(before.oam2_addr, 4);
        before.scanline = 261;
        before.dot = 0;
        restored.ppu.save_state_apply(before);
        restored.ppu.tick(&mut *restored.mapper);
        assert_eq!(restored.ppu.debug_oam()[32], 0);
    }

    #[test]
    fn sprite_idle_leaves_a_safe_oam_row_and_one_rendering_component_keeps_clocking() {
        let mut bus = build_bus();
        let mut ppu = bus.ppu.save_state_capture();
        ppu.scanline = 0;
        ppu.dot = 317;
        ppu.mask = 0x18;
        ppu.rendering_enabled = true;
        ppu.oam_rendering_enabled = true;
        ppu.oam2_addr = 31;
        bus.ppu.save_state_apply(ppu);
        bus.ppu.cpu_write(0x2001, 8, &mut *bus.mapper);
        for _ in 0..6 {
            bus.ppu.tick(&mut *bus.mapper);
        }
        let after = bus.ppu.save_state_capture();
        assert_eq!(after.oam2_addr, 0);
        assert!(after.oam_rendering_enabled);
    }

    #[test]
    fn sprite_overflow_visibility_matches_visual_2c02() {
        // Chip probe: eight contiguous sprites do not overflow, nine
        // raise the flag at dot 131. A skipped candidate adds two dots.
        for (count, gap, expected) in [(8, 0, None), (9, 0, Some(131)), (9, 1, Some(133))] {
            let mut bus = build_bus();
            let mut ppu = bus.ppu.save_state_capture();
            ppu.scanline = 0;
            ppu.dot = 1;
            ppu.mask = 8;
            ppu.oam.fill(0xFF);
            for sprite in 0..8 {
                ppu.oam[sprite * 4..sprite * 4 + 4].fill(0);
            }
            if count == 9 {
                let start = (8 + gap) * 4;
                ppu.oam[start..start + 4].fill(0);
            }
            bus.ppu.save_state_apply(ppu);
            let mut first = None;
            while bus.ppu.dot() <= 256 {
                let dot = bus.ppu.dot();
                bus.ppu.tick(&mut *bus.mapper);
                if first.is_none() && bus.ppu.debug_status() & 0x20 != 0 {
                    first = Some(dot);
                }
                if Some(dot + 1) == expected {
                    // Persist a comparison that has completed but
                    // has not yet reached the overflow output latch.
                    let snap = bus.ppu.save_state_capture();
                    assert!(snap.sprite_overflow_pending);
                    let encoded =
                        bincode::serde::encode_to_vec(&snap, bincode::config::standard()).unwrap();
                    let (snap, _): (crate::save_state::PpuSnap, usize) =
                        bincode::serde::decode_from_slice(&encoded, bincode::config::standard())
                            .unwrap();
                    let mut restored = build_bus();
                    restored.ppu.save_state_apply(snap);
                    bus = restored;
                }
            }
            assert_eq!(first, expected, "count={count} gap={gap}");
        }
    }

    #[test]
    fn sprite_zero_hit_is_visible_one_dot_after_pixel_detection() {
        for x in [0, 8, 128, 254, 255] {
            let mut bus = build_bus();
            let mut ppu = bus.ppu.save_state_capture();
            ppu.scanline = 1;
            ppu.dot = x + 1;
            ppu.mask = 0x1E;
            ppu.bg_pat_lo = 0x8000;
            ppu.sprite_count = 1;
            ppu.sprite_x[0] = x as u8;
            ppu.sprite_pat_lo[0] = 0x80;
            ppu.sprite_is_zero[0] = true;
            bus.ppu.save_state_apply(ppu);
            bus.ppu.tick(&mut *bus.mapper);
            assert_eq!(bus.ppu.debug_status() & 0x40, 0, "x={x}");
            let pending = bus.ppu.save_state_capture();
            assert_eq!(pending.sprite_zero_pending, x != 255, "x={x}");

            // Restore between detection and visibility. Already-detected
            // hits propagate even if rendering is disabled in between.
            let mut restored = build_bus();
            restored.ppu.save_state_apply(pending);
            restored.ppu.cpu_write(0x2001, 0, &mut *restored.mapper);
            restored.ppu.tick(&mut *restored.mapper);
            assert_eq!(
                restored.ppu.debug_status() & 0x40,
                if x == 255 { 0 } else { 0x40 },
                "x={x}",
            );
            assert!(!restored.ppu.save_state_capture().sprite_zero_pending);
        }
    }

    #[test]
    fn pending_sprite_flags_do_not_survive_reset_or_pre_render_clear() {
        for reset in [false, true] {
            let mut bus = build_bus();
            let mut ppu = bus.ppu.save_state_capture();
            ppu.sprite_zero_pending = true;
            ppu.sprite_overflow_pending = true;
            ppu.scanline = 261;
            ppu.dot = 1;
            bus.ppu.save_state_apply(ppu);
            if reset {
                bus.ppu.reset();
            }
            bus.ppu.tick(&mut *bus.mapper);
            assert_eq!(bus.ppu.debug_status() & 0x60, 0);
            assert!(!bus.ppu.save_state_capture().sprite_zero_pending);
            assert!(!bus.ppu.save_state_capture().sprite_overflow_pending);
        }
    }

    #[test]
    fn ppu_status_samples_live_sprite_flags_but_latches_vblank() {
        // At phase offset 1 a read advances two dots before assertion
        // and one afterward. Only the middle case straddles dot 1.
        for (scanline, dot, expected) in [(260, 339, 0xFB), (260, 340, 0x9B), (261, 0, 0x1B)] {
            for path in 0..4 {
                let mut bus = build_bus();
                let mut clock = bus.clock.save_state_capture();
                clock.master_cycles = 12;
                clock.ppu_cycles = 2;
                bus.clock.save_state_apply(clock);
                let mut ppu = bus.ppu.save_state_capture();
                ppu.scanline = scanline;
                ppu.dot = dot;
                ppu.status = 0xE0;
                ppu.open_bus = 0x1B;
                ppu.w_latch = true;
                bus.ppu.save_state_apply(ppu);
                let before = bus.clock.ppu_cycles();
                let addr = 0x3FFA; // PPUSTATUS mirror
                let value = match path {
                    0 => bus.read(addr),
                    1 => bus.dummy_read(addr),
                    _ => {
                        bus.tick_pre_access(true);
                        let value = if path == 2 {
                            bus.dma_bus_read(addr)
                        } else {
                            bus.oam_bus_read(addr, 0)
                        };
                        bus.finish_read_cycle(addr, value)
                    }
                };
                assert_eq!(value, expected, "scanline={scanline} dot={dot} path={path}");
                assert_eq!(bus.clock.ppu_cycles() - before, 3);
                assert_eq!(bus.open_bus, expected);
                assert_eq!(bus.internal_bus, expected);
                let ppu = bus.ppu.save_state_capture();
                assert_eq!(ppu.open_bus, expected);
                assert!(!ppu.w_latch);
                assert_eq!(ppu.status & 0x80, 0);
            }
        }
    }

    #[test]
    fn status_reads_update_internal_bus_without_driving_external_bus() {
        let mut bus = build_bus();
        bus.write(0, 0x20);
        let mut snap = bus.apu.save_state_capture();
        snap.dmc.bytes_remaining = 1;
        snap.dmc.timer = 100;
        bus.apu.save_state_apply(snap);
        assert_eq!(bus.read(0x4015), 0x30);
        assert_eq!(bus.internal_bus, 0x30);
        assert_eq!(bus.open_bus, 0x20);
        assert_eq!(bus.read(0x4000), 0x20);
    }

    #[test]
    fn dmc_status_alias_isolates_internal_bit_five_from_sample_data() {
        for seed in [0, 0x20] {
            for dma_addr in [0xC014, 0xC015] {
                let sample = seed ^ 0x20;
                let mut bus = build_bus_with_sample(sample, Region::Ntsc);
                bus.write(0, seed);
                let mut snap = bus.apu.save_state_capture();
                snap.dmc.bytes_remaining = 1;
                snap.dmc.timer = 100;
                bus.apu.save_state_apply(snap);
                bus.dmc_dma_running = true;
                bus.need_halt = true;
                bus.need_dummy_read = true;
                bus.dmc_dma_addr = dma_addr;
                let expected = if dma_addr == 0xC015 { seed } else { sample };
                assert_eq!(bus.read(0x4015) & 0x20, expected);
                assert_eq!(bus.open_bus, sample);
                assert_eq!(bus.read(0x4000), sample);
                assert_eq!(bus.read(0x4015) & 0x20, sample);
            }
        }
    }

    #[test]
    fn oam_and_halt_status_reads_preserve_bus_isolation() {
        let mut bus = build_bus_with_sample(0, Region::Ntsc);
        bus.write(0, 0x20);
        assert_eq!(bus.oam_bus_read(0xC015, 0x4000), 0x20);
        assert_eq!(bus.open_bus, 0);
        assert_eq!(bus.dma_bus_read(0x4015), 0x20);
        assert_eq!(bus.open_bus, 0);
        assert_eq!(bus.read(0x4000), 0);
    }

    #[test]
    fn distinct_internal_and_external_bus_values_survive_restore() {
        let mut bus = build_bus_with_sample(0, Region::Ntsc);
        bus.write(0, 0x20);
        bus.dmc_bus_read(0xC015, 0x4015);
        assert_eq!(bus.internal_bus & 0x20, 0x20);
        assert_eq!(bus.open_bus, 0);
        let mut restored = build_bus();
        restored.save_state_apply(bus.save_state_capture());
        restored.apu.save_state_apply(bus.apu.save_state_capture());
        assert_eq!(restored.read(0x4015) & 0x20, 0x20);
        assert_eq!(restored.read(0x4000), 0);
    }

    #[test]
    fn implicit_abort_stalls_one_read_but_is_suppressed_by_a_write() {
        for intervening_write in [false, true] {
            let mut bus = build_bus_with_sample(0x7B, Region::Ntsc);
            let mut snap = bus.apu.save_state_capture();
            snap.dmc.bytes_remaining = 1;
            snap.dmc.bits_remaining = 1;
            snap.dmc.timer = 1;
            bus.apu.save_state_apply(snap);
            bus.apu.dmc_dma_complete(0x7B);
            bus.read(0x8000);
            bus.read(0x8000); // Consume buffer and request the aborted reload.
            assert!(bus.need_halt);
            if intervening_write {
                bus.write(0, 0);
            }
            let before = bus.clock.cpu_cycles();
            bus.read(0x8000);
            assert_eq!(
                bus.clock.cpu_cycles().wrapping_sub(before),
                if intervening_write { 1 } else { 2 }
            );
            assert!(!bus.dmc_dma_running);
            assert!(bus.apu.save_state_capture().dmc.buffer.is_none());
        }
    }

    #[test]
    fn write_delayed_dmc_abort_does_not_stall_the_following_read() {
        let mut bus = build_bus_with_sample(0x7B, Region::Ntsc);
        let mut snap = bus.apu.save_state_capture();
        snap.dmc.disable_delay = 1;
        snap.dmc.bytes_remaining = 1;
        snap.dmc.timer = 100;
        bus.apu.save_state_apply(snap);
        bus.dmc_dma_running = true;
        bus.need_halt = true;
        bus.need_dummy_read = true;
        bus.dmc_dma_addr = 0xC000;
        bus.write(0, 0);
        let before = bus.clock.cpu_cycles();
        bus.read(0x8000);
        assert_eq!(bus.clock.cpu_cycles().wrapping_sub(before), 1);
        assert!(bus.apu.save_state_capture().dmc.buffer.is_none());
        assert!(!bus.dmc_abort);
    }

    #[test]
    fn dmc_abort_finishes_the_selected_cycle_on_both_alignments() {
        for halt_on_get in [false, true] {
            for abort_after in 1..=3 {
                let mut bus = build_bus_with_sample(0x7B, Region::Ntsc);
                if halt_on_get {
                    bus.read(0);
                }
                let mut snap = bus.apu.save_state_capture();
                snap.dmc.disable_delay = abort_after;
                snap.dmc.bytes_remaining = 1;
                snap.dmc.timer = 100;
                bus.apu.save_state_apply(snap);
                bus.dmc_dma_running = true;
                bus.need_halt = true;
                bus.need_dummy_read = true;
                bus.dmc_dma_addr = 0xC000;
                let before = bus.clock.cpu_cycles();
                bus.read(0x8000);
                let stall = bus.clock.cpu_cycles().wrapping_sub(before) - 1;
                let expected = match (halt_on_get, abort_after) {
                    (_, 1) => 1,
                    (_, 2) | (true, 3) => 3,
                    (false, 3) => 4,
                    _ => unreachable!(),
                };
                assert_eq!(stall, expected,
                    "halt_on_get={halt_on_get}, abort_after={abort_after}");
                assert!(!bus.dmc_abort, "abort must not poison the next DMA");
                assert!(!bus.dmc_dma_running);
                let fetched = bus.apu.save_state_capture().dmc.buffer.is_some();
                assert_eq!(fetched, halt_on_get && abort_after == 3);
            }
        }
    }

    #[test]
    fn dmc_abort_latch_and_disable_countdown_survive_restore() {
        for (latched, pending, delay, expected_cycles) in [
            (true, false, 0, 2),
            (false, false, 2, 4),
            (false, true, 0, 2),
        ] {
            let mut bus = build_bus_with_sample(0x7B, Region::Ntsc);
            bus.dmc_abort = latched;
            bus.dmc_dma_running = true;
            bus.need_halt = true;
            bus.need_dummy_read = true;
            bus.dmc_dma_addr = 0xC000;
            let mut apu = bus.apu.save_state_capture();
            apu.dmc.disable_delay = delay;
            apu.dmc.abort_pending = pending;
            apu.dmc.timer = 100;
            bus.apu.save_state_apply(apu);
            let mut restored = build_bus_with_sample(0x7B, Region::Ntsc);
            restored.save_state_apply(bus.save_state_capture());
            restored.apu.save_state_apply(bus.apu.save_state_capture());
            for candidate in [&mut bus, &mut restored] {
                let before = candidate.clock.cpu_cycles();
                candidate.read(0x8000);
                assert_eq!(candidate.clock.cpu_cycles().wrapping_sub(before), expected_cycles);
                assert!(candidate.apu.save_state_capture().dmc.buffer.is_none());
                assert!(!candidate.dmc_abort);
            }
        }
    }

    #[test]
    fn pal_oam_direct_status_read_still_requires_cpu_register_window() {
        let mut bus = build_bus_with_sample(0, Region::Pal);
        bus.apu.set_frame_irq_for_test(true);
        bus.open_bus = 0;
        assert_eq!(bus.oam_bus_read(0x4015, 0x8000), 0);
        assert_eq!(bus.oam_bus_read(0x4015, 0x4000) & 0x40, 0x40);
    }

    #[test]
    fn dmc_disable_does_not_cancel_a_queued_oam_halt() {
        let mut bus = build_bus();
        bus.ram[0x200] = 0x7B;
        bus.write(0x4015, 0); // Delayed DMC cancellation.
        bus.write(0x4014, 2);
        bus.write(0, 0); // Cancellation occurs while OAM is waiting.
        assert!(bus.need_halt);
        bus.read(0x8000);
        assert!(!bus.sprite_dma_running);
        assert_eq!(bus.ppu.debug_oam()[0], 0x7B);
    }

    #[test]
    fn queued_oam_dma_survives_restore_before_the_halting_read() {
        let mut bus = build_bus();
        bus.ram[0x200] = 0x7B;
        bus.write(0x4014, 2);
        let mut restored = build_bus();
        restored.save_state_apply(bus.save_state_capture());
        assert!(restored.sprite_dma_running);
        restored.read(0x8000);
        assert!(!restored.sprite_dma_running);
        assert_eq!(restored.ppu.debug_oam()[0], 0x7B);
    }

    #[test]
    fn oam_dma_waits_for_read_and_uses_the_last_written_page() {
        let mut bus = build_bus();
        bus.ram[0x200] = 0x7B;
        let before = bus.clock.cpu_cycles();
        bus.write(0x4014, 0);
        bus.write(0x4014, 2);
        assert_eq!(bus.clock.cpu_cycles().wrapping_sub(before), 2);
        assert!(bus.sprite_dma_running);
        bus.read(0x8000);
        assert!(!bus.sprite_dma_running);
        assert_eq!(bus.ppu.debug_oam()[0], 0x7B);
    }

    #[test]
    fn oam_page_40_does_not_activate_registers_when_cpu_is_outside_window() {
        let mut bus = build_bus();
        bus.apu.set_frame_irq_for_test(true);
        bus.controllers[0].shifter = 0xA5;
        bus.controllers[1].shifter = 0x5A;
        bus.write(0x4014, 0x40);
        bus.read(0x8000);
        assert!(bus.apu.irq_line());
        assert_eq!(bus.controllers[0].shifter, 0xA5);
        assert_eq!(bus.controllers[1].shifter, 0x5A);
    }

    #[test]
    fn oam_register_decode_uses_cpu_window_and_dma_low_bits() {
        let mut bus = build_bus_with_sample(0xFF, Region::Ntsc);
        bus.apu.set_frame_irq_for_test(true);
        assert_eq!(bus.oam_bus_read(0xC015, 0x4000) & 0x40, 0x40);
        assert_eq!(bus.open_bus, 0xFF, "status must not drive external bus");
        bus.controllers[0].shifter = 1;
        assert_eq!(bus.oam_bus_read(0xC016, 0x4000), 0xE1);
        assert_eq!(bus.oam_bus_read(0xC017, 0x4000), 0xE0);
    }

    #[test]
    fn dmc_conflicts_use_halted_cpu_decode_and_dma_low_address_bits() {
        for sample in [0, 0xFF] {
            for halted_addr in [0x3FFF, 0x4000, 0x401F, 0x4020, 0x8000] {
                for dma_addr in [0xC016, 0xFF16, 0xC017, 0xFF17] {
                    let mut bus = build_bus_with_sample(sample, Region::Ntsc);
                    bus.controllers[0].shifter = 0xA5;
                    bus.controllers[1].shifter = 0x5A;
                    let port = (dma_addr & 1) as usize;
                    let before = bus.controllers.map(|pad| pad.shifter);
                    let active = (0x4000..=0x401F).contains(&halted_addr);
                    bus.dmc_bus_read(dma_addr, halted_addr);
                    let expected = if active {
                        (sample & 0xE0) | (before[port] & 1)
                    } else {
                        sample
                    };
                    assert_eq!(bus.open_bus, expected);
                    for i in 0..2 {
                        let expected_shift = if active && i == port {
                            (before[i] >> 1) | 0x80
                        } else {
                            before[i]
                        };
                        assert_eq!(bus.controllers[i].shifter, expected_shift);
                    }
                }
            }
        }
    }

    #[test]
    fn dmc_status_conflict_clears_irq_without_driving_external_bus() {
        let mut bus = build_bus_with_sample(0xA5, Region::Ntsc);
        bus.apu.set_frame_irq_for_test(true);
        bus.dmc_bus_read(0xFF15, 0x4000);
        assert_eq!(bus.open_bus, 0xA5);
        assert!(bus.apu.irq_line(), "status read defers clearing until get");
        bus.read(0);
        bus.read(0);
        assert!(!bus.apu.irq_line());
    }

    #[test]
    fn dmc_fetch_populates_cpu_bus_with_controller_conflict_value() {
        let mut bus = build_bus_with_sample(0xFF, Region::Ntsc);
        bus.controllers[0].shifter = 0xFF;
        bus.dmc_dma_running = true;
        bus.need_halt = true;
        bus.need_dummy_read = true;
        bus.dmc_dma_addr = 0xC016;
        assert_eq!(bus.read(0x4000), 0xE1);
    }

    #[test]
    fn ntsc_dmc_controller_conflict_is_not_applied_to_pal() {
        let mut bus = build_bus_with_sample(0xFF, Region::Pal);
        bus.controllers[0].shifter = 0xA5;
        bus.dmc_bus_read(0xC016, 0x4000);
        assert_eq!(bus.open_bus, 0xFF);
        assert_eq!(bus.controllers[0].shifter, 0xA5);
    }

    #[test]
    fn controller_strobe_ignores_upper_write_bits() {
        let mut bus = build_bus();
        bus.controllers[0].shifter = 0xFF;
        bus.write(0x4016, 2);
        bus.read(0);
        assert_eq!(bus.controllers[0].shifter, 0xFF);
        assert!(!bus.controllers[0].strobe);
        bus.write(0x4016, 3);
        bus.read(0);
        bus.read(0);
        assert!(bus.controllers[0].strobe);
        assert_eq!(bus.controllers[0].shifter, 0);
    }

    #[test]
    fn controller_strobe_samples_the_previous_get_write_on_put() {
        for high_on_put in [false, true] {
            let mut bus = build_bus();
            for pad in &mut bus.controllers {
                pad.shifter = 0xFF;
            }
            bus.controllers[0].buttons = 0xA5;
            bus.controllers[1].buttons = 0x5A;
            // The next cycle is put when the completed count is odd.
            if ((bus.clock.cpu_cycles() & 1) == 1) != high_on_put {
                bus.read(0);
            }
            bus.write(0x4016, 1);
            bus.write(0x4016, 0);
            bus.read(0); // Let the low level reach the output latch.
            for (port, buttons) in [(0x4016, 0xA5), (0x4017, 0x5A)] {
                let mut report = 0u8;
                for bit in 0..8 {
                    report |= (bus.read(port) & 1) << bit;
                }
                assert_eq!(report, if high_on_put { 0xFF } else { buttons });
                assert_eq!(bus.read(port) & 1, 1, "report must end with ones");
            }
        }
    }

    #[test]
    fn controller_strobe_held_across_put_is_sampled() {
        let mut bus = build_bus();
        // First write on get, then leave it high for the following put.
        if bus.clock.cpu_cycles() & 1 != 0 {
            bus.read(0);
        }
        bus.controllers[0].buttons = 0x96;
        bus.write(0x4016, 1);
        bus.read(0);
        bus.write(0x4016, 0);
        bus.read(0);
        assert_eq!(bus.controllers[0].shifter, 0x96);
        assert!(!bus.controllers[0].strobe);
    }

    #[test]
    fn pending_controller_strobe_survives_restore() {
        let mut bus = build_bus();
        if bus.clock.cpu_cycles() & 1 != 0 {
            bus.read(0);
        }
        bus.controllers[0].buttons = 0x96;
        bus.write(0x4016, 1); // On get: pending high, actual OUT0 still low.
        assert!(!bus.controllers[0].strobe);
        let mut restored = build_bus();
        restored.save_state_apply(bus.save_state_capture());
        restored.read(0); // Following put commits the saved pending level.
        assert!(restored.controllers[0].strobe);
        assert_eq!(restored.controllers[0].shifter, 0x96);
    }

    #[test]
    fn dmc_rate_write_selects_reload_without_restarting_timer() {
        for (old_period, new_rate, new_period) in [(53, 0, 427), (427, 15, 53)] {
            // Sweep writes three cycles before expiry through two cycles
            // after it, in both slow-to-fast and fast-to-slow directions.
            for offset in -3i32..=2 {
                let mut bus = build_bus();
                let mut snap = bus.apu.save_state_capture();
                snap.dmc.period = old_period;
                snap.dmc.timer = (-offset).max(0) as u16;
                snap.dmc.bits_remaining = 8;
                bus.apu.save_state_apply(snap);
                for _ in 0..offset.max(0) {
                    bus.read(0);
                }
                bus.write(0x4010, new_rate);
                let after = bus.apu.save_state_capture();
                let expected_timer = if offset < 0 {
                    (-offset - 1) as u16
                } else if offset == 0 {
                    new_period
                } else {
                    old_period - offset as u16
                };
                assert_eq!(
                    after.dmc.timer, expected_timer,
                    "old period={old_period}, offset={offset}"
                );
                assert_eq!(after.dmc.period, new_period);
                assert_eq!(
                    after.dmc.bits_remaining, if offset < 0 { 8 } else { 7 },
                    "a rate write must not clock an extra output bit"
                );
            }
        }
    }

    #[test]
    fn frame_irq_status_double_reads_on_get_and_put() {
        for first_read_on_get in [false, true] {
            let mut bus = build_bus();
            // DMA get cycles start with an even completed-cycle count.
            if ((bus.clock.cpu_cycles() & 1) == 0) != first_read_on_get {
                bus.read(0);
            }
            bus.apu.set_frame_irq_for_test(true);
            assert_eq!(bus.read(0x4015) & 0x40, 0x40);
            assert_eq!(
                bus.read(0x4015) & 0x40,
                if first_read_on_get { 0x40 } else { 0 },
                "first read on get: {first_read_on_get}"
            );
            assert_eq!(bus.read(0x4015) & 0x40, 0);
        }
    }

    #[test]
    fn controller_cpu_and_dma_reads_preserve_all_three_open_bus_bits() {
        let mut bus = build_bus();
        for addr in [0x4016, 0x4017] {
            for upper in (0..=0xE0).step_by(0x20) {
                bus.open_bus = upper;
                assert_eq!(bus.read(addr) & 0xFE, upper);
                bus.open_bus = upper;
                assert_eq!(bus.dma_bus_read(addr) & 0xFE, upper);
            }
        }
    }

    #[test]
    fn jsr_through_apu_status_uses_stack_write_for_controller_operand() {
        let mut bus = build_bus();
        let mut cpu = crate::nes::cpu::Cpu::new();
        cpu.pc = 0x4015;
        cpu.sp = 0xFD;
        // Clock P1 completely so its next serial bit is 1; leave P2 at 0.
        for _ in 0..8 {
            bus.controllers[0].read();
        }
        // $4015 supplies JSR ($20), retaining the external bus's $60.
        // P1 then supplies low operand $61. JSR pushes return address
        // $4017 before reading P2, so its high operand must be $00.
        bus.write(0, 0x60);
        cpu.step(&mut bus).unwrap();
        assert_eq!(cpu.pc, 0x0061);
        assert_eq!(bus.ram[0x1FD], 0x40);
        assert_eq!(bus.ram[0x1FC], 0x17);
    }

    // OAM DMA is armed by a write and serviced by the next read.
    // The read takes 513 or 514 DMA cycles plus its own access cycle.
    #[test]
    fn oam_dma_halt_on_get_runs_513_beyond_sta() {
        let mut bus = build_bus();
        let _ = bus.read(0x0000);
        bus.write(0x4014, 0x00);
        let before = bus.clock.cpu_cycles();
        bus.read(0x8000);
        let dma_cycles = bus.clock.cpu_cycles() - before;
        assert_eq!(
            dma_cycles, 514,
            "resumed read + 1 halt + 512 pairs = 514 ticks (513-cycle DMA branch)"
        );
    }

    #[test]
    fn oam_dma_halt_on_put_runs_514_beyond_sta() {
        let mut bus = build_bus();
        bus.write(0x4014, 0x00);
        let before = bus.clock.cpu_cycles();
        bus.read(0x8000);
        let dma_cycles = bus.clock.cpu_cycles().wrapping_sub(before);
        assert_eq!(
            dma_cycles, 515,
            "resumed read + 1 halt + 1 align + 512 pairs = 515 ticks (514-cycle DMA branch)"
        );
    }
}
