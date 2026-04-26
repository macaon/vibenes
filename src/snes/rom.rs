// SPDX-License-Identifier: GPL-3.0-or-later
//! SNES (Super Famicom) ROM loader.
//!
//! Unlike iNES (which has a hard 4-byte magic), SNES carts use the
//! original cartridge ROM verbatim with a 64-byte header at one of
//! three locations depending on the mapping mode:
//!
//! - **LoROM** ($20): header at file offset $7FC0
//! - **HiROM** ($21): header at file offset $FFC0
//! - **ExHiROM** ($25): header at file offset $40FFC0
//!
//! Some dumps are wrapped in a 512-byte SMC copier header; others
//! are headerless. We don't trust file extensions or the mode byte
//! alone - both can lie or be corrupted. Instead we probe all six
//! base addresses (3 mappings × {headerless, headered}), score each
//! by how "header-shaped" the data looks (mode byte plausibility,
//! checksum vs complement, plausible reset vector, plausible first
//! opcode), and pick the highest scorer.
//!
//! Algorithm and scoring mirror Mesen2's `BaseCartridge::GetHeaderScore`
//! (Core/SNES/BaseCartridge.cpp) and the snes.nesdev.org wiki "ROM
//! header" page; see also the nes-expert SNES CPU reference.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use crate::core::Region;
use crate::crc32::crc32;

const COPIER_HEADER_SIZE: usize = 512;

/// Six combinations of (mapping × copier-header presence) Mesen2
/// probes. Each base address is the file offset where bank 0 of the
/// cart begins; the standard header lives `+0x7FC0` from that base.
const PROBE_BASES: [usize; 6] = [
    0x000000, // LoROM, no copier header
    0x000200, // LoROM, with copier header
    0x008000, // HiROM, no copier header
    0x008200, // HiROM, with copier header
    0x408000, // ExHiROM, no copier header
    0x408200, // ExHiROM, with copier header
];

/// Cartridge mapping mode. Determines how 24-bit CPU addresses route
/// onto the linear ROM image and where SRAM lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapMode {
    LoRom,
    HiRom,
    ExHiRom,
    ExLoRom,
}

impl MapMode {
    pub fn label(self) -> &'static str {
        match self {
            MapMode::LoRom => "LoROM",
            MapMode::HiRom => "HiROM",
            MapMode::ExHiRom => "ExHiROM",
            MapMode::ExLoRom => "ExLoROM",
        }
    }
}

/// On-cartridge coprocessor inferred from the chipset byte. We don't
/// implement any of these in Phase 1; the field is informational so
/// the F1 overlay and `cargo run` log can surface what would be
/// needed to actually run the game.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coprocessor {
    None,
    Dsp1,
    Dsp2,
    Dsp3,
    Dsp4,
    SuperFx,
    Obc1,
    Sa1,
    Sdd1,
    Srtc,
    Spc7110,
    St010,
    St011,
    St018,
    Cx4,
    SuperGameBoy,
    Satellaview,
    Unknown,
}

impl Coprocessor {
    pub fn label(self) -> &'static str {
        match self {
            Coprocessor::None => "none",
            Coprocessor::Dsp1 => "DSP-1",
            Coprocessor::Dsp2 => "DSP-2",
            Coprocessor::Dsp3 => "DSP-3",
            Coprocessor::Dsp4 => "DSP-4",
            Coprocessor::SuperFx => "SuperFX (GSU)",
            Coprocessor::Obc1 => "OBC-1",
            Coprocessor::Sa1 => "SA-1",
            Coprocessor::Sdd1 => "S-DD1",
            Coprocessor::Srtc => "S-RTC",
            Coprocessor::Spc7110 => "SPC7110",
            Coprocessor::St010 => "ST010",
            Coprocessor::St011 => "ST011",
            Coprocessor::St018 => "ST018",
            Coprocessor::Cx4 => "Cx4",
            Coprocessor::SuperGameBoy => "Super Game Boy",
            Coprocessor::Satellaview => "Satellaview",
            Coprocessor::Unknown => "unknown",
        }
    }
}

/// Parsed contents of the 64-byte cart header at `$7FC0` / `$FFC0` /
/// `$40FFC0`. Plus the 16-byte extended header prefix Nintendo added
/// later (maker code, game code, expansion RAM size, chipset subtype).
#[derive(Debug, Clone)]
pub struct Header {
    pub title: String,
    pub map_mode: MapMode,
    pub fast_rom: bool,
    pub rom_type: u8,
    pub coprocessor: Coprocessor,
    pub has_battery: bool,
    pub rom_size_bytes: usize,
    pub sram_size_bytes: usize,
    pub destination: u8,
    pub version: u8,
    pub checksum: u16,
    pub checksum_complement: u16,
    pub maker_code: [u8; 2],
    pub game_code: [u8; 4],
    pub expansion_ram_size: u8,
    /// Native-mode reset vector ($FFFC). Used by the score function
    /// to validate "looks like a reset vector," and surfaced for
    /// debug logging.
    pub reset_vector: u16,
}

/// A loaded SNES cartridge: the ROM payload plus the parsed header.
/// `rom` has any 512-byte copier header stripped before storage.
#[derive(Debug, Clone)]
pub struct Cartridge {
    pub rom: Vec<u8>,
    pub header: Header,
    pub region: Region,
    /// CRC32 of the post-copier-header ROM bytes. Used as the save-
    /// path key (mirrors NES `prg_chr_crc32`).
    pub rom_crc32: u32,
    /// Where in the post-copier-header ROM the standard 64-byte
    /// header starts ($7FC0 for LoROM, $FFC0 for HiROM, $40FFC0 for
    /// ExHiROM). Cached so callers reading vectors etc. don't have
    /// to recompute it.
    pub header_offset: usize,
    /// True when the original file carried a 512-byte SMC copier
    /// prefix that we stripped. Surfaced for the F1 overlay.
    pub had_copier_header: bool,
}

impl Cartridge {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let bytes = fs::read(path)
            .with_context(|| format!("failed to read SNES ROM: {}", path.display()))?;
        Self::parse(bytes).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn parse(mut bytes: Vec<u8>) -> Result<Self> {
        if bytes.len() < 0x8000 {
            bail!(
                "ROM too small to be a SNES cartridge: {} bytes (need at least 32 KiB)",
                bytes.len()
            );
        }

        let detection = detect_header(&bytes)
            .ok_or_else(|| anyhow!("no plausible SNES header at any of the 6 standard offsets"))?;

        if detection.had_copier_header {
            bytes.drain(..COPIER_HEADER_SIZE);
        }

        let header_offset = detection.header_offset_in_payload;
        let header = parse_header(&bytes, header_offset, detection.map_mode)?;
        let region = region_from_destination(header.destination);
        let rom_crc32 = crc32(&bytes);

        Ok(Self {
            rom: bytes,
            header,
            region,
            rom_crc32,
            header_offset,
            had_copier_header: detection.had_copier_header,
        })
    }

    pub fn rom_path_hint(&self) -> Option<PathBuf> {
        None
    }

    /// Single-line summary suitable for the F1 overlay or a CLI log.
    /// Mirrors the NES `Cartridge::describe` style so the two cores
    /// produce visually consistent boot lines.
    pub fn describe(&self) -> String {
        let h = &self.header;
        format!(
            "SNES {map} {fast} title=\"{title}\" rom={rom_kib}KiB sram={sram_kib}KiB region={region:?} coproc={coproc} battery={bat} crc={crc:08X}{copier}",
            map = h.map_mode.label(),
            fast = if h.fast_rom { "FastROM" } else { "SlowROM" },
            title = h.title,
            rom_kib = h.rom_size_bytes / 1024,
            sram_kib = h.sram_size_bytes / 1024,
            region = self.region,
            coproc = h.coprocessor.label(),
            bat = h.has_battery,
            crc = self.rom_crc32,
            copier = if self.had_copier_header {
                " (had copier header, stripped)"
            } else {
                ""
            },
        )
    }
}

/// Cheap content sniff: does anything in `bytes` look like a valid
/// SNES header at one of the standard offsets? Used by
/// [`crate::core::system::detect_system_bytes`] to disambiguate
/// when the extension is ambiguous and there's no iNES/fwNES magic.
/// Returning `false` is not the same as "definitely not SNES" - a
/// corrupted or extremely tiny dump still gets a `false` here, and
/// the caller falls back to the extension hint.
pub fn looks_like_snes(bytes: &[u8]) -> bool {
    if bytes.len() < 0x8000 {
        return false;
    }
    detect_header(bytes).is_some()
}

/// Result of the per-base-address probe: which mapping won, whether
/// a copier header was present, and where the 64-byte header starts
/// in the post-copier ROM bytes.
#[derive(Debug, Clone, Copy)]
struct Detection {
    map_mode: MapMode,
    had_copier_header: bool,
    /// Offset of the 64-byte header *after* a copier header (if any)
    /// has been stripped. Always one of $7FC0 / $FFC0 / $40FFC0.
    header_offset_in_payload: usize,
}

fn detect_header(rom: &[u8]) -> Option<Detection> {
    let mut best: Option<(i32, Detection)> = None;
    for &base in &PROBE_BASES {
        let Some(score) = score_at(rom, base) else {
            continue;
        };
        let had_copier_header = (base & 0x200) != 0;
        let copier_offset = if had_copier_header {
            COPIER_HEADER_SIZE
        } else {
            0
        };
        let header_offset_in_payload = base.saturating_sub(copier_offset) + 0x7FC0;
        let map_mode = pick_map_mode(rom, base);

        let detection = Detection {
            map_mode,
            had_copier_header,
            header_offset_in_payload,
        };

        if best.map_or(true, |(s, _)| score >= s) {
            best = Some((score, detection));
        }
    }
    best.map(|(_, d)| d)
}

/// Score how "header-shaped" the bytes at `base + 0x7FC0` look, using
/// the same heuristics Mesen2 uses (paraphrased in Rust). Returns
/// `None` when the candidate is structurally invalid (file too short
/// to host the header + reset vector, or reset vector pointing into
/// hardware/zero-page).
fn score_at(rom: &[u8], base: usize) -> Option<i32> {
    let header_start = base.checked_add(0x7FC0)?;
    let header_end = header_start.checked_add(0x40)?;
    if header_end > rom.len() {
        return None;
    }

    let map_mode_byte = rom[header_start + 0x15] & !0x10; // strip FastROM bit
    let rom_type = rom[header_start + 0x16];
    let rom_size = rom[header_start + 0x17];
    let sram_size = rom[header_start + 0x18];
    let complement = u16::from_le_bytes([rom[header_start + 0x1C], rom[header_start + 0x1D]]);
    let checksum = u16::from_le_bytes([rom[header_start + 0x1E], rom[header_start + 0x1F]]);
    let reset_vector = u16::from_le_bytes([rom[header_start + 0x3C], rom[header_start + 0x3D]]);

    let mut score = 0i32;

    // Mode byte must agree with where the header was found:
    // LoROM headers (mode $20/$22) live before $8000 in their bank;
    // HiROM/ExHiROM headers (mode $21/$25) live at or above $8000.
    let in_lorom_half = (base & 0x8000) == 0;
    if in_lorom_half {
        if map_mode_byte == 0x20 || map_mode_byte == 0x22 {
            score += 1;
        }
    } else if map_mode_byte == 0x21 || map_mode_byte == 0x25 {
        score += 1;
    }

    if rom_type < 0x08 {
        score += 1;
    }
    if rom_size < 0x10 {
        score += 1;
    }
    if sram_size < 0x08 {
        score += 1;
    }

    let sum = checksum.wrapping_add(complement);
    if sum == 0xFFFF && checksum != 0 && complement != 0 {
        score += 8;
    }

    if reset_vector < 0x8000 {
        return None;
    }

    // First opcode at the reset vector tells us a lot. The mode byte
    // can match by accident; getting an opcode that boot code
    // actually uses (CLI/SEI/JMP/JSR/STZ before entering the main
    // loop) is much rarer.
    let opcode_offset = base.wrapping_add(reset_vector as usize & 0x7FFF);
    if let Some(&op) = rom.get(opcode_offset) {
        match op {
            0x18 | 0x78 | 0x4C | 0x5C | 0x20 | 0x22 | 0x9C => score += 8, // CLI/SEI/JMP/JML/JSR/JSL/STZ
            0xC2 | 0xE2 | 0xA9 | 0xA2 | 0xA0 => score += 4, // REP/SEP/LDA-imm/LDX-imm/LDY-imm
            0x00 | 0xFF | 0xCC => score -= 8,                // BRK/SBC-imm/CPY-imm
            _ => {}
        }
    }

    Some(score.max(0))
}

fn pick_map_mode(rom: &[u8], base: usize) -> MapMode {
    let header_start = base + 0x7FC0;
    let map_byte = rom.get(header_start + 0x15).copied().unwrap_or(0) & !0x10;

    let in_lorom_half = (base & 0x8000) == 0;
    let in_ex_half = (base & 0x400000) != 0;

    if in_lorom_half {
        if map_byte == 0x22 {
            MapMode::ExLoRom
        } else {
            MapMode::LoRom
        }
    } else if in_ex_half || map_byte == 0x25 {
        MapMode::ExHiRom
    } else {
        MapMode::HiRom
    }
}

fn parse_header(rom: &[u8], header_offset: usize, map_mode: MapMode) -> Result<Header> {
    if header_offset + 0x40 > rom.len() {
        bail!(
            "header offset {:#X} + 0x40 exceeds ROM size {}",
            header_offset,
            rom.len()
        );
    }

    let title = decode_title(&rom[header_offset..header_offset + 21]);
    let map_byte = rom[header_offset + 0x15];
    let fast_rom = map_byte & 0x10 != 0;
    let rom_type = rom[header_offset + 0x16];
    let rom_size_byte = rom[header_offset + 0x17];
    let sram_size_byte = rom[header_offset + 0x18];
    let destination = rom[header_offset + 0x19];
    let _developer_id = rom[header_offset + 0x1A];
    let version = rom[header_offset + 0x1B];
    let checksum_complement = u16::from_le_bytes([rom[header_offset + 0x1C], rom[header_offset + 0x1D]]);
    let checksum = u16::from_le_bytes([rom[header_offset + 0x1E], rom[header_offset + 0x1F]]);
    let reset_vector = u16::from_le_bytes([rom[header_offset + 0x3C], rom[header_offset + 0x3D]]);

    // Extended header at header_offset - 0x10 (only present when
    // DeveloperId == 0x33; older carts have garbage there). We pull
    // it unconditionally and let consumers decide how much to trust.
    let ext_offset = header_offset.checked_sub(0x10).unwrap_or(0);
    let mut maker_code = [0u8; 2];
    let mut game_code = [0u8; 4];
    let mut expansion_ram_size = 0u8;
    if header_offset >= 0x10 {
        maker_code.copy_from_slice(&rom[ext_offset..ext_offset + 2]);
        game_code.copy_from_slice(&rom[ext_offset + 2..ext_offset + 6]);
        expansion_ram_size = rom[ext_offset + 0x0D];
    }

    let rom_size_bytes = if rom_size_byte > 0 && rom_size_byte < 0x10 {
        1024usize << rom_size_byte
    } else {
        rom.len()
    };
    let sram_size_bytes = if sram_size_byte > 0 && sram_size_byte < 0x10 {
        1024usize << sram_size_byte
    } else {
        0
    };

    let coprocessor = classify_coprocessor(rom_type, rom[header_offset.saturating_sub(1)]);
    let has_battery = matches!(rom_type & 0x0F, 0x02 | 0x05 | 0x06 | 0x09 | 0x0A);

    Ok(Header {
        title,
        map_mode,
        fast_rom,
        rom_type,
        coprocessor,
        has_battery,
        rom_size_bytes,
        sram_size_bytes,
        destination,
        version,
        checksum,
        checksum_complement,
        maker_code,
        game_code,
        expansion_ram_size,
        reset_vector,
    })
}

/// Cart titles are 21 bytes of Shift-JIS in theory, but commercial
/// games stuck to ASCII printable + space padding. Treat anything
/// outside printable-ASCII as `?` and trim trailing whitespace.
fn decode_title(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len());
    for &b in bytes {
        if (0x20..=0x7E).contains(&b) {
            s.push(b as char);
        } else {
            s.push('?');
        }
    }
    s.trim_end().to_string()
}

fn classify_coprocessor(rom_type: u8, cartridge_subtype: u8) -> Coprocessor {
    if (rom_type & 0x0F) < 0x03 {
        return Coprocessor::None;
    }
    let class = (rom_type & 0xF0) >> 4;
    match class {
        0x0 => Coprocessor::Dsp1, // DSP-1/2/3/4 disambiguated by title in Mesen; default to DSP-1 here
        0x1 => Coprocessor::SuperFx,
        0x2 => Coprocessor::Obc1,
        0x3 => Coprocessor::Sa1,
        0x4 => Coprocessor::Sdd1,
        0x5 => Coprocessor::Srtc,
        0xE => match rom_type {
            0xE3 => Coprocessor::SuperGameBoy,
            0xE5 => Coprocessor::Satellaview,
            _ => Coprocessor::Unknown,
        },
        0xF => match cartridge_subtype {
            0x00 => Coprocessor::Spc7110,
            0x01 => Coprocessor::St010, // ST010/ST011 distinguished by title
            0x02 => Coprocessor::St018,
            0x10 => Coprocessor::Cx4,
            _ => Coprocessor::Unknown,
        },
        _ => Coprocessor::Unknown,
    }
}

/// Destination code -> NTSC/PAL. Codes 0, 1, 13 (USA/Japan/Korea) and
/// other Americas/Asia codes are NTSC; European/Australian codes are
/// PAL. Unknown codes default to NTSC, matching Mesen2.
fn region_from_destination(dest: u8) -> Region {
    match dest {
        0x02 | 0x03 | 0x04 | 0x05 | 0x06 | 0x07 | 0x08 | 0x09 | 0x0A | 0x0B | 0x0C | 0x11 => {
            Region::Pal
        }
        _ => Region::Ntsc,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic 32 KiB LoROM-style ROM with a valid header
    /// at $7FC0, a checksum/complement pair that sums to $FFFF, and
    /// a SEI at the reset vector. This is the smallest legitimate
    /// header layout the detector should accept.
    fn synth_lorom(title: &str, dest: u8) -> Vec<u8> {
        let mut rom = vec![0u8; 0x8000];
        // CartName at $7FC0
        let bytes = title.as_bytes();
        let n = bytes.len().min(21);
        rom[0x7FC0..0x7FC0 + n].copy_from_slice(&bytes[..n]);
        for i in n..21 {
            rom[0x7FC0 + i] = b' ';
        }
        rom[0x7FC0 + 0x15] = 0x20; // LoROM
        rom[0x7FC0 + 0x16] = 0x00; // RomType
        rom[0x7FC0 + 0x17] = 0x07; // 128 KiB advertised
        rom[0x7FC0 + 0x18] = 0x00; // no SRAM
        rom[0x7FC0 + 0x19] = dest;
        rom[0x7FC0 + 0x1B] = 0x00;
        // checksum 0x1234, complement 0xEDCB so sum == 0xFFFF
        rom[0x7FC0 + 0x1C] = 0xCB;
        rom[0x7FC0 + 0x1D] = 0xED;
        rom[0x7FC0 + 0x1E] = 0x34;
        rom[0x7FC0 + 0x1F] = 0x12;
        // Native vectors zeroed; emulation reset vector at $7FFC -> $8000
        rom[0x7FFC] = 0x00;
        rom[0x7FFD] = 0x80;
        // Place an SEI at the reset target so the opcode-score bonus fires.
        rom[0x0000] = 0x78;
        rom
    }

    #[test]
    fn detects_synthetic_lorom_with_valid_checksum() {
        let rom = synth_lorom("VIBENES SNES        ", 0x01);
        let cart = Cartridge::parse(rom).expect("parse");
        assert_eq!(cart.header.map_mode, MapMode::LoRom);
        assert_eq!(cart.header.title.trim(), "VIBENES SNES");
        assert_eq!(cart.header.checksum, 0x1234);
        assert_eq!(cart.header.checksum_complement, 0xEDCB);
        assert!(!cart.had_copier_header);
        assert_eq!(cart.region, Region::Ntsc);
    }

    #[test]
    fn strips_512_byte_copier_header() {
        let mut rom = vec![0u8; COPIER_HEADER_SIZE];
        rom.extend_from_slice(&synth_lorom("HEADERED COPY", 0x00));
        let cart = Cartridge::parse(rom).expect("parse");
        assert!(cart.had_copier_header);
        assert_eq!(cart.rom.len(), 0x8000);
        assert_eq!(cart.header.title, "HEADERED COPY");
    }

    #[test]
    fn detects_synthetic_hirom() {
        // HiROM puts the header at $FFC0; we need at least 64 KiB.
        let mut rom = vec![0u8; 0x10000];
        let title = b"HIROM SAMPLE         ";
        rom[0xFFC0..0xFFC0 + 21].copy_from_slice(&title[..21]);
        rom[0xFFC0 + 0x15] = 0x21; // HiROM
        rom[0xFFC0 + 0x17] = 0x08;
        rom[0xFFC0 + 0x1C] = 0xAA;
        rom[0xFFC0 + 0x1D] = 0xBB;
        rom[0xFFC0 + 0x1E] = 0x55;
        rom[0xFFC0 + 0x1F] = 0x44;
        // sum: 0xBBAA + 0x4455 = 0xFFFF
        // emulation reset vector at $FFFC -> $8000
        rom[0xFFFC] = 0x00;
        rom[0xFFFD] = 0x80;
        // First opcode at bank 0:$8000 -> file offset $8000.
        rom[0x8000] = 0x78; // SEI

        let cart = Cartridge::parse(rom).expect("parse");
        assert_eq!(cart.header.map_mode, MapMode::HiRom);
    }

    #[test]
    fn rejects_too_small_file() {
        let err = Cartridge::parse(vec![0u8; 0x100]).unwrap_err();
        assert!(err.to_string().contains("too small"));
    }

    #[test]
    fn region_from_destination_picks_pal_for_europe() {
        assert_eq!(region_from_destination(0x00), Region::Ntsc); // Japan
        assert_eq!(region_from_destination(0x01), Region::Ntsc); // North America
        assert_eq!(region_from_destination(0x02), Region::Pal); // Europe
        assert_eq!(region_from_destination(0x0B), Region::Pal); // Australia
        assert_eq!(region_from_destination(0x0D), Region::Ntsc); // Korea
    }
}
