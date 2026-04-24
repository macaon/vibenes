// SPDX-License-Identifier: GPL-3.0-or-later
//! Quick probe — runs `dma_4016_read.nes` to completion and prints
//! the result line from the nametable. Used for verifying the
//! `08 08 07 08 08` golden CRC `F0AB808C` after DMA-timing changes.

use std::path::PathBuf;

use vibenes::blargg_2005_scan::{nametable_has_text, read_nametable_ascii, StuckPcDetector};
use vibenes::nes::Nes;
use vibenes::rom::Cartridge;

fn main() {
    let rom = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "dma_4016_read.nes".to_string());
    let p = if rom.starts_with('/') {
        PathBuf::from(&rom)
    } else {
        PathBuf::from(std::env::var("HOME").unwrap())
            .join("Git/nes-test-roms/dmc_dma_during_read4")
            .join(&rom)
    };
    let cart = Cartridge::load(&p).expect("load cartridge");
    let mut nes = Nes::from_cartridge(cart).expect("construct Nes");
    let mut detector = StuckPcDetector::new();
    let limit: u64 = 200_000_000;
    let start = nes.bus.clock.cpu_cycles();
    loop {
        if nes.bus.clock.cpu_cycles() - start > limit {
            eprintln!("cycle limit");
            return;
        }
        nes.run_cycles(10_000).expect("run");
        if detector.observe(nes.cpu.pc) && nametable_has_text(&nes) {
            let text = read_nametable_ascii(&nes);
            for line in text.lines() {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    println!("{trimmed}");
                }
            }
            return;
        }
    }
}
