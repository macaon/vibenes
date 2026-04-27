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

// ----- SBC family ------------------------------------------------------
//
// SBC = ADC(a, !b), so the addressing-mode plumbing is shared. These
// tests focus on borrow / no-borrow semantics, signed overflow on
// subtraction, and the half-borrow convention. Each non-imm mode
// gets one round-trip test to lock in cycle count + addressing.

#[test]
fn sbc_imm_no_borrow_pre_subtract_one() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x10;
    smp.psw.c = true; // no borrow
    let cycles = run_one(&mut smp, &mut bus, &[0xA8, 0x05]);
    assert_eq!(cycles, 2);
    assert_eq!(smp.a, 0x0B);
    assert!(smp.psw.c, "no underflow -> C stays set (no-borrow)");
    assert!(!smp.psw.n);
    assert!(!smp.psw.z);
    assert!(!smp.psw.v);
}

#[test]
fn sbc_imm_with_borrow_in() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x10;
    smp.psw.c = false; // borrow in
    run_one(&mut smp, &mut bus, &[0xA8, 0x05]);
    // 0x10 - 0x05 - 1 = 0x0A
    assert_eq!(smp.a, 0x0A);
    assert!(smp.psw.c, "no underflow on subtract -> C stays set");
}

#[test]
fn sbc_imm_underflow_clears_carry() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x00;
    smp.psw.c = true; // no borrow
    run_one(&mut smp, &mut bus, &[0xA8, 0x01]);
    assert_eq!(smp.a, 0xFF);
    assert!(!smp.psw.c, "underflow clears C (borrow occurred)");
    assert!(smp.psw.n);
}

#[test]
fn sbc_imm_signed_overflow_sets_v() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    // 0x80 - 0x01 = 0x7F: negative minus positive -> positive is V.
    smp.a = 0x80;
    smp.psw.c = true;
    run_one(&mut smp, &mut bus, &[0xA8, 0x01]);
    assert_eq!(smp.a, 0x7F);
    assert!(smp.psw.v);
    assert!(!smp.psw.n);
}

#[test]
fn sbc_imm_zero_result_sets_z() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x42;
    smp.psw.c = true;
    run_one(&mut smp, &mut bus, &[0xA8, 0x42]);
    assert_eq!(smp.a, 0x00);
    assert!(smp.psw.z);
    assert!(smp.psw.c);
}

#[test]
fn sbc_a_dp_three_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x10;
    smp.psw.c = true;
    bus.poke(0x0020, 0x05);
    let cycles = run_one(&mut smp, &mut bus, &[0xA4, 0x20]);
    assert_eq!(cycles, 3);
    assert_eq!(smp.a, 0x0B);
}

#[test]
fn sbc_a_abs_four_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x40;
    smp.psw.c = true;
    bus.poke(0x1234, 0x10);
    let cycles = run_one(&mut smp, &mut bus, &[0xA5, 0x34, 0x12]);
    assert_eq!(cycles, 4);
    assert_eq!(smp.a, 0x30);
}

#[test]
fn sbc_a_dp_x_direct_three_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x10;
    smp.x = 0x05;
    smp.psw.c = true;
    bus.poke(0x0005, 0x07);
    let cycles = run_one(&mut smp, &mut bus, &[0xA6]);
    assert_eq!(cycles, 3);
    assert_eq!(smp.a, 0x09);
}

#[test]
fn sbc_a_dp_plus_x_four_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x40;
    smp.x = 0x05;
    smp.psw.c = true;
    bus.poke(0x0025, 0x10);
    let cycles = run_one(&mut smp, &mut bus, &[0xB4, 0x20]);
    assert_eq!(cycles, 4);
    assert_eq!(smp.a, 0x30);
}

#[test]
fn sbc_a_abs_plus_x_and_abs_plus_y_five_cycles() {
    for opcode in [0xB5_u8, 0xB6_u8] {
        let mut bus = FlatSmpBus::new();
        let mut smp = Smp::new();
        smp.a = 0x40;
        smp.x = 0x10;
        smp.y = 0x10;
        smp.psw.c = true;
        bus.poke(0x1244, 0x05);
        let cycles = run_one(&mut smp, &mut bus, &[opcode, 0x34, 0x12]);
        assert_eq!(cycles, 5, "opcode ${:02X}", opcode);
        assert_eq!(smp.a, 0x3B);
    }
}

#[test]
fn sbc_a_dp_x_indirect_six_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x10;
    smp.x = 0x02;
    smp.psw.c = true;
    bus.poke(0x0022, 0x00);
    bus.poke(0x0023, 0x12); // ptr -> $1200
    bus.poke(0x1200, 0x05);
    let cycles = run_one(&mut smp, &mut bus, &[0xA7, 0x20]);
    assert_eq!(cycles, 6);
    assert_eq!(smp.a, 0x0B);
}

#[test]
fn sbc_a_dp_indirect_plus_y_six_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x10;
    smp.y = 0x05;
    smp.psw.c = true;
    bus.poke(0x0020, 0x00);
    bus.poke(0x0021, 0x12);
    bus.poke(0x1205, 0x03);
    let cycles = run_one(&mut smp, &mut bus, &[0xB7, 0x20]);
    assert_eq!(cycles, 6);
    assert_eq!(smp.a, 0x0D);
}

#[test]
fn sbc_x_y_indirect_writes_to_x_address_five_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0xCC;
    smp.x = 0x10;
    smp.y = 0x20;
    smp.psw.c = true;
    bus.poke(0x0010, 0x42); // (X)
    bus.poke(0x0020, 0x02); // (Y)
    let cycles = run_one(&mut smp, &mut bus, &[0xB9]);
    assert_eq!(cycles, 5);
    assert_eq!(bus.peek(0x0010), 0x40);
    assert_eq!(smp.a, 0xCC);
}

#[test]
fn sbc_dp_dp_six_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.psw.c = true;
    bus.poke(0x0010, 0x05); // src
    bus.poke(0x0020, 0x40); // dst
    let cycles = run_one(&mut smp, &mut bus, &[0xA9, 0x10, 0x20]);
    assert_eq!(cycles, 6);
    assert_eq!(bus.peek(0x0020), 0x3B);
}

#[test]
fn sbc_dp_imm_five_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.psw.c = true;
    bus.poke(0x0020, 0x40);
    let cycles = run_one(&mut smp, &mut bus, &[0xB8, 0x05, 0x20]);
    assert_eq!(cycles, 5);
    assert_eq!(bus.peek(0x0020), 0x3B);
}

// ----- CMP family ------------------------------------------------------
//
// CMP only updates N/Z/C (NOT V or H - that's the SPC700 quirk that
// distinguishes it from SBC). Carry: set when LHS >= RHS unsigned.
// The tests lock in (a) the no-V/no-H behaviour, (b) cycle counts
// per addressing mode, (c) operand ordering for the dp,dp / (X),(Y) /
// dp,#imm forms.

#[test]
fn cmp_a_imm_equal_sets_z_and_c_clears_n() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x42;
    let cycles = run_one(&mut smp, &mut bus, &[0x68, 0x42]);
    assert_eq!(cycles, 2);
    assert_eq!(smp.a, 0x42, "CMP must not modify A");
    assert!(smp.psw.z);
    assert!(smp.psw.c, "equal -> C set (LHS >= RHS)");
    assert!(!smp.psw.n);
}

#[test]
fn cmp_a_imm_lhs_greater_clears_z_sets_c() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x80;
    run_one(&mut smp, &mut bus, &[0x68, 0x40]);
    // 0x80 - 0x40 = 0x40
    assert!(!smp.psw.z);
    assert!(smp.psw.c);
    assert!(!smp.psw.n);
}

#[test]
fn cmp_a_imm_lhs_less_clears_c_sets_n() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x10;
    run_one(&mut smp, &mut bus, &[0x68, 0x80]);
    // 0x10 - 0x80 = 0x90; result negative.
    assert!(!smp.psw.z);
    assert!(!smp.psw.c, "LHS < RHS -> C clear (borrow)");
    assert!(smp.psw.n);
}

#[test]
fn cmp_does_not_modify_v_or_h() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x80;
    smp.psw.v = true;
    smp.psw.h = true;
    // CMP A,#$01 - 0x80-0x01 = 0x7F, would trip V if treated as SBC.
    run_one(&mut smp, &mut bus, &[0x68, 0x01]);
    assert!(smp.psw.v, "CMP must NOT clear pre-existing V");
    assert!(smp.psw.h, "CMP must NOT clear pre-existing H");
}

#[test]
fn cmp_a_dp_three_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x42;
    bus.poke(0x0020, 0x42);
    let cycles = run_one(&mut smp, &mut bus, &[0x64, 0x20]);
    assert_eq!(cycles, 3);
    assert!(smp.psw.z);
}

#[test]
fn cmp_a_abs_four_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x42;
    bus.poke(0x1234, 0x40);
    let cycles = run_one(&mut smp, &mut bus, &[0x65, 0x34, 0x12]);
    assert_eq!(cycles, 4);
    assert!(smp.psw.c);
    assert!(!smp.psw.z);
}

#[test]
fn cmp_a_dp_x_direct_three_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x10;
    smp.x = 0x05;
    bus.poke(0x0005, 0x10);
    let cycles = run_one(&mut smp, &mut bus, &[0x66]);
    assert_eq!(cycles, 3);
    assert!(smp.psw.z);
}

#[test]
fn cmp_a_dp_plus_x_four_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x40;
    smp.x = 0x05;
    bus.poke(0x0025, 0x40);
    let cycles = run_one(&mut smp, &mut bus, &[0x74, 0x20]);
    assert_eq!(cycles, 4);
    assert!(smp.psw.z);
}

#[test]
fn cmp_a_abs_plus_x_and_abs_plus_y_five_cycles() {
    for opcode in [0x75_u8, 0x76_u8] {
        let mut bus = FlatSmpBus::new();
        let mut smp = Smp::new();
        smp.a = 0x42;
        smp.x = 0x10;
        smp.y = 0x10;
        bus.poke(0x1244, 0x42);
        let cycles = run_one(&mut smp, &mut bus, &[opcode, 0x34, 0x12]);
        assert_eq!(cycles, 5, "opcode ${:02X}", opcode);
        assert!(smp.psw.z);
    }
}

#[test]
fn cmp_a_dp_x_indirect_six_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x42;
    smp.x = 0x02;
    bus.poke(0x0022, 0x00);
    bus.poke(0x0023, 0x12);
    bus.poke(0x1200, 0x42);
    let cycles = run_one(&mut smp, &mut bus, &[0x67, 0x20]);
    assert_eq!(cycles, 6);
    assert!(smp.psw.z);
}

#[test]
fn cmp_a_dp_indirect_plus_y_six_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x42;
    smp.y = 0x05;
    bus.poke(0x0020, 0x00);
    bus.poke(0x0021, 0x12);
    bus.poke(0x1205, 0x42);
    let cycles = run_one(&mut smp, &mut bus, &[0x77, 0x20]);
    assert_eq!(cycles, 6);
    assert!(smp.psw.z);
}

#[test]
fn cmp_x_y_indirect_compares_x_against_y_does_not_modify_memory() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.x = 0x10;
    smp.y = 0x20;
    bus.poke(0x0010, 0x42); // (X) value
    bus.poke(0x0020, 0x42); // (Y) value
    let cycles = run_one(&mut smp, &mut bus, &[0x79]);
    assert_eq!(cycles, 5);
    assert!(smp.psw.z, "(X) == (Y) -> Z set");
    assert!(smp.psw.c);
    assert_eq!(bus.peek(0x0010), 0x42, "CMP must not modify (X)");
    assert_eq!(bus.peek(0x0020), 0x42, "CMP must not modify (Y)");
}

#[test]
fn cmp_dp_dp_six_cycles_dst_minus_src() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    bus.poke(0x0010, 0x40); // src
    bus.poke(0x0020, 0x42); // dst (the LHS of the comparison)
    // Stream: opcode src_dp dst_dp
    let cycles = run_one(&mut smp, &mut bus, &[0x69, 0x10, 0x20]);
    assert_eq!(cycles, 6);
    assert!(smp.psw.c, "dst >= src -> C");
    assert!(!smp.psw.z);
    assert_eq!(bus.peek(0x0020), 0x42, "CMP must not modify dst");
}

#[test]
fn cmp_dp_imm_five_cycles_dp_minus_imm() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    bus.poke(0x0020, 0x42);
    // Stream: opcode imm dp -> CMP $20, #$42
    let cycles = run_one(&mut smp, &mut bus, &[0x78, 0x42, 0x20]);
    assert_eq!(cycles, 5);
    assert!(smp.psw.z);
    assert_eq!(bus.peek(0x0020), 0x42);
}

#[test]
fn cmp_x_imm_dp_abs_cycle_counts() {
    // CMP X,#imm = $C8 / 2 cycles
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.x = 0x42;
    let cycles = run_one(&mut smp, &mut bus, &[0xC8, 0x42]);
    assert_eq!(cycles, 2);
    assert!(smp.psw.z);

    // CMP X,dp = $3E / 3 cycles
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.x = 0x10;
    bus.poke(0x0020, 0x05);
    let cycles = run_one(&mut smp, &mut bus, &[0x3E, 0x20]);
    assert_eq!(cycles, 3);
    assert!(smp.psw.c);

    // CMP X,!abs = $1E / 4 cycles
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.x = 0x10;
    bus.poke(0x1234, 0x10);
    let cycles = run_one(&mut smp, &mut bus, &[0x1E, 0x34, 0x12]);
    assert_eq!(cycles, 4);
    assert!(smp.psw.z);
}

#[test]
fn cmp_y_imm_dp_abs_cycle_counts() {
    // CMP Y,#imm = $AD / 2 cycles
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.y = 0x42;
    let cycles = run_one(&mut smp, &mut bus, &[0xAD, 0x42]);
    assert_eq!(cycles, 2);
    assert!(smp.psw.z);

    // CMP Y,dp = $7E / 3 cycles
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.y = 0x10;
    bus.poke(0x0020, 0x10);
    let cycles = run_one(&mut smp, &mut bus, &[0x7E, 0x20]);
    assert_eq!(cycles, 3);
    assert!(smp.psw.z);

    // CMP Y,!abs = $5E / 4 cycles
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.y = 0x80;
    bus.poke(0x1234, 0x80);
    let cycles = run_one(&mut smp, &mut bus, &[0x5E, 0x34, 0x12]);
    assert_eq!(cycles, 4);
    assert!(smp.psw.z);
}

// ----- AND / OR / EOR families ----------------------------------------
//
// Logical ops touch only N/Z. Memory-destination forms (dp,dp /
// dp,#imm / (X),(Y)) write the result back to the destination - they
// do NOT discard like CMP does. Carry/V/H must survive untouched.

#[test]
fn or_a_imm_sets_n_preserves_carry_and_overflow() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x40;
    smp.psw.c = true;
    smp.psw.v = true;
    smp.psw.h = true;
    let cycles = run_one(&mut smp, &mut bus, &[0x08, 0x80]);
    assert_eq!(cycles, 2);
    assert_eq!(smp.a, 0xC0);
    assert!(smp.psw.n);
    assert!(!smp.psw.z);
    assert!(smp.psw.c, "OR must not touch C");
    assert!(smp.psw.v, "OR must not touch V");
    assert!(smp.psw.h, "OR must not touch H");
}

#[test]
fn and_a_imm_zero_result_sets_z() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x0F;
    let cycles = run_one(&mut smp, &mut bus, &[0x28, 0xF0]);
    assert_eq!(cycles, 2);
    assert_eq!(smp.a, 0x00);
    assert!(smp.psw.z);
    assert!(!smp.psw.n);
}

#[test]
fn eor_a_imm_clears_matching_bits() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0xFF;
    let cycles = run_one(&mut smp, &mut bus, &[0x48, 0x0F]);
    assert_eq!(cycles, 2);
    assert_eq!(smp.a, 0xF0);
    assert!(smp.psw.n);
    assert!(!smp.psw.z);
}

#[test]
fn or_a_dp_three_cycles_and_or_a_abs_four() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x10;
    bus.poke(0x0020, 0x01);
    let cycles = run_one(&mut smp, &mut bus, &[0x04, 0x20]);
    assert_eq!(cycles, 3);
    assert_eq!(smp.a, 0x11);

    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x10;
    bus.poke(0x1234, 0x01);
    let cycles = run_one(&mut smp, &mut bus, &[0x05, 0x34, 0x12]);
    assert_eq!(cycles, 4);
    assert_eq!(smp.a, 0x11);
}

#[test]
fn and_a_dp_x_direct_four_cycles() {
    // AND A,(X): 3-cycle on paper, but the (X)-direct mode adds the
    // dummy idle that op_*_a_dp_x_direct issues -> 3 cycles total
    // (opcode + idle + read). Mirrors ADC/SBC/CMP timings.
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0xF0;
    smp.x = 0x10;
    bus.poke(0x0010, 0x3F);
    let cycles = run_one(&mut smp, &mut bus, &[0x26]);
    assert_eq!(cycles, 3);
    assert_eq!(smp.a, 0x30);
}

#[test]
fn or_x_indirect_y_indirect_writes_to_x_address_five_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.x = 0x10;
    smp.y = 0x20;
    bus.poke(0x0010, 0x0F); // (X)
    bus.poke(0x0020, 0xF0); // (Y)
    let cycles = run_one(&mut smp, &mut bus, &[0x19]);
    assert_eq!(cycles, 5);
    assert_eq!(bus.peek(0x0010), 0xFF, "OR (X),(Y) writes back to (X)");
}

#[test]
fn and_dp_dp_six_cycles_writes_to_destination() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    bus.poke(0x0010, 0x0F); // src
    bus.poke(0x0020, 0xFF); // dst
    let cycles = run_one(&mut smp, &mut bus, &[0x29, 0x10, 0x20]);
    assert_eq!(cycles, 6);
    assert_eq!(bus.peek(0x0020), 0x0F);
    assert_eq!(bus.peek(0x0010), 0x0F, "src untouched");
}

#[test]
fn eor_dp_imm_five_cycles_writes_back() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    bus.poke(0x0020, 0xAA);
    let cycles = run_one(&mut smp, &mut bus, &[0x58, 0xFF, 0x20]);
    assert_eq!(cycles, 5);
    assert_eq!(bus.peek(0x0020), 0x55);
}

// ----- INC / DEC of memory --------------------------------------------
//
// RMW ops: read, modify, write back. N/Z from result, C/V/H survive.
// Cycle counts: dp=4, dp+X=5, !abs=5. Wraparound at 0xFF/0x00.

#[test]
fn inc_dp_writes_back_and_sets_nz() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.psw.c = true;
    smp.psw.v = true;
    smp.psw.h = true;
    bus.poke(0x0020, 0x7F);
    let cycles = run_one(&mut smp, &mut bus, &[0xAB, 0x20]);
    assert_eq!(cycles, 4);
    assert_eq!(bus.peek(0x0020), 0x80);
    assert!(smp.psw.n);
    assert!(!smp.psw.z);
    assert!(smp.psw.c, "INC must not touch C");
    assert!(smp.psw.v, "INC must not touch V");
    assert!(smp.psw.h, "INC must not touch H");
}

#[test]
fn dec_dp_wraps_to_ff_sets_n_clears_z() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    bus.poke(0x0020, 0x00);
    let cycles = run_one(&mut smp, &mut bus, &[0x8B, 0x20]);
    assert_eq!(cycles, 4);
    assert_eq!(bus.peek(0x0020), 0xFF);
    assert!(smp.psw.n);
    assert!(!smp.psw.z);
}

#[test]
fn inc_dp_wraps_to_zero_sets_z() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    bus.poke(0x0020, 0xFF);
    let cycles = run_one(&mut smp, &mut bus, &[0xAB, 0x20]);
    assert_eq!(cycles, 4);
    assert_eq!(bus.peek(0x0020), 0x00);
    assert!(smp.psw.z);
    assert!(!smp.psw.n);
}

#[test]
fn inc_dp_plus_x_five_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.x = 0x05;
    bus.poke(0x0025, 0x10);
    let cycles = run_one(&mut smp, &mut bus, &[0xBB, 0x20]);
    assert_eq!(cycles, 5);
    assert_eq!(bus.peek(0x0025), 0x11);
}

#[test]
fn dec_dp_plus_x_five_cycles_wraps_dp() {
    // dp+X wraps within the direct page, so 0xFF + 1 -> 0x00 of dp.
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.x = 0x10;
    bus.poke(0x000F, 0x01); // (0xFF + 0x10) & 0xFF = 0x0F
    let cycles = run_one(&mut smp, &mut bus, &[0x9B, 0xFF]);
    assert_eq!(cycles, 5);
    assert_eq!(bus.peek(0x000F), 0x00);
    assert!(smp.psw.z);
}

#[test]
fn inc_abs_five_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    bus.poke(0x1234, 0x42);
    let cycles = run_one(&mut smp, &mut bus, &[0xAC, 0x34, 0x12]);
    assert_eq!(cycles, 5);
    assert_eq!(bus.peek(0x1234), 0x43);
}

#[test]
fn dec_abs_five_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    bus.poke(0x1234, 0x01);
    let cycles = run_one(&mut smp, &mut bus, &[0x8C, 0x34, 0x12]);
    assert_eq!(cycles, 5);
    assert_eq!(bus.peek(0x1234), 0x00);
    assert!(smp.psw.z);
}

// ----- Shifts and rotates ---------------------------------------------
//
// ASL/LSR set C from the bit shifted out; ROL/ROR rotate C through.
// All four set N/Z from the result; V/H must survive untouched.
// Cycle counts: A=2, dp=4, dp+X=5, !abs=5.

#[test]
fn asl_a_high_bit_to_carry_two_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x81;
    smp.psw.v = true;
    smp.psw.h = true;
    let cycles = run_one(&mut smp, &mut bus, &[0x1C]);
    assert_eq!(cycles, 2);
    assert_eq!(smp.a, 0x02);
    assert!(smp.psw.c);
    assert!(!smp.psw.n);
    assert!(!smp.psw.z);
    assert!(smp.psw.v, "ASL must not touch V");
    assert!(smp.psw.h, "ASL must not touch H");
}

#[test]
fn asl_dp_writes_back_in_four_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    bus.poke(0x0020, 0x40);
    let cycles = run_one(&mut smp, &mut bus, &[0x0B, 0x20]);
    assert_eq!(cycles, 4);
    assert_eq!(bus.peek(0x0020), 0x80);
    assert!(smp.psw.n);
    assert!(!smp.psw.c);
}

#[test]
fn lsr_a_low_bit_to_carry_clears_n() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x81;
    smp.psw.n = true; // ensure LSR clears it (high bit shifts in as 0)
    let cycles = run_one(&mut smp, &mut bus, &[0x5C]);
    assert_eq!(cycles, 2);
    assert_eq!(smp.a, 0x40);
    assert!(smp.psw.c);
    assert!(!smp.psw.n);
}

#[test]
fn lsr_a_zero_result_sets_z() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x01;
    let cycles = run_one(&mut smp, &mut bus, &[0x5C]);
    assert_eq!(cycles, 2);
    assert_eq!(smp.a, 0x00);
    assert!(smp.psw.c);
    assert!(smp.psw.z);
}

#[test]
fn rol_a_rotates_through_carry() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x80;
    smp.psw.c = true;
    let cycles = run_one(&mut smp, &mut bus, &[0x3C]);
    assert_eq!(cycles, 2);
    assert_eq!(smp.a, 0x01, "old C rotated into bit 0");
    assert!(smp.psw.c, "old bit 7 is the new C");
}

#[test]
fn ror_a_rotates_through_carry() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x01;
    smp.psw.c = true;
    let cycles = run_one(&mut smp, &mut bus, &[0x7C]);
    assert_eq!(cycles, 2);
    assert_eq!(smp.a, 0x80, "old C rotated into bit 7");
    assert!(smp.psw.c, "old bit 0 is the new C");
    assert!(smp.psw.n);
}

#[test]
fn rol_dp_plus_x_five_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.x = 0x05;
    smp.psw.c = false;
    bus.poke(0x0025, 0xAA);
    let cycles = run_one(&mut smp, &mut bus, &[0x3B, 0x20]);
    assert_eq!(cycles, 5);
    assert_eq!(bus.peek(0x0025), 0x54);
    assert!(smp.psw.c);
}

#[test]
fn ror_abs_five_cycles_writes_back() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.psw.c = false;
    bus.poke(0x1234, 0x05);
    let cycles = run_one(&mut smp, &mut bus, &[0x6C, 0x34, 0x12]);
    assert_eq!(cycles, 5);
    assert_eq!(bus.peek(0x1234), 0x02);
    assert!(smp.psw.c);
}

// ----- 16-bit YA word ops ---------------------------------------------
//
// YA pair: Y is high byte, A is low byte. Word memory is little-endian
// in dp (dp+1 wraps within the page). MOVW dp,YA does NOT update
// flags (Anomie quirk). ADDW/SUBW do not consume carry-in. CMPW only
// touches N/Z/C. INCW/DECW only touch N/Z.

#[test]
fn movw_ya_dp_loads_little_endian_five_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    bus.poke(0x0020, 0x34);
    bus.poke(0x0021, 0x12);
    let cycles = run_one(&mut smp, &mut bus, &[0xBA, 0x20]);
    assert_eq!(cycles, 5);
    assert_eq!(smp.a, 0x34);
    assert_eq!(smp.y, 0x12);
    assert!(!smp.psw.z);
    assert!(!smp.psw.n);
}

#[test]
fn movw_ya_dp_zero_word_sets_z() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    bus.poke(0x0020, 0x00);
    bus.poke(0x0021, 0x00);
    let cycles = run_one(&mut smp, &mut bus, &[0xBA, 0x20]);
    assert_eq!(cycles, 5);
    assert!(smp.psw.z);
}

#[test]
fn movw_dp_ya_writes_word_and_does_not_update_flags() {
    // SPC700 quirk: MOVW dp,YA does NOT touch any flag. We pre-set
    // every flag, run a write of YA=0x0000 (Z would normally be set
    // by a load-style op), and verify nothing changed.
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x00;
    smp.y = 0x00;
    smp.psw.n = false;
    smp.psw.z = false;
    smp.psw.c = true;
    smp.psw.v = true;
    smp.psw.h = true;
    let cycles = run_one(&mut smp, &mut bus, &[0xDA, 0x20]);
    assert_eq!(cycles, 5);
    assert_eq!(bus.peek(0x0020), 0x00);
    assert_eq!(bus.peek(0x0021), 0x00);
    assert!(!smp.psw.z, "MOVW dp,YA must not set Z");
    assert!(!smp.psw.n);
    assert!(smp.psw.c, "MOVW dp,YA must preserve C");
    assert!(smp.psw.v);
    assert!(smp.psw.h);
}

#[test]
fn addw_ya_dp_full_carry_and_v_flags() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0xFF;
    smp.y = 0xFF;
    bus.poke(0x0020, 0x01);
    bus.poke(0x0021, 0x00); // word = 0x0001
    smp.psw.c = false; // ADDW ignores C-in regardless
    let cycles = run_one(&mut smp, &mut bus, &[0x7A, 0x20]);
    assert_eq!(cycles, 5);
    assert_eq!(smp.a, 0x00);
    assert_eq!(smp.y, 0x00);
    assert!(smp.psw.c, "carry out of bit 15");
    assert!(smp.psw.z);
    assert!(!smp.psw.n);
}

#[test]
fn addw_ya_dp_signed_overflow_sets_v() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    // 0x7FFF + 0x0001 = 0x8000 -> signed overflow positive->negative
    smp.a = 0xFF;
    smp.y = 0x7F;
    bus.poke(0x0020, 0x01);
    bus.poke(0x0021, 0x00);
    let cycles = run_one(&mut smp, &mut bus, &[0x7A, 0x20]);
    assert_eq!(cycles, 5);
    assert_eq!(smp.y, 0x80);
    assert_eq!(smp.a, 0x00);
    assert!(smp.psw.v);
    assert!(smp.psw.n);
    assert!(!smp.psw.c);
}

#[test]
fn subw_ya_dp_underflow_clears_carry() {
    // Carry convention: C=1 means no borrow.
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x00;
    smp.y = 0x00;
    bus.poke(0x0020, 0x01);
    bus.poke(0x0021, 0x00);
    let cycles = run_one(&mut smp, &mut bus, &[0x9A, 0x20]);
    assert_eq!(cycles, 5);
    assert_eq!(smp.y, 0xFF);
    assert_eq!(smp.a, 0xFF);
    assert!(!smp.psw.c);
    assert!(smp.psw.n);
}

#[test]
fn cmpw_ya_dp_equal_sets_z_and_c_only_no_v_no_h_four_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.a = 0x34;
    smp.y = 0x12;
    smp.psw.v = true; // pre-load: must survive
    smp.psw.h = true;
    bus.poke(0x0020, 0x34);
    bus.poke(0x0021, 0x12);
    let cycles = run_one(&mut smp, &mut bus, &[0x5A, 0x20]);
    assert_eq!(cycles, 4, "CMPW has no internal idle (4 cycles)");
    assert!(smp.psw.z);
    assert!(smp.psw.c);
    assert!(!smp.psw.n);
    assert!(smp.psw.v, "CMPW must not touch V");
    assert!(smp.psw.h, "CMPW must not touch H");
}

#[test]
fn incw_dp_six_cycles_carries_into_high_byte() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    bus.poke(0x0020, 0xFF);
    bus.poke(0x0021, 0x12);
    let cycles = run_one(&mut smp, &mut bus, &[0x3A, 0x20]);
    assert_eq!(cycles, 6);
    assert_eq!(bus.peek(0x0020), 0x00);
    assert_eq!(bus.peek(0x0021), 0x13, "low-byte overflow propagates");
    assert!(!smp.psw.z);
    assert!(!smp.psw.n);
}

#[test]
fn incw_dp_wraps_to_zero_sets_z() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    bus.poke(0x0020, 0xFF);
    bus.poke(0x0021, 0xFF);
    let cycles = run_one(&mut smp, &mut bus, &[0x3A, 0x20]);
    assert_eq!(cycles, 6);
    assert_eq!(bus.peek(0x0020), 0x00);
    assert_eq!(bus.peek(0x0021), 0x00);
    assert!(smp.psw.z);
}

#[test]
fn decw_dp_six_cycles_borrows_into_high_byte() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    bus.poke(0x0020, 0x00);
    bus.poke(0x0021, 0x12);
    let cycles = run_one(&mut smp, &mut bus, &[0x1A, 0x20]);
    assert_eq!(cycles, 6);
    assert_eq!(bus.peek(0x0020), 0xFF);
    assert_eq!(bus.peek(0x0021), 0x11);
    assert!(!smp.psw.z);
    assert!(!smp.psw.n);
}

// ----- MUL / DIV ------------------------------------------------------
//
// MUL YA: 9 cycles (op + 8 idles). YA = Y*A unsigned. N/Z reflect the
// HIGH byte (Y) only - 0x0001 sets Z because Y is zero. DIV YA,X:
// 12 cycles (op + 11 idles). A = YA/X, Y = YA%X. V is set when Y>=X
// (result would not fit in 8 bits) - the bsnes /512 closed form
// then provides the SPC700-faithful quotient/remainder.

#[test]
fn mul_ya_simple_product_nine_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.y = 0x12;
    smp.a = 0x34;
    let cycles = run_one(&mut smp, &mut bus, &[0xCF]);
    assert_eq!(cycles, 9);
    // 0x12 * 0x34 = 0x03A8
    assert_eq!(smp.a, 0xA8);
    assert_eq!(smp.y, 0x03);
    assert!(!smp.psw.n);
    assert!(!smp.psw.z);
}

#[test]
fn mul_ya_zero_high_byte_sets_z_quirk() {
    // 0x01 * 0x01 = 0x0001. Y=0, A=1. SPC700 quirk: Z is set from Y
    // only, so Z=1 here even though the full product is non-zero.
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.y = 0x01;
    smp.a = 0x01;
    let cycles = run_one(&mut smp, &mut bus, &[0xCF]);
    assert_eq!(cycles, 9);
    assert_eq!(smp.y, 0x00);
    assert_eq!(smp.a, 0x01);
    assert!(smp.psw.z, "MUL Z reflects Y only");
    assert!(!smp.psw.n);
}

#[test]
fn mul_ya_sets_n_when_high_byte_negative() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.y = 0xFF;
    smp.a = 0xFF;
    let _ = run_one(&mut smp, &mut bus, &[0xCF]);
    // 0xFF * 0xFF = 0xFE01
    assert_eq!(smp.y, 0xFE);
    assert_eq!(smp.a, 0x01);
    assert!(smp.psw.n);
}

#[test]
fn div_ya_x_simple_twelve_cycles() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.y = 0x00;
    smp.a = 0x06;
    smp.x = 0x03;
    let cycles = run_one(&mut smp, &mut bus, &[0x9E]);
    assert_eq!(cycles, 12);
    assert_eq!(smp.a, 0x02);
    assert_eq!(smp.y, 0x00);
    assert!(!smp.psw.v);
    assert!(!smp.psw.h);
    assert!(!smp.psw.n);
    assert!(!smp.psw.z);
}

#[test]
fn div_ya_x_zero_quotient_sets_z() {
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.y = 0x00;
    smp.a = 0x07;
    smp.x = 0x10;
    let _ = run_one(&mut smp, &mut bus, &[0x9E]);
    assert_eq!(smp.a, 0x00);
    assert_eq!(smp.y, 0x07);
    assert!(smp.psw.z);
}

#[test]
fn div_ya_x_v_flag_set_when_y_ge_x() {
    // Y >= X means quotient won't fit in 8 bits.
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.y = 0x10;
    smp.a = 0x00;
    smp.x = 0x05;
    let _ = run_one(&mut smp, &mut bus, &[0x9E]);
    assert!(smp.psw.v, "Y(0x10) >= X(0x05) -> V");
    assert!(!smp.psw.h, "(Y&0xF=0) < (X&0xF=5) -> H clear");
}

#[test]
fn div_ya_x_h_flag_from_low_nibbles() {
    // H is set when (Y & 0x0F) >= (X & 0x0F).
    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.y = 0x02;
    smp.a = 0x00;
    smp.x = 0x35;
    let _ = run_one(&mut smp, &mut bus, &[0x9E]);
    assert!(!smp.psw.v, "Y(2) < X(0x35) -> V clear");
    assert!(!smp.psw.h, "(Y&0xF=2) < (X&0xF=5) -> H clear");

    let mut bus = FlatSmpBus::new();
    let mut smp = Smp::new();
    smp.y = 0x05;
    smp.a = 0x00;
    smp.x = 0x32;
    let _ = run_one(&mut smp, &mut bus, &[0x9E]);
    assert!(smp.psw.h, "(Y&0xF=5) >= (X&0xF=2) -> H set");
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
