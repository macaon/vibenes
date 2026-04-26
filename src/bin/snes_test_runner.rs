// SPDX-License-Identifier: GPL-3.0-or-later
//! Headless harness for SNES CPU test ROMs. Loads a cart, builds
//! the LoROM bus + CPU, and steps until the program reaches a
//! steady-state PC (typical end-of-test pattern: `Loop: bra Loop`)
//! or hits a configurable instruction budget.
//!
//! No PPU yet, so the peter_lemon ROMs that report PASS/FAIL via
//! VRAM tile rendering can only be observed indirectly:
//! - we count MMIO writes by region (PPU / APU / CPU / DMA / I/O),
//! - we report the final PC and whether it landed in a tight loop,
//! - we surface CPU register state at exit.
//!
//! Once Phase 4 brings up a tile-fetch decoder we'll extend this
//! to grade the actual rendered output.

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use vibenes::snes::cpu::bus::SnesBus;
use vibenes::snes::rom::Cartridge;
use vibenes::snes::Snes;

const DEFAULT_BUDGET: u64 = 30_000_000;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut path: Option<PathBuf> = None;
    let mut budget = DEFAULT_BUDGET;
    let mut ppm_out: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--budget" => {
                let n = args.next().and_then(|v| v.parse().ok());
                match n {
                    Some(b) => budget = b,
                    None => {
                        eprintln!("--budget needs a numeric argument");
                        return ExitCode::from(2);
                    }
                }
            }
            "--ppm" => match args.next() {
                Some(p) => ppm_out = Some(PathBuf::from(p)),
                None => {
                    eprintln!("--ppm needs a path argument");
                    return ExitCode::from(2);
                }
            },
            "-h" | "--help" => {
                eprintln!("usage: snes_test_runner [--budget N] [--ppm out.ppm] <rom>");
                return ExitCode::SUCCESS;
            }
            other if path.is_none() => path = Some(PathBuf::from(other)),
            other => {
                eprintln!("unexpected argument: {other}");
                return ExitCode::from(2);
            }
        }
    }
    let Some(path) = path else {
        eprintln!("usage: snes_test_runner [--budget N] [--ppm out.ppm] <rom>");
        return ExitCode::from(2);
    };

    let cart = match Cartridge::load(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("load: {e:#}");
            return ExitCode::from(1);
        }
    };
    println!("loaded: {}", cart.describe());

    let mut snes = Snes::from_cartridge(cart);
    println!(
        "reset: PBR={:02X} PC={:04X} S={:04X}",
        snes.cpu.pbr, snes.cpu.pc, snes.cpu.s
    );

    // Steady-state detector. Many SNES test ROMs end with a small
    // poll loop (LDA $4212 / AND #$80 / BEQ self) waiting on vblank.
    // We declare "steady" once the last `WINDOW` instructions only
    // touched <= `LOOP_PCS` distinct PCs AND at least
    // `MIN_FRAMES_BEFORE_LOOP` vblanks have fired since boot - so
    // the test body has had time to actually run rather than us
    // catching the pre-vblank wait loop and exiting early.
    // Steady-state detector: a small loop that *persists across
    // multiple frames* without progressing. Catches the actual end-
    // of-test wait loop rather than the inner PrintText vblank polls
    // (which have the same loop shape but are interspersed with
    // distinct outer-PC visits between frames).
    const WINDOW: usize = 256;
    const LOOP_PCS: usize = 8;
    const STABLE_FRAMES_TO_DECLARE_DONE: u64 = 10;
    let mut window = std::collections::VecDeque::with_capacity(WINDOW);
    let mut instructions: u64 = 0;
    let mut steady_state = false;
    let mut steady_pc: u32 = 0;
    let mut last_loop_pcs: Option<Vec<u32>> = None;
    let mut loop_seen_at_frame: u64 = 0;

    while instructions < budget && !snes.cpu.stopped {
        let pbr_pc = ((snes.cpu.pbr as u32) << 16) | snes.cpu.pc as u32;
        window.push_back(pbr_pc);
        if window.len() > WINDOW {
            window.pop_front();
        }
        if window.len() == WINDOW {
            let mut uniq = window.iter().copied().collect::<Vec<_>>();
            uniq.sort_unstable();
            uniq.dedup();
            if uniq.len() <= LOOP_PCS {
                let now_frame = snes.bus.frame_count();
                let same_loop = last_loop_pcs
                    .as_ref()
                    .map(|p| p == &uniq)
                    .unwrap_or(false);
                if !same_loop {
                    last_loop_pcs = Some(uniq.clone());
                    loop_seen_at_frame = now_frame;
                } else if now_frame.saturating_sub(loop_seen_at_frame)
                    >= STABLE_FRAMES_TO_DECLARE_DONE
                {
                    steady_state = true;
                    steady_pc = *uniq.first().unwrap_or(&pbr_pc);
                    break;
                }
            } else {
                last_loop_pcs = None;
            }
        }
        let _op = snes.step_instruction();
        instructions += 1;
    }

    println!(
        "halted after {instructions} instructions, {} master cycles, {} frames",
        snes.bus.master_cycles(),
        snes.bus.frame_count(),
    );
    println!(
        "final: PBR={:02X} PC={:04X} A={:04X} X={:04X} Y={:04X} S={:04X} D={:04X} DBR={:02X} P={:02X} mode={:?}",
        snes.cpu.pbr,
        snes.cpu.pc,
        snes.cpu.c,
        snes.cpu.x,
        snes.cpu.y,
        snes.cpu.s,
        snes.cpu.d,
        snes.cpu.dbr,
        snes.cpu.p.pack(snes.cpu.mode),
        snes.cpu.mode,
    );
    let m = &snes.bus.mmio_writes;
    println!(
        "mmio writes: ppu_b={} apu={} cpu_ctrl={} dma={} joypad={} unmapped={}",
        m.ppu_b_bus, m.apu_ports, m.cpu_ctrl, m.dma_regs, m.joypad_io, m.stz_to_unmapped
    );
    println!("ppu: {}", snes.bus.ppu_state_summary());

    // Dump the rendered framebuffer if requested.
    if let Some(ppm_path) = ppm_out.as_ref() {
        let mut fb = vec![0u8; 256 * 224 * 4];
        snes.bus.render_frame(&mut fb);
        match write_ppm(ppm_path, &fb, 256, 224) {
            Ok(()) => println!("ppm: wrote {}", ppm_path.display()),
            Err(e) => eprintln!("ppm write failed: {e:#}"),
        }
    }

    // Scan VRAM for "FAIL" / "PASS" tile-code patterns. Peter_lemon
    // tests upload result text via $2118 (VMDATAL) byte-by-byte at
    // sequential VRAM word addresses, with VMAIN auto-increment.
    // The bytes land at the LOW byte of each word (offsets 0, 2, 4,
    // ... in the linear 64 KiB VRAM array). To find "FAIL" we walk
    // the low-byte slots looking for the four-character sequence;
    // same for "PASS". Returning the count of each lets the caller
    // distinguish "all sub-tests passed" from "some failed".
    let (pass_count, fail_count) = scan_vram_for_results(&snes.bus.vram);
    println!(
        "vram scan: {} PASS marker(s), {} FAIL marker(s)",
        pass_count, fail_count
    );

    if snes.cpu.stopped {
        println!("verdict: CPU stopped (STP)");
    } else if steady_state {
        let bank = (steady_pc >> 16) & 0xFF;
        let off = steady_pc & 0xFFFF;
        let label = if fail_count > 0 {
            "FAIL"
        } else if pass_count > 0 {
            "PASS"
        } else {
            "indeterminate"
        };
        println!(
            "verdict: {} (steady at {:02X}:{:04X}, {} pass / {} fail)",
            label, bank, off, pass_count, fail_count
        );
        if fail_count > 0 {
            return ExitCode::from(1);
        }
    } else {
        println!("verdict: budget exhausted, no steady-state observed");
    }

    ExitCode::SUCCESS
}

/// Write a 24-bit binary PPM (P6) of the rendered frame so we can
/// inspect peter_lemon's PASS/FAIL display without a windowed
/// viewer.  PPM is the simplest portable image format and most
/// tools (gimp, feh, ImageMagick `convert`) handle it.
fn write_ppm(path: &PathBuf, rgba: &[u8], w: u32, h: u32) -> std::io::Result<()> {
    let mut f = File::create(path)?;
    write!(f, "P6\n{w} {h}\n255\n")?;
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for px in rgba.chunks_exact(4) {
        rgb.push(px[0]);
        rgb.push(px[1]);
        rgb.push(px[2]);
    }
    f.write_all(&rgb)?;
    Ok(())
}

/// Walk every low-byte slot of `vram` looking for "PASS" and "FAIL"
/// tile-code sequences. Peter_lemon writes ASCII characters as the
/// low byte of consecutive 16-bit VRAM words via VMDATAL with
/// auto-increment, so a contiguous pass/fail label appears as four
/// consecutive low-byte slots holding the ASCII characters.
fn scan_vram_for_results(vram: &[u8]) -> (usize, usize) {
    let pass = b"PASS";
    let fail = b"FAIL";
    let mut p = 0;
    let mut f = 0;
    let mut i = 0;
    while i + 6 < vram.len() {
        let stride = 2;
        let chars = [
            vram[i],
            vram[i + stride],
            vram[i + 2 * stride],
            vram[i + 3 * stride],
        ];
        if chars == *pass {
            p += 1;
        } else if chars == *fail {
            f += 1;
        }
        i += stride;
    }
    (p, f)
}
