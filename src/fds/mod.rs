//! Famicom Disk System (FDS) support.
//!
//! The FDS is a disk-drive peripheral Nintendo sold in Japan 1986-1990.
//! Instead of a cart, games ship on a proprietary magnetic disk and
//! boot through an 8 KiB BIOS soldered into the "RAM Adapter" that
//! plugs into the Famicom cart slot. Software architecture:
//!
//! - **BIOS at `$E000-$FFFF`** supplies the reset vector, file-I/O
//!   routines, and error handlers. Games JSR into well-known entry
//!   points (LoadFiles at `$E445`, WriteFile at `$E1F8`, etc.) for
//!   every disk operation. Without the BIOS, reset vectors point into
//!   empty ROM and the CPU crashes on cycle one.
//! - **32 KiB PRG-RAM at `$6000-$DFFF`** holds the disk-loaded game
//!   code + data. Populated by the BIOS from the disk at power-on.
//! - **8 KiB CHR-RAM at PPU `$0000-$1FFF`** populated the same way.
//! - **Disk registers at `$4020-$4025` / `$4030-$4033`** control motor,
//!   read/write, CRC, and the 16-bit timer IRQ.
//! - **Audio at `$4040-$4097`** — 1 wavetable channel with FM
//!   modulator. Out of scope for the current phase (register file
//!   gets stored so audio-DSP work lands cleanly later).
//!
//! ## Scope of this module
//!
//! This is Phase 0 (file-format parsing + BIOS resolution + clean
//! error on missing BIOS). Subsequent phases layer on top:
//!
//! - **Phase 1** — mapper 20 implementation in [`crate::mapper::fds`],
//!   disk transport state machine, IRQ.
//! - **Phase 2** — disk-swap UI (eject / insert via the overlay).
//! - **Phase 3** — IPS-sidecar save persistence (writes during play
//!   get diffed against the original disk image).
//! - **Phase 4** — FDS audio synthesis (deferred alongside VRC6/7,
//!   MMC5, N163, Sunsoft 5B expansion audio).
//!
//! ## References
//!
//! - `~/Git/Mesen2/Core/NES/Mappers/FDS/Fds.{h,cpp}` — the best
//!   behavioral reference for the disk transport + BIOS handshake.
//!   GPL-3.0-or-later, same license as this project, so code patterns
//!   can be ported with attribution (per `CLAUDE.md` clean-room
//!   carve-out for non-core subsystems).
//! - `~/Git/Mesen2/Core/NES/Loaders/FdsLoader.{h,cpp}` — file-format
//!   parsing.
//! - nesdev.org/wiki/Family_Computer_Disk_System — complete hardware
//!   reference.

pub mod bios;
pub mod image;
pub mod ips;

pub use bios::{BiosError, FdsBios};
pub use image::{FdsImage, ImageError};

/// Runtime FDS bundle carried on `Cartridge::fds_data` when mapper
/// 20 is loaded. The mapper's constructor consumes this; ordinary
/// mappers leave it `None`.
///
/// Stays a plain data struct (no `Box<dyn ...>`) so mutating it from
/// inside the mapper after construction is straightforward.
#[derive(Debug, Clone)]
pub struct FdsData {
    /// Per-side data in the scan-ready form produced by
    /// [`FdsImage::gapped_sides`]: leading gap, per-block sync
    /// markers, fake CRCs, inter-block gaps. The disk transport's
    /// `disk_position` indexes into this.
    pub gapped_sides: Vec<Vec<u8>>,
    /// 56-byte disk-header block for each side — used by the auto-
    /// insert matching path (Phase 2+).
    pub headers: Vec<Vec<u8>>,
    /// The `disksys.rom` BIOS (exactly 8 KiB). Lives at `$E000-$FFFF`.
    pub bios: Vec<u8>,
    /// Whether the BIOS's CRC32 matched the known-good value — for
    /// diagnostic logging only; runtime behavior is identical either
    /// way.
    pub bios_known_good: bool,
    /// True when the source `.fds` file carried the 16-byte fwNES
    /// header. Phase 3 re-emits the header when saving a rebuilt
    /// image if the original had one.
    pub had_header: bool,
    /// Pristine per-side raw bytes (exactly [`image::SIDE_SIZE`] each)
    /// captured at load time. This is the diff base for IPS save
    /// encoding: the patch encodes the delta between "current disk"
    /// (rebuilt from the gapped runtime buffer) and "original disk"
    /// (this vec). On the load path, an IPS patch is applied to a
    /// reconstructed raw file built from these sides, which then gets
    /// re-sliced and re-gapped into a fresh runtime buffer.
    pub original_raw_sides: Vec<Vec<u8>>,
}
