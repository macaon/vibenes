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
- **APU** — pulse ×2, triangle, noise, DMC channels with shared length
  counter, envelope, sweep, and linear counter. Frame counter sequencer
  in both 4-step and 5-step modes with the $4017 write-delay quirk
  (`W+3` odd / `W+2` even) and IRQ window. $4015 read/write semantics
  wired (frame-IRQ acknowledge on read, DMC IRQ clear on write, mid-
  sample disable drops the pending DMA). $4010 IRQ-disable path clears
  latched DMC IRQ. DMC shift register, rate table, and bus-level DMA
  stall (4 CPU cycles, non-reentrant) are all in place — the DMC fetches
  sample bytes through the mapper and IRQs on non-looping completion.
  12 unit tests cover the $4015 / $4010 / DMC edge cases. No audio
  output device yet — mixer samples are computed and dropped.
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
| `instr_misc.nes` | 4/4 PASS (`04-dummy_reads_apu` now covered) |

### APU test results

| Suite | Result |
|---|---|
| `apu_test/rom_singles/1-len_ctr.nes` | PASS |
| `apu_test/rom_singles/2-len_table.nes` | PASS |
| `apu_test/rom_singles/3-irq_flag.nes` | PASS |
| `apu_test/rom_singles/4-jitter.nes` | PASS |
| `apu_test/rom_singles/5-len_timing.nes` | PASS |
| `apu_test/rom_singles/6-irq_flag_timing.nes` | PASS |
| `apu_test/rom_singles/7-dmc_basics.nes` | PASS |
| `apu_test/rom_singles/8-dmc_rates.nes` | PASS |

### Not yet

- **Penultimate-cycle IRQ/NMI polling** in the CPU core — needed for
  `cpu_interrupts_v2` singles 3–5 and for future mapper IRQs.
- **OAM DMA alignment** — currently charges 513 cycles unconditionally;
  hardware charges 514 when the DMA begins on an odd CPU cycle.
- **`$4016/$4017` DMC double-read bug** — the halt/dummy cycles of a
  DMC DMA don't replay the CPU's pending read address yet, so the
  controller-bit-deletion behavior checked by `dmc_dma_during_read4`
  is not modeled.
- **Audio output** — `Apu::output_sample()` produces a value every
  CPU cycle but nothing drains it; cpal + ring-buffered resampler is
  planned as a dedicated phase once CPU-side timing is locked down.
- **PPU rendering** (pattern/nametable/sprite pipeline) and the wgpu
  window + wgsl shaders.
- **Controllers** (beyond the shifter).
- **Additional mappers** (UxROM 2, MMC3 4, AxROM 7, …).
- Test suites that report via PPU screen instead of $6000
  (`branch_timing_tests`, `cpu_timing_test6`, `cpu_dummy_reads`, most
  of `apu_reset/*` and all of `dmc_tests/*`).

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
  apu/
    mod.rs            2A03 APU top-level: mix, tick, $4015, IRQ line
    frame_counter.rs  4-step/5-step sequencer, $4017 delay, IRQ window
    length.rs         shared length counter
    envelope.rs       envelope unit (pulse + noise)
    sweep.rs          pulse sweep (ones' vs two's complement)
    pulse.rs          pulse channel
    triangle.rs       triangle + linear counter
    noise.rs          noise + LFSR + region period table
    dmc.rs            DMC shift register, rate table, DMA request
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
