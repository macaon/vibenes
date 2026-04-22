# vibenes2 — Claude Code instructions

A clean-room, cycle-accurate NES emulator in Rust. Correctness first:
every subsystem lands with a passing suite of hardware test ROMs
before moving on. See `README.md` for architecture + current test
status.

## Rules

**Clean-room.** No code is copied from other emulators. The following
trees live under `~/Git/` for behavioral reference only — read them
freely for timings, edge cases, and stall tables, but describe the
model in our own words:

- `~/Git/Mesen2/Core/NES/` — APU, CPU, PPU, memory manager.
- `~/Git/punes/src/core/` — `cpu_inline.h`, `apu.c`.
- `~/Git/nestopia/source/core/` — `NstApu.cpp`.

The `nes-expert` skill at `~/.claude/skills/nes-expert/reference/{apu,cpu,timing,mappers,ppu}.md`
is the dense hardware cheat sheet. Consult it before inventing a model.

**Commit style.** Use the `<type>: <description>` format (feat, fix,
refactor, docs, test, chore, perf). **No `Co-Authored-By` trailer, no
AI attribution** in commit messages — the user maintains individual
author history. Never amend a pushed commit; always create a new one.

**Phase workflow.** Active work follows a numbered-phase plan; each
phase has a gating test ROM that must pass before moving on. The
current plan lives in memory at `phases_roadmap.md` and any in-flight
state lives in `PHASE<N>_WIP.md` at the repo root. When resuming, read
both before editing.

## Tests

Build + test pattern:

```
cargo build --release
cargo test --lib --release                  # 14+ unit tests
./target/release/test_runner <rom>          # headless blargg protocol
```

Standard test-ROM locations (under `~/Git/nes-test-roms/`):

| Suite | Purpose |
|---|---|
| `instr_test-v5/official_only.nes` | CPU opcode regression gate (must stay 16/16) |
| `instr_misc/instr_misc.nes` | Dummy reads / APU interaction |
| `apu_test/rom_singles/*.nes` | APU channels + frame counter |
| `apu_reset/*.nes` | Power-on + warm reset (needs runner reset support) |
| `cpu_interrupts_v2/rom_singles/*.nes` | IRQ/NMI penultimate-cycle polling |
| `dmc_dma_during_read4/*.nes` | DMC DMA + controller double-read |
| `sprdma_and_dmc_dma/*.nes` | OAM DMA + DMC DMA interleave |

The test runner supports blargg's `$81` reset request. Many
`apu_mixer` / `dmc_tests` / `apu_mixer_recordings` ROMs report via
audio only and are out of scope until cpal is wired up.

**Regression discipline.** Before committing, re-run (at minimum):
apu_test 1–8, apu_reset all six, instr_test-v5 official_only,
instr_misc, unit tests. If anything below the current phase regresses,
stop and investigate — don't paper over it.

## Working with risky changes

Branch for anything that touches every opcode path or changes the bus
↔ CPU interface (e.g., interrupt polling, DMA re-entry). Merge to
main only after `instr_test-v5` holds 16/16 on the branch. Small
surgical changes stay on `main`.

## Pointers

- `README.md` — architecture, test results, module layout.
- `PHASE<N>_WIP.md` — current in-flight phase state (if any).
- `~/.claude/projects/-home-marcus-Git-vibenes2/memory/` — persistent
  project memory (phases roadmap, notable incidents).
- `~/.claude/rules/common/` and `~/.claude/rules/rust/` — global
  standards (testing, git, Rust style). Project-specific rules above
  override those where they conflict.

## Current focus

CPU, PPU, and APU test suites are all green (see `README.md` §Status
for the full breakdown — `cpu_interrupts_v2 5/5`, `ppu_vbl_nmi 10/10`,
`oam_stress`, `apu_test 8/8`, `apu_reset 6/6`, `blargg_apu_2005 11/11`,
etc.). The PPU is effectively complete; `power_up_palette` is the lone
holdout and is won't-fix (unit-specific snapshot).

DMC/DMA iter-alignment is now solved for the
`dmc_dma_during_read4` suite (all five ROMs land on golden or
sanctioned CRCs, integration tests are strict-pattern). The fix
combined a parity-aware DMC stall (Mesen2's 3-or-4-cycle formula)
with a 1-tick DMC reset-timer offset to keep our DMC bit-shifts
in lock-step with Mesen. The trace-diff harness used to find it
lives in `tools/{trace_mesen.sh,mesen_trace.lua}` + env-gated
`src/cpu/trace.rs`; re-use for any future cycle-exact bisection.
See `notes/phase11/dma_iter_alignment.md` for the full story.

Remaining DMA work, in priority order:

- **OAM DMA get/put loop rewrite** — `sprdma_and_dmc_dma_512.nes`
  is the lone holdout from this class. Cycle pattern is off by 1
  cycle on 2 of 16 iters because the parity-only model can't
  capture the get/put-loop interaction at DMC-fires-near-OAM-end.
  `Bus::run_oam_dma` needs to become an explicit per-cycle get/put
  loop (Mesen2 `NesCpu.cpp:399-447`), with DMC service hijacking
  get cycles inside it rather than running as a separate stall.
  Preserve `cpu_interrupts_v2/4-irq_and_dma`. Full follow-up in
  `notes/phase11/dma_iter_alignment.md §5`.
- **MMC3 scanline-timing off-by-one** — `mmc3_test/4-scanline_timing`
  (both suites) fails #3 by ≥1 PPU cycle. Suspect: `on_ppu_addr`
  timestamp boundary vs Mesen2's CPU-cycle-granular filter.
  `notes/phase10/follow_ups.md §F1`.
- **MMC3 Rev A / MMC6** — Rev A firing semantics implemented and
  unit-tested, no runtime activation path (iNES 1.0 can't carry
  submapper info). `notes/phase10/follow_ups.md §F2`.

**Bigger unlocks beyond the corners:** VRC family (2/4/6/7) and FDS
mappers; MMC3 submapper activation path.

## Regression discipline

Before any commit, run the full sweep from §Tests (at minimum:
instr_test-v5, instr_misc, apu_test 1–8, apu_reset 1–6,
cpu_interrupts_v2 1–5, ppu_vbl_nmi, sprite_hit_tests, plus
`cargo test --release`). Any drop in a previously-green suite is a
stop-and-investigate signal — don't paper over it.

When a test is off by exactly one cycle, first question is whether
subsystems see the right state at the right moment (bus-access
ordering, pre/post-access split), not whether the CPU logic is
right. This pattern showed up repeatedly in Phase 5 and again in
the PPU VBL/NMI work.
