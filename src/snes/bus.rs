// SPDX-License-Identifier: GPL-3.0-or-later
//! SNES system bus - the 24-bit address space the 5A22 sees.
//!
//! Phase 2d brought up the memory map and MEMSEL access speeds.
//! Phase 3a adds the CPU's own MMIO surface that the boot prelude
//! actually pokes:
//! - frame counter (scanline + master-cycle position) advanced on
//!   every bus access, used to drive vblank/hblank flags
//! - $4200 NMITIMEN, $4210 RDNMI (clear-on-read vblank flag),
//!   $4212 HVBJOY (vblank/hblank/auto-joypad-busy bits)
//! - $4202/$4203 unsigned 8x8 multiplier, $4204-$4206 unsigned
//!   16/8 divider, sharing $4214-$4217 result registers
//! - vblank-edge NMI delivered to the CPU via [`LoRomBus::take_nmi`]
//!
//! Per-region access speeds (SLOW/FAST/XSLOW) match the real bus
//! so cycle assertions against test ROMs don't drift. MEMSEL ($420D
//! bit 0) flips banks $80-$FF to FastROM when set; reset value is 0.

use crate::core::Region;
use crate::snes::cpu::bus::SnesBus;
use crate::snes::rom::{Cartridge, MapMode};

/// Total master-clock ticks each access region charges per byte.
/// "Slow" is the cart default; "fast" is FastROM-eligible cart half
/// when MEMSEL is set; "io" is the legacy serial-joypad strip.
const SLOW: u64 = 8;
const FAST: u64 = 6;
const XSLOW: u64 = 12;

/// Master cycles per scanline (NTSC and PAL share 1364 cycles per
/// "normal" line; the 1360-cycle long-line correction lands once we
/// model the 4-cycle/dot vs 6-cycle/dot distinction at PPU dot 323
/// and 327. Phase 3a uses a flat 1364 line which is good enough for
/// vblank-poll boot prelude exit).
const LINE_CYCLES: u64 = 1364;
const LINES_PER_FRAME_NTSC: u64 = 262;
const LINES_PER_FRAME_PAL: u64 = 312;
/// Vblank starts at line 225 (no overscan) and runs to wraparound.
/// Bit 7 of HVBJOY / RDNMI tracks this window.
const VBLANK_START_LINE: u64 = 225;

pub struct LoRomBus {
    /// Copy of the post-copier-header ROM payload.
    pub rom: Vec<u8>,
    /// 128 KiB WRAM at $7E:0000-$7F:FFFF (with low-8K mirror at
    /// $00-$3F:$0000-$1FFF and $80-$BF:$0000-$1FFF).
    pub wram: Vec<u8>,
    master: u64,
    memsel_fast: bool,
    /// Memory data register / open-bus latch. Updated on every
    /// access; reads from unmapped regions return its current value.
    open_bus: u8,
    region: Region,

    // ---- $4200 NMITIMEN / IRQ source bits ---------------------------
    /// Bit 7 of $4200: enable vblank NMI delivery to the CPU.
    nmi_enable: bool,
    /// Bits 5-4 of $4200: 00=off, 01=H, 10=V, 11=HV. We only model
    /// the "off" path in 3a; H/V timer IRQ lands in 3b.
    irq_mode: u8,
    /// Bit 0 of $4200: enable auto-joypad read at vblank start. Not
    /// modelled yet; we still latch it for read-back compatibility.
    auto_joypad_enable: bool,

    // ---- $4210 RDNMI / $4211 TIMEUP ---------------------------------
    /// Bit 7 of RDNMI - "vblank fired since last read." Set when
    /// the scanline counter crosses VBLANK_START_LINE; cleared on
    /// any read of $4210.
    rdnmi_set: bool,
    /// Bit 7 of TIMEUP - the H/V timer IRQ flag. Cleared on read.
    timeup_set: bool,

    // ---- Frame state ------------------------------------------------
    /// Last scanline observed. Used to detect the vblank-start edge.
    last_line: u64,
    /// Latched `in_vblank` bit (HVBJOY bit 7). True from
    /// scanline VBLANK_START_LINE through the last line of frame.
    in_vblank: bool,
    /// Number of vblank entries observed since power-on. The
    /// headless runner uses this to gate "is the boot prelude
    /// finished?" detection - the test bodies start running only
    /// after the first NMI fires.
    frame_count: u64,

    // ---- Pending interrupt levels exposed to the CPU ---------------
    nmi_pending: bool,
    irq_pending: bool,

    // ---- $4202/$4203 multiplier and $4204-$4206 divider -------------
    wrmpya: u8,
    rdmpy: u16,
    rddiv: u16,
    /// Latched dividend low/high; only consumed on $4206 write.
    dividend: u16,

    // ---- $2115-$2119 VRAM access (slim model for grading) ----------
    /// 64 KiB VRAM. Word-addressed in CPU space - word `w` lives at
    /// bytes `[w*2, w*2+1]`. Even with no PPU rendering yet, the
    /// test runner needs the VRAM contents so it can scan for
    /// "FAIL"/"PASS" tile codes to grade peter_lemon CPU tests.
    pub vram: Vec<u8>,
    /// $2116/$2117 word address. Only the low 15 bits matter (the
    /// real VRAM is 32K words). Writes auto-increment per VMAIN.
    vmaddr: u16,
    /// $2115 VMAIN. Bit 7 = increment-on-VMDATAH; bits 1-0 =
    /// increment amount (00:+1, 01:+32, 10:+128, 11:+128). Bits 3-2
    /// are address-translation modes; we don't model those yet
    /// (peter_lemon doesn't use them).
    vmain: u8,

    // ---- Slim BG / CGRAM registers for Mode-1 BG1 rendering --------
    /// $2105 BGMODE. Low 3 bits = BG mode; bit 3 = mode-1 BG3 high
    /// priority; bits 7-4 = per-BG character size (1 = 16x16). Phase
    /// 4a only consumes bits 2-0.
    bgmode: u8,
    /// $2107 BG1SC: bits 7-2 base in 1 KiB units; bits 1-0 size.
    bg1sc: u8,
    /// $210B BG12NBA: low nibble = BG1 char base in 4 KiB units.
    bg12nba: u8,
    /// $2100 INIDISP: bit 7 force-blank, bits 3-0 brightness (0-15).
    inidisp: u8,
    /// 256 15-bit CGRAM colors (BGR, bit 15 ignored).
    cgram: [u16; 256],
    cgadd: u8,
    /// CGDATA double-write phase: false = next byte is low (buffered),
    /// true = next byte is high (commits + increments CGADD).
    cgadd_phase: bool,
    cgdata_low: u8,

    // ---- APU port latches ($2140-$2143) ----------------------------
    //
    // Real hardware: dual-port latches between the host 5A22 and
    // the SMP. Each direction has its own register. The CPU's
    // **read** of $214x sees what the SMP last wrote there; the
    // CPU's **write** to $214x deposits a byte into a separate
    // latch the SMP reads. As of Phase 5b.2 the SMP runs alongside
    // the CPU and writes the boot signature itself during IPL
    // execution; we still pre-seed `smp_to_cpu` with `$AA $BB $00
    // $00` at construction so commercial games that poll the ports
    // *before* enough SMP cycles have elapsed don't hang.
    pub apu_ports: super::smp::state::ApuPorts,

    /// Eight DMA channels. $4300-$437F is laid out as 8 channels x
    /// 16 bytes; we collapse it into per-channel state so the
    /// transfer engine doesn't have to keep parsing addresses.
    dma: [DmaChannel; 8],

    /// Diagnostic counters - a write to a stubbed MMIO region bumps
    /// the matching tally so the headless test runner can see how
    /// far the boot sequence got even before we model the PPU.
    pub mmio_writes: MmioCounters,
}

/// One DMA channel's register file. $43n0-$43nA are the GP-DMA
/// fields; $43nB-$43nF are open bus / unused on real hardware.
/// Phase 4b only services general-purpose MDMA (writes to $420B);
/// HDMA per-scanline servicing is a Phase 4c follow-up.
#[derive(Debug, Default, Clone, Copy)]
struct DmaChannel {
    /// $43n0 DMAP. Bit 7 = direction (0 CPU->B, 1 B->CPU); bit 6 =
    /// HDMA-indirect; bit 5 = unused; bits 4-3 = step (00 +1, 01
    /// fixed, 10 -1, 11 fixed); bits 2-0 = mode.
    dmap: u8,
    /// $43n1 BBAD. Destination low-byte; high byte is always $21.
    bbad: u8,
    /// $43n2/3 A1T - source low/mid bytes (16-bit offset).
    a1t: u16,
    /// $43n4 A1B - source bank.
    a1b: u8,
    /// $43n5/6 DAS - transfer size / HDMA indirect address.
    das: u16,
    /// $43n7 DASB - HDMA indirect-bank scratch (unused for MDMA).
    dasb: u8,
    /// $43n8/9 A2A - HDMA table-address scratch.
    a2a: u16,
    /// $43nA NTRL - HDMA line counter / repeat scratch.
    ntrl: u8,
}

/// Per-register write counters. Stubbed MMIO regions always swallow
/// the write; this lets the runner observe boot-sequence progress
/// without a real PPU/DMA/IRQ implementation.
#[derive(Debug, Default, Clone)]
pub struct MmioCounters {
    pub ppu_b_bus: u64,        // $2100-$21FF
    pub apu_ports: u64,        // $2140-$2143 (mirrored to $217F)
    pub cpu_ctrl: u64,         // $4200-$420D
    pub cpu_status: u64,       // $4210-$421F (read-only, but counted)
    pub dma_regs: u64,         // $4300-$437F
    pub joypad_io: u64,        // $4016-$4017 (XSlow region)
    pub stz_to_unmapped: u64,  // unrecognised writes
}

impl LoRomBus {
    pub fn from_cartridge(cart: &Cartridge) -> Self {
        if cart.header.map_mode != MapMode::LoRom {
            // HiROM/ExHiROM mapping is a 4c follow-up. Fall back to
            // LoROM addressing so the host doesn't panic - the cart
            // will boot to a confused state but the windowed app
            // stays alive and the user can load a different ROM.
            eprintln!(
                "vibenes: SNES bus only supports LoROM in Phase 4b - {} will misaddress",
                cart.header.map_mode.label()
            );
        }
        let mut bus = Self::from_rom(cart.rom.clone());
        bus.region = cart.region;
        bus
    }

    pub fn from_rom(rom: Vec<u8>) -> Self {
        Self {
            rom,
            wram: vec![0; 128 * 1024],
            master: 0,
            memsel_fast: false,
            open_bus: 0,
            region: Region::Ntsc,
            nmi_enable: false,
            irq_mode: 0,
            auto_joypad_enable: false,
            rdnmi_set: false,
            timeup_set: false,
            last_line: 0,
            in_vblank: false,
            frame_count: 0,
            nmi_pending: false,
            irq_pending: false,
            wrmpya: 0xFF,
            rdmpy: 0,
            rddiv: 0,
            dividend: 0,
            vram: vec![0; 64 * 1024],
            vmaddr: 0,
            vmain: 0,
            bgmode: 0,
            bg1sc: 0,
            bg12nba: 0,
            inidisp: 0x80, // reset: force-blank set
            cgram: [0; 256],
            cgadd: 0,
            cgadd_phase: false,
            cgdata_low: 0,
            // SMP IPL boot pattern - $AA on port 0, $BB on port 1.
            // Games' WaitForAPUReady loop checks these to know the
            // SMP is alive. Without a real SMP we hardcode the
            // magic so commercial ROMs clear the handshake.
            apu_ports: super::smp::state::ApuPorts::RESET,
            dma: [DmaChannel::default(); 8],
            mmio_writes: MmioCounters::default(),
        }
    }

    fn vmain_increment(&self) -> u16 {
        match self.vmain & 0x03 {
            0 => 1,
            1 => 32,
            _ => 128,
        }
    }

    /// Apply VRAM auto-increment after a VMDATAL or VMDATAH write,
    /// gated by VMAIN bit 7. Per snes-cpu.md: bit 7 = "increment on
    /// VMDATAH (1) vs VMDATAL (0)".
    fn vram_advance(&mut self, on_high: bool) {
        let increment_on_high = self.vmain & 0x80 != 0;
        if increment_on_high == on_high {
            self.vmaddr = self.vmaddr.wrapping_add(self.vmain_increment());
        }
    }

    fn lines_per_frame(&self) -> u64 {
        match self.region {
            Region::Ntsc => LINES_PER_FRAME_NTSC,
            Region::Pal => LINES_PER_FRAME_PAL,
        }
    }

    /// Advance the master clock by `cycles` and update the frame
    /// state. On the rising edge into vblank we set RDNMI bit 7
    /// and (if NMI is enabled) raise the CPU NMI line. The CPU
    /// reads the line via [`LoRomBus::take_nmi`] at instruction
    /// boundaries.
    fn advance_master(&mut self, cycles: u64) {
        let prev = self.master;
        self.master = self.master.wrapping_add(cycles);
        let line_total = self.lines_per_frame() * LINE_CYCLES;
        let prev_line = (prev / LINE_CYCLES) % self.lines_per_frame();
        let cur_line = (self.master / LINE_CYCLES) % self.lines_per_frame();
        if cur_line != prev_line || (self.master / line_total) != (prev / line_total) {
            self.last_line = cur_line;
            // Vblank entry edge.
            if cur_line == VBLANK_START_LINE
                && (prev_line < VBLANK_START_LINE || prev_line > cur_line)
            {
                self.in_vblank = true;
                self.rdnmi_set = true;
                self.frame_count = self.frame_count.wrapping_add(1);
                if self.nmi_enable {
                    self.nmi_pending = true;
                }
            }
            // Vblank exit edge (line 0 of next frame).
            if cur_line == 0 && prev_line != 0 {
                self.in_vblank = false;
            }
        }
    }

    /// Pop the latched NMI level. Returns `true` once per vblank
    /// edge while NMI is enabled - the CPU clears its own internal
    /// `nmi_pending` after dispatching.
    pub fn take_nmi(&mut self) -> bool {
        let n = self.nmi_pending;
        self.nmi_pending = false;
        n
    }

    /// Pop the latched IRQ level. Phase 3a always returns false;
    /// real H/V timer + cart IRQ sources land in 3b.
    pub fn take_irq(&mut self) -> bool {
        let i = self.irq_pending;
        self.irq_pending = false;
        i
    }

    /// Current scanline (0..lines_per_frame()).
    pub fn scanline(&self) -> u64 {
        (self.master / LINE_CYCLES) % self.lines_per_frame()
    }

    pub fn in_vblank(&self) -> bool {
        self.in_vblank
    }

    pub fn frame_count(&self) -> u64 {
        self.frame_count
    }

    /// Inherent accessor for the bus's master clock counter. Mirrors
    /// the [`SnesBus::master_cycles`] trait method but is callable
    /// from outside the trait. Single source of truth for SMP catch-up
    /// scheduling and (eventually) PPU dot timing.
    pub fn master_cycles(&self) -> u64 {
        self.master
    }

    /// Master-cycle cost of one access at `addr`. Same shape every
    /// SNES emulator uses: WRAM and most cart space cost 8, B-bus
    /// MMIO and FastROM cost 6, the legacy joypad strip costs 12.
    pub fn region_speed(&self, addr: u32) -> u64 {
        let bank = (addr >> 16) as u8;
        let off = (addr & 0xFFFF) as u16;
        match bank {
            0x00..=0x3F | 0x80..=0xBF => match off {
                0x0000..=0x1FFF => SLOW,
                0x2000..=0x3FFF => FAST,
                0x4000..=0x41FF => XSLOW,
                0x4200..=0x5FFF => FAST,
                0x6000..=0x7FFF => SLOW,
                0x8000..=0xFFFF => {
                    if bank >= 0x80 && self.memsel_fast {
                        FAST
                    } else {
                        SLOW
                    }
                }
            },
            0x40..=0x7D => SLOW,
            0x7E..=0x7F => SLOW,
            0xC0..=0xFF => {
                if self.memsel_fast {
                    FAST
                } else {
                    SLOW
                }
            }
        }
    }

    /// Translate a 24-bit CPU address to a flat ROM offset under
    /// LoROM rules. Returns `None` for addresses that don't map to
    /// ROM (e.g., the WRAM bank, MMIO ranges, the $0000-$7FFF half
    /// of cart banks, or addresses past the cart's actual size).
    fn lorom_offset(&self, addr: u32) -> Option<usize> {
        let bank = (addr >> 16) as u8;
        let off = (addr & 0xFFFF) as u16;
        // Strip the FastROM mirror bit so $80-$FD aliases $00-$7D.
        let logical_bank = bank & 0x7F;
        let bank_off = match (logical_bank, off) {
            (0x00..=0x3F, 0x8000..=0xFFFF) => {
                (logical_bank as usize) * 0x8000 + (off as usize - 0x8000)
            }
            (0x40..=0x7D, _) => {
                // LoROM banks $40-$7D: $0000-$7FFF mirrors $8000-$FFFF
                // of the corresponding ROM bank. Treat both halves
                // as the same 32 KiB.
                let logical_off = if off < 0x8000 {
                    off as usize
                } else {
                    off as usize - 0x8000
                };
                ((logical_bank as usize - 0x40) + 0x40) * 0x8000 + logical_off
            }
            _ => return None,
        };
        if bank_off < self.rom.len() {
            Some(bank_off)
        } else {
            None
        }
    }

    fn wram_index(addr: u32) -> Option<usize> {
        let bank = (addr >> 16) as u8;
        let off = (addr & 0xFFFF) as u16;
        match bank {
            0x7E => Some(off as usize),
            0x7F => Some(0x10000 + off as usize),
            0x00..=0x3F | 0x80..=0xBF if off < 0x2000 => Some(off as usize),
            _ => None,
        }
    }

    fn read_internal(&mut self, addr: u32) -> u8 {
        let bank = (addr >> 16) as u8;
        let off = (addr & 0xFFFF) as u16;
        if let Some(i) = Self::wram_index(addr) {
            let v = self.wram[i];
            self.open_bus = v;
            return v;
        }
        match (bank, off) {
            // === CPU status MMIO ===
            // $4210 RDNMI - bit 7 = vblank-fired flag, clear-on-read;
            // bits 3-0 = CPU revision (5A22 returns 2). Bits 6-4 are
            // open bus.
            (0x00..=0x3F | 0x80..=0xBF, 0x4210) => {
                self.mmio_writes.cpu_status += 1;
                let mut v = (self.open_bus & 0x70) | 0x02;
                if self.rdnmi_set {
                    v |= 0x80;
                }
                self.rdnmi_set = false;
                self.open_bus = v;
                v
            }
            // $4211 TIMEUP - H/V timer IRQ flag, clear-on-read.
            (0x00..=0x3F | 0x80..=0xBF, 0x4211) => {
                self.mmio_writes.cpu_status += 1;
                let mut v = self.open_bus & 0x7F;
                if self.timeup_set {
                    v |= 0x80;
                }
                self.timeup_set = false;
                self.open_bus = v;
                v
            }
            // $4212 HVBJOY - bit 7 vblank, bit 6 hblank, bit 0
            // auto-joypad busy. Bits 5-1 are open bus.
            (0x00..=0x3F | 0x80..=0xBF, 0x4212) => {
                self.mmio_writes.cpu_status += 1;
                let mut v = self.open_bus & 0x3E;
                if self.in_vblank {
                    v |= 0x80;
                }
                // Hblank: dot 274..339 of the line. Approximate
                // from master_cycles position within the line; the
                // exact dot timing lands with the PPU. Phase 3a's
                // approximation: hblank during the last quarter.
                let dot_master = self.master % LINE_CYCLES;
                if dot_master >= 1096 {
                    v |= 0x40;
                }
                self.open_bus = v;
                v
            }
            // === Multiplier / divider results (shared $4214-$4217) ===
            (0x00..=0x3F | 0x80..=0xBF, 0x4214) => {
                self.mmio_writes.cpu_status += 1;
                let v = self.rddiv as u8;
                self.open_bus = v;
                v
            }
            (0x00..=0x3F | 0x80..=0xBF, 0x4215) => {
                self.mmio_writes.cpu_status += 1;
                let v = (self.rddiv >> 8) as u8;
                self.open_bus = v;
                v
            }
            (0x00..=0x3F | 0x80..=0xBF, 0x4216) => {
                self.mmio_writes.cpu_status += 1;
                let v = self.rdmpy as u8;
                self.open_bus = v;
                v
            }
            (0x00..=0x3F | 0x80..=0xBF, 0x4217) => {
                self.mmio_writes.cpu_status += 1;
                let v = (self.rdmpy >> 8) as u8;
                self.open_bus = v;
                v
            }
            // CPU control + JOY ports (not yet modelled): open bus.
            (0x00..=0x3F | 0x80..=0xBF, 0x4218..=0x421F) => {
                self.mmio_writes.cpu_status += 1;
                self.open_bus
            }
            (0x00..=0x3F | 0x80..=0xBF, 0x2100..=0x213F) => self.open_bus,
            (0x00..=0x3F | 0x80..=0xBF, 0x2140..=0x217F) => {
                let v = self.apu_ports.cpu_read((off as usize) & 3);
                self.open_bus = v;
                v
            }
            (0x00..=0x3F | 0x80..=0xBF, 0x2180..=0x21FF) => self.open_bus,
            (0x00..=0x3F | 0x80..=0xBF, 0x4200..=0x420F) => self.open_bus,
            (0x00..=0x3F | 0x80..=0xBF, 0x4300..=0x437F) => self.open_bus,
            (0x00..=0x3F | 0x80..=0xBF, 0x4016..=0x4017) => self.open_bus,
            _ => match self.lorom_offset(addr) {
                Some(o) => {
                    let v = self.rom[o];
                    self.open_bus = v;
                    v
                }
                None => self.open_bus,
            },
        }
    }

    fn write_internal(&mut self, addr: u32, value: u8) {
        let bank = (addr >> 16) as u8;
        let off = (addr & 0xFFFF) as u16;
        self.open_bus = value;
        if let Some(i) = Self::wram_index(addr) {
            self.wram[i] = value;
            return;
        }
        match (bank, off) {
            // === Slim BG / CGRAM model for Mode-1 BG1 rendering ===
            (0x00..=0x3F | 0x80..=0xBF, 0x2100) => {
                self.mmio_writes.ppu_b_bus += 1;
                self.inidisp = value;
            }
            (0x00..=0x3F | 0x80..=0xBF, 0x2105) => {
                self.mmio_writes.ppu_b_bus += 1;
                self.bgmode = value;
            }
            (0x00..=0x3F | 0x80..=0xBF, 0x2107) => {
                self.mmio_writes.ppu_b_bus += 1;
                self.bg1sc = value;
            }
            (0x00..=0x3F | 0x80..=0xBF, 0x210B) => {
                self.mmio_writes.ppu_b_bus += 1;
                self.bg12nba = value;
            }
            (0x00..=0x3F | 0x80..=0xBF, 0x2121) => {
                self.mmio_writes.ppu_b_bus += 1;
                self.cgadd = value;
                self.cgadd_phase = false;
            }
            (0x00..=0x3F | 0x80..=0xBF, 0x2122) => {
                self.mmio_writes.ppu_b_bus += 1;
                if !self.cgadd_phase {
                    self.cgdata_low = value;
                    self.cgadd_phase = true;
                } else {
                    let word = (self.cgdata_low as u16) | ((value as u16) << 8);
                    self.cgram[self.cgadd as usize] = word & 0x7FFF;
                    self.cgadd = self.cgadd.wrapping_add(1);
                    self.cgadd_phase = false;
                }
            }
            // === Slim VRAM model for headless grading ===
            (0x00..=0x3F | 0x80..=0xBF, 0x2115) => {
                self.mmio_writes.ppu_b_bus += 1;
                self.vmain = value;
            }
            (0x00..=0x3F | 0x80..=0xBF, 0x2116) => {
                self.mmio_writes.ppu_b_bus += 1;
                self.vmaddr = (self.vmaddr & 0xFF00) | value as u16;
            }
            (0x00..=0x3F | 0x80..=0xBF, 0x2117) => {
                self.mmio_writes.ppu_b_bus += 1;
                self.vmaddr = (self.vmaddr & 0x00FF) | ((value as u16) << 8);
            }
            (0x00..=0x3F | 0x80..=0xBF, 0x2118) => {
                self.mmio_writes.ppu_b_bus += 1;
                let off = ((self.vmaddr as usize) << 1) & (self.vram.len() - 1);
                self.vram[off] = value;
                self.vram_advance(false);
            }
            (0x00..=0x3F | 0x80..=0xBF, 0x2119) => {
                self.mmio_writes.ppu_b_bus += 1;
                let off = (((self.vmaddr as usize) << 1) | 1) & (self.vram.len() - 1);
                self.vram[off] = value;
                self.vram_advance(true);
            }
            (0x00..=0x3F | 0x80..=0xBF, 0x2100..=0x213F) => {
                self.mmio_writes.ppu_b_bus += 1;
            }
            (0x00..=0x3F | 0x80..=0xBF, 0x2140..=0x217F) => {
                self.mmio_writes.apu_ports += 1;
                let port = (off as usize) & 3;
                // CPU writes deposit into the CPU->SMP half of the
                // mailbox. The SMP picks them up on its next read of
                // `$F4 + port`. Replies from the SMP land in the
                // SMP->CPU half via the integrated SMP bus.
                self.apu_ports.cpu_write(port, value);
            }
            (0x00..=0x3F | 0x80..=0xBF, 0x2180..=0x21FF) => {
                self.mmio_writes.ppu_b_bus += 1;
            }
            // === $4200 NMITIMEN ===
            (0x00..=0x3F | 0x80..=0xBF, 0x4200) => {
                self.mmio_writes.cpu_ctrl += 1;
                let prev_enable = self.nmi_enable;
                self.nmi_enable = value & 0x80 != 0;
                self.irq_mode = (value >> 4) & 0x03;
                self.auto_joypad_enable = value & 0x01 != 0;
                // If NMI gets enabled mid-vblank with the latched
                // RDNMI flag still set, the rising edge on the
                // enable line raises NMI immediately. Mirrors the
                // hardware behaviour (Anomie's NMI quirks doc).
                if !prev_enable && self.nmi_enable && self.rdnmi_set {
                    self.nmi_pending = true;
                }
            }
            // === $4202/$4203 unsigned 8x8 multiplier ===
            (0x00..=0x3F | 0x80..=0xBF, 0x4202) => {
                self.mmio_writes.cpu_ctrl += 1;
                self.wrmpya = value;
            }
            (0x00..=0x3F | 0x80..=0xBF, 0x4203) => {
                self.mmio_writes.cpu_ctrl += 1;
                // Writing factor B starts the multiply. Real
                // hardware advertises an 8-cycle latency on the
                // result; we compute immediately - software that
                // races the result either waits enough cycles
                // anyway or doesn't notice.
                self.rdmpy = (self.wrmpya as u16).wrapping_mul(value as u16);
                // Per Anomie's "65816 quirks": writing to WRMPYB
                // also clears the divider quotient (RDDIV) and
                // sets RDMPY to A * B. We replicate.
                self.rddiv = value as u16;
            }
            // === $4204-$4206 unsigned 16/8 divider ===
            (0x00..=0x3F | 0x80..=0xBF, 0x4204) => {
                self.mmio_writes.cpu_ctrl += 1;
                self.dividend = (self.dividend & 0xFF00) | value as u16;
            }
            (0x00..=0x3F | 0x80..=0xBF, 0x4205) => {
                self.mmio_writes.cpu_ctrl += 1;
                self.dividend = (self.dividend & 0x00FF) | ((value as u16) << 8);
            }
            (0x00..=0x3F | 0x80..=0xBF, 0x4206) => {
                self.mmio_writes.cpu_ctrl += 1;
                if value == 0 {
                    self.rddiv = 0xFFFF;
                    self.rdmpy = self.dividend;
                } else {
                    self.rddiv = self.dividend / value as u16;
                    self.rdmpy = self.dividend % value as u16;
                }
            }
            // === H/V timer targets - latched, IRQ delivery in 3b ===
            (0x00..=0x3F | 0x80..=0xBF, 0x4207..=0x420A) => {
                self.mmio_writes.cpu_ctrl += 1;
            }
            // === $420B MDMAEN / $420C HDMAEN / $420D MEMSEL ===
            (0x00..=0x3F | 0x80..=0xBF, 0x420B) => {
                self.mmio_writes.cpu_ctrl += 1;
                self.run_mdma(value);
            }
            (0x00..=0x3F | 0x80..=0xBF, 0x420C) => {
                self.mmio_writes.cpu_ctrl += 1;
                // HDMA enable - latched but not serviced in 4b.
            }
            (0x00..=0x3F | 0x80..=0xBF, 0x420D) => {
                self.mmio_writes.cpu_ctrl += 1;
                self.memsel_fast = value & 1 != 0;
            }
            (0x00..=0x3F | 0x80..=0xBF, 0x420E..=0x420F) => {
                self.mmio_writes.cpu_ctrl += 1;
            }
            (0x00..=0x3F | 0x80..=0xBF, 0x4300..=0x437F) => {
                self.mmio_writes.dma_regs += 1;
                let chan = ((off - 0x4300) >> 4) as usize;
                let reg = (off - 0x4300) & 0x0F;
                let c = &mut self.dma[chan];
                match reg {
                    0x0 => c.dmap = value,
                    0x1 => c.bbad = value,
                    0x2 => c.a1t = (c.a1t & 0xFF00) | value as u16,
                    0x3 => c.a1t = (c.a1t & 0x00FF) | ((value as u16) << 8),
                    0x4 => c.a1b = value,
                    0x5 => c.das = (c.das & 0xFF00) | value as u16,
                    0x6 => c.das = (c.das & 0x00FF) | ((value as u16) << 8),
                    0x7 => c.dasb = value,
                    0x8 => c.a2a = (c.a2a & 0xFF00) | value as u16,
                    0x9 => c.a2a = (c.a2a & 0x00FF) | ((value as u16) << 8),
                    0xA => c.ntrl = value,
                    _ => {} // $43nB-$43nF unused
                }
            }
            (0x00..=0x3F | 0x80..=0xBF, 0x4016..=0x4017) => {
                self.mmio_writes.joypad_io += 1;
            }
            _ => {
                self.mmio_writes.stz_to_unmapped += 1;
            }
        }
    }

    pub fn memsel_fast(&self) -> bool {
        self.memsel_fast
    }

    pub fn open_bus(&self) -> u8 {
        self.open_bus
    }

    /// Run general-purpose DMA on every channel selected by the
    /// MDMAEN bitmask. Per channel: copy DAS bytes from
    /// (a1b:a1t) to ($2100 + bbad)+pattern, applying the source-
    /// step rule (DMAP bits 4-3) and the per-mode write-pattern
    /// (DMAP bits 2-0). DAS = 0 means transfer 65536 bytes.
    /// Algorithm shape paraphrased from Mesen2 SnesDmaController -
    /// per-mode write patterns from the snes-cpu nes-expert
    /// reference and snes.nesdev.org.
    fn run_mdma(&mut self, mask: u8) {
        for chan in 0..8 {
            if mask & (1 << chan) == 0 {
                continue;
            }
            let direction = self.dma[chan].dmap & 0x80 != 0;
            let step: i32 = match (self.dma[chan].dmap >> 3) & 0x03 {
                0 => 1,
                2 => -1,
                _ => 0, // 1 or 3 = fixed
            };
            let mode = self.dma[chan].dmap & 0x07;
            let pattern: &[u8] = match mode {
                0 => &[0],
                1 => &[0, 1],
                2 | 6 => &[0, 0],
                3 | 7 => &[0, 0, 1, 1],
                4 => &[0, 1, 2, 3],
                5 => &[0, 1, 0, 1],
                _ => &[0],
            };
            let mut bytes_remaining = if self.dma[chan].das == 0 {
                0x10000usize
            } else {
                self.dma[chan].das as usize
            };
            let mut pi = 0usize;
            while bytes_remaining > 0 {
                let bbad = self.dma[chan].bbad;
                let bus_dest = 0x002100 + (bbad as u32 + pattern[pi] as u32);
                let src = ((self.dma[chan].a1b as u32) << 16)
                    | self.dma[chan].a1t as u32;
                if !direction {
                    let v = self.read_internal(src);
                    self.write_internal(bus_dest, v);
                } else {
                    let v = self.read_internal(bus_dest);
                    self.write_internal(src, v);
                }
                self.dma[chan].a1t = (self.dma[chan].a1t as i32).wrapping_add(step) as u16;
                self.dma[chan].das = self.dma[chan].das.wrapping_sub(1);
                bytes_remaining -= 1;
                pi = (pi + 1) % pattern.len();
                // Cycle-cost approximation: 8 master cycles per
                // byte plus 8 cycles channel-init overhead. Real
                // hardware bills exact per-region speeds; this is
                // close enough for now to keep the frame counter
                // honest.
                self.advance_master(8);
            }
            self.advance_master(8); // channel-init overhead
        }
    }

    pub fn ppu_state_summary(&self) -> String {
        format!(
            "INIDISP={:02X} BGMODE={:02X} BG1SC={:02X} BG12NBA={:02X} cgram[0..4]={:04X} {:04X} {:04X} {:04X}",
            self.inidisp, self.bgmode, self.bg1sc, self.bg12nba,
            self.cgram[0], self.cgram[1], self.cgram[2], self.cgram[3]
        )
    }

    /// Render a 256x224 RGBA frame from the current PPU register
    /// state, VRAM, and CGRAM. Phase 4a only handles Mode 1 BG1
    /// (4bpp tiles, 32x32 tilemap, no scroll, no flip, no priority,
    /// no sprites). When INIDISP force-blank is set the frame is
    /// black. Anything outside the supported subset is fall-through
    /// black so we don't generate garbage.
    pub fn render_frame(&self, out: &mut [u8]) {
        debug_assert_eq!(out.len(), 256 * 224 * 4);
        // INIDISP force-blank ($2100 b7) -> black frame.
        if self.inidisp & 0x80 != 0 {
            for px in out.chunks_exact_mut(4) {
                px.copy_from_slice(&[0, 0, 0, 0xFF]);
            }
            return;
        }

        let bgmode = self.bgmode & 0x07;
        // Phase 4a only renders Mode 0 / Mode 1 BG1 as 4bpp (we
        // promote Mode 0's 2bpp to 4bpp by zero-extending high
        // planes). Anything else: black.
        let bg1_bpp = match bgmode {
            0 => 2,
            1 => 4,
            _ => {
                for px in out.chunks_exact_mut(4) {
                    px.copy_from_slice(&[0, 0, 0, 0xFF]);
                }
                return;
            }
        };

        let tilemap_word_base = ((self.bg1sc >> 2) as usize) * 0x400;
        let tilemap_byte_base = (tilemap_word_base & 0x7FFF) << 1;
        let char_word_base = ((self.bg12nba & 0x0F) as usize) * 0x1000;
        let char_byte_base = (char_word_base & 0x7FFF) << 1;
        let bytes_per_tile = if bg1_bpp == 4 { 32 } else { 16 };
        let brightness = (self.inidisp & 0x0F) as u32;

        for y in 0..224 {
            for x in 0..256 {
                let tx = x / 8;
                let ty = y / 8;
                let entry_off = (tilemap_byte_base + (ty * 32 + tx) * 2) & 0xFFFE;
                let entry =
                    (self.vram[entry_off] as u16) | ((self.vram[entry_off + 1] as u16) << 8);
                let tile_idx = entry & 0x3FF;
                let palette = ((entry >> 10) & 0x07) as usize;
                let h_flip = entry & 0x4000 != 0;
                let v_flip = entry & 0x8000 != 0;

                let row_in_tile = if v_flip { 7 - (y % 8) } else { y % 8 };
                let col_in_tile = if h_flip { 7 - (x % 8) } else { x % 8 };
                let tile_off =
                    (char_byte_base + (tile_idx as usize) * bytes_per_tile) & (self.vram.len() - 1);
                let bit = 7 - col_in_tile;

                let p0 = (self.vram[(tile_off + row_in_tile * 2) & (self.vram.len() - 1)] >> bit)
                    & 1;
                let p1 = (self.vram[(tile_off + row_in_tile * 2 + 1) & (self.vram.len() - 1)]
                    >> bit)
                    & 1;
                let mut pixel_index = (p0 | (p1 << 1)) as u8;
                if bg1_bpp == 4 {
                    let p2 = (self.vram[(tile_off + 16 + row_in_tile * 2) & (self.vram.len() - 1)]
                        >> bit)
                        & 1;
                    let p3 = (self.vram
                        [(tile_off + 16 + row_in_tile * 2 + 1) & (self.vram.len() - 1)]
                        >> bit)
                        & 1;
                    pixel_index |= (p2 << 2) | (p3 << 3);
                }

                let cg = if pixel_index == 0 {
                    self.cgram[0] // backdrop
                } else {
                    let palette_size = if bg1_bpp == 4 { 16 } else { 4 };
                    self.cgram[(palette * palette_size + pixel_index as usize) & 0xFF]
                };
                let r5 = (cg & 0x1F) as u32;
                let g5 = ((cg >> 5) & 0x1F) as u32;
                let b5 = ((cg >> 10) & 0x1F) as u32;
                // Apply INIDISP brightness scaling.
                let r = ((r5 << 3) | (r5 >> 2)) * (brightness + 1) / 16;
                let g = ((g5 << 3) | (g5 >> 2)) * (brightness + 1) / 16;
                let b = ((b5 << 3) | (b5 >> 2)) * (brightness + 1) / 16;

                let off = (y * 256 + x) * 4;
                out[off] = r as u8;
                out[off + 1] = g as u8;
                out[off + 2] = b as u8;
                out[off + 3] = 0xFF;
            }
        }
    }
}

impl SnesBus for LoRomBus {
    fn read(&mut self, addr: u32) -> u8 {
        let speed = self.region_speed(addr);
        self.advance_master(speed);
        self.read_internal(addr)
    }

    fn write(&mut self, addr: u32, value: u8) {
        let speed = self.region_speed(addr);
        self.advance_master(speed);
        self.write_internal(addr, value);
    }

    fn idle(&mut self) {
        self.advance_master(FAST);
    }

    fn master_cycles(&self) -> u64 {
        self.master
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill_rom() -> Vec<u8> {
        let mut rom = vec![0; 0x8000];
        // Reset vector at $7FFC -> $8000, fetched at $00:FFFC-D
        rom[0x7FFC] = 0x00;
        rom[0x7FFD] = 0x80;
        rom[0x0000] = 0xEA; // NOP at the reset target
        rom
    }

    #[test]
    fn lorom_reset_vector_visible_at_00ffc_in_emulation() {
        let mut bus = LoRomBus::from_rom(fill_rom());
        assert_eq!(bus.read(0x00FFFC), 0x00);
        assert_eq!(bus.read(0x00FFFD), 0x80);
        assert_eq!(bus.read(0x008000), 0xEA);
    }

    #[test]
    fn wram_low_mirror_round_trips() {
        let mut bus = LoRomBus::from_rom(fill_rom());
        bus.write(0x000400, 0xAB);
        assert_eq!(bus.read(0x7E0400), 0xAB);
        // $80-$BF half mirrors the same low-WRAM window
        bus.write(0x800500, 0xCD);
        assert_eq!(bus.read(0x7E0500), 0xCD);
    }

    #[test]
    fn full_wram_visible_in_7e_7f() {
        let mut bus = LoRomBus::from_rom(fill_rom());
        bus.write(0x7F1234, 0x77);
        assert_eq!(bus.read(0x7F1234), 0x77);
    }

    #[test]
    fn region_speed_picks_fast_xslow_slow() {
        let bus = LoRomBus::from_rom(fill_rom());
        assert_eq!(bus.region_speed(0x000000), SLOW);
        assert_eq!(bus.region_speed(0x002100), FAST);
        assert_eq!(bus.region_speed(0x004016), XSLOW);
        assert_eq!(bus.region_speed(0x004200), FAST);
        assert_eq!(bus.region_speed(0x008000), SLOW);
        assert_eq!(bus.region_speed(0x808000), SLOW); // memsel still 0
    }

    #[test]
    fn memsel_flips_high_bank_to_fast() {
        let mut bus = LoRomBus::from_rom(fill_rom());
        bus.write(0x00420D, 0x01); // MEMSEL = FastROM
        assert!(bus.memsel_fast());
        assert_eq!(bus.region_speed(0x808000), FAST);
        assert_eq!(bus.region_speed(0xC08000), FAST);
        // Banks $00-$7D are unaffected.
        assert_eq!(bus.region_speed(0x008000), SLOW);
    }

    #[test]
    fn mmio_swallows_writes_and_counts() {
        let mut bus = LoRomBus::from_rom(fill_rom());
        bus.write(0x002100, 0x80); // INIDISP force-blank
        bus.write(0x004200, 0x81); // NMITIMEN
        bus.write(0x004310, 0x09); // DMA channel 1 control
        assert_eq!(bus.mmio_writes.ppu_b_bus, 1);
        assert!(bus.mmio_writes.cpu_ctrl >= 1);
        assert_eq!(bus.mmio_writes.dma_regs, 1);
    }

    #[test]
    fn multiplier_8x8_and_div_by_zero() {
        let mut bus = LoRomBus::from_rom(fill_rom());
        // $FF * $FF = $FE01.
        bus.write(0x004202, 0xFF);
        bus.write(0x004203, 0xFF);
        let lo = bus.read(0x004216);
        let hi = bus.read(0x004217);
        assert_eq!(((hi as u16) << 8) | lo as u16, 0xFE01);

        // Divide by zero: quotient $FFFF, remainder = original dividend.
        bus.write(0x004204, 0x34);
        bus.write(0x004205, 0x12);
        bus.write(0x004206, 0x00);
        let qlo = bus.read(0x004214);
        let qhi = bus.read(0x004215);
        let rlo = bus.read(0x004216);
        let rhi = bus.read(0x004217);
        assert_eq!(((qhi as u16) << 8) | qlo as u16, 0xFFFF);
        assert_eq!(((rhi as u16) << 8) | rlo as u16, 0x1234);

        // 16/8 normal: $1234 / $10 = q=$0123, r=$04.
        bus.write(0x004204, 0x34);
        bus.write(0x004205, 0x12);
        bus.write(0x004206, 0x10);
        let qlo = bus.read(0x004214);
        let qhi = bus.read(0x004215);
        let rlo = bus.read(0x004216);
        assert_eq!(((qhi as u16) << 8) | qlo as u16, 0x0123);
        assert_eq!(rlo, 0x04);
    }

    #[test]
    fn vblank_edge_sets_rdnmi_and_clears_on_read() {
        let mut bus = LoRomBus::from_rom(fill_rom());
        // Burn cycles past line 225 to trigger the vblank edge.
        // 225 lines * 1364 master cycles = 306900 cycles before the
        // edge; one more line crosses it. Use writes to WRAM to
        // advance.
        for _ in 0..40_000 {
            bus.write(0x000400, 0x00); // 8 master cycles per write
        }
        assert!(bus.in_vblank());
        let v = bus.read(0x004210);
        assert_eq!(v & 0x80, 0x80);
        // Second read clears the flag.
        let v = bus.read(0x004210);
        assert_eq!(v & 0x80, 0x00);
    }

    #[test]
    fn nmi_raises_when_enable_set_and_vblank_pending() {
        let mut bus = LoRomBus::from_rom(fill_rom());
        // Enable NMI before vblank.
        bus.write(0x004200, 0x80);
        for _ in 0..40_000 {
            bus.write(0x000400, 0x00);
        }
        assert!(bus.take_nmi(), "vblank entry should raise NMI when NMITIMEN bit 7 is set");
        // Subsequent take_nmi returns false until the next vblank.
        assert!(!bus.take_nmi());
    }
}
