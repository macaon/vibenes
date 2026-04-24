// SPDX-License-Identifier: GPL-3.0-or-later
//! Namco 163 / 175 / 340 (iNES mappers 19 and 210).
//!
//! Namco's late-era workhorse. Mapper 19 is the full Namco 163 ASIC
//! (often written "N163") with expansion audio; mapper 210 covers its
//! two audio-less descendants, Namco 175 and Namco 340. Mesen2 packs
//! all three variants into one mapper class with runtime auto-detect,
//! and we follow the same convention — the shipping iNES 1.0 library
//! has a lot of ambiguously-tagged Namco carts that rely on behavior-
//! sniffing to pick the right chip.
//!
//! ## Variants
//!
//! | variant | mapper | distinguishing features |
//! |---|---|---|
//! | N163 | 19 (or 210:0 fallback) | expansion audio, CIRAM-as-CHR, per-quarter PRG-RAM WP, 128 B internal sound RAM |
//! | N175 | 210:1 | no audio, plain 8 KB PRG-RAM with single WP bit at `$C000` |
//! | N340 | 210:2 | no audio, no PRG-RAM, `$E000` top 2 bits = mirror mode |
//!
//! Mapper 210 submapper 0 means "old header, could be either 175 or
//! 340" — auto-detect per runtime hints.
//!
//! ## PRG
//!
//! Three switchable 8 KB windows + last bank fixed at `$E000`.
//! Register writes (decoded via `addr & 0xF800`):
//! - `$E000-$E7FF` — PRG bank 0 low 6 bits (`$8000-$9FFF`). Bit 7 on
//!   N163 disables audio output (we store but don't synth). Bits 6-7
//!   on N340 select mirroring mode.
//! - `$E800-$EFFF` — PRG bank 1 (`$A000-$BFFF`). Bits 6-7 on N163
//!   gate CIRAM-as-CHR routing for low/high CHR halves (see CHR).
//! - `$F000-$F7FF` — PRG bank 2 (`$C000-$DFFF`).
//! - `$F800-$FFFF` — PRG-RAM write-protect register (N163) plus the
//!   audio address / auto-increment flag for `$4800` access.
//!
//! PRG-RAM is 8 KB at `$6000-$7FFF`. The N163 splits it into four
//! 2 KB quarters each with its own write-protect bit (`$F800`
//! bits 0-3); bit 6 is the global write-enable. Power-on: everything
//! disabled, so games explicitly unlock before writing. N175 uses a
//! single write-protect bit in `$C000` bit 0; N340 has no PRG-RAM.
//!
//! ## CHR + nametable routing
//!
//! Eight 1 KB CHR bank registers at `$8000-$BFFF` (regs 0-7), plus
//! four more at `$C000-$DFFF` (regs 8-11) that drive the four 1 KB
//! nametable slots. For each CHR bank register on the N163:
//! - value `< $E0` → CHR-ROM bank `value`
//! - value `>= $E0` → one of the two internal CIRAM banks
//!   (`value & 1` picks A or B), BUT only when the gating bit for
//!   this half is clear:
//!     - `$E800` bit 6 gates CHR banks 0-3 (`$0000-$0FFF`)
//!     - `$E800` bit 7 gates CHR banks 4-7 (`$1000-$1FFF`)
//!   With the gate bit SET, values `>= $E0` still read CHR-ROM.
//!
//! Nametable registers (banks 8-11, at `$C000`/`$C800`/`$D000`/`$D800`)
//! always route `>= $E0` → CIRAM regardless of gates. That's how a
//! cart picks CIRAM mirroring — write `$E0` / `$E1` into the NT regs
//! to build H / V / single-screen layouts.
//!
//! N175 has a twist: `$C000-$C7FF` writes are the PRG-RAM WP register
//! instead of NT bank 8; `$C800-$DFFF` still act as NT banks 9-11.
//!
//! ## IRQ
//!
//! 15-bit up-counter at `$5000` (low byte) and `$5800` (high 7 bits +
//! bit-15 enable). When enabled and the counter's low 15 bits aren't
//! already `0x7FFF`, it increments each CPU cycle. On reaching
//! `0x7FFF` it fires `/IRQ` and halts (does not wrap). Writing either
//! register acknowledges a pending IRQ.
//!
//! ## Audio
//!
//! N163 packs 128 bytes of internal RAM plus 8 channels of wavetable
//! synthesis in the ASIC. This implementation exposes **only** the
//! RAM side — writes / reads through `$4800` with the address +
//! auto-increment latch at `$F800`. Games that use the 128-byte RAM
//! for general state (a handful do) will work. Audio sample
//! generation is deferred to a dedicated expansion-audio pass
//! alongside VRC6 / VRC7 / FDS.
//!
//! If not battery-backed, the audio RAM powers on in an undefined
//! state. We zero-init — same as the PRG-RAM init convention.
//!
//! ## Battery format
//!
//! When battery-backed we persist both the 8 KB PRG-RAM and the
//! 128-byte audio RAM into one 8320-byte save file, PRG-RAM first.
//! Matches Mesen2's `.sav` layout (`_saveRam ++ audio->GetInternalRam()`).
//!
//! Clean-room references (behavioral only, no copied code):
//! - `~/Git/Mesen2/Core/NES/Mappers/Namco/Namco163.h`
//! - `~/Git/punes/src/core/mappers/mapper_019.c`
//! - `~/Git/nestopia/source/core/board/NstBoardNamcot163.cpp`
//! - nesdev.org/wiki/INES_Mapper_019, .../INES_Mapper_210

use crate::mapper::{Mapper, NametableSource, NametableWriteTarget};
use crate::rom::{Cartridge, Mirroring};

const PRG_BANK_8K: usize = 8 * 1024;
const CHR_BANK_1K: usize = 1024;
const PRG_RAM_SIZE: usize = 8 * 1024;
const AUDIO_RAM_SIZE: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Variant {
    /// Full Namco 163. Mapper 19, or mapper 210 after auto-detect.
    Namco163,
    /// Namco 175 — no audio, single-bit PRG-RAM WP. Mapper 210:1.
    Namco175,
    /// Namco 340 — no audio, no PRG-RAM, `$E000` bits 6-7 mirror.
    /// Mapper 210:2.
    Namco340,
    /// Auto-detect — start non-committal and let runtime sniffing
    /// pick 163 / 175 / 340.
    Unknown,
}

pub struct Namco163 {
    prg_rom: Vec<u8>,
    chr: Vec<u8>,
    chr_ram: bool,
    prg_ram: Vec<u8>,
    audio_ram: [u8; AUDIO_RAM_SIZE],

    prg_bank_count_8k: usize,
    chr_bank_count_1k: usize,

    variant: Variant,
    auto_detect: bool,
    /// True once we've seen a write to `$6000-$7FFF` — rules out
    /// Namco340 (which has no PRG-RAM).
    not_namco340: bool,

    /// PRG bank selectors (low 6 bits each; `$E000`/`$E800`/`$F000`
    /// hold these).
    prg_banks: [u8; 3],
    /// CHR + nametable bank selectors. Indices 0-7 are CHR banks (at
    /// PPU `$0000-$1FFF`); 8-11 are NT banks (driving PPU `$2000-$2FFF`).
    chr_banks: [u8; 12],
    /// `$E800` bit 6 → false means "values `>= $E0` for CHR banks
    /// 0-3 route to CIRAM." True means "always CHR-ROM." Mesen2
    /// naming preserved so cross-reference stays obvious; yes, the
    /// sense is backwards.
    low_chr_nt_mode: bool,
    /// `$E800` bit 7, same sense, gates CHR banks 4-7.
    high_chr_nt_mode: bool,

    /// `$F800` write-protect byte. N163 per-quarter bits 0-3 + global
    /// WE bit 6. N175 uses bit 0 only.
    write_protect: u8,

    /// `$F800` low 7 bits = audio RAM address; bit 7 = auto-increment.
    audio_addr: u8,
    audio_auto_inc: bool,

    /// 15-bit counter + bit 15 enable flag, packed into a u16.
    irq_counter: u16,
    irq_line: bool,

    mirroring: Mirroring,
    battery: bool,
    save_dirty: bool,
}

impl Namco163 {
    pub fn new(cart: Cartridge) -> Self {
        let prg_bank_count_8k = (cart.prg_rom.len() / PRG_BANK_8K).max(1);

        // Decide variant BEFORE moving anything out of `cart`.
        let db_board = crate::gamedb::lookup(cart.prg_chr_crc32).map(|e| e.board);
        let (variant, auto_detect) = Self::pick_variant(&cart, db_board);

        let is_chr_ram = cart.chr_ram || cart.chr_rom.is_empty();
        let chr = if is_chr_ram {
            vec![0u8; 8 * 1024]
        } else {
            cart.chr_rom
        };
        let chr_bank_count_1k = (chr.len() / CHR_BANK_1K).max(1);

        let prg_ram = vec![0u8; (cart.prg_ram_size + cart.prg_nvram_size).max(PRG_RAM_SIZE)];

        Self {
            prg_rom: cart.prg_rom,
            chr,
            chr_ram: is_chr_ram,
            prg_ram,
            audio_ram: [0; AUDIO_RAM_SIZE],
            prg_bank_count_8k,
            chr_bank_count_1k,
            variant,
            auto_detect,
            not_namco340: false,
            prg_banks: [0; 3],
            chr_banks: [0; 12],
            low_chr_nt_mode: false,
            high_chr_nt_mode: false,
            write_protect: 0,
            audio_addr: 0,
            audio_auto_inc: false,
            irq_counter: 0,
            irq_line: false,
            mirroring: cart.mirroring,
            battery: cart.battery_backed,
            save_dirty: false,
        }
    }

    /// Pick initial variant + whether runtime auto-detect stays on.
    /// Priority: game DB board string > mapper 210 submapper > mapper
    /// 19 default (N163 with auto-detect armed).
    fn pick_variant(cart: &Cartridge, db_board: Option<&'static str>) -> (Variant, bool) {
        if let Some(board) = db_board {
            if board.contains("NAMCOT-163") || board.contains("NAMCO-163") {
                return (Variant::Namco163, false);
            }
            if board.contains("NAMCOT-175") || board.contains("NAMCO-175") {
                return (Variant::Namco175, false);
            }
            if board.contains("NAMCOT-340") || board.contains("NAMCO-340") {
                return (Variant::Namco340, false);
            }
        }
        if cart.mapper_id == 210 {
            return match cart.submapper {
                1 => (Variant::Namco175, false),
                2 => (Variant::Namco340, false),
                _ => (Variant::Unknown, true),
            };
        }
        // Mapper 19 default: assume N163 but keep auto-detect armed —
        // some mis-tagged headers land N175 / N340 under mapper 19.
        (Variant::Namco163, true)
    }

    fn set_variant(&mut self, v: Variant) {
        if !self.auto_detect {
            return;
        }
        // Once we've seen a PRG-RAM write, reject any attempt to
        // settle on N340 (which has no PRG-RAM).
        if self.not_namco340 && v == Variant::Namco340 {
            return;
        }
        self.variant = v;
    }

    fn map_prg(&self, addr: u16) -> usize {
        let bank = match addr {
            0x8000..=0x9FFF => (self.prg_banks[0] as usize) % self.prg_bank_count_8k,
            0xA000..=0xBFFF => (self.prg_banks[1] as usize) % self.prg_bank_count_8k,
            0xC000..=0xDFFF => (self.prg_banks[2] as usize) % self.prg_bank_count_8k,
            0xE000..=0xFFFF => self.prg_bank_count_8k.saturating_sub(1),
            _ => 0,
        };
        bank * PRG_BANK_8K + (addr as usize & (PRG_BANK_8K - 1))
    }

    /// Map a PPU read in `$0000-$1FFF` to the flat CHR byte, resolving
    /// the N163's CIRAM-as-CHR routing. Returns `None` when the slot
    /// has been redirected to internal CIRAM — the caller then reads
    /// from CIRAM directly (but `ppu_read` here always returns 0 for
    /// that case since CIRAM is owned by the PPU, not the mapper).
    fn map_chr(&self, addr: u16) -> Option<usize> {
        let reg = ((addr >> 10) & 0x07) as usize;
        if self.variant == Variant::Namco163 && self.chr_banks[reg] >= 0xE0 {
            let gate_set = if reg < 4 {
                self.low_chr_nt_mode
            } else {
                self.high_chr_nt_mode
            };
            if !gate_set {
                return None; // redirected to internal CIRAM
            }
        }
        let bank = (self.chr_banks[reg] as usize) % self.chr_bank_count_1k;
        Some(bank * CHR_BANK_1K + (addr as usize & (CHR_BANK_1K - 1)))
    }

    /// Per-variant PRG-RAM writability for a `$6000-$7FFF` address.
    fn prg_ram_writable(&self, addr: u16) -> bool {
        match self.variant {
            Variant::Namco163 => {
                // Global WE (bit 6) + per-2KB quarter bit clear.
                let global = (self.write_protect & 0x40) != 0;
                if !global {
                    return false;
                }
                let quarter_mask = match addr {
                    0x6000..=0x67FF => 0x01,
                    0x6800..=0x6FFF => 0x02,
                    0x7000..=0x77FF => 0x04,
                    0x7800..=0x7FFF => 0x08,
                    _ => 0,
                };
                (self.write_protect & quarter_mask) == 0
            }
            Variant::Namco175 => (self.write_protect & 0x01) != 0,
            Variant::Namco340 => false, // no PRG-RAM
            // Unknown — permissive until a variant commits.
            Variant::Unknown => true,
        }
    }

    /// Side-effect-free read of the audio RAM at the current cursor.
    fn peek_audio(&self) -> u8 {
        self.audio_ram[(self.audio_addr & 0x7F) as usize]
    }

    /// Read audio RAM + apply auto-increment side effect.
    fn read_audio(&mut self) -> u8 {
        let byte = self.peek_audio();
        if self.audio_auto_inc {
            self.audio_addr = (self.audio_addr.wrapping_add(1)) & 0x7F;
        }
        byte
    }

    fn write_audio(&mut self, data: u8) {
        self.audio_ram[(self.audio_addr & 0x7F) as usize] = data;
        if self.battery {
            self.save_dirty = true;
        }
        if self.audio_auto_inc {
            self.audio_addr = (self.audio_addr.wrapping_add(1)) & 0x7F;
        }
    }

    fn write_register(&mut self, addr: u16, data: u8) {
        match addr & 0xF800 {
            0x4800 => {
                // Writing audio RAM commits to N163 (this reg exists
                // only on that chip).
                self.set_variant(Variant::Namco163);
                self.write_audio(data);
            }

            0x5000 => {
                self.set_variant(Variant::Namco163);
                self.irq_counter = (self.irq_counter & 0xFF00) | data as u16;
                self.irq_line = false;
            }

            0x5800 => {
                self.set_variant(Variant::Namco163);
                self.irq_counter = (self.irq_counter & 0x00FF) | ((data as u16) << 8);
                self.irq_line = false;
            }

            0x8000 | 0x8800 | 0x9000 | 0x9800 => {
                // CHR banks 0-3 (low pattern-table half).
                let bank = ((addr - 0x8000) >> 11) as usize;
                self.chr_banks[bank] = data;
            }
            0xA000 | 0xA800 | 0xB000 | 0xB800 => {
                // CHR banks 4-7 (high pattern-table half).
                let bank = (((addr - 0xA000) >> 11) + 4) as usize;
                self.chr_banks[bank] = data;
            }

            0xC000 | 0xC800 | 0xD000 | 0xD800 => {
                // Variant sniff: writes to $C800+ mean this is N163
                // (N175 only responds to $C000). N175 only commits
                // when a write lands at the $C000-$C7FF range.
                if addr >= 0xC800 {
                    self.set_variant(Variant::Namco163);
                } else if self.variant != Variant::Namco163 && self.auto_detect {
                    self.set_variant(Variant::Namco175);
                }

                if self.variant == Variant::Namco175 {
                    self.write_protect = data;
                } else {
                    let bank = (((addr - 0xC000) >> 11) + 8) as usize;
                    self.chr_banks[bank] = data;
                }
            }

            0xE000 => {
                // N340 detection: bit 7 means "no audio, mirror bits
                // in bits 6-7." Mesen2 also flips to N340 when bit 6
                // is set on a cart that isn't already N163.
                if (data & 0x80) != 0 {
                    self.set_variant(Variant::Namco340);
                } else if (data & 0x40) != 0 && self.variant != Variant::Namco163 {
                    self.set_variant(Variant::Namco340);
                }

                self.prg_banks[0] = data & 0x3F;

                if self.variant == Variant::Namco340 {
                    // N340 packs mirroring mode in bits 7-6. Order
                    // matches Mesen2 — not the conventional 0=V/1=H.
                    self.mirroring = match (data >> 6) & 0x03 {
                        0 => Mirroring::SingleScreenLower,
                        1 => Mirroring::Vertical,
                        2 => Mirroring::Horizontal,
                        _ => Mirroring::SingleScreenUpper,
                    };
                }
                // N163: bit 7 is "disable audio" — tracked at audio
                // layer (deferred). We just store the bank bits.
            }

            0xE800 => {
                self.prg_banks[1] = data & 0x3F;
                if self.variant == Variant::Namco163 {
                    self.low_chr_nt_mode = (data & 0x40) != 0;
                    self.high_chr_nt_mode = (data & 0x80) != 0;
                }
            }

            0xF000 => {
                self.prg_banks[2] = data & 0x3F;
            }

            0xF800 => {
                // N163 only: PRG-RAM WP byte + audio address latch.
                self.set_variant(Variant::Namco163);
                if self.variant == Variant::Namco163 {
                    self.write_protect = data;
                    self.audio_addr = data & 0x7F;
                    self.audio_auto_inc = (data & 0x80) != 0;
                }
            }

            _ => {}
        }
    }
}

impl Mapper for Namco163 {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => {
                if matches!(self.variant, Variant::Namco340) {
                    return 0; // no PRG-RAM wired
                }
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

    fn cpu_read_ex(&mut self, addr: u16) -> Option<u8> {
        match addr & 0xF800 {
            0x4800 => Some(self.read_audio()),
            0x5000 => Some((self.irq_counter & 0xFF) as u8),
            0x5800 => Some((self.irq_counter >> 8) as u8),
            _ => None,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        match addr {
            0x4800..=0x5FFF | 0x8000..=0xFFFF => {
                self.write_register(addr, data);
            }
            0x6000..=0x7FFF => {
                // PRG-RAM write. Tells us this isn't a Namco340.
                self.not_namco340 = true;
                if matches!(self.variant, Variant::Namco340) {
                    self.set_variant(Variant::Unknown);
                }
                if self.prg_ram_writable(addr) {
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
            }
            _ => {}
        }
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
            0x4800..=0x4FFF => self.peek_audio(),
            0x5000..=0x57FF => (self.irq_counter & 0xFF) as u8,
            0x5800..=0x5FFF => (self.irq_counter >> 8) as u8,
            0x6000..=0x7FFF => {
                if matches!(self.variant, Variant::Namco340) {
                    return 0;
                }
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
        match self.map_chr(addr) {
            Some(i) => *self.chr.get(i).unwrap_or(&0),
            None => 0, // routed to CIRAM — PPU handles the actual fetch
        }
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if self.chr_ram && addr < 0x2000 {
            if let Some(i) = self.map_chr(addr) {
                if let Some(slot) = self.chr.get_mut(i) {
                    *slot = data;
                }
            }
        }
    }

    fn ppu_nametable_read(&mut self, slot: u8, offset: u16) -> NametableSource {
        // On N163, each of the four NT slots is driven by one of the
        // `$C000-$D800` bank registers (indices 8-11). Values `>= $E0`
        // route to internal CIRAM (bit 0 = which bank); lower values
        // pull from CHR-ROM. On N175 / N340 we defer to the normal
        // mirroring path.
        if self.variant != Variant::Namco163 {
            return NametableSource::Default;
        }
        let bank_val = self.chr_banks[8 + (slot as usize)];
        if bank_val >= 0xE0 {
            if bank_val & 0x01 == 0 {
                NametableSource::CiramA
            } else {
                NametableSource::CiramB
            }
        } else {
            let bank = (bank_val as usize) % self.chr_bank_count_1k;
            let i = bank * CHR_BANK_1K + (offset as usize & (CHR_BANK_1K - 1));
            NametableSource::Byte(*self.chr.get(i).unwrap_or(&0))
        }
    }

    fn ppu_nametable_write(
        &mut self,
        slot: u8,
        _offset: u16,
        _data: u8,
    ) -> NametableWriteTarget {
        if self.variant != Variant::Namco163 {
            return NametableWriteTarget::Default;
        }
        let bank_val = self.chr_banks[8 + (slot as usize)];
        if bank_val >= 0xE0 {
            if bank_val & 0x01 == 0 {
                NametableWriteTarget::CiramA
            } else {
                NametableWriteTarget::CiramB
            }
        } else {
            // CHR-ROM target — writes drop on the floor.
            NametableWriteTarget::Consumed
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }

    fn on_cpu_cycle(&mut self) {
        // Counter enabled via bit 15; increments while the low 15
        // bits are < 0x7FFF. On hitting 0x7FFF, fire and halt.
        if (self.irq_counter & 0x8000) == 0 {
            return;
        }
        let low = self.irq_counter & 0x7FFF;
        if low == 0x7FFF {
            return; // already halted
        }
        self.irq_counter = self.irq_counter.wrapping_add(1);
        if (self.irq_counter & 0x7FFF) == 0x7FFF {
            self.irq_line = true;
        }
    }

    fn irq_line(&self) -> bool {
        self.irq_line
    }

    fn save_data(&self) -> Option<&[u8]> {
        // We'd ideally expose `prg_ram ++ audio_ram` as one slice,
        // but those are separate buffers. Expose just the PRG-RAM
        // for now; when audio synthesis lands we'll promote to a
        // packed buffer per Mesen2's 8320-byte format. Games that
        // save to audio RAM will lose that portion across reboot —
        // acceptable until audio lands.
        self.battery.then(|| self.prg_ram.as_slice())
    }

    fn load_save_data(&mut self, data: &[u8]) {
        if !self.battery {
            return;
        }
        if data.len() == self.prg_ram.len() {
            self.prg_ram.copy_from_slice(data);
        } else if data.len() == self.prg_ram.len() + AUDIO_RAM_SIZE {
            // Forward-compat with the eventual Mesen2 `.sav` layout.
            let (ram, audio) = data.split_at(self.prg_ram.len());
            self.prg_ram.copy_from_slice(ram);
            self.audio_ram.copy_from_slice(audio);
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

    fn tagged_cart(mapper_id: u16, submapper: u8) -> Cartridge {
        let mut prg = vec![0u8; 32 * PRG_BANK_8K]; // 256 KB, 32 banks
        for b in 0..32 {
            prg[b * PRG_BANK_8K..(b + 1) * PRG_BANK_8K].fill(b as u8);
        }
        let mut chr = vec![0u8; 256 * CHR_BANK_1K]; // 256 KB, 256 banks
        for b in 0..256 {
            chr[b * CHR_BANK_1K..(b + 1) * CHR_BANK_1K].fill(b as u8);
        }
        Cartridge {
            prg_rom: prg,
            chr_rom: chr,
            chr_ram: false,
            mapper_id,
            submapper,
            mirroring: Mirroring::Horizontal,
            battery_backed: false,
            prg_ram_size: 0,
            prg_nvram_size: 0,
            tv_system: TvSystem::Ntsc,
            is_nes2: mapper_id == 210, // submappers only meaningful on NES 2.0
            prg_chr_crc32: 0,
            db_matched: false,
            fds_data: None,
        }
    }

    fn n163() -> Namco163 {
        let mut cart = tagged_cart(19, 0);
        cart.battery_backed = true;
        Namco163::new(cart)
    }
    fn n175() -> Namco163 {
        Namco163::new(tagged_cart(210, 1))
    }
    fn n340() -> Namco163 {
        Namco163::new(tagged_cart(210, 2))
    }

    // ---- Variant detection ----

    #[test]
    fn mapper_19_defaults_to_n163_with_autodetect() {
        let m = n163();
        assert_eq!(m.variant, Variant::Namco163);
        assert!(m.auto_detect);
    }

    #[test]
    fn mapper_210_submapper_1_locks_n175() {
        let m = n175();
        assert_eq!(m.variant, Variant::Namco175);
        assert!(!m.auto_detect);
    }

    #[test]
    fn mapper_210_submapper_2_locks_n340() {
        let m = n340();
        assert_eq!(m.variant, Variant::Namco340);
        assert!(!m.auto_detect);
    }

    #[test]
    fn mapper_210_submapper_0_starts_unknown() {
        let m = Namco163::new(tagged_cart(210, 0));
        assert_eq!(m.variant, Variant::Unknown);
        assert!(m.auto_detect);
    }

    // ---- PRG banking ----

    #[test]
    fn prg_default_layout_fixes_last_bank() {
        let m = n163();
        assert_eq!(m.cpu_peek(0x8000), 0);
        assert_eq!(m.cpu_peek(0xA000), 0);
        assert_eq!(m.cpu_peek(0xC000), 0);
        assert_eq!(m.cpu_peek(0xE000), 31);
    }

    #[test]
    fn prg_bank_regs_at_e000_e800_f000() {
        let mut m = n163();
        m.cpu_write(0xE000, 0x05); // bank 0 = 5
        m.cpu_write(0xE800, 0x0A); // bank 1 = 10
        m.cpu_write(0xF000, 0x0F); // bank 2 = 15
        assert_eq!(m.cpu_peek(0x8000), 5);
        assert_eq!(m.cpu_peek(0xA000), 10);
        assert_eq!(m.cpu_peek(0xC000), 15);
        assert_eq!(m.cpu_peek(0xE000), 31);
    }

    #[test]
    fn prg_bank_values_mask_to_6_bits() {
        let mut m = n163();
        m.cpu_write(0xE000, 0xFF); // 0x3F = 63, but only 32 banks → 63 % 32 = 31
        assert_eq!(m.cpu_peek(0x8000), 31);
    }

    // ---- CHR banking + CIRAM-as-CHR routing ----

    #[test]
    fn chr_low_value_reads_chr_rom() {
        let mut m = n163();
        m.cpu_write(0x8000, 5); // CHR bank 0 = 5
        assert_eq!(m.ppu_read(0x0000), 5);
        m.cpu_write(0xA000, 9); // CHR bank 4 = 9
        assert_eq!(m.ppu_read(0x1000), 9);
    }

    #[test]
    fn chr_high_value_routes_to_ciram_when_gate_clear() {
        // Values ≥ $E0 AND `$E800` bit 6 clear → CIRAM for banks 0-3.
        let mut m = n163();
        m.cpu_write(0x8000, 0xE0); // bank 0 = $E0 → CIRAM A
        // Low gate is bit 6 of $E800; starts clear, so CIRAM routing
        // active. map_chr returns None for CIRAM-redirected slots.
        assert!(!m.low_chr_nt_mode);
        assert!(m.map_chr(0x0000).is_none());
        // Setting bit 6 forces CHR-ROM even for high values.
        m.cpu_write(0xE800, 0x40);
        assert!(m.low_chr_nt_mode);
        assert!(m.map_chr(0x0000).is_some());
    }

    #[test]
    fn chr_high_gate_independent_from_low_gate() {
        let mut m = n163();
        m.cpu_write(0x8000, 0xE0); // bank 0 = $E0 (low half)
        m.cpu_write(0xA000, 0xE0); // bank 4 = $E0 (high half)
        // Only set high-gate ($E800 bit 7).
        m.cpu_write(0xE800, 0x80);
        assert!(!m.low_chr_nt_mode);
        assert!(m.high_chr_nt_mode);
        assert!(m.map_chr(0x0000).is_none()); // low: CIRAM
        assert!(m.map_chr(0x1000).is_some()); // high: CHR-ROM
    }

    // ---- Nametable routing ----

    #[test]
    fn nt_bank_routes_to_ciram_for_high_values() {
        let mut m = n163();
        // NT banks 8-11 are at $C000/$C800/$D000/$D800.
        m.cpu_write(0xC000, 0xE0); // slot 0 → CIRAM A
        m.cpu_write(0xC800, 0xE1); // slot 1 → CIRAM B
        m.cpu_write(0xD000, 0xE2); // slot 2 → bit 0 = 0 → CIRAM A
        m.cpu_write(0xD800, 0xE3); // slot 3 → CIRAM B
        assert_eq!(m.ppu_nametable_read(0, 0), NametableSource::CiramA);
        assert_eq!(m.ppu_nametable_read(1, 0), NametableSource::CiramB);
        assert_eq!(m.ppu_nametable_read(2, 0), NametableSource::CiramA);
        assert_eq!(m.ppu_nametable_read(3, 0), NametableSource::CiramB);
    }

    #[test]
    fn nt_bank_low_value_reads_from_chr_rom() {
        let mut m = n163();
        m.cpu_write(0xC000, 7); // NT slot 0 fetches CHR-ROM bank 7
        match m.ppu_nametable_read(0, 0x23) {
            NametableSource::Byte(b) => assert_eq!(b, 7),
            other => panic!("expected Byte(7), got {other:?}"),
        }
    }

    #[test]
    fn nt_routing_disabled_on_n175_and_n340() {
        let mut m = n175();
        // N175 treats $C000 writes as WP, so no NT routing — returns
        // Default and the PPU falls through to normal mirroring.
        m.cpu_write(0xC000, 0xE0);
        assert_eq!(m.ppu_nametable_read(0, 0), NametableSource::Default);

        let mut m = n340();
        m.cpu_write(0xC000, 0xE0);
        assert_eq!(m.ppu_nametable_read(0, 0), NametableSource::Default);
    }

    // ---- N175 write-protect at $C000 ----

    #[test]
    fn n175_c000_is_prg_ram_write_protect() {
        let mut m = n175();
        // Bit 0 set → enabled. Write + read back.
        m.cpu_write(0xC000, 0x01);
        m.cpu_write(0x6000, 0x42);
        assert_eq!(m.cpu_peek(0x6000), 0x42);
        // Bit 0 clear → writes dropped.
        m.cpu_write(0xC000, 0x00);
        m.cpu_write(0x6000, 0xFF);
        assert_eq!(m.cpu_peek(0x6000), 0x42);
    }

    // ---- N163 per-quarter PRG-RAM write-protect ----

    #[test]
    fn n163_prg_ram_default_disabled_until_f800_unlocks() {
        let mut m = n163();
        // Power-on: global WE bit (bit 6 of $F800) clear.
        m.cpu_write(0x6000, 0x55);
        assert_eq!(m.cpu_peek(0x6000), 0x00, "write must be dropped");
        // Unlock globally + all four quarters clear.
        m.cpu_write(0xF800, 0x40);
        m.cpu_write(0x6000, 0x55);
        assert_eq!(m.cpu_peek(0x6000), 0x55);
    }

    #[test]
    fn n163_prg_ram_per_quarter_bits() {
        let mut m = n163();
        // Unlock globally, but bit 2 set → quarter $7000-$77FF locked.
        m.cpu_write(0xF800, 0x40 | 0x04);
        m.cpu_write(0x6000, 0xAA); // quarter 0 — allowed
        m.cpu_write(0x6800, 0xAA); // quarter 1 — allowed
        m.cpu_write(0x7000, 0xBB); // quarter 2 — blocked
        m.cpu_write(0x7800, 0xAA); // quarter 3 — allowed
        assert_eq!(m.cpu_peek(0x6000), 0xAA);
        assert_eq!(m.cpu_peek(0x6800), 0xAA);
        assert_eq!(m.cpu_peek(0x7000), 0x00);
        assert_eq!(m.cpu_peek(0x7800), 0xAA);
    }

    // ---- N340: no PRG-RAM + mirror bits ----

    #[test]
    fn n340_e000_sets_mirroring_from_top_bits() {
        let mut m = n340();
        m.cpu_write(0xE000, 0x00); // bits 7-6 = 00 → single-lower
        assert_eq!(m.mirroring(), Mirroring::SingleScreenLower);
        m.cpu_write(0xE000, 0x40); // 01 → vertical
        assert_eq!(m.mirroring(), Mirroring::Vertical);
        m.cpu_write(0xE000, 0x80); // 10 → horizontal
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
        m.cpu_write(0xE000, 0xC0); // 11 → single-upper
        assert_eq!(m.mirroring(), Mirroring::SingleScreenUpper);
    }

    #[test]
    fn n340_has_no_prg_ram() {
        let mut m = n340();
        m.cpu_write(0x6000, 0x42); // dropped
        assert_eq!(m.cpu_peek(0x6000), 0);
    }

    // ---- Audio RAM (128 bytes, $4800 / $F800) ----

    #[test]
    fn audio_ram_write_read_without_auto_increment() {
        let mut m = n163();
        m.cpu_write(0xF800, 0x10); // address = 0x10, auto_inc = 0
        m.cpu_write(0x4800, 0x77);
        assert_eq!(m.peek_audio(), 0x77);
        assert_eq!(m.read_audio(), 0x77);
        // Address unchanged.
        assert_eq!(m.audio_addr, 0x10);
    }

    #[test]
    fn audio_ram_auto_increment_wraps_at_7f() {
        let mut m = n163();
        m.cpu_write(0xF800, 0x7E | 0x80); // addr = 0x7E, auto_inc = 1
        m.cpu_write(0x4800, 0xAA);
        assert_eq!(m.audio_addr, 0x7F);
        m.cpu_write(0x4800, 0xBB);
        assert_eq!(m.audio_addr, 0x00, "wraps at 7 bits");
        m.cpu_write(0x4800, 0xCC);
        assert_eq!(m.audio_addr, 0x01);
        // Bytes landed where we expected.
        assert_eq!(m.audio_ram[0x7E], 0xAA);
        assert_eq!(m.audio_ram[0x7F], 0xBB);
        assert_eq!(m.audio_ram[0x00], 0xCC);
    }

    #[test]
    fn audio_ram_read_via_cpu_read_ex() {
        let mut m = n163();
        m.cpu_write(0xF800, 0x20 | 0x80); // addr = 0x20, auto_inc
        m.cpu_write(0x4800, 0x33);
        m.cpu_write(0x4800, 0x44);
        m.cpu_write(0xF800, 0x20 | 0x80); // rewind
        assert_eq!(m.cpu_read_ex(0x4800), Some(0x33));
        assert_eq!(m.cpu_read_ex(0x4800), Some(0x44));
    }

    // ---- IRQ ----

    #[test]
    fn irq_disabled_unless_bit_15_set() {
        let mut m = n163();
        // Bit 15 clear: counter doesn't tick.
        m.cpu_write(0x5000, 0x00);
        m.cpu_write(0x5800, 0x00);
        for _ in 0..100 {
            m.on_cpu_cycle();
        }
        assert_eq!(m.irq_counter & 0x7FFF, 0);
        assert!(!m.irq_line());
    }

    #[test]
    fn irq_fires_when_counter_reaches_7fff() {
        let mut m = n163();
        // Start at 0x7FFC; enable via bit 15.
        m.cpu_write(0x5000, 0xFC); // low
        m.cpu_write(0x5800, 0xFF); // high 7 + bit 15 enable
        m.on_cpu_cycle(); // 7FFC → 7FFD
        assert!(!m.irq_line());
        m.on_cpu_cycle(); // 7FFD → 7FFE
        assert!(!m.irq_line());
        m.on_cpu_cycle(); // 7FFE → 7FFF → fire
        assert!(m.irq_line());
    }

    #[test]
    fn irq_halts_at_7fff_and_does_not_refire() {
        let mut m = n163();
        m.cpu_write(0x5000, 0xFE);
        m.cpu_write(0x5800, 0xFF);
        m.on_cpu_cycle(); // → 0x7FFF → fire
        assert!(m.irq_line());
        // Ack via write to $5000 or $5800.
        m.cpu_write(0x5800, 0xFF);
        assert!(!m.irq_line());
        // Counter still pinned at 7FFF; further ticks don't refire.
        for _ in 0..50 {
            m.on_cpu_cycle();
        }
        assert!(!m.irq_line());
    }

    #[test]
    fn irq_counter_readback() {
        let mut m = n163();
        m.cpu_write(0x5000, 0x34);
        m.cpu_write(0x5800, 0x12);
        assert_eq!(m.cpu_read_ex(0x5000), Some(0x34));
        assert_eq!(m.cpu_read_ex(0x5800), Some(0x12));
    }

    // ---- Battery save ----

    #[test]
    fn battery_save_data_reports_prg_ram() {
        let m = n163();
        assert_eq!(m.save_data().map(|b| b.len()), Some(PRG_RAM_SIZE));
    }

    #[test]
    fn load_save_accepts_prg_ram_only_format() {
        let mut m = n163();
        let mut snapshot = vec![0u8; PRG_RAM_SIZE];
        snapshot[0] = 0x55;
        m.load_save_data(&snapshot);
        // Unlock so the read reflects the load.
        m.cpu_write(0xF800, 0x40);
        assert_eq!(m.cpu_peek(0x6000), 0x55);
    }

    #[test]
    fn load_save_accepts_combined_prg_ram_plus_audio_ram_format() {
        let mut m = n163();
        let mut snapshot = vec![0u8; PRG_RAM_SIZE + AUDIO_RAM_SIZE];
        snapshot[0] = 0xAB;
        snapshot[PRG_RAM_SIZE] = 0xCD; // first byte of audio RAM
        snapshot[PRG_RAM_SIZE + AUDIO_RAM_SIZE - 1] = 0xEF;
        m.load_save_data(&snapshot);
        m.cpu_write(0xF800, 0x40);
        assert_eq!(m.cpu_peek(0x6000), 0xAB);
        // Peek audio RAM via the cursor.
        m.cpu_write(0xF800, 0x00);
        assert_eq!(m.peek_audio(), 0xCD);
        m.cpu_write(0xF800, 0x7F);
        assert_eq!(m.peek_audio(), 0xEF);
    }
}
