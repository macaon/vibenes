// SPDX-License-Identifier: GPL-3.0-or-later
//! Integration tests against PeterLemon's SPC700 unit-test ROMs. Each
//! `.spc` file in the suite is a flat binary of SPC code starting at
//! `$0200` (per `bass`'s `output "...spc"` + `seek(SPCRAM)` convention,
//! NOT a standard SNES SPC sound dump). The tests run a sequence of
//! self-checking operations and write a one-byte handshake into
//! `$F4` (the SMP-to-CPU mailbox slot 0):
//!
//! - On pass for sub-test N: `$F4 = N` (test 1 = `$01`, ..., test 26 = `$1A`).
//! - On fail for sub-test N: `$F4 = 0x80 | N`. The SPC then spins.
//!
//! Our harness loads the binary into ARAM at `$0200`, points the SMP
//! at the entry, and runs for up to ~50 million SMP master cycles
//! (more than enough for even the longest PeterLemon test, which uses
//! ~256 ms timer waits between sub-tests). After the run we check
//! that `$F4` holds the LAST passing test's token - any failed test
//! would have written `$80|N` and stuck there.
//!
//! The `.spc` files are not vendored; we expect them at
//! `~/Git/snes-test-roms/PeterLemon/SNES-CPUTest-SPC700/<NAME>/<NAME>.spc`.
//! Override the base path via `SPC_TEST_ROMS_DIR` if your checkout is
//! elsewhere. Tests skip with a warning if the file is missing rather
//! than failing the suite; this keeps the regression gate green for
//! contributors who haven't cloned the test ROMs yet.

use std::path::PathBuf;

use vibenes::snes::ApuSubsystem;
use vibenes::snes::smp::harness::{load_raw_spc_image, run_smp_until_mailbox_byte};
use vibenes::snes::smp::ipl::Ipl;
use vibenes::snes::smp::state::ApuPorts;

const CYCLE_BUDGET: u64 = 50_000_000;

fn spc_path(name: &str) -> Option<PathBuf> {
    let base = std::env::var("SPC_TEST_ROMS_DIR").ok().map(PathBuf::from).or_else(|| {
        let home = std::env::var("HOME").ok()?;
        Some(
            PathBuf::from(home)
                .join("Git/snes-test-roms/PeterLemon/SNES-CPUTest-SPC700"),
        )
    })?;
    // PeterLemon ships two flavours:
    //
    //  1. `<base>/<short>/<NAME>.spc` (e.g. `ADC/SPC700ADC.spc`) - a
    //     RAW SPC binary, just the code starting at `$0200`. This is
    //     what bass's `output "...spc", create` + `seek(SPCRAM)` emits;
    //     it is what our harness expects and what we want to re-run
    //     for ISA validation (PC reset to $0200 = first sub-test).
    //
    //  2. `<base>/<NAME>.spc` (e.g. `SPC700ADC.spc`) - a STANDARD
    //     SNES-SPC700 sound file (256-byte header + 64 KiB ARAM dump
    //     + DSP state). Loading this raw at `$0200` would deposit the
    //     "SNES-SPC700 Sound File Data v0.30" ASCII header in ARAM
    //     and the SMP would execute it as opcodes - a catastrophic
    //     misload that produces wandering PC and stuck registers.
    //
    // We deliberately prefer the nested raw-binary path. The flat
    // path is left untouched for SPC music players (or future work
    // that parses the standard .spc header).
    let short = name.strip_prefix("SPC700").unwrap_or(name);
    let nested = base.join(short).join(format!("{name}.spc"));
    nested.is_file().then_some(nested)
}

/// Load `<NAME>/<NAME>.spc` and run the harness, asserting the
/// final mailbox handshake byte equals `expected_pass_token`. Skips
/// (with a warning) if the file isn't on disk.
fn run_test(name: &str, expected_pass_token: u8) {
    let path = match spc_path(name) {
        Some(p) => p,
        None => {
            eprintln!(
                "[skip] {name}: PeterLemon SPC700 .spc not found - clone \
                 https://github.com/PeterLemon/SNES (or set SPC_TEST_ROMS_DIR)"
            );
            return;
        }
    };
    let bytes = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));

    let mut apu = ApuSubsystem::new(Ipl::embedded());
    let mut ports = ApuPorts::RESET;
    load_raw_spc_image(&mut apu, &mut ports, &bytes, 0x0200);

    // Stop early if the mailbox shows a fail token (high bit set) or
    // the expected last-test pass token.
    let mut watch = vec![expected_pass_token];
    for n in 1..=expected_pass_token {
        watch.push(0x80 | n);
    }
    let observed = run_smp_until_mailbox_byte(&mut apu, &mut ports, CYCLE_BUDGET, &watch);

    if observed & 0x80 != 0 {
        let failing = observed & 0x7F;
        panic!(
            "{name}: sub-test {failing} failed (mailbox=${observed:02X}, \
             cycles={}, PC=${:04X}, A=${:02X}, X=${:02X}, Y=${:02X}, \
             PSW=${:02X}, $E0=${:02X}, $E1=${:02X})",
            apu.cycles,
            apu.smp.pc,
            apu.smp.a,
            apu.smp.x,
            apu.smp.y,
            apu.smp.psw.pack(),
            apu.aram[0xE0],
            apu.aram[0xE1],
        );
    }
    assert_eq!(
        observed, expected_pass_token,
        "{name}: harness did not reach final pass token within {CYCLE_BUDGET} cycles \
         (last mailbox byte ${observed:02X}, PC=${:04X}, cycles consumed {})",
        apu.smp.pc, apu.cycles,
    );
}

#[test]
fn peterlemon_spc700_adc() {
    // 26 sub-tests; final pass token = $1A.
    run_test("SPC700ADC", 0x1A);
}

// The remaining tests have known sub-test counts from inspecting each
// `<dir>/<NAME>_spc.asm`'s last `Pass<N>` label. If the count is wrong
// the test will report `harness did not reach final pass token` rather
// than a genuine ISA failure.

#[test]
fn peterlemon_spc700_and() {
    // AND test count = 24 (last pass label Pass24 -> token $18).
    run_test("SPC700AND", 0x18);
}

#[test]
fn peterlemon_spc700_dec() {
    // DEC test count = 8 (memory + register decrements).
    run_test("SPC700DEC", 0x08);
}

#[test]
fn peterlemon_spc700_eor() {
    run_test("SPC700EOR", 0x18);
}

#[test]
fn peterlemon_spc700_inc() {
    run_test("SPC700INC", 0x08);
}

#[test]
fn peterlemon_spc700_ora() {
    run_test("SPC700ORA", 0x18);
}

#[test]
fn peterlemon_spc700_sbc() {
    run_test("SPC700SBC", 0x1A);
}
