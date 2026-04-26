// SPDX-License-Identifier: GPL-3.0-or-later
//! Unit tests for the 65C816 core. We use [`FlatBus`] - a 16 MiB
//! linear memory that charges 8 master cycles per access - so each
//! test can poke a tiny program at $00:8000 and assert post-state
//! without dragging the SNES bus / mappers / MMIO in.

use super::bus::FlatBus;
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
