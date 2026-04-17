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
  the APU. No batching. OAM DMA charges 513 or 514 cycles based on
  the CPU-cycle parity at DMA start.
- **6502 CPU core** — all 151 official opcodes plus the stable
  unofficial set (LAX, SAX, SLO, RLA, SRE, RRA, DCP, ISB, ALR, ANC,
  ARR, AXS, LXA, SHX, SHY, LAS, TAS, AHX). Dummy reads/writes are
  emitted so the bus charges the right cycle count. JAM opcodes halt
  cleanly. Interrupts (NMI/IRQ/BRK/Reset) and the JMP indirect wrap
  bug implemented. Reset is the full 7-cycle sequence (5 dummy bus
  cycles + 2 vector reads) so APU/PPU see the correct cycle counts.
  **Penultimate-cycle IRQ/NMI polling** with CLI/SEI/PLP delayed-I
  and RTI immediate-I semantics. **BRK / IRQ → NMI vector hijack**
  when NMI is asserted during the service sequence; late NMIs are
  deferred to after the handler's first instruction (matches the
  `_prevNeedNmi = false` suppression in Mesen2's BRK).
- **Bus cycle split** into `tick_pre_access` (PPU + NMI edge latch)
  and `tick_post_access` (APU + mapper + IRQ line). PPU register
  reads see mid-cycle PPU state, needed for `cpu_interrupts_v2`
  iterations that sync to specific VBlank dots.
- **Mappers** — NROM (0), MMC1/SxROM (1) with serial shift and the
  consecutive-write filter, UxROM (2) with `$8000-$BFFF` switchable /
  `$C000-$FFFF` fixed-to-last split and CHR-RAM, CNROM (3), AxROM (7)
  with 32KB PRG banks and single-screen mirroring toggled by bit 4 of
  the bank-select write.
- **PPU stub** — register window at $2000-$2007, VBlank flag + NMI
  edge, scroll latch (t/v/x/w), palette and nametable mirroring,
  region-aware scanline count. No rendering yet.
- **APU** — pulse ×2, triangle, noise, DMC channels with shared length
  counter, envelope, sweep, and linear counter. Frame counter sequencer
  in both 4-step and 5-step modes with the $4017 write-delay quirk
  (`W+3` odd / `W+2` even) and IRQ window. Power-on and warm-reset
  behavior match nesdev: power simulates a `$4017=$00` write with a
  3-cycle delay (first frame IRQ at cycle ~29831, not 29828); warm
  reset preserves mode bit, forces IRQ-inhibit off, preserves DMC
  output level, and clears the `$4015` enable latches *without* zeroing
  length counter values. `$4015` read/write semantics wired (frame-IRQ
  acknowledge on read, DMC IRQ clear on write, mid-sample disable
  drops the pending DMA). `$4010` IRQ-disable path clears latched DMC
  IRQ. DMC shift register, rate table, and bus-level DMA stall
  (4 CPU cycles, non-reentrant) fetch sample bytes through the mapper
  and IRQ on non-looping completion. Unit tests cover the edges
  (25 across the crate).
  No audio output device yet — mixer samples are computed and dropped.
- **CPU interrupt-polling extras** — branch-delays-IRQ quirk (Mesen2
  `BranchRelative` + puNES `BRC` macro): a taken branch with no page
  cross whose IRQ was newly asserted during the branch suppresses the
  poll for one instruction. Unit-tested at the step level.
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
| `apu_reset/4015_cleared.nes` | PASS |
| `apu_reset/4017_timing.nes` | PASS (delay = 6 cycles, within 6–12) |
| `apu_reset/4017_written.nes` | PASS |
| `apu_reset/irq_flag_cleared.nes` | PASS |
| `apu_reset/len_ctrs_enabled.nes` | PASS |
| `apu_reset/works_immediately.nes` | PASS |

### CPU interrupt test results (`cpu_interrupts_v2/rom_singles/`)

| Suite | Result |
|---|---|
| `1-cli_latency.nes` | PASS |
| `2-nmi_and_brk.nes` | PASS (NMI-hijack-BRK fully working) |
| `3-nmi_and_irq.nes` | FAIL — oscillating row pattern (some iterations NMI 1 instruction late, others early, others miss the hijack). Investigation in progress. |
| `4-irq_and_dma.nes` | FAIL — IRQ recognition appears ~1 CPU cycle early relative to test expectations; flat shift (not oscillating like test 3). |
| `5-branch_delays_irq.nes` | FAIL — branch-delays-IRQ quirk implemented + unit-tested, but the ROM's `test_jmp` subtest (pure JMP, no branches) fails with the same class of bug as test 3. |

### Not yet

- **cpu_interrupts_v2 tests 3 / 4 / 5** — three failing ROMs that all
  exercise cycle-exact interrupt-detection timing. Test 3 shows an
  *oscillating* pattern across iterations (NMI sometimes late, sometimes
  early, hijack sometimes missed). Test 4 shows a *flat* 1-cycle early
  IRQ recognition — different signature, likely a different root cause.
  Test 5's `test_jmp` subtest matches test 3's shape. Verified during
  investigation: our PPU stub (rendering off) is sufficient for these
  tests — the bug is on the CPU/bus/APU side. One failed fix attempt
  (move APU tick to `tick_pre_access` per Mesen2's `StartCpuCycle`
  ordering) regressed `apu_test/4-6`; reverted. Root cause still open.
- **`$4016/$4017` DMC double-read bug** — the halt/dummy cycles of a
  DMC DMA don't replay the CPU's pending read address yet, so the
  controller-bit-deletion behavior checked by `dmc_dma_during_read4`
  is not modeled.
- **Audio output** — `Apu::output_sample()` produces a value every
  CPU cycle but nothing drains it; cpal + ring-buffered resampler is
  planned as a dedicated phase once CPU-side timing is locked down.
- **PPU rendering** (pattern/nametable/sprite pipeline) and the wgpu
  window + wgsl shaders.
- **Second controller / configurable key bindings** — player 1 is
  wired to the keyboard (`./run ROM`, keys: Z=B, X=A, Enter=Start,
  RShift=Select, arrows=D-pad; Esc quits). Player 2 and rebinding
  land later.
- **Additional mappers** — MMC3 (mapper 4) with A12-edge scanline IRQ,
  plus smaller boards as ROMs demand them.
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

Test runner (headless blargg protocol, supports the `$81` reset
request sent by `apu_reset`, `cpu_reset`, etc.):

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
    uxrom.rs          mapper 2 (UNROM/UOROM)
    cnrom.rs          mapper 3
    axrom.rs          mapper 7 (AOROM/AMROM)
  nes.rs              system glue
  main.rs             CLI entry (stub runtime)
  bin/
    test_runner.rs    headless blargg runner

target/release/
  vibenes             main binary
  test_runner         headless test runner
```
