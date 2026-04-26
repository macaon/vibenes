// SPDX-License-Identifier: GPL-3.0-or-later
//! Konami VRC6 - iNES mappers 24 (VRC6a) and 26 (VRC6b).
//!
//! The VRC6 is a Konami-proprietary mapper/audio ASIC found on five
//! Famicom titles, most notably Akumajō Densetsu (the JP Castlevania
//! III) where the sawtooth channel drives the lead melody. The mapper
//! logic is straightforward - PRG/CHR banking, scanline IRQ - while
//! the audio is the marquee feature (ported separately in
//! [`crate::mapper::vrc6_audio`]).
//!
//! ## Register map
//!
//! | Range | Purpose |
//! |---|---|
//! | `$6000-$7FFF` | 8 KiB PRG-RAM; access gated by `$B003.7` |
//! | `$8000-$8003` | 16 KiB PRG bank at `$8000-$BFFF` (bits 0-3) |
//! | `$9000-$9003` | Pulse 1 + audio control (delegated to [`Vrc6Audio`]) |
//! | `$A000-$A002` | Pulse 2 (audio) |
//! | `$B000-$B002` | Sawtooth (audio) |
//! | `$B003`       | Banking mode: mirroring, CHR layout, PRG-RAM enable |
//! | `$C000-$C003` | 8 KiB PRG bank at `$C000-$DFFF` (bits 0-4) |
//! | `$D000-$D003` | CHR bank registers 0-3 (1 KiB each) |
//! | `$E000-$E003` | CHR bank registers 4-7 (1 KiB each) |
//! | `$F000`       | VRC IRQ reload value |
//! | `$F001`       | VRC IRQ control |
//! | `$F002`       | VRC IRQ acknowledge |
//!
//! `$E000-$FFFF` is hardwired to the last 16 KiB of PRG (last two
//! 8 KiB banks).
//!
//! ## VRC6a vs VRC6b (mappers 24 vs 26)
//!
//! Same chip, different pin wiring: VRC6b swaps A0 and A1 on the
//! cart's address decoder. Emulation-wise we undo the swap on
//! writes before dispatching: `(a & 0xFFFC) | ((a&1)<<1) | ((a&2)>>1)`.
//! Nestopia/Mesen2/puNES all use this exact remap.
//!
//! ## `$B003` banking-mode decode (common cases)
//!
//! The full combinational table in Mesen2 covers CHR-ROM-as-nametable
//! split-screen modes used by a handful of homebrew ROMs. We
//! implement the real-world mirroring cases explicitly and treat
//! the exotic `bit 4 = 1` mode as a no-op (falls back to
//! CIRAM-mirrored default). The four commercial VRC6 titles all
//! use the simple path.
//!
//! ## References
//!
//! Port of Mesen2's `Core/NES/Mappers/Konami/VRC6.h` +
//! `VrcIrq.h`. Cross-referenced against `~/Git/punes/src/core/mappers/VRC6.c`
//! for the IRQ prescaler constant (341 CPU cycles per scanline), the
//! `enabled_after_ack` latch semantics, and the "clear IRQ source on
//! control write" detail.

use crate::mapper::vrc6_audio::Vrc6Audio;
use crate::mapper::Mapper;
use crate::rom::{Cartridge, Mirroring};

const PRG_BANK_8K: usize = 8 * 1024;
const CHR_BANK_1K: usize = 1024;
const PRG_RAM_SIZE: usize = 8 * 1024;

/// VRC scanline-mode IRQ prescaler reload (CPU cycles per scanline).
/// Mesen2 and puNES both use 341; the wiki says "341 CPU cycles but
/// gives 113.666 lines per frame when normalized against NTSC master
/// clock" - close enough to a true scanline for the IRQ timing games
/// that use this chip expect.
const PRESCALER_RELOAD: i16 = 341;

/// Number of CPU cycles of prescaler decrement per `clock` call.
/// Per Mesen2 / puNES: 3 (matches the 3 PPU dots per CPU cycle).
const PRESCALER_STEP: i16 = 3;

/// Flavour of the chip - only differs in the A0/A1 address swap on
/// register writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Variant {
    A, // mapper 24
    B, // mapper 26
}

// ---- VRC IRQ (shared with VRC2/4/6/7 per Konami convention) ----

/// Konami VRC-series IRQ. Two modes:
///
/// - **Scanline** (`cycle_mode = false`): prescaler decrements 3
///   CPU cycles at a time; hitting zero or below reloads the
///   prescaler and ticks the main counter.
/// - **Cycle** (`cycle_mode = true`): main counter ticks every CPU
///   cycle directly.
///
/// Counter overflow (`0xFF → reload`) asserts the mapper IRQ line.
/// Ack via `$F002` reloads the enable flag from `enable_after_ack`
/// - the "auto-restart" trick used by title-screen fade-ins.
///
/// Port of Mesen2's `VrcIrq` with Rust idioms; state machine is
/// byte-for-byte behavior-identical.
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

    fn set_reload_value(&mut self, value: u8) {
        self.reload_value = value;
    }

    fn set_control(&mut self, value: u8) {
        self.enabled_after_ack = (value & 0x01) != 0;
        self.enabled = (value & 0x02) != 0;
        self.cycle_mode = (value & 0x04) != 0;
        if self.enabled {
            self.counter = self.reload_value;
            self.prescaler = PRESCALER_RELOAD;
        }
        // $F001 writes also clear the pending-IRQ line per Mesen2
        // `ClearIrqSource`. Avoids a phantom IRQ hanging around
        // after the game rewrote the control register.
        self.irq_line = false;
    }

    fn acknowledge(&mut self) {
        self.enabled = self.enabled_after_ack;
        self.irq_line = false;
    }
}

// ---- Mapper ----

pub struct Vrc6 {
    variant: Variant,
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,
    prg_ram: Vec<u8>,

    /// `$8000` (bits 0-3) × 16 KiB at `$8000-$BFFF`. Expressed as an
    /// 8 KiB "page 0" index so `page0 = bank16k * 2` and the next 8 KiB
    /// slot at `$A000-$BFFF` is `page0 + 1`.
    prg_8000_16k: u8,
    /// `$C000` (bits 0-4) × 8 KiB at `$C000-$DFFF`.
    prg_c000_8k: u8,
    /// `$D000-$E003` CHR bank registers (1 KiB each).
    chr_regs: [u8; 8],
    /// `$B003` - banking mode + mirroring + PRG-RAM enable.
    banking_mode: u8,

    mirroring: Mirroring,
    hardwired_four_screen: bool,

    irq: VrcIrq,
    audio: Vrc6Audio,

    /// Save/load dirty flag for battery-backed PRG-RAM. Most VRC6
    /// carts don't have battery SRAM; Akumajō Densetsu has 8 KiB of
    /// battery RAM. Gate the dirty flip on `battery_backed`.
    battery_backed: bool,
    save_dirty: bool,
}

impl Vrc6 {
    pub fn new_a(cart: Cartridge) -> Self {
        Self::build(cart, Variant::A)
    }

    pub fn new_b(cart: Cartridge) -> Self {
        Self::build(cart, Variant::B)
    }

    fn build(cart: Cartridge, variant: Variant) -> Self {
        let chr_ram = cart.chr_ram;
        let chr = if chr_ram {
            vec![0u8; 8 * 1024]
        } else {
            cart.chr_rom
        };
        let prg_ram_size = cart.prg_ram_size.max(PRG_RAM_SIZE);
        let hardwired_four_screen = matches!(cart.mirroring, Mirroring::FourScreen);
        Self {
            variant,
            prg_rom: cart.prg_rom,
            chr,
            chr_ram,
            prg_ram: vec![0u8; prg_ram_size],
            prg_8000_16k: 0,
            prg_c000_8k: 0,
            chr_regs: [0; 8],
            banking_mode: 0,
            mirroring: cart.mirroring,
            hardwired_four_screen,
            irq: VrcIrq::new(),
            audio: Vrc6Audio::new(),
            battery_backed: cart.battery_backed,
            save_dirty: false,
        }
    }

    /// Map the address through the VRC6b A0/A1 swap when necessary.
    /// No-op for VRC6a.
    fn normalize_addr(&self, addr: u16) -> u16 {
        match self.variant {
            Variant::A => addr,
            Variant::B => (addr & 0xFFFC) | ((addr & 0x01) << 1) | ((addr & 0x02) >> 1),
        }
    }

    /// Resolve `$8000-$FFFF` CPU read to a byte in `prg_rom`.
    fn prg_read(&self, addr: u16) -> u8 {
        let offset = addr as usize - 0x8000;
        let bank8k: usize = match addr {
            0x8000..=0x9FFF => (self.prg_8000_16k as usize * 2) & self.prg_bank_mask_8k(),
            0xA000..=0xBFFF => (self.prg_8000_16k as usize * 2 + 1) & self.prg_bank_mask_8k(),
            0xC000..=0xDFFF => (self.prg_c000_8k as usize) & self.prg_bank_mask_8k(),
            // `$E000-$FFFF` is hardwired to the last 8 KiB (Mesen2
            // initializes `SelectPrgPage(3, -1)`).
            _ => self.prg_bank_mask_8k(),
        };
        let slot_offset = offset & (PRG_BANK_8K - 1);
        self.prg_rom[bank8k * PRG_BANK_8K + slot_offset]
    }

    fn prg_bank_mask_8k(&self) -> usize {
        (self.prg_rom.len() / PRG_BANK_8K).saturating_sub(1)
    }

    /// CHR mapping for `$0000-$1FFF`. Per `$B003 & 0x03`:
    ///  - 0: 8× 1 KiB, one per register directly.
    ///  - 1: 4× 2 KiB. Each chr_reg[0..4] gates a 2 KiB window; the
    ///    OR-mask bit from $B003.5 selects which half of the 2 KiB
    ///    the upper slot gets.
    ///  - 2, 3: 4× 1 KiB (regs 0-3) + 2× 2 KiB (regs 4, 5).
    ///
    /// The CHR registers are 8-bit indices of 1 KiB banks.
    fn chr_bank_1k(&self, slot: usize) -> usize {
        let mode = self.banking_mode & 0x03;
        let mask = if (self.banking_mode & 0x20) != 0 {
            0xFE
        } else {
            0xFF
        };
        let or_mask = if (self.banking_mode & 0x20) != 0 { 1 } else { 0 };

        let idx = match mode {
            0 => self.chr_regs[slot],
            1 => {
                // 4 × 2 KiB: slot pairs (0,1), (2,3), (4,5), (6,7)
                // pull from chr_reg[slot/2] with the OR-mask on odd slots.
                let base = self.chr_regs[slot / 2] & mask;
                if (slot & 1) == 0 {
                    base
                } else {
                    base | or_mask
                }
            }
            _ => {
                // 4 × 1 KiB (regs 0-3) for slots 0-3, then 2 × 2 KiB
                // (regs 4, 5) for slots 4-7.
                if slot < 4 {
                    self.chr_regs[slot]
                } else {
                    let base = self.chr_regs[4 + (slot - 4) / 2] & mask;
                    if ((slot - 4) & 1) == 0 {
                        base
                    } else {
                        base | or_mask
                    }
                }
            }
        };
        (idx as usize) & self.chr_bank_mask_1k()
    }

    fn chr_bank_mask_1k(&self) -> usize {
        if self.chr.is_empty() {
            return 0;
        }
        (self.chr.len() / CHR_BANK_1K).saturating_sub(1)
    }

    /// Apply `$B003` to derive mirroring. The hardwired four-screen
    /// flag on the cart overrides anything written here. Per Mesen2,
    /// only specific bit combinations select a fixed mirroring; the
    /// "default" case reads the low bit of certain CHR registers to
    /// drive per-nametable-slot routing (split-screen). We don't
    /// support the split-screen case - none of the commercial VRC6
    /// titles use it - so "default" falls back to the cart's header
    /// mirroring.
    fn update_mirroring(&mut self) {
        if self.hardwired_four_screen {
            self.mirroring = Mirroring::FourScreen;
            return;
        }
        // CHR-ROM-as-NT mode (bit 4) is also out-of-scope - treat as
        // CIRAM path. Real usage is homebrew-only.
        let masked = self.banking_mode & 0x2F;
        self.mirroring = match masked {
            0x20 | 0x27 => Mirroring::Vertical,
            0x23 | 0x24 => Mirroring::Horizontal,
            0x28 | 0x2F => Mirroring::SingleScreenLower,
            0x2B | 0x2C => Mirroring::SingleScreenUpper,
            // Any other combination falls back to whatever came from
            // the iNES header. This keeps Akumajō Densetsu (which
            // sets $B003 = 0x20 / 0x23 for V/H) working, without
            // bothering the split-screen edge case.
            _ => self.mirroring,
        };
    }

    fn write_register(&mut self, addr: u16, value: u8) {
        let addr = self.normalize_addr(addr);
        match addr & 0xF003 {
            // $8000-$8003: 16 KiB PRG bank at $8000.
            0x8000 | 0x8001 | 0x8002 | 0x8003 => {
                self.prg_8000_16k = value & 0x0F;
            }
            // $9000-$9002: pulse 1. $9003: audio control.
            0x9000 | 0x9001 | 0x9002 | 0x9003 => self.audio.write_register(addr, value),
            // $A000-$A002: pulse 2.
            0xA000 | 0xA001 | 0xA002 => self.audio.write_register(addr, value),
            // $B000-$B002: sawtooth.
            0xB000 | 0xB001 | 0xB002 => self.audio.write_register(addr, value),
            // $B003: banking mode / mirroring.
            0xB003 => {
                self.banking_mode = value;
                self.update_mirroring();
            }
            // $C000-$C003: 8 KiB PRG bank at $C000.
            0xC000 | 0xC001 | 0xC002 | 0xC003 => {
                self.prg_c000_8k = value & 0x1F;
            }
            // $D000-$D003: CHR regs 0-3.
            0xD000 | 0xD001 | 0xD002 | 0xD003 => {
                self.chr_regs[(addr & 0x03) as usize] = value;
            }
            // $E000-$E003: CHR regs 4-7.
            0xE000 | 0xE001 | 0xE002 | 0xE003 => {
                self.chr_regs[4 + (addr & 0x03) as usize] = value;
            }
            // $F000: IRQ reload value.
            0xF000 => self.irq.set_reload_value(value),
            // $F001: IRQ control.
            0xF001 => self.irq.set_control(value),
            // $F002: IRQ acknowledge.
            0xF002 => self.irq.acknowledge(),
            _ => {}
        }
    }
}

impl Mapper for Vrc6 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => {
                // PRG-RAM access is gated by $B003.7. When disabled
                // real hardware returns open bus; we return 0.
                if (self.banking_mode & 0x80) == 0 {
                    return 0;
                }
                let offset = (addr - 0x6000) as usize % self.prg_ram.len().max(1);
                self.prg_ram.get(offset).copied().unwrap_or(0)
            }
            0x8000..=0xFFFF => self.prg_read(addr),
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        match addr {
            0x6000..=0x7FFF => {
                if (self.banking_mode & 0x80) == 0 {
                    return;
                }
                let offset = (addr - 0x6000) as usize % self.prg_ram.len().max(1);
                if let Some(slot) = self.prg_ram.get_mut(offset) {
                    if *slot != data {
                        *slot = data;
                        if self.battery_backed {
                            self.save_dirty = true;
                        }
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
        let bank = self.chr_bank_1k(slot);
        let offset = (addr as usize) & (CHR_BANK_1K - 1);
        self.chr
            .get(bank * CHR_BANK_1K + offset)
            .copied()
            .unwrap_or(0)
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if !self.chr_ram || addr >= 0x2000 {
            return;
        }
        let slot = (addr as usize) / CHR_BANK_1K;
        let bank = self.chr_bank_1k(slot);
        let offset = (addr as usize) & (CHR_BANK_1K - 1);
        if let Some(slot) = self.chr.get_mut(bank * CHR_BANK_1K + offset) {
            *slot = data;
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }

    fn on_cpu_cycle(&mut self) {
        self.irq.clock();
        self.audio.clock();
    }

    fn irq_line(&self) -> bool {
        self.irq.irq_line
    }

    fn audio_output(&self) -> Option<f32> {
        Some(self.audio.mix_sample())
    }

    fn save_data(&self) -> Option<&[u8]> {
        if self.battery_backed {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rom::{Cartridge, TvSystem};

    fn make_cart(prg_banks_16k: usize, chr_banks_1k: usize, variant: Variant) -> Cartridge {
        let prg_size = prg_banks_16k * 16 * 1024;
        let mut prg = vec![0u8; prg_size];
        // Tag each 8 KiB bank with its index so reads can verify the
        // mapper is selecting the right one.
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
            mapper_id: match variant {
                Variant::A => 24,
                Variant::B => 26,
            },
            submapper: 0,
            mirroring: Mirroring::Vertical,
            battery_backed: true,
            prg_ram_size: 8 * 1024,
            prg_nvram_size: 0,
            tv_system: TvSystem::Ntsc,
            is_nes2: false,
            prg_chr_crc32: 0,
            db_matched: false,
            fds_data: None,
        }
    }

    // ---- Basic construction + fixed bank layout ----

    #[test]
    fn reset_maps_last_bank_fixed_at_e000() {
        let m = Vrc6::new_a(make_cart(8, 8, Variant::A));
        // Last 8 KiB of PRG tagged with bank index 15 (8 × 16 KiB / 8 KiB).
        assert_eq!(m.cpu_peek(0xE000), 15);
        assert_eq!(m.cpu_peek(0xFFFF), 15);
    }

    #[test]
    fn write_8000_switches_16k_bank_at_8000() {
        let mut m = Vrc6::new_a(make_cart(8, 8, Variant::A));
        // Bank 3 → page 6,7 cover $8000-$BFFF.
        m.cpu_write(0x8000, 3);
        assert_eq!(m.cpu_peek(0x8000), 6); // page 6 start
        assert_eq!(m.cpu_peek(0xA000), 7); // page 7 start
    }

    #[test]
    fn write_c000_switches_8k_bank_at_c000() {
        let mut m = Vrc6::new_a(make_cart(8, 8, Variant::A));
        m.cpu_write(0xC000, 9); // 8 KiB bank 9 at $C000
        assert_eq!(m.cpu_peek(0xC000), 9);
        assert_eq!(m.cpu_peek(0xDFFF), 9);
    }

    // ---- VRC6b address swap ----

    /// VRC6b (mapper 26) swaps A0 and A1. Writing to `$8001` on a
    /// VRC6b cart should land on `$8002` semantically (still the
    /// same PRG register) - but writing to `$F001` (IRQ control)
    /// vs `$F002` (IRQ ack) matters, because those are distinct
    /// sub-registers and the swap picks the wrong one if ignored.
    #[test]
    fn vrc6b_swaps_a0_a1_on_irq_registers() {
        let mut m = Vrc6::new_b(make_cart(8, 8, Variant::B));
        // Enable IRQ in cycle mode via $F001 - which, on a VRC6b
        // cart, the CPU writes to $F002 (since A0/A1 are swapped).
        m.cpu_write(0xF000, 0xFE); // reload = 0xFE
        m.cpu_write(0xF002, 0x06); // swap → $F001: ctrl = cycle + enable
        assert!(m.irq.enabled);
        assert!(m.irq.cycle_mode);
        assert_eq!(m.irq.counter, 0xFE);
    }

    #[test]
    fn vrc6a_does_not_swap() {
        let mut m = Vrc6::new_a(make_cart(8, 8, Variant::A));
        m.cpu_write(0xF000, 0xFE);
        m.cpu_write(0xF001, 0x06); // no swap → control write
        assert!(m.irq.enabled);
    }

    // ---- PRG-RAM gate ----

    #[test]
    fn prg_ram_gated_by_b003_bit_7() {
        let mut m = Vrc6::new_a(make_cart(8, 8, Variant::A));
        // Disabled by default.
        m.cpu_write(0x6000, 0xAB);
        assert_eq!(m.cpu_peek(0x6000), 0);
        // Enable via $B003.7.
        m.cpu_write(0xB003, 0x80);
        m.cpu_write(0x6000, 0xAB);
        assert_eq!(m.cpu_peek(0x6000), 0xAB);
    }

    #[test]
    fn prg_ram_writes_set_dirty_on_battery_carts() {
        let mut m = Vrc6::new_a(make_cart(8, 8, Variant::A));
        m.cpu_write(0xB003, 0x80); // enable PRG-RAM
        m.cpu_write(0x6500, 0x42);
        assert!(m.save_dirty());
        m.mark_saved();
        assert!(!m.save_dirty());
        // Writing the same value doesn't re-dirty.
        m.cpu_write(0x6500, 0x42);
        assert!(!m.save_dirty());
    }

    // ---- Mirroring via $B003 ----

    #[test]
    fn b003_mirroring_decodes_basic_cases() {
        let mut m = Vrc6::new_a(make_cart(8, 8, Variant::A));
        m.cpu_write(0xB003, 0x20);
        assert_eq!(m.mirroring(), Mirroring::Vertical);
        m.cpu_write(0xB003, 0x23);
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
        m.cpu_write(0xB003, 0x28);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenLower);
        m.cpu_write(0xB003, 0x2B);
        assert_eq!(m.mirroring(), Mirroring::SingleScreenUpper);
    }

    // ---- CHR banking modes ----

    #[test]
    fn chr_mode_0_selects_each_slot_independently() {
        let mut m = Vrc6::new_a(make_cart(8, 16, Variant::A));
        m.cpu_write(0xB003, 0x00); // mode = 0
        m.cpu_write(0xD000, 5); // slot 0 → bank 5
        m.cpu_write(0xD001, 10); // slot 1 → bank 10
        assert_eq!(m.ppu_read(0x0000), 5);
        assert_eq!(m.ppu_read(0x0400), 10);
    }

    #[test]
    fn chr_mode_1_pairs_slots_as_2k_banks() {
        let mut m = Vrc6::new_a(make_cart(8, 16, Variant::A));
        m.cpu_write(0xB003, 0x01); // mode = 1, or-mask bit off
        m.cpu_write(0xD000, 4); // chr_reg[0] = 4 → slots 0,1
        // Slots 0 and 1 both read from 4 (or-mask off).
        assert_eq!(m.ppu_read(0x0000), 4);
        assert_eq!(m.ppu_read(0x0400), 4);
        // Turn on or-mask: slot 1 becomes bank 5 (4 | 1).
        m.cpu_write(0xB003, 0x21);
        m.cpu_write(0xD000, 4);
        assert_eq!(m.ppu_read(0x0000), 4);
        assert_eq!(m.ppu_read(0x0400), 5);
    }

    // ---- VRC IRQ ----

    /// Cycle-mode IRQ fires after (0xFF - reload + 1) CPU cycles
    /// once enabled.
    #[test]
    fn cycle_mode_irq_fires_at_expected_cycle() {
        let mut m = Vrc6::new_a(make_cart(8, 8, Variant::A));
        m.cpu_write(0xF000, 0xFC); // reload = 0xFC → 3 cycles to fire
        m.cpu_write(0xF001, 0x06); // cycle + enable
        // Clock enables load, so counter = 0xFC. Each clock advances.
        // After the set_control call, on the next clock:
        // cycle1: counter 0xFC → 0xFD
        // cycle2: counter 0xFD → 0xFE
        // cycle3: counter 0xFE → 0xFF
        // cycle4: counter 0xFF → reload (0xFC), IRQ fires.
        for _ in 0..3 {
            m.on_cpu_cycle();
            assert!(!m.irq_line());
        }
        m.on_cpu_cycle();
        assert!(m.irq_line());
    }

    /// Ack latches enable from enable_after_ack. With bit 0 clear
    /// on the control write, ack disables the IRQ.
    #[test]
    fn ack_disables_when_enable_after_ack_clear() {
        let mut m = Vrc6::new_a(make_cart(8, 8, Variant::A));
        m.cpu_write(0xF000, 0xFC);
        m.cpu_write(0xF001, 0x06); // enabled=1, eaa=0, cycle
        for _ in 0..4 {
            m.on_cpu_cycle();
        }
        assert!(m.irq_line());
        m.cpu_write(0xF002, 0); // ack
        assert!(!m.irq_line());
        assert!(!m.irq.enabled);
    }

    /// Ack with `enable_after_ack` = 1 re-enables IRQ on next cycle,
    /// letting games use a self-restarting tick train.
    #[test]
    fn ack_reloads_enable_from_enable_after_ack() {
        let mut m = Vrc6::new_a(make_cart(8, 8, Variant::A));
        m.cpu_write(0xF000, 0xFC);
        m.cpu_write(0xF001, 0x07); // eaa=1, enabled=1, cycle
        for _ in 0..4 {
            m.on_cpu_cycle();
        }
        assert!(m.irq_line());
        m.cpu_write(0xF002, 0); // ack
        assert!(m.irq.enabled);
    }

    // ---- Audio hook ----

    #[test]
    fn audio_output_routes_through_mapper_trait() {
        let mut m = Vrc6::new_a(make_cart(8, 8, Variant::A));
        // Set up pulse1 ignore-duty, vol=10, enabled, freq=0.
        m.cpu_write(0x9000, 0x8A);
        m.cpu_write(0x9001, 0x00);
        m.cpu_write(0x9002, 0x80);
        m.on_cpu_cycle();
        let s = Mapper::audio_output(&m).unwrap();
        // 10 raw units × VRC6_MIX_SCALE ≈ 10 × 0.01494 ≈ 0.149.
        assert!(s > 0.10 && s < 0.20, "got {s}");
    }
}
