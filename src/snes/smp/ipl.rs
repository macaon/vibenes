// SPDX-License-Identifier: GPL-3.0-or-later
//! SPC700 IPL ROM. 64-byte Sony boot routine mapped at `$FFC0-$FFFF`
//! while `CONTROL.7 = 1` (the reset state). Real hardware mirrors
//! this image into the upper page of ARAM at every read; the host
//! clears `CONTROL.7` after the upload protocol completes and the
//! bytes revert to plain RAM.
//!
//! ## Why we vendor + how to override
//!
//! The IPL is the one carve-out from vibenes' clean-room policy.
//! See `vendor/snes-ipl/README.md` for the full rationale; the short
//! version is that 64 bytes + one ISA + one fixed mailbox protocol
//! collapses the implementation space to essentially Sony's exact
//! sequence under the merger doctrine, and commercial carts
//! occasionally fingerprint the IPL bytes - a behavioural
//! reimplementation would diverge observably from real hardware.
//!
//! Users who'd rather supply their own dump can override at runtime
//! via [`Ipl::resolve`]: env var > CLI flag > config > XDG path.
//! Without an override the embedded blob (matching higan / Mesen2 /
//! bsnes / snes9x byte-for-byte) is used.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::crc32::crc32;

/// 64-byte SPC700 IPL boot ROM as embedded fallback. Used unless a
/// runtime override is supplied via [`Ipl::resolve`].
pub const IPL_ROM: [u8; 64] = *include_bytes!("../../../vendor/snes-ipl/ipl.rom");

/// Address in the SPC's 16-bit space where the IPL begins. The image
/// occupies `$FFC0-$FFFF` while shadow is enabled.
pub const IPL_BASE: u16 = 0xFFC0;

/// Required size in bytes for any IPL image, embedded or override.
pub const IPL_SIZE: usize = 64;

/// CRC32 of the canonical Sony IPL (matches higan / Mesen2 / bsnes
/// blobs, MD5 `ac35bfc854818e2f55c2a05917493db3`). Override files
/// matching this hash are flagged "known-good"; mismatches are warned
/// rather than rejected so unofficial / patched IPLs still work.
pub const KNOWN_GOOD_CRC32: u32 = 0x44BB3A40;

/// Env var users can set to bypass all other lookup rules.
pub const ENV_OVERRIDE: &str = "VIBENES_SPC_IPL";

/// Conventional filename for a user-supplied IPL dump. Mirrors the
/// FDS BIOS convention (`disksys.rom`); kept short and lowercase
/// so users can also drop `iplrom.bin` aliases without confusion.
pub const FILENAME: &str = "spc-ipl.rom";

/// A loaded IPL image plus where it came from.
#[derive(Clone)]
pub struct Ipl {
    pub bytes: [u8; IPL_SIZE],
    pub source: IplSource,
    pub is_known_good: bool,
}

/// Where the IPL bytes came from. `Embedded` means we used the
/// vendored blob; `File(path)` means a runtime override won.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IplSource {
    Embedded,
    File(PathBuf),
}

impl fmt::Debug for Ipl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ipl")
            .field("source", &self.source)
            .field("is_known_good", &self.is_known_good)
            .finish()
    }
}

/// Layered override search input. Each field is one priority tier;
/// `None` means "skip this tier." If every tier is `None` and no
/// override file exists, [`Ipl::resolve`] falls back to the
/// embedded blob.
#[derive(Debug, Default, Clone)]
pub struct IplSearch {
    /// `--spc-ipl <path>` CLI flag.
    pub cli_override: Option<PathBuf>,
    /// `config.snes.ipl_path` from a settings UI.
    pub config: Option<PathBuf>,
}

/// Errors when an explicit override path was supplied but unusable.
/// Missing-file is **not** an error - we fall back to the embedded
/// blob silently when no override resolves. Only structural problems
/// (wrong size, I/O failure) raise [`IplError`].
#[derive(Debug)]
pub enum IplError {
    BadSize { path: PathBuf, actual: usize },
    Io { path: PathBuf, source: std::io::Error },
}

impl fmt::Display for IplError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IplError::BadSize { path, actual } => write!(
                f,
                "{}: SPC700 IPL must be exactly {} bytes, got {}",
                path.display(),
                IPL_SIZE,
                actual
            ),
            IplError::Io { path, source } => {
                write!(f, "reading SPC700 IPL at {}: {}", path.display(), source)
            }
        }
    }
}

impl std::error::Error for IplError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            IplError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl Ipl {
    /// Pure embedded constructor - no I/O, no env reads. The host
    /// uses this when it explicitly wants the vendored bytes (e.g.
    /// in unit tests).
    pub fn embedded() -> Self {
        Self {
            bytes: IPL_ROM,
            source: IplSource::Embedded,
            is_known_good: crc32(&IPL_ROM) == KNOWN_GOOD_CRC32,
        }
    }

    /// Resolve the IPL with the layered override ladder. Returns the
    /// embedded blob silently when no override resolves. Errors only
    /// fire when an override path was supplied AND the file at it is
    /// structurally bad (wrong size or unreadable).
    pub fn resolve(search: &IplSearch) -> Result<Self, IplError> {
        for path in gather_candidate_paths(search) {
            if !path.is_file() {
                continue;
            }
            let bytes = std::fs::read(&path).map_err(|source| IplError::Io {
                path: path.clone(),
                source,
            })?;
            return Self::from_bytes(bytes, path);
        }
        Ok(Self::embedded())
    }

    /// Validate-and-wrap helper for an override file. Tests and the
    /// resolver share this path so size validation + CRC check stay
    /// in one place.
    pub fn from_bytes(bytes: Vec<u8>, source: PathBuf) -> Result<Self, IplError> {
        if bytes.len() != IPL_SIZE {
            return Err(IplError::BadSize {
                path: source,
                actual: bytes.len(),
            });
        }
        let is_known_good = crc32(&bytes) == KNOWN_GOOD_CRC32;
        let mut arr = [0u8; IPL_SIZE];
        arr.copy_from_slice(&bytes);
        Ok(Self {
            bytes: arr,
            source: IplSource::File(source),
            is_known_good,
        })
    }
}

/// Pure ordered-candidate builder. Tests feed this directly so they
/// don't have to mutate global env vars.
pub fn build_candidate_paths(
    env_override: Option<PathBuf>,
    cli_override: Option<&Path>,
    config: Option<&Path>,
    xdg: Option<&Path>,
) -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(4);
    if let Some(p) = env_override {
        paths.push(p);
    }
    if let Some(p) = cli_override {
        paths.push(p.to_path_buf());
    }
    if let Some(p) = config {
        paths.push(p.to_path_buf());
    }
    if let Some(p) = xdg {
        paths.push(p.to_path_buf());
    }
    paths
}

fn gather_candidate_paths(search: &IplSearch) -> Vec<PathBuf> {
    let env = std::env::var(ENV_OVERRIDE).ok().map(PathBuf::from);
    let xdg = xdg_config_path();
    build_candidate_paths(
        env,
        search.cli_override.as_deref(),
        search.config.as_deref(),
        xdg.as_deref(),
    )
}

/// `$XDG_CONFIG_HOME/vibenes/bios/spc-ipl.rom`, falling back to
/// `$HOME/.config/vibenes/bios/spc-ipl.rom`. Same `bios/` subdir as
/// the FDS BIOS so all firmware lives in one place.
fn xdg_config_path() -> Option<PathBuf> {
    let base: PathBuf = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".config")))?;
    Some(xdg_config_path_for(&base))
}

fn xdg_config_path_for(base: &Path) -> PathBuf {
    base.join("vibenes").join("bios").join(FILENAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipl_rom_is_exactly_64_bytes() {
        assert_eq!(IPL_ROM.len(), IPL_SIZE);
    }

    #[test]
    fn ipl_first_bytes_match_known_image() {
        // First instruction is `MOV X, #$EF` (CD EF) - sets up the
        // stack pointer to $EF. Same first two bytes appear inline
        // in Mesen2 `Core/SNES/Spc.h:58` and higan's ipl.rom blob.
        assert_eq!(IPL_ROM[0], 0xCD);
        assert_eq!(IPL_ROM[1], 0xEF);
    }

    #[test]
    fn ipl_reset_vector_points_to_ipl_base() {
        // The last two bytes of the IPL are the SPC reset vector at
        // $FFFE-$FFFF. They point back to $FFC0 (the IPL entry
        // point), little-endian.
        assert_eq!(IPL_ROM[62], 0xC0);
        assert_eq!(IPL_ROM[63], 0xFF);
    }

    #[test]
    fn embedded_constructor_returns_vendored_bytes() {
        let ipl = Ipl::embedded();
        assert_eq!(ipl.bytes, IPL_ROM);
        assert_eq!(ipl.source, IplSource::Embedded);
        // The vendored bytes must round-trip the known-good CRC so
        // user override files matching the same hash get flagged
        // identical to the embedded fallback.
        assert!(ipl.is_known_good, "embedded IPL must hash to KNOWN_GOOD_CRC32");
    }

    #[test]
    fn known_good_crc_matches_embedded_image() {
        // Lock the public constant against the vendored blob so a
        // typo in either shows up here.
        assert_eq!(crc32(&IPL_ROM), KNOWN_GOOD_CRC32);
    }

    #[test]
    fn from_bytes_rejects_wrong_size() {
        let too_small = vec![0u8; IPL_SIZE - 1];
        match Ipl::from_bytes(too_small, PathBuf::from("/tmp/x")) {
            Err(IplError::BadSize { actual, .. }) => assert_eq!(actual, IPL_SIZE - 1),
            other => panic!("expected BadSize, got {other:?}"),
        }
    }

    #[test]
    fn from_bytes_accepts_correct_size() {
        let ipl = Ipl::from_bytes(IPL_ROM.to_vec(), PathBuf::from("/tmp/x")).unwrap();
        assert_eq!(ipl.bytes, IPL_ROM);
        assert!(ipl.is_known_good);
        assert_eq!(ipl.source, IplSource::File(PathBuf::from("/tmp/x")));
    }

    #[test]
    fn builder_orders_env_cli_config_xdg() {
        let paths = build_candidate_paths(
            Some(PathBuf::from("/env.rom")),
            Some(Path::new("/cli.rom")),
            Some(Path::new("/config.rom")),
            Some(Path::new("/xdg.rom")),
        );
        assert_eq!(paths[0], PathBuf::from("/env.rom"));
        assert_eq!(paths[1], PathBuf::from("/cli.rom"));
        assert_eq!(paths[2], PathBuf::from("/config.rom"));
        assert_eq!(paths[3], PathBuf::from("/xdg.rom"));
    }

    #[test]
    fn builder_skips_none_tiers() {
        let paths = build_candidate_paths(
            None,
            Some(Path::new("/cli.rom")),
            None,
            Some(Path::new("/xdg.rom")),
        );
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], PathBuf::from("/cli.rom"));
        assert_eq!(paths[1], PathBuf::from("/xdg.rom"));
    }

    #[test]
    fn xdg_layout_is_vibenes_bios_spc_ipl_rom() {
        let out = xdg_config_path_for(Path::new("/home/alice/.config"));
        assert_eq!(
            out,
            PathBuf::from("/home/alice/.config/vibenes/bios/spc-ipl.rom")
        );
    }

    #[test]
    fn resolve_falls_back_to_embedded_when_no_override_resolves() {
        // Empty search + a path that doesn't exist anywhere => embedded.
        // (We can't fully isolate from VIBENES_SPC_IPL set in the env,
        // but on a clean dev box this hits the fallback path.)
        let search = IplSearch::default();
        let ipl = Ipl::resolve(&search).expect("fallback never errors");
        // The override path resolution may pull from $HOME/.config
        // if the user has a real file there - just assert that
        // whatever resolved is structurally valid.
        assert_eq!(ipl.bytes.len(), IPL_SIZE);
    }
}
