//! Famicom Disk System (iNES mapper 20).
//!
//! Implements the RAM adapter hardware that replaces the cart on
//! Japanese Famicom systems. Unique among our mappers: it boots
//! through an 8 KiB Nintendo BIOS at `$E000-$FFFF`, runs code out of
//! 32 KiB of PRG-RAM that the BIOS loads from disk at power-on, and
//! drives a simulated mechanical disk transport through a handful of
//! registers at `$4020-$4033`.
//!
//! This is Phase 1 — enough to boot `fdsirqtests.fds` and single-
//! side games to their title screen. Features:
//!
//! - BIOS at `$E000-$FFFF` (read-only).
//! - 32 KiB PRG-RAM at `$6000-$DFFF` (RW).
//! - 8 KiB CHR-RAM at PPU `$0000-$1FFF` (RW).
//! - Disk transport state machine at `$4020-$4025` / `$4030-$4033`.
//!   Reads a byte from the current side every ~149 CPU cycles once
//!   the motor + disk-ready + read-mode bits are set.
//! - 16-bit IRQ timer at `$4020/$4021/$4022` plus the disk-transfer
//!   IRQ at `$4025.7`, both OR'd into the mapper's `/IRQ` output.
//! - Auto-insert side 0 on power-on (single-side games "just work").
//! - `$4040-$4097` audio register window — writes stored, reads
//!   return 0. Synthesis is deferred to the expansion-audio phase.
//!
//! **Deferred** (Phase 2+): multi-disk-side swap UI, $E445 header
//! matching, $4032 "disk change" heuristic, IPS save persistence,
//! audio DSP.
//!
//! ## References
//!
//! Port of Mesen2's `Core/NES/Mappers/FDS/Fds.{h,cpp}` (GPL-3.0-or-
//! later, same license as this project, so structural porting is
//! fine per the clean-room carve-out for non-core subsystems). Byte
//! counts (`_delay = 149` between bytes, 50000-cycle head-reset
//! spin-up, 28300-bit leading gap, etc.) are protocol-exact; I've
//! kept Mesen2's comments in-place where they document tested
//! behavior (e.g. the `149 not 150` quirk for Ai Senshi Nicol).

use crate::fds::FdsData;
use crate::mapper::Mapper;
use crate::rom::{Cartridge, Mirroring};

/// Sentinel value for "no disk inserted in the drive."
const NO_DISK_INSERTED: u32 = 0xFF;

/// Spin-up delay (in CPU cycles) between end-of-head and the first
/// byte being available. Matches Mesen2's `50000`.
const HEAD_RESET_DELAY: u32 = 50000;

/// Cycles between successive bytes during a disk scan. Per Mesen2's
/// comment: "used to be 150; 149 / 151 both fix the Ai Senshi Nicol
/// level-2→3 NMI/$2006-write interference." We match 149 exactly
/// since that's the validated value.
const INTER_BYTE_DELAY: u32 = 149;

/// Size of the audio register window `$4040-$4097`. Storage-only for
/// now; synthesis comes later.
const AUDIO_REG_COUNT: usize = 0x4098 - 0x4040;

pub struct Fds {
    // --- Memory ---
    /// 32 KiB PRG-RAM at `$6000-$DFFF`. BIOS loads the boot sectors
    /// into this at power-on; game code runs out of it.
    prg_ram: Vec<u8>,
    /// 8 KiB BIOS at `$E000-$FFFF`.
    bios: Vec<u8>,
    /// 8 KiB CHR-RAM at PPU `$0000-$1FFF`. BIOS loads pattern data
    /// into this alongside PRG-RAM.
    chr_ram: Vec<u8>,

    // --- Disk state ---
    /// Per-side scan-ready data (gap / sync / block / fake-CRC /
    /// gap). Built by `FdsImage::gapped_sides` at load.
    disk_sides: Vec<Vec<u8>>,
    /// Sentinel `NO_DISK_INSERTED` when ejected, else 0..side_count.
    disk_number: u32,
    /// Byte index into `disk_sides[disk_number]`.
    disk_position: u32,
    /// Cycles remaining before the next disk byte. `0` means ready-
    /// to-read on the next `on_cpu_cycle` tick.
    delay: u32,
    /// CRC accumulator for disk-block CRC checking. Updated per byte
    /// while the CRC-control bit is clear; latched when control goes
    /// high. Tests against zero at block-end.
    crc_accumulator: u16,
    /// Previous value of `$4025.4` (CRC control). Used for edge
    /// detection: the rising edge stops CRC updates and latches the
    /// "bad CRC" flag.
    previous_crc_control: bool,
    /// True once the first non-zero byte of a block has been read —
    /// gap-end sentinel. Cleared by a head reset / end-of-disk.
    gap_ended: bool,
    /// Set by the first timing-tick of a scan. Distinguishes "we're
    /// actively reading bytes" from "motor's off / head parked."
    scanning_disk: bool,
    /// One-shot flag: a block's data byte has been delivered to the
    /// `read_data_reg` (or written). Cleared by `$4024` writes and
    /// `$4030/$4031` reads.
    transfer_complete: bool,
    /// True when the head is at the end-of-disk sentinel position.
    /// `on_cpu_cycle` uses this to inject the 50000-cycle spin-up
    /// delay before the next byte.
    end_of_head: bool,
    /// Last byte read from the disk into the CPU-visible register.
    read_data_reg: u8,
    /// Next byte the CPU wrote to `$4024`, waiting to be written to
    /// the disk on the next transport tick.
    write_data_reg: u8,
    /// True when the last-read block's CRC didn't match.
    bad_crc: bool,

    // --- $4022 IRQ timer state ---
    irq_reload_value: u16,
    irq_counter: u16,
    irq_enabled: bool,
    irq_repeat_enabled: bool,

    // --- $4023 register-enable gates ---
    disk_reg_enabled: bool,
    sound_reg_enabled: bool,

    // --- $4025 control register fields ---
    motor_on: bool,
    reset_transfer: bool,
    read_mode: bool,
    /// Hardware bit 3 of `$4025`: false = vertical, true = horizontal.
    mirroring: Mirroring,
    crc_control: bool,
    disk_ready: bool,
    disk_irq_enabled: bool,

    // --- IRQ lines ---
    /// `/IRQ` from the `$4022` timer. Wire-ORed with `disk_irq_line`.
    timer_irq_line: bool,
    /// `/IRQ` from disk-transfer completion (`$4025.7` + transfer
    /// tick). Wire-ORed with `timer_irq_line`.
    disk_irq_line: bool,

    // --- $4040-$4097 audio register window ---
    /// Storage-only mirror of every write into the audio range, so
    /// future audio-synthesis work can pick up state that games
    /// configured before the synth was wired. Reads from this range
    /// return 0 for now (see `cpu_read_ex`).
    audio_regs: [u8; AUDIO_REG_COUNT],

    /// External-connector write register (`$4026` in, `$4033` out).
    /// Stored verbatim so reads from `$4033` round-trip.
    ext_con_reg: u8,
}

impl Fds {
    pub fn new(cart: Cartridge) -> Self {
        // `Cartridge::from_fds_bytes` populates `fds_data` for mapper
        // 20 loads; every other path leaves it `None`. Reaching this
        // constructor without it is a programming error, not user
        // input.
        let data: FdsData = cart.fds_data.expect(
            "mapper::build dispatched FDS (mapper 20) without Cartridge::fds_data populated",
        );

        let mirroring = cart.mirroring;
        let prg_ram_size = cart.prg_ram_size.max(32 * 1024);

        Self {
            prg_ram: vec![0u8; prg_ram_size],
            bios: data.bios,
            chr_ram: vec![0u8; 8 * 1024],
            disk_sides: data.gapped_sides,
            disk_number: 0, // auto-insert side 0 at power-on
            disk_position: 0,
            delay: 0,
            crc_accumulator: 0,
            previous_crc_control: false,
            gap_ended: true,
            scanning_disk: false,
            transfer_complete: false,
            end_of_head: true, // trigger spin-up on first scan
            read_data_reg: 0,
            write_data_reg: 0,
            bad_crc: false,
            irq_reload_value: 0,
            irq_counter: 0,
            irq_enabled: false,
            irq_repeat_enabled: false,
            disk_reg_enabled: true,
            sound_reg_enabled: true,
            motor_on: false,
            reset_transfer: false,
            read_mode: false,
            mirroring,
            crc_control: false,
            disk_ready: false,
            disk_irq_enabled: false,
            timer_irq_line: false,
            disk_irq_line: false,
            audio_regs: [0; AUDIO_REG_COUNT],
            ext_con_reg: 0,
        }
    }

    fn is_disk_inserted(&self) -> bool {
        self.disk_number != NO_DISK_INSERTED
    }

    fn current_side_len(&self) -> u32 {
        self.disk_sides
            .get(self.disk_number as usize)
            .map(|s| s.len() as u32)
            .unwrap_or(0)
    }

    fn read_disk_byte(&self) -> u8 {
        self.disk_sides
            .get(self.disk_number as usize)
            .and_then(|s| s.get(self.disk_position as usize))
            .copied()
            .unwrap_or(0)
    }

    fn write_disk_byte(&mut self, value: u8) {
        if let Some(side) = self.disk_sides.get_mut(self.disk_number as usize) {
            if let Some(slot) = side.get_mut(self.disk_position as usize) {
                *slot = value;
                // Phase 3 will set a dirty flag here to feed the IPS
                // save-pipeline. For now writes land in memory only.
            }
        }
    }

    /// Disk-block CRC update, mirrors Mesen2's `UpdateCrc`. Polynomial
    /// is `0x8408` (CCITT reflected), fed LSB-first.
    fn update_crc(&mut self, value: u8) {
        self.crc_accumulator ^= value as u16;
        for _ in 0..8 {
            let carry = self.crc_accumulator & 1;
            self.crc_accumulator >>= 1;
            if carry != 0 {
                self.crc_accumulator ^= 0x8408;
            }
        }
    }

    fn clock_timer_irq(&mut self) {
        if !self.irq_enabled {
            return;
        }
        if self.irq_counter == 0 {
            self.timer_irq_line = true;
            self.irq_counter = self.irq_reload_value;
            if !self.irq_repeat_enabled {
                self.irq_enabled = false;
            }
        } else {
            self.irq_counter -= 1;
        }
    }

    fn clock_disk(&mut self) {
        if !self.is_disk_inserted() || !self.motor_on {
            // Motor off or ejected — transport parks. The BIOS polls
            // `$4032` to detect this state.
            self.end_of_head = true;
            self.scanning_disk = false;
            return;
        }

        if self.reset_transfer && !self.scanning_disk {
            return;
        }

        if self.end_of_head {
            // Head just returned to start-of-side. Wait for drive
            // spin-up before delivering the first byte.
            self.delay = HEAD_RESET_DELAY;
            self.end_of_head = false;
            self.disk_position = 0;
            self.gap_ended = false;
            return;
        }

        if self.delay > 0 {
            self.delay -= 1;
            return;
        }

        // Inter-byte delay elapsed — process the next byte.
        self.scanning_disk = true;
        let mut need_irq = self.disk_irq_enabled;

        if self.read_mode {
            let disk_data = self.read_disk_byte();

            if !self.previous_crc_control {
                self.update_crc(disk_data);
            }

            if !self.disk_ready {
                self.gap_ended = false;
                self.crc_accumulator = 0;
                self.bad_crc = false;
            } else if disk_data != 0 && !self.gap_ended {
                // First non-zero byte of a block: gap ended. Mesen2
                // suppresses the disk IRQ specifically on this
                // transition (the BIOS uses the sync-detect interrupt
                // differently than data-byte interrupts).
                self.gap_ended = true;
                need_irq = false;
            }

            if self.gap_ended {
                self.transfer_complete = true;
                self.read_data_reg = disk_data;
                if need_irq {
                    self.disk_irq_line = true;
                }
            }

            if !self.previous_crc_control && self.crc_control {
                // CRC-control rose: latch the "bad CRC" status.
                self.bad_crc = self.crc_accumulator != 0;
            }
        } else {
            // Write mode.
            let mut disk_data = self.write_data_reg;

            if !self.crc_control {
                self.transfer_complete = true;
                if need_irq {
                    self.disk_irq_line = true;
                }
            }

            if !self.disk_ready {
                disk_data = 0;
                self.crc_accumulator = 0;
            }

            if !self.crc_control {
                self.update_crc(disk_data);
            } else {
                // Emit the accumulated CRC, byte at a time.
                disk_data = (self.crc_accumulator & 0xFF) as u8;
                self.crc_accumulator >>= 8;
            }

            self.write_disk_byte(disk_data);
            self.gap_ended = false;
            self.bad_crc = false;
        }

        self.previous_crc_control = self.crc_control;

        self.disk_position += 1;
        if self.disk_position >= self.current_side_len() {
            self.motor_on = false;
            if self.disk_irq_enabled {
                // Kosodate Gokko disk copier expects an IRQ when the
                // drive reaches end-of-disk — without it the software
                // locks on a black screen.
                self.disk_irq_line = true;
            }
        } else {
            self.delay = INTER_BYTE_DELAY;
        }
    }
}

impl Mapper for Fds {
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.cpu_peek(addr)
    }

    fn cpu_peek(&self, addr: u16) -> u8 {
        match addr {
            0x6000..=0xDFFF => {
                let i = (addr - 0x6000) as usize;
                *self.prg_ram.get(i).unwrap_or(&0)
            }
            0xE000..=0xFFFF => {
                let i = (addr - 0xE000) as usize;
                *self.bios.get(i).unwrap_or(&0)
            }
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, data: u8) {
        match addr {
            0x6000..=0xDFFF => {
                let i = (addr - 0x6000) as usize;
                if let Some(slot) = self.prg_ram.get_mut(i) {
                    *slot = data;
                }
            }
            // BIOS space — writes dropped.
            0xE000..=0xFFFF => {}

            // Disk registers at $4020-$4026 (only $4020-$4026 have
            // meaning; higher bytes in that page aren't decoded).
            0x4020..=0x4097 => self.write_register(addr, data),

            _ => {}
        }
    }

    fn cpu_read_ex(&mut self, addr: u16) -> Option<u8> {
        // Route $4020-$5FFF register reads through the FDS register
        // file; anything we don't recognize returns `None` so the bus
        // can fall through to open-bus.
        match addr {
            0x4030 => {
                if !self.disk_reg_enabled {
                    return Some(0);
                }
                // Bits 2,5 read as open bus on real hardware; we
                // return 0 for them. `transfer_complete` and the
                // timer-IRQ flag are cleared by this read.
                let mut val: u8 = 0;
                if self.timer_irq_line {
                    val |= 0x01;
                }
                if self.transfer_complete {
                    val |= 0x02;
                }
                if matches!(self.mirroring, Mirroring::Horizontal) {
                    val |= 0x08;
                }
                // Clear the disk-transfer status and the timer IRQ
                // flag, matching Mesen2.
                self.transfer_complete = false;
                self.timer_irq_line = false;
                self.disk_irq_line = false;
                Some(val)
            }
            0x4031 => {
                if !self.disk_reg_enabled {
                    return Some(0);
                }
                self.transfer_complete = false;
                self.disk_irq_line = false;
                Some(self.read_data_reg)
            }
            0x4032 => {
                if !self.disk_reg_enabled {
                    return Some(0);
                }
                let inserted = self.is_disk_inserted();
                let mut val: u8 = 0;
                if !inserted {
                    val |= 0x01; // disk not in drive
                }
                if !inserted || !self.scanning_disk {
                    val |= 0x02; // disk not ready
                }
                if !inserted {
                    val |= 0x04; // disk not writable
                }
                Some(val)
            }
            0x4033 => {
                if !self.disk_reg_enabled {
                    return Some(0);
                }
                Some(self.ext_con_reg)
            }
            0x4040..=0x4097 => {
                // Audio register read. Phase 1 returns 0 — games
                // usually check for a specific volume value, which is
                // safe enough. Once audio DSP lands, return the
                // envelope / counter state.
                Some(0)
            }
            _ => None,
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        if addr >= 0x2000 {
            return 0;
        }
        *self.chr_ram.get(addr as usize).unwrap_or(&0)
    }

    fn ppu_write(&mut self, addr: u16, data: u8) {
        if addr < 0x2000 {
            if let Some(slot) = self.chr_ram.get_mut(addr as usize) {
                *slot = data;
            }
        }
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }

    fn on_cpu_cycle(&mut self) {
        self.clock_timer_irq();
        self.clock_disk();
    }

    fn irq_line(&self) -> bool {
        self.timer_irq_line || self.disk_irq_line
    }
}

impl Fds {
    fn write_register(&mut self, addr: u16, value: u8) {
        // Respect the $4023 enable gates: disk registers at
        // $4024-$4026 are gated by bit 0; sound at $4040+ by bit 1.
        if (!self.disk_reg_enabled && (0x4024..=0x4026).contains(&addr))
            || (!self.sound_reg_enabled && addr >= 0x4040)
        {
            return;
        }

        match addr {
            0x4020 => {
                self.irq_reload_value = (self.irq_reload_value & 0xFF00) | value as u16;
            }
            0x4021 => {
                self.irq_reload_value =
                    (self.irq_reload_value & 0x00FF) | ((value as u16) << 8);
            }
            0x4022 => {
                self.irq_repeat_enabled = (value & 0x01) != 0;
                self.irq_enabled = (value & 0x02) != 0 && self.disk_reg_enabled;
                if self.irq_enabled {
                    self.irq_counter = self.irq_reload_value;
                } else {
                    self.timer_irq_line = false;
                }
            }
            0x4023 => {
                self.disk_reg_enabled = (value & 0x01) != 0;
                self.sound_reg_enabled = (value & 0x02) != 0;
                if !self.disk_reg_enabled {
                    self.irq_enabled = false;
                    self.timer_irq_line = false;
                    self.disk_irq_line = false;
                }
            }
            0x4024 => {
                self.write_data_reg = value;
                self.transfer_complete = false;
                // Mesen2 notes "unsure about clearing irq here; FCEUX
                // and Nintendulator don't do this, puNES does." We
                // follow Mesen2 + puNES — cleared.
                self.disk_irq_line = false;
            }
            0x4025 => {
                self.motor_on = (value & 0x01) != 0;
                self.reset_transfer = (value & 0x02) != 0;
                self.read_mode = (value & 0x04) != 0;
                self.mirroring = if value & 0x08 != 0 {
                    Mirroring::Horizontal
                } else {
                    Mirroring::Vertical
                };
                self.crc_control = (value & 0x10) != 0;
                // Bit 6 is hardware-wired to 1; doesn't affect us.
                self.disk_ready = (value & 0x40) != 0;
                self.disk_irq_enabled = (value & 0x80) != 0;

                // Per FCEUX / puNES / Nintendulator: $4025 writes
                // also ack the disk IRQ. Fixes "error $20 at power-on"
                // on some unlicensed carts.
                self.disk_irq_line = false;
            }
            0x4026 => {
                self.ext_con_reg = value;
            }
            0x4040..=0x4097 => {
                // Audio register — storage-only until the synth lands.
                let idx = (addr - 0x4040) as usize;
                self.audio_regs[idx] = value;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fds::image::FdsImage;
    use crate::rom::{Cartridge, TvSystem};

    /// Build a synthetic FDS cartridge with one 65500-byte side and
    /// an 8 KiB BIOS filled with a known pattern so tests can
    /// distinguish BIOS reads from PRG-RAM / disk reads by value.
    fn make_cart() -> Cartridge {
        let mut raw = vec![0u8; 65500];
        raw[0] = 0x01; // disk-header block tag
        raw[1..15].copy_from_slice(b"*NINTENDO-HVC*");
        let image = FdsImage::from_bytes(&raw).unwrap();

        let mut bios = vec![0u8; 8192];
        // Fill with rolling pattern so BIOS reads are distinguishable.
        for (i, b) in bios.iter_mut().enumerate() {
            *b = i as u8;
        }

        Cartridge {
            prg_rom: Vec::new(),
            chr_rom: Vec::new(),
            chr_ram: true,
            mapper_id: 20,
            submapper: 0,
            mirroring: Mirroring::Horizontal,
            battery_backed: true,
            prg_ram_size: 32 * 1024,
            prg_nvram_size: 0,
            tv_system: TvSystem::Ntsc,
            is_nes2: false,
            prg_chr_crc32: 0,
            db_matched: false,
            fds_data: Some(FdsData {
                gapped_sides: image.gapped_sides(),
                headers: image.headers(),
                bios,
                bios_known_good: true,
                had_header: false,
            }),
        }
    }

    // ---- Memory map ----

    #[test]
    fn bios_maps_to_e000_through_ffff() {
        let m = Fds::new(make_cart());
        // BIOS byte i == (i & 0xFF).
        assert_eq!(m.cpu_peek(0xE000), 0x00);
        assert_eq!(m.cpu_peek(0xE100), 0x00);
        assert_eq!(m.cpu_peek(0xFFFF), 0xFF);
    }

    #[test]
    fn prg_ram_roundtrips_across_6000_dfff() {
        let mut m = Fds::new(make_cart());
        m.cpu_write(0x6000, 0xAB);
        m.cpu_write(0xDFFF, 0xCD);
        assert_eq!(m.cpu_peek(0x6000), 0xAB);
        assert_eq!(m.cpu_peek(0xDFFF), 0xCD);
    }

    #[test]
    fn bios_space_rejects_writes() {
        let mut m = Fds::new(make_cart());
        m.cpu_write(0xE000, 0xFF);
        // Byte still reads as the BIOS pattern (0x00 for offset 0).
        assert_eq!(m.cpu_peek(0xE000), 0x00);
    }

    #[test]
    fn chr_ram_ppu_roundtrip() {
        let mut m = Fds::new(make_cart());
        m.ppu_write(0x0000, 0x55);
        m.ppu_write(0x1FFF, 0xAA);
        assert_eq!(m.ppu_read(0x0000), 0x55);
        assert_eq!(m.ppu_read(0x1FFF), 0xAA);
    }

    // ---- $4023 register-enable gate ----

    #[test]
    fn disk_regs_gated_by_4023_bit_0() {
        let mut m = Fds::new(make_cart());
        // Disable disk regs.
        m.cpu_write(0x4023, 0x00);
        m.cpu_write(0x4024, 0xAA); // should be ignored
        assert_eq!(m.write_data_reg, 0);
        // Re-enable and try again.
        m.cpu_write(0x4023, 0x01);
        m.cpu_write(0x4024, 0xAA);
        assert_eq!(m.write_data_reg, 0xAA);
    }

    #[test]
    fn sound_regs_gated_by_4023_bit_1() {
        let mut m = Fds::new(make_cart());
        m.cpu_write(0x4023, 0x01); // disk on, sound off
        m.cpu_write(0x4040, 0x99);
        assert_eq!(m.audio_regs[0], 0);
        m.cpu_write(0x4023, 0x03);
        m.cpu_write(0x4040, 0x99);
        assert_eq!(m.audio_regs[0], 0x99);
    }

    // ---- $4020-$4022 timer IRQ ----

    #[test]
    fn timer_irq_fires_after_reload_cycles_when_enabled() {
        let mut m = Fds::new(make_cart());
        m.cpu_write(0x4023, 0x01); // disk regs enabled
        m.cpu_write(0x4020, 5); // reload low
        m.cpu_write(0x4021, 0); // reload high
        m.cpu_write(0x4022, 0x02); // enable, no repeat
        for _ in 0..5 {
            m.on_cpu_cycle();
            assert!(!m.timer_irq_line);
        }
        m.on_cpu_cycle(); // cycle 6: counter hits 0, fires
        assert!(m.timer_irq_line);
    }

    #[test]
    fn timer_irq_repeat_bit_reloads_on_fire() {
        let mut m = Fds::new(make_cart());
        m.cpu_write(0x4023, 0x01);
        m.cpu_write(0x4020, 2);
        m.cpu_write(0x4021, 0);
        m.cpu_write(0x4022, 0x03); // repeat + enable
        for _ in 0..3 {
            m.on_cpu_cycle();
        }
        // First fire after counter hits zero (cycle 3 with counter=2
        // counting 2,1,0).
        assert!(m.timer_irq_line);
        // Ack via $4030 read.
        let _ = m.cpu_read_ex(0x4030);
        assert!(!m.timer_irq_line);
        // With repeat armed, counter auto-reloaded. Fires again.
        for _ in 0..3 {
            m.on_cpu_cycle();
        }
        assert!(m.timer_irq_line);
    }

    #[test]
    fn timer_irq_clears_on_disable_via_4022() {
        let mut m = Fds::new(make_cart());
        m.cpu_write(0x4023, 0x01);
        m.cpu_write(0x4020, 0);
        m.cpu_write(0x4022, 0x02); // enable
        m.on_cpu_cycle(); // fires at counter=0
        assert!(m.timer_irq_line);
        m.cpu_write(0x4022, 0x00); // disable — also acks
        assert!(!m.timer_irq_line);
    }

    // ---- $4030 status read acks IRQs ----

    #[test]
    fn read_4030_reports_and_clears_timer_irq() {
        let mut m = Fds::new(make_cart());
        m.cpu_write(0x4023, 0x01);
        m.cpu_write(0x4020, 0);
        m.cpu_write(0x4022, 0x02);
        m.on_cpu_cycle();
        assert!(m.timer_irq_line);
        let v = m.cpu_read_ex(0x4030).unwrap();
        assert_eq!(v & 0x01, 0x01, "bit 0 reflects timer IRQ");
        // Re-read should show it cleared.
        let v2 = m.cpu_read_ex(0x4030).unwrap();
        assert_eq!(v2 & 0x01, 0);
    }

    // ---- Disk transport ----

    #[test]
    fn disk_scan_advances_position_after_inter_byte_delay() {
        let mut m = Fds::new(make_cart());
        // Enable drive: motor on, read mode, disk-ready, IRQ off.
        m.cpu_write(0x4023, 0x01);
        m.cpu_write(0x4025, 0x01 | 0x04 | 0x40);

        // First call: end_of_head flag triggers the spin-up delay.
        m.on_cpu_cycle();
        assert_eq!(m.delay, HEAD_RESET_DELAY);
        // Burn the spin-up.
        for _ in 0..HEAD_RESET_DELAY {
            m.on_cpu_cycle();
        }
        // Next tick: process first byte. Position should advance.
        m.on_cpu_cycle();
        assert_eq!(m.disk_position, 1);
        assert_eq!(m.delay, INTER_BYTE_DELAY);
    }

    #[test]
    fn disk_not_ready_reported_at_4032_until_scanning() {
        let mut m = Fds::new(make_cart());
        // Before motor on: disk-not-ready bit set.
        let v = m.cpu_read_ex(0x4032).unwrap();
        assert_eq!(v & 0x02, 0x02);
        // Start scan.
        m.cpu_write(0x4023, 0x01);
        m.cpu_write(0x4025, 0x01 | 0x04 | 0x40);
        for _ in 0..=HEAD_RESET_DELAY {
            m.on_cpu_cycle();
        }
        m.on_cpu_cycle(); // first byte read — sets scanning_disk
        let v2 = m.cpu_read_ex(0x4032).unwrap();
        assert_eq!(v2 & 0x02, 0, "disk ready once scanning");
    }

    #[test]
    fn motor_off_clears_scanning_state() {
        let mut m = Fds::new(make_cart());
        m.cpu_write(0x4023, 0x01);
        m.cpu_write(0x4025, 0x01 | 0x04 | 0x40); // motor on
        for _ in 0..=HEAD_RESET_DELAY + 1 {
            m.on_cpu_cycle();
        }
        assert!(m.scanning_disk);
        m.cpu_write(0x4025, 0x00); // motor off
        m.on_cpu_cycle();
        assert!(!m.scanning_disk);
        assert!(m.end_of_head);
    }

    #[test]
    fn ejected_disk_reports_no_disk_bits_at_4032() {
        let mut m = Fds::new(make_cart());
        m.disk_number = NO_DISK_INSERTED;
        let v = m.cpu_read_ex(0x4032).unwrap();
        assert_eq!(v & 0x07, 0x07, "bits 0,1,2 all set when ejected");
    }

    // ---- Mirroring ($4025 bit 3) ----

    #[test]
    fn mirroring_toggles_via_4025() {
        let mut m = Fds::new(make_cart());
        m.cpu_write(0x4023, 0x01);
        m.cpu_write(0x4025, 0x08); // H
        assert_eq!(m.mirroring(), Mirroring::Horizontal);
        m.cpu_write(0x4025, 0x00); // V
        assert_eq!(m.mirroring(), Mirroring::Vertical);
    }

    // ---- IRQ line OR ----

    #[test]
    fn irq_line_wires_timer_and_disk_together() {
        let mut m = Fds::new(make_cart());
        assert!(!m.irq_line());
        m.timer_irq_line = true;
        assert!(m.irq_line());
        m.timer_irq_line = false;
        m.disk_irq_line = true;
        assert!(m.irq_line());
    }

    // ---- $4026/$4033 ext-con round-trip ----

    #[test]
    fn ext_con_write_read_roundtrips() {
        let mut m = Fds::new(make_cart());
        m.cpu_write(0x4023, 0x01);
        m.cpu_write(0x4026, 0x5A);
        assert_eq!(m.cpu_read_ex(0x4033), Some(0x5A));
    }

    // ---- CRC accumulator ----

    #[test]
    fn update_crc_matches_ccitt_polynomial() {
        let mut m = Fds::new(make_cart());
        // Known value: starting from 0, feeding byte 0x01 should
        // produce `0x01`-then-shifted. Exact check: feed 0 byte,
        // accumulator stays 0.
        m.crc_accumulator = 0;
        m.update_crc(0);
        assert_eq!(m.crc_accumulator, 0);
        // Feed 0x01 from zero: XOR gives 1, then 8 shift+poly cycles.
        m.crc_accumulator = 0;
        m.update_crc(0x01);
        // Hand-traced ripple: 0x0001 → carry1 → 0x8408, 0x4204,
        // 0x2102, 0x1081 → carry1 → 0x8C48, 0x4624, 0x2312, 0x1189.
        assert_eq!(m.crc_accumulator, 0x1189);
    }
}
