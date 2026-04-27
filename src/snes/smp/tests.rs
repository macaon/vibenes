// SPDX-License-Identifier: GPL-3.0-or-later
//! SPC700 unit tests. Exercises the foundation opcodes against a
//! [`FlatSmpBus`]: NOP, PSW manipulation, register transfers,
//! immediate / direct-page / absolute MOV, branches, stack push /
//! pop, INC / DEC of registers, JMP, RET. Cycle counts are checked
//! per-instruction so adding cycle-affecting addressing-mode quirks
//! later regresses loudly.

use super::bus::{FlatSmpBus, SmpBus};
use super::Smp;

/// Run a single instruction starting at `$0200`. Returns the SMP
/// cycles charged for that instruction (opcode fetch + everything
/// the handler did).
fn run_one(smp: &mut Smp, bus: &mut FlatSmpBus, program: &[u8]) -> u64 {
    bus.poke_slice(0x0200, program);
    smp.pc = 0x0200;
    let before = bus.cycles();
    smp.step(bus);
    bus.cycles() - before
}

#[test]
fn reset_loads_pc_from_fffe_fffd() {
    let mut bus = FlatSmpBus::new();
    bus.poke(0xFFFE, 0xC0);
    bus.poke(0xFFFF, 0xFF);
    let mut smp = Smp::new();
    smp.reset(&mut bus);
    assert_eq!(smp.pc, 0xFFC0);
    assert_eq!(smp.sp, 0xFF);
    assert_eq!(smp.psw, super::Status::default());
}

#[test]
fn nop_is_two_cycles_and_advances_pc() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    let cycles = run_one(&mut smp, &mut bus, &[0x00]);
    assert_eq!(cycles, 2);
    assert_eq!(smp.pc, 0x0201);
}

// ----- PSW manipulation -------------------------------------------------

#[test]
fn clrc_clears_carry_in_two_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.psw.c = true;
    let cycles = run_one(&mut smp, &mut bus, &[0x60]);
    assert_eq!(cycles, 2);
    assert!(!smp.psw.c);
}

#[test]
fn setc_sets_carry_in_two_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    let cycles = run_one(&mut smp, &mut bus, &[0x80]);
    assert_eq!(cycles, 2);
    assert!(smp.psw.c);
}

#[test]
fn notc_flips_carry_in_three_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.psw.c = false;
    let cycles = run_one(&mut smp, &mut bus, &[0xED]);
    assert_eq!(cycles, 3);
    assert!(smp.psw.c);
    let cycles2 = run_one(&mut smp, &mut bus, &[0xED]);
    assert_eq!(cycles2, 3);
    assert!(!smp.psw.c);
}

#[test]
fn clrp_setp_toggle_direct_page() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    let cycles = run_one(&mut smp, &mut bus, &[0x40]);
    assert_eq!(cycles, 2);
    assert!(smp.psw.p);
    let cycles2 = run_one(&mut smp, &mut bus, &[0x20]);
    assert_eq!(cycles2, 2);
    assert!(!smp.psw.p);
}

#[test]
fn clrv_clears_v_and_h() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.psw.v = true;
    smp.psw.h = true;
    let cycles = run_one(&mut smp, &mut bus, &[0xE0]);
    assert_eq!(cycles, 2);
    assert!(!smp.psw.v);
    assert!(!smp.psw.h);
}

#[test]
fn ei_di_toggle_interrupt_flag() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    let cycles = run_one(&mut smp, &mut bus, &[0xA0]);
    assert_eq!(cycles, 3);
    assert!(smp.psw.i);
    let cycles2 = run_one(&mut smp, &mut bus, &[0xC0]);
    assert_eq!(cycles2, 3);
    assert!(!smp.psw.i);
}

// ----- Register transfers ----------------------------------------------

#[test]
fn mov_a_x_copies_x_and_updates_nz() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.x = 0x80;
    let cycles = run_one(&mut smp, &mut bus, &[0x7D]);
    assert_eq!(cycles, 2);
    assert_eq!(smp.a, 0x80);
    assert!(smp.psw.n);
    assert!(!smp.psw.z);
}

#[test]
fn mov_a_y_zero_sets_z_clears_n() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0xFF;
    smp.y = 0x00;
    let cycles = run_one(&mut smp, &mut bus, &[0xDD]);
    assert_eq!(cycles, 2);
    assert_eq!(smp.a, 0x00);
    assert!(!smp.psw.n);
    assert!(smp.psw.z);
}

#[test]
fn mov_x_a_copies_a() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x42;
    let cycles = run_one(&mut smp, &mut bus, &[0x5D]);
    assert_eq!(cycles, 2);
    assert_eq!(smp.x, 0x42);
}

#[test]
fn mov_sp_x_does_not_set_flags() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.x = 0x00;
    smp.psw.z = false;
    smp.psw.n = false;
    let cycles = run_one(&mut smp, &mut bus, &[0xBD]);
    assert_eq!(cycles, 2);
    assert_eq!(smp.sp, 0x00);
    // Z would have been set by an N/Z-updating MOV - confirm it
    // stayed clear (this is the documented SPC700 quirk).
    assert!(!smp.psw.z);
    assert!(!smp.psw.n);
}

#[test]
fn mov_x_sp_does_set_flags() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.sp = 0x00;
    let cycles = run_one(&mut smp, &mut bus, &[0x9D]);
    assert_eq!(cycles, 2);
    assert_eq!(smp.x, 0x00);
    assert!(smp.psw.z);
}

// ----- INC / DEC of registers -----------------------------------------

#[test]
fn inc_a_wraps_and_sets_z() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0xFF;
    let cycles = run_one(&mut smp, &mut bus, &[0xBC]);
    assert_eq!(cycles, 2);
    assert_eq!(smp.a, 0x00);
    assert!(smp.psw.z);
    assert!(!smp.psw.n);
}

#[test]
fn dec_a_underflows_and_sets_n() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x00;
    let cycles = run_one(&mut smp, &mut bus, &[0x9C]);
    assert_eq!(cycles, 2);
    assert_eq!(smp.a, 0xFF);
    assert!(smp.psw.n);
    assert!(!smp.psw.z);
}

#[test]
fn inc_x_dec_x_inc_y_dec_y_each_take_two_cycles() {
    for opcode in [0x3D, 0x1D, 0xFC, 0xDC] {
        let mut bus = FlatSmpBus::new();
        let mut smp = Smp::new();
        let cycles = run_one(&mut smp, &mut bus, &[opcode]);
        assert_eq!(cycles, 2, "opcode ${:02X} should be 2 cycles", opcode);
    }
}

// ----- Immediate loads -------------------------------------------------

#[test]
fn mov_a_imm_loads_value_in_two_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    let cycles = run_one(&mut smp, &mut bus, &[0xE8, 0x42]);
    assert_eq!(cycles, 2);
    assert_eq!(smp.a, 0x42);
    assert!(!smp.psw.z);
    assert!(!smp.psw.n);
    assert_eq!(smp.pc, 0x0202);
}

#[test]
fn mov_x_imm_and_mov_y_imm() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    run_one(&mut smp, &mut bus, &[0xCD, 0x80]);
    assert_eq!(smp.x, 0x80);
    assert!(smp.psw.n);
    let mut smp2 = Smp::new();
    let mut bus2 = FlatSmpBus::new();
    run_one(&mut smp2, &mut bus2, &[0x8D, 0x00]);
    assert_eq!(smp2.y, 0x00);
    assert!(smp2.psw.z);
}

// ----- Direct-page loads/stores ---------------------------------------

#[test]
fn mov_a_dp_reads_page_zero_under_clrp() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    bus.poke(0x0010, 0x77);
    let cycles = run_one(&mut smp, &mut bus, &[0xE4, 0x10]);
    assert_eq!(cycles, 3);
    assert_eq!(smp.a, 0x77);
}

#[test]
fn mov_a_dp_reads_page_one_under_setp() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.psw.p = true;
    bus.poke(0x0110, 0xAB);
    let cycles = run_one(&mut smp, &mut bus, &[0xE4, 0x10]);
    assert_eq!(cycles, 3);
    assert_eq!(smp.a, 0xAB);
}

#[test]
fn mov_dp_a_writes_with_dummy_read_in_four_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x55;
    let cycles = run_one(&mut smp, &mut bus, &[0xC4, 0x10]);
    assert_eq!(cycles, 4);
    assert_eq!(bus.peek(0x0010), 0x55);
}

#[test]
fn mov_dp_x_y_pairs() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.x = 0x33;
    smp.y = 0x44;
    run_one(&mut smp, &mut bus, &[0xD8, 0x20]);
    assert_eq!(bus.peek(0x0020), 0x33);
    run_one(&mut smp, &mut bus, &[0xCB, 0x21]);
    assert_eq!(bus.peek(0x0021), 0x44);
}

// ----- Absolute loads/stores ------------------------------------------

#[test]
fn mov_a_abs_reads_full_16bit_address_in_four_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    bus.poke(0x1234, 0x99);
    let cycles = run_one(&mut smp, &mut bus, &[0xE5, 0x34, 0x12]);
    assert_eq!(cycles, 4);
    assert_eq!(smp.a, 0x99);
}

#[test]
fn mov_abs_a_writes_with_dummy_read_in_five_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0xAA;
    let cycles = run_one(&mut smp, &mut bus, &[0xC5, 0x34, 0x12]);
    assert_eq!(cycles, 5);
    assert_eq!(bus.peek(0x1234), 0xAA);
}

#[test]
fn mov_x_abs_and_mov_y_abs_load_with_flags() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    bus.poke(0x2000, 0x80);
    run_one(&mut smp, &mut bus, &[0xE9, 0x00, 0x20]);
    assert_eq!(smp.x, 0x80);
    assert!(smp.psw.n);
    bus.poke(0x2001, 0x00);
    run_one(&mut smp, &mut bus, &[0xEC, 0x01, 0x20]);
    assert_eq!(smp.y, 0x00);
    assert!(smp.psw.z);
}

// ----- Stack ops -------------------------------------------------------

#[test]
fn push_a_pop_a_round_trip() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0xCD;
    let push_cycles = run_one(&mut smp, &mut bus, &[0x2D]);
    assert_eq!(push_cycles, 4);
    assert_eq!(smp.sp, 0xFE);
    assert_eq!(bus.peek(0x01FF), 0xCD);

    smp.a = 0x00;
    let pop_cycles = run_one(&mut smp, &mut bus, &[0xAE]);
    assert_eq!(pop_cycles, 4);
    assert_eq!(smp.a, 0xCD);
    assert_eq!(smp.sp, 0xFF);
}

#[test]
fn push_psw_pop_psw_preserves_byte() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.psw.n = true;
    smp.psw.v = true;
    smp.psw.c = true;
    let pre = smp.psw.pack();
    run_one(&mut smp, &mut bus, &[0x0D]);
    assert_eq!(bus.peek(0x01FF), pre);
    smp.psw = super::Status::default();
    run_one(&mut smp, &mut bus, &[0x8E]);
    assert_eq!(smp.psw.pack(), pre);
}

#[test]
fn pop_a_does_not_alter_psw() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    bus.poke(0x01FF, 0x00);
    smp.sp = 0xFE;
    smp.psw.z = false;
    smp.psw.n = false;
    run_one(&mut smp, &mut bus, &[0xAE]);
    assert_eq!(smp.a, 0x00);
    assert!(!smp.psw.z, "POP A must not set Z even when popping zero");
}

// ----- Branches --------------------------------------------------------

#[test]
fn bra_takes_four_cycles_and_jumps_signed() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    // BRA +5 from PC=$0200; after operand fetch PC=$0202, target=$0207.
    let cycles = run_one(&mut smp, &mut bus, &[0x2F, 0x05]);
    assert_eq!(cycles, 4);
    assert_eq!(smp.pc, 0x0207);
}

#[test]
fn beq_taken_vs_not_taken_cycle_count() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.psw.z = true;
    let taken = run_one(&mut smp, &mut bus, &[0xF0, 0x05]);
    assert_eq!(taken, 4);
    assert_eq!(smp.pc, 0x0207);

    let mut smp2 = Smp::new();
    let mut bus2 = FlatSmpBus::new();
    smp2.psw.z = false;
    let not_taken = run_one(&mut smp2, &mut bus2, &[0xF0, 0x05]);
    assert_eq!(not_taken, 2);
    assert_eq!(smp2.pc, 0x0202);
}

#[test]
fn branch_offset_is_signed() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    // BNE -4 from PC=$0200; after operand fetch PC=$0202; target=$01FE.
    smp.psw.z = false;
    run_one(&mut smp, &mut bus, &[0xD0, 0xFC]);
    assert_eq!(smp.pc, 0x01FE);
}

// ----- JMP / RET -------------------------------------------------------

#[test]
fn jmp_abs_loads_pc_in_three_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    let cycles = run_one(&mut smp, &mut bus, &[0x5F, 0x34, 0x12]);
    assert_eq!(cycles, 3);
    assert_eq!(smp.pc, 0x1234);
}

#[test]
fn ret_pops_pc_in_five_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    bus.poke(0x01FE, 0x34); // low byte (popped first)
    bus.poke(0x01FF, 0x12); // high byte (popped second)
    smp.sp = 0xFD;
    let cycles = run_one(&mut smp, &mut bus, &[0x6F]);
    assert_eq!(cycles, 5);
    assert_eq!(smp.pc, 0x1234);
    assert_eq!(smp.sp, 0xFF);
}

// ----- Halts -----------------------------------------------------------

#[test]
fn sleep_parks_smp_until_unstuck() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    let cycles = run_one(&mut smp, &mut bus, &[0xEF]);
    assert_eq!(cycles, 3);
    assert!(smp.sleeping);
    // Subsequent steps should be no-ops that just charge an idle.
    let before = bus.cycles();
    smp.step(&mut bus);
    assert_eq!(bus.cycles() - before, 1);
    assert!(smp.sleeping);
}

#[test]
fn stop_halts_smp_permanently() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    let cycles = run_one(&mut smp, &mut bus, &[0xFF]);
    assert_eq!(cycles, 3);
    assert!(smp.stopped);
}

// ----- ADC family ------------------------------------------------------
//
// Flag semantics are verified once on `adc A,#imm` (the simplest
// addressing mode) and the remaining 11 modes get a single round-
// trip test each to lock in cycle count + correct effective-address
// resolution. The byte-level helper does the math; the per-mode
// tests exist to catch addressing bugs.

#[test]
fn adc_imm_no_carry_zero_result_sets_zero_clears_n_v_h() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x00;
    smp.psw.c = false;
    let cycles = run_one(&mut smp, &mut bus, &[0x88, 0x00]);
    assert_eq!(cycles, 2);
    assert_eq!(smp.a, 0x00);
    assert!(smp.psw.z);
    assert!(!smp.psw.n);
    assert!(!smp.psw.c);
    assert!(!smp.psw.v);
    assert!(!smp.psw.h);
}

#[test]
fn adc_imm_carry_in_propagates() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x00;
    smp.psw.c = true;
    run_one(&mut smp, &mut bus, &[0x88, 0x00]);
    assert_eq!(smp.a, 0x01);
    assert!(!smp.psw.z);
    assert!(!smp.psw.c);
}

#[test]
fn adc_imm_unsigned_overflow_sets_carry() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0xFF;
    smp.psw.c = false;
    run_one(&mut smp, &mut bus, &[0x88, 0x01]);
    assert_eq!(smp.a, 0x00);
    assert!(smp.psw.z);
    assert!(smp.psw.c);
    assert!(!smp.psw.n);
    // V is signed-overflow; 0xFF (-1) + 0x01 (+1) = 0 has no signed
    // overflow.
    assert!(!smp.psw.v);
}

#[test]
fn adc_imm_signed_overflow_sets_v() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    // 0x7F + 0x01 = 0x80: positive + positive -> negative is V.
    smp.a = 0x7F;
    smp.psw.c = false;
    run_one(&mut smp, &mut bus, &[0x88, 0x01]);
    assert_eq!(smp.a, 0x80);
    assert!(smp.psw.n);
    assert!(smp.psw.v);
    assert!(!smp.psw.c);
}

#[test]
fn adc_imm_signed_overflow_negative_to_positive() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    // 0x80 + 0x80 = 0x100: negative + negative -> positive (with
    // carry out). V is set; C is set; result is 0.
    smp.a = 0x80;
    smp.psw.c = false;
    run_one(&mut smp, &mut bus, &[0x88, 0x80]);
    assert_eq!(smp.a, 0x00);
    assert!(smp.psw.v);
    assert!(smp.psw.c);
    assert!(smp.psw.z);
}

#[test]
fn adc_imm_half_carry_when_low_nibble_overflows() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x0F;
    smp.psw.c = false;
    run_one(&mut smp, &mut bus, &[0x88, 0x01]);
    assert_eq!(smp.a, 0x10);
    assert!(smp.psw.h, "low-nibble carry must set H");
}

#[test]
fn adc_imm_no_half_carry_when_only_top_nibble_carries() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x10;
    smp.psw.c = false;
    run_one(&mut smp, &mut bus, &[0x88, 0x10]);
    assert_eq!(smp.a, 0x20);
    assert!(!smp.psw.h);
}

#[test]
fn adc_a_dp_three_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x10;
    bus.poke(0x0020, 0x05);
    let cycles = run_one(&mut smp, &mut bus, &[0x84, 0x20]);
    assert_eq!(cycles, 3);
    assert_eq!(smp.a, 0x15);
}

#[test]
fn adc_a_abs_four_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x40;
    bus.poke(0x1234, 0x02);
    let cycles = run_one(&mut smp, &mut bus, &[0x85, 0x34, 0x12]);
    assert_eq!(cycles, 4);
    assert_eq!(smp.a, 0x42);
}

#[test]
fn adc_a_dp_x_direct_three_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x01;
    smp.x = 0x10;
    bus.poke(0x0010, 0x07);
    let cycles = run_one(&mut smp, &mut bus, &[0x86]);
    assert_eq!(cycles, 3);
    assert_eq!(smp.a, 0x08);
}

#[test]
fn adc_a_dp_plus_x_four_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x10;
    smp.x = 0x05;
    // dp=$20, X=$05 -> read $0025
    bus.poke(0x0025, 0x03);
    let cycles = run_one(&mut smp, &mut bus, &[0x94, 0x20]);
    assert_eq!(cycles, 4);
    assert_eq!(smp.a, 0x13);
}

#[test]
fn adc_a_abs_plus_x_five_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x40;
    smp.x = 0x10;
    bus.poke(0x1244, 0x02);
    let cycles = run_one(&mut smp, &mut bus, &[0x95, 0x34, 0x12]);
    assert_eq!(cycles, 5);
    assert_eq!(smp.a, 0x42);
}

#[test]
fn adc_a_abs_plus_y_five_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x40;
    smp.y = 0x10;
    bus.poke(0x1244, 0x02);
    let cycles = run_one(&mut smp, &mut bus, &[0x96, 0x34, 0x12]);
    assert_eq!(cycles, 5);
    assert_eq!(smp.a, 0x42);
}

#[test]
fn adc_a_dp_x_indirect_six_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x10;
    smp.x = 0x02;
    // [dp+X] mode: dp=$20, X=$02 -> ptr at $0022/$0023
    bus.poke(0x0022, 0x00);
    bus.poke(0x0023, 0x12); // ptr -> $1200
    bus.poke(0x1200, 0x05);
    let cycles = run_one(&mut smp, &mut bus, &[0x87, 0x20]);
    assert_eq!(cycles, 6);
    assert_eq!(smp.a, 0x15);
}

#[test]
fn adc_a_dp_indirect_plus_y_six_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x10;
    smp.y = 0x05;
    // [dp]+Y mode: dp=$20 -> ptr at $0020/$0021, then +Y
    bus.poke(0x0020, 0x00);
    bus.poke(0x0021, 0x12); // ptr -> $1200; +5 = $1205
    bus.poke(0x1205, 0x03);
    let cycles = run_one(&mut smp, &mut bus, &[0x97, 0x20]);
    assert_eq!(cycles, 6);
    assert_eq!(smp.a, 0x13);
}

#[test]
fn adc_x_y_indirect_writes_to_x_address_five_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0xCC;
    smp.x = 0x10;
    smp.y = 0x20;
    bus.poke(0x0010, 0x40); // (X)
    bus.poke(0x0020, 0x02); // (Y)
    let cycles = run_one(&mut smp, &mut bus, &[0x99]);
    assert_eq!(cycles, 5);
    assert_eq!(bus.peek(0x0010), 0x42);
    // A is NOT touched by the memory-destination ADC.
    assert_eq!(smp.a, 0xCC);
}

#[test]
fn adc_dp_dp_six_cycles_src_first_in_stream() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    bus.poke(0x0010, 0x40); // src
    bus.poke(0x0020, 0x02); // dst (and result destination)
    // Stream: opcode src_dp dst_dp -> ADC $20, $10 in mnemonic.
    let cycles = run_one(&mut smp, &mut bus, &[0x89, 0x10, 0x20]);
    assert_eq!(cycles, 6);
    assert_eq!(bus.peek(0x0020), 0x42);
}

#[test]
fn adc_dp_imm_five_cycles_imm_first_in_stream() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    bus.poke(0x0020, 0x40);
    // Stream: opcode imm dp -> ADC $20, #$02 in mnemonic.
    let cycles = run_one(&mut smp, &mut bus, &[0x98, 0x02, 0x20]);
    assert_eq!(cycles, 5);
    assert_eq!(bus.peek(0x0020), 0x42);
}

// ----- IPL ROM smoke test ---------------------------------------------

#[test]
fn ipl_first_instruction_is_mov_x_imm_ef() {
    use super::ipl::IPL_ROM;
    // First two IPL bytes are `CD EF` = MOV X, #$EF. That's the
    // first instruction the SMP runs after reset on real hardware.
    assert_eq!(IPL_ROM[0], 0xCD);
    assert_eq!(IPL_ROM[1], 0xEF);

    // Run that instruction directly through the dispatcher to
    // confirm the foundation can execute IPL bytes verbatim.
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    let cycles = run_one(&mut smp, &mut bus, &[IPL_ROM[0], IPL_ROM[1]]);
    assert_eq!(cycles, 2);
    assert_eq!(smp.x, 0xEF);
    assert!(smp.psw.n);
}
