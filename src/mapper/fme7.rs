// SPDX-License-Identifier: GPL-3.0-or-later
//! Sunsoft FME-7 / 5A / 5B - iNES mapper 69.
//!
//! Drove a handful of late-era Famicom + NES titles, headlined by
//! *Gimmick!* (5B, with audio), *Batman: Return of the Joker* (FME-7,
//! the only US release on this chip), and *Hebereke* / *Gremlins 2 JP*
//! / *Barcode World* (FME-7, JP-only). The FME-7, 5A, and 5B share the
//! same banking / IRQ surface and an identical pinout; the 5B
//! additionally carries the YM2149F-derived audio expansion handled
//! in [`crate::mapper::sunsoft5b_audio`].
//!
//! ## Register surface (`addr & 0xE000`)
//!
//! | Range          | Effect                                          |
//! |----------------|-------------------------------------------------|
//! | `$8000-$9FFF`  | Command register: low 4 bits select command 0-F |
//! | `$A000-$BFFF`  | Parameter for the previously-latched command    |
//! | `$C000-$DFFF`  | (5B only) audio register select                 |
//! | `$E000-$FFFF`  | (5B only) audio register write                  |
//!
//! The FME-7 uses a load-the-command-then-write-the-parameter pattern
//! (similar to MMC3 `$8000` + `$8001`) instead of decoding the address
//! directly. The 16 commands cover: CHR slots 0-7 (`$0`-`$7`),
//! `$6000-$7FFF` PRG/RAM bank + RAM enable (`$8`), `$8000`/`$A000`/
//! `$C000` PRG slots (`$9`/`$A`/`$B`), mirroring (`$C`), IRQ control
//! (`$D`), IRQ counter low (`$E`), IRQ counter high (`$F`).
//!
//! ## `$8` PRG bank 0 layout - `ERbB BBBB`
//!
//! - bit 7 (`E`): RAM enable. When clear with the RAM-select bit set,
//!   `$6000-$7FFF` reads return open bus (we surface 0).
//! - bit 6 (`R`): 0 = window holds PRG-ROM bank `BBBBBB`,
//!   1 = window holds 8 KiB PRG-RAM (still bank-switchable -
//!   *Gimmick!* relies on this to address 8 KiB of save RAM).
//! - bits 0-5: bank index (6 bits → 512 KiB on the FME-7, 256 KiB on
//!   5A/5B; PRG-RAM banks max at 32 KiB on commercial carts).
//!
//! ## IRQ
//!
//! Independent enables for the counter (decrement gate) and the IRQ
//! line (signal gate). Counter is a plain 16-bit decrement-per-CPU-
//! cycle; IRQ asserts when the counter wraps from `$0000` to `$FFFF`
//! (i.e. underflows). Any write to command `$D` acknowledges a
//! pending IRQ. The latch is "directly settable" - no separate
//! reload register, unlike MMC3.
//!
//! Reference: <https://www.nesdev.org/wiki/Sunsoft_FME-7> and
//! <https://www.nesdev.org/wiki/Sunsoft_5B_audio>. Cross-checked
//! against `~/Git/Mesen2/Core/NES/Mappers/Sunsoft/SunsoftFme7.h`,
//! `~/Git/punes/src/core/mappers/mapper_069.c`, and Mesen2's
//! `Sunsoft5bAudio.h`. The IRQ semantics - counter ticks whenever
//! `irq_counter_enabled` regardless of `irq_enabled`, IRQ asserts
//! only on underflow when `irq_enabled` - mirror Mesen2 exactly.

use crate::mapper::sunsoft5b_audio::Sunsoft5bAudio;
use crate::mapper::Mapper;
use crate::rom::{Cartridge, Mirroring};

const PRG_BANK_8K: usize = 8 * 1024;
const CHR_BANK_1K: usize = 1024;
/// Commercial 5A/5B/FME-7 carts ship with at most 32 KiB of PRG-RAM
/// (4 banks × 8 KiB). We allocate 32 KiB unconditionally so the
/// `$8` bank-index always lands in-range.
const PRG_RAM_SIZE: usize = 32 * 1024;

pub struct Fme7 {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,
    prg_ram: Vec<u8>,

    /// Latched command number from `$8000-$9FFF` writes.
    command: u8,
    /// Last value written to command `$8` - bit 7 = RAM enable, bit 6 =
    /// RAM-select, bits 0-5 = bank.
    work_ram_value: u8,
    /// Bank indices for `$8000`, `$A000`, `$C000` (commands `$9`/`$A`/`$B`).
    /// `$E000` is hardwired to the last bank.
    prg_banks: [u8; 3],
    /// CHR bank indices for the 8 1 KiB slots (commands `$0`-`$7`).
    chr_banks: [u8; 8],

    /// `prg_bank_count - 1` for the PRG-ROM image, in 8 KiB units.
    prg_bank_mask: usize,
    /// `prg_ram_bank_count - 1`, in 8 KiB units (always 3 here).
    prg_ram_bank_mask: usize,
    chr_bank_mask: usize,

    mirroring: Mirroring,

    /// IRQ counter (16-bit) - decrements every CPU cycle when
    /// `irq_counter_enabled`, and asserts `/IRQ` on underflow when
    /// `irq_enabled`.
    irq_counter: u16,
    irq_enabled: bool,
    irq_counter_enabled: bool,
    irq_line: bool,

    audio: Sunsoft5bAudio,

    battery: bool,
    save_dirty: bool,
}

impl Fme7 {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count = (cart.prg_rom.len() / PRG_BANK_8K).max(1);
        debug_assert!(prg_bank_count.is_power_of_two());
        let prg_bank_mask = prg_bank_count - 1;

        let is_chr_ram = cart.chr_ram || cart.chr_rom.is_empty();
        let chr = if is_chr_ram {
            vec![0u8; 8 * CHR_BANK_1K]
        } else {
            cart.chr_rom
        };
        let chr_bank_count = (chr.len() / CHR_BANK_1K).max(1);
        debug_assert!(chr_bank_count.is_power_of_two());
        let chr_bank_mask = chr_bank_count - 1;

        // 32 KiB unconditionally - covers Gimmick!'s 8 KiB battery and
        // gives the FME-7's bank-select space room without relying on
        // the cart header.
        let prg_ram_total =
            (cart.prg_ram_size + cart.prg_nvram_size).max(PRG_RAM_SIZE);
        let prg_ram_bank_mask =
            ((prg_ram_total / PRG_BANK_8K).max(1)).next_power_of_two() - 1;

        Self {
            prg_rom: cart.prg_rom,
            chr,
            chr_ram: is_chr_ram,
            prg_ram: vec![0u8; prg_ram_total],
            command: 0,
            work_ram_value: 0,
            prg_banks: [0; 3],
            chr_banks: [0; 8],
            prg_bank_mask,
            prg_ram_bank_mask,
            chr_bank_mask,
            mirroring: cart.mirroring,
            irq_counter: 0,
            irq_enabled: false,
            irq_counter_enabled: false,
            irq_line: false,
            audio: Sunsoft5bAudio::new(),
            battery: cart.battery_backed,
            save_dirty: false,
        }
    }

    fn last_prg_rom_bank(&self) -> usize {
        self.prg_bank_mask
    }

    /// Resolve a `$6000-$7FFF` byte. The `$8` register decides whether
    /// this window reads PRG-ROM (bank from `work_ram_value`), PRG-RAM
    /// (bank from same), or open bus.
    fn read_6000_window(&self, addr: u16) -> u8 {
        let off = (addr - 0x6000) as usize;
        let ram_select = (self.work_ram_value & 0x40) != 0;
        let ram_enable = (self.work_ram_value & 0x80) != 0;
        if ram_select {
            if !ram_enable {
                return 0; // open bus per wiki §$8 register quirk
            }
            let bank = (self.work_ram_value & 0x3F) as usize & self.prg_ram_bank_mask;
            *self.prg_ram.get(bank * PRG_BANK_8K + off).unwrap_or(&0)
        } else {
            let bank = (self.work_ram_value & 0x3F) as usize & self.prg_bank_mask;
            *self.prg_rom.get(bank * PRG_BANK_8K + off).unwrap_or(&0)
        }
    }

    fn write_6000_window(&mut self, addr: u16, data: u8) {
        let ram_select = (self.work_ram_value & 0x40) != 0;
        let ram_enable = (self.work_ram_value & 0x80) != 0;
        if !ram_select || !ram_enable {
            return; // ROM mode or RAM disabled - write absorbed
        }
        let off = (addr - 0x6000) as usize;
        let bank = (self.work_ram_value & 0x3F) as usize & self.prg_ram_bank_mask;
        if let Some(slot) = self.prg_ram.get_mut(bank * PRG_BANK_8K + off) {
            if *slot != data {
                *slot = data;
                if self.battery {
                    self.save_dirty = true;
                }
            }
        }
    }

    fn read_prg_rom(&self, addr: u16) -> u8 {
        let slot = ((addr - 0x8000) >> 13) as usize; // 0..=3
        let bank = if slot < 3 {
            (self.prg_banks[slot] & 0x3F) as usize
        } else {
            self.last_prg_rom_bank()
        };
        let bank = bank & self.prg_bank_mask;
        let off = (addr & 0x1FFF) as usize;
        *self.prg_rom.get(bank * PRG_BANK_8K + off).unwrap_or(&0)
    }

    fn read_chr(&self, addr: u16) -> u8 {
        let slot = ((addr >> 10) & 0x07) as usize;
        let bank = (self.chr_banks[slot] as usize) & self.chr_bank_mask;
        let off = (addr & 0x03FF) as usize;
        *self.chr.get(bank * CHR_BANK_1K + off).unwrap_or(&0)
    }

    fn write_chr(&mut self, addr: u16, data: u8) {
        if !self.chr_ram {
            return;
        }
        let slot = ((addr >> 10) & 0x07) as usize;
        let bank = (self.chr_banks[slot] as usize) & self.chr_bank_mask;
        let off = (addr & 0x03FF) as usize;
        if let Some(b) = self.chr.get_mut(bank * CHR_BANK_1K + off) {
            *b = data;
        }
    }

    fn dispatch_parameter(&mut self, value: u8) {
        match self.command & 0x0F {
            0..=7 => self.chr_banks[self.command as usize] = value,
            0x8 => self.work_ram_value = value,
            0x9..=0xB => {
                self.prg_banks[(self.command - 0x9) as usize] = value & 0x3F;
            }
            0xC => {
                self.mirroring = match value & 0x03 {
                    0 => Mirroring::Vertical,
                    1 => Mirroring::Horizontal,
                    2 => Mirroring::SingleScreenLower,
                    _ => Mirroring::SingleScreenUpper,
                };
            }
            0xD => {
                // Any write here acks the pending IRQ; the counter
                // and signal enables are taken from this byte.
                self.irq_enabled = (value & 0x01) != 0;
                self.irq_counter_enabled = (value & 0x80) != 0;
                self.irq_line = false;
            }
            0xE => self.irq_counter = (self.irq_counter & 0xFF00) | u16::from(value),
            0xF => {
                self.irq_counter = (self.irq_counter & 0x00FF) | (u16::from(value) << 8)
            }
            _ => unreachable!(),
        }
    }
}

impl Mapper for Fme7 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => self.read_6000_window(addr),
            0x8000..=0xFFFF => self.read_prg_rom(addr),
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        match addr {
            0x6000..=0x7FFF => self.write_6000_window(addr, data),
            0x8000..=0xFFFF => match addr & 0xE000 {
                0x8000 => self.command = data & 0x0F,
                0xA000 => self.dispatch_parameter(data),
                0xC000 | 0xE000 => self.audio.write_register(addr, data),
                _ => {}
            },
            _ => {}
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr < 0x2000 {
            self.read_chr(addr)
        } else {
            0
        }
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if addr < 0x2000 {
            self.write_chr(addr, data);
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }

    fn on_cpu_cycle(&mut self) {
        if self.irq_counter_enabled {
            let prev = self.irq_counter;
            self.irq_counter = self.irq_counter.wrapping_sub(1);
            // Underflow happens when counter wraps from $0000 → $FFFF.
            if prev == 0 && self.irq_enabled {
                self.irq_line = true;
            }
        }
        self.audio.clock();
    }

    fn irq_line(&self) -> bool {
        self.irq_line
    }

    fn audio_output(&self) -> Option<f32> {
        Some(self.audio.mix_sample())
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

    /// 256 KiB PRG (32 banks of 8 KiB), 256 KiB CHR-ROM (256 banks).
    /// Bank N tagged with byte N for both sides.
    fn cart() -> Cartridge {
        let mut prg = vec![0u8; 32 * PRG_BANK_8K];
        for bank in 0..32 {
            let base = bank * PRG_BANK_8K;
            prg[base..base + PRG_BANK_8K].fill(bank as u8);
        }
        let mut chr = vec![0u8; 256 * CHR_BANK_1K];
        for bank in 0..256 {
            let base = bank * CHR_BANK_1K;
            chr[base..base + CHR_BANK_1K].fill(bank as u8);
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: chr,
            chr_ram: false,
            mapper_id: 69,
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

    /// Helper: latch a command then send its parameter.
    fn send_cmd(m: &mut Fme7, cmd: u8, param: u8) {
        m.cpu_write(0x8000, cmd);
        m.cpu_write(0xA000, param);
    }

    #[test]
    fn power_on_layout_pins_last_bank_at_e000_only() {
        let m = Fme7::new(cart());
        // PRG bank regs power up to 0; $8000/$A000/$C000 read bank 0,
        // $E000 reads the last bank.
        assert_eq!(m.cpu_peek(0x8000), 0);
        assert_eq!(m.cpu_peek(0xA000), 0);
        assert_eq!(m.cpu_peek(0xC000), 0);
        assert_eq!(m.cpu_peek(0xE000), 31);
        assert_eq!(m.cpu_peek(0xFFFF), 31);
    }

    #[test]
    fn prg_banks_route_via_command_parameter_pair() {
        let mut m = Fme7::new(cart());
        send_cmd(&mut m, 0x9, 0x05); // $8000 = bank 5
        send_cmd(&mut m, 0xA, 0x06); // $A000 = bank 6
        send_cmd(&mut m, 0xB, 0x07); // $C000 = bank 7
        assert_eq!(m.cpu_peek(0x8000), 5);
        assert_eq!(m.cpu_peek(0xA000), 6);
        assert_eq!(m.cpu_peek(0xC000), 7);
        // Last-bank fix unchanged.
        assert_eq!(m.cpu_peek(0xE000), 31);
    }

    #[test]
    fn chr_banks_route_individually() {
        let mut m = Fme7::new(cart());
        for slot in 0..8u8 {
            send_cmd(&mut m, slot, 0x10 + slot);
        }
        for slot in 0..8u8 {
            let addr = (slot as u16) * 0x0400;
            assert_eq!(m.ppu_read(addr), 0x10 + slot, "slot {slot}");
        }
    }

    #[test]
    fn mirroring_command_c_picks_each_of_four_modes() {
        let mut m = Fme7::new(cart());
        for (val, expected) in [
            (0, Mirroring::Vertical),
            (1, Mirroring::Horizontal),
            (2, Mirroring::SingleScreenLower),
            (3, Mirroring::SingleScreenUpper),
        ] {
            send_cmd(&mut m, 0xC, val);
            assert_eq!(m.mirroring(), expected, "value {val}");
        }
    }

    #[test]
    fn six_thousand_window_in_rom_mode_reads_prg_rom_bank() {
        let mut m = Fme7::new(cart());
        // ROM mode (bit 6 = 0), bank 5.
        send_cmd(&mut m, 0x8, 0x05);
        assert_eq!(m.cpu_peek(0x6000), 5);
        assert_eq!(m.cpu_peek(0x7FFF), 5);
        // Writes are absorbed in ROM mode.
        m.cpu_write(0x6000, 0x99);
        assert_eq!(m.cpu_peek(0x6000), 5);
    }

    #[test]
    fn six_thousand_window_in_ram_mode_with_disabled_chip_returns_open_bus() {
        let mut m = Fme7::new(cart());
        send_cmd(&mut m, 0x8, 0x40); // RAM-select = 1, RAM-enable = 0 → open bus
        assert_eq!(m.cpu_peek(0x6000), 0);
        assert_eq!(m.cpu_peek(0x7FFF), 0);
        m.cpu_write(0x6000, 0xAA);
        // Write absorbed; later enable should NOT see the write
        // (because the write was dropped at the disabled chip).
        send_cmd(&mut m, 0x8, 0xC0); // enable
        assert_eq!(m.cpu_peek(0x6000), 0);
    }

    #[test]
    fn six_thousand_window_in_ram_mode_round_trips_when_enabled() {
        let mut m = Fme7::new(cart());
        send_cmd(&mut m, 0x8, 0xC0); // RAM-select + RAM-enable + bank 0
        m.cpu_write(0x6000, 0x42);
        m.cpu_write(0x7FFF, 0x55);
        assert_eq!(m.cpu_peek(0x6000), 0x42);
        assert_eq!(m.cpu_peek(0x7FFF), 0x55);

        // Switch to PRG-RAM bank 1 and verify the previous bank is
        // distinct.
        send_cmd(&mut m, 0x8, 0xC1);
        assert_eq!(m.cpu_peek(0x6000), 0); // bank 1 still empty
        send_cmd(&mut m, 0x8, 0xC0);
        assert_eq!(m.cpu_peek(0x6000), 0x42);
    }

    #[test]
    fn irq_counter_decrements_only_when_counter_enabled() {
        let mut m = Fme7::new(cart());
        send_cmd(&mut m, 0xE, 0x10); // counter low = 0x10
        send_cmd(&mut m, 0xF, 0x00); // counter high = 0x00 → counter = 0x0010
        // No enables yet - counter held.
        for _ in 0..100 {
            m.on_cpu_cycle();
        }
        assert_eq!(m.irq_counter, 0x0010);
        // Enable counter only (signal off): counter ticks but no IRQ.
        send_cmd(&mut m, 0xD, 0x80);
        for _ in 0..0x10 {
            m.on_cpu_cycle();
        }
        assert!(!m.irq_line);
        // One more cycle underflows; signal still gated off → still no IRQ.
        m.on_cpu_cycle();
        assert!(!m.irq_line);
    }

    #[test]
    fn irq_fires_on_underflow_and_acks_via_command_d() {
        let mut m = Fme7::new(cart());
        send_cmd(&mut m, 0xE, 0x05);
        send_cmd(&mut m, 0xF, 0x00);
        send_cmd(&mut m, 0xD, 0x81); // counter on + signal on
        // 5 cycles to count down from 5 → 0; one more cycle to underflow.
        for _ in 0..5 {
            m.on_cpu_cycle();
            assert!(!m.irq_line(), "early IRQ at counter {:04X}", m.irq_counter);
        }
        // Counter is at 0; next cycle wraps to 0xFFFF and asserts.
        m.on_cpu_cycle();
        assert!(m.irq_line());
        assert_eq!(m.irq_counter, 0xFFFF);
        // Ack via command $D - any value clears the line.
        send_cmd(&mut m, 0xD, 0x00);
        assert!(!m.irq_line());
        assert!(!m.irq_enabled);
        assert!(!m.irq_counter_enabled);
    }

    #[test]
    fn audio_register_writes_route_through_audio_module() {
        let mut m = Fme7::new(cart());
        // Select internal register 8 (channel A volume) and set to 0x0F.
        m.cpu_write(0xC000, 0x08);
        m.cpu_write(0xE000, 0x0F);
        // Spin enough cycles to clock the audio module a few hundred
        // times - output should become non-zero with a non-zero
        // period and tone enabled.
        m.cpu_write(0xC000, 0x00); // period lo
        m.cpu_write(0xE000, 0x04);
        m.cpu_write(0xC000, 0x07); // mixer
        m.cpu_write(0xE000, 0xFE); // tone A on, others off
        let mut max_seen = 0.0_f32;
        for _ in 0..2048 {
            m.on_cpu_cycle();
            if let Some(s) = m.audio_output() {
                if s > max_seen {
                    max_seen = s;
                }
            }
        }
        assert!(max_seen > 0.0, "audio output never went non-zero");
    }
}
