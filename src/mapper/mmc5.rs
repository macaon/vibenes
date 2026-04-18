//! MMC5 / ExROM (mapper 5) — sub-phase A: PRG banking + PRG-RAM only.
//!
//! MMC5 is the most complex official NES mapper. This sub-phase ships
//! the CPU-visible slice of it — the PRG window selectors, PRG-RAM
//! with the two-register write-protect, and a stub for everything
//! else. CHR banking lands in sub-B, scanline IRQ in sub-C, multiply
//! and PRG-RAM protect-refinement in sub-D, ExRAM in sub-E, and
//! split-screen / ExAttr / audio in a later phase.
//!
//! ## CPU-visible registers ($5000-$5FFF)
//!
//! | Addr | Effect |
//! |---|---|
//! | `$5100` | PRG mode — bits 1-0. 0=32K, 1=16K+16K, 2=16K+8K+8K, 3=four 8K. |
//! | `$5101` | CHR mode (sub-B). |
//! | `$5102` | PRG-RAM write-protect 1 (low 2 bits). Writes enabled only when `$5102 & 3 == 2` AND `$5103 & 3 == 1`. |
//! | `$5103` | PRG-RAM write-protect 2 (low 2 bits). |
//! | `$5104` | ExRAM mode (sub-E). |
//! | `$5105` | Nametable slot mapping (sub-C). |
//! | `$5106/$5107` | Fill-mode tile + attribute (sub-C). |
//! | `$5113` | PRG-RAM bank at `$6000-$7FFF` (always RAM). |
//! | `$5114-$5117` | PRG bank registers. Bit 7 = ROM (1) / RAM (0), except `$5117` which is always ROM. |
//! | `$5120-$5130` | CHR banking (sub-B). |
//! | `$5200-$5203` | Split-screen / scanline-IRQ target (sub-C/F). |
//! | `$5204` | IRQ status / enable (sub-C). |
//! | `$5205/$5206` | Hardware multiplier (sub-D). |
//! | `$5C00-$5FFF` | ExRAM window (sub-E). |
//!
//! ## PRG window layout per `$5100`
//!
//! Writes to `$5114-$5117` always store the raw value; layout is
//! computed on demand by `resolve_prg`. `$5117` is fixed ROM; the
//! other three registers' bit 7 picks ROM (1) or RAM (0). Mode-
//! alignment masks off the low bits that don't matter at each
//! window size:
//!
//! | Mode | `$8000-$9FFF` | `$A000-$BFFF` | `$C000-$DFFF` | `$E000-$FFFF` |
//! |---|---|---|---|---|
//! | 0 | `$5117 & 0x7C`+0 | `+1` | `+2` | `+3` (32 KB ROM window) |
//! | 1 | `$5115 & 0x7E`+0 | `+1` (16 KB) | `$5117 & 0x7E`+0 | `+1` (16 KB ROM) |
//! | 2 | `$5115 & 0x7E`+0 | `+1` (16 KB) | `$5116 & 0x7F` (8 KB) | `$5117 & 0x7F` (8 KB ROM) |
//! | 3 | `$5114 & 0x7F` | `$5115 & 0x7F` | `$5116 & 0x7F` | `$5117 & 0x7F` (8 KB each) |
//!
//! ## Power-on defaults
//!
//! Per nesdev wiki + Mesen2 MMC5: `$5100 = 0x03` (8 KB mode), `$5117 =
//! 0xFF` (last ROM bank pinned). Other registers are zeroed — which
//! means `$5102/$5103` default to protect-engaged, so a fresh cart
//! can't accidentally corrupt battery RAM before the game unlocks it.
//!
//! Clean-room references (behavioral only, no copied code):
//! - `~/Git/Mesen2/Core/NES/Mappers/Nintendo/MMC5.h` — `GetCpuBankInfo`
//!   and `UpdatePrgBanks` are the canonical model for the mode table.
//! - `~/Git/punes/src/core/mappers/mapper_MMC5.c`
//! - `reference/mappers.md §Mapper 5`

use crate::mapper::Mapper;
use crate::rom::{Cartridge, Mirroring};

const PRG_BANK_8K: usize = 8 * 1024;
/// Minimum PRG-RAM we allocate even if the header says 0. Many MMC5
/// carts under-declare PRG-RAM in their iNES v1 header; allocating a
/// single 8 KB chip keeps them from faulting on the first `$6000`
/// write. Games that genuinely have 32 KB+ (Uncharted Waters, Just
/// Breed) rely on the header being correct.
const MIN_PRG_RAM: usize = 8 * 1024;

/// One of the four CPU PRG slots ($8000, $A000, $C000, $E000). Each
/// is an 8 KB window and resolves to either a ROM bank or a RAM bank.
#[derive(Debug, Clone, Copy)]
struct PrgSlot {
    kind: PrgKind,
    /// Bank index in 8 KB units. Masked against the backing store's
    /// bank count at read time so over-large values wrap harmlessly.
    bank_8k: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrgKind {
    Rom,
    Ram,
}

pub struct Mmc5 {
    prg_rom: Vec<u8>,
    prg_ram: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,
    mirroring: Mirroring,

    /// $5100 low 2 bits — PRG window layout selector.
    prg_mode: u8,
    /// Raw values written to $5113..=$5117. Indexed by (addr - $5113).
    prg_regs: [u8; 5],
    /// $5102 & 3 — half of the PRG-RAM write-protect pair.
    prg_ram_protect1: u8,
    /// $5103 & 3 — other half.
    prg_ram_protect2: u8,

    /// Resolved window table, recomputed after every bank-selector
    /// write. Indexed by (addr >> 13) & 3 (i.e. which 8 KB of
    /// $8000-$FFFF).
    prg_slots: [PrgSlot; 4],
    /// Resolved $6000-$7FFF slot (always RAM per nesdev). Recomputed
    /// after writes to $5113.
    prg_ram_slot: PrgSlot,

    prg_bank_count_8k: usize,
    prg_ram_bank_count_8k: usize,
}

impl Mmc5 {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count_8k = (cart.prg_rom.len() / PRG_BANK_8K).max(1);
        let prg_ram_size = cart.prg_ram_size.max(MIN_PRG_RAM);
        let prg_ram = vec![0u8; prg_ram_size];
        let prg_ram_bank_count_8k = (prg_ram.len() / PRG_BANK_8K).max(1);

        // CHR is a stub in sub-A: one 8 KB window taken from the cart's
        // supplied CHR-ROM, or a fresh 8 KB of CHR-RAM. Sub-B installs
        // the full banking model.
        let chr = if cart.chr_ram {
            vec![0u8; 8 * 1024]
        } else {
            cart.chr_rom
        };

        let mut m = Self {
            prg_rom: cart.prg_rom,
            prg_ram,
            chr,
            chr_ram: cart.chr_ram,
            mirroring: cart.mirroring,
            // Power-on: 8 KB mode (per nesdev wiki / Mesen2).
            prg_mode: 3,
            prg_regs: [0, 0, 0, 0, 0xFF],
            // Power-on: protect engaged. Cart must write the unlock
            // pattern (5102 & 3 == 2, 5103 & 3 == 1) before writes to
            // PRG-RAM stick.
            prg_ram_protect1: 0,
            prg_ram_protect2: 0,
            prg_slots: [PrgSlot {
                kind: PrgKind::Rom,
                bank_8k: 0,
            }; 4],
            prg_ram_slot: PrgSlot {
                kind: PrgKind::Ram,
                bank_8k: 0,
            },
            prg_bank_count_8k,
            prg_ram_bank_count_8k,
        };
        m.update_prg_banks();
        m
    }

    /// True when both halves of the write-protect pair have been
    /// driven to the unlock values. Required by all PRG-RAM writes.
    fn prg_ram_writable(&self) -> bool {
        self.prg_ram_protect1 == 0b10 && self.prg_ram_protect2 == 0b01
    }

    /// Re-resolve the four 8 KB PRG slots from `prg_mode` and the
    /// raw bank registers. Called after any write that could change
    /// the window layout.
    fn update_prg_banks(&mut self) {
        // $5117 is always ROM (last slot must be physical ROM — the
        // reset vector lives there). bit 7 is therefore part of the
        // bank index, masked off at each mode's alignment.
        let r5115 = self.prg_regs[2];
        let r5116 = self.prg_regs[3];
        let r5117 = self.prg_regs[4];

        match self.prg_mode {
            0 => {
                // One 32 KB ROM window over $8000-$FFFF from $5117.
                // Low 2 bits of the bank index are ignored (32 KB
                // alignment).
                let base = (r5117 & 0x7C) as usize;
                for i in 0..4 {
                    self.prg_slots[i] = PrgSlot {
                        kind: PrgKind::Rom,
                        bank_8k: base.wrapping_add(i),
                    };
                }
            }
            1 => {
                // 16 KB from $5115 at $8000-$BFFF (ROM or RAM per
                // bit 7); 16 KB ROM from $5117 at $C000-$FFFF. Low
                // bit of the bank index ignored (16 KB alignment).
                let (kind_low, base_low) = Self::decode_rom_ram(r5115, 0x7E);
                self.prg_slots[0] = PrgSlot {
                    kind: kind_low,
                    bank_8k: base_low,
                };
                self.prg_slots[1] = PrgSlot {
                    kind: kind_low,
                    bank_8k: base_low.wrapping_add(1),
                };
                let base_high = (r5117 & 0x7E) as usize;
                self.prg_slots[2] = PrgSlot {
                    kind: PrgKind::Rom,
                    bank_8k: base_high,
                };
                self.prg_slots[3] = PrgSlot {
                    kind: PrgKind::Rom,
                    bank_8k: base_high.wrapping_add(1),
                };
            }
            2 => {
                // 16 KB from $5115 at $8000-$BFFF (ROM or RAM);
                // 8 KB from $5116 at $C000-$DFFF (ROM or RAM);
                // 8 KB ROM from $5117 at $E000-$FFFF.
                let (kind_low, base_low) = Self::decode_rom_ram(r5115, 0x7E);
                self.prg_slots[0] = PrgSlot {
                    kind: kind_low,
                    bank_8k: base_low,
                };
                self.prg_slots[1] = PrgSlot {
                    kind: kind_low,
                    bank_8k: base_low.wrapping_add(1),
                };
                let (kind_mid, bank_mid) = Self::decode_rom_ram(r5116, 0x7F);
                self.prg_slots[2] = PrgSlot {
                    kind: kind_mid,
                    bank_8k: bank_mid,
                };
                self.prg_slots[3] = PrgSlot {
                    kind: PrgKind::Rom,
                    bank_8k: (r5117 & 0x7F) as usize,
                };
            }
            _ => {
                // Mode 3: four 8 KB windows. $5114-$5116 bit 7 picks
                // ROM/RAM; $5117 always ROM.
                let r5114 = self.prg_regs[1];
                let (kind0, bank0) = Self::decode_rom_ram(r5114, 0x7F);
                let (kind1, bank1) = Self::decode_rom_ram(r5115, 0x7F);
                let (kind2, bank2) = Self::decode_rom_ram(r5116, 0x7F);
                self.prg_slots[0] = PrgSlot {
                    kind: kind0,
                    bank_8k: bank0,
                };
                self.prg_slots[1] = PrgSlot {
                    kind: kind1,
                    bank_8k: bank1,
                };
                self.prg_slots[2] = PrgSlot {
                    kind: kind2,
                    bank_8k: bank2,
                };
                self.prg_slots[3] = PrgSlot {
                    kind: PrgKind::Rom,
                    bank_8k: (r5117 & 0x7F) as usize,
                };
            }
        }

        // PRG-RAM window at $6000-$7FFF — $5113 low 3 bits select
        // an 8 KB bank. Larger WRAM configurations can use bit 3
        // too, but sub-A's max is 64 KB via the header path below.
        let r5113 = self.prg_regs[0];
        let ram_mask = (self.prg_ram_bank_count_8k - 1).min(0x07);
        self.prg_ram_slot = PrgSlot {
            kind: PrgKind::Ram,
            bank_8k: (r5113 as usize) & ram_mask,
        };
    }

    /// Decode a `$5114-$5116` value into `(kind, bank_8k)`. Bit 7 = 1
    /// means ROM; bit 7 = 0 means RAM. `align_mask` strips the low
    /// bits that don't matter at the current window size.
    fn decode_rom_ram(value: u8, align_mask: u8) -> (PrgKind, usize) {
        let kind = if value & 0x80 != 0 {
            PrgKind::Rom
        } else {
            PrgKind::Ram
        };
        let bank = (value & 0x7F & align_mask) as usize;
        (kind, bank)
    }

    /// Resolve a CPU read/write address in the `$8000-$FFFF` range to
    /// a backing-store offset + kind. Wraps the bank index against
    /// the actual backing store so over-large register values map
    /// harmlessly.
    fn resolve_upper(&self, addr: u16) -> (PrgKind, usize) {
        let slot = &self.prg_slots[((addr >> 13) & 0x03) as usize];
        let offset_in_bank = (addr & 0x1FFF) as usize;
        match slot.kind {
            PrgKind::Rom => {
                let bank = slot.bank_8k % self.prg_bank_count_8k;
                (PrgKind::Rom, bank * PRG_BANK_8K + offset_in_bank)
            }
            PrgKind::Ram => {
                let bank = slot.bank_8k % self.prg_ram_bank_count_8k;
                (PrgKind::Ram, bank * PRG_BANK_8K + offset_in_bank)
            }
        }
    }

    fn read_cpu(&self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => {
                let offset_in_bank = (addr & 0x1FFF) as usize;
                let bank = self.prg_ram_slot.bank_8k % self.prg_ram_bank_count_8k;
                self.prg_ram[bank * PRG_BANK_8K + offset_in_bank]
            }
            0x8000..=0xFFFF => {
                let (kind, offset) = self.resolve_upper(addr);
                match kind {
                    PrgKind::Rom => self.prg_rom[offset],
                    PrgKind::Ram => self.prg_ram[offset],
                }
            }
            _ => 0,
        }
    }
}

impl Mapper for Mmc5 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.read_cpu(addr)
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        self.read_cpu(addr)
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        match addr {
            // $5100: PRG mode select — forces a window re-resolve.
            0x5100 => {
                self.prg_mode = data & 0x03;
                self.update_prg_banks();
            }
            // $5102 / $5103: two-register PRG-RAM write-protect. Both
            // must reach the unlock values ($5102 & 3 == 2, $5103 & 3
            // == 1) before writes to PRG-RAM actually land.
            0x5102 => self.prg_ram_protect1 = data & 0x03,
            0x5103 => self.prg_ram_protect2 = data & 0x03,
            // $5113: PRG-RAM bank at $6000-$7FFF.
            // $5114-$5117: upper PRG bank registers.
            0x5113..=0x5117 => {
                self.prg_regs[(addr - 0x5113) as usize] = data;
                self.update_prg_banks();
            }
            // $5101, $5104-$5107, $5120-$5130, $5200-$5206: stubbed
            // for later sub-phases. Swallow the write so games that
            // touch them during init don't panic. (CHR / NT / IRQ /
            // multiply land in sub-B/C/D.)
            0x5000..=0x5FFF => {}
            // PRG-RAM window.
            0x6000..=0x7FFF => {
                if self.prg_ram_writable() {
                    let offset_in_bank = (addr & 0x1FFF) as usize;
                    let bank = self.prg_ram_slot.bank_8k % self.prg_ram_bank_count_8k;
                    self.prg_ram[bank * PRG_BANK_8K + offset_in_bank] = data;
                }
            }
            // $8000-$FFFF: writes only stick if the slot is RAM-backed
            // and protect is disengaged. ROM-backed slots silently
            // swallow the write (matches real hardware).
            0x8000..=0xFFFF => {
                let (kind, offset) = self.resolve_upper(addr);
                if kind == PrgKind::Ram && self.prg_ram_writable() {
                    self.prg_ram[offset] = data;
                }
            }
            _ => {}
        }
    }

    fn cpu_read_ex(&mut self, addr: u16) -> Option<u8> {
        // Sub-A: no registers return meaningful data yet. Sub-C wires
        // $5204 (IRQ status), sub-D wires $5205/$5206 (multiply
        // product), sub-E wires $5C00-$5FFF (ExRAM). Returning None
        // here leaves the bus to fall through to open bus — indistinct
        // from a register that simply isn't present, which is what the
        // spec calls for.
        let _ = addr;
        None
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        // Sub-A stub: flat 8 KB CHR window. Sub-B installs banking.
        match addr {
            0x0000..=0x1FFF => self.chr[(addr as usize) % self.chr.len().max(1)],
            _ => 0,
        }
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if self.chr_ram {
            if let 0x0000..=0x1FFF = addr {
                let idx = (addr as usize) % self.chr.len().max(1);
                self.chr[idx] = data;
            }
        }
    }

    fn mirroring(&self) -> Mirroring {
        // Sub-C will override via $5105 NT slot mapping.
        self.mirroring
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rom::TvSystem;

    /// 128 KB PRG-ROM (16 × 8 KB banks) with each bank filled with
    /// its 8 KB bank index, plus 32 KB PRG-RAM (4 × 8 KB banks)
    /// whose backing store starts zeroed. CHR is a single 8 KB
    /// zeroed buffer — sub-A doesn't bank CHR.
    fn tagged_cart() -> Cartridge {
        let mut prg = vec![0u8; 16 * PRG_BANK_8K];
        for bank in 0..16 {
            prg[bank * PRG_BANK_8K..(bank + 1) * PRG_BANK_8K].fill(bank as u8);
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: vec![0u8; 8 * 1024],
            chr_ram: false,
            mapper_id: 5,
            submapper: 0,
            mirroring: Mirroring::Vertical,
            battery_backed: false,
            prg_ram_size: 32 * 1024,
            tv_system: TvSystem::Ntsc,
            is_nes2: false,
            prg_chr_crc32: 0,
            db_matched: false,
        }
    }

    /// Both halves of the PRG-RAM write-protect pair — after this,
    /// subsequent `$6000-$7FFF` writes stick.
    fn unlock_prg_ram(m: &mut Mmc5) {
        m.cpu_write(0x5102, 0b10);
        m.cpu_write(0x5103, 0b01);
    }

    #[test]
    fn power_on_defaults_pin_last_bank_at_e000() {
        // $5117 = 0xFF, mode 3: slot 3 reads from ROM bank 0x7F,
        // wrapped against 16 banks -> bank 15 (the highest).
        let m = Mmc5::new(tagged_cart());
        assert_eq!(m.cpu_peek(0xE000), 15);
        assert_eq!(m.cpu_peek(0xFFFF), 15);
    }

    #[test]
    fn prg_mode_0_selects_32k_rom_window_via_5117() {
        let mut m = Mmc5::new(tagged_cart());
        m.cpu_write(0x5100, 0x00); // mode 0
        // Pick bank group 8 (8 × 8 KB = 64 KB offset). $5117 & 0x7C
        // = 8 means 32 KB window starting at 8 KB-bank 8. Writing
        // `0x09` is stripped to 0x08.
        m.cpu_write(0x5117, 0x09);
        assert_eq!(m.cpu_peek(0x8000), 8);
        assert_eq!(m.cpu_peek(0xA000), 9);
        assert_eq!(m.cpu_peek(0xC000), 10);
        assert_eq!(m.cpu_peek(0xE000), 11);
    }

    #[test]
    fn prg_mode_1_splits_16k_at_8000_and_16k_at_c000() {
        let mut m = Mmc5::new(tagged_cart());
        m.cpu_write(0x5100, 0x01); // mode 1
        // $8000-$BFFF: 16 KB from $5115 & 0x7E. ROM (bit 7 set).
        m.cpu_write(0x5115, 0x84); // ROM, bank 4 (low bit stripped)
        // $C000-$FFFF: 16 KB from $5117 & 0x7E (always ROM).
        m.cpu_write(0x5117, 0x0C);
        assert_eq!(m.cpu_peek(0x8000), 4);
        assert_eq!(m.cpu_peek(0xA000), 5);
        assert_eq!(m.cpu_peek(0xC000), 12);
        assert_eq!(m.cpu_peek(0xE000), 13);
    }

    #[test]
    fn prg_mode_2_splits_16k_plus_8k_plus_8k() {
        let mut m = Mmc5::new(tagged_cart());
        m.cpu_write(0x5100, 0x02); // mode 2
        m.cpu_write(0x5115, 0x82); // ROM, bank 2 (16 KB)
        m.cpu_write(0x5116, 0x86); // ROM, bank 6 (8 KB)
        m.cpu_write(0x5117, 0x0A); // ROM, bank 10 (8 KB)
        assert_eq!(m.cpu_peek(0x8000), 2);
        assert_eq!(m.cpu_peek(0xA000), 3);
        assert_eq!(m.cpu_peek(0xC000), 6);
        assert_eq!(m.cpu_peek(0xE000), 10);
    }

    #[test]
    fn prg_mode_3_gives_four_independent_8k_slots() {
        let mut m = Mmc5::new(tagged_cart());
        m.cpu_write(0x5100, 0x03);
        m.cpu_write(0x5114, 0x83); // bank 3
        m.cpu_write(0x5115, 0x87); // bank 7
        m.cpu_write(0x5116, 0x8B); // bank 11
        m.cpu_write(0x5117, 0x0F); // bank 15 (always ROM)
        assert_eq!(m.cpu_peek(0x8000), 3);
        assert_eq!(m.cpu_peek(0xA000), 7);
        assert_eq!(m.cpu_peek(0xC000), 11);
        assert_eq!(m.cpu_peek(0xE000), 15);
    }

    #[test]
    fn bit7_clear_on_upper_reg_routes_slot_to_prg_ram() {
        // Mode 3, $5114 = 0x01 (bit 7 clear -> RAM, bank 1).
        // PRG-RAM is 4 × 8 KB, so bank 1 is the second 8 KB chunk.
        let mut m = Mmc5::new(tagged_cart());
        m.cpu_write(0x5100, 0x03);
        m.cpu_write(0x5114, 0x01);

        // Unlock protect, write a sentinel via $8000, confirm the same
        // byte reads back via $8000 and via the appropriately-banked
        // $6000 window.
        unlock_prg_ram(&mut m);
        m.cpu_write(0x8000, 0xAB);
        assert_eq!(m.cpu_peek(0x8000), 0xAB);
        // Route $6000-$7FFF to the same RAM bank (1) via $5113.
        m.cpu_write(0x5113, 0x01);
        assert_eq!(m.cpu_peek(0x6000), 0xAB);
    }

    #[test]
    fn bit7_on_5117_ignored_always_rom() {
        // $5117 bit 7 must not flip the slot to RAM — the last slot
        // is always ROM per real hardware. A value of 0x01 should
        // resolve to ROM bank 1.
        let mut m = Mmc5::new(tagged_cart());
        m.cpu_write(0x5100, 0x03);
        m.cpu_write(0x5117, 0x01);
        assert_eq!(m.cpu_peek(0xE000), 1);
        assert_eq!(m.cpu_peek(0xFFFF), 1);
    }

    #[test]
    fn prg_ram_write_blocked_until_both_protect_halves_match() {
        let mut m = Mmc5::new(tagged_cart());

        // Write with both halves at default (0/0) — should not stick.
        m.cpu_write(0x6000, 0xCC);
        assert_eq!(m.cpu_peek(0x6000), 0x00);

        // Only one half matches — still blocked.
        m.cpu_write(0x5102, 0b10);
        m.cpu_write(0x6000, 0xDD);
        assert_eq!(m.cpu_peek(0x6000), 0x00);

        // Full unlock — write lands.
        m.cpu_write(0x5103, 0b01);
        m.cpu_write(0x6000, 0xEE);
        assert_eq!(m.cpu_peek(0x6000), 0xEE);

        // Re-lock by scrambling either half.
        m.cpu_write(0x5102, 0x00);
        m.cpu_write(0x6000, 0xFF);
        assert_eq!(m.cpu_peek(0x6000), 0xEE);
    }

    #[test]
    fn prg_ram_bank_switch_via_5113() {
        let mut m = Mmc5::new(tagged_cart());
        unlock_prg_ram(&mut m);

        // Park distinct bytes in banks 0 and 2.
        m.cpu_write(0x5113, 0x00);
        m.cpu_write(0x6000, 0x11);
        m.cpu_write(0x5113, 0x02);
        m.cpu_write(0x6000, 0x22);

        // Confirm read-back follows the $5113 switch.
        m.cpu_write(0x5113, 0x00);
        assert_eq!(m.cpu_peek(0x6000), 0x11);
        m.cpu_write(0x5113, 0x02);
        assert_eq!(m.cpu_peek(0x6000), 0x22);
    }

    #[test]
    fn expansion_reads_return_none_until_wired_in_later_subphases() {
        // Sub-A leaves every $5000-$5FFF read falling through to open
        // bus via `None`. Sub-C/D/E override specific addresses.
        let mut m = Mmc5::new(tagged_cart());
        assert!(m.cpu_read_ex(0x5204).is_none()); // IRQ status (sub-C)
        assert!(m.cpu_read_ex(0x5205).is_none()); // multiply low (sub-D)
        assert!(m.cpu_read_ex(0x5206).is_none()); // multiply high (sub-D)
        assert!(m.cpu_read_ex(0x5C00).is_none()); // ExRAM (sub-E)
    }
}
