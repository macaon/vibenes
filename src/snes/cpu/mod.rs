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
        let hi = self.read8(bus, (ptr & 0xFF_0000) | (ptr.wrapping_add(1) & 0xFFFF)) as u32;
        ((self.dbr as u32) << 16) | (hi << 8) | lo
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
        let hi = self.read8(bus, (ptr & 0xFF_0000) | (ptr.wrapping_add(1) & 0xFFFF)) as u32;
        ((self.dbr as u32) << 16) | (hi << 8) | lo
    }

    fn addr_direct_indirect_y(&mut self, bus: &mut impl SnesBus) -> u32 {
        let dp = self.fetch_u8(bus) as u16;
        if self.d & 0xFF != 0 {
            bus.idle();
        }
        let ptr = self.d.wrapping_add(dp) as u32;
        let lo = self.read8(bus, ptr) as u32;
        let hi = self.read8(bus, (ptr & 0xFF_0000) | (ptr.wrapping_add(1) & 0xFFFF)) as u32;
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
            0xAA => {
                // TAX
                let v = self.read_a_width();
                if self.p.x {
                    self.x = (self.x & 0xFF00) | (v & 0xFF);
                } else {
                    self.x = v;
                }
                self.set_nz_x(self.read_x_width());
            }
            0x8A => {
                // TXA
                let v = self.read_x_width();
                self.write_a_width(v);
                self.set_nz_m(self.read_a_width());
            }
            0xA8 => {
                // TAY
                let v = self.read_a_width();
                if self.p.x {
                    self.y = (self.y & 0xFF00) | (v & 0xFF);
                } else {
                    self.y = v;
                }
                self.set_nz_x(self.read_y_width());
            }
            0x98 => {
                // TYA
                let v = self.read_y_width();
                self.write_a_width(v);
                self.set_nz_m(self.read_a_width());
            }
            0x9B => {
                // TXY
                self.y = self.x;
                self.enforce_index_width();
                self.set_nz_x(self.read_y_width());
            }
            0xBB => {
                // TYX
                self.x = self.y;
                self.enforce_index_width();
                self.set_nz_x(self.read_x_width());
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

            // === Misc ===
            0xEA => {} // NOP
            0x42 => {
                // WDM - 2-byte reserved NOP
                let _ = self.fetch_u8(bus);
            }

            other => {
                panic!(
                    "snes::cpu: unimplemented opcode {:#04X} at {:02X}:{:04X}",
                    other,
                    self.pbr,
                    self.pc.wrapping_sub(1)
                );
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

    fn branch(&mut self, bus: &mut impl SnesBus, taken: bool) {
        let off = self.fetch_u8(bus) as i8 as i32;
        if taken {
            bus.idle();
            self.pc = (self.pc as i32).wrapping_add(off) as u16;
        }
    }

    fn branch_long(&mut self, bus: &mut impl SnesBus) {
        let off = self.fetch_u16(bus) as i16 as i32;
        bus.idle();
        self.pc = (self.pc as i32).wrapping_add(off) as u16;
    }
}

#[cfg(test)]
mod tests;
