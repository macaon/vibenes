//! Official 6502 opcode set (151 instructions) with cycle-accurate bus
//! access patterns. Dummy reads/writes are emitted so the bus charges the
//! right number of cycles.

use super::Cpu;
use crate::bus::Bus;

type OpResult = Result<(), String>;

// ---------- Addressing-mode helpers ------------------------------------

fn addr_zp(cpu: &mut Cpu, bus: &mut Bus) -> u16 {
    cpu.fetch_byte(bus) as u16
}

fn addr_zp_x(cpu: &mut Cpu, bus: &mut Bus) -> u16 {
    let base = cpu.fetch_byte(bus);
    bus.read(base as u16); // dummy read
    base.wrapping_add(cpu.x) as u16
}

fn addr_zp_y(cpu: &mut Cpu, bus: &mut Bus) -> u16 {
    let base = cpu.fetch_byte(bus);
    bus.read(base as u16); // dummy read
    base.wrapping_add(cpu.y) as u16
}

fn addr_abs(cpu: &mut Cpu, bus: &mut Bus) -> u16 {
    cpu.fetch_word(bus)
}

fn addr_abs_indexed_read(cpu: &mut Cpu, bus: &mut Bus, index: u8) -> u16 {
    let base = cpu.fetch_word(bus);
    let effective = base.wrapping_add(index as u16);
    if (base & 0xFF00) != (effective & 0xFF00) {
        // Page crossed: the real chip first reads from the bad (un-carried)
        // high byte, then re-reads from the correct address. Emit the
        // dummy via `bus.dummy_read` so the page-cross-to-$2007 quirk
        // (buffer doesn't advance twice) is modelled — see
        // `Ppu::cpu_read_dummy`.
        let bad = (base & 0xFF00) | (effective & 0x00FF);
        bus.dummy_read(bad);
    }
    effective
}

fn addr_abs_indexed_rmw(cpu: &mut Cpu, bus: &mut Bus, index: u8) -> u16 {
    let base = cpu.fetch_word(bus);
    let effective = base.wrapping_add(index as u16);
    // Always perform the dummy read for RMW / STx.
    let bad = (base & 0xFF00) | (effective & 0x00FF);
    bus.read(bad);
    effective
}

fn addr_ind_x(cpu: &mut Cpu, bus: &mut Bus) -> u16 {
    let base = cpu.fetch_byte(bus);
    bus.read(base as u16); // dummy read at zp base
    let ptr = base.wrapping_add(cpu.x);
    let lo = bus.read(ptr as u16);
    let hi = bus.read(ptr.wrapping_add(1) as u16);
    u16::from_le_bytes([lo, hi])
}

fn addr_ind_y_read(cpu: &mut Cpu, bus: &mut Bus) -> u16 {
    let ptr = cpu.fetch_byte(bus);
    let lo = bus.read(ptr as u16);
    let hi = bus.read(ptr.wrapping_add(1) as u16);
    let base = u16::from_le_bytes([lo, hi]);
    let effective = base.wrapping_add(cpu.y as u16);
    if (base & 0xFF00) != (effective & 0xFF00) {
        let bad = (base & 0xFF00) | (effective & 0x00FF);
        bus.dummy_read(bad);
    }
    effective
}

fn addr_ind_y_rmw(cpu: &mut Cpu, bus: &mut Bus) -> u16 {
    let ptr = cpu.fetch_byte(bus);
    let lo = bus.read(ptr as u16);
    let hi = bus.read(ptr.wrapping_add(1) as u16);
    let base = u16::from_le_bytes([lo, hi]);
    let effective = base.wrapping_add(cpu.y as u16);
    let bad = (base & 0xFF00) | (effective & 0x00FF);
    bus.read(bad);
    effective
}

// ---------- ALU helpers -------------------------------------------------

fn adc(cpu: &mut Cpu, value: u8) {
    // 2A03 ignores decimal mode; binary ADC only.
    let a = cpu.a as u16;
    let m = value as u16;
    let c = cpu.p.carry() as u16;
    let sum = a + m + c;
    let result = sum as u8;
    cpu.p.set_carry(sum > 0xFF);
    cpu.p.set_overflow(((cpu.a ^ result) & (value ^ result) & 0x80) != 0);
    cpu.a = result;
    cpu.set_zn(result);
}

fn sbc(cpu: &mut Cpu, value: u8) {
    adc(cpu, value ^ 0xFF);
}

fn compare(cpu: &mut Cpu, reg: u8, value: u8) {
    let result = reg.wrapping_sub(value);
    cpu.p.set_carry(reg >= value);
    cpu.p.set_zero(reg == value);
    cpu.p.set_negative((result & 0x80) != 0);
}

fn and(cpu: &mut Cpu, value: u8) {
    cpu.a &= value;
    cpu.set_zn(cpu.a);
}
fn ora(cpu: &mut Cpu, value: u8) {
    cpu.a |= value;
    cpu.set_zn(cpu.a);
}
fn eor(cpu: &mut Cpu, value: u8) {
    cpu.a ^= value;
    cpu.set_zn(cpu.a);
}
fn bit(cpu: &mut Cpu, value: u8) {
    cpu.p.set_zero((cpu.a & value) == 0);
    cpu.p.set_overflow((value & 0x40) != 0);
    cpu.p.set_negative((value & 0x80) != 0);
}

fn asl_value(cpu: &mut Cpu, value: u8) -> u8 {
    cpu.p.set_carry((value & 0x80) != 0);
    let result = value.wrapping_shl(1);
    cpu.set_zn(result);
    result
}

fn lsr_value(cpu: &mut Cpu, value: u8) -> u8 {
    cpu.p.set_carry((value & 0x01) != 0);
    let result = value >> 1;
    cpu.set_zn(result);
    result
}

fn rol_value(cpu: &mut Cpu, value: u8) -> u8 {
    let carry_in = cpu.p.carry() as u8;
    cpu.p.set_carry((value & 0x80) != 0);
    let result = (value << 1) | carry_in;
    cpu.set_zn(result);
    result
}

fn ror_value(cpu: &mut Cpu, value: u8) -> u8 {
    let carry_in = cpu.p.carry() as u8;
    cpu.p.set_carry((value & 0x01) != 0);
    let result = (value >> 1) | (carry_in << 7);
    cpu.set_zn(result);
    result
}

fn inc_value(cpu: &mut Cpu, value: u8) -> u8 {
    let result = value.wrapping_add(1);
    cpu.set_zn(result);
    result
}

fn dec_value(cpu: &mut Cpu, value: u8) -> u8 {
    let result = value.wrapping_sub(1);
    cpu.set_zn(result);
    result
}

fn rmw_zp(cpu: &mut Cpu, bus: &mut Bus, op: fn(&mut Cpu, u8) -> u8) {
    let addr = addr_zp(cpu, bus);
    let value = bus.read(addr);
    bus.write(addr, value); // dummy write
    let result = op(cpu, value);
    bus.write(addr, result);
}

fn rmw_zp_x(cpu: &mut Cpu, bus: &mut Bus, op: fn(&mut Cpu, u8) -> u8) {
    let addr = addr_zp_x(cpu, bus);
    let value = bus.read(addr);
    bus.write(addr, value);
    let result = op(cpu, value);
    bus.write(addr, result);
}

fn rmw_abs(cpu: &mut Cpu, bus: &mut Bus, op: fn(&mut Cpu, u8) -> u8) {
    let addr = addr_abs(cpu, bus);
    let value = bus.read(addr);
    bus.write(addr, value);
    let result = op(cpu, value);
    bus.write(addr, result);
}

fn rmw_abs_x(cpu: &mut Cpu, bus: &mut Bus, op: fn(&mut Cpu, u8) -> u8) {
    let addr = addr_abs_indexed_rmw(cpu, bus, cpu.x);
    let value = bus.read(addr);
    bus.write(addr, value);
    let result = op(cpu, value);
    bus.write(addr, result);
}

// --- Combined RMW: perform an RMW op on memory, then apply an effect on A ---

type RmwOp = fn(&mut Cpu, u8) -> u8;
type Combine = fn(&mut Cpu, u8);

fn rmw_zp_combine(cpu: &mut Cpu, bus: &mut Bus, op: RmwOp, combine: Combine) {
    let addr = addr_zp(cpu, bus);
    let value = bus.read(addr);
    bus.write(addr, value);
    let result = op(cpu, value);
    bus.write(addr, result);
    combine(cpu, result);
}

fn rmw_zp_x_combine(cpu: &mut Cpu, bus: &mut Bus, op: RmwOp, combine: Combine) {
    let addr = addr_zp_x(cpu, bus);
    let value = bus.read(addr);
    bus.write(addr, value);
    let result = op(cpu, value);
    bus.write(addr, result);
    combine(cpu, result);
}

fn rmw_abs_combine(cpu: &mut Cpu, bus: &mut Bus, op: RmwOp, combine: Combine) {
    let addr = addr_abs(cpu, bus);
    let value = bus.read(addr);
    bus.write(addr, value);
    let result = op(cpu, value);
    bus.write(addr, result);
    combine(cpu, result);
}

fn rmw_abs_x_combine(cpu: &mut Cpu, bus: &mut Bus, op: RmwOp, combine: Combine) {
    let addr = addr_abs_indexed_rmw(cpu, bus, cpu.x);
    let value = bus.read(addr);
    bus.write(addr, value);
    let result = op(cpu, value);
    bus.write(addr, result);
    combine(cpu, result);
}

fn rmw_abs_y_combine(cpu: &mut Cpu, bus: &mut Bus, op: RmwOp, combine: Combine) {
    let addr = addr_abs_indexed_rmw(cpu, bus, cpu.y);
    let value = bus.read(addr);
    bus.write(addr, value);
    let result = op(cpu, value);
    bus.write(addr, result);
    combine(cpu, result);
}

fn rmw_ind_x_combine(cpu: &mut Cpu, bus: &mut Bus, op: RmwOp, combine: Combine) {
    let addr = addr_ind_x(cpu, bus);
    let value = bus.read(addr);
    bus.write(addr, value);
    let result = op(cpu, value);
    bus.write(addr, result);
    combine(cpu, result);
}

fn rmw_ind_y_combine(cpu: &mut Cpu, bus: &mut Bus, op: RmwOp, combine: Combine) {
    let addr = addr_ind_y_rmw(cpu, bus);
    let value = bus.read(addr);
    bus.write(addr, value);
    let result = op(cpu, value);
    bus.write(addr, result);
    combine(cpu, result);
}

fn ora_combine(cpu: &mut Cpu, value: u8) {
    ora(cpu, value);
}
fn and_combine(cpu: &mut Cpu, value: u8) {
    and(cpu, value);
}
fn eor_combine(cpu: &mut Cpu, value: u8) {
    eor(cpu, value);
}
fn adc_combine(cpu: &mut Cpu, value: u8) {
    adc(cpu, value);
}
fn sbc_combine(cpu: &mut Cpu, value: u8) {
    sbc(cpu, value);
}
fn cmp_a_combine(cpu: &mut Cpu, value: u8) {
    compare(cpu, cpu.a, value);
}

fn branch(cpu: &mut Cpu, bus: &mut Bus, condition: bool) {
    let offset = cpu.fetch_byte(bus) as i8;
    // `bus.prev_irq_line` is the IRQ-line snapshot captured at the
    // start of the current bus cycle. The operand fetch above is the
    // branch's cycle 2; when it returns, `prev_irq_line` reflects
    // the line state at the end of cycle 1 — one cycle before the
    // penultimate. The 6502's "branch-delays-IRQ" quirk only
    // suppresses recognition when IRQ was newly asserted *during*
    // the penultimate cycle (i.e. low at end-of-1, high at end-of-2),
    // matching Mesen2's `_prevRunIrq` sample inside `BranchRelative`
    // and puNES's `.before` in the `BRC` macro.
    let irq_before_penult = bus.prev_irq_line;
    if condition {
        bus.read(cpu.pc); // dummy read before branching
        let new_pc = (cpu.pc as i32).wrapping_add(offset as i32) as u16;
        let page_crossed = (cpu.pc & 0xFF00) != (new_pc & 0xFF00);
        if page_crossed {
            // Page cross: extra dummy read at the un-carried high byte.
            let bad = (cpu.pc & 0xFF00) | (new_pc & 0x00FF);
            bus.read(bad);
        }
        cpu.pc = new_pc;
        // 3-cycle taken branch form → apply the quirk iff IRQ was
        // still low one cycle before the penultimate. A branch that
        // started with IRQ already asserted doesn't get the free
        // pass — it polls normally on its penultimate.
        if !page_crossed && !irq_before_penult {
            cpu.mark_branch_taken_no_cross();
        }
    }
}

// ---------- Dispatch ----------------------------------------------------

pub fn execute(cpu: &mut Cpu, bus: &mut Bus, op: u8) -> OpResult {
    match op {
        // --- Load ---
        0xA9 => {
            let v = cpu.fetch_byte(bus);
            cpu.a = v;
            cpu.set_zn(v);
        }
        0xA5 => {
            let a = addr_zp(cpu, bus);
            let v = bus.read(a);
            cpu.a = v;
            cpu.set_zn(v);
        }
        0xB5 => {
            let a = addr_zp_x(cpu, bus);
            let v = bus.read(a);
            cpu.a = v;
            cpu.set_zn(v);
        }
        0xAD => {
            let a = addr_abs(cpu, bus);
            let v = bus.read(a);
            cpu.a = v;
            cpu.set_zn(v);
        }
        0xBD => {
            let a = addr_abs_indexed_read(cpu, bus, cpu.x);
            let v = bus.read(a);
            cpu.a = v;
            cpu.set_zn(v);
        }
        0xB9 => {
            let a = addr_abs_indexed_read(cpu, bus, cpu.y);
            let v = bus.read(a);
            cpu.a = v;
            cpu.set_zn(v);
        }
        0xA1 => {
            let a = addr_ind_x(cpu, bus);
            let v = bus.read(a);
            cpu.a = v;
            cpu.set_zn(v);
        }
        0xB1 => {
            let a = addr_ind_y_read(cpu, bus);
            let v = bus.read(a);
            cpu.a = v;
            cpu.set_zn(v);
        }

        0xA2 => {
            let v = cpu.fetch_byte(bus);
            cpu.x = v;
            cpu.set_zn(v);
        }
        0xA6 => {
            let a = addr_zp(cpu, bus);
            let v = bus.read(a);
            cpu.x = v;
            cpu.set_zn(v);
        }
        0xB6 => {
            let a = addr_zp_y(cpu, bus);
            let v = bus.read(a);
            cpu.x = v;
            cpu.set_zn(v);
        }
        0xAE => {
            let a = addr_abs(cpu, bus);
            let v = bus.read(a);
            cpu.x = v;
            cpu.set_zn(v);
        }
        0xBE => {
            let a = addr_abs_indexed_read(cpu, bus, cpu.y);
            let v = bus.read(a);
            cpu.x = v;
            cpu.set_zn(v);
        }

        0xA0 => {
            let v = cpu.fetch_byte(bus);
            cpu.y = v;
            cpu.set_zn(v);
        }
        0xA4 => {
            let a = addr_zp(cpu, bus);
            let v = bus.read(a);
            cpu.y = v;
            cpu.set_zn(v);
        }
        0xB4 => {
            let a = addr_zp_x(cpu, bus);
            let v = bus.read(a);
            cpu.y = v;
            cpu.set_zn(v);
        }
        0xAC => {
            let a = addr_abs(cpu, bus);
            let v = bus.read(a);
            cpu.y = v;
            cpu.set_zn(v);
        }
        0xBC => {
            let a = addr_abs_indexed_read(cpu, bus, cpu.x);
            let v = bus.read(a);
            cpu.y = v;
            cpu.set_zn(v);
        }

        // --- Store ---
        0x85 => {
            let a = addr_zp(cpu, bus);
            bus.write(a, cpu.a);
        }
        0x95 => {
            let a = addr_zp_x(cpu, bus);
            bus.write(a, cpu.a);
        }
        0x8D => {
            let a = addr_abs(cpu, bus);
            bus.write(a, cpu.a);
        }
        0x9D => {
            let a = addr_abs_indexed_rmw(cpu, bus, cpu.x);
            bus.write(a, cpu.a);
        }
        0x99 => {
            let a = addr_abs_indexed_rmw(cpu, bus, cpu.y);
            bus.write(a, cpu.a);
        }
        0x81 => {
            let a = addr_ind_x(cpu, bus);
            bus.write(a, cpu.a);
        }
        0x91 => {
            let a = addr_ind_y_rmw(cpu, bus);
            bus.write(a, cpu.a);
        }

        0x86 => {
            let a = addr_zp(cpu, bus);
            bus.write(a, cpu.x);
        }
        0x96 => {
            let a = addr_zp_y(cpu, bus);
            bus.write(a, cpu.x);
        }
        0x8E => {
            let a = addr_abs(cpu, bus);
            bus.write(a, cpu.x);
        }

        0x84 => {
            let a = addr_zp(cpu, bus);
            bus.write(a, cpu.y);
        }
        0x94 => {
            let a = addr_zp_x(cpu, bus);
            bus.write(a, cpu.y);
        }
        0x8C => {
            let a = addr_abs(cpu, bus);
            bus.write(a, cpu.y);
        }

        // --- Transfers ---
        0xAA => {
            bus.read(cpu.pc);
            cpu.x = cpu.a;
            cpu.set_zn(cpu.x);
        }
        0xA8 => {
            bus.read(cpu.pc);
            cpu.y = cpu.a;
            cpu.set_zn(cpu.y);
        }
        0xBA => {
            bus.read(cpu.pc);
            cpu.x = cpu.sp;
            cpu.set_zn(cpu.x);
        }
        0x8A => {
            bus.read(cpu.pc);
            cpu.a = cpu.x;
            cpu.set_zn(cpu.a);
        }
        0x9A => {
            bus.read(cpu.pc);
            cpu.sp = cpu.x;
        }
        0x98 => {
            bus.read(cpu.pc);
            cpu.a = cpu.y;
            cpu.set_zn(cpu.a);
        }

        // --- Stack ---
        0x48 => {
            bus.read(cpu.pc);
            cpu.push(bus, cpu.a);
        }
        0x08 => {
            bus.read(cpu.pc);
            // PHP pushes with B=1, U=1.
            let status = cpu.p.to_u8() | 0x30;
            cpu.push(bus, status);
        }
        0x68 => {
            bus.read(cpu.pc);
            bus.read(0x0100 | cpu.sp as u16); // dummy
            let v = cpu.pop(bus);
            cpu.a = v;
            cpu.set_zn(v);
        }
        0x28 => {
            bus.read(cpu.pc);
            bus.read(0x0100 | cpu.sp as u16); // dummy
            let v = cpu.pop(bus);
            cpu.p = super::flags::StatusFlags::from_bits(v);
        }

        // --- Logic (immediate + addressing set) ---
        0x29 => {
            let v = cpu.fetch_byte(bus);
            and(cpu, v);
        }
        0x25 => {
            let a = addr_zp(cpu, bus);
            let v = bus.read(a);
            and(cpu, v);
        }
        0x35 => {
            let a = addr_zp_x(cpu, bus);
            let v = bus.read(a);
            and(cpu, v);
        }
        0x2D => {
            let a = addr_abs(cpu, bus);
            let v = bus.read(a);
            and(cpu, v);
        }
        0x3D => {
            let a = addr_abs_indexed_read(cpu, bus, cpu.x);
            let v = bus.read(a);
            and(cpu, v);
        }
        0x39 => {
            let a = addr_abs_indexed_read(cpu, bus, cpu.y);
            let v = bus.read(a);
            and(cpu, v);
        }
        0x21 => {
            let a = addr_ind_x(cpu, bus);
            let v = bus.read(a);
            and(cpu, v);
        }
        0x31 => {
            let a = addr_ind_y_read(cpu, bus);
            let v = bus.read(a);
            and(cpu, v);
        }

        0x09 => {
            let v = cpu.fetch_byte(bus);
            ora(cpu, v);
        }
        0x05 => {
            let a = addr_zp(cpu, bus);
            let v = bus.read(a);
            ora(cpu, v);
        }
        0x15 => {
            let a = addr_zp_x(cpu, bus);
            let v = bus.read(a);
            ora(cpu, v);
        }
        0x0D => {
            let a = addr_abs(cpu, bus);
            let v = bus.read(a);
            ora(cpu, v);
        }
        0x1D => {
            let a = addr_abs_indexed_read(cpu, bus, cpu.x);
            let v = bus.read(a);
            ora(cpu, v);
        }
        0x19 => {
            let a = addr_abs_indexed_read(cpu, bus, cpu.y);
            let v = bus.read(a);
            ora(cpu, v);
        }
        0x01 => {
            let a = addr_ind_x(cpu, bus);
            let v = bus.read(a);
            ora(cpu, v);
        }
        0x11 => {
            let a = addr_ind_y_read(cpu, bus);
            let v = bus.read(a);
            ora(cpu, v);
        }

        0x49 => {
            let v = cpu.fetch_byte(bus);
            eor(cpu, v);
        }
        0x45 => {
            let a = addr_zp(cpu, bus);
            let v = bus.read(a);
            eor(cpu, v);
        }
        0x55 => {
            let a = addr_zp_x(cpu, bus);
            let v = bus.read(a);
            eor(cpu, v);
        }
        0x4D => {
            let a = addr_abs(cpu, bus);
            let v = bus.read(a);
            eor(cpu, v);
        }
        0x5D => {
            let a = addr_abs_indexed_read(cpu, bus, cpu.x);
            let v = bus.read(a);
            eor(cpu, v);
        }
        0x59 => {
            let a = addr_abs_indexed_read(cpu, bus, cpu.y);
            let v = bus.read(a);
            eor(cpu, v);
        }
        0x41 => {
            let a = addr_ind_x(cpu, bus);
            let v = bus.read(a);
            eor(cpu, v);
        }
        0x51 => {
            let a = addr_ind_y_read(cpu, bus);
            let v = bus.read(a);
            eor(cpu, v);
        }

        0x24 => {
            let a = addr_zp(cpu, bus);
            let v = bus.read(a);
            bit(cpu, v);
        }
        0x2C => {
            let a = addr_abs(cpu, bus);
            let v = bus.read(a);
            bit(cpu, v);
        }

        // --- Arithmetic ---
        0x69 => {
            let v = cpu.fetch_byte(bus);
            adc(cpu, v);
        }
        0x65 => {
            let a = addr_zp(cpu, bus);
            let v = bus.read(a);
            adc(cpu, v);
        }
        0x75 => {
            let a = addr_zp_x(cpu, bus);
            let v = bus.read(a);
            adc(cpu, v);
        }
        0x6D => {
            let a = addr_abs(cpu, bus);
            let v = bus.read(a);
            adc(cpu, v);
        }
        0x7D => {
            let a = addr_abs_indexed_read(cpu, bus, cpu.x);
            let v = bus.read(a);
            adc(cpu, v);
        }
        0x79 => {
            let a = addr_abs_indexed_read(cpu, bus, cpu.y);
            let v = bus.read(a);
            adc(cpu, v);
        }
        0x61 => {
            let a = addr_ind_x(cpu, bus);
            let v = bus.read(a);
            adc(cpu, v);
        }
        0x71 => {
            let a = addr_ind_y_read(cpu, bus);
            let v = bus.read(a);
            adc(cpu, v);
        }

        0xE9 => {
            let v = cpu.fetch_byte(bus);
            sbc(cpu, v);
        }
        0xE5 => {
            let a = addr_zp(cpu, bus);
            let v = bus.read(a);
            sbc(cpu, v);
        }
        0xF5 => {
            let a = addr_zp_x(cpu, bus);
            let v = bus.read(a);
            sbc(cpu, v);
        }
        0xED => {
            let a = addr_abs(cpu, bus);
            let v = bus.read(a);
            sbc(cpu, v);
        }
        0xFD => {
            let a = addr_abs_indexed_read(cpu, bus, cpu.x);
            let v = bus.read(a);
            sbc(cpu, v);
        }
        0xF9 => {
            let a = addr_abs_indexed_read(cpu, bus, cpu.y);
            let v = bus.read(a);
            sbc(cpu, v);
        }
        0xE1 => {
            let a = addr_ind_x(cpu, bus);
            let v = bus.read(a);
            sbc(cpu, v);
        }
        0xF1 => {
            let a = addr_ind_y_read(cpu, bus);
            let v = bus.read(a);
            sbc(cpu, v);
        }

        // --- Compare ---
        0xC9 => {
            let v = cpu.fetch_byte(bus);
            compare(cpu, cpu.a, v);
        }
        0xC5 => {
            let a = addr_zp(cpu, bus);
            let v = bus.read(a);
            compare(cpu, cpu.a, v);
        }
        0xD5 => {
            let a = addr_zp_x(cpu, bus);
            let v = bus.read(a);
            compare(cpu, cpu.a, v);
        }
        0xCD => {
            let a = addr_abs(cpu, bus);
            let v = bus.read(a);
            compare(cpu, cpu.a, v);
        }
        0xDD => {
            let a = addr_abs_indexed_read(cpu, bus, cpu.x);
            let v = bus.read(a);
            compare(cpu, cpu.a, v);
        }
        0xD9 => {
            let a = addr_abs_indexed_read(cpu, bus, cpu.y);
            let v = bus.read(a);
            compare(cpu, cpu.a, v);
        }
        0xC1 => {
            let a = addr_ind_x(cpu, bus);
            let v = bus.read(a);
            compare(cpu, cpu.a, v);
        }
        0xD1 => {
            let a = addr_ind_y_read(cpu, bus);
            let v = bus.read(a);
            compare(cpu, cpu.a, v);
        }

        0xE0 => {
            let v = cpu.fetch_byte(bus);
            compare(cpu, cpu.x, v);
        }
        0xE4 => {
            let a = addr_zp(cpu, bus);
            let v = bus.read(a);
            compare(cpu, cpu.x, v);
        }
        0xEC => {
            let a = addr_abs(cpu, bus);
            let v = bus.read(a);
            compare(cpu, cpu.x, v);
        }

        0xC0 => {
            let v = cpu.fetch_byte(bus);
            compare(cpu, cpu.y, v);
        }
        0xC4 => {
            let a = addr_zp(cpu, bus);
            let v = bus.read(a);
            compare(cpu, cpu.y, v);
        }
        0xCC => {
            let a = addr_abs(cpu, bus);
            let v = bus.read(a);
            compare(cpu, cpu.y, v);
        }

        // --- Shifts / rotates ---
        0x0A => {
            bus.read(cpu.pc);
            cpu.a = asl_value(cpu, cpu.a);
        }
        0x06 => rmw_zp(cpu, bus, asl_value),
        0x16 => rmw_zp_x(cpu, bus, asl_value),
        0x0E => rmw_abs(cpu, bus, asl_value),
        0x1E => rmw_abs_x(cpu, bus, asl_value),

        0x4A => {
            bus.read(cpu.pc);
            cpu.a = lsr_value(cpu, cpu.a);
        }
        0x46 => rmw_zp(cpu, bus, lsr_value),
        0x56 => rmw_zp_x(cpu, bus, lsr_value),
        0x4E => rmw_abs(cpu, bus, lsr_value),
        0x5E => rmw_abs_x(cpu, bus, lsr_value),

        0x2A => {
            bus.read(cpu.pc);
            cpu.a = rol_value(cpu, cpu.a);
        }
        0x26 => rmw_zp(cpu, bus, rol_value),
        0x36 => rmw_zp_x(cpu, bus, rol_value),
        0x2E => rmw_abs(cpu, bus, rol_value),
        0x3E => rmw_abs_x(cpu, bus, rol_value),

        0x6A => {
            bus.read(cpu.pc);
            cpu.a = ror_value(cpu, cpu.a);
        }
        0x66 => rmw_zp(cpu, bus, ror_value),
        0x76 => rmw_zp_x(cpu, bus, ror_value),
        0x6E => rmw_abs(cpu, bus, ror_value),
        0x7E => rmw_abs_x(cpu, bus, ror_value),

        // --- Increment / decrement ---
        0xE6 => rmw_zp(cpu, bus, inc_value),
        0xF6 => rmw_zp_x(cpu, bus, inc_value),
        0xEE => rmw_abs(cpu, bus, inc_value),
        0xFE => rmw_abs_x(cpu, bus, inc_value),
        0xC6 => rmw_zp(cpu, bus, dec_value),
        0xD6 => rmw_zp_x(cpu, bus, dec_value),
        0xCE => rmw_abs(cpu, bus, dec_value),
        0xDE => rmw_abs_x(cpu, bus, dec_value),
        0xE8 => {
            bus.read(cpu.pc);
            cpu.x = cpu.x.wrapping_add(1);
            cpu.set_zn(cpu.x);
        }
        0xC8 => {
            bus.read(cpu.pc);
            cpu.y = cpu.y.wrapping_add(1);
            cpu.set_zn(cpu.y);
        }
        0xCA => {
            bus.read(cpu.pc);
            cpu.x = cpu.x.wrapping_sub(1);
            cpu.set_zn(cpu.x);
        }
        0x88 => {
            bus.read(cpu.pc);
            cpu.y = cpu.y.wrapping_sub(1);
            cpu.set_zn(cpu.y);
        }

        // --- Jumps ---
        0x4C => {
            let target = cpu.fetch_word(bus);
            cpu.pc = target;
        }
        0x6C => {
            let ptr = cpu.fetch_word(bus);
            let lo = bus.read(ptr);
            // Famous JMP indirect bug: page wrap within the same page.
            let hi_addr = (ptr & 0xFF00) | ((ptr.wrapping_add(1)) & 0x00FF);
            let hi = bus.read(hi_addr);
            cpu.pc = u16::from_le_bytes([lo, hi]);
        }
        0x20 => {
            // JSR: fetch low, internal op, push PCH+PCL, fetch high.
            let lo = cpu.fetch_byte(bus);
            bus.read(0x0100 | cpu.sp as u16); // dummy read of stack
            cpu.push(bus, (cpu.pc >> 8) as u8);
            cpu.push(bus, (cpu.pc & 0xFF) as u8);
            let hi = cpu.fetch_byte(bus);
            cpu.pc = u16::from_le_bytes([lo, hi]);
        }
        0x60 => {
            // RTS
            bus.read(cpu.pc);
            bus.read(0x0100 | cpu.sp as u16); // dummy
            let lo = cpu.pop(bus);
            let hi = cpu.pop(bus);
            let ret = u16::from_le_bytes([lo, hi]);
            bus.read(ret); // internal op
            cpu.pc = ret.wrapping_add(1);
        }
        0x40 => {
            // RTI
            bus.read(cpu.pc);
            bus.read(0x0100 | cpu.sp as u16); // dummy
            let status = cpu.pop(bus);
            cpu.p = super::flags::StatusFlags::from_bits(status);
            let lo = cpu.pop(bus);
            let hi = cpu.pop(bus);
            cpu.pc = u16::from_le_bytes([lo, hi]);
        }

        // --- Branches ---
        0x10 => branch(cpu, bus, !cpu.p.negative()),
        0x30 => branch(cpu, bus, cpu.p.negative()),
        0x50 => branch(cpu, bus, !cpu.p.overflow()),
        0x70 => branch(cpu, bus, cpu.p.overflow()),
        0x90 => branch(cpu, bus, !cpu.p.carry()),
        0xB0 => branch(cpu, bus, cpu.p.carry()),
        0xD0 => branch(cpu, bus, !cpu.p.zero()),
        0xF0 => branch(cpu, bus, cpu.p.zero()),

        // --- Flag manipulation ---
        0x18 => {
            bus.read(cpu.pc);
            cpu.p.set_carry(false);
        }
        0x38 => {
            bus.read(cpu.pc);
            cpu.p.set_carry(true);
        }
        0x58 => {
            bus.read(cpu.pc);
            cpu.p.set_interrupt(false);
        }
        0x78 => {
            bus.read(cpu.pc);
            cpu.p.set_interrupt(true);
        }
        0xB8 => {
            bus.read(cpu.pc);
            cpu.p.set_overflow(false);
        }
        0xD8 => {
            bus.read(cpu.pc);
            cpu.p.set_decimal(false);
        }
        0xF8 => {
            bus.read(cpu.pc);
            cpu.p.set_decimal(true);
        }

        // --- BRK / NOP ---
        0x00 => {
            // BRK: one more byte of "signature" consumed; push PC+1 etc.
            let _sig = cpu.fetch_byte(bus);
            cpu.push(bus, (cpu.pc >> 8) as u8);
            cpu.push(bus, (cpu.pc & 0xFF) as u8);
            let status = cpu.p.to_u8() | 0x30; // B + U set on push
            cpu.push(bus, status);
            cpu.p.set_interrupt(true);
            // NMI hijack: if NMI was pending at end-of-push-P, redirect
            // vector to $FFFA. Pushed P keeps B=1 (BRK) — hijack only
            // retargets the vector. NMI latch consumed.
            let vector: u16 = if bus.prev_nmi_pending {
                bus.nmi_pending = false;
                0xFFFA
            } else {
                0xFFFE
            };
            let lo = bus.read(vector);
            let hi = bus.read(vector.wrapping_add(1));
            cpu.pc = u16::from_le_bytes([lo, hi]);
            // Suppress BRK's own poll at end-of-instruction. Any NMI
            // that arrived too late to hijack (cycles 6–7 of BRK) is
            // deferred to after the handler's first instruction, not
            // recognized immediately — matches Mesen2's explicit
            // `_prevNeedNmi = false` at end of BRK (NesCpu.cpp:238),
            // required by cpu_interrupts_v2/2-nmi_and_brk.
            bus.prev_nmi_pending = false;
        }
        0xEA => {
            bus.read(cpu.pc);
        }

        // ---------------- Unofficial opcodes ----------------
        // Implied NOP variants
        0x1A | 0x3A | 0x5A | 0x7A | 0xDA | 0xFA => {
            bus.read(cpu.pc);
        }
        // Immediate NOP variants (consume operand, no effect)
        0x80 | 0x82 | 0x89 | 0xC2 | 0xE2 => {
            cpu.fetch_byte(bus);
        }
        // Zero-page NOPs
        0x04 | 0x44 | 0x64 => {
            let a = addr_zp(cpu, bus);
            bus.read(a);
        }
        // Zero-page,X NOPs
        0x14 | 0x34 | 0x54 | 0x74 | 0xD4 | 0xF4 => {
            let a = addr_zp_x(cpu, bus);
            bus.read(a);
        }
        // Absolute NOP
        0x0C => {
            let a = addr_abs(cpu, bus);
            bus.read(a);
        }
        // Absolute,X NOPs (page-cross penalty via dummy read)
        0x1C | 0x3C | 0x5C | 0x7C | 0xDC | 0xFC => {
            let a = addr_abs_indexed_read(cpu, bus, cpu.x);
            bus.read(a);
        }
        // SBC immediate duplicate
        0xEB => {
            let v = cpu.fetch_byte(bus);
            sbc(cpu, v);
        }

        // LAX — load A and X together (no immediate — $AB is the unstable ANE)
        0xA7 => {
            let a = addr_zp(cpu, bus);
            let v = bus.read(a);
            cpu.a = v;
            cpu.x = v;
            cpu.set_zn(v);
        }
        0xB7 => {
            let a = addr_zp_y(cpu, bus);
            let v = bus.read(a);
            cpu.a = v;
            cpu.x = v;
            cpu.set_zn(v);
        }
        0xAF => {
            let a = addr_abs(cpu, bus);
            let v = bus.read(a);
            cpu.a = v;
            cpu.x = v;
            cpu.set_zn(v);
        }
        0xBF => {
            let a = addr_abs_indexed_read(cpu, bus, cpu.y);
            let v = bus.read(a);
            cpu.a = v;
            cpu.x = v;
            cpu.set_zn(v);
        }
        0xA3 => {
            let a = addr_ind_x(cpu, bus);
            let v = bus.read(a);
            cpu.a = v;
            cpu.x = v;
            cpu.set_zn(v);
        }
        0xB3 => {
            let a = addr_ind_y_read(cpu, bus);
            let v = bus.read(a);
            cpu.a = v;
            cpu.x = v;
            cpu.set_zn(v);
        }

        // SAX — store A & X (flags untouched)
        0x87 => {
            let a = addr_zp(cpu, bus);
            bus.write(a, cpu.a & cpu.x);
        }
        0x97 => {
            let a = addr_zp_y(cpu, bus);
            bus.write(a, cpu.a & cpu.x);
        }
        0x8F => {
            let a = addr_abs(cpu, bus);
            bus.write(a, cpu.a & cpu.x);
        }
        0x83 => {
            let a = addr_ind_x(cpu, bus);
            bus.write(a, cpu.a & cpu.x);
        }

        // SLO — ASL + ORA
        0x07 => rmw_zp_combine(cpu, bus, asl_value, ora_combine),
        0x17 => rmw_zp_x_combine(cpu, bus, asl_value, ora_combine),
        0x0F => rmw_abs_combine(cpu, bus, asl_value, ora_combine),
        0x1F => rmw_abs_x_combine(cpu, bus, asl_value, ora_combine),
        0x1B => rmw_abs_y_combine(cpu, bus, asl_value, ora_combine),
        0x03 => rmw_ind_x_combine(cpu, bus, asl_value, ora_combine),
        0x13 => rmw_ind_y_combine(cpu, bus, asl_value, ora_combine),

        // RLA — ROL + AND
        0x27 => rmw_zp_combine(cpu, bus, rol_value, and_combine),
        0x37 => rmw_zp_x_combine(cpu, bus, rol_value, and_combine),
        0x2F => rmw_abs_combine(cpu, bus, rol_value, and_combine),
        0x3F => rmw_abs_x_combine(cpu, bus, rol_value, and_combine),
        0x3B => rmw_abs_y_combine(cpu, bus, rol_value, and_combine),
        0x23 => rmw_ind_x_combine(cpu, bus, rol_value, and_combine),
        0x33 => rmw_ind_y_combine(cpu, bus, rol_value, and_combine),

        // SRE — LSR + EOR
        0x47 => rmw_zp_combine(cpu, bus, lsr_value, eor_combine),
        0x57 => rmw_zp_x_combine(cpu, bus, lsr_value, eor_combine),
        0x4F => rmw_abs_combine(cpu, bus, lsr_value, eor_combine),
        0x5F => rmw_abs_x_combine(cpu, bus, lsr_value, eor_combine),
        0x5B => rmw_abs_y_combine(cpu, bus, lsr_value, eor_combine),
        0x43 => rmw_ind_x_combine(cpu, bus, lsr_value, eor_combine),
        0x53 => rmw_ind_y_combine(cpu, bus, lsr_value, eor_combine),

        // RRA — ROR + ADC
        0x67 => rmw_zp_combine(cpu, bus, ror_value, adc_combine),
        0x77 => rmw_zp_x_combine(cpu, bus, ror_value, adc_combine),
        0x6F => rmw_abs_combine(cpu, bus, ror_value, adc_combine),
        0x7F => rmw_abs_x_combine(cpu, bus, ror_value, adc_combine),
        0x7B => rmw_abs_y_combine(cpu, bus, ror_value, adc_combine),
        0x63 => rmw_ind_x_combine(cpu, bus, ror_value, adc_combine),
        0x73 => rmw_ind_y_combine(cpu, bus, ror_value, adc_combine),

        // DCP — DEC + CMP (uses A)
        0xC7 => rmw_zp_combine(cpu, bus, dec_value, cmp_a_combine),
        0xD7 => rmw_zp_x_combine(cpu, bus, dec_value, cmp_a_combine),
        0xCF => rmw_abs_combine(cpu, bus, dec_value, cmp_a_combine),
        0xDF => rmw_abs_x_combine(cpu, bus, dec_value, cmp_a_combine),
        0xDB => rmw_abs_y_combine(cpu, bus, dec_value, cmp_a_combine),
        0xC3 => rmw_ind_x_combine(cpu, bus, dec_value, cmp_a_combine),
        0xD3 => rmw_ind_y_combine(cpu, bus, dec_value, cmp_a_combine),

        // ISB / ISC — INC + SBC
        0xE7 => rmw_zp_combine(cpu, bus, inc_value, sbc_combine),
        0xF7 => rmw_zp_x_combine(cpu, bus, inc_value, sbc_combine),
        0xEF => rmw_abs_combine(cpu, bus, inc_value, sbc_combine),
        0xFF => rmw_abs_x_combine(cpu, bus, inc_value, sbc_combine),
        0xFB => rmw_abs_y_combine(cpu, bus, inc_value, sbc_combine),
        0xE3 => rmw_ind_x_combine(cpu, bus, inc_value, sbc_combine),
        0xF3 => rmw_ind_y_combine(cpu, bus, inc_value, sbc_combine),

        // ALR — AND #imm then LSR A
        0x4B => {
            let v = cpu.fetch_byte(bus);
            cpu.a &= v;
            cpu.a = lsr_value(cpu, cpu.a);
        }
        // ANC — AND #imm, copies bit 7 into carry
        0x0B | 0x2B => {
            let v = cpu.fetch_byte(bus);
            cpu.a &= v;
            cpu.set_zn(cpu.a);
            cpu.p.set_carry((cpu.a & 0x80) != 0);
        }
        // ARR — AND #imm, then ROR A, with custom C/V from the result
        0x6B => {
            let v = cpu.fetch_byte(bus);
            cpu.a &= v;
            let carry_in = cpu.p.carry() as u8;
            let result = (cpu.a >> 1) | (carry_in << 7);
            cpu.a = result;
            cpu.set_zn(result);
            // Carry = bit 6; Overflow = bit 6 XOR bit 5.
            cpu.p.set_carry((result & 0x40) != 0);
            cpu.p
                .set_overflow(((result >> 6) ^ (result >> 5)) & 1 != 0);
        }
        // AXS / SBX — X = (A & X) - imm; sets C like CMP
        0xCB => {
            let v = cpu.fetch_byte(bus);
            let ax = cpu.a & cpu.x;
            let result = ax.wrapping_sub(v);
            cpu.p.set_carry(ax >= v);
            cpu.x = result;
            cpu.set_zn(result);
        }

        // LXA / ATX (opcode $AB). puNES, Nestopia, and Mesen2 all implement
        // this as a plain A = X = operand (no magic-constant ANE trick —
        // that's $8B). Blargg's test expects this deterministic form.
        0xAB => {
            let v = cpu.fetch_byte(bus);
            cpu.a = v;
            cpu.x = v;
            cpu.set_zn(v);
        }

        // ANE / XAA (opcode $8B). Unstable unofficial: real hardware does
        // `A = (A | magic) & X & operand`, where the `magic` constant
        // varies by chip / temperature and is usually $EE. puNES, Mesen2
        // and Nestopia all pick a fixed magic ($EE is the de-facto
        // standard across emulators) so test ROMs that bracket-check the
        // result still get a deterministic answer. Cycle cost = 2
        // (immediate). `instr_timing.nes` needs this one implemented —
        // its test harness executes every opcode; an unimplemented $8B
        // halts the CPU before any timing measurement runs.
        0x8B => {
            let v = cpu.fetch_byte(bus);
            let result = (cpu.a | 0xEE) & cpu.x & v;
            cpu.a = result;
            cpu.set_zn(result);
        }

        // SHY abs,X ($9C). value = Y & (base_hi + 1); on page cross the
        // store's high byte is ANDed with Y (see Nestopia NstCpu.cpp Shy /
        // puNES cpu.c SXX macro / Mesen2 SyaSxaAxa).
        0x9C => {
            let lo = cpu.fetch_byte(bus);
            let hi = cpu.fetch_byte(bus);
            let base = u16::from_le_bytes([lo, hi]);
            let effective = base.wrapping_add(cpu.x as u16);
            bus.read((base & 0xFF00) | (effective & 0x00FF));
            let value = cpu.y & hi.wrapping_add(1);
            let store_addr = if (base & 0xFF00) != (effective & 0xFF00) {
                effective & (((cpu.y as u16) << 8) | 0x00FF)
            } else {
                effective
            };
            bus.write(store_addr, value);
        }
        // SHX abs,Y ($9E). Same pattern, swap X/Y roles.
        0x9E => {
            let lo = cpu.fetch_byte(bus);
            let hi = cpu.fetch_byte(bus);
            let base = u16::from_le_bytes([lo, hi]);
            let effective = base.wrapping_add(cpu.y as u16);
            bus.read((base & 0xFF00) | (effective & 0x00FF));
            let value = cpu.x & hi.wrapping_add(1);
            let store_addr = if (base & 0xFF00) != (effective & 0xFF00) {
                effective & (((cpu.x as u16) << 8) | 0x00FF)
            } else {
                effective
            };
            bus.write(store_addr, value);
        }
        0x93 => {
            // AHX (ind),Y
            let ptr = cpu.fetch_byte(bus);
            let lo = bus.read(ptr as u16);
            let hi = bus.read(ptr.wrapping_add(1) as u16);
            let base = u16::from_le_bytes([lo, hi]);
            let effective = base.wrapping_add(cpu.y as u16);
            bus.read((base & 0xFF00) | (effective & 0x00FF));
            let value = cpu.a & cpu.x & hi.wrapping_add(1);
            bus.write(effective, value);
        }
        0x9F => {
            // AHX abs,Y
            let lo = cpu.fetch_byte(bus);
            let hi = cpu.fetch_byte(bus);
            let base = u16::from_le_bytes([lo, hi]);
            let effective = base.wrapping_add(cpu.y as u16);
            bus.read((base & 0xFF00) | (effective & 0x00FF));
            let value = cpu.a & cpu.x & hi.wrapping_add(1);
            bus.write(effective, value);
        }
        0x9B => {
            // TAS abs,Y: SP = A & X; then store SP & (high+1)
            let lo = cpu.fetch_byte(bus);
            let hi = cpu.fetch_byte(bus);
            let base = u16::from_le_bytes([lo, hi]);
            let effective = base.wrapping_add(cpu.y as u16);
            bus.read((base & 0xFF00) | (effective & 0x00FF));
            cpu.sp = cpu.a & cpu.x;
            let value = cpu.sp & hi.wrapping_add(1);
            bus.write(effective, value);
        }
        0xBB => {
            // LAS abs,Y: value = mem & SP; A=X=SP=value
            let a = addr_abs_indexed_read(cpu, bus, cpu.y);
            let v = bus.read(a);
            let result = v & cpu.sp;
            cpu.a = result;
            cpu.x = result;
            cpu.sp = result;
            cpu.set_zn(result);
        }

        // JAM / KIL / STP — halt the CPU. With all other 244 opcodes
        // explicitly handled above, this arm also makes the match
        // exhaustive on u8 — a missing future opcode would surface
        // as a non-exhaustive-match compile error rather than a
        // silent runtime halt.
        0x02 | 0x12 | 0x22 | 0x32 | 0x42 | 0x52 | 0x62 | 0x72 | 0x92 | 0xB2 | 0xD2 | 0xF2 => {
            return Err(format!(
                "CPU JAM: illegal opcode ${:02X} at PC=${:04X}",
                op,
                cpu.pc.wrapping_sub(1)
            ));
        }
    }
    Ok(())
}
