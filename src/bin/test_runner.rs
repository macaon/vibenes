//! Headless blargg test runner. Loads a cartridge, runs the CPU, and
//! watches the battery-backed PRG-RAM at $6000 for the standard status
//! handshake:
//!   $6001-$6003 must read "$DE $B0 $61" before $6000 is meaningful
//!   $6000 == $80 : still running
//!   $6000 == $81 : test requests a reset after ~100ms
//!   otherwise    : final result code (0 = pass, nonzero = fail number)
//! The ASCII message at $6004.. is printed on completion.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use vibenes::nes::Nes;
use vibenes::rom::Cartridge;

const SIGNATURE: [u8; 3] = [0xDE, 0xB0, 0x61];
const POLL_INTERVAL_CYCLES: u64 = 10_000;
const CYCLE_LIMIT_DEFAULT: u64 = 200_000_000; // ~1min of emulated NTSC CPU time

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: vibenes-test <rom.nes> [<rom.nes> ...]");
        return ExitCode::from(2);
    }

    let mut overall_exit = 0u8;
    for arg in args {
        let path = PathBuf::from(arg);
        match run_one(&path, CYCLE_LIMIT_DEFAULT) {
            Ok(outcome) => {
                println!("{}: {}", path.display(), outcome.summary);
                if !outcome.passed {
                    overall_exit = 1;
                }
            }
            Err(e) => {
                eprintln!("{}: ERROR: {:#}", path.display(), e);
                overall_exit = 1;
            }
        }
    }
    ExitCode::from(overall_exit)
}

struct Outcome {
    passed: bool,
    summary: String,
}

fn run_one(rom_path: &Path, cycle_limit: u64) -> Result<Outcome> {
    let cart = Cartridge::load(rom_path)
        .with_context(|| format!("loading {}", rom_path.display()))?;
    eprintln!("{}: {}", rom_path.display(), cart.describe());
    let mut nes = Nes::from_cartridge(cart)?;

    let mut signature_seen = false;
    let start_cycles = nes.bus.clock.cpu_cycles();
    // Blargg's reset protocol: when the test writes $81 to $6000, the
    // emulator should wait ~100ms and then toggle the Reset button. We
    // approximate 100ms of emulated time with ~180k CPU cycles on NTSC.
    const RESET_DELAY_CYCLES: u64 = 180_000;
    // After dispatching a reset, $6000 still holds the pre-reset $81
    // because RAM/PRG-RAM is preserved — this is real hardware
    // behavior. The test's reset handler only writes a fresh status
    // once its own pre-measurement delay finishes (often tens of
    // thousands of cycles), so ignore $81 until we observe either
    // $80 (running again) or a new result code.
    let mut ignore_reset_until_running = false;
    loop {
        let elapsed = nes.bus.clock.cpu_cycles() - start_cycles;
        if elapsed > cycle_limit {
            bail!("cycle limit exceeded after {} CPU cycles", elapsed);
        }

        if let Err(msg) = nes.run_cycles(POLL_INTERVAL_CYCLES) {
            return Ok(Outcome {
                passed: false,
                summary: format!("HALT ({})", msg),
            });
        }
        if nes.cpu.halted {
            let reason = nes
                .cpu
                .halt_reason
                .clone()
                .unwrap_or_else(|| "halted with no reason".to_string());
            return Ok(Outcome {
                passed: false,
                summary: format!("HALT ({})", reason),
            });
        }

        if !signature_seen {
            let sig = [
                nes.bus.peek(0x6001),
                nes.bus.peek(0x6002),
                nes.bus.peek(0x6003),
            ];
            if sig == SIGNATURE {
                signature_seen = true;
                eprintln!(
                    "{}: signature seen at cycle {}",
                    rom_path.display(),
                    elapsed
                );
            } else {
                continue;
            }
        }

        let status = nes.bus.peek(0x6000);
        match status {
            0x80 => {
                ignore_reset_until_running = false;
                continue;
            }
            0x81 if ignore_reset_until_running => continue,
            0x81 => {
                eprintln!(
                    "{}: test requested reset at cycle {}",
                    rom_path.display(),
                    elapsed
                );
                // Run ~100 ms of emulated time first so pre-reset code
                // (length-counter drains, frame IRQ assertions, etc.)
                // actually happens before we yank the Reset line.
                if let Err(msg) = nes.run_cycles(RESET_DELAY_CYCLES) {
                    return Ok(Outcome {
                        passed: false,
                        summary: format!("HALT during reset delay ({})", msg),
                    });
                }
                nes.reset();
                ignore_reset_until_running = true;
                eprintln!(
                    "{}: reset dispatched, resuming at cycle {}",
                    rom_path.display(),
                    nes.bus.clock.cpu_cycles()
                );
                continue;
            }
            code => {
                let message = read_message(&nes);
                let passed = code == 0;
                return Ok(Outcome {
                    passed,
                    summary: format!(
                        "{} code={} msg={:?}",
                        if passed { "PASS" } else { "FAIL" },
                        code,
                        message.trim()
                    ),
                });
            }
        }
    }
}

fn read_message(nes: &Nes) -> String {
    let mut buf = Vec::new();
    for i in 0..0x1000u16 {
        let addr = 0x6004u16.wrapping_add(i);
        let b = nes.bus.peek(addr);
        if b == 0 {
            break;
        }
        buf.push(b);
    }
    String::from_utf8_lossy(&buf).into_owned()
}
