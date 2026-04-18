//! Integration tests for the pre-$6000 blargg APU suite. Drives each
//! ROM through `Nes` in-process and uses `blargg_2005_scan` to pull
//! the result code off the nametable — no subprocess, no
//! `blargg_2005_report` binary involved. Keeps the regression gate
//! self-contained.
//!
//! ROMs 01–07 currently pass our APU and are active tests. 08–11 fail
//! and are marked `#[ignore]` until their respective Phase 3 fixes
//! land (`cargo test --test blargg_apu_2005 -- --include-ignored`
//! will run them to see the current failure mode).

use std::path::PathBuf;

use vibenes::blargg_2005_scan::{
    extract_result_code, nametable_has_text, read_nametable_ascii, StuckPcDetector,
};
use vibenes::nes::Nes;
use vibenes::rom::Cartridge;

const POLL_INTERVAL_CYCLES: u64 = 10_000;
const CYCLE_LIMIT: u64 = 200_000_000;

fn rom_path(name: &str) -> PathBuf {
    let home = std::env::var("HOME").expect("HOME must be set to locate test ROMs");
    PathBuf::from(home).join("Git/nes-test-roms/blargg_apu_2005.07.30").join(name)
}

fn run_rom(name: &str) -> (Option<u8>, String) {
    let path = rom_path(name);
    if !path.exists() {
        panic!("missing test ROM: {} — clone https://github.com/christopherpow/nes-test-roms", path.display());
    }
    let cart = Cartridge::load(&path).expect("load cartridge");
    let mut nes = Nes::from_cartridge(cart).expect("construct Nes");

    let mut detector = StuckPcDetector::new();
    let start = nes.bus.clock.cpu_cycles();
    loop {
        let elapsed = nes.bus.clock.cpu_cycles() - start;
        assert!(
            elapsed < CYCLE_LIMIT,
            "{name}: cycle limit exceeded before nametable settled"
        );
        nes.run_cycles(POLL_INTERVAL_CYCLES)
            .unwrap_or_else(|e| panic!("{name}: CPU error: {e}"));
        assert!(!nes.cpu.halted, "{name}: CPU halted");
        if detector.observe(nes.cpu.pc) && nametable_has_text(&nes) {
            let text = read_nametable_ascii(&nes);
            return (extract_result_code(&text), text);
        }
    }
}

fn assert_pass(name: &str) {
    let (code, text) = run_rom(name);
    assert_eq!(
        code,
        Some(1),
        "{name}: expected result code 1 (PASS), got {:?}\n--- nametable ---\n{text}",
        code
    );
}

#[test]
fn rom_01_len_ctr() {
    assert_pass("01.len_ctr.nes");
}

#[test]
fn rom_02_len_table() {
    assert_pass("02.len_table.nes");
}

#[test]
fn rom_03_irq_flag() {
    assert_pass("03.irq_flag.nes");
}

#[test]
fn rom_04_clock_jitter() {
    assert_pass("04.clock_jitter.nes");
}

#[test]
fn rom_05_len_timing_mode0() {
    assert_pass("05.len_timing_mode0.nes");
}

#[test]
fn rom_06_len_timing_mode1() {
    assert_pass("06.len_timing_mode1.nes");
}

#[test]
fn rom_07_irq_flag_timing() {
    assert_pass("07.irq_flag_timing.nes");
}

#[test]
#[ignore = "phase 3.1 — irq dispatch arrives 1 cycle late (reports code=2 'too soon')"]
fn rom_08_irq_timing() {
    assert_pass("08.irq_timing.nes");
}

#[test]
#[ignore = "phase 3.2 — power-on $4017 pre-write offset off (reports code=4 'fourth step too late')"]
fn rom_09_reset_timing() {
    assert_pass("09.reset_timing.nes");
}

#[test]
#[ignore = "phase 3.3 — halt write applied before same-cycle length clock (reports code=3)"]
fn rom_10_len_halt_timing() {
    assert_pass("10.len_halt_timing.nes");
}

#[test]
#[ignore = "phase 3.4 — length reload not deferred past same-cycle length clock (reports code=3)"]
fn rom_11_len_reload_timing() {
    assert_pass("11.len_reload_timing.nes");
}
