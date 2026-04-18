use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::path::Path;

use crate::crc32::crc32;
use crate::gamedb;

const INES_MAGIC: [u8; 4] = *b"NES\x1A";
const INES_HEADER_SIZE: usize = 16;
const PRG_BANK_SIZE: usize = 16 * 1024;
const CHR_BANK_SIZE: usize = 8 * 1024;
const TRAINER_SIZE: usize = 512;

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
    pub prg_ram_size: usize,
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
}

impl Cartridge {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let bytes = fs::read(path)
            .with_context(|| format!("failed to read ROM file: {}", path.display()))?;
        Self::from_bytes(&bytes)
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
        let tv_system: TvSystem;

        if is_nes2 {
            let mapper_hi = (header[8] & 0x0F) as u16;
            mapper_id = mapper_lo as u16 | (mapper_hi << 8);
            submapper = header[8] >> 4;
            let prg_hi = (header[9] & 0x0F) as usize;
            let chr_hi = (header[9] >> 4) as usize;
            prg_banks = prg_banks_lo | (prg_hi << 8);
            chr_banks = chr_banks_lo | (chr_hi << 8);
            // PRG-RAM size encoded as 64 << shift if nonzero.
            let shift = (header[10] & 0x0F) as u32;
            prg_ram_size = if shift == 0 { 0 } else { 64usize << shift };
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
            let ram_banks = header[8].max(1) as usize;
            prg_ram_size = ram_banks * 8 * 1024;
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
        let battery_backed = (flags6 & 0x02) != 0;
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

        let chr_rom: Vec<u8>;
        let chr_ram;
        if chr_size == 0 {
            chr_rom = vec![0; CHR_BANK_SIZE];
            chr_ram = true;
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
            chr_rom = bytes[prg_end..chr_end].to_vec();
            chr_ram = false;
        }

        let prg_ram_size = if prg_ram_size == 0 && !is_nes2 {
            8 * 1024
        } else {
            prg_ram_size
        };

        // PRG+CHR CRC32 over the ROM bodies (matches Mesen2's
        // `PrgChrCrc32` — iNesLoader.cpp:62-63). Computed over the
        // concatenation; trainer bytes, if present, are NOT included —
        // they sit between the header and the PRG and neither emulator
        // hashes them. The transient `concat` allocates once per load
        // (~300 KB typical); not worth inlining a streaming variant.
        let crc = {
            let mut buf = Vec::with_capacity(prg_rom.len() + chr_rom.len());
            buf.extend_from_slice(&prg_rom);
            buf.extend_from_slice(&chr_rom);
            crc32(&buf)
        };

        let mut cart = Self {
            prg_rom,
            chr_rom,
            chr_ram,
            mapper_id,
            submapper,
            mirroring,
            battery_backed,
            prg_ram_size,
            tv_system,
            is_nes2,
            prg_chr_crc32: crc,
            db_matched: false,
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
        let mut s = format!(
            "iNES{} mapper={} submapper={} prg={}KiB chr={}KiB({}) mirror={:?} battery={} prg_ram={}KiB tv={:?} crc={:08X}",
            if self.is_nes2 { "2.0" } else { "1.0" },
            self.mapper_id,
            self.submapper,
            self.prg_rom.len() / 1024,
            self.chr_rom.len() / 1024,
            if self.chr_ram { "RAM" } else { "ROM" },
            self.mirroring,
            self.battery_backed,
            self.prg_ram_size / 1024,
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
