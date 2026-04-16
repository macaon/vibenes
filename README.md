# vibenes

A clean-room, cycle-accurate NES emulator written in Rust.

## Project goal

Build an NES emulator from scratch in idiomatic Rust with a single master
clock driving every subsystem. Correctness comes first — each subsystem
lands with a passing suite of hardware test ROMs (starting with blargg's
CPU tests) before moving on. Rendering targets wgpu with wgsl shaders so
the graphics pipeline can grow custom filters/shaders later.

The implementation is clean-room: no code is copied from other emulators.
The `reference/` directories under `~/Git/puNES`, `~/Git/nestopia`, and
`~/Git/Mesen2` are read for behavioral details (especially unstable
opcodes and mapper quirks) but not for source.

## Current state

### Done

- **iNES 1.0 / NES 2.0 loader** — PRG/CHR extraction, trainer skip,
  mirroring, battery, mapper/submapper IDs, TV system detection.
- **Master clock + bus** — region-aware dividers (NTSC CPU÷12, PPU÷4;
  PAL CPU÷16, PPU÷5). Every CPU bus access advances the master clock
  by one CPU cycle; the clock then runs the PPU for the right number
  of dots (3 on NTSC, 3 or 4 per cycle on PAL's 1:3.2 ratio) and ticks
  the APU. No batching.
- **6502 CPU core** — all 151 official opcodes plus the stable
  unofficial set (LAX, SAX, SLO, RLA, SRE, RRA, DCP, ISB, ALR, ANC,
  ARR, AXS, LXA, SHX, SHY, LAS, TAS, AHX). Dummy reads/writes are
  emitted so the bus charges the right cycle count. JAM opcodes halt
  cleanly. Interrupts (NMI/IRQ/BRK/Reset) and the JMP indirect wrap
  bug implemented.
- **Mappers** — NROM (0), MMC1/SxROM (1) with serial shift and the
  consecutive-write filter, CNROM (3).
- **PPU stub** — register window at $2000-$2007, VBlank flag + NMI
  edge, scroll latch (t/v/x/w), palette and nametable mirroring,
  region-aware scanline count. No rendering yet.
- **APU stub** — register file at $4000-$4017; accepts writes so the
  bus stays quiet. No audio output, no frame counter IRQ yet.
- **Headless blargg test runner** (`cargo run --bin test_runner ROM`) —
  polls $6000 for the standard signature/status/message protocol.

### CPU test results

| Suite | Result |
|---|---|
| `instr_test-v5/all_instrs.nes` | All 16 tests passed |
| `instr_test-v5/rom_singles/` (16 files) | 16/16 PASS |
| `instr_test-v5/official_only.nes` | 16/16 PASS |
| `nes_instr_test/rom_singles/` (11 files) | 11/11 PASS |
| `cpu_dummy_writes_oam.nes` | PASS |
| `cpu_dummy_writes_ppumem.nes` | PASS |
| `instr_misc.nes` | 3/4 (4th needs APU frame IRQ) |

### Not yet

- APU frame counter + audio mixing
- PPU rendering (pattern/nametable/sprite pipeline)
- wgpu window + wgsl shaders
- Controllers (wiring beyond the shifter)
- OAMDMA timing refinements (extra alignment cycle)
- Precise interrupt polling (penultimate-cycle latching)
- Additional mappers (UxROM 2, MMC3 4, AxROM 7, …)
- Test suites that report via PPU screen instead of $6000
  (`branch_timing_tests`, `cpu_timing_test6`, `cpu_dummy_reads`)

## Build

```
cargo build --release
```

## Run

Main binary (currently just steps the CPU for ~1s of emulated time and
exits — no window yet):

```
./target/release/vibenes path/to/rom.nes
```

Test runner (headless blargg protocol):

```
./target/release/test_runner path/to/rom.nes [more.nes ...]
```

## Layout

```
src/
  lib.rs              module root
  rom.rs              iNES 1.0 / NES 2.0 parser
  clock.rs            master clock + region timing
  bus.rs              CPU memory map + per-access tick
  cpu/
    mod.rs            registers, reset, interrupts, step loop
    flags.rs          status register
    ops.rs            151 official + unofficial opcodes
  ppu.rs              2C02 register window + VBlank/NMI
  apu.rs              2A03 APU register stub
  mapper/
    mod.rs            trait + factory
    nrom.rs           mapper 0
    mmc1.rs           mapper 1 (SxROM)
    cnrom.rs          mapper 3
  nes.rs              system glue
  main.rs             CLI entry (stub runtime)
  bin/
    test_runner.rs    headless blargg runner

target/release/
  vibenes             main binary
  test_runner         headless test runner
```
