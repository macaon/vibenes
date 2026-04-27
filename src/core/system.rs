// SPDX-License-Identifier: GPL-3.0-or-later
//! Console identification. Given a ROM file, decide whether to route
//! it through the NES core or the SNES core. Filename extension is a
//! useful hint but never authoritative - many SNES dumps carry `.smc`
//! while their NES counterparts ship as `.nes`, but a copier-headered
//! `.bin` is just as common, and some sets misname files. We always
//! confirm with a content sniff before committing to a core.

use std::fs;
use std::path::Path;

use anyhow::{anyhow, Context, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum System {
    Nes,
    Snes,
}

impl System {
    pub fn label(self) -> &'static str {
        match self {
            System::Nes => "NES",
            System::Snes => "SNES",
        }
    }
}

const INES_MAGIC: [u8; 4] = *b"NES\x1A";
const FDS_MAGIC: [u8; 4] = *b"FDS\x1A";
/// Disk-header block tag at side-offset 0 of a raw `.fds` (no fwNES
/// wrapper). Exactly one byte; pairs with [`FDS_NINTENDO_HVC`] at
/// side-offset 1 to identify a Famicom Disk System dump.
const FDS_BLOCK_TAG_DISK_HEADER: u8 = 0x01;
/// 14-byte ASCII signature that begins every legitimate FDS disk
/// header block. Sits at side-offset 1 in raw `.fds` dumps and at
/// offset 17 (1 + 16-byte header) in fwNES-wrapped dumps.
const FDS_NINTENDO_HVC: &[u8; 14] = b"*NINTENDO-HVC*";

/// Lightweight extension guess. Returns `None` for unknown
/// extensions so the caller falls back to a content sniff. Intended
/// to short-circuit the common case (a well-named `.nes` file)
/// without hitting disk twice on top of the regular load.
pub fn system_from_extension(path: &Path) -> Option<System> {
    let ext = path.extension().and_then(|e| e.to_str())?;
    match ext.to_ascii_lowercase().as_str() {
        "nes" | "fds" | "qd" => Some(System::Nes),
        "smc" | "sfc" | "fig" | "swc" => Some(System::Snes),
        _ => None,
    }
}

/// Sniff a console identity from raw bytes. iNES / fwNES magic is
/// authoritative for NES; otherwise we ask the SNES header detector
/// to score the bytes - any non-zero score means a plausible SNES
/// header is present at one of the six standard offsets.
pub fn detect_system_bytes(bytes: &[u8]) -> Option<System> {
    if bytes.len() >= 4 {
        if bytes[..4] == INES_MAGIC || bytes[..4] == FDS_MAGIC {
            return Some(System::Nes);
        }
    }
    // Raw `.fds` dumps (no fwNES wrapper) skip the magic and start
    // directly with side 0: a 0x01 block tag followed by the
    // *NINTENDO-HVC* disk-header signature. Without this branch the
    // disk bytes fall through to the SNES header probe and score
    // high enough to misroute Zelda et al. into the SNES core.
    if bytes.len() >= 1 + FDS_NINTENDO_HVC.len()
        && bytes[0] == FDS_BLOCK_TAG_DISK_HEADER
        && &bytes[1..1 + FDS_NINTENDO_HVC.len()] == FDS_NINTENDO_HVC.as_slice()
    {
        return Some(System::Nes);
    }
    if crate::snes::rom::looks_like_snes(bytes) {
        return Some(System::Snes);
    }
    None
}

/// Combined sniff: extension hint, then content. The content sniff
/// always runs (and overrides) when the extension says SNES but the
/// bytes are clearly iNES, or vice versa - we trust the bytes over
/// the filename. Returns an error rather than guessing when neither
/// signal is clear, so the host can surface "unknown ROM format"
/// to the user instead of silently picking a core.
pub fn detect_system(path: &Path) -> Result<System> {
    let bytes = fs::read(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    if let Some(sys) = detect_system_bytes(&bytes) {
        return Ok(sys);
    }
    // Fall back to extension only when the bytes were inconclusive
    // (e.g. tiny file, or a corrupted SNES header that didn't score).
    if let Some(sys) = system_from_extension(path) {
        return Ok(sys);
    }
    Err(anyhow!(
        "could not identify {} as NES or SNES ROM",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_hints_for_known_suffixes() {
        assert_eq!(
            system_from_extension(Path::new("game.nes")),
            Some(System::Nes)
        );
        assert_eq!(
            system_from_extension(Path::new("game.smc")),
            Some(System::Snes)
        );
        assert_eq!(
            system_from_extension(Path::new("game.SFC")),
            Some(System::Snes)
        );
        assert_eq!(system_from_extension(Path::new("game.bin")), None);
    }

    #[test]
    fn ines_magic_wins_over_extension() {
        let mut bytes = vec![0u8; 32];
        bytes[..4].copy_from_slice(&INES_MAGIC);
        assert_eq!(detect_system_bytes(&bytes), Some(System::Nes));
    }

    #[test]
    fn returns_none_on_garbage() {
        assert_eq!(detect_system_bytes(&[0u8; 16]), None);
    }

    #[test]
    fn raw_fds_without_fwnes_header_detected_as_nes() {
        // Raw `.fds` dumps (e.g. Zelda no Densetsu v1.1) skip the
        // 16-byte fwNES wrapper. Side 0 starts with the 0x01 disk
        // header tag, then *NINTENDO-HVC* at side-offset 1. Pad up
        // past the SNES probe threshold so we can confirm the
        // raw-FDS check wins before `looks_like_snes` runs.
        let mut bytes = vec![0u8; 0x8000];
        bytes[0] = FDS_BLOCK_TAG_DISK_HEADER;
        bytes[1..1 + FDS_NINTENDO_HVC.len()].copy_from_slice(FDS_NINTENDO_HVC);
        assert_eq!(detect_system_bytes(&bytes), Some(System::Nes));
    }

    #[test]
    fn fwnes_wrapped_fds_still_detected_as_nes() {
        // fwNES-wrapped `.fds` keeps its `FDS\x1A` magic at offset 0;
        // the existing magic check already handles it. Lock that in
        // as a regression test alongside the raw-FDS coverage.
        let mut bytes = vec![0u8; 32];
        bytes[..4].copy_from_slice(&FDS_MAGIC);
        assert_eq!(detect_system_bytes(&bytes), Some(System::Nes));
    }
}
