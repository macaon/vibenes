// SPDX-License-Identifier: GPL-3.0-or-later
//! End-to-end battery-save persistence.
//!
//! Builds a synthetic battery-backed NROM in a tempdir, writes bytes
//! to PRG-RAM via the bus, saves, drops the Nes, and reloads -
//! verifying the bytes come back verbatim through a fresh mapper
//! instance. Also asserts non-battery carts never write a save file.
//!
//! Each test uses an explicit `SaveStyle` + `dir_override` so the
//! default (`ConfigDir`) doesn't write to the user's real
//! `~/.config/vibenes/saves/`.

use std::fs;
use std::path::{Path, PathBuf};

use vibenes::config::{SaveConfig, SaveStyle};
use vibenes::nes::Nes;
use vibenes::nes::rom::Cartridge;

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
    bytes.extend_from_slice(&[0u8; 7]); // remaining bytes zero - iNES 1.0.

    // 32 KiB PRG: all NOPs, reset vector at $FFFC/$FFFD = $8000.
    let mut prg = vec![0xEAu8; 32 * 1024];
    prg[0x7FFC] = 0x00;
    prg[0x7FFD] = 0x80;
    bytes.extend_from_slice(&prg);

    // 8 KiB CHR (zero).
    bytes.extend_from_slice(&[0u8; 8 * 1024]);

    let path = dir.join(name);
    fs::write(&path, &bytes).expect("write nes");
    path
}

fn load(path: &Path) -> Nes {
    let cart = Cartridge::load(path).expect("load cart");
    let crc = cart.prg_chr_crc32;
    let mut nes = Nes::from_cartridge(cart).expect("build nes");
    nes.attach_save_metadata(path.to_path_buf(), crc);
    nes
}

/// `SaveConfig` routed at a tempdir so the test doesn't pollute the
/// user's real config dir.
fn cfg_for(style: SaveStyle, dir: &Path) -> SaveConfig {
    SaveConfig {
        style,
        dir_override: Some(dir.to_path_buf()),
        ..SaveConfig::default()
    }
}

#[test]
fn battery_write_persists_across_reload_config_dir() {
    let dir = tempdir("persist-cfg");
    let rom_dir = dir.join("roms");
    let save_dir = dir.join("saves");
    fs::create_dir_all(&rom_dir).unwrap();
    fs::create_dir_all(&save_dir).unwrap();
    let rom = build_nes_file(&rom_dir, "game.nes", true);
    let cfg = cfg_for(SaveStyle::ConfigDir, &save_dir);
    let expected_sav = save_dir.join("game.sav");

    {
        let mut nes = load(&rom);
        assert!(!nes.load_battery(&cfg).expect("load"));
        for i in 0..0x100u16 {
            nes.bus.write(0x6000 + i, (i as u8).wrapping_add(0x33));
        }
        assert!(nes.save_battery(&cfg).expect("save"));
    }

    assert!(
        expected_sav.exists(),
        "config-dir save lives at {}",
        expected_sav.display()
    );
    let bytes = fs::read(&expected_sav).unwrap();
    assert_eq!(bytes.len(), 8 * 1024);
    for i in 0..0x100 {
        assert_eq!(bytes[i], (i as u8).wrapping_add(0x33));
    }

    {
        let mut nes = load(&rom);
        assert!(nes.load_battery(&cfg).expect("reload"));
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
fn battery_write_persists_next_to_rom_when_selected() {
    // Same roundtrip but with the opt-in `NextToRom` style. Confirms
    // the legacy placement still works when explicitly requested
    // (future settings UI toggle).
    let dir = tempdir("persist-next");
    let rom = build_nes_file(&dir, "game.nes", true);
    let sav = dir.join("game.sav");
    let cfg = SaveConfig {
        style: SaveStyle::NextToRom,
        ..SaveConfig::default()
    };

    {
        let mut nes = load(&rom);
        assert!(!nes.load_battery(&cfg).expect("load"));
        nes.bus.write(0x6000, 0x77);
        assert!(nes.save_battery(&cfg).expect("save"));
    }
    assert!(sav.exists());

    {
        let mut nes = load(&rom);
        assert!(nes.load_battery(&cfg).expect("reload"));
        assert_eq!(nes.bus.peek(0x6000), 0x77);
    }

    cleanup(&dir);
}

#[test]
fn non_battery_cart_does_not_write_a_save_file() {
    let dir = tempdir("nobat");
    let save_dir = dir.join("saves");
    fs::create_dir_all(&save_dir).unwrap();
    let rom = build_nes_file(&dir, "demo.nes", false);
    let cfg = cfg_for(SaveStyle::ConfigDir, &save_dir);

    let mut nes = load(&rom);
    nes.bus.write(0x6000, 0xAB);
    assert!(!nes.save_battery(&cfg).expect("save"));
    assert!(
        save_dir.read_dir().unwrap().next().is_none(),
        "save dir must stay empty for non-battery cart"
    );

    cleanup(&dir);
}

#[test]
fn save_is_skipped_when_ram_is_untouched() {
    let dir = tempdir("clean");
    let save_dir = dir.join("saves");
    fs::create_dir_all(&save_dir).unwrap();
    let rom = build_nes_file(&dir, "clean.nes", true);
    let cfg = cfg_for(SaveStyle::ConfigDir, &save_dir);

    let mut nes = load(&rom);
    let _ = nes.load_battery(&cfg).expect("load");
    assert!(!nes.save_battery(&cfg).expect("save"));
    assert!(
        save_dir.read_dir().unwrap().next().is_none(),
        "idle cart must not have produced a save file"
    );

    cleanup(&dir);
}

#[test]
fn by_crc_style_uses_crc_hex_filename() {
    let dir = tempdir("crc");
    let save_dir = dir.join("saves");
    fs::create_dir_all(&save_dir).unwrap();
    let rom = build_nes_file(&dir, "whatever.nes", true);
    let cfg = cfg_for(SaveStyle::ByCrc, &save_dir);

    let cart = Cartridge::load(&rom).expect("load cart");
    let crc = cart.prg_chr_crc32;
    let expected = save_dir.join(format!("{crc:08X}.sav"));

    let mut nes = load(&rom);
    nes.bus.write(0x6000, 0x42);
    assert!(nes.save_battery(&cfg).expect("save"));
    assert!(expected.exists(), "by-crc save lives at {}", expected.display());

    cleanup(&dir);
}
