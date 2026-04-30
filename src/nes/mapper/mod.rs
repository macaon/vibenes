// SPDX-License-Identifier: GPL-3.0-or-later
use anyhow::{bail, Result};

use crate::nes::rom::{Cartridge, Mirroring};

pub mod axrom;
pub mod bandai_74161;
pub mod bandai_fcg;
pub mod bnrom;
pub mod cnrom;
pub mod cnrom_protect;
pub mod codemasters_bf9096;
pub mod codemasters_bf909x;
pub mod eeprom_24c0x;
pub mod fds;
pub mod fds_audio;
pub mod fme7;
pub mod gxrom;
pub mod irem_74x161;
pub mod irem_g101;
pub mod irem_h3001;
pub mod irem_tam_s1;
pub mod jaleco_jf05;
pub mod jaleco_jf11_14;
pub mod jaleco_jf13;
pub mod jaleco_jf17;
pub mod jaleco_ss88006;
pub mod mapper037;
pub mod mmc1;
pub mod mmc2;
pub mod mmc3;
pub mod mmc4;
pub mod mmc5;
pub mod n163_audio;
pub mod namco163;
pub mod namco_118;
pub mod nrom;
pub mod rambo1;
pub mod sunsoft1;
pub mod sunsoft2;
pub mod sunsoft3;
pub mod sunsoft4;
pub mod sunsoft5b_audio;
pub mod taito_tc0190;
pub mod taito_tc110;
pub mod taito_x1005;
pub mod taito_x1017;
pub mod tc0690;
pub mod tqrom;
pub mod txsrom;
pub mod un1rom;
pub mod un1rom_180;
pub mod uxrom;
pub mod vrc1;
pub mod vrc2_4;
pub mod vrc3;
pub mod vrc6;
pub mod vrc6_audio;
pub mod vrc7;
pub mod vrc7_opll;

/// Classification of a PPU bus access, forwarded to the mapper via
/// [`Mapper::on_ppu_addr`]. MMC5 needs this to pick the BG vs sprite
/// CHR bank set (8×16 mode) and, in a later sub-phase, to detect
/// scanlines via the 3-consecutive-same-NT-fetch signature. Mappers
/// that don't care can ignore the kind entirely - MMC3's A12
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
    /// semantically idle - MMC5 does not count these toward its
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
/// independently to CIRAM A, CIRAM B, ExRAM, or fill-mode - a
/// freedom the flat `Mirroring` enum can't express.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NametableSource {
    /// No override - PPU falls through to CIRAM via `mirroring()`.
    /// The default for every mapper except MMC5 + (future) FDS.
    Default,
    /// Force the slot to CIRAM bank A (the PPU's first 1 KB of VRAM).
    CiramA,
    /// Force the slot to CIRAM bank B (the PPU's second 1 KB).
    CiramB,
    /// Mapper supplies the byte directly - ExRAM-as-NT or fill mode.
    Byte(u8),
}

/// Target for a single nametable-slot write. Symmetric with
/// [`NametableSource`] for reads. Returned by
/// [`Mapper::ppu_nametable_write`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NametableWriteTarget {
    /// No override - PPU writes to CIRAM via `mirroring()`.
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
    /// kind of fetch - see [`PpuFetchKind`]. Default no-op; MMC3 /
    /// MMC5 / MMC2 / MMC4 override.
    fn on_ppu_addr(&mut self, _addr: u16, _ppu_cycle: u64, _kind: PpuFetchKind) {}
    /// True when the cart is pulling /IRQ low. Wire-ORed with the APU
    /// IRQ line inside the bus. Default false; MMC3 / MMC5 / FME-7
    /// / VRC IRQ mappers override.
    fn irq_line(&self) -> bool {
        false
    }

    /// Expansion audio contribution for the current CPU cycle,
    /// pre-scaled so that the bus can linearly add it to the 2A03
    /// sample. `None` (the default) keeps the hot path a branchless
    /// no-op for carts without audio hardware.
    ///
    /// Mappers with expansion audio (FDS / VRC6 / VRC7 / MMC5 / N163 /
    /// Sunsoft 5B) clock their internal DSPs in [`Mapper::on_cpu_cycle`]
    /// and cache the resulting sample; this method just returns the
    /// cache. Each chip scales its own output to match the nesdev-wiki
    /// mixing ratio against the APU (FDS peak ≈ 2.4× APU-pulse peak,
    /// VRC6 ≈ 0.565×, etc.), so the bus stays dumb.
    fn audio_output(&self) -> Option<f32> {
        None
    }

    // ---- Battery-backed RAM persistence ----
    //
    // Wired through [`crate::nes::Nes::save_battery`] at app quit,
    // ROM swap, and periodic autosave. Default impls are "no
    // battery" - mappers with battery-backed PRG-RAM
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
    /// carts. Slice lifetime is borrowed from the mapper - copy the
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

    /// FDS-only: return a `&mut dyn FdsControl` for disk-swap + status
    /// queries. Every other mapper leaves this `None`, which lets the
    /// UI layer gray out the "Disk…" menu for iNES carts without any
    /// `Any` downcasting.
    fn as_fds_mut(&mut self) -> Option<&mut dyn FdsControl> {
        None
    }

    /// Immutable counterpart for status queries - building the disk-
    /// submenu doesn't need mutation.
    fn as_fds(&self) -> Option<&dyn FdsControl> {
        None
    }

    // ---- FDS disk-save persistence ----
    //
    // Parallel to the battery-save hooks above but distinct so the FDS
    // mapper can expose both channels: it has no battery-backed PRG-RAM
    // in the iNES sense (the "save" lives on the disk itself), and its
    // on-disk state is encoded as an IPS diff rather than a blob.
    //
    // Non-FDS mappers keep all four methods as no-op defaults.
    //
    // Contract:
    //  * `disk_save_data` returns `None` outside mapper 20, and on
    //    mapper 20 returns an IPS patch (`PATCH` + records + `EOF`)
    //    encoding the delta between the current disk bytes and the
    //    pristine-on-load snapshot. Interop with Mesen2's `.ips`
    //    format is the design goal - users can move `.ips` files
    //    between emulators.
    //  * `load_disk_save` is invoked once, right after cart mount,
    //    before the CPU reset. The IPS patch is applied to the
    //    pristine raw file and the result re-sliced / re-gapped into
    //    the runtime buffer the transport reads from.
    //  * `disk_save_dirty` flips true on any disk-write that actually
    //    changes a byte (write_disk_byte guards the compare). Cleared
    //    by `mark_disk_saved` after a successful flush.

    /// FDS disk state serialized as an IPS patch. `None` on non-FDS
    /// carts; on FDS carts returns a patch even when no writes have
    /// happened yet (magic + `EOF`, 8 bytes).
    fn disk_save_data(&self) -> Option<Vec<u8>> {
        None
    }

    /// Apply an IPS patch to restore disk state. Non-FDS mappers no-op.
    /// FDS mappers silently ignore malformed patches (the save file is
    /// user data - surfacing a parse error would block the game from
    /// booting with a known-working ROM).
    fn load_disk_save(&mut self, _ips_bytes: &[u8]) {}

    /// True when any disk byte has been modified since the last
    /// `mark_disk_saved` call. Non-FDS mappers always false.
    fn disk_save_dirty(&self) -> bool {
        false
    }

    /// Called by the save pipeline after a successful IPS write.
    /// Clears the disk-dirty flag. No-op outside FDS.
    fn mark_disk_saved(&mut self) {}

    // ---- Save-state snapshot persistence ----
    //
    // Used by [`crate::save_state`] to capture the mapper's full
    // dynamic state into a serde-friendly enum variant and replay
    // it on load. Returning `None` from `save_state_capture` (the
    // default) tells the save-state pipeline that this mapper isn't
    // covered yet (Phase 3b carts), and the user gets a clean
    // [`crate::save_state::SaveStateError::UnsupportedMapper`]
    // instead of a silent partial save.

    /// Capture the mapper's dynamic state. `None` is the
    /// "Phase 3a hasn't covered me" signal. Implemented mappers
    /// return `Some(MapperState::<Variant>(...))`.
    fn save_state_capture(&self) -> Option<crate::save_state::MapperState> {
        None
    }

    /// Restore mapper state from a previously-captured variant. The
    /// caller (see [`crate::save_state::Snapshot::apply`]) has
    /// already validated the file header against the live cart's
    /// mapper id, so a variant mismatch here is an internal error.
    /// Default impl returns `Err(UnsupportedMapper)` so unimplemented
    /// mappers fail-soft instead of silently no-op'ing.
    fn save_state_apply(
        &mut self,
        _state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        Err(crate::save_state::SaveStateError::UnsupportedMapper(0))
    }
}

/// Narrow interface exposed by mapper 20 for the host app: query
/// disk-side count / current side, eject the loaded disk, insert a
/// specific side. Wired from the overlay menu (`Screen::Disk`) and
/// the F4 hotkey. Plain trait so the UI layer stays decoupled from
/// `crate::nes::mapper::fds::Fds`'s concrete type.
pub trait FdsControl {
    /// Total number of disk sides in the image. Always ≥ 1 for
    /// well-formed ROMs.
    fn side_count(&self) -> u8;
    /// Currently-inserted side (0-indexed), or `None` when ejected.
    fn current_side(&self) -> Option<u8>;
    /// Remove the current disk. Subsequent `$4032` polls see the
    /// "disk not present" bits set. Called again during the
    /// physical-swap pause of [`FdsControl::insert`].
    fn eject(&mut self);
    /// Insert a specific side. If a disk is already inserted, ejects
    /// first and schedules the new side to appear after a short
    /// pause - games check the `disk removed → disk present`
    /// transition on `$4032`, so we can't jump directly from one
    /// side to another.
    fn insert(&mut self, side: u8);
}

pub fn build(cart: Cartridge) -> Result<Box<dyn Mapper>> {
    match cart.mapper_id {
        0 => Ok(Box::new(nrom::Nrom::new(cart))),
        1 => Ok(Box::new(mmc1::Mmc1::new(cart))),
        2 => Ok(Box::new(uxrom::Uxrom::new(cart))),
        3 => Ok(Box::new(cnrom::Cnrom::new(cart))),
        4 => Ok(Box::new(mmc3::Mmc3::new(cart))),
        48 => Ok(Box::new(tc0690::Tc0690::new(cart))),
        80 => Ok(Box::new(taito_x1005::TaitoX1005::new(cart))),
        82 => Ok(Box::new(taito_x1017::TaitoX1017::new(cart))),
        67 => Ok(Box::new(sunsoft3::Sunsoft3::new(cart))),
        68 => Ok(Box::new(sunsoft4::Sunsoft4::new(cart))),
        88 => Ok(Box::new(namco_118::Namco118::new_88(cart))),
        89 => Ok(Box::new(sunsoft2::Sunsoft2::new(cart))),
        95 => Ok(Box::new(namco_118::Namco118::new_95(cart))),
        118 => Ok(Box::new(txsrom::Txsrom::new(cart))),
        119 => Ok(Box::new(tqrom::Tqrom::new(cart))),
        207 => Ok(Box::new(taito_x1005::TaitoX1005::new_207(cart))),
        154 => Ok(Box::new(namco_118::Namco118::new_154(cart))),
        206 => Ok(Box::new(namco_118::Namco118::new_206(cart))),
        9 => Ok(Box::new(mmc2::Mmc2::new(cart))),
        10 => Ok(Box::new(mmc4::Mmc4::new(cart))),
        16 => Ok(Box::new(bandai_fcg::BandaiFcg::new(cart))),
        18 => Ok(Box::new(jaleco_ss88006::JalecoSs88006::new(cart))),
        19 => Ok(Box::new(namco163::Namco163::new(cart))),
        20 => Ok(Box::new(fds::Fds::new(cart))),
        21 | 22 | 23 | 25 => Ok(Box::new(vrc2_4::Vrc2_4::new(cart))),
        32 => Ok(Box::new(irem_g101::IremG101::new(cart))),
        33 => Ok(Box::new(taito_tc0190::TaitoTc0190::new(cart))),
        34 => Ok(Box::new(bnrom::Bnrom::new(cart))),
        37 => Ok(Box::new(mapper037::Mapper037::new(cart))),
        64 => Ok(Box::new(rambo1::Rambo1::new(cart))),
        65 => Ok(Box::new(irem_h3001::IremH3001::new(cart))),
        70 => Ok(Box::new(bandai_74161::Bandai74161::new_70(cart))),
        71 => Ok(Box::new(codemasters_bf909x::CodemastersBf909x::new(cart))),
        72 => Ok(Box::new(jaleco_jf17::JalecoJf17::new_72(cart))),
        92 => Ok(Box::new(jaleco_jf17::JalecoJf17::new_92(cart))),
        97 => Ok(Box::new(irem_tam_s1::IremTamS1::new(cart))),
        152 => Ok(Box::new(bandai_74161::Bandai74161::new_152(cart))),
        180 => Ok(Box::new(un1rom_180::Un1rom180::new(cart))),
        184 => Ok(Box::new(sunsoft1::Sunsoft1::new(cart))),
        69 => Ok(Box::new(fme7::Fme7::new(cart))),
        73 => Ok(Box::new(vrc3::Vrc3::new(cart))),
        75 => Ok(Box::new(vrc1::Vrc1::new(cart))),
        78 => Ok(Box::new(irem_74x161::Irem74x161::new(cart))),
        86 => Ok(Box::new(jaleco_jf13::JalecoJf13::new(cart))),
        87 => Ok(Box::new(jaleco_jf05::JalecoJf05::new(cart))),
        94 => Ok(Box::new(un1rom::Un1rom::new(cart))),
        140 => Ok(Box::new(jaleco_jf11_14::JalecoJf11_14::new(cart))),
        24 => Ok(Box::new(vrc6::Vrc6::new_a(cart))),
        26 => Ok(Box::new(vrc6::Vrc6::new_b(cart))),
        85 => Ok(Box::new(vrc7::Vrc7::new(cart))),
        159 => Ok(Box::new(bandai_fcg::BandaiFcg::new(cart))),
        210 => Ok(Box::new(namco163::Namco163::new(cart))),
        185 => Ok(Box::new(cnrom_protect::CnromProtect::new(cart))),
        189 => Ok(Box::new(taito_tc110::TaitoTc110::new(cart))),
        232 => Ok(Box::new(codemasters_bf9096::CodemastersBf9096::new(cart))),
        5 => Ok(Box::new(mmc5::Mmc5::new(cart))),
        7 => Ok(Box::new(axrom::Axrom::new(cart))),
        66 => Ok(Box::new(gxrom::Gxrom::new(cart))),
        other => bail!("unsupported mapper {}", other),
    }
}
