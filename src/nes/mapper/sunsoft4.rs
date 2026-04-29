// SPDX-License-Identifier: GPL-3.0-or-later
//! Sunsoft-4 - iNES mapper 68. Used by *After Burner II*, *After
//! Burner* (JP), *Maharaja* (JP), and *Ripple Island* (JP). The
//! signature feature is **CHR-as-nametable replacement**: bit 4 of
//! `$E000` redirects PPU nametable fetches at `$2000-$2FFF` into
//! CHR-ROM at offsets selected by a pair of 1 KiB bank registers
//! (`$C000` / `$D000`), which lets the cart store backgrounds as
//! "tile-of-tiles" CHR data instead of consuming nametable RAM.
//!
//! ## Register surface (`addr & 0xF000`)
//!
//! | Address  | Effect                                            |
//! |----------|---------------------------------------------------|
//! | `$8000`  | CHR bank 0 (2 KiB at PPU `$0000-$07FF`)           |
//! | `$9000`  | CHR bank 1 (`$0800-$0FFF`)                        |
//! | `$A000`  | CHR bank 2 (`$1000-$17FF`)                        |
//! | `$B000`  | CHR bank 3 (`$1800-$1FFF`)                        |
//! | `$C000`  | NTRAM bank register 0 (1 KiB CHR-ROM page)        |
//! | `$D000`  | NTRAM bank register 1                             |
//! | `$E000`  | bits 0-1: mirroring, bit 4: NTRAM enable          |
//! | `$F000`  | bits 0-3: PRG bank (16 KiB), bit 4: PRG-RAM enable|
//!
//! ## NTRAM (CHR-as-nametable) routing
//!
//! When `$E000.b4` is set, each of the four nametable slots draws
//! its bytes from CHR-ROM at offset `nt_regs[reg] * 0x400`,
//! addressable per slot via the following selector (per Mesen2,
//! cross-checked with puNES `mirroring_fix_068`):
//!
//! - Vertical mirroring   → slots `0/2` use `nt_regs[0]`, `1/3` use `nt_regs[1]`
//! - Horizontal mirroring → slots `0/1` use `nt_regs[0]`, `2/3` use `nt_regs[1]`
//! - Single-screen lower  → all slots use `nt_regs[0]`
//! - Single-screen upper  → all slots use `nt_regs[1]`
//!
//! Mesen2 force-sets bit 7 on the stored register value
//! (`value | 0x80`) - the purpose isn't documented but every
//! reference implementation does it, so we match. Likely a
//! protection-chip artifact that doesn't affect the bank index
//! since we mod by `chr_bank_count_1k` before indexing.
//!
//! ## Sunsoft-Maeda licensing chip (NES 2.0 submapper 1)
//!
//! Standard Sunsoft-4 hardware has only 3 PRG-bank lines, capping
//! commercial carts at 128 KiB. The Sunsoft-Maeda licensing
//! variant adds a 4th bank line via an external chip that gates
//! `$8000-$BFFF` access on a "license check is alive" keep-alive
//! signal. The chip works like this:
//!
//! 1. The game disables PRG-RAM at `$F000.b4`.
//! 2. The game writes to `$6000-$7FFF`. With PRG-RAM gated off the
//!    write would normally drop, but the licensing chip catches it
//!    as a keep-alive ping and arms a ~107520 CPU-cycle (60 ms)
//!    timer.
//! 3. While the timer is armed AND `$F000.b3 = 0` (external bank
//!    select), `$8000-$BFFF` reads from the upper PRG banks
//!    (8..). When `$F000.b3 = 1`, `$8000-$BFFF` reads from the
//!    inner bank set (0..=7) regardless.
//! 4. If the game stops sending keep-alive pings, the timer
//!    expires and `$8000-$BFFF` reads return open bus until the
//!    next ping. Games re-arm via a NMI / IRQ handler that pings
//!    every frame.
//!
//! Carts that depend on this: *Sugoro Quest: Dice no Senshitachi*
//! and a small set of JP-only Sunsoft-published licensing-
//! protected titles. Nesdev / Mesen2 list them under submapper 1.
//! Standard 128-KiB carts (After Burner II, Maharaja, Ripple
//! Island) never trigger any of this because the gate
//! `prg_bank_count > 8` is never true for them.
//!
//! Implementation cross-checked against
//! `~/Git/Mesen2/Core/NES/Mappers/Sunsoft/Sunsoft4.h` (always-on
//! licensing logic) and `~/Git/punes/src/core/mappers/mapper_068.c`
//! (PRG-RAM-disabled-write keep-alive trigger). We follow puNES's
//! tighter gate (only PRG-RAM-disabled writes arm the timer) so a
//! battery-RAM cart that's also mapper-68 doesn't accidentally arm
//! the licensing chip on every save write.
//!
//! References:
//! - <https://www.nesdev.org/wiki/INES_Mapper_068>
//! - `~/Git/Mesen2/Core/NES/Mappers/Sunsoft/Sunsoft4.h`
//! - `~/Git/punes/src/core/mappers/mapper_068.c`

use crate::nes::mapper::{Mapper, NametableSource};
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_16K: usize = 16 * 1024;
const CHR_BANK_2K: usize = 2 * 1024;
const CHR_BANK_1K: usize = 1024;

/// Licensing-chip keep-alive timer reload. ~107520 CPU cycles is
/// the canonical Mesen2 value (`1024 * 105`); games typically ping
/// once per frame (~29830 cycles NTSC), so this gives ~3.6 frames
/// of slack before the chip revokes external ROM access.
const LICENSING_TIMER_RELOAD: u32 = 1024 * 105;

pub struct Sunsoft4 {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,
    prg_ram: Vec<u8>,

    /// Switchable 16 KiB PRG bank at `$8000-$BFFF`. `$C000-$FFFF`
    /// is hardwired to the last bank.
    prg_bank: u8,
    /// `$F000.b4`. When clear, `$6000-$7FFF` reads return open bus
    /// (we surface 0) and writes are dropped.
    prg_ram_enabled: bool,

    /// Four 2 KiB CHR-ROM banks for the PPU pattern windows.
    chr_banks: [u8; 4],
    /// Two 1 KiB CHR-ROM bank registers used for NTRAM mode.
    /// Stored with bit 7 forced set per Mesen2 / puNES.
    nt_regs: [u8; 2],
    /// `$E000.b4`. When set, `ppu_nametable_read` substitutes a
    /// CHR byte for each NT fetch instead of falling through to
    /// CIRAM via the normal mirroring path.
    use_chr_for_nametables: bool,

    prg_bank_count_16k: usize,
    chr_bank_count_2k: usize,
    chr_bank_count_1k: usize,

    mirroring: Mirroring,

    /// Sunsoft-Maeda licensing chip keep-alive countdown in CPU
    /// cycles. Reloaded by writes to `$6000-$7FFF` while PRG-RAM
    /// is disabled (the chip's keep-alive trigger). Decremented
    /// once per CPU cycle in [`Mapper::on_cpu_cycle`]; when it
    /// reaches zero, external-ROM `$8000-$BFFF` reads return open
    /// bus until the next keep-alive ping. Always 0 on power-on
    /// and on every cart with prg_bank_count_16k <= 8.
    licensing_timer: u32,
    /// Latched at `$F000` write time: `bit 3 == 0` AND the cart
    /// has more than 8 PRG-bank lines. While true (and the timer
    /// is armed), `$8000-$BFFF` reads route through `external_page`
    /// instead of `prg_bank`. False forces internal-bank routing
    /// regardless of the timer.
    using_external_rom: bool,
    /// Resolved external-bank index (>= 8). Mesen2's formula:
    /// `0x08 | ((value & 0x07) % (count - 8))`. Cached at write
    /// time so the read path stays branch-light.
    external_page: u8,

    battery: bool,
    save_dirty: bool,
}

impl Sunsoft4 {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count_16k = (cart.prg_rom.len() / PRG_BANK_16K).max(1);
        let chr_ram = cart.chr_ram || cart.chr_rom.is_empty();
        let chr = if chr_ram {
            // No commercial Sunsoft-4 cart ships CHR-RAM, but we
            // keep the path defensive so a homebrew / mis-tagged
            // dump doesn't fault on construction.
            vec![0u8; 8 * 1024]
        } else {
            cart.chr_rom
        };
        let chr_bank_count_2k = (chr.len() / CHR_BANK_2K).max(1);
        let chr_bank_count_1k = (chr.len() / CHR_BANK_1K).max(1);
        let prg_ram_total = (cart.prg_ram_size + cart.prg_nvram_size).max(0x2000);
        Self {
            prg_rom: cart.prg_rom,
            chr,
            chr_ram,
            prg_ram: vec![0u8; prg_ram_total],
            prg_bank: 0,
            prg_ram_enabled: false,
            chr_banks: [0; 4],
            // Mesen2 / puNES initial state: NTRAM regs power on
            // with 0 (the bit-7 force happens on write only).
            nt_regs: [0; 2],
            use_chr_for_nametables: false,
            prg_bank_count_16k,
            chr_bank_count_2k,
            chr_bank_count_1k,
            mirroring: cart.mirroring,
            licensing_timer: 0,
            using_external_rom: false,
            external_page: 0x08,
            battery: cart.battery_backed,
            save_dirty: false,
        }
    }

    /// True when the licensing chip is keeping `$8000-$BFFF`
    /// pointed at external ROM right now: external mode latched,
    /// timer armed, and the cart has enough PRG banks to need
    /// it. Standard 128-KiB carts return false unconditionally.
    fn external_active(&self) -> bool {
        self.using_external_rom && self.licensing_timer > 0 && self.prg_bank_count_16k > 8
    }

    fn prg_index(&self, addr: u16) -> usize {
        let bank = if addr < 0xC000 {
            // Standard chip has 3 PRG-bank lines (low 3 bits of
            // `prg_bank`). Submapper-1 licensing variant routes
            // `$8000-$BFFF` through the external bank when the
            // chip is keeping the keep-alive timer alive.
            let internal = (self.prg_bank as usize & 0x07) % self.prg_bank_count_16k;
            if self.external_active() {
                (self.external_page as usize) % self.prg_bank_count_16k
            } else {
                internal
            }
        } else {
            self.prg_bank_count_16k - 1
        };
        let off = (addr as usize) & (PRG_BANK_16K - 1);
        bank * PRG_BANK_16K + off
    }

    fn chr_index(&self, addr: u16) -> usize {
        let slot = ((addr >> 11) & 0x03) as usize;
        let bank = (self.chr_banks[slot] as usize) % self.chr_bank_count_2k;
        let off = (addr as usize) & (CHR_BANK_2K - 1);
        bank * CHR_BANK_2K + off
    }

    /// Pick which `nt_regs[]` entry to consult for a given
    /// nametable slot (0..=3) under the current mirroring mode.
    /// Matches Mesen2's `UpdateNametables` selector and puNES's
    /// `mirroring_fix_068`.
    fn nt_reg_for_slot(&self, slot: u8) -> u8 {
        match self.mirroring {
            Mirroring::Vertical => slot & 0x01,
            Mirroring::Horizontal => (slot >> 1) & 0x01,
            Mirroring::SingleScreenLower => 0,
            Mirroring::SingleScreenUpper => 1,
            // Sunsoft-4 hardware doesn't drive 4-screen; fall back
            // to vertical so a misconfigured cart doesn't crash.
            Mirroring::FourScreen => slot & 0x01,
        }
    }
}

impl Mapper for Sunsoft4 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => {
                if !self.prg_ram_enabled {
                    return 0;
                }
                let i = (addr - 0x6000) as usize;
                self.prg_ram.get(i).copied().unwrap_or(0)
            }
            // Licensing-chip unmap: when external-ROM mode is
            // latched but the keep-alive timer has expired, the
            // chip detaches `$8000-$BFFF` and reads return open
            // bus (Mesen2's `RemoveCpuMemoryMapping`). `$C000-
            // $FFFF` stays on the last bank regardless because
            // the licensing chip only gates the lower window.
            0x8000..=0xBFFF
                if self.using_external_rom
                    && self.licensing_timer == 0
                    && self.prg_bank_count_16k > 8 =>
            {
                0
            }
            0x8000..=0xFFFF => {
                let idx = self.prg_index(addr);
                self.prg_rom.get(idx).copied().unwrap_or(0)
            }
            _ => 0,
        }
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => {
                if !self.prg_ram_enabled {
                    return 0;
                }
                let i = (addr - 0x6000) as usize;
                self.prg_ram.get(i).copied().unwrap_or(0)
            }
            0x8000..=0xBFFF
                if self.using_external_rom
                    && self.licensing_timer == 0
                    && self.prg_bank_count_16k > 8 =>
            {
                0
            }
            0x8000..=0xFFFF => {
                let idx = self.prg_index(addr);
                self.prg_rom.get(idx).copied().unwrap_or(0)
            }
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        match addr {
            0x6000..=0x7FFF => {
                if self.prg_ram_enabled {
                    let i = (addr - 0x6000) as usize;
                    if let Some(slot) = self.prg_ram.get_mut(i) {
                        if *slot != data {
                            *slot = data;
                            if self.battery {
                                self.save_dirty = true;
                            }
                        }
                    }
                } else if self.prg_bank_count_16k > 8 {
                    // PRG-RAM gated off, write would normally drop.
                    // The Sunsoft-Maeda licensing chip catches it
                    // as a keep-alive ping and rearms the
                    // external-ROM access timer. Standard 128-KiB
                    // submapper-0 carts skip this branch via the
                    // bank-count gate.
                    self.licensing_timer = LICENSING_TIMER_RELOAD;
                }
            }
            0x8000..=0xFFFF => match addr & 0xF000 {
                0x8000 => self.chr_banks[0] = data,
                0x9000 => self.chr_banks[1] = data,
                0xA000 => self.chr_banks[2] = data,
                0xB000 => self.chr_banks[3] = data,
                0xC000 => self.nt_regs[0] = data | 0x80,
                0xD000 => self.nt_regs[1] = data | 0x80,
                0xE000 => {
                    self.mirroring = match data & 0x03 {
                        0 => Mirroring::Vertical,
                        1 => Mirroring::Horizontal,
                        2 => Mirroring::SingleScreenLower,
                        _ => Mirroring::SingleScreenUpper,
                    };
                    self.use_chr_for_nametables = (data & 0x10) != 0;
                }
                0xF000 => {
                    self.prg_bank = data & 0x0F;
                    self.prg_ram_enabled = (data & 0x10) != 0;
                    // Submapper-1 licensing decode: bit 3 clear
                    // selects external bank set, bit 3 set forces
                    // internal. Carts with <= 8 PRG banks never
                    // engage the external path regardless.
                    if self.prg_bank_count_16k > 8 {
                        if (data & 0x08) == 0 {
                            self.using_external_rom = true;
                            let modulus =
                                (self.prg_bank_count_16k - 8).max(1);
                            self.external_page =
                                0x08 | (((data & 0x07) as usize) % modulus) as u8;
                        } else {
                            self.using_external_rom = false;
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr < 0x2000 {
            let idx = self.chr_index(addr);
            self.chr.get(idx).copied().unwrap_or(0)
        } else {
            0
        }
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if !self.chr_ram || addr >= 0x2000 {
            return;
        }
        let idx = self.chr_index(addr);
        if let Some(slot) = self.chr.get_mut(idx) {
            *slot = data;
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }

    fn on_cpu_cycle(&mut self) {
        // Sunsoft-Maeda licensing-chip keep-alive countdown. Non-
        // licensing carts never arm the timer (the cpu_write gate
        // requires `prg_bank_count_16k > 8`), so this is a single
        // predicated decrement on the hot path - cheap.
        if self.licensing_timer > 0 {
            self.licensing_timer -= 1;
        }
    }

    fn ppu_nametable_read(&mut self, slot: u8, offset: u16) -> NametableSource {
        if !self.use_chr_for_nametables {
            return NametableSource::Default;
        }
        let reg_idx = self.nt_reg_for_slot(slot) as usize;
        // Strip the bit-7 forced-set per Mesen2 before using as
        // bank index. The bit doesn't affect bank math (we mod by
        // count anyway), but keeping the strip explicit makes the
        // formula match the referenced source.
        let bank = (self.nt_regs[reg_idx] & 0x7F) as usize;
        let bank = bank % self.chr_bank_count_1k;
        let chr_off = bank * CHR_BANK_1K + (offset as usize & (CHR_BANK_1K - 1));
        let byte = self.chr.get(chr_off).copied().unwrap_or(0);
        NametableSource::Byte(byte)
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

    fn save_state_capture(&self) -> Option<crate::save_state::MapperState> {
        use crate::save_state::mapper::{MirroringSnap, Sunsoft4Snap};
        Some(crate::save_state::MapperState::Sunsoft4(Sunsoft4Snap {
            prg_ram: self.prg_ram.clone(),
            chr_ram_data: if self.chr_ram { self.chr.clone() } else { Vec::new() },
            prg_bank: self.prg_bank,
            prg_ram_enabled: self.prg_ram_enabled,
            chr_banks: self.chr_banks,
            nt_regs: self.nt_regs,
            use_chr_for_nametables: self.use_chr_for_nametables,
            mirroring: MirroringSnap::from_live(self.mirroring),
            licensing_timer: self.licensing_timer,
            using_external_rom: self.using_external_rom,
            external_page: self.external_page,
            save_dirty: self.save_dirty,
        }))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::Sunsoft4(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        if snap.prg_ram.len() == self.prg_ram.len() {
            self.prg_ram.copy_from_slice(&snap.prg_ram);
        }
        if self.chr_ram && snap.chr_ram_data.len() == self.chr.len() {
            self.chr.copy_from_slice(&snap.chr_ram_data);
        }
        self.prg_bank = snap.prg_bank;
        self.prg_ram_enabled = snap.prg_ram_enabled;
        self.chr_banks = snap.chr_banks;
        self.nt_regs = snap.nt_regs;
        self.use_chr_for_nametables = snap.use_chr_for_nametables;
        self.mirroring = snap.mirroring.to_live();
        self.licensing_timer = snap.licensing_timer;
        self.using_external_rom = snap.using_external_rom;
        self.external_page = snap.external_page;
        self.save_dirty = snap.save_dirty;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, TvSystem};

    fn cart() -> Cartridge {
        Cartridge {
            prg_rom: vec![0u8; 0x20000], // 128 KiB → 8× 16 KiB banks
            chr_rom: vec![0u8; 0x20000], // 128 KiB → 64× 2 KiB / 128× 1 KiB
            chr_ram: false,
            mapper_id: 68,
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
    fn power_on_state_is_safe_default() {
        let m = Sunsoft4::new(cart());
        assert_eq!(m.prg_bank, 0);
        assert!(!m.use_chr_for_nametables);
        assert!(!m.prg_ram_enabled);
    }

    #[test]
    fn nt_register_writes_force_bit7() {
        let mut m = Sunsoft4::new(cart());
        m.cpu_write(0xC000, 0x12);
        m.cpu_write(0xD000, 0x05);
        assert_eq!(m.nt_regs[0], 0x92);
        assert_eq!(m.nt_regs[1], 0x85);
    }

    #[test]
    fn e000_decodes_mirror_and_ntram_enable() {
        let mut m = Sunsoft4::new(cart());
        m.cpu_write(0xE000, 0x10); // NTRAM on, mirror = vertical
        assert!(m.use_chr_for_nametables);
        assert_eq!(m.mirroring, Mirroring::Vertical);
        m.cpu_write(0xE000, 0x01); // NTRAM off, mirror = horizontal
        assert!(!m.use_chr_for_nametables);
        assert_eq!(m.mirroring, Mirroring::Horizontal);
    }

    #[test]
    fn f000_gates_prg_ram_and_selects_bank() {
        let mut m = Sunsoft4::new(cart());
        // PRG-RAM gated off → reads return 0, writes drop.
        m.cpu_write(0xF000, 0x00);
        m.cpu_write(0x6000, 0xAB);
        assert_eq!(m.cpu_read(0x6000), 0);
        // Enable, write again, observe persistence.
        m.cpu_write(0xF000, 0x10);
        m.cpu_write(0x6000, 0xAB);
        assert_eq!(m.cpu_read(0x6000), 0xAB);
        // Bank index honors low 4 bits.
        m.cpu_write(0xF000, 0x13);
        assert_eq!(m.prg_bank, 0x03);
    }

    #[test]
    fn nametable_routing_picks_correct_register_per_mirror() {
        let mut m = Sunsoft4::new(cart());
        // After Burner-style: NTRAM on, vertical, distinct regs.
        m.cpu_write(0xC000, 0x00); // nt_regs[0] = 0x80, low bank index = 0
        m.cpu_write(0xD000, 0x01); // nt_regs[1] = 0x81, low bank index = 1
        m.cpu_write(0xE000, 0x10); // NTRAM on, vertical
        // Slot 0 → reg 0 → bank 0.
        // Slot 1 → reg 1 → bank 1.
        // We populate distinguishable sentinel bytes at the start
        // of each 1 KiB bank, then verify the routing.
        m.chr[0] = 0xAA;
        m.chr[CHR_BANK_1K] = 0xBB;
        match m.ppu_nametable_read(0, 0) {
            NametableSource::Byte(b) => assert_eq!(b, 0xAA),
            other => panic!("expected Byte, got {other:?}"),
        }
        match m.ppu_nametable_read(1, 0) {
            NametableSource::Byte(b) => assert_eq!(b, 0xBB),
            other => panic!("expected Byte, got {other:?}"),
        }
    }

    #[test]
    fn nametable_routing_off_returns_default() {
        let mut m = Sunsoft4::new(cart());
        m.cpu_write(0xE000, 0x01); // NTRAM disabled, mirror horizontal
        assert_eq!(
            m.ppu_nametable_read(0, 0),
            NametableSource::Default,
        );
    }

    /// 256-KiB cart for licensing tests. Submapper 1 carts
    /// (Sugoro Quest etc.) need 16 PRG banks; the licensing
    /// timer's `prg_bank_count_16k > 8` gate becomes true here.
    fn licensing_cart() -> Cartridge {
        let mut prg = vec![0u8; 0x40000]; // 256 KiB → 16x 16 KiB banks
        // Plant a sentinel at the top of bank 8 (first external
        // bank) so we can verify external mapping reads it.
        prg[8 * PRG_BANK_16K] = 0xE8;
        // Plant a different sentinel at the top of bank 0 (first
        // internal bank) for the no-external-ROM case.
        prg[0] = 0x10;
        Cartridge {
            prg_rom: prg,
            chr_rom: vec![0u8; 0x20000],
            chr_ram: false,
            mapper_id: 68,
            submapper: 1,
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
    fn licensing_chip_inert_on_128kib_carts() {
        // Standard cart: no external ROM, no timer, ever.
        let mut m = Sunsoft4::new(cart()); // 128 KiB → 8 banks
        // Disable PRG-RAM and write to $6000-$7FFF. On a
        // submapper-1 cart this would arm the timer; here it
        // must stay zero.
        m.cpu_write(0xF000, 0x00); // bit 4 = 0 → PRG-RAM disabled
        m.cpu_write(0x6000, 0x55);
        assert_eq!(m.licensing_timer, 0);
        assert!(!m.using_external_rom);
    }

    #[test]
    fn keep_alive_arms_timer_only_when_prg_ram_disabled() {
        let mut m = Sunsoft4::new(licensing_cart());
        // PRG-RAM enabled: write goes to RAM, timer doesn't arm.
        m.cpu_write(0xF000, 0x10);
        m.cpu_write(0x6000, 0x42);
        assert_eq!(m.licensing_timer, 0);
        assert_eq!(m.cpu_read(0x6000), 0x42);
        // Disable PRG-RAM, write again: that's the keep-alive.
        m.cpu_write(0xF000, 0x00);
        m.cpu_write(0x6000, 0xFF);
        assert_eq!(m.licensing_timer, LICENSING_TIMER_RELOAD);
    }

    #[test]
    fn external_rom_bit_clear_routes_to_upper_banks_when_armed() {
        let mut m = Sunsoft4::new(licensing_cart());
        // Arm the keep-alive timer first.
        m.cpu_write(0xF000, 0x00); // PRG-RAM off
        m.cpu_write(0x6000, 0x01); // arm timer
        // Now select external mode: bit 3 = 0, low 3 bits = 0
        // → external_page = 0x08 | 0 = 8.
        m.cpu_write(0xF000, 0x00);
        assert!(m.using_external_rom);
        assert_eq!(m.external_page, 0x08);
        // Read the sentinel at the top of bank 8.
        assert_eq!(m.cpu_read(0x8000), 0xE8);
    }

    #[test]
    fn external_rom_bit_set_forces_internal() {
        let mut m = Sunsoft4::new(licensing_cart());
        // Arm timer.
        m.cpu_write(0xF000, 0x00);
        m.cpu_write(0x6000, 0x01);
        // bit 3 set → internal mode despite armed timer.
        m.cpu_write(0xF000, 0x08);
        assert!(!m.using_external_rom);
        // Read returns internal-bank-0 sentinel.
        assert_eq!(m.cpu_read(0x8000), 0x10);
    }

    #[test]
    fn timer_expiry_unmaps_8000_bfff_to_open_bus() {
        let mut m = Sunsoft4::new(licensing_cart());
        // Arm + select external.
        m.cpu_write(0xF000, 0x00);
        m.cpu_write(0x6000, 0x01);
        m.cpu_write(0xF000, 0x00);
        assert_eq!(m.cpu_read(0x8000), 0xE8);
        // Tick the timer to zero. Cheap: reload is ~107k cycles
        // and on_cpu_cycle is a single decrement.
        for _ in 0..LICENSING_TIMER_RELOAD {
            m.on_cpu_cycle();
        }
        assert_eq!(m.licensing_timer, 0);
        // $8000-$BFFF returns open bus (we surface 0).
        assert_eq!(m.cpu_read(0x8000), 0);
        assert_eq!(m.cpu_read(0xBFFF), 0);
        // $C000-$FFFF still maps to the last bank (unaffected).
        // The last bank's last byte is index 0x3FFFF; we didn't
        // plant anything there so it's 0 too, but the read
        // doesn't go through the open-bus branch.
        let _ = m.cpu_read(0xFFFF);
    }
}
