// SPDX-License-Identifier: GPL-3.0-or-later
//! TQROM - iNES mapper 119. Nintendo's MMC3 derivative that
//! mixes 8 KiB of on-cart CHR-RAM with the cart's CHR-ROM,
//! selectable per 1 KiB CHR slot via bit 6 of each CHR bank
//! register value.
//!
//! ## CHR-RAM / ROM mix mechanism
//!
//! For each PPU CHR fetch, the mapper resolves which 1 KiB
//! bank register controls the slot using the standard MMC3
//! routing (`R0`/`R1` 2 KiB at the low or high half depending
//! on `$8000.b7`, `R2`-`R5` 1 KiB at the other half). Then it
//! inspects the **raw** register value's bits 6 and 7:
//!
//! - `value` in `0x40..=0x7F` (bit 6 set, bit 7 clear) → CHR-RAM
//!   bank, indexed by `value & 0x07` (8 KiB total = 8 banks).
//! - any other value → CHR-ROM, looked up via the standard
//!   `chr_bank_for(addr) % chr_bank_count_1k` resolution.
//!
//! For `R0`/`R1` (2 KiB banks), MMC3 internally pairs the
//! register value with `value | 0x01` to address the second
//! 1 KiB half. Bit 6 stays the same across the pair (since
//! `0x40 | 0x01 = 0x41`, still in `0x40..=0x7F`), so a 2 KiB
//! bank either lands fully in RAM or fully in ROM - no
//! mid-pair mode flips. The two halves get distinct RAM banks
//! when in RAM mode (`bank` and `bank | 1`).
//!
//! ## PPU writes
//!
//! Writes to RAM-selected slots commit to `chr_ram`; writes to
//! ROM-selected slots silently drop. Standard MMC3 behavior
//! for an "all CHR-ROM" cart - we just gate the commit on the
//! current bit-6 state of the register that owns the slot.
//!
//! ## Everything else
//!
//! Verbatim MMC3: PRG layout, `$A000` mirroring, `$A001`
//! PRG-RAM gates, IRQ counter (Rev B with the 10-PPU-cycle
//! A12 filter). Implemented as a thin wrapper around our
//! existing [`Mmc3`] (mirrors the `Mapper037` / `Txsrom`
//! patterns).
//!
//! ## Carts
//!
//! Williams-published Nintendo titles: *High Speed*, *Pin\*Bot*,
//! *Mall Madness*. These use the CHR-RAM half for dynamic
//! tile-swap effects (the pinball ramp-lit animations,
//! Mall Madness map updates).
//!
//! References:
//! - <https://www.nesdev.org/wiki/INES_Mapper_119>
//! - `~/Git/Mesen2/Core/NES/Mappers/Mmc3Variants/MMC3_ChrRam.h`
//!   (constructed with `(0x40, 0x7F, 8)` for mapper 119 in
//!   `MapperFactory.cpp`)
//! - `~/Git/punes/src/core/mappers/mapper_119.c`

use crate::nes::mapper::mmc3::Mmc3;
use crate::nes::mapper::{Mapper, NametableSource, NametableWriteTarget, PpuFetchKind};
use crate::nes::rom::{Cartridge, Mirroring};

const CHR_BANK_1K: usize = 1024;
const CHR_RAM_SIZE: usize = 8 * 1024;

pub struct Tqrom {
    inner: Mmc3,
    /// 8 KiB of on-cart CHR-RAM. Indexed in 1 KiB banks via
    /// `(reg_value & 0x07)` when the register's bit 6 is set
    /// and bit 7 is clear (value in `0x40..=0x7F`).
    chr_ram: [u8; CHR_RAM_SIZE],
}

impl Tqrom {
    pub fn new(cart: Cartridge) -> Self {
        Self {
            inner: Mmc3::new(cart),
            chr_ram: [0; CHR_RAM_SIZE],
        }
    }

    /// Resolve a CHR address to (is_ram, byte_index).
    /// `is_ram = true` means index into `self.chr_ram`;
    /// `is_ram = false` means index into `inner.chr()` after
    /// the standard MMC3 mod-by-bank-count.
    fn resolve_chr(&self, addr: u16) -> (bool, usize) {
        let raw = self.inner.chr_bank_raw(addr);
        let off = (addr as usize) & (CHR_BANK_1K - 1);
        // Mesen2's MMC3_ChrRam(0x40, 0x7F, 8) constructor: the
        // RAM selector is the value range `0x40..=0x7F`. Bit 7
        // set with bit 6 set falls outside the range and is
        // treated as CHR-ROM (no commercial cart writes those
        // values, but the gate keeps the behavior aligned with
        // Mesen2).
        if (0x40..=0x7F).contains(&raw) {
            let ram_bank = (raw as usize) & 0x07;
            (true, ram_bank * CHR_BANK_1K + off)
        } else {
            let rom_bank = self.inner.chr_bank_for(addr);
            (false, rom_bank * CHR_BANK_1K + off)
        }
    }
}

impl Mapper for Tqrom {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.inner.cpu_read(addr)
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        self.inner.cpu_peek(addr)
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        self.inner.cpu_write(addr, data);
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr >= 0x2000 {
            return 0;
        }
        let (is_ram, idx) = self.resolve_chr(addr);
        if is_ram {
            self.chr_ram.get(idx).copied().unwrap_or(0)
        } else {
            self.inner.chr().get(idx).copied().unwrap_or(0)
        }
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if addr >= 0x2000 {
            return;
        }
        let (is_ram, idx) = self.resolve_chr(addr);
        if is_ram {
            if let Some(slot) = self.chr_ram.get_mut(idx) {
                *slot = data;
            }
        }
        // ROM-selected slots: silently drop. Standard MMC3 behavior
        // for a CHR-ROM-only slot.
    }

    fn mirroring(&self) -> Mirroring {
        self.inner.mirroring()
    }

    fn on_cpu_cycle(&mut self) {
        self.inner.on_cpu_cycle();
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
        self.inner.irq_line()
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
        Some(crate::save_state::MapperState::Tqrom(Box::new(
            crate::save_state::mapper::TqromSnap {
                inner: inner_snap,
                chr_ram: self.chr_ram,
            },
        )))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::Tqrom(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        // Repackage borrowed inner snap for inner Mmc3's apply.
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
        self.chr_ram = snap.chr_ram;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, TvSystem};

    fn cart() -> Cartridge {
        // 256 KiB PRG (32x 8 KiB), 64 KiB CHR-ROM (64x 1 KiB).
        // CHR-RAM is the cart-internal 8 KiB always present on TQROM.
        let mut prg = vec![0u8; 0x40000];
        prg[0] = 0xAA; // PRG sentinel
        let mut chr = vec![0u8; 0x10000];
        // Plant sentinels so we can verify ROM read paths.
        chr[0] = 0x10; // start of bank 0
        chr[CHR_BANK_1K] = 0x11; // start of bank 1
        Cartridge {
            prg_rom: prg,
            chr_rom: chr,
            chr_ram: false,
            mapper_id: 119,
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

    fn write_bank(m: &mut Tqrom, select: u8, data: u8) {
        m.cpu_write(0x8000, select);
        m.cpu_write(0x8001, data);
    }

    #[test]
    fn power_on_chr_ram_zeroed() {
        let m = Tqrom::new(cart());
        assert!(m.chr_ram.iter().all(|&b| b == 0));
    }

    #[test]
    fn rom_bank_value_routes_through_chr_rom() {
        // CHR mode 0 ($8000.b7 = 0): R2 (1 KiB) maps to PPU
        // $1000-$13FF. Write R2 = 0x01 → ROM bank 1 → sentinel
        // 0x11 at $1000.
        let mut m = Tqrom::new(cart());
        write_bank(&mut m, 0x02, 0x01); // mode 0, R2, ROM bank 1
        assert_eq!(m.ppu_read(0x1000), 0x11);
    }

    #[test]
    fn ram_bank_value_routes_through_chr_ram() {
        let mut m = Tqrom::new(cart());
        // Pre-populate RAM bank 3 with a sentinel.
        m.chr_ram[3 * CHR_BANK_1K] = 0x77;
        // CHR-mode 1: R2 maps to PPU $0000.
        write_bank(&mut m, 0x82, 0x43); // 0x43 in [0x40, 0x7F] → RAM, bank 3
        assert_eq!(m.ppu_read(0x0000), 0x77);
    }

    #[test]
    fn ppu_write_to_ram_slot_commits() {
        let mut m = Tqrom::new(cart());
        write_bank(&mut m, 0x82, 0x42); // RAM, bank 2 at PPU $0000
        m.ppu_write(0x0000, 0xCD);
        assert_eq!(m.chr_ram[2 * CHR_BANK_1K], 0xCD);
        // Verify the read path picks up the same byte.
        assert_eq!(m.ppu_read(0x0000), 0xCD);
    }

    #[test]
    fn ppu_write_to_rom_slot_drops() {
        let mut m = Tqrom::new(cart());
        write_bank(&mut m, 0x82, 0x01); // ROM bank 1 at PPU $0000
        // CHR-RAM stays clean; the ROM is unwritten.
        m.ppu_write(0x0000, 0xCD);
        assert!(m.chr_ram.iter().all(|&b| b == 0));
        assert_eq!(m.ppu_read(0x0000), 0x11); // ROM sentinel intact
    }

    #[test]
    fn r0_2k_pair_both_halves_route_to_ram_with_distinct_banks() {
        let mut m = Tqrom::new(cart());
        // CHR-mode 0: R0 (2 KiB) at PPU $0000-$07FF.
        // 0x42 in RAM range → bank 2 for low half, bank 3 for high half.
        // (Internally MMC3 pairs r0 with r0 | 0x01 = 0x43, still in RAM range.)
        m.chr_ram[2 * CHR_BANK_1K] = 0x22; // bank 2 sentinel
        m.chr_ram[3 * CHR_BANK_1K] = 0x33; // bank 3 sentinel
        write_bank(&mut m, 0x00, 0x42); // R0, RAM bank 2 (paired with 3)
        assert_eq!(m.ppu_read(0x0000), 0x22);
        assert_eq!(m.ppu_read(0x0400), 0x33);
    }

    #[test]
    fn rom_and_ram_slots_can_coexist() {
        let mut m = Tqrom::new(cart());
        // CHR-mode 1: R2-R5 at PPU $0000-$0FFF.
        // Mix: R2 = ROM bank 1, R3 = RAM bank 4, R4 = ROM bank 0, R5 = RAM bank 5.
        m.chr_ram[4 * CHR_BANK_1K] = 0x44;
        m.chr_ram[5 * CHR_BANK_1K] = 0x55;
        write_bank(&mut m, 0x82, 0x01); // R2 = ROM 1
        write_bank(&mut m, 0x83, 0x44); // R3 = RAM 4
        write_bank(&mut m, 0x84, 0x00); // R4 = ROM 0
        write_bank(&mut m, 0x85, 0x45); // R5 = RAM 5
        assert_eq!(m.ppu_read(0x0000), 0x11); // R2 ROM bank 1
        assert_eq!(m.ppu_read(0x0400), 0x44); // R3 RAM bank 4
        assert_eq!(m.ppu_read(0x0800), 0x10); // R4 ROM bank 0
        assert_eq!(m.ppu_read(0x0C00), 0x55); // R5 RAM bank 5
    }

    #[test]
    fn switching_bank_value_flips_rom_ram_for_same_slot() {
        let mut m = Tqrom::new(cart());
        write_bank(&mut m, 0x82, 0x42); // RAM bank 2
        m.chr_ram[2 * CHR_BANK_1K] = 0x99;
        assert_eq!(m.ppu_read(0x0000), 0x99);
        // Same R2, but now ROM bank 0 (sentinel 0x10).
        write_bank(&mut m, 0x82, 0x00);
        assert_eq!(m.ppu_read(0x0000), 0x10);
        // And back to RAM, same bank: byte we wrote earlier is still there.
        write_bank(&mut m, 0x82, 0x42);
        assert_eq!(m.ppu_read(0x0000), 0x99);
    }

    #[test]
    fn high_bit_value_falls_back_to_rom() {
        let mut m = Tqrom::new(cart());
        // 0xC1 has bit 6 set but ALSO bit 7 - outside the [0x40, 0x7F]
        // range. Mesen2's gate treats this as ROM. No real cart writes
        // this, but the gate matters for fuzz / corruption robustness.
        // CHR mode 0: R2 maps to $1000-$13FF.
        write_bank(&mut m, 0x02, 0xC1); // mode 0, R2, value 0xC1
        // 0xC1 mod chr_bank_count_1k (64) = 0x01 → sentinel 0x11.
        assert_eq!(m.ppu_read(0x1000), 0x11);
    }
}
