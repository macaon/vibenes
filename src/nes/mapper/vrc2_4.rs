// SPDX-License-Identifier: GPL-3.0-or-later
//! Konami VRC2 + VRC4 - iNES mappers 21, 22, 23, 25.
//!
//! These two ASIC families ship under four mapper numbers because the
//! Famicom carts wired the chip's address-decode pins differently per
//! board. The chip behavior is uniform; only the `(A0, A1)` extraction
//! from a register-write address (and a few VRC2-vs-VRC4 feature
//! gates) varies. We model it as one mapper struct with a 9-value
//! [`Variant`] selector, exactly like Mesen2's `VRC2_4.h`.
//!
//! ## Mapper-ID → Variant resolution
//!
//! |Mapper| Submapper 0 (heuristic) | 1 | 2 | 3 |
//! |---|---|---|---|---|
//! | 21 | VRC4a / VRC4c (OR-merged) | VRC4a | VRC4c | - |
//! | 22 | VRC2a                     | VRC2a | -     | - |
//! | 23 | VRC2b / VRC4e (OR-merged) | -     | VRC4e | VRC2b |
//! | 25 | VRC2c / VRC4b / VRC4d     | VRC4b | VRC4d | VRC2c |
//!
//! Submapper 0 (legacy iNES 1.0 ROMs) is the common case in the wild;
//! we OR the candidate `(A0, A1)` extractions across all variants of
//! that mapper number - Mesen2's trick to make non-NES-2.0 dumps work
//! without per-game annotation. NES 2.0 ROMs with explicit submappers
//! land on a single variant.
//!
//! ## Register map (post-translation, `addr & 0xF00F`)
//!
//! | Range | Effect |
//! |---|---|
//! | `$8000-$8006` (offset 0/2/4/6) | PRG reg 0 (5 bits) |
//! | `$9000-$9001` | Mirroring (low 1-2 bits of value) |
//! | `$9002-$9003` (VRC4 only) | PRG mode (bit 1 swaps slot 0 / fixed `-2`) |
//! | `$A000-$A006` | PRG reg 1 (5 bits) |
//! | `$B000-$E006` | CHR regs 0-7, lo (4 b) at offset 0/2, hi (5 b) at 1/3 |
//! | `$F000` (VRC4) | IRQ reload low nibble |
//! | `$F001` (VRC4) | IRQ reload high nibble |
//! | `$F002` (VRC4) | IRQ control |
//! | `$F003` (VRC4) | IRQ acknowledge |
//!
//! ## VRC2 microwire latch (`$6000-$6FFF`)
//!
//! VRC2 carts without WRAM expose a 1-bit storage latch through the
//! `$6000` window - used as the EEPROM data line on a couple of
//! Japanese titles. Reads return the latch in bit 0 (rest open-bus,
//! we surface 0); writes store value bit 0. VRC4 carts ignore this
//! and use the window for normal PRG-RAM.
//!
//! ## References
//!
//! Port of Mesen2's `Core/NES/Mappers/Konami/VRC2_4.h` and `VrcIrq.h`,
//! cross-checked against `~/Git/punes/src/core/mappers/VRC4.c` and
//! `~/Git/punes/src/core/mappers/VRC2.c`. The `VrcIrq` model is
//! identical to the one already living inside [`crate::nes::mapper::vrc6`];
//! we keep a separate copy here so VRC6's file stays self-contained
//! and to add VRC4's split-nibble reload registers (`$F000` low,
//! `$F001` high) which VRC6 doesn't have.

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_8K: usize = 8 * 1024;
const CHR_BANK_1K: usize = 1024;
const PRG_RAM_SIZE: usize = 8 * 1024;

const PRESCALER_RELOAD: i16 = 341;
const PRESCALER_STEP: i16 = 3;

/// Concrete chip variant. Drives both the address-translation pinout
/// and feature gates (IRQ presence, mirroring decode width, CHR
/// right-shift). Order matters: anything `>= Vrc4a` is "VRC4 family".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Variant {
    Vrc2a, // mapper 22
    Vrc2b, // mapper 23 (sub 0/3)
    Vrc2c, // mapper 25 (sub 3)
    Vrc4a, // mapper 21 (sub 0/1)
    Vrc4b, // mapper 25 (sub 0/1)
    Vrc4c, // mapper 21 (sub 2)
    Vrc4d, // mapper 25 (sub 2)
    Vrc4e, // mapper 23 (sub 2)
}

impl Variant {
    fn is_vrc2(self) -> bool {
        matches!(self, Variant::Vrc2a | Variant::Vrc2b | Variant::Vrc2c)
    }
    fn is_vrc4(self) -> bool {
        !self.is_vrc2()
    }
}

/// Resolve a `(mapper_id, submapper)` pair into a single canonical
/// variant. Submapper 0 returns the "heuristic-OR" placeholder: the
/// translator inside the mapper detects this case and ORs all
/// candidate extractions.
fn detect_variant(mapper_id: u16, submapper: u8) -> (Variant, bool) {
    let (variant, heuristic) = match mapper_id {
        21 => match submapper {
            2 => (Variant::Vrc4c, false),
            // Submapper 0 (legacy) and 1 both default to VRC4a - for
            // mapper 21 the legacy decode is "OR of VRC4a + VRC4c
            // address bits" which we activate via the heuristic flag.
            _ => (Variant::Vrc4a, submapper == 0),
        },
        22 => (Variant::Vrc2a, false),
        23 => match submapper {
            2 => (Variant::Vrc4e, false),
            // 0 / 3: VRC2b. Submapper 0 enables OR-merge with VRC4e.
            _ => (Variant::Vrc2b, submapper == 0),
        },
        25 => match submapper {
            2 => (Variant::Vrc4d, false),
            3 => (Variant::Vrc2c, false),
            // 0 / 1: VRC4b. Submapper 0 enables OR-merge with VRC2c +
            // VRC4d (Mesen2's mapper-25 heuristic).
            _ => (Variant::Vrc4b, submapper == 0),
        },
        // Out-of-range mapper IDs shouldn't reach here - `build`
        // dispatches on the four IDs above.
        _ => (Variant::Vrc4a, false),
    };
    (variant, heuristic)
}

// ---- VRC IRQ ----

#[derive(Debug, Clone)]
struct VrcIrq {
    reload_value: u8,
    counter: u8,
    prescaler: i16,
    enabled: bool,
    enabled_after_ack: bool,
    cycle_mode: bool,
    irq_line: bool,
}

impl VrcIrq {
    fn new() -> Self {
        Self {
            reload_value: 0,
            counter: 0,
            prescaler: 0,
            enabled: false,
            enabled_after_ack: false,
            cycle_mode: false,
            irq_line: false,
        }
    }

    fn clock(&mut self) {
        if !self.enabled {
            return;
        }
        let tick = if self.cycle_mode {
            true
        } else {
            self.prescaler -= PRESCALER_STEP;
            self.prescaler <= 0
        };
        if tick {
            if self.counter == 0xFF {
                self.counter = self.reload_value;
                self.irq_line = true;
            } else {
                self.counter += 1;
            }
            if !self.cycle_mode {
                self.prescaler += PRESCALER_RELOAD;
            }
        }
    }

    fn set_reload_nibble(&mut self, value: u8, high: bool) {
        if high {
            self.reload_value = (self.reload_value & 0x0F) | ((value & 0x0F) << 4);
        } else {
            self.reload_value = (self.reload_value & 0xF0) | (value & 0x0F);
        }
    }

    fn set_control(&mut self, value: u8) {
        self.enabled_after_ack = (value & 0x01) != 0;
        self.enabled = (value & 0x02) != 0;
        self.cycle_mode = (value & 0x04) != 0;
        if self.enabled {
            self.counter = self.reload_value;
            self.prescaler = PRESCALER_RELOAD;
        }
        self.irq_line = false;
    }

    fn acknowledge(&mut self) {
        self.enabled = self.enabled_after_ack;
        self.irq_line = false;
    }

    fn save_state_capture(&self) -> crate::save_state::mapper::VrcIrqSnap {
        crate::save_state::mapper::VrcIrqSnap {
            reload_value: self.reload_value,
            counter: self.counter,
            prescaler: self.prescaler,
            enabled: self.enabled,
            enabled_after_ack: self.enabled_after_ack,
            cycle_mode: self.cycle_mode,
            irq_line: self.irq_line,
        }
    }

    fn save_state_apply(&mut self, snap: crate::save_state::mapper::VrcIrqSnap) {
        self.reload_value = snap.reload_value;
        self.counter = snap.counter;
        self.prescaler = snap.prescaler;
        self.enabled = snap.enabled;
        self.enabled_after_ack = snap.enabled_after_ack;
        self.cycle_mode = snap.cycle_mode;
        self.irq_line = snap.irq_line;
    }
}

// ---- Mapper ----

pub struct Vrc2_4 {
    variant: Variant,
    /// Set when the iNES header didn't carry submapper information and
    /// the address translator must OR all candidate `(A0, A1)`
    /// extractions for this mapper number. NES 2.0 ROMs with explicit
    /// submappers leave this false.
    heuristic: bool,

    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,

    /// `$6000-$7FFF` window. On VRC4 (and VRC2 carts that declared
    /// PRG-RAM in the header) this is normal SRAM. On VRC2 carts
    /// without WRAM this stays length-zero and the microwire latch
    /// (`microwire_latch` below) handles the window instead.
    prg_ram: Vec<u8>,

    /// VRC2 microwire data-line latch. Bit 0 of `$6000` reads/writes
    /// this. Untouched on VRC4.
    microwire_latch: u8,
    /// True iff `$6000-$7FFF` should route to `microwire_latch` rather
    /// than `prg_ram`. Set on VRC2 carts that ship without WRAM.
    use_microwire: bool,

    prg_reg_0: u8,
    prg_reg_1: u8,
    prg_mode: u8,

    /// 4-bit low-nibble half of each CHR bank register.
    chr_lo: [u8; 8],
    /// 5-bit high half of each CHR bank register. Combined: `(hi << 4) | lo`.
    chr_hi: [u8; 8],

    mirroring: Mirroring,
    hardwired_four_screen: bool,

    irq: VrcIrq,

    battery_backed: bool,
    save_dirty: bool,
}

impl Vrc2_4 {
    pub fn new(cart: Cartridge) -> Self {
        let (variant, heuristic) = detect_variant(cart.mapper_id, cart.submapper);

        let chr_ram = cart.chr_ram;
        let chr = if chr_ram {
            // 8 KiB CHR-RAM is the typical configuration for VRC4
            // carts; banking still operates over the 8 1-KiB slots.
            vec![0u8; 8 * 1024]
        } else {
            cart.chr_rom
        };

        // VRC2 carts split into two camps: those with explicit WRAM
        // (Wai Wai World 2 etc.) and those that use the $6000 window
        // for the microwire 1-bit EEPROM latch (Crisis Force, Akumajou
        // Special: Boku Dracula-kun). The header gives us the cue:
        // PRG-RAM declared → real SRAM; no PRG-RAM AND VRC2 → latch.
        // VRC4 always uses real WRAM (battery on Wai Wai World 2 etc.).
        let cart_has_ram = cart.prg_ram_size > 0 || cart.prg_nvram_size > 0;
        let use_microwire = variant.is_vrc2() && !cart_has_ram && !heuristic;
        // When heuristic mode might collapse a VRC2 + VRC4 board into
        // one variant, we always allocate WRAM - the VRC4 case needs
        // it and the microwire latch is harmless to skip for the
        // boards that would have used it (those are sub-3 declared).
        let prg_ram = if use_microwire {
            Vec::new()
        } else {
            vec![0u8; cart_has_ram.then_some(cart.prg_ram_size + cart.prg_nvram_size).unwrap_or(PRG_RAM_SIZE).max(PRG_RAM_SIZE)]
        };

        let hardwired_four_screen = matches!(cart.mirroring, Mirroring::FourScreen);

        Self {
            variant,
            heuristic,
            prg_rom: cart.prg_rom,
            chr,
            chr_ram,
            prg_ram,
            microwire_latch: 0,
            use_microwire,
            prg_reg_0: 0,
            prg_reg_1: 0,
            prg_mode: 0,
            chr_lo: [0; 8],
            chr_hi: [0; 8],
            mirroring: cart.mirroring,
            hardwired_four_screen,
            irq: VrcIrq::new(),
            battery_backed: cart.battery_backed,
            save_dirty: false,
        }
    }

    fn prg_bank_mask_8k(&self) -> usize {
        (self.prg_rom.len() / PRG_BANK_8K).saturating_sub(1)
    }

    fn chr_bank_mask_1k(&self) -> usize {
        if self.chr.is_empty() {
            return 0;
        }
        (self.chr.len() / CHR_BANK_1K).saturating_sub(1)
    }

    /// Resolve a CPU `$8000-$FFFF` read to a byte. Slots are 8 KiB
    /// each; mode 0 fixes `-2` and `-1` at slots 2/3, mode 1 swaps
    /// slot 0 with the `-2` fixed bank.
    fn prg_read(&self, addr: u16) -> u8 {
        let mask = self.prg_bank_mask_8k();
        let last = mask;
        let second_last = mask.saturating_sub(1);
        let bank = match (addr, self.prg_mode & 0x01) {
            (0x8000..=0x9FFF, 0) => self.prg_reg_0 as usize & mask,
            (0x8000..=0x9FFF, _) => second_last,
            (0xA000..=0xBFFF, _) => self.prg_reg_1 as usize & mask,
            (0xC000..=0xDFFF, 0) => second_last,
            (0xC000..=0xDFFF, _) => self.prg_reg_0 as usize & mask,
            (0xE000..=0xFFFF, _) => last,
            _ => 0,
        };
        let off = (addr as usize) & (PRG_BANK_8K - 1);
        self.prg_rom
            .get(bank * PRG_BANK_8K + off)
            .copied()
            .unwrap_or(0)
    }

    fn chr_page(&self, slot: usize) -> usize {
        let mut page = (self.chr_lo[slot] as usize) | ((self.chr_hi[slot] as usize) << 4);
        if self.variant == Variant::Vrc2a {
            // VRC2a wires CHR A10..A16 (low bit of the index ignored).
            // Per nesdev: the high 7 bits of each CHR reg drive the
            // page select; we fold that down to the regular page space
            // by right-shifting by 1.
            page >>= 1;
        }
        page & self.chr_bank_mask_1k()
    }

    /// Translate a register-write address into canonical
    /// `$F00F`-stride form. Encodes the 9 different VRC2/VRC4 board
    /// pinouts; on heuristic mode (submapper 0) ORs all candidate
    /// extractions for the mapper number so legacy iNES dumps work.
    fn translate(&self, addr: u16) -> u16 {
        let (mut a0, mut a1) = match self.variant {
            Variant::Vrc2a => ((addr >> 1) & 1, addr & 1), // mapper 22
            Variant::Vrc2b => (addr & 1, (addr >> 1) & 1), // mapper 23 native
            Variant::Vrc2c => ((addr >> 1) & 1, addr & 1), // mapper 25 native (same as VRC4b)
            Variant::Vrc4a => ((addr >> 1) & 1, (addr >> 2) & 1), // mapper 21
            Variant::Vrc4b => ((addr >> 1) & 1, addr & 1), // mapper 25
            Variant::Vrc4c => ((addr >> 6) & 1, (addr >> 7) & 1), // mapper 21
            Variant::Vrc4d => ((addr >> 3) & 1, (addr >> 2) & 1), // mapper 25
            Variant::Vrc4e => ((addr >> 2) & 1, (addr >> 3) & 1), // mapper 23
        };

        if self.heuristic {
            // OR in the other candidate variant(s) for this mapper.
            // Mirrors Mesen2's `_useHeuristics` branch.
            match self.variant {
                Variant::Vrc4a => {
                    // mapper 21: VRC4a (chosen) + VRC4c.
                    a0 |= (addr >> 6) & 1;
                    a1 |= (addr >> 7) & 1;
                }
                Variant::Vrc2b => {
                    // mapper 23: VRC2b (chosen) + VRC4e.
                    a0 |= (addr >> 2) & 1;
                    a1 |= (addr >> 3) & 1;
                }
                Variant::Vrc4b => {
                    // mapper 25: VRC4b (chosen) + VRC2c (same bits, no
                    // change) + VRC4d.
                    a0 |= (addr >> 3) & 1;
                    a1 |= (addr >> 2) & 1;
                }
                _ => {}
            }
        }

        (addr & 0xFF00) | ((a1 & 1) << 1) | (a0 & 1)
    }

    fn write_register(&mut self, raw_addr: u16, value: u8) {
        let addr = self.translate(raw_addr) & 0xF00F;

        match addr {
            0x8000..=0x8006 if (addr & 0x000C) == 0 => {
                // $8000 / $8002 / $8004 / $8006 - PRG reg 0.
                self.prg_reg_0 = value & 0x1F;
            }
            0x9000..=0x9003 => {
                // Mesen2 routes the entire $9000-$9003 range to
                // mirroring on VRC2-family variants (including
                // heuristic - sub 0 leaves us classified as VRC2b on
                // mapper 23 even though some legacy ROMs are VRC4e).
                // VRC4-family variants use $9000/1 for mirroring and
                // $9002/3 for PRG mode. Earlier code wrongly let
                // heuristic VRC2b set PRG mode at $9002/3, which
                // corrupted Parodius's PRG layout on register
                // overlap.
                let is_mirror_addr = self.variant.is_vrc2() || matches!(addr, 0x9000 | 0x9001);
                if is_mirror_addr {
                    // VRC2 strictly latches the low bit; VRC4 (and
                    // heuristic VRC2 - Mesen2 says so) pick from the
                    // 4-way table.
                    let mask = if self.variant.is_vrc2() && !self.heuristic {
                        0x01
                    } else {
                        0x03
                    };
                    if !self.hardwired_four_screen {
                        self.mirroring = match value & mask {
                            0 => Mirroring::Vertical,
                            1 => Mirroring::Horizontal,
                            2 => Mirroring::SingleScreenLower,
                            3 => Mirroring::SingleScreenUpper,
                            _ => self.mirroring,
                        };
                    }
                } else {
                    // VRC4 PRG mode bit (only reachable when variant
                    // is VRC4-family; VRC2 took the branch above).
                    self.prg_mode = (value >> 1) & 0x01;
                }
            }
            0xA000..=0xA006 if (addr & 0x000C) == 0 => {
                // PRG reg 1.
                self.prg_reg_1 = value & 0x1F;
            }
            0xB000..=0xE006 => {
                // CHR regs. The reg index is ((addr_hi - 3) << 1) plus
                // bit 1 of the address (which selects between the two
                // regs that share a 4 KiB block). Bit 0 picks lo vs hi
                // nibble.
                let high_nibble = (addr & 0x0001) != 0;
                let reg = ((((addr >> 12) & 0x07) as usize) - 3) * 2 + (((addr >> 1) & 0x01) as usize);
                if reg < 8 {
                    if high_nibble {
                        self.chr_hi[reg] = value & 0x1F;
                    } else {
                        self.chr_lo[reg] = value & 0x0F;
                    }
                }
            }
            0xF000 => self.irq.set_reload_nibble(value, false),
            0xF001 => self.irq.set_reload_nibble(value, true),
            0xF002 => self.irq.set_control(value),
            0xF003 => self.irq.acknowledge(),
            _ => {}
        }
    }
}

impl Mapper for Vrc2_4 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => {
                if self.use_microwire {
                    // VRC2 microwire latch: low bit visible at $6000;
                    // upper bits open-bus (we surface zero - no real
                    // game samples them).
                    return self.microwire_latch & 0x01;
                }
                if self.prg_ram.is_empty() {
                    return 0;
                }
                let off = (addr - 0x6000) as usize % self.prg_ram.len();
                self.prg_ram[off]
            }
            0x8000..=0xFFFF => self.prg_read(addr),
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        match addr {
            0x6000..=0x7FFF => {
                if self.use_microwire {
                    self.microwire_latch = data & 0x01;
                    return;
                }
                if self.prg_ram.is_empty() {
                    return;
                }
                let off = (addr - 0x6000) as usize % self.prg_ram.len();
                if self.prg_ram[off] != data {
                    self.prg_ram[off] = data;
                    if self.battery_backed {
                        self.save_dirty = true;
                    }
                }
            }
            0x8000..=0xFFFF => self.write_register(addr, data),
            _ => {}
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr >= 0x2000 {
            return 0;
        }
        let slot = (addr as usize) / CHR_BANK_1K;
        let bank = self.chr_page(slot);
        let off = (addr as usize) & (CHR_BANK_1K - 1);
        self.chr
            .get(bank * CHR_BANK_1K + off)
            .copied()
            .unwrap_or(0)
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if !self.chr_ram || addr >= 0x2000 {
            return;
        }
        let slot = (addr as usize) / CHR_BANK_1K;
        let bank = self.chr_page(slot);
        let off = (addr as usize) & (CHR_BANK_1K - 1);
        if let Some(b) = self.chr.get_mut(bank * CHR_BANK_1K + off) {
            *b = data;
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }

    fn on_cpu_cycle(&mut self) {
        // VRC2 has no IRQ. In heuristic mode (sub 0) we let it run -
        // the IRQ stays disabled until something writes $F002, and
        // pure-VRC2 ROMs never touch those addresses, so the cost is
        // a single branch in `clock()`.
        if self.variant.is_vrc4() || self.heuristic {
            self.irq.clock();
        }
    }

    fn irq_line(&self) -> bool {
        self.irq.irq_line
    }

    fn save_data(&self) -> Option<&[u8]> {
        if self.battery_backed && !self.prg_ram.is_empty() {
            Some(&self.prg_ram)
        } else {
            None
        }
    }

    fn load_save_data(&mut self, data: &[u8]) {
        if data.len() == self.prg_ram.len() {
            self.prg_ram.copy_from_slice(data);
        }
    }

    fn save_dirty(&self) -> bool {
        self.save_dirty
    }

    fn mark_saved(&mut self) {
        self.save_dirty = false;
    }

    fn save_state_capture(&self) -> Option<crate::save_state::MapperState> {
        use crate::save_state::mapper::{MirroringSnap, Vrc24Snap};
        Some(crate::save_state::MapperState::Vrc24(Box::new(Vrc24Snap {
            prg_ram: self.prg_ram.clone(),
            chr_ram_data: if self.chr_ram { self.chr.clone() } else { Vec::new() },
            microwire_latch: self.microwire_latch,
            prg_reg_0: self.prg_reg_0,
            prg_reg_1: self.prg_reg_1,
            prg_mode: self.prg_mode,
            chr_lo: self.chr_lo,
            chr_hi: self.chr_hi,
            mirroring: MirroringSnap::from_live(self.mirroring),
            irq: self.irq.save_state_capture(),
            save_dirty: self.save_dirty,
        })))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::Vrc24(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        if snap.prg_ram.len() == self.prg_ram.len() {
            self.prg_ram.copy_from_slice(&snap.prg_ram);
        }
        if self.chr_ram && snap.chr_ram_data.len() == self.chr.len() {
            self.chr.copy_from_slice(&snap.chr_ram_data);
        }
        self.microwire_latch = snap.microwire_latch;
        self.prg_reg_0 = snap.prg_reg_0;
        self.prg_reg_1 = snap.prg_reg_1;
        self.prg_mode = snap.prg_mode;
        self.chr_lo = snap.chr_lo;
        self.chr_hi = snap.chr_hi;
        if !self.hardwired_four_screen {
            self.mirroring = snap.mirroring.to_live();
        }
        self.irq.save_state_apply(snap.irq);
        self.save_dirty = snap.save_dirty;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::TvSystem;

    fn make_cart(prg_banks_16k: usize, chr_banks_1k: usize, mapper_id: u16, submapper: u8) -> Cartridge {
        let prg_size = prg_banks_16k * 16 * 1024;
        let mut prg = vec![0u8; prg_size];
        for (i, b) in prg.iter_mut().enumerate() {
            *b = (i / PRG_BANK_8K) as u8;
        }
        let chr_size = chr_banks_1k * 1024;
        let mut chr = vec![0u8; chr_size];
        for (i, b) in chr.iter_mut().enumerate() {
            *b = ((i / CHR_BANK_1K) & 0xFF) as u8;
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: chr,
            chr_ram: chr_banks_1k == 0,
            mapper_id,
            submapper,
            mirroring: Mirroring::Vertical,
            battery_backed: false,
            prg_ram_size: 8 * 1024,
            prg_nvram_size: 0,
            tv_system: TvSystem::Ntsc,
            is_nes2: submapper != 0,
            prg_chr_crc32: 0,
            db_matched: false,
            fds_data: None,
        }
    }

    // ---- Variant detection ----

    #[test]
    fn detect_variant_table() {
        assert_eq!(detect_variant(21, 1), (Variant::Vrc4a, false));
        assert_eq!(detect_variant(21, 2), (Variant::Vrc4c, false));
        assert_eq!(detect_variant(22, 0), (Variant::Vrc2a, false));
        assert_eq!(detect_variant(23, 0).0, Variant::Vrc2b);
        assert_eq!(detect_variant(23, 2), (Variant::Vrc4e, false));
        assert_eq!(detect_variant(23, 3), (Variant::Vrc2b, false));
        assert_eq!(detect_variant(25, 1), (Variant::Vrc4b, false));
        assert_eq!(detect_variant(25, 2), (Variant::Vrc4d, false));
        assert_eq!(detect_variant(25, 3), (Variant::Vrc2c, false));
    }

    #[test]
    fn detect_variant_legacy_submapper_0_uses_heuristic() {
        assert_eq!(detect_variant(21, 0), (Variant::Vrc4a, true));
        assert_eq!(detect_variant(23, 0), (Variant::Vrc2b, true));
        assert_eq!(detect_variant(25, 0), (Variant::Vrc4b, true));
        assert_eq!(detect_variant(22, 0).1, false); // VRC2a is unambiguous
    }

    // ---- PRG layout ----

    #[test]
    fn prg_mode_0_swaps_slot_0_with_reg_0_and_pins_last_two() {
        // Mapper 23 sub 0 → VRC2b + VRC4e heuristic merge. The OR
        // collapses canonical $F00F space onto the natural raw
        // address layout, so we can write $8000, $9002, … directly
        // without per-variant arithmetic. Pre-VRC4 features (PRG
        // mode, IRQ) take the VRC4 path because heuristic mode is
        // on.
        let mut m = Vrc2_4::new(make_cart(8, 8, 23, 0));
        m.cpu_write(0x8000, 3); // PRG reg 0 = 3 → slot 0 reads bank 3
        assert_eq!(m.cpu_peek(0x8000), 3);
        // $A000 hits PRG reg 1 (default 0).
        assert_eq!(m.cpu_peek(0xA000), 0);
        // Fixed: -2 at $C000, -1 at $E000. PRG is 16 banks of 8 KiB → 14 and 15.
        assert_eq!(m.cpu_peek(0xC000), 14);
        assert_eq!(m.cpu_peek(0xE000), 15);
    }

    #[test]
    fn prg_mode_1_puts_fixed_bank_at_8000() {
        // Mapper 21 sub 1 → VRC4a (non-heuristic). VRC4a address
        // translation: A0=(raw>>1)&1, A1=(raw>>2)&1. Raw $9004 →
        // A0=0, A1=1 → canonical $9002 (PRG-mode register). Raw
        // $8000 (canonical $8000) sets PRG reg 0.
        let mut m = Vrc2_4::new(make_cart(8, 8, 21, 1));
        m.cpu_write(0x8000, 7); // PRG reg 0 = 7
        m.cpu_write(0x9004, 0x02); // PRG mode = 1
        // mode 1: -2 at $8000, reg1 at $A000, reg0 at $C000, -1 at $E000.
        assert_eq!(m.cpu_peek(0x8000), 14);
        assert_eq!(m.cpu_peek(0xC000), 7);
        assert_eq!(m.cpu_peek(0xE000), 15);
    }

    // ---- VRC2 mirroring uses 1 bit only ----

    #[test]
    fn vrc2_mirroring_only_low_bit() {
        let mut m = Vrc2_4::new(make_cart(8, 8, 22, 0));
        m.cpu_write(0x9000, 0x00);
        assert_eq!(m.mirroring(), Mirroring::Vertical);
        m.cpu_write(0x9000, 0x01);
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
        // Bit 1 set on a pure-VRC2 cart should still pick from {V, H},
        // not single-screen.
        m.cpu_write(0x9000, 0x02);
        assert_eq!(m.mirroring(), Mirroring::Vertical);
        m.cpu_write(0x9000, 0x03);
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
    }

    #[test]
    fn heuristic_vrc2_routes_9002_to_mirroring_not_prg_mode() {
        // Mapper 23 sub 0 → heuristic VRC2b. Per Mesen2, $9000-$9003
        // all set mirroring on VRC2-family variants; only VRC4 uses
        // $9002/3 for PRG mode. Regression: an earlier bug let the
        // heuristic flag pull $9002 into the PRG-mode branch and
        // corrupted Parodius's PRG layout.
        let mut m = Vrc2_4::new(make_cart(8, 8, 23, 0));
        m.cpu_write(0x9002, 0x02); // value bit 1 = "PRG mode" on VRC4
        assert_eq!(m.prg_mode, 0, "VRC2 must not set PRG mode at $9002");
        // Same write should land on the mirroring table (heuristic
        // mask is 0x03 so value 0x02 → SingleScreenLower).
        assert_eq!(m.mirroring(), Mirroring::SingleScreenLower);
    }

    #[test]
    fn vrc4_mirroring_uses_both_bits() {
        // Mapper 23 sub 2 → VRC4e (no heuristic). VRC4e translates
        // $9000 to canonical $9000 directly only when bits 2/3 of
        // raw are zero - they are.
        let mut m = Vrc2_4::new(make_cart(8, 8, 23, 2));
        m.cpu_write(0x9000, 0x02);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenLower);
        m.cpu_write(0x9000, 0x03);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenUpper);
    }

    // ---- CHR banking + VRC2a right-shift ----

    #[test]
    fn chr_lo_hi_combine_into_9_bit_page() {
        // Use mapper 23 sub 0 (heuristic) for natural canonical
        // addressing - see prg_mode tests above.
        let mut m = Vrc2_4::new(make_cart(8, 256, 23, 0));
        m.cpu_write(0xB000, 0x05); // lo = 5
        m.cpu_write(0xB001, 0x01); // hi = 1 → page = 0x15 = 21
        assert_eq!(m.ppu_read(0x0000), 21);
    }

    #[test]
    fn vrc2a_chr_right_shifts_page() {
        // VRC2a (mapper 22) translates raw $B001 to canonical $B002
        // (= reg-1 lo) because A0/A1 swap with addr bits 1/0. Drive
        // raw $B000 (canonical $B000 = reg-0 lo) and raw $B002
        // (canonical $B001 = reg-0 hi) instead.
        let mut m = Vrc2_4::new(make_cart(8, 256, 22, 0));
        m.cpu_write(0xB000, 0x04); // lo = 4
        m.cpu_write(0xB002, 0x00); // hi = 0 → raw page 4 → shifted 2
        assert_eq!(m.ppu_read(0x0000), 2);
        m.cpu_write(0xB002, 0x01); // raw page 0x14 → shifted 10
        assert_eq!(m.ppu_read(0x0000), 10);
    }

    // ---- VRC IRQ ----

    #[test]
    fn vrc4_cycle_mode_irq_fires_at_expected_cycle() {
        // Mapper 23 sub 0 → heuristic; F-page addresses translate to
        // their natural canonical form.
        let mut m = Vrc2_4::new(make_cart(8, 8, 23, 0));
        m.cpu_write(0xF000, 0x0C); // reload low nibble
        m.cpu_write(0xF001, 0x0F); // reload high nibble → 0xFC
        m.cpu_write(0xF002, 0x06); // cycle + enable
        for _ in 0..3 {
            m.on_cpu_cycle();
            assert!(!m.irq_line());
        }
        m.on_cpu_cycle();
        assert!(m.irq_line());
    }

    #[test]
    fn ack_disables_when_enable_after_ack_clear() {
        let mut m = Vrc2_4::new(make_cart(8, 8, 23, 0));
        m.cpu_write(0xF000, 0x0C);
        m.cpu_write(0xF001, 0x0F);
        m.cpu_write(0xF002, 0x06);
        for _ in 0..4 {
            m.on_cpu_cycle();
        }
        assert!(m.irq_line());
        m.cpu_write(0xF003, 0); // ack
        assert!(!m.irq_line());
        assert!(!m.irq.enabled);
    }

    // ---- Address translation: per-variant pinout ----

    #[test]
    fn vrc4b_address_translation() {
        // Mapper 25 sub 1 = VRC4b. PRG reg 0 lives at $8000; on a
        // VRC4b cart the CPU emits $8002 (A0=swapped) to hit it.
        let mut m = Vrc2_4::new(make_cart(8, 8, 25, 1));
        m.cpu_write(0x8002, 5); // VRC4b: A0=(addr>>1)&1=1, A1=addr&1=0 → $8001
        // This actually lands on PRG reg 0 too because the table
        // matches by (a1<<1)|a0 mapped onto $F00F space; a sequence
        // of writes to the four PRG-reg aliases must all land on
        // PRG reg 0:
        for raw in [0x8000u16, 0x8001, 0x8002, 0x8003, 0x8004, 0x8005, 0x8006, 0x8007] {
            m.prg_reg_0 = 0;
            m.cpu_write(raw, 9);
            assert_eq!(m.prg_reg_0, 9, "raw={raw:04X}");
        }
    }

    #[test]
    fn vrc2b_address_translation_into_prg_reg() {
        // Mapper 23 sub 3 = VRC2b. A0 = addr&1, A1 = (addr>>1)&1, so
        // $8001 lands on $8001 (still PRG reg 0 - addr & 0xF00F &
        // 0x000C == 0).
        let mut m = Vrc2_4::new(make_cart(8, 8, 23, 3));
        for raw in [0x8000u16, 0x8001, 0x8002, 0x8003] {
            m.prg_reg_0 = 0;
            m.cpu_write(raw, 4);
            assert_eq!(m.prg_reg_0, 4, "raw={raw:04X}");
        }
    }

    // ---- VRC2 microwire latch ----

    #[test]
    fn vrc2_microwire_latch_roundtrips_low_bit() {
        // Mapper 22 with no PRG-RAM declared → microwire path.
        let mut cart = make_cart(8, 8, 22, 0);
        cart.prg_ram_size = 0;
        cart.prg_nvram_size = 0;
        let mut m = Vrc2_4::new(cart);
        assert!(m.use_microwire);
        m.cpu_write(0x6000, 0x00);
        assert_eq!(m.cpu_peek(0x6000), 0);
        m.cpu_write(0x6000, 0x01);
        assert_eq!(m.cpu_peek(0x6000), 1);
        m.cpu_write(0x6000, 0xFE); // only bit 0 stored
        assert_eq!(m.cpu_peek(0x6000), 0);
    }

    // ---- Battery dirty flag (VRC4 with battery WRAM) ----

    #[test]
    fn vrc4_battery_writes_set_dirty() {
        let mut cart = make_cart(8, 8, 21, 1);
        cart.battery_backed = true;
        let mut m = Vrc2_4::new(cart);
        m.cpu_write(0x6500, 0x42);
        assert!(m.save_dirty());
        m.mark_saved();
        assert!(!m.save_dirty());
        m.cpu_write(0x6500, 0x42); // same value: no re-dirty
        assert!(!m.save_dirty());
    }
}
