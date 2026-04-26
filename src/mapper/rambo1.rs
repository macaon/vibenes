// SPDX-License-Identifier: GPL-3.0-or-later
//! Tengen RAMBO-1 - iNES mapper 64 (and submapper 158 - Atari/Namco
//! Tengen Alien Syndrome, with extra 1-screen NT control).
//!
//! Atari Games' answer to the MMC3. Same general silhouette - eight
//! bank-register slots fronted by a `$8000`/`$8001` write-pair
//! addressing scheme - but with a few extensions:
//!
//! - **Six CHR registers** (R0..R5) instead of MMC3's six, plus **two
//!   extra 1 KB CHR registers** (R8/R9) selectable via the new
//!   "1 KB-everywhere" CHR mode bit. R0/R1 still address 2 KB pairs in
//!   the legacy mode, exactly like MMC3.
//! - **Three PRG registers** (R6, R7, R15) plus a 4th-slot mode bit,
//!   so the cart can pin the fixed bank to either `$E000` (legacy MMC3
//!   layout) or `$8000` (legacy MMC3 with PRG mode 1 swapped further).
//! - **Two IRQ counter modes**: the familiar A12-rising-edge counter
//!   from MMC3, plus an alternative "every 4 CPU cycles" mode used by
//!   *Skull & Crossbones* and *Hard Drivin'*. The same `$C000`/`$C001`
//!   write pair drives both.
//! - **Tighter A12 filter**: real silicon needs A12 held low for ~30
//!   PPU dots before a rising edge counts (vs. MMC3's ~10). Mesen2 calls
//!   this out in `Rambo1.h::NotifyVramAddressChange` and gates Hard
//!   Drivin's IRQ timing.
//!
//! ## Register surface (`addr & 0xE001`)
//!
//! | Address  | Effect                                                       |
//! |----------|--------------------------------------------------------------|
//! | `$8000`  | Bank-select: bits 0-3 = R index, bit 5 = CHR mode, bit 6 = PRG mode, bit 7 = CHR A12 invert |
//! | `$8001`  | Bank-data: writes to the R selected by the last `$8000`      |
//! | `$A000`  | Mirroring: bit 0 (0=Vertical, 1=Horizontal)                  |
//! | `$C000`  | IRQ counter reload latch                                     |
//! | `$C001`  | IRQ mode + pending reload: bit 0 (0=A12 edge, 1=CPU cycle)   |
//! | `$E000`  | IRQ disable + ack                                            |
//! | `$E001`  | IRQ enable                                                   |
//!
//! ## CHR layout (post-mode + post-inversion)
//!
//! With CHR mode = 0 (legacy MMC3-like, R0/R1 are 2 KB):
//!
//! ```text
//!   $0000-03FF: R0       $1000-13FF: R2
//!   $0400-07FF: R0|1     $1400-17FF: R3
//!   $0800-0BFF: R1       $1800-1BFF: R4
//!   $0C00-0FFF: R1|1     $1C00-1FFF: R5
//! ```
//!
//! With CHR mode = 1 (RAMBO-1 extra: R0/R1 stay 1 KB, R8/R9 split the
//! adjacent 1 KB slots):
//!
//! ```text
//!   $0000-03FF: R0       $1000-13FF: R2
//!   $0400-07FF: R8       $1400-17FF: R3
//!   $0800-0BFF: R1       $1800-1BFF: R4
//!   $0C00-0FFF: R9       $1C00-1FFF: R5
//! ```
//!
//! When the A12-invert bit (`$8000.b7`) is set, `$0000-$0FFF` and
//! `$1000-$1FFF` swap (XOR slot index by 4) - identical to MMC3.
//!
//! ## PRG layout
//!
//! With PRG mode = 0 (legacy MMC3): R6 at `$8000`, R7 at `$A000`, R15
//! at `$C000`, last bank pinned at `$E000`.
//!
//! With PRG mode = 1: R15 at `$8000`, R6 at `$A000`, R7 at `$C000`,
//! last bank pinned at `$E000` (the *Hard Drivin'* layout).
//!
//! ## IRQ
//!
//! Two-mode counter. In **A12 mode** ($C001 bit 0 = 0), the counter
//! ticks on each filtered A12 rising edge - PPU pattern fetches when
//! the BG/sprite tables aren't aliased. In **CPU mode** ($C001 bit 0 =
//! 1), the counter ticks every 4 CPU cycles via a 2-bit divider.
//!
//! The counter reload follows MMC3's pattern: when zero (or after the
//! pending-reload flag is set by a `$C001` write), the counter loads
//! `latch + N` where `N` is 1 if `latch <= 1` else 2 - the *Hard
//! Drivin'* off-by-one fix from FHorse / Mesen2. Counter underflow
//! to zero asserts `/IRQ` after a 1-cycle delay (CPU mode) or 2-cycle
//! delay (A12 mode), matching real silicon's pipelining.
//!
//! Mode-switch *cycle → A12* triggers one extra forced clock the next
//! time the prescaler aligns - the *Skull & Crossbones* fix from
//! Mesen2's `_forceClock` flag. Without it, the 4-CPU prescaler stalls
//! mid-count and the game's fade-in scroll glitches.
//!
//! ## References
//!
//! Wiki: <https://www.nesdev.org/wiki/RAMBO-1>. Mapper structure
//! ported from `~/Git/Mesen2/Core/NES/Mappers/Tengen/Rambo1.h`;
//! IRQ-quirk wording cross-checked against
//! `~/Git/punes/src/core/mappers/mapper_064.c` and Nestopia's
//! `NstBoardTengenRambo1.cpp`. The A12 filter constant (30 PPU dots)
//! is Mesen2's `A12Watcher::UpdateVramAddress<30>`.

use crate::mapper::{Mapper, PpuFetchKind};
use crate::rom::{Cartridge, Mirroring};

const PRG_BANK_8K: usize = 8 * 1024;
const CHR_BANK_1K: usize = 1024;
const PRG_RAM_SIZE: usize = 8 * 1024;

/// PPU dots A12 must stay low between rising edges before the next
/// rise counts toward the IRQ counter. MMC3 uses 10; RAMBO-1 needs 30
/// to gate *Hard Drivin'* correctly per Mesen2.
const A12_FILTER_PPU_CYCLES: u64 = 30;

/// IRQ delay (CPU cycles) between counter-hits-zero and `/IRQ`
/// assertion. CPU-cycle mode is 1 cycle behind; A12 mode 2 cycles.
const CPU_MODE_IRQ_DELAY: u8 = 1;
const A12_MODE_IRQ_DELAY: u8 = 2;

/// CPU-cycle mode prescaler: counter ticks every 4 CPU cycles.
const CPU_MODE_PRESCALE_MASK: u8 = 0x03;

pub struct Rambo1 {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,
    prg_ram: Vec<u8>,

    /// Sixteen bank registers, 8 of which are reachable in either CHR
    /// mode. R0-R5 are CHR; R6, R7, R15 are PRG; R8/R9 are the
    /// "extra" CHR banks that only appear in 1 KB CHR mode.
    bank_regs: [u8; 16],
    /// Last `$8000` write. Holds the R index, plus the three mode
    /// bits (CHR mode, PRG mode, A12 invert).
    bank_select: u8,

    prg_bank_count_8k: usize,
    chr_bank_count_1k: usize,

    mirroring: Mirroring,

    irq_latch: u8,
    irq_counter: u8,
    irq_reload_pending: bool,
    irq_enabled: bool,
    irq_cycle_mode: bool,
    /// `$E001` raises this; `/IRQ` falls only after the per-mode
    /// delay (1 or 2 CPU cycles after the counter underflows).
    irq_pending_delay: u8,
    irq_line: bool,

    /// 2-bit CPU prescaler for cycle mode (ticks 0 → 1 → 2 → 3 → 0).
    /// At each wraparound the IRQ counter advances.
    cpu_prescaler: u8,
    /// One-shot flag set when `$C001` switches from cycle mode to A12
    /// mode - the next prescaler-aligned tick fires anyway. Mesen2's
    /// `_forceClock`. Without it *Skull & Crossbones* glitches.
    force_clock: bool,

    /// Last PPU cycle at which A12 went high → low (or `None` if A12
    /// is currently high / no fall observed yet). On A12 rise we
    /// compare the elapsed dot-count against [`A12_FILTER_PPU_CYCLES`]
    /// to decide whether the rise counts.
    a12_low_since: Option<u64>,

    battery: bool,
    save_dirty: bool,
}

impl Rambo1 {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count_8k = (cart.prg_rom.len() / PRG_BANK_8K).max(1);
        let chr_ram = cart.chr_ram || cart.chr_rom.is_empty();
        let chr = if chr_ram {
            vec![0u8; 8 * 1024]
        } else {
            cart.chr_rom
        };
        let chr_bank_count_1k = (chr.len() / CHR_BANK_1K).max(1);

        let prg_ram_total =
            (cart.prg_ram_size + cart.prg_nvram_size).max(PRG_RAM_SIZE);

        Self {
            prg_rom: cart.prg_rom,
            chr,
            chr_ram,
            prg_ram: vec![0u8; prg_ram_total],
            bank_regs: [0u8; 16],
            bank_select: 0,
            prg_bank_count_8k,
            chr_bank_count_1k,
            mirroring: cart.mirroring,
            irq_latch: 0,
            irq_counter: 0,
            irq_reload_pending: false,
            irq_enabled: false,
            irq_cycle_mode: false,
            irq_pending_delay: 0,
            irq_line: false,
            cpu_prescaler: 0,
            force_clock: false,
            a12_low_since: None,
            battery: cart.battery_backed,
            save_dirty: false,
        }
    }

    fn prg_mode_swap(&self) -> bool {
        (self.bank_select & 0x40) != 0
    }

    fn chr_mode_1k(&self) -> bool {
        (self.bank_select & 0x20) != 0
    }

    fn chr_inverted(&self) -> bool {
        (self.bank_select & 0x80) != 0
    }

    fn last_prg_bank(&self) -> usize {
        self.prg_bank_count_8k.saturating_sub(1)
    }

    /// Resolve `$8000-$FFFF` to an 8 KB PRG bank index.
    fn prg_bank_for(&self, addr: u16) -> usize {
        let r6 = (self.bank_regs[6] & 0x3F) as usize;
        let r7 = (self.bank_regs[7] & 0x3F) as usize;
        let r15 = (self.bank_regs[15] & 0x3F) as usize;
        let last = self.last_prg_bank();
        let bank = if !self.prg_mode_swap() {
            match addr {
                0x8000..=0x9FFF => r6,
                0xA000..=0xBFFF => r7,
                0xC000..=0xDFFF => r15,
                0xE000..=0xFFFF => last,
                _ => 0,
            }
        } else {
            match addr {
                0x8000..=0x9FFF => r15,
                0xA000..=0xBFFF => r6,
                0xC000..=0xDFFF => r7,
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
        let r0 = self.bank_regs[0] as usize;
        let r1 = self.bank_regs[1] as usize;
        let r2 = self.bank_regs[2] as usize;
        let r3 = self.bank_regs[3] as usize;
        let r4 = self.bank_regs[4] as usize;
        let r5 = self.bank_regs[5] as usize;
        let r8 = self.bank_regs[8] as usize;
        let r9 = self.bank_regs[9] as usize;
        // The "low" half is $0000-$0FFF, "high" half is $1000-$1FFF.
        // CHR-A12 inversion swaps them. We compute the slot index in
        // the un-inverted layout, then XOR by 4 if inverted.
        let raw_slot = (addr / 0x400) as usize;
        let slot = if self.chr_inverted() {
            raw_slot ^ 0x04
        } else {
            raw_slot
        };
        let bank = if !self.chr_mode_1k() {
            // Legacy MMC3-style: R0/R1 cover 2 KB each (slots 0+1 and
            // 2+3), R2..R5 cover 1 KB each (slots 4..7).
            match slot {
                0 => r0,
                1 => r0 | 0x01,
                2 => r1,
                3 => r1 | 0x01,
                4 => r2,
                5 => r3,
                6 => r4,
                7 => r5,
                _ => 0,
            }
        } else {
            // 1 KB-everywhere mode: R0/R1 still cover slots 0/2 but
            // their adjacent slots 1/3 come from R8/R9.
            match slot {
                0 => r0,
                1 => r8,
                2 => r1,
                3 => r9,
                4 => r2,
                5 => r3,
                6 => r4,
                7 => r5,
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

    /// Apply the *Hard Drivin'* reload-quirk and decrement the IRQ
    /// counter by one tick. If the post-tick counter is zero and IRQ
    /// is enabled, schedule the per-mode delay before `/IRQ` rises.
    fn clock_irq_counter(&mut self, delay: u8) {
        if self.irq_reload_pending {
            // Per Mesen2: when latch is 0 or 1 we add 1, otherwise 2.
            // This shaves the off-by-one that Hard Drivin's IRQ
            // routine relied on.
            self.irq_counter = if self.irq_latch <= 1 {
                self.irq_latch.wrapping_add(1)
            } else {
                self.irq_latch.wrapping_add(2)
            };
            self.irq_reload_pending = false;
        } else if self.irq_counter == 0 {
            self.irq_counter = self.irq_latch.wrapping_add(1);
        }

        self.irq_counter = self.irq_counter.wrapping_sub(1);

        if self.irq_counter == 0 && self.irq_enabled {
            self.irq_pending_delay = delay;
        }
    }
}

impl Mapper for Rambo1 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => {
                let i = (addr - 0x6000) as usize;
                *self.prg_ram.get(i).unwrap_or(&0)
            }
            0x8000..=0xFFFF => {
                let off = self.map_prg(addr);
                *self.prg_rom.get(off).unwrap_or(&0)
            }
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        match addr {
            0x6000..=0x7FFF => {
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
            0x8000..=0xFFFF => match addr & 0xE001 {
                0x8000 => {
                    self.bank_select = data;
                }
                0x8001 => {
                    let r = (self.bank_select & 0x0F) as usize;
                    self.bank_regs[r] = data;
                }
                0xA000 => {
                    self.mirroring = if (data & 0x01) != 0 {
                        Mirroring::Horizontal
                    } else {
                        Mirroring::Vertical
                    };
                }
                0xC000 => {
                    self.irq_latch = data;
                }
                0xC001 => {
                    let new_cycle_mode = (data & 0x01) != 0;
                    // Mode flip cycle → A12: defer one extra forced
                    // tick so the prescaler doesn't strand mid-count.
                    if self.irq_cycle_mode && !new_cycle_mode {
                        self.force_clock = true;
                    }
                    self.irq_cycle_mode = new_cycle_mode;
                    if self.irq_cycle_mode {
                        self.cpu_prescaler = 0;
                    }
                    self.irq_reload_pending = true;
                }
                0xE000 => {
                    self.irq_enabled = false;
                    self.irq_pending_delay = 0;
                    self.irq_line = false;
                }
                0xE001 => {
                    self.irq_enabled = true;
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr < 0x2000 {
            *self.chr.get(self.map_chr(addr)).unwrap_or(&0)
        } else {
            0
        }
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if !self.chr_ram || addr >= 0x2000 {
            return;
        }
        let off = self.map_chr(addr);
        if let Some(b) = self.chr.get_mut(off) {
            *b = data;
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }

    fn on_cpu_cycle(&mut self) {
        // Drain the pending /IRQ delay first - if the previous cycle
        // armed it, fire now.
        if self.irq_pending_delay > 0 {
            self.irq_pending_delay -= 1;
            if self.irq_pending_delay == 0 && self.irq_enabled {
                self.irq_line = true;
            }
        }

        // CPU-mode counter ticks via a 4-CPU-cycle prescaler. The
        // force-clock flag piggybacks on the same prescaler so the
        // *Skull & Crossbones* mode-flip lands on a clean boundary
        // rather than mid-tick.
        if self.irq_cycle_mode || self.force_clock {
            self.cpu_prescaler = self.cpu_prescaler.wrapping_add(1) & CPU_MODE_PRESCALE_MASK;
            if self.cpu_prescaler == 0 {
                self.clock_irq_counter(CPU_MODE_IRQ_DELAY);
                self.force_clock = false;
            }
        }
    }

    fn on_ppu_addr(&mut self, addr: u16, ppu_cycle: u64, _kind: PpuFetchKind) {
        // RAMBO-1's A12 IRQ counter only ticks in A12 mode - cycle
        // mode entirely ignores the PPU bus.
        if self.irq_cycle_mode {
            return;
        }
        let a12_high = (addr & 0x1000) != 0;
        match (a12_high, self.a12_low_since) {
            (true, Some(low_at)) => {
                let elapsed = ppu_cycle.saturating_sub(low_at);
                if elapsed >= A12_FILTER_PPU_CYCLES {
                    self.clock_irq_counter(A12_MODE_IRQ_DELAY);
                }
                self.a12_low_since = None;
            }
            (true, None) => {
                // Still high or first observation - no rise to count.
            }
            (false, None) => {
                self.a12_low_since = Some(ppu_cycle);
            }
            (false, Some(_)) => {
                // Continuing low; nothing to do.
            }
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

    /// 256 KiB PRG (32 banks of 8 KiB), 64 KiB CHR (64 banks of 1 KiB).
    /// Each PRG bank tagged with its bank index; each CHR bank ditto.
    fn cart() -> Cartridge {
        let prg_banks = 32;
        let mut prg = vec![0u8; prg_banks * PRG_BANK_8K];
        for bank in 0..prg_banks {
            let base = bank * PRG_BANK_8K;
            prg[base..base + PRG_BANK_8K].fill(bank as u8);
        }
        let chr_banks = 64;
        let mut chr = vec![0u8; chr_banks * CHR_BANK_1K];
        for bank in 0..chr_banks {
            let base = bank * CHR_BANK_1K;
            chr[base..base + CHR_BANK_1K].fill(bank as u8);
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: chr,
            chr_ram: false,
            mapper_id: 64,
            submapper: 0,
            mirroring: Mirroring::Vertical,
            battery_backed: false,
            prg_ram_size: 0x2000,
            prg_nvram_size: 0,
            tv_system: TvSystem::Ntsc,
            is_nes2: false,
            prg_chr_crc32: 0,
            db_matched: false,
            fds_data: None,
        }
    }

    #[test]
    fn power_on_layout_pins_last_8k_bank_at_e000() {
        let m = Rambo1::new(cart());
        assert_eq!(m.cpu_peek(0x8000), 0); // R6 = 0
        assert_eq!(m.cpu_peek(0xA000), 0); // R7 = 0
        assert_eq!(m.cpu_peek(0xC000), 0); // R15 = 0
        assert_eq!(m.cpu_peek(0xE000), 31); // last bank
        assert_eq!(m.cpu_peek(0xFFFF), 31);
    }

    #[test]
    fn prg_legacy_mode_routes_r6_r7_r15() {
        let mut m = Rambo1::new(cart());
        // Select R6, write bank index 5.
        m.cpu_write(0x8000, 6);
        m.cpu_write(0x8001, 5);
        // Select R7, bank 10.
        m.cpu_write(0x8000, 7);
        m.cpu_write(0x8001, 10);
        // Select R15, bank 20.
        m.cpu_write(0x8000, 15);
        m.cpu_write(0x8001, 20);
        assert_eq!(m.cpu_peek(0x8000), 5);
        assert_eq!(m.cpu_peek(0xA000), 10);
        assert_eq!(m.cpu_peek(0xC000), 20);
        assert_eq!(m.cpu_peek(0xE000), 31);
    }

    #[test]
    fn prg_mode_swap_moves_r15_to_8000() {
        let mut m = Rambo1::new(cart());
        m.cpu_write(0x8000, 15);
        m.cpu_write(0x8001, 7);
        m.cpu_write(0x8000, 6);
        m.cpu_write(0x8001, 11);
        m.cpu_write(0x8000, 7);
        m.cpu_write(0x8001, 13);
        // Now flip PRG mode.
        m.cpu_write(0x8000, 0x40);
        assert_eq!(m.cpu_peek(0x8000), 7); // R15
        assert_eq!(m.cpu_peek(0xA000), 11); // R6
        assert_eq!(m.cpu_peek(0xC000), 13); // R7
        assert_eq!(m.cpu_peek(0xE000), 31); // last
    }

    #[test]
    fn chr_legacy_mode_pairs_r0_and_r1() {
        let mut m = Rambo1::new(cart());
        m.cpu_write(0x8000, 0);
        m.cpu_write(0x8001, 4); // R0 = 4 → slot 0 = bank 4, slot 1 = 5
        m.cpu_write(0x8000, 1);
        m.cpu_write(0x8001, 8); // R1 = 8 → slot 2 = 8, slot 3 = 9
        m.cpu_write(0x8000, 2);
        m.cpu_write(0x8001, 16); // R2 → slot 4
        m.cpu_write(0x8000, 5);
        m.cpu_write(0x8001, 30); // R5 → slot 7
        assert_eq!(m.ppu_read(0x0000), 4);
        assert_eq!(m.ppu_read(0x0400), 5);
        assert_eq!(m.ppu_read(0x0800), 8);
        assert_eq!(m.ppu_read(0x0C00), 9);
        assert_eq!(m.ppu_read(0x1000), 16);
        assert_eq!(m.ppu_read(0x1C00), 30);
    }

    #[test]
    fn chr_1k_mode_routes_r8_and_r9() {
        let mut m = Rambo1::new(cart());
        m.cpu_write(0x8000, 0);
        m.cpu_write(0x8001, 4);
        m.cpu_write(0x8000, 1);
        m.cpu_write(0x8001, 8);
        m.cpu_write(0x8000, 8);
        m.cpu_write(0x8001, 12);
        m.cpu_write(0x8000, 9);
        m.cpu_write(0x8001, 13);
        // Enable 1 KB-everywhere mode (bit 5 of $8000).
        m.cpu_write(0x8000, 0x20);
        assert_eq!(m.ppu_read(0x0000), 4);
        assert_eq!(m.ppu_read(0x0400), 12); // R8, not R0|1
        assert_eq!(m.ppu_read(0x0800), 8);
        assert_eq!(m.ppu_read(0x0C00), 13); // R9, not R1|1
    }

    #[test]
    fn chr_a12_inversion_swaps_low_and_high_halves() {
        let mut m = Rambo1::new(cart());
        m.cpu_write(0x8000, 0);
        m.cpu_write(0x8001, 4); // R0
        m.cpu_write(0x8000, 2);
        m.cpu_write(0x8001, 16); // R2
        // Without inversion: $0000 = R0 = 4, $1000 = R2 = 16.
        assert_eq!(m.ppu_read(0x0000), 4);
        assert_eq!(m.ppu_read(0x1000), 16);
        // Flip A12 invert.
        m.cpu_write(0x8000, 0x80);
        assert_eq!(m.ppu_read(0x0000), 16); // R2 now low
        assert_eq!(m.ppu_read(0x1000), 4); // R0 now high
    }

    #[test]
    fn a000_decodes_mirroring() {
        let mut m = Rambo1::new(cart());
        m.cpu_write(0xA000, 0);
        assert_eq!(m.mirroring(), Mirroring::Vertical);
        m.cpu_write(0xA000, 1);
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
    }

    #[test]
    fn cpu_mode_irq_fires_after_4_x_count_cycles_plus_delay() {
        let mut m = Rambo1::new(cart());
        // Latch 1 → counter loads to 3 on reload (latch+2). Each tick
        // is 4 CPU cycles; underflow → schedule 1-cycle delay.
        m.cpu_write(0xC000, 1);
        m.cpu_write(0xC001, 0x01); // cycle mode + reload-pending
        m.cpu_write(0xE001, 0); // enable IRQ
        let mut cycles_to_fire = 0;
        for n in 1..=200 {
            m.on_cpu_cycle();
            if m.irq_line() {
                cycles_to_fire = n;
                break;
            }
        }
        assert!(
            cycles_to_fire > 0 && cycles_to_fire < 30,
            "expected IRQ within ~3 ticks (≤24 CPU cycles), fired at {cycles_to_fire}"
        );
    }

    #[test]
    fn e000_disables_and_acks_irq() {
        let mut m = Rambo1::new(cart());
        m.cpu_write(0xC000, 0);
        m.cpu_write(0xC001, 0x01);
        m.cpu_write(0xE001, 0);
        // Run until /IRQ asserts (latch=0 fires near-immediately).
        for _ in 0..50 {
            m.on_cpu_cycle();
            if m.irq_line() {
                break;
            }
        }
        assert!(m.irq_line());
        m.cpu_write(0xE000, 0);
        assert!(!m.irq_line());
        assert!(!m.irq_enabled);
    }

    #[test]
    fn a12_mode_filters_short_low_pulses() {
        let mut m = Rambo1::new(cart());
        // Latch >= 2 so the reload value (latch+2) survives one tick.
        m.cpu_write(0xC000, 5);
        m.cpu_write(0xC001, 0x00); // A12 mode + reload-pending
        m.cpu_write(0xE001, 0);

        // Establish A12 low at cycle 0.
        m.on_ppu_addr(0x0000, 0, PpuFetchKind::BgPattern);
        // Rise too soon (10 dots elapsed, threshold 30) - must not
        // count, so the pending reload should still be unconsumed.
        m.on_ppu_addr(0x1000, 10, PpuFetchKind::BgPattern);
        assert_eq!(m.irq_counter, 0);
        assert!(m.irq_reload_pending);

        // Drop A12 again, wait long enough, then rise - counts.
        // Reload loads counter to latch+2 = 7, then post-decrement to 6.
        m.on_ppu_addr(0x0000, 20, PpuFetchKind::BgPattern);
        m.on_ppu_addr(0x1000, 60, PpuFetchKind::BgPattern);
        assert_eq!(m.irq_counter, 6);
        assert!(!m.irq_reload_pending);
    }
}
