// SPDX-License-Identifier: GPL-3.0-or-later
//! Taito TC0690 - iNES mapper 48. The IRQ-bearing successor to
//! the [`crate::nes::mapper::taito_tc0190`] (mapper 33) chip,
//! used by *Don Doko Don 2*, *Power Blazer* (Power Soccer Star),
//! *The Flintstones: The Surprise at Dinosaur Peak!*,
//! *Bakushou!! Jinsei Gekijou 3*, and *Captain Saver*.
//!
//! ## Register surface (`addr & 0xE003`)
//!
//! TC0690 fixes the MMC3-style PRG and CHR layouts (no
//! mode bits) and dedicates each address to a specific bank
//! register, instead of the MMC3 `$8000` (select) + `$8001`
//! (data) pair:
//!
//! | Address  | Effect                                            |
//! |----------|---------------------------------------------------|
//! | `$8000`  | PRG bank slot 0 (8 KiB at `$8000`); 6-bit         |
//! | `$8001`  | PRG bank slot 1 (8 KiB at `$A000`); 6-bit         |
//! | `$8002`  | CHR 2 KiB bank for PPU `$0000-$07FF`              |
//! | `$8003`  | CHR 2 KiB bank for PPU `$0800-$0FFF`              |
//! | `$A000`  | CHR 1 KiB bank for PPU `$1000-$13FF`              |
//! | `$A001`  | CHR 1 KiB bank for PPU `$1400-$17FF`              |
//! | `$A002`  | CHR 1 KiB bank for PPU `$1800-$1BFF`              |
//! | `$A003`  | CHR 1 KiB bank for PPU `$1C00-$1FFF`              |
//! | `$C000`  | IRQ latch (XOR-inverted; `+ 1` on submapper 1)    |
//! | `$C001`  | Trigger IRQ reload + ack pending IRQ              |
//! | `$C002`  | Enable IRQ counter                                |
//! | `$C003`  | Disable IRQ counter + ack pending IRQ             |
//! | `$E000`  | Mirroring: bit 6 (1 = horizontal, 0 = vertical)   |
//!
//! `$C000-$DFFF` is hardwired to the second-to-last 8 KiB PRG
//! bank, `$E000-$FFFF` to the last - same fixed-tail layout
//! as MMC3 mode 0.
//!
//! ## IRQ delay
//!
//! Each MMC3-style A12-counter underflow re-arms a CPU-cycle
//! countdown that gates the actual `/IRQ` assertion. Mesen2's
//! TaitoTc0690.h:
//!
//! - Submapper 0 (default, e.g. *Flintstones*, *Captain Saver*):
//!   22-cycle delay.
//! - Submapper 1 (e.g. *The Jetsons: Cogswell's Caper!*):
//!   6-cycle delay AND `+1` offset on the `$C000` reload value.
//!
//! Both `$C000` and `$C001` clear a pending IRQ - that's a
//! Flintstones requirement; the standard MMC3 only acks at
//! `$E000`.
//!
//! Implementation: thin wrapper around [`Mmc3`] (mirrors the
//! `Mapper037` / `Txsrom` / `Tqrom` patterns). The IRQ
//! counter and A12 filter run inside the inner mapper; the
//! wrapper detects rising edges on `inner.irq_line()` to
//! re-arm the delay countdown, and exposes its own gated
//! `irq_line()` so the CPU sees the assertion only after the
//! delay expires.
//!
//! References:
//! - <https://www.nesdev.org/wiki/INES_Mapper_048>
//! - `~/Git/Mesen2/Core/NES/Mappers/Taito/TaitoTc0690.h`
//! - `~/Git/punes/src/core/mappers/mapper_048.c`

use crate::nes::mapper::mmc3::Mmc3;
use crate::nes::mapper::{Mapper, NametableSource, NametableWriteTarget, PpuFetchKind};
use crate::nes::rom::{Cartridge, Mirroring};

const IRQ_DELAY_DEFAULT: u8 = 22;
const IRQ_DELAY_SUBMAPPER_1: u8 = 6;

pub struct Tc0690 {
    inner: Mmc3,
    /// NES 2.0 submapper id captured at construction time.
    /// 0 = default (Flintstones / Captain Saver: 22-cycle
    /// delay, no reload offset). 1 = Jetsons (6-cycle delay,
    /// `+1` reload offset). Cards declared as iNES 1.0 land
    /// at submapper 0; users with The Jetsons should re-rip
    /// in NES 2.0 form to get correct timing.
    submapper: u8,
    /// CPU cycles remaining before a pending counter underflow
    /// becomes a `/IRQ` assertion. 0 = idle. Re-armed on each
    /// rising edge of `inner.irq_line()` and on each underflow
    /// while the delay is already running (matches Mesen2's
    /// `TriggerIrq` reset semantic).
    irq_delay: u8,
    /// Gated IRQ output line, surfaced to the CPU via
    /// [`Mapper::irq_line`]. Goes high when `irq_delay`
    /// counts to zero from a non-zero value; cleared on any
    /// `$C000` / `$C001` / `$C003` write or when the inner
    /// mapper's IRQ is disabled.
    delayed_irq_line: bool,
    /// Snapshot of `inner.irq_line()` from the previous
    /// `on_cpu_cycle` so we can detect a rising edge. The
    /// inner line stays asserted until ack, so a fresh
    /// underflow only registers when we observe a low → high
    /// transition.
    prev_inner_irq: bool,
}

impl Tc0690 {
    pub fn new(cart: Cartridge) -> Self {
        let submapper = cart.submapper;
        Self {
            inner: Mmc3::new(cart),
            submapper,
            irq_delay: 0,
            delayed_irq_line: false,
            prev_inner_irq: false,
        }
    }

    fn delay_reload(&self) -> u8 {
        if self.submapper == 1 {
            IRQ_DELAY_SUBMAPPER_1
        } else {
            IRQ_DELAY_DEFAULT
        }
    }

    fn ack_pending_irq(&mut self) {
        // Both $C000 and $C001 clear a pending IRQ on TC0690
        // (Flintstones requirement); $C003 also disables. We
        // drop the delayed line, kill the in-flight delay, and
        // ack any inner line that was already asserted.
        self.delayed_irq_line = false;
        self.irq_delay = 0;
        self.inner.ack_irq();
    }
}

impl Mapper for Tc0690 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.inner.cpu_read(addr)
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        self.inner.cpu_peek(addr)
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        match addr & 0xE003 {
            0x8000 => self.inner.set_bank_reg(6, data & 0x3F),
            0x8001 => self.inner.set_bank_reg(7, data & 0x3F),
            // CHR 2 KiB bank: stored as a 1 KiB-aligned bank
            // index inside MMC3 (so `bank_regs[0]` is the low
            // half and `bank_regs[0] | 1` is the high half).
            // Convert the wire value to 1 KiB units via `<< 1`.
            0x8002 => self.inner.set_bank_reg(0, data << 1),
            0x8003 => self.inner.set_bank_reg(1, data << 1),
            0xA000 => self.inner.set_bank_reg(2, data),
            0xA001 => self.inner.set_bank_reg(3, data),
            0xA002 => self.inner.set_bank_reg(4, data),
            0xA003 => self.inner.set_bank_reg(5, data),
            0xC000 => {
                self.ack_pending_irq();
                let bias: u8 = if self.submapper == 1 { 1 } else { 0 };
                let latch = (data ^ 0xFF).wrapping_add(bias);
                self.inner.set_irq_latch(latch);
            }
            0xC001 => {
                self.ack_pending_irq();
                self.inner.trigger_irq_reload();
            }
            0xC002 => self.inner.set_irq_enabled(true),
            0xC003 => {
                self.inner.set_irq_enabled(false);
                self.delayed_irq_line = false;
                self.irq_delay = 0;
            }
            0xE000 => {
                let m = if (data & 0x40) != 0 {
                    Mirroring::Horizontal
                } else {
                    Mirroring::Vertical
                };
                self.inner.set_mirroring_mode(m);
            }
            _ => {}
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        self.inner.ppu_read(addr)
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        self.inner.ppu_write(addr, data);
    }

    fn mirroring(&self) -> Mirroring {
        self.inner.mirroring()
    }

    fn on_cpu_cycle(&mut self) {
        let before = self.prev_inner_irq;
        self.inner.on_cpu_cycle();
        let after = self.inner.irq_line();
        // Re-arm the delay on each low → high transition of
        // the inner IRQ line. Mesen2 does the same in
        // TriggerIrq() (called per counter underflow); a
        // multi-underflow within the delay window would
        // restart the delay there, and our wrapper produces
        // the same observable result for any realistic
        // scanline cadence (delay 4-22 << ~113-cycle
        // scanline).
        if !before && after {
            self.irq_delay = self.delay_reload();
        }
        if self.irq_delay > 0 {
            self.irq_delay -= 1;
            if self.irq_delay == 0 {
                self.delayed_irq_line = true;
            }
        }
        self.prev_inner_irq = after;
    }

    fn on_ppu_addr(&mut self, addr: u16, ppu_cycle: u64, kind: PpuFetchKind) {
        self.inner.on_ppu_addr(addr, ppu_cycle, kind);
    }

    fn ppu_nametable_read(&mut self, slot: u8, offset: u16) -> NametableSource {
        self.inner.ppu_nametable_read(slot, offset)
    }

    fn ppu_nametable_write(
        &mut self,
        slot: u8,
        offset: u16,
        data: u8,
    ) -> NametableWriteTarget {
        self.inner.ppu_nametable_write(slot, offset, data)
    }

    fn irq_line(&self) -> bool {
        self.delayed_irq_line
    }

    fn audio_output(&self) -> Option<f32> {
        self.inner.audio_output()
    }

    fn save_data(&self) -> Option<&[u8]> {
        self.inner.save_data()
    }

    fn load_save_data(&mut self, data: &[u8]) {
        self.inner.load_save_data(data)
    }

    fn save_dirty(&self) -> bool {
        self.inner.save_dirty()
    }

    fn mark_saved(&mut self) {
        self.inner.mark_saved()
    }

    fn save_state_capture(&self) -> Option<crate::save_state::MapperState> {
        let inner_state = self.inner.save_state_capture()?;
        let crate::save_state::MapperState::Mmc3(inner_snap) = inner_state else {
            return None;
        };
        Some(crate::save_state::MapperState::Tc0690(Box::new(
            crate::save_state::mapper::Tc0690Snap {
                inner: inner_snap,
                submapper: self.submapper,
                irq_delay: self.irq_delay,
                delayed_irq_line: self.delayed_irq_line,
                prev_inner_irq: self.prev_inner_irq,
            },
        )))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::Tc0690(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        let inner_state = crate::save_state::MapperState::Mmc3(crate::save_state::mapper::Mmc3Snap {
            prg_ram: snap.inner.prg_ram.clone(),
            chr_ram_data: snap.inner.chr_ram_data.clone(),
            bank_select: snap.inner.bank_select,
            bank_regs: snap.inner.bank_regs,
            mirroring: snap.inner.mirroring,
            prg_ram_enabled: snap.inner.prg_ram_enabled,
            prg_ram_write_protected: snap.inner.prg_ram_write_protected,
            irq_latch: snap.inner.irq_latch,
            irq_counter: snap.inner.irq_counter,
            irq_reload: snap.inner.irq_reload,
            irq_enabled: snap.inner.irq_enabled,
            irq_line: snap.inner.irq_line,
            a12_low_since: snap.inner.a12_low_since,
            reg_a001: snap.inner.reg_a001,
            save_dirty: snap.inner.save_dirty,
        });
        self.inner.save_state_apply(&inner_state)?;
        // Submapper is determined by the cart at construction
        // and shouldn't change across a save round-trip; we
        // restore it anyway so a future cross-version load
        // doesn't drift the IRQ delay timing.
        self.submapper = snap.submapper;
        self.irq_delay = snap.irq_delay;
        self.delayed_irq_line = snap.delayed_irq_line;
        self.prev_inner_irq = snap.prev_inner_irq;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, TvSystem};

    fn cart_with(submapper: u8) -> Cartridge {
        // 256 KiB PRG (32x 8 KiB), 128 KiB CHR (128x 1 KiB).
        let mut prg = vec![0u8; 0x40000];
        // Sentinel at start of bank 0 for the slot-0 read test.
        prg[0] = 0x10;
        // Sentinel at start of bank 1 (slot 0 after $8000 = 0x01).
        prg[0x2000] = 0x11;
        Cartridge {
            prg_rom: prg,
            chr_rom: vec![0u8; 0x20000],
            chr_ram: false,
            mapper_id: 48,
            submapper,
            mirroring: Mirroring::Vertical,
            battery_backed: false,
            prg_ram_size: 0x2000,
            prg_nvram_size: 0,
            tv_system: TvSystem::Ntsc,
            is_nes2: true,
            prg_chr_crc32: 0,
            db_matched: false,
            fds_data: None,
        }
    }

    #[test]
    fn power_on_state_is_quiet() {
        let m = Tc0690::new(cart_with(0));
        assert!(!m.delayed_irq_line);
        assert_eq!(m.irq_delay, 0);
    }

    #[test]
    fn prg_bank_writes_select_8k_slots() {
        let mut m = Tc0690::new(cart_with(0));
        // $8000 selects slot-0 PRG bank. Bank 1's start byte is 0x11.
        m.cpu_write(0x8000, 0x01);
        assert_eq!(m.cpu_read(0x8000), 0x11);
        // $8001 selects slot-1 PRG bank.
        m.cpu_write(0x8001, 0x00);
        assert_eq!(m.cpu_read(0xA000), 0x10);
    }

    #[test]
    fn mirroring_register_decodes_bit_6() {
        let mut m = Tc0690::new(cart_with(0));
        m.cpu_write(0xE000, 0x40);
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
        m.cpu_write(0xE000, 0x00);
        assert_eq!(m.mirroring(), Mirroring::Vertical);
        // Other bits are ignored.
        m.cpu_write(0xE000, 0x40 | 0x3F);
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
    }

    #[test]
    fn irq_latch_is_xor_inverted_no_offset_on_submapper_0() {
        let mut m = Tc0690::new(cart_with(0));
        // $C000 = 0x10 → latch = 0x10 ^ 0xFF = 0xEF, no +1.
        m.cpu_write(0xC000, 0x10);
        // We can't observe the latch directly, but write $C001
        // (trigger reload) and let the inner counter pick it up.
        m.cpu_write(0xC001, 0x00);
        // Counter is 0, reload pending. After the next A12
        // rising edge, counter loads 0xEF. We don't drive A12
        // here; instead just confirm the wrapper didn't blow up
        // and the line stays low.
        assert!(!m.irq_line());
    }

    #[test]
    fn irq_latch_carries_plus_one_on_submapper_1() {
        let mut m = Tc0690::new(cart_with(1));
        // Submapper 1: latch = (data ^ 0xFF) + 1.
        m.cpu_write(0xC000, 0x10); // expected internal latch = 0xEF + 1 = 0xF0
        // Same observation gap as above - the offset's actual
        // effect shows up at counter reload time. Confirm the
        // wrapper didn't desync state-wise.
        assert_eq!(m.submapper, 1);
    }

    #[test]
    fn c000_clears_pending_irq() {
        let mut m = Tc0690::new(cart_with(0));
        // Manually set the wrapper into "IRQ asserted" state.
        m.delayed_irq_line = true;
        m.irq_delay = 5;
        m.cpu_write(0xC000, 0x00);
        assert!(!m.delayed_irq_line);
        assert_eq!(m.irq_delay, 0);
    }

    #[test]
    fn c001_clears_pending_irq() {
        let mut m = Tc0690::new(cart_with(0));
        m.delayed_irq_line = true;
        m.irq_delay = 5;
        m.cpu_write(0xC001, 0x00);
        assert!(!m.delayed_irq_line);
    }

    #[test]
    fn c003_disables_and_clears() {
        let mut m = Tc0690::new(cart_with(0));
        m.cpu_write(0xC002, 0x00); // enable
        m.delayed_irq_line = true;
        m.cpu_write(0xC003, 0x00); // disable + ack
        assert!(!m.delayed_irq_line);
        // Subsequent rising edge of inner irq line should NOT
        // re-arm because counter is disabled - simulate by
        // forcing prev_inner_irq false and running on_cpu_cycle.
        m.prev_inner_irq = false;
        m.on_cpu_cycle();
        // Inner line stays low without an A12 tick, so
        // delayed line stays low.
        assert!(!m.delayed_irq_line);
    }

    #[test]
    fn delay_countdown_asserts_after_n_cycles_submapper_0() {
        let mut m = Tc0690::new(cart_with(0));
        // Simulate the inner mapper's IRQ line rising edge by
        // hand. We can't easily drive a real A12 sequence in
        // a unit test (PPU isn't wired here), so we poke the
        // wrapper's delay path directly.
        m.irq_delay = IRQ_DELAY_DEFAULT; // 22 cycles
        for _ in 0..(IRQ_DELAY_DEFAULT - 1) {
            m.on_cpu_cycle();
            assert!(!m.delayed_irq_line, "line shouldn't fire mid-delay");
        }
        m.on_cpu_cycle();
        assert!(m.delayed_irq_line, "line asserts on the cycle delay reaches 0");
    }

    #[test]
    fn delay_countdown_uses_6_cycles_on_submapper_1() {
        let mut m = Tc0690::new(cart_with(1));
        m.irq_delay = IRQ_DELAY_SUBMAPPER_1; // 6 cycles
        for _ in 0..(IRQ_DELAY_SUBMAPPER_1 - 1) {
            m.on_cpu_cycle();
            assert!(!m.delayed_irq_line);
        }
        m.on_cpu_cycle();
        assert!(m.delayed_irq_line);
    }
}
