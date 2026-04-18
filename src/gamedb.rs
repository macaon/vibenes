//! Cartridge game database, keyed by iNES PRG+CHR CRC32.
//!
//! The iNES 1.0 header often omits or mis-encodes things we need for
//! accurate emulation: region (Flags 9 bit 0 is almost universally 0
//! regardless of actual region), submapper, mapper chip revision
//! (MMC3A vs MMC3B affects IRQ semantics), bus-conflict presence,
//! CHR-RAM vs CHR-ROM sizes, PRG-RAM / SaveRAM sizes, and sometimes
//! mirroring on 4-screen boards. NES 2.0 headers carry this info
//! reliably, but most dumps in circulation are iNES 1.0.
//!
//! The database here is a verbatim copy of Mesen2's
//! `UI/Dependencies/MesenNesDB.txt` (GPLv3, derived from Nestopia's DB,
//! NesCartDB, and NewRisingSun's NES 2.0 header database). It is
//! embedded at compile time via `include_str!` and parsed lazily into
//! a `HashMap<u32, DbEntry>` on first lookup.
//!
//! Primary use today: region detection (NesPal / Dendy → PAL, so PAL
//! ROMs run at the correct 50 Hz frame rate even with an all-zero
//! iNES 1.0 header). Future uses: MMC3 Rev A detection (Phase 10D),
//! UxROM / CNROM bus-conflict accuracy, 4-screen mirroring override,
//! a GUI info panel, and supplementing missing PRG/CHR/WRAM sizes.

use std::collections::HashMap;
use std::sync::OnceLock;

/// Raw CSV embedded at build time. Format (18 comma-separated columns):
/// `CRC, System, Board, PCB, Chip, Mapper, PrgRomSize, ChrRomSize,
/// ChrRamSize, WorkRamSize, SaveRamSize, Battery, Mirroring,
/// ControllerType, BusConflicts, SubMapper, VsSystemType, PpuModel`.
///
/// Sizes are in **KiB**; a value of 0 means "not present". Blank strings
/// mean "unspecified, fall back to header".
const DB_CSV: &str = include_str!("../data/nes_db.csv");

static DB: OnceLock<HashMap<u32, DbEntry>> = OnceLock::new();

/// High-level game system / region marker from the DB. More specific
/// than our [`crate::clock::Region`] because the DB distinguishes
/// Famicom (Japanese NTSC), Dendy (Russian NES clone, PAL-like clock
/// but NTSC-like scanline count), and VS-System / Playchoice (arcade).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum System {
    NesNtsc,
    NesPal,
    Famicom,
    Dendy,
    VsSystem,
    Playchoice,
    /// Anything we don't recognize (Famiclones, VT-variants, etc.).
    /// Treated as NTSC for region purposes.
    Other,
}

/// Bus-conflict behavior override for carts whose mapper can't detect
/// it from the ROM alone (UxROM, CNROM, AxROM variants differ).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusConflicts {
    /// DB didn't flag it — use the mapper's default.
    Default,
    Yes,
    No,
}

/// Single row from the database. String fields (`board`, `pcb`, `chip`,
/// `mirroring`) are `&'static str` slices into [`DB_CSV`] — zero-copy.
#[derive(Debug, Clone, Copy)]
pub struct DbEntry {
    pub system: System,
    pub board: &'static str,
    pub pcb: &'static str,
    /// Mapper chip identifier — for MMC3 this distinguishes Rev A vs
    /// Rev B/C, which changes IRQ firing semantics. Mesen's `InitMapper`
    /// activates Rev A behavior on `chip.starts_with("MMC3A")`.
    pub chip: &'static str,
    pub mapper_id: u16,
    /// PRG-ROM size in bytes.
    pub prg_rom_size: u32,
    /// CHR-ROM size in bytes (0 when the cart uses CHR-RAM).
    pub chr_rom_size: u32,
    /// CHR-RAM size in bytes (0 when the cart uses CHR-ROM). NES 2.0
    /// only reliably encodes this in byte 11; the DB fills the gap for
    /// iNES 1.0 dumps.
    pub chr_ram_size: u32,
    /// Work-RAM size in bytes (non-battery-backed).
    pub work_ram_size: u32,
    /// Save-RAM size in bytes (battery-backed when `has_battery` set).
    pub save_ram_size: u32,
    pub has_battery: bool,
    /// Raw mirroring code: `"h"` horizontal, `"v"` vertical, `"4"` four-
    /// screen, `"1"` single-screen. Empty when unknown.
    pub mirroring: &'static str,
    /// Controller / input-device type (zapper, paddle, power pad, etc.).
    /// 1 = standard controller. Values are Mesen's `GameInputType` enum.
    pub input_type: u8,
    pub bus_conflicts: BusConflicts,
    /// NES 2.0 submapper ID (0–15). `None` if unspecified.
    pub submapper_id: Option<u8>,
    /// VS System PPU board type (0 = not VS System).
    pub vs_type: u8,
    /// VS System PPU palette model.
    pub vs_ppu_model: u8,
}

impl DbEntry {
    /// True if this entry's system uses PAL frame timing (50 Hz).
    /// Dendy is technically its own timing model but groups closer to
    /// PAL than NTSC for frame-pacing purposes.
    pub fn is_pal_like(&self) -> bool {
        matches!(self.system, System::NesPal | System::Dendy)
    }
}

/// Look up a cart by its PRG+CHR CRC32 (Mesen's `PrgChrCrc32`). Returns
/// `None` for carts the DB doesn't know (homebrew, pirated ROMs, new
/// releases, hacks).
pub fn lookup(crc: u32) -> Option<&'static DbEntry> {
    DB.get_or_init(load).get(&crc)
}

fn load() -> HashMap<u32, DbEntry> {
    let mut m = HashMap::with_capacity(11000);
    for line in DB_CSV.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(entry) = parse_row(line) else {
            continue;
        };
        m.insert(entry.0, entry.1);
    }
    m
}

fn parse_row(line: &'static str) -> Option<(u32, DbEntry)> {
    let parts: Vec<&'static str> = line.split(',').collect();
    if parts.len() < 18 {
        return None;
    }
    let crc = u32::from_str_radix(parts[0], 16).ok()?;
    let entry = DbEntry {
        system: parse_system(parts[1]),
        board: parts[2],
        pcb: parts[3],
        chip: parts[4],
        mapper_id: parts[5].parse().unwrap_or(0),
        prg_rom_size: kib(parts[6]),
        chr_rom_size: kib(parts[7]),
        chr_ram_size: kib(parts[8]),
        work_ram_size: kib(parts[9]),
        save_ram_size: kib(parts[10]),
        has_battery: parts[11] == "1",
        mirroring: parts[12],
        input_type: parts[13].parse().unwrap_or(0),
        bus_conflicts: parse_bus_conflicts(parts[14]),
        submapper_id: parts[15].parse().ok(),
        vs_type: parts[16].parse().unwrap_or(0),
        vs_ppu_model: parts[17].parse().unwrap_or(0),
    };
    Some((crc, entry))
}

fn kib(s: &str) -> u32 {
    s.parse::<u32>().unwrap_or(0).saturating_mul(1024)
}

fn parse_system(s: &str) -> System {
    match s {
        "NesNtsc" => System::NesNtsc,
        "NesPal" => System::NesPal,
        "Famicom" => System::Famicom,
        "Dendy" => System::Dendy,
        "VsSystem" => System::VsSystem,
        "Playchoice" => System::Playchoice,
        _ => System::Other,
    }
}

fn parse_bus_conflicts(s: &str) -> BusConflicts {
    match s {
        "Y" => BusConflicts::Yes,
        "N" => BusConflicts::No,
        _ => BusConflicts::Default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_parses_without_panic_and_has_entries() {
        // Force the lazy-init. Any parse bug would surface here.
        let db = DB.get_or_init(load);
        assert!(
            db.len() > 5000,
            "DB seems short ({} entries) — did the CSV get truncated?",
            db.len()
        );
    }

    #[test]
    fn known_pal_crc_maps_to_pal() {
        // `001388B3,NesPal,NES-TLROM,NES-TLROM-03,MMC3C,...` is the first
        // NesPal entry in the source DB (Mesen2 commit 2022-07-21). If
        // this ever stops matching, the CSV has been regenerated with a
        // different base set — verify then update the fixture.
        let entry = lookup(0x001388B3).expect("fixture not in DB");
        assert_eq!(entry.system, System::NesPal);
        assert_eq!(entry.mapper_id, 4);
        assert_eq!(entry.chip, "MMC3C");
        assert!(entry.is_pal_like());
    }

    #[test]
    fn known_ntsc_crc_maps_to_ntsc() {
        // `00098369,NesNtsc,,,,562,128,128,0,8,0,0,v,1,,1,,` — first
        // NTSC entry in the DB.
        let entry = lookup(0x00098369).expect("fixture not in DB");
        assert_eq!(entry.system, System::NesNtsc);
        assert!(!entry.is_pal_like());
    }

    #[test]
    fn unknown_crc_returns_none() {
        // CRC chosen to be unlikely to collide. If this ever fires, pick
        // another magic number.
        assert!(lookup(0xDEADBEEF).is_none());
    }
}
