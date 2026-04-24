// SPDX-License-Identifier: GPL-3.0-or-later
use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::path::Path;

use crate::crc32::crc32;
use crate::fds::bios::BiosSearch;
use crate::fds::{FdsBios, FdsData, FdsImage};
use crate::gamedb;

const INES_MAGIC: [u8; 4] = *b"NES\x1A";
const FDS_MAGIC: [u8; 4] = *b"FDS\x1A";
const INES_HEADER_SIZE: usize = 16;
const PRG_BANK_SIZE: usize = 16 * 1024;
const CHR_BANK_SIZE: usize = 8 * 1024;
const TRAINER_SIZE: usize = 512;

/// True when the file looks like a Famicom Disk System image — by
/// extension (`.fds` / `.qd`, case-insensitive) or by 4-byte fwNES
/// magic. Extension-only matches let bare `.fds` files without a
/// header still route to the FDS loader.
fn is_fds_bytes_or_ext(path: &Path, bytes: &[u8]) -> bool {
    if bytes.len() >= 4 && bytes[0..4] == FDS_MAGIC {
        return true;
    }
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("fds") || e.eq_ignore_ascii_case("qd"))
        .unwrap_or(false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mirroring {
    Horizontal,
    Vertical,
    FourScreen,
    SingleScreenLower,
    SingleScreenUpper,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TvSystem {
    Ntsc,
    Pal,
}

#[derive(Debug, Clone)]
pub struct Cartridge {
    pub prg_rom: Vec<u8>,
    pub chr_rom: Vec<u8>,
    pub chr_ram: bool,
    pub mapper_id: u16,
    pub submapper: u8,
    pub mirroring: Mirroring,
    pub battery_backed: bool,
    /// Volatile PRG work-RAM size in bytes. iNES 1.0 encodes only a
    /// single combined RAM number (in header[8], in 8 KiB units); we
    /// treat that as "battery-backed if flag6 bit 1 is set, else
    /// volatile" and populate one of `prg_ram_size`/`prg_nvram_size`
    /// accordingly. NES 2.0 encodes them as separate nibbles in
    /// header[10]: low = volatile (`64 << n` bytes), high = NVRAM.
    pub prg_ram_size: usize,
    /// Battery-backed PRG-RAM size in bytes. This is the byte count
    /// the save system persists to disk. 0 on non-battery carts. The
    /// total runtime RAM allocated by a mapper is
    /// `prg_ram_size + prg_nvram_size` (bounded below by the mapper's
    /// minimum 8 KiB window so cartridge firmware sees a valid
    /// `$6000-$7FFF` region even on no-RAM headers).
    pub prg_nvram_size: usize,
    pub tv_system: TvSystem,
    pub is_nes2: bool,
    /// CRC32 of the PRG-ROM || CHR-ROM data (matches Mesen2's
    /// `PrgChrCrc32`). Used as the key into the game database; also
    /// handy to display in debug UI.
    pub prg_chr_crc32: u32,
    /// True when any cartridge field was overridden from the game
    /// database (Mesen2's `MesenNesDB.txt`). Useful for "where did
    /// this region / mapper variant come from" debugging.
    pub db_matched: bool,
    /// Present only on mapper 20 (Famicom Disk System) carts. The
    /// mapper constructor consumes this; every other mapper ignores
    /// it. Held in `Option` rather than an enum split so the rest of
    /// the pipeline (mapper dispatch, save path, etc.) doesn't need
    /// a parallel universe for FDS.
    pub fds_data: Option<FdsData>,
}

impl Cartridge {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::load_with_fds_bios(path, None)
    }

    /// Variant of [`Cartridge::load`] that threads a `--fds-bios` CLI
    /// override through. For iNES carts the override is ignored;
    /// only the FDS load path consults it.
    pub fn load_with_fds_bios<P: AsRef<Path>>(
        path: P,
        fds_bios_override: Option<&Path>,
    ) -> Result<Self> {
        let path = path.as_ref();
        let bytes = fs::read(path)
            .with_context(|| format!("failed to read ROM file: {}", path.display()))?;

        // Dispatch to the FDS parser when the file clearly looks like
        // one — either `.fds` / `.qd` extension or the 4-byte fwNES
        // magic at offset 0. Otherwise fall through to iNES.
        if is_fds_bytes_or_ext(path, &bytes) {
            return Self::from_fds_bytes(path, &bytes, fds_bios_override);
        }
        Self::from_bytes(&bytes)
    }

    fn from_fds_bytes(
        path: &Path,
        bytes: &[u8],
        bios_override: Option<&Path>,
    ) -> Result<Self> {
        let image = FdsImage::from_bytes(bytes)
            .with_context(|| format!("parsing FDS image {}", path.display()))?;

        // Warn on any non-fatal image oddities at load time — matches
        // how Phase 0's `load_fds_info` surfaced them.
        for w in &image.warnings {
            log::warn!("FDS: {w}");
        }

        let search = BiosSearch {
            cli_override: bios_override.map(Path::to_path_buf),
            config: None, // populated by settings UI once that exists
            rom_dir: path.parent().map(Path::to_path_buf),
        };
        let bios = FdsBios::resolve(&search).with_context(|| {
            format!(
                "cannot load FDS ROM {} without the Nintendo BIOS",
                path.display()
            )
        })?;

        // CRC over the raw bytes (excluding the optional fwNES
        // header) gives a stable key into the game DB for FDS
        // titles. Not used today — FDS entries aren't in our DB yet —
        // but Phase 2's disk-swap UI may want per-game labels.
        let crc_bytes: &[u8] = if image.had_header {
            &bytes[16..]
        } else {
            bytes
        };
        let crc = crc32(crc_bytes);

        let fds_data = FdsData {
            gapped_sides: image.gapped_sides(),
            headers: image.headers(),
            bios: bios.bytes,
            bios_known_good: bios.is_known_good,
            had_header: image.had_header,
            // Pristine raw sides feed the IPS save-diff pipeline. Clone
            // out of the image before it goes out of scope — mapping to
            // gapped/headers above already consumed what we needed from
            // it via shared references.
            original_raw_sides: image.sides.clone(),
        };

        Ok(Self {
            // FDS-specific carts don't use the iNES prg_rom /
            // chr_rom arrays — BIOS + RAM serve the address space.
            // Leave these empty; the FDS mapper never consults them.
            prg_rom: Vec::new(),
            chr_rom: Vec::new(),
            chr_ram: true,
            mapper_id: 20,
            submapper: 0,
            // Runtime mirroring is written by the game via `$4025`
            // bit 3; pick horizontal as the initial value (matches
            // Mesen2 behavior before the BIOS configures the bus).
            mirroring: Mirroring::Horizontal,
            battery_backed: true, // .ips sidecar is the save file
            prg_ram_size: 32 * 1024,
            prg_nvram_size: 0,
            tv_system: TvSystem::Ntsc,
            is_nes2: false,
            prg_chr_crc32: crc,
            db_matched: false,
            fds_data: Some(fds_data),
        })
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < INES_HEADER_SIZE {
            bail!("file too short for iNES header");
        }
        let header = &bytes[..INES_HEADER_SIZE];
        if header[0..4] != INES_MAGIC {
            bail!("missing iNES magic bytes");
        }

        let prg_banks_lo = header[4] as usize;
        let chr_banks_lo = header[5] as usize;
        let flags6 = header[6];
        let flags7 = header[7];

        let is_nes2 = (flags7 & 0x0C) == 0x08;

        let mapper_lo = (flags6 >> 4) | (flags7 & 0xF0);
        let mapper_id: u16;
        let submapper: u8;
        let prg_banks: usize;
        let chr_banks: usize;
        let prg_ram_size: usize;
        let prg_nvram_size: usize;
        let tv_system: TvSystem;
        let battery_backed = (flags6 & 0x02) != 0;

        if is_nes2 {
            let mapper_hi = (header[8] & 0x0F) as u16;
            mapper_id = mapper_lo as u16 | (mapper_hi << 8);
            submapper = header[8] >> 4;
            let prg_hi = (header[9] & 0x0F) as usize;
            let chr_hi = (header[9] >> 4) as usize;
            prg_banks = prg_banks_lo | (prg_hi << 8);
            chr_banks = chr_banks_lo | (chr_hi << 8);
            // NES 2.0 header[10]:
            //   low nibble  = volatile PRG work-RAM (`64 << n` bytes)
            //   high nibble = battery-backed PRG-NVRAM (`64 << n` bytes)
            // Either can be zero; carts often use only one.
            let prg_ram_shift = (header[10] & 0x0F) as u32;
            let prg_nvram_shift = ((header[10] >> 4) & 0x0F) as u32;
            prg_ram_size = if prg_ram_shift == 0 { 0 } else { 64usize << prg_ram_shift };
            prg_nvram_size = if prg_nvram_shift == 0 { 0 } else { 64usize << prg_nvram_shift };
            tv_system = if (header[12] & 0x01) == 0 {
                TvSystem::Ntsc
            } else {
                TvSystem::Pal
            };
        } else {
            mapper_id = mapper_lo as u16;
            submapper = 0;
            prg_banks = prg_banks_lo;
            chr_banks = chr_banks_lo;
            // iNES 1.0 encodes one combined RAM count (8 KiB units). We
            // route it to NVRAM when flag6 bit 1 is set, volatile
            // otherwise. A zero-count header is rewritten to 8 KiB
            // below (every NROM/UxROM cart has SOME work-RAM slot even
            // if the header didn't bother to say so).
            let ram_banks = header[8].max(1) as usize;
            let total = ram_banks * 8 * 1024;
            if battery_backed {
                prg_ram_size = 0;
                prg_nvram_size = total;
            } else {
                prg_ram_size = total;
                prg_nvram_size = 0;
            }
            tv_system = if (header[9] & 0x01) == 0 {
                TvSystem::Ntsc
            } else {
                TvSystem::Pal
            };
        }

        let mirroring = if (flags6 & 0x08) != 0 {
            Mirroring::FourScreen
        } else if (flags6 & 0x01) != 0 {
            Mirroring::Vertical
        } else {
            Mirroring::Horizontal
        };
        let has_trainer = (flags6 & 0x04) != 0;

        let mut offset = INES_HEADER_SIZE;
        if has_trainer {
            offset = offset
                .checked_add(TRAINER_SIZE)
                .ok_or_else(|| anyhow!("trainer offset overflow"))?;
        }

        let prg_size = prg_banks
            .checked_mul(PRG_BANK_SIZE)
            .ok_or_else(|| anyhow!("prg size overflow"))?;
        let chr_size = chr_banks
            .checked_mul(CHR_BANK_SIZE)
            .ok_or_else(|| anyhow!("chr size overflow"))?;

        let prg_end = offset
            .checked_add(prg_size)
            .ok_or_else(|| anyhow!("prg end overflow"))?;
        if bytes.len() < prg_end {
            bail!(
                "ROM truncated: expected {} bytes of PRG-ROM starting at offset {}",
                prg_size,
                offset
            );
        }
        let prg_rom = bytes[offset..prg_end].to_vec();

        // On-disk CHR bytes are whatever the file carries for CHR-ROM.
        // CHR-RAM carts (header byte 5 == 0) have no on-disk CHR; the
        // 8 KB RAM buffer we synthesize below is runtime-only and must
        // NOT feed into the CRC (Mesen2 hashes file bytes after the
        // header / trainer — iNesLoader.cpp:62 — which excludes the
        // synthetic RAM). Without this distinction, CHR-RAM carts
        // produce a different CRC from Mesen's DB key and never match.
        let chr_on_disk: &[u8] = if chr_size == 0 {
            &[]
        } else {
            let chr_end = prg_end
                .checked_add(chr_size)
                .ok_or_else(|| anyhow!("chr end overflow"))?;
            if bytes.len() < chr_end {
                bail!(
                    "ROM truncated: expected {} bytes of CHR-ROM starting at offset {}",
                    chr_size,
                    prg_end
                );
            }
            &bytes[prg_end..chr_end]
        };

        // PRG+CHR CRC32 over the on-disk ROM bodies (matches Mesen2's
        // `PrgChrCrc32` — iNesLoader.cpp:62-63). Trainer bytes, if
        // present, are NOT included — they sit between the header and
        // the PRG and neither emulator hashes them. The transient
        // `concat` allocates once per load (~300 KB typical); not
        // worth inlining a streaming variant.
        let crc = {
            let mut buf = Vec::with_capacity(prg_rom.len() + chr_on_disk.len());
            buf.extend_from_slice(&prg_rom);
            buf.extend_from_slice(chr_on_disk);
            crc32(&buf)
        };

        let (chr_rom, chr_ram) = if chr_size == 0 {
            (vec![0u8; CHR_BANK_SIZE], true)
        } else {
            (chr_on_disk.to_vec(), false)
        };

        // iNES 1.0 with header[8]=0 already had its RAM bumped to 8
        // KiB by the `ram_banks.max(1)` above. NES 2.0 is trusted
        // verbatim — a NES 2.0 cart that declares 0/0 is valid and we
        // honor it (mappers still allocate their mandatory minimum
        // for the `$6000-$7FFF` window from `prg_ram_size.max(MIN)`).
        let mut cart = Self {
            prg_rom,
            chr_rom,
            chr_ram,
            mapper_id,
            submapper,
            mirroring,
            battery_backed,
            prg_ram_size,
            prg_nvram_size,
            tv_system,
            is_nes2,
            prg_chr_crc32: crc,
            db_matched: false,
            fds_data: None,
        };

        // Supplement the header from the game database. iNES 1.0 Flags 9
        // bit 0 is almost universally 0 regardless of actual region
        // (nesdev wiki: "virtually no ROM images in circulation make
        // use of it"), so for iNES 1.0 dumps the DB is the only
        // reliable source of region info. NES 2.0 byte 12 is trusted
        // and only overridden by the DB when the two disagree.
        if let Some(entry) = gamedb::lookup(crc) {
            cart.db_matched = true;
            if entry.is_pal_like() {
                cart.tv_system = TvSystem::Pal;
            } else if matches!(
                entry.system,
                gamedb::System::NesNtsc | gamedb::System::Famicom | gamedb::System::Playchoice
            ) {
                cart.tv_system = TvSystem::Ntsc;
            }
        }

        Ok(cart)
    }

    pub fn describe(&self) -> String {
        // FDS carts skip the iNES summary line entirely — they're
        // carried via `fds_data` and have very different stats
        // (BIOS + disk sides rather than PRG-ROM / CHR-ROM sizes).
        if let Some(fds) = &self.fds_data {
            return format!(
                "FDS {} side{} ({} KiB) BIOS={} header={} crc={:08X}",
                fds.gapped_sides.len(),
                if fds.gapped_sides.len() == 1 { "" } else { "s" },
                fds.gapped_sides.iter().map(|s| s.len()).sum::<usize>() / 1024,
                if fds.bios_known_good {
                    "known-good"
                } else {
                    "unrecognized"
                },
                if fds.had_header { "fwNES" } else { "bare" },
                self.prg_chr_crc32,
            );
        }
        let mut s = format!(
            "iNES{} mapper={} submapper={} prg={}KiB chr={}KiB({}) mirror={:?} battery={} prg_ram={}KiB prg_nvram={}KiB tv={:?} crc={:08X}",
            if self.is_nes2 { "2.0" } else { "1.0" },
            self.mapper_id,
            self.submapper,
            self.prg_rom.len() / 1024,
            self.chr_rom.len() / 1024,
            if self.chr_ram { "RAM" } else { "ROM" },
            self.mirroring,
            self.battery_backed,
            self.prg_ram_size / 1024,
            self.prg_nvram_size / 1024,
            self.tv_system,
            self.prg_chr_crc32,
        );
        if let Some(entry) = gamedb::lookup(self.prg_chr_crc32) {
            // Append the DB-matched chip/board info so the initial load
            // line also tells you WHY the region or mapper variant was
            // picked. Empty fields stay blank (the DB leaves most board
            // info empty for obscure homebrew / unlicensed carts).
            s.push_str(&format!(
                " db[{:?}{}{}]",
                entry.system,
                if !entry.board.is_empty() {
                    format!(" board={}", entry.board)
                } else {
                    String::new()
                },
                if !entry.chip.is_empty() {
                    format!(" chip={}", entry.chip)
                } else {
                    String::new()
                },
            ));
        }
        s
    }
}
