//! End-to-end battery-save persistence.
//!
//! Builds a synthetic battery-backed NROM in a tempdir, writes bytes
//! to PRG-RAM via the bus, saves, drops the Nes, and reloads —
//! verifying the bytes come back verbatim through a fresh mapper
//! instance. Also asserts the save file lands next to the ROM with
//! the `.sav` extension and that non-battery carts never write one.

use std::fs;
use std::path::{Path, PathBuf};

use vibenes::config::{SaveConfig, SaveStyle};
use vibenes::nes::Nes;
use vibenes::rom::Cartridge;

fn tempdir(label: &str) -> PathBuf {
    let base = std::env::temp_dir();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = base.join(format!("vibenes-bat-{label}-{}-{stamp}", std::process::id()));
    fs::create_dir(&p).expect("mkdir tempdir");
    p
}

fn cleanup(p: &Path) {
    let _ = fs::remove_dir_all(p);
}

/// Hand-roll a minimal NROM iNES ROM: 32 KiB PRG filled with NOP
/// ($EA), 8 KiB CHR zeroed, reset vector pointing into the NOP sled.
/// `battery` sets flag6 bit 1 and header[8]=1 (8 KiB PRG-RAM) so the
/// NROM mapper allocates a RAM window and the cart reports
/// `battery_backed=true` through the save pipeline.
fn build_nes_file(dir: &Path, name: &str, battery: bool) -> PathBuf {
    let mut bytes = Vec::with_capacity(16 + 32 * 1024 + 8 * 1024);
    // Header (16 bytes).
    bytes.extend_from_slice(b"NES\x1A");
    bytes.push(2); // 2 × 16 KiB PRG = 32 KiB
    bytes.push(1); // 1 × 8 KiB CHR
    bytes.push(if battery { 0x02 } else { 0x00 }); // flag6: battery bit
    bytes.push(0x00); // flag7
    bytes.push(1); // PRG-RAM: 1 × 8 KiB
    bytes.extend_from_slice(&[0u8; 7]); // remaining bytes zero — iNES 1.0.

    // 32 KiB PRG: all NOPs, reset vector at $FFFC/$FFFD = $8000.
    let mut prg = vec![0xEAu8; 32 * 1024];
    // prg[0x7FFC] = $FFFC offset. Vector value = $8000 → lo=$00, hi=$80.
    prg[0x7FFC] = 0x00;
    prg[0x7FFD] = 0x80;
    bytes.extend_from_slice(&prg);

    // 8 KiB CHR (zero).
    bytes.extend_from_slice(&[0u8; 8 * 1024]);

    let path = dir.join(name);
    fs::write(&path, &bytes).expect("write nes");
    path
}

/// Helper: load a ROM, attach save metadata, return the Nes.
/// Mirrors what `App::load_rom` does, minus the windowed-app glue.
fn load(path: &Path) -> Nes {
    let cart = Cartridge::load(path).expect("load cart");
    let crc = cart.prg_chr_crc32;
    let mut nes = Nes::from_cartridge(cart).expect("build nes");
    nes.attach_save_metadata(path.to_path_buf(), crc);
    nes
}

#[test]
fn battery_write_persists_across_reload() {
    let dir = tempdir("persist");
    let rom = build_nes_file(&dir, "game.nes", true);
    let sav = dir.join("game.sav");
    let cfg = SaveConfig::default();

    // 1. Load + populate PRG-RAM with a distinctive pattern.
    {
        let mut nes = load(&rom);
        let initial_loaded = nes.load_battery(&cfg).expect("load");
        assert!(!initial_loaded, "first load has no prior save");

        // Poke 256 distinctive bytes across $6000-$60FF.
        for i in 0..0x100u16 {
            nes.bus.write(0x6000 + i, (i as u8).wrapping_add(0x33));
        }
        // Verify writes stuck via the bus.
        for i in 0..0x100u16 {
            assert_eq!(nes.bus.peek(0x6000 + i), (i as u8).wrapping_add(0x33));
        }

        // Save.
        let saved = nes.save_battery(&cfg).expect("save");
        assert!(saved, "save should return Ok(true) after dirty writes");
    }

    // 2. File exists and has the expected shape.
    assert!(sav.exists(), "expected save file at {}", sav.display());
    let bytes = fs::read(&sav).expect("read sav");
    assert_eq!(bytes.len(), 8 * 1024, "NROM save is 8 KiB (matches PRG-RAM window)");
    for i in 0..0x100 {
        assert_eq!(bytes[i], (i as u8).wrapping_add(0x33));
    }

    // 3. Fresh Nes: pattern must come back through load_battery.
    {
        let mut nes = load(&rom);
        let loaded = nes.load_battery(&cfg).expect("load");
        assert!(loaded, "second load must find the prior save");
        for i in 0..0x100u16 {
            assert_eq!(
                nes.bus.peek(0x6000 + i),
                (i as u8).wrapping_add(0x33),
                "byte {i:#06X}"
            );
        }
    }

    cleanup(&dir);
}

#[test]
fn non_battery_cart_does_not_write_a_save_file() {
    let dir = tempdir("nobat");
    let rom = build_nes_file(&dir, "demo.nes", false);
    let sav = dir.join("demo.sav");
    let cfg = SaveConfig::default();

    let mut nes = load(&rom);
    // Even a write through the bus must not create a save file.
    nes.bus.write(0x6000, 0xAB);
    let saved = nes.save_battery(&cfg).expect("save");
    assert!(!saved, "non-battery carts must not persist RAM");
    assert!(!sav.exists(), "no save file should exist for non-battery cart");

    cleanup(&dir);
}

#[test]
fn save_is_skipped_when_ram_is_untouched() {
    // Gate to keep disk quiet on idle: if the cart was loaded and
    // never written, save_battery returns Ok(false) and no file is
    // created. Important for the autosave-every-N-frames path.
    let dir = tempdir("clean");
    let rom = build_nes_file(&dir, "clean.nes", true);
    let sav = dir.join("clean.sav");
    let cfg = SaveConfig::default();

    let mut nes = load(&rom);
    let _ = nes.load_battery(&cfg).expect("load");
    // No writes → save_dirty is false → save_battery is a no-op.
    let saved = nes.save_battery(&cfg).expect("save");
    assert!(!saved);
    assert!(!sav.exists(), "idle cart must not have produced a save file");

    cleanup(&dir);
}

#[test]
fn by_crc_save_style_still_resolves_today() {
    // Placeholder: `SaveStyle::ByCrc` currently falls back to the
    // next-to-ROM path (see src/save.rs). When the data-dir
    // resolution lands this test tightens to assert the CRC-keyed
    // path instead. Keeps behavior pinned during the interim.
    let dir = tempdir("crc");
    let rom = build_nes_file(&dir, "crc.nes", true);
    let cfg = SaveConfig {
        style: SaveStyle::ByCrc,
        ..SaveConfig::default()
    };
    let mut nes = load(&rom);
    nes.bus.write(0x6000, 0x42);
    assert!(nes.save_battery(&cfg).expect("save"));
    assert!(dir.join("crc.sav").exists());

    cleanup(&dir);
}
