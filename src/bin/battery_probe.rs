//! Diagnostic: load a cart, poke PRG-RAM at `$6000`, save, reload,
//! verify. Tells us whether the save pipeline works for a specific
//! ROM file without needing the user to run the windowed app.
//!
//! Usage: `battery_probe <rom.nes>`

use std::env;
use std::process::ExitCode;

use vibenes::config::SaveConfig;
use vibenes::nes::Nes;
use vibenes::rom::Cartridge;

fn main() -> ExitCode {
    let Some(path) = env::args().nth(1) else {
        eprintln!("usage: battery_probe <rom.nes>");
        return ExitCode::from(2);
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
        println!("NOT battery-backed — saves will never be created for this cart.");
        return ExitCode::from(0);
    }
    let crc = cart.prg_chr_crc32;

    let cfg = SaveConfig::default();
    let probe_bytes: [u8; 4] = [0xDE, 0xAD, 0xBE, 0xEF];

    // Phase 1: load, write probe bytes, save.
    {
        let mut nes = match Nes::from_cartridge(cart) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("build error: {e:#}");
                return ExitCode::from(1);
            }
        };
        nes.attach_save_metadata(path.clone(), crc);
        let sp = nes.save_path(&cfg).expect("save_path");
        println!("save path: {}", sp.display());

        let loaded = nes.load_battery(&cfg).expect("load");
        println!("existing save loaded? {loaded}");
        println!("save_data present? {}", nes.bus.mapper.save_data().is_some());
        println!(
            "save_data len: {:?}",
            nes.bus.mapper.save_data().map(|d| d.len())
        );

        // Peek what was at $6000 (either loaded-save bytes or fresh zeros).
        let before: Vec<u8> = (0..4).map(|i| nes.bus.peek(0x6000 + i)).collect();
        println!("before write: {before:02X?}");

        // Poke the probe bytes via the bus.
        for (i, b) in probe_bytes.iter().enumerate() {
            nes.bus.write(0x6000 + i as u16, *b);
        }
        let after: Vec<u8> = (0..4).map(|i| nes.bus.peek(0x6000 + i)).collect();
        println!("after write:  {after:02X?}");
        println!("save_dirty: {}", nes.bus.mapper.save_dirty());

        let saved = nes.save_battery(&cfg).expect("save");
        println!("save_battery returned: {saved} (true = file written)");
    }

    // Phase 2: fresh load, verify bytes persist.
    {
        let cart = Cartridge::load(&path).expect("reload cart");
        let mut nes = Nes::from_cartridge(cart).expect("rebuild nes");
        nes.attach_save_metadata(path.clone(), crc);
        let loaded = nes.load_battery(&cfg).expect("reload save");
        println!("phase 2 — existing save loaded? {loaded}");
        let roundtrip: Vec<u8> = (0..4).map(|i| nes.bus.peek(0x6000 + i)).collect();
        println!("roundtrip:    {roundtrip:02X?}");
        if &roundtrip[..] == &probe_bytes[..] {
            println!("OK — save pipeline works end-to-end on this ROM.");
            ExitCode::from(0)
        } else {
            println!("FAIL — expected {probe_bytes:02X?}, got {roundtrip:02X?}");
            ExitCode::from(1)
        }
    }
}
