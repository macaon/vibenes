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
- **6502 CPU core** — every opcode decoded (all 151 official plus
  the full stable + unstable unofficial set: LAX, SAX, SLO, RLA,
  SRE, RRA, DCP, ISB, ALR, ANC, ARR, AXS, LXA, ANE, SHX, SHY, LAS,
  TAS, AHX). 12 JAM/KIL/STP opcodes halt cleanly; the match is
  exhaustive so a missing future opcode would be a compile-time
  error rather than a silent halt. Dummy reads/writes are emitted
  so the bus charges the right cycle count. Interrupts
  (NMI/IRQ/BRK/Reset) and the JMP indirect wrap bug implemented.
  Reset is the full 7-cycle sequence (5 dummy bus cycles + 2 vector
  reads) so APU/PPU see the correct cycle counts; SP starts at
  $00 at power so the first reset's 3-cycle decrement wraps it to
  $FD (matches `cpu_reset/registers`).
  **Penultimate-cycle IRQ/NMI polling** with CLI/SEI/PLP delayed-I
  and RTI immediate-I semantics. **BRK / IRQ → NMI vector hijack**
  when NMI is asserted during the service sequence; late NMIs are
  deferred to after the handler's first instruction (matches the
  `_prevNeedNmi = false` suppression in Mesen2's BRK).
- **Bus cycle split** into `tick_pre_access` (APU + mapper tick,
  IRQ-line refresh, and all-but-the-last PPU dot) and
  `tick_post_access` (the final PPU dot + NMI poll + audio
  sampling). NTSC's 3 dots per CPU cycle run 2-pre + 1-post,
  matching Mesen2's master-clock arithmetic and required by
  `cpu_interrupts_v2/3-nmi_and_irq`: if dot 1 of scanline 241
  lands as the third dot of a CPU cycle, the VBL flag must not
  be visible to a same-cycle `$2002` read (otherwise `sync_vbl`
  exits one cycle early and every downstream timing drifts).
  The APU tick stays in the pre-access half so a `$4015` read
  on the same cycle as a frame-IRQ assertion sees the flag set
  (required by `blargg_apu_2005/08.irq_timing`). OAM DMA
  snapshots and restores `prev_irq_line`/`prev_nmi_pending`
  across its 513/514 stall cycles so STA `$4014`'s poll sees
  its own penultimate, not end-of-DMA state.
- **Mappers** — NROM (0), MMC1/SxROM (1) with serial shift and the
  consecutive-write filter, UxROM (2) with `$8000-$BFFF` switchable /
  `$C000-$FFFF` fixed-to-last split and CHR-RAM, CNROM (3), AxROM (7)
  with 32KB PRG banks and single-screen mirroring toggled by bit 4 of
  the bank-select write.
- **PPU** — full 2C02 rendering pipeline: per-dot background fetch +
  shift registers, per-slot sprite evaluation with the overflow
  diagonal-sweep bug, per-dot sprite pattern fetch (dots 257–320),
  pixel-precise sprite-0 hit, background/sprite mux with priority,
  palette and nametable mirroring, region-aware scanline count,
  `$2002` VBlank-race suppression. SMB's status-bar split, Golf's
  band, and similar mid-frame raster tricks render correctly.
- **APU** — pulse ×2, triangle, noise, DMC channels with shared length
  counter, envelope, sweep, and linear counter. Frame counter sequencer
  in both 4-step and 5-step modes with the $4017 write-delay quirk
  (`W+3` odd / `W+2` even) and IRQ window. Power-on lets the divider
  count from cycle 0 (matching puNES); `Cpu::reset`'s 7 startup
  ticks provide the full pre-first-instruction offset that blargg
  `09.reset_timing` probes. Warm reset preserves mode bit, forces
  IRQ-inhibit off, preserves DMC output level, and clears the
  `$4015` enable latches *without* zeroing length counter values.
  Length-counter halt and reload writes are **staged** — committed at
  end of cycle after any same-cycle half-frame clock, matching
  Mesen2's `_newHaltValue` / `_previousValue` model (required by
  blargg `10.len_halt_timing` and `11.len_reload_timing`). `$4015`
  read/write semantics wired (frame-IRQ acknowledge on read, DMC
  IRQ clear on write, mid-sample disable drops the pending DMA).
  `$4010` IRQ-disable path clears latched DMC IRQ. DMC shift
  register, rate table, and bus-level DMA stall (4 CPU cycles,
  non-reentrant) fetch sample bytes through the mapper and IRQ on
  non-looping completion. Unit tests cover the edges (39 across
  the crate).
- **Host audio output** — `cpal` device + band-limited `blip_buf`
  resampler. Per-cycle APU samples are fed into blip; the output
  device drains a ring buffer pre-filled with enough samples to
  survive frame stalls. Non-linear mixer formula matches the 2A03
  mixer tables. Runs silently if no audio device is available
  (headless CI, WSL without PulseAudio).
- **CPU interrupt-polling extras** — branch-delays-IRQ quirk (Mesen2
  `BranchRelative` + puNES `BRC` macro): a taken branch with no page
  cross whose IRQ was newly asserted during the branch suppresses the
  poll for one instruction. Unit-tested at the step level.
- **Windowed runtime** (`cargo run --release --bin vibenes ROM`) —
  wgpu + wgsl renderer, winit event loop paced to NTSC 60.0988 Hz /
  PAL 50.0070 Hz (not monitor refresh). Player-1 keyboard input
  (Z=B, X=A, Enter=Start, RShift=Select, arrows=D-pad, Esc quits).
  `R` triggers a warm reset (the console Reset button).
- **Headless blargg test runners** — `test_runner` (standard
  $6000/DE-B0-61 protocol, `$81` reset request supported) and
  `blargg_2005_report` (pre-$6000 suite: watches for the CPU trap
  in the `forever:` loop, scans nametable 0. Recognizes the
  `$hh` format used by the 2005-era devcart loader, the newer
  ca65 framework's `Passed` / `Failed` / `Error N` keywords, and
  the `All tests complete` completion sentinel).

### CPU test results

| Suite | Result |
|---|---|
| `instr_test-v5/all_instrs.nes` | All 16 tests passed |
| `instr_test-v5/rom_singles/` (16 files) | 16/16 PASS |
| `instr_test-v5/official_only.nes` | 16/16 PASS |
| `instr_test-v3/official_only.nes` | PASS |
| `nes_instr_test/rom_singles/` (11 files) | 11/11 PASS |
| `instr_timing/{1-instr_timing, 2-branch_timing}` | 2/2 PASS |
| `cpu_dummy_reads.nes` | PASS (via `blargg_2005_report`) |
| `cpu_dummy_writes_oam.nes` | PASS |
| `cpu_dummy_writes_ppumem.nes` | PASS |
| `cpu_exec_space/{apu, ppuio}` | 2/2 PASS |
| `cpu_reset/{ram_after_reset, registers}` | 2/2 PASS |
| `blargg_nes_cpu_test5/{official, cpu}` | 2/2 PASS (via `blargg_2005_report`) |
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

### blargg APU 2005 suite (`blargg_apu_2005.07.30/*.nes`)

Pre-$6000-protocol tests; results via on-screen `$hh` debug byte,
captured headlessly by `blargg_2005_report` and gated by
`cargo test --release --test blargg_apu_2005`.

| ROM | Result |
|---|---|
| `01.len_ctr.nes` | PASS |
| `02.len_table.nes` | PASS |
| `03.irq_flag.nes` | PASS |
| `04.clock_jitter.nes` | PASS |
| `05.len_timing_mode0.nes` | PASS |
| `06.len_timing_mode1.nes` | PASS |
| `07.irq_flag_timing.nes` | PASS |
| `08.irq_timing.nes` | PASS |
| `09.reset_timing.nes` | PASS |
| `10.len_halt_timing.nes` | PASS |
| `11.len_reload_timing.nes` | PASS |

### CPU interrupt test results (`cpu_interrupts_v2/rom_singles/`)

| Suite | Result |
|---|---|
| `1-cli_latency.nes` | PASS |
| `2-nmi_and_brk.nes` | PASS (NMI-hijack-BRK fully working) |
| `3-nmi_and_irq.nes` | PASS (fixed in phase 7: PPU tick split 2-pre/1-post) |
| `4-irq_and_dma.nes` | PASS (fixed in phase 7: save/restore IRQ poll state across OAM DMA + correct alignment parity) |
| `5-branch_delays_irq.nes` | PASS (fixed in phase 7: branch-IRQ quirk sample narrowed to end-of-cycle-1) |

### Not yet

- **`$4016/$4017` DMC double-read bug** — the halt/dummy cycles of a
  DMC DMA don't replay the CPU's pending read address yet, so the
  controller-bit-deletion behavior checked by `dmc_dma_during_read4`
  is not modeled.
- **Second controller / configurable key bindings** — player 1 is
  wired to the keyboard; player 2 and rebinding land later.
- **Additional mappers** — MMC3 (mapper 4) with A12-edge scanline IRQ,
  plus smaller boards as ROMs demand them.
- Test suites that report via PPU screen with a custom protocol
  (`branch_timing_tests`, `cpu_timing_test6`, `cpu_dummy_reads`,
  most `dmc_tests/*`). The pre-$6000 pattern used by
  `blargg_apu_2005` is now covered; similar reporter hooks can be
  added when those suites become priority.

## Build

```
cargo build --release
```

## Run

Windowed emulator (wgpu renderer, cpal audio, paced to NTSC/PAL
frame rate):

```
./target/release/vibenes path/to/rom.nes
```

Keys: `Z`=B, `X`=A, `Enter`=Start, `RShift`=Select, arrows=D-pad,
`R`=reset, `Esc`=quit.

Headless blargg test runners:

```
# $6000 status-byte protocol (apu_test, apu_reset, cpu_interrupts_v2,
# instr_test-v5, instr_misc, …). Supports the $81 reset request.
./target/release/test_runner path/to/rom.nes [more.nes ...]

# Pre-$6000 protocol (blargg_apu_2005.07.30). Watches for the
# final forever: loop, scans nametable 0 for the $hh result byte.
./target/release/blargg_2005_report path/to/rom.nes [more.nes ...]
```

Integration test gate (runs all 11 blargg_apu_2005 ROMs in-process):

```
cargo test --release --test blargg_apu_2005
```

## Layout

```
src/
  lib.rs                   module root
  rom.rs                   iNES 1.0 / NES 2.0 parser
  clock.rs                 master clock + region timing
  bus.rs                   CPU memory map + per-access tick
  nes.rs                   system glue
  app.rs                   shared NES construction for all binaries
  audio.rs                 cpal output + blip_buf resampler
  blargg_2005_scan.rs      stuck-PC detector + nametable ASCII scan
  cpu/
    mod.rs                 registers, reset, interrupts, step loop
    flags.rs               status register
    ops.rs                 151 official + unofficial opcodes
  ppu.rs                   2C02 — full render pipeline, sprite-0, VBlank
  apu/
    mod.rs                 2A03 APU top-level: mix, tick, $4015, IRQ line
    frame_counter.rs       4-step/5-step sequencer, $4017 delay, IRQ window
    length.rs              shared length counter (staged halt + reload)
    envelope.rs            envelope unit (pulse + noise)
    sweep.rs               pulse sweep (ones' vs two's complement)
    pulse.rs               pulse channel
    triangle.rs            triangle + linear counter
    noise.rs               noise + LFSR + region period table
    dmc.rs                 DMC shift register, rate table, DMA request
  mapper/
    mod.rs                 trait + factory
    nrom.rs                mapper 0
    mmc1.rs                mapper 1 (SxROM)
    uxrom.rs               mapper 2 (UNROM/UOROM)
    cnrom.rs               mapper 3
    axrom.rs               mapper 7 (AOROM/AMROM)
  gfx/
    mod.rs                 wgpu renderer + present pipeline
    shaders/               wgsl shaders
  main.rs                  windowed vibenes binary (winit + wgpu)
  bin/
    test_runner.rs         headless $6000-protocol runner
    blargg_2005_report.rs  headless pre-$6000-protocol runner
    frame_dump.rs          one-shot framebuffer PNG dump

tests/
  blargg_apu_2005.rs       integration suite (11 ROMs)

notes/
  phase6/                  investigation write-ups for 08-11

target/release/
  vibenes                  windowed emulator
  test_runner              $6000-protocol headless runner
  blargg_2005_report       pre-$6000-protocol headless runner
  frame_dump               framebuffer PNG dump
```
