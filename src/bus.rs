use crate::apu::Apu;
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
    pub fn read(&mut self, addr: u16) -> u8 {
        self.service_pending_dmc_dma();
        self.tick_pre_access();
        let value = match addr {
            0x0000..=0x1FFF => self.ram[(addr & 0x07FF) as usize],
            0x2000..=0x3FFF => self.ppu.cpu_read(addr, &mut *self.mapper),
            0x4015 => self.apu.read_status(),
            0x4016 => 0x40 | (self.controllers[0].read() & 1),
            0x4017 => 0x40 | (self.controllers[1].read() & 1),
            0x4018..=0x401F => self.open_bus,
            0x4020..=0xFFFF => self.mapper.cpu_read(addr),
            _ => self.open_bus,
        };
        self.open_bus = value;
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
                // Nesdev "OAM DMA": 513 CPU cycles total, plus one more
                // if the DMA begins on an odd CPU cycle. We capture the
                // parity at the moment the DMA starts (immediately after
                // STA's write) and pass it on so `run_oam_dma` can add
                // the alignment idle when needed.
                let extra_idle = (self.clock.cpu_cycles() & 1) != 0;
                self.run_oam_dma(data, extra_idle);
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

        let ppu_ticks = self.clock.advance_cpu_cycle();
        for _ in 0..ppu_ticks {
            self.ppu.tick(&mut *self.mapper);
        }
        if self.ppu.poll_nmi() {
            self.nmi_pending = true;
        }
    }

    /// Second half of a CPU cycle — runs after the bus access.
    ///
    /// APU + mapper tick here so writes to $4000–$4017 that trigger
    /// them (length loads, frame-counter reset, MMC3 A12 filter) land
    /// on the same cycle as the write. Re-samples the IRQ line after
    /// APU updates.
    fn tick_post_access(&mut self) {
        self.apu.tick_cpu_cycle();
        self.mapper.on_cpu_cycle();
        self.irq_line = self.apu.irq_line();
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
    /// [`Bus::read`] so the halt cycle lines up with the CPU's next read.
    ///
    /// Nesdev "DMC DMA" cycle model: the CPU is halted (1 cycle), then
    /// runs one dummy re-read (1 cycle), then one alignment idle if the
    /// next "get" cycle is a full cycle away (1 cycle), then the DMC's
    /// read takes place (1 cycle) — typical **4 CPU cycles**. We use a
    /// fixed 4-cycle stall for simplicity; this is within the 2..=4
    /// range all documented tests observe, and matches Mesen2's worst
    /// case which is what blargg's apu_test expects.
    fn service_pending_dmc_dma(&mut self) {
        if self.dmc_dma_active {
            return;
        }
        let Some(req) = self.apu.take_dmc_dma_request() else {
            return;
        };
        self.dmc_dma_active = true;
        // Halt + dummy + align: 3 idle CPU cycles. We do not replay the
        // CPU's pending read here; the controller/MMIO double-read bug
        // and its interaction with $4016/$4017 is a later phase.
        self.tick_cycle();
        self.tick_cycle();
        self.tick_cycle();
        // Fourth cycle: the DMC bus-master read. Sample addresses always
        // live in $8000..=$FFFF, so this is a pure mapper read.
        let byte = self.mapper.cpu_read(req.addr);
        self.tick_cycle();
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
        };
        Bus::new(Box::new(Nrom::new(cart)), Region::Ntsc)
    }

    #[test]
    fn oam_dma_even_parity_is_513_cycles() {
        let mut bus = build_bus();
        // Tick once so cpu_cycles becomes 1 (odd). STA's write cycle
        // will then put cpu_cycles at 2 (even) — the "no extra idle"
        // case, total DMA = 513.
        bus.tick_cycle();
        let before = bus.clock.cpu_cycles();
        bus.write(0x4014, 0x00);
        let dma_cycles = bus.clock.cpu_cycles() - before;
        // 1 (STA write) + 1 (idle) + 512 (256 read/write pairs) = 514.
        // The spec's "513" is measured from DMA start, not counting the
        // STA write itself, so we expect 514 ticks in our `write()`.
        assert_eq!(
            dma_cycles, 514,
            "even-parity entry: STA + 1 idle + 512 pairs = 514 ticks"
        );
    }

    #[test]
    fn oam_dma_odd_parity_is_514_cycles() {
        let mut bus = build_bus();
        // cpu_cycles starts at 0. STA's write cycle ticks once → 1 (odd)
        // → needs extra idle.
        let before = bus.clock.cpu_cycles();
        bus.write(0x4014, 0x00);
        let dma_cycles = bus.clock.cpu_cycles() - before;
        // 1 (STA write) + 2 idles + 512 pairs = 515.
        assert_eq!(
            dma_cycles, 515,
            "odd-parity entry: STA + 2 idles + 512 pairs = 515 ticks"
        );
    }
}
