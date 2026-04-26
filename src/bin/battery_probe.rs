// SPDX-License-Identifier: GPL-3.0-or-later
//! Diagnostic: load a cart, optionally run the CPU for N emulated
//! seconds, then dump PRG-RAM state + save-dirty flag + trigger the
//! save pipeline. Lets us verify end-to-end that:
//!   1. The save path resolves where we expect.
//!   2. The mapper returns correct `save_data`.
//!   3. PRG-RAM writes happen when they should (via real CPU
//!      execution) and set `save_dirty`.
//!   4. The atomic write + reload roundtrip preserves bytes.
//!
//! Usage:
//!   battery_probe <rom.nes>                   # offline poke test
//!   battery_probe <rom.nes> run <seconds>     # run CPU for N sec
//!                                             # then dump PRG-RAM

use std::env;
use std::process::ExitCode;

use vibenes::config::SaveConfig;
use vibenes::nes::Nes;
use vibenes::rom::Cartridge;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let Some(path) = args.get(1).cloned() else {
        eprintln!("usage: battery_probe <rom.nes> [run <seconds>]");
        return ExitCode::from(2);
    };
    let run_seconds: Option<u64> = match args.get(2).map(|s| s.as_str()) {
        Some("run") => match args.get(3).and_then(|s| s.parse().ok()) {
            Some(n) => Some(n),
            None => {
                eprintln!("usage: battery_probe <rom.nes> run <seconds>");
                return ExitCode::from(2);
            }
        },
        None => None,
        Some(other) => {
            eprintln!("unexpected arg {other:?}; try `run <seconds>`");
            return ExitCode::from(2);
        }
    };
    let path = std::path::PathBuf::from(path);

    let cart = match Cartridge::load(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("load error: {e:#}");
            return ExitCode::from(1);
        }
    };
    println!("{}", cart.describe());
    if !cart.battery_backed {
        println!("NOT battery-backed - saves will never be created for this cart.");
        return ExitCode::from(0);
    }
    let crc = cart.prg_chr_crc32;
    let cfg = SaveConfig::default();

    let mut nes = match Nes::from_cartridge(cart) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("build error: {e:#}");
            return ExitCode::from(1);
        }
    };
    nes.attach_save_metadata(path.clone(), crc);
    println!("save path: {}", nes.save_path(&cfg).unwrap().display());

    if let Some(secs) = run_seconds {
        // NTSC CPU ~ 1.789773 MHz. Run that many cycles × seconds.
        let cycles = 1_789_773 * secs;
        println!("running {secs} emulated second(s) = {cycles} CPU cycles…");
        if let Err(e) = nes.run_cycles(cycles) {
            eprintln!("CPU halted: {e}");
            return ExitCode::from(1);
        }
        // Scan prg_ram for any non-zero byte (via save_data snapshot).
        if let Some(ram) = nes.bus.mapper.save_data() {
            let nonzero = ram.iter().filter(|&&b| b != 0).count();
            let first_nz = ram.iter().position(|&b| b != 0);
            println!(
                "after run: prg_ram size={} nonzero_bytes={} first_nonzero_offset={:?}",
                ram.len(),
                nonzero,
                first_nz
            );
            if nonzero > 0 {
                // Print first 32 bytes so we can see what's there.
                let head: Vec<String> = ram[..32.min(ram.len())]
                    .iter()
                    .map(|b| format!("{b:02X}"))
                    .collect();
                println!("prg_ram[0..32] = {}", head.join(" "));
            }
        }
        println!("save_dirty (after run): {}", nes.bus.mapper.save_dirty());
        let saved = nes.save_battery(&cfg).expect("save");
        println!("save_battery returned: {saved} (true = file written)");
        return ExitCode::from(0);
    }

    // Offline poke test: direct bus.write, no CPU execution.
    let probe: [u8; 4] = [0xDE, 0xAD, 0xBE, 0xEF];
    let before: Vec<u8> = (0..4).map(|i| nes.bus.peek(0x6000 + i)).collect();
    println!("before write: {before:02X?}");
    for (i, b) in probe.iter().enumerate() {
        nes.bus.write(0x6000 + i as u16, *b);
    }
    let after: Vec<u8> = (0..4).map(|i| nes.bus.peek(0x6000 + i)).collect();
    println!("after write:  {after:02X?}  save_dirty={}", nes.bus.mapper.save_dirty());
    let saved = nes.save_battery(&cfg).expect("save");
    println!("save_battery returned: {saved}");

    let cart = Cartridge::load(&path).expect("reload cart");
    let mut n2 = Nes::from_cartridge(cart).expect("rebuild");
    n2.attach_save_metadata(path.clone(), crc);
    let loaded = n2.load_battery(&cfg).expect("reload");
    println!("phase 2 - existing save loaded? {loaded}");
    let roundtrip: Vec<u8> = (0..4).map(|i| n2.bus.peek(0x6000 + i)).collect();
    println!("roundtrip: {roundtrip:02X?}");
    if &roundtrip[..] == &probe[..] {
        println!("OK - offline pipeline works.");
        ExitCode::from(0)
    } else {
        println!("FAIL");
        ExitCode::from(1)
    }
}
