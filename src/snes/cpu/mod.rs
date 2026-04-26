// SPDX-License-Identifier: GPL-3.0-or-later
//! Ricoh 5A22 CPU core (WDC 65C816 with embedded DMA / multiplier /
//! divider). This module owns the architectural state and the
//! opcode dispatch; the bus side (memory map, MMIO, DMA, IRQ
//! sources) lives in [`crate::snes::bus`] and is plugged in via the
//! [`SnesBus`] trait so the CPU can be unit-tested against a flat-
//! memory stub without dragging the rest of the system in.
//!
//! ## Phase 2 status
//!
//! This commit lands the foundation: full register/flag/mode model,
//! reset sequence, all 24 addressing modes, mode-switch ops
//! (REP/SEP/XCE), every transfer/load/store/branch/jump/stack op,
//! INC/DEC, CLC/SEC etc., NOP/WDM/RTI. Subsequent commits expand
//! to AND/ORA/EOR/BIT/ASL/LSR/ROL/ROR/CMP/CPX/CPY/ADC/SBC,
//! then BCD, MVN/MVP, WAI/STP, BRK/COP, then cycle-exact
//! penalties (page cross, `D & $FF != 0`).
//!
//! References (paraphrased; clean-room-adjacent porting per
//! project policy): nes-expert SNES CPU reference at
//! `~/.claude/skills/nes-expert/reference/snes-cpu.md`,
//! snes.nesdev.org wiki (65C816, CPU_vectors, Errata,
//! 65c816_for_6502_developers, MVN_and_MVP_block_copy), Mesen2
//! `Core/SNES/SnesCpu*.h` for the dispatch shape.

pub mod bus;

use bus::SnesBus;

/// CPU operating mode. RESET always enters [`Mode::Emulation`]; the
/// canonical "go native" sequence is `CLC; XCE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 6502 compatibility. A is 8-bit, X/Y are 8-bit, S is forced to
    /// page 1 ($01xx), stack wraps in $0100-$01FF, vectors at
    /// $FFFA-$FFFF.
    Emulation,
    /// Full 65C816. Width of A and X/Y is selectable via the m and x
    /// flags in P; S is 16-bit and can roam anywhere in bank 0.
    Native,
}

/// Status register P. Stored as discrete bools rather than packed
/// bits so the dispatch reads cleanly; `pack` / `unpack` convert at
/// the PHP/PLP/RTI boundaries.
#[derive(Debug, Clone, Copy, Default)]
pub struct Status {
    pub n: bool,
    pub v: bool,
    /// Memory/accumulator width. `true` = 8-bit, `false` = 16-bit.
    /// Always `true` in emulation mode.
    pub m: bool,
    /// Index width. `true` = 8-bit X/Y, `false` = 16-bit. Always
    /// `true` in emulation mode. Setting this from 0 -> 1 also
    /// physically zeroes the high bytes of X and Y (errata).
    pub x: bool,
    pub d: bool,
    pub i: bool,
    pub z: bool,
    pub c: bool,
    /// In emulation mode the bit-4 position of P is the "B" (break)
    /// flag, only ever observable on the stack (PHP push, BRK push).
    /// Native mode treats it as the `x` index-width flag.
    pub b: bool,
}

impl Status {
    pub fn pack(&self, mode: Mode) -> u8 {
        let mut p = 0u8;
        if self.n {
            p |= 0x80;
        }
        if self.v {
            p |= 0x40;
        }
        match mode {
            Mode::Native => {
                if self.m {
                    p |= 0x20;
                }
                if self.x {
                    p |= 0x10;
                }
            }
            Mode::Emulation => {
                // Bit 5 always reads as 1 in emulation mode; bit 4
                // is the B flag (set on PHP/BRK pushes, cleared on
                // IRQ/NMI hardware pushes).
                p |= 0x20;
                if self.b {
                    p |= 0x10;
                }
            }
        }
        if self.d {
            p |= 0x08;
        }
        if self.i {
            p |= 0x04;
        }
        if self.z {
            p |= 0x02;
        }
        if self.c {
            p |= 0x01;
        }
        p
    }

    pub fn unpack(&mut self, p: u8, mode: Mode) {
        self.n = p & 0x80 != 0;
        self.v = p & 0x40 != 0;
        match mode {
            Mode::Native => {
                self.m = p & 0x20 != 0;
                self.x = p & 0x10 != 0;
                // B flag is meaningless in native; leave whatever
                // PHP/BRK previously stashed.
            }
            Mode::Emulation => {
                // m / x stay forced-on in emulation mode regardless
                // of what was pulled. The B flag tracks the pulled
                // bit-4.
                self.m = true;
                self.x = true;
                self.b = p & 0x10 != 0;
            }
        }
        self.d = p & 0x08 != 0;
        self.i = p & 0x04 != 0;
        self.z = p & 0x02 != 0;
        self.c = p & 0x01 != 0;
    }
}

/// 65C816 state. C is the full 16-bit accumulator; the public `a()`
/// and `b()` accessors read its low/high halves. X and Y are 16-bit;
/// when `P.x = 1` the high bytes are physically zero (we re-enforce
/// this on every SEP/REP/XCE that toggles x).
#[derive(Debug, Clone, Copy)]
pub struct Cpu {
    pub c: u16,
    pub x: u16,
    pub y: u16,
    pub d: u16,
    pub s: u16,
    pub pc: u16,
    pub pbr: u8,
    pub dbr: u8,
    pub p: Status,
    pub mode: Mode,
    /// Outstanding interrupt requests. Polled at the boundary
    /// between instructions. NMI is edge-triggered (set externally,
    /// auto-cleared on dispatch); IRQ is level-triggered (the bus
    /// owner re-asserts every step until cleared).
    pub nmi_pending: bool,
    pub irq_pending: bool,
    /// `true` while the CPU is halted by `STP`. Only `/RESET`
    /// clears this.
    pub stopped: bool,
    /// `true` while the CPU is in `WAI`. Cleared when any IRQ /
    /// NMI is asserted, even if I=1 (in which case the handler is
    /// not entered but execution resumes after the WAI).
    pub waiting: bool,
}

impl Cpu {
    pub fn new() -> Self {
        Self {
            c: 0,
            x: 0,
            y: 0,
            d: 0,
            s: 0x01FF,
            pc: 0,
            pbr: 0,
            dbr: 0,
            p: Status {
                m: true,
                x: true,
                i: true,
                ..Status::default()
            },
            mode: Mode::Emulation,
            nmi_pending: false,
            irq_pending: false,
            stopped: false,
            waiting: false,
        }
    }

    pub fn a(&self) -> u8 {
        self.c as u8
    }

    pub fn b(&self) -> u8 {
        (self.c >> 8) as u8
    }

    pub fn set_a(&mut self, v: u8) {
        self.c = (self.c & 0xFF00) | v as u16;
    }

    /// Reset (cold boot or `/RESET`). Forces emulation mode, sets
    /// I and clears D, points S into page 1, and loads PC from
    /// $00:FFFC. Other registers are left in their power-on
    /// indeterminate state for everything except S - games that
    /// care about WRAM / register init re-initialise themselves.
    pub fn reset(&mut self, bus: &mut impl SnesBus) {
        self.mode = Mode::Emulation;
        self.p.m = true;
        self.p.x = true;
        self.p.d = false;
        self.p.i = true;
        self.p.b = true;
        self.pbr = 0;
        self.dbr = 0;
        self.d = 0x0000;
        self.s = 0x01FF;
        self.x &= 0x00FF;
        self.y &= 0x00FF;
        self.stopped = false;
        self.waiting = false;
        self.nmi_pending = false;
        self.irq_pending = false;
        let lo = bus.read(0x00FFFC) as u16;
        let hi = bus.read(0x00FFFD) as u16;
        self.pc = (hi << 8) | lo;
    }

    /// Drive one instruction. Returns the opcode that ran (handy
    /// for tests + tracing). Halts (STP) and waits (WAI) advance
    /// the clock without retiring an instruction; in those states
    /// the caller should still tick the bus owner.
    pub fn step(&mut self, bus: &mut impl SnesBus) -> u8 {
        if self.stopped {
            bus.idle();
            return 0xDB; // STP itself
        }
        if self.waiting {
            if self.nmi_pending || self.irq_pending {
                self.waiting = false;
                if self.p.i && !self.nmi_pending {
                    // I=1: leave the IRQ pending; the program
                    // resumes after the WAI.
                } else {
                    return self.service_interrupt(bus);
                }
            } else {
                bus.idle();
                return 0xCB; // WAI itself
            }
        }
        if self.nmi_pending {
            return self.service_interrupt(bus);
        }
        if self.irq_pending && !self.p.i {
            return self.service_interrupt(bus);
        }
        let opcode = self.fetch_op(bus);
        self.dispatch(opcode, bus);
        opcode
    }

    fn fetch_op(&mut self, bus: &mut impl SnesBus) -> u8 {
        let pc = self.pbr_pc();
        self.pc = self.pc.wrapping_add(1);
        bus.read(pc)
    }

    fn fetch_u8(&mut self, bus: &mut impl SnesBus) -> u8 {
        let pc = self.pbr_pc();
        self.pc = self.pc.wrapping_add(1);
        bus.read(pc)
    }

    fn fetch_u16(&mut self, bus: &mut impl SnesBus) -> u16 {
        let lo = self.fetch_u8(bus) as u16;
        let hi = self.fetch_u8(bus) as u16;
        (hi << 8) | lo
    }

    fn fetch_u24(&mut self, bus: &mut impl SnesBus) -> u32 {
        let lo = self.fetch_u8(bus) as u32;
        let mid = self.fetch_u8(bus) as u32;
        let hi = self.fetch_u8(bus) as u32;
        (hi << 16) | (mid << 8) | lo
    }

    /// 24-bit "current instruction fetch address" (PBR : PC) as a
    /// flat u32 for the bus.
    fn pbr_pc(&self) -> u32 {
        ((self.pbr as u32) << 16) | self.pc as u32
    }

    fn read8(&self, bus: &mut impl SnesBus, addr: u32) -> u8 {
        bus.read(addr & 0x00FF_FFFF)
    }

    /// 16-bit read with optional bank-wrap. When `bank_bound = true`
    /// the second byte's low 16 bits wrap inside the original bank
    /// (matches DBR-relative behaviour). When `false` the address
    /// rolls over into the next bank (the "long" modes do this).
    fn read16(&self, bus: &mut impl SnesBus, addr: u32, bank_bound: bool) -> u16 {
        let lo = self.read8(bus, addr) as u16;
        let next = if bank_bound {
            (addr & 0xFF_0000) | ((addr.wrapping_add(1)) & 0xFFFF)
        } else {
            addr.wrapping_add(1) & 0x00FF_FFFF
        };
        let hi = self.read8(bus, next) as u16;
        (hi << 8) | lo
    }

    fn write8(&self, bus: &mut impl SnesBus, addr: u32, value: u8) {
        bus.write(addr & 0x00FF_FFFF, value);
    }

    fn write16(&self, bus: &mut impl SnesBus, addr: u32, value: u16, bank_bound: bool) {
        self.write8(bus, addr, value as u8);
        let next = if bank_bound {
            (addr & 0xFF_0000) | ((addr.wrapping_add(1)) & 0xFFFF)
        } else {
            addr.wrapping_add(1) & 0x00FF_FFFF
        };
        self.write8(bus, next, (value >> 8) as u8);
    }

    fn push8(&mut self, bus: &mut impl SnesBus, value: u8) {
        let addr = self.s as u32;
        self.write8(bus, addr, value);
        if self.mode == Mode::Emulation {
            self.s = 0x0100 | ((self.s.wrapping_sub(1)) & 0x00FF);
        } else {
            self.s = self.s.wrapping_sub(1);
        }
    }

    fn pull8(&mut self, bus: &mut impl SnesBus) -> u8 {
        if self.mode == Mode::Emulation {
            self.s = 0x0100 | ((self.s.wrapping_add(1)) & 0x00FF);
        } else {
            self.s = self.s.wrapping_add(1);
        }
        self.read8(bus, self.s as u32)
    }

    fn push16(&mut self, bus: &mut impl SnesBus, value: u16) {
        self.push8(bus, (value >> 8) as u8);
        self.push8(bus, value as u8);
    }

    fn pull16(&mut self, bus: &mut impl SnesBus) -> u16 {
        let lo = self.pull8(bus) as u16;
        let hi = self.pull8(bus) as u16;
        (hi << 8) | lo
    }

    fn set_nz_u8(&mut self, value: u8) {
        self.p.n = value & 0x80 != 0;
        self.p.z = value == 0;
    }

    fn set_nz_u16(&mut self, value: u16) {
        self.p.n = value & 0x8000 != 0;
        self.p.z = value == 0;
    }

    /// Re-enforce the "x flag clears index high bytes" errata. Call
    /// after any P mutation that may set the x flag.
    fn enforce_index_width(&mut self) {
        if self.p.x {
            self.x &= 0x00FF;
            self.y &= 0x00FF;
        }
    }

    /// Service the highest-priority pending interrupt: NMI > IRQ.
    /// Returns the synthetic "opcode" $00 (BRK) for tracing - real
    /// 65C816 doesn't expose what it dispatched, but the test bus
    /// + future tracer find it convenient.
    fn service_interrupt(&mut self, bus: &mut impl SnesBus) -> u8 {
        let nmi = self.nmi_pending;
        if nmi {
            self.nmi_pending = false;
        }
        let p_pushed = self.p.pack(self.mode) & !0x10; // hardware push clears B
        match self.mode {
            Mode::Native => {
                self.push8(bus, self.pbr);
                self.push16(bus, self.pc);
                self.push8(bus, p_pushed);
            }
            Mode::Emulation => {
                self.push16(bus, self.pc);
                self.push8(bus, p_pushed);
            }
        }
        self.p.d = false;
        self.p.i = true;
        self.pbr = 0;
        let vec_addr = match (self.mode, nmi) {
            (Mode::Native, true) => 0x00FFEA,
            (Mode::Native, false) => 0x00FFEE,
            (Mode::Emulation, true) => 0x00FFFA,
            (Mode::Emulation, false) => 0x00FFFE,
        };
        let lo = bus.read(vec_addr) as u16;
        let hi = bus.read(vec_addr + 1) as u16;
        self.pc = (hi << 8) | lo;
        0x00
    }

    // Addressing-mode resolvers. Each returns the 24-bit effective
    // address. They DO NOT issue any bus read of the data itself;
    // the caller does that with the right width. Operand-fetch
    // reads (the bytes embedded in the instruction stream) DO
    // happen here because they're charged against the instruction.

    fn addr_direct(&mut self, bus: &mut impl SnesBus) -> u32 {
        let dp = self.fetch_u8(bus) as u16;
        if self.d & 0xFF != 0 {
            bus.idle();
        }
        self.d.wrapping_add(dp) as u32
    }

    fn addr_direct_x(&mut self, bus: &mut impl SnesBus) -> u32 {
        let dp = self.fetch_u8(bus) as u16;
        if self.d & 0xFF != 0 {
            bus.idle();
        }
        bus.idle();
        self.d.wrapping_add(dp).wrapping_add(self.x) as u32
    }

    fn addr_direct_y(&mut self, bus: &mut impl SnesBus) -> u32 {
        let dp = self.fetch_u8(bus) as u16;
        if self.d & 0xFF != 0 {
            bus.idle();
        }
        bus.idle();
        self.d.wrapping_add(dp).wrapping_add(self.y) as u32
    }

    fn addr_direct_indirect(&mut self, bus: &mut impl SnesBus) -> u32 {
        let dp = self.fetch_u8(bus) as u16;
        if self.d & 0xFF != 0 {
            bus.idle();
        }
        let ptr = self.d.wrapping_add(dp) as u32;
        let lo = self.read8(bus, ptr) as u32;
        let hi = self.read8(bus, self.dp_indirect_high_addr(ptr)) as u32;
        ((self.dbr as u32) << 16) | (hi << 8) | lo
    }

    /// Compute where the high byte of a direct-page-indirect pointer
    /// lives. In native mode (or whenever `D & $FF != 0`) this is
    /// just the next byte. In emulation mode with `D & $FF == 0`,
    /// the W65C816 has a 6502-era page-wrap quirk: if `ptr + 1` lands
    /// at `$xx00`, the read instead wraps back to `$xx00 - $100`,
    /// staying inside the original direct page. Mirrors Mesen2
    /// `GetDirectAddressIndirectWordWithPageWrap`
    /// (Core/SNES/SnesCpu.Shared.h:515-530).
    fn dp_indirect_high_addr(&self, ptr: u32) -> u32 {
        let next = (ptr & 0xFF_0000) | ((ptr.wrapping_add(1)) & 0xFFFF);
        if self.mode == Mode::Emulation && (self.d & 0xFF) == 0 && (next & 0xFF) == 0 {
            (ptr & 0xFF_0000) | (next.wrapping_sub(0x100) & 0xFFFF)
        } else {
            next
        }
    }

    fn addr_direct_indirect_long(&mut self, bus: &mut impl SnesBus) -> u32 {
        let dp = self.fetch_u8(bus) as u16;
        if self.d & 0xFF != 0 {
            bus.idle();
        }
        let ptr = self.d.wrapping_add(dp) as u32;
        let lo = self.read8(bus, ptr) as u32;
        let mid = self.read8(bus, (ptr & 0xFF_0000) | (ptr.wrapping_add(1) & 0xFFFF)) as u32;
        let hi = self.read8(bus, (ptr & 0xFF_0000) | (ptr.wrapping_add(2) & 0xFFFF)) as u32;
        (hi << 16) | (mid << 8) | lo
    }

    fn addr_direct_x_indirect(&mut self, bus: &mut impl SnesBus) -> u32 {
        let dp = self.fetch_u8(bus) as u16;
        if self.d & 0xFF != 0 {
            bus.idle();
        }
        bus.idle();
        let ptr = self.d.wrapping_add(dp).wrapping_add(self.x) as u32;
        let lo = self.read8(bus, ptr) as u32;
        let hi = self.read8(bus, self.dp_indirect_high_addr(ptr)) as u32;
        ((self.dbr as u32) << 16) | (hi << 8) | lo
    }

    fn addr_direct_indirect_y(&mut self, bus: &mut impl SnesBus) -> u32 {
        let dp = self.fetch_u8(bus) as u16;
        if self.d & 0xFF != 0 {
            bus.idle();
        }
        let ptr = self.d.wrapping_add(dp) as u32;
        let lo = self.read8(bus, ptr) as u32;
        let hi = self.read8(bus, self.dp_indirect_high_addr(ptr)) as u32;
        let base = ((self.dbr as u32) << 16) | (hi << 8) | lo;
        base.wrapping_add(self.y as u32) & 0x00FF_FFFF
    }

    fn addr_direct_indirect_long_y(&mut self, bus: &mut impl SnesBus) -> u32 {
        let base = self.addr_direct_indirect_long(bus);
        base.wrapping_add(self.y as u32) & 0x00FF_FFFF
    }

    fn addr_absolute(&mut self, bus: &mut impl SnesBus) -> u32 {
        let off = self.fetch_u16(bus) as u32;
        ((self.dbr as u32) << 16) | off
    }

    fn addr_absolute_x(&mut self, bus: &mut impl SnesBus) -> u32 {
        let base = self.addr_absolute(bus);
        base.wrapping_add(self.x as u32) & 0x00FF_FFFF
    }

    fn addr_absolute_y(&mut self, bus: &mut impl SnesBus) -> u32 {
        let base = self.addr_absolute(bus);
        base.wrapping_add(self.y as u32) & 0x00FF_FFFF
    }

    fn addr_long(&mut self, bus: &mut impl SnesBus) -> u32 {
        self.fetch_u24(bus)
    }

    fn addr_long_x(&mut self, bus: &mut impl SnesBus) -> u32 {
        let base = self.fetch_u24(bus);
        base.wrapping_add(self.x as u32) & 0x00FF_FFFF
    }

    fn addr_stack_rel(&mut self, bus: &mut impl SnesBus) -> u32 {
        let off = self.fetch_u8(bus) as u16;
        bus.idle();
        self.s.wrapping_add(off) as u32
    }

    fn addr_stack_rel_indirect_y(&mut self, bus: &mut impl SnesBus) -> u32 {
        let off = self.fetch_u8(bus) as u16;
        bus.idle();
        let ptr = self.s.wrapping_add(off) as u32;
        let lo = self.read8(bus, ptr) as u32;
        let hi = self.read8(bus, (ptr.wrapping_add(1)) & 0x00FF_FFFF) as u32;
        bus.idle();
        let base = ((self.dbr as u32) << 16) | (hi << 8) | lo;
        base.wrapping_add(self.y as u32) & 0x00FF_FFFF
    }

    /// Width-aware accumulator load: returns the 16-bit value with
    /// the high byte zero-extended when m=1.
    fn load_a_width(&mut self, bus: &mut impl SnesBus, addr: u32) -> u16 {
        if self.p.m {
            self.read8(bus, addr) as u16
        } else {
            self.read16(bus, addr, true)
        }
    }

    fn load_x_width(&mut self, bus: &mut impl SnesBus, addr: u32) -> u16 {
        if self.p.x {
            self.read8(bus, addr) as u16
        } else {
            self.read16(bus, addr, true)
        }
    }

    fn store_a_width(&mut self, bus: &mut impl SnesBus, addr: u32, value: u16) {
        if self.p.m {
            self.write8(bus, addr, value as u8);
        } else {
            self.write16(bus, addr, value, true);
        }
    }

    /// 16-bit RMW writes commit high byte first, then low byte.
    /// This mirrors how the W65C816 latches the modified word
    /// internally and matches Mesen2's `WriteWordRmw`
    /// (Core/SNES/SnesCpu.Shared.h:456-461). Normal STA in 16-bit
    /// keeps the low-then-high order via [`store_a_width`].
    fn store_a_width_rmw(&mut self, bus: &mut impl SnesBus, addr: u32, value: u16) {
        if self.p.m {
            self.write8(bus, addr, value as u8);
        } else {
            let next = (addr & 0xFF_0000) | ((addr.wrapping_add(1)) & 0xFFFF);
            self.write8(bus, next, (value >> 8) as u8);
            self.write8(bus, addr, value as u8);
        }
    }

    fn store_x_width(&mut self, bus: &mut impl SnesBus, addr: u32, value: u16) {
        if self.p.x {
            self.write8(bus, addr, value as u8);
        } else {
            self.write16(bus, addr, value, true);
        }
    }

    fn set_nz_m(&mut self, value: u16) {
        if self.p.m {
            self.set_nz_u8(value as u8);
        } else {
            self.set_nz_u16(value);
        }
    }

    fn set_nz_x(&mut self, value: u16) {
        if self.p.x {
            self.set_nz_u8(value as u8);
        } else {
            self.set_nz_u16(value);
        }
    }

    fn write_a_width(&mut self, value: u16) {
        if self.p.m {
            self.set_a(value as u8);
        } else {
            self.c = value;
        }
    }

    fn read_a_width(&self) -> u16 {
        if self.p.m {
            self.c & 0x00FF
        } else {
            self.c
        }
    }

    fn read_x_width(&self) -> u16 {
        if self.p.x {
            self.x & 0x00FF
        } else {
            self.x
        }
    }

    fn read_y_width(&self) -> u16 {
        if self.p.x {
            self.y & 0x00FF
        } else {
            self.y
        }
    }

    /// The big switch. Phase 2a covers control-flow, transfers,
    /// loads/stores, branches, jumps, stack ops, increments, mode
    /// switches, and NOP/WDM/RTI. Unimplemented opcodes panic so
    /// later phases can spot what's still missing without silently
    /// passing through.
    fn dispatch(&mut self, opcode: u8, bus: &mut impl SnesBus) {
        match opcode {
            // === Status flag manipulation ===
            0x18 => self.p.c = false,            // CLC
            0x38 => self.p.c = true,             // SEC
            0x58 => self.p.i = false,            // CLI
            0x78 => self.p.i = true,             // SEI
            0xB8 => self.p.v = false,            // CLV
            0xD8 => self.p.d = false,            // CLD
            0xF8 => self.p.d = true,             // SED

            // REP / SEP / XCE
            0xC2 => {
                let m = self.fetch_u8(bus);
                let mut p = self.p.pack(self.mode);
                p &= !m;
                self.p.unpack(p, self.mode);
                bus.idle();
                self.enforce_index_width();
            }
            0xE2 => {
                let m = self.fetch_u8(bus);
                let mut p = self.p.pack(self.mode);
                p |= m;
                self.p.unpack(p, self.mode);
                bus.idle();
                self.enforce_index_width();
            }
            0xFB => {
                let prev_c = self.p.c;
                let prev_e = self.mode == Mode::Emulation;
                self.p.c = prev_e;
                self.mode = if prev_c { Mode::Emulation } else { Mode::Native };
                bus.idle();
                if self.mode == Mode::Emulation {
                    self.p.m = true;
                    self.p.x = true;
                    self.s = 0x0100 | (self.s & 0x00FF);
                    self.enforce_index_width();
                }
            }

            // === Transfers ===
            //
            // Per WDC: register-to-register transfers ALWAYS read the
            // full 16-bit source. The width of the write (and the NZ
            // flag basis) is taken from the **destination** register's
            // flag - x for TAX/TAY/TXY/TYX, m for TXA/TYA. Caught by
            // CPUTRN test C: m=1 / x=0 / C=$FFFF / TAX should give
            // X=$FFFF, not X=$00FF. Mirrors Mesen2 SetRegister
            // (Core/SNES/SnesCpu.Shared.h `SetRegister(uint16_t&, ...)`).
            0xAA => {
                // TAX - source = full C, dest width = x
                if self.p.x {
                    let lo = self.c as u8;
                    self.x = (self.x & 0xFF00) | lo as u16;
                    self.set_nz_u8(lo);
                } else {
                    self.x = self.c;
                    self.set_nz_u16(self.x);
                }
            }
            0xA8 => {
                // TAY - same shape, dest = Y
                if self.p.x {
                    let lo = self.c as u8;
                    self.y = (self.y & 0xFF00) | lo as u16;
                    self.set_nz_u8(lo);
                } else {
                    self.y = self.c;
                    self.set_nz_u16(self.y);
                }
            }
            0x8A => {
                // TXA - source = full X, dest width = m
                if self.p.m {
                    let lo = self.x as u8;
                    self.c = (self.c & 0xFF00) | lo as u16;
                    self.set_nz_u8(lo);
                } else {
                    self.c = self.x;
                    self.set_nz_u16(self.c);
                }
            }
            0x98 => {
                // TYA - source = full Y, dest width = m
                if self.p.m {
                    let lo = self.y as u8;
                    self.c = (self.c & 0xFF00) | lo as u16;
                    self.set_nz_u8(lo);
                } else {
                    self.c = self.y;
                    self.set_nz_u16(self.c);
                }
            }
            0x9B => {
                // TXY - source = full X, dest width = x
                if self.p.x {
                    let lo = self.x as u8;
                    self.y = (self.y & 0xFF00) | lo as u16;
                    self.set_nz_u8(lo);
                } else {
                    self.y = self.x;
                    self.set_nz_u16(self.y);
                }
            }
            0xBB => {
                // TYX - source = full Y, dest width = x
                if self.p.x {
                    let lo = self.y as u8;
                    self.x = (self.x & 0xFF00) | lo as u16;
                    self.set_nz_u8(lo);
                } else {
                    self.x = self.y;
                    self.set_nz_u16(self.x);
                }
            }
            0xBA => {
                // TSX - 8 or 16 depending on x
                let v = self.s;
                if self.p.x {
                    self.x = (self.x & 0xFF00) | (v & 0xFF);
                } else {
                    self.x = v;
                }
                self.set_nz_x(self.read_x_width());
            }
            0x9A => {
                // TXS - in emulation S high byte forced to $01
                if self.mode == Mode::Emulation {
                    self.s = 0x0100 | (self.x & 0x00FF);
                } else {
                    self.s = self.x;
                }
            }
            0x5B => {
                // TCD - full 16-bit transfer regardless of m
                self.d = self.c;
                self.set_nz_u16(self.d);
            }
            0x7B => {
                // TDC
                self.c = self.d;
                self.set_nz_u16(self.c);
            }
            0x1B => {
                // TCS - full 16-bit
                if self.mode == Mode::Emulation {
                    self.s = 0x0100 | (self.c & 0x00FF);
                } else {
                    self.s = self.c;
                }
            }
            0x3B => {
                // TSC - 16-bit observation, sets NZ from S
                self.c = self.s;
                self.set_nz_u16(self.c);
            }
            0xEB => {
                // XBA - swap A and B, NZ from new low byte
                self.c = self.c.rotate_right(8);
                self.set_nz_u8(self.c as u8);
                bus.idle();
                bus.idle();
            }

            // === Loads ===
            0xA9 => {
                // LDA #imm
                let v = if self.p.m {
                    self.fetch_u8(bus) as u16
                } else {
                    self.fetch_u16(bus)
                };
                self.write_a_width(v);
                self.set_nz_m(v);
            }
            0xA5 => { let a = self.addr_direct(bus); self.do_lda(bus, a); }
            0xB5 => { let a = self.addr_direct_x(bus); self.do_lda(bus, a); }
            0xB2 => { let a = self.addr_direct_indirect(bus); self.do_lda(bus, a); }
            0xA7 => { let a = self.addr_direct_indirect_long(bus); self.do_lda(bus, a); }
            0xA1 => { let a = self.addr_direct_x_indirect(bus); self.do_lda(bus, a); }
            0xB1 => { let a = self.addr_direct_indirect_y(bus); self.do_lda(bus, a); }
            0xB7 => { let a = self.addr_direct_indirect_long_y(bus); self.do_lda(bus, a); }
            0xAD => { let a = self.addr_absolute(bus); self.do_lda(bus, a); }
            0xBD => { let a = self.addr_absolute_x(bus); self.do_lda(bus, a); }
            0xB9 => { let a = self.addr_absolute_y(bus); self.do_lda(bus, a); }
            0xAF => { let a = self.addr_long(bus); self.do_lda(bus, a); }
            0xBF => { let a = self.addr_long_x(bus); self.do_lda(bus, a); }
            0xA3 => { let a = self.addr_stack_rel(bus); self.do_lda(bus, a); }
            0xB3 => { let a = self.addr_stack_rel_indirect_y(bus); self.do_lda(bus, a); }

            0xA2 => {
                // LDX #imm
                let v = if self.p.x {
                    self.fetch_u8(bus) as u16
                } else {
                    self.fetch_u16(bus)
                };
                self.x = if self.p.x { v & 0xFF } else { v };
                self.set_nz_x(self.read_x_width());
            }
            0xA6 => { let a = self.addr_direct(bus); self.do_ldx(bus, a); }
            0xB6 => { let a = self.addr_direct_y(bus); self.do_ldx(bus, a); }
            0xAE => { let a = self.addr_absolute(bus); self.do_ldx(bus, a); }
            0xBE => { let a = self.addr_absolute_y(bus); self.do_ldx(bus, a); }

            0xA0 => {
                // LDY #imm
                let v = if self.p.x {
                    self.fetch_u8(bus) as u16
                } else {
                    self.fetch_u16(bus)
                };
                self.y = if self.p.x { v & 0xFF } else { v };
                self.set_nz_x(self.read_y_width());
            }
            0xA4 => { let a = self.addr_direct(bus); self.do_ldy(bus, a); }
            0xB4 => { let a = self.addr_direct_x(bus); self.do_ldy(bus, a); }
            0xAC => { let a = self.addr_absolute(bus); self.do_ldy(bus, a); }
            0xBC => { let a = self.addr_absolute_x(bus); self.do_ldy(bus, a); }

            // === Stores ===
            0x85 => { let a = self.addr_direct(bus); self.do_sta(bus, a); }
            0x95 => { let a = self.addr_direct_x(bus); self.do_sta(bus, a); }
            0x92 => { let a = self.addr_direct_indirect(bus); self.do_sta(bus, a); }
            0x87 => { let a = self.addr_direct_indirect_long(bus); self.do_sta(bus, a); }
            0x81 => { let a = self.addr_direct_x_indirect(bus); self.do_sta(bus, a); }
            0x91 => { let a = self.addr_direct_indirect_y(bus); self.do_sta(bus, a); }
            0x97 => { let a = self.addr_direct_indirect_long_y(bus); self.do_sta(bus, a); }
            0x8D => { let a = self.addr_absolute(bus); self.do_sta(bus, a); }
            0x9D => { let a = self.addr_absolute_x(bus); self.do_sta(bus, a); }
            0x99 => { let a = self.addr_absolute_y(bus); self.do_sta(bus, a); }
            0x8F => { let a = self.addr_long(bus); self.do_sta(bus, a); }
            0x9F => { let a = self.addr_long_x(bus); self.do_sta(bus, a); }
            0x83 => { let a = self.addr_stack_rel(bus); self.do_sta(bus, a); }
            0x93 => { let a = self.addr_stack_rel_indirect_y(bus); self.do_sta(bus, a); }

            0x86 => { let a = self.addr_direct(bus); self.do_stx(bus, a); }
            0x96 => { let a = self.addr_direct_y(bus); self.do_stx(bus, a); }
            0x8E => { let a = self.addr_absolute(bus); self.do_stx(bus, a); }

            0x84 => { let a = self.addr_direct(bus); self.do_sty(bus, a); }
            0x94 => { let a = self.addr_direct_x(bus); self.do_sty(bus, a); }
            0x8C => { let a = self.addr_absolute(bus); self.do_sty(bus, a); }

            0x64 => { let a = self.addr_direct(bus); self.do_stz(bus, a); }
            0x74 => { let a = self.addr_direct_x(bus); self.do_stz(bus, a); }
            0x9C => { let a = self.addr_absolute(bus); self.do_stz(bus, a); }
            0x9E => { let a = self.addr_absolute_x(bus); self.do_stz(bus, a); }

            // === Branches ===
            0x80 => self.branch(bus, true),                 // BRA
            0x82 => self.branch_long(bus),                  // BRL
            0xF0 => self.branch(bus, self.p.z),             // BEQ
            0xD0 => self.branch(bus, !self.p.z),            // BNE
            0xB0 => self.branch(bus, self.p.c),             // BCS
            0x90 => self.branch(bus, !self.p.c),            // BCC
            0x30 => self.branch(bus, self.p.n),             // BMI
            0x10 => self.branch(bus, !self.p.n),            // BPL
            0x70 => self.branch(bus, self.p.v),             // BVS
            0x50 => self.branch(bus, !self.p.v),            // BVC

            // === Jumps and calls ===
            0x4C => {
                // JMP abs
                let addr = self.fetch_u16(bus);
                self.pc = addr;
            }
            0x5C => {
                // JML / JMP long
                let addr = self.fetch_u24(bus);
                self.pc = addr as u16;
                self.pbr = (addr >> 16) as u8;
            }
            0x6C => {
                // JMP (abs) - indirect through bank 0, page-wrap
                // bug FIXED on 65C816 (full 16-bit increment).
                let ptr = self.fetch_u16(bus) as u32;
                let lo = bus.read(ptr) as u16;
                let hi = bus.read(ptr.wrapping_add(1) & 0xFFFF) as u16;
                self.pc = (hi << 8) | lo;
            }
            0x7C => {
                // JMP (abs,X) - indirect through PBR
                let ptr_lo = self.fetch_u16(bus);
                bus.idle();
                let ptr = ((self.pbr as u32) << 16) | (ptr_lo.wrapping_add(self.x) as u32);
                let lo = bus.read(ptr) as u16;
                let hi = bus.read((ptr & 0xFF_0000) | (ptr.wrapping_add(1) & 0xFFFF)) as u16;
                self.pc = (hi << 8) | lo;
            }
            0xDC => {
                // JML [abs] - long indirect through bank 0
                let ptr = self.fetch_u16(bus) as u32;
                let lo = bus.read(ptr) as u32;
                let mid = bus.read(ptr.wrapping_add(1) & 0xFFFF) as u32;
                let hi = bus.read(ptr.wrapping_add(2) & 0xFFFF) as u32;
                self.pc = ((mid << 8) | lo) as u16;
                self.pbr = hi as u8;
            }
            0x20 => {
                // JSR abs - push (PC - 1) within PBR
                let addr = self.fetch_u16(bus);
                bus.idle();
                let ret = self.pc.wrapping_sub(1);
                self.push16(bus, ret);
                self.pc = addr;
            }
            0x22 => {
                // JSL long - push PBR then (PC - 1)
                let lo = self.fetch_u8(bus) as u32;
                let mid = self.fetch_u8(bus) as u32;
                self.push8(bus, self.pbr);
                bus.idle();
                let hi = self.fetch_u8(bus) as u32;
                let ret = self.pc.wrapping_sub(1);
                self.push16(bus, ret);
                self.pc = ((mid << 8) | lo) as u16;
                self.pbr = hi as u8;
            }
            0xFC => {
                // JSR (abs,X) - push (PC - 1), then indirect through PBR
                let ptr_lo = self.fetch_u8(bus) as u16;
                self.push16(bus, self.pc.wrapping_sub(0)); // push PC of last operand byte? we push PC after both operand bytes - but only one fetched yet
                // Real 65C816 sequence: read low operand, push PCH, push PCL,
                // read high operand, then resolve. Implement faithfully.
                let ptr_hi = self.fetch_u8(bus) as u16;
                bus.idle();
                let ptr_base = (ptr_hi << 8) | ptr_lo;
                let ptr = ((self.pbr as u32) << 16) | (ptr_base.wrapping_add(self.x) as u32);
                let lo = bus.read(ptr) as u16;
                let hi = bus.read((ptr & 0xFF_0000) | (ptr.wrapping_add(1) & 0xFFFF)) as u16;
                self.pc = (hi << 8) | lo;
            }
            0x60 => {
                // RTS
                let ret = self.pull16(bus);
                bus.idle();
                bus.idle();
                bus.idle();
                self.pc = ret.wrapping_add(1);
            }
            0x6B => {
                // RTL
                let ret = self.pull16(bus);
                let bank = self.pull8(bus);
                bus.idle();
                bus.idle();
                self.pc = ret.wrapping_add(1);
                self.pbr = bank;
            }
            0x40 => {
                // RTI
                let p = self.pull8(bus);
                self.p.unpack(p, self.mode);
                self.pc = self.pull16(bus);
                if self.mode == Mode::Native {
                    self.pbr = self.pull8(bus);
                }
                self.enforce_index_width();
            }

            // === Stack ===
            0x48 => {
                // PHA
                let v = self.read_a_width();
                if self.p.m {
                    self.push8(bus, v as u8);
                } else {
                    self.push16(bus, v);
                }
            }
            0x68 => {
                // PLA
                bus.idle();
                bus.idle();
                let v = if self.p.m {
                    self.pull8(bus) as u16
                } else {
                    self.pull16(bus)
                };
                self.write_a_width(v);
                self.set_nz_m(v);
            }
            0xDA => {
                // PHX
                let v = self.read_x_width();
                if self.p.x {
                    self.push8(bus, v as u8);
                } else {
                    self.push16(bus, v);
                }
            }
            0xFA => {
                // PLX
                bus.idle();
                bus.idle();
                let v = if self.p.x {
                    self.pull8(bus) as u16
                } else {
                    self.pull16(bus)
                };
                self.x = if self.p.x { v & 0xFF } else { v };
                self.set_nz_x(self.read_x_width());
            }
            0x5A => {
                // PHY
                let v = self.read_y_width();
                if self.p.x {
                    self.push8(bus, v as u8);
                } else {
                    self.push16(bus, v);
                }
            }
            0x7A => {
                // PLY
                bus.idle();
                bus.idle();
                let v = if self.p.x {
                    self.pull8(bus) as u16
                } else {
                    self.pull16(bus)
                };
                self.y = if self.p.x { v & 0xFF } else { v };
                self.set_nz_x(self.read_y_width());
            }
            0x08 => {
                // PHP
                let p = self.p.pack(self.mode);
                self.push8(bus, p);
            }
            0x28 => {
                // PLP
                bus.idle();
                bus.idle();
                let p = self.pull8(bus);
                self.p.unpack(p, self.mode);
                self.enforce_index_width();
            }
            0x8B => {
                // PHB
                self.push8(bus, self.dbr);
            }
            0xAB => {
                // PLB
                bus.idle();
                bus.idle();
                let v = self.pull8(bus);
                self.dbr = v;
                self.set_nz_u8(v);
            }
            0x4B => {
                // PHK
                self.push8(bus, self.pbr);
            }
            0x0B => {
                // PHD - 16-bit
                self.push16(bus, self.d);
            }
            0x2B => {
                // PLD
                bus.idle();
                bus.idle();
                let v = self.pull16(bus);
                self.d = v;
                self.set_nz_u16(v);
            }
            0xF4 => {
                // PEA #abs - push absolute as 16-bit
                let v = self.fetch_u16(bus);
                self.push16(bus, v);
            }
            0xD4 => {
                // PEI (dp) - push direct-page-indirect 16-bit
                let dp = self.fetch_u8(bus) as u16;
                if self.d & 0xFF != 0 {
                    bus.idle();
                }
                let ptr = self.d.wrapping_add(dp) as u32;
                let lo = bus.read(ptr) as u16;
                let hi = bus.read((ptr & 0xFF_0000) | (ptr.wrapping_add(1) & 0xFFFF)) as u16;
                self.push16(bus, (hi << 8) | lo);
            }
            0x62 => {
                // PER offset - push PC + signed 16-bit offset
                let off = self.fetch_u16(bus) as i16 as i32;
                bus.idle();
                let pushed = (self.pc as i32).wrapping_add(off) as u16;
                self.push16(bus, pushed);
            }

            // === INC / DEC ===
            0x1A => {
                // INC A
                let v = self.read_a_width().wrapping_add(1);
                let v = if self.p.m { v & 0xFF } else { v };
                self.write_a_width(v);
                self.set_nz_m(v);
            }
            0x3A => {
                // DEC A
                let v = self.read_a_width().wrapping_sub(1);
                let v = if self.p.m { v & 0xFF } else { v };
                self.write_a_width(v);
                self.set_nz_m(v);
            }
            0xE8 => {
                // INX
                self.x = if self.p.x {
                    (self.x & 0xFF00) | ((self.x.wrapping_add(1)) & 0xFF)
                } else {
                    self.x.wrapping_add(1)
                };
                self.set_nz_x(self.read_x_width());
            }
            0xCA => {
                // DEX
                self.x = if self.p.x {
                    (self.x & 0xFF00) | ((self.x.wrapping_sub(1)) & 0xFF)
                } else {
                    self.x.wrapping_sub(1)
                };
                self.set_nz_x(self.read_x_width());
            }
            0xC8 => {
                // INY
                self.y = if self.p.x {
                    (self.y & 0xFF00) | ((self.y.wrapping_add(1)) & 0xFF)
                } else {
                    self.y.wrapping_add(1)
                };
                self.set_nz_x(self.read_y_width());
            }
            0x88 => {
                // DEY
                self.y = if self.p.x {
                    (self.y & 0xFF00) | ((self.y.wrapping_sub(1)) & 0xFF)
                } else {
                    self.y.wrapping_sub(1)
                };
                self.set_nz_x(self.read_y_width());
            }

            // === AND / ORA / EOR (width-aware) ===
            0x29 => { let v = self.imm_a_width(bus); self.do_and(v); }
            0x25 => { let a = self.addr_direct(bus); self.alu_a(bus, a, AluOp::And); }
            0x35 => { let a = self.addr_direct_x(bus); self.alu_a(bus, a, AluOp::And); }
            0x32 => { let a = self.addr_direct_indirect(bus); self.alu_a(bus, a, AluOp::And); }
            0x27 => { let a = self.addr_direct_indirect_long(bus); self.alu_a(bus, a, AluOp::And); }
            0x21 => { let a = self.addr_direct_x_indirect(bus); self.alu_a(bus, a, AluOp::And); }
            0x31 => { let a = self.addr_direct_indirect_y(bus); self.alu_a(bus, a, AluOp::And); }
            0x37 => { let a = self.addr_direct_indirect_long_y(bus); self.alu_a(bus, a, AluOp::And); }
            0x2D => { let a = self.addr_absolute(bus); self.alu_a(bus, a, AluOp::And); }
            0x3D => { let a = self.addr_absolute_x(bus); self.alu_a(bus, a, AluOp::And); }
            0x39 => { let a = self.addr_absolute_y(bus); self.alu_a(bus, a, AluOp::And); }
            0x2F => { let a = self.addr_long(bus); self.alu_a(bus, a, AluOp::And); }
            0x3F => { let a = self.addr_long_x(bus); self.alu_a(bus, a, AluOp::And); }
            0x23 => { let a = self.addr_stack_rel(bus); self.alu_a(bus, a, AluOp::And); }
            0x33 => { let a = self.addr_stack_rel_indirect_y(bus); self.alu_a(bus, a, AluOp::And); }

            0x09 => { let v = self.imm_a_width(bus); self.do_ora(v); }
            0x05 => { let a = self.addr_direct(bus); self.alu_a(bus, a, AluOp::Ora); }
            0x15 => { let a = self.addr_direct_x(bus); self.alu_a(bus, a, AluOp::Ora); }
            0x12 => { let a = self.addr_direct_indirect(bus); self.alu_a(bus, a, AluOp::Ora); }
            0x07 => { let a = self.addr_direct_indirect_long(bus); self.alu_a(bus, a, AluOp::Ora); }
            0x01 => { let a = self.addr_direct_x_indirect(bus); self.alu_a(bus, a, AluOp::Ora); }
            0x11 => { let a = self.addr_direct_indirect_y(bus); self.alu_a(bus, a, AluOp::Ora); }
            0x17 => { let a = self.addr_direct_indirect_long_y(bus); self.alu_a(bus, a, AluOp::Ora); }
            0x0D => { let a = self.addr_absolute(bus); self.alu_a(bus, a, AluOp::Ora); }
            0x1D => { let a = self.addr_absolute_x(bus); self.alu_a(bus, a, AluOp::Ora); }
            0x19 => { let a = self.addr_absolute_y(bus); self.alu_a(bus, a, AluOp::Ora); }
            0x0F => { let a = self.addr_long(bus); self.alu_a(bus, a, AluOp::Ora); }
            0x1F => { let a = self.addr_long_x(bus); self.alu_a(bus, a, AluOp::Ora); }
            0x03 => { let a = self.addr_stack_rel(bus); self.alu_a(bus, a, AluOp::Ora); }
            0x13 => { let a = self.addr_stack_rel_indirect_y(bus); self.alu_a(bus, a, AluOp::Ora); }

            0x49 => { let v = self.imm_a_width(bus); self.do_eor(v); }
            0x45 => { let a = self.addr_direct(bus); self.alu_a(bus, a, AluOp::Eor); }
            0x55 => { let a = self.addr_direct_x(bus); self.alu_a(bus, a, AluOp::Eor); }
            0x52 => { let a = self.addr_direct_indirect(bus); self.alu_a(bus, a, AluOp::Eor); }
            0x47 => { let a = self.addr_direct_indirect_long(bus); self.alu_a(bus, a, AluOp::Eor); }
            0x41 => { let a = self.addr_direct_x_indirect(bus); self.alu_a(bus, a, AluOp::Eor); }
            0x51 => { let a = self.addr_direct_indirect_y(bus); self.alu_a(bus, a, AluOp::Eor); }
            0x57 => { let a = self.addr_direct_indirect_long_y(bus); self.alu_a(bus, a, AluOp::Eor); }
            0x4D => { let a = self.addr_absolute(bus); self.alu_a(bus, a, AluOp::Eor); }
            0x5D => { let a = self.addr_absolute_x(bus); self.alu_a(bus, a, AluOp::Eor); }
            0x59 => { let a = self.addr_absolute_y(bus); self.alu_a(bus, a, AluOp::Eor); }
            0x4F => { let a = self.addr_long(bus); self.alu_a(bus, a, AluOp::Eor); }
            0x5F => { let a = self.addr_long_x(bus); self.alu_a(bus, a, AluOp::Eor); }
            0x43 => { let a = self.addr_stack_rel(bus); self.alu_a(bus, a, AluOp::Eor); }
            0x53 => { let a = self.addr_stack_rel_indirect_y(bus); self.alu_a(bus, a, AluOp::Eor); }

            // === ADC / SBC (binary mode; BCD adjustment lands in 2c) ===
            0x69 => { let v = self.imm_a_width(bus); self.do_adc(v); }
            0x65 => { let a = self.addr_direct(bus); self.alu_a(bus, a, AluOp::Adc); }
            0x75 => { let a = self.addr_direct_x(bus); self.alu_a(bus, a, AluOp::Adc); }
            0x72 => { let a = self.addr_direct_indirect(bus); self.alu_a(bus, a, AluOp::Adc); }
            0x67 => { let a = self.addr_direct_indirect_long(bus); self.alu_a(bus, a, AluOp::Adc); }
            0x61 => { let a = self.addr_direct_x_indirect(bus); self.alu_a(bus, a, AluOp::Adc); }
            0x71 => { let a = self.addr_direct_indirect_y(bus); self.alu_a(bus, a, AluOp::Adc); }
            0x77 => { let a = self.addr_direct_indirect_long_y(bus); self.alu_a(bus, a, AluOp::Adc); }
            0x6D => { let a = self.addr_absolute(bus); self.alu_a(bus, a, AluOp::Adc); }
            0x7D => { let a = self.addr_absolute_x(bus); self.alu_a(bus, a, AluOp::Adc); }
            0x79 => { let a = self.addr_absolute_y(bus); self.alu_a(bus, a, AluOp::Adc); }
            0x6F => { let a = self.addr_long(bus); self.alu_a(bus, a, AluOp::Adc); }
            0x7F => { let a = self.addr_long_x(bus); self.alu_a(bus, a, AluOp::Adc); }
            0x63 => { let a = self.addr_stack_rel(bus); self.alu_a(bus, a, AluOp::Adc); }
            0x73 => { let a = self.addr_stack_rel_indirect_y(bus); self.alu_a(bus, a, AluOp::Adc); }

            0xE9 => { let v = self.imm_a_width(bus); self.do_sbc(v); }
            0xE5 => { let a = self.addr_direct(bus); self.alu_a(bus, a, AluOp::Sbc); }
            0xF5 => { let a = self.addr_direct_x(bus); self.alu_a(bus, a, AluOp::Sbc); }
            0xF2 => { let a = self.addr_direct_indirect(bus); self.alu_a(bus, a, AluOp::Sbc); }
            0xE7 => { let a = self.addr_direct_indirect_long(bus); self.alu_a(bus, a, AluOp::Sbc); }
            0xE1 => { let a = self.addr_direct_x_indirect(bus); self.alu_a(bus, a, AluOp::Sbc); }
            0xF1 => { let a = self.addr_direct_indirect_y(bus); self.alu_a(bus, a, AluOp::Sbc); }
            0xF7 => { let a = self.addr_direct_indirect_long_y(bus); self.alu_a(bus, a, AluOp::Sbc); }
            0xED => { let a = self.addr_absolute(bus); self.alu_a(bus, a, AluOp::Sbc); }
            0xFD => { let a = self.addr_absolute_x(bus); self.alu_a(bus, a, AluOp::Sbc); }
            0xF9 => { let a = self.addr_absolute_y(bus); self.alu_a(bus, a, AluOp::Sbc); }
            0xEF => { let a = self.addr_long(bus); self.alu_a(bus, a, AluOp::Sbc); }
            0xFF => { let a = self.addr_long_x(bus); self.alu_a(bus, a, AluOp::Sbc); }
            0xE3 => { let a = self.addr_stack_rel(bus); self.alu_a(bus, a, AluOp::Sbc); }
            0xF3 => { let a = self.addr_stack_rel_indirect_y(bus); self.alu_a(bus, a, AluOp::Sbc); }

            // === CMP / CPX / CPY ===
            0xC9 => { let v = self.imm_a_width(bus); self.do_cmp_a(v); }
            0xC5 => { let a = self.addr_direct(bus); self.alu_a(bus, a, AluOp::Cmp); }
            0xD5 => { let a = self.addr_direct_x(bus); self.alu_a(bus, a, AluOp::Cmp); }
            0xD2 => { let a = self.addr_direct_indirect(bus); self.alu_a(bus, a, AluOp::Cmp); }
            0xC7 => { let a = self.addr_direct_indirect_long(bus); self.alu_a(bus, a, AluOp::Cmp); }
            0xC1 => { let a = self.addr_direct_x_indirect(bus); self.alu_a(bus, a, AluOp::Cmp); }
            0xD1 => { let a = self.addr_direct_indirect_y(bus); self.alu_a(bus, a, AluOp::Cmp); }
            0xD7 => { let a = self.addr_direct_indirect_long_y(bus); self.alu_a(bus, a, AluOp::Cmp); }
            0xCD => { let a = self.addr_absolute(bus); self.alu_a(bus, a, AluOp::Cmp); }
            0xDD => { let a = self.addr_absolute_x(bus); self.alu_a(bus, a, AluOp::Cmp); }
            0xD9 => { let a = self.addr_absolute_y(bus); self.alu_a(bus, a, AluOp::Cmp); }
            0xCF => { let a = self.addr_long(bus); self.alu_a(bus, a, AluOp::Cmp); }
            0xDF => { let a = self.addr_long_x(bus); self.alu_a(bus, a, AluOp::Cmp); }
            0xC3 => { let a = self.addr_stack_rel(bus); self.alu_a(bus, a, AluOp::Cmp); }
            0xD3 => { let a = self.addr_stack_rel_indirect_y(bus); self.alu_a(bus, a, AluOp::Cmp); }

            0xE0 => { let v = self.imm_x_width(bus); self.do_cmp_x(v); }
            0xE4 => { let a = self.addr_direct(bus); let v = self.load_x_width(bus, a); self.do_cmp_x(v); }
            0xEC => { let a = self.addr_absolute(bus); let v = self.load_x_width(bus, a); self.do_cmp_x(v); }

            0xC0 => { let v = self.imm_x_width(bus); self.do_cmp_y(v); }
            0xC4 => { let a = self.addr_direct(bus); let v = self.load_x_width(bus, a); self.do_cmp_y(v); }
            0xCC => { let a = self.addr_absolute(bus); let v = self.load_x_width(bus, a); self.do_cmp_y(v); }

            // === BIT (immediate is Z-only, others are full N/V/Z) ===
            0x89 => {
                let v = self.imm_a_width(bus);
                let masked = self.read_a_width() & v;
                self.p.z = masked == 0;
            }
            0x24 => { let a = self.addr_direct(bus); self.do_bit(bus, a); }
            0x34 => { let a = self.addr_direct_x(bus); self.do_bit(bus, a); }
            0x2C => { let a = self.addr_absolute(bus); self.do_bit(bus, a); }
            0x3C => { let a = self.addr_absolute_x(bus); self.do_bit(bus, a); }

            // === ASL / LSR / ROL / ROR (accumulator + memory) ===
            0x0A => self.do_asl_a(),
            0x06 => { let a = self.addr_direct(bus); self.rmw(bus, a, RmwOp::Asl); }
            0x16 => { let a = self.addr_direct_x(bus); self.rmw(bus, a, RmwOp::Asl); }
            0x0E => { let a = self.addr_absolute(bus); self.rmw(bus, a, RmwOp::Asl); }
            0x1E => { let a = self.addr_absolute_x(bus); self.rmw(bus, a, RmwOp::Asl); }

            0x4A => self.do_lsr_a(),
            0x46 => { let a = self.addr_direct(bus); self.rmw(bus, a, RmwOp::Lsr); }
            0x56 => { let a = self.addr_direct_x(bus); self.rmw(bus, a, RmwOp::Lsr); }
            0x4E => { let a = self.addr_absolute(bus); self.rmw(bus, a, RmwOp::Lsr); }
            0x5E => { let a = self.addr_absolute_x(bus); self.rmw(bus, a, RmwOp::Lsr); }

            0x2A => self.do_rol_a(),
            0x26 => { let a = self.addr_direct(bus); self.rmw(bus, a, RmwOp::Rol); }
            0x36 => { let a = self.addr_direct_x(bus); self.rmw(bus, a, RmwOp::Rol); }
            0x2E => { let a = self.addr_absolute(bus); self.rmw(bus, a, RmwOp::Rol); }
            0x3E => { let a = self.addr_absolute_x(bus); self.rmw(bus, a, RmwOp::Rol); }

            0x6A => self.do_ror_a(),
            0x66 => { let a = self.addr_direct(bus); self.rmw(bus, a, RmwOp::Ror); }
            0x76 => { let a = self.addr_direct_x(bus); self.rmw(bus, a, RmwOp::Ror); }
            0x6E => { let a = self.addr_absolute(bus); self.rmw(bus, a, RmwOp::Ror); }
            0x7E => { let a = self.addr_absolute_x(bus); self.rmw(bus, a, RmwOp::Ror); }

            // === INC / DEC memory ===
            0xE6 => { let a = self.addr_direct(bus); self.rmw(bus, a, RmwOp::Inc); }
            0xF6 => { let a = self.addr_direct_x(bus); self.rmw(bus, a, RmwOp::Inc); }
            0xEE => { let a = self.addr_absolute(bus); self.rmw(bus, a, RmwOp::Inc); }
            0xFE => { let a = self.addr_absolute_x(bus); self.rmw(bus, a, RmwOp::Inc); }
            0xC6 => { let a = self.addr_direct(bus); self.rmw(bus, a, RmwOp::Dec); }
            0xD6 => { let a = self.addr_direct_x(bus); self.rmw(bus, a, RmwOp::Dec); }
            0xCE => { let a = self.addr_absolute(bus); self.rmw(bus, a, RmwOp::Dec); }
            0xDE => { let a = self.addr_absolute_x(bus); self.rmw(bus, a, RmwOp::Dec); }

            // === TSB / TRB ===
            0x04 => { let a = self.addr_direct(bus); self.rmw(bus, a, RmwOp::Tsb); }
            0x0C => { let a = self.addr_absolute(bus); self.rmw(bus, a, RmwOp::Tsb); }
            0x14 => { let a = self.addr_direct(bus); self.rmw(bus, a, RmwOp::Trb); }
            0x1C => { let a = self.addr_absolute(bus); self.rmw(bus, a, RmwOp::Trb); }

            // === MVN / MVP (block move) ===
            0x54 => self.do_block_move(bus, false),
            0x44 => self.do_block_move(bus, true),

            // === BRK / COP (software interrupts) ===
            0x00 => self.do_brk(bus),
            0x02 => self.do_cop(bus),

            // === WAI / STP ===
            0xCB => {
                self.waiting = true;
                bus.idle();
                bus.idle();
            }
            0xDB => {
                self.stopped = true;
                bus.idle();
                bus.idle();
            }

            // === Misc ===
            0xEA => {} // NOP
            0x42 => {
                // WDM - 2-byte reserved NOP
                let _ = self.fetch_u8(bus);
            }

        }
    }

    // Per-mnemonic helpers. Each takes the resolved 24-bit
    // effective address and does the width-aware load/store +
    // flag update. The dispatch arms call the relevant
    // `addr_*` resolver inline first.

    fn do_lda(&mut self, bus: &mut impl SnesBus, addr: u32) {
        let v = self.load_a_width(bus, addr);
        self.write_a_width(v);
        self.set_nz_m(v);
    }

    fn do_ldx(&mut self, bus: &mut impl SnesBus, addr: u32) {
        let v = self.load_x_width(bus, addr);
        self.x = if self.p.x { v & 0xFF } else { v };
        self.set_nz_x(self.read_x_width());
    }

    fn do_ldy(&mut self, bus: &mut impl SnesBus, addr: u32) {
        let v = self.load_x_width(bus, addr);
        self.y = if self.p.x { v & 0xFF } else { v };
        self.set_nz_x(self.read_y_width());
    }

    fn do_sta(&mut self, bus: &mut impl SnesBus, addr: u32) {
        let v = self.read_a_width();
        self.store_a_width(bus, addr, v);
    }

    fn do_stx(&mut self, bus: &mut impl SnesBus, addr: u32) {
        let v = self.read_x_width();
        self.store_x_width(bus, addr, v);
    }

    fn do_sty(&mut self, bus: &mut impl SnesBus, addr: u32) {
        let v = self.read_y_width();
        self.store_x_width(bus, addr, v);
    }

    fn do_stz(&mut self, bus: &mut impl SnesBus, addr: u32) {
        self.store_a_width(bus, addr, 0);
    }

    /// Width-aware immediate-operand fetch tied to the m flag.
    fn imm_a_width(&mut self, bus: &mut impl SnesBus) -> u16 {
        if self.p.m {
            self.fetch_u8(bus) as u16
        } else {
            self.fetch_u16(bus)
        }
    }

    /// Width-aware immediate-operand fetch tied to the x flag (for
    /// CPX/CPY immediate).
    fn imm_x_width(&mut self, bus: &mut impl SnesBus) -> u16 {
        if self.p.x {
            self.fetch_u8(bus) as u16
        } else {
            self.fetch_u16(bus)
        }
    }

    /// AND/ORA/EOR/ADC/SBC/CMP common: load operand at `addr` with
    /// m-width, dispatch the ALU op.
    fn alu_a(&mut self, bus: &mut impl SnesBus, addr: u32, op: AluOp) {
        let v = self.load_a_width(bus, addr);
        match op {
            AluOp::And => self.do_and(v),
            AluOp::Ora => self.do_ora(v),
            AluOp::Eor => self.do_eor(v),
            AluOp::Adc => self.do_adc(v),
            AluOp::Sbc => self.do_sbc(v),
            AluOp::Cmp => self.do_cmp_a(v),
        }
    }

    fn do_and(&mut self, v: u16) {
        let a = self.read_a_width();
        let r = a & v;
        self.write_a_width(r);
        self.set_nz_m(r);
    }

    fn do_ora(&mut self, v: u16) {
        let a = self.read_a_width();
        let r = a | v;
        self.write_a_width(r);
        self.set_nz_m(r);
    }

    fn do_eor(&mut self, v: u16) {
        let a = self.read_a_width();
        let r = a ^ v;
        self.write_a_width(r);
        self.set_nz_m(r);
    }

    /// ADC. Width follows P.m. Decimal-mode (P.D=1) does the BCD
    /// adjustment per WDC's official algorithm; binary mode does
    /// the standard signed-overflow + carry-out math. Algorithm
    /// shape paraphrased from Mesen2 `SnesCpu.Instructions.h::Add8/16`
    /// (Core/SNES/SnesCpu.Instructions.h:4-71). Decimal mode also
    /// charges one extra internal cycle per the WDC datasheet.
    fn do_adc(&mut self, v: u16) {
        let a = self.read_a_width();
        let c = self.p.c as u32;
        if self.p.m {
            let lhs = a as u8 as u32;
            let rhs = v as u8 as u32;
            let mut r;
            if self.p.d {
                let mut nib = (lhs & 0x0F) + (rhs & 0x0F) + c;
                if nib > 0x09 {
                    nib += 0x06;
                }
                r = (lhs & 0xF0) + (rhs & 0xF0)
                    + (if nib > 0x0F { 0x10 } else { 0 })
                    + (nib & 0x0F);
            } else {
                r = lhs + rhs + c;
            }
            // V: signed-overflow on the binary intermediate.
            self.p.v = ((!(lhs ^ rhs)) & (lhs ^ r) & 0x80) != 0;
            if self.p.d && r > 0x9F {
                r += 0x60;
            }
            self.p.c = r > 0xFF;
            let r8 = r as u8;
            self.write_a_width(r8 as u16);
            self.set_nz_u8(r8);
        } else {
            let lhs = a as u32;
            let rhs = v as u32;
            let mut r;
            if self.p.d {
                let mut nib = (lhs & 0x0F) + (rhs & 0x0F) + c;
                if nib > 0x09 {
                    nib += 0x06;
                }
                r = (lhs & 0xF0) + (rhs & 0xF0)
                    + (if nib > 0x0F { 0x10 } else { 0 })
                    + (nib & 0x0F);
                if r > 0x9F {
                    r += 0x60;
                }
                r = (lhs & 0xF00) + (rhs & 0xF00)
                    + (if r > 0xFF { 0x100 } else { 0 })
                    + (r & 0xFF);
                if r > 0x9FF {
                    r += 0x600;
                }
                r = (lhs & 0xF000) + (rhs & 0xF000)
                    + (if r > 0xFFF { 0x1000 } else { 0 })
                    + (r & 0xFFF);
            } else {
                r = lhs + rhs + c;
            }
            self.p.v = ((!(lhs ^ rhs)) & (lhs ^ r) & 0x8000) != 0;
            if self.p.d && r > 0x9FFF {
                r += 0x6000;
            }
            self.p.c = r > 0xFFFF;
            let r16 = r as u16;
            self.write_a_width(r16);
            self.set_nz_u16(r16);
        }
    }

    /// SBC. Per the WDC manual the binary path is ADC of the one's-
    /// complement with C as borrow-in. Decimal-mode gets its own
    /// nibble-walk that mirrors `Add` but uses `<= 0x0F`/`<= 0xFF`
    /// underflow probes and subtracts the adjustment. Algorithm
    /// from Mesen2 `Sub8/Sub16` (Core/SNES/SnesCpu.Instructions.h:82-149).
    fn do_sbc(&mut self, v: u16) {
        if !self.p.d {
            let mask = if self.p.m { 0x00FF } else { 0xFFFF };
            self.do_adc((!v) & mask);
            return;
        }
        // Decimal-mode SBC: caller passes the original operand and
        // we walk it as a "subtract via BCD adjust" rather than the
        // ones-complement-add trick.
        let a = self.read_a_width();
        let c = self.p.c as u32;
        if self.p.m {
            // Mesen2's Sub8 takes the inverted value; replicate.
            let lhs = a as u8 as u32;
            let rhs = (!v) as u8 as u32;
            let mut r;
            let mut nib = (lhs & 0x0F) + (rhs & 0x0F) + c;
            if nib <= 0x0F {
                nib = nib.wrapping_sub(0x06);
            }
            r = (lhs & 0xF0) + (rhs & 0xF0)
                + (if nib > 0x0F { 0x10 } else { 0 })
                + (nib & 0x0F);
            self.p.v = ((!(lhs ^ rhs)) & (lhs ^ r) & 0x80) != 0;
            if r <= 0xFF {
                r = r.wrapping_sub(0x60);
            }
            self.p.c = r > 0xFF;
            let r8 = r as u8;
            self.write_a_width(r8 as u16);
            self.set_nz_u8(r8);
        } else {
            let lhs = a as u32;
            let rhs = (!v) as u32;
            let mut r;
            let mut nib = (lhs & 0x0F) + (rhs & 0x0F) + c;
            if nib <= 0x0F {
                nib = nib.wrapping_sub(0x06);
            }
            r = (lhs & 0xF0) + (rhs & 0xF0)
                + (if nib > 0x0F { 0x10 } else { 0 })
                + (nib & 0x0F);
            if r <= 0xFF {
                r = r.wrapping_sub(0x60);
            }
            r = (lhs & 0xF00) + (rhs & 0xF00)
                + (if r > 0xFF { 0x100 } else { 0 })
                + (r & 0xFF);
            if r <= 0xFFF {
                r = r.wrapping_sub(0x600);
            }
            r = (lhs & 0xF000) + (rhs & 0xF000)
                + (if r > 0xFFF { 0x1000 } else { 0 })
                + (r & 0xFFF);
            self.p.v = ((!(lhs ^ rhs)) & (lhs ^ r) & 0x8000) != 0;
            if r <= 0xFFFF {
                r = r.wrapping_sub(0x6000);
            }
            self.p.c = r > 0xFFFF;
            let r16 = r as u16;
            self.write_a_width(r16);
            self.set_nz_u16(r16);
        }
    }

    fn do_cmp_a(&mut self, v: u16) {
        let a = self.read_a_width();
        if self.p.m {
            let lhs = a as u8;
            let rhs = v as u8;
            let r = lhs.wrapping_sub(rhs);
            self.p.c = lhs >= rhs;
            self.set_nz_u8(r);
        } else {
            let r = a.wrapping_sub(v);
            self.p.c = a >= v;
            self.set_nz_u16(r);
        }
    }

    fn do_cmp_x(&mut self, v: u16) {
        let x = self.read_x_width();
        if self.p.x {
            let lhs = x as u8;
            let rhs = v as u8;
            let r = lhs.wrapping_sub(rhs);
            self.p.c = lhs >= rhs;
            self.set_nz_u8(r);
        } else {
            let r = x.wrapping_sub(v);
            self.p.c = x >= v;
            self.set_nz_u16(r);
        }
    }

    fn do_cmp_y(&mut self, v: u16) {
        let y = self.read_y_width();
        if self.p.x {
            let lhs = y as u8;
            let rhs = v as u8;
            let r = lhs.wrapping_sub(rhs);
            self.p.c = lhs >= rhs;
            self.set_nz_u8(r);
        } else {
            let r = y.wrapping_sub(v);
            self.p.c = y >= v;
            self.set_nz_u16(r);
        }
    }

    /// BIT against memory: N = bit (m_width - 1) of operand,
    /// V = bit (m_width - 2) of operand, Z = (a AND operand) == 0.
    fn do_bit(&mut self, bus: &mut impl SnesBus, addr: u32) {
        let v = self.load_a_width(bus, addr);
        let a = self.read_a_width();
        if self.p.m {
            let v8 = v as u8;
            self.p.n = v8 & 0x80 != 0;
            self.p.v = v8 & 0x40 != 0;
            self.p.z = (a as u8 & v8) == 0;
        } else {
            self.p.n = v & 0x8000 != 0;
            self.p.v = v & 0x4000 != 0;
            self.p.z = (a & v) == 0;
        }
    }

    fn do_asl_a(&mut self) {
        let a = self.read_a_width();
        if self.p.m {
            let a8 = a as u8;
            self.p.c = a8 & 0x80 != 0;
            let r = a8 << 1;
            self.write_a_width(r as u16);
            self.set_nz_u8(r);
        } else {
            self.p.c = a & 0x8000 != 0;
            let r = a << 1;
            self.write_a_width(r);
            self.set_nz_u16(r);
        }
    }

    fn do_lsr_a(&mut self) {
        let a = self.read_a_width();
        if self.p.m {
            let a8 = a as u8;
            self.p.c = a8 & 1 != 0;
            let r = a8 >> 1;
            self.write_a_width(r as u16);
            self.set_nz_u8(r);
        } else {
            self.p.c = a & 1 != 0;
            let r = a >> 1;
            self.write_a_width(r);
            self.set_nz_u16(r);
        }
    }

    fn do_rol_a(&mut self) {
        let a = self.read_a_width();
        let cin = self.p.c as u16;
        if self.p.m {
            let a8 = a as u8;
            self.p.c = a8 & 0x80 != 0;
            let r = ((a8 << 1) as u16 | cin) as u8;
            self.write_a_width(r as u16);
            self.set_nz_u8(r);
        } else {
            self.p.c = a & 0x8000 != 0;
            let r = (a << 1) | cin;
            self.write_a_width(r);
            self.set_nz_u16(r);
        }
    }

    fn do_ror_a(&mut self) {
        let a = self.read_a_width();
        if self.p.m {
            let a8 = a as u8;
            let cin = (self.p.c as u8) << 7;
            self.p.c = a8 & 1 != 0;
            let r = (a8 >> 1) | cin;
            self.write_a_width(r as u16);
            self.set_nz_u8(r);
        } else {
            let cin = (self.p.c as u16) << 15;
            self.p.c = a & 1 != 0;
            let r = (a >> 1) | cin;
            self.write_a_width(r);
            self.set_nz_u16(r);
        }
    }

    /// Read-modify-write to memory at `addr` with m-width. The real
    /// 65C816 does read, internal cycle, write; we model the
    /// internal cycle with one `bus.idle()`.
    fn rmw(&mut self, bus: &mut impl SnesBus, addr: u32, op: RmwOp) {
        let v = self.load_a_width(bus, addr);
        bus.idle();
        let r = match op {
            RmwOp::Asl => {
                if self.p.m {
                    self.p.c = v & 0x80 != 0;
                    let r = (v as u8) << 1;
                    self.set_nz_u8(r);
                    r as u16
                } else {
                    self.p.c = v & 0x8000 != 0;
                    let r = v << 1;
                    self.set_nz_u16(r);
                    r
                }
            }
            RmwOp::Lsr => {
                self.p.c = v & 1 != 0;
                let r = v >> 1;
                if self.p.m {
                    self.set_nz_u8(r as u8);
                } else {
                    self.set_nz_u16(r);
                }
                r
            }
            RmwOp::Rol => {
                let cin = self.p.c as u16;
                if self.p.m {
                    self.p.c = v & 0x80 != 0;
                    let r = (((v as u8) << 1) as u16 | cin) as u8;
                    self.set_nz_u8(r);
                    r as u16
                } else {
                    self.p.c = v & 0x8000 != 0;
                    let r = (v << 1) | cin;
                    self.set_nz_u16(r);
                    r
                }
            }
            RmwOp::Ror => {
                if self.p.m {
                    let cin = (self.p.c as u8) << 7;
                    self.p.c = v & 1 != 0;
                    let r = ((v as u8) >> 1) | cin;
                    self.set_nz_u8(r);
                    r as u16
                } else {
                    let cin = (self.p.c as u16) << 15;
                    self.p.c = v & 1 != 0;
                    let r = (v >> 1) | cin;
                    self.set_nz_u16(r);
                    r
                }
            }
            RmwOp::Inc => {
                let r = v.wrapping_add(1);
                let r = if self.p.m { r & 0xFF } else { r };
                self.set_nz_m(r);
                r
            }
            RmwOp::Dec => {
                let r = v.wrapping_sub(1);
                let r = if self.p.m { r & 0xFF } else { r };
                self.set_nz_m(r);
                r
            }
            RmwOp::Tsb => {
                let a = self.read_a_width();
                self.p.z = (a & v) == 0;
                a | v
            }
            RmwOp::Trb => {
                let a = self.read_a_width();
                self.p.z = (a & v) == 0;
                v & !a
            }
        };
        self.store_a_width_rmw(bus, addr, r);
    }

    /// MVN ($54) and MVP ($44). Operand byte order in the
    /// instruction stream is `[opcode] [destbank] [srcbank]`.
    /// Iterates A+1 times: copy one byte, advance/retreat X and Y,
    /// decrement A. DBR persists at the destination bank after
    /// completion.
    fn do_block_move(&mut self, bus: &mut impl SnesBus, decrement: bool) {
        let dest_bank = self.fetch_u8(bus);
        let src_bank = self.fetch_u8(bus);
        self.dbr = dest_bank;
        loop {
            let src = ((src_bank as u32) << 16) | self.x as u32;
            let dest = ((dest_bank as u32) << 16) | self.y as u32;
            let v = self.read8(bus, src);
            self.write8(bus, dest, v);
            bus.idle();
            bus.idle();
            if decrement {
                self.x = if self.p.x {
                    (self.x & 0xFF00) | ((self.x.wrapping_sub(1)) & 0xFF)
                } else {
                    self.x.wrapping_sub(1)
                };
                self.y = if self.p.x {
                    (self.y & 0xFF00) | ((self.y.wrapping_sub(1)) & 0xFF)
                } else {
                    self.y.wrapping_sub(1)
                };
            } else {
                self.x = if self.p.x {
                    (self.x & 0xFF00) | ((self.x.wrapping_add(1)) & 0xFF)
                } else {
                    self.x.wrapping_add(1)
                };
                self.y = if self.p.x {
                    (self.y & 0xFF00) | ((self.y.wrapping_add(1)) & 0xFF)
                } else {
                    self.y.wrapping_add(1)
                };
            }
            self.c = self.c.wrapping_sub(1);
            if self.c == 0xFFFF {
                break;
            }
        }
    }

    /// BRK ($00). Two-byte instruction (opcode + signature byte).
    /// Advances PC past the signature, pushes PBR (native only),
    /// PC, P (with B = 1), then loads the BRK vector.
    fn do_brk(&mut self, bus: &mut impl SnesBus) {
        let _sig = self.fetch_u8(bus);
        let p_pushed = self.p.pack(self.mode) | 0x10; // software push sets B
        match self.mode {
            Mode::Native => {
                self.push8(bus, self.pbr);
                self.push16(bus, self.pc);
                self.push8(bus, p_pushed);
            }
            Mode::Emulation => {
                self.push16(bus, self.pc);
                self.push8(bus, p_pushed);
            }
        }
        self.p.d = false;
        self.p.i = true;
        self.pbr = 0;
        let vec_addr = match self.mode {
            Mode::Native => 0x00FFE6,
            Mode::Emulation => 0x00FFFE,
        };
        let lo = bus.read(vec_addr) as u16;
        let hi = bus.read(vec_addr + 1) as u16;
        self.pc = (hi << 8) | lo;
    }

    /// COP ($02). Same shape as BRK but its own vector pair.
    fn do_cop(&mut self, bus: &mut impl SnesBus) {
        let _sig = self.fetch_u8(bus);
        let p_pushed = self.p.pack(self.mode) | 0x10;
        match self.mode {
            Mode::Native => {
                self.push8(bus, self.pbr);
                self.push16(bus, self.pc);
                self.push8(bus, p_pushed);
            }
            Mode::Emulation => {
                self.push16(bus, self.pc);
                self.push8(bus, p_pushed);
            }
        }
        self.p.d = false;
        self.p.i = true;
        self.pbr = 0;
        let vec_addr = match self.mode {
            Mode::Native => 0x00FFE4,
            Mode::Emulation => 0x00FFF4,
        };
        let lo = bus.read(vec_addr) as u16;
        let hi = bus.read(vec_addr + 1) as u16;
        self.pc = (hi << 8) | lo;
    }

    fn branch(&mut self, bus: &mut impl SnesBus, taken: bool) {
        let off = self.fetch_u8(bus) as i8 as i32;
        if taken {
            bus.idle();
            let new_pc = (self.pc as i32).wrapping_add(off) as u16;
            // Emulation mode charges an extra internal cycle when
            // the taken branch crosses a page (low-byte rollover).
            // Mirrors Mesen2 `BranchRelative`
            // (Core/SNES/SnesCpu.Instructions.h:163-176).
            if self.mode == Mode::Emulation && (new_pc & 0xFF00) != (self.pc & 0xFF00) {
                bus.idle();
            }
            self.pc = new_pc;
        }
    }

    fn branch_long(&mut self, bus: &mut impl SnesBus) {
        let off = self.fetch_u16(bus) as i16 as i32;
        bus.idle();
        self.pc = (self.pc as i32).wrapping_add(off) as u16;
    }
}

/// Tag the binary ALU opcode the dispatch table is invoking.
/// Lets the load/operate/store sequence share `alu_a` instead of
/// duplicating it per mnemonic.
#[derive(Debug, Clone, Copy)]
enum AluOp {
    And,
    Ora,
    Eor,
    Adc,
    Sbc,
    Cmp,
}

/// Tag the read-modify-write opcode the dispatch table is invoking.
/// `rmw` does the load + idle + width-specific op + store with the
/// per-op variation living inside the match.
#[derive(Debug, Clone, Copy)]
enum RmwOp {
    Asl,
    Lsr,
    Rol,
    Ror,
    Inc,
    Dec,
    Tsb,
    Trb,
}

#[cfg(test)]
mod tests;
