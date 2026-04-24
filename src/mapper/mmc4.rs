//! MMC4 / FxROM (mapper 10).
//!
//! The sibling of MMC2 — same register layout, same CHR latch trick,
//! same mirroring control. Differences:
//!
//! - **PRG windows are 16 KB, not 8 KB.**
//!   - `$8000-$BFFF` — 16 KB switchable via `$A000` (4-bit)
//!   - `$C000-$FFFF` — last 16 KB bank (fixed)
//! - **Latch triggers use the range form on both sides** (MMC2 has the
//!   range on the right only, single addresses on the left):
//!   - `$0FD8-$0FDF` → left latch = 0 (FD)
//!   - `$0FE8-$0FEF` → left latch = 1 (FE)
//!   - `$1FD8-$1FDF` → right latch = 0 (FD)
//!   - `$1FE8-$1FEF` → right latch = 1 (FE)
//! - **8 KB PRG-RAM at `$6000-$7FFF`**, battery-backed when the iNES
//!   `flag6 bit 1` is set. Shipping battery carts: Fire Emblem
//!   (1990), Fire Emblem Gaiden (1992). Famicom Wars (1988) is
//!   MMC4 without battery.
//!
//! All three MMC4 titles are Japan-only. Total library: 3 games.
//!
//! CHR register and latch semantics are identical to MMC2 — see
//! [`crate::mapper::mmc2`] for the longer writeup. The triggering
//! fetch uses the pre-trigger bank; the change takes effect on the
//! next fetch. We mirror that in `ppu_read` by resolving the bank
//! first, then updating the latch.
//!
//! Clean-room references (behavioral only, no copied code):
//! - `~/Git/Mesen2/Core/NES/Mappers/Nintendo/MMC4.h` (derives from
//!   `MMC2.h` — we duplicate the shared logic rather than build an
//!   inheritance chain, Rust-idiomatic)
//! - `~/Git/punes/src/core/mappers/mapper_010.c` + `MMC4.c`
//! - `~/Git/nestopia/source/core/board/NstBoardMmc4.cpp`
//! - nesdev.org/wiki/MMC4

use crate::mapper::Mapper;
use crate::rom::{Cartridge, Mirroring};

const PRG_BANK_16K: usize = 16 * 1024;
const CHR_BANK_4K: usize = 4 * 1024;
const PRG_RAM_SIZE: usize = 8 * 1024;

pub struct Mmc4 {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,
    mirroring: Mirroring,

    prg_bank_count_16k: usize,
    chr_bank_count_4k: usize,

    prg_ram: Vec<u8>,
    battery: bool,
    save_dirty: bool,

    /// `$A000` — 16 KB PRG bank index for `$8000-$BFFF`. 4-bit.
    prg_bank: u8,
    /// `$B000` — 4 KB CHR bank for `$0000-$0FFF` when left latch = 0.
    left_fd: u8,
    /// `$C000` — 4 KB CHR bank for `$0000-$0FFF` when left latch = 1.
    left_fe: u8,
    /// `$D000` — 4 KB CHR bank for `$1000-$1FFF` when right latch = 0.
    right_fd: u8,
    /// `$E000` — 4 KB CHR bank for `$1000-$1FFF` when right latch = 1.
    right_fe: u8,

    /// Left-window latch (0 = FD, 1 = FE). Power-on: 1.
    left_latch: u8,
    /// Right-window latch (0 = FD, 1 = FE). Power-on: 1.
    right_latch: u8,
}

impl Mmc4 {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count_16k = (cart.prg_rom.len() / PRG_BANK_16K).max(1);

        let is_chr_ram = cart.chr_ram || cart.chr_rom.is_empty();
        let chr = if is_chr_ram {
            vec![0u8; 8 * 1024]
        } else {
            cart.chr_rom
        };
        let chr_bank_count_4k = (chr.len() / CHR_BANK_4K).max(1);

        // 8 KB PRG-RAM regardless of what the header claims. All known
        // MMC4 carts use exactly 8 KB; some headers are noisy about
        // the size. Match CNROM's defensive clamp.
        let prg_ram = vec![0u8; (cart.prg_ram_size + cart.prg_nvram_size).max(PRG_RAM_SIZE)];

        Self {
            prg_rom: cart.prg_rom,
            chr,
            chr_ram: is_chr_ram,
            mirroring: cart.mirroring,
            prg_bank_count_16k,
            chr_bank_count_4k,
            prg_ram,
            battery: cart.battery_backed,
            save_dirty: false,
            prg_bank: 0,
            left_fd: 0,
            left_fe: 0,
            right_fd: 0,
            right_fe: 0,
            left_latch: 1,
            right_latch: 1,
        }
    }

    fn left_chr_bank(&self) -> usize {
        let bank = if self.left_latch == 0 {
            self.left_fd
        } else {
            self.left_fe
        };
        (bank as usize) % self.chr_bank_count_4k
    }

    fn right_chr_bank(&self) -> usize {
        let bank = if self.right_latch == 0 {
            self.right_fd
        } else {
            self.right_fe
        };
        (bank as usize) % self.chr_bank_count_4k
    }

    fn map_chr(&self, addr: u16) -> usize {
        let bank = if addr < 0x1000 {
            self.left_chr_bank()
        } else {
            self.right_chr_bank()
        };
        bank * CHR_BANK_4K + (addr as usize & (CHR_BANK_4K - 1))
    }

    fn map_prg(&self, addr: u16) -> usize {
        let bank = match addr {
            0x8000..=0xBFFF => (self.prg_bank as usize) % self.prg_bank_count_16k,
            0xC000..=0xFFFF => self.prg_bank_count_16k.saturating_sub(1),
            _ => 0,
        };
        bank * PRG_BANK_16K + (addr as usize & (PRG_BANK_16K - 1))
    }

    /// Post-read latch update. Range form on both sides (the key
    /// difference from MMC2). The triggering fetch has already been
    /// served from the pre-trigger bank.
    fn update_latch(&mut self, addr: u16) {
        match addr {
            0x0FD8..=0x0FDF => self.left_latch = 0,
            0x0FE8..=0x0FEF => self.left_latch = 1,
            0x1FD8..=0x1FDF => self.right_latch = 0,
            0x1FE8..=0x1FEF => self.right_latch = 1,
            _ => {}
        }
    }
}

impl Mapper for Mmc4 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
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
            0x8000..=0xFFFF => match addr & 0xF000 {
                0xA000 => self.prg_bank = data & 0x0F,
                0xB000 => self.left_fd = data & 0x1F,
                0xC000 => self.left_fe = data & 0x1F,
                0xD000 => self.right_fd = data & 0x1F,
                0xE000 => self.right_fe = data & 0x1F,
                0xF000 => {
                    self.mirroring = if data & 0x01 != 0 {
                        Mirroring::Horizontal
                    } else {
                        Mirroring::Vertical
                    };
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => {
                let i = (addr - 0x6000) as usize;
                *self.prg_ram.get(i).unwrap_or(&0)
            }
            0x8000..=0xFFFF => {
                let i = self.map_prg(addr);
                *self.prg_rom.get(i).unwrap_or(&0)
            }
            _ => 0,
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr >= 0x2000 {
            return 0;
        }
        let i = self.map_chr(addr);
        let byte = *self.chr.get(i).unwrap_or(&0);
        self.update_latch(addr);
        byte
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if self.chr_ram && addr < 0x2000 {
            let i = self.map_chr(addr);
            if let Some(slot) = self.chr.get_mut(i) {
                *slot = data;
            }
        }
        if addr < 0x2000 {
            self.update_latch(addr);
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
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

    /// 256 KB PRG (16 × 16 KB banks) where every byte equals its 16 KB
    /// bank index; 128 KB CHR (32 × 4 KB banks). Matches the MMC2 test
    /// convention.
    fn tagged_cart() -> Cartridge {
        let mut prg = vec![0u8; 16 * PRG_BANK_16K];
        for b in 0..16 {
            prg[b * PRG_BANK_16K..(b + 1) * PRG_BANK_16K].fill(b as u8);
        }
        let mut chr = vec![0u8; 32 * CHR_BANK_4K];
        for b in 0..32 {
            chr[b * CHR_BANK_4K..(b + 1) * CHR_BANK_4K].fill(b as u8);
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: chr,
            chr_ram: false,
            mapper_id: 10,
            submapper: 0,
            mirroring: Mirroring::Vertical,
            battery_backed: false,
            prg_ram_size: 0,
            prg_nvram_size: 0,
            tv_system: TvSystem::Ntsc,
            is_nes2: false,
            prg_chr_crc32: 0,
            db_matched: false,
        }
    }

    fn battery_cart() -> Cartridge {
        let mut cart = tagged_cart();
        cart.battery_backed = true;
        cart
    }

    // ---- PRG banking (16 KB windows — the MMC4 delta from MMC2) ----

    #[test]
    fn prg_default_layout_fixes_last_16k() {
        let m = Mmc4::new(tagged_cart());
        // $A000 default = 0 → switchable 16 KB window reads bank 0.
        assert_eq!(m.cpu_peek(0x8000), 0);
        assert_eq!(m.cpu_peek(0xBFFF), 0);
        // Fixed window at $C000-$FFFF → last bank (15).
        assert_eq!(m.cpu_peek(0xC000), 15);
        assert_eq!(m.cpu_peek(0xFFFF), 15);
    }

    #[test]
    fn prg_a000_switches_16k_low_window() {
        let mut m = Mmc4::new(tagged_cart());
        m.cpu_write(0xA000, 7);
        // Whole 16 KB window moves — not just the first 8 KB.
        assert_eq!(m.cpu_peek(0x8000), 7);
        assert_eq!(m.cpu_peek(0x9FFF), 7);
        assert_eq!(m.cpu_peek(0xA000), 7); // still bank 7 — MMC2 would be different here
        assert_eq!(m.cpu_peek(0xBFFF), 7);
        // Fixed window unchanged.
        assert_eq!(m.cpu_peek(0xC000), 15);
    }

    #[test]
    fn prg_a000_masks_to_4_bits() {
        let mut m = Mmc4::new(tagged_cart());
        m.cpu_write(0xA000, 0xF9);
        assert_eq!(m.cpu_peek(0x8000), 9);
    }

    // ---- CHR latch: range form on BOTH sides (the other MMC4 delta) ----

    #[test]
    fn chr_left_latch_triggers_across_0fd8_to_0fdf_range() {
        // MMC4 (unlike MMC2) uses the 8-address range form on the left
        // side too. Every address $0FD8-$0FDF must flip to FD.
        let mut m = Mmc4::new(tagged_cart());
        m.cpu_write(0xB000, 10); // left FD
        m.cpu_write(0xC000, 20); // left FE

        for trigger in 0x0FD8..=0x0FDF {
            // Reset to FE.
            m.ppu_read(0x0FE8);
            assert_eq!(m.ppu_read(0x0000), 20);
            // Trigger flips to FD.
            m.ppu_read(trigger);
            assert_eq!(m.ppu_read(0x0000), 10, "addr ${:04X} failed", trigger);
        }
        for trigger in 0x0FE8..=0x0FEF {
            m.ppu_read(0x0FD8);
            assert_eq!(m.ppu_read(0x0000), 10);
            m.ppu_read(trigger);
            assert_eq!(m.ppu_read(0x0000), 20, "addr ${:04X} failed", trigger);
        }
    }

    #[test]
    fn chr_right_latch_triggers_across_full_range() {
        let mut m = Mmc4::new(tagged_cart());
        m.cpu_write(0xD000, 14);
        m.cpu_write(0xE000, 21);

        for trigger in 0x1FD8..=0x1FDF {
            m.ppu_read(0x1FE8);
            assert_eq!(m.ppu_read(0x1000), 21);
            m.ppu_read(trigger);
            assert_eq!(m.ppu_read(0x1000), 14, "addr ${:04X} failed", trigger);
        }
        for trigger in 0x1FE8..=0x1FEF {
            m.ppu_read(0x1FD8);
            assert_eq!(m.ppu_read(0x1000), 14);
            m.ppu_read(trigger);
            assert_eq!(m.ppu_read(0x1000), 21, "addr ${:04X} failed", trigger);
        }
    }

    #[test]
    fn chr_triggering_fetch_uses_pre_trigger_bank() {
        // The read at the trigger address itself must return the OLD
        // bank's byte; only subsequent reads see the swap.
        let mut m = Mmc4::new(tagged_cart());
        m.cpu_write(0xB000, 3);
        m.cpu_write(0xC000, 7);
        // Power-on = FE = bank 7.
        assert_eq!(m.ppu_read(0x0FD8), 7); // triggering fetch — pre-trigger bank
        // Now the latch has flipped to FD.
        assert_eq!(m.ppu_read(0x0000), 3);
    }

    #[test]
    fn chr_triggers_ignore_out_of_range_addresses() {
        let mut m = Mmc4::new(tagged_cart());
        m.cpu_write(0xB000, 3);
        m.cpu_write(0xC000, 7);
        // Prime FD.
        m.ppu_read(0x0FD8);
        assert_eq!(m.ppu_read(0x0000), 3);
        // $0FD7 (below range) and $0FE0 (between ranges) must not flip.
        m.ppu_read(0x0FD7);
        assert_eq!(m.ppu_read(0x0000), 3);
        m.ppu_read(0x0FE0);
        assert_eq!(m.ppu_read(0x0000), 3);
        // $0FF0 (above both) must not flip.
        m.ppu_read(0x0FF0);
        assert_eq!(m.ppu_read(0x0000), 3);
    }

    #[test]
    fn chr_left_and_right_latches_are_independent() {
        let mut m = Mmc4::new(tagged_cart());
        m.cpu_write(0xB000, 1);
        m.cpu_write(0xC000, 2);
        m.cpu_write(0xD000, 3);
        m.cpu_write(0xE000, 4);

        m.ppu_read(0x0FD8);
        assert_eq!(m.ppu_read(0x0000), 1);
        assert_eq!(m.ppu_read(0x1000), 4); // right still on FE

        m.ppu_read(0x1FD8);
        assert_eq!(m.ppu_read(0x0000), 1); // left still on FD
        assert_eq!(m.ppu_read(0x1000), 3);
    }

    #[test]
    fn chr_power_on_latches_are_both_fe() {
        let mut m = Mmc4::new(tagged_cart());
        m.cpu_write(0xB000, 4);
        m.cpu_write(0xC000, 5);
        m.cpu_write(0xD000, 6);
        m.cpu_write(0xE000, 7);
        // No latch triggers yet — both reads must see the FE-side bank.
        assert_eq!(m.ppu_read(0x0000), 5);
        assert_eq!(m.ppu_read(0x1000), 7);
    }

    // ---- Mirroring ----

    #[test]
    fn f000_toggles_mirroring() {
        let mut m = Mmc4::new(tagged_cart());
        m.cpu_write(0xF000, 0);
        assert_eq!(m.mirroring(), Mirroring::Vertical);
        m.cpu_write(0xF000, 1);
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
    }

    // ---- PRG-RAM + battery ----

    #[test]
    fn prg_ram_roundtrip_at_6000_7fff() {
        let mut m = Mmc4::new(tagged_cart());
        m.cpu_write(0x6000, 0xAB);
        m.cpu_write(0x7FFF, 0xCD);
        assert_eq!(m.cpu_peek(0x6000), 0xAB);
        assert_eq!(m.cpu_peek(0x7FFF), 0xCD);
    }

    #[test]
    fn non_battery_cart_exposes_no_save_data() {
        let m = Mmc4::new(tagged_cart());
        assert!(m.save_data().is_none());
        assert!(!m.save_dirty());
    }

    #[test]
    fn battery_cart_exposes_8kib_save_data() {
        let mut m = Mmc4::new(battery_cart());
        assert_eq!(m.save_data().map(|s| s.len()), Some(PRG_RAM_SIZE));
        // Writing to PRG-RAM dirties the save.
        assert!(!m.save_dirty());
        m.cpu_write(0x6000, 0x42);
        assert!(m.save_dirty());
        m.mark_saved();
        assert!(!m.save_dirty());
    }

    #[test]
    fn battery_cart_load_save_data_roundtrips() {
        let mut m = Mmc4::new(battery_cart());
        let mut snapshot = vec![0u8; PRG_RAM_SIZE];
        snapshot[0] = 0x11;
        snapshot[PRG_RAM_SIZE - 1] = 0x22;
        m.load_save_data(&snapshot);
        assert_eq!(m.cpu_peek(0x6000), 0x11);
        assert_eq!(m.cpu_peek(0x7FFF), 0x22);
    }

    #[test]
    fn battery_cart_load_save_rejects_wrong_size() {
        let mut m = Mmc4::new(battery_cart());
        // Prime the RAM so we can detect whether `load_save_data`
        // silently stomped on it.
        m.cpu_write(0x6000, 0x55);
        m.load_save_data(&[0; 4096]); // too short
        assert_eq!(m.cpu_peek(0x6000), 0x55);
        m.load_save_data(&[0; 16384]); // too long
        assert_eq!(m.cpu_peek(0x6000), 0x55);
    }

    // ---- Register decoding ----

    #[test]
    fn registers_decode_by_top_nibble_of_address() {
        let mut m = Mmc4::new(tagged_cart());
        m.cpu_write(0xABCD, 5); // any $A000-$AFFF → PRG bank
        assert_eq!(m.cpu_peek(0x8000), 5);
        m.cpu_write(0xB111, 3); // any $B000-$BFFF → left FD
        m.ppu_read(0x0FD8); // flip to FD
        assert_eq!(m.ppu_read(0x0000), 3);
        m.cpu_write(0xFABC, 1); // any $F000-$FFFF → mirroring
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
    }
}
