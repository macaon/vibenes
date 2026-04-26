// SPDX-License-Identifier: GPL-3.0-or-later
//! Headless runner for blargg's pre-$6000 APU suite
//! (`blargg_apu_2005.07.30/*.nes`). These ROMs report via on-screen
//! text + APU beeps only, so the `test_runner` signature handshake
//! never fires. We instead watch the CPU PC: once it's trapped in a
//! tight `forever:` loop we scan nametable 0 for the result.
//!
//! Usage: `blargg_2005_report <rom.nes> [<rom.nes> ...]`. Exits 0 iff
//! every ROM reports result code 1 (blargg's universal "passed" code);
//! prints the scanned text on failure so the specific fault code
//! (2 = "too soon", 3 = "too late", etc.) is visible in CI logs.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use vibenes::blargg_2005_scan::{
    extract_result_code, has_result_marker, read_nametable_ascii, StuckPcDetector,
};
use vibenes::nes::Nes;
use vibenes::nes::rom::Cartridge;

const POLL_INTERVAL_CYCLES: u64 = 10_000;
const CYCLE_LIMIT_DEFAULT: u64 = 200_000_000; // ~1 min NTSC

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: blargg_2005_report <rom.nes> [<rom.nes> ...]");
        return ExitCode::from(2);
    }
    let mut overall = 0u8;
    for arg in args {
        let path = PathBuf::from(arg);
        match run_one(&path, CYCLE_LIMIT_DEFAULT) {
            Ok(outcome) => {
                println!("{}: {}", path.display(), outcome.summary);
                if !outcome.passed {
                    overall = 1;
                }
                // Always dump the nametable if `VERBOSE=1`, otherwise
                // only on failure - useful for debugging tests that
                // print raw data without a pass/fail keyword.
                let verbose = std::env::var_os("VERBOSE").is_some();
                if !outcome.transcript.is_empty() && (!outcome.passed || verbose) {
                    eprintln!("--- nametable dump ---");
                    eprintln!("{}", outcome.transcript);
                    eprintln!("----------------------");
                }
            }
            Err(e) => {
                eprintln!("{}: ERROR: {:#}", path.display(), e);
                overall = 1;
            }
        }
    }
    ExitCode::from(overall)
}

struct Outcome {
    passed: bool,
    summary: String,
    transcript: String,
}

fn run_one(rom_path: &Path, cycle_limit: u64) -> Result<Outcome> {
    let cart = Cartridge::load(rom_path)
        .with_context(|| format!("loading {}", rom_path.display()))?;
    eprintln!("{}: {}", rom_path.display(), cart.describe());
    let mut nes = Nes::from_cartridge(cart)?;

    let mut detector = StuckPcDetector::new();
    let start_cycles = nes.bus.clock.cpu_cycles();
    loop {
        let elapsed = nes.bus.clock.cpu_cycles() - start_cycles;
        if elapsed > cycle_limit {
            let text = read_nametable_ascii(&nes);
            return Ok(Outcome {
                passed: false,
                summary: format!("TIMEOUT after {} cycles", elapsed),
                transcript: text,
            });
        }

        if let Err(msg) = nes.run_cycles(POLL_INTERVAL_CYCLES) {
            return Ok(Outcome {
                passed: false,
                summary: format!("HALT ({})", msg),
                transcript: read_nametable_ascii(&nes),
            });
        }
        if nes.cpu.halted {
            let reason = nes
                .cpu
                .halt_reason
                .clone()
                .unwrap_or_else(|| "halted".to_string());
            return Ok(Outcome {
                passed: false,
                summary: format!("HALT ({})", reason),
                transcript: read_nametable_ascii(&nes),
            });
        }

        let pc_stuck = detector.observe(nes.cpu.pc);
        if pc_stuck {
            let text = read_nametable_ascii(&nes);
            // Gate on a recognized marker - otherwise a long test like
            // `cpu_timing_test6` fires the stuck-PC heuristic during
            // its 16-second NMI-wait loop while only the "6502 TIMING
            // TEST" header is on screen, and the first-digit fallback
            // in `extract_result_code` would mis-report `6`.
            if !has_result_marker(&text) {
                continue;
            }
            let code = extract_result_code(&text);
            let passed = code == Some(1);
            let summary = match code {
                Some(c) => format!(
                    "{} code={} at cycle {}",
                    if passed { "PASS" } else { "FAIL" },
                    c,
                    elapsed
                ),
                None => format!("UNKNOWN (no digit on screen) at cycle {}", elapsed),
            };
            return Ok(Outcome {
                passed,
                summary,
                transcript: text,
            });
        }
    }
}
