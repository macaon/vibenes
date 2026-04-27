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
pub mod state;

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

            // --- INC / DEC of memory (RMW) ---
            0xAB => self.op_inc_dp(bus),
            0xBB => self.op_inc_dp_plus_x(bus),
            0xAC => self.op_inc_abs(bus),
            0x8B => self.op_dec_dp(bus),
            0x9B => self.op_dec_dp_plus_x(bus),
            0x8C => self.op_dec_abs(bus),

            // --- MUL / DIV ---
            0xCF => self.op_mul_ya(bus),
            0x9E => self.op_div_ya_x(bus),

            // --- SET1 / CLR1 dp.bit (16 opcodes) ---
            0x02 => self.op_set1_dp(bus, 0),
            0x22 => self.op_set1_dp(bus, 1),
            0x42 => self.op_set1_dp(bus, 2),
            0x62 => self.op_set1_dp(bus, 3),
            0x82 => self.op_set1_dp(bus, 4),
            0xA2 => self.op_set1_dp(bus, 5),
            0xC2 => self.op_set1_dp(bus, 6),
            0xE2 => self.op_set1_dp(bus, 7),
            0x12 => self.op_clr1_dp(bus, 0),
            0x32 => self.op_clr1_dp(bus, 1),
            0x52 => self.op_clr1_dp(bus, 2),
            0x72 => self.op_clr1_dp(bus, 3),
            0x92 => self.op_clr1_dp(bus, 4),
            0xB2 => self.op_clr1_dp(bus, 5),
            0xD2 => self.op_clr1_dp(bus, 6),
            0xF2 => self.op_clr1_dp(bus, 7),

            // --- TSET1 / TCLR1 !abs ---
            0x0E => self.op_tset1_abs(bus),
            0x4E => self.op_tclr1_abs(bus),

            // --- BBS / BBC dp.bit, rel (16 ops) ---
            0x03 => self.op_bbs_dp(bus, 0),
            0x23 => self.op_bbs_dp(bus, 1),
            0x43 => self.op_bbs_dp(bus, 2),
            0x63 => self.op_bbs_dp(bus, 3),
            0x83 => self.op_bbs_dp(bus, 4),
            0xA3 => self.op_bbs_dp(bus, 5),
            0xC3 => self.op_bbs_dp(bus, 6),
            0xE3 => self.op_bbs_dp(bus, 7),
            0x13 => self.op_bbc_dp(bus, 0),
            0x33 => self.op_bbc_dp(bus, 1),
            0x53 => self.op_bbc_dp(bus, 2),
            0x73 => self.op_bbc_dp(bus, 3),
            0x93 => self.op_bbc_dp(bus, 4),
            0xB3 => self.op_bbc_dp(bus, 5),
            0xD3 => self.op_bbc_dp(bus, 6),
            0xF3 => self.op_bbc_dp(bus, 7),

            // --- CBNE / DBNZ ---
            0x2E => self.op_cbne_dp(bus),
            0xDE => self.op_cbne_dp_plus_x(bus),
            0x6E => self.op_dbnz_dp(bus),
            0xFE => self.op_dbnz_y(bus),

            // --- Carry/memory bit ops (13/3 split address) ---
            0x4A => self.op_and1_c_bit(bus),
            0x6A => self.op_and1_c_notbit(bus),
            0x0A => self.op_or1_c_bit(bus),
            0x2A => self.op_or1_c_notbit(bus),
            0x8A => self.op_eor1_c_bit(bus),
            0xEA => self.op_not1_bit(bus),
            0xAA => self.op_mov1_c_bit(bus),
            0xCA => self.op_mov1_bit_c(bus),

            // --- 16-bit YA word ops ---
            0xBA => self.op_movw_ya_dp(bus),
            0xDA => self.op_movw_dp_ya(bus),
            0x7A => self.op_addw_ya_dp(bus),
            0x9A => self.op_subw_ya_dp(bus),
            0x5A => self.op_cmpw_ya_dp(bus),
            0x3A => self.op_incw_dp(bus),
            0x1A => self.op_decw_dp(bus),

            // --- Shifts and rotates (RMW for memory forms) ---
            0x1C => self.op_asl_a(bus),
            0x0B => self.op_asl_dp(bus),
            0x1B => self.op_asl_dp_plus_x(bus),
            0x0C => self.op_asl_abs(bus),
            0x5C => self.op_lsr_a(bus),
            0x4B => self.op_lsr_dp(bus),
            0x5B => self.op_lsr_dp_plus_x(bus),
            0x4C => self.op_lsr_abs(bus),
            0x3C => self.op_rol_a(bus),
            0x2B => self.op_rol_dp(bus),
            0x3B => self.op_rol_dp_plus_x(bus),
            0x2C => self.op_rol_abs(bus),
            0x7C => self.op_ror_a(bus),
            0x6B => self.op_ror_dp(bus),
            0x7B => self.op_ror_dp_plus_x(bus),
            0x6C => self.op_ror_abs(bus),

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
            0x7F => self.op_reti(bus),
            0x3F => self.op_call_abs(bus),
            0x4F => self.op_pcall(bus),
            0xDF => self.op_daa(bus),
            0xBE => self.op_das(bus),
            0x9F => self.op_xcn(bus),

            // --- TCALL n (16 ops) ---
            0x01 => self.op_tcall(bus, 0),
            0x11 => self.op_tcall(bus, 1),
            0x21 => self.op_tcall(bus, 2),
            0x31 => self.op_tcall(bus, 3),
            0x41 => self.op_tcall(bus, 4),
            0x51 => self.op_tcall(bus, 5),
            0x61 => self.op_tcall(bus, 6),
            0x71 => self.op_tcall(bus, 7),
            0x81 => self.op_tcall(bus, 8),
            0x91 => self.op_tcall(bus, 9),
            0xA1 => self.op_tcall(bus, 10),
            0xB1 => self.op_tcall(bus, 11),
            0xC1 => self.op_tcall(bus, 12),
            0xD1 => self.op_tcall(bus, 13),
            0xE1 => self.op_tcall(bus, 14),
            0xF1 => self.op_tcall(bus, 15),

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

            // --- CMP A family (12 modes) ---
            0x68 => self.op_cmp_a_imm(bus),
            0x64 => self.op_cmp_a_dp(bus),
            0x65 => self.op_cmp_a_abs(bus),
            0x66 => self.op_cmp_a_dp_x_direct(bus),
            0x74 => self.op_cmp_a_dp_plus_x(bus),
            0x75 => self.op_cmp_a_abs_plus_x(bus),
            0x76 => self.op_cmp_a_abs_plus_y(bus),
            0x67 => self.op_cmp_a_dp_x_indirect(bus),
            0x77 => self.op_cmp_a_dp_indirect_plus_y(bus),
            0x79 => self.op_cmp_x_indirect_y_indirect(bus),
            0x69 => self.op_cmp_dp_dp(bus),
            0x78 => self.op_cmp_dp_imm(bus),

            // --- CMP X (3 modes) ---
            0xC8 => self.op_cmp_x_imm(bus),
            0x3E => self.op_cmp_x_dp(bus),
            0x1E => self.op_cmp_x_abs(bus),

            // --- CMP Y (3 modes) ---
            0xAD => self.op_cmp_y_imm(bus),
            0x7E => self.op_cmp_y_dp(bus),
            0x5E => self.op_cmp_y_abs(bus),

            // --- OR family (12 modes, base $00) ---
            0x08 => self.op_or_a_imm(bus),
            0x04 => self.op_or_a_dp(bus),
            0x05 => self.op_or_a_abs(bus),
            0x06 => self.op_or_a_dp_x_direct(bus),
            0x14 => self.op_or_a_dp_plus_x(bus),
            0x15 => self.op_or_a_abs_plus_x(bus),
            0x16 => self.op_or_a_abs_plus_y(bus),
            0x07 => self.op_or_a_dp_x_indirect(bus),
            0x17 => self.op_or_a_dp_indirect_plus_y(bus),
            0x19 => self.op_or_x_indirect_y_indirect(bus),
            0x09 => self.op_or_dp_dp(bus),
            0x18 => self.op_or_dp_imm(bus),

            // --- AND family (12 modes, base $20) ---
            0x28 => self.op_and_a_imm(bus),
            0x24 => self.op_and_a_dp(bus),
            0x25 => self.op_and_a_abs(bus),
            0x26 => self.op_and_a_dp_x_direct(bus),
            0x34 => self.op_and_a_dp_plus_x(bus),
            0x35 => self.op_and_a_abs_plus_x(bus),
            0x36 => self.op_and_a_abs_plus_y(bus),
            0x27 => self.op_and_a_dp_x_indirect(bus),
            0x37 => self.op_and_a_dp_indirect_plus_y(bus),
            0x39 => self.op_and_x_indirect_y_indirect(bus),
            0x29 => self.op_and_dp_dp(bus),
            0x38 => self.op_and_dp_imm(bus),

            // --- EOR family (12 modes, base $40) ---
            0x48 => self.op_eor_a_imm(bus),
            0x44 => self.op_eor_a_dp(bus),
            0x45 => self.op_eor_a_abs(bus),
            0x46 => self.op_eor_a_dp_x_direct(bus),
            0x54 => self.op_eor_a_dp_plus_x(bus),
            0x55 => self.op_eor_a_abs_plus_x(bus),
            0x56 => self.op_eor_a_abs_plus_y(bus),
            0x47 => self.op_eor_a_dp_x_indirect(bus),
            0x57 => self.op_eor_a_dp_indirect_plus_y(bus),
            0x59 => self.op_eor_x_indirect_y_indirect(bus),
            0x49 => self.op_eor_dp_dp(bus),
            0x58 => self.op_eor_dp_imm(bus),

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

    // ----- INC / DEC of memory (RMW) ----------------------------------
    //
    // Read-modify-write: read operand, compute new value, write it
    // back. Flags: N/Z from the result; C/V/H untouched. Cycle counts:
    //   INC/DEC dp     - 4 cycles (opcode + dp_fetch + read + write)
    //   INC/DEC dp+X   - 5 cycles (adds idle for X-add)
    //   INC/DEC !abs   - 5 cycles (opcode + lo + hi + read + write)

    fn op_inc_dp(&mut self, bus: &mut impl SmpBus) {
        let dp = self.fetch_u8(bus);
        let v = self.read_dp(bus, dp).wrapping_add(1);
        self.set_nz(v);
        self.write_dp(bus, dp, v);
    }

    fn op_dec_dp(&mut self, bus: &mut impl SmpBus) {
        let dp = self.fetch_u8(bus);
        let v = self.read_dp(bus, dp).wrapping_sub(1);
        self.set_nz(v);
        self.write_dp(bus, dp, v);
    }

    fn op_inc_dp_plus_x(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_dp_plus_x(bus);
        let v = bus.read(addr).wrapping_add(1);
        self.set_nz(v);
        bus.write(addr, v);
    }

    fn op_dec_dp_plus_x(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_dp_plus_x(bus);
        let v = bus.read(addr).wrapping_sub(1);
        self.set_nz(v);
        bus.write(addr, v);
    }

    fn op_inc_abs(&mut self, bus: &mut impl SmpBus) {
        let addr = self.fetch_u16(bus);
        let v = bus.read(addr).wrapping_add(1);
        self.set_nz(v);
        bus.write(addr, v);
    }

    fn op_dec_abs(&mut self, bus: &mut impl SmpBus) {
        let addr = self.fetch_u16(bus);
        let v = bus.read(addr).wrapping_sub(1);
        self.set_nz(v);
        bus.write(addr, v);
    }

    // ----- Shifts and rotates -----------------------------------------
    //
    // ASL: C <- bit 7, result = v << 1.            N/Z from result.
    // LSR: C <- bit 0, result = v >> 1.            N=0, Z from result.
    // ROL: C_new <- bit 7, result = (v<<1)|C_old.  N/Z from result.
    // ROR: C_new <- bit 0, result = (v>>1)|C_old<<7. N/Z from result.
    // V and H are preserved across all four. Cycle counts:
    //   *A    - 2 (opcode + idle)
    //   *dp   - 4 (opcode + dp_fetch + read + write)
    //   *dp+X - 5 (RMW + idle for X-add)
    //   *!abs - 5 (opcode + lo + hi + read + write)

    fn asl_byte(&mut self, v: u8) -> u8 {
        self.psw.c = (v & 0x80) != 0;
        let r = v << 1;
        self.set_nz(r);
        r
    }
    fn lsr_byte(&mut self, v: u8) -> u8 {
        self.psw.c = (v & 0x01) != 0;
        let r = v >> 1;
        self.set_nz(r);
        r
    }
    fn rol_byte(&mut self, v: u8) -> u8 {
        let new_c = (v & 0x80) != 0;
        let old_c = self.psw.c as u8;
        let r = (v << 1) | old_c;
        self.psw.c = new_c;
        self.set_nz(r);
        r
    }
    fn ror_byte(&mut self, v: u8) -> u8 {
        let new_c = (v & 0x01) != 0;
        let old_c = if self.psw.c { 0x80 } else { 0 };
        let r = (v >> 1) | old_c;
        self.psw.c = new_c;
        self.set_nz(r);
        r
    }

    // ASL
    fn op_asl_a(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        self.a = self.asl_byte(self.a);
    }
    fn op_asl_dp(&mut self, bus: &mut impl SmpBus) {
        let dp = self.fetch_u8(bus);
        let v = self.read_dp(bus, dp);
        let r = self.asl_byte(v);
        self.write_dp(bus, dp, r);
    }
    fn op_asl_dp_plus_x(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_dp_plus_x(bus);
        let v = bus.read(addr);
        let r = self.asl_byte(v);
        bus.write(addr, r);
    }
    fn op_asl_abs(&mut self, bus: &mut impl SmpBus) {
        let addr = self.fetch_u16(bus);
        let v = bus.read(addr);
        let r = self.asl_byte(v);
        bus.write(addr, r);
    }

    // LSR
    fn op_lsr_a(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        self.a = self.lsr_byte(self.a);
    }
    fn op_lsr_dp(&mut self, bus: &mut impl SmpBus) {
        let dp = self.fetch_u8(bus);
        let v = self.read_dp(bus, dp);
        let r = self.lsr_byte(v);
        self.write_dp(bus, dp, r);
    }
    fn op_lsr_dp_plus_x(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_dp_plus_x(bus);
        let v = bus.read(addr);
        let r = self.lsr_byte(v);
        bus.write(addr, r);
    }
    fn op_lsr_abs(&mut self, bus: &mut impl SmpBus) {
        let addr = self.fetch_u16(bus);
        let v = bus.read(addr);
        let r = self.lsr_byte(v);
        bus.write(addr, r);
    }

    // ROL
    fn op_rol_a(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        self.a = self.rol_byte(self.a);
    }
    fn op_rol_dp(&mut self, bus: &mut impl SmpBus) {
        let dp = self.fetch_u8(bus);
        let v = self.read_dp(bus, dp);
        let r = self.rol_byte(v);
        self.write_dp(bus, dp, r);
    }
    fn op_rol_dp_plus_x(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_dp_plus_x(bus);
        let v = bus.read(addr);
        let r = self.rol_byte(v);
        bus.write(addr, r);
    }
    fn op_rol_abs(&mut self, bus: &mut impl SmpBus) {
        let addr = self.fetch_u16(bus);
        let v = bus.read(addr);
        let r = self.rol_byte(v);
        bus.write(addr, r);
    }

    // ROR
    fn op_ror_a(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        self.a = self.ror_byte(self.a);
    }
    fn op_ror_dp(&mut self, bus: &mut impl SmpBus) {
        let dp = self.fetch_u8(bus);
        let v = self.read_dp(bus, dp);
        let r = self.ror_byte(v);
        self.write_dp(bus, dp, r);
    }
    fn op_ror_dp_plus_x(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_dp_plus_x(bus);
        let v = bus.read(addr);
        let r = self.ror_byte(v);
        bus.write(addr, r);
    }
    fn op_ror_abs(&mut self, bus: &mut impl SmpBus) {
        let addr = self.fetch_u16(bus);
        let v = bus.read(addr);
        let r = self.ror_byte(v);
        bus.write(addr, r);
    }

    // ----- 16-bit YA word ops -----------------------------------------
    //
    // YA forms a 16-bit register pair (Y = high, A = low). Word
    // operands are loaded little-endian from the direct page; dp+1
    // wraps within the page (offset is 8-bit). Quirks:
    //   - MOVW dp, YA does NOT update flags (Anomie: it is the only
    //     "load-style" SPC700 op that leaves PSW alone).
    //   - ADDW / SUBW do NOT use the carry flag as input - they are
    //     fresh adds / subtracts. C is set from the new carry.
    //   - CMPW only touches N/Z/C (no V/H), mirroring 8-bit CMP.
    //   - ADDW H is set on carry from bit 11; SUBW H is set on
    //     no-borrow from bit 12 (i.e. low 12 bits LHS >= RHS).
    // Cycle counts (from bsnes / Anomie):
    //   MOVW YA, dp - 5 (op + dp + read_lo + idle + read_hi)
    //   MOVW dp, YA - 5 (op + dp + dummy_read + write_lo + write_hi)
    //   ADDW / SUBW - 5 (op + dp + read_lo + idle + read_hi)
    //   CMPW        - 4 (op + dp + read_lo + read_hi - no idle)
    //   INCW / DECW - 6 (op + dp + read_lo + write_lo + read_hi + write_hi)

    fn ya_word(&self) -> u16 {
        ((self.y as u16) << 8) | (self.a as u16)
    }
    fn set_ya_from_word(&mut self, w: u16) {
        self.a = w as u8;
        self.y = (w >> 8) as u8;
    }
    fn set_nz16(&mut self, w: u16) {
        self.psw.n = (w & 0x8000) != 0;
        self.psw.z = w == 0;
    }

    fn op_movw_ya_dp(&mut self, bus: &mut impl SmpBus) {
        let dp = self.fetch_u8(bus);
        self.a = self.read_dp(bus, dp);
        bus.idle();
        self.y = self.read_dp(bus, dp.wrapping_add(1));
        let ya = self.ya_word();
        self.set_nz16(ya);
    }

    fn op_movw_dp_ya(&mut self, bus: &mut impl SmpBus) {
        let dp = self.fetch_u8(bus);
        let _ = self.read_dp(bus, dp); // dummy read
        self.write_dp(bus, dp, self.a);
        self.write_dp(bus, dp.wrapping_add(1), self.y);
        // SPC700 quirk: MOVW dp, YA does not update flags.
    }

    fn op_addw_ya_dp(&mut self, bus: &mut impl SmpBus) {
        let dp = self.fetch_u8(bus);
        let lo = self.read_dp(bus, dp) as u16;
        bus.idle();
        let hi = self.read_dp(bus, dp.wrapping_add(1)) as u16;
        let word = (hi << 8) | lo;
        let ya = self.ya_word();
        self.psw.h = ((ya & 0x0FFF) + (word & 0x0FFF)) > 0x0FFF;
        let wide = (ya as u32) + (word as u32);
        let result = wide as u16;
        self.psw.v = ((!(ya ^ word)) & (ya ^ result) & 0x8000) != 0;
        self.psw.c = wide > 0xFFFF;
        self.set_ya_from_word(result);
        self.set_nz16(result);
    }

    fn op_subw_ya_dp(&mut self, bus: &mut impl SmpBus) {
        let dp = self.fetch_u8(bus);
        let lo = self.read_dp(bus, dp) as u16;
        bus.idle();
        let hi = self.read_dp(bus, dp.wrapping_add(1)) as u16;
        let word = (hi << 8) | lo;
        let ya = self.ya_word();
        let signed = (ya as i32) - (word as i32);
        let result = signed as u16;
        self.psw.h = ((ya & 0x0FFF) as i32 - (word & 0x0FFF) as i32) >= 0;
        self.psw.v = ((ya ^ word) & (ya ^ result) & 0x8000) != 0;
        self.psw.c = signed >= 0;
        self.set_ya_from_word(result);
        self.set_nz16(result);
    }

    fn op_cmpw_ya_dp(&mut self, bus: &mut impl SmpBus) {
        let dp = self.fetch_u8(bus);
        let lo = self.read_dp(bus, dp) as u16;
        let hi = self.read_dp(bus, dp.wrapping_add(1)) as u16;
        let word = (hi << 8) | lo;
        let ya = self.ya_word();
        let result = ya.wrapping_sub(word);
        self.psw.c = ya >= word;
        self.set_nz16(result);
    }

    fn op_incw_dp(&mut self, bus: &mut impl SmpBus) {
        let dp = self.fetch_u8(bus);
        let mut result = self.read_dp(bus, dp) as u16;
        result = result.wrapping_add(1);
        self.write_dp(bus, dp, result as u8);
        let hi = self.read_dp(bus, dp.wrapping_add(1)) as u16;
        result = result.wrapping_add(hi << 8);
        self.set_nz16(result);
        self.write_dp(bus, dp.wrapping_add(1), (result >> 8) as u8);
    }

    fn op_decw_dp(&mut self, bus: &mut impl SmpBus) {
        let dp = self.fetch_u8(bus);
        let mut result = self.read_dp(bus, dp) as u16;
        result = result.wrapping_sub(1);
        self.write_dp(bus, dp, result as u8);
        let hi = self.read_dp(bus, dp.wrapping_add(1)) as u16;
        result = result.wrapping_add(hi << 8);
        self.set_nz16(result);
        self.write_dp(bus, dp.wrapping_add(1), (result >> 8) as u8);
    }

    // ----- MUL YA / DIV YA, X -----------------------------------------
    //
    // MUL YA ($CF, 9 cycles): YA = Y * A unsigned. Sets N/Z from the
    // high byte (Y) only - quirk: a result like 0x0001 sets Z=1
    // because Y is zero (per Mesen2 `Mul` and Anomie's notes).
    //
    // DIV YA, X ($9E, 12 cycles): A = YA / X (quotient), Y = YA % X
    // (remainder). Hardware uses an iterative 9-step shift-subtract;
    // we port Mesen2's loop verbatim. V is set when Y >= X (result
    // would not fit in 8 bits), H when (Y & 0xF) >= (X & 0xF). N/Z
    // come from the resulting A. The /512 quirk: when V is set the
    // returned quotient/remainder come out of the loop's collision
    // case rather than a true division - this is the SPC700's exact
    // observable behaviour and games depend on it.

    fn op_mul_ya(&mut self, bus: &mut impl SmpBus) {
        for _ in 0..8 {
            bus.idle();
        }
        let result = (self.y as u16) * (self.a as u16);
        self.a = result as u8;
        self.y = (result >> 8) as u8;
        self.psw.n = (self.y & 0x80) != 0;
        self.psw.z = self.y == 0;
    }

    // ----- Bit operations ---------------------------------------------
    //
    // SET1 / CLR1 dp.bit are 4-cycle RMW ops on a single bit of a
    // direct-page byte; no flag updates. Bit number is encoded in
    // bits 5-7 of the opcode (we pass it explicitly for clarity).
    //
    // TSET1 / TCLR1 !abs (6 cycles) compare A against the memory
    // value (set N/Z from `A - mem`, like CMP but without C/V/H),
    // then OR / AND-NOT A into the memory location. Includes a
    // dummy read between read and write.
    //
    // The remaining bit ops use a 13/3-split absolute address: the
    // 16-bit operand encodes addr in bits 0-12 and bit# in bits
    // 13-15. They operate on PSW.C with one bit of memory.

    fn fetch_bit_addr(&mut self, bus: &mut impl SmpBus) -> (u16, u8) {
        let lo = self.fetch_u8(bus) as u16;
        let hi = self.fetch_u8(bus) as u16;
        let combined = (hi << 8) | lo;
        (combined & 0x1FFF, ((combined >> 13) & 0x07) as u8)
    }

    fn op_set1_dp(&mut self, bus: &mut impl SmpBus, bit: u8) {
        let dp = self.fetch_u8(bus);
        let v = self.read_dp(bus, dp);
        self.write_dp(bus, dp, v | (1 << bit));
    }

    fn op_clr1_dp(&mut self, bus: &mut impl SmpBus, bit: u8) {
        let dp = self.fetch_u8(bus);
        let v = self.read_dp(bus, dp);
        self.write_dp(bus, dp, v & !(1 << bit));
    }

    fn op_tset1_abs(&mut self, bus: &mut impl SmpBus) {
        let addr = self.fetch_u16(bus);
        let data = bus.read(addr);
        let cmp = self.a.wrapping_sub(data);
        self.set_nz(cmp);
        let _ = bus.read(addr); // dummy read
        bus.write(addr, data | self.a);
    }

    fn op_tclr1_abs(&mut self, bus: &mut impl SmpBus) {
        let addr = self.fetch_u16(bus);
        let data = bus.read(addr);
        let cmp = self.a.wrapping_sub(data);
        self.set_nz(cmp);
        let _ = bus.read(addr); // dummy read
        bus.write(addr, data & !self.a);
    }

    fn op_and1_c_bit(&mut self, bus: &mut impl SmpBus) {
        let (addr, bit) = self.fetch_bit_addr(bus);
        let data = bus.read(addr);
        self.psw.c = self.psw.c && ((data >> bit) & 1) != 0;
    }

    fn op_and1_c_notbit(&mut self, bus: &mut impl SmpBus) {
        let (addr, bit) = self.fetch_bit_addr(bus);
        let data = bus.read(addr);
        self.psw.c = self.psw.c && ((data >> bit) & 1) == 0;
    }

    fn op_or1_c_bit(&mut self, bus: &mut impl SmpBus) {
        let (addr, bit) = self.fetch_bit_addr(bus);
        let data = bus.read(addr);
        bus.idle();
        self.psw.c = self.psw.c || ((data >> bit) & 1) != 0;
    }

    fn op_or1_c_notbit(&mut self, bus: &mut impl SmpBus) {
        let (addr, bit) = self.fetch_bit_addr(bus);
        let data = bus.read(addr);
        bus.idle();
        self.psw.c = self.psw.c || ((data >> bit) & 1) == 0;
    }

    fn op_eor1_c_bit(&mut self, bus: &mut impl SmpBus) {
        let (addr, bit) = self.fetch_bit_addr(bus);
        let data = bus.read(addr);
        bus.idle();
        let mem_bit = ((data >> bit) & 1) != 0;
        self.psw.c ^= mem_bit;
    }

    fn op_not1_bit(&mut self, bus: &mut impl SmpBus) {
        let (addr, bit) = self.fetch_bit_addr(bus);
        let data = bus.read(addr);
        bus.write(addr, data ^ (1 << bit));
    }

    fn op_mov1_c_bit(&mut self, bus: &mut impl SmpBus) {
        let (addr, bit) = self.fetch_bit_addr(bus);
        let data = bus.read(addr);
        self.psw.c = ((data >> bit) & 1) != 0;
    }

    fn op_mov1_bit_c(&mut self, bus: &mut impl SmpBus) {
        let (addr, bit) = self.fetch_bit_addr(bus);
        let data = bus.read(addr);
        bus.idle();
        let mask = 1 << bit;
        let new = (data & !mask) | (if self.psw.c { mask } else { 0 });
        bus.write(addr, new);
    }

    // ----- BBS / BBC / CBNE / DBNZ (branch-on-condition) --------------
    //
    // BBS dp.bit, rel: branch if mem[dp].bit is set.   5 / 7 cycles.
    // BBC dp.bit, rel: branch if mem[dp].bit is clear. 5 / 7 cycles.
    // CBNE dp, rel:    branch if A != mem[dp].         5 / 7 cycles.
    // CBNE dp+X, rel:  same with X-indexed dp.         6 / 8 cycles.
    // DBNZ dp, rel:    decrement mem[dp]; branch != 0. 5 / 7 cycles (RMW).
    // DBNZ Y, rel:     decrement Y; branch if != 0.    4 / 6 cycles.
    // None of these touch any flag (Anomie / bsnes); they only read or
    // RMW memory and possibly take the branch.

    fn op_bbs_dp(&mut self, bus: &mut impl SmpBus, bit: u8) {
        let dp = self.fetch_u8(bus);
        let v = self.read_dp(bus, dp);
        bus.idle();
        let offset = self.fetch_u8(bus) as i8;
        if (v & (1 << bit)) != 0 {
            self.branch_taken(bus, offset);
        }
    }

    fn op_bbc_dp(&mut self, bus: &mut impl SmpBus, bit: u8) {
        let dp = self.fetch_u8(bus);
        let v = self.read_dp(bus, dp);
        bus.idle();
        let offset = self.fetch_u8(bus) as i8;
        if (v & (1 << bit)) == 0 {
            self.branch_taken(bus, offset);
        }
    }

    fn op_cbne_dp(&mut self, bus: &mut impl SmpBus) {
        let dp = self.fetch_u8(bus);
        let v = self.read_dp(bus, dp);
        bus.idle();
        let offset = self.fetch_u8(bus) as i8;
        if self.a != v {
            self.branch_taken(bus, offset);
        }
    }

    fn op_cbne_dp_plus_x(&mut self, bus: &mut impl SmpBus) {
        let dp = self.fetch_u8(bus);
        bus.idle();
        let v = self.read_dp(bus, dp.wrapping_add(self.x));
        bus.idle();
        let offset = self.fetch_u8(bus) as i8;
        if self.a != v {
            self.branch_taken(bus, offset);
        }
    }

    fn op_dbnz_dp(&mut self, bus: &mut impl SmpBus) {
        let dp = self.fetch_u8(bus);
        let v = self.read_dp(bus, dp).wrapping_sub(1);
        self.write_dp(bus, dp, v);
        let offset = self.fetch_u8(bus) as i8;
        if v != 0 {
            self.branch_taken(bus, offset);
        }
    }

    fn op_dbnz_y(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        bus.idle();
        let offset = self.fetch_u8(bus) as i8;
        self.y = self.y.wrapping_sub(1);
        if self.y != 0 {
            self.branch_taken(bus, offset);
        }
    }

    fn op_div_ya_x(&mut self, bus: &mut impl SmpBus) {
        for _ in 0..11 {
            bus.idle();
        }
        let ya = self.ya_word() as i32;
        let x_u8 = self.x;
        let y_u8 = self.y;
        let x = x_u8 as i32;
        self.psw.v = y_u8 >= x_u8;
        self.psw.h = (y_u8 & 0x0F) >= (x_u8 & 0x0F);
        if (y_u8 as i32) < (x << 1) {
            // y_u8 < 2x implies x > 0 (else 0 < 0 is false), so /x is safe.
            self.a = (ya / x) as u8;
            self.y = (ya % x) as u8;
        } else {
            // The /512 quirk: when V is set the SPC700 emits a closed-
            // form approximation rather than performing true division.
            // Bsnes-plus's expression matches hardware exactly; games
            // depend on this observable behaviour.
            let denom = 256 - x;
            let term = ya - (x << 9);
            self.a = (0xFF - term.wrapping_div(denom)) as u8;
            self.y = (x + term.wrapping_rem(denom)) as u8;
        }
        self.psw.n = (self.a & 0x80) != 0;
        self.psw.z = self.a == 0;
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

    // ----- CALL / PCALL / TCALL / RETI --------------------------------
    //
    // CALL !abs ($3F, 8 cycles): push PC of next instruction, jump to
    //   absolute address. Order: opcode + lo + hi + 3 idles + push_hi
    //   + push_lo. The 3 internal cycles are bsnes-faithful.
    // PCALL u ($4F, 6 cycles): page-call - jump to $FF00 + u. Pattern:
    //   opcode + offset + 2 idles + push_hi + push_lo.
    // TCALL n ($n1, 8 cycles): table call - vector at 0xFFDE - 2n.
    //   Pattern: opcode + 3 idles + push_hi + push_lo + read vec_lo
    //   + read vec_hi.
    // RETI ($7F, 6 cycles): pop PSW, then PC, then 2 idle cycles.
    //   Order matters: PSW pops first (bsnes; matches stack push order
    //   used by interrupts when those land).

    fn op_call_abs(&mut self, bus: &mut impl SmpBus) {
        let target = self.fetch_u16(bus);
        bus.idle();
        bus.idle();
        bus.idle();
        let return_addr = self.pc;
        self.push16(bus, return_addr);
        self.pc = target;
    }

    fn op_pcall(&mut self, bus: &mut impl SmpBus) {
        let lo = self.fetch_u8(bus);
        bus.idle();
        bus.idle();
        let return_addr = self.pc;
        self.push16(bus, return_addr);
        self.pc = 0xFF00 | lo as u16;
    }

    fn op_tcall(&mut self, bus: &mut impl SmpBus, n: u8) {
        bus.idle();
        bus.idle();
        bus.idle();
        let return_addr = self.pc;
        self.push16(bus, return_addr);
        let vec_addr = 0xFFDEu16.wrapping_sub((n as u16) << 1);
        let lo = bus.read(vec_addr) as u16;
        let hi = bus.read(vec_addr.wrapping_add(1)) as u16;
        self.pc = (hi << 8) | lo;
    }

    fn op_reti(&mut self, bus: &mut impl SmpBus) {
        let psw_byte = self.pop8(bus);
        self.psw.unpack(psw_byte);
        self.pc = self.pop16(bus);
        bus.idle();
        bus.idle();
    }

    // ----- DAA / DAS / XCN --------------------------------------------
    //
    // DAA A ($DF, 3 cycles): decimal adjust after BCD addition. If the
    //   pre-DAA A was > 0x99 or C is set, add 0x60 and set C. Then if
    //   the (post-correction) low nibble is > 9 or H is set, add 0x06.
    //   N/Z reflect the final A. C may be promoted to 1; never cleared.
    // DAS A ($BE, 3 cycles): mirror for subtraction. Subtract 0x60 if
    //   A > 0x99 or C is clear, then -0x06 if low nibble > 9 or H clear.
    //   C may be cleared to 0; never set.
    // XCN A ($9F, 5 cycles): exchange high and low nibbles of A
    //   (((A&0x0F)<<4) | (A>>4)). Sets N/Z; leaves C/V/H untouched.

    fn op_daa(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        bus.idle();
        if self.psw.c || self.a > 0x99 {
            self.a = self.a.wrapping_add(0x60);
            self.psw.c = true;
        }
        if self.psw.h || (self.a & 0x0F) > 9 {
            self.a = self.a.wrapping_add(0x06);
        }
        self.set_nz(self.a);
    }

    fn op_das(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        bus.idle();
        if !self.psw.c || self.a > 0x99 {
            self.a = self.a.wrapping_sub(0x60);
            self.psw.c = false;
        }
        if !self.psw.h || (self.a & 0x0F) > 9 {
            self.a = self.a.wrapping_sub(0x06);
        }
        self.set_nz(self.a);
    }

    fn op_xcn(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        bus.idle();
        bus.idle();
        bus.idle();
        self.a = (self.a >> 4) | (self.a << 4);
        self.set_nz(self.a);
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

    // ----- CMP family --------------------------------------------------
    //
    // CMP performs `a - b` and updates N/Z/C only - SPC700's CMP does
    // NOT touch V or H (per Anomie's notes + Mesen2 `Cmp`). Carry
    // convention matches SBC: C=1 means a >= b (no borrow). The
    // memory-destination forms (CMP (X),(Y), CMP dp,dp, CMP dp,#imm)
    // are pure comparisons - they set flags but don't write the
    // result back, so they're slightly cheaper than the SBC siblings.

    fn cmp_byte(&mut self, a: u8, b: u8) {
        let result = a.wrapping_sub(b);
        self.psw.c = a >= b;
        self.set_nz(result);
    }

    // CMP A
    fn op_cmp_a_imm(&mut self, bus: &mut impl SmpBus) {
        let v = self.fetch_u8(bus);
        self.cmp_byte(self.a, v);
    }

    fn op_cmp_a_dp(&mut self, bus: &mut impl SmpBus) {
        let dp = self.fetch_u8(bus);
        let v = self.read_dp(bus, dp);
        self.cmp_byte(self.a, v);
    }

    fn op_cmp_a_abs(&mut self, bus: &mut impl SmpBus) {
        let addr = self.fetch_u16(bus);
        let v = bus.read(addr);
        self.cmp_byte(self.a, v);
    }

    fn op_cmp_a_dp_x_direct(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        let v = bus.read(self.addr_dp_x_direct());
        self.cmp_byte(self.a, v);
    }

    fn op_cmp_a_dp_plus_x(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_dp_plus_x(bus);
        let v = bus.read(addr);
        self.cmp_byte(self.a, v);
    }

    fn op_cmp_a_abs_plus_x(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_abs_plus_x(bus);
        let v = bus.read(addr);
        self.cmp_byte(self.a, v);
    }

    fn op_cmp_a_abs_plus_y(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_abs_plus_y(bus);
        let v = bus.read(addr);
        self.cmp_byte(self.a, v);
    }

    fn op_cmp_a_dp_x_indirect(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_dp_x_indirect(bus);
        let v = bus.read(addr);
        self.cmp_byte(self.a, v);
    }

    fn op_cmp_a_dp_indirect_plus_y(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_dp_indirect_plus_y(bus);
        let v = bus.read(addr);
        self.cmp_byte(self.a, v);
    }

    fn op_cmp_x_indirect_y_indirect(&mut self, bus: &mut impl SmpBus) {
        // CMP (X),(Y): 5 cycles. Opcode + idle + read_x + read_y +
        // idle (no write - the result is discarded after flag update).
        bus.idle();
        let x_val = bus.read(self.addr_dp_x_direct());
        let y_val = bus.read(self.addr_dp_y_direct());
        self.cmp_byte(x_val, y_val);
        // The 5th cycle is the idle that an SBC would have used for
        // its writeback; CMP keeps it but doesn't touch memory.
        bus.idle();
    }

    fn op_cmp_dp_dp(&mut self, bus: &mut impl SmpBus) {
        // CMP dp,dp: 6 cycles. Opcode + src_dp + read_src + dst_dp +
        // read_dst + idle. Stream order: src, dst.
        let src_dp = self.fetch_u8(bus);
        let src_val = self.read_dp(bus, src_dp);
        let dst_dp = self.fetch_u8(bus);
        let dst_val = self.read_dp(bus, dst_dp);
        self.cmp_byte(dst_val, src_val);
        bus.idle();
    }

    fn op_cmp_dp_imm(&mut self, bus: &mut impl SmpBus) {
        // CMP dp,#imm: 5 cycles. Opcode + imm + dp_fetch + read_dp +
        // idle. Stream order: imm, dp.
        let imm = self.fetch_u8(bus);
        let dp = self.fetch_u8(bus);
        let dst_val = self.read_dp(bus, dp);
        self.cmp_byte(dst_val, imm);
        bus.idle();
    }

    // CMP X (3 modes)
    fn op_cmp_x_imm(&mut self, bus: &mut impl SmpBus) {
        let v = self.fetch_u8(bus);
        self.cmp_byte(self.x, v);
    }

    fn op_cmp_x_dp(&mut self, bus: &mut impl SmpBus) {
        let dp = self.fetch_u8(bus);
        let v = self.read_dp(bus, dp);
        self.cmp_byte(self.x, v);
    }

    fn op_cmp_x_abs(&mut self, bus: &mut impl SmpBus) {
        let addr = self.fetch_u16(bus);
        let v = bus.read(addr);
        self.cmp_byte(self.x, v);
    }

    // CMP Y (3 modes)
    fn op_cmp_y_imm(&mut self, bus: &mut impl SmpBus) {
        let v = self.fetch_u8(bus);
        self.cmp_byte(self.y, v);
    }

    fn op_cmp_y_dp(&mut self, bus: &mut impl SmpBus) {
        let dp = self.fetch_u8(bus);
        let v = self.read_dp(bus, dp);
        self.cmp_byte(self.y, v);
    }

    fn op_cmp_y_abs(&mut self, bus: &mut impl SmpBus) {
        let addr = self.fetch_u16(bus);
        let v = bus.read(addr);
        self.cmp_byte(self.y, v);
    }

    // ----- AND / OR / EOR families ------------------------------------
    //
    // Bitwise logical ops: cycle counts and addressing modes mirror
    // ADC/SBC exactly (12 modes each). Flags: only N/Z are touched -
    // C/V/H are preserved (Anomie's notes; Mesen2 `Or` / `And` / `Eor`).
    // Memory-destination forms (dp,dp / dp,#imm / (X),(Y)) compute the
    // op into the destination and write the result back, like SBC -
    // not like CMP which discards the result.

    fn or_byte(&mut self, a: u8, b: u8) -> u8 {
        let r = a | b;
        self.set_nz(r);
        r
    }
    fn and_byte(&mut self, a: u8, b: u8) -> u8 {
        let r = a & b;
        self.set_nz(r);
        r
    }
    fn eor_byte(&mut self, a: u8, b: u8) -> u8 {
        let r = a ^ b;
        self.set_nz(r);
        r
    }

    // ----- OR ----------------------------------------------------------
    fn op_or_a_imm(&mut self, bus: &mut impl SmpBus) {
        let v = self.fetch_u8(bus);
        self.a = self.or_byte(self.a, v);
    }
    fn op_or_a_dp(&mut self, bus: &mut impl SmpBus) {
        let dp = self.fetch_u8(bus);
        let v = self.read_dp(bus, dp);
        self.a = self.or_byte(self.a, v);
    }
    fn op_or_a_abs(&mut self, bus: &mut impl SmpBus) {
        let addr = self.fetch_u16(bus);
        let v = bus.read(addr);
        self.a = self.or_byte(self.a, v);
    }
    fn op_or_a_dp_x_direct(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        let v = bus.read(self.addr_dp_x_direct());
        self.a = self.or_byte(self.a, v);
    }
    fn op_or_a_dp_plus_x(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_dp_plus_x(bus);
        let v = bus.read(addr);
        self.a = self.or_byte(self.a, v);
    }
    fn op_or_a_abs_plus_x(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_abs_plus_x(bus);
        let v = bus.read(addr);
        self.a = self.or_byte(self.a, v);
    }
    fn op_or_a_abs_plus_y(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_abs_plus_y(bus);
        let v = bus.read(addr);
        self.a = self.or_byte(self.a, v);
    }
    fn op_or_a_dp_x_indirect(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_dp_x_indirect(bus);
        let v = bus.read(addr);
        self.a = self.or_byte(self.a, v);
    }
    fn op_or_a_dp_indirect_plus_y(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_dp_indirect_plus_y(bus);
        let v = bus.read(addr);
        self.a = self.or_byte(self.a, v);
    }
    fn op_or_x_indirect_y_indirect(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        let y_val = bus.read(self.addr_dp_y_direct());
        let x_addr = self.addr_dp_x_direct();
        let x_val = bus.read(x_addr);
        let result = self.or_byte(x_val, y_val);
        bus.write(x_addr, result);
    }
    fn op_or_dp_dp(&mut self, bus: &mut impl SmpBus) {
        let src_dp = self.fetch_u8(bus);
        let src_val = self.read_dp(bus, src_dp);
        let dst_dp = self.fetch_u8(bus);
        let dst_val = self.read_dp(bus, dst_dp);
        let result = self.or_byte(dst_val, src_val);
        self.write_dp(bus, dst_dp, result);
    }
    fn op_or_dp_imm(&mut self, bus: &mut impl SmpBus) {
        let imm = self.fetch_u8(bus);
        let dp = self.fetch_u8(bus);
        let dst_val = self.read_dp(bus, dp);
        let result = self.or_byte(dst_val, imm);
        self.write_dp(bus, dp, result);
    }

    // ----- AND ---------------------------------------------------------
    fn op_and_a_imm(&mut self, bus: &mut impl SmpBus) {
        let v = self.fetch_u8(bus);
        self.a = self.and_byte(self.a, v);
    }
    fn op_and_a_dp(&mut self, bus: &mut impl SmpBus) {
        let dp = self.fetch_u8(bus);
        let v = self.read_dp(bus, dp);
        self.a = self.and_byte(self.a, v);
    }
    fn op_and_a_abs(&mut self, bus: &mut impl SmpBus) {
        let addr = self.fetch_u16(bus);
        let v = bus.read(addr);
        self.a = self.and_byte(self.a, v);
    }
    fn op_and_a_dp_x_direct(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        let v = bus.read(self.addr_dp_x_direct());
        self.a = self.and_byte(self.a, v);
    }
    fn op_and_a_dp_plus_x(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_dp_plus_x(bus);
        let v = bus.read(addr);
        self.a = self.and_byte(self.a, v);
    }
    fn op_and_a_abs_plus_x(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_abs_plus_x(bus);
        let v = bus.read(addr);
        self.a = self.and_byte(self.a, v);
    }
    fn op_and_a_abs_plus_y(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_abs_plus_y(bus);
        let v = bus.read(addr);
        self.a = self.and_byte(self.a, v);
    }
    fn op_and_a_dp_x_indirect(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_dp_x_indirect(bus);
        let v = bus.read(addr);
        self.a = self.and_byte(self.a, v);
    }
    fn op_and_a_dp_indirect_plus_y(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_dp_indirect_plus_y(bus);
        let v = bus.read(addr);
        self.a = self.and_byte(self.a, v);
    }
    fn op_and_x_indirect_y_indirect(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        let y_val = bus.read(self.addr_dp_y_direct());
        let x_addr = self.addr_dp_x_direct();
        let x_val = bus.read(x_addr);
        let result = self.and_byte(x_val, y_val);
        bus.write(x_addr, result);
    }
    fn op_and_dp_dp(&mut self, bus: &mut impl SmpBus) {
        let src_dp = self.fetch_u8(bus);
        let src_val = self.read_dp(bus, src_dp);
        let dst_dp = self.fetch_u8(bus);
        let dst_val = self.read_dp(bus, dst_dp);
        let result = self.and_byte(dst_val, src_val);
        self.write_dp(bus, dst_dp, result);
    }
    fn op_and_dp_imm(&mut self, bus: &mut impl SmpBus) {
        let imm = self.fetch_u8(bus);
        let dp = self.fetch_u8(bus);
        let dst_val = self.read_dp(bus, dp);
        let result = self.and_byte(dst_val, imm);
        self.write_dp(bus, dp, result);
    }

    // ----- EOR ---------------------------------------------------------
    fn op_eor_a_imm(&mut self, bus: &mut impl SmpBus) {
        let v = self.fetch_u8(bus);
        self.a = self.eor_byte(self.a, v);
    }
    fn op_eor_a_dp(&mut self, bus: &mut impl SmpBus) {
        let dp = self.fetch_u8(bus);
        let v = self.read_dp(bus, dp);
        self.a = self.eor_byte(self.a, v);
    }
    fn op_eor_a_abs(&mut self, bus: &mut impl SmpBus) {
        let addr = self.fetch_u16(bus);
        let v = bus.read(addr);
        self.a = self.eor_byte(self.a, v);
    }
    fn op_eor_a_dp_x_direct(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        let v = bus.read(self.addr_dp_x_direct());
        self.a = self.eor_byte(self.a, v);
    }
    fn op_eor_a_dp_plus_x(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_dp_plus_x(bus);
        let v = bus.read(addr);
        self.a = self.eor_byte(self.a, v);
    }
    fn op_eor_a_abs_plus_x(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_abs_plus_x(bus);
        let v = bus.read(addr);
        self.a = self.eor_byte(self.a, v);
    }
    fn op_eor_a_abs_plus_y(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_abs_plus_y(bus);
        let v = bus.read(addr);
        self.a = self.eor_byte(self.a, v);
    }
    fn op_eor_a_dp_x_indirect(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_dp_x_indirect(bus);
        let v = bus.read(addr);
        self.a = self.eor_byte(self.a, v);
    }
    fn op_eor_a_dp_indirect_plus_y(&mut self, bus: &mut impl SmpBus) {
        let addr = self.addr_dp_indirect_plus_y(bus);
        let v = bus.read(addr);
        self.a = self.eor_byte(self.a, v);
    }
    fn op_eor_x_indirect_y_indirect(&mut self, bus: &mut impl SmpBus) {
        bus.idle();
        let y_val = bus.read(self.addr_dp_y_direct());
        let x_addr = self.addr_dp_x_direct();
        let x_val = bus.read(x_addr);
        let result = self.eor_byte(x_val, y_val);
        bus.write(x_addr, result);
    }
    fn op_eor_dp_dp(&mut self, bus: &mut impl SmpBus) {
        let src_dp = self.fetch_u8(bus);
        let src_val = self.read_dp(bus, src_dp);
        let dst_dp = self.fetch_u8(bus);
        let dst_val = self.read_dp(bus, dst_dp);
        let result = self.eor_byte(dst_val, src_val);
        self.write_dp(bus, dst_dp, result);
    }
    fn op_eor_dp_imm(&mut self, bus: &mut impl SmpBus) {
        let imm = self.fetch_u8(bus);
        let dp = self.fetch_u8(bus);
        let dst_val = self.read_dp(bus, dp);
        let result = self.eor_byte(dst_val, imm);
        self.write_dp(bus, dp, result);
    }
}

impl Default for Smp {
    fn default() -> Self {
        Self::new()
    }
}
