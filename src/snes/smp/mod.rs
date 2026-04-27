// SPDX-License-Identifier: GPL-3.0-or-later
//! Sony SPC700 (S-SMP) CPU core. The SNES audio CPU is an 8-bit
//! 6502-shaped (but not 6502-compatible) processor running at
//! ~1.024 MHz. It owns 64 KiB of ARAM, three timers (8 / 8 / 64 kHz),
//! and the four mailbox bytes that connect it to the host 65C816 at
//! `$2140-$2143` / `$F4-$F7`.
//!
//! ## Phase 5a status
//!
//! This commit lands the foundation for the SMP: register/flag/state
//! model, reset, fetch helpers, and a 256-entry dispatch table. A
//! starter set of opcodes is implemented (NOP, transfers, immediate
//! loads, direct-page and absolute MOV, branches, stack push/pop,
//! INC/DEC of registers, JMP, RET, and PSW manipulation). Every
//! other opcode panics with its hex value so a missing implementation
//! is loud rather than silent. Subsequent commits expand into ALU
//! (ADC/SBC/CMP/AND/OR/EOR), shift/rotate, MOV with addressing
//! variants, MUL / DIV / MOVW (16-bit YA), bit manipulation
//! (SET1/CLR1/TSET1/TCLR1/AND1/OR1/EOR1/NOT1/MOV1), TCALL/PCALL/CALL,
//! BBS/BBC/CBNE/DBNZ, DAA/DAS, XCN, and the IPL-on shadow + I/O
//! page handling that sub-phase 5c needs.
//!
//! References (paraphrased; clean-room-adjacent porting per project
//! policy): nes-expert SNES APU reference at
//! `~/.claude/skills/nes-expert/reference/snes-apu.md`,
//! Mesen2 `Core/SNES/Spc.h`, `Spc.cpp`, `Spc.Instructions.cpp` for
//! the dispatch shape and per-instruction cycle accounting,
//! higan `higan/sfc/smp/` for the IPL handshake reference,
//! Anomie's "SPC700 Reference Notes" + snes.nesdev.org wiki SPC700
//! pages for the ISA tables and quirks (TSET1/TCLR1, DIV overflow,
//! direct-page wrap).

pub mod bus;
pub mod ipl;

#[cfg(test)]
mod tests;

use bus::SmpBus;

/// Status register (PSW) - `N V P B H I Z C`.
///
/// Stored as discrete bools rather than packed bits so each opcode
/// reads cleanly; [`Status::pack`] / [`Status::unpack`] convert at
/// PHP / PLP / RETI / interrupt boundaries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Status {
    pub n: bool,
    pub v: bool,
    /// Direct-page select. `false` -> DP at `$00xx`, `true` -> DP at
    /// `$01xx`. Set by SETP, cleared by CLRP. Most code keeps P=0.
    pub p: bool,
    /// Software break. Set by BRK, observed only on the stack.
    pub b: bool,
    /// Half-carry, used by DAA/DAS.
    pub h: bool,
    /// Master interrupt enable. The SNES does not wire any interrupt
    /// to the SMP, so I has no behavioural effect on real hardware,
    /// but EI / DI / RETI still maintain the bit so SPC dumps round-
    /// trip cleanly.
    pub i: bool,
    pub z: bool,
    pub c: bool,
}

impl Status {
    /// Pack into the on-stack / `MOV PSW,A` byte layout:
    /// `N V P B H I Z C` from MSB to LSB.
    pub fn pack(&self) -> u8 {
        let mut p = 0u8;
        if self.n {
            p |= 0x80;
        }
        if self.v {
            p |= 0x40;
        }
        if self.p {
            p |= 0x20;
        }
        if self.b {
            p |= 0x10;
        }
        if self.h {
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

    pub fn unpack(&mut self, byte: u8) {
        self.n = byte & 0x80 != 0;
        self.v = byte & 0x40 != 0;
        self.p = byte & 0x20 != 0;
        self.b = byte & 0x10 != 0;
        self.h = byte & 0x08 != 0;
        self.i = byte & 0x04 != 0;
        self.z = byte & 0x02 != 0;
        self.c = byte & 0x01 != 0;
    }
}

/// SPC700 architectural state.
///
/// The accumulator pair `YA` is exposed as the helper [`Smp::ya`]
/// rather than a stored 16-bit register; real silicon keeps Y and A
/// as separate 8-bit cells and only ALU ops like MUL / DIV / MOVW /
/// ADDW / SUBW / CMPW treat them as a wide register.
#[derive(Debug, Clone, Copy)]
pub struct Smp {
    pub a: u8,
    pub x: u8,
    pub y: u8,
    /// Stack pointer. The actual stack lives at `$0100 | sp`.
    pub sp: u8,
    pub pc: u16,
    pub psw: Status,
    /// `true` while the SMP is halted by `STOP`. Only `/RESET`
    /// clears this; the SNES has no way to un-stop the SMP at
    /// runtime - games never use it.
    pub stopped: bool,
    /// `true` while the SMP is parked in `SLEEP`. Cleared by any
    /// pending interrupt; the SMP is not actually wired for IRQ on
    /// SNES hardware so this stays true forever in practice on
    /// real games. Preserved for SPC dump round-tripping.
    pub sleeping: bool,
}

impl Smp {
    pub fn new() -> Self {
        Self {
            a: 0,
            x: 0,
            y: 0,
            sp: 0xFF,
            pc: 0,
            psw: Status::default(),
            stopped: false,
            sleeping: false,
        }
    }

    /// Combined Y:A 16-bit accessor (Y is the high byte). Used by
    /// MOVW / ADDW / SUBW / CMPW / MUL / DIV in later commits.
    pub fn ya(&self) -> u16 {
        ((self.y as u16) << 8) | self.a as u16
    }

    pub fn set_ya(&mut self, value: u16) {
        self.a = value as u8;
        self.y = (value >> 8) as u8;
    }

    /// Reset (cold boot or `/RESET`). Loads PC from `$FFFE-$FFFF`
    /// (which inside the canonical IPL ROM points back to `$FFC0`,
    /// the IPL entry point), zeroes PSW, and parks SP in page 1.
    /// The IPL itself sets SP to `$EF` as its first instruction; we
    /// pick `$FF` here so a unit test that bypasses the IPL still
    /// has a usable stack.
    pub fn reset(&mut self, bus: &mut impl SmpBus) {
        self.a = 0;
        self.x = 0;
        self.y = 0;
        self.sp = 0xFF;
        self.psw = Status::default();
        self.stopped = false;
        self.sleeping = false;
        let lo = bus.read(0xFFFE) as u16;
        let hi = bus.read(0xFFFF) as u16;
        self.pc = (hi << 8) | lo;
    }

    /// Drive one instruction. Returns the opcode that ran (handy
    /// for tests + tracing). STOP and SLEEP advance the bus by one
    /// idle cycle without retiring an instruction.
    pub fn step(&mut self, bus: &mut impl SmpBus) -> u8 {
        if self.stopped {
            bus.idle();
            return 0xFF; // STOP itself
        }
        if self.sleeping {
            bus.idle();
            return 0xEF; // SLEEP itself
        }
        let opcode = self.fetch_op(bus);
        self.dispatch(opcode, bus);
        opcode
    }

    fn fetch_op(&mut self, bus: &mut impl SmpBus) -> u8 {
        let pc = self.pc;
        self.pc = self.pc.wrapping_add(1);
        bus.read(pc)
    }

    fn fetch_u8(&mut self, bus: &mut impl SmpBus) -> u8 {
        let pc = self.pc;
        self.pc = self.pc.wrapping_add(1);
        bus.read(pc)
    }

    fn fetch_u16(&mut self, bus: &mut impl SmpBus) -> u16 {
        let lo = self.fetch_u8(bus) as u16;
        let hi = self.fetch_u8(bus) as u16;
        (hi << 8) | lo
    }

    /// Direct-page address resolution. P=0 -> page `$00`; P=1 ->
    /// page `$01`. Selectable per-instruction by toggling PSW.P.
    fn dp_addr(&self, offset: u8) -> u16 {
        let page = if self.psw.p { 0x0100 } else { 0x0000 };
        page | offset as u16
    }

    fn read_dp(&self, bus: &mut impl SmpBus, offset: u8) -> u8 {
        bus.read(self.dp_addr(offset))
    }

    fn write_dp(&self, bus: &mut impl SmpBus, offset: u8, value: u8) {
        bus.write(self.dp_addr(offset), value);
    }

    fn push8(&mut self, bus: &mut impl SmpBus, value: u8) {
        let addr = 0x0100 | self.sp as u16;
        bus.write(addr, value);
        self.sp = self.sp.wrapping_sub(1);
    }

    fn pop8(&mut self, bus: &mut impl SmpBus) -> u8 {
        self.sp = self.sp.wrapping_add(1);
        let addr = 0x0100 | self.sp as u16;
        bus.read(addr)
    }

    /// Push a 16-bit value high-byte-first so a matching [`Smp::pop16`]
    /// reproduces the original word. CALL / PCALL / TCALL land in
    /// the next sub-phase and consume this helper.
    #[allow(dead_code)]
    fn push16(&mut self, bus: &mut impl SmpBus, value: u16) {
        self.push8(bus, (value >> 8) as u8);
        self.push8(bus, value as u8);
    }

    fn pop16(&mut self, bus: &mut impl SmpBus) -> u16 {
        let lo = self.pop8(bus) as u16;
        let hi = self.pop8(bus) as u16;
        (hi << 8) | lo
    }

    fn set_nz(&mut self, value: u8) {
        self.psw.n = value & 0x80 != 0;
        self.psw.z = value == 0;
    }

    // ----- Addressing-mode resolvers ----------------------------------
    //
    // Each helper consumes the addressing-mode operand bytes from the
    // instruction stream (when applicable) and returns the effective
    // 16-bit address. Bus reads of the OPERAND BYTES are charged here;
    // the data read/write is done by the caller so RMW ops can see the
    // address before reading the value.

    /// `(X)` - direct page indexed by X. No idle, no operand fetch:
    /// the addressing mode is implicit, charged 1 idle cycle by
    /// callers (e.g. `MOV A,(X)` is 3 cycles = opcode + idle + read).
    fn addr_dp_x_direct(&self) -> u16 {
        self.dp_addr(self.x)
    }

    /// `(Y)` - direct page indexed by Y. Same pattern as `(X)`.
    fn addr_dp_y_direct(&self) -> u16 {
        self.dp_addr(self.y)
    }

    /// `dp+X` - direct page plus X (no indirection). Spec charges an
    /// extra idle cycle for the X-add in addition to the base dp
    /// access cost. Returns the effective dp page address.
    fn addr_dp_plus_x(&mut self, bus: &mut impl SmpBus) -> u16 {
        let dp = self.fetch_u8(bus);
        bus.idle();
        self.dp_addr(dp.wrapping_add(self.x))
    }

    /// `dp+Y` - direct page plus Y (no indirection).
    #[allow(dead_code)]
    fn addr_dp_plus_y(&mut self, bus: &mut impl SmpBus) -> u16 {
        let dp = self.fetch_u8(bus);
        bus.idle();
        self.dp_addr(dp.wrapping_add(self.y))
    }

    /// `!abs+X` - absolute plus X. Idle cycle for the index add.
    fn addr_abs_plus_x(&mut self, bus: &mut impl SmpBus) -> u16 {
        let base = self.fetch_u16(bus);
        bus.idle();
        base.wrapping_add(self.x as u16)
    }

    /// `!abs+Y` - absolute plus Y.
    fn addr_abs_plus_y(&mut self, bus: &mut impl SmpBus) -> u16 {
        let base = self.fetch_u16(bus);
        bus.idle();
        base.wrapping_add(self.y as u16)
    }

    /// `[dp+X]` - indirect via dp word at `dp+X` (read 2 bytes from
    /// dp page). Spec: opcode + dp_fetch + idle + ptr_lo + ptr_hi +
    /// read_target. The pointer read crosses inside the dp page; the
    /// high byte's offset wraps modulo 256 within the page (Mesen2
    /// `Spc::GetIndIndexedDpAddr`).
    fn addr_dp_x_indirect(&mut self, bus: &mut impl SmpBus) -> u16 {
        let dp = self.fetch_u8(bus);
        bus.idle();
        let ptr_offset = dp.wrapping_add(self.x);
        let lo = self.read_dp(bus, ptr_offset) as u16;
        let hi = self.read_dp(bus, ptr_offset.wrapping_add(1)) as u16;
        (hi << 8) | lo
    }

    /// `[dp]+Y` - indirect via dp word at `dp`, then Y added to the
    /// pointer. Spec: opcode + dp_fetch + ptr_lo + ptr_hi + idle +
    /// read_target.
    fn addr_dp_indirect_plus_y(&mut self, bus: &mut impl SmpBus) -> u16 {
        let dp = self.fetch_u8(bus);
        let lo = self.read_dp(bus, dp) as u16;
        let hi = self.read_dp(bus, dp.wrapping_add(1)) as u16;
        bus.idle();
        ((hi << 8) | lo).wrapping_add(self.y as u16)
    }

    /// ADC byte-level core. Performs `a + b + C`, sets N/V/H/Z/C
    /// per the SPC700 spec. Returns the 8-bit result.
    fn adc_byte(&mut self, a: u8, b: u8) -> u8 {
        let c = if self.psw.c { 1u16 } else { 0 };
        let sum = a as u16 + b as u16 + c;
        self.psw.c = sum > 0xFF;
        let result = sum as u8;
        // Half-carry: low nibble overflow into bit 4.
        self.psw.h = ((a & 0x0F) + (b & 0x0F) + c as u8) > 0x0F;
        // Signed overflow: operands share sign, result differs.
        self.psw.v = ((a ^ result) & (b ^ result) & 0x80) != 0;
        self.set_nz(result);
        result
    }

    /// Take a signed 8-bit branch. Charged 2 extra cycles when the
    /// branch is taken (Mesen2 `BranchRelative`); the not-taken path
    /// already paid for the operand fetch.
    fn branch_taken(&mut self, bus: &mut impl SmpBus, offset: i8) {
        bus.idle();
        bus.idle();
        self.pc = self.pc.wrapping_add(offset as i16 as u16);
    }

    /// Master dispatch. Each arm matches the opcode by hex; helpers
    /// keep the bodies short. Unimplemented opcodes panic loudly so
    /// missing coverage shows up in the first failing test rather
    /// than as silent corruption.
    fn dispatch(&mut self, opcode: u8, bus: &mut impl SmpBus) {
        match opcode {
            // --- 0x00 row ---
            0x00 => self.op_nop(bus),

            // --- PSW manipulation ---
            0x60 => self.op_clrc(bus),
            0x80 => self.op_setc(bus),
            0xED => self.op_notc(bus),
            0x20 => self.op_clrp(bus),
            0x40 => self.op_setp(bus),
            0xE0 => self.op_clrv(bus),
            0xA0 => self.op_ei(bus),
            0xC0 => self.op_di(bus),

            // --- Halts ---
            0xEF => self.op_sleep(bus),
            0xFF => self.op_stop(bus),

            // --- Register transfers ---
            0x7D => self.op_mov_a_x(bus),
            0xDD => self.op_mov_a_y(bus),
            0x5D => self.op_mov_x_a(bus),
            0xFD => self.op_mov_y_a(bus),
            0x9D => self.op_mov_x_sp(bus),
            0xBD => self.op_mov_sp_x(bus),

            // --- INC / DEC of registers ---
            0xBC => self.op_inc_a(bus),
            0x9C => self.op_dec_a(bus),
            0x3D => self.op_inc_x(bus),
            0x1D => self.op_dec_x(bus),
            0xFC => self.op_inc_y(bus),
            0xDC => self.op_dec_y(bus),

            // --- Immediate loads ---
            0xE8 => self.op_mov_a_imm(bus),
            0xCD => self.op_mov_x_imm(bus),
            0x8D => self.op_mov_y_imm(bus),

            // --- Direct-page loads ---
            0xE4 => self.op_mov_a_dp(bus),
            0xF8 => self.op_mov_x_dp(bus),
            0xEB => self.op_mov_y_dp(bus),

            // --- Direct-page stores ---
            0xC4 => self.op_mov_dp_a(bus),
            0xD8 => self.op_mov_dp_x(bus),
            0xCB => self.op_mov_dp_y(bus),

            // --- Absolute loads / stores ---
            0xE5 => self.op_mov_a_abs(bus),
            0xE9 => self.op_mov_x_abs(bus),
            0xEC => self.op_mov_y_abs(bus),
            0xC5 => self.op_mov_abs_a(bus),
            0xC9 => self.op_mov_abs_x(bus),
            0xCC => self.op_mov_abs_y(bus),

            // --- Stack ops ---
            0x2D => self.op_push_a(bus),
            0x4D => self.op_push_x(bus),
            0x6D => self.op_push_y(bus),
            0x0D => self.op_push_psw(bus),
            0xAE => self.op_pop_a(bus),
            0xCE => self.op_pop_x(bus),
            0xEE => self.op_pop_y(bus),
            0x8E => self.op_pop_psw(bus),

            // --- Branches ---
            0x2F => self.op_bra(bus),
            0xF0 => self.op_branch_if(bus, self.psw.z),
            0xD0 => self.op_branch_if(bus, !self.psw.z),
            0xB0 => self.op_branch_if(bus, self.psw.c),
            0x90 => self.op_branch_if(bus, !self.psw.c),
            0x30 => self.op_branch_if(bus, self.psw.n),
            0x10 => self.op_branch_if(bus, !self.psw.n),
            0x70 => self.op_branch_if(bus, self.psw.v),
            0x50 => self.op_branch_if(bus, !self.psw.v),

            // --- JMP / RET ---
            0x5F => self.op_jmp_abs(bus),
            0x6F => self.op_ret(bus),

            // --- ADC family (12 addressing modes) ---
            0x88 => self.op_adc_a_imm(bus),
            0x84 => self.op_adc_a_dp(bus),
            0x85 => self.op_adc_a_abs(bus),
            0x86 => self.op_adc_a_dp_x_direct(bus),
            0x94 => self.op_adc_a_dp_plus_x(bus),
            0x95 => self.op_adc_a_abs_plus_x(bus),
            0x96 => self.op_adc_a_abs_plus_y(bus),
            0x87 => self.op_adc_a_dp_x_indirect(bus),
            0x97 => self.op_adc_a_dp_indirect_plus_y(bus),
            0x99 => self.op_adc_x_indirect_y_indirect(bus),
            0x89 => self.op_adc_dp_dp(bus),
            0x98 => self.op_adc_dp_imm(bus),

            // --- SBC family (mirrors ADC; same 12 modes) ---
            0xA8 => self.op_sbc_a_imm(bus),
            0xA4 => self.op_sbc_a_dp(bus),
            0xA5 => self.op_sbc_a_abs(bus),
            0xA6 => self.op_sbc_a_dp_x_direct(bus),
            0xB4 => self.op_sbc_a_dp_plus_x(bus),
            0xB5 => self.op_sbc_a_abs_plus_x(bus),
            0xB6 => self.op_sbc_a_abs_plus_y(bus),
            0xA7 => self.op_sbc_a_dp_x_indirect(bus),
            0xB7 => self.op_sbc_a_dp_indirect_plus_y(bus),
            0xB9 => self.op_sbc_x_indirect_y_indirect(bus),
            0xA9 => self.op_sbc_dp_dp(bus),
            0xB8 => self.op_sbc_dp_imm(bus),

            other => panic!(
                "snes/smp: unimplemented opcode ${other:02X} at PC=${:04X}",
                self.pc.wrapping_sub(1)
            ),
        }
    }

    // ----- 0x00 / NOP --------------------------------------------------

    fn op_nop(&mut self, bus: &mut impl SmpBus) {
        // 2 cycles total; opcode fetch already paid 1.
        bus.idle();
    }

    // ----- PSW flag ops ------------------------------------------------

    fn op_clrc(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        self.psw.c = false;
    }

    fn op_setc(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        self.psw.c = true;
    }

    fn op_notc(&mut self, bus: &mut impl SmpBus) {
        // 3 cycles per spec; one extra idle beyond the simple flip.
        bus.idle();
        bus.idle();
        self.psw.c = !self.psw.c;
    }

    fn op_clrp(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        self.psw.p = false;
    }

    fn op_setp(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        self.psw.p = true;
    }

    fn op_clrv(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        self.psw.v = false;
        self.psw.h = false;
    }

    fn op_ei(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        bus.idle();
        self.psw.i = true;
    }

    fn op_di(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        bus.idle();
        self.psw.i = false;
    }

    // ----- Halts -------------------------------------------------------

    fn op_sleep(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        bus.idle();
        self.sleeping = true;
    }

    fn op_stop(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        bus.idle();
        self.stopped = true;
    }

    // ----- Register transfers (2 cycles each) -------------------------

    fn op_mov_a_x(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        self.a = self.x;
        self.set_nz(self.a);
    }

    fn op_mov_a_y(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        self.a = self.y;
        self.set_nz(self.a);
    }

    fn op_mov_x_a(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        self.x = self.a;
        self.set_nz(self.x);
    }

    fn op_mov_y_a(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        self.y = self.a;
        self.set_nz(self.y);
    }

    fn op_mov_x_sp(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        self.x = self.sp;
        self.set_nz(self.x);
    }

    fn op_mov_sp_x(&mut self, bus: &mut impl SmpBus) {
        // MOV SP,X does NOT update N/Z (spec quirk - SP transfers
        // never flag, mirrored by Mesen2 `MOV_SpX`).
        bus.idle();
        self.sp = self.x;
    }

    // ----- INC / DEC of registers (2 cycles each) ---------------------

    fn op_inc_a(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        self.a = self.a.wrapping_add(1);
        self.set_nz(self.a);
    }

    fn op_dec_a(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        self.a = self.a.wrapping_sub(1);
        self.set_nz(self.a);
    }

    fn op_inc_x(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        self.x = self.x.wrapping_add(1);
        self.set_nz(self.x);
    }

    fn op_dec_x(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        self.x = self.x.wrapping_sub(1);
        self.set_nz(self.x);
    }

    fn op_inc_y(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        self.y = self.y.wrapping_add(1);
        self.set_nz(self.y);
    }

    fn op_dec_y(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        self.y = self.y.wrapping_sub(1);
        self.set_nz(self.y);
    }

    // ----- Immediate loads (2 cycles each) ----------------------------

    fn op_mov_a_imm(&mut self, bus: &mut impl SmpBus) {
        let v = self.fetch_u8(bus);
        self.a = v;
        self.set_nz(v);
    }

    fn op_mov_x_imm(&mut self, bus: &mut impl SmpBus) {
        let v = self.fetch_u8(bus);
        self.x = v;
        self.set_nz(v);
    }

    fn op_mov_y_imm(&mut self, bus: &mut impl SmpBus) {
        let v = self.fetch_u8(bus);
        self.y = v;
        self.set_nz(v);
    }

    // ----- Direct-page loads (3 cycles each) --------------------------

    fn op_mov_a_dp(&mut self, bus: &mut impl SmpBus) {
        let dp = self.fetch_u8(bus);
        let v = self.read_dp(bus, dp);
        self.a = v;
        self.set_nz(v);
    }

    fn op_mov_x_dp(&mut self, bus: &mut impl SmpBus) {
        let dp = self.fetch_u8(bus);
        let v = self.read_dp(bus, dp);
        self.x = v;
        self.set_nz(v);
    }

    fn op_mov_y_dp(&mut self, bus: &mut impl SmpBus) {
        let dp = self.fetch_u8(bus);
        let v = self.read_dp(bus, dp);
        self.y = v;
        self.set_nz(v);
    }

    // ----- Direct-page stores (4 cycles each) -------------------------
    //
    // Spec calls these 4 cycles: opcode + dp fetch + dummy read of
    // destination + write. Mesen2 `STA` / `STX` / `STY` follow the
    // same pattern (read-before-write). Stores do NOT update N/Z.

    fn op_mov_dp_a(&mut self, bus: &mut impl SmpBus) {
        let dp = self.fetch_u8(bus);
        let _ = self.read_dp(bus, dp);
        self.write_dp(bus, dp, self.a);
    }

    fn op_mov_dp_x(&mut self, bus: &mut impl SmpBus) {
        let dp = self.fetch_u8(bus);
        let _ = self.read_dp(bus, dp);
        self.write_dp(bus, dp, self.x);
    }

    fn op_mov_dp_y(&mut self, bus: &mut impl SmpBus) {
        let dp = self.fetch_u8(bus);
        let _ = self.read_dp(bus, dp);
        self.write_dp(bus, dp, self.y);
    }

    // ----- Absolute loads (4 cycles each) -----------------------------

    fn op_mov_a_abs(&mut self, bus: &mut impl SmpBus) {
        let addr = self.fetch_u16(bus);
        let v = bus.read(addr);
        self.a = v;
        self.set_nz(v);
    }

    fn op_mov_x_abs(&mut self, bus: &mut impl SmpBus) {
        let addr = self.fetch_u16(bus);
        let v = bus.read(addr);
        self.x = v;
        self.set_nz(v);
    }

    fn op_mov_y_abs(&mut self, bus: &mut impl SmpBus) {
        let addr = self.fetch_u16(bus);
        let v = bus.read(addr);
        self.y = v;
        self.set_nz(v);
    }

    // ----- Absolute stores (5 cycles each) ----------------------------

    fn op_mov_abs_a(&mut self, bus: &mut impl SmpBus) {
        let addr = self.fetch_u16(bus);
        let _ = bus.read(addr);
        bus.write(addr, self.a);
    }

    fn op_mov_abs_x(&mut self, bus: &mut impl SmpBus) {
        let addr = self.fetch_u16(bus);
        let _ = bus.read(addr);
        bus.write(addr, self.x);
    }

    fn op_mov_abs_y(&mut self, bus: &mut impl SmpBus) {
        let addr = self.fetch_u16(bus);
        let _ = bus.read(addr);
        bus.write(addr, self.y);
    }

    // ----- Stack ops (4 cycles each) ----------------------------------
    //
    // PUSH r:  opcode + idle + write + idle  = 4
    // POP  r:  opcode + idle + idle  + read  = 4

    fn op_push_a(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        self.push8(bus, self.a);
        bus.idle();
    }

    fn op_push_x(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        self.push8(bus, self.x);
        bus.idle();
    }

    fn op_push_y(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        self.push8(bus, self.y);
        bus.idle();
    }

    fn op_push_psw(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        let p = self.psw.pack();
        self.push8(bus, p);
        bus.idle();
    }

    fn op_pop_a(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        bus.idle();
        self.a = self.pop8(bus);
        // POP A does NOT touch flags. (POP X / POP Y same; only
        // POP PSW changes flags by virtue of unpacking the byte.)
    }

    fn op_pop_x(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        bus.idle();
        self.x = self.pop8(bus);
    }

    fn op_pop_y(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        bus.idle();
        self.y = self.pop8(bus);
    }

    fn op_pop_psw(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        bus.idle();
        let byte = self.pop8(bus);
        self.psw.unpack(byte);
    }

    // ----- Branches ---------------------------------------------------
    //
    // Spec: 2 cycles when not taken, 4 cycles when taken. Opcode +
    // operand fetch already pay 2; [`Smp::branch_taken`] adds the
    // extra 2 idle cycles.

    fn op_bra(&mut self, bus: &mut impl SmpBus) {
        let offset = self.fetch_u8(bus) as i8;
        self.branch_taken(bus, offset);
    }

    fn op_branch_if(&mut self, bus: &mut impl SmpBus, condition: bool) {
        let offset = self.fetch_u8(bus) as i8;
        if condition {
            self.branch_taken(bus, offset);
        }
    }

    // ----- JMP / RET --------------------------------------------------

    fn op_jmp_abs(&mut self, bus: &mut impl SmpBus) {
        // 3 cycles: opcode + lo + hi. PC just snaps.
        let target = self.fetch_u16(bus);
        self.pc = target;
    }

    fn op_ret(&mut self, bus: &mut impl SmpBus) {
        // 5 cycles: opcode + idle + idle + pop_lo + pop_hi.
        bus.idle();
        bus.idle();
        self.pc = self.pop16(bus);
    }

    // ----- ADC family --------------------------------------------------
    //
    // All 12 addressing modes share the same byte-level operation
    // (`adc_byte`), differing only in how the operand bytes are
    // located and (for the three memory-destination variants) where
    // the result is written. Cycle counts follow Mesen2 / Anomie's
    // SPC700 timing tables: imm=2, dp=3, abs=4, dp+X=4, abs+X=5,
    // abs+Y=5, (X)=3, [dp+X]=6, [dp]+Y=6, (X),(Y)=5, dp,#imm=5,
    // dp,dp=6.

    fn op_adc_a_imm(&mut self, bus: &mut impl SmpBus) {
        let v = self.fetch_u8(bus);
        self.a = self.adc_byte(self.a, v);
    }

    fn op_adc_a_dp(&mut self, bus: &mut impl SmpBus) {
        let dp = self.fetch_u8(bus);
        let v = self.read_dp(bus, dp);
        self.a = self.adc_byte(self.a, v);
    }

    fn op_adc_a_abs(&mut self, bus: &mut impl SmpBus) {
        let addr = self.fetch_u16(bus);
        let v = bus.read(addr);
        self.a = self.adc_byte(self.a, v);
    }

    fn op_adc_a_dp_x_direct(&mut self, bus: &mut impl SmpBus) {
        // ADC A,(X): direct page indexed by X register. 3 cycles =
        // opcode + idle + read.
        bus.idle();
        let v = bus.read(self.addr_dp_x_direct());
        self.a = self.adc_byte(self.a, v);
    }

    fn op_adc_a_dp_plus_x(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_dp_plus_x(bus);
        let v = bus.read(addr);
        self.a = self.adc_byte(self.a, v);
    }

    fn op_adc_a_abs_plus_x(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_abs_plus_x(bus);
        let v = bus.read(addr);
        self.a = self.adc_byte(self.a, v);
    }

    fn op_adc_a_abs_plus_y(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_abs_plus_y(bus);
        let v = bus.read(addr);
        self.a = self.adc_byte(self.a, v);
    }

    fn op_adc_a_dp_x_indirect(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_dp_x_indirect(bus);
        let v = bus.read(addr);
        self.a = self.adc_byte(self.a, v);
    }

    fn op_adc_a_dp_indirect_plus_y(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_dp_indirect_plus_y(bus);
        let v = bus.read(addr);
        self.a = self.adc_byte(self.a, v);
    }

    fn op_adc_x_indirect_y_indirect(&mut self, bus: &mut impl SmpBus) {
        // ADC (X),(Y): 5 cycles. Opcode + idle + read_y + read_x +
        // write_x. Memory destination - A is unchanged.
        bus.idle();
        let y_val = bus.read(self.addr_dp_y_direct());
        let x_addr = self.addr_dp_x_direct();
        let x_val = bus.read(x_addr);
        let result = self.adc_byte(x_val, y_val);
        bus.write(x_addr, result);
    }

    fn op_adc_dp_dp(&mut self, bus: &mut impl SmpBus) {
        // ADC dp,dp: 6 cycles. Opcode + src_dp + read_src + dst_dp +
        // read_dst + write_dst. Operand byte order in stream is
        // `src dst` (Mesen2 + fullsnes - the source byte is at the
        // LOWER address in the instruction stream).
        let src_dp = self.fetch_u8(bus);
        let src_val = self.read_dp(bus, src_dp);
        let dst_dp = self.fetch_u8(bus);
        let dst_val = self.read_dp(bus, dst_dp);
        let result = self.adc_byte(dst_val, src_val);
        self.write_dp(bus, dst_dp, result);
    }

    fn op_adc_dp_imm(&mut self, bus: &mut impl SmpBus) {
        // ADC dp,#imm: 5 cycles. Opcode + imm + dp_fetch + read_dp +
        // write_dp. Operand byte order in stream is `imm dp`.
        let imm = self.fetch_u8(bus);
        let dp = self.fetch_u8(bus);
        let dst_val = self.read_dp(bus, dp);
        let result = self.adc_byte(dst_val, imm);
        self.write_dp(bus, dp, result);
    }

    // ----- SBC family --------------------------------------------------
    //
    // SPC700 SBC computes `a + (~b) + C`, a standard subtract-with-
    // borrow where C=1 means "no borrow." Implementing it as
    // `adc_byte(a, !b)` propagates every flag correctly:
    //   - C: set when a + ~b + C overflows (i.e. no borrow occurred)
    //   - V: set when a and ~b share a sign but result differs (which
    //        is equivalent to a and b having different signs and the
    //        result having the wrong sign from a)
    //   - H: set when no half-borrow from bit 4 (low-nibble add of
    //        a + ~b + C overflows past bit 3)
    //   - N/Z: from the 8-bit result
    // Cycle counts and addressing modes mirror ADC exactly.

    fn sbc_byte(&mut self, a: u8, b: u8) -> u8 {
        self.adc_byte(a, !b)
    }

    fn op_sbc_a_imm(&mut self, bus: &mut impl SmpBus) {
        let v = self.fetch_u8(bus);
        self.a = self.sbc_byte(self.a, v);
    }

    fn op_sbc_a_dp(&mut self, bus: &mut impl SmpBus) {
        let dp = self.fetch_u8(bus);
        let v = self.read_dp(bus, dp);
        self.a = self.sbc_byte(self.a, v);
    }

    fn op_sbc_a_abs(&mut self, bus: &mut impl SmpBus) {
        let addr = self.fetch_u16(bus);
        let v = bus.read(addr);
        self.a = self.sbc_byte(self.a, v);
    }

    fn op_sbc_a_dp_x_direct(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        let v = bus.read(self.addr_dp_x_direct());
        self.a = self.sbc_byte(self.a, v);
    }

    fn op_sbc_a_dp_plus_x(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_dp_plus_x(bus);
        let v = bus.read(addr);
        self.a = self.sbc_byte(self.a, v);
    }

    fn op_sbc_a_abs_plus_x(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_abs_plus_x(bus);
        let v = bus.read(addr);
        self.a = self.sbc_byte(self.a, v);
    }

    fn op_sbc_a_abs_plus_y(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_abs_plus_y(bus);
        let v = bus.read(addr);
        self.a = self.sbc_byte(self.a, v);
    }

    fn op_sbc_a_dp_x_indirect(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_dp_x_indirect(bus);
        let v = bus.read(addr);
        self.a = self.sbc_byte(self.a, v);
    }

    fn op_sbc_a_dp_indirect_plus_y(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_dp_indirect_plus_y(bus);
        let v = bus.read(addr);
        self.a = self.sbc_byte(self.a, v);
    }

    fn op_sbc_x_indirect_y_indirect(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        let y_val = bus.read(self.addr_dp_y_direct());
        let x_addr = self.addr_dp_x_direct();
        let x_val = bus.read(x_addr);
        let result = self.sbc_byte(x_val, y_val);
        bus.write(x_addr, result);
    }

    fn op_sbc_dp_dp(&mut self, bus: &mut impl SmpBus) {
        let src_dp = self.fetch_u8(bus);
        let src_val = self.read_dp(bus, src_dp);
        let dst_dp = self.fetch_u8(bus);
        let dst_val = self.read_dp(bus, dst_dp);
        let result = self.sbc_byte(dst_val, src_val);
        self.write_dp(bus, dst_dp, result);
    }

    fn op_sbc_dp_imm(&mut self, bus: &mut impl SmpBus) {
        let imm = self.fetch_u8(bus);
        let dp = self.fetch_u8(bus);
        let dst_val = self.read_dp(bus, dp);
        let result = self.sbc_byte(dst_val, imm);
        self.write_dp(bus, dp, result);
    }
}

impl Default for Smp {
    fn default() -> Self {
        Self::new()
    }
}
