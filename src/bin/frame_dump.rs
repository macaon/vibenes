//! Diagnostic binary — run a ROM for N frames, dump the final
//! framebuffer as a PPM image to stdout, and print PPU state
//! (mask / ctrl / v / t / fine_x) + the first 16 pixels of a
//! user-selected scanline to stderr. Used to diagnose BG-pipeline
//! glitches without needing a windowed build.
//!
//! Usage:
//!   frame_dump <rom> [frames=120] [inspect_scanline=120]
//!
//! The PPM is the P6 (binary) form, 256×240 at 8-bit RGB. Pipe to a
//! file or display:
//!   target/release/frame_dump rom.nes > frame.ppm
//!   feh frame.ppm  (or convert/magick to PNG)

use std::io::Write;

use vibenes::app;
use vibenes::ppu::{FRAME_HEIGHT, FRAME_WIDTH};
use vibenes::rom::Cartridge;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: {} <rom> [frames=120] [inspect_scanline=120]", args[0]);
        std::process::exit(2);
    }
    let rom_path = &args[1];
    let frames: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(120);
    let inspect_sl: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(120);

    let cart = Cartridge::load(rom_path).expect("load rom");
    eprintln!("loaded: {}", cart.describe());
    let mut nes = app::build_nes(cart).expect("build nes");

    for _ in 0..frames {
        if nes.cpu.halted {
            break;
        }
        nes.step_until_frame().expect("step");
    }

    let fb = &nes.bus.ppu.frame_buffer;

    // Inspect the requested scanline: first 16 pixels as (R,G,B).
    eprintln!(
        "after {} frames: halted={} mask=${:02X} ctrl=${:02X} status=${:02X}",
        frames,
        nes.cpu.halted,
        nes.bus.ppu.debug_mask(),
        nes.bus.ppu.debug_ctrl(),
        nes.bus.ppu.debug_status(),
    );
    let (v, t, fine_x) = nes.bus.ppu.debug_scroll();
    eprintln!("v=${:04X} t=${:04X} fine_x={}", v, t, fine_x);
    eprintln!(
        "cpu: a=${:02X} x=${:02X} y=${:02X} sp=${:02X} pc=${:04X} halted={}",
        nes.cpu.a, nes.cpu.x, nes.cpu.y, nes.cpu.sp, nes.cpu.pc, nes.cpu.halted
    );
    // Palette RAM (32 bytes).
    eprint!("palette:");
    for (i, v) in nes.bus.ppu.debug_palette().iter().enumerate() {
        if i % 4 == 0 {
            eprint!(" ");
        }
        eprint!("{:02X}", v);
    }
    eprintln!();
    // Top 32 bytes of each nametable page.
    let vram = nes.bus.ppu.debug_vram();
    for row in [0usize, 16, 17] {
        eprint!("NT0 row {:2}:", row);
        for c in 0..32 {
            eprint!(" {:02X}", vram[row * 32 + c]);
        }
        eprintln!();
    }
    // Probe CHR at a few tiles via the mapper.
    for tile in [0u16, 0xE0, 0xEC, 0xF4] {
        let base = tile * 16;
        eprint!("CHR tile ${:03X} pat @${:04X}:", tile, base);
        for off in 0..16 {
            eprint!(" {:02X}", nes.bus.mapper.ppu_read(base + off));
        }
        eprintln!();
    }
    // Count non-zero OAM entries.
    let oam = nes.bus.ppu.debug_oam();
    let nonzero_oam = oam.iter().filter(|&&b| b != 0 && b != 0xFF).count();
    eprintln!("OAM non-zero/-FF bytes: {}", nonzero_oam);
    eprint!("OAM[0..16] (sprites 0..3 Y,tile,attr,X):");
    for i in 0..16 {
        eprint!(" {:02X}", oam[i]);
    }
    eprintln!();
    let mask = nes.bus.ppu.debug_mask();
    eprintln!(
        "  $2001 bits: grayscale={} bg_show_left={} sp_show_left={} bg_enable={} sp_enable={} R={} G={} B={}",
        (mask >> 0) & 1,
        (mask >> 1) & 1,
        (mask >> 2) & 1,
        (mask >> 3) & 1,
        (mask >> 4) & 1,
        (mask >> 5) & 1,
        (mask >> 6) & 1,
        (mask >> 7) & 1,
    );
    if inspect_sl < FRAME_HEIGHT {
        // Compress scanline to single-letter color codes (G=gray,
        // K=black, ? = other), tile-grouped for fast visual scanning.
        let classify = |r: u8, g: u8, b: u8| -> char {
            if r == 0xAB && g == 0xAB && b == 0xAB { 'G' }
            else if r == 0 && g == 0 && b == 0 { 'K' }
            else { '?' }
        };
        eprintln!("per-scanline compressed view, 'G'=gray 'K'=black '?'=other, '.'=tile boundary:");
        for y in 0..FRAME_HEIGHT {
            eprint!("y={:3}: ", y);
            for x in 0..FRAME_WIDTH {
                if x > 0 && x % 8 == 0 { eprint!(" "); }
                let i = (y * FRAME_WIDTH + x) * 4;
                eprint!("{}", classify(fb[i], fb[i + 1], fb[i + 2]));
            }
            eprintln!();
        }
        // Count distinct RGB values across the whole scanline as a
        // sanity check — if the "black left column" is really 8 pixels
        // of backdrop, pixels 0..7 should all be identical.
        let mut unique: Vec<[u8; 3]> = Vec::new();
        for x in 0..FRAME_WIDTH {
            let i = (inspect_sl * FRAME_WIDTH + x) * 4;
            let px = [fb[i], fb[i + 1], fb[i + 2]];
            if !unique.contains(&px) {
                unique.push(px);
            }
        }
        eprintln!("  scanline {} unique colors: {}", inspect_sl, unique.len());
    }

    // Write PPM P6 to stdout.
    let mut out = std::io::stdout().lock();
    writeln!(out, "P6").unwrap();
    writeln!(out, "{} {}", FRAME_WIDTH, FRAME_HEIGHT).unwrap();
    writeln!(out, "255").unwrap();
    for y in 0..FRAME_HEIGHT {
        for x in 0..FRAME_WIDTH {
            let i = (y * FRAME_WIDTH + x) * 4;
            out.write_all(&[fb[i], fb[i + 1], fb[i + 2]]).unwrap();
        }
    }
}
