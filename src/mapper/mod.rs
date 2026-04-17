use anyhow::{bail, Result};

use crate::rom::{Cartridge, Mirroring};

pub mod axrom;
pub mod cnrom;
pub mod mmc1;
pub mod nrom;
pub mod uxrom;

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
    /// behavior (MMC1 consecutive-write filter) advance.
    fn on_cpu_cycle(&mut self) {}
    /// Called by the PPU every time it drives its address bus. `ppu_cycle`
    /// is a monotonic PPU-dot timestamp the mapper can use to filter
    /// glitches (e.g. MMC3's ≥10-PPU-cycle A12-low requirement before a
    /// rising edge counts). Default no-op; MMC3 / MMC5 / MMC2 / MMC4
    /// override to observe A12 or the CHR-latch tile reads.
    fn on_ppu_addr(&mut self, _addr: u16, _ppu_cycle: u64) {}
    /// True when the cart is pulling /IRQ low. Wire-ORed with the APU
    /// IRQ line inside the bus. Default false; MMC3 / MMC5 / FME-7
    /// / VRC IRQ mappers override.
    fn irq_line(&self) -> bool {
        false
    }
}

pub fn build(cart: Cartridge) -> Result<Box<dyn Mapper>> {
    match cart.mapper_id {
        0 => Ok(Box::new(nrom::Nrom::new(cart))),
        1 => Ok(Box::new(mmc1::Mmc1::new(cart))),
        2 => Ok(Box::new(uxrom::Uxrom::new(cart))),
        3 => Ok(Box::new(cnrom::Cnrom::new(cart))),
        7 => Ok(Box::new(axrom::Axrom::new(cart))),
        other => bail!("unsupported mapper {}", other),
    }
}
