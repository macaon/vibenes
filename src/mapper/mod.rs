use anyhow::{bail, Result};

use crate::rom::{Cartridge, Mirroring};

pub mod cnrom;
pub mod mmc1;
pub mod nrom;

pub trait Mapper: Send {
    fn cpu_read(&mut self, addr: u16) -> u8;
    fn cpu_write(&mut self, addr: u16, data: u8);
    fn ppu_read(&mut self, addr: u16) -> u8;
    fn ppu_write(&mut self, addr: u16, data: u8);
    fn mirroring(&self) -> Mirroring;
    /// Side-effect-free read for debuggers / test harnesses. Defaults to 0
    /// so mappers with read side effects don't accidentally leak state.
    fn cpu_peek(&self, _addr: u16) -> u8 {
        0
    }
    /// Called once per CPU cycle by the bus. Lets mappers with timing-sensitive
    /// behavior (MMC1 consecutive-write filter, MMC3 A12 IRQ, etc.) advance.
    fn on_cpu_cycle(&mut self) {}
}

pub fn build(cart: Cartridge) -> Result<Box<dyn Mapper>> {
    match cart.mapper_id {
        0 => Ok(Box::new(nrom::Nrom::new(cart))),
        1 => Ok(Box::new(mmc1::Mmc1::new(cart))),
        3 => Ok(Box::new(cnrom::Cnrom::new(cart))),
        other => bail!("unsupported mapper {}", other),
    }
}
