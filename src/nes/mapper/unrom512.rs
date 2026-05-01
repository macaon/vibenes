// SPDX-License-Identifier: GPL-3.0-or-later
//! UNROM-512 (iNES mapper 30) - Sealie Computing / RetroUSB.
//!
//! Modern commercial-but-unlicensed board sold by Sealie (formerly
//! RetroUSB) since the early 2010s. Used by virtually every
//! commercially-released NES homebrew title: Battle Kid 1+2 (Sivak),
//! Lizard (Brad Smith), Halcyon, From Below, Tower of Turmoil, Owlia,
//! Project Blue, Multidude, Haunted Halloween '85/'86 (mapper 111
//! sister board), and dozens more.
//!
//! Single 8-bit latch in `$8000-$FFFF` (post-bus-conflict on
//! submappers 0 and 2):
//!
//! ```text
//! 7  bit  0
//! ---- ----
//! M CC PPPPP
//! | || |||||
//! | || +++++- 16 KiB PRG-ROM bank at $8000-$BFFF (5 bits, 32 banks max)
//! | ++------- 8 KiB CHR-RAM bank at PPU $0000-$1FFF (4 banks of 8 KiB)
//! +---------- Mirroring control (sub 3, or single-screen header variant)
//! ```
//!
//! - PRG window split: switchable 16 KiB at `$8000-$BFFF`, fixed
//!   last 16 KiB at `$C000-$FFFF`.
//! - CHR is always 32 KiB on-cart RAM (the chip ID UNROM-**512**
//!   refers to the 512 KiB max PRG; CHR-RAM size is independent
//!   and hardwired by Sealie's PCB).
//! - Submapper variants:
//!   - 0: hardwired mirroring per iNES header, with bus conflicts.
//!   - 1: SST39SF040 flash chip - PRG can be reprogrammed in-cart
//!     (writes to `$8000-$BFFF` send commands to flash, `$C000-$FFFF`
//!     still selects PRG bank). No bus conflicts. **Flash chip is
//!     not modeled here:** bank-switching works, programming writes
//!     are silently dropped, and battery save-to-flash is a no-op.
//!   - 2: bus conflicts always (no flash).
//!   - 3: D7 of the latch toggles vertical/horizontal mirroring at
//!     runtime (Sealie's UNROM-512 V/H board variant).
//!   - 4: LED register lives at `$8000-$BFFF`; we ignore those
//!     writes (matches Mesen2). Bank-select still works via
//!     `$C000-$FFFF`.
//!
//! The 4-screen InfiniteNesLives variant additionally maps the
//! upper 8 KiB of CHR-RAM to PPU `$2000-$3EFF` as nametable RAM;
//! we leave `Mirroring::FourScreen` in place (the PPU currently
//! interprets it as vertical mirroring) - the same compromise
//! mapper 77 makes for now.
//!
//! Clean-room references (behavioral only):
//! - `~/Git/Mesen2/Core/NES/Mappers/Homebrew/UnRom512.h`
//! - `~/Git/punes/src/core/mappers/mapper_030.c`
//! - nesdev.org/wiki/INES_Mapper_030

use crate::nes::mapper::Mapper;
use crate::nes::rom::{Cartridge, Mirroring};

const PRG_BANK_16K: usize = 16 * 1024;
const CHR_BANK_8K: usize = 8 * 1024;
const CHR_RAM_TOTAL: usize = 32 * 1024;

pub struct UnRom512 {
    prg_rom: Vec<u8>,
    chr_ram: Vec<u8>,
    submapper: u8,
    mirroring: Mirroring,
    /// True when D7 of the latch toggles mirroring (sub 3 always;
    /// extended in Mesen for header single-screen variants - we
    /// follow the conservative sub-3-only path until we surface
    /// the raw header bits to the mapper).
    enable_mirroring_bit: bool,

    prg_bank_count_16k: usize,

    /// Latch byte. Bits 0-4 = PRG, bits 5-6 = CHR, bit 7 = mirror.
    /// Stored as the post-bus-conflict value when applicable.
    reg: u8,
}

impl UnRom512 {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count_16k = (cart.prg_rom.len() / PRG_BANK_16K).max(1);
        let chr_ram = vec![0u8; CHR_RAM_TOTAL];
        let submapper = cart.submapper;

        let (mirroring, enable_mirroring_bit) = if submapper == 3 {
            // Sub 3 starts in vertical and toggles V/H via D7.
            (Mirroring::Vertical, true)
        } else {
            (cart.mirroring, false)
        };

        Self {
            prg_rom: cart.prg_rom,
            chr_ram,
            submapper,
            mirroring,
            enable_mirroring_bit,
            prg_bank_count_16k,
            reg: 0,
        }
    }

    fn prg_bank(&self) -> usize {
        ((self.reg & 0x1F) as usize) % self.prg_bank_count_16k
    }

    fn chr_bank(&self) -> usize {
        ((self.reg >> 5) & 0x03) as usize
    }

    fn map_prg(&self, addr: u16) -> usize {
        match addr {
            0x8000..=0xBFFF => {
                self.prg_bank() * PRG_BANK_16K + ((addr - 0x8000) as usize)
            }
            0xC000..=0xFFFF => {
                let last = self.prg_bank_count_16k.saturating_sub(1);
                last * PRG_BANK_16K + ((addr - 0xC000) as usize)
            }
            _ => 0,
        }
    }

    fn map_chr(&self, addr: u16) -> usize {
        self.chr_bank() * CHR_BANK_8K + (addr as usize)
    }

    /// Per Mesen: bus conflicts on `(sub == 0 && !battery) || sub == 2`.
    /// We don't model the flash battery, so sub 0 always counts as
    /// "no battery" here.
    fn has_bus_conflict(&self) -> bool {
        matches!(self.submapper, 0 | 2)
    }

    /// True for writes that should land in flash rather than the
    /// bank-select latch: sub 1 with addr in `$8000-$BFFF`. Mesen's
    /// `WriteRegister` routes those to `_flash->Write`; we drop them
    /// since the SST39SF040 isn't modeled. Latch writes via
    /// `$C000-$FFFF` still update bank selection so games that probe
    /// flash but rely on bank-switching elsewhere keep working.
    fn write_targets_flash(&self, addr: u16) -> bool {
        self.submapper == 1 && addr < 0xC000
    }

    /// True for writes routed to the LED register on sub 4
    /// (`$8000-$BFFF`). Mesen removes these from the register
    /// range entirely; we explicitly drop them.
    fn write_targets_led(&self, addr: u16) -> bool {
        self.submapper == 4 && addr < 0xC000
    }
}

impl Mapper for UnRom512 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
            0x8000..=0xFFFF => {
                let i = self.map_prg(addr);
                *self.prg_rom.get(i).unwrap_or(&0)
            }
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        if !(0x8000..=0xFFFF).contains(&addr) {
            return;
        }
        if self.write_targets_flash(addr) || self.write_targets_led(addr) {
            return;
        }

        let value = if self.has_bus_conflict() {
            let i = self.map_prg(addr);
            let rom_byte = *self.prg_rom.get(i).unwrap_or(&0xFF);
            data & rom_byte
        } else {
            data
        };

        self.reg = value;

        if self.enable_mirroring_bit {
            self.mirroring = if self.reg & 0x80 != 0 {
                Mirroring::Vertical
            } else {
                Mirroring::Horizontal
            };
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr >= 0x2000 {
            return 0;
        }
        let i = self.map_chr(addr);
        *self.chr_ram.get(i).unwrap_or(&0)
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if addr >= 0x2000 {
            return;
        }
        let i = self.map_chr(addr);
        if let Some(slot) = self.chr_ram.get_mut(i) {
            *slot = data;
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }

    fn save_state_capture(&self) -> Option<crate::save_state::MapperState> {
        use crate::save_state::mapper::{MirroringSnap, UnRom512Snap};
        Some(crate::save_state::MapperState::UnRom512(Box::new(UnRom512Snap {
            chr_ram: self.chr_ram.clone(),
            mirroring: MirroringSnap::from_live(self.mirroring),
            reg: self.reg,
            submapper: self.submapper,
        })))
    }

    fn save_state_apply(
        &mut self,
        state: &crate::save_state::MapperState,
    ) -> Result<(), crate::save_state::SaveStateError> {
        let crate::save_state::MapperState::UnRom512(snap) = state else {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        };
        // Cross-submapper apply is rejected: bus-conflict semantics
        // and mirroring-bit wiring differ across variants.
        if snap.submapper != self.submapper {
            return Err(crate::save_state::SaveStateError::UnsupportedMapper(0));
        }
        if snap.chr_ram.len() == self.chr_ram.len() {
            self.chr_ram.copy_from_slice(&snap.chr_ram);
        }
        self.mirroring = snap.mirroring.to_live();
        self.reg = snap.reg;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nes::rom::{Cartridge, TvSystem};

    /// 256 KiB PRG (16 banks * 16 KiB). Tag layout:
    /// - PRG bank N: first byte = `0x80 + N`, rest = 0xFF.
    /// PRG fill is 0xFF so bus-conflict ANDs are no-ops away from
    /// the tag byte.
    fn cart(submapper: u8) -> Cartridge {
        let mut prg = vec![0xFFu8; 16 * PRG_BANK_16K];
        for b in 0..16 {
            prg[b * PRG_BANK_16K] = 0x80 + b as u8;
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: vec![],
            chr_ram: true,
            mapper_id: 30,
            submapper,
            mirroring: Mirroring::Horizontal,
            battery_backed: false,
            prg_ram_size: 0,
            prg_nvram_size: 0,
            tv_system: TvSystem::Ntsc,
            is_nes2: true,
            prg_chr_crc32: 0,
            db_matched: false,
            fds_data: None,
        }
    }

    fn m(submapper: u8) -> UnRom512 {
        UnRom512::new(cart(submapper))
    }

    #[test]
    fn power_on_low_bank_is_zero_high_is_last() {
        let mut m = m(0);
        // $8000-$BFFF window starts on bank 0.
        assert_eq!(m.cpu_peek(0x8000), 0x80);
        // $C000-$FFFF fixed to last bank (15 in our test cart).
        assert_eq!(m.cpu_peek(0xC000), 0x8F);
    }

    #[test]
    fn low_five_bits_select_prg_bank() {
        let mut m = m(0);
        // Write at $8001 - PRG ROM byte is 0xFF, bus conflict no-op.
        m.cpu_write(0x8001, 0x05);
        assert_eq!(m.cpu_peek(0x8000), 0x85);
        m.cpu_write(0x8001, 0x0F);
        assert_eq!(m.cpu_peek(0x8000), 0x8F);
    }

    #[test]
    fn bits_5_and_6_select_chr_bank() {
        let mut m = m(0);
        // CHR bank tag bytes - write to bank 2 then back to 0.
        m.cpu_write(0x8001, 0x40); // CHR bank 2
        m.ppu_write(0x0000, 0xC2);
        m.cpu_write(0x8001, 0x60); // CHR bank 3
        m.ppu_write(0x0000, 0xC3);
        // Switch back: bank 2 still holds 0xC2, bank 3 still 0xC3.
        m.cpu_write(0x8001, 0x40);
        assert_eq!(m.ppu_read(0x0000), 0xC2);
        m.cpu_write(0x8001, 0x60);
        assert_eq!(m.ppu_read(0x0000), 0xC3);
    }

    #[test]
    fn high_window_stays_fixed_across_bank_switch() {
        let mut m = m(0);
        m.cpu_write(0x8001, 0x05);
        assert_eq!(m.cpu_peek(0xC000), 0x8F);
        m.cpu_write(0x8001, 0x0A);
        assert_eq!(m.cpu_peek(0xC000), 0x8F);
    }

    #[test]
    fn bus_conflict_masks_value_at_tag_byte_address_on_sub_zero() {
        let mut m = m(0);
        // ROM[$8000] = 0x80. Writing 0x0F there: 0x0F & 0x80 = 0,
        // so PRG bank stays 0.
        m.cpu_write(0x8000, 0x0F);
        assert_eq!(m.cpu_peek(0x8000), 0x80);
    }

    #[test]
    fn bus_conflict_active_on_sub_two() {
        let mut m = m(2);
        m.cpu_write(0x8000, 0x0F);
        assert_eq!(m.cpu_peek(0x8000), 0x80);
    }

    #[test]
    fn no_bus_conflict_on_sub_one() {
        let mut m = m(1);
        // Sub 1 has flash; writes to $C000-$FFFF still update the
        // bank-select latch and skip the bus-conflict AND.
        m.cpu_write(0xC000, 0x05);
        assert_eq!(m.cpu_peek(0x8000), 0x85);
    }

    #[test]
    fn sub_one_writes_to_low_window_drop_silently() {
        let mut m = m(1);
        // $8000-$BFFF on sub 1 → flash command. We drop them, so
        // the bank-select latch is unchanged.
        m.cpu_write(0x8001, 0x05);
        assert_eq!(m.cpu_peek(0x8000), 0x80);
    }

    #[test]
    fn sub_four_ignores_low_window_writes() {
        let mut m = m(4);
        // LED register at $8000-$BFFF is dropped.
        m.cpu_write(0x9000, 0x05);
        assert_eq!(m.cpu_peek(0x8000), 0x80);
        // $C000-$FFFF still selects PRG bank.
        m.cpu_write(0xC000, 0x05);
        assert_eq!(m.cpu_peek(0x8000), 0x85);
    }

    #[test]
    fn sub_three_d7_toggles_vertical_horizontal_mirroring() {
        let mut m = m(3);
        // Initial state per Mesen: vertical.
        assert_eq!(m.mirroring(), Mirroring::Vertical);
        // D7 clear → horizontal.
        m.cpu_write(0xC000, 0x00);
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
        // D7 set → vertical.
        m.cpu_write(0xC000, 0x80);
        assert_eq!(m.mirroring(), Mirroring::Vertical);
    }

    #[test]
    fn sub_zero_mirroring_stays_at_header_value() {
        let mut m = m(0);
        // Header was Horizontal in our test cart - D7 in the latch
        // should not toggle anything.
        m.cpu_write(0xC000, 0x80);
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
    }

    #[test]
    fn writes_below_8000_are_ignored() {
        let mut m = m(0);
        m.cpu_write(0x4020, 0xFF);
        m.cpu_write(0x6000, 0xFF);
        m.cpu_write(0x7FFF, 0xFF);
        assert_eq!(m.cpu_peek(0x8000), 0x80);
    }

    #[test]
    fn chr_ram_round_trips_per_bank() {
        let mut m = m(0);
        for b in 0..4u8 {
            m.cpu_write(0x8001, b << 5);
            m.ppu_write(0x0123, 0xA0 + b);
        }
        for b in 0..4u8 {
            m.cpu_write(0x8001, b << 5);
            assert_eq!(m.ppu_read(0x0123), 0xA0 + b);
        }
    }

    #[test]
    fn save_state_round_trip_preserves_chr_ram_and_latch() {
        let mut a = m(3);
        a.cpu_write(0xC000, 0x80 | 0x05); // V/H + PRG=5
        a.ppu_write(0x0010, 0xAA);
        let snap = a.save_state_capture().unwrap();

        let mut b = m(3);
        b.save_state_apply(&snap).unwrap();
        assert_eq!(b.reg, a.reg);
        assert_eq!(b.mirroring(), a.mirroring());
        assert_eq!(b.ppu_read(0x0010), 0xAA);
    }

    #[test]
    fn save_state_rejects_cross_submapper_apply() {
        let mut a = m(0);
        let snap_sub3 = m(3).save_state_capture().unwrap();
        assert!(a.save_state_apply(&snap_sub3).is_err());
    }

    #[test]
    fn save_state_rejects_cross_variant_apply() {
        use crate::save_state::mapper::{MapperState, NromSnap};
        let mut m = m(0);
        let bogus = MapperState::Nrom(NromSnap::default());
        assert!(m.save_state_apply(&bogus).is_err());
    }
}
