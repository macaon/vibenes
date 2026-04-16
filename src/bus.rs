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

    open_bus: u8,
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
            open_bus: 0,
        }
    }

    pub fn region(&self) -> Region {
        self.clock.region()
    }

    /// One CPU bus read. Every read costs one CPU cycle.
    pub fn read(&mut self, addr: u16) -> u8 {
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
        self.tick_cycle();
        value
    }

    /// One CPU bus write. Every write costs one CPU cycle.
    pub fn write(&mut self, addr: u16, data: u8) {
        self.open_bus = data;
        match addr {
            0x0000..=0x1FFF => self.ram[(addr & 0x07FF) as usize] = data,
            0x2000..=0x3FFF => self.ppu.cpu_write(addr, data, &mut *self.mapper),
            0x4000..=0x4013 | 0x4015 | 0x4017 => self.apu.write_reg(addr, data),
            0x4014 => {
                self.tick_cycle();
                self.run_oam_dma(data);
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
        self.tick_cycle();
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

    fn tick_cycle(&mut self) {
        let ppu_ticks = self.clock.advance_cpu_cycle();
        for _ in 0..ppu_ticks {
            self.ppu.tick(&mut *self.mapper);
        }
        self.apu.tick_cpu_cycle();
        self.mapper.on_cpu_cycle();
        if self.ppu.poll_nmi() {
            self.nmi_pending = true;
        }
        self.irq_line = self.apu.irq_line();
    }

    fn run_oam_dma(&mut self, page: u8) {
        // 513 or 514 cycles: 1 idle (+1 if on odd cycle) then 256 read/write pairs.
        // We charge 1 idle cycle now (a 2nd will come naturally from the
        // write parity mismatch in real hardware; omitted for the stub).
        self.tick_cycle();
        let base = (page as u16) << 8;
        for i in 0..=0xFFu16 {
            let byte = self.read(base | i);
            self.write(0x2004, byte);
        }
    }
}
