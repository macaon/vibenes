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
    pub fn new(mapper: Box<dyn Mapper>, region: Region, mapper_id: u16) -> Self {
        Self {
            clock: MasterClock::new(region),
            ram: [0; 0x800],
            ppu: Ppu::new(region),
            apu: Apu::new(region),
            mapper_id,
            mapper,
            controllers: [Controller::default(); 2],
            nmi_pending: false,
            irq_line: false,
            prev_irq_line: false,
            prev_nmi_pending: false,
            prev_nmi_flag: false,
            open_bus: 0,
            need_halt: false,
            need_dummy_read: false,
            dmc_dma_running: false,
            dmc_dma_addr: 0,
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
    pub fn mapper_id(&self) -> u16 {
        self.mapper_id
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
            nmi_pending: self.nmi_pending,
            irq_line: self.irq_line,
            prev_irq_line: self.prev_irq_line,
            prev_nmi_pending: self.prev_nmi_pending,
            prev_nmi_flag: self.prev_nmi_flag,
            open_bus: self.open_bus,
            need_halt: self.need_halt,
            need_dummy_read: self.need_dummy_read,
            dmc_dma_running: self.dmc_dma_running,
            dmc_dma_addr: self.dmc_dma_addr,
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
        self.nmi_pending = snap.nmi_pending;
        self.irq_line = snap.irq_line;
        self.prev_irq_line = snap.prev_irq_line;
        self.prev_nmi_pending = snap.prev_nmi_pending;
        self.prev_nmi_flag = snap.prev_nmi_flag;
        self.open_bus = snap.open_bus;
        self.need_halt = snap.need_halt;
        self.need_dummy_read = snap.need_dummy_read;
        self.dmc_dma_running = snap.dmc_dma_running;
        self.dmc_dma_addr = snap.dmc_dma_addr;
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
        self.tick_post_access(true);
        value
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
            // $4015 bit 5 reads as open bus; the APU drives bits 0-4 / 6-7.
            0x4015 => (self.apu.read_status() & 0xDF) | (self.open_bus & 0x20),
            // $4016/$4017: bit 0 = controller, bits 1-4 = 0, bit 6 = 1
            // (A14-decode coupling on the data lines), bits 5 and 7 come
            // from CPU open bus.
            0x4016 => (self.open_bus & 0xA0) | 0x40 | (self.controllers[0].read() & 1),
            0x4017 => (self.open_bus & 0xA0) | 0x40 | (self.controllers[1].read() & 1),
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
        if suppress_open_bus_update {
            self.open_bus = preserved_open_bus;
        } else {
            self.open_bus = value;
        }
        self.tick_post_access(true);
        value
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
        self.tick_pre_access(false);
        match addr {
            0x0000..=0x1FFF => self.ram[(addr & 0x07FF) as usize] = data,
            0x2000..=0x3FFF => self.ppu.cpu_write(addr, data, &mut *self.mapper),
            0x4000..=0x4013 | 0x4015 | 0x4017 => self.apu.write_reg(addr, data),
            0x4014 => {
                // STA $4014's final write cycle.
                self.tick_post_access(false);
                // Snapshot the CPU's penultimate-cycle interrupt samples
                // so they survive OAM DMA's 513/514 cycles. Each DMA
                // cycle's `tick_pre_access` overwrites `prev_irq_line`
                // / `prev_nmi_pending`; without a save/restore, STA's
                // end-of-step poll would see end-of-DMA state and
                // mis-attribute any IRQ/NMI that asserted during the
                // DMA window to STA itself. The next instruction's
                // cycle-1 tick re-captures `bus.irq_line` (which stays
                // live across DMA) so a DMA-window assertion still
                // fires on the cycle after DMA releases - matches
                // Mesen2's lazy-DMA-inside-MemoryRead model. Required
                // by `cpu_interrupts_v2/4-irq_and_dma.nes`.
                let saved_prev_irq = self.prev_irq_line;
                let saved_prev_nmi = self.prev_nmi_pending;
                // Arm OAM DMA and run the shared get/put loop. The
                // pending-read address is $4014 itself (write-only, so
                // reads during the loop's halt/align cycles return
                // open bus without side effects). Mesen2 defers this
                // to the next instruction's MemoryRead; we run it
                // synchronously here for the same net effect - the
                // CPU can't do anything else while halted anyway.
                self.sprite_dma_running = true;
                self.sprite_dma_page = data;
                self.need_halt = true;
                self.process_pending_dma(0x4014);
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
        self.prev_irq_line = self.irq_line;
        self.prev_nmi_pending = self.nmi_pending;

        let pre_ticks = self.clock.start_cpu_cycle(is_read);
        for _ in 0..pre_ticks {
            self.ppu.tick(&mut *self.mapper);
        }

        self.apu.tick_cpu_cycle();
        self.mapper.on_cpu_cycle();
        self.irq_line = self.apu.irq_line() | self.mapper.irq_line();

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
        let _ = self.dma_bus_read(pending_addr);
        self.tick_post_access(true);

        let mut sprite_counter: u16 = 0;
        let mut sprite_byte: u8 = 0;

        while self.dmc_dma_running || self.sprite_dma_running {
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
                    // DMC fetch - uses `mapper.cpu_read` direct so
                    // the PRG bus sees the DMC address, not
                    // `pending_addr` (matches Mesen2 line 406:
                    // `ProcessDmaRead(_apu->GetDmcReadAddress(), …)`).
                    // The fetched byte also drives the CPU data bus -
                    // AccuracyCoin's "DMA + Open Bus" pretest reads
                    // `LDA $4000` immediately after a DMC fetch and
                    // expects the sample byte (typically $00) on the
                    // open-bus latch.
                    self.tick_pre_access(true);
                    let byte = self.mapper.cpu_read(self.dmc_dma_addr);
                    self.open_bus = byte;
                    self.tick_post_access(true);
                    self.apu.dmc_dma_complete(byte);
                    self.dmc_dma_running = false;
                } else if self.sprite_dma_running {
                    // Sprite DMA read - sprite_counter indexes bytes
                    // 0..255 (the read slot of each read/write pair).
                    let sprite_addr =
                        ((self.sprite_dma_page as u16) << 8) | (sprite_counter >> 1);
                    self.tick_pre_access(true);
                    sprite_byte = self.dma_bus_read(sprite_addr);
                    self.tick_post_access(true);
                    sprite_counter = sprite_counter.wrapping_add(1);
                } else {
                    // DMC waiting, no sprite DMA - dummy read of
                    // pending_addr (clear_flag above already
                    // consumed one pending flag).
                    self.tick_pre_access(true);
                    if !skip_dummy_reads {
                        let _ = self.dma_bus_read(pending_addr);
                    }
                    self.tick_post_access(true);
                }
            } else {
                // Put cycle.
                if self.sprite_dma_running && (sprite_counter & 1) == 1 {
                    // Sprite write - commit the byte read last cycle
                    // to $2004.
                    self.tick_pre_access(false);
                    self.ppu.cpu_write(0x2004, sprite_byte, &mut *self.mapper);
                    self.open_bus = sprite_byte;
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
                        let _ = self.dma_bus_read(pending_addr);
                    }
                    self.tick_post_access(true);
                }
            }
        }

        self.in_dma_loop = false;
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
            0x4015 => (self.apu.read_status() & 0xDF) | (self.open_bus & 0x20),
            0x4016 => (self.open_bus & 0xA0) | 0x40 | (self.controllers[0].read() & 1),
            0x4017 => (self.open_bus & 0xA0) | 0x40 | (self.controllers[1].read() & 1),
            0x4018..=0x401F => self.open_bus,
            0x4020..=0x5FFF => self.mapper.cpu_read_ex(addr).unwrap_or(self.open_bus),
            0x6000..=0xFFFF => self.mapper.cpu_read(addr),
            _ => self.open_bus,
        };
        self.open_bus = value;
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::mapper::nrom::Nrom;
    use crate::nes::rom::{Cartridge, Mirroring, TvSystem};

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
            prg_nvram_size: 0,
            tv_system: TvSystem::Ntsc,
            is_nes2: false,
            prg_chr_crc32: 0,
            db_matched: false,
            fds_data: None,
        };
        Bus::new(Box::new(Nrom::new(cart)), Region::Ntsc, 0)
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
        // Advance cpu_cycles by 1 (a cheap RAM read) so STA enters on
        // the opposite parity from the "halt_on_get" test. With
        // cpu_cycles=1 before STA, the write-cycle pushes it to 2 and
        // the halt lands on cycle 3 (odd get-cycle prereq), requiring
        // one align cycle → DMA = 514 cycles beyond STA's own cycle.
        // Total ticks in `write()` = 1 + 514 = 515.
        let _ = bus.read(0x0000);
        let before = bus.clock.cpu_cycles();
        bus.write(0x4014, 0x00);
        let dma_cycles = bus.clock.cpu_cycles() - before;
        assert_eq!(
            dma_cycles, 515,
            "STA + 1 halt + 1 align + 512 pairs = 515 ticks (514-cycle DMA branch)"
        );
    }
}
