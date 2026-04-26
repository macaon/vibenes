// SPDX-License-Identifier: GPL-3.0-or-later
//! Headless SNES header probe. Takes one or more ROM paths on the
//! command line, runs each through `core::system::detect_system` and,
//! if SNES, parses the cartridge and prints the one-line summary.
//! Used to spot-check the header detector against commercial dumps
//! before the SNES core is wired into the windowed binary.

use std::path::Path;
use std::process::ExitCode;

use vibenes::core::system::{detect_system, System};
use vibenes::snes::rom::Cartridge;

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: snes_probe <rom> [<rom>...]");
        return ExitCode::from(2);
    }
    let mut any_failed = false;
    for arg in args {
        let path = Path::new(&arg);
        match detect_system(path) {
            Ok(System::Nes) => {
                println!("{}: NES (skipped - this tool probes SNES headers)", path.display());
            }
            Ok(System::Snes) => match Cartridge::load(path) {
                Ok(cart) => println!("{}: {}", path.display(), cart.describe()),
                Err(e) => {
                    eprintln!("{}: SNES detect ok but parse failed: {e:#}", path.display());
                    any_failed = true;
                }
            },
            Err(e) => {
                eprintln!("{}: {e:#}", path.display());
                any_failed = true;
            }
        }
    }
    if any_failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}
