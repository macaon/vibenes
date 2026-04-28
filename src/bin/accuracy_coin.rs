// SPDX-License-Identifier: GPL-3.0-or-later
//! Headless harness for AccuracyCoin
//! (<https://github.com/100thCoin/AccuracyCoin>). Boots the ROM, presses
//! Start at the top-of-menu cursor (`menuCursorYPos = $FF` after init),
//! which the ROM treats as `AutomaticallyRunEveryTestInROM`. After the
//! runner has had time to execute, dumps the per-test result bytes
//! ($0400-$0490) decoded as PASS / FAIL N / SKIP.
//!
//! Usage: `accuracy_coin <rom.nes>`. The ROM in this repo lives at
//! `~/Git/nes-test-roms/AccuracyCoin.nes`.
//!
//! Result-byte encoding (from `AccuracyCoin.asm` - `TEST_Fail` returns
//! `(ErrorCode << 2) | 2`, primary pass returns `1`, behavior-pass
//! returns `(behavior << 2) | 1`):
//!   * `$FF`            → skipped (or never run)
//!   * `byte == $01`    → PASS (primary)
//!   * `bit 0 == 1`     → PASS with behavior code = `byte >> 2`
//!   * `bit 1 == 1`     → FAIL with error code = `byte >> 2`
//!   * `$00`            → never written (auto-runner not reached this slot)
//!
//! Tunables (env vars):
//!   `ACC_BOOT_FRAMES`   frames to run before pressing Start (default 240)
//!   `ACC_HOLD_FRAMES`   frames Start is held (default 8)
//!   `ACC_RELEASE_FRAMES` frames after release before scoring (default 4)
//!   `ACC_RUN_FRAMES`    frames to run after release (default 36000)
//!   `ACC_DUMP_NAMETABLE` set to 1 to also dump nametable A/B as ASCII

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use vibenes::nes::Nes;
use vibenes::nes::rom::Cartridge;

const BUTTON_START: u8 = 0x08;

/// (result_addr, page_label, test_name). Page labels match the
/// AccuracyCoin README so failure lines map 1:1 onto its docs.
const TESTS: &[(u16, &str, &str)] = &[
    // Page 1 - CPU Behavior
    (0x0405, "1",  "ROM is not writable"),
    (0x0403, "1",  "RAM Mirroring"),
    (0x044D, "1",  "PC Wraparound"),
    (0x0474, "1",  "The Decimal Flag"),
    (0x0475, "1",  "The B Flag"),
    (0x0406, "1",  "Dummy read cycles"),
    (0x0407, "1",  "Dummy write cycles"),
    (0x0408, "1",  "Open Bus"),
    (0x047D, "1",  "All NOP instructions"),
    // Page 2 - Addressing Mode Wraparound
    (0x046E, "2",  "Absolute Indexed"),
    (0x046F, "2",  "Zero Page Indexed"),
    (0x0470, "2",  "Indirect"),
    (0x0471, "2",  "Indirect, X"),
    (0x0472, "2",  "Indirect, Y"),
    (0x0473, "2",  "Relative"),
    // Page 3 - Unofficial: SLO
    (0x0409, "3",  "$03   SLO indirect,X"),
    (0x040A, "3",  "$07   SLO zeropage"),
    (0x040B, "3",  "$0F   SLO absolute"),
    (0x040C, "3",  "$13   SLO indirect,Y"),
    (0x040D, "3",  "$17   SLO zeropage,X"),
    (0x040E, "3",  "$1B   SLO absolute,Y"),
    (0x040F, "3",  "$1F   SLO absolute,X"),
    // Page 4 - Unofficial: RLA
    (0x0419, "4",  "$23   RLA indirect,X"),
    (0x041A, "4",  "$27   RLA zeropage"),
    (0x041B, "4",  "$2F   RLA absolute"),
    (0x041C, "4",  "$33   RLA indirect,Y"),
    (0x041D, "4",  "$37   RLA zeropage,X"),
    (0x041E, "4",  "$3B   RLA absolute,Y"),
    (0x041F, "4",  "$3F   RLA absolute,X"),
    // Page 5 - Unofficial: SRE  (note SRE_47 deliberately at $47F)
    (0x0420, "5",  "$43   SRE indirect,X"),
    (0x047F, "5",  "$47   SRE zeropage"),
    (0x0422, "5",  "$4F   SRE absolute"),
    (0x0423, "5",  "$53   SRE indirect,Y"),
    (0x0424, "5",  "$57   SRE zeropage,X"),
    (0x0425, "5",  "$5B   SRE absolute,Y"),
    (0x0426, "5",  "$5F   SRE absolute,X"),
    // Page 6 - Unofficial: RRA
    (0x0427, "6",  "$63   RRA indirect,X"),
    (0x0428, "6",  "$67   RRA zeropage"),
    (0x0429, "6",  "$6F   RRA absolute"),
    (0x042A, "6",  "$73   RRA indirect,Y"),
    (0x042B, "6",  "$77   RRA zeropage,X"),
    (0x042C, "6",  "$7B   RRA absolute,Y"),
    (0x042D, "6",  "$7F   RRA absolute,X"),
    // Page 7 - Unofficial: *AX
    (0x042E, "7",  "$83   SAX indirect,X"),
    (0x042F, "7",  "$87   SAX zeropage"),
    (0x0430, "7",  "$8F   SAX absolute"),
    (0x0431, "7",  "$97   SAX zeropage,Y"),
    (0x0432, "7",  "$A3   LAX indirect,X"),
    (0x0433, "7",  "$A7   LAX zeropage"),
    (0x0434, "7",  "$AF   LAX absolute"),
    (0x0435, "7",  "$B3   LAX indirect,Y"),
    (0x0436, "7",  "$B7   LAX zeropage,Y"),
    (0x0437, "7",  "$BF   LAX absolute,X"),
    // Page 8 - Unofficial: DCP
    (0x0438, "8",  "$C3   DCP indirect,X"),
    (0x0439, "8",  "$C7   DCP zeropage"),
    (0x043A, "8",  "$CF   DCP absolute"),
    (0x043B, "8",  "$D3   DCP indirect,Y"),
    (0x043C, "8",  "$D7   DCP zeropage,X"),
    (0x043D, "8",  "$DB   DCP absolute,Y"),
    (0x043E, "8",  "$DF   DCP absolute,X"),
    // Page 9 - Unofficial: ISC
    (0x043F, "9",  "$E3   ISC indirect,X"),
    (0x0440, "9",  "$E7   ISC zeropage"),
    (0x0441, "9",  "$EF   ISC absolute"),
    (0x0442, "9",  "$F3   ISC indirect,Y"),
    (0x0443, "9",  "$F7   ISC zeropage,X"),
    (0x0444, "9",  "$FB   ISC absolute,Y"),
    (0x0445, "9",  "$FF   ISC absolute,X"),
    // Page 10 - Unofficial: SH*
    (0x0446, "10", "$93   SHA indirect,Y"),
    (0x0447, "10", "$9F   SHA absolute,Y"),
    (0x0448, "10", "$9B   SHS absolute,Y"),
    (0x0449, "10", "$9C   SHY absolute,X"),
    (0x044A, "10", "$9E   SHX absolute,Y"),
    (0x044B, "10", "$BB   LAE absolute,Y"),
    // Page 11 - Unofficial Immediates
    (0x0410, "11", "$0B   ANC Immediate"),
    (0x0411, "11", "$2B   ANC Immediate"),
    (0x0412, "11", "$4B   ASR Immediate"),
    (0x0413, "11", "$6B   ARR Immediate"),
    (0x0414, "11", "$8B   ANE Immediate"),
    (0x0415, "11", "$AB   LXA Immediate"),
    (0x0416, "11", "$CB   AXS Immediate"),
    (0x0417, "11", "$EB   SBC Immediate"),
    // Page 12 - CPU Interrupts
    (0x0461, "12", "Interrupt flag latency"),
    (0x0462, "12", "NMI Overlap BRK"),
    (0x0463, "12", "NMI Overlap IRQ"),
    // Page 13 - APU Registers and DMA
    (0x046C, "13", "DMA + Open Bus"),
    (0x0488, "13", "DMA + $2002 Read"),
    (0x044C, "13", "DMA + $2007 Read"),
    (0x044F, "13", "DMA + $2007 Write"),
    (0x045D, "13", "DMA + $4015 Read"),
    (0x045E, "13", "DMA + $4016 Read"),
    (0x046B, "13", "DMC DMA Bus Conflicts"),
    (0x0477, "13", "DMC DMA + OAM DMA"),
    (0x0479, "13", "Explicit DMA Abort"),
    (0x0478, "13", "Implicit DMA Abort"),
    // Page 14 - APU
    (0x0465, "14", "APU Length Counter"),
    (0x0466, "14", "APU Length Table"),
    (0x0467, "14", "Frame Counter IRQ"),
    (0x0468, "14", "Frame Counter 4-step"),
    (0x0469, "14", "Frame Counter 5-step"),
    (0x046A, "14", "Delta Modulation Channel"),
    (0x045C, "14", "APU Register Activation"),
    (0x045F, "14", "Controller Strobing"),
    (0x047A, "14", "Controller Clocking"),
    // Page 15 - Power-on (page 3 results -> draw-only, omitted from runner table)
    // Tests on page 15 use result_DrawTest = $03FF, which the auto-runner
    // explicitly skips. We still surface that single byte for awareness.
    (0x03FF, "15", "Power-On (last DRAW result)"),
    // Page 16 - PPU Behavior
    (0x0485, "16", "CHR ROM is not writable"),
    (0x0404, "16", "PPU Register Mirroring"),
    (0x044E, "16", "PPU Register Open Bus"),
    (0x0476, "16", "PPU Read Buffer"),
    (0x047E, "16", "Palette RAM Quirks"),
    (0x0486, "16", "Rendering Flag Behavior"),
    (0x048A, "16", "$2007 read w/ rendering"),
    // Page 17 - PPU VBlank Timing
    (0x0450, "17", "VBlank beginning"),
    (0x0451, "17", "VBlank end"),
    (0x0452, "17", "NMI Control"),
    (0x0453, "17", "NMI Timing"),
    (0x0454, "17", "NMI Suppression"),
    (0x0455, "17", "NMI at VBlank end"),
    (0x0456, "17", "NMI disabled at VBlank"),
    // Page 18 - Sprite Evaluation
    (0x0459, "18", "Sprite overflow behavior"),
    (0x0457, "18", "Sprite 0 Hit behavior"),
    (0x048D, "18", "$2002 flag timing"),
    (0x0489, "18", "Suddenly Resize Sprite"),
    (0x0458, "18", "Arbitrary Sprite zero"),
    (0x045A, "18", "Misaligned OAM behavior"),
    (0x045B, "18", "Address $2004 behavior"),
    (0x047B, "18", "OAM Corruption"),
    (0x0480, "18", "INC $4014"),
    // Page 19 - PPU Misc
    (0x0481, "19", "Attributes As Tiles"),
    (0x0482, "19", "t Register Quirks"),
    (0x0483, "19", "Stale BG Shift Registers"),
    (0x048F, "19", "Stale Sprite Shift Regs"),
    (0x0487, "19", "BG Serial In"),
    (0x0484, "19", "Sprites On Scanline 0"),
    (0x048C, "19", "$2004 Stress Test"),
    (0x048E, "19", "$2007 Stress Test"),
    // Page 20 - CPU Behavior 2
    (0x0460, "20", "Instruction Timing"),
    (0x046D, "20", "Implied Dummy Reads"),
    (0x048B, "20", "Branch Dummy Reads"),
    (0x047C, "20", "JSR Edge Cases"),
    (0x0490, "20", "Internal Data Bus"),
];

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rom_path = match args.first() {
        Some(p) => PathBuf::from(p),
        None => {
            eprintln!("usage: accuracy_coin <rom.nes>");
            return ExitCode::from(2);
        }
    };

    match run(&rom_path) {
        Ok(report) => {
            println!("{}", report.text);
            if report.failures > 0 {
                ExitCode::from(1)
            } else {
                ExitCode::from(0)
            }
        }
        Err(e) => {
            eprintln!("ERROR: {:#}", e);
            ExitCode::from(2)
        }
    }
}

fn env_frames(name: &str, default: u32) -> u32 {
    std::env::var(name).ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

struct Report {
    text: String,
    failures: u32,
}

fn run(rom_path: &PathBuf) -> Result<Report> {
    let cart = Cartridge::load(rom_path)
        .with_context(|| format!("loading {}", rom_path.display()))?;
    eprintln!("{}: {}", rom_path.display(), cart.describe());
    let mut nes = Nes::from_cartridge(cart)?;

    let boot = env_frames("ACC_BOOT_FRAMES", 240);
    let hold = env_frames("ACC_HOLD_FRAMES", 8);
    let release = env_frames("ACC_RELEASE_FRAMES", 4);
    let run_frames = env_frames("ACC_RUN_FRAMES", 12_000);

    step_frames(&mut nes, boot)?;
    nes.bus.controllers[0].buttons = BUTTON_START;
    step_frames(&mut nes, hold)?;
    nes.bus.controllers[0].buttons = 0;
    step_frames(&mut nes, release)?;
    step_frames(&mut nes, run_frames)?;

    eprintln!(
        "post-run state: PC=${:04X} A=${:02X} X=${:02X} Y=${:02X} P=${:02X} SP=${:02X}",
        nes.cpu.pc, nes.cpu.a, nes.cpu.x, nes.cpu.y, nes.cpu.p.to_u8(), nes.cpu.sp
    );
    eprintln!(
        "  Debug_EC=${:02X} ErrorCode=${:02X} initialSubTest=${:02X} suitePointer=${:04X}",
        nes.bus.peek(0xEC),
        nes.bus.peek(0x10),
        nes.bus.peek(0x11),
        u16::from_le_bytes([nes.bus.peek(0x05), nes.bus.peek(0x06)])
    );
    eprintln!(
        "  result_DMADMASync_PreTest=${:02X} result_VblankSync_PreTest=${:02X} PostAllTestTally=${:02X}",
        nes.bus.peek(0x12),
        nes.bus.peek(0x3A),
        nes.bus.peek(0x37)
    );

    Ok(format_report(&nes))
}

fn step_frames(nes: &mut Nes, frames: u32) -> Result<()> {
    for _ in 0..frames {
        if nes.cpu.halted {
            anyhow::bail!(
                "CPU halted: {}",
                nes.cpu
                    .halt_reason
                    .clone()
                    .unwrap_or_else(|| "no reason".into())
            );
        }
        nes.step_until_frame().map_err(anyhow::Error::msg)?;
    }
    Ok(())
}

fn format_report(nes: &Nes) -> Report {
    let mut buf = String::new();
    let mut counts = (0u32, 0u32, 0u32, 0u32); // pass, fail, skip, untouched

    let mut current_page = "";
    for &(addr, page, name) in TESTS {
        if page != current_page {
            buf.push_str(&format!("\n=== Page {} ===\n", page));
            current_page = page;
        }
        let val = nes.bus.peek(addr);
        let line = decode_result(val);
        if line.starts_with("PASS") {
            counts.0 += 1;
        } else if line.starts_with("FAIL") {
            counts.1 += 1;
        } else if line.starts_with("SKIP") {
            counts.2 += 1;
        } else {
            counts.3 += 1;
        }
        buf.push_str(&format!(
            "  ${:04X}  {}  ${:02X}  {}\n",
            addr, line, val, name
        ));
    }
    let summary = format!(
        "\n--- AccuracyCoin: {} pass, {} fail, {} skip, {} untouched (of {}) ---\n",
        counts.0,
        counts.1,
        counts.2,
        counts.3,
        TESTS.len()
    );
    buf.push_str(&summary);

    if std::env::var_os("ACC_DUMP_NAMETABLE").is_some() {
        buf.push_str("\n--- nametable A ($2000) ---\n");
        push_nametable(&mut buf, &nes.bus.ppu.debug_vram()[0..0x400]);
        buf.push_str("--- nametable B ($2400) ---\n");
        push_nametable(&mut buf, &nes.bus.ppu.debug_vram()[0x400..0x800]);
    }

    Report {
        text: buf,
        failures: counts.1,
    }
}

fn decode_result(byte: u8) -> String {
    match byte {
        0xFF => "SKIP    ".to_string(),
        0x00 => "----    ".to_string(),
        0x01 => "PASS    ".to_string(),
        v if v & 0x01 == 1 => format!("PASS B{:X}", v >> 2),
        v if v & 0x02 == 2 => format!("FAIL  {:X} ", v >> 2),
        v => format!("?? ${:02X}  ", v),
    }
}

fn push_nametable(out: &mut String, bytes: &[u8]) {
    for row in 0..30 {
        for col in 0..32 {
            let b = bytes[row * 32 + col];
            out.push(tile_to_ascii(b));
        }
        out.push('\n');
    }
}

fn tile_to_ascii(b: u8) -> char {
    match b {
        0x00 => ' ',
        0x20..=0x7E => b as char,
        _ => '.',
    }
}
