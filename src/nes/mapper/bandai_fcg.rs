// SPDX-License-Identifier: GPL-3.0-or-later
//! Bandai FCG-1 / FCG-2 / LZ93D50 (iNES mappers 16 and 159).
//!
//! Mappers 16 and 159 both cover Bandai FCG boards; 159 is a narrow
//! specialization for LZ93D50 + 24C01. Shared implementation with
//! variant + EEPROM detection:
//!
//! | variant | mapper:sub | reg range | IRQ counter | EEPROM |
//! |---|---|---|---|---|
//! | FCG-1 / FCG-2 | 16:4 | `$6000-$7FFF` | direct writes | none |
//! | LZ93D50 | 16:5 | `$8000-$FFFF` | latched via `$x00A` | 0 / 24C01 / 24C02 |
//! | LZ93D50 + 24C02 | 16:3 | `$8000-$FFFF` | latched | 256 bytes |
//! | LZ93D50 + 24C01 | 159 | `$8000-$FFFF` | latched | 128 bytes |
//! | legacy iNES 1.0 | 16:0 | both | latched (default) | 256 bytes if battery |
//!
//! **Submapper 4** (Dragon Ball 3, SD Gundam Gachapon Senshi, Dragon
//! Ball Z: Kyoushuu! Saiyajin, etc.) - FCG-1/2 ASIC. Registers only
//! decoded at `$6000-$7FFF`. Writes to `$x00B` / `$x00C` modify the
//! IRQ counter directly (no latch).
//!
//! **Submapper 5** - LZ93D50. Registers only at `$8000-$FFFF`. The
//! IRQ counter has a 16-bit reload latch: `$x00B` / `$x00C` write the
//! latch; `$x00A` copies the latch into the live counter. Games:
//! Dragon Ball Z II/III, Dragon Ball Z Gaiden, Rokudenashi Blues, SD
//! Gundam Gaiden 2-3, Crayon Shin-chan (no EEPROM). Mapper-16 EEPROM
//! carts always use 24C02 per the nesdev wiki.
//!
//! **Mapper 159** - LZ93D50 + 24C01 (128 bytes) unconditionally.
//! Same register layout, same IRQ as submapper 5. Games: Dragon Ball
//! Z: Kyoushuu! Saiya-jin, Magical Taruruuto-kun 1/2, SD Gundam
//! Gaiden: Knight Gundam Monogatari.
//!
//! **Submapper 3** - LZ93D50 with 256-byte 24C02 EEPROM, always
//! present. Datach Joint ROM System is its own mapper ID (157) but
//! the standalone LZ93D50+24C02 boards live here.
//!
//! **Submapper 0 / legacy iNES 1.0** - header couldn't disambiguate,
//! so we accept writes at BOTH ranges and default to 24C02 if the
//! cart is marked battery-backed (matches Mesen2's fallback). No
//! battery → no EEPROM.
//!
//! ## Register map (within the live range)
//!
//! The low 4 bits of the write address select the register. Writes
//! to `$6101`, `$61F1`, `$7EE1`, `$8001`, `$A0F1` all decode as
//! `$x001` (CHR bank 1).
//!
//! | reg (`addr & 0x000F`) | effect |
//! |---|---|
//! | `$x000-$x007` | CHR bank 0-7 (1 KB each, 8-bit selector) |
//! | `$x008` | PRG bank (4-bit, 16 KB at `$8000-$BFFF`) |
//! | `$x009` | Mirroring: 0=V, 1=H, 2=Single-A, 3=Single-B |
//! | `$x00A` | IRQ enable (bit 0). LZ93D50 also copies latch→counter. ACKs pending IRQ. |
//! | `$x00B` | IRQ counter low - FCG-1/2: direct. LZ93D50: latch low byte. |
//! | `$x00C` | IRQ counter high - FCG-1/2: direct. LZ93D50: latch high. |
//! | `$x00D` | EEPROM control - bit 5 = SCL, bit 6 = SDA. No-op when no EEPROM. |
//!
//! `$C000-$FFFF` is fixed to the last 16 KB PRG bank. PRG bank reg
//! masks to 4 bits so up to 16 banks (256 KB).
//!
//! ## IRQ counter quirk
//!
//! 16-bit down counter, clocked every CPU cycle while enabled. The
//! exact semantic (documented by Mesen2 as load-bearing for Famicom
//! Jump II and Magical Taruruuto-kun 2): **check for zero BEFORE
//! decrementing**. A game that loads counter=N and enables gets N+1
//! cycles before `/IRQ` fires; the counter then wraps to `$FFFF` and
//! does not fire again until re-enabled or reloaded.
//!
//! `$x00A` write acknowledges a pending IRQ (line goes low).
//!
//! ## EEPROM
//!
//! `$x00D` bit 5 drives SCL, bit 6 drives SDA. Reads at `$6000-$7FFF`
//! return the EEPROM's SDA output in bit 4 (on carts that have one).
//! Save file is the raw EEPROM contents - 128 bytes for 24C01,
//! 256 bytes for 24C02 - persisted through the mapper's normal
//! `save_data` / `load_save_data` hooks.
//!
//! ## Scope
//!
//! Out: mapper 153 (SRAM variant is its own mapper), mapper 157
//! (Datach barcode), mapper 159 (Bandai Karaoke). Large-PRG (>256 KB)
//! carts that use the CHR regs' low bit as an extra PRG bank bit -
//! no such mapper-16 cart exists in the commercial library.
//!
//! Clean-room references (behavioral only, no copied code):
//! - `~/Git/Mesen2/Core/NES/Mappers/Bandai/BandaiFcg.h`
//! - `~/Git/Mesen2/Core/NES/Mappers/Bandai/Eeprom24C0{1,2}.h`
//! - `~/Git/punes/src/core/mappers/{mapper_016.c,FCG.c,LZ93D50.c}`
//! - `~/Git/nestopia/source/core/board/NstBoardBandaiLz93d50.cpp`
//! - nesdev.org/wiki/INES_Mapper_016

use crate::nes::mapper::eeprom_24c0x::{Eeprom24C0X, EepromChip};
use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_16K: usize = 16 * 1024;
const CHR_BANK_1K: usize = 1024;

/// ASIC + EEPROM pairing detected from the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Variant {
    /// Submapper 4 - registers at `$6000-$7FFF`, IRQ counter direct.
    Fcg12,
    /// Submapper 5 - registers at `$8000-$FFFF`, latched IRQ counter.
    Lz93d50,
    /// Legacy iNES 1.0 / explicit submapper 0 - ambiguous. Accept
    /// writes at BOTH ranges. Uses LZ93D50 IRQ semantics.
    Legacy,
}

impl Variant {
    fn latched_irq(self) -> bool {
        !matches!(self, Variant::Fcg12)
    }
}

pub struct BandaiFcg {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,
    mirroring: Mirroring,

    prg_bank_count_16k: usize,
    chr_bank_count_1k: usize,

    variant: Variant,

    /// `$x008` - 4-bit 16 KB PRG bank for `$8000-$BFFF`.
    prg_bank: u8,
    /// `$x000-$x007` - 1 KB CHR bank selectors.
    chr_regs: [u8; 8],

    irq_enabled: bool,
    irq_counter: u16,
    /// LZ93D50 reload latch - unused for FCG-1/2.
    irq_reload: u16,
    irq_line: bool,

    /// Present on submapper 3, submapper 5 with declared NVRAM, and
    /// legacy battery carts. `None` means no EEPROM is wired (FCG-1/2
    /// always, LZ93D50 without battery, legacy non-battery).
    eeprom: Option<Eeprom24C0X>,
    /// Set on any EEPROM bit that moves the stored-byte state through
    /// `$x00D` writes. Cleared after a save flush. Non-EEPROM carts
    /// stay false forever.
    save_dirty: bool,
}

impl BandaiFcg {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count_16k = (cart.prg_rom.len() / PRG_BANK_16K).max(1);

        // The game database's `board` string is the authoritative
        // source for ASIC + EEPROM identification - NES 2.0 headers
        // don't always populate `prg_nvram_size` for EEPROM carts
        // (Dragon Ball Z II, for instance, reports 0 KiB despite
        // shipping with a 24C02 chip). Per CLAUDE.md, Mesen2 uses
        // Chip/board heuristics for similar disambiguation; we mirror
        // that pattern.
        let db_board = crate::gamedb::lookup(cart.prg_chr_crc32).map(|e| e.board);

        let variant = Self::pick_variant(&cart, db_board);
        // Decide EEPROM presence BEFORE we move anything out of `cart`.
        let eeprom = Self::pick_eeprom(&cart, variant, db_board);

        let is_chr_ram = cart.chr_ram || cart.chr_rom.is_empty();
        let chr = if is_chr_ram {
            vec![0u8; 8 * 1024]
        } else {
            cart.chr_rom
        };
        let chr_bank_count_1k = (chr.len() / CHR_BANK_1K).max(1);

        Self {
            prg_rom: cart.prg_rom,
            chr,
            chr_ram: is_chr_ram,
            mirroring: cart.mirroring,
            prg_bank_count_16k,
            chr_bank_count_1k,
            variant,
            prg_bank: 0,
            chr_regs: [0; 8],
            irq_enabled: false,
            irq_counter: 0,
            irq_reload: 0,
            irq_line: false,
            eeprom,
            save_dirty: false,
        }
    }

    /// Resolve ASIC variant (register range + IRQ latch behavior).
    /// Order of authority: mapper 159 → always LZ93D50 > game DB
    /// board string (explicit) > NES 2.0 submapper > fall-through to
    /// Legacy.
    fn pick_variant(cart: &Cartridge, db_board: Option<&'static str>) -> Variant {
        // Mapper 159 is a strict LZ93D50 + 24C01 variant - no FCG-1/2
        // behavior possible.
        if cart.mapper_id == 159 {
            return Variant::Lz93d50;
        }
        if let Some(board) = db_board {
            if board.contains("FCG-1") || board.contains("FCG-2") {
                return Variant::Fcg12;
            }
            if board.contains("LZ93D50") {
                return Variant::Lz93d50;
            }
        }
        if cart.is_nes2 {
            return match cart.submapper {
                3 | 5 => Variant::Lz93d50,
                4 => Variant::Fcg12,
                _ => Variant::Legacy,
            };
        }
        Variant::Legacy
    }

    /// Resolve EEPROM presence + chip. Order of authority:
    ///   1. Mapper 159 - always 24C01 (128 bytes).
    ///   2. FCG-1/2 variant - never an EEPROM.
    ///   3. Game DB `+24C0X` suffix - explicit chip (DBZ II has
    ///      `prg_nvram_size=0` in the header despite shipping with a
    ///      24C02, so the DB is more reliable than the header).
    ///   4. Bare `LZ93D50` in DB - no EEPROM, overrides any header
    ///      NVRAM size (Crayon Shin-chan is the one shipping example).
    ///   5. Submapper 3 - always 24C02.
    ///   6. Submapper 5 - chip size from `prg_nvram_size`.
    ///   7. Legacy iNES 1.0 with battery flag - default to 24C02
    ///      (Mesen2's fallback).
    fn pick_eeprom(
        cart: &Cartridge,
        variant: Variant,
        db_board: Option<&'static str>,
    ) -> Option<Eeprom24C0X> {
        // Mapper 159 - always 24C01, no exceptions.
        if cart.mapper_id == 159 {
            return Some(Eeprom24C0X::new(EepromChip::C24C01));
        }
        // FCG-1/2 never has an EEPROM wired.
        if matches!(variant, Variant::Fcg12) {
            return None;
        }
        if let Some(board) = db_board {
            if board.contains("+24C02") {
                return Some(Eeprom24C0X::new(EepromChip::C24C02));
            }
            if board.contains("+24C01") {
                return Some(Eeprom24C0X::new(EepromChip::C24C01));
            }
            // Bare `LZ93D50` in DB → no EEPROM, regardless of header.
            if board.contains("LZ93D50") {
                return None;
            }
        }
        if cart.is_nes2 && cart.submapper == 3 {
            return Some(Eeprom24C0X::new(EepromChip::C24C02));
        }
        if cart.is_nes2 && cart.submapper == 5 {
            return match cart.prg_nvram_size {
                128 => Some(Eeprom24C0X::new(EepromChip::C24C01)),
                256 => Some(Eeprom24C0X::new(EepromChip::C24C02)),
                _ => None,
            };
        }
        if matches!(variant, Variant::Legacy) && cart.battery_backed {
            return Some(Eeprom24C0X::new(EepromChip::C24C02));
        }
        None
    }

    fn in_register_range(&self, addr: u16) -> bool {
        match self.variant {
            Variant::Fcg12 => (0x6000..=0x7FFF).contains(&addr),
            Variant::Lz93d50 => (0x8000..=0xFFFF).contains(&addr),
            Variant::Legacy => (0x6000..=0xFFFF).contains(&addr),
        }
    }

    fn map_prg(&self, addr: u16) -> usize {
        let bank = match addr {
            0x8000..=0xBFFF => (self.prg_bank as usize) % self.prg_bank_count_16k,
            0xC000..=0xFFFF => self.prg_bank_count_16k.saturating_sub(1),
            _ => 0,
        };
        bank * PRG_BANK_16K + (addr as usize & (PRG_BANK_16K - 1))
    }

    fn map_chr(&self, addr: u16) -> usize {
        let reg = ((addr >> 10) & 0x07) as usize;
        let bank = (self.chr_regs[reg] as usize) % self.chr_bank_count_1k;
        bank * CHR_BANK_1K + (addr as usize & (CHR_BANK_1K - 1))
    }

    fn write_register(&mut self, addr: u16, data: u8) {
        match addr & 0x000F {
            r @ 0x0..=0x7 => {
                self.chr_regs[r as usize] = data;
            }
            0x8 => {
                self.prg_bank = data & 0x0F;
            }
            0x9 => {
                self.mirroring = match data & 0x03 {
                    0 => Mirroring::Vertical,
                    1 => Mirroring::Horizontal,
                    2 => Mirroring::SingleScreenLower,
                    _ => Mirroring::SingleScreenUpper,
                };
            }
            0xA => {
                self.irq_enabled = (data & 0x01) != 0;
                if self.variant.latched_irq() {
                    self.irq_counter = self.irq_reload;
                }
                self.irq_line = false;
            }
            0xB => {
                if self.variant.latched_irq() {
                    self.irq_reload = (self.irq_reload & 0xFF00) | data as u16;
                } else {
                    self.irq_counter = (self.irq_counter & 0xFF00) | data as u16;
                }
            }
            0xC => {
                if self.variant.latched_irq() {
                    self.irq_reload = (self.irq_reload & 0x00FF) | ((data as u16) << 8);
                } else {
                    self.irq_counter = (self.irq_counter & 0x00FF) | ((data as u16) << 8);
                }
            }
            0xD => {
                // EEPROM control: bit 5 = SCL, bit 6 = SDA. No-op on
                // carts without an EEPROM.
                if let Some(eeprom) = self.eeprom.as_mut() {
                    let scl = (data >> 5) & 1;
                    let sda = (data >> 6) & 1;
                    eeprom.write(scl, sda);
                    self.save_dirty = true;
                }
            }
            _ => {}
        }
    }
}

impl Mapper for BandaiFcg {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => {
                // Mapper-16 carts with an EEPROM expose its SDA output
                // in bit 4. Other bits on real hardware are open bus;
                // our bus interface doesn't track CPU open-bus bytes
                // so we return zeros for those bits. Every game that
                // uses the EEPROM masks with 0x10 before checking.
                match self.eeprom.as_ref() {
                    Some(e) => (e.read() & 1) << 4,
                    None => 0,
                }
            }
            0x8000..=0xFFFF => {
                let i = self.map_prg(addr);
                *self.prg_rom.get(i).unwrap_or(&0)
            }
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        if self.in_register_range(addr) {
            self.write_register(addr, data);
        }
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => match self.eeprom.as_ref() {
                Some(e) => (e.read() & 1) << 4,
                None => 0,
            },
            0x8000..=0xFFFF => {
                let i = self.map_prg(addr);
                *self.prg_rom.get(i).unwrap_or(&0)
            }
            _ => 0,
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr >= 0x2000 {
            return 0;
        }
        let i = self.map_chr(addr);
        *self.chr.get(i).unwrap_or(&0)
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if self.chr_ram && addr < 0x2000 {
            let i = self.map_chr(addr);
            if let Some(slot) = self.chr.get_mut(i) {
                *slot = data;
            }
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }

    fn on_cpu_cycle(&mut self) {
        if !self.irq_enabled {
            return;
        }
        // Check-zero-BEFORE-decrement - the exact quirk Mesen2
        // documents as load-bearing for Famicom Jump II and Magical
        // Taruruuto-kun 2. Counter = N, enabled → N+1 cycles before
        // /IRQ asserts. After firing, wraps to `0xFFFF` and does not
        // refire until reloaded or re-enabled.
        if self.irq_counter == 0 {
            self.irq_line = true;
        }
        self.irq_counter = self.irq_counter.wrapping_sub(1);
    }

    fn irq_line(&self) -> bool {
        self.irq_line
    }

    // ---- Battery-backed EEPROM persistence ----

    fn save_data(&self) -> Option<&[u8]> {
        self.eeprom.as_ref().map(|e| e.bytes())
    }

    fn load_save_data(&mut self, data: &[u8]) {
        if let Some(e) = self.eeprom.as_mut() {
            e.load(data);
        }
    }

    fn save_dirty(&self) -> bool {
        self.save_dirty && self.eeprom.is_some()
    }

    fn mark_saved(&mut self) {
        self.save_dirty = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, Mirroring, TvSystem};

    fn tagged_cart(is_nes2: bool, submapper: u8) -> Cartridge {
        let mut prg = vec![0u8; 8 * PRG_BANK_16K];
        for b in 0..8 {
            prg[b * PRG_BANK_16K..(b + 1) * PRG_BANK_16K].fill(b as u8);
        }
        let mut chr = vec![0u8; 64 * CHR_BANK_1K];
        for b in 0..64 {
            chr[b * CHR_BANK_1K..(b + 1) * CHR_BANK_1K].fill(b as u8);
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: chr,
            chr_ram: false,
            mapper_id: 16,
            submapper,
            mirroring: Mirroring::Horizontal,
            battery_backed: false,
            prg_ram_size: 0,
            prg_nvram_size: 0,
            tv_system: TvSystem::Ntsc,
            is_nes2,
            prg_chr_crc32: 0,
            db_matched: false,
            fds_data: None,
        }
    }

    fn fcg12() -> BandaiFcg {
        BandaiFcg::new(tagged_cart(true, 4))
    }
    fn lz93d50() -> BandaiFcg {
        BandaiFcg::new(tagged_cart(true, 5))
    }
    fn legacy() -> BandaiFcg {
        BandaiFcg::new(tagged_cart(false, 0))
    }
    fn lz93d50_with_24c01() -> BandaiFcg {
        let mut cart = tagged_cart(true, 5);
        cart.battery_backed = true;
        cart.prg_nvram_size = 128;
        BandaiFcg::new(cart)
    }
    fn lz93d50_with_24c02() -> BandaiFcg {
        let mut cart = tagged_cart(true, 5);
        cart.battery_backed = true;
        cart.prg_nvram_size = 256;
        BandaiFcg::new(cart)
    }
    fn sub3_with_24c02() -> BandaiFcg {
        let mut cart = tagged_cart(true, 3);
        cart.battery_backed = true;
        BandaiFcg::new(cart)
    }
    fn legacy_battery() -> BandaiFcg {
        let mut cart = tagged_cart(false, 0);
        cart.battery_backed = true;
        BandaiFcg::new(cart)
    }

    fn mapper_159() -> BandaiFcg {
        let mut cart = tagged_cart(true, 0);
        cart.mapper_id = 159;
        cart.battery_backed = true;
        cart.prg_nvram_size = 128;
        BandaiFcg::new(cart)
    }

    // ---- Variant detection ----

    #[test]
    fn variant_detected_from_submapper() {
        assert_eq!(fcg12().variant, Variant::Fcg12);
        assert_eq!(lz93d50().variant, Variant::Lz93d50);
        assert_eq!(legacy().variant, Variant::Legacy);
        assert_eq!(sub3_with_24c02().variant, Variant::Lz93d50);
        // Unknown NES 2.0 submappers fall through to Legacy.
        assert_eq!(
            BandaiFcg::new(tagged_cart(true, 0)).variant,
            Variant::Legacy
        );
    }

    // ---- EEPROM selection ----

    #[test]
    fn fcg12_never_has_eeprom() {
        let mut cart = tagged_cart(true, 4);
        cart.battery_backed = true; // even with a battery flag
        cart.prg_nvram_size = 256;
        let m = BandaiFcg::new(cart);
        assert!(m.eeprom.is_none());
    }

    #[test]
    fn submapper_3_always_has_24c02() {
        let m = sub3_with_24c02();
        assert_eq!(m.save_data().map(|b| b.len()), Some(256));
    }

    #[test]
    fn submapper_5_picks_chip_from_nvram_size() {
        assert_eq!(lz93d50_with_24c01().save_data().map(|b| b.len()), Some(128));
        assert_eq!(lz93d50_with_24c02().save_data().map(|b| b.len()), Some(256));
        // Submapper 5 without a battery / NVRAM declaration: no EEPROM.
        assert!(lz93d50().save_data().is_none());
    }

    #[test]
    fn mapper_159_always_has_24c01_and_lz93d50_variant() {
        let m = mapper_159();
        assert_eq!(m.variant, Variant::Lz93d50);
        assert_eq!(m.save_data().map(|b| b.len()), Some(128));
        // Even without a battery flag or declared NVRAM, mapper 159
        // still gets the 24C01 - the chip is hardwired to the board.
        let mut cart = tagged_cart(false, 0);
        cart.mapper_id = 159;
        let m2 = BandaiFcg::new(cart);
        assert_eq!(m2.save_data().map(|b| b.len()), Some(128));
    }

    #[test]
    fn mapper_159_uses_8000_register_range() {
        let mut m = mapper_159();
        // $6000-$7FFF is pure EEPROM read on 159 - writes go nowhere.
        m.cpu_write(0x6008, 4);
        assert_eq!(m.cpu_peek(0x8000), 0);
        m.cpu_write(0x8008, 4);
        assert_eq!(m.cpu_peek(0x8000), 4);
    }

    #[test]
    fn legacy_battery_defaults_to_24c02() {
        assert_eq!(legacy_battery().save_data().map(|b| b.len()), Some(256));
        assert!(legacy().save_data().is_none()); // no battery → none
    }

    // ---- PRG banking ----

    #[test]
    fn prg_default_layout_fixes_last_bank() {
        let m = fcg12();
        assert_eq!(m.cpu_peek(0x8000), 0);
        assert_eq!(m.cpu_peek(0xBFFF), 0);
        assert_eq!(m.cpu_peek(0xC000), 7);
        assert_eq!(m.cpu_peek(0xFFFF), 7);
    }

    #[test]
    fn prg_bank_switches_via_x008() {
        let mut m = fcg12();
        m.cpu_write(0x6008, 3);
        assert_eq!(m.cpu_peek(0x8000), 3);
        m.cpu_write(0x7FF8, 5);
        assert_eq!(m.cpu_peek(0x8000), 5);
    }

    #[test]
    fn prg_bank_masks_and_wraps() {
        let mut m = fcg12();
        m.cpu_write(0x6008, 0xF5);
        assert_eq!(m.cpu_peek(0x8000), 5);
        m.cpu_write(0x6008, 0x0F);
        assert_eq!(m.cpu_peek(0x8000), 7); // 15 % 8 = 7
    }

    // ---- CHR banking ----

    #[test]
    fn chr_regs_map_1kib_windows() {
        let mut m = fcg12();
        for i in 0..8u8 {
            m.cpu_write(0x6000 | i as u16, 10 + i);
        }
        for i in 0..8 {
            let addr = (i as u16) * CHR_BANK_1K as u16;
            assert_eq!(m.ppu_read(addr), 10 + i as u8);
            assert_eq!(m.ppu_read(addr + CHR_BANK_1K as u16 - 1), 10 + i as u8);
        }
    }

    #[test]
    fn chr_bank_selector_is_8_bits() {
        let mut m = fcg12();
        m.cpu_write(0x6000, 0x3F);
        assert_eq!(m.ppu_read(0x0000), 63);
        m.cpu_write(0x6000, 0x7F); // 0x7F % 64 = 63
        assert_eq!(m.ppu_read(0x0000), 63);
    }

    // ---- Mirroring (including single-screen) ----

    #[test]
    fn x009_mirroring_all_four_modes() {
        let mut m = fcg12();
        m.cpu_write(0x6009, 0);
        assert_eq!(m.mirroring(), Mirroring::Vertical);
        m.cpu_write(0x6009, 1);
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
        m.cpu_write(0x6009, 2);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenLower);
        m.cpu_write(0x6009, 3);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenUpper);
    }

    // ---- Register-range gating ----

    #[test]
    fn fcg12_accepts_writes_at_6000_only() {
        let mut m = fcg12();
        m.cpu_write(0x8008, 4); // dropped
        assert_eq!(m.cpu_peek(0x8000), 0);
        m.cpu_write(0x6008, 4); // accepted
        assert_eq!(m.cpu_peek(0x8000), 4);
    }

    #[test]
    fn lz93d50_accepts_writes_at_8000_only() {
        let mut m = lz93d50();
        m.cpu_write(0x6008, 4); // dropped
        assert_eq!(m.cpu_peek(0x8000), 0);
        m.cpu_write(0x8008, 4); // accepted
        assert_eq!(m.cpu_peek(0x8000), 4);
    }

    #[test]
    fn legacy_accepts_writes_at_both_ranges() {
        let mut m = legacy();
        m.cpu_write(0x6008, 2);
        assert_eq!(m.cpu_peek(0x8000), 2);
        m.cpu_write(0x8008, 5);
        assert_eq!(m.cpu_peek(0x8000), 5);
    }

    #[test]
    fn register_decodes_by_low_4_bits_of_address() {
        let mut m = fcg12();
        m.cpu_write(0x600A, 1); // enable IRQ
        m.cpu_write(0x6ECB, 0x34);
        m.cpu_write(0x7FEC, 0x12);
        assert_eq!(m.irq_counter, 0x1234);
    }

    // ---- IRQ ----

    #[test]
    fn irq_does_not_tick_when_disabled() {
        let mut m = fcg12();
        m.cpu_write(0x600B, 0x10);
        for _ in 0..100 {
            m.on_cpu_cycle();
        }
        assert_eq!(m.irq_counter, 0x0010);
        assert!(!m.irq_line());
    }

    #[test]
    fn fcg12_irq_fires_n_plus_1_cycles_after_enable() {
        let mut m = fcg12();
        m.cpu_write(0x600B, 3);
        m.cpu_write(0x600C, 0);
        m.cpu_write(0x600A, 1);
        for i in 1..=3 {
            m.on_cpu_cycle();
            assert!(!m.irq_line(), "fired early at cycle {i}");
        }
        m.on_cpu_cycle();
        assert!(m.irq_line());
        assert_eq!(m.irq_counter, 0xFFFF);
    }

    #[test]
    fn lz93d50_x00a_copies_latch_to_counter() {
        let mut m = lz93d50();
        m.cpu_write(0x800B, 0x05);
        m.cpu_write(0x800C, 0x00);
        assert_eq!(m.irq_counter, 0);
        m.cpu_write(0x800A, 1);
        assert_eq!(m.irq_counter, 5);
    }

    #[test]
    fn fcg12_x00a_does_not_copy_latch() {
        let mut m = fcg12();
        m.cpu_write(0x600B, 0x05);
        m.cpu_write(0x600C, 0x00);
        assert_eq!(m.irq_counter, 5);
        m.cpu_write(0x600A, 1);
        assert_eq!(m.irq_counter, 5);
    }

    #[test]
    fn x00a_write_acknowledges_pending_irq() {
        let mut m = fcg12();
        m.cpu_write(0x600B, 0);
        m.cpu_write(0x600A, 1);
        m.on_cpu_cycle();
        assert!(m.irq_line());
        m.cpu_write(0x600A, 1);
        assert!(!m.irq_line());
    }

    #[test]
    fn x00a_disable_acknowledges_and_stops_ticking() {
        let mut m = fcg12();
        m.cpu_write(0x600B, 0);
        m.cpu_write(0x600A, 1);
        m.on_cpu_cycle();
        assert!(m.irq_line());
        m.cpu_write(0x600A, 0);
        assert!(!m.irq_line());
        for _ in 0..10 {
            m.on_cpu_cycle();
        }
        assert!(!m.irq_line());
    }

    #[test]
    fn irq_does_not_refire_after_single_wrap() {
        let mut m = fcg12();
        m.cpu_write(0x600B, 1);
        m.cpu_write(0x600A, 1);
        m.on_cpu_cycle(); // counter 1 → 0
        m.on_cpu_cycle(); // counter 0 → fire + wrap
        assert!(m.irq_line());
        m.cpu_write(0x600A, 1); // ack
        for _ in 0..100 {
            m.on_cpu_cycle();
            assert!(!m.irq_line());
        }
    }

    // ---- EEPROM integration ----

    #[test]
    fn non_eeprom_cart_reads_return_zero_at_6000() {
        let m = fcg12();
        assert_eq!(m.cpu_peek(0x6000), 0);
        assert_eq!(m.cpu_peek(0x7FFF), 0);
    }

    #[test]
    fn eeprom_read_surfaces_sda_output_in_bit_4() {
        // LZ93D50 + 24C02 register range is $8000-$FFFF; $6000-$7FFF
        // is pure EEPROM read on this variant.
        let mut m = lz93d50_with_24c02();
        // Default SDA output is 1 - bit 4 reads as `0x10`.
        assert_eq!(m.cpu_peek(0x6000), 0x10);
        // After a STOP condition pulse (which keeps output=1, but
        // covers the no-op case).
        m.cpu_write(0x800D, 0b0110_0000); // SCL=1 SDA=1
        m.cpu_write(0x800D, 0b0100_0000); // SCL=1 SDA=0 - START
        // In Address mode with counter=0; output still 1 until first
        // clock fall.
        assert_eq!(m.cpu_peek(0x6000), 0x10);
    }

    #[test]
    fn eeprom_write_then_read_via_bus() {
        // End-to-end: bus write through $x00D with real I²C framing
        // stores a byte, then reading it back returns the same value.
        let mut m = lz93d50_with_24c02();

        // Helper - emit one I²C bit cycle. SCL drops first so SDA can
        // safely transition, then rises for the bit sample. Ends at
        // `(SCL=1, SDA=sda)`. The bit value goes in $x00D as bit 5=SCL,
        // bit 6=SDA.
        fn pulse(m: &mut BandaiFcg, sda: u8) {
            m.cpu_write(0x800D, (sda << 6)); // SCL=0 SDA=sda (falling)
            m.cpu_write(0x800D, (1 << 5) | (sda << 6)); // SCL=1 SDA=sda (rising)
        }
        fn start(m: &mut BandaiFcg) {
            m.cpu_write(0x800D, 0); // SCL=0 SDA=0
            m.cpu_write(0x800D, 1 << 6); // SCL=0 SDA=1
            m.cpu_write(0x800D, (1 << 5) | (1 << 6)); // SCL=1 SDA=1
            m.cpu_write(0x800D, 1 << 5); // SCL=1 SDA=0 - START
        }
        fn stop(m: &mut BandaiFcg) {
            m.cpu_write(0x800D, 0); // SCL=0 SDA=0
            m.cpu_write(0x800D, 1 << 5); // SCL=1 SDA=0
            m.cpu_write(0x800D, (1 << 5) | (1 << 6)); // SCL=1 SDA=1 - STOP
        }
        fn send_byte(m: &mut BandaiFcg, b: u8) {
            for i in 0..8 {
                pulse(m, (b >> (7 - i)) & 1);
            }
        }

        // Write 0x77 to address 0x40.
        start(&mut m);
        send_byte(&mut m, 0xA0); // slave, write
        pulse(&mut m, 0); // ACK
        send_byte(&mut m, 0x40); // word address
        pulse(&mut m, 0);
        send_byte(&mut m, 0x77); // data
        pulse(&mut m, 0);
        stop(&mut m);

        // Confirm internal state via save_data.
        assert_eq!(m.save_data().unwrap()[0x40], 0x77);
        assert!(m.save_dirty());
    }

    #[test]
    fn load_save_data_roundtrip() {
        let mut m = lz93d50_with_24c02();
        let mut snapshot = vec![0u8; 256];
        snapshot[0] = 0xDE;
        snapshot[255] = 0xAD;
        m.load_save_data(&snapshot);
        let read_back = m.save_data().unwrap();
        assert_eq!(read_back[0], 0xDE);
        assert_eq!(read_back[255], 0xAD);
    }

    #[test]
    fn mark_saved_clears_dirty() {
        let mut m = lz93d50_with_24c02();
        m.cpu_write(0x800D, 0b0110_0000); // any EEPROM-pin write → dirty
        assert!(m.save_dirty());
        m.mark_saved();
        assert!(!m.save_dirty());
    }

    #[test]
    fn non_eeprom_cart_never_reports_dirty() {
        let mut m = fcg12();
        // $x00D writes are still dispatched to the register path, but
        // should be a no-op for the save state on FCG-1/2.
        m.cpu_write(0x600D, 0x60);
        assert!(!m.save_dirty());
        assert!(m.save_data().is_none());
    }

    // ---- Low-noise edges ----

    #[test]
    fn cpu_writes_below_register_range_are_dropped() {
        let mut m = fcg12();
        m.cpu_write(0x4020, 0xFF);
        m.cpu_write(0x5FFF, 0xFF);
        assert_eq!(m.cpu_peek(0x8000), 0);
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
    }
}
