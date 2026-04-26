// SPDX-License-Identifier: GPL-3.0-or-later
//! Unit tests for the 65C816 core. We use [`FlatBus`] - a 16 MiB
//! linear memory that charges 8 master cycles per access - so each
//! test can poke a tiny program at $00:8000 and assert post-state
//! without dragging the SNES bus / mappers / MMIO in.

use super::bus::{FlatBus, SnesBus};
use super::{Cpu, Mode};

/// Helper: build a CPU + bus with the reset vector at $00:FFFC
/// pointing at `start`, and the supplied program bytes laid down
/// starting at `start`. After return, `cpu.pc == start`.
fn boot_with(start: u16, program: &[u8]) -> (Cpu, FlatBus) {
    let mut bus = FlatBus::new();
    bus.poke(0x00FFFC, start as u8);
    bus.poke(0x00FFFD, (start >> 8) as u8);
    bus.poke_slice(start as u32, program);
    let mut cpu = Cpu::new();
    cpu.reset(&mut bus);
    assert_eq!(cpu.pc, start);
    (cpu, bus)
}

#[test]
fn reset_loads_pc_from_vector_and_enters_emulation() {
    let mut bus = FlatBus::new();
    bus.poke(0x00FFFC, 0x34);
    bus.poke(0x00FFFD, 0x12);
    let mut cpu = Cpu::new();
    cpu.reset(&mut bus);
    assert_eq!(cpu.pc, 0x1234);
    assert_eq!(cpu.mode, Mode::Emulation);
    assert!(cpu.p.m);
    assert!(cpu.p.x);
    assert!(cpu.p.i);
    assert!(!cpu.p.d);
    assert_eq!(cpu.s, 0x01FF);
}

#[test]
fn nop_advances_pc_one() {
    let (mut cpu, mut bus) = boot_with(0x8000, &[0xEA]);
    let op = cpu.step(&mut bus);
    assert_eq!(op, 0xEA);
    assert_eq!(cpu.pc, 0x8001);
}

#[test]
fn clc_sec_toggle_carry() {
    let (mut cpu, mut bus) = boot_with(0x8000, &[0x38, 0x18]); // SEC, CLC
    cpu.step(&mut bus);
    assert!(cpu.p.c);
    cpu.step(&mut bus);
    assert!(!cpu.p.c);
}

#[test]
fn xce_swaps_to_native_and_back() {
    let (mut cpu, mut bus) = boot_with(0x8000, &[0x18, 0xFB, 0x38, 0xFB]);
    // CLC; XCE -> native
    cpu.step(&mut bus);
    cpu.step(&mut bus);
    assert_eq!(cpu.mode, Mode::Native);
    // P.c now == 1 (was the old E=1)
    assert!(cpu.p.c);
    // SEC; XCE -> emulation
    cpu.step(&mut bus);
    cpu.step(&mut bus);
    assert_eq!(cpu.mode, Mode::Emulation);
    assert!(cpu.p.m);
    assert!(cpu.p.x);
}

#[test]
fn lda_imm_8bit_in_emulation() {
    let (mut cpu, mut bus) = boot_with(0x8000, &[0xA9, 0x42]);
    cpu.step(&mut bus);
    assert_eq!(cpu.a(), 0x42);
    assert!(!cpu.p.z);
    assert!(!cpu.p.n);
}

#[test]
fn lda_imm_16bit_in_native_clears_m_flag_via_rep() {
    // CLC; XCE; REP #$30; LDA #$BEEF; expect C = $BEEF.
    let (mut cpu, mut bus) = boot_with(
        0x8000,
        &[0x18, 0xFB, 0xC2, 0x30, 0xA9, 0xEF, 0xBE],
    );
    cpu.step(&mut bus); // CLC
    cpu.step(&mut bus); // XCE
    cpu.step(&mut bus); // REP #$30 -> m=0, x=0
    assert!(!cpu.p.m);
    assert!(!cpu.p.x);
    cpu.step(&mut bus); // LDA #$BEEF
    assert_eq!(cpu.c, 0xBEEF);
}

#[test]
fn rep_sep_set_and_clear_m_x() {
    let (mut cpu, mut bus) = boot_with(0x8000, &[0x18, 0xFB, 0xC2, 0x30, 0xE2, 0x20]);
    cpu.step(&mut bus); // CLC
    cpu.step(&mut bus); // XCE -> native
    cpu.step(&mut bus); // REP #$30 -> m=0, x=0
    assert!(!cpu.p.m);
    assert!(!cpu.p.x);
    cpu.step(&mut bus); // SEP #$20 -> m=1
    assert!(cpu.p.m);
    assert!(!cpu.p.x);
}

#[test]
fn sep_x_zeroes_index_high_bytes() {
    // Native; widen X/Y; load 16-bit garbage; SEP #$10 must
    // physically zero the high byte (errata).
    let (mut cpu, mut bus) = boot_with(0x8000, &[0x18, 0xFB, 0xC2, 0x30, 0xE2, 0x10]);
    cpu.step(&mut bus); // CLC
    cpu.step(&mut bus); // XCE
    cpu.step(&mut bus); // REP #$30
    cpu.x = 0xBEEF;
    cpu.y = 0xDEAD;
    cpu.step(&mut bus); // SEP #$10
    assert!(cpu.p.x);
    assert_eq!(cpu.x, 0x00EF);
    assert_eq!(cpu.y, 0x00AD);
}

#[test]
fn sta_then_lda_round_trips_through_dbr_absolute() {
    // E mode, absolute store and load: STA $7E00 / LDA $7E00 with DBR=0
    // Place result at PRG end so we don't collide with reset vector.
    let (mut cpu, mut bus) = boot_with(
        0x8000,
        &[
            0xA9, 0x55, // LDA #$55
            0x8D, 0x00, 0x7E, // STA $7E00
            0xA9, 0x00, // LDA #$00
            0xAD, 0x00, 0x7E, // LDA $7E00
        ],
    );
    cpu.step(&mut bus);
    cpu.step(&mut bus);
    assert_eq!(bus.peek(0x7E00), 0x55);
    cpu.step(&mut bus);
    assert_eq!(cpu.a(), 0x00);
    cpu.step(&mut bus);
    assert_eq!(cpu.a(), 0x55);
}

#[test]
fn tax_in_native_16bit_copies_full_c() {
    let (mut cpu, mut bus) = boot_with(0x8000, &[0x18, 0xFB, 0xC2, 0x30, 0xA9, 0xCD, 0xAB, 0xAA]);
    cpu.step(&mut bus); // CLC
    cpu.step(&mut bus); // XCE
    cpu.step(&mut bus); // REP #$30
    cpu.step(&mut bus); // LDA #$ABCD
    cpu.step(&mut bus); // TAX
    assert_eq!(cpu.x, 0xABCD);
}

#[test]
fn bra_jumps_with_signed_offset() {
    let (mut cpu, mut bus) = boot_with(0x8000, &[0x80, 0x10]); // BRA +16
    cpu.step(&mut bus);
    assert_eq!(cpu.pc, 0x8012);
}

#[test]
fn beq_taken_only_when_zero_set() {
    let (mut cpu, mut bus) = boot_with(0x8000, &[0xF0, 0x10, 0xF0, 0x10]);
    cpu.p.z = false;
    cpu.step(&mut bus); // BEQ not taken
    assert_eq!(cpu.pc, 0x8002);
    cpu.p.z = true;
    cpu.step(&mut bus); // BEQ taken
    assert_eq!(cpu.pc, 0x8014);
}

#[test]
fn jmp_abs_replaces_pc_within_pbr() {
    let (mut cpu, mut bus) = boot_with(0x8000, &[0x4C, 0x34, 0x12]);
    cpu.step(&mut bus);
    assert_eq!(cpu.pc, 0x1234);
    assert_eq!(cpu.pbr, 0);
}

#[test]
fn jsr_then_rts_round_trips() {
    // Program: JSR $1234 placed at $8000; at $1234 put NOP, RTS.
    let mut bus = FlatBus::new();
    bus.poke(0x00FFFC, 0x00);
    bus.poke(0x00FFFD, 0x80);
    bus.poke_slice(0x008000, &[0x20, 0x34, 0x12]);
    bus.poke_slice(0x001234, &[0xEA, 0x60]);
    let mut cpu = Cpu::new();
    cpu.reset(&mut bus);
    cpu.step(&mut bus); // JSR
    assert_eq!(cpu.pc, 0x1234);
    cpu.step(&mut bus); // NOP
    cpu.step(&mut bus); // RTS
    assert_eq!(cpu.pc, 0x8003);
}

#[test]
fn jsl_jml_and_rtl_round_trip_in_native() {
    let mut bus = FlatBus::new();
    bus.poke(0x00FFFC, 0x00);
    bus.poke(0x00FFFD, 0x80);
    // 0x8000: CLC, XCE, JSL $7E1234, NOP, ...
    bus.poke_slice(0x008000, &[0x18, 0xFB, 0x22, 0x34, 0x12, 0x7E, 0xEA]);
    // $7E:1234: NOP, RTL
    bus.poke_slice(0x7E1234, &[0xEA, 0x6B]);
    let mut cpu = Cpu::new();
    cpu.reset(&mut bus);
    cpu.step(&mut bus); // CLC
    cpu.step(&mut bus); // XCE -> native
    cpu.step(&mut bus); // JSL
    assert_eq!(cpu.pc, 0x1234);
    assert_eq!(cpu.pbr, 0x7E);
    cpu.step(&mut bus); // NOP
    cpu.step(&mut bus); // RTL
    assert_eq!(cpu.pc, 0x8006);
    assert_eq!(cpu.pbr, 0x00);
}

#[test]
fn pha_pla_round_trip_8bit() {
    let (mut cpu, mut bus) = boot_with(0x8000, &[0xA9, 0x42, 0x48, 0xA9, 0x00, 0x68]);
    cpu.step(&mut bus); // LDA #$42
    cpu.step(&mut bus); // PHA
    cpu.step(&mut bus); // LDA #$00
    assert_eq!(cpu.a(), 0x00);
    cpu.step(&mut bus); // PLA
    assert_eq!(cpu.a(), 0x42);
}

#[test]
fn xba_swaps_a_and_b_halves() {
    let (mut cpu, mut bus) = boot_with(0x8000, &[0x18, 0xFB, 0xC2, 0x30, 0xA9, 0x34, 0x12, 0xEB]);
    cpu.step(&mut bus); // CLC
    cpu.step(&mut bus); // XCE -> native
    cpu.step(&mut bus); // REP #$30
    cpu.step(&mut bus); // LDA #$1234
    assert_eq!(cpu.c, 0x1234);
    cpu.step(&mut bus); // XBA
    assert_eq!(cpu.c, 0x3412);
}

#[test]
fn inc_dec_a_wraps_correctly_in_8bit() {
    let (mut cpu, mut bus) = boot_with(0x8000, &[0xA9, 0xFF, 0x1A, 0x3A]);
    cpu.step(&mut bus); // LDA #$FF
    cpu.step(&mut bus); // INC A -> $00, Z=1
    assert_eq!(cpu.a(), 0x00);
    assert!(cpu.p.z);
    cpu.step(&mut bus); // DEC A -> $FF, N=1
    assert_eq!(cpu.a(), 0xFF);
    assert!(cpu.p.n);
}

#[test]
fn long_load_uses_full_24bit_address() {
    let mut bus = FlatBus::new();
    bus.poke(0x00FFFC, 0x00);
    bus.poke(0x00FFFD, 0x80);
    bus.poke_slice(0x008000, &[0xAF, 0x21, 0x43, 0x7E]); // LDA $7E:4321
    bus.poke(0x7E4321, 0xAB);
    let mut cpu = Cpu::new();
    cpu.reset(&mut bus);
    cpu.step(&mut bus);
    assert_eq!(cpu.a(), 0xAB);
}

#[test]
fn stz_writes_zero() {
    let (mut cpu, mut bus) = boot_with(
        0x8000,
        &[0xA9, 0x55, 0x8D, 0x00, 0x7E, 0x9C, 0x00, 0x7E],
    );
    cpu.step(&mut bus); // LDA #$55
    cpu.step(&mut bus); // STA $7E00
    assert_eq!(bus.peek(0x7E00), 0x55);
    cpu.step(&mut bus); // STZ $7E00
    assert_eq!(bus.peek(0x7E00), 0x00);
}

#[test]
fn and_ora_eor_immediate_8bit() {
    let (mut cpu, mut bus) = boot_with(
        0x8000,
        &[
            0xA9, 0xF0, // LDA #$F0
            0x29, 0x0F, // AND #$0F -> $00
            0xA9, 0x12, // LDA #$12
            0x09, 0x80, // ORA #$80 -> $92
            0x49, 0xFF, // EOR #$FF -> $6D
        ],
    );
    cpu.step(&mut bus); // LDA #$F0
    cpu.step(&mut bus); // AND #$0F
    assert_eq!(cpu.a(), 0x00);
    assert!(cpu.p.z);
    assert!(!cpu.p.n);
    cpu.step(&mut bus); // LDA #$12
    cpu.step(&mut bus); // ORA #$80
    assert_eq!(cpu.a(), 0x92);
    assert!(cpu.p.n);
    cpu.step(&mut bus); // EOR #$FF
    assert_eq!(cpu.a(), 0x6D);
    assert!(!cpu.p.n);
    assert!(!cpu.p.z);
}

#[test]
fn adc_8bit_binary_carry_overflow() {
    // CLC; LDA #$50; ADC #$50 -> $A0, V=1, C=0, N=1
    let (mut cpu, mut bus) = boot_with(0x8000, &[0x18, 0xA9, 0x50, 0x69, 0x50]);
    cpu.step(&mut bus); // CLC
    cpu.step(&mut bus); // LDA #$50
    cpu.step(&mut bus); // ADC #$50
    assert_eq!(cpu.a(), 0xA0);
    assert!(cpu.p.v);
    assert!(!cpu.p.c);
    assert!(cpu.p.n);
}

#[test]
fn sbc_8bit_binary_borrow() {
    // SEC; LDA #$50; SBC #$30 -> $20, C=1 (no borrow)
    let (mut cpu, mut bus) = boot_with(0x8000, &[0x38, 0xA9, 0x50, 0xE9, 0x30]);
    cpu.step(&mut bus); // SEC
    cpu.step(&mut bus); // LDA #$50
    cpu.step(&mut bus); // SBC #$30
    assert_eq!(cpu.a(), 0x20);
    assert!(cpu.p.c);
    // SEC; LDA #$30; SBC #$50 -> $E0 (borrow), C=0, N=1
    let (mut cpu, mut bus) = boot_with(0x8000, &[0x38, 0xA9, 0x30, 0xE9, 0x50]);
    cpu.step(&mut bus); // SEC
    cpu.step(&mut bus); // LDA #$30
    cpu.step(&mut bus); // SBC #$50
    assert_eq!(cpu.a(), 0xE0);
    assert!(!cpu.p.c);
    assert!(cpu.p.n);
}

#[test]
fn adc_16bit_carry_propagates() {
    // Native, 16-bit A: $00FF + $0001 = $0100
    let (mut cpu, mut bus) = boot_with(
        0x8000,
        &[0x18, 0xFB, 0xC2, 0x30, 0xA9, 0xFF, 0x00, 0x18, 0x69, 0x01, 0x00],
    );
    cpu.step(&mut bus); // CLC
    cpu.step(&mut bus); // XCE
    cpu.step(&mut bus); // REP #$30
    cpu.step(&mut bus); // LDA #$00FF
    cpu.step(&mut bus); // CLC
    cpu.step(&mut bus); // ADC #$0001
    assert_eq!(cpu.c, 0x0100);
    assert!(!cpu.p.c);
    assert!(!cpu.p.z);
}

#[test]
fn cmp_sets_carry_when_a_ge_operand() {
    let (mut cpu, mut bus) = boot_with(0x8000, &[0xA9, 0x42, 0xC9, 0x42, 0xC9, 0x10, 0xC9, 0x80]);
    cpu.step(&mut bus); // LDA #$42
    cpu.step(&mut bus); // CMP #$42 -> Z=1, C=1
    assert!(cpu.p.z);
    assert!(cpu.p.c);
    cpu.step(&mut bus); // CMP #$10 -> A>op, Z=0, C=1
    assert!(!cpu.p.z);
    assert!(cpu.p.c);
    cpu.step(&mut bus); // CMP #$80 -> A<op, Z=0, C=0
    assert!(!cpu.p.c);
}

#[test]
fn cpx_cpy_immediate() {
    let (mut cpu, mut bus) = boot_with(0x8000, &[0xA2, 0x10, 0xE0, 0x10, 0xA0, 0x05, 0xC0, 0x10]);
    cpu.step(&mut bus); // LDX #$10
    cpu.step(&mut bus); // CPX #$10
    assert!(cpu.p.z);
    cpu.step(&mut bus); // LDY #$05
    cpu.step(&mut bus); // CPY #$10 -> Y<op, C=0
    assert!(!cpu.p.c);
}

#[test]
fn bit_immediate_only_updates_z() {
    let (mut cpu, mut bus) = boot_with(0x8000, &[0xA9, 0xF0, 0x89, 0x0F, 0x89, 0x80]);
    cpu.step(&mut bus); // LDA #$F0 -> N=1
    assert!(cpu.p.n);
    cpu.step(&mut bus); // BIT #$0F -> Z=1, N unchanged from prior LDA
    assert!(cpu.p.z);
    assert!(cpu.p.n);
    cpu.step(&mut bus); // BIT #$80 -> A&op = $80, Z=0
    assert!(!cpu.p.z);
}

#[test]
fn bit_absolute_pulls_n_v_from_operand() {
    // LDA #$FF; STA $7E00 ($FF stored); LDA #$00; BIT $7E00 -> N=1, V=1, Z=1.
    let (mut cpu, mut bus) = boot_with(
        0x8000,
        &[0xA9, 0xFF, 0x8D, 0x00, 0x7E, 0xA9, 0x00, 0x2C, 0x00, 0x7E],
    );
    cpu.step(&mut bus); // LDA #$FF
    cpu.step(&mut bus); // STA $7E00
    cpu.step(&mut bus); // LDA #$00
    cpu.step(&mut bus); // BIT $7E00
    assert!(cpu.p.n);
    assert!(cpu.p.v);
    assert!(cpu.p.z);
}

#[test]
fn asl_lsr_rol_ror_accumulator() {
    // A = $81; ASL -> $02 with C=1, N=0
    let (mut cpu, mut bus) = boot_with(0x8000, &[0xA9, 0x81, 0x0A]);
    cpu.step(&mut bus); // LDA #$81
    cpu.step(&mut bus); // ASL
    assert_eq!(cpu.a(), 0x02);
    assert!(cpu.p.c);
    // A = $02; LSR -> $01, C=0
    let (mut cpu, mut bus) = boot_with(0x8000, &[0xA9, 0x02, 0x4A]);
    cpu.step(&mut bus);
    cpu.step(&mut bus);
    assert_eq!(cpu.a(), 0x01);
    assert!(!cpu.p.c);
    // A = $80; SEC; ROL -> $01, C=1
    let (mut cpu, mut bus) = boot_with(0x8000, &[0xA9, 0x80, 0x38, 0x2A]);
    cpu.step(&mut bus);
    cpu.step(&mut bus);
    cpu.step(&mut bus);
    assert_eq!(cpu.a(), 0x01);
    assert!(cpu.p.c);
    // A = $01; SEC; ROR -> $80, C=1, N=1
    let (mut cpu, mut bus) = boot_with(0x8000, &[0xA9, 0x01, 0x38, 0x6A]);
    cpu.step(&mut bus);
    cpu.step(&mut bus);
    cpu.step(&mut bus);
    assert_eq!(cpu.a(), 0x80);
    assert!(cpu.p.c);
    assert!(cpu.p.n);
}

#[test]
fn inc_dec_memory_round_trip() {
    let (mut cpu, mut bus) = boot_with(
        0x8000,
        &[0xA9, 0x10, 0x8D, 0x00, 0x7E, 0xEE, 0x00, 0x7E, 0xCE, 0x00, 0x7E, 0xCE, 0x00, 0x7E],
    );
    cpu.step(&mut bus); // LDA #$10
    cpu.step(&mut bus); // STA $7E00
    cpu.step(&mut bus); // INC $7E00
    assert_eq!(bus.peek(0x7E00), 0x11);
    cpu.step(&mut bus); // DEC $7E00
    cpu.step(&mut bus); // DEC $7E00 -> $0F
    assert_eq!(bus.peek(0x7E00), 0x0F);
}

#[test]
fn tsb_trb_set_and_clear_bits() {
    // STA stores $0F at $7E00. TSB $7E00 with A=$F0 -> mem becomes
    // $FF, Z = (A & mem) == 0, with mem-pre = $0F so Z=1.
    let (mut cpu, mut bus) = boot_with(
        0x8000,
        &[
            0xA9, 0x0F, 0x8D, 0x00, 0x7E, // mem = $0F
            0xA9, 0xF0, 0x0C, 0x00, 0x7E, // TSB $7E00 (A=$F0)
            0xA9, 0x0F, 0x1C, 0x00, 0x7E, // TRB $7E00 (A=$0F)
        ],
    );
    for _ in 0..4 {
        cpu.step(&mut bus); // LDA, STA, LDA, TSB
    }
    // After TSB: mem = $FF; A & pre = $F0 & $0F = 0 -> Z=1.
    // Asserting before the next LDA so the freshly-loaded A
    // doesn't overwrite the flag we care about.
    assert_eq!(bus.peek(0x7E00), 0xFF);
    assert!(cpu.p.z);
    cpu.step(&mut bus); // LDA #$0F
    cpu.step(&mut bus); // TRB
    // After TRB: A & pre = $0F & $FF = $0F (nonzero) -> Z=0,
    // mem = $FF & ~$0F = $F0
    assert_eq!(bus.peek(0x7E00), 0xF0);
    assert!(!cpu.p.z);
}

#[test]
fn mvn_copies_block_forward() {
    // Native, 16-bit X/Y/A. Source $7E:1000..1003 = "ABCD".
    // A = 3 (4 bytes - 1), X = $1000, Y = $2000, MVN bank $7E -> $7E.
    // After: $7E:2000..2003 == ABCD; X=$1004, Y=$2004; A=$FFFF.
    let mut bus = FlatBus::new();
    bus.poke(0x00FFFC, 0x00);
    bus.poke(0x00FFFD, 0x80);
    bus.poke_slice(
        0x008000,
        &[
            0x18, 0xFB, // CLC; XCE -> native
            0xC2, 0x30, // REP #$30
            0xA9, 0x03, 0x00, // LDA #$0003
            0xA2, 0x00, 0x10, // LDX #$1000
            0xA0, 0x00, 0x20, // LDY #$2000
            0x54, 0x7E, 0x7E, // MVN $7E,$7E
            0xEA,
        ],
    );
    bus.poke_slice(0x7E1000, b"ABCD");
    let mut cpu = Cpu::new();
    cpu.reset(&mut bus);
    cpu.step(&mut bus); // CLC
    cpu.step(&mut bus); // XCE
    cpu.step(&mut bus); // REP
    cpu.step(&mut bus); // LDA
    cpu.step(&mut bus); // LDX
    cpu.step(&mut bus); // LDY
    cpu.step(&mut bus); // MVN (loops internally)
    assert_eq!(&bus.ram[0x7E2000..0x7E2004], b"ABCD");
    assert_eq!(cpu.x, 0x1004);
    assert_eq!(cpu.y, 0x2004);
    assert_eq!(cpu.c, 0xFFFF);
    assert_eq!(cpu.dbr, 0x7E);
}

#[test]
fn adc_decimal_mode_8bit_carries_per_nibble() {
    // SED; CLC; LDA #$25; ADC #$48 -> $73 (BCD), C=0
    let (mut cpu, mut bus) = boot_with(0x8000, &[0xF8, 0x18, 0xA9, 0x25, 0x69, 0x48]);
    cpu.step(&mut bus); // SED
    cpu.step(&mut bus); // CLC
    cpu.step(&mut bus); // LDA #$25
    cpu.step(&mut bus); // ADC #$48
    assert_eq!(cpu.a(), 0x73);
    assert!(!cpu.p.c);
    // SED; CLC; LDA #$58; ADC #$46 -> $04, C=1 (BCD overflow past 99)
    let (mut cpu, mut bus) = boot_with(0x8000, &[0xF8, 0x18, 0xA9, 0x58, 0x69, 0x46]);
    cpu.step(&mut bus);
    cpu.step(&mut bus);
    cpu.step(&mut bus);
    cpu.step(&mut bus);
    assert_eq!(cpu.a(), 0x04);
    assert!(cpu.p.c);
}

#[test]
fn sbc_decimal_mode_8bit_with_borrow() {
    // SED; SEC; LDA #$50; SBC #$25 -> $25, C=1 (no borrow)
    let (mut cpu, mut bus) = boot_with(0x8000, &[0xF8, 0x38, 0xA9, 0x50, 0xE9, 0x25]);
    for _ in 0..4 {
        cpu.step(&mut bus);
    }
    assert_eq!(cpu.a(), 0x25);
    assert!(cpu.p.c);
    // SED; SEC; LDA #$25; SBC #$50 -> $75, C=0 (borrow)
    let (mut cpu, mut bus) = boot_with(0x8000, &[0xF8, 0x38, 0xA9, 0x25, 0xE9, 0x50]);
    for _ in 0..4 {
        cpu.step(&mut bus);
    }
    assert_eq!(cpu.a(), 0x75);
    assert!(!cpu.p.c);
}

#[test]
fn adc_decimal_mode_16bit_propagates() {
    // Native, 16-bit A; SED; CLC; LDA #$1234; ADC #$5678 -> $6912 BCD
    let (mut cpu, mut bus) = boot_with(
        0x8000,
        &[
            0x18, 0xFB, 0xC2, 0x30, // CLC, XCE, REP #$30
            0xF8, 0x18, // SED, CLC
            0xA9, 0x34, 0x12, // LDA #$1234
            0x69, 0x78, 0x56, // ADC #$5678
        ],
    );
    for _ in 0..7 {
        cpu.step(&mut bus);
    }
    assert_eq!(cpu.c, 0x6912);
    assert!(!cpu.p.c);
}

#[test]
fn branch_taken_crossing_page_in_emulation_charges_extra_cycle() {
    // BEQ at $80FC takes a +5 forward branch -> $8103 (page cross).
    // Same branch in native mode does not charge the extra idle.
    let mut bus = FlatBus::new();
    bus.poke(0x00FFFC, 0xFC);
    bus.poke(0x00FFFD, 0x80);
    bus.poke_slice(0x0080FC, &[0xF0, 0x05]);
    let mut cpu = Cpu::new();
    cpu.reset(&mut bus);
    cpu.p.z = true;
    let before = bus.master_cycles();
    cpu.step(&mut bus); // BEQ +5 (taken, page-crosses)
    let emu_cost = bus.master_cycles() - before;
    assert_eq!(cpu.pc, 0x8103);

    // Native-mode rerun: same setup but in native, no extra idle.
    let mut bus = FlatBus::new();
    bus.poke(0x00FFFC, 0xFA);
    bus.poke(0x00FFFD, 0x80);
    // CLC, XCE, BEQ +5 from $80FC (so layout: CLC@$80FA, XCE@$80FB, BEQ@$80FC)
    bus.poke_slice(0x0080FA, &[0x18, 0xFB, 0xF0, 0x05]);
    let mut cpu = Cpu::new();
    cpu.reset(&mut bus);
    cpu.step(&mut bus); // CLC
    cpu.step(&mut bus); // XCE -> native
    cpu.p.z = true;
    let before = bus.master_cycles();
    cpu.step(&mut bus); // BEQ +5 (taken, page-crosses, but native)
    let nat_cost = bus.master_cycles() - before;
    assert_eq!(cpu.pc, 0x8103);
    assert!(emu_cost > nat_cost, "emulation should cost more than native on page cross: emu={emu_cost} nat={nat_cost}");
}

#[test]
fn dp_indirect_emulation_page_wrap_quirk() {
    // Emulation, D=$0000; STA via (dp) where dp=$FF.
    // The pointer's low byte is at $00:00FF, high byte at $00:0000
    // (wrapped, NOT $00:0100). Place the pointer there and check
    // that the load lands at the right address.
    let (mut cpu, mut bus) = boot_with(0x8000, &[0xB2, 0xFF]); // LDA ($FF)
    bus.poke(0x0000FF, 0x34); // pointer low
    bus.poke(0x000000, 0x12); // pointer high (wrapped)
    bus.poke(0x000100, 0xAA); // would-be high if NOT wrapping
    bus.poke(0x001234, 0x77); // target
    cpu.step(&mut bus);
    assert_eq!(cpu.a(), 0x77, "DP indirect should wrap inside DP page in emulation");
}

#[test]
fn rmw_16bit_writes_high_byte_first() {
    // Native, 16-bit; LDA #$1234; STA $7E00; ASL $7E00
    // After ASL, mem should be $2468; verify we wrote both bytes.
    // The high-then-low write order is internal but the final value
    // is identical. We assert correct final state here; cycle-order
    // matters for MMIO and lands with the bus tests in Phase 2d.
    let (mut cpu, mut bus) = boot_with(
        0x8000,
        &[
            0x18, 0xFB, 0xC2, 0x30, // CLC; XCE; REP #$30
            0xA9, 0x34, 0x12, // LDA #$1234
            0x8D, 0x00, 0x7E, // STA $7E00
            0x0E, 0x00, 0x7E, // ASL $7E00
        ],
    );
    for _ in 0..6 {
        cpu.step(&mut bus);
    }
    let lo = bus.peek(0x7E00) as u16;
    let hi = bus.peek(0x7E01) as u16;
    assert_eq!((hi << 8) | lo, 0x2468);
}

#[test]
fn brk_pushes_state_and_loads_emulation_vector() {
    let mut bus = FlatBus::new();
    bus.poke(0x00FFFC, 0x00);
    bus.poke(0x00FFFD, 0x80);
    bus.poke(0x00FFFE, 0x00); // BRK/IRQ vector
    bus.poke(0x00FFFF, 0x90);
    bus.poke_slice(0x008000, &[0x00, 0xAA]); // BRK + signature
    let mut cpu = Cpu::new();
    cpu.reset(&mut bus);
    cpu.step(&mut bus);
    assert_eq!(cpu.pc, 0x9000);
    assert!(cpu.p.i);
    assert_eq!(cpu.pbr, 0);
}

#[test]
fn nmi_pushes_and_loads_emulation_vector() {
    let mut bus = FlatBus::new();
    bus.poke(0x00FFFC, 0x00);
    bus.poke(0x00FFFD, 0x80);
    bus.poke(0x00FFFA, 0x00); // NMI vector lo
    bus.poke(0x00FFFB, 0x90); // NMI vector hi -> $9000
    bus.poke_slice(0x008000, &[0xEA]); // NOP at PC after reset
    let mut cpu = Cpu::new();
    cpu.reset(&mut bus);
    cpu.nmi_pending = true;
    cpu.step(&mut bus);
    assert_eq!(cpu.pc, 0x9000);
    assert!(cpu.p.i);
    assert!(!cpu.nmi_pending);
}
