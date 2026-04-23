# vibenes

A clean-room, cycle-accurate NES emulator in Rust. Single master clock
drives every subsystem. Correctness first — each subsystem lands with a
passing suite of hardware test ROMs before the next.

Clean-room means no code is copied from other emulators. Mesen2, puNES,
and nestopia live under `~/Git/` as behavioral references; I read them
for hardware specifics and describe the model in my own words.

## Status

| Subsystem | State |
|---|---|
| iNES 1.0 / NES 2.0 loader | Complete, CRC32-keyed game DB for region + chip detection |
| 6502 CPU core | All 256 opcodes (official + stable unofficial + ANE/XAA), cycle-accurate, full interrupt model including NMI hijack and branch-delays-IRQ quirk |
| Master clock + bus | Region-aware, per-access tick, 2/1 PPU dot pre/post-access split |
| PPU | Full render pipeline, per-dot sprite evaluation + pattern fetch, pixel-precise sprite-0 hit, level-triggered NMI signal (bus-side rising-edge detection), 1-cycle-delayed `rendering_enabled`, VBlank-race suppression, NTSC odd-frame dot skip, per-bit I/O open-bus decay |
| APU | 5 channels, frame counter with `$4017` write delay, unified parity-gated DMC/OAM DMA get/put loop (Mesen2 port) with DMC-mid-OAM hijacking, halt-cycle replay, staged length-counter writes, non-linear mixer |
| Host audio | cpal + blip_buf, band-limited resampling, pre-filled ring buffer |
| Windowed runtime | wgpu/wgsl renderer, NTSC/PAL-paced, keyboard input |
| Overlay UI | egui-based NES-mini-style menu (F1) — scale, aspect ratio, recent ROMs, file swap, reset |
| Mappers | NROM (0), MMC1/SxROM (1), UxROM (2), CNROM (3), MMC3/TxROM (4), MMC5/ExROM (5), AxROM (7) |

### Tested green

Every ROM in these suites passes:

**CPU**
- `instr_test-v5/*` (16/16), `instr_test-v3`, `instr_misc` (4/4)
- `instr_timing` (2/2), `nes_instr_test` (11/11)
- `cpu_dummy_reads`, `cpu_dummy_writes/*` (2/2)
- `cpu_exec_space/{apu, ppuio}` (2/2)
- `cpu_reset/{ram_after_reset, registers}` (2/2)
- `blargg_nes_cpu_test5/{official, cpu}` (2/2)
- `cpu_interrupts_v2/*` (5/5)
- `cpu_timing_test6/cpu_timing_test` (1/1) — 16-second per-opcode timing test
- `branch_timing_tests/*` (3/3)

**APU**
- `apu_test/*` (8/8), `apu_reset/*` (6/6)
- `blargg_apu_2005.07.30/*` (11/11) — gated by the `blargg_apu_2005`
  integration test
- `dmc_dma_during_read4/*` (5/5) — strict-pattern integration tests:
  `dma_4016_read` lands on the golden `08 08 07 08 08` (CRC
  `F0AB808C`); `dma_2007_read` on a sanctioned `44 55` at iter 2
  (CRC `5E3DF9C4`). Driven by Mesen2's parity-aware DMC stall (3
  cycles entry-even, 4 entry-odd) plus a 1-tick DMC reset-timer
  alignment. See `notes/phase11/dma_iter_alignment.md`.
- `sprdma_and_dmc_dma{,_512}.nes` (2/2) — both pass with
  Mesen-matching cycle patterns, including the 524-cycle iter 4
  of the `_512` variant. Driven by the unified DMC/OAM DMA
  get/put loop (port of Mesen2 `NesCpu.cpp:325-448`): DMC DMA
  firing mid-OAM hijacks a sprite-read get cycle rather than
  running as a separate stall. See `notes/phase11/dma_iter_alignment.md §6`.

**PPU**
- `sprite_hit_tests_2005.10.05/*` (11/11)
- `sprite_overflow_tests/*` (5/5)
- `ppu_vbl_nmi/*` (10/10) — VBlank set/clear timing, NMI
  control/suppression/on/off timing, even/odd frame dot-skip timing
- `oam_read`, `oam_stress`, `ppu_read_buffer/test_ppu_read_buffer`
  (1/1 each)
- `ppu_open_bus` — I/O bus per-bit decay (~600 ms), per-register
  refresh masks, $2004 attribute-byte bit-2-4 masking
- `blargg_ppu_tests_2005.09.15b/{palette_ram, sprite_ram,
  vbl_clear_time, vram_access}` (4/5 — `power_up_palette` is
  hardware-unit-specific, won't-fix)

**Mappers**
- `mmc3_test/{1-clocking, 2-details, 3-A12_clocking, 5-MMC3}` (4/6)
  and `mmc3_test_2/{1-clocking, 2-details, 3-A12_clocking, 5-MMC3}`
  (4/6) — banking + A12-filtered IRQ counter + Rev B firing. See
  "Not yet" for the remaining `4-scanline_timing` and Rev A /
  `6-MMC3_alt` / `6-MMC6` details.

### Not yet

- **MMC3 scanline-timing off-by-one** — `mmc3_test/4-scanline_timing`
  (both suites) fails test #3 by ≥1 PPU cycle. The A12 rise that
  clocks the counter lands later than expected in the test's
  VBL-anchored countdown. Suspect: `on_ppu_addr` timestamp boundary
  vs Mesen2's CPU-cycle-granular filter. Write-up in
  `notes/phase10/follow_ups.md §F1`.
- **MMC3 Rev A / MMC6 submapper** — `6-MMC3_alt` and `6-MMC6` need
  Rev A firing semantics (no refire on reload-to-zero). The logic
  is implemented (`alt_irq_behavior` flag, unit-tested) but has no
  runtime activation path; iNES 1.0 can't carry submapper info.
  Write-up in `notes/phase10/follow_ups.md §F2`.
- **`blargg_ppu_tests_2005.09.15b/power_up_palette`** — **won't fix**.
  Compares the power-on palette byte-for-byte against values
  captured from blargg's specific NES unit; passing requires
  hardcoding that unit's power-on contents, which isn't hardware
  behavior worth reproducing.
- **Additional mappers** — MMC1/3/5 + NROM/UxROM/CNROM/AxROM
  cover a large slice of the commercial library; VRC family (2/4/6/7)
  and FDS are the next meaningful unlocks.
- **Second controller + rebinding** — player 1 is wired to the
  keyboard; player 2 and configurable bindings are future work.

## Building + running

```
cargo build --release
./target/release/vibenes [path/to/rom.nes]
```

The binary can launch without a ROM; use the overlay's File menu to
load one. Current region (NTSC/PAL) is detected from the iNES header
and the built-in CRC32 game DB, and the host audio sample rate is
matched to it.

**Keys**: `Z`=B, `X`=A, `Enter`=Start, `RShift`=Select, arrows=D-pad,
`R`=reset, `F1`=overlay menu, `Esc`=back/quit.

The overlay menu (F1) pauses the emulator and shows a centered modal
over a darkened freeze-frame: Scale (1×–6×), Aspect (Auto / 1:1 / 5:4
/ 8:7 NTSC / 11:8 PAL), Recent ROMs, Load ROM, Reset, Quit. Navigate
with ↑/↓/Enter/Esc or the mouse.

## Testing

```
# Unit tests + integration suites
cargo test --release

# Headless blargg runners (for ROMs not in the integration suites)
./target/release/test_runner ROM.nes          # $6000/DE-B0-61 protocol
./target/release/blargg_2005_report ROM.nes   # pre-$6000 nametable scan
```

`test_runner` handles the standard blargg `$6000` status-byte protocol
including the `$81` reset request. `blargg_2005_report` watches for the
CPU trapping in a `forever:` loop and scans nametable 0 for a result —
recognizes `$hh` debug bytes (2005-era devcart loader), ca65 framework
keywords (`Passed` / `Failed` / `Error N`), blargg keywords (`PASSED`
/ `FAILED` / `FAIL OP`), and `All tests complete`. The scanner gates
on a recognized marker so a long test's header text (e.g.
`cpu_timing_test6`'s 16-second countdown) can't be mis-parsed as a
result digit.

Integration test suites gate against curated ROM sets:
- `tests/blargg_apu_2005.rs` — the 11-ROM 2005 APU suite
- `tests/dmc_dma_during_read4.rs` — 5 DMC/DMA interaction ROMs,
  strict-pattern (golden CRC `F0AB808C` on `dma_4016_read`,
  sanctioned `5E3DF9C4` on `dma_2007_read`)

### Cycle-exact bisection harness

For DMA / interrupt / sub-instruction work where our model drifts
from hardware test ROMs, `tools/trace_mesen.sh <rom> <cycles>` runs
Mesen2 in headless `--testRunner` mode against `tools/mesen_trace.lua`
and emits one line per executed instruction (cyc, pc, op, registers,
master clock, DMC state). Our side has the matching trace gated by
`VIBENES_TRACE_LIMIT=N` env var (`src/cpu/trace.rs`). `diff` on
`(cyc, pc, op)` columns pinpoints the first divergence. This is what
found the parity-aware DMC stall fix in commit `b413b09`.

## Notable design decisions

### Master-clock-driven bus cycle

`clock.start_cpu_cycle(is_read)` and `clock.end_cpu_cycle(is_read)`
split each CPU cycle into two phases. On NTSC, a read advances the
master clock by 5 (start) then 7 (end); a write by 7 (start) then
5 (end) — matching Mesen2 `NesCpu.cpp:73-75,317-322`. PPU runs to
`master_cycles - ppu_offset` (`ppu_offset = 1` per Mesen2 default),
so the number of PPU dots ticked per phase is derived from
master-clock phase rather than hardcoded.

In steady state on NTSC this produces a 2/1 split (2 dots in the
start phase, 1 in the end phase) for both reads and writes. The
2/1 split is required by `cpu_interrupts_v2/3-nmi_and_irq`: when
scanline-241 dot 1 lands as the 3rd dot of a CPU cycle, VBL must
not be visible to a same-cycle `$2002` read (otherwise `sync_vbl`
exits one cycle early and every downstream timing drifts).

`Bus::tick_pre_access(is_read)` wraps `start_cpu_cycle` and runs
the APU tick, mapper tick, and IRQ-line refresh alongside the
emitted PPU dots. `Bus::tick_post_access(is_read)` wraps
`end_cpu_cycle` and performs NMI rising-edge detection on the
PPU's live `nmi_flag`.

APU tick stays in pre-access so `$4015` reads on the frame-IRQ
assertion cycle see the flag set (blargg `08.irq_timing`). OAM DMA
snapshots/restores `prev_irq_line`/`prev_nmi_pending` across its
stall cycles so STA `$4014`'s CPU-level poll sees its own
penultimate, not end-of-DMA state.

### Staged length writes (APU)

Length-counter halt and reload writes are buffered in `LengthCounter::
{pending_halt, pending_reload}` and committed at end of cycle *after*
any same-cycle half-frame clock. Mirrors Mesen2's `_newHaltValue` /
`_previousValue` pattern. Required by `blargg_apu_2005/10.len_halt_
timing` and `11.len_reload_timing`.

### Branch-delays-IRQ quirk

Taken branch with no page cross (3 cycles) suppresses IRQ recognition
iff the IRQ line rose *during* the penultimate cycle. The gate lives in
`ops::branch()` — it snapshots `bus.prev_irq_line` right after operand
fetch (end-of-cycle-1) and only marks the quirk when the line was
still low. Matches Mesen2 `BranchRelative` + puNES `BRC`.

### NMI hijack on BRK / IRQ

After the push phase of BRK or IRQ service, if an NMI is pending the
vector fetch is redirected from `$FFFE` to `$FFFA` (and the NMI latch
cleared). `prev_nmi_pending` is always cleared at the end of the
service so a late NMI (cycles 6–7) is deferred until after the
handler's first instruction — matches Mesen2's explicit
`_prevNeedNmi = false` at the end of `BRK()`.

### A12-filtered MMC3 IRQ counter

MMC3's scanline counter clocks on A12 low→high transitions, filtered
so two rises within three CPU cycles count as one (prevents rendering
BG/sprite fetches from spuriously ticking the counter). `on_ppu_addr`
in the mapper trait is called from `ppu_bus_read`/`write` before the
dot advances, so the timestamp is "at the start of this dot".

### Pixel-precise sprite-0 hit

Sprite-0 hit fires per-pixel during the BG/sprite mux at dots 2–257,
with all five hardware gates (both rendering enables, both left-8-col
enables, non-transparent BG pixel, non-transparent sprite pixel,
dot ≠ 256). Required by SMB's status-bar/playfield scroll split and
NES Open Tournament Golf's title band.

## Layout

```
src/
├── main.rs, app.rs                   windowed binary + shared glue
├── bus.rs, clock.rs                  CPU bus + master clock
├── cpu/{mod,flags,ops}.rs            6502 core, status, all opcodes
├── ppu.rs                            2C02 render pipeline
├── apu/                              pulse × 2, triangle, noise, DMC,
│                                     frame counter, envelope, sweep,
│                                     length counter
├── mapper/                           NROM, MMC1, UxROM, CNROM, MMC3,
│                                     MMC5, AxROM
├── gfx/                              wgpu renderer + wgsl passthrough
├── ui/                               egui overlay (menus, commands,
│                                     recent ROMs)
├── audio.rs                          cpal + blip_buf
├── video.rs                          scale + pixel-aspect settings
├── gamedb.rs, crc32.rs               CRC32-keyed region/chip DB
├── nes.rs, rom.rs                    system glue + iNES parser
├── blargg_2005_scan.rs               stuck-PC + nametable scanner
└── bin/
    ├── test_runner.rs                $6000-protocol runner
    ├── blargg_2005_report.rs         pre-$6000-protocol runner
    ├── frame_dump.rs                 framebuffer PNG dump
    └── dma_4016_dump.rs              DMC/DMA ROM nametable dumper

tests/
├── blargg_apu_2005.rs                APU suite (11 ROMs)
└── dmc_dma_during_read4.rs           DMC/DMA suite (5 ROMs)

tools/
├── trace_mesen.sh                    Mesen2 headless trace wrapper
└── mesen_trace.lua                   per-instruction trace script

assets/fonts/                         VT323 pixel font (SIL OFL) for
                                      the overlay menu

notes/
└── phase{9,10,11}/                   investigation notes for open
                                      DMA/MMC3 corners
```
