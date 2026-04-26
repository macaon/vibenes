// SPDX-License-Identifier: GPL-3.0-or-later
//! MMC3 / TxROM (mapper 4) + MMC6 / HKROM sub-mode.
//!
//! The MMC3 is the workhorse NES mapper - roughly 28% of the licensed
//! library. Four register pairs at $8000-$FFFF decoded by A0 + address:
//!
//! | addr mask `$E001` | write effect |
//! |---|---|
//! | `$8000` | Bank select: low 3 bits index R0..R7; bit 6 = PRG mode; bit 7 = CHR A12 inversion |
//! | `$8001` | Bank data: value -> R[bank_select & 7] (bit 0 masked for R0/R1) |
//! | `$A000` | Mirroring: bit 0 = 0 vertical, 1 horizontal (no-op if FourScreen) |
//! | `$A001` | PRG-RAM protect: bit 7 enable, bit 6 write-protect |
//! | `$C000` | IRQ latch - reload value |
//! | `$C001` | IRQ reload: counter <- 0, reload flag set |
//! | `$E000` | IRQ disable + acknowledge |
//! | `$E001` | IRQ enable |
//!
//! The A12 IRQ counter (phase 10B) is clocked on filtered A12 rising
//! edges delivered via [`Mapper::on_ppu_addr`]. A12 must be low for
//! at least [`A12_FILTER_PPU_CYCLES`] PPU cycles before the next rise
//! counts - Mesen's A12Watcher approach, chosen over puNES's per-path
//! latches because our `master_ppu_cycle` is PPU-cycle granular and
//! the test ROMs' tolerance for the single-filter model is well-
//! documented.
//!
//! **PRG layout** (bit 6 of $8000):
//! - 0: R6 at $8000-$9FFF, R7 at $A000-$BFFF, second-to-last at $C000, last at $E000
//! - 1: second-to-last at $8000, R7 at $A000, R6 at $C000-$DFFF, last at $E000
//!
//! R6 and R7 store 6-bit PRG bank indices in 8 KB units (top 2 bits
//! ignored per nesdev).
//!
//! **CHR layout** (bit 7 of $8000):
//! - 0: R0 (2K) $0000, R1 (2K) $0800, R2-R5 (1K each) $1000-$1FFF
//! - 1: R2-R5 (1K each) $0000-$0FFF, R0 (2K) $1000, R1 (2K) $1800
//!
//! R0/R1 mask bit 0 of the written value so a 2K bank is always
//! 2K-aligned (matches Mesen2 `WriteRegister` case $8001 with
//! `_currentRegister <= 1`).
//!
//! **MMC6 / HKROM** (used by StarTropics, etc.) is mapper 4 with NES 2.0
//! submapper 1. Semantic differences we model as a sub-mode of `Mmc3`:
//!
//! - **On-chip PRG-RAM**: 1 KiB (not 8 KiB), mirrored four times across
//!   `$7000-$7FFF`. `$6000-$6FFF` is not wired - reads return 0, writes
//!   are dropped.
//! - **Two 512-byte halves** with independent per-direction enables:
//!   - `$7000-$71FF` ("bank 0") gated by `$A001` bits 4/5 (write/read).
//!   - `$7200-$73FF` ("bank 1") gated by `$A001` bits 6/7 (write/read).
//! - **Global chip-enable** at `$8000` bit 5. When clear, the entire
//!   1 KiB is inaccessible regardless of `$A001`.
//! - Power-on: all enables clear - the cart must opt in before it can
//!   read or write RAM.
//!
//! Clean-room references (behavioral only, no copied code):
//! - `~/Git/Mesen2/Core/NES/Mappers/Nintendo/MMC3.h` (MMC6 decoding
//!   mirrors the `firstBankAccess` / `lastBankAccess` model there)
//! - `~/Git/Mesen2/Core/NES/Mappers/A12Watcher.h`
//! - `~/Git/puNES/src/core/mappers/MMC3.c`
//! - `~/Git/puNES/src/core/irqA12.c`
//! - `reference/mappers.md §Mapper 4`, `mesen-notes.md §20-21`, `punes-notes.md §MMC3 A12 filter`

use crate::mapper::{Mapper, PpuFetchKind};
use crate::rom::{Cartridge, Mirroring};

const PRG_BANK_8K: usize = 8 * 1024;
const CHR_BANK_1K: usize = 1024;
const PRG_RAM_SIZE: usize = 8 * 1024;
const PRG_RAM_SIZE_MMC6: usize = 1024;

/// Minimum number of PPU cycles A12 must be held low before the next
/// rising edge is counted by the IRQ counter. Mesen's `A12Watcher`
/// template defaults to 10; the Mesen2 wiki prose says "~8-12 CPU
/// cycles" (≈ 24-36 PPU cycles under the standard 3:1 ratio), but the
/// template constant is what their test-ROM scores are validated
/// against. Tune here if `mmc3_test/4-scanline_timing` drifts.
const A12_FILTER_PPU_CYCLES: u64 = 10;

pub struct Mmc3 {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,
    prg_ram: Vec<u8>,

    /// Last write to $8000. Bits: 0-2 = R index, 6 = PRG mode, 7 = CHR inversion.
    bank_select: u8,
    /// R0..R7. For R0/R1 bit 0 is masked on write.
    bank_regs: [u8; 8],

    /// Derived from cart header + $A000 writes. FourScreen overrides $A000.
    mirroring: Mirroring,
    hardwired_four_screen: bool,

    /// $A001 bit 7 - PRG-RAM chip enable. Real MMC3 returns open bus when
    /// disabled; we return 0 for simplicity and because the current bus
    /// design doesn't expose open bus to mapper reads. Default enabled so
    /// carts that never write $A001 still work.
    prg_ram_enabled: bool,
    /// $A001 bit 6 - PRG-RAM write protect.
    prg_ram_write_protected: bool,

    prg_bank_count_8k: usize,
    chr_bank_count_1k: usize,

    // --- IRQ state ---
    irq_latch: u8,
    irq_counter: u8,
    irq_reload: bool,
    irq_enabled: bool,
    irq_line: bool,
    /// PPU cycle at which A12 transitioned from high to low. `None` means
    /// A12 is currently high (or we haven't observed a fall yet at power-
    /// on). On an A12 rise, the elapsed PPU-cycle count is compared
    /// against [`A12_FILTER_PPU_CYCLES`] to decide whether this rise
    /// clocks the counter.
    a12_low_since: Option<u64>,
    /// MMC3 Rev A firing semantics. Default false (Rev B): IRQ fires
    /// whenever the counter hits zero at an A12 rise, including
    /// reload-from-zero (can double-fire). Rev A: IRQ fires only on a
    /// non-zero→zero transition. Three activation paths (see
    /// `Mmc3::new`): NES 2.0 submapper 4, game-DB chip prefix "MMC3A"
    /// (Mesen convention), or the `VIBENES_MMC3_FORCE_REV_A` env var
    /// for test ROMs that ship as iNES 1.0.
    alt_irq_behavior: bool,

    /// MMC6 / HKROM sub-mode. When set, `$6000-$7FFF` handling follows
    /// the MMC6 model (1 KiB at `$7000-$7FFF`, per-half enables) instead
    /// of the MMC3 model (8 KiB at `$6000-$7FFF`, chip-level enable).
    /// Two activation paths (see `Mmc3::new`): NES 2.0 submapper 1, or
    /// game-DB chip prefix "MMC6" (Mesen convention for HKROM carts).
    /// Independent of `alt_irq_behavior` - MMC6 carts may use either
    /// Rev A or Rev B IRQ semantics.
    is_mmc6: bool,
    /// Raw byte last written to `$A001`. On MMC6, bits 4-7 gate RAM
    /// access per 512-byte half (bits 4/5 = bank 0 write/read,
    /// bits 6/7 = bank 1 write/read). On MMC3 the `prg_ram_enabled`
    /// and `prg_ram_write_protected` flags above are the source of
    /// truth instead; this field is only read when `is_mmc6`.
    reg_a001: u8,
    /// Battery-backed PRG-RAM flag. True when cart's flag6 bit 1 was
    /// set; controls whether `save_data()` exposes RAM to the save
    /// pipeline. Commercial examples: Zelda (MMC1), Crystalis (MMC3A),
    /// Kirby's Adventure (MMC3), StarTropics (MMC6).
    battery: bool,
    /// Mutated-since-last-save flag. Any change to `prg_ram` from a
    /// CPU write sets it; `mark_saved` clears it.
    save_dirty: bool,
}

impl Mmc3 {
    pub fn new(cart: Cartridge) -> Self {
        // Rev A vs Rev B firing semantics activation. Real carts
        // need a data source richer than the iNES 1.0 header -
        // submapper bits weren't carried, so the authoritative info
        // is either the NES 2.0 header or a per-CRC game database.
        //
        // - NES 2.0 submapper 4: MMC3A per nesdev
        //   (https://www.nesdev.org/wiki/NES_2.0_submappers, mapper 4).
        // - Game DB chip prefix `MMC3A`: mirrors Mesen2's
        //   `_forceMmc3RevAIrqs = Chip.substr(0,5) == "MMC3A"`
        //   (MMC3.h:197-199). Covers Crystalis and other Rev A carts.
        // - `VIBENES_MMC3_FORCE_REV_A` env var: iNES 1.0 test ROMs
        //   (`mmc3_test/6-MMC3_alt`, `mmc3_test/6-MMC6`,
        //   `mmc3_irq_tests/5.MMC3_rev_A`) aren't in the DB and carry
        //   no submapper info, so a runtime override is the only way
        //   to validate the Rev A code path against them.
        let mut alt_irq_behavior = cart.is_nes2 && cart.submapper == 4;
        // MMC6 / HKROM detection. Submapper 1 is the NES 2.0 convention
        // for HKROM; the game DB chip prefix "MMC6" covers commercial
        // carts that pre-date NES 2.0 headers (StarTropics &c.).
        let mut is_mmc6 = cart.is_nes2 && cart.submapper == 1;
        if let Some(entry) = crate::gamedb::lookup(cart.prg_chr_crc32) {
            if entry.chip.starts_with("MMC3A") {
                alt_irq_behavior = true;
            }
            if entry.chip.starts_with("MMC6") {
                is_mmc6 = true;
            }
        }
        if std::env::var("VIBENES_MMC3_FORCE_REV_A").is_ok() {
            alt_irq_behavior = true;
        }
        let prg_bank_count_8k = (cart.prg_rom.len() / PRG_BANK_8K).max(1);

        let is_chr_ram = cart.chr_ram || cart.chr_rom.is_empty();
        let chr = if is_chr_ram {
            vec![0u8; 8 * 1024]
        } else {
            cart.chr_rom
        };
        let chr_bank_count_1k = (chr.len() / CHR_BANK_1K).max(1);

        // MMC6's 1 KiB is hardwired on-chip - the iNES `prg_ram_size`
        // (often reported as 8 KiB for HKROM carts) is misleading here.
        // MMC3 external-WRAM carts get at least 8 KiB regardless of
        // what the header claims.
        let prg_ram = if is_mmc6 {
            vec![0u8; PRG_RAM_SIZE_MMC6]
        } else {
            vec![0u8; (cart.prg_ram_size + cart.prg_nvram_size).max(PRG_RAM_SIZE)]
        };

        let hardwired_four_screen = matches!(cart.mirroring, Mirroring::FourScreen);
        let mirroring = cart.mirroring;

        Self {
            prg_rom: cart.prg_rom,
            chr,
            chr_ram: is_chr_ram,
            prg_ram,
            bank_select: 0,
            bank_regs: [0; 8],
            mirroring,
            hardwired_four_screen,
            // MMC3 powers up with RAM enabled (permissive - matches
            // most carts' expectations and blargg test behavior).
            // For MMC6 these fields are ignored; the equivalent gate
            // is `bank_select & 0x20` + `reg_a001` bits 4-7.
            prg_ram_enabled: true,
            prg_ram_write_protected: false,
            prg_bank_count_8k,
            chr_bank_count_1k,
            irq_latch: 0,
            irq_counter: 0,
            irq_reload: false,
            irq_enabled: false,
            irq_line: false,
            a12_low_since: None,
            alt_irq_behavior,
            is_mmc6,
            // $A001 = 0 on MMC6 means all four enable bits clear - the
            // cart must write non-zero before it can touch RAM.
            reg_a001: 0,
            battery: cart.battery_backed,
            save_dirty: false,
        }
    }

    /// Advance the IRQ counter one step on a filtered A12 rising edge.
    ///
    /// - If the counter is zero or the reload flag is set, load from the
    ///   `$C000` latch; otherwise decrement.
    /// - If the post-step counter is zero and IRQ is enabled, assert
    ///   `/IRQ`. Rev B fires unconditionally at zero; Rev A only on a
    ///   transition *into* zero (`prev != 0 || was_reload`).
    fn clock_irq_counter(&mut self) {
        let prev = self.irq_counter;
        let was_reload = self.irq_reload;
        if self.irq_counter == 0 || self.irq_reload {
            self.irq_counter = self.irq_latch;
        } else {
            self.irq_counter -= 1;
        }
        self.irq_reload = false;

        let should_fire = if self.alt_irq_behavior {
            (prev > 0 || was_reload) && self.irq_counter == 0
        } else {
            self.irq_counter == 0
        };
        if should_fire && self.irq_enabled {
            self.irq_line = true;
        }
    }

    fn prg_mode_1(&self) -> bool {
        (self.bank_select & 0x40) != 0
    }

    fn chr_inverted(&self) -> bool {
        (self.bank_select & 0x80) != 0
    }

    fn second_last_prg_bank(&self) -> usize {
        self.prg_bank_count_8k.saturating_sub(2)
    }

    fn last_prg_bank(&self) -> usize {
        self.prg_bank_count_8k.saturating_sub(1)
    }

    /// Resolve `$8000-$FFFF` to an 8 KB PRG bank index. Crate-public
    /// so multicart wrappers (e.g. mapper 37) can intercept the bank
    /// number and re-route through their own outer-bank logic.
    pub(crate) fn prg_bank_for(&self, addr: u16) -> usize {
        let r6 = (self.bank_regs[6] & 0x3F) as usize;
        let r7 = (self.bank_regs[7] & 0x3F) as usize;
        let second_last = self.second_last_prg_bank();
        let last = self.last_prg_bank();
        let bank = if !self.prg_mode_1() {
            match addr {
                0x8000..=0x9FFF => r6,
                0xA000..=0xBFFF => r7,
                0xC000..=0xDFFF => second_last,
                0xE000..=0xFFFF => last,
                _ => 0,
            }
        } else {
            match addr {
                0x8000..=0x9FFF => second_last,
                0xA000..=0xBFFF => r7,
                0xC000..=0xDFFF => r6,
                0xE000..=0xFFFF => last,
                _ => 0,
            }
        };
        bank % self.prg_bank_count_8k
    }

    fn map_prg(&self, addr: u16) -> usize {
        let bank = self.prg_bank_for(addr);
        let offset = (addr as usize) & (PRG_BANK_8K - 1);
        bank * PRG_BANK_8K + offset
    }

    /// Resolve `$0000-$1FFF` to a 1 KB CHR bank index. Crate-public
    /// for the same reason as [`Mmc3::prg_bank_for`].
    pub(crate) fn chr_bank_for(&self, addr: u16) -> usize {
        // R0 and R1 are 2 KB banks; their stored value already has bit 0
        // masked, so pairing `r` with `r | 1` gives the two 1 KB halves.
        let r0 = self.bank_regs[0] as usize;
        let r1 = self.bank_regs[1] as usize;
        let r2 = self.bank_regs[2] as usize;
        let r3 = self.bank_regs[3] as usize;
        let r4 = self.bank_regs[4] as usize;
        let r5 = self.bank_regs[5] as usize;
        let bank = if !self.chr_inverted() {
            match addr {
                0x0000..=0x03FF => r0,
                0x0400..=0x07FF => r0 | 0x01,
                0x0800..=0x0BFF => r1,
                0x0C00..=0x0FFF => r1 | 0x01,
                0x1000..=0x13FF => r2,
                0x1400..=0x17FF => r3,
                0x1800..=0x1BFF => r4,
                0x1C00..=0x1FFF => r5,
                _ => 0,
            }
        } else {
            match addr {
                0x0000..=0x03FF => r2,
                0x0400..=0x07FF => r3,
                0x0800..=0x0BFF => r4,
                0x0C00..=0x0FFF => r5,
                0x1000..=0x13FF => r0,
                0x1400..=0x17FF => r0 | 0x01,
                0x1800..=0x1BFF => r1,
                0x1C00..=0x1FFF => r1 | 0x01,
                _ => 0,
            }
        };
        bank % self.chr_bank_count_1k
    }

    fn map_chr(&self, addr: u16) -> usize {
        let bank = self.chr_bank_for(addr);
        let offset = (addr as usize) & (CHR_BANK_1K - 1);
        bank * CHR_BANK_1K + offset
    }

    /// True when `$6000-$7FFF` writes would land in PRG-RAM (chip
    /// enabled, write-protect clear). Crate-public so multicart
    /// wrappers can gate their outer-bank latch on the same signal -
    /// mapper 37 in particular uses `$6000-$7FFF` for the latch and
    /// per the wiki "you will need to enable writes to PRG-RAM to
    /// update it". MMC6's 4-bit-per-half gate is intentionally NOT
    /// covered here; wrappers built on MMC6 would need their own
    /// query. Returns true on plain MMC3 even if the cart never
    /// touched `$A001` (power-on default = enabled, unprotected).
    pub(crate) fn cpu_can_write_wram(&self) -> bool {
        !self.is_mmc6 && self.prg_ram_enabled && !self.prg_ram_write_protected
    }

    /// Borrow the PRG-ROM image. Crate-public for multicart wrappers
    /// that translate the [`Mmc3::prg_bank_for`] result through their
    /// own outer-bank logic and then index the underlying bytes
    /// directly.
    pub(crate) fn prg_rom(&self) -> &[u8] {
        &self.prg_rom
    }

    /// Borrow the CHR image (ROM or RAM, depending on cart). Same
    /// rationale as [`Mmc3::prg_rom`] for multicart wrappers.
    pub(crate) fn chr(&self) -> &[u8] {
        &self.chr
    }

    /// Mutable CHR borrow for the CHR-RAM write path. Mapper 37
    /// is CHR-ROM-only per spec, so this is unused there, but the
    /// wrapper keeps it available for future multicarts that ship
    /// CHR-RAM.
    pub(crate) fn chr_mut(&mut self) -> &mut [u8] {
        &mut self.chr
    }

    /// True when CHR is RAM (writable). Mirror of the constructor's
    /// `is_chr_ram` decision.
    pub(crate) fn chr_is_ram(&self) -> bool {
        self.chr_ram
    }

    /// MMC6 `$6000-$7FFF` read. `$6000-$6FFF` is not mapped - returns 0.
    /// `$7000-$7FFF` folds to the 1 KiB on-chip RAM (mirrored four
    /// times) with bit 9 of the folded offset selecting the 512-byte
    /// half (0 = bank 0 at `$7000-$71FF`, 1 = bank 1 at `$7200-$73FF`).
    /// Gated by `$8000` bit 5 (chip enable) and the matching `$A001`
    /// read-enable bit; disabled reads return 0.
    fn mmc6_read(&self, addr: u16) -> u8 {
        if !self.mmc6_chip_enabled() || addr < 0x7000 {
            return 0;
        }
        let folded = (addr as usize) & (PRG_RAM_SIZE_MMC6 - 1);
        let high_half = (folded & 0x200) != 0;
        let readable = if high_half {
            (self.reg_a001 & 0x80) != 0
        } else {
            (self.reg_a001 & 0x20) != 0
        };
        if !readable {
            return 0;
        }
        *self.prg_ram.get(folded).unwrap_or(&0)
    }

    /// MMC6 `$6000-$7FFF` write, mirror of `mmc6_read` with the matching
    /// per-half write-enable bits from `$A001` (bank 0: bit 4; bank 1:
    /// bit 6). Writes with the chip disabled or the enable clear are
    /// silently dropped.
    fn mmc6_write(&mut self, addr: u16, data: u8) {
        if !self.mmc6_chip_enabled() || addr < 0x7000 {
            return;
        }
        let folded = (addr as usize) & (PRG_RAM_SIZE_MMC6 - 1);
        let high_half = (folded & 0x200) != 0;
        let writable = if high_half {
            (self.reg_a001 & 0x40) != 0
        } else {
            (self.reg_a001 & 0x10) != 0
        };
        if !writable {
            return;
        }
        if let Some(slot) = self.prg_ram.get_mut(folded) {
            if *slot != data {
                *slot = data;
                if self.battery {
                    self.save_dirty = true;
                }
            }
        }
    }

    /// Global MMC6 chip-enable: `$8000` bit 5. When clear, the whole
    /// 1 KiB is inaccessible regardless of `$A001` - power-on state.
    fn mmc6_chip_enabled(&self) -> bool {
        (self.bank_select & 0x20) != 0
    }

    fn write_register(&mut self, addr: u16, value: u8) {
        // Decode by top 3 bits of addr + A0 (i.e. addr & 0xE001).
        match addr & 0xE001 {
            0x8000 => {
                self.bank_select = value;
            }
            0x8001 => {
                let idx = (self.bank_select & 0x07) as usize;
                let stored = if idx <= 1 {
                    // R0/R1 are 2 KB banks - low bit ignored so pairing
                    // `r | 1` in the mapper always lands on a 2 KB-aligned
                    // slot regardless of the writer's intent.
                    value & !0x01
                } else {
                    value
                };
                self.bank_regs[idx] = stored;
            }
            0xA000 => {
                if !self.hardwired_four_screen {
                    self.mirroring = if value & 0x01 != 0 {
                        Mirroring::Horizontal
                    } else {
                        Mirroring::Vertical
                    };
                }
            }
            0xA001 => {
                if self.is_mmc6 {
                    // MMC6 re-purposes $A001 bits 4-7 for per-half
                    // read/write enables. Bits 0-3 are unused; store
                    // the raw byte and decode at access time.
                    self.reg_a001 = value;
                } else {
                    self.prg_ram_enabled = (value & 0x80) != 0;
                    self.prg_ram_write_protected = (value & 0x40) != 0;
                }
            }
            0xC000 => {
                self.irq_latch = value;
            }
            0xC001 => {
                self.irq_counter = 0;
                self.irq_reload = true;
            }
            0xE000 => {
                self.irq_enabled = false;
                self.irq_line = false;
            }
            0xE001 => {
                self.irq_enabled = true;
            }
            _ => {}
        }
    }
}

impl Mapper for Mmc3 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        match addr {
            0x6000..=0x7FFF => {
                if self.is_mmc6 {
                    self.mmc6_write(addr, data);
                } else if self.prg_ram_enabled && !self.prg_ram_write_protected {
                    let i = (addr - 0x6000) as usize;
                    if let Some(slot) = self.prg_ram.get_mut(i) {
                        if *slot != data {
                            *slot = data;
                            if self.battery {
                                self.save_dirty = true;
                            }
                        }
                    }
                }
            }
            0x8000..=0xFFFF => self.write_register(addr, data),
            _ => {}
        }
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => {
                if self.is_mmc6 {
                    self.mmc6_read(addr)
                } else if self.prg_ram_enabled {
                    let i = (addr - 0x6000) as usize;
                    *self.prg_ram.get(i).unwrap_or(&0)
                } else {
                    0
                }
            }
            0x8000..=0xFFFF => {
                let i = self.map_prg(addr);
                *self.prg_rom.get(i).unwrap_or(&0)
            }
            _ => 0,
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr < 0x2000 {
            let i = self.map_chr(addr);
            *self.chr.get(i).unwrap_or(&0)
        } else {
            0
        }
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

    fn on_ppu_addr(&mut self, addr: u16, ppu_cycle: u64, _kind: PpuFetchKind) {
        let a12 = (addr & 0x1000) != 0;
        if a12 {
            // On a transition out of a low period, check the filter.
            if let Some(low_since) = self.a12_low_since {
                let elapsed = ppu_cycle.wrapping_sub(low_since);
                if elapsed >= A12_FILTER_PPU_CYCLES {
                    self.clock_irq_counter();
                }
                // Whether filtered or not, we're no longer in a low
                // window - the counter restarts only on the next fall.
                self.a12_low_since = None;
            }
        } else if self.a12_low_since.is_none() {
            self.a12_low_since = Some(ppu_cycle);
        }
    }

    fn irq_line(&self) -> bool {
        self.irq_line
    }

    fn save_data(&self) -> Option<&[u8]> {
        self.battery.then(|| self.prg_ram.as_slice())
    }

    fn load_save_data(&mut self, data: &[u8]) {
        if self.battery && data.len() == self.prg_ram.len() {
            self.prg_ram.copy_from_slice(data);
        }
    }

    fn save_dirty(&self) -> bool {
        self.save_dirty
    }

    fn mark_saved(&mut self) {
        self.save_dirty = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rom::{Cartridge, Mirroring, TvSystem};

    /// 128 KB PRG (16 × 8 KB banks) + 32 KB CHR-ROM (32 × 1 KB banks).
    /// Every PRG byte equals the bank index; every CHR byte equals the
    /// 1 KB bank index. Lets the tests assert "this address reads back
    /// bank N" without any arithmetic.
    fn tagged_cart() -> Cartridge {
        let mut prg = vec![0u8; 16 * PRG_BANK_8K];
        for b in 0..16 {
            prg[b * PRG_BANK_8K..(b + 1) * PRG_BANK_8K].fill(b as u8);
        }
        let mut chr = vec![0u8; 32 * CHR_BANK_1K];
        for b in 0..32 {
            chr[b * CHR_BANK_1K..(b + 1) * CHR_BANK_1K].fill(b as u8);
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: chr,
            chr_ram: false,
            mapper_id: 4,
            submapper: 0,
            mirroring: Mirroring::Vertical,
            battery_backed: false,
            prg_ram_size: PRG_RAM_SIZE,
            prg_nvram_size: 0,
            tv_system: TvSystem::Ntsc,
            is_nes2: false,
            prg_chr_crc32: 0,
            db_matched: false,
            fds_data: None,
        }
    }

    fn write_reg(m: &mut Mmc3, addr: u16, value: u8) {
        m.cpu_write(addr, value);
    }

    fn select_bank(m: &mut Mmc3, reg: u8, value: u8) {
        // Bank-select: leave PRG mode / CHR inversion unchanged by OR'ing
        // the reg index into the existing bank_select top bits.
        let bs = (m.bank_select & 0xC0) | (reg & 0x07);
        write_reg(m, 0x8000, bs);
        write_reg(m, 0x8001, value);
    }

    // ---- PRG mode 0 (bit 6 clear) ----

    #[test]
    fn prg_mode0_default_layout() {
        let mut m = Mmc3::new(tagged_cart());
        // Before any $8001 writes R6=R7=0. Second-to-last = bank 14,
        // last = bank 15.
        assert_eq!(m.cpu_peek(0x8000), 0); // R6
        assert_eq!(m.cpu_peek(0xA000), 0); // R7
        assert_eq!(m.cpu_peek(0xC000), 14); // second-to-last
        assert_eq!(m.cpu_peek(0xE000), 15); // last
    }

    #[test]
    fn prg_mode0_r6_r7_switch_low_windows() {
        let mut m = Mmc3::new(tagged_cart());
        select_bank(&mut m, 6, 5);
        select_bank(&mut m, 7, 9);
        assert_eq!(m.cpu_peek(0x8000), 5);
        assert_eq!(m.cpu_peek(0x9FFF), 5);
        assert_eq!(m.cpu_peek(0xA000), 9);
        assert_eq!(m.cpu_peek(0xBFFF), 9);
        // Fixed windows unchanged.
        assert_eq!(m.cpu_peek(0xC000), 14);
        assert_eq!(m.cpu_peek(0xE000), 15);
    }

    #[test]
    fn prg_mode0_r6_top_two_bits_ignored() {
        let mut m = Mmc3::new(tagged_cart());
        // 0xC0 | 3 = 0xC3; top two bits masked off -> bank 3.
        select_bank(&mut m, 6, 0xC3);
        assert_eq!(m.cpu_peek(0x8000), 3);
    }

    // ---- PRG mode 1 (bit 6 set) ----

    #[test]
    fn prg_mode1_swaps_low_fixed_with_r6() {
        let mut m = Mmc3::new(tagged_cart());
        select_bank(&mut m, 6, 5);
        select_bank(&mut m, 7, 9);
        // Flip into PRG mode 1.
        write_reg(&mut m, 0x8000, 0x40 | 6);
        assert_eq!(m.cpu_peek(0x8000), 14); // second-to-last at $8000
        assert_eq!(m.cpu_peek(0xA000), 9); // R7 still here
        assert_eq!(m.cpu_peek(0xC000), 5); // R6 moved here
        assert_eq!(m.cpu_peek(0xE000), 15); // last unchanged
    }

    // ---- CHR banking ----

    #[test]
    fn chr_mode0_default_layout() {
        let mut m = Mmc3::new(tagged_cart());
        // R0..R5 = 0 by default. In mode 0: R0(2K)=$0000, R1(2K)=$0800,
        // R2-R5(1K) at $1000-$1FFF. All zero so everything reads 0 or 1
        // (the 1 KB half of the 2 KB slot).
        assert_eq!(m.ppu_read(0x0000), 0); // R0 low half
        assert_eq!(m.ppu_read(0x0400), 1); // R0 | 1
        assert_eq!(m.ppu_read(0x0800), 0); // R1 low half
        assert_eq!(m.ppu_read(0x0C00), 1); // R1 | 1
        assert_eq!(m.ppu_read(0x1000), 0); // R2
        assert_eq!(m.ppu_read(0x1C00), 0); // R5
    }

    #[test]
    fn chr_mode0_r0_2k_bank_mask_bit0() {
        let mut m = Mmc3::new(tagged_cart());
        // Write R0 = 0x05 - bit 0 must be masked, giving 0x04. So
        // $0000-$03FF reads bank 4, $0400-$07FF reads bank 5.
        select_bank(&mut m, 0, 0x05);
        assert_eq!(m.ppu_read(0x0000), 4);
        assert_eq!(m.ppu_read(0x0400), 5);
    }

    #[test]
    fn chr_mode0_1k_banks_distinct() {
        let mut m = Mmc3::new(tagged_cart());
        // R2..R5 = 10..13
        select_bank(&mut m, 2, 10);
        select_bank(&mut m, 3, 11);
        select_bank(&mut m, 4, 12);
        select_bank(&mut m, 5, 13);
        assert_eq!(m.ppu_read(0x1000), 10);
        assert_eq!(m.ppu_read(0x1400), 11);
        assert_eq!(m.ppu_read(0x1800), 12);
        assert_eq!(m.ppu_read(0x1C00), 13);
    }

    #[test]
    fn chr_mode1_inverts_2k_and_1k_regions() {
        let mut m = Mmc3::new(tagged_cart());
        select_bank(&mut m, 0, 0x08); // R0 2K bank
        select_bank(&mut m, 1, 0x0A); // R1 2K bank
        select_bank(&mut m, 2, 20);
        select_bank(&mut m, 3, 21);
        select_bank(&mut m, 4, 22);
        select_bank(&mut m, 5, 23);
        // Flip CHR inversion.
        write_reg(&mut m, 0x8000, 0x80);
        // 1K banks now at $0000-$0FFF.
        assert_eq!(m.ppu_read(0x0000), 20);
        assert_eq!(m.ppu_read(0x0400), 21);
        assert_eq!(m.ppu_read(0x0800), 22);
        assert_eq!(m.ppu_read(0x0C00), 23);
        // 2K banks now at $1000-$1FFF.
        assert_eq!(m.ppu_read(0x1000), 8);
        assert_eq!(m.ppu_read(0x1400), 9);
        assert_eq!(m.ppu_read(0x1800), 10);
        assert_eq!(m.ppu_read(0x1C00), 11);
    }

    // ---- Mirroring ----

    #[test]
    fn a000_mirroring_toggles_h_v() {
        let mut m = Mmc3::new(tagged_cart());
        write_reg(&mut m, 0xA000, 0);
        assert_eq!(m.mirroring(), Mirroring::Vertical);
        write_reg(&mut m, 0xA001, 0); // different addr, wrong reg - shouldn't change mirror
        assert_eq!(m.mirroring(), Mirroring::Vertical);
        write_reg(&mut m, 0xA000, 1);
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
        // Odd address in the $A000 range still decodes as $A000
        // *unless* A0 is set - $A002 decodes as $A000, $A003 as $A001.
        write_reg(&mut m, 0xA002, 0);
        assert_eq!(m.mirroring(), Mirroring::Vertical);
    }

    #[test]
    fn four_screen_ignores_a000_writes() {
        let mut cart = tagged_cart();
        cart.mirroring = Mirroring::FourScreen;
        let mut m = Mmc3::new(cart);
        write_reg(&mut m, 0xA000, 1);
        assert_eq!(m.mirroring(), Mirroring::FourScreen);
        write_reg(&mut m, 0xA000, 0);
        assert_eq!(m.mirroring(), Mirroring::FourScreen);
    }

    // ---- PRG-RAM ----

    #[test]
    fn prg_ram_roundtrip() {
        let mut m = Mmc3::new(tagged_cart());
        m.cpu_write(0x6000, 0xAB);
        m.cpu_write(0x7FFF, 0xCD);
        assert_eq!(m.cpu_peek(0x6000), 0xAB);
        assert_eq!(m.cpu_peek(0x7FFF), 0xCD);
    }

    #[test]
    fn prg_ram_write_protect_blocks_writes() {
        let mut m = Mmc3::new(tagged_cart());
        m.cpu_write(0x6000, 0xAA);
        // $A001 bit 7 = enable, bit 6 = write-protect.
        write_reg(&mut m, 0xA001, 0xC0);
        m.cpu_write(0x6000, 0xFF);
        assert_eq!(m.cpu_peek(0x6000), 0xAA);
    }

    #[test]
    fn prg_ram_disable_returns_zero() {
        let mut m = Mmc3::new(tagged_cart());
        m.cpu_write(0x6000, 0x42);
        // Clear enable bit - reads return 0 regardless of stored byte.
        write_reg(&mut m, 0xA001, 0x00);
        assert_eq!(m.cpu_peek(0x6000), 0);
    }

    // ---- CHR-RAM path ----

    #[test]
    fn chr_ram_write_when_cart_has_no_chr_rom() {
        let mut cart = tagged_cart();
        cart.chr_rom = vec![];
        cart.chr_ram = true;
        let mut m = Mmc3::new(cart);
        m.ppu_write(0x0100, 0x77);
        assert_eq!(m.ppu_read(0x0100), 0x77);
    }

    #[test]
    fn chr_rom_writes_are_ignored() {
        let mut m = Mmc3::new(tagged_cart());
        let before = m.ppu_read(0x0100);
        m.ppu_write(0x0100, 0xFF);
        // CHR-ROM carts reject PPU writes (chr_ram flag is false).
        assert_eq!(m.ppu_read(0x0100), before);
        assert!(!m.chr_ram);
    }

    // ---- A12 IRQ state machine ----

    /// Clock the A12 filter by driving a low→high→... sequence with
    /// explicit PPU cycle timestamps. `rise_gap` is the PPU-cycle gap
    /// between the A12 fall and the following rise - ≥10 counts,
    /// <10 is filtered. Returns the Mmc3 in a known state.
    fn toggle_a12(m: &mut Mmc3, start_cycle: u64, rises: usize, rise_gap: u64) {
        let mut t = start_cycle;
        for _ in 0..rises {
            m.on_ppu_addr(0x0000, t, PpuFetchKind::Idle); // A12 low
            t += rise_gap;
            m.on_ppu_addr(0x1000, t, PpuFetchKind::Idle); // A12 high - filtered rise if gap >= 10
            t += 1;
        }
    }

    #[test]
    fn irq_counter_decrements_on_filtered_rise() {
        let mut m = Mmc3::new(tagged_cart());
        write_reg(&mut m, 0xC000, 3); // latch = 3
        write_reg(&mut m, 0xC001, 0); // arm reload
        write_reg(&mut m, 0xE001, 0); // enable IRQ

        // First rise: reload counter to 3.
        toggle_a12(&mut m, 100, 1, 20);
        assert!(!m.irq_line(), "first rise only reloads");
        // Three more rises: 3 -> 2 -> 1 -> 0; only the fourth rise
        // (the one that hits zero) asserts /IRQ.
        toggle_a12(&mut m, 200, 3, 20);
        assert!(m.irq_line(), "IRQ asserted when counter reaches 0");
    }

    #[test]
    fn a12_filter_rejects_short_low_windows() {
        let mut m = Mmc3::new(tagged_cart());
        write_reg(&mut m, 0xC000, 1); // latch = 1
        write_reg(&mut m, 0xC001, 0);
        write_reg(&mut m, 0xE001, 0);

        // Fall->rise with only 5 PPU cycles low: must be filtered.
        toggle_a12(&mut m, 100, 3, 5);
        assert!(!m.irq_line(), "short low windows are filtered out");
        // A proper wide rise after - this is rise #1 so it only
        // reloads the counter, not fires.
        toggle_a12(&mut m, 1000, 1, 20);
        assert!(!m.irq_line());
        // Next wide rise now clocks 1 -> 0 and fires.
        toggle_a12(&mut m, 2000, 1, 20);
        assert!(m.irq_line());
    }

    #[test]
    fn e000_clears_irq_line_and_disables() {
        let mut m = Mmc3::new(tagged_cart());
        write_reg(&mut m, 0xC000, 1);
        write_reg(&mut m, 0xC001, 0);
        write_reg(&mut m, 0xE001, 0);
        toggle_a12(&mut m, 100, 2, 20); // rise 1 reload, rise 2 -> 0 -> fire
        assert!(m.irq_line());

        // Acknowledge + disable.
        write_reg(&mut m, 0xE000, 0);
        assert!(!m.irq_line());

        // Further rises don't refire while disabled.
        toggle_a12(&mut m, 1000, 5, 20);
        assert!(!m.irq_line());
    }

    #[test]
    fn c001_forces_reload_on_next_rise() {
        let mut m = Mmc3::new(tagged_cart());
        write_reg(&mut m, 0xC000, 5);
        write_reg(&mut m, 0xE001, 0);

        // Get the counter partway down.
        write_reg(&mut m, 0xC001, 0);
        toggle_a12(&mut m, 100, 3, 20); // counter now 5 -> 5 -> 4 -> 3
        assert_eq!(m.irq_counter, 3);
        assert!(!m.irq_line());

        // Change the latch + arm reload - next A12 rise reloads to 5,
        // not continues decrement to 2.
        write_reg(&mut m, 0xC000, 2);
        write_reg(&mut m, 0xC001, 0);
        toggle_a12(&mut m, 500, 1, 20);
        assert_eq!(m.irq_counter, 2);
    }

    #[test]
    fn rev_b_refires_on_reload_from_zero() {
        // Rev B behavior: if latch=0, reload sets counter=0 and fires
        // every A12 rise.
        let mut m = Mmc3::new(tagged_cart());
        write_reg(&mut m, 0xC000, 0);
        write_reg(&mut m, 0xC001, 0);
        write_reg(&mut m, 0xE001, 0);

        toggle_a12(&mut m, 100, 1, 20);
        assert!(m.irq_line(), "Rev B fires on first reload-to-zero");
        // Acknowledge clears the line AND disables IRQ on real hardware;
        // for this test we want to see Rev B refiring, so re-enable.
        write_reg(&mut m, 0xE000, 0);
        write_reg(&mut m, 0xE001, 0);
        assert!(!m.irq_line());

        // Next A12 rise fires again on Rev B - counter was 0,
        // reload to 0, still 0, still fires.
        toggle_a12(&mut m, 1000, 1, 20);
        assert!(m.irq_line(), "Rev B refires every zero");
    }

    #[test]
    fn rev_a_does_not_refire_on_reload_from_zero() {
        let mut m = Mmc3::new(tagged_cart());
        m.alt_irq_behavior = true; // Rev A
        write_reg(&mut m, 0xC000, 0);
        write_reg(&mut m, 0xC001, 0);
        write_reg(&mut m, 0xE001, 0);

        // First rise: was_reload=true, prev=0, post=0. Rev A fires
        // because reload semantics count as a transition.
        toggle_a12(&mut m, 100, 1, 20);
        assert!(m.irq_line(), "Rev A fires on the reload transition");
        write_reg(&mut m, 0xE000, 0);

        // Second rise: counter was 0 and no reload armed, so we hit
        // the "counter==0 -> reload to latch (0)" path - prev=0,
        // was_reload=false. Rev A requires prev>0 OR was_reload.
        // Neither holds, so no fire.
        toggle_a12(&mut m, 1000, 1, 20);
        assert!(!m.irq_line(), "Rev A suppresses repeat-at-zero");
    }

    // ---- MMC6 / HKROM sub-mode ----

    /// Minimal MMC6 cart: submapper 1 on a NES 2.0 header so the
    /// activation gate trips without touching the game database.
    fn mmc6_cart() -> Cartridge {
        let mut cart = tagged_cart();
        cart.is_nes2 = true;
        cart.submapper = 1;
        // iNES often claims 8 KiB here for HKROM - MMC6 should ignore
        // the header and hardwire 1 KiB. Use a deliberately-wrong
        // large value to prove the point.
        cart.prg_ram_size = 8 * 1024;
        cart.battery_backed = true;
        cart
    }

    /// Drive MMC6 into "everything open" - chip enabled + both halves
    /// readable and writable. Lets a test focus on the bit under test
    /// (mirroring, half selection, etc.) instead of replaying the
    /// enable sequence in every case.
    fn mmc6_open_ram(m: &mut Mmc3) {
        // $8000 bit 5 = chip enable. Preserve the other bits (just
        // write the enable; R index 0 and PRG/CHR modes left as 0).
        write_reg(m, 0x8000, 0x20);
        // $A001 with all four enable bits set: 0xF0 = bits 7|6|5|4.
        write_reg(m, 0xA001, 0xF0);
    }

    #[test]
    fn mmc6_detected_via_nes2_submapper_1() {
        let m = Mmc3::new(mmc6_cart());
        assert!(m.is_mmc6, "submapper 1 should activate MMC6 mode");
        // Detection must not accidentally turn on Rev A IRQ semantics.
        assert!(
            !m.alt_irq_behavior,
            "MMC6 detection is orthogonal to Rev A IRQ"
        );
    }

    #[test]
    fn mmc6_allocates_1kib_not_header_claimed_size() {
        let m = Mmc3::new(mmc6_cart());
        assert_eq!(
            m.prg_ram.len(),
            PRG_RAM_SIZE_MMC6,
            "MMC6 must hardwire 1 KiB regardless of header prg_ram_size"
        );
    }

    #[test]
    fn mmc6_plain_mapper4_without_submapper_stays_on_mmc3_path() {
        // Non-NES2 cart with no DB match: plain MMC3, 8 KiB RAM.
        let m = Mmc3::new(tagged_cart());
        assert!(!m.is_mmc6);
        assert_eq!(m.prg_ram.len(), PRG_RAM_SIZE);
    }

    #[test]
    fn mmc6_6000_to_6fff_is_open_bus() {
        let mut m = Mmc3::new(mmc6_cart());
        mmc6_open_ram(&mut m);
        // Writes in $6000-$6FFF must be dropped even with everything
        // enabled - this region isn't wired on HKROM.
        m.cpu_write(0x6000, 0xAB);
        m.cpu_write(0x6FFF, 0xCD);
        assert_eq!(m.cpu_peek(0x6000), 0);
        assert_eq!(m.cpu_peek(0x6FFF), 0);
    }

    #[test]
    fn mmc6_7000_to_7fff_mirrors_1kib_four_times() {
        let mut m = Mmc3::new(mmc6_cart());
        mmc6_open_ram(&mut m);
        // Write at the first mirror; read-back across all four.
        m.cpu_write(0x7000, 0x11);
        m.cpu_write(0x7001, 0x22);
        assert_eq!(m.cpu_peek(0x7000), 0x11);
        assert_eq!(m.cpu_peek(0x7400), 0x11, "mirror 2");
        assert_eq!(m.cpu_peek(0x7800), 0x11, "mirror 3");
        assert_eq!(m.cpu_peek(0x7C00), 0x11, "mirror 4");
        // And confirm writes through a higher mirror land at the same
        // byte - not at a stale 8 KiB offset.
        m.cpu_write(0x7C00, 0x99);
        assert_eq!(m.cpu_peek(0x7000), 0x99);
    }

    #[test]
    fn mmc6_chip_disable_suppresses_all_access() {
        let mut m = Mmc3::new(mmc6_cart());
        mmc6_open_ram(&mut m);
        m.cpu_write(0x7000, 0x55);
        assert_eq!(m.cpu_peek(0x7000), 0x55);
        // Clear $8000 bit 5 - chip off. Reads must return 0 (chip
        // driving 0 / open-bus equivalent) and writes must be dropped.
        write_reg(&mut m, 0x8000, 0x00);
        assert_eq!(m.cpu_peek(0x7000), 0);
        m.cpu_write(0x7000, 0xFF);
        // Re-enable and confirm the original byte is still there -
        // the disabled write never landed.
        write_reg(&mut m, 0x8000, 0x20);
        write_reg(&mut m, 0xA001, 0xF0);
        assert_eq!(m.cpu_peek(0x7000), 0x55);
    }

    #[test]
    fn mmc6_per_half_read_enable_gates_independently() {
        let mut m = Mmc3::new(mmc6_cart());
        mmc6_open_ram(&mut m);
        // Prime both halves with distinct bytes.
        m.cpu_write(0x7000, 0x11); // bank 0, low half
        m.cpu_write(0x7200, 0x22); // bank 1, high half

        // Enable only bank 0 read (bit 5 set, bit 7 clear); writes
        // stay open so we can still modify if we wanted to.
        write_reg(&mut m, 0xA001, 0x20 | 0x10 | 0x40);
        assert_eq!(m.cpu_peek(0x7000), 0x11);
        assert_eq!(m.cpu_peek(0x7200), 0, "bank 1 read-disabled → 0");

        // Flip: bank 1 read, bank 0 blocked.
        write_reg(&mut m, 0xA001, 0x80 | 0x10 | 0x40);
        assert_eq!(m.cpu_peek(0x7000), 0, "bank 0 read-disabled → 0");
        assert_eq!(m.cpu_peek(0x7200), 0x22);
    }

    #[test]
    fn mmc6_per_half_write_enable_gates_independently() {
        let mut m = Mmc3::new(mmc6_cart());
        mmc6_open_ram(&mut m);
        // Start with known zeros; we only care about what lands.
        m.cpu_write(0x7000, 0x00);
        m.cpu_write(0x7200, 0x00);

        // Writes only to bank 0 (bit 4); reads stay open.
        write_reg(&mut m, 0xA001, 0x10 | 0x20 | 0x80);
        m.cpu_write(0x7000, 0xAA); // bank 0 - accepted
        m.cpu_write(0x7200, 0xBB); // bank 1 - dropped
        assert_eq!(m.cpu_peek(0x7000), 0xAA);
        assert_eq!(m.cpu_peek(0x7200), 0x00);

        // Swap to bank 1 write-enable only.
        write_reg(&mut m, 0xA001, 0x40 | 0x20 | 0x80);
        m.cpu_write(0x7000, 0xCC); // bank 0 - dropped
        m.cpu_write(0x7200, 0xDD); // bank 1 - accepted
        assert_eq!(m.cpu_peek(0x7000), 0xAA, "bank 0 write was dropped");
        assert_eq!(m.cpu_peek(0x7200), 0xDD);
    }

    #[test]
    fn mmc6_power_on_state_blocks_access() {
        // Straight out of reset: bank_select = 0 (chip disable clear)
        // and reg_a001 = 0 (all enables clear). Nothing should be
        // readable or writable until the cart opts in.
        let mut m = Mmc3::new(mmc6_cart());
        m.cpu_write(0x7000, 0xFF);
        assert_eq!(m.cpu_peek(0x7000), 0);
        // Even after enabling reads but not the chip, still blocked.
        write_reg(&mut m, 0xA001, 0xF0);
        m.cpu_write(0x7000, 0xFF);
        assert_eq!(m.cpu_peek(0x7000), 0);
    }

    #[test]
    fn mmc6_half_boundary_at_71ff_and_7200() {
        // Verify the split lands exactly between $71FF (bank 0) and
        // $7200 (bank 1) - gate bank 0 only, write both sides, confirm
        // only the bank 0 side was accepted.
        let mut m = Mmc3::new(mmc6_cart());
        mmc6_open_ram(&mut m);
        // Write at the boundary through both mirrors.
        m.cpu_write(0x71FF, 0x11);
        m.cpu_write(0x7200, 0x22);
        // Disable bank 1 write; preserve reads.
        write_reg(&mut m, 0xA001, 0x10 | 0x20 | 0x80);
        m.cpu_write(0x71FF, 0x99); // still writable (bank 0)
        m.cpu_write(0x7200, 0x99); // dropped (bank 1)
        assert_eq!(m.cpu_peek(0x71FF), 0x99);
        assert_eq!(m.cpu_peek(0x7200), 0x22);
    }

    #[test]
    fn mmc3_path_unaffected_when_is_mmc6_false() {
        // Regression guard: the existing MMC3 PRG-RAM behavior must
        // not change when MMC6 detection is off. $6000 writable,
        // 8 KiB range, bits 7/6 of $A001 gate enable and write-protect.
        let mut m = Mmc3::new(tagged_cart());
        assert!(!m.is_mmc6);
        m.cpu_write(0x6000, 0xAA);
        m.cpu_write(0x7FFF, 0xBB);
        assert_eq!(m.cpu_peek(0x6000), 0xAA);
        assert_eq!(m.cpu_peek(0x7FFF), 0xBB);
        // $A001 bit 7|6 - enable + write-protect.
        write_reg(&mut m, 0xA001, 0xC0);
        m.cpu_write(0x6000, 0xFF); // dropped
        assert_eq!(m.cpu_peek(0x6000), 0xAA);
    }

    // ---- Register-address aliasing ----

    #[test]
    fn a0_and_top_bits_select_register_bank() {
        // $8000 and $9FFE both decode as "bank select" (A0 clear, top
        // nibble = 0x8 or 0x9 - both mask to $8000). $8001 and $9FFF
        // decode as "bank data". Verify via a R6 write at a non-$8000
        // address.
        let mut m = Mmc3::new(tagged_cart());
        // Select R6 via $9FFE.
        m.cpu_write(0x9FFE, 6);
        // Write the bank value via $9FFF.
        m.cpu_write(0x9FFF, 5);
        assert_eq!(m.cpu_peek(0x8000), 5);
    }

    // ---- Battery-backed save roundtrip ----

    fn battery_cart() -> Cartridge {
        let mut c = tagged_cart();
        c.battery_backed = true;
        c
    }

    #[test]
    fn battery_mmc3_roundtrip() {
        let mut m = Mmc3::new(battery_cart());
        assert!(m.save_data().is_some());
        assert!(!m.save_dirty());

        // Populate PRG-RAM via $6000-$7FFF (MMC3 powers up unlocked).
        for i in 0..0x100 {
            m.cpu_write(0x6000 + i as u16, 0xC0 ^ i as u8);
        }
        assert!(m.save_dirty());

        let snapshot = m.save_data().unwrap().to_vec();
        m.mark_saved();
        assert!(!m.save_dirty());

        let mut fresh = Mmc3::new(battery_cart());
        fresh.load_save_data(&snapshot);
        for i in 0..0x100 {
            assert_eq!(fresh.cpu_read(0x6000 + i as u16), 0xC0 ^ i as u8);
        }
    }

    #[test]
    fn write_protected_prg_ram_does_not_dirty() {
        // Game sets $A001 bit 6 (write-protect) before RAM is
        // meaningful. Writes must be dropped AND not mark the save
        // dirty - otherwise we'd autosave a sea of zeros after every
        // reset.
        let mut m = Mmc3::new(battery_cart());
        m.cpu_write(0xA001, 0xC0); // enable + write-protect
        m.cpu_write(0x6000, 0xAB);
        assert!(!m.save_dirty());
        assert_eq!(m.cpu_read(0x6000), 0x00);
    }

    #[test]
    fn non_battery_mmc3_returns_none() {
        let m = Mmc3::new(tagged_cart());
        assert!(m.save_data().is_none());
    }
}
