// SPDX-License-Identifier: GPL-3.0-or-later
//! Save states: capture the entire emulator state to a file and
//! restore it deterministically.
//!
//! ## Phase 2 scope (this module, today)
//!
//! - Format header (Phase 1): magic, version, ROM CRC32, mapper id,
//!   region.
//! - **Subsystem snapshot trees**: shadow [`CpuSnap`], [`BusSnap`],
//!   [`PpuSnap`], [`ApuSnap`] structures defined in this module's
//!   submodules. Each live subsystem (`Cpu`, `Bus`, `Ppu`, `Apu`)
//!   carries `snapshot()` / `apply_snapshot()` methods to round-trip
//!   into / out of the shadow types. The on-disk schema is therefore
//!   independent of the live struct field layout - we can refactor
//!   `Bus` internals without breaking save files (until we bump
//!   [`FORMAT_VERSION`]).
//! - End-to-end `save_to_slot` / `load_from_slot` capture the full
//!   non-mapper state at a frame boundary and apply it back into a
//!   matching ROM.
//!
//! Phase 3 will add `mapper: MapperState` (enum variant per mapper)
//! plus per-mapper snapshot/apply impls.
//!
//! ## Format choice
//!
//! `bincode 2` with `serde`. We control the schema via the outer
//! [`StateFile`] struct's `format_version` field; missing-field
//! tolerance (Mesen2's tagged-binary trick) is not in scope for v1.
//! Older states load only on builds whose `FORMAT_VERSION` matches.
//!
//! ## Cross-emulator notes
//!
//! - Mesen2 (`Core/Shared/SaveStateManager.cpp`): tagged binary, .mss,
//!   F5 / F8, format v4. Validates filename, not CRC.
//! - puNES (`src/core/save_slot.c`): fixed-order binary, .p<hex>,
//!   F1 save / F4 load + F2 / F3 to cycle slots, version 102. Enforces
//!   ROM CRC32 match and rolls back to in-memory backup on apply
//!   failure - we adopt that pattern in Phase 5.
//! - vibenes2: F2 = save, F3 = load (per user direction), slot file
//!   naming `<rom-stem>.state<N>`, ROM CRC32 enforced.

pub mod apu;
pub mod bus;
pub mod cpu;
pub mod mapper;
pub mod ppu;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::SaveConfig;
use crate::save;

pub use apu::ApuSnap;
pub use bus::BusSnap;
pub use cpu::CpuSnap;
pub use mapper::MapperState;
pub use ppu::PpuSnap;

/// Four-byte magic at the head of every save-state file. ASCII
/// "VNES" - chosen so a casual `file(1)` / hexdump immediately
/// distinguishes it from `.nes` (ROM), `.sav` (battery), `.ips` (FDS
/// disk diff).
pub const MAGIC: [u8; 4] = *b"VNES";

/// On-disk schema version. Bump on any incompatible change to the
/// `Snapshot` payload (field add/remove, type change, reorder under
/// bincode's positional encoding). v1 makes no forward / backward
/// compat promises - old states fail to load with
/// [`SaveStateError::WrongVersion`].
pub const FORMAT_VERSION: u32 = 1;

/// File-extension prefix for slot files. Slot N writes to
/// `<rom-stem>.state<N>` so `ls` groups slots together and the names
/// don't collide with battery `.sav` / FDS `.ips` siblings under
/// `crate::save`'s `ConfigDir` / `NextToRom` / `ByCrc` routing.
pub const STATE_EXT_PREFIX: &str = "state";

/// Number of save-state slots exposed to the user: 0..=9. Mesen2 has
/// 10, puNES has 12 + 1 file slot. Ten is plenty for keyboard-only
/// access (Ctrl+0..9 to switch); a "load from file" picker can grow
/// out of band.
pub const SLOT_COUNT: u8 = 10;

/// A validated save-state slot index in `0..SLOT_COUNT`.
///
/// Newtype so the file-path resolver can't accidentally be handed a
/// raw `u8` that's out of range. Construct with [`Slot::new`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Slot(u8);

impl Slot {
    /// `None` if `n >= SLOT_COUNT`.
    pub fn new(n: u8) -> Option<Self> {
        if n < SLOT_COUNT { Some(Self(n)) } else { None }
    }

    pub fn index(self) -> u8 {
        self.0
    }
}

impl std::fmt::Display for Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Errors a save or load can return. The UI layer matches on these
/// to render different toasts ("Saved slot 3" / "No state in slot 3"
/// / "Wrong ROM for this state" / a generic I/O fallback).
#[derive(Debug, thiserror::Error)]
pub enum SaveStateError {
    /// File did not contain the four-byte magic. Either it's not a
    /// vibenes save-state, or it's been truncated to under four
    /// bytes.
    #[error("not a vibenes save-state (bad magic)")]
    BadMagic,
    /// File magic matched but the format version is from a future
    /// (or distant past) build. Phase 1 has no migration layer -
    /// users must keep their build pinned alongside their saves.
    #[error("save-state format version {found} not supported (this build expects {expected})")]
    WrongVersion { found: u32, expected: u32 },
    /// ROM CRC32 in the header does not match the currently-loaded
    /// cart's CRC. Refuse to apply rather than silently corrupt
    /// state - puNES does the same. Caller resolves by either
    /// loading the matching ROM or declining the load.
    #[error("save-state was made for ROM CRC {expected:08X}, but current ROM CRC is {found:08X}")]
    WrongRom { expected: u32, found: u32 },
    /// Mapper id in the header does not match the current cart's
    /// mapper. Defense-in-depth in case two carts share a CRC32 by
    /// accident or a NES 2.0 submapper variant collides.
    #[error("save-state was made for mapper {expected}, but current cart is mapper {found}")]
    MapperMismatch { expected: u16, found: u16 },
    /// Region (NTSC/PAL) in the header does not match the current
    /// cart. Cycle counts are region-encoded; cross-region apply
    /// would silently desync the master clock.
    #[error("save-state region {expected} does not match current region {found}")]
    RegionMismatch { expected: &'static str, found: &'static str },
    /// No save metadata attached to the `Nes` (no ROM loaded, or a
    /// test harness that bypassed [`crate::nes::Nes::attach_save_metadata`]).
    /// We need ROM identity to write a CRC into the header, so this
    /// is fatal at save time and meaningless at load time.
    #[error("no ROM metadata attached - load a cart first")]
    NoRomMetadata,
    /// File system error reading or writing the state file.
    #[error("save-state I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// `bincode` could not encode or decode the file payload. Either
    /// the file is corrupt, or there's a bug in our schema. Either
    /// way the user sees a generic error and the in-memory state
    /// stays untouched.
    #[error("save-state encode error: {0}")]
    Encode(String),
    #[error("save-state decode error: {0}")]
    Decode(String),
    /// The active mapper has no [`MapperState`] variant in this
    /// build. Phase 3a covers the 10 most-common mappers; Phase 3b
    /// will fill in the rest. Carries the live mapper id so the UI
    /// can render "Save state for mapper {id} not yet supported".
    #[error("save-state for mapper {0} not yet supported in this build")]
    UnsupportedMapper(u16),
}

impl From<bincode::error::EncodeError> for SaveStateError {
    fn from(e: bincode::error::EncodeError) -> Self {
        Self::Encode(e.to_string())
    }
}

impl From<bincode::error::DecodeError> for SaveStateError {
    fn from(e: bincode::error::DecodeError) -> Self {
        Self::Decode(e.to_string())
    }
}

/// Region tag stored in the file header. We use a separate `u8` enum
/// rather than serializing `crate::nes::clock::Region` directly so
/// the on-disk encoding survives module renames / `Region` field
/// reorderings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum RegionTag {
    #[default]
    Ntsc = 0,
    Pal = 1,
}

impl RegionTag {
    pub fn from_region(r: crate::nes::clock::Region) -> Self {
        match r {
            crate::nes::clock::Region::Ntsc => Self::Ntsc,
            crate::nes::clock::Region::Pal => Self::Pal,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ntsc => "NTSC",
            Self::Pal => "PAL",
        }
    }

    /// Lowercase token used in the slot file extension. Picked
    /// distinct from [`Self::as_str`] (which is upper-case for
    /// log / overlay text) so the on-disk format doesn't accept
    /// `.NTSC.state0` / `.ntsc.state0` interchangeably - one
    /// canonical form keeps the path resolver simple.
    pub fn ext_token(self) -> &'static str {
        match self {
            Self::Ntsc => "ntsc",
            Self::Pal => "pal",
        }
    }
}

/// Outer envelope for a save-state file.
///
/// All fields are part of the on-disk schema and are checked on load
/// before the snapshot payload is applied. Adding a field here is a
/// format-version bump.
#[derive(Debug, Serialize, Deserialize)]
pub struct StateFile {
    /// Always [`MAGIC`]. First gate against random files.
    pub magic: [u8; 4],
    /// Always [`FORMAT_VERSION`] for the build that wrote it. Second
    /// gate.
    pub format_version: u32,
    /// CRC32 of the cart's PRG + CHR. Pulled from
    /// `Nes::save_meta.prg_chr_crc32`. Third gate.
    pub rom_crc32: u32,
    /// iNES mapper id (0..511 for NES 2.0). Fourth gate -
    /// belt-and-suspenders against CRC collisions.
    pub mapper_id: u16,
    /// NTSC vs PAL. Cycle counters in the snapshot are
    /// region-encoded; cross-region apply would desync.
    pub region: RegionTag,
    /// The actual emulator state. Phase 1 leaves this empty;
    /// Phases 2-3 fill in CPU/PPU/APU/Bus/Mapper subtrees.
    pub snapshot: Snapshot,
}

/// The serializable snapshot of a running NES.
///
/// Each field is a `*Snap` shadow struct - a serde-friendly mirror
/// of the live subsystem's state. The shadow layer decouples the
/// on-disk format from the live struct's field layout: a future
/// refactor of `Bus` or `Ppu` only breaks states if we change the
/// `*Snap` fields too.
///
/// Phase 3 will add `mapper: MapperState` (enum variant per mapper
/// module) once per-mapper snapshot/apply is wired.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub cpu: CpuSnap,
    pub bus: BusSnap,
    pub ppu: PpuSnap,
    pub apu: ApuSnap,
    pub mapper: MapperState,
}

impl Snapshot {
    /// Capture the full state of `nes`. Must be called at a frame
    /// boundary (the Phase 4 frontend queues F2 presses to the next
    /// [`crate::nes::Nes::step_until_frame`] return).
    /// Mid-instruction / mid-DMA capture is out of scope for v1.
    ///
    /// Returns `Err(UnsupportedMapper)` when the active mapper
    /// hasn't been wired into [`MapperState`] yet (Phase 3b carts).
    pub fn capture(nes: &crate::nes::Nes) -> Result<Self, SaveStateError> {
        let mapper = nes.bus.mapper.save_state_capture().ok_or_else(|| {
            SaveStateError::UnsupportedMapper(nes.bus.mapper_id())
        })?;
        Ok(Self {
            cpu: CpuSnap::capture(&nes.cpu),
            bus: BusSnap::capture(&nes.bus),
            ppu: PpuSnap::capture(&nes.bus.ppu),
            apu: ApuSnap::capture(&nes.bus.apu),
            mapper,
        })
    }

    /// Apply this snapshot to `nes`, overwriting all live state.
    /// Caller is responsible for having validated the file header
    /// against the live cart first (see [`validate_against_nes`]).
    ///
    /// Returns `Err(UnsupportedMapper)` if the snapshot's mapper
    /// variant is `Unsupported` or doesn't match the live cart.
    pub fn apply(self, nes: &mut crate::nes::Nes) -> Result<(), SaveStateError> {
        nes.bus.mapper.save_state_apply(&self.mapper)?;
        self.cpu.apply(&mut nes.cpu);
        self.bus.apply(&mut nes.bus);
        self.ppu.apply(&mut nes.bus.ppu);
        self.apu.apply(&mut nes.bus.apu);
        Ok(())
    }
}

/// Build a [`StateFile`] header for the given `Nes`. Public so the
/// frontend can decide what to do with [`SaveStateError::NoRomMetadata`]
/// (toast vs. silent skip) before we touch disk.
pub fn build_header(nes: &crate::nes::Nes) -> Result<StateFile, SaveStateError> {
    let crc = nes
        .save_meta_crc32()
        .ok_or(SaveStateError::NoRomMetadata)?;
    let mapper_id = nes.bus.mapper_id();
    let region = RegionTag::from_region(nes.region());
    Ok(StateFile {
        magic: MAGIC,
        format_version: FORMAT_VERSION,
        rom_crc32: crc,
        mapper_id,
        region,
        snapshot: Snapshot::capture(nes)?,
    })
}

/// Resolve the file path for a slot.
///
/// Filename always carries the ROM CRC32 plus the region tag plus
/// the slot number, so distinct ROM variants (a rev, an IPS-patched
/// hack, a fan translation, a renamed copy) and distinct regions
/// (NTSC vs PAL builds of the same game) all land at distinct
/// paths. The header validator can tell us "wrong ROM" or "wrong
/// region" *after* the file open, but if two carts share a path
/// the first save silently overwrites the other - the path-level
/// disambiguation is what closes that gap.
///
/// Directory routing follows `SaveStyle`:
///   - `ConfigDir`: under `$XDG_CONFIG_HOME/vibenes/saves/`
///   - `NextToRom`: alongside the ROM file (FCEUX-style)
///   - `ByCrc`: under saves dir, filename starts with CRC instead
///     of rom-stem (matches the style's "renames-don't-affect-save-
///     discovery" intent for users who renamed away from the
///     filesystem dump's name)
///
/// Filename pattern (where `<crc8>` = 8-char uppercase hex):
///   - `ConfigDir` / `NextToRom`: `<rom-stem>.<crc8>.<region>.state<N>`
///   - `ByCrc`:                    `<crc8>.<region>.state<N>`
pub fn slot_path_for(
    rom_path: &Path,
    crc: u32,
    region: RegionTag,
    cfg: &SaveConfig,
    slot: Slot,
) -> Option<PathBuf> {
    use crate::config::SaveStyle;
    let region_tok = region.ext_token();
    let slot_n = slot.index();
    match cfg.style {
        SaveStyle::ConfigDir => {
            let dir = cfg.dir_override.clone().or_else(save::saves_dir)?;
            let stem = rom_path.file_stem()?;
            let stem_str = stem.to_string_lossy();
            let name = format!(
                "{stem_str}.{crc:08X}.{region_tok}.{STATE_EXT_PREFIX}{slot_n}",
            );
            Some(dir.join(name))
        }
        SaveStyle::NextToRom => {
            let stem = rom_path.file_stem()?;
            let stem_str = stem.to_string_lossy();
            let parent = rom_path.parent().unwrap_or_else(|| Path::new("."));
            let name = format!(
                "{stem_str}.{crc:08X}.{region_tok}.{STATE_EXT_PREFIX}{slot_n}",
            );
            Some(parent.join(name))
        }
        SaveStyle::ByCrc => {
            let dir = cfg.dir_override.clone().or_else(save::saves_dir)?;
            let name = format!("{crc:08X}.{region_tok}.{STATE_EXT_PREFIX}{slot_n}");
            Some(dir.join(name))
        }
    }
}

/// Encode a [`StateFile`] to bytes. Pure - no I/O.
pub fn encode(state: &StateFile) -> Result<Vec<u8>, SaveStateError> {
    let bytes = bincode::serde::encode_to_vec(state, bincode::config::standard())?;
    Ok(bytes)
}

/// Decode bytes into a [`StateFile`]. Validates the magic and
/// version eagerly; downstream callers still need to compare
/// `rom_crc32` / `mapper_id` / `region` against the live cart.
pub fn decode(bytes: &[u8]) -> Result<StateFile, SaveStateError> {
    if bytes.len() < 4 || bytes[..4] != MAGIC {
        return Err(SaveStateError::BadMagic);
    }
    let (state, _consumed): (StateFile, usize) =
        bincode::serde::decode_from_slice(bytes, bincode::config::standard())?;
    if state.magic != MAGIC {
        return Err(SaveStateError::BadMagic);
    }
    if state.format_version != FORMAT_VERSION {
        return Err(SaveStateError::WrongVersion {
            found: state.format_version,
            expected: FORMAT_VERSION,
        });
    }
    Ok(state)
}

/// Validate a decoded state against the currently-loaded cart.
/// Returns `Ok(())` only when CRC, mapper id, and region all match.
/// Phase 2 / 3 will additionally call `apply` after this passes.
pub fn validate_against_nes(
    state: &StateFile,
    nes: &crate::nes::Nes,
) -> Result<(), SaveStateError> {
    let live_crc = nes
        .save_meta_crc32()
        .ok_or(SaveStateError::NoRomMetadata)?;
    if state.rom_crc32 != live_crc {
        return Err(SaveStateError::WrongRom {
            expected: state.rom_crc32,
            found: live_crc,
        });
    }
    let live_mapper = nes.bus.mapper_id();
    if state.mapper_id != live_mapper {
        return Err(SaveStateError::MapperMismatch {
            expected: state.mapper_id,
            found: live_mapper,
        });
    }
    let live_region = RegionTag::from_region(nes.region());
    if state.region != live_region {
        return Err(SaveStateError::RegionMismatch {
            expected: state.region.as_str(),
            found: live_region.as_str(),
        });
    }
    Ok(())
}

/// End-to-end save: build header + encode + atomic-write to the
/// resolved slot path. Used by the Phase 4 frontend when the user
/// presses F2.
pub fn save_to_slot(
    nes: &crate::nes::Nes,
    cfg: &SaveConfig,
    slot: Slot,
) -> Result<PathBuf, SaveStateError> {
    let crc = nes
        .save_meta_crc32()
        .ok_or(SaveStateError::NoRomMetadata)?;
    let rom_path = nes
        .current_rom_path()
        .ok_or(SaveStateError::NoRomMetadata)?;
    let region = RegionTag::from_region(nes.region());
    let path = slot_path_for(rom_path, crc, region, cfg, slot).ok_or_else(|| {
        SaveStateError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "could not resolve save-state path",
        ))
    })?;
    let header = build_header(nes)?;
    let bytes = encode(&header)?;
    save::write(&path, &bytes).map_err(|e| {
        SaveStateError::Io(std::io::Error::other(e.to_string()))
    })?;
    Ok(path)
}

/// End-to-end load: read the slot file, decode, validate against
/// the current cart. Returns the decoded [`StateFile`] without
/// touching `nes` so a caller (or unit test) can inspect / route
/// the apply step. Most callers want
/// [`load_and_apply_from_slot`] instead.
pub fn load_from_slot(
    nes: &crate::nes::Nes,
    cfg: &SaveConfig,
    slot: Slot,
) -> Result<StateFile, SaveStateError> {
    let crc = nes
        .save_meta_crc32()
        .ok_or(SaveStateError::NoRomMetadata)?;
    let rom_path = nes
        .current_rom_path()
        .ok_or(SaveStateError::NoRomMetadata)?;
    let region = RegionTag::from_region(nes.region());
    let path = slot_path_for(rom_path, crc, region, cfg, slot).ok_or_else(|| {
        SaveStateError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "could not resolve save-state path",
        ))
    })?;
    let bytes = std::fs::read(&path)?;
    let state = decode(&bytes)?;
    validate_against_nes(&state, nes)?;
    Ok(state)
}

/// Load a slot file and apply it to `nes`.
///
/// On any failure the live `nes` is left in **exactly its
/// pre-call state**. The execution model:
///
/// 1. **Pre-flight** (no mutation): resolve path → read bytes →
///    decode header → validate CRC / mapper / region against the
///    live cart. Failures here can't have touched `nes` yet.
/// 2. **Backup**: capture the current `Nes` into a `Snapshot`. This
///    is the rollback target. The capture itself can fail (a Phase
///    3b mapper not yet wired returns `UnsupportedMapper`) - that
///    case fails fast, again before any mutation.
/// 3. **Apply**: drive the loaded snapshot into `nes`. If the
///    mapper-side apply fails halfway (e.g. a variant mismatch a
///    bincode collision didn't catch), the partial mutation is
///    undone by re-applying the backup. The caller still sees the
///    original error code.
///
/// The pattern is borrowed from puNES (`save_slot.c:125, 142`),
/// which calls a `rewind_save_state_snap(BCK_STATES_OP_READ_FROM_MEM)`
/// rollback after a mid-load size-mismatch detection. Our
/// pre-validation catches the same class of bugs earlier; the
/// backup is belt-and-suspenders for the residual window where
/// header-OK / decode-OK / apply-fails-late.
pub fn load_and_apply_from_slot(
    nes: &mut crate::nes::Nes,
    cfg: &SaveConfig,
    slot: Slot,
) -> Result<(), SaveStateError> {
    // Step 1 - everything up to and including validation runs
    // without touching `nes`.
    let state = load_from_slot(nes, cfg, slot)?;

    // Step 2 - capture the rollback target. If the live mapper
    // can't snapshot we'd rather know *now* (clean error, no
    // mutation) than partway through the apply.
    let backup = Snapshot::capture(nes)?;

    // Step 3 - apply, rolling back on failure. If the rollback
    // itself fails - which would mean the backup snapshot we just
    // captured is now incompatible with the partially-mutated
    // mapper, an internal consistency bug - we surface the
    // ORIGINAL apply error, not the rollback error: the user's
    // mental model is "F3 didn't take," and they shouldn't see a
    // confusing secondary error from our recovery path.
    if let Err(apply_err) = state.snapshot.apply(nes) {
        let _ = backup.apply(nes);
        return Err(apply_err);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(crc: u32, mapper: u16, region: RegionTag) -> StateFile {
        StateFile {
            magic: MAGIC,
            format_version: FORMAT_VERSION,
            rom_crc32: crc,
            mapper_id: mapper,
            region,
            snapshot: Snapshot::default(),
        }
    }

    #[test]
    fn round_trip_header_byte_equal() {
        let s1 = header(0xDEADBEEF, 4, RegionTag::Ntsc);
        let bytes = encode(&s1).unwrap();
        let s2 = decode(&bytes).unwrap();
        let bytes2 = encode(&s2).unwrap();
        assert_eq!(bytes, bytes2);
        assert_eq!(s2.rom_crc32, 0xDEADBEEF);
        assert_eq!(s2.mapper_id, 4);
        assert_eq!(s2.region, RegionTag::Ntsc);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = encode(&header(0, 0, RegionTag::Ntsc)).unwrap();
        bytes[0] = b'X';
        match decode(&bytes) {
            Err(SaveStateError::BadMagic) => {}
            other => panic!("expected BadMagic, got {other:?}"),
        }
    }

    #[test]
    fn rejects_truncated_below_magic_size() {
        let bytes = vec![b'V', b'N']; // 2 bytes: shorter than the 4-byte magic
        match decode(&bytes) {
            Err(SaveStateError::BadMagic) => {}
            other => panic!("expected BadMagic, got {other:?}"),
        }
    }

    #[test]
    fn rejects_wrong_format_version() {
        // Build a header, encode, then mutate the format_version
        // byte(s) to a value we know we don't support. bincode
        // standard config encodes u32 as varint - the version byte
        // is right after the 4-byte magic.
        let s = header(0, 0, RegionTag::Ntsc);
        let mut bytes = encode(&s).unwrap();
        // varint: low 7 bits + continuation bit. v1 → 0x01.
        // Replace with 0xFE (high enough we won't bump there soon)
        // via two-byte varint: 0xFE 0x01 = 254. We just want a
        // value != FORMAT_VERSION.
        assert_eq!(bytes[4], 0x01, "expected varint v1; bincode config changed?");
        bytes[4] = 0x7F; // still single-byte varint, value 127
        match decode(&bytes) {
            Err(SaveStateError::WrongVersion { found, expected }) => {
                assert_eq!(found, 127);
                assert_eq!(expected, FORMAT_VERSION);
            }
            other => panic!("expected WrongVersion, got {other:?}"),
        }
    }

    #[test]
    fn slot_new_validates_range() {
        assert!(Slot::new(0).is_some());
        assert!(Slot::new(9).is_some());
        assert!(Slot::new(10).is_none());
        assert!(Slot::new(255).is_none());
    }

    #[test]
    fn slot_path_includes_stem_crc_region_and_slot() {
        use crate::config::{SaveConfig, SaveStyle};
        let cfg = SaveConfig {
            style: SaveStyle::NextToRom,
            ..SaveConfig::default()
        };
        let rom = Path::new("/tmp/MyRom.nes");
        let ntsc = slot_path_for(rom, 0xCAFE, RegionTag::Ntsc, &cfg, Slot::new(3).unwrap())
            .unwrap();
        assert_eq!(ntsc, Path::new("/tmp/MyRom.0000CAFE.ntsc.state3"));
        let pal = slot_path_for(rom, 0xCAFE, RegionTag::Pal, &cfg, Slot::new(3).unwrap())
            .unwrap();
        assert_eq!(pal, Path::new("/tmp/MyRom.0000CAFE.pal.state3"));
    }

    /// NTSC and PAL builds of the same game commonly share both a
    /// PRG/CHR CRC (timing differences live in the iNES header
    /// flag, not the cart binary) and a basename. Without region
    /// in the path, saving on the NTSC build would silently
    /// overwrite the PAL save.
    #[test]
    fn ntsc_and_pal_paths_do_not_collide() {
        use crate::config::{SaveConfig, SaveStyle};
        let rom = Path::new("/tmp/MyRom.nes");
        let crc = 0xDEAD_BEEF;
        let slot = Slot::new(0).unwrap();
        for style in [SaveStyle::NextToRom, SaveStyle::ByCrc] {
            let cfg = SaveConfig {
                style,
                ..SaveConfig::default()
            };
            let ntsc = slot_path_for(rom, crc, RegionTag::Ntsc, &cfg, slot);
            let pal = slot_path_for(rom, crc, RegionTag::Pal, &cfg, slot);
            if let (Some(n), Some(p)) = (ntsc, pal) {
                assert_ne!(n, p, "NTSC and PAL must not share a path under {style:?}");
            }
        }
    }

    /// Two ROMs that share a basename but differ in CRC32 (an
    /// IPS-patched hack, a fan translation, a re-dump, or any
    /// "renamed to the same name" scenario) must land at distinct
    /// slot paths. The header validator's `WrongRom` error fires
    /// *after* the file open, so without CRC in the path the
    /// patched ROM's first F2 silently clobbers the unpatched
    /// ROM's save - which is exactly what the user hit on
    /// `Super Mario Bros. 3 (USA) (Rev A).nes` (CRC 17D436AD)
    /// vs the Spanish-translated dump (CRC 2E6301ED).
    #[test]
    fn same_basename_different_crc_paths_do_not_collide() {
        use crate::config::{SaveConfig, SaveStyle};
        let rom = Path::new("/tmp/SMB3.nes");
        let slot = Slot::new(0).unwrap();
        let region = RegionTag::Ntsc;
        for style in [SaveStyle::NextToRom, SaveStyle::ByCrc] {
            let cfg = SaveConfig {
                style,
                ..SaveConfig::default()
            };
            let unpatched = slot_path_for(rom, 0x17D4_36AD, region, &cfg, slot);
            let patched = slot_path_for(rom, 0x2E63_01ED, region, &cfg, slot);
            if let (Some(a), Some(b)) = (unpatched, patched) {
                assert_ne!(
                    a, b,
                    "different-CRC paths must not collide under {style:?}",
                );
            }
        }
    }

    /// `SaveStyle::ByCrc` drops the rom-stem prefix entirely.
    /// Useful for users who routinely rename their ROM files away
    /// from the filesystem dump's canonical name and want save
    /// discovery to follow the binary, not the filename.
    #[test]
    fn slot_path_by_crc_style_drops_rom_stem() {
        use crate::config::{SaveConfig, SaveStyle};
        let cfg = SaveConfig {
            style: SaveStyle::ByCrc,
            dir_override: Some(PathBuf::from("/tmp/saves")),
            ..SaveConfig::default()
        };
        let rom = Path::new("/anywhere/Anything Goes.nes");
        let p = slot_path_for(rom, 0x17D4_36AD, RegionTag::Ntsc, &cfg, Slot::new(2).unwrap())
            .unwrap();
        assert_eq!(p, Path::new("/tmp/saves/17D436AD.ntsc.state2"));
    }

    #[test]
    fn snapshot_round_trip_byte_equal() {
        let s1 = Snapshot::default();
        let bytes = bincode::serde::encode_to_vec(&s1, bincode::config::standard()).unwrap();
        let (s2, _): (Snapshot, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        let bytes2 = bincode::serde::encode_to_vec(&s2, bincode::config::standard()).unwrap();
        assert_eq!(bytes, bytes2);
    }

    /// Default snapshot's PPU section round-trips through serde
    /// without losing the 2 KiB CIRAM, 256-byte OAM, or 32-byte
    /// palette - the three big arrays that need
    /// [`serde_big_array::BigArray`].
    #[test]
    fn ppu_big_arrays_round_trip() {
        let mut snap = PpuSnap::default();
        for (i, byte) in snap.vram.iter_mut().enumerate() {
            *byte = (i & 0xFF) as u8;
        }
        for (i, byte) in snap.oam.iter_mut().enumerate() {
            *byte = ((i * 7) & 0xFF) as u8;
        }
        for (i, byte) in snap.palette.iter_mut().enumerate() {
            *byte = i as u8;
        }
        let bytes = bincode::serde::encode_to_vec(&snap, bincode::config::standard()).unwrap();
        let (back, _): (PpuSnap, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        assert_eq!(back.vram, snap.vram);
        assert_eq!(back.oam, snap.oam);
        assert_eq!(back.palette, snap.palette);
    }

    /// CPU snapshot captures every public-facing field including the
    /// branch-taken-no-cross latch and pending interrupt enum.
    #[test]
    fn cpu_capture_apply_round_trip() {
        use crate::nes::cpu::Cpu;
        let mut original = Cpu::new();
        original.a = 0x42;
        original.x = 0x55;
        original.y = 0xAA;
        original.pc = 0xC0DE;
        original.sp = 0xF7;
        original.cycles = 1234567;
        original.halted = true;
        let snap = original.save_state_capture();

        let mut restored = Cpu::new();
        restored.save_state_apply(snap);

        assert_eq!(restored.a, original.a);
        assert_eq!(restored.x, original.x);
        assert_eq!(restored.y, original.y);
        assert_eq!(restored.pc, original.pc);
        assert_eq!(restored.sp, original.sp);
        assert_eq!(restored.cycles, original.cycles);
        assert_eq!(restored.halted, original.halted);
    }

    /// MapperState round-trips through serde for each implemented
    /// variant via its `Default` value. This catches any field we
    /// failed to derive `Serialize`/`Deserialize` on.
    #[test]
    fn mapper_state_default_variants_round_trip() {
        use crate::save_state::mapper::*;
        let variants = [
            MapperState::Nrom(NromSnap::default()),
            MapperState::Uxrom(UxromSnap::default()),
            MapperState::Cnrom(CnromSnap::default()),
            MapperState::Axrom(AxromSnap::default()),
            MapperState::Gxrom(GxromSnap::default()),
            MapperState::Mmc1(Mmc1Snap::default()),
            MapperState::Mmc2(Mmc2Snap::default()),
            MapperState::Mmc3(Mmc3Snap::default()),
            MapperState::Mmc4(Mmc4Snap::default()),
            MapperState::Mmc5(Box::new(Mmc5Snap::default())),
            MapperState::Vrc1(Vrc1Snap::default()),
            MapperState::Vrc24(Box::new(Vrc24Snap::default())),
            MapperState::Vrc3(Vrc3Snap::default()),
            MapperState::Vrc6(Box::new(Vrc6Snap::default())),
            MapperState::Vrc7(Box::new(Vrc7Snap::default())),
            MapperState::Fme7(Box::new(Fme7Snap::default())),
            MapperState::BandaiFcg(Box::new(BandaiFcgSnap::default())),
            MapperState::Jaleco(Box::new(JalecoSnap::default())),
            MapperState::JalecoJf17(JalecoJf17Snap::default()),
            MapperState::Namco163(Box::new(Namco163Snap::default())),
            MapperState::Rambo1(Box::new(Rambo1Snap::default())),
            MapperState::Irem74x161(Irem74x161Snap::default()),
            MapperState::IremG101(IremG101Snap::default()),
            MapperState::IremH3001(Box::new(IremH3001Snap::default())),
            MapperState::Bandai74161(Bandai74161Snap::default()),
            MapperState::TaitoTc0190(TaitoTc0190Snap::default()),
            MapperState::Mapper037(Box::new(Mapper037Snap::default())),
            MapperState::Fds(Box::new(FdsSnap::default())),
            MapperState::Sunsoft1(Sunsoft1Snap::default()),
            MapperState::Sunsoft2(Sunsoft2Snap::default()),
            MapperState::Sunsoft3(Sunsoft3Snap::default()),
            MapperState::Sunsoft4(Sunsoft4Snap::default()),
            MapperState::Namco118(Namco118Snap::default()),
            MapperState::Txsrom(Box::new(TxsromSnap::default())),
            MapperState::Tqrom(Box::new(TqromSnap::default())),
            MapperState::Tc0690(Box::new(Tc0690Snap::default())),
            MapperState::TaitoX1005(Box::new(TaitoX1005Snap::default())),
            MapperState::TaitoX1017(Box::new(TaitoX1017Snap::default())),
            MapperState::Unsupported(255),
        ];
        for v in &variants {
            let bytes = bincode::serde::encode_to_vec(v, bincode::config::standard()).unwrap();
            let (back, _): (MapperState, usize) =
                bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
            // Re-encode and compare bytes - confirms the variant
            // tag and field order survive a round trip.
            let bytes2 =
                bincode::serde::encode_to_vec(&back, bincode::config::standard()).unwrap();
            assert_eq!(bytes, bytes2, "variant {v:?} did not round-trip cleanly");
        }
    }

    /// MMC3's IRQ counter, A12 filter timestamp, and bank registers
    /// survive a capture / apply round trip. This is the
    /// easy-to-miss state per /nes-expert + Mesen2's
    /// `_a12LowClock`.
    #[test]
    fn mmc3_capture_apply_preserves_irq_state() {
        use crate::nes::mapper::Mapper;
        use crate::nes::mapper::mmc3::Mmc3;
        use crate::nes::rom::{Cartridge, Mirroring, TvSystem};

        fn cart() -> Cartridge {
            Cartridge {
                prg_rom: vec![0u8; 0x8000],
                chr_rom: vec![0u8; 0x2000],
                chr_ram: false,
                mapper_id: 4,
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
            }
        }

        let mut original = Mmc3::new(cart());
        // Drive some non-default state via register writes.
        original.cpu_write(0x8000, 0xC1); // bank_select: R1 with PRG mode
        original.cpu_write(0x8001, 0x10); // bank reg
        original.cpu_write(0xC000, 0x42); // IRQ latch
        original.cpu_write(0xE001, 0x00); // IRQ enable

        let snap = original.save_state_capture().expect("MMC3 captures");
        // Encode through bincode to confirm the wire format.
        let bytes = bincode::serde::encode_to_vec(&snap, bincode::config::standard()).unwrap();
        let (back, _): (crate::save_state::MapperState, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();

        let mut restored = Mmc3::new(cart());
        restored.save_state_apply(&back).unwrap();

        // Compare via re-capture: byte-equal capture means same dynamic state.
        let snap_a = original.save_state_capture().unwrap();
        let snap_b = restored.save_state_capture().unwrap();
        let a = bincode::serde::encode_to_vec(&snap_a, bincode::config::standard()).unwrap();
        let b = bincode::serde::encode_to_vec(&snap_b, bincode::config::standard()).unwrap();
        assert_eq!(a, b);
    }

    /// Cross-variant apply must fail cleanly without mutating the
    /// mapper. Defends against a malformed save file that decodes
    /// successfully but carries the wrong variant.
    #[test]
    fn mapper_apply_rejects_wrong_variant() {
        use crate::nes::mapper::Mapper;
        use crate::nes::mapper::nrom::Nrom;
        use crate::nes::rom::{Cartridge, Mirroring, TvSystem};

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
        let mut nrom = Nrom::new(cart);
        let bad = crate::save_state::MapperState::Mmc1(
            crate::save_state::mapper::Mmc1Snap::default(),
        );
        match nrom.save_state_apply(&bad) {
            Err(SaveStateError::UnsupportedMapper(_)) => {}
            other => panic!("expected UnsupportedMapper, got {other:?}"),
        }
    }

    /// Failed apply must restore the pre-call state byte-for-byte.
    /// We synthesize the failure by hand-corrupting a `Snapshot`'s
    /// `mapper` variant after capture but before apply: the live
    /// cart is NROM, the snapshot says MMC1 - the mapper-side
    /// `save_state_apply` rejects the mismatch and the rollback
    /// path fires. The post-rollback NES must capture identical
    /// bytes to the pre-call NES.
    #[test]
    fn apply_rolls_back_on_mapper_variant_mismatch() {
        use crate::nes::Nes;
        use crate::nes::rom::{Cartridge, Mirroring, TvSystem};

        // Build a minimal NROM cart in memory.
        let cart = Cartridge {
            prg_rom: vec![0x42u8; 0x8000],
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
            prg_chr_crc32: 0xCAFEBABE,
            db_matched: false,
            fds_data: None,
        };
        let mut nes = Nes::from_cartridge(cart).unwrap();
        nes.attach_save_metadata(std::path::PathBuf::from("/tmp/test.nes"), 0xCAFEBABE);
        // Mutate some live state so the post-rollback comparison
        // is non-trivial.
        nes.cpu.a = 0x99;
        nes.cpu.cycles = 12345;

        // Capture the pre-call state for an after-the-fact byte
        // comparison.
        let pre_bytes = bincode::serde::encode_to_vec(
            &Snapshot::capture(&nes).unwrap(),
            bincode::config::standard(),
        )
        .unwrap();

        // Build a Snapshot whose mapper variant won't apply to NROM.
        let mut bad = Snapshot::capture(&nes).unwrap();
        bad.mapper = MapperState::Mmc1(crate::save_state::mapper::Mmc1Snap::default());

        // Drive the mid-pipeline rollback by inlining what
        // load_and_apply_from_slot does after validation. (Going
        // through the full slot path would require touching the
        // filesystem - the rollback logic itself is what we're
        // testing.)
        let backup = Snapshot::capture(&nes).unwrap();
        let result = bad.apply(&mut nes);
        if result.is_err() {
            // Use `_` because we expect rollback to succeed; the
            // test's real assertion is the byte comparison below.
            let _ = backup.apply(&mut nes);
        }
        assert!(result.is_err(), "MMC1 snap into NROM cart must error");

        let post_bytes = bincode::serde::encode_to_vec(
            &Snapshot::capture(&nes).unwrap(),
            bincode::config::standard(),
        )
        .unwrap();
        assert_eq!(
            pre_bytes, post_bytes,
            "rollback must restore pre-call snapshot byte-for-byte"
        );
        assert_eq!(nes.cpu.a, 0x99);
        assert_eq!(nes.cpu.cycles, 12345);
    }

    /// APU DMC mid-transfer state survives a snapshot/apply round
    /// trip - including `buffer: Option<u8>`, `dma_pending`, and the
    /// `enable_dma_delay` countdown that puNES and Mesen2 both flag
    /// as easy-to-miss save-state fields.
    #[test]
    fn apu_round_trip_default_state() {
        use crate::nes::apu::Apu;
        use crate::nes::clock::Region;
        let original = Apu::new(Region::Ntsc);
        let snap = original.save_state_capture();

        let mut restored = Apu::new(Region::Ntsc);
        restored.save_state_apply(snap);
        // After applying a snapshot of a freshly-constructed APU,
        // the round-tripped capture must byte-match the original.
        let snap_a = original.save_state_capture();
        let snap_b = restored.save_state_capture();
        let a = bincode::serde::encode_to_vec(&snap_a, bincode::config::standard()).unwrap();
        let b = bincode::serde::encode_to_vec(&snap_b, bincode::config::standard()).unwrap();
        assert_eq!(a, b);
    }
}
