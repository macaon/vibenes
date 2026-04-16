use anyhow::Result;

use crate::bus::Bus;
use crate::clock::Region;
use crate::cpu::Cpu;
use crate::mapper;
use crate::rom::Cartridge;

pub struct Nes {
    pub cpu: Cpu,
    pub bus: Bus,
}

impl Nes {
    pub fn from_cartridge(cart: Cartridge) -> Result<Self> {
        let region = Region::from_tv_system(cart.tv_system);
        let mapper = mapper::build(cart)?;
        let bus = Bus::new(mapper, region);
        let mut nes = Self {
            cpu: Cpu::new(),
            bus,
        };
        nes.cpu.reset(&mut nes.bus);
        Ok(nes)
    }

    pub fn region(&self) -> Region {
        self.bus.region()
    }

    pub fn step(&mut self) -> Result<(), String> {
        self.cpu.step(&mut self.bus)
    }

    pub fn run_cycles(&mut self, cycles: u64) -> Result<(), String> {
        let end = self.bus.clock.cpu_cycles() + cycles;
        while self.bus.clock.cpu_cycles() < end {
            self.step()?;
            if self.cpu.halted {
                break;
            }
        }
        Ok(())
    }
}
