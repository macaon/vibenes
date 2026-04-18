//! MMC3 / TxROM (mapper 4).
//!
//! The MMC3 is the workhorse NES mapper — roughly 28% of the licensed
//! library. Four register pairs at $8000-$FFFF decoded by A0 + address:
//!
//! | addr mask `$E001` | write effect |
//! |---|---|
//! | `$8000` | Bank select: low 3 bits index R0..R7; bit 6 = PRG mode; bit 7 = CHR A12 inversion |
//! | `$8001` | Bank data: value -> R[bank_select & 7] (bit 0 masked for R0/R1) |
//! | `$A000` | Mirroring: bit 0 = 0 vertical, 1 horizontal (no-op if FourScreen) |
//! | `$A001` | PRG-RAM protect: bit 7 enable, bit 6 write-protect |
//! | `$C000` | IRQ latch — reload value |
//! | `$C001` | IRQ reload: counter <- 0, reload flag set |
//! | `$E000` | IRQ disable + acknowledge |
//! | `$E001` | IRQ enable |
//!
//! This module (phase 10A) implements only the banking side. The IRQ
//! state machine lands in phase 10B alongside an A12 rising-edge watcher
//! called from [`Mapper::on_ppu_addr`]. To keep the wire-up stable, the
//! IRQ registers are decoded here but stored into placeholder fields —
//! writes are absorbed without ever asserting [`Mapper::irq_line`].
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
//! Clean-room references (behavioral only, no copied code):
//! - `~/Git/Mesen2/Core/NES/Mappers/Nintendo/MMC3.h`
//! - `~/Git/Mesen2/Core/NES/Mappers/A12Watcher.h`
//! - `~/Git/puNES/src/core/mappers/MMC3.c`
//! - `~/Git/puNES/src/core/irqA12.c`
//! - `reference/mappers.md §Mapper 4`, `mesen-notes.md §20-21`, `punes-notes.md §MMC3 A12 filter`

use crate::mapper::Mapper;
use crate::rom::{Cartridge, Mirroring};

const PRG_BANK_8K: usize = 8 * 1024;
const CHR_BANK_1K: usize = 1024;
const PRG_RAM_SIZE: usize = 8 * 1024;

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

    /// $A001 bit 7 — PRG-RAM chip enable. Real MMC3 returns open bus when
    /// disabled; we return 0 for simplicity and because the current bus
    /// design doesn't expose open bus to mapper reads. Default enabled so
    /// carts that never write $A001 still work.
    prg_ram_enabled: bool,
    /// $A001 bit 6 — PRG-RAM write protect.
    prg_ram_write_protected: bool,

    prg_bank_count_8k: usize,
    chr_bank_count_1k: usize,

    // --- IRQ state (decoded in 10A, activated in 10B) ---
    irq_latch: u8,
    irq_counter: u8,
    irq_reload: bool,
    irq_enabled: bool,
    irq_line: bool,
}

impl Mmc3 {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count_8k = (cart.prg_rom.len() / PRG_BANK_8K).max(1);

        let is_chr_ram = cart.chr_ram || cart.chr_rom.is_empty();
        let chr = if is_chr_ram {
            vec![0u8; 8 * 1024]
        } else {
            cart.chr_rom
        };
        let chr_bank_count_1k = (chr.len() / CHR_BANK_1K).max(1);

        let prg_ram = vec![0u8; cart.prg_ram_size.max(PRG_RAM_SIZE)];

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
            prg_ram_enabled: true,
            prg_ram_write_protected: false,
            prg_bank_count_8k,
            chr_bank_count_1k,
            irq_latch: 0,
            irq_counter: 0,
            irq_reload: false,
            irq_enabled: false,
            irq_line: false,
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

    /// Resolve `$8000-$FFFF` to an 8 KB PRG bank index.
    fn prg_bank_for(&self, addr: u16) -> usize {
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

    /// Resolve `$0000-$1FFF` to a 1 KB CHR bank index.
    fn chr_bank_for(&self, addr: u16) -> usize {
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

    fn write_register(&mut self, addr: u16, value: u8) {
        // Decode by top 3 bits of addr + A0 (i.e. addr & 0xE001).
        match addr & 0xE001 {
            0x8000 => {
                self.bank_select = value;
            }
            0x8001 => {
                let idx = (self.bank_select & 0x07) as usize;
                let stored = if idx <= 1 {
                    // R0/R1 are 2 KB banks — low bit ignored so pairing
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
                self.prg_ram_enabled = (value & 0x80) != 0;
                self.prg_ram_write_protected = (value & 0x40) != 0;
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
                if self.prg_ram_enabled && !self.prg_ram_write_protected {
                    let i = (addr - 0x6000) as usize;
                    if let Some(slot) = self.prg_ram.get_mut(i) {
                        *slot = data;
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
                if self.prg_ram_enabled {
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
            tv_system: TvSystem::Ntsc,
            is_nes2: false,
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
        // Write R0 = 0x05 — bit 0 must be masked, giving 0x04. So
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
        write_reg(&mut m, 0xA001, 0); // different addr, wrong reg — shouldn't change mirror
        assert_eq!(m.mirroring(), Mirroring::Vertical);
        write_reg(&mut m, 0xA000, 1);
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
        // Odd address in the $A000 range still decodes as $A000
        // *unless* A0 is set — $A002 decodes as $A000, $A003 as $A001.
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
        // Clear enable bit — reads return 0 regardless of stored byte.
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

    // ---- Register-address aliasing ----

    #[test]
    fn a0_and_top_bits_select_register_bank() {
        // $8000 and $9FFE both decode as "bank select" (A0 clear, top
        // nibble = 0x8 or 0x9 — both mask to $8000). $8001 and $9FFF
        // decode as "bank data". Verify via a R6 write at a non-$8000
        // address.
        let mut m = Mmc3::new(tagged_cart());
        // Select R6 via $9FFE.
        m.cpu_write(0x9FFE, 6);
        // Write the bank value via $9FFF.
        m.cpu_write(0x9FFF, 5);
        assert_eq!(m.cpu_peek(0x8000), 5);
    }
}
