use anyhow::{bail, Result};

use crate::rom::{Cartridge, Mirroring};

pub mod axrom;
pub mod cnrom;
pub mod mmc1;
pub mod mmc3;
pub mod mmc5;
pub mod nrom;
pub mod uxrom;

/// Classification of a PPU bus access, forwarded to the mapper via
/// [`Mapper::on_ppu_addr`]. MMC5 needs this to pick the BG vs sprite
/// CHR bank set (8×16 mode) and, in a later sub-phase, to detect
/// scanlines via the 3-consecutive-same-NT-fetch signature. Mappers
/// that don't care can ignore the kind entirely — MMC3's A12
/// counter, for instance, looks only at bit 12 of the address.
///
/// Sub-phases land progressively: B uses `BgPattern` / `SpritePattern`
/// to split the CHR banking; C uses `BgNametable` for IRQ detection;
/// F will use `BgAttribute` for ExAttr mode. We define the full
/// taxonomy up front so we don't have to re-widen later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PpuFetchKind {
    /// Anything not part of a BG or sprite rendering fetch. Covers
    /// CPU `$2007` data-port reads/writes, `$2006`-triggered address
    /// latches, and any idle-frame access. Default for mappers that
    /// don't distinguish.
    Idle,
    /// Background nametable (tile ID) fetch at `(dot-1) % 8 == 0`
    /// within dots 1-256/321-336, plus the two dummy NT fetches at
    /// dots 337/339.
    BgNametable,
    /// Background attribute-table fetch at `(dot-1) % 8 == 2`.
    BgAttribute,
    /// Background pattern-table fetch (low or high plane) at
    /// `(dot-1) % 8 == 4` or `6`. MMC5 uses this to route through the
    /// BG CHR bank set (`$5120-$5127`).
    BgPattern,
    /// Garbage nametable fetch during the sprite pattern window
    /// (dots 257-320, slot cycle 1). Present on real hardware but
    /// semantically idle — MMC5 does not count these toward its
    /// scanline IRQ detector.
    SpriteNametable,
    /// Garbage attribute fetch in the sprite window (slot cycle 3).
    SpriteAttribute,
    /// Sprite pattern-table fetch (slot cycles 5 and 7). MMC5 routes
    /// these through the sprite CHR bank set (`$5128-$512B`) in 8×16
    /// sprite mode.
    SpritePattern,
}

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
    /// Read from cartridge expansion space `$4020-$5FFF`. Most carts
    /// don't decode this range and return open bus; mappers that DO
    /// claim it (MMC5, FDS) override and return `Some(value)`. The
    /// bus falls through to `open_bus` when this returns `None`.
    fn cpu_read_ex(&mut self, _addr: u16) -> Option<u8> {
        None
    }
    /// Called once per CPU cycle by the bus. Lets mappers with timing-sensitive
    /// behavior (MMC1 consecutive-write filter) advance.
    fn on_cpu_cycle(&mut self) {}
    /// Called by the PPU every time it drives its address bus.
    /// `ppu_cycle` is a monotonic PPU-dot timestamp the mapper can
    /// use to filter glitches (e.g. MMC3's ≥10-PPU-cycle A12-low
    /// requirement before a rising edge counts). `kind` tags the
    /// kind of fetch — see [`PpuFetchKind`]. Default no-op; MMC3 /
    /// MMC5 / MMC2 / MMC4 override.
    fn on_ppu_addr(&mut self, _addr: u16, _ppu_cycle: u64, _kind: PpuFetchKind) {}
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
        4 => Ok(Box::new(mmc3::Mmc3::new(cart))),
        5 => Ok(Box::new(mmc5::Mmc5::new(cart))),
        7 => Ok(Box::new(axrom::Axrom::new(cart))),
        other => bail!("unsupported mapper {}", other),
    }
}
