use anyhow::{bail, Result};

use crate::rom::{Cartridge, Mirroring};

pub mod axrom;
pub mod bandai_fcg;
pub mod cnrom;
pub mod eeprom_24c0x;
pub mod gxrom;
pub mod jaleco_ss88006;
pub mod mmc1;
pub mod mmc2;
pub mod mmc3;
pub mod mmc4;
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

/// Source for a single nametable-slot read, returned by
/// [`Mapper::ppu_nametable_read`]. The PPU uses this to decide which
/// byte the fetch yields. Lets MMC5 route each of its four NT slots
/// independently to CIRAM A, CIRAM B, ExRAM, or fill-mode — a
/// freedom the flat `Mirroring` enum can't express.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NametableSource {
    /// No override — PPU falls through to CIRAM via `mirroring()`.
    /// The default for every mapper except MMC5 + (future) FDS.
    Default,
    /// Force the slot to CIRAM bank A (the PPU's first 1 KB of VRAM).
    CiramA,
    /// Force the slot to CIRAM bank B (the PPU's second 1 KB).
    CiramB,
    /// Mapper supplies the byte directly — ExRAM-as-NT or fill mode.
    Byte(u8),
}

/// Target for a single nametable-slot write. Symmetric with
/// [`NametableSource`] for reads. Returned by
/// [`Mapper::ppu_nametable_write`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NametableWriteTarget {
    /// No override — PPU writes to CIRAM via `mirroring()`.
    Default,
    /// Write to CIRAM bank A.
    CiramA,
    /// Write to CIRAM bank B.
    CiramB,
    /// Mapper consumed the write (ExRAM-as-NT, or dropped for fill).
    Consumed,
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
    /// behavior (MMC1 consecutive-write filter, MMC5 ppu-idle counter) advance.
    fn on_cpu_cycle(&mut self) {}

    /// Called by the PPU when it's about to read a nametable byte at
    /// `$2000-$2FFF`. `slot` is the 1 KB slot index (0..=3). See
    /// [`NametableSource`] for the reply taxonomy. MMC5 uses this
    /// for `$5105` NT slot mapping, fill-mode, and ExRAM-as-NT.
    /// Default `NametableSource::Default` defers to the PPU's
    /// mirroring-based CIRAM fetch (the pre-MMC5 path).
    fn ppu_nametable_read(&mut self, _slot: u8, _offset: u16) -> NametableSource {
        NametableSource::Default
    }

    /// Called by the PPU on a nametable write. See
    /// [`NametableWriteTarget`] for the reply taxonomy. MMC5 routes
    /// writes based on `$5105`: CIRAM A/B slots land in the relevant
    /// CIRAM bank, ExRAM-mapped slots store in ExRAM, fill-mode
    /// slots drop the write (read-only on real hardware).
    fn ppu_nametable_write(
        &mut self,
        _slot: u8,
        _offset: u16,
        _data: u8,
    ) -> NametableWriteTarget {
        NametableWriteTarget::Default
    }
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

    // ---- Battery-backed RAM persistence ----
    //
    // Wired through [`crate::nes::Nes::save_battery`] at app quit,
    // ROM swap, and periodic autosave. Default impls are "no
    // battery" — mappers with battery-backed PRG-RAM
    // (nrom/mmc1/uxrom/cnrom/mmc3/mmc5 when the cart's flag6 bit 1
    // was set) override.
    //
    // Contract:
    //  * `save_data` returns `None` on non-battery carts. On battery
    //    carts it returns the exact bytes to persist; the save file
    //    is written verbatim (no header / no CRC / no versioning).
    //  * `load_save_data` is called once, just after mapper
    //    construction, BEFORE the CPU reset's first read. Should
    //    accept only data whose length matches `save_data()`'s
    //    current size; silently ignore mismatches (e.g. from a
    //    firmware update changing RAM layout).
    //  * `save_dirty` is set by any PRG-RAM write and cleared by
    //    `mark_saved`. The save pipeline skips disk writes when it
    //    returns false, so the common "game sits on title screen"
    //    case is a cheap no-op.

    /// Snapshot of battery-backed PRG-RAM, or `None` on non-battery
    /// carts. Slice lifetime is borrowed from the mapper — copy the
    /// bytes before calling any mutable mapper method.
    fn save_data(&self) -> Option<&[u8]> {
        None
    }

    /// Restore battery-backed PRG-RAM from a previously-saved slice.
    /// Mappers should silently no-op on length mismatch.
    fn load_save_data(&mut self, _data: &[u8]) {}

    /// True if PRG-RAM has changed since the last `mark_saved` call.
    /// Used by the save pipeline to skip disk writes when nothing
    /// changed. Non-battery mappers always return false.
    fn save_dirty(&self) -> bool {
        false
    }

    /// Called by the save pipeline after a successful write to disk.
    /// Clears the dirty flag.
    fn mark_saved(&mut self) {}
}

pub fn build(cart: Cartridge) -> Result<Box<dyn Mapper>> {
    match cart.mapper_id {
        0 => Ok(Box::new(nrom::Nrom::new(cart))),
        1 => Ok(Box::new(mmc1::Mmc1::new(cart))),
        2 => Ok(Box::new(uxrom::Uxrom::new(cart))),
        3 => Ok(Box::new(cnrom::Cnrom::new(cart))),
        4 => Ok(Box::new(mmc3::Mmc3::new(cart))),
        9 => Ok(Box::new(mmc2::Mmc2::new(cart))),
        10 => Ok(Box::new(mmc4::Mmc4::new(cart))),
        16 => Ok(Box::new(bandai_fcg::BandaiFcg::new(cart))),
        18 => Ok(Box::new(jaleco_ss88006::JalecoSs88006::new(cart))),
        159 => Ok(Box::new(bandai_fcg::BandaiFcg::new(cart))),
        5 => Ok(Box::new(mmc5::Mmc5::new(cart))),
        7 => Ok(Box::new(axrom::Axrom::new(cart))),
        66 => Ok(Box::new(gxrom::Gxrom::new(cart))),
        other => bail!("unsupported mapper {}", other),
    }
}
